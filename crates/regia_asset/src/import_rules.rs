// SPDX-License-Identifier: MPL-2.0
//! `config/import_rules.ron` v1: project-wide import settings rules (`docs/specs/12-assets.md`
//! § 10, ADR-3321).
//!
//! A rule gives every source under a project-relative glob the same partial importer settings —
//! "every texture under `assets/ui/` stays uncompressed", "every mesh under `assets/props/` gets
//! convex collision" — without editing each `.meta`. Precedence, low to high: importer default,
//! file-name inference (a texture's `_n` suffix), the matching rules in file order (later rules
//! win), then the explicit fields of the asset's own `.meta`.
//!
//! The rules are resolved at cook time: a `.meta` keeps only its explicit overrides. The matched
//! rules of each source are hashed into its cook key (`regia_asset_pipeline::cook_inputs`), so
//! editing one rule recooks exactly the assets it matches. `regia assets import-rules check`
//! reports each asset's effective settings and which rule (if any) set each field, plus the
//! codes `import_rule_unused` (a rule matches nothing) and `import_rule_unknown_field` (a field
//! the importer does not know).
//!
//! This module is the runtime-home of the format (both the desktop pipeline and the agent
//! protocol read it; neither may disagree with the other). Settings merging is a text-level scan
//! of the RON object (`SettingsObject`): a merged value passes through byte-for-byte, so whatever
//! each importer's settings parser accepts keeps working unchanged. (A semantic merge through
//! `ron::Value` is not used: it drops bare enum identifiers like `Some(normal)` to unit.)

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Path of the rules file inside a project.
pub const IMPORT_RULES_FILE: &str = "config/import_rules.ron";

/// The `format_version` of `config/import_rules.ron`.
pub const IMPORT_RULES_FORMAT_VERSION: u32 = 1;

/// Longest glob (bytes) [`glob_match`] takes; a longer pattern or path never matches.
const MAX_GLOB_LEN: usize = 512;

/// Longest path (bytes) [`glob_match`] takes; a longer pattern or path never matches.
const MAX_PATH_LEN: usize = 4096;

/// One rule of `config/import_rules.ron`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRule {
    /// Project-relative glob (`*` within a segment, `**` across, `?` one character).
    pub glob: String,
    /// Only this importer's sources (`png`, `gltf`, `audio`, ...); every importer when absent.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "plain_option")]
    pub importer: Option<String>,
    /// Partial importer settings, the same RON object an importer's `.meta` `settings` carries.
    pub settings: String,
}

/// (De)serialize an `Option<T>` as a plain `T` when set, so rules read `importer: "png"`.
mod plain_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer, T: Serialize>(value: &Option<T>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => v.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
        T::deserialize(d).map(Some)
    }
}

/// `config/import_rules.ron` v1.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRulesFile {
    /// Format version ([`IMPORT_RULES_FORMAT_VERSION`]).
    #[serde(default = "default_version")]
    pub format_version: u32,
    /// The rules, applied in order; later rules overwrite earlier ones field by field.
    #[serde(default)]
    pub rules: Vec<ImportRule>,
}

fn default_version() -> u32 {
    IMPORT_RULES_FORMAT_VERSION
}

/// Errors loading `config/import_rules.ron`; the stable code is always `invalid_config`.
#[derive(Debug, Error)]
pub enum ImportRulesFileError {
    /// The RON did not parse (the message has line and column) or has an unknown field.
    #[error("config/import_rules.ron is invalid: {0}")]
    Parse(String),
    /// Written by a newer format.
    #[error("config/import_rules.ron format_version {found} is newer than the supported {supported}")]
    Version {
        /// Version in the file.
        found: u32,
        /// Newest version read.
        supported: u32,
    },
    /// A rule is malformed (empty glob or importer, settings that are not a RON object).
    #[error("config/import_rules.ron rule {index}: {reason}")]
    BadRule {
        /// The rule's index in `rules` (0-based).
        index: usize,
        /// What is wrong.
        reason: String,
    },
}

