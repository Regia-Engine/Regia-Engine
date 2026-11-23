// SPDX-License-Identifier: MPL-2.0
//! The v0.1.4 frame-summary sections that split a frame from a player's log alone:
//! `frame_breakdown_ms` (main-thread phases that sum to the frame), `top_systems` (the costliest
//! ECS systems), `churn` (creations by label, uploads by source, mesh cache growth) and `counts`
//! (entities, draws, lights, shadow casters). The hitch event gets the same phase split for its
//! frame.
//!
//! Everything here is preallocated in [`Breakdown::new`]; a frame only adds numbers. The label
//! table allocates a `String` the first time a label is seen in a window.

use std::time::Duration;

use regia_app::{FramePhases, PHASE_COUNT, PHASE_NAMES, Phase};
use regia_core::profiling::{
    MAX_HOST_SPANS, MAX_SYSTEMS, host_span_name, system_name, take_host_times, take_system_times,
};
use regia_render::{CreateRecord, FrameRenderer, UploadSplit, WindowPhases};
use serde_json::{Map, Value, json};

/// Systems listed in `top_systems`.
const TOP_SYSTEMS: usize = 10;
/// Buckets of the per-system frame-time histogram: bucket `i` holds values below
/// `2^((i + 1) / 2)` µs (about 46 ms for the last).
const SYS_BUCKETS: usize = 32;
/// Distinct creation labels a window keeps (later ones are counted as `other`).
const MAX_LABELS: usize = 48;
/// Labels listed in `churn.created_by_label`.
const TOP_LABELS: usize = 12;
/// Host sections listed per block in `host_spans`.
const TOP_HOST: usize = 32;
/// Frames of phase records kept for hitch events (they are emitted two frames late).
const RING: usize = 8;
/// Texture views listed per window in `texture_views.per_view` (later ones are summed only).
const MAX_TEXTURE_VIEWS: usize = 8;

/// One texture view's sums over a window: `(asset id, renders, CPU ms, GPU ms)`.
type ViewSums = (u64, u64, f64, f64);

/// Render-schedule split names (`WindowPhases` order).
const RENDER_SPLIT: [&str; 4] = ["acquire", "render_into", "tail", "present"];
/// `counts` names, in [`FrameCounts`] order.
const COUNT_NAMES: [&str; COUNTS] = [
    "mesh_instances",
    "drawn",
    "per_object_draws",
    "local_lights",
    "shadow_casters",
    "culled",
    // Skinned crowds (ADR-1368): skinned colour draws, and the draws they would take batched.
    "skinned_draws",
    "skinned_batches_possible",
];
/// Entries in `counts` (`COUNT_NAMES`).
const COUNTS: usize = 8;

/// Per-frame counts fed to [`Breakdown::frame`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FrameCounts {
    pub mesh_instances: u64,
    pub local_lights: u64,
}

struct LabelCount {
    kind: regia_render::FramePathKind,
    label: String,
    count: u64,
    bytes: u64,
}

pub(crate) struct Breakdown {
    // Phases.
    phase_sum: [f64; PHASE_COUNT],
    phase_frames: u64,
    fixed_steps: u64,
    last_phase_frame: u64,
    runner: Option<crate::LiveRunnerState>,
    prev_gap: bool,
    win_sum: [f64; 4],
    win_frames: u64,
    last_win_seq: u64,
    /// `(telemetry frame, its phases)` and `(telemetry frame, its render split)`.
    ring: [(u64, FramePhases); RING],
    win_ring: [(u64, WindowPhases); RING],
    // Systems.
    sys_scratch: Vec<(u32, Duration, u32)>,
    sys_sum_ns: Vec<u64>,
    sys_max_ns: Vec<u64>,
    sys_runs: Vec<u64>,
    sys_frames: Vec<u32>,
    sys_hist: Vec<[u32; SYS_BUCKETS]>,
    sys_touched: Vec<u32>,
    // Host sections (`regia_core::profiling::set_host_span_prefixes`).
    host_main: HostBlock,
    host_other: HostBlock,
    host_top_ns: u64,
    host_frames: u64,
    host_frame_top_ns: u64,
    // Churn.
    labels: Vec<LabelCount>,
    labels_other: u64,
    upload_at_window: UploadSplit,
    mesh_cache_at_window: Option<(usize, usize, usize)>,
    // Counts.
    count_sum: [u64; COUNTS],
    count_max: [u64; COUNTS],
    count_frames: u64,
    // Texture views (`regia_render::TextureViewStats`).
    last_view_seq: u64,
    view_frames: u64,
    views_sum: u64,
    views_max: u32,
    view_renders: u64,
    view_cpu_ms: f64,
    view_gpu_ms: f64,
    per_view: [ViewSums; MAX_TEXTURE_VIEWS],
    per_view_len: usize,
}

