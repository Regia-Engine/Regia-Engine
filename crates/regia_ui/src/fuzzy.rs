// SPDX-License-Identifier: MPL-2.0
//! Fuzzy search for palettes and pickers (ADR-273 W17, ADR-1842): subsequence matching with a
//! score that favors prefixes, word starts and runs of consecutive characters.
//!
//! [`fuzzy_match`] scores one candidate and reports which characters matched (for highlighting);
//! [`fuzzy_rank`] orders a list. Both are pure and deterministic: equal scores keep list order.
//! Matching is case-insensitive over Unicode scalar values (simple lowercase folding), and every
//! query character must appear in order. An empty query matches everything with score 0.
//!
//! The agent surface is `ui.fuzzy` (`book/agents/ui-fuzzy.md`).

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

#[cfg(test)]
use std::cell::Cell;

// Cells combined while scoring one rank. Test-only: the regression test checks this stays
// linear in the candidate length, which the per-column rescan of every earlier column is not.
#[cfg(test)]
thread_local! {
    static SCORED_CELLS: Cell<u64> = const { Cell::new(0) };
}

#[cfg(test)]
fn reset_scored_cells() {
    SCORED_CELLS.with(|c| c.set(0));
}

#[cfg(test)]
fn scored_cells() -> u64 {
    SCORED_CELLS.with(|c| c.get())
}

/// Counts one combined cell. Empty outside tests so a release rank does not touch thread-local state.
#[inline]
fn note_scored_cell() {
    #[cfg(test)]
    SCORED_CELLS.with(|c| c.set(c.get().saturating_add(1)));
}

/// Score for each matched character.
const MATCH: i32 = 16;
/// Extra score when the match directly follows the previous matched character.
const CONSECUTIVE: i32 = 24;
/// Extra score for a match at a word start (after a separator, or a lower-to-upper camel step).
const WORD_START: i32 = 20;
/// Extra score when the first query character matches the first candidate character.
const PREFIX: i32 = 28;
/// Penalty per skipped candidate character before the first match (capped).
const LEADING_GAP: i32 = 2;
/// Cap on the leading-gap penalty.
const LEADING_GAP_CAP: i32 = 12;
/// Penalty per skipped character between two matches.
const GAP: i32 = 1;
/// Bonus when the query, with whitespace removed, has the same number of characters as the candidate.
const EXACT: i32 = 64;

/// One scored candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    /// Index of the candidate in the input list.
    pub index: usize,
    /// Higher is better.
    pub score: i32,
    /// Character positions (not bytes) in the candidate that matched the query, ascending.
    pub positions: Vec<usize>,
}

