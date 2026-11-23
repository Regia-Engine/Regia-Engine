// SPDX-License-Identifier: MPL-2.0
//! Opt-in crash-report and match-log upload (`docs/specs/21-tooling-and-packaging.md` § 5).
//!
//! [`CrashReporter`] uploads pending crash reports (written by `regia_platform::crash`) and a
//! game-provided match log to a configurable endpoint on next start. Uploading is off by default
//! (empty endpoint, no consent), retries with exponential backoff, caps per-report size, and never
//! deletes a report the server did not accept. A transport failure, HTTP 408, HTTP 429, or a 5xx
//! leaves the `crash-*.txt` in place for the next launch. Any other non-2xx (including 3xx, which
//! are not followed) renames that file to `*.rejected`, so a later start does not send it again.
//! Every upload's name and body go through [`scrub_upload`] (invalid UTF-8 becomes U+FFFD, the
//! home folder becomes `~`, secrets become `[redacted]`, email addresses become `[email]`) before
//! they leave the machine.
//!
//! Production endpoints must be `https://`; sending over TLS needs the crate's `tls` feature
//! (ureq + rustls). Without it, `https://` is refused. Plain `http://` is sent only when the host
//! string is `localhost` or a literal loopback IP (names are not resolved), or when
//! [`CrashReporter::allow_cleartext`] is set. Any other `http://` host turns uploads off.
//!
//! # Example
//!
//! ```
//! use regia_crash::{CrashReporter, CrashReporterPlugin};
//!
//! let reporter = CrashReporter {
//!     endpoint: "https://crash.example.com/regia".into(),
//!     consent: true,
//!     ..CrashReporter::default()
//! };
//! // Insert `reporter` before adding the plugin; it uploads once at Startup.
//! let _ = CrashReporterPlugin;
//! ```

pub mod outbox;
mod reporter;
#[cfg(feature = "tls")]
pub mod tls_guard;
pub mod transport;

pub mod stack_dump;

pub use reporter::{
    CrashReporter, CrashReporterPlugin, CrashUploadSet, DEFAULT_MAX_BYTES, UploadSummary, scrub_upload,
};
