// SPDX-License-Identifier: MPL-2.0
//! Diagnostic, not a gate (`#[ignore]`): records where every heap allocation of the drawing thread
//! comes from, frame by frame, for a run of steady frames. It is how the lone one-frame allocation
//! event of the steady-frame tests (`hwrt_frame_alloc`, `oit_frame_alloc`, `frame_alloc_steady`)
//! was traced to its source (ADR-2299).
//!
//! Each allocation and reallocation made while a frame is drawn, on the drawing thread and on the
//! renderer's encode workers (`FrameRenderer::set_encode_worker_start_hook`), stores the frame
//! number, the thread (0 drawing, 1.. workers), the size and up to `DEPTH` raw return addresses
//! from glibc's `backtrace()`, into a buffer reserved before recording starts (the allocator hook
//! itself never allocates through the Rust allocator). After the run, `records.txt` (one line per
//! allocation: frame, thread, realloc, size, addresses in hex), `counts.txt` (frame, allocations,
//! drawing-thread allocations, microseconds), `maps.txt` (`/proc/self/maps`) and `exe.txt` are
//! written to `REGIA_ALLOC_PROBE_OUT` (default: a fresh temp dir, printed). An address resolves
//! with `llvm-addr2line -f -i -C -e <exe> <address - exe load base - 1>` (the load base is the
//! exe's first mapping in `maps.txt`); diff a spike frame's sites against a neighbor's.
//!
//! ```text
//! REGIA_ALLOC_PROBE_FRAMES=2000 REGIA_ALLOC_PROBE_TRACE=screen \
//!   cargo test -p regia_render --test alloc_spike_probe -- --ignored --nocapture
//! ```
//!
//! `REGIA_ALLOC_PROBE_SCENE=glass` draws `frame_alloc_steady`'s glass scene instead of the hwrt one.
//! `REGIA_ALLOC_PROBE_WORKERS=0` counts the drawing thread only (the steady-frame tests before
//! ADR-2299): the lone +48 frames come back, each one a segment the drawing thread finished
//! because no worker had started it yet.
//!
//! Linux with glibc only (`backtrace()`); the other targets build an empty test binary.

#![cfg(all(target_os = "linux", target_env = "gnu"))]
#![allow(unsafe_code)]

use std::cell::Cell;
use std::ffi::{c_int, c_void};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use regia_core::glam::{Mat4, Vec3};
use regia_render::hwrt::RtShadowRequest;
use regia_render::{
    ALPHA_PREMULTIPLIED, DirectionalLightUniform, FrameQuality, FrameRenderer, FrameTarget, LabMaterial,
    LabMesh, LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh,
    translate_mesh,
};
use regia_test_utils::alloc_counter::{AllocEvent, AllocHook, AllocKind, CountingAlloc};

// SAFETY: the declaration matches glibc's `int backtrace(void **buffer, int size)` from
// `execinfo.h` (pointer to an array of `void *`, C `int` count, C `int` result), and the file only
// builds on Linux with glibc (`#![cfg]` above), where libc is always linked. Each call site states
// why its arguments are valid.
unsafe extern "C" {
    /// glibc `execinfo.h`: fills `buffer` with up to `size` return addresses of the calling stack.
    fn backtrace(buffer: *mut *mut c_void, size: c_int) -> c_int;
}

/// Return addresses kept per allocation (the allocator's own frames included).
const DEPTH: usize = 48;
/// Records reserved per recorded frame (a steady frame makes about 210 here).
const RESERVE_PER_FRAME: usize = 600;

#[derive(Clone, Copy)]
struct Record {
    frame: u32,
    thread: u8,
    realloc: bool,
    size: usize,
    depth: u8,
    addrs: [usize; DEPTH],
}

struct Probe;
thread_local! {
    /// This thread's tag: `None` for threads not counted, 0 for the drawing thread, 1.. for the
    /// encode workers.
    static TAG: Cell<Option<u8>> = const { Cell::new(None) };
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}
/// Allocations of the counted threads.
static ALLOCS: AtomicU64 = AtomicU64::new(0);
/// Allocations of the drawing thread alone (the count of the tests before ADR-2299).
static DRAWING_ALLOCS: AtomicU64 = AtomicU64::new(0);
static FRAME: AtomicU32 = AtomicU32::new(0);
static RECORDING: AtomicBool = AtomicBool::new(false);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static NEXT_WORKER: AtomicU8 = AtomicU8::new(1);
static RECORDS: Mutex<Vec<Record>> = Mutex::new(Vec::new());

/// The encode workers' start hook: counts the worker's allocations from now on.
fn count_worker() {
    let tag = NEXT_WORKER.fetch_add(1, Ordering::Relaxed);
    let _ = TAG.try_with(|t| t.set(Some(tag)));
}

