// SPDX-License-Identifier: MPL-2.0
//! Seasons on instanced scatter classes (ADR-3146).
//!
//! The rule is `regia_framework::season::leaf_present` (the framework's own test compares the two): the
//! same instance hash and density give the same answer on the CPU and in `impostor_cull.wgsl` (compute,
//! lavapipe). Off is bit-identical, a tint rewrites parameters under the same keys, and a season change
//! writes no instance on the GPU.

use std::sync::Arc;

use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, LabMaterial, LabScene,
    LabTexture, LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast,
    SharedPassDraw,
};
use regia_terrain::{
    CdlodConfig, ImpostorInstance, ImpostorLayer, ImpostorSettings, LEAF_RANK_SCALE, ScatterField,
    ScatterLibrary, ScatterMeshAsset, ScatterMeshLod, ScatterMeshPart, ScatterSeason, ScatterSet,
    ScatterSource, ScatterView, TerrainLayers, TerrainMap, TerrainSharedDraw, leaf_present, leaf_rank,
};

const SET: &str = r#"
ScatterSet(
    version: 2,
    seed: 23,
    classes: [
        ScatterClass(
            name: "oaks",
            meshes: [
                ScatterMeshRef(mesh: "tree", weight: 1.0, min_scale: 1.0, max_scale: 1.0, scale_bias: 1.0),
            ],
            cell_size: 8.0,
            density: 0.7,
            base_blend: 0.0,
            contact_ao: 0.0,
            fade_start: 120.0,
            fade_end: 160.0,
            cast_shadows: true,
            shadow_max_lod: 1,
            shadow_distance: 0.0,
        ),
        ScatterClass(
            name: "rocks",
            meshes: [
                ScatterMeshRef(mesh: "rock", weight: 1.0, min_scale: 1.0, max_scale: 1.0, scale_bias: 1.0),
            ],
            cell_size: 16.0,
            density: 0.3,
            base_blend: 0.0,
            contact_ao: 0.0,
            fade_start: 120.0,
            fade_end: 160.0,
            cast_shadows: false,
        ),
    ],
)
"#;

const BARK: [f32; 4] = [0.30, 0.20, 0.10, 1.0];
const LEAVES: [f32; 4] = [0.10, 0.50, 0.10, 1.0];

fn plane(y0: f32, y1: f32, half: f32) -> ScatterMeshLod {
    let vertices = [
        ([-half, y0, 0.0], [0.0, 1.0]),
        ([half, y0, 0.0], [1.0, 1.0]),
        ([half, y1, 0.0], [1.0, 0.0]),
        ([-half, y1, 0.0], [0.0, 0.0]),
    ]
    .iter()
    .map(|&(p, uv)| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], uv))
    .collect();
    ScatterMeshLod {
        vertices,
        indices: vec![0, 1, 2, 0, 2, 3],
        screen_size: 0.0,
    }
}

/// A trunk and a masked crown with a hole in its corners.
fn tree() -> ScatterMeshAsset {
    let n = 8u32;
    let mut rgba = Vec::new();
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 + 0.5 - 4.0, y as f32 + 0.5 - 4.0);
            rgba.extend_from_slice(&[200, 200, 200, if dx * dx + dy * dy < 14.0 { 255 } else { 0 }]);
        }
    }
    ScatterMeshAsset {
        lods: vec![plane(0.0, 8.0, 0.4)],
        material: LabMaterial {
            base_color: BARK,
            roughness: 1.0,
            double_sided: true,
            ..LabMaterial::default()
        },
        parts: vec![ScatterMeshPart {
            lods: vec![plane(4.0, 12.0, 4.0)],
            material: LabMaterial {
                base_color: LEAVES,
                roughness: 0.9,
                alpha_mode: ALPHA_MASK,
                alpha_cutoff: 0.5,
                double_sided: true,
                albedo: Some(LabTexture {
                    width: n,
                    height: n,
                    rgba,
                }),
                ..LabMaterial::default()
            },
        }],
        shadow_lod: 0,
    }
}

