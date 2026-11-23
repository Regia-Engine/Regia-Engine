// SPDX-License-Identifier: MPL-2.0
//! What thin film on 500 materials costs a steady frame's CPU side, by counters (ADR-2482): heap
//! allocations, bytes uploaded, GPU objects created and the colour pass's encoder calls, against
//! the same 500 materials without the film. No wall clock.
//!
//! The card-costs run of 2026-11-03 measured +7.2 to +8.1 ms of CPU frame on `synthetic_atrium`
//! (500 materials) with the film on every material: a filmed draw takes the CPU colour loop
//! (ADR-1977), so each filmed draw is its own draw with its own pipeline and group 3 bind.
//!
//! Its own test binary: it installs a counting global allocator that counts every thread.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::backtrace::Backtrace;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast, ThinFilmFactors, box_mesh,
    frame_path_counters,
};

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
/// While set, every counted allocation also records its call stack ([`allocation_sites`]).
static RECORD: AtomicBool = AtomicBool::new(false);
static SITES: Mutex<Vec<(String, Backtrace)>> = Mutex::new(Vec::new());

thread_local! {
    /// Set while this thread records a stack: the recording's own allocations are not recorded.
    static IN_RECORD: Cell<bool> = const { Cell::new(false) };
}

fn allocs() -> u64 {
    ALLOCS.load(Ordering::Relaxed)
}

fn count() {
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    if RECORD.load(Ordering::Relaxed) && !IN_RECORD.with(Cell::get) {
        IN_RECORD.with(|flag| flag.set(true));
        let trace = Backtrace::force_capture();
        let thread = std::thread::current().name().unwrap_or("<unnamed>").to_owned();
        if let Ok(mut sites) = SITES.lock() {
            sites.push((thread, trace));
        }
        IN_RECORD.with(|flag| flag.set(false));
    }
}

// SAFETY: forwards to the system allocator; only counts (and, for the diagnostics, records).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

const MATERIALS: usize = 500;
const BATCH: u64 = 4;

fn materials(film: bool) -> Vec<LabMaterial> {
    (0..MATERIALS)
        .map(|i| LabMaterial {
            base_color: [
                0.05 + 0.9 * ((i % 7) as f32 / 6.0),
                0.05 + 0.9 * ((i % 11) as f32 / 10.0),
                0.05 + 0.9 * ((i % 13) as f32 / 12.0),
                1.0,
            ],
            roughness: 0.2 + 0.6 * ((i % 5) as f32 / 4.0),
            thin_film: if film {
                ThinFilmFactors {
                    factor: 1.0,
                    ior: 1.3,
                    thickness_min: 100.0,
                    thickness_max: 200.0 + (i % 9) as f32 * 30.0,
                }
            } else {
                ThinFilmFactors::OFF
            },
            ..LabMaterial::EMPTY
        })
        .collect()
}

fn scene<'a>(materials: &'a [LabMaterial], cube: &'a (Vec<LabVertex>, Vec<u32>)) -> FrameScene<'a> {
    let meshes: Vec<FrameMesh<'a>> = materials
        .iter()
        .enumerate()
        .map(|(i, material)| {
            let (col, row) = ((i % 25) as f32, (i / 25) as f32);
            FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1000 + i as u64,
                object_key: 5000 + i as u64,
                vertices: cube.0.as_slice(),
                indices: cube.1.as_slice(),
                material,
                model: Mat4::from_translation(Vec3::new(-6.0 + col * 0.5, -4.75 + row * 0.5, 0.0))
                    .to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        })
        .collect();
    FrameScene {
        width: 200,
        height: 160,
        eye: [0.0, 0.0, 14.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([-0.35, -0.5, -0.8], [1.0, 0.98, 0.95], 20_000.0),
        local_lights: &[],
        quality: FrameQuality {
            taau: false,
            ..FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh))
        },
        meshes,
    }
}

#[derive(Debug)]
struct Cost {
    allocs: f64,
    upload_bytes: u64,
    creates: u64,
    opaque: regia_render::PassDrawCounts,
    film_drawn: usize,
}

fn measure(device: &RenderDevice, film: bool) -> Cost {
    let materials = materials(film);
    let cube = box_mesh([0.18, 0.18, 0.18]);
    let scene = scene(&materials, &cube);
    let mut renderer = FrameRenderer::new();
    for _ in 0..4 {
        renderer.render_offscreen(device, &scene).expect("warm frame");
    }
    let start = frame_path_counters();
    let before = allocs();
    for _ in 0..BATCH {
        renderer.render_offscreen(device, &scene).expect("steady frame");
    }
    let per_frame = (allocs() - before) as f64 / BATCH as f64;
    let c = frame_path_counters().since(start);
    Cost {
        allocs: per_frame,
        upload_bytes: c.upload_bytes / BATCH,
        creates: c.pipelines + c.bind_groups + c.buffers + c.textures,
        opaque: renderer.draw_counts().opaque,
        film_drawn: renderer.thin_film_stats().drawn,
    }
}

