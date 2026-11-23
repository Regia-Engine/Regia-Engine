// SPDX-License-Identifier: MPL-2.0
//! Provenance and review badges in the Content Browser (`docs/specs/12-assets.md` § 1a,
//! `docs/specs/23-agent-native.md` § 5, ADR-2428).
//!
//! Each list row (and each grid tile's tooltip) shows who made the asset (`hand`, `agent`, `gen`,
//! `import`), its review state, and how many `config/provenance.ron` rules it breaks. The badges
//! are read from the `.meta` files once per content-index generation, and only for the rows on
//! screen, so a large folder costs nothing extra per frame.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regia_asset::{MetaSidecar, ProvenanceTool, ReviewerState, meta_path_for};
use regia_asset_pipeline::provenance_policy::{ProvenancePolicy, ReviewKind, load_policy, tool_label};
use regia_asset_pipeline::validators::Severity;
use regia_ecs::prelude::*;

/// What a badge says about one asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceBadge {
    /// Who made it (`hand`, `agent`, `generator`, `import`, `unknown`).
    pub tool: &'static str,
    /// Its review state (`pending`, `owner`, `named`).
    pub review: ReviewKind,
    /// The named reviewer, when there is one.
    pub reviewer: Option<String>,
    /// Policy errors (`config/provenance.ron`); 0 without a policy.
    pub policy_errors: usize,
    /// Policy warnings.
    pub policy_warnings: usize,
    /// Whether the policy wants a review it does not see.
    pub needs_review: bool,
    /// The short text drawn after the row's name.
    pub text: String,
    /// The tooltip: origin, model, timestamp and each policy finding.
    pub hover: String,
}

fn short_tool(tool: &ProvenanceTool) -> &'static str {
    match tool {
        ProvenanceTool::Generator => "gen",
        other => tool_label(other),
    }
}

/// The badge of `rel` (project-relative source path) from its `.meta`, or `None` when the asset
/// has no `.meta`, or no provenance block and no policy finding (nothing to show).
#[must_use]
pub fn badge_for(
    project_root: &Path,
    rel: &str,
    policy: Option<&ProvenancePolicy>,
) -> Option<ProvenanceBadge> {
    let bytes = std::fs::read(meta_path_for(&project_root.join(rel))).ok()?;
    let Ok(meta) = MetaSidecar::from_ron(&bytes) else {
        return Some(ProvenanceBadge {
            tool: "unknown",
            review: ReviewKind::Pending,
            reviewer: None,
            policy_errors: usize::from(policy.is_some()),
            policy_warnings: 0,
            needs_review: false,
            text: "\u{26a0} .meta unreadable".to_string(),
            hover: "The .meta does not parse; its provenance cannot be read.".to_string(),
        });
    };
    let findings = policy.map(|p| p.check_meta(rel, &meta)).unwrap_or_default();
    let Some(p) = meta.provenance.as_ref() else {
        if findings.is_empty() {
            return None;
        }
        let mut badge = badge_parts(&regia_asset::Provenance::default(), policy, &findings);
        badge.text = format!("\u{26a0} policy {} \u{b7} no provenance", badge.policy_errors);
        return Some(badge);
    };
    Some(badge_parts(p, policy, &findings))
}

fn badge_parts(
    p: &regia_asset::Provenance,
    policy: Option<&ProvenancePolicy>,
    findings: &[regia_asset_pipeline::provenance_policy::PolicyFinding],
) -> ProvenanceBadge {
    let review = ReviewKind::of(&p.reviewer);
    let reviewer = match &p.reviewer {
        ReviewerState::Named(name) => Some(name.clone()),
        _ => None,
    };
    let errors = findings.iter().filter(|f| f.severity == Severity::Error).count();
    let warnings = findings.len() - errors;
    let needs_review = policy.is_some_and(|pol| pol.needs_review(Some(p)));
    let made = matches!(p.tool, ProvenanceTool::Agent | ProvenanceTool::Generator);
    let review_text = match (review, &reviewer) {
        (ReviewKind::Pending, _) if made || needs_review => "review pending".to_string(),
        (ReviewKind::Pending, _) => String::new(),
        (ReviewKind::Owner, _) => "\u{2713} owner".to_string(),
        (ReviewKind::Named, Some(name)) => format!("\u{2713} {name}"),
        (ReviewKind::Named, None) => "\u{2713} named".to_string(),
    };
    let mut parts: Vec<String> = Vec::new();
    if errors > 0 {
        parts.push(format!("\u{26a0} policy {errors}"));
    }
    parts.push(short_tool(&p.tool).to_string());
    if !review_text.is_empty() {
        parts.push(review_text);
    }
    let mut hover = vec![format!(
        "made by: {}; review: {}",
        tool_label(&p.tool),
        reviewer.clone().unwrap_or_else(|| review.label().to_string())
    )];
    for (label, value) in [
        ("origin", &p.origin),
        ("generator", &p.generator),
        ("client", &p.client),
        ("model", &p.model),
        ("created", &p.created_utc),
    ] {
        if let Some(v) = value {
            hover.push(format!("{label}: {v}"));
        }
    }
    for (key, value) in &p.extensions {
        hover.push(format!("{key}: {value}"));
    }
    for f in findings {
        hover.push(format!("{} {}: {}", f.severity.label(), f.rule, f.text));
    }
    ProvenanceBadge {
        tool: tool_label(&p.tool),
        review,
        reviewer,
        policy_errors: errors,
        policy_warnings: warnings,
        needs_review,
        text: parts.join(" \u{b7} "),
        hover: hover.join("\n"),
    }
}

