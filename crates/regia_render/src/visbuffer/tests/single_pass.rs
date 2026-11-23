// SPDX-License-Identifier: MPL-2.0
//! The single-pass resolve, the shared samplers and the dense-scene selection (ADR-2071).

use super::super::VisInstanceSet;
use super::*;
use crate::texture_table::{MaterialSampler, MaterialSamplers};
use crate::{VisibilityPath, VisibilitySelect};

/// A table device and one without the table on the test adapter, or `None` when it cannot.
fn devices() -> Option<(crate::RenderDevice, crate::RenderDevice)> {
    crate::texture_table::frame_tests::pair()
        .filter(|(table, _)| super::super::single_pass::fallback_on(&table.device).is_none())
}

fn table_device() -> Option<crate::RenderDevice> {
    devices().map(|(table, _)| table)
}

const MATERIALS: usize = 6;

/// Six materials over one floor, each with its own checker size, some emissive or normal-mapped.
struct TableFixture {
    fixture: Fixture,
    materials: Vec<LabMaterial>,
}

fn table_fixture() -> TableFixture {
    let fixture = fixture(4);
    let materials = (0..MATERIALS)
        .map(|i| LabMaterial {
            albedo: Some(checker(8 << (i % 4))),
            normal_map: (i % 3 == 1).then(|| checker(16)),
            base_color: [1.0 - 0.1 * i as f32, 0.5 + 0.08 * i as f32, 0.4, 1.0],
            roughness: 0.2 + 0.12 * i as f32,
            emissive: if i == 2 { [0.2, 0.05, 0.0] } else { [0.0; 3] },
            emissive_intensity: 1000.0,
            ..LabMaterial::default()
        })
        .collect();
    TableFixture { fixture, materials }
}

fn table_scene<'a>(
    f: &'a TableFixture,
    post: &'a PostSettings,
    path: VisibilityPath,
    vis: bool,
) -> FrameScene<'a> {
    let mut frame = scene(&f.fixture, post, vis, false);
    frame.quality.visibility_path = path;
    for (index, mesh) in frame.meshes.iter_mut().enumerate().skip(1) {
        let m = index % MATERIALS;
        mesh.material = &f.materials[m];
        mesh.material_key = 2000 + m as u64;
    }
    frame
}

const SINGLE: VisibilityPath = VisibilityPath {
    select: VisibilitySelect::Always,
    single_pass_resolve: true,
};

