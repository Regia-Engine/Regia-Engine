// SPDX-License-Identifier: MPL-2.0
//! Root evidence for content closures (CT-25, ADR-8477): one bounded resolver that the chunk cook
//! (`[project] startup_level`, `[cook.chunks.*] entry`) and named build profiles
//! (`[profiles.*] entry_levels` / `include`) both call, so equivalent declarations reach the same
//! cooked roots and every root carries the reason it exists.
//!
//! The resolver starts from [`RootDecl`]s plus the implicit rules (every top-level `ui/*.ui.ron`
//! loads at start; every level the walk reaches ships its procedural bakes) and walks the raw files
//! those name. Each raw file is read once and its references are extracted by format:
//!
//! - **UI layouts** (`*.ui.ron`): the typed UI dependency extractor
//!   ([`crate::registry_bridge::ui_layout_dependencies`]), so a label or node name that happens to
//!   spell an asset path is not a dependency; `use` paths of other layouts are followed.
//! - **Levels** (`*.level.ron` and the files of a `*.level.d/`): every RON string literal, decoded
//!   (escapes, `\u{..}`, raw strings), that names a raw file, a cooked asset path, an asset id or a
//!   level id. Reflected component data is untyped text in a level, so the decoded strings are the
//!   level's references.
//! - **Unknown text formats**: the legacy generic quoted-string scan, labeled
//!   [`Extraction::Fallback`] and reported, never counted as typed coverage.
//! - **Binary raw files**: not scanned ([`Extraction::Binary`]).
//!
//! A malformed level or UI layout, an unreadable file, or a declared level that does not exist is an
//! error ([`RootError`]); the resolver never turns them into a smaller closure. Work is bounded: each
//! raw file is read at most once and the [`RootCounters`] record what the walk did. Output is
//! independent of registry and directory order (ordered maps, canonical declaration order, ids sorted).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use regia_asset::AssetId;
use serde::Serialize;

/// The largest raw file the resolver reads (a level or layout above it is a `content_root_limit`
/// error rather than an unbounded read).
pub const ROOT_FILE_MAX_BYTES: u64 = 256 << 20;

/// Why a root (or a raw file in the walk) is part of the closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    /// `[project] startup_level`: the level the game boots into.
    StartupLevel,
    /// A build profile's `entry_levels`.
    EntryLevel,
    /// A top-level `ui/*.ui.ron`, loaded at start by the runtime (ADR-3148).
    StartupUi,
    /// An explicit dynamic root: a profile `include` glob or a chunk `entry` glob (content the game
    /// loads by name, which no file references).
    Declared,
    /// A procedural bake of a level the walk reached (spec 62 § 8).
    LevelBake,
    /// Named by a raw file the walk read (`via`).
    Reference,
}

impl RootKind {
    /// The report spelling: `startup_level`, `entry_level`, `startup_ui`, `declared`, `level_bake`,
    /// `reference`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::StartupLevel => "startup_level",
            Self::EntryLevel => "entry_level",
            Self::StartupUi => "startup_ui",
            Self::Declared => "declared",
            Self::LevelBake => "level_bake",
            Self::Reference => "reference",
        }
    }
}

/// How a raw file's references were extracted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Extraction {
    /// A level: decoded RON string literals resolved against the project.
    Level,
    /// A UI layout: the typed UI dependency extractor.
    UiLayout,
    /// An unknown text format: the conservative generic quoted-string scan. Not typed coverage.
    Fallback,
    /// Not text: nothing to scan.
    Binary,
}

impl Extraction {
    /// The report spelling: `level`, `ui_layout`, `fallback`, `binary`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Level => "level",
            Self::UiLayout => "ui_layout",
            Self::Fallback => "fallback",
            Self::Binary => "binary",
        }
    }
}

/// One declared root: a level path ([`RootKind::StartupLevel`], [`RootKind::EntryLevel`]) or a path
/// glob under `assets/` ([`RootKind::Declared`]).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RootDecl {
    /// What kind of declaration this is.
    pub kind: RootKind,
    /// The configuration item that declares it (`[project] startup_level`,
    /// `[profiles.demo] include[0]`, `[cook.chunks.dlc] entry[1]`).
    pub declared_by: String,
    /// The level path or glob, as written.
    pub pattern: String,
}

impl RootDecl {
    /// A declaration of `kind` by `declared_by`.
    #[must_use]
    pub fn new(kind: RootKind, declared_by: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            kind,
            declared_by: declared_by.into(),
            pattern: pattern.into(),
        }
    }
}

