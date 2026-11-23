// SPDX-License-Identifier: MPL-2.0
//! Structural diff of assets and graphs against a git revision (spec addendum § 10.8, P-182).
//!
//! Three layers, all read-only:
//!
//! - **RON tree diff.** [`parse_ron`] reads any RON document into a [`RonValue`] tree that keeps
//!   struct and variant names (a generic value would drop them), and [`diff_values`] lists the
//!   paths that were added, removed or changed ([`PathChange`]): struct fields by name, map entries
//!   by key, sequences aligned by their longest common run of equal items.
//! - **Graph diff.** An adapter per graph format turns a document (and its layout sidecar, when
//!   there is one) into a [`GraphDoc`]: nodes with an id, a kind, an authored canvas position and a
//!   body, and wires by endpoints. [`diff_graphs`] matches nodes by id, then the rest by kind and
//!   position (a renamed node that stayed put), and compares bodies and wires. Adapters: material
//!   graphs and functions (`.matgraph.ron`, `.matfunc.ron`), animation graphs (`.animgraph.ron`)
//!   and logic graphs (`.logic.ron`, rules as nodes). Any other `.ron` is a tree diff.
//! - **Revisions.** [`Git`] runs `git` through an argument vector (never a shell string), with the
//!   project root as its working directory: `git show <rev>:<path>` for a side and
//!   `git log --follow` for the revision picker ([`asset_revisions`]). A project outside a git work
//!   tree (or no `git` program) is `no_revision_control`.
//!
//! [`diff_asset`] is the entry point the Content Browser, the graph panels, the revision control
//! panel and `editor.asset_diff` share; [`diff_texts`] diffs two texts without any revision
//! control. Error codes: see [`DiffError`]. Agent pages: `book/agents/asset-diff.md`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use regia_agent::protocol::{self, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a diff could not be made. `code` is the agent protocol's `reason`:
///
/// | Code | Meaning |
/// |---|---|
/// | `no_project` | No project is open (agent) or the file is not inside one (panels). |
/// | `bad_path` | The path parameter is missing or empty. |
/// | `path_outside_project` | The path leads outside the project (absolute elsewhere, `..`, a symlink out). |
/// | `no_revision_control` | The project is not in a git work tree, or `git` cannot be run. |
/// | `bad_revision` | A revision is malformed or names no commit. |
/// | `not_in_revision` | The file does not exist at that revision. |
/// | `read_failed` | The working copy of the file cannot be read. |
/// | `unsupported_asset` | The file is not a RON document (only `.ron` assets diff). |
/// | `parse_failed` | A side is not valid RON, or not a valid graph of its kind. |
/// | `git_failed` | `git` ran but failed for another reason. |
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffError {
    /// Stable lowercase reason code (table above).
    pub code: &'static str,
    /// What went wrong, for people.
    pub message: String,
}

impl DiffError {
    /// An error with `code` and `message`.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for DiffError {}

/// Every [`DiffError::code`], in the order of the table (the agent page lists the same).
pub const ERROR_CODES: &[&str] = &[
    "no_project",
    "bad_path",
    "path_outside_project",
    "no_revision_control",
    "bad_revision",
    "not_in_revision",
    "read_failed",
    "unsupported_asset",
    "parse_failed",
    "git_failed",
];

// ---------------------------------------------------------------------------
// RON tree
// ---------------------------------------------------------------------------

/// A RON value that keeps the names a typed reader would see (`Mul("a", "b")`, `Clip((..))`).
#[derive(Clone, Debug, PartialEq)]
pub enum RonValue {
    /// A number, boolean or character, as written (`1.5`, `true`, `'x'`).
    Scalar(String),
    /// A string or byte string, decoded.
    Str(String),
    /// A bare identifier: a unit variant or unit struct (`None`, `Linear`).
    Ident(String),
    /// A list `[a, b]`.
    Seq(Vec<RonValue>),
    /// A map `{k: v}`, in written order.
    Map(Vec<(RonValue, RonValue)>),
    /// A struct `(a: 1)` or `Name(a: 1)`, fields in written order.
    Struct {
        /// The struct or variant name, when written.
        name: Option<String>,
        /// Fields in written order.
        fields: Vec<(String, RonValue)>,
    },
    /// A tuple `(1, 2)`, a tuple struct or variant `Name(1, 2)`, `Some(x)` or the unit `()`.
    Tuple {
        /// The tuple struct or variant name, when written.
        name: Option<String>,
        /// Items in order.
        items: Vec<RonValue>,
    },
}

/// Nesting deeper than this is refused (a hostile file cannot overflow the stack).
pub const MAX_DEPTH: usize = 200;

/// Reads a RON document (comments, `#![enable(..)]` attributes, raw strings and all value forms)
/// into a [`RonValue`].
///
/// # Errors
/// A message with the line and column of the first syntax error.
pub fn parse_ron(text: &str) -> Result<RonValue, String> {
    let mut p = RonParser {
        src: text.as_bytes(),
        text,
        i: 0,
        depth: 0,
    };
    p.skip();
    while p.src[p.i..].starts_with(b"#![") {
        while p.i < p.src.len() && p.src[p.i] != b']' {
            p.i += 1;
        }
        if p.i >= p.src.len() {
            return Err(p.error("unterminated attribute"));
        }
        p.i += 1;
        p.skip();
    }
    let value = p.value()?;
    p.skip();
    if p.i < p.src.len() {
        return Err(p.error("unexpected text after the value"));
    }
    Ok(value)
}

struct RonParser<'a> {
    src: &'a [u8],
    text: &'a str,
    i: usize,
    depth: usize,
}