impl Breakdown {
    pub(crate) fn new() -> Self {
        Self {
            phase_sum: [0.0; PHASE_COUNT],
            phase_frames: 0,
            fixed_steps: 0,
            last_phase_frame: 0,
            runner: None,
            prev_gap: false,
            win_sum: [0.0; 4],
            win_frames: 0,
            last_win_seq: 0,
            ring: [(0, FramePhases::default()); RING],
            win_ring: [(0, WindowPhases::default()); RING],
            sys_scratch: Vec::with_capacity(512),
            sys_sum_ns: vec![0; MAX_SYSTEMS],
            sys_max_ns: vec![0; MAX_SYSTEMS],
            sys_runs: vec![0; MAX_SYSTEMS],
            sys_frames: vec![0; MAX_SYSTEMS],
            sys_hist: vec![[0; SYS_BUCKETS]; MAX_SYSTEMS],
            sys_touched: Vec::with_capacity(512),
            host_main: HostBlock::new(),
            host_other: HostBlock::new(),
            host_top_ns: 0,
            host_frames: 0,
            host_frame_top_ns: 0,
            labels: Vec::with_capacity(MAX_LABELS),
            labels_other: 0,
            upload_at_window: regia_render::upload_split(),
            mesh_cache_at_window: None,
            count_sum: [0; COUNTS],
            count_max: [0; COUNTS],
            count_frames: 0,
            last_view_seq: 0,
            view_frames: 0,
            views_sum: 0,
            views_max: 0,
            view_renders: 0,
            view_cpu_ms: 0.0,
            view_gpu_ms: 0.0,
            per_view: [(0, 0, 0.0, 0.0); MAX_TEXTURE_VIEWS],
            per_view_len: 0,
        }
    }

    /// Adds the newest `Render` schedule's texture-view numbers (once per publication).
    fn texture_views(&mut self) {
        regia_render::with_texture_view_stats(|stats| {
            if stats.seq == self.last_view_seq {
                return;
            }
            self.last_view_seq = stats.seq;
            self.view_frames += 1;
            self.views_sum += u64::from(stats.views);
            self.views_max = self.views_max.max(stats.views);
            self.view_renders += u64::from(stats.rendered);
            self.view_cpu_ms += f64::from(stats.cpu_ms);
            self.view_gpu_ms += f64::from(stats.gpu_ms);
            for &(id, cpu, gpu) in &stats.per_view {
                let known = self.per_view[..self.per_view_len].iter().position(|v| v.0 == id);
                let slot = match known {
                    Some(slot) => slot,
                    None if self.per_view_len < MAX_TEXTURE_VIEWS => {
                        self.per_view[self.per_view_len] = (id, 0, 0.0, 0.0);
                        self.per_view_len += 1;
                        self.per_view_len - 1
                    }
                    None => continue,
                };
                let entry = &mut self.per_view[slot];
                entry.1 += 1;
                entry.2 += f64::from(cpu);
                entry.3 += f64::from(gpu);
            }
        });
    }

