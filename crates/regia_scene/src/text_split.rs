// SPDX-License-Identifier: MPL-2.0
//! Linear-time splitting of scene text into the document header and its entity records.
//!
//! `ron` 0.12.2 parses numbers reached through `deserialize_any` (every `RawValue` capture and
//! every `ron::Value`) with `Parser::next_bytes_is_float`, which searches the *whole remaining
//! input* for `".."`. In a document without `".."` every such number scans to the end of the
//! file, so a scene's load time grows with (numbers × file size): 10,000 entities took 50–70 s in
//! release and ~80 s in debug. The fix is upstream on `ron`'s master branch but not
//! released yet.
//!
//! Instead of parsing the whole document in one `ron` call, [`split_scene_text`] runs one
//! lexical pass that finds the top-level `entities: [ ... ]` list and the byte range of each
//! element. The caller parses each element on its own (the search is then bounded by the
//! element's length) and the remaining document with the list body blanked out. Blanking keeps
//! every byte offset, line and column of the header, and entity errors are mapped back with
//! [`relocate_error`], so error positions match a whole-document parse.
//!
//! The splitter is deliberately conservative: anything it does not fully understand (RON
//! extensions, unterminated tokens, mismatched brackets, a missing or repeated `entities` key)
//! returns `None`, and the caller falls back to the whole-document parse.

use std::ops::Range;

/// The result of [`split_scene_text`].
pub(crate) struct SplitScene {
    /// The document with the body of the `entities` list replaced by spaces (newlines kept), so
    /// it reads as `entities: [ ]` with all positions unchanged.
    pub header: String,
    /// Byte ranges of the list's elements in the original text, in order, without surrounding
    /// whitespace or comments.
    pub entities: Vec<Range<usize>>,
}

/// One lexical token of RON as far as structure is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Open(u8),
    Close(u8),
    Comma,
    Colon,
    /// An identifier or number (the bytes `start..end` of the text).
    Word,
    /// A string, byte string, char or any other single character.
    Other,
    Eof,
}

/// A byte cursor over RON text. Every token boundary it produces sits on an ASCII byte or the
/// end of the text, so slicing at them is always valid UTF-8.
struct Lexer<'a> {
    bytes: &'a [u8],
    pos: usize,
}

/// `None` means "not understood": the caller falls back to the plain parse.
type Lex<T> = Option<T>;

impl Lexer<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    /// Skips whitespace and comments (line comments, nested block comments).
    fn skip_trivia(&mut self) -> Lex<()> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C) => self.pos += 1,
                Some(b'/') if self.peek_at(1) == Some(b'/') => {
                    while let Some(byte) = self.peek() {
                        if byte == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    self.pos += 2;
                    let mut level = 1usize;
                    while level > 0 {
                        match (self.peek()?, self.peek_at(1)) {
                            (b'/', Some(b'*')) => {
                                level += 1;
                                self.pos += 2;
                            }
                            (b'*', Some(b'/')) => {
                                level -= 1;
                                self.pos += 2;
                            }
                            _ => self.pos += 1,
                        }
                    }
                }
                _ => return Some(()),
            }
        }
    }

    /// Reads the next token after trivia. Returns the token and its start offset.
    fn next(&mut self) -> Lex<(Token, usize)> {
        self.skip_trivia()?;
        let start = self.pos;
        let Some(byte) = self.peek() else {
            return Some((Token::Eof, start));
        };
        let token = match byte {
            b'(' | b'[' | b'{' => {
                self.pos += 1;
                Token::Open(byte)
            }
            b')' | b']' | b'}' => {
                self.pos += 1;
                Token::Close(byte)
            }
            b',' => {
                self.pos += 1;
                Token::Comma
            }
            b':' => {
                self.pos += 1;
                Token::Colon
            }
            b'"' => {
                self.pos += 1;
                self.quoted_string()?;
                Token::Other
            }
            b'\'' => {
                self.pos += 1;
                self.char_literal()?;
                Token::Other
            }
            // Extensions (`#![enable(...)]`) change how values parse: leave them to `ron`.
            b'#' => return None,
            byte if is_word_byte(byte) => {
                while self.peek().is_some_and(is_word_byte) {
                    self.pos += 1;
                }
                let word = &self.bytes[start..self.pos];
                match (word, self.peek()) {
                    (b"b" | b"r" | b"br", Some(b'"')) | (b"r" | b"br", Some(b'#')) => {
                        if self.raw_or_byte_string(word)? {
                            Token::Other
                        } else {
                            Token::Word
                        }
                    }
                    (b"b", Some(b'\'')) => {
                        self.pos += 1;
                        self.char_literal()?;
                        Token::Other
                    }
                    _ => Token::Word,
                }
            }
            _ => {
                self.pos += 1;
                Token::Other
            }
        };
        Some((token, start))
    }

    /// After the opening `"`: skips to just past the closing quote, honouring `\` escapes.
    fn quoted_string(&mut self) -> Lex<()> {
        loop {
            match self.peek()? {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Some(());
                }
                _ => self.pos += 1,
            }
        }
    }

    /// After the opening `'`: one (possibly escaped) character, then the closing `'`.
    fn char_literal(&mut self) -> Lex<()> {
        if self.peek()? == b'\\' {
            self.pos += 2;
        }
        // The rest of the char (a multi-byte character or `\u{...}`) runs to the closing quote.
        loop {
            match self.peek()? {
                b'\'' => {
                    self.pos += 1;
                    return Some(());
                }
                b'\n' => return None,
                _ => self.pos += 1,
            }
        }
    }

    /// At `"` or `#` after a `b`/`r`/`br` prefix. Returns `true` for a string (skipped) and
    /// `false` for a raw identifier `r#name` (skipped as a word).
    fn raw_or_byte_string(&mut self, prefix: &[u8]) -> Lex<bool> {
        if prefix == b"b" {
            // b"..." uses ordinary escapes.
            self.pos += 1;
            self.quoted_string()?;
            return Some(true);
        }
        let mut hashes = 0usize;
        while self.peek() == Some(b'#') {
            hashes += 1;
            self.pos += 1;
        }
        if self.peek() != Some(b'"') {
            // `r#ident` (raw identifier); `br#` without a quote is not valid RON.
            if prefix != b"r" || hashes != 1 {
                return None;
            }
            while self.peek().is_some_and(is_word_byte) {
                self.pos += 1;
            }
            return Some(false);
        }
        self.pos += 1;
        // Raw string: no escapes, ends at `"` followed by `hashes` hashes.
        loop {
            if self.peek()? == b'"' && (1..=hashes).all(|offset| self.peek_at(offset) == Some(b'#')) {
                self.pos += 1 + hashes;
                return Some(true);
            }
            self.pos += 1;
        }
    }
}

