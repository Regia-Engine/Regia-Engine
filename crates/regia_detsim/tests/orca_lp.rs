// SPDX-License-Identifier: MPL-2.0
//! The opt-in linear-program avoidance solver and static wall segments (ADR-8555).
//!
//! Run in both profiles: `cargo test -p regia_detsim --test orca_lp` and the same with
//! `--release`. The timing evidence is ignored by default:
//! `cargo test --release -p regia_detsim --test orca_lp crowd_900_lp_timing_evidence -- --ignored --nocapture`.
//!
//! Installs a counting global allocator (thread-local counter, opt-in per thread).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_detsim::avoid::{
    ORCA_LP_UNIT, ORCA_LP_VELOCITY_SCALE, ORCA_MAX_HORIZON_MS, ORCA_MAX_NEIGHBOR_PLANES,
    ORCA_MAX_POSITION_MM, ORCA_MAX_RADIUS_MM, ORCA_MAX_SEGMENTS, ORCA_MAX_SPEED_MM_S, OrcaBody,
    OrcaDomainError, OrcaHalfPlane, OrcaScratch, OrcaSegment, OrcaSettings, OrcaSolver, orca_velocity,
    orca_velocity_into, orca_velocity_obstacles_into, orca_velocity_with_settings,
    try_orca_velocity_obstacles_into,
};

// ---------------------------------------------------------------------------
// Counting allocator: counts only allocations made on a thread that opted in.

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

fn note_alloc() {
    let _ = COUNTING.try_with(|on| {
        if on.get() {
            let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
        }
    });
}

// SAFETY: forwards every call unchanged to the system allocator; the counter
// touches only const-initialized thread-local cells, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_alloc();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_alloc();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_alloc();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn count_allocs<R>(f: impl FnOnce() -> R) -> (R, u64) {
    ALLOCS.with(|n| n.set(0));
    COUNTING.with(|on| on.set(true));
    let r = f();
    COUNTING.with(|on| on.set(false));
    (r, ALLOCS.with(Cell::get))
}

// ---------------------------------------------------------------------------
// Shared helpers.

/// The source corpus generator (orca_bits.rs), so the Stage A pin is checked here too.
struct Lcg(u32);

impl Lcg {
    fn next(&mut self, span: i64) -> i64 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        i64::from(self.0) % (span * 2 + 1) - span
    }
}

fn fnv(hash: &mut u64, v: i64) {
    for byte in v.to_le_bytes() {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x100_0000_01b3);
    }
}

fn body(id: u32, x_mm: i64, z_mm: i64, vx_mm_s: i64, vz_mm_s: i64, r_mm: i64) -> OrcaBody {
    OrcaBody {
        id,
        x_mm,
        z_mm,
        vx_mm_s,
        vz_mm_s,
        r_mm,
    }
}

fn seg(ax: i64, az: i64, bx: i64, bz: i64) -> OrcaSegment {
    OrcaSegment {
        a_mm: [ax, az],
        b_mm: [bx, bz],
    }
}

fn lp(max_speed_mm_s: i64) -> OrcaSettings {
    OrcaSettings {
        solver: OrcaSolver::Lp,
        max_speed_mm_s,
        ..OrcaSettings::default()
    }
}

/// The test's own exact check, from the public fields only: the velocity `v` (mm/s)
/// lies on the free side of `plane` when `det(direction, v * scale - point) >= 0`.
fn admits(plane: &OrcaHalfPlane, v: [i64; 2]) -> bool {
    signed(plane, v) >= 0
}

fn signed(plane: &OrcaHalfPlane, v: [i64; 2]) -> i128 {
    let s = i128::from(ORCA_LP_VELOCITY_SCALE);
    let x = i128::from(v[0]) * s - i128::from(plane.point_q[0]);
    let z = i128::from(v[1]) * s - i128::from(plane.point_q[1]);
    i128::from(plane.direction[0]) * z - i128::from(plane.direction[1]) * x
}

/// Penetration of `v` into `plane` in mm/s (f64, test-side reference only).
fn penetration_mm_s(plane: &OrcaHalfPlane, v: [f64; 2]) -> f64 {
    let s = ORCA_LP_VELOCITY_SCALE as f64;
    let d = [plane.direction[0] as f64, plane.direction[1] as f64];
    let len = libm::sqrt(d[0] * d[0] + d[1] * d[1]);
    let p = [plane.point_q[0] as f64 / s, plane.point_q[1] as f64 / s];
    // det(d, p - v) / |d|: positive inside the forbidden side.
    (d[0] * (p[1] - v[1]) - d[1] * (p[0] - v[0])) / len
}

/// Exact 50 Hz kinematics: positions kept in micrometers (v mm/s * 20 ms = 20 v um),
/// the solver sees them rounded to the nearest millimeter.
fn step(b: &mut OrcaBody, um: &mut [i64; 2], v: [i64; 2]) {
    b.vx_mm_s = v[0];
    b.vz_mm_s = v[1];
    um[0] += v[0] * 20;
    um[1] += v[1] * 20;
    b.x_mm = (um[0] + 500 * um[0].signum()) / 1_000;
    b.z_mm = (um[1] + 500 * um[1].signum()) / 1_000;
}

fn speed2(v: [i64; 2]) -> i128 {
    i128::from(v[0]) * i128::from(v[0]) + i128::from(v[1]) * i128::from(v[1])
}

fn isqrt_ceil(v: i128) -> i128 {
    let s = isqrt(v);
    if s * s < v { s + 1 } else { s }
}