fn rock() -> ScatterMeshAsset {
    ScatterMeshAsset {
        lods: vec![plane(0.0, 1.0, 1.0)],
        material: LabMaterial {
            base_color: [0.4, 0.4, 0.4, 1.0],
            ..LabMaterial::default()
        },
        parts: Vec::new(),
        shadow_lod: 0,
    }
}

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"TerrainLayers(version: 1, wetness_default: 0.0, layers: [
            LayerDesc(name: "dust", albedo: (0.2, 0.2, 0.2), roughness: 0.92, tiling: 2.0,
                slope_min: 0.0, slope_max: 1.6, altitude_min: -1000.0, altitude_max: 5000.0,
                blend_sharpness: 0.15, triplanar: false),
        ])"#,
    )
    .expect("layers")
}

fn map() -> TerrainMap {
    TerrainMap::from_heights(65, 65, vec![0.0; 65 * 65], 512.0, 1.0, 0.0).expect("flat map")
}

fn field() -> ScatterField {
    let layers = layers();
    let set = ScatterSet::from_ron(SET, Some(&layers)).expect("set");
    let mut library = ScatterLibrary::new();
    library.insert("tree".into(), tree());
    library.insert("rock".into(), rock());
    let source = ScatterSource {
        map: Arc::new(map()),
        holes: None,
        layers: Some(layers),
        density_maps: regia_terrain::DensityMaps::new(),
    };
    let mut f = ScatterField::new(&set, source, &library, 0x5EA5).expect("field");
    f.set_background_streaming(false);
    f
}

const EYE: [f32; 3] = [0.0, 3.0, 40.0];

fn view() -> ScatterView {
    ScatterView::look_at(EYE, [0.0, 3.0, -100.0], 0.9, 16.0 / 9.0, 0.1, 2000.0)
}

fn is_leaf(material: &LabMaterial) -> bool {
    material.alpha_mode == ALPHA_MASK
}

/// Places of the instances that drew or cast their leaves this frame, and of those that drew their bark.
/// Bark is the double-sided unmasked material: the field shades its colour per tint variant, and the
/// rock is single-sided.
fn drawn(f: &ScatterField) -> (Vec<[f32; 2]>, Vec<[f32; 2]>) {
    let mut meshes: Vec<FrameMesh<'_>> = Vec::new();
    f.collect(&view(), &mut meshes);
    let place = |m: &FrameMesh<'_>| [m.model[3][0], m.model[3][2]];
    let leaves = meshes.iter().filter(|m| is_leaf(m.material)).map(place).collect();
    let bark = meshes
        .iter()
        .filter(|m| !is_leaf(m.material) && m.material.double_sided)
        .map(place)
        .collect();
    (leaves, bark)
}

/// Everything the renderer sees of a frame's draws, bit for bit.
type Draw = (u64, u64, u64, [[u32; 4]; 4], [u32; 4], usize, bool);

fn fingerprint(f: &ScatterField) -> Vec<Draw> {
    let mut meshes: Vec<FrameMesh<'_>> = Vec::new();
    f.collect(&view(), &mut meshes);
    meshes
        .iter()
        .map(|m| {
            (
                m.object_key,
                m.mesh_key,
                m.material_key,
                m.model.map(|c| c.map(f32::to_bits)),
                m.material.base_color.map(f32::to_bits),
                m.indices.len(),
                m.shadow != ShadowCast::Off,
            )
        })
        .collect()
}

#[test]
fn a_leafless_instance_drops_its_leaves_and_keeps_its_bark() {
    let mut f = field();
    f.settle_streaming(EYE);
    let (leaves_all, bark_all) = drawn(&f);
    assert_eq!(leaves_all.len(), bark_all.len(), "one crown per trunk");
    assert!(leaves_all.len() > 100, "{} trees in view", leaves_all.len());
    let mut last = leaves_all.len() + 1;
    for density in [1.0, 0.75, 0.5, 0.25, 0.0] {
        assert!(f.set_class_season("oaks", Some(ScatterSeason::new([1.0; 3], density))));
        let (leaves, bark) = drawn(&f);
        let share = leaves.len() as f32 / leaves_all.len() as f32;
        eprintln!(
            "density {density}: {} of {} crowns",
            leaves.len(),
            leaves_all.len()
        );
        assert!((share - density).abs() < 0.1, "density {density}: share {share}");
        assert_eq!(bark, bark_all, "every trunk stays at density {density}");
        assert!(leaves.len() < last, "the crowns thin as the density falls");
        assert!(
            leaves.iter().all(|p| leaves_all.contains(p)),
            "the survivors are crowns that were there"
        );
        last = leaves.len();
    }
    let mut stats_meshes = Vec::new();
    let stats = f.collect(&view(), &mut stats_meshes);
    assert_eq!(
        stats.leafless,
        stats.emitted - rocks_emitted(&stats_meshes),
        "density 0: all bare"
    );
}

