// SPDX-License-Identifier: MPL-2.0
//! Localization pipeline, step 1 (ADR-1570): gather.
//!
//! This module builds an in-memory key manifest. The project gather tool that `regia loc` runs
//! (it writes `gathered.ftl` files, ADR-1466) is [`crate::loc_gather`]; ADR-1589 records why both
//! exist.
//!
//! [`gather`] scans in-memory sources (Fluent `.ftl` files, UI assets, scenes) and builds a
//! [`KeyManifest`]: every localization key with the places that define it, per locale, and the
//! places that reference it. The scan is pure and deterministic: sources are processed in path
//! order, so the same inputs give the same manifest and the same JSON no matter how the caller
//! lists them. Reading files is the caller's job, so the step works the same in tools, tests and
//! the editor.
//!
//! What counts as a reference in a UI asset or scene (RON text): a string literal that starts with
//! `loc:` (`"loc:menu-start"`) or that is the argument of a `Loc(...)` constructor
//! (`Loc("menu-start")`). Comments (`//` and nested `/* */`) are skipped. What counts as a
//! definition in a `.ftl` file: a message at the start of a line (`id = ...`); terms (`-id`) and
//! attributes are not keys the runtime resolves, so they are not gathered. The next step is
//! [`crate::loc_compile`].

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::UiError;

/// The message ids a Fluent source defines, in file order (duplicates included).
pub(crate) fn ftl_keys(source: &str) -> Vec<String> {
    scan_ftl("", source).messages.into_iter().map(|m| m.key).collect()
}

/// A position in a source file. `line` and `column` count from 1; `column` counts characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct SourceLoc {
    /// The path the caller gave the source (used as is, never normalized).
    pub path: String,
    /// Line, from 1.
    pub line: u32,
    /// Column in characters, from 1.
    pub column: u32,
}

impl std::fmt::Display for SourceLoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.path, self.line, self.column)
    }
}

/// What a [`GatherSource`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// A Fluent file that defines messages for `locale`.
    Ftl {
        /// The locale tag (`en`, `pt-BR`).
        locale: String,
    },
    /// A `.ui.ron` asset (or any RON text that holds UI text spans).
    UiAsset,
    /// A scene (or any other RON text) whose string values may name keys with `loc:`.
    Scene,
}

/// One input of [`gather`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatherSource {
    /// The path reported in source locations.
    pub path: String,
    /// How to scan the text.
    pub kind: SourceKind,
    /// The file contents.
    pub text: String,
}

impl GatherSource {
    /// A Fluent file for `locale`.
    pub fn ftl(path: impl Into<String>, locale: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: SourceKind::Ftl {
                locale: locale.into(),
            },
            text: text.into(),
        }
    }

    /// A UI asset.
    pub fn ui_asset(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: SourceKind::UiAsset,
            text: text.into(),
        }
    }

    /// A scene.
    pub fn scene(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: SourceKind::Scene,
            text: text.into(),
        }
    }
}

/// Where a reference came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    /// A UI asset.
    Ui,
    /// A scene.
    Scene,
}

/// One place that uses a key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct KeyReference {
    /// Where the key is written.
    pub at: SourceLoc,
    /// What kind of source it is in.
    pub kind: ReferenceKind,
}

/// Everything known about one key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct KeyEntry {
    /// Definitions per locale, in path order. More than one entry for a locale is a duplicate.
    pub definitions: BTreeMap<String, Vec<SourceLoc>>,
    /// References from UI assets and scenes, in path and position order.
    pub references: Vec<KeyReference>,
}

/// A problem found while gathering. Codes are lowercase ascii, digits and `_`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GatherDiagnostic {
    /// Stable code: `ftl_syntax`, `duplicate_key`, `invalid_key`, `unterminated_string`,
    /// `unterminated_comment`, `empty_locale`.
    pub code: &'static str,
    /// Human-readable detail.
    pub message: String,
    /// Where it happened, when there is a place.
    pub at: Option<SourceLoc>,
}