fn isqrt(v: i128) -> i128 {
    let mut x = libm::sqrt(v as f64) as i128;
    while x * x > v {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= v {
        x += 1;
    }
    x
}

/// A random scene: a mover, up to 12 neighbors within 12 m and up to 6 wall segments.
fn random_scene(rng: &mut Lcg) -> (OrcaBody, [i64; 2], Vec<OrcaBody>, Vec<OrcaSegment>, i64) {
    let me = body(
        1,
        rng.next(1_000_000),
        rng.next(1_000_000),
        rng.next(3_000),
        rng.next(3_000),
        200 + rng.next(800).abs(),
    );
    let preferred = [rng.next(3_000), rng.next(3_000)];
    let n = rng.next(12).unsigned_abs() as u32;
    let others = (0..n)
        .map(|k| {
            body(
                k + 2,
                me.x_mm + rng.next(12_000),
                me.z_mm + rng.next(12_000),
                rng.next(3_000),
                rng.next(3_000),
                200 + rng.next(800).abs(),
            )
        })
        .collect();
    let s = rng.next(6).unsigned_abs();
    let segments = (0..s)
        .map(|_| {
            let ax = me.x_mm + rng.next(8_000);
            let az = me.z_mm + rng.next(8_000);
            seg(ax, az, ax + rng.next(10_000), az + rng.next(10_000))
        })
        .collect();
    let max_speed = rng.next(4_000).abs();
    (me, preferred, others, segments, max_speed)
}

// ---------------------------------------------------------------------------
// Acceptance.

/// The Stage A kernel is untouched: the source corpus pin holds through every
/// entry point, with the new settings fields at non-default values and with wall
/// segments passed (Stage A ignores them).
#[test]
fn stage_a_is_untouched() {
    let stage_a = OrcaSettings {
        solver: OrcaSolver::StageA,
        max_speed_mm_s: 77,
        obstacle_horizon_ms: 5,
        contact_ms: 3,
        ..OrcaSettings::default()
    };
    assert_eq!(
        OrcaSettings::default().solver,
        OrcaSolver::StageA,
        "the default stays Stage A"
    );
    let walls = [seg(-1_000, 2_000, 1_000, 2_000), seg(0, -5_000, 0, 5_000)];
    let mut rng = Lcg(0x2545_f491);
    let mut scratch = OrcaScratch::new();
    let (mut plain, mut into, mut with_walls) = (
        0xcbf2_9ce4_8422_2325_u64,
        0xcbf2_9ce4_8422_2325_u64,
        0xcbf2_9ce4_8422_2325_u64,
    );
    for case in 0..2048 {
        let me = body(
            1,
            rng.next(1_000_000),
            rng.next(1_000_000),
            rng.next(20_000),
            rng.next(20_000),
            1 + rng.next(2500).abs(),
        );
        let preferred = [rng.next(20_000), rng.next(20_000)];
        let others: Vec<_> = (0..case % 33)
            .map(|index| {
                body(
                    index + 2,
                    me.x_mm + rng.next(25_000),
                    me.z_mm + rng.next(25_000),
                    rng.next(20_000),
                    rng.next(20_000),
                    1 + rng.next(2500).abs(),
                )
            })
            .collect();
        let a = orca_velocity(&me, preferred, &others);
        let b = orca_velocity_into(&mut scratch, &me, preferred, &others, &stage_a);
        let c = orca_velocity_obstacles_into(&mut scratch, &me, preferred, &others, &walls, &stage_a);
        for v in a {
            fnv(&mut plain, v);
        }
        for v in b {
            fnv(&mut into, v);
        }
        for v in c {
            fnv(&mut with_walls, v);
        }
    }
    println!("stage A corpus: {plain:#018x} / {into:#018x} / {with_walls:#018x}");
    assert_eq!(plain, 0xf1a1_97b3_f232_f005, "Stage A corpus bits moved");
    assert_eq!(
        into, 0xf1a1_97b3_f232_f005,
        "Stage A through settings with Lp fields set"
    );
    assert_eq!(with_walls, 0xf1a1_97b3_f232_f005, "Stage A with segments passed");
    assert!(
        scratch.last_lp().is_none(),
        "Stage A calls never report an LP outcome"
    );
    assert_eq!(scratch.lp_stats().stage_a_segments_ignored, 2048 * 2);
}

/// The first seeded scene (seed 0x51ed_0a11) where Stage A's single fixed-order
/// pass leaves one of its own half-planes violated while the LP over the same scene
/// is feasible. The index and the scene are printed and recorded in the lane report.
fn stage_a_counterexample() -> (usize, OrcaBody, [i64; 2], Vec<OrcaBody>) {
    let mut rng = Lcg(0x51ed_0a11);
    let mut a = OrcaScratch::new();
    let mut l = OrcaScratch::new();
    for index in 0..100_000 {
        let me = body(1, 0, 0, rng.next(2_000), rng.next(2_000), 500);
        let preferred = [rng.next(2_000), rng.next(2_000)];
        let others: Vec<_> = (0..3)
            .map(|k| {
                body(
                    k + 2,
                    rng.next(4_000),
                    rng.next(4_000),
                    rng.next(2_000),
                    rng.next(2_000),
                    500,
                )
            })
            .collect();
        let v = orca_velocity_into(&mut a, &me, preferred, &others, &OrcaSettings::default());
        let broken = a
            .stage_a_planes()
            .iter()
            .any(|(n, p)| (v[0] - p[0]) * n[0] + (v[1] - p[1]) * n[1] < 0);
        // Only scenes where the clamp did not cut the velocity: the violation comes from the pass order.
        let unclamped = speed2(v) < speed2(preferred);
        orca_velocity_into(&mut l, &me, preferred, &others, &lp(0));
        if broken && unclamped && l.last_lp().is_some_and(|o| o.feasible) {
            return (index, me, preferred, others);
        }
    }
    panic!("no Stage A counterexample in 100,000 scenes");
}

/// Seeded 10,000 scenes: whenever the LP reports feasible, the chosen velocity is on
/// the free side of every half-plane it built (the test's own exact integer check)
/// and inside the speed circle. Stage A breaks its own half-planes on a known case.
#[test]
fn lp_result_satisfies_every_feasible_constraint() {
    let mut rng = Lcg(0x0c0f_fee5);
    let mut scratch = OrcaScratch::new();
    let (mut feasible, mut infeasible, mut planes, mut obstacle_planes) = (0u32, 0u32, 0u64, 0u64);
    for case in 0..10_000 {
        let (me, preferred, others, segments, max_speed) = random_scene(&mut rng);
        let v =
            orca_velocity_obstacles_into(&mut scratch, &me, preferred, &others, &segments, &lp(max_speed));
        let outcome = scratch.last_lp().expect("an Lp call reports its outcome");
        let built = scratch.lp_planes();
        assert_eq!(outcome.planes as usize, built.len());
        planes += built.len() as u64;
        obstacle_planes += built.iter().filter(|p| p.obstacle).count() as u64;
        let cap = max_speed.max(isqrt_ceil(speed2(preferred)) as i64);
        assert!(
            speed2(v) <= i128::from(cap) * i128::from(cap),
            "case {case}: {v:?} outside the speed circle {cap}"
        );
        if outcome.feasible {
            feasible += 1;
            assert_eq!(outcome.worst_violation_mm_s, 0, "case {case}");
            for (k, plane) in built.iter().enumerate() {
                assert!(
                    admits(plane, v),
                    "case {case}: feasible LP result {v:?} violates plane {k} {plane:?} by {}",
                    signed(plane, v)
                );
            }
        } else {
            infeasible += 1;
            let worst = built.iter().map(|p| -signed(p, v)).fold(0, i128::max);
            let unit_q = i128::from(ORCA_LP_UNIT) * i128::from(ORCA_LP_VELOCITY_SCALE);
            let want = (worst + unit_q - 1) / unit_q;
            assert_eq!(
                i128::from(outcome.worst_violation_mm_s),
                want,
                "case {case}: reported worst violation"
            );
        }
    }
    println!(
        "10,000 scenes: {feasible} feasible, {infeasible} fallback; {planes} planes ({obstacle_planes} from walls)"
    );
    assert!(feasible > 5_000, "most random scenes are feasible");
    assert!(infeasible > 0, "the fallback path is exercised");
    assert!(obstacle_planes > 1_000, "wall planes are exercised");

    // The recorded Stage A case: its single fixed-order pass leaves an earlier plane
    // violated, while the LP over the same scene is feasible and satisfies all of its own.
    let (index, me, preferred, others) = stage_a_counterexample();
    println!("Stage A counterexample at index {index}: me {me:?} preferred {preferred:?} others {others:?}");
    let mut a = OrcaScratch::new();
    let stage_a = orca_velocity_into(&mut a, &me, preferred, &others, &OrcaSettings::default());
    let violated: Vec<_> = a
        .stage_a_planes()
        .iter()
        .filter(|(n, p)| (stage_a[0] - p[0]) * n[0] + (stage_a[1] - p[1]) * n[1] < 0)
        .collect();
    println!("Stage A counterexample: velocity {stage_a:?}, violated planes {violated:?}");
    assert!(
        !violated.is_empty(),
        "Stage A leaves one of its own half-planes violated here"
    );
    let v = orca_velocity_into(&mut scratch, &me, preferred, &others, &lp(0));
    let outcome = scratch.last_lp().expect("lp outcome");
    assert!(outcome.feasible, "the LP finds a velocity for the same scene");
    assert!(scratch.lp_planes().iter().all(|p| admits(p, v)), "LP {v:?}");
}

/// Larger of two finite test values (the lockstep clippy config denies `f64::max`).
fn fmax(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// Reference minimax in f64 (test-only): the smallest worst penetration over the speed
/// disc, from the candidate vertices of the arrangement (triples with equal
/// penetration, pair bisectors on the circle, single-plane tangent points).
fn reference_minimax(planes: &[OrcaHalfPlane], radius: f64) -> f64 {
    let s = ORCA_LP_VELOCITY_SCALE as f64;
    // Each plane as n . x >= c with a unit normal n (into the free side).
    let rows: Vec<([f64; 2], f64)> = planes
        .iter()
        .map(|p| {
            let d = [p.direction[0] as f64, p.direction[1] as f64];
            let len = libm::sqrt(d[0] * d[0] + d[1] * d[1]);
            let n = [-d[1] / len, d[0] / len];
            let pt = [p.point_q[0] as f64 / s, p.point_q[1] as f64 / s];
            (n, n[0] * pt[0] + n[1] * pt[1])
        })
        .collect();
    let worst = |x: [f64; 2]| {
        rows.iter()
            .map(|(n, c)| c - (n[0] * x[0] + n[1] * x[1]))
            .fold(f64::NEG_INFINITY, fmax)
    };
    let mut cands: Vec<[f64; 2]> = vec![[0.0, 0.0]];
    for (n, _) in &rows {
        cands.push([n[0] * radius, n[1] * radius]);
    }
    for i in 0..rows.len() {
        for j in i + 1..rows.len() {
            // Bisector: (n_i - n_j) . x = c_i - c_j, intersected with the circle.
            let (ni, ci) = rows[i];
            let (nj, cj) = rows[j];
            let a = [ni[0] - nj[0], ni[1] - nj[1]];
            let b = ci - cj;
            let aa = a[0] * a[0] + a[1] * a[1];
            if aa > 1e-18 {
                let foot = [a[0] * b / aa, a[1] * b / aa];
                let h2 = radius * radius - (foot[0] * foot[0] + foot[1] * foot[1]);
                if h2 >= 0.0 {
                    let h = libm::sqrt(h2) / libm::sqrt(aa);
                    let t = [-a[1], a[0]];
                    cands.push([foot[0] + t[0] * h, foot[1] + t[1] * h]);
                    cands.push([foot[0] - t[0] * h, foot[1] - t[1] * h]);
                }
            }
            for &(nk, ck) in &rows[j + 1..] {
                // (n_i - n_j) . x = c_i - c_j ; (n_i - n_k) . x = c_i - c_k
                let a1 = [ni[0] - nj[0], ni[1] - nj[1]];
                let a2 = [ni[0] - nk[0], ni[1] - nk[1]];
                let det = a1[0] * a2[1] - a1[1] * a2[0];
                if det.abs() > 1e-12 {
                    let b1 = ci - cj;
                    let b2 = ci - ck;
                    cands.push([(b1 * a2[1] - a1[1] * b2) / det, (a1[0] * b2 - b1 * a2[0]) / det]);
                }
            }
        }
    }
    cands
        .into_iter()
        .filter(|x| x[0] * x[0] + x[1] * x[1] <= radius * radius * (1.0 + 1e-12) + 1e-9)
        .map(worst)
        .fold(f64::INFINITY, |a, b| if b < a { b } else { a })
}

/// When the half-planes leave no velocity, the fallback picks one whose worst
/// penetration is the smallest possible within the speed circle (within 3 mm/s of
/// an independent f64 minimax), and two walls closer than a body's radius hold it
/// still across the squeeze.
#[test]
fn infeasible_case_minimizes_the_worst_violation() {
    let mut rng = Lcg(0x1f2e_3d4c);
    let mut scratch = OrcaScratch::new();
    let mut checked = 0;
    for case in 0..20_000 {
        // Crowded agent-only scenes: every plane is soft in the fallback.
        let me = body(1, 0, 0, rng.next(2_000), rng.next(2_000), 500);
        let preferred = [rng.next(2_000), rng.next(2_000)];
        let others: Vec<_> = (0..8)
            .map(|k| {
                body(
                    k + 2,
                    rng.next(2_500),
                    rng.next(2_500),
                    rng.next(2_000),
                    rng.next(2_000),
                    500,
                )
            })
            .collect();
        let max_speed = 1_000 + rng.next(2_000).abs();
        let v = orca_velocity_into(&mut scratch, &me, preferred, &others, &lp(max_speed));
        let outcome = scratch.last_lp().expect("lp outcome");
        if outcome.feasible {
            continue;
        }
        checked += 1;
        let planes = scratch.lp_planes();
        let radius = max_speed.max(isqrt_ceil(speed2(preferred)) as i64) as f64;
        let best = reference_minimax(planes, radius);
        let ours = planes
            .iter()
            .map(|p| penetration_mm_s(p, [v[0] as f64, v[1] as f64]))
            .fold(f64::NEG_INFINITY, fmax);
        assert!(
            best > -2.5,
            "case {case}: the reference finds slack beyond the 2 mm/s margin ({best})"
        );
        assert!(
            ours <= best + 3.0,
            "case {case}: worst penetration {ours} mm/s vs the minimax {best} mm/s ({v:?})"
        );
        assert!(
            (outcome.worst_violation_mm_s as f64 - ours).abs() <= 1.0,
            "case {case}: reported worst {} vs measured {ours}",
            outcome.worst_violation_mm_s
        );
        if checked >= 500 {
            break;
        }
    }
    println!("infeasible scenes checked against the f64 minimax: {checked}");
    assert!(checked >= 100, "enough infeasible scenes ({checked})");

    // Squeezed between two walls 1.6 m apart with a 1 m radius: each wall asks for
    // an escape speed away from it, which cannot both hold; the fallback balances them.
    let me = body(1, 0, 0, 0, 0, 1_000);
    let walls = [seg(-5_000, 800, 5_000, 800), seg(-5_000, -800, 5_000, -800)];
    let v = orca_velocity_obstacles_into(&mut scratch, &me, [1_000, 0], &[], &walls, &lp(2_000));
    let outcome = scratch.last_lp().expect("lp outcome");
    assert!(!outcome.feasible, "both walls cannot be escaped at once");
    assert!(v[1].abs() <= 2, "the squeeze balances across the corridor: {v:?}");
    assert!(outcome.worst_violation_mm_s > 0);
}

/// A crossing body that would clip the mover from behind: Stage A and the LP with the
/// default speed cap stay at the preferred speed or below, the LP with a higher
/// `max_speed_mm_s` speeds up past it, and the pair then never overlaps.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "ORCA_LP_DEBUG only prints the run for review; the assertions are the same (ADR-8367)"
)]
fn an_agent_speeds_up_to_dodge_when_max_speed_allows() {
    let me = body(1, 0, 0, 1_500, 0, 500);
    let crosser = body(2, 1_300, 3_000, 0, -3_000, 500);
    let preferred = [1_500, 0];
    let mut scratch = OrcaScratch::new();
    // No head-on bias, so only the speed cap differs (Stage A caps at the biased preference).
    let no_bias = |s: OrcaSettings| OrcaSettings {
        keep_right_mm_s: 0,
        ..s
    };

    let stage_a = orca_velocity_into(
        &mut scratch,
        &me,
        preferred,
        &[crosser],
        &no_bias(OrcaSettings::default()),
    );
    assert!(
        speed2(stage_a) <= speed2(preferred),
        "Stage A caps at the preferred speed: {stage_a:?}"
    );
    let capped = orca_velocity_into(&mut scratch, &me, preferred, &[crosser], &no_bias(lp(0)));
    assert!(
        speed2(capped) <= speed2(preferred),
        "default cap = preferred speed: {capped:?}"
    );
    let fast = orca_velocity_into(&mut scratch, &me, preferred, &[crosser], &no_bias(lp(3_000)));
    let outcome = scratch.last_lp().expect("lp outcome");
    println!("stage A {stage_a:?}, LP capped {capped:?}, LP max 3 m/s {fast:?}");
    assert!(outcome.feasible);
    assert!(
        speed2(fast) > speed2(preferred),
        "the LP speeds up to dodge: {fast:?}"
    );
    assert!(fast[0] > preferred[0], "forward, ahead of the crosser: {fast:?}");

    // Both bodies steer with the LP for 3 s at 50 Hz: no overlap.
    let mut a = me;
    let mut b = crosser;
    let (mut ua, mut ub) = ([a.x_mm * 1_000, a.z_mm * 1_000], [b.x_mm * 1_000, b.z_mm * 1_000]);
    let mut sb = OrcaScratch::new();
    let mut min_gap = i64::MAX;
    for _ in 0..150 {
        let va = orca_velocity_into(&mut scratch, &a, preferred, &[b], &no_bias(lp(3_000)));
        let vb = orca_velocity_into(&mut sb, &b, [0, -3_000], &[a], &no_bias(lp(3_000)));
        step(&mut a, &mut ua, va);
        step(&mut b, &mut ub, vb);
        let d = isqrt(speed2([b.x_mm - a.x_mm, b.z_mm - a.z_mm])) as i64;
        min_gap = min_gap.min(d - a.r_mm - b.r_mm);
        if std::env::var("ORCA_LP_DEBUG").is_ok() {
            println!(
                "gap {} a {:?} b {:?} va {va:?} vb {vb:?} fa {:?} fb {:?}",
                d - 1_000,
                [a.x_mm, a.z_mm],
                [b.x_mm, b.z_mm],
                scratch.last_lp(),
                sb.last_lp()
            );
        }
    }
    println!("closest approach gap: {min_gap} mm");
    assert!(min_gap >= 0, "the pair never overlaps: {min_gap}");
}

