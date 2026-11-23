// SPDX-License-Identifier: MPL-2.0
//! `api-snapshot`: the public API of the stable crates against the snapshots in
//! `docs/api-snapshots/` (spec 57 § 11.1, `docs/api-snapshots/README.md`).
//!
//! Without flags it scans the crates listed in [`STABLE_CRATES`] and fails on an item that was
//! removed or whose signature changed. A new item only prints a note: it breaks nobody, and
//! lanes that merge in parallel would otherwise collide on every addition. `--update` rewrites
//! the snapshots; a removed or changed item also needs `--breaking "<why>"`, which appends the
//! change to `docs/api-snapshots/BREAKING.md`.
//!
//! A snapshot also records `#[deprecated(since, note)]` per item as a `<item> deprecated` line.
//! A public item removed while the old snapshot had it deprecated is a normal retirement; one
//! removed without that is `removed_without_deprecation`, a warning until `freeze = true` is set in
//! `docs/api-snapshots/policy.toml`, an error that no `--breaking` reason can accept after it
//! (spec 57 § 11.1, ADR-2527). `--rotate <version>` moves a released version's `BREAKING.md` rows
//! into `docs/api-snapshots/breaking/<version>.md`.

pub mod scan;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use scan::{ApiLine, DEPRECATED_SUFFIX, lib_rs, scan_crate_with};

/// Where the snapshots live, relative to the workspace root.
pub const SNAPSHOT_DIR: &str = "docs/api-snapshots";

/// The crates under the semver policy of spec 57 § 11.1: the umbrella crate and every crate
/// `docs/api-review.md` reviews.
pub const STABLE_CRATES: &[&str] = &[
    "regia",
    "regia_2d",
    "regia_agent",
    "regia_ai",
    "regia_animation",
    "regia_app",
    "regia_asset",
    "regia_audio",
    "regia_core",
    "regia_crash",
    "regia_ecs",
    "regia_framework",
    "regia_input",
    "regia_matgraph",
    "regia_media",
    "regia_nav",
    "regia_net",
    "regia_physics",
    "regia_pick",
    "regia_platform",
    "regia_procgen",
    "regia_quick",
    "regia_render",
    "regia_scene",
    "regia_script_lua",
    "regia_steam",
    "regia_telemetry",
    "regia_terrain",
    "regia_ui",
    "regia_vfx",
    "regia_world",
    "regia_xr",
];

/// Modules that moved out of a stable crate into their own workspace crate behind a whole-crate
/// re-export: `(crate, sibling)`. The scan reads the sibling in place of `pub use <sibling> as <name>;`
/// in that crate, so the move lists the same lines (ADR-8877). Every other whole-crate re-export (the
/// `regia` facade's) stays one `pub use` line.
pub const MOVED_MODULES: &[(&str, &str)] = &[("regia_nav", "regia_nav_det")];

/// Module-scoped surfaces of crates that are otherwise internal: `(crate, module)`. The snapshot
/// is `<crate>.<module>.txt` and lists the items whose public path starts with the module.
pub const SCOPED_SURFACES: &[(&str, &str)] = &[("regia_editor", "extension")];

/// Command-line options of `cargo xtask api-snapshot`.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct Args {
    /// Rewrite the snapshots from the sources instead of checking them.
    #[arg(long)]
    pub update: bool,
    /// With `--update`: why removed or changed items are accepted (logged in `BREAKING.md`).
    #[arg(long, requires = "update")]
    pub breaking: Option<String>,
    /// Only this crate (or a module-scoped surface such as `regia_editor.extension`).
    #[arg(long = "crate")]
    pub only: Option<String>,
    /// Move the `BREAKING.md` rows of this released version into `breaking/<version>.md`.
    #[arg(long, conflicts_with_all = ["update", "breaking"])]
    pub rotate: Option<String>,
}

/// One item whose signature moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedItem {
    /// Its public path.
    pub key: String,
    /// The signatures the snapshot has and the sources no longer do.
    pub was: Vec<String>,
    /// The signatures the sources have and the snapshot does not.
    pub now: Vec<String>,
}

