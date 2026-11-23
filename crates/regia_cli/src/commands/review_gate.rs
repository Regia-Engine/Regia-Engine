// SPDX-License-Identifier: MPL-2.0
//! The shipping review gate (`docs/specs/23-agent-native.md` § 5, ADR-2346): with
//! `[provenance] require_review_for_shipping = true` in `Regia.toml`, `regia cook --quality final`
//! and `regia package` (even with fresh paks) refuse while any agent- or generator-made asset is unreviewed (its `.meta` provenance says
//! `tool: agent | generator` and `reviewer: pending`). Assets reviewed through staged review
//! (`review.accept`, the editor's Agent Changes panel) carry `reviewer: owner`.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Directories the scan never enters at any depth: VCS and tool state.
const SKIP_DIRS: &[&str] = &[".git", ".regia", ".ferrite"]; // rename-keep

/// Directories skipped only at the project root: build output (`assets/build/` is content).
const ROOT_SKIP_DIRS: &[&str] = &["target", "build", "node_modules"];

/// Whether the project's `Regia.toml` turns the gate on.
///
/// # Errors
/// A `Regia.toml` that exists but is not valid TOML, or a non-boolean setting.
pub fn required(project: &Path) -> Result<bool> {
    let path = project.join("Regia.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(anyhow::anyhow!("cannot read {}: {e}", path.display())),
    };
    let table: toml::Table = text
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid {}: {e}", path.display()))?;
    match table
        .get("provenance")
        .and_then(|p| p.get("require_review_for_shipping"))
    {
        None => Ok(false),
        Some(toml::Value::Boolean(on)) => Ok(*on),
        Some(other) => Err(anyhow::anyhow!(
            "[provenance] require_review_for_shipping must be true or false, not {other}"
        )),
    }
}

/// The source files (project-relative) whose `.meta` says agent- or generator-made and
/// `reviewer: pending`, sorted. Unparseable metas are reported too: an unreadable record is not
/// a reviewed one.
pub fn unreviewed(project: &Path) -> Vec<(String, &'static str)> {
    let mut found = Vec::new();
    let mut pending: Vec<PathBuf> = vec![project.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                let skip = SKIP_DIRS.contains(&name.as_ref())
                    || (dir == project && ROOT_SKIP_DIRS.contains(&name.as_ref()));
                if !skip {
                    pending.push(path);
                }
                continue;
            }
            if !kind.is_file() || !name.ends_with(".meta") {
                continue;
            }
            let rel = path
                .strip_prefix(project)
                .unwrap_or(&path)
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");
            let verdict = match std::fs::read(&path).map(|b| regia_asset::MetaSidecar::from_ron(&b)) {
                Ok(Ok(meta)) => meta.provenance.and_then(|p| {
                    let made = matches!(
                        p.tool,
                        regia_asset::ProvenanceTool::Agent | regia_asset::ProvenanceTool::Generator
                    );
                    (made && p.reviewer == regia_asset::ReviewerState::Pending).then_some("pending")
                }),
                _ => Some("unreadable_meta"),
            };
            if let Some(verdict) = verdict {
                found.push((rel, verdict));
            }
        }
    }
    found.sort();
    found
}

/// The gate `regia cook --quality final` and `regia package` run.
///
/// # Errors
/// When the gate is on and assets are unreviewed (the first 20 are named).
pub fn check(project: &Path) -> Result<()> {
    // `config/provenance.ron` (ADR-2428): absent, nothing changes.
    policy_check(project)?;
    if !required(project)? {
        return Ok(());
    }
    let found = unreviewed(project);
    if found.is_empty() {
        return Ok(());
    }
    let listed: Vec<String> = found
        .iter()
        .take(20)
        .map(|(path, why)| format!("  {path} ({why})"))
        .collect();
    let more = found.len().saturating_sub(20);
    anyhow::bail!(
        "{} asset(s) made by an agent or generator are not reviewed, and Regia.toml has \
         [provenance] require_review_for_shipping = true:\n{}{}\nReview them in the editor's \
         Agent Changes panel (or set reviewer: owner in their .meta after checking them).",
        found.len(),
        listed.join("\n"),
        if more > 0 {
            format!("\n  ... and {more} more")
        } else {
            String::new()
        }
    )
}

