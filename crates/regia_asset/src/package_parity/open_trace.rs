// SPDX-License-Identifier: MPL-2.0
//! Classify package files from strace text.
//!
//! Requirements: the open-trace classifier (mark `requirements`, commit c9513623f).
//! Rewritten in Rust. The tool only reads.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

const F_READ: u8 = 1;
const F_WRITE: u8 = 2;
const F_STAT: u8 = 4;
const F_OPEN_FAIL: u8 = 8;
const F_ANY_FAIL: u8 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SysKind {
    Open,
    Stat,
    Chdir,
}

/// How a package file was touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileClass {
    /// A successful read open.
    Opened,
    /// Stat, access, a failed open of an existing file, or a write-only open.
    StatOnly,
    /// A failed open of a path that is not on disk.
    MissingTried,
    /// No tracked call named this file.
    NeverTouched,
}

impl FileClass {
    /// The report spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Opened => "OPENED",
            Self::StatOnly => "STAT_ONLY",
            Self::MissingTried => "MISSING_TRIED",
            Self::NeverTouched => "NEVER_TOUCHED",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Opened => 0,
            Self::StatOnly => 1,
            Self::MissingTried => 2,
            Self::NeverTouched => 3,
        }
    }

    /// Report order: opened, stat-only, missing, never touched.
    #[must_use]
    pub fn all() -> [Self; 4] {
        [
            Self::Opened,
            Self::StatOnly,
            Self::MissingTried,
            Self::NeverTouched,
        ]
    }
}

/// Count and total bytes for one class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClassStats {
    /// Number of files.
    pub count: u64,
    /// Sum of file sizes.
    pub bytes: u64,
}

impl ClassStats {
    fn add(&mut self, size: u64) {
        self.count += 1;
        self.bytes += size;
    }
}

/// One classified path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// Absolute path text.
    pub path: String,
    /// Path relative to the package root, with `/` separators.
    pub relative: String,
    /// Size in bytes. Missing paths are zero.
    pub size: u64,
    /// Classification.
    pub class: FileClass,
}

/// One manifest chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkReport {
    /// Chunk name.
    pub name: String,
    /// Files named by the chunk.
    pub total_files: u64,
    /// Bytes of those files.
    pub total_bytes: u64,
    /// Counts by class, in [`FileClass::all`] order.
    pub classes: [ClassStats; 4],
    /// Chunk files that were only stat'd.
    pub stat_only_files: Vec<FileEntry>,
    /// Chunk files with no touch.
    pub never_touched_files: Vec<FileEntry>,
}

impl ChunkReport {
    /// Count of one class inside this chunk.
    #[must_use]
    pub fn class_count(&self, class: FileClass) -> u64 {
        self.classes[class.index()].count
    }
}

/// Classification of one package root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenReport {
    /// Package root text.
    pub root: String,
    /// Trace paths that were merged.
    pub trace_files: Vec<String>,
    /// Regular files and symlinks under the root.
    pub total_files: u64,
    /// Sum of those sizes. Missing tries are not included.
    pub total_bytes: u64,
    /// Counts by class, in [`FileClass::all`] order.
    pub classes: [ClassStats; 4],
    /// Group key to class counts.
    pub groups: BTreeMap<String, [ClassStats; 4]>,
    /// On-disk files, sorted by relative path.
    pub files: Vec<FileEntry>,
    /// On-disk files classified [`FileClass::StatOnly`].
    pub stat_only_files: Vec<FileEntry>,
    /// On-disk files classified [`FileClass::NeverTouched`].
    pub never_touched_files: Vec<FileEntry>,
    /// Failed paths under the root that are not on disk.
    pub missing_tried: Vec<FileEntry>,
    /// Per-chunk reports when a manifest was supplied.
    pub per_chunk: Option<BTreeMap<String, ChunkReport>>,
    /// Quoted paths strace marked as truncated.
    pub truncated_paths: u64,
}

impl OpenReport {
    /// Count for one class.
    #[must_use]
    pub fn class_count(&self, class: FileClass) -> u64 {
        self.classes[class.index()].count
    }

    /// Bytes for one class.
    #[must_use]
    pub fn class_bytes(&self, class: FileClass) -> u64 {
        self.classes[class.index()].bytes
    }

    /// The on-disk entry with this relative path.
    #[must_use]
    pub fn by_relative(&self, relative: &str) -> Option<&FileEntry> {
        self.files.iter().find(|e| e.relative == relative)
    }

    /// Count of one class inside a path group.
    #[must_use]
    pub fn group_count(&self, group: &str, class: FileClass) -> u64 {
        self.groups.get(group).map_or(0, |g| g[class.index()].count)
    }

    /// Bytes of one class inside a path group.
    #[must_use]
    pub fn group_bytes(&self, group: &str, class: FileClass) -> u64 {
        self.groups.get(group).map_or(0, |g| g[class.index()].bytes)
    }