impl RonParser<'_> {
    fn error(&self, what: &str) -> String {
        let mut end = self.i.min(self.text.len());
        while !self.text.is_char_boundary(end) {
            end -= 1;
        }
        let before = &self.text[..end];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        format!("{what} at line {line}, column {col}")
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.i).copied()
    }

    /// Skips whitespace, line comments and (nested) block comments.
    fn skip(&mut self) {
        loop {
            while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                self.i += 1;
            }
            if self.src[self.i..].starts_with(b"//") {
                while self.peek().is_some_and(|c| c != b'\n') {
                    self.i += 1;
                }
                continue;
            }
            if self.src[self.i..].starts_with(b"/*") {
                let mut level = 0usize;
                while self.i < self.src.len() {
                    if self.src[self.i..].starts_with(b"/*") {
                        level += 1;
                        self.i += 2;
                    } else if self.src[self.i..].starts_with(b"*/") {
                        level -= 1;
                        self.i += 2;
                        if level == 0 {
                            break;
                        }
                    } else {
                        self.i += 1;
                    }
                }
                continue;
            }
            break;
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        self.skip();
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected `{}`", c as char)))
        }
    }

    fn value(&mut self) -> Result<RonValue, String> {
        self.skip();
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("nesting too deep"));
        }
        let out = self.value_inner();
        self.depth -= 1;
        out
    }

    fn value_inner(&mut self) -> Result<RonValue, String> {
        match self.peek() {
            None => Err(self.error("expected a value")),
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.skip();
                    if self.peek() == Some(b']') {
                        self.i += 1;
                        return Ok(RonValue::Seq(items));
                    }
                    items.push(self.value()?);
                    self.skip();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {}
                        _ => return Err(self.error("expected `,` or `]`")),
                    }
                }
            }
            Some(b'{') => {
                self.i += 1;
                let mut entries = Vec::new();
                loop {
                    self.skip();
                    if self.peek() == Some(b'}') {
                        self.i += 1;
                        return Ok(RonValue::Map(entries));
                    }
                    let key = self.value()?;
                    self.expect(b':')?;
                    let value = self.value()?;
                    entries.push((key, value));
                    self.skip();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {}
                        _ => return Err(self.error("expected `,` or `}`")),
                    }
                }
            }
            Some(b'(') => {
                self.i += 1;
                self.paren(None)
            }
            Some(b'"') => self.string().map(RonValue::Str),
            Some(b'b') if self.src.get(self.i + 1) == Some(&b'"') => {
                self.i += 1;
                self.string().map(RonValue::Str)
            }
            Some(b'r' | b'b')
                if matches!(self.src.get(self.i + 1), Some(b'"' | b'#'))
                    || self.src[self.i..].starts_with(b"br\"")
                    || self.src[self.i..].starts_with(b"br#") =>
            {
                if self.peek() == Some(b'b') {
                    self.i += 1;
                }
                self.raw_string().map(RonValue::Str)
            }
            Some(b'\'') => self.char_lit().map(RonValue::Scalar),
            Some(c) if c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.' => {
                let start = self.i;
                self.i += 1;
                while self
                    .peek()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'+' | b'-'))
                {
                    // `1e-5`: a sign only after an exponent marker.
                    let c = self.src[self.i];
                    if matches!(c, b'+' | b'-') && !matches!(self.src[self.i - 1], b'e' | b'E') {
                        break;
                    }
                    self.i += 1;
                }
                Ok(RonValue::Scalar(self.text[start..self.i].to_string()))
            }
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                let name = self.ident()?;
                self.skip();
                if self.peek() == Some(b'(') {
                    self.i += 1;
                    return self.paren(Some(name));
                }
                Ok(match name.as_str() {
                    "true" | "false" | "inf" | "NaN" => RonValue::Scalar(name),
                    _ => RonValue::Ident(name),
                })
            }
            Some(_) => Err(self.error("unexpected character")),
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        if self.src[self.i..].starts_with(b"r#") {
            self.i += 2;
        }
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.i += 1;
        }
        if start == self.i {
            return Err(self.error("expected a name"));
        }
        Ok(self.text[start..self.i].to_string())
    }

    /// After `(`: a struct body (`a: 1, ..`) or tuple items.
    fn paren(&mut self, name: Option<String>) -> Result<RonValue, String> {
        self.skip();
        let save = self.i;
        let is_struct = match self.peek() {
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                let _ = self.ident();
                self.skip();
                let colon = self.peek() == Some(b':') && self.src.get(self.i + 1) != Some(&b':');
                self.i = save;
                colon
            }
            _ => false,
        };
        if is_struct {
            let mut fields = Vec::new();
            loop {
                self.skip();
                if self.peek() == Some(b')') {
                    self.i += 1;
                    return Ok(RonValue::Struct { name, fields });
                }
                let field = self.ident()?;
                self.expect(b':')?;
                let value = self.value()?;
                fields.push((field, value));
                self.skip();
                match self.peek() {
                    Some(b',') => self.i += 1,
                    Some(b')') => {}
                    _ => return Err(self.error("expected `,` or `)`")),
                }
            }
        }
        let mut items = Vec::new();
        loop {
            self.skip();
            if self.peek() == Some(b')') {
                self.i += 1;
                return Ok(RonValue::Tuple { name, items });
            }
            items.push(self.value()?);
            self.skip();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b')') => {}
                _ => return Err(self.error("expected `,` or `)`")),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let Some(c) = self.text[self.i..].chars().next() else {
                return Err(self.error("unterminated string"));
            };
            self.i += c.len_utf8();
            match c {
                '"' => return Ok(out),
                '\\' => out.push(self.escape()?),
                c => out.push(c),
            }
        }
    }

    fn escape(&mut self) -> Result<char, String> {
        let Some(c) = self.peek() else {
            return Err(self.error("unterminated escape"));
        };
        self.i += 1;
        Ok(match c {
            b'n' => '\n',
            b't' => '\t',
            b'r' => '\r',
            b'0' => '\0',
            b'\\' => '\\',
            b'"' => '"',
            b'\'' => '\'',
            b'x' => {
                let byte = self
                    .text
                    .get(self.i..self.i + 2)
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                    .ok_or_else(|| self.error("bad `\\x` escape"))?;
                self.i += 2;
                byte as char
            }
            b'u' => {
                let rest = &self.text[self.i..];
                let (digits, used) = if let Some(body) = rest.strip_prefix('{') {
                    let end = body.find('}').ok_or_else(|| self.error("bad `\\u` escape"))?;
                    (&body[..end], end + 2)
                } else {
                    (rest.get(..4).ok_or_else(|| self.error("bad `\\u` escape"))?, 4)
                };
                let c = u32::from_str_radix(digits, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| self.error("bad `\\u` escape"))?;
                self.i += used;
                c
            }
            _ => return Err(self.error("unknown escape")),
        })
    }

    fn raw_string(&mut self) -> Result<String, String> {
        self.i += 1; // `r`
        let mut hashes = 0;
        while self.peek() == Some(b'#') {
            hashes += 1;
            self.i += 1;
        }
        if self.peek() != Some(b'"') {
            return Err(self.error("expected `\"` in a raw string"));
        }
        self.i += 1;
        let close = format!("\"{}", "#".repeat(hashes));
        let rest = &self.text[self.i..];
        let end = rest
            .find(&close)
            .ok_or_else(|| self.error("unterminated raw string"))?;
        let body = rest[..end].to_string();
        self.i += end + close.len();
        Ok(body)
    }

    fn char_lit(&mut self) -> Result<String, String> {
        let start = self.i;
        self.i += 1;
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated character")),
                Some(b'\\') => self.i += 2,
                Some(b'\'') => {
                    self.i += 1;
                    return Ok(self.text[start..self.i].to_string());
                }
                Some(_) => self.i += 1,
            }
        }
    }
}

impl RonValue {
    /// The struct or variant name (`Mul` of `Mul("a", "b")`, `Linear` of a unit variant).
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Ident(n) => Some(n),
            Self::Struct { name, .. } | Self::Tuple { name, .. } => name.as_deref(),
            _ => None,
        }
    }

    /// The value of struct field `key`.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&RonValue> {
        match self {
            Self::Struct { fields, .. } => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// This value without struct field `key` (other values unchanged).
    #[must_use]
    pub fn without_field(&self, key: &str) -> RonValue {
        match self {
            Self::Struct { name, fields } => Self::Struct {
                name: name.clone(),
                fields: fields.iter().filter(|(k, _)| k != key).cloned().collect(),
            },
            other => other.clone(),
        }
    }

    /// Written as compact RON (one line), which `ron` reads back into the same typed value.
    #[must_use]
    pub fn to_ron(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        use std::fmt::Write as _;
        match self {
            Self::Scalar(s) | Self::Ident(s) => out.push_str(s),
            Self::Str(s) => {
                let _ = write!(out, "{s:?}");
            }
            Self::Seq(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    v.write(out);
                }
                out.push(']');
            }
            Self::Map(entries) => {
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    k.write(out);
                    out.push_str(": ");
                    v.write(out);
                }
                out.push('}');
            }
            Self::Struct { name, fields } => {
                out.push_str(name.as_deref().unwrap_or(""));
                out.push('(');
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(k);
                    out.push_str(": ");
                    v.write(out);
                }
                out.push(')');
            }
            Self::Tuple { name, items } => {
                out.push_str(name.as_deref().unwrap_or(""));
                out.push('(');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    v.write(out);
                }
                out.push(')');
            }
        }
    }

    /// [`RonValue::to_ron`] cut to at most `max` characters (with `…`).
    #[must_use]
    pub fn summary(&self, max: usize) -> String {
        let full = self.to_ron();
        if full.chars().count() <= max {
            return full;
        }
        let mut cut: String = full.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }

    /// Whether the value is a leaf (no fields, items or entries to descend into).
    #[must_use]
    pub fn is_leaf(&self) -> bool {
        match self {
            Self::Scalar(_) | Self::Str(_) | Self::Ident(_) => true,
            Self::Seq(v) => v.is_empty(),
            Self::Map(v) => v.is_empty(),
            Self::Struct { fields, .. } => fields.is_empty(),
            Self::Tuple { items, .. } => items.is_empty(),
        }
    }
}

