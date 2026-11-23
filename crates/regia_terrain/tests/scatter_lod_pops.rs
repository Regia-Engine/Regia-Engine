// SPDX-License-Identifier: MPL-2.0
//! Scatter LOD pops on a slow dolly (motion-look lane, round 2).
//!
//! The camera backs away from a boulder field at 0.2 m per frame through the full runtime
//! frame (depth prepass, GPU-driven colour, TAA). A LOD switch that happens in one frame is a
//! temporal outlier in the tiles it covers: a frame-to-frame change far above the change the
//! same tile shows in the frames around it. Switches now cross-fade over
//! `LOD_FADE_FRAMES` with complementary dither (and have hysteresis), so the change is spread
//! over eight frames. Scored against a run whose rocks never switch (TAA's own motion noise
//! is the same in every run, so only what a switch changes is counted).

use std::sync::Arc;

use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset,
};
use regia_terrain::{
    RockStyle, ScatterField, ScatterLibrary, ScatterMeshAsset, ScatterSet, ScatterSource, ScatterView,
    TerrainMap, procedural_rock,
};

const W: u32 = 320;
const H: u32 = 180;
const FOV: f32 = 0.9;
const FRAMES: usize = 96;
const TILE: u32 = 16;
/// Frame-to-frame jump of a tile's deviation (levels) that counts as a pop.
const POP_STEP: f32 = 4.0;

const SET: &str = r#"
ScatterSet(
    version: 2,
    seed: 5,
    classes: [
        ScatterClass(
            name: "boulders",
            meshes: [
                ScatterMeshRef(mesh: "rock_a", weight: 1.0, min_scale: 0.8, max_scale: 2.5, scale_bias: 2.0),
                ScatterMeshRef(mesh: "rock_b", weight: 1.0, min_scale: 0.8, max_scale: 2.5, scale_bias: 2.0),
            ],
            cell_size: 3.0,
            density: 1.0,
            base_blend: 0.0,
            // Every LOD switch of the dolly happens within ~25 m (screen size 0.12 for rocks of
            // ≤ 2.5 m × 2.5); rocks beyond 80 m only cost lavapipe time (the 300–400 m fade
            // drew ~10× more rocks and made this the slowest test in the gate). The distance
            // fade is the same in every run, so it cancels in the scored deviation.
            fade_start: 60.0,
            fade_end: 80.0,
        ),
    ],
)
"#;

/// LOD behaviour of one run.
#[derive(Clone, Copy, PartialEq)]
enum Lods {
    /// LOD 0 only: the reference no switch disturbs.
    Fixed,
    /// Switches in one frame.
    Hard,
    /// Switches cross-faded.
    Faded,
}

/// Renders the dolly; returns each frame's luma and the cross-fading instance-frames.
fn dolly(device: &RenderDevice, lods: Lods) -> (Vec<Vec<f32>>, u32) {
    let crossfade = lods == Lods::Faded;
    let map = TerrainMap::from_heights(129, 129, vec![0.0; 129 * 129], 512.0, 1.0, 0.0).expect("flat map");
    let set = ScatterSet::from_ron(SET, None).expect("scatter set");
    let mut library = ScatterLibrary::new();
    library.insert("rock_a".into(), two_shape_rock(1, 2, lods == Lods::Fixed));
    library.insert("rock_b".into(), two_shape_rock(3, 4, lods == Lods::Fixed));
    let mut field =
        ScatterField::new(&set, ScatterSource::new(Arc::new(map)), &library, 0x51).expect("scatter field");
    field.set_lod_crossfade(crossfade);
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        taa: false,
        ..PostSettings::golden_deterministic()
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.render_scale_permille = 1000;
    // Rocks only: the lighting of the ground is not what is scored.
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    // The GPU irradiance probes (Desktop High's Depth look) converge over the dolly's frames, which
    // changes the rocks' light between the compared runs (ADR-132: frame-comparison tests turn
    // them off).
    quality.gi_probes = false;
    let mut renderer = FrameRenderer::new();
    let mut frames = Vec::with_capacity(FRAMES);
    let mut fading = 0;
    for i in 0..FRAMES {
        let z = 10.0 + i as f32 * 0.2;
        let eye = [0.0, 3.0, z];
        let target = [0.0, 0.0, z - 40.0];
        field.update(eye);
        let lab = LabScene {
            width: W,
            height: H,
            eye,
            target,
            fov_y: FOV,
            near: 0.1,
            far: 2000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 12_000.0,
            double_sided: false,
            post: None,
            directional: DirectionalLightUniform::new([0.5, -0.6, -0.6], [1.0, 0.95, 0.9], 90_000.0),
            local_lights: Vec::new(),
            materials: vec![regia_render::LabMaterial {
                base_color: [0.2, 0.17, 0.14, 1.0],
                roughness: 1.0,
                ..regia_render::LabMaterial::default()
            }],
            meshes: vec![ground()],
        };
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&post);
        let view = ScatterView::look_at(eye, target, FOV, W as f32 / H as f32, 0.1, 2000.0);
        let mut rocks: Vec<FrameMesh<'_>> = Vec::new();
        fading += field.collect(&view, &mut rocks).lod_fading;
        scene.meshes.extend(rocks);
        let image = renderer
            .render_offscreen(device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"));
        if let Some(dir) = regia_core::brand::env_var_os("SCATTER_POPS_DUMP") {
            let dir = std::path::PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&dir);
            let mut ppm = format!("P6\n{W} {H}\n255\n").into_bytes();
            for p in image.pixels.as_chunks::<4>().0 {
                ppm.extend_from_slice(&p[..3]);
            }
            let _ = std::fs::write(dir.join(format!("{}_{i:03}.ppm", lods as u8)), ppm);
        }
        frames.push(
            image
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
                .collect(),
        );
    }
    (frames, fading)
}

