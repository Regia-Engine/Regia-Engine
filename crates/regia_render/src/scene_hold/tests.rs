// SPDX-License-Identifier: MPL-2.0
//! Scene hold (ADR-7890): lavapipe, fixed frame counts, byte-exact readbacks; no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use super::*;
use crate::overlay::{OverlayDraw, WindowOverlay};
use crate::runtime::{ExtractedCamera, ExtractedTarget, ExtractedView};
use crate::{ExtractedFrame, RuntimeRenderer};

const SIZE: (u32, u32) = (64, 64);
/// The UI patch: the top-left `UI_PX` × `UI_PX` pixels.
const UI_PX: u32 = 16;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// A UI layer that fills the top-left patch with a solid colour (the blend constant), so a test
/// can change "the UI" per frame without touching the rest of the image.
struct UiPatch {
    color: Arc<Mutex<[f64; 4]>>,
    pipeline: Option<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
}

impl OverlayDraw for UiPatch {
    fn draw(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        _width: u32,
        _height: u32,
    ) {
        if self.pipeline.as_ref().is_none_or(|(f, _)| *f != format) {
            let module = device.device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("ui patch"),
                source: wgpu::ShaderSource::Wgsl(
                    "@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
                         let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
                         return vec4<f32>(uv * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
                     }
                     @fragment fn fs() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }"
                        .into(),
                ),
            });
            let blend = wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Constant,
                dst_factor: wgpu::BlendFactor::Zero,
                operation: wgpu::BlendOperation::Add,
            };
            let pipeline = device
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("ui patch"),
                    layout: None,
                    vertex: wgpu::VertexState {
                        module: &module,
                        entry_point: Some("vs"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &module,
                        entry_point: Some("fs"),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format,
                            blend: Some(wgpu::BlendState {
                                color: blend,
                                alpha: blend,
                            }),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    multiview_mask: None,
                    cache: None,
                });
            self.pipeline = Some((format, pipeline));
        }
        let Some((_, pipeline)) = self.pipeline.as_ref() else {
            return;
        };
        let [r, g, b, a] = *self.color.lock().unwrap();
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ui patch"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(pipeline);
        pass.set_blend_constant(wgpu::Color { r, g, b, a });
        pass.set_scissor_rect(0, 0, UI_PX, UI_PX);
        pass.draw(0..3, 0..1);
    }
}

fn ui() -> (WindowOverlay, Arc<Mutex<[f64; 4]>>) {
    let color = Arc::new(Mutex::new([0.0, 0.0, 1.0, 1.0]));
    let mut overlay = WindowOverlay::default();
    overlay.set(Box::new(UiPatch {
        color: Arc::clone(&color),
        pipeline: None,
    }));
    (overlay, color)
}