/// What differs between a snapshot and the sources.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApiDiff {
    /// Public paths that are gone, with their old signatures.
    pub removed: Vec<ApiLine>,
    /// Paths that exist on both sides with a different signature.
    pub changed: Vec<ChangedItem>,
    /// Paths that are new.
    pub added: Vec<ApiLine>,
    /// Paths that are gone and were `#[deprecated]` in the old snapshot: a normal retirement.
    pub retired: Vec<ApiLine>,
    /// The deprecation lines differ (an item was deprecated, or lost its deprecation).
    pub deprecations_changed: bool,
}

impl ApiDiff {
    /// Whether a user of the old API can stop compiling.
    #[must_use]
    pub fn breaking(&self) -> bool {
        !self.removed.is_empty() || !self.changed.is_empty()
    }

    /// Nothing differs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.breaking() && self.added.is_empty() && self.retired.is_empty() && !self.deprecations_changed
    }

    /// The removed public paths that were not deprecated in the old snapshot. An item that takes
    /// its members with it counts once: only the outermost removed path is named.
    #[must_use]
    pub fn removed_without_deprecation(&self) -> Vec<&str> {
        let keys: BTreeSet<&str> = self.removed.iter().map(|l| l.key.as_str()).collect();
        keys.iter()
            .copied()
            .filter(|key| !keys.iter().any(|other| other != key && covers(other, key)))
            .collect()
    }
}

/// Whether `outer` is `inner` or an item that contains it (`A` covers `A::new`, `A.x`, `A impl Clone`).
fn covers(outer: &str, inner: &str) -> bool {
    inner.strip_prefix(outer).is_some_and(|rest| {
        rest.is_empty() || rest.starts_with("::") || rest.starts_with('.') || rest.starts_with(' ')
    })
}

fn is_deprecation(line: &ApiLine) -> bool {
    line.key.ends_with(DEPRECATED_SUFFIX)
}

fn by_key(lines: &[ApiLine]) -> BTreeMap<&str, BTreeSet<&str>> {
    let mut map: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for line in lines {
        map.entry(&line.key).or_default().insert(&line.signature);
    }
    map
}

/// Compares a snapshot with the current sources. A key keeps its place when one of several
/// signatures remains: the missing ones count as changed.
#[must_use]
pub fn diff(old: &[ApiLine], new: &[ApiLine]) -> ApiDiff {
    let old_deprecated: Vec<&ApiLine> = old.iter().filter(|l| is_deprecation(l)).collect();
    let new_deprecated: Vec<&ApiLine> = new.iter().filter(|l| is_deprecation(l)).collect();
    let items = |lines: &[ApiLine]| -> Vec<ApiLine> {
        lines.iter().filter(|l| !is_deprecation(l)).cloned().collect()
    };
    let mut out = diff_items(&items(old), &items(new));
    out.deprecations_changed = old_deprecated != new_deprecated;
    // A removal is a retirement when the old snapshot had the item or an outer item deprecated.
    let deprecated_keys: Vec<&str> = old_deprecated
        .iter()
        .filter_map(|l| l.key.strip_suffix(DEPRECATED_SUFFIX))
        .collect();
    let (retired, removed): (Vec<ApiLine>, Vec<ApiLine>) = std::mem::take(&mut out.removed)
        .into_iter()
        .partition(|l| deprecated_keys.iter().any(|d| covers(d, &l.key)));
    out.removed = removed;
    out.retired = retired;
    out
}

fn diff_items(old: &[ApiLine], new: &[ApiLine]) -> ApiDiff {
    let (before, after) = (by_key(old), by_key(new));
    let mut out = ApiDiff::default();
    for (key, was) in &before {
        let Some(now) = after.get(key) else {
            out.removed.extend(was.iter().map(|signature| ApiLine {
                key: (*key).to_owned(),
                signature: (*signature).to_owned(),
            }));
            continue;
        };
        let gone: Vec<String> = was.difference(now).map(|s| (*s).to_owned()).collect();
        let extra: Vec<String> = now.difference(was).map(|s| (*s).to_owned()).collect();
        if !gone.is_empty() {
            out.changed.push(ChangedItem {
                key: (*key).to_owned(),
                was: gone,
                now: extra,
            });
        } else {
            out.added.extend(extra.into_iter().map(|signature| ApiLine {
                key: (*key).to_owned(),
                signature,
            }));
        }
    }
    for (key, now) in &after {
        if !before.contains_key(key) {
            out.added.extend(now.iter().map(|signature| ApiLine {
                key: (*key).to_owned(),
                signature: (*signature).to_owned(),
            }));
        }
    }
    out
}

