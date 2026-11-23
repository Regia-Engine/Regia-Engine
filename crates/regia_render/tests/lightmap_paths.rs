// SPDX-License-Identifier: MPL-2.0
//! Lightmap receivers on every draw path and every tier (ADR-2107): the GPU-driven batches, the
//! visibility buffer and the Mobile lite shader shade a receiver with its entry, as the CPU colour
//! loop does, and whatever is not a receiver stays as it was. No golden image: every check compares
//! pixels of frames rendered here (lavapipe, deterministic).

use std::sync::Arc;

use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
};
use regia_asset_pipeline::meshlet::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::forward_lite::ForwardLite;
use regia_render::lightmap::{LightmapForwardStats, LightmapSettings, LightmapTier, receiver_mesh};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, IndirectStrategy,
    LabMaterial, LabMesh, LabScene, LabVertex, MeshletBuilder, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset,
};

const W: u32 = 64;
const H: u32 = 64;
const ENTRY_A: u32 = 0;
const ENTRY_B: u32 = 1;
const KEY_A: u64 = 0x0B1E_C7A1;
const KEY_B: u64 = 0x0B1E_C7A2;
const KEY_PLAIN: u64 = 0x0B1E_C7A3;
const MESH: u64 = 0x0A11_0101 << 3;
/// Atlas 8 × 2: entry A over the right half, `A_LOW` then `A_HIGH`; entry B over the left half.
const A_LOW: f32 = 2_000.0;
const A_HIGH: f32 = 20_000.0;
const B_LIGHT: f32 = 8_000.0;

/// The binary's one device (ADR-7557, the ambient_frame pattern). Each test used to open and drop
/// its own, so a device was destroyed while other test threads named objects: the Vulkan loader
/// list race of ADR-2113 (the Arc rows of older pregate passes list this binary). Every test
/// still builds its own renderers on it; their stats are per renderer.
fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => {
            eprintln!("lightmap_paths adapter: {:?}", device.adapter.get_info());
            device
        }
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A second `device()` call opens no instance and no device (`loader_list_lock_count` counts both),
/// so the binary creates its one device once, whichever test asks first.
#[test]
fn every_test_shares_the_binary_device() {
    let first = device();
    let before = regia_render::loader_list_lock_count();
    let second = device();
    assert_eq!(
        regia_render::loader_list_lock_count(),
        before,
        "a second device() call created an instance or a device"
    );
    assert_eq!(first.device_id(), second.device_id());
}

fn quad(x: f32, y: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 0.0, 1.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([x - 0.3, y - 0.3, 0.0], n, [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.3, y - 0.3, 0.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.3, y + 0.3, 0.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - 0.3, y + 0.3, 0.0], n, [0.0, 0.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

/// A level lit by flat ambient (the lightmap replaces it), with the given material and post.
fn lab(material: LabMaterial, post: bool) -> LabScene {
    let (vertices, indices) = quad(0.0, 0.0);
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 2.0],
        target: [0.0; 3],
        fov_y: std::f32::consts::FRAC_PI_3,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        // A fixed exposure: frames of one renderer compare across lighting changes.
        post: post.then(|| PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![material],
        meshes: vec![LabMesh {
            vertices,
            indices,
            ..LabMesh::unskinned()
        }],
    }
}

fn lightmap() -> Lightmap {
    let irr: Vec<[f32; 3]> = (0..2)
        .flat_map(|_| 0..8u32)
        .map(|x| match x {
            0..=3 => [B_LIGHT; 3],
            4 | 5 => [A_LOW; 3],
            _ => [A_HIGH; 3],
        })
        .collect();
    let entry = |name: &str, rect: [u32; 4], scale_offset: [f32; 4]| LightmapEntry {
        name: name.into(),
        source_vertices: 4,
        rect,
        scale_offset,
        vertex_count: 0,
        index_count: 0,
        first_vertex: 0,
        first_index: 0,
        charts: 1,
        area_m2: 0.36,
        texels_per_metre: 1.0,
        key: None,
        seams: None,
        page: 0,
    };
    let unwrap = || MeshUnwrap {
        xref: vec![0, 1, 2, 3],
        uv: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
        indices: vec![0, 1, 2, 0, 2, 3],
    };
    Lightmap::from_irradiance(
        LightmapLayout {
            width: 8,
            height: 2,
            density: 1.0,
            padding: 1,
            entries: vec![
                entry("a", [4, 0, 4, 2], [0.5, 1.0, 0.5, 0.0]),
                entry("b", [0, 0, 4, 2], [0.5, 1.0, 0.0, 0.0]),
            ],
            unwraps: vec![unwrap(), unwrap()],
        },
        &irr,
        vec![Coverage::BAKED; 16],
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    )
    .expect("valid lightmap")
}

fn enabled() -> LightmapSettings {
    LightmapSettings {
        enabled: true,
        ..LightmapSettings::default()
    }
}

fn translate(x: f32, y: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, y, 0.0, 1.0],
    ]
}

/// The screen pixels of the instances at x = -0.65 (A, its low and high columns), 0.0 (B) and 0.65
/// (the plain one).
const A_LEFT: (u32, u32) = (10, 32);
const A_RIGHT: (u32, u32) = (18, 32);
const B_PX: (u32, u32) = (32, 32);
const PLAIN_PX: (u32, u32) = (50, 32);

fn px(img: &[u8], (x, y): (u32, u32)) -> u32 {
    let i = ((y * W + x) * 4) as usize;
    u32::from(img[i]) + u32::from(img[i + 1]) + u32::from(img[i + 2])
}

fn meshlet_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        let attributes = Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        });
        build_meshlet_mesh_with_attributes(&positions, attributes, indices, &MeshletSettings::default()).ok()
    })
}

/// How a frame draws.
#[derive(Clone, Copy)]
struct Path {
    gpu_driven: bool,
    visibility_buffer: bool,
    lite: Option<ForwardLite>,
    post: bool,
}

const CPU: Path = Path {
    gpu_driven: false,
    visibility_buffer: false,
    lite: None,
    post: false,
};
const GPU: Path = Path {
    gpu_driven: true,
    ..CPU
};
const CPU_POST: Path = Path { post: true, ..CPU };
const VIS: Path = Path {
    visibility_buffer: true,
    post: true,
    ..CPU
};

