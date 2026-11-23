// SPDX-License-Identifier: MPL-2.0
//! Comma-separated values as RFC 4180 defines them (`docs/specs/content-data.md` § 3.3).
//!
//! The reader follows the RFC's grammar: records end at a line break, fields are separated by
//! commas, a field may be enclosed in double quotes, and inside a quoted field a pair of double
//! quotes stands for one quote while commas and line breaks are plain text. Spaces are part of a
//! field. On top of the grammar the reader accepts what spreadsheets write in practice: LF or a lone
//! CR as well as CRLF, a missing line break after the last record, a UTF-8 byte-order mark at the
//! start, and empty lines (skipped, they hold no record). Everything else the RFC forbids is an
//! error with its line: a quote inside an unquoted field, text after a closing quote, a quoted field
//! that never closes, and (with [`CsvOptions::equal_fields`]) a record whose field count differs
//! from the first record's.
//!
//! The writer quotes a field only when it must (a comma, a quote, CR or LF) or when it has leading
//! or trailing spaces that a spreadsheet might trim, doubles every quote inside it, and ends every
//! record with CRLF, as the RFC's records do.

use thiserror::Error;

/// Reader options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsvOptions {
    /// Every record must have as many fields as the first one (RFC 4180 § 2, rule 4).
    pub equal_fields: bool,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self { equal_fields: true }
    }
}

/// One record: its fields and the line it starts on (1-based).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvRecord {
    /// Field texts with quoting removed.
    pub fields: Vec<String>,
    /// Line of the record's first character (1-based; a quoted line break makes a record span
    /// several lines).
    pub line: usize,
}

/// What went wrong while reading.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum CsvErrorKind {
    /// A double quote inside a field that does not start with one.
    #[error("a double quote inside an unquoted field (enclose the field in quotes and double the quote)")]
    QuoteInUnquotedField,
    /// Text after the closing quote of a quoted field, before the next comma or line break.
    #[error("text after the closing quote of a field")]
    TextAfterClosingQuote,
    /// A quoted field that is still open at the end of the input.
    #[error("a quoted field is not closed")]
    UnterminatedQuote,
    /// A record with a different number of fields from the first record.
    #[error("record has {found} field(s), expected {expected}")]
    FieldCount {
        /// Fields in the first record.
        expected: usize,
        /// Fields in this record.
        found: usize,
    },
}

/// A read error with the line it was found on.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("line {line}: {kind}")]
pub struct CsvError {
    /// Line (1-based).
    pub line: usize,
    /// The problem.
    pub kind: CsvErrorKind,
}

/// Reads `text` with the default options (equal field counts).
///
/// # Errors
///
/// The first [`CsvError`] in the text.
pub fn parse(text: &str) -> Result<Vec<CsvRecord>, CsvError> {
    parse_with(text, CsvOptions::default())
}

