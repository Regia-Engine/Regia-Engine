// SPDX-License-Identifier: MPL-2.0
//! Translator exchange through XLIFF 2.0 files (spec 36 § 3, ADR-2550).
//!
//! XLIFF is the hand-off most translation vendors and CAT tools take. This module is pure (no file
//! IO) and sits beside [`crate::loc_po`]: [`write_xliff`] turns the same [`PoFile`] the PO exporter
//! builds into XLIFF text (one `<unit>` per message, same entry layout, same stale and fuzzy
//! rules), and [`read_xliff`] turns a returned file back into a [`PoFile`] so the PO import path
//! (`plan_import`, with its translation checks) decides what becomes `imported.ftl`. The two
//! formats therefore import to identical bytes.
//!
//! # Placeholder protection
//!
//! Fluent placeables (`{ $name }`, `{ -brand }`, `{ NUMBER($n) }`) become inline `<ph>` codes with
//! `canDelete="no"` and an `equiv` attribute carrying the exact source text, so a translator tool
//! shows them as opaque tokens and can reorder but not edit them. The structure of a select
//! (`{ $n ->`, each `[one]` / `*[other]` key, the closing brace) is also a `<ph>`, with
//! `canDelete="yes"`: a language with other plural categories may add or drop variants, and the
//! translation checks (`loc_plural_missing`, `loc_variable_mismatch`) judge the result. On import
//! each code is replaced by the source's `equiv` text for its `id` (the file's own `equiv` of a
//! target code is ignored), a protected code missing from the target skips the unit with
//! `loc_xliff_placeholder_missing`, and a code id the source never had skips it with
//! `loc_xliff_unknown_code`.
//!
//! # Reading untrusted files
//!
//! The XML reader is strict and small: no DTD (`loc_xliff_doctype`), no custom entities, limits on
//! size, depth, node count and attributes ([`XliffLimits`]). The XLIFF 2.0 format is public
//! (OASIS); the reader and writer here are original code.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::loc_po::{EXPORT_VERSION, ImportContext, ImportNote, ImportPlan, PoEntry, PoFile, plan_import};

/// The XLIFF 2.0 namespace (also used by 2.1).
pub const XLIFF_NS: &str = "urn:oasis:names:tc:xliff:document:2.0";
/// The namespace of the `rg:` extension attributes (`rg:fuzzy`, `rg:export-version`).
pub const REGIA_NS: &str = "urn:regia:xliff:1";

/// Limits applied while reading a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XliffLimits {
    /// Largest accepted input, in bytes.
    pub max_bytes: usize,
    /// Deepest element nesting.
    pub max_depth: usize,
    /// Most elements and text nodes in one file.
    pub max_nodes: usize,
    /// Most attributes on one element.
    pub max_attributes: usize,
    /// Longest single attribute value or text node, in bytes.
    pub max_text: usize,
}

impl Default for XliffLimits {
    fn default() -> Self {
        Self {
            max_bytes: 32 * 1024 * 1024,
            max_depth: 24,
            max_nodes: 2_000_000,
            max_attributes: 32,
            max_text: 1024 * 1024,
        }
    }
}

/// An XLIFF file that does not write or read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("XLIFF line {line}: {code}: {message}")]
pub struct XliffError {
    /// Stable code (`loc_xliff_syntax`, `loc_xliff_doctype`, `loc_xliff_limit`, `loc_xliff_version`, ...).
    pub code: &'static str,
    /// 1-based line (0 when not tied to a line).
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

fn err(code: &'static str, line: usize, message: impl Into<String>) -> XliffError {
    XliffError {
        code,
        line,
        message: message.into(),
    }
}

// ---------------------------------------------------------------------------------------------
// Inline codes: splitting a Fluent pattern into text and `<ph>` codes.
// ---------------------------------------------------------------------------------------------

/// One piece of a pattern: translatable text or an opaque code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Plain text.
    Text(String),
    /// A code standing for `equiv` verbatim; `protected` codes must stay in a translation.
    Code {
        /// The Fluent source the code stands for.
        equiv: String,
        /// Whether a translator may not delete it (a placeable, as opposed to select structure).
        protected: bool,
    },
}

