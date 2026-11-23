// SPDX-License-Identifier: MPL-2.0
//! A host pass at a frame-graph slot (ADR-1552, `book/agents/render-graph.md`).
//!
//! `BorderPass` is an `overlay` pass: one full-screen triangle with its own WGSL shader that
//! darkens a band along the image's border, blended over the finished frame. The example renders a
//! small lit scene offscreen with the pass registered, writes the frame to a PNG, and prints the
//! frame graph the way `render.graph` reports it.
//!
//! It then switches on both halves of two-phase occlusion, which are optional built-in nodes of
//! the same graph (ADR-4905, ADR-5178): `prepass_occlusion` and `shadow_occlusion` join the graph
//! without any change to the renderer's frame code. The next frame is GPU-driven (`gpu_driven` on
//! in the lab quality), so on a device that draws GPU objects the graph's dispatch records both
//! nodes, the shadow cull runs, and the prepass cull reaches its record function (two quads are
//! below its Hi-Z threshold, so it reports `few_instances`). On a device without GPU objects both
//! nodes must be listed as inactive instead (ADR-5626). The host pass still records at its slot.
//! The example exits 1 if a node is missing from the graph, the dispatch counted a violation, a
//! node's dispatch state or cull report does not match the device, or the host pass failed.
//!
//! `cargo run -p regia_render --example custom_pass [OUT.png]` (default: `custom_pass.png` in the
//! system temp directory). Prints `skip: <reason>` and exits 0 without a GPU adapter.

use std::process::ExitCode;

use regia_render::{
    DirectionalLightUniform, FramePass, FramePassContext, FrameQuality, FrameRenderer, FrameStep, GraphSlot,
    IndirectStrategy, LabMaterial, LabMesh, LabScene, LabVertex, PrepassOcclusionSkip, RenderDevice,
    slot_name,
};

const SHADER: &str = r"
struct Out { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: Out;
    out.pos = vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    // Distance to the nearest edge in uv; the band is the outer 8 %.
    let edge = min(min(in.uv.x, 1.0 - in.uv.x), min(in.uv.y, 1.0 - in.uv.y));
    let shade = 1.0 - smoothstep(0.0, 0.08, edge);
    return vec4<f32>(0.0, 0.0, 0.0, 0.8 * shade);
}
";

/// Darkens the border of the output image. Builds its pipeline on first use per target format,
/// through the frame's tracked device, so the hitch counters see the create.
#[derive(Default)]
struct BorderPass {
    pipeline: Option<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
}

impl BorderPass {
    fn pipeline(&mut self, ctx: &FramePassContext<'_>) -> &wgpu::RenderPipeline {
        if self
            .pipeline
            .as_ref()
            .is_none_or(|(format, _)| *format != ctx.color_format)
        {
            let module = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("border pass"),
                source: wgpu::ShaderSource::Wgsl(SHADER.into()),
            });
            let pipeline = ctx
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("border pass"),
                    layout: None,
                    vertex: wgpu::VertexState {
                        module: &module,
                        entry_point: Some("vs"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        buffers: &[],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &module,
                        entry_point: Some("fs"),
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: ctx.color_format,
                            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                });
            self.pipeline = Some((ctx.color_format, pipeline));
        }
        match &self.pipeline {
            Some((_, pipeline)) => pipeline,
            None => unreachable!("built above"),
        }
    }
}

impl FramePass for BorderPass {
    fn name(&self) -> &str {
        "border"
    }

    fn slot(&self) -> GraphSlot {
        GraphSlot::Overlay
    }

    fn reads(&self) -> &[&'static str] {
        &["output"]
    }

    fn writes(&self) -> &[&'static str] {
        &["output"]
    }

    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        let pipeline = self.pipeline(ctx).clone();
        let [x, y, w, h] = ctx.viewport;
        let mut pass = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("border pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: ctx.color,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_viewport(x, y, w, h, 0.0, 1.0);
        pass.set_pipeline(&pipeline);
        pass.draw(0..3, 0..1);
        Ok(())
    }
}

fn scene() -> LabScene {
    let quad = |x: f32, z: f32| LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x - 0.5, -0.5, z], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.5, -0.5, z], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.5, 0.5, z], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - 0.5, 0.5, z], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    };
    LabScene {
        width: 320,
        height: 200,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.6, 0.5, 1.0],
            ..LabMaterial::default()
        }],
        meshes: vec![quad(-0.6, 0.0), quad(0.6, -0.5)],
    }
}

fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| format!("png header: {e}"))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| format!("png data: {e}"))
}