fn table_frames(
    device: &crate::RenderDevice,
    f: &TableFixture,
    path: VisibilityPath,
) -> (FrameRenderer, Vec<u8>) {
    let settings = post();
    let mut renderer = comparison_renderer(device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    let frame = table_scene(f, &settings, path, true);
    render(&mut renderer, device, &frame);
    let pixels = render(&mut renderer, device, &frame);
    (renderer, pixels)
}

#[test]
fn single_pass_frames_equal_the_per_material_resolve_byte_for_byte() {
    let Some(device) = table_device() else {
        return;
    };
    let f = table_fixture();
    let (per_material, classic) = table_frames(&device, &f, VisibilityPath::DEFAULT);
    let (single, pixels) = table_frames(&device, &f, SINGLE);
    let report = per_material.visbuffer_resolve_report();
    assert!(!report.requested && !report.single_pass, "{report:?}");
    assert_eq!(report.draws, 1 + u64::from(report.materials), "{report:?}");
    let report = single.visbuffer_resolve_report();
    assert!(report.requested && report.single_pass, "{report:?}");
    assert_eq!(report.fallback, None);
    assert_eq!(report.draws, 1, "{report:?}");
    assert!(report.materials >= 3, "{report:?}");
    assert_eq!(classic.len(), pixels.len());
    let (worst, close) = diff(&classic, &pixels);
    assert_eq!(worst, 0, "{:.3}% within 2 levels", close * 100.0);
}

/// The arena is replaced at 256 materials: the map and the group follow it, and every material
/// still shades its own record.
#[test]
fn single_pass_follows_the_arena_when_it_grows() {
    let Some(device) = table_device() else {
        return;
    };
    let settings = post();
    let f = table_fixture();
    let many: Vec<LabMaterial> = (0..300)
        .map(|i| LabMaterial {
            base_color: [
                (i % 7) as f32 / 7.0,
                (i % 11) as f32 / 11.0,
                (i % 13) as f32 / 13.0,
                1.0,
            ],
            ..LabMaterial::default()
        })
        .collect();
    let draw = |path: VisibilityPath| {
        let mut renderer = comparison_renderer(&device);
        renderer.set_meshlet_builder(Some(single_level_builder()));
        // First the few materials, then enough to replace the arena.
        let few = table_scene(&f, &settings, path, true);
        render(&mut renderer, &device, &few);
        let mut crowded = table_scene(&f, &settings, path, true);
        for (index, mesh) in crowded.meshes.iter_mut().enumerate().skip(1) {
            let m = index % many.len();
            mesh.material = &many[m];
            mesh.material_key = 5000 + m as u64;
        }
        // Touch every record so the arena really holds 300.
        let filler: Vec<FrameMesh<'_>> = many
            .iter()
            .enumerate()
            .map(|(m, material)| FrameMesh {
                material,
                material_key: 5000 + m as u64,
                object_key: 9000 + m as u64,
                model: translate(1000.0, -1000.0, m as f32, 0.01),
                ..crowded.meshes[1]
            })
            .collect();
        crowded.meshes.extend(filler);
        render(&mut renderer, &device, &crowded);
        let pixels = render(&mut renderer, &device, &crowded);
        (renderer.visbuffer_resolve_report(), pixels)
    };
    let (classic_report, classic) = draw(VisibilityPath::DEFAULT);
    let (report, pixels) = draw(SINGLE);
    assert!(report.single_pass && !classic_report.single_pass, "{report:?}");
    let (worst, close) = diff(&classic, &pixels);
    assert_eq!(worst, 0, "{:.3}% within 2 levels", close * 100.0);
}

/// The slot map names materials by slot, so the frames must not care which slots they hold: high
/// ones near the rebuild, or the fresh ones after it. Presetting the table stands in for the tens
/// of thousands of materials a long session would take to get there.
#[test]
fn single_pass_frames_survive_high_slots_and_the_table_rebuild() {
    let Some(device) = table_device() else {
        return;
    };
    let f = table_fixture();
    let settings = post();
    let rebuild_at = super::super::gpu::MATERIAL_SLOTS_REBUILD_AT;
    let frame = table_scene(&f, &settings, SINGLE, true);
    let material_key = frame.meshes[1].material_key;
    let mut control = comparison_renderer(&device);
    control.set_meshlet_builder(Some(single_level_builder()));
    let first = render(&mut control, &device, &frame);
    let second = render(&mut control, &device, &frame);

    let mut renderer = comparison_renderer(&device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    // One short of the rebuild, so this frame keeps the table and its materials land on top.
    renderer.visbuffer_pass_mut().fill_material_slots(rebuild_at - 1);
    let high = render(&mut renderer, &device, &frame);
    let (entries, slot) = renderer.visbuffer_pass_mut().material_slot_state(material_key);
    assert!(
        slot.is_some_and(|slot| slot as usize >= rebuild_at - 1),
        "control: the material took a high slot ({slot:?}, {entries} entries)"
    );
    assert!(
        entries >= rebuild_at,
        "control: the next frame rebuilds ({entries})"
    );
    assert!(renderer.visbuffer_resolve_report().single_pass);
    assert_eq!(first, high, "the frame does not depend on the slot numbers");

    let rebuilt = render(&mut renderer, &device, &frame);
    let (entries, slot) = renderer.visbuffer_pass_mut().material_slot_state(material_key);
    assert!(
        entries < MATERIALS * 2 && slot.is_some_and(|slot| (slot as usize) < MATERIALS * 2),
        "the table was rebuilt from this frame's materials ({slot:?}, {entries} entries)"
    );
    assert!(renderer.visbuffer_resolve_report().single_pass);
    assert_eq!(second, rebuilt, "the rebuild is invisible");
}

#[test]
fn a_warm_single_pass_frame_creates_nothing() {
    let Some(device) = table_device() else {
        return;
    };
    let settings = post();
    let f = table_fixture();
    let mut renderer = comparison_renderer(&device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    let frame = table_scene(&f, &settings, SINGLE, true);
    for _ in 0..3 {
        render(&mut renderer, &device, &frame);
    }
    assert!(renderer.visbuffer_resolve_report().single_pass);
    let before = renderer.visbuffer_create_stats();
    for _ in 0..5 {
        render(&mut renderer, &device, &frame);
    }
    assert!(renderer.visbuffer_resolve_report().single_pass);
    let after = renderer.visbuffer_create_stats();
    assert_eq!(after.total.objects(), before.total.objects(), "{after:?}");
    assert_eq!(after.last_frame.upload_bytes, before.last_frame.upload_bytes);
    assert!(after.quiet_frames >= 5, "{after:?}");
}

#[test]
fn off_the_table_the_request_keeps_the_per_material_resolve_and_says_why() {
    let Some((_, device)) = devices() else {
        return;
    };
    let f = table_fixture();
    let (_, classic) = table_frames(&device, &f, VisibilityPath::DEFAULT);
    let (asked, pixels) = table_frames(&device, &f, SINGLE);
    let report = asked.visbuffer_resolve_report();
    assert!(report.requested && !report.single_pass, "{report:?}");
    assert_eq!(report.fallback, Some(crate::SinglePassFallback::NoTextureTable));
    assert_eq!(
        report.fallback.map(crate::SinglePassFallback::as_str),
        Some("vis_no_texture_table")
    );
    assert_eq!(classic, pixels);
}

// ---- The module: what validation alone cannot show (ADR-356, fact 4) ----

fn single_pass_module() -> naga::Module {
    crate::shader_lib::compose_source(
        super::super::single_pass::single_pass_source(),
        "vis_resolve_single.wgsl",
        super::super::single_pass::checked_defs(),
    )
    .expect("compose")
}

fn validate(module: &naga::Module) -> naga::valid::ModuleInfo {
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(module)
    .expect("validate")
}

/// Functions reachable from `entry` (their handles), by walking call statements.
fn reachable(module: &naga::Module, entry: &naga::Function) -> Vec<naga::Handle<naga::Function>> {
    fn calls(block: &naga::Block, out: &mut Vec<naga::Handle<naga::Function>>) {
        for statement in block.iter() {
            match statement {
                naga::Statement::Call { function, .. } => out.push(*function),
                naga::Statement::Block(inner) => calls(inner, out),
                naga::Statement::If { accept, reject, .. } => {
                    calls(accept, out);
                    calls(reject, out);
                }
                naga::Statement::Switch { cases, .. } => {
                    for case in cases {
                        calls(&case.body, out);
                    }
                }
                naga::Statement::Loop { body, continuing, .. } => {
                    calls(body, out);
                    calls(continuing, out);
                }
                _ => {}
            }
        }
    }
    let mut seen = Vec::new();
    let mut stack = Vec::new();
    calls(&entry.body, &mut stack);
    while let Some(handle) = stack.pop() {
        if !seen.contains(&handle) {
            seen.push(handle);
            calls(&module.functions[handle].body, &mut stack);
        }
    }
    seen
}

fn implicit_derivatives(function: &naga::Function, what: &str, out: &mut Vec<String>) {
    for (_, expression) in function.expressions.iter() {
        match expression {
            naga::Expression::ImageSample {
                level: naga::SampleLevel::Auto | naga::SampleLevel::Bias(_),
                ..
            } => out.push(format!("{what}: implicit-LOD sample")),
            naga::Expression::Derivative { .. } => out.push(format!("{what}: derivative")),
            _ => {}
        }
    }
}

/// Every implicit derivative reachable from `fs_vis_resolve` in `module`.
fn implicit_derivatives_of_the_resolve(module: &naga::Module) -> Vec<String> {
    let entry = module
        .entry_points
        .iter()
        .find(|e| e.name == "fs_vis_resolve")
        .expect("entry");
    let mut found = Vec::new();
    implicit_derivatives(&entry.function, "fs_vis_resolve", &mut found);
    for handle in reachable(module, &entry.function) {
        let function = &module.functions[handle];
        let name = function.name.clone().unwrap_or_default();
        implicit_derivatives(function, &name, &mut found);
    }
    found
}

#[test]
fn no_implicit_derivative_is_reachable_from_the_single_pass_entry() {
    let module = single_pass_module();
    validate(&module);
    let found = implicit_derivatives_of_the_resolve(&module);
    assert!(found.is_empty(), "{found:#?}");
    // The walk sees them where they are: the per-material resolve keeps one behind a branch that
    // its record never takes, in the same function this variant replaced.
    let mut defs = super::super::single_pass::checked_defs();
    defs.remove(super::super::single_pass::VIS_SINGLE_PASS_DEF);
    let classic =
        crate::shader_lib::compose_source(super::super::gpu::resolve_source(), "vis_resolve.wgsl", defs)
            .expect("compose");
    let control = implicit_derivatives_of_the_resolve(&classic);
    assert!(
        control.iter().any(|hit| hit.contains("sample_slot_with")),
        "the walk found nothing in the per-material resolve: {control:#?}"
    );
}

#[test]
fn the_single_pass_module_binds_the_arena_the_map_and_the_table() {
    let module = single_pass_module();
    let bindings = |group: u32| {
        let mut b: Vec<u32> = module
            .global_variables
            .iter()
            .filter_map(|(_, v)| v.binding.as_ref())
            .filter(|b| b.group == group)
            .map(|b| b.binding)
            .collect();
        b.sort_unstable();
        b
    };
    assert_eq!(bindings(2), vec![0, 1], "the whole arena and the slot map");
    assert_eq!(bindings(4), vec![0, 1, 3, 4], "the table and the sampler set");
    assert!(
        module
            .global_variables
            .iter()
            .all(|(_, v)| v.name.as_deref() != Some("material")),
        "no one-record binding is left"
    );
}

/// Every access into the texture table carries `NonUniform`: hardware needs the decoration on a
/// per-pixel index, and neither naga's validation nor `spirv-val` can tell it is missing.
#[test]
fn the_table_index_carries_the_non_uniform_decoration() {
    const OP_NAME_DECORATE: u32 = 71;
    const OP_ACCESS_CHAIN: u32 = 65;
    const OP_IN_BOUNDS_ACCESS_CHAIN: u32 = 66;
    const DECORATION_NON_UNIFORM: u32 = 5300;
    const DECORATION_BINDING: u32 = 33;
    const DECORATION_SET: u32 = 34;
    let module = single_pass_module();
    let info = validate(&module);
    let words = naga::back::spv::write_vec(
        &module,
        &info,
        &naga::back::spv::Options {
            lang_version: (1, 5),
            ..Default::default()
        },
        None,
    )
    .expect("spv");
    let words = words.as_slice();
    let instructions = || {
        let mut at = 5;
        std::iter::from_fn(move || {
            let word = *words.get(at)?;
            let length = (word >> 16).max(1) as usize;
            let range = at..at + length;
            at += length;
            Some((word & 0xffff, range))
        })
    };
    let mut in_table_group = std::collections::BTreeSet::new();
    let mut binding_zero = std::collections::BTreeSet::new();
    let mut non_uniform = std::collections::BTreeSet::new();
    for (op, range) in instructions() {
        let inst = &words[range];
        if op == OP_NAME_DECORATE {
            match inst[2] {
                DECORATION_SET if inst[3] == crate::texture_table::TEXTURE_TABLE_GROUP => {
                    in_table_group.insert(inst[1]);
                }
                DECORATION_BINDING if inst[3] == 0 => {
                    binding_zero.insert(inst[1]);
                }
                DECORATION_NON_UNIFORM => {
                    non_uniform.insert(inst[1]);
                }
                _ => {}
            }
        }
    }
    let table: Vec<u32> = in_table_group.intersection(&binding_zero).copied().collect();
    assert_eq!(table.len(), 1, "one texture table variable: {table:?}");
    let mut accesses = 0;
    for (op, range) in instructions() {
        let inst = &words[range];
        if matches!(op, OP_ACCESS_CHAIN | OP_IN_BOUNDS_ACCESS_CHAIN) && inst[3] == table[0] {
            accesses += 1;
            assert!(
                non_uniform.contains(&inst[2]),
                "table access %{} has no NonUniform decoration",
                inst[2]
            );
        }
    }
    assert!(accesses > 0, "the module never indexes the table");
}

/// `spirv-val` and the explicit-layout scan (ADR-026), which `shader_check` cannot run on this
/// variant: its gate device has no binding-array features.
#[test]
fn the_single_pass_module_passes_spirv_val() {
    crate::shader_check::spirv_validate("vis_resolve_single.wgsl", &single_pass_module())
        .unwrap_or_else(|err| panic!("{err}"));
}

// ---- The shared samplers ----

/// A quad at `uv` -1..2 (past the edge of the texture), filling the frame.
fn sampler_scene<'a>(
    f: &'a Fixture,
    material: &'a LabMaterial,
    data: &'a (Vec<LabVertex>, Vec<u32>),
    post: &'a PostSettings,
    vis: bool,
    single: bool,
) -> FrameScene<'a> {
    let mut frame = scene(f, post, vis, false);
    frame.quality.visibility_path.single_pass_resolve = single;
    frame.eye = [0.0, 0.0, 2.0];
    frame.target = [0.0, 0.0, 0.0];
    frame.directional_shadows = false;
    frame.double_sided = true;
    frame.meshes = vec![FrameMesh {
        is_static: false,
        mesh_key: 11,
        material_key: 12,
        object_key: 13,
        vertices: &data.0,
        indices: &data.1,
        material,
        model: translate(0.0, 0.0, 0.0, 1.0),
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    }];
    frame
}

fn big_uv_quad() -> (Vec<LabVertex>, Vec<u32>) {
    let corner =
        |x: f32, y: f32, u: f32, v: f32| LabVertex::from_pos_normal_uv([x, y, 0.0], [0.0, 0.0, 1.0], [u, v]);
    let mut vertices = vec![
        corner(-2.0, -2.0, -1.0, 2.0),
        corner(2.0, -2.0, 2.0, 2.0),
        corner(2.0, 2.0, 2.0, -1.0),
        corner(-2.0, 2.0, -1.0, -1.0),
    ];
    let indices = vec![0, 1, 2, 0, 2, 3];
    // A normal map needs tangents: without them the slot's sampler has nothing to show.
    for vertex in &mut vertices {
        vertex.tangent = [1.0, 0.0, 0.0, 1.0];
    }
    (vertices, indices)
}

/// Dark on the left half, light on the right: repeating and clamping differ past the edge, and
/// nearest and linear differ at the middle.
fn half_and_half() -> LabTexture {
    const SIZE: u32 = 16;
    let rgba = (0..SIZE * SIZE)
        .flat_map(|i| {
            if i % SIZE < SIZE / 2 {
                [30, 30, 30, 255]
            } else {
                [220, 220, 220, 255]
            }
        })
        .collect();
    LabTexture {
        width: SIZE,
        height: SIZE,
        rgba,
    }
}

fn sampler_material(sampler: MaterialSampler) -> LabMaterial {
    LabMaterial {
        albedo: Some(half_and_half()),
        unlit: true,
        samplers: MaterialSamplers {
            base_color: sampler,
            ..MaterialSamplers::default()
        },
        ..LabMaterial::default()
    }
}

fn sampler_frame(device: &crate::RenderDevice, sampler: MaterialSampler, vis: bool, single: bool) -> Vec<u8> {
    let settings = post();
    let f = fixture(1);
    let data = big_uv_quad();
    let material = sampler_material(sampler);
    let mut renderer = comparison_renderer(device);
    if vis {
        renderer.set_meshlet_builder(Some(single_level_builder()));
    }
    let frame = sampler_scene(&f, &material, &data, &settings, vis, single);
    render(&mut renderer, device, &frame);
    render(&mut renderer, device, &frame)
}

#[test]
fn the_sampler_set_changes_only_what_a_material_chooses() {
    let Some((device, classic)) = devices() else {
        return;
    };
    let repeat = sampler_frame(&device, MaterialSampler::Repeat, false, false);
    let clamp = sampler_frame(&device, MaterialSampler::Clamp, false, false);
    let point = sampler_frame(&device, MaterialSampler::PointClamp, false, false);
    assert_ne!(repeat, clamp, "a clamped checker stops at the edge");
    assert_ne!(clamp, point, "point sampling is not linear");
    // The default is the sampler of every earlier release: the same frame off the table.
    let off = sampler_frame(&classic, MaterialSampler::Repeat, false, false);
    assert_eq!(off, repeat, "the table's default sampler is the per-material one");
}

/// Cases of [`slot_material`]: the four glTF slots, then the normal map read by the detail branch.
const SLOT_CASES: usize = 5;

/// One lit quad with one map on one slot, through `sampler` on that slot.
fn slot_material(slot: usize, sampler: MaterialSampler) -> LabMaterial {
    let mut material = LabMaterial {
        roughness: 0.5,
        ..LabMaterial::default()
    };
    let map = Some(half_and_half());
    let mut samplers = MaterialSamplers::default();
    match slot {
        0 => {
            material.albedo = map;
            samplers.base_color = sampler;
        }
        1 => {
            material.normal_map = map;
            material.normal_scale = 1.0;
            samplers.normal = sampler;
        }
        2 => {
            material.occlusion_map = map;
            material.occlusion_strength = 1.0;
            samplers.orm = sampler;
        }
        3 => {
            material.emissive_map = map;
            material.emissive = [1.0; 3];
            material.emissive_intensity = 40_000.0;
            samplers.emissive = sampler;
        }
        // The detail branch reads the normal again. The base branch's read still reaches the
        // frame through the Toksvig roughness, so these frames differ whatever this branch reads:
        // the two resolves agree on it here, and `table_slots_read_through_the_sampler_helpers`
        // holds its sampler.
        _ => {
            material.normal_map = map;
            material.normal_scale = 1.0;
            material.detail_normal = Some(LabTexture {
                width: 1,
                height: 1,
                rgba: vec![128, 128, 255, 255],
            });
            material.hard_surface.detail_normal_strength = 1.0;
            samplers.normal = sampler;
        }
    }
    material.samplers = samplers;
    material
}

fn slot_frame(
    device: &crate::RenderDevice,
    slot: usize,
    sampler: MaterialSampler,
    single: bool,
) -> (Vec<u8>, crate::VisResolveReport) {
    let settings = post();
    let f = fixture(1);
    let data = big_uv_quad();
    let material = slot_material(slot, sampler);
    let mut renderer = comparison_renderer(device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    let frame = sampler_scene(&f, &material, &data, &settings, true, single);
    render(&mut renderer, device, &frame);
    let pixels = render(&mut renderer, device, &frame);
    assert!(
        renderer.visbuffer_active(),
        "slot {slot}: the frame took the path"
    );
    (pixels, renderer.visbuffer_resolve_report())
}

/// Every glTF slot reads its own choice through the single pass, in the pixels the per-material
/// resolve draws. A resolve that ignored the choice would still match the other resolve, so the
/// choices must also differ from each other.
#[test]
fn the_single_pass_resolve_reads_each_slots_sampler() {
    let Some(device) = table_device() else {
        return;
    };
    for slot in 0..SLOT_CASES {
        let mut frames = Vec::new();
        for sampler in MaterialSampler::ALL {
            let (per_material, report) = slot_frame(&device, slot, sampler, false);
            assert!(!report.single_pass, "slot {slot}: {report:?}");
            let (single, report) = slot_frame(&device, slot, sampler, true);
            assert!(report.single_pass, "slot {slot}: {report:?}");
            assert_eq!(report.draws, 1, "slot {slot}");
            assert_eq!(per_material, single, "slot {slot} {}", sampler.as_str());
            frames.push(single);
        }
        assert_ne!(frames[0], frames[1], "slot {slot}: repeat and clamp");
        assert_ne!(frames[1], frames[2], "slot {slot}: clamp and point");
    }
}

/// The sites of `source` that sample a glTF slot of the table directly, passing by the slot's
/// sampler choice. Frames cannot always show one (the detail branch's normal read, above).
fn direct_slot_samples(source: &str) -> Vec<String> {
    source
        .lines()
        .filter(|line| line.contains("textureSample"))
        .filter(|line| (0..4).any(|slot| line.contains(&format!("[table_index({slot}u)]"))))
        .map(|line| line.trim().to_string())
        .collect()
}

/// Every sample of a glTF slot on the table goes through a helper that reads the slot's choice.
#[test]
fn table_slots_read_through_the_sampler_helpers() {
    assert_eq!(
        direct_slot_samples(
            "let ts = textureSampleLevel(texture_table[table_index(1u)], tex_samp, uv, 0.0);"
        )
        .len(),
        1,
        "control: the search finds the mistake it looks for"
    );
    for (name, source) in [
        ("lab_forward", include_str!("../../../shaders/lab_forward.wgsl")),
        ("lab_shadow", include_str!("../../../shaders/lab_shadow.wgsl")),
        (
            "translucent_shadow",
            include_str!("../../../shaders/translucent_shadow.wgsl"),
        ),
        (
            "forward_mobile",
            include_str!("../../../shaders/forward_mobile.wgsl"),
        ),
    ] {
        assert_eq!(direct_slot_samples(source), Vec::<String>::new(), "{name}");
    }
}

/// The forward path reads the same choice: a clamped map draws differently from a repeating one,
/// there as on the path.
#[test]
fn the_forward_path_reads_each_slots_sampler() {
    let Some(device) = table_device() else {
        return;
    };
    let settings = post();
    let f = fixture(1);
    let data = big_uv_quad();
    for slot in 0..SLOT_CASES {
        let frame_of = |sampler| {
            let material = slot_material(slot, sampler);
            let mut renderer = comparison_renderer(&device);
            let frame = sampler_scene(&f, &material, &data, &settings, false, false);
            render(&mut renderer, &device, &frame);
            render(&mut renderer, &device, &frame)
        };
        assert_ne!(
            frame_of(MaterialSampler::Repeat),
            frame_of(MaterialSampler::Clamp),
            "slot {slot}"
        );
    }
}

/// Persistent sets take their record through the slot map like the mesh list.
#[test]
fn single_pass_shades_persistent_sets_like_the_per_material_resolve() {
    let Some(device) = table_device() else {
        return;
    };
    let f = fixture(2);
    let settings = post();
    let models = [translate(-1.0, 0.5, -2.0, 1.0), translate(1.0, 0.5, -2.0, 1.0)];
    let draw = |path: VisibilityPath| {
        let mut renderer = comparison_renderer(&device);
        renderer.set_meshlet_builder(Some(single_level_builder()));
        assert!(renderer.set_instance_set(
            77,
            &VisInstanceSet {
                mesh_key: 2,
                vertices: &f.sphere.0,
                indices: &f.sphere.1,
                material_key: 1002,
                material: &f.rock,
                models: &models,
                shadow: ShadowCast::All,
                instance_shadows: &[],
            }
        ));
        let mut frame = scene(&f, &settings, true, false);
        frame.quality.visibility_path = path;
        render(&mut renderer, &device, &frame);
        let pixels = render(&mut renderer, &device, &frame);
        (renderer.visbuffer_resolve_report(), pixels)
    };
    let (report_a, per_material) = draw(VisibilityPath::DEFAULT);
    let (report_b, single) = draw(SINGLE);
    assert!(!report_a.single_pass && report_b.single_pass, "{report_b:?}");
    let (worst, close) = diff(&per_material, &single);
    assert_eq!(worst, 0, "{:.3}% within 2 levels", close * 100.0);
}

/// A streamed material fades in on the single pass's own gradient branch (`min_lod > 0`), every
/// frame in the pixels of the per-material resolve.
#[test]
fn a_streamed_material_fades_in_identically_through_the_single_pass() {
    let Some(device) = table_device() else {
        return;
    };
    let pattern = |size: u32| {
        let mut rgba = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            for x in 0..size {
                let on = ((x / 8) + (y / 8)).is_multiple_of(2);
                rgba.extend_from_slice(&[if on { 200 } else { 60 }, (x * 255 / size) as u8, 90, 255]);
            }
        }
        LabTexture {
            width: size,
            height: size,
            rgba,
        }
    };
    let material = LabMaterial {
        albedo: Some(pattern(256)),
        ..LabMaterial::default()
    };
    let streaming = crate::TextureStreaming {
        enabled: true,
        initial_max_px: 16,
        max_px: 0,
        upload_kib_per_frame: 24,
        upload_us_per_frame: 1_000_000_000,
        fade_frames: 3,
        need_margin_percent: 200,
        resident_all: true,
        budget_mib: 0,
        eager: false,
    };
    let settings = post();
    let f = fixture(1);
    let data = big_uv_quad();
    let frame_with = |single: bool| {
        let mut frame = sampler_scene(&f, &material, &data, &settings, true, single);
        frame.quality.texture_streaming = streaming;
        frame
    };
    let mut per_material = comparison_renderer(&device);
    per_material.set_meshlet_builder(Some(single_level_builder()));
    let mut one_draw = comparison_renderer(&device);
    one_draw.set_meshlet_builder(Some(single_level_builder()));
    let (classic_frame, single_frame) = (frame_with(false), frame_with(true));
    let mut first = None;
    let mut changed = false;
    for frame in 0..24 {
        let a = render(&mut per_material, &device, &classic_frame);
        let b = render(&mut one_draw, &device, &single_frame);
        assert!(one_draw.visbuffer_resolve_report().single_pass, "frame {frame}");
        assert_eq!(a, b, "frame {frame}");
        let start = first.get_or_insert_with(|| b.clone());
        changed |= *start != b;
    }
    assert!(changed, "the material never swapped: the fade branch did not run");
}

/// While a graph's set builds on its workers the mesh draws the standard surface. On the table
/// path that needs a record the standard pipelines can bind: no validation error, no hole, on the
/// forward path and through both resolves, and the graph takes over when its set lands.
#[test]
fn a_pending_graph_draws_a_standard_twin_on_the_table() {
    const GRAPH: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    return regia::pbr::surface::Surface(
        vec3<f32>(0.1, 0.9, 0.1),
        1.0,
        0.0,
        1.0,
        in.world_normal,
        1.0,
        vec3<f32>(0.0),
        1.0,
    );
}
";
    let Some(device) = table_device() else {
        return;
    };
    let settings = post();
    let f = fixture(1);
    let data = big_uv_quad();
    let material = LabMaterial {
        base_color: [0.9, 0.1, 0.1, 1.0],
        surface_graph: Some(Arc::new(crate::SurfaceGraph::new(GRAPH))),
        ..LabMaterial::default()
    };
    for (vis, single) in [(false, false), (true, false), (true, true)] {
        let scope = device.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut renderer = comparison_renderer(&device);
        renderer.set_async_material_graphs(true);
        renderer.set_meshlet_builder(Some(single_level_builder()));
        let frame = sampler_scene(&f, &material, &data, &settings, vis, single);
        let pending = render(&mut renderer, &device, &frame);
        if vis {
            let report = renderer.visbuffer_resolve_report();
            assert_eq!(report.single_pass, single, "{report:?}");
            assert_eq!(report.materials, 1, "{report:?}");
        }
        let error = regia_core::tasks::block_on(scope.pop());
        assert!(error.is_none(), "vis {vis} single {single}: {error:?}");
        // The pass did not leave a hole: the quad fills the frame in the base colour's red.
        let red = pending
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > p[1].saturating_add(16))
            .count();
        assert!(
            red * 2 > pending.len() / 4,
            "vis {vis} single {single}: {red} red pixels of {}",
            pending.len() / 4
        );
        renderer.settle_material_graphs();
        render(&mut renderer, &device, &frame);
        let landed = render(&mut renderer, &device, &frame);
        assert_ne!(pending, landed, "vis {vis} single {single}: the graph took over");
    }
}

