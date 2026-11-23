// SPDX-License-Identifier: MPL-2.0
//! `agent-examples`: the JSON examples in the agent guide are checked against the protocol.
//!
//! Every ```` ```json ```` fence in `book/agents/*.md`, `book/user-guide/agent-protocol.md` and the
//! bundled `agent.guide` topic texts is split into statements and each statement is read as one of:
//!
//! - a JSON-RPC request (`{ "jsonrpc": .., "method": .., "params": .. }`),
//! - `method.name { params }` (the prose shorthand; a bare `method.name` means no params),
//! - a bare params object, bound to the known method named in backticks between the previous
//!   heading and the fence whose declared params cover most of the object's keys (the nearest
//!   mention on a tie, or when none covers a key),
//! - a response (a statement after `→` or `->`, or a JSON-RPC object with `result`).
//!
//! Requests are checked against [`regia_agent::schema::METHODS`] and the built-in extension rows
//! (curve tables, choosers, abilities, cloth, breadcrumbs, trace): the method exists, every param is
//! declared, required params are present and each value's JSON kind matches the declared `kind` (the
//! mapping `contract.rs` tests use: `integer` is a whole JSON number, `any` matches everything; an
//! `array` param whose description says a lone number works also takes a number, and an `object`
//! param whose description gives `true` as a value also takes a boolean).
//! A response's top-level keys must be named in the method's `result` description when that starts
//! with `{`. `editor.*` methods are host handlers outside the runtime schema; they are counted and skipped.
//!
//! Fence rules (also in `book/agents/protocol.md`): a fence tagged `json,ignore`, or preceded by
//! `<!-- not-a-request -->`, is skipped and counted. `...` (or `…`) stands for elided content: it is
//! accepted as an object entry, a value or an array element, the quoted forms `"..."` and
//! `"...": "..."` mean the same, and a params object that holds one is not checked for missing
//! required params. `//` comments and trailing commas are accepted. A fence that is a
//! fragment with no resolvable method is counted, not failed.
//!
//! Findings that cannot be fixed live in `docs/ci/agent-examples.toml` (`format_version = 1`, one row
//! per finding with a reason). A finding with no row fails; a row with no finding fails.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use regia_agent::schema::{self, MethodSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Where the baseline lives.
pub const BASELINE_PATH: &str = "docs/ci/agent-examples.toml";

/// The class of one finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// The statement is not JSON (even leniently).
    Unparseable,
    /// The method is in neither the schema nor the built-in extension rows.
    UnknownMethod,
    /// A param the method does not declare.
    UnknownParam,
    /// A required param is absent.
    MissingRequired,
    /// A value's JSON kind differs from the declared `kind`.
    KindMismatch,
    /// A response key the method's `result` description does not name.
    UnknownResultKey,
}

impl Class {
    /// Every class, in report order.
    pub const ALL: [Class; 6] = [
        Class::Unparseable,
        Class::UnknownMethod,
        Class::UnknownParam,
        Class::MissingRequired,
        Class::KindMismatch,
        Class::UnknownResultKey,
    ];

    /// The name used in the baseline and the report.
    pub fn name(self) -> &'static str {
        match self {
            Class::Unparseable => "unparseable",
            Class::UnknownMethod => "unknown_method",
            Class::UnknownParam => "unknown_param",
            Class::MissingRequired => "missing_required",
            Class::KindMismatch => "kind_mismatch",
            Class::UnknownResultKey => "unknown_result_key",
        }
    }
}

/// One problem in one example.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Repo-relative file (or `crates/regia_agent/src/discovery.rs` for guide topics).
    pub file: String,
    /// 1-based line of the statement.
    pub line: usize,
    /// What kind of problem.
    pub class: Class,
    /// Stable identity inside the file, e.g. `scene.spawn:radius`. The baseline matches on it.
    pub key: String,
    /// Human text.
    pub message: String,
}

/// What a scan counted besides findings.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Counts {
    /// `json` fences seen.
    pub fences: usize,
    /// Statements checked as requests.
    pub requests: usize,
    /// Statements checked as responses.
    pub responses: usize,
    /// Fences skipped by `json,ignore` or `<!-- not-a-request -->`.
    pub skipped_fences: usize,
    /// Statements for `editor.*` methods (host handlers, not in the runtime schema).
    pub editor_skipped: usize,
    /// Bare objects with no resolvable method, and responses with no request.
    pub fragments: usize,
}

/// The methods examples are checked against.
pub struct Protocol {
    methods: BTreeMap<String, MethodSchema>,
}

impl Protocol {
    /// The runtime schema plus the built-in extension rows.
    pub fn current() -> Self {
        let mut methods: BTreeMap<String, MethodSchema> = schema::METHODS
            .iter()
            .map(|m| (m.name.to_string(), m.clone()))
            .collect();
        let mut world = bevy_ecs::world::World::new();
        regia_agent::install_builtin_extensions(&mut world);
        if let Some(ext) = world.get_resource::<regia_agent::extension::AgentExtension>() {
            for name in ext.method_names() {
                if let Some(row) = ext.method_schema(name) {
                    methods.entry(name.to_string()).or_insert_with(|| row.clone());
                }
            }
        }
        Protocol { methods }
    }

    /// A protocol of the given rows (tests).
    pub fn from_rows(rows: &[MethodSchema]) -> Self {
        Protocol {
            methods: rows.iter().map(|m| (m.name.to_string(), m.clone())).collect(),
        }
    }

    fn get(&self, name: &str) -> Option<&MethodSchema> {
        self.methods.get(name)
    }
}

// ---------------------------------------------------------------------------------------------
// Lenient JSON