/// Bytes of identifiers, numbers and raw identifiers. Non-ASCII bytes count as word bytes, so
/// tokens never end inside a multi-byte character.
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-') || byte >= 0x80
}

/// Splits a scene document into its header and the byte ranges of its `entities` elements.
///
/// Returns `None` when the text is not a plain struct document with exactly one top-level
/// `entities: [...]` field, or when the lexer meets anything it does not understand; the caller
/// then parses the whole text with `ron`, which also produces the right error.
pub(crate) fn split_scene_text(text: &str) -> Option<SplitScene> {
    let mut lexer = Lexer {
        bytes: text.as_bytes(),
        pos: 0,
    };
    let mut stack: Vec<u8> = Vec::new();
    // Byte range of the list body (between `[` and `]`) and its elements.
    let mut list: Option<(Range<usize>, Vec<Range<usize>>)> = None;
    // Set while the lexer is inside the list: depth of the list's `[` on the stack, start and
    // end of the current element's tokens.
    let mut in_list: Option<(usize, usize)> = None;
    let mut element: Option<(usize, usize)> = None;
    let mut elements: Vec<Range<usize>> = Vec::new();
    // The token before the current one at depth 1 was a word `entities` / that plus `:`.
    let mut key_state = 0u8;

    loop {
        let (token, start) = lexer.next()?;
        let end = lexer.pos;

        if let Some((list_depth, list_open)) = in_list {
            let at_list_level = stack.len() == list_depth;
            match token {
                Token::Comma if at_list_level => {
                    let (from, to) = element.take()?;
                    elements.push(from..to);
                }
                Token::Close(b']') if at_list_level => {
                    if let Some((from, to)) = element.take() {
                        elements.push(from..to);
                    }
                    stack.pop();
                    list = Some((list_open + 1..start, std::mem::take(&mut elements)));
                    in_list = None;
                }
                Token::Eof => return None,
                _ => {
                    match token {
                        Token::Open(byte) => stack.push(byte),
                        Token::Close(byte) if stack.pop() != Some(matching_open(byte)) => {
                            return None;
                        }
                        _ => {}
                    }
                    element = Some((element.map_or(start, |(from, _)| from), end));
                }
            }
            continue;
        }

        match token {
            Token::Eof => break,
            Token::Open(byte) => {
                if stack.is_empty() && byte != b'(' {
                    return None;
                }
                if key_state == 2 && byte == b'[' && stack.len() == 1 {
                    if list.is_some() {
                        // `entities` twice: let `ron` report the duplicate field.
                        return None;
                    }
                    stack.push(byte);
                    in_list = Some((stack.len(), start));
                    key_state = 0;
                    continue;
                }
                if key_state == 2 {
                    // `entities:` followed by something other than a list.
                    return None;
                }
                stack.push(byte);
            }
            Token::Close(byte) => {
                if stack.pop() != Some(matching_open(byte)) {
                    return None;
                }
            }
            _ if key_state == 2 => return None,
            Token::Word if stack.len() == 1 && text.get(start..end) == Some("entities") => {
                key_state = 1;
                continue;
            }
            Token::Colon if key_state == 1 => {
                key_state = 2;
                continue;
            }
            _ => {}
        }
        key_state = 0;
    }
    if !stack.is_empty() {
        return None;
    }
    let (body, entities) = list?;

    let mut header = String::with_capacity(text.len());
    header.push_str(&text[..body.start]);
    header.extend(
        text[body.clone()]
            .chars()
            .map(|c| if c == '\n' { '\n' } else { ' ' }),
    );
    header.push_str(&text[body.end..]);
    Some(SplitScene { header, entities })
}

