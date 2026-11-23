// SPDX-License-Identifier: MPL-2.0
//! Time the all-pairs loop against `DetBroadphase` on a crowd (prints, asserts nothing but the
//! contact count): `cargo run --release -p regia_detsim --example broadphase_cost -- 900 60 [cell_size]
//! [margin]`. `--queries` times the range/nearest mix; `--shots [shots] [bursts] [cones]` (defaults
//! 200, 50, 900 per tick) times segment, burst and cone queries against linear scans. `--spread`
//! adds one body parked at (1e6, 1e6) and one of radius 60 m to the crowd and times the grid with
//! the old settings (no bounds, no large class) against `world_bounds` and the default large class
//! (ADR-8837).

// A host-side timing probe, never sim code: it reads its arguments and the clock (ADR-8367).
#![expect(
    clippy::disallowed_methods,
    reason = "timing probe: command-line arguments and Instant::now, outside any sim tick (ADR-8367)"
)]

use std::time::Instant;

use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, DetBroadphase, FloorRule, for_each_contact_n2};

fn crowd(n: usize) -> Vec<BroadBody> {
    let mut s: u64 = 0x5eed;
    let mut next = move || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (s >> 40) as f32 * (1.0 / 16_777_216.0)
    };
    // Discs of 0.5-2.5 m (about 9 m² each) covering about half the ground, like a big fight.
    let side = regia_detsim::math::sqrt(n as f32 * 18.0);
    (0..n)
        .map(|_| {
            let (x, z, r) = (next() * side, next() * side, 0.5 + next() * 2.0);
            BroadBody::new([x, 0.0, z], r, [-2.0, 4.0])
        })
        .collect()
}

fn push(bodies: &mut [BroadBody], c: regia_detsim::broadphase::Contact) {
    let dist = regia_detsim::math::sqrt(c.dist_sq);
    let (nx, nz) = if dist > 1e-4 {
        (c.dx / dist, c.dz / dist)
    } else {
        (1.0, 0.0)
    };
    let half = (c.min_dist - dist) * 0.5;
    bodies[c.a].pos[0] -= nx * half;
    bodies[c.a].pos[2] -= nz * half;
    bodies[c.b].pos[0] += nx * half;
    bodies[c.b].pos[2] += nz * half;
}

