// SPDX-License-Identifier: MPL-2.0
//! CT-14: report locations must not turn filenames into a secret side channel.

use std::path::{Path, PathBuf};
use std::process::Command;

use regia_cli::package_hygiene::{Allowance, Policy, audit_tree, check_tree};
use serde_json::Value;

const PRIVATE: &str = "synthetic-private-location-value";
const CONTENT: &[u8] = b"prefix private-content-fixture suffix";

struct Tree(PathBuf);
impl Tree {
    fn new() -> anyhow::Result<Self> {
        let root = regia_test_utils::unique_temp_dir("hygiene-report-privacy");
        std::fs::create_dir_all(&root)?;
        Ok(Self(root))
    }

    fn write(&self, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
        regia_test_utils::write_atomic(&self.0.join(name), bytes)?;
        Ok(())
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn policy() -> Policy {
    Policy {
        forbidden_strings: vec![PRIVATE.into(), "private-content-fixture".into()],
        ..Policy::default()
    }
}

fn assert_private(text: &str) {
    for secret in [
        PRIVATE,
        "private-content-fixture",
        "prefix private-content",
        "suffix",
    ] {
        assert!(!text.contains(secret), "report leaked a synthetic private value");
    }
}

#[test]
fn findings_and_errors_redact_paths_before_text_json_and_debug() -> anyhow::Result<()> {
    let tree = Tree::new()?;
    tree.write(&format!("{PRIVATE}.txt"), CONTENT)?;
    let error = check_tree(&tree.0, &policy()).expect_err("planted finding");
    assert_private(&error.to_string());
    assert_private(&format!("{error:?}"));
    assert_private(&serde_json::to_string(&error)?);
    let report = error.report.expect("completed report");
    assert_eq!(report.files_scanned, 1);
    assert_eq!(report.bytes_scanned, CONTENT.len() as u64);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(std::fs::read(tree.0.join(format!("{PRIVATE}.txt")))?, CONTENT);
    Ok(())
}

#[test]
fn scrubbed_collisions_keep_distinct_nonreversible_ids() -> anyhow::Result<()> {
    let tree = Tree::new()?;
    for value in ["one-private-value", "two-private-value"] {
        tree.write(&format!("{value}.txt"), CONTENT)?;
    }
    let policy = Policy {
        forbidden_strings: vec![
            "one-private-value".into(),
            "two-private-value".into(),
            "private-content-fixture".into(),
        ],
        ..Policy::default()
    };
    let report = audit_tree(&tree.0, &policy)?;
    let json = serde_json::to_value(&report)?;
    let findings = json["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0]["path"], findings[1]["path"]);
    let ids: Vec<_> = findings
        .iter()
        .map(|f| f["location_id"].as_str().expect("opaque ID"))
        .collect();
    assert_ne!(ids[0], ids[1]);
    for id in ids {
        assert!(
            id.strip_prefix("location-")
                .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
        );
    }
    assert_eq!(report, audit_tree(&tree.0, &policy)?);
    Ok(())
}

#[test]
fn allowance_matches_original_path_even_when_name_scanning_is_off() -> anyhow::Result<()> {
    let tree = Tree::new()?;
    let name = format!("{PRIVATE}.txt");
    tree.write(&name, CONTENT)?;
    let mut policy = policy();
    policy.scan_names = false;
    policy.scan_secrets = false;
    policy.allow.push(Allowance {
        rule: "forbidden_string_2".into(),
        path: name,
        reason: "Synthetic reviewed fixture".into(),
        definition: None,
    });
    let report = check_tree(&tree.0, &policy)?;
    assert_eq!(report.allowed_findings, 1);
    assert!(report.findings.is_empty());
    policy.allow.clear();
    assert_private(&serde_json::to_string(&audit_tree(&tree.0, &policy)?)?);
    Ok(())
}

#[test]
fn size_and_finding_limit_locations_are_scrubbed() -> anyhow::Result<()> {
    for finding_limit in [false, true] {
        let tree = Tree::new()?;
        tree.write(&format!("{PRIVATE}.txt"), CONTENT)?;
        let mut policy = policy();
        if finding_limit {
            policy.forbidden_strings.push("prefix".into());
            policy.max_findings = 1;
        } else {
            policy.max_file_bytes = 1;
        }
        let error = audit_tree(&tree.0, &policy).expect_err("limit");
        assert_eq!(error.code, "package_hygiene_limit");
        assert_private(&error.to_string());
        let json = serde_json::to_value(&error)?;
        assert!(json["location_id"].as_str().is_some());
        assert_private(&json.to_string());
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn unreadable_file_location_is_scrubbed() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tree = Tree::new()?;
    let name = format!("{PRIVATE}.txt");
    tree.write(&name, CONTENT)?;
    let path = tree.0.join(name);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0))?;
    let result = audit_tree(&tree.0, &policy());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let error = result.expect_err("fixture needs an unprivileged test runner");
    assert_eq!(error.code, "package_hygiene_io");
    assert_private(&error.to_string());
    assert_private(&serde_json::to_string(&error)?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn escaped_control_and_symlink_errors_stay_inside_temp_root() -> anyhow::Result<()> {
    for separator in ["\\", "\n", "\u{1b}", ":", "\"\\n"] {
        let tree = Tree::new()?;
        tree.write(&format!("{PRIVATE}{separator}bad"), CONTENT)?;
        let error = audit_tree(&tree.0, &policy()).expect_err("unsupported name");
        assert_eq!(error.code, "package_hygiene_path");
        assert_private(&error.to_string());
        assert!(!error.to_string().chars().any(char::is_control));
        assert_private(&serde_json::to_string(&error)?);
    }
    let tree = Tree::new()?;
    tree.write("target", b"safe")?;
    std::os::unix::fs::symlink(tree.0.join("target"), tree.0.join(PRIVATE))?;
    let error = audit_tree(&tree.0, &policy()).expect_err("symlink refused");
    assert_eq!(error.code, "package_hygiene_path");
    assert_private(&error.to_string());
    Ok(())
}

fn cli(project: &Path, json: bool) -> anyhow::Result<std::process::Output> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_regia"));
    cmd.args(["package", "--platform", "linux", "--project"])
        .arg(project)
        .arg("--audit-dir")
        .arg(project.join("stage"));
    if json {
        cmd.arg("--json");
    }
    Ok(cmd.output()?)
}

#[test]
fn real_cli_text_and_json_hide_literal_and_token_filenames() -> anyhow::Result<()> {
    for name in [PRIVATE.to_owned(), format!("ghp_{}", "x".repeat(36))] {
        let tree = Tree::new()?;
        std::fs::create_dir(tree.0.join("stage"))?;
        tree.write(
            "Regia.toml",
            format!("[package_hygiene]\nforbidden_strings=['{PRIVATE}', 'private-content-fixture']\n")
                .as_bytes(),
        )?;
        tree.write(&format!("stage/{name}.txt"), CONTENT)?;
        for json in [false, true] {
            let output = cli(&tree.0, json)?;
            assert_eq!(output.status.code(), Some(1));
            for bytes in [&output.stdout, &output.stderr] {
                let text = String::from_utf8_lossy(bytes);
                assert_private(&text);
                assert!(!text.contains(&name), "filename leaked");
            }
            if json {
                let report: Value = serde_json::from_slice(&output.stdout)?;
                assert!(report["hygiene"]["findings"][0]["location_id"].is_string());
            }
        }
    }
    Ok(())
}

#[test]
fn authorized_local_review_resolves_ids_without_retaining_paths_in_reports() -> anyhow::Result<()> {
    use regia_cli::package_hygiene::audit_tree_with_locations;
    let tree = Tree::new()?;
    let name = format!("{PRIVATE}.txt");
    tree.write(&name, CONTENT)?;
    let mut visits = Vec::new();
    let report = audit_tree_with_locations(&tree.0, &policy(), &mut |id, path| {
        visits.push((id.to_owned(), path.to_owned()));
    })?;
    assert_eq!(
        visits.len(),
        2,
        "one root and one file, not one callback per match"
    );
    assert_eq!(visits[0], ("location-0".into(), ".".into()));
    assert_eq!(visits[1], (report.findings[0].location_id.clone(), name));
    assert_eq!(report, audit_tree(&tree.0, &policy())?);
    assert_private(&serde_json::to_string(&report)?);
    Ok(())
}

#[test]
fn privacy_is_independent_of_detection_flags_and_reuses_core_scrubbing() -> anyhow::Result<()> {
    for name in [
        format!("ghp_{}", "x".repeat(36)),
        "token=synthetic-token-value".into(),
        "contact@example.invalid".into(),
        "contact%40example.invalid".into(),
    ] {
        let tree = Tree::new()?;
        tree.write(&name, CONTENT)?;
        let policy = Policy {
            scan_names: false,
            scan_secrets: false,
            scan_emails: false,
            ..policy()
        };
        let error = check_tree(&tree.0, &policy).expect_err("content finding");
        for rendered in [
            error.to_string(),
            format!("{error:?}"),
            serde_json::to_string(&error)?,
        ] {
            assert_private(&rendered);
            assert!(!rendered.contains(&name), "unscrubbed location");
            assert!(!rendered.contains("synthetic-token-value"));
        }
    }
    Ok(())
}

#[test]
fn overlapping_literals_and_utf8_byte_patterns_redact_whole_locations() -> anyhow::Result<()> {
    let tree = Tree::new()?;
    tree.write("abécd.txt", CONTENT)?;
    let policy = Policy {
        forbidden_strings: vec!["abé".into(), "écd".into(), "private-content-fixture".into()],
        forbidden_patterns: vec![r"(?-u:\xA9)".into()],
        ..Policy::default()
    };
    let report = audit_tree(&tree.0, &policy)?;
    assert_eq!(report.findings[0].path, "[redacted]");
    assert!(!serde_json::to_string(&report)?.contains("abé"));
    let byte_only = Policy {
        forbidden_strings: vec!["private-content-fixture".into()],
        forbidden_patterns: policy.forbidden_patterns,
        ..Policy::default()
    };
    assert_eq!(audit_tree(&tree.0, &byte_only)?.findings[0].path, "[redacted]");
    Ok(())
}

#[test]
fn location_work_is_bounded_by_entries_and_not_by_content_matches() -> anyhow::Result<()> {
    use regia_cli::package_hygiene::audit_tree_with_locations;
    let tree = Tree::new()?;
    for i in 0..16 {
        tree.write(&format!("{i:02}-{PRIVATE}"), &CONTENT.repeat(1024))?;
    }
    let mut visits = 0;
    let mut policy = policy();
    policy.max_entries = 16;
    let report = audit_tree_with_locations(&tree.0, &policy, &mut |_, _| visits += 1)?;
    assert_eq!(visits, 17);
    assert_eq!(report.files_scanned, 16);
    assert_eq!(report.bytes_scanned, 16 * 1024 * CONTENT.len() as u64);
    assert_eq!(
        report.findings.len(),
        16,
        "one location/finding per file, not per match"
    );
    policy.max_entries = 15;
    visits = 0;
    let error = audit_tree_with_locations(&tree.0, &policy, &mut |_, _| visits += 1).expect_err("entry cap");
    assert_eq!(error.code, "package_hygiene_limit");
    assert_eq!(visits, 1, "queue budget refuses before visiting children");
    Ok(())
}

#[test]
fn pak_locations_keep_asset_ids_and_never_reveal_manifest_names() -> anyhow::Result<()> {
    use regia_asset::{AssetId, AssetManifest, ManifestEntry};
    use regia_asset_pipeline::pak_writer::{PakWriteOptions, write_pak};
    use regia_core::id::TypePathHash;
    let cooked = Tree::new()?;
    let staged = Tree::new()?;
    let id = AssetId::NIL;
    cooked.write(&regia_asset::cooked_file_name(id), CONTENT)?;
    let manifest = AssetManifest {
        entries: vec![ManifestEntry {
            path: format!("{PRIVATE}/never-opened"),
            id,
            type_path: "opaque".into(),
        }],
    };
    write_pak(
        &staged.0.join(format!("{PRIVATE}.fpak")),
        &cooked.0,
        &[(id, TypePathHash(0))],
        &manifest,
        &PakWriteOptions::default(),
    )?;
    let policy = Policy {
        deny_terms: vec![format!("sub:{PRIVATE}")],
        ..policy()
    };
    let mut revealed = Vec::new();
    let report =
        regia_cli::package_hygiene::audit_tree_with_locations(&staged.0, &policy, &mut |_, path| {
            revealed.push(path.to_owned())
        })?;
    assert_eq!(report.pak_entries_scanned, 1);
    let decoded: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.path.ends_with(&format!("::{id}")))
        .collect();
    assert!(decoded.iter().any(|f| f.source == "name"));
    assert!(decoded.iter().any(|f| f.source == "content"));
    for finding in decoded {
        assert_eq!(finding.path, format!("[redacted]::{id}"));
        assert_eq!(finding.location_id, format!("location-1::{id}"));
    }
    assert!(revealed.iter().all(|p| !p.contains("never-opened")));
    assert_private(&serde_json::to_string(&report)?);
    Ok(())
}

#[test]
fn package_agent_report_round_trip_supports_wait_until_and_expect() -> anyhow::Result<()> {
    use regia_agent::cook_package::{CliTally, RegiaCliProgram};
    use regia_agent::{AgentConfig, AgentPermissions, LocalReply, call_local};
    use regia_app::prelude::*;
    use serde_json::json;
    let tree = Tree::new()?;
    std::fs::create_dir(tree.0.join("stage"))?;
    tree.write(
        "Regia.toml",
        format!("[package_hygiene]\nforbidden_strings=['{PRIVATE}', 'private-content-fixture']\n").as_bytes(),
    )?;
    tree.write(&format!("stage/{PRIVATE}"), CONTENT)?;
    // Build plugin schedules without finishing plugins or opening a network endpoint/window.
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_agent::AgentPlugin));
    let mut config = AgentConfig::default();
    config.permissions.insert("build".into());
    app.world_mut()
        .insert_resource(AgentPermissions { config, source: None });
    app.world_mut().insert_resource(regia_agent::AgentSettings {
        project_dir: Some(tree.0.clone()),
    });
    app.world_mut()
        .insert_resource(RegiaCliProgram(PathBuf::from(env!("CARGO_BIN_EXE_regia"))));
    let LocalReply::Done(reply) = call_local(
        app.world_mut(),
        "agent.package",
        json!({"audit_dir":"stage", "platform":"linux", "dry_run":true}),
        None,
    ) else {
        panic!("preview must be immediate")
    };
    let reply = reply.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let args: Vec<_> = reply["command"]
        .as_array()
        .expect("command")
        .iter()
        .map(|s| s.as_str().expect("argument").to_owned())
        .collect();
    let output = std::process::Command::new(&args[0])
        .args(&args[1..])
        .current_dir(&tree.0)
        .output()?;
    let mut tally = CliTally::default();
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    for line in stdout.lines().chain(stderr.lines()) {
        tally.feed(line);
    }
    let result = tally.result(output.status.code(), false, &args);
    assert_eq!(result["report"]["error"]["code"], "package_hygiene_failed");
    assert_eq!(
        result["report"]["hygiene"]["findings"][0]["location_id"],
        "location-1"
    );
    assert_private(&result.to_string());
    assert_private(&stderr);
    // A local verification host publishes the actual job report to its event tap. The
    // standard package job is unchanged; this adapter is only the integration workflow.
    app.world_mut()
        .resource_mut::<regia_agent::AgentEventLog>()
        .push_host(0, "hygiene_review", result);
    let condition = json!({"events_contain":{"kind":"hygiene_review", "contains":"location-1"}});
    let LocalReply::Pending(wait) = call_local(
        app.world_mut(),
        "play.wait_until",
        json!({"condition": condition, "timeout_ms":120000}),
        None,
    ) else {
        panic!("wait must be deferred")
    };
    app.world_mut().run_schedule(Last);
    let response = wait.try_recv()?;
    assert_eq!(response.result.expect("wait result")["satisfied"], true);
    let LocalReply::Done(reply) = call_local(
        app.world_mut(),
        "play.expect",
        json!({"assertions":[
            {"name":"report has opaque identity", "condition":condition},
            {"name":"report has no private filename", "condition":{"not":{"events_contain":{"kind":"hygiene_review", "contains":PRIVATE}}}},
            {"name":"report has no matched snippet", "condition":{"not":{"events_contain":{"kind":"hygiene_review", "contains":"private-content-fixture"}}}}
        ]}),
        None,
    ) else {
        panic!("expect must be immediate")
    };
    assert_eq!(reply.map_err(|e| anyhow::anyhow!("{e:?}"))?["pass"], true);
    Ok(())
}