/// The header of a snapshot file; lines starting with `#` are not items.
const HEADER: &str =
    "# Generated by `cargo xtask api-snapshot --update`; do not edit (docs/api-snapshots/README.md).\n";

/// A snapshot's text: the header, then `key<TAB>signature`, sorted.
#[must_use]
pub fn render(lines: &[ApiLine]) -> String {
    let mut text = String::from(HEADER);
    for line in lines {
        text.push_str(&line.key);
        text.push('\t');
        text.push_str(&line.signature);
        text.push('\n');
    }
    text
}

/// Reads a snapshot's text back. A line without a tab is a corrupt file (a hand edit or a merge
/// left over), not an empty API.
pub fn parse(text: &str) -> Result<Vec<ApiLine>> {
    let mut lines = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((key, signature)) = line.split_once('\t') else {
            bail!("line {}: no tab between key and signature", number + 1);
        };
        lines.push(ApiLine {
            key: key.to_owned(),
            signature: signature.to_owned(),
        });
    }
    Ok(lines)
}

/// The report of one crate's diff.
#[must_use]
pub fn report(name: &str, diff: &ApiDiff) -> String {
    let mut out = String::new();
    for line in &diff.removed {
        let _ = writeln!(out, "{name}: removed {}\n    was: {}", line.key, line.signature);
    }
    for item in &diff.changed {
        let _ = writeln!(out, "{name}: changed {}", item.key);
        for was in &item.was {
            let _ = writeln!(out, "    was: {was}");
        }
        for now in &item.now {
            let _ = writeln!(out, "    now: {now}");
        }
    }
    out
}

fn crate_dir(name: &str) -> String {
    format!("crates/{name}")
}

/// One snapshot file: a whole crate, or one module of a crate that is otherwise internal.
struct Surface {
    /// The snapshot's name and the `--crate` selector: `regia_app`, `regia_editor.extension`.
    id: String,
    krate: &'static str,
    module: Option<&'static str>,
}

fn snapshot_path(root: &Path, id: &str) -> std::path::PathBuf {
    root.join(SNAPSHOT_DIR).join(format!("{id}.txt"))
}

fn selected(args: &Args) -> Result<Vec<Surface>> {
    let mut all: Vec<Surface> = STABLE_CRATES
        .iter()
        .map(|name| Surface {
            id: (*name).to_owned(),
            krate: name,
            module: None,
        })
        .collect();
    all.extend(SCOPED_SURFACES.iter().map(|(krate, module)| Surface {
        id: format!("{krate}.{module}"),
        krate,
        module: Some(module),
    }));
    let Some(only) = &args.only else { return Ok(all) };
    all.retain(|s| s.id == *only || (s.krate == only && s.module.is_none()));
    if all.is_empty() {
        bail!("{only} is not a stable crate or surface (see xtask/src/api_snapshot.rs)");
    }
    Ok(all)
}

/// The scan of one surface: the whole crate, or the items under its module.
fn scan_surface(root: &Path, surface: &Surface) -> Result<Vec<ApiLine>> {
    let moved: Vec<&str> = MOVED_MODULES
        .iter()
        .filter(|(krate, _)| *krate == surface.krate)
        .map(|(_, sibling)| *sibling)
        .collect();
    let all = scan_crate_with(&lib_rs(root, &crate_dir(surface.krate)), &moved)
        .with_context(|| format!("scanning {}", surface.id))?;
    Ok(match surface.module {
        None => all,
        Some(module) => all.into_iter().filter(|l| covers(module, &l.key)).collect(),
    })
}

/// `docs/api-snapshots/policy.toml`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// After the freeze a public item may only leave through a deprecation window; no
    /// `--breaking` reason accepts `removed_without_deprecation`.
    pub freeze: bool,
}