/// The three instances of one mesh and material on `path`: `receivers` of the installed `map`.
struct Frame {
    img: Vec<u8>,
    stats: LightmapForwardStats,
    instanced: usize,
    per_object: usize,
    vis_instances: u32,
}

fn draw(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    material: &LabMaterial,
    map: &Lightmap,
    path: Path,
) -> Frame {
    let lab = lab(material.clone(), path.post);
    let base = lab.as_frame_scene();
    let template = base.meshes[0];
    let (v, i) = quad(0.0, 0.0);
    let built = receiver_mesh(map, ENTRY_A as usize, &v, &i).expect("the baked mesh");
    let instance = |x: f32, object_key: u64| FrameMesh {
        mesh_key: MESH,
        object_key,
        model: translate(x, 0.0),
        vertices: &built.vertices,
        indices: &built.indices,
        uv1: &built.uv2,
        ..template
    };
    let scene = FrameScene {
        quality: FrameQuality {
            gpu_driven: path.gpu_driven,
            visibility_buffer: path.visibility_buffer,
            ..base.quality
        },
        meshes: vec![
            instance(-0.65, KEY_A),
            instance(0.0, KEY_B),
            instance(0.65, KEY_PLAIN),
        ],
        ..base.clone()
    };
    renderer.set_forward_lite(path.lite);
    let img = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    assert_eq!(img.len(), (W * H * 4) as usize);
    let draws = renderer.draw_stats();
    Frame {
        img,
        stats: renderer.lightmap_forward_stats(),
        instanced: draws.instanced,
        per_object: draws.per_object,
        vis_instances: renderer.visbuffer_stats().instances,
    }
}

fn renderer_with(map: &Arc<Lightmap>, settings: LightmapSettings, path: Path) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    if path.visibility_buffer {
        renderer.set_meshlet_builder(Some(meshlet_builder()));
    }
    renderer.set_lightmap(Some(map.clone()), settings);
    renderer.set_lightmap_receivers(&[(KEY_A, ENTRY_A), (KEY_B, ENTRY_B)]);
    renderer
}

fn frame(device: &RenderDevice, map: &Arc<Lightmap>, settings: LightmapSettings, path: Path) -> Frame {
    let mut renderer = renderer_with(map, settings, path);
    draw(&mut renderer, device, &LabMaterial::default(), map, path)
}

fn no_lightmap(device: &RenderDevice, map: &Arc<Lightmap>, path: Path) -> Vec<u8> {
    let mut renderer = FrameRenderer::new();
    if path.visibility_buffer {
        renderer.set_meshlet_builder(Some(meshlet_builder()));
    }
    draw(&mut renderer, device, &LabMaterial::default(), map, path).img
}

/// The largest channel difference over the whole frame.
fn max_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

fn takes_gpu_driven(device: &RenderDevice) -> bool {
    let takes = IndirectStrategy::select(&device.capabilities).uses_gpu_objects();
    if !takes {
        eprintln!("skip: this device has no GPU-driven instanced path");
    }
    takes
}

/// Each receiver in a GPU-driven frame reads its own entry from the per-slot table inside one batch
/// of one mesh and material, a non-receiver in that batch keeps its probe light, and the pixels are
/// the CPU colour loop's.
#[test]
fn receivers_in_a_shared_batch_read_their_own_entries_and_match_the_cpu_loop() {
    let device = device();
    if !takes_gpu_driven(&device) {
        return;
    }
    let map = Arc::new(lightmap());
    let cpu = frame(&device, &map, enabled(), CPU);
    assert_eq!(
        (cpu.stats.drawn, cpu.stats.instanced, cpu.stats.fallback),
        (2, 0, 0),
        "{:?}",
        cpu.stats
    );
    let gpu = frame(&device, &map, enabled(), GPU);
    assert_eq!(
        (
            gpu.stats.drawn,
            gpu.stats.instanced,
            gpu.stats.fallback,
            gpu.stats.unsupported
        ),
        (2, 2, 0, 0),
        "{:?}",
        gpu.stats
    );
    assert_eq!(
        (gpu.instanced, gpu.per_object),
        (3, 0),
        "all three instances draw from the batches, none from the CPU loop"
    );
    assert!(
        max_diff(&gpu.img, &cpu.img) <= 1,
        "the instanced path differs from the CPU loop by {}",
        max_diff(&gpu.img, &cpu.img)
    );
    // The entries really differ on screen.
    assert!(
        px(&gpu.img, A_RIGHT) > px(&gpu.img, A_LEFT) + 30,
        "entry A's columns"
    );
    let (low, b, high) = (px(&gpu.img, A_LEFT), px(&gpu.img, B_PX), px(&gpu.img, A_RIGHT));
    assert!(
        low < b && b < high,
        "entry B sits between A's columns: {low} {b} {high}"
    );
    // The non-receiver keeps exactly its light.
    let plain = no_lightmap(&device, &map, GPU);
    assert_eq!(
        px(&gpu.img, PLAIN_PX),
        px(&plain, PLAIN_PX),
        "a non-receiver sharing the batch is unchanged"
    );
}

/// A GPU-driven frame with a lightmap installed but no receiver, or a disabled lightmap, is the
/// frame without any lightmap, byte for byte.
#[test]
fn a_gpu_driven_frame_without_receivers_is_byte_identical() {
    let device = device();
    if !takes_gpu_driven(&device) {
        return;
    }
    let map = Arc::new(lightmap());
    let plain = no_lightmap(&device, &map, GPU);
    let disabled = frame(&device, &map, LightmapSettings::default(), GPU);
    assert!(disabled.img == plain, "a disabled lightmap changes the frame");
    let mut renderer = FrameRenderer::new();
    renderer.set_lightmap(Some(map.clone()), enabled());
    let unused = draw(&mut renderer, &device, &LabMaterial::default(), &map, GPU);
    assert!(
        unused.img == plain,
        "a lightmap without receivers changes the frame"
    );
    assert_eq!(unused.stats.drawn, 0);
}

