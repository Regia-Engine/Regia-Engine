// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask public-export`: the public repository's single-commit snapshot of one revision
//! (ADR-10229, spec 125 § 11).
//!
//! The export is the revision's tracked files (read from git, never the working tree) filtered by
//! the reviewed manifest `xtask/public-export.toml`:
//!
//! - `exclude` globs drop internal material (orchestration, farm bookkeeping, the builder manual);
//! - `scrub` rules rewrite private host names and paths in the files that are kept (in source
//!   files only inside `//` comments, so code and test data are never changed);
//! - `drop_self_hosted_jobs` removes workflow jobs that need the owner's self-hosted runners;
//! - the `overlay` directories are copied over the export (in order; by default over the root) (the user-facing `AGENTS.md` and
//!   `CLAUDE.md` replace the builder manual).
//!
//! `--check DIR` scans an export and reports every hit as `file:line [rule] detail`: the manifest's
//! `rule` patterns (private hosts and paths, farm talk, secrets, engine names), `hashed` terms
//! (names that must never be printed, stored only as BLAKE3 digests of their lowercase words),
//! files over `max_file_bytes`, missing `required` files, an `AGENTS.md` without the user-manual
//! marker line, broken relative links in the root entry files, and `include_str!`/`include_bytes!`
//! targets the export dropped. Rules print the matched text unless they are marked `redact`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;

/// The manifest's path in the repository.
pub const MANIFEST_PATH: &str = "xtask/public-export.toml";

/// Command-line arguments of `cargo xtask public-export`.
#[derive(clap::Args, Debug, Default)]
pub struct Args {
    /// Revision to export (a commit, tag or branch). Required with `--out`.
    #[arg(long)]
    pub rev: Option<String>,
    /// Directory to write the export to. It must not exist or be empty.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Scan an export directory and fail on any hit (may be combined with `--out` to check what was just written).
    #[arg(long)]
    pub check: Option<PathBuf>,
    /// Manifest to use instead of `xtask/public-export.toml` at the revision.
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// Git repository to read the revision from (default: this workspace).
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Print the `[[hashed]]` entries for a term that must never be printed (one for the spaced words, one joined).
    #[arg(long)]
    pub hash_term: Option<String>,
}

/// The reviewed export manifest (`xtask/public-export.toml`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Globs of tracked paths left out of the export.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Globs that are kept even when an `exclude` glob matches them.
    #[serde(default)]
    pub keep: Vec<String>,
    /// Tracked directories copied into the export (`from` is not exported itself), in order: a
    /// later overlay replaces an earlier one's file.
    #[serde(default)]
    pub overlay: Vec<Overlay>,
    /// Workflow globs whose `runs-on: [self-hosted, ...]` jobs are removed; a file left with no job is dropped.
    #[serde(default)]
    pub drop_self_hosted_jobs: Vec<String>,
    /// Text rewrites applied to exported files.
    #[serde(default)]
    pub scrub: Vec<Scrub>,
    /// Checks `--check` runs on every exported text file.
    #[serde(default)]
    pub rule: Vec<Rule>,
    /// Terms checked by digest only.
    #[serde(default)]
    pub hashed: Vec<Hashed>,
    /// Files the export must contain (the licence set).
    #[serde(default)]
    pub required: Vec<String>,
    /// Root files whose relative Markdown links must resolve inside the export.
    #[serde(default)]
    pub link_check: Vec<String>,
    /// A line the root `AGENTS.md` must contain (the user manual, not the builder manual).
    #[serde(default)]
    pub agents_marker: Option<String>,
    /// Rust files whose `include_str!`/`include_bytes!` text is test data the per-line check cannot
    /// tell from code (inside multi-line raw strings), each with a reviewed reason in the manifest.
    #[serde(default)]
    pub include_skip: Vec<String>,
    /// Largest file the export may hold, in bytes.
    #[serde(default)]
    pub max_file_bytes: Option<u64>,
}

/// One overlay directory.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overlay {
    /// Tracked directory whose files are copied.
    pub from: String,
    /// Export directory they land in (empty: the root).
    #[serde(default)]
    pub to: String,
}

/// One text rewrite.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scrub {
    /// Globs of the files it applies to (all text files when empty).
    #[serde(default)]
    pub paths: Vec<String>,
    /// Globs of files it never touches.
    #[serde(default)]
    pub skip: Vec<String>,
    /// Regular expression to replace.
    pub pattern: String,
    /// Replacement (`$1` style groups allowed).
    pub replace: String,
    /// Also rewrite code lines of Rust and WGSL files, not only `//` comments. Only for
    /// identifier-safe replacements applied the same way to a value and the tests that read it.
    #[serde(default)]
    pub code: bool,
}

/// One `--check` rule.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Short id printed in each hit.
    pub id: String,
    /// Regular expression that must not match.
    pub pattern: String,
    /// Globs the rule applies to (all text files when empty).
    #[serde(default)]
    pub paths: Vec<String>,
    /// Globs the rule does not apply to, each with a reviewed reason in the manifest.
    #[serde(default)]
    pub skip: Vec<String>,
    /// Exact matched strings that are fine (for example a placeholder address).
    #[serde(default)]
    pub allow: Vec<String>,
    /// A regular expression a whole match may fit to be fine (for example the reserved example domains).
    #[serde(default)]
    pub allow_pattern: Option<String>,
    /// Do not print the matched text in the report.
    #[serde(default)]
    pub redact: bool,
}

