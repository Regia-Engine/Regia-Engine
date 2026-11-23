// SPDX-License-Identifier: MIT-0
//! The soak timeline as a PNG (no font: the legend lives in the evidence page).
//!
//! Top panel (0 to 8 × p50, so plots compare by eye), one column per bucket of frames: wall time min–max (grey), GPU frame max
//! (blue), CPU `render_into` max (orange); horizontal lines at p50 (green) and 2 × p50 (red),
//! 1 ms grid (faint). A red mark on the top edge is a bucket holding a frame over 2 × p50;
//! frames above the panel are clipped there. Bottom strips: camera cuts (magenta),
//! time-of-day jumps (yellow), VFX/decal spawns (cyan), frames that created a pipeline,
//! bind group, buffer or texture (red), terrain uploads (white, height ∝ bytes).

use std::path::Path;

use crate::LookdevError;
use crate::chase::ChaseSample;
use crate::soak::{EVENT_CUT, EVENT_SPAWN, EVENT_TIME_OF_DAY};

const PANEL: u32 = 600;
const STRIP: u32 = 14;

struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(width: u32, height: u32, bg: [u8; 3]) -> Self {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            pixels.extend_from_slice(&[bg[0], bg[1], bg[2], 255]);
        }
        Self {
            width,
            height,
            pixels,
        }
    }

    fn put(&mut self, x: u32, y: u32, c: [u8; 3]) {
        if x < self.width && y < self.height {
            let i = ((y * self.width + x) * 4) as usize;
            self.pixels[i..i + 3].copy_from_slice(&c);
        }
    }

    fn vline(&mut self, x: u32, y0: u32, y1: u32, c: [u8; 3]) {
        for y in y0.min(y1)..=y0.max(y1) {
            self.put(x, y, c);
        }
    }

    fn hline(&mut self, y: u32, c: [u8; 3], dashed: bool) {
        for x in 0..self.width {
            if !dashed || (x / 6) % 2 == 0 {
                self.put(x, y, c);
            }
        }
    }
}

/// Writes the timeline of `samples` (one map's measured frames) to `path`, `width` columns.
///
/// # Errors
///
/// The PNG could not be written.
pub fn write_timeline(
    path: &Path,
    samples: &[ChaseSample],
    p50: f32,
    width: u32,
) -> Result<(), LookdevError> {
    let width = width.max(64);
    let strips = 5;
    let height = PANEL + strips * STRIP + 4;
    let mut canvas = Canvas::new(width, height, [18, 18, 22]);
    // A fixed 0..8 x p50 axis, so two timelines compare by eye; slower frames clip at the top.
    let y_max = (p50 * 8.0).max(1.0);
    let y_of = |ms: f32| -> u32 {
        let v = (ms / y_max).clamp(0.0, 1.0);
        (PANEL - 1) - (v * (PANEL - 1) as f32) as u32
    };
    let mut ms = 1.0;
    while ms < y_max {
        canvas.hline(y_of(ms), [40, 40, 48], false);
        ms += if y_max > 40.0 { 5.0 } else { 1.0 };
    }
    let n = samples.len().max(1);
    let max_upload = samples
        .iter()
        .map(|s| s.shared_upload.1)
        .max()
        .unwrap_or(0)
        .max(1);
    for x in 0..width {
        let a = x as usize * n / width as usize;
        let b = ((x as usize + 1) * n / width as usize).max(a + 1).min(n);
        let Some(bucket) = samples.get(a..b) else { continue };
        if bucket.is_empty() {
            continue;
        }
        let (lo, hi) = bucket.iter().fold((f32::MAX, 0.0f32), |(lo, hi), s| {
            (lo.min(s.wall_ms), hi.max(s.wall_ms))
        });
        canvas.vline(x, y_of(lo), y_of(hi), [150, 150, 150]);
        let gpu = bucket.iter().map(|s| s.gpu.frame_ms).fold(0.0f32, f32::max);
        let cpu = bucket.iter().map(|s| s.cpu_ms).fold(0.0f32, f32::max);
        canvas.put(x, y_of(gpu), [80, 140, 255]);
        canvas.put(x, y_of(cpu), [255, 160, 40]);
        if hi > 2.0 * p50 {
            canvas.vline(x, 0, 4, [255, 40, 40]);
        }
        let strip = |k: u32| PANEL + 2 + k * STRIP;
        let any = |bit: u8| bucket.iter().any(|s| s.events & bit != 0);
        if any(EVENT_CUT) {
            canvas.vline(x, strip(0), strip(0) + STRIP - 3, [230, 60, 230]);
        }
        if any(EVENT_TIME_OF_DAY) {
            canvas.vline(x, strip(1), strip(1) + STRIP - 3, [240, 220, 40]);
        }
        if any(EVENT_SPAWN) {
            canvas.vline(x, strip(2), strip(2) + STRIP - 3, [40, 220, 230]);
        }
        let created = bucket.iter().any(|s| {
            let c = s.counters;
            c.pipelines + c.bind_groups + c.buffers + c.textures > 0
        });
        if created {
            canvas.vline(x, strip(3), strip(3) + STRIP - 3, [255, 40, 40]);
        }
        let upload = bucket.iter().map(|s| s.shared_upload.1).max().unwrap_or(0);
        if upload > 0 {
            let h = ((upload as f32 / max_upload as f32) * (STRIP - 3) as f32).ceil() as u32;
            canvas.vline(
                x,
                strip(4) + STRIP - 3 - h.min(STRIP - 3),
                strip(4) + STRIP - 3,
                [230, 230, 230],
            );
        }
    }
    canvas.hline(y_of(p50), [60, 200, 90], false);
    canvas.hline(y_of(2.0 * p50), [230, 50, 50], true);
    crate::default_look::write_png(path, canvas.width, canvas.height, &canvas.pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spike_marks_the_top_edge_and_the_create_strip() {
        let mut samples = vec![
            ChaseSample {
                wall_ms: 4.0,
                ..ChaseSample::default()
            };
            200
        ];
        samples[100].wall_ms = 20.0;
        samples[100].counters.bind_groups = 3;
        let dir = std::env::temp_dir().join(format!("soak-plot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("t.png");
        write_timeline(&path, &samples, 4.0, 200).expect("writes");
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).expect("open")));
        let mut reader = decoder.read_info().expect("png");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("frame");
        let px = |x: u32, y: u32| {
            let i = ((y * info.width + x) * 4) as usize;
            [buf[i], buf[i + 1], buf[i + 2]]
        };
        assert_eq!(px(100, 1), [255, 40, 40]);
        assert_eq!(px(50, 1), [18, 18, 22]);
        assert_eq!(px(100, PANEL + 2 + 3 * STRIP + 2), [255, 40, 40]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
