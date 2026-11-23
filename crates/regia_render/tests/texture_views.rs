// SPDX-License-Identifier: MPL-2.0
//! Render-to-texture views (v0.1.6, ADR-184 "Texture views keep their own view state").
//!
//! DO27's cockpit screens, target feed and map rendered through the window's own view state:
//! every frame flipped the renderer between the window camera and a 512² camera, so targets
//! were reallocated, history and paged shadows invalidated, and the host read the images back
//! with a blocking poll. These tests pin the fix:
//!
//! - texture views leave the main view's pixels, shadow cache and frame-path creations alone;
//! - a view's image reaches a material on the GPU (a screen quad in the main view);
//! - a non-blocking readback returns the view's pixels without a blocking poll;
//! - `update_every = 3` renders the view on one frame in three (runtime system);
//! - the screen shows the view's image from the first frame, not a black one;
//! - dropping the views returns their GPU memory (no cache keeps their buffers or targets);
//! - objects entering a view's sight create nothing: the view draws through the GPU-driven
//!   batches, not per object (v0.1.6 merge gate, `arena` `spawn_hitch`; ADR-184 addendum).

use regia_app::prelude::*;
use regia_render::{
    Camera, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, FrameTarget,
    LabMaterial, LabMesh, LabScene, LabVertex, MaterialImage, MaterialImages, PostSettings, RenderDevice,
    RenderProfile, RenderProfilePreset, RenderTarget, RuntimeRenderer, TextureViewSettings, extract_frame,
    render_texture_views, spawn_lab_scene,
};
use regia_scene::{GlobalTransform, Transform};

const W: u32 = 96;
const H: u32 = 64;
/// Asset id of the screen view.
const SCREEN: u64 = 0x5C;
/// Asset id of the top-down map view.
const MAP: u64 = 0x3A;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip texture_views: {err}");
            None
        }
    }
}

fn quad(center: [f32; 3], half: [f32; 2], normal_z: bool) -> Vec<LabVertex> {
    let [cx, cy, cz] = center;
    let [hx, hy] = half;
    let (corners, normal) = if normal_z {
        (
            [
                [cx - hx, cy - hy, cz],
                [cx + hx, cy - hy, cz],
                [cx + hx, cy + hy, cz],
                [cx - hx, cy + hy, cz],
            ],
            [0.0, 0.0, 1.0],
        )
    } else {
        (
            [
                [cx - hx, cy, cz + hy],
                [cx + hx, cy, cz + hy],
                [cx + hx, cy, cz - hy],
                [cx - hx, cy, cz - hy],
            ],
            [0.0, 1.0, 0.0],
        )
    };
    let uvs = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
    corners
        .into_iter()
        .zip(uvs)
        .map(|(position, uv)| LabVertex {
            position,
            normal,
            uv,
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

/// A lit floor with grey boxes (shadow casters) in front of the main camera, a red box behind
/// it (only the screen camera sees that), and a "screen" quad facing the main camera whose
/// albedo is `screen` (or plain grey).
fn scene(screen: Option<regia_render::DynamicTexture>) -> LabScene {
    let (b, bi) = regia_render::box_mesh([0.6, 0.6, 0.6]);
    let (red, red_i) = regia_render::box_mesh([3.0, 3.0, 0.5]);
    let mut screen_material = LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        roughness: 0.9,
        ..LabMaterial::default()
    };
    if let Some(texture) = screen {
        screen_material.images = MaterialImages {
            albedo: Some(MaterialImage::Dynamic(texture)),
            ..MaterialImages::EMPTY
        };
    }
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 2.5, 6.0],
        target: [0.0, 1.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 14.5,
            auto_exposure: false,
            bloom: true,
            taa: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.35, -0.8, -0.45], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.6, 0.6, 0.6, 1.0],
                roughness: 0.7,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.9, 0.05, 0.03, 1.0],
                roughness: 0.8,
                ..LabMaterial::default()
            },
            screen_material,
        ],
        meshes: vec![
            LabMesh {
                vertices: quad([0.0, 0.0, 0.0], [20.0, 20.0], false),
                indices: vec![0, 1, 2, 0, 2, 3],
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: regia_render::translate_mesh(&b, [-1.5, 0.6, 0.0]),
                indices: bi.clone(),
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: regia_render::translate_mesh(&b, [1.8, 0.6, -1.0]),
                indices: bi,
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: regia_render::translate_mesh(&red, [0.0, 3.0, 30.0]),
                indices: red_i,
                material: 1,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: quad([0.0, 1.6, -0.5], [0.9, 0.7], true),
                indices: vec![0, 1, 2, 0, 2, 3],
                material: 2,
                ..LabMesh::unskinned()
            },
        ],
    }
}

