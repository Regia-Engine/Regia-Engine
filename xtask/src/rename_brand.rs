// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask rename-brand`: the rename of the engine from its old name to Regia Engine
//! (ADR-1388, `docs/rename-regia.md`).
//!
//! The tool is deterministic and idempotent so the same binary converts `main` and every lane
//! branch to identical results, and a second run changes nothing:
//!
//! - **Files** come from `git ls-files --cached --others --exclude-standard` (or a sorted walk that
//!   skips `.git`, `target`, `.fgate`, `.regia` and `.ferrite` outside a git checkout), processed
//!   in sorted order.
//! - **Text** is rewritten occurrence by occurrence of the old name, in any ASCII case, by its
//!   context: `ferrite` → `regia`, `FERRITE` → `REGIA`, `Ferrite` inside an identifier →
//!   `Regia`, `Ferrite.toml` → `Regia.toml`, `Ferrite/` (a per-user directory) → `RegiaEngine/`,
//!   the bare word `Ferrite` → `Regia Engine`, `com.ferrite.` (Android ids, Java packages and JNI
//!   symbols in any separator) → `com.regiaengine.`. Mixed casings are reported, never guessed.
//! - **Paths** are renamed with the same rules (a bare `Ferrite` component becomes `Regia`).
//! - **Binary files** (a NUL byte or invalid UTF-8) are moved but never rewritten: on-disk formats,
//!   magics and golden images keep every byte.
//! - **Environment reads** in crates that depend on the core crate are routed through
//!   `brand::env_var` / `brand::env_var_os`, which read the new `REGIA_*` name first and the old
//!   name for one release.
//! - **`Cargo.lock`** package blocks and dependency lists are re-sorted the way cargo writes them.
//! - **The keep list** (`xtask/rename-keep.txt`, compiled in) and in-file markers protect what
//!   must not change: a line containing `rename-keep`, the lines from `rename-keep-begin` to
//!   `rename-keep-end`, `keep-file` globs (neither path nor content), `keep-content` globs (moved,
//!   not rewritten) and `keep-text` literals (protected wherever they appear).
//!
//! The tool never reads outside the tree it converts (symlinks are moved, never followed) and
//! refuses a root inside a path listed in `CLEANROOM-DENY.txt`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;

/// The keep list, compiled in so every tree is converted by the same rules.
pub const KEEP_LIST: &str = include_str!("../rename-keep.txt");

/// Marker that protects the line it is on.
const LINE_MARKER: &str = "rename-keep";
/// Marker that starts a protected block (the marker line included).
const BLOCK_BEGIN: &str = "rename-keep-begin";
/// Marker that ends a protected block (the marker line included).
const BLOCK_END: &str = "rename-keep-end";

/// The flag in the core crate's `brand` module that selects the new names.
const FLAG_BEFORE: &str = "pub const BRAND_RENAMED: bool = false;";
/// The flag after the rename.
const FLAG_AFTER: &str = "pub const BRAND_RENAMED: bool = true;";

/// The line inserted under the title of `docs/decisions.md`.
const DECISIONS_NOTE: &str =
    "The project was named Ferrite before ADR-1388 (2026-10-22); ADRs before it keep the old name.";

/// Command-line arguments of `cargo xtask rename-brand`.
#[derive(ClapArgs, Debug, Default, Clone)]
pub struct Args {
    /// Compute every change and print the summary, write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Also print every rewritten file, every moved path, every kept hit and every leftover.
    #[arg(long)]
    pub report: bool,
    /// The rename check (ADR-8670): a dry run that fails (exit 1) when any file would be
    /// rewritten or moved, or any leftover remains, i.e. an old-name hit outside the keep list.
    #[arg(long)]
    pub check: bool,
    /// The tree to convert (default: the workspace this xtask was built from).
    #[arg(long)]
    pub root: Option<PathBuf>,
}

/// The parsed keep list.
#[derive(Debug, Default, Clone)]
pub struct KeepList {
    /// Globs whose files keep both their path and their content.
    pub files: Vec<String>,
    /// Globs whose files keep their content (their path is still renamed).
    pub contents: Vec<String>,
    /// Literals protected wherever they appear.
    pub texts: Vec<String>,
}

impl KeepList {
    /// Parses the keep-list format: `keep-file <glob>`, `keep-content <glob>` or
    /// `keep-text <literal>` per line; `#` starts a comment line.
    pub fn parse(text: &str) -> Result<Self> {
        let mut keep = Self::default();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (kind, value) = line
                .split_once(char::is_whitespace)
                .with_context(|| format!("rename-keep.txt:{}: expected `<kind> <value>`", n + 1))?;
            let value = value.trim().to_owned();
            match kind {
                "keep-file" => keep.files.push(value),
                "keep-content" => keep.contents.push(value),
                "keep-text" => keep.texts.push(value),
                other => bail!("rename-keep.txt:{}: unknown kind `{other}`", n + 1),
            }
        }
        Ok(keep)
    }

    /// The compiled-in keep list.
    pub fn builtin() -> Result<Self> {
        Self::parse(KEEP_LIST)
    }

    /// A glob written with the old names also matches the converted tree (the glob is renamed
    /// the way paths are), so a second run keeps exactly what the first one kept.
    fn matches(globs: &[String], old: &str, new: &str) -> bool {
        globs.iter().any(|g| {
            let renamed = rename_path(g);
            [g.as_str(), renamed.as_str()]
                .iter()
                .any(|g| glob_match(g, old) || glob_match(g, new))
        })
    }

    /// Whether the file at `old` (renamed to `new`) keeps its path and content.
    pub fn keeps_file(&self, old: &str, new: &str) -> bool {
        Self::matches(&self.files, old, new)
    }

    /// Whether the file at `old` (renamed to `new`) keeps its content.
    pub fn keeps_content(&self, old: &str, new: &str) -> bool {
        self.keeps_file(old, new) || Self::matches(&self.contents, old, new)
    }
}

