// SPDX-License-Identifier: MPL-2.0
//! `[[do27]]` rows of `docs/ported-sources.toml`: code copied or adapted from the owner's own game,
//! Direct Order 27, under the owner's 2026-11-16 copy exception (ADR-8012 part A-1).
//!
//! Each row names one DO27 file (crate, path, line range), the DO27 commit it was read at, the
//! manifest mark (`as-is`, `adapt`, `requirements`) and the file's provenance class (`P1`-`P3`;
//! `P4` baseline-game values and `P5` privacy files never enter Regia, `P6` third-party crates use
//! a `[[source]]` row). `cargo xtask notices --check` runs [`validate`] and [`check_files`]; the
//! rows are not rendered into `THIRD_PARTY_NOTICES.md` (the owner's own code needs no notice).
//!
//! A draft row carries `planned = "<plan lane>"` (ADR-8041): the absorb-plan lane that will copy the file.
//! It has no `files_to` and is not checked on disk until that lane lands the code and deletes the field.
//! A draft must not overlap a landed range of the same source file (ADR-8242).
//!
//! A copied data file whose format cannot carry a comment (JSON, binary vectors; [`HEADERLESS_DATA`])
//! carries its header in a sidecar `<file>.NOTICE` next to it instead, and needs a
//! `docs/provenance.md` row of class `do27-owner-code` naming its DO27 path and commit (ADR-8497).

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

/// Lines at the top of a copied file that must hold its "Portions copied from Direct Order 27" header.
pub const HEADER_LINES: usize = 40;

/// The manifest marks a row may carry (`docs/specs/25-sim-bridge-and-do27.md` § 2).
pub const MARKS: &[&str] = &["as-is", "adapt", "requirements"];

/// DO27 identifiers that a copied file must not keep (ADR-8012 A-1; rev-b M3 adds the hull name).
pub const DO27_IDENTIFIERS: &[&str] = &["ironreach", "warhull"];

/// Extensions of data formats that cannot carry a comment header (ADR-8497). A copied file of one
/// of these kinds carries its header in the sidecar `<file>.NOTICE` ([`NOTICE_SIDECAR`]).
pub const HEADERLESS_DATA: &[&str] = &["json", "jsonl", "bin", "png", "glb", "ktx2", "wav", "ogg"];

/// Suffix of the sidecar that holds a headerless data file's header (`fixture.json.NOTICE`).
pub const NOTICE_SIDECAR: &str = ".NOTICE";

/// The provenance log a copied data file must have a row in (ADR-8497).
const PROVENANCE_LOG: &str = "docs/provenance.md";

/// File names of DO27's privacy class (P5): never copied, whatever class a row claims.
const PRIVACY_FILES: &[&str] = &["upload.rs", "secret.rs", "reserved_names"];

/// One DO27 file (or line range) that Regia copied, adapted or took requirements from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Do27Port {
    /// DO27 crate or top-level folder, e.g. `ir-sim` or `shell-regia/game`.
    #[serde(rename = "crate")]
    pub krate: String,
    /// Path inside the crate, e.g. `src/math.rs`.
    pub path: String,
    /// One-based inclusive line range `A-B`, or `all`.
    pub lines: String,
    /// The DO27 commit the file was read at (9 to 40 lowercase hex digits).
    pub sha: String,
    /// `as-is`, `adapt` or `requirements` (the manifest mark; binding).
    pub mark: String,
    /// DO27 provenance class: `P1`, `P2` or `P3`.
    pub class: String,
    /// For a `P2` file that is copied: the per-file review that cleared it (e.g. `D-02`).
    #[serde(default)]
    pub cleared_by: String,
    /// For `adapt`: the named issue the adaptation fixes.
    #[serde(default)]
    pub note: String,
    /// Repository-relative Regia paths that hold the copied code (or implement the requirement).
    #[serde(default)]
    pub files_to: Vec<String>,
    /// A draft row (ADR-8041): the absorb-plan lane that will copy the file (e.g. `F-01`). The
    /// copying lane deletes the field when the code lands; until then `files_to` must be empty.
    #[serde(default)]
    pub planned: String,
}

impl Do27Port {
    fn name(&self) -> String {
        format!(
            "do27 {}/{} ({})",
            self.krate.trim(),
            self.path.trim(),
            self.lines.trim()
        )
    }

    fn copies_code(&self) -> bool {
        matches!(self.mark.as_str(), "as-is" | "adapt")
    }
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    do27: Vec<Do27Port>,
    // The `[[source]]` rows belong to `notices`; they are parsed and checked there.
    #[serde(default, rename = "source")]
    _source: Vec<toml::Value>,
}

/// Parses the `[[do27]]` rows of `docs/ported-sources.toml` text.
pub fn parse(text: &str) -> Result<Vec<Do27Port>> {
    let file: File = toml::from_str(text)?;
    Ok(file.do27)
}

