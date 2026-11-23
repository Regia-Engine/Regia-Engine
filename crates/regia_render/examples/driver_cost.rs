// SPDX-License-Identifier: MPL-2.0
//! Driver side of a pipeline's cost: builds `lab_forward.wgsl`'s forward pipeline
//! (`vs_main` + one fragment entry point, the CPU path's vertex layout, an automatic layout)
//! once per variant of the shader and prints how long `create_render_pipeline` took.
//! Run it with the driver's own shader cache off to see the cold compile
//! (`__GL_SHADER_DISK_CACHE=0` on NVIDIA, `MESA_SHADER_CACHE_DISABLE=1` on Mesa).
//!
//! ```text
//! driver_cost [--defs A,B] [--entry fs_main] [--rounds N] [--checks all|no-loop-bounding|no-int-div|no-bounds|loops+div|none] [--passthrough | --dont-unroll] [VARIANTS]
//! ```
//!
//! `--passthrough` writes the SPIR-V here (wgpu-hal's Vulkan writer options) and hands it to
//! `create_shader_module_passthrough` with an explicit layout; `--dont-unroll` also marks every
//! loop `DontUnroll` (SPIR-V loop control), which WGSL cannot express. The time then includes
//! the SPIR-V writing.
//!
//! With `--rounds N` every variant is built N times, round-robin, and the median and minimum
//! are printed (a driver's compile is CPU work: a loaded host spreads single runs widely).
//!
//! `VARIANTS` is a text file, one variant per line: `name|find|replace` edits the embedded
//! `lab_forward.wgsl` (several `|find|replace` pairs may follow one name; every `find` must
//! occur in the source; `\\n` is a newline). Without it, only the shader as shipped (`base`) is built. `base` is
//! built first and again last, so a driver that caches across pipelines in one process shows.

use std::collections::HashMap;
use std::time::Instant;

use naga_oil::compose::ShaderDefValue;
use regia_render::RenderDevice;

const FORWARD: &str = include_str!("../shaders/lab_forward.wgsl");

/// The CPU path's vertex stream (`VertexInput` in `lab_forward.wgsl`).
const ATTRIBUTES: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
    0 => Float32x3, 1 => Float32x3, 2 => Float32x2, 3 => Float32x4,
    4 => Float32x4, 5 => Uint32x4, 6 => Float32x4
];

/// The runtime checks naga adds to the SPIR-V, by `--checks` name.
fn runtime_checks(name: &str) -> Result<wgpu::ShaderRuntimeChecks, String> {
    let mut checks = wgpu::ShaderRuntimeChecks::checked();
    match name {
        "all" => {}
        "no-loop-bounding" => checks.force_loop_bounding = false,
        "no-int-div" => checks.int_div_checks = false,
        "no-bounds" => checks.bounds_checks = false,
        "loops+div" => {
            checks.force_loop_bounding = false;
            checks.int_div_checks = false;
        }
        // SAFETY: only constructs the value; the timing harness never draws with the pipeline.
        #[allow(unsafe_code)]
        "none" => checks = unsafe { wgpu::ShaderRuntimeChecks::all(false) },
        other => return Err(format!("unknown --checks {other}")),
    }
    Ok(checks)
}

/// How the harness hands the shader to wgpu.
#[derive(Clone, Copy)]
enum Mode {
    /// naga IR through wgpu (the engine's path), with these runtime checks.
    Naga(wgpu::ShaderRuntimeChecks),
    /// SPIR-V written here with wgpu-hal's Vulkan writer options, through
    /// `create_shader_module_passthrough`; `dont_unroll` marks every loop `DontUnroll`.
    Passthrough { dont_unroll: bool },
}

/// One stage's SPIR-V the way wgpu-hal's Vulkan backend writes it (an RTX-class Vulkan 1.3+
/// device with robust buffer and image access), optionally with every `OpLoopMerge` hinted
/// `DontUnroll`.
fn stage_spirv(
    module: &naga::Module,
    info: &naga::valid::ModuleInfo,
    stage: naga::ShaderStage,
    entry: &str,
    dont_unroll: bool,
) -> Result<Vec<u32>, String> {
    use naga::back::spv;
    let (module, info) = naga::back::pipeline_constants::process_overrides(
        module,
        info,
        Some((stage, entry)),
        &naga::back::PipelineConstants::default(),
    )
    .map_err(|e| e.to_string())?;
    let options = spv::Options {
        lang_version: (1, 6),
        flags: spv::WriterFlags::LABEL_VARYINGS | spv::WriterFlags::FORCE_POINT_SIZE,
        bounds_check_policies: naga::proc::BoundsCheckPolicies {
            index: naga::proc::BoundsCheckPolicy::Restrict,
            buffer: naga::proc::BoundsCheckPolicy::Unchecked,
            image_load: naga::proc::BoundsCheckPolicy::Unchecked,
            binding_array: naga::proc::BoundsCheckPolicy::Unchecked,
        },
        ..spv::Options::default()
    };
    let pipeline = spv::PipelineOptions {
        shader_stage: stage,
        entry_point: entry.to_string(),
    };
    let mut words = spv::write_vec(&module, &info, &options, Some(&pipeline)).map_err(|e| e.to_string())?;
    if dont_unroll {
        // OpLoopMerge (246): merge block, continue target, loop control. DontUnroll = 0x2.
        let mut at = 5;
        while at < words.len() {
            let count = (words[at] >> 16) as usize;
            if words[at] & 0xffff == 246 && count >= 4 {
                words[at + 3] = 0x2;
            }
            at += count.max(1);
        }
    }
    Ok(words)
}

