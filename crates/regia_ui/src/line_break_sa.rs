// SPDX-License-Identifier: MPL-2.0
//! Word segmentation for UAX #14 class SA scripts — Thai, Lao, Khmer and Myanmar — whose
//! orthographies write words without spaces (spec 36; ADR-3323; `docs/specs/18` § 2).
//!
//! cosmic-text's word wrap follows UAX #14, which assigns these scripts' letters line-break
//! class SA and offers no break opportunities without a dictionary, so a Thai sentence used to
//! wrap mid-word at arbitrary glyphs. This module asks ICU's line segmenter (compiled data:
//! dictionaries for all four scripts plus LSTM models where available) for the word boundaries
//! and hands them to the text wrapper as zero-width break opportunities. The segmentation
//! result is cached per string, so a text shaped once is never re-segmented per frame.
//!
//! Public sources (the code here is original):
//! - Unicode Standard Annex #14, "Unicode Line Breaking Algorithm" (the SA class):
//!   <https://www.unicode.org/reports/tr14/>
//! - ICU4X `LineSegmenter` (the segmentation engine and its data): `icu_segmenter` crate,
//!   Unicode License v3, see `docs/03-workspace-and-dependencies.md`.

use std::cell::RefCell;

use icu_segmenter::options::LineBreakOptions;
use icu_segmenter::{LineSegmenter, LineSegmenterBorrowed};

/// One cached segmentation.
#[derive(Debug)]
struct Entry {
    /// `DefaultHasher` of `text` (the full string is compared before a hit is trusted).
    hash: u64,
    text: Box<str>,
    /// Internal break offsets: sorted byte offsets strictly inside `text` where the segmenter
    /// allows a line break (0 and `text.len()` are never stored).
    breaks: Box<[usize]>,
}

/// How many segmentations are kept; covers the texts on screen plus the focused input's label.
const CACHE_CAP: usize = 32;

/// Dictionary-backed segmenter for the SA scripts, with a small LRU cache keyed by the whole
/// string. Cheap to construct: [`LineSegmenter::new_auto`] borrows compiled data.
#[derive(Debug)]
pub struct SaSegmenter {
    /// Borrows ICU's compiled data; `Copy`, `Send` and `Sync`.
    segmenter: LineSegmenterBorrowed<'static>,
    /// Most recently used last.
    cache: Vec<Entry>,
    /// Segmentations performed since construction (tests: cache hits never touch this).
    segments: u64,
}

impl Default for SaSegmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl SaSegmenter {
    /// A segmenter over ICU's compiled data (`new_auto`: the best dictionary/LSTM available
    /// per script).
    pub fn new() -> Self {
        Self {
            segmenter: LineSegmenter::new_auto(LineBreakOptions::default()),
            cache: Vec::new(),
            segments: 0,
        }
    }

    /// Sorted internal break offsets of `text` (byte offsets where a line break is allowed).
    ///
    /// The result is cached: repeating a string is a lookup, so re-shaping or re-measuring the
    /// same text never re-runs the segmenter. A real segmentation logs a `ui_sa_segment`
    /// tracing span.
    pub fn breaks(&mut self, text: &str) -> &[usize] {
        let hash = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(text, &mut h);
            std::hash::Hasher::finish(&h)
        };
        let hit = self.cache.iter().position(|e| e.hash == hash && &*e.text == text);
        if let Some(pos) = hit {
            let entry = self.cache.remove(pos);
            self.cache.push(entry);
            let idx = self.cache.len() - 1;
            return &self.cache[idx].breaks;
        }
        let _span = tracing::debug_span!("ui_sa_segment", chars = text.chars().count()).entered();
        self.segments = self.segments.saturating_add(1);
        let mut all: Vec<usize> = self.segmenter.segment_str(text).collect();
        // `segment_str` yields 0 first and `text.len()` last; keep the internal boundaries.
        if all.len() > 1 {
            all.pop();
        }
        if !all.is_empty() && all[0] == 0 {
            all.remove(0);
        }
        let breaks: Box<[usize]> = all.into_boxed_slice();
        if self.cache.len() >= CACHE_CAP {
            self.cache.remove(0);
        }
        self.cache.push(Entry {
            hash,
            text: text.into(),
            breaks,
        });
        let idx = self.cache.len() - 1;
        &self.cache[idx].breaks
    }

    /// How many real segmentations (cache misses) this segmenter has run.
    pub fn segments(&self) -> u64 {
        self.segments
    }
}

