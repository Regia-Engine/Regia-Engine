// SPDX-License-Identifier: MPL-2.0
//! `package.parity` (ADR-8428, book/agents/package-parity.md).
//!
//! The method only diffs traces the caller already has. Launching the two runs
//! stays on `regia package parity`.

use std::path::Path;

use serde_json::{Value, json};

use crate::params::{opt_bool, opt_str};
use crate::protocol::{self, RpcError};

/// `package.parity`.
///
/// # Errors
///
/// `parity_bad_params` when a trace path is missing and this is not a dry run.
/// `parity_trace_missing` when a trace path is not a file. `parity_not_a_directory`
/// when a root is not a directory. An unreadable trace is an internal error.
pub(crate) fn call(params: &Value) -> Result<Value, RpcError> {
    if opt_bool(params, "dry_run")?.unwrap_or(false) {
        return Ok(json!({
            "ok": true,
            "dry_run": true,
            "method": "package.parity",
            "compares": "dev access trace against the package access trace",
            "failure": "a dev open with no package account",
            "report_only": ["drift", "dieted", "cooked", "never_touched"],
        }));
    }
    let dev_trace = required(params, "dev_trace")?;
    let dev_root = required(params, "dev_root")?;
    let package_trace = required(params, "package_trace")?;
    let package_root = required(params, "package_root")?;
    require_file(dev_trace)?;
    require_file(package_trace)?;
    require_dir(dev_root, "dev_root")?;
    require_dir(package_root, "package_root")?;
    let report = regia_asset::package_parity::diff_paths(
        Path::new(dev_trace),
        Path::new(dev_root),
        Path::new(package_trace),
        Path::new(package_root),
    )
    .map_err(|err| RpcError::internal(format!("package parity could not read a trace: {err}")))?;
    Ok(report.json_value())
}

fn required<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    match opt_str(params, key)? {
        Some(text) if !text.is_empty() => Ok(text),
        _ => Err(bad_params(&format!("`{key}` is required"))),
    }
}

fn bad_params(message: &str) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        "parity_bad_params",
        message,
        "pass dev_trace, dev_root, package_trace and package_root, or dry_run true",
    )
}

fn require_file(path: &str) -> Result<(), RpcError> {
    if Path::new(path).is_file() {
        Ok(())
    } else {
        Err(RpcError::new(
            protocol::NOT_FOUND,
            "parity_trace_missing",
            format!("trace file is missing: {path}"),
            "pass the path of an existing access-trace file",
        ))
    }
}

fn require_dir(path: &str, which: &str) -> Result<(), RpcError> {
    if Path::new(path).is_dir() {
        Ok(())
    } else {
        Err(RpcError::new(
            protocol::INVALID_PARAMS,
            "parity_not_a_directory",
            format!("{which} is not a directory"),
            "pass the dev project directory or the extracted package directory",
        ))
    }
}
