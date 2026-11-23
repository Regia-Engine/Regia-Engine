// SPDX-License-Identifier: MPL-2.0
//! Flame-graph capture and rendering (`docs/specs/10-core.md` § 1.3).
//!
//! [`FrameProfiler`](super::FrameProfiler) keeps per-span-name totals for overlays, which loses the
//! call hierarchy. When flame capture is enabled
//! ([`FrameProfiler::set_flame_enabled`](super::FrameProfiler::set_flame_enabled)) it also records
//! each span exit with its call path, and [`FrameProfiler::flame_graph`](super::FrameProfiler::flame_graph)
//! folds those over a window of frames into a [`FlameGraph`]: the engine's span call tree, with
//! inclusive and self time, the mean calls per frame, a folded-stack listing and a self-contained SVG.
//! This is the data behind the editor's flame view and "capture to file".
//!
//! The tree covers the spans the profiler times by name — the engine's own `scope!` spans and host
//! sections — not `bevy_ecs` system spans (those are timed per system and reported as a flat table; see
//! [`super::RunTotals`]). System and schedule spans are skipped in a span's path, so a span with no timed
//! ancestor appears as a root.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// SVG width in pixels (the flame graph's total-inclusive-time baseline).
const SVG_WIDTH: f64 = 1200.0;
/// Row height in pixels.
const SVG_ROW: f64 = 18.0;
/// Minimum bar width in pixels, so a tiny span is still visible.
const SVG_MIN_W: f64 = 1.0;
/// Bar width below which the label is omitted.
const SVG_TEXT_MIN_W: f64 = 24.0;

/// One node of an aggregated flame graph: a span name, its cost and its children.
///
/// A name may appear in more than one place (under different parents); each position is its own node.
#[derive(Debug, Clone, PartialEq)]
pub struct FlameNode {
    /// The span name.
    pub name: &'static str,
    /// Mean per-frame time spent in the span, inclusive of its children.
    pub inclusive: Duration,
    /// `inclusive` minus the sum of the children's `inclusive` time (the time spent directly in this span).
    pub self_time: Duration,
    /// Mean enter/exit pairs per frame over the captured window.
    pub calls: f64,
    /// Child spans, most inclusive time first.
    pub children: Vec<FlameNode>,
}

/// The aggregated call tree of the spans recorded over a window of frames.
///
/// `frames` is the divisor every duration and call count is averaged over; a window of one frame is the
/// current frame. `frame_time` is the mean wall time of a frame over the same window.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FlameGraph {
    /// Top-level spans (those with no timed ancestor), most inclusive time first.
    pub roots: Vec<FlameNode>,
    /// Frames the graph aggregates (the mean's divisor).
    pub frames: usize,
    /// Mean wall time of a frame over the same window (`Duration::ZERO` when nothing was recorded).
    pub frame_time: Duration,
}

impl FlameGraph {
    /// Mean per-frame time spent in every root span summed (the width of the whole flame graph).
    #[must_use]
    pub fn total_time(&self) -> Duration {
        self.roots.iter().map(|r| r.inclusive).sum()
    }

    /// The flame graph as folded stacks: one line per node with self time, `name;name;…  <self ms>`.
    ///
    /// The weight is the node's **self** time in milliseconds (not a sample count), so the lines sum to
    /// the same total as [`Self::total_time`]; this is the text form `flamegraph.pl` and grep consume.
    #[must_use]
    pub fn folded_stacks(&self) -> String {
        let mut out = String::new();
        for root in &self.roots {
            folded_into(root, "", &mut out);
        }
        out
    }