/// A quad lit by the sun (the surface tests' offscreen scene).
fn quad_scene() -> ExtractedFrame {
    let corners = [
        [-1.0, 0.0, -1.0],
        [1.0, 0.0, -1.0],
        [1.0, 0.0, 1.0],
        [-1.0, 0.0, 1.0],
    ];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let mesh = crate::CpuMesh {
        vertices: corners
            .into_iter()
            .zip(uvs)
            .map(|(position, uv)| crate::lab_frame::LabVertex {
                position,
                normal: [0.0, 1.0, 0.0],
                uv,
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect(),
        indices: vec![0, 2, 1, 0, 3, 2],
    };
    let mut frame = ExtractedFrame::default();
    frame.views.push(ExtractedView {
        camera: ExtractedCamera {
            eye: [0.0, 3.0, 4.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 1.0,
            near: 0.1,
            far: 30.0,
        },
        viewport: [0.0, 0.0, 1.0, 1.0],
        order: 0,
        clear: true,
        target: ExtractedTarget::Window,
        layer_mask: u32::MAX,
        post: crate::PostSettings {
            enabled: false,
            auto_exposure: false,
            exposure_ev: 14.0,
            ..crate::PostSettings::default()
        },
    });
    frame.meshes.push(crate::runtime::ExtractedMesh {
        is_static: false,
        entity: Entity::PLACEHOLDER,
        mesh: Arc::new(mesh),
        material: Arc::new(crate::CpuMaterial::default()),
        layer: 0,
        model: regia_core::math::Mat4::IDENTITY.to_cols_array_2d(),
        joints: Vec::new(),
        decal_layer: 0,
        uv1: None,
        shadow: crate::ShadowCast::All,
        draw_indices: None,
        lod_key: 0,
        object_key_xor: 0,
        gi_proxy: true,
        lod_dither: 0.0,
    });
    frame.directional = Some(crate::DirectionalLightUniform::new(
        [0.3, -0.9, 0.2],
        [1.0, 1.0, 1.0],
        100_000.0,
    ));
    frame.directional_shadows = false;
    frame.environment = crate::runtime::Environment::default();
    frame
}

/// A window-like target: a render attachment that can be read (`COPY_SRC`) but not written by a
/// copy (a swap-chain image is rarely a copy destination), so the held frame takes the blit.
fn target(device: &RenderDevice, (w, h): (u32, u32)) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("scene hold test target"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

fn read(device: &RenderDevice, texture: &wgpu::Texture) -> Vec<u8> {
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("scene hold test read"),
        });
    let pending = crate::screenshot::encode_copy(device, &mut encoder, texture, FORMAT);
    device.queue.submit([encoder.finish()]);
    crate::screenshot::finish(device, pending).expect("readback").rgba
}

/// Pixels outside the UI patch, in order.
fn scene_region(rgba: &[u8], width: u32) -> Vec<u8> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            let (x, y) = (*i as u32 % width, *i as u32 / width);
            x >= UI_PX || y >= UI_PX
        })
        .flat_map(|(_, p)| *p)
        .collect()
}

/// The UI patch's first pixel.
fn ui_pixel(rgba: &[u8]) -> [u8; 4] {
    [rgba[0], rgba[1], rgba[2], rgba[3]]
}

struct Run {
    device: RenderDevice,
    renderer: RuntimeRenderer,
    scene: ExtractedFrame,
    overlay: WindowOverlay,
    color: Arc<Mutex<[f64; 4]>>,
    retained: RetainedScene,
    hold: SceneHold,
    /// The host's scene prepare holds the scene back (ADR-7883).
    prepare: bool,
    target: (wgpu::Texture, wgpu::TextureView),
}

impl Run {
    fn new() -> Self {
        let device = crate::test_device();
        let target = target(&device, SIZE);
        let (overlay, color) = ui();
        Self {
            device,
            renderer: RuntimeRenderer::default(),
            scene: quad_scene(),
            overlay,
            color,
            retained: RetainedScene::default(),
            hold: SceneHold::default(),
            prepare: false,
            target,
        }
    }

    fn frame(&mut self) -> SceneHoldFrame {
        render_views_held(
            &self.device,
            &mut self.renderer.0,
            &self.scene,
            (&self.target.0, &self.target.1),
            Some(&self.overlay),
            &mut self.retained,
            (&self.hold, self.prepare),
        )
        .expect("frame")
    }

    fn read(&self) -> Vec<u8> {
        read(&self.device, &self.target.0)
    }

    fn ui_color(&self, color: [f64; 4]) {
        *self.color.lock().unwrap() = color;
    }
}

/// Frames before the hold turns on.
const N: usize = 3;
/// Held frames.
const HELD: usize = 10;

fn ui_color(k: usize) -> [f64; 4] {
    [(k as f64 + 1.0) / 16.0, 1.0 - k as f64 / 16.0, 0.25, 1.0]
}

