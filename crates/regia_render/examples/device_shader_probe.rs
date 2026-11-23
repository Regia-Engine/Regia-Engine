// SPDX-License-Identifier: MPL-2.0
//! On-device shader probe: compiles the engine's shaders into real pipelines on the
//! device it runs on, one at a time, printing each name *before* the driver sees it.
//!
//! A driver whose shader compiler crashes (Mali's did on texture function parameters,
//! ADR-124) takes the process down inside `vkCreate*Pipelines`; the last printed
//! line names the culprit. Built for Android as a plain executable and run from
//! `adb shell` (no APK needed):
//!
//! ```text
//! cargo ndk -t arm64-v8a build --release -p regia_render --example device_shader_probe
//! adb push target/aarch64-linux-android/release/examples/device_shader_probe /data/local/tmp/
//! adb shell /data/local/tmp/device_shader_probe            # engine frames (Lab, Mobile Low/High)
//! adb shell /data/local/tmp/device_shader_probe a.wgsl b.wgsl  # stand-alone files
//! ```
//!
//! A stand-alone file needs `vs_main` and `fs_main` (no vertex inputs, one colour
//! output) and gets an automatic layout; `cs_main` makes it a compute pipeline.

use std::io::Write;

use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, FrameTarget,
    LabMaterial, LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast,
};

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn probe_file(device: &RenderDevice, path: &str) -> Result<(), String> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let scope = device.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let module = device.device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(path),
        source: wgpu::ShaderSource::Wgsl(source.as_str().into()),
    });
    say(&format!("{path}: pipeline ..."));
    if source.contains("fn cs_main") {
        let _ = device
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(path),
                layout: None,
                module: &module,
                entry_point: Some("cs_main"),
                compilation_options: Default::default(),
                cache: None,
            });
    } else {
        let _ = device
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(path),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
    }
    match regia_core::tasks::block_on(scope.pop()) {
        Some(err) => Err(format!("{path}: {err}")),
        None => Ok(()),
    }
}

fn probe_engine(device: &RenderDevice) -> Result<(), String> {
    let material = LabMaterial {
        base_color: [0.7, 0.7, 0.7, 1.0],
        roughness: 0.6,
        ..LabMaterial::EMPTY
    };
    let vertices = [
        LabVertex::from_pos_normal_uv([-1.0, 0.0, -1.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([1.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([1.0, 0.0, 1.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-1.0, 0.0, 1.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
    ];
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe target"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let settings = PostSettings {
        auto_exposure: true,
        bloom: true,
        bloom_mips: 3,
        taa: true,
        ..PostSettings::default()
    };
    let mut renderer = FrameRenderer::new();
    for (name, quality) in [
        ("lab", FrameQuality::LAB),
        (
            "mobile_low",
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileLow)),
        ),
        (
            "mobile_high",
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::MobileHigh)),
        ),
    ] {
        say(&format!("engine frame {name} ..."));
        let scene = FrameScene {
            width: 64,
            height: 64,
            eye: [0.0, 2.0, 4.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 1.0,
            near: 0.1,
            far: 30.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 8_000.0,
            double_sided: false,
            post: Some(&settings),
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.0, -1.0, -0.4], [1.0; 3], 100_000.0),
            local_lights: &[],
            quality,
            meshes: vec![FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &vertices,
                indices: &[0, 2, 1, 0, 3, 2],
                material: &material,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }],
        };
        for _ in 0..2 {
            renderer
                .render_into(device, &scene, FrameTarget::full(&view, format, 64, 64))
                .map_err(|e| format!("{name}: {e}"))?;
        }
        device.wait_idle().map_err(|e| e.to_string())?;
        say(&format!("engine frame {name}: ok"));
    }
    Ok(())
}

fn main() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            say(&format!("no device: {err}"));
            std::process::exit(2);
        }
    };
    let info = device.adapter.get_info();
    say(&format!(
        "adapter: {} ({:?}, driver {} {})",
        info.name, info.backend, info.driver, info.driver_info
    ));
    let files: Vec<String> = std::env::args().skip(1).collect();
    let mut failed = false;
    if files.is_empty()
        && let Err(err) = probe_engine(&device)
    {
        say(&format!("FAIL {err}"));
        failed = true;
    }
    for path in &files {
        match probe_file(&device, path) {
            Ok(()) => say(&format!("{path}: ok")),
            Err(err) => {
                say(&format!("FAIL {err}"));
                failed = true;
            }
        }
    }
    std::process::exit(i32::from(failed));
}