/// A parsed value plus whether `...` appeared directly in it (an object entry or array element).
struct Lenient<'a> {
    src: &'a [u8],
    pos: usize,
    /// Set when a `...` was accepted at the top object level being parsed.
    elided_top: bool,
    depth: usize,
}

impl<'a> Lenient<'a> {
    fn new(text: &'a str) -> Self {
        Lenient {
            src: text.as_bytes(),
            pos: 0,
            elided_top: false,
            depth: 0,
        }
    }

    fn ws(&mut self) {
        loop {
            while self.pos < self.src.len() && self.src[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            if self.src[self.pos..].starts_with(b"//") {
                while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn eat_ellipsis(&mut self) -> bool {
        self.ws();
        if self.src[self.pos..].starts_with(b"...") {
            self.pos += 3;
            true
        } else if self.src[self.pos..].starts_with("…".as_bytes()) {
            self.pos += "…".len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        let Some(&c) = self.src.get(self.pos) else {
            return Err("unexpected end".into());
        };
        match c {
            b'{' => {
                self.depth += 1;
                if self.depth > 64 {
                    return Err("nested too deep".into());
                }
                let top = self.depth == 1;
                self.pos += 1;
                let mut map = Map::new();
                loop {
                    if self.eat_ellipsis() {
                        if top {
                            self.elided_top = true;
                        }
                    } else {
                        self.ws();
                        match self.src.get(self.pos) {
                            Some(b'}') => {
                                self.pos += 1;
                                break;
                            }
                            Some(b'"') => {
                                let Value::String(key) = self.string()? else {
                                    return Err("bad key".into());
                                };
                                self.ws();
                                if self.src.get(self.pos) != Some(&b':') {
                                    return Err(format!("expected `:` after \"{key}\""));
                                }
                                self.pos += 1;
                                let v = self.value()?;
                                if is_elision_text(&key) {
                                    // `"...": ...`: an elided entry, like a bare `...`.
                                    if top {
                                        self.elided_top = true;
                                    }
                                } else {
                                    map.insert(key, v);
                                }
                            }
                            _ => return Err(format!("expected a key at byte {}", self.pos)),
                        }
                    }
                    self.ws();
                    match self.src.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            break;
                        }
                        _ => return Err(format!("expected `,` or `}}` at byte {}", self.pos)),
                    }
                }
                self.depth -= 1;
                Ok(Value::Object(map))
            }
            b'[' => {
                self.depth += 1;
                if self.depth > 64 {
                    return Err("nested too deep".into());
                }
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    if !self.eat_ellipsis() {
                        self.ws();
                        if self.src.get(self.pos) == Some(&b']') {
                            self.pos += 1;
                            break;
                        }
                        let v = self.value()?;
                        if !is_elision(&v) {
                            items.push(v);
                        }
                    }
                    self.ws();
                    match self.src.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            break;
                        }
                        _ => return Err(format!("expected `,` or `]` at byte {}", self.pos)),
                    }
                }
                self.depth -= 1;
                Ok(Value::Array(items))
            }
            b'"' => self.string(),
            _ if self.src[self.pos..].starts_with("…".as_bytes())
                || self.src[self.pos..].starts_with(b"...") =>
            {
                // `"key": …`: an elided value, read as the string `…` (no kind is checked on it).
                self.pos += if self.src[self.pos] == b'.' {
                    3
                } else {
                    "…".len()
                };
                Ok(Value::String("…".into()))
            }
            _ => {
                let start = self.pos;
                while self.pos < self.src.len()
                    && matches!(self.src[self.pos], b'-' | b'+' | b'.' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
                {
                    self.pos += 1;
                }
                let word = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("");
                match word {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" => Ok(Value::Null),
                    _ => serde_json::from_str::<serde_json::Number>(word)
                        .map(Value::Number)
                        .map_err(|_| format!("unexpected `{word}` at byte {start}")),
                }
            }
        }
    }

    fn string(&mut self) -> Result<Value, String> {
        let start = self.pos;
        self.pos += 1;
        while self.pos < self.src.len() {
            match self.src[self.pos] {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    let raw = std::str::from_utf8(&self.src[start..self.pos]).map_err(|e| e.to_string())?;
                    return serde_json::from_str(raw).map_err(|e| e.to_string());
                }
                _ => self.pos += 1,
            }
        }
        Err("unterminated string".into())
    }
}

/// Whether `text` is an elision mark: `...` or `…`.
fn is_elision_text(text: &str) -> bool {
    matches!(text.trim(), "..." | "…")
}

/// Whether `value` is an elided value: a bare `...` / `…`, or the string `"..."` / `"…"`.
fn is_elision(value: &Value) -> bool {
    value.as_str().is_some_and(is_elision_text)
}

/// Parses `text` (one value, nothing after it) leniently. The flag is whether a `...` entry sat
/// directly in the top-level object.
fn parse_lenient(text: &str) -> Result<(Value, bool), String> {
    let mut p = Lenient::new(text);
    let v = p.value()?;
    p.ws();
    if p.pos < p.src.len() {
        return Err(format!("text after the value at byte {}", p.pos));
    }
    Ok((v, p.elided_top))
}

// ---------------------------------------------------------------------------------------------
// Markdown walking

/// One fenced block.
struct Fence<'a> {
    info: &'a str,
    /// Line number (1-based) of the first body line.
    first_line: usize,
    body: Vec<&'a str>,
    /// Index of the opening line in the file's line list.
    open_idx: usize,
}

