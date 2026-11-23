// SPDX-License-Identifier: MPL-2.0
//! Automatic gain for voice chat (`docs/specs/59-voice-chat.md` § 2.7 step 4, ADR-2608).
//!
//! A frame-rate digital gain toward [`AGC_TARGET_DB`] RMS, between [`AGC_MIN_GAIN_DB`] and
//! [`AGC_MAX_GAIN_DB`]. Loud speech pulls the gain down fast (the *attack*, [`AGC_ATTACK_DB`] per
//! 20 ms frame); quiet speech lets it rise slowly (the *release*, [`AGC_RELEASE_DB`] per frame),
//! the classic asymmetric compressor-style level control.
//!
//! **Noise gate**: the gain only adapts while the frame is speech, which here means its level is
//! above both [`AGC_GATE_DB`] and the tracked noise floor plus [`AGC_GATE_MARGIN_DB`] (the floor
//! follows quiet frames down at once and rises slowly, a minimum follower; it starts at 0 dBFS, so
//! the first pause sets it and speech before any pause is not boosted; frames of digital silence
//! do not move it; after 3 s of gate open without a break it rises ten times faster, so a step up
//! in room noise is boosted for seconds, not minutes). While the gate is closed
//! the applied gain is capped at 0 dB, so silence and room noise are never boosted.
//!
//! The applied gain moves linearly across each frame (no zipper steps), and a hard limiter caps
//! peaks at [`AGC_LIMIT`]: the frame's gain is lowered so its peak fits, and anything a ramp still
//! pushes over is clamped.

/// Target speech level, dBFS RMS.
pub const AGC_TARGET_DB: f32 = -20.0;
/// Largest boost, dB.
pub const AGC_MAX_GAIN_DB: f32 = 18.0;
/// Largest cut, dB.
pub const AGC_MIN_GAIN_DB: f32 = -18.0;
/// Gain fall per 20 ms frame when speech is too loud, dB (attack: 10 dB in 100 ms).
pub const AGC_ATTACK_DB: f32 = 2.0;
/// Gain rise per 20 ms frame when speech is too quiet, dB (release: 7.5 dB/s).
pub const AGC_RELEASE_DB: f32 = 0.15;
/// Absolute gate: frames below this level never count as speech, dBFS.
pub const AGC_GATE_DB: f32 = -55.0;
/// Frames must be this far above the tracked noise floor to count as speech, dB.
pub const AGC_GATE_MARGIN_DB: f32 = 9.0;
/// Limiter ceiling, linear peak (-1 dBFS).
pub const AGC_LIMIT: f32 = 0.891;
/// Noise floor rise per frame, dB (it falls to a quieter frame at once).
const FLOOR_RISE_DB: f32 = 0.05;
/// Frames at or below this level are digital silence (a stream start, a mute toggle) and do not
/// move the noise floor, which would otherwise take half a minute to climb back.
const SILENCE_DB: f32 = -90.0;
/// Frames the gate may stay open without a break before the floor is taken to have moved up (a
/// fan switched on): 3 s, longer than a talker goes without a pause.
const OPEN_RUN_FRAMES: u32 = 150;
/// Noise floor rise per frame after [`OPEN_RUN_FRAMES`] of open gate, dB.
const FLOOR_FAST_RISE_DB: f32 = 0.5;

/// Counters and state of an [`AutoGain`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AgcStats {
    /// Frames processed.
    pub frames: u64,
    /// Frames the gate held closed.
    pub gated_frames: u64,
    /// Frames the limiter touched.
    pub limited_frames: u64,
    /// The adaptive gain, dB.
    pub gain_db: f32,
    /// The gain applied at the end of the last frame, dB.
    pub applied_db: f32,
    /// The tracked noise floor, dBFS.
    pub noise_floor_db: f32,
    /// Whether the gate was open on the last frame.
    pub gate_open: bool,
}

/// The automatic gain stage.
#[derive(Clone, Debug)]
pub struct AutoGain {
    gain_db: f32,
    applied: f32,
    floor_db: f32,
    open_run: u32,
    stats: AgcStats,
}

