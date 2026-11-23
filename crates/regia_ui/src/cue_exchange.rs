// SPDX-License-Identifier: MPL-2.0
//! WebVTT and TTML import/export for subtitle cues (spec 56 § 11, ADR-3359).
//!
//! The cue model maps to both timed-text formats: one [`TimedCue`] is text plus start/end
//! seconds on the sound's clock. `regia loc cues export` resolves a `.subs.ron` track's
//! `text_key` cues through the project's `Localization` and writes WebVTT or TTML for a
//! subtitle vendor; `regia loc cues import` reads a returned file back into a `.subs.ron`
//! track of raw-text cues. Times are written to the millisecond; a round trip keeps times and
//! text within 1 ms and byte-for-byte text, with two documented mappings: a last cue whose
//! `until` is open (`None`) exports with `end == start` and imports back open, and an empty line
//! inside a cue's WebVTT text exports as one space (an empty line would end the cue block).
//! Character references and cue-text tags in a vendor's file resolve to plain text.
//!
//! Both readers are bounded ([`CueLimits`]): hostile files are refused with a coded
//! [`CueError`] instead of growing memory without limit, and both formats fuzz
//! (`subtitles.webvtt`, `subtitles.ttml`).
//!
//! The WebVTT file format is public (W3C WebVTT, <https://www.w3.org/TR/webvtt1/>); the TTML
//! subset is the `p begin/end` body of W3C TTML 2 (<https://www.w3.org/TR/ttml2/>). The parsers
//! and writers here are original code.

use std::fmt;

/// One cue on the wire: seconds from the sound start, text with `\n` line breaks.
#[derive(Debug, Clone, PartialEq)]
pub struct TimedCue {
    /// Start, seconds (`>= 0`).
    pub start: f64,
    /// End, seconds (`>= start`).
    pub end: f64,
    /// The cue text.
    pub text: String,
}

/// Hostile-input limits both readers enforce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CueLimits {
    /// Largest accepted input, in bytes.
    pub max_bytes: usize,
    /// Most cues in one file.
    pub max_cues: usize,
    /// Longest single cue's text, in bytes.
    pub max_cue_text: usize,
}

impl Default for CueLimits {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024,
            max_cues: 10_000,
            max_cue_text: 4096,
        }
    }
}

/// Why a file did not read. `code` is stable and documented (book page `agents/localization`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueError {
    /// Stable lowercase code (`webvtt_header`, `ttml_time`, ...).
    pub code: &'static str,
    /// The 1-based line (0 when the limit is about the whole file).
    pub line: u32,
    /// A sentence for people.
    pub message: String,
}

impl fmt::Display for CueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} (line {})", self.code, self.message, self.line)
    }
}

impl std::error::Error for CueError {}

fn err(code: &'static str, line: u32, message: impl Into<String>) -> CueError {
    CueError {
        code,
        line,
        message: message.into(),
    }
}

/// Seconds quantized to whole milliseconds, the wire precision (round half up).
#[cfg(test)]
fn quantize(seconds: f64) -> f64 {
    (seconds.max(0.0) * 1000.0).round() / 1000.0
}

/// `seconds` as `HH:MM:SS.mmm` (hours at least two digits).
fn format_time(seconds: f64) -> String {
    let ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    let frac = ms % 1000;
    format!("{h:02}:{m:02}:{s:02}.{frac:03}")
}

/// A WebVTT/TTML timestamp: `HH:MM:SS.mmm`, `H:MM:SS.mmm` or `MM:SS.mmm`.
fn parse_time(text: &str) -> Option<f64> {
    let (hms, frac) = text.split_once('.')?;
    if frac.is_empty() || frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parts: Vec<&str> = hms.split(':').collect();
    let (h, m, s) = match parts.as_slice() {
        [m, s] => ("0", *m, *s),
        [h, m, s] => {
            if h.is_empty() {
                return None;
            }
            (*h, *m, *s)
        }
        _ => return None,
    };
    if m.len() != 2 || s.len() != 2 || !m.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) || !h.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let h: u64 = h.parse().ok()?;
    let m: u64 = m.parse().ok()?;
    let s: u64 = s.parse().ok()?;
    if m >= 60 || s >= 60 {
        return None;
    }
    let scale = 10u64.pow(u32::try_from(3 - frac.len()).ok()?);
    let frac_ms: u64 = frac.parse::<u64>().ok()?.checked_mul(scale)?;
    let seconds = h
        .checked_mul(3600)?
        .checked_add(m.checked_mul(60)?)?
        .checked_add(s)?;
    Some(seconds as f64 + frac_ms as f64 / 1000.0)
}

/// A cue track with explicit ends: `until: None` runs to the next cue's start, or to the cue's
/// own start for an open last cue (the export mapping, documented on the module).
#[must_use]
pub fn track_to_timed(track: &regia_asset::dialogue::SubtitleTrack) -> Vec<TimedCue> {
    let mut out = Vec::with_capacity(track.cues.len());
    for (i, cue) in track.cues.iter().enumerate() {
        let end = cue
            .until
            .unwrap_or_else(|| track.cues.get(i + 1).map_or(cue.at, |next| next.at.max(cue.at)));
        out.push(TimedCue {
            start: f64::from(cue.at),
            end: f64::from(end),
            text: cue.text.clone().unwrap_or_default(),
        });
    }
    out
}

