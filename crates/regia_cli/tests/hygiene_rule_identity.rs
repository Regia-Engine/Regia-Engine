// SPDX-License-Identifier: MPL-2.0
//! Stable package-hygiene rule identity (ADR-8247).
//!
//! Positional allowances on the released base follow list order. These tests require an
//! allowance to keep waiving the rule it was bound to when rules are reordered or inserted,
//! to go stale when that rule's body changes, and to survive a one-time migration of the
//! original index.

use std::path::{Path, PathBuf};
use std::process::Command;

use regia_agent::cook_package::{CliTally, RegiaCliProgram};
use regia_agent::{AgentConfig, AgentPermissions, LocalReply, call_local};
use regia_app::prelude::*;
use regia_cli::package_hygiene::{
    Allowance, OBSOLETE_ALLOWANCE, Policy, Rule, audit_tree, definition_digest,
};
use serde_json::{Value, json};

const KEEP: &str = "KEEP_PAIR_7c2e";
const OTHER: &str = "OTHER_PAIR_7c2e";
const INSERTED: &str = "INSERTED_PAIR_7c2e";
const CHANGED: &str = "CHANGED_PAIR_7c2e";

struct Temp(PathBuf);

impl Temp {
    fn new(label: &str) -> Self {
        Self(regia_test_utils::unique_temp_dir(label))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_file(root: &Path, relative: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let path = root.join(relative);
    let parent = path.parent().ok_or_else(|| anyhow::anyhow!("missing parent"))?;
    std::fs::create_dir_all(parent)?;
    regia_test_utils::write_atomic(&path, bytes)?;
    Ok(())
}

fn pair_tree(bytes: &[u8]) -> anyhow::Result<Temp> {
    let temp = Temp::new("hygiene-rule-identity");
    write_file(temp.path(), "reviewed/keep.txt", bytes)?;
    Ok(temp)
}

fn json_of(report: &impl serde::Serialize) -> anyhow::Result<String> {
    Ok(serde_json::to_string(report)?)
}

fn assert_redacted(json: &str) {
    for literal in [KEEP, OTHER, INSERTED, CHANGED] {
        assert!(!json.contains(literal), "report exposed a policy literal");
    }
}

fn legacy_manifest() -> String {
    format!(
        "[package_hygiene]\nforbidden_strings = ['{KEEP}', '{OTHER}']\n\
         [[package_hygiene.allow]]\nrule = 'forbidden_string_1'\n\
         path = 'reviewed/keep.txt'\nreason = 'Reviewed keep pair'\n"
    )
}

#[test]
fn reordering_or_inserting_rules_does_not_retarget_an_allowance() -> anyhow::Result<()> {
    let tree = pair_tree(format!("{KEEP} {OTHER} {INSERTED}").as_bytes())?;
    let migrated = Policy::from_manifest(&legacy_manifest())?.migrated()?;
    assert_eq!(migrated.rules[0].id, "forbidden_string_1");
    assert_eq!(migrated.rules[0].body, KEEP);
    assert_eq!(migrated.allow[0].path, "reviewed/keep.txt");
    assert_eq!(
        migrated.allow[0].definition.as_deref(),
        Some(definition_digest("string", KEEP).as_str())
    );
    let digest = migrated.effective()?.digest;

    let mut reordered = migrated.clone();
    reordered.rules.reverse();
    let report = audit_tree(tree.path(), &reordered)?;
    let json = json_of(&report)?;
    assert_redacted(&json);
    assert_eq!(report.policy_digest, digest);
    assert_eq!(report.allowed_findings, 1);
    assert!(
        report
            .findings
            .iter()
            .all(|finding| finding.rule != "forbidden_string_1")
    );
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.rule == "forbidden_string_2")
    );
    assert!(
        report
            .findings
            .iter()
            .all(|finding| finding.rule != OBSOLETE_ALLOWANCE)
    );

    let mut inserted = migrated.clone();
    inserted.rules.insert(
        0,
        Rule {
            id: "inserted_pair".into(),
            kind: "string".into(),
            body: INSERTED.into(),
        },
    );
    let report = audit_tree(tree.path(), &inserted)?;
    assert_eq!(report.allowed_findings, 1);
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.rule == "inserted_pair")
    );
    assert!(
        report
            .findings
            .iter()
            .all(|finding| finding.rule != "forbidden_string_1")
    );
    assert_ne!(report.policy_digest, digest);
    assert_redacted(&json_of(&report)?);
    assert_redacted(&json_of(&inserted.effective()?)?);
    Ok(())
}

