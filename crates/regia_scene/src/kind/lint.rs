// SPDX-License-Identifier: MPL-2.0
//! The kind lint: level entities without a kind, missing required components, unknown tags,
//! conflicting markers and two roles on one entity (spec 101 § 5).
//!
//! Pure: reads [`ProjectFiles`] and a [`KindCatalog`], never the disk. `Strictness::Off` reports
//! nothing; `Warn` makes every finding a warning, `Deny` an error, and notes stay notes.

use std::collections::HashMap;

use regia_core::id::Guid;
use serde::{Deserialize, Serialize};

use super::roles::{ComponentProbe, classify, has_game_component, ron_says_sensor};
use super::scan::{KindCatalog, ProjectFiles, ScannedScene, is_kind_tag, prefab_root, record_kind, tag_name};
use crate::format::SceneEntity;

/// How strictly a project holds level entities to kinds (`[kinds] strictness` in `Regia.toml`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strictness {
    /// Nothing is checked (the default).
    #[default]
    Off,
    /// Findings are warnings.
    Warn,
    /// Findings are errors (preflight and the cook fail).
    Deny,
}

impl Strictness {
    /// The setting's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Warn => "warn",
            Self::Deny => "deny",
        }
    }
}

impl std::str::FromStr for Strictness {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "warn" => Ok(Self::Warn),
            "deny" => Ok(Self::Deny),
            other => Err(format!("strictness `{other}` is not off, warn or deny")),
        }
    }
}

/// How serious a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A suggestion; never fails a run.
    Note,
    /// Printed; does not fail a run.
    Warning,
    /// Fails `regia preflight` and the cook.
    Error,
}

/// One lint finding.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Finding {
    /// The file (project-relative).
    pub file: String,
    /// The entity's guid in the file.
    pub entity: String,
    /// The finding code (`kind_missing`, ...).
    pub code: String,
    /// How serious it is.
    pub severity: Severity,
    /// The entity's name, if it has one.
    pub name: Option<String>,
    /// What is wrong and what to do.
    pub message: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let severity = match self.severity {
            Severity::Note => "note",
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        let name = self.name.as_deref().unwrap_or("(unnamed)");
        write!(
            f,
            "{severity}[{}]: {} {name} ({}): {}",
            self.code, self.file, self.entity, self.message
        )
    }
}

/// Whether any finding is an error.
#[must_use]
pub fn has_errors(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Error)
}

/// The text of the two-roles suggestion.
pub const TWO_ROLES_SUGGESTION: &str = "two roles on one entity, consider a child";

/// What the lint knows about one record.
struct RecordFacts {
    kind: Option<String>,
    conflicts: Vec<String>,
    overridden: bool,
    /// Components the kind's required list is checked against (the record's own plus, for an
    /// instance, the template root's and the instance's additions to it).
    available: Vec<String>,
    multi_family: Option<Vec<&'static str>>,
    game_component: bool,
}

/// The kind and components of one level or prefab record.
fn facts(catalog: &KindCatalog, files: &ProjectFiles, record: &SceneEntity) -> RecordFacts {
    let mut kind = record_kind(catalog, record);
    let mut overridden = false;
    let mut available: Vec<String> = record.components.keys().cloned().collect();
    if let Some(instance) = &record.prefab_instance
        && let Some(prefab) = files.prefab(instance.prefab.guid())
        && let Some(root) = prefab_root(&prefab.file)
    {
        available.extend(root.components.keys().cloned());
        available.extend(
            instance
                .added_components
                .iter()
                .filter(|a| a.target == root.guid)
                .map(|a| a.component.clone()),
        );
        if kind.is_none() {
            kind = record_kind(catalog, root);
        }
        // An instance override of the root's tag changes the instance's kind.
        if let Some(name) = instance
            .overrides
            .iter()
            .filter(|o| o.target == root.guid && is_kind_tag(&o.component))
            .find_map(|o| tag_name(o.value.get_ron()))
        {
            kind = Some((name, Vec::new()));
            overridden = true;
        }
    }
    available.sort();
    available.dedup();
    let classification = classify(record.components.iter().map(|(path, raw)| ComponentProbe {
        type_path: path,
        sensor: ron_says_sensor(raw.get_ron()),
    }));
    let multi_family = classification
        .is_multi_family()
        .then(|| classification.families.iter().map(|f| f.name()).collect());
    let game_component = has_game_component(
        record
            .components
            .keys()
            .map(String::as_str)
            .filter(|p| !is_kind_tag(p) && catalog.by_marker(p).is_none()),
    );
    let (kind, conflicts) = match kind {
        Some((name, conflicts)) => (Some(name), conflicts),
        None => (None, Vec::new()),
    };
    RecordFacts {
        kind,
        conflicts,
        overridden,
        available,
        multi_family,
        game_component,
    }
}

