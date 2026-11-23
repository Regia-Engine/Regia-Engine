// SPDX-License-Identifier: MPL-2.0
//! Emissive fits keyed by mesh content, new fits off the frame thread (v0.1.18, ADR-225).
//!
//! DO27 v0.1.1 (Regia Engine v0.1.16) logged 60–598 ms frames while shooting hulls, all inside
//! `emissive_lights` > `emissive_probes`: each damage step landed the same intact mesh again as
//! a new `Arc<CpuMesh>`, and the renderer fitted and traced its emissive strips again on the
//! frame thread. A synthetic scene on lavapipe (no DO27 content): a ground, an emissive lamp
//! (so the first frame is the load frame), and
//!
//! * a hull strip landed under 100 fresh `Arc`s on one object, as the runtime keys meshes (the
//!   generation id [`regia_render::runtime::mesh_key`] gives each `Arc`, ADR-4783) and lends them
//!   ([`FrameRenderer::hold_mesh_identities`]): one fit and one probe trace in total, and the same
//!   lights every frame;
//! * a large new emissive mesh past the load frame: the frame it lands spends less than
//!   [`EMISSIVE_FRAME_BUDGET_MS`] in the emissive collection, as does every frame until its
//!   lights land from the worker.

use std::sync::Arc;
use std::time::{Duration, Instant};

use regia_core::glam::{Mat4, Vec3};
use regia_render::emissive_lights::EMISSIVE_FRAME_BUDGET_MS;
use regia_render::{
    CpuMesh, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameTarget, LOD_DITHER_NONE,
    LabMaterial, LabMesh, LabScene, RenderDevice, ShadowCast,
};

const W: u32 = 96;
const H: u32 = 64;
/// Object key of the hull whose emissive strip lands again and again.
const HULL: u64 = 0x4855_4c4c;
/// Material key of the extra meshes' emissive material.
const STRIP_MATERIAL: u64 = 0x5354_5249;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn emissive() -> LabMaterial {
    LabMaterial {
        base_color: [0.05, 0.05, 0.05, 1.0],
        emissive: [1.0, 0.6, 0.3],
        emissive_intensity: 2000.0,
        roughness: 0.8,
        ..LabMaterial::default()
    }
}