/// A sampler choice under a stable material key reaches the record on the next frame.
///
/// The exact comparison runs without the temporal resolve: with it, the control's history never
/// saw the repeat frames, so equality would also ask the resolve to reject every bit of the
/// earlier frames' history, which is not what this test is about (ADR-3954). The Desktop High
/// default (temporal resolve on) is checked too: exact on Vulkan; on Metal the M5 keeps 1830
/// blue values one level apart at every later frame, and none with the resolve off
/// (`sampler_change_diagnostics`), so there it may differ by at most one level.
#[test]
fn a_material_can_change_its_sampler_under_its_key() {
    let Some(device) = table_device() else {
        return;
    };
    let settings = post();
    let f = fixture(1);
    let data = big_uv_quad();
    let repeat = sampler_material(MaterialSampler::Repeat);
    let clamp = sampler_material(MaterialSampler::Clamp);
    for taau in [false, true] {
        let mut first = sampler_scene(&f, &repeat, &data, &settings, false, false);
        let mut second = sampler_scene(&f, &clamp, &data, &settings, false, false);
        for scene in [&mut first, &mut second] {
            scene.quality.taau &= taau;
        }
        let mut renderer = comparison_renderer(&device);
        render(&mut renderer, &device, &first);
        let a = render(&mut renderer, &device, &first);
        render(&mut renderer, &device, &second);
        let b = render(&mut renderer, &device, &second);
        assert_ne!(a, b, "taau {taau}: the record took the new choice");
        // The same four frames with the clamp from the start: only the choice differed.
        let mut control = comparison_renderer(&device);
        for _ in 0..3 {
            render(&mut control, &device, &second);
        }
        let c = render(&mut control, &device, &second);
        if !taau || device.capabilities.backend != wgpu::Backend::Metal {
            assert_eq!(b, c, "taau {taau}");
        } else {
            let worst = b.iter().zip(&c).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
            assert!(
                worst <= 1,
                "taau on, Metal: the changed renderer differs from the control by {worst} levels"
            );
        }
    }
}

