// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask doc-examples`: the ratchet on compiled documentation examples of the stable crates.
//!
//! A stable crate ([`crate::api_snapshot::STABLE_CRATES`], ADR-2527) shows a user how to start with
//! it in an example that the merge gate compiles (`cargo test --workspace` runs doctests). This
//! command counts the fenced code blocks in the `///` and `//!` comments under each stable crate's
//! `src`, separately for the crate-level docs (`//!` in `lib.rs`) and the item docs (everything else),
//! and holds the counts to `docs/ci/doc-examples.toml`:
//!
//! - **Compiled** fences are untagged, `rust`, `no_run`, `should_panic` and `compile_fail` ones. A
//!   crate may not drop below its recorded floor of them, crate-level and item-level.
//! - **Ignored** fences are `ignore` ones (`rust,ignore`): nothing checks them. A crate may not exceed
//!   its recorded ceiling. Fences in another language (`text`, `ron`, `toml`, `sh`, ...) are not
//!   code examples and are not counted.
//! - Every stable crate needs at least one crate-level compiled example, unless its row carries a
//!   `waiver` (a reason, for crates another lane or a later sprint covers). A waiver on a crate that
//!   has the example is stale and fails the check.
//!
//! `--update` records a raise (a higher floor, a lower ceiling, a new crate, a stale waiver removed).
//! Lowering a floor or raising a ceiling needs `--lower "<why>"`, which appends the reason to the
//! file's `lowered` list.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::api_snapshot::STABLE_CRATES;

/// The ratchet file, relative to the workspace root.
pub const FLOORS_PATH: &str = "docs/ci/doc-examples.toml";

/// The `format_version` the ratchet file carries.
pub const FORMAT_VERSION: u32 = 1;

/// Written above the tables, since a TOML round trip drops comments.
const HEADER: &str = "\
# Compiled documentation examples per stable crate (ADR-2527 crates), held by `cargo xtask doc-examples`.
# `*_compiled` are floors (untagged, rust, no_run, should_panic and compile_fail fences); `*_ignored_max`
# are ceilings (ignore fences). `crate_` rows count `//!` in lib.rs, `item_` rows everything else under src.
# A `waiver` excuses the missing crate-level example of one crate for the reason given. `--update`
# records a raise; a lower floor or higher ceiling needs `--lower \"<why>\"`, which appends to `lowered`.

";

/// Fence counts of one level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Fences the doctest harness compiles.
    pub compiled: u32,
    /// `ignore` fences.
    pub ignored: u32,
}

/// What a source scan found in one crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CrateCounts {
    /// `//!` fences in `lib.rs`.
    pub crate_level: Counts,
    /// Every other doc fence under `src`.
    pub items: Counts,
}

impl CrateCounts {
    fn add(&mut self, other: Self) {
        self.crate_level.compiled += other.crate_level.compiled;
        self.crate_level.ignored += other.crate_level.ignored;
        self.items.compiled += other.items.compiled;
        self.items.ignored += other.items.ignored;
    }
}

/// One crate's recorded floors.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrateRow {
    /// Floor of crate-level compiled fences.
    pub crate_compiled: u32,
    /// Ceiling of crate-level `ignore` fences.
    pub crate_ignored_max: u32,
    /// Floor of item-level compiled fences.
    pub item_compiled: u32,
    /// Ceiling of item-level `ignore` fences.
    pub item_ignored_max: u32,
    /// Why the crate has no crate-level compiled example yet. Empty when it has one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub waiver: String,
}

/// One accepted lowering, appended by `--lower`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lowered {
    /// The crate.
    pub krate: String,
    /// The field of [`CrateRow`].
    pub field: String,
    /// The value before.
    pub from: u32,
    /// The value after.
    pub to: u32,
    /// Why.
    pub reason: String,
}

/// `docs/ci/doc-examples.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RatchetFile {
    /// [`FORMAT_VERSION`].
    pub format_version: u32,
    /// Crate name to its row.
    #[serde(default)]
    pub crates: BTreeMap<String, CrateRow>,
    /// Every lowering accepted so far, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lowered: Vec<Lowered>,
}

impl RatchetFile {
    /// Parses the file's text.
    ///
    /// # Errors
    /// Bad TOML or another `format_version`.
    pub fn parse(text: &str) -> Result<Self> {
        let file: Self = toml::from_str(text).context("parsing the doc-examples file")?;
        if file.format_version != FORMAT_VERSION {
            bail!(
                "{FLOORS_PATH}: format_version {} is not {FORMAT_VERSION}",
                file.format_version
            );
        }
        Ok(file)
    }