/// A term checked by digest: the BLAKE3 of its lowercase words joined by one space.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hashed {
    /// What the report calls it (never the term itself).
    pub label: String,
    /// Number of words in the term.
    pub words: usize,
    /// BLAKE3 hex digest of the term's first word.
    pub first: String,
    /// BLAKE3 hex digest of the whole term.
    pub blake3: String,
}

/// One `--check` finding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hit {
    /// Export-relative path with `/` separators.
    pub path: String,
    /// 1-based line, 0 for whole-file findings.
    pub line: usize,
    /// Rule id.
    pub rule: String,
    /// What matched (or the label of a redacted or hashed rule).
    pub detail: String,
}

impl std::fmt::Display for Hit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            write!(f, "{} [{}] {}", self.path, self.rule, self.detail)
        } else {
            write!(f, "{}:{} [{}] {}", self.path, self.line, self.rule, self.detail)
        }
    }
}

/// What an export wrote.
#[derive(Debug, Default, Clone)]
pub struct ExportSummary {
    /// Files written.
    pub files: usize,
    /// Bytes written.
    pub bytes: u64,
    /// Tracked files left out by `exclude`.
    pub excluded: usize,
    /// Files a scrub rule changed.
    pub scrubbed: usize,
    /// Workflow jobs removed.
    pub dropped_jobs: Vec<String>,
    /// The overlay came from the working tree because the revision has no overlay directory.
    pub overlay_from_worktree: bool,
}

/// Parse a manifest.
pub fn parse_manifest(text: &str) -> Result<Manifest> {
    toml::from_str(text).context("parse the public-export manifest")
}

/// A compiled glob (`**` any number of path segments, `*` within one segment, `?` one character).
#[derive(Debug, Clone)]
pub struct Glob(Regex);

impl Glob {
    /// Compile a glob. A trailing `/` means the directory and everything under it.
    pub fn new(glob: &str) -> Result<Self> {
        let glob = glob.strip_prefix("./").unwrap_or(glob);
        let owned;
        let glob = if let Some(dir) = glob.strip_suffix('/') {
            owned = format!("{dir}/**");
            owned.as_str()
        } else {
            glob
        };
        let mut re = String::from("^");
        let chars: Vec<char> = glob.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '*' && chars.get(i + 1) == Some(&'*') {
                let at_start = i == 0 || chars[i - 1] == '/';
                if at_start && chars.get(i + 2) == Some(&'/') {
                    re.push_str("(?:.*/)?");
                    i += 3;
                    continue;
                }
                re.push_str(".*");
                i += 2;
                continue;
            }
            match c {
                '*' => re.push_str("[^/]*"),
                '?' => re.push_str("[^/]"),
                c => re.push_str(&regex::escape(&c.to_string())),
            }
            i += 1;
        }
        re.push('$');
        Ok(Self(Regex::new(&re).with_context(|| format!("glob `{glob}`"))?))
    }

    /// Whether a `/`-separated relative path matches.
    pub fn matches(&self, path: &str) -> bool {
        self.0.is_match(path)
    }
}

fn globs(list: &[String]) -> Result<Vec<Glob>> {
    list.iter().map(|g| Glob::new(g)).collect()
}

fn any_match(globs: &[Glob], path: &str) -> bool {
    globs.iter().any(|g| g.matches(path))
}

/// Whether bytes look like text: valid UTF-8 with no NUL byte.
pub fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

fn is_comment_scrubbed(path: &str) -> bool {
    [".rs", ".wgsl"].iter().any(|ext| path.ends_with(ext))
}

struct ScrubRule {
    paths: Vec<Glob>,
    skip: Vec<Glob>,
    re: Regex,
    replace: String,
    code: bool,
}

/// Compiled scrub rules.
pub struct Scrubber {
    rules: Vec<ScrubRule>,
}

impl Scrubber {
    /// Compile a manifest's scrub rules.
    pub fn new(manifest: &Manifest) -> Result<Self> {
        let mut rules = Vec::new();
        for s in &manifest.scrub {
            let re = Regex::new(&s.pattern).with_context(|| format!("scrub pattern `{}`", s.pattern))?;
            rules.push(ScrubRule {
                paths: globs(&s.paths)?,
                skip: globs(&s.skip)?,
                re,
                replace: s.replace.clone(),
                code: s.code,
            });
        }
        Ok(Self { rules })
    }

    /// Rewrite one file's text. Rust and WGSL files are only rewritten inside `//` comment lines,
    /// except by rules marked `code`.
    pub fn apply(&self, path: &str, text: &str) -> Option<String> {
        let rules: Vec<&ScrubRule> = self
            .rules
            .iter()
            .filter(|r| (r.paths.is_empty() || any_match(&r.paths, path)) && !any_match(&r.skip, path))
            .collect();
        if rules.is_empty() {
            return None;
        }
        let code_file = is_comment_scrubbed(path);
        let mut out = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            let comment = !code_file || line.trim_start().starts_with("//");
            let mut l = line.to_string();
            for r in &rules {
                if (comment || r.code) && r.re.is_match(&l) {
                    l = r.re.replace_all(&l, r.replace.as_str()).into_owned();
                }
            }
            out.push_str(&l);
        }
        (out != text).then_some(out)
    }
}

