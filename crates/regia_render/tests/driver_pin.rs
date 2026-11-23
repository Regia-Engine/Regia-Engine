// SPDX-License-Identifier: MPL-2.0
//! Every device a process creates selects the same adapter, because the GPU drivers stay loaded
//! once the first device has loaded them (ADR-5794).
//!
//! Creating an instance makes the Vulkan loader load every installed driver library four times
//! (three queries wgpu makes before the instance, each of which loads the drivers and unloads
//! them again, then the instance itself), and dropping the last instance unloads them. A driver
//! whose thread-local storage is allocated from glibc's static TLS block (NVIDIA's
//! `libnvidia-tls.so`) takes a little of that block on every load and the block is never given
//! back: after 72 load cycles the loader reported `cannot allocate memory in static TLS block`,
//! dropped the driver, and the next device was created on another adapter (or, with a strict
//! `REGIA_GPU_UUID`, not at all). Measured on eight RTX 5090s, driver
//! 615.71.09, with every installed driver loaded: the 19th `RenderDevice::headless()` of a process
//! found `[Intel(R) Graphics, llvmpipe]` only; with NVIDIA's driver alone (the merge gate's GPU
//! pass) the 27th found no adapter. A test binary's later tests then skipped and counted as
//! passed, and a long-lived host (a batch renderer, an editor reopening its device) would have
//! dropped to the CPU rasterizer without a word.
//!
//! This binary holds one test so no other thread creates or drops a device meanwhile.

use regia_render::RenderDevice;

/// Device creations in one process. Each loads the drivers four times without the fix, so this
/// is 128 load cycles: past the 72 that lost the driver with every installed driver loaded, and
/// past the 104 with NVIDIA's alone.
const CREATIONS: usize = 32;

/// What identifies the adapter a device was created on.
fn adapter_of(device: &RenderDevice) -> (String, String, wgpu::Backend) {
    let info = device.adapter.get_info();
    (info.name, info.device_pci_bus_id, info.backend)
}

/// `REGIA_DRIVER_PIN_CREATIONS` runs more creations (a diagnosis aid; the default is the test).
fn creations() -> usize {
    std::env::var("REGIA_DRIVER_PIN_CREATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n >= 2)
        .unwrap_or(CREATIONS)
}

/// The shared libraries mapped into this process now (the file paths of `/proc/self/maps`), or
/// `None` where the platform has no such file.
fn mapped_libraries() -> Option<std::collections::BTreeSet<String>> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    Some(
        maps.lines()
            .filter_map(|line| line.split_whitespace().nth(5))
            .filter(|path| {
                let name = path.rsplit('/').next().unwrap_or(path);
                name.ends_with(".so") || name.contains(".so.")
            })
            .map(str::to_owned)
            .collect(),
    )
}

/// Whether `path` is the Vulkan loader library (`libvulkan.so.1...`; the drivers are
/// `libvulkan_<name>.so` or carry their vendor's name).
fn is_vulkan_loader(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .starts_with("libvulkan.so")
}

#[test]
fn every_device_creation_of_a_process_selects_the_first_ones_adapter() {
    let first = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip driver_pin: {err}");
            return;
        }
    };
    let want = adapter_of(&first);
    eprintln!("driver_pin adapter: {want:?}");
    let while_alive = mapped_libraries();
    // Dropped before the next creation: without the fix this unloads every driver.
    drop(first);
    // The cause, checked directly where the platform lets the test see it: the Vulkan loader the
    // first device had loaded is still mapped when the process's only device is gone, so some
    // instance still holds it and, through it, the drivers. Without the fix the loader goes on
    // every Linux adapter, the CPU rasterizer included; the loop below needs a driver that
    // cannot be loaded again and again. Only the loader is asserted: which helper libraries a
    // driver or a layer drops with a device is its own business (the full list is printed).
    if cfg!(target_os = "linux")
        && want.2 == wgpu::Backend::Vulkan
        && let (Some(alive), Some(dropped)) = (while_alive, mapped_libraries())
    {
        let unloaded: Vec<&String> = alive.difference(&dropped).collect();
        eprintln!("driver_pin: libraries unloaded with the device: {unloaded:?}");
        if !alive.iter().any(|path| is_vulkan_loader(path)) {
            eprintln!("driver_pin: no libvulkan.so mapping to check");
        }
        let lost: Vec<&String> = unloaded
            .iter()
            .copied()
            .filter(|path| is_vulkan_loader(path))
            .collect();
        assert!(
            lost.is_empty(),
            "dropping the only device of the process unloaded the Vulkan loader {lost:?} and its \
             drivers with it, so the next device loads them again (unloaded in all: {unloaded:?})"
        );
    }
    let creations = creations();
    for n in 1..creations {
        let device = match RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => panic!(
                "device creation {n} of {creations} failed after the first one succeeded on {want:?}: {err}"
            ),
        };
        assert_eq!(
            adapter_of(&device),
            want,
            "device creation {n} of {creations} selected another adapter"
        );
    }
}