/// The key manifest: every key, where it is defined and where it is used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct KeyManifest {
    /// Locales that have at least one `.ftl` source, sorted.
    pub locales: BTreeSet<String>,
    /// Keys sorted by name.
    pub keys: BTreeMap<String, KeyEntry>,
}

impl KeyManifest {
    /// Keys that something references but no locale defines, with their references.
    pub fn undefined_keys(&self) -> Vec<(&str, &[KeyReference])> {
        self.keys
            .iter()
            .filter(|(_, e)| e.definitions.is_empty() && !e.references.is_empty())
            .map(|(k, e)| (k.as_str(), e.references.as_slice()))
            .collect()
    }

    /// Keys that some locale defines but nothing references. Keys built at runtime look unused
    /// here, so treat this as a hint, not an error.
    pub fn unused_keys(&self) -> Vec<&str> {
        self.keys
            .iter()
            .filter(|(_, e)| !e.definitions.is_empty() && e.references.is_empty())
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// The manifest as pretty JSON (stable key order).
    pub fn to_json(&self) -> Result<String, UiError> {
        serde_json::to_string_pretty(self).map_err(|e| UiError::Localization(format!("manifest json: {e}")))
    }

    /// One line per key: `key  defined: en,fr  refs: 3`, sorted by key.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for (key, entry) in &self.keys {
            let locales: Vec<&str> = entry.definitions.keys().map(String::as_str).collect();
            out.push_str(&format!(
                "{key}  defined: {}  refs: {}\n",
                if locales.is_empty() {
                    "-".to_owned()
                } else {
                    locales.join(",")
                },
                entry.references.len()
            ));
        }
        out
    }
}

/// One Fluent message found by the scanner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtlMessage {
    /// The message id.
    pub key: String,
    /// Where the id starts.
    pub at: SourceLoc,
    /// The verbatim source of the message: the id line and its indented continuation lines
    /// (attributes and select variants included), without trailing blank lines or comments.
    pub block: String,
    /// Whether the message has a value (text after `=` or indented value lines), as opposed to
    /// attributes only.
    pub has_value: bool,
    /// Names of the `$variables` the block uses, sorted.
    pub variables: BTreeSet<String>,
}

/// The output of [`gather`]: the manifest, the parsed messages per locale and the problems.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gathered {
    /// The key manifest.
    pub manifest: KeyManifest,
    /// Messages per locale, in path then line order (the last definition of a key wins when
    /// compiling).
    pub messages: BTreeMap<String, Vec<FtlMessage>>,
    /// Problems, in the order found (path order).
    pub diagnostics: Vec<GatherDiagnostic>,
}

