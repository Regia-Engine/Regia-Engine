// SPDX-License-Identifier: MPL-2.0
//! Seed-stable mutation for fuzzing loaders of untrusted bytes and text (ADR-538, ADR-1917).
//!
//! [`FuzzRng`] is a SplitMix64 stream; [`mutate_bytes`] and [`mutate_text`] corrupt a valid fixture with
//! one randomly chosen strategy (truncate, flip, splice, hostile length or version words, `NaN`/`inf`,
//! deep nesting, ...). Nothing here reads a clock or OS entropy: the same seed gives the same inputs on
//! every platform. The time-boxed test harness lives in `regia_test_utils::fuzz`; the agent's
//! `fuzz.run` (`docs/specs/97-fuzz-hardening.md`) uses these mutators in a running dev build.

/// Nesting depth the deep-nesting text mutation inserts: comfortably above `ron`'s default recursion
/// limit of 128, so a deep-nesting input must be rejected rather than overflow the stack.
pub const DEEP_NEST: usize = 256;

/// A small, seed-stable PRNG (SplitMix64) so fuzz runs are reproducible across machines and Rust
/// versions. It is intentionally not `rand`-crate dependent: fuzz tests must be deterministic.
#[derive(Debug, Clone)]
pub struct FuzzRng(u64);

