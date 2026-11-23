// SPDX-License-Identifier: MPL-2.0
//! CSV exchange of a sequence's marker list (ADR-4597).
//!
//! The sheet is an exchange file, not a `.seq.ron` version. Columns, in order, are
//! `time_seconds`, `name` and `payload`. A header row is required. Applying a sheet replaces
//! [`Sequence::markers`] and nothing else; a bad row returns [`MarkerCsvError`] (protocol code
//! `seq_marker_csv`) with the row number and leaves the sequence unchanged.
//!
//! Times are decimal seconds (a dot, not a comma). A field that contains a comma, a quote, a
//! line break or surrounding whitespace is quoted, and a quote inside it is doubled (RFC 4180).
//! An unquoted field is trimmed; a quoted field is kept as written, so a sheet written by
//! [`markers_to_csv`] reads back as the same markers.

use std::collections::BTreeSet;
use std::fmt;
use std::iter::Peekable;
use std::str::Chars;

use super::{Marker, Sequence};

/// The header, in order.
const COLUMNS: [&str; 3] = ["time_seconds", "name", "payload"];

/// Why a marker CSV was refused. The sequence is left unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerCsvError {
    /// 1-based row of the bad record. The header is row 1. A quoted field that spans lines
    /// counts each physical line, and the row is the line where the record starts.
    pub row: u32,
    /// What is wrong on that row, without the row number.
    pub message: String,
}

impl MarkerCsvError {
    /// The agent-protocol code (`seq_marker_csv`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        "seq_marker_csv"
    }
}

impl fmt::Display for MarkerCsvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "row {}: {}", self.row, self.message)
    }
}

impl std::error::Error for MarkerCsvError {}

fn fail(row: u32, message: impl Into<String>) -> MarkerCsvError {
    MarkerCsvError {
        row,
        message: message.into(),
    }
}

/// One CSV field. `quoted` is how it was written, so a quoted payload keeps its spaces.
struct Field {
    text: String,
    quoted: bool,
}

/// One CSV record and the physical line it starts on.
struct Record {
    line: u32,
    fields: Vec<Field>,
}

fn bump(line: &mut u32) {
    *line = line.saturating_add(1);
}

/// Writes `markers` as CSV. The first line is the header `time_seconds,name,payload`.
/// Marker order is the slice order.
#[must_use]
pub fn markers_to_csv(markers: &[Marker]) -> String {
    let mut out = String::from("time_seconds,name,payload\n");
    for marker in markers {
        out.push_str(&marker.time.to_string());
        out.push(',');
        push_field(&mut out, &marker.name);
        out.push(',');
        push_field(&mut out, &marker.payload);
        out.push('\n');
    }
    out
}

fn push_field(out: &mut String, value: &str) {
    let quote = value.chars().any(|c| matches!(c, ',' | '"' | '\n' | '\r'))
        || value.starts_with(char::is_whitespace)
        || value.ends_with(char::is_whitespace);
    if !quote {
        out.push_str(value);
        return;
    }
    out.push('"');
    for c in value.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
}

/// Parses a marker CSV into markers, in file order.
///
/// A bad row is an error and no markers are returned. Times are finite and zero or more.
/// Names are non-empty and unique. The payload may be empty.
pub fn csv_to_markers(csv: &str) -> Result<Vec<Marker>, MarkerCsvError> {
    let records = parse_records(csv)?;
    if records.is_empty() {
        return Err(fail(1, "a header row is required"));
    }
    check_header(&records[0])?;
    let mut markers = Vec::with_capacity(records.len().saturating_sub(1));
    let mut names = BTreeSet::new();
    for record in records.iter().skip(1) {
        markers.push(marker_from_record(record, &mut names)?);
    }
    Ok(markers)
}

/// Replaces `sequence.markers` with the markers in `csv`.
///
/// On a bad row the sequence is not changed: tracks, channels and the previous markers stay.
pub fn apply_marker_csv(sequence: &mut Sequence, csv: &str) -> Result<(), MarkerCsvError> {
    let markers = csv_to_markers(csv)?;
    sequence.markers = markers;
    Ok(())
}

fn field_text(field: &Field) -> String {
    if field.quoted {
        field.text.clone()
    } else {
        field.text.trim().to_string()
    }
}