/// Matches `path` (relative, `/`-separated) against `glob`: `**` crosses directories, `*` and `?`
/// do not.
pub fn glob_match(glob: &str, path: &str) -> bool {
    fn go(g: &[u8], p: &[u8]) -> bool {
        match g.split_first() {
            None => p.is_empty(),
            Some((b'*', rest)) if rest.first() == Some(&b'*') => {
                let after = &rest[1..];
                if let Some(tail) = after.strip_prefix(b"/") {
                    // `**/`: zero or more whole directories.
                    go(tail, p) || (1..=p.len()).any(|i| p[i - 1] == b'/' && go(tail, &p[i..]))
                } else {
                    (0..=p.len()).any(|i| go(after, &p[i..]))
                }
            }
            Some((b'*', rest)) => (0..=p.len())
                .take_while(|&i| i == 0 || p[i - 1] != b'/')
                .any(|i| go(rest, &p[i..])),
            Some((b'?', rest)) => p.first().is_some_and(|&c| c != b'/') && go(rest, &p[1..]),
            Some((&c, rest)) => p.first() == Some(&c) && go(rest, &p[1..]),
        }
    }
    go(glob.as_bytes(), path.as_bytes())
}

/// What one rewrite did, by rule.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Counts {
    /// Replacements per rule name.
    pub rules: BTreeMap<&'static str, usize>,
    /// Old-name hits left alone because a keep rule or marker protects them.
    pub kept: usize,
    /// Old-name hits in a casing no rule covers (reported, never guessed), as `(line, text)`.
    pub leftovers: Vec<(usize, String)>,
}

impl Counts {
    fn bump(&mut self, rule: &'static str) {
        *self.rules.entry(rule).or_default() += 1;
    }

    /// Total replacements.
    pub fn replacements(&self) -> usize {
        self.rules.values().sum()
    }

    fn absorb(&mut self, other: &Counts) {
        for (rule, n) in &other.rules {
            *self.rules.entry(rule).or_default() += n;
        }
        self.kept += other.kept;
    }
}

/// Where the rewrite runs: prose and code, or a path.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    Path,
}

const OLD: &[u8] = b"ferrite";

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Byte ranges of `text` no rule may touch: keep-text literals, marked lines and marked blocks.
/// A keep-text ending in `#` (`decisions.md#`) also covers the link fragment after it, up to the
/// next `)`, quote, `>`, `]` or whitespace: anchors into a kept file must keep matching its headings.
fn protected_ranges(text: &str, keep_texts: &[String]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for literal in keep_texts.iter().filter(|t| !t.is_empty()) {
        let mut from = 0;
        while let Some(at) = text[from..].find(literal.as_str()) {
            let start = from + at;
            let mut end = start + literal.len();
            if literal.ends_with('#') {
                end += text[end..]
                    .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '>' | ']'))
                    .unwrap_or(text.len() - end);
            }
            ranges.push((start, end));
            from = end;
        }
    }
    let mut offset = 0;
    let mut in_block = false;
    for line in text.split_inclusive('\n') {
        let end = offset + line.len();
        if line.contains(BLOCK_BEGIN) {
            in_block = true;
        }
        if in_block || line.contains(LINE_MARKER) {
            ranges.push((offset, end));
        }
        if line.contains(BLOCK_END) {
            in_block = false;
        }
        offset = end;
    }
    ranges.sort_unstable();
    ranges
}

fn is_protected(ranges: &[(usize, usize)], start: usize, end: usize) -> bool {
    ranges.iter().any(|&(a, b)| a < end && start < b)
}

/// One replacement: the rule's name, the new text, and how many bytes after the old name it also
/// consumes.
type Replacement = (&'static str, &'static str, usize);

