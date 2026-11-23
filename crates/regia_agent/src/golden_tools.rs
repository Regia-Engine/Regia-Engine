// SPDX-License-Identifier: MPL-2.0
//! The golden tools of `docs/specs/23-agent-native.md` § 3.7 (ADR-2821): `agent.golden_test` runs a
//! named golden's test and reports expected, actual and diff; `agent.bless_golden` stages a proposal
//! holding before, after and diff in the review queue. **An agent alone can never bless a golden**:
//! no protocol method writes under a `goldens` directory, under any permission or tool scope.
//!
//! The rule is held in layers:
//!
//! - `guard_params` runs in the dispatcher before every handler (built-in, extension, registered)
//!   and refuses a request whose string parameters name a path with a `goldens` component.
//! - `review::ReviewQueue::decide` refuses a golden proposal for every in-process caller, so
//!   `review.accept` with the `review` permission and the editor's Agent Changes panel leave it
//!   pending. Text changes to a golden path are refused when applied.
//! - [`scrub_golden_env`] removes `REGIA_BLESS` and `REGIA_SKIP_GOLDENS` (both spellings) from every
//!   test process the protocol starts, so the app's environment cannot turn a compare into a write.
//! - [`owner_accept`] is the only code that installs a proposal. Nothing in the dispatcher calls it;
//!   `regia golden accept` does, after a person at a terminal typed the golden's name.
//!
//! The boundary is the protocol. Project code run through the `build` permission, and an agent with
//! its own shell, are the owner's user (specs/23 § 5); the ADR says what stands in for this there.
//!
//! The actual frame comes from the plate a compare records under `REGIA_PLATE_DIR`
//! (`regia_test_utils::golden`), on pass and on fail, so no test changes for this to work.

use std::path::{Path, PathBuf};
use std::process::Command;

use regia_app::prelude::*;
use regia_render::CapturedImage;
use serde_json::{Value, json};

use crate::jobs::{JobHandle, JobRegistry, JobState, reason_error};
use crate::protocol::{INTERNAL_ERROR, INVALID_PARAMS, NOT_FOUND, RpcError};
use crate::review::{ChangeStatus, GoldenProposal, ReviewQueue, StagedChange};
use crate::run_tests::Run;
use crate::safety::AgentPermissions;
use crate::schema::Permission;

/// The directory name that holds golden images.
pub const GOLDENS_DIR: &str = "goldens";

/// The longest accepted golden name.
pub const MAX_NAME_BYTES: usize = 128;

/// Most close matches a `golden_unknown` answer carries.
pub const MAX_CLOSE_MATCHES: usize = 5;

/// Most directories a project scan visits (a bound on a pathological tree).
const MAX_SCAN_DIRS: usize = 200_000;

/// Directories a scan never enters at any depth, besides every hidden one.
const SCAN_SKIP_DIRS: &[&str] = &["target", "node_modules", "build", "vendor"];

/// Largest image side the job decodes.
const MAX_IMAGE_SIDE: u32 = 16_384;

/// Test job folders kept under the agent state folder.
const KEEP_JOB_DIRS: usize = 16;

/// Parameter keys that hold free text, never a path: [`guard_params`] does not read them.
const FREE_TEXT_KEYS: &[&str] = &[
    "reason",
    "note",
    "title",
    "text",
    "description",
    "message",
    "ron",
    "source",
    "script",
    "content",
    "summary",
    "comment",
];

/// Methods [`guard_params`] leaves alone: they read, stage or name a golden and write none.
const GUARD_EXEMPT: &[&str] = &[
    "agent.hello",
    "agent.guide",
    "agent.schema",
    "agent.changes",
    "errors.explain",
    "logs.read",
    "agent.job_status",
    "agent.job_cancel",
    "agent.golden_test",
    "agent.bless_golden",
    // Takes test names, in an alphabet without separators: `goldens` is a test target's name.
    "agent.run_tests",
    // Two images as base64: no path, and `/` is in that alphabet.
    "seeing.compare_images",
    "review.list",
    "review.diff",
    "review.accept",
    "review.reject",
];

/// The environment variables a test process under the protocol never sees.
const SCRUBBED_ENV: [&str; 2] = ["REGIA_BLESS", "REGIA_SKIP_GOLDENS"];

// ---------------------------------------------------------------------------------------------
// The guards.

/// Whether a path, by its spelling, has a `goldens` component. Both separators count, case folds
/// (Windows and macOS file systems), and the trailing dots and spaces Windows strips do not hide it.
#[must_use]
pub fn names_golden(path: &str) -> bool {
    path.split(['/', '\\']).any(|part| {
        part.trim_end_matches(['.', ' '])
            .trim_start()
            .eq_ignore_ascii_case(GOLDENS_DIR)
    })
}

/// Whether a string names a golden anywhere in it: a parameter can be a path, or text holding paths
/// (a `.render_job.ron` passed inline), so it is split on what no path token contains.
fn mentions_golden(text: &str) -> bool {
    text.split(|c: char| {
        c.is_whitespace()
            || c.is_control()
            || matches!(
                c,
                '"' | '\''
                    | '`'
                    | ','
                    | ';'
                    | ':'
                    | '='
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '<'
                    | '>'
                    | '|'
            )
    })
    .any(names_golden)
}

/// Whether `path`, resolved against `root` when it lies under it, is under a goldens directory: the
/// check for a path a handler built itself, or read from a file.
#[must_use]
pub fn under_goldens(root: &Path, path: &Path) -> bool {
    let shown = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    names_golden(&shown)
}

/// The first string under `value` that names a golden, skipping [`FREE_TEXT_KEYS`].
fn find_golden_path<'a>(key: Option<&str>, value: &'a Value) -> Option<&'a str> {
    match value {
        Value::String(text) => (key.is_none_or(|k| !FREE_TEXT_KEYS.contains(&k)) && mentions_golden(text))
            .then_some(text.as_str()),
        Value::Array(items) => items.iter().find_map(|item| find_golden_path(key, item)),
        Value::Object(map) => map.iter().find_map(|(k, v)| find_golden_path(Some(k), v)),
        _ => None,
    }
}