fn check_header(record: &Record) -> Result<(), MarkerCsvError> {
    if record.fields.len() > COLUMNS.len() {
        return Err(unknown_column(record.line, &record.fields[COLUMNS.len()]));
    }
    for (index, expected) in COLUMNS.iter().enumerate() {
        let got = record
            .fields
            .get(index)
            .map(|field| field.text.trim())
            .unwrap_or("");
        if got != *expected {
            if got.is_empty() {
                return Err(fail(record.line, format!("missing column `{expected}`")));
            }
            return Err(fail(record.line, format!("unknown column `{got}`")));
        }
    }
    Ok(())
}

fn unknown_column(row: u32, field: &Field) -> MarkerCsvError {
    let name = field.text.trim();
    if name.is_empty() {
        fail(row, "unknown column")
    } else {
        fail(row, format!("unknown column `{name}`"))
    }
}

fn marker_from_record(record: &Record, names: &mut BTreeSet<String>) -> Result<Marker, MarkerCsvError> {
    if record.fields.len() > COLUMNS.len() {
        return Err(unknown_column(record.line, &record.fields[COLUMNS.len()]));
    }
    if record.fields.len() != COLUMNS.len() {
        return Err(fail(
            record.line,
            format!("expected 3 columns, found {}", record.fields.len()),
        ));
    }
    let time_text = record.fields[0].text.trim();
    let time: f32 = time_text.parse().map_err(|_| {
        fail(
            record.line,
            format!("time `{time_text}` must be a decimal number of seconds"),
        )
    })?;
    if !time.is_finite() {
        return Err(fail(record.line, format!("time `{time_text}` must be finite")));
    }
    if time < 0.0 {
        return Err(fail(
            record.line,
            format!("time `{time_text}` must be zero or more"),
        ));
    }
    let name = field_text(&record.fields[1]);
    if name.is_empty() {
        return Err(fail(record.line, "marker name is empty"));
    }
    if !names.insert(name.clone()) {
        return Err(fail(record.line, format!("duplicate marker name `{name}`")));
    }
    let payload = field_text(&record.fields[2]);
    Ok(Marker { name, time, payload })
}

fn parse_records(csv: &str) -> Result<Vec<Record>, MarkerCsvError> {
    let mut chars = csv.chars().peekable();
    if chars.peek() == Some(&'\u{feff}') {
        chars.next();
    }
    let mut records = Vec::new();
    let mut line = 1u32;
    while chars.peek().is_some() {
        let start = line;
        let mut fields = Vec::new();
        loop {
            let (field, ended) = read_field(&mut chars, &mut line, start)?;
            fields.push(field);
            if ended {
                break;
            }
        }
        // A blank line (a spreadsheet export's trailing newline, a hand-edited gap) is no record.
        let blank = matches!(fields.as_slice(), [only] if !only.quoted && only.text.trim().is_empty());
        if !blank {
            records.push(Record { line: start, fields });
        }
    }
    Ok(records)
}

/// Reads one field. The bool is `true` when this field ends the record.
fn read_field(
    chars: &mut Peekable<Chars<'_>>,
    line: &mut u32,
    row: u32,
) -> Result<(Field, bool), MarkerCsvError> {
    if chars.peek() == Some(&'"') {
        chars.next();
        return read_quoted(chars, line, row);
    }
    let mut text = String::new();
    loop {
        match chars.peek().copied() {
            Some(',' | '\n' | '\r') | None => {
                let ended = end_of_record(chars, line, row)?;
                return Ok((Field { text, quoted: false }, ended));
            }
            Some(c) => {
                chars.next();
                text.push(c);
            }
        }
    }
}

fn read_quoted(
    chars: &mut Peekable<Chars<'_>>,
    line: &mut u32,
    row: u32,
) -> Result<(Field, bool), MarkerCsvError> {
    let mut text = String::new();
    loop {
        match chars.next() {
            Some('"') if chars.peek() == Some(&'"') => {
                chars.next();
                text.push('"');
            }
            Some('"') => break,
            Some('\n') => {
                text.push('\n');
                bump(line);
            }
            Some('\r') => {
                text.push('\r');
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    text.push('\n');
                }
                bump(line);
            }
            Some(c) => text.push(c),
            None => return Err(fail(row, "a quoted field is not closed")),
        }
    }
    while matches!(chars.peek(), Some(' ' | '\t')) {
        chars.next();
    }
    let ended = end_of_record(chars, line, row)?;
    Ok((Field { text, quoted: true }, ended))
}