impl FuzzRng {
    /// A PRNG from a fixed seed. A test that wants a *different* deterministic sequence picks a
    /// different seed; the same seed always reproduces the same corruptions.
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64-bit word (SplitMix64; the constants are the public-domain SplitMix64 stream).
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n` (uniform enough for fuzzing). `n == 0` has no value to pick: it returns `0`
    /// and draws nothing from the stream.
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }

    /// A value in `lo..=hi`. Panics in every profile if `hi < lo`, which would be a test bug, not a
    /// fuzz finding.
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        assert!(lo <= hi, "FuzzRng::range: hi ({hi}) < lo ({lo})");
        match (hi - lo).checked_add(1) {
            Some(span) => lo + self.below(span),
            // The full `usize` range: every word is a valid value.
            None => self.next_u64() as usize,
        }
    }

    /// `true` roughly one time in `denominator`. `chance(0)` is `false` (never) and draws nothing.
    pub fn chance(&mut self, denominator: usize) -> bool {
        denominator != 0 && self.below(denominator) == 0
    }

    /// A uniformly random byte.
    pub fn byte(&mut self) -> u8 {
        (self.next_u64() & 0xFF) as u8
    }
}

/// Corrupt a byte buffer with one randomly chosen strategy.
///
/// The strategies target the failure modes alpha testers hit: truncation, bit flips, spliced
/// chunks, overwritten length/version words (`u32`-wide), random inserts and long zero runs.
pub fn mutate_bytes(bytes: &[u8], rng: &mut FuzzRng) -> Vec<u8> {
    let mut out = bytes.to_vec();
    if out.is_empty() {
        out.push(rng.byte());
        return out;
    }
    match rng.below(8) {
        // Truncate to a random prefix (including zero bytes).
        0 => {
            let keep = rng.below(out.len());
            out.truncate(keep);
        }
        // Flip a random window of bits.
        1 => {
            let start = rng.below(out.len());
            let span = rng.range(1, out.len() - start);
            for b in &mut out[start..start + span] {
                *b ^= rng.byte();
            }
        }
        // Overwrite a single byte.
        2 => {
            let i = rng.below(out.len());
            out[i] = rng.byte();
        }
        // Insert random bytes at a random position.
        3 => {
            let i = rng.below(out.len());
            let n = rng.range(1, 24);
            let junk: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
            out.splice(i..i, junk);
        }
        // Overwrite a u32 word with a hostile value (huge lengths, off-by-one versions).
        4 => {
            if out.len() >= 4 {
                let i = rng.below(out.len() - 3);
                let hostile = [u32::MAX, 0, 1, 0x7FFF_FFFF, u32::MAX - 1][rng.below(5)];
                out[i..i + 4].copy_from_slice(&hostile.to_le_bytes());
            }
        }
        // Splice out a random chunk.
        5 => {
            let start = rng.below(out.len());
            let span = rng.range(1, out.len() - start);
            out.splice(start..start + span, std::iter::empty());
        }
        // Append random junk.
        6 => {
            let n = rng.range(1, 32);
            out.extend((0..n).map(|_| rng.byte()));
        }
        // Zero a random window (length/offset fields become 0).
        7 => {
            let start = rng.below(out.len());
            let span = rng.range(1, out.len() - start);
            for b in &mut out[start..start + span] {
                *b = 0;
            }
        }
        _ => unreachable!(),
    }
    out
}

/// Corrupt RON/JSON text with one randomly chosen strategy: NaN/inf/huge numbers, deep nesting,
/// truncation, garbage tokens, version-word rewrites and bare-token splicing. Every index is
/// snapped to a UTF-8 char boundary so the mutator itself cannot panic.
pub fn mutate_text(text: &str, rng: &mut FuzzRng) -> String {
    if text.is_empty() {
        return "(".into();
    }
    let mut out = text.to_string();
    match rng.below(7) {
        // Truncate to a random char-boundary prefix.
        0 => {
            let keep = out.floor_char_boundary(rng.below(out.len() + 1));
            out.truncate(keep);
        }
        // Replace a random digit run with a hostile float literal.
        1 => {
            let hostile = [
                "NaN",
                "inf",
                "-inf",
                "1e308",
                "-1e308",
                "99999999999999999999999999999999",
            ];
            if let Some((start, end)) = find_digit_run(&out) {
                out.replace_range(start..end, hostile[rng.below(hostile.len())]);
            } else {
                out.push_str(" NaN");
            }
        }
        // Rewrite a `version` / `format_version` number to a hostile value.
        2 => {
            let key = ["format_version:", "version:"]
                .into_iter()
                .find(|k| out.contains(*k));
            if let Some(key) = key
                && let Some(key_start) = out.find(key)
                && let Some(digit_off) = out[key_start + key.len()..].find(|c: char| c.is_ascii_digit())
            {
                let digits_start = key_start + key.len() + digit_off;
                let digits_end = out[digits_start..]
                    .find(|c: char| !c.is_ascii_digit())
                    .map(|off| digits_start + off)
                    .unwrap_or(out.len());
                let hostile = ["0", "2", "99", "4294967295"];
                out.replace_range(digits_start..digits_end, hostile[rng.below(hostile.len())]);
            }
        }
        // Insert deep nesting past ron's recursion limit (recursive deserialisers must return an
        // error, not overflow the stack).
        3 => {
            let i = out.floor_char_boundary(rng.below(out.len() + 1));
            out.insert_str(i, &"(".repeat(DEEP_NEST));
        }
        // Splice in a garbage token.
        4 => {
            let garbage = ["]", ")", "}", ":", ",", "\"", "Some(", "None", "nan"];
            let i = out.floor_char_boundary(rng.below(out.len() + 1));
            out.insert_str(i, garbage[rng.below(garbage.len())]);
        }
        // Duplicate a random substring.
        5 => {
            let start = out.floor_char_boundary(rng.below(out.len()));
            let end = out.floor_char_boundary(start + rng.range(1, out.len() - start));
            let dup = out[start..end].to_string();
            out.insert_str(start, &dup);
        }
        // Splice in an unusual Unicode scalar (parsers must handle non-ASCII without panicking).
        6 => {
            let scalars = ['💥', '\u{0}', '\u{FFFF}', 'ß', '界'];
            out.push(scalars[rng.below(scalars.len())]);
        }
        _ => unreachable!(),
    }
    out
}

/// `(start, end)` of the first maximal run of ASCII digits in `s`, or `None`.
fn find_digit_run(s: &str) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(u8::is_ascii_digit)?;
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    Some((start, end))
}

/// Hostile replacements for a number literal: integer edges of every width, values one past them, `f32`
/// overflow and subnormals, and the non-finite spellings RON accepts.
const HOSTILE_NUMBERS: &[&str] = &[
    "0",
    "-1",
    "-0.0",
    "65536",
    "2147483648",
    "4294967295",
    "4294967296",
    "-2147483649",
    "18446744073709551615",
    "18446744073709551616",
    "-9223372036854775809",
    "1e39",
    "-1e39",
    "3.4028235e38",
    "1e-45",
    "1e308",
    "NaN",
    "inf",
    "-inf",
];

/// Hostile replacements for a string literal: empty, path escapes, a long run, a NUL and wide characters.
const HOSTILE_STRINGS: &[&str] = &[
    "\"\"",
    "\"../../../../etc/passwd\"",
    "\"/absolute/path\"",
    "\"C:\\\\Windows\\\\x\"",
    "\"\\u{0}\"",
    "\"界💥\u{FFFF}\"",
    "\" \"",
    "\"::\"",
];

/// Most bytes one list amplification may add.
const MAX_AMPLIFY_BYTES: usize = 64 * 1024;

/// Byte spans of the literal tokens of RON or JSON text.
#[derive(Default)]
struct ValueTokens {
    numbers: Vec<(usize, usize)>,
    strings: Vec<(usize, usize)>,
    bools: Vec<(usize, usize)>,
    /// Inner spans of `[...]` lists (between the brackets).
    lists: Vec<(usize, usize)>,
}

/// Finds number, string, boolean and list tokens, skipping comments and digits inside identifiers and strings.
/// Every span starts and ends at an ASCII byte, so it is always on a char boundary.
fn scan_values(text: &str) -> ValueTokens {
    let b = text.as_bytes();
    let mut tokens = ValueTokens::default();
    let mut open: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == b'"' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            let end = (i + 1).min(b.len());
            tokens.strings.push((start, end));
            i = end;
        } else if c == b'/' && next == Some(b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && next == Some(b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if c.is_ascii_digit() || (c == b'-' && next.is_some_and(|n| n.is_ascii_digit())) {
            let start = i;
            i += 1;
            while i < b.len() {
                let d = b[i];
                let exponent_sign = (d == b'+' || d == b'-') && matches!(b[i - 1], b'e' | b'E');
                if d.is_ascii_digit() || matches!(d, b'.' | b'e' | b'E' | b'_') || exponent_sign {
                    i += 1;
                } else {
                    break;
                }
            }
            tokens.numbers.push((start, i));
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            if matches!(&text[start..i], "true" | "false") {
                tokens.bools.push((start, i));
            }
        } else {
            if c == b'[' {
                open.push(i + 1);
            } else if c == b']'
                && let Some(start) = open.pop()
            {
                tokens.lists.push((start, i));
            }
            i += 1;
        }
    }
    tokens
}

/// Corrupt RON or JSON text while keeping it well formed, so the input gets past the parser into validation
/// and the code that uses the values: a number becomes an integer edge, an `f32` overflow or a non-finite value;
/// a string becomes empty, a path escape or a long run; a boolean flips; a list is emptied or its elements
/// repeated (at most 64 KiB more). Falls back to [`mutate_text`] when the text has no token of the chosen kind.
pub fn mutate_text_values(text: &str, rng: &mut FuzzRng) -> String {
    let tokens = scan_values(text);
    let pick = |spans: &[(usize, usize)], rng: &mut FuzzRng| spans.get(rng.below(spans.len())).copied();
    let mut out = text.to_string();
    let applied = match rng.below(6) {
        0 | 1 => pick(&tokens.numbers, rng).map(|(s, e)| {
            out.replace_range(s..e, HOSTILE_NUMBERS[rng.below(HOSTILE_NUMBERS.len())]);
        }),
        2 => pick(&tokens.strings, rng).map(|(s, e)| {
            if rng.chance(4) {
                out.replace_range(s..e, &format!("\"{}\"", "a".repeat(4096)));
            } else {
                out.replace_range(s..e, HOSTILE_STRINGS[rng.below(HOSTILE_STRINGS.len())]);
            }
        }),
        3 => pick(&tokens.bools, rng).map(|(s, e)| {
            let flipped = if &out[s..e] == "true" { "false" } else { "true" };
            out.replace_range(s..e, flipped);
        }),
        4 => pick(&tokens.lists, rng).map(|(s, e)| out.replace_range(s..e, "")),
        _ => pick(&tokens.lists, rng).map(|(s, e)| {
            let inner = out[s..e].trim_end().trim_end_matches(',').to_string();
            if !inner.trim().is_empty() {
                let copies = rng.range(1, 63).min(MAX_AMPLIFY_BYTES / (inner.len() + 1));
                let mut grown = inner.clone();
                for _ in 0..copies {
                    grown.push(',');
                    grown.push_str(&inner);
                }
                out.replace_range(s..e, &grown);
            }
        }),
    };
    match applied {
        Some(()) => out,
        None => mutate_text(text, rng),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_zero_and_chance_zero_draw_nothing() {
        let mut rng = FuzzRng::new(5);
        let mut twin = FuzzRng::new(5);
        assert_eq!(rng.below(0), 0);
        assert!(!rng.chance(0));
        assert_eq!(rng.next_u64(), twin.next_u64(), "the stream did not advance");
    }

    #[test]
    #[should_panic(expected = "FuzzRng::range")]
    fn swapped_range_bounds_panic() {
        let _ = FuzzRng::new(1).range(5, 4);
    }

    #[test]
    fn range_stays_inside_and_handles_the_full_span() {
        let mut rng = FuzzRng::new(9);
        for _ in 0..64 {
            assert!((3..=6).contains(&rng.range(3, 6)));
        }
        assert_eq!(rng.range(7, 7), 7);
        let _ = rng.range(0, usize::MAX);
    }

    #[test]
    fn value_tokens_skip_identifiers_strings_and_comments() {
        let text = "(frac_0_lod1: 2, s: \"a1 [3]\", // 9\n l: [1.5e-3, -4], ok: true /* 7 */)";
        let t = scan_values(text);
        let numbers: Vec<&str> = t.numbers.iter().map(|&(s, e)| &text[s..e]).collect();
        assert_eq!(numbers, ["2", "1.5e-3", "-4"]);
        assert_eq!(t.strings.len(), 1);
        assert_eq!(t.bools.len(), 1);
        let lists: Vec<&str> = t.lists.iter().map(|&(s, e)| &text[s..e]).collect();
        assert_eq!(lists, ["1.5e-3, -4"]);
    }

    #[test]
    fn value_mutations_are_deterministic_and_bounded() {
        let text = "(a: 1, b: [ (x: 2.0), (x: 3.0), ], c: \"name\", d: false)";
        let mut a = FuzzRng::new(5);
        let mut b = FuzzRng::new(5);
        for _ in 0..500 {
            let x = mutate_text_values(text, &mut a);
            assert_eq!(x, mutate_text_values(text, &mut b));
            assert!(
                x.len() <= text.len() + MAX_AMPLIFY_BYTES + 4096 + DEEP_NEST + 64,
                "{}",
                x.len()
            );
        }
    }

    #[test]
    fn list_amplification_keeps_the_text_well_formed() {
        let text = "[1, 2, ]";
        let mut rng = FuzzRng::new(0);
        let mut saw = false;
        for _ in 0..200 {
            let out = mutate_text_values(text, &mut rng);
            if out.matches(',').count() > 2 && !out.contains("NaN") {
                saw = true;
                assert!(!out.contains(",,"), "{out}");
            }
        }
        assert!(saw, "amplification never ran");
    }
}