    /// A self-contained SVG flame graph: width ∝ inclusive time, depth down the page, one colour per depth.
    ///
    /// Deterministic for a given tree; suitable for "capture to file" and for a quick look without the
    /// editor. Names are XML-escaped.
    #[must_use]
    pub fn to_svg(&self) -> String {
        let depth = self.max_depth();
        let height = (depth + 1) as f64 * SVG_ROW + 4.0;
        let mut out = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{SVG_WIDTH:.0}\" height=\"{height:.0}\" viewBox=\"0 0 {SVG_WIDTH:.0} {height:.0}\">\n"
        );
        let _ = writeln!(
            out,
            "<rect x=\"0\" y=\"0\" width=\"{SVG_WIDTH:.0}\" height=\"{height:.0}\" fill=\"#1b1b1b\"/>"
        );
        let total = self.total_time();
        if !total.is_zero() {
            let mut x = 0.0;
            for root in &self.roots {
                emit(root, 0, &mut x, total, &mut out);
            }
        }
        out.push_str("</svg>\n");
        out
    }

    fn max_depth(&self) -> usize {
        self.roots.iter().map(|r| r.max_depth()).max().unwrap_or(0)
    }
}

impl FlameNode {
    fn max_depth(&self) -> usize {
        1 + self.children.iter().map(|c| c.max_depth()).max().unwrap_or(0)
    }
}

/// One frame's flame exits, already folded by call path.
///
/// Exits at the same position (same path and name) share a node. A name under two parents is two
/// nodes. Nodes store raw totals; [`Self::into_graph`] divides by the window length. A steady
/// frame (the same paths every frame) allocates nothing: `clear` returns every node's slot and
/// its children list to the next frame, keeping their capacity (ADR-7493).
#[derive(Debug)]
pub(crate) struct FrameFold {
    arena: Vec<Building>,
    index: HashMap<(usize, &'static str), usize>,
    /// Arena slots of the previous frame's nodes, recycled before the arena grows. Their
    /// `Building::children` lists keep their capacity.
    free: Vec<usize>,
    exits: u64,
}

impl FrameFold {
    pub(crate) fn new() -> Self {
        Self {
            arena: vec![Building::new("")],
            index: HashMap::new(),
            free: Vec::new(),
            exits: 0,
        }
    }

    /// An empty fold shared by frames that recorded no flame edges.
    pub(crate) fn empty_arc() -> Arc<Self> {
        static EMPTY: OnceLock<Arc<FrameFold>> = OnceLock::new();
        Arc::clone(EMPTY.get_or_init(|| Arc::new(FrameFold::new())))
    }

    pub(crate) fn exits(&self) -> u64 {
        self.exits
    }

    /// Drops the recorded exits and keeps the arena, its children lists and the index's capacity
    /// for the next frame: a steady frame (the same paths again) then allocates nothing.
    pub(crate) fn clear(&mut self) {
        self.free.clear();
        for slot in 1..self.arena.len() {
            self.arena[slot].reuse("");
            self.free.push(slot);
        }
        self.arena[0].reuse("");
        self.index.clear();
        self.exits = 0;
    }