/// Why one root or raw file is in the closure: the first reason the walk found (declarations in
/// canonical order, then breadth first).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RootWhy {
    /// The kind of reason.
    pub kind: RootKind,
    /// The declaration the reason traces back to.
    pub declared_by: String,
    /// The raw file (path under `assets/`) that named it, for references and bakes.
    pub via: Option<String>,
    /// Needed at boot: reached from the startup level, an entry level or a startup layout without
    /// passing through another level (a level a level names is a travel target, not a boot need).
    /// The chunk cook pins boot roots in `base`.
    pub boot: bool,
}

/// What a resolution did; every counter is bounded by the project's raw files and their bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RootCounters {
    /// Raw files read (each at most once).
    pub files_read: usize,
    /// Bytes read from them.
    pub bytes_read: u64,
    /// Strings (or typed fields) examined as possible references.
    pub strings_examined: usize,
    /// Of those, the ones that resolved to project content.
    pub references: usize,
    /// Largest walk queue.
    pub peak_queue: usize,
}

/// The result of [`resolve_roots`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RootEvidence {
    /// Cooked roots (the seeds of the cooked dependency closure) and why each exists.
    pub cooked: BTreeMap<AssetId, RootWhy>,
    /// Raw files (paths under `assets/`) the closure ships, and why.
    pub raw: BTreeMap<String, RootWhy>,
    /// How each raw file read was scanned.
    pub scans: BTreeMap<String, Extraction>,
    /// Declarations that matched nothing (`declared_by`), sorted.
    pub unmatched: Vec<String>,
    /// Declarations (`declared_by`) that matched at least one raw file or cooked asset.
    pub matched: BTreeSet<String>,
    /// Work done.
    pub counters: RootCounters,
}

impl RootEvidence {
    /// Every cooked root id, sorted.
    #[must_use]
    pub fn cooked_ids(&self) -> Vec<AssetId> {
        self.cooked.keys().copied().collect()
    }

    /// The cooked roots needed at boot (the chunk cook's startup pin), sorted.
    #[must_use]
    pub fn boot_ids(&self) -> Vec<AssetId> {
        self.cooked
            .iter()
            .filter(|(_, why)| why.boot)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Raw files scanned with the generic fallback (unknown formats), sorted.
    #[must_use]
    pub fn fallback_files(&self) -> Vec<String> {
        self.scans
            .iter()
            .filter(|(_, e)| **e == Extraction::Fallback)
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// Whether every scanned raw file had a typed extractor (no fallback scan was needed).
    #[must_use]
    pub fn typed_complete(&self) -> bool {
        !self.scans.values().any(|e| *e == Extraction::Fallback)
    }

    /// The report form (`regia assets chunks --json` `roots`, `regia package --dry-run`, the
    /// editor's Content Chunks state): records sorted by target, `path_of` names a cooked id.
    #[must_use]
    pub fn report(&self, path_of: &dyn Fn(AssetId) -> String) -> RootReport {
        let mut records: Vec<RootRecord> = self
            .cooked
            .iter()
            .map(|(id, why)| RootRecord {
                target: path_of(*id),
                id: Some(id.to_string()),
                why: why.clone(),
            })
            .chain(self.raw.iter().map(|(path, why)| RootRecord {
                target: format!("assets/{path}"),
                id: None,
                why: why.clone(),
            }))
            .collect();
        records.sort_by(|a, b| (&a.target, &a.id).cmp(&(&b.target, &b.id)));
        let count = |e: Extraction| self.scans.values().filter(|x| **x == e).count();
        RootReport {
            records,
            coverage: RootCoverage {
                typed: count(Extraction::Level) + count(Extraction::UiLayout),
                binary: count(Extraction::Binary),
                fallback: self.fallback_files(),
                complete: self.typed_complete(),
            },
            unmatched: self.unmatched.clone(),
            counters: self.counters,
        }
    }
}

/// One root or raw file in a [`RootReport`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RootRecord {
    /// The cooked asset's registry path, or `assets/<raw path>`.
    pub target: String,
    /// The cooked asset id (raw files have none).
    pub id: Option<String>,
    /// Why it is in the closure.
    #[serde(flatten)]
    pub why: RootWhy,
}

/// How much of the walk had a typed extractor.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RootCoverage {
    /// Raw files read with a typed extractor (levels, UI layouts).
    pub typed: usize,
    /// Binary raw files (not scanned).
    pub binary: usize,
    /// Raw files of unknown text formats read with the conservative generic scan.
    pub fallback: Vec<String>,
    /// `true` when no fallback scan was needed.
    pub complete: bool,
}