#[test]
fn changing_the_rule_body_invalidates_the_allowance() -> anyhow::Result<()> {
    let tree = pair_tree(format!("{CHANGED} {OTHER}").as_bytes())?;
    let mut policy = Policy::from_manifest(&legacy_manifest())?.migrated()?;
    let keep = policy
        .rules
        .iter_mut()
        .find(|rule| rule.id == "forbidden_string_1")
        .ok_or_else(|| anyhow::anyhow!("missing migrated rule"))?;
    keep.body = CHANGED.into();
    let again = policy.migrated()?;
    assert_eq!(again.allow[0].definition, policy.allow[0].definition);
    let report = audit_tree(tree.path(), &again)?;
    let json = json_of(&report)?;
    assert_redacted(&json);
    assert_eq!(report.allowed_findings, 0);
    assert!(report.findings.iter().any(|finding| {
        finding.rule == OBSOLETE_ALLOWANCE
            && finding.source == "policy"
            && finding.path.ends_with("/forbidden_string_1")
    }));
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.rule == "forbidden_string_1")
    );
    assert!(json.contains(OBSOLETE_ALLOWANCE));
    assert!(json.contains("\"policy_digest\":\"b3:"));
    Ok(())
}

#[test]
fn duplicate_ids_and_empty_reasons_fail_without_echoing_bodies() -> anyhow::Result<()> {
    let duplicate = format!(
        "[[package_hygiene.rules]]\nid = 'same'\nkind = 'string'\nbody = '{KEEP}'\n\
         [[package_hygiene.rules]]\nid = 'same'\nkind = 'string'\nbody = '{OTHER}'\n"
    );
    let error = Policy::from_manifest(&duplicate).expect_err("duplicate id");
    assert_eq!(error.code, "package_hygiene_config");
    assert_eq!(error.location, "duplicate_rule_id");
    assert_redacted(&error.to_string());

    let blank = format!(
        "[[package_hygiene.rules]]\nid = 'named'\nkind = 'string'\nbody = '{KEEP}'\n\
         [[package_hygiene.allow]]\nrule = 'named'\npath = '*'\nreason = ' '\n\
         definition = '{}'\n",
        definition_digest("string", KEEP)
    );
    let error = Policy::from_manifest(&blank).expect_err("empty reason");
    assert_eq!(error.code, "package_hygiene_config");
    assert_eq!(error.location, "allow_1");
    assert_redacted(&error.to_string());
    Ok(())
}

#[test]
fn migration_preserves_the_original_rule_and_path_pair() -> anyhow::Result<()> {
    let tree = pair_tree(format!("prefix {KEEP} middle {OTHER}").as_bytes())?;
    let loaded = Policy::from_manifest(&legacy_manifest())?;
    let migrated = loaded.migrated()?;
    assert!(migrated.forbidden_strings.is_empty());
    assert_eq!(
        migrated
            .rules
            .iter()
            .find(|rule| rule.id == "forbidden_string_1")
            .map(|rule| rule.body.as_str()),
        Some(KEEP)
    );
    assert_eq!(migrated.allow[0].rule, "forbidden_string_1");
    assert_eq!(migrated.allow[0].path, "reviewed/keep.txt");
    let report = audit_tree(tree.path(), &migrated)?;
    assert_eq!(report.files_scanned, 1);
    assert_eq!(
        report.bytes_scanned,
        format!("prefix {KEEP} middle {OTHER}").len() as u64
    );
    assert_eq!(report.allowed_findings, 1);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].rule, "forbidden_string_2");
    assert_eq!(report.findings[0].path, "reviewed/keep.txt");
    Ok(())
}

#[test]
fn an_edited_legacy_list_is_not_reinterpreted() -> anyhow::Result<()> {
    let text = format!(
        "[package_hygiene]\nforbidden_strings = ['{OTHER}', '{KEEP}']\n\
         [[package_hygiene.allow]]\nrule = 'forbidden_string_1'\n\
         definition = '{}'\npath = 'reviewed/keep.txt'\nreason = 'Reviewed keep pair'\n",
        definition_digest("string", KEEP)
    );
    let migrated = Policy::from_manifest(&text)?.migrated()?;
    assert_eq!(migrated.rules[0].body, OTHER);
    assert_eq!(
        migrated.allow[0].definition.as_deref(),
        Some(definition_digest("string", KEEP).as_str())
    );
    let tree = pair_tree(format!("{OTHER}\n{KEEP}").as_bytes())?;
    let report = audit_tree(tree.path(), &migrated)?;
    assert_eq!(report.allowed_findings, 0);
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.rule == "forbidden_string_1")
    );
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.rule == "forbidden_string_2")
    );
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.rule == OBSOLETE_ALLOWANCE)
    );
    assert_redacted(&json_of(&report)?);
    Ok(())
}

#[test]
fn a_policy_without_allowances_keeps_its_behavior() -> anyhow::Result<()> {
    let policy = Policy::from_manifest(&format!("[package_hygiene]\nforbidden_strings = ['{KEEP}']\n"))?;
    assert_eq!(policy.migrated()?, policy);
    let tree = pair_tree(format!("xx{KEEP}yy").as_bytes())?;
    let report = audit_tree(tree.path(), &policy)?;
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].rule, "forbidden_string_1");
    assert_eq!(report.findings[0].offset, 2);
    assert_eq!(audit_tree(tree.path(), &policy.migrated()?)?, report);
    assert_redacted(&json_of(&report)?);
    Ok(())
}

