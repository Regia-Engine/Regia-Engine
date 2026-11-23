// SPDX-License-Identifier: MPL-2.0
//! `specs/27` § 4.2 stage 5: the reference path tracer shades the same material graph the forward
//! pass does (ADR-2725). Three graphs compiled by `regia_matgraph` (a procedural colour that reads
//! the material clock, a texture with a tangent-space normal map, a layered metal/dielectric
//! blend) are drawn by the forward pass and traced from the same frame; the tracer's `direct` AOV
//! is held to the forward frame's linear HDR per object, bound to the § 2.2 "Direct lighting +
//! materials" row (mean relative RMSE ≤ 1 %, FLIP ≤ 0.03). The forward frame comes from
//! `FrameRenderer::request_hdr_capture`, as in `forward_parity.rs`.

use std::collections::BTreeMap;

use regia_core::glam::{Mat4, Vec3};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtImage, PtSettings, PtSky};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabTexture,
    LabVertex, PostSettings, RenderDevice, ShadowCast, SurfaceGraph,
};

const WIDTH: u32 = 192;
const HEIGHT: u32 = 96;
/// The material clock both sides freeze (seconds).
const CLOCK: f32 = 2.5;

/// A procedural colour: gradient noise over the world position, shifted by the material clock.
const PROCEDURAL: &str = r#"(
    format_version: 4,
    name: "pt_procedural",
    params: [
        (name: "scale", value: Float(2.5)),
        (name: "warm", value: Color((0.85, 0.35, 0.12))),
        (name: "cool", value: Color((0.10, 0.35, 0.80))),
        (name: "rough", value: Float(0.6)),
    ],
    textures: [],
    nodes: {
        "wp": WorldPosition,
        "scale": Param("scale"),
        "p": Mul("wp", "scale"),
        "t": Time,
        "pt": Add("p", "t"),
        "n": Noise(input: "pt", octaves: 3),
        "warm": Param("warm"),
        "cool": Param("cool"),
        "base": Lerp(a: "cool", b: "warm", t: "n"),
        "rough": Param("rough"),
    },
    output: (
        base_color: Some("base"),
        roughness: Some("rough"),
    ),
)"#;

/// A texture and a tangent-space normal map.
const TEXTURED: &str = r#"(
    format_version: 4,
    name: "pt_textured",
    params: [
        (name: "tint", value: Color((1.0, 0.95, 0.9))),
        (name: "strength", value: Float(0.8)),
    ],
    textures: [
        (name: "albedo", path: "albedo.png", color_space: Srgb),
        (name: "normal", path: "normal.png", color_space: Linear),
    ],
    nodes: {
        "uv": TexCoord,
        "a": Sample(texture: "albedo", uv: "uv"),
        "a_rgb": Swizzle(input: "a", channels: "rgb"),
        "tint": Param("tint"),
        "base": Mul("a_rgb", "tint"),
        "n": Sample(texture: "normal", uv: "uv"),
        "n_unpacked": UnpackNormal("n"),
        "strength": Param("strength"),
        "normal": NormalStrength(normal: "n_unpacked", strength: "strength"),
        "rough": Float(0.55),
    },
    output: (
        base_color: Some("base"),
        roughness: Some("rough"),
        normal: Some("normal"),
        normal_space: Tangent,
    ),
)"#;