    /// Adds telemetry frame `frame`. `phases` is the app's last completed frame (the one before
    /// this), so it is recorded as frame `frame - 1`; `gap` says this frame's interval was a gap.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn frame(
        &mut self,
        frame: u64,
        gap: bool,
        phases: Option<&FramePhases>,
        renderer: Option<&FrameRenderer>,
        counts: FrameCounts,
        created: &[CreateRecord],
    ) {
        // Main-thread phases: the record lags one frame, so a gap frame's own record arrives
        // with the next frame and is skipped then.
        let window = regia_render::window_phases();
        if let Some(p) = phases.filter(|p| p.frame != self.last_phase_frame) {
            self.last_phase_frame = p.frame;
            self.runner = p.runner.map(crate::LiveRunnerState::from);
            let of = frame.saturating_sub(1);
            self.ring[(of as usize) % RING] = (of, *p);
            if !self.prev_gap {
                for (sum, v) in self.phase_sum.iter_mut().zip(p.ms) {
                    *sum += f64::from(v);
                }
                self.fixed_steps += u64::from(p.fixed_steps);
                self.phase_frames += 1;
            }
        }
        self.prev_gap = gap;
        // The render split is this frame's (`Render` ran before telemetry in `Last`).
        if window.seq != self.last_win_seq {
            self.last_win_seq = window.seq;
            self.win_ring[(frame as usize) % RING] = (frame, window);
            if !gap {
                for (sum, v) in self.win_sum.iter_mut().zip([
                    window.acquire_ms,
                    window.render_into_ms,
                    window.tail_ms,
                    window.present_ms,
                ]) {
                    *sum += f64::from(v);
                }
                self.win_frames += 1;
            }
        }
        self.texture_views();
        // ECS systems (timed by the frame-profiler layer; nothing without it).
        take_system_times(&mut self.sys_scratch);
        for &(slot, time, runs) in &self.sys_scratch {
            let i = slot as usize;
            if i >= MAX_SYSTEMS {
                continue;
            }
            let ns = time.as_nanos() as u64;
            if self.sys_frames[i] == 0 {
                self.sys_touched.push(slot);
            }
            self.sys_sum_ns[i] += ns;
            self.sys_max_ns[i] = self.sys_max_ns[i].max(ns);
            self.sys_runs[i] += u64::from(runs);
            self.sys_frames[i] = self.sys_frames[i].saturating_add(1);
            let us = (ns as f64 / 1000.0) + 1.0;
            let bucket = ((2.0 * us.log2()) as usize).min(SYS_BUCKETS - 1);
            self.sys_hist[i][bucket] = self.sys_hist[i][bucket].saturating_add(1);
        }
        // Host sections: the main thread's, other threads' (a sim thread), and the main thread's
        // time inside top-level sections.
        let top = take_host_times(&mut self.host_main.scratch, &mut self.host_other.scratch);
        self.host_frame_top_ns = top.as_nanos() as u64;
        if !gap {
            self.host_main.add();
            self.host_other.add();
            self.host_top_ns += self.host_frame_top_ns;
            self.host_frames += 1;
        }
        // Creations by label.
        for c in created {
            let label = c.label();
            if let Some(entry) = self
                .labels
                .iter_mut()
                .find(|e| e.kind == c.kind && e.label == label)
            {
                entry.count += 1;
                entry.bytes += c.bytes;
            } else if self.labels.len() < MAX_LABELS {
                self.labels.push(LabelCount {
                    kind: c.kind,
                    label: label.to_owned(),
                    count: 1,
                    bytes: c.bytes,
                });
            } else {
                self.labels_other += 1;
            }
        }
        // Counts.
        if let Some(r) = renderer {
            if self.mesh_cache_at_window.is_none() {
                self.mesh_cache_at_window = Some(r.cache_sizes());
            }
            let d = r.draw_stats();
            let values = [
                counts.mesh_instances,
                d.drawn as u64,
                d.per_object as u64,
                counts.local_lights,
                r.shadow_caster_count() as u64,
                d.culled as u64,
                d.skinned_draws as u64,
                d.skinned_batches_possible as u64,
            ];
            for ((sum, max), v) in self.count_sum.iter_mut().zip(&mut self.count_max).zip(values) {
                *sum += v;
                *max = (*max).max(v);
            }
            self.count_frames += 1;
        }
    }

    /// The `n` costliest systems of the newest frame (the one [`Self::frame`] just added), for
    /// a hitch event.
    pub(crate) fn frame_top_systems(&self, n: usize) -> Value {
        let mut list: Vec<&(u32, Duration, u32)> = self.sys_scratch.iter().collect();
        list.sort_unstable_by_key(|a| std::cmp::Reverse(a.1));
        Value::Array(
            list.into_iter()
                .take(n)
                .map(|&(slot, time, runs)| {
                    json!({
                        "system": system_name(slot).unwrap_or("?"),
                        "ms": r3(time.as_secs_f64() * 1000.0),
                        "runs": runs,
                    })
                })
                .collect(),
        )
    }

    /// The newest frame's main-thread host sections (`n` largest) and their top-level total,
    /// for a hitch event. Null when the host emits none.
    pub(crate) fn frame_host_spans(&self, n: usize) -> Value {
        if self.host_main.scratch.is_empty() {
            return Value::Null;
        }
        let mut list: Vec<&(u32, Duration, u32)> = self.host_main.scratch.iter().collect();
        list.sort_unstable_by_key(|a| std::cmp::Reverse(a.1));
        json!({
            "top_level_ms": r3(self.host_frame_top_ns as f64 / 1e6),
            "sections": list
                .into_iter()
                .take(n)
                .map(|&(slot, time, _)| json!({
                    "span": host_span_name(slot).map_or("?", |(name, _)| name),
                    "ms": r3(time.as_secs_f64() * 1000.0),
                }))
                .collect::<Vec<_>>(),
        })
    }

    /// The phase split recorded for telemetry frame `frame`, for its hitch event.
    /// The main-thread phase record of telemetry frame `frame`'s update, once it arrived (it
    /// lags one frame).
    pub(crate) fn phases_of(&self, frame: u64) -> Option<FramePhases> {
        self.ring
            .iter()
            .find(|(f, p)| *f == frame && p.frame > 0)
            .map(|(_, p)| *p)
    }

    pub(crate) fn hitch_json(&self, frame: u64) -> Option<Value> {
        let (_, phases) = self.ring.iter().find(|(f, p)| *f == frame && p.frame > 0)?;
        let window = self
            .win_ring
            .iter()
            .find(|(f, w)| *f == frame && w.seq > 0)
            .map(|(_, w)| *w)
            .unwrap_or_default();
        let mut out = Map::new();
        for (name, v) in PHASE_NAMES.iter().zip(phases.ms) {
            out.insert((*name).to_owned(), json!(r2(f64::from(v))));
        }
        if window.seq > 0 {
            out.insert(
                "render_split".to_owned(),
                render_split(
                    [
                        window.acquire_ms,
                        window.render_into_ms,
                        window.tail_ms,
                        window.present_ms,
                    ]
                    .map(f64::from),
                    f64::from(phases.get(Phase::Render)),
                ),
            );
        }
        out.insert(
            "runner".to_owned(),
            json!(phases.runner.map(crate::LiveRunnerState::from)),
        );
        out.insert("sum_ms".to_owned(), json!(r2(f64::from(phases.total_ms()))));
        Some(Value::Object(out))
    }

    /// The main thread's wall time for telemetry frame `frame`, for its hitch event:
    /// `(frame_ms_wall, update_ms_wall)`. The first is every phase (end of the update before to
    /// the end of this one); the second leaves out the time between updates (`outside_update`,
    /// `host_schedules`, `pacing`), so it is `App::update` itself: the sim, extract and the render
    /// with its acquire and present waits.
    pub(crate) fn hitch_wall_ms(&self, frame: u64) -> Option<(f64, f64)> {
        let (_, phases) = self.ring.iter().find(|(f, p)| *f == frame && p.frame > 0)?;
        let frame_wall = f64::from(phases.total_ms());
        let between = [
            Phase::OutsideUpdate,
            Phase::HostSchedules,
            Phase::Pacing,
            Phase::EventWait,
        ]
        .map(|p| f64::from(phases.get(p)))
        .iter()
        .sum::<f64>();
        Some((frame_wall, (frame_wall - between).max(0.0)))
    }

    /// The four summary sections, then a reset for the next window. `frame_mean_ms` is the
    /// window's mean frame interval; `entities` is the spawned-entity count now.
    pub(crate) fn summary(
        &mut self,
        frame_mean_ms: f64,
        window_frames: u64,
        renderer: Option<&FrameRenderer>,
        entities: Option<(u32, u32)>,
    ) -> [(&'static str, Value); 6] {
        // frame_breakdown_ms
        let mut phases = Map::new();
        let n = self.phase_frames.max(1) as f64;
        let mut sum = 0.0;
        let mut render_mean = 0.0;
        for (i, name) in PHASE_NAMES.iter().enumerate() {
            let mean = self.phase_sum[i] / n;
            sum += mean;
            if i == Phase::Render as usize {
                render_mean = mean;
            }
            phases.insert((*name).to_owned(), json!(r2(mean)));
        }
        let breakdown = if self.phase_frames == 0 {
            Value::Null
        } else {
            let wn = self.win_frames.max(1) as f64;
            let split = self.win_sum.map(|s| s / wn);
            phases.insert(
                "render_split".to_owned(),
                if self.win_frames == 0 {
                    Value::Null
                } else {
                    render_split(split, render_mean)
                },
            );
            phases.insert(
                "fixed_steps_per_frame".to_owned(),
                json!(r2(self.fixed_steps as f64 / n)),
            );
            phases.insert("runner".to_owned(), json!(self.runner));
            phases.insert("sum_ms".to_owned(), json!(r2(sum)));
            phases.insert("frame_ms".to_owned(), json!(r2(frame_mean_ms)));
            phases.insert("unaccounted_ms".to_owned(), json!(r2(frame_mean_ms - sum)));
            phases.insert("frames".to_owned(), json!(self.phase_frames));
            Value::Object(phases)
        };

        // top_systems
        let frames = window_frames.max(1) as f64;
        let sums = &self.sys_sum_ns;
        self.sys_touched
            .sort_unstable_by_key(|&slot| std::cmp::Reverse(sums[slot as usize]));
        let top: Vec<Value> = self
            .sys_touched
            .iter()
            .take(TOP_SYSTEMS)
            .map(|&slot| {
                let i = slot as usize;
                let name = system_name(slot);
                json!({
                    "system": name.unwrap_or("?"),
                    "mean_ms": r3(self.sys_sum_ns[i] as f64 / 1e6 / frames),
                    "p95_ms": r3(hist_p95(&self.sys_hist[i], self.sys_frames[i]).min(self.sys_max_ns[i] as f64 / 1e6)),
                    "max_ms": r3(self.sys_max_ns[i] as f64 / 1e6),
                    "runs_per_frame": r2(self.sys_runs[i] as f64 / frames),
                })
            })
            .collect();
        let systems_total_ms: f64 = self
            .sys_touched
            .iter()
            .map(|&s| self.sys_sum_ns[s as usize] as f64)
            .sum::<f64>()
            / 1e6
            / frames;
        let top_systems = json!({
            "timed": self.sys_touched.len(),
            "all_systems_ms": r2(systems_total_ms),
            "top": top,
        });

        // churn
        self.labels
            .sort_unstable_by(|a, b| b.count.cmp(&a.count).then(b.bytes.cmp(&a.bytes)));
        let by_label: Vec<Value> = self
            .labels
            .iter()
            .take(TOP_LABELS)
            .map(|l| json!({ "kind": format!("{:?}", l.kind), "label": l.label, "count": l.count, "bytes": l.bytes }))
            .collect();
        let pipelines_by_label: Vec<Value> = self
            .labels
            .iter()
            .filter(|l| l.kind == regia_render::FramePathKind::Pipeline)
            .take(TOP_LABELS)
            .map(|l| json!({ "label": l.label, "count": l.count }))
            .collect();
        let up = regia_render::upload_split();
        let at = self.upload_at_window;
        let total = up.total_bytes - at.total_bytes;
        let belt = up.belt_bytes - at.belt_bytes;
        let texture = up.texture_bytes - at.texture_bytes;
        let mesh = up.mesh_bytes - at.mesh_bytes;
        let mesh_cache = renderer.map(FrameRenderer::cache_sizes);
        let growth = |now: Option<(usize, usize, usize)>, i: usize| -> Value {
            match (now, self.mesh_cache_at_window) {
                (Some(n), Some(s)) => {
                    let pick = |t: (usize, usize, usize)| [t.0, t.1, t.2][i] as i64;
                    json!(pick(n) - pick(s))
                }
                _ => Value::Null,
            }
        };
        let churn = json!({
            "created_by_label": by_label,
            "labels_not_kept": self.labels_other,
            "pipelines_by_label": pipelines_by_label,
            "upload_bytes": {
                "total": total,
                "per_frame": r2(total as f64 / frames),
                "belt": belt,
                "texture": texture,
                "mesh": mesh,
                "other_buffer": other_buffer_bytes(total, belt, texture, mesh),
            },
            "mesh_buffers_created": up.mesh_buffers - at.mesh_buffers,
            "cache": mesh_cache.map(|(m, mat, o)| json!({
                "meshes": m, "materials": mat, "objects": o,
                "meshes_growth": growth(mesh_cache, 0),
                "materials_growth": growth(mesh_cache, 1),
                "objects_growth": growth(mesh_cache, 2),
            })),
        });

        // counts
        let cn = self.count_frames.max(1) as f64;
        let mut counts = Map::new();
        for (i, name) in COUNT_NAMES.iter().enumerate() {
            counts.insert(
                (*name).to_owned(),
                json!({ "mean": r2(self.count_sum[i] as f64 / cn), "max": self.count_max[i] }),
            );
        }
        if let Some((spawned, slots)) = entities {
            counts.insert("entities".to_owned(), json!(spawned));
            counts.insert("entity_slots".to_owned(), json!(slots));
        }

        // host_spans
        let host_spans = if self.host_main.touched.is_empty() && self.host_other.touched.is_empty() {
            Value::Null
        } else {
            let hn = self.host_frames.max(1) as f64;
            let top_level = self.host_top_ns as f64 / 1e6 / hn;
            json!({
                "main": {
                    "sections": self.host_main.json(hn),
                    "top_level_ms": r2(top_level),
                    "frame_ms": r2(frame_mean_ms),
                    "unattributed_ms": r2(frame_mean_ms - top_level),
                },
                "other_threads": { "sections": self.host_other.json(hn) },
                "frames": self.host_frames,
            })
        };

        // texture_views (v0.1.6): render-to-texture cameras, per host frame and per view.
        let vn = self.view_frames.max(1) as f64;
        let per_view: Vec<Value> = self.per_view[..self.per_view_len]
            .iter()
            .map(|&(id, renders, cpu, gpu)| {
                let n = renders.max(1) as f64;
                json!({
                    "asset_id": id,
                    "renders": renders,
                    "cpu_ms_mean": r3(cpu / n),
                    "gpu_ms_mean": r3(gpu / n),
                })
            })
            .collect();
        let texture_views = json!({
            "views_mean": r2(self.views_sum as f64 / vn),
            "views_max": self.views_max,
            "renders_per_frame": r2(self.view_renders as f64 / vn),
            "cpu_ms_per_frame": r3(self.view_cpu_ms / vn),
            "gpu_ms_per_frame": r3(self.view_gpu_ms / vn),
            "frames": self.view_frames,
            "per_view": per_view,
        });

        // Reset.
        self.view_frames = 0;
        self.views_sum = 0;
        self.views_max = 0;
        self.view_renders = 0;
        self.view_cpu_ms = 0.0;
        self.view_gpu_ms = 0.0;
        self.per_view_len = 0;
        self.host_main.reset();
        self.host_other.reset();
        self.host_top_ns = 0;
        self.host_frames = 0;
        self.phase_sum = [0.0; PHASE_COUNT];
        self.phase_frames = 0;
        self.fixed_steps = 0;
        self.win_sum = [0.0; 4];
        self.win_frames = 0;
        for &slot in &self.sys_touched {
            let i = slot as usize;
            self.sys_sum_ns[i] = 0;
            self.sys_max_ns[i] = 0;
            self.sys_runs[i] = 0;
            self.sys_frames[i] = 0;
            self.sys_hist[i] = [0; SYS_BUCKETS];
        }
        self.sys_touched.clear();
        self.labels.clear();
        self.labels_other = 0;
        self.upload_at_window = up;
        self.mesh_cache_at_window = mesh_cache;
        self.count_sum = [0; COUNTS];
        self.count_max = [0; COUNTS];
        self.count_frames = 0;
        [
            ("frame_breakdown_ms", breakdown),
            ("top_systems", top_systems),
            ("churn", churn),
            ("counts", Value::Object(counts)),
            ("host_spans", host_spans),
            ("texture_views", texture_views),
        ]
    }
}

