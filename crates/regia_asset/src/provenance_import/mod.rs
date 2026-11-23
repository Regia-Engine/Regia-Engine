// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/provenance.rs, commit c9513623f), ADR-8012.
//! Pack provenance records into `.meta` provenance (ADR-8834, `book/agents/provenance-import.md`).
//!
//! A game that kept its provenance outside the engine has one TOML record per content pack
//! (`provenance/<pack>.toml`: `schema`, `pack_id`, `status` and one `[[files]]` row per file with
//! its hash, length, origin, tool, generator, seed, review and two flags). [`import_provenance`]
//! moves every row into the provenance block of the source's `.meta` sidecar, so the project's
//! `config/provenance.ron` policy, `regia assets validate` and the final cook see it.
//!
//! The `.meta` format does not change (it stays v2): the fields the block has no slot for become
//! entries of its `extensions` map, which already exists for project-defined fields. The rules
//! (ADR-8834):
//!
//! - **Built-in fields.** `origin`, `generator` (`"none"` is no generator), `created_utc` and the
//!   reviewer (`owner`; empty or `pending` is pending; any other name is a named reviewer) map to
//!   the block's own fields. The tool label is kept verbatim as the extension `source_tool`; the
//!   block's tool is `hand` for the label `hand`, `generator` when the row names a generator, and
//!   `import` otherwise. A row field named `client`, `session` or `model` fills that built-in field.
//! - **Hash and flags.** `sha256` (lowercase), `byte_len`, `foreign_game_file` and
//!   `third_party_mod_file` are extensions with those names. The source on disk must match the
//!   recorded length and SHA-256, or the row is a conflict.
//! - **Seeds.** A non-negative seed is the block's `seed`. A negative seed (records store a signed
//!   64-bit integer, the block an unsigned one) leaves `seed` empty and is kept exactly as the
//!   extension `seed_signed`, so a generator re-run can use the recorded value.
//! - **Unknown fields are kept.** Every other row field (a voice row's `line_id`, `variant`,
//!   `text`, `voice`, `tts`, `take_approved_utc`, ...) becomes an extension under its own name, its
//!   value as text (strings as they are, numbers, booleans and dates as written, arrays as TOML
//!   inline text). Pack-level fields other than `schema`, `pack_id`, `status` and `files` are kept
//!   on every row of the pack as `pack_<name>`; `pack_id` and `pack_status` are always set. A field
//!   is refused (the record is invalid) only when its name cannot be an extension key (lowercase
//!   ascii, digits and `_`, 1 to 64 bytes), when it would overwrite a key the import sets
//!   ([`RESERVED_EXTENSION_KEYS`], or the `pack_` prefix on a row), or when its value is a table.
//! - **Duplicates are reported.** Rows for one source (same path, any case, in one record or
//!   across records) are listed in [`ImportReport::duplicates`]; identical rows migrate once, rows
//!   that differ are a conflict.
//! - **All or nothing.** A source that is not on disk (`missing_source`) or has no `.meta` yet
//!   (`missing_meta`: import or cook it first, or run `regia assets ids import`) is skipped and
//!   reported. A `.meta` that does not read, one whose provenance block differs from the migrated
//!   one, a hash or length mismatch and differing duplicates are conflicts: nothing is written and
//!   the call fails with [`CODE_PROVENANCE_IMPORT_CONFLICT`]. A `.meta` that already carries the
//!   migrated block is kept untouched, so a second run writes nothing (the import is idempotent).
//!   A `.meta` that is a symbolic link is a conflict (`meta_link`; the import neither writes
//!   through a link nor replaces it), and so are two rows whose sidecars are one file
//!   (`meta_changed`). The writes are staged: every new sidecar is written beside its original
//!   first, then renamed over it; a sidecar that changed meanwhile or a failed write puts back the
//!   ones already replaced.
//! - **Paths stay in the project.** A row path (and `under`) follows the project path rules on every
//!   host: no device name (`NUL`, `COM1.obj`), no segment of only dots and spaces, no segment ending
//!   in a dot or a space. The source and its `.meta` must resolve under `assets/`: a folder link
//!   that leads outside makes the call fail with [`CODE_PROVENANCE_IMPORT_INVALID`], writing nothing.
//!
//! The record shape (the pack and file row fields and their types) is adapted from the owner's
//! game's provenance doctor (`docs/ported-sources.toml`, ADR-8012): that reader dropped every field
//! it did not name, so voice-row fields were lost; this one reads the row as a table and keeps them.
//! The doctor's checks (allowed tools, generator re-runs, quarantine manifests) are not copied; the
//! migration code is original.

