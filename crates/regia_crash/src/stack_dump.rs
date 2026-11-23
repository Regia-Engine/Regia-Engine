// SPDX-License-Identifier: MPL-2.0
//! All-thread hang reports (P-08, ADR-8156).
//!
//! Install the sampler at startup and call [`capture`] or [`write_hang_report`]
//! from a watchdog/normal thread. Linux GNU only; other targets return Unsupported.
//! This does not install a panic hook or timer. Request signals are opt-in. A host can call
//! it on its own stall trigger. Saved reports are available through the existing
//! agent method `diagnostics.crash_reports` and the normal opt-in uploader.

pub use regia_platform::stack_sampling::{
    FRAME_CAPACITY, SampleOutcome, Settings, StackDump, ThreadSample, capture, install,
    install_request_handler, install_request_signal, install_with_signal, request, take_request,
};

/// Captures thread stacks and saves a scrubbed `crash-*.txt` hang report.
///
/// # Errors
/// Returns capture errors, or an IO error if the crash directory is unconfigured
/// or the report cannot be written. Call `install` once before using this function.
pub fn write_hang_report(reason: &str, settings: &Settings) -> std::io::Result<std::path::PathBuf> {
    let dump = capture(settings)?;
    regia_platform::crash::write_hang_report(reason, &dump.text())
        .ok_or_else(|| std::io::Error::other("hang report directory is unconfigured or not writable"))
}