/// One thread group's host sections over a window.
struct HostBlock {
    scratch: Vec<(u32, Duration, u32)>,
    sum_ns: Vec<u64>,
    max_ns: Vec<u64>,
    calls: Vec<u64>,
    touched: Vec<u32>,
}

impl HostBlock {
    fn new() -> Self {
        Self {
            scratch: Vec::with_capacity(64),
            sum_ns: vec![0; MAX_HOST_SPANS],
            max_ns: vec![0; MAX_HOST_SPANS],
            calls: vec![0; MAX_HOST_SPANS],
            touched: Vec::with_capacity(64),
        }
    }

    /// Adds the frame in `scratch`.
    fn add(&mut self) {
        for &(slot, time, calls) in &self.scratch {
            let i = slot as usize;
            if i >= MAX_HOST_SPANS {
                continue;
            }
            // First touch: nothing recorded yet (a torn sample can carry time with zero calls).
            if self.calls[i] == 0 && self.sum_ns[i] == 0 {
                self.touched.push(slot);
            }
            let ns = time.as_nanos() as u64;
            self.sum_ns[i] += ns;
            self.max_ns[i] = self.max_ns[i].max(ns);
            self.calls[i] += u64::from(calls);
        }
    }

    /// `[{span, target, mean_ms, max_ms, calls_per_frame}]`, costliest first, over `frames`.
    fn json(&mut self, frames: f64) -> Value {
        let sums = &self.sum_ns;
        self.touched
            .sort_unstable_by_key(|&slot| std::cmp::Reverse(sums[slot as usize]));
        Value::Array(
            self.touched
                .iter()
                .take(TOP_HOST)
                .map(|&slot| {
                    let i = slot as usize;
                    let (name, target) = host_span_name(slot).unwrap_or(("?", "?"));
                    json!({
                        "span": name,
                        "target": target,
                        "mean_ms": r3(self.sum_ns[i] as f64 / 1e6 / frames),
                        "max_ms": r3(self.max_ns[i] as f64 / 1e6),
                        "calls_per_frame": r2(self.calls[i] as f64 / frames),
                    })
                })
                .collect(),
        )
    }