fn main() -> ExitCode {
    let out = std::env::args_os().nth(1).map_or_else(
        || std::env::temp_dir().join("custom_pass.png"),
        std::path::PathBuf::from,
    );
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            println!("skip: {err}");
            return ExitCode::SUCCESS;
        }
    };
    let lab = scene();
    let mut renderer = FrameRenderer::new();
    if let Err(err) = renderer.register_pass(Box::new(BorderPass::default())) {
        eprintln!("register: {err} ({})", err.code());
        return ExitCode::FAILURE;
    }
    let frame_scene = lab.as_frame_scene();
    let frame = match renderer.render_offscreen(&device, &frame_scene) {
        Ok(frame) => frame,
        Err(err) => {
            eprintln!("render: {err}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(err) = write_png(&out, frame.width, frame.height, &frame.pixels) {
        eprintln!("{err}");
        return ExitCode::FAILURE;
    }
    println!("wrote {}", out.display());
    let report = renderer.graph_report();
    for step in report.steps {
        match step {
            FrameStep::Builtin { name, module, .. } => println!("pass  {name} (frame::{module})"),
            FrameStep::Slot(slot) => {
                let passes: Vec<&str> = report
                    .extensions
                    .iter()
                    .filter(|p| p.slot == *slot)
                    .map(|p| p.name.as_str())
                    .collect();
                println!("slot  {} {passes:?}", slot_name(*slot));
            }
        }
    }
    for pass in &report.extensions {
        println!(
            "host pass {}: runs {}, failures {}, last cpu {:.3} ms",
            pass.name, pass.runs, pass.failures, pass.last_cpu_ms
        );
    }

    // Two-phase occlusion as graph passes: both phase-1 culls are optional built-in nodes. They
    // cull GPU-driven draws only, so this frame turns `gpu_driven` on.
    renderer.set_two_phase_occlusion(true);
    renderer.set_shadow_occlusion(true);
    // Two quads: let the shadow cull run on them (the default threshold is 512 casters).
    renderer.set_shadow_occlusion_min_casters(Some(1));
    let gpu_scene = lab.as_frame_scene_with_quality(FrameQuality {
        gpu_driven: true,
        ..FrameQuality::LAB
    });
    if let Err(err) = renderer.render_offscreen(&device, &gpu_scene) {
        eprintln!("render: {err}");
        return ExitCode::FAILURE;
    }
    let report = renderer.graph_report();
    println!("graph order: {}", report.graph.order.join(" > "));
    let dispatch = &report.dispatch;
    println!(
        "dispatch: frames {}, recorded {:?}, inactive {:?}, violations {}",
        dispatch.frames, dispatch.recorded, dispatch.inactive, dispatch.violations
    );
    println!(
        "prepass occlusion skipped: {:?}; shadow occlusion active: {}",
        renderer.prepass_occlusion_stats().skipped.map(|s| s.name()),
        renderer.shadow_occlusion_stats().active
    );
    let mut ok = dispatch.violations == 0 && dispatch.skipped.is_empty();
    for node in ["prepass_occlusion", "shadow_occlusion"] {
        if !report.graph.order.iter().any(|n| n == node) {
            eprintln!("{node} is not in the graph");
            ok = false;
        }
    }
    // Each node records only when its phase 1 can cull: on a device that draws GPU objects.
    let gpu_objects = IndirectStrategy::select(&device.capabilities).uses_gpu_objects();
    for node in ["prepass_occlusion", "shadow_occlusion"] {
        let (recorded, inactive) = (
            dispatch.recorded.contains(&node),
            dispatch.inactive.contains(&node),
        );
        if (recorded, inactive) != (gpu_objects, !gpu_objects) {
            eprintln!(
                "{node}: recorded {recorded}, inactive {inactive}, device draws GPU objects {gpu_objects}"
            );
            ok = false;
        }
    }
    let prepass = renderer.prepass_occlusion_stats();
    let shadow = renderer.shadow_occlusion_stats();
    if gpu_objects {
        // The shadow cull ran; the prepass cull reached its own threshold check.
        if !shadow.active {
            eprintln!(
                "the shadow cull did not run: {:?}",
                shadow.skipped.map(|s| s.name())
            );
            ok = false;
        }
        if !prepass.active && prepass.skipped != Some(PrepassOcclusionSkip::FewInstances) {
            eprintln!(
                "the prepass cull did not run: {:?}",
                prepass.skipped.map(|s| s.name())
            );
            ok = false;
        }
    } else {
        println!("note: the device draws no GPU objects, so neither cull can run");
    }
    for pass in &report.extensions {
        if pass.runs != 2 || pass.failures != 0 {
            eprintln!(
                "host pass {}: runs {}, failures {}",
                pass.name, pass.runs, pass.failures
            );
            ok = false;
        }
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