/// Whether `key` is a valid Fluent message id: an ascii letter, then letters, digits, `_` or `-`.
pub fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Scans every source and builds the manifest. Sources are processed in path order (ties in
/// input order), so the result does not depend on how the caller ordered them.
pub fn gather(sources: &[GatherSource]) -> Gathered {
    let _span = tracing::debug_span!("loc_gather", sources = sources.len()).entered();
    let mut order: Vec<&GatherSource> = sources.iter().collect();
    order.sort_by(|a, b| a.path.cmp(&b.path));
    let mut out = Gathered::default();
    for source in order {
        match &source.kind {
            SourceKind::Ftl { locale } => gather_ftl(source, locale, &mut out),
            SourceKind::UiAsset => gather_ron(source, ReferenceKind::Ui, &mut out),
            SourceKind::Scene => gather_ron(source, ReferenceKind::Scene, &mut out),
        }
    }
    for (key, entry) in &out.manifest.keys {
        for (locale, defs) in &entry.definitions {
            if let [first, .., last] = defs.as_slice() {
                out.diagnostics.push(GatherDiagnostic {
                    code: "duplicate_key",
                    message: format!(
                        "key {key:?} is defined {} times in {locale} (first at {first}); the last wins",
                        defs.len()
                    ),
                    at: Some(last.clone()),
                });
            }
        }
    }
    out
}

fn gather_ftl(source: &GatherSource, locale: &str, out: &mut Gathered) {
    if locale.trim().is_empty() {
        out.diagnostics.push(GatherDiagnostic {
            code: "empty_locale",
            message: format!("{} has an empty locale tag", source.path),
            at: None,
        });
        return;
    }
    out.manifest.locales.insert(locale.to_owned());
    let scan = scan_ftl(&source.path, &source.text);
    out.diagnostics.extend(scan.diagnostics);
    let list = out.messages.entry(locale.to_owned()).or_default();
    for message in scan.messages {
        out.manifest
            .keys
            .entry(message.key.clone())
            .or_default()
            .definitions
            .entry(locale.to_owned())
            .or_default()
            .push(message.at.clone());
        list.push(message);
    }
}

fn gather_ron(source: &GatherSource, kind: ReferenceKind, out: &mut Gathered) {
    let scan = scan_ron_strings(&source.path, &source.text);
    out.diagnostics.extend(scan.diagnostics);
    for literal in scan.literals {
        let key = match literal.value.strip_prefix("loc:") {
            Some(rest) => rest.to_owned(),
            None if literal.after_loc_call => literal.value.clone(),
            None => continue,
        };
        if !is_valid_key(&key) {
            out.diagnostics.push(GatherDiagnostic {
                code: "invalid_key",
                message: format!("{key:?} is not a valid localization key"),
                at: Some(literal.at),
            });
            continue;
        }
        out.manifest
            .keys
            .entry(key)
            .or_default()
            .references
            .push(KeyReference { at: literal.at, kind });
    }
}

struct FtlScan {
    messages: Vec<FtlMessage>,
    diagnostics: Vec<GatherDiagnostic>,
}

/// The `$names` used in `text`.
fn variables_in(text: &str) -> BTreeSet<String> {
    let mut vars = BTreeSet::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' {
            let mut j = i + 1;
            let mut name = String::new();
            while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_' || chars[j] == '-')
            {
                name.push(chars[j]);
                j += 1;
            }
            if name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                vars.insert(name);
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    vars
}

/// What a line that starts in column 1 is.
enum HeadLine<'a> {
    Message(&'a str, &'a str),
    Term,
    Other,
}

fn classify_head(line: &str) -> HeadLine<'_> {
    let (is_term, body) = match line.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    if !body.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return HeadLine::Other;
    }
    let id_end = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(body.len());
    let (id, rest) = body.split_at(id_end);
    match rest.trim_start_matches([' ', '\t']).strip_prefix('=') {
        Some(_) if is_term => HeadLine::Term,
        Some(value) => HeadLine::Message(id, value),
        None => HeadLine::Other,
    }
}

struct OpenMessage {
    key: String,
    at: SourceLoc,
    block: String,
    has_value: bool,
    in_attributes: bool,
    pending_blank: usize,
}

impl OpenMessage {
    fn finish(self) -> FtlMessage {
        let variables = variables_in(&self.block);
        FtlMessage {
            key: self.key,
            at: self.at,
            block: self.block,
            has_value: self.has_value,
            variables,
        }
    }
}

