// SPDX-License-Identifier: MPL-2.0
//! Field paths inside raw RON text (`docs/formats/level-variants.md`, ADR-7823).
//!
//! A level variant stores field overrides as `(target, component, path, value)` like a prefab
//! instance does, but it is resolved into a *scene file*, not into live components: the override
//! is applied to the component's raw RON text, so resolving needs no type registry (the cook and
//! a dedicated server resolve variants of game types they never registered).
//!
//! The text is read into a small span tree: every value is a leaf (number, string, identifier, …)
//! or a bracketed group `( … )`, `[ … ]`, `{ … }` with an optional name (`Transform( … )`) and
//! items; an item in a parenthesized group may carry a field name (`intensity: 2.0`). Paths are
//! the reflection paths the editor already uses: `.field` names a struct field, `.0` a tuple
//! element and `[2]` a list element; `""` or `"."` is the whole value.
//!
//! Splicing replaces only the addressed value's bytes, so the rest of the text keeps its exact
//! spelling, and a field the text omits (a serde default) is appended to its struct. Original
//! code: a recursive-descent reader over the RON grammar published at
//! <https://github.com/ron-rs/ron/blob/master/docs/grammar.md>.

/// One step of a field path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// A named struct field (`.intensity`).
    Field(String),
    /// A tuple (`.0`) or list (`[0]`) element.
    Index(usize),
}

/// Splits a reflection path (`.translation`, `.items[2].0`) into segments; `""` and `"."` are the
/// empty path (the whole value).
///
/// # Errors
///
/// A segment that is empty, an unclosed `[`, or an index that is not a number.
pub fn parse_path(path: &str) -> Result<Vec<Segment>, String> {
    let mut out = Vec::new();
    if path.is_empty() || path == "." {
        return Ok(out);
    }
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end] != b'.' && bytes[end] != b'[' {
                    end += 1;
                }
                let name = &path[start..end];
                if name.is_empty() {
                    return Err(format!("empty segment in path `{path}`"));
                }
                if name.bytes().all(|b| b.is_ascii_digit()) {
                    out.push(Segment::Index(
                        name.parse().map_err(|_| format!("bad index in `{path}`"))?,
                    ));
                } else if name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    out.push(Segment::Field(name.to_owned()));
                } else {
                    return Err(format!("bad field name `{name}` in path `{path}`"));
                }
                i = end;
            }
            b'[' => {
                let close = path[i..]
                    .find(']')
                    .map(|at| i + at)
                    .ok_or_else(|| format!("unclosed `[` in path `{path}`"))?;
                let index = path[i + 1..close]
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad index in path `{path}`"))?;
                out.push(Segment::Index(index));
                i = close + 1;
            }
            _ if i == 0 => {
                // A path written without its leading dot (`intensity`).
                return parse_path(&format!(".{path}"));
            }
            _ => return Err(format!("unexpected `{}` in path `{path}`", bytes[i] as char)),
        }
    }
    Ok(out)
}

/// Joins segments back into a path (`.a.b[2]`; a tuple index is written `.N`).
#[must_use]
pub fn join_path(segments: &[Segment]) -> String {
    let mut out = String::new();
    for segment in segments {
        match segment {
            Segment::Field(name) => {
                out.push('.');
                out.push_str(name);
            }
            Segment::Index(index) => {
                out.push('.');
                out.push_str(&index.to_string());
            }
        }
    }
    out
}

/// A parsed value: its byte span and its shape.
#[derive(Clone, Debug)]
struct Node {
    start: usize,
    end: usize,
    group: Option<Group>,
}

/// A bracketed value.
#[derive(Clone, Debug)]
struct Group {
    /// `(`, `[` or `{`.
    open: u8,
    /// The name before `(` (`Transform`), as a byte span.
    name: Option<(usize, usize)>,
    /// Offset just after the opening bracket.
    inner_start: usize,
    items: Vec<Item>,
}

#[derive(Clone, Debug)]
struct Item {
    key: Option<String>,
    value: Node,
}

impl Group {
    /// A parenthesized group whose items all carry field names (an empty `()` counts).
    fn is_struct(&self) -> bool {
        self.open == b'(' && self.items.iter().all(|item| item.key.is_some())
    }
}