fn is_separator(c: char) -> bool {
    !c.is_alphanumeric()
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Reused tables for one calling thread. A rank of thousands of labels must not allocate a fresh
/// jagged matrix per label. Grown to the largest label seen on this thread and then kept.
#[derive(Default)]
struct Scratch {
    q: Vec<char>,
    cand: Vec<char>,
    folded: Vec<char>,
    boundary: Vec<bool>,
    /// Scores for the previous query character, one per candidate column. `i32::MIN` is empty.
    prev: Vec<i32>,
    curr: Vec<i32>,
    /// Predecessor column of query row `j` at candidate column `i`, packed `j * n + i`.
    from: Vec<u32>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

fn with_scratch<R>(body: impl FnOnce(&mut Scratch) -> R) -> R {
    SCRATCH.with(|slot| {
        if let Ok(mut guard) = slot.try_borrow_mut() {
            body(&mut guard)
        } else {
            // A re-entrant call (none today) gets its own tables instead of aliasing the outer ones.
            body(&mut Scratch::default())
        }
    })
}

/// Scores `candidate` against `query`, or `None` when the query is not a subsequence of it.
///
/// Among the possible alignments the best-scoring one is returned. The recurrence is the standard
/// best-predecessor score over query and candidate positions (`"fb"` in `"foo_bar"` picks the word
/// start of `bar`). Each later query character is one pass over the candidate: the best earlier
/// column is a running maximum, so the work is linear in the candidate length, not cubic.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    with_scratch(|scratch| align(scratch, query, candidate, true))
}

fn score_candidate(query: &str, candidate: &str) -> Option<i32> {
    with_scratch(|scratch| align(scratch, query, candidate, false)).map(|(score, _)| score)
}

fn opening_score(i: usize, at_boundary: bool) -> i32 {
    let mut s = MATCH;
    if at_boundary {
        s += WORD_START;
    }
    if i == 0 {
        s += PREFIX;
    }
    let lead = match i32::try_from(i) {
        Ok(i) => (i * LEADING_GAP).min(LEADING_GAP_CAP),
        // The penalty is capped, so a huge index still pays only the cap.
        Err(_) => LEADING_GAP_CAP,
    };
    s - lead
}

fn matched_score(base: i64, at_boundary: bool) -> Option<i32> {
    let mut s = base + i64::from(MATCH);
    if at_boundary {
        s += i64::from(WORD_START);
    }
    match i32::try_from(s) {
        Ok(score) if score != i32::MIN => Some(score),
        _ => None,
    }
}

fn align(
    scratch: &mut Scratch,
    query: &str,
    candidate: &str,
    record_path: bool,
) -> Option<(i32, Vec<usize>)> {
    scratch.q.clear();
    scratch
        .q
        .extend(query.chars().filter(|c| !c.is_whitespace()).map(fold));
    let m = scratch.q.len();
    if m == 0 {
        return Some((0, Vec::new()));
    }
    scratch.cand.clear();
    scratch.cand.extend(candidate.chars());
    let n = scratch.cand.len();
    if m > n {
        return None;
    }
    // Predecessor indexes are `u32`. A label past that length is not a search string.
    if u32::try_from(n).is_err() {
        return None;
    }
    scratch.folded.clear();
    scratch.folded.extend(scratch.cand.iter().copied().map(fold));
    scratch.boundary.clear();
    scratch.boundary.reserve(n);
    for i in 0..n {
        let at_boundary = i == 0
            || is_separator(scratch.cand[i - 1])
            || (scratch.cand[i - 1].is_lowercase() && scratch.cand[i].is_uppercase());
        scratch.boundary.push(at_boundary);
    }
    scratch.prev.clear();
    scratch.prev.resize(n, i32::MIN);
    for i in 0..n {
        if scratch.folded[i] == scratch.q[0] {
            scratch.prev[i] = opening_score(i, scratch.boundary[i]);
        }
    }
    if record_path {
        let cells = m.checked_mul(n)?;
        scratch.from.clear();
        scratch.from.resize(cells, u32::MAX);
    }
    for j in 1..m {
        scratch.curr.clear();
        scratch.curr.resize(n, i32::MIN);
        // Best `prev[p] + GAP * (p + 1)` for p <= i - 2. That is the best non-adjacent predecessor:
        // `prev[p] - GAP * (i - p - 1) = key - GAP * i`, and an earlier p wins a tie.
        let mut running_key = i64::MIN;
        let mut running_from = u32::MAX;
        for i in j..n {
            note_scored_cell();
            if scratch.folded[i] == scratch.q[j] {
                let mut top = i64::MIN;
                let mut top_from = u32::MAX;
                if running_from != u32::MAX {
                    let non_adj = running_key - i64::from(GAP) * (i as i64);
                    if non_adj > top {
                        top = non_adj;
                        top_from = running_from;
                    }
                }
                let adjacent = scratch.prev[i - 1];
                if adjacent != i32::MIN {
                    let adj = i64::from(adjacent) + i64::from(CONSECUTIVE);
                    // Strict: an equal non-adjacent predecessor is earlier and keeps the tie.
                    if adj > top {
                        top = adj;
                        top_from = (i as u32) - 1;
                    }
                }
                if top != i64::MIN
                    && let Some(score) = matched_score(top, scratch.boundary[i])
                {
                    scratch.curr[i] = score;
                    if record_path {
                        scratch.from[j * n + i] = top_from;
                    }
                }
            }
            let prev = scratch.prev[i - 1];
            if prev != i32::MIN {
                let key = i64::from(prev) + i64::from(GAP) * (i as i64);
                if key > running_key {
                    running_key = key;
                    running_from = (i as u32) - 1;
                }
            }
        }
        std::mem::swap(&mut scratch.prev, &mut scratch.curr);
    }
    let mut end = usize::MAX;
    let mut score = i32::MIN;
    for (i, &cell) in scratch.prev.iter().enumerate() {
        if cell > score {
            score = cell;
            end = i;
        }
    }
    if end == usize::MAX {
        return None;
    }
    if m == n {
        score = score.saturating_add(EXACT);
    }
    let positions = if !record_path {
        Vec::new()
    } else if m == 1 {
        vec![end]
    } else {
        reconstruct(&scratch.from, m, n, end)?
    };
    Some((score, positions))
}

fn reconstruct(from: &[u32], m: usize, n: usize, end: usize) -> Option<Vec<usize>> {
    let mut positions = vec![0; m];
    let mut at = end;
    for j in (0..m).rev() {
        if at >= n {
            return None;
        }
        positions[j] = at;
        if j == 0 {
            break;
        }
        let pred = *from.get(j * n + at)?;
        if pred == u32::MAX {
            return None;
        }
        let pred = pred as usize;
        if pred >= at {
            return None;
        }
        at = pred;
    }
    Some(positions)
}

/// Ranks `candidates` against `query`: best first, ties in input order, non-matches dropped.
/// At most `limit` results (`0` means no limit).
///
/// Every candidate is scored. A positive `limit` keeps a heap of that many rows and orders only
/// those, instead of sorting every match. One call can exceed a tenth of a millisecond on a long
/// list, so the span is `ui_fuzzy_rank`.
pub fn fuzzy_rank<S: AsRef<str>>(query: &str, candidates: &[S], limit: usize) -> Vec<FuzzyMatch> {
    let _span = tracing::debug_span!("ui_fuzzy_rank", candidates = candidates.len(), limit).entered();
    if limit == 0 {
        let mut out: Vec<FuzzyMatch> = candidates
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                fuzzy_match(query, candidate.as_ref()).map(|(score, positions)| FuzzyMatch {
                    index,
                    score,
                    positions,
                })
            })
            .collect();
        out.sort_by(|a, b| b.score.cmp(&a.score).then(a.index.cmp(&b.index)));
        return out;
    }
    // Worst of the kept rows is the greatest: lowest score, then the later index.
    let mut heap: BinaryHeap<(Reverse<i32>, usize)> = BinaryHeap::with_capacity(limit.min(candidates.len()));
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(score) = score_candidate(query, candidate.as_ref()) else {
            continue;
        };
        if heap.len() < limit {
            heap.push((Reverse(score), index));
        } else if let Some((Reverse(worst), _)) = heap.peek()
            && score > *worst
        {
            heap.pop();
            heap.push((Reverse(score), index));
        }
    }
    let mut out: Vec<FuzzyMatch> = heap
        .into_iter()
        .map(|(Reverse(score), index)| {
            let positions = fuzzy_match(query, candidates[index].as_ref())
                .map_or_else(Vec::new, |(_, positions)| positions);
            FuzzyMatch {
                index,
                score,
                positions,
            }
        })
        .collect();
    out.sort_by(|a, b| b.score.cmp(&a.score).then(a.index.cmp(&b.index)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_matches_everything_in_order() {
        let r = fuzzy_rank("", &["b", "a"], 0);
        assert_eq!(r.iter().map(|m| m.index).collect::<Vec<_>>(), [0, 1]);
        assert!(r.iter().all(|m| m.score == 0 && m.positions.is_empty()));
    }

    #[test]
    fn subsequence_required_and_case_folded() {
        assert!(fuzzy_match("xyz", "hello").is_none());
        assert!(fuzzy_match("lh", "hello").is_none(), "order matters");
        assert!(fuzzy_match("HEL", "hello").is_some());
        assert!(fuzzy_match("toolong", "to").is_none());
    }

    #[test]
    fn prefix_and_exact_outrank_scattered() {
        let items = ["open file", "reopen", "Open", "o p e n"];
        let r = fuzzy_rank("open", &items, 0);
        assert_eq!(r[0].index, 2, "exact wins");
        assert_eq!(r[1].index, 0, "prefix next");
        assert!(r.iter().position(|m| m.index == 1).unwrap() > 1);
    }

    #[test]
    fn word_starts_beat_mid_word_matches() {
        let r = fuzzy_rank("fb", &["fabric", "foo_bar", "xfxb"], 0);
        assert_eq!(r[0].index, 1);
        let (_, pos) = fuzzy_match("fb", "foo_bar").unwrap();
        assert_eq!(pos, [0, 4], "highlight the f and the b of bar");
    }

    #[test]
    fn camel_case_steps_count_as_word_starts() {
        let (_, pos) = fuzzy_match("sp", "saveProject").unwrap();
        assert_eq!(pos, [0, 4]);
    }

    #[test]
    fn ties_keep_input_order_and_limit_applies() {
        let r = fuzzy_rank("a", &["a", "a", "a"], 2);
        assert_eq!(r.iter().map(|m| m.index).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn unicode_positions_are_characters() {
        let (_, pos) = fuzzy_match("ü", "Grüße").unwrap();
        assert_eq!(pos, [2]);
    }

    #[test]
    fn deterministic() {
        let items = ["Edit: Undo", "Edit: Redo", "File: Open", "File: Open Recent"];
        assert_eq!(fuzzy_rank("fo", &items, 0), fuzzy_rank("fo", &items, 0));
    }

    /// The alignment the ranker must keep: best predecessor by the same bonuses, earliest on a tie.
    /// Independent of [`fuzzy_match`], so a faster table cannot drift from the locked order.
    fn reference_match(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
        let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).map(fold).collect();
        let cand: Vec<char> = candidate.chars().collect();
        if q.is_empty() {
            return Some((0, Vec::new()));
        }
        if q.len() > cand.len() {
            return None;
        }
        let folded: Vec<char> = cand.iter().copied().map(fold).collect();
        let boundary: Vec<bool> = (0..cand.len())
            .map(|i| {
                i == 0 || is_separator(cand[i - 1]) || (cand[i - 1].is_lowercase() && cand[i].is_uppercase())
            })
            .collect();
        let n = cand.len();
        let m = q.len();
        let mut best = vec![vec![i32::MIN; n]; m];
        let mut from = vec![vec![usize::MAX; n]; m];
        for i in 0..n {
            if folded[i] != q[0] {
                continue;
            }
            let mut s = MATCH;
            if boundary[i] {
                s += WORD_START;
            }
            if i == 0 {
                s += PREFIX;
            }
            s -= (i as i32 * LEADING_GAP).min(LEADING_GAP_CAP);
            best[0][i] = s;
        }
        for j in 1..m {
            for i in j..n {
                if folded[i] != q[j] {
                    continue;
                }
                let mut top = i32::MIN;
                let mut top_from = usize::MAX;
                for (p, &prev) in best[j - 1].iter().enumerate().take(i).skip(j - 1) {
                    if prev == i32::MIN {
                        continue;
                    }
                    let step = if p + 1 == i {
                        CONSECUTIVE
                    } else {
                        -(GAP * (i - p - 1) as i32)
                    };
                    if prev + step > top {
                        top = prev + step;
                        top_from = p;
                    }
                }
                if top == i32::MIN {
                    continue;
                }
                let mut s = top + MATCH;
                if boundary[i] {
                    s += WORD_START;
                }
                best[j][i] = s;
                from[j][i] = top_from;
            }
        }
        let mut end = usize::MAX;
        let mut score = i32::MIN;
        for (i, &b) in best[m - 1].iter().enumerate().take(n) {
            if b > score {
                score = b;
                end = i;
            }
        }
        if end == usize::MAX {
            return None;
        }
        let mut positions = vec![0; m];
        let mut at = end;
        for j in (0..m).rev() {
            positions[j] = at;
            if j > 0 {
                at = from[j][at];
            }
        }
        if m == n {
            score += EXACT;
        }
        Some((score, positions))
    }

    #[test]
    fn fuzzy_match_agrees_with_the_full_alignment() {
        let long_a = "a".repeat(32);
        let pairs = [
            ("", "hello"),
            ("   ", "hello"),
            ("xyz", "hello"),
            ("lh", "hello"),
            ("HEL", "hello"),
            ("toolong", "to"),
            ("open", "open file"),
            ("open", "reopen"),
            ("open", "Open"),
            ("open", "o p e n"),
            ("fb", "fabric"),
            ("fb", "foo_bar"),
            ("fb", "xfxb"),
            ("sp", "saveProject"),
            ("ü", "Grüße"),
            ("aa", long_a.as_str()),
            ("fo", "File: Open Recent"),
            ("sw", "switch_000"),
            ("ab", "ab"),
            ("Open", "open"),
            ("ab", "a_b"),
            ("Qs", "QueryString"),
            ("git", "Git: Status"),
        ];
        for (query, candidate) in pairs {
            assert_eq!(
                fuzzy_match(query, candidate),
                reference_match(query, candidate),
                "{query:?} in {candidate:?}"
            );
        }
    }

    #[test]
    fn fuzzy_rank_limit_is_the_top_of_the_full_order() {
        let mut items: Vec<String> = (0..64).map(|i| format!("item_{i:04}_extra")).collect();
        items[2] = "Open".into();
        items[7] = "foo_bar".into();
        items[9] = "saveProject".into();
        let full = fuzzy_rank("e", &items, 0);
        let top = fuzzy_rank("e", &items, 20);
        assert!(full.len() > 20, "the fixture must truncate");
        assert_eq!(top, full[..20]);
        assert_eq!(fuzzy_rank("a", &["a", "a", "a"], 2).len(), 2);
    }

    #[test]
    fn fuzzy_rank_limit_scores_linear_cells() {
        const NAMES: usize = 2_000;
        const NAME_CHARS: usize = 32;
        // One later query character visits `NAME_CHARS - 1` columns. A second pass over the
        // 20 kept rows is the most a bounded heap adds. The cubic rescan is far above that.
        let columns = (NAME_CHARS - 1) as u64;
        let scoring = NAMES as u64 * columns;
        let budget = scoring + 20 * columns;
        let names: Vec<String> = (0..NAMES).map(|_| "a".repeat(NAME_CHARS)).collect();
        reset_scored_cells();
        let top = fuzzy_rank("aa", &names, 20);
        let cells = scored_cells();
        assert_eq!(
            top.iter().map(|m| m.index).collect::<Vec<_>>(),
            (0..20).collect::<Vec<_>>()
        );
        assert_eq!(top[0].positions, vec![0, 1]);
        assert!(
            cells >= scoring,
            "scored {cells} cells, expected at least {scoring}"
        );
        assert!(
            cells <= budget,
            "scored {cells} cells; a linear pass of {NAMES} names × {NAME_CHARS} chars is at most {budget}, not names × query chars × name chars²"
        );
    }
}