fn fences<'a>(lines: &[&'a str]) -> Vec<Fence<'a>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let ticks = line.chars().take_while(|c| *c == '`').count();
        if ticks >= 3 && !line.trim_start().starts_with("````` ") {
            let info = line[ticks..].trim();
            let mut j = i + 1;
            while j < lines.len() {
                let t = lines[j].trim_end();
                let closing = t.chars().take_while(|c| *c == '`').count();
                if closing >= ticks && t.len() == closing {
                    break;
                }
                j += 1;
            }
            out.push(Fence {
                info,
                first_line: i + 2,
                body: lines[(i + 1).min(lines.len())..j.min(lines.len())].to_vec(),
                open_idx: i,
            });
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

fn is_json_fence(info: &str) -> bool {
    info == "json" || info.starts_with("json,") || info.starts_with("json ")
}

fn is_ignored(f: &Fence<'_>, lines: &[&str]) -> bool {
    if f.info.split([',', ' ']).any(|t| t == "ignore") {
        return true;
    }
    lines[..f.open_idx]
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.contains("<!-- not-a-request -->"))
}

/// A method-shaped token: lowercase words joined by `.` (at least one dot).
fn method_token(s: &str) -> Option<&str> {
    let end = s
        .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.'))
        .unwrap_or(s.len());
    let tok = &s[..end];
    (tok.contains('.')
        && !tok.starts_with('.')
        && !tok.ends_with('.')
        && !tok.contains("..")
        && tok
            .split('.')
            .all(|p| p.starts_with(|c: char| c.is_ascii_lowercase())))
    .then_some(tok)
}

/// The known methods named in backticks between the previous heading (included) and the fence,
/// nearest first: lines bottom-up, and on one line the last mention first. Each method once.
fn context_methods(lines: &[&str], open_idx: usize, proto: &Protocol) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in lines[..open_idx].iter().rev() {
        let mut on_line: Vec<&str> = line
            .split('`')
            .skip(1)
            .step_by(2)
            .filter_map(|span| method_token(span.trim()))
            .filter(|tok| proto.get(tok).is_some() || tok.starts_with("editor."))
            .collect();
        on_line.reverse();
        for tok in on_line {
            if !out.iter().any(|m| m == tok) {
                out.push(tok.to_string());
            }
        }
        if line.starts_with('#') {
            break;
        }
    }
    out
}

/// The method a bare object is bound to: among `candidates` (nearest first), the one whose
/// declared params cover the most of the object's keys; the nearest on a tie, and the nearest
/// when none covers a key (the object is then read as that method's result, if at all).
fn bind_bare<'c>(value: &Value, candidates: &'c [String], proto: &Protocol) -> Option<&'c str> {
    let keys = value.as_object();
    let mut best: Option<(&str, usize)> = None;
    for name in candidates {
        let covered = match (keys, proto.get(name)) {
            (Some(map), Some(row)) => map
                .keys()
                .filter(|k| row.params.iter().any(|p| p.name == k.as_str()))
                .count(),
            _ => 0,
        };
        if best.is_none_or(|(_, n)| covered > n) {
            best = Some((name.as_str(), covered));
        }
    }
    best.map(|(name, _)| name)
}

/// Splits a fence body into statements: `(line, text)`, closing on bracket depth 0. Comment-only
/// lines and blank lines at depth 0 are dropped.
fn statements(f: &Fence<'_>) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0;
    let mut depth: i32 = 0;
    for (n, raw) in f.body.iter().enumerate() {
        let line = f.first_line + n;
        let mut in_str = false;
        let mut esc = false;
        let bytes = raw.as_bytes();
        let mut k = 0;
        let mut code_end = raw.len();
        let mut arrow_at = None;
        while k < bytes.len() {
            let c = bytes[k];
            if in_str {
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => depth -= 1,
                    b'-' if depth == 0 && bytes.get(k + 1) == Some(&b'>') && !raw[..k].trim().is_empty() => {
                        arrow_at = Some(k);
                        break;
                    }
                    0xE2 if depth == 0 && raw[k..].starts_with('→') && !raw[..k].trim().is_empty() => {
                        arrow_at = Some(k);
                        break;
                    }
                    b'/' if bytes.get(k + 1) == Some(&b'/') => {
                        code_end = k;
                        break;
                    }
                    _ => {}
                }
            }
            k += 1;
        }
        if let Some(at) = arrow_at {
            // `call {..} → {..}` on one line: two statements.
            if cur.is_empty() {
                start = line;
            }
            cur.push_str(raw[..at].trim_end());
            cur.push('\n');
            out.push((start, std::mem::take(&mut cur)));
            start = line;
            let rest = raw[at..].trim_end();
            let rest = rest.split(" // ").next().unwrap_or(rest);
            cur.push_str(rest);
            cur.push('\n');
            // Recount depth for the response part.
            depth = 0;
            let mut in_s = false;
            let mut esc2 = false;
            for c in rest.bytes() {
                if in_s {
                    if esc2 {
                        esc2 = false;
                    } else if c == b'\\' {
                        esc2 = true;
                    } else if c == b'"' {
                        in_s = false;
                    }
                } else {
                    match c {
                        b'"' => in_s = true,
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => depth -= 1,
                        _ => {}
                    }
                }
            }
            if depth <= 0 {
                out.push((start, std::mem::take(&mut cur)));
                depth = 0;
            }
            continue;
        }
        let code = raw[..code_end].trim_end();
        if cur.is_empty() {
            if code.trim().is_empty() {
                continue;
            }
            start = line;
        }
        cur.push_str(code);
        cur.push('\n');
        if depth <= 0 {
            out.push((start, std::mem::take(&mut cur)));
            depth = 0;
        }
    }
    if !cur.trim().is_empty() {
        out.push((start, cur));
    }
    out
}

