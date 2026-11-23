// SPDX-License-Identifier: MPL-2.0
//! The opt-in terrain budget (`regia_render::TerrainBudget`): each cut changes the frame
//! only where it says it does.
//!
//! A photoreal four-layer terrain over rolling hills, a camera 25 m up looking across it, so
//! the screen runs from ~40 m (bottom) to the horizon. The view distance of every pixel comes
//! from a CPU march through the same heightfield. Each cut renders against the default look:
//! - the far-layer and vertex-normal cuts leave every pixel nearer than their distance
//!   unchanged (bloom may bleed a level or two) and do change pixels past it;
//! - hardware-filtered normals change the frame only by the filter's weight precision.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, SharedPassDraw, TerrainBudget,
};
use regia_terrain::noise::fbm2;
use regia_terrain::{
    CdlodConfig, LayerDesc, LayerStyle, MaterialParams, TerrainLayers, TerrainMap, TerrainMaterial,
    TerrainSharedDraw,
};

const W: u32 = 192;
const H: u32 = 108;
const WORLD_M: f32 = 1280.0;
const EYE: [f32; 3] = [-500.0, 0.0, 0.0];
const FOV_Y: f32 = 0.8;

fn height(x: f32, z: f32) -> f32 {
    40.0 * fbm2(x / 260.0, z / 260.0, 3) + 12.0 * fbm2(x / 60.0, z / 60.0, 4)
}

fn map() -> TerrainMap {
    let n = 513u32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        let z = row as f32 / (n - 1) as f32 * WORLD_M - WORLD_M * 0.5;
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * WORLD_M - WORLD_M * 0.5;
            heights.push(height(x, z));
        }
    }
    TerrainMap::from_heights(n, n, heights, WORLD_M, 1.0, 0.0).expect("map")
}

fn layers(map: &TerrainMap) -> TerrainLayers {
    let (lo, hi) = map.min_max_height();
    let layer = |name: &str, tint: [f32; 3], slope: (f32, f32), alt: (f32, f32), tiling: f32| LayerDesc {
        name: name.into(),
        albedo: tint,
        roughness: 0.85,
        tiling,
        slope_min: slope.0,
        slope_max: slope.1,
        altitude_min: alt.0,
        altitude_max: alt.1,
        blend_sharpness: 0.3,
        triplanar: name == "rock",
        height_blend: 0.6,
        detail_normal_strength: 0.8,
        ..LayerDesc::ground()
    };
    let span = hi - lo;
    TerrainLayers {
        layers: vec![
            layer("dust", [0.8, 0.6, 0.45], (0.0, 0.6), (lo - 1.0, hi + 1.0), 0.6),
            layer(
                "sand",
                [0.9, 0.7, 0.5],
                (0.0, 0.4),
                (lo - 1.0, lo + span * 0.6),
                0.4,
            ),
            layer("gravel", [0.6, 0.55, 0.5], (0.2, 0.9), (lo - 1.0, hi + 1.0), 0.7),
            layer(
                "rock",
                [0.7, 0.5, 0.4],
                (0.5, 1.6),
                (lo + span * 0.3, hi + 1.0),
                0.3,
            ),
        ],
        ..Default::default()
    }
}

fn material() -> TerrainMaterial {
    let styles = [
        LayerStyle::Sand,
        LayerStyle::Sand,
        LayerStyle::Dirt,
        LayerStyle::Rock,
    ];
    TerrainMaterial {
        layers: styles
            .iter()
            .enumerate()
            .map(|(i, style)| Some(style.bake(64, 40 + i as u32)))
            .collect(),
        macro_set: None,
        params: MaterialParams::default(),
    }
}

fn render(device: &RenderDevice, map: &TerrainMap, budget: TerrainBudget) -> Vec<u8> {
    let mut draw = TerrainSharedDraw::new(device, map.clone(), &layers(map), CdlodConfig::default(), 5)
        .expect("terrain");
    draw.set_material(&material(), &[]).expect("material");
    let mut renderer = FrameRenderer::new();
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let post = PostSettings {
        exposure_ev: 14.0,
        taa: false,
        ..PostSettings::golden_deterministic()
    };
    let lab = LabScene {
        width: W,
        height: H,
        eye: eye(map),
        target: target(map),
        fov_y: FOV_Y,
        near: 0.1,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.5, -0.55, -0.67], [1.0, 0.92, 0.8], 60_000.0),
        local_lights: Vec::new(),
        materials: Vec::new(),
        meshes: Vec::new(),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.terrain_budget = budget;
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    renderer
        .render_offscreen(device, &frame)
        .unwrap_or_else(|err| panic!("frame: {err}"))
        .pixels
}