impl ImportRulesFileError {
    /// Stable lowercase code for the agent protocol and the CLI.
    #[must_use]
    pub fn code(&self) -> &'static str {
        "invalid_config"
    }
}

impl ImportRulesFile {
    /// Parse and check `config/import_rules.ron` text.
    ///
    /// # Errors
    ///
    /// [`ImportRulesFileError`] for bad RON (unknown fields included; the parse message carries
    /// the line and column), a newer `format_version`, or a malformed rule.
    pub fn from_ron(text: &str) -> Result<Self, ImportRulesFileError> {
        let file: Self = ron::from_str(text).map_err(|e| ImportRulesFileError::Parse(e.to_string()))?;
        if file.format_version > IMPORT_RULES_FORMAT_VERSION {
            return Err(ImportRulesFileError::Version {
                found: file.format_version,
                supported: IMPORT_RULES_FORMAT_VERSION,
            });
        }
        for (index, rule) in file.rules.iter().enumerate() {
            let bad = |reason: &str| {
                Err(ImportRulesFileError::BadRule {
                    index,
                    reason: reason.to_owned(),
                })
            };
            if rule.glob.is_empty() {
                return bad("the glob is empty");
            }
            if rule.glob.len() > MAX_GLOB_LEN {
                return bad("the glob is too long");
            }
            if rule.importer.as_deref().is_some_and(str::is_empty) {
                return bad("the importer filter is empty");
            }
            if let Err(e) = SettingsObject::parse(&rule.settings) {
                return bad(&format!("settings are not a RON object: {e}"));
            }
        }
        Ok(file)
    }

    /// Serialize as pretty RON.
    ///
    /// # Errors
    ///
    /// Serialization failed (a serializer bug; the file's shape is fixed).
    pub fn to_ron(&self) -> Result<String, ImportRulesFileError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| ImportRulesFileError::Parse(e.to_string()))
    }

    /// The rules of `<project_root>/config/import_rules.ron`: `None` when the file is absent.
    ///
    /// # Errors
    ///
    /// [`ImportRulesFileError`] when the file exists but does not parse or validate.
    pub fn for_project(project_root: &Path) -> Result<Option<Self>, ImportRulesFileError> {
        let path = project_root.join(IMPORT_RULES_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_ron(&text).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ImportRulesFileError::Parse(format!(
                "cannot read {IMPORT_RULES_FILE}: {e}"
            ))),
        }
    }

    /// The rules matching `rel_path` (project-relative, `/`-separated) for `importer`, as
    /// `(index, rule)` pairs in file order. Globs match ignoring ASCII case, so a project cooks
    /// the same on a case-insensitive volume (Windows, macOS) and on Linux.
    #[must_use]
    pub fn matched_rules(&self, rel_path: &str, importer: &str) -> Vec<(usize, &ImportRule)> {
        if rel_path.len() > MAX_PATH_LEN {
            return Vec::new();
        }
        // ASCII case-fold into stack buffers. `to_ascii_lowercase` only changes A–Z, so the
        // byte length and the UTF-8 stay valid and the match is the same string as before.
        let mut path_buf = [0u8; MAX_PATH_LEN];
        let path_len = rel_path.len();
        path_buf[..path_len].copy_from_slice(rel_path.as_bytes());
        path_buf[..path_len].make_ascii_lowercase();
        let Ok(path) = std::str::from_utf8(&path_buf[..path_len]) else {
            return Vec::new();
        };
        let mut glob_buf = [0u8; MAX_GLOB_LEN];
        self.rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| {
                let importer_ok = rule.importer.as_deref().is_none_or(|wanted| wanted == importer);
                let glob = rule.glob.as_bytes();
                if !importer_ok || glob.len() > MAX_GLOB_LEN {
                    false
                } else {
                    glob_buf[..glob.len()].copy_from_slice(glob);
                    glob_buf[..glob.len()].make_ascii_lowercase();
                    std::str::from_utf8(&glob_buf[..glob.len()])
                        .is_ok_and(|lowered| glob_match(lowered, path))
                }
            })
            .collect()
    }
}