mod sha256;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use thiserror::Error;

use crate::meta::{MetaSidecar, Provenance, ProvenanceTool, ReviewerState, meta_path_for};

pub use sha256::file_sha256_hex;

/// Newest pack record `schema` read (3 is the shape this module documents; 1 and 2 had the same
/// row fields).
pub const MAX_RECORD_SCHEMA: u32 = 3;

/// Longest pack record read, in bytes.
pub const MAX_RECORD_BYTES: u64 = 16 << 20;

/// The extension keys the import sets itself; a record field may not overwrite one.
pub const RESERVED_EXTENSION_KEYS: &[&str] = &[
    "byte_len",
    "foreign_game_file",
    "pack_id",
    "pack_status",
    "seed_signed",
    "sha256",
    "source_tool",
    "third_party_mod_file",
];

/// The prefix of pack-level fields kept on each row (`pack_<name>`); reserved for them.
pub const PACK_FIELD_PREFIX: &str = "pack_";

/// Code: rows conflict with the project (hash, existing provenance, unreadable `.meta`, differing
/// duplicates); nothing was written.
pub const CODE_PROVENANCE_IMPORT_CONFLICT: &str = "provenance_import_conflict";
/// Code: a record does not parse or breaks a rule.
pub const CODE_PROVENANCE_IMPORT_INVALID: &str = "provenance_import_invalid";
/// Code: a record (or the folder holding them) does not read.
pub const CODE_PROVENANCE_IMPORT_UNREADABLE: &str = "provenance_import_unreadable";
/// Code: a `.meta` could not be written.
pub const CODE_PROVENANCE_IMPORT_WRITE_FAILED: &str = "provenance_import_write_failed";

/// Every code this module reports.
pub const PROVENANCE_IMPORT_CODES: &[&str] = &[
    CODE_PROVENANCE_IMPORT_CONFLICT,
    CODE_PROVENANCE_IMPORT_INVALID,
    CODE_PROVENANCE_IMPORT_UNREADABLE,
    CODE_PROVENANCE_IMPORT_WRITE_FAILED,
];

/// The conflict kinds of [`ImportConflict::kind`].
pub const CONFLICT_KINDS: &[&str] = &[
    "byte_len_mismatch",
    "duplicate_differs",
    "hash_mismatch",
    "meta_changed",
    "meta_link",
    "meta_unreadable",
    "provenance_differs",
    "source_unreadable",
];

/// Row fields the import reads itself (everything else is kept as an extension).
const ROW_FIELDS: &[&str] = &[
    "path",
    "sha256",
    "byte_len",
    "origin",
    "tool",
    "generator",
    "seed",
    "created_utc",
    "reviewer",
    "foreign_game_file",
    "third_party_mod_file",
];

/// Built-in block fields a record row may not carry as extensions (the policy refuses them as
/// extension names); `client`, `session` and `model` fill the built-in field instead.
const BUILTIN_ONLY: &[&str] = &["client", "session", "model"];

/// One pack provenance record, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackRecord {
    /// Where the record came from (its path as given), for reports.
    pub source: String,
    /// The record's `schema` number.
    pub schema: u32,
    /// The pack id.
    pub pack_id: String,
    /// The pack status (for example `approved` or `quarantine`), kept as `pack_status`.
    pub status: String,
    /// The rows, in record order.
    pub rows: Vec<PackRow>,
}

/// One file row of a pack record, with its migrated provenance block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackRow {
    /// The path relative to the pack's content root (`/`-separated).
    pub path: String,
    /// Recorded SHA-256, lowercase hex.
    pub sha256: String,
    /// Recorded length in bytes.
    pub byte_len: u64,
    /// The `.meta` provenance block the row migrates to.
    pub provenance: Provenance,
}

/// Rows for one source found more than once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DuplicateRow {
    /// The source, relative to `assets/` (the first spelling seen).
    pub path: String,
    /// The record each row came from, in input order (a record repeats when it lists the path twice).
    pub records: Vec<String>,
    /// Whether every row is the same (then it migrates once; otherwise it is a conflict).
    pub identical: bool,
}

/// One row the import refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImportConflict {
    /// The source, relative to `assets/`.
    pub path: String,
    /// The record the row came from.
    pub record: String,
    /// One of [`CONFLICT_KINDS`].
    pub kind: String,
    /// What differs.
    pub detail: String,
}

