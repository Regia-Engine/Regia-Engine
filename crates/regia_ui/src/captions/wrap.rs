// SPDX-License-Identifier: MPL-2.0
//! Line breaking, balancing and pagination of caption text, pure over a [`TextMeasure`]
//! (`docs/specs/56-dialogue-and-subtitles.md` §§ 2.7, 2.8).
//!
//! Break opportunities follow the subset of UAX #14 that captions need: after a space, after a
//! hyphen between letters or digits, and between ideographs (Han, Hiragana, Katakana) except that a
//! closing punctuation mark never begins a line and an opening bracket never ends one. A literal
//! `\n` is a hard break. Everything else (a long word, a URL) stays whole and, if wider than the
//! block, is split between grapheme clusters.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

/// Measures text in logical pixels. A test double makes every rule testable without a GPU.
pub trait TextMeasure {
    /// The width of `text` at `size_px`; `rtl` is the paragraph direction.
    fn width(&mut self, text: &str, size_px: f32, rtl: bool) -> f32;

    /// The width of `text[range]`. Wrapping asks this for many overlapping ranges of one text, so a
    /// shaper can answer them all from one shaping run.
    fn range_width(&mut self, text: &str, range: Range<usize>, size_px: f32, rtl: bool) -> f32 {
        self.width(&text[range], size_px, rtl)
    }
}

/// The result of [`wrap`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapped {
    /// The lines, without trailing spaces.
    pub lines: Vec<String>,
    /// The byte range of each line in the source text (trailing spaces excluded).
    pub ranges: Vec<Range<usize>>,
    /// True when there are more than the allowed lines (the caller paginates).
    pub overflow: bool,
}

/// One page of a paginated caption piece.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The page's source text.
    pub text: String,
    /// The wrapped lines of this page.
    pub lines: Vec<String>,
    /// Offset of the page start within the piece window, in microseconds.
    pub from_us: u64,
    /// Offset of the page end within the piece window, in microseconds.
    pub to_us: u64,
    /// Graphemes on this page.
    pub graphemes: usize,
}

/// The number of grapheme clusters (UAX #29); a space counts as one.
pub fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

/// True for locales whose script is written without spaces and read at the slower default
/// (`zh`, `ja`, `ko`).
pub fn is_cjk_locale(locale: &str) -> bool {
    matches!(primary(locale), "zh" | "ja" | "ko")
}

/// True for right-to-left locales (`ar`, `he`, `fa`, `ur`): the direction comes from the locale,
/// not from the first character.
pub fn is_rtl_locale(locale: &str) -> bool {
    matches!(primary(locale), "ar" | "he" | "fa" | "ur")
}

fn primary(locale: &str) -> &str {
    locale.split(['-', '_']).next().unwrap_or("")
}

fn is_ideographic(c: char) -> bool {
    matches!(c,
        '\u{2E80}'..='\u{2FDF}' // radicals
        | '\u{3040}'..='\u{30FF}' // Hiragana, Katakana
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF66}'..='\u{FF9F}'
        | '\u{20000}'..='\u{2FA1F}')
}

fn is_closing(c: char) -> bool {
    matches!(
        c,
        '、' | '。'
            | '，'
            | '．'
            | '）'
            | '」'
            | '』'
            | '】'
            | '〕'
            | '〉'
            | '》'
            | '！'
            | '？'
            | '：'
            | '；'
            | 'ー'
            | '々'
            | ')'
            | ']'
            | '}'
            | '!'
            | '?'
            | ','
            | '.'
            | ':'
            | ';'
            | '”'
            | '’'
            | '…'
    )
}

fn is_opening(c: char) -> bool {
    matches!(
        c,
        '（' | '「' | '『' | '【' | '〔' | '〈' | '《' | '(' | '[' | '{' | '“' | '‘'
    )
}

/// May a line break between `prev` and `next` (adjacent, no space between)?
fn break_between(prev: char, next: char) -> bool {
    if is_closing(next) || is_opening(prev) {
        return false;
    }
    if prev == '-' && next.is_alphanumeric() {
        return true;
    }
    is_ideographic(prev) || is_ideographic(next)
}

/// Splits text into chunks that cannot be broken inside; each chunk keeps its trailing spaces.
/// `'\n'` ends a chunk and is reported as a hard break by `hard`. `sa_breaks` are byte offsets
/// where dictionary segmentation (Thai, Lao, Khmer, Myanmar; ADR-3323) allows an extra break.
fn chunks(text: &str, sa_breaks: &[usize]) -> Vec<(Range<usize>, bool)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\n' {
            out.push((start..i, true));
            start = i + c.len_utf8();
            prev = None;
            continue;
        }
        if c == ' ' {
            // Absorb the run of spaces into this chunk.
            let mut end = i + 1;
            while let Some(&(j, d)) = chars.peek() {
                if d == ' ' {
                    end = j + 1;
                    chars.next();
                } else {
                    break;
                }
            }
            if end > start {
                out.push((start..end, false));
                start = end;
            }
            prev = Some(' ');
            continue;
        }
        let sa_break = sa_breaks.binary_search(&i).is_ok();
        if let Some(p) = prev
            && p != ' '
            && i > start
            && (sa_break || break_between(p, c))
        {
            out.push((start..i, false));
            start = i;
        }
        prev = Some(c);
    }
    if start < text.len() {
        out.push((start..text.len(), false));
    }
    out
}

