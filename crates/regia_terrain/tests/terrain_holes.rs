// SPDX-License-Identifier: MPL-2.0
//! Terrain holes (ADR-088): a tunnel-mouth footprint cut through the shared terrain.
//!
//! DO27's tunnels were blocked because the terrain was never cut at the mouths. Here a
//! 25 m × 20 m rotated rectangle is cut through flat ground with a red floor slab 9 m below
//! (standing in for the ramp and tube). Every pixel of the frame is predicted by casting its
//! ray against the exact footprint: ground outside the cut is grey, the floor seen through it
//! is red where the sun reaches it through the opening and dark where the ground around the
//! cut shades it. That checks, at once, that
//!
//! - the mesh under the cut shows through (colour pass and depth prepass discard),
//! - the cut casts no shadow (every sun cascade discards), at noon and at a 55° sun,
//! - the ground outside the footprint still shadows what lies under it,
//! - the border is crack-free and in place at three LOD levels of the node under the cut.
//!
//! Height queries answer NaN inside the cut.

use std::sync::OnceLock;

use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, HoleShape, HolesMask, LayerDesc, LayerTextures, MaterialParams, MinMaxPyramid, Rgba8Image,
    TerrainLayers, TerrainMap, TerrainMaterial, TerrainSharedDraw, select_nodes,
};

const W: u32 = 320;
const H: u32 = 180;
/// Top of the floor slab under the cut.
const FLOOR_Y: f32 = -9.0;
/// Radius of the flat ground around the cut (rolling hills beyond).
const FLAT_R: f32 = 60.0;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// The cut: 25 m × 20 m, rotated 0.4 rad, centred on the origin.
fn mouth() -> HolesMask {
    HolesMask::from_shapes(vec![HoleShape::Rect {
        center: [0.0, 0.0],
        half_extents: [12.5, 10.0],
        rotation_radians: 0.4,
    }])
    .expect("mask")
}

/// 1024 m, 1 m samples: flat inside `FLAT_R`, rolling hills (so the LOD morph moves
/// vertices) beyond.
fn map() -> TerrainMap {
    let n = 1025u32;
    let world = 1024.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
            let z = row as f32 / (n - 1) as f32 * world - world * 0.5;
            let r = (x * x + z * z).sqrt();
            let t = ((r - FLAT_R) / 40.0).clamp(0.0, 1.0);
            heights.push(6.0 * (x / 37.0).sin() * (z / 29.0).cos() * t * t * (3.0 - 2.0 * t));
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 40.0, -20.0).expect("map")
}

fn draw(holes: bool) -> TerrainSharedDraw {
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let v = regia_terrain::linear_to_srgb8(0.4);
    let mut draw =
        TerrainSharedDraw::new(device(), map(), &stack, CdlodConfig::default(), 3).expect("terrain");
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(64, 64, [v, v, v, 255]),
                normal: Rgba8Image::neutral_normal(64),
                height: Rgba8Image::neutral_height(64),
                orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
                use_orm: false,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    if holes {
        draw.set_holes(Some(&mouth()));
    }
    draw
}

/// One shot of the cut.
struct Shot {
    name: &'static str,
    eye: [f32; 3],
    fov_y: f32,
    sun_elevation_deg: f32,
}

fn sun_travel(elevation_deg: f32) -> [f32; 3] {
    // The sun stands toward +x (azimuth slightly off the axis), travelling toward −x and down.
    let el = elevation_deg.to_radians();
    let az = 0.3f32;
    [-el.cos() * az.cos(), -el.sin(), -el.cos() * az.sin()]
}

fn render(shot: &Shot, holes: bool) -> Vec<u8> {
    render_frames(shot, holes, 1).0
}