/// What an import did (or, with `dry_run`, would do). Paths are relative to `assets/`, sorted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    /// Whether the `.meta` files were written (false for a dry run or a refused import).
    pub applied: bool,
    /// Records read.
    pub records: usize,
    /// Rows read across all records.
    pub rows: usize,
    /// Sources whose `.meta` got (or would get) the migrated provenance.
    pub written: Vec<String>,
    /// Sources whose `.meta` already carries it (left untouched).
    pub kept: Vec<String>,
    /// Rows whose source is not on disk (skipped).
    pub missing_source: Vec<String>,
    /// Rows whose source has no `.meta` yet (skipped: import or cook it first).
    pub missing_meta: Vec<String>,
    /// Sources more than one row names.
    pub duplicates: Vec<DuplicateRow>,
    /// Rows refused; any of these means nothing was written.
    pub conflicts: Vec<ImportConflict>,
}

/// Errors importing pack provenance records.
#[derive(Debug, Error)]
pub enum ProvenanceImportError {
    /// A record or folder could not be read.
    #[error("cannot read {path}: {source}")]
    Unreadable {
        /// The path.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// A record does not parse or breaks a rule.
    #[error("{0}")]
    Invalid(String),
    /// Rows conflict with the project; nothing was written.
    #[error("{} row(s) conflict; nothing was written", .0.conflicts.len())]
    Conflict(Box<ImportReport>),
    /// A `.meta` could not be written.
    #[error("cannot write {path}: {source}")]
    Write {
        /// The `.meta` path.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
}

impl ProvenanceImportError {
    /// The stable lowercase code ([`PROVENANCE_IMPORT_CODES`]).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unreadable { .. } => CODE_PROVENANCE_IMPORT_UNREADABLE,
            Self::Invalid(_) => CODE_PROVENANCE_IMPORT_INVALID,
            Self::Conflict(_) => CODE_PROVENANCE_IMPORT_CONFLICT,
            Self::Write { .. } => CODE_PROVENANCE_IMPORT_WRITE_FAILED,
        }
    }
}

/// Whether `name` can be an extension key (the policy's rule: lowercase ascii, digits and `_`,
/// 1 to 64 bytes).
fn is_key(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Why `path` is not a safe `/`-separated relative path, if it is not.
fn check_rel_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("is empty");
    }
    if path.contains('\\') {
        return Err("uses `\\` (paths are `/`-separated)");
    }
    if path.starts_with('/') || path.contains(':') {
        return Err("is absolute");
    }
    if path.chars().any(char::is_control) {
        return Err("holds a control character");
    }
    if path.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err("has an empty, `.` or `..` component");
    }
    // The project path rules on top (every host, since a record written on Linux is imported on
    // Windows too): device names (`NUL`, `mesh/COM1.obj`, `aux.obj`), segments of only dots and
    // spaces (`.. `, `...`) and segments ending in a dot or a space (`bar.obj.`).
    use regia_core::project_path::{ProjectPathError, check_relative};
    match check_relative(path) {
        Ok(_) => {}
        Err(ProjectPathError::Device(_)) => {
            return Err("names a reserved device (`CON`, `NUL`, `COM1`, ...)");
        }
        Err(ProjectPathError::ParentDir) => {
            return Err("has a component of only dots and spaces");
        }
        Err(ProjectPathError::TrailingDotOrSpace) => {
            return Err("has a component ending in a dot or a space");
        }
        Err(_) => return Err("is not a safe project path"),
    }
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".meta") {
        return Err("names a .meta sidecar, not a source");
    }
    Ok(())
}

/// A field value as extension text: strings as they are; numbers, booleans, dates and arrays as
/// TOML writes them. A table is refused.
fn value_text(value: &toml::Value) -> Result<String, &'static str> {
    match value {
        toml::Value::String(s) => Ok(s.clone()),
        toml::Value::Table(_) => Err("is a table (an extension holds one value)"),
        other => Ok(other.to_string()),
    }
}

struct RowReader<'a> {
    record: &'a str,
    index: usize,
    table: &'a toml::Table,
}

impl RowReader<'_> {
    fn bad(&self, why: String) -> ProvenanceImportError {
        ProvenanceImportError::Invalid(format!("{}: files[{}]: {why}", self.record, self.index))
    }

    fn string(&self, key: &str) -> Result<String, ProvenanceImportError> {
        match self.table.get(key) {
            Some(toml::Value::String(s)) => Ok(s.clone()),
            Some(toml::Value::Datetime(d)) if key == "created_utc" => Ok(d.to_string()),
            Some(_) => Err(self.bad(format!("`{key}` must be a string"))),
            None => Err(self.bad(format!("`{key}` is missing"))),
        }
    }

    fn boolean(&self, key: &str) -> Result<bool, ProvenanceImportError> {
        match self.table.get(key) {
            Some(toml::Value::Boolean(b)) => Ok(*b),
            Some(_) => Err(self.bad(format!("`{key}` must be true or false"))),
            None => Err(self.bad(format!("`{key}` is missing"))),
        }
    }
}