/// The replacement for the old name at `at` (any case) in `text`; `None` for a casing no rule
/// covers.
fn replacement(text: &[u8], at: usize, mode: Mode) -> Option<Replacement> {
    let end = at + OLD.len();
    let word = &text[at..end];
    let before = &text[..at];
    let after = &text[end..];
    let prev = before.last().copied();
    let next = after.first().copied();
    // Reverse-DNS ids (`com.ferrite.x`), their directories (`com/ferrite/`) and JNI symbols
    // (`Java_com_ferrite_x`).
    if word == OLD {
        for sep in *b"./_\\" {
            let com = [b'c', b'o', b'm', sep];
            if before.ends_with(&com) && next == Some(sep) {
                let ok = before.len() == 4 || !is_ident(before[before.len() - 5]) || sep == b'_';
                if ok {
                    return Some(("android-id", "regiaengine", 0));
                }
            }
        }
        if after.starts_with(b".dev") && !after.get(4).copied().is_some_and(is_ident) {
            return Some(("domain", "regiaengine.com", ".dev".len()));
        }
        return Some(("lower", "regia", 0));
    }
    if word == b"FERRITE" {
        return Some(("upper", "REGIA", 0));
    }
    if word == b"Ferrite" {
        let inside = prev.is_some_and(is_ident) || next.is_some_and(is_ident);
        if after.starts_with(b".toml") {
            return Some(("manifest", "Regia", 0));
        }
        // Inside an identifier, or inside a hyphenated token such as an HTTP header name
        // (`X-Ferrite-Report-Name`).
        if inside || (prev == Some(b'-') && next == Some(b'-')) {
            return Some(("title", "Regia", 0));
        }
        if mode == Mode::Path {
            return Some(("title", "Regia", 0));
        }
        // A per-user directory: `Ferrite/` or `Ferrite\`, but not a backslash that starts a string
        // escape in source code (`"Hello Ferrite\"`, `Ferrite\n`).
        let escape =
            next == Some(b'\\') && matches!(after.get(1), Some(b'"' | b'\'' | b'n' | b't' | b'r' | b'0'));
        if next == Some(b'/') || (next == Some(b'\\') && !escape) {
            return Some(("user-dir", "RegiaEngine", 0));
        }
        let engine_follows = [b" Engine".as_slice(), b" engine", b"-Engine", b"-engine"]
            .iter()
            .any(|e| after.starts_with(e) && !after.get(e.len()).copied().is_some_and(is_ident));
        if engine_follows {
            return Some(("title", "Regia", 0));
        }
        return Some(("product", "Regia Engine", 0));
    }
    None
}

