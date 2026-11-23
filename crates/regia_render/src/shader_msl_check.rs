// SPDX-License-Identifier: MPL-2.0
//! Metal shader translation check that runs on any host (ADR-2644).
//!
//! wgpu's Metal backend turns each naga module into Metal Shading Language with naga's MSL
//! writer, one entry point per pipeline stage, and hands the text to the Metal compiler. A
//! Linux host cannot run the compiler, so [`msl_module_problems`] runs the half it can: the
//! writer, with the options wgpu-hal 30's Metal backend passes (`metal/device.rs`
//! `load_shader` and `create_pipeline_layout`):
//!
//! - MSL [`MSL_LANG_VERSION`] (what wgpu picks on macOS 26, the test Mac's system);
//! - `fake_missing_bindings` off, so a resource without a slot is an error rather than invalid
//!   MSL;
//! - restricted index, buffer and image-load bounds checks (wgpu's default runtime checks), loop
//!   bounding, zero-initialized workgroup memory, integer-division checks;
//! - a slot per resource the entry point uses, counted per kind (buffers, textures, samplers)
//!   in group then binding order, a binding array taking one argument-buffer slot, then the
//!   sizes buffer after the buffers when a storage buffer is bound or the stage is a vertex
//!   stage;
//! - vertex pulling on, the vertex inputs in one interleaved buffer at slot 30 (wgpu counts vertex
//!   buffers down from Metal's 31 buffer slots).
//!
//! An entry point over Metal's per-stage argument limits (31 buffers, 128 textures, 16
//! samplers) is an error too. Passing proves naga can express the module in MSL within those
//! limits. It does not prove the Metal compiler accepts the text: with [`MSL_DUMP_DIR_ENV`] set
//! the text of every entry point is written there with a `manifest.txt`, and a Mac compiles it
//! with `xcrun -sdk macosx metal -c` (`docs/06-testing-and-ci.md` § shader gate).

use std::collections::BTreeMap;

use naga::back::msl;

/// The MSL version the check writes: wgpu-hal picks 4.0 on macOS 26 and later.
pub const MSL_LANG_VERSION: (u8, u8) = (4, 0);

/// Metal's buffer argument slots per stage.
pub const METAL_MAX_BUFFERS: u32 = 31;

/// Metal's texture argument slots per stage (Apple GPUs and Mac2).
pub const METAL_MAX_TEXTURES: u32 = 128;

/// Metal's sampler argument slots per stage.
pub const METAL_MAX_SAMPLERS: u32 = 16;

/// Environment variable naming a directory the check writes every entry point's MSL into
/// (`<name>__<entry>.metal`, plus `manifest.txt` lines `file|entry|stage`), for `xcrun metal`.
pub const MSL_DUMP_DIR_ENV: &str = "REGIA_MSL_DUMP_DIR";

/// What naga's MSL writer rejects in `module` (named `name` in each message): validation and
/// writer errors and entry points over Metal's argument limits. Empty when the module passes.
#[must_use]
pub fn msl_module_problems(name: &str, module: &naga::Module) -> Vec<String> {
    let info = match naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(module)
    {
        Ok(info) => info,
        Err(err) => return vec![format!("{name}: naga validation: {err}")],
    };
    let constants = naga::back::PipelineConstants::new();
    let (module, info) =
        match naga::back::pipeline_constants::process_overrides(module, &info, None, &constants) {
            Ok(pair) => pair,
            Err(err) => return vec![format!("{name}: pipeline overrides: {err}")],
        };
    match msl_entry_points(name, &module, &info) {
        Ok(_) => Vec::new(),
        Err(err) => vec![err],
    }
}

/// Parses each `(name, wgsl)` source and runs [`msl_module_problems`] on the module as
/// `ShaderSource::Wgsl` hands it to wgpu and, when that differs, as [`crate::wgsl_source`] hands it.
#[must_use]
pub fn wgsl_msl_problems(sources: &[(&str, &str)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, source) in sources {
        match naga::front::wgsl::parse_str(source) {
            Ok(mut module) => {
                problems.extend(msl_module_problems(name, &module));
                if crate::specialize_handle_arguments(&mut module) {
                    problems.extend(msl_module_problems(&format!("{name} (wgsl_source)"), &module));
                }
            }
            Err(err) => problems.push(format!("{name}: wgsl parse: {err}")),
        }
    }
    problems
}