/// ADR-3373 diagnostics for a backend where the test above fails (Metal: 1830 pixels, blue only,
/// one level): the difference between the changed renderer and the clamp-from-the-start control
/// at frames 4 to 12. A difference that fades is history from the earlier sampler; one that stays
/// is state the change left behind. Each comparison also runs with the temporal resolve off and
/// with the static-screen reuse off. `cargo test -p regia_render --lib -- --ignored --nocapture
/// sampler_change_diagnostics`.
#[test]
#[ignore = "diagnostics: prints the sampler change's difference over frames, asserts nothing"]
fn sampler_change_diagnostics() {
    let Some(device) = table_device() else {
        return;
    };
    let settings = post();
    let f = fixture(1);
    let data = big_uv_quad();
    let repeat = sampler_material(MaterialSampler::Repeat);
    let clamp = sampler_material(MaterialSampler::Clamp);
    let info = device.adapter.get_info();
    println!("adapter {} ({:?})", info.name, info.backend);
    // mac-pass-v0217: the same comparison with the temporal resolve off, with the static-screen
    // reuse off (`paged_sun_shadows` false turns `reuse_static_screen` off), and with both: tells
    // a frozen resolve or a held gather (computed at another frame index) from state the sampler
    // change itself leaves behind.
    for (name, taau, reuse) in [
        ("default", true, true),
        ("taau off", false, true),
        ("screen reuse off", true, false),
        ("taau and reuse off", false, false),
    ] {
        let mut first = sampler_scene(&f, &repeat, &data, &settings, false, false);
        let mut second = sampler_scene(&f, &clamp, &data, &settings, false, false);
        for scene in [&mut first, &mut second] {
            scene.quality.taau &= taau;
            scene.quality.paged_sun_shadows &= reuse;
        }
        println!(
            "variant {name}: taau {} reuse_static_screen {}",
            second.quality.taau,
            second.quality.reuse_static_screen()
        );
        let mut changed = comparison_renderer(&device);
        let mut control = comparison_renderer(&device);
        for frame in 1..=12 {
            let scene = if frame <= 2 { &first } else { &second };
            let a = render(&mut changed, &device, scene);
            let b = render(&mut control, &device, &second);
            if frame < 3 {
                continue;
            }
            let (mut count, mut worst, mut channels) = (0usize, 0u8, [0usize; 4]);
            for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
                if pa == pb {
                    continue;
                }
                count += 1;
                for ((x, y), channel) in pa.iter().zip(pb).zip(channels.iter_mut()) {
                    if x != y {
                        *channel += 1;
                        worst = worst.max(x.abs_diff(*y));
                    }
                }
            }
            println!(
                "  frame {frame}: {count} pixels differ (by channel {channels:?}), largest {worst}; \
                 screen cache hit changed {} control {}",
                changed.screen_pass_report().screen_cache_hit,
                control.screen_pass_report().screen_cache_hit
            );
        }
    }
}

