// SPDX-License-Identifier: MPL-2.0
//! Slow probes off the frame thread: process RSS (a `/proc` read) and VRAM (wgpu's allocator
//! report) are sampled by a low-duty background thread; the frame only loads atomics.
//!
//! The thread holds a `wgpu::Device` clone, so it must end (and drop that clone) before the
//! app's device is destroyed: [`stop_and_join`] wakes it and joins it with a bounded wait. A
//! probe left running past `main` drops the last device reference while the process's exit
//! handlers tear the GPU driver down (SIGSEGV in NVIDIA's `libnvidia-glcore`, v0.1.0 validation).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Values the probe thread publishes (`u64::MAX` = unknown).
#[derive(Default)]
pub(crate) struct ProbeValues {
    pub rss: AtomicU64,
    pub rss_peak: AtomicU64,
    pub vram_allocated: AtomicU64,
    pub vram_reserved: AtomicU64,
    pub samples: AtomicU64,
    /// Seqlock generation for the four values above: odd while the probe thread is writing them.
    seq: AtomicU64,
    stop: AtomicBool,
}

/// One consistent read of the four probe values (`None` = unknown).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProbeSnapshot {
    pub rss: Option<u64>,
    pub rss_peak: Option<u64>,
    pub vram_allocated: Option<u64>,
    pub vram_reserved: Option<u64>,
}

impl ProbeValues {
    pub fn new() -> Arc<Self> {
        let v = Self::default();
        for a in [&v.rss, &v.rss_peak, &v.vram_allocated, &v.vram_reserved] {
            a.store(u64::MAX, Ordering::Relaxed);
        }
        Arc::new(v)
    }

    pub fn get(a: &AtomicU64) -> Option<u64> {
        let v = a.load(Ordering::Relaxed);
        (v != u64::MAX).then_some(v)
    }

    /// Publishes values as one generation (single writer: the probe thread). `None` keeps the
    /// previous value of that slot.
    pub fn publish(&self, rss: Option<u64>, peak: Option<u64>, vram: Option<(u64, u64)>) {
        let start = self.seq.load(Ordering::Relaxed);
        self.seq.store(start.wrapping_add(1), Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        if let Some(v) = rss {
            self.rss.store(v, Ordering::Relaxed);
        }
        if let Some(v) = peak {
            self.rss_peak.store(v, Ordering::Relaxed);
        }
        if let Some((allocated, reserved)) = vram {
            self.vram_allocated.store(allocated, Ordering::Relaxed);
            self.vram_reserved.store(reserved, Ordering::Relaxed);
        }
        self.seq.store(start.wrapping_add(2), Ordering::Release);
    }

    /// The four values from one generation (retries while the probe thread is mid-publish).
    pub fn snapshot(&self) -> ProbeSnapshot {
        loop {
            let before = self.seq.load(Ordering::Acquire);
            if !before.is_multiple_of(2) {
                std::hint::spin_loop();
                continue;
            }
            let snap = ProbeSnapshot {
                rss: Self::get(&self.rss),
                rss_peak: Self::get(&self.rss_peak),
                vram_allocated: Self::get(&self.vram_allocated),
                vram_reserved: Self::get(&self.vram_reserved),
            };
            std::sync::atomic::fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return snap;
            }
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

/// Starts the probe thread. `device` is `None` until the render device exists; the thread
/// then samples RSS only. Returns the thread's handle (`None` when it could not start), which
/// the owner passes to [`stop_and_join`].
pub(crate) fn spawn(
    values: Arc<ProbeValues>,
    device: Option<wgpu::Device>,
    every: Duration,
    vram: Option<VramWatch>,
) -> Option<JoinHandle<()>> {
    // Registered (ADR-287): the end of the process waits for the probe and its device clone.
    let stop = Arc::downgrade(&values);
    let spawned = regia_platform::GpuWorkerBuilder::new("regia-telemetry-probe")
        .on_stop(move || {
            if let Some(values) = stop.upgrade() {
                values.stop();
            }
        })
        .spawn(move |worker| run(&values, device, every, vram, worker));
    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            tracing::warn!(target: crate::target::TELEMETRY, error = %e, "telemetry probe thread did not start; RSS and VRAM stay unknown");
            None
        }
    }
}

/// The probe loop. Owns the device clone and drops it on return, so it is released on this
/// thread before a join of the thread returns.
fn run(
    values: &ProbeValues,
    device: Option<wgpu::Device>,
    every: Duration,
    vram: Option<VramWatch>,
    worker: &regia_platform::GpuWorker,
) {
    loop {
        {
            // The allocator report is a driver-side walk: marked for the exit log.
            let _call = device.is_some().then(|| worker.driver_call());
            sample(values, device.as_ref().zip(vram));
        }
        values.samples.fetch_add(1, Ordering::Relaxed);
        // Park until the next sample; [`stop_and_join`] unparks the thread, so a stop ends it
        // at once (a spurious wake-up only re-checks the flag).
        let next = Instant::now() + every;
        loop {
            if values.stopped() {
                drop(device);
                return;
            }
            let now = Instant::now();
            if now >= next {
                break;
            }
            std::thread::park_timeout(next - now);
        }
    }
}

/// Stops the probe thread and joins it, waiting at most `wait` (a sample in a stuck driver call
/// must not hang quit). True when the thread ended, with its device clone dropped, before this
/// returned.
pub(crate) fn stop_and_join(values: &ProbeValues, handle: JoinHandle<()>, wait: Duration) -> bool {
    values.stop();
    handle.thread().unpark();
    let deadline = Instant::now() + wait;
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    if handle.is_finished() {
        let _ = handle.join();
        true
    } else {
        tracing::warn!(
            target: crate::target::TELEMETRY,
            wait_ms = wait.as_millis() as u64,
            "telemetry probe thread still busy at shutdown; left running"
        );
        false
    }
}

/// VRAM sampling settings of the probe.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VramWatch {
    /// The adapter's device-local memory, MiB (`None` when unknown: no pressure signal).
    pub adapter_mib: Option<u64>,
}

