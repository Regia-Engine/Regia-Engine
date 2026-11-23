// SPDX-License-Identifier: MPL-2.0
//! Profile captures (`docs/specs/83-prefab-profiler-volumes.md` § 4.2, ADR-2323): the frame
//! profiler's flame graph over a window of frames and the GPU pass times, as one JSON document
//! (`.rprof.json`, format `profile-capture` v2). Version 2 (ADR-7325) adds the optional per-thread
//! timeline of the captured frames
//! ([`CaptureTimelineFrame`](crate::profile_capture::CaptureTimelineFrame)); a v1 file reads as v2
//! with no timeline.
//!
//! A capture is a diagnostic artefact, not content: the engine never loads it. It is written by
//! the editor's Profiler panel and the agent method `profiler.capture`, and read back by tools
//! ([`ProfileCapture::from_json`](crate::profile_capture::ProfileCapture::from_json)). Nodes are sorted by inclusive time then name, durations are
//! milliseconds, so two captures of the same data serialize byte-identically.

use std::path::Path;

use regia_core::profiling::{FlameGraph, FlameNode, TimelineFrame};
use serde::{Deserialize, Serialize};

/// The `profile-capture` format version this build reads and writes.
pub const PROFILE_CAPTURE_FORMAT_VERSION: u32 = 2;

/// Default most timeline spans a capture keeps (all frames, all threads); the rest are counted in
/// [`ProfileCapture::timeline_truncated`].
pub const DEFAULT_MAX_TIMELINE_SPANS: usize = 200_000;

/// File name suffix of a capture.
pub const PROFILE_CAPTURE_SUFFIX: &str = ".rprof.json";

/// Default depth limit of a capture's CPU tree.
pub const DEFAULT_MAX_DEPTH: usize = 8;

/// Default node limit of a capture's CPU tree.
pub const DEFAULT_MAX_NODES: usize = 2_000;

/// Errors from reading or writing a capture.
#[derive(Debug, thiserror::Error)]
pub enum ProfileCaptureError {
    /// The text is not a capture.
    #[error("profile capture JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// Written by a newer engine.
    #[error(
        "profile capture format_version {found} is newer than this engine's {PROFILE_CAPTURE_FORMAT_VERSION}"
    )]
    Newer {
        /// The version in the file.
        found: u32,
    },
    /// Writing the file failed.
    #[error("cannot write {path}: {source}")]
    Io {
        /// The file.
        path: String,
        /// The error.
        source: std::io::Error,
    },
}

/// One span of the CPU tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureNode {
    /// The span name.
    pub name: String,
    /// Mean per-frame time including children, milliseconds.
    pub inclusive_ms: f64,
    /// Mean per-frame time in the span itself, milliseconds.
    pub self_ms: f64,
    /// Mean calls per frame.
    pub calls: f64,
    /// Child spans, most inclusive time first (ties by name).
    #[serde(default)]
    pub children: Vec<CaptureNode>,
}

/// One GPU pass's time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GpuPassTime {
    /// The pass name (`GpuTimings::passes`).
    pub pass: String,
    /// Milliseconds, last frame.
    pub ms: f64,
}

/// One span of a captured thread timeline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureSpan {
    /// The span name.
    pub name: String,
    /// Enter, ns from the frame's start (0 for a span entered in an earlier frame).
    pub start_ns: u64,
    /// Exit, ns from the frame's start.
    pub end_ns: u64,
    /// Recorded spans of the same thread open around it (0 = a root).
    pub depth: u16,
}

/// One thread of a captured frame's timeline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureThread {
    /// The profiler's index of the thread (stable while the process runs).
    pub index: u32,
    /// The thread's name (`thread <index>` for an unnamed thread).
    pub name: String,
    /// Its spans by start, then depth, then end.
    pub spans: Vec<CaptureSpan>,
}

/// The per-thread timeline of one captured frame (format v2, ADR-7325).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureTimelineFrame {
    /// The frame's number in the profiled process (frames ended before it).
    pub number: u64,
    /// Wall time of the frame, ns.
    pub frame_ns: u64,
    /// Spans the profiler dropped at its per-frame cap.
    #[serde(default)]
    pub dropped: u64,
    /// Threads that recorded spans, by index.
    pub threads: Vec<CaptureThread>,
}