/// Rewrites every old-name occurrence in `text` outside the protected ranges.
fn rewrite_occurrences(text: &str, keep_texts: &[String], mode: Mode, counts: &mut Counts) -> String {
    let bytes = text.as_bytes();
    let ranges = if mode == Mode::Text {
        protected_ranges(text, keep_texts)
    } else {
        Vec::new()
    };
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i + OLD.len() <= bytes.len() {
        if !bytes[i..i + OLD.len()].eq_ignore_ascii_case(OLD) {
            i += 1;
            continue;
        }
        let end = i + OLD.len();
        if is_protected(&ranges, i, end) {
            counts.kept += 1;
            i = end;
            continue;
        }
        match replacement(bytes, i, mode) {
            Some((rule, new, extra)) => {
                out.push_str(&text[copied..i]);
                out.push_str(new);
                copied = end + extra;
                counts.bump(rule);
                i = copied;
            }
            None => {
                let line = text[..i].matches('\n').count() + 1;
                counts.leftovers.push((line, text[i..end].to_owned()));
                i = end;
            }
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Rewrites one text file's content with the brand rules (`keep_texts` and the in-file markers
/// protect what they cover).
pub fn rename_text(text: &str, keep_texts: &[String], counts: &mut Counts) -> String {
    rewrite_occurrences(text, keep_texts, Mode::Text, counts)
}

/// The renamed form of a relative, `/`-separated path.
pub fn rename_path(path: &str) -> String {
    rewrite_occurrences(path, &[], Mode::Path, &mut Counts::default())
}

/// Routes `std::env::var(` / `std::env::var_os(` through the core crate's `brand` helpers so the
/// old variable names stay readable. `core_path` is `crate` inside the core library and the core
/// crate's name elsewhere. Protected lines are left alone.
pub fn route_env_reads(text: &str, core_path: &str, keep_texts: &[String], counts: &mut Counts) -> String {
    let ranges = protected_ranges(text, keep_texts);
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(at) = text[from..].find("std::env::var") {
        let start = from + at;
        let rest = &text[start + "std::env::var".len()..];
        let (tail, helper) = if rest.starts_with("_os(") {
            ("_os(", "env_var_os(")
        } else if rest.starts_with('(') {
            ("(", "env_var(")
        } else {
            from = start + 1;
            continue;
        };
        let end = start + "std::env::var".len() + tail.len();
        let preceded_by_path = text[..start].ends_with("::")
            || text[..start].ends_with(|c: char| c.is_alphanumeric() || c == '_');
        if preceded_by_path || is_protected(&ranges, start, end) {
            from = end;
            continue;
        }
        out.push_str(&text[copied..start]);
        out.push_str(core_path);
        out.push_str("::brand::");
        out.push_str(helper);
        copied = end;
        from = end;
        counts.bump("route-env");
    }
    out.push_str(&text[copied..]);
    out
}

/// Re-sorts `Cargo.lock` the way cargo writes it after crate names changed: `[[package]]` blocks
/// by name (stable, so versions keep their order) and each `dependencies` list by name.
pub fn sort_cargo_lock(text: &str) -> String {
    let Some(first) = text.find("[[package]]") else {
        return text.to_owned();
    };
    let (head, body) = text.split_at(first);
    let mut blocks: Vec<String> = Vec::new();
    for chunk in body.split("[[package]]").filter(|c| !c.is_empty()) {
        blocks.push(format!("[[package]]{}", sort_dependency_lists(chunk)));
    }
    let name_of = |block: &str| {
        block
            .lines()
            .find_map(|l| l.strip_prefix("name = \""))
            .map(|n| n.trim_end_matches('"').to_owned())
            .unwrap_or_default()
    };
    // The last block carries the file's trailing text; keep it last-terminated after sorting.
    let mut keyed: Vec<(String, String)> = blocks.into_iter().map(|b| (name_of(&b), b)).collect();
    for (_, block) in &mut keyed {
        let trimmed = block.trim_end_matches('\n').len();
        block.truncate(trimmed);
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = head.to_owned();
    for (i, (_, block)) in keyed.iter().enumerate() {
        out.push_str(block);
        out.push('\n');
        if i + 1 < keyed.len() {
            out.push('\n');
        }
    }
    out
}

fn sort_dependency_lists(block: &str) -> String {
    let mut out = String::with_capacity(block.len());
    let mut lines = block.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        out.push_str(line);
        if line.trim_end() != "dependencies = [" {
            continue;
        }
        let mut entries = Vec::new();
        while let Some(entry) = lines.peek() {
            if entry.trim_start().starts_with(']') {
                break;
            }
            entries.push(*entry);
            lines.next();
        }
        let key = |e: &str| {
            e.trim()
                .trim_matches(|c| c == '"' || c == ',')
                .split(' ')
                .next()
                .unwrap_or("")
                .to_owned()
        };
        entries.sort_by_key(|e| key(e));
        for entry in entries {
            out.push_str(entry);
        }
    }
    out
}

/// Inserts the one-line history note under the title of `docs/decisions.md` (once).
pub fn insert_decisions_note(text: &str) -> String {
    if text.contains(DECISIONS_NOTE) {
        return text.to_owned();
    }
    match text.split_once('\n') {
        Some((title, rest)) => format!("{title}\n\n{DECISIONS_NOTE}\n{rest}"),
        None => format!("{text}\n\n{DECISIONS_NOTE}\n"),
    }
}

/// One file of the tree as the tool sees it.
#[derive(Debug, Clone)]
struct Entry {
    old: String,
    new: String,
}

/// What a run did (or would do, for `--dry-run`).
#[derive(Debug, Default)]
pub struct Outcome {
    /// Files scanned.
    pub files: usize,
    /// Files whose content changed: path (new name) → replacements.
    pub rewritten: BTreeMap<String, usize>,
    /// The subset of [`Self::rewritten`] with an old-name hit (any rule but `route-env`, which
    /// only routes environment reads through the brand helpers): path (new name) → hits. The
    /// rename check fails on these, not on routing alone (ADR-8670).
    pub name_hits: BTreeMap<String, usize>,
    /// Paths moved: old → new.
    pub moved: BTreeMap<String, String>,
    /// Totals by rule, plus kept hits.
    pub counts: Counts,
    /// Leftover old-name hits outside the keep list: `path:line: text`.
    pub leftovers: Vec<String>,
    /// Kept hits per file (keep-file, keep-content, keep-text or marker): path → hits.
    pub kept: BTreeMap<String, usize>,
    /// Binary files that contain the old name (never rewritten): path → hits.
    pub binary_hits: BTreeMap<String, usize>,
    /// Moves refused because a different file already exists at the target.
    pub conflicts: Vec<String>,
}

impl Outcome {
    /// Whether the run changed (or would change) nothing.
    pub fn is_noop(&self) -> bool {
        self.rewritten.is_empty() && self.moved.is_empty()
    }

    /// Whether the tree passes the rename check (`--check`, ADR-8670): no old-name hit to
    /// rewrite ([`Self::name_hits`]), no path to move and no leftover, so every old-name hit is
    /// in the keep list. Environment reads the tool would only route through the brand helpers
    /// do not count.
    pub fn is_clean(&self) -> bool {
        self.name_hits.is_empty() && self.moved.is_empty() && self.leftovers.is_empty()
    }

    /// A human-readable summary; `detail` adds every file, move, kept hit and leftover.
    pub fn render(&self, detail: bool) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "rename-brand: {} files scanned, {} rewritten, {} moved, {} replacements, {} kept hits in {} files, {} leftover hits, {} binary files with old-name bytes",
            self.files,
            self.rewritten.len(),
            self.moved.len(),
            self.counts.replacements(),
            self.counts.kept,
            self.kept.len(),
            self.leftovers.len(),
            self.binary_hits.len(),
        );
        for (rule, n) in &self.counts.rules {
            let _ = writeln!(s, "  rule {rule}: {n}");
        }
        for conflict in &self.conflicts {
            let _ = writeln!(s, "  conflict: {conflict}");
        }
        if detail {
            for (old, new) in &self.moved {
                let _ = writeln!(s, "move {old} -> {new}");
            }
            for (path, n) in &self.rewritten {
                let _ = writeln!(s, "rewrite {path} ({n})");
            }
            for (path, n) in &self.kept {
                let _ = writeln!(s, "kept {path} ({n})");
            }
            for (path, n) in &self.binary_hits {
                let _ = writeln!(s, "binary {path} ({n})");
            }
        }
        for leftover in &self.leftovers {
            let _ = writeln!(s, "leftover {leftover}");
        }
        s
    }
}

/// Refuses a root inside a path the cleanroom deny list names (`CLEANROOM-DENY.txt` at the root).
fn check_cleanroom(root: &Path) -> Result<()> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    let deny = std::fs::read_to_string(root.join("CLEANROOM-DENY.txt")).unwrap_or_default();
    // With a trailing `/`, so `/a/b/` covers the directory `/a/b` itself.
    let root_dir = format!(
        "{}/",
        deny_form(&canonical.to_string_lossy()).trim_end_matches('/')
    );
    for line in deny.lines().map(str::trim) {
        let form = deny_form(line);
        if !is_absolute_deny(&form) {
            continue;
        }
        let prefix = form.split('*').next().unwrap_or(&form);
        if root_dir.starts_with(prefix) {
            bail!(
                "{} is inside a cleanroom deny path ({line}); refusing",
                canonical.display()
            );
        }
    }
    Ok(())
}

