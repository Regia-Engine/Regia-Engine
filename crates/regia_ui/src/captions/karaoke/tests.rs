// SPDX-License-Identifier: MPL-2.0
use super::*;
use crate::captions::{TextMeasure, wrap};
use crate::text::{CaretCluster, CaretLine};

struct Mono;
impl TextMeasure for Mono {
    fn width(&mut self, text: &str, size_px: f32, _rtl: bool) -> f32 {
        text.chars().count() as f32 * size_px * 0.5
    }
}

#[test]
fn words_join_punctuation_and_split_on_spaces() {
    let t = "\"Hello,\" she said -- don't go!";
    let words: Vec<&str> = word_ranges(t).into_iter().map(|r| &t[r]).collect();
    assert_eq!(words, vec!["\"Hello,\"", "she", "said", "don't", "go!"]);
}

#[test]
fn ideographs_are_one_word_each() {
    let t = "你好，世界。";
    let words: Vec<&str> = word_ranges(t).into_iter().map(|r| &t[r]).collect();
    assert_eq!(words, vec!["你", "好，", "世", "界。"]);
}

#[test]
fn syllable_estimate() {
    assert_eq!(syllable_weight("a"), 1);
    assert_eq!(syllable_weight("hello"), 2);
    assert_eq!(syllable_weight("beautiful"), 3);
    assert_eq!(syllable_weight("hmm"), 1);
    assert_eq!(syllable_weight("2026"), 2);
    assert_eq!(syllable_weight("好"), 1);
}

#[test]
fn estimate_tiles_the_whole_duration() {
    let text = "one two seven four five";
    let w = word_timings(text, 1_000_000, 3_500_000);
    assert_eq!(w.len(), 5);
    assert_eq!(w[0].start_us, 1_000_000);
    assert_eq!(w.last().unwrap().end_us, 3_500_000);
    for pair in w.windows(2) {
        assert_eq!(pair[0].end_us, pair[1].start_us, "no gap, no overlap");
    }
    // "seven" has two syllables and gets more time than "one" (one syllable; "three" was used
    // here before, but it has one syllable: lead decision on spec 56 § 12, ADR-5959).
    let one = w[0].end_us - w[0].start_us;
    let seven = w[2].end_us - w[2].start_us;
    assert!(seven > one);
}

/// Common English words get their dictionary syllable counts: silent final `e`, `-ed`, `-es`,
/// consonant `y` before a vowel and a word-final `ia`/`io` hiatus (ADR-5959). The plain
/// vowel-group count gave "one" 2, "five" 2, "jumped" 2, "makes" 2, "player" 1, "radio" 2.
#[test]
fn english_words_get_their_syllable_counts() {
    for (word, syllables) in [
        ("one", 1),
        ("two", 1),
        ("three", 1),
        ("four", 1),
        ("five", 1),
        ("seven", 2),
        ("there", 1),
        ("whole", 1),
        ("the", 1),
        ("be", 1),
        ("table", 2),
        ("little", 2),
        ("people", 2),
        ("jumped", 1),
        ("loved", 1),
        ("wanted", 2),
        ("needed", 2),
        ("need", 1),
        ("makes", 1),
        ("goes", 1),
        ("boxes", 2),
        ("places", 2),
        ("wishes", 2),
        ("yes", 1),
        ("player", 2),
        ("happy", 2),
        ("they", 1),
        ("radio", 3),
        ("media", 3),
        ("via", 2),
        ("Asia", 2),
        ("hello", 2),
        ("beautiful", 3),
        ("don't", 1),
        ("we've", 1),
        ("\"Five,\"", 1),
        ("ONE", 1),
    ] {
        assert_eq!(syllable_weight(word), syllables, "{word}");
    }
    // Non-English spellings keep the plain vowel-group count.
    assert_eq!(syllable_weight("café"), 2);
    assert_eq!(syllable_weight("née"), 1);
}

#[test]
fn highlight_advances_word_by_word_at_the_cue_times() {
    // Two sub-cue pieces: the second piece's words fill exactly its own window.
    let a = word_timings("go now", 0, 1_000_000);
    let b = word_timings("or stay", 1_500_000, 3_000_000);
    assert_eq!(b[0].start_us, 1_500_000, "a sub-cue start is a word start");
    assert_eq!(words_started(&a, 0), 1);
    assert_eq!(words_started(&a, a[1].start_us - 1), 1);
    assert_eq!(words_started(&a, a[1].start_us), 2);
    assert_eq!(highlighted_prefix(&a, 0), "go".len());
    assert_eq!(highlighted_prefix(&a, 999_999.max(a[1].start_us)), "go now".len());
    assert_eq!(words_started(&b, 1_499_999), 0);
    assert_eq!(highlighted_prefix(&b, 1_499_999), 0);
    assert_eq!(highlighted_prefix(&b, 1_500_000), 2);
    assert_eq!(highlighted_prefix(&b, 3_000_000), "or stay".len());
}

#[test]
fn degenerate_windows_do_not_panic() {
    assert!(word_timings("", 0, 100).is_empty());
    assert!(word_timings("...", 0, 100).is_empty());
    let w = word_timings("a b", 500, 100);
    assert!(w.iter().all(|w| w.start_us == 500 && w.end_us == 500));
}

#[test]
fn highlighted_lines_wrap_identically() {
    let text = "the quick brown fox jumps over the lazy dog again and again";
    let words = word_timings(text, 0, 6_000_000);
    let reference = wrap(text, 160.0, 3, false, 16.0, &mut Mono);
    for t in (0..=6_000_000).step_by(250_000) {
        // Highlighting never touches the text: wrapping it again gives the same lines and ranges.
        let prefix = highlighted_prefix(&words, t);
        let again = wrap(text, 160.0, 3, false, 16.0, &mut Mono);
        assert_eq!(again, reference);
        let hl = line_highlights(&reference.ranges, prefix);
        assert_eq!(hl.len(), reference.lines.len());
        for (h, r) in hl.iter().zip(&reference.ranges) {
            assert!(h.start == r.start && h.end >= r.start && h.end <= r.end);
        }
        // Lines before the one holding the prefix are fully highlighted, after it not at all.
        let full = hl
            .iter()
            .zip(&reference.ranges)
            .filter(|(h, r)| h.end == r.end && r.end > r.start)
            .count();
        let none = hl.iter().filter(|h| h.is_empty()).count();
        assert!(full + none >= hl.len() - 1);
    }
}

#[test]
fn rtl_clusters_highlight_in_reading_order() {
    // Logical text "abcd", shaped right to left: visual order is d c b a (x0 ascending).
    let clusters: Vec<CaretCluster> = (0..4)
        .rev()
        .enumerate()
        .map(|(vis, b)| CaretCluster {
            start: b,
            end: b + 1,
            x0: vis as f32 * 10.0,
            x1: vis as f32 * 10.0 + 10.0,
            rtl: true,
        })
        .collect();
    let map = CaretMap {
        text: "abcd".into(),
        lines: vec![CaretLine {
            top: 0.0,
            height: 10.0,
            start: 0,
            end: 4,
            origin_x: 0.0,
            rtl: true,
            clusters,
        }],
    };
    let flags = mark_clusters(&map, 2);
    // Visual d, c are not highlighted; b, a are (the right-hand side reads first).
    assert_eq!(flags, vec![vec![false, false, true, true]]);
}
