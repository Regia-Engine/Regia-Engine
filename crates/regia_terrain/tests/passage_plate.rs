// SPDX-License-Identifier: MPL-2.0
//! Plan row W-07 (ADR-8175): a GPU plate of the Switchbacks bore drawn from the cooked render mesh
//! (`regia_world_geom::render`), for the lead's review of the bore look. No golden: the test checks the frames are
//! lit and that the cooked mesh is what is drawn, and writes the plates when `REGIA_DUMP_DIR` is set:
//!
//! ```text
//! REGIA_DUMP_DIR=/tmp/out cargo test -p regia_terrain --test passage_plate -- --nocapture
//! ```
//!
//! writes `passage_plate/{collision_flat,cooked,cooked_pieces,sheet}.png`: the canonical (collision) triangles drawn
//! with a flat material as DO27's shell drew them; the cooked render mesh with the box-projected rock texture and a
//! per-vertex tint (the vertices the subdivision adds); the cooked mesh with every piece of the subdivision grid
//! coloured, to show the grid; and the three side by side.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::OnceLock;

use regia_core::glam::Vec3;
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer,
    LabLocalLight, LabMaterial, LabMesh, LabScene, LabSpotLight, LabTexture, LabVertex, OutdoorParams,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, TimeOfDay,
};
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, CookOutput, GroundInput, cook};
use regia_world_geom::cooked::split_file;
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::render::{LayerRender, PresentationCooked};

const W: u32 = 480;
const H: u32 = 270;

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// The Switchbacks map cooked on its flat -256 m canyon floor.
fn cooked_switchbacks() -> CookOutput {
    let terrain =
        TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), 1280.0).with_zero_counts(16384);
    let ground = GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 16],
    };
    let passages = PassagesDocument::from_layout(for_map("switchbacks").unwrap())
        .to_ron()
        .unwrap();
    let read = |_: &str| Ok(passages.as_bytes().to_vec());
    let doc = "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"sb.passages.ron\"))])";
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    match run(doc) {
        Ok(out) => out,
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            let waived = waive_ramp_facets(doc, &report).expect("waive ramp facets");
            run(&waived).unwrap_or_else(|e| panic!("{e} ({})", e.code()))
        }
        Err(e) => panic!("{e} ({})", e.code()),
    }
}

fn m(p: [i32; 3]) -> [f32; 3] {
    p.map(|c| c as f32 / 1000.0)
}

fn normal(t: &[[f32; 3]; 3]) -> [f32; 3] {
    let a = Vec3::from_array(t[1]) - Vec3::from_array(t[0]);
    let b = Vec3::from_array(t[2]) - Vec3::from_array(t[0]);
    a.cross(b).normalize_or_zero().to_array()
}

/// Box projection: the two axes across the face's dominant normal axis, in UV tiles.
fn box_uv(p: [f32; 3], n: [f32; 3], tile: f32) -> [f32; 2] {
    let (ax, ay, az) = (n[0].abs(), n[1].abs(), n[2].abs());
    if ay >= ax && ay >= az {
        [p[0] / tile, p[2] / tile]
    } else if ax >= az {
        [p[2] / tile, p[1] / tile]
    } else {
        [p[0] / tile, p[1] / tile]
    }
}

/// A deterministic 0..1 hash of a millimetre position.
fn hash01(p: [i32; 3]) -> f32 {
    let mut h: u32 = 0x811c_9dc5;
    for c in p {
        for b in c.to_le_bytes() {
            h ^= u32::from(b);
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// A small procedural rock albedo (sRGB bytes): blotches over a basalt grey.
fn rock_texture() -> LabTexture {
    let n = 128u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let v = 0.55
                + 0.45 * hash01([x as i32 / 4, y as i32 / 4, 7]) * 0.6
                + 0.2 * hash01([x as i32, y as i32, 3]);
            let g = (v.clamp(0.0, 1.0) * 150.0) as u8;
            rgba.extend_from_slice(&[g, (f32::from(g) * 0.93) as u8, (f32::from(g) * 0.85) as u8, 255]);
        }
    }
    LabTexture {
        width: n,
        height: n,
        rgba,
    }
}

enum Look {
    /// The canonical triangles, flat material (what the shell drew).
    CollisionFlat,
    /// The cooked render mesh, textured, per-vertex tint.
    Cooked,
    /// The cooked render mesh, one colour per subdivision piece parity.
    Pieces,
}

fn meshes(layer: &LayerRender, canonical: &[[[i32; 3]; 3]], look: &Look) -> Vec<LabMesh> {
    let tile = f32::from_bits(layer.uv_tile_m_bits);
    let mut vertices = Vec::new();
    let tris: Vec<([[i32; 3]; 3], u16)> = match look {
        Look::CollisionFlat => canonical.iter().map(|t| (*t, 0)).collect(),
        Look::Cooked | Look::Pieces => layer
            .triangles_mm()
            .into_iter()
            .zip(&layer.triangles)
            .map(|(t, r)| (t, r.source.piece))
            .collect(),
    };
    for (t, piece) in &tris {
        let p = t.map(m);
        let n = normal(&p);
        for (k, q) in p.iter().enumerate() {
            let mut v = LabVertex::from_pos_normal_uv(*q, n, box_uv(*q, n, tile));
            v.color = match look {
                Look::CollisionFlat => [1.0; 4],
                Look::Cooked => {
                    let s = 0.8 + 0.2 * hash01(t[k]);
                    [s, s, s, 1.0]
                }
                Look::Pieces => {
                    if piece % 2 == 0 {
                        [1.0, 0.75, 0.55, 1.0]
                    } else {
                        [0.55, 0.75, 1.0, 1.0]
                    }
                }
            };
            vertices.push(v);
        }
    }
    let indices = (0..vertices.len() as u32).collect();
    vec![LabMesh {
        vertices,
        indices,
        material: usize::from(matches!(look, Look::Cooked)),
        ..LabMesh::unskinned()
    }]
}

fn render(meshes: Vec<LabMesh>, eye: [f32; 3], target: [f32; 3]) -> Vec<u8> {
    static RENDERER: OnceLock<std::sync::Mutex<FrameRenderer>> = OnceLock::new();
    let renderer = RENDERER.get_or_init(|| std::sync::Mutex::new(FrameRenderer::new()));
    let mut renderer = renderer.lock().unwrap();
    renderer.set_camera_cut_detection(None);
    let dir = (Vec3::from_array(target) - Vec3::from_array(eye)).normalize();
    let headlight = LabLocalLight::Spot(LabSpotLight {
        position: eye,
        direction: dir.to_array(),
        color: [1.0, 0.95, 0.85],
        intensity: 60_000.0,
        range: 120.0,
        inner_angle: 0.3,
        outer_angle: 0.7,
        cast_shadows: false,
        source_radius: None,
    });
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 6.0,
        ..PostSettings::golden_deterministic()
    };
    let lab = LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 1.1,
        near: 0.1,
        far: 600.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 6.0,
        ambient_lux: 20.0,
        double_sided: true,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([0.25, -0.8, 0.55], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: vec![headlight],
        materials: vec![
            LabMaterial {
                base_color: [0.45, 0.30, 0.18, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [1.0, 1.0, 1.0, 1.0],
                roughness: 0.85,
                albedo: Some(rock_texture()),
                ..LabMaterial::default()
            },
        ],
        meshes,
    };
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    renderer.set_outdoor(OutdoorParams {
        time: Some(TimeOfDay::noon()),
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Earth),
        ..OutdoorParams::default()
    });
    renderer
        .render_offscreen(device(), &frame)
        .expect("plate frame")
        .pixels
}

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(file) = std::fs::File::create(path) else {
        eprintln!("passage_plate: cannot create {}", path.display());
        return;
    };
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(rgba);
    }
}

