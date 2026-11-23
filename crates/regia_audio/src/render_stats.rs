// SPDX-License-Identifier: MPL-2.0
//! Measurements of a rendered stereo buffer (ADR-544): level, clipping, non-finite samples and
//! the largest sample-to-sample step, the simplest click detector. Used by the offline
//! correctness tests and the `audio.render_check` agent method.
//!
//! A click is a discontinuity: a waveform that jumps instead of moving. For a sine of amplitude
//! `a` and frequency `f` the largest step between two samples is `2π f a / rate` (0.029 for
//! 440 Hz at 0.5 and 48 kHz); a sound cut off mid-cycle steps by up to `a`. Comparing
//! [`RenderStats::max_step`] with the step the content itself can make tells them apart.

use crate::spatial::gain_to_db;

/// Sample magnitude counted as clipped: within 0.01 dB of full scale.
pub const CLIP_LEVEL: f32 = 0.9989;

/// Level and integrity of one rendered buffer, per channel where it matters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderStats {
    /// Stereo frames measured.
    pub frames: usize,
    /// Highest left-channel magnitude.
    pub peak_left: f32,
    /// Highest right-channel magnitude.
    pub peak_right: f32,
    /// Left-channel RMS.
    pub rms_left: f32,
    /// Right-channel RMS.
    pub rms_right: f32,
    /// Largest step between two consecutive samples of one channel.
    pub max_step: f32,
    /// Frame index (from the start of the buffer) where [`RenderStats::max_step`] happens.
    pub max_step_frame: usize,
    /// Samples at or above [`CLIP_LEVEL`].
    pub clipped_samples: usize,
    /// NaN or infinite samples (they are skipped by every other measurement).
    pub nonfinite_samples: usize,
}

impl RenderStats {
    /// Measures interleaved stereo (`[left, right, left, right, …]`, as
    /// [`crate::Audio::render_offline`] returns it). A trailing odd sample is ignored.
    pub fn measure(interleaved: &[f32]) -> Self {
        let (frames, _) = interleaved.as_chunks::<2>();
        let mut stats = Self {
            frames: frames.len(),
            ..Self::default()
        };
        let mut sum = [0.0f64; 2];
        let mut previous: Option<[f32; 2]> = None;
        for (index, frame) in frames.iter().enumerate() {
            // A non-finite sample holds the channel's previous value, so it is neither a peak
            // nor a step.
            let mut clean = previous.unwrap_or([0.0; 2]);
            for (channel, &sample) in frame.iter().enumerate() {
                if !sample.is_finite() {
                    stats.nonfinite_samples += 1;
                    continue;
                }
                clean[channel] = sample;
                let magnitude = sample.abs();
                if magnitude >= CLIP_LEVEL {
                    stats.clipped_samples += 1;
                }
                sum[channel] += f64::from(sample) * f64::from(sample);
            }
            stats.peak_left = stats.peak_left.max(clean[0].abs());
            stats.peak_right = stats.peak_right.max(clean[1].abs());
            if let Some(previous) = previous {
                let step = (clean[0] - previous[0]).abs().max((clean[1] - previous[1]).abs());
                if step > stats.max_step {
                    stats.max_step = step;
                    stats.max_step_frame = index;
                }
            }
            previous = Some(clean);
        }
        let n = frames.len().max(1) as f64;
        stats.rms_left = (sum[0] / n).sqrt() as f32;
        stats.rms_right = (sum[1] / n).sqrt() as f32;
        stats
    }

    /// The louder channel's peak in dBFS (−100 for silence).
    pub fn peak_db(&self) -> f32 {
        gain_to_db(self.peak_left.max(self.peak_right))
    }

    /// The RMS of both channels together, in dBFS (−100 for silence).
    pub fn rms_db(&self) -> f32 {
        let power = (self.rms_left * self.rms_left + self.rms_right * self.rms_right) * 0.5;
        gain_to_db(power.sqrt())
    }

    /// True when nothing is clipped or non-finite.
    pub fn is_clean(&self) -> bool {
        self.clipped_samples == 0 && self.nonfinite_samples == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo(left: impl Fn(usize) -> f32, right: impl Fn(usize) -> f32, frames: usize) -> Vec<f32> {
        (0..frames).flat_map(|i| [left(i), right(i)]).collect()
    }

    #[test]
    fn a_sine_measures_its_peak_rms_and_slope() {
        let w = 440.0 * std::f32::consts::TAU / 48_000.0;
        let buffer = stereo(|i| (i as f32 * w).sin() * 0.5, |_| 0.0, 48_000);
        let stats = RenderStats::measure(&buffer);
        assert_eq!(stats.frames, 48_000);
        assert!((stats.peak_left - 0.5).abs() < 1e-3);
        assert_eq!(stats.peak_right, 0.0);
        assert!((stats.rms_left - 0.5 / 2f32.sqrt()).abs() < 1e-3);
        assert!(
            (stats.max_step - w * 0.5).abs() < 1e-3,
            "a sine's steepest step is 2πfa/rate: {}",
            stats.max_step
        );
        assert!(stats.is_clean());
        assert!((stats.peak_db() + 6.02).abs() < 0.02);
        // Left at −9.03 dB RMS, right silent: −12.04 dB together.
        assert!((stats.rms_db() + 12.04).abs() < 0.05, "{}", stats.rms_db());
    }

    #[test]
    fn a_cut_is_a_step_and_overs_and_nan_are_counted() {
        let buffer = stereo(
            |i| if i < 100 { 0.5 } else { 0.0 },
            |i| if i == 7 { f32::NAN } else { 1.0 },
            200,
        );
        let stats = RenderStats::measure(&buffer);
        assert!((stats.max_step - 0.5).abs() < 1e-6);
        assert_eq!(stats.max_step_frame, 100);
        assert_eq!(stats.nonfinite_samples, 1);
        assert_eq!(
            stats.clipped_samples, 199,
            "every finite right sample is at full scale"
        );
        assert!(!stats.is_clean());
        assert_eq!(RenderStats::measure(&[]), RenderStats::default());
        assert_eq!(
            RenderStats::measure(&[0.25]).frames,
            0,
            "a lone sample is not a frame"
        );
    }
}
