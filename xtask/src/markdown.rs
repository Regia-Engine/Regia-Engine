// SPDX-License-Identifier: MPL-2.0
//! A small Markdown-to-HTML renderer for the book (`cargo xtask manual`).
//!
//! It covers the subset the book is written in, following the public CommonMark 0.31 and GitHub
//! Flavored Markdown (tables, strikethrough) specifications: ATX headings, paragraphs, fenced code,
//! bullet and ordered lists (nested, tight or loose), block quotes, pipe tables with alignment,
//! thematic breaks, HTML comments and raw block HTML; inline code spans, `*`/`_` emphasis and strong
//! emphasis, `~~` strikethrough, inline links and images, autolinks, raw inline HTML, backslash
//! escapes, entities and hard breaks. Reference-style links, setext headings and indented code
//! blocks are not used by the book and are not supported (indented lines are paragraph text).
//!
//! Three hooks let the caller decide what the book needs: the URL a link points to, the id of a
//! heading, and the HTML of a fenced code block (syntax highlighting).

/// Caller decisions while rendering.
pub trait Hooks {
    /// The `href`/`src` to emit for a link or image destination as written.
    fn link(&mut self, dest: &str) -> String {
        dest.to_string()
    }

    /// Whether this destination is an anchor. False means the label is kept and no `<a>` is
    /// emitted. The one-file manual uses that for a Markdown file it does not contain.
    fn emit_as_link(&mut self, _dest: &str) -> bool {
        true
    }

    /// The `id` of a heading, given its level and plain text. `None` emits no id.
    fn heading_id(&mut self, _level: u8, _text: &str) -> Option<String> {
        None
    }

    /// The HTML of a fenced code block, given its info string and code.
    fn code_block(&mut self, info: &str, code: &str) -> String {
        let lang = info.split([' ', ',']).next().unwrap_or("");
        if lang.is_empty() {
            format!("<pre><code>{}</code></pre>\n", escape(code))
        } else {
            format!(
                "<pre><code class=\"language-{}\">{}</code></pre>\n",
                escape(lang),
                escape(code)
            )
        }
    }
}

/// Hooks that change nothing.
pub struct PlainHooks;

impl Hooks for PlainHooks {}

/// HTML-escapes text (`&`, `<`, `>`, `"`).
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// Renders a Markdown document to HTML.
pub fn render(markdown: &str, hooks: &mut dyn Hooks) -> String {
    let text = markdown.replace("\r\n", "\n").replace('\t', "    ");
    let lines: Vec<&str> = text.split('\n').collect();
    let blocks = parse_blocks(&lines);
    let mut out = String::with_capacity(markdown.len() * 2);
    for block in &blocks {
        emit_block(block, hooks, &mut out, false);
    }
    out
}

/// Plain text of an inline Markdown fragment (markup removed), for heading ids and titles.
pub fn plain_text(inline_md: &str) -> String {
    let html = render_inline(inline_md, &mut PlainHooks);
    let mut out = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

/// Column alignment of a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    None,
    Left,
    Center,
    Right,
}

/// A parsed block.
#[derive(Debug, Clone, PartialEq)]
enum Block {
    Heading(u8, String),
    Paragraph(String),
    Code {
        info: String,
        code: String,
    },
    List {
        ordered: bool,
        start: u64,
        items: Vec<Vec<Block>>,
        loose: bool,
    },
    Quote(Vec<Block>),
    Table {
        head: Vec<String>,
        aligns: Vec<Align>,
        rows: Vec<Vec<String>>,
    },
    Rule,
    Html(String),
}

/// Number of leading spaces.
fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Removes up to `n` leading spaces.
fn dedent(line: &str, n: usize) -> &str {
    let k = indent_of(line).min(n);
    &line[k..]
}

