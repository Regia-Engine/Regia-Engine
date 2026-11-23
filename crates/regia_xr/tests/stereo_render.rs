// SPDX-License-Identifier: MPL-2.0
//! The XR frame through the renderer (spec 58 §§ 2.6, 8.12-13, ADR-1918): the fake runtime scripts
//! poses and views, the eyes draw on any headless adapter (lavapipe is the reference). No golden
//! image: every check compares frames of this test with each other.

use std::sync::Mutex;

use regia_app::prelude::*;
use regia_core::glam::{Quat, UVec2, Vec3};
use regia_platform::xr::{Eye, Fov, LayerSpace, Pose, SimEvent, XrRuntime};
use regia_render::{
    DirectionalLightUniform, ExtractedFrame, EyeCamera, FrameQuality, LabMaterial, LabMesh, LabScene,
    LabVertex, MeshRenderer, OverlayDraw, RenderDevice, RenderPlugin, RuntimeRenderer, spawn_lab_scene,
};
use regia_scene::Transform;
use regia_xr::perf;
use regia_xr::render::multiview::OBJECT_SLOT;
use regia_xr::render::{
    EYE_COMPOSITE_PASS, MirrorPainter, MirrorSource, PanelImage, SharedMirror, StereoForward,
};
use regia_xr::sim::{ScriptPose, ScriptState, SimTick, XrSimScript};
use regia_xr::stereo::{eye_camera, union_cull_matrix};
use regia_xr::{
    Mirror, OptionValue, SimEyeTargets, StereoMode, XrConfig, XrPanel, XrPanels, XrPlugin, XrRenderPlugin,
    XrRenderProfile, XrRenderState, XrRig, XrSession, XrState, drive_session_state, last_views, set_option,
};

static SERIAL_GPU: Mutex<()> = Mutex::new(());

const EYE_W: u32 = 96;
const EYE_H: u32 = 80;

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn quad(center: [f32; 3], half: f32) -> LabMesh {
    let [x, y, z] = center;
    LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x - half, y - half, z], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + half, y - half, z], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + half, y + half, z], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - half, y + half, z], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

fn lab(meshes: Vec<LabMesh>) -> LabScene {
    LabScene {
        width: EYE_W,
        height: EYE_H,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.0,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 20_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.8, 0.8, 0.8, 1.0],
            ..LabMaterial::default()
        }],
        meshes,
    }
}

/// A wall behind everything, so a pixel is never the empty clear colour.
fn wall() -> LabMesh {
    quad([0.0, 0.0, -30.0], 60.0)
}

struct Rig {
    app: App,
    device: RenderDevice,
}

fn rig(scene: &LabScene) -> Rig {
    rig_with(scene, XrPlugin::simulator())
}

fn rig_with(scene: &LabScene, plugin: XrPlugin) -> Rig {
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    let mut app = App::new();
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(regia_app::MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    // One fixed step per update: no check depends on how fast the machine runs.
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / 60.0),
    ));
    app.init_resource::<regia_input::InputInjection>();
    app.insert_resource(device.clone());
    app.add_plugins(RenderPlugin { headless: true });
    app.init_resource::<RuntimeRenderer>();
    app.add_plugins(plugin);
    app.add_plugins(XrRenderPlugin);
    app.world_mut()
        .resource_mut::<XrRenderProfile>()
        .recommended_extent = UVec2::new(EYE_W, EYE_H);
    app.world_mut().resource_mut::<XrRenderProfile>().per_eye_history = false;
    app.world_mut().spawn((XrRig, Transform::default()));
    spawn_lab_scene(app.world_mut(), scene);
    Rig { app, device }
}

impl Rig {
    fn focus(&mut self) {
        drive_session_state(self.app.world_mut(), ScriptState::Focused).expect("focus");
    }

    fn step(&mut self, frames: u32) {
        for _ in 0..frames {
            self.app.update();
        }
    }

    fn simulate(&mut self, event: SimEvent) {
        self.app
            .world_mut()
            .resource_mut::<XrRuntime>()
            .backend_mut()
            .simulate(event)
            .expect("simulate");
    }

    fn eye(&self, eye: Eye) -> Vec<u8> {
        self.app
            .world()
            .resource::<SimEyeTargets>()
            .read_rgba8(eye)
            .expect("read eye")
    }

    fn display(&self, eye: Eye) -> Vec<u8> {
        let render = self.app.world().resource::<XrRenderState>();
        let texture = render.display_texture(eye).expect("display image");
        regia_xr::eye_targets::read_rgba8(&self.device, texture, UVec2::new(EYE_W, EYE_H))
            .expect("read display")
    }

    fn stats(&self) -> regia_xr::XrRenderStats {
        self.app.world().resource::<XrRenderState>().stats()
    }
}

fn at(x: u32, y: u32) -> usize {
    ((y * EYE_W + x) * 4) as usize
}

fn lit_center(pixels: &[u8], threshold: u8) -> f32 {
    let (mut sum, mut count) = (0.0, 0.0);
    for y in 0..EYE_H {
        for x in 0..EYE_W {
            if pixels[at(x, y) + 1] > threshold {
                sum += x as f32;
                count += 1.0;
            }
        }
    }
    assert!(count > 0.0, "nothing above {threshold}");
    sum / count
}

