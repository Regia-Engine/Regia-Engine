// SPDX-License-Identifier: MPL-2.0
//! The first projected decal and the first glass instance (own canopy, crack damage) of a warm
//! map compile nothing on the frame path: the engine's frame warm
//! (`FrameRenderer::warm_extracted`, what `GameFrameWarm` runs) builds the decal receiver when
//! translucent layers are installed and the `GLASS` sets when a transmissive standard material
//! is in the frame; a host renderer gets the same from `prewarm_decals` and
//! `prewarm_glass_instances`. Its own test binary: the pipeline counter is per process, so the
//! tests take turns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use regia_render::pipeline_cache::pipelines_created;
use regia_render::pipeline_warm::canonical_frame;
use regia_render::runtime::ExtractedMesh;
use regia_render::{
    ExtractedFrame, FrameRenderer, GlassInstance, LabMaterial, LayerCamera, PipelineWarmQueue, RenderDevice,
    TranslucentLayer,
};

/// One test at a time (the pipeline counter is process-wide).
static SERIAL: Mutex<()> = Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

/// An effect layer that draws nothing but reports decals once `decals` is set: the frame
/// then stamps the decal receiver.
struct DecalLayer {
    decals: Arc<AtomicBool>,
}

impl TranslucentLayer for DecalLayer {
    fn simulate(
        &self,
        _depth: &wgpu::TextureView,
        _camera: &LayerCamera,
        _encoder: &mut wgpu::CommandEncoder,
    ) {
    }

    fn draw(&self, _pass: &mut wgpu::RenderPass<'_>, _format: wgpu::TextureFormat) {}

    fn has_decals(&self) -> bool {
        self.decals.load(Ordering::Relaxed)
    }
}

/// The canonical frame plus a glass pane (transmissive standard material) above the quad.
fn frame_with_pane() -> (ExtractedFrame, u64) {
    let mut frame = canonical_frame();
    let base = frame.meshes[0].clone();
    let entity = regia_ecs::prelude::Entity::from_raw_u32(7).expect("entity index");
    let pane = ExtractedMesh {
        entity,
        material: Arc::new(LabMaterial {
            base_color: [0.8, 0.9, 1.0, 0.2],
            transmission: 1.0,
            roughness: 0.05,
            ..LabMaterial::default()
        }),
        model: regia_core::math::Mat4::from_translation(regia_core::math::Vec3::new(0.0, 0.5, 0.0))
            .to_cols_array_2d(),
        ..base
    };
    frame.meshes.push(pane);
    (frame, entity.to_bits())
}

fn canopy(object_key: u64) -> GlassInstance {
    GlassInstance {
        object_key,
        inside: true,
        specular_scale: 0.15,
        damage: None,
    }
}

/// Renders the frame's first view once offscreen (the window path's shape at a real size).
fn draw(device: &RenderDevice, renderer: &mut FrameRenderer, frame: &ExtractedFrame) {
    let (view_index, _) = frame.window_views()[0];
    let scene = frame.frame_scene(view_index, 160, 96);
    renderer.render_offscreen(device, &scene).expect("frame");
}

/// A renderer with the decal layer installed, warmed on `frame` without decals or glass.
fn warmed(device: &RenderDevice, frame: &ExtractedFrame) -> (FrameRenderer, Arc<AtomicBool>) {
    let decals = Arc::new(AtomicBool::new(false));
    let mut renderer = FrameRenderer::new();
    renderer.set_translucent_layers(vec![Arc::new(DecalLayer {
        decals: Arc::clone(&decals),
    })]);
    renderer
        .warm_extracted(device, frame, wgpu::TextureFormat::Rgba8Unorm, true)
        .expect("warm");
    // The offscreen frame at its real size once (targets; the warm built the pipelines).
    draw(device, &mut renderer, frame);
    (renderer, decals)
}

/// Frames with a decal and a canopy after the warm: how many pipelines they created.
fn effect_frames(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    decals: &AtomicBool,
    frame: &ExtractedFrame,
    key: u64,
) -> u64 {
    let before = pipelines_created();
    decals.store(true, Ordering::Relaxed);
    renderer.set_glass(vec![canopy(key)]);
    for _ in 0..3 {
        draw(device, renderer, frame);
    }
    pipelines_created() - before
}

#[test]
fn the_engine_frame_warm_covers_the_first_decal_and_glass_instance() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (frame, key) = frame_with_pane();
    let (mut renderer, decals) = warmed(&device, &frame);
    let created = effect_frames(&device, &mut renderer, &decals, &frame, key);
    println!("decal + canopy frames after the engine warm: {created} pipeline creates");
    assert_eq!(
        created,
        0,
        "the first decal / glass instance compiled on the frame path; outside sites: {:?}",
        PipelineWarmQueue::default().progress().outside_sites
    );
}

#[test]
fn without_the_effects_warm_the_first_decal_and_canopy_compile() {
    // The control: a frame without a transmissive mesh does not warm glass, and a renderer
    // whose layers came after its warm has no decal receiver. Both then compile on the frame
    // that first draws them, which is what the warm above removes.
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (with_pane, key) = frame_with_pane();
    let plain = canonical_frame();
    let mut renderer = FrameRenderer::new();
    renderer
        .warm_extracted(&device, &plain, wgpu::TextureFormat::Rgba8Unorm, true)
        .expect("warm");
    draw(&device, &mut renderer, &with_pane);
    let decals = Arc::new(AtomicBool::new(false));
    renderer.set_translucent_layers(vec![Arc::new(DecalLayer {
        decals: Arc::clone(&decals),
    })]);
    draw(&device, &mut renderer, &with_pane);
    let cold = effect_frames(&device, &mut renderer, &decals, &with_pane, key);
    assert!(
        cold > 0,
        "the unwarmed decal receiver and glass sets compile on the frame"
    );

    // The host-side calls give the same renderer what the engine warm would have.
    let mut host = FrameRenderer::new();
    host.warm_extracted(&device, &plain, wgpu::TextureFormat::Rgba8Unorm, true)
        .expect("warm");
    draw(&device, &mut host, &with_pane);
    let decals = Arc::new(AtomicBool::new(false));
    host.set_translucent_layers(vec![Arc::new(DecalLayer {
        decals: Arc::clone(&decals),
    })]);
    draw(&device, &mut host, &with_pane);
    host.prewarm_decals(&device).expect("decal prewarm");
    let targets = host.prewarm_glass_instances(&device).expect("glass prewarm");
    assert!(targets >= 1, "at least the frame's forward target");
    let warm = effect_frames(&device, &mut host, &decals, &with_pane, key);
    println!(
        "decal + canopy frames: {cold} pipeline creates cold, {warm} after prewarm_decals + prewarm_glass_instances"
    );
    assert_eq!(
        warm,
        0,
        "prewarm_decals / prewarm_glass_instances left a compile; outside sites: {:?}",
        PipelineWarmQueue::default().progress().outside_sites
    );
}
