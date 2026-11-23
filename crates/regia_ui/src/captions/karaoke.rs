// SPDX-License-Identifier: MPL-2.0
//! Karaoke-style word highlighting for captions (`docs/specs/56-dialogue-and-subtitles.md` § 12,
//! ADR-3234).
//!
//! The highlight is a *style on text that is already wrapped and shaped*: this module only answers
//! "which byte prefix of the page text is highlighted at time `t`", and maps that prefix onto
//! wrapped lines or shaped clusters. It never changes the text, so line breaks, pagination and
//! right-to-left runs are exactly those of the unhighlighted caption. The prefix is in logical
//! (reading) order, so in a right-to-left line the highlight grows from the right.
//!
//! Timing rule (documented, deterministic):
//! 1. The window is the page's window `[from_us, to_us)`. A caption with sub-cues (a conversation
//!    line's `SubCue` list) has one piece per sub-cue and pages never cross pieces, so the window
//!    of a page is already the sub-cue's window: every word of a sub-cue lies inside it and a
//!    sub-cue start is a word start.
//! 2. Words are runs of letters and digits; punctuation joins the word before it. Ideographs are one
//!    word each.
//! 3. A word's weight is its estimated syllable count (at least 1) for alphabetic words, or its
//!    grapheme count for ideographs and other scripts; the window is divided in proportion to the
//!    weights, rounded so the words tile the window exactly with no gaps and the last word ends at
//!    `to_us`. The count is vowel groups (`a e i o u y` and accented Latin vowels), with these
//!    English rules for words spelled in ASCII letters (ADR-5959, spec 56 § 12):
//!    - `y` before a vowel is a consonant (`yes`, `player`);
//!    - a final `e` after a consonant is silent (`one`, `five`, `there`), except in a final
//!      consonant + `le` (`table`, `little`);
//!    - a final `ed` is silent unless it follows `t` or `d` (`jumped`, `loved`; `wanted` keeps it);
//!    - a final `es` is silent unless it follows `s`, `x`, `z`, `c`, `g`, `ch` or `sh` (`makes`;
//!      `boxes`, `places`, `wishes` keep it);
//!    - a word-final `ia` or `io` is two syllables unless it follows `c`, `s`, `t` or `g`
//!      (`radio`, `media`, `via`; `Asia`, `ratio` stay as estimated);
//!
//!    a silent ending is dropped only when the word keeps at least one other vowel group.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::text::CaretMap;

/// One word and when it is sung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordTiming {
    /// The word's bytes in the page text (trailing punctuation included).
    pub range: Range<usize>,
    /// When the word starts to be highlighted, in microseconds on the page's clock.
    pub start_us: u64,
    /// When the word ends.
    pub end_us: u64,
}

fn is_ideograph(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30FF}' | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}' | '\u{20000}'..='\u{2FA1F}' | '\u{AC00}'..='\u{D7AF}')
}

/// The estimated syllable weight of a word: vowel groups with the English silent-ending rules of
/// the module docs for alphabetic text, graphemes for ideographs; at least 1.
pub fn syllable_weight(word: &str) -> usize {
    if word.chars().any(is_ideograph) {
        return word
            .graphemes(true)
            .filter(|g| g.chars().any(char::is_alphanumeric))
            .count()
            .max(1);
    }
    let letters: Vec<char> = word
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    if letters.iter().all(char::is_ascii_alphabetic) && !letters.is_empty() {
        return english_syllables(&letters);
    }
    let mut groups = 0;
    let mut in_vowel = false;
    for &c in &letters {
        let v = matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
            || (!c.is_ascii() && c.is_alphabetic() && "àáâãäåæèéêëìíîïòóôõöøùúûüýÿœ".contains(c));
        if v && !in_vowel {
            groups += 1;
        }
        in_vowel = v;
    }
    if groups == 0 {
        // No vowels (digits, "hmm", non-Latin scripts): one unit per three letters.
        letters.len().div_ceil(3).max(1)
    } else {
        groups
    }
}