/// Masked and double-sided receivers take their instanced variants: the alpha test still cuts, the
/// back face is drawn.
#[test]
fn masked_and_double_sided_receivers_take_their_instanced_variants() {
    let device = device();
    if !takes_gpu_driven(&device) {
        return;
    }
    let map = Arc::new(lightmap());
    for material in [
        LabMaterial {
            alpha_mode: regia_render::ALPHA_MASK,
            alpha_cutoff: 0.5,
            ..LabMaterial::default()
        },
        LabMaterial {
            double_sided: true,
            ..LabMaterial::default()
        },
    ] {
        let mut cpu = renderer_with(&map, enabled(), CPU);
        let cpu = draw(&mut cpu, &device, &material, &map, CPU);
        let mut gpu = renderer_with(&map, enabled(), GPU);
        let gpu = draw(&mut gpu, &device, &material, &map, GPU);
        assert_eq!(gpu.stats.instanced, 2, "{:?}", gpu.stats);
        assert!(
            max_diff(&gpu.img, &cpu.img) <= 1,
            "{} against the CPU loop",
            max_diff(&gpu.img, &cpu.img)
        );
    }
}

/// A receiver in the visibility buffer shades with its entry through the resolve, its pixels the
/// forward path's, and a cluster instance that is not a receiver keeps its light.
#[test]
fn visibility_receivers_read_their_entries_and_match_the_forward_path() {
    let device = device();
    let map = Arc::new(lightmap());
    let forward = frame(&device, &map, enabled(), CPU_POST);
    let vis = frame(&device, &map, enabled(), VIS);
    assert_eq!(vis.vis_instances, 3, "all three are cluster instances");
    assert_eq!(
        (vis.stats.drawn, vis.stats.visbuffer, vis.stats.fallback),
        (2, 2, 0),
        "{:?}",
        vis.stats
    );
    for point in [A_LEFT, A_RIGHT, B_PX] {
        assert!(
            px(&vis.img, point).abs_diff(px(&forward.img, point)) <= 6,
            "at {point:?}: {} vs {}",
            px(&vis.img, point),
            px(&forward.img, point)
        );
    }
    assert!(
        px(&vis.img, A_RIGHT) > px(&vis.img, A_LEFT) + 30,
        "entry A's columns"
    );
    let plain = no_lightmap(&device, &map, VIS);
    assert_eq!(
        px(&vis.img, PLAIN_PX),
        px(&plain, PLAIN_PX),
        "a cluster instance that is not a receiver is unchanged"
    );
}

/// A mesh that went resident before it became a receiver gets its UV2 without a rebuild, and
/// removing the receivers gives the plain resolve back, byte for byte.
#[test]
fn a_resident_mesh_becomes_a_visibility_receiver_and_back() {
    let device = device();
    let map = Arc::new(lightmap());
    let material = LabMaterial::default();
    let mut renderer = FrameRenderer::new();
    renderer.set_meshlet_builder(Some(meshlet_builder()));
    let before = draw(&mut renderer, &device, &material, &map, VIS);
    assert_eq!(before.vis_instances, 3);
    assert_eq!(before.stats.visbuffer, 0);
    renderer.set_lightmap(Some(map.clone()), enabled());
    renderer.set_lightmap_receivers(&[(KEY_A, ENTRY_A), (KEY_B, ENTRY_B)]);
    let lit = draw(&mut renderer, &device, &material, &map, VIS);
    assert_eq!(lit.stats.visbuffer, 2, "{:?}", lit.stats);
    assert!(px(&lit.img, A_RIGHT) > px(&lit.img, A_LEFT) + 30);
    renderer.set_lightmap_receivers(&[]);
    let after = draw(&mut renderer, &device, &material, &map, VIS);
    assert_eq!(after.stats.visbuffer, 0);
    assert_eq!(
        px(&after.img, B_PX),
        px(&before.img, B_PX),
        "the plain resolve again"
    );
}

/// A visibility receiver whose mesh has no UV2 stream, and any receiver of a mesh that does not take
/// meshlets, stays on the forward path's rules (counted as unsupported, no wrong pixels).
#[test]
fn a_visibility_frame_without_a_builder_keeps_receivers_on_the_forward_paths() {
    let device = device();
    let map = Arc::new(lightmap());
    let mut renderer = renderer_with(&map, enabled(), CPU);
    let frame = draw(&mut renderer, &device, &LabMaterial::default(), &map, VIS);
    assert_eq!(frame.vis_instances, 0);
    assert_eq!(frame.stats.drawn, 2, "{:?}", frame.stats);
    assert_eq!(frame.stats.visbuffer, 0);
}

/// Both lite tiers shade receivers with the lite shader: the entry's light reaches the frame, and
/// what is not a receiver is the lite frame without a lightmap.
#[test]
fn the_lite_tiers_shade_receivers_and_leave_the_rest() {
    let device = device();
    let map = Arc::new(lightmap());
    for tier in [ForwardLite::Low, ForwardLite::High] {
        let path = Path {
            lite: Some(tier),
            ..CPU
        };
        let lit = frame(&device, &map, enabled(), path);
        assert_eq!(
            (lit.stats.drawn, lit.stats.lite, lit.stats.fallback),
            (2, 2, 0),
            "{tier:?}: {:?}",
            lit.stats
        );
        assert!(
            px(&lit.img, A_RIGHT) > px(&lit.img, A_LEFT) + 30,
            "{tier:?}: entry A's columns"
        );
        let (low, b, high) = (px(&lit.img, A_LEFT), px(&lit.img, B_PX), px(&lit.img, A_RIGHT));
        assert!(low < b && b < high, "{tier:?}: {low} {b} {high}");
        let plain = no_lightmap(&device, &map, path);
        assert_eq!(
            px(&lit.img, PLAIN_PX),
            px(&plain, PLAIN_PX),
            "{tier:?}: the plain instance"
        );
        let disabled = frame(&device, &map, LightmapSettings::default(), path);
        assert!(
            disabled.img == plain,
            "{tier:?}: a disabled lightmap changes the lite frame"
        );
        // The lite shader agrees with the full one on the baked light, with its bilinear lookup
        // (the full shader asked for bilinear as well, and the flat quad has no normal map).
        let full = frame(
            &device,
            &map,
            LightmapSettings {
                bicubic: false,
                ..enabled()
            },
            CPU,
        );
        for point in [A_LEFT, A_RIGHT, B_PX] {
            let (a, b) = (px(&lit.img, point), px(&full.img, point));
            assert!(
                a.abs_diff(b) <= b / 8 + 6,
                "{tier:?} at {point:?}: lite {a}, full {b}"
            );
        }
    }
}