    /// Plain-text report.
    #[must_use]
    pub fn render_text(&self) -> String {
        render_text(self)
    }

    /// Markdown report.
    #[must_use]
    pub fn render_markdown(&self) -> String {
        render_markdown(self)
    }

    /// JSON object of the report.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        report_json(self)
    }
}

#[derive(Clone, Debug, Default)]
struct Touch {
    flags: u8,
    attempts: u32,
}

impl Touch {
    fn touched(&self) -> bool {
        self.flags & (F_READ | F_WRITE | F_STAT | F_OPEN_FAIL) != 0
    }

    fn read_open(&self) -> bool {
        self.flags & F_READ != 0
    }

    fn any_failed(&self) -> bool {
        self.flags & F_ANY_FAIL != 0
    }

    fn merge(left: &Self, right: &Self) -> Self {
        Self {
            flags: left.flags | right.flags,
            attempts: left.attempts.saturating_add(right.attempts),
        }
    }
}

struct TouchIndex {
    by_absolute: HashMap<String, Touch>,
    by_relative: HashMap<String, Touch>,
    truncated_paths: u64,
}

impl TouchIndex {
    fn record(&mut self, absolute: Option<String>, relative: Option<String>) -> Option<&mut Touch> {
        if let Some(abs) = absolute {
            return Some(self.by_absolute.entry(abs).or_default());
        }
        relative.map(|rel| self.by_relative.entry(rel).or_default())
    }

    fn lookup(&self, absolute: &str, relative: &str) -> Option<Touch> {
        match (self.by_absolute.get(absolute), self.by_relative.get(relative)) {
            (Some(a), Some(b)) => Some(Touch::merge(a, b)),
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b.clone()),
            (None, None) => None,
        }
    }
}

struct Parser {
    index: TouchIndex,
    cwd: HashMap<String, String>,
    fds: HashMap<String, HashMap<i32, String>>,
    pending: HashMap<(String, String), String>,
}

impl Parser {
    fn new() -> Self {
        Self {
            index: TouchIndex {
                by_absolute: HashMap::new(),
                by_relative: HashMap::new(),
                truncated_paths: 0,
            },
            cwd: HashMap::new(),
            fds: HashMap::new(),
            pending: HashMap::new(),
        }
    }

    fn feed_text(&mut self, text: &str) {
        for line in text.lines() {
            self.feed_line(line);
        }
    }

    fn feed_line(&mut self, line: &str) {
        let (pid, text) = strip_pid(line);
        if text.is_empty()
            || text.starts_with("---")
            || text.starts_with("+++")
            || text.starts_with("strace:")
        {
            return;
        }
        if let Some((syscall, tail, result)) = parse_resumed(&text) {
            let combined = if let Some(args) = self.pending.remove(&(pid.clone(), syscall.to_string())) {
                format!("{args}{tail}")
            } else {
                tail.trim().to_string()
            };
            self.record_call(syscall, &combined, result, &pid);
            return;
        }
        if let Some((syscall, args)) = parse_unfinished(&text) {
            self.pending.insert((pid, syscall.to_string()), args.to_string());
            return;
        }
        if let Some((syscall, args, result)) = parse_normal(&text) {
            self.record_call(syscall, args, result, &pid);
        }
    }

    fn record_call(&mut self, syscall: &str, args_text: &str, result_text: &str, pid: &str) {
        let Some(kind) = sys_kind(syscall) else {
            return;
        };
        if kind == SysKind::Chdir {
            self.record_chdir(syscall, args_text, result_text, pid);
            return;
        }
        let parts = split_args(args_text);
        let path_index = usize::from(is_dirfd(syscall));
        if parts.len() <= path_index {
            self.record_fragment(kind, args_text, result_text, pid);
            return;
        }
        let Some((raw, truncated)) = unquote_strace(&parts[path_index]) else {
            self.record_fragment(kind, args_text, result_text, pid);
            return;
        };
        if truncated {
            self.index.truncated_paths += 1;
            return;
        }
        let dirfd = if is_dirfd(syscall) {
            parts.first().map(String::as_str)
        } else {
            None
        };
        let (absolute, relative) = self.resolve(dirfd, &raw, pid);
        let Some(touch) = self.index.record(absolute.clone(), relative) else {
            return;
        };
        let (succeeded, value) = parse_result(result_text);
        touch.attempts = touch.attempts.saturating_add(1);
        if kind == SysKind::Open {
            let flags = parts.get(path_index + 1).map(String::as_str);
            apply_open(touch, succeeded, is_read_flags(flags));
            if succeeded == Some(true)
                && let (Some(fd), Some(abs)) = (value, absolute)
            {
                self.fds.entry(pid.to_string()).or_default().insert(fd, abs);
            }
        } else {
            touch.flags |= F_STAT;
            if succeeded == Some(false) {
                touch.flags |= F_ANY_FAIL;
            }
        }
    }