/// Exact segment-crossing test: does the move p0 -> p1 touch the wall a -> b?
fn crosses(p0: [i64; 2], p1: [i64; 2], w: &OrcaSegment) -> bool {
    fn orient(a: [i64; 2], b: [i64; 2], c: [i64; 2]) -> i128 {
        let v = i128::from(b[0] - a[0]) * i128::from(c[1] - a[1])
            - i128::from(b[1] - a[1]) * i128::from(c[0] - a[0]);
        v.signum()
    }
    fn on(a: [i64; 2], b: [i64; 2], c: [i64; 2]) -> bool {
        a[0].min(b[0]) <= c[0] && c[0] <= a[0].max(b[0]) && a[1].min(b[1]) <= c[1] && c[1] <= a[1].max(b[1])
    }
    let (a, b) = (w.a_mm, w.b_mm);
    let (o1, o2, o3, o4) = (
        orient(p0, p1, a),
        orient(p0, p1, b),
        orient(a, b, p0),
        orient(a, b, p1),
    );
    if o1 != o2 && o3 != o4 && o1 != 0 && o2 != 0 && o3 != 0 && o4 != 0 {
        return true;
    }
    (o1 == 0 && on(p0, p1, a))
        || (o2 == 0 && on(p0, p1, b))
        || (o3 == 0 && on(a, b, p0))
        || (o4 == 0 && on(a, b, p1))
}

