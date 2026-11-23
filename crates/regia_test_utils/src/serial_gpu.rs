// SPDX-License-Identifier: MPL-2.0
//! One test at a time on one device per test binary, on a GPU (ADR-7483).
//!
//! `cargo test` runs a binary's tests on parallel threads. A test that opens its own instance and
//! device while sibling tests use theirs livelocked the NVIDIA driver on the RTX 5090s (the test
//! threads parked in the driver and the binary never finished: the `HANG` rows of the GPU
//! pre-gate). One device per process ([`RenderDevice::shared_headless`], ADR-2113) removes the
//! instance and device churn, and running one test at a time on it also keeps two tests from
//! compiling pipelines at once, the other NVIDIA fault the GPU sweeps met (ADR-7334).
//!
//! [`serial_device`] waits for the binary's GPU turn and returns the shared device with the turn
//! attached: it dereferences to [`RenderDevice`], and the turn ends when the value is dropped (at
//! the end of the test). A test that panicked during its turn leaves the turn usable for the
//! next. A test takes one turn: calling [`serial_device`] again on the same thread while the first
//! value lives waits forever.
//!
//! On a CPU adapter (lavapipe, WARP) the turn is given back at once: those drivers have neither
//! fault, the shared device is enough there (ADR-2113), and the CI's lavapipe pass keeps running a
//! binary's tests side by side ([`takes_turns`]).

use std::ops::Deref;
use std::sync::{Mutex, MutexGuard, PoisonError};

use regia_render::{RenderDevice, RenderError};

/// The binary's GPU turn.
static TURN: Mutex<()> = Mutex::new(());

/// The process's shared headless device, held for one test at a time on a GPU (see the module
/// docs).
pub struct SerialGpu {
    device: RenderDevice,
    // Declared after `device`, so the device clone is dropped before the turn ends. `None` on a
    // CPU adapter.
    _turn: Option<MutexGuard<'static, ()>>,
}

impl Deref for SerialGpu {
    type Target = RenderDevice;

    fn deref(&self) -> &RenderDevice {
        &self.device
    }
}

/// Whether tests on an adapter of this type take turns: every type but a CPU adapter.
pub fn takes_turns(device_type: wgpu::DeviceType) -> bool {
    device_type != wgpu::DeviceType::Cpu
}

/// Waits for this binary's GPU turn, then returns the process's shared headless device. On a CPU
/// adapter the turn is given back before this returns.
///
/// # Errors
///
/// No adapter ([`RenderDevice::shared_headless`]); the turn is given back and a later call tries
/// to create the device again.
pub fn serial_device() -> Result<SerialGpu, RenderError> {
    device_with_turn(takes_turns)
}

fn device_with_turn(turns: fn(wgpu::DeviceType) -> bool) -> Result<SerialGpu, RenderError> {
    let turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    let device = RenderDevice::shared_headless()?;
    let keep = turns(device.adapter.get_info().device_type);
    Ok(SerialGpu {
        device,
        _turn: keep.then_some(turn),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn always(_: wgpu::DeviceType) -> bool {
        true
    }

    #[test]
    fn every_adapter_type_but_a_cpu_takes_turns() {
        assert!(!takes_turns(wgpu::DeviceType::Cpu));
        for kind in [
            wgpu::DeviceType::DiscreteGpu,
            wgpu::DeviceType::IntegratedGpu,
            wgpu::DeviceType::VirtualGpu,
            wgpu::DeviceType::Other,
        ] {
            assert!(takes_turns(kind), "{kind:?}");
        }
    }

    /// With turns on (forced here, so lavapipe checks it too), no two holders overlap and every
    /// turn gets the one shared device.
    #[test]
    fn turns_never_overlap_and_share_one_device() {
        let Ok(first) = device_with_turn(always) else {
            eprintln!("skipped: no headless adapter");
            return;
        };
        let id = first.device_id();
        drop(first);
        let inside = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        let ids: Vec<u64> = std::thread::scope(|s| {
            let workers: Vec<_> = (0..6)
                .map(|_| {
                    s.spawn(|| {
                        let gpu = device_with_turn(always).expect("the device existed a moment ago");
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        // Work on the device during the turn, so an overlapping turn would be seen.
                        for _ in 0..50 {
                            std::thread::yield_now();
                        }
                        let id = gpu.device_id();
                        inside.fetch_sub(1, Ordering::SeqCst);
                        id
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().expect("worker")).collect()
        });
        assert_eq!(most.load(Ordering::SeqCst), 1, "two tests held the GPU at once");
        assert!(
            ids.iter().all(|&i| i == id),
            "every turn gets the one shared device"
        );
    }

    /// The public entry point keeps the turn exactly when the adapter's type takes turns.
    #[test]
    fn the_turn_is_kept_only_where_the_adapter_takes_turns() {
        let Ok(gpu) = serial_device() else {
            eprintln!("skipped: no headless adapter");
            return;
        };
        let kind = gpu.adapter.get_info().device_type;
        assert_eq!(gpu._turn.is_some(), takes_turns(kind), "{kind:?}");
    }

    #[test]
    fn a_test_that_panicked_in_its_turn_leaves_the_turn_usable() {
        if device_with_turn(always).is_err() {
            eprintln!("skipped: no headless adapter");
            return;
        }
        let panicked = std::thread::spawn(|| {
            let _gpu = device_with_turn(always).expect("device");
            panic!("a failing test during its GPU turn");
        })
        .join();
        assert!(panicked.is_err());
        assert!(device_with_turn(always).is_ok(), "the next test gets its turn");
    }
}
