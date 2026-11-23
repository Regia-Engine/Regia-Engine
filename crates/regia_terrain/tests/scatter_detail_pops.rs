// SPDX-License-Identifier: MPL-2.0
//! Pops of the dense-scatter screen-size fade ([`ScatterDetail`]) on a slow dolly (lane
//! scatter-density), scored like `scatter_lod_pops.rs`.
//!
//! Rocks shrink out by their projected size as the camera backs away. Scored against the run
//! without the limits, the fade is compared with the same rocks cut in one frame where they
//! reach the limit (`fade_ratio` → 1) and with the class's own distance fade over the same
//! distances (the engine's existing shrink-out). A rock shrinking quickly near the camera
//! changes a tile by a few levels in a frame under this scoring whatever drives the shrink, so
//! the bar is: no worse than the distance fade (+2) and at most half the cut's pops. (Clump
//! LODs are checked on the CPU in `scatter::clump`'s tests: they keep the covered silhouette.)

use std::sync::Arc;

use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset,
};
use regia_terrain::{
    RockStyle, ScatterDetail, ScatterField, ScatterLibrary, ScatterSet, ScatterSource, ScatterView,
    TerrainMap, procedural_rock,
};

const W: u32 = 320;
const H: u32 = 180;
const FOV: f32 = 0.9;
const FRAMES: usize = 96;
const TILE: u32 = 16;
/// Frame-to-frame jump of a tile's deviation (levels) that counts as a pop.
const POP_STEP: f32 = 4.0;

const BOULDERS: &str = r#"
ScatterSet(
    version: 2,
    seed: 9,
    classes: [
        ScatterClass(
            name: "boulders",
            meshes: [
                ScatterMeshRef(mesh: "rock", weight: 1.0, min_scale: 1.5, max_scale: 4.0, scale_bias: 2.0),
            ],
            cell_size: 4.0,
            density: 1.0,
            base_blend: 0.0,
            fade_start: 150.0,
            fade_end: 200.0,
        ),
    ],
)
"#;

/// Renders the dolly over `field` (with sun shadows or without); returns each frame's luma.
fn dolly(device: &RenderDevice, field: &mut ScatterField, shadows: bool) -> Vec<Vec<f32>> {
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
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    let mut renderer = FrameRenderer::new();
    let mut frames = Vec::with_capacity(FRAMES);
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
            directional_shadows: shadows,
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
        field.collect(&view, &mut rocks);
        scene.meshes.extend(rocks);
        let image = renderer
            .render_offscreen(device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"));
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
    frames
}

fn field(set: &str, library: &ScatterLibrary) -> ScatterField {
    let map = TerrainMap::from_heights(129, 129, vec![0.0; 129 * 129], 512.0, 1.0, 0.0).expect("flat map");
    let set = ScatterSet::from_ron(set, None).expect("scatter set");
    ScatterField::new(&set, ScatterSource::new(Arc::new(map)), library, 0x77).expect("scatter field")
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

/// Pops of `run` against `reference` (`scatter_lod_pops.rs`'s scoring): a tile-frame whose
/// deviation jumps by more than `step` levels and by over 3× the median jump of that tile in
/// the four frames before and after.
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
fn screen_size_fade_does_not_pop() {
    let device = RenderDevice::shared_headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let mut library = ScatterLibrary::new();
    let mut rock = procedural_rock(5, RockStyle::angular([0.45, 0.38, 0.32]));
    // One LOD: only the fade changes between the runs.
    rock.lods.truncate(1);
    rock.lods[0].screen_size = 0.0;
    library.insert("rock".into(), rock);
    // Rocks of ~0.75–2 m radius end at 8 px radius, ~9–23 m away at 180 px (16 px wide: a
    // whole tile changes when one is cut): the dolly crosses that.
    let detail = |fade_ratio: f32| ScatterDetail {
        min_radius_px: 8.0,
        fade_ratio,
        shadow_min_radius_m: 0.0,
        reference_height_px: H as f32,
        reference_fov_y: FOV,
    };
    // No sun shadows: paged shadows redraw in steps as the camera moves (in every run,
    // reference included), which would be scored as pops of their own; this scores the rocks.
    let reference = dolly(&device, &mut field(BOULDERS, &library), false);
    let mut cut = field(BOULDERS, &library);
    cut.set_detail(Some(detail(1.0001)));
    let hard = dolly(&device, &mut cut, false);
    let mut faded = field(BOULDERS, &library);
    faded.set_detail(Some(detail(2.0)));
    let soft = dolly(&device, &mut faded, false);
    // Control: the class's own distance fade over the distances the limit fades these rocks.
    let control_set = BOULDERS
        .replace("fade_start: 150.0", "fade_start: 12.0")
        .replace("fade_end: 200.0", "fade_end: 24.0");
    let control = dolly(&device, &mut field(&control_set, &library), false);
    let (hard_pops, soft_pops) = (
        pops(&hard, &reference, POP_STEP),
        pops(&soft, &reference, POP_STEP),
    );
    let control_pops = pops(&control, &reference, POP_STEP);
    eprintln!(
        "screen-size fade, {FRAMES} frames: tile pops (> {POP_STEP} levels) cut {hard_pops}, faded {soft_pops}, \
         distance-fade control {control_pops}; at 2 / 8 levels: cut {} / {}, faded {} / {}, control {} / {}",
        pops(&hard, &reference, 2.0),
        pops(&hard, &reference, 8.0),
        pops(&soft, &reference, 2.0),
        pops(&soft, &reference, 8.0),
        pops(&control, &reference, 2.0),
        pops(&control, &reference, 8.0),
    );
    assert!(
        hard_pops >= 10,
        "a cut at the limit pops {hard_pops} tiles (the scene must show it)"
    );
    assert!(
        soft_pops <= control_pops + 2 && soft_pops * 2 <= hard_pops,
        "the screen-size fade pops {soft_pops} tiles (distance fade: {control_pops}, cut: {hard_pops})"
    );
}