/// Distance (mm, floor) from a point to a segment, exact integer projection.
fn dist_to_segment(p: [i64; 2], w: &OrcaSegment) -> i64 {
    let (a, b) = (w.a_mm, w.b_mm);
    let e = [i128::from(b[0] - a[0]), i128::from(b[1] - a[1])];
    let r = [i128::from(p[0] - a[0]), i128::from(p[1] - a[1])];
    let den = e[0] * e[0] + e[1] * e[1];
    let num = (r[0] * e[0] + r[1] * e[1]).clamp(0, den);
    let c = if den == 0 {
        [0, 0]
    } else {
        [e[0] * num / den, e[1] * num / den]
    };
    isqrt((r[0] - c[0]) * (r[0] - c[0]) + (r[1] - c[1]) * (r[1] - c[1])) as i64
}

struct WallRun {
    crossings: u32,
    wall_contacts: u64,
    body_contacts: u64,
    /// Deepest body-body overlap seen, in mm (0 when none).
    max_body_overlap: i64,
    min_wall_clearance: i64,
    arrived: u32,
    /// Bodies whose X passed `midline_mm` in their travel direction.
    passed: u32,
    fallbacks: u64,
}

/// 64 bodies, 600 ticks at 50 Hz, integer kinematics; `goal` gives each body's
/// target from its position and index.
fn run_walls(
    solver: OrcaSettings,
    walls: &[OrcaSegment],
    start: &[([i64; 2], [i64; 2])],
    midline_mm: i64,
    waypoint: impl Fn([i64; 2], [i64; 2]) -> [i64; 2],
) -> WallRun {
    let speed = 1_600_i64;
    let mut bodies: Vec<OrcaBody> = start
        .iter()
        .enumerate()
        .map(|(k, (p, _))| body(k as u32 + 1, p[0], p[1], 0, 0, 300))
        .collect();
    let goals: Vec<[i64; 2]> = start.iter().map(|(_, g)| *g).collect();
    let mut scratch = OrcaScratch::new();
    let mut out = WallRun {
        crossings: 0,
        wall_contacts: 0,
        body_contacts: 0,
        max_body_overlap: 0,
        min_wall_clearance: i64::MAX,
        arrived: 0,
        passed: 0,
        fallbacks: 0,
    };
    let mut velocities = vec![[0_i64; 2]; bodies.len()];
    let mut um: Vec<[i64; 2]> = bodies.iter().map(|b| [b.x_mm * 1_000, b.z_mm * 1_000]).collect();
    for _tick in 0..600 {
        let snapshot = bodies.clone();
        for (k, me) in snapshot.iter().enumerate() {
            let p = [me.x_mm, me.z_mm];
            let target = waypoint(p, goals[k]);
            let to = [target[0] - p[0], target[1] - p[1]];
            let len = isqrt(speed2(to)) as i64;
            let preferred = if len <= 50 {
                [0, 0]
            } else {
                let s = speed.min(len * 2);
                [to[0] * s / len, to[1] * s / len]
            };
            // Neighbors within 6 m from the same-tick snapshot.
            let near: Vec<OrcaBody> = snapshot
                .iter()
                .filter(|o| o.id != me.id && speed2([o.x_mm - me.x_mm, o.z_mm - me.z_mm]) <= 36_000_000)
                .copied()
                .collect();
            let near_walls: Vec<OrcaSegment> = walls
                .iter()
                .filter(|w| dist_to_segment(p, w) <= 4_000)
                .copied()
                .collect();
            velocities[k] =
                orca_velocity_obstacles_into(&mut scratch, me, preferred, &near, &near_walls, &solver);
            if scratch.last_lp().is_some_and(|o| !o.feasible) {
                out.fallbacks += 1;
            }
        }
        for ((b, v), u) in bodies.iter_mut().zip(&velocities).zip(&mut um) {
            let p0 = [b.x_mm, b.z_mm];
            step(b, u, *v);
            let p1 = [b.x_mm, b.z_mm];
            for w in walls {
                if crosses(p0, p1, w) {
                    out.crossings += 1;
                }
                let d = dist_to_segment(p1, w);
                out.min_wall_clearance = out.min_wall_clearance.min(d - b.r_mm);
                if d < b.r_mm {
                    out.wall_contacts += 1;
                }
            }
        }
        for i in 0..bodies.len() {
            for j in i + 1..bodies.len() {
                let d2 = speed2([bodies[j].x_mm - bodies[i].x_mm, bodies[j].z_mm - bodies[i].z_mm]);
                let rr = i128::from(bodies[i].r_mm + bodies[j].r_mm);
                if d2 < rr * rr {
                    out.body_contacts += 1;
                    out.max_body_overlap = out.max_body_overlap.max((rr - isqrt(d2)) as i64);
                }
            }
        }
    }
    out.arrived = bodies
        .iter()
        .zip(&goals)
        .filter(|(b, g)| speed2([g[0] - b.x_mm, g[1] - b.z_mm]) <= 1_500 * 1_500)
        .count() as u32;
    out.passed = bodies
        .iter()
        .zip(start)
        .filter(|(b, (s, g))| {
            (s[0] < midline_mm) == (b.x_mm > midline_mm) && (g[0] > midline_mm) == (s[0] < midline_mm)
        })
        .count() as u32;
    out
}

/// Walls and `(start, goal)` per body.
type WallScene = (Vec<OrcaSegment>, Vec<([i64; 2], [i64; 2])>);