    fn reset(&mut self) {
        for &slot in &self.touched {
            let i = slot as usize;
            self.sum_ns[i] = 0;
            self.max_ns[i] = 0;
            self.calls[i] = 0;
        }
        self.touched.clear();
    }
}

/// `{acquire, render_into, tail, present, other}`: the `Render` schedule split, with the rest of
/// the schedule's systems as `other`.
fn render_split(split: [f64; 4], render_ms: f64) -> Value {
    let mut out = Map::new();
    for (name, v) in RENDER_SPLIT.iter().zip(split) {
        out.insert((*name).to_owned(), json!(r2(v)));
    }
    out.insert(
        "other".to_owned(),
        json!(r2(render_ms - split.iter().sum::<f64>())),
    );
    Value::Object(out)
}

/// Upload bytes in none of the named sources: a signed remainder, so a source counted twice (a
/// negative value) stays visible instead of reading as zero.
fn other_buffer_bytes(total: u64, belt: u64, texture: u64, mesh: u64) -> i64 {
    let named = i128::from(belt) + i128::from(texture) + i128::from(mesh);
    (i128::from(total) - named).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// The 95th percentile (bucket upper edge, ms) of a per-system histogram of `n` frames.
fn hist_p95(hist: &[u32; SYS_BUCKETS], n: u32) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let want = (f64::from(n) * 0.95).ceil() as u64;
    let mut seen = 0u64;
    for (i, &c) in hist.iter().enumerate() {
        seen += u64::from(c);
        if seen >= want {
            let us = 2f64.powf((i + 1) as f64 / 2.0) - 1.0;
            return us / 1000.0;
        }
    }
    2f64.powf(SYS_BUCKETS as f64 / 2.0) / 1000.0
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p95_bucket_edges_bound_the_value() {
        let mut hist = [0u32; SYS_BUCKETS];
        // 100 frames at ~1 ms (1000 µs → bucket floor(2·log2(1001)) = 19).
        let bucket = ((2.0 * 1001f64.log2()) as usize).min(SYS_BUCKETS - 1);
        hist[bucket] = 100;
        let p95 = hist_p95(&hist, 100);
        assert!(
            (p95 - (2f64.powf(10.0) - 1.0) / 1000.0).abs() < 1e-12,
            "bucket 19 upper edge, got {p95}"
        );
    }

    #[test]
    fn p95_does_not_saturate_on_a_long_window() {
        let mut hist = [0u32; SYS_BUCKETS];
        hist[10] = 100_000;
        hist[20] = 1;
        let p95 = hist_p95(&hist, 100_001);
        assert!(
            (p95 - (2f64.powf(5.5) - 1.0) / 1000.0).abs() < 1e-12,
            "p95 {p95} must be bucket 10's upper edge"
        );
    }

    #[test]
    fn other_buffer_is_a_signed_remainder() {
        assert_eq!(other_buffer_bytes(100, 40, 30, 20), 10);
        assert_eq!(other_buffer_bytes(100_000, 100_000, 0, 100_000), -100_000);
    }

    #[test]
    fn phases_sum_to_the_frame_and_the_remainder_is_explicit() {
        let mut b = Breakdown::new();
        for f in 1..=10u64 {
            let mut p = FramePhases {
                frame: f,
                ..FramePhases::default()
            };
            p.ms[Phase::Update as usize] = 6.0;
            p.ms[Phase::Render as usize] = 3.0;
            p.ms[Phase::OutsideUpdate as usize] = 1.0;
            b.frame(f + 1, false, Some(&p), None, FrameCounts::default(), &[]);
        }
        let [(name, breakdown), ..] = b.summary(11.0, 10, None, None);
        assert_eq!(name, "frame_breakdown_ms");
        assert_eq!(breakdown["update"], json!(6.0));
        assert_eq!(breakdown["sum_ms"], json!(10.0));
        assert_eq!(breakdown["unaccounted_ms"], json!(1.0));
        assert_eq!(breakdown["frames"], json!(10));
        // The window resets.
        let [(_, empty), ..] = b.summary(11.0, 0, None, None);
        assert_eq!(empty, Value::Null);
    }

    #[test]
    fn a_hitch_frame_finds_its_phase_record() {
        let mut b = Breakdown::new();
        let mut p = FramePhases {
            frame: 41,
            ..FramePhases::default()
        };
        p.ms[Phase::Update as usize] = 33.0;
        // Telemetry frame 42 sees the app's record of the frame before it.
        b.frame(42, false, Some(&p), None, FrameCounts::default(), &[]);
        let hitch = b.hitch_json(41).expect("record for frame 41");
        assert_eq!(hitch["update"], json!(33.0));
        assert!(b.hitch_json(42).is_none());
    }

    #[test]
    fn hitch_wall_times_split_inside_and_between_updates() {
        let mut b = Breakdown::new();
        let mut p = FramePhases {
            frame: 9,
            ..FramePhases::default()
        };
        p.ms[Phase::OutsideUpdate as usize] = 28.0;
        p.ms[Phase::Pacing as usize] = 2.0;
        p.ms[Phase::Update as usize] = 20.0;
        p.ms[Phase::Render as usize] = 16.0;
        b.frame(10, false, Some(&p), None, FrameCounts::default(), &[]);
        assert_eq!(b.hitch_wall_ms(9), Some((66.0, 36.0)));
        assert_eq!(b.hitch_wall_ms(10), None);
    }
}

#[cfg(test)]
mod edge_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_torn_host_sample_is_listed_once() {
        let mut block = HostBlock::new();
        // Time swapped out, call count not yet: calls == 0 but time > 0.
        block.scratch.push((3, Duration::from_micros(50), 0));
        block.add();
        block.scratch.clear();
        block.scratch.push((3, Duration::from_micros(50), 1));
        block.add();
        assert_eq!(block.touched, vec![3]);
    }
}
