// SPDX-License-Identifier: MPL-2.0
//! wgpu device hooks: uncaptured errors become `regia::gpu_error` events, a lost device becomes
//! `regia::device_lost`. Both also feed `regia_render::gpu_health` (error count, memory
//! pressure on out-of-memory, the device-lost exit).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::rate::{RateLimiter, key};
use crate::target;

/// The app frame the telemetry plugin last counted (errors are stamped with it).
pub(crate) static FRAME: AtomicU64 = AtomicU64::new(0);

/// Longest message kept in a `regia::gpu_error` event.
pub const GPU_ERROR_MESSAGE_BYTES: usize = 2048;

static GPU_ERROR_REPEATS: Mutex<RateLimiter> = Mutex::new(RateLimiter::new(Duration::from_secs(5)));

/// Installs the uncaptured-error handler and the device-lost callback on `device`.
///
/// wgpu's default handler panics on any uncaptured error. This one logs instead (rate-limited
/// per kind, pass and the first 200 bytes of the message, over a fixed 5 s window), and panics afterwards only when `panic_on_error` is set (debug builds by
/// default), so a release build keeps running on a validation error (ADR in docs/decisions.md).
pub fn install(device: &wgpu::Device, panic_on_error: bool) {
    device.on_uncaptured_error(Arc::new(move |error: wgpu::Error| {
        on_error(&error, panic_on_error);
    }));
    device.set_device_lost_callback(|reason, message| {
        // The render plugin stops drawing and exits with a crash bundle on a loss (ADR-171).
        let lost = regia_render::gpu_health::note_device_lost(reason, &message);
        if lost {
            regia_core::telemetry::add(regia_core::telemetry::Counter::DeviceLost, 1);
        }
        let frame = FRAME.load(Ordering::Relaxed);
        let reason_name = match reason {
            wgpu::DeviceLostReason::Destroyed => "destroyed",
            wgpu::DeviceLostReason::Unknown => "unknown",
        };
        let data = serde_json::json!({ "reason": reason_name, "frame": frame, "message": message, "exits": lost }).to_string();
        if reason == wgpu::DeviceLostReason::Destroyed {
            tracing::info!(target: target::DEVICE_LOST, reason = reason_name, frame, data = %data, "GPU device destroyed: {message}");
        } else {
            tracing::error!(target: target::DEVICE_LOST, reason = reason_name, frame, data = %data, "GPU device lost: {message}");
        }
    });
}

/// Classifies and reports one uncaptured error (separate from [`install`] for tests).
pub(crate) fn on_error(error: &wgpu::Error, panic_on_error: bool) {
    // Counts it (`render.problems`, the overlay); an out-of-memory error also frees idle GPU
    // memory (`regia_render::gpu_health`).
    regia_render::gpu_health::note_uncaptured_error(error);
    let (kind, text) = match error {
        wgpu::Error::OutOfMemory { .. } => ("out_of_memory", error.to_string()),
        wgpu::Error::Validation { description, .. } => ("validation", description.clone()),
        wgpu::Error::Internal { description, .. } => ("internal", description.clone()),
    };
    let frame = FRAME.load(Ordering::Relaxed);
    // wgpu reports uncaptured errors synchronously on the calling thread: the current span is
    // the pass (or system) that made the failing call.
    let span = tracing::Span::current();
    let pass = span.metadata().map_or("none", |m| m.name());
    let head = &text.as_bytes()[..text.len().min(200)];
    let repeated = GPU_ERROR_REPEATS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .check(key(&[kind.as_bytes(), pass.as_bytes(), head]), Instant::now());
    if let Some(repeated) = repeated {
        let mut message = text.clone();
        if message.len() > GPU_ERROR_MESSAGE_BYTES {
            let mut cut = GPU_ERROR_MESSAGE_BYTES;
            while !message.is_char_boundary(cut) {
                cut -= 1;
            }
            message.truncate(cut);
        }
        let data = serde_json::json!({
            "kind": kind, "frame": frame, "pass": pass, "repeated": repeated, "message": message,
        })
        .to_string();
        tracing::error!(target: target::GPU_ERROR, kind, frame, pass, repeated, data = %data, "wgpu {kind} error in {pass}");
    }
    // Calls on a lost device fail until the app has exited (ADR-171): never fatal then.
    if panic_on_error && !regia_render::device_lost() {
        panic!("wgpu {kind} error in {pass} (frame {frame}): {text}");
    }
}
