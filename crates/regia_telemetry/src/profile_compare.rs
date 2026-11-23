// SPDX-License-Identifier: MPL-2.0
//! Comparing two profile captures (`docs/specs/83-prefab-profiler-volumes.md` § 4.4, ADR-9740):
//! per span, the inclusive and self time of capture `a` and capture `b` and their difference
//! (`b - a`), sorted by the size of the change.
//!
//! Spans are matched by their path from the root (`frame;render;shadows`), so a span with the
//! same name under two parents is two rows. A span in only one capture is a row with zero on the
//! other side. Every number is rounded to the microsecond and the order is total (size of the
//! inclusive change, then of the self change, then the path), so one pair of captures always gives
//! the same rows and the same JSON bytes. The editor's Profiler panel colors its flame view by
//! these deltas; the agent twin is `profiler.compare` (`book/agents/profiler.md`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::profile_capture::{CaptureNode, ProfileCapture};

/// Which captures a span was found in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanPresence {
    /// In both captures.
    Both,
    /// Only in `a` (the span went away).
    OnlyA,
    /// Only in `b` (the span is new).
    OnlyB,
}

/// One span's change between two captures.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpanDelta {
    /// The span names from the root to this span.
    pub path: Vec<String>,
    /// Mean inclusive time in `a`, ms (0 when absent).
    pub inclusive_a: f64,
    /// Mean inclusive time in `b`, ms (0 when absent).
    pub inclusive_b: f64,
    /// `inclusive_b - inclusive_a`, ms.
    pub inclusive_delta: f64,
    /// Mean self time in `a`, ms.
    pub self_a: f64,
    /// Mean self time in `b`, ms.
    pub self_b: f64,
    /// `self_b - self_a`, ms.
    pub self_delta: f64,
    /// Mean calls per frame in `a`.
    pub calls_a: f64,
    /// Mean calls per frame in `b`.
    pub calls_b: f64,
    /// Which captures hold the span.
    pub presence: SpanPresence,
}

impl SpanDelta {
    /// The span's own name (the last path element).
    #[must_use]
    pub fn name(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }

    /// The path joined with `;` (the folded-stack key).
    #[must_use]
    pub fn key(&self) -> String {
        self.path.join(";")
    }
}

/// One GPU pass's change between two captures.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GpuPassDelta {
    /// The pass name.
    pub pass: String,
    /// Milliseconds in `a` (0 when absent).
    pub ms_a: f64,
    /// Milliseconds in `b` (0 when absent).
    pub ms_b: f64,
    /// `ms_b - ms_a`.
    pub delta: f64,
}

/// The comparison of capture `a` (the baseline) with capture `b`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureDiff {
    /// Mean frame time of `a`, ms.
    pub frame_ms_a: f64,
    /// Mean frame time of `b`, ms.
    pub frame_ms_b: f64,
    /// `frame_ms_b - frame_ms_a`.
    pub frame_ms_delta: f64,
    /// Every span of either capture, largest change first.
    pub spans: Vec<SpanDelta>,
    /// Every GPU pass of either capture, largest change first.
    pub gpu: Vec<GpuPassDelta>,
}

impl CaptureDiff {
    /// The span at `key` (the `;`-joined path).
    #[must_use]
    pub fn span(&self, key: &str) -> Option<&SpanDelta> {
        self.spans.iter().find(|s| s.key() == key)
    }

    /// The largest absolute inclusive change, ms (0 for an empty diff); the flame view scales its
    /// colors by it.
    #[must_use]
    pub fn max_abs_inclusive_delta(&self) -> f64 {
        self.spans
            .iter()
            .map(|s| s.inclusive_delta.abs())
            .fold(0.0, f64::max)
    }

    /// The pretty JSON text (byte-identical for equal diffs).
    ///
    /// # Errors
    /// Serialization failed (not expected for finite numbers).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        Ok(text)
    }
}

/// Rounds to the microsecond and folds `-0.0` into `0.0`, so equal data prints equal text.
fn us(ms: f64) -> f64 {
    let rounded = (ms * 1000.0).round() / 1000.0;
    if rounded == 0.0 || !rounded.is_finite() {
        0.0
    } else {
        rounded
    }
}

/// Totals per path: (inclusive, self, calls). A path twice in one tree (not produced by the
/// profiler, possible in a hand-edited file) adds up.
type Totals = BTreeMap<Vec<String>, (f64, f64, f64)>;

fn collect(nodes: &[CaptureNode], prefix: &mut Vec<String>, out: &mut Totals) {
    for node in nodes {
        prefix.push(node.name.clone());
        let entry = out.entry(prefix.clone()).or_insert((0.0, 0.0, 0.0));
        entry.0 += node.inclusive_ms;
        entry.1 += node.self_ms;
        entry.2 += node.calls;
        collect(&node.children, prefix, out);
        prefix.pop();
    }
}