fn trimmed(text: &str, range: &Range<usize>) -> Range<usize> {
    let slice = &text[range.clone()];
    range.start..range.start + slice.trim_end_matches(' ').len()
}

/// Wraps `text` to `width` logical pixels, greedily, then balances the last line.
/// `max_lines` only decides the `overflow` flag. Pure over `m`. Texts with UAX #14 class SA
/// scripts (Thai, Lao, Khmer, Myanmar) additionally break at the segmenter's word boundaries
/// (ADR-3323).
pub fn wrap(
    text: &str,
    width: f32,
    max_lines: usize,
    rtl: bool,
    size_px: f32,
    m: &mut dyn TextMeasure,
) -> Wrapped {
    let all = crate::line_break_sa::with_sa_breaks(text, |sa| chunks(text, sa));
    // Each line is a list of chunk ranges.
    let mut lines: Vec<Vec<Range<usize>>> = vec![Vec::new()];
    for (range, hard) in all {
        let line_start = lines.last().and_then(|l| l.first()).map(|r| r.start);
        let candidate = line_start.map(|s| s..trimmed(text, &range).end);
        let fits = match candidate {
            Some(c) if !c.is_empty() => m.range_width(text, c, size_px, rtl) <= width,
            _ => true,
        };
        if !fits
            && let Some(last) = lines.last_mut()
            && !last.is_empty()
        {
            lines.push(Vec::new());
        }
        // A chunk wider than the block alone is split between graphemes.
        let own = trimmed(text, &range);
        if !own.is_empty() && m.range_width(text, own.clone(), size_px, rtl) > width {
            if lines.last().is_some_and(|l| !l.is_empty()) {
                lines.push(Vec::new());
            }
            let mut piece_start = own.start;
            let mut piece_end = own.start;
            for (offset, g) in text[own.clone()].grapheme_indices(true) {
                let g_end = own.start + offset + g.len();
                if piece_end > piece_start && m.range_width(text, piece_start..g_end, size_px, rtl) > width {
                    if let Some(last) = lines.last_mut() {
                        last.push(piece_start..piece_end);
                    }
                    lines.push(Vec::new());
                    piece_start = piece_end;
                }
                piece_end = g_end;
            }
            if let Some(last) = lines.last_mut() {
                last.push(piece_start..range.end);
            }
        } else if let Some(last) = lines.last_mut() {
            last.push(range);
        }
        if hard {
            lines.push(Vec::new());
        }
    }
    lines.retain(|l| !l.is_empty());
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    balance(text, &mut lines, width, size_px, rtl, m);

    let mut out_lines = Vec::new();
    let mut out_ranges = Vec::new();
    for line in &lines {
        let (Some(first), Some(last)) = (line.first(), line.last()) else {
            out_lines.push(String::new());
            out_ranges.push(0..0);
            continue;
        };
        let range = trimmed(text, &(first.start..last.end));
        out_lines.push(text[range.clone()].to_string());
        out_ranges.push(range);
    }
    let overflow = out_lines.len() > max_lines;
    Wrapped {
        lines: out_lines,
        ranges: out_ranges,
        overflow,
    }
}

fn line_width(text: &str, line: &[Range<usize>], size_px: f32, rtl: bool, m: &mut dyn TextMeasure) -> f32 {
    match (line.first(), line.last()) {
        (Some(first), Some(last)) => {
            let range = trimmed(text, &(first.start..last.end));
            m.range_width(text, range, size_px, rtl)
        }
        _ => 0.0,
    }
}

