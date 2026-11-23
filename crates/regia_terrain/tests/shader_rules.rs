// SPDX-License-Identifier: MPL-2.0
//! ADR-026 gate for the terrain shaders (`cargo xtask check-shaders` covers only
//! `regia_render`). naga 30 decorates struct and array types with explicit layout and
//! reuses them for Function-storage variables; SPIR-V 1.6 rejects that
//! (`VUID-StandaloneSpirv-None-10684`), Mesa refuses the module, and wgpu-hal hides the
//! message. Two checks per module:
//!
//! 1. no function-local `var` of a struct or array type in the naga IR;
//! 2. the SPIR-V 1.6 naga emits (wgpu-hal's options) passes `spirv-val --target-env
//!    vulkan1.3` — this also catches dynamic indexing of a value array, which naga spills
//!    to a decorated Function local.

use std::io::Write;
use std::process::{Command, Stdio};

const SHADERS: [(&str, &str); 5] = [
    ("terrain.wgsl", include_str!("../src/gpu/terrain.wgsl")),
    ("vt.wgsl", include_str!("../src/gpu/vt.wgsl")),
    ("scatter.wgsl", include_str!("../src/gpu/scatter.wgsl")),
    ("water.wgsl", include_str!("../src/gpu/water.wgsl")),
    ("sky.wgsl", include_str!("../src/gpu/sky.wgsl")),
];

/// The composed impostor modules (cull; draw over both terrain variants).
fn impostor_modules() -> Vec<(&'static str, String)> {
    use regia_terrain::gpu::impostor::{cull_source, draw_source};
    vec![
        ("impostor_cull.wgsl", cull_source()),
        ("impostor.wgsl", draw_source(SHADERS[0].1)),
        (
            "impostor.wgsl (hw height)",
            draw_source(&regia_terrain::gpu::hw_height_source()),
        ),
    ]
}

/// The composed grass modules (generator; blade draw over both terrain variants, ADR-1400).
fn grass_modules() -> Vec<(&'static str, String)> {
    use regia_terrain::gpu::grass::{cull_source, draw_source, weather_draw_source};
    vec![
        ("grass_cull.wgsl", cull_source()),
        ("grass.wgsl", draw_source(SHADERS[0].1)),
        (
            "grass.wgsl (hw height)",
            draw_source(&regia_terrain::gpu::hw_height_source()),
        ),
        // What a dry frame compiles (spec 99 § 6.4): the terrain's weather stripped too.
        (
            "grass.wgsl (dry)",
            draw_source(&regia_terrain::gpu::without_surface_weather(SHADERS[0].1)),
        ),
        // The surface-weather variant of a wet or snowy frame (grass stage 3, ADR-3249).
        ("grass.wgsl (weather)", weather_draw_source(SHADERS[0].1)),
        (
            "grass.wgsl (weather, hw height)",
            weather_draw_source(&regia_terrain::gpu::hw_height_source()),
        ),
    ]
}

/// The terrain with the engine's clustered point and spot lights composed in
/// (`with_engine_lights`), over both height-read variants.
fn lights_modules() -> Vec<(&'static str, String)> {
    use regia_terrain::gpu::{hw_height_source, with_engine_lights};
    vec![
        ("terrain.wgsl (lights)", with_engine_lights(SHADERS[0].1)),
        (
            "terrain.wgsl (lights, hw height)",
            with_engine_lights(&hw_height_source()),
        ),
    ]
}

/// The shared colour pass's full sources: the engine's irradiance probes and point and spot
/// lights composed in. Only `spirv-val` covers these: the probe module
/// (`regia_render::ddgi_shading_wgsl`) has struct function locals and a dynamic vector index
/// (`ddgi_cube_dir`) the ADR-026 / Mali rules below would flag; it is the engine's code.
fn shared_modules() -> Vec<(&'static str, String)> {
    use regia_terrain::gpu::{shared_source, without_cost_views, without_ground_look};
    vec![
        ("terrain.wgsl (shared)", shared_source(false, true)),
        ("terrain.wgsl (shared, hw height)", shared_source(true, true)),
        // What the shared pipelines compile while the ground look is off (ADR-1452).
        (
            "terrain.wgsl (shared, no ground look)",
            without_ground_look(&shared_source(false, true)),
        ),
        (
            "terrain.wgsl (shared, hw height, no ground look)",
            without_ground_look(&shared_source(true, true)),
        ),
        // ... and with no cost debug view on (ADR-1491).
        (
            "terrain.wgsl (shared, no ground look, no cost views)",
            without_cost_views(&without_ground_look(&shared_source(false, true))),
        ),
        (
            "terrain.wgsl (shared, hw height, no ground look, no cost views)",
            without_cost_views(&without_ground_look(&shared_source(true, true))),
        ),
    ]
}