/// An opening code fence: (indent, fence char, fence length, info string).
fn fence_open(line: &str) -> Option<(usize, char, usize, String)> {
    let indent = indent_of(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = rest.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = rest.chars().take_while(|c| *c == ch).count();
    if len < 3 {
        return None;
    }
    let info = rest[len..].trim().to_string();
    if ch == '`' && info.contains('`') {
        return None;
    }
    Some((indent, ch, len, info))
}

/// Whether `line` closes a fence of `ch` × `len`.
fn fence_close(line: &str, ch: char, len: usize) -> bool {
    let indent = indent_of(line);
    if indent > 3 {
        return false;
    }
    let rest = line[indent..].trim_end();
    rest.chars().all(|c| c == ch) && rest.chars().count() >= len
}

/// An ATX heading: (level, text).
fn heading(line: &str) -> Option<(u8, String)> {
    if indent_of(line) > 3 {
        return None;
    }
    let rest = line.trim_start();
    let level = rest.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let after = &rest[level..];
    if !after.is_empty() && !after.starts_with(' ') {
        return None;
    }
    let mut text = after.trim();
    // Optional closing sequence.
    let stripped = text.trim_end_matches('#');
    if stripped.len() != text.len() && (stripped.is_empty() || stripped.ends_with(' ')) {
        text = stripped.trim_end();
    }
    Some((level as u8, text.to_string()))
}

/// A thematic break (`---`, `***`, `___`, spaces allowed).
fn is_rule(line: &str) -> bool {
    if indent_of(line) > 3 {
        return false;
    }
    let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3
        && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

/// A list item marker: (indent, ordered, start number, content column, first-line content).
fn list_marker(line: &str) -> Option<(usize, bool, u64, usize, String)> {
    let indent = indent_of(line);
    let rest = &line[indent..];
    let (ordered, number, marker_len) = if let Some(c) = rest.chars().next()
        && (c == '-' || c == '*' || c == '+')
    {
        (false, 0, 1)
    } else {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 || digits > 9 {
            return None;
        }
        let delim = rest[digits..].chars().next()?;
        if delim != '.' && delim != ')' {
            return None;
        }
        (true, rest[..digits].parse().ok()?, digits + 1)
    };
    let after = &rest[marker_len..];
    if after.is_empty() {
        return Some((indent, ordered, number, indent + marker_len + 1, String::new()));
    }
    if !after.starts_with(' ') {
        return None;
    }
    let spaces = indent_of(after);
    let spaces = if spaces > 4 { 1 } else { spaces };
    let content_col = indent + marker_len + spaces;
    Some((
        indent,
        ordered,
        number,
        content_col,
        after[spaces.min(after.len())..].to_string(),
    ))
}

/// Splits a table row into cells (outer pipes optional; `\|` is a literal pipe).
fn split_cells(line: &str) -> Vec<String> {
    let mut t = line.trim();
    if let Some(s) = t.strip_prefix('|') {
        t = s;
    }
    if t.ends_with('|') && !t.ends_with("\\|") {
        t = &t[..t.len() - 1];
    }
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut in_code = 0usize;
    let chars: Vec<char> = t.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && chars.get(i + 1) == Some(&'|') {
            cur.push('|');
            i += 2;
            continue;
        }
        if c == '`' {
            let run = chars[i..].iter().take_while(|x| **x == '`').count();
            if in_code == 0 {
                in_code = run;
            } else if in_code == run {
                in_code = 0;
            }
            for _ in 0..run {
                cur.push('`');
            }
            i += run;
            continue;
        }
        if c == '|' && in_code == 0 {
            cells.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(c);
        }
        i += 1;
    }
    cells.push(cur.trim().to_string());
    cells
}

/// The alignments of a table delimiter row, or `None` when `line` is not one.
fn delimiter_row(line: &str) -> Option<Vec<Align>> {
    if !line.contains('-') {
        return None;
    }
    let cells = split_cells(line);
    let mut aligns = Vec::with_capacity(cells.len());
    for cell in cells {
        let c = cell.trim();
        let left = c.starts_with(':');
        let right = c.ends_with(':');
        let core = c.trim_matches(':');
        if core.is_empty() || !core.chars().all(|ch| ch == '-') {
            return None;
        }
        aligns.push(match (left, right) {
            (true, true) => Align::Center,
            (true, false) => Align::Left,
            (false, true) => Align::Right,
            (false, false) => Align::None,
        });
    }
    Some(aligns)
}

/// Whether `line` starts a block that interrupts a paragraph.
fn interrupts_paragraph(line: &str) -> bool {
    if line.trim().is_empty() {
        return true;
    }
    if fence_open(line).is_some() || heading(line).is_some() || is_rule(line) {
        return true;
    }
    let t = line.trim_start();
    if indent_of(line) <= 3 && t.starts_with('>') {
        return true;
    }
    if indent_of(line) <= 3 && t.starts_with("<!--") {
        return true;
    }
    if let Some((_, ordered, start, _, content)) = list_marker(line) {
        return !content.trim().is_empty() && (!ordered || start == 1);
    }
    false
}

/// Block-level HTML tags whose lines pass through untouched until a blank line.
const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "details",
    "div",
    "dl",
    "fieldset",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "ul",
];

/// Whether `line` opens a raw HTML block.
fn html_block_start(line: &str) -> bool {
    if indent_of(line) > 3 {
        return false;
    }
    let t = line.trim_start();
    let Some(rest) = t.strip_prefix('<') else {
        return false;
    };
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let name: String = rest.chars().take_while(char::is_ascii_alphanumeric).collect();
    !name.is_empty() && BLOCK_TAGS.contains(&name.to_ascii_lowercase().as_str())
}

