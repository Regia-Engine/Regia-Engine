// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask check-ecology`: every `.biome.ron` in the repository validated for plausible ecology
//! (ADR-2456, book `agents/biomes.md` "Ecology").
//!
//! The job walks `crates/`, `samples/`, `templates/` and `assets/` (skipping `target` and hidden
//! directories), reads each `.biome.ron` with the `.ecology.ron` beside it when there is one, and runs
//! `regia_procgen::biome::ecology::validate_ecology`. Every issue is printed with its reason; any
//! error-level issue, or a biome or profile that does not parse, fails the job. Never edits files.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use regia_procgen::biome::BiomeDef;
use regia_procgen::biome::ecology::{
    EcologyProfile, EcologyReport, Severity, SpeciesCatalog, ecology_path_for, validate_ecology,
};

/// The directories searched, relative to the repository root.
pub const ROOTS: [&str; 4] = ["crates", "samples", "templates", "assets"];

/// Every `.biome.ron` under the search roots, sorted.
pub fn biome_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for r in ROOTS {
        walk(&root.join(r), &mut out);
    }
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&path, out),
            Ok(t) if t.is_file() && name.ends_with(".biome.ron") => out.push(path),
            _ => {}
        }
    }
}

/// One biome's result: its report, or why it could not be read.
pub struct Checked {
    /// The `.biome.ron`.
    pub path: PathBuf,
    /// The profile used, if one sits beside the biome.
    pub ecology: Option<PathBuf>,
    /// The report, or the read or parse error.
    pub report: Result<EcologyReport, String>,
}

/// Validates the biome text `biome` with the optional profile text `ecology`.
///
/// # Errors
///
/// A message when either text does not parse.
pub fn check_texts(biome: &str, ecology: Option<&str>) -> Result<EcologyReport, String> {
    let def = BiomeDef::from_ron(biome).map_err(|e| e.to_string())?;
    let profile = ecology
        .map(EcologyProfile::from_ron)
        .transpose()
        .map_err(|e| e.to_string())?;
    let catalog = profile
        .as_ref()
        .map_or_else(|| SpeciesCatalog::builtin().clone(), EcologyProfile::catalog);
    Ok(validate_ecology(&def, profile.as_ref(), &catalog))
}

/// Validates one `.biome.ron` file and the profile beside it.
pub fn check_file(path: &Path) -> Checked {
    let eco = PathBuf::from(ecology_path_for(&path.to_string_lossy()));
    let eco_text = std::fs::read_to_string(&eco).ok();
    let report = std::fs::read_to_string(path)
        .map_err(|e| format!("read: {e}"))
        .and_then(|t| check_texts(&t, eco_text.as_deref()));
    Checked {
        path: path.to_path_buf(),
        ecology: eco_text.map(|_| eco),
        report,
    }
}

/// Runs the check over the repository at `root`.
///
/// # Errors
///
/// When the search roots cannot be listed.
pub fn run(root: &Path) -> Result<ExitCode> {
    let files = biome_files(root);
    if files.is_empty() {
        anyhow::bail!("no .biome.ron files under {}", root.display());
    }
    let mut failed = 0usize;
    for f in &files {
        let c = check_file(f);
        let rel = c.path.strip_prefix(root).unwrap_or(&c.path).display().to_string();
        match &c.report {
            Err(e) => {
                failed += 1;
                println!("FAIL {rel}: {e}");
            }
            Ok(r) => {
                let profile = if c.ecology.is_some() {
                    "profile"
                } else if r.site_inferred {
                    "site inferred"
                } else {
                    "no site"
                };
                let mark = if r.plausible() { "ok  " } else { "FAIL" };
                if !r.plausible() {
                    failed += 1;
                }
                println!(
                    "{mark} {rel} ({profile}, {} species, {} unmapped)",
                    r.species.len(),
                    r.unmapped.len()
                );
                for i in &r.issues {
                    let sev = match i.severity {
                        Severity::Error => "error",
                        Severity::Warning => "warning",
                    };
                    println!("     {sev} {} at {}: {}", i.code, i.path, i.message);
                }
            }
        }
    }
    println!(
        "check-ecology: {} biome file(s), {} implausible or unreadable",
        files.len(),
        failed
    );
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The repository root of this checkout.
    fn repo_root() -> Option<PathBuf> {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(Path::to_path_buf)
    }

    #[test]
    fn the_repository_biomes_are_found_and_plausible() {
        let root = repo_root().expect("root");
        let files = biome_files(&root);
        let names: Vec<String> = files
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        for want in [
            "birch_woodland.biome.ron",
            "boreal_spruce_forest.biome.ron",
            "pine_forest.biome.ron",
        ] {
            assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
        }
        for f in &files {
            let c = check_file(f);
            match &c.report {
                Ok(r) => assert!(r.plausible(), "{}: {:?}", f.display(), r.issues),
                Err(e) => panic!("{}: {e}", f.display()),
            }
            if f.to_string_lossy().contains("starter/biomes/") {
                assert!(c.ecology.is_some(), "{} ships a profile", f.display());
            }
        }
    }

    #[test]
    fn a_birch_forest_with_maple_fails_with_the_reason() {
        let text = r#"BiomeDef(name: "Birch forest", layers: [(name: "canopy", entries: [
            (name: "birch", spawn: Mesh("trees/birch.glb"), weight: 6.0),
            (name: "maple", spawn: Mesh("trees/maple.glb"), weight: 2.0)])])"#;
        let r = check_texts(text, None).expect("parses");
        assert!(!r.plausible());
        let i = r
            .issues
            .iter()
            .find(|i| i.path == "canopy/maple")
            .expect("maple flagged");
        assert_eq!(i.code, "ecology_reaction");
        assert!(check_texts("BiomeDef(name: 3)", None).is_err());
    }
}
