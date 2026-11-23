// SPDX-License-Identifier: MIT-0
//! Voice loopback latency measurement (`docs/specs/16-audio.md` § 3b, § 6): capture →
//! [`regia_audio::VoiceStreamData`] playback, the path DO27 voice chat uses.
//!
//! The microphone is opened, captured frames are fed straight into the voice stream (as a
//! transport would after decoding), and the stream reports how long a chunk takes from drain to
//! first render. The spec's bar is < 60 ms of *added* pipeline latency (excluding network).
//!
//! `VOICE_LOOPBACK_SILENT=1` mutes the Voice bus for unattended runs: the latency is recorded
//! inside the stream's mixer pass, so bus volume does not affect the measurement, but the
//! headset does not echo the microphone.
//!
//! Run on a machine with an input device (Linux/Windows desktop): `cargo run -p audio_lab
//! --example voice_loopback`. Without an input device the offline backend measures the same
//! pipeline end to end without hardware.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use regia_audio::{Audio, AudioSettings, VoiceCaptureSettings};

const SECONDS: f64 = 8.0;
const FRAME: f64 = 1.0 / 60.0;

fn main() -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .try_init();

    // A real output device when there is one; the offline renderer otherwise (both drive the
    // identical playback path).
    let mut audio = Audio::new(&AudioSettings::default());
    tracing::info!(target: "voice_loopback", headless = audio.is_headless(), "output opened");

    if let Err(error) = audio.start_capture(VoiceCaptureSettings::default()) {
        tracing::error!(target: "voice_loopback", "no capture device: {error}");
        return ExitCode::FAILURE;
    }
    let (device, rate) = (audio.capture_device().to_string(), audio.capture_sample_rate());
    tracing::info!(target: "voice_loopback", device, rate, "capturing");

    if std::env::var("VOICE_LOOPBACK_SILENT").is_ok_and(|v| !v.is_empty()) {
        // Mute playback without touching the measurement (latency is recorded in the stream's
        // mixer pass, before the bus volume is applied). One tick applies the bus blend.
        audio
            .set_bus_volume("Voice", -120.0, Duration::ZERO)
            .expect("the Voice bus exists");
        audio.tick(FRAME);
        tracing::info!(target: "voice_loopback", "VOICE_LOOPBACK_SILENT: the Voice bus is muted");
    }

    audio
        .start_voice_loopback(40)
        .expect("the Voice bus exists on the default layout");

    let frames_total = (SECONDS / FRAME) as usize;
    let started = Instant::now();
    for _ in 0..frames_total {
        audio.tick_voice(FRAME);
        if audio.is_headless() {
            let _ = audio.render_offline(800);
        } else {
            std::thread::sleep(Duration::from_secs_f64(FRAME));
        }
    }
    let _ = started.elapsed();

    let Some(stats) = audio.voice_stats() else {
        tracing::error!(target: "voice_loopback", "loopback stream never ran");
        return ExitCode::FAILURE;
    };
    let (rms, peak) = audio.capture_level_db().unwrap_or((-100.0, -100.0));
    tracing::info!(
        target: "voice_loopback",
        "loopback done: last {} ms, max {} ms, pushed {}, dropped {}, buffered {}, input rms {:.0} dBFS peak {:.0} dBFS",
        stats.last_latency_ms,
        stats.max_latency_ms,
        stats.chunks_pushed,
        stats.chunks_dropped,
        stats.buffered_frames,
        rms,
        peak
    );
    if stats.chunks_pushed == 0 {
        tracing::error!(target: "voice_loopback", "no frames captured (muted mic?)");
        return ExitCode::FAILURE;
    }
    if stats.max_latency_ms >= 60 {
        tracing::error!(target: "voice_loopback", "added latency ≥ 60 ms — investigate");
        return ExitCode::FAILURE;
    }
    tracing::info!(target: "voice_loopback", "PASS: added latency below 60 ms");
    ExitCode::SUCCESS
}