/// Remove the jobs of a workflow whose `runs-on` names a self-hosted runner. Returns the new text
/// (None when no job is left) and the removed job ids.
pub fn drop_self_hosted(text: &str) -> (Option<String>, Vec<String>) {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some(jobs_at) = lines.iter().position(|l| l.trim_end() == "jobs:") else {
        return (Some(text.to_string()), Vec::new());
    };
    let job_header = |l: &str| {
        l.starts_with("  ")
            && !l.starts_with("   ")
            && !l.trim_start().starts_with('#')
            && l.trim_end().ends_with(':')
            && !l.trim().contains(' ')
    };
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(jobs_at + 1) {
        if !l.trim().is_empty() && !l.starts_with(' ') && !l.starts_with('#') {
            end = i;
            break;
        }
    }
    let starts: Vec<usize> = (jobs_at + 1..end).filter(|&i| job_header(lines[i])).collect();
    let mut out: Vec<&str> = lines[..=jobs_at].to_vec();
    out.extend_from_slice(&lines[jobs_at + 1..starts.first().copied().unwrap_or(end)]);
    let mut dropped = Vec::new();
    let mut kept = 0;
    for (n, &start) in starts.iter().enumerate() {
        let stop = starts.get(n + 1).copied().unwrap_or(end);
        let block = &lines[start..stop];
        let self_hosted = block
            .iter()
            .any(|l| l.trim_start().starts_with("runs-on:") && l.contains("self-hosted"));
        if self_hosted {
            dropped.push(lines[start].trim().trim_end_matches(':').to_string());
        } else {
            kept += 1;
            out.extend_from_slice(block);
        }
    }
    out.extend_from_slice(&lines[end..]);
    if kept == 0 && !starts.is_empty() {
        return (None, dropped);
    }
    (Some(out.concat()), dropped)
}

/// One tracked file at a revision.
#[derive(Debug, Clone)]
struct Entry {
    mode: String,
    sha: String,
    path: String,
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn tree(repo: &Path, rev: &str) -> Result<Vec<Entry>> {
    let raw = git(repo, &["ls-tree", "-r", "-z", "--full-tree", rev])?;
    let mut entries = Vec::new();
    for rec in raw.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let rec = std::str::from_utf8(rec).context("non-UTF-8 path in the tree")?;
        let (meta, path) = rec.split_once('\t').context("ls-tree record without a tab")?;
        let mut parts = meta.split(' ');
        let (Some(mode), Some(kind), Some(sha)) = (parts.next(), parts.next(), parts.next()) else {
            bail!("ls-tree record `{meta}`");
        };
        if kind != "blob" {
            // Submodules (`commit`) are not part of a snapshot.
            continue;
        }
        entries.push(Entry {
            mode: mode.to_string(),
            sha: sha.to_string(),
            path: path.to_string(),
        });
    }
    Ok(entries)
}

/// Reads blobs through one `git cat-file --batch` process.
struct Blobs {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
}

impl Blobs {
    fn new(repo: &Path) -> Result<Self> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("start git cat-file --batch")?;
        let out = BufReader::new(child.stdout.take().context("cat-file stdout")?);
        Ok(Self { child, out })
    }

    fn read(&mut self, sha: &str) -> Result<Vec<u8>> {
        let stdin = self.child.stdin.as_mut().context("cat-file stdin")?;
        writeln!(stdin, "{sha}")?;
        stdin.flush()?;
        let mut header = String::new();
        self.out.read_line(&mut header)?;
        let size: usize = header
            .split_whitespace()
            .nth(2)
            .and_then(|s| s.parse().ok())
            .with_context(|| format!("cat-file header `{}`", header.trim()))?;
        let mut data = vec![0; size];
        self.out.read_exact(&mut data)?;
        let mut nl = [0u8; 1];
        self.out.read_exact(&mut nl)?;
        Ok(data)
    }
}