/// Parses lines into blocks.
fn parse_blocks(lines: &[&str]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        // Fenced code.
        if let Some((indent, ch, len, info)) = fence_open(line) {
            let mut code = String::new();
            i += 1;
            while i < lines.len() && !fence_close(lines[i], ch, len) {
                code.push_str(dedent(lines[i], indent));
                code.push('\n');
                i += 1;
            }
            i += 1; // closing fence (or end of input)
            blocks.push(Block::Code { info, code });
            continue;
        }
        if let Some((level, text)) = heading(line) {
            blocks.push(Block::Heading(level, text));
            i += 1;
            continue;
        }
        if is_rule(line) {
            blocks.push(Block::Rule);
            i += 1;
            continue;
        }
        // HTML comment block.
        if indent_of(line) <= 3 && line.trim_start().starts_with("<!--") {
            let mut html = String::new();
            loop {
                html.push_str(lines[i]);
                html.push('\n');
                let done = lines[i].contains("-->");
                i += 1;
                if done || i >= lines.len() {
                    break;
                }
            }
            blocks.push(Block::Html(html));
            continue;
        }
        if html_block_start(line) {
            let mut html = String::new();
            while i < lines.len() && !lines[i].trim().is_empty() {
                html.push_str(lines[i]);
                html.push('\n');
                i += 1;
            }
            blocks.push(Block::Html(html));
            continue;
        }
        // Block quote.
        if indent_of(line) <= 3 && line.trim_start().starts_with('>') {
            let mut inner: Vec<String> = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                let t = l.trim_start();
                if indent_of(l) <= 3 && t.starts_with('>') {
                    let body = &t[1..];
                    inner.push(body.strip_prefix(' ').unwrap_or(body).to_string());
                } else if !l.trim().is_empty()
                    && !inner.last().is_none_or(|s| s.trim().is_empty())
                    && !interrupts_paragraph(l)
                {
                    inner.push(l.to_string()); // lazy continuation
                } else {
                    break;
                }
                i += 1;
            }
            let refs: Vec<&str> = inner.iter().map(String::as_str).collect();
            blocks.push(Block::Quote(parse_blocks(&refs)));
            continue;
        }
        // Table.
        if line.contains('|')
            && let Some(aligns) = lines.get(i + 1).and_then(|l| delimiter_row(l))
        {
            let head = split_cells(line);
            if head.len() == aligns.len() {
                let mut rows = Vec::new();
                i += 2;
                while i < lines.len() && !lines[i].trim().is_empty() && lines[i].contains('|') {
                    let mut cells = split_cells(lines[i]);
                    cells.resize(aligns.len(), String::new());
                    rows.push(cells);
                    i += 1;
                }
                blocks.push(Block::Table { head, aligns, rows });
                continue;
            }
        }
        // List.
        if let Some((indent, ordered, start, _, _)) = list_marker(line) {
            let (block, next) = parse_list(lines, i, indent, ordered, start);
            blocks.push(block);
            i = next;
            continue;
        }
        // Paragraph.
        let mut text = String::new();
        while i < lines.len() {
            let l = lines[i];
            if !text.is_empty() && interrupts_paragraph(l) {
                break;
            }
            if l.trim().is_empty() {
                break;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(l.trim_start());
            i += 1;
        }
        blocks.push(Block::Paragraph(text));
    }
    blocks
}

/// Parses a list starting at `start_line`; returns the block and the next line index.
fn parse_list(lines: &[&str], start_line: usize, indent: usize, ordered: bool, start: u64) -> (Block, usize) {
    let mut items: Vec<Vec<Block>> = Vec::new();
    let mut loose = false;
    let mut i = start_line;
    while i < lines.len() {
        let Some((m_indent, m_ordered, _, content_col, first)) = list_marker(lines[i]) else {
            break;
        };
        if m_ordered != ordered || m_indent.abs_diff(indent) > 1 || is_rule(lines[i]) {
            break;
        }
        // Collect the item's lines, dedented to its content column.
        let mut item: Vec<String> = vec![first];
        i += 1;
        let mut saw_blank = false;
        let mut item_has_inner_blank = false;
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty() {
                // A blank line continues the item only if indented content follows.
                let next = lines[i + 1..]
                    .iter()
                    .position(|x| !x.trim().is_empty())
                    .map(|k| i + 1 + k);
                match next {
                    Some(k) if indent_of(lines[k]) >= content_col => {
                        item_has_inner_blank = true;
                        item.push(String::new());
                        i += 1;
                        continue;
                    }
                    _ => {
                        saw_blank = true;
                        break;
                    }
                }
            }
            if indent_of(l) >= content_col {
                item.push(dedent(l, content_col).to_string());
                i += 1;
                continue;
            }
            // A sibling or a less-indented block ends the item; otherwise lazy continuation.
            if list_marker(l).is_some() || interrupts_paragraph(l) {
                break;
            }
            let last_is_text = item
                .last()
                .is_some_and(|s| !s.trim().is_empty() && fence_open(s).is_none());
            if last_is_text {
                item.push(l.trim_start().to_string());
                i += 1;
                continue;
            }
            break;
        }
        let refs: Vec<&str> = item.iter().map(String::as_str).collect();
        let blocks = parse_blocks(&refs);
        if item_has_inner_blank && blocks.len() > 1 {
            loose = true;
        }
        items.push(blocks);
        if saw_blank {
            // Skip the blank lines; continue only with a sibling item.
            let mut k = i;
            while k < lines.len() && lines[k].trim().is_empty() {
                k += 1;
            }
            let sibling = k < lines.len()
                && list_marker(lines[k])
                    .is_some_and(|(mi, mo, _, _, _)| mo == ordered && mi.abs_diff(indent) <= 1)
                && !is_rule(lines[k]);
            if sibling {
                loose = true;
                i = k;
            } else {
                break;
            }
        }
    }
    (
        Block::List {
            ordered,
            start,
            items,
            loose,
        },
        i,
    )
}

