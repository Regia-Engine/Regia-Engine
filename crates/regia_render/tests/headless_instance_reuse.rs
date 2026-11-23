// SPDX-License-Identifier: MPL-2.0
//! A process creates one headless instance, however often it lists or probes adapters while other
//! threads create named objects (ADR-7557).
//!
//! The Vulkan loader of Debian 13 (1.4.309) frees, on a new instance's first enumeration, the
//! entry of every driver that reports no device, before it unlinks it from the lists that each
//! debug-utils object naming (`vkSetDebugUtilsObjectNameEXT`, which wgpu calls for every labeled
//! object in a debug build) walks (ADR-2113). On the build server's Intel Arc pass
//! (`VK_LOADER_DRIVERS_SELECT=*intel*` selects Mesa's `intel_hasvk` too, which has no device on
//! Battlemage) every instance a test created mid-run raced the other test threads, and the
//! `regia_render` lib binary died with SIGSEGV in `loader_get_icd_and_device`.
//!
//! On a machine whose drivers all have a device (lavapipe alone, the gate's NVIDIA pass) this test
//! cannot crash on either side of the fix; there it checks the count. With a deviceless driver
//! selected beside the real one (the Arc pass, or lavapipe plus `intel_hasvk` and the 1.4.309
//! loader, the recipe in ADR-7557) the code before the fix created an instance per listing and
//! died with SIGSEGV in 3 of 3 runs (libvulkan +0x2d06e under `create_buffer`'s object naming),
//! and passes 5 of 5 after it.
//!
//! This binary holds one test, so nothing else creates an instance meanwhile.

use std::sync::atomic::{AtomicBool, Ordering};

use regia_render::RenderDevice;

/// Adapter listings and ray-tracing probes made while the naming threads run.
const LISTINGS: usize = 32;
/// Threads creating labeled buffers on the shared device.
const NAMERS: usize = 4;

#[test]
fn listing_adapters_beside_named_objects_creates_no_new_instance() {
    let device = match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip headless_instance_reuse: {err}");
            return;
        }
    };
    let info = device.adapter.get_info();
    eprintln!(
        "headless_instance_reuse adapter: {} ({:?})",
        info.name, info.backend
    );
    let after_device = regia_render::instance_creation_count();
    assert!(after_device >= 1, "the shared device's instance was counted");
    let stop = AtomicBool::new(false);
    let named = std::thread::scope(|scope| {
        let namers: Vec<_> = (0..NAMERS)
            .map(|thread| {
                let device = &device;
                let stop = &stop;
                scope.spawn(move || {
                    let mut created = 0_u64;
                    // At least a few objects each, then until the listings are done.
                    while created < 16 || !stop.load(Ordering::Relaxed) {
                        let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some(&format!("instance reuse namer {thread} #{created}")),
                            size: 256,
                            usage: wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        });
                        drop(buffer);
                        created += 1;
                    }
                    created
                })
            })
            .collect();
        let mut listed = 0;
        for _ in 0..LISTINGS {
            listed += regia_render::list_adapters().len();
            listed += regia_render::hwrt::probe_adapters().len();
        }
        stop.store(true, Ordering::Relaxed);
        assert!(
            listed >= 2 * LISTINGS,
            "every listing found the shared device's adapter"
        );
        namers
            .into_iter()
            .map(|namer| namer.join().expect("naming thread"))
            .sum::<u64>()
    });
    assert!(named >= 16 * NAMERS as u64);
    assert_eq!(
        regia_render::instance_creation_count(),
        after_device,
        "{LISTINGS} listings and probes reused the headless devices' instance"
    );
}