/// Two layers (a rough red dielectric and a gold metal) blended by a noise mask.
const LAYERED: &str = r#"(
    format_version: 4,
    name: "pt_layered",
    params: [
        (name: "paint", value: Color((0.7, 0.08, 0.05))),
        (name: "gold", value: Color((1.0, 0.78, 0.34))),
        (name: "mask_scale", value: Float(3.0)),
    ],
    textures: [],
    nodes: {
        "wp": WorldPosition,
        "mask_scale": Param("mask_scale"),
        "p": Mul("wp", "mask_scale"),
        "n": Noise(input: "p", octaves: 2),
        "lo": Float(0.4),
        "hi": Float(0.6),
        "mask": Smoothstep(edge0: "lo", edge1: "hi", x: "n"),
        "paint": Param("paint"),
        "gold": Param("gold"),
        "base": Lerp(a: "paint", b: "gold", t: "mask"),
        "zero": Float(0.0),
        "one": Float(1.0),
        "metallic": Lerp(a: "zero", b: "one", t: "mask"),
        "r_paint": Float(0.8),
        "r_gold": Float(0.45),
        "rough": Lerp(a: "r_paint", b: "r_gold", t: "mask"),
    },
    output: (
        base_color: Some("base"),
        metallic: Some("metallic"),
        roughness: Some("rough"),
    ),
)"#;

fn checker() -> LabTexture {
    let n = 16u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let on = (x / 4 + y / 4) % 2 == 0;
            let c = if on {
                [200u8, 170, 90, 255]
            } else {
                [60u8, 110, 150, 255]
            };
            rgba.extend_from_slice(&c);
        }
    }
    LabTexture {
        width: n,
        height: n,
        rgba,
    }
}

/// A smooth bump field as a tangent-space normal map.
fn bumps() -> LabTexture {
    let n = 16u32;
    let mut rgba = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let fx = (x as f32 + 0.5) / n as f32 * std::f32::consts::TAU;
            let fy = (y as f32 + 0.5) / n as f32 * std::f32::consts::TAU;
            let nx = 0.45 * fx.cos();
            let ny = 0.45 * fy.sin();
            let nz = (1.0 - nx * nx - ny * ny).max(0.0).sqrt();
            let q = |v: f32| ((v * 0.5 + 0.5) * 255.0 + 0.5) as u8;
            rgba.extend_from_slice(&[q(nx), q(ny), q(nz), 255]);
        }
    }
    LabTexture {
        width: n,
        height: n,
        rgba,
    }
}

fn material(ron: &str, images: &BTreeMap<String, LabTexture>) -> LabMaterial {
    let graph = regia_matgraph::MaterialGraph::from_ron(ron).expect("graph");
    let compiled = regia_matgraph::compile(&graph).unwrap_or_else(|e| panic!("{e}"));
    regia_matgraph::lab_material(
        &graph,
        &compiled,
        regia_matgraph::default_params(&graph, 0.0),
        images,
    )
    .expect("material")
}