/// Writes one block.
fn emit_block(block: &Block, hooks: &mut dyn Hooks, out: &mut String, tight: bool) {
    match block {
        Block::Heading(level, text) => {
            let plain = plain_text(text);
            match hooks.heading_id(*level, &plain) {
                Some(id) => out.push_str(&format!("<h{level} id=\"{}\">", escape(&id))),
                None => out.push_str(&format!("<h{level}>")),
            }
            out.push_str(&render_inline(text, hooks));
            out.push_str(&format!("</h{level}>\n"));
        }
        Block::Paragraph(text) => {
            if tight {
                out.push_str(&render_inline(text, hooks));
                out.push('\n');
            } else {
                out.push_str("<p>");
                out.push_str(&render_inline(text, hooks));
                out.push_str("</p>\n");
            }
        }
        Block::Code { info, code } => out.push_str(&hooks.code_block(info, code)),
        Block::List {
            ordered,
            start,
            items,
            loose,
        } => {
            if *ordered {
                if *start == 1 {
                    out.push_str("<ol>\n");
                } else {
                    out.push_str(&format!("<ol start=\"{start}\">\n"));
                }
            } else {
                out.push_str("<ul>\n");
            }
            for item in items {
                out.push_str("<li>");
                for (k, b) in item.iter().enumerate() {
                    if k > 0 && !*loose && matches!(b, Block::Paragraph(_)) {
                        out.push('\n');
                    }
                    emit_block(b, hooks, out, !*loose);
                }
                // Trim the newline a tight paragraph left before `</li>`.
                if out.ends_with('\n') && !*loose && matches!(item.last(), Some(Block::Paragraph(_))) {
                    out.pop();
                }
                out.push_str("</li>\n");
            }
            out.push_str(if *ordered { "</ol>\n" } else { "</ul>\n" });
        }
        Block::Quote(blocks) => {
            out.push_str("<blockquote>\n");
            for b in blocks {
                emit_block(b, hooks, out, false);
            }
            out.push_str("</blockquote>\n");
        }
        Block::Table { head, aligns, rows } => {
            let attr = |a: Align| match a {
                Align::None => String::new(),
                Align::Left => " style=\"text-align:left\"".to_string(),
                Align::Center => " style=\"text-align:center\"".to_string(),
                Align::Right => " style=\"text-align:right\"".to_string(),
            };
            out.push_str("<table>\n<thead>\n<tr>");
            for (cell, a) in head.iter().zip(aligns) {
                out.push_str(&format!("<th{}>{}</th>", attr(*a), render_inline(cell, hooks)));
            }
            out.push_str("</tr>\n</thead>\n<tbody>\n");
            for row in rows {
                out.push_str("<tr>");
                for (cell, a) in row.iter().zip(aligns) {
                    out.push_str(&format!("<td{}>{}</td>", attr(*a), render_inline(cell, hooks)));
                }
                out.push_str("</tr>\n");
            }
            out.push_str("</tbody>\n</table>\n");
        }
        Block::Rule => out.push_str("<hr>\n"),
        Block::Html(html) => out.push_str(html),
    }
}

// ---------------------------------------------------------------------------
// Inlines
// ---------------------------------------------------------------------------

/// Whether `c` is ASCII punctuation (escapable with a backslash).
fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation()
}

