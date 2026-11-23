// SPDX-License-Identifier: MPL-2.0
//! Synthetic capture sources for tests and the agent protocol
//! (`docs/specs/59-voice-chat.md` § 7). They never open a device.

use super::rng::Pcg32;

/// Converts dB relative to full scale to a linear gain.
fn db_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// A deterministic mono source.
#[derive(Clone, Debug)]
pub enum TestSource {
    /// Digital silence.
    Silence,
    /// A sine wave: phase is `2 pi f n / rate`, accumulated in f64 from the sample index.
    Sine {
        /// Frequency in Hz.
        freq_hz: f64,
        /// Linear amplitude.
        amplitude: f32,
        /// Sample index of the next sample.
        index: u64,
    },
    /// Uniform white noise from a seeded PCG32.
    Noise {
        /// Linear amplitude.
        amplitude: f32,
        /// Generator state.
        rng: Pcg32,
    },
}

impl TestSource {
    /// A sine source at `level_db` dBFS peak.
    #[must_use]
    pub fn sine(freq_hz: f64, level_db: f32) -> Self {
        Self::Sine {
            freq_hz,
            amplitude: db_gain(level_db),
            index: 0,
        }
    }

    /// A noise source at `level_db` dBFS peak.
    #[must_use]
    pub fn noise(seed: u64, level_db: f32) -> Self {
        Self::Noise {
            amplitude: db_gain(level_db),
            rng: Pcg32::new(seed),
        }
    }

    /// Fills `out` with the next samples at `sample_rate`.
    pub fn fill(&mut self, out: &mut [f32], sample_rate: u32) {
        match self {
            Self::Silence => out.fill(0.0),
            Self::Sine {
                freq_hz,
                amplitude,
                index,
            } => {
                for o in out.iter_mut() {
                    let phase = std::f64::consts::TAU * *freq_hz * (*index as f64) / f64::from(sample_rate);
                    *o = phase.sin() as f32 * *amplitude;
                    *index += 1;
                }
            }
            Self::Noise { amplitude, rng } => {
                for o in out.iter_mut() {
                    *o = (rng.next_f32() * 2.0 - 1.0) * *amplitude;
                }
            }
        }
    }
}
