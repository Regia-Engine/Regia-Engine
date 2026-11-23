// SPDX-License-Identifier: MPL-2.0
//! Probe-relative SSAO over a baked GI volume (ADR-1104, `baked_gi/ao_split.rs`), on the GPU.
//!
//! A floor lit only by a synthetic baked volume (uniform sky probes, so no path-tracer bake): a
//! terrain layer of 8 m columns everywhere, and a 0.5 m detail brick around a 2 m wall. A 1 m cube
//! with a capsule occluder inside it stands on the terrain-only part.
//!
//! - The screen pass writes the split words: brick texels carry ρ = 0.25 m and no terrain share,
//!   terrain texels no brick share, and the shares add up to the lookup's coverage.
//! - With the split on, the brick's pixels are never darker and on average lighter, and where
//!   the SSAO darkens them (the wall foot's double count) at least half of that comes back; the
//!   terrain-only pixels are unchanged (8 m columns resolve nothing the kernel sees).
//! - With a capsule, the split gives the SSAO's share of its occlusion back: lighter beside the
//!   cube, never darker.
//!
//! Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI).

use std::sync::{Arc, OnceLock};

use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiVolume, DetailBrick, ProbeData, ProbeFlags, ProbeMeta, SH_Y00, ShRgb,
    TerrainLayer, VolumeLayout,
};
use regia_render::baked_gi::{GiCapsule, ProbeAoSplit};
use regia_render::{
    BakedGiSettings, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, box_mesh, translate_mesh,
};

const W: u32 = 192;
const H: u32 = 128;
/// Uniform sky radiance of every probe (cd/m²): mid-grey floor at EV 13.5.
const SKY: f32 = 3000.0;
/// The cube's centre and half edge (terrain-only part of the floor).
const CUBE: [f32; 3] = [2.5, 0.5, 0.0];

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("baked GI needs a GPU; lavapipe is a GPU here"))
}

/// Radiance SH of a uniform radiance `l` (no L1).
fn uniform(l: f32) -> ShRgb {
    let k = 4.0 * std::f32::consts::PI * SH_Y00 * l;
    [[k, 0.0, 0.0, 0.0], [k, 0.0, 0.0, 0.0], [k, 0.0, 0.0, 0.0]]
}

/// Terrain layer (8 m columns, levels 1, 3, 6 m over a flat ground at 0) and a 0.5 m brick
/// from (−4.5, −1, −2.5) to (0.5, 2.5, 2.5) around the wall; every probe valid, lit by `SKY`.
fn volume() -> Arc<BakedGiVolume> {
    static V: OnceLock<Arc<BakedGiVolume>> = OnceLock::new();
    V.get_or_init(|| {
        let counts = [5u32, 5u32];
        let mut layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [-16.0, -16.0],
                spacing: [8.0, 8.0],
                counts,
                levels: vec![1.0, 3.0, 6.0],
                ground: vec![0.0; (counts[0] * counts[1]) as usize],
            }),
            bricks: vec![DetailBrick {
                origin: [-4.5, -1.0, -2.5],
                spacing: [0.5; 3],
                counts: [11, 8, 11],
                first_probe: 0,
                flags: 0,
            }],
            moment_resolution: 4,
            max_distance: 16.0,
        };
        layout.assign_brick_offsets();
        let far = 16.0;
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| ProbeData {
                sh: [uniform(0.0), uniform(SKY), uniform(0.0), uniform(0.0)],
                moments: vec![[far, far * far]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        Arc::new(
            BakedGiVolume::from_probes(
                &layout,
                BakeLights {
                    sky_scale: 1.0,
                    ..BakeLights::default()
                },
                BakeInfo::default(),
                &probes,
            )
            .expect("synthetic volume"),
        )
    })
    .clone()
}

fn mesh(centre: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (v, indices) = box_mesh(half);
    LabMesh {
        vertices: translate_mesh(&v, centre),
        indices,
        material,
        ..LabMesh::unskinned()
    }
}

/// The floor, a 2 m wall inside the brick and the cube on the terrain-only part; no sun, no
/// sky map: the baked volume is the only light.
fn scene() -> LabScene {
    let grey = LabMaterial {
        base_color: [0.45, 0.45, 0.45, 1.0],
        roughness: 0.95,
        ..LabMaterial::default()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 3.0, 7.0],
        target: [0.0, 0.3, -0.5],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 13.5,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 13.5,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![grey],
        meshes: vec![
            mesh([0.0, -0.25, 0.0], [16.0, 0.25, 16.0], 0),
            mesh([-2.0, 1.0, -1.0], [1.5, 1.0, 0.15], 0),
            mesh(CUBE, [0.5; 3], 0),
        ],
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q.ssao = SsaoMode::GtaoHalfRes;
    q
}

