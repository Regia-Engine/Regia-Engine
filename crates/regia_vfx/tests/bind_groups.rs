// SPDX-License-Identifier: MPL-2.0
//! A warm VFX frame creates no bind groups or pipelines (specs/27 § 2.4).
//!
//! `VfxGpu::set_depth_view`, `set_material_views` and `set_haze` used to rebuild five bind
//! groups every frame through an uncounted device: the hitch counters could not see them.
//! The VFX context now creates through `regia_render::TrackedDevice` and rebuilds only when
//! a view changes.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::{
    DirectionalLightUniform, FogSettings, FramePathKind, FrameQuality, FrameScene, RenderDevice,
    RuntimeRenderer,
};
use regia_vfx::gpu::VfxGpu;
use regia_vfx::{VfxPlugin, VfxProfile, VfxRuntimePlugin, VfxScene};

// The receiver layer the decal material path samples (a filterable colour texture).
const RECEIVER_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn depth_view(device: &RenderDevice) -> wgpu::TextureView {
    view(device, wgpu::TextureFormat::Depth32Float)
}

fn view(device: &RenderDevice, format: wgpu::TextureFormat) -> wgpu::TextureView {
    device
        .device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("test depth"),
            size: wgpu::Extent3d {
                width: 16,
                height: 16,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn depth_view_rebinds_only_when_it_changes() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut gpu = VfxGpu::new(&device, VfxProfile::desktop_high()).expect("vfx gpu");
    let first = depth_view(&device);
    let second = depth_view(&device);
    let bind_groups = || regia_render::frame_path_counters().bind_groups;

    let before = bind_groups();
    gpu.set_depth_view(Some(&first));
    let created = bind_groups() - before;
    assert_eq!(
        created, 3,
        "a new depth view rebuilds sim, particle and decal groups (counted)"
    );

    let before = bind_groups();
    for _ in 0..10 {
        gpu.set_depth_view(Some(&first));
        gpu.set_material_views(None, None);
        gpu.set_haze(false);
    }
    assert_eq!(
        bind_groups() - before,
        0,
        "the same views every frame create nothing"
    );

    let before = bind_groups();
    gpu.set_depth_view(Some(&second));
    assert_eq!(
        bind_groups() - before,
        3,
        "a resized/new depth view still rebinds"
    );
    let before = bind_groups();
    gpu.set_material_views(Some(&view(&device, RECEIVER_FORMAT)), None);
    assert_eq!(
        bind_groups() - before,
        1,
        "a new receiver view rebuilds the decal group"
    );
}

#[test]
fn warm_runtime_frames_create_no_vfx_bind_groups() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.insert_resource(device.clone());
    app.init_resource::<RuntimeRenderer>();
    let mut lib = regia_vfx::scene::VfxLibrary::new();
    lib.add(regia_vfx::showcase::smoke()).expect("smoke");
    app.insert_resource(VfxScene::new(lib));
    app.add_plugins((VfxPlugin, VfxRuntimePlugin));
    let at = Vec3::new(0.0, 0.0, 0.0);
    app.world_mut()
        .resource_mut::<VfxScene>()
        .spawn("smoke", at, Vec3::Z, 7)
        .expect("spawn smoke");
    let eye = at + Vec3::new(0.0, 2.0, 12.0);
    let frame = |app: &mut App| {
        app.update();
        let scene = FrameScene {
            width: 64,
            height: 36,
            eye: eye.to_array(),
            target: (at + Vec3::Y * 2.0).to_array(),
            fov_y: 0.9,
            near: 0.1,
            far: 500.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 15.0,
            ambient_lux: 20_000.0,
            double_sided: false,
            post: None,
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 1.0, 1.0], 100_000.0),
            local_lights: &[],
            quality: FrameQuality::LAB,
            meshes: Vec::new(),
        };
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .render_offscreen(&device, &scene)
            .expect("frame");
    };
    for _ in 0..8 {
        frame(&mut app);
    }
    regia_render::trace_frame_path(true);
    let before = regia_render::frame_path_counters();
    for _ in 0..20 {
        frame(&mut app);
    }
    let delta = regia_render::frame_path_counters().since(before);
    let named: Vec<String> = regia_render::take_frame_path_events()
        .into_iter()
        .filter(|event| matches!(event.kind, FramePathKind::BindGroup | FramePathKind::Pipeline))
        .map(|event| event.label)
        .collect();
    regia_render::trace_frame_path(false);
    assert!(
        app.world().resource::<VfxScene>().estimated_live() > 0,
        "the smoke must be live for the frames to exercise the VFX layer"
    );
    assert_eq!(
        (delta.bind_groups, delta.pipelines),
        (0, 0),
        "warm frames created bind groups/pipelines: {named:?}"
    );
}

/// The soft-particle layer is ready before anything soft lives: the frames with no soft
/// particle build its targets, bind group, params buffer and composite pipeline, so the first
/// dust or smoke spawn mid-play creates nothing (the rc1 soak counted `vfx soft composite`
/// at frame 506, the first spawn).
#[test]
fn the_first_soft_spawn_creates_nothing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    const HDR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
    let device = device();
    let mut gpu = VfxGpu::new(&device, VfxProfile::desktop_high()).expect("vfx gpu");
    let scene_depth = depth_view(&device);
    let attachment_depth = depth_view(&device);
    let colour = view(&device, HDR);
    let camera = regia_vfx::gpu::VfxCamera {
        eye: Vec3::new(0.0, 2.0, 12.0),
        target: Vec3::ZERO,
        fov_y: 0.9,
        near: 0.1,
        far: 500.0,
    };
    let frame = |gpu: &mut VfxGpu| {
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("test") });
        gpu.set_depth_view(Some(&scene_depth));
        gpu.set_soft_pass(true);
        gpu.set_frame(&camera, 1.0 / 60.0, 16, 16);
        gpu.simulate(&mut encoder);
        gpu.record_soft_pass(&mut encoder, 16, 16).expect("soft layer");
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("test transparent"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &colour,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &attachment_depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            gpu.draw_soft_composite(&mut pass, HDR).expect("soft composite");
        }
        device.queue.submit([encoder.finish()]);
    };
    gpu.set_soft_live(false);
    assert!(!gpu.soft_pass_active());
    for _ in 0..3 {
        frame(&mut gpu);
    }
    regia_render::trace_frame_path(true);
    let before = regia_render::frame_path_counters();
    gpu.set_soft_live(true);
    assert!(
        gpu.soft_pass_active(),
        "desktop high draws soft particles at half resolution"
    );
    for _ in 0..3 {
        frame(&mut gpu);
    }
    let delta = regia_render::frame_path_counters().since(before);
    let named: Vec<String> = regia_render::take_frame_path_events()
        .into_iter()
        .map(|event| format!("{:?} {}", event.kind, event.label))
        .collect();
    regia_render::trace_frame_path(false);
    assert_eq!(
        (delta.pipelines, delta.bind_groups, delta.buffers, delta.textures),
        (0, 0, 0, 0),
        "the first soft frames created: {named:?}"
    );
}