/// A camera-facing quad with tangents (the normal map's frame), UVs over 0..1.
fn quad(half: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let corner = |x: f32, y: f32, u: f32, v: f32| {
        let mut vertex = LabVertex::from_pos_normal_uv([x, y, 0.0], [0.0, 0.0, 1.0], [u, v]);
        vertex.tangent = [1.0, 0.0, 0.0, 1.0];
        vertex
    };
    (
        vec![
            corner(-half, -half, 0.0, 1.0),
            corner(half, -half, 1.0, 1.0),
            corner(half, half, 1.0, 0.0),
            corner(-half, half, 0.0, 0.0),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

struct Frame {
    forward: Vec<[f32; 3]>,
    traced: PtImage,
}

fn post() -> PostSettings {
    PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        taa: false,
        ..PostSettings::default()
    }
}

/// Draws `materials` side by side (spheres, the textured one a quad) and traces the same frame.
fn render(
    device: &RenderDevice,
    tracer: &PathTracer,
    materials: &[LabMaterial],
    clock_in_trace: bool,
) -> Frame {
    let (sphere_v, sphere_i) = regia_render::uv_sphere(1.0, 64, 32);
    let (plane_v, plane_i) = quad(1.0);
    let post = post();
    let meshes: Vec<FrameMesh<'_>> = materials
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let textured = m
                .surface_graph
                .as_deref()
                .is_some_and(|g| g.textures.iter().any(Option::is_some));
            let (vertices, indices, mesh_key) = if textured {
                (&plane_v, &plane_i, 2)
            } else {
                (&sphere_v, &sphere_i, 1)
            };
            FrameMesh {
                is_static: false,
                mesh_key,
                material_key: 100 + i as u64,
                object_key: 200 + i as u64,
                vertices,
                indices,
                material: m,
                model: Mat4::from_translation(Vec3::new((i as f32 - 1.0) * 3.6, 0.0, 0.0)).to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        })
        .collect();
    let scene = FrameScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [0.0, 0.0, 9.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.62,
        near: 0.1,
        far: 100.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(&post),
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([-0.35, -0.5, -0.8], [1.0, 0.98, 0.95], 20_000.0),
        local_lights: &[],
        // Specular AA is a screen-space filter the unjittered trace does not apply (as in
        // `forward_parity.rs`).
        quality: FrameQuality {
            specular_aa: false,
            ..FrameQuality::LAB
        },
        meshes,
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_material_clock(CLOCK, CLOCK);
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, &scene).expect("forward frame");
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("hdr readback")
        .expect("the post stack ran, so the HDR copy exists");
    let pt_scene = from_frame(
        &scene,
        &ExtractOptions {
            sky: Some(PtSky::None),
            material_clock: if clock_in_trace {
                renderer.material_clock()
            } else {
                None
            },
            ..ExtractOptions::default()
        },
    );
    let settings = PtSettings {
        spp: 1,
        max_bounces: 1,
        jitter: false,
        ..PtSettings::default()
    };
    let traced = tracer.render(&pt_scene, &settings).expect("trace");
    Frame {
        forward: hdr.rgb,
        traced,
    }
}

fn lum(c: [f32; 3]) -> f64 {
    0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2])
}

/// Interior pixels of object `object` (its third of the image) that the sun lights.
fn mask(img: &PtImage, object: usize) -> Vec<usize> {
    let (w, h) = (img.width as usize, img.height as usize);
    let third = w / 3;
    let lit: Vec<f64> = img.direct.iter().map(|c| lum(*c)).filter(|v| *v > 0.0).collect();
    let floor = 0.01 * lit.iter().sum::<f64>() / lit.len().max(1) as f64;
    let mut out = Vec::new();
    for y in 1..h - 1 {
        for x in (object * third).max(1)..((object + 1) * third).min(w - 1) {
            let i = y * w + x;
            let inside = [i, i - 1, i + 1, i - w, i + w]
                .iter()
                .all(|&j| img.depth[j] > 0.0);
            if inside && img.shadow[i] >= 0.5 && lum(img.direct[i]) >= floor {
                out.push(i);
            }
        }
    }
    out
}

/// Display encoding for FLIP: exposure that puts the mean at 0.18, clamp, sRGB.
fn display(values: &[[f32; 3]], scale: f32) -> Vec<[f32; 3]> {
    let encode = |v: f32| {
        let v = (v * scale).clamp(0.0, 1.0);
        if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    };
    values.iter().map(|c| c.map(encode)).collect()
}

struct Score {
    pixels: usize,
    rel_rmse: f32,
    flip: f32,
}

fn score(frame: &Frame, object: usize) -> Score {
    let img = &frame.traced;
    let px = mask(img, object);
    let traced: Vec<[f32; 3]> = px.iter().map(|&i| img.direct[i]).collect();
    let forward: Vec<[f32; 3]> = px.iter().map(|&i| frame.forward[i]).collect();
    let (_, rel_rmse) = regia_render::compare::rmse(&traced, &forward);
    // FLIP over the object's pixels, everything else black on both sides.
    let n = (img.width * img.height) as usize;
    let mean = traced.iter().map(|c| lum(*c)).sum::<f64>() / traced.len().max(1) as f64;
    let scale = (0.18 / mean.max(1e-9)) as f32;
    let mut a = vec![[0.0f32; 3]; n];
    let mut b = vec![[0.0f32; 3]; n];
    for &i in &px {
        a[i] = img.direct[i];
        b[i] = frame.forward[i];
    }
    let flip = regia_render::compare::flip(
        &display(&a, scale),
        &display(&b, scale),
        img.width,
        img.height,
        67.0,
    );
    // The mean over the object's pixels, not over the black background.
    let flip_mean = px.iter().map(|&i| f64::from(flip.map[i])).sum::<f64>() / px.len().max(1) as f64;
    Score {
        pixels: px.len(),
        rel_rmse,
        flip: flip_mean as f32,
    }
}

