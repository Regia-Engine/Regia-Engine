// SPDX-License-Identifier: MPL-2.0
//! Microbenchmarks for the v0.1.22 hot paths (`docs/06-testing-and-ci.md` § 9): `ReplaySim`
//! tick, seek and snapshot, and camera path sampling. Every test is `#[ignore]`: the gate never
//! runs them, and they assert only that the work happened, never a time. Run with
//!
//! ```text
//! cargo test -p regia_framework --release --test perf_micro -- --ignored --nocapture
//! ```

use std::hint::black_box;
use std::time::{Duration, Instant};

use regia_core::math::{Quat, Vec3};
use regia_framework::replay_sim::{
    FullCodec, NavBeacon, NavRecord, PilotInput, ReplayEntity, ReplayHeader, ReplaySim, SyntheticReplay,
};
use regia_framework::shots::asset::{
    CameraPath, Ease, LookAt, PathInterp, PathKey, ScalarKey, Shot, ShotCamera, ShotSequence, Track,
};

/// Samples of a measured operation, reported as p50 / p99 / max.
fn report(name: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "perf-micro {name}: n={} p50={:?} p99={:?} max={:?}",
        samples.len(),
        at(0.5),
        at(0.99),
        samples[samples.len() - 1]
    );
}

/// Times `iterations` calls of `op`, each as one sample, after a short warm-up.
fn measure(name: &str, iterations: usize, mut op: impl FnMut(usize)) {
    for i in 0..iterations.min(100) {
        op(i);
    }
    let samples = (0..iterations)
        .map(|i| {
            let start = Instant::now();
            op(i);
            start.elapsed()
        })
        .collect();
    report(name, samples);
}

/// Snapshot rows are `id, team, x`, three bytes per entity.
struct RowCodec;

impl FullCodec for RowCodec {
    fn decode(&self, snapshot: &[u8]) -> Result<Vec<ReplayEntity>, String> {
        Ok(snapshot
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| ReplayEntity {
                id: u64::from(c[0]),
                kind: "warhull".into(),
                team: u32::from(c[1]),
                translation: Vec3::new(f32::from(c[2] as i8), 0.0, 0.0),
                rotation: Quat::IDENTITY,
            })
            .collect())
    }

    fn state_hash(&self, _snapshot: &[u8]) -> Option<u64> {
        None
    }
}

const TICKS: u32 = 36_000;
const FULL_EVERY: u32 = 60;
const ENTITIES: usize = 64;

/// Ten minutes at 60 Hz: inputs every tick, a full each second, a nav record every two.
fn long_replay() -> ReplaySim {
    let header = ReplayHeader {
        version: 3,
        protocol: "perf/1".into(),
        tick_rate: 60,
        ..ReplayHeader::default()
    };
    let rows = [(
        1u8,
        PilotInput {
            throttle: 100,
            ..PilotInput::default()
        },
    )];
    let mut snapshot = Vec::with_capacity(ENTITIES * 3);
    for e in 0..ENTITIES {
        snapshot.extend_from_slice(&[e as u8, (e % 2) as u8, (e % 100) as u8]);
    }
    let nav = |tick: u32| NavRecord {
        tick,
        beacons: (0..8)
            .map(|i| NavBeacon {
                id: i,
                owner_slot: 0,
                team: 0,
                number: i as u8,
                x_cm: i as i32 * 100,
                y_cm: 0,
                z_cm: 0,
                health: 100,
            })
            .collect(),
        waypoints: Vec::new(),
    };
    let mut writer = SyntheticReplay::new(&header);
    for tick in 0..TICKS {
        writer = writer.inputs(tick, &rows);
        if tick % FULL_EVERY == 0 {
            writer = writer.full(&snapshot, u64::from(tick));
        }
        if tick % (2 * FULL_EVERY) == 0 {
            writer = writer.nav(&nav(tick));
        }
    }
    let mut sim = ReplaySim::with_codec(RowCodec);
    sim.load_bytes(&writer.finish()).expect("synthetic replay parses");
    assert!(sim.end_tick() >= TICKS - 1, "replay has its ticks");
    sim
}