impl CaptureTimelineFrame {
    /// The capture form of a profiler timeline frame.
    #[must_use]
    pub fn from_frame(frame: &TimelineFrame) -> Self {
        Self {
            number: frame.number,
            frame_ns: frame.duration_ns,
            dropped: frame.dropped,
            threads: frame
                .threads
                .iter()
                .map(|t| CaptureThread {
                    index: t.index,
                    name: t.name.to_string(),
                    spans: t
                        .spans
                        .iter()
                        .map(|s| CaptureSpan {
                            name: s.name.to_string(),
                            start_ns: s.start_ns,
                            end_ns: s.end_ns,
                            depth: s.depth,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// Spans kept, every thread.
    #[must_use]
    pub fn span_count(&self) -> usize {
        self.threads.iter().map(|t| t.spans.len()).sum()
    }

    /// The frame's extent on a strip axis: its wall time, or its last span's end when that is
    /// later (a span that outlasted the frame's wall clock).
    #[must_use]
    pub fn span_ns(&self) -> u64 {
        self.threads
            .iter()
            .flat_map(|t| t.spans.iter().map(|s| s.end_ns))
            .fold(self.frame_ns, u64::max)
            .max(1)
    }
}

/// Where one retained frame sits on a strip's time axis ([`timeline_strip`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureStripFrame {
    /// The frame's number in the profiled process.
    pub number: u64,
    /// The frame's wall time, ns.
    pub frame_ns: u64,
    /// The frame's start on the strip's axis, ns (the sum of the earlier frames' extents).
    pub start_ns: u64,
    /// The frame's extent on the axis ([`CaptureTimelineFrame::span_ns`]).
    pub span_ns: u64,
    /// Spans the profiler dropped at its per-frame cap.
    pub dropped: u64,
}

/// Consecutive retained frames laid on one time axis ([`timeline_strip`], ADR-7325).
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineStrip {
    /// The frames on the axis, oldest first.
    pub frames: Vec<CaptureStripFrame>,
    /// Every frame's spans shifted onto the axis, threads merged by name; the number is the
    /// newest frame's, the wall time the whole axis.
    pub combined: CaptureTimelineFrame,
}

impl Default for TimelineStrip {
    fn default() -> Self {
        Self {
            frames: Vec::new(),
            combined: CaptureTimelineFrame {
                number: 0,
                frame_ns: 0,
                dropped: 0,
                threads: Vec::new(),
            },
        }
    }
}

/// Lays `frames` (consecutive, oldest first) on one time axis: each frame's spans are shifted
/// past the extents of the frames before it, so frame k's first span starts at or after frame
/// k-1's last end, and threads of the same name merge into one (a thread first seen in a later
/// frame is appended, in first-seen order). The combined frame's number is the newest frame's
/// and its wall time spans the whole axis, so a view over it zooms and pans across the strip.
/// Empty input gives an empty strip.
#[must_use]
pub fn timeline_strip(frames: &[CaptureTimelineFrame]) -> TimelineStrip {
    let _span = tracing::info_span!("timeline_strip").entered();
    if frames.is_empty() {
        return TimelineStrip::default();
    }
    let mut out = TimelineStrip::default();
    let mut offset = 0_u64;
    let mut threads: Vec<CaptureThread> = Vec::new();
    let mut dropped = 0_u64;
    for frame in frames {
        let extent = frame.span_ns();
        out.frames.push(CaptureStripFrame {
            number: frame.number,
            frame_ns: frame.frame_ns,
            start_ns: offset,
            span_ns: extent,
            dropped: frame.dropped,
        });
        for thread in &frame.threads {
            let at = match threads.iter().position(|t| t.name == thread.name) {
                Some(at) => at,
                None => {
                    threads.push(CaptureThread {
                        index: thread.index,
                        name: thread.name.clone(),
                        spans: Vec::new(),
                    });
                    threads.len() - 1
                }
            };
            let merged = &mut threads[at];
            merged.spans.extend(thread.spans.iter().map(|s| CaptureSpan {
                name: s.name.clone(),
                start_ns: s.start_ns + offset,
                end_ns: s.end_ns + offset,
                depth: s.depth,
            }));
        }
        dropped += frame.dropped;
        offset += extent;
    }
    let last = out.frames.last().copied().unwrap_or(CaptureStripFrame {
        number: 0,
        frame_ns: 0,
        start_ns: 0,
        span_ns: 0,
        dropped: 0,
    });
    for thread in &mut threads {
        thread
            .spans
            .sort_unstable_by_key(|s| (s.start_ns, s.depth, s.end_ns));
    }
    out.combined = CaptureTimelineFrame {
        number: last.number,
        frame_ns: offset,
        dropped,
        threads,
    };
    out
}

/// A `.rprof.json` document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileCapture {
    /// Format version; missing loads as 1, newer is refused.
    #[serde(default)]
    pub format_version: u32,
    /// Frames the CPU tree averages over.
    pub frames: usize,
    /// Mean frame wall time over the same frames, milliseconds.
    pub frame_ms: f64,
    /// Top-level spans.
    pub cpu: Vec<CaptureNode>,
    /// Nodes left out by the depth or node limit (0 = the tree is complete).
    #[serde(default)]
    pub truncated: usize,
    /// GPU pass times of the last frame; empty without GPU timestamps.
    #[serde(default)]
    pub gpu: Vec<GpuPassTime>,
    /// The tree as folded stacks (`a;b;c <self ms>` per line), for flame-graph tools.
    #[serde(default)]
    pub folded: String,
    /// Version 2: the per-thread timeline of the captured frames, oldest first; empty when the
    /// profiler's timeline was off (and in every v1 file).
    #[serde(default)]
    pub timeline: Vec<CaptureTimelineFrame>,
    /// Timeline spans left out by the capture's span limit.
    #[serde(default)]
    pub timeline_truncated: usize,
}

fn ms(duration: std::time::Duration) -> f64 {
    // Microsecond resolution keeps the text short and stable.
    (duration.as_secs_f64() * 1_000_000.0).round() / 1000.0
}

fn convert(
    node: &FlameNode,
    depth: usize,
    max_depth: usize,
    budget: &mut usize,
    truncated: &mut usize,
) -> CaptureNode {
    let mut children: Vec<CaptureNode> = Vec::new();
    if depth + 1 < max_depth {
        for child in by_cost(&node.children) {
            if *budget == 0 {
                *truncated += 1 + count(child);
                continue;
            }
            *budget -= 1;
            children.push(convert(child, depth + 1, max_depth, budget, truncated));
        }
    } else {
        *truncated += node.children.iter().map(|c| 1 + count(c)).sum::<usize>();
    }
    sort_nodes(&mut children);
    CaptureNode {
        name: node.name.to_string(),
        inclusive_ms: ms(node.inclusive),
        self_ms: ms(node.self_time),
        calls: node.calls,
        children,
    }
}

/// `nodes` most inclusive first, ties by name, so the node limit keeps the costliest spans
/// whatever order the graph listed them in.
fn by_cost(nodes: &[FlameNode]) -> Vec<&FlameNode> {
    let mut sorted: Vec<&FlameNode> = nodes.iter().collect();
    sorted.sort_by(|a, b| b.inclusive.cmp(&a.inclusive).then_with(|| a.name.cmp(b.name)));
    sorted
}

fn count(node: &FlameNode) -> usize {
    node.children.iter().map(|c| 1 + count(c)).sum()
}

fn sort_nodes(nodes: &mut [CaptureNode]) {
    nodes.sort_by(|a, b| {
        b.inclusive_ms
            .total_cmp(&a.inclusive_ms)
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn folded(nodes: &[CaptureNode], prefix: &str, out: &mut String) {
    for node in nodes {
        let path = if prefix.is_empty() {
            node.name.clone()
        } else {
            format!("{prefix};{}", node.name)
        };
        out.push_str(&format!("{path} {}\n", node.self_ms));
        folded(&node.children, &path, out);
    }
}

impl ProfileCapture {
    /// A capture of `graph` (at most `max_depth` levels and `max_nodes` nodes) and the GPU passes
    /// (`gpu`: `(pass, ms)` pairs; passes at 0 ms are left out).
    #[must_use]
    pub fn from_flame(graph: &FlameGraph, gpu: &[(&str, f32)], max_depth: usize, max_nodes: usize) -> Self {
        let max_depth = max_depth.max(1);
        let mut budget = max_nodes;
        let mut truncated = 0;
        let mut cpu = Vec::new();
        for root in by_cost(&graph.roots) {
            if budget == 0 {
                truncated += 1 + count(root);
                continue;
            }
            budget -= 1;
            cpu.push(convert(root, 0, max_depth, &mut budget, &mut truncated));
        }
        sort_nodes(&mut cpu);
        let mut text = String::new();
        folded(&cpu, "", &mut text);
        Self {
            format_version: PROFILE_CAPTURE_FORMAT_VERSION,
            frames: graph.frames,
            frame_ms: ms(graph.frame_time),
            cpu,
            truncated,
            gpu: gpu
                .iter()
                .filter(|(_, time)| time.is_finite() && *time > 0.0)
                .map(|(pass, time)| GpuPassTime {
                    pass: (*pass).to_string(),
                    ms: (f64::from(*time) * 1000.0).round() / 1000.0,
                })
                .collect(),
            folded: text,
            timeline: Vec::new(),
            timeline_truncated: 0,
        }
    }

    /// Adds the per-thread timeline of `frames` (oldest first; frames with no threads are left
    /// out), keeping at most `max_spans` spans: a frame that does not fit whole is cut at a thread
    /// boundary and the spans left out are counted in [`Self::timeline_truncated`].
    #[must_use]
    pub fn with_timeline(mut self, frames: &[TimelineFrame], max_spans: usize) -> Self {
        let mut budget = max_spans;
        for frame in frames.iter().filter(|f| !f.threads.is_empty()) {
            let mut captured = CaptureTimelineFrame::from_frame(frame);
            let mut kept = Vec::new();
            for thread in std::mem::take(&mut captured.threads) {
                if thread.spans.len() <= budget {
                    budget -= thread.spans.len();
                    kept.push(thread);
                } else {
                    self.timeline_truncated += thread.spans.len();
                }
            }
            if !kept.is_empty() {
                captured.threads = kept;
                self.timeline.push(captured);
            }
        }
        self
    }

    /// Timeline spans in the capture, every frame and thread.
    #[must_use]
    pub fn timeline_span_count(&self) -> usize {
        self.timeline.iter().map(CaptureTimelineFrame::span_count).sum()
    }

    /// Every node of the CPU tree, depth-first.
    #[must_use]
    pub fn node_count(&self) -> usize {
        fn walk(nodes: &[CaptureNode]) -> usize {
            nodes.iter().map(|n| 1 + walk(&n.children)).sum()
        }
        walk(&self.cpu)
    }

    /// The pretty JSON text.
    ///
    /// # Errors
    /// Serialization failed (not expected for finite numbers).
    pub fn to_json(&self) -> Result<String, ProfileCaptureError> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        Ok(text)
    }

    /// Parses a capture; a missing `format_version` (0) or version 1 is read as the current
    /// version (the reader step: v1 has no timeline, so it loads with none), a newer one is refused.
    ///
    /// # Errors
    /// Not JSON of the capture's shape, or a newer version.
    pub fn from_json(text: &str) -> Result<Self, ProfileCaptureError> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        let found = value
            .get("format_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if found > u64::from(PROFILE_CAPTURE_FORMAT_VERSION) {
            return Err(ProfileCaptureError::Newer {
                found: u32::try_from(found).unwrap_or(u32::MAX),
            });
        }
        let mut capture: Self = serde_json::from_value(value)?;
        // Reader steps: 0 (no field) and 1 (no `timeline`) are the current shape with defaults.
        if capture.format_version < PROFILE_CAPTURE_FORMAT_VERSION {
            capture.format_version = PROFILE_CAPTURE_FORMAT_VERSION;
        }
        Ok(capture)
    }

    /// Writes the capture to `path` atomically (a temporary file, then a rename), creating the
    /// parent directory. Each call has its own temp name, so two writers of one path (the editor
    /// Profiler panel and `profiler.capture`) never truncate the same temp file (ADR-2396).
    ///
    /// # Errors
    /// The directory or file cannot be written.
    pub fn write(&self, path: &Path) -> Result<(), ProfileCaptureError> {
        let io = |source: std::io::Error| ProfileCaptureError::Io {
            path: path.display().to_string(),
            source,
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let text = self.to_json()?;
        crate::summary::write_atomic(path, &text).map_err(io)
    }
}

/// The `from` versions of the `profile-capture` migration steps (`0`: no `format_version`; `1`:
/// no `timeline`), both read as the current version by [`ProfileCapture::from_json`].
#[must_use]
pub fn profile_capture_migration_sources() -> Vec<u32> {
    vec![0, 1]
}

/// The ordered serde field lists of the capture document (`cargo xtask check-formats` entry
/// `profile-capture`).
#[must_use]
pub fn profile_capture_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<ProfileCapture>(),
        regia_core::schema::serde_field_names::<CaptureNode>(),
        regia_core::schema::serde_field_names::<GpuPassTime>(),
        regia_core::schema::serde_field_names::<CaptureTimelineFrame>(),
        regia_core::schema::serde_field_names::<CaptureThread>(),
        regia_core::schema::serde_field_names::<CaptureSpan>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn node(name: &'static str, inclusive_us: u64, self_us: u64, children: Vec<FlameNode>) -> FlameNode {
        FlameNode {
            name,
            inclusive: Duration::from_micros(inclusive_us),
            self_time: Duration::from_micros(self_us),
            calls: 1.0,
            children,
        }
    }

    fn graph() -> FlameGraph {
        FlameGraph {
            roots: vec![
                node("update", 2_000, 500, vec![node("physics", 1_500, 1_500, vec![])]),
                node(
                    "render",
                    6_000,
                    1_000,
                    vec![
                        node("shadows", 2_000, 2_000, vec![]),
                        node("forward", 3_000, 2_500, vec![node("sort", 500, 500, vec![])]),
                    ],
                ),
            ],
            frames: 60,
            frame_time: Duration::from_micros(8_250),
        }
    }

    #[test]
    fn a_capture_sorts_folds_and_round_trips() {
        let gpu = [("shadows", 0.9_f32), ("ssao", 0.0), ("forward", 2.25)];
        let capture = ProfileCapture::from_flame(&graph(), &gpu, DEFAULT_MAX_DEPTH, DEFAULT_MAX_NODES);
        assert_eq!(capture.frames, 60);
        assert_eq!(capture.frame_ms, 8.25);
        let names: Vec<&str> = capture.cpu.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["render", "update"], "most inclusive first");
        assert_eq!(capture.cpu[0].children[0].name, "forward");
        assert_eq!(capture.node_count(), 6);
        assert_eq!(capture.truncated, 0);
        assert_eq!(capture.gpu.len(), 2, "passes at 0 ms are left out");
        assert!(
            capture.folded.contains("render;forward;sort 0.5\n"),
            "{}",
            capture.folded
        );
        let text = capture.to_json().expect("json");
        let back = ProfileCapture::from_json(&text).expect("parse");
        assert_eq!(back, capture);
        assert_eq!(back.to_json().expect("again"), text, "byte-identical");
    }

    /// A strip shifts consecutive frames onto one time axis: each frame's spans move past the
    /// extents of the frames before it, threads merge by name (first-seen order, later frames
    /// append new threads), and the frame list carries each start offset (ADR-7325).
    #[test]
    fn a_strip_shifts_frames_onto_one_axis_and_merges_threads() {
        let frame = |number: u64, spans: Vec<CaptureSpan>, extra: Option<CaptureThread>| {
            let mut threads = vec![CaptureThread {
                index: 0,
                name: "main".to_owned(),
                spans,
            }];
            threads.extend(extra);
            CaptureTimelineFrame {
                number,
                frame_ns: 1_000_000,
                dropped: 0,
                threads,
            }
        };
        let span = |name: &str, start: u64, end: u64, depth: u16| CaptureSpan {
            name: name.to_owned(),
            start_ns: start,
            end_ns: end,
            depth,
        };
        let frames = vec![
            frame(10, vec![span("a", 0, 400_000, 0)], None),
            frame(
                11,
                vec![span("b", 100_000, 900_000, 0)],
                Some(CaptureThread {
                    index: 3,
                    name: "worker".to_owned(),
                    spans: vec![span("w", 0, 200_000, 0)],
                }),
            ),
            // The last span's end outlasts the wall time: the extent is the later.
            frame(12, vec![span("c", 0, 2_500_000, 0)], None),
        ];
        let strip = timeline_strip(&frames);
        let starts: Vec<(u64, u64)> = strip.frames.iter().map(|f| (f.number, f.start_ns)).collect();
        assert_eq!(starts, [(10, 0), (11, 1_000_000), (12, 2_000_000)]);
        assert_eq!(
            strip.frames[2].span_ns, 2_500_000,
            "the extent covers the last span"
        );
        assert_eq!(strip.combined.number, 12, "the strip ends at the newest frame");
        assert_eq!(strip.combined.span_count(), 4);
        let names: Vec<&str> = strip.combined.threads.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["main", "worker"], "threads merge by name");
        let main: Vec<(u64, u64)> = strip.combined.threads[0]
            .spans
            .iter()
            .map(|s| (s.start_ns, s.end_ns))
            .collect();
        assert_eq!(
            main,
            [(0, 400_000), (1_100_000, 1_900_000), (2_000_000, 4_500_000)],
            "each frame's spans shift past the earlier extents"
        );
        assert_eq!(
            strip.combined.threads[1].spans[0].start_ns, 1_000_000,
            "a thread seen in a later frame starts at that frame's offset"
        );
        // Empty input: an empty strip.
        assert_eq!(timeline_strip(&[]), TimelineStrip::default());
        // A single frame keeps its own times.
        let one = timeline_strip(&frames[..1]);
        assert_eq!(one.frames[0].start_ns, 0);
        assert_eq!(one.combined.span_count(), 1);
    }

    /// Two writers of one capture path must not share a temp file. The agent method
    /// `profiler.capture` and the editor Profiler panel both call [`ProfileCapture::write`], and
    /// nothing serializes them. A directory at the old fixed name (`<file>.json.tmp`, from
    /// `Path::with_extension`) stands in for a writer mid-flight on that shared name: a shared
    /// temp can neither truncate a directory nor rename it over the target (ADR-2396).
    #[test]
    fn two_writes_of_one_capture_path_do_not_share_a_temp_file() {
        let dir = regia_test_utils::unique_temp_dir("profile-capture-temp");
        let path = dir.join("one.rprof.json");
        let shared = path.with_extension("json.tmp");
        std::fs::create_dir_all(&shared).expect("the other writer's temp");
        let capture = ProfileCapture::from_flame(&graph(), &[], DEFAULT_MAX_DEPTH, DEFAULT_MAX_NODES);
        capture.write(&path).expect("a write with its own temp name");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(ProfileCapture::from_json(&text).expect("parse"), capture);
        let target = path.file_name().and_then(|n| n.to_str()).expect("name");
        let blocked = shared.file_name().and_then(|n| n.to_str()).expect("blocked");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("list")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != target && name != blocked)
            .collect();
        assert!(leftovers.is_empty(), "no temp file is left: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn limits_truncate_and_count_what_they_drop() {
        let shallow = ProfileCapture::from_flame(&graph(), &[], 1, DEFAULT_MAX_NODES);
        assert_eq!(shallow.node_count(), 2);
        assert_eq!(shallow.truncated, 4);
        let few = ProfileCapture::from_flame(&graph(), &[], DEFAULT_MAX_DEPTH, 3);
        assert_eq!(few.node_count(), 3);
        assert_eq!(few.truncated, 3);
        let empty =
            ProfileCapture::from_flame(&FlameGraph::default(), &[], DEFAULT_MAX_DEPTH, DEFAULT_MAX_NODES);
        assert!(empty.cpu.is_empty() && empty.folded.is_empty());
    }

    #[test]
    fn versions_migrate_and_newer_is_refused() {
        let bare = r#"{ "frames": 1, "frame_ms": 1.0, "cpu": [] }"#;
        assert_eq!(
            ProfileCapture::from_json(bare).expect("v0").format_version,
            PROFILE_CAPTURE_FORMAT_VERSION
        );
        let newer = r#"{ "format_version": 3, "anything": true }"#;
        assert!(matches!(
            ProfileCapture::from_json(newer),
            Err(ProfileCaptureError::Newer { found: 3 })
        ));
        assert!(ProfileCapture::from_json("not json").is_err());
    }

    /// ADR-7325: a v1 file (the fixture, written by v1 engines) loads through the reader step as
    /// the current version with no timeline and the same tree.
    #[test]
    fn a_v1_fixture_loads_with_no_timeline() {
        let text = include_str!("../tests/fixtures/profile-capture-v1.rprof.json");
        let capture = ProfileCapture::from_json(text).expect("v1 fixture");
        assert_eq!(capture.format_version, PROFILE_CAPTURE_FORMAT_VERSION);
        assert!(capture.timeline.is_empty() && capture.timeline_truncated == 0);
        assert_eq!(capture.frames, 60);
        assert_eq!(capture.node_count(), 3);
        assert_eq!(profile_capture_migration_sources(), [0, 1]);
    }

    /// `(name, start_ns, end_ns, depth)`.
    type SpanRow = (&'static str, u64, u64, u16);
    /// `(thread name, spans)`.
    type ThreadRows<'a> = (&'static str, &'a [SpanRow]);

    fn timeline_frame(number: u64, threads: &[ThreadRows<'_>]) -> TimelineFrame {
        use regia_core::profiling::{TimelineSpan, TimelineThread};
        TimelineFrame {
            number,
            duration_ns: 16_000_000,
            dropped: number % 2,
            threads: threads
                .iter()
                .enumerate()
                .map(|(i, (name, spans))| TimelineThread {
                    index: i as u32,
                    name: std::sync::Arc::from(*name),
                    spans: spans
                        .iter()
                        .map(|&(name, start_ns, end_ns, depth)| TimelineSpan {
                            name: std::borrow::Cow::Borrowed(name),
                            start_ns,
                            end_ns,
                            depth,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// ADR-7325: a v2 capture with a two-thread timeline round-trips byte-identically; the span
    /// limit cuts at a thread and counts what it left out; empty frames are left out.
    #[test]
    fn a_v2_capture_with_a_timeline_round_trips_byte_identically() {
        let frames = [
            timeline_frame(
                7,
                &[
                    (
                        "main",
                        &[("frame", 0, 9_000_000, 0), ("update", 100, 4_000_123, 1)],
                    ),
                    ("regia-compute (0)", &[("par_chunk", 1_000, 2_500, 0)]),
                ],
            ),
            timeline_frame(8, &[]),
        ];
        let capture = ProfileCapture::from_flame(&graph(), &[], DEFAULT_MAX_DEPTH, DEFAULT_MAX_NODES)
            .with_timeline(&frames, DEFAULT_MAX_TIMELINE_SPANS);
        assert_eq!(capture.format_version, 2);
        assert_eq!(capture.timeline.len(), 1, "a frame with no threads is left out");
        assert_eq!(capture.timeline_span_count(), 3);
        assert_eq!(capture.timeline[0].threads[1].name, "regia-compute (0)");
        assert_eq!(capture.timeline[0].dropped, 1);
        let text = capture.to_json().expect("json");
        assert!(text.contains("\"format_version\": 2"), "{text}");
        let back = ProfileCapture::from_json(&text).expect("parse");
        assert_eq!(back, capture);
        assert_eq!(back.to_json().expect("again"), text, "byte-identical");
        let cut = ProfileCapture::from_flame(&graph(), &[], DEFAULT_MAX_DEPTH, DEFAULT_MAX_NODES)
            .with_timeline(&frames, 2);
        assert_eq!(cut.timeline_span_count(), 2);
        assert_eq!(cut.timeline_truncated, 1);
        assert_eq!(cut.timeline[0].threads.len(), 1);
    }

    #[test]
    fn write_is_atomic_and_creates_the_folder() {
        // A unique directory per process and call (pid + counter); `regia_test_utils` depends on
        // this crate through `regia`, so it is not a dev-dependency here.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-profile-capture-{}-{n}", std::process::id()));
        let path = dir.join("captures/one.rprof.json");
        let capture = ProfileCapture::from_flame(&graph(), &[], DEFAULT_MAX_DEPTH, DEFAULT_MAX_NODES);
        capture.write(&path).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(ProfileCapture::from_json(&text).expect("parse"), capture);
        assert!(!path.with_extension("json.tmp").exists());
        let fingerprint = profile_capture_schema_fingerprint();
        assert!(fingerprint.contains("format_version") && fingerprint.contains("inclusive_ms"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