fn main() {
    let queries = std::env::args().any(|arg| arg == "--queries");
    if let Some(at) = std::env::args().position(|arg| arg == "--shots") {
        let rest: Vec<usize> = std::env::args()
            .skip(at + 1)
            .map_while(|a| a.parse().ok())
            .collect();
        let mut head = std::env::args()
            .skip(1)
            .take(at - 1)
            .filter_map(|a| a.parse::<usize>().ok());
        let n = head.next().unwrap_or(900);
        let passes = head.next().unwrap_or(600).max(1);
        let mix = ShotMix {
            shots: rest.first().copied().unwrap_or(200),
            bursts: rest.get(1).copied().unwrap_or(50),
            cones: rest.get(2).copied().unwrap_or(900),
        };
        if let Err(error) = shot_mix(n, passes, mix) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    let spread = std::env::args().any(|arg| arg == "--spread");
    let mut args = std::env::args()
        .skip(1)
        .filter(|arg| arg != "--queries" && arg != "--spread");
    let n: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(900);
    let passes: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(60);
    let defaults = BroadphaseConfig::default();
    let cell_size: f32 = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(defaults.cell_size);
    let margin: f32 = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(defaults.margin);

    if queries {
        if let Err(error) = query_mix(n, passes.max(1), cell_size, margin) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }

    if spread {
        spread_mix(n, passes.max(1), cell_size, margin);
        return;
    }

    let mut a = crowd(n);
    let start = Instant::now();
    let mut contacts_n2 = 0_u64;
    for _ in 0..passes {
        for_each_contact_n2(&mut a, FloorRule::Bands, |b, c| {
            contacts_n2 += 1;
            push(b, c);
        });
    }
    let n2 = start.elapsed();

    let mut b = crowd(n);
    let mut grid = DetBroadphase::new(BroadphaseConfig {
        cell_size,
        margin,
        ..defaults
    });
    let start = Instant::now();
    let mut contacts_grid = 0_u64;
    for _ in 0..passes {
        grid.for_each_contact(&mut b, |b, c| {
            contacts_grid += 1;
            push(b, c);
        });
    }
    let g = start.elapsed();

    assert_eq!(contacts_n2, contacts_grid, "the loops disagree");
    assert_eq!(a, b, "the loops disagree");
    let per = |d: std::time::Duration| d.as_secs_f64() * 1e3 / f64::from(passes);
    println!(
        "{n} bodies, {passes} passes, {} contacts/pass: all-pairs {:.3} ms/pass, grid {:.3} ms/pass ({:?})",
        contacts_grid / u64::from(passes),
        per(n2),
        per(g),
        grid.stats()
    );
}

/// The crowd plus one parked and one large body, through the grid with the old settings and with
/// `world_bounds` and the default large class: same contacts, fewer candidate tests (ADR-8837).
fn spread_mix(n: usize, passes: u32, cell_size: f32, margin: f32) {
    let mut bodies = crowd(n);
    let side = regia_detsim::math::sqrt(n as f32 * 18.0);
    bodies.push(BroadBody::new([1.0e6, 0.0, 1.0e6], 1.0, [-2.0, 4.0]));
    bodies.push(BroadBody::new([side * 0.5, 0.0, side * 0.5], 60.0, [-2.0, 4.0]));
    let base = BroadphaseConfig {
        cell_size,
        margin,
        ..BroadphaseConfig::default()
    };
    let old = BroadphaseConfig {
        large_radius: Some(f32::INFINITY),
        ..base
    };
    let new = BroadphaseConfig {
        world_bounds: Some([-10.0, -10.0, side + 10.0, side + 10.0]),
        ..base
    };
    let mut results = Vec::new();
    for (name, config) in [("old settings", old), ("bounds + large class", new)] {
        let mut b = bodies.clone();
        let mut grid = DetBroadphase::new(config);
        let mut contacts = 0_u64;
        let mut candidates = 0_u64;
        let start = Instant::now();
        for _ in 0..passes {
            grid.for_each_contact(&mut b, |b, c| {
                contacts += 1;
                push(b, c);
            });
            candidates += grid.stats().candidates;
        }
        let ms = start.elapsed().as_secs_f64() * 1e3 / f64::from(passes);
        println!(
            "{} bodies (one parked, one 60 m), {passes} passes, {name}: {ms:.3} ms/pass, {} candidates/pass, {} contacts/pass ({:?})",
            b.len(),
            candidates / u64::from(passes),
            contacts / u64::from(passes),
            grid.stats()
        );
        results.push((contacts, b));
    }
    assert!(results[0] == results[1], "the settings disagree");
}

/// One shared query build, 900 range queries, 900 nearest queries and the pair pass.
/// A 30 m mean spacing distinguishes this field from the dense collision crowd.
fn query_mix(
    n: usize,
    passes: u32,
    pair_cell: f32,
    margin: f32,
) -> Result<(), regia_detsim::spatial::SpatialError> {
    use regia_detsim::spatial::{DetSpatial, SpatialRow};
    use std::hint::black_box;
    use std::ops::ControlFlow;

    let mut bodies = crowd(n);
    let spread = 30.0 / regia_detsim::math::sqrt(18.0);
    for body in &mut bodies {
        body.pos[0] *= spread;
        body.pos[2] *= spread;
    }
    let mut rows: Vec<_> = bodies
        .iter()
        .enumerate()
        .map(|(slot, body)| SpatialRow {
            slot,
            body: BroadBody { radius: 0.0, ..*body },
            group_mask: 1 << (slot % 40),
            flags: 0,
        })
        .collect();
    let mut spatial = DetSpatial::new(BroadphaseConfig {
        cell_size: 40.0,
        ..Default::default()
    });
    let mut pairs = DetBroadphase::new(BroadphaseConfig {
        cell_size: pair_cell,
        margin,
        ..Default::default()
    });
    let mut query_times = Vec::with_capacity(passes as usize);
    let mut total_times = Vec::with_capacity(passes as usize);
    let mut build_times = Vec::with_capacity(passes as usize);
    let mut within_times = Vec::with_capacity(passes as usize);
    let mut nearest_times = Vec::with_capacity(passes as usize);
    let mut pair_times = Vec::with_capacity(passes as usize);
    let mut checksum = 0_u64;
    // Ten warmup ticks are excluded from the reported percentiles.
    for tick in 0..passes.saturating_add(10) {
        for (row, body) in rows.iter_mut().zip(&bodies) {
            row.body.pos = body.pos;
        }
        spatial.reset_stats();
        let begin = Instant::now();
        spatial.build(&rows)?;
        let built = Instant::now();
        for q in &rows {
            let mask = !q.group_mask;
            let _ = spatial.within(q.body, 80.0, mask, |slot| {
                let p = rows[slot].body.pos;
                let dx = p[0] - q.body.pos[0];
                let dz = p[2] - q.body.pos[2];
                if dx * dx + dz * dz <= 80.0 * 80.0 {
                    checksum = checksum.wrapping_add(slot as u64);
                }
                ControlFlow::Continue(())
            });
        }
        let ranged = Instant::now();
        for q in &rows {
            let found = spatial.nearest(q.body, !q.group_mask, 120.0, |slot| {
                let p = rows[slot].body.pos;
                let dx = p[0] - q.body.pos[0];
                let dz = p[2] - q.body.pos[2];
                dx * dx + dz * dz
            });
            checksum = checksum.wrapping_add(found.map_or(0, |s| s as u64 + 1));
        }
        let queried = Instant::now();
        pairs.for_each_contact(&mut bodies, push);
        let ended = Instant::now();
        if tick >= 10 {
            query_times.push(queried.duration_since(begin));
            total_times.push(ended.duration_since(begin));
            build_times.push(built.duration_since(begin));
            within_times.push(ranged.duration_since(built));
            nearest_times.push(queried.duration_since(ranged));
            pair_times.push(ended.duration_since(queried));
        }
        black_box(checksum);
    }
    fn percentiles(times: &mut [std::time::Duration]) -> (f64, f64) {
        times.sort_unstable();
        let n = times.len();
        (
            times[(n / 2).min(n - 1)].as_secs_f64() * 1e3,
            times[(n * 99).div_ceil(100).saturating_sub(1).min(n - 1)].as_secs_f64() * 1e3,
        )
    }
    for (name, times) in [
        ("build", &mut build_times),
        ("within", &mut within_times),
        ("nearest", &mut nearest_times),
        ("query_index", &mut query_times),
        ("pairs", &mut pair_times),
        ("total", &mut total_times),
    ] {
        let (p50, p99) = percentiles(times);
        println!(
            "{{\"workload\":\"spatial_queries\",\"pass\":\"{name}\",\"rows\":{n},\"samples\":{passes},\"p50_ms\":{p50:.6},\"p99_ms\":{p99:.6}}}"
        );
    }
    println!(
        "stats={:?} pair_candidates={} checksum={checksum}",
        spatial.stats(),
        pairs.stats().candidates
    );
    Ok(())
}

/// Per-tick query counts for `--shots`.
#[derive(Clone, Copy)]
struct ShotMix {
    shots: usize,
    bursts: usize,
    cones: usize,
}

/// One build, `shots` projectile segments (first hit in slot order), `bursts` 12 m bursts and
/// `cones` friendly-fire cones per tick, through the index and through linear scans of the same
/// exact tests. The two checksums must agree; the example exits non-zero if they do not.
fn shot_mix(n: usize, passes: usize, mix: ShotMix) -> Result<(), regia_detsim::spatial::SpatialError> {
    use regia_detsim::math::{cos, sin, sqrt};
    use regia_detsim::spatial::{DetSpatial, SpatialRow};
    use std::hint::black_box;
    use std::ops::ControlFlow;

    let mut bodies = crowd(n);
    let spread = 30.0 / sqrt(18.0);
    for body in &mut bodies {
        body.pos[0] *= spread;
        body.pos[2] *= spread;
    }
    let rows: Vec<_> = bodies
        .iter()
        .enumerate()
        .map(|(slot, body)| SpatialRow {
            slot,
            body: BroadBody { radius: 0.5, ..*body },
            group_mask: 1 << (slot % 2),
            flags: 0,
        })
        .collect();
    let mut spatial = DetSpatial::new(BroadphaseConfig {
        cell_size: 40.0,
        ..Default::default()
    });
    // The game's exact tests.
    let capsule = |a: [f32; 2], b: [f32; 2], r: f32, row: &SpatialRow| {
        let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
        let (px, pz) = (row.body.pos[0] - a[0], row.body.pos[2] - a[1]);
        let mut t = (px * dx + pz * dz) / (dx * dx + dz * dz);
        if t.is_nan() || t <= 0.0 {
            t = 0.0;
        } else if t > 1.0 {
            t = 1.0;
        }
        let (ex, ez) = (px - t * dx, pz - t * dz);
        let reach = r + row.body.radius;
        ex * ex + ez * ez <= reach * reach
    };
    let disc = |c: [f32; 2], r: f32, row: &SpatialRow| {
        let (dx, dz) = (row.body.pos[0] - c[0], row.body.pos[2] - c[1]);
        let reach = r + row.body.radius;
        dx * dx + dz * dz <= reach * reach
    };
    let cone = |c: [f32; 2], dir: [f32; 2], cos_half: f32, r: f32, row: &SpatialRow| {
        let (vx, vz) = (row.body.pos[0] - c[0], row.body.pos[2] - c[1]);
        let d2 = vx * vx + vz * vz;
        let reach = r + row.body.radius;
        d2 <= reach * reach && (d2 == 0.0 || vx * dir[0] + vz * dir[1] >= cos_half * sqrt(d2))
    };
    struct Shot {
        shooter: usize,
        to: [f32; 2],
        dir: [f32; 2],
    }
    let mut times: [Vec<std::time::Duration>; 6] = Default::default();
    let (mut indexed, mut linear) = (0_u64, 0_u64);
    let mut seed = 0x5407_u64;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 40) as f32 * (1.0 / 16_777_216.0)
    };
    let mut shots = Vec::with_capacity(mix.shots.max(mix.cones));
    for tick in 0..passes + 10 {
        shots.clear();
        for i in 0..mix.shots.max(mix.cones) {
            let shooter = (i * 7 + tick) % n;
            let angle = next() * std::f32::consts::TAU;
            let dir = [cos(angle), sin(angle)];
            // Muzzle velocity 400 m/s at 60 Hz plus a few long hitscan rays.
            let len = if i % 10 == 0 { 400.0 } else { 400.0 / 60.0 };
            let p = rows[shooter].body.pos;
            shots.push(Shot {
                shooter,
                to: [p[0] + dir[0] * len, p[2] + dir[1] * len],
                dir,
            });
        }
        let t0 = Instant::now();
        spatial.build(&rows)?;
        let t1 = Instant::now();
        for s in &shots[..mix.shots] {
            let from = rows[s.shooter].body;
            let a = [from.pos[0], from.pos[2]];
            let mask = !rows[s.shooter].group_mask;
            let hit = spatial
                .first_along_segment(from, s.to, 0.05, mask, |slot| capsule(a, s.to, 0.05, &rows[slot]));
            indexed = indexed.wrapping_add(hit.map_or(0, |h| h as u64 + 1));
        }
        let t2 = Instant::now();
        for s in &shots[..mix.bursts] {
            let c = rows[s.shooter].body;
            let _ = spatial.within(c, 12.0, u64::MAX, |slot| {
                if disc([c.pos[0], c.pos[2]], 12.0, &rows[slot]) {
                    indexed = indexed.wrapping_add(slot as u64);
                }
                ControlFlow::Continue(())
            });
        }
        let t3 = Instant::now();
        for s in &shots[..mix.cones] {
            let apex = rows[s.shooter].body;
            let c = [apex.pos[0], apex.pos[2]];
            let mask = rows[s.shooter].group_mask;
            let _ = spatial.within_sector(apex, s.dir, 0.985, 100.0, mask, |slot| {
                if slot != s.shooter && cone(c, s.dir, 0.985, 100.0, &rows[slot]) {
                    indexed = indexed.wrapping_add(slot as u64 + 1);
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            });
        }
        let t4 = Instant::now();
        // The same work as linear scans in slot order.
        for s in &shots[..mix.shots] {
            let a = [rows[s.shooter].body.pos[0], rows[s.shooter].body.pos[2]];
            let mask = !rows[s.shooter].group_mask;
            let hit = rows
                .iter()
                .position(|row| row.group_mask & mask != 0 && capsule(a, s.to, 0.05, row));
            linear = linear.wrapping_add(hit.map_or(0, |h| h as u64 + 1));
        }
        for s in &shots[..mix.bursts] {
            let c = rows[s.shooter].body.pos;
            for row in &rows {
                if disc([c[0], c[2]], 12.0, row) {
                    linear = linear.wrapping_add(row.slot as u64);
                }
            }
        }
        for s in &shots[..mix.cones] {
            let c = rows[s.shooter].body.pos;
            let mask = rows[s.shooter].group_mask;
            if let Some(row) = rows.iter().find(|row| {
                row.group_mask & mask != 0
                    && row.slot != s.shooter
                    && cone([c[0], c[2]], s.dir, 0.985, 100.0, row)
            }) {
                linear = linear.wrapping_add(row.slot as u64 + 1);
            }
        }
        let t5 = Instant::now();
        if tick >= 10 {
            for (slot, d) in times
                .iter_mut()
                .zip([t1 - t0, t2 - t1, t3 - t2, t4 - t3, t4 - t0, t5 - t4])
            {
                slot.push(d);
            }
        }
        black_box((indexed, linear));
    }
    for (name, samples) in [
        "build",
        "segments",
        "bursts",
        "cones",
        "index_total",
        "linear_total",
    ]
    .into_iter()
    .zip(times.iter_mut())
    {
        samples.sort_unstable();
        let k = samples.len();
        let p50 = samples[(k / 2).min(k - 1)].as_secs_f64() * 1e3;
        let p99 = samples[(k * 99).div_ceil(100).saturating_sub(1).min(k - 1)].as_secs_f64() * 1e3;
        println!(
            "{{\"workload\":\"spatial_shots\",\"pass\":\"{name}\",\"rows\":{n},\"shots\":{},\"bursts\":{},\"cones\":{},\"samples\":{passes},\"p50_ms\":{p50:.6},\"p99_ms\":{p99:.6}}}",
            mix.shots, mix.bursts, mix.cones
        );
    }
    println!(
        "stats={:?} checksum={indexed} linear_checksum={linear}",
        spatial.stats()
    );
    if indexed != linear {
        eprintln!("index and linear scan disagree");
        std::process::exit(2);
    }
    Ok(())
}