    fn record_fragment(&mut self, kind: SysKind, args_text: &str, result_text: &str, pid: &str) {
        let Some((raw, truncated)) = unquote_strace(args_text) else {
            return;
        };
        if truncated {
            self.index.truncated_paths += 1;
            return;
        }
        let (absolute, relative) = self.resolve(None, &raw, pid);
        let Some(touch) = self.index.record(absolute, relative) else {
            return;
        };
        let (succeeded, _) = parse_result(result_text);
        touch.attempts = touch.attempts.saturating_add(1);
        if kind == SysKind::Open {
            if succeeded == Some(true) {
                touch.flags |= F_READ;
            } else if succeeded == Some(false) {
                touch.flags |= F_OPEN_FAIL | F_ANY_FAIL;
            }
        } else if kind == SysKind::Stat {
            touch.flags |= F_STAT;
            if succeeded == Some(false) {
                touch.flags |= F_ANY_FAIL;
            }
        }
    }

    fn record_chdir(&mut self, syscall: &str, args_text: &str, result_text: &str, pid: &str) {
        let (succeeded, _) = parse_result(result_text);
        if succeeded != Some(true) {
            return;
        }
        let parts = split_args(args_text);
        let Some(first) = parts.first() else {
            return;
        };
        if syscall == "fchdir" {
            let head = first.split('<').next().unwrap_or(first);
            let Ok(fd) = head.trim().parse::<i32>() else {
                return;
            };
            if let Some(target) = self.fds.get(pid).and_then(|m| m.get(&fd)).cloned() {
                self.cwd.insert(pid.to_string(), target);
            }
            return;
        }
        let Some((raw, truncated)) = unquote_strace(first) else {
            return;
        };
        if truncated {
            return;
        }
        if let (Some(abs), _) = self.resolve(None, &raw, pid) {
            self.cwd.insert(pid.to_string(), abs);
        }
    }

    fn resolve(&self, dirfd: Option<&str>, raw: &str, pid: &str) -> (Option<String>, Option<String>) {
        if raw.starts_with('/') {
            return (Some(norm_posix(raw)), None);
        }
        let relative = norm_posix(raw);
        let base = match dirfd {
            Some(token) => self.dirfd_base(token, pid),
            None => self.cwd.get(pid).cloned(),
        };
        match base {
            Some(base) => (Some(norm_posix(&format!("{base}/{relative}"))), None),
            None => (None, Some(relative)),
        }
    }

    fn dirfd_base(&self, token: &str, pid: &str) -> Option<String> {
        let token = token.trim();
        let (head, embedded) = if let Some(start) = token.find('<') {
            let end = token.rfind('>')?;
            (&token[..start], Some(&token[start + 1..end]))
        } else {
            (token, None)
        };
        if let Some(path) = embedded {
            return Some(norm_posix(path));
        }
        if head == "AT_FDCWD" {
            return self.cwd.get(pid).cloned();
        }
        let fd: i32 = head.parse().ok()?;
        self.fds.get(pid)?.get(&fd).cloned()
    }
}

fn apply_open(touch: &mut Touch, succeeded: Option<bool>, read: bool) {
    if succeeded == Some(true) {
        touch.flags |= if read { F_READ } else { F_WRITE };
    } else if succeeded == Some(false) {
        touch.flags |= F_OPEN_FAIL | F_ANY_FAIL;
    }
}

fn sys_kind(name: &str) -> Option<SysKind> {
    match name {
        "open" | "openat" | "openat2" => Some(SysKind::Open),
        "access" | "faccessat" | "faccessat2" | "lstat" | "lstat64" | "newfstatat" | "stat" | "stat64"
        | "statx" => Some(SysKind::Stat),
        "chdir" | "fchdir" => Some(SysKind::Chdir),
        _ => None,
    }
}

fn is_dirfd(name: &str) -> bool {
    matches!(
        name,
        "faccessat" | "faccessat2" | "newfstatat" | "openat" | "openat2" | "statx"
    )
}

fn strip_pid(line: &str) -> (String, String) {
    let text = line.trim();
    if let Some(rest) = text.strip_prefix("[pid ")
        && let Some((pid, after)) = rest.split_once("] ")
        && !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
    {
        return (pid.to_string(), after.trim().to_string());
    }
    let text = strip_stamp(text);
    if let Some((pid, rest)) = split_pid(&text) {
        return (pid, rest);
    }
    ("?".to_string(), text)
}