/// The root report of a closure.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RootReport {
    /// Every root and raw file with its reason, sorted by target.
    pub records: Vec<RootRecord>,
    /// Extraction coverage.
    pub coverage: RootCoverage,
    /// Declarations that matched nothing.
    pub unmatched: Vec<String>,
    /// Work counters.
    pub counters: RootCounters,
}

/// Why root resolution failed. Every variant fails a cook or a package; none is a smaller closure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RootError {
    /// A declared level is neither a raw file nor a cooked asset.
    #[error("content_root_missing: {kind} `{pattern}` not found under {assets} (declared by {declared_by})")]
    Missing {
        /// `startup level` or `entry level`.
        kind: &'static str,
        /// The declaring item.
        declared_by: String,
        /// The path as written.
        pattern: String,
        /// The assets directory searched.
        assets: String,
    },
    /// A raw file in the closure could not be read.
    #[error("content_root_unreadable: assets/{path}: {message}")]
    Unreadable {
        /// Path under `assets/`.
        path: String,
        /// The IO error.
        message: String,
    },
    /// A level or UI layout in the closure does not parse.
    #[error("content_root_malformed: assets/{path}: {message}")]
    Malformed {
        /// Path under `assets/`.
        path: String,
        /// What is wrong.
        message: String,
    },
    /// A raw file in the closure is larger than [`ROOT_FILE_MAX_BYTES`].
    #[error("content_root_limit: assets/{path} is {bytes} bytes (the limit is 256 MiB)")]
    TooLarge {
        /// Path under `assets/`.
        path: String,
        /// Its size.
        bytes: u64,
    },
}

impl RootError {
    /// The stable code: `content_root_missing`, `content_root_unreadable`, `content_root_malformed`
    /// or `content_root_limit`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing { .. } => "content_root_missing",
            Self::Unreadable { .. } => "content_root_unreadable",
            Self::Malformed { .. } => "content_root_malformed",
            Self::TooLarge { .. } => "content_root_limit",
        }
    }

    /// The file or declaration the error is about (`assets/<path>`, or the declared pattern).
    #[must_use]
    pub fn subject(&self) -> String {
        match self {
            Self::Missing { pattern, .. } => pattern.clone(),
            Self::Unreadable { path, .. } | Self::Malformed { path, .. } | Self::TooLarge { path, .. } => {
                format!("assets/{path}")
            }
        }
    }
}

/// What the resolver resolves against.
pub struct RootInputs<'a> {
    /// `<project>/assets`.
    pub assets_dir: PathBuf,
    /// Every raw file under `assets/` with its size ([`raw_universe`]).
    pub raw: &'a BTreeMap<String, u64>,
    /// Every cooked asset: id and registry/manifest path (`assets/...`, `path#sub` allowed).
    pub cooked: &'a [(AssetId, String)],
    /// Paths (under `assets/`) a profile excludes; the walk neither ships nor enters them.
    pub exclude: &'a dyn Fn(&str) -> bool,
}

/// `assets/x`, `assets://x` and `./x` all name `x` under `assets/`.
#[must_use]
pub fn normalize_asset_path(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    for prefix in ["assets://", "./", "assets/"] {
        if let Some(rest) = p.strip_prefix(prefix) {
            p = rest.to_string();
        }
    }
    p
}

/// Glob match over `/`-separated paths: `*` any run without `/`, `?` one character, `**` anything
/// (the pattern is normalized like a path first).
#[must_use]
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn rec(p: &[u8], s: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'*', rest)) if rest.first() == Some(&b'*') => {
                let rest = &rest[1..];
                (0..=s.len()).any(|i| rec(rest, &s[i..]))
            }
            Some((b'*', rest)) => (0..=s.len()).any(|i| rec(rest, &s[i..]) && !s[..i].contains(&b'/')),
            Some((b'?', rest)) => s.first().is_some_and(|c| *c != b'/') && rec(rest, &s[1..]),
            Some((c, rest)) => s.first() == Some(c) && rec(rest, &s[1..]),
        }
    }
    rec(normalize_asset_path(pattern).as_bytes(), path.as_bytes())
}

/// Source extensions the cook turns into cooked assets (they are not raw files).
#[must_use]
pub fn is_cookable(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e).to_ascii_lowercase();
    match ext.as_str() {
        "glb" | "gltf" | "png" | "jpg" | "jpeg" | "fgiv" | "flmap" | "cube" | "pgout" => true,
        "ron" => name.ends_with(".material.ron") || name.ends_with(".scatter.ron"),
        _ => false,
    }
}