fn parse(name: &str, source: &str) -> (naga::Module, naga::valid::ModuleInfo) {
    let module = match naga::front::wgsl::parse_str(source) {
        Ok(module) => module,
        Err(err) => panic!("{name}: {}", err.emit_to_string(source)),
    };
    let info = match naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    {
        Ok(info) => info,
        Err(err) => panic!("{name}: naga validation: {err:?}"),
    };
    (module, info)
}

/// Function-local variables whose type is a struct or an array (the ADR-026 pattern).
fn aggregate_locals(module: &naga::Module) -> Vec<String> {
    let mut found = Vec::new();
    let functions = module
        .functions
        .iter()
        .map(|(_, f)| f)
        .chain(module.entry_points.iter().map(|ep| &ep.function));
    for function in functions {
        for (_, var) in function.local_variables.iter() {
            let inner = &module.types[var.ty].inner;
            if matches!(
                inner,
                naga::TypeInner::Struct { .. } | naga::TypeInner::Array { .. }
            ) {
                found.push(format!(
                    "{}: var {}",
                    function.name.as_deref().unwrap_or("?"),
                    var.name.as_deref().unwrap_or("?")
                ));
            }
        }
    }
    found
}

fn identity_binding_map(module: &naga::Module) -> naga::back::spv::BindingMap {
    let mut map = naga::back::spv::BindingMap::default();
    for (_, var) in module.global_variables.iter() {
        if let Some(binding) = &var.binding {
            map.insert(
                *binding,
                naga::back::spv::BindingInfo {
                    descriptor_set: binding.group,
                    binding: binding.binding,
                    binding_array_size: None,
                },
            );
        }
    }
    map
}

/// wgpu-hal's desktop Vulkan 1.3 options (mirrors `regia_render::shader_check`).
fn spirv_options(module: &naga::Module) -> naga::back::spv::Options<'static> {
    naga::back::spv::Options {
        lang_version: (1, 6),
        flags: naga::back::spv::WriterFlags::LABEL_VARYINGS | naga::back::spv::WriterFlags::FORCE_POINT_SIZE,
        fake_missing_bindings: false,
        binding_map: identity_binding_map(module),
        capabilities: None,
        bounds_check_policies: naga::proc::BoundsCheckPolicies {
            index: naga::proc::BoundsCheckPolicy::Restrict,
            buffer: naga::proc::BoundsCheckPolicy::Unchecked,
            image_load: naga::proc::BoundsCheckPolicy::Restrict,
            binding_array: naga::proc::BoundsCheckPolicy::Unchecked,
        },
        zero_initialize_workgroup_memory: naga::back::spv::ZeroInitializeWorkgroupMemoryMode::Native,
        force_loop_bounding: true,
        ray_query_initialization_tracking: true,
        trace_ray_argument_validation: true,
        use_storage_input_output_16: false,
        debug_info: None,
        task_dispatch_limits: None,
        mesh_shader_primitive_indices_clamp: true,
        emit_int_div_checks: true,
    }
}

/// `Ok(None)` when `spirv-val` is not installed (the IR check still ran). `constants` are the
/// pipeline-overridable constants a pipeline sets (`override` declarations), resolved first as
/// wgpu does (`terrain.wgsl`'s `hole_cut`).
fn spirv_val(
    name: &str,
    module: &naga::Module,
    info: &naga::valid::ModuleInfo,
    constants: &naga::back::PipelineConstants,
) -> Result<Option<()>, String> {
    spirv_val_entry(name, module, info, constants, None)
}