    pub(crate) fn add(&mut self, path: &[&'static str], name: &'static str, total: Duration) {
        self.add_exits(path, name, total, 1);
    }

    fn add_exits(&mut self, path: &[&'static str], name: &'static str, total: Duration, exits: u64) {
        if exits == 0 && total.is_zero() {
            return;
        }
        let mut at = 0usize;
        for &step in path.iter().chain(std::iter::once(&name)) {
            at = match self.index.get(&(at, step)) {
                Some(&child) => child,
                None => {
                    let child = if let Some(slot) = self.free.pop() {
                        self.arena[slot].reuse(step);
                        slot
                    } else {
                        self.arena.push(Building::new(step));
                        self.arena.len() - 1
                    };
                    self.arena[at].children.push(child);
                    self.index.insert((at, step), child);
                    child
                }
            };
        }
        self.arena[at].total += total;
        self.arena[at].exits += exits;
        self.exits += exits;
    }

    /// Adds `other`'s exits into this fold. `other` is unchanged.
    pub(crate) fn merge(&mut self, other: &FrameFold) {
        let mut path = Vec::new();
        self.merge_at(other, 0, &mut path);
    }

    fn merge_at(&mut self, other: &FrameFold, at: usize, path: &mut Vec<&'static str>) {
        if at != 0 {
            let exits = other.arena[at].exits;
            let total = other.arena[at].total;
            if (exits > 0 || !total.is_zero())
                && let Some(name) = path.pop()
            {
                self.add_exits(path, name, total, exits);
                path.push(name);
            }
        }
        let children = other.arena[at].children.clone();
        for child in children {
            path.push(other.arena[child].name);
            self.merge_at(other, child, path);
            path.pop();
        }
    }

    pub(crate) fn into_graph(self, frames: usize, frame_time: Duration) -> FlameGraph {
        let divisor = frames.max(1) as f64;
        let mut roots: Vec<FlameNode> = self.arena[0]
            .children
            .iter()
            .map(|&child| build_node(&self.arena, child, divisor))
            .collect();
        sort_nodes(&mut roots);
        FlameGraph {
            roots,
            frames,
            frame_time,
        }
    }
}

/// Builds a [`FlameGraph`] from `(path, name, total)` span exits, where `path` is the span's timed
/// ancestors, outermost first (empty for a top-level span).
///
/// Exits at the same position (same path and name) are aggregated; `calls` is the number of exits folded
/// into a node, divided by `frames`. Because nodes are keyed by their full path, a name used under two
/// parents is two nodes and a recursive span nests inside itself, each with only its own time. A node on
/// the path of a recorded exit that never exited itself in the window (still open at the frame's end, or
/// dropped by the per-frame cap) is kept with the time of its children, so the tree always nests.
#[cfg(test)]
pub(crate) fn from_edges<P, I>(frames: usize, frame_time: Duration, edges: I) -> FlameGraph
where
    P: AsRef<[&'static str]>,
    I: IntoIterator<Item = (P, &'static str, Duration)>,
{
    let mut fold = FrameFold::new();
    for (path, name, total) in edges {
        fold.add(path.as_ref(), name, total);
    }
    fold.into_graph(frames, frame_time)
}

/// One tree position while a [`FrameFold`] aggregates: the window totals and the child positions.
#[derive(Debug)]
struct Building {
    name: &'static str,
    total: Duration,
    exits: u64,
    children: Vec<usize>,
}

impl Building {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            total: Duration::ZERO,
            exits: 0,
            children: Vec::new(),
        }
    }

    /// Prepares a used slot for a new node: a new name, no totals, and an empty children list
    /// that keeps its capacity (the next `push` into it allocates nothing, ADR-7493).
    fn reuse(&mut self, name: &'static str) {
        self.name = name;
        self.total = Duration::ZERO;
        self.exits = 0;
        self.children.clear();
    }
}

fn build_node(arena: &[Building], at: usize, divisor: f64) -> FlameNode {
    let node = &arena[at];
    let mut children: Vec<FlameNode> = node
        .children
        .iter()
        .map(|&child| build_node(arena, child, divisor))
        .collect();
    sort_nodes(&mut children);
    // Window totals become per-frame means; a node is never narrower than its children.
    let children_time: Duration = children.iter().map(|c| c.inclusive).sum();
    let inclusive = node.total.div_f64(divisor).max(children_time);
    FlameNode {
        name: node.name,
        inclusive,
        self_time: inclusive.saturating_sub(children_time),
        calls: node.exits as f64 / divisor,
        children,
    }
}

/// Most inclusive time first, then by name (so equal trees serialize identically).
fn sort_nodes(nodes: &mut [FlameNode]) {
    nodes.sort_by(|a, b| b.inclusive.cmp(&a.inclusive).then(a.name.cmp(b.name)));
}

fn folded_into(node: &FlameNode, prefix: &str, out: &mut String) {
    let path = if prefix.is_empty() {
        node.name.to_owned()
    } else {
        format!("{prefix};{}", node.name)
    };
    if !node.self_time.is_zero() {
        let _ = writeln!(out, "{path} {:.3}", node.self_time.as_secs_f64() * 1000.0);
    }
    for child in &node.children {
        folded_into(child, &path, out);
    }
}

fn emit(node: &FlameNode, depth: usize, x: &mut f64, total: Duration, out: &mut String) {
    let w = (node.inclusive.as_secs_f64() / total.as_secs_f64() * SVG_WIDTH).max(SVG_MIN_W);
    let x0 = *x;
    *x += w;
    let y = depth as f64 * SVG_ROW;
    let fill = fill(depth);
    let title = xml_escape(&format!(
        "{} — {:.2} ms ({:.1}%), self {:.2} ms",
        node.name,
        node.inclusive.as_secs_f64() * 1000.0,
        node.inclusive.as_secs_f64() / total.as_secs_f64() * 100.0,
        node.self_time.as_secs_f64() * 1000.0,
    ));
    let _ = writeln!(
        out,
        "<g><title>{title}</title><rect x=\"{x0:.2}\" y=\"{y:.2}\" width=\"{w:.2}\" height=\"{SVG_ROW:.2}\" fill=\"{fill}\"/></g>"
    );
    if w >= SVG_TEXT_MIN_W {
        let text = xml_escape(node.name);
        let _ = writeln!(
            out,
            "<text x=\"{:.2}\" y=\"{:.2}\" font-size=\"11\" fill=\"#ffffff\">{text}</text>",
            x0 + 3.0,
            y + SVG_ROW - 4.0,
        );
    }
    let mut child_x = x0;
    for child in &node.children {
        emit(child, depth + 1, &mut child_x, total, out);
    }
}

/// A stable colour per depth (the classic flame-graph look, deterministic across runs).
fn fill(depth: usize) -> &'static str {
    const PALETTE: [&str; 8] = [
        "#e6194b", "#3cb44b", "#ffe119", "#4363d8", "#f58231", "#911eb4", "#42d4f4", "#f032e6",
    ];
    PALETTE[depth % PALETTE.len()]
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn builds_a_tree_with_inclusive_and_self_time() {
        // outer (10) → inner (6); inner → leaf (2), each over one frame.
        let edges: [(&[&'static str], _, _); 3] = [
            (&[], "outer", ms(10)),
            (&["outer"], "inner", ms(6)),
            (&["outer", "inner"], "leaf", ms(2)),
        ];
        let graph = from_edges(1, ms(16), edges);
        assert_eq!(graph.frames, 1);
        assert_eq!(graph.total_time(), ms(10));
        let outer = &graph.roots[0];
        assert_eq!(outer.name, "outer");
        assert_eq!(outer.inclusive, ms(10));
        assert_eq!(outer.self_time, ms(4));
        assert_eq!(outer.calls, 1.0);
        let inner = &outer.children[0];
        assert_eq!(inner.name, "inner");
        assert_eq!(inner.inclusive, ms(6));
        assert_eq!(inner.self_time, ms(4));
        let leaf = &inner.children[0];
        assert_eq!(leaf.name, "leaf");
        assert_eq!(leaf.inclusive, ms(2));
        assert_eq!(leaf.self_time, ms(2));
        assert!(leaf.children.is_empty());
    }

    #[test]
    fn averages_over_frames_and_counts_calls() {
        // Two frames: outer always runs; leaf runs only in one of them.
        let edges: [(&[&'static str], _, _); 3] = [
            (&[], "outer", ms(10)),
            (&[], "outer", ms(6)),
            (&["outer"], "leaf", ms(2)),
        ];
        let graph = from_edges(2, ms(16), edges);
        let outer = &graph.roots[0];
        assert_eq!(outer.inclusive, ms(8)); // (10 + 6) / 2
        assert_eq!(outer.calls, 1.0); // 2 exits / 2 frames
        assert_eq!(outer.children[0].calls, 0.5); // 1 exit / 2 frames
    }

    #[test]
    fn recursion_nests_and_totals_still_add_up() {
        // outer (10) recurses into itself once: the inner call (6) is its own node.
        let edges: [(&[&'static str], _, _); 2] = [(&[], "outer", ms(10)), (&["outer"], "outer", ms(6))];
        let graph = from_edges(1, ms(16), edges);
        let outer = &graph.roots[0];
        assert_eq!(outer.inclusive, ms(10));
        assert_eq!(outer.self_time, ms(4));
        assert_eq!(outer.children[0].name, "outer");
        assert_eq!(outer.children[0].inclusive, ms(6));
        assert!(outer.children[0].children.is_empty());
    }

    #[test]
    fn same_name_under_different_parents_stays_separate() {
        // `work` runs under `a` and under `b`, with a `leaf` child only under `a`: the leaf's time
        // belongs to `a;work` alone (a parent-name tree would show it under both).
        let edges: [(&[&'static str], _, _); 5] = [
            (&[], "a", ms(10)),
            (&["a"], "work", ms(4)),
            (&["a", "work"], "leaf", ms(3)),
            (&[], "b", ms(8)),
            (&["b"], "work", ms(3)),
        ];
        let graph = from_edges(1, ms(16), edges);
        assert_eq!(graph.roots.len(), 2);
        assert_eq!(graph.roots[0].name, "a");
        let a_work = &graph.roots[0].children[0];
        assert_eq!(a_work.inclusive, ms(4));
        assert_eq!(a_work.self_time, ms(1));
        assert_eq!(a_work.children[0].name, "leaf");
        let b_work = &graph.roots[1].children[0];
        assert_eq!(b_work.inclusive, ms(3));
        assert_eq!(b_work.self_time, ms(3));
        assert!(b_work.children.is_empty());
    }

    #[test]
    fn an_ancestor_that_never_exited_still_contains_its_children() {
        // `frame` was still open when the window closed: only its child exited.
        let edges: [(&[&'static str], _, _); 1] = [(&["frame"], "render", ms(5))];
        let graph = from_edges(1, ms(16), edges);
        let frame = &graph.roots[0];
        assert_eq!(frame.name, "frame");
        assert_eq!(frame.inclusive, ms(5));
        assert_eq!(frame.self_time, Duration::ZERO);
        assert_eq!(frame.calls, 0.0);
        assert_eq!(frame.children[0].inclusive, ms(5));
    }

    #[test]
    fn folded_stacks_lists_self_time_lines() {
        let edges: [(&[&'static str], _, _); 2] = [(&[], "outer", ms(10)), (&["outer"], "inner", ms(6))];
        let folded = from_edges(1, ms(16), edges).folded_stacks();
        assert!(folded.contains("outer 4.000"), "{folded}");
        assert!(folded.contains("outer;inner 6.000"), "{folded}");
    }

    #[test]
    fn svg_is_self_contained_and_escapes_names() {
        let edges: [(&[&'static str], _, _); 2] = [(&[], "a<&>", ms(10)), (&["a<&>"], "child", ms(2))];
        let svg = from_edges(1, ms(16), edges).to_svg();
        assert!(svg.starts_with("<svg"), "{svg}");
        assert!(svg.ends_with("</svg>\n"), "{svg}");
        assert!(svg.contains("a&lt;&amp;&gt;"), "{svg}");
        assert!(svg.contains("<rect"), "{svg}");
        // The empty graph renders a bare background.
        assert!(FlameGraph::default().to_svg().contains("<rect"));
    }

    #[test]
    fn merging_two_folds_matches_folding_their_edges_together() {
        let mut a = FrameFold::new();
        a.add(&[], "outer", ms(10));
        a.add(&["outer"], "inner", ms(4));
        let mut b = FrameFold::new();
        b.add(&[], "outer", ms(6));
        b.add(&["outer"], "leaf", ms(2));
        let mut merged = FrameFold::new();
        merged.merge(&a);
        merged.merge(&b);
        assert_eq!(merged.exits(), 4);
        let graph = merged.into_graph(2, ms(16));
        let together = from_edges(
            2,
            ms(16),
            [
                (&[] as &[&str], "outer", ms(10)),
                (&["outer"] as &[&str], "inner", ms(4)),
                (&[], "outer", ms(6)),
                (&["outer"], "leaf", ms(2)),
            ],
        );
        assert_eq!(graph, together);
    }
}