/// Scans Fluent text for messages. Terms, comments and attributes are skipped; a line in column 1
/// that is none of those is reported as `ftl_syntax`.
fn scan_ftl(path: &str, text: &str) -> FtlScan {
    let mut messages = Vec::new();
    let mut diagnostics = Vec::new();
    let mut open: Option<OpenMessage> = None;
    for (index, raw) in text.lines().enumerate() {
        let line_no = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let raw = if index == 0 {
            raw.trim_start_matches('\u{FEFF}')
        } else {
            raw
        };
        if raw.trim().is_empty() {
            if let Some(msg) = open.as_mut() {
                msg.pending_blank += 1;
            }
            continue;
        }
        let starts_indented = raw.starts_with([' ', '\t']);
        if starts_indented {
            if let Some(msg) = open.as_mut() {
                let trimmed = raw.trim_start();
                if trimmed.starts_with('.') {
                    msg.in_attributes = true;
                } else if !msg.in_attributes {
                    msg.has_value = true;
                }
                for _ in 0..msg.pending_blank {
                    msg.block.push('\n');
                }
                msg.pending_blank = 0;
                msg.block.push('\n');
                msg.block.push_str(raw);
            }
            continue;
        }
        // The closing brace of a select expression may sit in column 1 (the Fluent serializer
        // writes it there): it belongs to the open message.
        if raw.starts_with('}')
            && let Some(msg) = open.as_mut()
        {
            for _ in 0..msg.pending_blank {
                msg.block.push('\n');
            }
            msg.pending_blank = 0;
            msg.block.push('\n');
            msg.block.push_str(raw);
            continue;
        }
        // A column-1 line ends the open message.
        if let Some(msg) = open.take() {
            messages.push(msg.finish());
        }
        if raw.starts_with('#') {
            continue;
        }
        match classify_head(raw) {
            HeadLine::Message(id, value) => {
                open = Some(OpenMessage {
                    key: id.to_owned(),
                    at: SourceLoc {
                        path: path.to_owned(),
                        line: line_no,
                        column: 1,
                    },
                    block: raw.to_owned(),
                    has_value: !value.trim().is_empty(),
                    in_attributes: false,
                    pending_blank: 0,
                });
            }
            HeadLine::Term => {}
            HeadLine::Other => diagnostics.push(GatherDiagnostic {
                code: "ftl_syntax",
                message: format!("line {line_no} is not a message, term or comment"),
                at: Some(SourceLoc {
                    path: path.to_owned(),
                    line: line_no,
                    column: 1,
                }),
            }),
        }
    }
    if let Some(msg) = open.take() {
        messages.push(msg.finish());
    }
    FtlScan {
        messages,
        diagnostics,
    }
}

/// A string literal found in RON text.
struct RonLiteral {
    value: String,
    at: SourceLoc,
    after_loc_call: bool,
}