/// Refuses a request that names a path under a `goldens` directory (`golden_path_denied`).
///
/// # Errors
/// `F0002` with `data.reason` `golden_path_denied`.
pub(crate) fn guard_params(method: &str, params: &Value) -> Result<(), RpcError> {
    if GUARD_EXEMPT.contains(&method) {
        return Ok(());
    }
    match find_golden_path(None, params) {
        Some(path) => Err(golden_path_denied(path)),
        None => Ok(()),
    }
}

/// The `golden_path_denied` error for `path`.
#[must_use]
pub fn golden_path_denied(path: &str) -> RpcError {
    let mut error = reason_error(
        INVALID_PARAMS,
        "F0002",
        "golden_path_denied",
        format!("{path} is under a goldens directory; only the owner changes goldens"),
    );
    if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
        data.insert(
            "hint".into(),
            json!("agent.golden_test reads a golden and agent.bless_golden proposes one: see book/agents/goldens.md"),
        );
    }
    error
}

/// The golden bless and skip variables, in both spellings.
#[must_use]
pub fn scrubbed_env_names() -> Vec<String> {
    SCRUBBED_ENV
        .iter()
        .flat_map(|name| {
            let other = regia_core::brand::alternate_env_name(name).map(|(other, _)| other);
            std::iter::once((*name).to_string()).chain(other)
        })
        .collect()
}

/// Removes the golden bless and skip variables from a child's environment.
pub fn scrub_golden_env(command: &mut Command) {
    for name in scrubbed_env_names() {
        command.env_remove(name);
    }
}

// ---------------------------------------------------------------------------------------------
// Parameters.

fn opt_str<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a string"))),
    }
}

fn opt_bool(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a boolean"))),
    }
}

fn req_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    opt_str(params, key)?.ok_or_else(|| RpcError::invalid_params(format!("`{key}` is required")))
}

/// A golden's name: 1..=[`MAX_NAME_BYTES`] of ASCII letters, digits, `_`, `-`, `.` and `:`, not
/// starting with `-`.
fn check_name(name: &str) -> Result<(), RpcError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'));
    if ok {
        Ok(())
    } else {
        Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "golden_bad_name",
            format!(
                "`name` is the golden's file stem, or <owner>:<stem>: 1..={MAX_NAME_BYTES} ASCII letters, digits, `_`, `-`, `.` or `:` (got {name:?})"
            ),
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// Scanning the project.

/// One golden image found in the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldenEntry {
    /// The file stem.
    pub stem: String,
    /// The folder that owns it: the first folder above the file not named `goldens`.
    pub owner: String,
    /// Project-relative path with `/` separators.
    pub rel: String,
    /// Absolute path.
    pub path: PathBuf,
}

impl GoldenEntry {
    /// The unambiguous name `<owner>:<stem>`.
    #[must_use]
    pub fn qualified(&self) -> String {
        format!("{}:{}", self.owner, self.stem)
    }

    /// The plate names `regia_test_utils::golden` records this golden under: the PNG compare's, then
    /// the pixel compare's.
    #[must_use]
    pub fn plate_names(&self) -> [String; 2] {
        [
            safe_file_stem(&format!("{}-{}", self.owner, self.stem)),
            safe_file_stem(&format!("pixel-{}", self.stem)),
        ]
    }
}

/// Keeps ascii letters, digits, `-` and `_`, as the plate writer does.
fn safe_file_stem(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Every directory of the project a scan looks at, sorted: no hidden folder, no build output, no
/// symlink followed.
fn project_dirs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        if found.len() >= MAX_SCAN_DIRS {
            tracing::warn!(target: "regia_agent", "golden scan stopped at {MAX_SCAN_DIRS} directories");
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_dir && !name.starts_with('.') && !SCAN_SKIP_DIRS.contains(&name.as_ref()) {
                pending.push(entry.path());
            }
        }
        found.push(dir);
    }
    found.sort();
    found
}

/// A path as a project-relative `/` string.
fn rel_of(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The golden an image file is, given its path: `None` when no folder above it names a goldens
/// directory under `root`.
fn entry_for(root: &Path, path: &Path) -> Option<GoldenEntry> {
    let stem = path.file_stem()?.to_str()?.to_string();
    let rel = rel_of(root, path);
    if !names_golden(&rel) {
        return None;
    }
    let owner = path
        .parent()?
        .ancestors()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .find(|n| !n.eq_ignore_ascii_case(GOLDENS_DIR))?
        .to_string();
    Some(GoldenEntry {
        stem,
        owner,
        rel,
        path: path.to_path_buf(),
    })
}

/// Every `goldens/**/*.png` of the project, sorted by path. Only directories and file names are
/// read, so this stays cheap enough to answer a call with.
#[must_use]
pub fn scan_goldens(root: &Path) -> Vec<GoldenEntry> {
    let mut entries = Vec::new();
    for dir in project_dirs(root) {
        if !names_golden(&rel_of(root, &dir)) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            let is_png = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
            if is_png
                && file.file_type().is_ok_and(|kind| kind.is_file())
                && let Some(entry) = entry_for(root, &path)
            {
                entries.push(entry);
            }
        }
    }
    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    entries
}

/// Levenshtein distance over bytes (names are ASCII).
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let cost = usize::from(x != y);
            let next = (row[j + 1] + 1).min(row[j] + 1).min(diagonal + cost);
            diagonal = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// Up to [`MAX_CLOSE_MATCHES`] stems close to `name`: nearest first, a stem holding it counts as near.
#[must_use]
pub fn close_matches(entries: &[GoldenEntry], name: &str) -> Vec<String> {
    let wanted = name.rsplit(':').next().unwrap_or(name);
    let limit = (wanted.len() / 2).max(3);
    let mut scored: Vec<(usize, String)> = entries
        .iter()
        .filter_map(|entry| {
            let distance = if entry.stem.contains(wanted) {
                1
            } else {
                edit_distance(wanted, &entry.stem)
            };
            (distance <= limit).then(|| (distance, entry.stem.clone()))
        })
        .collect();
    scored.sort();
    scored.dedup();
    scored
        .into_iter()
        .take(MAX_CLOSE_MATCHES)
        .map(|(_, stem)| stem)
        .collect()
}