fn graph_materials() -> Vec<LabMaterial> {
    let mut images = BTreeMap::new();
    images.insert("albedo".to_string(), checker());
    images.insert("normal".to_string(), bumps());
    vec![
        material(PROCEDURAL, &BTreeMap::new()),
        material(TEXTURED, &images),
        material(LAYERED, &BTreeMap::new()),
    ]
}

#[test]
fn three_graphs_trace_as_the_forward_pass_shades_them() {
    let device = RenderDevice::headless().expect("headless device");
    let tracer = PathTracer::new(&device).expect("compile");
    let materials = graph_materials();
    let frame = render(&device, &tracer, &materials, true);
    let stats = &frame.traced.metadata.surface_graphs;
    assert_eq!(stats.graphs, 3, "{stats:?}");
    assert_eq!(stats.materials, 3, "{stats:?}");
    assert!(stats.refusals.is_empty(), "{stats:?}");
    // § 2.2 "Direct lighting + materials": mean relative RMSE ≤ 1 %, FLIP ≤ 0.03.
    for (object, name) in ["procedural (clock)", "texture + normal map", "layered blend"]
        .into_iter()
        .enumerate()
    {
        let s = score(&frame, object);
        eprintln!(
            "{name}: {} px, relative RMSE {:.4}, FLIP {:.4}",
            s.pixels, s.rel_rmse, s.flip
        );
        assert!(s.pixels > 500, "{name}: {} compared pixels", s.pixels);
        assert!(s.rel_rmse <= 0.01, "{name}: relative RMSE {}", s.rel_rmse);
        assert!(s.flip <= 0.03, "{name}: FLIP {}", s.flip);
    }
    // The same graphs again: the composed kernels are cached, nothing recompiles.
    let again = render(&device, &tracer, &materials, true);
    assert_eq!(tracer.graph_kernel_compiles(), 1);
    assert_eq!(again.traced.direct, frame.traced.direct);
}

/// The material clock is the forward pass's: a trace that does not take the frame's clock shades
/// the time-reading graph at the material's own slot 0 and leaves § 2.2, the others are unchanged.
#[test]
fn the_material_clock_freezes_a_time_graph_on_both_sides() {
    let device = RenderDevice::headless().expect("headless device");
    let tracer = PathTracer::new(&device).expect("compile");
    let materials = graph_materials();
    let frozen = render(&device, &tracer, &materials, true);
    let unclocked = render(&device, &tracer, &materials, false);
    let with = score(&frozen, 0);
    let without = score(&unclocked, 0);
    eprintln!(
        "clock: with {:.4} / {:.4}, without {:.4} / {:.4}",
        with.rel_rmse, with.flip, without.rel_rmse, without.flip
    );
    assert!(with.rel_rmse <= 0.01);
    // Without the frame's clock the trace leaves the § 2.2 row on both measures.
    assert!(
        without.rel_rmse > 0.01 && without.flip > 0.03,
        "the clock did not reach the trace: {} / {}",
        without.rel_rmse,
        without.flip
    );
    // Graphs that read no time ignore the clock.
    let w = frozen.traced.width as usize;
    for (a, b) in frozen
        .traced
        .direct
        .iter()
        .zip(&unclocked.traced.direct)
        .enumerate()
        .filter(|(i, _)| i % w >= w / 3)
        .map(|(_, p)| p)
    {
        assert_eq!(a, b);
    }
}