/// Reads `text` into records.
///
/// # Errors
///
/// The first [`CsvError`] in the text.
pub fn parse_with(text: &str, options: CsvOptions) -> Result<Vec<CsvRecord>, CsvError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records: Vec<CsvRecord> = Vec::new();
    let mut chars = text.chars().peekable();
    let mut line = 1usize;

    // State of the record being read.
    let mut fields: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut record_line = 1usize;
    // The record has any content: a field separator, a character, or a quoted (maybe empty) field.
    let mut started = false;

    loop {
        let Some(c) = chars.next() else {
            if started {
                fields.push(std::mem::take(&mut field));
                push_record(&mut records, &mut fields, record_line, options)?;
            }
            return Ok(records);
        };
        if !started {
            record_line = line;
        }
        match c {
            '"' if field.is_empty() => {
                started = true;
                let quote_line = line;
                // Quoted field: read to the closing quote.
                loop {
                    match chars.next() {
                        None => {
                            return Err(CsvError {
                                line: quote_line,
                                kind: CsvErrorKind::UnterminatedQuote,
                            });
                        }
                        Some('"') => {
                            if chars.peek() == Some(&'"') {
                                chars.next();
                                field.push('"');
                            } else {
                                break;
                            }
                        }
                        Some(other) => {
                            if other == '\n' || (other == '\r' && chars.peek() != Some(&'\n')) {
                                line += 1;
                            }
                            field.push(other);
                        }
                    }
                }
                // After the closing quote: a separator, a line break or the end.
                match chars.peek() {
                    None | Some(',' | '\r' | '\n') => {}
                    Some(_) => {
                        return Err(CsvError {
                            line,
                            kind: CsvErrorKind::TextAfterClosingQuote,
                        });
                    }
                }
                // Mark the field as quoted-and-done so a following character is caught above.
                // (`field` may be empty: `""` is an empty field.)
                if let Some(',') = chars.peek() {
                    chars.next();
                    fields.push(std::mem::take(&mut field));
                    // A separator right before the end still opens one more (empty) field.
                    if chars.peek().is_none() {
                        fields.push(String::new());
                        push_record(&mut records, &mut fields, record_line, options)?;
                        started = false;
                    }
                } else {
                    fields.push(std::mem::take(&mut field));
                    match chars.next() {
                        Some('\r') => {
                            if chars.peek() == Some(&'\n') {
                                chars.next();
                            }
                            line += 1;
                        }
                        Some('\n') => line += 1,
                        _ => {}
                    }
                    push_record(&mut records, &mut fields, record_line, options)?;
                    started = false;
                }
            }
            '"' => {
                return Err(CsvError {
                    line,
                    kind: CsvErrorKind::QuoteInUnquotedField,
                });
            }
            ',' => {
                started = true;
                fields.push(std::mem::take(&mut field));
                if chars.peek().is_none() {
                    fields.push(String::new());
                    push_record(&mut records, &mut fields, record_line, options)?;
                    started = false;
                }
            }
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                line += 1;
                if started {
                    fields.push(std::mem::take(&mut field));
                    push_record(&mut records, &mut fields, record_line, options)?;
                    started = false;
                }
                // An empty line holds no record.
            }
            other => {
                started = true;
                field.push(other);
            }
        }
    }
}

fn push_record(
    records: &mut Vec<CsvRecord>,
    fields: &mut Vec<String>,
    line: usize,
    options: CsvOptions,
) -> Result<(), CsvError> {
    let fields = std::mem::take(fields);
    if options.equal_fields
        && let Some(first) = records.first()
        && first.fields.len() != fields.len()
    {
        return Err(CsvError {
            line,
            kind: CsvErrorKind::FieldCount {
                expected: first.fields.len(),
                found: fields.len(),
            },
        });
    }
    records.push(CsvRecord { fields, line });
    Ok(())
}

/// Whether the writer encloses `field` in quotes.
#[must_use]
pub fn needs_quotes(field: &str) -> bool {
    field.contains([',', '"', '\r', '\n']) || field.starts_with([' ', '\t']) || field.ends_with([' ', '\t'])
}

/// Appends one record (fields joined by commas, quoted where needed, ended by CRLF) to `out`.
///
/// A record of exactly one empty field is written as `""`: an empty line holds no record when the
/// text is read back, so the bare form would vanish.
pub fn write_record<S: AsRef<str>>(out: &mut String, fields: &[S]) {
    if let [only] = fields
        && only.as_ref().is_empty()
    {
        out.push_str("\"\"\r\n");
        return;
    }
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let field = field.as_ref();
        if needs_quotes(field) {
            out.push('"');
            for c in field.chars() {
                if c == '"' {
                    out.push('"');
                }
                out.push(c);
            }
            out.push('"');
        } else {
            out.push_str(field);
        }
    }
    out.push_str("\r\n");
}