/// Whether two values mean the same: numbers compare by value (`1` = `1.0`), struct fields and
/// map entries regardless of order, and a missing struct name matches any name.
#[must_use]
pub fn same(a: &RonValue, b: &RonValue) -> bool {
    use RonValue as V;
    match (a, b) {
        (V::Scalar(x), V::Scalar(y)) => scalar_eq(x, y),
        (V::Str(x), V::Str(y)) | (V::Ident(x), V::Ident(y)) => x == y,
        (V::Seq(x), V::Seq(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q)),
        (V::Map(x), V::Map(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.iter().any(|(k2, v2)| same(k, k2) && same(v, v2)))
        }
        (V::Struct { name: n1, fields: f1 }, V::Struct { name: n2, fields: f2 }) => {
            names_match(n1, n2)
                && f1.len() == f2.len()
                && f1
                    .iter()
                    .all(|(k, v)| f2.iter().any(|(k2, v2)| k == k2 && same(v, v2)))
        }
        (V::Tuple { name: n1, items: i1 }, V::Tuple { name: n2, items: i2 }) => {
            names_match(n1, n2) && i1.len() == i2.len() && i1.iter().zip(i2).all(|(p, q)| same(p, q))
        }
        _ => false,
    }
}

fn names_match(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

fn scalar_eq(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (
        a.replace('_', "").parse::<f64>(),
        b.replace('_', "").parse::<f64>(),
    ) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Structural diff
// ---------------------------------------------------------------------------

/// What happened to an item between the old and the new side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// Only on the new side.
    Added,
    /// Only on the old side.
    Removed,
    /// On both sides, different.
    Changed,
    /// On both sides, the same.
    Unchanged,
}

impl ChangeKind {
    /// The protocol name: `added`, `removed`, `changed` or `unchanged`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Changed => "changed",
            Self::Unchanged => "unchanged",
        }
    }
}

/// One changed path of a document (`shading.roughness`, `nodes["albedo"]`, `rules[2].priority`).
#[derive(Clone, Debug, PartialEq)]
pub struct PathChange {
    /// Where: field names joined by `.`, sequence indices and map keys in `[..]`, tuple items as
    /// `.0`. Empty for the whole document.
    pub path: String,
    /// Added, removed or changed.
    pub kind: ChangeKind,
    /// The old value (compact RON), absent when added.
    pub old: Option<RonValue>,
    /// The new value (compact RON), absent when removed.
    pub new: Option<RonValue>,
}

/// Longest sequence pair (old length × new length) aligned by a longest common subsequence;
/// longer pairs are aligned by index.
pub const LCS_CELLS: usize = 1 << 20;

fn join_field(path: &str, field: &str) -> String {
    if path.is_empty() {
        field.to_string()
    } else {
        format!("{path}.{field}")
    }
}

/// The paths that differ between `old` and `new` (empty when they are [`same`]).
#[must_use]
pub fn diff_values(old: &RonValue, new: &RonValue) -> Vec<PathChange> {
    let mut out = Vec::new();
    diff_at("", old, new, &mut out);
    out
}

fn changed(path: &str, old: &RonValue, new: &RonValue, out: &mut Vec<PathChange>) {
    out.push(PathChange {
        path: path.to_string(),
        kind: ChangeKind::Changed,
        old: Some(old.clone()),
        new: Some(new.clone()),
    });
}

fn diff_at(path: &str, old: &RonValue, new: &RonValue, out: &mut Vec<PathChange>) {
    use RonValue as V;
    if same(old, new) {
        return;
    }
    match (old, new) {
        (V::Struct { name: n1, fields: f1 }, V::Struct { name: n2, fields: f2 }) if names_match(n1, n2) => {
            for (k, v) in f1 {
                let p = join_field(path, k);
                match f2.iter().find(|(k2, _)| k2 == k) {
                    Some((_, v2)) => diff_at(&p, v, v2, out),
                    None => out.push(PathChange {
                        path: p,
                        kind: ChangeKind::Removed,
                        old: Some(v.clone()),
                        new: None,
                    }),
                }
            }
            for (k, v) in f2 {
                if !f1.iter().any(|(k1, _)| k1 == k) {
                    out.push(PathChange {
                        path: join_field(path, k),
                        kind: ChangeKind::Added,
                        old: None,
                        new: Some(v.clone()),
                    });
                }
            }
        }
        (V::Tuple { name: n1, items: i1 }, V::Tuple { name: n2, items: i2 })
            if names_match(n1, n2) && i1.len() == i2.len() =>
        {
            for (i, (a, b)) in i1.iter().zip(i2).enumerate() {
                diff_at(&join_field(path, &i.to_string()), a, b, out);
            }
        }
        (V::Seq(a), V::Seq(b)) => diff_seq(path, a, b, out),
        (V::Map(a), V::Map(b)) => {
            for (k, v) in a {
                let p = format!("{path}[{}]", k.summary(60));
                match b.iter().find(|(k2, _)| same(k, k2)) {
                    Some((_, v2)) => diff_at(&p, v, v2, out),
                    None => out.push(PathChange {
                        path: p,
                        kind: ChangeKind::Removed,
                        old: Some(v.clone()),
                        new: None,
                    }),
                }
            }
            for (k, v) in b {
                if !a.iter().any(|(k1, _)| same(k1, k)) {
                    out.push(PathChange {
                        path: format!("{path}[{}]", k.summary(60)),
                        kind: ChangeKind::Added,
                        old: None,
                        new: Some(v.clone()),
                    });
                }
            }
        }
        _ => changed(path, old, new, out),
    }
}

/// Aligns two sequences by a longest common subsequence of equal items; the unmatched items
/// between two matches pair up in order (and diff as changed), the rest are added or removed.
fn diff_seq(path: &str, a: &[RonValue], b: &[RonValue], out: &mut Vec<PathChange>) {
    let (n, m) = (a.len(), b.len());
    // Matched index pairs, ascending.
    let mut anchors: Vec<(usize, usize)> = Vec::new();
    if n.saturating_mul(m) <= LCS_CELLS {
        let w = m + 1;
        let mut table = vec![0u32; (n + 1) * w];
        for (i, x) in a.iter().enumerate().rev() {
            for (j, y) in b.iter().enumerate().rev() {
                table[i * w + j] = if same(x, y) {
                    table[(i + 1) * w + j + 1] + 1
                } else {
                    table[(i + 1) * w + j].max(table[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if same(&a[i], &b[j]) {
                anchors.push((i, j));
                i += 1;
                j += 1;
            } else if table[(i + 1) * w + j] >= table[i * w + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
    } else {
        anchors.extend((0..n.min(m)).filter(|&k| same(&a[k], &b[k])).map(|k| (k, k)));
    }
    anchors.push((n, m));
    let (mut i0, mut j0) = (0, 0);
    for (ai, bj) in anchors {
        let olds = i0..ai;
        let news = j0..bj;
        let pairs = olds.len().min(news.len());
        for (k, (x, y)) in a[i0..i0 + pairs].iter().zip(&b[j0..j0 + pairs]).enumerate() {
            diff_at(&format!("{path}[{}]", j0 + k), x, y, out);
        }
        for (i, x) in a.iter().enumerate().take(ai).skip(i0 + pairs) {
            out.push(PathChange {
                path: format!("{path}[{i}]"),
                kind: ChangeKind::Removed,
                old: Some(x.clone()),
                new: None,
            });
        }
        for (j, y) in b.iter().enumerate().take(bj).skip(j0 + pairs) {
            out.push(PathChange {
                path: format!("{path}[{j}]"),
                kind: ChangeKind::Added,
                old: None,
                new: Some(y.clone()),
            });
        }
        i0 = ai + 1;
        j0 = bj + 1;
    }
}

// ---------------------------------------------------------------------------
// Graph adapters
// ---------------------------------------------------------------------------

/// What a file is, by its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetKind {
    /// `.matgraph.ron`.
    MaterialGraph,
    /// `.matfunc.ron`.
    MaterialFunction,
    /// `.animgraph.ron`.
    AnimGraph,
    /// `.logic.ron` (rules are the nodes).
    LogicGraph,
    /// Any other `.ron` document: a tree diff.
    Ron,
}

impl AssetKind {
    /// The kind of `path`, or `None` for a file that is not RON.
    #[must_use]
    pub fn of_path(path: &str) -> Option<Self> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".layout.ron") {
            return Some(Self::Ron);
        }
        Some(if lower.ends_with(".matgraph.ron") {
            Self::MaterialGraph
        } else if lower.ends_with(".matfunc.ron") {
            Self::MaterialFunction
        } else if lower.ends_with(".animgraph.ron") {
            Self::AnimGraph
        } else if lower.ends_with(".logic.ron") {
            Self::LogicGraph
        } else if lower.ends_with(".ron") {
            Self::Ron
        } else {
            return None;
        })
    }

    /// The protocol name: `material_graph`, `material_function`, `anim_graph`, `logic_graph`, `ron`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaterialGraph => "material_graph",
            Self::MaterialFunction => "material_function",
            Self::AnimGraph => "anim_graph",
            Self::LogicGraph => "logic_graph",
            Self::Ron => "ron",
        }
    }

    /// Whether the file diffs as a graph.
    #[must_use]
    pub fn is_graph(self) -> bool {
        !matches!(self, Self::Ron)
    }

    /// Whether the format keeps canvas positions in a `.layout.ron` sidecar.
    #[must_use]
    pub fn has_layout_sidecar(self) -> bool {
        matches!(
            self,
            Self::MaterialGraph | Self::MaterialFunction | Self::AnimGraph
        )
    }
}

/// The layout sidecar of a graph path: `x.matgraph.ron` → `x.matgraph.layout.ron`.
#[must_use]
pub fn layout_sidecar(path: &str) -> String {
    let stem = path.strip_suffix(".ron").unwrap_or(path);
    format!("{stem}.layout.ron")
}

/// A node of a graph document.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphNode {
    /// The node's id in its format: the name (material graphs, logic rules) or the node path
    /// (`root/base`, animation graphs).
    pub id: String,
    /// The node kind (`mul`, `blend`, `TriggerEnter`…).
    pub kind: String,
    /// The authored canvas position (from the layout sidecar), when there is one.
    pub pos: Option<[f32; 2]>,
    /// What the node holds, without its canvas position.
    pub body: RonValue,
}