fn mean_luma(px: &[u8]) -> f64 {
    let n = px.len() / 4;
    px.as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .sum::<f64>()
        / n.max(1) as f64
}

/// The Switchbacks bore from inside the tube, drawn three ways. The cooked plates must be lit and differ from the
/// flat collision draw (the cooked mesh, not the canonical triangles, is on screen).
#[test]
fn switchbacks_bore_plate_from_the_cooked_render_mesh() {
    let out = cooked_switchbacks();
    let parts = split_file(&out.file.encode().unwrap()).unwrap();
    let pres = PresentationCooked::decode(&parts.presentation).unwrap();
    let layer = &pres.layers[0];
    let canonical: Vec<[[i32; 3]; 3]> = out
        .file
        .canonical
        .triangles_sorted()
        .into_iter()
        .map(|(o, t)| t.v.map(|p| [p[0] + o[0], p[1], p[2] + o[1]]))
        .collect();
    // Inside the tube, a third of the way along, looking down the bore.
    let layout = for_map("switchbacks").unwrap();
    let sampled = layout.sample(None);
    let tube = &sampled.passages[0];
    let k = tube.samples.len() / 3;
    let (a, b) = (
        tube.samples[k].p,
        tube.samples[(k + 6).min(tube.samples.len() - 1)].p,
    );
    let eye = [a[0], a[1] + 3.0, a[2]];
    let target = [b[0], b[1] + 2.5, b[2]];
    let flat = render(meshes(layer, &canonical, &Look::CollisionFlat), eye, target);
    let cooked = render(meshes(layer, &canonical, &Look::Cooked), eye, target);
    let pieces = render(meshes(layer, &canonical, &Look::Pieces), eye, target);
    let (lf, lc, lp) = (mean_luma(&flat), mean_luma(&cooked), mean_luma(&pieces));
    eprintln!(
        "passage_plate: {} canonical, {} render triangles; mean luma flat {lf:.1}, cooked {lc:.1}, pieces {lp:.1}",
        canonical.len(),
        layer.triangles.len()
    );
    assert!(lf > 3.0 && lc > 3.0 && lp > 3.0, "a black plate");
    let moved = flat
        .as_chunks::<4>()
        .0
        .iter()
        .zip(cooked.as_chunks::<4>().0)
        .filter(|(x, y)| x.iter().zip(y.iter()).any(|(p, q)| p.abs_diff(*q) > 8))
        .count();
    assert!(
        moved > (W * H / 10) as usize,
        "the cooked plate looks like the flat collision draw ({moved} px moved)"
    );
    if let Some(dir) = regia_core::brand::env_var_os("REGIA_DUMP_DIR") {
        let dir = Path::new(&dir).join("passage_plate");
        write_png(&dir.join("collision_flat.png"), W, H, &flat);
        write_png(&dir.join("cooked.png"), W, H, &cooked);
        write_png(&dir.join("cooked_pieces.png"), W, H, &pieces);
        let mut sheet = vec![0u8; (W * 3 * H * 4) as usize];
        for (col, img) in [&flat, &cooked, &pieces].into_iter().enumerate() {
            for y in 0..H as usize {
                let row = y * W as usize * 4;
                let dst = (y * W as usize * 3 + col * W as usize) * 4;
                sheet[dst..dst + W as usize * 4].copy_from_slice(&img[row..row + W as usize * 4]);
            }
        }
        write_png(&dir.join("sheet.png"), W * 3, H, &sheet);
        eprintln!("passage_plate: wrote {}", dir.display());
    }
}