    /// The file's text.
    ///
    /// # Errors
    /// A failed TOML serialization.
    pub fn render(&self) -> Result<String> {
        Ok(format!("{HEADER}{}", toml::to_string_pretty(self)?))
    }
}

/// How a fence's info string is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FenceKind {
    Compiled,
    Ignored,
    /// Another language or a plain-text block.
    Other,
}

/// Classifies the text after the opening backticks.
fn classify(info: &str) -> FenceKind {
    let mut ignored = false;
    for token in info.split([',', ' ', '\t']).filter(|t| !t.is_empty()) {
        if token.starts_with("ignore") {
            ignored = true;
        } else if !matches!(
            token,
            "rust" | "no_run" | "should_panic" | "compile_fail" | "test_harness"
        ) && !token.starts_with("edition")
        {
            return FenceKind::Other;
        }
    }
    if ignored {
        FenceKind::Ignored
    } else {
        FenceKind::Compiled
    }
}

/// Splits a doc fence line into its fence character, its length and the info string.
fn fence_of(body: &str) -> Option<(char, usize, &str)> {
    let first = body.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let len = body.chars().take_while(|c| *c == first).count();
    (len >= 3).then(|| (first, len, &body[len..]))
}

/// Counts the doc fences in one source file. `crate_root` is true for `lib.rs`, whose `//!` lines
/// are the crate-level docs.
pub fn count_source(text: &str, crate_root: bool) -> CrateCounts {
    let mut out = CrateCounts::default();
    // (fence char, length) of the open fence and where its counts go.
    let mut open: Option<(char, usize)> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let (inner, body) = if let Some(rest) = trimmed.strip_prefix("//!") {
            (true, rest)
        } else if let Some(rest) = trimmed.strip_prefix("///") {
            if rest.starts_with('/') {
                // `////` is a plain comment, not a doc comment.
                open = None;
                continue;
            }
            (false, rest)
        } else {
            open = None;
            continue;
        };
        let body = body.trim();
        match open {
            None => {
                if let Some((ch, len, info)) = fence_of(body) {
                    open = Some((ch, len));
                    let level = if inner && crate_root {
                        &mut out.crate_level
                    } else {
                        &mut out.items
                    };
                    match classify(info) {
                        FenceKind::Compiled => level.compiled += 1,
                        FenceKind::Ignored => level.ignored += 1,
                        FenceKind::Other => {}
                    }
                }
            }
            Some((ch, len)) => {
                if body.len() >= len && body.chars().all(|c| c == ch) {
                    open = None;
                }
            }
        }
    }
    out
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| dir.display().to_string())? {
        entries.push(entry?.path());
    }
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Scans `crates/<name>/src`.
///
/// # Errors
/// An unreadable source directory or file.
pub fn scan_crate(root: &Path, name: &str) -> Result<CrateCounts> {
    let src = root.join("crates").join(name).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files)?;
    let lib = src.join("lib.rs");
    let mut total = CrateCounts::default();
    for file in files {
        let text = std::fs::read_to_string(&file).with_context(|| file.display().to_string())?;
        total.add(count_source(&text, file == lib));
    }
    Ok(total)
}

/// What the check found wrong, one line each.
pub fn check(file: &RatchetFile, counts: &BTreeMap<String, CrateCounts>) -> Vec<String> {
    let mut problems = Vec::new();
    for name in STABLE_CRATES {
        let Some(found) = counts.get(*name) else {
            problems.push(format!("{name}: not scanned"));
            continue;
        };
        let Some(row) = file.crates.get(*name) else {
            problems.push(format!(
                "{name}: no row in {FLOORS_PATH}; run `cargo xtask doc-examples --update`"
            ));
            continue;
        };
        let rows = [
            (
                "crate_compiled",
                found.crate_level.compiled,
                row.crate_compiled,
                true,
            ),
            ("item_compiled", found.items.compiled, row.item_compiled, true),
            (
                "crate_ignored_max",
                found.crate_level.ignored,
                row.crate_ignored_max,
                false,
            ),
            (
                "item_ignored_max",
                found.items.ignored,
                row.item_ignored_max,
                false,
            ),
        ];
        for (field, now, recorded, is_floor) in rows {
            if is_floor && now < recorded {
                problems.push(format!(
                    "{name}: {field} dropped from {recorded} to {now}; restore the example(s), or `--lower \"<why>\"`"
                ));
            }
            if !is_floor && now > recorded {
                problems.push(format!(
                    "{name}: {field} is {now} ignored example(s), the ceiling is {recorded}; make it a compiled or no_run example"
                ));
            }
        }
        match (found.crate_level.compiled, row.waiver.is_empty()) {
            (0, true) => problems.push(format!(
                "{name}: no crate-level compiled example; add a ```` ``` ```` block to the `//!` docs of lib.rs that shows how to start (use `no_run` when it needs a window or a GPU)"
            )),
            (n, false) if n > 0 => problems.push(format!(
                "{name}: has a crate-level compiled example, so the waiver is stale; remove it (or run --update)"
            )),
            _ => {}
        }
    }
    for name in file.crates.keys() {
        if !STABLE_CRATES.contains(&name.as_str()) {
            problems.push(format!("{name}: in {FLOORS_PATH} but not a stable crate"));
        }
    }
    problems
}

