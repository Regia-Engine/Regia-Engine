// SPDX-License-Identifier: MPL-2.0
//! Translator exchange through gettext PO files (spec 36 § 3, ADR-1668).
//!
//! Regia's source of truth stays Fluent; a PO file is the hand-off to translators who use a standard
//! tool. This module is pure (no file IO): [`read_po`] and [`write_po`] convert text, [`export_po`]
//! builds the file for one locale and [`plan_import`] decides which entries become
//! `imported.ftl`. The CLI (`regia loc export|import`) does the reading and writing.
//!
//! The entry layout is the common gettext convention for keyed messages: `msgctxt` is the message key,
//! `msgid` the source pattern as written in Fluent, `msgstr` the translation. The PO file format is
//! public (GNU gettext manual); the parser and writer here are original code. Output is
//! deterministic: entries sorted by key, UTF-8, `\n` newlines, no line wrapping.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::loc_check::{self, CheckOptions, Severity};

/// The value of the `X-Regia-Export-Version` header.
pub const EXPORT_VERSION: &str = "1";
/// The comment `regia loc gather` puts above a placeholder translation.
pub const PLACEHOLDER_MARKER: &str = "# TODO translate";
/// The comment `imported.ftl` carries above each message: a hash of the source text it was made against.
pub const SOURCE_HASH_PREFIX: &str = "# regia-source:";

/// One PO entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PoEntry {
    /// `msgctxt`: the Fluent message key.
    pub ctxt: String,
    /// `msgid`: the source pattern in Fluent syntax.
    pub msgid: String,
    /// `msgstr`: the translation pattern (empty when untranslated).
    pub msgstr: String,
    /// Comment lines without the marker: `#.` extracted comments, then `# ` translator comments.
    pub comments: Vec<String>,
    /// `#:` source references (`file:line`).
    pub refs: Vec<String>,
    /// `#,` flags (`fluent-format`, `fuzzy`).
    pub flags: Vec<String>,
}

/// A parsed or generated PO file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PoFile {
    /// Header fields in order (`Language`, `Content-Type`, ...).
    pub header: Vec<(String, String)>,
    /// The entries (the header entry is not among them).
    pub entries: Vec<PoEntry>,
}

impl PoFile {
    /// The value of header field `name`.
    #[must_use]
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.header
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A PO file that does not read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("PO line {line}: {message}")]
pub struct PoError {
    /// 1-based line.
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// Write `file` as PO text: header entry, then entries in the order given (the exporter sorts).
#[must_use]
pub fn write_po(file: &PoFile) -> String {
    let mut out = String::new();
    out.push_str("msgid \"\"\nmsgstr \"\"\n");
    for (key, value) in &file.header {
        out.push_str(&format!("\"{}\"\n", escape(&format!("{key}: {value}\n"))));
    }
    for entry in &file.entries {
        out.push('\n');
        for c in &entry.comments {
            out.push_str(&format!("#. {c}\n"));
        }
        for r in &entry.refs {
            out.push_str(&format!("#: {r}\n"));
        }
        if !entry.flags.is_empty() {
            out.push_str(&format!("#, {}\n", entry.flags.join(", ")));
        }
        out.push_str(&format!("msgctxt \"{}\"\n", escape(&entry.ctxt)));
        out.push_str(&format!("msgid \"{}\"\n", escape(&entry.msgid)));
        out.push_str(&format!("msgstr \"{}\"\n", escape(&entry.msgstr)));
    }
    out
}

fn unquote(raw: &str, line: usize) -> Result<String, PoError> {
    let raw = raw.trim();
    let inner = raw
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .ok_or_else(|| PoError {
            line,
            message: format!("expected a quoted string, found `{raw}`"),
        })?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => {
                return Err(PoError {
                    line,
                    message: "a string ends in a lone backslash".into(),
                });
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Ctxt,
    Id,
    Str,
}

/// Parse PO text. Reads CRLF, a BOM, multi-line (continuation) strings, `#.`, `#:`, `#,` and translator
/// comments; obsolete `#~` entries are skipped. Plural forms are not supported (Fluent selects instead).
///
/// # Errors
///
/// [`PoError`] with the line of the first thing that is not PO.
pub fn read_po(text: &str) -> Result<PoFile, PoError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut file = PoFile::default();
    let mut current = PoEntry::default();
    let mut field: Option<Field> = None;
    let mut started = false;
    let mut start_line = 0usize;
    let mut finished: Vec<(PoEntry, usize)> = Vec::new();
    let mut flush = |entry: &mut PoEntry, started: &mut bool, line: usize| {
        if *started {
            finished.push((std::mem::take(entry), line));
        }
        *started = false;
    };
    for (index, raw) in text.lines().enumerate() {
        let n = index + 1;
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() {
            flush(&mut current, &mut started, start_line);
            field = None;
            continue;
        }
        if line.starts_with("#~") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            if field == Some(Field::Str) {
                flush(&mut current, &mut started, start_line);
                field = None;
            }
            if !started {
                start_line = n;
            }
            started = true;
            if let Some(c) = rest.strip_prefix('.') {
                current.comments.push(c.trim().to_owned());
            } else if let Some(r) = rest.strip_prefix(':') {
                current.refs.extend(r.split_whitespace().map(str::to_owned));
            } else if let Some(f) = rest.strip_prefix(',') {
                current.flags.extend(
                    f.split(',')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty()),
                );
            } else {
                current.comments.push(rest.trim().to_owned());
            }
            continue;
        }
        if line.starts_with('"') {
            let s = unquote(line, n)?;
            match field {
                Some(Field::Ctxt) => current.ctxt.push_str(&s),
                Some(Field::Id) => current.msgid.push_str(&s),
                Some(Field::Str) => current.msgstr.push_str(&s),
                None => {
                    return Err(PoError {
                        line: n,
                        message: "a string continues nothing".into(),
                    });
                }
            }
            continue;
        }
        let (keyword, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match keyword {
            "msgctxt" => {
                if field == Some(Field::Str) {
                    flush(&mut current, &mut started, start_line);
                }
                if !started {
                    start_line = n;
                }
                started = true;
                field = Some(Field::Ctxt);
                current.ctxt = unquote(rest, n)?;
            }
            "msgid" => {
                if field == Some(Field::Str) {
                    flush(&mut current, &mut started, start_line);
                }
                if !started {
                    start_line = n;
                }
                started = true;
                field = Some(Field::Id);
                current.msgid = unquote(rest, n)?;
            }
            "msgstr" => {
                field = Some(Field::Str);
                current.msgstr = unquote(rest, n)?;
            }
            "msgid_plural" | "msgstr[0]" | "msgstr[1]" => {
                return Err(PoError {
                    line: n,
                    message: "plural entries are not supported; Fluent selects the plural form".into(),
                });
            }
            other => {
                return Err(PoError {
                    line: n,
                    message: format!("unknown keyword `{other}`"),
                });
            }
        }
    }
    flush(&mut current, &mut started, start_line);
    for (entry, _) in finished {
        if entry.ctxt.is_empty() && entry.msgid.is_empty() && file.header.is_empty() {
            for l in entry.msgstr.lines() {
                if let Some((k, v)) = l.split_once(':') {
                    file.header.push((k.trim().to_owned(), v.trim().to_owned()));
                }
            }
        } else {
            file.entries.push(entry);
        }
    }
    Ok(file)
}