/// Consumes the delimiter after a field. `true` when the record ends.
fn end_of_record(chars: &mut Peekable<Chars<'_>>, line: &mut u32, row: u32) -> Result<bool, MarkerCsvError> {
    match chars.peek().copied() {
        Some(',') => {
            chars.next();
            Ok(false)
        }
        Some('\r') => {
            chars.next();
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            bump(line);
            Ok(true)
        }
        Some('\n') => {
            chars.next();
            bump(line);
            Ok(true)
        }
        None => Ok(true),
        Some(c) => Err(fail(row, format!("unexpected {c:?} after a quoted field"))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{AudioSection, BoolKey, Channel, Ease, ScalarKey, Track, Vec3Key};
    use super::*;

    fn marker(name: &str, time: f32, payload: &str) -> Marker {
        Marker {
            name: name.to_string(),
            time,
            payload: payload.to_string(),
        }
    }

    fn sample() -> Sequence {
        Sequence {
            name: "intro".into(),
            tracks: vec![
                Track {
                    name: "fade".into(),
                    target: "ui".into(),
                    property: "opacity".into(),
                    keys: Channel::Float(vec![
                        ScalarKey::new(0.0, 0.0, Ease::Linear),
                        ScalarKey::new(2.0, 1.0, Ease::Linear),
                    ]),
                    enabled: true,
                    locked: false,
                },
                Track {
                    name: "move".into(),
                    target: "hero".into(),
                    property: "transform.position".into(),
                    keys: Channel::Vec3(vec![
                        Vec3Key::new(0.0, [0.0, 0.0, 0.0], Ease::Linear),
                        Vec3Key::new(2.0, [1.0, 0.0, 0.0], Ease::Linear),
                    ]),
                    enabled: true,
                    locked: false,
                },
                Track {
                    name: "show".into(),
                    target: "ui".into(),
                    property: "visible".into(),
                    keys: Channel::Bool(vec![BoolKey {
                        time: 0.0,
                        value: true,
                    }]),
                    enabled: true,
                    locked: false,
                },
                Track {
                    name: "bell".into(),
                    target: String::new(),
                    property: String::new(),
                    keys: Channel::Audio(vec![AudioSection::new("bell", 0.5, 1.5)]),
                    enabled: true,
                    locked: false,
                },
            ],
            markers: vec![
                marker("late", 2.0, "z"),
                marker("early", 0.5, ""),
                marker("mid", 1.0, "a,b"),
            ],
            ..Sequence::default()
        }
    }

    #[test]
    fn three_markers_round_trip_in_order() {
        let csv = "time_seconds,name,payload\n2,late,z\n0.5,early,\n1,mid,\"a,b\"\n";
        let markers = csv_to_markers(csv).expect("parses");
        assert_eq!(
            markers,
            vec![
                marker("late", 2.0, "z"),
                marker("early", 0.5, ""),
                marker("mid", 1.0, "a,b")
            ]
        );
        let again = csv_to_markers(&markers_to_csv(&markers)).expect("round trip");
        assert_eq!(again, markers);
    }

    #[test]
    fn blank_lines_are_skipped_and_errors_keep_their_line() {
        let csv = "time_seconds,name,payload\r\n0,a,\r\n\r\n1,b,\n\n";
        let markers = csv_to_markers(csv).expect("a trailing or inner blank line is no row");
        assert_eq!(markers, vec![marker("a", 0.0, ""), marker("b", 1.0, "")]);
        let error = csv_to_markers("time_seconds,name,payload\n\nx,c,\n").expect_err("bad time");
        assert!(error.to_string().contains('3'), "{error}");
        // A quoted empty field is data, not a blank line.
        assert!(csv_to_markers("time_seconds,name,payload\n\"\"\n").is_err());
    }

    #[test]
    fn quoted_payload_and_awkward_times_keep_their_bits() {
        let markers = vec![
            marker("say \"hi\"", 0.1, "a\nb"),
            marker(" spaced ", -0.0, "  pad  "),
            marker("\u{2003}cue\u{2003}", 3.0, "\u{000b}payload\u{000c}"),
        ];
        let back = csv_to_markers(&markers_to_csv(&markers)).expect("round trip");
        assert_eq!(back, markers);
        assert_eq!(back[0].time.to_bits(), 0.1f32.to_bits());
        assert_eq!(back[1].time.to_bits(), (-0.0f32).to_bits());
        for time in [0.0f32, 1.0, 1.5, 10.0, f32::MIN_POSITIVE, 1.0e20, 0.3] {
            let one = vec![marker("t", time, "")];
            let back = csv_to_markers(&markers_to_csv(&one)).expect("time");
            assert_eq!(back[0].time.to_bits(), time.to_bits(), "{time}");
        }
    }

    #[test]
    fn a_duplicate_name_is_refused_and_writes_nothing() {
        let mut seq = sample();
        let before = seq.clone();
        let err = apply_marker_csv(&mut seq, "time_seconds,name,payload\n0,early,\n1,early,again\n")
            .expect_err("duplicate");
        assert_eq!(err.row, 3);
        assert_eq!(err.code(), "seq_marker_csv");
        assert!(err.to_string().contains("row 3"), "{err}");
        assert!(err.message.contains("duplicate"), "{err}");
        assert_eq!(seq, before);
    }

    #[test]
    fn a_negative_time_is_refused_and_writes_nothing() {
        let mut seq = sample();
        let before = seq.clone();
        let err =
            apply_marker_csv(&mut seq, "time_seconds,name,payload\n-0.25,late,\n").expect_err("negative");
        assert_eq!(err.row, 2);
        assert_eq!(err.code(), "seq_marker_csv");
        assert!(err.message.contains("zero or more"), "{err}");
        assert_eq!(seq, before);
    }

    #[test]
    fn non_finite_empty_name_and_a_missing_header_are_refused() {
        let nan = csv_to_markers("time_seconds,name,payload\nnan,a,\n").expect_err("nan");
        assert_eq!(nan.row, 2);
        assert!(nan.message.contains("finite"), "{nan}");
        let inf = csv_to_markers("time_seconds,name,payload\ninf,a,\n").expect_err("inf");
        assert_eq!(inf.row, 2);
        let empty = csv_to_markers("time_seconds,name,payload\n1,,\n").expect_err("name");
        assert_eq!(empty.row, 2);
        assert!(empty.message.contains("empty"), "{empty}");
        let missing = csv_to_markers("").expect_err("empty file");
        assert_eq!(missing.row, 1);
        assert_eq!(missing.code(), "seq_marker_csv");
    }

    #[test]
    fn an_unknown_column_is_refused() {
        let wrong = csv_to_markers("time_seconds,name,note\n0,a,\n").expect_err("note");
        assert_eq!(wrong.row, 1);
        assert!(wrong.message.contains("unknown column `note`"), "{wrong}");
        let extra = csv_to_markers("time_seconds,name,payload,extra\n").expect_err("extra");
        assert_eq!(extra.row, 1);
        assert!(extra.message.contains("unknown column `extra`"), "{extra}");
        let swapped = csv_to_markers("name,time_seconds,payload\n").expect_err("order");
        assert!(swapped.message.contains("unknown column `name`"), "{swapped}");
    }

    #[test]
    fn a_newline_inside_a_payload_keeps_the_next_row_number() {
        let csv = "time_seconds,name,payload\r\n0,a,\"line\r\nline\"\r\n-1,b,\r\n";
        let err = csv_to_markers(csv).expect_err("negative after a wrapped payload");
        assert_eq!(err.row, 4, "{err}");
    }

    #[test]
    fn saving_without_a_marker_edit_keeps_the_bytes() {
        let seq = sample();
        seq.validate().expect("sample is valid");
        let text = seq.to_ron().expect("saves");
        let hash = blake3::hash(text.as_bytes());
        let mut loaded = Sequence::from_ron(&text).expect("loads");
        assert_eq!(loaded.markers, seq.markers, "the load did not change markers");
        let resaved = loaded.to_ron().expect("saves again");
        assert_eq!(blake3::hash(resaved.as_bytes()), hash, "{resaved}");
        let csv = markers_to_csv(&loaded.markers);
        apply_marker_csv(&mut loaded, &csv).expect("the same sheet");
        let after = loaded.to_ron().expect("saves after the sheet");
        assert_eq!(
            blake3::hash(after.as_bytes()),
            hash,
            "applying the exported sheet changed the .seq.ron bytes\n{after}"
        );
    }

    #[test]
    fn an_import_replaces_markers_and_no_channel() {
        let mut seq = sample();
        let tracks = seq.tracks.clone();
        let csv = "time_seconds,name,payload\n0,only,cue\n4,later,\n";
        apply_marker_csv(&mut seq, csv).expect("imports");
        assert_eq!(seq.tracks, tracks);
        assert_eq!(
            seq.markers.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["only", "later"]
        );
        let bad = "time_seconds,name,payload\n-1,nope,\n";
        let markers = seq.markers.clone();
        apply_marker_csv(&mut seq, bad).expect_err("refused");
        assert_eq!(seq.tracks, tracks);
        assert_eq!(seq.markers, markers);
    }
}