/// [`spirv_val`] for one entry point (`Some((stage, name))`): overrides resolved and the module
/// written for that entry point alone, as wgpu compiles a pipeline stage.
fn spirv_val_entry(
    name: &str,
    module: &naga::Module,
    info: &naga::valid::ModuleInfo,
    constants: &naga::back::PipelineConstants,
    entry: Option<(naga::ShaderStage, &str)>,
) -> Result<Option<()>, String> {
    let (module, info) = naga::back::pipeline_constants::process_overrides(module, info, entry, constants)
        .map_err(|err| format!("{name}: overrides {constants:?}: {err}"))?;
    let pipeline = entry.map(|(shader_stage, entry_point)| naga::back::spv::PipelineOptions {
        shader_stage,
        entry_point: entry_point.to_owned(),
    });
    let words = naga::back::spv::write_vec(&module, &info, &spirv_options(&module), pipeline.as_ref())
        .map_err(|err| format!("{name}: spirv emit: {err}"))?;
    let mut child = match Command::new("spirv-val")
        .args(["--target-env", "vulkan1.3", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Ok(None),
    };
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(&bytes)
            .map_err(|err| format!("{name}: spirv-val write: {err}"))?;
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .map_err(|err| format!("{name}: spirv-val wait: {err}"))?;
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    if output.status.success() || naga_workgroup_stride_only(&diagnostic) {
        Ok(Some(()))
    } else {
        Err(format!(
            "{name}: spirv-val: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        ))
    }
}

/// The one `spirv-val` diagnostic the engine's shader gate allowlists (ADR-135): naga's
/// `ArrayStride` on a Workgroup array (`VUID-StandaloneSpirv-None-10684` on the Workgroup storage
/// class), which drivers accept. The same VUID on a Function or Private variable stays an error
/// (and `terrain_shaders_have_no_aggregate_function_locals` checks those in the IR); a diagnostic
/// naming any other VUID is never ignored.
fn naga_workgroup_stride_only(diagnostic: &str) -> bool {
    diagnostic.contains("VUID-StandaloneSpirv-None-10684")
        && diagnostic.contains("the Workgroup storage class")
        && diagnostic.matches("VUID-").count() == 1
}

/// No terrain shader declares a struct- or array-typed function-local `var`.
#[test]
fn terrain_shaders_have_no_aggregate_function_locals() {
    let mut problems = Vec::new();
    let hw = regia_terrain::gpu::hw_height_source();
    let impostors: Vec<_> = impostor_modules().into_iter().chain(grass_modules()).collect();
    let shared = lights_modules();
    for (name, source) in SHADERS
        .into_iter()
        .chain([("terrain.wgsl (hw height)", hw.as_str())])
        .chain(impostors.iter().map(|(n, s)| (*n, s.as_str())))
        .chain(shared.iter().map(|(n, s)| (*n, s.as_str())))
    {
        let (module, _) = parse(name, source);
        for local in aggregate_locals(&module) {
            problems.push(format!("{name}: {local}"));
        }
    }
    assert!(
        problems.is_empty(),
        "ADR-026: struct/array function locals:\n{}",
        problems.join("\n")
    );
}

/// The SPIR-V 1.6 naga emits for every terrain shader passes `spirv-val`.
#[test]
fn terrain_shaders_pass_spirv_val() {
    let mut problems = Vec::new();
    let mut validated = 0;
    let hw = regia_terrain::gpu::hw_height_source();
    // The grass generator as a whole module; the blade draw per entry point
    // (`grass_draw_entry_points_pass_spirv_val`).
    let impostors: Vec<_> = impostor_modules()
        .into_iter()
        .chain(grass_modules().into_iter().take(1))
        .collect();
    let shared: Vec<_> = lights_modules().into_iter().chain(shared_modules()).collect();
    for (name, source) in SHADERS
        .into_iter()
        .chain([("terrain.wgsl (hw height)", hw.as_str())])
        .chain(impostors.iter().map(|(n, s)| (*n, s.as_str())))
        .chain(shared.iter().map(|(n, s)| (*n, s.as_str())))
    {
        let (module, info) = parse(name, source);
        // Every override at its default, then each boolean override switched on (the
        // pipelines that set it: `hole_cut` for the hole-cut colour and receiver passes).
        let mut sets = vec![naga::back::PipelineConstants::default()];
        for (_, o) in module.overrides.iter() {
            if let Some(o_name) = &o.name {
                let mut set = naga::back::PipelineConstants::default();
                set.insert(o_name.clone(), 1.0);
                sets.push(set);
            }
        }
        for constants in &sets {
            match spirv_val(name, &module, &info, constants) {
                Ok(Some(())) => validated += 1,
                Ok(None) => {}
                Err(err) => problems.push(err),
            }
        }
    }
    if validated == 0 && problems.is_empty() {
        eprintln!("spirv-val not installed: only the IR check ran (install SPIRV-Tools)");
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The grass blade draw (terrain source + `grass.wgsl`, both height variants) passes `spirv-val`
/// per entry point, with `hole_cut` off and on: the way wgpu compiles its pipelines. naga 30.0.1's
/// whole-module writer panics on this module after resolving the overrides ("Expression … is
/// not cached"); every entry point alone writes and validates (ADR-1400).
#[test]
fn grass_draw_entry_points_pass_spirv_val() {
    let mut problems = Vec::new();
    let mut checked = 0;
    for (name, source) in grass_modules().into_iter().skip(1) {
        let (module, info) = parse(name, &source);
        let mut sets = vec![naga::back::PipelineConstants::default()];
        let mut on = naga::back::PipelineConstants::default();
        on.insert("hole_cut".to_owned(), 1.0);
        sets.push(on);
        for ep in module.entry_points.iter().filter(|ep| ep.name.contains("grass")) {
            for constants in &sets {
                checked += 1;
                if let Err(err) = spirv_val_entry(name, &module, &info, constants, Some((ep.stage, &ep.name)))
                {
                    problems.push(format!("{}: {err}", ep.name));
                }
            }
        }
    }
    assert!(checked >= 24, "{checked} grass entry points checked");
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The shared source really carries the engine's point and spot lights (the markers were
/// found and the code composed): group 3 from binding 10.
#[test]
fn the_shared_variant_carries_the_engine_lights() {
    for (name, source) in shared_modules() {
        assert!(
            source.contains("@group(3) @binding(10)") && !source.contains("@engine-lights-begin"),
            "{name}"
        );
    }
}

/// The hardware-filtered variant really replaces the fallback (the markers were found).
#[test]
fn the_hw_height_variant_reads_the_filtered_view() {
    let hw = regia_terrain::gpu::hw_height_source();
    assert!(hw.contains("height_filtered") && !hw.contains("@hw-height-begin"));
}

/// Terrain modules reach the GPU through `regia_render::wgsl_source`, which specialises
/// texture parameters away: Mali's compiler (Pixel 6, r54p3) segfaults on `textureLoad`
/// through one (`tex_height_at`). A raw `include_wgsl!` took the DO27 map down on the phone.
#[test]
fn terrain_modules_are_mali_safe() {
    let gpu = include_str!("../src/gpu/mod.rs");
    assert!(
        !gpu.contains("include_wgsl!") && !gpu.contains("ShaderSource::Wgsl"),
        "create terrain modules with regia_render::wgsl_source"
    );
    let mut problems = Vec::new();
    let hw = regia_terrain::gpu::hw_height_source();
    let shared = lights_modules();
    for (name, source) in SHADERS
        .into_iter()
        .chain([("terrain.wgsl (hw height)", hw.as_str())])
        .chain(shared.iter().map(|(n, s)| (*n, s.as_str())))
    {
        let (mut module, _) = parse(name, source);
        regia_render::specialize_handle_arguments(&mut module);
        for hazard in regia_render::image_parameter_hazards(&module) {
            problems.push(format!("{name}: {hazard}"));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Dynamic indexing (`v[i]` with a runtime `i`, read or write) of a vector value or a
/// function-local vector, in the naga IR. Uniform and storage vectors (an access chain into
/// the buffer, e.g. `engine_csm.split[i]`) are allowed: they ran clean on the Mali-G78.
fn dynamic_vector_indexing(module: &naga::Module, info: &naga::valid::ModuleInfo) -> Vec<String> {
    let is_vector = |inner: &naga::TypeInner| match inner {
        naga::TypeInner::Vector { .. } => true,
        naga::TypeInner::Pointer { base, space } => {
            *space == naga::AddressSpace::Function
                && matches!(module.types[*base].inner, naga::TypeInner::Vector { .. })
        }
        naga::TypeInner::ValuePointer { size, space, .. } => {
            size.is_some() && *space == naga::AddressSpace::Function
        }
        _ => false,
    };
    let mut found = Vec::new();
    let functions = module
        .functions
        .iter()
        .map(|(handle, f)| (f, &info[handle]))
        .chain(
            module
                .entry_points
                .iter()
                .enumerate()
                .map(|(i, ep)| (&ep.function, info.get_entry_point(i))),
        );
    for (function, finfo) in functions {
        for (_, expr) in function.expressions.iter() {
            if let naga::Expression::Access { base, .. } = *expr
                && is_vector(finfo[base].ty.inner_with(&module.types))
            {
                found.push(function.name.clone().unwrap_or_else(|| "?".to_owned()));
            }
        }
    }
    found.dedup();
    found
}

/// Mali-G78 (r54p3, Pixel 6) lost the device on most frames of the photoreal material path
/// while the splat weights were indexed as `wa[i]` / `wa[i] = w`; select-based lanes
/// (`lane`, `with_lane`) run clean (photoreal 19/19 three times on the phone).
#[test]
fn terrain_shaders_have_no_dynamic_vector_indexing() {
    let mut problems = Vec::new();
    let hw = regia_terrain::gpu::hw_height_source();
    let shared: Vec<_> = lights_modules().into_iter().chain(grass_modules()).collect();
    for (name, source) in SHADERS
        .into_iter()
        .chain([("terrain.wgsl (hw height)", hw.as_str())])
        .chain(shared.iter().map(|(n, s)| (*n, s.as_str())))
    {
        let (module, info) = parse(name, source);
        for function in dynamic_vector_indexing(&module, &info) {
            problems.push(format!("{name}: fn {function}"));
        }
    }
    assert!(
        problems.is_empty(),
        "dynamic vector index (use lane/with_lane):\n{}",
        problems.join("\n")
    );
}
