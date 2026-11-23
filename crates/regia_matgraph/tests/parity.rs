// SPDX-License-Identifier: MPL-2.0
//! Material graphs on the GPU (lavapipe in CI): two existing hand-written materials rebuilt as
//! graphs must render the same frame (every channel within 1/255), and a graph-only layered
//! rock must render a sensible, sand-layered frame.
//!
//! - Hard-surface panel: `regia_render::hard_surface::hull_closeup_scene` (parallax, paint,
//!   curvature edge wear, faded detail grain and whiteout detail normal, clearcoat lobe).
//! - Scatter rock: the Mars stand-in rocks of `regia_terrain::scatter::rock` (baked vertex
//!   layers × tint), one graph with one instance per rock style.
//!
//! `REGIA_EVIDENCE=1` writes the frames to `docs/evidence/material-graph/`; frames always go
//! to `<temp>/material-graph`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_core::glam::{Mat4, Vec3};
use regia_matgraph::{
    MaterialGraph, MaterialInstance, ParamValue, ShaderCache, default_params, instance_material, lab_material,
};
use regia_render::hard_surface::{
    hull_closeup_quality, hull_closeup_scene, pack_aux, pack_detail_image, pack_detail_normal,
};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabTexture,
    LabVertex, RenderDevice,
};
use regia_terrain::material::{LayerStyle, Rgba8Image};
use regia_terrain::scatter::rock::{RockPalette, procedural_rock};

const HULL: &str = include_str!("../assets/hull_panel.matgraph.ron");
const ROCK: &str = include_str!("../assets/scatter_rock.matgraph.ron");
const LAYERED: &str = include_str!("../assets/layered_rock.matgraph.ron");

fn out_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![std::env::temp_dir().join("material-graph")];
    if regia_core::brand::env_var("REGIA_EVIDENCE").ok().as_deref() == Some("1") {
        dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/evidence/material-graph"));
    }
    dirs
}

fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("evidence dir");
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png pixels");
    writer.finish().expect("png finish");
}

fn save(name: &str, scene: &LabScene, pixels: &[u8]) {
    let file = format!("{name}.png");
    for (k, dir) in out_dirs().into_iter().enumerate() {
        // The first is the temp preview (best-effort); `REGIA_EVIDENCE` adds the docs folder,
        // which must be written.
        if k == 0 {
            write_preview(&dir.join(&file), scene.width, scene.height, pixels);
        } else {
            write_png(&dir.join(&file), scene.width, scene.height, pixels);
        }
    }
}

/// A fresh renderer per frame, so no cache entry is shared between the two versions.
fn render(device: &RenderDevice, scene: &LabScene, quality: FrameQuality) -> Vec<u8> {
    let mut renderer = FrameRenderer::new();
    renderer
        .render_offscreen(device, &scene.as_frame_scene_with_quality(quality))
        .expect("offscreen frame")
        .pixels
}

/// (largest channel difference, pixels with any channel different, diff visualisation ×16).
fn compare(a: &[u8], b: &[u8]) -> (u8, usize, Vec<u8>) {
    assert_eq!(a.len(), b.len());
    let mut max = 0u8;
    let mut differing = 0usize;
    let mut vis = Vec::with_capacity(a.len());
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        max = max.max(d);
        if d > 0 {
            differing += 1;
        }
        let v = d.saturating_mul(16);
        vis.extend_from_slice(&[v, v, v, 255]);
    }
    (max, differing, vis)
}

fn luma(p: &[u8; 4]) -> f64 {
    0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2])
}

fn texture(image: &Rgba8Image) -> LabTexture {
    LabTexture {
        width: image.width,
        height: image.height,
        rgba: image.data.clone(),
    }
}

fn param(graph: &MaterialGraph, name: &str) -> ParamValue {
    graph
        .params
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.value)
        .unwrap_or_else(|| panic!("param {name}"))
}