/// Counts one allocation of a counted thread and, while recording, stores its stack without
/// allocating.
fn note(realloc: bool, size: usize) {
    let Some(thread) = TAG.try_with(Cell::get).ok().flatten() else {
        return;
    };
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    if thread == 0 {
        DRAWING_ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
    if !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    if IN_HOOK.try_with(|h| h.replace(true)).unwrap_or(true) {
        return;
    }
    let mut buf = [std::ptr::null_mut::<c_void>(); DEPTH];
    // SAFETY: `buf` holds `DEPTH` pointers; glibc writes at most that many.
    let n = unsafe { backtrace(buf.as_mut_ptr(), DEPTH as c_int) };
    let depth = usize::try_from(n).unwrap_or(0).min(DEPTH);
    let mut addrs = [0usize; DEPTH];
    for (dst, src) in addrs.iter_mut().zip(&buf[..depth]) {
        *dst = *src as usize;
    }
    let frame = FRAME.load(Ordering::Relaxed);
    let mut records = RECORDS.lock().unwrap_or_else(PoisonError::into_inner);
    if records.len() < records.capacity() {
        records.push(Record {
            frame,
            thread,
            realloc,
            size,
            depth: depth as u8,
            addrs,
        });
    } else {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    drop(records);
    let _ = IN_HOOK.try_with(|h| h.set(false));
}

// The shared counting allocator (ADR-3291) with the probe as its hook: `note` runs on every
// request (a `realloc` is one request), and the allocator does not call it again for what it
// allocates itself.
impl AllocHook for Probe {
    fn on_request(&self, event: AllocEvent) {
        note(event.kind == AllocKind::Realloc, event.size);
    }
}

#[global_allocator]
static A: CountingAlloc<Probe> = CountingAlloc::with_hook(Probe);

fn allocs() -> (u64, u64) {
    (
        ALLOCS.load(Ordering::Relaxed),
        DRAWING_ALLOCS.load(Ordering::Relaxed),
    )
}

const W: u32 = 160;
const H: u32 = 96;

/// The scene of `hwrt_frame_alloc.rs`: a floor and nine boxes, sun shadows on.
fn scene() -> LabScene {
    let (gv, gi) = box_mesh([20.0, 0.05, 20.0]);
    let (bv, bi) = box_mesh([0.5, 0.5, 0.5]);
    let mut meshes = vec![LabMesh {
        vertices: translate_mesh(&gv, [0.0, -0.05, 0.0]),
        indices: gi,
        material: 0,
        ..LabMesh::unskinned()
    }];
    for i in -4..=4 {
        meshes.push(LabMesh {
            vertices: translate_mesh(&bv, [i as f32 * 1.5, 1.0, 0.0]),
            indices: bi.clone(),
            material: 0,
            ..LabMesh::unskinned()
        });
    }
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 8.0, 8.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -0.8, 0.2], [1.0, 1.0, 1.0], 20000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes,
    }
}