/// A sampler choice is a material change: the epoch joins the static-screen key, so a held view
/// must see it move, or it keeps last frame's screen passes.
#[test]
fn a_sampler_change_moves_the_material_epoch() {
    let Some(device) = table_device() else {
        return;
    };
    let settings = post();
    let f = fixture(1);
    let data = big_uv_quad();
    let mut renderer = comparison_renderer(&device);
    let repeat = sampler_material(MaterialSampler::Repeat);
    let clamp = sampler_material(MaterialSampler::Clamp);
    let first = sampler_scene(&f, &repeat, &data, &settings, false, false);
    render(&mut renderer, &device, &first);
    let steady = renderer.material_epoch_for_tests();
    render(&mut renderer, &device, &first);
    assert_eq!(
        renderer.material_epoch_for_tests(),
        steady,
        "control: an unchanged material leaves the epoch alone"
    );
    let second = sampler_scene(&f, &clamp, &data, &settings, false, false);
    render(&mut renderer, &device, &second);
    assert_ne!(
        renderer.material_epoch_for_tests(),
        steady,
        "the choice changed under the key"
    );
}

/// ADR-9603: a dynamic texture (a video's picture) rewritten in place is a material change too:
/// the renderer copies the new picture into its resident texture under the same material key,
/// so the epoch has to move, or a held view keeps last frame's screen passes (or, where the
/// static-screen reuse is on, its whole composite) over the new picture. `render_out_job_video`
/// rendered output frames 16 and 17 alike on every RTX 5090.
#[test]
fn a_dynamic_texture_write_moves_the_material_epoch() {
    let device = crate::device::test_device();
    let settings = post();
    let f = fixture(1);
    let data = big_uv_quad();
    let picture = crate::DynamicTexture::new(4, 4, true);
    picture.write(&[40u8; 64]).expect("first picture");
    let material = LabMaterial {
        unlit: true,
        images: crate::MaterialImages {
            albedo: Some(crate::MaterialImage::Dynamic(picture.clone())),
            ..crate::MaterialImages::EMPTY
        },
        ..LabMaterial::default()
    };
    let frame = sampler_scene(&f, &material, &data, &settings, false, false);
    let mut renderer = comparison_renderer(&device);
    render(&mut renderer, &device, &frame);
    let steady = renderer.material_epoch_for_tests();
    render(&mut renderer, &device, &frame);
    assert_eq!(
        renderer.material_epoch_for_tests(),
        steady,
        "control: a picture already copied leaves the epoch alone"
    );
    picture.write(&[200u8; 64]).expect("next picture");
    render(&mut renderer, &device, &frame);
    assert_ne!(
        renderer.material_epoch_for_tests(),
        steady,
        "the picture changed under the key"
    );
    let moved = renderer.material_epoch_for_tests();
    render(&mut renderer, &device, &frame);
    assert_eq!(
        renderer.material_epoch_for_tests(),
        moved,
        "a held picture leaves the epoch alone again"
    );
}

