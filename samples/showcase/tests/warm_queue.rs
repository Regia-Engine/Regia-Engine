// SPDX-License-Identifier: MIT-0
//! The windowed demo builds every pipeline on the warm queue (ADR-5131, ledger D03).
//!
//! The first tests need no GPU: the demo's loading screen (a boot splash with no cap, the warm terrain build, the
//! first-frame warm's limits) and when it may finish. The last two run the headless app on the headless device
//! (lavapipe on the CI hosts): with [`WarmTerrainBuild`] the terrain draw and all its pipelines are built by a
//! map-tier warm job, so no pipeline is created outside the queue in `regia_terrain`; without it (the inline build of
//! the headless app and filming runs) the same terrain creates them on the calling thread. Its own test binary: the
//! pipeline counters are per process, so the GPU tests take turns.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia::prelude::*;
use regia_render::{BootSplash, GameFrameWarm, PipelineWarmQueue, RenderDevice, WarmTier};
use showcase::app::{Options, ShowcaseStats, WarmTerrainBuild};
use showcase::demo::{
    LOADING_TEXT, LOADING_WARM_MAX, LOADING_WARM_PROGRESS_GRACE, configure_loading, loading_done,
};
use showcase::scene::{SceneData, load_scene};

/// One GPU test at a time (the pipeline counters are process-wide).
static SERIAL: Mutex<()> = Mutex::new(());

fn example() -> Arc<SceneData> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene");
    Arc::new(load_scene(&dir).expect("example scene"))
}

#[test]
fn the_windowed_demo_loads_behind_a_splash_without_a_cap() {
    let mut app = App::new();
    app.init_resource::<GameFrameWarm>();
    configure_loading(&mut app);
    let splash = app.world().resource::<BootSplash>();
    assert_eq!(splash.text, LOADING_TEXT);
    assert_eq!(
        splash.max_duration, None,
        "the splash waits for the first frame's warm"
    );
    assert!(!splash.is_finishing());
    assert!(app.world().contains_resource::<WarmTerrainBuild>());
    let warm = app.world().resource::<GameFrameWarm>();
    assert!(warm.on_splash_finish);
    assert!(
        warm.timeout >= LOADING_WARM_PROGRESS_GRACE,
        "a queued warm is not canceled at 15 s"
    );
    assert!(warm.max_timeout >= LOADING_WARM_MAX);
    assert!(warm.progress_grace >= LOADING_WARM_PROGRESS_GRACE);
    assert!(LOADING_WARM_MAX > GameFrameWarm::default().max_timeout);
}

/// fixrev-demo-warm-k (review finding 7): `configure_loading` changed `GameFrameWarm` only when the resource
/// already existed (`RenderPlugin` inits it when `run_with` adds the default plugins first). Called before that,
/// it silently left the first-frame warm on the engine's limits. It now inits the resource itself; a later
/// `init_resource` (the render plugin) keeps the loading screen's values.
#[test]
fn the_loading_screen_sets_its_first_frame_warm_limits_before_the_render_plugin() {
    let mut app = App::new();
    configure_loading(&mut app);
    let Some(warm) = app.world().get_resource::<GameFrameWarm>() else {
        panic!("configure_loading inits GameFrameWarm when nothing did yet");
    };
    assert!(warm.on_splash_finish);
    assert!(warm.timeout >= LOADING_WARM_PROGRESS_GRACE);
    assert!(warm.max_timeout >= LOADING_WARM_MAX);
    assert!(warm.progress_grace >= LOADING_WARM_PROGRESS_GRACE);
    app.init_resource::<GameFrameWarm>();
    let warm = app.world().resource::<GameFrameWarm>();
    assert!(
        warm.max_timeout >= LOADING_WARM_MAX,
        "the render plugin's init keeps them"
    );
}

#[test]
fn the_loading_screen_waits_for_the_terrain_and_the_boot_tier() {
    let mut stats = ShowcaseStats::default();
    assert!(!loading_done(&stats, None), "no terrain yet");
    stats.terrain_installed = true;
    assert!(loading_done(&stats, None));
    let queue = PipelineWarmQueue::new(1);
    queue.push(WarmTier::Boot, "pending", |_: &RenderDevice| Ok(()));
    assert!(
        !loading_done(&stats, Some(&queue)),
        "the boot tier is still queued"
    );
    let failed = ShowcaseStats {
        terrain_error: Some("terrain: broken".into()),
        ..ShowcaseStats::default()
    };
    assert!(
        loading_done(&failed, None),
        "a failed terrain does not hold the demo forever"
    );
}

/// Outside-the-queue pipeline creations at `regia_terrain` sites so far.
fn terrain_outside(queue: &PipelineWarmQueue) -> u32 {
    queue
        .progress()
        .outside_sites
        .iter()
        .filter(|(site, _)| site.contains("regia_terrain"))
        .map(|(_, n)| n)
        .sum()
}

/// The headless app on the example scene, or `None` without any adapter.
fn app(warm: bool) -> Option<App> {
    let options = Options {
        size: (160, 90),
        plain: true,
        scatter_lod: 2,
        ..Options::default()
    };
    let mut app = match showcase::app::build_headless_app(example(), options, None) {
        Ok(app) => app,
        Err(err) => {
            eprintln!("note: no GPU adapter ({err}); the warm-queue test is skipped");
            return None;
        }
    };
    if warm {
        app.init_resource::<WarmTerrainBuild>();
    }
    app.finish_plugins();
    Some(app)
}

fn installed(app: &App) -> bool {
    let stats = app.world().resource::<ShowcaseStats>();
    assert!(stats.terrain_error.is_none(), "{:?}", stats.terrain_error);
    stats.terrain_installed
}

#[test]
fn the_warm_terrain_build_creates_no_terrain_pipeline_outside_the_queue() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(mut app) = app(true) else {
        return;
    };
    let device = app.world().resource::<RenderDevice>().clone();
    let queue = app.world().resource::<PipelineWarmQueue>().clone();
    queue.start(&device);
    let outside = terrain_outside(&queue);
    let map_before = queue.progress().tiers[WarmTier::Map as usize].pipelines;
    // The first updates queue the job (the renderer and the device exist from the first one).
    for _ in 0..3 {
        app.update();
    }
    assert!(app.world().resource::<WarmTerrainBuild>().is_queued());
    assert!(
        queue.wait_tier(WarmTier::Map, Duration::from_secs(900)),
        "map tier"
    );
    let warm = queue.progress();
    assert!(warm.last_error.is_none(), "{:?}", warm.last_error);
    app.update();
    assert!(installed(&app), "installed on the update after the job");
    let built = queue.progress().tiers[WarmTier::Map as usize].pipelines - map_before;
    println!(
        "terrain job: {built} pipelines on the warm worker; outside sites {:?}",
        queue.progress().outside_sites
    );
    assert!(built >= 10, "the job built the terrain sets ({built})");
    assert_eq!(
        terrain_outside(&queue),
        outside,
        "{:?}",
        queue.progress().outside_sites
    );
    queue.shutdown();
}

#[test]
fn the_inline_build_creates_the_terrain_pipelines_on_the_calling_thread() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(mut app) = app(false) else {
        return;
    };
    let queue = app.world().resource::<PipelineWarmQueue>().clone();
    let outside = terrain_outside(&queue);
    app.update();
    assert!(installed(&app), "built on the first update");
    assert!(
        terrain_outside(&queue) > outside,
        "the counter sees the inline build: {:?}",
        queue.progress().outside_sites
    );
}