/// The file after `--update`, or why it cannot be written. `lower` is the reason that allows a lower
/// floor or a higher ceiling.
///
/// # Errors
/// A floor that would drop or a ceiling that would rise without a `lower` reason.
pub fn update(
    file: &RatchetFile,
    counts: &BTreeMap<String, CrateCounts>,
    lower: Option<&str>,
) -> Result<(RatchetFile, Vec<String>)> {
    let mut next = file.clone();
    next.format_version = FORMAT_VERSION;
    let mut notes = Vec::new();
    let mut refused = Vec::new();
    for name in STABLE_CRATES {
        let Some(found) = counts.get(*name) else {
            continue;
        };
        let row = next.crates.entry((*name).to_string()).or_insert_with(|| {
            notes.push(format!("{name}: new row"));
            CrateRow {
                crate_compiled: found.crate_level.compiled,
                crate_ignored_max: found.crate_level.ignored,
                item_compiled: found.items.compiled,
                item_ignored_max: found.items.ignored,
                waiver: String::new(),
            }
        });
        let targets = [
            ("crate_compiled", found.crate_level.compiled, true),
            ("item_compiled", found.items.compiled, true),
            ("crate_ignored_max", found.crate_level.ignored, false),
            ("item_ignored_max", found.items.ignored, false),
        ];
        for (field, now, is_floor) in targets {
            let slot = match field {
                "crate_compiled" => &mut row.crate_compiled,
                "item_compiled" => &mut row.item_compiled,
                "crate_ignored_max" => &mut row.crate_ignored_max,
                _ => &mut row.item_ignored_max,
            };
            if *slot == now {
                continue;
            }
            // A raise of a floor or a tightening of a ceiling is free; the other way is a lowering.
            let better = if is_floor { now > *slot } else { now < *slot };
            if !better {
                let Some(why) = lower else {
                    refused.push(format!(
                        "{name}: {field} {} -> {now} is a lowering; pass --lower \"<why>\"",
                        *slot
                    ));
                    continue;
                };
                next.lowered.push(Lowered {
                    krate: (*name).to_string(),
                    field: field.to_string(),
                    from: *slot,
                    to: now,
                    reason: why.to_string(),
                });
            }
            notes.push(format!("{name}: {field} {} -> {now}", *slot));
            *slot = now;
        }
        if found.crate_level.compiled > 0 && !row.waiver.is_empty() {
            notes.push(format!("{name}: waiver removed"));
            row.waiver.clear();
        }
    }
    if !refused.is_empty() {
        bail!("{}", refused.join("\n"));
    }
    Ok((next, notes))
}

/// Arguments of `cargo xtask doc-examples`.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct Args {
    /// Record a raise (higher floors, lower ceilings, new crates) in `docs/ci/doc-examples.toml`.
    #[arg(long)]
    pub update: bool,
    /// With `--update`: accept a lower floor or a higher ceiling for this reason, appended to the file.
    #[arg(long, value_name = "WHY", requires = "update")]
    pub lower: Option<String>,
}

fn scan_all(root: &Path) -> Result<BTreeMap<String, CrateCounts>> {
    let mut counts = BTreeMap::new();
    for name in STABLE_CRATES {
        counts.insert((*name).to_string(), scan_crate(root, name)?);
    }
    Ok(counts)
}

fn print_table(counts: &BTreeMap<String, CrateCounts>) {
    println!("{:<18} {:>14} {:>14}", "crate", "crate c/i", "items c/i");
    for (name, c) in counts {
        println!(
            "{name:<18} {:>9}/{:<4} {:>9}/{:<4}",
            c.crate_level.compiled, c.crate_level.ignored, c.items.compiled, c.items.ignored
        );
    }
}

