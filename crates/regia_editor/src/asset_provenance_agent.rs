// SPDX-License-Identifier: MPL-2.0
//! Provenance, the project policy and asset repair over the protocol (ADR-2428,
//! `book/agents/asset-provenance.md`): `editor.asset_provenance` reads an asset's provenance block,
//! its review state and the `config/provenance.ron` findings (or a project summary);
//! `editor.asset_repair_metas` re-pairs orphaned `.meta` files with their moved sources by content
//! hash; `editor.asset_subasset_changes` lists the sub-asset re-maps and missing sub-assets of recent
//! cooks. Editor methods: the policy and the asset database live in the desktop-only pipeline, which
//! no runtime crate may depend on.

use std::collections::BTreeMap;

use std::path::PathBuf;

use regia_agent::protocol::{self, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset::{MetaSidecar, meta_path_for};
use regia_asset_pipeline::AssetDatabase;
use regia_asset_pipeline::provenance_policy::{
    PolicyFinding, ProvenancePolicy, ReviewKind, check_project, load_policy, tool_label,
};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::agent_ext::EditorProject;
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/asset-provenance.md";

/// Most findings or changes one answer lists (`limit` caps lower).
pub const MAX_LISTED: u64 = 1000;

fn error(code: i64, regia_code: &str, reason: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({ "code": regia_code, "reason": reason, "hint": HINT })),
    }
}

fn limit(params: &Value, default: u64) -> Result<usize, RpcError> {
    match params.get("limit") {
        None | Some(Value::Null) => Ok(default as usize),
        Some(v) => match v.as_u64() {
            Some(n) if (1..=MAX_LISTED).contains(&n) => Ok(n as usize),
            _ => Err(error(
                protocol::INVALID_PARAMS,
                "F0002",
                "bad_limit",
                format!("`limit` must be a whole number from 1 to {MAX_LISTED}"),
            )),
        },
    }
}

/// The open project's root, or `no_project`.
fn project_root(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| {
            error(
                protocol::NOT_FOUND,
                "F0003",
                "no_project",
                "no project is open in the editor",
            )
        })
}

fn policy(world: &World) -> Result<(PathBuf, Option<ProvenancePolicy>), RpcError> {
    let root = project_root(world)?;
    let policy = load_policy(&root).map_err(|e| {
        error(
            protocol::INVALID_PARAMS,
            "F0002",
            "provenance_policy_invalid",
            e.to_string(),
        )
    })?;
    Ok((root, policy))
}

fn finding_json(f: &PolicyFinding) -> Value {
    json!({ "source": f.source, "rule": f.rule, "severity": f.severity.label(), "text": f.text })
}

/// `editor.asset_provenance { asset?, limit? }`.
fn provenance(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (root, policy) = policy(world)?;
    let Some(asset) = params
        .get("asset")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Ok(summary(&root, policy.as_ref(), limit(params, 200)?));
    };
    let rel = asset.replace('\\', "/");
    let rel = if rel.starts_with("assets/") {
        rel
    } else {
        format!("assets/{rel}")
    };
    let source = regia_core::project_path::resolve_write(&root, &rel).map_err(|e| {
        error(
            protocol::INVALID_PARAMS,
            "F0002",
            "path_outside_project",
            format!("`asset` must be a project-relative path inside the project: {e}"),
        )
    })?;
    if !source.is_file() {
        return Err(error(
            protocol::NOT_FOUND,
            "F0003",
            "asset_not_found",
            format!("no source file `{rel}`"),
        ));
    }
    let meta_path = meta_path_for(&source);
    let meta = match std::fs::read(&meta_path) {
        Ok(bytes) => Some(MetaSidecar::from_ron(&bytes).map_err(|e| {
            error(
                protocol::INVALID_PARAMS,
                "F0002",
                "meta_unreadable",
                format!("the .meta of `{rel}` does not parse: {e}"),
            )
        })?),
        Err(_) => None,
    };
    let Some(meta) = meta else {
        return Ok(json!({
            "asset": rel, "meta": false, "id": null, "importer": null, "provenance": null,
            "tool": "unknown", "review": "pending", "reviewer": null, "policy": policy.is_some(),
            "needs_review": false, "findings": [],
        }));
    };
    let p = meta.provenance.clone().unwrap_or_default();
    let findings = policy
        .as_ref()
        .map(|pol| pol.check_meta(&rel, &meta))
        .unwrap_or_default();
    let reviewer = match &p.reviewer {
        regia_asset::ReviewerState::Named(name) => Some(name.clone()),
        _ => None,
    };
    Ok(json!({
        "asset": rel,
        "meta": true,
        "id": meta.id.to_string(),
        "importer": meta.importer,
        "provenance": meta.provenance.as_ref().map(|b| serde_json::to_value(b).unwrap_or(Value::Null)),
        "tool": tool_label(&p.tool),
        "review": ReviewKind::of(&p.reviewer).label(),
        "reviewer": reviewer,
        "policy": policy.is_some(),
        "needs_review": policy.as_ref().is_some_and(|pol| pol.needs_review(meta.provenance.as_ref())),
        "findings": findings.iter().map(finding_json).collect::<Vec<_>>(),
    }))
}

