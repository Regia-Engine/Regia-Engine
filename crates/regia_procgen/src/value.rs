// SPDX-License-Identifier: MPL-2.0
//! Untyped RON values for graph files and node settings (spec 62 § 4).
//!
//! Graph files keep node settings as plain RON values until compile. A generic RON value model
//! loses enum variant names (`Weighted`, `ByAttr(attr: "kind")`), which settings need, so this
//! module has its own small reader and a canonical writer: [`SettingValue::parse`] reads any RON
//! value the settings blocks of specs 62-68 use, and [`SettingValue::write`] writes it back so that
//! write → read → write is byte-identical.

use std::fmt::Write as _;

/// One untyped RON value.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingValue {
    /// `true` / `false`.
    Bool(bool),
    /// An integer literal (`4`, `-2`).
    Int(i64),
    /// A float literal (`4.0`, `0.8`, `1e-3`).
    Float(f64),
    /// A string literal.
    Str(String),
    /// A bare identifier: a unit enum variant (`Weighted`, `World`, `None`).
    Ident(String),
    /// `Name(a, b)`: a tuple variant or newtype (`Bind("spacing")`, `Some(2.0)`).
    Call(String, Vec<SettingValue>),
    /// `(a: 1, b: 2)` or `Name(a: 1)`: a struct, with an optional variant name.
    Record(Option<String>, Vec<(String, SettingValue)>),
    /// `(a, b, c)`: an anonymous tuple (vectors, pairs).
    Tuple(Vec<SettingValue>),
    /// `[a, b]`.
    List(Vec<SettingValue>),
    /// `{ key: value }`, in file order.
    Map(Vec<(SettingValue, SettingValue)>),
}

/// A parse failure with its byte offset.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("RON parse error at byte {offset}: {message}")]
pub struct ParseError {
    /// Byte offset into the text.
    pub offset: usize,
    /// What went wrong.
    pub message: String,
}

