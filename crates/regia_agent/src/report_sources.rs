// SPDX-License-Identifier: MPL-2.0
//! Read-only local artifact inventory (`diagnostics.report_sources`, ADR-8058).

use regia_ecs::prelude::World;
use regia_framework::report_sources::{ReportSourceDirs, ReportSourceOptions};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Lists the configured artifact inventory without accepting caller-supplied paths.
pub(crate) fn report_sources(world: &World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_known_and_types("diagnostics.report_sources", params, &[])?;
    let report = ReportSourceDirs::from_world(world).list(&ReportSourceOptions::default());
    let artifacts: Vec<_> = report
        .artifacts
        .iter()
        .map(|artifact| {
            let key = artifact.pipeline_key.as_ref().map(|key| {
                json!({
                    "backend": key.backend,
                    "vendor": key.vendor,
                    "device": key.device,
                    "driver": key.driver,
                    "driver_info": key.driver_info,
                    "wgpu_version": key.wgpu_version,
                    "shader_hash": key.shader_hash,
                })
            });
            json!({
                "kind": artifact.kind.as_str(),
                "path": artifact.path.to_string_lossy(),
                "bytes": artifact.bytes,
                "modified_unix_ms": artifact.modified_unix_ms,
                "pipeline_key": key,
            })
        })
        .collect();
    let issues: Vec<_> = report
        .issues
        .iter()
        .map(|issue| {
            json!({
                "path": issue.path.to_string_lossy(), "code": issue.code, "message": issue.message,
            })
        })
        .collect();
    let path = |p: &Option<std::path::PathBuf>| p.as_ref().map(|p| p.to_string_lossy().into_owned());
    Ok(json!({
        "directories": {
            "logs": path(&report.directories.logs),
            "crashes": path(&report.directories.crashes),
            "feedback": path(&report.directories.feedback),
            "pipeline_cache": path(&report.directories.pipeline_cache),
        },
        "count": artifacts.len(),
        "artifacts": artifacts,
        "issues": issues,
        "truncated": report.truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::App;
    use regia_platform::{DirMount, MountPoint, Vfs};
    use regia_test_utils::{TempDir, write_atomic};

    #[test]
    fn report_sources_dispatch_lists_the_mounted_log_directory_read_only() {
        let dir = TempDir::new("agent-report-sources");
        write_atomic(&dir.join("session.log"), b"log").expect("log");
        let feedback_dir = TempDir::new("agent-report-sources-feedback");
        let dump = feedback_dir.join("native-crash-1600000000000.dmp");
        write_atomic(&dump, b"preserved dump").expect("fallback dump");
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::Logs, DirMount::writable(dir.path()));
        let mut app = App::new();
        app.insert_resource(vfs);
        app.insert_resource(regia_framework::feedback::FeedbackConfig {
            dir: Some(feedback_dir.to_path_buf()),
            ..Default::default()
        });
        app.insert_resource(crate::AgentPermissions {
            config: crate::AgentConfig::fail_closed(),
            source: None,
        });
        let result =
            crate::dispatch::call_in_process(app.world_mut(), "diagnostics.report_sources", json!({}))
                .expect("synchronous")
                .expect("read-only query");
        assert_eq!(
            result["directories"]["logs"],
            dir.path().to_string_lossy().as_ref()
        );
        assert!(
            result["artifacts"]
                .as_array()
                .expect("array")
                .iter()
                .any(|artifact| artifact["kind"] == "log"
                    && artifact["path"] == dir.join("session.log").to_string_lossy().as_ref()
                    && artifact["bytes"] == 3
                    && artifact["pipeline_key"].is_null())
        );
        assert!(
            result["artifacts"]
                .as_array()
                .expect("array")
                .iter()
                .any(|artifact| artifact["kind"] == "crash"
                    && artifact["path"] == dump.to_string_lossy().as_ref()
                    && artifact["bytes"] == 14
                    && artifact["pipeline_key"].is_null())
        );
        assert_eq!(
            std::fs::read(dir.join("session.log")).expect("still there"),
            b"log"
        );
    }

    #[test]
    fn report_sources_refuses_caller_supplied_paths() {
        let mut app = App::new();
        let error = crate::dispatch::call_in_process(
            app.world_mut(),
            "diagnostics.report_sources",
            json!({"path": "/"}),
        )
        .expect("synchronous")
        .expect_err("no arbitrary paths");
        assert_eq!(error.code, crate::protocol::INVALID_PARAMS);
    }
}