/// One Fluent message block read from `.ftl` text without a full parse: the value pattern verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtlBlock {
    /// The message id.
    pub key: String,
    /// The value pattern: the text after `=` (first line) and continuation lines with their common
    /// indent removed, joined with `\n`. Empty for a message with only attributes.
    pub pattern: String,
    /// 1-based line of the `key =` line.
    pub line: u32,
    /// The message has attributes (`.name = ...`).
    pub has_attributes: bool,
    /// Text of `# note: ...` comment lines directly above.
    pub notes: Vec<String>,
    /// The translator context: every `#` comment line directly above that is not a tool marker
    /// (`# TODO translate`, `# regia-source:`, `# source:`, a `##` group comment), with a leading `note:`
    /// removed. Exports carry each line as a note (`#.` in PO, a note in XLIFF).
    pub context: Vec<String>,
    /// The comment lines directly above include [`PLACEHOLDER_MARKER`].
    pub placeholder: bool,
    /// The source hash recorded above by `imported.ftl`, when present.
    pub source_hash: Option<String>,
    /// Index of the first line of the block (comments above included) and of the line after it.
    pub span: (usize, usize),
    /// The block's own text (the message line, its continuation lines and attributes; no comments),
    /// lines joined with `\n`, trailing blank lines trimmed.
    pub raw: String,
}

/// A message id, or (when `terms` is set) a Fluent term id including its leading `-`.
///
/// The identifier itself is ASCII. Term ids are returned with the `-` so they do not collide
/// with a message of the same name. PO export passes `terms = false` (spec 36: terms are not
/// exported).
fn is_entry_start(line: &str, terms: bool) -> Option<&str> {
    let (body, prefix) = if terms {
        line.strip_prefix('-').map_or((line, 0), |body| (body, 1))
    } else {
        (line, 0)
    };
    let id_len = body
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .count();
    if id_len == 0 || !body.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let (_, rest) = body.split_at(id_len);
    rest.trim_start()
        .strip_prefix('=')
        .map(|_| &line[..prefix + id_len])
}

/// Read the message blocks of `.ftl` text (terms and standalone comments are skipped).
#[must_use]
pub fn ftl_blocks(text: &str) -> Vec<FtlBlock> {
    scan_ftl_blocks(text, false)
}

/// Message blocks plus Fluent terms (`-brand = ...`). Shipping bundles use this so a packaged
/// locale can still resolve `{ -brand }`. PO export stays on [`ftl_blocks`].
#[must_use]
pub(crate) fn ftl_blocks_with_terms(text: &str) -> Vec<FtlBlock> {
    scan_ftl_blocks(text, true)
}

/// `{` minus `}` in one line of a pattern.
fn brace_depth(line: &str) -> i64 {
    line.bytes().fold(0, |depth, b| match b {
        b'{' => depth + 1,
        b'}' => depth - 1,
        _ => depth,
    })
}

fn scan_ftl_blocks(text: &str, terms: bool) -> Vec<FtlBlock> {
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let Some(key) = is_entry_start(line, terms) else {
            i += 1;
            continue;
        };
        // Comments directly above.
        let mut first = i;
        while first > 0 && lines[first - 1].starts_with('#') {
            first -= 1;
        }
        let comments = &lines[first..i];
        let after_eq = line.split_once('=').map_or("", |(_, r)| r.trim_start());
        let mut pattern_lines: Vec<String> = Vec::new();
        if !after_eq.is_empty() {
            pattern_lines.push(after_eq.to_owned());
        }
        let mut has_attributes = false;
        let mut j = i + 1;
        let mut continuation: Vec<&str> = Vec::new();
        // Placeables still open in the value: Fluent lets the `}` that closes one (a select
        // expression's, typically) start a line at column 0, and that line is still the message.
        let mut open = brace_depth(after_eq);
        let closes = |l: &str, open: i64| open > 0 && l.starts_with('}');
        while j < lines.len() {
            let l = lines[j];
            if l.is_empty() {
                // A blank line belongs to the pattern only if an indented line follows.
                let mut k = j;
                while k < lines.len() && lines[k].is_empty() {
                    k += 1;
                }
                if k < lines.len()
                    && ((lines[k].starts_with([' ', '\t']) && !lines[k].trim_start().starts_with('.'))
                        || closes(lines[k], open))
                {
                    continuation.extend(std::iter::repeat_n("", k - j));
                    j = k;
                    continue;
                }
                break;
            }
            if closes(l, open) {
                open += brace_depth(l);
                continuation.push(l);
                j += 1;
                continue;
            }
            if !l.starts_with([' ', '\t']) {
                break;
            }
            if l.trim_start().starts_with('.') && l.trim_start()[1..].contains('=') {
                has_attributes = true;
                // Attributes (and their continuation lines) are not part of the value.
                j += 1;
                while j < lines.len()
                    && lines[j].starts_with([' ', '\t'])
                    && !lines[j].trim_start().starts_with('.')
                {
                    j += 1;
                }
                continue;
            }
            open += brace_depth(l);
            continuation.push(l);
            j += 1;
        }
        // Indentation is spaces and tabs only (ASCII, so the cut below is on a char boundary):
        // a line that starts with an ideographic space keeps it as text. A column-0 `}` does
        // not set the common indent.
        let lead = |l: &str| l.len() - l.trim_start_matches([' ', '\t']).len();
        let indent = continuation
            .iter()
            .filter(|l| !l.is_empty() && !l.starts_with('}'))
            .map(|l| lead(l))
            .min()
            .unwrap_or(0);
        for l in continuation {
            pattern_lines.push(if l.is_empty() {
                String::new()
            } else {
                l[lead(l).min(indent)..].to_owned()
            });
        }
        out.push(FtlBlock {
            key: key.to_owned(),
            pattern: pattern_lines.join("\n"),
            line: u32::try_from(i + 1).unwrap_or(u32::MAX),
            has_attributes,
            notes: comments
                .iter()
                .filter_map(|c| c.strip_prefix("# note:"))
                .map(|n| n.trim().to_owned())
                .collect(),
            context: comments.iter().filter_map(|c| context_text(c)).collect(),
            placeholder: comments.iter().any(|c| c.trim() == PLACEHOLDER_MARKER),
            source_hash: comments
                .iter()
                .find_map(|c| c.strip_prefix(SOURCE_HASH_PREFIX))
                .map(|h| h.trim().to_owned()),
            span: (first, j),
            raw: lines[i..j].join("\n").trim_end().to_owned(),
        });
        i = j.max(i + 1);
    }
    out
}

