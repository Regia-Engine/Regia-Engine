// SPDX-License-Identifier: MPL-2.0
//! The persistent pipeline cache serves the one device that opened it, also when other devices on
//! other wgpu instances live in the same process (ADR-2073).
//!
//! Each `wgpu::Instance` numbers its devices and caches on its own, and `wgpu::Device` equality
//! compares only that number, so the first device of every instance equals the first device of
//! every other. A cache handed to the wrong device is looked up in that device's own tables, where
//! wgpu panics with `Cannot get non-existent resource PipelineCacheId(0,1)`. Any process with a
//! cache directory and two headless devices hit this, on every adapter.
//!
//! Since ADR-7557 headless devices with one descriptor share one instance, where wgpu tells them
//! apart; the test opens the second instance with other instance flags (ADR-7643).
//!
//! One test in its own binary: the install slot and the cache directory are process-wide.

use regia_render::{RenderDevice, pipeline_cache};

/// The flags `RenderDevice::headless` gives its instance: a copy of the crate's private
/// `instance_flags` (device.rs), Android's `DEBUG` removal included. Keep the two in step.
fn instance_flags() -> wgpu::InstanceFlags {
    let mut flags = wgpu::InstanceFlags::from_build_config();
    if cfg!(target_os = "android") {
        flags.remove(wgpu::InstanceFlags::DEBUG);
    }
    flags.with_env()
}

const SHADER: &str = "@compute @workgroup_size(1) fn main() {}";

/// What every engine creation site does: passes `cache_for(device)` in the descriptor. wgpu panics
/// here when the cache is not this device's.
fn create_pipeline(device: &wgpu::Device) {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pipeline_cache_instances"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let _pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("pipeline_cache_instances"),
        layout: None,
        module: &module,
        entry_point: Some("main"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: pipeline_cache::cache_for(device).as_deref(),
    });
}

fn headless() -> RenderDevice {
    RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)")
}

#[test]
fn a_cache_serves_only_the_device_that_opened_it() {
    // A directory from the environment would win over the one this test sets.
    #[allow(unsafe_code)]
    // SAFETY: one test, before any warm worker or driver exists.
    unsafe {
        std::env::remove_var("REGIA_PIPELINE_CACHE_DIR");
        std::env::remove_var("FERRITE_PIPELINE_CACHE_DIR");
        std::env::remove_var("REGIA_PIPELINE_CACHE");
        std::env::remove_var("FERRITE_PIPELINE_CACHE");
        std::env::remove_var("REGIA_SHIPPED_PIPELINE_CACHE_DIR");
        std::env::remove_var("FERRITE_SHIPPED_PIPELINE_CACHE_DIR");
    }
    let dir = regia_test_utils::unique_temp_dir("fpc-instances");
    pipeline_cache::set_directory(Some(dir.clone()));

    let first = headless();
    if !first.device.features().contains(wgpu::Features::PIPELINE_CACHE) {
        eprintln!("adapter has no PIPELINE_CACHE; a cache cannot reach the wrong device");
        pipeline_cache::set_directory(None);
        return;
    }
    assert!(pipeline_cache::cache_for(&first).is_some());
    create_pipeline(&first.device);
    let instances = regia_render::instance_creation_count();

    // A second headless device leaves the cache with the live device that holds it.
    let second = headless();
    assert!(
        !second.device.features().contains(wgpu::Features::PIPELINE_CACHE),
        "a headless device takes no cache from a live device"
    );
    assert!(pipeline_cache::cache_for(&second).is_none());
    assert!(pipeline_cache::cache_for(&first).is_some());
    create_pipeline(&second.device);
    create_pipeline(&first.device);

    // Headless devices share one instance per descriptor (ADR-7557), so `first` and `second`
    // are two devices of one instance, and wgpu numbers them apart.
    assert_eq!(
        regia_render::instance_creation_count(),
        instances,
        "a second headless device reuses the first one's instance"
    );
    assert!(second.device != first.device);

    // The first device's handle outlives its install (a warm worker, a renderer) and a third
    // device on the same instance takes the install. wgpu numbers a device of one instance once
    // for the process (an id's epoch grows when its index is reused), so the outlived handle
    // and the third device differ even under `==`, and the handle's own cache is gone.
    let outlived = first.device.clone();
    let info = first.adapter.get_info();
    drop(first);
    drop(second);
    assert!(
        pipeline_cache::installed().is_none(),
        "the last clone ends the install"
    );
    let third = headless();
    assert_eq!(regia_render::instance_creation_count(), instances);
    assert!(pipeline_cache::cache_for(&third).is_some());
    assert!(
        outlived != third.device,
        "two live devices of one instance compare equal: wgpu reused a device id"
    );
    assert!(outlived.features().contains(wgpu::Features::PIPELINE_CACHE));
    assert!(pipeline_cache::cache_for(&outlived).is_none());
    create_pipeline(&outlived);
    create_pipeline(&third.device);
    drop(third);
    assert!(pipeline_cache::installed().is_none());

    // A device on another instance: other instance flags (a tool forcing validation,
    // `RenderDevice::headless_with_flags`) or a window's instance never share one. Its first
    // device has the same number as the outlived handle, the first device of the shared
    // instance, so `==` calls them one device; `same_device` tells them apart by the raw device.
    let other_flags = instance_flags() ^ wgpu::InstanceFlags::DISCARD_HAL_LABELS;
    let other = RenderDevice::headless_with_flags(other_flags).expect("no GPU adapter");
    assert_eq!(
        regia_render::instance_creation_count(),
        instances + 1,
        "other instance flags get an instance of their own"
    );
    assert!(pipeline_cache::cache_for(&other).is_some());
    assert!(
        outlived == other.device,
        "wgpu now tells devices of two instances apart: `same_device` can go back to `==`"
    );
    assert!(pipeline_cache::cache_for(&outlived).is_none());
    create_pipeline(&outlived);
    create_pipeline(&other.device);

    // A later install replaces an earlier one (a window's device takes it from a headless one).
    // The device that lost it keeps working, uncached; its cache is gone from wgpu's tables.
    // Both directions of the cross-instance pair: the cache stays with `outlived` alone.
    let replacement = pipeline_cache::PersistentPipelineCache::open(&outlived, &info, &dir)
        .expect("the device has the feature");
    let _replacement = pipeline_cache::install(&outlived, replacement);
    assert!(pipeline_cache::cache_for(&outlived).is_some());
    assert!(pipeline_cache::cache_for(&other).is_none());
    create_pipeline(&other.device);
    create_pipeline(&outlived);
    pipeline_cache::uninstall();
    drop(other);
    drop(outlived);

    // The failure as it was found: test threads, each with its own device, creating pipelines side
    // by side. The devices open one after another: opening instances on parallel threads can
    // livelock NVIDIA's driver with or without a cache (`RenderDevice::shared_headless`).
    let devices: Vec<RenderDevice> = (0..4).map(|_| headless()).collect();
    assert!(pipeline_cache::cache_for(&devices[0]).is_some());
    assert!(
        devices[1..]
            .iter()
            .all(|d| pipeline_cache::cache_for(d).is_none())
    );
    let start = std::sync::Barrier::new(devices.len());
    std::thread::scope(|scope| {
        for device in &devices {
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for _ in 0..8 {
                    create_pipeline(&device.device);
                }
            });
        }
    });
    drop(devices);

    pipeline_cache::set_directory(None);
    let _ = std::fs::remove_dir_all(&dir);
}