#[test]
fn held_frames_keep_the_scene_byte_for_byte_and_redraw_only_the_ui() {
    let mut run = Run::new();
    for _ in 0..N {
        assert_eq!(run.frame().step, HoldStep::Full);
    }
    run.hold.set(true);
    run.ui_color(ui_color(0));
    let captured = run.frame();
    assert_eq!(
        captured.step,
        HoldStep::Capture,
        "the first frame of a hold renders the scene"
    );
    assert_eq!(captured.scene_views, 1);
    let base = run.read();
    let base_scene = scene_region(&base, SIZE.0);
    assert!(base_scene.iter().any(|&c| c > 10), "the scene drew something");
    let index = run.renderer.0.frame_index();
    let mut previous_ui = ui_pixel(&base);
    for k in 1..=HELD {
        run.ui_color(ui_color(k));
        let frame = run.frame();
        assert_eq!(frame.step, HoldStep::Held, "held frame {k}");
        assert!(frame.held);
        assert_eq!(frame.scene_views, 0, "held frame {k} rendered a scene view");
        assert_eq!(
            run.renderer.0.frame_index(),
            index,
            "held frame {k} ran the renderer (a scene pass)"
        );
        let image = run.read();
        assert!(
            scene_region(&image, SIZE.0) == base_scene,
            "held frame {k}: the scene region differs from the captured frame"
        );
        let ui = ui_pixel(&image);
        assert_ne!(ui, previous_ui, "held frame {k}: the UI change is not visible");
        previous_ui = ui;
    }
    assert_eq!(run.retained.counts().held_frames, HELD as u64);
    assert_eq!(run.retained.counts().captures, 1);
}