/// The entry `name` selects.
///
/// # Errors
/// `golden_unknown` with `data.close` (close matches), or `golden_ambiguous` with `data.matches`.
pub fn pick_golden<'a>(entries: &'a [GoldenEntry], name: &str) -> Result<&'a GoldenEntry, RpcError> {
    let matches: Vec<&GoldenEntry> = match name.split_once(':') {
        Some((owner, stem)) => entries
            .iter()
            .filter(|e| e.owner == owner && e.stem == stem)
            .collect(),
        None => entries.iter().filter(|e| e.stem == name).collect(),
    };
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => {
            let close = close_matches(entries, name);
            let mut error = reason_error(
                NOT_FOUND,
                "F0003",
                "golden_unknown",
                format!("no golden named {name}"),
            );
            if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
                data.insert("close".into(), json!(close));
                data.insert(
                    "hint".into(),
                    json!("a golden's name is the stem of goldens/<stem>.png; see data.close"),
                );
            }
            Err(error)
        }
        several => {
            let names: Vec<String> = several.iter().map(|e| e.qualified()).collect();
            let mut error = reason_error(
                INVALID_PARAMS,
                "F0002",
                "golden_ambiguous",
                format!("{name} is in {} folders; name one", names.len()),
            );
            if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
                data.insert("matches".into(), json!(names));
            }
            Err(error)
        }
    }
}

/// The test that compares a golden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldenTest {
    /// The package (`--package`), when the source has a manifest with one.
    pub package: Option<String>,
    /// `lib`, or the test target's name.
    pub target: String,
    /// The enclosing test function (`-- <filter> --exact`), when the name sits inside one.
    pub filter: Option<String>,
    /// Project-relative source file.
    pub file: String,
}

/// The `#[test]` function around byte `offset` of `source`. `None` when the code there is in a
/// helper function or outside any function, so the whole target has to run. A plain line scan, as
/// the source is ordinary rustfmt output; a wrong answer only widens or empties the filter.
#[must_use]
pub fn enclosing_test_fn(source: &str, offset: usize) -> Option<String> {
    let head = source.get(..offset)?;
    let mut pending = false;
    let mut current = None;
    for line in head.lines() {
        let line = line.trim_start();
        if line.starts_with("#[") && (line.starts_with("#[test]") || line.contains("::test]")) {
            pending = true;
            continue;
        }
        let signature = line
            .strip_prefix("pub ")
            .unwrap_or(line)
            .strip_prefix("async ")
            .unwrap_or_else(|| line.strip_prefix("pub ").unwrap_or(line));
        if let Some(rest) = signature.strip_prefix("fn ") {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            current = (pending && !name.is_empty()).then_some(name);
            pending = false;
        }
    }
    current
}

/// The name of the package whose manifest is nearest above `dir`, and its folder.
fn package_of(root: &Path, dir: &Path) -> Option<(String, PathBuf)> {
    for folder in dir.ancestors() {
        if !folder.starts_with(root) {
            return None;
        }
        if let Ok(text) = std::fs::read_to_string(folder.join("Cargo.toml"))
            && let Ok(manifest) = text.parse::<toml::Table>()
            && let Some(name) = manifest
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(toml::Value::as_str)
        {
            return Some((name.to_string(), folder.to_path_buf()));
        }
    }
    None
}

/// Finds where in `source` the golden's name appears as a literal, or, for a numbered name
/// (`shadow_cam3`), as the start of a format string (`"shadow_cam{index}"`).
fn literal_offset(source: &str, stem: &str) -> Option<usize> {
    let numbered = stem.trim_end_matches(|c: char| c.is_ascii_digit());
    source
        .find(&format!("\"{stem}\""))
        .or_else(|| source.find(&format!("/{stem}.png\"")))
        .or_else(|| source.find(&format!("\"{stem}.png\"")))
        .or_else(|| {
            (numbered.len() >= 4 && numbered.len() < stem.len())
                .then(|| source.find(&format!("\"{numbered}{{")))
                .flatten()
        })
}

/// The test sources a golden's name can be in: `tests/*.rs` and `tests/*/main.rs` of every package,
/// with the target each one builds.
fn test_sources(root: &Path) -> Vec<(PathBuf, String)> {
    let mut sources = Vec::new();
    for dir in project_dirs(root) {
        if dir.file_name().is_none_or(|n| n != "tests") {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                continue;
            };
            if file.file_type().is_ok_and(|kind| kind.is_dir()) {
                let main = path.join("main.rs");
                if main.is_file() {
                    sources.push((main, stem));
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                sources.push((path, stem));
            }
        }
    }
    sources
}

/// Finds the test that compares `entry`: tests of the package owning the golden first, then files
/// named for goldens, then path order; the owning package's library tests last.
///
/// # Errors
/// `("golden_no_test", message)` when no source holds the name as a literal.
pub fn find_test(root: &Path, entry: &GoldenEntry) -> Result<GoldenTest, (&'static str, String)> {
    let owner = package_of(root, entry.path.parent().unwrap_or(root));
    let in_owner = |path: &Path| owner.as_ref().is_some_and(|(_, dir)| path.starts_with(dir));
    let mut sources = test_sources(root);
    sources.sort_by_key(|(path, target)| (!in_owner(path), !target.contains("golden"), path.clone()));
    for (path, target) in sources {
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(offset) = literal_offset(&source, &entry.stem) {
            return Ok(GoldenTest {
                package: package_of(root, path.parent().unwrap_or(root)).map(|(name, _)| name),
                target,
                filter: enclosing_test_fn(&source, offset),
                file: rel_of(root, &path),
            });
        }
    }
    if let Some((package, dir)) = &owner {
        for folder in project_dirs(&dir.join("src")) {
            let Ok(files) = std::fs::read_dir(&folder) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                if let Ok(source) = std::fs::read_to_string(&path)
                    && let Some(offset) = literal_offset(&source, &entry.stem)
                {
                    return Ok(GoldenTest {
                        package: Some(package.clone()),
                        target: "lib".into(),
                        filter: enclosing_test_fn(&source, offset),
                        file: rel_of(root, &path),
                    });
                }
            }
        }
    }
    Err((
        "golden_no_test",
        format!(
            "no test names {} as a literal; run its test with agent.run_tests, or name the golden in the test",
            entry.stem
        ),
    ))
}