fn summary(root: &std::path::Path, policy: Option<&ProvenancePolicy>, limit: usize) -> Value {
    let mut by_tool: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut by_review: BTreeMap<&'static str, u64> = BTreeMap::new();
    let metas = regia_asset_pipeline::provenance_policy::project_metas(root);
    let mut unreadable = 0u64;
    for (_, path) in &metas {
        match std::fs::read(path).map(|b| MetaSidecar::from_ron(&b)) {
            Ok(Ok(meta)) => {
                let p = meta.provenance.unwrap_or_default();
                *by_tool.entry(tool_label(&p.tool)).or_default() += 1;
                *by_review.entry(ReviewKind::of(&p.reviewer).label()).or_default() += 1;
            }
            _ => unreadable += 1,
        }
    }
    let findings = policy.map(|p| check_project(root, p)).unwrap_or_default();
    let errors = findings
        .iter()
        .filter(|f| f.severity == regia_asset_pipeline::validators::Severity::Error)
        .count();
    json!({
        "policy": policy.is_some(),
        "assets": metas.len(),
        "unreadable": unreadable,
        "by_tool": by_tool,
        "by_review": by_review,
        "errors": errors,
        "warnings": findings.len() - errors,
        "findings": findings.iter().take(limit).map(finding_json).collect::<Vec<_>>(),
        "truncated": findings.len() > limit,
    })
}

/// `editor.asset_repair_metas { dry_run? }`.
fn repair_metas(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dry_run = match params.get("dry_run") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(error(
                protocol::INVALID_PARAMS,
                "F0002",
                "bad_dry_run",
                "`dry_run` must be true or false",
            ));
        }
    };
    let root = project_root(world)?;
    let mut db = AssetDatabase::open(&root).map_err(|e| {
        error(
            protocol::INTERNAL_ERROR,
            "F0012",
            "asset_db_unreadable",
            e.to_string(),
        )
    })?;
    let report = db.repair_orphaned_metas(&root, !dry_run);
    if !dry_run && !report.repaired.is_empty() {
        db.save(&root).map_err(|e| {
            error(
                protocol::INTERNAL_ERROR,
                "F0012",
                "asset_db_unwritable",
                e.to_string(),
            )
        })?;
    }
    let mut value = serde_json::to_value(&report)
        .map_err(|e| error(protocol::INTERNAL_ERROR, "F0012", "internal", e.to_string()))?;
    if let Some(o) = value.as_object_mut() {
        o.insert("warnings".into(), json!(report.warning_lines()));
    }
    Ok(value)
}

/// `editor.asset_subasset_changes { source?, code?, limit? }`.
fn subasset_changes(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world)?;
    let limit = limit(params, 200)?;
    let source = params.get("source").and_then(Value::as_str).map(|s| {
        let s = s.replace('\\', "/");
        if s.starts_with("assets/") {
            s
        } else {
            format!("assets/{s}")
        }
    });
    let code = params.get("code").and_then(Value::as_str);
    if let Some(code) = code
        && code != regia_asset_pipeline::subasset_remap::SUBASSET_MISSING_CODE
        && code != regia_asset_pipeline::subasset_remap::SUBASSET_REMAPPED_CODE
    {
        return Err(error(
            protocol::INVALID_PARAMS,
            "F0002",
            "bad_code",
            "`code` must be asset_subasset_missing or asset_subasset_remapped",
        ));
    }
    let all = regia_asset_pipeline::subasset_remap::read_changes_log(&root);
    let kept: Vec<Value> = all
        .into_iter()
        .filter(|e| source.as_deref().is_none_or(|s| e["source"] == s))
        .filter(|e| code.is_none_or(|c| e["code"] == c))
        .collect();
    let skip = kept.len().saturating_sub(limit);
    let count = kept.len();
    Ok(json!({ "count": count, "changes": kept.into_iter().skip(skip).collect::<Vec<_>>() }))
}