/// Writes every record (see [`write_record`]).
#[must_use]
pub fn to_string<S: AsRef<str>>(records: &[Vec<S>]) -> String {
    let mut out = String::new();
    for record in records {
        write_record(&mut out, record);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(text: &str) -> Vec<Vec<String>> {
        parse(text)
            .expect("parse")
            .into_iter()
            .map(|r| r.fields)
            .collect()
    }

    #[test]
    fn simple_records_with_and_without_final_line_break() {
        let want = vec![vec!["aaa", "bbb", "ccc"], vec!["zzz", "yyy", "xxx"]];
        assert_eq!(fields("aaa,bbb,ccc\r\nzzz,yyy,xxx\r\n"), want);
        assert_eq!(fields("aaa,bbb,ccc\r\nzzz,yyy,xxx"), want);
        assert_eq!(fields("aaa,bbb,ccc\nzzz,yyy,xxx\n"), want);
        assert_eq!(fields("aaa,bbb,ccc\rzzz,yyy,xxx"), want);
    }

    #[test]
    fn quoted_fields_hold_commas_line_breaks_and_doubled_quotes() {
        assert_eq!(
            fields("\"aaa\",\"b\r\nbb\",\"ccc\"\r\nzzz,yyy,xxx\r\n"),
            vec![vec!["aaa", "b\r\nbb", "ccc"], vec!["zzz", "yyy", "xxx"]]
        );
        assert_eq!(
            fields("\"aaa\",\"b\"\"bb\",\"c,cc\""),
            vec![vec!["aaa", "b\"bb", "c,cc"]]
        );
    }

    #[test]
    fn spaces_are_part_of_a_field_and_empty_fields_count() {
        assert_eq!(fields(" a , b ,"), vec![vec![" a ", " b ", ""]]);
        assert_eq!(fields("a,,c\n,,\n"), vec![vec!["a", "", "c"], vec!["", "", ""]]);
        assert_eq!(fields("\"\",x"), vec![vec!["", "x"]]);
        assert_eq!(fields("x,\"\""), vec![vec!["x", ""]]);
        assert_eq!(fields("x,\"\",\n"), vec![vec!["x", "", ""]]);
    }

    #[test]
    fn record_lines_count_quoted_line_breaks() {
        let records = parse("k,v\r\n\"a\",\"line1\r\nline2\"\r\nb,c\r\n").expect("parse");
        assert_eq!(records.iter().map(|r| r.line).collect::<Vec<_>>(), vec![1, 2, 4]);
    }

    #[test]
    fn bom_and_empty_lines_are_skipped() {
        assert_eq!(
            fields("\u{feff}a,b\n\n\nc,d\n\n"),
            vec![vec!["a", "b"], vec!["c", "d"]]
        );
        assert!(parse("").expect("empty").is_empty());
        assert!(parse("\r\n\r\n").expect("blank").is_empty());
    }

    #[test]
    fn errors_name_their_line() {
        assert_eq!(
            parse("a,b\nc,d\"e\n").expect_err("error"),
            CsvError {
                line: 2,
                kind: CsvErrorKind::QuoteInUnquotedField
            }
        );
        assert_eq!(
            parse("a,b\n\"c\"x,d\n").expect_err("error"),
            CsvError {
                line: 2,
                kind: CsvErrorKind::TextAfterClosingQuote
            }
        );
        assert_eq!(
            parse("a,b\n\"c,d\n").expect_err("error"),
            CsvError {
                line: 2,
                kind: CsvErrorKind::UnterminatedQuote
            }
        );
        assert_eq!(
            parse("a,b\nc\n").expect_err("error"),
            CsvError {
                line: 2,
                kind: CsvErrorKind::FieldCount {
                    expected: 2,
                    found: 1
                }
            }
        );
        let ragged = parse_with("a,b\nc\n", CsvOptions { equal_fields: false }).expect("ragged");
        assert_eq!(ragged[1].fields, vec!["c"]);
    }

    #[test]
    fn writer_quotes_only_when_needed_and_round_trips() {
        let records = vec![
            vec!["key", "text"],
            vec!["plain", "hello"],
            vec!["comma", "a,b"],
            vec!["quote", "say \"hi\""],
            vec!["break", "one\r\ntwo\nthree"],
            vec!["space", " padded "],
            vec!["empty", ""],
            vec!["unicode", "ゲーム, ünïcode"],
        ];
        let text = to_string(&records);
        assert!(text.starts_with("key,text\r\nplain,hello\r\ncomma,\"a,b\"\r\nquote,\"say \"\"hi\"\"\"\r\n"));
        assert!(text.ends_with("\r\n"));
        let back = fields(&text);
        let want: Vec<Vec<String>> = records
            .iter()
            .map(|r| r.iter().map(|s| (*s).to_owned()).collect())
            .collect();
        assert_eq!(back, want);
    }
}