/// The badges read so far, valid for one content-index generation (an edit-world resource).
#[derive(Resource, Debug, Default)]
pub struct ProvenanceBadges {
    root: Option<PathBuf>,
    generation: Option<u64>,
    policy: Option<ProvenancePolicy>,
    policy_error: Option<String>,
    badges: HashMap<String, Option<ProvenanceBadge>>,
    /// `.meta` files read since the editor started (a cost counter for tests and stats).
    pub reads: u64,
}

impl ProvenanceBadges {
    /// Start a frame: a new project or index generation drops the cached badges and re-reads
    /// `config/provenance.ron`.
    pub fn begin(&mut self, root: Option<&Path>, generation: u64) {
        if self.root.as_deref() == root && self.generation == Some(generation) {
            return;
        }
        self.root = root.map(Path::to_path_buf);
        self.generation = Some(generation);
        self.badges.clear();
        self.policy = None;
        self.policy_error = None;
        if let Some(root) = root {
            match load_policy(root) {
                Ok(policy) => self.policy = policy,
                Err(e) => self.policy_error = Some(e.to_string()),
            }
        }
    }

    /// The badge of `rel`, read on first use in this generation.
    pub fn get(&mut self, rel: &str) -> Option<&ProvenanceBadge> {
        let root = self.root.clone()?;
        if !self.badges.contains_key(rel) {
            self.reads += 1;
            let badge = badge_for(&root, rel, self.policy.as_ref());
            self.badges.insert(rel.to_owned(), badge);
        }
        self.badges.get(rel).and_then(Option::as_ref)
    }

    /// Why `config/provenance.ron` could not be used, if it could not.
    #[must_use]
    pub fn policy_error(&self) -> Option<&str> {
        self.policy_error.as_deref()
    }

    /// Whether the project has a policy.
    #[must_use]
    pub fn has_policy(&self) -> bool {
        self.policy.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::{AssetId, Provenance};

    fn put_meta(root: &Path, rel: &str, provenance: Option<Provenance>) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, b"x").expect("source");
        let mut meta = MetaSidecar::new("png", AssetId::new_random());
        meta.provenance = provenance;
        std::fs::write(meta_path_for(&path), meta.to_ron_string().expect("ron")).expect("meta");
    }

    #[test]
    fn badges_show_tool_review_and_policy_and_are_read_once_per_generation() {
        let root = regia_test_utils::unique_temp_dir("content-provenance");
        put_meta(
            &root,
            "assets/agent.png",
            Some(Provenance {
                tool: ProvenanceTool::Agent,
                client: Some("claude".into()),
                ..Provenance::default()
            }),
        );
        put_meta(
            &root,
            "assets/named.png",
            Some(Provenance {
                tool: ProvenanceTool::Generator,
                reviewer: ReviewerState::Named("ana".into()),
                ..Provenance::default()
            }),
        );
        put_meta(&root, "assets/plain.png", None);
        let mut badges = ProvenanceBadges::default();
        badges.begin(Some(&root), 1);
        let agent = badges.get("assets/agent.png").cloned().expect("badge");
        assert_eq!(agent.text, "agent \u{b7} review pending");
        assert!(agent.hover.contains("client: claude"), "{}", agent.hover);
        assert_eq!(
            badges.get("assets/named.png").map(|b| b.text.clone()),
            Some("gen \u{b7} \u{2713} ana".to_string())
        );
        assert!(
            badges.get("assets/plain.png").is_none(),
            "no provenance, no policy: no badge"
        );
        let _ = badges.get("assets/agent.png");
        assert_eq!(badges.reads, 3, "cached within a generation");
        // A policy that requires an origin: the plain asset now shows its violation.
        std::fs::create_dir_all(root.join("config")).expect("config");
        std::fs::write(
            root.join("config/provenance.ron"),
            "(kinds: { \"*\": (required: [\"origin\"]) }, review: (required_for: [agent]))",
        )
        .expect("policy");
        badges.begin(Some(&root), 1);
        assert!(
            badges.get("assets/plain.png").is_none(),
            "same generation: cached"
        );
        badges.begin(Some(&root), 2);
        assert!(badges.has_policy());
        let plain = badges.get("assets/plain.png").cloned().expect("violation badge");
        assert_eq!(plain.policy_errors, 1);
        assert!(plain.text.starts_with("\u{26a0} policy 1"), "{}", plain.text);
        let agent = badges.get("assets/agent.png").cloned().expect("badge");
        assert_eq!(agent.policy_errors, 2, "{}", agent.hover);
        assert!(agent.needs_review);
        assert!(agent.hover.contains("error unreviewed"), "{}", agent.hover);
        let _ = std::fs::remove_dir_all(&root);
    }
}