fn split_pid(text: &str) -> Option<(String, String)> {
    let (pid, rest) = text.split_once(|c: char| c.is_whitespace())?;
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rest = strip_stamp(rest.trim());
    let head = rest
        .split('(')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .next()
        .unwrap_or("");
    if sys_kind(head).is_some() || rest.starts_with("<...") || rest.starts_with('-') || rest.starts_with('+')
    {
        Some((pid.to_string(), rest))
    } else {
        None
    }
}

fn strip_stamp(text: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.len() < 9 || bytes[2] != b':' || bytes[5] != b':' {
        return text.to_string();
    }
    if !bytes[..2].iter().all(u8::is_ascii_digit)
        || !bytes[3..5].iter().all(u8::is_ascii_digit)
        || !bytes[6..8].iter().all(u8::is_ascii_digit)
    {
        return text.to_string();
    }
    let mut i = 8;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    if bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        text[i..].trim().to_string()
    } else {
        text.to_string()
    }
}

fn parse_resumed(text: &str) -> Option<(&str, &str, &str)> {
    let after = text.strip_prefix("<...")?.trim_start();
    let (name, more) = after.split_once(" resumed>")?;
    if name.is_empty() || name.contains(char::is_whitespace) || sys_kind(name).is_none() {
        return None;
    }
    let (args, result) = split_eq(more)?;
    Some((name, args, result))
}

fn parse_unfinished(text: &str) -> Option<(&str, &str)> {
    let cut = text.rfind("<unfinished")?;
    let (name, rest) = text.split_once('(')?;
    let name = name.trim();
    if sys_kind(name).is_none() || !rest.contains("<unfinished") {
        return None;
    }
    Some((name, &rest[..cut.saturating_sub(name.len() + 1)]))
}

fn parse_normal(text: &str) -> Option<(&str, &str, &str)> {
    let (name, rest) = text.split_once('(')?;
    let name = name.trim();
    sys_kind(name)?;
    let close = matching_paren(rest)?;
    let args = &rest[..close];
    let after = rest[close + 1..].trim();
    let result = after.strip_prefix('=')?.trim();
    if result.is_empty() {
        return None;
    }
    Some((name, args, result))
}

fn matching_paren(text: &str) -> Option<usize> {
    let mut depth = 1i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_eq(text: &str) -> Option<(&str, &str)> {
    let idx = text.rfind(") =").or_else(|| text.rfind(")="))?;
    let args = &text[..idx];
    let after = text[idx + 1..].trim_start();
    let result = after.trim_start_matches('=').trim();
    if result.is_empty() {
        None
    } else {
        Some((args, result))
    }
}

fn parse_result(text: &str) -> (Option<bool>, Option<i32>) {
    let text = text.trim();
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return (None, None);
    };
    let mut digits = String::new();
    if first == '-' {
        digits.push('-');
    } else if first.is_ascii_digit() {
        digits.push(first);
    } else {
        return (None, None);
    }
    for ch in chars {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            break;
        }
    }
    let Ok(value) = digits.parse::<i32>() else {
        return (None, None);
    };
    if value == -1 {
        (Some(false), Some(value))
    } else {
        (Some(true), Some(value))
    }
}

fn is_read_flags(flags: Option<&str>) -> bool {
    let Some(token) = flags else {
        return true;
    };
    if token.contains("O_RDWR") || token.contains("O_RDONLY") {
        true
    } else {
        !token.contains("O_WRONLY")
    }
}