/// Whether a comment line above a message is a tool marker, not translator context: the placeholder, the
/// source hash, the gather tool's `# source: file:line`, or a `##` group comment.
#[must_use]
pub fn is_tool_comment(line: &str) -> bool {
    line.starts_with("##")
        || line.trim() == PLACEHOLDER_MARKER
        || line.starts_with(SOURCE_HASH_PREFIX)
        || line.starts_with("# source:")
}

/// The translator-context text of a comment line (`# text`, or `# note: text`), `None` for a tool marker
/// or an empty comment.
fn context_text(line: &str) -> Option<String> {
    if is_tool_comment(line) {
        return None;
    }
    let text = line.strip_prefix('#')?.trim();
    let text = text.strip_prefix("note:").map_or(text, str::trim);
    (!text.is_empty()).then(|| text.to_owned())
}

/// A stable 64-bit hash of `text` (FNV-1a), hex; `imported.ftl` records it per message.
#[must_use]
pub fn source_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Which entries `export` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportFilter {
    /// Every source message.
    #[default]
    All,
    /// Messages the locale has no real translation for.
    Untranslated,
    /// Messages whose stored translation was made against a different source text.
    Stale,
}

/// What [`export_po`] reads.
#[derive(Debug, Clone, Default)]
pub struct ExportInput {
    /// The project name and version for `Project-Id-Version`.
    pub project: String,
    /// The source locale tag.
    pub source: String,
    /// The target locale tag.
    pub locale: String,
    /// The source locale's `.ftl` files `(path, text)` in runtime order.
    pub source_files: Vec<(String, String)>,
    /// The target locale's `.ftl` files `(path, text)`.
    pub target_files: Vec<(String, String)>,
    /// Source references per key (`file:line`) from the gather scan.
    pub references: BTreeMap<String, Vec<String>>,
    /// Which entries to write.
    pub filter: ExportFilter,
}

/// What [`export_po`] returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportOutput {
    /// The file.
    pub file: PoFile,
    /// Messages with attributes (their attributes are not exported).
    pub skipped_attributes: usize,
    /// Fluent terms (`-brand = ...`) of the source locale; terms are not exported (spec 36 § 3),
    /// and the CLI says so once.
    pub skipped_terms: usize,
}

/// The real translations of `files`: messages not marked as gather placeholders, by key.
pub(crate) fn translations(files: &[(String, String)]) -> BTreeMap<String, FtlBlock> {
    let mut out = BTreeMap::new();
    for (_, text) in files {
        for block in ftl_blocks(text) {
            if block.placeholder {
                out.remove(&block.key);
                continue;
            }
            out.insert(block.key.clone(), block);
        }
    }
    out
}

/// The source messages by key (later files win, as the runtime merges them).
pub(crate) fn source_blocks(files: &[(String, String)]) -> BTreeMap<String, FtlBlock> {
    let mut out = BTreeMap::new();
    for (_, text) in files {
        for block in ftl_blocks(text) {
            out.insert(block.key.clone(), block);
        }
    }
    out
}

/// Translation coverage of one locale against the source locale (spec 36 § 10, `localization.coverage`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LocaleCoverage {
    /// The locale tag.
    pub locale: String,
    /// Source messages with a value (the ones a translator sees).
    pub messages: usize,
    /// Of those, the ones the locale has a real translation for.
    pub translated: usize,
    /// `messages - translated`.
    pub untranslated: usize,
    /// Of the untranslated ones, those with a `# TODO translate` placeholder.
    pub placeholders: usize,
    /// Translated messages whose recorded source hash differs from the current source text.
    pub stale: usize,
    /// `translated / messages` in percent (100 when there is nothing to translate).
    pub percent: f64,
}

/// The coverage of `locale` (`target_files`) against the source locale's files, both given in runtime
/// order. The source locale against itself is fully translated.
#[must_use]
pub fn coverage(
    locale: &str,
    source_files: &[(String, String)],
    target_files: &[(String, String)],
) -> LocaleCoverage {
    let source = source_blocks(source_files);
    let target = translations(target_files);
    let mut placeholder_keys = BTreeSet::new();
    for (_, text) in target_files {
        for block in ftl_blocks(text) {
            if block.placeholder {
                placeholder_keys.insert(block.key);
            } else {
                placeholder_keys.remove(&block.key);
            }
        }
    }
    let mut out = LocaleCoverage {
        locale: locale.to_owned(),
        ..LocaleCoverage::default()
    };
    for (key, block) in source.iter().filter(|(_, b)| !b.pattern.is_empty()) {
        out.messages += 1;
        match target.get(key) {
            Some(have) => {
                out.translated += 1;
                if have
                    .source_hash
                    .as_deref()
                    .is_some_and(|h| h != source_hash(&block.pattern))
                {
                    out.stale += 1;
                }
            }
            None if placeholder_keys.contains(key) => out.placeholders += 1,
            None => {}
        }
    }
    out.untranslated = out.messages - out.translated;
    out.percent = if out.messages == 0 {
        100.0
    } else {
        // Message counts stay far below 2^52, so the conversion is exact.
        out.translated as f64 * 100.0 / out.messages as f64
    };
    out
}

