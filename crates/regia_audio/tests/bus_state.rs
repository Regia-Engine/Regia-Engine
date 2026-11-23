// SPDX-License-Identifier: MPL-2.0
//! Bus state through the offline renderer: a layout-muted bus stays silent from the first frame
//! and across output suspend/resume. Non-streaming sounds only, so rendering needs no pacing.

use regia_audio::{Audio, AudioSettings, BusLayout, PlaybackParams, SoundAsset, SoundClip};

const RATE: u32 = 48_000;
const FRAME: f64 = 1.0 / 60.0;

fn loud_loop() -> SoundAsset {
    let samples: Vec<f32> = (0..RATE / 2)
        .map(|i| ((i as f32 / RATE as f32) * 440.0 * std::f32::consts::TAU).sin() * 0.5)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("sine clip"))
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

fn render(audio: &mut Audio, ticks: usize) -> Vec<f32> {
    let mut out = Vec::new();
    for _ in 0..ticks {
        audio.tick(FRAME);
        out.extend(
            audio
                .render_offline((f64::from(RATE) * FRAME) as usize)
                .expect("offline backend"),
        );
    }
    out
}

fn muted_sfx_audio() -> Audio {
    let mut layout = BusLayout::default_tree();
    for bus in &mut layout.buses {
        if bus.name == "Sfx" {
            bus.mute = true;
        }
    }
    Audio::new(&AudioSettings {
        headless: true,
        buses: layout,
        ..AudioSettings::default()
    })
}

fn play_on_sfx(audio: &mut Audio) {
    let params = PlaybackParams {
        bus: Some("Sfx".to_string()),
        ..PlaybackParams::default()
    };
    audio.play_asset(&loud_loop(), params).expect("enqueue");
    audio.drain();
}

#[test]
fn layout_muted_bus_reports_muted_and_stays_silent_after_the_first_tick() {
    let mut audio = muted_sfx_audio();
    assert_eq!(audio.bus_muted("Sfx"), Some(true));
    assert_eq!(audio.bus_muted("Music"), Some(false));
    play_on_sfx(&mut audio);
    let out = render(&mut audio, 6);
    assert!(rms(&out) < 1e-3, "a muted bus must be silent, rms {}", rms(&out));
}

#[test]
fn layout_muted_bus_stays_silent_after_suspend_and_resume() {
    let mut audio = muted_sfx_audio();
    audio.suspend_output();
    audio.resume_output();
    assert_eq!(audio.bus_muted("Sfx"), Some(true));
    play_on_sfx(&mut audio);
    let out = render(&mut audio, 6);
    assert!(rms(&out) < 1e-3, "muted after resume, rms {}", rms(&out));
}

#[test]
fn unmuting_a_layout_muted_bus_makes_it_audible() {
    let mut audio = muted_sfx_audio();
    play_on_sfx(&mut audio);
    audio.set_bus_mute("Sfx", false).expect("Sfx exists");
    let out = render(&mut audio, 6);
    assert!(rms(&out) > 0.05, "unmuted bus is heard, rms {}", rms(&out));
    audio.set_bus_mute("Sfx", true).expect("Sfx exists");
    // Mute fades over at most 50 ms; measure after it has settled.
    let _ = render(&mut audio, 4);
    let out = render(&mut audio, 4);
    assert!(rms(&out) < 1e-3, "muted again, rms {}", rms(&out));
}