/// A path or deny-list line in the form the cleanroom prefix check compares. Unchanged on Unix.
/// On Windows: without the `\\?\` verbatim prefix that `canonicalize` adds, `/`-separated and
/// lowercase (Windows paths ignore case), so `C:\UE\` in the list covers `c:/ue/x`.
fn deny_form(path: &str) -> String {
    if cfg!(windows) {
        let path = match path.strip_prefix(r"\\?\UNC\") {
            Some(share) => format!(r"\\{share}"),
            None => path.strip_prefix(r"\\?\").unwrap_or(path).to_owned(),
        };
        path.replace('\\', "/").to_lowercase()
    } else {
        path.to_owned()
    }
}

/// Whether a deny-list line (in [`deny_form`]) names an absolute path: `/...` on every host and,
/// on Windows, also a drive path `c:/...`.
fn is_absolute_deny(form: &str) -> bool {
    let drive = matches!(form.as_bytes(), [letter, b':', b'/', ..] if letter.is_ascii_alphabetic());
    form.starts_with('/') || (cfg!(windows) && drive)
}

/// The tree's files, relative and `/`-separated, sorted.
fn list_files(root: &Path) -> Result<Vec<String>> {
    let git = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .output();
    let mut files: Vec<String> = match git {
        Ok(out) if out.status.success() && root.join(".git").exists() => out
            .stdout
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .filter(|p| {
                root.join(p)
                    .symlink_metadata()
                    .is_ok_and(|m| m.is_file() || m.is_symlink())
            })
            .collect(),
        _ => {
            let mut files = Vec::new();
            walk(root, root, &mut files)?;
            files
        }
    };
    files.sort();
    files.dedup();
    Ok(files)
}

fn walk(root: &Path, dir: &Path, files: &mut Vec<String>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            // Build output, the merge gate's scratch (`.fgate/`) and the project state / cache
            // directories (`.regia/`, its old name): never part of the tree's own files.
            if [".git", "target", ".fgate", ".regia", ".ferrite"]
                .iter()
                .any(|d| name == *d)
            {
                continue;
            }
            walk(root, &path, files)?;
        } else if kind.is_file() || kind.is_symlink() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            files.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// The crates (directory names under `crates/`) whose `[dependencies]` include the core crate
/// under its old or new name, so their code can call `brand::env_var`.
fn crates_with_core(root: &Path, files: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for file in files {
        let Some(rest) = file.strip_prefix("crates/") else {
            continue;
        };
        let Some((krate, "Cargo.toml")) = rest.split_once('/') else {
            continue;
        };
        let path = root.join(file);
        if !path.symlink_metadata().is_ok_and(|m| m.is_file()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut in_deps = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_deps = line == "[dependencies]"
                    || (line.starts_with("[target.") && line.ends_with(".dependencies]"));
                continue;
            }
            let is_core = ["ferrite_core", "regia_core"].iter().any(|c| {
                line.strip_prefix(c)
                    .is_some_and(|r| r.trim_start().starts_with('=') || r.starts_with('.'))
            });
            if in_deps && is_core && !line.contains("optional = true") {
                out.insert(krate.to_owned());
            }
        }
    }
    out
}

/// Whether `path` is the core crate's `brand` module (the only file whose flag is flipped).
fn is_brand_module(path: &str) -> bool {
    path == "crates/ferrite_core/src/brand.rs" || path == "crates/regia_core/src/brand.rs"
}

/// How a file's environment reads are routed, if at all: the path to the core crate.
fn env_route_for(old: &str, with_core: &BTreeSet<String>) -> Option<&'static str> {
    if !old.ends_with(".rs") {
        return None;
    }
    let rest = old.strip_prefix("crates/")?;
    let (krate, inner) = rest.split_once('/')?;
    let in_crate_code = ["src/", "tests/", "examples/", "benches/"]
        .iter()
        .any(|d| inner.starts_with(d));
    if !in_crate_code {
        return None;
    }
    let is_core = krate == "ferrite_core" || krate == "regia_core";
    if is_core {
        // Inside the core library itself, `crate::`; its integration tests and binaries are
        // separate crates.
        let lib = inner.starts_with("src/") && !inner.starts_with("src/bin/") && inner != "src/main.rs";
        return Some(if lib { "crate" } else { "regia_core" });
    }
    with_core.contains(krate).then_some("regia_core")
}