/// Renders `frames` frames of `shot` with one renderer: the last frame's pixels and each
/// frame's terrain hole tests (`TerrainDrawStats::hole_tests`).
fn render_frames(shot: &Shot, holes: bool, frames: usize) -> (Vec<u8>, Vec<u32>) {
    let draw = draw(holes);
    let stats = draw.stats();
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    // The floor slab: 80 m square, 1 m thick, top at FLOOR_Y.
    let (verts, idx) = regia_render::box_mesh([40.0, 0.5, 40.0]);
    let verts: Vec<_> = verts
        .iter()
        .map(|v| regia_render::LabVertex {
            position: [v.position[0], v.position[1] + FLOOR_Y - 0.5, v.position[2]],
            ..*v
        })
        .collect();
    let scene = LabScene {
        width: W,
        height: H,
        eye: shot.eye,
        target: [0.0, FLOOR_Y * 0.5, 0.0],
        fov_y: shot.fov_y,
        near: 0.5,
        far: 4000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new(
            sun_travel(shot.sun_elevation_deg),
            [1.0, 1.0, 1.0],
            20_000.0,
        ),
        local_lights: Vec::new(),
        materials: vec![regia_render::LabMaterial {
            base_color: [0.8, 0.04, 0.04, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..regia_render::LabMaterial::default()
        }],
        meshes: vec![regia_render::LabMesh {
            vertices: verts,
            indices: idx,
            material: 0,
            ..regia_render::LabMesh::unskinned()
        }],
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        ..OutdoorParams::default()
    });
    renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
    let mut tests = Vec::with_capacity(frames);
    let mut pixels = Vec::new();
    for _ in 0..frames.max(1) {
        pixels = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
        tests.push(stats.hole_tests());
    }
    if let Some(dir) = regia_core::brand::env_var_os("TERRAIN_HOLES_DUMP") {
        let path = std::path::Path::new(&dir).join(format!("{}_holes{holes}.png", shot.name));
        if let Ok(file) = std::fs::File::create(&path) {
            let mut encoder = png::Encoder::new(file, W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(&pixels);
            }
        }
    }
    (pixels, tests)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// Lit ground.
    Ground,
    /// The floor, lit through the cut.
    FloorLit,
    /// The floor in the shadow of the ground around the cut.
    FloorShadow,
    /// Not predicted (beyond the flat ground).
    Skip,
}

/// The renderer's camera pair: right-handed look-at, DirectX perspective.
fn inverse_view_proj(shot: &Shot) -> Mat4 {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(shot.eye),
        Vec3::new(0.0, FLOOR_Y * 0.5, 0.0),
        Vec3::Y,
    );
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(
        shot.fov_y,
        W as f32 / H as f32,
        0.5,
        4000.0,
    );
    (proj * view).inverse()
}

/// What pixel (x, y) shows, from its ray and the exact footprint.
fn predict(shot: &Shot, inv: Mat4, mask: &HolesMask, x: f32, y: f32) -> Class {
    let ndc = [x / W as f32 * 2.0 - 1.0, 1.0 - y / H as f32 * 2.0];
    let a = inv * Vec4::new(ndc[0], ndc[1], 0.0, 1.0);
    let b = inv * Vec4::new(ndc[0], ndc[1], 1.0, 1.0);
    let (a, b) = (a.truncate() / a.w, b.truncate() / b.w);
    let dir = (b - a).normalize();
    let eye = Vec3::from_array(shot.eye);
    if dir.y >= -1e-4 {
        return Class::Skip;
    }
    let ground = eye + dir * (-eye.y / dir.y);
    if ground.x.hypot(ground.z) > FLAT_R - 5.0 {
        return Class::Skip;
    }
    if !mask.contains(ground.x, ground.z) {
        return Class::Ground;
    }
    let floor = eye + dir * ((FLOOR_Y - eye.y) / dir.y);
    // Toward the sun from the floor point, up to the ground plane.
    let to_sun = -Vec3::from_array(sun_travel(shot.sun_elevation_deg));
    let up = floor + to_sun * ((0.0 - FLOOR_Y) / to_sun.y);
    if mask.contains(up.x, up.z) {
        Class::FloorLit
    } else {
        Class::FloorShadow
    }
}

fn classify(px: &[u8], x: u32, y: u32) -> Option<Class> {
    let i = ((y * W + x) * 4) as usize;
    let (r, g, b) = (i32::from(px[i]), i32::from(px[i + 1]), i32::from(px[i + 2]));
    let max = r.max(g).max(b);
    if max < 28 {
        Some(Class::FloorShadow)
    } else if r > 2 * g && r > 2 * b {
        Some(Class::FloorLit)
    } else if (r - g).abs() * 4 < max && (r - b).abs() * 4 < max {
        Some(Class::Ground)
    } else {
        None
    }
}

