// SPDX-License-Identifier: MPL-2.0
//! `diagnostics.crash_reports` (specs/23 § 3.7): list pending crash reports or read one by name.
//!
//! Crash reports are written by the panic hook (`regia_platform::crash`) to `logs://crash-*.txt`;
//! this handler surfaces them to agents and tools so a failure can be diagnosed without pulling the
//! file by hand.

use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Lists pending crash reports, or reads one by its file name.
pub(crate) fn crash_reports(params: &Value) -> Result<Value, RpcError> {
    if params.get("name").is_some_and(|v| !v.is_null() && !v.is_string()) {
        return Err(RpcError::invalid_params("`name` must be a string"));
    }
    match crate::params::opt_str(params, "name")? {
        Some(name) => {
            let content = regia_platform::crash::read(name).ok_or_else(|| {
                RpcError::not_found(
                    format!("no crash report {name}"),
                    "call diagnostics.crash_reports without a name to list pending reports",
                )
            })?;
            Ok(json!({ "name": name, "content": content }))
        }
        None => {
            let reports: Vec<Value> = regia_platform::crash::pending()
                .into_iter()
                .map(|report| {
                    json!({
                        "name": report.name,
                        "bytes": report.bytes,
                        "modified_unix_ms": report.modified_unix_ms,
                    })
                })
                .collect();
            let count = reports.len();
            Ok(json!({ "reports": reports, "count": count }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Points the crash directory at a temp dir with one seeded report.
    fn seeded(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("regia-agent-crash-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        regia_platform::crash::configure(
            Some(dir.clone()),
            regia_platform::crash::BuildInfo::detect(),
            regia_platform::OsInfo::detect(),
        );
        std::fs::write(dir.join("crash-2026-10-06_00-00-01.txt"), "boom detail").expect("seed");
        dir
    }

    #[test]
    fn lists_and_reads_pending_reports() {
        let dir = seeded("roundtrip");

        let list = crash_reports(&json!({})).expect("list");
        assert_eq!(list["count"], 1);
        assert_eq!(list["reports"][0]["name"], "crash-2026-10-06_00-00-01.txt");
        assert_eq!(list["reports"][0]["bytes"], 11);

        let read = crash_reports(&json!({ "name": "crash-2026-10-06_00-00-01.txt" })).expect("read");
        assert_eq!(read["content"], "boom detail");

        let missing = crash_reports(&json!({ "name": "crash-nope.txt" }));
        let error = missing.expect_err("unknown report 404s");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("F0003")
        );

        // Path traversal never reads outside the crash directory.
        assert!(crash_reports(&json!({ "name": "../Cargo.toml" })).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_string_name_is_refused_not_treated_as_a_listing() {
        for bad in [
            json!({ "name": 5 }),
            json!({ "name": ["a"] }),
            json!({ "name": true }),
        ] {
            let err = crash_reports(&bad).expect_err("type");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{bad}");
        }
    }

    #[test]
    fn names_that_could_leave_the_crash_directory_or_are_empty_are_not_found() {
        for name in [
            "",
            ".",
            "..",
            "/etc/passwd",
            "a/b.txt",
            "..\\x",
            "crash-\u{0}.txt",
        ] {
            assert!(crash_reports(&json!({ "name": name })).is_err(), "{name:?}");
        }
    }
}