fn corridor() -> WallScene {
    // A 6 m wide corridor; 32 bodies walk each way (counterflow), 6 rows of 0.6 m bodies.
    let walls = vec![
        seg(-20_000, 3_000, 20_000, 3_000),
        seg(-20_000, -3_000, 20_000, -3_000),
    ];
    let mut start = Vec::new();
    for k in 0..32_i64 {
        let (col, row) = (k / 6, k % 6);
        let z = -2_250 + row * 900;
        start.push(([-5_000 - col * 900, z], [6_000, z]));
        start.push(([5_000 + col * 900, z], [-6_000, z]));
    }
    (walls, start)
}

fn choke() -> WallScene {
    // A 20 m room, closed top and bottom, split at x = 0 by a wall with a 2.4 m gap.
    let walls = vec![
        seg(-15_000, 10_000, 15_000, 10_000),
        seg(-15_000, -10_000, 15_000, -10_000),
        seg(0, 1_200, 0, 10_000),
        seg(0, -1_200, 0, -10_000),
    ];
    let start = (0..64_i64)
        .map(|k| {
            let (col, row) = (k / 8, k % 8);
            (
                [-1_500 - col * 1_200, -7_000 + row * 2_000],
                [8_000, -3_500 + row * 1_000],
            )
        })
        .collect();
    (walls, start)
}

/// Through the gap first, then to the goal.
fn via_gap(p: [i64; 2], goal: [i64; 2]) -> [i64; 2] {
    if p[0] < 0 {
        [600, p[1].clamp(-700, 700)]
    } else {
        goal
    }
}

/// A corridor (counterflow) and a choke, 64 bodies, 600 ticks: no body center ever
/// crosses a wall segment with the LP solver; contacts are counted as evidence. The
/// same runs under Stage A (walls unknown to it) do cross, so the check can fail.
#[test]
fn agents_do_not_cross_a_wall_segment() {
    let settings = lp(1_800);
    let (walls, start) = corridor();
    let run = run_walls(settings, &walls, &start, 0, |_, g| g);
    println!(
        "corridor LP: crossings {} wall contacts {} body contacts {} (deepest {} mm) min wall clearance {} mm passed {}/64 arrived {}/64 fallbacks {}",
        run.crossings,
        run.wall_contacts,
        run.body_contacts,
        run.max_body_overlap,
        run.min_wall_clearance,
        run.passed,
        run.arrived,
        run.fallbacks
    );
    assert_eq!(run.crossings, 0, "no center crosses a corridor wall");
    assert!(
        run.passed >= 48,
        "the counterflow gets past each other ({}/64)",
        run.passed
    );

    let (walls, start) = choke();
    let run = run_walls(settings, &walls, &start, 0, via_gap);
    println!(
        "choke LP: crossings {} wall contacts {} body contacts {} (deepest {} mm) min wall clearance {} mm through the gap {}/64 arrived {}/64 fallbacks {}",
        run.crossings,
        run.wall_contacts,
        run.body_contacts,
        run.max_body_overlap,
        run.min_wall_clearance,
        run.passed,
        run.arrived,
        run.fallbacks
    );
    assert_eq!(run.crossings, 0, "no center crosses the choke walls");
    assert!(run.passed >= 16, "the choke drains ({}/64 through)", run.passed);

    // Control: Stage A has no walls, so the same choke run sends centers through them.
    let control = run_walls(OrcaSettings::default(), &walls, &start, 0, |_, g| g);
    println!(
        "choke Stage A control: crossings {} body contacts {} (deepest {} mm) through the gap {}/64",
        control.crossings, control.body_contacts, control.max_body_overlap, control.passed
    );
    assert!(
        control.crossings > 0,
        "the crossing check detects a wall-blind solver"
    );
}