/// `config/import_rules.ron` is at its first version: no migration steps.
#[must_use]
pub fn import_rules_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape of `config/import_rules.ron` for `cargo xtask check-formats`
/// (`docs/formats/import-rules-ron.md`, ADR-3321).
#[must_use]
pub fn import_rules_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<ImportRulesFile>(),
        regia_core::schema::serde_field_names::<ImportRule>(),
    ])
}

/// Merge `matched` rules under the explicit `.meta` fields: field by field, later rules win over
/// earlier ones and an explicit field wins over every rule. `explicit` is an importer settings
/// object exactly as stored in a `.meta` `settings` string.
///
/// Returns `None` when nothing sets any field (the importer's defaults apply unchanged). A
/// malformed `explicit` string is logged and ignored (rules still apply); a malformed rule
/// settings string cannot occur — [`ImportRulesFile::from_ron`] validated it.
#[must_use]
pub fn resolve_settings(explicit: Option<&str>, matched: &[(usize, &ImportRule)]) -> Option<String> {
    let mut merged = SettingsObject::default();
    for (_, rule) in matched {
        if let Ok(settings) = SettingsObject::parse(&rule.settings) {
            merged.merge(&settings);
        }
    }
    if let Some(text) = explicit.filter(|s| !s.trim().is_empty()) {
        match SettingsObject::parse(text) {
            Ok(explicit_obj) => merged.merge(&explicit_obj),
            Err(e) => tracing::warn!(
                target: "regia_asset",
                ".meta settings {text:?} ignored: {e}; import rules still apply"
            ),
        }
    }
    (!merged.fields.is_empty()).then(|| merged.to_ron())
}