/// Timed cues back to a track: a last cue whose end equals its start imports open
/// (`until: None`); every other cue gets an explicit `until`. All imported cues are raw-text
/// cues: a timed-text file carries display text, not Fluent keys.
#[must_use]
pub fn timed_to_track(cues: &[TimedCue]) -> regia_asset::dialogue::SubtitleTrack {
    let n = cues.len();
    let cues = cues
        .iter()
        .enumerate()
        .map(|(i, c)| regia_asset::dialogue::Cue {
            at: c.start as f32,
            until: if i + 1 == n && c.end <= c.start {
                None
            } else {
                Some(c.end as f32)
            },
            text_key: None,
            text: Some(c.text.clone()),
        })
        .collect();
    regia_asset::dialogue::SubtitleTrack {
        format_version: regia_asset::dialogue::SUBTITLES_VERSION,
        priority: 1,
        keep_breaks: false,
        one_at_a_time: false,
        cues,
    }
}

// --- WebVTT --------------------------------------------------------------------------------------

/// Writes cues as a WebVTT document (header, one block per cue, blank line separated).
///
/// `&`, `<` and `>` in the text are written as character references, as the WebVTT cue text
/// syntax requires; a tab or carriage return is written as a numeric reference so it reads back
/// exactly. An empty line inside a cue's text would end the cue block, so it is written as one
/// space (it still shows as an empty line).
#[must_use]
pub fn to_webvtt(cues: &[TimedCue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues {
        out.push_str(&format!(
            "{} --> {}\n",
            format_time(cue.start),
            format_time(cue.end)
        ));
        for line in cue.text.split('\n') {
            if line.is_empty() {
                out.push(' ');
            } else {
                out.push_str(&xml_escape(line));
            }
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// One character reference at the start of `after` (which starts with `&`): the text it stands
/// for and its length, or `None` when it is not one this reader knows (kept literally).
/// Numeric references to control characters other than a line feed, a tab and a carriage
/// return are not decoded.
fn char_reference(after: &str) -> Option<(char, usize)> {
    const NAMED: [(&str, char); 8] = [
        ("&amp;", '&'),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&quot;", '"'),
        ("&apos;", '\''),
        ("&nbsp;", '\u{a0}'),
        ("&lrm;", '\u{200e}'),
        ("&rlm;", '\u{200f}'),
    ];
    if let Some((name, c)) = NAMED.iter().find(|(name, _)| after.starts_with(name)) {
        return Some((*c, name.len()));
    }
    let body = after.strip_prefix("&#")?;
    let end = body.find(';')?;
    let digits = &body[..end];
    let value = if let Some(hex) = digits.strip_prefix(['x', 'X']) {
        if hex.is_empty() || hex.len() > 6 {
            return None;
        }
        u32::from_str_radix(hex, 16).ok()?
    } else {
        if digits.is_empty() || digits.len() > 7 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()?
    };
    let c = char::from_u32(value)?;
    if c.is_control() && !matches!(c, '\n' | '\t' | '\r') {
        return None;
    }
    Some((c, 2 + end + 1))
}

/// WebVTT cue text to plain text: character references resolve, and the cue-text tags
/// (`c`, `i`, `b`, `u`, `v`, `lang`, `ruby`, `rt`, timestamps) are dropped. A `<` that does not
/// start one of those tags is text.
fn webvtt_text(line: &str) -> String {
    const TAGS: [&str; 8] = ["c", "i", "b", "u", "v", "lang", "ruby", "rt"];
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(['&', '<']) {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        if let Some(past_amp) = after.strip_prefix('&') {
            match char_reference(after) {
                Some((c, len)) => {
                    out.push(c);
                    rest = &after[len..];
                }
                None => {
                    out.push('&');
                    rest = past_amp;
                }
            }
            continue;
        }
        let tag = after.find('>').map(|end| (&after[1..end], end));
        let known = tag.is_some_and(|(inner, _)| {
            let name = inner.strip_prefix('/').unwrap_or(inner);
            let name_end = name.find(['.', ' ', '\t']).unwrap_or(name.len());
            TAGS.contains(&&name[..name_end]) || (!inner.starts_with('/') && parse_time(inner).is_some())
        });
        match tag {
            Some((_, end)) if known => rest = &after[end + 1..],
            _ => {
                out.push('<');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A WebVTT `NOTE`, `STYLE` or `REGION` block: the line is that word, or that word then a space
/// or tab. Cue identifiers that only start with the word (`NOTES`) are not metadata.
fn is_metadata_block(first: &str) -> bool {
    fn kind(first: &str, name: &str) -> bool {
        let Some(rest) = first.strip_prefix(name) else {
            return false;
        };
        rest.is_empty() || rest.starts_with([' ', '\t'])
    }
    kind(first, "NOTE") || kind(first, "STYLE") || kind(first, "REGION")
}

/// Reads a WebVTT document. `NOTE`, `STYLE` and `REGION` blocks are skipped; cue identifiers
/// are accepted and ignored; settings after the end timestamp are ignored.
///
/// # Errors
///
/// [`CueError`] with a stable code: `webvtt_header` (no `WEBVTT` signature), `webvtt_timestamp`,
/// `webvtt_cue_order` (an end before its start or a start before the previous cue's start),
/// `webvtt_limit` (a [`CueLimits`] bound) and `webvtt_text` (an empty cue).
pub fn read_webvtt(text: &str, limits: &CueLimits) -> Result<Vec<TimedCue>, CueError> {
    if text.len() > limits.max_bytes {
        return Err(err(
            "webvtt_limit",
            0,
            format!(
                "the file is {} bytes; the limit is {}",
                text.len(),
                limits.max_bytes
            ),
        ));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // WebVTT line terminators are CRLF, LF or a lone CR.
    let unified;
    let text = if text.contains('\r') {
        unified = text.replace("\r\n", "\n").replace('\r', "\n");
        unified.as_str()
    } else {
        text
    };
    let mut lines = text.split('\n').enumerate().peekable();
    let Some((_, header)) = lines.next() else {
        return Err(err("webvtt_header", 1, "the file is empty"));
    };
    let signature = header.trim_start().strip_prefix("WEBVTT");
    if !signature.is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return Err(err("webvtt_header", 1, "the first line must be `WEBVTT`"));
    }
    // Header lines (`Kind: captions`, `Language: en`) run to the first empty line; a line with
    // `-->` already starts a cue (the W3C parser does the same).
    while lines
        .next_if(|(_, line)| !line.is_empty() && !line.contains("-->"))
        .is_some()
    {}
    let mut cues = Vec::new();
    let mut prev_start = 0.0f64;
    // A block: consecutive non-blank lines. The first line with `-->` holds the times.
    let mut block: Vec<(usize, &str)> = Vec::new();
    let flush = |block: &mut Vec<(usize, &str)>,
                 cues: &mut Vec<TimedCue>,
                 prev_start: &mut f64,
                 limits: &CueLimits|
     -> Result<(), CueError> {
        if block.is_empty() {
            return Ok(());
        }
        let (line_no, first) = block[0];
        // A block kind is the whole token (`NOTE`, `STYLE`, `REGION`), optionally followed by a
        // space or tab. A cue id such as `NOTES` only shares the prefix and is a cue.
        if is_metadata_block(first) {
            block.clear();
            return Ok(());
        }
        let (times_at, times_line) = if first.contains("-->") {
            (line_no, first)
        } else if block.len() > 1 && block[1].1.contains("-->") {
            block[1]
        } else {
            return Err(err(
                "webvtt_timestamp",
                line_no as u32 + 1,
                format!("expected a cue timestamp, found `{first}`"),
            ));
        };
        let times_at = times_at as u32 + 1;
        let mut parts = times_line.split_whitespace();
        let (Some(start), Some(_arrow), Some(end)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(err(
                "webvtt_timestamp",
                times_at,
                format!("expected `start --> end`, found `{times_line}`"),
            ));
        };
        let Some(start) = parse_time(start) else {
            return Err(err("webvtt_timestamp", times_at, format!("bad start `{start}`")));
        };
        let Some(end) = parse_time(end) else {
            return Err(err("webvtt_timestamp", times_at, format!("bad end `{end}`")));
        };
        if end < start {
            return Err(err(
                "webvtt_cue_order",
                times_at,
                format!("the end {end:.3} is before the start {start:.3}"),
            ));
        }
        if start < *prev_start {
            return Err(err(
                "webvtt_cue_order",
                times_at,
                format!(
                    "cue starts at {start:.3}, before the previous cue at {:.3}",
                    *prev_start
                ),
            ));
        }
        let body_start = if first.contains("-->") { 1 } else { 2 };
        let cue_text = block[body_start.min(block.len())..]
            .iter()
            .map(|(_, l)| webvtt_text(l))
            .collect::<Vec<_>>()
            .join("\n");
        if cue_text.trim().is_empty() {
            return Err(err("webvtt_text", times_at, "the cue has no text"));
        }
        if cue_text.len() > limits.max_cue_text {
            return Err(err(
                "webvtt_limit",
                times_at,
                format!(
                    "the cue text is {} bytes; the limit is {}",
                    cue_text.len(),
                    limits.max_cue_text
                ),
            ));
        }
        if cues.len() >= limits.max_cues {
            return Err(err(
                "webvtt_limit",
                times_at,
                format!("more than {} cues", limits.max_cues),
            ));
        }
        *prev_start = start;
        cues.push(TimedCue {
            start,
            end,
            text: cue_text,
        });
        block.clear();
        Ok(())
    };
    for (index, line) in lines {
        // Only an empty line ends a block: a line of spaces is cue text. But a line with `-->`
        // starts a new cue once the block already has its timing line (the W3C "collect a
        // WebVTT block" step), so a separator line holding spaces or a tab cannot merge two
        // cues into one.
        let new_cue =
            !line.is_empty() && line.contains("-->") && block.iter().any(|(_, l)| l.contains("-->"));
        if new_cue {
            // The lines before a second `-->` line are the cue; a whitespace-only line right
            // before it is separator residue (a padded blank line), not caption text.
            while block.last().is_some_and(|(_, l)| l.trim().is_empty()) {
                block.pop();
            }
        }
        if line.is_empty() || new_cue {
            flush(&mut block, &mut cues, &mut prev_start, limits)?;
        }
        if !line.is_empty() {
            block.push((index, line));
        }
    }
    flush(&mut block, &mut cues, &mut prev_start, limits)?;
    Ok(cues)
}

// --- TTML ----------------------------------------------------------------------------------------

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // XML text may not hold a tab or carriage return that is meant as text rather
            // than white-space handling, and the readers treat a raw tab or CR as source
            // layout; write them as numeric references so they read back byte for byte.
            '\t' => out.push_str("&#9;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
    out
}

/// Like [`xml_escape`], and quotes, for a value written inside a double-quoted attribute.
fn xml_attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// Writes cues as a TTML 2 document: one `p` per cue in the minimal `body/div` shape, `\n` as
/// `<br/>`, `xml:lang` naming the cue language.
#[must_use]
pub fn to_ttml(cues: &[TimedCue], language: &str) -> String {
    let lang = if language.is_empty() { "en" } else { language };
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<tt xmlns=\"http://www.w3.org/ns/ttml\" xml:lang=\"",
    );
    out.push_str(&xml_attr(lang));
    out.push_str("\"><body><div>\n");
    for cue in cues {
        out.push_str(&format!(
            "<p begin=\"{}\" end=\"{}\">{}</p>\n",
            format_time(cue.start),
            format_time(cue.end),
            xml_escape(&cue.text).replace('\n', "<br/>")
        ));
    }
    out.push_str("</div></body></tt>\n");
    out
}

/// Reads the `p begin/end` body of a TTML document; everything else (head, styling, regions,
/// spans) is ignored. `<br/>` becomes `\n`; character references `&amp;` `&lt;` `&gt;` resolve.
///
/// # Errors
///
/// [`CueError`] with a stable code: `ttml_root` (no `tt` element), `ttml_time`,
/// `ttml_cue_order`, `ttml_limit` and `ttml_syntax`.
pub fn read_ttml(text: &str, limits: &CueLimits) -> Result<Vec<TimedCue>, CueError> {
    if text.len() > limits.max_bytes {
        return Err(err(
            "ttml_limit",
            0,
            format!(
                "the file is {} bytes; the limit is {}",
                text.len(),
                limits.max_bytes
            ),
        ));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(tt_start) = text.find("<tt") else {
        return Err(err("ttml_root", 1, "no `<tt>` element"));
    };
    let mut cues = Vec::new();
    let mut prev_start = 0.0f64;
    let mut rest = &text[tt_start..];
    // Cue offsets only grow, so line numbers count on from the last one (one pass over the file).
    let counted = std::cell::Cell::new((0usize, 1usize));
    let line_of = |at: usize| {
        let (from, line) = counted.get();
        let line = if at >= from {
            line + text[from..at].bytes().filter(|&b| b == b'\n').count()
        } else {
            1 + text[..at].bytes().filter(|&b| b == b'\n').count()
        };
        counted.set((at, line));
        u32::try_from(line).unwrap_or(u32::MAX)
    };
    let mut scan = 0usize;
    while let Some(rel) = rest[scan..].find("<p") {
        let p_at = scan + rel;
        // `<profile>`, `<param>` and any other name that merely starts with `p` is not a cue.
        let boundary = rest.as_bytes().get(p_at + 2).copied();
        if !matches!(boundary, Some(b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r')) {
            scan = p_at + 2;
            continue;
        }
        let abs_p = text.len() - rest.len() + p_at;
        let after_p = &rest[p_at + 2..];
        let Some(tag_end_rel) = after_p.find('>') else {
            return Err(err("ttml_syntax", line_of(abs_p), "a `<p` never closes"));
        };
        let tag = &after_p[..tag_end_rel];
        let self_closing = tag.ends_with('/');
        let attrs = &tag[..tag.len() - usize::from(self_closing)];
        let attr = |name: &str| -> Option<&str> {
            attrs.split_whitespace().find_map(|a| {
                let (k, v) = a.split_once('=')?;
                (k == name).then(|| v.trim_matches('"'))
            })
        };
        let line = line_of(abs_p);
        let Some(begin) = attr("begin") else {
            return Err(err("ttml_time", line, "a `<p>` has no `begin`"));
        };
        let Some(start) = parse_time(begin) else {
            return Err(err("ttml_time", line, format!("bad begin `{begin}`")));
        };
        let Some(end_attr) = attr("end") else {
            return Err(err("ttml_time", line, "a `<p>` has no `end`"));
        };
        let Some(end) = parse_time(end_attr) else {
            return Err(err("ttml_time", line, format!("bad end `{end_attr}`")));
        };
        if end < start {
            return Err(err(
                "ttml_cue_order",
                line,
                format!("the end {end:.3} is before the begin {start:.3}"),
            ));
        }
        if start < prev_start {
            return Err(err(
                "ttml_cue_order",
                line,
                format!("cue begins at {start:.3}, before the previous cue at {prev_start:.3}"),
            ));
        }
        let body = if self_closing {
            ""
        } else {
            let body_start = abs_p + 2 + tag_end_rel + 1;
            let Some(close) = text[body_start..].find("</p>") else {
                return Err(err("ttml_syntax", line, "a `<p>` never closes"));
            };
            &text[body_start..body_start + close]
        };
        let cue_text = ttml_text(body);
        if cue_text.trim().is_empty() {
            return Err(err("ttml_text", line, "the cue has no text"));
        }
        if cue_text.len() > limits.max_cue_text {
            return Err(err(
                "ttml_limit",
                line,
                format!(
                    "the cue text is {} bytes; the limit is {}",
                    cue_text.len(),
                    limits.max_cue_text
                ),
            ));
        }
        if cues.len() >= limits.max_cues {
            return Err(err(
                "ttml_limit",
                line,
                format!("more than {} cues", limits.max_cues),
            ));
        }
        prev_start = start;
        cues.push(TimedCue {
            start,
            end,
            text: cue_text,
        });
        let consumed = if self_closing {
            abs_p + 2 + tag_end_rel + 1
        } else {
            let body_start = abs_p + 2 + tag_end_rel + 1;
            body_start + text[body_start..].find("</p>").map_or(0, |c| c + 4)
        };
        rest = &text[consumed..];
        scan = 0;
    }
    if cues.is_empty() {
        return Err(err("ttml_root", 1, "no `<p>` cues in the document"));
    }
    Ok(cues)
}

/// Marks a run of source whitespace in [`ttml_text`] until the lines are finished.
const SOURCE_SPACE: char = '\0';

/// Source text of a `p` body: a whitespace run that holds a line break or a tab is source
/// layout (XML's default white-space handling), marked to collapse; spaces alone are text.
fn push_ttml_source(out: &mut String, text: &str) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if matches!(c, ' ' | '\t' | '\n' | '\r' | SOURCE_SPACE) {
            let mut run = String::from(c);
            while let Some(&next) = chars.peek() {
                if !matches!(next, ' ' | '\t' | '\n' | '\r' | SOURCE_SPACE) {
                    break;
                }
                run.push(next);
                chars.next();
            }
            if run.contains(['\t', '\n', '\r', SOURCE_SPACE]) {
                out.push(SOURCE_SPACE);
            } else {
                out.push_str(&run);
            }
        } else {
            out.push(c);
        }
    }
}

/// TTML `p` body text: `<br/>` (and `<br />`) become `\n`, character references resolve, and
/// tags (spans and their attributes) are dropped. Source layout whitespace (a run with a line
/// break or a tab) collapses to one space inside a line and to nothing at a line's ends.
fn ttml_text(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(lt) = rest.find(['&', '<']) {
        push_ttml_source(&mut out, &rest[..lt]);
        let after = &rest[lt..];
        if after.starts_with("<br") {
            out.push('\n');
            let end = after.find('>').map_or(after.len(), |i| i + 1);
            rest = &after[end..];
        } else if after.starts_with('<') {
            let end = after.find('>').map_or(after.len(), |i| i + 1);
            rest = &after[end..];
        } else if let Some((c, len)) = char_reference(after) {
            out.push(c);
            rest = &after[len..];
        } else {
            out.push('&');
            rest = &after[1..];
        }
    }
    push_ttml_source(&mut out, rest);
    out.split('\n')
        .map(|line| {
            let mut kept = String::with_capacity(line.len());
            for c in line.trim_matches(SOURCE_SPACE).chars() {
                if c != SOURCE_SPACE {
                    kept.push(c);
                } else if !kept.ends_with(SOURCE_SPACE) {
                    kept.push(SOURCE_SPACE);
                }
            }
            kept.replace(SOURCE_SPACE, " ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start: f64, end: f64, text: &str) -> TimedCue {
        TimedCue {
            start,
            end,
            text: text.to_owned(),
        }
    }

    #[test]
    fn timestamps_round_trip_to_the_millisecond() {
        for (s, want) in [
            (0.0, "00:00:00.000"),
            (1.5, "00:00:01.500"),
            (61.25, "00:01:01.250"),
            (3661.0, "01:01:01.000"),
        ] {
            assert_eq!(format_time(s), want);
            assert_eq!(parse_time(want), Some(s));
        }
        assert_eq!(parse_time("1:02:03.004"), Some(3723.004));
        // Malformed timestamps refuse.
        for bad in [
            "",
            "1:2:3",
            "01:61:00.000",
            "01:00:61.000",
            "01:00:00.",
            "01:00:00.0000",
            "aa:00:00.000",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn webvtt_round_trip_keeps_times_and_text() {
        let cues = vec![
            cue(0.5, 2.2, "Hello there"),
            cue(2.4, 4.0, "General & Kenobi\nsecond line"),
        ];
        let text = to_webvtt(&cues);
        assert!(text.starts_with("WEBVTT\n\n"));
        let back = read_webvtt(&text, &CueLimits::default()).expect("reads");
        assert_eq!(back, cues);
    }

    #[test]
    fn webvtt_reads_ids_notes_and_settings() {
        let text = "WEBVTT - title\n\nNOTE this is a note\nwith two lines\n\n1\n00:00:01.000 --> 00:00:02.000 align:center\nHi\n\nSTYLE\n::cue { color: lime }\n\n00:00:03.000 --> 00:00:04.000\nBye\n";
        let cues = read_webvtt(text, &CueLimits::default()).expect("reads");
        assert_eq!(cues, vec![cue(1.0, 2.0, "Hi"), cue(3.0, 4.0, "Bye")]);
    }

    #[test]
    fn webvtt_cue_ids_that_share_a_metadata_prefix_stay_cues() {
        // `NOTES` is a cue identifier. A prefix check drops the cue and keeps only `Bye`.
        let text = "\
WEBVTT

NOTES
00:00:01.000 --> 00:00:02.000
Hi

NOTE this is a note

STYLE
::cue { color: lime }

00:00:03.000 --> 00:00:04.000
Bye
";
        let cues = read_webvtt(text, &CueLimits::default()).expect("reads");
        assert_eq!(cues, vec![cue(1.0, 2.0, "Hi"), cue(3.0, 4.0, "Bye")]);
    }

    #[test]
    fn webvtt_refuses_hostile_or_broken_input() {
        let limits = CueLimits::default();
        assert_eq!(
            read_webvtt("not vtt", &limits).expect_err("header").code,
            "webvtt_header"
        );
        let bad_time =
            read_webvtt("WEBVTT\n\n00:00:02.000 --> 00:00:01.000\nx\n", &limits).expect_err("order");
        assert_eq!(bad_time.code, "webvtt_cue_order");
        assert_eq!(
            read_webvtt("WEBVTT\n\n00:00:01.0x0 --> 00:00:02.000\nx\n", &limits)
                .expect_err("timestamp")
                .code,
            "webvtt_timestamp"
        );
        // Out-of-order starts.
        assert_eq!(
            read_webvtt(
                "WEBVTT\n\n00:00:05.000 --> 00:00:06.000\na\n\n00:00:01.000 --> 00:00:02.000\nb\n",
                &limits
            )
            .expect_err("order")
            .code,
            "webvtt_cue_order"
        );
        // Limits: bytes, cues, text length.
        assert_eq!(
            read_webvtt(
                "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nx\n",
                &CueLimits {
                    max_bytes: 8,
                    ..limits
                }
            )
            .expect_err("bytes")
            .code,
            "webvtt_limit"
        );
        assert_eq!(
            read_webvtt(
                "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nx\n",
                &CueLimits {
                    max_cues: 0,
                    ..limits
                }
            )
            .expect_err("cues")
            .code,
            "webvtt_limit"
        );
        assert_eq!(
            read_webvtt(
                "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nlong\n",
                &CueLimits {
                    max_cue_text: 2,
                    ..limits
                }
            )
            .expect_err("text")
            .code,
            "webvtt_limit"
        );
        // CRLF and a BOM read fine.
        let crlf = "WEBVTT\r\n\r\n00:00:01.000 --> 00:00:02.000\r\nHi\r\n";
        assert_eq!(
            read_webvtt(crlf, &limits).expect("crlf"),
            vec![cue(1.0, 2.0, "Hi")]
        );
        assert_eq!(
            read_webvtt(&format!("\u{feff}{crlf}"), &limits).expect("bom"),
            vec![cue(1.0, 2.0, "Hi")]
        );
    }

    #[test]
    fn ttml_round_trip_keeps_times_and_text() {
        let cues = vec![cue(0.5, 2.25, "Hello & <there>\nsecond"), cue(3.0, 4.0, "Bye")];
        let text = to_ttml(&cues, "de");
        assert!(text.contains("xml:lang=\"de\""));
        let back = read_ttml(&text, &CueLimits::default()).expect("reads");
        assert_eq!(back, cues);
    }

    #[test]
    fn ttml_refuses_hostile_or_broken_input() {
        let limits = CueLimits::default();
        assert_eq!(
            read_ttml("<html></html>", &limits).expect_err("root").code,
            "ttml_root"
        );
        assert_eq!(
            read_ttml("<tt></tt>", &limits).expect_err("no cues").code,
            "ttml_root"
        );
        assert_eq!(
            read_ttml(
                "<tt><body><div><p begin=\"x\" end=\"00:00:01.000\">t</p></div></body></tt>",
                &limits
            )
            .expect_err("time")
            .code,
            "ttml_time"
        );
        assert_eq!(
            read_ttml(
                "<tt><body><div><p begin=\"00:00:02.000\" end=\"00:00:01.000\">t</p></div></body></tt>",
                &limits
            )
            .expect_err("order")
            .code,
            "ttml_cue_order"
        );
        assert_eq!(
            read_ttml(
                "<tt><body><div><p begin=\"00:00:01.000\" end=\"00:00:02.000\">long</p></div></body></tt>",
                &CueLimits {
                    max_cue_text: 2,
                    ..limits
                }
            )
            .expect_err("text")
            .code,
            "ttml_limit"
        );
        // Spans and styling around the cues are ignored.
        let styled = "<tt><head><styling/></head><body><div><p begin=\"00:00:01.000\" end=\"00:00:02.000\"><span>t</span>wo<br/>lines</p></div></body></tt>";
        let cues = read_ttml(styled, &limits).expect("styled");
        assert_eq!(cues, vec![cue(1.0, 2.0, "two\nlines")]);
    }

    #[test]
    fn ttml_ignores_tags_that_only_start_with_p() {
        // IMSC documents put `<profile>` in the head. A search for the prefix `<p` used to
        // treat that tag as a cue and reject the file for a missing `begin`.
        let doc = "<tt xmlns=\"http://www.w3.org/ns/ttml\"><head><profile use=\"http://www.w3.org/ns/ttml/profile/imsc1/text\"/></head><body><div><p begin=\"00:00:01.000\" end=\"00:00:02.000\">Hi</p></div></body></tt>";
        let cues = read_ttml(doc, &CueLimits::default()).expect("profile is not a cue");
        assert_eq!(cues, vec![cue(1.0, 2.0, "Hi")]);
    }

    #[test]
    fn ttml_language_quotes_do_not_inject_cues() {
        let hostile = "en\"><p begin=\"00:00:00.000\" end=\"00:00:01.000\">x</p><x y=\"";
        let text = to_ttml(&[cue(1.0, 2.0, "Hi")], hostile);
        let cues = read_ttml(&text, &CueLimits::default()).expect("one document");
        assert_eq!(cues, vec![cue(1.0, 2.0, "Hi")]);
        assert!(
            !text.contains("xml:lang=\"en\""),
            "the quote must not close the lang attribute: {text}"
        );
    }

    #[test]
    fn track_mapping_is_exact_apart_from_the_open_last_cue() {
        let track = regia_asset::dialogue::SubtitleTrack {
            format_version: regia_asset::dialogue::SUBTITLES_VERSION,
            priority: 2,
            keep_breaks: true,
            one_at_a_time: false,
            cues: vec![
                regia_asset::dialogue::Cue {
                    at: 0.0,
                    until: None,
                    text_key: None,
                    text: Some("a".to_owned()),
                },
                regia_asset::dialogue::Cue {
                    at: 1.0,
                    until: None,
                    text_key: None,
                    text: Some("b".to_owned()),
                },
            ],
        };
        // The open first cue runs to the next cue's start; the open last cue exports with
        // end == start and imports back open.
        let timed = track_to_timed(&track);
        assert_eq!(timed, vec![cue(0.0, 1.0, "a"), cue(1.0, 1.0, "b")]);
        let back = timed_to_track(&timed);
        assert_eq!(back.cues[0].until, Some(1.0));
        assert_eq!(back.cues[1].until, None);
        assert_eq!(back.cues[1].text.as_deref(), Some("b"));
        assert_eq!(quantize(1.23456), 1.235);
    }

    /// A WebVTT header may carry metadata lines (`Kind:`, `Language:`) up to the first empty
    /// line; they are not cues. Exported files from video sites carry them.
    #[test]
    fn webvtt_skips_header_metadata_lines() {
        let text = "WEBVTT\nKind: captions\nLanguage: en\n\n00:00:01.000 --> 00:00:02.000\nHi\n";
        let cues = read_webvtt(text, &CueLimits::default()).expect("reads");
        assert_eq!(cues, vec![cue(1.0, 2.0, "Hi")]);
        // A cue right under the signature (no empty line) is still a cue.
        let text = "WEBVTT\n00:00:01.000 --> 00:00:02.000\nHi\n";
        let cues = read_webvtt(text, &CueLimits::default()).expect("reads");
        assert_eq!(cues, vec![cue(1.0, 2.0, "Hi")]);
        // The signature is the whole word.
        let err = read_webvtt("WEBVTTX\n\n", &CueLimits::default()).expect_err("no signature");
        assert_eq!(err.code, "webvtt_header");
    }

    /// A lone CR ends a line in WebVTT: such a file must not read as zero cues.
    #[test]
    fn webvtt_reads_cr_only_line_endings() {
        let text = "WEBVTT\r\r00:00:01.000 --> 00:00:02.000\rHi\r\r00:00:03.000 --> 00:00:04.000\rBye\r";
        let cues = read_webvtt(text, &CueLimits::default()).expect("reads");
        assert_eq!(cues, vec![cue(1.0, 2.0, "Hi"), cue(3.0, 4.0, "Bye")]);
    }

    /// Cue text is escaped on export and resolved on import: a vendor's `&amp;` is `&`, markup is
    /// not shown as text, and our own export round-trips `<`, `>` and `&` exactly.
    #[test]
    fn webvtt_text_escapes_and_resolves_references_and_tags() {
        let text = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<v Tom>Tom &amp; Jerry</v> &lt;3\n<i>Hi</i> <c.loud>there</c> &#x2019;\n";
        let cues = read_webvtt(text, &CueLimits::default()).expect("reads");
        assert_eq!(cues, vec![cue(1.0, 2.0, "Tom & Jerry <3\nHi there \u{2019}")]);
        let cues = vec![cue(0.0, 1.0, "a <3 b & c > d <i>")];
        let out = to_webvtt(&cues);
        assert!(out.contains("a &lt;3 b &amp; c &gt; d &lt;i&gt;"), "{out}");
        assert_eq!(read_webvtt(&out, &CueLimits::default()).expect("reads"), cues);
        // A `<` that starts no cue tag is text.
        let text = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n1 < 2 and <3\n";
        let cues = read_webvtt(text, &CueLimits::default()).expect("reads");
        assert_eq!(cues[0].text, "1 < 2 and <3");
    }

    /// An empty line inside a cue would end its block: the export keeps it as one space, so the
    /// file reads back, and a line of spaces on import is text, not a block end.
    #[test]
    fn webvtt_cue_text_with_an_empty_line_reads_back() {
        let cues = vec![cue(0.0, 1.0, "a\n\nb"), cue(2.0, 3.0, "c")];
        let out = to_webvtt(&cues);
        let back = read_webvtt(&out, &CueLimits::default()).expect("reads back");
        assert_eq!(back, vec![cue(0.0, 1.0, "a\n \nb"), cue(2.0, 3.0, "c")]);
        assert_eq!(to_webvtt(&back), out, "a second round trip is stable");
    }

    /// The W3C "collect a WebVTT block" step ends a block at a later line that contains `-->`
    /// once the block already has its timing line, so a separator line holding spaces or a tab
    /// (common after hand edits or Windows tools) cannot merge two cues into one.
    #[test]
    fn webvtt_second_timing_line_starts_a_new_block() {
        for sep in ["   ", "\t"] {
            let text = format!(
                "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHi\n{sep}\n00:00:03.000 --> 00:00:04.000\nBye\n"
            );
            let cues = read_webvtt(&text, &CueLimits::default()).expect("reads");
            assert_eq!(
                cues,
                vec![cue(1.0, 2.0, "Hi"), cue(3.0, 4.0, "Bye")],
                "separator {sep:?}"
            );
        }
        // An identifier line before the timing line does not change the split.
        let text = "WEBVTT\n\nfirst\n00:00:01.000 --> 00:00:02.000\nHi\n00:00:03.000 --> 00:00:04.000\nBye\n";
        let cues = read_webvtt(text, &CueLimits::default()).expect("reads");
        assert_eq!(cues, vec![cue(1.0, 2.0, "Hi"), cue(3.0, 4.0, "Bye")]);
    }

    /// TTML text resolves every predefined and numeric character reference, and source layout
    /// (line breaks and indentation) is not caption text.
    #[test]
    fn ttml_text_resolves_references_and_collapses_source_layout() {
        let text = "<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div>\n<p begin=\"00:00:01.000\" end=\"00:00:02.000\">\n    Don&apos;t say &quot;hi&quot; &#39;x&#x2019;\n</p>\n<p begin=\"00:00:03.000\" end=\"00:00:04.000\">\n    first<br/>\n    second <span>and</span>\n    third\n</p>\n</div></body></tt>\n";
        let cues = read_ttml(text, &CueLimits::default()).expect("reads");
        assert_eq!(
            cues,
            vec![
                cue(1.0, 2.0, "Don't say \"hi\" 'x\u{2019}"),
                cue(3.0, 4.0, "first\nsecond and third"),
            ]
        );
        // Our own export still round-trips exactly, spaces included.
        let ours = vec![cue(0.0, 1.0, "two  spaces\n indented & <b>")];
        let back = read_ttml(&to_ttml(&ours, "en"), &CueLimits::default()).expect("reads");
        assert_eq!(back, ours);
    }

    /// A tab in cue text round-trips byte for byte: the export writes it as a numeric character
    /// reference and the reader decodes it, so it is not collapsed into a source-layout space.
    #[test]
    fn ttml_tab_round_trips_exactly() {
        let cues = vec![cue(0.0, 1.0, "Name:\tValue")];
        let out = to_ttml(&cues, "en");
        assert!(out.contains("Name:&#9;Value"), "{out}");
        let back = read_ttml(&out, &CueLimits::default()).expect("reads");
        assert_eq!(back, cues);
    }

    /// Error lines in a TTML file are counted in one pass, and still point at the right line.
    #[test]
    fn ttml_error_lines_count_from_the_last_cue() {
        let text = "<tt>\n<p begin=\"00:00:01.000\" end=\"00:00:02.000\">a</p>\n\n<p begin=\"00:00:03.000\" end=\"bad\">b</p>\n</tt>\n";
        let err = read_ttml(text, &CueLimits::default()).expect_err("bad end");
        assert_eq!((err.code, err.line), ("ttml_time", 4));
    }
}