fn bright_quad_scene(depth: f32, half: f32) -> LabScene {
    let mut scene = lab(vec![quad([0.0, 0.0, -depth], half)]);
    scene.materials[0].emissive = [1.0, 1.0, 1.0];
    scene.materials[0].emissive_intensity = 4.0;
    scene
}

#[test]
fn eyes_draw_through_the_frame_and_submit_a_projection_layer_with_depth() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall(), quad([0.0, 0.0, -2.0], 0.5)]));
    r.focus();
    r.step(3);
    let stats = r.stats();
    assert_eq!(stats.mode, StereoMode::TwoPass);
    assert_eq!(stats.scene_passes, 2);
    assert!(stats.frames >= 1);
    assert!(stats.drawn[0] > 0 && stats.drawn[1] > 0);
    assert_eq!(stats.drawn[0], stats.drawn[1], "one cull for both eyes");
    assert_eq!(stats.depth_layers, 2);
    let rt = r.app.world().resource::<XrRuntime>();
    let layers = rt.last_layers();
    let projection = layers.projection.expect("projection layer");
    let report = rt.last_report().expect("frame");
    assert_eq!(projection.views[0].fov, report.views[0].fov);
    let depth = projection.views[1].depth.expect("depth info");
    assert_eq!((depth.min_depth, depth.max_depth), (0.0, 1.0));
    assert_eq!(depth.near_m, r.app.world().resource::<XrConfig>().near_m);
    assert_eq!(depth.image.rect, [0, 0, EYE_W, EYE_H]);
    let state = r.app.world().resource::<XrState>();
    assert!(state.last_error.is_none(), "{:?}", state.last_error);
    assert!(state.stats.frames >= 1);
    assert_eq!(state.stats.dropped_frames, 0);
    assert_eq!(
        r.app.world().resource::<ExtractedFrame>().window_views().len(),
        0,
        "the eyes replace the desktop view"
    );
    let left = r.eye(Eye::Left);
    assert!(left.chunks(4).any(|p| p[0] > 40), "the eye image was drawn");
    let graph = r.app.world().resource::<RuntimeRenderer>().0.graph_report();
    let pass = graph
        .extensions
        .iter()
        .find(|p| p.name == EYE_COMPOSITE_PASS)
        .expect("composite pass registered");
    assert!(pass.runs >= 2 && pass.failures == 0, "{pass:?}");
}

#[test]
fn a_scripted_head_replays_to_the_same_eye_images() {
    let _gpu = serial();
    let keyframe = |tick, position, orientation| SimTick {
        tick,
        head: ScriptPose {
            position,
            orientation,
        },
        left: None,
        right: None,
        inputs: Vec::new(),
        state: None,
    };
    let script = XrSimScript {
        ticks: vec![
            keyframe(0, (0.0, 0.0, 0.0), (0.0, 0.0, 0.0, 1.0)),
            keyframe(4, (0.3, 0.1, 0.0), (0.0, 0.1, 0.0, 0.995)),
        ],
        ..XrSimScript::default()
    };
    let run = || {
        let plugin = XrPlugin {
            script: Some(script.clone()),
            ..XrPlugin::simulator()
        };
        let mut r = rig_with(&bright_quad_scene(2.0, 0.3), plugin);
        r.focus();
        r.step(6);
        let tick = r.app.world().resource::<XrSession>().tick();
        (tick, r.eye(Eye::Left), r.eye(Eye::Right))
    };
    let (tick, left, right) = run();
    assert!(tick >= 4, "the script ran past its last keyframe: {tick}");
    assert_ne!(left, right);
    let again = run();
    assert_eq!(again.0, tick, "manual time steps the same ticks");
    assert_eq!(again.1, left, "the same script draws the same left eye");
    assert_eq!(again.2, right, "the same script draws the same right eye");
}

#[test]
fn near_objects_disparate_more_than_far_ones_and_ipd_zero_gives_identical_eyes() {
    let _gpu = serial();
    let disparity = |scene: LabScene| {
        let mut r = rig(&scene);
        r.focus();
        r.simulate(SimEvent::ViewConfig {
            ipd_m: 0.1,
            fov: Fov::symmetric(0.9, 0.9),
        });
        r.step(2);
        lit_center(&r.eye(Eye::Left), 60) - lit_center(&r.eye(Eye::Right), 60)
    };
    let near = disparity(bright_quad_scene(1.0, 0.2));
    let far = disparity(bright_quad_scene(6.0, 1.2));
    // ipd * focal / depth at focal = 48 / tan(0.9) pixels.
    let focal = 48.0 / 0.9_f32.tan();
    assert!((near - 0.1 * focal).abs() < 1.5, "near {near}");
    assert!((far - 0.1 * focal / 6.0).abs() < 1.5, "far {far}");
    assert!(near > far, "{near} {far}");

    let mut r = rig(&bright_quad_scene(1.0, 0.2));
    r.focus();
    r.simulate(SimEvent::MonoEyes(true));
    r.step(2);
    assert_eq!(r.eye(Eye::Left), r.eye(Eye::Right), "IPD 0");
}