/// A `.ui.ron` directly in `assets/ui/` (`rel` relative to `assets/`): the runtime loads every one
/// at start.
#[must_use]
pub fn is_startup_ui_layout(rel: &str) -> bool {
    rel.strip_prefix("ui/")
        .is_some_and(|name| !name.contains('/') && name.ends_with(".ui.ron"))
}

/// Every raw file under `<project>/assets/` (`.meta` sidecars and cookable sources excluded) as
/// `(path under assets/, size)`.
///
/// # Errors
/// [`RootError::Unreadable`] when the walk cannot list a directory or stat a file.
pub fn raw_universe(project: &Path) -> Result<BTreeMap<String, u64>, RootError> {
    let assets = project.join("assets");
    let mut out = BTreeMap::new();
    if !assets.is_dir() {
        return Ok(out);
    }
    let unreadable = |path: String, e: &dyn std::fmt::Display| RootError::Unreadable {
        path,
        message: e.to_string(),
    };
    for entry in walkdir::WalkDir::new(&assets).sort_by_file_name() {
        let entry = entry.map_err(|e| {
            let rel = e
                .path()
                .and_then(|p| p.strip_prefix(&assets).ok())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            unreadable(rel, &e)
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(&assets) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.ends_with(".meta") || is_cookable(&rel) {
            continue;
        }
        let size = entry.metadata().map_err(|e| unreadable(rel.clone(), &e))?.len();
        out.insert(rel, size);
    }
    Ok(out)
}

/// The level suffixes (`regia_scene::level_dir`).
const LEVEL_FILE: &str = regia_scene::level_dir::LEVEL_FILE_SUFFIX;
const LEVEL_DIR: &str = regia_scene::level_dir::LEVEL_DIR_SUFFIX;

/// The raw files that hold `key`: the file itself or, for a level, whichever form exists
/// (`maps/x.level.ron` and `maps/x.level.d` name one level; the directory's files hold it).
fn raw_files_of(raw: &BTreeMap<String, u64>, key: &str) -> Vec<String> {
    if raw.contains_key(key) {
        return vec![key.to_string()];
    }
    let Some(stem) = key
        .strip_suffix(LEVEL_FILE)
        .or_else(|| key.strip_suffix(LEVEL_DIR))
    else {
        return Vec::new();
    };
    let file = format!("{stem}{LEVEL_FILE}");
    if raw.contains_key(&file) {
        return vec![file];
    }
    let prefix = format!("{stem}{LEVEL_DIR}/");
    raw.range(prefix.clone()..)
        .take_while(|(path, _)| path.starts_with(&prefix))
        .map(|(path, _)| path.clone())
        .collect()
}

/// The level a raw file holds (`maps/x.level.ron`, or `maps/x.level.d` for a file inside it).
fn level_of_raw(rel: &str) -> Option<&str> {
    if rel.ends_with(LEVEL_FILE) {
        return Some(rel);
    }
    let marker = format!("{LEVEL_DIR}/");
    rel.find(&marker).map(|at| &rel[..at + LEVEL_DIR.len()])
}

/// The cooked paths (normalized) under the bake directory of the level `key` (`maps/x.procgen/`).
fn level_bakes(by_path: &BTreeMap<String, Vec<AssetId>>, key: &str) -> Vec<String> {
    let prefix = format!(
        "{}/",
        regia_procgen::world::level_bake_dir(Path::new(key))
            .to_string_lossy()
            .replace('\\', "/")
    );
    by_path
        .range(prefix.clone()..)
        .take_while(|(p, _)| p.starts_with(&prefix))
        .map(|(p, _)| p.clone())
        .collect()
}

/// The decoded string literals of RON text (also JSON), in order. Fails on anything that is not
/// well formed at the token level: unterminated strings, characters or block comments, bad escapes,
/// unbalanced brackets. Levels treat that as malformed; unknown formats fall back to the undecoded
/// scan.
///
/// # Errors
/// The first token error, with its byte offset.
pub fn ron_string_literals(text: &str) -> Result<Vec<String>, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut depth: Vec<u8> = Vec::new();
    let mut i = 0;
    let mut prev_ident = false;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                prev_ident = false;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let start = i;
                let mut nest = 1usize;
                i += 2;
                while nest > 0 {
                    match (bytes.get(i), bytes.get(i + 1)) {
                        (None, _) => return Err(format!("unterminated block comment at byte {start}")),
                        (Some(b'/'), Some(b'*')) => {
                            nest += 1;
                            i += 2;
                        }
                        (Some(b'*'), Some(b'/')) => {
                            nest -= 1;
                            i += 2;
                        }
                        _ => i += 1,
                    }
                }
                prev_ident = false;
            }
            b'"' => {
                let (s, next) = decode_string(text, i + 1)?;
                out.push(s);
                i = next;
                prev_ident = false;
            }
            b'r' if !prev_ident && matches!(bytes.get(i + 1), Some(b'"' | b'#')) => {
                let mut hashes = 0;
                let mut j = i + 1;
                while bytes.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if bytes.get(j) != Some(&b'"') {
                    // `r#ident` (a raw identifier).
                    i = j;
                    prev_ident = true;
                    continue;
                }
                let close = format!("\"{}", "#".repeat(hashes));
                let body = j + 1;
                let Some(end) = text[body..].find(&close) else {
                    return Err(format!("unterminated raw string at byte {i}"));
                };
                out.push(text[body..body + end].to_owned());
                i = body + end + close.len();
                prev_ident = false;
            }
            b'\'' => {
                let start = i;
                i += 1;
                match bytes.get(i) {
                    Some(b'\\') => {
                        let (_, next) = decode_escape(text, i + 1)?;
                        i = next;
                    }
                    Some(_) => {
                        let ch = text[i..].chars().next().ok_or("truncated character")?;
                        i += ch.len_utf8();
                    }
                    None => return Err(format!("unterminated character at byte {start}")),
                }
                if bytes.get(i) != Some(&b'\'') {
                    return Err(format!("unterminated character at byte {start}"));
                }
                i += 1;
                prev_ident = false;
            }
            b'(' | b'[' | b'{' => {
                depth.push(c);
                i += 1;
                prev_ident = false;
            }
            b')' | b']' | b'}' => {
                let open = match c {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                if depth.pop() != Some(open) {
                    return Err(format!("unbalanced `{}` at byte {i}", c as char));
                }
                i += 1;
                prev_ident = false;
            }
            _ => {
                prev_ident = c.is_ascii_alphanumeric() || c == b'_';
                i += 1;
            }
        }
    }
    if let Some(open) = depth.last() {
        return Err(format!("unclosed `{}` at end of text", *open as char));
    }
    Ok(out)
}