impl GoldenTest {
    /// The `cargo test` arguments for this test.
    ///
    /// # Errors
    /// `run_tests_bad_name` for a package, target or filter outside the allowed alphabet.
    pub fn args(&self) -> Result<Vec<String>, RpcError> {
        crate::run_tests::command_args(&json!({
            "package": self.package,
            "target": self.target,
            "filter": self.filter,
            "exact": self.filter.is_some(),
        }))
    }
}

// ---------------------------------------------------------------------------------------------
// Measuring.

/// `regia_test_utils::golden::measure_golden`'s statistics of a frame (the runtime cannot depend on
/// the test crate; a test holds the two to the same numbers).
#[must_use]
pub fn frame_stats(image: &CapturedImage) -> Value {
    let count = (image.width as usize * image.height as usize).max(1);
    let (mut total, mut white, mut black, mut object) = (0.0f64, 0usize, 0usize, 0usize);
    for pixel in image.rgba.as_chunks::<4>().0 {
        let lum = 0.2126 * f64::from(pixel[0]) + 0.7152 * f64::from(pixel[1]) + 0.0722 * f64::from(pixel[2]);
        total += lum;
        white += usize::from(lum >= 245.0);
        black += usize::from(lum <= 8.0);
        object += usize::from(pixel[0] > 8 || pixel[1] > 8 || pixel[2] > 8);
    }
    let pct = |n: usize| n as f64 / count as f64 * 100.0;
    json!({
        "mean_lum": total / count as f64,
        "near_white_pct": pct(white),
        "near_black_pct": pct(black),
        "object_pct": pct(object),
        "width": image.width,
        "height": image.height,
    })
}

/// PSNR in dB over RGB against a peak of 255; `None` for identical frames or different sizes.
fn psnr_rgb(a: &CapturedImage, b: &CapturedImage) -> Option<f64> {
    if (a.width, a.height) != (b.width, b.height) || a.rgba.len() != b.rgba.len() {
        return None;
    }
    let mut sum = 0u64;
    for (x, y) in a.rgba.as_chunks::<4>().0.iter().zip(b.rgba.as_chunks::<4>().0) {
        for channel in 0..3 {
            let d = u64::from(x[channel].abs_diff(y[channel]));
            sum += d * d;
        }
    }
    if sum == 0 {
        return None;
    }
    let mse = sum as f64 / (a.rgba.len() / 4 * 3) as f64;
    Some(10.0 * (255.0 * 255.0 / mse).log10())
}

