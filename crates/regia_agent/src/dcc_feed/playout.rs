// SPDX-License-Identifier: MPL-2.0
//! Snap and delayed playout buffers (`specs/60` § 2.6).
//!
//! `snap` applies the newest frame. `delayed` evaluates a fixed delay behind the newest sample and
//! interpolates between the two bracketing samples: translation, scale and scalar values
//! linearly, rotations with normalized shortest-path slerp. Time is a plain `f64` in seconds the
//! caller owns (fixed-step time in replay), so nothing here reads a clock.

use std::collections::VecDeque;

/// One sampled transform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Frame id; non-increasing ids are dropped.
    pub frame_id: u64,
    /// Time this sample is valid at, in seconds on the receiver's timeline.
    pub time_s: f64,
    /// Translation.
    pub t: [f32; 3],
    /// Unit quaternion x, y, z, w.
    pub r: [f32; 4],
    /// Scale.
    pub s: [f32; 3],
}

/// Counters of one buffer.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlayoutCounters {
    /// Frames dropped for a non-increasing `frame_id`.
    pub dropped_out_of_order: u64,
    /// Frames evicted for age.
    pub evicted_age: u64,
    /// Frames evicted for the queue cap.
    pub evicted_cap: u64,
}

/// A per-item sample queue.
#[derive(Debug, Clone)]
pub struct PlayoutBuffer {
    samples: VecDeque<Sample>,
    last_frame_id: Option<u64>,
    cap: usize,
    max_age_s: f64,
    counters: PlayoutCounters,
}

impl PlayoutBuffer {
    /// A buffer keeping at most `cap` (>= 1) samples no older than `max_age_s` seconds.
    #[must_use]
    pub fn new(cap: usize, max_age_s: f64) -> Self {
        Self {
            samples: VecDeque::new(),
            last_frame_id: None,
            cap: cap.max(1),
            max_age_s,
            counters: PlayoutCounters::default(),
        }
    }

    /// The counters.
    #[must_use]
    pub fn counters(&self) -> PlayoutCounters {
        self.counters
    }

    /// Samples currently queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether no sample is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Adds a sample; returns false (and counts it) when its `frame_id` does not increase.
    pub fn push(&mut self, s: Sample) -> bool {
        if self.last_frame_id.is_some_and(|last| s.frame_id <= last) {
            self.counters.dropped_out_of_order += 1;
            return false;
        }
        self.last_frame_id = Some(s.frame_id);
        self.samples.push_back(s);
        while self.samples.len() > self.cap {
            self.samples.pop_front();
            self.counters.evicted_cap += 1;
        }
        true
    }

    /// Evicts samples older than the age limit relative to `now_s`, always keeping the newest
    /// sample at or before `now_s - max_age` so a late evaluation still has a bracket start.
    pub fn evict(&mut self, now_s: f64) {
        let cutoff = now_s - self.max_age_s;
        while self.samples.len() > 1 && self.samples[1].time_s <= cutoff {
            self.samples.pop_front();
            self.counters.evicted_age += 1;
        }
    }

    /// The newest sample (`snap` playout).
    #[must_use]
    pub fn newest(&self) -> Option<Sample> {
        self.samples.back().copied()
    }

    /// The value at `t_s`: the bracketing pair interpolated; before the first sample the first
    /// sample; after the last sample the last.
    #[must_use]
    pub fn evaluate(&self, t_s: f64) -> Option<Sample> {
        let first = self.samples.front()?;
        if t_s <= first.time_s {
            return Some(*first);
        }
        let last = self.samples.back()?;
        if t_s >= last.time_s {
            return Some(*last);
        }
        let hi = self.samples.iter().position(|s| s.time_s >= t_s)?;
        let (a, b) = (self.samples[hi - 1], self.samples[hi]);
        let span = b.time_s - a.time_s;
        let f = if span > 0.0 {
            ((t_s - a.time_s) / span) as f32
        } else {
            1.0
        };
        Some(Sample {
            frame_id: a.frame_id,
            time_s: t_s,
            t: lerp3(a.t, b.t, f),
            r: slerp(a.r, b.r, f),
            s: lerp3(a.s, b.s, f),
        })
    }
}

/// Linear interpolation of three floats.
#[must_use]
pub fn lerp3(a: [f32; 3], b: [f32; 3], f: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * f,
        a[1] + (b[1] - a[1]) * f,
        a[2] + (b[2] - a[2]) * f,
    ]
}

/// Normalized shortest-path spherical interpolation of unit quaternions (x, y, z, w).
#[must_use]
pub fn slerp(a: [f32; 4], mut b: [f32; 4], f: f32) -> [f32; 4] {
    let mut dot: f64 = (0..4).map(|i| f64::from(a[i]) * f64::from(b[i])).sum();
    if dot < 0.0 {
        dot = -dot;
        for x in &mut b {
            *x = -*x;
        }
    }
    let f = f64::from(f);
    let (wa, wb) = if dot > 0.9995 {
        (1.0 - f, f) // nearly parallel: normalized lerp
    } else {
        let theta = dot.clamp(-1.0, 1.0).acos();
        let sin = theta.sin();
        (((1.0 - f) * theta).sin() / sin, (f * theta).sin() / sin)
    };
    let mut out = [0.0f64; 4];
    for i in 0..4 {
        out[i] = wa * f64::from(a[i]) + wb * f64::from(b[i]);
    }
    let len = out.iter().map(|x| x * x).sum::<f64>().sqrt();
    if len < 1e-12 {
        return a;
    }
    [
        (out[0] / len) as f32,
        (out[1] / len) as f32,
        (out[2] / len) as f32,
        (out[3] / len) as f32,
    ]
}