/// The end (exclusive, in chars) of a code span opening at `i`, and its content range.
fn code_span(chars: &[char], i: usize) -> Option<(usize, usize, usize)> {
    let run = chars[i..].iter().take_while(|c| **c == '`').count();
    let mut j = i + run;
    while j < chars.len() {
        if chars[j] == '`' {
            let close = chars[j..].iter().take_while(|c| **c == '`').count();
            if close == run {
                return Some((j + close, i + run, j));
            }
            j += close;
        } else {
            j += 1;
        }
    }
    None
}

/// The index of the `]` matching the `[` at `i` (code spans and escapes skipped).
fn matching_bracket(chars: &[char], i: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut j = i;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 1,
            '`' => {
                if let Some((end, _, _)) = code_span(chars, j) {
                    j = end;
                    continue;
                }
            }
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Parses `(dest "title")` at `i`: returns (end index, destination, title).
fn link_tail(chars: &[char], i: usize) -> Option<(usize, String, Option<String>)> {
    if chars.get(i) != Some(&'(') {
        return None;
    }
    let mut j = i + 1;
    while j < chars.len() && chars[j] == ' ' {
        j += 1;
    }
    let mut dest = String::new();
    if chars.get(j) == Some(&'<') {
        j += 1;
        while j < chars.len() && chars[j] != '>' {
            dest.push(chars[j]);
            j += 1;
        }
        j += 1;
    } else {
        let mut depth = 0i32;
        while j < chars.len() {
            let c = chars[j];
            if c == '\\' && chars.get(j + 1).is_some_and(|n| is_punct(*n)) {
                dest.push(chars[j + 1]);
                j += 2;
                continue;
            }
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            } else if c == ' ' || c == '\n' {
                break;
            }
            dest.push(c);
            j += 1;
        }
    }
    while j < chars.len() && (chars[j] == ' ' || chars[j] == '\n') {
        j += 1;
    }
    let mut title = None;
    if let Some(&q) = chars.get(j)
        && (q == '"' || q == '\'')
    {
        let close = q;
        let mut t = String::new();
        j += 1;
        while j < chars.len() && chars[j] != close {
            t.push(chars[j]);
            j += 1;
        }
        j += 1;
        title = Some(t);
        while j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
    }
    if chars.get(j) != Some(&')') {
        return None;
    }
    Some((j + 1, dest, title))
}

/// Whether a delimiter run of `c` at `[start, end)` can open (left-flanking) emphasis.
fn can_open(chars: &[char], start: usize, end: usize, c: char) -> bool {
    let next = chars.get(end).copied();
    let prev = start.checked_sub(1).map(|k| chars[k]);
    let next_ws = next.is_none_or(char::is_whitespace);
    if next_ws {
        return false;
    }
    let next_punct = next.is_some_and(is_punct);
    let prev_ws_or_punct = prev.is_none_or(|p| p.is_whitespace() || is_punct(p));
    let left = !next_punct || prev_ws_or_punct;
    if c == '_' {
        left && prev.is_none_or(|p| !p.is_alphanumeric())
    } else {
        left
    }
}

/// Whether a delimiter run of `c` at `[start, end)` can close (right-flanking) emphasis.
fn can_close(chars: &[char], start: usize, end: usize, c: char) -> bool {
    let prev = start.checked_sub(1).map(|k| chars[k]);
    let next = chars.get(end).copied();
    if prev.is_none_or(char::is_whitespace) {
        return false;
    }
    let prev_punct = prev.is_some_and(is_punct);
    let next_ws_or_punct = next.is_none_or(|n| n.is_whitespace() || is_punct(n));
    let right = !prev_punct || next_ws_or_punct;
    if c == '_' {
        right && next.is_none_or(|n| !n.is_alphanumeric())
    } else {
        right
    }
}

/// Finds a closing run of at least `want` × `c` after `from` (code spans and links skipped).
fn find_closer(chars: &[char], from: usize, c: char, want: usize) -> Option<(usize, usize)> {
    let mut j = from;
    while j < chars.len() {
        let ch = chars[j];
        if ch == '\\' {
            j += 2;
            continue;
        }
        if ch == '`' {
            if let Some((end, _, _)) = code_span(chars, j) {
                j = end;
                continue;
            }
            j += chars[j..].iter().take_while(|x| **x == '`').count();
            continue;
        }
        if ch == '<'
            && let Some(end) = raw_html_end(chars, j)
        {
            j = end;
            continue;
        }
        if ch == c {
            let run = chars[j..].iter().take_while(|x| **x == c).count();
            if run >= want && can_close(chars, j, j + run, c) {
                return Some((j, run));
            }
            j += run;
            continue;
        }
        j += 1;
    }
    None
}