enum Stmt {
    Request {
        method: String,
        params: Value,
        elided: bool,
    },
    Response(Value),
    /// A bare object under a method heading: params when it names a declared param, else a result.
    Bare {
        method: String,
        value: Value,
        elided: bool,
    },
    Fragment,
    Bad(String),
}

fn classify(text: &str, ctx: &[String], proto: &Protocol) -> Stmt {
    let t = text.trim();
    // A response: after an arrow.
    for arrow in ["→", "->"] {
        if let Some(rest) = t.strip_prefix(arrow) {
            // A response is an illustration (prose, `...`, a formula): when it is not JSON it is
            // counted as a fragment, not a finding.
            return match parse_lenient(rest.trim()) {
                Ok((v, _)) => Stmt::Response(v),
                Err(_) => Stmt::Fragment,
            };
        }
    }
    if t.starts_with('{') {
        return match parse_lenient(t) {
            Ok((v, elided)) => {
                if let Some(method) = v.get("method").and_then(Value::as_str) {
                    let params = v
                        .get("params")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    return Stmt::Request {
                        method: method.to_string(),
                        params,
                        elided,
                    };
                }
                if v.get("result").is_some() || v.get("error").is_some() {
                    return Stmt::Response(v);
                }
                match bind_bare(&v, ctx, proto) {
                    Some(m) => Stmt::Bare {
                        method: m.to_string(),
                        value: v,
                        elided,
                    },
                    None => Stmt::Fragment,
                }
            }
            Err(e) => Stmt::Bad(e),
        };
    }
    if let Some(method) = method_token(t) {
        let rest = t[method.len()..].trim();
        if rest.is_empty() {
            return Stmt::Request {
                method: method.to_string(),
                params: Value::Object(Map::new()),
                elided: false,
            };
        }
        if rest.starts_with('{') {
            return match parse_lenient(rest) {
                Ok((params, elided)) => Stmt::Request {
                    method: method.to_string(),
                    params,
                    elided,
                },
                Err(e) => Stmt::Bad(e),
            };
        }
        return Stmt::Fragment;
    }
    Stmt::Fragment
}

fn kind_ok(value: &Value, kind: &str) -> bool {
    match kind {
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    }
}

/// Whether `value` fits the param: its declared kind, or a shorthand its description allows (an
/// `array` param whose description says a lone number works takes a number; an `object` param whose
/// description gives `true` as a value takes a boolean). An elided value fits anything.
fn param_ok(value: &Value, param: &schema::ParamSchema) -> bool {
    if is_elision(value) || kind_ok(value, param.kind) {
        return true;
    }
    let desc = param.description.to_ascii_lowercase();
    match param.kind {
        "array" => value.is_number() && desc.contains("lone number"),
        "object" => value.is_boolean() && (desc.starts_with("true or ") || desc.contains("`true`")),
        _ => false,
    }
}

fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "non-integer number",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn identifiers(text: &str) -> BTreeSet<&str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect()
}

/// Scans one markdown text. `file` is only used for the findings.
pub fn scan_markdown(file: &str, text: &str, proto: &Protocol, counts: &mut Counts) -> Vec<Finding> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for fence in fences(&lines) {
        if !is_json_fence(fence.info) {
            continue;
        }
        counts.fences += 1;
        if is_ignored(&fence, &lines) {
            counts.skipped_fences += 1;
            continue;
        }
        let ctx = context_methods(&lines, fence.open_idx, proto);
        let mut last: Option<String> = None;
        for (line, stmt) in statements(&fence) {
            match classify(&stmt, &ctx, proto) {
                Stmt::Fragment => counts.fragments += 1,
                Stmt::Bad(e) => {
                    let head: String = stmt.trim().chars().take(48).collect();
                    out.push(Finding {
                        file: file.into(),
                        line,
                        class: Class::Unparseable,
                        key: head.clone(),
                        message: format!("not JSON ({e}): {head}"),
                    });
                }
                Stmt::Request {
                    method,
                    params,
                    elided,
                } => {
                    last = Some(method.clone());
                    if method.starts_with("editor.") {
                        counts.editor_skipped += 1;
                        continue;
                    }
                    counts.requests += 1;
                    check_request(file, line, &method, &params, elided, proto, &mut out);
                }
                Stmt::Bare {
                    method,
                    value,
                    elided,
                } => {
                    let Some(row) = proto.get(&method) else {
                        // An `editor.*` heading, or a method outside the schema.
                        if method.starts_with("editor.") {
                            counts.editor_skipped += 1;
                        } else {
                            counts.fragments += 1;
                        }
                        continue;
                    };
                    let names_param = value
                        .as_object()
                        .is_some_and(|m| m.keys().any(|k| row.params.iter().any(|p| p.name == k)));
                    if names_param {
                        last = Some(method.clone());
                        counts.requests += 1;
                        check_request(file, line, &method, &value, elided, proto, &mut out);
                    } else {
                        // Probably the method's answer shown without an arrow: check it only when
                        // it shares a key with the result description.
                        let named = identifiers(row.result);
                        let shares = value
                            .as_object()
                            .is_some_and(|m| m.keys().any(|k| named.contains(k.as_str())));
                        if shares {
                            counts.responses += 1;
                            check_response(file, line, &method, &value, proto, &mut out);
                        } else {
                            counts.fragments += 1;
                        }
                    }
                }
                Stmt::Response(v) => {
                    let Some(method) = last.as_deref() else {
                        counts.fragments += 1;
                        continue;
                    };
                    if method.starts_with("editor.") {
                        continue;
                    }
                    counts.responses += 1;
                    check_response(file, line, method, &v, proto, &mut out);
                }
            }
        }
    }
    out
}