/// The double sun of ADR-2024 on the lite path: a `sun_direct` lightmap's receiver drops the live
/// sun's diffuse term; every other draw keeps the sun whole.
#[test]
fn a_lite_receiver_of_a_direct_sun_lightmap_drops_the_live_sun_diffuse() {
    let device = device();
    let frame_with = |contents: u32, sun: f32| {
        let mut map = lightmap();
        map.bake.contents = contents;
        let map = Arc::new(map);
        let mut renderer = renderer_with(&map, enabled(), CPU);
        renderer.set_forward_lite(Some(ForwardLite::Low));
        let mut lab = lab(
            LabMaterial {
                roughness: 1.0,
                ..LabMaterial::default()
            },
            false,
        );
        lab.directional = DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0; 3], sun);
        let base = lab.as_frame_scene();
        let template = base.meshes[0];
        let (v, i) = quad(0.0, 0.0);
        let built = receiver_mesh(&map, 0, &v, &i).expect("the baked mesh");
        let instance = |x: f32, object_key: u64| FrameMesh {
            mesh_key: MESH,
            object_key,
            model: translate(x, 0.0),
            vertices: &built.vertices,
            indices: &built.indices,
            uv1: &built.uv2,
            ..template
        };
        let scene = FrameScene {
            meshes: vec![
                instance(-0.65, KEY_A),
                instance(0.0, KEY_B),
                instance(0.65, KEY_PLAIN),
            ],
            ..base.clone()
        };
        renderer.render_offscreen(&device, &scene).expect("frame").pixels
    };
    const SUN: f32 = 12_000.0;
    let indirect_only = frame_with(LightmapContents::SKY, SUN);
    let holds_sun = frame_with(LightmapContents::SKY | LightmapContents::SUN_DIRECT, SUN);
    let dark_sun = frame_with(LightmapContents::SKY | LightmapContents::SUN_DIRECT, 0.0);
    assert!(
        px(&indirect_only, A_LEFT) > px(&holds_sun, A_LEFT) + 20,
        "the live sun's diffuse is gone: {} vs {}",
        px(&indirect_only, A_LEFT),
        px(&holds_sun, A_LEFT)
    );
    assert!(
        px(&holds_sun, A_LEFT).abs_diff(px(&dark_sun, A_LEFT)) <= 24,
        "what is left of the sun is its specular: {} vs {}",
        px(&holds_sun, A_LEFT),
        px(&dark_sun, A_LEFT)
    );
    assert_eq!(
        px(&indirect_only, PLAIN_PX),
        px(&holds_sun, PLAIN_PX),
        "a non-receiver keeps the live sun whole"
    );
}

/// Channel-sum bound between a visibility-resolve receiver and the forward path's sun-off
/// reading of the same quad. The two paths already agree within 6 on a lightmapped quad
/// (`visibility_receivers_read_their_entries_and_match_the_forward_path`). A second diffuse sun
/// on this quad is tens of counts, so 8 rejects it and still fits the resolve's reconstruction.
const VIS_SUN_OFF_EPSILON: u32 = 8;

/// What is left of a rough dielectric's live sun once its diffuse term is gone: the specular
/// lobe. The lite-path check of the same policy uses this bound.
const VIS_SUN_SPECULAR_EPSILON: u32 = 24;

/// The double sun of ADR-2024 on the visibility resolve. The `LIGHTMAP` shade is the forward
/// fragment body (`shade_surface`), which already drops the live sun's diffuse through
/// `lightmap_holds_sun_diffuse` when the lightmap's contents include `sun_direct`. This test
/// holds that: on a lit quad, the visibility-path sun diffuse is gone where the lightmap holds
/// the sun, the pixel matches the forward path's sun-off reading within
/// [`VIS_SUN_OFF_EPSILON`], and a cluster instance that is not a receiver keeps the live sun.
#[test]
fn a_visibility_receiver_of_a_direct_sun_lightmap_drops_the_live_sun_diffuse() {
    /// One frame of the three quads. `path` is the visibility resolve or the forward colour loop.
    struct Shot {
        img: Vec<u8>,
        vis_instances: u32,
        visbuffer: usize,
    }
    let device = device();
    let shot = |path: Path, contents: u32, sun: f32| {
        let mut map = lightmap();
        map.bake.contents = contents;
        let map = Arc::new(map);
        let mut renderer = renderer_with(&map, enabled(), path);
        let mut lab = lab(
            LabMaterial {
                roughness: 1.0,
                ..LabMaterial::default()
            },
            path.post,
        );
        // Toward the quads' front: the sun is the bulk of the light on a rough dielectric.
        lab.directional = DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0; 3], sun);
        let base = lab.as_frame_scene();
        let template = base.meshes[0];
        let (v, i) = quad(0.0, 0.0);
        let built = receiver_mesh(&map, ENTRY_A as usize, &v, &i).expect("the baked mesh");
        let instance = |x: f32, object_key: u64| FrameMesh {
            mesh_key: MESH,
            object_key,
            model: translate(x, 0.0),
            vertices: &built.vertices,
            indices: &built.indices,
            uv1: &built.uv2,
            ..template
        };
        let scene = FrameScene {
            quality: FrameQuality {
                gpu_driven: path.gpu_driven,
                visibility_buffer: path.visibility_buffer,
                ..base.quality
            },
            meshes: vec![
                instance(-0.65, KEY_A),
                instance(0.0, KEY_B),
                instance(0.65, KEY_PLAIN),
            ],
            ..base.clone()
        };
        let img = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
        let stats = renderer.lightmap_forward_stats();
        Shot {
            img,
            vis_instances: renderer.visbuffer_stats().instances,
            visbuffer: stats.visbuffer,
        }
    };
    const SUN: f32 = 12_000.0;
    const WITH_SUN: u32 = LightmapContents::SKY | LightmapContents::SUN_DIRECT;
    let vis_holds = shot(VIS, WITH_SUN, SUN);
    let vis_live = shot(VIS, LightmapContents::SKY, SUN);
    let vis_dark = shot(VIS, WITH_SUN, 0.0);
    let forward_off = shot(CPU_POST, WITH_SUN, SUN);
    assert_eq!(
        vis_holds.vis_instances, 3,
        "the three quads are cluster instances"
    );
    assert_eq!(
        vis_holds.visbuffer, 2,
        "both receivers shade in the visibility resolve, not the forward fallback: {:?}",
        vis_holds.visbuffer
    );
    assert_eq!(
        forward_off.visbuffer, 0,
        "the sun-off reading is the forward path"
    );

    let gated = px(&vis_holds.img, A_LEFT);
    let stacked = px(&vis_live.img, A_LEFT);
    let dark = px(&vis_dark.img, A_LEFT);
    let forward = px(&forward_off.img, A_LEFT);
    assert!(
        stacked > gated + 20,
        "the live sun's diffuse is not stacked on the baked sun: stacked {stacked}, gated {gated}"
    );
    assert!(
        gated.abs_diff(dark) <= VIS_SUN_SPECULAR_EPSILON,
        "visibility-path sun diffuse is 0; what remains is specular: gated {gated}, sun off {dark}"
    );
    assert!(
        gated.abs_diff(forward) <= VIS_SUN_OFF_EPSILON,
        "visibility sun-off {gated} vs forward sun-off {forward} (epsilon {VIS_SUN_OFF_EPSILON})"
    );
    // A cluster instance that is not a receiver keeps the live sun, on the same frame.
    assert_eq!(
        px(&vis_holds.img, PLAIN_PX),
        px(&vis_live.img, PLAIN_PX),
        "a non-receiver keeps the live sun whole"
    );
    assert!(
        px(&vis_holds.img, PLAIN_PX) > px(&vis_dark.img, PLAIN_PX) + 20,
        "the non-receiver's sun is visible"
    );
}

