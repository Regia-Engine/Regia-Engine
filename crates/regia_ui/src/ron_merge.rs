// SPDX-License-Identifier: MPL-2.0
//! Comment-preserving `.ui.ron` saves (ADR-2489, `docs/specs/81-ui-designer.md` § 4.5): a three-way
//! text merge. The canonical text of the tree as it was opened and of the tree as it is now are
//! compared field by field, and only what differs is rewritten in the hand-written source; every
//! other byte, comments and key order included, is copied.
//!
//! The three texts are read with a small span parser of RON's shape (`scan`): values with their
//! byte ranges, and for each struct field or list element its *chunk*, which also holds the comment
//! lines above it, the comma and a trailing comment on the same line. Removing or moving a chunk
//! therefore takes its comments along. The merge knows nothing of UI nodes, and [`merge_ui_ron`]
//! parses its result and compares the tree before returning it, so a shape the merge gets wrong is
//! an error (the designer then writes the canonical form and says so), never a wrong file.

use std::collections::HashMap;

use crate::authoring::{UiFile, UiMigrators, parse_ui_ron, write_ui_ron};

/// Most nesting the span parser follows (the RON parser's own default limit).
const MAX_DEPTH: usize = 128;

/// A value on one line up to this length replaces or joins a hand-written file on one line.
const COMPACT_LIMIT: usize = 100;

/// Most elements the list alignment compares exactly (past this, unmatched elements pair by position).
const MAX_ALIGN: usize = 2048;

/// A parsed value: its bytes and, for a bracketed value, its elements.
#[derive(Debug, Clone)]
struct Value {
    /// Start of the value, a leading name included (`slider(…)`, `UiFile(…)`).
    start: usize,
    end: usize,
    /// The bracket, name excluded, for a sequence.
    seq: Option<Seq>,
    /// End of a leading name (`start..name_end`); `start` when it has none.
    name_end: usize,
}

/// The inside of `( )`, `[ ]`.
#[derive(Debug, Clone)]
struct Seq {
    /// Offset of the opening bracket.
    open: usize,
    /// Offset of the closing bracket.
    close: usize,
    /// End of what stays with the opening bracket (its own line, in a multi-line sequence).
    prefix_end: usize,
    /// Each element on its own line.
    multiline: bool,
    items: Vec<Item>,
}

/// One element of a sequence: a struct field or a list or tuple element.
#[derive(Debug, Clone)]
struct Item {
    /// Start of the chunk: leading comment lines included.
    chunk_start: usize,
    /// End of the chunk: comma, a trailing comment and the newline included.
    chunk_end: usize,
    /// The struct field's name; `None` for a list or tuple element.
    key: Option<(usize, usize)>,
    value: Value,
    /// The comma after the value.
    comma: Option<usize>,
    /// Where the element itself starts (its key, or its value).
    head: usize,
}

impl Seq {
    fn is_struct(&self) -> bool {
        !self.items.is_empty() && self.items.iter().all(|i| i.key.is_some())
    }

    fn is_plain(&self) -> bool {
        self.items.iter().all(|i| i.key.is_none())
    }
}

/// The span parser. Offsets stay on ASCII delimiters, so every slice is on a character boundary.
struct Scan<'a> {
    b: &'a [u8],
}