#[test]
fn a_held_frames_gpu_passes_are_the_composite_and_the_ui_only() {
    let mut run = Run::new();
    let full = run.frame();
    assert!(!full.held);
    let names = |list: Vec<(&'static str, f32)>| list.into_iter().map(|(n, _)| n).collect::<Vec<_>>();
    let scene_passes = names(gpu_pass_report(&run.renderer.0, &full));
    assert!(
        scene_passes.contains(&"forward"),
        "a scene frame lists the scene passes: {scene_passes:?}"
    );
    run.hold.set(true);
    run.frame();
    let mut last = SceneHoldFrame::default();
    for _ in 0..HELD {
        last = run.frame();
        // The readback waits for the GPU, so the next frame finds the previous frame's stamps.
        let _ = run.read();
        assert_eq!(
            names(gpu_pass_report(&run.renderer.0, &last)),
            ["scene hold composite", "ui"],
            "a held frame reports no scene pass"
        );
    }
    if run.device.capabilities.timestamp_query {
        assert!(
            last.timed,
            "held frames are timed on a device with timestamp queries"
        );
        assert!(last.gpu_ms.is_finite() && last.gpu_ms >= 0.0);
        assert!((last.gpu_ms - (last.composite_ms + last.ui_ms)).abs() < 1e-6);
    }
}

#[test]
fn a_resize_while_held_renders_one_scene_frame_then_holds_again() {
    let mut run = Run::new();
    run.hold.set(true);
    assert_eq!(run.frame().step, HoldStep::Capture);
    assert_eq!(run.frame().step, HoldStep::Held);
    // The window is resized: a new draw image of another size.
    run.target = target(&run.device, (48, 40));
    let index = run.renderer.0.frame_index();
    let resized = run.frame();
    assert_eq!(
        resized.step,
        HoldStep::Capture,
        "a resize renders one scene frame"
    );
    assert_eq!(resized.scene_views, 1);
    assert_eq!(run.renderer.0.frame_index(), index + 1);
    let after = run.frame();
    assert_eq!(after.step, HoldStep::Held, "then holds again");
    assert_eq!(run.renderer.0.frame_index(), index + 1);
    assert_eq!(
        run.retained.kept_key(),
        Some(HoldKey {
            width: 48,
            height: 40,
            format: FORMAT
        })
    );
    // A new draw format (an HDR toggle switches to the display stage's intermediate) is the same.
    let hdr = HoldKey {
        width: 48,
        height: 40,
        format: wgpu::TextureFormat::Rgba16Float,
    };
    assert_eq!(run.retained.plan(&run.hold, Some(hdr)), HoldStep::Capture);
    // A host refresh renders one scene frame too.
    let mut run = Run::new();
    run.hold.set(true);
    run.frame();
    assert_eq!(run.frame().step, HoldStep::Held);
    run.hold.refresh();
    assert_eq!(run.frame().step, HoldStep::Capture);
    assert_eq!(run.frame().step, HoldStep::Held);
}

#[test]
fn turning_the_hold_off_renders_the_frame_a_never_held_run_renders() {
    // Held run: N full frames, one capture, HELD held frames, then the hold is off.
    let mut held = Run::new();
    for _ in 0..N {
        held.frame();
    }
    held.hold.set(true);
    held.frame();
    for _ in 0..HELD {
        assert!(held.frame().held);
    }
    held.hold.set(false);
    let index = held.renderer.0.frame_index();
    let resumed = held.frame();
    assert_eq!(resumed.step, HoldStep::Full);
    assert_eq!(
        resumed.scene_views, 1,
        "the frame after the hold renders the scene"
    );
    assert_eq!(held.renderer.0.frame_index(), index + 1);
    assert_eq!(held.retained.kept_key(), None, "the kept image is freed");
    let resumed_image = held.read();
    // Control: the same renderer frames, never held (compared at the same renderer frame index).
    let mut control = Run::new();
    for _ in 0..=N + 1 {
        control.frame();
    }
    assert_eq!(control.renderer.0.frame_index(), held.renderer.0.frame_index());
    assert!(
        control.read() == resumed_image,
        "the first frame after the hold differs from a never-held run at the same frame index"
    );
}

/// ADR-10105, the pause-freeze bar: with the hold following the gameplay pause, a paused game
/// shows the same scene pixels for 300 frames (no renderer frame runs, so no jitter, noise or
/// history moves), the overlay still redraws, and unpausing renders the frame a never-paused run
/// renders at the same renderer frame index.
#[test]
fn a_hold_that_follows_the_pause_keeps_the_paused_world_still_for_300_frames() {
    const PAUSED: usize = 300;
    let mut run = Run::new();
    run.hold.set_follow_pause(true);
    for _ in 0..N {
        assert_eq!(run.frame().step, HoldStep::Full, "not paused: full frames");
    }
    // What `follow_pause` does when the game sets `Time::paused`.
    run.hold.note_paused(true);
    assert!(run.hold.is_on());
    run.ui_color(ui_color(0));
    assert_eq!(run.frame().step, HoldStep::Capture);
    let paused_scene = scene_region(&run.read(), SIZE.0);
    let index = run.renderer.0.frame_index();
    let mut previous_ui = ui_pixel(&run.read());
    for k in 1..=PAUSED {
        // The overlay changes every frame (the pause screen animates); the world must not.
        run.ui_color(ui_color(k % 15));
        let frame = run.frame();
        assert_eq!(frame.step, HoldStep::Held, "paused frame {k}");
        assert_eq!(frame.scene_views, 0, "paused frame {k} rendered a scene view");
        let image = run.read();
        assert!(
            scene_region(&image, SIZE.0) == paused_scene,
            "paused frame {k}: the world pixels moved"
        );
        let ui = ui_pixel(&image);
        assert_ne!(ui, previous_ui, "paused frame {k}: the overlay did not redraw");
        previous_ui = ui;
    }
    assert_eq!(
        run.renderer.0.frame_index(),
        index,
        "no renderer frame ran while paused"
    );
    // Unpause: full frames again, continuing where the renderer stopped.
    run.hold.note_paused(false);
    assert!(!run.hold.is_on());
    assert_eq!(run.frame().step, HoldStep::Full);
    let resumed = run.read();
    // The control draws the overlay the paused run left on (the pause screen's last color), so the
    // whole image compares, the UI corner included.
    let mut control = Run::new();
    control.ui_color(ui_color(PAUSED % 15));
    for _ in 0..=N + 1 {
        control.frame();
    }
    assert_eq!(control.renderer.0.frame_index(), run.renderer.0.frame_index());
    assert!(
        control.read() == resumed,
        "the first frame after the pause differs from a never-paused run at the same frame index"
    );
}

/// The `follow_pause` system copies `Time::paused`; a hold that does not follow the pause ignores
/// it, and `set(false)` does not end a hold that the pause keeps on.
#[test]
fn follow_pause_tracks_time_paused_only_when_asked() {
    use regia_ecs::ecs::system::RunSystemOnce;
    let mut world = World::new();
    world.insert_resource(Time::default());
    world.insert_resource(SceneHold::default());
    world.resource_mut::<Time>().paused = true;
    world.run_system_once(follow_pause).expect("system runs");
    assert!(world.resource::<SceneHold>().paused());
    assert!(
        !world.resource::<SceneHold>().is_on(),
        "not following: the pause alone does nothing"
    );
    world.resource_mut::<SceneHold>().set_follow_pause(true);
    assert!(
        world.resource::<SceneHold>().is_on(),
        "following: on while paused"
    );
    world.resource_mut::<SceneHold>().set(false);
    assert!(world.resource::<SceneHold>().is_on(), "the pause keeps it on");
    world.resource_mut::<Time>().paused = false;
    world.run_system_once(follow_pause).expect("system runs");
    assert!(!world.resource::<SceneHold>().is_on(), "unpaused: off");
}

#[test]
fn a_target_that_cannot_be_held_renders_full_frames() {
    let mut retained = RetainedScene::default();
    let mut hold = SceneHold::default();
    hold.set(true);
    assert_eq!(retained.plan(&hold, None), HoldStep::Full);
    assert!(!retained.is_holding());
    assert!(SceneHoldFrame::default().gpu_passes().is_empty());
    assert_eq!(HoldStep::Held.name(), "held");
    assert!(!HoldStep::Held.renders_scene());
}

/// Integration of ADR-7883 and ADR-7890 (the window draw filter): a host that opens a scene prepare
/// while the game is paused and the scene is held gets the prepare's behavior (no scene, the kept
/// image freed, the UI over a clear), and the hold's behavior again once the prepare closes (one
/// capture, then held frames that run no scene pass).
#[test]
fn a_paused_hold_during_an_open_prepare_draws_no_scene_then_holds_again() {
    let mut run = Run::new();
    run.hold.set(true);
    assert_eq!(run.frame().step, HoldStep::Capture);
    assert_eq!(run.frame().step, HoldStep::Held);
    let held_scene = scene_region(&run.read(), SIZE.0);
    assert!(
        held_scene.iter().any(|&c| c > 10),
        "the held scene drew something"
    );
    let index = run.renderer.0.frame_index();

    // The host opens a scene prepare while paused: the scene is dropped, held image and all.
    run.prepare = true;
    let mut previous_ui = ui_pixel(&run.read());
    for k in 0..3 {
        run.ui_color(ui_color(k));
        let frame = run.frame();
        assert_eq!(
            frame.step,
            HoldStep::Full,
            "prepare frame {k}: the hold does not apply"
        );
        assert!(!frame.held, "prepare frame {k} composited the kept scene");
        assert_eq!(frame.scene_views, 0, "prepare frame {k} rendered a scene view");
        assert_eq!(
            run.renderer.0.frame_index(),
            index,
            "prepare frame {k} ran the renderer (a scene pass)"
        );
        assert_eq!(
            run.retained.kept_key(),
            None,
            "prepare frame {k} kept a scene image"
        );
        let image = run.read();
        assert!(
            scene_region(&image, SIZE.0) != held_scene,
            "prepare frame {k} still shows the held scene"
        );
        let ui = ui_pixel(&image);
        assert_ne!(ui, previous_ui, "prepare frame {k}: the UI change is not visible");
        previous_ui = ui;
    }

    // The prepare closes, still paused: one capture renders the scene, then it is held again.
    run.prepare = false;
    let captured = run.frame();
    assert_eq!(captured.step, HoldStep::Capture);
    assert_eq!(captured.scene_views, 1);
    assert_eq!(run.renderer.0.frame_index(), index + 1);
    let held = run.frame();
    assert_eq!(held.step, HoldStep::Held);
    assert_eq!(held.scene_views, 0);
    assert_eq!(
        run.renderer.0.frame_index(),
        index + 1,
        "a held frame ran the renderer"
    );
    assert!(
        scene_region(&run.read(), SIZE.0) == held_scene,
        "the scene held after the prepare differs from the one held before it"
    );
}