/// Rock draws: the single-sided unmasked material (the field shades its colour per tint variant).
fn rocks_emitted(meshes: &[FrameMesh<'_>]) -> u32 {
    meshes
        .iter()
        .filter(|m| !is_leaf(m.material) && !m.material.double_sided)
        .count() as u32
}

#[test]
fn the_crowns_that_fall_are_the_same_in_every_run_and_monotonic() {
    let run = |density: f32| {
        let mut f = field();
        f.settle_streaming(EYE);
        f.set_class_season("oaks", Some(ScatterSeason::new([1.0; 3], density)));
        drawn(&f).0
    };
    let half = run(0.5);
    assert_eq!(half, run(0.5), "two runs keep the same crowns");
    let quarter = run(0.25);
    assert!(
        quarter.iter().all(|p| half.contains(p)),
        "a crown at 0.25 stays at 0.5"
    );
    assert!(quarter.len() < half.len());
}

#[test]
fn a_season_on_another_class_changes_nothing_and_off_is_bit_identical() {
    let mut f = field();
    f.settle_streaming(EYE);
    let plain = fingerprint(&f);
    let (mut meshes, generation) = (Vec::new(), f.state_generation());
    f.collect(&view(), &mut meshes);
    let keep: Vec<(*const LabMaterial, u64)> = meshes
        .iter()
        .map(|m| (m.material as *const LabMaterial, m.material_key))
        .collect();
    drop(meshes);
    assert!(f.set_class_season("oaks", None));
    assert!(f.set_class_season("oaks", Some(ScatterSeason::new([1.0; 3], 1.0))));
    assert_eq!(f.class_season("oaks"), None, "a neutral season reads as none");
    assert_eq!(f.state_generation(), generation, "and changes nothing");
    assert_eq!(fingerprint(&f), plain);
    assert!(!f.set_class_season("no such class", None));
    // On, then off: the very materials and keys the field was built with.
    f.set_class_season("oaks", Some(ScatterSeason::new([2.0, 0.9, 0.4], 0.4)));
    assert_ne!(fingerprint(&f), plain);
    f.set_class_season("oaks", None);
    assert_eq!(
        fingerprint(&f),
        plain,
        "off after on: the same draws, bit for bit"
    );
    let mut meshes = Vec::new();
    f.collect(&view(), &mut meshes);
    let back: Vec<(*const LabMaterial, u64)> = meshes
        .iter()
        .map(|m| (m.material as *const LabMaterial, m.material_key))
        .collect();
    assert_eq!(back, keep, "the built materials themselves");
}

#[test]
fn a_tint_rewrites_the_leaf_material_under_its_key_and_leaves_the_bark() {
    let mut f = field();
    f.settle_streaming(EYE);
    let tint_of = |f: &ScatterField| {
        let mut meshes: Vec<FrameMesh<'_>> = Vec::new();
        f.collect(&view(), &mut meshes);
        let leaf = meshes.iter().find(|m| is_leaf(m.material)).map(|m| {
            (
                m.material_key,
                m.material.base_color,
                m.material as *const LabMaterial,
            )
        });
        let bark = meshes
            .iter()
            .find(|m| m.material.base_color[0] > m.material.base_color[1] && !is_leaf(m.material))
            .map(|m| (m.material_key, m.material.base_color));
        (leaf.expect("a crown"), bark.expect("a trunk"))
    };
    let (leaf_plain, bark_plain) = tint_of(&f);
    f.set_class_season("oaks", Some(ScatterSeason::new([2.0, 1.0, 0.5], 1.0)));
    let (leaf_a, bark_a) = tint_of(&f);
    assert_eq!(bark_a, bark_plain, "the bark takes no tint");
    assert_ne!(leaf_a.0, leaf_plain.0, "the class draws its own copy");
    for k in 0..3 {
        let factor = [2.0, 1.0, 0.5][k];
        assert!(
            (leaf_a.1[k] - leaf_plain.1[k] * factor).abs() < 1e-5,
            "{:?} from {:?}",
            leaf_a.1,
            leaf_plain.1
        );
    }
    // A new tint: the same key and the same material, so the renderer rewrites its uniform only.
    f.set_class_season("oaks", Some(ScatterSeason::new([1.0, 1.5, 1.0], 1.0)));
    let (leaf_b, _) = tint_of(&f);
    assert_eq!(
        (leaf_b.0, leaf_b.2),
        (leaf_a.0, leaf_a.2),
        "no new copy for a new tint"
    );
    assert!((leaf_b.1[1] - leaf_plain.1[1] * 1.5).abs() < 1e-5);
    assert_eq!(leaf_b.1[0], leaf_plain.1[0]);
}

/// A frame in flight holds the copies: the tint waits, and the next call lands it. Extracted draws hold
/// `Arc`s (`ScatterField::extract_meshes`), as a render thread's frame does.
#[test]
fn a_tint_waits_for_a_frame_in_flight() {
    let mut f = field();
    f.settle_streaming(EYE);
    let first = ScatterSeason::new([2.0, 1.0, 0.5], 1.0);
    let second = ScatterSeason::new([0.5, 1.0, 2.0], 1.0);
    f.set_class_season("oaks", Some(first));
    let mut in_flight = Vec::new();
    f.extract_meshes(&view(), &mut in_flight);
    let leaf_base = |f: &ScatterField| {
        let mut meshes: Vec<FrameMesh<'_>> = Vec::new();
        f.collect(&view(), &mut meshes);
        meshes
            .iter()
            .find(|m| is_leaf(m.material))
            .map(|m| m.material.base_color)
            .expect("a crown")
    };
    let held = leaf_base(&f);
    assert!(f.set_class_season("oaks", Some(second)));
    assert_eq!(leaf_base(&f), held, "the copy stays while a frame holds it");
    drop(in_flight);
    assert!(f.set_class_season("oaks", Some(second)));
    assert_ne!(leaf_base(&f), held, "the next call lands the tint");
}

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    RenderDevice::headless().unwrap_or_else(|err| panic!("no headless GPU (lavapipe?): {err}"))
}