/// Directions at the lite tiers: High bends the baked light with them (one more fetch), Low has no
/// direction lookup.
#[test]
fn the_direction_reaches_lite_high_and_not_lite_low() {
    let device = device();
    let mut map = lightmap();
    map.set_directions(&[[1.0, 0.0, 0.0, 1.0]; 16])
        .expect("one per texel");
    let map = Arc::new(map);
    let flat = LightmapSettings {
        directional: false,
        ..enabled()
    };
    let lite = |tier| Path {
        lite: Some(tier),
        ..CPU
    };
    let high = frame(&device, &map, enabled(), lite(ForwardLite::High));
    let high_flat = frame(&device, &map, flat, lite(ForwardLite::High));
    assert!(
        px(&high.img, A_RIGHT) + 30 < px(&high_flat.img, A_RIGHT),
        "a sideways direction darkens: {} vs {}",
        px(&high.img, A_RIGHT),
        px(&high_flat.img, A_RIGHT)
    );
    let low = frame(&device, &map, enabled(), lite(ForwardLite::Low));
    let low_flat = frame(&device, &map, flat, lite(ForwardLite::Low));
    assert!(low.img == low_flat.img, "Low reads no direction");
    assert_eq!(low.stats.tier, LightmapTier::Bilinear.name());
    assert_eq!(high.stats.tier, LightmapTier::Directional.name());
}

/// Every profile row draws a receiver with the lookup its row allows: the static path exists at
/// every tier, on this adapter with the full shader, and forced to the lite shader for the rows
/// that ask for it on a tile GPU.
#[test]
fn every_profile_row_draws_receivers() {
    let device = device();
    let map = Arc::new(lightmap());
    for preset in RenderProfilePreset::ALL {
        let profile = RenderProfile::from_preset(preset);
        let quality = FrameQuality::from_profile(&profile);
        let tier = profile.forward_lite;
        let lab = lab(LabMaterial::default(), false);
        let base = lab.as_frame_scene();
        let template = base.meshes[0];
        let (v, i) = quad(0.0, 0.0);
        let built = receiver_mesh(&map, 0, &v, &i).expect("the baked mesh");
        let instance = |x: f32, object_key: u64| FrameMesh {
            mesh_key: MESH,
            object_key,
            model: translate(x, 0.0),
            vertices: &built.vertices,
            indices: &built.indices,
            uv1: &built.uv2,
            ..template
        };
        let scene = FrameScene {
            quality,
            meshes: vec![
                instance(-0.65, KEY_A),
                instance(0.0, KEY_B),
                instance(0.65, KEY_PLAIN),
            ],
            ..base.clone()
        };
        let mut renderer = renderer_with(&map, enabled(), CPU);
        renderer.set_forward_lite(Some(tier));
        renderer.render_offscreen(&device, &scene).expect("frame");
        let stats = renderer.lightmap_forward_stats();
        assert_eq!(
            (stats.drawn, stats.fallback, stats.unsupported),
            (2, 0, 0),
            "{preset:?}: {stats:?}"
        );
        assert_eq!(
            stats.tier,
            profile.lightmap_tier.for_frame(tier).name(),
            "{preset:?}"
        );
        if tier == ForwardLite::Off {
            assert_eq!(stats.lite, 0, "{preset:?}");
        } else {
            assert_eq!(stats.lite, 2, "{preset:?}");
        }
    }
}

/// The profile's tier caps the settings: a bicubic lightmap draws its bilinear frame at a row that
/// allows only that, without any setting changing, on the full shader too.
#[test]
fn the_profile_tier_caps_the_lookup_on_the_full_shader() {
    let device = device();
    let map = Arc::new(lightmap());
    let render = |settings: LightmapSettings, tier: LightmapTier| {
        let lab = lab(LabMaterial::default(), false);
        let base = lab.as_frame_scene();
        let template = base.meshes[0];
        let (v, i) = quad(0.0, 0.0);
        let built = receiver_mesh(&map, 0, &v, &i).expect("the baked mesh");
        let scene = FrameScene {
            quality: FrameQuality {
                lightmap_tier: tier,
                ..base.quality
            },
            meshes: vec![FrameMesh {
                mesh_key: MESH,
                object_key: KEY_A,
                model: translate(-0.65, 0.0),
                vertices: &built.vertices,
                indices: &built.indices,
                uv1: &built.uv2,
                ..template
            }],
            ..base.clone()
        };
        let mut renderer = renderer_with(&map, settings, CPU);
        renderer.render_offscreen(&device, &scene).expect("frame").pixels
    };
    let bilinear = LightmapSettings {
        bicubic: false,
        ..enabled()
    };
    let capped = render(enabled(), LightmapTier::Bilinear);
    let asked = render(bilinear, LightmapTier::Full);
    assert!(capped == asked, "the cap draws the bilinear frame");
    let full = render(enabled(), LightmapTier::Full);
    assert!(
        full != capped,
        "the bicubic frame differs (the test sees the filter)"
    );
}