fn split_args(args_text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut current = String::new();
    for ch in args_text.chars() {
        if in_string {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                current.push(ch);
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() || !parts.is_empty() {
        parts.push(tail);
    }
    parts
}

fn unquote_strace(token: &str) -> Option<(String, bool)> {
    let start = token.find('"')?;
    let mut out = String::new();
    let mut chars = token[start + 1..].chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '"' {
            let tail: String = chars.collect();
            return Some((out, tail.trim_start().starts_with("...")));
        }
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(nxt) = chars.next() else {
            return Some((out, true));
        };
        if let Some(simple) = simple_escape(nxt) {
            out.push(simple);
            continue;
        }
        if nxt == 'x' {
            let Some(h1) = chars.next() else {
                return Some((out, true));
            };
            let Some(h2) = chars.next() else {
                return Some((out, true));
            };
            let hex = format!("{h1}{h2}");
            if let Ok(v) = u32::from_str_radix(&hex, 16)
                && let Some(c) = char::from_u32(v)
            {
                out.push(c);
                continue;
            }
            out.push(nxt);
            continue;
        }
        if ('0'..='7').contains(&nxt) {
            let mut oct = String::new();
            oct.push(nxt);
            for _ in 0..2 {
                if chars.peek().is_some_and(|c| ('0'..='7').contains(c)) {
                    if let Some(c) = chars.next() {
                        oct.push(c);
                    }
                } else {
                    break;
                }
            }
            if let Ok(v) = u32::from_str_radix(&oct, 8)
                && let Some(c) = char::from_u32(v)
            {
                out.push(c);
                continue;
            }
            out.push(nxt);
            continue;
        }
        out.push(nxt);
    }
    Some((out, true))
}

fn simple_escape(ch: char) -> Option<char> {
    Some(match ch {
        'a' => '\u{0007}',
        'b' => '\u{0008}',
        'f' => '\u{000c}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\u{000b}',
        '"' | '\\' | '\'' | '?' => ch,
        _ => return None,
    })
}

fn norm_posix(path: &str) -> String {
    let abs = path.starts_with('/');
    let mut out = Vec::new();
    for part in path.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            out.pop();
            continue;
        }
        out.push(part);
    }
    let joined = out.join("/");
    if abs {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// Parse one strace blob and classify `root`.
#[must_use]
pub fn classify_text(root: &Path, text: &str) -> OpenReport {
    let mut parser = Parser::new();
    parser.feed_text(text);
    classify_index(root, parser.index, Vec::new(), None)
}

/// Read trace files, optionally a manifest, and classify `root`.
///
/// # Errors
///
/// Returns an error when a trace or the manifest cannot be read, or the manifest is not the expected object.
pub fn build_open_report(
    root: &Path,
    traces: &[PathBuf],
    manifest: Option<&Path>,
) -> Result<OpenReport, String> {
    let mut parser = Parser::new();
    let mut names = Vec::new();
    for path in traces {
        let text =
            fs::read_to_string(path).map_err(|err| format!("cannot read trace file {path:?}: {err}"))?;
        parser.pending.clear();
        parser.feed_text(&text);
        names.push(path.display().to_string());
    }
    let manifest = match manifest {
        Some(path) => Some(load_manifest(path)?),
        None => None,
    };
    Ok(classify_index(root, parser.index, names, manifest.as_ref()))
}

fn load_manifest(path: &Path) -> Result<serde_json::Value, String> {
    let text = fs::read_to_string(path).map_err(|err| format!("cannot read manifest {path:?}: {err}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| format!("invalid manifest JSON in {path:?}: {err}"))?;
    if !value.is_object() {
        return Err(format!("manifest {path:?} must hold a JSON object"));
    }
    Ok(value)
}

fn classify_index(
    root: &Path,
    index: TouchIndex,
    trace_files: Vec<String>,
    manifest: Option<&serde_json::Value>,
) -> OpenReport {
    let root_text = root.display().to_string();
    let mut classes = [ClassStats::default(); 4];
    let mut groups: BTreeMap<String, [ClassStats; 4]> = BTreeMap::new();
    let mut files = Vec::new();
    let mut stat_only = Vec::new();
    let mut never = Vec::new();
    let mut on_disk: BTreeMap<String, FileEntry> = BTreeMap::new();
    let mut total_bytes = 0u64;
    let mut paths = Vec::new();
    walk_files(root, &mut paths);
    paths.sort();
    for path in paths {
        let Ok(rel_path) = path.strip_prefix(root) else {
            continue;
        };
        let relative = rel_path.to_string_lossy().replace('\\', "/");
        let size = file_size(&path);
        let absolute = path.display().to_string();
        let touch = index.lookup(&absolute, &relative);
        let class = classify_touch(touch.as_ref());
        let entry = FileEntry {
            path: absolute,
            relative: relative.clone(),
            size,
            class,
        };
        classes[class.index()].add(size);
        total_bytes += size;
        let group = groups
            .entry(group_key(&relative))
            .or_insert([ClassStats::default(); 4]);
        group[class.index()].add(size);
        if class == FileClass::StatOnly {
            stat_only.push(entry.clone());
        } else if class == FileClass::NeverTouched {
            never.push(entry.clone());
        }
        on_disk.insert(relative, entry.clone());
        files.push(entry);
    }
    let missing = missing_entries(&index, &root_text, &on_disk);
    for entry in &missing {
        classes[FileClass::MissingTried.index()].add(0);
        let group = groups
            .entry(group_key(&entry.relative))
            .or_insert([ClassStats::default(); 4]);
        group[FileClass::MissingTried.index()].add(0);
    }
    let per_chunk = manifest.map(|m| per_chunk(m, &root_text, &on_disk, &missing));
    OpenReport {
        root: root_text,
        trace_files,
        total_files: files.len() as u64,
        total_bytes,
        classes,
        groups,
        files,
        stat_only_files: stat_only,
        never_touched_files: never,
        missing_tried: missing,
        per_chunk,
        truncated_paths: index.truncated_paths,
    }
}

fn classify_touch(touch: Option<&Touch>) -> FileClass {
    match touch {
        Some(touch) if touch.read_open() => FileClass::Opened,
        Some(touch) if touch.touched() => FileClass::StatOnly,
        _ => FileClass::NeverTouched,
    }
}

fn group_key(relative: &str) -> String {
    let parts: Vec<&str> = relative.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [a, b, ..] => format!("{a}/{b}"),
        [a] => (*a).to_string(),
        _ => ".".to_string(),
    }
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path)
        .or_else(|_| fs::symlink_metadata(path))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn walk_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut kids: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    kids.sort_by_key(|e| e.file_name());
    for entry in kids {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk_files(&path, out);
        } else if kind.is_file() || kind.is_symlink() {
            out.push(path);
        }
    }
}