/// Runs `impostor_cull.wgsl` over `ranks` at `density` and returns the slots it keeps, with every other
/// test of the cull wide open.
fn gpu_keeps(device: &RenderDevice, ranks: &[u32], density: f32) -> Vec<u32> {
    use wgpu::util::DeviceExt as _;
    let d = &device.device;
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("impostor cull"),
        source: wgpu::ShaderSource::Wgsl(regia_terrain::gpu::impostor::cull_source().into()),
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("impostor cull"),
        layout: None,
        module: &module,
        entry_point: Some("cs_cull"),
        compilation_options: Default::default(),
        cache: None,
    });
    let count = ranks.len() as u32;
    // `ImpParams`: six planes that hold everything, the eye at the origin, a 1000 px target.
    let mut params: Vec<f32> = Vec::new();
    for _ in 0..6 {
        params.extend_from_slice(&[0.0, 0.0, 0.0, 1.0e9]);
    }
    params.extend_from_slice(&[0.0; 4]);
    params.extend_from_slice(&[1000.0, 1.0, 0.05, 0.0]);
    params.extend_from_slice(&[8.0, 32.0, 6.0, 0.001]);
    let mut params: Vec<u8> = bytemuck::cast_slice(&params).to_vec();
    params.extend_from_slice(bytemuck::cast_slice(&[count, 0u32, 0, 0]));
    let instances: Vec<ImpostorInstance> = ranks
        .iter()
        .map(|&rank| ImpostorInstance {
            centre: [0.0, 0.0, -100.0],
            radius: 10.0,
            pivot: [0.0, 0.0, -100.0],
            scale: 1.0,
            rotation: [0, 32767 << 16],
            packed: 0,
            leaf_rank: rank,
        })
        .collect();
    let proto = regia_terrain::ImpostorProto {
        tint: [1.0; 4],
        card: [0.0, 0.0, 10.0, 1.0],
        range: [0.0, 1.0, 1.0e6, 0.0],
        extra: [1.0, -1.0, 0.0, 0.0],
        season: [1.0, 1.0, 1.0, density],
    };
    let storage = |bytes: &[u8], extra: wgpu::BufferUsages| {
        d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: bytes,
            usage: wgpu::BufferUsages::STORAGE | extra,
        })
    };
    let params_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: None,
        contents: &params,
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let instance_buf = storage(bytemuck::cast_slice(&instances), wgpu::BufferUsages::empty());
    let proto_buf = storage(bytemuck::bytes_of(&proto), wgpu::BufferUsages::empty());
    let visible_buf = storage(&vec![0u8; ranks.len() * 4], wgpu::BufferUsages::COPY_SRC);
    let args_buf = storage(
        bytemuck::cast_slice(&[0u32, 1, 0, 0]),
        wgpu::BufferUsages::COPY_SRC,
    );
    // The outdoor-portal tile mask (ADR-8843): every slot's bit set. `counts.z` is 0 here (the eye
    // is in no cell), so the cull does not read it, but the layout still has the binding.
    let tile_mask_buf = storage(
        bytemuck::cast_slice(&vec![u32::MAX; ranks.len().div_ceil(32).max(1)]),
        wgpu::BufferUsages::empty(),
    );
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: instance_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: proto_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: visible_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: args_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: tile_mask_buf.as_entire_binding(),
            },
        ],
    });
    let read = |source: &wgpu::Buffer, bytes: u64, encoder: &mut wgpu::CommandEncoder| {
        let staging = d.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        encoder.copy_buffer_to_buffer(source, 0, &staging, 0, bytes);
        staging
    };
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(count.div_ceil(64), 1, 1);
    }
    let visible_read = read(&visible_buf, ranks.len() as u64 * 4, &mut encoder);
    let args_read = read(&args_buf, 16, &mut encoder);
    device.queue.submit([encoder.finish()]);
    let words = |buffer: &wgpu::Buffer| -> Vec<u32> {
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let mapped = slice.get_mapped_range().expect("mapped");
        let out = bytemuck::cast_slice(&mapped).to_vec();
        drop(mapped);
        buffer.unmap();
        out
    };
    let args = words(&args_read);
    let visible = words(&visible_read);
    let kept = (args[0] / 6) as usize;
    let mut slots = visible[..kept].to_vec();
    slots.sort_unstable();
    slots
}