/// Slot counters of one stage.
#[derive(Default)]
struct Counters {
    buffers: u32,
    textures: u32,
    samplers: u32,
}

/// The resources of entry point `index` as wgpu-hal's Metal pipeline layout assigns them, and the
/// storage-buffer binding-array lengths.
fn entry_resources(
    module: &naga::Module,
    info: &naga::valid::ModuleInfo,
    index: usize,
) -> (
    msl::EntryPointResources,
    naga::FastHashMap<naga::ResourceBinding, u32>,
    Counters,
) {
    let entry = &module.entry_points[index];
    let uses = info.get_entry_point(index);
    let mut bound: BTreeMap<naga::ResourceBinding, naga::Handle<naga::GlobalVariable>> = BTreeMap::new();
    for (handle, var) in module.global_variables.iter() {
        if let Some(binding) = var.binding
            && !uses[handle].is_empty()
        {
            bound.insert(binding, handle);
        }
    }
    let mut counters = Counters::default();
    let mut resources = msl::BindingMap::new();
    let mut lengths = naga::FastHashMap::default();
    let mut storage = false;
    for (binding, handle) in bound {
        let var = &module.global_variables[handle];
        let mut target = msl::BindTarget::default();
        let inner = &module.types[var.ty].inner;
        if let naga::TypeInner::BindingArray { size, .. } = inner {
            if matches!(var.space, naga::AddressSpace::Storage { .. }) {
                storage = true;
                let length = match size {
                    naga::ArraySize::Constant(n) => n.get(),
                    _ => crate::shader_check::MATRIX_TEXTURE_TABLE_SIZE,
                };
                lengths.insert(binding, length);
            }
            // wgpu's bindless path: one argument buffer.
            target.buffer = Some(slot(counters.buffers));
            counters.buffers += 1;
        } else {
            match (&var.space, inner) {
                (naga::AddressSpace::Uniform, _) => {
                    target.buffer = Some(slot(counters.buffers));
                    counters.buffers += 1;
                }
                (naga::AddressSpace::Storage { access }, _) => {
                    storage = true;
                    target.buffer = Some(slot(counters.buffers));
                    counters.buffers += 1;
                    target.mutable = access.contains(naga::StorageAccess::STORE);
                }
                (_, naga::TypeInner::Sampler { .. }) => {
                    target.sampler = Some(msl::BindSamplerTarget::Resource(slot(counters.samplers)));
                    counters.samplers += 1;
                }
                (_, naga::TypeInner::Image { class, .. }) => {
                    target.texture = Some(slot(counters.textures));
                    counters.textures += 1;
                    if let naga::ImageClass::Storage { access, .. } = class {
                        target.mutable = access.contains(naga::StorageAccess::STORE)
                            || access.contains(naga::StorageAccess::ATOMIC);
                    }
                }
                (_, naga::TypeInner::AccelerationStructure { .. }) => {
                    target.buffer = Some(slot(counters.buffers));
                    counters.buffers += 1;
                }
                _ => {}
            }
        }
        resources.insert(binding, target);
    }
    let sizes_buffer = if storage || entry.stage == naga::ShaderStage::Vertex {
        let at = counters.buffers;
        counters.buffers += 1;
        Some(slot(at))
    } else {
        None
    };
    (
        msl::EntryPointResources {
            resources,
            immediates_buffer: None,
            sizes_buffer,
        },
        lengths,
        counters,
    )
}

/// A Metal slot (saturating: the limit check reports anything past 255).
fn slot(index: u32) -> msl::Slot {
    msl::Slot::try_from(index).unwrap_or(msl::Slot::MAX)
}

