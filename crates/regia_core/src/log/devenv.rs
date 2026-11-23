// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-log/src/devenv.rs, commit d6ed37677), ADR-8012.
// Ported from DO27 ir-log/src/devenv.rs:1-65, d6ed37677 (regia-absorb-1).
// Owner-authorized copy, 2026-11-16, absorb plan A-1; class P3; runtime as-is.
//! Developer-only environment reads.
//!
//! A tester build compiles without the `devtools` feature: every read here then returns
//! "not set" as a constant, so test hooks, fixtures, tuning-file overrides and upload
//! overrides cannot be switched on from the environment, and the variable names drop out of
//! the binary (the call is `#[inline(always)]`, the name is an unused argument).
//!
//! Paths and settings a player legitimately sets (log dir, data dir, upload outbox dir,
//! cache dir, port) stay on plain `std::env`.

use std::env::VarError;
use std::ffi::OsString;

/// True when this build honors developer variables.
pub const ENABLED: bool = cfg!(feature = "devtools");

/// `std::env::var` under `devtools`; always `NotPresent` without it.
#[inline(always)]
pub fn var(name: &str) -> Result<String, VarError> {
    #[cfg(feature = "devtools")]
    {
        std::env::var(name)
    }
    #[cfg(not(feature = "devtools"))]
    {
        let _ = name;
        Err(VarError::NotPresent)
    }
}

/// `std::env::var_os` under `devtools`; always `None` without it.
#[inline(always)]
pub fn var_os(name: &str) -> Option<OsString> {
    #[cfg(feature = "devtools")]
    {
        std::env::var_os(name)
    }
    #[cfg(not(feature = "devtools"))]
    {
        let _ = name;
        None
    }
}

/// The variable is set (to anything).
#[inline(always)]
pub fn is_set(name: &str) -> bool {
    var_os(name).is_some()
}

/// The variable is exactly `1`.
#[inline(always)]
pub fn flag(name: &str) -> bool {
    var(name).ok().as_deref() == Some("1")
}

#[cfg(all(test, feature = "devtools"))]
mod tests {
    #[test]
    fn reads_the_environment() {
        assert!(!super::is_set("REGIA_DEVENV_TEST_UNSET_VAR"));
        assert!(super::var("REGIA_DEVENV_TEST_UNSET_VAR").is_err());
        assert!(std::hint::black_box(super::ENABLED));
    }
}