#[test]
fn the_rig_moves_the_eyes_through_the_world() {
    let _gpu = serial();
    let mut r = rig(&bright_quad_scene(2.0, 0.3));
    r.focus();
    r.step(2);
    let before = lit_center(&r.eye(Eye::Left), 60);
    let mut rigs = r.app.world_mut().query_filtered::<&mut Transform, With<XrRig>>();
    rigs.single_mut(r.app.world_mut()).expect("rig").translation.x = 0.5;
    r.step(2);
    let after = lit_center(&r.eye(Eye::Left), 60);
    assert!(
        after < before - 5.0,
        "the image moves against the rig: {before} -> {after}"
    );
}

#[test]
fn the_vignette_darkens_the_edge_only_while_the_rig_moves() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.focus();
    r.step(3);
    let still = r.eye(Eye::Left);
    assert_eq!(r.app.world().resource::<XrSession>().vignette(), 0.0);
    let mut rigs = r.app.world_mut().query_filtered::<&mut Transform, With<XrRig>>();
    let mut x = 0.0;
    for _ in 0..16 {
        x += 0.2;
        rigs.single_mut(r.app.world_mut()).expect("rig").translation.x = x;
        r.app.update();
    }
    let strength = r.app.world().resource::<XrSession>().vignette();
    assert!(strength > 0.05, "{strength}");
    let moving = r.eye(Eye::Left);
    let (cx, cy) = (EYE_W / 2, EYE_H / 2);
    assert!(
        i32::from(moving[at(cx, cy)]).abs_diff(i32::from(still[at(cx, cy)])) <= 1,
        "the center stays: {} {}",
        moving[at(cx, cy)],
        still[at(cx, cy)]
    );
    assert!(
        moving[at(1, 1)] < still[at(1, 1)],
        "the corner darkens: {} {}",
        moving[at(1, 1)],
        still[at(1, 1)]
    );
}

#[test]
fn the_lens_mask_blacks_out_the_corners_and_saves_their_depth() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.focus();
    r.step(2);
    let open = r.eye(Eye::Right);
    r.simulate(SimEvent::LensMask { radius: 0.9 });
    r.step(2);
    let stats = r.stats();
    assert!(
        stats.hidden_triangles[0] > 0 && stats.hidden_triangles[1] > 0,
        "{stats:?}"
    );
    let masked = r.eye(Eye::Right);
    assert_eq!(masked[at(1, 1)..at(1, 1) + 3], [0, 0, 0], "a corner is black");
    let c = at(EYE_W / 2, EYE_H / 2);
    assert_eq!(masked[c..c + 3], open[c..c + 3], "the center is untouched");
    let depth = r
        .app
        .world()
        .resource::<XrRenderState>()
        .depth_texture(Eye::Right)
        .expect("depth")
        .clone();
    let device = r.device.clone();
    let values = read_depth(&device, &depth);
    assert_eq!(values[(EYE_W + 1) as usize], 0.0, "the mask holds the near plane");
    assert!(values[(EYE_H / 2 * EYE_W + EYE_W / 2) as usize] > 0.0);

    set_option(r.app.world_mut(), "hidden_area", &OptionValue::Bool(false)).expect("option");
    r.step(2);
    assert_eq!(r.stats().hidden_triangles, [0, 0]);
    let off = r.eye(Eye::Right);
    assert_eq!(
        off[at(1, 1)..at(1, 1) + 3],
        open[at(1, 1)..at(1, 1) + 3],
        "the option turns it off"
    );
}

fn read_depth(device: &RenderDevice, texture: &wgpu::Texture) -> Vec<f32> {
    let size = texture.size();
    let row =
        (size.width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test depth"),
        size: u64::from(row) * u64::from(size.height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::DepthOnly,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(size.height),
            },
        },
        size,
    );
    device.queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped");
    let mut out = Vec::with_capacity((size.width * size.height) as usize);
    for y in 0..size.height {
        let start = (y * row) as usize;
        let line = &mapped[start..start + (size.width * 4) as usize];
        out.extend_from_slice(bytemuck::cast_slice::<u8, f32>(line));
    }
    out
}

fn panel(id: u32, priority: i32, color: [u8; 4]) -> XrPanel {
    XrPanel {
        id,
        priority,
        space: LayerSpace::Head,
        pose: Pose {
            position: Vec3::new(0.0, 0.0, -1.0),
            orientation: Quat::IDENTITY,
        },
        size_m: [0.5, 0.5],
        image: PanelImage::Solid(color),
    }
}

#[test]
fn a_quad_layer_reaches_the_runtime_and_stays_out_of_the_eye_image() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.focus();
    r.app
        .world_mut()
        .resource_mut::<XrPanels>()
        .set(panel(4, 0, [255, 0, 0, 255]));
    r.step(2);
    let stats = r.stats();
    assert_eq!((stats.quads_composed, stats.quads_overflow), (1, 0));
    let layers = r.app.world().resource::<XrRuntime>().last_layers().clone();
    assert_eq!(layers.quads.len(), 1);
    assert_eq!(layers.quads[0].id, 4);
    let c = at(EYE_W / 2, EYE_H / 2);
    let eye = r.eye(Eye::Left);
    assert!(
        eye[c] < 200 || eye[c + 1] > 100,
        "the runtime composes it, not the eye"
    );
    let shown = r.display(Eye::Left);
    assert_eq!(
        shown[c..c + 3],
        [255, 0, 0],
        "the simulator's compositor shows the panel"
    );
    assert_eq!(
        shown[at(1, 1)..at(1, 1) + 3],
        eye[at(1, 1)..at(1, 1) + 3],
        "the rest is the eye"
    );
}

