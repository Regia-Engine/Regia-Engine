// SPDX-License-Identifier: MPL-2.0
//! `ui.a11y_audit`: accessibility defects in the screen the UI shows now (ADR-3025, book
//! `agents/ui-a11y-audit.md`). The rules live in `regia_ui::a11y_audit`; this file parses the
//! parameters and prints the report. It reads: the live UI is left as it was.

use regia_ecs::prelude::*;
use regia_ui::FocusState;
use regia_ui::a11y_audit::{AuditOptions, AuditProfile, AuditReport, AuditRule, AuditSeverity, audit_ui};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

/// Most findings one reply lists; `findings_total` still counts them all.
const MAX_FINDINGS: usize = 4096;

/// The parameters of `ui.a11y_audit`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AuditParams {
    /// Only this node and its subtree (entity bits).
    root: Option<u64>,
    /// `desktop` or `touch`.
    profile: Option<String>,
    /// Rule codes to run.
    rules: Option<Vec<String>>,
    /// Replaces the profile's minimum target, in UI px.
    min_target: Option<f32>,
    /// The scale the clipping rule lays out at.
    text_scale: Option<f32>,
}

fn invalid(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn options_of(world: &World, params: &Value) -> Result<AuditOptions, RpcError> {
    let parsed: AuditParams = match params {
        Value::Null => AuditParams::default(),
        other => serde_json::from_value(other.clone())
            .map_err(|e| RpcError::invalid_params(format!("ui.a11y_audit: {e}")))?,
    };
    let mut options = AuditOptions::default();
    if let Some(bits) = parsed.root {
        let entity = Entity::try_from_bits(bits).ok_or_else(|| {
            invalid(
                "bad_entity",
                "`root` is not an entity id",
                "take a node id from ui.query or ui.a11y_tree, or omit it for every root",
            )
        })?;
        if world.get_entity(entity).is_err() {
            return Err(RpcError::new(
                NOT_FOUND,
                "unknown_entity",
                "`root` names no entity",
                "take a node id from ui.query or ui.a11y_tree",
            ));
        }
        options.root = Some(entity);
    }
    if let Some(name) = &parsed.profile {
        options.profile = AuditProfile::parse(name).ok_or_else(|| {
            invalid(
                "bad_profile",
                format!("unknown profile {name:?}"),
                "use desktop or touch",
            )
        })?;
    }
    for code in parsed.rules.iter().flatten() {
        let rule = AuditRule::parse(code).ok_or_else(|| {
            invalid(
                "unknown_rule",
                format!("unknown rule {code:?}"),
                "see book/agents/ui-a11y-audit.md for the rule codes",
            )
        })?;
        options.rules.push(rule);
    }
    options.min_target_px = parsed.min_target;
    if let Some(scale) = parsed.text_scale {
        options.text_scale = scale;
    }
    Ok(options)
}

fn report_json(report: &AuditReport, options: &AuditOptions) -> Value {
    let count = |severity: AuditSeverity| report.findings.iter().filter(|f| f.severity == severity).count();
    let findings: Vec<Value> = report
        .findings
        .iter()
        .take(MAX_FINDINGS)
        .map(|f| {
            json!({
                "rule": f.rule.code(),
                "severity": f.severity.name(),
                "node": f.node,
                "path": f.path,
                "measured": f.measured,
                "required": f.required,
                "criterion": f.criterion,
                "hint": f.hint,
                "needs_eyes": f.needs_eyes,
            })
        })
        .collect();
    let exemptions: Vec<Value> = report
        .exemptions
        .iter()
        .take(MAX_FINDINGS)
        .map(|e| json!({ "rule": e.rule.code(), "path": e.path, "exemption": e.exemption }))
        .collect();
    let skipped: Vec<Value> = report
        .skipped
        .iter()
        .map(|(rule, why)| json!({ "rule": rule.code(), "reason": why }))
        .collect();
    json!({
        "clean": report.is_clean(),
        "theme": report.theme,
        "profile": options.profile.name(),
        "min_target_px": report.min_target_px,
        "text_scale": report.text_scale,
        "reduced_motion": report.reduced_motion,
        "nodes": report.nodes,
        "truncated": report.truncated,
        "errors": count(AuditSeverity::Error),
        "warnings": count(AuditSeverity::Warning),
        "infos": count(AuditSeverity::Info),
        "findings_total": report.findings.len(),
        "findings": findings,
        "exemptions": exemptions,
        "skipped": skipped,
    })
}

/// `ui.a11y_audit { root?, profile?, rules?, min_target?, text_scale? }`.
pub(crate) fn a11y_audit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<FocusState>().is_none() {
        return Err(RpcError::new(
            UNSUPPORTED,
            "ui_not_installed",
            "the app has no runtime UI",
            "the app needs regia_ui::UiPlugin",
        ));
    }
    let options = options_of(world, params)?;
    let report = audit_ui(world, &options);
    Ok(report_json(&report, &options))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::a11y::Accessible;
    use regia_ui::focus::Focusable;
    use regia_ui::widgets::Button;
    use regia_ui::{
        Background, BackgroundKind, ComputedNode, Interactable, NodeName, UiColor, UiNode, UiRoot,
    };

    fn computed(rect: [f32; 4]) -> ComputedNode {
        ComputedNode {
            rect,
            clip_rect: [-100_000.0, -100_000.0, 300_000.0, 300_000.0],
            scale: 1.0,
            ..Default::default()
        }
    }

    /// A white screen with one 12 px, unnamed button.
    fn screen() -> (World, Entity, Entity) {
        let mut world = World::new();
        world.init_resource::<FocusState>();
        let root = world
            .spawn((
                UiRoot::default(),
                NodeName("screen".to_owned()),
                computed([0.0, 0.0, 800.0, 600.0]),
                Background {
                    kind: BackgroundKind::Color(UiColor::WHITE),
                },
            ))
            .id();
        let close = world
            .spawn((
                UiNode,
                NodeName("close".to_owned()),
                Button,
                Focusable::default(),
                Interactable::default(),
                computed([10.0, 10.0, 12.0, 12.0]),
                ChildOf(root),
            ))
            .id();
        world.spawn((
            UiNode,
            NodeName("next".to_owned()),
            Button,
            Focusable::default(),
            Interactable::default(),
            Accessible::default().with_label("Next"),
            computed([24.0, 10.0, 80.0, 40.0]),
            ChildOf(root),
        ));
        (world, root, close)
    }

    fn code(e: RpcError) -> String {
        e.data.expect("data")["code"].as_str().expect("code").to_owned()
    }

    #[test]
    fn the_reply_lists_findings_with_every_field_and_the_counts() {
        let (mut world, _, close) = screen();
        let out = a11y_audit(&mut world, &json!({ "rules": ["a11y_target_size"] })).expect("audit");
        assert_eq!(out["clean"], json!(false));
        assert_eq!(out["profile"], json!("desktop"));
        assert_eq!(out["min_target_px"], json!(24.0));
        assert_eq!(out["errors"], json!(1));
        assert_eq!(out["findings_total"], json!(1));
        let finding = &out["findings"][0];
        assert_eq!(finding["rule"], json!("a11y_target_size"));
        assert_eq!(finding["severity"], json!("error"));
        assert_eq!(finding["node"], json!(close.to_bits()));
        assert_eq!(finding["path"], json!("screen/close"));
        assert_eq!(finding["measured"], json!("12.0x12.0 px"));
        assert_eq!(finding["required"], json!("24.0x24.0 px"));
        assert_eq!(finding["criterion"], json!("2.5.8"));
        assert_eq!(finding["needs_eyes"], json!(false));
        assert!(!finding["hint"].as_str().expect("hint").is_empty());
    }

    #[test]
    fn profile_and_min_target_change_the_minimum() {
        let (mut world, _, _) = screen();
        let out = a11y_audit(
            &mut world,
            &json!({ "profile": "touch", "rules": ["a11y_target_size"] }),
        )
        .expect("audit");
        assert_eq!(out["profile"], json!("touch"));
        assert_eq!(out["min_target_px"], json!(44.0));
        assert_eq!(out["findings_total"], json!(2), "{out}");
        let out = a11y_audit(
            &mut world,
            &json!({ "min_target": 10.0, "rules": ["a11y_target_size"] }),
        )
        .expect("audit");
        assert_eq!(out["clean"], json!(true), "{out}");
    }

    #[test]
    fn root_narrows_the_report_to_a_subtree() {
        let (mut world, root, close) = screen();
        let all = a11y_audit(&mut world, &json!({ "rules": ["a11y_unnamed_interactive"] })).expect("audit");
        assert_eq!(all["findings_total"], json!(1), "{all}");
        let narrowed = a11y_audit(
            &mut world,
            &json!({ "root": close.to_bits(), "rules": ["a11y_target_size"] }),
        )
        .expect("audit");
        assert_eq!(narrowed["findings"][0]["path"], json!("screen/close"));
        let whole = a11y_audit(
            &mut world,
            &json!({ "root": root.to_bits(), "rules": ["a11y_target_size"] }),
        )
        .expect("audit");
        assert_eq!(whole["findings_total"], json!(1));
    }

    #[test]
    fn two_calls_answer_the_same_bytes() {
        let (mut world, _, _) = screen();
        let first = a11y_audit(&mut world, &json!({})).expect("audit");
        let second = a11y_audit(&mut world, &json!({})).expect("audit");
        assert_eq!(first.to_string(), second.to_string());
    }

    #[test]
    fn errors_carry_lowercase_codes() {
        let (mut world, _, _) = screen();
        let bad = |world: &mut World, params: Value| code(a11y_audit(world, &params).expect_err("refused"));
        assert_eq!(bad(&mut world, json!({ "profile": "phone" })), "bad_profile");
        assert_eq!(bad(&mut world, json!({ "rules": ["a11y_nope"] })), "unknown_rule");
        assert_eq!(bad(&mut world, json!({ "root": 0 })), "bad_entity");
        let gone = world.spawn_empty().id();
        world.despawn(gone);
        assert_eq!(
            bad(&mut world, json!({ "root": gone.to_bits() })),
            "unknown_entity"
        );
        // Typed parameters: a wrong type or an unknown field is F0002, not a silent default.
        assert_eq!(bad(&mut world, json!({ "profile": 3 })), "F0002");
        assert_eq!(bad(&mut world, json!({ "profiel": "touch" })), "F0002");
        let mut bare = World::new();
        assert_eq!(bad(&mut bare, json!({})), "ui_not_installed");
    }
}