impl Drop for Blobs {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

fn write_file(out: &Path, rel: &str, data: &[u8], mode: &str) -> Result<()> {
    let dest = out.join(rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    #[cfg(unix)]
    if mode == "120000" {
        let target = std::str::from_utf8(data).context("symlink target")?;
        std::os::unix::fs::symlink(target, &dest).with_context(|| format!("symlink {}", dest.display()))?;
        return Ok(());
    }
    fs::write(&dest, data).with_context(|| format!("write {}", dest.display()))?;
    #[cfg(unix)]
    if mode == "100755" {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

/// Export `rev` of `repo` into `out` under `manifest`.
pub fn export(repo: &Path, rev: &str, out: &Path, manifest: &Manifest) -> Result<ExportSummary> {
    if out.exists() && fs::read_dir(out)?.next().is_some() {
        bail!("{} exists and is not empty", out.display());
    }
    fs::create_dir_all(out)?;
    let exclude = globs(&manifest.exclude)?;
    let keep = globs(&manifest.keep)?;
    let jobs = globs(&manifest.drop_self_hosted_jobs)?;
    let scrubber = Scrubber::new(manifest)?;
    let overlay_dirs: Vec<(String, String)> = manifest
        .overlay
        .iter()
        .map(|o| {
            let to = o.to.trim_matches('/');
            (
                format!("{}/", o.from.trim_end_matches('/')),
                if to.is_empty() {
                    String::new()
                } else {
                    format!("{to}/")
                },
            )
        })
        .collect();
    let mut blobs = Blobs::new(repo)?;
    let mut summary = ExportSummary::default();
    let mut written: BTreeMap<String, (Vec<u8>, String)> = BTreeMap::new();
    let mut overlays: Vec<Vec<(String, Entry)>> = vec![Vec::new(); overlay_dirs.len()];
    'entries: for e in tree(repo, rev)? {
        for (n, (from, to)) in overlay_dirs.iter().enumerate() {
            if let Some(rel) = e.path.strip_prefix(from.as_str()) {
                overlays[n].push((format!("{to}{rel}"), e));
                continue 'entries;
            }
        }
        if any_match(&exclude, &e.path) && !any_match(&keep, &e.path) {
            summary.excluded += 1;
            continue;
        }
        let mut data = blobs.read(&e.sha)?;
        if e.mode != "120000" && is_text(&data) {
            let text = String::from_utf8_lossy(&data).into_owned();
            let mut text = match scrubber.apply(&e.path, &text) {
                Some(t) => {
                    summary.scrubbed += 1;
                    t
                }
                None => text,
            };
            if any_match(&jobs, &e.path) {
                let (kept, dropped) = drop_self_hosted(&text);
                summary
                    .dropped_jobs
                    .extend(dropped.into_iter().map(|j| format!("{}:{j}", e.path)));
                match kept {
                    Some(t) => text = t,
                    None => {
                        summary.excluded += 1;
                        continue;
                    }
                }
            }
            data = text.into_bytes();
        }
        written.insert(e.path, (data, e.mode));
    }
    if overlays.iter().all(Vec::is_empty) {
        // A revision from before the overlays existed: take them from the working tree.
        for (from, to) in &overlay_dirs {
            let dir = repo.join(from.trim_end_matches('/'));
            if dir.is_dir() {
                summary.overlay_from_worktree = true;
                for rel in walk(&dir)? {
                    let data = fs::read(dir.join(&rel)).with_context(|| format!("read overlay {rel}"))?;
                    written.insert(format!("{to}{rel}"), (data, "100644".to_string()));
                }
            }
        }
    }
    // In manifest order: a later overlay replaces an earlier one's file.
    for list in overlays {
        for (rel, e) in list {
            let mut data = blobs.read(&e.sha)?;
            // Overlay files are scrubbed like any other (a generated `llms-full.txt` mirrors the book).
            if e.mode != "120000"
                && is_text(&data)
                && let Some(t) = scrubber.apply(&rel, &String::from_utf8_lossy(&data))
            {
                summary.scrubbed += 1;
                data = t.into_bytes();
            }
            written.insert(rel, (data, e.mode));
        }
    }
    for (rel, (data, mode)) in &written {
        write_file(out, rel, data, mode)?;
        summary.files += 1;
        summary.bytes += data.len() as u64;
    }
    Ok(summary)
}

/// BLAKE3 hex of a string.
pub fn digest(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex().to_string()
}

/// The lowercase alphanumeric words of a line.
pub fn words(line: &str) -> Vec<String> {
    line.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The `[[hashed]]` entries for `term`: its words spaced, and joined into one word when it has several.
pub fn hashed_entries(label: &str, term: &str) -> Vec<Hashed> {
    let w = words(term);
    let mut out = Vec::new();
    if w.is_empty() {
        return out;
    }
    out.push(Hashed {
        label: label.to_string(),
        words: w.len(),
        first: digest(&w[0]),
        blake3: digest(&w.join(" ")),
    });
    if w.len() > 1 {
        let joined = w.concat();
        out.push(Hashed {
            label: label.to_string(),
            words: 1,
            first: digest(&joined),
            blake3: digest(&joined),
        });
    }
    out
}

struct CompiledRule {
    id: String,
    re: Regex,
    paths: Vec<Glob>,
    skip: Vec<Glob>,
    allow: BTreeSet<String>,
    allow_re: Option<Regex>,
    redact: bool,
}

/// The compiled checks of a manifest.
pub struct Checker {
    rules: Vec<CompiledRule>,
    hashed_first: HashMap<String, Vec<Hashed>>,
    include_re: Regex,
    manifest_dir_re: Regex,
    link_re: Regex,
    include_skip: Vec<Glob>,
    manifest: Manifest,
}

impl Checker {
    /// Compile a manifest's checks.
    pub fn new(manifest: &Manifest) -> Result<Self> {
        let mut rules = Vec::new();
        for r in &manifest.rule {
            rules.push(CompiledRule {
                id: r.id.clone(),
                re: Regex::new(&r.pattern).with_context(|| format!("rule `{}`", r.id))?,
                paths: globs(&r.paths)?,
                skip: globs(&r.skip)?,
                allow: r.allow.iter().cloned().collect(),
                allow_re: match &r.allow_pattern {
                    Some(p) => Some(
                        Regex::new(&format!("^(?:{p})$"))
                            .with_context(|| format!("rule `{}` allow_pattern", r.id))?,
                    ),
                    None => None,
                },
                redact: r.redact,
            });
        }
        let mut hashed_first: HashMap<String, Vec<Hashed>> = HashMap::new();
        for h in &manifest.hashed {
            hashed_first.entry(h.first.clone()).or_default().push(h.clone());
        }
        Ok(Self {
            rules,
            hashed_first,
            include_re: Regex::new(r#"include_(?:str|bytes)!\(\s*"([^"]+)"\s*\)"#)?,
            manifest_dir_re: Regex::new(
                r#"include_(?:str|bytes)!\(\s*concat!\(\s*env!\(\s*"CARGO_MANIFEST_DIR"\s*\)\s*,\s*"([^"]+)"\s*\)\s*\)"#,
            )?,
            link_re: Regex::new(r"\]\(([^)\s#]+)(?:#[^)]*)?\)")?,
            include_skip: globs(&manifest.include_skip)?,
            manifest: manifest.clone(),
        })
    }

    /// Check one text file's content.
    pub fn check_text(&self, path: &str, text: &str, hits: &mut Vec<Hit>) {
        let line_of = |offset: usize| text[..offset].bytes().filter(|&b| b == b'\n').count() + 1;
        for r in &self.rules {
            if (!r.paths.is_empty() && !any_match(&r.paths, path)) || any_match(&r.skip, path) {
                continue;
            }
            for m in r.re.find_iter(text) {
                if r.allow.contains(m.as_str()) || r.allow_re.as_ref().is_some_and(|a| a.is_match(m.as_str()))
                {
                    continue;
                }
                let detail = if r.redact {
                    "(redacted)".to_string()
                } else {
                    m.as_str().trim().to_string()
                };
                hits.push(Hit {
                    path: path.to_string(),
                    line: line_of(m.start()),
                    rule: r.id.clone(),
                    detail,
                });
            }
        }
        if !self.hashed_first.is_empty() {
            for (n, line) in text.lines().enumerate() {
                let w = words(line);
                for (i, word) in w.iter().enumerate() {
                    let Some(cands) = self.hashed_first.get(&digest(word)) else {
                        continue;
                    };
                    for h in cands {
                        if i + h.words <= w.len() && digest(&w[i..i + h.words].join(" ")) == h.blake3 {
                            hits.push(Hit {
                                path: path.to_string(),
                                line: n + 1,
                                rule: "hashed_term".to_string(),
                                detail: h.label.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    fn check_includes(&self, root: &Path, path: &str, text: &str, hits: &mut Vec<Hit>) {
        let file = root.join(path);
        let dir = file.parent().unwrap_or(root);
        let crate_dir = dir
            .ancestors()
            .find(|d| d.join("Cargo.toml").is_file())
            .unwrap_or(root);
        let mut check = |offset: usize, target: PathBuf, shown: &str| {
            if !in_code(text, offset) {
                return;
            }
            if !target.exists() {
                let line = text[..offset].bytes().filter(|&b| b == b'\n').count() + 1;
                hits.push(Hit {
                    path: path.to_string(),
                    line,
                    rule: "include_missing".to_string(),
                    detail: format!("`{shown}` is not in the export"),
                });
            }
        };
        for c in self.include_re.captures_iter(text) {
            let (Some(all), Some(rel)) = (c.get(0), c.get(1)) else {
                continue;
            };
            check(all.start(), dir.join(rel.as_str()), rel.as_str());
        }
        for c in self.manifest_dir_re.captures_iter(text) {
            let (Some(all), Some(rel)) = (c.get(0), c.get(1)) else {
                continue;
            };
            check(
                all.start(),
                crate_dir.join(rel.as_str().trim_start_matches('/')),
                rel.as_str(),
            );
        }
    }

    /// Scan an export directory.
    pub fn check_dir(&self, root: &Path) -> Result<Vec<Hit>> {
        let mut hits = Vec::new();
        let max = self.manifest.max_file_bytes.unwrap_or(50 * 1024 * 1024);
        let files = walk(root)?;
        for rel in &files {
            let rel = rel.clone();
            let full = root.join(&rel);
            let meta = fs::symlink_metadata(&full)?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.len() > max {
                hits.push(Hit {
                    path: rel.clone(),
                    line: 0,
                    rule: "file_too_large".to_string(),
                    detail: format!("{} bytes (limit {max})", meta.len()),
                });
            }
            let data = fs::read(&full).with_context(|| format!("read {}", full.display()))?;
            if !is_text(&data) {
                continue;
            }
            let text = String::from_utf8_lossy(&data);
            self.check_text(&rel, &text, &mut hits);
            if rel.ends_with(".rs") && !any_match(&self.include_skip, &rel) {
                self.check_includes(root, &rel, &text, &mut hits);
            }
        }
        if root.join(".gitignore").is_file() {
            for rel in ignored_files(root, &files)? {
                hits.push(Hit {
                    path: rel,
                    line: 0,
                    rule: "ignored_by_gitignore".to_string(),
                    detail: "the snapshot's .gitignore hides this file from `git add`".to_string(),
                });
            }
        }
        for req in &self.manifest.required {
            if !root.join(req).is_file() {
                hits.push(Hit {
                    path: req.clone(),
                    line: 0,
                    rule: "required_missing".to_string(),
                    detail: "required file is missing".to_string(),
                });
            }
        }
        if let Some(marker) = &self.manifest.agents_marker {
            let agents = fs::read_to_string(root.join("AGENTS.md")).unwrap_or_default();
            if !agents.lines().any(|l| l.trim() == marker.trim()) {
                hits.push(Hit {
                    path: "AGENTS.md".to_string(),
                    line: 0,
                    rule: "builder_manual".to_string(),
                    detail: "the root AGENTS.md lacks the user-manual marker line".to_string(),
                });
            }
        }
        for f in &self.manifest.link_check {
            let Ok(text) = fs::read_to_string(root.join(f)) else {
                continue;
            };
            let base = root.join(f);
            let dir = base.parent().unwrap_or(root);
            for (n, line) in text.lines().enumerate() {
                for c in self.link_re.captures_iter(line) {
                    let Some(target) = c.get(1).map(|m| m.as_str()) else {
                        continue;
                    };
                    if target.contains("://") || target.starts_with("mailto:") {
                        continue;
                    }
                    if !dir.join(target).exists() {
                        hits.push(Hit {
                            path: f.clone(),
                            line: n + 1,
                            rule: "link_missing".to_string(),
                            detail: format!("`{target}` is not in the export"),
                        });
                    }
                }
            }
        }
        hits.sort();
        Ok(hits)
    }
}

/// Whether `offset` of a Rust source is code: not on a `//` comment line and not inside a string
/// literal on its line (an `include_str!` written as test data is not an include).
pub fn in_code(text: &str, offset: usize) -> bool {
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let before = &text[start..offset];
    if before.trim_start().starts_with("//") {
        return false;
    }
    let mut quotes: usize = 0;
    let mut escaped = false;
    for c in before.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => quotes += 1,
            _ => {}
        }
    }
    quotes.is_multiple_of(2)
}

/// The files of `root` its own `.gitignore` files ignore (they would be left out of the public
/// repository's first commit), through `git check-ignore --no-index` on a throwaway bare repository.
pub fn ignored_files(root: &Path, files: &[String]) -> Result<Vec<String>> {
    static PROBE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = PROBE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let probe = std::env::temp_dir().join(format!(
        "regia-public-export-probe-{}-{n}.git",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&probe);
    let init = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&probe)
        .output()
        .context("run git init")?;
    if !init.status.success() {
        bail!("git init --bare {} failed", probe.display());
    }
    let mut child = Command::new("git")
        .env("GIT_DIR", &probe)
        .env("GIT_WORK_TREE", root)
        .current_dir(root)
        .args(["check-ignore", "--no-index", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("run git check-ignore")?;
    let mut stdin = child.stdin.take().context("check-ignore stdin")?;
    let list = files.join("\n");
    let writer = std::thread::spawn(move || stdin.write_all(list.as_bytes()));
    let out = child.wait_with_output().context("wait for git check-ignore")?;
    let _ = writer.join();
    let _ = fs::remove_dir_all(&probe);
    // Exit 1 means nothing is ignored; 128 is an error.
    if out.status.code() == Some(128) {
        bail!("git check-ignore failed in {}", root.display());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

/// Every file under `root`, as sorted `/`-separated relative paths (`.git` skipped).
pub fn walk(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for entry in
            fs::read_dir(root.join(&rel)).with_context(|| format!("read {}", root.join(&rel).display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            if rel.as_os_str().is_empty() && name == ".git" {
                continue;
            }
            let child = rel.join(&name);
            if entry.file_type()?.is_dir() {
                stack.push(child);
            } else {
                out.push(child.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn load_manifest(root: &Path, args: &Args) -> Result<Manifest> {
    let text = match (&args.manifest, &args.rev) {
        (Some(p), _) => fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?,
        (None, Some(rev)) => String::from_utf8(git(root, &["show", &format!("{rev}:{MANIFEST_PATH}")])?)
            .context("manifest is not UTF-8")?,
        (None, None) => fs::read_to_string(root.join(MANIFEST_PATH)).with_context(|| {
            format!("read {MANIFEST_PATH} (the public export does not ship it; pass --manifest)")
        })?,
    };
    parse_manifest(&text)
}

/// Run `cargo xtask public-export`.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let root = args.repo.as_deref().unwrap_or(root);
    if let Some(term) = &args.hash_term {
        for h in hashed_entries("(label)", term) {
            println!(
                "[[hashed]]\nlabel = \"{}\"\nwords = {}\nfirst = \"{}\"\nblake3 = \"{}\"\n",
                h.label, h.words, h.first, h.blake3
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    if args.out.is_none() && args.check.is_none() {
        bail!("pass --rev REV --out DIR to export, and/or --check DIR to scan an export");
    }
    let manifest = load_manifest(root, args)?;
    if let Some(out) = &args.out {
        let rev = args.rev.as_deref().context("--out needs --rev")?;
        let commit = String::from_utf8(git(root, &["rev-parse", &format!("{rev}^{{commit}}")])?)?;
        let s = export(root, commit.trim(), out, &manifest)?;
        println!(
            "public-export: {} at {}: {} files, {} bytes; {} tracked files excluded, {} scrubbed",
            out.display(),
            commit.trim(),
            s.files,
            s.bytes,
            s.excluded,
            s.scrubbed
        );
        if s.overlay_from_worktree {
            println!("  the revision has no overlay directory: the overlay came from the working tree");
        }
        for j in &s.dropped_jobs {
            println!("  dropped self-hosted job {j}");
        }
    }
    if let Some(dir) = &args.check {
        let hits = Checker::new(&manifest)?.check_dir(dir)?;
        let mut per_rule: BTreeMap<&str, usize> = BTreeMap::new();
        for h in &hits {
            println!("{h}");
            *per_rule.entry(h.rule.as_str()).or_default() += 1;
        }
        let rules: Vec<String> = per_rule.iter().map(|(r, n)| format!("{r} {n}")).collect();
        println!(
            "public-export --check {}: {} hits{}",
            dir.display(),
            hits.len(),
            if rules.is_empty() {
                String::new()
            } else {
                format!(" ({})", rules.join(", "))
            }
        );
        if !hits.is_empty() {
            return Ok(ExitCode::FAILURE);
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_gitignore_paths() {
        let g = Glob::new("docs/evidence/").expect("test fixture");
        assert!(g.matches("docs/evidence/a/b.log"));
        assert!(!g.matches("docs/evidence.md"));
        let g = Glob::new("**/LANE-REPORT.md").expect("test fixture");
        assert!(g.matches("LANE-REPORT.md"));
        assert!(g.matches("crates/x/LANE-REPORT.md"));
        let g = Glob::new("docs/releases/v*-*.md").expect("test fixture");
        assert!(g.matches("docs/releases/v0.1.28-public-export.md"));
        assert!(!g.matches("docs/releases/v0.6.1.md"));
        assert!(!g.matches("docs/releases/sub/v0.1-a.md"));
        let g = Glob::new("*.tsv").expect("test fixture");
        assert!(g.matches("branch-triage.tsv"));
        assert!(!g.matches("docs/x.tsv"));
    }

    #[test]
    fn scrub_rewrites_text_and_only_comments_in_code() {
        let m = parse_manifest(
            r#"
[[scrub]]
pattern = '(?i)\bhostzed\b'
replace = "the build host"

[[scrub]]
pattern = '203\.0\.113\.9'
replace = "192.0.2.9"
code = true
"#,
        )
        .expect("test fixture");
        let s = Scrubber::new(&m).expect("test fixture");
        assert_eq!(
            s.apply("a.md", "ran on Hostzed\n").as_deref(),
            Some("ran on the build host\n")
        );
        let code = "// ran on hostzed\nlet h = \"hostzed\";\n";
        assert_eq!(
            s.apply("a.rs", code).as_deref(),
            Some("// ran on the build host\nlet h = \"hostzed\";\n")
        );
        assert_eq!(s.apply("a.md", "nothing\n"), None);
        let both = "// at 203.0.113.9 on hostzed\nlet ip = \"203.0.113.9\"; // hostzed\n";
        assert_eq!(
            s.apply("x/b.rs", both).as_deref(),
            Some("// at 192.0.2.9 on the build host\nlet ip = \"192.0.2.9\"; // hostzed\n")
        );
    }

    #[test]
    fn self_hosted_jobs_are_dropped_and_empty_files_go() {
        let wf = "on:\n  push:\njobs:\n  linux:\n    runs-on: ubuntu-24.04\n    steps: []\n  windows:\n    # vm\n    runs-on: [self-hosted, Windows]\n    steps: []\n  docs:\n    runs-on: ubuntu-24.04\n";
        let (kept, dropped) = drop_self_hosted(wf);
        assert_eq!(dropped, vec!["windows".to_string()]);
        let kept = kept.expect("test fixture");
        assert!(kept.contains("  linux:") && kept.contains("  docs:"));
        assert!(!kept.contains("self-hosted"));
        let only = "jobs:\n  gpu:\n    runs-on: [self-hosted, Linux]\n";
        assert_eq!(drop_self_hosted(only).0, None);
    }

    #[test]
    fn hashed_terms_hit_without_printing_the_term() {
        let m = Manifest {
            hashed: hashed_entries("the test name", "Zed Quux Co"),
            ..Manifest::default()
        };
        let c = Checker::new(&m).expect("test fixture");
        let mut hits = Vec::new();
        c.check_text(
            "a.md",
            "Licensor: zed quux co.\nzedquuxco here\nzed quux only\n",
            &mut hits,
        );
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].line, hits[1].line), (1, 2));
        assert!(
            hits.iter()
                .all(|h| h.detail == "the test name" && !h.detail.contains("quux"))
        );
    }

    #[test]
    fn rules_respect_paths_skip_allow_and_redact() {
        let m = parse_manifest(
            r#"
[[rule]]
id = "mail"
pattern = '[a-z]+@[a-z]+\.[a-z]+'
allow = ["contact@example.com"]
allow_pattern = '[a-z]+@[a-z]+\.invalid'
skip = ["vendor/**"]

[[rule]]
id = "token"
pattern = 'tok_[A-Za-z0-9]{8}'
paths = ["**/*.md"]
redact = true
"#,
        )
        .expect("test fixture");
        let c = Checker::new(&m).expect("test fixture");
        let mut hits = Vec::new();
        // Built at run time so this file carries no address the real manifest would flag.
        let me = format!("me@{}.net", "home");
        c.check_text(
            "a.md",
            &format!("write contact@example.com or {me}\ntok_ABCDEFGH t@x.invalid\n"),
            &mut hits,
        );
        c.check_text("vendor/x.md", &format!("{me}\n"), &mut hits);
        c.check_text("a.rs", "tok_ABCDEFGH\n", &mut hits);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].detail, me);
        assert_eq!(
            (hits[1].rule.as_str(), hits[1].line, hits[1].detail.as_str()),
            ("token", 2, "(redacted)")
        );
    }

    #[test]
    fn check_dir_reports_missing_licence_marker_includes_links_and_size() {
        let dir = regia_test_utils::unique_temp_dir("public-export-check");
        fs::create_dir_all(dir.join("c/src")).expect("test fixture");
        fs::write(dir.join("c/Cargo.toml"), "[package]\n").expect("test fixture");
        fs::write(
            dir.join("c/src/lib.rs"),
            "const A: &str = include_str!(\"../gone.md\");\nconst B: &str = include_str!(\"lib.rs\");\n\
             //! include_str!(\"../doc.md\")\nlet t = \"include_str!(\\\"../fixture.md\\\")\";\n",
        )
        .expect("test fixture");
        fs::write(
            dir.join("AGENTS.md"),
            "# builder\nSee [install](book/start/install.md).\n",
        )
        .expect("test fixture");
        fs::write(dir.join("big.bin"), vec![0u8; 2048]).expect("test fixture");
        fs::write(dir.join(".gitignore"), "/hidden/\n").expect("test fixture");
        fs::create_dir_all(dir.join("hidden")).expect("test fixture");
        fs::write(dir.join("hidden/skill.md"), "x\n").expect("test fixture");
        let m = Manifest {
            required: vec!["LICENSE".to_string()],
            agents_marker: Some("<!-- user manual -->".to_string()),
            link_check: vec!["AGENTS.md".to_string()],
            max_file_bytes: Some(1024),
            ..Manifest::default()
        };
        let hits = Checker::new(&m)
            .expect("test fixture")
            .check_dir(&dir)
            .expect("test fixture");
        let rules: Vec<&str> = hits.iter().map(|h| h.rule.as_str()).collect();
        assert_eq!(
            rules,
            vec![
                "builder_manual",
                "link_missing",
                "required_missing",
                "file_too_large",
                "include_missing",
                "ignored_by_gitignore"
            ],
            "{hits:?}"
        );
        assert_eq!((hits[1].line, hits[4].line), (2, 1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_filters_scrubs_and_overlays_a_revision() {
        let repo = regia_test_utils::unique_temp_dir("public-export-repo");
        let put = |rel: &str, text: &str| {
            let p = repo.join(rel);
            fs::create_dir_all(p.parent().expect("test fixture")).expect("test fixture");
            fs::write(p, text).expect("test fixture");
        };
        put("AGENTS.md", "builder manual\n");
        put("secret/notes.txt", "internal\n");
        put("docs/a.md", "ran on hostzed\n");
        put("ov1/AGENTS.md", "user one\n");
        put("ov1/README.md", "readme one\n");
        put("ov2/AGENTS.md", "user two on hostzed\n");
        put("sk/setup/SKILL.md", "skill\n");
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .expect("test fixture");
            assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "fixture"]);
        let m = parse_manifest(
            r#"
exclude = ["secret/", "AGENTS.md"]

[[overlay]]
from = "ov1"

[[overlay]]
from = "ov2"

[[overlay]]
from = "sk"
to = ".claude/skills"

[[scrub]]
pattern = 'hostzed'
replace = "the build host"
"#,
        )
        .expect("test fixture");
        let out = regia_test_utils::unique_temp_dir("public-export-out");
        let _ = fs::remove_dir_all(&out);
        let s = export(&repo, "HEAD", &out, &m).expect("test fixture");
        let files = walk(&out).expect("test fixture");
        assert_eq!(
            files,
            vec![
                ".claude/skills/setup/SKILL.md",
                "AGENTS.md",
                "README.md",
                "docs/a.md"
            ],
            "{files:?}"
        );
        assert_eq!(
            fs::read_to_string(out.join("AGENTS.md")).expect("test fixture"),
            "user two on the build host\n"
        );
        assert_eq!(
            fs::read_to_string(out.join("docs/a.md")).expect("test fixture"),
            "ran on the build host\n"
        );
        assert_eq!(
            (s.files, s.excluded, s.scrubbed, s.overlay_from_worktree),
            (4, 2, 2, false)
        );
        assert!(
            export(&repo, "HEAD", &out, &m).is_err(),
            "a non-empty output directory is refused"
        );
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
    }
}
