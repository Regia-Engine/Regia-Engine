// SPDX-License-Identifier: MPL-2.0
//! The first-game-frame pipeline cache checkpoint (ADR-5857): `save_installed_in_background`
//! writes the installed cache from its own thread, off the frame path, so a first launch closed
//! before the warm queue drains keeps the pipelines it compiled. A second request with nothing new
//! writes nothing; without an installed cache nothing starts. One test in its own binary: the cache
//! install is process-wide.

use regia_render::pipeline_cache::{self, CacheLoad};
use regia_render::pipeline_warm::canonical_frame;
use regia_render::{FrameRenderer, RenderDevice};

#[test]
fn first_frame_checkpoint_writes_the_cache_from_a_background_thread() {
    // A directory from the environment would win over the one this test sets.
    #[allow(unsafe_code)]
    // SAFETY: single test in its own binary, before any warm worker or driver exists.
    unsafe {
        std::env::remove_var("REGIA_PIPELINE_CACHE_DIR");
        std::env::remove_var("FERRITE_PIPELINE_CACHE_DIR");
        std::env::remove_var("REGIA_PIPELINE_CACHE");
        std::env::remove_var("FERRITE_PIPELINE_CACHE");
        std::env::remove_var("REGIA_SHIPPED_PIPELINE_CACHE_DIR");
        std::env::remove_var("FERRITE_SHIPPED_PIPELINE_CACHE_DIR");
    }
    assert!(
        pipeline_cache::save_installed_in_background("test").is_none(),
        "no installed cache: nothing starts"
    );
    let dir = regia_test_utils::unique_temp_dir("first-frame-cache-checkpoint");
    pipeline_cache::set_directory(Some(dir.clone()));
    pipeline_cache::set_shipped_directory(None);
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    if !device.device.features().contains(wgpu::Features::PIPELINE_CACHE) {
        eprintln!("adapter has no PIPELINE_CACHE; the checkpoint does not apply");
        return;
    }
    let path = pipeline_cache::installed()
        .expect("a cache directory installs a cache")
        .path()
        .to_path_buf();
    assert_eq!(pipeline_cache::status().load, Some(CacheLoad::Missing));
    assert_eq!(pipeline_cache::status().saves, 0);
    assert!(!path.exists(), "a fresh directory has no cache file");

    // What the first frame compiles.
    let frame = canonical_frame();
    let mut renderer = FrameRenderer::new();
    renderer
        .warm_extracted(&device, &frame, wgpu::TextureFormat::Rgba8Unorm, true)
        .expect("warm");
    // Graph pipelines still building on workers would land between the two saves below.
    renderer.settle_material_graphs();
    let created = pipeline_cache::pipelines_created();
    assert!(created > 0, "the warm compiled pipelines");

    let handle =
        pipeline_cache::save_installed_in_background("first_game_frame").expect("a save thread starts");
    assert_eq!(
        handle.thread().name(),
        Some("regia-pcache-save"),
        "the save runs on its own thread, not the caller's"
    );
    handle.join().expect("the save thread does not panic");
    let status = pipeline_cache::status();
    assert_eq!(status.saves, 1, "{status:?}");
    assert!(status.saved_bytes > 0, "{status:?}");
    let written = std::fs::metadata(&path)
        .expect("the checkpoint wrote the cache file")
        .len();
    assert_eq!(written, status.saved_bytes);

    // Nothing compiled since: a second checkpoint writes nothing.
    pipeline_cache::save_installed_in_background("again")
        .expect("a save thread starts")
        .join()
        .expect("no panic");
    assert_eq!(
        pipeline_cache::pipelines_created(),
        created,
        "nothing compiled in between"
    );
    assert_eq!(pipeline_cache::status().saves, 1);

    drop(renderer);
    drop(device);
    assert!(
        pipeline_cache::save_installed_in_background("after drop").is_none(),
        "the device's drop ends the install: nothing starts"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