/// Glob over a `/`-separated path: `**` matches any run (separators included), `**/` zero or more
/// whole folders (so `**/props/` never matches inside `myprops/`), `*` any run within a segment,
/// `?` one non-separator character; everything else matches itself. Linear
/// in pattern × path (no backtracking blow-up); an over-long pattern or path never matches.
#[must_use]
pub fn glob_match(pattern: &str, path: &str) -> bool {
    if pattern.len() > MAX_GLOB_LEN || path.len() > MAX_PATH_LEN {
        return false;
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Token {
        Any,
        /// `**/`: nothing, or a run ending in `/` (whole folders).
        AnyDirs,
        Star,
        One,
        Char(char),
    }
    // The pattern and the path are capped, so the dynamic-programming tables fit on the stack.
    // A cook calls this once per rule per asset; the tables used to be a fresh heap allocation
    // every time.
    let mut chars = ['\0'; MAX_GLOB_LEN];
    let mut n_chars = 0usize;
    for c in pattern.chars() {
        chars[n_chars] = c;
        n_chars += 1;
    }
    let mut tokens = [Token::Any; MAX_GLOB_LEN];
    let mut n_tokens = 0usize;
    let mut i = 0usize;
    while i < n_chars {
        let token = match chars[i] {
            '*' if i + 1 < n_chars && chars[i + 1] == '*' => {
                i += 2;
                // `**/` matches whole folders, or no folder at all.
                if i < n_chars && chars[i] == '/' {
                    i += 1;
                    Token::AnyDirs
                } else {
                    Token::Any
                }
            }
            '*' => {
                i += 1;
                Token::Star
            }
            '?' => {
                i += 1;
                Token::One
            }
            c => {
                i += 1;
                Token::Char(c)
            }
        };
        tokens[n_tokens] = token;
        n_tokens += 1;
    }
    let mut text = ['\0'; MAX_PATH_LEN];
    let mut n_text = 0usize;
    for c in path.chars() {
        text[n_text] = c;
        n_text += 1;
    }
    // reach[j]: the tokens so far can match text[..j].
    let mut reach = [false; MAX_PATH_LEN + 1];
    let mut next = [false; MAX_PATH_LEN + 1];
    reach[0] = true;
    for token in tokens.iter().take(n_tokens).copied() {
        next[..=n_text].fill(false);
        match token {
            Token::Any | Token::Star => {
                let mut open = false;
                for (j, (reached, slot)) in reach[..=n_text]
                    .iter()
                    .zip(next[..=n_text].iter_mut())
                    .enumerate()
                {
                    if *reached {
                        open = true;
                    }
                    if open {
                        *slot = true;
                    }
                    if token == Token::Star && j < n_text && text[j] == '/' {
                        open = false;
                    }
                }
            }
            Token::AnyDirs => {
                let mut open = false;
                for j in 0..=n_text {
                    if reach[j] {
                        open = true;
                        next[j] = true;
                    } else if open && j > 0 && text[j - 1] == '/' {
                        next[j] = true;
                    }
                }
            }
            Token::One => {
                for (j, &ch) in text[..n_text].iter().enumerate() {
                    next[j + 1] = reach[j] && ch != '/';
                }
            }
            Token::Char(c) => {
                for (j, &ch) in text[..n_text].iter().enumerate() {
                    next[j + 1] = reach[j] && ch == c;
                }
            }
        }
        reach[..=n_text].copy_from_slice(&next[..=n_text]);
    }
    reach[n_text]
}

/// A parse error of a settings object: `message` at the 1-based `line` and `column` of the
/// (trimmed, comment-stripped) text.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message} (line {line}, column {column})")]
pub struct SettingsObjectError {
    /// What is wrong.
    pub message: String,
    /// 1-based line of the problem.
    pub line: usize,
    /// 1-based column of the problem.
    pub column: usize,
}

/// A partial importer settings object: ordered top-level field name → raw RON value text.
///
/// The values are kept as their exact source text, so a merged settings string carries every
/// value through byte-for-byte (enums, nested structs, numbers, whatever the importer accepts).
/// Parsed with a small scanner instead of `ron::Value`, which loses bare enum identifiers
/// (`Some(normal)` becomes unit).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsObject {
    /// The fields in first-write order (the object's own order; overwritten fields keep it).
    pub fields: Vec<(String, String)>,
}