/// Per class: (pixels judged, pixels matching). A pixel is judged when every prediction
/// within `margin_px` agrees (the edge itself and the shadow's penumbra are not judged).
fn agreement(shot: &Shot, px: &[u8], mask: &HolesMask) -> [(u32, u32); 3] {
    let inv = inverse_view_proj(shot);
    let grid: Vec<Class> = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| predict(shot, inv, mask, x as f32 + 0.5, y as f32 + 0.5))
        .collect();
    let at = |x: i32, y: i32| {
        grid[(y.clamp(0, H as i32 - 1) as u32 * W + x.clamp(0, W as i32 - 1) as u32) as usize]
    };
    let mut out = [(0u32, 0u32); 3];
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            let c = at(x, y);
            let slot = match c {
                Class::Ground => 0,
                Class::FloorLit => 1,
                Class::FloorShadow => 2,
                Class::Skip => continue,
            };
            // Two pixels around the cut edge; five around the shadow's edge (cascade PCF).
            let margin = if c == Class::Ground { 2 } else { 5 };
            let uniform = (-margin..=margin).all(|dy| (-margin..=margin).all(|dx| at(x + dx, y + dy) == c));
            if !uniform {
                continue;
            }
            out[slot].0 += 1;
            if classify(px, x as u32, y as u32) == Some(c) {
                out[slot].1 += 1;
            }
        }
    }
    out
}

fn check(shot: &Shot, expect_shadow: bool) {
    let mask = mouth();
    let px = render(shot, true);
    let [ground, lit, shadow] = agreement(shot, &px, &mask);
    let pct = |(n, ok): (u32, u32)| f64::from(ok) * 100.0 / f64::from(n.max(1));
    eprintln!(
        "{}: ground {}/{} ({:.2} %), floor lit {}/{} ({:.2} %), floor shadow {}/{} ({:.2} %)",
        shot.name,
        ground.1,
        ground.0,
        pct(ground),
        lit.1,
        lit.0,
        pct(lit),
        shadow.1,
        shadow.0,
        pct(shadow)
    );
    assert!(
        ground.0 > 2000,
        "{}: too little ground judged ({})",
        shot.name,
        ground.0
    );
    assert!(
        lit.0 > 300,
        "{}: too little lit floor judged ({})",
        shot.name,
        lit.0
    );
    assert!(
        pct(ground) > 99.5,
        "{}: ground where the cut is not: {:.2} %",
        shot.name,
        pct(ground)
    );
    assert!(
        pct(lit) > 99.0,
        "{}: the floor under the cut is hidden or shadowed by the cut: {:.2} % lit",
        shot.name,
        pct(lit)
    );
    if expect_shadow {
        assert!(
            shadow.0 > 150,
            "{}: too little shadowed floor judged ({})",
            shot.name,
            shadow.0
        );
        assert!(
            pct(shadow) > 98.0,
            "{}: the ground around the cut does not shadow the floor: {:.2} %",
            shot.name,
            pct(shadow)
        );
    }
}

/// The level of the node drawing the cut's centre from `eye`.
fn level_under_cut(eye: [f32; 3]) -> u32 {
    let draw = draw(false);
    let map = draw.map().clone();
    let pyramid = MinMaxPyramid::build(&map, draw.root_level()).expect("pyramid");
    let selection = select_nodes(
        &map,
        &pyramid,
        draw.config(),
        draw.root_level(),
        [eye[0], eye[2]],
        None,
    );
    selection
        .nodes
        .iter()
        .find(|d| {
            let (x, z, s) = d.node.world_rect(map.world_width(), draw.root_level());
            (x..x + s).contains(&0.5) && (z..z + s).contains(&0.5)
        })
        .map(|d| d.node.level)
        .expect("a node under the cut")
}