impl PackRecord {
    /// Parses a pack record; `source` names it in errors and reports.
    ///
    /// # Errors
    ///
    /// [`ProvenanceImportError::Invalid`] for bad TOML, a missing or mistyped field, a newer
    /// `schema`, an unsafe path, a malformed hash, or a field that cannot be kept (see the module
    /// docs).
    pub fn parse(source: &str, text: &str) -> Result<Self, ProvenanceImportError> {
        let bad = |why: String| ProvenanceImportError::Invalid(format!("{source}: {why}"));
        let doc: toml::Table = toml::from_str(text).map_err(|e| bad(format!("not a TOML record: {e}")))?;
        let schema = match doc.get("schema") {
            Some(toml::Value::Integer(n)) if (1..=i64::from(MAX_RECORD_SCHEMA)).contains(n) => *n as u32,
            Some(toml::Value::Integer(n)) => {
                return Err(bad(format!(
                    "schema {n} is not supported (1 to {MAX_RECORD_SCHEMA})"
                )));
            }
            Some(_) => return Err(bad("`schema` must be an integer".into())),
            None => return Err(bad("`schema` is missing".into())),
        };
        let text_field = |key: &str| match doc.get(key) {
            Some(toml::Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(bad(format!("`{key}` must be a string"))),
            None => Err(bad(format!("`{key}` is missing"))),
        };
        let pack_id = text_field("pack_id")?;
        if pack_id.trim().is_empty() {
            return Err(bad("`pack_id` is empty".into()));
        }
        let status = text_field("status")?;
        let mut pack_ext = BTreeMap::new();
        pack_ext.insert("pack_id".to_string(), pack_id.clone());
        pack_ext.insert("pack_status".to_string(), status.clone());
        for (key, value) in &doc {
            if matches!(key.as_str(), "schema" | "pack_id" | "status" | "files") {
                continue;
            }
            let name = format!("{PACK_FIELD_PREFIX}{key}");
            if !is_key(&name) {
                return Err(bad(format!(
                    "pack field `{key}` cannot be kept: `{name}` must be lowercase ascii, digits and `_` (1 to 64 bytes)"
                )));
            }
            if pack_ext.contains_key(&name) {
                return Err(bad(format!(
                    "pack field `{key}` would overwrite the extension `{name}` the import sets"
                )));
            }
            let text = value_text(value).map_err(|why| bad(format!("pack field `{key}` {why}")))?;
            pack_ext.insert(name, text);
        }
        let rows = match doc.get("files") {
            None => Vec::new(),
            Some(toml::Value::Array(rows)) => rows
                .iter()
                .enumerate()
                .map(|(index, row)| match row {
                    toml::Value::Table(table) => parse_row(
                        &RowReader {
                            record: source,
                            index,
                            table,
                        },
                        &pack_ext,
                    ),
                    _ => Err(bad(format!("files[{index}] must be a table"))),
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(bad("`files` must be an array of tables".into())),
        };
        Ok(Self {
            source: source.to_string(),
            schema,
            pack_id,
            status,
            rows,
        })
    }
}

fn parse_row(
    r: &RowReader<'_>,
    pack_ext: &BTreeMap<String, String>,
) -> Result<PackRow, ProvenanceImportError> {
    let path = r.string("path")?;
    check_rel_path(&path).map_err(|why| r.bad(format!("path {path:?} {why}")))?;
    let sha256 = r.string("sha256")?.to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(r.bad(format!("{path}: `sha256` must be 64 hex digits")));
    }
    let byte_len = match r.table.get("byte_len") {
        Some(toml::Value::Integer(n)) if *n >= 0 => *n as u64,
        Some(_) => return Err(r.bad(format!("{path}: `byte_len` must be a non-negative integer"))),
        None => return Err(r.bad(format!("{path}: `byte_len` is missing"))),
    };
    let origin = r.string("origin")?;
    let tool_label = r.string("tool")?;
    let generator = r.string("generator")?;
    let created_utc = r.string("created_utc")?;
    let reviewer = r.string("reviewer")?;
    let foreign = r.boolean("foreign_game_file")?;
    let third_party = r.boolean("third_party_mod_file")?;
    let seed = match r.table.get("seed") {
        None => None,
        Some(toml::Value::Integer(n)) => Some(*n),
        Some(_) => return Err(r.bad(format!("{path}: `seed` must be an integer"))),
    };

    let generator = Some(generator).filter(|g| !g.is_empty() && g != "none");
    let tool = if tool_label.eq_ignore_ascii_case("hand") {
        ProvenanceTool::Hand
    } else if generator.is_some() {
        ProvenanceTool::Generator
    } else {
        ProvenanceTool::Import
    };
    let reviewer = match reviewer.as_str() {
        "owner" => ReviewerState::Owner,
        "" | "pending" => ReviewerState::Pending,
        other => ReviewerState::Named(other.to_string()),
    };
    let mut extensions = pack_ext.clone();
    extensions.insert("sha256".into(), sha256.clone());
    extensions.insert("byte_len".into(), byte_len.to_string());
    extensions.insert("source_tool".into(), tool_label);
    extensions.insert("foreign_game_file".into(), foreign.to_string());
    extensions.insert("third_party_mod_file".into(), third_party.to_string());
    let seed = match seed {
        Some(n) if n < 0 => {
            extensions.insert("seed_signed".into(), n.to_string());
            None
        }
        Some(n) => Some(n as u64),
        None => None,
    };
    let mut provenance = Provenance {
        origin: Some(origin).filter(|o| !o.is_empty()),
        tool,
        generator,
        seed,
        client: None,
        session: None,
        model: None,
        created_utc: Some(created_utc).filter(|c| !c.is_empty()),
        reviewer,
        extensions: BTreeMap::new(),
    };
    for (key, value) in r.table {
        if ROW_FIELDS.contains(&key.as_str()) {
            continue;
        }
        let text = value_text(value).map_err(|why| r.bad(format!("{path}: field `{key}` {why}")))?;
        if BUILTIN_ONLY.contains(&key.as_str()) {
            let slot = match key.as_str() {
                "client" => &mut provenance.client,
                "session" => &mut provenance.session,
                _ => &mut provenance.model,
            };
            *slot = Some(text).filter(|t| !t.is_empty());
            continue;
        }
        if !is_key(key) {
            return Err(r.bad(format!(
                "{path}: field `{key}` cannot be kept: an extension key is lowercase ascii, digits and `_` (1 to 64 bytes)"
            )));
        }
        if RESERVED_EXTENSION_KEYS.contains(&key.as_str()) || key.starts_with(PACK_FIELD_PREFIX) {
            return Err(r.bad(format!(
                "{path}: field `{key}` would overwrite an extension the import sets"
            )));
        }
        extensions.insert(key.clone(), text);
    }
    provenance.extensions = extensions;
    Ok(PackRow {
        path,
        sha256,
        byte_len,
        provenance,
    })
}

/// Reads and parses one record file (at most [`MAX_RECORD_BYTES`]); `label` names it in reports.
///
/// # Errors
///
/// [`ProvenanceImportError::Unreadable`] when the file does not read (or is too large, or not
/// UTF-8), [`ProvenanceImportError::Invalid`] when it does not parse.
pub fn read_pack_record(path: &Path, label: &str) -> Result<PackRecord, ProvenanceImportError> {
    use std::io::Read as _;
    let unreadable = |source| ProvenanceImportError::Unreadable {
        path: path.to_path_buf(),
        source,
    };
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(unreadable(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("larger than {MAX_RECORD_BYTES} bytes"),
        )));
    }
    let text = String::from_utf8(bytes)
        .map_err(|e| unreadable(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
    PackRecord::parse(label, &text)
}

/// The record files `path` names: the file itself, or every `*.toml` file directly in the folder,
/// sorted by name. In a folder, a `*.toml` that is a symbolic link counts when its target is a
/// regular file inside the folder; a folder named `*.toml` is skipped.
///
/// # Errors
///
/// [`ProvenanceImportError::Unreadable`] when the path or folder does not read, or when a `*.toml`
/// in the folder is a FIFO, socket or device, or a link that dangles, leaves the folder or names
/// something other than a regular file.
pub fn record_files(path: &Path) -> Result<Vec<PathBuf>, ProvenanceImportError> {
    let unreadable = |source| ProvenanceImportError::Unreadable {
        path: path.to_path_buf(),
        source,
    };
    let meta = std::fs::metadata(path).map_err(unreadable)?;
    if meta.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !meta.is_dir() {
        // A FIFO or a device would block the reader.
        return Err(unreadable(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file or folder",
        )));
    }
    let folder = path.canonicalize().map_err(unreadable)?;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let name = entry.file_name();
        let is_toml = name
            .to_str()
            .is_some_and(|n| !n.starts_with('.') && n.to_ascii_lowercase().ends_with(".toml"));
        if !is_toml {
            continue;
        }
        let entry_path = entry.path();
        let refuse = |why: &str| ProvenanceImportError::Unreadable {
            path: entry_path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, why.to_string()),
        };
        let kind = entry_path
            .symlink_metadata()
            .map_err(|source| ProvenanceImportError::Unreadable {
                path: entry_path.clone(),
                source,
            })?
            .file_type();
        if kind.is_dir() {
            // A folder named `*.toml` is not a record (the folder rule reads files directly inside).
            continue;
        }
        if kind.is_symlink() {
            // A link to a regular file inside the folder is a record; one that leaves the folder,
            // dangles or names anything else is refused, never skipped.
            let target = entry_path
                .canonicalize()
                .map_err(|_| refuse("a link whose target does not resolve"))?;
            if !target.starts_with(&folder) {
                return Err(refuse("a link to a file outside the record folder"));
            }
            if !target.symlink_metadata().is_ok_and(|m| m.is_file()) {
                return Err(refuse("a link to something that is not a regular file"));
            }
        } else if !kind.is_file() {
            // A FIFO, socket or device named `*.toml` would block or mislead the reader.
            return Err(refuse("not a regular file"));
        }
        out.push(entry_path);
    }
    out.sort();
    Ok(out)
}