/// The first eight frames of `trace` outside the allocator, the standard library and this test:
/// where an allocation came from.
fn site(trace: &Backtrace) -> String {
    let text = trace.to_string();
    let skip = [
        "std::",
        "alloc::",
        "core::",
        "thin_film_cost::",
        "__rust",
        "<alloc::",
        "<core::",
        "<std::",
        "__rdl",
    ];
    text.lines()
        .map(str::trim)
        .filter_map(|line| line.split_once(": ").map(|(_, symbol)| symbol))
        .filter(|symbol| !symbol.starts_with("at "))
        .filter(|symbol| !matches!(*symbol, "alloc" | "realloc" | "dealloc" | "<unknown>"))
        .filter(|symbol| !skip.iter().any(|prefix| symbol.starts_with(prefix)))
        .take(8)
        .collect::<Vec<_>>()
        .join(" <- ")
}

/// ADR-3373 diagnostics: where a steady frame's heap allocations come from, by call site, for the
/// plain and the filmed frame. On Metal both frames allocate several times what they do on
/// Vulkan (1873 and 2314 per frame on the M5 against about 300 on lavapipe); this names the sites
/// so the extra count can be attributed before any per-backend budget.
/// `cargo test -p regia_render --test thin_film_cost -- --ignored --nocapture allocation_sites`.
#[test]
#[ignore = "diagnostics: prints a steady frame's allocation sites, asserts nothing"]
fn allocation_sites_of_a_steady_frame() {
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    let info = device.adapter.get_info();
    println!("adapter {} ({:?})", info.name, info.backend);
    for film in [false, true] {
        let materials = materials(film);
        let cube = box_mesh([0.18, 0.18, 0.18]);
        let scene = scene(&materials, &cube);
        let mut renderer = FrameRenderer::new();
        for _ in 0..4 {
            renderer.render_offscreen(&device, &scene).expect("warm frame");
        }
        if let Ok(mut sites) = SITES.lock() {
            sites.clear();
        }
        RECORD.store(true, Ordering::SeqCst);
        renderer.render_offscreen(&device, &scene).expect("steady frame");
        RECORD.store(false, Ordering::SeqCst);
        let traces = SITES
            .lock()
            .map(|mut sites| std::mem::take(&mut *sites))
            .unwrap_or_default();
        let mut by_site: BTreeMap<(String, String), usize> = BTreeMap::new();
        for (thread, trace) in &traces {
            *by_site.entry((thread.clone(), site(trace))).or_default() += 1;
        }
        let mut sorted: Vec<_> = by_site.into_iter().collect();
        sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        println!("film {film}: {} allocations in one steady frame", traces.len());
        for ((thread, stack), n) in sorted.iter().take(25) {
            println!("  {n:5}  [{thread}]  {stack}");
        }
    }
}

#[test]
fn a_steady_filmed_frame_of_500_materials_creates_and_uploads_nothing_per_material() {
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    let plain = measure(&device, false);
    let film = measure(&device, true);
    eprintln!("500 materials, plain: {plain:?}");
    eprintln!("500 materials, film:  {film:?}");
    assert_eq!(
        film.film_drawn, MATERIALS,
        "every filmed draw shades with the film"
    );
    assert_eq!(
        (plain.creates, film.creates),
        (0, 0),
        "steady frames create nothing"
    );
    // The film blocks and the object uniforms are uploaded when they change: a steady frame
    // writes none of them.
    assert_eq!(
        film.upload_bytes, plain.upload_bytes,
        "a steady filmed frame uploads per material"
    );
    // No allocation per material or per draw (measured 262 film, 302 plain on lavapipe: the
    // filmed frame skips the GPU-driven colour batches).
    assert!(
        film.allocs <= plain.allocs + MATERIALS as f64 / 10.0,
        "a filmed frame allocates {} per frame, the plain one {}",
        film.allocs,
        plain.allocs
    );
    // Both draw one colour draw per material; the plain frame binds one object group for all
    // of them (the GPU-driven batches).
    assert_eq!((plain.opaque.draws, film.opaque.draws), (500, 500));
    assert_eq!(plain.opaque.bind_group_switches[1], 1);
}

/// What ADR-2482 measured as the thin film's CPU cost: every filmed draw takes the CPU colour loop
/// (ADR-1977), so it binds its own object group (group 1) and the material's film group
/// (group 3), 1000 more bind-group switches on this frame than the plain one. The fix is a frame
/// change (filmed batches on the GPU-driven path, written up in ADR-2482 for the frame owner);
/// this test is its acceptance check and fails until then.
#[test]
#[ignore = "needs the frame change in ADR-2482 (filmed batches on the GPU-driven colour path)"]
fn filmed_draws_ride_the_gpu_driven_batches() {
    let device = RenderDevice::headless().expect("no headless GPU (lavapipe?)");
    let film = measure(&device, true);
    assert_eq!(film.film_drawn, MATERIALS);
    assert_eq!(
        film.opaque.bind_group_switches[1], 1,
        "filmed draws bind one object group each: {:?}",
        film.opaque
    );
}