/// A wire: node `from` feeds input `slot` of node `to`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GraphWire {
    /// The source node id.
    pub from: String,
    /// The destination node id (`output` for a material graph's surface outputs).
    pub to: String,
    /// The destination input.
    pub slot: String,
}

/// A graph document as the diff sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphDoc {
    /// Nodes in document order.
    pub nodes: Vec<GraphNode>,
    /// Wires.
    pub wires: Vec<GraphWire>,
    /// The rest of the document (parameters, outputs, shading…), diffed as a tree.
    pub rest: RonValue,
}

fn parse_failed(side: &str, e: impl std::fmt::Display) -> DiffError {
    DiffError::new("parse_failed", format!("the {side} side cannot be read: {e}"))
}

/// Canvas positions by node id from a layout sidecar's `positions` map.
fn sidecar_positions(layout: Option<&str>) -> BTreeMap<String, [f32; 2]> {
    let mut out = BTreeMap::new();
    let Some(tree) = layout.and_then(|t| parse_ron(t).ok()) else {
        return out;
    };
    let Some(RonValue::Map(entries)) = tree.field("positions") else {
        return out;
    };
    for (k, v) in entries {
        let (RonValue::Str(id), RonValue::Seq(xy) | RonValue::Tuple { items: xy, .. }) = (k, v) else {
            continue;
        };
        let nums: Vec<f32> = xy
            .iter()
            .filter_map(|c| match c {
                RonValue::Scalar(s) => s.parse::<f32>().ok(),
                _ => None,
            })
            .collect();
        if let [x, y] = nums[..]
            && x.is_finite()
            && y.is_finite()
        {
            out.insert(id.clone(), [x, y]);
        }
    }
    out
}

/// Builds the [`GraphDoc`] of a graph file's `text` (with its layout sidecar's text, if any).
///
/// # Errors
/// `parse_failed` (`side` names the side in the message), or `unsupported_asset` for
/// [`AssetKind::Ron`].
pub fn graph_doc(
    kind: AssetKind,
    text: &str,
    layout: Option<&str>,
    side: &str,
) -> Result<GraphDoc, DiffError> {
    match kind {
        AssetKind::MaterialGraph | AssetKind::MaterialFunction => material_doc(text, layout, side),
        AssetKind::AnimGraph => anim_doc(text, layout, side),
        AssetKind::LogicGraph => logic_doc(text, side),
        AssetKind::Ron => Err(DiffError::new("unsupported_asset", "not a graph format")),
    }
}