/// Loads the `[[do27]]` rows from the workspace root; a missing file means nothing was ported.
pub fn load(root: &Path) -> Result<Vec<Do27Port>> {
    let path = root.join(crate::notices::PORTED_SOURCES);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn relative_path(p: &str) -> bool {
    let p = p.trim();
    !p.is_empty() && !p.starts_with('/') && !p.contains('\\') && !p.split('/').any(|part| part == "..")
}

fn line_range(lines: &str) -> bool {
    let lines = lines.trim();
    if lines == "all" {
        return true;
    }
    let Some((a, b)) = lines.split_once('-') else {
        return false;
    };
    match (a.parse::<u32>(), b.parse::<u32>()) {
        (Ok(a), Ok(b)) => (1..=b).contains(&a),
        _ => false,
    }
}

fn commit_sha(sha: &str) -> bool {
    (9..=40).contains(&sha.len())
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// An absorb-plan lane id: one or two capital letters, an optional `-`, then two to four digits
/// (`F-01`, `NL-02`, `N224`).
fn plan_lane(id: &str) -> bool {
    let letters = id.bytes().take_while(u8::is_ascii_uppercase).count();
    let rest = id[letters..].strip_prefix('-').unwrap_or(&id[letters..]);
    (1..=2).contains(&letters) && (2..=4).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Problems with the rows themselves, one message each. An empty list passes.
pub fn validate(rows: &[Do27Port]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        let name = row.name();
        let mut problem = |message: String| problems.push(format!("{name}: {message}"));
        if !relative_path(&row.krate) || !relative_path(&row.path) {
            problem("`crate` and `path` must be non-empty relative paths without `..`".into());
        }
        if !line_range(&row.lines) {
            problem("`lines` must be `all` or a one-based range `A-B` with A <= B".into());
        }
        if !commit_sha(&row.sha) {
            problem(
                "`sha` must be the DO27 commit the file was read at (9 to 40 lowercase hex digits)".into(),
            );
        }
        if !MARKS.contains(&row.mark.as_str()) {
            problem(format!(
                "`mark` `{}` is not one of {}",
                row.mark,
                MARKS.join(", ")
            ));
        }
        let file_name = row.path.rsplit('/').next().unwrap_or_default();
        if PRIVACY_FILES.iter().any(|p| file_name.starts_with(p)) {
            problem("a DO27 privacy file (P5) never enters Regia, not even as requirements; re-derive the behavior".into());
        }
        match row.class.as_str() {
            "P3" => {}
            "P1" if row.copies_code() => {
                problem("a P1 file is requirements only (it cites behavior findings); use mark `requirements`".into())
            }
            "P1" => {}
            "P2" if row.copies_code() && row.cleared_by.trim().is_empty() => problem(
                "a P2 file may be copied only after its per-file review: name it in `cleared_by` (e.g. `D-02`)".into(),
            ),
            "P2" => {}
            "P4" => problem("a P4 file carries baseline-game values and never enters Regia".into()),
            "P5" => problem("a P5 file is privacy material and never enters Regia".into()),
            "P6" => problem("a P6 third-party crate is a `[[source]]` row (ADR-086), not a `[[do27]]` row".into()),
            other => problem(format!("`class` `{other}` is not one of P1, P2, P3")),
        }
        if row.mark == "adapt" && row.note.trim().is_empty() {
            problem("an `adapt` row names the issue its change fixes in `note`".into());
        }
        if !row.planned.is_empty() && !plan_lane(&row.planned) {
            problem(format!(
                "`planned` `{}` must be the absorb-plan lane id that copies the file (e.g. `F-01`, `N224`, `NL-02`)",
                row.planned
            ));
        }
        if !row.planned.is_empty() {
            if !row.files_to.is_empty() {
                problem(
                    "a row cannot carry both `planned` and `files_to`; delete `planned` when it lands".into(),
                );
            }
            if rows.iter().any(|landed| {
                landed.planned.is_empty()
                    && landed.krate == row.krate
                    && landed.path == row.path
                    && ranges_overlap(&landed.lines, &row.lines)
            }) {
                problem("this source range already has a landed row; delete the obsolete draft".into());
            }
        }
        if row.copies_code() && row.planned.is_empty() && row.files_to.is_empty() {
            problem(
                "an `as-is` or `adapt` row lists the Regia files that hold the code in `files_to`".into(),
            );
        }
        for to in &row.files_to {
            if !relative_path(to) {
                problem(format!(
                    "`files_to` entry `{to}` must be a repository-relative path"
                ));
            }
        }
        if !seen.insert((
            row.krate.clone(),
            row.path.clone(),
            row.lines.clone(),
            row.sha.clone(),
        )) {
            problem("the same file, range and commit is listed twice".into());
        }
    }
    problems
}

fn ranges_overlap(left: &str, right: &str) -> bool {
    if left == "all" || right == "all" {
        return true;
    }
    let range = |s: &str| {
        let (start, end) = s.split_once('-')?;
        Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok()?))
    };
    match (range(left), range(right)) {
        (Some((a, b)), Some((c, d))) => a <= d && c <= b,
        _ => false, // Invalid ranges are diagnosed separately by `line_range`.
    }
}

/// Whether `to` is a data file whose format cannot carry a comment header ([`HEADERLESS_DATA`]).
pub fn headerless_data(to: &str) -> bool {
    to.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.ends_with('/') && HEADERLESS_DATA.contains(&ext.to_ascii_lowercase().as_str())
    })
}

/// Whether `head` (a file's first [`HEADER_LINES`] lines) holds the copy header for `source` at
/// commit `short`.
fn has_header(head: &str, source: &str, short: &str) -> bool {
    head.contains("Direct Order 27") && head.contains(source) && head.contains(short)
}