fn assert_parity(
    device: &RenderDevice,
    name: &str,
    hand: &LabScene,
    graph: &LabScene,
    quality: FrameQuality,
) {
    let a = render(device, hand, quality);
    let b = render(device, graph, quality);
    let (max, differing, vis) = compare(&a, &b);
    save(&format!("{name}_hand"), hand, &a);
    save(&format!("{name}_graph"), graph, &b);
    save(&format!("{name}_diff16"), hand, &vis);
    let lit = a.as_chunks::<4>().0.iter().filter(|p| luma(p) > 8.0).count();
    eprintln!(
        "{name}: max channel diff {max}/255, {differing} of {} pixels differ, {lit} lit pixels",
        a.len() / 4
    );
    assert!(
        lit > a.len() / 4 / 10,
        "{name}: frame is mostly black ({lit} lit pixels)"
    );
    assert!(
        max <= 1,
        "{name}: graph frame differs from the hand-written one by {max}/255"
    );
}

#[test]
fn hull_panel_graph_matches_the_hand_written_material() {
    let device = RenderDevice::headless().expect("headless device");
    let graph = MaterialGraph::from_ron(HULL).expect("hull graph");
    let compiled = regia_matgraph::compile(&graph).expect("compile hull");
    regia_render::validate_surface_graph(&compiled.wgsl).expect("spirv-val hull graph");

    let hand = hull_closeup_scene();
    let hull = &hand.materials[0];
    // The graph's defaults are the hand-written values, so the comparison is like for like.
    let rgb = |c: [f32; 4]| ParamValue::Color([c[0], c[1], c[2]]);
    assert_eq!(param(&graph, "tint"), rgb(hull.base_color));
    assert_eq!(param(&graph, "roughness"), ParamValue::Float(hull.roughness));
    assert_eq!(param(&graph, "metallic"), ParamValue::Float(hull.metallic));
    let hs = &hull.hard_surface;
    assert_eq!(param(&graph, "parallax_scale"), ParamValue::Float(hs.pom_scale));
    assert_eq!(param(&graph, "wear_tint"), ParamValue::Color(hs.edge_tint));
    assert_eq!(param(&graph, "wear_bias"), ParamValue::Float(hs.edge_bias));
    assert_eq!(
        param(&graph, "wear_contrast"),
        ParamValue::Float(hs.edge_contrast)
    );
    assert_eq!(
        param(&graph, "wear_intensity"),
        ParamValue::Float(hs.edge_intensity)
    );
    assert_eq!(
        param(&graph, "wear_roughness"),
        ParamValue::Float(hs.edge_roughness_bias)
    );
    assert_eq!(
        param(&graph, "wear_metallic"),
        ParamValue::Float(hs.edge_metallic_bias)
    );
    assert_eq!(
        param(&graph, "detail_scale"),
        ParamValue::Vec2(hs.detail_uv_scale)
    );
    assert_eq!(
        param(&graph, "detail_albedo"),
        ParamValue::Float(hs.detail_albedo_strength)
    );
    assert_eq!(
        param(&graph, "detail_normal"),
        ParamValue::Float(hs.detail_normal_strength)
    );
    assert_eq!(
        param(&graph, "detail_fade_end"),
        ParamValue::Float(hs.detail_fade_end)
    );
    assert_eq!(
        param(&graph, "detail_fade_range"),
        ParamValue::Float(hs.detail_fade_end - hs.detail_fade_start)
    );
    assert_eq!(graph.shading.clearcoat, hs.clearcoat);
    assert_eq!(graph.shading.clearcoat_roughness, hs.clearcoat_roughness);

    // Channel-packed images, exactly as the hand-written path uploads them.
    let mut images = BTreeMap::new();
    images.insert("paint".to_string(), hull.albedo.clone().expect("paint"));
    images.insert(
        "grain".to_string(),
        pack_detail_image(hull.detail_albedo.as_ref(), hull.detail_roughness.as_ref()).expect("grain"),
    );
    images.insert(
        "flake".to_string(),
        pack_detail_normal(hull.detail_normal.as_ref(), hull.detail_mask.as_ref()).expect("flake"),
    );
    images.insert(
        "relief".to_string(),
        pack_aux(hull.height_map.as_ref(), hull.curvature_map.as_ref()).expect("relief"),
    );
    let mut graph_scene = hull_closeup_scene();
    graph_scene.materials[0] =
        lab_material(&graph, &compiled, default_params(&graph, 0.0), &images).expect("hull material");
    assert_parity(&device, "hull_panel", &hand, &graph_scene, hull_closeup_quality());

    // Control: the graph really drives the frame. An instance that repaints the hull red
    // (same code, same pipelines) must move most hull pixels.
    let mut red = MaterialInstance::new("hull_panel.matgraph.ron");
    red.set("tint", ParamValue::Color([0.6, 0.05, 0.03]));
    let mut red_scene = hull_closeup_scene();
    red_scene.materials[0] = instance_material(&graph, &compiled, &red, &images, 0.0).expect("red hull");
    let base = render(&device, &graph_scene, hull_closeup_quality());
    let repainted = render(&device, &red_scene, hull_closeup_quality());
    save("hull_panel_red_instance", &red_scene, &repainted);
    let (_, moved, _) = compare(&base, &repainted);
    eprintln!("hull_panel red instance: {moved} pixels change");
    assert!(moved > 50_000, "a red tint instance moved only {moved} pixels");
}