/// What the tracer cannot evaluate it refuses by name, in the image's metadata: a graph using a
/// screen-space derivative traces with its factors, and the other graphs still compose.
#[test]
fn a_derivative_graph_is_refused_by_name_and_the_rest_compose() {
    let device = RenderDevice::headless().expect("headless device");
    let tracer = PathTracer::new(&device).expect("compile");
    let derivative = LabMaterial {
        surface_graph: Some(std::sync::Arc::new(SurfaceGraph::new(
            r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    let edge = fwidth(in.uv.x) * 40.0;
    return regia::pbr::surface::Surface(vec3<f32>(edge), 1.0, 0.0, 0.5, in.world_normal, 1.0, vec3<f32>(0.0), 0.0);
}
",
        ))),
        ..LabMaterial::EMPTY
    };
    let mut materials = graph_materials();
    materials[1] = derivative;
    let frame = render(&device, &tracer, &materials, true);
    let stats = &frame.traced.metadata.surface_graphs;
    assert_eq!(stats.graphs, 2, "{stats:?}");
    assert_eq!(stats.materials, 2, "{stats:?}");
    assert_eq!(stats.refusals.len(), 1, "{stats:?}");
    assert_eq!(stats.refusals[0].construct, "fwidth");
    assert_eq!(stats.refusals[0].material, 1);
    // The metadata JSON (`seeing.reference_trace`) carries it.
    let json = serde_json::to_value(&frame.traced.metadata).expect("json");
    assert_eq!(json["surface_graphs"]["refusals"][0]["construct"], "fwidth");
    // A scene without graphs writes no `surface_graphs` key: older sidecars are unchanged.
    let plain = render(&device, &tracer, &[LabMaterial::EMPTY], true);
    let json = serde_json::to_value(&plain.traced.metadata).expect("json");
    assert!(json.get("surface_graphs").is_none(), "{json}");
}

/// Composing graphs into the kernels leaves every standard material alone: the Cornell box traced
/// with the base kernels and with a kernel set that carries a graph (on a material no instance
/// uses) gives the same image.
#[test]
fn composing_graphs_leaves_standard_materials_alone() {
    let device = RenderDevice::headless().expect("headless device");
    let tracer = PathTracer::new(&device).expect("compile");
    let settings = PtSettings {
        spp: 4,
        max_bounces: 4,
        ..PtSettings::default()
    };
    let plain = regia_pt::scenes::cornell_box(48, 48, true);
    let base = tracer.render(&plain, &settings).expect("base kernels");
    assert!(base.metadata.surface_graphs.is_empty());
    let mut with_graph = plain.clone();
    let unused = with_graph.add_material(regia_pt::PtMaterial::default());
    let lab = material(PROCEDURAL, &BTreeMap::new());
    let g = lab.surface_graph.as_deref().expect("graph material");
    with_graph.surface_graphs.push(regia_pt::PtSurfaceGraph {
        material: unused,
        surface: regia_render::surface_graph::surface_source(&g.code())
            .expect("surface")
            .into(),
        params: g.params,
        textures: [None; regia_render::surface_graph::GRAPH_TEXTURE_SLOTS],
        vertex_offset: false,
        uses_uv1: false,
        uses_time: true,
    });
    let composed = tracer.render(&with_graph, &settings).expect("graph kernels");
    assert_eq!(composed.metadata.surface_graphs.graphs, 1);
    assert_eq!(tracer.graph_kernel_compiles(), 1);
    let peak = base
        .beauty
        .iter()
        .flat_map(|c| c.iter().copied())
        .fold(0.0f32, f32::max);
    let worst = base
        .beauty
        .iter()
        .zip(&composed.beauty)
        .flat_map(|(a, b)| (0..3).map(move |k| (a[k] - b[k]).abs()))
        .fold(0.0f32, f32::max);
    assert!(worst <= 1e-5 * peak.max(1e-6), "worst {worst} of peak {peak}");
}