/// Signals GPU memory pressure (`regia_render::gpu_health`, at most once per 30 s, trim only)
/// when the allocator's live allocations exceed 85 % of the adapter's device-local VRAM.
/// Allocated, not reserved: a healthy card keeps whole allocator blocks reserved near the top of
/// VRAM. Never with less than 2 GiB of device-local VRAM (an APU carve-out; the report also
/// counts host-visible memory).
pub(crate) fn check_vram_pressure(report_allocated: u64, watch: VramWatch) -> bool {
    watch
        .adapter_mib
        .is_some_and(|mib| regia_render::gpu_health::vram_over_threshold(report_allocated, mib))
}

fn sample(values: &ProbeValues, device: Option<(&wgpu::Device, VramWatch)>) {
    let rss = regia_render::process_rss_bytes();
    let peak = regia_render::memory::process_peak_rss_bytes();
    let report = device.and_then(|(device, watch)| device.generate_allocator_report().map(|r| (r, watch)));
    values.publish(
        rss,
        peak,
        report
            .as_ref()
            .map(|(r, _)| (r.total_allocated_bytes, r.total_reserved_bytes)),
    );
    if let Some((report, watch)) = report {
        // What filled it, by coarse bucket (the summary's `memory.categories`, crash bundles).
        regia_render::memory::publish_summary_buckets(
            report.allocations.iter().map(|a| (a.name.as_str(), a.size)),
        );
        // And by engine subsystem, beside the CPU heap ledger (`memory.subsystems`, ADR-539).
        regia_render::memory::publish_subsystem_bytes(
            report.allocations.iter().map(|a| (a.name.as_str(), a.size)),
        );
        if check_vram_pressure(report.total_allocated_bytes, watch) {
            regia_render::gpu_health::signal_pressure(regia_render::gpu_health::PressureSource::VramHigh);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_starts_unknown_and_keeps_unpublished_slots() {
        let v = ProbeValues::new();
        let none = v.snapshot();
        assert_eq!((none.rss, none.vram_allocated), (None, None));
        v.publish(Some(7), None, Some((3, 9)));
        v.publish(Some(8), Some(10), None);
        let s = v.snapshot();
        assert_eq!(s.rss, Some(8));
        assert_eq!(s.rss_peak, Some(10));
        assert_eq!((s.vram_allocated, s.vram_reserved), (Some(3), Some(9)));
    }

    #[test]
    fn snapshot_never_mixes_two_publishes() {
        let v = ProbeValues::new();
        v.publish(Some(0), Some(1000), Some((0, 1000)));
        let writer = {
            let v = Arc::clone(&v);
            std::thread::spawn(move || {
                for n in 1..20_000u64 {
                    v.publish(Some(n), Some(n + 1000), Some((n, n + 1000)));
                }
            })
        };
        // A fixed number of reads: every one must be internally consistent.
        for _ in 0..20_000 {
            let s = v.snapshot();
            let (rss, peak) = (s.rss.expect("rss"), s.rss_peak.expect("rss peak"));
            assert_eq!(peak, rss + 1000, "rss and its peak are one generation");
            let (a, r) = (
                s.vram_allocated.expect("allocated"),
                s.vram_reserved.expect("reserved"),
            );
            assert_eq!(r, a + 1000, "allocated and reserved are one generation");
            assert_eq!(rss, a, "all four slots are one generation");
        }
        writer.join().expect("writer thread");
    }
}