impl Scan<'_> {
    /// Past whitespace and comments.
    fn trivia(&self, mut i: usize) -> usize {
        loop {
            match self.b.get(i) {
                Some(c) if c.is_ascii_whitespace() => i += 1,
                Some(b'/') if self.b.get(i + 1) == Some(&b'/') => {
                    while i < self.b.len() && self.b[i] != b'\n' {
                        i += 1;
                    }
                }
                Some(b'/') if self.b.get(i + 1) == Some(&b'*') => i = self.block_comment(i),
                _ => return i,
            }
        }
    }

    /// Past a (nested) block comment opening at `open`.
    fn block_comment(&self, open: usize) -> usize {
        let mut nest = 1;
        let mut i = open + 2;
        while i < self.b.len() && nest > 0 {
            if self.b[i] == b'/' && self.b.get(i + 1) == Some(&b'*') {
                nest += 1;
                i += 2;
            } else if self.b[i] == b'*' && self.b.get(i + 1) == Some(&b'/') {
                nest -= 1;
                i += 2;
            } else {
                i += 1;
            }
        }
        i.min(self.b.len())
    }

    /// Past the rest of the line when only trivia remains on it: `Some(after the newline)`.
    fn rest_of_line(&self, mut i: usize) -> Option<usize> {
        loop {
            match self.b.get(i) {
                Some(b' ' | b'\t' | b'\r') => i += 1,
                Some(b'\n') => return Some(i + 1),
                None => return Some(i),
                Some(b'/') if self.b.get(i + 1) == Some(&b'/') => {
                    while i < self.b.len() && self.b[i] != b'\n' {
                        i += 1;
                    }
                }
                Some(b'/') if self.b.get(i + 1) == Some(&b'*') => {
                    let end = self.block_comment(i);
                    if self.b[i..end].contains(&b'\n') {
                        return None;
                    }
                    i = end;
                }
                _ => return None,
            }
        }
    }

    /// Past spaces and tabs.
    fn spaces(&self, mut i: usize) -> usize {
        while matches!(self.b.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        i
    }

    /// Past an identifier (raw `r#name` included) at `i`.
    fn ident(&self, i: usize) -> usize {
        let mut j = i;
        if self.b.get(j) == Some(&b'r') && self.b.get(j + 1) == Some(&b'#') {
            j += 2;
        }
        while self
            .b
            .get(j)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || !c.is_ascii())
        {
            j += 1;
        }
        j
    }

    /// Past a quoted literal opening at `open`.
    fn quoted(&self, open: usize, quote: u8) -> Option<usize> {
        let mut i = open + 1;
        while i < self.b.len() {
            if self.b[i] == b'\\' {
                i += 2;
            } else if self.b[i] == quote {
                return Some(i + 1);
            } else {
                i += 1;
            }
        }
        None
    }

    /// Past a raw string whose `#`s or quote start at `i`.
    fn raw_string(&self, mut i: usize) -> Option<usize> {
        let mut hashes = 0;
        while self.b.get(i) == Some(&b'#') {
            hashes += 1;
            i += 1;
        }
        if self.b.get(i) != Some(&b'"') {
            return None;
        }
        i += 1;
        while i < self.b.len() {
            if self.b[i] == b'"'
                && self
                    .b
                    .get(i + 1..i + 1 + hashes)
                    .is_some_and(|tail| tail.iter().all(|&c| c == b'#'))
            {
                return Some(i + 1 + hashes);
            }
            i += 1;
        }
        None
    }

    /// One value at `i` (no leading trivia).
    fn value(&self, i: usize, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        let plain = |end: usize| Value {
            start: i,
            end,
            seq: None,
            name_end: i,
        };
        match *self.b.get(i)? {
            b'(' | b'[' => {
                let seq = self.seq(i, depth)?;
                Some(Value {
                    start: i,
                    end: seq.close + 1,
                    seq: Some(seq),
                    name_end: i,
                })
            }
            b'{' => self.opaque(i).map(plain),
            b'"' => self.quoted(i, b'"').map(plain),
            b'\'' => self.quoted(i, b'\'').map(plain),
            c if c.is_ascii_alphabetic() || c == b'_' || !c.is_ascii() => {
                // Raw and byte strings look like names at first.
                if c == b'r'
                    && matches!(self.b.get(i + 1), Some(b'"' | b'#'))
                    && let Some(end) = self.raw_string(i + 1)
                {
                    return Some(plain(end));
                }
                if c == b'b' && self.b.get(i + 1) == Some(&b'"') {
                    return self.quoted(i + 1, b'"').map(plain);
                }
                if c == b'b' && self.b.get(i + 1) == Some(&b'\'') {
                    return self.quoted(i + 1, b'\'').map(plain);
                }
                if c == b'b'
                    && self.b.get(i + 1) == Some(&b'r')
                    && matches!(self.b.get(i + 2), Some(b'"' | b'#'))
                {
                    return self.raw_string(i + 2).map(plain);
                }
                let name_end = self.ident(i);
                let open = self.trivia(name_end);
                if matches!(self.b.get(open), Some(b'(')) {
                    let seq = self.seq(open, depth)?;
                    Some(Value {
                        start: i,
                        end: seq.close + 1,
                        seq: Some(seq),
                        name_end,
                    })
                } else {
                    Some(plain(name_end))
                }
            }
            b')' | b']' | b'}' | b',' | b':' => None,
            _ => {
                // A number: up to the next delimiter.
                let mut j = i;
                while self.b.get(j).is_some_and(|c| {
                    !(c.is_ascii_whitespace() || matches!(c, b',' | b')' | b']' | b'}' | b':' | b'/'))
                }) {
                    j += 1;
                }
                (j > i).then(|| plain(j))
            }
        }
    }

    /// Past a `{ }` map: kept whole, the `.ui.ron` shape has none.
    fn opaque(&self, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        let mut i = open;
        while i < self.b.len() {
            match self.b[i] {
                b'"' => {
                    i = self.quoted(i, b'"')?;
                    continue;
                }
                b'\'' => {
                    i = self.quoted(i, b'\'')?;
                    continue;
                }
                b'/' if matches!(self.b.get(i + 1), Some(b'/' | b'*')) => {
                    i = self.trivia(i);
                    continue;
                }
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// A bracketed sequence opening at `open`.
    fn seq(&self, open: usize, depth: usize) -> Option<Seq> {
        let closer = if self.b[open] == b'(' { b')' } else { b']' };
        let (mut chunk_start, multiline) = match self.rest_of_line(open + 1) {
            Some(after) => (after, true),
            None => (self.spaces(open + 1), false),
        };
        let prefix_end = chunk_start;
        let mut items = Vec::new();
        loop {
            let head = self.trivia(chunk_start);
            let c = *self.b.get(head)?;
            if c == closer {
                return Some(Seq {
                    open,
                    close: head,
                    prefix_end,
                    multiline,
                    items,
                });
            }
            let (key, value_at) = self.key(head);
            let value = self.value(value_at, depth + 1)?;
            let after_value = self.trivia(value.end);
            let (comma, after) = match self.b.get(after_value) {
                Some(b',') => (Some(after_value), after_value + 1),
                Some(c) if *c == closer => (None, value.end),
                _ => return None,
            };
            let chunk_end = self.rest_of_line(after).unwrap_or_else(|| self.spaces(after));
            items.push(Item {
                chunk_start,
                chunk_end,
                key,
                value,
                comma,
                head,
            });
            chunk_start = chunk_end;
        }
    }

    /// A struct field's `name:` at `i`: its span and where the value starts.
    fn key(&self, i: usize) -> (Option<(usize, usize)>, usize) {
        let c = self.b.get(i).copied().unwrap_or(0);
        if c.is_ascii_alphabetic() || c == b'_' {
            let end = self.ident(i);
            let colon = self.trivia(end);
            if self.b.get(colon) == Some(&b':') {
                return (Some((i, end)), self.trivia(colon + 1));
            }
        }
        (None, i)
    }
}

/// The root value of a RON text, past comments and `#![enable(…)]` attributes.
fn parse_root(text: &str) -> Option<Value> {
    let scan = Scan { b: text.as_bytes() };
    let mut i = scan.trivia(0);
    while text.as_bytes().get(i) == Some(&b'#') {
        let close = text[i..].find(']')? + i;
        i = scan.trivia(close + 1);
    }
    let value = scan.value(i, 0)?;
    (scan.trivia(value.end) >= text.len()).then_some(value)
}

/// One output element while a sequence is rebuilt.
struct Entry {
    /// Through the value's end.
    body: String,
    /// The comma, the trailing comment and the newline, as written.
    tail: String,
    has_comma: bool,
}

/// The three texts of one merge.
struct Merge<'a> {
    src: &'a str,
    old: &'a str,
    new: &'a str,
}

impl Merge<'_> {
    /// The merged text of one value. Unchanged values are copied from the source.
    fn value(&self, s: &Value, o: &Value, n: &Value) -> String {
        if compact(self.old, o) == compact(self.new, n) {
            return self.src[s.start..s.end].to_owned();
        }
        // `Some(x)` in a hand-written file is `x` with implicit `Some`.
        if s.seq.as_ref().is_some_and(|q| q.items.len() == 1)
            && &self.src[s.start..s.name_end] == "Some"
            && let Some(q) = &s.seq
            && o.name_end == o.start
        {
            let inner = self.value(&q.items[0].value, o, n);
            return format!(
                "{}{inner}{}",
                &self.src[s.start..q.items[0].value.start],
                &self.src[q.items[0].value.end..s.end]
            );
        }
        let same_name = self.old[o.start..o.name_end] == self.new[n.start..n.name_end];
        if let (Some(sq), Some(oq), Some(nq), true) = (&s.seq, &o.seq, &n.seq, same_name) {
            let same_bracket = self.src.as_bytes()[sq.open] == self.old.as_bytes()[oq.open]
                && self.old.as_bytes()[oq.open] == self.new.as_bytes()[nq.open];
            if same_bracket && sq.is_struct() && oq.is_struct() && nq.is_struct() {
                return self.structure(s, sq, oq, nq);
            }
            if same_bracket
                && sq.is_plain()
                && oq.is_plain()
                && nq.is_plain()
                && sq.items.len() == oq.items.len()
                && !sq.items.is_empty()
                && !nq.items.is_empty()
            {
                return self.list(s, sq, oq, nq);
            }
        }
        render(self.new, n, line_indent(self.src, s.start))
    }

    fn key<'t>(text: &'t str, item: &Item) -> &'t str {
        item.key.map_or("", |(a, b)| &text[a..b])
    }

    /// Merges a struct field by field. The source's field order stays; a new field goes after the
    /// source field that comes closest before it in the canonical order.
    fn structure(&self, s: &Value, sq: &Seq, oq: &Seq, nq: &Seq) -> String {
        let find = |seq: &'_ Seq, text: &str, key: &str| -> Option<usize> {
            seq.items.iter().position(|i| Self::key(text, i) == key)
        };
        let mut after_source: HashMap<usize, Vec<Entry>> = HashMap::new();
        let mut last_present = None;
        for item in &nq.items {
            let key = Self::key(self.new, item);
            if let Some(at) = find(sq, self.src, key) {
                last_present = Some(at);
            } else {
                let at = last_present.unwrap_or(usize::MAX);
                let indent = sq
                    .items
                    .first()
                    .map_or(String::new(), |f| indent_of(self.src, f.head));
                let value = render(self.new, &item.value, &indent);
                after_source.entry(at).or_default().push(new_entry(
                    &format!("{key}: {value}"),
                    &indent,
                    sq.multiline,
                ));
            }
        }
        let mut entries = after_source.remove(&usize::MAX).unwrap_or_default();
        for (index, item) in sq.items.iter().enumerate() {
            let key = Self::key(self.src, item);
            let old = find(oq, self.old, key).map(|i| &oq.items[i]);
            let new = find(nq, self.new, key).map(|i| &nq.items[i]);
            match (old, new) {
                // Reset to its default: the canonical form drops the field, and so does the file.
                (Some(_), None) => {}
                (Some(o), Some(n)) => {
                    let merged = self.value(&item.value, &o.value, &n.value);
                    entries.push(self.source_entry(item, Some(merged)));
                }
                // A default written out in the source that now holds a value.
                (None, Some(n)) => {
                    let indent = line_indent(self.src, item.value.start);
                    let text = render(self.new, &n.value, indent);
                    entries.push(self.source_entry(item, Some(text)));
                }
                (None, None) => entries.push(self.source_entry(item, None)),
            }
            if let Some(added) = after_source.remove(&index) {
                entries.extend(added);
            }
        }
        self.assemble(s, sq, entries)
    }

    /// Merges a list: unchanged elements are copied, changed ones merged, new ones written, gone
    /// ones dropped with their comments, moved ones carried with theirs.
    fn list(&self, s: &Value, sq: &Seq, oq: &Seq, nq: &Seq) -> String {
        let old_text: Vec<String> = oq.items.iter().map(|i| compact(self.old, &i.value)).collect();
        let new_text: Vec<String> = nq.items.iter().map(|i| compact(self.new, &i.value)).collect();
        let pairs = align(&old_text, &new_text);
        let indent = sq
            .items
            .first()
            .map_or(String::new(), |f| indent_of(self.src, f.head));
        let entries = nq
            .items
            .iter()
            .zip(&pairs)
            .map(|(item, pair)| match *pair {
                Some(o) => {
                    let merged = self.value(&sq.items[o].value, &oq.items[o].value, &item.value);
                    self.source_entry(&sq.items[o], Some(merged))
                }
                None => new_entry(&render(self.new, &item.value, &indent), &indent, sq.multiline),
            })
            .collect();
        self.assemble(s, sq, entries)
    }

    /// A chunk of the source, its value replaced when `value` is given.
    fn source_entry(&self, item: &Item, value: Option<String>) -> Entry {
        let value = value.unwrap_or_else(|| self.src[item.value.start..item.value.end].to_owned());
        let body = format!("{}{value}", &self.src[item.chunk_start..item.value.start]);
        Entry {
            body,
            tail: self.src[item.value.end..item.chunk_end].to_owned(),
            has_comma: item.comma.is_some(),
        }
    }

    /// The sequence's text from its entries. An element that gains a successor gets a comma; an
    /// inline element that became the last one loses its separator.
    fn assemble(&self, s: &Value, sq: &Seq, entries: Vec<Entry>) -> String {
        let mut out = self.src[s.start..sq.prefix_end].to_owned();
        let count = entries.len();
        for (i, entry) in entries.into_iter().enumerate() {
            out.push_str(&entry.body);
            let last = i + 1 == count;
            if last && !sq.multiline && entry.has_comma && !entry.tail.contains('\n') {
                // `(a: 1, b: 2)` without `b` is `(a: 1)`.
                let kept = entry.tail.trim_end().strip_suffix(',').unwrap_or(&entry.tail);
                out.push_str(kept);
            } else if !last && !entry.has_comma {
                out.push(',');
                if entry.tail.is_empty() {
                    out.push(' ');
                }
                out.push_str(&entry.tail);
            } else {
                out.push_str(&entry.tail);
            }
        }
        let suffix_start = sq.items.last().map_or(sq.prefix_end, |l| l.chunk_end);
        out.push_str(&self.src[suffix_start..s.end]);
        out
    }
}