fn missing_entries(
    index: &TouchIndex,
    root_text: &str,
    on_disk: &BTreeMap<String, FileEntry>,
) -> Vec<FileEntry> {
    let mut missing = Vec::new();
    let mut seen = BTreeMap::<String, ()>::new();
    let mut absolutes: Vec<_> = index.by_absolute.iter().collect();
    absolutes.sort_by(|a, b| a.0.cmp(b.0));
    for (absolute, touch) in absolutes {
        if !touch.any_failed() {
            continue;
        }
        let Some(relative) = under_root(absolute, root_text) else {
            continue;
        };
        if on_disk.contains_key(&relative) || seen.contains_key(&relative) {
            continue;
        }
        seen.insert(relative.clone(), ());
        missing.push(FileEntry {
            path: absolute.clone(),
            relative,
            size: 0,
            class: FileClass::MissingTried,
        });
    }
    let mut relatives: Vec<_> = index.by_relative.iter().collect();
    relatives.sort_by(|a, b| a.0.cmp(b.0));
    for (relative, touch) in relatives {
        if !touch.any_failed()
            || on_disk.contains_key(relative)
            || seen.contains_key(relative)
            || relative.starts_with("../")
        {
            continue;
        }
        seen.insert(relative.clone(), ());
        missing.push(FileEntry {
            path: format!("{root_text}/{relative}"),
            relative: relative.clone(),
            size: 0,
            class: FileClass::MissingTried,
        });
    }
    missing
}

fn under_root(absolute: &str, root_text: &str) -> Option<String> {
    let prefix = if root_text.ends_with('/') {
        root_text.to_string()
    } else {
        format!("{root_text}/")
    };
    absolute.strip_prefix(&prefix).map(|rel| rel.replace('\\', "/"))
}

fn per_chunk(
    manifest: &serde_json::Value,
    root_text: &str,
    on_disk: &BTreeMap<String, FileEntry>,
    missing: &[FileEntry],
) -> BTreeMap<String, ChunkReport> {
    let mut result = BTreeMap::new();
    let Some(chunks) = manifest.get("chunks").and_then(|v| v.as_array()) else {
        return result;
    };
    let missing_by: BTreeMap<_, _> = missing.iter().map(|e| (e.relative.clone(), e.clone())).collect();
    for (position, chunk) in chunks.iter().enumerate() {
        let Some(chunk) = chunk.as_object() else {
            continue;
        };
        let name = chunk
            .get("name")
            .or_else(|| chunk.get("id"))
            .or_else(|| chunk.get("chunk"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("chunk-{position}"));
        let Some(files) = chunk.get("files").and_then(|v| v.as_array()) else {
            continue;
        };
        let mut report = ChunkReport {
            name: name.clone(),
            total_files: 0,
            total_bytes: 0,
            classes: [ClassStats::default(); 4],
            stat_only_files: Vec::new(),
            never_touched_files: Vec::new(),
        };
        for raw in files {
            let relative = manifest_relative(raw.as_str().unwrap_or(""), root_text);
            let entry = on_disk
                .get(&relative)
                .cloned()
                .or_else(|| missing_by.get(&relative).cloned())
                .unwrap_or_else(|| FileEntry {
                    path: format!("{root_text}/{relative}"),
                    relative: relative.clone(),
                    size: 0,
                    class: FileClass::NeverTouched,
                });
            report.total_files += 1;
            report.total_bytes += entry.size;
            report.classes[entry.class.index()].add(entry.size);
            if entry.class == FileClass::StatOnly {
                report.stat_only_files.push(entry);
            } else if entry.class == FileClass::NeverTouched {
                report.never_touched_files.push(entry);
            }
        }
        result.insert(name, report);
    }
    result
}

fn manifest_relative(raw: &str, root_text: &str) -> String {
    let text = raw.trim();
    if text.starts_with('/') {
        return under_root(text, root_text).unwrap_or_else(|| text.trim_start_matches('/').to_string());
    }
    norm_posix(text).trim_start_matches("./").to_string()
}

fn render_text(report: &OpenReport) -> String {
    let mut lines = vec![
        format!("Root: {}", report.root),
        format!(
            "Total files: {} ({} bytes)",
            report.total_files, report.total_bytes
        ),
        String::new(),
        "Class counts:".to_string(),
    ];
    for class in FileClass::all() {
        let stats = report.classes[class.index()];
        lines.push(format!(
            "  {}: {} files, {} bytes",
            class.as_str(),
            stats.count,
            stats.bytes
        ));
    }
    if report.truncated_paths > 0 {
        lines.push(format!(
            "  (ignored {} truncated path(s))",
            report.truncated_paths
        ));
    }
    lines.push(String::new());
    lines.push("By first two path segments:".to_string());
    for (group, stats) in &report.groups {
        let parts: Vec<String> = FileClass::all()
            .into_iter()
            .filter(|c| stats[c.index()].count > 0)
            .map(|c| {
                format!(
                    "{}={}/{}B",
                    c.as_str(),
                    stats[c.index()].count,
                    stats[c.index()].bytes
                )
            })
            .collect();
        lines.push(format!("  {group}: {}", parts.join(", ")));
    }
    lines.push(String::new());
    lines.push("STAT_ONLY files (candidates for same-length sparse stubs):".to_string());
    if report.stat_only_files.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for entry in &report.stat_only_files {
            lines.push(format!("  {} {}", entry.size, entry.relative));
        }
    }
    lines.push(String::new());
    lines.push("NEVER_TOUCHED files (candidates to drop):".to_string());
    if report.never_touched_files.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for entry in &report.never_touched_files {
            lines.push(format!("  {} {}", entry.size, entry.relative));
        }
    }
    lines.push(String::new());
    lines.push("MISSING_TRIED paths (failed opens, not on disk):".to_string());
    if report.missing_tried.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for entry in &report.missing_tried {
            lines.push(format!("  {}", entry.relative));
        }
    }
    lines.join("\n") + "\n"
}