/// The vertex buffer mapping of `entry`'s `@location` inputs: one interleaved buffer at slot
/// [`METAL_MAX_BUFFERS`]` - 1`, as wgpu maps vertex buffer 0.
fn vertex_mappings(
    module: &naga::Module,
    entry: &naga::EntryPoint,
) -> Result<Vec<msl::VertexBufferMapping>, String> {
    let mut inputs = Vec::new();
    for argument in &entry.function.arguments {
        match &argument.binding {
            Some(naga::Binding::Location { location, .. }) => inputs.push((*location, argument.ty)),
            Some(naga::Binding::BuiltIn(_)) => {}
            None => {
                if let naga::TypeInner::Struct { members, .. } = &module.types[argument.ty].inner {
                    for member in members {
                        if let Some(naga::Binding::Location { location, .. }) = member.binding {
                            inputs.push((location, member.ty));
                        }
                    }
                }
            }
        }
    }
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    inputs.sort_by_key(|(location, _)| *location);
    let mut offset = 0u32;
    let mut attributes = Vec::new();
    for (location, ty) in inputs {
        let format = crate::shader_check::vertex_format(module, ty)
            .ok_or_else(|| format!("@location({location}): no vertex format for its type"))?;
        attributes.push(msl::AttributeMapping {
            shader_location: location,
            offset,
            format,
        });
        offset += u32::try_from(format.size()).unwrap_or(u32::MAX);
    }
    Ok(vec![msl::VertexBufferMapping {
        id: METAL_MAX_BUFFERS - 1,
        stride: offset.max(4),
        step_mode: msl::VertexBufferStepMode::ByVertex,
        attributes,
    }])
}

/// Writes every entry point of `module` (validated, overrides substituted) as MSL with wgpu's
/// Metal options. Returns the number of entry points written, or the first error with the
/// module's `name`.
///
/// # Errors
///
/// The writer's error, a vertex input with no vertex format, or an entry point over Metal's
/// per-stage argument limits.
pub fn msl_entry_points(
    name: &str,
    module: &naga::Module,
    info: &naga::valid::ModuleInfo,
) -> Result<usize, String> {
    let restrict = naga::proc::BoundsCheckPolicy::Restrict;
    let mut written = 0usize;
    for (index, entry) in module.entry_points.iter().enumerate() {
        let (resources, lengths, counters) = entry_resources(module, info, index);
        let vertex_buffers = if entry.stage == naga::ShaderStage::Vertex {
            vertex_mappings(module, entry).map_err(|err| format!("{name} ({}): msl: {err}", entry.name))?
        } else {
            Vec::new()
        };
        let buffers = counters.buffers + u32::try_from(vertex_buffers.len()).unwrap_or(u32::MAX);
        let mut over = Vec::new();
        if buffers > METAL_MAX_BUFFERS {
            over.push(format!("{buffers} buffers (limit {METAL_MAX_BUFFERS})"));
        }
        if counters.textures > METAL_MAX_TEXTURES {
            over.push(format!(
                "{} textures (limit {METAL_MAX_TEXTURES})",
                counters.textures
            ));
        }
        if counters.samplers > METAL_MAX_SAMPLERS {
            over.push(format!(
                "{} samplers (limit {METAL_MAX_SAMPLERS})",
                counters.samplers
            ));
        }
        if !over.is_empty() {
            return Err(format!(
                "{name} ({}): over Metal's per-stage argument limits: {}",
                entry.name,
                over.join(", ")
            ));
        }
        let options = msl::Options {
            lang_version: MSL_LANG_VERSION,
            per_entry_point_map: msl::EntryPointResourceMap::from([(entry.name.clone(), resources)]),
            inline_samplers: Vec::new(),
            spirv_cross_compatibility: false,
            fake_missing_bindings: false,
            bounds_check_policies: naga::proc::BoundsCheckPolicies {
                index: restrict,
                buffer: restrict,
                image_load: restrict,
                binding_array: naga::proc::BoundsCheckPolicy::Unchecked,
            },
            zero_initialize_workgroup_memory: true,
            force_loop_bounding: true,
            task_dispatch_limits: None,
            mesh_shader_primitive_indices_clamp: true,
            emit_int_div_checks: true,
            ray_query_initialization_tracking: true,
        };
        let pipeline = msl::PipelineOptions {
            entry_point: Some((entry.stage, entry.name.clone())),
            allow_and_force_point_size: false,
            vertex_pulling_transform: true,
            vertex_buffer_mappings: vertex_buffers,
            binding_array_length_map: lengths,
        };
        let (text, _) = msl::write_string(module, info, &options, &pipeline)
            .map_err(|err| format!("{name} ({}): msl: {err}", entry.name))?;
        dump_msl(name, entry, &text);
        written += 1;
    }
    Ok(written)
}