fn ground() -> LabMesh {
    let h = 4.0;
    let mut mesh = LabMesh::unskinned();
    for (p, uv) in [
        ([-h, 0.0, -h], [0.0, 0.0]),
        ([-h, 0.0, h], [0.0, 1.0]),
        ([h, 0.0, h], [1.0, 1.0]),
        ([h, 0.0, -h], [1.0, 0.0]),
    ] {
        mesh.vertices
            .push(LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], uv));
    }
    mesh.indices = vec![0, 1, 2, 0, 2, 3];
    mesh
}

/// Five Mars stand-in rocks (one per palette style) on a dusty ground, low sun.
fn rock_scene(materials: Vec<LabMaterial>) -> LabScene {
    let mut scene = LabScene {
        width: 960,
        height: 540,
        eye: [0.0, 1.1, 3.1],
        target: [0.0, 0.25, 0.0],
        fov_y: 0.62,
        near: 0.05,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 13.5,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([-0.55, -0.55, -0.62], [1.0, 0.94, 0.86], 90_000.0),
        local_lights: Vec::new(),
        materials,
        meshes: Vec::new(),
    };
    let mut floor = ground();
    floor.material = scene.materials.len();
    scene.materials.push(LabMaterial {
        base_color: [0.24, 0.15, 0.09, 1.0],
        roughness: 0.95,
        ..LabMaterial::default()
    });
    scene.meshes.push(floor);
    for style in 0..5usize {
        let asset = procedural_rock(0x5eed_0000 + style as u64, RockPalette::Mars.style(style));
        let lod = &asset.lods[0];
        let mut mesh = LabMesh::unskinned();
        mesh.vertices = lod.vertices.clone();
        mesh.indices = lod.indices.clone();
        mesh.material = style;
        let x = (style as f32 - 2.0) * 0.62;
        let scale = 0.5 + 0.06 * style as f32;
        mesh.model = (Mat4::from_translation(Vec3::new(x, 0.0, -0.1 * (style % 2) as f32))
            * Mat4::from_rotation_y(0.7 * style as f32)
            * Mat4::from_scale(Vec3::splat(scale)))
        .to_cols_array_2d();
        scene.meshes.push(mesh);
    }
    scene
}

#[test]
fn scatter_rock_graph_instances_match_the_stand_in_rocks() {
    let device = RenderDevice::headless().expect("headless device");
    let graph = MaterialGraph::from_ron(ROCK).expect("rock graph");
    let mut cache = ShaderCache::new();
    let hand_materials: Vec<LabMaterial> = (0..5)
        .map(|style| procedural_rock(0x5eed_0000 + style as u64, RockPalette::Mars.style(style)).material)
        .collect();
    let mut graph_materials = Vec::new();
    for hand in &hand_materials {
        // One graph, one instance per rock style: only the uniform block differs.
        let (compiled, _) = cache.get_or_compile(&graph).expect("compile rock");
        let mut instance = MaterialInstance::new("scatter_rock.matgraph.ron");
        instance
            .set(
                "tint",
                ParamValue::Color([hand.base_color[0], hand.base_color[1], hand.base_color[2]]),
            )
            .set("roughness", ParamValue::Float(hand.roughness));
        graph_materials.push(
            instance_material(&graph, &compiled, &instance, &BTreeMap::new(), 0.0).expect("rock material"),
        );
    }
    let stats = cache.stats();
    assert_eq!((stats.misses, stats.hits, stats.entries), (1, 4, 1), "{stats:?}");
    let keys: Vec<u64> = graph_materials
        .iter()
        .filter_map(|m| m.surface_graph.as_ref().map(|g| g.key))
        .collect();
    assert!(
        keys.windows(2).all(|w| w[0] == w[1]),
        "instances share one pipeline key"
    );
    let mut quality = FrameQuality::LAB;
    quality.parallax_occlusion = false;
    assert_parity(
        &device,
        "scatter_rock",
        &rock_scene(hand_materials),
        &rock_scene(graph_materials),
        quality,
    );
}