fn material_doc(text: &str, layout: Option<&str>, side: &str) -> Result<GraphDoc, DiffError> {
    let tree = parse_ron(text).map_err(|e| parse_failed(side, e))?;
    let positions = sidecar_positions(layout);
    let mut nodes = Vec::new();
    let mut wires = Vec::new();
    if let Some(RonValue::Map(entries)) = tree.field("nodes") {
        for (k, v) in entries {
            let RonValue::Str(id) = k else {
                return Err(parse_failed(side, "a node name is not a string"));
            };
            // The typed node gives its kind and inputs; a node this build cannot read still
            // diffs by its written variant name (no wires).
            let typed = ron::from_str::<regia_matgraph::Node>(&v.to_ron()).ok();
            let kind = typed.as_ref().map_or_else(
                || v.name().unwrap_or("node").to_string(),
                |n| n.kind().to_string(),
            );
            if let Some(node) = &typed {
                for (i, input) in node.inputs().into_iter().enumerate() {
                    wires.push(GraphWire {
                        from: input.to_string(),
                        to: id.clone(),
                        slot: format!("in{i}"),
                    });
                }
            }
            nodes.push(GraphNode {
                id: id.clone(),
                kind,
                pos: positions.get(id).copied(),
                body: v.clone(),
            });
        }
    }
    // Graph surface outputs (`output: (base_color: Some("c"))`) and function outputs
    // (`outputs: {"out": "c"}`) are wires into the `output` node.
    if let Some(RonValue::Struct { fields, .. }) = tree.field("output") {
        for (slot, v) in fields {
            let from = match v {
                RonValue::Str(s) => Some(s),
                RonValue::Tuple { name, items } if name.as_deref() == Some("Some") => {
                    match items.as_slice() {
                        [RonValue::Str(s)] => Some(s),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(from) = from {
                wires.push(GraphWire {
                    from: from.clone(),
                    to: "output".into(),
                    slot: slot.clone(),
                });
            }
        }
    }
    if let Some(RonValue::Map(entries)) = tree.field("outputs") {
        for (k, v) in entries {
            if let (RonValue::Str(slot), RonValue::Str(from)) = (k, v) {
                wires.push(GraphWire {
                    from: from.clone(),
                    to: "output".into(),
                    slot: slot.clone(),
                });
            }
        }
    }
    Ok(GraphDoc {
        nodes,
        wires,
        rest: tree.without_field("nodes"),
    })
}

fn anim_doc(text: &str, layout: Option<&str>, side: &str) -> Result<GraphDoc, DiffError> {
    use crate::anim::model::{GraphModel, NodeKind};
    let graph = regia_animation::graph::AnimGraph::from_ron_str(text).map_err(|e| parse_failed(side, e))?;
    let positions = sidecar_positions(layout);
    let model = GraphModel::from_graph(&graph, &BTreeMap::new());
    let compiled = model.compile().map_err(|e| parse_failed(side, e))?;
    let path_of = |id: &crate::anim::model::NodeId| compiled.paths.get(id).cloned();
    let mut nodes = Vec::new();
    let mut wires = Vec::new();
    // Document order: by node path.
    let mut ids: Vec<_> = model
        .nodes
        .keys()
        .filter_map(|id| Some((path_of(id)?, *id)))
        .collect();
    ids.sort();
    for (path, id) in ids {
        let Some(node) = model.nodes.get(&id) else {
            continue;
        };
        let names = node.kind.input_names();
        for (slot, input) in node.inputs.iter().enumerate() {
            if let Some(from) = input.as_ref().and_then(path_of) {
                wires.push(GraphWire {
                    from,
                    to: path.clone(),
                    slot: names.get(slot).cloned().unwrap_or_else(|| format!("in{slot}")),
                });
            }
        }
        let text = ron::to_string(&node.kind).map_err(|e| parse_failed(side, e))?;
        let mut body = parse_ron(&text).map_err(|e| parse_failed(side, e))?;
        if let NodeKind::StateMachine(machine) = &node.kind {
            // A state's output names a node by the model's counter, which is not stable between
            // revisions: name it by path, and drop the state's canvas position (layout data).
            let outputs: Vec<Option<String>> = machine
                .states
                .iter()
                .map(|s| s.output.as_ref().and_then(path_of))
                .collect();
            scrub_states(&mut body, &outputs);
        }
        nodes.push(GraphNode {
            pos: positions.get(&path).copied(),
            id: path,
            kind: node.kind.label().to_string(),
            body,
        });
    }
    let tree = parse_ron(text).map_err(|e| parse_failed(side, e))?;
    Ok(GraphDoc {
        nodes,
        wires,
        rest: tree.without_field("root"),
    })
}

/// In a serialized state machine body, replaces each state's `output` with its node path and
/// removes its `pos`.
fn scrub_states(body: &mut RonValue, outputs: &[Option<String>]) {
    fn states_of(v: &mut RonValue) -> Option<&mut Vec<RonValue>> {
        match v {
            RonValue::Struct { fields, .. } => fields.iter_mut().find_map(|(k, v)| match (k.as_str(), v) {
                ("states", RonValue::Seq(items)) => Some(items),
                (_, other) => states_of(other),
            }),
            RonValue::Tuple { items, .. } => items.iter_mut().find_map(states_of),
            _ => None,
        }
    }
    let Some(states) = states_of(body) else {
        return;
    };
    for (state, output) in states.iter_mut().zip(outputs) {
        if let RonValue::Struct { fields, .. } = state {
            fields.retain(|(k, _)| k != "pos");
            for (k, v) in fields.iter_mut() {
                if k == "output" {
                    *v = output
                        .as_ref()
                        .map_or_else(|| RonValue::Ident("None".into()), |p| RonValue::Str(p.clone()));
                }
            }
        }
    }
}

fn logic_doc(text: &str, side: &str) -> Result<GraphDoc, DiffError> {
    let tree = parse_ron(text).map_err(|e| parse_failed(side, e))?;
    let mut nodes = Vec::new();
    if let Some(RonValue::Seq(rules)) = tree.field("rules") {
        for (i, rule) in rules.iter().enumerate() {
            let id = match rule.field("name") {
                Some(RonValue::Str(name)) => name.clone(),
                _ => format!("rule {i}"),
            };
            let kind = rule
                .field("event")
                .and_then(RonValue::name)
                .unwrap_or("rule")
                .to_string();
            nodes.push(GraphNode {
                id,
                kind,
                pos: None,
                body: rule.clone(),
            });
        }
    }
    Ok(GraphDoc {
        nodes,
        wires: Vec::new(),
        rest: tree.without_field("rules"),
    })
}

// ---------------------------------------------------------------------------
// Graph diff
// ---------------------------------------------------------------------------

/// How far (canvas units) two authored positions may be apart and still match by position.
pub const POSITION_MATCH: f32 = 1.0;

/// One node of a graph diff.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeDiff {
    /// Added, removed, changed or unchanged.
    pub status: ChangeKind,
    /// The old node's id (absent when added).
    pub old_id: Option<String>,
    /// The new node's id (absent when removed).
    pub new_id: Option<String>,
    /// The node kind (the new side's when on both).
    pub kind: String,
    /// The old side's authored position.
    pub old_pos: Option<[f32; 2]>,
    /// The new side's authored position.
    pub new_pos: Option<[f32; 2]>,
    /// The changed fields of a changed node's body (paths inside the node).
    pub fields: Vec<PathChange>,
    /// The authored position moved (layout only; not a change on its own).
    pub moved: bool,
}

impl NodeDiff {
    /// The id the node is shown by (the new id, else the old one).
    #[must_use]
    pub fn id(&self) -> &str {
        self.new_id.as_deref().or(self.old_id.as_deref()).unwrap_or("")
    }

    /// Whether a matched node changed its id (matched by kind and position).
    #[must_use]
    pub fn renamed(&self) -> bool {
        matches!((&self.old_id, &self.new_id), (Some(a), Some(b)) if a != b)
    }
}

/// One wire of a graph diff (ids are the new side's for added and unchanged wires, the old
/// side's for removed ones).
#[derive(Clone, Debug, PartialEq)]
pub struct WireDiff {
    /// Added, removed or unchanged.
    pub status: ChangeKind,
    /// The wire.
    pub wire: GraphWire,
}

/// The diff of two graph documents.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphDiff {
    /// Every node of both sides: matched and added ones in the new order, then removed ones.
    pub nodes: Vec<NodeDiff>,
    /// Every wire of both sides.
    pub wires: Vec<WireDiff>,
    /// Changes outside the nodes (parameters, outputs, shading…).
    pub document: Vec<PathChange>,
}

fn near(a: Option<[f32; 2]>, b: Option<[f32; 2]>) -> bool {
    match (a, b) {
        (Some(p), Some(q)) => (p[0] - q[0]).hypot(p[1] - q[1]) <= POSITION_MATCH,
        _ => false,
    }
}

/// Diffs two graph documents: nodes match by id, then the unmatched ones by kind and authored
/// position ([`POSITION_MATCH`]); a matched node is changed when its body or its id differs;
/// wires compare by endpoints after the old ids are mapped onto their matches.
#[must_use]
pub fn diff_graphs(old: &GraphDoc, new: &GraphDoc) -> GraphDiff {
    let _span = tracing::info_span!("asset_diff.graph").entered();
    let old_index: HashMap<&str, usize> = old
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    // new index → old index
    let mut matched: Vec<Option<usize>> = vec![None; new.nodes.len()];
    let mut used = vec![false; old.nodes.len()];
    for (j, n) in new.nodes.iter().enumerate() {
        if let Some(&i) = old_index.get(n.id.as_str()) {
            matched[j] = Some(i);
            used[i] = true;
        }
    }
    for (j, n) in new.nodes.iter().enumerate() {
        if matched[j].is_some() || n.pos.is_none() {
            continue;
        }
        let candidate = old
            .nodes
            .iter()
            .enumerate()
            .find(|(i, o)| !used[*i] && o.kind == n.kind && near(o.pos, n.pos));
        if let Some((i, _)) = candidate {
            matched[j] = Some(i);
            used[i] = true;
        }
    }
    let mut nodes = Vec::new();
    let mut rename: HashMap<&str, &str> = HashMap::new();
    for (j, n) in new.nodes.iter().enumerate() {
        match matched[j] {
            Some(i) => {
                let o = &old.nodes[i];
                rename.insert(o.id.as_str(), n.id.as_str());
                let fields = diff_values(&o.body, &n.body);
                let status = if fields.is_empty() && o.id == n.id {
                    ChangeKind::Unchanged
                } else {
                    ChangeKind::Changed
                };
                nodes.push(NodeDiff {
                    status,
                    old_id: Some(o.id.clone()),
                    new_id: Some(n.id.clone()),
                    kind: n.kind.clone(),
                    old_pos: o.pos,
                    new_pos: n.pos,
                    fields,
                    moved: o.pos.is_some() && n.pos.is_some() && !near(o.pos, n.pos),
                });
            }
            None => nodes.push(NodeDiff {
                status: ChangeKind::Added,
                old_id: None,
                new_id: Some(n.id.clone()),
                kind: n.kind.clone(),
                old_pos: None,
                new_pos: n.pos,
                fields: Vec::new(),
                moved: false,
            }),
        }
    }
    for (i, o) in old.nodes.iter().enumerate() {
        if !used[i] {
            nodes.push(NodeDiff {
                status: ChangeKind::Removed,
                old_id: Some(o.id.clone()),
                new_id: None,
                kind: o.kind.clone(),
                old_pos: o.pos,
                new_pos: None,
                fields: Vec::new(),
                moved: false,
            });
        }
    }
    let map = |id: &str| {
        rename
            .get(id)
            .map_or_else(|| id.to_string(), |n| (*n).to_string())
    };
    let mapped_old: Vec<(GraphWire, GraphWire)> = old
        .wires
        .iter()
        .map(|w| {
            (
                GraphWire {
                    from: map(&w.from),
                    to: map(&w.to),
                    slot: w.slot.clone(),
                },
                w.clone(),
            )
        })
        .collect();
    let old_set: HashSet<&GraphWire> = mapped_old.iter().map(|(m, _)| m).collect();
    let new_set: HashSet<&GraphWire> = new.wires.iter().collect();
    let mut wires = Vec::new();
    for w in &new.wires {
        wires.push(WireDiff {
            status: if old_set.contains(w) {
                ChangeKind::Unchanged
            } else {
                ChangeKind::Added
            },
            wire: w.clone(),
        });
    }
    for (m, original) in &mapped_old {
        if !new_set.contains(m) {
            wires.push(WireDiff {
                status: ChangeKind::Removed,
                wire: original.clone(),
            });
        }
    }
    GraphDiff {
        nodes,
        wires,
        document: diff_values(&old.rest, &new.rest),
    }
}

// ---------------------------------------------------------------------------
// Asset diff
// ---------------------------------------------------------------------------

/// What a diff found.
#[derive(Clone, Debug, PartialEq)]
pub enum DiffBody {
    /// A graph diff (material, animation or logic graph).
    Graph(GraphDiff),
    /// A tree diff of any other RON document, with both trees for the two-column view.
    Tree {
        /// The changed paths.
        changes: Vec<PathChange>,
        /// The old document.
        old: RonValue,
        /// The new document.
        new: RonValue,
    },
}

/// One entry of a diff's change list (what the panel lists and `editor.asset_diff` returns).
#[derive(Clone, Debug, PartialEq)]
pub enum ChangeEntry {
    /// A node was added, removed or changed (index into [`GraphDiff::nodes`]).
    Node(usize),
    /// A wire was added or removed (index into [`GraphDiff::wires`]).
    Wire(usize),
    /// A document path outside the nodes, or a tree diff path (index into the path list).
    Field(usize),
}

impl DiffBody {
    /// The document-level path changes (the tree diff's changes, a graph's non-node changes).
    #[must_use]
    pub fn fields(&self) -> &[PathChange] {
        match self {
            Self::Graph(g) => &g.document,
            Self::Tree { changes, .. } => changes,
        }
    }

    /// The change list: changed nodes, then changed wires, then document paths.
    #[must_use]
    pub fn entries(&self) -> Vec<ChangeEntry> {
        let mut out = Vec::new();
        if let Self::Graph(g) = self {
            out.extend(
                g.nodes
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| n.status != ChangeKind::Unchanged)
                    .map(|(i, _)| ChangeEntry::Node(i)),
            );
            out.extend(
                g.wires
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| w.status != ChangeKind::Unchanged)
                    .map(|(i, _)| ChangeEntry::Wire(i)),
            );
        }
        out.extend((0..self.fields().len()).map(ChangeEntry::Field));
        out
    }

    /// The status of one change list entry.
    #[must_use]
    pub fn status(&self, entry: &ChangeEntry) -> ChangeKind {
        match (self, entry) {
            (Self::Graph(g), ChangeEntry::Node(i)) => {
                g.nodes.get(*i).map_or(ChangeKind::Unchanged, |n| n.status)
            }
            (Self::Graph(g), ChangeEntry::Wire(i)) => {
                g.wires.get(*i).map_or(ChangeKind::Unchanged, |w| w.status)
            }
            (_, ChangeEntry::Field(i)) => self.fields().get(*i).map_or(ChangeKind::Unchanged, |f| f.kind),
            _ => ChangeKind::Unchanged,
        }
    }

    /// A one-line description of an entry ("Added node gloss (float)").
    #[must_use]
    pub fn describe(&self, entry: &ChangeEntry) -> String {
        let verb = |k: ChangeKind| match k {
            ChangeKind::Added => "Added",
            ChangeKind::Removed => "Removed",
            ChangeKind::Changed => "Changed",
            ChangeKind::Unchanged => "Unchanged",
        };
        match (self, entry) {
            (Self::Graph(g), ChangeEntry::Node(i)) => g.nodes.get(*i).map_or_else(String::new, |n| {
                let mut s = format!("{} node {} ({})", verb(n.status), n.id(), n.kind);
                if n.renamed() {
                    s.push_str(&format!(", was {}", n.old_id.as_deref().unwrap_or("")));
                }
                if n.status == ChangeKind::Changed && !n.fields.is_empty() {
                    let paths: Vec<&str> = n.fields.iter().take(3).map(|f| f.path.as_str()).collect();
                    let shown = if paths.iter().all(|p| p.is_empty()) {
                        "value".to_string()
                    } else {
                        paths.join(", ")
                    };
                    s.push_str(&format!(": {shown}"));
                    if n.fields.len() > 3 {
                        s.push_str(&format!(" and {} more", n.fields.len() - 3));
                    }
                }
                s
            }),
            (Self::Graph(g), ChangeEntry::Wire(i)) => g.wires.get(*i).map_or_else(String::new, |w| {
                format!(
                    "{} wire {} → {}.{}",
                    verb(w.status),
                    w.wire.from,
                    w.wire.to,
                    w.wire.slot
                )
            }),
            (_, ChangeEntry::Field(i)) => self.fields().get(*i).map_or_else(String::new, |f| {
                let path = if f.path.is_empty() {
                    "(document)"
                } else {
                    f.path.as_str()
                };
                format!("{} {path}", verb(f.kind))
            }),
            _ => String::new(),
        }
    }
}

