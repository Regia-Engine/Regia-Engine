// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask check-kinds`: the kind lint over every sample project at its own `[kinds] strictness`
//! (`docs/specs/101-entity-kinds-and-project-map.md` § 5, ADR-2488). A step of `cargo xtask preflight`.
//!
//! Samples without the setting are `off` and pass untouched; a sample at `deny` fails the step on an
//! error finding. Warnings and notes are printed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use regia_scene::kind::lint::{Strictness, has_errors, lint_project};
use regia_scene::kind::scan::{KindCatalog, scan_project};

/// The sample project folders (a `Regia.toml` directly inside), sorted.
fn sample_projects(root: &Path) -> Result<Vec<PathBuf>> {
    let samples = root.join("samples");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&samples)
        .with_context(|| format!("cannot read {}", samples.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("Regia.toml").is_file())
        .collect();
    out.sort();
    Ok(out)
}

/// A project's `[kinds] strictness`.
///
/// # Errors
///
/// The manifest does not parse or the value is not `off`, `warn` or `deny`.
pub fn strictness_of(project: &Path) -> Result<Strictness> {
    let path = project.join("Regia.toml");
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let table: toml::Table = text
        .parse()
        .with_context(|| format!("{} is not TOML", path.display()))?;
    match table
        .get("kinds")
        .and_then(|k| k.get("strictness"))
        .and_then(|s| s.as_str())
    {
        None => Ok(Strictness::Off),
        Some(s) => s
            .parse()
            .map_err(|e: String| anyhow::anyhow!("{}: [kinds] {e}", path.display())),
    }
}

/// Runs the lint over every sample; the problems that fail the step (error findings, bad settings).
///
/// # Errors
///
/// The samples folder cannot be read.
pub fn check(root: &Path) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for project in sample_projects(root)? {
        let name = project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let strictness = match strictness_of(&project) {
            Ok(s) => s,
            Err(e) => {
                problems.push(format!("{e:#}"));
                continue;
            }
        };
        if strictness == Strictness::Off {
            continue;
        }
        let files = scan_project(&project);
        let catalog = KindCatalog::new(None, &files);
        let findings = lint_project(&files, &catalog, strictness);
        for finding in &findings {
            println!("samples/{name}: {finding}");
        }
        if has_errors(&findings) {
            problems.push(format!(
                "samples/{name}: the kind lint found errors at strictness {}",
                strictness.name()
            ));
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sample_passes_at_its_own_strictness() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let problems = check(root).expect("samples readable");
        assert!(problems.is_empty(), "{problems:#?}");
    }
}