/// Two charts of one flat surface, side by side in the mesh and apart in the atlas with black texels
/// between them: a triangle across the seam would read black. A flat surface simplifies as far as it
/// can, so the visibility frame draws a coarse cluster level; it must stay inside its charts.
#[test]
fn a_coarse_cluster_level_stays_inside_its_charts() {
    const N: u32 = 32;
    const COLUMNS: u32 = 2 * N + 1;
    let device = device();
    let n = [0.0, 0.0, 1.0];
    let mut source = Vec::new();
    for j in 0..=N {
        for i in 0..COLUMNS {
            let (u, v) = (i as f32 / (2 * N) as f32, j as f32 / N as f32);
            source.push(LabVertex::from_pos_normal_uv(
                [u * 2.0 - 1.0, v - 0.5, 0.0],
                n,
                [u, v],
            ));
        }
    }
    let mut source_indices = Vec::new();
    let quad_at = |indices: &mut Vec<u32>, at: &dyn Fn(u32, u32) -> u32, i: u32, j: u32| {
        indices.extend([
            at(i, j),
            at(i + 1, j),
            at(i + 1, j + 1),
            at(i, j),
            at(i + 1, j + 1),
            at(i, j + 1),
        ]);
    };
    let source_at = |i: u32, j: u32| j * COLUMNS + i;
    // The unwrap keeps the source's triangle order: chart A's triangles, then chart B's.
    for chart in 0..2u32 {
        for j in 0..N {
            for i in 0..N {
                quad_at(&mut source_indices, &source_at, i + chart * N, j);
            }
        }
    }
    // The unwrap: chart A over columns 0..=N, chart B over N..=2N (the seam column split in two).
    let (mut xref, mut uv) = (Vec::new(), Vec::new());
    for chart in 0..2u32 {
        for j in 0..=N {
            for i in 0..=N {
                xref.push(j * COLUMNS + i + chart * N);
                let left = if chart == 0 { 0.0625 } else { 0.5625 };
                uv.push([
                    left + i as f32 / N as f32 * 0.375,
                    0.125 + j as f32 / N as f32 * 0.75,
                ]);
            }
        }
    }
    let row = N + 1;
    let mut indices = Vec::new();
    for chart in 0..2u32 {
        let at = |i: u32, j: u32| chart * row * row + j * row + i;
        for j in 0..N {
            for i in 0..N {
                quad_at(&mut indices, &at, i, j);
            }
        }
    }
    let (width, height) = (32u32, 8u32);
    let irradiance: Vec<[f32; 3]> = (0..width * height)
        .map(|t| {
            if matches!(t % width, 15 | 16) {
                [0.0; 3]
            } else {
                [A_HIGH; 3]
            }
        })
        .collect();
    let map = Arc::new(
        Lightmap::from_irradiance(
            LightmapLayout {
                width,
                height,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "charts".into(),
                    source_vertices: source.len() as u32,
                    rect: [0, 0, width, height],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 2,
                    area_m2: 2.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap { xref, uv, indices }],
            },
            &irradiance,
            vec![Coverage::BAKED; (width * height) as usize],
            LightmapBakeInfo {
                contents: LightmapContents::SKY,
                ..LightmapBakeInfo::default()
            },
        )
        .expect("valid lightmap"),
    );
    let built = receiver_mesh(&map, 0, &source, &source_indices).expect("the baked mesh");
    let lab = lab(LabMaterial::default(), true);
    let base = lab.as_frame_scene();
    let template = base.meshes[0];
    let settings = LightmapSettings {
        bicubic: false,
        ..enabled()
    };
    let render = |visibility_buffer: bool| {
        let mut renderer = FrameRenderer::new();
        if visibility_buffer {
            renderer.set_meshlet_builder(Some(meshlet_builder()));
        }
        renderer.set_lightmap(Some(map.clone()), settings);
        renderer.set_lightmap_receivers(&[(KEY_A, 0)]);
        let scene = FrameScene {
            quality: FrameQuality {
                visibility_buffer,
                ..base.quality
            },
            meshes: vec![FrameMesh {
                mesh_key: MESH + 8,
                object_key: KEY_A,
                vertices: &built.vertices,
                indices: &built.indices,
                uv1: &built.uv2,
                ..template
            }],
            ..base.clone()
        };
        // The second frame's counters say what the first drew.
        let mut img = Vec::new();
        for _ in 0..3 {
            img = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
        }
        (img, renderer, built.indices.len() as u32 / 3)
    };
    let (forward, _, _) = render(false);
    let (vis, renderer, triangles) = render(true);
    let stats = renderer.lightmap_forward_stats();
    assert_eq!(stats.visbuffer, 1, "{stats:?}");
    let counters = renderer
        .visbuffer_counters(&device)
        .expect("counters")
        .expect("a visibility frame");
    let drawn = counters.phase1_clusters + counters.phase2_clusters + counters.software_clusters;
    let leaves = triangles.div_ceil(regia_render::visbuffer::MAX_MESHLET_TRIANGLES as u32);
    eprintln!("{drawn} clusters drawn, at least {leaves} at the finest level: {counters:?}");
    assert!(
        drawn < leaves,
        "the frame must draw a coarse level to prove anything: {counters:?}"
    );
    assert!(
        max_diff(&vis, &forward) <= 3,
        "the coarse level differs from the full mesh by {}: a triangle crossed a chart seam",
        max_diff(&vis, &forward)
    );
}

/// Installing another lightmap on a running renderer rebuilds the tables' bind groups: the next
/// GPU-driven and visibility frames read the new atlas, not the old one.
#[test]
fn another_lightmap_reaches_the_table_paths() {
    let device = device();
    let map = Arc::new(lightmap());
    let brighter = Arc::new({
        let mut m = lightmap();
        for texel in &mut m.texels {
            // Rgb9e5: raise the exponent field by one, which doubles every texel.
            *texel += 1 << 27;
        }
        m
    });
    for path in [GPU, VIS] {
        if path.gpu_driven && !takes_gpu_driven(&device) {
            continue;
        }
        let mut renderer = renderer_with(&map, enabled(), path);
        let material = LabMaterial::default();
        let first = draw(&mut renderer, &device, &material, &map, path);
        renderer.set_lightmap(Some(brighter.clone()), enabled());
        let second = draw(&mut renderer, &device, &material, &brighter, path);
        assert!(
            px(&second.img, B_PX) > px(&first.img, B_PX) + 10,
            "the new atlas: {} vs {}",
            px(&second.img, B_PX),
            px(&first.img, B_PX)
        );
        let fresh = frame(&device, &brighter, enabled(), path);
        assert_eq!(
            px(&second.img, B_PX),
            px(&fresh.img, B_PX),
            "the same frame as a new renderer's"
        );
    }
}