/// A renderer lighting the scene with the volume, the split `on` or off, and `capsules`.
fn renderer(on: bool, capsules: &[GiCapsule]) -> FrameRenderer {
    let mut r = FrameRenderer::new();
    r.set_baked_gi(Some(volume()));
    r.set_baked_gi_enabled(Some(true));
    r.set_baked_gi_settings(BakedGiSettings {
        probe_relative_ao: on,
        ..BakedGiSettings::default()
    });
    r.set_gi_occluders(capsules);
    r
}

fn render(r: &mut FrameRenderer) -> Vec<u8> {
    render_with(r, quality())
}

fn render_with(r: &mut FrameRenderer, quality: FrameQuality) -> Vec<u8> {
    let lab = scene();
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut last = Vec::new();
    for _ in 0..2 {
        last = r.render_offscreen(device(), &frame).expect("render").pixels;
    }
    last
}

/// The split per lookup texel of `r`'s last frame (half resolution).
fn split(r: &FrameRenderer) -> (u32, u32, Vec<ProbeAoSplit>) {
    r.baked_gi_split_read_back(device())
        .expect("read back")
        .expect("the screen lookup ran")
}

/// Full-resolution pixel indices (into RGBA8 rows) of the lookup texels `keep` selects.
fn pixels_of(hw: u32, hh: u32, split: &[ProbeAoSplit], keep: impl Fn(&ProbeAoSplit) -> bool) -> Vec<usize> {
    let mut out = Vec::new();
    for ty in 0..hh {
        for tx in 0..hw {
            if !keep(&split[(ty * hw + tx) as usize]) {
                continue;
            }
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (x, y) = (tx * 2 + dx, ty * 2 + dy);
                if x < W && y < H {
                    out.push(((y * W + x) * 4) as usize);
                }
            }
        }
    }
    out
}

fn luma(px: &[u8], i: usize) -> f32 {
    0.2126 * f32::from(px[i]) + 0.7152 * f32::from(px[i + 1]) + 0.0722 * f32::from(px[i + 2])
}

fn mean_luma(px: &[u8], at: &[usize]) -> f32 {
    at.iter().map(|&i| luma(px, i)).sum::<f32>() / at.len().max(1) as f32
}

/// Largest per-channel drop of `on` under `off` over the whole frame (8-bit levels).
fn worst_darkening(on: &[u8], off: &[u8]) -> i32 {
    on.iter()
        .zip(off)
        .map(|(a, b)| i32::from(*b) - i32::from(*a))
        .max()
        .unwrap_or(0)
}

fn brick(s: &ProbeAoSplit) -> bool {
    s.brick_share > 0.99
}

/// Terrain-lit with no brick share at all (a texel in a brick's fading last cell has one).
fn terrain_only(s: &ProbeAoSplit) -> bool {
    s.brick_share == 0.0 && s.terrain_share > 0.99
}

#[test]
fn the_screen_pass_writes_the_split_the_lookup_says() {
    let mut r = renderer(true, &[]);
    render(&mut r);
    let status = r.baked_gi_status();
    assert!(status.active, "the volume lit the frame: {status:?}");
    assert!(
        status.probe_relative_ao,
        "the SSAO ran with the split: {status:?}"
    );
    let (hw, hh, words) = split(&r);
    let (lw, lh, texels) = r
        .baked_gi_read_back(device())
        .expect("read back")
        .expect("the screen lookup ran");
    assert_eq!((hw, hh), (lw, lh));
    let (mut bricks, mut terrain) = (0, 0);
    for (s, t) in words.iter().zip(&texels) {
        assert!(s.brick_share + s.terrain_share <= 1.0 + 1e-4, "{s:?}");
        assert!(
            (s.brick_share + s.terrain_share - t.coverage).abs() < 2e-3,
            "the shares add up to the coverage: {s:?} vs {}",
            t.coverage
        );
        if brick(s) {
            assert_eq!(s.brick_rho, 0.25, "half the brick's 0.5 m cell");
            bricks += 1;
        }
        if terrain_only(s) {
            assert_eq!(s.brick_rho, 0.0, "no brick: no brick distance");
            terrain += 1;
        }
        // Half the 8 m columns wherever the terrain layer holds light; 0 where it holds none
        // (deep in the brick, outside the volume). A share below one unorm16 step reads 0.
        assert!(
            s.terrain_rho == 4.0 || (s.terrain_rho == 0.0 && s.terrain_share == 0.0),
            "{s:?}"
        );
        if terrain_only(s) {
            assert_eq!(s.terrain_rho, 4.0, "half the 8 m columns");
        }
    }
    eprintln!(
        "split texels: {bricks} brick, {terrain} terrain-only of {}",
        words.len()
    );
    assert!(
        bricks >= 50 && terrain >= 50,
        "{bricks} brick, {terrain} terrain-only texels"
    );
    // Split off: the status says so.
    let mut off = renderer(false, &[]);
    render(&mut off);
    assert!(off.baked_gi_status().active);
    assert!(!off.baked_gi_status().probe_relative_ao);
}