/// Build the PO file of `input.locale` (spec 36 § 3 "Export").
#[must_use]
pub fn export_po(input: &ExportInput) -> ExportOutput {
    let _span = tracing::debug_span!("loc_export_po", locale = %input.locale).entered();
    let source = source_blocks(&input.source_files);
    let target = translations(&input.target_files);
    let parsed = loc_check::parse_locale(&input.source, &input.source_files);
    let mut entries = Vec::new();
    let mut skipped_attributes = 0usize;
    for (key, block) in &source {
        if block.pattern.is_empty() {
            if block.has_attributes {
                skipped_attributes += 1;
            }
            continue;
        }
        if block.has_attributes {
            skipped_attributes += 1;
        }
        let have = target.get(key);
        let hash = source_hash(&block.pattern);
        let stale = have.is_some_and(|t| t.source_hash.as_deref().is_some_and(|h| h != hash));
        let wanted = match input.filter {
            ExportFilter::All => true,
            ExportFilter::Untranslated => have.is_none(),
            ExportFilter::Stale => stale,
        };
        if !wanted {
            continue;
        }
        let mut comments = Vec::new();
        if let Some(info) = parsed.messages.get(key)
            && !info.variables.is_empty()
        {
            let vars: Vec<String> = info.variables.iter().map(|v| format!("${v}")).collect();
            comments.push(format!("variables: {}", vars.join(", ")));
        }
        for note in &block.context {
            comments.push(format!("note: {note}"));
        }
        if parsed
            .messages
            .get(key)
            .is_some_and(|m| m.selectors.iter().any(loc_check::SelectorInfo::is_plural))
            && let Some(cats) = crate::plural::categories(&input.locale)
        {
            let names: Vec<&str> = cats.iter().map(|c| c.as_str()).collect();
            comments.push(format!(
                "plural categories for {}: {}",
                input.locale,
                names.join(", ")
            ));
        }
        entries.push(PoEntry {
            ctxt: key.clone(),
            msgid: block.pattern.clone(),
            msgstr: have.map(|t| t.pattern.clone()).unwrap_or_default(),
            comments,
            refs: input.references.get(key).cloned().unwrap_or_default(),
            flags: vec!["fluent-format".to_owned()],
        });
    }
    let header = vec![
        ("Project-Id-Version".to_owned(), input.project.clone()),
        ("Language".to_owned(), input.locale.clone()),
        ("MIME-Version".to_owned(), "1.0".to_owned()),
        ("Content-Type".to_owned(), "text/plain; charset=UTF-8".to_owned()),
        ("X-Regia-Source-Locale".to_owned(), input.source.clone()),
        ("X-Regia-Export-Version".to_owned(), EXPORT_VERSION.to_owned()),
    ];
    let skipped_terms = input
        .source_files
        .iter()
        .flat_map(|(_, text)| ftl_blocks_with_terms(text))
        .filter(|b| b.key.starts_with('-'))
        .map(|b| b.key)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    ExportOutput {
        file: PoFile { header, entries },
        skipped_attributes,
        skipped_terms,
    }
}

/// What [`plan_import`] reads.
#[derive(Debug, Clone, Default)]
pub struct ImportContext {
    /// The locale being imported (`--locale`, or the file's `Language`).
    pub locale: String,
    /// The source locale tag.
    pub source: String,
    /// The source locale's `.ftl` files.
    pub source_files: Vec<(String, String)>,
    /// The existing text of `gathered.ftl` for the locale, when there is one.
    pub gathered: Option<String>,
    /// The existing text of `imported.ftl`, when there is one: earlier imports stay unless this file
    /// translates the same key again.
    pub existing_imported: Option<String>,
}

/// One note of an [`ImportReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportNote {
    /// Stable code (`loc_po_stale`, `loc_variable_mismatch`, ...).
    pub code: String,
    /// The message key.
    pub key: String,
    /// A sentence for people.
    pub message: String,
}

/// What an import did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Entries written to `imported.ftl`.
    pub imported: usize,
    /// Entries skipped, with the reason.
    pub skipped: Vec<ImportNote>,
    /// Entries imported with a warning.
    pub warnings: Vec<ImportNote>,
    /// The file itself was refused (`loc_po_language_mismatch`).
    pub refused: Option<ImportNote>,
}

impl ImportReport {
    /// How many skipped entries have `code`.
    #[must_use]
    pub fn skipped_with(&self, code: &str) -> usize {
        self.skipped.iter().filter(|n| n.code == code).count()
    }

    /// Whether a CLI run should exit 1: the file was refused, or (`strict`) an entry was stale or fuzzy.
    #[must_use]
    pub fn failed(&self, strict: bool) -> bool {
        self.refused.is_some()
            || (strict
                && self
                    .skipped
                    .iter()
                    .any(|n| n.code == "loc_po_stale" || n.code == "loc_po_fuzzy"))
    }
}

/// The outcome of [`plan_import`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportPlan {
    /// The whole new `imported.ftl` (empty when nothing was accepted).
    pub imported_ftl: String,
    /// The new `gathered.ftl`, when matching placeholders were removed.
    pub gathered_ftl: Option<String>,
    /// The report.
    pub report: ImportReport,
}

fn note(code: &str, key: &str, message: impl Into<String>) -> ImportNote {
    ImportNote {
        code: code.to_owned(),
        key: key.to_owned(),
        message: message.into(),
    }
}