/// Whether the provenance log has a `do27-owner-code` row naming `source` at commit `short`.
fn provenance_names(log: &str, source: &str, short: &str) -> bool {
    log.lines()
        .any(|line| line.contains("class: do27-owner-code") && line.contains(source) && line.contains(short))
}

/// Problems with the copied files on disk: each `files_to` exists; a copied file names its DO27
/// source and commit in its header and keeps no DO27 identifier. A copied data file that cannot
/// carry a comment ([`headerless_data`]) holds the header in its `<file>.NOTICE` sidecar instead and
/// needs a `docs/provenance.md` row naming the DO27 path and commit (ADR-8497).
pub fn check_files(root: &Path, rows: &[Do27Port]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut provenance: Option<String> = None;
    // A draft row (`planned`) waits for its lane: nothing is on disk yet.
    for row in rows.iter().filter(|r| r.planned.is_empty()) {
        let name = row.name();
        for to in &row.files_to {
            let Ok(bytes) = std::fs::read(root.join(to)) else {
                problems.push(format!("{name}: `{to}` does not exist or is not readable"));
                continue;
            };
            if !row.copies_code() {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            let source = format!("{}/{}", row.krate.trim(), row.path.trim());
            let short = row.sha.get(..9).unwrap_or(&row.sha);
            let header = format!(
                "\"Portions copied from Direct Order 27 ({source}, commit {short}), ADR-8012.\" within its first \
                 {HEADER_LINES} lines"
            );
            let mut checked = vec![(to.clone(), text.into_owned())];
            if headerless_data(to) {
                let sidecar = format!("{to}{NOTICE_SIDECAR}");
                match std::fs::read_to_string(root.join(&sidecar)) {
                    Ok(notice) => {
                        let head = notice.lines().take(HEADER_LINES).collect::<Vec<_>>().join("\n");
                        if !has_header(&head, &source, short) {
                            problems.push(format!("{name}: `{sidecar}` lacks the header ({header})"));
                        }
                        checked.push((sidecar, notice));
                    }
                    Err(_) => problems.push(format!(
                        "{name}: `{to}` is data that cannot carry a comment header: put the header ({header}) in \
                         the sidecar `{sidecar}` (ADR-8497)"
                    )),
                }
                let log = provenance.get_or_insert_with(|| {
                    std::fs::read_to_string(root.join(PROVENANCE_LOG)).unwrap_or_default()
                });
                if !provenance_names(log, &source, short) {
                    problems.push(format!(
                        "{name}: the copied data file `{to}` needs a {PROVENANCE_LOG} row of class \
                         `do27-owner-code` naming `{source}` and commit {short} (ADR-8497)"
                    ));
                }
            } else {
                let head = checked[0]
                    .1
                    .lines()
                    .take(HEADER_LINES)
                    .collect::<Vec<_>>()
                    .join("\n");
                if !has_header(&head, &source, short) {
                    problems.push(format!("{name}: `{to}` lacks its header ({header})"));
                }
            }
            for (file, text) in &checked {
                let lower = text.to_ascii_lowercase();
                for id in DO27_IDENTIFIERS {
                    if lower.contains(id) {
                        problems.push(format!(
                            "{name}: `{file}` keeps the DO27 identifier `{id}`; rename it"
                        ));
                    }
                }
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
[[source]]
project = "Fixture"
url = "https://example.org/fixture"
licence = "MIT"
copyright = "Copyright (c) Fixture"
commit = "abc1234"
files_from = ["a.rs"]
files_to = ["b.rs"]

[[do27]]
crate = "ir-sim"
path = "src/math.rs"
lines = "all"
sha = "f7e8f4740"
mark = "as-is"
class = "P3"
files_to = ["crates/fixture/src/math.rs"]

[[do27]]
crate = "ir-sim"
path = "src/tick.rs"
lines = "1-40"
sha = ""
mark = "copy"
class = "P3"
files_to = ["crates/fixture/src/tick.rs"]

[[do27]]
crate = "ir-sim"
path = "src/rules.rs"
lines = "40-1"
sha = "F7E8F4740"
mark = "requirements"
class = "P4"

[[do27]]
crate = "ir-net"
path = "src/upload.rs"
lines = "all"
sha = "f7e8f4740"
mark = "requirements"
class = "P3"

[[do27]]
crate = "ir-net"
path = "src/reconnect.rs"
lines = "all"
sha = "f7e8f4740"
mark = "adapt"
class = "P2"
files_to = []

[[do27]]
crate = "ir-net"
path = "src/config.rs"
lines = "1-20"
sha = "f7e8f4740"
mark = "as-is"
class = "P1"
files_to = ["crates/fixture/src/config.rs"]
"#;

    fn problems_for(problems: &[String], file: &str) -> Vec<String> {
        problems.iter().filter(|p| p.contains(file)).cloned().collect()
    }

    /// ADR-8012 A-1 acceptance: every `[[do27]]` row carries the DO27 commit, a binding mark and
    /// an allowed class; the notices parser accepts the table next to `[[source]]` rows; copied
    /// files carry the header and no DO27 identifier; the repository file passes.
    #[test]
    fn ported_sources_rows_carry_sha_and_mark() {
        // The notices parser (the `[[source]]` reader) must accept the new table.
        let sources = crate::notices::parse_ported(FIXTURE);
        assert!(sources.is_ok(), "notices rejects [[do27]] rows: {sources:?}");
        assert_eq!(sources.map(|s| s.len()).unwrap_or_default(), 1);

        let rows = parse(FIXTURE);
        assert!(rows.is_ok(), "{rows:?}");
        let rows = rows.unwrap_or_default();
        assert_eq!(rows.len(), 6);
        let problems = validate(&rows);

        assert!(problems_for(&problems, "src/math.rs").is_empty(), "{problems:#?}");

        let tick = problems_for(&problems, "src/tick.rs").join("\n");
        assert!(tick.contains("`sha`"), "{tick}");
        assert!(tick.contains("`mark`"), "{tick}");

        let rules = problems_for(&problems, "src/rules.rs").join("\n");
        assert!(rules.contains("`lines`"), "{rules}");
        assert!(rules.contains("`sha`"), "upper-case hex is refused: {rules}");
        assert!(rules.contains("P4"), "{rules}");

        let upload = problems_for(&problems, "src/upload.rs").join("\n");
        assert!(
            upload.contains("privacy"),
            "a P5 file name is refused whatever the class: {upload}"
        );

        let reconnect = problems_for(&problems, "src/reconnect.rs").join("\n");
        assert!(reconnect.contains("`cleared_by`"), "{reconnect}");
        assert!(reconnect.contains("`note`"), "{reconnect}");
        assert!(reconnect.contains("`files_to`"), "{reconnect}");

        let config = problems_for(&problems, "src/config.rs").join("\n");
        assert!(config.contains("P1"), "a P1 file is requirements only: {config}");

        // Copied files on disk: header, DO27 identifiers.
        let dir = regia_test_utils::unique_temp_dir("do27-ports");
        let src = dir.join("crates/fixture/src");
        assert!(std::fs::create_dir_all(&src).is_ok());
        let good = rows[0].clone();
        let mut missing_header = good.clone();
        missing_header.path = "src/clock.rs".into();
        missing_header.files_to = vec!["crates/fixture/src/clock.rs".into()];
        let mut renamed = good.clone();
        renamed.path = "src/hover.rs".into();
        renamed.files_to = vec!["crates/fixture/src/hover.rs".into()];
        let mut absent = good.clone();
        absent.path = "src/gone.rs".into();
        absent.files_to = vec!["crates/fixture/src/gone.rs".into()];
        let header =
            "// Portions copied from Direct Order 27 (ir-sim/src/math.rs, commit f7e8f4740), ADR-8012.\n";
        assert!(std::fs::write(src.join("math.rs"), format!("{header}pub fn f() {{}}\n")).is_ok());
        assert!(std::fs::write(src.join("clock.rs"), "pub fn f() {}\n").is_ok());
        let hover_header =
            "// Portions copied from Direct Order 27 (ir-sim/src/hover.rs, commit f7e8f4740), ADR-8012.\n";
        assert!(
            std::fs::write(
                src.join("hover.rs"),
                format!("{hover_header}use ironreach_sim::X;\n")
            )
            .is_ok()
        );
        let on_disk = check_files(&dir, &[good, missing_header, renamed, absent]);
        assert!(problems_for(&on_disk, "src/math.rs").is_empty(), "{on_disk:#?}");
        assert!(
            problems_for(&on_disk, "src/clock.rs")
                .join("\n")
                .contains("header"),
            "{on_disk:#?}"
        );
        assert!(
            problems_for(&on_disk, "src/hover.rs")
                .join("\n")
                .contains("ironreach"),
            "{on_disk:#?}"
        );
        assert!(
            problems_for(&on_disk, "src/gone.rs")
                .join("\n")
                .contains("does not exist"),
            "{on_disk:#?}"
        );
        let _ = std::fs::remove_dir_all(&dir);

        // The repository's own file parses in both readers and every row passes.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        assert!(crate::notices::load_ported(&root).is_ok());
        let repo = load(&root);
        assert!(repo.is_ok(), "{repo:?}");
        let repo = repo.unwrap_or_default();
        let mut problems = validate(&repo);
        problems.extend(check_files(&root, &repo));
        assert!(problems.is_empty(), "{problems:#?}");
    }

    /// Every non-tunnel DO27 file the absorb plan copies (marks *as-is* or *adapt*), with the plan
    /// lane that copies it (lane D-02, `docs/do27-port-review.md`).
    const PLAN_COPIES: &[(&str, &str, &str)] = &[
        ("ir-sim", "src/math.rs", "F-01"),
        ("ir-sim", "src/tick.rs", "F-01"),
        ("ir-sim", "clippy.toml", "F-01"),
        ("ir-sim", "src/trace_harness.rs", "F-02"),
        ("ir-sim", "src/rate_gate.rs", "F-02"),
        ("ir-sim", "src/desync_hash.rs", "F-04"),
        ("ir-sim", "src/state/desync_fold.rs", "F-04"),
        ("ir-sim", "src/desync_hash/tests.rs", "F-04"),
        ("ir-sim", "src/entity_ids.rs", "F-06"),
        ("ir-sim", "src/state.rs", "F-06"),
        ("ir-sim", "src/soak.rs", "F-07"),
        ("shell-regia/game", "src/front_net.rs", "N224"),
        ("ir-net", "src/reconnect.rs", "N225"),
        ("ir-core", "src/desync_replay.rs", "N227"),
        ("ir-net", "src/host_check.rs", "N229"),
        ("ir-net", "src/port_check.rs", "N229"),
        ("ir-net", "src/rehost_session.rs", "N301"),
        ("ir-net", "src/rehost_session/tests.rs", "N301"),
        ("ir-net", "src/live_rehost.rs", "N301"),
        ("ir-net", "src/live_rehost/tests.rs", "N301"),
        ("ir-net", "src/lockstep_migration.rs", "N301"),
        ("ir-core", "src/replay_library.rs", "NL-02"),
        ("ir-log", "src/lib.rs", "NL-07"),
        ("ir-log", "src/devenv.rs", "NL-07"),
        ("ir-log", "src/session.rs", "NL-07"),
        ("ir-sim", "src/pathing/grid.rs", "S-01"),
        ("ir-sim", "src/pathing/search.rs", "S-02"),
        ("ir-sim", "src/pathing/mod.rs", "S-02"),
        ("ir-sim", "src/pathing/tests.rs", "S-03"),
        ("ir-sim", "src/pathing/avoid.rs", "S-04"),
        ("ir-sim", "src/pathing/crowd.rs", "S-05"),
        ("ir-sim", "src/pathing/navmesh.rs", "S-06"),
        ("ir-sim", "src/hover.rs", "S-07"),
        ("ir-sim", "src/site_physics.rs", "S-07"),
        ("ir-sim", "src/lowg.rs", "S-07"),
        ("ir-sim", "src/ballistic.rs", "S-07"),
        ("ir-sim", "src/handling.rs", "S-08"),
        ("ir-sim", "tests/handling_audit.rs", "S-08"),
        ("ir-sim", "src/ai/mod.rs", "S-09"),
        ("ir-sim", "src/ai/utility.rs", "S-09"),
        ("ir-sim", "tests/ai_core.rs", "S-09"),
        ("ir-sim", "src/ai/influence.rs", "S-10"),
        ("ir-sim", "src/ai/perception.rs", "S-10"),
        ("ir-sim", "src/ai/tactics.rs", "S-11"),
        ("ir-sim", "src/ai/difficulty.rs", "S-11"),
        ("ir-sim", "src/ai/aim.rs", "S-11"),
        ("ir-sim", "tests/bot_ai.rs", "S-11"),
        ("ir-sim", "src/ai/motor.rs", "S-12"),
        ("ir-sim", "src/ai/combat.rs", "S-12"),
        ("ir-sim", "tests/ai_combat.rs", "S-12"),
        ("ir-sim", "src/ai/wingman.rs", "S-13"),
        ("ir-sim", "src/ai/mode_bots.rs", "S-13"),
        ("ir-sim", "src/modes/mod.rs", "S-14"),
        ("ir-sim", "src/modes/ctf.rs", "S-14"),
        ("ir-sim", "src/modes/koth.rs", "S-14"),
        ("ir-sim", "src/match_flow.rs", "S-14"),
        ("ir-sim", "src/series.rs", "S-15"),
        ("ir-sim", "src/map_vote.rs", "S-15"),
        ("ir-sim", "src/win_condition.rs", "S-15"),
        ("ir-sim", "src/end_stats.rs", "S-15"),
        ("ir-sim", "src/spectator.rs", "S-15"),
        ("ir-sim", "src/fog.rs", "S-16"),
        ("ir-sim", "src/gait.rs", "S-17"),
        ("ir-sim", "src/harness.rs", "S-18"),
        ("ir-sim", "src/profile_alpha.rs", "S-18"),
        ("shell-regia/game", "src/present.rs", "P-01"),
        ("shell-regia/game", "src/world/coords.rs", "P-02"),
        ("shell-regia/game", "src/front/gpu.rs", "P-05"),
        ("shell-regia/game", "src/fast_quit.rs", "P-07"),
        ("shell-regia/game", "src/single_instance.rs", "P-07"),
        ("shell-regia/game", "src/stack_dump.rs", "P-08"),
        ("shell-regia/game", "src/winplat.rs", "P-08"),
        ("shell-regia/game", "src/world/backdrop.rs", "P-09"),
        ("shell-regia/game", "src/world/foot_plant.rs", "P-10"),
        ("shell-regia/game", "src/world/rig.rs", "P-12"),
        // P-13 `observer_cam.rs` is not in this copy list. The plan marked it adapt, but the
        // landed spectator modes copy no lines (review R01). `do27_manifest_covers_the_absorb_plan`
        // requires a finished requirements row instead of a false `copies_code` row.
        ("shell-regia/game", "src/photo_mode.rs", "P-14"),
        ("shell-regia/game", "src/world/meshkit.rs", "P-18"),
        ("shell-regia/game", "src/world/holo.rs", "P-18"),
        ("shell-regia/game", "src/world/decimate.rs", "P-19"),
        ("shell-regia/game", "src/world/cache_cap.rs", "P-20"),
        ("shell-regia/game", "src/platform.rs", "P-21"),
        ("ir-content", "src/heightfield_json.rs", "C-20"),
        ("ir-content", "src/heightfield_png.rs", "C-20"),
        ("ir-content", "src/provenance.rs", "C-24"),
    ];

    /// DO27 files the plan names that are P4 (baseline-game values) or P5 (privacy): no row may name them.
    const NEVER: &[(&str, &str)] = &[
        // ADR-8164 / ADR-8012 A-8: Regia records its own neutral hull, never this fixture.
        ("spec", "fixtures/warhull/libm-1000.txt"),
        ("ir-net", "src/upload.rs"),
        ("ir-net", "src/secret.rs"),
        ("ir-net", "src/reserved_names.rs"),
        ("ir-net", "src/reserved_names.txt"),
        ("ir-net", "tests/upload_shutdown.rs"),
        ("ir-net", "tests/scrub_privacy.rs"),
        ("ir-net", "src/lobby_room.rs"),
    ];

    /// ADR-8041: a draft row names its plan lane, needs no `files_to` and skips the disk check until
    /// the lane copies the file; a bad lane id is refused; a landed row is checked on disk as before.
    #[test]
    fn planned_rows_wait_for_their_lane() {
        let text = r#"
[[do27]]
crate = "ir-sim"
path = "src/math.rs"
lines = "all"
sha = "d6ed3767703b1cabb12348cb3deb97ac27ac4fcb"
mark = "as-is"
class = "P2"
cleared_by = "D-02"
planned = "F-01"

[[do27]]
crate = "ir-sim"
path = "src/fog.rs"
lines = "all"
sha = "d6ed3767703b1cabb12348cb3deb97ac27ac4fcb"
mark = "adapt"
class = "P3"
note = "sight table becomes data"
planned = "fog lane"
files_to = ["crates/fixture/src/fog.rs"]

[[do27]]
crate = "ir-sim"
path = "src/gait.rs"
lines = "all"
sha = "d6ed3767703b1cabb12348cb3deb97ac27ac4fcb"
mark = "adapt"
class = "P2"
cleared_by = "D-02"
note = "tripod stepper only"
files_to = ["crates/fixture/src/gait.rs"]
"#;
        let rows = parse(text);
        assert!(rows.is_ok(), "{rows:?}");
        let rows = rows.unwrap_or_default();
        assert_eq!(rows.len(), 3);
        let problems = validate(&rows);
        assert!(problems_for(&problems, "src/math.rs").is_empty(), "{problems:#?}");
        assert!(problems_for(&problems, "src/gait.rs").is_empty(), "{problems:#?}");
        let fog = problems_for(&problems, "src/fog.rs").join("\n");
        assert!(
            fog.contains("`planned`"),
            "a lane id is a plan id like F-01: {fog}"
        );

        // Nothing is on disk: the draft rows wait, the landed row is checked.
        let dir = regia_test_utils::unique_temp_dir("do27-planned");
        let on_disk = check_files(&dir, &rows);
        assert!(problems_for(&on_disk, "src/math.rs").is_empty(), "{on_disk:#?}");
        assert!(problems_for(&on_disk, "src/fog.rs").is_empty(), "{on_disk:#?}");
        assert!(
            problems_for(&on_disk, "src/gait.rs")
                .join("\n")
                .contains("does not exist"),
            "{on_disk:#?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whether `note` names the plan lane `lane` as a whole id (`C-20` in `C-20/ADR-8063`, not in
    /// `C-200` or `NC-20`).
    fn names_lane(note: &str, lane: &str) -> bool {
        note.match_indices(lane).any(|(at, _)| {
            let before = note[..at].chars().next_back();
            let after = note[at + lane.len()..].chars().next();
            !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-')
                && !after.is_some_and(|c| c.is_ascii_digit())
        })
    }

    /// Whether the manifest records the plan row `lane` for `krate`/`path`: a copy row (a draft of
    /// that lane, or landed), or a landed `requirements` row whose `note` names the lane, for a
    /// lane that chose to implement the file from requirements instead of copying it (ADR-8497;
    /// C-20 under ADR-8063).
    fn covers_plan_row(rows: &[Do27Port], krate: &str, path: &str, lane: &str) -> bool {
        rows.iter().any(|r| {
            r.krate == krate
                && r.path == path
                && if r.copies_code() {
                    r.planned == lane || r.planned.is_empty()
                } else {
                    r.planned.is_empty() && !r.files_to.is_empty() && names_lane(&r.note, lane)
                }
        })
    }

    /// ADR-8497: a landed requirements row covers its plan row only when it names the plan lane;
    /// a draft requirements row, a row for another lane or another file does not.
    #[test]
    fn requirements_rows_cover_their_plan_row_by_lane() {
        let row = |mark: &str, note: &str, planned: &str, files_to: &[&str]| Do27Port {
            krate: "ir-content".into(),
            path: "src/heightfield_json.rs".into(),
            lines: "1-386".into(),
            sha: "d6ed3767703b1cabb12348cb3deb97ac27ac4fcb".into(),
            mark: mark.into(),
            class: "P3".into(),
            cleared_by: String::new(),
            note: note.into(),
            files_to: files_to.iter().map(|s| (*s).to_string()).collect(),
            planned: planned.into(),
        };
        let to = &["crates/regia_terrain/src/source.rs"];
        let covers = |r: Do27Port| covers_plan_row(&[r], "ir-content", "src/heightfield_json.rs", "C-20");
        assert!(covers(row(
            "requirements",
            "regia-absorb-1; C-20/ADR-8063: original Rust",
            "",
            to
        )));
        assert!(covers(row("requirements", "plan row C-20", "", to)));
        assert!(
            !covers(row("requirements", "C-200 and NC-20 only", "", to)),
            "whole lane ids only"
        );
        assert!(
            !covers(row("requirements", "original Rust", "", to)),
            "the note must name the lane"
        );
        assert!(
            !covers(row("requirements", "C-20", "C-20", &[])),
            "a draft is not a landed record"
        );
        assert!(
            !covers(row("requirements", "C-20", "", &[])),
            "a landed record names its files"
        );
        assert!(
            covers(row("adapt", "", "C-20", &[])),
            "the copy draft of the lane"
        );
        assert!(covers(row("as-is", "", "", to)), "a landed copy row");
        assert!(!covers(row("adapt", "", "S-02", &[])), "another lane's draft");
        assert!(
            !covers_plan_row(
                &[row("requirements", "C-20", "", to)],
                "ir-content",
                "src/heightfield_png.rs",
                "C-20"
            ),
            "another file"
        );
    }

    /// ADR-8497: a copied data file that cannot carry a comment holds its header in a sidecar
    /// `<file>.NOTICE` and needs a `docs/provenance.md` row naming the DO27 path and commit; the
    /// sidecar is accepted only for data formats, and DO27 identifiers are refused in both files.
    #[test]
    fn headerless_data_needs_a_notice_sidecar_and_a_provenance_row() {
        assert!(headerless_data("crates/x/tests/fixtures/mesh.json"));
        assert!(headerless_data("crates/x/tests/fixtures/vec.BIN"));
        assert!(!headerless_data("crates/x/src/lib.rs"));
        assert!(!headerless_data("crates/x/tests/fixtures/README.md"));
        assert!(!headerless_data("crates/x/tests/fixtures/.json"));

        let row = |path: &str, to: &str| Do27Port {
            krate: "spec".into(),
            path: path.into(),
            lines: "all".into(),
            sha: "c06c17084218b7bd2fa80b5a15492d27483069d9".into(),
            mark: "adapt".into(),
            class: "P3".into(),
            cleared_by: String::new(),
            note: "one column replaced".into(),
            files_to: vec![to.into()],
            planned: String::new(),
        };
        let dir = regia_test_utils::unique_temp_dir("do27-sidecar");
        let fixtures = dir.join("fixtures");
        assert!(std::fs::create_dir_all(&fixtures).is_ok());
        assert!(std::fs::create_dir_all(dir.join("docs")).is_ok());
        let notice = |source: &str| {
            format!("Portions copied from Direct Order 27 ({source}, commit c06c17084), ADR-8012.\n")
        };
        let write = |name: &str, text: &str| std::fs::write(fixtures.join(name), text).is_ok();
        // good.json: sidecar and provenance row. bare.json: no sidecar. blank.json: a sidecar
        // without the header. orphan.json: sidecar, no provenance row. named.json: the sidecar
        // keeps a DO27 identifier. code.rs: the sidecar is no excuse for source code.
        assert!(write("good.json", "{\"a\": 1}\n"));
        assert!(write("good.json.NOTICE", &notice("spec/maps/good.json")));
        assert!(write("bare.json", "{}\n"));
        assert!(write("blank.json", "{}\n"));
        assert!(write("blank.json.NOTICE", "Copied from somewhere.\n"));
        assert!(write("orphan.json", "{}\n"));
        assert!(write("orphan.json.NOTICE", &notice("spec/maps/orphan.json")));
        assert!(write("named.json", "{}\n"));
        assert!(write(
            "named.json.NOTICE",
            &format!("{}The Warhull mesh.\n", notice("spec/maps/named.json"))
        ));
        assert!(write("code.rs", "pub fn f() {}\n"));
        assert!(write("code.rs.NOTICE", &notice("spec/maps/code.rs")));
        let log = ["good.json", "bare.json", "blank.json", "named.json", "code.rs"]
            .iter()
            .map(|f| {
                format!(
                    "| Fixture {f} | data | [class: do27-owner-code; approval: ADR-8012] Direct Order 27 \
                     `spec/maps/{f}` all, commit c06c17084218b7bd2fa80b5a15492d27483069d9, adapt | |\n"
                )
            })
            .collect::<String>();
        assert!(std::fs::write(dir.join("docs/provenance.md"), log).is_ok());
        let rows: Vec<_> = [
            "good.json",
            "bare.json",
            "blank.json",
            "orphan.json",
            "named.json",
            "code.rs",
        ]
        .iter()
        .map(|f| row(&format!("maps/{f}"), &format!("fixtures/{f}")))
        .collect();
        let problems = check_files(&dir, &rows);
        assert!(
            problems_for(&problems, "maps/good.json").is_empty(),
            "{problems:#?}"
        );
        let bare = problems_for(&problems, "maps/bare.json").join("\n");
        assert!(bare.contains("fixtures/bare.json.NOTICE"), "{bare}");
        let blank = problems_for(&problems, "maps/blank.json").join("\n");
        assert!(blank.contains("lacks the header"), "{blank}");
        let orphan = problems_for(&problems, "maps/orphan.json").join("\n");
        assert!(orphan.contains("docs/provenance.md"), "{orphan}");
        assert!(!orphan.contains("header"), "{orphan}");
        let named = problems_for(&problems, "maps/named.json").join("\n");
        assert!(named.contains("`warhull`"), "{named}");
        let code = problems_for(&problems, "maps/code.rs").join("\n");
        assert!(code.contains("lacks its header"), "{code}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Lane D-02 acceptance: the draft manifest has a row for every file the plan copies, each row
    /// carries the DO27 commit, a mark, a class and the plan lane, and no row names a P4/P5 file.
    #[test]
    fn do27_manifest_covers_the_absorb_plan() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let rows = load(&root);
        assert!(rows.is_ok(), "{rows:?}");
        let rows = rows.unwrap_or_default();
        assert!(!rows.is_empty(), "docs/ported-sources.toml has no [[do27]] rows");
        let missing: Vec<String> = PLAN_COPIES
            .iter()
            .filter(|(krate, path, lane)| !covers_plan_row(&rows, krate, path, lane))
            .map(|(krate, path, lane)| format!("{lane} {krate}/{path}"))
            .collect();
        assert!(
            missing.is_empty(),
            "plan copies without a [[do27]] copy row: {missing:#?}"
        );
        // S-18 implemented the arena from requirements (ADR-8151); a stale copy draft
        // must not satisfy this requirement or reclassify that implementation.
        let arena: Vec<_> = rows
            .iter()
            .filter(|r| r.krate == "ir-sim" && r.path == "src/bin/ai_arena.rs")
            .collect();
        assert_eq!(
            arena.len(),
            1,
            "the arena has exactly one landed requirements record"
        );
        assert_eq!(arena[0].mark, "requirements");
        assert!(arena[0].planned.is_empty());
        assert!(
            arena[0]
                .files_to
                .iter()
                .any(|p| p == "crates/regia_detsim/src/arena.rs")
        );
        // R01: P-13 is implemented and copies no lines. A copies_code row would claim a port
        // that did not happen. The finished row is requirements, class P3, at the absorb-1 pin,
        // with `planned` deleted and the spectator sources named.
        let observer = rows
            .iter()
            .find(|row| row.krate == "shell-regia/game" && row.path == "src/observer_cam.rs");
        let Some(observer) = observer else {
            panic!("P-13 observer_cam.rs has no [[do27]] row");
        };
        assert_eq!(observer.mark, "requirements", "P-13 mark");
        assert_eq!(observer.class, "P3", "P-13 class");
        assert_eq!(
            observer.sha, "d6ed3767703b1cabb12348cb3deb97ac27ac4fcb",
            "P-13 sha"
        );
        assert!(
            observer.planned.is_empty(),
            "landed P-13 must not stay planned: {}",
            observer.planned
        );
        assert!(
            observer
                .files_to
                .iter()
                .any(|path| path == "crates/regia_framework/src/spectator.rs"),
            "P-13 files_to: {:?}",
            observer.files_to
        );
        assert!(
            !rows.iter().any(|row| {
                row.krate == "shell-regia/game" && row.path == "src/observer_cam.rs" && row.copies_code()
            }),
            "observer_cam.rs is requirements, not a copied file"
        );
        for row in &rows {
            assert!(commit_sha(&row.sha), "{}: sha", row.name());
            assert!(MARKS.contains(&row.mark.as_str()), "{}: mark", row.name());
            assert!(
                ["P1", "P2", "P3"].contains(&row.class.as_str()),
                "{}: class",
                row.name()
            );
            assert!(
                !NEVER.iter().any(|(k, p)| row.krate == *k && row.path == *p),
                "{}: a P4/P5 file has a row",
                row.name()
            );
        }
    }

    #[test]
    fn planned_rows_cannot_shadow_landed_rows_or_name_destinations() {
        let text = r#"
[[do27]]
crate = "ir-sim"
path = "src/example.rs"
lines = "all"
sha = "d6ed3767703b1cabb12348cb3deb97ac27ac4fcb"
mark = "as-is"
class = "P3"
planned = "S-18"

[[do27]]
crate = "ir-sim"
path = "src/example.rs"
lines = "1-20"
sha = "d6ed3767703b1cabb12348cb3deb97ac27ac4fcb"
mark = "requirements"
class = "P3"
files_to = ["crates/fixture/src/example.rs"]
"#;
        let rows = parse(text).expect("fixture rows");
        let problems = validate(&rows).join("\n");
        assert!(problems.contains("already has a landed row"), "{problems}");
        let mut reversed = rows.clone();
        reversed.reverse();
        assert!(
            validate(&reversed)
                .join("\n")
                .contains("already has a landed row")
        );
        let mut disjoint = rows.clone();
        disjoint[0].lines = "21-40".into();
        assert!(
            validate(&disjoint).is_empty(),
            "unlanded disjoint ranges remain planned"
        );
        disjoint[0].lines = "20-40".into();
        assert!(
            validate(&disjoint)
                .join("\n")
                .contains("already has a landed row")
        );
        let mut draft = rows[0].clone();
        draft.files_to.push("crates/fixture/src/example.rs".into());
        let problems = validate(&[draft]).join("\n");
        assert!(problems.contains("both `planned` and `files_to`"), "{problems}");
    }
}
