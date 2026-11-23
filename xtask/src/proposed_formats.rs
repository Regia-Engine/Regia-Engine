// SPDX-License-Identifier: MPL-2.0
//! Proposed content formats: formats an ADR designs before any crate defines them (ADR-8019).
//!
//! `check-formats` hashes the serde shape of every *built* format, so a format that exists only in a
//! design (an ADR and a spec page, no type yet) is invisible to it. Build lanes then find out late
//! that the format had no `docs/specs/12-assets.md` § 4a row, no freeze line and no owner. The
//! register `docs/formats/proposed-formats.ron` lists those designed formats, and this check holds
//! each one to the same paperwork a built format has:
//!
//! - a § 4a table row that names the file in backticks, the ADR and the word `proposed`;
//! - the spec page it names exists and mentions the file and the registry name;
//! - the ADR exists in `docs/decisions.md`;
//! - the name is not a built format yet: the build lane that registers the live format (and its
//!   `format-hashes.ron` row) removes the register entry in the same commit;
//! - names and files are unique, the version is at least 1 and the kind is `source` or `cooked`.
//!
//! `cargo xtask check-formats` runs it, so the merge gate does too.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

/// Where the register lives, relative to the repository root.
pub const REGISTER: &str = "docs/formats/proposed-formats.ron";

/// The spec page that holds the format table.
pub const ASSETS_SPEC: &str = "docs/specs/12-assets.md";

/// The register file.
#[derive(Debug, Deserialize)]
pub struct ProposedFormats {
    /// Designed, unbuilt formats.
    pub formats: Vec<ProposedFormat>,
}

/// One designed format.
#[derive(Debug, Clone, Deserialize)]
pub struct ProposedFormat {
    /// The registry name the build lane will use (`check-formats`, `regia upgrade-project`).
    pub name: String,
    /// The file pattern as the spec writes it, for example `<map>.meshlayers.ron`.
    pub file: String,
    /// `source` (authored, migrated forever) or `cooked` (derived data, recooked on a mismatch).
    pub kind: String,
    /// The version the first build writes.
    pub version: u32,
    /// The ADR that designs it, for example `ADR-8019`.
    pub adr: String,
    /// The spec page that specifies the shape, relative to the repository root.
    pub spec: String,
    /// The build lane (plan row) that turns it into a built format.
    pub lane: String,
}