const fn opt(name: &'static str, kind: &'static str, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required: false,
        description,
    }
}

const PROVENANCE_PARAMS: &[ParamSchema] = &[
    opt(
        "asset",
        "string",
        "source path, `assets/...` (default: the project summary)",
    ),
    opt(
        "limit",
        "number",
        "most findings listed in the summary (1 to 1000, default 200)",
    ),
];

const REPAIR_PARAMS: &[ParamSchema] = &[opt(
    "dry_run",
    "boolean",
    "report what would move without writing (default false)",
)];

const CHANGES_PARAMS: &[ParamSchema] = &[
    opt("source", "string", "only this source"),
    opt(
        "code",
        "string",
        "asset_subasset_missing or asset_subasset_remapped",
    ),
    opt(
        "limit",
        "number",
        "newest changes listed (1 to 1000, default 200)",
    ),
];

/// `editor.asset_provenance`, `editor.asset_repair_metas` and `editor.asset_subasset_changes`.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.asset_provenance",
                tool: "asset_provenance",
                description: "An asset's provenance block, review state and config/provenance.ron findings; without `asset`, a project summary.",
                params: PROVENANCE_PARAMS,
                result: "{ asset, meta, id, importer, provenance, tool, review, reviewer, policy, needs_review, findings: [{ source, rule, severity, text }] } | { policy, assets, unreadable, by_tool, by_review, errors, warnings, findings, truncated }",
                mutating: false,
                permission: Permission::Read,
            },
            provenance as Handler,
        ),
        (
            MethodSchema {
                name: "editor.asset_repair_metas",
                tool: "asset_repair_metas",
                description: "Re-pair orphaned .meta files with their moved sources by content hash (file name without a database row); the GUIDs are kept.",
                params: REPAIR_PARAMS,
                result: "{ applied, repaired: [{ from_meta, to_source, id, rule }], orphans: [{ meta, id, reason, candidates }], warnings }",
                mutating: true,
                permission: Permission::Assets,
            },
            repair_metas as Handler,
        ),
        (
            MethodSchema {
                name: "editor.asset_subasset_changes",
                tool: "asset_subasset_changes",
                description: "Sub-asset re-maps (renamed, id kept) and missing sub-assets (with referrers) of recent cooks, newest last.",
                params: CHANGES_PARAMS,
                result: "{ count, changes: [{ code, source, path, old_path?, id, rule?, referrers? }] }",
                mutating: false,
                permission: Permission::Read,
            },
            subasset_changes as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::{AssetId, Provenance, ProvenanceTool, ReviewerState};

    fn world_at(root: &std::path::Path) -> World {
        let mut world = World::new();
        world.insert_resource(EditorProject {
            root: Some(root.to_path_buf()),
        });
        world
    }

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let (_, handler) = methods()
            .into_iter()
            .find(|(schema, _)| schema.name == method)
            .unwrap_or_else(|| panic!("{method} registered"));
        handler(world, &params)
    }

    fn put(root: &std::path::Path, rel: &str, provenance: Option<Provenance>) -> AssetId {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, rel.as_bytes()).expect("source");
        let id = AssetId::new_random();
        let mut meta = MetaSidecar::new("png", id);
        meta.provenance = provenance;
        std::fs::write(meta_path_for(&path), meta.to_ron_string().expect("ron")).expect("meta");
        id
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn provenance_reports_one_asset_and_a_project_summary() {
        let root = regia_test_utils::unique_temp_dir("agent-provenance");
        let id = put(
            &root,
            "assets/agent.png",
            Some(Provenance {
                tool: ProvenanceTool::Agent,
                model: Some("m".into()),
                ..Provenance::default()
            }),
        );
        put(
            &root,
            "assets/ok.png",
            Some(Provenance {
                tool: ProvenanceTool::Hand,
                reviewer: ReviewerState::Owner,
                ..Provenance::default()
            }),
        );
        let mut world = world_at(&root);
        let one = call(
            &mut world,
            "editor.asset_provenance",
            json!({ "asset": "agent.png" }),
        )
        .expect("one");
        assert_eq!(one["asset"], "assets/agent.png");
        assert_eq!(one["id"], id.to_string());
        assert_eq!(one["tool"], "agent");
        assert_eq!(one["review"], "pending");
        assert_eq!(one["policy"], false);
        assert_eq!(one["findings"], json!([]));
        let all = call(&mut world, "editor.asset_provenance", json!({})).expect("summary");
        assert_eq!(all["assets"], 2);
        assert_eq!(all["by_review"]["owner"], 1);
        assert_eq!(all["policy"], false);
        std::fs::create_dir_all(root.join("config")).expect("config");
        std::fs::write(
            root.join("config/provenance.ron"),
            "(review: (required_for: [agent]))",
        )
        .expect("policy");
        let one = call(
            &mut world,
            "editor.asset_provenance",
            json!({ "asset": "assets/agent.png" }),
        )
        .expect("one");
        assert_eq!(one["needs_review"], true);
        assert_eq!(one["findings"][0]["rule"], "unreviewed");
        let all = call(&mut world, "editor.asset_provenance", json!({ "limit": 1 })).expect("summary");
        assert_eq!(all["errors"], 1);
        let missing = call(
            &mut world,
            "editor.asset_provenance",
            json!({ "asset": "nope.png" }),
        )
        .expect_err("missing");
        assert_eq!(reason(&missing), "asset_not_found");
        let bad = call(&mut world, "editor.asset_provenance", json!({ "limit": 0 })).expect_err("limit");
        assert_eq!(reason(&bad), "bad_limit");
        std::fs::write(root.join("config/provenance.ron"), "(oops: 1)").expect("bad policy");
        let bad = call(&mut world, "editor.asset_provenance", json!({})).expect_err("policy");
        assert_eq!(reason(&bad), "provenance_policy_invalid");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repair_metas_refuses_without_a_project() {
        let mut world = World::new();
        let err = call(&mut world, "editor.asset_repair_metas", json!({})).expect_err("no project");
        assert_eq!(reason(&err), "no_project");
    }

    #[test]
    fn repair_metas_dry_runs_then_re_pairs_and_changes_read_the_log() {
        let root = regia_test_utils::unique_temp_dir("agent-repair");
        std::fs::create_dir_all(root.join("assets/new")).expect("dir");
        // A meta whose source moved; no database row, so the unique file name decides.
        let id = AssetId::new_random();
        let meta = MetaSidecar::new("png", id);
        std::fs::write(
            root.join("assets/rock.png.meta"),
            meta.to_ron_string().expect("ron"),
        )
        .expect("meta");
        std::fs::write(root.join("assets/new/rock.png"), b"px").expect("source");
        let mut world = world_at(&root);
        let dry = call(
            &mut world,
            "editor.asset_repair_metas",
            json!({ "dry_run": true }),
        )
        .expect("dry");
        assert_eq!(dry["applied"], false);
        assert_eq!(dry["repaired"][0]["to_source"], "assets/new/rock.png");
        assert!(root.join("assets/rock.png.meta").is_file());
        let done = call(&mut world, "editor.asset_repair_metas", json!({})).expect("apply");
        assert_eq!(done["applied"], true);
        assert_eq!(done["repaired"][0]["rule"], "file_name");
        assert!(root.join("assets/new/rock.png.meta").is_file());
        let bad = call(
            &mut world,
            "editor.asset_repair_metas",
            json!({ "dry_run": "yes" }),
        )
        .expect_err("bad");
        assert_eq!(reason(&bad), "bad_dry_run");
        // The sub-asset log: empty, then the events a cook wrote.
        let none = call(&mut world, "editor.asset_subasset_changes", json!({})).expect("empty");
        assert_eq!(none["count"], 0);
        let event = regia_asset_pipeline::subasset_remap::SubAssetChange {
            code: regia_asset_pipeline::subasset_remap::SUBASSET_MISSING_CODE,
            source: "assets/hero.glb".into(),
            path: "material/Metal".into(),
            old_path: None,
            id,
            rule: None,
            referrers: vec!["assets/levels/a.level.ron".into()],
        };
        regia_asset_pipeline::subasset_remap::log_changes(&root, &[event]).expect("log");
        let got = call(
            &mut world,
            "editor.asset_subasset_changes",
            json!({ "source": "hero.glb", "code": "asset_subasset_missing" }),
        )
        .expect("changes");
        assert_eq!(got["count"], 1);
        assert_eq!(got["changes"][0]["referrers"][0], "assets/levels/a.level.ron");
        let bad = call(
            &mut world,
            "editor.asset_subasset_changes",
            json!({ "code": "x" }),
        )
        .expect_err("code");
        assert_eq!(reason(&bad), "bad_code");
        let _ = std::fs::remove_dir_all(&root);
    }
}