/// Options of [`import_provenance`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// Folder under `assets/` that holds the pack's content root (`/`-separated); `None` means the
    /// record paths are relative to `assets/` itself.
    pub under: Option<String>,
    /// Report what would be written without writing.
    pub dry_run: bool,
}

/// What a `.meta` path holds.
enum Existing {
    Missing,
    Meta(Box<MetaSidecar>, Vec<u8>),
    Unreadable(String),
}

fn read_meta(path: &Path) -> Existing {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Existing::Missing,
        Err(e) => return Existing::Unreadable(e.to_string()),
        // A FIFO or a device in its place would block or mislead the read.
        Ok(m) if !m.is_file() => return Existing::Unreadable("not a regular file".into()),
        Ok(_) => {}
    }
    match std::fs::read(path) {
        Ok(bytes) => match MetaSidecar::from_ron(&bytes) {
            Ok(meta) => Existing::Meta(Box::new(meta), bytes),
            Err(e) => Existing::Unreadable(e.to_string()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Existing::Missing,
        Err(e) => Existing::Unreadable(e.to_string()),
    }
}

/// Moves every row of `records` into the provenance block of its source's `.meta` under
/// `assets_root` (the project's `assets/` folder); see the module docs for the rules.
///
/// # Errors
///
/// [`ProvenanceImportError::Invalid`] for an unsafe `under` or a source or `.meta` that resolves
/// outside `assets_root`, [`ProvenanceImportError::Conflict`] (nothing written) when any row
/// conflicts, [`ProvenanceImportError::Write`] when a `.meta` cannot be written (the sidecars already
/// replaced get their old bytes back), [`ProvenanceImportError::Unreadable`] when `assets_root`
/// exists but does not resolve.
pub fn import_provenance(
    assets_root: &Path,
    records: &[PackRecord],
    options: &ImportOptions,
) -> Result<ImportReport, ProvenanceImportError> {
    let _span = tracing::info_span!(target: "regia_asset", "provenance_import").entered();
    let under = options
        .under
        .as_deref()
        .map(|u| u.trim_matches('/'))
        .filter(|u| !u.is_empty());
    if let Some(under) = under {
        check_rel_path(under)
            .map_err(|why| ProvenanceImportError::Invalid(format!("under {under:?} {why}")))?;
    }
    let mut report = ImportReport {
        records: records.len(),
        rows: records.iter().map(|r| r.rows.len()).sum(),
        ..ImportReport::default()
    };

    // Group the rows by destination path, folding ASCII case (one file on a case-insensitive disk).
    let mut groups: BTreeMap<String, Vec<(String, &PackRecord, &PackRow)>> = BTreeMap::new();
    for record in records {
        for row in &record.rows {
            let dest = match under {
                Some(u) => format!("{u}/{}", row.path),
                None => row.path.clone(),
            };
            groups
                .entry(dest.to_ascii_lowercase())
                .or_default()
                .push((dest, record, row));
        }
    }

    // A missing `assets/` holds nothing (every row is `missing_source`); otherwise each path is
    // checked against it.
    let root_exists = assets_root.symlink_metadata().is_ok();
    let mut seen_meta: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut to_write: Vec<(String, PathBuf, MetaSidecar, Vec<u8>)> = Vec::new();
    for group in groups.values() {
        let (dest, record, row) = &group[0];
        let conflict = |kind: &str, detail: String| ImportConflict {
            path: dest.clone(),
            record: record.source.clone(),
            kind: kind.to_string(),
            detail,
        };
        if group.len() > 1 {
            let identical = group.iter().all(|(d, _, r)| d == dest && *r == *row);
            report.duplicates.push(DuplicateRow {
                path: dest.clone(),
                records: group.iter().map(|(_, rec, _)| rec.source.clone()).collect(),
                identical,
            });
            if !identical {
                report.conflicts.push(conflict(
                    "duplicate_differs",
                    format!("{} rows name this source and they differ; keep one", group.len()),
                ));
                continue;
            }
        }
        let source = dest
            .split('/')
            .fold(assets_root.to_path_buf(), |path, part| path.join(part));
        let meta_file = meta_path_for(&source);
        if root_exists {
            // A folder link under `assets/` can lead anywhere: the source and its sidecar must
            // resolve under `assets/` before either is read, so nothing outside is read or written.
            for path in [&source, &meta_file] {
                stay_under(assets_root, path, dest)?;
            }
        }
        match std::fs::metadata(&source) {
            Ok(m) if m.is_file() => {
                if m.len() != row.byte_len {
                    report.conflicts.push(conflict(
                        "byte_len_mismatch",
                        format!("recorded {} bytes, on disk {}", row.byte_len, m.len()),
                    ));
                    continue;
                }
            }
            Ok(_) => {
                report.missing_source.push(dest.clone());
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report.missing_source.push(dest.clone());
                continue;
            }
            Err(e) => {
                report
                    .conflicts
                    .push(conflict("source_unreadable", e.to_string()));
                continue;
            }
        }
        match file_sha256_hex(&source) {
            Ok(hash) if hash == row.sha256 => {}
            Ok(hash) => {
                report.conflicts.push(conflict(
                    "hash_mismatch",
                    format!("recorded sha256 {}, on disk {hash}", row.sha256),
                ));
                continue;
            }
            Err(e) => {
                report
                    .conflicts
                    .push(conflict("source_unreadable", e.to_string()));
                continue;
            }
        }
        match read_meta(&meta_file) {
            Existing::Missing => report.missing_meta.push(dest.clone()),
            Existing::Unreadable(why) => report.conflicts.push(conflict("meta_unreadable", why)),
            Existing::Meta(meta, bytes) => {
                // Two rows whose sidecars are one file (a linked `.meta`, or two spellings through
                // a folder link): one write would change the other's sidecar under it.
                let canonical = match meta_file.canonicalize() {
                    Ok(c) => c,
                    Err(e) => {
                        report.conflicts.push(conflict("meta_unreadable", e.to_string()));
                        continue;
                    }
                };
                if let Some(first) = seen_meta.get(&canonical) {
                    report.conflicts.push(conflict(
                        "meta_changed",
                        format!(
                            "this .meta is the same file as the .meta of {first} (a link), so \
                             writing one would change the other; give each source its own .meta"
                        ),
                    ));
                    continue;
                }
                seen_meta.insert(canonical, dest.clone());
                let empty = Provenance::default();
                let same = meta.provenance.as_ref() == Some(&row.provenance);
                let other = meta.provenance.as_ref().is_some_and(|p| *p != empty);
                let linked = meta_file
                    .symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_symlink());
                if same {
                    report.kept.push(dest.clone());
                } else if other {
                    report.conflicts.push(conflict(
                        "provenance_differs",
                        "the .meta already has another provenance block; remove it to import, or \
                         stop importing this row"
                            .into(),
                    ));
                } else if linked {
                    report.conflicts.push(conflict(
                        "meta_link",
                        "the .meta is a symbolic link; the import neither writes through a link nor \
                         replaces it (give the source its own .meta file)"
                            .into(),
                    ));
                } else {
                    let mut meta = *meta;
                    meta.provenance = Some(row.provenance.clone());
                    to_write.push((dest.clone(), meta_file, meta, bytes));
                }
            }
        }
    }
    report.written = to_write.iter().map(|(d, ..)| d.clone()).collect();
    sort_report(&mut report);
    if !report.conflicts.is_empty() {
        return Err(ProvenanceImportError::Conflict(Box::new(report)));
    }
    if options.dry_run {
        return Ok(report);
    }
    write_all_or_nothing(&to_write, &mut report)?;
    report.applied = true;
    Ok(report)
}