#[test]
fn layered_rock_graph_renders_sand_on_the_tops() {
    let device = RenderDevice::headless().expect("headless device");
    let graph = MaterialGraph::from_ron(LAYERED).expect("layered graph");
    let compiled = regia_matgraph::compile(&graph).expect("compile layered");
    regia_render::validate_surface_graph(&compiled.wgsl).expect("spirv-val layered graph");
    let rock = LayerStyle::Rock.bake(256, 11);
    let sand = LayerStyle::Sand.bake(256, 5);
    let mut images = BTreeMap::new();
    images.insert("rock_albedo".to_string(), texture(&rock.albedo));
    images.insert("rock_normal".to_string(), texture(&rock.normal));
    images.insert("rock_height".to_string(), texture(&rock.height));
    images.insert("sand_albedo".to_string(), texture(&sand.albedo));
    images.insert("sand_height".to_string(), texture(&sand.height));

    let build = |amount: f32| -> Vec<LabMaterial> {
        (0..5)
            .map(|style| {
                let hand =
                    procedural_rock(0x5eed_0000 + style as u64, RockPalette::Mars.style(style)).material;
                let mut instance = MaterialInstance::new("layered_rock.matgraph.ron");
                // The stand-in's style colour drives the stone tint (scaled to the bake's albedo).
                let k = 4.0;
                instance
                    .set(
                        "rock_tint",
                        ParamValue::Color([
                            hand.base_color[0] * k,
                            hand.base_color[1] * k,
                            hand.base_color[2] * k,
                        ]),
                    )
                    .set("sand_amount", ParamValue::Float(amount));
                instance_material(&graph, &compiled, &instance, &images, 0.0).expect("layered material")
            })
            .collect()
    };
    let mut quality = FrameQuality::LAB;
    quality.parallax_occlusion = false;
    let sandy_scene = rock_scene(build(1.0));
    let sandy = render(&device, &sandy_scene, quality);
    let bare = render(&device, &rock_scene(build(0.0)), quality);
    save("layered_rock", &sandy_scene, &sandy);
    save("layered_rock_bare", &sandy_scene, &bare);
    let (_, changed, _) = compare(&sandy, &bare);
    let lit = sandy.as_chunks::<4>().0.iter().filter(|p| luma(p) > 8.0).count();
    let mean: f64 = sandy.as_chunks::<4>().0.iter().map(luma).sum::<f64>() / (sandy.len() / 4) as f64;
    eprintln!("layered rock: {changed} pixels change with sand, {lit} lit, mean luma {mean:.1}");
    assert!(lit > sandy.len() / 4 / 2, "layered rock frame is mostly black");
    assert!(changed > 5_000, "sand layer changed only {changed} pixels");
    assert!((25.0..=210.0).contains(&mean), "mean luma {mean:.1}");
}

/// A preview image for a person to look at: written best-effort under the OS temp directory. A
/// preview is not asserted on, so a folder another user owns (a shared `/tmp`) or a read-only temp
/// volume prints a note instead of failing a correct render (audit-v035-flaky M6).
fn write_preview(path: &Path, width: u32, height: u32, pixels: &[u8]) {
    let written = (|| -> Result<(), Box<dyn std::error::Error>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::File::create(path)?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(pixels)?;
        writer.finish()?;
        Ok(())
    })();
    if let Err(err) = written {
        eprintln!("preview {} not written: {err}", path.display());
    }
}