#[test]
fn panels_over_the_runtimes_budget_fall_back_into_the_eye_image_under_the_composed_ones() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.focus();
    let budget = r.app.world().resource::<XrRuntime>().backend().max_layers();
    assert_eq!(budget, 16);
    {
        let mut panels = r.app.world_mut().resource_mut::<XrPanels>();
        // 16 panels: one over the 15 quad layers. The lowest priority overflows.
        panels.set(panel(100, -1, [0, 255, 0, 255]));
        for id in 0..15 {
            let mut small = panel(id, id as i32, [0, 0, 255, 255]);
            small.pose.position.x = 5.0;
            panels.set(small);
        }
    }
    r.step(2);
    let stats = r.stats();
    assert_eq!((stats.quads_composed, stats.quads_overflow), (15, 1));
    let layers = r.app.world().resource::<XrRuntime>().last_layers().clone();
    assert_eq!(layers.quads.len(), 15);
    assert!(layers.quads.iter().all(|q| q.id != 100));
    let c = at(EYE_W / 2, EYE_H / 2);
    assert_eq!(r.eye(Eye::Left)[c..c + 3], [0, 255, 0], "drawn into the eye");
}

#[test]
fn a_head_locked_panel_lands_between_the_eyes_with_parallax() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.focus();
    r.simulate(SimEvent::ViewConfig {
        ipd_m: 0.1,
        fov: Fov::symmetric(0.9, 0.9),
    });
    r.app
        .world_mut()
        .resource_mut::<XrPanels>()
        .set(panel(1, 0, [255, 0, 0, 255]));
    r.step(2);
    let center_of_red = |pixels: &[u8]| {
        let (mut sum, mut count) = (0.0, 0.0);
        for y in 0..EYE_H {
            for x in 0..EYE_W {
                let p = &pixels[at(x, y)..at(x, y) + 3];
                if p == [255, 0, 0] {
                    sum += x as f32;
                    count += 1.0;
                }
            }
        }
        sum / f32::max(count, 1.0)
    };
    let left = center_of_red(&r.display(Eye::Left));
    let right = center_of_red(&r.display(Eye::Right));
    let expected = 0.1 * (48.0 / 0.9_f32.tan());
    assert!(((left - right) - expected).abs() < 1.5, "{left} {right}");
}

fn offscreen(device: &RenderDevice, format: wgpu::TextureFormat, w: u32, h: u32) -> wgpu::Texture {
    device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test mirror"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

#[test]
fn the_mirror_shows_the_chosen_eye_and_both_side_by_side() {
    let _gpu = serial();
    let device = RenderDevice::headless().expect("gpu");
    let make = |rgba: [u8; 4]| {
        let t = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test eye"),
            size: wgpu::Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        device.queue.write_texture(
            t.as_image_copy(),
            &rgba.repeat(16),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(16),
                rows_per_image: None,
            },
            t.size(),
        );
        t.create_view(&wgpu::TextureViewDescriptor::default())
    };
    let views = [make([200, 0, 0, 255]), make([0, 0, 200, 255])];
    let shared = SharedMirror::default();
    let mut painter = MirrorPainter::new(shared.clone());
    let mut show = |mode: Mirror, format: wgpu::TextureFormat| {
        shared.set(MirrorSource {
            views: Some(views.clone()),
            extent: [4, 4],
            mode,
        });
        let target = offscreen(&device, format, 16, 8);
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.device.create_command_encoder(&Default::default());
        painter.draw(&device, &mut encoder, &view, format, 16, 8);
        device.queue.submit([encoder.finish()]);
        regia_render::read_rgba8(&device.device, &device.queue, &target, 16, 8).expect("read")
    };
    let srgb = wgpu::TextureFormat::Rgba8UnormSrgb;
    let px = |pixels: &[u8], x: usize, y: usize| [pixels[(y * 16 + x) * 4], pixels[(y * 16 + x) * 4 + 2]];
    let left = show(Mirror::Left, srgb);
    assert_eq!(px(&left, 8, 4), [200, 0]);
    let right = show(Mirror::Right, srgb);
    assert_eq!(px(&right, 8, 4), [0, 200]);
    let both = show(Mirror::Both, srgb);
    assert_eq!(px(&both, 4, 4), [200, 0]);
    assert_eq!(px(&both, 12, 4), [0, 200]);
    // A plain target stores the same encoded bytes.
    let plain = show(Mirror::Left, wgpu::TextureFormat::Rgba8Unorm);
    assert!(px(&plain, 8, 4)[0].abs_diff(200) <= 1);
}