/// The LP corpus pin: 2048 seeded scenes with neighbors and walls; velocities,
/// feasibility and the worst violation hashed. Recorded on x86-64; aarch64 must match.
#[test]
fn lp_bits_match_the_pinned_hash() {
    let mut rng = Lcg(0x9e37_79b9);
    let mut scratch = OrcaScratch::new();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut reversed_hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut reversed_scratch = OrcaScratch::new();
    for _ in 0..2048 {
        let (me, preferred, others, segments, max_speed) = random_scene(&mut rng);
        let settings = lp(max_speed);
        let v = orca_velocity_obstacles_into(&mut scratch, &me, preferred, &others, &segments, &settings);
        let outcome = scratch.last_lp().expect("lp outcome");
        for x in [
            v[0],
            v[1],
            i64::from(outcome.feasible),
            outcome.worst_violation_mm_s,
            i64::from(outcome.planes),
        ] {
            fnv(&mut hash, x);
        }
        // Neighbor input order and segment endpoint order do not change the answer.
        let mut rev = others.clone();
        rev.reverse();
        let flipped: Vec<OrcaSegment> = segments
            .iter()
            .map(|s| seg(s.b_mm[0], s.b_mm[1], s.a_mm[0], s.a_mm[1]))
            .collect();
        let w =
            orca_velocity_obstacles_into(&mut reversed_scratch, &me, preferred, &rev, &flipped, &settings);
        for x in [w[0], w[1]] {
            fnv(&mut reversed_hash, x);
        }
        assert_eq!(v, w, "input order changed the LP answer");
    }
    println!(
        "LP corpus: {hash:#018x} (arch={} os={})",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    assert_eq!(hash, LP_CORPUS_PIN, "LP corpus bits moved");
    let _ = reversed_hash;
}

/// Recorded on x86-64 Linux (the test machine) at the lane head; the Mac aarch64 run must equal it.
const LP_CORPUS_PIN: u64 = 0x6ad9_4cef_d7ba_8a30;

// ---------------------------------------------------------------------------
// Further checks.

/// The LP path allocates nothing once the scratch has grown.
#[test]
fn lp_scratch_form_allocates_nothing() {
    let mut rng = Lcg(0x7777_1234);
    let scenes: Vec<_> = (0..256).map(|_| random_scene(&mut rng)).collect();
    let mut scratch = OrcaScratch::with_capacity(16, 32);
    for (me, preferred, others, segments, max_speed) in &scenes {
        orca_velocity_obstacles_into(&mut scratch, me, *preferred, others, segments, &lp(*max_speed));
    }
    let (sum, allocs) = count_allocs(|| {
        let mut sum = 0_i64;
        for (me, preferred, others, segments, max_speed) in &scenes {
            let v =
                orca_velocity_obstacles_into(&mut scratch, me, *preferred, others, segments, &lp(*max_speed));
            sum = sum.wrapping_add(v[0]).wrapping_add(v[1]);
        }
        sum
    });
    println!("256 warm LP calls: {allocs} allocations (checksum {sum})");
    assert_eq!(allocs, 0);
}

/// Lp-only settings and segments are checked against the domain with named codes;
/// the plain form returns the documented fallback.
#[test]
fn lp_domain_errors_are_named() {
    let me = body(1, 0, 0, 0, 0, 500);
    let pref = [1_000, 0];
    let mut scratch = OrcaScratch::new();
    let cases: Vec<(OrcaSettings, Vec<OrcaSegment>, OrcaDomainError, &str)> = vec![
        (
            OrcaSettings {
                horizon_ms: 0,
                ..lp(0)
            },
            vec![],
            OrcaDomainError::Horizon,
            "orca_horizon",
        ),
        (lp(-1), vec![], OrcaDomainError::MaxSpeed, "orca_max_speed"),
        (
            lp(ORCA_MAX_SPEED_MM_S + 1),
            vec![],
            OrcaDomainError::MaxSpeed,
            "orca_max_speed",
        ),
        (
            OrcaSettings {
                obstacle_horizon_ms: 0,
                ..lp(0)
            },
            vec![],
            OrcaDomainError::ObstacleHorizon,
            "orca_obstacle_horizon",
        ),
        (
            OrcaSettings {
                contact_ms: 0,
                ..lp(0)
            },
            vec![],
            OrcaDomainError::Contact,
            "orca_contact",
        ),
        (
            lp(0),
            vec![seg(0, 0, (1 << 31) + 1, 0)],
            OrcaDomainError::Segment,
            "orca_segment",
        ),
        (
            lp(0),
            vec![seg(0, 1_000, 1, 1_000); ORCA_MAX_SEGMENTS + 1],
            OrcaDomainError::SegmentCap,
            "orca_segment_cap",
        ),
    ];
    for (settings, segments, want, code) in cases {
        let got = try_orca_velocity_obstacles_into(&mut scratch, &me, pref, &[], &segments, &settings);
        assert_eq!(got, Err(want), "{code}");
        assert_eq!(want.code(), code);
        assert_eq!(
            orca_velocity_obstacles_into(&mut scratch, &me, pref, &[], &segments, &settings),
            pref
        );
    }
    // Stage A does not read the Lp fields, so they are not checked for it.
    let stage_a = OrcaSettings {
        max_speed_mm_s: -1,
        contact_ms: 0,
        ..OrcaSettings::default()
    };
    assert_eq!(
        orca_velocity_with_settings(&me, pref, &[], &stage_a),
        orca_velocity(&me, pref, &[])
    );
    assert_eq!(OrcaSolver::Lp.name(), "lp");
    assert_eq!(OrcaSolver::StageA.name(), "stage_a");
    let fields = scratch.lp_stats().fields();
    assert!(fields.iter().all(|(name, _)| name.starts_with("orca_lp_")));
}

/// A clear path keeps the preference; a parked body and a wall are steered round; a
/// body touching a wall moves away from it.
#[test]
fn lp_basic_behavior() {
    let mut scratch = OrcaScratch::new();
    let me = body(1, 0, 0, 1_000, 0, 500);
    assert_eq!(
        orca_velocity_into(&mut scratch, &me, [1_000, 0], &[], &lp(0)),
        [1_000, 0]
    );
    let rock = body(2, 3_000, 100, 0, 0, 500);
    let v = orca_velocity_into(&mut scratch, &me, [1_000, 0], &[rock], &lp(0));
    assert!(v[1] < 0, "steers round the parked body on the free side: {v:?}");
    // Heading straight at a wall 1 m ahead at 2 m/s: the wall plane slows the approach.
    let fast = body(1, 0, 0, 2_000, 0, 500);
    let wall = [seg(1_500, -3_000, 1_500, 3_000)];
    let v = orca_velocity_obstacles_into(&mut scratch, &fast, [2_000, 0], &[], &wall, &lp(0));
    assert!(v[0] < 2_000 && v[0] >= 0, "slows for the wall: {v:?}");
    // Touching a wall (center 300 mm from it, radius 500): moves away.
    let touching = body(1, 1_200, 0, 0, 0, 500);
    let v = orca_velocity_obstacles_into(&mut scratch, &touching, [0, 0], &[], &wall, &lp(2_000));
    assert!(v[0] < 0, "pushed off the wall: {v:?}");
    // Serde round trip keeps the solver choice; missing fields take the defaults.
    let json = serde_json::to_string(&lp(1_234)).expect("serialize");
    assert!(json.contains("\"solver\":\"lp\""), "{json}");
    let back: OrcaSettings = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, lp(1_234));
    let old: OrcaSettings = serde_json::from_str(r#"{"max_neighbors":4}"#).expect("old settings");
    assert_eq!(
        old,
        OrcaSettings {
            max_neighbors: 4,
            ..OrcaSettings::default()
        }
    );
    assert_eq!(old.solver, OrcaSolver::StageA);
}

/// Timing evidence only (never asserted): 900 bodies, 8 neighbors and up to 8 wall
/// segments each, Stage A vs the LP per tick.
#[test]
#[ignore = "timing evidence; run with --release --ignored --nocapture"]
#[expect(
    clippy::disallowed_methods,
    reason = "timing evidence: Instant::now around the solver calls, never asserted (ADR-8367)"
)]
fn crowd_900_lp_timing_evidence() {
    use std::time::Instant;
    let side = 30_i64;
    let bodies: Vec<OrcaBody> = (0..900_i64)
        .map(|k| {
            let (x, z) = (k % side, k / side);
            body(
                k as u32 + 1,
                x * 1_500 + (k * 37) % 400,
                z * 1_500 + (k * 91) % 400,
                ((k * 13) % 2_000) - 1_000,
                ((k * 29) % 2_000) - 1_000,
                400,
            )
        })
        .collect();
    // Eight wall segments around each body's cell (a grid of walls every 6 m).
    let walls_for = |b: &OrcaBody| -> Vec<OrcaSegment> {
        let cx = b.x_mm.div_euclid(6_000) * 6_000;
        let cz = b.z_mm.div_euclid(6_000) * 6_000;
        (0..8_i64)
            .map(|k| match k % 4 {
                0 => seg(cx - 6_000 * (k / 4), cz, cx + 3_000, cz),
                1 => seg(cx, cz - 6_000 * (k / 4), cx, cz + 3_000),
                2 => seg(
                    cx + 6_000,
                    cz + 1_000 + 2_000 * (k / 4),
                    cx + 6_000,
                    cz + 2_000 + 2_000 * (k / 4),
                ),
                _ => seg(
                    cx + 1_000 + 2_000 * (k / 4),
                    cz + 6_000,
                    cx + 2_000 + 2_000 * (k / 4),
                    cz + 6_000,
                ),
            })
            .collect()
    };
    let lists: Vec<(Vec<OrcaBody>, Vec<OrcaSegment>)> = bodies
        .iter()
        .map(|me| {
            let mut near: Vec<(i128, OrcaBody)> = bodies
                .iter()
                .filter(|o| o.id != me.id)
                .map(|o| (speed2([o.x_mm - me.x_mm, o.z_mm - me.z_mm]), *o))
                .collect();
            near.sort_by_key(|(d, o)| (*d, o.id));
            (near.into_iter().take(8).map(|(_, o)| o).collect(), walls_for(me))
        })
        .collect();
    let mut scratch = OrcaScratch::with_capacity(16, 32);
    let mut report = |name: &str, settings: OrcaSettings, with_walls: bool| {
        let mut samples = Vec::new();
        let mut sum = 0_i64;
        for _ in 0..200 {
            let t = Instant::now();
            for (me, (near, walls)) in bodies.iter().zip(&lists) {
                let w: &[OrcaSegment] = if with_walls { walls } else { &[] };
                let v = orca_velocity_obstacles_into(&mut scratch, me, [1_000, 0], near, w, &settings);
                sum = sum.wrapping_add(v[0] ^ v[1]);
            }
            samples.push(t.elapsed().as_secs_f64() * 1e3);
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "{name}: p50 {:.3} ms p99 {:.3} ms per tick (900 bodies; checksum {sum})",
            samples[100], samples[198]
        );
    };
    report("Stage A, 8 neighbors", OrcaSettings::default(), false);
    report("LP, 8 neighbors", lp(1_500), false);
    report("LP, 8 neighbors + 8 walls", lp(1_500), true);
    let stats = scratch.lp_stats();
    println!("lp stats: {:?}", stats.fields());
}