/// Decodes one escape starting after the backslash at `i`; returns the character and the next index.
fn decode_escape(text: &str, i: usize) -> Result<(char, usize), String> {
    let bytes = text.as_bytes();
    let bad = || format!("invalid escape at byte {}", i.saturating_sub(1));
    let hex = |s: &str| u32::from_str_radix(s, 16).ok().and_then(char::from_u32);
    match bytes.get(i) {
        Some(b'n') => Ok(('\n', i + 1)),
        Some(b'r') => Ok(('\r', i + 1)),
        Some(b't') => Ok(('\t', i + 1)),
        Some(b'0') => Ok(('\0', i + 1)),
        Some(b'\\') => Ok(('\\', i + 1)),
        Some(b'"') => Ok(('"', i + 1)),
        Some(b'\'') => Ok(('\'', i + 1)),
        Some(b'/') => Ok(('/', i + 1)),
        Some(b'x') => {
            let s = text.get(i + 1..i + 3).ok_or_else(bad)?;
            let v = u8::from_str_radix(s, 16).map_err(|_| bad())?;
            if v > 0x7f {
                return Err(bad());
            }
            Ok((char::from(v), i + 3))
        }
        Some(b'u') if bytes.get(i + 1) == Some(&b'{') => {
            let end = text[i + 2..].find('}').ok_or_else(bad)? + i + 2;
            let digits = &text[i + 2..end];
            if digits.is_empty() || digits.len() > 6 {
                return Err(bad());
            }
            Ok((hex(digits).ok_or_else(bad)?, end + 1))
        }
        Some(b'u') => {
            let s = text.get(i + 1..i + 5).ok_or_else(bad)?;
            Ok((hex(s).ok_or_else(bad)?, i + 5))
        }
        _ => Err(bad()),
    }
}

/// Decodes a `"` string whose body starts at `start`; returns it and the index after the quote.
fn decode_string(text: &str, start: usize) -> Result<(String, usize), String> {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = start;
    let mut run = start;
    loop {
        match bytes.get(i) {
            None => return Err(format!("unterminated string at byte {}", start - 1)),
            Some(b'"') => {
                out.push_str(&text[run..i]);
                return Ok((out, i + 1));
            }
            Some(b'\\') => {
                out.push_str(&text[run..i]);
                let (ch, next) = decode_escape(text, i + 1)?;
                out.push(ch);
                i = next;
                run = i;
            }
            Some(_) => i += 1,
        }
    }
}