/// Runs the command.
///
/// # Errors
/// An unreadable source tree or ratchet file, or a refused `--update`.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let counts = scan_all(root)?;
    let path = root.join(FLOORS_PATH);
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => RatchetFile::parse(&text)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && args.update => RatchetFile {
            format_version: FORMAT_VERSION,
            ..RatchetFile::default()
        },
        Err(err) => return Err(err).with_context(|| path.display().to_string()),
    };
    if args.update {
        let (next, notes) = update(&existing, &counts, args.lower.as_deref())?;
        if next == existing {
            println!("doc-examples: {FLOORS_PATH} is up to date");
        } else {
            std::fs::write(&path, next.render()?).with_context(|| path.display().to_string())?;
            for note in &notes {
                println!("doc-examples: {note}");
            }
            println!("doc-examples: wrote {FLOORS_PATH}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let problems = check(&existing, &counts);
    if problems.is_empty() {
        print_table(&counts);
        println!("doc-examples: ok");
        Ok(ExitCode::SUCCESS)
    } else {
        for problem in &problems {
            eprintln!("error: {problem}");
        }
        eprintln!("doc-examples: {} problem(s)", problems.len());
        Ok(ExitCode::FAILURE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"//! Crate docs.
//!
//! ```
//! let a = 1;
//! ```
//!
//! ```no_run
//! loop {}
//! ```
//!
//! ```rust,ignore
//! needs_a_window();
//! ```
//!
//! ```text
//! not code
//! ```

/// An item.
///
/// ```
/// assert_eq!(1, 1);
/// ```
/// ```should_panic
/// panic!();
/// ```
/// ```ron
/// (a: 1)
/// ```
pub fn item() {}

    /// Indented, with a longer fence that contains a shorter one.
    ///
    /// ````rust
    /// ```
    /// ````
    pub fn indented() {}

//// ```
//// not a doc comment
//// ```
// ```
// a plain comment
// ```
"#;

    #[test]
    fn counts_crate_level_and_item_fences_apart() {
        let c = count_source(SOURCE, true);
        assert_eq!(
            c.crate_level,
            Counts {
                compiled: 2,
                ignored: 1
            }
        );
        assert_eq!(
            c.items,
            Counts {
                compiled: 3,
                ignored: 0
            }
        );
    }

    #[test]
    fn inner_docs_outside_lib_rs_count_as_items() {
        let c = count_source(SOURCE, false);
        assert_eq!(c.crate_level, Counts::default());
        assert_eq!(
            c.items,
            Counts {
                compiled: 5,
                ignored: 1
            }
        );
    }

    #[test]
    fn classifies_info_strings() {
        assert_eq!(classify(""), FenceKind::Compiled);
        assert_eq!(classify("rust"), FenceKind::Compiled);
        assert_eq!(classify("no_run"), FenceKind::Compiled);
        assert_eq!(classify("rust,no_run"), FenceKind::Compiled);
        assert_eq!(classify("should_panic"), FenceKind::Compiled);
        assert_eq!(classify("edition2021"), FenceKind::Compiled);
        assert_eq!(classify("ignore"), FenceKind::Ignored);
        assert_eq!(classify("rust,ignore"), FenceKind::Ignored);
        assert_eq!(classify("text"), FenceKind::Other);
        assert_eq!(classify("ron"), FenceKind::Other);
        assert_eq!(classify("sh"), FenceKind::Other);
    }

    #[test]
    fn an_unclosed_fence_ends_with_its_comment_block() {
        let text = "/// ```\n/// a\nfn f() {}\n/// ```\n/// b\n";
        let c = count_source(text, false);
        // Two openings: the first block never closed, the second starts fresh after code.
        assert_eq!(c.items.compiled, 2);
    }

    fn counts_for_all(per_crate: CrateCounts) -> BTreeMap<String, CrateCounts> {
        STABLE_CRATES
            .iter()
            .map(|n| ((*n).to_string(), per_crate))
            .collect()
    }

    fn sample() -> CrateCounts {
        CrateCounts {
            crate_level: Counts {
                compiled: 1,
                ignored: 0,
            },
            items: Counts {
                compiled: 3,
                ignored: 1,
            },
        }
    }

    fn recorded(counts: &BTreeMap<String, CrateCounts>) -> RatchetFile {
        update(
            &RatchetFile {
                format_version: FORMAT_VERSION,
                ..RatchetFile::default()
            },
            counts,
            None,
        )
        .expect("a first update records everything")
        .0
    }

    #[test]
    fn a_fresh_update_then_check_is_clean() {
        let counts = counts_for_all(sample());
        let file = recorded(&counts);
        assert_eq!(file.crates.len(), STABLE_CRATES.len());
        assert!(check(&file, &counts).is_empty());
    }

    #[test]
    fn dropping_below_a_floor_fails() {
        let mut counts = counts_for_all(sample());
        let file = recorded(&counts);
        counts.get_mut("regia_nav").expect("row").items.compiled = 2;
        let problems = check(&file, &counts);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("regia_nav") && problems[0].contains("item_compiled"));
    }

    #[test]
    fn exceeding_an_ignore_ceiling_fails() {
        let mut counts = counts_for_all(sample());
        let file = recorded(&counts);
        counts.get_mut("regia_media").expect("row").crate_level.ignored = 1;
        let problems = check(&file, &counts);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("crate_ignored_max"));
    }

    #[test]
    fn a_crate_without_a_crate_level_example_fails_unless_waived() {
        let mut counts = counts_for_all(sample());
        counts.get_mut("regia_world").expect("row").crate_level.compiled = 0;
        let mut file = recorded(&counts);
        let problems = check(&file, &counts);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("no crate-level compiled example"));
        file.crates.get_mut("regia_world").expect("row").waiver = "another lane".into();
        assert!(check(&file, &counts).is_empty());
    }

    #[test]
    fn a_waiver_on_a_crate_that_has_the_example_is_stale() {
        let counts = counts_for_all(sample());
        let mut file = recorded(&counts);
        file.crates.get_mut("regia_nav").expect("row").waiver = "old".into();
        let problems = check(&file, &counts);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("stale"));
        // `--update` removes it.
        let (next, notes) = update(&file, &counts, None).expect("a stale waiver is cleaned");
        assert!(next.crates["regia_nav"].waiver.is_empty());
        assert!(notes.iter().any(|n| n.contains("waiver removed")));
    }

    #[test]
    fn update_records_a_raise_and_refuses_a_lowering_without_a_reason() {
        let mut counts = counts_for_all(sample());
        let file = recorded(&counts);
        let pick = counts.get_mut("regia_pick").expect("row");
        pick.items.compiled = 6;
        pick.items.ignored = 0;
        let (raised, _) = update(&file, &counts, None).expect("a raise needs no reason");
        assert_eq!(raised.crates["regia_pick"].item_compiled, 6);
        assert_eq!(raised.crates["regia_pick"].item_ignored_max, 0);
        assert!(raised.lowered.is_empty());

        counts.get_mut("regia_pick").expect("row").items.compiled = 2;
        let refused = update(&raised, &counts, None).expect_err("a lowering needs a reason");
        assert!(refused.to_string().contains("--lower"));

        let (lowered, _) =
            update(&raised, &counts, Some("moved to regia_pick_ext")).expect("a lowering with a reason");
        assert_eq!(lowered.crates["regia_pick"].item_compiled, 2);
        assert_eq!(lowered.lowered.len(), 1);
        assert_eq!(lowered.lowered[0].reason, "moved to regia_pick_ext");
        assert_eq!((lowered.lowered[0].from, lowered.lowered[0].to), (6, 2));
    }

    #[test]
    fn a_missing_row_and_a_foreign_row_are_reported() {
        let counts = counts_for_all(sample());
        let mut file = recorded(&counts);
        file.crates.remove("regia_xr");
        file.crates.insert("not_stable".into(), CrateRow::default());
        let problems = check(&file, &counts);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.starts_with("regia_xr: no row")));
        assert!(problems.iter().any(|p| p.starts_with("not_stable")));
    }

    #[test]
    fn the_file_round_trips_and_checks_its_version() {
        let counts = counts_for_all(sample());
        let mut file = recorded(&counts);
        file.lowered.push(Lowered {
            krate: "regia".into(),
            field: "item_compiled".into(),
            from: 4,
            to: 3,
            reason: "x".into(),
        });
        let text = file.render().expect("renders");
        assert!(text.starts_with("# Compiled documentation examples"));
        assert_eq!(RatchetFile::parse(&text).expect("parses"), file);
        let bad = text.replace("format_version = 1", "format_version = 2");
        assert!(RatchetFile::parse(&bad).is_err());
    }
}