impl SettingsObject {
    /// Parse a RON object text (the shape of an importer's `.meta` `settings`).
    ///
    /// # Errors
    ///
    /// [`SettingsObjectError`] with the line and column: not an object, an unbalanced bracket,
    /// an unterminated string, a field without a value, or a duplicate field.
    pub fn parse(text: &str) -> Result<Self, SettingsObjectError> {
        let mut obj = Self::default();
        let blanked = blank_comments(text.trim())?;
        let trimmed = blanked.trim();
        if trimmed.is_empty() || trimmed == "()" {
            return Ok(obj);
        }
        let inner = match trimmed.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
            Some(inner) => {
                if !balanced(inner) {
                    return Err(position_error(trimmed, trimmed.len() - 1, "unbalanced brackets"));
                }
                inner
            }
            None => return Err(position_error(trimmed, 0, "expected a RON object `(…)`")),
        };
        for field in split_fields(inner) {
            let field_base = offset_of(trimmed, field);
            let (name, value) = field
                .split_once(':')
                .ok_or_else(|| position_error(trimmed, field_base, "a field needs `name: value`"))?;
            let name = unquote_name(name.trim()).ok_or_else(|| {
                position_error(
                    trimmed,
                    field_base,
                    "a field name must be an identifier or a quoted string",
                )
            })?;
            let value = value.trim();
            if value.is_empty() {
                return Err(position_error(
                    trimmed,
                    field_base + field.len(),
                    "a field needs a value",
                ));
            }
            if obj.fields.iter().any(|(existing, _)| *existing == name) {
                return Err(position_error(
                    trimmed,
                    field_base,
                    &format!("duplicate field `{name}`"),
                ));
            }
            obj.fields.push((name, value.to_owned()));
        }
        Ok(obj)
    }

    /// Overlay `other`: each of its fields replaces this one's field of the same name, or is
    /// appended. `self` loses.
    pub fn merge(&mut self, other: &Self) {
        for (name, value) in &other.fields {
            match self.fields.iter_mut().find(|(existing, _)| existing == name) {
                Some(slot) => slot.1 = value.clone(),
                None => self.fields.push((name.clone(), value.clone())),
            }
        }
    }

    /// The fields as one RON object text.
    #[must_use]
    pub fn to_ron(&self) -> String {
        if self.fields.is_empty() {
            return "()".to_string();
        }
        let body = self
            .fields
            .iter()
            .map(|(name, value)| format!("{name}: {value}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("({body})")
    }

    /// The raw value text of one field.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Blank every `//` and `/* */` comment to spaces (one per comment character), keeping newlines,
/// so the blanked text reads the same as the original without the comments. Strings and char
/// literals are left alone (a `//` inside one is not a comment). An unterminated comment or string
/// is an error at its start.
fn blank_comments(text: &str) -> Result<String, SettingsObjectError> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let (byte, c) = chars[i];
        match c {
            '"' | '\'' => {
                let mut j = i + 1;
                let mut closed = false;
                let mut escaped = false;
                while j < chars.len() {
                    let (_, d) = chars[j];
                    if escaped {
                        escaped = false;
                    } else if d == '\\' {
                        escaped = true;
                    } else if d == c {
                        closed = true;
                        break;
                    }
                    j += 1;
                }
                if !closed {
                    return Err(position_error(text, byte + 1, "unterminated string literal"));
                }
                i = j + 1;
            }
            '/' if chars.get(i + 1).is_some_and(|&(_, d)| d == '/') => {
                let mut j = i;
                while j < chars.len() && chars[j].1 != '\n' {
                    out[j] = ' ';
                    j += 1;
                }
                i = j;
            }
            '/' if chars.get(i + 1).is_some_and(|&(_, d)| d == '*') => {
                let mut j = i;
                let mut closed = false;
                while j + 1 < chars.len() {
                    if chars[j].1 == '*' && chars[j + 1].1 == '/' {
                        closed = true;
                        break;
                    }
                    j += 1;
                }
                if !closed {
                    return Err(position_error(text, byte + 1, "unterminated block comment"));
                }
                // `j` is the `*` of the closer. Blank through the `/` as well (`j + 1`).
                for slot in out.iter_mut().take(j + 2).skip(i) {
                    if *slot != '\n' {
                        *slot = ' ';
                    }
                }
                i = j + 2;
            }
            _ => i += 1,
        }
    }
    Ok(out.into_iter().collect())
}

/// Skip a string or char literal body after its opening quote; `false` when unterminated.
fn skip_quoted(chars: &mut impl Iterator<Item = char>, quote: char) -> bool {
    let mut escaped = false;
    for c in chars.by_ref() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == quote {
            return true;
        }
    }
    false
}

/// Split an object's inner text into raw top-level fields, tracking bracket depth and skipping
/// string/char literals (comments were blanked to spaces by [`blank_comments`] already).
fn split_fields(inner: &str) -> Vec<&str> {
    let bytes = inner.as_bytes();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut fields = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                fields.push(&inner[start..i]);
                start = i + 1;
            }
            b'"' | b'\'' => {
                let quote = bytes[i] as char;
                let mut rest = inner[i + 1..].chars();
                // Unterminated literals were rejected by blank_comments.
                let _ = skip_quoted(&mut rest, quote);
                // `rest` begins right after the closing quote: step back onto the quote so the
                // loop's `i += 1` lands just past it.
                i = inner.len() - rest.as_str().len() - 1;
            }
            _ => {}
        }
        i += 1;
    }
    if !inner[start..].trim().is_empty() {
        fields.push(&inner[start..]);
    }
    fields
}