#[test]
fn the_wall_foot_under_a_brick_gets_its_double_count_back() {
    let mut on_r = renderer(true, &[]);
    let on = render(&mut on_r);
    let off = render(&mut renderer(false, &[]));
    assert_eq!(on, render(&mut renderer(true, &[])), "the split is deterministic");
    let (hw, hh, s) = split(&on_r);
    // Never darker anywhere (one level of rounding).
    let worst = worst_darkening(&on, &off);
    assert!(worst <= 1, "the split darkened a pixel by {worst} levels");
    // Lighter on average where the brick lights the frame (the wall's foot and faces).
    let at = pixels_of(hw, hh, &s, brick);
    let (l_on, l_off) = (mean_luma(&on, &at), mean_luma(&off, &at));
    eprintln!("brick pixels ({}): luma {l_off:.2} -> {l_on:.2}", at.len());
    assert!(l_on > l_off, "brick pixels: luma {l_off:.2} -> {l_on:.2}");
    // The double count sits where the SSAO darkens a brick pixel (the wall's foot): there the
    // split gives back at least half of what the SSAO took. Over every brick pixel the SSAO
    // takes under 1 % on lavapipe, so the mean alone cannot show it (ADR-1389).
    let mut no_ssao = quality();
    no_ssao.ssao = SsaoMode::Off;
    let bare = render_with(&mut renderer(false, &[]), no_ssao);
    let foot: Vec<usize> = at
        .iter()
        .copied()
        .filter(|&i| luma(&bare, i) - luma(&off, i) >= 3.0)
        .collect();
    let (f_bare, f_off, f_on) = (
        mean_luma(&bare, &foot),
        mean_luma(&off, &foot),
        mean_luma(&on, &foot),
    );
    eprintln!(
        "brick pixels the SSAO darkens ({}): no SSAO {f_bare:.2}, split off {f_off:.2}, split on {f_on:.2}",
        foot.len()
    );
    assert!(
        foot.len() >= 100,
        "the SSAO darkens only {} brick pixels",
        foot.len()
    );
    assert!(
        f_on - f_off >= 0.5 * (f_bare - f_off),
        "the split gave back {:.2} of the SSAO's {:.2} levels at the wall foot",
        f_on - f_off,
        f_bare - f_off
    );
    // The terrain layer resolves nothing the kernel sees: unchanged but for rounding (and the
    // blur's reach across a brick edge).
    let at = pixels_of(hw, hh, &s, terrain_only);
    let same = at
        .iter()
        .filter(|&&i| (0..3).all(|c| (i32::from(on[i + c]) - i32::from(off[i + c])).abs() <= 1))
        .count();
    eprintln!("terrain-only pixels: {same} of {} unchanged", at.len());
    assert!(
        same * 100 >= at.len() * 95,
        "terrain-only pixels changed: {same} of {} kept",
        at.len()
    );
}

#[test]
fn a_capsule_leaves_the_ssao_its_share() {
    let caps = [GiCapsule::sphere(CUBE, 0.5)];
    let mut on_r = renderer(true, &caps);
    let on = render(&mut on_r);
    let off = render(&mut renderer(false, &caps));
    let bare = render(&mut renderer(true, &[]));
    let (hw, hh, s) = split(&on_r);
    let at = pixels_of(hw, hh, &s, terrain_only);
    let (l_on, l_off, l_bare) = (mean_luma(&on, &at), mean_luma(&off, &at), mean_luma(&bare, &at));
    eprintln!(
        "terrain-only pixels ({}): capsule split {l_on:.3}, whole {l_off:.3}, none {l_bare:.3}",
        at.len()
    );
    assert!(l_off < l_bare, "the capsule darkens the floor beside the cube");
    assert!(l_on > l_off, "the split gives the kernel's share back");
    let worst = worst_darkening(&on, &off);
    assert!(worst <= 1, "the split darkened a pixel by {worst} levels");
    let lighter = at
        .iter()
        .filter(|&&i| luma(&on, i) >= luma(&off, i) + 2.0)
        .count();
    assert!(lighter >= 20, "only {lighter} pixels beside the cube got lighter");
}