/// Moves a break earlier while the last line holds under a third of the first line and the move
/// keeps the previous line non-empty and the new last line no longer than the one it came from.
fn balance(
    text: &str,
    lines: &mut [Vec<Range<usize>>],
    width: f32,
    size_px: f32,
    rtl: bool,
    m: &mut dyn TextMeasure,
) {
    let n = lines.len();
    if n < 2 {
        return;
    }
    let first_w = line_width(text, &lines[0], size_px, rtl, m);
    loop {
        let last_w = line_width(text, &lines[n - 1], size_px, rtl, m);
        if last_w * 3.0 >= first_w || lines[n - 2].len() < 2 {
            return;
        }
        let Some(moved) = lines[n - 2].last().cloned() else {
            return;
        };
        // A hard break (the chunk before ended the paragraph) must not be undone: only soft
        // breaks, which are those whose previous chunk does not end at a newline.
        let end = lines[n - 2].last().map_or(0, |r| r.end);
        if text[end..].starts_with('\n') {
            return;
        }
        let mut new_last = vec![moved.clone()];
        new_last.extend(lines[n - 1].iter().cloned());
        let new_prev: Vec<_> = lines[n - 2][..lines[n - 2].len() - 1].to_vec();
        let new_last_w = line_width(text, &new_last, size_px, rtl, m);
        let new_prev_w = line_width(text, &new_prev, size_px, rtl, m);
        if new_last_w > width || new_last_w > new_prev_w {
            return;
        }
        lines[n - 2] = new_prev;
        lines[n - 1] = new_last;
    }
}

