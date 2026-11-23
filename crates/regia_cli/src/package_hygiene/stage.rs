// SPDX-License-Identifier: MPL-2.0
//! Shipping-stage preparation: notes drop, build-path scrub, allowlist, dev probe.
//!
//! Original Rust (ADR-8427). The behavior is derived from the C-10 requirements review.
//! No shell or Python is copied. Reports never include matched path text or probe markers.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::policy::{Compiled, compile_globs};
use super::{AuditError, Finding, PathScrub, Policy, Report, StageRules};

const DEV_MARKERS: &[&[u8]] = &[b"regia-dev-feature", b"regia-cheat-hook"];
const UNIX_ROOTS: &[(&[u8], bool)] = &[
    (b"/home/", true),
    (b"/srv/", true),
    (b"/media/", true),
    (b"/tmp/", false),
];

pub(super) fn prepare(root: &Path, policy: &Policy, stage: &StageRules) -> Result<Report, AuditError> {
    let mut report = Report::default();
    apply(root, policy, stage, true, &mut report)?;
    finish(report)
}

pub(super) fn review(
    root: &Path,
    policy: &Policy,
    stage: &StageRules,
    report: &mut Report,
) -> Result<(), AuditError> {
    apply(root, policy, stage, false, report)
}

pub(super) fn review_hosts(
    root: &Path,
    policy: &Policy,
    scanner: &super::host_strings::Scanner,
    report: &mut Report,
) -> Result<(), AuditError> {
    for file in collect(root, policy, None)? {
        if super::host_strings::is_executable_file(&file.abs)? {
            let data = read_file(&file.abs, &file.rel, policy)?;
            scanner.scan(&data, &file.rel, &file.id, policy, report)?;
        }
    }
    report.findings.sort();
    Ok(())
}

fn finish(mut report: Report) -> Result<Report, AuditError> {
    report.findings.sort();
    report.actions.sort();
    if report.findings.is_empty() {
        Ok(report)
    } else {
        Err(rejected(report))
    }
}

fn rejected(report: Report) -> AuditError {
    AuditError {
        code: "package_hygiene_failed",
        location: "staged package rejected".into(),
        location_id: None,
        report: Some(Box::new(report)),
        matcher_bytes: 0,
        matchers_compiled: 0,
    }
}

fn apply(
    root: &Path,
    policy: &Policy,
    stage: &StageRules,
    mutate: bool,
    report: &mut Report,
) -> Result<(), AuditError> {
    let rules = Compiled::new(policy)?;
    let allow_globs = match &stage.stage_allow {
        Some(globs) => Some(compile_globs(globs, "stage_allow")?),
        None => None,
    };
    let legal = compile_globs(&stage.legal_notices, "legal_notices")?;
    let files = if mutate {
        collect(root, policy, Some(report))?
    } else {
        collect(root, policy, None)?
    };
    for file in &files {
        if drop_note(file, &legal, &rules, mutate, policy, report)? {
            continue;
        }
        let data = read_file(&file.abs, &file.rel, policy)?;
        scrub_file(file, &data, policy, &rules, mutate, report)?;
        if let Some(globs) = &allow_globs
            && !globs.iter().any(|glob| glob.is_match(file.rel.as_bytes()))
            && !waived(&rules, &file.rel, "stage_allow", report)
        {
            push_finding(report, policy, finding(file, "stage_allow", "name", 0))?;
        }
        check_dev(file, &data, &rules, mutate, policy, report)?;
    }
    if !mutate {
        report.findings.sort();
    }
    Ok(())
}

struct StagedFile {
    abs: PathBuf,
    rel: String,
    executable: bool,
    /// Opaque stage-local identity (`stage-N`, sorted traversal order). It encodes no path, so
    /// a finding can name its file without the report keeping the path (ADR-8229, ADR-8574).
    id: String,
}

fn drop_note(
    file: &StagedFile,
    legal: &[regex::bytes::Regex],
    rules: &Compiled,
    mutate: bool,
    policy: &Policy,
    report: &mut Report,
) -> Result<bool, AuditError> {
    if !is_internal_note(&file.rel) || legal.iter().any(|glob| glob.is_match(file.rel.as_bytes())) {
        return Ok(false);
    }
    if waived(rules, &file.rel, "internal_notes", report) {
        return Ok(false);
    }
    if mutate {
        std::fs::remove_file(&file.abs).map_err(|_| io(&file.rel))?;
        report.actions.push(finding(file, "internal_notes", "drop", 0));
    } else {
        push_finding(report, policy, finding(file, "internal_notes", "content", 0))?;
    }
    Ok(true)
}