/// Domain corners the 2048-scene corpus does not draw: extreme speeds, the full
/// neighbor and segment caps, a zero-length segment, duplicate and nearly parallel
/// walls. Debug overflow checks are the witness. A feasible result must satisfy
/// every unshifted plane; every result stays inside the speed circle.
#[test]
fn domain_corners_stay_inside_the_overflow_bound() {
    let p = ORCA_MAX_POSITION_MM;
    let v = ORCA_MAX_SPEED_MM_S;
    let r = ORCA_MAX_RADIUS_MM;
    let settings = OrcaSettings {
        solver: OrcaSolver::Lp,
        max_neighbors: ORCA_MAX_NEIGHBOR_PLANES,
        horizon_ms: ORCA_MAX_HORIZON_MS,
        max_speed_mm_s: v,
        obstacle_horizon_ms: ORCA_MAX_HORIZON_MS,
        contact_ms: 1,
        keep_right_mm_s: v,
        min_preferred_mm_s: 0,
        head_on_range_mm: p,
        ..OrcaSettings::default()
    };
    let me = body(1, 0, 0, v, -v, r);
    // Magnitudes, not the plane cap, are what can overflow. A handful of bodies at
    // the position and speed limits, plus the contact disc.
    let mut others: Vec<OrcaBody> = (0..8)
        .map(|i| {
            let sign = if i % 2 == 0 { 1 } else { -1 };
            body(2 + i, sign * p / 2, -sign * p / 4, -v, v, r)
        })
        .collect();
    others[0] = body(2, r / 2, 0, -v, 0, r);
    let mut segments = vec![
        seg(0, 0, 0, 0),
        seg(p, -p, -p, p),
        seg(-p, p, p, -p),
        seg(2_000, 1_000, 20_000, 1_000),
        seg(2_000, 1_000, 20_000, 1_001),
    ];
    // The segment cap, parked on the domain corner so each one is a point obstacle
    // at the position limit. The range rule drops those the speed circle cannot reach.
    while segments.len() < ORCA_MAX_SEGMENTS {
        let k = segments.len() as i64;
        segments.push(seg(p - k, p, p - k, p));
    }
    let mut scratch = OrcaScratch::with_capacity(ORCA_MAX_NEIGHBOR_PLANES, ORCA_MAX_SEGMENTS);
    let preferred = [v, v];
    let got = orca_velocity_obstacles_into(&mut scratch, &me, preferred, &others, &segments, &settings);
    // The circle is `max(max_speed, ceil|preferred|)`, and this preference is `[v, v]`.
    let cap = i128::from(v).max(isqrt_ceil(i128::from(v) * i128::from(v) * 2));
    assert!(
        speed2(got) <= cap * cap,
        "left the speed circle: {got:?} cap {cap}"
    );
    let outcome = scratch.last_lp().expect("lp ran");
    if outcome.feasible {
        assert!(
            scratch.lp_planes().iter().all(|plane| admits(plane, got)),
            "feasible result violates a plane"
        );
    }
    let mut rev_neighbors = others.clone();
    rev_neighbors.reverse();
    let neighbor_order =
        orca_velocity_obstacles_into(&mut scratch, &me, preferred, &rev_neighbors, &segments, &settings);
    assert_eq!(got, neighbor_order, "neighbor insertion order changed the answer");
    let mut endpoints_only = segments.clone();
    for s in &mut endpoints_only {
        std::mem::swap(&mut s.a_mm, &mut s.b_mm);
    }
    let endpoints =
        orca_velocity_obstacles_into(&mut scratch, &me, preferred, &others, &endpoints_only, &settings);
    assert_eq!(got, endpoints, "endpoint order changed the answer");
    let _ = outcome;
    let max_point = scratch
        .lp_planes()
        .iter()
        .flat_map(|plane| plane.point_q)
        .map(i64::unsigned_abs)
        .max()
        .unwrap_or(0);
    println!(
        "corner: v={got:?} feasible={} planes={} max |point_q|={}",
        scratch.last_lp().is_some_and(|o| o.feasible),
        scratch.lp_planes().len(),
        max_point
    );
    // ADR-8555 bound 6: plane points stay inside 2^39 Q, so the i64 cast cannot wrap.
    assert!(
        max_point <= 1_u64 << 39,
        "plane point exceeded the ADR-8555 bound: {max_point}"
    );
}

/// A zero-length segment is a point obstacle. Two calls, and the same segment
/// listed twice, return one velocity and do not panic.
#[test]
fn zero_length_and_duplicate_segments_are_deterministic() {
    let settings = lp(1_500);
    let me = body(1, 0, 0, 0, 0, 400);
    let point = seg(800, 0, 800, 0);
    let mut scratch = OrcaScratch::new();
    let once = orca_velocity_obstacles_into(&mut scratch, &me, [1_000, 0], &[], &[point], &settings);
    let twice = orca_velocity_obstacles_into(&mut scratch, &me, [1_000, 0], &[], &[point, point], &settings);
    let again = orca_velocity_obstacles_into(&mut scratch, &me, [1_000, 0], &[], &[point], &settings);
    assert_eq!(once, again);
    assert!(speed2(twice) <= 1_500 * 1_500);
    let on_top = seg(0, 0, 0, 0);
    let pushed = orca_velocity_obstacles_into(&mut scratch, &me, [0, 0], &[], &[on_top], &settings);
    assert_eq!(
        pushed,
        orca_velocity_obstacles_into(&mut scratch, &me, [0, 0], &[], &[on_top], &settings)
    );
    println!("zero-length: ahead {once:?} duplicate {twice:?} on top {pushed:?}");
}

/// Walls that still intersect the speed circle stay hard when only the neighbors
/// make the program infeasible: the result does not enter the wall half-plane.
#[test]
fn infeasible_neighbors_do_not_push_through_a_hard_wall() {
    let settings = OrcaSettings {
        solver: OrcaSolver::Lp,
        max_neighbors: 16,
        max_speed_mm_s: 1_500,
        ..OrcaSettings::default()
    };
    let me = body(1, 0, 0, 1_000, 0, 400);
    // 1.6 m ahead: inside the default 1 s obstacle horizon at 1.5 m/s, with room to stop.
    let wall = seg(1_600, -4_000, 1_600, 4_000);
    let others: Vec<OrcaBody> = (0..12)
        .map(|i| {
            let z = (i as i64 - 6) * 300;
            body(10 + i, -200, z, 0, 0, 400)
        })
        .collect();
    let mut scratch = OrcaScratch::new();
    let v = orca_velocity_obstacles_into(&mut scratch, &me, [1_500, 0], &others, &[wall], &settings);
    let outcome = scratch.last_lp().expect("lp");
    let wall_planes: Vec<_> = scratch
        .lp_planes()
        .iter()
        .filter(|p| p.obstacle)
        .cloned()
        .collect();
    assert!(!wall_planes.is_empty(), "the wall was skipped");
    assert!(
        wall_planes.iter().all(|p| admits(p, v)),
        "hard wall violated: v={v:?} feasible={} planes={wall_planes:?}",
        outcome.feasible
    );
    println!(
        "hard wall: v={v:?} feasible={} worst={} mm/s",
        outcome.feasible, outcome.worst_violation_mm_s
    );
}

/// A corridor with 1 mm of clearance on each side. The 2 mm/s inward margin makes
/// the two wall planes conflict, so they drop to soft. With no neighbor the
/// unshifted planes still have to admit the result. A neighbor overlapping from
/// the left is recorded, not required to stay out: the ADR drops walls to soft
/// when the walls themselves conflict.
#[test]
fn one_millimeter_corridor_fallback_stays_between_the_unshifted_walls() {
    let settings = lp(1_500);
    let me = body(1, 0, 0, 0, 1_000, 400);
    let walls = [seg(401, -5_000, 401, 5_000), seg(-401, -5_000, -401, 5_000)];
    let mut scratch = OrcaScratch::new();
    let alone = orca_velocity_obstacles_into(&mut scratch, &me, [0, 1_000], &[], &walls, &settings);
    let alone_outcome = scratch.last_lp().expect("lp");
    let alone_planes: Vec<_> = scratch
        .lp_planes()
        .iter()
        .filter(|p| p.obstacle)
        .cloned()
        .collect();
    assert_eq!(alone_planes.len(), 2, "a wall was skipped: {alone_planes:?}");
    assert!(
        alone_planes.iter().all(|p| admits(p, alone)),
        "corridor fallback entered a wall: v={alone:?} feasible={} planes={alone_planes:?}",
        alone_outcome.feasible
    );
    let neighbor = [body(2, -700, 0, 500, 0, 400)];
    let pushed = orca_velocity_obstacles_into(&mut scratch, &me, [0, 1_000], &neighbor, &walls, &settings);
    let pushed_outcome = scratch.last_lp().expect("lp");
    let pushed_planes: Vec<_> = scratch
        .lp_planes()
        .iter()
        .filter(|p| p.obstacle)
        .cloned()
        .collect();
    let worst = pushed_planes
        .iter()
        .map(|p| -signed(p, pushed).min(0))
        .max()
        .unwrap_or(0);
    println!(
        "corridor alone v={alone:?} feasible={} worst_mm_s={} | with neighbor v={pushed:?} feasible={} unshifted_violation_q={}",
        alone_outcome.feasible, alone_outcome.worst_violation_mm_s, pushed_outcome.feasible, worst
    );
}

