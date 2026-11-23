// SPDX-License-Identifier: MPL-2.0
//! Bounds how far one XeSS context's recording runs ahead of the GPU (ADR-7695).
//!
//! XeSS SR 2.0.2 keeps the per-execute data it writes while an execute is recorded in a fixed set
//! of slots. On the Intel N150 its output for identical inputs stayed identical with up to 10
//! executes recorded and not yet finished on the GPU, and earlier frames were corrupted from 11 on
//! (the smoke test submits 48 frames without waiting; its release build diverged in every
//! Performance-first run). The XeSS guide states no such limit, so the adapter keeps every context
//! well inside it: before recording an execute it waits until the execute [`MAX_AHEAD`]
//! evaluations back has finished.
//!
//! Each execute marks a small buffer from a ring in the transition encoder that is submitted with
//! it. Mapping that buffer completes once that submission has finished on the GPU, or at once
//! when it was never submitted, so the wait needs no submission index and works for engine frames
//! that submit the commands themselves.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Executes one context may have recorded and not yet finished on the GPU. Four keeps a wide
/// margin below the bound measured on the N150 (10), on hardware whose bound is unknown, and a
/// real-time frame loop is rarely more than three frames ahead, so it does not wait there.
pub(crate) const MAX_AHEAD: usize = 4;
// The N150 kept XeSS's output with 10 outstanding executes and lost it from 11.
const _: () = assert!(
    MAX_AHEAD >= 1 && MAX_AHEAD < 10,
    "stay well inside the measured bound"
);

/// The ring of pacing buffers of one context.
pub(crate) struct Pacer {
    limit: usize,
    slots: VecDeque<wgpu::Buffer>,
}

impl Pacer {
    /// A pacer that lets `limit` executes (at least one) be outstanding.
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            slots: VecDeque::new(),
        }
    }

    /// Call before recording one execute: waits until the execute `limit` evaluations back has
    /// finished on the GPU (or was never submitted), then marks this execute in `before`, the
    /// encoder submitted together with it.
    pub(crate) fn pace(&mut self, device: &wgpu::Device, before: &mut wgpu::CommandEncoder) {
        let _span = tracing::info_span!("xess_pace").entered();
        let reused = if self.slots.len() >= self.limit {
            self.slots
                .pop_front()
                .filter(|oldest| wait_until_idle(device, oldest))
        } else {
            None
        };
        let buffer = reused.unwrap_or_else(|| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("xess pacing"),
                size: 4,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        before.clear_buffer(&buffer, 0, None);
        self.slots.push_back(buffer);
    }
}

/// Waits until every submission that uses `buffer` has finished, by mapping it. `true` when the
/// buffer is unmapped again and can be reused; `false` when mapping failed (device lost) or
/// polling failed, and the caller makes a new one.
fn wait_until_idle(device: &wgpu::Device, buffer: &wgpu::Buffer) -> bool {
    let mapped = Arc::new(AtomicBool::new(false));
    let answered = Arc::new(AtomicBool::new(false));
    let (ok, done) = (Arc::clone(&mapped), Arc::clone(&answered));
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        ok.store(result.is_ok(), Ordering::Release);
        done.store(true, Ordering::Release);
    });
    let mut polls = 0u32;
    while !answered.load(Ordering::Acquire) {
        if let Err(error) = device.poll(wgpu::PollType::Poll) {
            tracing::warn!(%error, "XeSS pacing: device poll failed");
            return false;
        }
        polls = polls.saturating_add(1);
        if answered.load(Ordering::Acquire) {
            break;
        }
        // The GPU is finishing a frame (a few milliseconds at most): spin briefly, then back off.
        if polls < 64 {
            std::thread::yield_now();
        } else {
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
    }
    if polls > 1 {
        tracing::trace!(polls, "XeSS pacing waited for an earlier execute");
    }
    if mapped.load(Ordering::Acquire) {
        buffer.unmap();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> (wgpu::Device, wgpu::Queue) {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::VULKAN | wgpu::Backends::METAL | wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(descriptor);
        let adapter =
            regia_core::tasks::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .unwrap_or_else(|error| panic!("no adapter for the pacing test (lavapipe in CI): {error}"));
        regia_core::tasks::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("xess pacing test"),
            ..Default::default()
        }))
        .unwrap_or_else(|error| panic!("no device for the pacing test: {error}"))
    }

    /// After `pace` returns for execute `n`, execute `n - limit` has finished on the GPU, and the
    /// ring never holds more than `limit` buffers.
    #[test]
    fn recording_waits_for_the_execute_limit_frames_back() {
        let (device, queue) = device();
        let limit = 2;
        let mut pacer = Pacer::new(limit);
        let finished: Vec<Arc<AtomicBool>> = (0..8).map(|_| Arc::new(AtomicBool::new(false))).collect();
        for (n, flag) in finished.iter().enumerate() {
            let mut before = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pacer.pace(&device, &mut before);
            assert!(pacer.slots.len() <= limit);
            if let Some(back) = n.checked_sub(limit) {
                assert!(
                    finished[back].load(Ordering::Acquire),
                    "execute {back} had not finished when execute {n} was recorded"
                );
            }
            queue.submit([before.finish()]);
            let flag = Arc::clone(flag);
            queue.on_submitted_work_done(move || flag.store(true, Ordering::Release));
        }
        assert_eq!(pacer.slots.len(), limit, "the ring reuses its buffers");
    }

    /// An execute whose commands were dropped without being submitted does not block later ones.
    #[test]
    fn an_unsubmitted_execute_does_not_block() {
        let (device, queue) = device();
        let mut pacer = Pacer::new(1);
        let mut dropped = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        pacer.pace(&device, &mut dropped);
        drop(dropped);
        for _ in 0..3 {
            let mut before = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            pacer.pace(&device, &mut before);
            queue.submit([before.finish()]);
        }
        assert_eq!(pacer.slots.len(), 1);
    }
}