fn hash_of(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// What the job knows when the run ends.
struct JobContext {
    root: PathBuf,
    entry: GoldenEntry,
    job_dir: PathBuf,
    test: GoldenTest,
}

/// The `golden` part of a finished job's result.
fn golden_report(context: &JobContext) -> Value {
    let root = &context.root;
    let plates = context.job_dir.join("plates");
    let expected_bytes = std::fs::read(&context.entry.path).ok();
    let mut report = json!({
        "name": context.entry.qualified(),
        "path": context.entry.rel,
        "expected": context.entry.rel,
        "expected_hash": expected_bytes.as_deref().map(hash_of),
        "actual": null,
        "actual_hash": null,
        "diff": null,
        "stats": null,
        "test": {
            "package": context.test.package,
            "target": context.test.target,
            "filter": context.test.filter,
            "file": context.test.file,
        },
    });
    let plate = context
        .entry
        .plate_names()
        .into_iter()
        .map(|name| plates.join(format!("{name}.png")))
        .find(|path| path.is_file());
    let Some(actual_path) = plate else {
        report["note"] =
            json!("the test recorded no frame: it failed before its compare, or compares without one");
        return report;
    };
    let Ok(actual_bytes) = std::fs::read(&actual_path) else {
        report["note"] = json!("the recorded frame cannot be read");
        return report;
    };
    report["actual"] = json!(rel_of(root, &actual_path));
    report["actual_hash"] = json!(hash_of(&actual_bytes));
    let Ok(actual) = crate::image::decode_png_rgba_within(&actual_bytes, MAX_IMAGE_SIDE) else {
        report["note"] = json!("the recorded frame is not a decodable PNG");
        return report;
    };
    let mut stats = frame_stats(&actual);
    let expected =
        expected_bytes.and_then(|bytes| crate::image::decode_png_rgba_within(&bytes, MAX_IMAGE_SIDE).ok());
    if let Some(expected) = &expected
        && let Ok(compared) = crate::seeing::compare_rgba(expected, &actual)
    {
        let identical = compared.same_size && compared.max_channel_diff == 0;
        stats["ssim"] = json!(compared.ssim);
        stats["max_channel_diff"] = json!(compared.max_channel_diff);
        stats["same_size"] = json!(compared.same_size);
        stats["identical"] = json!(identical);
        stats["psnr"] = json!(psnr_rgb(expected, &actual));
        let diff_path = context.job_dir.join("diff.png");
        match crate::image::png_bytes(&compared.diff)
            .and_then(|png| crate::review::write_atomic(&diff_path, &png))
        {
            Ok(()) => report["diff"] = json!(rel_of(root, &diff_path)),
            Err(error) => report["note"] = json!(format!("the diff image was not written: {error}")),
        }
    }
    report["stats"] = stats;
    report
}

/// Adds the golden report to a finished run's result.
fn finish_golden(mut result: Value, context: &JobContext) -> Value {
    let _span = tracing::info_span!(target: "regia_agent", "golden_report").entered();
    let report = golden_report(context);
    if let Some(object) = result.as_object_mut() {
        object.insert("golden".into(), report);
    }
    result
}

/// Where the staged images of proposals live under the project.
#[must_use]
pub fn staging_root(root: &Path) -> PathBuf {
    regia_core::brand::project_state_dir(root)
        .join("agent")
        .join("review")
        .join("golden_proposals")
}

/// Where a test job keeps its plates and diff, and the older job folders thinned out.
fn job_folder(root: &Path, job: &str) -> PathBuf {
    let parent = regia_core::brand::project_state_dir(root)
        .join("agent")
        .join("golden_jobs");
    if let Ok(entries) = std::fs::read_dir(&parent) {
        let mut old: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        old.sort();
        let excess = old.len().saturating_sub(KEEP_JOB_DIRS);
        for dir in old.into_iter().take(excess) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    let suffix = job.replace(':', "-");
    parent.join(format!("{:016x}-{suffix}", crate::safety::unix_ms_now()))
}

/// The worker of a `golden:<n>` job: resolves the test, then runs it through `agent.run_tests`'s
/// machinery with a plate folder in the environment.
fn run_golden_job(mut context: JobContext, program: PathBuf, env: Vec<(String, String)>, handle: JobHandle) {
    let _span = tracing::info_span!(target: "regia_agent", "golden_test").entered();
    handle.progress(None, "finding the golden's test");
    let test = match find_test(&context.root, &context.entry) {
        Ok(test) => test,
        Err((reason, message)) => {
            handle.fail(reason, message);
            return;
        }
    };
    handle.event(json!({
        "kind": "resolved", "package": test.package, "target": test.target,
        "filter": test.filter, "file": test.file,
    }));
    if handle.cancel_requested() {
        handle.finish(json!({ "ok": false }));
        return;
    }
    let args = match test.args() {
        Ok(args) => args,
        Err(error) => {
            handle.fail("run_tests_bad_name", error.message);
            return;
        }
    };
    context.test = test;
    let plates = context.job_dir.join("plates");
    if let Err(error) = std::fs::create_dir_all(&plates) {
        handle.fail(
            "golden_io",
            format!("cannot create {}: {error}", plates.display()),
        );
        return;
    }
    let mut env = env;
    env.push((
        "REGIA_PLATE_DIR".to_string(),
        plates.to_string_lossy().into_owned(),
    ));
    let mut command_line = vec![program.display().to_string()];
    command_line.extend(args.iter().cloned());
    let run = Run {
        program: program.clone(),
        args,
        root: context.root.clone(),
        env,
        command_line,
        start_message: "compiling and running",
        thread_name: "regia-golden-test",
        tally: Box::new(crate::run_tests::RunTally::default()),
        finish: Box::new(move |result| finish_golden(result, &context)),
    };
    if let Err(error) = crate::run_tests::spawn_run(run, handle.clone()) {
        handle.fail(
            "run_tests_spawn_failed",
            format!("cannot start {}: {error}", program.display()),
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The protocol methods.

/// `agent.golden_test`: starts the golden's test and answers its job id.
pub(crate) fn golden_test(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // The schema's permission is `build` (it runs project code); the result also shows the image.
    let permissions = world
        .get_resource_or_insert_with::<AgentPermissions>(Default::default)
        .clone();
    if !permissions.allows(Permission::Read) {
        return Err(permissions.denied_error(Permission::Read));
    }
    let name = req_str(params, "name")?;
    check_name(name)?;
    let dry_run = opt_bool(params, "dry_run")?.unwrap_or(false);
    // Absolute: cargo runs a test in its package's folder, and the plate folder goes in its
    // environment, so a relative project root (`--project .`) would put the plates elsewhere.
    let root = crate::req_path::project_root(world);
    let root = std::path::absolute(&root).unwrap_or(root);
    let entries = scan_goldens(&root);
    let entry = pick_golden(&entries, name)?.clone();
    if !root.join("Cargo.toml").is_file() {
        return Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "run_tests_no_manifest",
            "the project root has no Cargo.toml, so it has no test targets",
        ));
    }
    let (program, env) = crate::run_tests::host_command(world);
    if dry_run {
        let test = find_test(&root, &entry)
            .map_err(|(reason, message)| reason_error(NOT_FOUND, "F0003", reason, message))?;
        let mut command = vec![program.display().to_string()];
        command.extend(test.args()?);
        return Ok(json!({
            "dry_run": true, "name": entry.qualified(), "path": entry.rel, "command": command,
            "test": { "package": test.package, "target": test.target, "filter": test.filter, "file": test.file },
        }));
    }
    let mut registry = world.get_resource_or_insert_with::<JobRegistry>(Default::default);
    if let Some(running) = registry.running("tests").or_else(|| registry.running("golden")) {
        return Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "run_tests_busy",
            format!(
                "{running} is still running; poll it with agent.job_status or stop it with agent.job_cancel"
            ),
        ));
    }
    let (job, handle) = registry.start("golden", entry.qualified(), true);
    let context = JobContext {
        job_dir: job_folder(&root, &job),
        root,
        entry: entry.clone(),
        test: GoldenTest {
            package: None,
            target: String::new(),
            filter: None,
            file: String::new(),
        },
    };
    let worker = handle.clone();
    let spawned = std::thread::Builder::new()
        .name("regia-golden-test".into())
        .spawn(move || run_golden_job(context, program, env, worker));
    if let Err(error) = spawned {
        handle.fail("run_tests_spawn_failed", format!("no worker thread: {error}"));
    }
    Ok(json!({ "job": job, "name": entry.qualified(), "path": entry.rel }))
}

/// A refusal of `agent.bless_golden`.
fn bless_refused(reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = reason_error(INVALID_PARAMS, "F0002", reason, message);
    if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
        data.insert("hint".into(), json!("see book/agents/goldens.md"));
    }
    error
}