fn matching_open(close: u8) -> u8 {
    match close {
        b')' => b'(',
        b']' => b'[',
        _ => b'{',
    }
}

/// Moves an error reported for `text[offset..]` (parsed on its own) to its position in `text`.
pub(crate) fn relocate_error(
    mut error: ron::error::SpannedError,
    text: &str,
    offset: usize,
) -> ron::error::SpannedError {
    let before = &text[..offset];
    let line_offset = before.matches('\n').count();
    let col_offset = before.rsplit('\n').next().map_or(0, |line| line.chars().count());
    for position in [&mut error.span.start, &mut error.span.end] {
        if position.line == 1 {
            position.col += col_offset;
        }
        position.line += line_offset;
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elements(text: &str) -> Option<Vec<&str>> {
        let split = split_scene_text(text)?;
        assert_eq!(split.header.len(), text.len(), "header keeps every byte offset");
        Some(split.entities.iter().map(|range| &text[range.clone()]).collect())
    }

    #[test]
    fn splits_plain_and_named_documents() {
        assert_eq!(
            elements("SceneFile(format_version: 5, entities: [(a: 1), (b: [2, 3])], resources: {})"),
            Some(vec!["(a: 1)", "(b: [2, 3])"])
        );
        assert_eq!(
            elements("(entities: [ SceneEntity(x: (1, 2)) , (y: {\"k\": 3}), ], format_version: 1)"),
            Some(vec!["SceneEntity(x: (1, 2))", "(y: {\"k\": 3})"])
        );
        assert_eq!(elements("(format_version: 5, entities: [])"), Some(vec![]));
        assert_eq!(
            elements("(format_version: 5, entities: [ /* none */ ])"),
            Some(vec![])
        );
    }

    #[test]
    fn brackets_inside_strings_chars_and_comments_are_ignored() {
        let text = r##"(
    // entities: [ in a comment
    format_version: 5,
    entities: [
        /* leading ] */ (s: "]\"[", c: ']', q: '\'', u: '\u{5D}', r: r#"]"#, b: b"]", br: br"[") // trailing ]
        ,
        (/* nested /* ] */ */ e: entities, raw: r#entities),
    ],
)"##;
        let parts = elements(text).expect("split");
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with("(s:") && parts[0].ends_with("br\"[\")"));
        assert!(parts[1].starts_with("(/* nested"));
    }

    #[test]
    fn header_blanks_the_list_body_and_keeps_lines() {
        let text = "(format_version: 5, entities: [\n    (a: 1),\n], resources: {})";
        let split = split_scene_text(text).expect("split");
        assert_eq!(
            split.header,
            "(format_version: 5, entities: [\n           \n], resources: {})"
        );
    }

    #[test]
    fn anything_unusual_falls_back() {
        for text in [
            "#![enable(implicit_some)]\n(format_version: 5, entities: [])",
            "(format_version: 5)",
            "(format_version: 5, entities: [(a: \"unterminated)])",
            "(format_version: 5, entities: [(a: 1]])",
            "(format_version: 5, entities: [(a: 1)], entities: [])",
            "(format_version: 5, entities: None)",
            "(format_version: 5, entities: [(a: 1),, (b: 2)])",
            "(format_version: 5, entities: [(a: 1)]",
            "[entities: []]",
            "(format_version: 5, entities: [(a: /* open)])",
        ] {
            assert!(split_scene_text(text).is_none(), "{text}");
        }
    }

    #[test]
    fn relocated_errors_point_into_the_whole_text() {
        let text = "(format_version: 5, entities: [\n  (a: 1),\n  (b: ?)\n])";
        let split = split_scene_text(text).expect("split");
        let range = split.entities[1].clone();
        let error = ron::from_str::<ron::Value>(&text[range.clone()]).expect_err("bad value");
        let relocated = relocate_error(error, text, range.start);
        let whole = ron::from_str::<ron::Value>(text).expect_err("bad document");
        assert_eq!(relocated.span.end.line, 3);
        assert_eq!(relocated.span.end, whole.span.end);
        assert_eq!(relocated.code, whole.code);
    }
}