fn scrub_file(
    file: &StagedFile,
    data: &[u8],
    policy: &Policy,
    rules: &Compiled,
    mutate: bool,
    report: &mut Report,
) -> Result<(), AuditError> {
    if policy.path_scrub == PathScrub::Off || !is_text_asset(&file.rel) || data.contains(&0) {
        return Ok(());
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return Ok(());
    };
    let reject_only = !mutate || policy.path_scrub == PathScrub::Reject;
    match scrub_text(text, reject_only) {
        Scrub::Clean => Ok(()),
        Scrub::Rewritten { text, offset } if mutate && policy.path_scrub == PathScrub::Rewrite => {
            if waived(rules, &file.rel, "build_path", report) {
                return Ok(());
            }
            replace_file(&file.abs, &file.rel, text.as_bytes())?;
            report
                .actions
                .push(finding(file, "build_path", "rewrite", offset));
            Ok(())
        }
        Scrub::Rewritten { offset, .. } | Scrub::Hit { offset } => {
            if waived(rules, &file.rel, "build_path", report) {
                return Ok(());
            }
            push_finding(report, policy, finding(file, "build_path", "content", offset))
        }
    }
}

fn check_dev(
    file: &StagedFile,
    data: &[u8],
    rules: &Compiled,
    spawn: bool,
    policy: &Policy,
    report: &mut Report,
) -> Result<(), AuditError> {
    if !is_probe_candidate(data) {
        return Ok(());
    }
    if let Some(offset) = marker_offset(data) {
        if !waived(rules, &file.rel, "dev_flag", report) {
            push_finding(report, policy, finding(file, "dev_flag", "content", offset))?;
        }
        return Ok(());
    }
    if spawn && file.executable && probe_accepts(&file.abs)? && !waived(rules, &file.rel, "dev_flag", report)
    {
        push_finding(report, policy, finding(file, "dev_flag", "probe", 0))?;
    }
    Ok(())
}

fn collect(root: &Path, policy: &Policy, tally: Option<&mut Report>) -> Result<Vec<StagedFile>, AuditError> {
    let meta = std::fs::symlink_metadata(root).map_err(|_| io("."))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(path_err("."));
    }
    let mut files = Vec::new();
    let mut entries = 0usize;
    let mut bytes = 0u64;
    walk(root, root, 0, policy, &mut entries, &mut bytes, &mut files)?;
    for (index, file) in files.iter_mut().enumerate() {
        file.id = format!("stage-{}", index + 1);
    }
    if let Some(report) = tally {
        report.files_scanned = u64::try_from(files.len()).unwrap_or(u64::MAX);
        report.bytes_scanned = bytes;
    }
    Ok(files)
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    policy: &Policy,
    entries: &mut usize,
    bytes: &mut u64,
    files: &mut Vec<StagedFile>,
) -> Result<(), AuditError> {
    if depth > 64 {
        return Err(limit(&rel_label(root, dir)?));
    }
    let label = rel_label(root, dir)?;
    let read = std::fs::read_dir(dir).map_err(|_| io(&label))?;
    let mut paths = Vec::new();
    for entry in read {
        *entries = entries.checked_add(1).ok_or_else(|| limit(&label))?;
        if *entries > policy.max_entries {
            return Err(limit(&label));
        }
        paths.push(entry.map_err(|_| io(&label))?.path());
    }
    paths.sort();
    for path in paths {
        let rel = relative(root, &path)?;
        let meta = std::fs::symlink_metadata(&path).map_err(|_| io(&rel))?;
        if meta.file_type().is_symlink() || (!meta.is_dir() && !meta.is_file()) {
            return Err(path_err(&rel));
        }
        if meta.is_dir() {
            walk(root, &path, depth + 1, policy, entries, bytes, files)?;
        } else {
            let size = meta.len();
            if size > policy.max_file_bytes {
                return Err(limit(&rel));
            }
            *bytes = bytes.checked_add(size).ok_or_else(|| limit(&rel))?;
            if *bytes > policy.max_total_bytes {
                return Err(limit(&rel));
            }
            files.push(StagedFile {
                abs: path,
                rel,
                executable: is_executable(&meta),
                id: String::new(),
            });
        }
    }
    Ok(())
}

fn rel_label(root: &Path, dir: &Path) -> Result<String, AuditError> {
    let rel = relative(root, dir)?;
    Ok(if rel.is_empty() { ".".to_owned() } else { rel })
}

fn relative(root: &Path, path: &Path) -> Result<String, AuditError> {
    let relative = path.strip_prefix(root).map_err(|_| path_err("."))?;
    let mut components = Vec::new();
    for component in relative.components() {
        let value = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| path_err("non_utf8_name"))?;
        if value.contains('\\') || value.contains(':') || value.chars().any(char::is_control) {
            return Err(path_err("invalid_name"));
        }
        components.push(value);
    }
    Ok(components.join("/"))
}