fn check_request(
    file: &str,
    line: usize,
    method: &str,
    params: &Value,
    elided: bool,
    proto: &Protocol,
    out: &mut Vec<Finding>,
) {
    let mut push = |class: Class, key: String, message: String| {
        out.push(Finding {
            file: file.into(),
            line,
            class,
            key,
            message,
        });
    };
    let Some(row) = proto.get(method) else {
        push(
            Class::UnknownMethod,
            method.into(),
            format!("`{method}` is not a method of the current protocol"),
        );
        return;
    };
    let Some(map) = params.as_object() else {
        if !(params.is_null() && row.params.iter().all(|p| !p.required)) {
            push(
                Class::KindMismatch,
                format!("{method}:params"),
                format!("`{method}` params must be an object, got {}", kind_name(params)),
            );
        }
        return;
    };
    for (key, value) in map {
        match row.params.iter().find(|p| p.name == key) {
            None => push(
                Class::UnknownParam,
                format!("{method}:{key}"),
                format!("`{method}` has no param `{key}`"),
            ),
            Some(p) if !param_ok(value, p) => push(
                Class::KindMismatch,
                format!("{method}:{key}"),
                format!(
                    "`{method}` param `{key}` is declared {}, the example has {}",
                    p.kind,
                    kind_name(value)
                ),
            ),
            Some(_) => {}
        }
    }
    if !elided {
        for p in row.params.iter().filter(|p| p.required) {
            if !map.contains_key(p.name) {
                push(
                    Class::MissingRequired,
                    format!("{method}:{}", p.name),
                    format!("`{method}` requires `{}`", p.name),
                );
            }
        }
    }
}

fn check_response(
    file: &str,
    line: usize,
    method: &str,
    value: &Value,
    proto: &Protocol,
    out: &mut Vec<Finding>,
) {
    let Some(row) = proto.get(method) else {
        return;
    };
    let body = match value.get("result") {
        Some(r) => r,
        None if value.get("error").is_some() => return,
        None => value,
    };
    let (Some(map), true) = (body.as_object(), row.result.trim_start().starts_with('{')) else {
        return;
    };
    let named = identifiers(row.result);
    for key in map.keys() {
        if !named.contains(key.as_str()) {
            out.push(Finding {
                file: file.into(),
                line,
                class: Class::UnknownResultKey,
                key: format!("{method}:{key}"),
                message: format!("`{method}` result does not list `{key}` (result: {})", row.result),
            });
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Workspace scan and baseline

/// One row of `docs/ci/agent-examples.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// Repo-relative file.
    pub file: String,
    /// Finding class name.
    pub class: Class,
    /// The finding's key.
    pub key: String,
    /// Why this cannot be fixed (a page-versus-code mismatch needing a code decision, ...).
    pub reason: String,
}

/// The baseline file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Baseline {
    /// Always 1.
    pub format_version: u32,
    /// Accepted findings.
    #[serde(default, rename = "row")]
    pub rows: Vec<Row>,
}

/// Everything a run found.
pub struct Report {
    /// Every finding, before the baseline.
    pub findings: Vec<Finding>,
    /// What was counted.
    pub counts: Counts,
    /// Findings with no baseline row.
    pub new: Vec<Finding>,
    /// Baseline rows with no finding (or with a blank reason).
    pub stale: Vec<String>,
}

fn md_files(root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let dir = root.join("book/agents");
    if dir.is_dir() {
        for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                files.push(format!("book/agents/{name}"));
            }
        }
    }
    if root.join("book/user-guide/agent-protocol.md").is_file() {
        files.push("book/user-guide/agent-protocol.md".into());
    }
    files.sort();
    Ok(files)
}

/// Scans the workspace's pages and guide topics.
pub fn scan(root: &Path, proto: &Protocol) -> Result<(Vec<Finding>, Counts)> {
    let mut findings = Vec::new();
    let mut counts = Counts::default();
    for file in md_files(root)? {
        let text = std::fs::read_to_string(root.join(&file)).with_context(|| format!("reading {file}"))?;
        findings.extend(scan_markdown(&file, &text, proto, &mut counts));
    }
    for topic in regia_agent::GUIDE_TOPICS {
        let file = format!("crates/regia_agent/src/discovery.rs#{}", topic.name);
        findings.extend(scan_markdown(&file, topic.text, proto, &mut counts));
    }
    findings.sort_by(|a, b| (&a.file, a.line, a.class).cmp(&(&b.file, b.line, b.class)));
    Ok((findings, counts))
}

/// Applies the baseline to `findings`.
pub fn apply_baseline(findings: &[Finding], baseline: &Baseline) -> (Vec<Finding>, Vec<String>) {
    let rows: BTreeSet<(&str, Class, &str)> = baseline
        .rows
        .iter()
        .map(|r| (r.file.as_str(), r.class, r.key.as_str()))
        .collect();
    let seen: BTreeSet<(&str, Class, &str)> = findings
        .iter()
        .map(|f| (f.file.as_str(), f.class, f.key.as_str()))
        .collect();
    let new = findings
        .iter()
        .filter(|f| !rows.contains(&(f.file.as_str(), f.class, f.key.as_str())))
        .cloned()
        .collect();
    let mut stale: Vec<String> = baseline
        .rows
        .iter()
        .filter(|r| !seen.contains(&(r.file.as_str(), r.class, r.key.as_str())))
        .map(|r| format!("stale baseline row: {} [{}] {}", r.file, r.class.name(), r.key))
        .collect();
    stale.extend(
        baseline
            .rows
            .iter()
            .filter(|r| r.reason.trim().is_empty() || r.reason.trim().eq_ignore_ascii_case("todo"))
            .map(|r| {
                format!(
                    "baseline row has no reason: {} [{}] {}",
                    r.file,
                    r.class.name(),
                    r.key
                )
            }),
    );
    (new, stale)
}

