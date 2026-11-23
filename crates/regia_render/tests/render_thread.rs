// SPDX-License-Identifier: MPL-2.0
//! The render thread for a host that drives `FrameRenderer` itself (ADR-2074), and the CPU pipelining
//! switch of the frame, on the headless device.
//!
//! A moving camera over a Desktop High frame (sun cascades with paged shadows, TAA, post): the frames
//! rendered on a render thread, or with the sun page cache updated on the compute pool, are
//! byte-identical to the inline frames of the same scene, every frame.

use std::sync::atomic::{AtomicBool, Ordering};

use regia_render::{
    DirectionalLightUniform, FramePipelining, FrameQuality, FrameRenderer, FrameScene, FrameTarget,
    LabMaterial, LabMesh, LabScene, LabVertex, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, RenderThreadError, RenderThreadSettings, with_render_thread,
};

const WIDTH: u32 = 96;
const HEIGHT: u32 = 64;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const FRAMES: usize = 8;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("no adapter: {err}");
            None
        }
    }
}

fn quad(half: f32, y: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 1.0, 0.0];
    (
        vec![
            LabVertex::from_pos_normal_uv([-half, y, -half], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([half, y, -half], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([half, y, half], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([-half, y, half], n, [0.0, 1.0]),
        ],
        vec![0, 2, 1, 0, 3, 2],
    )
}

/// A ground and a grid of floating quads that cast sun shadows.
fn lab() -> LabScene {
    let (vertices, indices) = quad(30.0, 0.0);
    let mut meshes = vec![LabMesh {
        vertices,
        indices,
        material: 0,
        ..LabMesh::unskinned()
    }];
    for x in 0..24 {
        for z in 0..24 {
            let (mut vertices, indices) = quad(0.4, 1.0 + (x % 3) as f32);
            for v in &mut vertices {
                v.position[0] += x as f32 * 1.2 - 14.0;
                v.position[2] += z as f32 * 1.2 - 14.0;
            }
            meshes.push(LabMesh {
                vertices,
                indices,
                material: 1,
                ..LabMesh::unskinned()
            });
        }
    }
    LabScene {
        width: WIDTH,
        height: HEIGHT,
        eye: [0.0, 8.0, 22.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -0.8, -0.35], [1.0, 0.97, 0.9], 110_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.35, 0.4, 0.3, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.7, 0.3, 0.2, 1.0],
                roughness: 0.4,
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

fn quality(pipelining: FramePipelining) -> FrameQuality {
    let mut profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
    profile.render_scale = 1.0;
    profile.frame_pipelining = pipelining;
    FrameQuality::from_profile(&profile)
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: true,
        taa: true,
        ..PostSettings::golden_deterministic()
    }
}

/// The camera orbits; frames 5 and 6 hold it, so the page cache's clean path runs as well.
fn at<'a>(base: &FrameScene<'a>, frame: usize) -> FrameScene<'a> {
    let angle = frame.min(5) as f32 * 0.05;
    FrameScene {
        eye: [22.0 * angle.sin(), 8.0, 22.0 * angle.cos()],
        ..base.clone()
    }
}

struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

