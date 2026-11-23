// SPDX-License-Identifier: MPL-2.0
//! Heap allocations of a steady frame with the radiance cache (ADR-2085).
//!
//! The same frame with the cache off, on while cells are being rewritten, and on at rest. The planner
//! and the update keep their lists, so a batch must not grow over the one before it, and the cache
//! costs only a few allocations per frame (one compute pass, one more bind group). The counts
//! are printed (`--nocapture`).
//!
//! Its own test binary: it installs a counting global allocator.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_core::glam::Mat4;
use regia_render::{
    DdgiSettings, DirectionalLightUniform, DynamicGiTier, FrameQuality, FrameRenderer, FrameTarget, GiMode,
    LabMaterial, LabMesh, LabScene, PostSettings, RadianceCacheSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, box_mesh,
};

struct Counting;
thread_local! {
    static COUNTED: Cell<bool> = const { Cell::new(false) };
}
static ALLOCS: AtomicU64 = AtomicU64::new(0);

fn count_this_thread() {
    let _ = COUNTED.try_with(|c| c.set(true));
}

fn note() {
    if COUNTED.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

fn allocs() -> u64 {
    ALLOCS.load(Ordering::Relaxed)
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

const W: u32 = 160;
const H: u32 = 96;
const BATCH: u64 = 8;

fn block(centre: [f32; 3], half: [f32; 3]) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material: 0,
        model: Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

fn scene() -> (LabScene, PostSettings, FrameQuality) {
    let t = 0.1;
    let post = PostSettings {
        exposure_ev: 14.5,
        ..PostSettings::golden_deterministic()
    };
    let lab = LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.6, -1.5],
        target: [0.0, 0.8, 1.5],
        fov_y: 1.0,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new([-0.53, -0.77, -0.35], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![
            block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0]),
            block([3.0 + t, 1.5, 0.0], [t, 1.5, 3.0 + 2.0 * t]),
            block([-3.0 - t, 1.5, 0.0], [t, 1.5, 3.0 + 2.0 * t]),
            block([0.0, 1.5, -3.0 - t], [3.0, 1.5, t]),
            block([-1.9, 1.5, 3.0 + t], [1.1, 1.5, t]),
            block([1.9, 1.5, 3.0 + t], [1.1, 1.5, t]),
            block([0.0, 3.0 + t, 0.0], [3.0 + 2.0 * t, t, 3.0 + 2.0 * t]),
        ],
    };
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q.msaa_samples = 1;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Probes;
    q.gi_probes = true;
    q.screen_space_gi = true;
    // A live cache invalidates the static-screen gather; the off/rest controls must
    // gather too. The non-paged shadow quality opts every control out of screen reuse.
    // Keep the original incremental allocation bounds, comparing equal base work.
    q.paged_sun_shadows = false;
    assert!(!q.reuse_static_screen());
    q.baked_gi = false;
    (lab, post, q)
}

#[test]
fn the_cache_adds_only_a_few_allocations_and_none_grow() {
    // ADR-2299 / ADR-4760: encode segments may finish on a worker or on the caller.
    // Count both; measuring only the caller measures scheduling instead of allocations.
    count_this_thread();
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let (lab, post, quality) = scene();
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let target = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("steady target"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let draw = |renderer: &mut FrameRenderer| {
        renderer
            .render_into(&device, &frame, FrameTarget::full(&view, format, W, H))
            .expect("frame");
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    let batch = |renderer: &mut FrameRenderer| {
        let before = allocs();
        for _ in 0..BATCH {
            draw(renderer);
            assert!(
                renderer.screen_pass_report().ssgi_gather,
                "all controls must gather SSGI, rather than compare a live gather to a reused one"
            );
        }
        allocs() - before
    };
    let batches = |renderer: &mut FrameRenderer| -> [u64; 6] { std::array::from_fn(|_| batch(renderer)) };
    let settle = |renderer: &mut FrameRenderer| {
        // Use the existing hwrt_frame_alloc warm/settle rule, without changing any
        // measured growth or cache-overhead bound below.
        for _ in 0..40 {
            draw(renderer);
        }
        assert_eq!(renderer.ddgi_pending_bakes(), 0, "synchronous probe bakes");
        let mut previous = batch(renderer);
        for _ in 1..12 {
            let next = batch(renderer);
            if previous.abs_diff(next) <= BATCH {
                return;
            }
            previous = next;
        }
        panic!("allocations did not settle before measurement: last batch {previous}");
    };
    let mut settings = DdgiSettings {
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..DdgiSettings::default()
    };
    DynamicGiTier::Low.apply(&mut settings);
    settings.update_divisor = 1;

    let mut off = FrameRenderer::new();
    off.set_encode_worker_start_hook(count_this_thread);
    off.set_ddgi_settings(settings.clone());
    off.set_ddgi_enabled(Some(true));
    settle(&mut off);
    let off_batches = batches(&mut off);
    drop(off);

    // Live: a budget so small the refresh never finishes in this test, so every frame runs the
    // update's compute pass.
    let mut live = FrameRenderer::new();
    live.set_encode_worker_start_hook(count_this_thread);
    live.set_ddgi_settings(settings.clone());
    live.set_ddgi_enabled(Some(true));
    live.set_radiance_cache(true);
    live.set_radiance_cache_settings(RadianceCacheSettings {
        budget_cells: 16,
        ..RadianceCacheSettings::default()
    });
    settle(&mut live);
    let live_batches = batches(&mut live);
    let status = live.radiance_cache_status();
    assert!(status.active && status.updated_cells > 0, "{status:?}");
    drop(live);

    // At rest: the same gather, nothing rewritten.
    let mut rest = FrameRenderer::new();
    rest.set_encode_worker_start_hook(count_this_thread);
    rest.set_ddgi_settings(settings);
    rest.set_ddgi_enabled(Some(true));
    rest.set_radiance_cache(true);
    for _ in 0..200 {
        draw(&mut rest);
        if rest.radiance_cache_status().idle {
            break;
        }
    }
    assert!(
        rest.radiance_cache_status().idle,
        "{:?}",
        rest.radiance_cache_status()
    );
    settle(&mut rest);
    let rest_batches = batches(&mut rest);

    println!("off {off_batches:?}, live {live_batches:?}, rest {rest_batches:?} (of {BATCH} frames)");
    // Measured on lavapipe: a burst of 50 to 220 allocations lands in a batch now and then, with the cache
    // off as well, so one batch says nothing. A leak raises the floor from one half of the run to the next.
    let floor = |b: &[u64]| b.iter().copied().min().unwrap_or(0);
    let (off, live, rest) = (floor(&off_batches), floor(&live_batches), floor(&rest_batches));
    println!(
        "per frame: off {:.1}, live {:.1}, rest {:.1}",
        off as f64 / BATCH as f64,
        live as f64 / BATCH as f64,
        rest as f64 / BATCH as f64
    );
    for batches in [&live_batches, &rest_batches] {
        assert!(
            floor(&batches[3..]) < floor(&batches[..3]) + BATCH,
            "a steady cached frame's allocations grow: {batches:?}"
        );
    }
    // While cells are being rewritten: one compute pass, one more bind group, one staged write (measured:
    // 21 a frame, almost all of them inside wgpu's pass recording).
    assert!(
        live <= off + 32 * BATCH,
        "off {off_batches:?}, live {live_batches:?}"
    );
    assert!(
        rest <= off + 16 * BATCH,
        "off {off_batches:?}, rest {rest_batches:?}"
    );
}