/// The end of raw inline HTML (a tag or comment) starting at `i`.
fn raw_html_end(chars: &[char], i: usize) -> Option<usize> {
    let next = *chars.get(i + 1)?;
    if next == '!' && chars.get(i + 2) == Some(&'-') && chars.get(i + 3) == Some(&'-') {
        let mut j = i + 4;
        while j + 2 < chars.len() {
            if chars[j] == '-' && chars[j + 1] == '-' && chars[j + 2] == '>' {
                return Some(j + 3);
            }
            j += 1;
        }
        return None;
    }
    let name_start = if next == '/' { i + 2 } else { i + 1 };
    if !chars.get(name_start).is_some_and(char::is_ascii_alphabetic) {
        return None;
    }
    let mut j = name_start;
    while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '-') {
        j += 1;
    }
    // Attributes, then `>` or `/>`.
    let mut quote: Option<char> = None;
    while j < chars.len() {
        let ch = chars[j];
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '>' => return Some(j + 1),
            None if ch == '<' || ch == '\n' && j > i + 200 => return None,
            None => {}
        }
        j += 1;
    }
    None
}

/// An autolink `<scheme://…>` or `<a@b.c>` at `i`: (end, url, is_email).
fn autolink(chars: &[char], i: usize) -> Option<(usize, String, bool)> {
    let mut j = i + 1;
    let mut s = String::new();
    while j < chars.len() && chars[j] != '>' {
        if chars[j] == ' ' || chars[j] == '<' || chars[j] == '\n' {
            return None;
        }
        s.push(chars[j]);
        j += 1;
    }
    if j >= chars.len() {
        return None;
    }
    if let Some(colon) = s.find(':') {
        let scheme = &s[..colon];
        if scheme.len() >= 2
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
        {
            return Some((j + 1, s, false));
        }
    }
    if s.contains('@') && !s.starts_with('@') && s.contains('.') {
        return Some((j + 1, s, true));
    }
    None
}

/// Whether `&…;` at `i` is an entity or numeric character reference (kept verbatim).
fn entity_end(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if chars.get(j) == Some(&'#') {
        j += 1;
        if chars.get(j).is_some_and(|c| *c == 'x' || *c == 'X') {
            j += 1;
        }
    }
    let start = j;
    while j < chars.len() && chars[j].is_ascii_alphanumeric() && j - start < 32 {
        j += 1;
    }
    (j > start && chars.get(j) == Some(&';')).then_some(j + 1)
}

/// Renders inline Markdown to HTML.
pub fn render_inline(text: &str, hooks: &mut dyn Hooks) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 16);
    inline_range(&chars, 0, chars.len(), hooks, &mut out);
    out
}