/// The obstacle planes of the last solve.
fn wall_planes(scratch: &OrcaScratch) -> Vec<OrcaHalfPlane> {
    scratch
        .lp_planes()
        .iter()
        .filter(|p| p.obstacle)
        .cloned()
        .collect()
}

/// Largest penetration of `v` into `planes` in mm/s, rounded up (0 when every plane admits it).
fn worst_mm_s(planes: &[OrcaHalfPlane], v: [i64; 2]) -> i128 {
    planes
        .iter()
        .map(|p| {
            let len = isqrt_ceil(
                i128::from(p.direction[0]) * i128::from(p.direction[0])
                    + i128::from(p.direction[1]) * i128::from(p.direction[1]),
            );
            let pen = (-signed(p, v)).max(0);
            let den = len * i128::from(ORCA_LP_VELOCITY_SCALE);
            (pen + den - 1) / den
        })
        .max()
        .unwrap_or(0)
}

/// Review finding 2 (ADR-8703): a body pressed into two parallel walls at once (both
/// contact planes push it inward, so the walls alone conflict and drop to soft) keeps
/// moving along its preference. Every point on the bisector between the walls has the
/// same penetration, and the tie goes to the point nearest the preference, not to an end
/// of the speed circle. Both corridor axes and both preference signs.
#[test]
fn squeezed_between_parallel_walls_keeps_the_preference() {
    let settings = lp(1_500);
    let mut scratch = OrcaScratch::new();
    // Walls 395 mm either side of a radius-400 body: 5 mm of overlap on each.
    let along_z = [seg(395, -5_000, 395, 5_000), seg(-395, -5_000, -395, 5_000)];
    let along_x = [seg(-5_000, 395, 5_000, 395), seg(-5_000, -395, 5_000, -395)];
    for (walls, preferred, want) in [
        (&along_z, [0, 1_000], [0, 1_000]),
        (&along_z, [0, -1_000], [0, -1_000]),
        (&along_x, [1_000, 0], [1_000, 0]),
        (&along_x, [-1_000, 0], [-1_000, 0]),
        // A sideways preference: the tie point is the foot of the preference on the
        // bisector, so the body stops across the corridor instead of running along it.
        (&along_z, [700, 0], [0, 0]),
    ] {
        let me = body(1, 0, 0, preferred[0], preferred[1], 400);
        let got = orca_velocity_obstacles_into(&mut scratch, &me, preferred, &[], walls, &settings);
        let outcome = scratch.last_lp().expect("lp");
        assert!(!outcome.feasible, "both contact planes cannot hold: {got:?}");
        assert_eq!(got, want, "pref {preferred:?}: the fallback left the preference");
        // Equal, minimal penetration of both walls: 5 mm over contact_ms 100 = 50 mm/s.
        assert!(worst_mm_s(&wall_planes(&scratch), got) <= 50, "{got:?}");
        let again = orca_velocity_obstacles_into(&mut scratch, &me, preferred, &[], walls, &settings);
        assert_eq!(got, again);
    }
}

/// Review finding 2, the 1 mm corridor (ADR-8703): with no neighbor the fallback keeps the
/// forward preference instead of reversing at the speed cap, and with one overlapping
/// neighbor the walls stay hard (they only conflict through the 2 mm/s margin, so the
/// unshifted walls are kept hard and the neighbor is the soft plane). The walls are then
/// held to within the 1 mm/s output rounding.
#[test]
fn one_millimeter_corridor_keeps_the_preference_and_the_walls() {
    let settings = lp(1_500);
    let me = body(1, 0, 0, 0, 1_000, 400);
    let walls = [seg(401, -5_000, 401, 5_000), seg(-401, -5_000, -401, 5_000)];
    let mut scratch = OrcaScratch::new();
    let alone = orca_velocity_obstacles_into(&mut scratch, &me, [0, 1_000], &[], &walls, &settings);
    assert_eq!(alone, [0, 1_000], "the corridor fallback reversed the preference");
    assert_eq!(worst_mm_s(&wall_planes(&scratch), alone), 0, "{alone:?}");
    let neighbor = [body(2, -700, 0, 500, 0, 400)];
    let pushed = orca_velocity_obstacles_into(&mut scratch, &me, [0, 1_000], &neighbor, &walls, &settings);
    let outcome = scratch.last_lp().expect("lp");
    assert!(!outcome.feasible);
    assert!(
        worst_mm_s(&wall_planes(&scratch), pushed) <= 1,
        "the neighbor pushed the body through a wall: {pushed:?}"
    );
    assert!(pushed[1] > 0, "the body reversed: {pushed:?}");
    println!("1 mm corridor: alone {alone:?} with neighbor {pushed:?}");
}

/// Review finding 3 (ADR-8703): a zero-length segment exactly on the body center has no
/// outward direction, so it gives no plane; the result is what the other planes give,
/// here the preference itself. Off center it is still a point obstacle.
#[test]
fn zero_length_segment_on_the_center_gives_no_plane() {
    let settings = lp(1_500);
    let mut scratch = OrcaScratch::new();
    for (pos, preferred) in [
        ([0, 0], [0, 0]),
        ([0, 0], [600, -300]),
        ([12_345, -678], [-900, 0]),
    ] {
        let me = body(1, pos[0], pos[1], 0, 0, 400);
        let on_top = seg(pos[0], pos[1], pos[0], pos[1]);
        let got = orca_velocity_obstacles_into(&mut scratch, &me, preferred, &[], &[on_top], &settings);
        assert_eq!(got, preferred, "at {pos:?}");
        assert!(wall_planes(&scratch).is_empty());
        let outcome = scratch.last_lp().expect("lp");
        assert!(outcome.feasible);
    }
    // Next to a real wall, the on-center point does not change the wall's answer.
    let me = body(1, 0, 0, 1_000, 0, 400);
    let wall = seg(1_600, -4_000, 1_600, 4_000);
    let plain = orca_velocity_obstacles_into(&mut scratch, &me, [1_500, 0], &[], &[wall], &settings);
    let with_point = orca_velocity_obstacles_into(
        &mut scratch,
        &me,
        [1_500, 0],
        &[],
        &[seg(0, 0, 0, 0), wall],
        &settings,
    );
    assert_eq!(plain, with_point);
    // 100 mm off center it is a touching point obstacle and pushes away from itself.
    let near =
        orca_velocity_obstacles_into(&mut scratch, &me, [0, 0], &[], &[seg(100, 0, 100, 0)], &settings);
    assert!(near[0] < 0, "{near:?}");
}

/// ADR-8703 near tie: a squeeze whose walls are tilted 1 mm over 20 m (sin 5e-5, inside the
/// fallback's 2^-14 parallel tolerance) behaves like the straight one: the body keeps its
/// preference along the corridor, it does not run to an end of the speed circle.
#[test]
fn slightly_tilted_squeeze_keeps_the_preference() {
    let settings = lp(1_500);
    let mut scratch = OrcaScratch::new();
    let walls = [seg(395, -10_000, 396, 10_000), seg(-395, -10_000, -395, 10_000)];
    for preferred in [[0, 1_000], [0, -1_000]] {
        let me = body(1, 0, 0, 0, 0, 400);
        let got = orca_velocity_obstacles_into(&mut scratch, &me, preferred, &[], &walls, &settings);
        assert!(!scratch.last_lp().expect("lp").feasible);
        assert!(
            got[0].abs() <= 3 && (got[1] - preferred[1]).abs() <= 2,
            "pref {preferred:?}: {got:?}"
        );
        assert!(worst_mm_s(&wall_planes(&scratch), got) <= 53, "{got:?}");
    }
}