/// The provenance policy gate (`config/provenance.ron`, ADR-2428): `regia cook --quality final` and
/// `regia package` refuse every source whose `.meta` breaks the policy, with the code
/// `provenance_policy`. A project without the file passes.
///
/// # Errors
/// An invalid policy file, or sources that break it (the first 20 are named).
pub fn policy_check(project: &Path) -> Result<()> {
    let refusals = regia_asset_pipeline::provenance_policy::shipping_refusals(project)
        .map_err(|e| anyhow::anyhow!("{e} ({})", e.code()))?;
    if refusals.is_empty() {
        return Ok(());
    }
    let mut sources: Vec<&str> = refusals.iter().map(|f| f.source.as_str()).collect();
    sources.dedup();
    let listed: Vec<String> = refusals
        .iter()
        .take(20)
        .map(|f| format!("  {}: {} ({})", f.source, f.text, f.rule))
        .collect();
    let more = refusals.len().saturating_sub(20);
    anyhow::bail!(
        "provenance_policy: {} asset(s) break config/provenance.ron ({} finding(s)):\n{}{}\nFix the \
         provenance block in their .meta (or the policy), then cook again.",
        sources.len(),
        refusals.len(),
        listed.join("\n"),
        if more > 0 {
            format!("\n  ... and {more} more")
        } else {
            String::new()
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(dir: &Path, source: &str, provenance: Option<regia_asset::Provenance>) {
        let mut meta = regia_asset::MetaSidecar::new("material_ron", regia_asset::AssetId::new_random());
        meta.provenance = provenance;
        let path = regia_asset::meta_path_for(&dir.join(source));
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, meta.to_ron_string().expect("ron")).expect("meta");
    }

    fn made_by(
        tool: regia_asset::ProvenanceTool,
        reviewer: regia_asset::ReviewerState,
    ) -> regia_asset::Provenance {
        regia_asset::Provenance {
            tool,
            reviewer,
            ..regia_asset::Provenance::default()
        }
    }

    #[test]
    fn the_gate_names_unreviewed_agent_and_generator_assets_only() {
        use regia_asset::{ProvenanceTool as T, ReviewerState as R};
        let dir = regia_test_utils::unique_temp_dir("review-gate");
        meta(
            &dir,
            "assets/hand.material.ron",
            Some(made_by(T::Hand, R::Pending)),
        );
        meta(&dir, "assets/none.material.ron", None);
        meta(
            &dir,
            "assets/agent.material.ron",
            Some(made_by(T::Agent, R::Pending)),
        );
        meta(
            &dir,
            "assets/gen.material.ron",
            Some(made_by(T::Generator, R::Pending)),
        );
        meta(&dir, "assets/ok.material.ron", Some(made_by(T::Agent, R::Owner)));
        meta(
            &dir,
            "target/skipped.material.ron",
            Some(made_by(T::Agent, R::Pending)),
        );
        // `build` is skipped at the root only: under assets it is content.
        meta(
            &dir,
            "assets/build/nested.material.ron",
            Some(made_by(T::Agent, R::Pending)),
        );
        meta(
            &dir,
            "build/skipped.material.ron",
            Some(made_by(T::Agent, R::Pending)),
        );
        std::fs::write(dir.join("assets/broken.png.meta"), "not ron").expect("broken");
        assert!(!required(&dir).expect("no manifest"), "off without Regia.toml");
        assert!(check(&dir).is_ok());
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"x\"\n[provenance]\nrequire_review_for_shipping = true\n",
        )
        .expect("manifest");
        let found = unreviewed(&dir);
        assert_eq!(
            found,
            vec![
                ("assets/agent.material.ron".to_string(), "pending"),
                ("assets/broken.png".to_string(), "unreadable_meta"),
                ("assets/build/nested.material.ron".to_string(), "pending"),
                ("assets/gen.material.ron".to_string(), "pending"),
            ]
        );
        let error = check(&dir).expect_err("refused").to_string();
        assert!(error.contains("assets/agent.material.ron"), "{error}");
        std::fs::write(
            dir.join("Regia.toml"),
            "[provenance]\nrequire_review_for_shipping = \"yes\"\n",
        )
        .expect("bad");
        assert!(
            required(&dir).is_err(),
            "a non-boolean setting is an error, not off"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_policy_gate_refuses_breaking_assets_and_passes_without_a_policy() {
        use regia_asset::{ProvenanceTool as T, ReviewerState as R};
        let dir = regia_test_utils::unique_temp_dir("review-gate-policy");
        let source = |name: &str| {
            let path = dir.join("assets").join(name);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, b"x").expect("source");
        };
        source("hand.material.ron");
        meta(
            &dir,
            "assets/hand.material.ron",
            Some(made_by(T::Hand, R::Pending)),
        );
        source("agent.material.ron");
        meta(
            &dir,
            "assets/agent.material.ron",
            Some(made_by(T::Agent, R::Pending)),
        );
        // No policy file: the base behavior (no Regia.toml gate either), so the cook passes.
        assert!(policy_check(&dir).is_ok());
        assert!(check(&dir).is_ok());
        std::fs::create_dir_all(dir.join("config")).expect("config");
        std::fs::write(
            dir.join("config/provenance.ron"),
            "(review: (required_for: [agent]), kinds: { \"*\": (required: [\"tool\"]) })",
        )
        .expect("policy");
        let error = check(&dir).expect_err("refused").to_string();
        assert!(error.starts_with("provenance_policy:"), "{error}");
        assert!(error.contains("assets/agent.material.ron"), "{error}");
        assert!(error.contains("unreviewed"), "{error}");
        assert!(!error.contains("hand.material.ron"), "{error}");
        meta(
            &dir,
            "assets/agent.material.ron",
            Some(made_by(T::Agent, R::Owner)),
        );
        assert!(check(&dir).is_ok(), "reviewed assets pass");
        std::fs::write(dir.join("config/provenance.ron"), "(colour: 1)").expect("bad");
        let error = check(&dir).expect_err("bad policy").to_string();
        assert!(error.contains("invalid_config"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