struct RonScan {
    literals: Vec<RonLiteral>,
    diagnostics: Vec<GatherDiagnostic>,
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether the literal that starts at `start` is the argument of `Loc(`.
fn follows_loc_call(chars: &[char], start: usize) -> bool {
    let mut j = start;
    while j > 0 && chars[j - 1].is_whitespace() {
        j -= 1;
    }
    if j == 0 || chars[j - 1] != '(' {
        return false;
    }
    j -= 1;
    while j > 0 && chars[j - 1].is_whitespace() {
        j -= 1;
    }
    j >= 3 && chars[j - 3..j] == ['L', 'o', 'c'] && (j == 3 || !is_ident_char(chars[j - 4]))
}

/// Finds every string literal outside comments, with its position. Handles `\"` style escapes
/// and raw strings (`r"..."`, `r#"..."#`).
fn scan_ron_strings(path: &str, text: &str) -> RonScan {
    let chars: Vec<char> = text.chars().collect();
    let mut literals = Vec::new();
    let mut diagnostics = Vec::new();
    let mut i = 0;
    let mut line = 1u32;
    let mut col = 1u32;
    let loc = |line: u32, col: u32| SourceLoc {
        path: path.to_owned(),
        line,
        column: col,
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '\n' {
            line += 1;
            col = 1;
            i += 1;
        } else if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
                col += 1;
            }
        } else if c == '/' && next == Some('*') {
            let (start_line, start_col) = (line, col);
            let mut depth = 0usize;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                    col += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    col += 2;
                    if depth == 0 {
                        break;
                    }
                } else if chars[i] == '\n' {
                    line += 1;
                    col = 1;
                    i += 1;
                } else {
                    i += 1;
                    col += 1;
                }
            }
            if depth > 0 {
                diagnostics.push(GatherDiagnostic {
                    code: "unterminated_comment",
                    message: "block comment is never closed".to_owned(),
                    at: Some(loc(start_line, start_col)),
                });
            }
        } else if c == '"'
            || (c == 'r' && matches!(next, Some('"' | '#')) && (i == 0 || !is_ident_char(chars[i - 1])))
        {
            let (start_line, start_col, start_idx) = (line, col, i);
            let mut value = String::new();
            let mut closed = false;
            if c == 'r' {
                // Raw string: count the hashes, then read to `"` plus the same hashes.
                let mut j = i + 1;
                let mut hashes = 0usize;
                while chars.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if chars.get(j) != Some(&'"') {
                    // `r#` that is not a raw string (an identifier): skip the `r`.
                    i += 1;
                    col += 1;
                    continue;
                }
                j += 1;
                col += u32::try_from(j - i).unwrap_or(0);
                i = j;
                while i < chars.len() {
                    if chars[i] == '"'
                        && chars[i + 1..].iter().take(hashes).filter(|&&h| h == '#').count() == hashes
                    {
                        i += 1 + hashes;
                        col += 1 + u32::try_from(hashes).unwrap_or(0);
                        closed = true;
                        break;
                    }
                    if chars[i] == '\n' {
                        line += 1;
                        col = 1;
                    } else {
                        col += 1;
                    }
                    value.push(chars[i]);
                    i += 1;
                }
            } else {
                i += 1;
                col += 1;
                while i < chars.len() {
                    let ch = chars[i];
                    if ch == '"' {
                        i += 1;
                        col += 1;
                        closed = true;
                        break;
                    }
                    if ch == '\\' {
                        let esc = chars.get(i + 1).copied();
                        i += 2;
                        col += 2;
                        match esc {
                            Some('n') => value.push('\n'),
                            Some('t') => value.push('\t'),
                            Some('r') => value.push('\r'),
                            Some(other) => value.push(other),
                            None => {}
                        }
                        continue;
                    }
                    if ch == '\n' {
                        line += 1;
                        col = 1;
                    } else {
                        col += 1;
                    }
                    value.push(ch);
                    i += 1;
                }
            }
            if closed {
                literals.push(RonLiteral {
                    value,
                    at: loc(start_line, start_col),
                    after_loc_call: follows_loc_call(&chars, start_idx),
                });
            } else {
                diagnostics.push(GatherDiagnostic {
                    code: "unterminated_string",
                    message: "string literal is never closed".to_owned(),
                    at: Some(loc(start_line, start_col)),
                });
            }
        } else {
            i += 1;
            col += 1;
        }
    }
    RonScan {
        literals,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EN: &str = "\
# A comment
## Group comment
menu-start = Start Game
menu-quit =
    Quit to desktop
    and save
-brand = Regia
items = { $count ->
    [one] { $count } item
   *[other] { $count } items
    }
tooltip = Hover
    .title = Title for { $name }

greeting = Hello, { $name }!
";

    #[test]
    fn valid_keys() {
        assert!(is_valid_key("menu-start"));
        assert!(is_valid_key("a_b-9"));
        assert!(!is_valid_key(""));
        assert!(!is_valid_key("-term"));
        assert!(!is_valid_key("9lives"));
        assert!(!is_valid_key("has space"));
        assert!(!is_valid_key("naïve"));
    }

    #[test]
    fn ftl_messages_get_lines_blocks_and_variables() {
        let g = gather(&[GatherSource::ftl("en/main.ftl", "en", EN)]);
        assert!(g.diagnostics.is_empty(), "{:?}", g.diagnostics);
        let keys: Vec<&str> = g.manifest.keys.keys().map(String::as_str).collect();
        assert_eq!(keys, ["greeting", "items", "menu-quit", "menu-start", "tooltip"]);
        let en = &g.messages["en"];
        let start = en.iter().find(|m| m.key == "menu-start").expect("menu-start");
        assert_eq!(start.at.line, 3);
        assert_eq!(start.block, "menu-start = Start Game");
        assert!(start.has_value);
        let quit = en.iter().find(|m| m.key == "menu-quit").expect("menu-quit");
        assert_eq!(quit.block, "menu-quit =\n    Quit to desktop\n    and save");
        assert!(quit.has_value, "value on indented lines");
        let items = en.iter().find(|m| m.key == "items").expect("items");
        assert_eq!(items.variables.iter().collect::<Vec<_>>(), ["count"]);
        assert!(items.block.ends_with("}"));
        let tip = en.iter().find(|m| m.key == "tooltip").expect("tooltip");
        assert!(tip.block.contains(".title"));
        assert_eq!(tip.variables.iter().collect::<Vec<_>>(), ["name"]);
    }

    #[test]
    fn attribute_only_message_has_no_value() {
        let g = gather(&[GatherSource::ftl("a.ftl", "en", "btn =\n    .label = Go\n")]);
        let m = &g.messages["en"][0];
        assert!(!m.has_value);
    }

    #[test]
    fn terms_and_comments_are_not_keys() {
        let g = gather(&[GatherSource::ftl(
            "a.ftl",
            "en",
            "-brand = X\n    more\n# c\nreal = y\n",
        )]);
        let keys: Vec<&str> = g.manifest.keys.keys().map(String::as_str).collect();
        assert_eq!(keys, ["real"]);
        assert!(g.diagnostics.is_empty());
    }

    #[test]
    fn junk_line_is_reported_with_its_position() {
        let g = gather(&[GatherSource::ftl("a.ftl", "en", "ok = 1\n???\nalso = 2\n")]);
        assert_eq!(g.diagnostics.len(), 1);
        assert_eq!(g.diagnostics[0].code, "ftl_syntax");
        assert_eq!(g.diagnostics[0].at.as_ref().map(|a| a.line), Some(2));
        assert_eq!(g.manifest.keys.len(), 2, "scanning continues");
    }

    #[test]
    fn crlf_and_bom_are_tolerated() {
        let g = gather(&[GatherSource::ftl("a.ftl", "en", "\u{FEFF}a = 1\r\nb = 2\r\n")]);
        assert!(g.diagnostics.is_empty());
        assert_eq!(g.messages["en"][0].block, "a = 1");
        assert_eq!(g.messages["en"].len(), 2);
    }

    #[test]
    fn duplicate_definitions_are_reported_once_and_last_wins_later() {
        let g = gather(&[
            GatherSource::ftl("en/a.ftl", "en", "x = one\n"),
            GatherSource::ftl("en/b.ftl", "en", "x = two\ny = 1\n"),
        ]);
        let dups: Vec<_> = g
            .diagnostics
            .iter()
            .filter(|d| d.code == "duplicate_key")
            .collect();
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].at.as_ref().map(|a| a.path.as_str()), Some("en/b.ftl"));
        assert_eq!(g.manifest.keys["x"].definitions["en"].len(), 2);
    }

    #[test]
    fn ui_references_loc_calls_and_prefixed_strings() {
        let ui = r#"(
    root: (
        text: (spans: [Loc("menu-start"), Literal("plain"), Text("loc:menu-quit")]),
        // Loc("commented-out")
        /* "loc:also-commented" /* nested */ Loc("still-commented") */
        other: Loc ( "spaced" ),
        notloc: Bloc("notakey"),
        title: "not-a-ref",
    ),
)"#;
        let g = gather(&[GatherSource::ui_asset("ui/main.ui.ron", ui)]);
        assert!(g.diagnostics.is_empty(), "{:?}", g.diagnostics);
        let keys: Vec<&str> = g.manifest.keys.keys().map(String::as_str).collect();
        assert_eq!(keys, ["menu-quit", "menu-start", "spaced"]);
        let start = &g.manifest.keys["menu-start"].references[0];
        assert_eq!((start.at.line, start.at.column), (3, 28));
        assert_eq!(start.kind, ReferenceKind::Ui);
        assert_eq!(g.manifest.keys["menu-quit"].references[0].at.line, 3);
    }

    #[test]
    fn scene_references_are_marked_as_scene() {
        let g = gather(&[GatherSource::scene(
            "levels/a.scene.ron",
            "(name: \"loc:level-a-name\", note: \"hello\")",
        )]);
        let r = &g.manifest.keys["level-a-name"].references[0];
        assert_eq!(r.kind, ReferenceKind::Scene);
        assert_eq!(r.at.column, 8);
    }

    #[test]
    fn invalid_reference_keys_are_diagnostics_not_keys() {
        let g = gather(&[GatherSource::ui_asset(
            "a.ron",
            "(a: Loc(\"bad key\"), b: \"loc:\", c: \"loc:9x\")",
        )]);
        assert!(g.manifest.keys.is_empty());
        assert_eq!(
            g.diagnostics.iter().filter(|d| d.code == "invalid_key").count(),
            3
        );
    }

    #[test]
    fn unterminated_string_and_comment_are_reported() {
        let g = gather(&[GatherSource::ui_asset("a.ron", "(a: Loc(\"open)")]);
        assert_eq!(g.diagnostics[0].code, "unterminated_string");
        let g = gather(&[GatherSource::ui_asset("b.ron", "(a: 1) /* never closed")]);
        assert_eq!(g.diagnostics[0].code, "unterminated_comment");
    }

    #[test]
    fn escapes_and_raw_strings() {
        let g = gather(&[GatherSource::ui_asset(
            "a.ron",
            "(a: \"say \\\"loc:no\\\"\", b: r#\"loc:raw-key\"#, c: \"x\\\\\", d: Loc(\"after-escape\"))",
        )]);
        let keys: Vec<&str> = g.manifest.keys.keys().map(String::as_str).collect();
        assert_eq!(keys, ["after-escape", "raw-key"]);
        assert!(g.diagnostics.is_empty(), "{:?}", g.diagnostics);
    }

    #[test]
    fn identifier_ending_in_r_is_not_a_raw_string() {
        let g = gather(&[GatherSource::ui_asset(
            "a.ron",
            "(color\"loc:x\" , other: Loc(\"k\"))",
        )]);
        let keys: Vec<&str> = g.manifest.keys.keys().map(String::as_str).collect();
        assert_eq!(keys, ["k", "x"]);
    }

    #[test]
    fn undefined_and_unused_keys() {
        let g = gather(&[
            GatherSource::ftl("en.ftl", "en", "used = 1\nunused = 2\n"),
            GatherSource::ui_asset("a.ron", "(a: Loc(\"used\"), b: Loc(\"ghost\"))"),
        ]);
        assert_eq!(g.manifest.unused_keys(), ["unused"]);
        let undefined = g.manifest.undefined_keys();
        assert_eq!(undefined.len(), 1);
        assert_eq!(undefined[0].0, "ghost");
        assert_eq!(undefined[0].1[0].at.line, 1);
    }

    #[test]
    fn result_does_not_depend_on_input_order() {
        let a = GatherSource::ftl("en/a.ftl", "en", "a = 1\n");
        let b = GatherSource::ftl("fr/a.ftl", "fr", "a = un\nb = deux\n");
        let c = GatherSource::ui_asset("ui/z.ron", "(x: Loc(\"b\"))");
        let d = GatherSource::scene("ui/a.ron", "(x: \"loc:a\")");
        let one = gather(&[a.clone(), b.clone(), c.clone(), d.clone()]);
        let two = gather(&[d, c, b, a]);
        assert_eq!(one, two);
        assert_eq!(
            one.manifest.to_json().expect("json"),
            two.manifest.to_json().expect("json")
        );
    }

    #[test]
    fn json_and_text_forms() {
        let g = gather(&[
            GatherSource::ftl("en.ftl", "en", "a = 1\n"),
            GatherSource::ui_asset("u.ron", "(x: Loc(\"a\"), y: Loc(\"zz\"))"),
        ]);
        let json = g.manifest.to_json().expect("json");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parses");
        assert_eq!(v["keys"]["a"]["definitions"]["en"][0]["line"], 1);
        assert_eq!(v["keys"]["a"]["references"][0]["kind"], "ui");
        assert_eq!(
            g.manifest.to_text(),
            "a  defined: en  refs: 1\nzz  defined: -  refs: 1\n"
        );
    }

    #[test]
    fn empty_locale_tag_is_rejected_without_a_panic() {
        let g = gather(&[GatherSource::ftl("x.ftl", "  ", "a = 1\n")]);
        assert_eq!(g.diagnostics[0].code, "empty_locale");
        assert!(g.manifest.keys.is_empty());
        assert!(g.manifest.locales.is_empty());
    }

    #[test]
    fn empty_input_gathers_nothing() {
        let g = gather(&[]);
        assert_eq!(g, Gathered::default());
    }
}