#[test]
fn cli_incomplete_scan_error_has_a_safe_location_and_identity() -> anyhow::Result<()> {
    let tree = Tree::new()?;
    std::fs::create_dir(tree.0.join("stage"))?;
    tree.write(
        "Regia.toml",
        format!("[package_hygiene]\nforbidden_strings=['{PRIVATE}']\nmax_file_bytes=1\n").as_bytes(),
    )?;
    tree.write(&format!("stage/{PRIVATE}"), CONTENT)?;
    let output = cli(&tree.0, true)?;
    assert_eq!(output.status.code(), Some(1));
    let response: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(response["error"]["code"], "package_hygiene_limit");
    assert_eq!(response["error"]["message"], "[redacted]");
    assert_eq!(response["error"]["location_id"], "location-1");
    assert!(response["hygiene"].is_null());
    assert_private(&response.to_string());
    assert_private(&String::from_utf8(output.stderr)?);
    Ok(())
}

#[test]
#[ignore = "release timing measurement on an assigned N150 or Deck; not a correctness budget"]
fn report_location_release_measurement() -> anyhow::Result<()> {
    let tree = Tree::new()?;
    for i in 0..16 {
        tree.write(&format!("{i:02}-{PRIVATE}"), &CONTENT.repeat(1024))?;
    }
    let policy = policy();
    // Warm filesystem/regex code before measuring complete audits, including compilation.
    std::hint::black_box(audit_tree(&tree.0, &policy)?);
    let start = std::time::Instant::now();
    let mut files = 0;
    let mut bytes = 0;
    for _ in 0..16 {
        let report = std::hint::black_box(audit_tree(&tree.0, &policy)?);
        files += report.files_scanned;
        bytes += report.bytes_scanned;
    }
    println!(
        "hygiene privacy: audits=16 files={files} bytes={bytes} total_us={}",
        start.elapsed().as_micros()
    );
    Ok(())
}

#[test]
fn malformed_pak_errors_never_receive_a_private_reader_label() -> anyhow::Result<()> {
    let tree = Tree::new()?;
    tree.write(&format!("{PRIVATE}.fpak"), b"not a pak")?;
    let error = audit_tree(&tree.0, &policy()).expect_err("invalid pak");
    assert_eq!(error.code, "package_hygiene_pak");
    assert_eq!(error.location, "[redacted]");
    assert_eq!(error.location_id.as_deref(), Some("location-1"));
    assert_private(&error.to_string());
    assert_private(&format!("{error:?}"));
    assert_private(&serde_json::to_string(&error)?);
    Ok(())
}