/// The 1-based line of `translation` that byte `pos` of its rendered message (`ftl_message`)
/// falls on (spec 36 § 3 import step 4: the parser message and the line within the entry).
fn line_in_translation(candidate: &str, translation: &str, pos: usize) -> usize {
    if !translation.contains('\n') {
        return 1;
    }
    // A multi-line message renders as `key =` and then one line per translation line.
    let before = candidate
        .as_bytes()
        .get(..pos.min(candidate.len()))
        .unwrap_or_default();
    let newlines = before.iter().filter(|b| **b == b'\n').count();
    newlines.clamp(1, translation.lines().count().max(1))
}

/// Render one Fluent message `key = pattern` with continuation indentation.
fn ftl_message(key: &str, pattern: &str) -> String {
    if pattern.contains('\n') {
        let body: Vec<String> = pattern
            .lines()
            .map(|l| {
                if l.is_empty() {
                    String::new()
                } else {
                    format!("    {l}")
                }
            })
            .collect();
        format!("{key} =\n{}\n", body.join("\n"))
    } else {
        format!("{key} = {pattern}\n")
    }
}

/// Decide the import of `po` (spec 36 § 3 "Import"). Pure: returns the files to write.
#[must_use]
pub fn plan_import(po: &PoFile, ctx: &ImportContext) -> ImportPlan {
    let _span =
        tracing::debug_span!("loc_import_po", locale = %ctx.locale, entries = po.entries.len()).entered();
    let mut plan = ImportPlan::default();
    match po.header_value("Language") {
        Some(l) if l == ctx.locale => {}
        other => {
            plan.report.refused = Some(note(
                "loc_po_language_mismatch",
                "",
                format!(
                    "the file's Language is {} but the import is for {}",
                    other.map_or("missing".to_owned(), |l| format!("`{l}`")),
                    ctx.locale
                ),
            ));
            return plan;
        }
    }
    let source = source_blocks(&ctx.source_files);
    let source_parsed = loc_check::parse_locale(&ctx.source, &ctx.source_files);
    let mut accepted: BTreeMap<String, (String, String)> = BTreeMap::new(); // key -> (pattern, hash)
    for entry in &po.entries {
        if entry.msgstr.trim().is_empty() {
            continue;
        }
        let key = entry.ctxt.as_str();
        let Some(src) = source.get(key) else {
            plan.report.skipped.push(note(
                "loc_po_unknown_key",
                key,
                format!("`{key}` is not a message of the source locale"),
            ));
            continue;
        };
        if entry.flags.iter().any(|f| f == "fuzzy") {
            plan.report.skipped.push(note(
                "loc_po_fuzzy",
                key,
                "the entry is marked fuzzy; confirm the translation first",
            ));
            continue;
        }
        if entry.msgid != src.pattern {
            plan.report.skipped.push(note(
                "loc_po_stale",
                key,
                "the source text changed since the export; export again and update the translation",
            ));
            continue;
        }
        let translation = entry.msgstr.trim_end_matches('\n');
        let candidate = ftl_message(key, translation);
        if let Err((_, errors)) = fluent_syntax::parser::parse(candidate.as_str()) {
            let first = errors.first();
            let line = first.map_or(1, |e| line_in_translation(&candidate, translation, e.pos.start));
            plan.report.skipped.push(note(
                "loc_po_bad_pattern",
                key,
                format!(
                    "the translation does not parse (line {line} of the translation): {}",
                    first.map_or_else(String::new, |e| format!("{:?}", e.kind))
                ),
            ));
            continue;
        }
        // Run the existing translation checks on just this message.
        let files = vec![("import.ftl".to_owned(), candidate.clone())];
        let target = loc_check::parse_locale(&ctx.locale, &files);
        // Only this key's findings count below, and they read only this key of the source: checking
        // the whole source for every entry made an import quadratic (spec 36 § 12 budget).
        let source_message = loc_check::ParsedLocale {
            locale: source_parsed.locale.clone(),
            messages: source_parsed
                .messages
                .get(key)
                .map(|m| (key.to_owned(), m.clone()))
                .into_iter()
                .collect(),
            findings: Vec::new(),
        };
        let report = loc_check::validate(
            &[source_message, target],
            &CheckOptions {
                source: ctx.source.clone(),
                ..CheckOptions::default()
            },
        );
        let mut skip = None;
        let mut warns = Vec::new();
        for f in &report.findings {
            if f.key.as_deref() != Some(key) || f.locale != ctx.locale {
                continue;
            }
            if f.code != "loc_variable_mismatch" && f.code != "loc_plural_missing" {
                continue;
            }
            // A translation that drops (or adds) a variable is refused whatever the validator's own
            // severity is: the spec's acceptance rule for import.
            if f.severity == Severity::Error || f.code == "loc_variable_mismatch" {
                skip = Some(note(f.code, key, f.message.clone()));
            } else {
                warns.push(note(f.code, key, f.message.clone()));
            }
        }
        if let Some(n) = skip {
            plan.report.skipped.push(n);
            continue;
        }
        plan.report.warnings.extend(warns);
        accepted.insert(
            key.to_owned(),
            (
                entry.msgstr.trim_end_matches('\n').to_owned(),
                source_hash(&src.pattern),
            ),
        );
    }
    plan.report.imported = accepted.len();
    if !accepted.is_empty() {
        let mut all: BTreeMap<String, (String, String)> = BTreeMap::new();
        if let Some(existing) = &ctx.existing_imported {
            for b in ftl_blocks(existing) {
                let hash = b.source_hash.clone().unwrap_or_default();
                all.insert(b.key, (b.pattern, hash));
            }
        }
        all.extend(accepted.clone());
        let mut ftl = String::new();
        for (key, (pattern, hash)) in &all {
            if hash.is_empty() {
                ftl.push_str(&ftl_message(key, pattern));
                continue;
            }
            ftl.push_str(&format!("{SOURCE_HASH_PREFIX} {hash}\n"));
            ftl.push_str(&ftl_message(key, pattern));
        }
        plan.imported_ftl = ftl;
        if let Some(gathered) = &ctx.gathered {
            let updated = remove_placeholders(gathered, &accepted.keys().cloned().collect());
            if &updated != gathered {
                plan.gathered_ftl = Some(updated);
            }
        }
    }
    plan
}