/// Checks that `path` (built under `assets_root`) resolves under it; `dest` names the row.
fn stay_under(assets_root: &Path, path: &Path, dest: &str) -> Result<(), ProvenanceImportError> {
    use regia_core::project_path::{ProjectPathError, check_under_root};
    match check_under_root(assets_root, path) {
        Ok(()) => Ok(()),
        Err(ProjectPathError::Io(why)) => Err(ProvenanceImportError::Unreadable {
            path: assets_root.to_path_buf(),
            source: std::io::Error::other(why),
        }),
        Err(_) => Err(ProvenanceImportError::Invalid(format!(
            "{dest}: {} leads outside assets/ (through a symbolic link); nothing was written",
            path.display()
        ))),
    }
}

/// Writes every sidecar of `to_write` or none (as far as the filesystem allows): every new
/// sidecar is staged as a temporary file beside it first (a write error removes the staged files
/// and changes nothing); then each original is re-read and the staged file renamed over it. When
/// an original changed since the import read it (`meta_changed`) or a rename fails, the sidecars
/// already replaced get their old bytes back and nothing else is renamed.
fn write_all_or_nothing(
    to_write: &[(String, PathBuf, MetaSidecar, Vec<u8>)],
    report: &mut ImportReport,
) -> Result<(), ProvenanceImportError> {
    let mut staged: Vec<PathBuf> = Vec::with_capacity(to_write.len());
    let unstage = |staged: &[PathBuf]| {
        for tmp in staged {
            let _ = std::fs::remove_file(tmp);
        }
    };
    for (dest, meta_file, meta, _) in to_write {
        let text = match meta.to_ron_string() {
            Ok(text) => text,
            Err(e) => {
                unstage(&staged);
                return Err(ProvenanceImportError::Invalid(format!("{dest}: {e}")));
            }
        };
        match stage(meta_file, text.as_bytes()) {
            Ok(tmp) => staged.push(tmp),
            Err(source) => {
                unstage(&staged);
                return Err(ProvenanceImportError::Write {
                    path: meta_file.clone(),
                    source,
                });
            }
        }
    }
    // Restores the sidecars already replaced (`done` of them) from the bytes the import read.
    let restore = |done: usize| -> Vec<String> {
        let mut lost = Vec::new();
        for (dest, meta_file, _, before) in &to_write[..done] {
            if replace_atomic(meta_file, before).is_err() {
                lost.push(dest.clone());
            }
        }
        lost
    };
    for (i, ((dest, meta_file, _, before), tmp)) in to_write.iter().zip(&staged).enumerate() {
        // The sidecar changed since it was read (an editor or a cook wrote it): undo and leave.
        let current = std::fs::symlink_metadata(meta_file)
            .ok()
            .filter(|m| m.is_file())
            .and_then(|_| std::fs::read(meta_file).ok());
        if current.as_deref() != Some(before.as_slice()) {
            unstage(&staged[i..]);
            let lost = restore(i);
            report.written.clear();
            report.conflicts.push(ImportConflict {
                path: dest.clone(),
                record: String::new(),
                kind: "meta_changed".into(),
                detail: if lost.is_empty() {
                    "the .meta changed while the import ran; nothing was written, run it again".into()
                } else {
                    format!(
                        "the .meta changed while the import ran; restoring {} failed, check them",
                        lost.join(", ")
                    )
                },
            });
            sort_report(report);
            return Err(ProvenanceImportError::Conflict(Box::new(std::mem::take(report))));
        }
        if let Err(source) = std::fs::rename(tmp, meta_file) {
            unstage(&staged[i..]);
            let _ = restore(i);
            return Err(ProvenanceImportError::Write {
                path: meta_file.clone(),
                source,
            });
        }
    }
    Ok(())
}

fn sort_report(report: &mut ImportReport) {
    report.written.sort();
    report.kept.sort();
    report.missing_source.sort();
    report.missing_meta.sort();
    report.duplicates.sort_by(|a, b| a.path.cmp(&b.path));
    report.conflicts.sort_by(|a, b| a.path.cmp(&b.path));
}

static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to a new temporary file beside `path` and returns its path.
fn stage(path: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    // `create_new`: never open (or follow) something already at the temporary name.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    match file.write_all(bytes) {
        Ok(()) => Ok(tmp),
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Replaces `path` with `bytes` atomically (a temporary file beside it, then a rename).
fn replace_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = stage(path, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}