/// Desktop High (TAAU, paged sun shadows, SSAO, SSR) without the probes, which converge over
/// frames on their own clock.
fn desktop_high() -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.gi_probes = false;
    quality
}

/// The screen camera: behind the main camera, looking back at the sunlit face of the red box.
fn screen_view<'a>(lab: &'a LabScene, quality: FrameQuality) -> FrameScene<'a> {
    let mut scene = lab.as_frame_scene_with_quality(quality);
    scene.width = 64;
    scene.height = 64;
    scene.eye = [0.0, 3.0, 40.0];
    scene.target = [0.0, 3.0, 30.0];
    scene.fov_y = 0.6;
    scene
}

/// A top-down camera far above the main view.
fn map_view<'a>(lab: &'a LabScene, quality: FrameQuality) -> FrameScene<'a> {
    let mut scene = lab.as_frame_scene_with_quality(quality);
    scene.width = 64;
    scene.height = 64;
    scene.eye = [40.0, 120.0, -40.0];
    scene.target = [40.01, 0.0, -40.0];
    scene.fov_y = 0.5;
    scene
}

struct Main {
    renderer: FrameRenderer,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl Main {
    fn new(device: &RenderDevice) -> Self {
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("texture views main"),
            size: wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut renderer = FrameRenderer::new();
        renderer.set_ddgi_enabled(Some(false));
        Self {
            renderer,
            texture,
            view,
        }
    }

    fn frame(&mut self, device: &RenderDevice, lab: &LabScene) {
        let scene = lab.as_frame_scene_with_quality(desktop_high());
        let target = FrameTarget {
            color: Some(&self.texture),
            ..FrameTarget::full(&self.view, wgpu::TextureFormat::Rgba8UnormSrgb, W, H)
        };
        self.renderer
            .render_into(device, &scene, target)
            .unwrap_or_else(|err| panic!("main frame: {err}"));
    }

    fn views(&mut self, device: &RenderDevice, lab: &LabScene, settings: &TextureViewSettings) {
        let quality = desktop_high();
        self.renderer
            .render_texture_view(device, SCREEN, screen_view(lab, quality), settings)
            .unwrap_or_else(|err| panic!("screen view: {err}"));
        self.renderer
            .render_texture_view(
                device,
                MAP,
                map_view(lab, quality),
                &TextureViewSettings::default(),
            )
            .unwrap_or_else(|err| panic!("map view: {err}"));
    }

    /// One more main frame, read back (the comparison frame).
    fn pixels(&mut self, device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
        let scene = lab.as_frame_scene_with_quality(desktop_high());
        self.renderer
            .render_offscreen(device, &scene)
            .unwrap_or_else(|err| panic!("read: {err}"))
            .pixels
    }
}

/// `(shadow passes, dirty pages, atlas tiles drawn, cascades skipped, terrain draws)` of the
/// main view's last frame.
fn shadow_work(renderer: &FrameRenderer) -> (u32, usize, u32, u32, u64) {
    let report = renderer.shadow_cache_report();
    (
        report.shadow_passes,
        report.dirty_pages.len(),
        report.atlas_tiles_drawn,
        report.cascades_skipped,
        renderer.shared_shadow_draws(),
    )
}

fn max_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn texture_views_leave_the_main_view_alone() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = scene(None);
    let settings = TextureViewSettings::default();
    let mut off = Main::new(&device);
    let mut on = Main::new(&device);
    // Warm: pipelines, uploads, the views' own targets and shadow maps.
    for _ in 0..10 {
        off.frame(&device, &lab);
        on.views(&device, &lab, &settings);
        on.frame(&device, &lab);
    }
    // Steady state: views and main view create nothing, and the main view does the same
    // shadow work with views as without.
    let before = regia_render::frame_path_counters();
    let mut work_on = Vec::new();
    for _ in 0..4 {
        on.views(&device, &lab, &settings);
        on.frame(&device, &lab);
        work_on.push(shadow_work(&on.renderer));
    }
    let created = regia_render::frame_path_counters().since(before);
    let mut work_off = Vec::new();
    for _ in 0..4 {
        off.frame(&device, &lab);
        work_off.push(shadow_work(&off.renderer));
    }
    eprintln!("steady creations with views: {created:?}");
    eprintln!("main shadow work, views on {work_on:?}, off {work_off:?}");
    assert_eq!(created.textures, 0, "textures created per steady frame");
    assert_eq!(created.bind_groups, 0, "bind groups created per steady frame");
    assert_eq!(created.buffers, 0, "buffers created per steady frame");
    assert_eq!(created.pipelines, 0, "pipelines created per steady frame");
    assert_eq!(
        work_on, work_off,
        "texture views changed the main view's shadow work"
    );
    // A held main camera on paged shadows redraws nothing.
    assert!(work_on.iter().all(|w| w.0 == 0 && w.1 == 0), "{work_on:?}");