/// The readable text a golden proposal shows in `review.diff` and the Agent Changes panel.
fn summary_text(name: &str, hash: &str, stats: Option<&Value>, accept: Option<u64>) -> String {
    let mut text = format!("golden: {name}\nblake3: {hash}\n");
    if let Some(stats) = stats {
        for key in [
            "width",
            "height",
            "mean_lum",
            "near_white_pct",
            "near_black_pct",
            "object_pct",
            "ssim",
            "psnr",
            "max_channel_diff",
        ] {
            if let Some(value) = stats.get(key).filter(|v| !v.is_null()) {
                text.push_str(&format!("{key}: {value}\n"));
            }
        }
    }
    if let Some(id) = accept {
        text.push_str(&format!(
            "accept: regia golden accept {id} (owner, at a terminal)\n"
        ));
    }
    text
}

/// The golden part of the finished job `job`, checked against `entry`.
fn finished_job(world: &mut World, job: &str, entry: &GoldenEntry) -> Result<Value, RpcError> {
    let registry = world.get_resource_or_insert_with::<JobRegistry>(Default::default);
    let Some((kind, state, result)) = registry.outcome(job) else {
        return Err(bless_refused(
            "golden_job_not_found",
            format!("no job {job} in this editor session; run agent.golden_test first"),
        ));
    };
    if kind != "golden" {
        return Err(bless_refused(
            "golden_job_mismatch",
            format!("{job} is not an agent.golden_test job"),
        ));
    }
    if state != JobState::Done {
        return Err(bless_refused(
            "golden_job_not_run",
            format!("{job} is {}; it has to run to the end", state.name()),
        ));
    }
    let result = result.unwrap_or(Value::Null);
    let golden = result.get("golden").cloned().unwrap_or(Value::Null);
    if golden.get("name").and_then(Value::as_str) != Some(entry.qualified().as_str()) {
        return Err(bless_refused(
            "golden_job_mismatch",
            format!("{job} tested {} (not {})", golden["name"], entry.qualified()),
        ));
    }
    let ran = result["passed"].as_u64().unwrap_or(0) + result["failed"].as_u64().unwrap_or(0);
    if ran == 0 || golden["actual"].as_str().is_none() {
        return Err(bless_refused(
            "golden_job_not_run",
            format!(
                "{job} ran {ran} tests and recorded {}; there is no actual image to propose",
                if golden["actual"].is_string() {
                    "a frame"
                } else {
                    "no frame"
                }
            ),
        ));
    }
    Ok(golden)
}

/// Reads a file the job recorded and checks it still hashes as recorded.
fn read_recorded(root: &Path, rel: &str, hash: &str, what: &str) -> Result<Vec<u8>, RpcError> {
    let bytes = std::fs::read(root.join(rel))
        .map_err(|e| bless_refused("golden_stale", format!("the {what} {rel} cannot be read: {e}")))?;
    if hash_of(&bytes) != hash {
        return Err(bless_refused(
            "golden_stale",
            format!("the {what} {rel} changed since the test; run agent.golden_test again"),
        ));
    }
    Ok(bytes)
}

/// `agent.bless_golden`: stages a proposal. Writes nothing under any golden directory.
pub(crate) fn bless_golden(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = req_str(params, "name")?;
    check_name(name)?;
    let Some(reason) = opt_str(params, "reason")?.filter(|r| !r.trim().is_empty()) else {
        return Err(bless_refused(
            "golden_reason_required",
            "`reason` (why the new image is right) is required: it goes in the commit message",
        ));
    };
    let job = req_str(params, "job")?;
    let dry_run = opt_bool(params, "dry_run")?.unwrap_or(false);
    let root = crate::req_path::project_root(world);
    let root = std::path::absolute(&root).unwrap_or(root);
    let entries = scan_goldens(&root);
    let entry = pick_golden(&entries, name)?.clone();
    let golden = finished_job(world, job, &entry)?;
    let (Some(actual), Some(actual_hash), Some(expected_hash)) = (
        golden["actual"].as_str(),
        golden["actual_hash"].as_str(),
        golden["expected_hash"].as_str(),
    ) else {
        return Err(bless_refused(
            "golden_job_not_run",
            format!("{job} has no actual or expected image"),
        ));
    };
    let after = read_recorded(&root, actual, actual_hash, "actual image")?;
    let before = read_recorded(&root, &entry.rel, expected_hash, "golden")?;
    if golden["stats"]["identical"] == true || after == before {
        return Err(bless_refused(
            "golden_unchanged",
            format!("the actual image of {job} is identical to the golden; there is nothing to bless"),
        ));
    }
    let diff = golden["diff"]
        .as_str()
        .and_then(|rel| std::fs::read(root.join(rel)).ok())
        .ok_or_else(|| bless_refused("golden_stale", format!("the diff image of {job} is gone")))?;
    let session = world
        .get_resource::<crate::safety::AgentSession>()
        .cloned()
        .unwrap_or_default();
    let mut queue = crate::review::queue(world);
    if queue.pending().any(|c| c.golden.is_some() && c.path == entry.rel) {
        return Err(bless_refused(
            "golden_already_pending",
            format!(
                "{} already has a pending proposal; withdraw it with review.reject first",
                entry.rel
            ),
        ));
    }
    if queue.pending().count() >= crate::review::MAX_PENDING {
        return Err(bless_refused(
            "review_queue_full",
            "the review queue is full; wait for the owner's review",
        ));
    }
    let (Some(id), Some(set)) = (
        queue.file.next_id.checked_add(1),
        queue.file.next_set.checked_add(1),
    ) else {
        return Err(bless_refused(
            "review_queue_full",
            "the queue's ids are exhausted",
        ));
    };
    let dir = staging_root(&root).join(id.to_string());
    let dir_rel = rel_of(&root, &dir);
    let stats = golden["stats"].clone();
    let images = |file: &str| format!("{dir_rel}/{file}");
    if dry_run {
        return Ok(json!({
            "dry_run": true, "name": entry.qualified(), "path": entry.rel, "stats": stats, "staged": false,
        }));
    }
    let staged = std::fs::create_dir_all(&dir)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            for (file, bytes) in [
                ("before.png", &before),
                ("after.png", &after),
                ("diff.png", &diff),
            ] {
                crate::review::write_atomic(&dir.join(file), bytes)?;
            }
            Ok(())
        });
    if let Err(error) = staged {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(reason_error(
            INTERNAL_ERROR,
            "F0001",
            "golden_io",
            format!("cannot stage the images under {dir_rel}: {error}"),
        ));
    }
    let before_text = summary_text(&entry.qualified(), expected_hash, None, None);
    let after_text = summary_text(&entry.qualified(), actual_hash, Some(&stats), Some(id));
    let diff_counts = crate::review::line_diff(&before_text, &after_text);
    let change = StagedChange {
        id,
        set,
        path: entry.rel.clone(),
        before: Some(before_text),
        after: after_text,
        reason: reason.to_string(),
        title: format!("bless golden {}", entry.qualified()),
        client: session.client.clone(),
        session: session.session.clone(),
        model: session.model.clone(),
        images: [
            ("before".to_string(), images("before.png")),
            ("after".to_string(), images("after.png")),
            ("diff".to_string(), images("diff.png")),
        ]
        .into_iter()
        .collect(),
        created_ms: crate::safety::unix_ms_now(),
        status: ChangeStatus::Pending,
        decided_ms: None,
        decided_by: None,
        note: None,
        added: diff_counts.added,
        removed: diff_counts.removed,
        texts_dropped: false,
        golden: Some(GoldenProposal {
            name: entry.qualified(),
            job: job.to_string(),
            dir: dir_rel.clone(),
            before_hash: expected_hash.to_string(),
            after_hash: actual_hash.to_string(),
            stats: stats.to_string(),
        }),
    };
    queue.file.next_id = id;
    queue.file.next_set = set;
    queue.file.changes.push(change);
    queue.save();
    let persisted = queue.last_error.is_none();
    Ok(json!({
        "id": id, "set": set, "name": entry.qualified(), "path": entry.rel, "status": "pending",
        "before": images("before.png"), "after": images("after.png"), "diff": images("diff.png"),
        "stats": stats, "persisted": persisted,
        "accept": format!("regia golden accept {id}"),
    }))
}