/// The syllable estimate of a lowercase ASCII word (the rules in the module docs).
fn english_syllables(w: &[char]) -> usize {
    let n = w.len();
    let plain_vowel = |c: char| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u');
    // `y` is a vowel unless a vowel follows it.
    let vowel = |i: usize| {
        let c = w[i];
        plain_vowel(c) || (c == 'y' && !w.get(i + 1).copied().is_some_and(plain_vowel))
    };
    let consonant = |i: usize| !vowel(i);
    let mut groups = 0;
    for i in 0..n {
        if vowel(i) && (i == 0 || !vowel(i - 1)) {
            groups += 1;
        }
    }
    if groups == 0 {
        return n.div_ceil(3).max(1);
    }
    // Silent endings: the final `e` is a vowel group of its own (a consonant before it).
    let ends = |suffix: &str| n >= suffix.len() && w[n - suffix.len()..].iter().copied().eq(suffix.chars());
    let silent = if n >= 2 && w[n - 1] == 'e' && consonant(n - 2) {
        // Consonant + `le` keeps its syllable (`table`).
        !(n >= 3 && w[n - 2] == 'l' && consonant(n - 3))
    } else if n >= 3 && (ends("ed") || ends("es")) && consonant(n - 3) {
        let before = w[n - 3];
        if ends("ed") {
            !matches!(before, 't' | 'd')
        } else {
            let sibilant = matches!(before, 's' | 'x' | 'z' | 'c' | 'g')
                || (before == 'h' && n >= 4 && matches!(w[n - 4], 'c' | 's'));
            !sibilant
        }
    } else {
        false
    };
    if silent && groups > 1 {
        groups -= 1;
    }
    // Word-final hiatus: `radio`, `media`.
    if n >= 3
        && w[n - 2] == 'i'
        && matches!(w[n - 1], 'a' | 'o')
        && !matches!(w[n - 3], 'c' | 's' | 't' | 'g')
        && consonant(n - 3)
    {
        groups += 1;
    }
    groups
}

/// Splits `text` into word ranges (see the module docs).
pub fn word_ranges(text: &str) -> Vec<Range<usize>> {
    let mut words: Vec<Range<usize>> = Vec::new();
    // Opening punctuation after a space waits for the word it belongs to.
    let mut pending: Option<usize> = None;
    for (i, tok) in text.split_word_bound_indices() {
        if tok.chars().any(char::is_alphanumeric) {
            let start = pending.take().unwrap_or(i);
            words.push(start..i + tok.len());
        } else if tok.chars().all(char::is_whitespace) {
            // Punctuation standing alone between spaces (a dash) belongs to no word.
            pending = None;
            continue;
        } else {
            match words.last_mut() {
                Some(last) if last.end == i => last.end = i + tok.len(),
                _ => {
                    pending.get_or_insert(i);
                }
            }
        }
    }
    words
}

/// Estimates word timings over `[from_us, to_us)`. The words tile the window exactly: each word
/// starts where the previous ends and the last ends at `to_us`. Empty text gives no words.
pub fn word_timings(text: &str, from_us: u64, to_us: u64) -> Vec<WordTiming> {
    let ranges = word_ranges(text);
    if ranges.is_empty() {
        return Vec::new();
    }
    let to_us = to_us.max(from_us);
    let weights: Vec<u64> = ranges
        .iter()
        .map(|r| syllable_weight(&text[r.clone()]) as u64)
        .collect();
    let total: u64 = weights.iter().sum();
    let span = u128::from(to_us - from_us);
    let mut acc = 0u64;
    let mut out = Vec::with_capacity(ranges.len());
    for (range, w) in ranges.into_iter().zip(weights) {
        let start = from_us + (span * u128::from(acc) / u128::from(total)) as u64;
        acc += w;
        let end = from_us + (span * u128::from(acc) / u128::from(total)) as u64;
        out.push(WordTiming {
            range,
            start_us: start,
            end_us: end,
        });
    }
    out
}

/// The end byte of the highlighted prefix at time `t_us`: the end of the last word that has
/// started. `0` before the first word starts.
pub fn highlighted_prefix(words: &[WordTiming], t_us: u64) -> usize {
    words
        .iter()
        .take_while(|w| w.start_us <= t_us)
        .last()
        .map_or(0, |w| w.range.end)
}

/// The number of words that have started at `t_us`.
pub fn words_started(words: &[WordTiming], t_us: u64) -> usize {
    words.iter().take_while(|w| w.start_us <= t_us).count()
}

/// For each wrapped line (`ranges` from `wrap`, bytes of the page text), the highlighted byte range
/// within it (empty when none of it is highlighted).
pub fn line_highlights(line_ranges: &[Range<usize>], prefix_end: usize) -> Vec<Range<usize>> {
    line_ranges
        .iter()
        .map(|r| {
            let end = prefix_end.clamp(r.start, r.end);
            r.start..end
        })
        .collect()
}

/// Marks each shaped cluster of `map` (per visual line) as highlighted when its bytes lie in the
/// prefix. Works for any run direction because clusters carry their byte range.
pub fn mark_clusters(map: &CaretMap, prefix_end: usize) -> Vec<Vec<bool>> {
    map.lines
        .iter()
        .map(|line| line.clusters.iter().map(|c| c.end <= prefix_end).collect())
        .collect()
}

#[cfg(test)]
mod tests;