/// The pipeline layout of every bound global of `module` (passthrough modules have no
/// reflection, so the layout is explicit).
fn layout_of(d: &wgpu::Device, module: &naga::Module) -> Result<wgpu::PipelineLayout, String> {
    use naga::{AddressSpace, ImageClass, ImageDimension, ScalarKind, TypeInner};
    let mut groups: Vec<Vec<wgpu::BindGroupLayoutEntry>> = Vec::new();
    for (_, var) in module.global_variables.iter() {
        let Some(binding) = &var.binding else {
            continue;
        };
        let ty = match var.space {
            AddressSpace::Uniform => wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            AddressSpace::Storage { access } => wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage {
                    read_only: !access.contains(naga::StorageAccess::STORE),
                },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            AddressSpace::Handle => match module.types[var.ty].inner {
                TypeInner::Sampler { comparison } => wgpu::BindingType::Sampler(if comparison {
                    wgpu::SamplerBindingType::Comparison
                } else {
                    wgpu::SamplerBindingType::NonFiltering
                }),
                TypeInner::Image { dim, arrayed, class } => {
                    let view_dimension = match (dim, arrayed) {
                        (ImageDimension::D1, _) => wgpu::TextureViewDimension::D1,
                        (ImageDimension::D2, false) => wgpu::TextureViewDimension::D2,
                        (ImageDimension::D2, true) => wgpu::TextureViewDimension::D2Array,
                        (ImageDimension::D3, _) => wgpu::TextureViewDimension::D3,
                        (ImageDimension::Cube, false) => wgpu::TextureViewDimension::Cube,
                        (ImageDimension::Cube, true) => wgpu::TextureViewDimension::CubeArray,
                    };
                    let (sample_type, multisampled) = match class {
                        ImageClass::Sampled { kind, multi } => (
                            match kind {
                                ScalarKind::Sint => wgpu::TextureSampleType::Sint,
                                ScalarKind::Uint => wgpu::TextureSampleType::Uint,
                                _ => wgpu::TextureSampleType::Float { filterable: false },
                            },
                            multi,
                        ),
                        ImageClass::Depth { multi } => (wgpu::TextureSampleType::Depth, multi),
                        _ => return Err(format!("binding {binding:?}: storage or external image")),
                    };
                    wgpu::BindingType::Texture {
                        sample_type,
                        view_dimension,
                        multisampled,
                    }
                }
                ref other => return Err(format!("binding {binding:?}: {other:?}")),
            },
            other => return Err(format!("binding {binding:?} in {other:?}")),
        };
        let group = binding.group as usize;
        if groups.len() <= group {
            groups.resize_with(group + 1, Vec::new);
        }
        groups[group].push(wgpu::BindGroupLayoutEntry {
            binding: binding.binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty,
            count: None,
        });
    }
    let layouts: Vec<wgpu::BindGroupLayout> = groups
        .iter()
        .map(|entries| {
            d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("driver_cost"),
                entries,
            })
        })
        .collect();
    let refs: Vec<Option<&wgpu::BindGroupLayout>> = layouts.iter().map(Some).collect();
    Ok(d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("driver_cost"),
        bind_group_layouts: &refs,
        immediate_size: 0,
    }))
}