/// The same instance hash and density give the same answer on the CPU and on the GPU.
#[test]
fn the_cpu_and_the_gpu_keep_the_same_instances() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let hashes: Vec<u64> = (0..4096u64)
        .map(|i| i.wrapping_mul(0x0000_0100_0000_01b3) ^ 0x7777)
        .collect();
    let ranks: Vec<u32> = hashes.iter().map(|&h| leaf_rank(h)).collect();
    for density in [0.0, 1.0e-4, 0.1, 0.25, 1.0 / 3.0, 0.5, 0.6, 0.75, 0.999, 1.0] {
        let cpu: Vec<u32> = hashes
            .iter()
            .enumerate()
            .filter(|(_, h)| leaf_present(**h, density))
            .map(|(slot, _)| slot as u32)
            .collect();
        let gpu = gpu_keeps(&device, &ranks, density);
        eprintln!("density {density}: CPU {} kept, GPU {}", cpu.len(), gpu.len());
        assert_eq!(cpu, gpu, "density {density}");
    }
    // At the edge of the comparison: a rank exactly at the density falls, the rank below it stays.
    let rank = 0x00A5_5A5Au32;
    let density = rank as f32 / LEAF_RANK_SCALE;
    let edge = [rank - 1, rank, rank + 1];
    assert_eq!(gpu_keeps(&device, &edge, density), vec![0], "the edge rank");
    assert!(regia_terrain::scatter::season::rank_present(rank - 1, density));
    assert!(!regia_terrain::scatter::season::rank_present(rank, density));
    assert!(!regia_terrain::scatter::season::rank_present(rank + 1, density));
}

