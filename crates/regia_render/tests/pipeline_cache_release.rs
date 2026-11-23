// SPDX-License-Identifier: MPL-2.0
//! The persistent pipeline cache's install ends with the last clone of the [`RenderDevice`] that
//! made it, so the cache (which holds the device) never keeps a Vulkan device alive into the
//! process's exit handlers (`pipeline_cache` module docs). One test in its own binary: the
//! install slot is process-wide.

use regia_render::RenderDevice;
use regia_render::pipeline_cache;

#[test]
fn install_ends_with_the_last_device_clone() {
    let dir = std::env::temp_dir().join(format!("regia-fpc-release-{}", std::process::id()));
    pipeline_cache::set_directory(Some(dir.clone()));
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    pipeline_cache::set_directory(None);
    if !device.device.features().contains(wgpu::Features::PIPELINE_CACHE) {
        eprintln!("adapter has no PIPELINE_CACHE; nothing to release");
        return;
    }
    assert!(
        pipeline_cache::installed().is_some(),
        "a cache directory installs a cache"
    );
    assert!(pipeline_cache::cache_for(&device).is_some());

    let clone = device.clone();
    drop(device);
    assert!(
        pipeline_cache::installed().is_some(),
        "a live clone keeps the install"
    );
    // A descriptor's borrow outlives the install without dangling.
    let held = pipeline_cache::cache_for(&clone);
    let raw = clone.device.clone();
    drop(clone);
    assert!(
        pipeline_cache::installed().is_none(),
        "the last clone ends the install"
    );
    assert!(held.as_deref().is_some());
    drop(held);
    assert!(pipeline_cache::cache_for(&raw).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