/// Converts the tree at `root` with the given keep list. With `dry_run`, nothing is written.
pub fn convert(root: &Path, keep: &KeepList, dry_run: bool) -> Result<Outcome> {
    check_cleanroom(root)?;
    let files = list_files(root)?;
    let with_core = crates_with_core(root, &files);
    let mut outcome = Outcome {
        files: files.len(),
        ..Outcome::default()
    };
    let mut writes: Vec<(Entry, Option<String>)> = Vec::new();

    for old in &files {
        let new = rename_path(old);
        let entry = Entry {
            old: old.clone(),
            new: if keep.keeps_file(old, &new) {
                old.clone()
            } else {
                new
            },
        };
        let path = root.join(&entry.old);
        let meta = path
            .symlink_metadata()
            .with_context(|| format!("stat {}", path.display()))?;
        let mut content = None;
        if meta.is_file() {
            let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let hits = count_old(&bytes);
            match String::from_utf8(bytes) {
                Ok(text) if !text.contains('\0') => {
                    let mut counts = Counts::default();
                    let mut updated = text.clone();
                    if is_brand_module(&entry.old) && updated.contains(FLAG_BEFORE) {
                        updated = updated.replace(FLAG_BEFORE, FLAG_AFTER);
                        counts.bump("brand-flag");
                    }
                    if keep.keeps_content(&entry.old, &entry.new) {
                        counts.kept += hits;
                    } else {
                        updated = rename_text(&updated, &keep.texts, &mut counts);
                        if let Some(core_path) = env_route_for(&entry.old, &with_core) {
                            updated = route_env_reads(&updated, core_path, &keep.texts, &mut counts);
                        }
                        if entry.new.ends_with("Cargo.lock") {
                            updated = sort_cargo_lock(&updated);
                        }
                    }
                    if entry.new == "docs/decisions.md" {
                        let noted = insert_decisions_note(&updated);
                        if noted != updated {
                            counts.bump("decisions-note");
                            updated = noted;
                        }
                    }
                    for (line, text) in &counts.leftovers {
                        outcome.leftovers.push(format!("{}:{line}: {text}", entry.new));
                    }
                    if counts.kept > 0 {
                        outcome.kept.insert(entry.new.clone(), counts.kept);
                    }
                    outcome.counts.absorb(&counts);
                    let hits = counts.replacements() - counts.rules.get("route-env").copied().unwrap_or(0);
                    if hits > 0 {
                        outcome.name_hits.insert(entry.new.clone(), hits);
                    }
                    if updated != text {
                        outcome.rewritten.insert(entry.new.clone(), counts.replacements());
                        content = Some(updated);
                    }
                }
                _ => {
                    if hits > 0 {
                        outcome.binary_hits.insert(entry.new.clone(), hits);
                    }
                }
            }
        }
        if entry.old.to_ascii_lowercase().contains("ferrite") && entry.old == entry.new {
            *outcome.kept.entry(entry.new.clone()).or_default() += 1;
        }
        if entry.old != entry.new {
            outcome.moved.insert(entry.old.clone(), entry.new.clone());
        }
        if content.is_some() || entry.old != entry.new {
            writes.push((entry, content));
        }
    }

    if dry_run {
        return Ok(outcome);
    }
    for (entry, content) in writes {
        let from = root.join(&entry.old);
        let to = root.join(&entry.new);
        if let Some(text) = &content {
            write_atomic(&from, text)?;
        }
        if entry.old == entry.new {
            continue;
        }
        if to.symlink_metadata().is_ok() {
            // A tree that is mid-way: the target exists already. Identical content folds; a
            // different file is never overwritten.
            // Symlinks are never followed (one could point outside the tree): only regular files
            // are compared.
            let regular = |p: &Path| p.symlink_metadata().is_ok_and(|m| m.is_file());
            if regular(&from) && regular(&to) && std::fs::read(&from).ok() == std::fs::read(&to).ok() {
                std::fs::remove_file(&from).with_context(|| format!("removing {}", from.display()))?;
                remove_empty_parents(root, &from);
            } else {
                outcome.conflicts.push(format!(
                    "{} -> {} (target exists and differs)",
                    entry.old, entry.new
                ));
                outcome.moved.remove(&entry.old);
            }
            continue;
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::rename(&from, &to)
            .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        remove_empty_parents(root, &from);
    }
    Ok(outcome)
}

fn count_old(bytes: &[u8]) -> usize {
    bytes
        .windows(OLD.len())
        .filter(|w| w.eq_ignore_ascii_case(OLD))
        .count()
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_file_name(format!(
        ".{}.rename-brand-tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

fn remove_empty_parents(root: &Path, file: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == root || std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// `cargo xtask rename-brand`.
pub fn run(args: &Args, default_root: &Path) -> Result<std::process::ExitCode> {
    let root = args.root.clone().unwrap_or_else(|| default_root.to_path_buf());
    let keep = KeepList::builtin()?;
    let outcome = convert(&root, &keep, args.dry_run || args.check)?;
    print!("{}", outcome.render(args.report));
    if args.check {
        for (path, hits) in &outcome.name_hits {
            println!("old-name hit {path} ({hits})");
        }
        for (old, new) in &outcome.moved {
            println!("old-name path {old} -> {new}");
        }
        for leftover in &outcome.leftovers {
            println!("leftover {leftover}");
        }
        return Ok(if outcome.is_clean() {
            println!("rename-brand: check passed, every old-name hit is in xtask/rename-keep.txt");
            std::process::ExitCode::SUCCESS
        } else {
            println!(
                "rename-brand: check FAILED: write the new name (and read both where old data has the old one), or add the hit to xtask/rename-keep.txt with the reason (ADR-8670)"
            );
            std::process::ExitCode::FAILURE
        });
    }
    if args.dry_run {
        println!("rename-brand: dry run, nothing written");
    } else if !outcome.is_noop() {
        println!("rename-brand: now run `cargo fmt --all`, then the checks in docs/rename-regia.md");
    }
    Ok(if outcome.conflicts.is_empty() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_lines_are_absolute_paths() {
        assert!(is_absolute_deny("/home/x/UnrealEngine/"));
        assert!(!is_absolute_deny("# comment"));
        assert!(!is_absolute_deny("relative/dir"));
        assert!(!is_absolute_deny(""));
        assert_eq!(is_absolute_deny("c:/ue/"), cfg!(windows));
    }

    #[cfg(not(windows))]
    #[test]
    fn deny_form_keeps_unix_paths_as_they_are() {
        assert_eq!(deny_form("/Home/X/UE/"), "/Home/X/UE/");
        assert_eq!(deny_form(r"/a\\b"), r"/a\\b");
    }

    #[cfg(windows)]
    #[test]
    fn deny_form_compares_windows_paths_by_drive_separator_and_case() {
        assert_eq!(deny_form(r"\\?\C:\Work\UE"), "c:/work/ue");
        assert_eq!(deny_form(r"C:\Work\UE\"), "c:/work/ue/");
        assert_eq!(deny_form("C:/Work/UE/*"), "c:/work/ue/*");
        assert_eq!(deny_form(r"\\?\UNC\server\share\ue"), "//server/share/ue");
    }

    fn text(input: &str) -> String {
        rename_text(input, &[], &mut Counts::default())
    }

    #[test]
    fn case_rules() {
        assert_eq!(text("use ferrite_core::Guid;"), "use regia_core::Guid;");
        assert_eq!(text("FERRITE_LOG=debug"), "REGIA_LOG=debug");
        assert_eq!(text("pub struct FerritePlugin;"), "pub struct RegiaPlugin;");
        assert_eq!(text("Ferrite.toml"), "Regia.toml");
        assert_eq!(text("Ferrite is an engine."), "Regia Engine is an engine.");
        assert_eq!(text("the Ferrite Engine repo"), "the Regia Engine repo");
        assert_eq!(text("X-Ferrite-Report-Name"), "X-Regia-Report-Name");
        assert_eq!(text("Ferrite-side code"), "Regia Engine-side code");
        assert_eq!(
            text("~/.local/share/Ferrite/<exe>"),
            "~/.local/share/RegiaEngine/<exe>"
        );
        assert_eq!(text(r"%APPDATA%\Ferrite\<exe>"), r"%APPDATA%\RegiaEngine\<exe>");
        assert_eq!(
            text(r"%APPDATA%\\Ferrite\\<exe>"),
            r"%APPDATA%\\RegiaEngine\\<exe>"
        );
        assert_eq!(
            text(r#"name = \"Hello Ferrite\"\n"#),
            r#"name = \"Hello Regia Engine\"\n"#
        );
        assert_eq!(text(r"Ferrite\n"), r"Regia Engine\n");
        assert_eq!(text("ferrite new my_game"), "regia new my_game");
        assert_eq!(text("<root>/.ferrite/cooked"), "<root>/.regia/cooked");
        assert_eq!(text("ferrite-out"), "regia-out");
        assert_eq!(text("libferrite_dylib.so"), "libregia_dylib.so");
    }

    #[test]
    fn android_ids_java_dirs_and_jni_symbols() {
        assert_eq!(text("com.ferrite.samples.arena"), "com.regiaengine.samples.arena");
        assert_eq!(text("java/com/ferrite/my_game/"), "java/com/regiaengine/my_game/");
        assert_eq!(text("Java_com_ferrite_x_Act_m"), "Java_com_regiaengine_x_Act_m");
        assert_eq!(text("https://ferrite.dev/docs"), "https://regiaengine.com/docs");
        // `telecom.ferrite.` is not a reverse-DNS `com`.
        assert_eq!(text("telecom.ferrite.x"), "telecom.regia.x");
    }

    #[test]
    fn mixed_case_is_reported_not_guessed() {
        let mut counts = Counts::default();
        assert_eq!(rename_text("a FeRRite b", &[], &mut counts), "a FeRRite b");
        assert_eq!(counts.leftovers, vec![(1, "FeRRite".to_owned())]);
    }

    #[test]
    fn markers_and_keep_texts_protect() {
        let keep = vec!["/srv/build-bulk/ferrite".to_owned()];
        let input = "ferrite_a\nlet x = \"FERRITE_\"; // rename-keep\n// rename-keep-begin\nferrite_b\n// rename-keep-end\nferrite_c /srv/build-bulk/ferrite/x\n";
        let mut counts = Counts::default();
        let out = rename_text(input, &keep, &mut counts);
        assert_eq!(
            out,
            "regia_a\nlet x = \"FERRITE_\"; // rename-keep\n// rename-keep-begin\nferrite_b\n// rename-keep-end\nregia_c /srv/build-bulk/ferrite/x\n"
        );
        assert_eq!(counts.kept, 3);
    }

    #[test]
    fn anchors_into_kept_files_are_protected() {
        let keep = vec!["decisions.md#".to_owned()];
        let input = "[ADR-206](decisions.md#adr-206-volume-in-ferrite_asset) and ferrite_asset\n";
        let out = rename_text(input, &keep, &mut Counts::default());
        assert_eq!(
            out,
            "[ADR-206](decisions.md#adr-206-volume-in-ferrite_asset) and regia_asset\n"
        );
    }

    #[test]
    fn paths() {
        assert_eq!(
            rename_path("crates/ferrite_core/src/lib.rs"),
            "crates/regia_core/src/lib.rs"
        );
        assert_eq!(
            rename_path("crates/ferrite/src/lib.rs"),
            "crates/regia/src/lib.rs"
        );
        assert_eq!(
            rename_path("samples/arena/Ferrite.toml"),
            "samples/arena/Regia.toml"
        );
        assert_eq!(
            rename_path("crates/ferrite_cli/templates/android/FerriteActivity.java"),
            "crates/regia_cli/templates/android/RegiaActivity.java"
        );
        assert_eq!(rename_path("docs/x/Ferrite/y"), "docs/x/Regia/y");
    }

    #[test]
    fn env_routing() {
        let mut counts = Counts::default();
        let out = route_env_reads(
            "let a = std::env::var(\"REGIA_X\");\nlet b = std::env::var_os(K);\nlet c = std::env::vars();\nlet d = std::env::var(E); // rename-keep\n",
            "regia_core",
            &[],
            &mut counts,
        );
        assert_eq!(
            out,
            "let a = regia_core::brand::env_var(\"REGIA_X\");\nlet b = regia_core::brand::env_var_os(K);\nlet c = std::env::vars();\nlet d = std::env::var(E); // rename-keep\n"
        );
        assert_eq!(counts.rules.get("route-env"), Some(&2));
    }

    #[test]
    fn globs() {
        assert!(glob_match("docs/evidence/**", "docs/evidence/a/b.png"));
        assert!(glob_match("docs/releases/v0.*.md", "docs/releases/v0.1.24.md"));
        assert!(!glob_match("docs/releases/v0.*.md", "docs/releases/sub/v0.1.md"));
        assert!(glob_match(
            "**/legacy-brand/**",
            "crates/regia_scene/tests/fixtures/legacy-brand/a.ron"
        ));
        assert!(glob_match(
            "xtask/src/rename_brand.rs",
            "xtask/src/rename_brand.rs"
        ));
        assert!(glob_match("a/?.rs", "a/b.rs"));
        assert!(!glob_match("a/*.rs", "a/b/c.rs"));
    }

    #[test]
    fn cargo_lock_is_resorted() {
        let lock = "# header\nversion = 4\n\n[[package]]\nname = \"bevy\"\nversion = \"1\"\ndependencies = [\n \"glam\",\n \"regia_core\",\n \"anyhow\",\n]\n\n[[package]]\nname = \"regia_core\"\nversion = \"0.1\"\n\n[[package]]\nname = \"glam\"\nversion = \"0.33\"\n";
        let sorted = sort_cargo_lock(lock);
        assert_eq!(
            sorted,
            "# header\nversion = 4\n\n[[package]]\nname = \"bevy\"\nversion = \"1\"\ndependencies = [\n \"anyhow\",\n \"glam\",\n \"regia_core\",\n]\n\n[[package]]\nname = \"glam\"\nversion = \"0.33\"\n\n[[package]]\nname = \"regia_core\"\nversion = \"0.1\"\n"
        );
        assert_eq!(sort_cargo_lock(&sorted), sorted, "sorting is idempotent");
    }

    #[test]
    fn decisions_note_is_inserted_once() {
        let once = insert_decisions_note("# Architecture decision records\n\nFormat.\n");
        assert!(
            once.starts_with(
                "# Architecture decision records\n\nThe project was named Ferrite before ADR-1388"
            )
        );
        assert_eq!(insert_decisions_note(&once), once);
    }

    #[test]
    fn builtin_keep_list_parses() {
        let keep = KeepList::builtin().expect("keep list");
        assert!(keep.keeps_content("xtask/src/rename_brand.rs", "xtask/src/rename_brand.rs"));
        assert!(keep.keeps_content(
            "crates/ferrite_core/src/brand.rs",
            "crates/regia_core/src/brand.rs"
        ));
        assert!(keep.keeps_file("docs/evidence/parity/2026-10-07/ferrite-do27-sheet.png", "x"));
        // A converted tree: both paths carry the new name.
        assert!(keep.keeps_content("crates/regia_core/src/brand.rs", "crates/regia_core/src/brand.rs"));
    }
}