/// The impostors' leaf rank and the mesh path's answer come from the one object key.
#[test]
fn an_impostors_rank_is_its_instances() {
    let mut f = field();
    f.set_impostors(Some(ImpostorSettings {
        grid: 4,
        cell: 8,
        ..ImpostorSettings::default()
    }));
    f.settle_streaming(EYE);
    let mut instances = Vec::new();
    f.impostor_instances(&mut instances);
    assert!(instances.len() > 100);
    // Distinct, 24 bits, about a half under one half.
    let under = instances.iter().filter(|i| i.leaf_rank < 1 << 23).count();
    assert!(instances.iter().all(|i| i.leaf_rank < 1 << 24));
    let share = under as f32 / instances.len() as f32;
    assert!((share - 0.5).abs() < 0.1, "share {share}");
}

const FAR_W: u32 = 192;
const FAR_H: u32 = 108;

struct FarRig {
    device: RenderDevice,
    renderer: FrameRenderer,
    field: ScatterField,
    layer: ImpostorLayer,
    post: PostSettings,
    quality: FrameQuality,
}

impl FarRig {
    /// Impostors only, the field's season set before the bake as `season` says.
    fn new(season: Option<ScatterSeason>) -> Self {
        let device = device();
        let layers = layers();
        let mut field = field();
        let settings = ImpostorSettings {
            grid: 6,
            cell: 16,
            ..ImpostorSettings::default()
        };
        field.set_impostors(Some(settings));
        field.set_class_season("oaks", season);
        field.settle_streaming(EYE);
        let atlas = field.bake_impostors(Some(settings));
        let layer = ImpostorLayer::new(&device.device, &device.queue, &atlas);
        let mut draw =
            TerrainSharedDraw::new(&device, map(), &layers, CdlodConfig::default(), 5).expect("terrain");
        draw.set_impostors(Some(&layer));
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        let post = PostSettings {
            auto_exposure: false,
            exposure_ev: 14.0,
            bloom: false,
            ..PostSettings::golden_deterministic()
        };
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.render_scale_permille = 1000;
        quality.gpu_driven = false;
        quality.screen_space_gi = false;
        quality.ssr = false;
        quality.volumetric_fog = false;
        quality.volumetric_clouds = false;
        quality.gi_probes = false;
        quality.taau = false;
        Self {
            device,
            renderer,
            field,
            layer,
            post,
            quality,
        }
    }

    /// The far field from far enough out that everything is an impostor: no mesh is submitted.
    fn frame(&mut self) -> Vec<u8> {
        let eye = [0.0, 30.0, 256.0];
        let target = [0.0, 0.0, 0.0];
        self.field.update(eye);
        self.layer.update(&self.device.queue, &self.field);
        let lab = LabScene {
            width: FAR_W,
            height: FAR_H,
            eye,
            target,
            fov_y: 0.5,
            near: 0.5,
            far: 3000.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 14.0,
            ambient_lux: 12_000.0,
            double_sided: false,
            post: None,
            directional: DirectionalLightUniform::new([0.3, -0.7, -0.6], [1.0, 1.0, 1.0], 60_000.0),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        };
        let mut scene = lab.as_frame_scene_with_quality(self.quality);
        scene.post = Some(&self.post);
        self.renderer
            .render_offscreen(&self.device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels
    }
}

/// Pixels whose green leads the other channels: the far crowns.
fn green_pixels(pixels: &[u8]) -> usize {
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[1] > p[0].saturating_add(10) && p[1] > p[2].saturating_add(10))
        .count()
}