    on.views(&device, &lab, &settings);
    let a = off.pixels(&device, &lab);
    let b = on.pixels(&device, &lab);
    let diff = max_diff(&a, &b);
    eprintln!("main view max channel difference views on vs off: {diff}");
    assert!(
        diff <= 1,
        "texture views changed the main view's pixels (max {diff})"
    );

    // The views rendered something of their own.
    let screen = on
        .renderer
        .read_texture_target(&device, SCREEN)
        .unwrap_or_else(|err| panic!("screen: {err}"));
    let reds = screen
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] > p[1].saturating_add(40) && p[0] > p[2].saturating_add(40))
        .count();
    assert!(
        reds * 4 > screen.pixels.len() / 4,
        "the screen camera should see the red box ({reds} red pixels)"
    );
    assert_eq!(on.renderer.texture_view_ids(), vec![MAP, SCREEN]);
}

#[test]
fn a_view_image_shows_on_a_quad_through_the_gpu() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let settings = TextureViewSettings::with_texture([64, 64]);
    let handle = settings.texture.clone().expect("texture");
    let lab = scene(Some(handle));
    let mut main = Main::new(&device);
    // The view creates the material texture and copies its image into it before the main view
    // samples it (same frame); later frames keep doing so.
    for _ in 0..4 {
        main.views(&device, &lab, &settings);
        main.frame(&device, &lab);
    }
    main.views(&device, &lab, &settings);
    let pixels = main.pixels(&device, &lab);
    // The screen quad covers the image centre.
    let at = |x: u32, y: u32| {
        let i = ((y * W + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };
    let centre = at(W / 2, H / 2 - 4);
    eprintln!("screen quad centre {centre:?}");
    assert!(
        centre[0] > centre[1].saturating_add(30) && centre[0] > centre[2].saturating_add(30),
        "the screen quad does not show the view's red box: {centre:?}"
    );

    // The same scene with a plain grey screen is not red: the colour came through the view.
    let grey_lab = scene(None);
    let mut grey = Main::new(&device);
    for _ in 0..2 {
        grey.frame(&device, &grey_lab);
    }
    let grey_px = grey.pixels(&device, &grey_lab);
    let i = (((H / 2 - 4) * W + W / 2) * 4) as usize;
    let g = [grey_px[i], grey_px[i + 1], grey_px[i + 2]];
    assert!(g[0] < g[1].saturating_add(20), "control screen is red: {g:?}");
}

#[test]
fn async_readback_returns_the_view_without_a_blocking_poll() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = scene(None);
    let mut main = Main::new(&device);
    for _ in 0..3 {
        main.views(&device, &lab, &TextureViewSettings::default());
        main.frame(&device, &lab);
    }
    let before = regia_render::frame_path_counters();
    main.renderer
        .request_texture_readback(&device, SCREEN)
        .unwrap_or_else(|err| panic!("request: {err}"));
    let mut result = None;
    for _ in 0..400 {
        if let Some(pixels) = main.renderer.take_texture_readback(SCREEN) {
            result = Some(pixels.unwrap_or_else(|err| panic!("readback: {err}")));
            break;
        }
        device
            .poll_nonblocking()
            .unwrap_or_else(|err| panic!("poll: {err}"));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let polls = regia_render::frame_path_counters().since(before).blocking_polls;
    let result = result.expect("the readback never completed");
    assert_eq!(polls, 0, "the async readback waited on the device");
    assert!(
        main.renderer.take_texture_readback(SCREEN).is_none(),
        "taken twice"
    );
    let blocking = main
        .renderer
        .read_texture_target(&device, SCREEN)
        .unwrap_or_else(|err| panic!("read: {err}"));
    assert_eq!((result.width, result.height), (64, 64));
    assert_eq!(result.pixels, blocking.pixels, "async and blocking reads differ");
    // No target, no request.
    assert!(main.renderer.request_texture_readback(&device, 0xDEAD).is_err());
}

#[test]
fn update_every_three_renders_the_view_on_one_frame_in_three() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = scene(None);
    let mut app = App::new();
    app.init_resource::<regia_render::ExtractedFrame>();
    app.insert_resource(device);
    app.init_resource::<RuntimeRenderer>();
    spawn_lab_scene(app.world_mut(), &lab);
    let transform = {
        use regia_core::math::{Quat, Vec3};
        Transform {
            translation: Vec3::new(0.0, 3.0, 40.0),
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    };
    app.world_mut().spawn((
        transform,
        GlobalTransform(transform.compute_affine()),
        Camera {
            target: RenderTarget::Texture { asset_id: SCREEN },
            ..Camera::default()
        },
        TextureViewSettings {
            size: [32, 32],
            ..TextureViewSettings::default().update_every(3)
        },
    ));
    app.add_systems(Render, (extract_frame, render_texture_views).chain());
    let mut rendered = Vec::new();
    for _ in 0..9 {
        app.update();
        let stats = regia_render::texture_view_stats();
        assert_eq!(stats.views, 1);
        rendered.push(stats.rendered);
    }
    assert_eq!(rendered, vec![1, 0, 0, 1, 0, 0, 1, 0, 0]);
    let renderer = app.world().resource::<RuntimeRenderer>();
    assert_eq!(renderer.0.texture_view_ids(), vec![SCREEN]);
}

/// The red share of the main view's screen-quad centre (`> 0` when the screen shows the view).
fn screen_red(pixels: &[u8]) -> i32 {
    let i = (((H / 2 - 4) * W + W / 2) * 4) as usize;
    i32::from(pixels[i]) - i32::from(pixels[i + 1].max(pixels[i + 2]))
}

#[test]
fn the_screen_shows_the_view_on_its_first_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let settings = TextureViewSettings::with_texture([64, 64]);
    let handle = settings.texture.clone().expect("texture");
    let lab = scene(Some(handle));
    let mut main = Main::new(&device);
    // One host frame: the views, then the main view (read back).
    main.views(&device, &lab, &settings);
    let pixels = main.pixels(&device, &lab);
    let red = screen_red(&pixels);
    eprintln!("first-frame screen red excess {red}");
    assert!(
        red > 30,
        "the screen was not showing the view on its first frame ({red})"
    );
}

