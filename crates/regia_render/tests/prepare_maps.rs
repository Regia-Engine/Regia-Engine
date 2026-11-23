// SPDX-License-Identifier: MPL-2.0
//! Install prepare of a game's maps (ADR-8453), end to end, in its own process.
//!
//! A two-map fixture: map `a` is the canonical lit frame, map `b` adds what the canonical frame
//! never draws (a double-sided blended material, a transmissive one, height fog). The prepare
//! runs on its own headless device (the `regia prepare` path) into a fresh cache directory and
//! saves the driver cache. Then a second device, like the game's next launch, loads that cache,
//! warms map `b` on a warm worker the way the game-frame warm does and renders 300 frames of it.
//! Nothing it compiles may be new to the driver cache: every pipeline the game creates is one the
//! prepare created (same label, colour and depth formats, sample count and creation site: the same
//! descriptor, a driver-cache hit), and the 300 frames create no pipeline outside the warm workers.
//! (The driver cache's own payload cannot show hits here: lavapipe stores next to nothing in it.)
//!
//! The host status API is read before, during and after the prepare (needed -> running ->
//! done), and the stamp is checked against a changed map list.
//!
//! Process-global state (the cache directory, the installed cache, the warm counters): one test
//! in its own binary.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use regia_render::pipeline_cache::{self, CacheLoad};
use regia_render::pipeline_warm::{PipelineWarmQueue, WarmTier, canonical_frame};
use regia_render::prepare::{self, HostPrepareStatus, PrepareMap, PrepareRequest, PrepareState};
use regia_render::{ALPHA_BLEND, CpuMaterial, ExtractedFrame, FrameRenderer, FrameTarget, RenderDevice};

/// The game's window format in this test.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// The game's window size in this test.
const SIZE: (u32, u32) = (96, 54);
/// Frames of map `b` after its warm.
const FRAMES: u32 = 300;

/// Map `b`: the canonical frame plus a double-sided blended quad, a transmissive quad and fog.
fn map_b() -> ExtractedFrame {
    let mut frame = canonical_frame();
    let base = frame.meshes[0].clone();
    let blended = CpuMaterial {
        alpha_mode: ALPHA_BLEND,
        double_sided: true,
        base_color: [0.2, 0.6, 0.9, 0.5],
        ..CpuMaterial::default()
    };
    let glass = CpuMaterial {
        transmission: 0.8,
        ior: 1.5,
        ..CpuMaterial::default()
    };
    for (offset, material) in [(1.5_f32, blended), (-1.5, glass)] {
        let mut mesh = base.clone();
        mesh.material = std::sync::Arc::new(material);
        mesh.model[3][0] = offset;
        frame.meshes.push(mesh);
    }
    frame.fog.enabled = true;
    frame.fog.density = 0.02;
    frame
}

/// Calls of map a's host job (one per window format).
static JOB_CALLS: AtomicUsize = AtomicUsize::new(0);

fn maps(b_fingerprint: &str) -> Vec<PrepareMap> {
    vec![
        PrepareMap::new("a", "fixture-a-1")
            .with_frame(canonical_frame())
            .with_job("host", |_device, _format| {
                JOB_CALLS.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }),
        PrepareMap::new("b", b_fingerprint).with_frame(map_b()),
    ]
}

/// What makes two creations the same pipeline here: label, colour and depth formats, samples and
/// the creation site (the thread that built it and the renderer's key history are left out).
type Identity = (String, String, String, u32, String);

fn identities(log: Vec<(String, regia_render::CreateDetail)>) -> std::collections::BTreeSet<Identity> {
    log.into_iter()
        .map(|(label, detail)| {
            let site = detail
                .site
                .map_or_else(String::new, |site| format!("{}:{}", site.file(), site.line()));
            (
                label,
                format!("{:?}", detail.format),
                format!("{:?}", detail.depth_format),
                detail.samples,
                site,
            )
        })
        .collect()
}

fn format_key(key: &Identity) -> String {
    format!("{} {}/{} x{} at {}", key.0, key.1, key.2, key.3, key.4)
}