/// Remove the `# TODO translate` placeholder blocks of `keys` from gathered text: the marker, the
/// `# source:` comment above it and the message lines. Any other text stays.
#[must_use]
pub fn remove_placeholders(gathered: &str, keys: &BTreeSet<String>) -> String {
    let lines: Vec<&str> = gathered.lines().collect();
    let mut drop = vec![false; lines.len()];
    for block in ftl_blocks(gathered) {
        if !block.placeholder || !keys.contains(&block.key) {
            continue;
        }
        let (first, end) = block.span;
        for slot in &mut drop[first..end] {
            *slot = true;
        }
        // A `# source:` line just above the comment block, and the blank separator before it.
        let mut above = first;
        if above > 0 && lines[above - 1].starts_with("# source:") {
            above -= 1;
            drop[above] = true;
        }
        if above > 0 && lines[above - 1].is_empty() {
            drop[above - 1] = true;
        }
    }
    // Kept lines keep their own endings (CR LF, or none on the last line): rebuilding with `\n`
    // turned a no-op into a rewrite of the gathered file.
    // Regression: tests/tqs_sweep.rs::remove_placeholders_is_a_no_op_when_nothing_is_removed
    let mut out = String::with_capacity(gathered.len());
    for (line, dropped) in gathered.split_inclusive('\n').zip(drop) {
        if !dropped {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "\
# note: shown in the pause menu
menu-quit = Quit
hud-ammo = { $count ->
    [one] { $count } round
   *[other] { $count } rounds
    }
greet = Hello, { $name }!
    .title = Greeting
";

    fn input(target: &str) -> ExportInput {
        ExportInput {
            project: "demo 1.0".into(),
            source: "en".into(),
            locale: "fr".into(),
            source_files: vec![("assets/localization/en/main.ftl".into(), SOURCE.into())],
            target_files: if target.is_empty() {
                Vec::new()
            } else {
                vec![("assets/localization/fr/main.ftl".into(), target.into())]
            },
            references: BTreeMap::from([("menu-quit".into(), vec!["scenes/menu.ron:12".into()])]),
            filter: ExportFilter::All,
        }
    }

    #[test]
    fn export_has_header_sorted_entries_comments_and_flags() {
        let out = export_po(&input(""));
        let po = &out.file;
        assert_eq!(po.header_value("Language"), Some("fr"));
        assert_eq!(po.header_value("X-Regia-Export-Version"), Some("1"));
        assert_eq!(po.header_value("Content-Type"), Some("text/plain; charset=UTF-8"));
        let keys: Vec<&str> = po.entries.iter().map(|e| e.ctxt.as_str()).collect();
        assert_eq!(keys, ["greet", "hud-ammo", "menu-quit"]);
        assert_eq!(out.skipped_attributes, 1);
        let ammo = &po.entries[1];
        assert!(
            ammo.msgid.starts_with("{ $count ->\n [one] { $count } round"),
            "{:?}",
            ammo.msgid
        );
        assert!(ammo.comments.contains(&"variables: $count".to_owned()));
        assert!(
            ammo.comments
                .iter()
                .any(|c| c.starts_with("plural categories for fr: one")),
            "{:?}",
            ammo.comments
        );
        assert_eq!(po.entries[2].comments, ["note: shown in the pause menu"]);
        assert_eq!(po.entries[2].refs, ["scenes/menu.ron:12"]);
        assert!(
            po.entries
                .iter()
                .all(|e| e.flags == ["fluent-format"] && e.msgstr.is_empty())
        );
        // Deterministic.
        assert_eq!(write_po(&export_po(&input("")).file), write_po(po));
    }

    #[test]
    fn write_then_read_round_trips_with_escapes() {
        let mut out = export_po(&input("")).file;
        out.entries[0].msgstr = "Tab\there \"quoted\" back\\slash\nline two".into();
        let text = write_po(&out);
        assert!(!text.contains("\r"));
        let back = read_po(&text).expect("reads");
        assert_eq!(back, out);
    }

    #[test]
    fn read_handles_crlf_bom_continuations_and_obsolete() {
        let text = "\u{feff}msgid \"\"\r\nmsgstr \"\"\r\n\"Language: fr\\n\"\r\n\"X-Other: a: b\\n\"\r\n\r\n\
                    #. variables: $name\r\n#: a.ron:1 b.ron:2\r\n#, fuzzy, fluent-format\r\nmsgctxt \"greet\"\r\nmsgid \"Hello, { $name }!\"\r\n\
                    msgstr \"\"\r\n\"Bonjour, \"\r\n\"{ $name } !\"\r\n\r\n#~ msgid \"gone\"\r\n#~ msgstr \"parti\"\r\n";
        let po = read_po(text).expect("reads");
        assert_eq!(po.header_value("Language"), Some("fr"));
        assert_eq!(po.header_value("X-Other"), Some("a: b"));
        assert_eq!(po.entries.len(), 1);
        let e = &po.entries[0];
        assert_eq!(e.msgstr, "Bonjour, { $name } !");
        assert_eq!(e.refs, ["a.ron:1", "b.ron:2"]);
        assert_eq!(e.flags, ["fuzzy", "fluent-format"]);
        assert_eq!(e.comments, ["variables: $name"]);
    }

    #[test]
    fn read_rejects_garbage_with_a_line() {
        let err = read_po("msgid \"\"\nmsgstr \"\"\n\nnonsense here\n").expect_err("bad");
        assert_eq!(err.line, 4);
        let err = read_po("msgctxt \"a\"\nmsgid \"b\"\nmsgid_plural \"c\"\n").expect_err("plural");
        assert_eq!(err.line, 3);
    }

    fn filled(po: &mut PoFile, key: &str, msgstr: &str) {
        po.entries
            .iter_mut()
            .find(|e| e.ctxt == key)
            .expect("entry")
            .msgstr = msgstr.into();
    }

    fn ctx() -> ImportContext {
        ImportContext {
            locale: "fr".into(),
            source: "en".into(),
            source_files: vec![("en/main.ftl".into(), SOURCE.into())],
            gathered: None,
            existing_imported: None,
        }
    }

    #[test]
    fn round_trip_export_fill_import_reproduces_messages() {
        let mut po = export_po(&input("")).file;
        filled(&mut po, "menu-quit", "Quitter");
        filled(&mut po, "greet", "Bonjour, { $name } !");
        filled(
            &mut po,
            "hud-ammo",
            "{ $count ->\n    [one] { $count } cartouche\n   *[other] { $count } cartouches\n}",
        );
        let plan = plan_import(&po, &ctx());
        assert_eq!(plan.report.imported, 3, "{:?}", plan.report);
        assert!(plan.report.skipped.is_empty());
        let blocks = ftl_blocks(&plan.imported_ftl);
        assert_eq!(blocks.len(), 3);
        let quit = blocks.iter().find(|b| b.key == "menu-quit").expect("quit");
        assert_eq!(quit.pattern, "Quitter");
        assert_eq!(quit.source_hash.as_deref(), Some(source_hash("Quit").as_str()));
        let ammo = blocks.iter().find(|b| b.key == "hud-ammo").expect("ammo");
        assert!(ammo.pattern.contains("[one] { $count } cartouche"));
        // The produced Fluent parses and passes the existing validator.
        let files = vec![("imported.ftl".to_owned(), plan.imported_ftl.clone())];
        let parsed = loc_check::parse_locale("fr", &files);
        assert!(parsed.findings.is_empty(), "{:?}", parsed.findings);
        // Sorted by key; idempotent.
        let keys: Vec<&str> = blocks.iter().map(|b| b.key.as_str()).collect();
        assert_eq!(keys, ["greet", "hud-ammo", "menu-quit"]);
        assert_eq!(plan_import(&po, &ctx()).imported_ftl, plan.imported_ftl);
        // The translated file re-exports with the translations filled in and as not stale.
        let mut again = input("");
        again.target_files = files;
        again.filter = ExportFilter::Stale;
        assert!(export_po(&again).file.entries.is_empty());
        again.filter = ExportFilter::Untranslated;
        assert!(export_po(&again).file.entries.is_empty());
    }

    #[test]
    fn stale_fuzzy_unknown_and_bad_entries_are_skipped_with_codes() {
        let mut po = export_po(&input("")).file;
        filled(&mut po, "menu-quit", "Quitter");
        po.entries
            .iter_mut()
            .find(|e| e.ctxt == "menu-quit")
            .expect("quit")
            .msgid = "Old text".into();
        filled(&mut po, "greet", "Bonjour, { $name } !");
        po.entries
            .iter_mut()
            .find(|e| e.ctxt == "greet")
            .expect("greet")
            .flags
            .push("fuzzy".into());
        po.entries.push(PoEntry {
            ctxt: "nope".into(),
            msgid: "x".into(),
            msgstr: "y".into(),
            ..PoEntry::default()
        });
        filled(&mut po, "hud-ammo", "{ $count -> [one] broken");
        let plan = plan_import(&po, &ctx());
        assert_eq!(plan.report.imported, 0);
        assert_eq!(plan.report.skipped_with("loc_po_stale"), 1);
        assert_eq!(plan.report.skipped_with("loc_po_fuzzy"), 1);
        assert_eq!(plan.report.skipped_with("loc_po_unknown_key"), 1);
        assert_eq!(plan.report.skipped_with("loc_po_bad_pattern"), 1);
        let bad = plan
            .report
            .skipped
            .iter()
            .find(|n| n.code == "loc_po_bad_pattern")
            .expect("bad pattern note");
        assert!(
            bad.message.contains("(line 1 of the translation)"),
            "{}",
            bad.message
        );
        assert!(plan.imported_ftl.is_empty());
        assert!(!plan.report.failed(false));
        assert!(plan.report.failed(true));
    }

    /// Spec 36 § 3 import step 4: a parse failure names the line inside the translation
    /// (lane spec-15-35, ADR-5778).
    #[test]
    fn a_bad_pattern_names_its_line_inside_the_translation() {
        let mut po = export_po(&input("")).file;
        filled(&mut po, "menu-quit", "Quitter le jeu\n{ broken");
        let plan = plan_import(&po, &ctx());
        let bad = plan
            .report
            .skipped
            .iter()
            .find(|n| n.code == "loc_po_bad_pattern")
            .expect("bad pattern note");
        assert_eq!(bad.key, "menu-quit");
        assert!(
            bad.message.contains("(line 2 of the translation)"),
            "{}",
            bad.message
        );
        assert_eq!(line_in_translation("k = x\n", "x", 3), 1);
        assert_eq!(
            line_in_translation("k =\n    a\n    b\n", "a\nb", 0),
            1,
            "clamped up"
        );
        assert_eq!(
            line_in_translation("k =\n    a\n    b\n", "a\nb", 999),
            2,
            "clamped down"
        );
    }

    #[test]
    fn a_translation_dropping_a_variable_is_an_error_and_a_missing_plural_a_warning() {
        let mut po = export_po(&input("")).file;
        filled(&mut po, "greet", "Bonjour !");
        filled(
            &mut po,
            "hud-ammo",
            "{ $count ->\n   *[other] { $count } cartouches\n}",
        );
        let plan = plan_import(&po, &ctx());
        assert_eq!(
            plan.report.skipped_with("loc_variable_mismatch"),
            1,
            "{:?}",
            plan.report
        );
        assert!(plan.report.imported <= 1);
    }

    #[test]
    fn each_entry_is_checked_against_its_own_source_message() {
        let mut po = export_po(&input("")).file;
        filled(&mut po, "menu-quit", "Quitter");
        filled(&mut po, "greet", "Bonjour !");
        filled(
            &mut po,
            "hud-ammo",
            "{ $count ->\n    [many] { $count } cartouches !\n   *[other] { $count } cartouches\n}",
        );
        let plan = plan_import(&po, &ctx());
        assert_eq!(plan.report.imported, 2, "{:?}", plan.report);
        let skipped: Vec<&str> = plan.report.skipped.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(skipped, ["greet"]);
        let warned: Vec<&str> = plan.report.warnings.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(warned, ["hud-ammo"]);
    }

    #[test]
    fn a_partial_second_import_keeps_earlier_translations() {
        let mut first = export_po(&input("")).file;
        filled(&mut first, "menu-quit", "Quitter");
        let one = plan_import(&first, &ctx());
        let mut second = export_po(&input("")).file;
        filled(&mut second, "greet", "Bonjour, { $name } !");
        let mut c = ctx();
        c.existing_imported = Some(one.imported_ftl.clone());
        let two = plan_import(&second, &c);
        assert_eq!(two.report.imported, 1);
        let keys: Vec<String> = ftl_blocks(&two.imported_ftl).into_iter().map(|b| b.key).collect();
        assert_eq!(keys, ["greet", "menu-quit"]);
        // Re-importing the same file changes nothing.
        c.existing_imported = Some(two.imported_ftl.clone());
        assert_eq!(plan_import(&second, &c).imported_ftl, two.imported_ftl);
    }

    #[test]
    fn language_mismatch_refuses_the_file() {
        let mut po = export_po(&input("")).file;
        filled(&mut po, "menu-quit", "Quitter");
        let mut c = ctx();
        c.locale = "de".into();
        let plan = plan_import(&po, &c);
        assert_eq!(
            plan.report.refused.as_ref().map(|n| n.code.as_str()),
            Some("loc_po_language_mismatch")
        );
        assert!(plan.report.failed(false));
        po.header.retain(|(k, _)| k != "Language");
        assert!(plan_import(&po, &ctx()).report.refused.is_some());
    }

    #[test]
    fn stale_filter_finds_translations_made_against_old_source() {
        let target = format!(
            "{SOURCE_HASH_PREFIX} {}\nmenu-quit = Quitter\n",
            source_hash("Exit")
        );
        let mut i = input(&target);
        i.filter = ExportFilter::Stale;
        let out = export_po(&i);
        assert_eq!(out.file.entries.len(), 1);
        assert_eq!(out.file.entries[0].ctxt, "menu-quit");
        assert_eq!(out.file.entries[0].msgstr, "Quitter");
        // A hand-written translation has no recorded hash, so it is never stale.
        let mut i = input("menu-quit = Quitter\n");
        i.filter = ExportFilter::Stale;
        assert!(export_po(&i).file.entries.is_empty());
        i.filter = ExportFilter::Untranslated;
        assert_eq!(export_po(&i).file.entries.len(), 2);
    }

    #[test]
    fn placeholders_are_removed_from_gathered_only_for_imported_keys() {
        let gathered = "\n# source: scenes/menu.ron:12\n# TODO translate\nmenu-quit = Quit\n\n# source: hud.ron:1\n# TODO translate\ngreet = Hello, { $name }!\n    .title = Greeting\n\n# hand written\nother = Autre\n";
        let mut po = export_po(&input("")).file;
        filled(&mut po, "menu-quit", "Quitter");
        let mut c = ctx();
        c.gathered = Some(gathered.into());
        let plan = plan_import(&po, &c);
        let updated = plan.gathered_ftl.expect("changed");
        assert!(!updated.contains("menu-quit"), "{updated}");
        assert!(updated.contains("greet = Hello"));
        assert!(updated.contains("other = Autre"));
        assert!(updated.contains("# hand written"));
        // Placeholders count as untranslated on export.
        let mut i = input(gathered);
        i.filter = ExportFilter::Untranslated;
        assert_eq!(export_po(&i).file.entries.len(), 3);
    }

    #[test]
    fn ftl_blocks_read_multiline_patterns_attributes_and_notes() {
        let blocks = ftl_blocks(SOURCE);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].notes, ["shown in the pause menu"]);
        assert_eq!(blocks[0].pattern, "Quit");
        assert_eq!(blocks[1].key, "hud-ammo");
        assert_eq!(blocks[1].pattern.lines().count(), 4);
        assert!(blocks[2].has_attributes);
        assert_eq!(blocks[2].pattern, "Hello, { $name }!");
    }

    #[test]
    fn export_counts_the_terms_it_leaves_out() {
        // Spec 36 § 3: terms are not exported and the export says so (lane spec-15-35, ADR-5778).
        let mut with_terms = input("");
        with_terms.source_files.push((
            "assets/localization/en/terms.ftl".into(),
            "-brand = Regia\n-studio = Example\nabout = Made with { -brand }\n".into(),
        ));
        with_terms
            .source_files
            .push(("assets/localization/en/zz.ftl".into(), "-brand = Regia\n".into()));
        let out = export_po(&with_terms);
        assert_eq!(out.skipped_terms, 2, "distinct terms across files");
        assert!(out.file.entries.iter().all(|e| !e.ctxt.starts_with('-')));
        assert!(out.file.entries.iter().any(|e| e.ctxt == "about"));
        assert_eq!(export_po(&input("")).skipped_terms, 0);
    }

    #[test]
    fn message_blocks_skip_terms_and_the_bundle_scanner_keeps_them() {
        let text = "-brand = Regia\nhello = Hello { -brand }\n";
        assert!(ftl_blocks(text).iter().all(|b| b.key != "-brand"));
        let blocks = ftl_blocks_with_terms(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].key, "-brand");
        assert_eq!(blocks[0].raw, "-brand = Regia");
        assert_eq!(blocks[1].pattern, "Hello { -brand }");
    }

    #[test]
    fn the_context_is_every_comment_above_that_is_not_a_tool_marker() {
        let text = "## group\n\n# source: scenes/menu.ron:12\n# Shown in the pause menu\n# note: keep it short\n# TODO translate\n# regia-source: 00ff\n#\nquit = Quit\n";
        let blocks = ftl_blocks(text);
        assert_eq!(blocks[0].context, ["Shown in the pause menu", "keep it short"]);
        // The `# note:` lines stay what `notes` reads.
        assert_eq!(blocks[0].notes, ["keep it short"]);
        assert!(is_tool_comment("## group") && is_tool_comment("# source: a.ron:1"));
        assert!(!is_tool_comment("# a hand-written comment"));
        // Exports carry the plain comment as an extracted note, like a `# note:` one.
        let mut export = input("");
        export.source_files = vec![("assets/localization/en/main.ftl".into(), text.into())];
        let out = export_po(&export);
        let entry = out.file.entries.iter().find(|e| e.ctxt == "quit").expect("quit");
        assert_eq!(
            entry.comments,
            ["note: Shown in the pause menu", "note: keep it short"]
        );
    }
}