#[test]
fn sampler_bits_pack_two_per_slot() {
    let samplers = MaterialSamplers {
        base_color: MaterialSampler::Clamp,
        normal: MaterialSampler::PointClamp,
        orm: MaterialSampler::Repeat,
        emissive: MaterialSampler::Clamp,
    };
    assert_eq!(samplers.bits(), 0b01_00_10_01);
    assert_eq!(MaterialSamplers::default().bits(), 0);
    assert_eq!(MaterialSamplers::DEFAULT, MaterialSamplers::default());
}

// ---- The dense-scene selection ----

fn dense_quality(enter_instances: u32) -> VisibilityPath {
    VisibilityPath {
        select: VisibilitySelect::Dense(crate::DenseSceneSelect {
            enter_triangles: u64::MAX,
            enter_instances,
            enter_frames: 2,
            leave_frames: 3,
        }),
        single_pass_resolve: false,
    }
}

#[test]
fn a_sparse_scene_under_the_dense_select_is_the_classic_frame() {
    let device = crate::test_device();
    let f = fixture(3);
    let settings = post();
    let mut classic = comparison_renderer(&device);
    let off = scene(&f, &settings, false, false);
    render(&mut classic, &device, &off);
    let reference = render(&mut classic, &device, &off);

    let mut selected = comparison_renderer(&device);
    selected.set_meshlet_builder(Some(single_level_builder()));
    let mut asked = scene(&f, &settings, true, false);
    asked.quality.visibility_path = dense_quality(100_000);
    render(&mut selected, &device, &asked);
    let pixels = render(&mut selected, &device, &asked);
    assert!(!selected.visbuffer_active());
    assert_eq!(selected.visbuffer_stats().frames, 0, "no frame took the path");
    assert_eq!(reference, pixels, "a declined frame is the classic frame");
    assert_eq!(selected.visbuffer_dense_report().entered, 0);
}

