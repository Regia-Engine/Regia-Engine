// SPDX-License-Identifier: MPL-2.0
//! Renders one scripted scenario per propagation medium to WAV for listen checks
//! (`docs/specs/16-audio.md` § 3c). Headless and deterministic: the offline mixer, synthesised
//! sources, a fixed timeline.
//!
//! ```text
//! cargo run --release -p regia_audio --example propagation_render -- docs/evidence/audio/2026-10-08
//! ```
//!
//! The same 10-second script plays in each scenario: the pilot's breathing and a radio call
//! (direct paths), footsteps 6 m away, a hull landing 40 m away, a projectile flying past 10 m
//! away and an explosion 400 m away. Scenarios: `earth`, `mars`, `europa_standing`,
//! `europa_jumping` (the pilot jumps at 3.4 s, during the explosion's ground arrival) and
//! `europa_hab` (inside a pressurised hab: a machine inside, the same exterior events outside).

use std::path::{Path, PathBuf};

use regia_audio::{
    Audio, AudioEmitter, AudioInteriorVolume, AudioMedium, AudioPropagation, AudioSettings, EmitterCategory,
    InteriorFrame, ListenerContact, ListenerCoupling, PlaybackParams, SoundAsset, SoundClip,
};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;

const RATE: u32 = 48_000;
const FRAME: f64 = 1.0 / 60.0;
const SECONDS: f64 = 10.0;

/// Deterministic white noise in −1..1 (xorshift32).
struct Noise(u32);

impl Noise {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}

fn clip(samples: Vec<f32>) -> Result<SoundAsset, String> {
    Ok(SoundAsset::from_clip(
        &SoundClip::new(samples, RATE).map_err(|e| e.to_string())?,
    ))
}

fn synth(seconds: f32, mut f: impl FnMut(f32, &mut Noise) -> f32) -> Vec<f32> {
    let mut noise = Noise(0x9E37_79B9);
    (0..(seconds * RATE as f32) as usize)
        .map(|i| f(i as f32 / RATE as f32, &mut noise))
        .collect()
}

/// A low boom with a noisy crack: an explosion.
fn explosion() -> Vec<f32> {
    let mut lp = 0.0f32;
    synth(2.5, |t, n| {
        lp += 0.08 * (n.next() - lp);
        let boom = (t * 38.0 * std::f32::consts::TAU).sin() * (-t / 0.6).exp() * 0.7;
        let crack = n.next() * (-t / 0.05).exp() * 0.5;
        let rumble = lp * (-t / 0.9).exp() * 1.5;
        (boom + crack + rumble) * (t / 0.002).min(1.0)
    })
}

/// A boot on regolith: a thump and a crunch.
fn footstep() -> Vec<f32> {
    synth(0.25, |t, n| {
        let thump = (t * 85.0 * std::f32::consts::TAU).sin() * (-t / 0.04).exp() * 0.6;
        let crunch = n.next() * (-t / 0.02).exp() * 0.25;
        thump + crunch
    })
}

/// A heavy hull touching down.
fn hull_landing() -> Vec<f32> {
    let mut lp = 0.0f32;
    synth(1.2, |t, n| {
        lp += 0.05 * (n.next() - lp);
        let clang = (t * 210.0 * std::f32::consts::TAU).sin() * (-t / 0.25).exp() * 0.3;
        let thud = (t * 55.0 * std::f32::consts::TAU).sin() * (-t / 0.3).exp() * 0.6;
        clang + thud + lp * (-t / 0.4).exp() * 1.2
    })
}

/// A projectile whoosh (air only).
fn projectile() -> Vec<f32> {
    synth(0.6, |t, n| {
        let env = (std::f32::consts::PI * t / 0.6).sin().powi(2);
        n.next() * env * 0.35
    })
}

/// Suit breathing: band-limited noise swelling in and out; held between 1.6 s and 8.4 s so the
/// world events can be measured on their own.
fn breathing() -> Vec<f32> {
    let (mut a, mut b) = (0.0f32, 0.0f32);
    synth(SECONDS as f32, |t, n| {
        a += 0.25 * (n.next() - a);
        b += 0.02 * (a - b);
        let held = (1.6..8.4).contains(&t);
        let env = if held {
            0.0
        } else {
            (t * std::f32::consts::TAU / 3.2).sin().powi(2)
        };
        (a - b) * env * 0.5
    })
}

/// A radio call: two short 1 kHz beeps and a band-passed chatter.
fn radio() -> Vec<f32> {
    synth(1.4, |t, n| {
        let beep = if t < 0.12 || (0.2..0.32).contains(&t) {
            (t * 1_000.0 * std::f32::consts::TAU).sin() * 0.25
        } else {
            0.0
        };
        let chatter = if t > 0.4 {
            let syll = (t * 7.0 * std::f32::consts::TAU).sin().max(0.0);
            (t * 640.0 * std::f32::consts::TAU).sin() * syll * 0.15 + n.next() * 0.02
        } else {
            0.0
        };
        beep + chatter
    })
}

/// A deployed machine's hum (looped as one-shots inside the hab).
fn machine() -> Vec<f32> {
    synth(SECONDS as f32, |t, _| {
        ((t * 120.0 * std::f32::consts::TAU).sin() * 0.03
            + (t * 2_400.0 * std::f32::consts::TAU).sin() * 0.006)
            * (t / 0.2).min(1.0)
    })
}