/// Switching a running renderer from the Low lite tier back to the full shader brings back what
/// Low left off the GPU: the frame is a new renderer's, directions included.
#[test]
fn leaving_the_low_tier_brings_the_directions_back() {
    let device = device();
    let mut map = lightmap();
    map.set_directions(&[[1.0, 0.0, 0.0, 1.0]; 16])
        .expect("one per texel");
    let map = Arc::new(map);
    let low = Path {
        lite: Some(ForwardLite::Low),
        ..CPU
    };
    let material = LabMaterial::default();
    let mut renderer = renderer_with(&map, enabled(), low);
    let at_low = draw(&mut renderer, &device, &material, &map, low);
    assert_eq!(at_low.stats.tier, "bilinear");
    let back = draw(&mut renderer, &device, &material, &map, CPU);
    assert_eq!(back.stats.tier, "full");
    let fresh = frame(&device, &map, enabled(), CPU);
    assert!(
        back.img == fresh.img,
        "the full shader's frame, with its directions"
    );
}

/// The stats name what the forward pass's copy holds on the GPU (ADR-3145): nothing before a
/// receiver draws, the RGB9E5 atlas alone at the Low lite tier (no direction section), the atlas
/// and the directions at the full shader, and nothing again after a clear.
#[test]
fn the_stats_report_the_bytes_each_tier_uploaded() {
    let device = device();
    let mut map = lightmap();
    map.set_directions(&[[1.0, 0.0, 0.0, 1.0]; 16])
        .expect("one per texel");
    let texels = u64::from(map.width) * u64::from(map.height);
    let map = Arc::new(map);
    let low = Path {
        lite: Some(ForwardLite::Low),
        ..CPU
    };
    let material = LabMaterial::default();
    let mut renderer = renderer_with(&map, enabled(), low);
    let before = renderer.lightmap_forward_stats();
    assert_eq!(
        (
            before.uploaded_bytes,
            before.native_blocks,
            before.uploaded_directions
        ),
        (0, false, false),
        "nothing is uploaded before a receiver draws: {before:?}"
    );
    let at_low = draw(&mut renderer, &device, &material, &map, low).stats;
    assert_eq!(at_low.tier, "bilinear");
    assert_eq!(
        (
            at_low.uploaded_bytes,
            at_low.native_blocks,
            at_low.uploaded_directions
        ),
        (texels * 4, false, false),
        "Low uploads the RGB9E5 atlas only: {at_low:?}"
    );
    let full = draw(&mut renderer, &device, &material, &map, CPU).stats;
    assert_eq!(full.tier, "full");
    assert_eq!(
        (full.uploaded_bytes, full.native_blocks, full.uploaded_directions),
        (texels * 8, false, true),
        "the full shader adds the direction texture: {full:?}"
    );
    renderer.set_lightmap(None, enabled());
    let cleared = renderer.lightmap_forward_stats();
    assert_eq!(
        (cleared.uploaded_bytes, cleared.uploaded_directions),
        (0, false),
        "a clear frees the copy: {cleared:?}"
    );
}

/// More receivers than the table's first size grow it, and the frame stays the CPU loop's.
#[test]
fn many_instanced_receivers_grow_the_table() {
    const COUNT: usize = 40;
    let device = device();
    if !takes_gpu_driven(&device) {
        return;
    }
    let map = Arc::new(lightmap());
    let (v, i) = quad(0.0, 0.0);
    let built = receiver_mesh(&map, 0, &v, &i).expect("the baked mesh");
    let lab = lab(LabMaterial::default(), false);
    let base = lab.as_frame_scene();
    let template = base.meshes[0];
    let render = |gpu_driven: bool| {
        let receivers: Vec<(u64, u32)> = (0..COUNT)
            .map(|n| (0x0B1F_0000 + n as u64, (n % 2) as u32))
            .collect();
        let meshes: Vec<FrameMesh<'_>> = (0..COUNT)
            .map(|n| FrameMesh {
                mesh_key: MESH,
                object_key: 0x0B1F_0000 + n as u64,
                model: {
                    let mut model = translate((n % 8) as f32 * 0.16 - 0.56, (n / 8) as f32 * 0.16 - 0.32);
                    for (a, axis) in model.iter_mut().enumerate().take(3) {
                        axis[a] = 0.2;
                    }
                    model
                },
                vertices: &built.vertices,
                indices: &built.indices,
                uv1: &built.uv2,
                ..template
            })
            .collect();
        let scene = FrameScene {
            quality: FrameQuality {
                gpu_driven,
                ..base.quality
            },
            meshes,
            ..base.clone()
        };
        let mut renderer = FrameRenderer::new();
        renderer.set_lightmap(Some(map.clone()), enabled());
        renderer.set_lightmap_receivers(&receivers);
        let img = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
        (img, renderer.lightmap_forward_stats())
    };
    let (cpu, _) = render(false);
    let (gpu, stats) = render(true);
    assert_eq!(stats.instanced, COUNT, "{stats:?}");
    assert!(
        max_diff(&gpu, &cpu) <= 1,
        "{} against the CPU loop",
        max_diff(&gpu, &cpu)
    );
}

/// Meshlets cooked from the source mesh do not fit a receiver whose unwrap split its vertices (a
/// game's cooked `.fmlt`, under the swapped mesh's key): the mesh is declined and the receiver draws
/// from the batches, with the right pixels, instead of through the visibility buffer.
#[test]
fn source_meshlets_leave_a_split_receiver_on_the_forward_paths() {
    let device = device();
    let map = Arc::new(lightmap());
    let (v, i) = quad(0.0, 0.0);
    let built = receiver_mesh(&map, 0, &v, &i).expect("the baked mesh");
    // The source had one vertex fewer than the receiver: as if a seam had split that one.
    let mut split = built.vertices.clone();
    split.push(built.vertices[0]);
    let mut uv2 = built.uv2.clone();
    uv2.push(built.uv2[0]);
    let positions: Vec<[f32; 3]> = built.vertices.iter().map(|v| v.position).collect();
    let cooked =
        build_meshlet_mesh_with_attributes(&positions, None, &built.indices, &MeshletSettings::default())
            .expect("meshlets of the source");
    let lab = lab(LabMaterial::default(), true);
    let base = lab.as_frame_scene();
    let template = base.meshes[0];
    let scene = FrameScene {
        quality: FrameQuality {
            visibility_buffer: true,
            ..base.quality
        },
        meshes: vec![FrameMesh {
            mesh_key: MESH,
            object_key: KEY_A,
            model: translate(-0.65, 0.0),
            vertices: &split,
            indices: &built.indices,
            uv1: &uv2,
            ..template
        }],
        ..base.clone()
    };
    let mut renderer = FrameRenderer::new();
    renderer.insert_meshlets(MESH, Arc::new(cooked));
    renderer.set_lightmap(Some(map.clone()), enabled());
    renderer.set_lightmap_receivers(&[(KEY_A, ENTRY_A)]);
    let img = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
    let stats = renderer.lightmap_forward_stats();
    assert_eq!(
        (stats.drawn, stats.visbuffer, stats.fallback),
        (1, 0, 0),
        "{stats:?}"
    );
    assert_eq!(renderer.visbuffer_stats().instances, 0);
    assert!(px(&img, A_RIGHT) > px(&img, A_LEFT) + 30, "entry A's columns");
}