/// Reads the policy; a missing file is the pre-1.0 default (`freeze = false`).
pub fn read_policy(root: &Path) -> Result<Policy> {
    let path = root.join(SNAPSHOT_DIR).join("policy.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Policy::default()),
        Err(err) => return Err(err).with_context(|| path.display().to_string()),
    };
    let table: toml::Table = toml::from_str(&text).with_context(|| path.display().to_string())?;
    let freeze = match table.get("freeze") {
        None => false,
        Some(value) => value
            .as_bool()
            .with_context(|| format!("{}: freeze must be true or false", path.display()))?,
    };
    Ok(Policy { freeze })
}

/// The lines naming `removed_without_deprecation` for one surface: warnings before the freeze,
/// errors after it.
#[must_use]
pub fn deprecation_findings(name: &str, diff: &ApiDiff, policy: Policy) -> String {
    let level = if policy.freeze { "error" } else { "warning" };
    let mut out = String::new();
    for key in diff.removed_without_deprecation() {
        let _ = writeln!(out, "{level}: {name}: removed_without_deprecation {key}");
    }
    out
}

/// One `BREAKING.md` row per removed or changed item.
fn breaking_rows(name: &str, diff: &ApiDiff, version: &str, why: &str) -> String {
    let mut out = String::new();
    let mut row = |kind: &str, key: &str| {
        let key = key.replace('|', "\\|");
        let _ = writeln!(out, "| {version} | {name} | {kind} | `{key}` | {why} |");
    };
    for line in &diff.removed {
        row("removed", &line.key);
    }
    for item in &diff.changed {
        row("changed", &item.key);
    }
    out
}

fn workspace_version(root: &Path) -> Result<String> {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).context("reading Cargo.toml")?;
    let value: toml::Table = toml::from_str(&text).context("parsing Cargo.toml")?;
    value
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .context("no [workspace.package] version")
}

/// Runs the check, the update or the rotation.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if let Some(version) = &args.rotate {
        let moved = rotate(root, version)?;
        println!("api-snapshot: {moved} row(s) of {version} moved to {SNAPSHOT_DIR}/breaking/{version}.md");
        return Ok(ExitCode::SUCCESS);
    }
    let policy = read_policy(root)?;
    let mut breaking = false;
    let mut frozen = false;
    let mut additions = 0;
    let mut rows = String::new();
    let mut writes = Vec::new();
    for surface in selected(args)? {
        let name = surface.id.as_str();
        let current = scan_surface(root, &surface)?;
        let path = snapshot_path(root, name);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err).with_context(|| path.display().to_string()),
        };
        let Some(text) = text else {
            if !args.update {
                println!("{name}: no snapshot at {}", path.display());
                breaking = true;
                continue;
            }
            writes.push((path, render(&current)));
            continue;
        };
        let old = parse(&text).with_context(|| path.display().to_string())?;
        let diff = diff(&old, &current);
        additions += diff.added.len();
        print!("{}", report(name, &diff));
        let findings = deprecation_findings(name, &diff, policy);
        print!("{findings}");
        if policy.freeze && !findings.is_empty() {
            frozen = true;
        }
        if diff.breaking() {
            breaking = true;
            if let Some(why) = &args.breaking {
                rows.push_str(&breaking_rows(name, &diff, &workspace_version(root)?, why));
            }
        }
        if !diff.is_empty() {
            writes.push((path, render(&current)));
        }
    }
    if !args.update {
        if breaking || frozen {
            println!(
                "api-snapshot: FAIL: removed or changed public items (docs/api-snapshots/README.md).\n\
                 Intended: cargo xtask api-snapshot --update --breaking \"<why>\""
            );
            return Ok(ExitCode::FAILURE);
        }
        println!("api-snapshot: ok ({additions} new item(s) not in the snapshots yet)");
        return Ok(ExitCode::SUCCESS);
    }
    if frozen {
        println!(
            "api-snapshot: nothing written: the API is frozen (policy.toml) and an item was removed \
             without a deprecation window; deprecate it first (specs/57 § 11.1)"
        );
        return Ok(ExitCode::FAILURE);
    }
    if breaking && args.breaking.is_none() {
        println!("api-snapshot: nothing written: name the reason with --breaking \"<why>\"");
        return Ok(ExitCode::FAILURE);
    }
    std::fs::create_dir_all(root.join(SNAPSHOT_DIR)).context("creating the snapshot folder")?;
    let count = writes.len();
    for (path, text) in writes {
        std::fs::write(&path, text).with_context(|| path.display().to_string())?;
    }
    if !rows.is_empty() {
        append_breaking(root, &rows)?;
    }
    println!("api-snapshot: {count} snapshot(s) written");
    Ok(ExitCode::SUCCESS)
}