#[test]
fn dropping_texture_views_returns_their_memory() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = scene(None);
    let quality = desktop_high();
    let settings = TextureViewSettings::with_texture([256, 256]);
    let mut main = Main::new(&device);
    for _ in 0..6 {
        main.frame(&device, &lab);
    }
    let settle = |device: &RenderDevice| {
        device.wait_idle().unwrap_or_else(|err| panic!("wait: {err}"));
        device.wait_idle().unwrap_or_else(|err| panic!("wait: {err}"));
        regia_render::memory_report(device)
    };
    let base = settle(&device);
    let mut screen = screen_view(&lab, quality);
    screen.width = 256;
    screen.height = 256;
    let mut map = map_view(&lab, quality);
    map.width = 256;
    map.height = 256;
    for _ in 0..4 {
        main.renderer
            .render_texture_view(&device, SCREEN, screen.clone(), &settings)
            .unwrap_or_else(|err| panic!("screen view: {err}"));
        main.renderer
            .render_texture_view(&device, MAP, map.clone(), &TextureViewSettings::default())
            .unwrap_or_else(|err| panic!("map view: {err}"));
        main.frame(&device, &lab);
    }
    let with_views = settle(&device);
    main.renderer.drop_texture_view(SCREEN);
    main.renderer.drop_texture_view(MAP);
    drop(settings);
    for _ in 0..6 {
        main.frame(&device, &lab);
    }
    let after = settle(&device);
    eprintln!(
        "allocated: main only {} KB, with two 256² views {} KB, after dropping them {} KB",
        base.allocated_bytes >> 10,
        with_views.allocated_bytes >> 10,
        after.allocated_bytes >> 10
    );
    assert!(main.renderer.texture_view_ids().is_empty());
    if !base.available {
        eprintln!("skip allocator check: the backend has no allocator report");
        return;
    }
    assert!(
        with_views.allocated_bytes > base.allocated_bytes + (1 << 20),
        "the views allocated nothing measurable"
    );
    // Every label back to its baseline count: the views' buffers and targets are gone, not
    // just small enough to hide in a margin. Not the views': the per-object uniforms of meshes
    // only a view drew (the shared object cache, evicted by its residency budget like any
    // mesh) and wgpu's own indirect-validation pools.
    let shared = |label: &str| label == "object uniform" || label.starts_with("(wgpu internal)");
    let count = |report: &regia_render::MemoryReport, label: &str| {
        report
            .labels
            .iter()
            .find(|l| l.label == label)
            .map_or(0, |l| l.count)
    };
    let own_bytes = |report: &regia_render::MemoryReport| -> u64 {
        report
            .labels
            .iter()
            .filter(|l| !shared(&l.label))
            .map(|l| l.bytes)
            .sum()
    };
    let mut kept = Vec::new();
    for label in after.labels.iter().filter(|l| !shared(&l.label)) {
        let before = count(&base, &label.label);
        if label.count > before {
            kept.push((label.label.clone(), before, label.count));
        }
    }
    eprintln!(
        "renderer-owned: main only {} KB, after dropping the views {} KB",
        own_bytes(&base) >> 10,
        own_bytes(&after) >> 10
    );
    assert!(kept.is_empty(), "dropped views kept allocations: {kept:?}");
    assert!(
        own_bytes(&after) <= own_bytes(&base) + (64 << 10),
        "dropped views kept {} KB",
        (own_bytes(&after).saturating_sub(own_bytes(&base))) >> 10
    );
}

