// SPDX-License-Identifier: MPL-2.0
//! C-09: actual CLI rejection, redacted JSON, and the agent's command/report surface.

use std::path::{Path, PathBuf};
use std::process::Command;

use regia_agent::cook_package::{CliTally, RegiaCliProgram};
use regia_agent::{AgentConfig, AgentPermissions, LocalReply, call_local};
use regia_app::prelude::*;
use serde_json::{Value, json};

struct Project(PathBuf);
impl Project {
    fn new() -> anyhow::Result<Self> {
        let root = regia_test_utils::unique_temp_dir("package-hygiene-e2e");
        std::fs::create_dir_all(root.join("dist/player"))?;
        regia_test_utils::write_atomic(
            &root.join("Regia.toml"),
            b"[package_hygiene]\nforbidden_strings=['--developer-console']\n",
        )?;
        Ok(Self(root))
    }
    fn stage(&self) -> PathBuf {
        self.0.join("dist/player")
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn audit(project: &Project) -> anyhow::Result<std::process::Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["package", "--platform", "linux", "--audit-dir"])
        .arg(project.stage())
        .arg("--project")
        .arg(&project.0)
        .arg("--json")
        .output()?)
}

#[test]
fn planted_dev_flag_fails_real_cli_and_clean_package_passes() -> anyhow::Result<()> {
    let project = Project::new()?;
    let player = project.stage().join("player");
    let planted = b"\x7fELF\0merged--developer-consoleNEXT";
    regia_test_utils::write_atomic(&player, planted)?;
    let output = audit(&project)?;
    assert_eq!(output.status.code(), Some(1));
    let response: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"]["code"], "package_hygiene_failed");
    assert_eq!(response["hygiene"]["findings"][0]["rule"], "forbidden_string_1");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("--developer-console"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("--developer-console"));
    assert_eq!(
        std::fs::read(&player)?,
        planted,
        "audit must not rewrite staged files"
    );
    assert!(
        !project.0.join("build").exists(),
        "audit must never start cooking"
    );
    regia_test_utils::write_atomic(&player, b"\x7fELF\0release")?;
    let output = audit(&project)?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(response["audit_only"], true);
    assert_eq!(response["hygiene"]["files_scanned"], 1);
    assert!(response.get("artifact").is_none(), "audit creates no artifact");
    Ok(())
}

#[test]
fn secret_scan_runs_with_default_project_policy() -> anyhow::Result<()> {
    let project = Project::new()?;
    regia_test_utils::write_atomic(&project.0.join("Regia.toml"), b"[project]\nname='Sample'\n")?;
    let secret = format!("ghp_{}", "x".repeat(36));
    regia_test_utils::write_atomic(&project.stage().join("config.txt"), secret.as_bytes())?;
    let output = audit(&project)?;
    assert!(!output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(response["hygiene"]["findings"][0]["rule"], "secret_github_token");
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&secret));
    Ok(())
}

#[test]
fn malformed_policy_and_missing_directory_are_structured_failures() -> anyhow::Result<()> {
    let project = Project::new()?;
    regia_test_utils::write_atomic(
        &project.0.join("Regia.toml"),
        b"[package_hygiene]\nforbidden_patterns=['private[']\n",
    )?;
    let output = audit(&project)?;
    let response: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(response["error"]["code"], "package_hygiene_config");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private["));
    regia_test_utils::write_atomic(&project.0.join("Regia.toml"), b"")?;
    std::fs::remove_dir_all(project.stage())?;
    let output = audit(&project)?;
    let response: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(response["error"]["code"], "package_hygiene_io");
    Ok(())
}

#[test]
fn normal_packaging_redacts_malformed_policy_before_manifest_loading() -> anyhow::Result<()> {
    let project = Project::new()?;
    let marker = "private-policy-fixture";
    regia_test_utils::write_atomic(
        &project.0.join("Regia.toml"),
        format!("[package_hygiene]\nforbidden_strings = [\"{marker}\n").as_bytes(),
    )?;
    for profile in ["shipping", "dev"] {
        let output = Command::new(env!("CARGO_BIN_EXE_regia"))
            .args([
                "package",
                "--platform",
                "linux",
                "--profile",
                profile,
                "--project",
            ])
            .arg(&project.0)
            .arg("--json")
            .output()?;
        assert_eq!(output.status.code(), Some(1));
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(marker),
            "normal packaging must redact malformed policy diagnostics"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains(marker));
        let response: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(response["error"]["code"], "package_hygiene_config");
        assert!(!project.0.join("build").exists());
    }
    Ok(())
}

fn agent_world(root: &Path) -> World {
    let mut world = World::new();
    let mut config = AgentConfig::default();
    config.permissions.insert("build".into());
    world.insert_resource(AgentPermissions { config, source: None });
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.to_path_buf()),
    });
    world.insert_resource(RegiaCliProgram(PathBuf::from(env!("CARGO_BIN_EXE_regia"))));
    world
}

#[test]
fn agent_dispatch_builds_the_audit_command_and_job_tally_retains_its_failure() -> anyhow::Result<()> {
    let project = Project::new()?;
    regia_test_utils::write_atomic(&project.stage().join("player"), b"\0--developer-console")?;
    let mut world = agent_world(&project.0);
    let LocalReply::Done(reply) = call_local(
        &mut world,
        "agent.package",
        json!({"audit_dir":"dist/player", "platform":"linux", "dry_run":true}),
        None,
    ) else {
        panic!("preview is immediate")
    };
    let reply = reply.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let args: Vec<&str> = reply["command"]
        .as_array()
        .expect("command")
        .iter()
        .map(|v| v.as_str().expect("string"))
        .collect();
    assert!(args.contains(&"--audit-dir=dist/player"));
    // Execute the exact command returned by the dispatcher. Process completion synchronizes
    // the test; feed the real output to the same tally the asynchronous job uses.
    let output = Command::new(args[0])
        .args(&args[1..])
        .current_dir(&project.0)
        .output()?;
    let mut tally = CliTally::default();
    for line in String::from_utf8(output.stdout)?.lines() {
        tally.feed(line);
    }
    let result = tally.result(
        output.status.code(),
        false,
        &args.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
    );
    assert_eq!(result["ok"], false);
    assert_eq!(result["report"]["error"]["code"], "package_hygiene_failed");
    assert_eq!(
        result["report"]["hygiene"]["findings"][0]["rule"],
        "forbidden_string_1"
    );
    assert!(result["artifact"].is_null());
    Ok(())
}

#[test]
fn stage_rule_ids_have_error_explanations() -> anyhow::Result<()> {
    let project = Project::new()?;
    let mut world = agent_world(&project.0);
    world
        .resource_mut::<AgentPermissions>()
        .config
        .permissions
        .insert("read".into());
    for code in ["build_path", "dev_flag", "stage_allow", "internal_notes"] {
        let LocalReply::Done(reply) = call_local(&mut world, "errors.explain", json!({ "code": code }), None)
        else {
            anyhow::bail!("explain is immediate");
        };
        let reply = reply.map_err(|error| anyhow::anyhow!("{error:?}"))?;
        assert_eq!(reply["code"], code);
        assert!(
            reply["meaning"]
                .as_str()
                .unwrap_or("")
                .contains("package hygiene")
        );
        assert!(reply["fix"].as_str().unwrap_or("").contains("package-hygiene.md"));
    }
    Ok(())
}
