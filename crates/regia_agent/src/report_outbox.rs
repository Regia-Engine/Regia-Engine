// SPDX-License-Identifier: MPL-2.0
//! `report.outbox_status`: the incremental report outbox's pending bytes per source and its last
//! error (plan row NL-08, ADR-8402, book/agents/report-outbox.md). Read-only: it reads the ledger
//! and the logs' sizes, never their contents, and sends nothing.

use regia_crash::CrashReporter;
use regia_crash::outbox::{Outbox, OutboxSources};
use regia_ecs::prelude::World;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// The ledger and the session logs of the configured logs directory, as the startup flush sees
/// them.
pub(crate) fn outbox_status(world: &World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_known_and_types("report.outbox_status", params, &[])?;
    let reporter = world.get_resource::<CrashReporter>();
    let configured = reporter.and_then(|r| r.outbox.clone());
    let enabled = reporter.is_some_and(|r| r.outbox.is_some() && r.enabled());
    let settings = configured.clone().unwrap_or_default();
    // The mounted `logs://` directory (where the platform plugin points the crash writer too),
    // else the crash directory itself.
    let logs_dir = regia_framework::report_sources::ReportSourceDirs::from_world(world)
        .logs
        .or_else(regia_platform::crash::crash_dir);
    let ledger = ledger_for(&settings, logs_dir.as_deref())?;
    let sources = logs_dir
        .as_deref()
        .map(OutboxSources::from_logs_dir)
        .unwrap_or_default();
    let status = Outbox::open(&ledger, settings.clone()).status(&sources);
    let rows: Vec<Value> = status
        .sources
        .iter()
        .map(|source| {
            json!({
                "path": source.path.to_string_lossy(),
                "bytes": source.bytes,
                "confirmed": source.confirmed,
                "pending_bytes": source.pending_bytes,
                "tracked": source.tracked,
                "crash_resend": source.crash_resend,
                "last_error": source.last_error,
            })
        })
        .collect();
    Ok(json!({
        "configured": configured.is_some(),
        "enabled": enabled,
        "ledger": status.ledger.to_string_lossy(),
        "part_cap": settings.effective_part_cap(),
        "max_rows": settings.max_rows,
        "rows": status.rows,
        "pending_bytes": status.pending_bytes,
        "sources": rows,
        "last_error": status.last_error,
        "load_error": status.load_error,
    }))
}

/// The ledger file, or `outbox_unavailable` when there is no logs directory and no explicit file.
fn ledger_for(
    settings: &regia_crash::outbox::OutboxSettings,
    logs_dir: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, RpcError> {
    settings.ledger_path(logs_dir).ok_or_else(|| {
        RpcError::new(
            protocol::NOT_FOUND,
            "outbox_unavailable",
            "no logs directory is configured and the outbox names no ledger file",
            "the platform plugin configures the logs directory; set CrashReporter::outbox.ledger otherwise",
        )
    })
}

#[cfg(test)]
mod tests {
    use regia_app::App;
    use regia_crash::outbox::OutboxSettings;
    use regia_platform::{DirMount, MountPoint, Vfs};
    use regia_test_utils::{TempDir, write_atomic};
    use serde_json::json;

    use super::*;

    fn read_only_app() -> App {
        let mut app = App::new();
        app.insert_resource(crate::AgentPermissions {
            config: crate::AgentConfig::fail_closed(),
            source: None,
        });
        app
    }

    #[test]
    fn outbox_status_dispatch_reports_pending_bytes_and_the_last_error() {
        let dir = TempDir::new("agent-report-outbox");
        let log = dir.join("2026-11-17_10-00-00.log");
        write_atomic(&log, b"0123456789\nabcdef\n").expect("log");
        let ledger = dir.join("outbox.ron");
        let ledger_text = format!(
            "(version: 1, next_seq: 1, rows: [(path: {:?}, identity: (size: 11, modified_unix_ms: 0, \
             header_len: 0, header_hash: 0), confirmed: 11, seq: 0, last_error: Some(\"2026-11-17_10-00-00.log;11-18: \
             server answered HTTP 503\"))], crash_markers: [], last_error: Some(\"server answered HTTP 503\"))",
            log.to_string_lossy()
        );
        write_atomic(&ledger, ledger_text.as_bytes()).expect("ledger");
        let mut app = read_only_app();
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::Logs, DirMount::writable(dir.path()));
        app.insert_resource(vfs);
        app.insert_resource(CrashReporter {
            endpoint: "https://reports.example.com/outbox".into(),
            consent: true,
            outbox: Some(OutboxSettings::default()),
            ..CrashReporter::default()
        });
        let result = crate::dispatch::call_in_process(app.world_mut(), "report.outbox_status", json!({}))
            .expect("synchronous")
            .expect("read-only query");
        assert_eq!(result["ledger"], ledger.to_string_lossy().as_ref());
        assert_eq!(result["configured"], true);
        assert_eq!(result["enabled"], true);
        assert_eq!(result["part_cap"], 256 * 1024);
        assert_eq!(result["rows"], 1);
        assert_eq!(result["load_error"], Value::Null);
        assert_eq!(result["last_error"], "server answered HTTP 503");
        let source = result["sources"]
            .as_array()
            .expect("sources")
            .iter()
            .find(|s| s["path"] == log.to_string_lossy().as_ref())
            .expect("the session log is listed");
        assert_eq!(source["bytes"], 18);
        assert_eq!(source["confirmed"], 11);
        assert_eq!(source["pending_bytes"], 7);
        assert_eq!(source["tracked"], true);
        assert!(
            source["last_error"]
                .as_str()
                .is_some_and(|e| e.contains("HTTP 503")),
            "{source}"
        );
        assert_eq!(result["pending_bytes"], 7);
        // Read-only: neither the ledger nor the log changed.
        assert_eq!(std::fs::read(&ledger).expect("ledger"), ledger_text.as_bytes());
        assert_eq!(std::fs::read(&log).expect("log"), b"0123456789\nabcdef\n");
    }

    #[test]
    fn no_logs_directory_and_no_ledger_is_outbox_unavailable() {
        let error = ledger_for(&OutboxSettings::default(), None).expect_err("nowhere to look");
        assert_eq!(error.code, protocol::NOT_FOUND);
        assert_eq!(error.data.as_ref().expect("data")["code"], "outbox_unavailable");
        let explicit = OutboxSettings {
            ledger: Some("/state/outbox.ron".into()),
            ..OutboxSettings::default()
        };
        assert_eq!(
            ledger_for(&explicit, None).expect("explicit"),
            std::path::PathBuf::from("/state/outbox.ron")
        );
    }

    #[test]
    fn outbox_status_refuses_unknown_params() {
        let mut app = read_only_app();
        let error =
            crate::dispatch::call_in_process(app.world_mut(), "report.outbox_status", json!({"path": "/"}))
                .expect("synchronous")
                .expect_err("no arbitrary paths");
        assert_eq!(error.code, crate::protocol::INVALID_PARAMS);
    }
}