// ---------------------------------------------------------------------------------------------
// The owner's side. Nothing in the dispatcher or in a handler calls these; `regia golden` does.

/// The owner's answer at the prompt: what they typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerConfirmation {
    typed: String,
}

impl OwnerConfirmation {
    /// What the owner typed at the terminal (the host checks it is one, the accept checks the text).
    #[must_use]
    pub fn typed(text: &str) -> Self {
        Self {
            typed: text.trim().to_string(),
        }
    }
}

/// A refused owner decision: a stable code and a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct OwnerError {
    /// The code.
    pub code: &'static str,
    /// What happened.
    pub message: String,
}

fn owner_error(code: &'static str, message: impl Into<String>) -> OwnerError {
    OwnerError {
        code,
        message: message.into(),
    }
}

/// What accepting did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerAccepted {
    /// The golden's name.
    pub name: String,
    /// The project-relative golden path, now holding the proposal's image.
    pub path: String,
    /// The proposer's reason.
    pub reason: String,
    /// The block appended to [`commit_message_path`].
    pub commit_message: String,
}

/// The file accepted proposals append their commit message block to, for `git commit -F`.
#[must_use]
pub fn commit_message_path(root: &Path) -> PathBuf {
    regia_core::brand::project_state_dir(root)
        .join("agent")
        .join("review")
        .join("golden_commit_message.txt")
}

/// The queue of `root`, read for an owner decision.
fn owner_queue(root: &Path) -> Result<ReviewQueue, OwnerError> {
    let queue = ReviewQueue::load(root);
    match &queue.last_error {
        Some(error) => Err(owner_error("golden_queue_unreadable", error.clone())),
        None => Ok(queue),
    }
}

/// The pending golden proposal `id`, as a clone.
fn pending_proposal(queue: &ReviewQueue, id: u64) -> Result<(StagedChange, GoldenProposal), OwnerError> {
    let Some(change) = queue.file.changes.iter().find(|c| c.id == id) else {
        return Err(owner_error(
            "review_change_not_found",
            format!("no staged change {id}"),
        ));
    };
    if change.status != ChangeStatus::Pending {
        return Err(owner_error(
            "review_already_decided",
            format!("change {id} is already {}", change.status.name()),
        ));
    }
    let Some(golden) = &change.golden else {
        return Err(owner_error(
            "golden_not_a_proposal",
            format!("change {id} is not a golden proposal; decide it in the Agent Changes panel"),
        ));
    };
    Ok((change.clone(), golden.clone()))
}

/// The pending golden proposals of the project, oldest first.
///
/// # Errors
/// `golden_queue_unreadable`.
pub fn pending_proposals(root: &Path) -> Result<Vec<StagedChange>, OwnerError> {
    let queue = owner_queue(root)?;
    Ok(queue.pending().filter(|c| c.golden.is_some()).cloned().collect())
}

/// One pending proposal, for the owner's prompt to show before asking.
///
/// # Errors
/// As [`owner_accept`] before its confirmation check.
pub fn proposal(root: &Path, id: u64) -> Result<StagedChange, OwnerError> {
    let queue = owner_queue(root)?;
    pending_proposal(&queue, id).map(|(change, _)| change)
}

