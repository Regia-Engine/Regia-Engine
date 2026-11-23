// SPDX-License-Identifier: MIT-0
//! The measured frames-per-second overlay: the wall time between two frames (`Last` to `Last`), over a rolling
//! window. Nothing is estimated: before the first two frames the overlay says so.

use std::collections::VecDeque;
use std::time::Instant;

use regia::prelude::*;
use serde_json::{Value, json};

/// Frames the rolling window holds (about two seconds at 60 fps).
pub const WINDOW_FRAMES: usize = 120;

/// A frame rate for a person: whole numbers, one decimal below 10 fps (a slow machine's `0.6` is not `1`, and its
/// 1 % low is not `0`).
#[must_use]
pub fn fps_text(fps: f64) -> String {
    if fps < 10.0 {
        format!("{fps:.1}")
    } else {
        format!("{fps:.0}")
    }
}

/// Where frame times come from.
#[derive(Resource, Debug, Default)]
pub enum FrameClock {
    /// The wall clock between two `Last` stages (the running app).
    #[default]
    Wall,
    /// A fixed frame time in milliseconds (tests and scripts that need repeatable numbers).
    Fixed(f64),
}

/// The rolling frame-time window and the last frame's time.
#[derive(Resource, Debug, Default)]
pub struct FpsMeter {
    last: Option<Instant>,
    window: VecDeque<f64>,
    /// The last frame's time, milliseconds (`None` before the second frame).
    pub last_ms: Option<f64>,
    /// Frames measured since the demo started.
    pub frames: u64,
}

/// What the overlay shows, from the rolling window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FpsReading {
    /// Frames over the window's total time.
    pub fps: f64,
    /// The window's average frame time, milliseconds.
    pub avg_ms: f64,
    /// The window's slowest frame, milliseconds.
    pub worst_ms: f64,
    /// The 1 % low: the frame rate of the 99th percentile frame time (nearest rank).
    pub low_1pct_fps: f64,
    /// Frames in the window.
    pub frames: usize,
}

impl FpsMeter {
    /// Records one frame time in milliseconds (non-finite or non-positive times are dropped).
    pub fn push(&mut self, ms: f64) {
        if !ms.is_finite() || ms <= 0.0 {
            return;
        }
        if self.window.len() == WINDOW_FRAMES {
            self.window.pop_front();
        }
        self.window.push_back(ms);
        self.last_ms = Some(ms);
        self.frames += 1;
    }

    /// The window's reading, `None` while it is empty.
    #[must_use]
    pub fn reading(&self) -> Option<FpsReading> {
        if self.window.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = self.window.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let total: f64 = sorted.iter().sum();
        let n = sorted.len();
        let p99 = sorted[(((n - 1) as f64 * 0.99).round() as usize).min(n - 1)];
        Some(FpsReading {
            fps: 1000.0 * n as f64 / total,
            avg_ms: total / n as f64,
            worst_ms: sorted[n - 1],
            low_1pct_fps: 1000.0 / p99,
            frames: n,
        })
    }

    /// The overlay line: `60 fps (16.7 ms, 1 % low 48 fps)`, or `measuring…` before the first frame time.
    #[must_use]
    pub fn overlay_text(&self) -> String {
        match self.reading() {
            Some(r) => format!(
                "{} fps ({:.1} ms, 1 % low {} fps)",
                fps_text(r.fps),
                r.avg_ms,
                fps_text(r.low_1pct_fps)
            ),
            None => "measuring frame rate".to_owned(),
        }
    }

    /// The reading as JSON (`demo.state`'s `fps`).
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self.reading() {
            Some(r) => json!({
                "measured": true,
                "fps": r.fps,
                "avg_ms": r.avg_ms,
                "worst_ms": r.worst_ms,
                "low_1pct_fps": r.low_1pct_fps,
                "window_frames": r.frames,
                "last_ms": self.last_ms,
                "frames": self.frames,
                "text": self.overlay_text(),
            }),
            None => json!({ "measured": false, "frames": self.frames, "text": self.overlay_text() }),
        }
    }
}

/// `Last`: measures this frame's time and records it.
pub fn measure(clock: Res<FrameClock>, mut meter: ResMut<FpsMeter>) {
    let ms = match *clock {
        FrameClock::Fixed(ms) => Some(ms),
        FrameClock::Wall => {
            let now = Instant::now();
            let ms = meter
                .last
                .map(|last| now.duration_since(last).as_secs_f64() * 1000.0);
            meter.last = Some(now);
            ms
        }
    };
    if let Some(ms) = ms {
        meter.push(ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reading_is_the_harmonic_rate_of_the_window() {
        let mut meter = FpsMeter::default();
        assert!(meter.reading().is_none());
        assert_eq!(meter.overlay_text(), "measuring frame rate");
        for _ in 0..98 {
            meter.push(10.0);
        }
        meter.push(50.0);
        meter.push(50.0);
        let Some(r) = meter.reading() else {
            panic!("reading");
        };
        assert_eq!(r.frames, 100);
        assert!((r.fps - 1000.0 * 100.0 / 1080.0).abs() < 1e-9);
        assert!((r.worst_ms - 50.0).abs() < 1e-9);
        assert!(
            (r.low_1pct_fps - 20.0).abs() < 1e-9,
            "rank 98.01 rounds to index 98, one of the two slow frames"
        );
        assert!(
            meter.overlay_text().starts_with("93 fps (10.8 ms"),
            "{}",
            meter.overlay_text()
        );
        assert_eq!(fps_text(59.6), "60");
        assert_eq!(fps_text(0.56), "0.6");
        // One slow frame in a hundred is above the 99th percentile's nearest rank.
        let mut one = FpsMeter::default();
        for _ in 0..99 {
            one.push(10.0);
        }
        one.push(50.0);
        assert!(
            one.reading()
                .is_some_and(|r| (r.low_1pct_fps - 100.0).abs() < 1e-9)
        );
    }

    #[test]
    fn the_window_rolls_and_drops_bad_times() {
        let mut meter = FpsMeter::default();
        for _ in 0..(WINDOW_FRAMES + 30) {
            meter.push(20.0);
        }
        meter.push(f64::NAN);
        meter.push(0.0);
        let Some(r) = meter.reading() else {
            panic!("reading");
        };
        assert_eq!(r.frames, WINDOW_FRAMES);
        assert_eq!(meter.frames, (WINDOW_FRAMES + 30) as u64);
        assert!((r.fps - 50.0).abs() < 1e-9);
    }
}