fn target(device: &RenderDevice) -> Target {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("render thread test"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
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
    Target { texture, view }
}

fn read(device: &RenderDevice, texture: &wgpu::Texture) -> Vec<u8> {
    let row = (WIDTH * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(row * HEIGHT),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
    );
    device.queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    buffer.slice(..).get_mapped_range().expect("mapped").to_vec()
}

/// Every frame's bytes, rendered inline.
fn inline(device: &RenderDevice, pipelining: FramePipelining) -> (Vec<Vec<u8>>, (FramePipelining, bool)) {
    let lab = lab();
    let post = post();
    let mut base = lab.as_frame_scene_with_quality(quality(pipelining));
    base.post = Some(&post);
    let out = target(device);
    let mut renderer = FrameRenderer::new();
    let mut frames = Vec::new();
    for frame in 0..FRAMES {
        renderer
            .render_into(
                device,
                &at(&base, frame),
                FrameTarget::full(&out.view, FORMAT, WIDTH, HEIGHT),
            )
            .expect("frame");
        frames.push(read(device, &out.texture));
    }
    (
        frames,
        (renderer.frame_pipelining(), renderer.shadow_plan_on_tasks()),
    )
}

fn assert_same(what: &str, inline: &[Vec<u8>], other: &[Vec<u8>]) {
    assert_eq!(inline.len(), other.len());
    for (frame, (a, b)) in inline.iter().zip(other).enumerate() {
        let different = a.iter().zip(b).filter(|(x, y)| x != y).count();
        assert_eq!(different, 0, "{what}: frame {frame} differs in {different} bytes");
    }
    assert_ne!(
        inline[0], inline[3],
        "{what}: the camera did not move, so this proves little"
    );
}

#[test]
fn frames_on_the_render_thread_are_byte_identical_to_inline_frames() {
    let Some(device) = device() else { return };
    let (expected, _) = inline(&device, FramePipelining::OFF);
    let lab = lab();
    let post = post();
    let mut base = lab.as_frame_scene_with_quality(quality(FramePipelining::OFF));
    base.post = Some(&post);
    let out = target(&device);
    let mut renderer = FrameRenderer::new();
    let (frames, stats) =
        with_render_thread(&mut renderer, &device, RenderThreadSettings::default(), |host| {
            let mut frames = Vec::new();
            for frame in 0..FRAMES {
                host.submit(
                    at(&base, frame),
                    FrameTarget::full(&out.view, FORMAT, WIDTH, HEIGHT),
                )
                .expect("the render thread is alive");
                // The barrier: every frame handed over has been submitted.
                host.with_renderer(|_| ()).expect("the render thread is alive");
                frames.push(read(&device, &out.texture));
            }
            assert_eq!(host.take_error(), None);
            frames
        })
        .expect("the thread started");
    assert_same("render thread", &expected, &frames);
    assert_eq!(stats.frames, FRAMES as u64);
    assert_eq!(stats.failed, 0);
    assert_eq!(
        renderer.frame_index(),
        FRAMES as u64,
        "the renderer is back and counted every frame"
    );
}

/// Without a barrier the host runs ahead of the frames (the point of the thread), and the last
/// frame still comes out the same: nothing was dropped, reordered or overwritten.
#[test]
fn a_host_running_ahead_gets_the_same_last_frame() {
    let Some(device) = device() else { return };
    let (expected, _) = inline(&device, FramePipelining::OFF);
    let lab = lab();
    let post = post();
    let mut base = lab.as_frame_scene_with_quality(quality(FramePipelining::OFF));
    base.post = Some(&post);
    let out = target(&device);
    let mut renderer = FrameRenderer::new();
    let (_, stats) = with_render_thread(&mut renderer, &device, RenderThreadSettings::default(), |host| {
        for frame in 0..FRAMES {
            host.submit(
                at(&base, frame),
                FrameTarget::full(&out.view, FORMAT, WIDTH, HEIGHT),
            )
            .expect("the render thread is alive");
        }
    })
    .expect("the thread started");
    assert_eq!(stats.frames, FRAMES as u64);
    assert_eq!(renderer.frame_index(), FRAMES as u64);
    let last = read(&device, &out.texture);
    assert_eq!(&last, expected.last().expect("frames"), "the last frame moved");
}

#[test]
fn renderer_calls_run_in_frame_order_on_one_thread() {
    let Some(device) = device() else { return };
    let lab = lab();
    let post = post();
    let mut base = lab.as_frame_scene_with_quality(quality(FramePipelining::OFF));
    base.post = Some(&post);
    let out = target(&device);
    let mut renderer = FrameRenderer::new();
    let host_thread = std::thread::current().id();
    let (seen, _) = with_render_thread(
        &mut renderer,
        &device,
        RenderThreadSettings {
            frames_in_flight: 0,
            ..RenderThreadSettings::default()
        },
        |host| {
            let mut seen = Vec::new();
            for frame in 0..3 {
                host.submit(
                    at(&base, frame),
                    FrameTarget::full(&out.view, FORMAT, WIDTH, HEIGHT),
                )
                .expect("alive");
                let (index, thread, name) = host
                    .with_renderer(|renderer| {
                        (
                            renderer.frame_index(),
                            std::thread::current().id(),
                            std::thread::current().name().map(str::to_owned),
                        )
                    })
                    .expect("alive");
                assert_ne!(thread, host_thread);
                assert_eq!(name.as_deref(), Some("regia-render"));
                seen.push(index);
            }
            seen
        },
    )
    .expect("the thread started");
    assert_eq!(
        seen,
        [1, 2, 3],
        "each call sees every frame handed over before it"
    );
}

/// A posted call runs on the render thread in order with the frames, and the host does not wait for
/// it: a host that presents posts its whole frame (acquire, record, present) this way. The frames a
/// posted call records are the inline frames.
#[test]
fn posted_calls_run_in_order_on_the_render_thread_without_a_reply() {
    let Some(device) = device() else { return };
    let (expected, _) = inline(&device, FramePipelining::OFF);
    let lab = lab();
    let post = post();
    let mut base = lab.as_frame_scene_with_quality(quality(FramePipelining::OFF));
    base.post = Some(&post);
    let out = target(&device);
    let mut renderer = FrameRenderer::new();
    let host_thread = std::thread::current().id();
    let seen = std::sync::Mutex::new(Vec::new());
    let (frames, stats) = with_render_thread(
        &mut renderer,
        &device,
        RenderThreadSettings {
            frames_in_flight: 0,
            ..RenderThreadSettings::default()
        },
        |host| {
            let mut frames = Vec::new();
            for frame in 0..FRAMES {
                let (scene, view, gpu, seen) = (at(&base, frame), &out.view, &device, &seen);
                host.post(move |renderer| {
                    assert_ne!(std::thread::current().id(), host_thread);
                    renderer
                        .render_into(gpu, &scene, FrameTarget::full(view, FORMAT, WIDTH, HEIGHT))
                        .expect("frame");
                    seen.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(renderer.frame_index());
                })
                .expect("the render thread is alive");
                host.with_renderer(|_| ()).expect("the render thread is alive");
                frames.push(read(&device, &out.texture));
            }
            frames
        },
    )
    .expect("the thread started");
    assert_same("posted frames", &expected, &frames);
    assert_eq!(
        stats.frames, 0,
        "posted calls are not counted as handed-over frames"
    );
    let seen = seen
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(seen, (1..=FRAMES as u64).collect::<Vec<_>>());
}

/// One queued frame lets the host hand a second command over while the render thread is still in the
/// first; a rendezvous (`queued_frames: 0`) returns only once the render thread has taken the command,
/// so the first has finished by then. No timing: the first call waits for the host's signal.
#[test]
fn the_queue_depth_decides_how_far_the_host_runs_ahead() {
    let Some(device) = device() else { return };
    for queued_frames in [1, 0] {
        let mut renderer = FrameRenderer::new();
        let first_done = AtomicBool::new(false);
        let (signal, release) = std::sync::mpsc::channel::<()>();
        let settings = RenderThreadSettings {
            frames_in_flight: 0,
            queued_frames,
        };
        with_render_thread(&mut renderer, &device, settings, |host| {
            let first_done = &first_done;
            host.post(move |_| {
                let _ = release.recv();
                first_done.store(true, Ordering::SeqCst);
            })
            .expect("alive");
            if queued_frames == 1 {
                // Queued behind the blocked first call: returns at once (a rendezvous would hang here).
                host.post(|_| ()).expect("alive");
                assert!(
                    !first_done.load(Ordering::SeqCst),
                    "the first call is still waiting"
                );
                signal.send(()).expect("the render thread listens");
            } else {
                std::thread::scope(|scope| {
                    let signal = signal.clone();
                    scope.spawn(move || {
                        let _ = signal.send(());
                    });
                    host.post(|_| ()).expect("alive");
                });
                assert!(
                    first_done.load(Ordering::SeqCst),
                    "a rendezvous hands the second call over only after the first returned"
                );
            }
            host.with_renderer(|_| ()).expect("alive");
        })
        .expect("the thread started");
        assert!(first_done.load(Ordering::SeqCst));
    }
}

/// With a frames-in-flight limit the render thread takes each submission to wait on it; with 0 it
/// leaves them (a host with a swapchain gets its wait from the acquire).
#[test]
fn the_frames_in_flight_limit_is_what_takes_the_submissions() {
    let Some(device) = device() else { return };
    let lab = lab();
    let post = post();
    let mut base = lab.as_frame_scene_with_quality(quality(FramePipelining::OFF));
    base.post = Some(&post);
    let out = target(&device);
    for (frames_in_flight, left) in [(0, true), (1, false), (2, false)] {
        let mut renderer = FrameRenderer::new();
        with_render_thread(
            &mut renderer,
            &device,
            RenderThreadSettings {
                frames_in_flight,
                ..RenderThreadSettings::default()
            },
            |host| {
                for frame in 0..3 {
                    host.submit(
                        at(&base, frame),
                        FrameTarget::full(&out.view, FORMAT, WIDTH, HEIGHT),
                    )
                    .expect("alive");
                    let still_there = host
                        .with_renderer(|renderer| renderer.take_submission().is_some())
                        .expect("alive");
                    assert_eq!(still_there, left, "limit {frames_in_flight}, frame {frame}");
                }
            },
        )
        .expect("the thread started");
    }
}

/// A panic on the render thread reaches the host as an error, and the scope raises it again.
#[test]
fn a_panic_on_the_render_thread_stops_the_host_and_comes_back() {
    let Some(device) = device() else { return };
    let mut renderer = FrameRenderer::new();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_render_thread(&mut renderer, &device, RenderThreadSettings::default(), |host| {
            let first = host.with_renderer(|_| -> u32 { panic!("on purpose") });
            assert_eq!(first, Err(RenderThreadError::Stopped));
            assert_eq!(RenderThreadError::Stopped.code(), "render_thread_stopped");
            let second = host.with_renderer(|_| 1);
            assert_eq!(second, Err(RenderThreadError::Stopped), "the thread is gone");
        })
    }));
    let panic = outcome.expect_err("the scope raises the thread's panic again");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"on purpose"));
}

#[test]
fn the_parallel_shadow_plan_leaves_every_frame_byte_identical() {
    let Some(device) = device() else { return };
    let (off, off_applied) = inline(&device, FramePipelining::OFF);
    let (on, on_applied) = inline(&device, FramePipelining::ON);
    assert_same("parallel shadow plan", &off, &on);
    assert!(
        on_applied.1,
        "the update did not run on tasks, so this proves nothing"
    );
    assert!(on_applied.0.parallel_shadow_plan);
    if std::env::var("REGIA_FRAME_PIPELINING").is_err() {
        assert!(!off_applied.1);
        assert!(!off_applied.0.parallel_shadow_plan);
    }
}