/// Deepest nesting the reader follows (a hostile file must not overflow the stack).
const MAX_DEPTH: usize = 64;

struct Reader<'a> {
    text: &'a str,
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
        }
    }

    fn err(&self, at: usize, what: &str) -> String {
        format!("{what} at byte {at}")
    }

    fn skip_ws(&self, mut i: usize) -> usize {
        loop {
            while i < self.bytes.len() && self.bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if self.bytes[i..].starts_with(b"//") {
                while i < self.bytes.len() && self.bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if self.bytes[i..].starts_with(b"/*") {
                let mut depth = 0usize;
                while i < self.bytes.len() {
                    if self.bytes[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if self.bytes[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            return i;
        }
    }

    fn is_ident_start(b: u8) -> bool {
        b.is_ascii_alphabetic() || b == b'_'
    }

    fn is_ident(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    /// End of a quoted string or char starting at `i` (the quote), past the closing quote.
    fn quoted_end(&self, i: usize, quote: u8) -> Result<usize, String> {
        let mut j = i + 1;
        while j < self.bytes.len() {
            match self.bytes[j] {
                b'\\' => j += 2,
                b if b == quote => return Ok(j + 1),
                _ => j += 1,
            }
        }
        Err(self.err(i, "unterminated string"))
    }

    /// End of a raw string `r#"…"#` starting at `i` (the `r`).
    fn raw_string_end(&self, i: usize) -> Result<usize, String> {
        let mut j = i + 1;
        let mut hashes = 0;
        while j < self.bytes.len() && self.bytes[j] == b'#' {
            hashes += 1;
            j += 1;
        }
        if j >= self.bytes.len() || self.bytes[j] != b'"' {
            return Err(self.err(i, "bad raw string"));
        }
        let closing: Vec<u8> = std::iter::once(b'"')
            .chain(std::iter::repeat_n(b'#', hashes))
            .collect();
        let rest = &self.bytes[j + 1..];
        rest.windows(closing.len())
            .position(|window| window == closing.as_slice())
            .map(|at| j + 1 + at + closing.len())
            .ok_or_else(|| self.err(i, "unterminated raw string"))
    }

    fn value(&self, i: usize, depth: usize) -> Result<Node, String> {
        if depth > MAX_DEPTH {
            return Err(self.err(i, "nesting too deep"));
        }
        let i = self.skip_ws(i);
        let Some(&first) = self.bytes.get(i) else {
            return Err(self.err(i, "expected a value"));
        };
        match first {
            b'"' | b'\'' => Ok(Node {
                start: i,
                end: self.quoted_end(i, first)?,
                group: None,
            }),
            b'r' if matches!(self.bytes.get(i + 1), Some(b'"' | b'#')) => Ok(Node {
                start: i,
                end: self.raw_string_end(i)?,
                group: None,
            }),
            b'b' if self.bytes.get(i + 1) == Some(&b'"') => Ok(Node {
                start: i,
                end: self.quoted_end(i + 1, b'"')?,
                group: None,
            }),
            b'(' | b'[' | b'{' => self.group(i, i, None, depth),
            b if Self::is_ident_start(b) => {
                let mut end = i + 1;
                while end < self.bytes.len() && Self::is_ident(self.bytes[end]) {
                    end += 1;
                }
                let after = self.skip_ws(end);
                if self.bytes.get(after) == Some(&b'(') {
                    self.group(i, after, Some((i, end)), depth)
                } else {
                    Ok(Node {
                        start: i,
                        end,
                        group: None,
                    })
                }
            }
            _ => {
                let mut end = i;
                while end < self.bytes.len()
                    && !self.bytes[end].is_ascii_whitespace()
                    && !matches!(self.bytes[end], b',' | b')' | b']' | b'}' | b':')
                {
                    end += 1;
                }
                if end == i {
                    return Err(self.err(i, "expected a value"));
                }
                Ok(Node {
                    start: i,
                    end,
                    group: None,
                })
            }
        }
    }

    fn group(
        &self,
        start: usize,
        open_at: usize,
        name: Option<(usize, usize)>,
        depth: usize,
    ) -> Result<Node, String> {
        let open = self.bytes[open_at];
        let close = match open {
            b'(' => b')',
            b'[' => b']',
            _ => b'}',
        };
        let mut items = Vec::new();
        let mut i = open_at + 1;
        loop {
            i = self.skip_ws(i);
            match self.bytes.get(i) {
                None => return Err(self.err(open_at, "unclosed bracket")),
                Some(&b) if b == close => break,
                _ => {}
            }
            let first = self.value(i, depth + 1)?;
            let mut j = self.skip_ws(first.end);
            let item = if self.bytes.get(j) == Some(&b':') && open != b'[' {
                let key = (open == b'(' && first.group.is_none())
                    .then(|| self.text[first.start..first.end].to_owned());
                let value = self.value(j + 1, depth + 1)?;
                j = self.skip_ws(value.end);
                Item { key, value }
            } else {
                Item {
                    key: None,
                    value: first,
                }
            };
            items.push(item);
            match self.bytes.get(j) {
                Some(b',') => i = j + 1,
                Some(&b) if b == close => i = j,
                _ => return Err(self.err(j, "expected `,` or a closing bracket")),
            }
        }
        Ok(Node {
            start,
            end: i + 1,
            group: Some(Group {
                open,
                name,
                inner_start: open_at + 1,
                items,
            }),
        })
    }

    fn document(&self) -> Result<Node, String> {
        let node = self.value(0, 0)?;
        let rest = self.skip_ws(node.end);
        if rest != self.bytes.len() {
            return Err(self.err(rest, "trailing characters"));
        }
        Ok(node)
    }
}

/// The text with whitespace and comments outside strings removed and trailing commas dropped:
/// two spellings of one value compare equal (`(a: 1,)` and `( a:1 )`).
#[must_use]
pub fn compact(text: &str) -> String {
    let reader = Reader::new(text);
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let next = reader.skip_ws(i);
        if next != i {
            i = next;
            continue;
        }
        let b = bytes[i];
        let end = match b {
            b'"' | b'\'' => reader.quoted_end(i, b).unwrap_or(bytes.len()),
            b'r' if matches!(bytes.get(i + 1), Some(b'"' | b'#')) => {
                reader.raw_string_end(i).unwrap_or(bytes.len())
            }
            b',' => {
                let after = reader.skip_ws(i + 1);
                if matches!(bytes.get(after), Some(b')' | b']' | b'}') | None) {
                    i = after;
                    continue;
                }
                i + 1
            }
            _ => {
                // One char (UTF-8 safe: copy up to the next char boundary).
                let mut end = i + 1;
                while end < bytes.len() && !text.is_char_boundary(end) {
                    end += 1;
                }
                end
            }
        };
        out.push_str(&text[i..end]);
        i = end;
    }
    out
}

/// Whether two RON texts spell the same value (see [`compact`]).
#[must_use]
pub fn same_value(a: &str, b: &str) -> bool {
    a == b || compact(a) == compact(b)
}

/// The text of the value at `path` inside `text`.
///
/// # Errors
///
/// The text does not parse, or the path names nothing in it.
pub fn get(text: &str, path: &str) -> Result<String, String> {
    let segments = parse_path(path)?;
    let reader = Reader::new(text);
    let mut node = reader.document()?;
    for segment in &segments {
        node = child(&node, segment, text)?.clone();
    }
    Ok(text[node.start..node.end].to_owned())
}

fn child<'n>(node: &'n Node, segment: &Segment, text: &str) -> Result<&'n Node, String> {
    let Some(group) = &node.group else {
        return Err(format!("`{}` has no fields", &text[node.start..node.end]));
    };
    match segment {
        Segment::Field(name) => group
            .items
            .iter()
            .find(|item| item.key.as_deref() == Some(name.as_str()))
            .map(|item| &item.value)
            .ok_or_else(|| format!("no field `{name}`")),
        Segment::Index(index) if group.open != b'{' && group.items.iter().all(|item| item.key.is_none()) => {
            group
                .items
                .get(*index)
                .map(|item| &item.value)
                .ok_or_else(|| format!("index {index} is out of range"))
        }
        Segment::Index(index) => Err(format!("element {index} is not addressable here")),
    }
}

/// `text` with the value at `path` replaced by `value` (a whole replacement for the empty path).
/// A struct field the text omits is appended to its struct, so an override of a defaulted field
/// still applies.
///
/// # Errors
///
/// The text does not parse, or the path's parent does not exist in it.
pub fn set(text: &str, path: &str, value: &str) -> Result<String, String> {
    let segments = parse_path(path)?;
    if segments.is_empty() {
        return Ok(value.to_owned());
    }
    let reader = Reader::new(text);
    let mut node = reader.document()?;
    let (last, parents) = segments.split_last().ok_or("empty path")?;
    for segment in parents {
        node = child(&node, segment, text)?.clone();
    }
    match child(&node, last, text) {
        Ok(target) => Ok(format!("{}{value}{}", &text[..target.start], &text[target.end..])),
        Err(missing) => {
            let (Segment::Field(name), Some(group)) = (last, &node.group) else {
                return Err(missing);
            };
            if !group.is_struct() {
                return Err(missing);
            }
            let (at, insert) = match group.items.last() {
                Some(item) => (item.value.end, format!(", {name}: {value}")),
                None => (group.inner_start, format!("{name}: {value}")),
            };
            Ok(format!("{}{insert}{}", &text[..at], &text[at..]))
        }
    }
}

/// The field-level differences that turn `base` into `edited`, as `(path, value text)` pairs
/// relative to `prefix`. Two structs with the same name and a superset of the base's fields are
/// compared field by field (a field the base omits is a difference); anything else that differs
/// is one whole replacement at `prefix`. Tuples and lists are compared whole, so a moved entity's
/// override is `.translation`, a path the inspector shows. Texts that do not parse are compared
/// whole.
#[must_use]
pub fn diff(base: &str, edited: &str, prefix: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if same_value(base, edited) {
        return out;
    }
    let (Ok(base_node), Ok(edited_node)) = (Reader::new(base).document(), Reader::new(edited).document())
    else {
        out.push((prefix.to_owned(), edited.trim().to_owned()));
        return out;
    };
    diff_nodes(base, &base_node, edited, &edited_node, prefix, &mut out);
    out
}

fn diff_nodes(base: &str, b: &Node, edited: &str, e: &Node, prefix: &str, out: &mut Vec<(String, String)>) {
    let b_text = &base[b.start..b.end];
    let e_text = &edited[e.start..e.end];
    if same_value(b_text, e_text) {
        return;
    }
    if let (Some(bg), Some(eg)) = (&b.group, &e.group) {
        let name = |text: &str, g: &Group| g.name.map(|(s, e)| text[s..e].to_owned());
        let comparable = bg.is_struct()
            && eg.is_struct()
            && !eg.items.is_empty()
            && name(base, bg) == name(edited, eg)
            && bg
                .items
                .iter()
                .all(|bi| eg.items.iter().any(|ei| ei.key == bi.key));
        if comparable {
            for item in &eg.items {
                let Some(key) = item.key.as_deref() else { continue };
                let path = format!("{prefix}.{key}");
                match bg.items.iter().find(|bi| bi.key.as_deref() == Some(key)) {
                    Some(base_item) => diff_nodes(base, &base_item.value, edited, &item.value, &path, out),
                    None => out.push((path, edited[item.value.start..item.value.end].to_owned())),
                }
            }
            return;
        }
    }
    out.push((prefix.to_owned(), e_text.to_owned()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_parse_and_join() {
        assert_eq!(parse_path("").expect("ron path"), vec![]);
        assert_eq!(parse_path(".").expect("ron path"), vec![]);
        assert_eq!(
            parse_path(".a.b[2].0").expect("ron path"),
            vec![
                Segment::Field("a".into()),
                Segment::Field("b".into()),
                Segment::Index(2),
                Segment::Index(0)
            ]
        );
        assert_eq!(
            parse_path("intensity").expect("ron path"),
            vec![Segment::Field("intensity".into())]
        );
        assert!(parse_path(".a..b").is_err());
        assert!(parse_path(".a[x]").is_err());
        assert_eq!(join_path(&parse_path(".a[1].b").expect("ron path")), ".a.1.b");
    }

    #[test]
    fn set_replaces_only_the_addressed_bytes() {
        let text = "(translation: (1.0, 0.0, 0.0), /* keep */ scale: (1.0, 1.0, 1.0))";
        let out = set(text, ".translation", "(5.0, 0.0, 0.0)").expect("ron path");
        assert_eq!(
            out,
            "(translation: (5.0, 0.0, 0.0), /* keep */ scale: (1.0, 1.0, 1.0))"
        );
        let out = set(text, ".scale.1", "2.0").expect("ron path");
        assert_eq!(
            out,
            "(translation: (1.0, 0.0, 0.0), /* keep */ scale: (1.0, 2.0, 1.0))"
        );
        let named = r#"Lamp(name: "a, (b)", intensity: 1.0)"#;
        assert_eq!(
            set(named, ".intensity", "2.5").expect("ron path"),
            r#"Lamp(name: "a, (b)", intensity: 2.5)"#
        );
        assert_eq!(set("[1, 2, 3]", "[1]", "9").expect("ron path"), "[1, 9, 3]");
        assert_eq!(set("(a: 1)", "", "(a: 2)").expect("ron path"), "(a: 2)");
    }

    #[test]
    fn set_appends_a_field_the_text_omits() {
        assert_eq!(set("(a: 1)", ".b", "2").expect("ron path"), "(a: 1, b: 2)");
        assert_eq!(set("(a: 1,)", ".b", "2").expect("ron path"), "(a: 1, b: 2,)");
        assert_eq!(set("()", ".b", "2").expect("ron path"), "(b: 2)");
        assert!(set("(1, 2)", ".b", "2").is_err(), "a tuple has no named fields");
        assert!(set("(a: 1)", ".x.y", "2").is_err(), "a missing parent is refused");
        assert!(set("(a: 1", ".a", "2").is_err(), "an unclosed group is refused");
    }

    #[test]
    fn get_reads_nested_values() {
        let text = "(t: (translation: (1.0, 2.0, 3.0)), list: [(x: 1), (x: 2)], s: Some(\"q\"))";
        assert_eq!(get(text, ".t.translation").expect("ron path"), "(1.0, 2.0, 3.0)");
        assert_eq!(get(text, ".t.translation.2").expect("ron path"), "3.0");
        assert_eq!(get(text, ".list[1].x").expect("ron path"), "2");
        assert_eq!(get(text, ".s").expect("ron path"), "Some(\"q\")");
        assert!(get(text, ".nope").is_err());
    }

    #[test]
    fn compact_ignores_layout_comments_and_trailing_commas() {
        assert!(same_value("(a: 1, b: \"x y\",)", "( a:1,b:\"x y\" )"));
        assert!(same_value("(a: 1) // c", "(a:1)"));
        assert!(!same_value("(a: \"x y\")", "(a: \"xy\")"));
        assert!(!same_value("A", "B"));
    }

    #[test]
    fn diff_descends_into_structs_only() {
        let base = "(translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0))";
        let edited = "(translation: (5.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0))";
        assert_eq!(
            diff(base, edited, ""),
            vec![(".translation".to_owned(), "(5.0, 0.0, 0.0)".to_owned())]
        );
        // A field the base omits is an override of its own.
        assert_eq!(
            diff("(a: 1)", "(a: 1, b: 2)", ""),
            vec![(".b".to_owned(), "2".to_owned())]
        );
        // A dropped field, a renamed struct or a changed enum variant replace the whole value.
        assert_eq!(
            diff("(a: 1, b: 2)", "(a: 1)", ""),
            vec![(String::new(), "(a: 1)".to_owned())]
        );
        assert_eq!(
            diff("A(a: 1)", "B(a: 1)", ""),
            vec![(String::new(), "B(a: 1)".to_owned())]
        );
        assert!(diff("(a: 1)", "( a: 1, )", "").is_empty());
        // Applying the diff to the base gives the edited value back.
        let mut out = base.to_owned();
        for (path, value) in diff(base, edited, "") {
            out = set(&out, &path, &value).expect("ron path");
        }
        assert!(same_value(&out, edited));
    }

    #[test]
    fn hostile_nesting_is_refused_not_overflowed() {
        let deep = format!("{}{}", "(".repeat(10_000), ")".repeat(10_000));
        assert!(get(&deep, ".a").is_err());
        assert!(set(&deep, ".a", "1").is_err());
    }
}