/// The double-quoted substrings of `text`, undecoded (the legacy generic scan of unknown formats).
fn quoted_strings(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() && bytes[j] != b'"' {
            j += if bytes[j] == b'\\' { 2 } else { 1 };
        }
        if j >= bytes.len() {
            break;
        }
        out.push(&text[start..j]);
        i = j + 1;
    }
    out
}

/// The state of one resolution: indexes, the evidence so far and the two walk queues (boot first).
struct Walk<'a> {
    inputs: &'a RootInputs<'a>,
    by_path: BTreeMap<String, Vec<AssetId>>,
    by_id_text: BTreeMap<String, AssetId>,
    path_of_id: BTreeMap<AssetId, String>,
    levels_by_id: BTreeMap<String, String>,
    ev: RootEvidence,
    boot_queue: VecDeque<String>,
    later_queue: VecDeque<String>,
}

impl<'a> Walk<'a> {
    fn new(inputs: &'a RootInputs<'a>) -> Self {
        let mut by_path: BTreeMap<String, Vec<AssetId>> = BTreeMap::new();
        let mut by_id_text = BTreeMap::new();
        let mut path_of_id = BTreeMap::new();
        for (id, path) in inputs.cooked {
            // glTF sub-assets are `path#name`; every one belongs to the file path.
            let key = normalize_asset_path(path.split('#').next().unwrap_or(path));
            by_path.entry(key).or_default().push(*id);
            by_id_text.insert(id.to_string().to_lowercase(), *id);
            path_of_id.insert(*id, normalize_asset_path(path));
        }
        for ids in by_path.values_mut() {
            ids.sort();
            ids.dedup();
        }
        let levels_by_id = if inputs.assets_dir.is_dir() {
            regia_framework::level_assets::LevelSourceIndex::scan(&inputs.assets_dir)
                .levels
                .iter()
                .filter_map(|(id, path)| {
                    let rel = path.strip_prefix(&inputs.assets_dir).ok()?;
                    Some((
                        id.to_string().to_lowercase(),
                        rel.to_string_lossy().replace('\\', "/"),
                    ))
                })
                .collect()
        } else {
            BTreeMap::new()
        };
        Self {
            inputs,
            by_path,
            by_id_text,
            path_of_id,
            levels_by_id,
            ev: RootEvidence::default(),
            boot_queue: VecDeque::new(),
            later_queue: VecDeque::new(),
        }
    }

    /// Adds a raw file (once) and queues it for reading.
    fn reach_raw(&mut self, path: &str, why: &RootWhy) -> bool {
        if !self.inputs.raw.contains_key(path) || (self.inputs.exclude)(path) {
            return false;
        }
        if self.ev.raw.contains_key(path) {
            return true;
        }
        self.ev.raw.insert(path.to_owned(), why.clone());
        if why.boot {
            self.boot_queue.push_back(path.to_owned());
        } else {
            self.later_queue.push_back(path.to_owned());
        }
        let queued = self.boot_queue.len() + self.later_queue.len();
        self.ev.counters.peak_queue = self.ev.counters.peak_queue.max(queued);
        true
    }

    /// Adds the cooked assets at a normalized path (once each); whether the path is a cooked asset.
    fn reach_cooked_path(&mut self, key: &str, why: &RootWhy) -> bool {
        if (self.inputs.exclude)(key) {
            return false;
        }
        let Some(ids) = self.by_path.get(key).cloned() else {
            return false;
        };
        for id in ids {
            self.ev.cooked.entry(id).or_insert_with(|| why.clone());
        }
        true
    }

    /// Adds one cooked id named by id (once), unless its path is excluded.
    fn reach_cooked_id(&mut self, id: AssetId, why: &RootWhy) -> bool {
        if self.path_of_id.get(&id).is_some_and(|p| (self.inputs.exclude)(p)) {
            return false;
        }
        self.ev.cooked.entry(id).or_insert_with(|| why.clone());
        true
    }

    /// A level (raw file or cooked-only) and its bakes.
    fn reach_level(&mut self, key: &str, why: &RootWhy) -> bool {
        let files = raw_files_of(self.inputs.raw, key);
        let mut hit = false;
        for f in &files {
            hit |= self.reach_raw(f, why);
        }
        if self.reach_cooked_path(key, why) {
            hit = true;
            // A cooked-only level pulls its bakes here; a raw level pulls them when it is read.
            self.reach_bakes(key, why);
        }
        hit || !files.is_empty()
    }

    fn reach_bakes(&mut self, level: &str, why: &RootWhy) {
        let bake = RootWhy {
            kind: RootKind::LevelBake,
            declared_by: why.declared_by.clone(),
            via: Some(level.to_owned()),
            boot: why.boot,
        };
        for p in level_bakes(&self.by_path, level) {
            self.reach_cooked_path(&p, &bake);
        }
    }