#[allow(unsafe_code)]
fn build(d: &wgpu::Device, source: &str, defs: &[String], entry: &str, mode: Mode) -> Result<f64, String> {
    let shader_defs: HashMap<String, ShaderDefValue> = defs
        .iter()
        .map(|d| (d.clone(), ShaderDefValue::Bool(true)))
        .collect();
    let module =
        regia_render::compose_source(source, "lab_forward.wgsl", shader_defs).map_err(|e| e.to_string())?;
    let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
    let (vertex_module, fragment_module, layout, naga_ms) = match mode {
        Mode::Naga(checks) => {
            // SAFETY: the pipeline is only compiled, never drawn with, so a shader without some
            // of naga's runtime checks never runs.
            let module = unsafe {
                d.create_shader_module_trusted(
                    wgpu::ShaderModuleDescriptor {
                        label: Some("driver_cost"),
                        source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
                    },
                    checks,
                )
            };
            (module.clone(), module, None, 0.0)
        }
        Mode::Passthrough { dont_unroll } => {
            let start = Instant::now();
            let info = naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .map_err(|e| format!("{e:?}"))?;
            let mut modules = Vec::new();
            for (stage, name) in [
                (naga::ShaderStage::Vertex, "vs_main"),
                (naga::ShaderStage::Fragment, entry),
            ] {
                let words = stage_spirv(&module, &info, stage, name, dont_unroll)?;
                let entry_points = [wgpu::PassthroughShaderEntryPoint {
                    name: name.into(),
                    workgroup_size: (0, 0, 0),
                }];
                // SAFETY: naga wrote and validated this SPIR-V; the pipeline is never drawn with.
                modules.push(unsafe {
                    d.create_shader_module_passthrough(wgpu::ShaderModuleDescriptorPassthrough {
                        label: Some("driver_cost"),
                        entry_points: std::borrow::Cow::Borrowed(&entry_points),
                        spirv: Some(std::borrow::Cow::Owned(words)),
                        ..Default::default()
                    })
                });
            }
            let naga_ms = start.elapsed().as_secs_f64() * 1e3;
            let fragment = modules.pop().ok_or("no fragment module")?;
            let vertex = modules.pop().ok_or("no vertex module")?;
            (vertex, fragment, Some(layout_of(d, &module)?), naga_ms)
        }
    };
    let stride = ATTRIBUTES
        .iter()
        .map(|a| a.offset + a.format.size())
        .max()
        .unwrap_or(0);
    let start = Instant::now();
    let _pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("driver_cost"),
        layout: layout.as_ref(),
        vertex: wgpu::VertexState {
            module: &vertex_module,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: stride,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &ATTRIBUTES,
            })],
        },
        fragment: Some(wgpu::FragmentState {
            module: &fragment_module,
            entry_point: Some(entry),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba16Float,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    let ms = start.elapsed().as_secs_f64() * 1e3 + naga_ms;
    match regia_core::tasks::block_on(scope.pop()) {
        None => Ok(ms),
        Some(e) => Err(format!("validation: {e}")),
    }
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut defs = vec!["ALPHA_TEST".to_string()];
    let mut entry = "fs_main".to_string();
    let mut rounds = 1usize;
    let mut checks = wgpu::ShaderRuntimeChecks::checked();
    let mut passthrough = None;
    let mut file = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--defs" => {
                defs = args
                    .get(i + 1)
                    .map(|s| {
                        s.split(',')
                            .filter(|d| !d.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                i += 1;
            }
            "--checks" => {
                checks = runtime_checks(args.get(i + 1).map_or("all", String::as_str))?;
                i += 1;
            }
            "--passthrough" => passthrough = Some(false),
            "--dont-unroll" => passthrough = Some(true),
            "--rounds" => {
                rounds = args.get(i + 1).and_then(|r| r.parse().ok()).unwrap_or(1).max(1);
                i += 1;
            }
            "--entry" => {
                entry = args.get(i + 1).cloned().unwrap_or(entry);
                i += 1;
            }
            path => file = Some(path.to_string()),
        }
        i += 1;
    }
    let mut variants = vec![("base".to_string(), FORWARD.to_string())];
    if let Some(path) = file {
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
        for line in text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        {
            let mut parts = line.split('|');
            let name = parts.next().unwrap_or("?").to_string();
            let mut source = FORWARD.to_string();
            let rest: Vec<&str> = parts.collect();
            for pair in rest.chunks(2) {
                let [find, replace] = pair else {
                    return Err(format!("{name}: find without replace"));
                };
                let (find, replace) = (find.replace("\\n", "\n"), replace.replace("\\n", "\n"));
                if !source.contains(&find) {
                    return Err(format!("{name}: `{find}` not in lab_forward.wgsl"));
                }
                source = source.replace(&find, &replace);
            }
            variants.push((name, source));
        }
    }
    variants.push(("base again".to_string(), FORWARD.to_string()));
    let device = RenderDevice::headless().map_err(|e| e.to_string())?;
    let mode = passthrough.map_or(Mode::Naga(checks), |dont_unroll| Mode::Passthrough {
        dont_unroll,
    });
    // Passthrough needs its own device with `PASSTHROUGH_SHADERS`.
    let d = if passthrough.is_some() {
        let (d, _queue) =
            regia_core::tasks::block_on(device.adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("driver_cost passthrough"),
                required_features: device.device.features() | wgpu::Features::PASSTHROUGH_SHADERS,
                required_limits: device.device.limits(),
                ..Default::default()
            }))
            .map_err(|e| e.to_string())?;
        d
    } else {
        device.device.clone()
    };
    println!(
        "adapter {} ({}), defs {defs:?}, entry {entry}",
        device.adapter.get_info().name,
        device.adapter.get_info().driver_info
    );
    let mut times: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
    for round in 0..rounds {
        for (k, (name, source)) in variants.iter().enumerate() {
            match build(&d, source, &defs, &entry, mode) {
                Ok(ms) => {
                    println!("round {round} {ms:>8.1} ms  {name}");
                    times[k].push(ms);
                }
                Err(e) => println!("DRIVER_COST failed  {name}: {e}"),
            }
        }
    }
    for ((name, _), t) in variants.iter().zip(&mut times) {
        if t.is_empty() {
            continue;
        }
        t.sort_by(f64::total_cmp);
        println!(
            "DRIVER_COST median {:>8.1} ms  min {:>8.1} ms  {name}",
            t[t.len() / 2],
            t[0]
        );
    }
    Ok(())
}