/// Reads the register at `root`.
pub fn load(root: &Path) -> anyhow::Result<ProposedFormats> {
    let path = root.join(REGISTER);
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    ron::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The § 4a section of the assets spec: from its `## 4a.` heading to the next `## ` heading.
pub fn section_4a(spec_text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in spec_text.lines() {
        if line.starts_with("## ") {
            inside = line.starts_with("## 4a.");
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Every problem with `register`, given the § 4a section text, a reader for spec pages (by
/// repository-relative path; `None` when the file does not exist), the ADR headings present
/// (`ADR-NNNN`) and the names of the built formats.
pub fn problems(
    register: &ProposedFormats,
    section_4a: &str,
    read_spec: &dyn Fn(&str) -> Option<String>,
    adrs: &BTreeSet<String>,
    built: &BTreeSet<String>,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut names = BTreeSet::new();
    let mut files = BTreeSet::new();
    for f in &register.formats {
        let label = format!("proposed format `{}` ({})", f.name, f.file);
        if !names.insert(f.name.as_str()) {
            errors.push(format!("{label}: the name is listed twice in {REGISTER}"));
        }
        if !files.insert(f.file.as_str()) {
            errors.push(format!("{label}: the file is listed twice in {REGISTER}"));
        }
        if f.version == 0 {
            errors.push(format!("{label}: version 0; a format starts at v1"));
        }
        if f.kind != "source" && f.kind != "cooked" {
            errors.push(format!("{label}: kind `{}`; use `source` or `cooked`", f.kind));
        }
        if f.lane.trim().is_empty() {
            errors.push(format!("{label}: no build lane named"));
        }
        if built.contains(&f.name) {
            errors.push(format!(
                "{label}: already a built format (it has a format-hashes row); remove it from {REGISTER}"
            ));
        }
        if !adrs.contains(&f.adr) {
            errors.push(format!(
                "{label}: {} is not an ADR heading in docs/decisions.md",
                f.adr
            ));
        }
        let ticked = format!("`{}`", f.file);
        let has_row = section_4a.lines().any(|line| {
            line.trim_start().starts_with('|')
                && line.contains(&ticked)
                && line.contains(&f.adr)
                && line.contains("proposed")
        });
        if !has_row {
            errors.push(format!(
                "{label}: no row in {ASSETS_SPEC} § 4a that names {ticked}, {} and `proposed`",
                f.adr
            ));
        }
        match read_spec(&f.spec) {
            None => errors.push(format!("{label}: its spec page {} does not exist", f.spec)),
            Some(text) => {
                if !text.contains(&ticked) {
                    errors.push(format!("{label}: {} never mentions {ticked}", f.spec));
                }
                if !text.contains(&format!("`{}`", f.name)) {
                    errors.push(format!(
                        "{label}: {} never names the registry name `{}`",
                        f.spec, f.name
                    ));
                }
            }
        }
    }
    errors
}

/// The ADR numbers that have a heading (`## ADR-NNNN:`) in `decisions`.
pub fn adr_headings(decisions: &str) -> BTreeSet<String> {
    decisions
        .lines()
        .filter_map(|line| line.strip_prefix("## "))
        .filter_map(|rest| rest.split(':').next())
        .filter(|id| id.starts_with("ADR-"))
        .map(str::to_string)
        .collect()
}

/// Checks the register at `root` against the repository files and the built format names.
pub fn check(root: &Path, built: &BTreeSet<String>) -> Vec<String> {
    let register = match load(root) {
        Ok(register) => register,
        Err(err) => return vec![format!("{err:#}")],
    };
    let assets = match std::fs::read_to_string(root.join(ASSETS_SPEC)) {
        Ok(text) => text,
        Err(err) => return vec![format!("reading {ASSETS_SPEC}: {err}")],
    };
    let decisions = match std::fs::read_to_string(root.join("docs/decisions.md")) {
        Ok(text) => text,
        Err(err) => return vec![format!("reading docs/decisions.md: {err}")],
    };
    let read_spec = |rel: &str| std::fs::read_to_string(root.join(rel)).ok();
    problems(
        &register,
        &section_4a(&assets),
        &read_spec,
        &adr_headings(&decisions),
        built,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(name: &str, file: &str) -> ProposedFormat {
        ProposedFormat {
            name: name.to_string(),
            file: file.to_string(),
            kind: "source".to_string(),
            version: 1,
            adr: "ADR-1".to_string(),
            spec: "docs/specs/x.md".to_string(),
            lane: "W-1".to_string(),
        }
    }

    fn adrs() -> BTreeSet<String> {
        ["ADR-1".to_string()].into_iter().collect()
    }

    fn spec_page(rel: &str) -> Option<String> {
        (rel == "docs/specs/x.md").then(|| "`thing` lives in `<map>.thing.ron`.".to_string())
    }

    const ROW: &str = "| `<map>.thing.ron` (ADR-1) | Regia Engine | proposed |\n";

    #[test]
    fn a_proposed_format_with_its_row_page_and_adr_passes() {
        let register = ProposedFormats {
            formats: vec![format("thing", "<map>.thing.ron")],
        };
        let errors = problems(&register, ROW, &spec_page, &adrs(), &BTreeSet::new());
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn a_proposed_format_without_a_spec_row_is_named() {
        let register = ProposedFormats {
            formats: vec![format("thing", "<map>.thing.ron")],
        };
        let errors = problems(&register, "", &spec_page, &adrs(), &BTreeSet::new());
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("no row in docs/specs/12-assets.md § 4a"),
            "{}",
            errors[0]
        );
        // A row that names a longer file (`<map>.thing.ron.bak`) or forgets `proposed` is not the row.
        let other = "| `<map>.thing.ron.bak` (ADR-1) | proposed |\n| `<map>.thing.ron` (ADR-1) | v1 |\n";
        let errors = problems(&register, other, &spec_page, &adrs(), &BTreeSet::new());
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn a_missing_page_adr_or_registry_name_is_named() {
        let mut f = format("thing", "<map>.thing.ron");
        f.adr = "ADR-2".to_string();
        f.spec = "docs/specs/missing.md".to_string();
        let row = "| `<map>.thing.ron` (ADR-2) | proposed |\n";
        let register = ProposedFormats { formats: vec![f] };
        let errors = problems(&register, row, &spec_page, &adrs(), &BTreeSet::new());
        assert!(
            errors.iter().any(|e| e.contains("ADR-2 is not an ADR heading")),
            "{errors:?}"
        );
        assert!(errors.iter().any(|e| e.contains("does not exist")), "{errors:?}");
        let mut g = format("other-name", "<map>.thing.ron");
        g.spec = "docs/specs/x.md".to_string();
        let register = ProposedFormats { formats: vec![g] };
        let errors = problems(&register, ROW, &spec_page, &adrs(), &BTreeSet::new());
        assert!(
            errors
                .iter()
                .any(|e| e.contains("never names the registry name `other-name`")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_built_format_must_leave_the_register() {
        let register = ProposedFormats {
            formats: vec![format("thing", "<map>.thing.ron")],
        };
        let built: BTreeSet<String> = ["thing".to_string()].into_iter().collect();
        let errors = problems(&register, ROW, &spec_page, &adrs(), &built);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("already a built format"), "{}", errors[0]);
    }

    #[test]
    fn duplicates_a_zero_version_and_a_bad_kind_are_refused() {
        let mut bad = format("thing", "<map>.thing.ron");
        bad.version = 0;
        bad.kind = "binary".to_string();
        let register = ProposedFormats {
            formats: vec![format("thing", "<map>.thing.ron"), bad],
        };
        let errors = problems(&register, ROW, &spec_page, &adrs(), &BTreeSet::new());
        assert!(
            errors.iter().any(|e| e.contains("name is listed twice")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("file is listed twice")),
            "{errors:?}"
        );
        assert!(errors.iter().any(|e| e.contains("version 0")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("kind `binary`")), "{errors:?}");
    }

    #[test]
    fn section_4a_stops_at_the_next_heading_and_adr_headings_are_read() {
        let spec = "## 4. Before\n| `<a>` proposed ADR-1 |\n## 4a. Formats\n| `<b>` |\n### 4a.1 sub\nmore\n## 5. After\n| `<c>` |\n";
        let s = section_4a(spec);
        assert!(s.contains("`<b>`") && s.contains("more"), "{s}");
        assert!(!s.contains("`<a>`") && !s.contains("`<c>`"), "{s}");
        let adrs = adr_headings("## ADR-7974: Title\ntext ADR-9 in prose\n## ADR-8019: x\n## Not an ADR\n");
        assert_eq!(
            adrs.into_iter().collect::<Vec<_>>(),
            vec!["ADR-7974".to_string(), "ADR-8019".to_string()]
        );
    }
}