fn read_file(path: &Path, rel: &str, policy: &Policy) -> Result<Vec<u8>, AuditError> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| io(rel))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(path_err(rel));
    }
    let size = meta.len();
    if size > policy.max_file_bytes {
        return Err(limit(rel));
    }
    let file = std::fs::File::open(path).map_err(|_| io(rel))?;
    let mut data = Vec::new();
    file.take(size + 1).read_to_end(&mut data).map_err(|_| io(rel))?;
    if data.len() as u64 != size {
        return Err(io(rel));
    }
    Ok(data)
}

fn replace_file(path: &Path, rel: &str, bytes: &[u8]) -> Result<(), AuditError> {
    let mut name = path.file_name().ok_or_else(|| io(rel))?.to_os_string();
    name.push(".hygiene-tmp");
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, bytes).map_err(|_| io(rel))?;
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(rel));
    }
    Ok(())
}

fn is_internal_note(rel: &str) -> bool {
    let Some((dir, name)) = rel.rsplit_once('/') else {
        return false;
    };
    if !dir.split('/').any(|part| part == "content") {
        return false;
    }
    if let Some((_, ext)) = name.rsplit_once('.')
        && ext.eq_ignore_ascii_case("md")
    {
        return true;
    }
    let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name);
    matches!(stem.to_ascii_lowercase().as_str(), "readme" | "notes" | "todo")
}

fn is_text_asset(rel: &str) -> bool {
    let name = rel.rsplit_once('/').map_or(rel, |(_, name)| name);
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    matches!(ext.to_ascii_lowercase().as_str(), "json" | "ron" | "gltf" | "txt")
}

fn is_probe_candidate(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x7fELF") || bytes.starts_with(b"MZ") || bytes.starts_with(b"#!")
}

fn marker_offset(bytes: &[u8]) -> Option<usize> {
    DEV_MARKERS
        .iter()
        .filter_map(|needle| find_bytes(bytes, needle))
        .min()
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|window| window == needle)
}

fn is_executable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        true
    }
}

enum Scrub {
    Clean,
    Rewritten { text: String, offset: usize },
    Hit { offset: usize },
}

fn scrub_text(text: &str, reject_only: bool) -> Scrub {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut index = 0;
    let mut first = None;
    while index < bytes.len() {
        match match_path(bytes, index) {
            Some(Hit::Bad) => return Scrub::Hit { offset: index },
            Some(Hit::Rel { end, rel }) => {
                if reject_only {
                    return Scrub::Hit { offset: index };
                }
                if first.is_none() {
                    first = Some(index);
                }
                out.push_str(&rel);
                index = end;
            }
            None => index = push_char(text, index, &mut out),
        }
    }
    match first {
        Some(offset) => Scrub::Rewritten { text: out, offset },
        None => Scrub::Clean,
    }
}

enum Hit {
    Rel { end: usize, rel: String },
    Bad,
}

fn match_path(bytes: &[u8], index: usize) -> Option<Hit> {
    if !boundary_ok(bytes, index) {
        return None;
    }
    if bytes.get(index) == Some(&b'/') {
        return match_unix(bytes, index);
    }
    if bytes
        .get(index)
        .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'C'))
    {
        return match_windows(bytes, index);
    }
    None
}

fn boundary_ok(bytes: &[u8], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let previous = bytes[index - 1];
    !(previous.is_ascii_alphanumeric() || previous == b'/' || previous == b'\\' || previous == b':')
}

fn match_unix(bytes: &[u8], index: usize) -> Option<Hit> {
    for (prefix, skip_user) in UNIX_ROOTS {
        if !bytes[index..].starts_with(prefix) {
            continue;
        }
        let mut cursor = index + prefix.len();
        if *skip_user {
            let start = cursor;
            while cursor < bytes.len() && is_segment_byte(bytes[cursor]) {
                cursor += 1;
            }
            if cursor == start {
                return Some(Hit::Bad);
            }
            let Some(next) = take_sep(bytes, cursor) else {
                return Some(Hit::Bad);
            };
            cursor = next;
        }
        return Some(take_rest(bytes, cursor));
    }
    None
}

fn match_windows(bytes: &[u8], index: usize) -> Option<Hit> {
    if index + 2 > bytes.len() || bytes[index + 1] != b':' {
        return None;
    }
    let after_drive = take_sep(bytes, index + 2)?;
    if after_drive + 5 > bytes.len() || !bytes[after_drive..after_drive + 5].eq_ignore_ascii_case(b"Users") {
        return None;
    }
    let after_users = take_sep(bytes, after_drive + 5)?;
    let mut cursor = after_users;
    let user_start = cursor;
    while cursor < bytes.len() && is_segment_byte(bytes[cursor]) {
        cursor += 1;
    }
    if cursor == user_start {
        return Some(Hit::Bad);
    }
    let Some(rest_at) = take_sep(bytes, cursor) else {
        return Some(Hit::Bad);
    };
    Some(take_rest(bytes, rest_at))
}