/// The top of `BREAKING.md`.
const BREAKING_HEADER: &str = "# Accepted breaking API changes\n\nAppended by `cargo xtask api-snapshot --update --breaking \"<why>\"` (see README.md).\n\n| Version | Crate | Kind | Item | Why |\n|---|---|---|---|---|\n";

fn append_breaking(root: &Path, rows: &str) -> Result<()> {
    let path = root.join(SNAPSHOT_DIR).join("BREAKING.md");
    let mut text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => BREAKING_HEADER.to_owned(),
        Err(err) => return Err(err).with_context(|| path.display().to_string()),
    };
    text.push_str(rows);
    std::fs::write(&path, text).with_context(|| path.display().to_string())
}

/// The version cell of a `BREAKING.md` table row, or `None` for any other line.
fn row_version(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('|')?;
    let cell = rest.split('|').next()?.trim();
    let mut parts = cell.split('.');
    let numeric = |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    (numeric(parts.next()) && numeric(parts.next()) && numeric(parts.next()) && parts.next().is_none())
        .then_some(cell)
}

/// Moves the `BREAKING.md` rows whose version cell is `version` into
/// `docs/api-snapshots/breaking/<version>.md` and keeps the rest. Returns the number of rows moved.
///
/// Nothing is touched when the version is the workspace's current one (unreleased), when a row
/// of the table has an unreadable version cell, or when the rows to move are not found.
pub fn rotate(root: &Path, version: &str) -> Result<usize> {
    if version.is_empty()
        || version
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
    {
        bail!("{version:?} is not a version");
    }
    if workspace_version(root)? == version {
        bail!("{version} is the version being built: its rows are unreleased, nothing rotated");
    }
    let path = root.join(SNAPSHOT_DIR).join("BREAKING.md");
    let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    let (mut keep, mut moved) = (String::new(), String::new());
    let mut count = 0;
    // The first two table lines are the column header and its rule; they are not rows.
    let mut table_lines = 0;
    for line in text.lines() {
        if line.starts_with('|') {
            table_lines += 1;
            if table_lines > 2 {
                match row_version(line) {
                    None => bail!(
                        "{}: a row has no readable version, nothing rotated: {line}",
                        path.display()
                    ),
                    Some(v) if v == version => {
                        moved.push_str(line);
                        moved.push('\n');
                        count += 1;
                        continue;
                    }
                    Some(_) => {}
                }
            }
        }
        keep.push_str(line);
        keep.push('\n');
    }
    if count == 0 {
        bail!("no row of version {version} in {}", path.display());
    }
    let dir = root.join(SNAPSHOT_DIR).join("breaking");
    std::fs::create_dir_all(&dir).with_context(|| dir.display().to_string())?;
    let target = dir.join(format!("{version}.md"));
    let mut archive = match std::fs::read_to_string(&target) {
        Ok(existing) => existing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => format!(
            "# Accepted breaking API changes of {version}\n\nRotated out of `../BREAKING.md` by `cargo xtask api-snapshot --rotate {version}`.\n\n| Version | Crate | Kind | Item | Why |\n|---|---|---|---|---|\n"
        ),
        Err(err) => return Err(err).with_context(|| target.display().to_string()),
    };
    archive.push_str(&moved);
    // The archive first: a failure between the two writes duplicates rows, never loses them.
    std::fs::write(&target, archive).with_context(|| target.display().to_string())?;
    std::fs::write(&path, keep).with_context(|| path.display().to_string())?;
    Ok(count)
}