#[test]
fn the_mirror_reads_the_composed_display_on_the_simulator() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.focus();
    r.app
        .world_mut()
        .resource_mut::<XrPanels>()
        .set(panel(1, 0, [255, 0, 0, 255]));
    r.step(2);
    let overlay = r.app.world().resource::<regia_render::WindowOverlay>();
    assert!(overlay.is_set(), "the plugin installs the mirror layer");
    let shared = r.app.world().resource::<XrRenderState>().mirror().clone();
    let mut painter = MirrorPainter::new(shared);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let target = offscreen(&r.device, format, EYE_W, EYE_H);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = r.device.device.create_command_encoder(&Default::default());
    painter.draw(&r.device, &mut encoder, &view, format, EYE_W, EYE_H);
    r.device.queue.submit([encoder.finish()]);
    let pixels =
        regia_render::read_rgba8(&r.device.device, &r.device.queue, &target, EYE_W, EYE_H).expect("read");
    let c = at(EYE_W / 2, EYE_H / 2);
    assert_eq!(pixels[c..c + 3], [255, 0, 0]);
}

#[test]
fn a_hidden_session_draws_no_eye_and_leaves_the_desktop_view() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    drive_session_state(r.app.world_mut(), ScriptState::Synchronized).expect("sync");
    r.step(3);
    assert_eq!(r.stats().frames, 0, "no eye render while hidden");
    let rt = r.app.world().resource::<XrRuntime>();
    assert!(
        rt.last_report().is_some_and(|report| !report.rendered),
        "frames still flow"
    );
    assert!(rt.last_layers().is_empty(), "an empty list clears the display");
    assert!(
        !r.app
            .world()
            .resource::<ExtractedFrame>()
            .window_views()
            .is_empty(),
        "the desktop view stays"
    );
}

#[test]
fn a_session_that_hides_gives_the_window_back_and_the_mirror_goes_quiet() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.focus();
    r.step(2);
    drive_session_state(r.app.world_mut(), ScriptState::Visible).expect("visible");
    r.step(1);
    assert!(
        r.app
            .world()
            .resource::<ExtractedFrame>()
            .window_views()
            .is_empty()
    );
    drive_session_state(r.app.world_mut(), ScriptState::Synchronized).expect("hidden");
    r.step(1);
    assert!(
        !r.app
            .world()
            .resource::<ExtractedFrame>()
            .window_views()
            .is_empty()
    );
    let shared = r.app.world().resource::<XrRenderState>().mirror().clone();
    let mut painter = MirrorPainter::new(shared);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let target = offscreen(&r.device, format, 8, 8);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = r.device.device.create_command_encoder(&Default::default());
    {
        // A window frame already drew here; the mirror must leave it alone.
        let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("desktop view"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::GREEN),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
    }
    painter.draw(&r.device, &mut encoder, &view, format, 8, 8);
    r.device.queue.submit([encoder.finish()]);
    let pixels = regia_render::read_rgba8(&r.device.device, &r.device.queue, &target, 8, 8).expect("read");
    assert_eq!(pixels[..3], [0, 255, 0], "no old eye image over the desktop view");
}

#[test]
fn an_ended_session_releases_the_eyes() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.focus();
    r.step(2);
    assert_eq!(
        r.app
            .world()
            .resource::<RuntimeRenderer>()
            .0
            .texture_view_ids()
            .len(),
        2
    );
    drive_session_state(r.app.world_mut(), ScriptState::Stopping).expect("stop");
    r.step(2);
    assert!(
        r.app
            .world()
            .resource::<RuntimeRenderer>()
            .0
            .texture_view_ids()
            .is_empty()
    );
    assert!(!r.app.world().contains_resource::<SimEyeTargets>());
    r.focus();
    r.step(2);
    assert_eq!(
        r.app
            .world()
            .resource::<RuntimeRenderer>()
            .0
            .texture_view_ids()
            .len(),
        2,
        "a new session draws"
    );
    assert!(r.app.world().resource::<XrState>().last_error.is_none());
}

#[test]
fn a_paused_frame_loop_renders_nothing_and_counts_stale_frames() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.focus();
    r.step(2);
    let frames = r.stats().frames;
    r.app.world_mut().resource_mut::<XrSession>().frame_paused = true;
    r.step(3);
    assert_eq!(r.stats().frames, frames);
    assert_eq!(r.app.world().resource::<XrState>().stats.stale_frames, 3);
}

#[test]
fn the_render_scale_resizes_the_targets_between_frames() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.focus();
    r.step(1);
    set_option(r.app.world_mut(), "render_scale", &OptionValue::Number(0.5)).expect("scale");
    let views_before_scale = perf::texture_views();
    r.step(2);
    use regia_xr::EyeTargets;
    assert_eq!(
        r.app.world().resource::<SimEyeTargets>().extent(),
        UVec2::new(EYE_W / 2, EYE_H / 2)
    );
    assert_eq!(
        r.app.world().resource::<XrRenderProfile>().extent,
        UVec2::new(EYE_W / 2, EYE_H / 2)
    );
    assert!(r.app.world().resource::<XrState>().last_error.is_none());
    assert!(
        perf::texture_views() > views_before_scale,
        "a scale change builds new eye views"
    );
}