/// Renders `chars[from..to]`.
fn inline_range(chars: &[char], from: usize, to: usize, hooks: &mut dyn Hooks, out: &mut String) {
    let slice = &chars[from..to];
    let mut i = 0;
    let n = slice.len();
    while i < n {
        let c = slice[i];
        match c {
            '\\' => {
                if let Some(&next) = slice.get(i + 1) {
                    if next == '\n' {
                        out.push_str("<br>\n");
                        i += 2;
                        continue;
                    }
                    if is_punct(next) {
                        out.push_str(&escape(&next.to_string()));
                        i += 2;
                        continue;
                    }
                }
                out.push('\\');
                i += 1;
            }
            '`' => {
                if let Some((end, a, b)) = code_span(slice, i) {
                    let raw: String = slice[a..b].iter().collect::<String>().replace('\n', " ");
                    let content =
                        if raw.len() >= 2 && raw.starts_with(' ') && raw.ends_with(' ') && raw.trim() != "" {
                            &raw[1..raw.len() - 1]
                        } else {
                            raw.as_str()
                        };
                    out.push_str("<code>");
                    out.push_str(&escape(content));
                    out.push_str("</code>");
                    i = end;
                } else {
                    let run = slice[i..].iter().take_while(|x| **x == '`').count();
                    for _ in 0..run {
                        out.push('`');
                    }
                    i += run;
                }
            }
            '!' if slice.get(i + 1) == Some(&'[') => {
                if let Some(close) = matching_bracket(slice, i + 1)
                    && let Some((end, dest, title)) = link_tail(slice, close + 1)
                {
                    let alt: String = plain_text(&slice[i + 2..close].iter().collect::<String>());
                    let src = hooks.link(&dest);
                    out.push_str(&format!("<img src=\"{}\" alt=\"{}\"", escape(&src), escape(&alt)));
                    if let Some(t) = title {
                        out.push_str(&format!(" title=\"{}\"", escape(&t)));
                    }
                    out.push('>');
                    i = end;
                } else {
                    out.push('!');
                    i += 1;
                }
            }
            '[' => {
                if let Some(close) = matching_bracket(slice, i)
                    && let Some((end, dest, title)) = link_tail(slice, close + 1)
                {
                    if hooks.emit_as_link(&dest) {
                        let href = hooks.link(&dest);
                        out.push_str(&format!("<a href=\"{}\"", escape(&href)));
                        if let Some(t) = title {
                            out.push_str(&format!(" title=\"{}\"", escape(&t)));
                        }
                        out.push('>');
                        inline_range(slice, i + 1, close, hooks, out);
                        out.push_str("</a>");
                    } else {
                        inline_range(slice, i + 1, close, hooks, out);
                    }
                    i = end;
                } else {
                    out.push('[');
                    i += 1;
                }
            }
            '<' => {
                if let Some((end, url, email)) = autolink(slice, i) {
                    let href = if email {
                        format!("mailto:{url}")
                    } else {
                        hooks.link(&url)
                    };
                    out.push_str(&format!("<a href=\"{}\">{}</a>", escape(&href), escape(&url)));
                    i = end;
                } else if let Some(end) = raw_html_end(slice, i) {
                    out.extend(&slice[i..end]);
                    i = end;
                } else {
                    out.push_str("&lt;");
                    i += 1;
                }
            }
            '&' => {
                if let Some(end) = entity_end(slice, i) {
                    out.extend(&slice[i..end]);
                    i = end;
                } else {
                    out.push_str("&amp;");
                    i += 1;
                }
            }
            '*' | '_' | '~' => {
                let run = slice[i..].iter().take_while(|x| **x == c).count();
                let end = i + run;
                if c == '~' {
                    if run == 2
                        && can_open(slice, i, end, c)
                        && let Some((close, _)) = find_closer(slice, end, '~', 2)
                    {
                        out.push_str("<del>");
                        inline_range(slice, end, close, hooks, out);
                        out.push_str("</del>");
                        i = close + 2;
                        continue;
                    }
                } else if can_open(slice, i, end, c) {
                    let want = run.min(3);
                    if let Some((close, close_run)) = find_closer(slice, end, c, want.min(2)) {
                        let used = want.min(close_run);
                        // Extra opening delimiters stay literal.
                        for _ in 0..run - used {
                            out.push(c);
                        }
                        let (open_tag, close_tag) = match used {
                            1 => ("<em>", "</em>"),
                            2 => ("<strong>", "</strong>"),
                            _ => ("<em><strong>", "</strong></em>"),
                        };
                        out.push_str(open_tag);
                        inline_range(slice, end, close, hooks, out);
                        out.push_str(close_tag);
                        i = close + used;
                        continue;
                    }
                }
                for _ in 0..run {
                    out.push(c);
                }
                i = end;
            }
            '\n' => {
                // Two trailing spaces make a hard break.
                if out.ends_with("  ") {
                    let trimmed = out.trim_end_matches(' ').len();
                    out.truncate(trimmed);
                    out.push_str("<br>\n");
                } else {
                    let trimmed = out.trim_end_matches(' ').len();
                    out.truncate(trimmed);
                    out.push('\n');
                }
                i += 1;
            }
            '>' => {
                out.push_str("&gt;");
                i += 1;
            }
            '"' => {
                out.push_str("&quot;");
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(text: &str) -> String {
        render(text, &mut PlainHooks)
    }

    #[test]
    fn headings_paragraphs_and_rules() {
        assert_eq!(
            md("# Title #\n\nSome *text*.\n\n---\n"),
            "<h1>Title</h1>\n<p>Some <em>text</em>.</p>\n<hr>\n"
        );
        assert_eq!(md("#nope"), "<p>#nope</p>\n");
        assert_eq!(md("a\nb"), "<p>a\nb</p>\n");
    }

    #[test]
    fn inline_markup() {
        let html = render_inline(
            "**bold** and _em_ and snake_case_name and `a < b` and ~~gone~~",
            &mut PlainHooks,
        );
        assert_eq!(
            html,
            "<strong>bold</strong> and <em>em</em> and snake_case_name and <code>a &lt; b</code> and <del>gone</del>"
        );
        assert_eq!(render_inline("``a ` b``", &mut PlainHooks), "<code>a ` b</code>");
        assert_eq!(render_inline("2 * 3 * 4", &mut PlainHooks), "2 * 3 * 4");
        assert_eq!(render_inline("\\*lit\\*", &mut PlainHooks), "*lit*");
        assert_eq!(
            render_inline("a &amp; b & c", &mut PlainHooks),
            "a &amp; b &amp; c"
        );
        assert_eq!(
            render_inline("***both***", &mut PlainHooks),
            "<em><strong>both</strong></em>"
        );
    }

    #[test]
    fn links_images_and_autolinks() {
        assert_eq!(
            render_inline(
                "[the `x` page](a/b.md#c \"T\") ![alt *x*](i.png)",
                &mut PlainHooks
            ),
            "<a href=\"a/b.md#c\" title=\"T\">the <code>x</code> page</a> <img src=\"i.png\" alt=\"alt x\">"
        );
        assert_eq!(
            render_inline("<https://example.com/a> <!-- c --> <br>", &mut PlainHooks),
            "<a href=\"https://example.com/a\">https://example.com/a</a> <!-- c --> <br>"
        );
        assert_eq!(
            render_inline("[not a link] (x)", &mut PlainHooks),
            "[not a link] (x)"
        );
        assert_eq!(render_inline("a <b and c", &mut PlainHooks), "a &lt;b and c");
    }

    #[test]
    fn fenced_code_keeps_content() {
        let html = md("```rust,ignore\nlet a = 1 < 2;\n\n  indented\n```\n");
        assert_eq!(
            html,
            "<pre><code class=\"language-rust\">let a = 1 &lt; 2;\n\n  indented\n</code></pre>\n"
        );
        let html = md("- item\n\n  ```console\n  $ x\n  ```\n");
        assert!(
            html.contains("<li><p>item</p>\n<pre><code class=\"language-console\">$ x\n</code></pre>\n</li>"),
            "{html}"
        );
    }

    #[test]
    fn tight_and_loose_and_nested_lists() {
        assert_eq!(md("- a\n- b\n"), "<ul>\n<li>a</li>\n<li>b</li>\n</ul>\n");
        assert_eq!(
            md("- a\n\n- b\n"),
            "<ul>\n<li><p>a</p>\n</li>\n<li><p>b</p>\n</li>\n</ul>\n"
        );
        assert_eq!(
            md("1. one\n   continued\n2. two\n   - x\n   - y\n"),
            "<ol>\n<li>one\ncontinued</li>\n<li>two\n<ul>\n<li>x</li>\n<li>y</li>\n</ul>\n</li>\n</ol>\n"
        );
        assert_eq!(
            md("3. c\n4. d\n"),
            "<ol start=\"3\">\n<li>c</li>\n<li>d</li>\n</ol>\n"
        );
        // A paragraph after the list is not swallowed by it.
        assert_eq!(md("- a\n\nafter\n"), "<ul>\n<li>a</li>\n</ul>\n<p>after</p>\n");
        // Lazy continuation.
        assert_eq!(md("- a\nb\n"), "<ul>\n<li>a\nb</li>\n</ul>\n");
    }

    #[test]
    fn tables_with_alignment_and_escaped_pipes() {
        let html = md("| A | B |\n|:--|--:|\n| `x|y` | a \\| b |\n| only |\n");
        assert_eq!(
            html,
            "<table>\n<thead>\n<tr><th style=\"text-align:left\">A</th><th style=\"text-align:right\">B</th></tr>\n</thead>\n<tbody>\n\
             <tr><td style=\"text-align:left\"><code>x|y</code></td><td style=\"text-align:right\">a | b</td></tr>\n\
             <tr><td style=\"text-align:left\">only</td><td style=\"text-align:right\"></td></tr>\n</tbody>\n</table>\n"
        );
    }

    #[test]
    fn quotes_and_html_blocks() {
        assert_eq!(
            md("> **Note:** a\n> b\n"),
            "<blockquote>\n<p><strong>Note:</strong> a\nb</p>\n</blockquote>\n"
        );
        assert_eq!(
            md("<!-- generated -->\n\ntext\n"),
            "<!-- generated -->\n<p>text</p>\n"
        );
        assert_eq!(
            md("<div class=\"x\">\nraw *x*\n</div>\n"),
            "<div class=\"x\">\nraw *x*\n</div>\n"
        );
    }

    #[test]
    fn hooks_rewrite_links_ids_and_code() {
        struct H;
        impl Hooks for H {
            fn link(&mut self, dest: &str) -> String {
                format!("#{dest}")
            }
            fn heading_id(&mut self, level: u8, text: &str) -> Option<String> {
                Some(format!("h{level}-{}", text.to_lowercase().replace(' ', "-")))
            }
            fn code_block(&mut self, info: &str, code: &str) -> String {
                format!("[{info}:{}]", code.trim())
            }
        }
        let html = render("## The `x` Part\n\n[a](b.md)\n\n```ron\n()\n```\n", &mut H);
        assert_eq!(
            html,
            "<h2 id=\"h2-the-x-part\">The <code>x</code> Part</h2>\n<p><a href=\"#b.md\">a</a></p>\n[ron:()]"
        );
    }

    #[test]
    fn plain_text_strips_markup() {
        assert_eq!(
            plain_text("The `scene.*` *methods* & [links](x.md)"),
            "The scene.* methods & links"
        );
    }
}