/// Compares capture `a` (the baseline) with capture `b`: per span (matched by path) the
/// inclusive and self times of both and `b - a`, sorted by the absolute inclusive change, then the
/// absolute self change, then the path; GPU passes likewise by name. A pure function: the same
/// captures give the same diff.
#[must_use]
pub fn compare(a: &ProfileCapture, b: &ProfileCapture) -> CaptureDiff {
    let (mut in_a, mut in_b) = (Totals::new(), Totals::new());
    collect(&a.cpu, &mut Vec::new(), &mut in_a);
    collect(&b.cpu, &mut Vec::new(), &mut in_b);
    let mut paths: Vec<&Vec<String>> = in_a.keys().chain(in_b.keys()).collect();
    paths.sort();
    paths.dedup();
    let mut spans: Vec<SpanDelta> = paths
        .into_iter()
        .map(|path| {
            let (ia, sa, ca) = in_a.get(path).copied().unwrap_or_default();
            let (ib, sb, cb) = in_b.get(path).copied().unwrap_or_default();
            let presence = match (in_a.contains_key(path), in_b.contains_key(path)) {
                (true, false) => SpanPresence::OnlyA,
                (false, true) => SpanPresence::OnlyB,
                _ => SpanPresence::Both,
            };
            SpanDelta {
                path: path.clone(),
                inclusive_a: us(ia),
                inclusive_b: us(ib),
                inclusive_delta: us(ib - ia),
                self_a: us(sa),
                self_b: us(sb),
                self_delta: us(sb - sa),
                calls_a: us(ca),
                calls_b: us(cb),
                presence,
            }
        })
        .collect();
    spans.sort_by(|x, y| {
        y.inclusive_delta
            .abs()
            .total_cmp(&x.inclusive_delta.abs())
            .then_with(|| y.self_delta.abs().total_cmp(&x.self_delta.abs()))
            .then_with(|| x.path.cmp(&y.path))
    });
    let mut passes: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
    for pass in &a.gpu {
        passes.entry(pass.pass.as_str()).or_default().0 += pass.ms;
    }
    for pass in &b.gpu {
        passes.entry(pass.pass.as_str()).or_default().1 += pass.ms;
    }
    let mut gpu: Vec<GpuPassDelta> = passes
        .into_iter()
        .map(|(pass, (ma, mb))| GpuPassDelta {
            pass: pass.to_owned(),
            ms_a: us(ma),
            ms_b: us(mb),
            delta: us(mb - ma),
        })
        .collect();
    gpu.sort_by(|x, y| {
        y.delta
            .abs()
            .total_cmp(&x.delta.abs())
            .then_with(|| x.pass.cmp(&y.pass))
    });
    CaptureDiff {
        frame_ms_a: us(a.frame_ms),
        frame_ms_b: us(b.frame_ms),
        frame_ms_delta: us(b.frame_ms - a.frame_ms),
        spans,
        gpu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_capture::GpuPassTime;

    fn n(name: &str, inclusive: f64, self_ms: f64, children: Vec<CaptureNode>) -> CaptureNode {
        CaptureNode {
            name: name.into(),
            inclusive_ms: inclusive,
            self_ms,
            calls: 1.0,
            children,
        }
    }

    fn capture(frame_ms: f64, cpu: Vec<CaptureNode>, gpu: &[(&str, f64)]) -> ProfileCapture {
        ProfileCapture {
            format_version: 1,
            frames: 60,
            frame_ms,
            cpu,
            truncated: 0,
            gpu: gpu
                .iter()
                .map(|(pass, ms)| GpuPassTime {
                    pass: (*pass).into(),
                    ms: *ms,
                })
                .collect(),
            folded: String::new(),
            timeline: Vec::new(),
            timeline_truncated: 0,
        }
    }

    /// Fixture `a`: frame 10 ms; render 6 (self 1) with shadows 2 and forward 3; update 2.
    fn fixture_a() -> ProfileCapture {
        capture(
            10.0,
            vec![
                n(
                    "render",
                    6.0,
                    1.0,
                    vec![n("forward", 3.0, 3.0, vec![]), n("shadows", 2.0, 2.0, vec![])],
                ),
                n("update", 2.0, 2.0, vec![]),
            ],
            &[("forward", 1.5), ("shadows", 0.75)],
        )
    }

    /// Fixture `b`: shadows doubled (4 ms), forward unchanged, update gone, a new `physics`
    /// 0.5 ms, render's self 0.9; GPU shadows 1.0 and a new ssao pass.
    fn fixture_b() -> ProfileCapture {
        capture(
            11.4,
            vec![
                n(
                    "render",
                    7.9,
                    0.9,
                    vec![n("shadows", 4.0, 4.0, vec![]), n("forward", 3.0, 3.0, vec![])],
                ),
                n("physics", 0.5, 0.5, vec![]),
            ],
            &[("forward", 1.5), ("shadows", 1.0), ("ssao", 0.25)],
        )
    }

    #[test]
    fn fixture_captures_give_the_expected_deltas_in_order() {
        let diff = compare(&fixture_a(), &fixture_b());
        assert_eq!(diff.frame_ms_delta, 1.4);
        let rows: Vec<(String, f64, f64, SpanPresence)> = diff
            .spans
            .iter()
            .map(|s| (s.key(), s.inclusive_delta, s.self_delta, s.presence))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("render;shadows".to_owned(), 2.0, 2.0, SpanPresence::Both),
                ("update".to_owned(), -2.0, -2.0, SpanPresence::OnlyA),
                ("render".to_owned(), 1.9, -0.1, SpanPresence::Both),
                ("physics".to_owned(), 0.5, 0.5, SpanPresence::OnlyB),
                ("render;forward".to_owned(), 0.0, 0.0, SpanPresence::Both),
            ]
        );
        let update = diff.span("update").expect("update row");
        assert_eq!((update.inclusive_a, update.inclusive_b), (2.0, 0.0));
        assert_eq!(update.name(), "update");
        let gpu: Vec<(&str, f64)> = diff.gpu.iter().map(|g| (g.pass.as_str(), g.delta)).collect();
        // Equal changes order by pass name.
        assert_eq!(gpu, vec![("shadows", 0.25), ("ssao", 0.25), ("forward", 0.0)]);
        assert_eq!(diff.max_abs_inclusive_delta(), 2.0);
    }

    #[test]
    fn a_comparison_is_byte_identical_across_runs_and_round_trips() {
        let one = compare(&fixture_a(), &fixture_b()).to_json().expect("json");
        let two = compare(&fixture_a(), &fixture_b()).to_json().expect("json");
        assert_eq!(one, two, "two runs give the same bytes");
        let back: CaptureDiff = serde_json::from_str(&one).expect("parse");
        assert_eq!(back.to_json().expect("again"), one);
        // Sibling order in the file does not matter: the paths match, not the positions.
        let mut shuffled = fixture_b();
        shuffled.cpu.reverse();
        shuffled.cpu[1].children.reverse();
        assert_eq!(compare(&fixture_a(), &shuffled).to_json().expect("json"), one);
    }

    #[test]
    fn a_capture_against_itself_changes_nothing_and_rounding_hides_float_noise() {
        let diff = compare(&fixture_a(), &fixture_a());
        assert!(
            diff.spans
                .iter()
                .all(|s| s.inclusive_delta == 0.0 && s.self_delta == 0.0 && s.presence == SpanPresence::Both)
        );
        // 0.1 + 0.2 - 0.3 is not 0 in f64; the microsecond rounding makes it 0 (and never -0).
        let a = capture(0.3, vec![n("x", 0.3, 0.3, vec![])], &[]);
        let b = capture(0.1 + 0.2, vec![n("x", 0.1 + 0.2, 0.1 + 0.2, vec![])], &[]);
        let diff = compare(&a, &b);
        assert_eq!(diff.spans[0].inclusive_delta.to_bits(), 0.0_f64.to_bits());
        assert_eq!(diff.frame_ms_delta.to_bits(), 0.0_f64.to_bits());
        let empty = compare(&capture(0.0, vec![], &[]), &capture(0.0, vec![], &[]));
        assert!(empty.spans.is_empty() && empty.gpu.is_empty());
        assert_eq!(empty.max_abs_inclusive_delta(), 0.0);
    }

    #[test]
    fn the_same_name_under_two_parents_is_two_rows() {
        let a = capture(
            1.0,
            vec![
                n("a", 1.0, 0.0, vec![n("sort", 1.0, 1.0, vec![])]),
                n("b", 1.0, 0.0, vec![n("sort", 1.0, 1.0, vec![])]),
            ],
            &[],
        );
        let mut b = a.clone();
        b.cpu[1].children[0].inclusive_ms = 3.0;
        let diff = compare(&a, &b);
        assert_eq!(diff.span("b;sort").map(|s| s.inclusive_delta), Some(2.0));
        assert_eq!(diff.span("a;sort").map(|s| s.inclusive_delta), Some(0.0));
    }
}