#[test]
fn builtin_ids_stay_stable_when_configurable_rules_move() -> anyhow::Result<()> {
    let token = format!("ghp_{}", "a".repeat(36));
    let tree = pair_tree(token.as_bytes())?;
    let mut policy = Policy {
        rules: vec![
            Rule {
                id: "zeta".into(),
                kind: "string".into(),
                body: "zeta-body".into(),
            },
            Rule {
                id: "alpha".into(),
                kind: "string".into(),
                body: "alpha-body".into(),
            },
        ],
        ..Policy::default()
    };
    policy.rules.reverse();
    let report = audit_tree(tree.path(), &policy)?;
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.rule == "secret_github_token")
    );
    let json = json_of(&report)?;
    assert!(!json.contains(&token));
    assert!(!json.contains("zeta-body"));
    assert!(!json.contains("alpha-body"));
    let email = Policy {
        scan_emails: true,
        allow: vec![Allowance {
            rule: "secret_email".into(),
            path: "reviewed/*.txt".into(),
            reason: "Reviewed contact".into(),
            definition: Some(definition_digest("builtin", "secret_email")),
        }],
        ..Policy::default()
    };
    let mail = pair_tree(b"person@example.invalid")?;
    let report = audit_tree(mail.path(), &email)?;
    assert_eq!(report.allowed_findings, 1);
    assert!(
        report
            .findings
            .iter()
            .all(|finding| finding.rule != "secret_email")
    );
    Ok(())
}

#[test]
fn the_cli_and_agent_report_stable_ids_and_the_policy_digest() -> anyhow::Result<()> {
    let project = Temp::new("hygiene-rule-identity-cli");
    let marker = "RULEIDTOKEN9f3a";
    let manifest = format!(
        "[[package_hygiene.rules]]\nid = 'marker_rule'\nkind = 'string'\nbody = '{marker}'\n\
         [[package_hygiene.allow]]\nrule = 'marker_rule'\n\
         definition = '{}'\npath = 'kept.txt'\nreason = 'Reviewed before the body changed'\n",
        definition_digest("string", "OTHER_BODY_9f3a")
    );
    write_file(project.path(), "Regia.toml", manifest.as_bytes())?;
    write_file(project.path(), "dist/player/kept.txt", marker.as_bytes())?;
    let output = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["package", "--platform", "linux", "--audit-dir"])
        .arg(project.path().join("dist/player"))
        .arg("--project")
        .arg(project.path())
        .arg("--json")
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let response: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(response["error"]["code"], "package_hygiene_failed");
    let hygiene = &response["hygiene"];
    assert!(hygiene["policy_digest"].as_str().unwrap_or("").starts_with("b3:"));
    let findings = hygiene["findings"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("findings"))?;
    assert!(
        findings
            .iter()
            .any(|finding| finding["rule"] == OBSOLETE_ALLOWANCE)
    );
    assert!(findings.iter().any(|finding| finding["rule"] == "marker_rule"));
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(!rendered.contains(marker));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(marker));

    let mut world = World::new();
    let mut config = AgentConfig::default();
    config.permissions.insert("build".into());
    world.insert_resource(AgentPermissions { config, source: None });
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.path().to_path_buf()),
    });
    world.insert_resource(RegiaCliProgram(PathBuf::from(env!("CARGO_BIN_EXE_regia"))));
    let LocalReply::Done(reply) = call_local(
        &mut world,
        "agent.package",
        json!({"audit_dir": "dist/player", "platform": "linux", "dry_run": true}),
        None,
    ) else {
        anyhow::bail!("preview is immediate");
    };
    let reply = reply.map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let args: Vec<String> = reply["command"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("command"))?
        .iter()
        .map(|value| value.as_str().unwrap_or("").to_owned())
        .collect();
    assert!(args.iter().any(|arg| arg.contains("--audit-dir=")));
    let output = Command::new(&args[0])
        .args(&args[1..])
        .current_dir(project.path())
        .output()?;
    let mut tally = CliTally::default();
    for line in String::from_utf8(output.stdout)?.lines() {
        tally.feed(line);
    }
    let result = tally.result(output.status.code(), false, &args);
    assert_eq!(result["report"]["error"]["code"], "package_hygiene_failed");
    assert!(
        result["report"]["hygiene"]["findings"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("job findings"))?
            .iter()
            .any(|finding| finding["rule"] == OBSOLETE_ALLOWANCE)
    );
    let digest = result["report"]["hygiene"]["policy_digest"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("digest"))?;
    assert!(digest.starts_with("b3:"));
    assert!(!result.to_string().contains(marker));
    Ok(())
}