/// One side of a diff.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffSide {
    /// The revision as asked (`HEAD`, a hash, a branch), or `None` for the working copy.
    pub rev: Option<String>,
    /// The full commit hash the revision resolved to (`None` for the working copy).
    pub hash: Option<String>,
    /// The path the file had on this side (project-relative; differs after a rename).
    pub path: String,
}

impl DiffSide {
    /// "HEAD (1a2b3c4)", "working copy".
    #[must_use]
    pub fn label(&self) -> String {
        match (&self.rev, &self.hash) {
            (Some(rev), Some(hash)) if rev.as_str() != hash.as_str() => {
                format!("{rev} ({})", &hash[..hash.len().min(9)])
            }
            (Some(rev), _) => rev[..rev.len().min(12)].to_string(),
            (None, _) => "working copy".to_string(),
        }
    }
}

/// The diff of one asset between two sides.
#[derive(Clone, Debug, PartialEq)]
pub struct AssetDiff {
    /// The project-relative path asked for.
    pub path: String,
    /// What the file is.
    pub kind: AssetKind,
    /// The old side.
    pub old: DiffSide,
    /// The new side.
    pub new: DiffSide,
    /// What changed.
    pub body: DiffBody,
}

/// Diffs two texts of a file of `kind` (and their layout sidecars, for graphs).
///
/// # Errors
/// `parse_failed` when a side is not valid RON or not a valid graph of its kind.
pub fn diff_texts(
    kind: AssetKind,
    old: &str,
    old_layout: Option<&str>,
    new: &str,
    new_layout: Option<&str>,
) -> Result<DiffBody, DiffError> {
    let _span = tracing::info_span!("asset_diff.texts", kind = kind.as_str()).entered();
    if kind.is_graph() {
        let a = graph_doc(kind, old, old_layout, "old")?;
        let b = graph_doc(kind, new, new_layout, "new")?;
        return Ok(DiffBody::Graph(diff_graphs(&a, &b)));
    }
    let a = parse_ron(old).map_err(|e| parse_failed("old", e))?;
    let b = parse_ron(new).map_err(|e| parse_failed("new", e))?;
    Ok(DiffBody::Tree {
        changes: diff_values(&a, &b),
        old: a,
        new: b,
    })
}

// ---------------------------------------------------------------------------
// Revisions
// ---------------------------------------------------------------------------

/// One entry of a file's history (`git log --follow`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    /// Full commit hash.
    pub hash: String,
    /// Abbreviated hash.
    pub short: String,
    /// Commit date, `YYYY-MM-DD`.
    pub date: String,
    /// Author name.
    pub author: String,
    /// First line of the message.
    pub subject: String,
    /// The file's path in that commit, relative to the project root (it differs before a rename).
    pub path: String,
}

/// How [`asset_revisions`] and [`diff_asset`] run git: the program and, for tests, a ceiling
/// directory above which git does not look for a repository.
#[derive(Clone, Debug)]
pub struct Git {
    /// The program (`git`).
    pub program: OsString,
    /// `GIT_CEILING_DIRECTORIES` for the commands (`None`: inherit).
    pub ceiling: Option<PathBuf>,
}

impl Default for Git {
    fn default() -> Self {
        Self {
            program: "git".into(),
            ceiling: None,
        }
    }
}