/// A rock whose LOD 1 is another rock: the switch (at a large projected size, so near the
/// camera) changes the silhouette, as a badly authored LOD chain would, and is visible.
/// `fixed` keeps LOD 0 only (same placement and keys: the reference run).
fn two_shape_rock(seed: u64, other: u64, fixed: bool) -> ScatterMeshAsset {
    let grey = [0.45, 0.38, 0.32];
    let mut near = procedural_rock(seed, RockStyle::angular(grey));
    let far = procedural_rock(other, RockStyle::rounded(grey));
    near.lods.truncate(1);
    if fixed {
        near.lods[0].screen_size = 0.0;
        return near;
    }
    near.lods[0].screen_size = 0.12;
    let mut lod1 = far.lods[0].clone();
    lod1.screen_size = 0.0;
    near.lods.push(lod1);
    near
}

/// A dark ground plane under the field.
fn ground() -> regia_render::LabMesh {
    let n = [0.0, 1.0, 0.0];
    regia_render::LabMesh {
        vertices: [[-300.0, 300.0], [300.0, 300.0], [300.0, -300.0], [-300.0, -300.0]]
            .iter()
            .map(|&[x, z]| regia_render::LabVertex::from_pos_normal_uv([x, 0.0, z], n, [0.0, 0.0]))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..regia_render::LabMesh::unskinned()
    }
}

/// Mean |a − b| per 16 px tile.
fn tile_deviation(a: &[f32], b: &[f32]) -> Vec<f32> {
    let (tx, ty) = (W / TILE, H / TILE);
    let mut tiles = vec![0.0f32; (tx * ty) as usize];
    for y in 0..ty * TILE {
        for x in 0..tx * TILE {
            let i = (y * W + x) as usize;
            tiles[((y / TILE) * tx + x / TILE) as usize] += (a[i] - b[i]).abs() / (TILE * TILE) as f32;
        }
    }
    tiles
}

/// Pops of `run` against the no-switch reference. Until a rock switches, a run and the
/// reference are the same frame; afterwards the other LOD's shape drifts against the
/// reference as the camera moves. A pop is a tile-frame whose deviation from the reference
/// jumps by more than `step` levels and by over 3× the median jump of that tile in the four
/// frames before and after: a switch in one frame is such a spike, a cross-fade a ramp of
/// eight similar steps, and the drift is smooth.
fn pops(run: &[Vec<f32>], reference: &[Vec<f32>], step: f32) -> u32 {
    let deviation: Vec<Vec<f32>> = run
        .iter()
        .zip(reference)
        .map(|(a, b)| tile_deviation(a, b))
        .collect();
    let jumps: Vec<Vec<f32>> = deviation
        .windows(2)
        .map(|w| {
            w[1].iter()
                .zip(&w[0])
                .map(|(now, before)| (now - before).abs())
                .collect()
        })
        .collect();
    let mut count = 0;
    for f in 0..jumps.len() {
        for t in 0..jumps[f].len() {
            let mut around: Vec<f32> = (f.saturating_sub(4)..(f + 5).min(jumps.len()))
                .filter(|&g| g != f)
                .map(|g| jumps[g][t])
                .collect();
            around.sort_by(f32::total_cmp);
            let median = around.get(around.len() / 2).copied().unwrap_or(0.0);
            if jumps[f][t] > step && jumps[f][t] > 3.0 * median {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn slow_dolly_lod_switches_do_not_pop() {
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let started = std::time::Instant::now();
    let (reference, _) = dolly(&device, Lods::Fixed);
    let (hard, hard_fading) = dolly(&device, Lods::Hard);
    let (soft, soft_fading) = dolly(&device, Lods::Faded);
    let hard_pops = pops(&hard, &reference, POP_STEP);
    let soft_pops = pops(&soft, &reference, POP_STEP);
    eprintln!(
        "slow dolly, {FRAMES} frames: tile pops (deviation from the no-switch run jumping > {POP_STEP} levels) \
         hard LOD switches {hard_pops}, cross-faded {soft_pops}; {soft_fading} instance-frames cross-faded \
         ({hard_fading} with the fade off); at 2 / 8 levels: hard {} / {}, faded {} / {}; {:.0} s",
        pops(&hard, &reference, 2.0),
        pops(&hard, &reference, 8.0),
        pops(&soft, &reference, 2.0),
        pops(&soft, &reference, 8.0),
        started.elapsed().as_secs_f32(),
    );
    assert_eq!(hard_fading, 0, "the cross-fade ran while switched off");
    assert!(
        soft_fading > 50,
        "the dolly crossed few LOD switches ({soft_fading} fading instance-frames)"
    );
    assert!(
        hard_pops >= 10,
        "hard LOD switches pop {hard_pops} tiles (the scene must show them)"
    );
    assert!(
        soft_pops * 4 <= hard_pops,
        "cross-faded LOD switches still pop {soft_pops} tiles (hard switches: {hard_pops}; ≤ a quarter)"
    );
}