#[test]
fn a_dense_scene_takes_the_path_and_gives_it_back() {
    let device = crate::test_device();
    let f = fixture(6);
    let settings = post();
    let mut renderer = comparison_renderer(&device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    let mut dense = scene(&f, &settings, true, false);
    let total = dense.meshes.len() as u32;
    assert!(total >= 6, "{total}");
    dense.quality.visibility_path = dense_quality(total);
    render(&mut renderer, &device, &dense);
    assert!(!renderer.visbuffer_active(), "one frame is not a dense scene yet");
    render(&mut renderer, &device, &dense);
    assert!(renderer.visbuffer_active(), "the second frame in a row");
    assert_eq!(renderer.visbuffer_stats().frames, 1);
    let report = renderer.visbuffer_dense_report();
    assert_eq!(report.entered, 1);
    assert_eq!(report.density.instances, total);
    // Under half the threshold for `leave_frames`: the scene gives the path back.
    let mut sparse = scene(&f, &settings, true, false);
    sparse.meshes.truncate(1);
    sparse.quality.visibility_path = dense_quality(total);
    for frame in 0..2 {
        render(&mut renderer, &device, &sparse);
        assert!(
            renderer.visbuffer_active(),
            "frame {frame} under the threshold stays"
        );
    }
    render(&mut renderer, &device, &sparse);
    assert!(!renderer.visbuffer_active(), "the third frame in a row leaves");
    assert_eq!(renderer.visbuffer_dense_report().left, 1);
    // Back on `Always` the gate forgets and the switch alone decides.
    let mut always = scene(&f, &settings, true, false);
    always.quality.visibility_path = VisibilityPath::DEFAULT;
    render(&mut renderer, &device, &always);
    assert!(renderer.visbuffer_active());
    assert!(!renderer.visbuffer_dense_report().dense);
}

/// The switch going off forgets the scene: turned back on, a sparse level starts sparse.
#[test]
fn switching_the_path_off_forgets_a_dense_scene() {
    let device = crate::test_device();
    let f = fixture(6);
    let settings = post();
    let mut renderer = comparison_renderer(&device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    let mut dense = scene(&f, &settings, true, false);
    let total = dense.meshes.len() as u32;
    dense.quality.visibility_path = dense_quality(total);
    render(&mut renderer, &device, &dense);
    render(&mut renderer, &device, &dense);
    assert!(
        renderer.visbuffer_dense_report().dense,
        "control: the scene is dense"
    );

    let mut off = scene(&f, &settings, false, false);
    off.quality.visibility_path = dense_quality(total);
    render(&mut renderer, &device, &off);
    assert!(!renderer.visbuffer_active());
    assert!(!renderer.visbuffer_dense_report().dense, "the switch went off");

    // A gate that remembered would hold this sparse frame on the path for `leave_frames`.
    let mut sparse = scene(&f, &settings, true, false);
    sparse.meshes.truncate(1);
    sparse.quality.visibility_path = dense_quality(total);
    render(&mut renderer, &device, &sparse);
    assert!(!renderer.visbuffer_active(), "a sparse level starts sparse");
}

#[test]
fn persistent_sets_keep_the_path_under_the_dense_select() {
    let device = crate::test_device();
    let f = fixture(3);
    let settings = post();
    let mut renderer = comparison_renderer(&device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    let models = [translate(0.0, 0.5, -2.0, 1.0)];
    assert!(renderer.set_instance_set(
        77,
        &VisInstanceSet {
            mesh_key: 2,
            vertices: &f.sphere.0,
            indices: &f.sphere.1,
            material_key: 1002,
            material: &f.rock,
            models: &models,
            shadow: ShadowCast::All,
            instance_shadows: &[],
        }
    ));
    let mut frame = scene(&f, &settings, true, false);
    frame.quality.visibility_path = dense_quality(1 << 20);
    render(&mut renderer, &device, &frame);
    assert!(renderer.visbuffer_active(), "sets draw nowhere else");
    assert!(!renderer.visbuffer_dense_report().dense);
}

#[test]
fn only_desktop_high_and_ultra_offer_the_dense_profile() {
    for preset in RenderProfilePreset::ALL {
        let default = RenderProfile::from_preset(preset);
        assert_eq!(default.visibility_path, VisibilityPath::DEFAULT, "{preset:?}");
        assert!(!default.visibility_buffer, "{preset:?}");
        let dense = RenderProfile::dense_scenes(preset);
        let offered = matches!(
            preset,
            RenderProfilePreset::DesktopHigh | RenderProfilePreset::DesktopUltra
        );
        assert_eq!(dense.is_some(), offered, "{preset:?}");
        if let Some(dense) = dense {
            assert!(dense.visibility_buffer);
            assert!(dense.visibility_path.single_pass_resolve);
            assert_eq!(
                dense.visibility_path.select,
                VisibilitySelect::Dense(crate::DenseSceneSelect::DEFAULT)
            );
        }
    }
    let lab = FrameQuality::LAB;
    assert!(!lab.visibility_buffer);
    assert_eq!(lab.visibility_path, VisibilityPath::DEFAULT);
}