impl SettingValue {
    /// Parses one RON value; trailing text other than whitespace and comments is an error.
    ///
    /// # Errors
    ///
    /// [`ParseError`] on malformed input.
    ///
    /// ```
    /// use regia_procgen::{NodeEntry, SettingValue};
    ///
    /// // Settings are RON text; a node entry holds the parsed value.
    /// let spacing = SettingValue::parse("4.0")?;
    /// let node = NodeEntry::new("sample.ground").with("spacing_m", spacing);
    /// assert!(node.settings.contains_key("spacing_m"));
    /// assert!(SettingValue::parse("(unclosed").is_err());
    /// # Ok::<(), regia_procgen::value::ParseError>(())
    /// ```
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut p = Parser {
            s: text.as_bytes(),
            i: 0,
            depth: 0,
        };
        let v = p.value()?;
        p.ws();
        if p.i != p.s.len() {
            return Err(p.err("trailing characters"));
        }
        Ok(v)
    }

    /// The canonical RON text of this value (single line).
    pub fn write(&self) -> String {
        let mut out = String::new();
        self.write_into(&mut out);
        out
    }

    /// Appends the canonical text to `out`.
    pub fn write_into(&self, out: &mut String) {
        match self {
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Self::Float(f) => write_float(*f, out),
            Self::Str(s) => write_str(s, out),
            Self::Ident(s) => out.push_str(s),
            Self::Call(name, args) => {
                out.push_str(name);
                out.push('(');
                write_list(args, out);
                out.push(')');
            }
            Self::Record(name, fields) => {
                if let Some(name) = name {
                    out.push_str(name);
                }
                out.push('(');
                for (k, (name, v)) in fields.iter().enumerate() {
                    if k > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(name);
                    out.push_str(": ");
                    v.write_into(out);
                }
                out.push(')');
            }
            Self::Tuple(items) => {
                out.push('(');
                write_list(items, out);
                out.push(')');
            }
            Self::List(items) => {
                out.push('[');
                write_list(items, out);
                out.push(']');
            }
            Self::Map(entries) => {
                out.push('{');
                for (k, (key, v)) in entries.iter().enumerate() {
                    if k > 0 {
                        out.push_str(", ");
                    }
                    key.write_into(out);
                    out.push_str(": ");
                    v.write_into(out);
                }
                out.push('}');
            }
        }
    }

    /// The parameter name when this value is `Bind("name")`.
    pub fn as_bind(&self) -> Option<&str> {
        match self {
            Self::Call(name, args) if name == "Bind" && args.len() == 1 => match &args[0] {
                Self::Str(s) => Some(s),
                _ => None,
            },
            _ => None,
        }
    }

    /// A number as f64 (`Int` widens).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(i) => Some(*i as f64),
            Self::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// An integer (a float with no fractional part also counts).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            Self::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Some(*f as i64),
            _ => None,
        }
    }

    /// A boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// A string or identifier as text.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Str(s) | Self::Ident(s) => Some(s),
            _ => None,
        }
    }

    /// The elements of a tuple, list or call arguments.
    pub fn as_items(&self) -> Option<&[SettingValue]> {
        match self {
            Self::Tuple(v) | Self::List(v) => Some(v),
            _ => None,
        }
    }

    /// A field of a record.
    pub fn field(&self, name: &str) -> Option<&SettingValue> {
        match self {
            Self::Record(_, fields) => fields.iter().find(|(k, _)| k == name).map(|(_, v)| v),
            _ => None,
        }
    }

    /// A 3-vector from a 3-tuple or list of numbers, or a broadcast scalar.
    pub fn as_vec3(&self) -> Option<[f64; 3]> {
        if let Some(s) = self.as_f64() {
            return Some([s, s, s]);
        }
        let items = self.as_items()?;
        if items.len() != 3 {
            return None;
        }
        Some([items[0].as_f64()?, items[1].as_f64()?, items[2].as_f64()?])
    }

    /// Replaces every `Bind("name")` leaf through `lookup`; unknown names are collected.
    pub fn resolve_binds(
        &self,
        lookup: &dyn Fn(&str) -> Option<SettingValue>,
        unknown: &mut Vec<String>,
    ) -> Self {
        if let Some(name) = self.as_bind() {
            return match lookup(name) {
                Some(v) => v,
                None => {
                    unknown.push(name.to_string());
                    self.clone()
                }
            };
        }
        let map_vec = |v: &[SettingValue], unknown: &mut Vec<String>| {
            v.iter()
                .map(|x| x.resolve_binds(lookup, unknown))
                .collect::<Vec<_>>()
        };
        match self {
            Self::Call(n, a) => Self::Call(n.clone(), map_vec(a, unknown)),
            Self::Tuple(a) => Self::Tuple(map_vec(a, unknown)),
            Self::List(a) => Self::List(map_vec(a, unknown)),
            Self::Record(n, f) => Self::Record(
                n.clone(),
                f.iter()
                    .map(|(k, v)| (k.clone(), v.resolve_binds(lookup, unknown)))
                    .collect(),
            ),
            Self::Map(e) => Self::Map(
                e.iter()
                    .map(|(k, v)| (k.clone(), v.resolve_binds(lookup, unknown)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

fn write_list(items: &[SettingValue], out: &mut String) {
    for (k, v) in items.iter().enumerate() {
        if k > 0 {
            out.push_str(", ");
        }
        v.write_into(out);
    }
}

/// Floats always carry a `.` or an exponent so they read back as floats; Rust's shortest
/// round-trip formatting keeps the exact bits.
fn write_float(f: f64, out: &mut String) {
    let s = format!("{f:?}");
    out.push_str(&s);
    if !s.contains(['.', 'e', 'E', 'N', 'i']) {
        out.push_str(".0");
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Nesting cap: graph files are user data; a hostile file must not overflow the stack.
const MAX_DEPTH: usize = 64;

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    depth: usize,
}

impl Parser<'_> {
    fn err(&self, message: &str) -> ParseError {
        ParseError {
            offset: self.i,
            message: message.to_string(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    /// Skips whitespace, `//` line comments and `/* */` block comments.
    fn ws(&mut self) {
        loop {
            while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                self.i += 1;
            }
            if self.s[self.i..].starts_with(b"//") {
                while self.peek().is_some_and(|c| c != b'\n') {
                    self.i += 1;
                }
            } else if self.s[self.i..].starts_with(b"/*") {
                self.i += 2;
                while self.i < self.s.len() && !self.s[self.i..].starts_with(b"*/") {
                    self.i += 1;
                }
                self.i = (self.i + 2).min(self.s.len());
            } else {
                return;
            }
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), ParseError> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(self.err(&format!("expected `{}`", c as char)))
        }
    }

    fn value(&mut self) -> Result<SettingValue, ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        let v = self.value_inner();
        self.depth -= 1;
        v
    }

    fn value_inner(&mut self) -> Result<SettingValue, ParseError> {
        self.ws();
        match self.peek() {
            None => Err(self.err("unexpected end of input")),
            Some(b'"') => self.string().map(SettingValue::Str),
            Some(b'[') => {
                self.i += 1;
                let items = self.seq(b']')?;
                Ok(SettingValue::List(items))
            }
            Some(b'{') => {
                self.i += 1;
                let mut entries = Vec::new();
                loop {
                    if self.eat(b'}') {
                        break;
                    }
                    let k = self.value()?;
                    self.expect(b':')?;
                    let v = self.value()?;
                    entries.push((k, v));
                    if !self.eat(b',') {
                        self.expect(b'}')?;
                        break;
                    }
                }
                Ok(SettingValue::Map(entries))
            }
            Some(b'(') => {
                self.i += 1;
                self.paren_body(None)
            }
            Some(c) if c == b'-' || c == b'+' || c.is_ascii_digit() || c == b'.' => self.number(),
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                let name = self.ident();
                match name.as_str() {
                    "true" => return Ok(SettingValue::Bool(true)),
                    "false" => return Ok(SettingValue::Bool(false)),
                    "inf" => return Ok(SettingValue::Float(f64::INFINITY)),
                    "NaN" => return Ok(SettingValue::Float(f64::NAN)),
                    _ => {}
                }
                // No whitespace skip: `Name (` is still a call in RON, but keep it strict and simple.
                let save = self.i;
                self.ws();
                if self.peek() == Some(b'(') {
                    self.i += 1;
                    self.paren_body(Some(name))
                } else {
                    self.i = save;
                    Ok(SettingValue::Ident(name))
                }
            }
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    /// After `(` (optionally preceded by a name): a record `(a: 1)`, a tuple `(1, 2)`, `()`.
    fn paren_body(&mut self, name: Option<String>) -> Result<SettingValue, ParseError> {
        self.ws();
        // Record when the first token is `ident :` (and not `ident ::`).
        let save = self.i;
        let mut is_record = false;
        if self.peek().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_') {
            let _ = self.ident();
            self.ws();
            is_record = self.peek() == Some(b':');
        }
        self.i = save;
        if is_record {
            let mut fields = Vec::new();
            loop {
                if self.eat(b')') {
                    break;
                }
                self.ws();
                if !self.peek().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_') {
                    return Err(self.err("expected a field name"));
                }
                let k = self.ident();
                self.expect(b':')?;
                let v = self.value()?;
                fields.push((k, v));
                if !self.eat(b',') {
                    self.expect(b')')?;
                    break;
                }
            }
            return Ok(SettingValue::Record(name, fields));
        }
        let items = self.seq(b')')?;
        Ok(match name {
            Some(n) => SettingValue::Call(n, items),
            None => SettingValue::Tuple(items),
        })
    }

    fn seq(&mut self, close: u8) -> Result<Vec<SettingValue>, ParseError> {
        let mut items = Vec::new();
        loop {
            if self.eat(close) {
                break;
            }
            items.push(self.value()?);
            if !self.eat(b',') {
                self.expect(close)?;
                break;
            }
        }
        Ok(items)
    }

    fn ident(&mut self) -> String {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }

    fn number(&mut self) -> Result<SettingValue, ParseError> {
        let start = self.i;
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.i += 1;
        }
        if self.s[self.i..].starts_with(b"inf") {
            self.i += 3;
            let neg = self.s[start] == b'-';
            return Ok(SettingValue::Float(if neg {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }));
        }
        let mut is_float = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'_' {
                self.i += 1;
            } else if c == b'.' || c == b'e' || c == b'E' {
                is_float = true;
                self.i += 1;
                if (c == b'e' || c == b'E') && matches!(self.peek(), Some(b'-' | b'+')) {
                    self.i += 1;
                }
            } else {
                break;
            }
        }
        let text: String = String::from_utf8_lossy(&self.s[start..self.i])
            .chars()
            .filter(|c| *c != '_')
            .collect();
        if is_float {
            text.parse::<f64>()
                .map(SettingValue::Float)
                .map_err(|_| self.err("bad float"))
        } else {
            text.parse::<i64>()
                .map(SettingValue::Int)
                .map_err(|_| self.err("bad integer"))
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        // At the opening quote.
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.err("unterminated string"));
            };
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return Err(self.err("unterminated escape"));
                    };
                    self.i += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'0' => out.push(0),
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        _ => return Err(self.err("unknown escape")),
                    }
                }
                c => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| self.err("string is not UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_settings_forms_of_specs_62_to_68() {
        let v = SettingValue::parse(
            r#"( pick: Weighted, by: ByAttr(attr: "kind"), entries: [ (mesh: Bind("rock_mesh"), weight: 1.0) ],
                 v: (0, 1.5, -2e-3), opt: Some(2), none: None, m: { "a": 1, 2: true } ) // trailing"#,
        )
        .expect("parse");
        assert_eq!(v.field("pick"), Some(&SettingValue::Ident("Weighted".into())));
        assert_eq!(
            v.field("by"),
            Some(&SettingValue::Record(
                Some("ByAttr".into()),
                vec![("attr".into(), SettingValue::Str("kind".into()))]
            ))
        );
        let entries = v
            .field("entries")
            .and_then(SettingValue::as_items)
            .expect("entries");
        assert_eq!(
            entries[0].field("mesh").and_then(SettingValue::as_bind),
            Some("rock_mesh")
        );
        assert_eq!(
            v.field("v").and_then(SettingValue::as_vec3),
            Some([0.0, 1.5, -0.002])
        );
        assert_eq!(
            v.field("opt"),
            Some(&SettingValue::Call("Some".into(), vec![SettingValue::Int(2)]))
        );
    }

    #[test]
    fn write_read_write_is_byte_identical() {
        let text = r#"(a: 1, b: 2.5, c: "x\"y", d: [Foo, Bar(1, 2)], e: {1: (x: 0.1)}, f: (1.0, 2.0, 3.0), g: 1e-7)"#;
        let once = SettingValue::parse(text).expect("parse").write();
        let twice = SettingValue::parse(&once).expect("reparse").write();
        assert_eq!(once, twice);
        assert!(once.contains("b: 2.5") && once.contains("g: 1e-7"));
    }

    #[test]
    fn rejects_malformed_and_too_deep_input() {
        assert!(SettingValue::parse("(a: 1").is_err());
        assert!(SettingValue::parse("[1, 2] x").is_err());
        assert!(SettingValue::parse("\"open").is_err());
        let deep = "[".repeat(200) + &"]".repeat(200);
        assert!(SettingValue::parse(&deep).is_err());
    }

    #[test]
    fn resolves_binds_and_reports_unknown_names() {
        let v = SettingValue::parse(r#"(s: Bind("spacing"), l: [Bind("nope")])"#).expect("parse");
        let mut unknown = Vec::new();
        let r = v.resolve_binds(
            &|n| (n == "spacing").then_some(SettingValue::Float(2.0)),
            &mut unknown,
        );
        assert_eq!(r.field("s"), Some(&SettingValue::Float(2.0)));
        assert_eq!(unknown, vec!["nope".to_string()]);
    }
}