/// `text` is bracket-balanced when every opener has its closer. Comments are already blanked.
/// Brackets inside a string or char literal are not structure (an unterminated literal, which
/// [`blank_comments`] already rejects, counts as unbalanced).
fn balanced(text: &str) -> bool {
    let mut chars = text.chars();
    let mut stack: Vec<char> = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                if !skip_quoted(&mut chars, c) {
                    return false;
                }
            }
            '(' | '[' | '{' => stack.push(c),
            ')' | ']' | '}' => {
                let ok = matches!(
                    (stack.pop(), c),
                    (Some('('), ')') | (Some('['), ']') | (Some('{'), '}')
                );
                if !ok {
                    return false;
                }
            }
            _ => {}
        }
    }
    stack.is_empty()
}

/// The byte offset of `slice` inside `full` (both are slices of the same text).
fn offset_of(full: &str, slice: &str) -> usize {
    slice.as_ptr() as usize - full.as_ptr() as usize
}

/// A [`SettingsObjectError`] at byte `offset` of `text` (1-based line and column).
fn position_error(text: &str, offset: usize, message: &str) -> SettingsObjectError {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
    let column = offset - before.rfind('\n').map_or(0, |i| i + 1) + 1;
    SettingsObjectError {
        message: message.to_owned(),
        line,
        column,
    }
}