fn render_markdown(report: &OpenReport) -> String {
    let mut lines = vec![
        "# open_trace report".to_string(),
        String::new(),
        format!("Root: `{}`", report.root),
        format!(
            "Total files: {} ({} bytes)",
            report.total_files, report.total_bytes
        ),
        String::new(),
        "| Class | Files | Bytes |".to_string(),
        "| --- | ---: | ---: |".to_string(),
    ];
    for class in FileClass::all() {
        let stats = report.classes[class.index()];
        lines.push(format!(
            "| {} | {} | {} |",
            class.as_str(),
            stats.count,
            stats.bytes
        ));
    }
    push_md_files(&mut lines, "STAT_ONLY", &report.stat_only_files, true);
    push_md_files(&mut lines, "NEVER_TOUCHED", &report.never_touched_files, true);
    push_md_files(&mut lines, "MISSING_TRIED", &report.missing_tried, false);
    lines.join("\n") + "\n"
}

fn push_md_files(lines: &mut Vec<String>, title: &str, entries: &[FileEntry], with_size: bool) {
    lines.push(String::new());
    lines.push(format!("## {title}"));
    lines.push(String::new());
    if entries.is_empty() {
        lines.push("(none)".to_string());
        return;
    }
    for entry in entries {
        if with_size {
            lines.push(format!("- `{}` ({} bytes)", entry.relative, entry.size));
        } else {
            lines.push(format!("- `{}`", entry.relative));
        }
    }
}

fn report_json(report: &OpenReport) -> serde_json::Value {
    let mut classes = serde_json::Map::new();
    for class in FileClass::all() {
        let stats = report.classes[class.index()];
        classes.insert(
            class.as_str().to_string(),
            serde_json::json!({"count": stats.count, "bytes": stats.bytes}),
        );
    }
    let mut groups = serde_json::Map::new();
    for (group, stats) in &report.groups {
        let mut one = serde_json::Map::new();
        for class in FileClass::all() {
            one.insert(
                class.as_str().to_string(),
                serde_json::json!({"count": stats[class.index()].count, "bytes": stats[class.index()].bytes}),
            );
        }
        groups.insert(group.clone(), serde_json::Value::Object(one));
    }
    let mut value = serde_json::json!({
        "root": report.root,
        "trace_files": report.trace_files,
        "total_files": report.total_files,
        "total_bytes": report.total_bytes,
        "classes": classes,
        "groups": groups,
        "files": report.files.iter().map(entry_json).collect::<Vec<_>>(),
        "opened_files": report.files.iter().filter(|e| e.class == FileClass::Opened).map(entry_json).collect::<Vec<_>>(),
        "stat_only_files": report.stat_only_files.iter().map(entry_json).collect::<Vec<_>>(),
        "never_touched_files": report.never_touched_files.iter().map(entry_json).collect::<Vec<_>>(),
        "missing_tried": report.missing_tried.iter().map(entry_json).collect::<Vec<_>>(),
        "truncated_paths": report.truncated_paths,
    });
    if let Some(chunks) = &report.per_chunk {
        let mut map = serde_json::Map::new();
        for (name, chunk) in chunks {
            map.insert(name.clone(), chunk_json(chunk));
        }
        if let Some(obj) = value.as_object_mut() {
            obj.insert("per_chunk".to_string(), serde_json::Value::Object(map));
        }
    }
    value
}