fn take_rest(bytes: &[u8], start: usize) -> Hit {
    let mut end = start;
    while end < bytes.len() && is_path_byte(bytes[end]) {
        end += 1;
    }
    if end == start {
        return Hit::Bad;
    }
    let rel = normalize(&bytes[start..end]);
    if rel.is_empty() {
        Hit::Bad
    } else {
        Hit::Rel { end, rel }
    }
}

fn take_sep(bytes: &[u8], index: usize) -> Option<usize> {
    match bytes.get(index) {
        Some(b'/') => Some(index + 1),
        Some(b'\\') => {
            let mut cursor = index;
            while bytes.get(cursor) == Some(&b'\\') {
                cursor += 1;
            }
            Some(cursor)
        }
        _ => None,
    }
}

fn normalize(rest: &[u8]) -> String {
    let mut out = String::new();
    let mut index = 0;
    while index < rest.len() {
        if rest[index] == b'/' || rest[index] == b'\\' {
            index = take_sep(rest, index).unwrap_or(index + 1);
            if !out.is_empty() && !out.ends_with('/') {
                out.push('/');
            }
        } else {
            out.push(char::from(rest[index]));
            index += 1;
        }
    }
    while out.ends_with('/') {
        out.pop();
    }
    out
}

fn is_segment_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.'
}

fn is_path_byte(byte: u8) -> bool {
    is_segment_byte(byte) || byte == b'/' || byte == b'\\'
}

fn push_char(text: &str, index: usize, out: &mut String) -> usize {
    let Some(ch) = text[index..].chars().next() else {
        return index + 1;
    };
    out.push(ch);
    index + ch.len_utf8()
}

enum ProbeCode {
    Accepted,
    Refused,
    Inconclusive,
    Unrunnable,
}

fn probe_accepts(path: &Path) -> Result<bool, AuditError> {
    match run_probe(path, false)? {
        ProbeCode::Accepted => Ok(true),
        ProbeCode::Unrunnable => Ok(false),
        ProbeCode::Refused | ProbeCode::Inconclusive => {
            Ok(matches!(run_probe(path, true)?, ProbeCode::Accepted))
        }
    }
}

fn run_probe(path: &Path, cheat: bool) -> Result<ProbeCode, AuditError> {
    let cwd = ProbeDir::create()?;
    let mut cmd = Command::new(path);
    cmd.arg(if cheat {
        "--regia-cheat-probe"
    } else {
        "--regia-dev-probe"
    })
    .env_clear()
    .current_dir(&cwd.path)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    if cheat {
        cmd.env("REGIA_CHEAT", "1");
    }
    let code = match cmd.spawn() {
        Ok(mut child) => match wait_bounded(&mut child) {
            Some(0) => ProbeCode::Accepted,
            Some(2) => ProbeCode::Refused,
            _ => ProbeCode::Inconclusive,
        },
        Err(_) => ProbeCode::Unrunnable,
    };
    Ok(code)
}

struct ProbeDir {
    path: PathBuf,
}

impl ProbeDir {
    fn create() -> Result<Self, AuditError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("regia-hygiene-probe-{}-{n}", std::process::id()));
        std::fs::create_dir(&path).map_err(|_| io("probe_cwd"))?;
        Ok(Self { path })
    }
}

impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn wait_bounded(child: &mut std::process::Child) -> Option<i32> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if start.elapsed() < Duration::from_secs(2) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// A stage finding carries the privacy-scrubbed display path (home paths, keyed secrets and
/// emails replaced, like the content scan's) and the file's opaque `stage-N` identity.
fn finding(file: &StagedFile, rule: &str, source: &str, offset: usize) -> Finding {
    Finding {
        path: regia_core::privacy::scrub_report(&file.rel),
        location_id: file.id.clone(),
        rule: rule.to_owned(),
        source: source.to_owned(),
        offset,
    }
}

fn waived(rules: &Compiled, path: &str, rule: &str, report: &mut Report) -> bool {
    if rules.allowed(path, rule) {
        report.allowed_findings = report.allowed_findings.saturating_add(1);
        true
    } else {
        false
    }
}

fn push_finding(report: &mut Report, policy: &Policy, hit: Finding) -> Result<(), AuditError> {
    if report.findings.len() >= policy.max_findings {
        return Err(limit(&hit.path));
    }
    report.findings.push(hit);
    Ok(())
}

fn io(path: &str) -> AuditError {
    AuditError::new("package_hygiene_io", path)
}

fn path_err(path: &str) -> AuditError {
    AuditError::new("package_hygiene_path", path)
}

fn limit(path: &str) -> AuditError {
    AuditError::new("package_hygiene_limit", path)
}