/// True for a character of one of the four dictionary-segmented SA blocks (Thai, Lao, Khmer,
/// Myanmar, including the Myanmar extensions and Khmer symbols).
pub(crate) fn is_sa_char(c: char) -> bool {
    matches!(c,
        '\u{0E01}'..='\u{0E5B}' // Thai
        | '\u{0E81}'..='\u{0EDD}' // Lao
        | '\u{1780}'..='\u{17FF}' // Khmer
        | '\u{19E0}'..='\u{19FF}' // Khmer symbols
        | '\u{1000}'..='\u{109F}' // Myanmar
        | '\u{AA60}'..='\u{AA7F}' // Myanmar ext A
        | '\u{A9E0}'..='\u{A9FF}' // Myanmar ext B
    )
}

/// True when `text` contains a character of one of the four dictionary-segmented SA blocks
/// (Thai, Lao, Khmer, Myanmar, including the Myanmar extensions and Khmer symbols). Texts
/// without one take no SA path at all and shape byte-identically to before (ADR-3323).
pub fn has_sa(text: &str) -> bool {
    text.chars().any(is_sa_char)
}

thread_local! {
    static SHARED: RefCell<SaSegmenter> = RefCell::new(SaSegmenter::new());
}

/// Calls `f` with the cached internal break offsets of `text` (empty slice when the text has no
/// SA characters), through a lazily created segmenter shared by the pure text helpers (caption
/// wrapping, word-wise caret movement). Layout goes through [`TextSystem`](crate::text::TextSystem)'s
/// own segmenter instead.
pub fn with_sa_breaks<R>(text: &str, f: impl FnOnce(&[usize]) -> R) -> R {
    if !has_sa(text) {
        return f(&[]);
    }
    SHARED.with(|s| {
        let mut seg = s.borrow_mut();
        let breaks = seg.breaks(text);
        f(breaks)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixtures used across the lane: ICU's segmentation of hand-checked vocabulary.
    /// ฉัน "I", กิน "eat", ข้าว "rice" — each its own word.
    #[test]
    fn thai_sentence_breaks_into_words() {
        let mut seg = SaSegmenter::new();
        let thai = "ฉันกินข้าว";
        let breaks = seg.breaks(thai);
        assert_eq!(breaks, ["ฉัน".len(), "ฉันกิน".len()]);
    }

    #[test]
    fn breaks_repeat_from_cache_without_resegmenting() {
        let mut seg = SaSegmenter::new();
        let thai = "ฉันกินข้าว";
        let first: Vec<usize> = seg.breaks(thai).to_vec();
        let n = seg.segments();
        for _ in 0..8 {
            assert_eq!(seg.breaks(thai), first.as_slice());
        }
        assert_eq!(seg.segments(), n, "repeated strings must be cache hits");
        // A different string segments again.
        let _ = seg.breaks("วันนี้อากาศดี");
        assert_eq!(seg.segments(), n + 1);
    }

    #[test]
    fn cache_evicts_the_oldest_entry() {
        let mut seg = SaSegmenter::new();
        let thai = "ฉันกินข้าว";
        let _ = seg.breaks(thai);
        for i in 0..CACHE_CAP {
            let _ = seg.breaks(&format!("คำ{i}"));
        }
        let n = seg.segments();
        let _ = seg.breaks(thai);
        assert_eq!(seg.segments(), n + 1, "the oldest entry was evicted");
    }

    #[test]
    fn lao_khmer_and_myanmar_segment_too() {
        let mut seg = SaSegmenter::new();
        // ຂ້າພະເຈົ້າ "I" + ກິນ "eat" + ເຂົ້າ "rice".
        assert_eq!(seg.breaks("ຂ້າພະເຈົ້າກິນເຂົ້າ"), ["ຂ້າພະເຈົ້າ".len(), "ຂ້າພະເຈົ້າກິນ".len()]);
        // ខ្ញុំ "I" + ញ៉ាំ "eat" + បាយ "rice".
        assert_eq!(seg.breaks("ខ្ញុំញ៉ាំបាយ"), ["ខ្ញុំ".len(), "ខ្ញុំញ៉ាំ".len()]);
        // ကျွန်တော် "I" + စား "eat" + ဆန် "rice".
        assert_eq!(seg.breaks("ကျွန်တော်စားဆန်"), ["ကျွန်တော်".len(), "ကျွန်တော်စား".len()]);
    }

    #[test]
    fn has_sa_detects_the_four_blocks_only() {
        assert!(has_sa("ฉัน"));
        assert!(has_sa("ຂ້າພະເຈົ້າ"));
        assert!(has_sa("ខ្ញុំ"));
        assert!(has_sa("ကျွန်တော်"));
        assert!(has_sa("save ฉัน now"));
        assert!(!has_sa("hello world"));
        assert!(!has_sa("你好世界"));
        assert!(!has_sa("مرحبا"));
        assert!(!has_sa(""));
    }

    #[test]
    fn non_sa_text_gets_no_breaks_from_the_helper() {
        let mut seen = false;
        let out = with_sa_breaks("hello world", |b| {
            seen = true;
            b.len()
        });
        assert_eq!(out, 0);
        assert!(seen);
    }
}