#[test]
#[ignore = "microbenchmark: run by hand, see docs/06-testing-and-ci.md § 9"]
fn replay_sim_parse_load() {
    let sim = long_replay();
    let file = sim.file().expect("loaded").clone();
    measure("replay_sim.load (clone of parsed file)", 20, |_| {
        let mut sim = ReplaySim::with_codec(RowCodec);
        sim.load(black_box(file.clone()));
        black_box(sim.end_tick());
    });
}

#[test]
#[ignore = "microbenchmark: run by hand, see docs/06-testing-and-ci.md § 9"]
fn replay_sim_tick() {
    let mut sim = long_replay();
    // One tick forward, then the snapshot a renderer would read.
    measure("replay_sim.step(1)+snapshot", 20_000, |i| {
        if sim.tick() + 1 >= sim.end_tick() {
            sim.seek(0);
        }
        sim.step(1);
        black_box(sim.last_step_events().len());
        if i % 4 == 0 {
            black_box(sim.snapshot());
        }
    });
    measure("replay_sim.step(60) (one second)", 500, |_| {
        if sim.tick() + 60 >= sim.end_tick() {
            sim.seek(0);
        }
        black_box(sim.step(60));
    });
}

#[test]
#[ignore = "microbenchmark: run by hand, see docs/06-testing-and-ci.md § 9"]
fn replay_sim_seek() {
    let mut sim = long_replay();
    let end = sim.end_tick();
    // A fixed odd stride visits ticks across the whole file, forwards and backwards.
    measure("replay_sim.seek+snapshot (scattered)", 5_000, |i| {
        let tick = (i as u32).wrapping_mul(7_919) % end;
        black_box(sim.seek(tick));
        black_box(sim.snapshot());
    });
    measure("replay_sim.seek (no snapshot)", 20_000, |i| {
        black_box(sim.seek((i as u32).wrapping_mul(7_919) % end));
    });
}

fn path_keys(count: usize, seconds: f32) -> Vec<PathKey> {
    (0..count)
        .map(|i| {
            let f = i as f32 / (count - 1) as f32;
            let a = f * std::f32::consts::TAU;
            PathKey {
                time: f * seconds,
                position: [40.0 * a.cos(), 5.0 + 3.0 * (2.0 * a).sin(), 40.0 * a.sin()],
            }
        })
        .collect()
}

fn sequence(interp: PathInterp, keys: usize) -> ShotSequence {
    let seconds = 30.0;
    let track = |a: f32, b: f32| {
        Track(vec![
            ScalarKey {
                time: 0.0,
                value: a,
                ease: Ease::default(),
            },
            ScalarKey {
                time: seconds,
                value: b,
                ease: Ease::default(),
            },
        ])
    };
    ShotSequence {
        shots: vec![Shot {
            name: "orbit".into(),
            duration: seconds,
            camera: ShotCamera {
                path: CameraPath {
                    interp,
                    keys: path_keys(keys, seconds),
                    look_at: LookAt::Track(path_keys(keys, seconds)),
                },
                fov: track(35.0, 60.0),
                roll: track(0.0, 5.0),
                dof: None,
                rotation: Vec::new(),
            },
            fade_in: 0.5,
            fade_out: 0.5,
            fade_color: [0.0; 3],
        }],
        ..ShotSequence::default()
    }
}

#[test]
#[ignore = "microbenchmark: run by hand, see docs/06-testing-and-ci.md § 9"]
fn camera_path_sampling() {
    for (label, interp) in [
        ("catmull_rom", PathInterp::CatmullRom),
        ("linear", PathInterp::Linear),
        ("step", PathInterp::Step),
    ] {
        for keys in [8, 256] {
            let seq = sequence(interp, keys);
            seq.validate().expect("valid benchmark sequence");
            let total = seq.duration();
            // 30 s at 60 fps = 1800 frames per pass; each frame is one sample.
            measure(&format!("shots.sample {label} keys={keys}"), 18_000, |i| {
                let t = (i % 1_800) as f64 / 1_800.0 * total;
                black_box(seq.sample(black_box(t)).expect("sample"));
            });
        }
    }
}