/// A field name: an identifier, or a quoted string (unescaped only for the duplicate check).
fn unquote_name(name: &str) -> Option<String> {
    if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.is_empty() {
        return Some(name.to_owned());
    }
    name.strip_prefix('"')
        .and_then(|n| n.strip_suffix('"'))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = r#"(
        format_version: 1,
        rules: [
            (glob: "assets/ui/**", importer: "png", settings: "(compress: false)"),
            (glob: "assets/**", settings: "(quality: \"low\")"),
            (glob: "assets/props/**", importer: "gltf", settings: "(meshlets: true, collision: ConvexDecomposition)"),
        ],
    )"#;

    fn rules() -> ImportRulesFile {
        ImportRulesFile::from_ron(RULES).expect("fixture parses")
    }

    #[test]
    fn the_file_parses_and_validates() {
        let file = rules();
        assert_eq!(file.rules.len(), 3);
        assert_eq!(file.rules[0].importer.as_deref(), Some("png"));
        assert_eq!(file.rules[1].importer, None);
        // Round trip.
        let text = file.to_ron().expect("serialize");
        assert_eq!(ImportRulesFile::from_ron(&text).expect("re-parse"), file);
        assert!(import_rules_schema_fingerprint().contains("glob"));
        assert!(import_rules_migration_sources().is_empty());
    }

    #[test]
    fn parse_errors_carry_line_and_column() {
        let err = ImportRulesFile::from_ron("(format_version: 1, rules: [(glob: 5, settings: \"()\")])")
            .expect_err("glob is not a string");
        let text = err.to_string();
        // ron's spanned error prints `line:column-range`: "1:35-1:36: Expected string".
        assert!(text.contains("1:35"), "{text}");
        let ok = ImportRulesFile::from_ron("(format_version: 1,\n rules: [(glob: \"a\", settings: \"()\")])")
            .expect("multiline parses");
        assert_eq!(ok.format_version, 1);
        let err =
            ImportRulesFile::from_ron("(format_version: 1, unknown_key: true)").expect_err("unknown field");
        assert!(err.to_string().contains("unknown_key"), "{err}");
    }

    #[test]
    fn newer_versions_and_bad_rules_are_refused() {
        let err = ImportRulesFile::from_ron("(format_version: 2)").expect_err("newer");
        assert!(matches!(err, ImportRulesFileError::Version { found: 2, .. }));
        assert_eq!(err.code(), "invalid_config");
        let err =
            ImportRulesFile::from_ron("(rules: [(glob: \"\", settings: \"()\")])").expect_err("empty glob");
        assert!(matches!(err, ImportRulesFileError::BadRule { index: 0, .. }));
        let err = ImportRulesFile::from_ron("(rules: [(glob: \"a\", settings: \"42\")])")
            .expect_err("not an object");
        assert!(err.to_string().contains("settings"), "{err}");
        let err = ImportRulesFile::from_ron("(rules: [(glob: \"a\", settings: \"(x: )\")])")
            .expect_err("value missing");
        assert!(err.to_string().contains("settings"), "{err}");
    }

    #[test]
    fn globs_match_segments_and_case() {
        assert!(glob_match("assets/ui/**", "assets/ui/icons/a.png"));
        assert!(glob_match("assets/ui/**", "assets/ui/a.png"));
        assert!(!glob_match("assets/ui/**", "assets/uix/a.png"));
        assert!(glob_match("assets/*", "assets/a.png"));
        assert!(!glob_match("assets/*", "assets/ui/a.png"));
        assert!(glob_match("**/props/*_n.png", "assets/props/rock_n.png"));
        assert!(glob_match("assets/?.png", "assets/a.png"));
        assert!(!glob_match("assets/?.png", "assets/ab.png"));
        // The matcher itself is case-sensitive; `matched_rules` folds case.
        assert!(!glob_match("ASSETS/**", "assets/a.png"));
        assert!(!glob_match(&"a".repeat(600), "a.png"));
        assert!(glob_match("a.png", "a.png"));
    }

    /// corr-asset: `**/` stands for whole folders, never part of a folder name.
    #[test]
    fn corr_asset_double_star_slash_matches_whole_folders_only() {
        assert!(!glob_match("**/props/*_n.png", "assets/myprops/rock_n.png"));
        assert!(!glob_match("assets/**/a.png", "assets/xa.png"));
        assert!(!glob_match("**/ui/**", "assets/gui/a.png"));
        assert!(glob_match("**/props/*_n.png", "props/rock_n.png"));
        assert!(glob_match("**/props/*_n.png", "assets/props/rock_n.png"));
        assert!(glob_match("assets/**/a.png", "assets/a.png"));
        assert!(glob_match("assets/**/a.png", "assets/b/c/a.png"));
        assert!(glob_match("**/ui/**", "assets/ui/icons/a.png"));
        assert!(glob_match("**", "anything/at/all"));
    }

    #[test]
    fn rules_match_by_glob_and_importer() {
        let file = rules();
        let matched = file.matched_rules("assets/ui/icon.png", "png");
        assert_eq!(matched.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1]);
        // The importer filter excludes rule 0 from a gltf source.
        let matched = file.matched_rules("assets/ui/panel.gltf", "gltf");
        assert_eq!(matched.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![1]);
        // A filterless rule reaches every importer; globs fold ASCII case.
        let matched = file.matched_rules("ASSETS/SFX/HIT.WAV", "audio");
        assert_eq!(matched.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![1]);
        assert!(!file.matched_rules("assets/sfx/hit.wav", "gltf").is_empty());
    }

    #[test]
    fn explicit_meta_fields_beat_rules_and_later_rules_win() {
        let file = rules();
        let matched = file.matched_rules("assets/ui/icon.png", "png");
        let merged =
            resolve_settings(Some("(compress: true, max_size: Some(1024))"), &matched).expect("settings");
        // compress: true is explicit and beats rule 0's false; quality comes from rule 1.
        assert_eq!(merged, "(compress: true, quality: \"low\", max_size: Some(1024))");
        let matched = file.matched_rules("assets/props/crate.gltf", "gltf");
        let merged = resolve_settings(None, &matched).expect("settings");
        assert_eq!(
            merged,
            "(quality: \"low\", meshlets: true, collision: ConvexDecomposition)"
        );
    }

    #[test]
    fn nothing_set_resolves_to_none() {
        let file = ImportRulesFile::default();
        assert_eq!(resolve_settings(None, &file.matched_rules("a.png", "png")), None);
        assert_eq!(resolve_settings(Some("()"), &[]), None);
        assert_eq!(resolve_settings(Some("  "), &[]), None);
    }

    #[test]
    fn a_malformed_explicit_string_is_ignored_with_rules_kept() {
        let file = rules();
        let matched = file.matched_rules("assets/ui/icon.png", "png");
        let merged = resolve_settings(Some("(compress: "), &matched).expect("settings");
        assert_eq!(merged, "(compress: false, quality: \"low\")");
    }

    #[test]
    fn the_scanner_keeps_values_byte_for_byte() {
        let obj = SettingsObject::parse(
            "(usage: Some(normal), ratios: [1.0, 0.5], note: \"a, (b)\", cube_face_px: Some(1024))",
        )
        .expect("parse");
        assert_eq!(obj.get("usage"), Some("Some(normal)"));
        assert_eq!(obj.get("ratios"), Some("[1.0, 0.5]"));
        assert_eq!(obj.get("note"), Some("\"a, (b)\""));
        let mut base = SettingsObject::parse("(usage: Some(color), max_size: Some(2048))").expect("parse");
        base.merge(&obj);
        assert_eq!(
            base.to_ron(),
            "(usage: Some(normal), max_size: Some(2048), ratios: [1.0, 0.5], note: \"a, (b)\", cube_face_px: Some(1024))"
        );
    }

    #[test]
    fn the_scanner_reports_positions_and_duplicates() {
        let err = SettingsObject::parse("(a: 1,\n b: [2, 3)").expect_err("unbalanced");
        assert_eq!((err.line, err.column), (2, 10), "{err:?}");
        let err = SettingsObject::parse("(a: \"unterminated)").expect_err("string");
        assert!(err.line >= 1);
        let err = SettingsObject::parse("(a: 1, a: 2)").expect_err("duplicate");
        assert!(err.message.contains("duplicate"), "{err}");
        let err = SettingsObject::parse("(a)").expect_err("no colon");
        assert!(err.message.contains("name: value"), "{err}");
        let err = SettingsObject::parse("42").expect_err("not an object");
        assert!(err.message.contains("RON object"), "{err}");
        // Comments and chars are skipped by the splitter.
        let obj = SettingsObject::parse("(a: ',', // comment, with comma\n b: 2)").expect("parse");
        assert_eq!(obj.get("a"), Some("','"));
        assert_eq!(obj.get("b"), Some("2"));
    }

    #[test]
    fn audfix_block_comments_and_brackets_inside_strings() {
        // A block comment is blanked, including its closer, so a comma inside it is not a field.
        let obj = SettingsObject::parse("(a: 1, /* c, */ b: 2)").expect("block comment");
        assert_eq!(obj.fields.len(), 2, "{obj:?}");
        assert_eq!(obj.get("a"), Some("1"));
        assert_eq!(obj.get("b"), Some("2"));
        // A bracket inside a string or char is not structure. A real extra closer still fails.
        let obj = SettingsObject::parse("(name: \"a)b\", n: 1)").expect("paren in string");
        assert_eq!(obj.get("name"), Some("\"a)b\""));
        assert_eq!(obj.get("n"), Some("1"));
        let obj = SettingsObject::parse("(name: \"a)\", note: \"}\", ch: ')')").expect("closers in literals");
        assert_eq!(obj.get("name"), Some("\"a)\""));
        assert_eq!(obj.get("note"), Some("\"}\""));
        assert_eq!(obj.get("ch"), Some("')'"));
        let err = SettingsObject::parse("(a: 1))").expect_err("real extra closer");
        assert!(err.message.contains("unbalanced"), "{err}");
        let err = SettingsObject::parse("(a: (1)").expect_err("real missing closer");
        assert!(err.message.contains("unbalanced"), "{err}");
    }
}
