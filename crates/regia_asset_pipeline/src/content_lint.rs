// SPDX-License-Identifier: MPL-2.0
//! Shared admission policy for authoring-only glTF diagnostics (ADR-8226).
//!
//! Counters charge payload reservations before allocation. They are deterministic
//! admission bounds, not a measurement of allocator overhead or resident memory.
//! JSON parser storage is bounded separately by `max_json_bytes`.

use serde::{Deserialize, Serialize};

pub(crate) mod io;
pub(crate) mod json;
pub(crate) mod loader;

pub(crate) use loader::open;

/// Per-operation limits; hosts may share this policy between both lint tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContentLimits {
    /// Aggregate bytes read for JSON, chunk headers and loaded buffer payloads.
    pub max_encoded_bytes: u64,
    /// Maximum JSON chunk/document bytes, before parsing or copying JSON.
    pub max_json_bytes: u64,
    /// Aggregate actual external-buffer sizes, including undeclared trailing bytes.
    pub max_external_bytes: u64,
    /// Conservative decoded payload reservations, including normalization scratch.
    pub max_decoded_bytes: u64,
    /// Maximum declared accessors, before typed parsing and normalization.
    pub max_accessors: u64,
    /// Aggregate accessor components and structural/normalization work units.
    pub max_work: u64,
    /// Maximum bytes streamed when repairing and verifying a source file.
    pub max_copy_bytes: u64,
}

impl Default for ContentLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 256 * 1024 * 1024,
            max_json_bytes: 16 * 1024 * 1024,
            max_external_bytes: 256 * 1024 * 1024,
            max_decoded_bytes: 512 * 1024 * 1024,
            max_accessors: 100_000,
            max_work: 64_000_000,
            max_copy_bytes: 8 * 1024 * 1024 * 1024,
        }
    }
}

/// Deterministic counters; refused charges do not advance their counter.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentUsage {
    /// Admitted encoded bytes, charged before each read.
    pub encoded_bytes: u64,
    /// Admitted external-buffer lengths.
    pub external_bytes: u64,
    /// Conservative admitted decoded payload bytes (not resident heap usage).
    pub decoded_bytes: u64,
    /// Declared accessor count admitted before typed parsing.
    pub accessors: u64,
    /// Admitted deterministic work units.
    pub work: u64,
    /// Raw source-buffer allocation requests made after admission. Normalization
    /// scratch and output are included in `decoded_bytes` reservations instead.
    pub buffer_bytes_allocated: u64,
    /// Bytes streamed by repair/copy verification.
    pub copied_bytes: u64,
}

/// Failure of bounded content loading. No partial report is a successful result.
#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    /// A resource ceiling or checked arithmetic bound was exceeded.
    #[error("content input limit exceeded: {0}")]
    Limit(&'static str),
    /// The caller requested cancellation between bounded units.
    #[error("content lint cancelled")]
    Cancelled,
    /// A source or external file changed during inspection.
    #[error("content source changed while reading: {0}")]
    Changed(String),
    /// Malformed input.
    #[error("{0}")]
    Invalid(String),
    /// A filesystem operation failed.
    #[error("{0}")]
    Io(String),
}

impl ContentError {
    /// Stable host/agent reason code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Limit(_) => "content_lint_limit",
            Self::Cancelled => "content_lint_cancelled",
            Self::Changed(_) => "content_lint_changed",
            Self::Invalid(_) | Self::Io(_) => "gltf_unreadable",
        }
    }
}

impl From<std::io::Error> for ContentError {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            Self::Invalid(error.to_string())
        } else {
            Self::Io(error.to_string())
        }
    }
}

/// Operation-local admission counters and a cooperative cancellation probe.
/// The probe may use an atomic flag; it is called before IO, parsing and each
/// normalization unit. Do not reuse a budget for an independent operation.
pub struct ContentBudget<'a> {
    limits: ContentLimits,
    usage: ContentUsage,
    cancelled: &'a dyn Fn() -> bool,
}

impl<'a> ContentBudget<'a> {
    /// Starts an operation with the supplied policy and cancellation probe.
    pub fn new(limits: ContentLimits, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self {
            limits,
            usage: ContentUsage::default(),
            cancelled,
        }
    }

    /// Effective limits for this operation.
    pub fn limits(&self) -> &ContentLimits {
        &self.limits
    }

    /// Counters remain inspectable after both success and failure.
    pub fn usage(&self) -> &ContentUsage {
        &self.usage
    }

    /// Checks cancellation without charging work.
    pub fn checkpoint(&self) -> Result<(), ContentError> {
        if (self.cancelled)() {
            Err(ContentError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn encoded(&mut self, bytes: u64) -> Result<(), ContentError> {
        self.checkpoint()?;
        charge(
            &mut self.usage.encoded_bytes,
            bytes,
            self.limits.max_encoded_bytes,
            "max_encoded_bytes",
        )
    }
    pub(crate) fn external(&mut self, bytes: u64) -> Result<(), ContentError> {
        charge(
            &mut self.usage.external_bytes,
            bytes,
            self.limits.max_external_bytes,
            "max_external_bytes",
        )
    }
    pub(crate) fn decoded(&mut self, bytes: u64) -> Result<(), ContentError> {
        charge(
            &mut self.usage.decoded_bytes,
            bytes,
            self.limits.max_decoded_bytes,
            "max_decoded_bytes",
        )
    }
    pub(crate) fn work(&mut self, units: u64) -> Result<(), ContentError> {
        self.checkpoint()?;
        charge(&mut self.usage.work, units, self.limits.max_work, "max_work")
    }
    pub(crate) fn accessors(&mut self, count: u64) -> Result<(), ContentError> {
        charge(
            &mut self.usage.accessors,
            count,
            self.limits.max_accessors,
            "max_accessors",
        )
    }
    pub(crate) fn copied(&mut self, bytes: u64) -> Result<(), ContentError> {
        self.checkpoint()?;
        charge(
            &mut self.usage.copied_bytes,
            bytes,
            self.limits.max_copy_bytes,
            "max_copy_bytes",
        )
    }
    pub(crate) fn allocated(&mut self, bytes: u64) -> Result<(), ContentError> {
        charge(
            &mut self.usage.buffer_bytes_allocated,
            bytes,
            self.limits.max_decoded_bytes,
            "max_decoded_bytes",
        )
    }
}

fn charge(total: &mut u64, amount: u64, limit: u64, name: &'static str) -> Result<(), ContentError> {
    *total = total
        .checked_add(amount)
        .filter(|n| *n <= limit)
        .ok_or(ContentError::Limit(name))?;
    Ok(())
}

pub(crate) fn product(a: u64, b: u64) -> Result<u64, ContentError> {
    a.checked_mul(b).ok_or(ContentError::Limit("size arithmetic"))
}

/// The checked loader pads every decoded buffer to a four-byte boundary.
pub(crate) fn padded(bytes: u64) -> Result<u64, ContentError> {
    product(bytes.div_ceil(4), 4)
}