#[test]
fn the_cut_shows_the_floor_and_casts_no_shadow_at_noon() {
    check(
        &Shot {
            name: "noon",
            eye: [6.0, 45.0, 18.0],
            fov_y: 0.9,
            sun_elevation_deg: 89.0,
        },
        false,
    );
}

#[test]
fn the_ground_around_the_cut_shadows_the_floor_and_the_cut_does_not() {
    check(
        &Shot {
            name: "sun55",
            eye: [6.0, 45.0, 18.0],
            fov_y: 0.9,
            sun_elevation_deg: 55.0,
        },
        true,
    );
}

#[test]
fn the_cut_border_holds_at_every_lod() {
    // The same cut from 35 m, 600 m and 1 km: the node under it is a different level each
    // time (32, 64 and 128 m nodes; the morph moves the hills' vertices), and the border
    // stays where the footprint is. Noon, so the floor's sun does not depend on the cascades'
    // reach at a kilometre.
    let shots = [
        Shot {
            name: "lod_near",
            eye: [10.0, 30.0, 14.0],
            fov_y: 1.2,
            sun_elevation_deg: 89.0,
        },
        Shot {
            name: "lod_mid",
            eye: [300.0, 430.0, 300.0],
            fov_y: 0.075,
            sun_elevation_deg: 89.0,
        },
        Shot {
            name: "lod_far",
            eye: [520.0, 735.0, 520.0],
            fov_y: 0.045,
            sun_elevation_deg: 89.0,
        },
    ];
    let levels: Vec<u32> = shots.iter().map(|s| level_under_cut(s.eye)).collect();
    eprintln!("node levels under the cut: {levels:?}");
    let mut distinct = levels.clone();
    distinct.dedup();
    assert!(
        distinct.len() == 3,
        "the shots must see the cut at three LOD levels: {levels:?}"
    );
    for shot in &shots {
        check(shot, false);
    }
}

#[test]
fn without_the_cut_the_floor_is_hidden() {
    // The control: the same noon shot with no hole shows ground everywhere it was predicted.
    let shot = Shot {
        name: "uncut",
        eye: [6.0, 45.0, 18.0],
        fov_y: 0.9,
        sun_elevation_deg: 89.0,
    };
    let px = render(&shot, false);
    let red = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .filter(|&(x, y)| classify(&px, x, y) == Some(Class::FloorLit))
        .count();
    assert_eq!(red, 0, "the floor shows through uncut ground");
}

#[test]
fn heights_are_nan_inside_the_cut() {
    let mut draw = draw(false);
    let points = [[0.0, 0.0], [8.0, 3.0], [30.0, 0.0], [0.0, -25.0]];
    assert!(draw.rendered_heights(&points).iter().all(|h| h.is_finite()));
    draw.set_holes(Some(&mouth()));
    let h = draw.rendered_heights(&points);
    assert!(h[0].is_nan() && h[1].is_nan(), "inside the cut: {h:?}");
    assert!(h[2].is_finite() && h[3].is_finite(), "outside the cut: {h:?}");
    let surface = draw.surface();
    assert!(surface.in_hole(0.0, 0.0) && !surface.in_hole(30.0, 0.0));
    // Removing the holes restores the ground.
    draw.set_holes(None);
    assert!(draw.rendered_heights(&points).iter().all(|h| h.is_finite()));
    assert!(!surface.in_hole(0.0, 0.0));
}

/// Each node's hole state is computed once while the mask holds: the next frames of the same
/// view test no node. (Up to v0.1.6 every frame tested every shape for every node of every
/// pass list, copying each shape's points or coverage per test.) A kept state is the value the
/// test returned, so the cut the shots above check is unchanged.
#[test]
fn hole_states_are_kept_between_frames() {
    let shot = Shot {
        name: "kept",
        eye: [6.0, 45.0, 18.0],
        fov_y: 0.9,
        sun_elevation_deg: 90.0,
    };
    let (_, tests) = render_frames(&shot, true, 3);
    assert!(tests[0] > 0, "the first frame classifies its nodes: {tests:?}");
    assert_eq!(
        &tests[1..],
        &[0, 0],
        "later frames reuse the kept states: {tests:?}"
    );
}