/// With [`MSL_DUMP_DIR_ENV`] set, writes `text` as `<name>__<entry>.metal` there and appends
/// `file|entry|stage` to `manifest.txt` (one append per line, so parallel writers do not
/// interleave). Best effort: the check does not depend on it.
fn dump_msl(name: &str, entry: &naga::EntryPoint, text: &str) {
    use std::io::Write as _;
    let Some(dir) = std::env::var_os(MSL_DUMP_DIR_ENV) else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let safe: String = format!("{name}__{}", entry.name)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let file = format!("{safe}.metal");
    let tmp = dir.join(format!("{file}.tmp{}", std::process::id()));
    if std::fs::write(&tmp, text).is_err() || std::fs::rename(&tmp, dir.join(&file)).is_err() {
        return;
    }
    if let Ok(mut manifest) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("manifest.txt"))
    {
        let _ = manifest.write_all(format!("{file}|{}|{:?}\n", entry.name, entry.stage).as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> naga::Module {
        match naga::front::wgsl::parse_str(source) {
            Ok(module) => module,
            Err(err) => panic!("fixture does not parse: {err}"),
        }
    }

    #[test]
    fn a_vertex_fragment_and_compute_module_translates() {
        let module = parse(
            "struct Out { @builtin(position) p: vec4<f32>, @location(0) uv: vec2<f32> }\n\
             @group(0) @binding(0) var<uniform> tint: vec4<f32>;\n\
             @group(0) @binding(1) var<storage, read_write> counts: array<u32>;\n\
             @group(1) @binding(0) var color: texture_2d<f32>;\n\
             @group(1) @binding(1) var smp: sampler;\n\
             @vertex fn vs(@location(0) pos: vec3<f32>, @location(1) uv: vec2<f32>) -> Out {\n\
                 return Out(vec4<f32>(pos, 1.0), uv);\n\
             }\n\
             @fragment fn fs(in: Out) -> @location(0) vec4<f32> {\n\
                 return tint * textureSample(color, smp, in.uv);\n\
             }\n\
             @compute @workgroup_size(64) fn cs(@builtin(global_invocation_id) id: vec3<u32>) {\n\
                 counts[id.x] = counts[id.x] + 1u;\n\
             }\n",
        );
        assert_eq!(msl_module_problems("fixture", &module), Vec::<String>::new());
    }

    /// Seventeen samplers in one fragment stage cannot bind on Metal.
    #[test]
    fn an_entry_point_over_metals_sampler_limit_fails() {
        let mut source = String::from("@group(0) @binding(0) var color: texture_2d<f32>;\n");
        let mut sum = String::from("vec4<f32>(0.0)");
        for i in 0..17 {
            source.push_str(&format!("@group(1) @binding({i}) var s{i}: sampler;\n"));
            sum = format!("{sum} + textureSample(color, s{i}, uv)");
        }
        source.push_str(&format!(
            "@fragment fn fs(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {{ return {sum}; }}\n"
        ));
        let problems = msl_module_problems("many_samplers", &parse(&source));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("17 samplers (limit 16)"), "{problems:?}");
    }

    /// Every engine module the SPIR-V gate covers translates to MSL (on the base, nothing checked
    /// Metal translation before a Mac compiled the module).
    #[test]
    fn every_engine_shader_translates_to_msl() {
        let mut failures = Vec::new();
        let mut translated = 0usize;
        for (name, source, defs) in crate::shader_check::shader_sources() {
            let label = format!("{name} {:?}", crate::shader_check::define_key(&defs));
            match crate::shader_lib::compose_source(source, name, defs) {
                Ok(module) => {
                    let problems = msl_module_problems(&label, &module);
                    if problems.is_empty() {
                        translated += 1;
                    } else {
                        failures.extend(problems);
                    }
                }
                Err(err) => failures.push(format!("{label}: compose: {err}")),
            }
        }
        for extra in crate::shader_hlsl::extra_engine_modules() {
            let label = extra.label();
            match extra.module() {
                Ok(module) => {
                    let problems = msl_module_problems(&label, &module);
                    if problems.is_empty() {
                        translated += 1;
                    } else {
                        failures.extend(problems);
                    }
                }
                Err(err) => failures.push(format!("{label}: {err}")),
            }
        }
        assert!(translated > 50, "only {translated} modules translated");
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
