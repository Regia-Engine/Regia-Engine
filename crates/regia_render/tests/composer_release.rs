// SPDX-License-Identifier: MPL-2.0
//! The shader composer cache is dropped once composition goes quiet (ADR-3351).
//!
//! Before ADR-3351 every thread that composed a shader kept its own composer over the engine
//! library (the parsed library and every module it built, per shader-def set) for the life of the
//! process, so an idle app held it although nothing composed after start-up. Now the composers sit
//! in a pool that `RenderPlugin` drops after `COMPOSER_IDLE_FRAMES` app frames without a
//! composition. CPU only: no device.

regia_test_utils::install_tagging_allocator!();

use std::collections::HashMap;

use regia_app::prelude::*;

const OWNER: &str = "render.shader_modules";

const SOURCE: &str = r#"
#import regia::clustering
#import regia::mesh
#import regia::pbr::lighting

@fragment
fn main() -> @location(0) vec4<f32> {
    let n = regia::clustering::cluster_count();
    let d = regia::pbr::lighting::d_ggx(1.0, 0.5);
    return vec4<f32>(f32(n) * 0.0 + d * 0.0, 0.0, 0.0, 1.0);
}
"#;

fn held() -> u64 {
    regia_core::memory::owner_snapshot().live_bytes(OWNER)
}

/// The composer cache holds the parsed library after a composition; an app drops it after
/// `COMPOSER_IDLE_FRAMES` frames without one, keeps it while compositions continue, composes the
/// same module after the release, and a release from a thread that never composed reaches the
/// pool. One test: the pool is process-wide.
#[test]
fn an_idle_app_drops_the_composer_cache_and_composes_the_same_after() {
    // The system `RenderPlugin` adds, alone (the plugin's other systems need a device and assets).
    let mut app = App::new();
    app.add_systems(Last, regia_render::release_idle_composers);
    app.update();
    let before = held();
    let first = regia_render::compose_source(SOURCE, "composer_release.wgsl", HashMap::new())
        .expect("the probe composes");
    let first_text = format!("{first:?}");
    drop(first);
    let with_cache = held();
    let stats = regia_render::composer_cache_stats();
    assert!(stats.pooled >= 1, "{stats:?}");
    // The parsed library is far larger than this: the bound only proves the cache is counted.
    assert!(
        with_cache > before + 256 * 1024,
        "the composer cache holds {} B",
        with_cache.saturating_sub(before)
    );

    // Compositions every few frames keep it.
    for frame in 0..(2 * regia_render::COMPOSER_IDLE_FRAMES) {
        if frame % 10 == 0 {
            drop(regia_render::compose_source(
                SOURCE,
                "composer_release.wgsl",
                HashMap::new(),
            ));
        }
        app.update();
    }
    assert!(
        regia_render::composer_cache_stats().pooled >= 1,
        "kept while composing"
    );

    // Idle frames drop it.
    for _ in 0..=regia_render::COMPOSER_IDLE_FRAMES {
        app.update();
    }
    let after = regia_render::composer_cache_stats();
    assert_eq!(after.pooled, 0, "{after:?}");
    assert!(after.releases > stats.releases, "{after:?}");
    let idle = held();
    assert!(
        idle < before + 64 * 1024,
        "an idle app still holds {} B of composer cache ({} B before the release)",
        idle.saturating_sub(before),
        with_cache.saturating_sub(before)
    );

    // The next composition builds a composer again and gives the same module.
    let again = regia_render::compose_source(SOURCE, "composer_release.wgsl", HashMap::new())
        .expect("composes after the release");
    assert_eq!(format!("{again:?}"), first_text);
    assert!(regia_render::composer_cache_stats().builds > stats.builds);

    // A release from a thread that never composed reaches the pool.
    let released = std::thread::spawn(regia_render::release_composer_cache)
        .join()
        .expect("the release thread does not panic");
    assert!(released >= 1);
    assert_eq!(regia_render::composer_cache_stats().pooled, 0);
}