/// The position of the `}` that closes the `{` at `open`, skipping nested braces and quoted
/// string literals; `None` when unbalanced before `end`.
fn matching_close(chars: &[char], open: usize, end: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    let mut in_string = false;
    while i < end {
        let c = chars[i];
        if in_string {
            if c == '\\' {
                i += 1;
            } else if c == '"' {
                in_string = false;
            }
        } else {
            match c {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// The position of the first top-level `->` in `chars[from..to]`.
fn top_level_arrow(chars: &[char], from: usize, to: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut i = from;
    while i < to {
        let c = chars[i];
        if in_string {
            if c == '\\' {
                i += 1;
            } else if c == '"' {
                in_string = false;
            }
        } else {
            match c {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => depth = depth.saturating_sub(1),
                '-' if depth == 0 && chars.get(i + 1) == Some(&'>') && i + 1 < to => return Some(i),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Whether a variant key (`[one]` or `*[other]`) starts at `chars[i..]` after optional whitespace.
fn marker_at(chars: &[char], mut i: usize, end: usize) -> Option<usize> {
    while i < end && chars[i].is_whitespace() {
        i += 1;
    }
    if i < end && chars[i] == '*' && chars.get(i + 1) == Some(&'[') {
        return Some(i);
    }
    if i < end && chars[i] == '[' {
        return Some(i);
    }
    None
}

fn rest_blank(chars: &[char], i: usize, end: usize) -> bool {
    chars[i..end].iter().all(|c| c.is_whitespace())
}

fn push_text(out: &mut Vec<Piece>, text: &mut String) {
    if !text.is_empty() {
        out.push(Piece::Text(std::mem::take(text)));
    }
}

/// Reads `chars[i..end]` as pattern text (placeables become codes); inside a select variant it
/// stops where the next variant key or the closing whitespace starts. Returns the stop position.
fn read_pattern(chars: &[char], mut i: usize, end: usize, in_variant: bool, out: &mut Vec<Piece>) -> usize {
    let mut text = String::new();
    while i < end {
        let c = chars[i];
        if in_variant && c == '\n' && (marker_at(chars, i + 1, end).is_some() || rest_blank(chars, i, end)) {
            break;
        }
        if c == '{' {
            if let Some(close) = matching_close(chars, i, end) {
                push_text(out, &mut text);
                read_placeable(chars, i, close, out);
                i = close + 1;
                continue;
            }
            text.extend(&chars[i..end]);
            i = end;
            break;
        }
        text.push(c);
        i += 1;
    }
    push_text(out, &mut text);
    i
}

fn read_placeable(chars: &[char], open: usize, close: usize, out: &mut Vec<Piece>) {
    let whole = || Piece::Code {
        equiv: chars[open..=close].iter().collect(),
        protected: true,
    };
    let Some(arrow) = top_level_arrow(chars, open + 1, close) else {
        out.push(whole());
        return;
    };
    let mut local = vec![Piece::Code {
        equiv: chars[open..arrow + 2].iter().collect(),
        protected: false,
    }];
    let mut j = arrow + 2;
    let mut variants = 0usize;
    while j < close {
        let Some(k) = marker_at(chars, j, close) else {
            break;
        };
        let Some(bracket) = chars[k..close].iter().position(|&c| c == ']').map(|p| k + p) else {
            out.push(whole());
            return;
        };
        let mut after = bracket + 1;
        while after < close && chars[after].is_whitespace() {
            after += 1;
        }
        local.push(Piece::Code {
            equiv: chars[j..after].iter().collect(),
            protected: false,
        });
        j = read_pattern(chars, after, close, true, &mut local);
        variants += 1;
    }
    if variants == 0 {
        out.push(whole());
        return;
    }
    local.push(Piece::Code {
        equiv: chars[j..=close].iter().collect(),
        protected: false,
    });
    out.extend(local);
}

/// Splits a Fluent pattern into text and codes. Concatenating the text and every code's `equiv`
/// gives `pattern` back exactly.
#[must_use]
pub fn split_pattern(pattern: &str) -> Vec<Piece> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = Vec::new();
    read_pattern(&chars, 0, chars.len(), false, &mut out);
    out
}

// ---------------------------------------------------------------------------------------------
// Writing.
// ---------------------------------------------------------------------------------------------

fn xml_char_ok(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

fn escape_text(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
}

fn escape_attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
    out
}

fn write_content(pattern: &str, out: &mut String) {
    let mut id = 0usize;
    for piece in split_pattern(pattern) {
        match piece {
            Piece::Text(t) => escape_text(&t, out),
            Piece::Code { equiv, protected } => {
                id += 1;
                let disp = if protected {
                    format!(" disp=\"{}\"", escape_attr(&display_of(&equiv)))
                } else {
                    String::new()
                };
                out.push_str(&format!(
                    "<ph id=\"{id}\" canDelete=\"{}\" equiv=\"{}\"{disp}/>",
                    if protected { "no" } else { "yes" },
                    escape_attr(&equiv)
                ));
            }
        }
    }
}

/// What a tool shows for a protected placeable: the inside of the braces, trimmed.
fn display_of(equiv: &str) -> String {
    let inner = equiv.trim_start_matches('{').trim_end_matches('}').trim();
    inner.to_owned()
}

/// Write `file` (as [`crate::loc_po::export_po`] builds it) as XLIFF 2.0 text. Deterministic: same
/// order as the PO file, UTF-8, `\n` newlines.
///
/// # Errors
///
/// A message holds a character XML 1.0 cannot carry (`loc_xliff_bad_char`), or the file has no
/// `Language` header.
pub fn write_xliff(file: &PoFile) -> Result<String, XliffError> {
    let _span = tracing::debug_span!("loc_write_xliff", entries = file.entries.len()).entered();
    let Some(target) = file.header_value("Language") else {
        return Err(err("loc_xliff_bad_header", 0, "the file has no Language"));
    };
    let source = file.header_value("X-Regia-Source-Locale").unwrap_or("en");
    let project = file.header_value("Project-Id-Version").unwrap_or("");
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<xliff xmlns=\"{XLIFF_NS}\" xmlns:rg=\"{REGIA_NS}\" version=\"2.0\" srcLang=\"{}\" trgLang=\"{}\" rg:export-version=\"{EXPORT_VERSION}\">\n",
        escape_attr(source),
        escape_attr(target)
    ));
    out.push_str(&format!(
        "  <file id=\"messages\" original=\"{}\">\n",
        escape_attr(project)
    ));
    for entry in &file.entries {
        for text in [&entry.ctxt, &entry.msgid, &entry.msgstr] {
            if let Some(c) = text.chars().find(|c| !xml_char_ok(*c)) {
                return Err(err(
                    "loc_xliff_bad_char",
                    0,
                    format!(
                        "`{}` holds U+{:04X}, which XML 1.0 cannot carry",
                        entry.ctxt,
                        u32::from(c)
                    ),
                ));
            }
        }
        let review = entry
            .flags
            .iter()
            .any(|f| f == crate::loc_glossary::TM_REVIEW_FLAG);
        // A translation-memory pre-fill is an unconfirmed target. XLIFF says so with the segment
        // state, which a CAT tool moves past `initial` when the translator confirms it; an
        // `rg:fuzzy` would survive that confirmation, so it is not written for one (ADR-5577).
        let fuzzy = !review && entry.flags.iter().any(|f| f == "fuzzy");
        out.push_str(&format!(
            "    <unit id=\"{0}\" name=\"{0}\"{1}{2}>\n",
            escape_attr(&entry.ctxt),
            if fuzzy { " rg:fuzzy=\"yes\"" } else { "" },
            if review { " rg:review=\"yes\"" } else { "" }
        ));
        if !entry.comments.is_empty() || !entry.refs.is_empty() {
            out.push_str("      <notes>\n");
            for r in &entry.refs {
                out.push_str(&format!(
                    "        <note category=\"location\">{}</note>\n",
                    escape_text_string(r)
                ));
            }
            for c in &entry.comments {
                out.push_str(&format!(
                    "        <note category=\"regia\">{}</note>\n",
                    escape_text_string(c)
                ));
            }
            out.push_str("      </notes>\n");
        }
        let state = if entry.msgstr.trim().is_empty() || review {
            "initial"
        } else {
            "translated"
        };
        out.push_str(&format!(
            "      <segment state=\"{state}\">\n        <source xml:space=\"preserve\">"
        ));
        write_content(&entry.msgid, &mut out);
        out.push_str("</source>\n");
        if entry.msgstr.is_empty() {
            out.push_str("        <target xml:space=\"preserve\"></target>\n");
        } else {
            out.push_str("        <target xml:space=\"preserve\">");
            write_target_content(&entry.msgid, &entry.msgstr, &mut out);
            out.push_str("</target>\n");
        }
        out.push_str("      </segment>\n    </unit>\n");
    }
    out.push_str("  </file>\n</xliff>\n");
    Ok(out)
}

fn escape_text_string(text: &str) -> String {
    let mut out = String::new();
    escape_text(text, &mut out);
    out
}

/// A target pattern, with its codes numbered like the source's: a piece whose `equiv` is one of the source's
/// codes reuses that id. Any other piece (a plural category the source lacks, say) is written as plain text,
/// which reads back unchanged, so a re-import never meets a code id the source does not have.
fn write_target_content(source: &str, target: &str, out: &mut String) {
    let mut ids: BTreeMap<String, (usize, bool)> = BTreeMap::new();
    let mut next = 0usize;
    for piece in split_pattern(source) {
        if let Piece::Code { equiv, protected } = piece {
            next += 1;
            ids.entry(equiv).or_insert((next, protected));
        }
    }
    for piece in split_pattern(target) {
        match piece {
            Piece::Text(t) => escape_text(&t, out),
            Piece::Code { equiv, .. } => match ids.get(&equiv) {
                Some(&(id, protected)) => {
                    let disp = if protected {
                        format!(" disp=\"{}\"", escape_attr(&display_of(&equiv)))
                    } else {
                        String::new()
                    };
                    out.push_str(&format!(
                        "<ph id=\"{id}\" canDelete=\"{}\" equiv=\"{}\"{disp}/>",
                        if protected { "no" } else { "yes" },
                        escape_attr(&equiv)
                    ));
                }
                None => escape_text(&equiv, out),
            },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A strict, small XML reader.
// ---------------------------------------------------------------------------------------------

/// A parsed attribute: namespace URI, local name, value.
type Attr = (String, String, String);

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Elem(Element),
}

#[derive(Debug, Clone)]
struct Element {
    ns: String,
    local: String,
    attrs: Vec<Attr>,
    children: Vec<Node>,
    line: usize,
}

impl Element {
    fn attr(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(ns, l, _)| ns.is_empty() && l == local)
            .map(|(_, _, v)| v.as_str())
    }

    fn ext_attr(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(ns, l, _)| ns == REGIA_NS && l == local)
            .map(|(_, _, v)| v.as_str())
    }

    fn elements<'a>(&'a self, local: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter_map(move |n| match n {
            Node::Elem(e) if e.ns == XLIFF_NS && e.local == local => Some(e),
            _ => None,
        })
    }
}

struct Reader<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    limits: XliffLimits,
    nodes: usize,
}

impl Reader<'_> {
    fn line(&self) -> usize {
        self.bytes[..self.pos.min(self.bytes.len())]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1
    }

    fn fail(&self, code: &'static str, message: impl Into<String>) -> XliffError {
        err(code, self.line(), message)
    }

    fn syntax(&self, message: impl Into<String>) -> XliffError {
        self.fail("loc_xliff_syntax", message)
    }

    fn starts(&self, s: &str) -> bool {
        self.bytes[self.pos..].starts_with(s.as_bytes())
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn count(&mut self) -> Result<(), XliffError> {
        self.nodes += 1;
        if self.nodes > self.limits.max_nodes {
            return Err(self.fail("loc_xliff_limit", "too many nodes"));
        }
        Ok(())
    }

    fn name(&mut self) -> Result<String, XliffError> {
        let start = self.pos;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':') || b >= 0x80 {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.syntax("a name was expected"));
        }
        Ok(self.src[start..self.pos].to_owned())
    }

    /// Decode entity and character references in `raw`.
    fn decode(&self, raw: &str, attribute: bool) -> Result<String, XliffError> {
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(at) = rest.find(['&', '\r', '\n', '\t']) {
            out.push_str(&rest[..at]);
            let c = rest.as_bytes()[at];
            if c == b'&' {
                let tail = &rest[at + 1..];
                let Some(semi) = tail.find(';') else {
                    return Err(self.syntax("an `&` without `;`"));
                };
                let entity = &tail[..semi];
                match entity {
                    "amp" => out.push('&'),
                    "lt" => out.push('<'),
                    "gt" => out.push('>'),
                    "quot" => out.push('"'),
                    "apos" => out.push('\''),
                    _ => {
                        let code = if let Some(hex) = entity.strip_prefix("#x") {
                            u32::from_str_radix(hex, 16).ok()
                        } else if let Some(dec) = entity.strip_prefix('#') {
                            dec.parse::<u32>().ok()
                        } else {
                            return Err(self.fail(
                                "loc_xliff_entity",
                                format!("the entity `&{entity};` is not one of the five predefined ones"),
                            ));
                        };
                        match code.and_then(char::from_u32).filter(|c| xml_char_ok(*c)) {
                            Some(ch) => out.push(ch),
                            None => return Err(self.syntax(format!("`&{entity};` is not a valid character"))),
                        }
                    }
                }
                rest = &tail[semi + 1..];
                continue;
            }
            // Literal whitespace: newlines normalize to `\n` in text and to a space in attributes.
            let mut skip = 1;
            if c == b'\r' && rest.as_bytes().get(at + 1) == Some(&b'\n') {
                skip = 2;
            }
            if attribute {
                out.push(' ');
            } else if c == b'\t' {
                out.push('\t');
            } else {
                out.push('\n');
            }
            rest = &rest[at + skip..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn skip_misc(&mut self) -> Result<(), XliffError> {
        loop {
            self.skip_ws();
            if self.starts("<!--") {
                self.skip_comment()?;
            } else if self.starts("<?") {
                match self.src[self.pos..].find("?>") {
                    Some(end) => self.pos += end + 2,
                    None => return Err(self.syntax("an unterminated processing instruction")),
                }
            } else if self.starts("<!DOCTYPE") || self.starts("<!doctype") {
                return Err(self.fail(
                    "loc_xliff_doctype",
                    "a DOCTYPE is not accepted (no DTDs or custom entities)",
                ));
            } else {
                return Ok(());
            }
        }
    }

    fn skip_comment(&mut self) -> Result<(), XliffError> {
        match self.src[self.pos + 4..].find("-->") {
            Some(end) => {
                self.pos += 4 + end + 3;
                Ok(())
            }
            None => Err(self.syntax("an unterminated comment")),
        }
    }

    fn document(&mut self) -> Result<Element, XliffError> {
        if self.src.starts_with('\u{FEFF}') {
            self.pos = '\u{FEFF}'.len_utf8();
        }
        self.skip_misc()?;
        if !self.starts("<") {
            return Err(self.syntax("the document has no root element"));
        }
        let mut scope: Vec<(String, String)> = Vec::new();
        let root = self.element(&mut scope, 1)?;
        self.skip_misc()?;
        if self.pos < self.bytes.len() {
            return Err(self.syntax("text after the root element"));
        }
        Ok(root)
    }

    fn element(&mut self, scope: &mut Vec<(String, String)>, depth: usize) -> Result<Element, XliffError> {
        if depth > self.limits.max_depth {
            return Err(self.fail("loc_xliff_limit", "elements nest too deeply"));
        }
        self.count()?;
        let src = self.src;
        let line = self.line();
        self.pos += 1; // `<`
        let qname = self.name()?;
        let mut raw_attrs: Vec<(String, String)> = Vec::new();
        let self_closing;
        loop {
            self.skip_ws();
            if self.starts("/>") {
                self.pos += 2;
                self_closing = true;
                break;
            }
            if self.starts(">") {
                self.pos += 1;
                self_closing = false;
                break;
            }
            let name = self.name()?;
            self.skip_ws();
            if !self.starts("=") {
                return Err(self.syntax(format!("attribute `{name}` has no value")));
            }
            self.pos += 1;
            self.skip_ws();
            let quote = match self.bytes.get(self.pos) {
                Some(&q @ (b'"' | b'\'')) => q,
                _ => return Err(self.syntax(format!("attribute `{name}` is not quoted"))),
            };
            self.pos += 1;
            let start = self.pos;
            while self.pos < self.bytes.len() && self.bytes[self.pos] != quote {
                if self.bytes[self.pos] == b'<' {
                    return Err(self.syntax("`<` in an attribute value"));
                }
                self.pos += 1;
            }
            if self.pos >= self.bytes.len() {
                return Err(self.syntax("an unterminated attribute value"));
            }
            let raw = &src[start..self.pos];
            self.pos += 1;
            if raw.len() > self.limits.max_text {
                return Err(self.fail("loc_xliff_limit", "an attribute value is too long"));
            }
            if raw_attrs.iter().any(|(n, _)| *n == name) {
                return Err(self.syntax(format!("attribute `{name}` is given twice")));
            }
            if raw_attrs.len() >= self.limits.max_attributes {
                return Err(self.fail("loc_xliff_limit", "too many attributes on one element"));
            }
            raw_attrs.push((name, self.decode(raw, true)?));
        }
        // Namespace declarations first.
        let scope_mark = scope.len();
        for (name, value) in &raw_attrs {
            if name == "xmlns" {
                scope.push((String::new(), value.clone()));
            } else if let Some(prefix) = name.strip_prefix("xmlns:") {
                scope.push((prefix.to_owned(), value.clone()));
            }
        }
        let resolve = |scope: &[(String, String)], prefix: &str| -> Option<String> {
            scope
                .iter()
                .rev()
                .find(|(p, _)| p == prefix)
                .map(|(_, uri)| uri.clone())
        };
        let (prefix, local) = qname.split_once(':').unwrap_or(("", qname.as_str()));
        let ns = if prefix == "xml" {
            "http://www.w3.org/XML/1998/namespace".to_owned()
        } else {
            resolve(&scope[..], prefix).unwrap_or_default()
        };
        let mut attrs = Vec::new();
        for (name, value) in raw_attrs {
            if name == "xmlns" || name.starts_with("xmlns:") {
                continue;
            }
            let (p, l) = name.split_once(':').unwrap_or(("", name.as_str()));
            let ans = if p.is_empty() {
                String::new()
            } else if p == "xml" {
                "http://www.w3.org/XML/1998/namespace".to_owned()
            } else {
                match resolve(&scope[..], p) {
                    Some(uri) => uri,
                    None => return Err(self.syntax(format!("the prefix `{p}` is not declared"))),
                }
            };
            attrs.push((ans, l.to_owned(), value));
        }
        let mut element = Element {
            ns,
            local: local.to_owned(),
            attrs,
            children: Vec::new(),
            line,
        };
        if !self_closing {
            self.content(&mut element, &qname, scope, depth)?;
        }
        scope.truncate(scope_mark);
        Ok(element)
    }

    fn content(
        &mut self,
        element: &mut Element,
        qname: &str,
        scope: &mut Vec<(String, String)>,
        depth: usize,
    ) -> Result<(), XliffError> {
        let src = self.src;
        loop {
            if self.pos >= self.bytes.len() {
                return Err(self.syntax(format!("`<{qname}>` is not closed")));
            }
            if self.starts("</") {
                self.pos += 2;
                let close = self.name()?;
                self.skip_ws();
                if !self.starts(">") || close != qname {
                    return Err(self.syntax(format!("`</{close}>` does not close `<{qname}>`")));
                }
                self.pos += 1;
                return Ok(());
            }
            if self.starts("<!--") {
                self.skip_comment()?;
            } else if self.starts("<![CDATA[") {
                let start = self.pos + 9;
                let Some(end) = src[start..].find("]]>") else {
                    return Err(self.syntax("an unterminated CDATA section"));
                };
                self.count()?;
                let text = &src[start..start + end];
                if text.len() > self.limits.max_text {
                    return Err(self.fail("loc_xliff_limit", "a text node is too long"));
                }
                element.children.push(Node::Text(text.to_owned()));
                self.pos = start + end + 3;
            } else if self.starts("<?") {
                match self.src[self.pos..].find("?>") {
                    Some(end) => self.pos += end + 2,
                    None => return Err(self.syntax("an unterminated processing instruction")),
                }
            } else if self.starts("<!") {
                return Err(self.fail(
                    "loc_xliff_doctype",
                    "a declaration inside the document is not accepted",
                ));
            } else if self.starts("<") {
                let child = self.element(scope, depth + 1)?;
                element.children.push(Node::Elem(child));
            } else {
                let start = self.pos;
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'<' {
                    self.pos += 1;
                }
                let raw = &src[start..self.pos];
                if raw.len() > self.limits.max_text {
                    return Err(self.fail("loc_xliff_limit", "a text node is too long"));
                }
                self.count()?;
                let text = self.decode(raw, false)?;
                element.children.push(Node::Text(text));
            }
        }
    }
}

fn parse_xml(text: &str, limits: XliffLimits) -> Result<Element, XliffError> {
    if text.len() > limits.max_bytes {
        return Err(err(
            "loc_xliff_limit",
            0,
            format!(
                "the file is {} bytes; the limit is {}",
                text.len(),
                limits.max_bytes
            ),
        ));
    }
    let mut reader = Reader {
        src: text,
        bytes: text.as_bytes(),
        pos: 0,
        limits,
        nodes: 0,
    };
    reader.document()
}

// ---------------------------------------------------------------------------------------------
// Interpreting an XLIFF document.
// ---------------------------------------------------------------------------------------------

/// What [`read_xliff`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XliffImport {
    /// The units that read cleanly, as the PO entries the PO import path decides on.
    pub po: PoFile,
    /// Units refused while reading (placeholder and inline-code problems).
    pub skipped: Vec<ImportNote>,
}

fn note(code: &str, key: &str, message: impl Into<String>) -> ImportNote {
    ImportNote {
        code: code.to_owned(),
        key: key.to_owned(),
        message: message.into(),
    }
}

/// A source code: its id, the Fluent text it stands for, and whether it must stay.
#[derive(Debug, Clone)]
struct SourceCode {
    equiv: String,
    protected: bool,
}

/// Why a segment's content did not read.
enum ContentError {
    /// An inline element the importer does not take.
    Unsupported(String),
    /// A `<ph>` without an id (or, in the source, without `equiv`).
    Malformed(String),
}

/// The text of a `<source>` or `<target>` with its codes resolved against `codes` (for a source,
/// `codes` is filled in as the codes are found). Returns the text and the ids of codes used.
fn content_text(
    element: &Element,
    codes: &mut BTreeMap<String, SourceCode>,
    is_source: bool,
    used: &mut BTreeSet<String>,
    unknown: &mut Vec<String>,
) -> Result<String, ContentError> {
    let mut out = String::new();
    for node in &element.children {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Elem(e) if e.ns == XLIFF_NS => match e.local.as_str() {
                "ph" => {
                    let Some(id) = e.attr("id") else {
                        return Err(ContentError::Malformed("a <ph> has no id".to_owned()));
                    };
                    if is_source {
                        let Some(equiv) = e.attr("equiv") else {
                            return Err(ContentError::Malformed(format!("source code {id} has no equiv")));
                        };
                        codes.entry(id.to_owned()).or_insert_with(|| SourceCode {
                            equiv: equiv.to_owned(),
                            protected: e.attr("canDelete") == Some("no"),
                        });
                        out.push_str(equiv);
                    } else if let Some(code) = codes.get(id) {
                        used.insert(id.to_owned());
                        out.push_str(&code.equiv);
                    } else {
                        unknown.push(id.to_owned());
                    }
                }
                "mrk" | "sm" | "em" => {
                    out.push_str(&content_text(e, codes, is_source, used, unknown)?);
                }
                "cp" => {
                    let ch = e
                        .attr("hex")
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .and_then(char::from_u32);
                    match ch {
                        Some(c) => out.push(c),
                        None => return Err(ContentError::Malformed("a <cp> has no valid hex".to_owned())),
                    }
                }
                other => return Err(ContentError::Unsupported(other.to_owned())),
            },
            Node::Elem(e) => {
                return Err(ContentError::Unsupported(e.local.clone()));
            }
        }
    }
    Ok(out)
}

fn collect_units<'a>(parent: &'a Element, out: &mut Vec<&'a Element>) {
    for child in &parent.children {
        if let Node::Elem(e) = child
            && e.ns == XLIFF_NS
        {
            match e.local.as_str() {
                "unit" => out.push(e),
                "group" => collect_units(e, out),
                _ => {}
            }
        }
    }
}

fn read_unit(unit: &Element, po: &mut PoFile, skipped: &mut Vec<ImportNote>) {
    let key = unit.attr("name").or_else(|| unit.attr("id")).unwrap_or("");
    if key.is_empty() {
        skipped.push(note(
            "loc_xliff_bad_unit",
            "",
            format!("the <unit> at line {} has no id", unit.line),
        ));
        return;
    }
    let mut codes: BTreeMap<String, SourceCode> = BTreeMap::new();
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut unknown: Vec<String> = Vec::new();
    let mut source = String::new();
    let mut target = String::new();
    let mut with_target = 0usize;
    let mut parts = 0usize;
    // First every source (so the code table is complete), then every target.
    let mut parts_list: Vec<&Element> = Vec::new();
    for child in &unit.children {
        if let Node::Elem(e) = child
            && e.ns == XLIFF_NS
            && (e.local == "segment" || e.local == "ignorable")
        {
            parts_list.push(e);
        }
    }
    for part in &parts_list {
        for src in part.elements("source") {
            match content_text(src, &mut codes, true, &mut used, &mut unknown) {
                Ok(t) => source.push_str(&t),
                Err(ContentError::Unsupported(what)) => {
                    skipped.push(note(
                        "loc_xliff_unsupported_inline",
                        key,
                        format!("the source holds a <{what}> element the importer does not take"),
                    ));
                    return;
                }
                Err(ContentError::Malformed(why)) => {
                    skipped.push(note("loc_xliff_bad_unit", key, why));
                    return;
                }
            }
        }
    }
    for part in &parts_list {
        parts += 1;
        let mut found = false;
        for tgt in part.elements("target") {
            found = true;
            match content_text(tgt, &mut codes, false, &mut used, &mut unknown) {
                Ok(t) => target.push_str(&t),
                Err(ContentError::Unsupported(what)) => {
                    skipped.push(note(
                        "loc_xliff_unsupported_inline",
                        key,
                        format!("the translation holds a <{what}> element the importer does not take"),
                    ));
                    return;
                }
                Err(ContentError::Malformed(why)) => {
                    skipped.push(note("loc_xliff_bad_unit", key, why));
                    return;
                }
            }
        }
        if found {
            with_target += 1;
        }
    }
    if with_target > 0 && with_target < parts && !target.trim().is_empty() {
        skipped.push(note(
            "loc_xliff_incomplete_segment",
            key,
            "some segments of the unit have a translation and some do not",
        ));
        return;
    }
    if !unknown.is_empty() {
        skipped.push(note(
            "loc_xliff_unknown_code",
            key,
            format!(
                "the translation uses inline code(s) {} that the source does not have",
                unknown.join(", ")
            ),
        ));
        return;
    }
    if !target.trim().is_empty() {
        // A protected code counts as kept when a code with the same source text is used: the writer
        // gives a repeated placeable one id, so the first occurrence's id stands for all of them.
        let used_equivs: BTreeSet<&str> = used
            .iter()
            .filter_map(|id| codes.get(id).map(|c| c.equiv.as_str()))
            .collect();
        let missing: Vec<&String> = codes
            .iter()
            .filter(|(id, c)| c.protected && !used.contains(*id) && !used_equivs.contains(c.equiv.as_str()))
            .map(|(id, _)| id)
            .collect();
        if !missing.is_empty() {
            let shown: Vec<String> = missing
                .iter()
                .filter_map(|id| codes.get(*id).map(|c| c.equiv.clone()))
                .collect();
            skipped.push(note(
                "loc_xliff_placeholder_missing",
                key,
                format!(
                    "the translation dropped the protected placeholder(s) {}",
                    shown.join(", ")
                ),
            ));
            return;
        }
    }
    let mut flags = vec!["fluent-format".to_owned()];
    if unit.ext_attr("fuzzy") == Some("yes") {
        flags.push("fuzzy".to_owned());
    }
    if unit.ext_attr("review") == Some("yes") {
        // A pre-fill is confirmed once every segment left `initial` (XLIFF 2.0's default state);
        // until then it imports as fuzzy, so an unreviewed suggestion is not a translation.
        let confirmed = parts_list
            .iter()
            .filter(|p| p.local == "segment")
            .all(|p| p.attr("state").is_some_and(|s| s != "initial"));
        if !confirmed && !flags.iter().any(|f| f == "fuzzy") {
            flags.push("fuzzy".to_owned());
        }
        flags.push(crate::loc_glossary::TM_REVIEW_FLAG.to_owned());
    }
    po.entries.push(PoEntry {
        ctxt: key.to_owned(),
        msgid: source,
        msgstr: target,
        comments: Vec::new(),
        refs: Vec::new(),
        flags,
    });
}

/// Read a returned XLIFF 2.0 file (spec 36 § 3). The header carries `Language` (from `trgLang`) and
/// `X-Regia-Source-Locale` (from `srcLang`); units that cannot be read are reported in
/// [`XliffImport::skipped`] with their codes.
///
/// # Errors
///
/// The text is not well-formed XML within `limits`, has a DOCTYPE, is not XLIFF 2.x, or lacks
/// `trgLang`.
pub fn read_xliff_with(text: &str, limits: XliffLimits) -> Result<XliffImport, XliffError> {
    let _span = tracing::debug_span!("loc_read_xliff", bytes = text.len()).entered();
    let root = parse_xml(text, limits)?;
    if root.local != "xliff" {
        return Err(err(
            "loc_xliff_syntax",
            root.line,
            "the root element is not <xliff>",
        ));
    }
    if root.ns != XLIFF_NS {
        let hint = if root.ns.contains("xliff:document:1") {
            " (XLIFF 1.x; Regia reads XLIFF 2.0)"
        } else {
            ""
        };
        return Err(err(
            "loc_xliff_version",
            root.line,
            format!("the namespace `{}` is not XLIFF 2.0{hint}", root.ns),
        ));
    }
    if !root.attr("version").is_some_and(|v| v.starts_with("2.")) {
        return Err(err("loc_xliff_version", root.line, "the version is not 2.x"));
    }
    let Some(target) = root.attr("trgLang") else {
        return Err(err("loc_xliff_bad_header", root.line, "the file has no trgLang"));
    };
    let header = vec![
        ("Language".to_owned(), target.to_owned()),
        ("Content-Type".to_owned(), "text/plain; charset=UTF-8".to_owned()),
        (
            "X-Regia-Source-Locale".to_owned(),
            root.attr("srcLang").unwrap_or("").to_owned(),
        ),
        ("X-Regia-Export-Version".to_owned(), EXPORT_VERSION.to_owned()),
    ];
    let mut import = XliffImport {
        po: PoFile {
            header,
            entries: Vec::new(),
        },
        skipped: Vec::new(),
    };
    for file in root.elements("file") {
        let mut units = Vec::new();
        collect_units(file, &mut units);
        for unit in units {
            read_unit(unit, &mut import.po, &mut import.skipped);
        }
    }
    Ok(import)
}

/// [`read_xliff_with`] under the default [`XliffLimits`].
///
/// # Errors
///
/// As [`read_xliff_with`].
pub fn read_xliff(text: &str) -> Result<XliffImport, XliffError> {
    read_xliff_with(text, XliffLimits::default())
}

/// Decide the import of a read XLIFF file: the units refused while reading first, then the PO
/// import path's checks on the rest ([`plan_import`]).
#[must_use]
pub fn plan_import_xliff(import: &XliffImport, ctx: &ImportContext) -> ImportPlan {
    let mut plan = plan_import(&import.po, ctx);
    if plan.report.refused.is_none() {
        let mut skipped = import.skipped.clone();
        skipped.append(&mut plan.report.skipped);
        plan.report.skipped = skipped;
    }
    plan
}

/// Whether `text` looks like an XLIFF file rather than PO (first non-blank character is `<`).
#[must_use]
pub fn looks_like_xliff(text: &str) -> bool {
    text.trim_start_matches('\u{FEFF}').trim_start().starts_with('<')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loc_po::{ExportFilter, ExportInput, export_po};

    const SOURCE: &str = "\
# note: shown in the pause menu
menu-quit = Quit
hud-ammo = { $count ->
    [one] { $count } round
   *[other] { $count } rounds
    }
greet = Hello, { $name }!
    .title = Greeting
tags = a < b & \"c\"
";

    fn input() -> ExportInput {
        ExportInput {
            project: "demo 1.0".into(),
            source: "en".into(),
            locale: "fr".into(),
            source_files: vec![("assets/localization/en/main.ftl".into(), SOURCE.into())],
            target_files: Vec::new(),
            references: BTreeMap::from([("menu-quit".into(), vec!["scenes/menu.ron:12".into()])]),
            filter: ExportFilter::All,
        }
    }

    fn ctx() -> ImportContext {
        ImportContext {
            locale: "fr".into(),
            source: "en".into(),
            source_files: vec![("assets/localization/en/main.ftl".into(), SOURCE.into())],
            ..ImportContext::default()
        }
    }

    fn exported() -> String {
        write_xliff(&export_po(&input()).file).expect("writes")
    }

    /// Fill the target of `key` in exported XLIFF text with `inner` (raw XML).
    fn fill(xlf: &str, key: &str, inner: &str) -> String {
        let marker = format!("<unit id=\"{key}\"");
        let at = xlf.find(&marker).expect("unit");
        let rest = &xlf[at..];
        let t = rest.find("<target xml:space=\"preserve\">").expect("target") + at;
        let open = "<target xml:space=\"preserve\">";
        let end = xlf[t..].find("</target>").expect("end") + t;
        format!("{}{open}{inner}{}", &xlf[..t], &xlf[end..])
    }

    #[test]
    fn split_pattern_round_trips_and_protects_placeables() {
        for pattern in [
            "Hello",
            "Hello, { $name }!",
            "{ $count ->\n [one] { $count } round\n *[other] { $count } rounds\n}",
            "{ $n ->\n    [0] none\n   *[other] { NUMBER($n, minimumFractionDigits: 2) } { -brand } items\n}",
            "a { \"}\" } b",
            "unbalanced { open",
            "{ $a ->\n *[x] { $b ->\n  [y] deep\n *[z] deeper\n }\n}",
            "",
            "plain\nmultiline",
        ] {
            let joined: String = split_pattern(pattern)
                .iter()
                .map(|p| match p {
                    Piece::Text(t) => t.clone(),
                    Piece::Code { equiv, .. } => equiv.clone(),
                })
                .collect();
            assert_eq!(joined, pattern, "round trip of {pattern:?}");
        }
        let pieces = split_pattern("Hello, { $name }!");
        assert_eq!(
            pieces,
            [
                Piece::Text("Hello, ".into()),
                Piece::Code {
                    equiv: "{ $name }".into(),
                    protected: true
                },
                Piece::Text("!".into()),
            ]
        );
        // Variant text stays translatable; select structure is deletable, placeables are not.
        let pieces = split_pattern("{ $c ->\n [one] { $c } round\n *[other] { $c } rounds\n}");
        let texts: Vec<&str> = pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Text(t) => Some(t.as_str()),
                Piece::Code { .. } => None,
            })
            .collect();
        assert_eq!(texts, [" round", " rounds"]);
        let protected = pieces
            .iter()
            .filter(|p| matches!(p, Piece::Code { protected: true, .. }))
            .count();
        assert_eq!(protected, 2);
    }

    #[test]
    fn export_is_xliff_20_with_units_codes_notes_and_is_deterministic() {
        let xlf = exported();
        assert_eq!(xlf, exported());
        assert!(
            xlf.contains("xmlns=\"urn:oasis:names:tc:xliff:document:2.0\""),
            "{xlf}"
        );
        assert!(xlf.contains("srcLang=\"en\" trgLang=\"fr\""), "{xlf}");
        let keys: Vec<usize> = ["greet", "hud-ammo", "menu-quit", "tags"]
            .iter()
            .map(|k| xlf.find(&format!("<unit id=\"{k}\"")).expect("unit"))
            .collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "sorted by key");
        assert!(
            xlf.contains(
                "<source xml:space=\"preserve\">Hello, <ph id=\"1\" canDelete=\"no\" equiv=\"{ $name }\" disp=\"$name\"/>!</source>"
            ),
            "{xlf}"
        );
        assert!(xlf.contains("<note category=\"location\">scenes/menu.ron:12</note>"));
        assert!(xlf.contains("<note category=\"regia\">variables: $count</note>"));
        assert!(xlf.contains("a &lt; b &amp; \"c\""), "{xlf}");
        // It is XML our own strict reader accepts.
        let back = read_xliff(&xlf).expect("reads");
        assert!(back.skipped.is_empty());
        assert_eq!(back.po.header_value("Language"), Some("fr"));
        assert_eq!(back.po.header_value("X-Regia-Source-Locale"), Some("en"));
    }

    #[test]
    fn untouched_export_reads_back_as_the_same_entries() {
        let po = export_po(&input()).file;
        let back = read_xliff(&write_xliff(&po).expect("writes")).expect("reads");
        assert_eq!(back.po.entries.len(), po.entries.len());
        for (a, b) in po.entries.iter().zip(&back.po.entries) {
            assert_eq!((&a.ctxt, &a.msgid, &a.msgstr), (&b.ctxt, &b.msgid, &b.msgstr));
        }
    }

    /// Spec 36 § 3.2: a translation-memory pre-fill carries the review flag through XLIFF as
    /// `rg:review="yes"` and reads back as the same flag (lane spec-15-35, ADR-5778).
    #[test]
    fn the_review_flag_round_trips_through_xliff() {
        let mut po = export_po(&input()).file;
        let first = &mut po.entries[0];
        first.msgstr = "Bonjour, { $name } !".to_owned();
        first.flags.push(crate::loc_glossary::TM_REVIEW_FLAG.to_owned());
        let xlf = write_xliff(&po).expect("writes");
        assert!(xlf.contains("rg:review=\"yes\""), "{xlf}");
        assert_eq!(xlf.matches("rg:review=").count(), 1, "only the flagged unit");
        let back = read_xliff(&xlf).expect("reads");
        let flagged: Vec<&str> = back
            .po
            .entries
            .iter()
            .filter(|e| e.flags.iter().any(|f| f == crate::loc_glossary::TM_REVIEW_FLAG))
            .map(|e| e.ctxt.as_str())
            .collect();
        assert_eq!(flagged, [po.entries[0].ctxt.as_str()]);
        assert_eq!(back.po.entries[0].msgstr, po.entries[0].msgstr);
    }

    #[test]
    fn xliff_import_equals_po_import_for_the_same_translations() {
        let mut po = export_po(&input()).file;
        for e in &mut po.entries {
            e.msgstr = match e.ctxt.as_str() {
                "greet" => "Bonjour, { $name } !".into(),
                "menu-quit" => "Quitter".into(),
                "hud-ammo" => {
                    "{ $count ->\n [one] { $count } cartouche\n *[other] { $count } cartouches\n}".into()
                }
                "tags" => "a < b & \"c\" é".into(),
                _ => String::new(),
            };
        }
        let via_po = plan_import(&po, &ctx());
        let xlf = write_xliff(&po).expect("writes");
        let read = read_xliff(&xlf).expect("reads");
        assert!(read.skipped.is_empty(), "{:?}", read.skipped);
        let via_xliff = plan_import_xliff(&read, &ctx());
        assert_eq!(via_xliff.imported_ftl, via_po.imported_ftl);
        assert_eq!(via_xliff.report.imported, 4);
        assert!(!via_xliff.imported_ftl.is_empty());
    }

    #[test]
    fn a_translator_who_deletes_a_placeholder_is_refused_with_its_code() {
        let xlf = fill(&exported(), "greet", "Bonjour !");
        let read = read_xliff(&xlf).expect("reads");
        let plan = plan_import_xliff(&read, &ctx());
        assert_eq!(plan.report.imported, 0);
        let n = plan
            .report
            .skipped
            .iter()
            .find(|n| n.key == "greet")
            .expect("greet is skipped");
        assert_eq!(n.code, "loc_xliff_placeholder_missing");
        assert!(n.message.contains("{ $name }"), "{}", n.message);
    }

    #[test]
    fn a_reordered_or_repeated_placeholder_is_fine_and_an_unknown_code_is_not() {
        let ph = "<ph id=\"1\" canDelete=\"no\" equiv=\"{ $name }\"/>";
        let xlf = fill(&exported(), "greet", &format!("{ph}, bonjour {ph}"));
        let plan = plan_import_xliff(&read_xliff(&xlf).expect("reads"), &ctx());
        assert_eq!(plan.report.imported, 1, "{:?}", plan.report.skipped);
        assert!(plan.imported_ftl.contains("greet = { $name }, bonjour { $name }"));
        // The equiv of a target code is ignored: the source's text for the id wins.
        let forged = fill(
            &exported(),
            "greet",
            "<ph id=\"1\" canDelete=\"no\" equiv=\"{ $other }\"/> salut",
        );
        let plan = plan_import_xliff(&read_xliff(&forged).expect("reads"), &ctx());
        assert!(
            plan.imported_ftl.contains("greet = { $name } salut"),
            "{}",
            plan.imported_ftl
        );
        let unknown = fill(&exported(), "greet", "<ph id=\"9\" equiv=\"{ $zzz }\"/> salut");
        let plan = plan_import_xliff(&read_xliff(&unknown).expect("reads"), &ctx());
        assert_eq!(plan.report.skipped[0].code, "loc_xliff_unknown_code");
    }

    #[test]
    fn stale_and_fuzzy_and_language_follow_the_po_rules() {
        // Stale: the source changed after the export.
        let xlf = fill(&exported(), "menu-quit", "Quitter");
        let mut changed = ctx();
        changed.source_files = vec![(
            "assets/localization/en/main.ftl".into(),
            SOURCE.replace("menu-quit = Quit", "menu-quit = Quit game"),
        )];
        let plan = plan_import_xliff(&read_xliff(&xlf).expect("reads"), &changed);
        assert_eq!(plan.report.skipped_with("loc_po_stale"), 1);
        assert!(plan.report.failed(true));
        // Fuzzy.
        let fuzzy = xlf.replace(
            "<unit id=\"menu-quit\"",
            "<unit rg:fuzzy=\"yes\" id=\"menu-quit\"",
        );
        let plan = plan_import_xliff(&read_xliff(&fuzzy).expect("reads"), &ctx());
        assert_eq!(plan.report.skipped_with("loc_po_fuzzy"), 1);
        // Wrong language.
        let mut other = ctx();
        other.locale = "de".into();
        let plan = plan_import_xliff(&read_xliff(&xlf).expect("reads"), &other);
        assert_eq!(
            plan.report.refused.expect("refused").code,
            "loc_po_language_mismatch"
        );
    }

    #[test]
    fn variable_and_plural_checks_run_on_the_rebuilt_pattern() {
        // A placeholder swapped for another variable's code is not a source code: refused.
        // A select whose variants lost the variable is caught by the PO checks.
        let xlf = fill(&exported(), "hud-ammo", "Munitions");
        let plan = plan_import_xliff(&read_xliff(&xlf).expect("reads"), &ctx());
        assert_eq!(plan.report.imported, 0);
        let code = &plan
            .report
            .skipped
            .iter()
            .find(|n| n.key == "hud-ammo")
            .expect("skipped")
            .code;
        assert!(
            code == "loc_xliff_placeholder_missing" || code == "loc_variable_mismatch",
            "{code}"
        );
    }

    #[test]
    fn the_reader_refuses_hostile_and_foreign_files() {
        let doctype = "<?xml version=\"1.0\"?><!DOCTYPE x [<!ENTITY a \"b\">]><xliff/>";
        assert_eq!(
            read_xliff(doctype).expect_err("doctype").code,
            "loc_xliff_doctype"
        );
        let entity = exported().replace("Quit<", "&bomb;<");
        assert_eq!(read_xliff(&entity).expect_err("entity").code, "loc_xliff_entity");
        let v1 = "<xliff xmlns=\"urn:oasis:names:tc:xliff:document:1.2\" version=\"1.2\"/>";
        assert_eq!(read_xliff(v1).expect_err("1.2").code, "loc_xliff_version");
        assert_eq!(read_xliff("<xliff").expect_err("cut").code, "loc_xliff_syntax");
        assert_eq!(
            read_xliff("<a></b>").expect_err("mismatch").code,
            "loc_xliff_syntax"
        );
        assert_eq!(read_xliff("").expect_err("empty").code, "loc_xliff_syntax");
        let deep = format!("{}{}", "<a>".repeat(200), "</a>".repeat(200));
        assert_eq!(read_xliff(&deep).expect_err("deep").code, "loc_xliff_limit");
        let tiny = XliffLimits {
            max_bytes: 10,
            ..XliffLimits::default()
        };
        assert_eq!(
            read_xliff_with(&exported(), tiny).expect_err("big").code,
            "loc_xliff_limit"
        );
        let many = format!(
            "<a {}/>",
            (0..64)
                .map(|i| format!("a{i}=\"1\""))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert_eq!(read_xliff(&many).expect_err("attrs").code, "loc_xliff_limit");
        let no_target =
            "<xliff xmlns=\"urn:oasis:names:tc:xliff:document:2.0\" version=\"2.0\" srcLang=\"en\"/>";
        assert_eq!(
            read_xliff(no_target).expect_err("trg").code,
            "loc_xliff_bad_header"
        );
    }

    #[test]
    fn writer_rejects_characters_xml_cannot_carry() {
        let mut po = export_po(&input()).file;
        po.entries[0].msgstr = "bad \u{1} char".into();
        assert_eq!(write_xliff(&po).expect_err("bad char").code, "loc_xliff_bad_char");
    }

    #[test]
    fn multi_segment_units_cdata_and_char_references_read() {
        let xlf = "<?xml version=\"1.0\"?>\n<xliff xmlns=\"urn:oasis:names:tc:xliff:document:2.0\" version=\"2.1\" srcLang=\"en\" trgLang=\"fr\">\
<file id=\"f\"><group id=\"g\"><unit id=\"menu-quit\"><segment><source>Qu</source><target>Quit&#xE9;</target></segment>\
<ignorable><source>i</source><target><![CDATA[t]]></target></ignorable><segment><source>t</source><target><mrk id=\"m\" type=\"term\">er</mrk></target></segment></unit></group></file></xliff>";
        let read = read_xliff(xlf).expect("reads");
        assert!(read.skipped.is_empty(), "{:?}", read.skipped);
        assert_eq!(read.po.entries[0].msgid, "Quit");
        assert_eq!(read.po.entries[0].msgstr, "Quitéter");
        let half = xlf.replace("<target>Quit&#xE9;</target>", "");
        let read = read_xliff(&half).expect("reads");
        assert_eq!(read.skipped[0].code, "loc_xliff_incomplete_segment");
    }

    #[test]
    fn crlf_and_entity_text_normalize() {
        let r = Reader {
            src: "",
            bytes: b"",
            pos: 0,
            limits: XliffLimits::default(),
            nodes: 0,
        };
        assert_eq!(
            r.decode("a\r\nb\rc &amp; &#65;", false).expect("text"),
            "a\nb\nc & A"
        );
        assert_eq!(r.decode("a\tb\nc", true).expect("attr"), "a b c");
        assert!(r.decode("&#0;", false).is_err());
        assert!(r.decode("&amp", false).is_err());
    }

    #[test]
    fn looks_like_xliff_tells_it_from_po() {
        assert!(looks_like_xliff("\u{FEFF}  <?xml version=\"1.0\"?>"));
        assert!(!looks_like_xliff("msgid \"\"\nmsgstr \"\"\n"));
    }
}