    /// Resolves one string a raw file names; whether it named project content.
    fn reference(&mut self, s: &str, from: &str, why: &RootWhy, follow_levels: bool) -> bool {
        self.ev.counters.strings_examined += 1;
        let key = normalize_asset_path(s.split('#').next().unwrap_or(s));
        let id_text = s.strip_prefix("asset:").unwrap_or(s).trim().to_lowercase();
        let target = self.levels_by_id.get(&id_text).cloned().unwrap_or(key);
        let mut child = RootWhy {
            kind: RootKind::Reference,
            declared_by: why.declared_by.clone(),
            via: Some(from.to_owned()),
            boot: why.boot,
        };
        let files = raw_files_of(self.inputs.raw, &target);
        let hit = if !files.is_empty() {
            // A level a file names is a travel target: it ships, but it is not needed at boot.
            if level_of_raw(&files[0]).is_some() {
                if !follow_levels {
                    return false;
                }
                child.boot = false;
            }
            let mut any = false;
            for f in &files {
                any |= self.reach_raw(f, &child);
            }
            any
        } else if self.reach_cooked_path(&target, &child) {
            true
        } else if let Some(id) = self.by_id_text.get(&id_text).copied() {
            self.reach_cooked_id(id, &child)
        } else {
            false
        };
        if hit {
            self.ev.counters.references += 1;
        }
        hit
    }