fn eye(map: &TerrainMap) -> [f32; 3] {
    [EYE[0], map.sample_bilinear(EYE[0], EYE[2]) + 25.0, EYE[2]]
}

fn target(map: &TerrainMap) -> [f32; 3] {
    let e = eye(map);
    [e[0] + 100.0, e[1] - 12.0, e[2]]
}

/// View distance of each pixel's terrain hit (`f32::INFINITY` for sky), by marching the
/// heightfield along the pixel's centre ray.
fn distances(map: &TerrainMap) -> Vec<f32> {
    let e = glam::Vec3::from_array(eye(map));
    let f = (glam::Vec3::from_array(target(map)) - e).normalize();
    let r = f.cross(glam::Vec3::Y).normalize();
    let u = r.cross(f);
    let t = (FOV_Y * 0.5).tan();
    let aspect = W as f32 / H as f32;
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        for x in 0..W {
            let sx = ((x as f32 + 0.5) / W as f32 * 2.0 - 1.0) * t * aspect;
            let sy = (1.0 - (y as f32 + 0.5) / H as f32 * 2.0) * t;
            let d = (f + r * sx + u * sy).normalize();
            let mut hit = f32::INFINITY;
            let mut s = 1.0;
            while s < 1400.0 {
                let p = e + d * s;
                if p.x.abs() > WORLD_M * 0.5 || p.z.abs() > WORLD_M * 0.5 {
                    break;
                }
                if p.y <= map.sample_bilinear(p.x, p.z) {
                    hit = s;
                    break;
                }
                s += 0.5 + s * 0.004;
            }
            out.push(hit);
        }
    }
    out
}

fn diff(a: &[u8], b: &[u8]) -> Vec<u32> {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .map(|(p, q)| (0..3).map(|c| u32::from(p[c].abs_diff(q[c]))).max().unwrap_or(0))
        .collect()
}

/// `(max Δ nearer than near_m, mean Δ beyond far_m, pixels beyond far_m)`.
fn split(d: &[u32], dist: &[f32], near_m: f32, far_m: f32) -> (u32, f32, usize) {
    let near = d
        .iter()
        .zip(dist)
        .filter(|(_, m)| **m < near_m)
        .map(|(v, _)| *v)
        .max()
        .unwrap_or(0);
    let far: Vec<u32> = d
        .iter()
        .zip(dist)
        .filter(|(_, m)| **m > far_m && m.is_finite())
        .map(|(v, _)| *v)
        .collect();
    let mean = far.iter().sum::<u32>() as f32 / far.len().max(1) as f32;
    (near, mean, far.len())
}

#[test]
fn each_budget_cut_changes_only_what_it_names() {
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let map = map();
    let dist = distances(&map);
    let base = render(&device, &map, TerrainBudget::OFF);
    // Same frame twice: the comparison has no noise floor of its own.
    assert_eq!(
        diff(&base, &render(&device, &map, TerrainBudget::OFF))
            .iter()
            .max(),
        Some(&0)
    );

    for (name, budget) in [
        (
            "far layers",
            TerrainBudget {
                far_layers_m: 150,
                ..TerrainBudget::OFF
            },
        ),
        (
            "vertex normal",
            TerrainBudget {
                vertex_normal_m: 150,
                ..TerrainBudget::OFF
            },
        ),
    ] {
        let d = diff(&base, &render(&device, &map, budget));
        // Nearer than the cut (minus a few metres of bloom bleed): unchanged.
        let (near_max, far_mean, far_n) = split(&d, &dist, 140.0, 190.0);
        eprintln!("{name}: max Δ under 140 m {near_max}, mean Δ past 190 m {far_mean:.3} over {far_n} px");
        assert!(far_n > 500, "the view must reach past the cut");
        assert!(
            near_max <= 2,
            "{name} changed pixels nearer than its distance (max Δ {near_max})"
        );
        assert!(far_mean > 0.05, "{name} changed nothing past its distance");
    }

    // Hardware-filtered normals: everywhere, by the filter weight precision only. Without
    // filterable floats the cut stays off and the frame is identical.
    let hw = TerrainBudget {
        hw_filtered_normals: true,
        ..TerrainBudget::OFF
    };
    let d = diff(&base, &render(&device, &map, hw));
    let mean = d.iter().sum::<u32>() as f32 / d.len() as f32;
    let over = d.iter().filter(|&&v| v > 8).count();
    eprintln!("hw normals: mean Δ {mean:.3}, {over} px over 8");
    assert!(
        mean < 0.5 && over * 200 <= d.len(),
        "hw normals: mean Δ {mean:.3}, {over} px over 8"
    );
}