#[test]
fn a_season_on_the_far_field_writes_the_table_and_no_instance() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rig = FarRig::new(None);
    let plain = rig.frame();
    let again = rig.frame();
    assert_eq!(plain, again, "control: a held far field keeps its bytes");
    let uploads = rig.layer.instance_uploads();
    assert!(uploads > 0, "the instances were uploaded");
    let full = green_pixels(&plain);
    assert!(full > 200, "{full} crown pixels");

    rig.field
        .set_class_season("oaks", Some(ScatterSeason::new([1.0; 3], 0.3)));
    let thin = green_pixels(&rig.frame());
    assert_eq!(
        rig.layer.instance_uploads(),
        uploads,
        "a season change writes no instance"
    );
    eprintln!("far crown pixels: full {full}, density 0.3 {thin}");
    assert!(thin < full, "the far field thins: {full} -> {thin}");
    assert!(thin > 0, "and keeps some crowns");

    rig.field
        .set_class_season("oaks", Some(ScatterSeason::new([1.0; 3], 0.0)));
    assert_eq!(green_pixels(&rig.frame()), 0, "density 0: no crown left");

    // A tint alone: the crowns stay and turn. Leaves of (0.1, 0.5, 0.1) under (6, 0.6, 0.3) are
    // (0.6, 0.3, 0.03): red leads.
    rig.field
        .set_class_season("oaks", Some(ScatterSeason::new([6.0, 0.6, 0.3], 1.0)));
    let tinted = rig.frame();
    assert!(
        green_pixels(&tinted) * 2 < full,
        "an orange tint leaves {} of {full} green pixels",
        green_pixels(&tinted)
    );
    assert_eq!(rig.layer.instance_uploads(), uploads, "still no instance");

    // Off: the bytes of a far field that never had a season, in the same layer.
    rig.field.set_class_season("oaks", None);
    assert_eq!(rig.frame(), plain, "off is bit-identical");
    assert_eq!(rig.layer.instance_uploads(), uploads);
}

/// The card holds every part of a tree with the holes of its masked ones, and a season at bake time adds a
/// card of what stays without leaves: a leafless far tree is its trunk.
#[test]
fn a_tree_card_holds_its_crown_and_a_season_bakes_a_bare_card() {
    let settings = ImpostorSettings {
        grid: 4,
        cell: 16,
        ..ImpostorSettings::default()
    };
    let mut f = field();
    let plain = f.bake_impostors(Some(settings));
    assert_eq!(plain.cards.len(), 2, "one card per mesh: {}", plain.cards.len());
    assert!(
        plain
            .protos
            .iter()
            .all(|p| p.extra[1] < 0.0 && p.season == [1.0; 4]),
        "no bare card without a season"
    );
    f.set_class_season("oaks", Some(ScatterSeason::new([1.0; 3], 0.5)));
    let seasonal = f.bake_impostors(Some(settings));
    assert_eq!(seasonal.cards.len(), 3, "the oak's bare card");
    let bare_layer = seasonal
        .protos
        .iter()
        .map(|p| p.extra[1])
        .find(|&layer| layer >= 0.0)
        .expect("a bare layer") as usize;
    let covered = |card: &regia_terrain::ImpostorCard| {
        card.albedo[0]
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] >= 128)
            .count()
    };
    let oak = seasonal.protos[0].card[0] as usize;
    let (crown, trunk) = (
        covered(&seasonal.cards[oak]),
        covered(&seasonal.cards[bare_layer]),
    );
    eprintln!("covered texels: with leaves {crown}, bare {trunk}");
    assert!(trunk > 0, "the trunk remains");
    assert!(crown > 4 * trunk, "the crown is on the card: {crown} vs {trunk}");
    let total = seasonal.cards[oak].albedo[0].len() / 4;
    assert!(
        crown * 2 < total,
        "the mask's holes stay holes: {crown} of {total} texels"
    );
    let rock_proto = seasonal.protos.len() - 1;
    assert!(
        seasonal.protos[rock_proto].extra[1] < 0.0,
        "another class bakes as before"
    );
}

/// With a bare card a leafless far tree draws that card: the frame keeps its trunks where the plain card
/// would have left nothing.
#[test]
fn a_leafless_far_tree_draws_its_bare_card() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let winter = ScatterSeason::new([1.0; 3], 0.0);
    let mut empty = FarRig::new(None);
    empty.field.set_class_season("oaks", Some(winter));
    let dropped = empty.frame();
    let mut bare = FarRig::new(Some(winter));
    let trunks = bare.frame();
    assert_eq!(green_pixels(&trunks), 0, "no crown on a bare card");
    let differing = dropped
        .as_chunks::<4>()
        .0
        .iter()
        .zip(trunks.as_chunks::<4>().0)
        .filter(|(a, b)| a != b)
        .count();
    eprintln!("{differing} pixels hold trunks");
    assert!(differing > 10, "the bare cards draw: {differing} pixels");
}