/// Most entries the revision picker lists.
pub const MAX_REVISIONS: usize = 200;

/// Whether `rev` is a revision we pass to git: no option-like or range forms, no path part,
/// no whitespace or control characters.
#[must_use]
pub fn valid_revision(rev: &str) -> bool {
    !rev.is_empty()
        && rev.len() <= 200
        && !rev.starts_with('-')
        && !rev.contains("..")
        && rev.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~' | '^' | '@' | '{' | '}')
        })
}

impl Git {
    fn command(&self, root: &Path) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.arg("-C").arg(root);
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_PREFIX",
        ] {
            cmd.env_remove(var);
        }
        cmd.env("GIT_TERMINAL_PROMPT", "0").env("GIT_OPTIONAL_LOCKS", "0");
        if let Some(ceiling) = &self.ceiling {
            cmd.env("GIT_CEILING_DIRECTORIES", ceiling);
        }
        cmd.stdin(std::process::Stdio::null());
        cmd
    }

    fn run(&self, root: &Path, args: &[&str]) -> Result<std::process::Output, DiffError> {
        self.command(root).args(args).output().map_err(|e| {
            DiffError::new(
                "no_revision_control",
                format!("No revision control (cannot run git: {e})"),
            )
        })
    }

    /// The project root's path inside its work tree (`""` at the top, `game/` below it).
    ///
    /// # Errors
    /// `no_revision_control` outside a git work tree or without git.
    pub fn prefix(&self, root: &Path) -> Result<String, DiffError> {
        let out = self.run(root, &["rev-parse", "--is-inside-work-tree", "--show-prefix"])?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        if !out.status.success() || lines.next() != Some("true") {
            return Err(DiffError::new(
                "no_revision_control",
                "No revision control: the project is not in a git work tree",
            ));
        }
        Ok(lines.next().unwrap_or("").to_string())
    }

    /// The full hash of commit `rev`.
    ///
    /// # Errors
    /// `bad_revision`.
    pub fn resolve(&self, root: &Path, rev: &str) -> Result<String, DiffError> {
        if !valid_revision(rev) {
            return Err(DiffError::new(
                "bad_revision",
                format!("`{rev}` is not a revision name"),
            ));
        }
        let spec = format!("{rev}^{{commit}}");
        let out = self.run(root, &["rev-parse", "--verify", "--quiet", &spec])?;
        let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || hash.is_empty() {
            return Err(DiffError::new("bad_revision", format!("`{rev}` names no commit")));
        }
        Ok(hash)
    }

    /// The text of `full` (a work-tree-relative path) in commit `hash`; `None` when the file is
    /// not in that commit.
    ///
    /// # Errors
    /// `parse_failed` for a file that is not UTF-8 text.
    pub fn show(&self, root: &Path, hash: &str, full: &str) -> Result<Option<String>, DiffError> {
        let spec = format!("{hash}:{full}");
        let out = self.run(root, &["show", &spec])?;
        if !out.status.success() {
            return Ok(None);
        }
        String::from_utf8(out.stdout)
            .map(Some)
            .map_err(|_| DiffError::new("parse_failed", format!("{full} at {hash} is not UTF-8 text")))
    }

    /// The history of `rel` (project-relative), newest first, following renames.
    ///
    /// # Errors
    /// `no_revision_control`, `git_failed`.
    pub fn revisions(&self, root: &Path, rel: &str, limit: usize) -> Result<Vec<Revision>, DiffError> {
        let prefix = self.prefix(root)?;
        let limit = format!("-n{}", limit.clamp(1, MAX_REVISIONS));
        let out = self.run(
            root,
            &[
                "-c",
                "core.quotepath=off",
                "log",
                "--follow",
                &limit,
                "--date=short",
                "--format=%x1e%H%x1f%h%x1f%ad%x1f%an%x1f%s",
                "--name-only",
                "--",
                rel,
            ],
        )?;
        if !out.status.success() {
            return Err(DiffError::new(
                "git_failed",
                format!("git log failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut list = Vec::new();
        for record in text.split('\u{1e}').filter(|r| !r.trim().is_empty()) {
            let mut lines = record.lines();
            let header = lines.next().unwrap_or("");
            let parts: Vec<&str> = header.split('\u{1f}').collect();
            let [hash, short, date, author, subject] = parts[..] else {
                continue;
            };
            let full = lines.rfind(|l| !l.trim().is_empty()).unwrap_or(rel);
            let path = full.strip_prefix(prefix.as_str()).unwrap_or(full).to_string();
            list.push(Revision {
                hash: hash.into(),
                short: short.into(),
                date: date.into(),
                author: author.into(),
                subject: subject.into(),
                path,
            });
        }
        Ok(list)
    }
}

/// `path` (project-relative, or absolute under `root`) as a checked project-relative path with
/// `/` separators.
///
/// # Errors
/// `bad_path` for an empty path, `path_outside_project` for one that leads outside `root`.
pub fn project_relative(root: &Path, path: &str) -> Result<String, DiffError> {
    if path.trim().is_empty() {
        return Err(DiffError::new("bad_path", "the path is empty"));
    }
    let as_path = Path::new(path);
    let rel = if as_path.is_absolute() {
        let croot = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let stripped = as_path
            .strip_prefix(root)
            .or_else(|_| as_path.strip_prefix(&croot))
            .map_err(|_| {
                DiffError::new(
                    "path_outside_project",
                    format!("{path} is not inside the project"),
                )
            })?;
        stripped
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    } else {
        path.replace('\\', "/")
    };
    regia_core::project_path::resolve_write(root, &rel)
        .map_err(|e| DiffError::new("path_outside_project", format!("{path}: {e}")))?;
    let checked = regia_core::project_path::check_relative(&rel)
        .map_err(|e| DiffError::new("path_outside_project", format!("{path}: {e}")))?;
    Ok(checked
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/"))
}

/// The project that holds `file`: the nearest ancestor with a project manifest (`Regia.toml`, or
/// the older `Ferrite.toml`).
#[must_use]
pub fn project_root_of(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .find(|dir| dir.join("Regia.toml").is_file() || dir.join("Ferrite.toml").is_file())
        .map(Path::to_path_buf)
}

/// The history of the asset at `path` (see [`Git::revisions`]).
///
/// # Errors
/// As [`project_relative`] and [`Git::revisions`].
pub fn asset_revisions(root: &Path, path: &str, limit: usize) -> Result<Vec<Revision>, DiffError> {
    asset_revisions_with(&Git::default(), root, path, limit)
}

/// [`asset_revisions`] with a given [`Git`].
///
/// # Errors
/// As [`asset_revisions`].
pub fn asset_revisions_with(
    git: &Git,
    root: &Path,
    path: &str,
    limit: usize,
) -> Result<Vec<Revision>, DiffError> {
    let rel = project_relative(root, path)?;
    git.revisions(root, &rel, limit)
}

/// One side's text and layout sidecar.
struct SideText {
    side: DiffSide,
    text: String,
    layout: Option<String>,
}

fn read_side(
    git: &Git,
    root: &Path,
    rel: &str,
    kind: AssetKind,
    rev: Option<&str>,
) -> Result<SideText, DiffError> {
    let Some(rev) = rev else {
        let file = root.join(rel);
        let text = std::fs::read_to_string(&file)
            .map_err(|e| DiffError::new("read_failed", format!("cannot read {rel}: {e}")))?;
        let layout = kind
            .has_layout_sidecar()
            .then(|| std::fs::read_to_string(root.join(layout_sidecar(rel))).ok())
            .flatten();
        return Ok(SideText {
            side: DiffSide {
                rev: None,
                hash: None,
                path: rel.to_string(),
            },
            text,
            layout,
        });
    };
    let prefix = git.prefix(root)?;
    let hash = git.resolve(root, rev)?;
    let mut path = rel.to_string();
    let mut text = git.show(root, &hash, &format!("{prefix}{path}"))?;
    if text.is_none() {
        // Renamed since: the history names the file's path in that commit.
        if let Some(older) = git
            .revisions(root, rel, MAX_REVISIONS)?
            .into_iter()
            .find(|r| r.hash == hash)
        {
            path = older.path;
            text = git.show(root, &hash, &format!("{prefix}{path}"))?;
        }
    }
    let text =
        text.ok_or_else(|| DiffError::new("not_in_revision", format!("{rel} is not in revision {rev}")))?;
    let layout = if kind.has_layout_sidecar() {
        git.show(root, &hash, &format!("{prefix}{}", layout_sidecar(&path)))
            .ok()
            .flatten()
    } else {
        None
    };
    Ok(SideText {
        side: DiffSide {
            rev: Some(rev.to_string()),
            hash: Some(hash),
            path,
        },
        text,
        layout,
    })
}

/// Diffs the asset at `path` (project-relative, or absolute inside `root`) between revision
/// `rev_a` (default `HEAD`) and `rev_b` (default: the working copy). Read-only: runs
/// `git rev-parse`, `git show` and, after a rename, `git log --follow`.
///
/// The revision control panel and `editor.asset_diff` call this.
///
/// # Errors
/// See [`DiffError`].
pub fn diff_asset(
    root: &Path,
    path: &str,
    rev_a: Option<&str>,
    rev_b: Option<&str>,
) -> Result<AssetDiff, DiffError> {
    diff_asset_with(&Git::default(), root, path, rev_a, rev_b)
}

/// [`diff_asset`] with a given [`Git`].
///
/// # Errors
/// As [`diff_asset`].
pub fn diff_asset_with(
    git: &Git,
    root: &Path,
    path: &str,
    rev_a: Option<&str>,
    rev_b: Option<&str>,
) -> Result<AssetDiff, DiffError> {
    let _span = tracing::info_span!("asset_diff.asset").entered();
    let rel = project_relative(root, path)?;
    let kind = AssetKind::of_path(&rel)
        .ok_or_else(|| DiffError::new("unsupported_asset", format!("{rel} is not a RON asset")))?;
    let old = read_side(git, root, &rel, kind, Some(rev_a.unwrap_or("HEAD")))?;
    let new = read_side(git, root, &rel, kind, rev_b)?;
    let body = diff_texts(
        kind,
        &old.text,
        old.layout.as_deref(),
        &new.text,
        new.layout.as_deref(),
    )?;
    Ok(AssetDiff {
        path: rel,
        kind,
        old: old.side,
        new: new.side,
        body,
    })
}

// ---------------------------------------------------------------------------
// JSON (agent protocol)
// ---------------------------------------------------------------------------

fn path_change_json(c: &PathChange) -> Value {
    json!({
        "path": c.path,
        "status": c.kind.as_str(),
        "old": c.old.as_ref().map(|v| v.summary(200)),
        "new": c.new.as_ref().map(|v| v.summary(200)),
    })
}

impl AssetDiff {
    /// The `editor.asset_diff` result: `{ path, kind, rev_a, rev_b, hash_a, hash_b, path_a, path_b,
    /// counts: {added, removed, changed}, changes: [..] }`. A change is
    /// `{ target: "node", status, id, old_id, kind, fields: [{path, status, old, new}] }`,
    /// `{ target: "wire", status, from, to, slot }` or `{ target: "field", status, path, old, new }`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut counts = BTreeMap::from([("added", 0u32), ("removed", 0), ("changed", 0)]);
        let mut changes = Vec::new();
        for entry in self.body.entries() {
            let status = self.body.status(&entry);
            if let Some(c) = counts.get_mut(status.as_str()) {
                *c += 1;
            }
            changes.push(match (&self.body, &entry) {
                (DiffBody::Graph(g), ChangeEntry::Node(i)) => {
                    let n = &g.nodes[*i];
                    json!({
                        "target": "node",
                        "status": status.as_str(),
                        "id": n.id(),
                        "old_id": n.old_id,
                        "kind": n.kind,
                        "fields": n.fields.iter().map(path_change_json).collect::<Vec<_>>(),
                    })
                }
                (DiffBody::Graph(g), ChangeEntry::Wire(i)) => {
                    let w = &g.wires[*i];
                    json!({
                        "target": "wire",
                        "status": status.as_str(),
                        "from": w.wire.from,
                        "to": w.wire.to,
                        "slot": w.wire.slot,
                    })
                }
                (body, ChangeEntry::Field(i)) => {
                    let mut v = path_change_json(&body.fields()[*i]);
                    v["target"] = json!("field");
                    v
                }
                _ => continue,
            });
        }
        json!({
            "path": self.path,
            "kind": self.kind.as_str(),
            "rev_a": self.old.rev,
            "rev_b": self.new.rev,
            "hash_a": self.old.hash,
            "hash_b": self.new.hash,
            "path_a": self.old.path,
            "path_b": self.new.path,
            "counts": counts,
            "changes": changes,
        })
    }
}

impl Revision {
    /// `{ hash, short, date, author, subject, path }`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "hash": self.hash,
            "short": self.short,
            "date": self.date,
            "author": self.author,
            "subject": self.subject,
            "path": self.path,
        })
    }
}

