// SPDX-License-Identifier: MPL-2.0
//! DO27 ask 4 (tunnels v2): a 30 m tunnel mouth cut through a slope, flown in from 9 km
//! to a few metres, at every LOD level of the node under the cut (ADR-088, ADR-247).
//!
//! The ground is an exact plane (a 0.2 slope, about 11°) inside 70 m of the mouth, so every
//! LOD draws it exactly and the prediction below is exact; rolling hills beyond keep the morph
//! moving vertices elsewhere. The mouth is a 30 m × 24 m convex polygon. Under it lies a red
//! apron, the footprint grown by 0.5 m, 2 cm under the plane: the tunnel's lip and ramp. Every
//! pixel is predicted from its ray against the plane and the exact footprint, and checked:
//!
//! - **no cracks**: no judged ground pixel shows the black sky;
//! - **no floating skirt**: no judged pixel inside the mouth shows terrain (grey);
//! - **no z-fighting**: the apron 2 cm under the cut shows clean red at every distance; without
//!   the cut (the control) the terrain covers it, so the clean red is the cut's doing. At the far
//!   levels the uncut terrain lets blocks of the apron through: the pyramid-filtered far-LOD
//!   surface sits a little under the true plane in places (`TerrainSharedDraw::rendered_heights`
//!   is that surface), which is also why the apron reaches only 0.5 m past the cut.
//!
//! Lavapipe-friendly: 320 × 180, one frame per shot.

use std::sync::OnceLock;

use regia_core::glam::Vec3;
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
/// Rise per metre along +x inside [`FLAT_R`].
const SLOPE: f32 = 0.2;
/// Radius of the exact plane around the mouth.
const FLAT_R: f32 = 70.0;
/// The apron's depth under the plane, metres.
const APRON_DEPTH: f32 = 0.02;
/// Quadtree depth of the test terrain (levels 0..=ROOT).
const ROOT: u32 = 3;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// The mouth's footprint: a convex hexagon, 30 m along x, 24 m along z.
fn mouth_points() -> Vec<[f32; 2]> {
    vec![
        [-15.0, -4.0],
        [-8.0, -12.0],
        [9.0, -11.0],
        [15.0, 2.0],
        [7.0, 12.0],
        [-9.0, 11.0],
    ]
}

fn mouth() -> HolesMask {
    HolesMask::from_shapes(vec![HoleShape::Polygon {
        points: mouth_points(),
    }])
    .expect("mask")
}

fn plane(x: f32) -> f32 {
    SLOPE * x
}

/// 1024 m, 1 m samples: the plane inside `FLAT_R`, hills beyond.
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
            let s = t * t * (3.0 - 2.0 * t);
            let hills = 6.0 * (x / 37.0).sin() * (z / 29.0).cos();
            heights.push(plane(x) * (1.0 - s) + hills * s);
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 80.0, -40.0).expect("map")
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
        TerrainSharedDraw::new(device(), map(), &stack, CdlodConfig::default(), ROOT).expect("terrain");
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

/// The apron: the footprint grown 0.5 m from its centroid, a fan on the plane `APRON_DEPTH`
/// under it (CCW from above, normals up the slope).
fn apron() -> regia_render::LabMesh {
    let points = mouth_points();
    let n = points.len() as f32;
    let c = points
        .iter()
        .fold([0.0, 0.0], |a, p| [a[0] + p[0] / n, a[1] + p[1] / n]);
    let normal = Vec3::new(-SLOPE, 1.0, 0.0).normalize().to_array();
    // Colour, tangent and skin defaults from the stock box.
    let base = regia_render::box_mesh([1.0; 3]).0[0];
    let vertex = |x: f32, z: f32| regia_render::LabVertex {
        position: [x, plane(x) - APRON_DEPTH, z],
        normal,
        ..base
    };
    let mut vertices = vec![vertex(c[0], c[1])];
    for p in &points {
        let (dx, dz) = (p[0] - c[0], p[1] - c[1]);
        let len = dx.hypot(dz).max(1e-3);
        vertices.push(vertex(p[0] + dx / len * 0.5, p[1] + dz / len * 0.5));
    }
    let k = points.len() as u32;
    // The points run counter-clockwise in (x, z), which is clockwise seen from +y: flip each
    // triangle so it faces up.
    let indices = (0..k).flat_map(|i| [0, 1 + (i + 1) % k, 1 + i]).collect();
    regia_render::LabMesh {
        vertices,
        indices,
        material: 0,
        ..regia_render::LabMesh::unskinned()
    }
}