/// The installed cache's payload when it has creations it has not saved (`None` otherwise).
fn installed_payload_len() -> Option<usize> {
    pipeline_cache::snapshot_installed().map(|pending| pending.len())
}

fn outside() -> u64 {
    // The counter is process-wide; any queue reports it.
    PipelineWarmQueue::new(1).progress().pipelines_outside
}

#[test]
fn prepared_maps_load_from_cache_and_the_status_walks_needed_to_done() {
    #[allow(unsafe_code)]
    // SAFETY: one test in its own binary, before any warm worker or driver exists.
    unsafe {
        for name in [
            "REGIA_PIPELINE_CACHE_DIR",
            "FERRITE_PIPELINE_CACHE_DIR",
            "REGIA_PIPELINE_CACHE",
            "FERRITE_PIPELINE_CACHE",
            "REGIA_SHIPPED_PIPELINE_CACHE_DIR",
            "FERRITE_SHIPPED_PIPELINE_CACHE_DIR",
        ] {
            std::env::remove_var(name);
        }
    }
    pipeline_cache::set_shipped_directory(None);
    let dir = regia_test_utils::unique_temp_dir("prepare-maps");
    std::fs::create_dir_all(&dir).expect("temp dir");

    // The adapter, from a device that installs no cache (no directory set yet).
    let adapter = {
        let probe = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
        if !probe.adapter.features().contains(wgpu::Features::PIPELINE_CACHE) {
            eprintln!("adapter has no PIPELINE_CACHE; a driver cache cannot carry the prepare");
            return;
        }
        probe.adapter.get_info()
    };

    let mut request = PrepareRequest::new(dir.clone());
    request.nice = false;
    request.timeout = Duration::from_secs(900);
    request.maps = maps("fixture-b-1");

    // (c) Before: needed, no status yet.
    let before = prepare::host_status(&request, &adapter).expect("status before");
    assert_eq!(
        before,
        HostPrepareStatus {
            needed: true,
            state: None,
            done: 0,
            total: 0,
            eta: None,
        },
        "a fresh cache directory needs a prepare"
    );

    // (c) During: the host polls while the warm runs.
    let mut during: Vec<HostPrepareStatus> = Vec::new();
    let mut during_phases: Vec<String> = Vec::new();
    regia_render::log_pipeline_creates(true);
    let report = prepare::run(&request, |progress| {
        if progress.state == PrepareState::Running {
            during.push(prepare::host_status(&request, &adapter).expect("status during"));
            during_phases.push(progress.phase.clone());
        }
    })
    .unwrap_or_else(|err| panic!("prepare failed: {err}"));
    let prepared = identities(regia_render::take_pipeline_creates());
    assert!(!report.skipped);
    assert_eq!(
        JOB_CALLS.load(Ordering::Relaxed),
        2,
        "map a's host job runs once per window format"
    );
    assert!(report.worker_ms > 0, "the warm workers did the work: {report:?}");
    for unit in ["map:a:bgra8-srgb", "map:b:rgba8-srgb", "maps:"] {
        assert!(
            report.units.contains(unit),
            "unit {unit} missing: {}",
            report.units
        );
    }
    let running = during.first().expect("a status read while running");
    assert!(running.needed, "{running:?}");
    assert_eq!(running.state, Some(PrepareState::Running), "{running:?}");
    assert!(running.total >= 4, "two maps x two window formats: {running:?}");

    // The agent's `render.pipeline_warm` → `prepare` reads this.
    let (last, _elapsed) = prepare::last_progress().expect("a prepare ran in this process");
    assert_eq!(last.state, PrepareState::Ready);
    assert_eq!(last.phase, "done");
    assert!(
        during_phases
            .iter()
            .any(|phase| phase == "map:a" || phase == "map:b"),
        "the running map names the phase: {during_phases:?}"
    );

    // (c) After: done.
    let after = prepare::host_status(&request, &adapter).expect("status after");
    assert!(!after.needed, "{after:?}");
    assert_eq!(after.state, Some(PrepareState::Ready));
    assert_eq!(after.done, after.total);
    assert!(after.total >= 4, "{after:?}");
    assert_eq!(after.fraction(), 1.0);

    // (b) The stamp covers the map list: a changed map or a shorter list is stale.
    assert!(prepare::stamp_is_current(&request).expect("check"));
    let mut changed = request.clone();
    changed.maps = maps("fixture-b-2");
    assert!(
        !prepare::stamp_is_current(&changed).expect("check changed"),
        "map b's content changed: the stamp must be stale"
    );
    let mut shorter = request.clone();
    shorter.maps.truncate(1);
    assert!(
        !prepare::stamp_is_current(&shorter).expect("check shorter"),
        "the map list changed: the stamp must be stale"
    );
    assert!(prepare::host_status(&changed, &adapter).expect("status").needed);

    // (a) The game's next launch: a new device loads the prepared cache.
    let device = RenderDevice::headless().expect("game device");
    assert!(
        matches!(pipeline_cache::status().load, Some(CacheLoad::Loaded(_))),
        "the game device must load the prepared cache: {:?}",
        pipeline_cache::status()
    );
    let loaded = installed_payload_len();
    // The game's creations from here: its warm of map b and its 300 frames.
    let _ = regia_render::take_pipeline_creates();

    // The game-frame warm of map b, on a warm worker, as `GameFrameWarm` runs it.
    let queue = PipelineWarmQueue::new(2);
    let (send, receive) = mpsc::channel();
    let frame = map_b();
    let warm_frame = frame.clone();
    queue.push(WarmTier::Map, "game warm b", move |device: &RenderDevice| {
        let mut renderer = FrameRenderer::new();
        renderer
            .warm_extracted_at(device, &warm_frame, FORMAT, false, Some(SIZE))
            .map_err(|e| e.to_string())?;
        renderer.settle_material_graphs();
        send.send(renderer).map_err(|e| e.to_string())
    });
    queue.start(&device);
    assert!(
        queue.wait_tier(WarmTier::Map, Duration::from_secs(900)),
        "the game warm finished: {}",
        queue.describe()
    );
    let mut renderer = receive
        .recv_timeout(Duration::from_secs(60))
        .expect("the warmed renderer");
    regia_render::pipeline_warm::release_deferred();

    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("prepare maps window"),
        size: wgpu::Extent3d {
            width: SIZE.0,
            height: SIZE.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let outside_before = outside();
    for _ in 0..FRAMES {
        let scene = frame.frame_scene(0, SIZE.0, SIZE.1);
        renderer
            .render_into(&device, &scene, FrameTarget::full(&view, FORMAT, SIZE.0, SIZE.1))
            .expect("frame");
        device.poll_nonblocking().expect("poll");
    }
    device.wait_idle().expect("idle");
    // Deferred variants the first frames released build on the workers; wait for them too.
    for _ in 0..600 {
        if regia_render::pipeline_warm::deferred_pending() == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let outside_after = outside();
    let grown = installed_payload_len();
    let game = identities(regia_render::take_pipeline_creates());
    regia_render::log_pipeline_creates(false);
    queue.shutdown();
    eprintln!(
        "prepare: {} distinct pipelines; game: {} distinct; driver cache payload {loaded:?} -> {grown:?} bytes",
        prepared.len(),
        game.len()
    );
    assert_eq!(
        outside_after - outside_before,
        0,
        "map b's {FRAMES} frames created pipelines outside the warm workers"
    );
    assert!(
        !game.is_empty(),
        "the game's warm created its pipelines (from the cache)"
    );
    let missed: Vec<String> = game.difference(&prepared).map(format_key).collect();
    assert!(
        missed.is_empty(),
        "loading map b compiled {} pipelines the prepare never built (driver-cache misses):\n{}",
        missed.len(),
        missed.join("\n")
    );

    drop(renderer);
    drop(device);
    let _ = std::fs::remove_dir_all(&dir);
}