/// Crates spawned in front of the screen camera, `count` of them on a grid: the lab's grey box
/// (one mesh, one material) with a new object key each, as a spawner adds them.
fn crates<'a>(template: &FrameMesh<'a>, count: usize) -> Vec<FrameMesh<'a>> {
    (0..count)
        .map(|i| {
            let (col, row) = ((i % 16) as f32, (i / 16) as f32);
            // The template box (1.2 m) sits at x = -1.5; the crates stand 0.3 m apart (no shared
            // faces, so the draw order cannot decide a pixel) at x -11.25..11.25, z 28 and nearer.
            let offset = [col * 1.5 - 9.75, 0.0, 28.0 - row * 1.5];
            FrameMesh {
                object_key: 0xC4A7_E000 + i as u64,
                model: [
                    [1.0, 0.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 0.0],
                    [offset[0], offset[1], offset[2], 1.0],
                ],
                ..*template
            }
        })
        .collect()
}

/// One renderer with a main view and a 512² screen view over the lab scene plus `crates`
/// crates; returns what the steady frames created, the view's mean GPU ms over the timed
/// frames and the view's image. `view_gpu_driven` picks the view's draw path (off: the
/// per-object path texture views took before the ADR-184 addendum).
fn crate_run(
    device: &RenderDevice,
    lab: &LabScene,
    view_gpu_driven: bool,
    timed_frames: usize,
) -> (regia_render::FramePathCounters, f32, Vec<u8>) {
    let max_crates: usize = regia_core::brand::env_var("TEXTURE_VIEW_COST_CRATES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128);
    let warm_frames: usize = regia_core::brand::env_var("TEXTURE_VIEW_COST_WARMUP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let mut main = Main::new(device);
    // A level reserves its instance budget at load (`FrameRenderer::prewarm`): the cull and
    // instance buffers are sized once, so a spawn does not regrow them.
    main.renderer.reserve_instances(max_crates as u32 + 64);
    main.renderer.set_gpu_timing(device, true);
    let settings = TextureViewSettings::default();
    let base = lab.as_frame_scene_with_quality(desktop_high());
    let template = base.meshes[1];
    let frame = |main: &mut Main, count: usize| {
        let mut view_quality = desktop_high();
        view_quality.gpu_driven = view_gpu_driven;
        let mut view = screen_view(lab, view_quality);
        view.width = 512;
        view.height = 512;
        view.meshes.extend(crates(&template, count));
        main.renderer
            .render_texture_view(device, SCREEN, view, &settings)
            .unwrap_or_else(|err| panic!("screen view: {err}"));
        let mut scene = lab.as_frame_scene_with_quality(desktop_high());
        scene.meshes.extend(crates(&template, count));
        let target = FrameTarget {
            color: Some(&main.texture),
            ..FrameTarget::full(&main.view, wgpu::TextureFormat::Rgba8UnormSrgb, W, H)
        };
        main.renderer
            .render_into(device, &scene, target)
            .unwrap_or_else(|err| panic!("main frame: {err}"));
    };
    // Warm: pipelines, the view's targets and buffers, with the first crates in sight.
    for _ in 0..warm_frames {
        frame(&mut main, 16);
    }
    // Spawns: eight new crates a frame until the budget, then held for the timed frames.
    let before = regia_render::frame_path_counters();
    let mut count = 16;
    while count < max_crates {
        count = (count + 8).min(max_crates);
        frame(&mut main, count);
    }
    let created = regia_render::frame_path_counters().since(before);
    let mut gpu_ms = Vec::new();
    let mut passes: Vec<(&'static str, f32)> = Vec::new();
    for _ in 0..timed_frames {
        frame(&mut main, count);
        let Some(timings) = main.renderer.texture_view_gpu_timings(SCREEN) else {
            continue;
        };
        if timings.frame_ms > 0.0 {
            gpu_ms.push(timings.frame_ms);
            for (i, (name, ms)) in timings.passes().into_iter().enumerate() {
                match passes.get_mut(i) {
                    Some(sum) => sum.1 += ms,
                    None => passes.push((name, ms)),
                }
            }
        }
    }
    let n = gpu_ms.len().max(1) as f32;
    let mean = gpu_ms.iter().sum::<f32>() / n;
    let busy: Vec<String> = passes
        .iter()
        .filter(|(_, ms)| *ms > 0.0)
        .map(|(name, ms)| format!("{name} {:.3}", ms / n))
        .collect();
    eprintln!(
        "{} view, {count} crates: {} timed frames, mean GPU ms per pass: {}",
        if view_gpu_driven {
            "GPU-driven"
        } else {
            "per-object"
        },
        gpu_ms.len(),
        busy.join(", ")
    );
    let image = main
        .renderer
        .read_texture_target(device, SCREEN)
        .unwrap_or_else(|err| panic!("screen: {err}"))
        .pixels;
    (created, mean, image)
}

/// Crates spawned in a 512² view's sight create no bind group or buffer, and the view's
/// GPU-driven image equals its per-object one. Prints the view's GPU ms per pass on both paths:
/// for a timing run set `TEXTURE_VIEW_COST_WARMUP=120 TEXTURE_VIEW_COST_FRAMES=300` (and
/// `TEXTURE_VIEW_COST_CRATES`, default 128) on an exclusive card; `TEXTURE_VIEW_COST_DUMP=<dir>`
/// writes both images.
#[test]
fn objects_entering_a_view_create_nothing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let lab = scene(None);
    let timed: usize = regia_core::brand::env_var("TEXTURE_VIEW_COST_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let (created, batched_ms, batched) = crate_run(&device, &lab, true, timed);
    let (per_object_created, per_object_ms, per_object) = crate_run(&device, &lab, false, timed);
    eprintln!("spawns into a 512² view, GPU-driven view: created {created:?}");
    eprintln!("spawns into a 512² view, per-object view: created {per_object_created:?}");
    eprintln!(
        "512² view GPU ms (mean of {timed} frames): GPU-driven {batched_ms:.3}, per object {per_object_ms:.3}"
    );
    let diff = max_diff(&batched, &per_object);
    let (mut differing, mut lo, mut hi) = (0usize, [u32::MAX; 2], [0u32; 2]);
    for (i, (a, b)) in batched
        .as_chunks::<4>()
        .0
        .iter()
        .zip(per_object.as_chunks::<4>().0)
        .enumerate()
    {
        if a.iter().zip(b).any(|(x, y)| x.abs_diff(*y) > 1) {
            differing += 1;
            let (x, y) = ((i % 512) as u32, (i / 512) as u32);
            lo = [lo[0].min(x), lo[1].min(y)];
            hi = [hi[0].max(x), hi[1].max(y)];
        }
    }
    eprintln!(
        "view image max channel difference GPU-driven vs per object: {diff} ({differing} pixels > 1, \
         x {}..{} y {}..{})",
        lo[0], hi[0], lo[1], hi[1]
    );
    if let Ok(dir) = regia_core::brand::env_var("TEXTURE_VIEW_COST_DUMP") {
        for (name, pixels) in [("gpu_driven", &batched), ("per_object", &per_object)] {
            let path = std::path::Path::new(&dir).join(format!("view_{name}.png"));
            let file = std::fs::File::create(&path).unwrap_or_else(|err| panic!("{path:?}: {err}"));
            let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 512, 512);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .and_then(|mut w| w.write_image_data(pixels))
                .unwrap_or_else(|err| panic!("{path:?}: {err}"));
        }
    }
    let reds = batched
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] > p[1].saturating_add(40) && p[0] > p[2].saturating_add(40))
        .count();
    assert!(reds > 0, "the screen camera should see the red box");
    // The GPU-driven path draws the same picture; a ≤ 1 level difference is the batch order.
    assert!(
        diff <= 1,
        "the view's GPU-driven image differs from the per-object one ({diff})"
    );
    if !regia_render::IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
        eprintln!("skip creation check: the device has no instanced indirect draws (per-object path)");
        return;
    }
    assert_eq!(
        created.bind_groups, 0,
        "bind groups created by spawns in a view's sight"
    );
    assert_eq!(created.buffers, 0, "buffers created by spawns in a view's sight");
    assert_eq!(
        created.textures, 0,
        "textures created by spawns in a view's sight"
    );
    assert_eq!(
        created.pipelines, 0,
        "pipelines created by spawns in a view's sight"
    );
}