/// Spec 58 § 8.12: the same scene by multiview and by one pass per layer. Skipped, with a note,
/// when the adapter has no multiview.
#[test]
fn multiview_and_one_pass_per_layer_match_per_eye() {
    let _gpu = serial();
    let device = RenderDevice::headless().expect("gpu");
    if !StereoForward::multiview_supported(&device.device) {
        eprintln!("skipped: this adapter has no multiview");
        return;
    }
    let mut scene = lab(vec![
        wall(),
        quad([0.2, 0.0, -2.0], 0.5),
        quad([-0.3, 0.1, -1.2], 0.2),
    ]);
    scene.materials.push(LabMaterial {
        base_color: [0.1, 0.3, 0.7, 1.0],
        roughness: 0.3,
        ..LabMaterial::default()
    });
    scene.meshes[1].material = 1;
    scene.meshes[2].material = 1;
    let frame = scene.as_frame_scene_with_quality(FrameQuality::LAB);
    let views =
        regia_platform::xr::stereo_views(&Pose::IDENTITY, 0.08, Fov::symmetric(0.9, 0.8)).expect("views");
    let cull = union_cull_matrix(&views, 200.0).expect("union");
    let cameras = [
        eye_camera(&views[0], &Pose::IDENTITY, 0.05, cull).expect("camera"),
        eye_camera(&views[1], &Pose::IDENTITY, 0.05, cull).expect("camera"),
    ];
    let image = || {
        device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test eye"),
            size: wgpu::Extent3d {
                width: EYE_W,
                height: EYE_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: regia_xr::eye_targets::SIM_EYE_FORMAT,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    };
    let mut forward = StereoForward::default();
    let images = [image(), image()];
    let render = |forward: &mut StereoForward, multiview: bool| {
        let report = forward.render(
            &device.device,
            &device.queue,
            &frame,
            &cameras,
            &images,
            multiview,
        );
        let read = |t: &wgpu::Texture| {
            regia_render::read_rgba8(&device.device, &device.queue, t, EYE_W, EYE_H).expect("read")
        };
        (report, read(&images[0]), read(&images[1]))
    };
    let (one, one_left, one_right) = render(&mut forward, true);
    let (two, two_left, two_right) = render(&mut forward, false);
    assert_eq!((one.passes, two.passes), (1, 2));
    assert_eq!(one.drawn, 3);
    assert_eq!(one.drawn, two.drawn);
    let worst = |a: &[u8], b: &[u8]| a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
    assert!(
        worst(&one_left, &two_left) <= 1,
        "left {}",
        worst(&one_left, &two_left)
    );
    assert!(
        worst(&one_right, &two_right) <= 1,
        "right {}",
        worst(&one_right, &two_right)
    );
    assert_ne!(one_left, one_right, "each layer takes its own view");
}

#[test]
fn the_multiview_mode_runs_through_the_frame_loop_with_its_composite_and_depth() {
    let _gpu = serial();
    let mut r = rig(&bright_quad_scene(2.0, 0.4));
    if !StereoForward::multiview_supported(&r.device.device) {
        eprintln!("skipped: this adapter has no multiview");
        return;
    }
    r.focus();
    set_option(
        r.app.world_mut(),
        "stereo_mode",
        &OptionValue::Text("multiview".into()),
    )
    .expect("mode");
    r.simulate(SimEvent::LensMask { radius: 0.8 });
    r.step(2);
    let stats = r.stats();
    assert_eq!(stats.mode, StereoMode::Multiview);
    assert_eq!(stats.scene_passes, 1);
    assert_eq!(stats.depth_layers, 2);
    assert!(r.app.world().resource::<XrRenderProfile>().multiview);
    let left = r.eye(Eye::Left);
    assert_eq!(
        left[at(1, 1)..at(1, 1) + 3],
        [0, 0, 0],
        "the mask paints in this mode too"
    );
    assert!(lit_center(&left, 60) > 0.0);
    assert!(r.app.world().resource::<XrState>().last_error.is_none());
    assert!(last_views(r.app.world()).is_some());
}

#[test]
fn an_empty_panel_list_does_not_copy_the_eye_images() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.app.world_mut().resource_mut::<XrConfig>().mirror = Mirror::Off;
    r.focus();
    perf::reset();
    r.step(2);
    assert!(r.stats().frames >= 2, "both frames drew");
    assert_eq!(
        (perf::display_copies(), perf::display_submits()),
        (0, 0),
        "no panels: no display copy and no extra submit"
    );
    assert!(
        r.app
            .world()
            .resource::<XrRenderState>()
            .display_texture(Eye::Left)
            .is_none()
    );
}

#[test]
fn panel_bind_groups_are_reused_on_the_second_frame() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.focus();
    let panels = 4u32;
    {
        let mut stored = r.app.world_mut().resource_mut::<XrPanels>();
        for id in 0..panels {
            stored.set(panel(id, id as i32, [255, 0, 0, 255]));
        }
    }
    perf::reset();
    r.step(2);
    assert_eq!(r.stats().quads_composed, panels);
    // Two eyes, two frames: one group per panel, created on the first draw.
    assert_eq!(
        (perf::quad_bind_groups(), perf::quad_uniform_writes()),
        (u64::from(panels), 4),
        "groups stay at the panel count; each eye packs one uniform write"
    );
}