/// One camera position on the flight in.
#[derive(Clone, Copy, Debug)]
struct Shot {
    eye: [f32; 3],
    target: [f32; 3],
    fov_y: f32,
    /// Near and far planes, scaled to the distance (as a game camera's depth range would be
    /// for a 2 cm separation to resolve; the cut itself does not depend on it).
    near: f32,
    far: f32,
}

fn render(shot: &Shot, holes: bool, name: &str) -> Vec<u8> {
    let draw = draw(holes);
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: shot.eye,
        target: shot.target,
        fov_y: shot.fov_y,
        near: shot.near,
        far: shot.far,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        // Noon: the apron is lit right up to the cut's edge.
        directional: DirectionalLightUniform::new(sun_travel(), [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![regia_render::LabMaterial {
            base_color: [0.8, 0.04, 0.04, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..regia_render::LabMaterial::default()
        }],
        meshes: vec![apron()],
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
    let pixels = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
    if let Some(dir) = regia_core::brand::env_var_os("TERRAIN_HOLES_DUMP") {
        let path = std::path::Path::new(&dir).join(format!("flythrough_{name}_holes{holes}.png"));
        if let Ok(file) = std::fs::File::create(&path) {
            let mut encoder = png::Encoder::new(file, W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if let Ok(mut writer) = encoder.write_header() {
                let _ = writer.write_image_data(&pixels);
            }
        }
    }
    pixels
}

fn sun_travel() -> [f32; 3] {
    let el = 89.0f32.to_radians();
    let az = 0.3f32;
    [-el.cos() * az.cos(), -el.sin(), -el.cos() * az.sin()]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// Lit terrain (grey).
    Ground,
    /// The apron seen through the cut (red).
    Apron,
    /// The black sky: a crack where ground is predicted.
    Sky,
    /// Not predicted (off the plane).
    Skip,
}

/// The camera ray through pixel position (x, y): the renderer's right-handed look-at with a
/// vertical field of view, built from the camera basis (inverting a 9 km projection in f32 is
/// too coarse at a 0.004 rad field of view).
fn pixel_dir(shot: &Shot, x: f32, y: f32) -> Vec3 {
    let fwd = (Vec3::from_array(shot.target) - Vec3::from_array(shot.eye)).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd);
    let th = (0.5 * shot.fov_y).tan();
    let aspect = W as f32 / H as f32;
    let sx = x / W as f32 * 2.0 - 1.0;
    let sy = 1.0 - y / H as f32 * 2.0;
    (fwd + right * (sx * th * aspect) + up * (sy * th)).normalize()
}

/// What pixel (x, y) shows: its ray against the plane and the exact footprint.
fn predict(shot: &Shot, mask: &HolesMask, x: f32, y: f32) -> Class {
    let dir = pixel_dir(shot, x, y);
    let eye = Vec3::from_array(shot.eye);
    // eye.y + t·d.y = SLOPE · (eye.x + t·d.x)
    let denom = dir.y - SLOPE * dir.x;
    if denom.abs() < 1e-6 {
        return Class::Skip;
    }
    let t = (SLOPE * eye.x - eye.y) / denom;
    if t <= 0.0 {
        return Class::Skip;
    }
    let p = eye + dir * t;
    if p.x.hypot(p.z) > FLAT_R - 5.0 {
        return Class::Skip;
    }
    if mask.contains(p.x, p.z) {
        Class::Apron
    } else {
        Class::Ground
    }
}

fn classify(px: &[u8], x: u32, y: u32) -> Class {
    let i = ((y * W + x) * 4) as usize;
    let (r, g, b) = (i32::from(px[i]), i32::from(px[i + 1]), i32::from(px[i + 2]));
    let max = r.max(g).max(b);
    if max < 12 {
        Class::Sky
    } else if r > 2 * g && r > 2 * b {
        Class::Apron
    } else if (r - g).abs() * 4 < max && (r - b).abs() * 4 < max {
        Class::Ground
    } else {
        Class::Skip
    }
}

/// Judged pixels and what they showed.
#[derive(Debug, Default)]
struct Tally {
    ground: u32,
    ground_ok: u32,
    /// Judged ground pixels showing the sky.
    cracks: u32,
    apron: u32,
    apron_ok: u32,
    /// Judged apron pixels showing terrain.
    skirt: u32,
}

/// Judges every pixel whose neighbourhood (two pixels for ground, five for the apron, which
/// takes the edge's shadow-map filtering) is predicted the same.
fn tally(shot: &Shot, px: &[u8], mask: &HolesMask) -> Tally {
    let grid: Vec<Class> = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| predict(shot, mask, x as f32 + 0.5, y as f32 + 0.5))
        .collect();
    let at = |x: i32, y: i32| {
        grid[(y.clamp(0, H as i32 - 1) as u32 * W + x.clamp(0, W as i32 - 1) as u32) as usize]
    };
    let mut out = Tally::default();
    for y in 0..H as i32 {
        for x in 0..W as i32 {
            let c = at(x, y);
            let margin = match c {
                Class::Ground => 2,
                Class::Apron => 5,
                _ => continue,
            };
            if !(-margin..=margin).all(|dy| (-margin..=margin).all(|dx| at(x + dx, y + dy) == c)) {
                continue;
            }
            let seen = classify(px, x as u32, y as u32);
            match c {
                Class::Ground => {
                    out.ground += 1;
                    out.ground_ok += u32::from(seen == Class::Ground);
                    out.cracks += u32::from(seen == Class::Sky);
                }
                _ => {
                    out.apron += 1;
                    out.apron_ok += u32::from(seen == Class::Apron);
                    out.skirt += u32::from(seen == Class::Ground);
                }
            }
        }
    }
    out
}

/// The level of the node drawing the mouth's centre from `eye`.
fn level_under_mouth(map: &TerrainMap, pyramid: &MinMaxPyramid, eye: [f32; 3]) -> u32 {
    let selection = select_nodes(
        map,
        pyramid,
        &CdlodConfig::default(),
        ROOT,
        [eye[0], eye[2]],
        None,
    );
    selection
        .nodes
        .iter()
        .find(|d| {
            let (x, z, s) = d.node.world_rect(map.world_width(), ROOT);
            (x..x + s).contains(&0.5) && (z..z + s).contains(&0.5)
        })
        .map(|d| d.node.level)
        .expect("a node under the mouth")
}

/// The flight in: along a descending line toward the mouth, one shot per LOD level (the first
/// distance that selects it), framing the mouth at about 60 % of the frame height, then from
/// 40 m, then a grazing pass 3 m over the apron toward the far rim.
fn flight() -> Vec<(String, Shot)> {
    let map = map();
    let pyramid = MinMaxPyramid::build(&map, ROOT).expect("pyramid");
    let toward = Vec3::new(0.55, 0.62, 0.55).normalize();
    let mut shots: Vec<(u32, Shot)> = Vec::new();
    // Level 3 (the whole map) only draws from over 4 km away (`range_factor` × node size).
    let mut distance = 9000.0f32;
    while distance > 15.0 {
        let eye = toward * distance;
        let eye = [eye.x, eye.y + plane(eye.x), eye.z];
        let level = level_under_mouth(&map, &pyramid, eye);
        if !shots.iter().any(|(l, _)| *l == level) {
            let fov_y = (2.0 * (20.0 / distance).atan()).min(1.2);
            shots.push((
                level,
                Shot {
                    eye,
                    target: [0.0, 0.0, 0.0],
                    fov_y,
                    near: (distance * 0.02).max(0.25),
                    far: distance * 3.0 + 500.0,
                },
            ));
        }
        distance *= 0.85;
    }
    let mut levels: Vec<u32> = shots.iter().map(|(l, _)| *l).collect();
    levels.sort_unstable();
    assert_eq!(
        levels,
        (0..=ROOT).collect::<Vec<_>>(),
        "the flight must see the mouth at every LOD level"
    );
    let mut out: Vec<(String, Shot)> = shots.into_iter().map(|(l, s)| (format!("lod{l}"), s)).collect();
    // Level 0 again from 40 m, the mouth filling the frame.
    let eye = toward * 40.0;
    out.push((
        "near".into(),
        Shot {
            eye: [eye.x, eye.y + plane(eye.x), eye.z],
            target: [0.0, 0.0, 0.0],
            fov_y: 1.0,
            near: 0.25,
            far: 2000.0,
        },
    ));
    out.push((
        "graze".into(),
        Shot {
            eye: [-12.0, plane(-12.0) + 3.0, -2.0],
            target: [14.0, plane(14.0) - 1.0, 4.0],
            fov_y: 1.1,
            near: 0.25,
            far: 2000.0,
        },
    ));
    out
}

#[test]
fn the_mouth_holds_without_cracks_skirts_or_z_fighting_at_every_lod() {
    let mask = mouth();
    for (name, shot) in flight() {
        let px = render(&shot, true, &name);
        let t = tally(&shot, &px, &mask);
        let pct = |ok: u32, n: u32| f64::from(ok) * 100.0 / f64::from(n.max(1));
        eprintln!(
            "{name} (eye {:?}, fov {:.3}): ground {}/{} ({:.2} %), cracks {}, apron {}/{} ({:.2} %), skirt {}",
            shot.eye,
            shot.fov_y,
            t.ground_ok,
            t.ground,
            pct(t.ground_ok, t.ground),
            t.cracks,
            t.apron_ok,
            t.apron,
            pct(t.apron_ok, t.apron),
            t.skirt
        );
        assert!(t.ground > 1500, "{name}: too little ground judged ({})", t.ground);
        assert!(
            t.apron > 1500,
            "{name}: too little of the mouth judged ({})",
            t.apron
        );
        assert_eq!(t.cracks, 0, "{name}: the sky shows through the ground");
        assert_eq!(t.skirt, 0, "{name}: terrain drawn inside the mouth");
        assert!(
            pct(t.ground_ok, t.ground) > 99.5,
            "{name}: ground where it is not predicted: {:.2} %",
            pct(t.ground_ok, t.ground)
        );
        assert!(
            pct(t.apron_ok, t.apron) > 99.5,
            "{name}: the apron 2 cm under the cut is not clean: {:.2} %",
            pct(t.apron_ok, t.apron)
        );
    }
}

#[test]
fn without_the_cut_the_terrain_covers_the_apron() {
    // The control: uncut, the terrain 2 cm over the apron covers it (all of it near, over 90 %
    // at the far levels, where the far-LOD surface dips under the plane in blocks), so the clean
    // red of the cut frames is the cut's doing, not a depth tie the apron happens to win.
    let mask = mouth();
    // Strict (10 %) everywhere but Metal. Metal rounds depth differently, and the far-LOD surface
    // (which dips under the apron plane in blocks) shows the apron through up to ~12 % at lod2 on
    // Apple M5 (ADR-275); the near level stays exactly zero.
    let max_percent = if device().capabilities.backend == wgpu::Backend::Metal {
        20
    } else {
        10
    };
    for (name, shot) in flight().into_iter().filter(|(n, _)| n.starts_with("lod")) {
        let px = render(&shot, false, &name);
        let t = tally(&shot, &px, &mask);
        eprintln!(
            "{name} uncut: apron judged {}, red {}, grey {}",
            t.apron, t.apron_ok, t.skirt
        );
        assert!(
            t.apron > 1500,
            "{name}: too little of the mouth judged ({})",
            t.apron
        );
        assert!(
            t.apron_ok * 100 < t.apron * max_percent,
            "{name}: the apron shows through uncut ground: {} of {}",
            t.apron_ok,
            t.apron
        );
        if name == "lod0" {
            assert_eq!(
                t.apron_ok, 0,
                "{name}: the apron shows through uncut ground near by"
            );
        }
    }
}