/// A new element of a sequence.
fn new_entry(text: &str, indent: &str, multiline: bool) -> Entry {
    if multiline {
        Entry {
            body: format!("{indent}{text}"),
            tail: ",\n".to_owned(),
            has_comma: true,
        }
    } else {
        Entry {
            body: text.to_owned(),
            tail: ", ".to_owned(),
            has_comma: true,
        }
    }
}

/// For each new element: the old element it continues, by exact text in order (the longest common
/// subsequence between the first and last difference), then exact text anywhere (a move), then the
/// same `name` (a moved node that was also edited), then position inside the gap (an edit in place).
fn align(old: &[String], new: &[String]) -> Vec<Option<usize>> {
    fn pair(pairs: &mut [Option<usize>], used: &mut [bool], n: usize, o: usize) {
        pairs[n] = Some(o);
        used[o] = true;
    }
    let mut pairs = vec![None; new.len()];
    let mut used = vec![false; old.len()];
    let mut head = 0;
    while head < old.len() && head < new.len() && old[head] == new[head] {
        pair(&mut pairs, &mut used, head, head);
        head += 1;
    }
    let mut tail = 0;
    while tail < old.len() - head
        && tail < new.len() - head
        && old[old.len() - 1 - tail] == new[new.len() - 1 - tail]
    {
        pair(&mut pairs, &mut used, new.len() - 1 - tail, old.len() - 1 - tail);
        tail += 1;
    }
    let (o_mid, n_mid) = (&old[head..old.len() - tail], &new[head..new.len() - tail]);
    if o_mid.len() <= MAX_ALIGN && n_mid.len() <= MAX_ALIGN {
        // Longest common subsequence over the middle.
        let (h, w) = (o_mid.len(), n_mid.len());
        let mut table = vec![0u32; (h + 1) * (w + 1)];
        for i in (0..h).rev() {
            for j in (0..w).rev() {
                table[i * (w + 1) + j] = if o_mid[i] == n_mid[j] {
                    table[(i + 1) * (w + 1) + j + 1] + 1
                } else {
                    table[(i + 1) * (w + 1) + j].max(table[i * (w + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < h && j < w {
            if o_mid[i] == n_mid[j] {
                pair(&mut pairs, &mut used, head + j, head + i);
                i += 1;
                j += 1;
            } else if table[(i + 1) * (w + 1) + j] >= table[i * (w + 1) + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
    }
    // A move: the same text elsewhere.
    let mut free: HashMap<&str, Vec<usize>> = HashMap::new();
    for (o, text) in old.iter().enumerate().rev() {
        if !used[o] {
            free.entry(text).or_default().push(o);
        }
    }
    for (n, text) in new.iter().enumerate() {
        if pairs[n].is_none()
            && let Some(o) = free.get_mut(text.as_str()).and_then(Vec::pop)
        {
            pair(&mut pairs, &mut used, n, o);
        }
    }
    // The same name: a node moved and edited.
    for (n, text) in new.iter().enumerate() {
        if pairs[n].is_none()
            && let Some(wanted) = name_field(text)
            && let Some(o) = (0..old.len()).find(|&o| !used[o] && name_field(&old[o]) == Some(wanted))
        {
            pair(&mut pairs, &mut used, n, o);
        }
    }
    // An edit in place: unmatched elements between the same matched neighbors, in order.
    let mut next_old = 0;
    let mut n = 0;
    while n < new.len() {
        if let Some(o) = pairs[n] {
            next_old = next_old.max(o + 1);
        } else {
            let limit = pairs[n + 1..]
                .iter()
                .flatten()
                .next()
                .copied()
                .unwrap_or(old.len())
                .max(next_old);
            if let Some(o) = (next_old..limit).find(|&o| !used[o]) {
                pair(&mut pairs, &mut used, n, o);
                next_old = o + 1;
            }
        }
        n += 1;
    }
    pairs
}

/// The `name` a compact node starts with (`(name: "row", …)`; the canonical order puts it first).
fn name_field(compact: &str) -> Option<&str> {
    let rest = compact.strip_prefix("(name: \"")?;
    Some(&rest[..rest.find('"')?])
}

/// The value on one line: `(a: 1, b: [2, 3])`.
fn compact(text: &str, value: &Value) -> String {
    let Some(seq) = &value.seq else {
        return text[value.start..value.end].to_owned();
    };
    let mut out = String::with_capacity(value.end - value.start);
    out.push_str(&text[value.start..value.name_end]);
    out.push_str(&text[seq.open..=seq.open]);
    for (i, item) in seq.items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if let Some((a, b)) = item.key {
            out.push_str(&text[a..b]);
            out.push_str(": ");
        }
        out.push_str(&compact(text, &item.value));
    }
    out.push_str(&text[seq.close..=seq.close]);
    out
}

/// A canonical value as new text at `indent`: on one line when it is short, else as the writer
/// laid it out, shifted to `indent`.
fn render(text: &str, value: &Value, indent: &str) -> String {
    let short = compact(text, value);
    if short.len() <= COMPACT_LIMIT {
        return short;
    }
    let own = line_indent(text, value.start);
    let raw = &text[value.start..value.end];
    let mut out = String::with_capacity(raw.len());
    for (i, line) in raw.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            out.push_str(indent);
            out.push_str(line.strip_prefix(own).unwrap_or(line));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// The whitespace at the start of the line holding `at`.
fn line_indent(text: &str, at: usize) -> &str {
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let line = &text[start..at];
    &line[..line.len() - line.trim_start().len()]
}

/// The indent new siblings of the element at `head` take.
fn indent_of(text: &str, head: usize) -> String {
    line_indent(text, head).to_owned()
}

/// `source`, whose tree is `old`, rewritten so that it parses as `new`, with every byte outside the
/// changed fields kept. `Err` when the source is not readable by the span parser or the result is
/// not exactly `new`: the caller then falls back on the canonical writer.
pub fn merge_ui_ron(source: &str, old: &UiFile, new: &UiFile) -> Result<String, crate::UiError> {
    let fail = |reason: &str| crate::UiError::Write(format!("merge: {reason}"));
    if old == new {
        return Ok(source.to_owned());
    }
    let old_text = write_ui_ron(old)?;
    let new_text = write_ui_ron(new)?;
    let Some(text) = merge_ron_text(source, &old_text, &new_text) else {
        return Err(fail("the source is not plain RON"));
    };
    let back = parse_ui_ron(&text, "merged", &UiMigrators::default())?;
    if back != *new {
        return Err(fail("the merged text loads as a different tree"));
    }
    Ok(text)
}

/// The three-way merge of any RON text (ADR-9760): `source` is a hand-written text whose value
/// `old_text` writes canonically; the result is `source` with every value that differs between
/// `old_text` and `new_text` rewritten, and every other byte (comments, key order, layout) copied.
/// `None` when one of the three texts is not plain RON the span parser reads.
///
/// The merge knows no schema: a caller must load the result and compare it with the value it meant
/// to write, and fall back on the canonical text when they differ, as [`merge_ui_ron`] does.
#[must_use]
pub fn merge_ron_text(source: &str, old_text: &str, new_text: &str) -> Option<String> {
    let (s, o, n) = (parse_root(source)?, parse_root(old_text)?, parse_root(new_text)?);
    let merge = Merge {
        src: source,
        old: old_text,
        new: new_text,
    };
    let merged = merge.value(&s, &o, &n);
    Some(format!("{}{merged}{}", &source[..s.start], &source[s.end..]))
}

/// Why [`fuzz_check`] stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FuzzFailure {
    /// The text is not a loadable `.ui.ron`.
    Rejected(String),
    /// The code broke one of its promises on a text it accepted.
    Bug(String),
}

/// The `ui-repeat` fuzz target: any text that parses survives the code a v6 file reaches. Each
/// converter runs on hostile values, each repeat's keys go through the diff, and the file's tree with
/// a small edit merges back into the text, loading as exactly that tree (or refusing to merge).
pub fn fuzz_check(text: &str) -> Result<String, FuzzFailure> {
    let rejected = |e: crate::UiError| FuzzFailure::Rejected(e.to_string());
    let old = parse_ui_ron(text, "fuzz.ui.ron", &UiMigrators::default()).map_err(rejected)?;
    let source = crate::authoring::upgrade_ui_ron(text, "fuzz.ui.ron").map_err(rejected)?;
    let mut keys = Vec::new();
    fuzz_walk(&old.root, &mut keys);
    crate::repeat::unique_keys(&mut keys);
    let new_keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    let old_keys: Vec<&str> = new_keys.iter().rev().copied().collect();
    let plan = crate::repeat::plan_keyed_diff(&old_keys, &new_keys);
    if plan.sources.len() != new_keys.len() {
        return Err(FuzzFailure::Bug("the keyed plan lost items".to_owned()));
    }
    let mut new = old.clone();
    new.root.name = Some("fuzz \"edit\" ⟦é⟧".to_owned());
    new.root.children.reverse();
    if let Some(child) = new.root.children.first_mut() {
        child.opacity = Some(0.5);
    }
    // An `Err` is a refusal (the designer writes the canonical form).
    let Ok(merged) = merge_ui_ron(&source, &old, &new) else {
        return Ok(String::new());
    };
    match parse_ui_ron(&merged, "merged", &UiMigrators::default()) {
        Ok(back) if back == new => Ok(merged),
        _ => Err(FuzzFailure::Bug("the merge returned a different tree".to_owned())),
    }
}

fn fuzz_walk(node: &crate::authoring::UiNodeDef, keys: &mut Vec<String>) {
    for bind in node.bind.iter().chain(&node.bindings) {
        if let Some(converter) = &bind.converter {
            for raw in [
                "",
                "NaN",
                "inf",
                "-1e308",
                "1e308",
                "18446744073709551615",
                "true",
                "é(",
                "Some(3)",
            ] {
                let _ = converter.apply(raw);
            }
            let _ = converter.invert(&crate::interaction::UiValue::Number(f32::NAN));
            let _ = converter.invert(&crate::interaction::UiValue::Number(1.0));
            let _ = converter.problem();
        }
    }
    if let Some(repeat) = &node.repeat {
        keys.push(repeat.source.clone());
        keys.extend(repeat.key.clone());
    }
    for child in &node.children {
        fuzz_walk(child, keys);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authoring::{BindDef, BindTargetDef, RepeatDef, UI_RON_VERSION, UiNodeDef, WidgetDef};
    use crate::style::Dimension;

    const SOURCE: &str = r#"// HUD layout, hand-written.
#![enable(implicit_some)]
(
    version: 11, // bumped by hand
    root: (
        // Column root.
        style: (direction: column, padding: 16, gap: (8, 8)),
        children: [
            // The score line.
            (
                name: "score", // shown top left
                classes: ["surface"],
                style: (padding: 8, size: (200, Auto)),
                text: (spans: ["Score: 0"], style: (font_px: 22)),
                bind: (source: "Score.value", format: "Score: {:03}"),
            ),
            /* The bar. */
            (name: "health", widget: progress(1.0), style: (size: (200, 10))),
            (
                widget: button,
                interactable: true,
                command: "menu.start",
                children: [(
                    text: (spans: [Loc("menu-start")]),
                )],
            ),
            // Trailing note before the bracket.
        ],
    ),
)
// End of file.
"#;

    fn tree(text: &str) -> UiFile {
        parse_ui_ron(text, "t", &UiMigrators::default()).expect("parses")
    }

    /// Merges one edit and returns the text; the text always parses to the edited tree (the merge
    /// checks) and is checked here as well.
    fn merged(edit: impl FnOnce(&mut UiFile)) -> String {
        let old = tree(SOURCE);
        let mut new = old.clone();
        edit(&mut new);
        let text = merge_ui_ron(SOURCE, &old, &new).expect("merges");
        assert_eq!(tree(&text), new, "{text}");
        text
    }

    /// The lines of `text` not in `SOURCE`, and the lines of `SOURCE` not in `text`, in order.
    fn line_diff(text: &str) -> (Vec<String>, Vec<String>) {
        let lines = |t: &str| t.lines().map(str::to_owned).collect::<Vec<_>>();
        let (a, b) = (lines(SOURCE), lines(text));
        let added = b.iter().filter(|l| !a.contains(l)).cloned().collect();
        let gone = a.iter().filter(|l| !b.contains(l)).cloned().collect();
        (added, gone)
    }

    #[test]
    fn no_change_is_byte_identical() {
        assert_eq!(merged(|_| {}), SOURCE);
    }

    #[test]
    fn one_property_changes_one_line() {
        let text = merged(|f| {
            f.root.children[0].style.padding = crate::style::Edges::px(4.0);
        });
        let (added, gone) = line_diff(&text);
        assert_eq!(
            added,
            ["                style: (padding: 4.0, size: (200, Auto)),"]
        );
        assert_eq!(gone, ["                style: (padding: 8, size: (200, Auto)),"]);
    }

    #[test]
    fn a_string_edit_changes_only_that_string() {
        let text = merged(|f| f.root.children[0].name = Some("points".to_owned()));
        let expected = SOURCE.replace(r#"name: "score", // shown"#, r#"name: "points", // shown"#);
        assert_eq!(text, expected);
    }

    #[test]
    fn a_nested_inline_edit_keeps_the_line_shape() {
        let text = merged(|f| {
            f.root.children[1].style.size = [Dimension::Points(300.0), Dimension::Points(10.0)];
        });
        let expected = SOURCE.replace("style: (size: (200, 10))),", "style: (size: (300.0, 10))),");
        assert_eq!(text, expected);
    }

    #[test]
    fn a_new_field_goes_on_its_own_line_after_its_canonical_neighbor() {
        let text = merged(|f| f.root.children[2].disabled = true);
        let expected = SOURCE.replace(
            "                interactable: true,\n",
            "                interactable: true,\n                disabled: true,\n",
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn a_new_field_joins_an_inline_struct() {
        let text = merged(|f| f.root.children[1].opacity = Some(0.5));
        assert!(
            text.contains(
                r#"(name: "health", widget: progress(1.0), style: (size: (200, 10)), opacity: 0.5),"#
            ),
            "{text}"
        );
        let (_, gone) = line_diff(&text);
        assert_eq!(gone.len(), 1);
    }

    #[test]
    fn a_reset_field_goes_with_its_line_and_comment() {
        let text = merged(|f| f.root.children[0].name = None);
        let expected = SOURCE.replace("                name: \"score\", // shown top left\n", "");
        assert_eq!(text, expected);
    }

    #[test]
    fn a_reset_last_inline_field_leaves_no_separator() {
        let text = merged(|f| f.root.children[1].style = crate::style::Style::default());
        assert!(
            text.contains(r#"(name: "health", widget: progress(1.0)),"#),
            "{text}"
        );
    }

    #[test]
    fn removing_a_child_takes_its_comment_and_nothing_else() {
        let text = merged(|f| {
            f.root.children.remove(1);
        });
        let expected = SOURCE.replace(
            "            /* The bar. */\n            (name: \"health\", widget: progress(1.0), style: (size: (200, 10))),\n",
            "",
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn a_new_child_is_inserted_between_its_neighbors() {
        let text = merged(|f| {
            f.root.children.insert(
                1,
                UiNodeDef {
                    name: Some("spacer".to_owned()),
                    ..UiNodeDef::default()
                },
            );
        });
        let expected = SOURCE.replace(
            "            /* The bar. */\n",
            "            (name: \"spacer\"),\n            /* The bar. */\n",
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn moving_a_child_carries_its_comments() {
        let text = merged(|f| {
            let moved = f.root.children.remove(0);
            f.root.children.push(moved);
        });
        let mut lines: Vec<&str> = SOURCE.lines().collect();
        let mut got: Vec<&str> = text.lines().collect();
        lines.sort_unstable();
        got.sort_unstable();
        assert_eq!(lines, got, "only the order changed:\n{text}");
        let comment = text.find("// The score line.").expect("comment kept");
        let button = text.find("widget: button").expect("button");
        assert!(button < comment, "the comment moved with its node:\n{text}");
    }

    #[test]
    fn edits_inside_a_moved_named_child_still_keep_its_comments() {
        let text = merged(|f| {
            let mut moved = f.root.children.remove(0);
            moved.command = Some("score.open".to_owned());
            moved.interactable = true;
            f.root.children.push(moved);
        });
        assert!(text.contains("// shown top left"), "{text}");
        assert!(text.contains("// The score line."), "{text}");
    }

    #[test]
    fn edits_deep_in_the_tree_touch_one_line() {
        let text = merged(|f| {
            f.root.children[2].children[0].text = Some(crate::authoring::TextDef {
                spans: vec![crate::authoring::TextSpanDef::Lit("Go".to_owned())],
                align: Default::default(),
                wrap: Default::default(),
                overflow: Default::default(),
                line_height: 1.2,
                style: Default::default(),
                markup: false,
            });
        });
        let (added, gone) = line_diff(&text);
        assert_eq!(gone.len(), 1, "{text}");
        assert_eq!(added, [r#"                    text: (spans: ["Go"]),"#]);
    }

    #[test]
    fn an_older_file_changes_only_its_version_line_when_opened_and_saved() {
        // The designer merges against the upgraded text, so the version line is the only change.
        let v2 = SOURCE.replacen("version: 11", "version: 2", 1);
        let upgraded = crate::authoring::upgrade_ui_ron(&v2, "t").expect("upgrade");
        let old = tree(&upgraded);
        let text = merge_ui_ron(&upgraded, &old, &old).expect("merge");
        assert_eq!(text, SOURCE);
        assert_eq!(old.version, UI_RON_VERSION);
    }

    #[test]
    fn repeat_and_bindings_are_added_in_the_format_shape() {
        let text = merged(|f| {
            let node = &mut f.root.children[1];
            node.repeat = Some(RepeatDef {
                source: "Vm.items".to_owned(),
                key: Some("id".to_owned()),
                ..RepeatDef::default()
            });
            node.bindings = vec![BindDef {
                source: ".hp".to_owned(),
                target: BindTargetDef::Progress,
                ..BindDef::default()
            }];
            node.widget = Some(WidgetDef::Progress(0.5));
        });
        assert!(
            text.contains(r#"repeat: (source: "Vm.items", key: "id")"#),
            "{text}"
        );
        assert!(text.contains("progress(0.5)"), "{text}");
    }

    #[test]
    fn long_new_values_are_laid_out_at_the_field_indent() {
        let mut node = UiNodeDef::default();
        for i in 0..8 {
            node.children.push(UiNodeDef {
                name: Some(format!("child-with-a-long-name-{i}")),
                ..UiNodeDef::default()
            });
        }
        let text = merged(|f| f.root.children.push(node));
        assert!(text.contains("                children: [\n"), "{text}");
        let (_, gone) = line_diff(&text);
        assert!(gone.is_empty(), "appending changes no line: {gone:?}");
    }

    /// A source with `Some(...)` wrappers and defaults spelled out merges too.
    #[test]
    fn explicit_some_and_written_defaults_are_understood() {
        let src = r#"(version: 9, root: (
    clip: false,
    name: Some("a"),
    text: Some((spans: ["x"])),
    children: [],
))"#;
        let old = tree(src);
        let mut new = old.clone();
        new.root.clip = true;
        new.root.name = Some("b".to_owned());
        new.root.text = None;
        new.root.children.push(UiNodeDef::default());
        let text = merge_ui_ron(src, &old, &new).expect("merge");
        assert_eq!(tree(&text), new, "{text}");
        assert!(text.contains("clip: true"), "{text}");
        assert!(text.contains("name: Some(\"b\")"), "{text}");
    }

    #[test]
    fn broken_sources_are_errors_not_panics() {
        let old = tree(SOURCE);
        let mut new = old.clone();
        new.root.children.clear();
        for src in ["", "(", ")", "(version: 9, root: (", "\"", "/*", "(a: )", "#!["] {
            assert!(merge_ui_ron(src, &old, &new).is_err(), "{src:?}");
        }
    }

    #[test]
    fn non_ascii_text_and_comments_survive() {
        let src = "// héllo ⟦x⟧\n(version: 9, root: (name: \"é\", /* ñ */ children: [(name: \"日本語\"), (name: \"b\")]))\n";
        let old = tree(src);
        let mut new = old.clone();
        new.root.children[1].name = Some("ü".to_owned());
        let text = merge_ui_ron(src, &old, &new).expect("merge");
        assert_eq!(text, src.replace("\"b\"", "\"ü\""));
    }
}