    /// Reads and scans one raw file.
    fn read(&mut self, path: &str) -> Result<(), RootError> {
        let why = self
            .ev
            .raw
            .get(path)
            .cloned()
            .ok_or_else(|| RootError::Unreadable {
                path: path.to_owned(),
                message: "not in the closure".to_owned(),
            })?;
        if let Some(level) = level_of_raw(path) {
            self.reach_bakes(level, &why);
        }
        let size = self.inputs.raw.get(path).copied().unwrap_or(0);
        if size > ROOT_FILE_MAX_BYTES {
            return Err(RootError::TooLarge {
                path: path.to_owned(),
                bytes: size,
            });
        }
        let full = self.inputs.assets_dir.join(path);
        let bytes = std::fs::read(&full).map_err(|e| RootError::Unreadable {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
        self.ev.counters.files_read += 1;
        self.ev.counters.bytes_read += bytes.len() as u64;
        let is_level = level_of_raw(path).is_some() && path.ends_with(".ron");
        let is_ui = path.ends_with(".ui.ron");
        let malformed = |message: String| RootError::Malformed {
            path: path.to_owned(),
            message,
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) if is_level || is_ui => return Err(malformed("not UTF-8 text".to_owned())),
            Err(_) => {
                self.ev.scans.insert(path.to_owned(), Extraction::Binary);
                return Ok(());
            }
        };
        if is_ui {
            self.ev.scans.insert(path.to_owned(), Extraction::UiLayout);
            let deps = crate::registry_bridge::ui_layout_dependencies(&text, path).map_err(malformed)?;
            for dep in deps {
                self.reference(&dep, path, &why, false);
            }
            for used in crate::ui_refs::ui_layout_references(&text) {
                if used.ends_with(".ui.ron") {
                    self.reference(&used, path, &why, false);
                }
            }
            return Ok(());
        }
        if is_level {
            self.ev.scans.insert(path.to_owned(), Extraction::Level);
            for s in ron_string_literals(&text).map_err(malformed)? {
                self.reference(&s, path, &why, true);
            }
            return Ok(());
        }
        self.ev.scans.insert(path.to_owned(), Extraction::Fallback);
        // Unknown format: decoded literals when the text is RON-like, else the legacy undecoded
        // scan. Conservative (any string that names content is followed) and reported as fallback.
        match ron_string_literals(&text) {
            Ok(strings) => {
                for s in strings {
                    self.reference(&s, path, &why, true);
                }
            }
            Err(_) => {
                for s in quoted_strings(&text) {
                    self.reference(s, path, &why, true);
                }
            }
        }
        Ok(())
    }

    fn drain(&mut self, boot: bool) -> Result<(), RootError> {
        loop {
            let next = if boot {
                self.boot_queue.pop_front()
            } else {
                self.boot_queue
                    .pop_front()
                    .or_else(|| self.later_queue.pop_front())
            };
            let Some(path) = next else {
                return Ok(());
            };
            self.read(&path)?;
        }
    }
}

/// Resolves the roots of a closure from `decls` and (when `implicit`) the startup UI layouts.
/// Declarations are processed in canonical order (boot kinds first, then by `declared_by` and
/// pattern), so the result does not depend on the order they were given in.
///
/// # Errors
/// [`RootError`]: a declared level that does not exist, an unreadable raw file, a malformed level
/// or UI layout, or a raw file over [`ROOT_FILE_MAX_BYTES`].
pub fn resolve_roots(
    inputs: &RootInputs<'_>,
    decls: &[RootDecl],
    implicit: bool,
) -> Result<RootEvidence, RootError> {
    let _span =
        tracing::info_span!(target: "regia_asset_pipeline", "closure_roots", decls = decls.len()).entered();
    let mut decls: Vec<RootDecl> = decls.to_vec();
    decls.sort();
    decls.dedup();
    let mut walk = Walk::new(inputs);
    let seed = |decl: &RootDecl, boot: bool| RootWhy {
        kind: decl.kind,
        declared_by: decl.declared_by.clone(),
        via: None,
        boot,
    };
    // Boot roots first: the startup and entry levels and the startup layouts, drained before any
    // travel target or dynamic root so their boot mark never depends on order.
    for decl in decls
        .iter()
        .filter(|d| matches!(d.kind, RootKind::StartupLevel | RootKind::EntryLevel))
    {
        let key = normalize_asset_path(&decl.pattern);
        if walk.reach_level(&key, &seed(decl, true)) {
            walk.ev.matched.insert(decl.declared_by.clone());
        } else if !(inputs.exclude)(&key) {
            return Err(RootError::Missing {
                kind: if decl.kind == RootKind::StartupLevel {
                    "startup level"
                } else {
                    "entry level"
                },
                declared_by: decl.declared_by.clone(),
                pattern: decl.pattern.clone(),
                assets: inputs.assets_dir.display().to_string(),
            });
        }
    }
    if implicit {
        let layouts: Vec<String> = inputs
            .raw
            .keys()
            .filter(|p| is_startup_ui_layout(p))
            .cloned()
            .collect();
        for p in layouts {
            let why = RootWhy {
                kind: RootKind::StartupUi,
                declared_by: "startup ui (ui/*.ui.ron)".to_owned(),
                via: None,
                boot: true,
            };
            walk.reach_raw(&p, &why);
        }
    }
    walk.drain(true)?;
    // Explicit dynamic roots: content loaded by name, declared by an include or entry glob.
    for decl in decls.iter().filter(|d| d.kind == RootKind::Declared) {
        let why = seed(decl, false);
        let raw: Vec<String> = inputs
            .raw
            .keys()
            .filter(|p| glob_match(&decl.pattern, p))
            .cloned()
            .collect();
        let cooked: Vec<String> = walk
            .by_path
            .keys()
            .filter(|p| glob_match(&decl.pattern, p))
            .cloned()
            .collect();
        let mut hit = false;
        for p in raw {
            hit |= walk.reach_raw(&p, &why);
        }
        for p in cooked {
            hit |= walk.reach_cooked_path(&p, &why);
        }
        if hit {
            walk.ev.matched.insert(decl.declared_by.clone());
        } else {
            walk.ev.unmatched.push(decl.declared_by.clone());
        }
    }
    walk.drain(false)?;
    walk.ev.unmatched.sort();
    Ok(walk.ev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_decode_escapes_unicode_and_raw_strings() {
        let text = r##"(a: "t/caf\u{e9}.png", b: "q\"x", c: r#"raw "x""#, d: 'c', e: "\u00e9", f: "\x41")"##;
        let got = ron_string_literals(text).expect("well formed");
        assert_eq!(got, ["t/café.png", "q\"x", "raw \"x\"", "é", "A"]);
    }

    #[test]
    fn comments_and_identifiers_are_not_strings() {
        let text = "// \"not\"\n/* \"no\" /* nested */ */ (bar: Some(\"yes\"), r#type: 1, rr: \"x\")";
        assert_eq!(ron_string_literals(text).expect("ok"), ["yes", "x"]);
    }

    #[test]
    fn malformed_text_is_an_error() {
        for bad in [
            "(a: \"open)",
            "(a: \"\\q\")",
            "(a: [1, 2)",
            "(a: 1",
            "/* open",
            "('ab')",
        ] {
            assert!(ron_string_literals(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn globs_and_paths_normalize() {
        assert_eq!(normalize_asset_path("assets://ui/a.png"), "ui/a.png");
        assert!(glob_match("assets/textures/**", "textures/a/b.png"));
        assert!(!glob_match("textures/*", "textures/a/b.png"));
    }
}
