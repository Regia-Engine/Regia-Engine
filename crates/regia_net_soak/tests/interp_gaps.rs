// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 9.7 (row R6, plan N211): proxy interpolation over the named soak profiles.
//!
//! A 60 Hz server sends a 30 Hz state stream through the impaired in-process network (seeded,
//! virtual time, no wall clock); the client runs the real interpolation core (`InterpClock`,
//! `SnapshotBuffer`, `NetInterpolation::sample_params`) and renders at 60 frames per second.
//!
//! - `congested` with a binding budget (64 moving entities, room for 16 per send, the spec 72
//!   priority accumulator picks them): an entity without `NetVelocity` moving at constant speed is
//!   never shown slower than 80 % of its speed.
//! - `bad-wifi` (bursty loss): proxies with velocities extrapolate on at most 1 % of frames.

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;

use glam::{Quat, Vec3};
use regia_net::DatagramSocket;
use regia_net::interp::{InterpProfile, NetInterpolation, ProxyState, ProxyTimelineState, SnapshotBuffer};
use regia_net::model::NetId;
use regia_net::send::{Candidate, TickBudget, select};
use regia_net::settings::InterpolationSettings;
use regia_net_soak::impair::ImpairedNet;
use regia_net_soak::profiles::Profile;

const HZ: f64 = 60.0;
const MSPT: f64 = 1000.0 / HZ;
const SEND_TICKS: u64 = 2;
/// Bytes of one entity record in the simulated state frame.
const RECORD_BYTES: usize = 40;
/// Frames before this time are start-up (the delay settles) and are not measured.
const WARMUP_MS: u64 = 3_000;

/// One run's numbers.
#[derive(Debug, Default)]
struct Outcome {
    /// Entity frames measured.
    frames: u64,
    /// Entity frames shown extrapolating or starved.
    extrapolated: u64,
    /// Entity frames shown holding (a moving entity standing still).
    holding: u64,
    /// The lowest shown speed over the true speed, over every measured frame.
    min_speed_ratio: f64,
    /// The highest shown speed over the true speed.
    max_speed_ratio: f64,
    /// Where the lowest ratio was seen: `(ms, entity)`.
    min_at: (u64, usize),
    /// State frames delivered / sent.
    delivered: u64,
    sent: u64,
    /// The largest target delay seen, ms.
    max_target_ms: f64,
}

fn direction(e: usize) -> Vec3 {
    let a = e as f32 * 0.7;
    Vec3::new(a.cos(), 0.0, a.sin())
}

fn speed(e: usize) -> f32 {
    2.0 + (e % 5) as f32
}

fn truth(e: usize, tick: f64) -> Vec3 {
    Vec3::new(e as f32 * 3.0, 0.0, 0.0) + direction(e) * speed(e) * (tick / HZ) as f32
}