fn rpc(err: DiffError) -> RpcError {
    let (code, hint) = match err.code {
        "no_project" | "not_in_revision" | "read_failed" => (
            protocol::NOT_FOUND,
            "check the path with editor.list_assets and the revision with editor.asset_revisions",
        ),
        "no_revision_control" | "git_failed" => (
            protocol::INTERNAL_ERROR,
            "the project must be inside a git work tree and git must be on PATH",
        ),
        _ => (
            protocol::INVALID_PARAMS,
            "see book/agents/asset-diff.md for the parameters",
        ),
    };
    RpcError {
        code,
        message: err.message,
        data: Some(json!({ "reason": err.code, "hint": hint })),
    }
}

fn root_and_path(world: &World, params: &Value) -> Result<(PathBuf, String), DiffError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| DiffError::new("bad_path", "`path` (project-relative) is required"))?;
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| DiffError::new("no_project", "no project is open"))?;
    Ok((root, path.to_string()))
}

fn opt_str<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, DiffError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(DiffError::new(
            "bad_revision",
            format!("`{key}` must be a string or null"),
        )),
    }
}

fn editor_asset_diff(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (root, path) = root_and_path(world, params).map_err(rpc)?;
    let rev_a = opt_str(params, "rev_a").map_err(rpc)?;
    let rev_b = opt_str(params, "rev_b").map_err(rpc)?;
    diff_asset(&root, &path, rev_a, rev_b)
        .map(|d| d.to_json())
        .map_err(rpc)
}

fn editor_asset_revisions(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (root, path) = root_and_path(world, params).map_err(rpc)?;
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(50, |n| usize::try_from(n).unwrap_or(MAX_REVISIONS));
    let list = asset_revisions(&root, &path, limit).map_err(rpc)?;
    Ok(json!({ "path": path, "revisions": list.iter().map(Revision::to_json).collect::<Vec<_>>() }))
}

/// The asset diff methods (`editor.asset_diff`, `editor.asset_revisions`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.asset_diff",
                tool: "asset_diff",
                description: "Diff an asset against a git revision (read-only): material, animation and logic \
                    graphs by node (matched by id, then kind and position) and wire, any other .ron as a tree of \
                    changed field paths. rev_a defaults to HEAD, rev_b to the working copy.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative asset path, e.g. assets/materials/hull.matgraph.ron.",
                    },
                    ParamSchema {
                        name: "rev_a",
                        kind: "string",
                        required: false,
                        description: "The old revision (a commit, branch, tag or HEAD~n). Default HEAD.",
                    },
                    ParamSchema {
                        name: "rev_b",
                        kind: "string",
                        required: false,
                        description: "The new revision. Default: the working copy on disk.",
                    },
                ],
                result: "{ path, kind, rev_a, rev_b, hash_a, hash_b, path_a, path_b, counts: {added, removed, \
                    changed}, changes: [{target: node|wire|field, status, ...}] }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_asset_diff as Handler,
        ),
        (
            MethodSchema {
                name: "editor.asset_revisions",
                tool: "asset_revisions",
                description: "List the git history of an asset (newest first, following renames): what the \
                    Diff against revision… picker shows.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative asset path.",
                    },
                    ParamSchema {
                        name: "limit",
                        kind: "integer",
                        required: false,
                        description: "Most entries to return (default 50, at most 200).",
                    },
                ],
                result: "{ path, revisions: [{hash, short, date, author, subject, path}] }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_asset_revisions as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests;