fn load_baseline(root: &Path) -> Result<Baseline> {
    let path = root.join(BASELINE_PATH);
    if !path.is_file() {
        return Ok(Baseline {
            format_version: 1,
            rows: Vec::new(),
        });
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let baseline: Baseline = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if baseline.format_version != 1 {
        bail!(
            "{BASELINE_PATH}: format_version {} (expected 1)",
            baseline.format_version
        );
    }
    Ok(baseline)
}

/// Checks the workspace at `root` against the baseline.
pub fn check(root: &Path) -> Result<Report> {
    let proto = Protocol::current();
    let (findings, counts) = scan(root, &proto)?;
    let baseline = load_baseline(root)?;
    let (new, stale) = apply_baseline(&findings, &baseline);
    Ok(Report {
        findings,
        counts,
        new,
        stale,
    })
}

/// Errors for `book-check`: new findings and stale baseline rows. Empty for fixture books
/// that have no dispatcher (the check needs the real protocol tables to mean anything).
pub fn book_check_errors(root: &Path) -> Vec<String> {
    if !root.join("crates/regia_agent/src/dispatch.rs").is_file() {
        return Vec::new();
    }
    match check(root) {
        Ok(report) => report
            .new
            .iter()
            .map(|f| {
                format!(
                    "agent-examples: {}:{}: [{}] {}",
                    f.file,
                    f.line,
                    f.class.name(),
                    f.message
                )
            })
            .chain(
                report
                    .stale
                    .iter()
                    .map(|s| format!("agent-examples: {BASELINE_PATH}: {s}")),
            )
            .collect(),
        Err(err) => vec![format!("agent-examples: {err:#}")],
    }
}

/// The per-class counts line.
pub fn class_counts(findings: &[Finding]) -> String {
    Class::ALL
        .iter()
        .map(|c| {
            format!(
                "{}={}",
                c.name(),
                findings.iter().filter(|f| f.class == *c).count()
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Baseline text for `findings` (reasons left as `TODO`, which the check rejects).
pub fn baseline_text(findings: &[Finding]) -> String {
    let mut seen = BTreeSet::new();
    let mut out = String::from("format_version = 1\n");
    for f in findings {
        if seen.insert((f.file.clone(), f.class, f.key.clone())) {
            out.push_str(&format!(
                "\n[[row]]\nfile = {:?}\nclass = {:?}\nkey = {:?}\nreason = \"TODO\"\n",
                f.file,
                f.class.name(),
                f.key
            ));
        }
    }
    out
}

/// `cargo xtask agent-examples [--print-baseline]`.
pub fn run(root: &Path, print_baseline: bool) -> Result<std::process::ExitCode> {
    let report = check(root)?;
    let c = &report.counts;
    println!(
        "agent-examples: {} fences, {} requests, {} responses; skipped {} fences, {} editor.* statements, {} fragments",
        c.fences, c.requests, c.responses, c.skipped_fences, c.editor_skipped, c.fragments
    );
    println!("agent-examples: findings {}", class_counts(&report.findings));
    if print_baseline {
        print!("{}", baseline_text(&report.findings));
        return Ok(std::process::ExitCode::SUCCESS);
    }
    if report.new.is_empty() && report.stale.is_empty() {
        println!("agent-examples: ok ({} baselined)", report.findings.len());
        return Ok(std::process::ExitCode::SUCCESS);
    }
    for f in &report.new {
        eprintln!("error: {}:{}: [{}] {}", f.file, f.line, f.class.name(), f.message);
    }
    for s in &report.stale {
        eprintln!("error: {BASELINE_PATH}: {s}");
    }
    eprintln!(
        "agent-examples: {} new finding(s), {} baseline problem(s)",
        report.new.len(),
        report.stale.len()
    );
    Ok(std::process::ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_text(text: &str) -> (Vec<Finding>, Counts) {
        let proto = Protocol::current();
        let mut counts = Counts::default();
        let f = scan_markdown("page.md", text, &proto, &mut counts);
        (f, counts)
    }

    fn classes(f: &[Finding]) -> Vec<(Class, String)> {
        f.iter().map(|f| (f.class, f.key.clone())).collect()
    }

    #[test]
    fn a_good_jsonrpc_request_is_clean() {
        let (f, c) = scan_text(
            "```json\n{ \"jsonrpc\": \"2.0\", \"id\": 1, \"method\": \"scene.spawn\", \"params\": { \"name\": \"A\" } }\n```\n",
        );
        assert!(f.is_empty(), "{f:?}");
        assert_eq!(c.requests, 1);
    }

    #[test]
    fn shorthand_with_unknown_and_missing_params() {
        let row = schema::method("scene.spawn").expect("scene.spawn");
        let required = row.params.iter().find(|p| p.required).map(|p| p.name);
        let text = "```json\nscene.spawn { \"bogus\": 1 }\n```\n";
        let (f, _) = scan_text(text);
        let got = classes(&f);
        assert!(
            got.contains(&(Class::UnknownParam, "scene.spawn:bogus".into())),
            "{got:?}"
        );
        if let Some(name) = required {
            assert!(
                got.contains(&(Class::MissingRequired, format!("scene.spawn:{name}"))),
                "{got:?}"
            );
        }
    }

    #[test]
    fn unknown_method_and_editor_skip() {
        let (f, c) = scan_text("```json\nscene.nope { }\neditor.anything { \"x\": 1 }\n```\n");
        assert_eq!(classes(&f), vec![(Class::UnknownMethod, "scene.nope".into())]);
        assert_eq!(c.editor_skipped, 1);
    }

    #[test]
    fn kind_mismatch_uses_the_declared_kind() {
        let row = schema::METHODS
            .iter()
            .find_map(|m| m.params.iter().find(|p| p.kind == "boolean").map(|p| (m, p)))
            .expect("a boolean param exists");
        let (m, p) = row;
        let text = format!("```json\n{} {{ \"{}\": \"yes\" }}\n```\n", m.name, p.name);
        let (f, _) = scan_text(&text);
        assert!(
            f.iter()
                .any(|f| f.class == Class::KindMismatch && f.key == format!("{}:{}", m.name, p.name)),
            "{f:?}"
        );
    }

    #[test]
    fn bare_params_take_the_method_from_the_heading() {
        let text = "### `scene.nope`\n\n```json\n{ \"a\": 1 }\n```\n";
        // Unknown method in a heading is not a context method, so the object is a fragment.
        let (f, c) = scan_text(text);
        assert!(f.is_empty());
        assert_eq!(c.fragments, 1);
        let text = "### `scene.despawn`\n\n```json\n{ \"entity\": 1, \"zzz\": 1 }\n```\n";
        let (f, _) = scan_text(text);
        assert!(f.iter().any(|f| f.key == "scene.despawn:zzz"), "{f:?}");
    }

    #[test]
    fn ignore_markers_skip_and_count() {
        let (f, c) = scan_text(
            "```json,ignore\nscene.nope { }\n```\n\n<!-- not-a-request -->\n```json\nscene.nope { }\n```\n",
        );
        assert!(f.is_empty());
        assert_eq!(c.skipped_fences, 2);
        assert_eq!(c.fences, 2);
    }

    #[test]
    fn elisions_comments_and_trailing_commas_parse() {
        let (f, _) = scan_text(
            "```json\nscene.despawn { \"entity\": 4294967300, ... } // note\n→ { \"despawned\": true, ... }\n```\n",
        );
        assert!(f.iter().all(|f| f.class != Class::Unparseable), "{f:?}");
        let (f, _) = scan_text("```json\nscene.despawn { \"entity\": 1,, }\n```\n");
        assert!(f.iter().any(|f| f.class == Class::Unparseable));
    }

    #[test]
    fn responses_are_checked_against_the_result_description() {
        let row = schema::METHODS
            .iter()
            .find(|m| m.result.starts_with('{'))
            .expect("a method with a keyed result");
        let text = format!(
            "```json\n{} {{ ... }}\n→ {{ \"zz_not_a_key\": 1 }}\n```\n",
            row.name
        );
        let (f, c) = scan_text(&text);
        assert_eq!(c.responses, 1);
        assert!(f.iter().any(|f| f.class == Class::UnknownResultKey), "{f:?}");
    }

    #[test]
    fn an_inline_arrow_splits_request_and_response() {
        let (f, c) = scan_text("```json\nscene.despawn { \"entity\": 1 }   → { \"despawned\": true }\n```\n");
        assert_eq!(c.requests, 1, "{f:?}");
        assert_eq!(c.responses, 1, "{f:?}");
        assert!(f.iter().all(|f| f.class != Class::Unparseable), "{f:?}");
    }

    #[test]
    fn a_bare_object_without_a_param_name_is_read_as_the_result() {
        let (f, c) = scan_text("### `scene.despawn`\n\n```json\n{ \"zzz\": 1 }\n```\n");
        assert!(f.is_empty(), "{f:?}");
        assert_eq!(c.fragments, 1);
    }

    #[test]
    fn a_bare_object_binds_to_the_method_whose_params_it_covers() {
        // A sentence that names the method and, later, another one whose keys it accepts.
        let text = "## Plans\n\n`render.capture_plan` takes any `render.out_start` key:\n\n```json\n\
                    { \"preset\": \"showcase_4k60\", \"name\": \"forest\", \"codec\": \"hevc\", \"duration_s\": 20,\n  \
                    \"sequence\": \"shots/forest.shots.ron\" }\n\
                    → { \"preset\": \"showcase_4k60\", \"width\": 3840, \"height\": 2160, \"fps\": 60, \"steps\": [] }\n```\n";
        let (f, c) = scan_text(text);
        assert!(f.is_empty(), "{f:?}");
        assert_eq!((c.requests, c.responses), (1, 1));

        // An incidental "as in `pacing.set`" on the nearest line.
        let text = "## Try it\n\n`pacing.simulate` runs the model in virtual time.\n\n\
                    - `cost` is `work` or `frame`, as in `pacing.set`.\n\n```json\n\
                    { \"refresh_hz\": 60, \"frame_ms\": [6], \"frames\": 2000, \"follow_hint\": true,\n  \
                    \"swapchain_images\": 2, \"gpu_ms\": 7, \"latch_ms\": 4, \"cost\": \"frame\" }\n```\n";
        let (f, c) = scan_text(text);
        assert!(f.is_empty(), "{f:?}");
        assert_eq!(c.requests, 1);

        // Two methods on one line, one object of each, each reply checked against its own method.
        let page = |volume_reply: &str| {
            format!(
                "## Areas\n\n`nav.add_area_volume` paints an area and `nav.set_area_cost` sets its cost:\n\n```json\n\
                 {{ \"shape\": \"box\", \"min\": [-6, -1, -8], \"max\": [6, 1, 8], \"area\": 3 }}\n\
                 → {volume_reply}\n\n{{ \"area\": 3, \"cost\": 8.0 }}\n\
                 → {{ \"costs\": [{{ \"area\": 3, \"cost\": 8.0 }}] }}\n```\n"
            )
        };
        let (f, c) = scan_text(&page(
            "{ \"id\": 0, \"rebuilt\": [[1, 1]], \"pending\": [], \"revision\": 4 }",
        ));
        assert!(f.is_empty(), "{f:?}");
        assert_eq!((c.requests, c.responses), (2, 2));
        let (f, _) = scan_text(&page("{ \"id\": 0, \"zz_not_a_key\": 1 }"));
        assert_eq!(
            classes(&f),
            vec![(Class::UnknownResultKey, "nav.add_area_volume:zz_not_a_key".into())]
        );
    }

    #[test]
    fn a_tie_or_no_cover_binds_the_nearest_mention() {
        // `filter` is declared on both; the nearest mention wins the tie and reports `zzz`.
        let text = "`nav.find_path` and `nav.crowd_set_filter` take a `filter`:\n\n```json\n\
                    { \"filter\": {}, \"zzz\": 1 }\n```\n";
        let (f, _) = scan_text(text);
        assert!(f.iter().any(|f| f.key == "nav.crowd_set_filter:zzz"), "{f:?}");
        // No method covers a key: the nearest one's result is checked.
        let text = "`nav.find_path` then `nav.add_area_volume`:\n\n```json\n{ \"revision\": 4 }\n```\n";
        let (f, c) = scan_text(text);
        assert!(f.is_empty(), "{f:?}");
        assert_eq!(c.responses, 1);
    }

    #[test]
    fn description_shorthands_pass_the_kind_check() {
        // `frame_ms` is an array whose description says a lone number works.
        let (f, _) = scan_text("```json\npacing.simulate { \"frame_ms\": 20 }\n```\n");
        assert!(f.is_empty(), "{f:?}");
        // `audio` is an object whose description gives `true` as a value.
        let (f, _) = scan_text("```json\nrender.out_start { \"out\": \"o\", \"audio\": true }\n```\n");
        assert!(f.iter().all(|f| f.key != "render.out_start:audio"), "{f:?}");
        // An array param without that sentence still refuses a number.
        let (f, _) = scan_text("```json\nnav.find_path { \"from\": 1, \"to\": [0, 0, 0] }\n```\n");
        assert_eq!(
            classes(&f),
            vec![(Class::KindMismatch, "nav.find_path:from".into())]
        );
    }

    #[test]
    fn quoted_ellipsis_is_elision() {
        let (f, c) = scan_text(
            "```json\n{ \"method\": \"captions.state\" }\n\
             → { \"entries\": [ { \"rect\": { \"...\": \"...\" } }, \"...\" ], \"…\": \"…\" }\n```\n",
        );
        assert!(f.is_empty(), "{f:?}");
        assert_eq!(c.responses, 1);
        // A quoted `"...": "..."` entry in params skips the required check, like a bare `...`.
        let (f, _) = scan_text("```json\nnav.set_area_cost { \"area\": 3, \"...\": \"...\" }\n```\n");
        assert!(f.is_empty(), "{f:?}");
        // An elided value fits any kind.
        let (f, _) = scan_text("```json\nnav.set_area_cost { \"area\": 3, \"cost\": \"...\" }\n```\n");
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn a_non_json_fence_is_ignored() {
        let (f, c) =
            scan_text("```ron\nscene.nope { }\n```\n````markdown\n```json\nscene.nope { }\n```\n````\n");
        assert!(f.is_empty());
        assert_eq!(c.fences, 0);
    }

    #[test]
    fn baseline_rows_match_and_stale_rows_fail() {
        let finding = Finding {
            file: "p.md".into(),
            line: 3,
            class: Class::UnknownParam,
            key: "a.b:c".into(),
            message: String::new(),
        };
        let row = |key: &str, reason: &str| Row {
            file: "p.md".into(),
            class: Class::UnknownParam,
            key: key.into(),
            reason: reason.into(),
        };
        let good = Baseline {
            format_version: 1,
            rows: vec![row("a.b:c", "code lacks it")],
        };
        let (new, stale) = apply_baseline(std::slice::from_ref(&finding), &good);
        assert!(new.is_empty() && stale.is_empty());
        let (new, stale) = apply_baseline(&[], &good);
        assert!(new.is_empty());
        assert_eq!(stale.len(), 1);
        let (new, _) = apply_baseline(
            std::slice::from_ref(&finding),
            &Baseline {
                format_version: 1,
                rows: vec![],
            },
        );
        assert_eq!(new.len(), 1);
        let todo = Baseline {
            format_version: 1,
            rows: vec![row("a.b:c", "TODO")],
        };
        let (_, stale) = apply_baseline(&[finding], &todo);
        assert_eq!(stale.len(), 1);
    }

    #[test]
    fn baseline_text_round_trips() {
        let f = Finding {
            file: "p.md".into(),
            line: 1,
            class: Class::Unparseable,
            key: "x \"q\"".into(),
            message: String::new(),
        };
        let text = baseline_text(&[f]).replace("TODO", "because");
        let parsed: Baseline = toml::from_str(&text).expect("toml");
        assert_eq!(parsed.format_version, 1);
        assert_eq!(parsed.rows[0].class, Class::Unparseable);
        assert_eq!(parsed.rows[0].key, "x \"q\"");
    }
}