fn boxed(center: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = regia_render::box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: Mat4::from_translation(Vec3::from_array(center)).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

/// A ground and an emissive lamp under a low sun.
fn scene() -> LabScene {
    let ground = LabMaterial {
        base_color: [0.35, 0.33, 0.3, 1.0],
        roughness: 0.8,
        ..LabMaterial::default()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 2.0, 6.0],
        target: [0.0, 0.6, 0.0],
        fov_y: 1.0,
        near: 0.05,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 10.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([-0.3, -0.6, -0.75], [1.0, 0.97, 0.92], 2_000.0),
        local_lights: Vec::new(),
        materials: vec![ground, emissive()],
        meshes: vec![
            boxed([0.0, -0.05, 0.0], [20.0, 0.05, 20.0], 0),
            boxed([-2.0, 0.6, -1.0], [0.4, 0.04, 0.04], 1),
        ],
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::LAB;
    q.emissive_lights = 64;
    q
}

/// One frame of `lab` plus `extra` meshes (mesh, object key, position) keyed by their `Arc`'s
/// generation id and lent to the renderer, as the runtime does (the lent mesh's pin attaches only
/// under that key, ADR-4783).
///
/// The frame goes through the live path ([`FrameRenderer::render_into`] into the persistent
/// offscreen target), as a game frame does. `render_offscreen` is a capture: since the audit
/// fix-render4 #3 it runs emissive fits complete on the frame thread (goldens must not depend on
/// the wall clock), so it cannot show what a live frame pays.
fn render(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    lab: &LabScene,
    material: &LabMaterial,
    extra: &[(&Arc<CpuMesh>, u64, Vec3)],
) {
    let mut frame = lab.as_frame_scene_with_quality(quality());
    for &(mesh, object, at) in extra {
        frame.meshes.push(FrameMesh {
            is_static: false,
            mesh_key: regia_render::runtime::mesh_key(mesh),
            material_key: STRIP_MATERIAL,
            object_key: object,
            vertices: &mesh.vertices,
            indices: &mesh.indices,
            material,
            model: Mat4::from_translation(at).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: LOD_DITHER_NONE,
        });
    }
    renderer.hold_mesh_identities(extra.iter().map(|e| e.0));
    let target = renderer.offscreen_target(device, W, H, false);
    renderer
        .render_into(
            device,
            &frame,
            FrameTarget::full(&target.view, wgpu::TextureFormat::Rgba8Unorm, W, H),
        )
        .expect("frame");
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_same_hull_strip_landed_under_100_fresh_arcs_fits_and_traces_once() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene();
    let material = emissive();
    let (vertices, indices) = regia_render::box_mesh([0.6, 0.03, 0.03]);
    let mut renderer = FrameRenderer::new();
    let mut traced = Vec::new();
    let mut uploaded = Vec::new();
    for _ in 0..100 {
        // A damage step lands the intact strip again as a new `Arc`; the previous one is dropped
        // (its address may come back for a later mesh).
        let strip = Arc::new(CpuMesh {
            vertices: vertices.clone(),
            indices: indices.clone(),
        });
        render(
            &mut renderer,
            &device,
            &lab,
            &material,
            &[(&strip, HULL, Vec3::new(0.5, 0.8, 0.0))],
        );
        let report = renderer.emissive_lights();
        assert_eq!(report.meshes, 2, "{report:?}");
        traced.push(report.probes_traced);
        uploaded.push(report.uploaded);
    }
    let stats = renderer.emissive_fit_stats();
    eprintln!("probes traced per frame {traced:?}\nuploaded {uploaded:?}\n{stats:?}");
    // The lamp and the strip: two fits, ever; their probes traced in the first frame only.
    assert_eq!(stats.fits_done, 2, "{stats:?}");
    assert!(
        traced[0] >= 2 && traced[1..].iter().all(|&n| n == 0),
        "{traced:?}"
    );
    // A cache hit counts a mesh key that took already-fitted content. Every landing's `Arc` gets a
    // new generation id, so each of the 99 later landings is a new key; the check stays a lower
    // bound, as it was while the key was the `Arc` address (which the allocator reuses). What must
    // hold is that every hit
    // reused the lights and the probe trace (equal counts), that no landing re-fitted (two fits
    // above, one trace frame above), and that content hashing ran for each landing.
    assert!(stats.cache_hits > 0, "{stats:?}");
    assert_eq!(stats.cache_hits, stats.probe_hits, "{stats:?}");
    assert!(stats.hashes >= 100, "{stats:?}");
    // The strip's lights in every frame.
    assert!(
        uploaded.iter().all(|&n| n == uploaded[0] && n >= 2),
        "{uploaded:?}"
    );
}

/// Runs `attempt` up to `attempts` times and passes if any run's per-frame times (ms) all stay
/// under `budget_ms`, the first (landing) frame included. These are wall-time measurements: on a
/// loaded host a single preempted frame can exceed the budget by 20x while typical frames cost
/// 0.005-0.05 ms. A real regression (the landing frame paying the whole fit) costs far more than
/// the budget on every run, so it fails every attempt. Panics listing every attempt otherwise,
/// with each attempt's landing-frame cost and the host's load ([`budget_failure`]).
///
/// The bar is wall-clock on purpose (ADR-5908): a landing frame measured beside 60 compilers is
/// not the product's budget, so the merge gate runs this binary in its solo phase
/// (`docs/ci/flaky-tests.toml`, `emissive-hitch-wall-clock`) instead of loosening the bar.
fn assert_frames_under_budget_in_any_attempt(
    attempts: usize,
    budget_ms: f32,
    mut attempt: impl FnMut() -> Vec<f32>,
) {
    let mut runs = Vec::new();
    for _ in 0..attempts {
        let ms = attempt();
        let ok = !ms.is_empty() && ms.iter().all(|&m| m < budget_ms);
        runs.push(ms);
        if ok {
            return;
        }
    }
    panic!("{}", budget_failure(budget_ms, &runs, &host_load()));
}

/// The failure text of [`assert_frames_under_budget_in_any_attempt`]: the landing frame's cost
/// and the worst frame of each attempt, the host's load, then every frame time.
fn budget_failure(budget_ms: f32, runs: &[Vec<f32>], host: &str) -> String {
    let landing: Vec<String> = runs
        .iter()
        .map(|ms| {
            ms.first()
                .map_or_else(|| "none".to_string(), |m| format!("{m:.3} ms"))
        })
        .collect();
    let worst: Vec<String> = runs
        .iter()
        .map(|ms| format!("{:.3} ms", ms.iter().copied().fold(0.0_f32, f32::max)))
        .collect();
    format!(
        "no attempt kept every frame under {budget_ms} ms; landing frame per attempt: [{}]; worst frame per \
         attempt: [{}]; host load: {host}; per-attempt frame times: {runs:?}",
        landing.join(", "),
        worst.join(", ")
    )
}

/// The host's 1-, 5- and 15-minute load averages and its thread count: a wall-clock failure on a
/// host loaded several times over its threads says nothing about the engine.
fn host_load() -> String {
    let threads = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    match std::fs::read_to_string("/proc/loadavg") {
        Ok(text) => {
            let averages: Vec<&str> = text.split_whitespace().take(3).collect();
            format!("load average {} on {threads} threads", averages.join(" "))
        }
        Err(_) => format!("unknown (no /proc/loadavg) on {threads} threads"),
    }
}

/// One fresh renderer: lands a large emissive mesh and returns the emissive collection time of
/// each frame from the landing frame until its lights arrive. The structural assertions run on
/// every attempt.
fn landing_frame_times(device: &RenderDevice) -> Vec<f32> {
    let lab = scene();
    let mut material = emissive();
    // Review of fix-render8 finding 1 (restored by fixrev5 after merge 14c3159d3 dropped it):
    // the per-frame signature must not rehash a large emissive map byte at a time. A 1 MiB
    // map; the CPU-only guard is `material_signature_hashes_the_map_word_wise`.
    material.emissive_map = Some(regia_render::LabTexture {
        width: 512,
        height: 512,
        rgba: vec![255; 512 * 512 * 4],
    });
    let mut renderer = FrameRenderer::new();
    for _ in 0..2 {
        render(&mut renderer, device, &lab, &material, &[]);
    }
    let before = renderer.emissive_lights().uploaded;
    // 16 384 curved emitting triangles: the fit splits them into many lights.
    let (vertices, indices) = regia_render::uv_sphere(0.5, 128, 64);
    let lamp = Arc::new(CpuMesh { vertices, indices });
    let extra = [(&lamp, 77, Vec3::new(1.0, 1.0, 0.0))];
    let started = Instant::now();
    let mut ms = Vec::new();
    loop {
        render(&mut renderer, device, &lab, &material, &extra);
        let report = renderer.emissive_lights();
        ms.push(report.cpu_ms);
        if report.uploaded > before || started.elapsed() > Duration::from_secs(20) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let stats = renderer.emissive_fit_stats();
    let report = renderer.emissive_lights();
    eprintln!("emissive collection ms per frame from the landing frame: {ms:?}\n{report:?}\n{stats:?}");
    assert!(
        report.uploaded > before,
        "the lamp's lights never landed: {report:?} {stats:?}"
    );
    assert!(stats.background && stats.fits_deferred == 1, "{stats:?}");
    ms
}

/// The bound is wall-clock (ADR-5908): when it fails, the message must say what the landing
/// frame cost in each attempt and how loaded the host was, so a gate log tells a regression
/// (every landing frame far over the bar) from an oversubscribed host (a lavapipe pass beside 60
/// compilers). No GPU: the attempts are fixed numbers.
#[test]
fn a_budget_failure_names_the_landing_cost_and_the_host_load() {
    let mut attempts = 0;
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_frames_under_budget_in_any_attempt(2, 1.0, || {
            attempts += 1;
            vec![2.5, 0.25]
        });
    }));
    assert_eq!(attempts, 2, "every attempt runs before the failure");
    let Err(payload) = caught else {
        panic!("frames of 2.5 ms passed a 1 ms budget");
    };
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_default();
    assert!(
        message.contains("landing frame per attempt: [2.500 ms, 2.500 ms]"),
        "{message}"
    );
    assert!(message.contains("host load: "), "{message}");
    if cfg!(target_os = "linux") {
        // Linux always has /proc/loadavg: the numbers are there, not the fallback.
        assert!(
            message.contains("load average ") && !message.contains("unknown"),
            "{message}"
        );
    }
}

#[test]
fn a_large_new_emissive_mesh_costs_its_landing_frame_less_than_the_budget() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    assert_frames_under_budget_in_any_attempt(3, EMISSIVE_FRAME_BUDGET_MS, || landing_frame_times(&device));
}