fn entry_json(entry: &FileEntry) -> serde_json::Value {
    serde_json::json!({
        "path": entry.path,
        "relative": entry.relative,
        "size": entry.size,
        "class": entry.class.as_str(),
    })
}

fn chunk_json(chunk: &ChunkReport) -> serde_json::Value {
    let mut classes = serde_json::Map::new();
    for class in FileClass::all() {
        let stats = chunk.classes[class.index()];
        classes.insert(
            class.as_str().to_string(),
            serde_json::json!({"count": stats.count, "bytes": stats.bytes}),
        );
    }
    serde_json::json!({
        "name": chunk.name,
        "total_files": chunk.total_files,
        "total_bytes": chunk.total_bytes,
        "classes": classes,
        "stat_only_files": chunk.stat_only_files.iter().map(entry_json).collect::<Vec<_>>(),
        "never_touched_files": chunk.never_touched_files.iter().map(entry_json).collect::<Vec<_>>(),
    })
}

const HELP: &str = "\
usage: regia package parity --classify --trace FILE --root DIR

Classify files under a package root from strace output.

Examples:
  regia package parity --classify --trace run.strace --root pkg/
  regia package parity --classify --trace a.strace --trace b.strace --root pkg/ --json
  regia package parity --classify --trace traces/ --root pkg/ --manifest chunks.json --markdown

Exit codes:
  0  success; the report was printed
  2  usage error, a missing root/trace, or an unreadable trace/manifest
";

/// Run the open-trace command. Arguments do not include the program name.
#[must_use]
pub fn open_trace_main(args: &[String]) -> (i32, String, String) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return (0, HELP.to_string(), String::new());
    }
    let mut traces = Vec::new();
    let mut root: Option<String> = None;
    let mut manifest: Option<String> = None;
    let mut as_json = false;
    let mut as_markdown = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let take = |i: &mut usize| -> Option<String> {
            *i += 1;
            args.get(*i).cloned()
        };
        match arg.as_str() {
            "--trace" => match take(&mut i) {
                Some(v) => traces.push(v),
                None => return (2, String::new(), "error: --trace needs a path\n".to_string()),
            },
            "--root" => match take(&mut i) {
                Some(v) => root = Some(v),
                None => return (2, String::new(), "error: --root needs a path\n".to_string()),
            },
            "--manifest" => match take(&mut i) {
                Some(v) => manifest = Some(v),
                None => return (2, String::new(), "error: --manifest needs a path\n".to_string()),
            },
            "--json" => as_json = true,
            "--markdown" => as_markdown = true,
            other if other.starts_with('-') => {
                return (2, String::new(), format!("error: unknown argument {other}\n"));
            }
            _ => return (2, String::new(), format!("error: unexpected argument {arg}\n")),
        }
        i += 1;
    }
    if as_json && as_markdown {
        return (
            2,
            String::new(),
            "error: --json and --markdown are exclusive\n".to_string(),
        );
    }
    let Some(root) = root else {
        return (2, String::new(), "error: --root is required\n".to_string());
    };
    let root_path = PathBuf::from(&root);
    if !root_path.is_dir() {
        return (
            2,
            String::new(),
            format!("error: --root is not a directory: {root}\n"),
        );
    }
    let expanded = expand_traces(&traces);
    if expanded.is_empty() {
        return (2, String::new(), "error: no trace files found\n".to_string());
    }
    for path in &expanded {
        if !path.is_file() {
            return (
                2,
                String::new(),
                format!("error: trace file not found: {}\n", path.display()),
            );
        }
    }
    let manifest_path = manifest.as_ref().map(PathBuf::from);
    match build_open_report(&root_path, &expanded, manifest_path.as_deref()) {
        Ok(report) => {
            let stdout = if as_json {
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(&report.to_json()).unwrap_or_else(|_| "{}".to_string())
                )
            } else if as_markdown {
                report.render_markdown()
            } else {
                report.render_text()
            };
            (0, stdout, String::new())
        }
        Err(err) => (2, String::new(), format!("error: {err}\n")),
    }
}

fn expand_traces(values: &[String]) -> Vec<PathBuf> {
    let mut expanded = Vec::new();
    for value in values {
        let path = PathBuf::from(value);
        if path.is_dir() {
            if let Ok(entries) = fs::read_dir(&path) {
                let mut kids: Vec<_> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_file())
                    .collect();
                kids.sort();
                expanded.extend(kids);
            }
        } else {
            expanded.push(path);
        }
    }
    expanded
}