#[test]
fn two_pass_clones_the_draw_list_once() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall(), quad([0.0, 0.0, -2.0], 0.4)]));
    r.focus();
    perf::reset();
    r.step(2);
    let frames = r.stats().frames;
    assert!(frames >= 2);
    assert_eq!(r.stats().mode, StereoMode::TwoPass);
    assert_eq!(
        perf::scene_clones(),
        frames,
        "one draw-list clone per two-eye frame"
    );
}

fn eye_cameras() -> [EyeCamera; 2] {
    let views =
        regia_platform::xr::stereo_views(&Pose::IDENTITY, 0.08, Fov::symmetric(0.9, 0.8)).expect("views");
    let cull = union_cull_matrix(&views, 200.0).expect("union");
    [
        eye_camera(&views[0], &Pose::IDENTITY, 0.05, cull).expect("camera"),
        eye_camera(&views[1], &Pose::IDENTITY, 0.05, cull).expect("camera"),
    ]
}

fn eye_image(device: &RenderDevice) -> wgpu::Texture {
    device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test eye"),
        size: wgpu::Extent3d {
            width: EYE_W,
            height: EYE_H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: regia_xr::eye_targets::SIM_EYE_FORMAT,
        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

#[test]
fn multiview_object_uniforms_upload_in_one_write() {
    let _gpu = serial();
    let device = RenderDevice::headless().expect("gpu");
    let meshes: Vec<LabMesh> = (0..8u16)
        .map(|i| {
            let x = (f32::from(i) - 3.5) * 0.2;
            quad([x, 0.0, -2.0], 0.08)
        })
        .collect();
    let mut scene = lab(meshes);
    scene.materials[0].emissive = [1.0, 1.0, 1.0];
    scene.materials[0].emissive_intensity = 4.0;
    let frame = scene.as_frame_scene_with_quality(FrameQuality::LAB);
    let cameras = eye_cameras();
    let images = [eye_image(&device), eye_image(&device)];
    let mut forward = StereoForward::default();
    let multiview = StereoForward::multiview_supported(&device.device);
    perf::reset();
    let report = forward.render(
        &device.device,
        &device.queue,
        &frame,
        &cameras,
        &images,
        multiview,
    );
    assert_eq!(report.drawn, 8);
    assert_eq!(perf::object_uniform_writes(), 1, "one object-uniform upload");
    assert_eq!(perf::object_uniform_bytes(), 8 * OBJECT_SLOT);
    let read = |t: &wgpu::Texture| {
        regia_render::read_rgba8(&device.device, &device.queue, t, EYE_W, EYE_H).expect("read")
    };
    assert_ne!(read(&images[0]), read(&images[1]), "each eye keeps its own view");
}

#[test]
fn multiview_mesh_cache_updates_and_releases() {
    let _gpu = serial();
    let device = RenderDevice::headless().expect("gpu");
    let cameras = eye_cameras();
    let images = [eye_image(&device), eye_image(&device)];
    let mut forward = StereoForward::default();
    let paint = |forward: &mut StereoForward, scene: &LabScene| {
        let mut frame = scene.as_frame_scene_with_quality(FrameQuality::LAB);
        frame.meshes[0].mesh_key = 0;
        forward.render(&device.device, &device.queue, &frame, &cameras, &images, false);
        regia_render::read_rgba8(&device.device, &device.queue, &images[0], EYE_W, EYE_H).expect("read")
    };
    let mut scene = lab(vec![quad([-0.7, 0.0, -2.0], 0.3)]);
    scene.materials[0].emissive = [1.0, 1.0, 1.0];
    scene.materials[0].emissive_intensity = 4.0;
    let first = paint(&mut forward, &scene);
    scene.meshes[0] = quad([0.7, 0.0, -2.0], 0.3);
    let second = paint(&mut forward, &scene);
    let (left, right) = (lit_center(&first, 40), lit_center(&second, 40));
    assert!(
        right > left + 8.0,
        "a key-0 mesh uploads every frame ({left} -> {right})"
    );

    if !StereoForward::multiview_supported(&device.device) {
        eprintln!("skipped mesh release: this adapter has no multiview");
        return;
    }
    let meshes: Vec<LabMesh> = (0..6u16)
        .map(|i| {
            let x = (f32::from(i) - 2.5) * 0.25;
            quad([x, 0.0, -2.0], 0.08)
        })
        .collect();
    let mut r = rig(&lab(meshes));
    r.focus();
    set_option(
        r.app.world_mut(),
        "stereo_mode",
        &OptionValue::Text("multiview".into()),
    )
    .expect("mode");
    r.step(2);
    assert_eq!(r.stats().mode, StereoMode::Multiview);
    let before = perf::mesh_entries();
    assert!(before >= 2, "cached distinct meshes ({before})");
    let ids: Vec<Entity> = {
        let mut query = r.app.world_mut().query::<(Entity, &MeshRenderer)>();
        query.iter(r.app.world_mut()).map(|(entity, _)| entity).collect()
    };
    assert!(ids.len() >= 2);
    for entity in ids.into_iter().skip(1) {
        assert!(r.app.world_mut().despawn(entity));
    }
    r.step(1);
    let after = perf::mesh_entries();
    assert!(
        after < before,
        "released mesh keys leave the multiview cache ({before} -> {after})"
    );
}

/// A same-size edit of a cached (non-zero key) mesh announced through
/// `note_mesh_content_changed` must reach the multiview cache, as it does the main mesh cache.
/// Before the fix the cache kept the first upload and its bounds for good.
#[test]
fn multiview_mesh_cache_rewrites_same_size_content_edits() {
    let _gpu = serial();
    let device = RenderDevice::headless().expect("gpu");
    let cameras = eye_cameras();
    let images = [eye_image(&device), eye_image(&device)];
    let mut forward = StereoForward::default();
    let left = quad([-0.7, 0.0, -2.0], 0.3);
    // A registry key, so the content revision of this key can move.
    let owner = std::sync::Arc::new(regia_render::CpuMesh {
        vertices: left.vertices.clone(),
        indices: left.indices.clone(),
    });
    let key = regia_render::mesh_key(&owner);
    assert_ne!(key, 0);
    let paint = |forward: &mut StereoForward, scene: &LabScene| {
        let mut frame = scene.as_frame_scene_with_quality(FrameQuality::LAB);
        frame.meshes[0].mesh_key = key;
        forward.render(&device.device, &device.queue, &frame, &cameras, &images, false);
        regia_render::read_rgba8(&device.device, &device.queue, &images[0], EYE_W, EYE_H).expect("read")
    };
    let mut scene = lab(vec![left]);
    scene.materials[0].emissive = [1.0, 1.0, 1.0];
    scene.materials[0].emissive_intensity = 4.0;
    let first = paint(&mut forward, &scene);
    // Same vertex and index counts, new positions, same key: an in-place edit.
    scene.meshes[0] = quad([0.7, 0.0, -2.0], 0.3);
    regia_render::note_mesh_content_changed(&owner);
    let second = paint(&mut forward, &scene);
    let (before, after) = (lit_center(&first, 40), lit_center(&second, 40));
    assert!(
        after > before + 8.0,
        "a same-size content edit is rewritten into the cached buffers ({before} -> {after})"
    );
}

/// Mesh keys released on a frame where the eyes do not draw (paused here) must still leave the
/// multiview cache: `extract_frame` hands each key out once. Before the fix they stayed for good.
#[test]
fn multiview_cache_releases_keys_dropped_while_eyes_are_paused() {
    let _gpu = serial();
    let meshes: Vec<LabMesh> = (0..6u16)
        .map(|i| {
            let x = (f32::from(i) - 2.5) * 0.25;
            quad([x, 0.0, -2.0], 0.08)
        })
        .collect();
    let mut r = rig(&lab(meshes));
    if !StereoForward::multiview_supported(&r.device.device) {
        eprintln!("skipped: this adapter has no multiview");
        return;
    }
    r.focus();
    set_option(
        r.app.world_mut(),
        "stereo_mode",
        &OptionValue::Text("multiview".into()),
    )
    .expect("mode");
    r.step(2);
    assert_eq!(r.stats().mode, StereoMode::Multiview);
    let before = perf::mesh_entries();
    assert!(before >= 2, "cached distinct meshes ({before})");
    r.app.world_mut().resource_mut::<XrSession>().frame_paused = true;
    let ids: Vec<Entity> = {
        let mut query = r.app.world_mut().query::<(Entity, &MeshRenderer)>();
        query.iter(r.app.world_mut()).map(|(entity, _)| entity).collect()
    };
    assert!(ids.len() >= 2);
    for entity in ids.into_iter().skip(1) {
        assert!(r.app.world_mut().despawn(entity));
    }
    // The keys are extracted on these frames; the paused eyes do not draw.
    r.step(2);
    r.app.world_mut().resource_mut::<XrSession>().frame_paused = false;
    r.step(1);
    let after = perf::mesh_entries();
    assert!(
        after < before,
        "keys released while paused leave the multiview cache ({before} -> {after})"
    );
}

/// An ended session drops the multiview mesh cache: its keys may be released while no eye frame
/// runs, and nothing would hand them to the cache later.
#[test]
fn ended_session_drops_the_multiview_mesh_cache() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![
        quad([-0.3, 0.0, -2.0], 0.1),
        quad([0.3, 0.0, -2.0], 0.1),
    ]));
    if !StereoForward::multiview_supported(&r.device.device) {
        eprintln!("skipped: this adapter has no multiview");
        return;
    }
    r.focus();
    set_option(
        r.app.world_mut(),
        "stereo_mode",
        &OptionValue::Text("multiview".into()),
    )
    .expect("mode");
    r.step(2);
    assert!(
        perf::mesh_entries() >= 2,
        "meshes are cached while the session draws"
    );
    set_option(r.app.world_mut(), "enabled", &OptionValue::Bool(false)).expect("disable");
    r.step(1);
    assert_eq!(
        perf::mesh_entries(),
        0,
        "the ended session's mesh cache is dropped"
    );
}

#[test]
fn eye_views_are_created_with_the_targets() {
    let _gpu = serial();
    let mut r = rig(&lab(vec![wall()]));
    r.focus();
    perf::reset();
    r.step(1);
    let created = perf::texture_views();
    assert!(created > 0, "the first frame builds the eye views");
    r.step(2);
    assert_eq!(
        perf::texture_views(),
        created,
        "later frames reuse the eye and mirror views"
    );
}