impl Default for AutoGain {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoGain {
    /// A stage at 0 dB whose noise floor is not known yet (it starts at full scale and falls to
    /// the first quiet frame).
    #[must_use]
    pub fn new() -> Self {
        Self {
            gain_db: 0.0,
            applied: 1.0,
            floor_db: 0.0,
            open_run: 0,
            stats: AgcStats::default(),
        }
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> AgcStats {
        self.stats
    }

    /// Clears the counters (the gain and floor stay).
    pub fn reset_stats(&mut self) {
        self.stats = AgcStats {
            gain_db: self.stats.gain_db,
            applied_db: self.stats.applied_db,
            noise_floor_db: self.stats.noise_floor_db,
            gate_open: self.stats.gate_open,
            ..AgcStats::default()
        };
    }

    /// Levels one frame in place.
    pub fn process(&mut self, frame: &mut [f32]) {
        self.process_held(frame, false);
    }

    /// Levels one frame in place; `hold` keeps the gate shut for this frame whatever its level
    /// (the chain sets it during far-end single talk, so residual echo is never boosted).
    pub fn process_held(&mut self, frame: &mut [f32], hold: bool) {
        if frame.is_empty() {
            return;
        }
        self.stats.frames += 1;
        let level = super::transmit::frame_rms_db(frame);
        // Minimum follower: down at once, up slowly (faster once the gate has been open for
        // longer than anyone talks without a pause); digital silence is not a noise floor.
        if level > SILENCE_DB {
            let rise = if self.open_run > OPEN_RUN_FRAMES {
                FLOOR_FAST_RISE_DB
            } else {
                FLOOR_RISE_DB
            };
            self.floor_db = if level < self.floor_db {
                level
            } else {
                (self.floor_db + rise).min(level)
            };
        }
        let open = !hold && level > AGC_GATE_DB && level > self.floor_db + AGC_GATE_MARGIN_DB;
        self.open_run = if open { self.open_run.saturating_add(1) } else { 0 };
        if open {
            let wanted = (AGC_TARGET_DB - level).clamp(AGC_MIN_GAIN_DB, AGC_MAX_GAIN_DB);
            if wanted < self.gain_db {
                self.gain_db = (self.gain_db - AGC_ATTACK_DB).max(wanted);
            } else {
                self.gain_db = (self.gain_db + AGC_RELEASE_DB).min(wanted);
            }
        } else {
            self.stats.gated_frames += 1;
        }
        let gain_db = if open { self.gain_db } else { self.gain_db.min(0.0) };
        let mut target = 10f32.powf(gain_db / 20.0);
        let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let mut limited = false;
        if peak * target > AGC_LIMIT {
            target = AGC_LIMIT / peak;
            limited = true;
            // Keep the adaptive gain from pushing into the limiter again next frame.
            self.gain_db = self.gain_db.min(20.0 * target.log10());
        }
        let start = self.applied;
        let step = (target - start) / frame.len() as f32;
        for (i, s) in frame.iter_mut().enumerate() {
            let g = start + step * (i + 1) as f32;
            let v = *s * g;
            if v.abs() > AGC_LIMIT {
                limited = true;
                *s = AGC_LIMIT.copysign(v);
            } else {
                *s = v;
            }
        }
        self.applied = target;
        if limited {
            self.stats.limited_frames += 1;
        }
        self.stats.gain_db = self.gain_db;
        self.stats.applied_db = 20.0 * target.max(1e-6).log10();
        self.stats.noise_floor_db = self.floor_db;
        self.stats.gate_open = open;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_chat::dsp_fixtures::{normalize, rms_db, speech_like, white_noise};

    const RATE: u32 = 16_000;
    const FRAME: usize = 320;
    /// Frames of talk, then of pause, per burst of the level-stepped fixtures.
    const ON: usize = 12;
    const OFF: usize = 4;

    /// Speech bursts at `level` dBFS: 12 frames of steady tilted noise, then 4 frames of -75 dBFS
    /// room noise, repeated. Returns the signal and, per frame, whether it is talk.
    fn bursts(frames: usize, seed: u64, level: f32) -> (Vec<f32>, Vec<bool>) {
        let mut talk = white_noise(frames * FRAME, seed, level);
        let mut z = 0.0f32;
        for v in &mut talk {
            z = 0.5 * z + 0.5 * *v;
            *v = z;
        }
        normalize(&mut talk, level);
        let room = white_noise(frames * FRAME, seed + 1, -75.0);
        let on: Vec<bool> = (0..frames).map(|i| i % (ON + OFF) < ON).collect();
        let mut out = room;
        for (i, is_on) in on.iter().enumerate() {
            if *is_on {
                out[i * FRAME..(i + 1) * FRAME].copy_from_slice(&talk[i * FRAME..(i + 1) * FRAME]);
            }
        }
        (out, on)
    }

    fn run(agc: &mut AutoGain, input: &[f32]) -> Vec<f32> {
        let mut out = input.to_vec();
        for f in out.chunks_mut(FRAME) {
            agc.process(f);
        }
        out
    }

    fn frame_db(s: &[f32], i: usize) -> f64 {
        rms_db(&s[i * FRAME..(i + 1) * FRAME])
    }

    /// Talk frames in `range` that are not the first of their burst (that one carries the ramp up
    /// from the gated gain).
    fn settled_talk(on: &[bool], range: std::ops::Range<usize>) -> Vec<usize> {
        range.filter(|i| on[*i] && *i > 0 && on[*i - 1]).collect()
    }

    /// Mean output level of `frames` is within 1.5 dB of the target and every frame within 3 dB
    /// (the gain sits slightly below the target: it follows loud frames down at once and quiet
    /// ones up slowly).
    fn assert_at_target(out: &[f32], frames: &[usize], what: &str) {
        assert!(!frames.is_empty());
        let target = f64::from(AGC_TARGET_DB);
        let mut sum = 0.0;
        for i in frames {
            let db = frame_db(out, *i);
            assert!((db - target).abs() <= 3.0, "{what} frame {i}: {db:.1} dBFS");
            sum += db;
        }
        let mean = sum / frames.len() as f64;
        assert!((mean - target).abs() <= 1.5, "{what}: mean {mean:.1} dBFS");
    }

    /// A level-stepped fixture: quiet talk (-32 dBFS) rises to the target within the release
    /// time, then a loud step (-6 dBFS) is pulled down within the attack time, and the output
    /// never exceeds the limiter.
    #[test]
    fn level_steps_settle_at_the_target() {
        let (quiet, q_on) = bursts(240, 1, -32.0);
        let (loud, l_on) = bursts(96, 3, -6.0);
        let input: Vec<f32> = quiet.iter().chain(&loud).copied().collect();
        let on: Vec<bool> = q_on.iter().chain(&l_on).copied().collect();
        let mut agc = AutoGain::new();
        let out = run(&mut agc, &input);
        // Release: 12 dB at 0.15 dB per talk frame is 80 talk frames (about 107 frames); check
        // from frame 160.
        assert_at_target(&out, &settled_talk(&on, 160..240), "quiet");
        // Attack: the limiter cuts the first loud frame at once, then 2 dB per frame covers the
        // rest of the swing; check from frame 16 of the loud part.
        assert_at_target(&out, &settled_talk(&on, 256..336), "loud");
        assert!(out.iter().all(|s| s.abs() <= AGC_LIMIT));
        assert!(agc.stats().limited_frames >= 1);
        // Pauses are never boosted.
        for i in (1..336).filter(|i| !on[*i] && !on[*i - 1]) {
            let gain = frame_db(&out, i) - frame_db(&input, i);
            assert!(gain <= 0.01, "pause frame {i} boosted by {gain:.2} dB");
        }
    }

    /// The boost never exceeds +18 dB: talk at -50 dBFS ends near -32 dBFS.
    #[test]
    fn boost_is_capped() {
        let (input, on) = bursts(400, 3, -50.0);
        let mut agc = AutoGain::new();
        let out = run(&mut agc, &input);
        let frames = settled_talk(&on, 300..400);
        let mean = frames.iter().map(|i| frame_db(&out, *i)).sum::<f64>() / frames.len() as f64;
        assert!(
            (mean - (-50.0 + f64::from(AGC_MAX_GAIN_DB))).abs() <= 1.0,
            "{mean:.1} dBFS"
        );
        assert!((agc.stats().gain_db - AGC_MAX_GAIN_DB).abs() < 0.01);
    }

    /// Silence and low noise after speech are not boosted: once the gain has risen on quiet
    /// talk, a -65 dBFS noise tail comes out no louder than it went in.
    #[test]
    fn noise_after_speech_is_not_boosted() {
        let (speech, _) = bursts(160, 4, -32.0);
        let noise = white_noise(100 * FRAME, 5, -65.0);
        let input: Vec<f32> = speech.iter().chain(&noise).copied().collect();
        let mut agc = AutoGain::new();
        let out = run(&mut agc, &input);
        assert!(
            agc.stats().gain_db > 6.0,
            "the gain never rose: {:?}",
            agc.stats()
        );
        // One frame of ramp, then never louder than the input.
        for i in 161..260 {
            let gain = frame_db(&out, i) - frame_db(&input, i);
            assert!(gain <= 0.01, "noise frame {i} boosted by {gain:.2} dB");
        }
        // Digital silence stays silent.
        let mut zero = vec![0.0f32; FRAME];
        agc.process(&mut zero);
        assert!(zero.iter().all(|s| *s == 0.0));
    }

    /// Room noise at -45 dBFS between speech-like syllables: the floor tracker holds the gate
    /// shut in the pauses, so the pauses are not boosted even though -45 is above the absolute
    /// gate.
    #[test]
    fn gate_follows_the_noise_floor() {
        let n = RATE as usize * 6;
        let speech = speech_like(n, RATE, 6, -30.0);
        let noise = white_noise(n, 7, -45.0);
        let input: Vec<f32> = speech.iter().zip(&noise).map(|(s, v)| s + v).collect();
        let mut agc = AutoGain::new();
        let out = run(&mut agc, &input);
        let mut checked = 0;
        for i in 50..(n / FRAME) {
            if rms_db(&speech[i * FRAME..(i + 1) * FRAME]) < -90.0
                && rms_db(&speech[(i - 1) * FRAME..i * FRAME]) < -90.0
            {
                let gain = frame_db(&out, i) - frame_db(&input, i);
                assert!(gain <= 0.01, "pause frame {i} boosted by {gain:.2} dB");
                checked += 1;
            }
        }
        assert!(checked > 20, "fixture has too few pause frames ({checked})");
        assert!(agc.stats().gated_frames > 0);
        assert!(
            agc.stats().gain_db > 3.0,
            "the talk was never levelled: {:?}",
            agc.stats()
        );
    }

    /// A frame of digital silence (a stream start, a mute toggle) in a room with -45 dBFS noise
    /// does not drop the noise floor, so the room noise after it is not boosted.
    #[test]
    fn digital_silence_does_not_reset_the_floor() {
        let n = RATE as usize * 4;
        let speech = speech_like(n, RATE, 6, -30.0);
        let noise = white_noise(n, 7, -45.0);
        let talk: Vec<f32> = speech.iter().zip(&noise).map(|(s, v)| s + v).collect();
        let mut agc = AutoGain::new();
        run(&mut agc, &talk);
        assert!(agc.stats().gain_db > 3.0, "{:?}", agc.stats());
        let floor = agc.stats().noise_floor_db;
        let mut zero = vec![0.0f32; FRAME];
        agc.process(&mut zero);
        assert_eq!(agc.stats().noise_floor_db, floor);
        let room = white_noise(100 * FRAME, 10, -45.0);
        let out = run(&mut agc, &room);
        for i in 1..100 {
            let gain = frame_db(&out, i) - frame_db(&room, i);
            assert!(gain <= 0.01, "room frame {i} boosted by {gain:.2} dB");
        }
    }

    /// Room noise that steps up by 30 dB (a fan switched on) reads as speech at first, but the
    /// floor catches up within seconds and the boost ends.
    #[test]
    fn a_louder_room_stops_being_boosted() {
        let (talk, _) = bursts(160, 11, -32.0);
        let mut agc = AutoGain::new();
        run(&mut agc, &talk);
        let fan = white_noise(500 * FRAME, 12, -45.0);
        let out = run(&mut agc, &fan);
        // 150 frames of open gate, then 0.5 dB per frame closes the 30 dB gap in about 45.
        for i in 260..500 {
            let gain = frame_db(&out, i) - frame_db(&fan, i);
            assert!(gain <= 0.01, "fan frame {i} still boosted by {gain:.2} dB");
        }
    }
}