/// The scene of `frame_alloc_steady.rs`: a floor with bright bars under a refracting glass pane
/// (`REGIA_ALLOC_PROBE_SCENE=glass`; drawn with screen-space refraction on).
fn glass_scene() -> LabScene {
    let slab = |half: [f32; 3], model: Mat4, material: usize| {
        let (vertices, indices) = box_mesh(half);
        LabMesh {
            vertices,
            indices,
            material,
            model: model.to_cols_array_2d(),
            ..LabMesh::unskinned()
        }
    };
    let floor = LabMaterial {
        base_color: [0.4, 0.4, 0.4, 1.0],
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let bar = LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        emissive: [1.0, 0.9, 0.8],
        emissive_intensity: 3000.0,
        roughness: 1.0,
        ..LabMaterial::default()
    };
    let glass = LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        roughness: 0.05,
        transmission: 1.0,
        ior: 1.5,
        transmission_thickness: 0.05,
        alpha_mode: ALPHA_PREMULTIPLIED,
        ..LabMaterial::default()
    };
    let mut meshes = vec![slab(
        [10.0, 0.005, 10.0],
        Mat4::from_translation(Vec3::new(0.0, -1.005, 0.0)),
        0,
    )];
    for i in -6..=6 {
        meshes.push(slab(
            [0.125, 0.2, 10.0],
            Mat4::from_translation(Vec3::new(i as f32 * 0.7, -0.8, 0.0)),
            1,
        ));
    }
    meshes.push(slab([1.2, 0.005, 1.2], Mat4::from_rotation_z(0.3), 2));
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 4.0, 2.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -0.8, 0.2], [1.0, 1.0, 1.0], 20000.0),
        local_lights: Vec::new(),
        materials: vec![floor, bar, glass],
        meshes,
    }
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "diagnostic: records an allocation backtrace per allocation; run by hand"]
fn record_the_allocation_sites_of_steady_frames() {
    let frames = env_u32("REGIA_ALLOC_PROBE_FRAMES", 600);
    let warm_frames = env_u32("REGIA_ALLOC_PROBE_WARM", 120);
    let request = match std::env::var("REGIA_ALLOC_PROBE_TRACE").as_deref() {
        Ok("screen") => RtShadowRequest::Screen,
        Ok("compute") => RtShadowRequest::Compute,
        _ => RtShadowRequest::Off,
    };
    let out = std::env::var_os("REGIA_ALLOC_PROBE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| regia_test_utils::unique_temp_dir("alloc-probe"));
    std::fs::create_dir_all(&out).expect("probe output dir");

    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    let glass = std::env::var("REGIA_ALLOC_PROBE_SCENE").as_deref() == Ok("glass");
    let lab = if glass { glass_scene() } else { scene() };
    let post = PostSettings {
        exposure_ev: 12.0,
        ..PostSettings::golden_deterministic()
    };
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let target = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe target"),
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
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    quality.sun_shadow_trace = request;
    quality.screen_space_refraction = glass;
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    TAG.with(|t| t.set(Some(0)));
    let workers = std::env::var("REGIA_ALLOC_PROBE_WORKERS").as_deref() != Ok("0");
    if workers {
        renderer.set_encode_worker_start_hook(count_worker);
    }
    let draw = |renderer: &mut FrameRenderer| {
        renderer
            .render_into(&device, &frame, FrameTarget::full(&view, format, W, H))
            .expect("frame");
        let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    };
    let mut warm = 0;
    while warm < warm_frames || (renderer.ddgi_pending_bakes() > 0 && warm < 2000) {
        draw(&mut renderer);
        warm += 1;
    }

    RECORDS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .reserve_exact(frames as usize * RESERVE_PER_FRAME);
    // glibc loads its unwinder on the first `backtrace()` call: do that before recording.
    let mut buf = [std::ptr::null_mut::<c_void>(); 4];
    // SAFETY: `buf` holds 4 pointers.
    let _ = unsafe { backtrace(buf.as_mut_ptr(), 4) };
    let mut counts: Vec<(u64, u64, u128)> = Vec::with_capacity(frames as usize);

    for f in 0..frames {
        FRAME.store(f, Ordering::Relaxed);
        let (before, before_drawing) = allocs();
        let t = std::time::Instant::now();
        RECORDING.store(true, Ordering::Relaxed);
        draw(&mut renderer);
        RECORDING.store(false, Ordering::Relaxed);
        let (after, after_drawing) = allocs();
        counts.push((
            after - before,
            after_drawing - before_drawing,
            t.elapsed().as_micros(),
        ));
    }

    let records = std::mem::take(&mut *RECORDS.lock().unwrap_or_else(PoisonError::into_inner));
    let dropped = DROPPED.load(Ordering::Relaxed);
    let mut file =
        std::io::BufWriter::new(std::fs::File::create(out.join("records.txt")).expect("records file"));
    for r in &records {
        let _ = write!(
            file,
            "{} {} {} {}",
            r.frame,
            r.thread,
            u8::from(r.realloc),
            r.size
        );
        for a in &r.addrs[..r.depth as usize] {
            let _ = write!(file, " {a:x}");
        }
        let _ = writeln!(file);
    }
    let _ = file.flush();
    let mut file =
        std::io::BufWriter::new(std::fs::File::create(out.join("counts.txt")).expect("counts file"));
    for (f, (n, drawing, us)) in counts.iter().enumerate() {
        let _ = writeln!(file, "{f} {n} {drawing} {us}");
    }
    let _ = file.flush();
    std::fs::copy("/proc/self/maps", out.join("maps.txt")).expect("maps");
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    std::fs::write(out.join("exe.txt"), &exe).expect("exe");

    let summary = |pick: fn(&(u64, u64, u128)) -> u64| {
        let mut sorted: Vec<u64> = counts.iter().map(pick).collect();
        sorted.sort_unstable();
        let median = sorted[sorted.len() / 2];
        let off: Vec<(usize, u64)> = counts
            .iter()
            .map(pick)
            .enumerate()
            .filter(|&(_, n)| n != median)
            .collect();
        format!(
            "median {median}, min {}, max {}, {} frames off the median (first 24: {:?})",
            sorted[0],
            sorted[sorted.len() - 1],
            off.len(),
            &off[..off.len().min(24)]
        )
    };
    println!(
        "alloc probe {request:?} (workers counted: {workers}, {} encode threads): {frames} frames \
         after {warm} warm; counted threads: {}; drawing thread alone: {}; {} records ({dropped} \
         dropped); out {} exe {exe}",
        renderer.encode_threads(),
        summary(|c| c.0),
        summary(|c| c.1),
        records.len(),
        out.display()
    );
    assert_eq!(dropped, 0, "raise RESERVE_PER_FRAME");
}