/// Lints every level (and every prefab root's required components) at `strictness`. Sorted by
/// file, entity, code.
#[must_use]
pub fn lint_project(files: &ProjectFiles, catalog: &KindCatalog, strictness: Strictness) -> Vec<Finding> {
    let _span = tracing::info_span!("kind_lint_project").entered();
    if strictness == Strictness::Off {
        return Vec::new();
    }
    let level_severity = match strictness {
        Strictness::Deny => Severity::Error,
        _ => Severity::Warning,
    };
    let mut out = Vec::new();
    for level in &files.levels {
        lint_scene(files, catalog, level, level_severity, true, &mut out);
    }
    for prefab in &files.prefabs {
        lint_scene(files, catalog, prefab, level_severity, false, &mut out);
    }
    for (path, error) in &files.errors {
        out.push(Finding {
            file: path.clone(),
            entity: String::new(),
            code: "kind_parse".into(),
            severity: level_severity,
            name: None,
            message: format!("could not read: {error}"),
        });
    }
    out.sort();
    out
}

fn lint_scene(
    files: &ProjectFiles,
    catalog: &KindCatalog,
    scene: &ScannedScene,
    severity: Severity,
    is_level: bool,
    out: &mut Vec<Finding>,
) {
    let records = &scene.file.entities;
    let by_guid: HashMap<Guid, &SceneEntity> = records.iter().map(|r| (r.guid, r)).collect();
    let facts_of: HashMap<Guid, RecordFacts> = records
        .iter()
        .map(|r| (r.guid, facts(catalog, files, r)))
        .collect();
    // A record with a kinded ancestor is a part of a kinded object.
    let kinded_ancestor = |record: &SceneEntity| {
        let mut parent = record.parent;
        let mut steps = 0;
        while let Some(guid) = parent {
            steps += 1;
            if steps > records.len() {
                return false;
            }
            if facts_of.get(&guid).is_some_and(|f| f.kind.is_some()) {
                return true;
            }
            parent = by_guid.get(&guid).and_then(|r| r.parent);
        }
        false
    };
    for record in records {
        let Some(facts) = facts_of.get(&record.guid) else {
            continue;
        };
        let mut push = |code: &str, severity: Severity, message: String| {
            out.push(Finding {
                file: scene.path.clone(),
                entity: record.guid.to_string(),
                code: code.to_string(),
                severity,
                name: record.name.clone(),
                message,
            });
        };
        if let Some(families) = &facts.multi_family {
            push(
                "kind_two_roles",
                Severity::Note,
                format!("{TWO_ROLES_SUGGESTION} ({})", families.join(" + ")),
            );
        }
        match &facts.kind {
            Some(name) => {
                if facts.overridden {
                    push(
                        "kind_overridden",
                        Severity::Note,
                        format!("the instance overrides its prefab's kind with `{name}`"),
                    );
                }
                if !facts.conflicts.is_empty() {
                    push(
                        "kind_conflict",
                        severity,
                        format!(
                            "carries the markers of `{name}` and {}; an entity has one kind (`{name}` is used)",
                            facts.conflicts.join(", ")
                        ),
                    );
                }
                match catalog.get(name) {
                    None => push(
                        "kind_unknown",
                        severity,
                        format!("names kind `{name}`, which no .kind.ron or code kind declares"),
                    ),
                    Some(info) => {
                        let missing: Vec<&str> = info
                            .required
                            .iter()
                            .filter(|r| !facts.available.iter().any(|a| a == *r))
                            .map(String::as_str)
                            .collect();
                        if !missing.is_empty() {
                            push(
                                "kind_required_missing",
                                severity,
                                format!("kind `{name}` requires {}", missing.join(", ")),
                            );
                        }
                    }
                }
            }
            None if is_level
                && !kinded_ancestor(record)
                && (facts.game_component || facts.multi_family.is_some()) =>
            {
                let why = if facts.game_component {
                    "carries game components"
                } else {
                    "has two roles"
                };
                push(
                    "kind_missing",
                    severity,
                    format!(
                        "{why} but has no kind; add a kind marker or a KindTag (or make it a kinded prefab)"
                    ),
                );
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests;