/// Runs `entities` constant-speed proxies over `profile` for `ms`. `per_send`: the budget in
/// records per state frame (`None` sends every entity in every frame). `velocity`: the states
/// carry `NetVelocity`.
fn run(
    profile: &str,
    seed: u64,
    entities: usize,
    per_send: Option<usize>,
    velocity: bool,
    ms: u64,
) -> Outcome {
    let p = Profile::named(profile).expect("profile");
    let net = ImpairedNet::new(seed);
    let server_addr = SocketAddr::from(([10, 0, 0, 1], 7777));
    let server = net
        .add_endpoint(server_addr, p.server_link(false))
        .expect("server");
    let client_addr = p
        .client_addr(0)
        .unwrap_or_else(|| SocketAddr::from(([10, 1, 0, 1], 40_000)));
    let client = net
        .add_endpoint(client_addr, p.client_link(0, ms))
        .expect("client");
    let mut ss = net.socket(server);
    let mut sc = net.socket(client);
    let client_public = net.public_addr(client);

    let mut ni = NetInterpolation::new(InterpolationSettings::default(), HZ, SEND_TICKS as f64);
    let profile = InterpProfile::default();
    let mut bufs: Vec<SnapshotBuffer> = (0..entities).map(|_| SnapshotBuffer::new(32)).collect();
    let mut acc = vec![0.0f32; entities];
    let mut frames_sent: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut last: Vec<Option<(f64, Vec3)>> = vec![None; entities];
    let mut out = Outcome {
        min_speed_ratio: f64::MAX,
        ..Outcome::default()
    };
    let mut next_tick = 0u64;
    let mut next_frame = 0.0f64;
    let mut last_frame: Option<f64> = None;
    let mut buf = vec![0u8; 4096];
    for t in 0..=ms {
        net.advance_to_ms(t);
        // Server: one state frame every SEND_TICKS ticks.
        while (next_tick as f64 * MSPT) <= t as f64 {
            let tick = next_tick;
            next_tick += SEND_TICKS;
            let picked: Vec<usize> = match per_send {
                None => (0..entities).collect(),
                Some(n) => {
                    let mut cands: Vec<Candidate> = (0..entities)
                        .map(|e| {
                            acc[e] += 1.0;
                            Candidate {
                                id: NetId(e as u32),
                                spawn: false,
                                bytes: RECORD_BYTES,
                                acc: acc[e],
                                forced: false,
                            }
                        })
                        .collect();
                    let budget = TickBudget {
                        bytes: (n * RECORD_BYTES) as f64,
                        spawns: 0,
                    };
                    let (ids, _) = select(&mut cands, budget, 0.0);
                    let picked: Vec<usize> = ids.iter().map(|id| id.0 as usize).collect();
                    for &e in &picked {
                        acc[e] = 0.0;
                    }
                    picked
                }
            };
            let mut payload = vec![0u8; 8 + picked.len() * RECORD_BYTES];
            payload[..8].copy_from_slice(&tick.to_le_bytes());
            ss.send_to(&payload, client_public).expect("send");
            frames_sent.insert(tick, picked);
            out.sent += 1;
        }
        // Client: arrivals (a transport drops duplicates).
        while let Some((n, _)) = sc.recv_from(&mut buf).expect("io") {
            if n < 8 {
                continue;
            }
            let tick = u64::from_le_bytes(buf[..8].try_into().expect("8 bytes"));
            if !seen.insert(tick) {
                continue;
            }
            out.delivered += 1;
            // As the client's state channel does: frames newer than the newest are arrival
            // samples, older (reordered) ones only fill loss gaps; both land in the buffers.
            if ni.clock.newest_tick().is_none_or(|n| tick > n) {
                ni.clock.on_state(tick, t as f64);
            } else {
                ni.clock.on_late_state(tick);
            }
            for &e in frames_sent.get(&tick).map(Vec::as_slice).unwrap_or(&[]) {
                bufs[e].push(ProxyState {
                    tick,
                    translation: truth(e, tick as f64),
                    rotation: Quat::IDENTITY,
                    scale: Vec3::ONE,
                    velocity: velocity.then(|| direction(e) * speed(e)),
                    teleport: false,
                    steady: false,
                });
            }
        }
        // Client: 60 frames per second.
        if t as f64 >= next_frame {
            next_frame += MSPT;
            let now = t as f64;
            let frame_ms = last_frame.map_or(0.0, |l| now - l);
            last_frame = Some(now);
            let Some(interp) = ni.clock.advance(now) else {
                continue;
            };
            out.max_target_ms = out.max_target_ms.max(ni.clock.target_delay_ms());
            let params = ni.sample_params();
            for e in 0..entities {
                let Some(s) = bufs[e].sample(interp, &profile, &params, frame_ms) else {
                    continue;
                };
                let pos = s.pose.translation;
                let prev = last[e].replace((interp, pos));
                if t < WARMUP_MS {
                    continue;
                }
                out.frames += 1;
                match s.state {
                    ProxyTimelineState::Extrapolating | ProxyTimelineState::Starved => out.extrapolated += 1,
                    ProxyTimelineState::Holding => out.holding += 1,
                    ProxyTimelineState::Interpolating => {}
                }
                if let Some((pi, pp)) = prev {
                    let d_ticks = interp - pi;
                    if d_ticks > 1e-6 {
                        let true_m = f64::from(speed(e)) * d_ticks / HZ;
                        let ratio = f64::from(pos.distance(pp)) / true_m;
                        if ratio < out.min_speed_ratio {
                            out.min_speed_ratio = ratio;
                            out.min_at = (t, e);
                        }
                        out.max_speed_ratio = out.max_speed_ratio.max(ratio);
                    }
                }
            }
        }
    }
    out
}

/// R6 acceptance 1: `congested`, 64 moving entities, a budget for 16 per send (each entity every
/// ~4 sends): an entity without `NetVelocity` at constant speed is never shown slower than 80 % of
/// its true speed (base: held for most of each deferral, about 6 Hz stop-and-go).
#[test]
fn congested_budget_deferred_entities_never_drop_under_80_percent_speed() {
    for seed in [12_097, 7, 31] {
        let o = run("congested", seed, 64, Some(16), false, 30_000);
        eprintln!("congested seed {seed}: {o:?}");
        assert!(o.frames > 64 * 1_500, "frames measured: {}", o.frames);
        assert!(
            o.min_speed_ratio >= 0.8,
            "seed {seed}: shown speed fell to {:.1} % of true at {:?} (holding frames {})",
            o.min_speed_ratio * 100.0,
            o.min_at,
            o.holding
        );
    }
}

/// R6 acceptance 2: `bad-wifi` (8 ms + 0-30 ms jitter, 1 % random loss plus Gilbert-Elliott
/// bursts): proxies with velocities extrapolate on at most 1 % of frames, because the delay holds
/// the measured burst-loss run (base: two lost frames in a row always extrapolate).
#[test]
fn bad_wifi_extrapolates_on_at_most_one_percent_of_frames() {
    for seed in [12_097, 7, 31] {
        let o = run("bad-wifi", seed, 16, None, true, 60_000);
        eprintln!("bad-wifi seed {seed}: {o:?}");
        assert!(o.frames > 16 * 3_000, "frames measured: {}", o.frames);
        assert!(
            o.delivered < o.sent,
            "the profile loses frames: {}/{}",
            o.delivered,
            o.sent
        );
        let share = o.extrapolated as f64 / o.frames as f64;
        assert!(
            share <= 0.01,
            "seed {seed}: {:.2} % of frames extrapolated ({} of {}), max target delay {:.0} ms",
            share * 100.0,
            o.extrapolated,
            o.frames,
            o.max_target_ms
        );
        assert!(
            o.max_target_ms <= InterpolationSettings::default().max_delay_ms + 1e-9,
            "the delay stays within max_delay_ms"
        );
    }
}