/// Splits wrapped text into pages of at most `max_lines` lines and divides `planned_us` among them
/// in proportion to graphemes (the last page takes the remainder). Pages hold whole lines, so every
/// page boundary is a legal break; lines are spread evenly across the pages.
pub fn paginate(
    text: &str,
    planned_us: u64,
    width: f32,
    max_lines: usize,
    rtl: bool,
    size_px: f32,
    m: &mut dyn TextMeasure,
) -> Vec<Page> {
    let max_lines = max_lines.max(1);
    let wrapped = wrap(text, width, max_lines, rtl, size_px, m);
    let count = wrapped.lines.len();
    let page_count = count.div_ceil(max_lines).max(1);
    let per_page = count.div_ceil(page_count).max(1);
    let mut pages: Vec<Page> = Vec::new();
    for (index, (lines, ranges)) in wrapped
        .lines
        .chunks(per_page)
        .zip(wrapped.ranges.chunks(per_page))
        .enumerate()
    {
        let start = ranges.first().map_or(0, |r| r.start);
        let end = ranges.last().map_or(0, |r| r.end);
        let page_text = text.get(start..end).unwrap_or("").to_string();
        let graphemes = grapheme_count(&page_text).max(1);
        pages.push(Page {
            text: page_text,
            lines: lines.to_vec(),
            from_us: 0,
            to_us: 0,
            graphemes,
        });
        let _ = index;
    }
    let total: usize = pages.iter().map(|p| p.graphemes).sum::<usize>().max(1);
    let mut cursor = 0u64;
    let last = pages.len().saturating_sub(1);
    for (i, page) in pages.iter_mut().enumerate() {
        let share = if i == last {
            planned_us.saturating_sub(cursor)
        } else {
            planned_us.saturating_mul(page.graphemes as u64) / total as u64
        };
        page.from_us = cursor;
        cursor = cursor.saturating_add(share);
        page.to_us = cursor;
    }
    pages
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;

    /// Latin graphemes are 10 px wide at 20 px size; ideographs are 20 px (full width).
    pub struct Mono;

    impl TextMeasure for Mono {
        fn width(&mut self, text: &str, size_px: f32, _rtl: bool) -> f32 {
            text.graphemes(true)
                .map(|g| {
                    let wide = g.chars().next().is_some_and(is_ideographic);
                    if wide { size_px } else { size_px / 2.0 }
                })
                .sum()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Mono;
    use super::*;

    fn w(text: &str, width: f32) -> Wrapped {
        wrap(text, width, 2, false, 20.0, &mut Mono)
    }

    #[test]
    fn breaks_at_spaces_and_trims_them() {
        let out = w("one two three four", 100.0);
        assert_eq!(out.lines, vec!["one two", "three four"]);
        assert!(!out.overflow);
    }

    #[test]
    fn a_hyphen_between_letters_allows_a_break_but_not_before_a_space_run() {
        let out = w("well-known fact", 60.0);
        assert_eq!(out.lines[0], "well-");
        assert!(out.lines.iter().all(|l| Mono.width(l, 20.0, false) <= 60.0));
    }

    #[test]
    fn long_word_splits_between_graphemes() {
        let out = w("abcdefghij", 40.0);
        assert_eq!(out.lines, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn cjk_breaks_between_ideographs_but_not_before_closing_punctuation() {
        // 5 ideographs per 100 px line at 20 px.
        let out = wrap("今日は天気がいい。散歩に行こう", 100.0, 2, false, 20.0, &mut Mono);
        assert_eq!(out.lines[0].chars().count(), 5);
        for line in &out.lines {
            let first = line.chars().next().unwrap_or(' ');
            assert!(!is_closing(first), "line starts with closing punctuation: {line}");
        }
        // The fifth character would be followed by '。': the break must move earlier or keep it with the line.
        let out = wrap("あいうえ。おかきく", 100.0, 3, false, 20.0, &mut Mono);
        assert!(out.lines.iter().all(|l| !l.starts_with('。')));
    }

    #[test]
    fn opening_bracket_never_ends_a_line() {
        let out = wrap("あいうえ「おかきく」", 100.0, 3, false, 20.0, &mut Mono);
        assert!(out.lines.iter().all(|l| !l.ends_with('「')), "{:?}", out.lines);
    }

    #[test]
    fn hard_breaks_split_paragraphs() {
        let out = w("ab\ncd", 200.0);
        assert_eq!(out.lines, vec!["ab", "cd"]);
    }

    #[test]
    fn balance_moves_a_word_down_when_the_last_line_is_tiny() {
        // Greedy gives "aaaa bbbb cccc" / "d"; balancing moves "cccc" down.
        let out = w("aaaa bbbb cccc d", 150.0);
        assert_eq!(out.lines, vec!["aaaa bbbb", "cccc d"]);
    }

    #[test]
    fn overflow_flag_follows_max_lines() {
        let out = wrap("one two three four five six", 60.0, 2, false, 20.0, &mut Mono);
        assert!(out.overflow);
    }

    #[test]
    fn ranges_index_the_source() {
        let text = "one two three four";
        let out = w(text, 90.0);
        for (line, range) in out.lines.iter().zip(&out.ranges) {
            assert_eq!(&text[range.clone()], line);
        }
    }

    #[test]
    fn pagination_splits_by_grapheme_proportion() {
        let text = "aaaa bbbb cccc dddd eeee ffff";
        let pages = paginate(text, 6_000_000, 90.0, 2, false, 20.0, &mut Mono);
        assert!(pages.len() >= 2);
        assert_eq!(pages.first().map(|p| p.from_us), Some(0));
        assert_eq!(pages.last().map(|p| p.to_us), Some(6_000_000));
        for pair in pages.windows(2) {
            assert_eq!(pair[0].to_us, pair[1].from_us);
        }
        for page in &pages {
            assert!(page.lines.len() <= 2);
            assert!(page.lines.iter().all(|l| Mono.width(l, 20.0, false) <= 90.0));
        }
        let total: usize = pages.iter().map(|p| p.graphemes).sum();
        let expected: u64 = 6_000_000;
        let first = &pages[0];
        assert_eq!(
            first.to_us - first.from_us,
            expected * first.graphemes as u64 / total as u64
        );
    }

    #[test]
    fn short_text_is_one_page_with_the_full_window() {
        let pages = paginate("hello", 2_000_000, 200.0, 2, false, 20.0, &mut Mono);
        assert_eq!(pages.len(), 1);
        assert_eq!((pages[0].from_us, pages[0].to_us), (0, 2_000_000));
    }

    #[test]
    fn thai_breaks_at_dictionary_word_boundaries() {
        // ฉัน / กำลัง / กิน — hand-checked Thai vocabulary, ICU segmentation (ADR-3323). The
        // fake measure counts graphemes at 10 px: ฉัน (2 graphemes) fits 35 px, ฉัน+กำลัง (5
        // graphemes = 50 px) does not, and 4 graphemes — what glyph fallback would put on the
        // first line — split กำลัง mid-word.
        let m = &mut Mono;
        let width = 35.0;
        let out = wrap("ฉันกำลังกิน", width, 3, false, 20.0, m);
        assert_eq!(
            out.lines,
            vec!["ฉัน", "กำลัง", "กิน"],
            "each line must hold exactly one word"
        );
        for (line, range) in out.lines.iter().zip(&out.ranges) {
            assert_eq!(&"ฉันกำลังกิน"[range.clone()], line);
        }
    }

    #[test]
    fn mixed_latin_thai_breaks_at_either_scripts_boundaries() {
        // Words: go | ฉัน | กิน | ข้าว. At 35 px each Thai word fits whole (ข้าว is the widest
        // at 30 px) but two do not (40 px); without dictionary breaks the run splits mid-word
        // after three graphemes.
        let m = &mut Mono;
        let out = wrap("go ฉันกินข้าว", 35.0, 4, false, 20.0, m);
        assert_eq!(
            out.lines,
            vec!["go", "ฉัน", "กิน", "ข้าว"],
            "Thai words stay whole, Latin wraps at its space"
        );
    }

    #[test]
    fn locale_helpers() {
        assert!(is_rtl_locale("ar-EG"));
        assert!(is_rtl_locale("he"));
        assert!(!is_rtl_locale("en"));
        assert!(is_cjk_locale("ja-JP"));
        assert!(is_cjk_locale("zh_Hans"));
        assert!(!is_cjk_locale("pt-BR"));
    }

    #[test]
    fn graphemes_count_clusters_once() {
        assert_eq!(grapheme_count("e\u{301}a"), 2);
        assert_eq!(grapheme_count("a b"), 3);
        assert_eq!(grapheme_count("👨‍👩‍👧"), 1);
    }
}