enum Event {
    Play {
        asset: usize,
        at: Vec3,
        category: EmitterCategory,
        bus: &'static str,
        /// The emitter's full-level radius, metres (how loud the source is).
        loudness: f32,
    },
    Contact(ListenerContact),
}

struct Scenario {
    name: &'static str,
    medium: AudioMedium,
    interior: bool,
    jump: bool,
}

fn script(interior: bool, jump: bool) -> Vec<(f64, Event)> {
    use EmitterCategory::*;
    let play = |asset, at, category, bus, loudness| Event::Play {
        asset,
        at,
        category,
        bus,
        loudness,
    };
    let mut events = vec![
        (0.0, play(4, Vec3::new(0.0, 0.1, 0.0), Body, "World", 2.0)),
        (0.3, play(5, Vec3::new(0.0, 0.0, -1.0), Radio, "Voice", 2.0)),
        (
            2.0,
            play(2, Vec3::new(-40.0, 0.0, -10.0), HullLanding, "World", 10.0),
        ),
        (
            2.6,
            play(3, Vec3::new(-10.0, 1.5, -10.0), Projectile, "World", 4.0),
        ),
        (
            3.2,
            play(0, Vec3::new(400.0, 0.0, -50.0), Explosion, "World", 40.0),
        ),
    ];
    for i in 0..6 {
        events.push((
            0.8 + 0.5 * f64::from(i),
            play(1, Vec3::new(6.0, 0.0, -2.0), Footstep, "World", 2.0),
        ));
    }
    if interior {
        events.push((0.0, play(6, Vec3::new(2.0, 0.0, 2.0), Machinery, "World", 2.0)));
    }
    if jump {
        events.push((3.25, Event::Contact(ListenerContact::None)));
        events.push((5.0, Event::Contact(ListenerContact::Boots)));
    }
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    events
}

fn render(scenario: &Scenario, assets: &[SoundAsset]) -> Result<Vec<f32>, String> {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio.set_medium(scenario.medium.clone());
    if scenario.interior {
        audio.set_interiors(vec![InteriorFrame {
            entity: Entity::from_raw_u32(1).ok_or("entity")?,
            center: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            volume: AudioInteriorVolume {
                half_extents: Vec3::new(6.0, 3.0, 6.0),
                pressurised: true,
                wall: "steel".into(),
            },
        }]);
        audio.set_listener_coupling(ListenerCoupling {
            contact: ListenerContact::Structure,
            ..ListenerCoupling::default()
        });
    }
    let events = script(scenario.interior, scenario.jump);
    let mut next = 0;
    let mut out = Vec::new();
    let mut now = 0.0;
    while now < SECONDS {
        while next < events.len() && events[next].0 <= now {
            match &events[next].1 {
                Event::Play {
                    asset,
                    at,
                    category,
                    bus,
                    loudness,
                } => audio
                    .play_asset_propagated(
                        &assets[*asset],
                        PlaybackParams::on_bus(*bus),
                        *at,
                        &AudioEmitter {
                            min_distance: *loudness,
                            max_distance: 2_000.0,
                            ..AudioEmitter::default()
                        },
                        &AudioPropagation::for_category(*category),
                    )
                    .map_err(|e| e.to_string())?,
                Event::Contact(contact) => audio.set_listener_coupling(ListenerCoupling {
                    contact: *contact,
                    ..audio.listener_coupling().clone()
                }),
            }
            next += 1;
        }
        audio.drain();
        audio.tick(FRAME);
        out.extend(
            audio
                .render_offline((f64::from(RATE) * FRAME) as usize)
                .map_err(|e| e.to_string())?,
        );
        now += FRAME;
    }
    Ok(out)
}

/// 16-bit stereo PCM WAV.
fn write_wav(path: &Path, frames: &[f32]) -> std::io::Result<()> {
    let data: Vec<u8> = frames
        .iter()
        .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32_767.0) as i16).to_le_bytes())
        .collect();
    let mut wav = Vec::with_capacity(44 + data.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(path, wav)
}

fn main() -> Result<(), String> {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "target/propagation_render".into()),
    );
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let assets = [
        clip(explosion())?,
        clip(footstep())?,
        clip(hull_landing())?,
        clip(projectile())?,
        clip(breathing())?,
        clip(radio())?,
        clip(machine())?,
    ];
    let scenarios = [
        Scenario {
            name: "earth",
            medium: AudioMedium::earth(),
            interior: false,
            jump: false,
        },
        Scenario {
            name: "mars",
            medium: AudioMedium::mars(),
            interior: false,
            jump: false,
        },
        Scenario {
            name: "europa_standing",
            medium: AudioMedium::airless(),
            interior: false,
            jump: false,
        },
        Scenario {
            name: "europa_jumping",
            medium: AudioMedium::airless(),
            interior: false,
            jump: true,
        },
        Scenario {
            name: "europa_hab",
            medium: AudioMedium::airless(),
            interior: true,
            jump: false,
        },
    ];
    for scenario in &scenarios {
        let frames = render(scenario, &assets)?;
        let peak = frames.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let rms = (frames.iter().map(|s| s * s).sum::<f32>() / frames.len().max(1) as f32).sqrt();
        let path = dir.join(format!("{}.wav", scenario.name));
        write_wav(&path, &frames).map_err(|e| e.to_string())?;
        println!(
            "{:<16} peak {:>6.1} dBFS  rms {:>6.1} dBFS  -> {}",
            scenario.name,
            20.0 * peak.max(1e-9).log10(),
            20.0 * rms.max(1e-9).log10(),
            path.display()
        );
    }
    Ok(())
}
