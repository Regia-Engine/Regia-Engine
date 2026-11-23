// SPDX-License-Identifier: MPL-2.0
//! Regression guard for the DO27 tester item decal-copy-src (v0.5.1 telemetry-truth lane, ADR-7898).
//!
//! At v0.1.21 the TSR output texture had no `COPY_SRC`, so the decal scene-color copy was skipped
//! and logged "decal scene-color copy skipped" once per renderer. v0.2.8 fixed it (ceaec5022,
//! ADR-1863); `every_temporal_upscaler_gives_the_pass_a_copyable_image` covers the post stack's
//! copy but nothing covered the decal path. This test passes on the base it was written against:
//! it guards the fix.
//!
//! Oracle: a translucent layer that draws in the HDR pass and has decals, at DesktopHigh with
//! render scale 870 (the tester's TSR 0.87), receives a scene-color view in
//! `bind_decal_targets` on every frame, for each temporal upscaler, and the skip warning never
//! fires.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    LayerCamera, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, TemporalUpscaler,
    TranslucentLayer,
};
use tracing_subscriber::layer::SubscriberExt;

const FRAMES: u32 = 4;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A layer standing in for the VFX runtime's decal layer: counts the scene-color views it is given.
#[derive(Default)]
struct DecalStub {
    with_scene: AtomicU32,
    without_scene: AtomicU32,
}

impl TranslucentLayer for DecalStub {
    fn simulate(
        &self,
        _depth: &wgpu::TextureView,
        _camera: &LayerCamera,
        _encoder: &mut wgpu::CommandEncoder,
    ) {
    }

    fn draw(&self, _pass: &mut wgpu::RenderPass<'_>, _format: wgpu::TextureFormat) {}

    fn participates_in_hdr(&self) -> bool {
        true
    }

    fn has_decals(&self) -> bool {
        true
    }

    fn bind_decal_targets(&self, _receiver: Option<&wgpu::TextureView>, scene: Option<&wgpu::TextureView>) {
        let counter = if scene.is_some() {
            &self.with_scene
        } else {
            &self.without_scene
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Counts `regia_render` warnings that report a skipped decal scene-color copy.
#[derive(Clone, Default)]
struct SkipWarnings(Arc<Mutex<Vec<String>>>);

struct Message(String);

impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SkipWarnings {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        if message.0.contains("decal scene-color copy skipped") {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(message.0);
        }
    }
}

fn wall() -> LabMesh {
    let corners = [
        ([-1.5, -1.5, 0.0], [0.0, 1.0]),
        ([1.5, -1.5, 0.0], [1.0, 1.0]),
        ([1.5, 1.5, 0.0], [1.0, 0.0]),
        ([-1.5, 1.5, 0.0], [0.0, 0.0]),
    ];
    LabMesh {
        vertices: corners
            .iter()
            .map(|&(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

fn scene() -> LabScene {
    LabScene {
        width: 160,
        height: 96,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            taa: true,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-0.3, -0.5, -0.8], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![wall()],
    }
}

#[test]
fn every_temporal_upscaler_gives_decals_a_scene_copy_every_frame() {
    let device = device();
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = true;
    quality.render_scale_permille = 870;
    let lab = scene();
    let warnings = SkipWarnings::default();
    let subscriber = tracing_subscriber::registry().with(warnings.clone());
    tracing::subscriber::with_default(subscriber, || {
        for upscaler in [
            TemporalUpscaler::Taau,
            TemporalUpscaler::Tsr,
            TemporalUpscaler::Fsr3,
        ] {
            let layer = Arc::new(DecalStub::default());
            let mut renderer = FrameRenderer::new();
            renderer.set_temporal_upscaler(upscaler);
            renderer.set_translucent_layers(vec![Arc::clone(&layer) as Arc<dyn TranslucentLayer>]);
            for _ in 0..FRAMES {
                let frame = lab.as_frame_scene_with_quality(quality);
                renderer.render_offscreen(&device, &frame).expect("frame");
            }
            assert_eq!(
                layer.without_scene.load(Ordering::Relaxed),
                0,
                "{upscaler:?}: a frame bound no scene-color copy for the decals"
            );
            assert_eq!(
                layer.with_scene.load(Ordering::Relaxed),
                FRAMES,
                "{upscaler:?}: the decal targets were not bound once per frame"
            );
        }
    });
    let skipped = warnings
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(skipped.is_empty(), "the copy was skipped: {skipped:?}");
}