/// The owner accepts proposal `id`: the golden gets the proposal's exact bytes, the change becomes
/// `accepted` by `owner`, and the reason is appended to [`commit_message_path`].
///
/// # Errors
/// `golden_confirmation_mismatch` when the typed text is not the golden's stem or name;
/// `review_conflict` when the golden moved on since the proposal; `golden_staged_changed` when the
/// staged image no longer hashes as proposed; `review_path_denied`; `review_write_failed`; and the
/// codes of [`pending_proposals`] and an unknown or decided id.
pub fn owner_accept(
    root: &Path,
    id: u64,
    confirmation: &OwnerConfirmation,
) -> Result<OwnerAccepted, OwnerError> {
    let mut queue = owner_queue(root)?;
    let (change, golden) = pending_proposal(&queue, id)?;
    let stem = golden.name.rsplit(':').next().unwrap_or(&golden.name);
    if confirmation.typed != stem && confirmation.typed != golden.name {
        return Err(owner_error(
            "golden_confirmation_mismatch",
            format!("type {stem} to bless it"),
        ));
    }
    if !names_golden(&change.path) || !change.path.to_ascii_lowercase().ends_with(".png") {
        return Err(owner_error(
            "review_path_denied",
            format!("{} is not a golden image path", change.path),
        ));
    }
    let target = regia_core::project_path::resolve_write(root, &change.path)
        .map_err(|e| owner_error("review_path_denied", format!("{}: {e}", change.path)))?;
    let current = std::fs::read(&target)
        .map_err(|e| owner_error("review_conflict", format!("{}: {e}", change.path)))?;
    if hash_of(&current) != golden.before_hash {
        return Err(owner_error(
            "review_conflict",
            format!(
                "{} changed since the proposal; ask for a new golden_test and proposal",
                change.path
            ),
        ));
    }
    let after_path = regia_core::project_path::resolve_read(root, &format!("{}/after.png", golden.dir))
        .map_err(|e| owner_error("review_path_denied", format!("{}: {e}", golden.dir)))?;
    let after = std::fs::read(&after_path)
        .map_err(|e| owner_error("golden_staged_changed", format!("{}: {e}", after_path.display())))?;
    if hash_of(&after) != golden.after_hash {
        return Err(owner_error(
            "golden_staged_changed",
            format!(
                "{} no longer holds the proposed image; reject this proposal",
                after_path.display()
            ),
        ));
    }
    crate::review::write_atomic(&target, &after)
        .map_err(|e| owner_error("review_write_failed", format!("{}: {e}", change.path)))?;
    // The golden is in. The reason is recorded and the queue saved whatever happens to the other.
    let commit_message = commit_message_block(&change, &golden);
    let message_file = commit_message_path(root);
    let existing = std::fs::read_to_string(&message_file).unwrap_or_default();
    let message_written =
        crate::review::write_atomic(&message_file, format!("{existing}{commit_message}\n").as_bytes());
    if let Some(row) = queue.file.changes.iter_mut().find(|c| c.id == id) {
        row.status = ChangeStatus::Accepted;
        row.decided_ms = Some(crate::safety::unix_ms_now());
        row.decided_by = Some("owner".to_string());
    }
    queue.save();
    if let Some(error) = &queue.last_error {
        return Err(owner_error(
            "review_write_failed",
            format!(
                "{} is installed but the queue was not saved: {error}; the reason was:\n{commit_message}",
                change.path
            ),
        ));
    }
    if let Err(error) = message_written {
        return Err(owner_error(
            "review_write_failed",
            format!(
                "{} is installed but {} was not written: {error}; the reason was:\n{commit_message}",
                change.path,
                message_file.display()
            ),
        ));
    }
    Ok(OwnerAccepted {
        name: golden.name,
        path: change.path,
        reason: change.reason,
        commit_message,
    })
}

/// Agent-supplied text for a person to read: control characters (a terminal escape, a line break
/// forging another field) are shown, not sent.
#[must_use]
pub fn plain_text(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let escaped = c.is_control().then(|| c.escape_default().collect::<String>());
            escaped
                .unwrap_or_else(|| c.to_string())
                .chars()
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The commit message block of an accepted proposal (AGENTS.md rule 4: the reason goes in it).
fn commit_message_block(change: &StagedChange, golden: &GoldenProposal) -> String {
    let stats: Value = serde_json::from_str(&golden.stats).unwrap_or(Value::Null);
    let measured = ["ssim", "psnr", "max_channel_diff"]
        .iter()
        .filter_map(|key| {
            stats
                .get(*key)
                .filter(|v| !v.is_null())
                .map(|v| format!("{key} {v}"))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let model = if change.model.is_empty() {
        String::new()
    } else {
        format!(" ({})", plain_text(&change.model))
    };
    format!(
        "bless golden {} ({})\n\nReason: {}\nMeasured against the old image: {measured}\nProposed by {}{model} from {}; accepted by the owner at a terminal (ADR-2821).\n",
        golden.name,
        change.path,
        plain_text(&change.reason),
        plain_text(&change.client),
        golden.job
    )
}

/// The owner rejects proposal `id`: nothing changes but the queue, and the staged images go.
///
/// # Errors
/// An unknown or decided id, `golden_not_a_proposal`, or an unreadable queue.
pub fn owner_reject(root: &Path, id: u64, note: Option<&str>) -> Result<(), OwnerError> {
    let mut queue = owner_queue(root)?;
    pending_proposal(&queue, id)?;
    queue.decide(id, false, "owner", note);
    queue.save();
    match &queue.last_error {
        Some(error) => Err(owner_error("review_write_failed", error.clone())),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// Fuzzing.

/// Fuzz loader (`agent.golden_test_source`): reads a Rust source the way the test scan does.
///
/// # Errors
/// A found function name that is not an identifier, marked as a bug.
pub fn fuzz_scan_source(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = String::from_utf8_lossy(bytes);
    if text.trim().is_empty() {
        return Err("no source".to_string());
    }
    if let Some(offset) = literal_offset(&text, "sponza")
        && let Some(name) = enclosing_test_fn(&text, offset)
        && !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(format!(
            "{}function name {name:?}",
            crate::fuzz_targets::BUG_PREFIX
        ));
    }
    for line in text.lines().take(256) {
        let _ = names_golden(line);
    }
    Ok(Vec::new())
}

/// Fuzz fixture of [`fuzz_scan_source`].
#[must_use]
pub fn fuzz_source_fixture() -> Vec<u8> {
    b"use x::golden_path;\n\nfn helper() {}\n\n#[test]\nfn sponza_matches() {\n    check(&golden_path(\"sponza\"));\n}\n"
        .to_vec()
}

#[cfg(test)]
mod tests;