/// [`lightmap`] on three pages (ADR-3300): entry A stays on page 0, entry B moves to page 2 at the
/// same rectangle. Page 0 holds `A_LOW` in the left half of B's old rectangle and page 1 nothing,
/// so a receiver that reads any page but its own draws another light. The right half (columns 2
/// and 3) keeps B's texels on page 0: A's own bilinear and bicubic taps at its left edge reach
/// them (the 8×2 atlas has no gutter), and A must draw the same frame as on one page.
fn paged_lightmap() -> Lightmap {
    let flat = lightmap();
    let n = flat.page_texels();
    let page = || {
        (0..flat.height)
            .flat_map(|y| (0..flat.width).map(move |x| (x, y)))
            .map(|(x, y)| flat.irradiance_at(x, y))
            .collect::<Vec<_>>()
    };
    let mut irr = page();
    // Page 0: the left half of B's rectangle (x 0..2) is no longer B's light.
    for y in 0..flat.height {
        for x in 0..2 {
            irr[(y * flat.width + x) as usize] = [A_LOW; 3];
        }
    }
    let mut cov = flat.coverage.clone();
    cov.extend(vec![Coverage::EMPTY; n]);
    // Page 1 empty, page 2 the flat atlas as it was.
    irr.extend(vec![[0.0; 3]; n]);
    irr.extend(page());
    cov.extend(flat.coverage.iter().copied());
    let mut entries = flat.entries.clone();
    entries[ENTRY_B as usize].page = 2;
    Lightmap::from_irradiance(
        LightmapLayout {
            width: flat.width,
            height: flat.height,
            density: flat.density,
            padding: flat.padding,
            entries,
            unwraps: flat.unwraps.clone(),
        },
        &irr,
        cov,
        flat.bake,
    )
    .expect("valid paged lightmap")
}

/// A receiver on page 2 samples page 2 (ADR-3300, frame readback on lavapipe): on the CPU colour
/// loop, the GPU-driven batches, the visibility buffer and both lite tiers, a frame whose entry B is
/// on page 2 of a three-page lightmap is the frame of the one-page lightmap, pixel for pixel, and
/// the receiver's pixel is not page 0's light. Needs the texture-array bindings of the frame patch
/// (`reports/sprint-lightmap-multipage-atlas-frame.patch`, which removes the `ignore`).
#[test]
fn a_receiver_on_page_2_samples_page_2_in_the_frame() {
    let device = device();
    let flat = Arc::new(lightmap());
    let paged = Arc::new(paged_lightmap());
    assert_eq!(paged.pages(), 3);
    let lite = |tier| Path {
        lite: Some(tier),
        ..CPU
    };
    let mut paths = vec![("cpu", CPU), ("vis", VIS), ("lite_low", lite(ForwardLite::Low))];
    paths.push(("lite_high", lite(ForwardLite::High)));
    if takes_gpu_driven(&device) {
        paths.push(("gpu", GPU));
    }
    for (name, path) in paths {
        let want = frame(&device, &flat, enabled(), path);
        let got = frame(&device, &paged, enabled(), path);
        assert_eq!(got.stats.drawn, want.stats.drawn, "{name}: {:?}", got.stats);
        assert_eq!(
            max_diff(&got.img, &want.img),
            0,
            "{name}: the page-2 receiver draws page 2's light"
        );
        let none = no_lightmap(&device, &flat, path);
        assert!(
            px(&want.img, B_PX).abs_diff(px(&none, B_PX)) > 6,
            "{name}: B is lightmapped"
        );
    }
}

/// A version 5 or 6 file (ADR-3300) reads as one page and draws the frame its migrated version 7
/// file draws, pixel for pixel, on the CPU colour loop, the visibility buffer and the Mobile lite
/// shader (the checked-in fixtures; their two quad entries are receivers A and B).
#[test]
fn version_5_and_6_files_draw_as_their_migrated_one_page_files() {
    let device = device();
    for (version, bytes) in [
        (
            5,
            &include_bytes!("../../regia_asset/tests/fixtures/lightmap-v5.flmap")[..],
        ),
        (
            6,
            &include_bytes!("../../regia_asset/tests/fixtures/lightmap-v6.flmap")[..],
        ),
    ] {
        let old = Arc::new(Lightmap::from_bytes(bytes).expect("reads"));
        assert_eq!(old.pages(), 1, "v{version}");
        let migrated = regia_asset::lightmap::migrate_lightmap_file(bytes).expect("migrates");
        let new = Arc::new(Lightmap::from_bytes(&migrated).expect("reads"));
        assert_eq!(new.pages(), 1);
        let lite = Path {
            lite: Some(ForwardLite::Low),
            ..CPU
        };
        for (name, path) in [("cpu", CPU), ("vis", VIS), ("lite_low", lite)] {
            let a = frame(&device, &old, enabled(), path);
            let b = frame(&device, &new, enabled(), path);
            assert_eq!(a.stats.drawn, b.stats.drawn, "v{version} {name}");
            assert_eq!(max_diff(&a.img, &b.img), 0, "v{version} {name}: the same frame");
            // For a cross-build comparison (ADR-3300 report): the build before pages prints the
            // same line for the same file and path.
            eprintln!("FRAMEHASH v{version} {name} {:016x}", fnv1a(&a.img));
        }
    }
}

/// FNV-1a over a frame's bytes (a fingerprint to compare two builds' frames by eye in a log).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}
