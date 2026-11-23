// SPDX-License-Identifier: MPL-2.0
//! ADR-3273: the cloth solver at the spec 55 § 10 budget setting (4 substeps, 1 iteration, grid)
//! stays finite and unstretched, and the runaway guard catches a solver that blows up without
//! touching any other solver. Fixed steps and counters only; no clock, no GPU, no files.

use regia_core::math::{Isometry, Vec3};
use regia_physics::cloth::{
    ClothConfig, ClothFault, ClothFaultKind, ClothGrid, ClothGuard, ClothRecovery, ClothSet, ClothSolver,
    ClothTuning, DEFAULT_GUARD_STRAIN, DEFAULT_STIFF_LIMIT, FAULT_EVENTS_MAX, MAX_ITERATIONS, SnapshotKind,
    SnapshotShape, TearParams, WorldSnapshot,
};

const DT: f32 = 1.0 / 60.0;

/// The spec 55 § 10 fixture shape: an `n × n` sheet at `spacing`, starting horizontal at
/// y = 1.5, row 0 pinned, stretch compliance 0, shear 1e-4, bend 1e-2, 0.05 kg particles.
fn sheet(n: usize, spacing: f32, substeps: u32) -> ClothSolver {
    let mut s = ClothSolver::new(ClothConfig {
        substeps,
        iterations: 1,
        ..ClothConfig::default()
    })
    .expect("config");
    let h = s
        .add_grid(&ClothGrid {
            origin: Vec3::new(0.0, 1.5, 0.0),
            u_axis: Vec3::X,
            v_axis: Vec3::Z,
            columns: n,
            rows: n,
            spacing,
            particle_mass: 0.05,
            stretch_compliance: 0.0,
            shear_compliance: 1e-4,
            bend_compliance: 1e-2,
        })
        .expect("grid");
    for c in 0..n {
        s.pin(h.index(c, 0).expect("row 0")).expect("pin");
    }
    s
}

/// Largest length ratio of the sheet's zero-compliance links (grid lines and the `(c, r)`-
/// `(c + 1, r + 1)` diagonals) and of its shear diagonals, from the particle positions
/// (`ClothGrid` stores row by row, row 0 first).
fn split_strain(s: &ClothSolver, n: usize, spacing: f32) -> (f32, f32) {
    let p = |c: usize, r: usize| s.particles()[r * n + c].position;
    let diagonal = spacing * std::f32::consts::SQRT_2;
    let (mut stiff, mut shear) = (0.0f32, 0.0f32);
    for r in 0..n {
        for c in 0..n {
            if c + 1 < n {
                stiff = stiff.max(p(c, r).distance(p(c + 1, r)) / spacing);
            }
            if r + 1 < n {
                stiff = stiff.max(p(c, r).distance(p(c, r + 1)) / spacing);
            }
            if c + 1 < n && r + 1 < n {
                stiff = stiff.max(p(c, r).distance(p(c + 1, r + 1)) / diagonal);
                shear = shear.max(p(c + 1, r).distance(p(c, r + 1)) / diagonal);
            }
        }
    }
    (stiff, shear)
}

fn all_finite(s: &ClothSolver) -> bool {
    s.particles()
        .iter()
        .all(|p| p.position.is_finite() && p.velocity.is_finite())
}

/// The lane-report repro (cloth-extras, "Finding for the lead"): 32 × 32 at 0.03 m, 4 substeps,
/// 1 iteration. The base solver stretched it to 1.9x by step 20 and filled it with NaN from
/// step 270. It must stay finite and under 1.1x strain for 600 steps without the guard's help.
#[test]
fn spec_budget_sheet_stays_finite_and_unstretched() {
    let mut s = sheet(32, 0.03, 4);
    let mut worst = 0.0f32;
    for step in 0..600 {
        s.step(DT).expect("step");
        assert!(all_finite(&s), "non-finite at step {step}");
        let stretch = s.stats().max_stretch;
        worst = worst.max(stretch);
        assert!(stretch < 1.1, "step {step}: max strain {stretch}");
    }
    let health = s.health();
    eprintln!("spec budget sheet: worst strain {worst}, health {health:?}");
    assert_eq!(
        health.faults, 0,
        "the solver must be stable on its own, not by the guard"
    );
    assert!(health.peak_strain < 1.1);
}

/// The same sheet with the guard off: the fix alone keeps it finite (the guard is a backstop).
#[test]
fn spec_budget_sheet_is_stable_with_the_guard_off() {
    let mut s = sheet(32, 0.03, 4);
    s.set_guard(ClothGuard {
        enabled: false,
        ..ClothGuard::default()
    })
    .expect("guard");
    for step in 0..600 {
        s.step(DT).expect("step");
        assert!(all_finite(&s), "non-finite at step {step}");
    }
    assert!(s.stats().max_stretch < 1.1, "{}", s.stats().max_stretch);
}

/// A solver whose step passes the guard is bit-identical with the guard on and off.
#[test]
fn a_passing_guard_changes_nothing() {
    let mut on = sheet(16, 0.05, 8);
    let mut off = sheet(16, 0.05, 8);
    off.set_guard(ClothGuard {
        enabled: false,
        ..ClothGuard::default()
    })
    .expect("guard");
    for step in 0..300 {
        on.step(DT).expect("step");
        off.step(DT).expect("step");
        assert_eq!(on.state_digest(), off.state_digest(), "step {step}");
    }
    assert_eq!(on.health().faults, 0);
    assert_eq!(off.health().max_strain, 0.0, "the guard off measures nothing");
}

/// Puts particle `index` so far out that the next step's link lengths overflow to infinity and
/// the solver produces NaN (a finite but absurd input, the only way to inject NaN through the
/// validated API).
fn poison(s: &mut ClothSolver, index: usize) {
    s.set_position(index, Vec3::splat(1.0e38))
        .expect("finite position");
}

/// Acceptance: a NaN in one of two solvers leaves the other's digest unchanged, and the faulted
/// one is finite again after the step that faulted (the last good state, velocities zeroed).
#[test]
fn nan_in_one_solver_leaves_the_other_untouched_and_recovers_in_one_step() {
    let mut reference = sheet(12, 0.05, 4);
    let mut set = ClothSet::new();
    set.insert("bad", sheet(12, 0.05, 4));
    set.insert("good", sheet(12, 0.05, 4));
    for parallel in [false, true] {
        set.set_parallel(parallel);
        for _ in 0..20 {
            set.step_all(DT).expect("step");
            reference.step(DT).expect("step");
        }
        assert_eq!(
            set.get("good").expect("good").state_digest(),
            reference.state_digest()
        );
        let before: Vec<Vec3> = set
            .get("bad")
            .expect("bad")
            .particles()
            .iter()
            .map(|p| p.position)
            .collect();
        let faults_before = set.get("bad").expect("bad").health().faults;
        poison(set.get_mut("bad").expect("bad"), 40);
        // The faulted step: stepping the set still succeeds.
        assert_eq!(set.step_all(DT).expect("step"), 2);
        reference.step(DT).expect("step");
        assert_eq!(
            set.get("good").expect("good").state_digest(),
            reference.state_digest(),
            "the other solver's digest moved (parallel {parallel})"
        );
        let bad = set.get_mut("bad").expect("bad");
        assert!(
            all_finite(bad),
            "not recovered within one step (parallel {parallel})"
        );
        let health = bad.health();
        assert_eq!(health.faults, faults_before + 1);
        let fault = health.last_fault.expect("fault");
        assert_eq!(fault.kind, ClothFaultKind::NonFinite);
        assert_eq!(fault.code(), "cloth_fault_non_finite");
        assert_eq!(fault.recovery, ClothRecovery::LastGood);
        assert_eq!(fault.recovery.name(), "last_good");
        let after: Vec<Vec3> = bad.particles().iter().map(|p| p.position).collect();
        assert_eq!(after, before, "the last good state is the one before the poison");
        assert!(bad.particles().iter().all(|p| p.velocity == Vec3::ZERO));
        let mut events: Vec<ClothFault> = Vec::new();
        bad.drain_faults(&mut events);
        assert_eq!(events, [fault]);
        // And it keeps running cleanly.
        for _ in 0..30 {
            set.step_all(DT).expect("step");
            reference.step(DT).expect("step");
        }
        let bad = set.get("bad").expect("bad");
        assert!(all_finite(bad));
        assert_eq!(bad.health().faults, faults_before + 1, "a single fault");
        assert_eq!(
            set.get("good").expect("good").state_digest(),
            reference.state_digest()
        );
    }
}

/// A second fault right after the first goes to the rest pose (the last good state would only
/// fail again); pinned particles the fault reached go back to where they were added.
#[test]
fn a_fault_right_after_a_fault_resets_to_the_rest_pose() {
    let mut s = sheet(8, 0.05, 4);
    let rest: Vec<Vec3> = s.particles().iter().map(|p| p.position).collect();
    for _ in 0..10 {
        s.step(DT).expect("step");
    }
    poison(&mut s, 20);
    s.step(DT).expect("step");
    assert_eq!(
        s.health().last_fault.expect("fault").recovery,
        ClothRecovery::LastGood
    );
    poison(&mut s, 20);
    s.step(DT).expect("step");
    let health = s.health();
    assert_eq!(health.faults, 2);
    assert_eq!(health.pose_resets, 1);
    assert_eq!(
        health.last_fault.expect("fault").recovery,
        ClothRecovery::RestPose
    );
    assert_eq!(ClothRecovery::RestPose.name(), "rest_pose");
    let now: Vec<Vec3> = s.particles().iter().map(|p| p.position).collect();
    assert_eq!(now, rest, "rest pose");
    assert!(all_finite(&s));
    // A clean step clears the streak: the next fault restores the last good state again.
    s.step(DT).expect("step");
    poison(&mut s, 5);
    s.step(DT).expect("step");
    assert_eq!(
        s.health().last_fault.expect("fault").recovery,
        ClothRecovery::LastGood
    );
}

/// A poisoned pinned particle (inverse mass 0) is repaired too: `0 * NaN` used to spread the NaN
/// to every pinned particle through the distance update.
#[test]
fn a_poisoned_pin_is_repaired() {
    let mut s = sheet(8, 0.05, 4);
    for _ in 0..5 {
        s.step(DT).expect("step");
    }
    let pinned: Vec<Vec3> = s.particles()[..8].iter().map(|p| p.position).collect();
    poison(&mut s, 3);
    s.step(DT).expect("step");
    assert!(all_finite(&s));
    let now: Vec<Vec3> = s.particles()[..8].iter().map(|p| p.position).collect();
    assert_eq!(now, pinned);
}

/// A fault keeps a pin where game code moved it since the last good step (only the free
/// particles go back), unless the fault reached the pin.
#[test]
fn a_fault_keeps_the_callers_pin_move() {
    let mut s = sheet(8, 0.05, 4);
    for _ in 0..10 {
        s.step(DT).expect("step");
    }
    let moved = s.particles()[3].position + Vec3::new(0.01, 0.0, 0.0);
    s.set_position(3, moved).expect("pin move");
    let other_pins: Vec<Vec3> = s.particles()[..8].iter().map(|p| p.position).collect();
    poison(&mut s, 40);
    s.step(DT).expect("step");
    let fault = s.health().last_fault.expect("fault");
    assert_eq!(fault.kind, ClothFaultKind::NonFinite);
    assert_eq!(fault.recovery, ClothRecovery::LastGood);
    assert!(all_finite(&s));
    let now: Vec<Vec3> = s.particles()[..8].iter().map(|p| p.position).collect();
    assert_eq!(now, other_pins, "every pin where the caller left it");
    assert_eq!(s.particles()[3].position, moved);
}

/// A pin moved far stretches its links past `max_strain` before the step (here the link to its
/// pinned neighbour, which no solver can shorten): no fault, and the pin stays where it was put.
/// The guard used to fault every step and put the pin back.
#[test]
fn a_pin_moved_far_is_followed_not_undone() {
    let mut s = sheet(8, 0.05, 4);
    for _ in 0..30 {
        s.step(DT).expect("step");
    }
    let target = s.particles()[0].position - Vec3::new(1.0, 0.0, 0.0);
    s.set_position(0, target).expect("move the pin");
    for step in 0..120 {
        s.step(DT).expect("step");
        assert!(all_finite(&s), "step {step}");
        assert_eq!(
            s.particles()[0].position,
            target,
            "step {step}: the pin move was undone"
        );
    }
    let health = s.health();
    assert_eq!(health.faults, 0, "{:?}", health.last_fault);
    // The pin-to-pin link stays 21x (1.05 m over 0.05 m): no solver can shorten it, and the
    // inextensible links between the two pins cannot all hold either.
    assert!(health.max_strain > DEFAULT_GUARD_STRAIN, "{}", health.max_strain);
}

/// A reset drops the last good copy: a fault right after it goes to the pose, not to a state
/// from before the reset.
#[test]
fn a_fault_after_a_reset_goes_to_the_pose() {
    let mut s = sheet(8, 0.05, 4);
    let rest: Vec<Vec3> = s.particles().iter().map(|p| p.position).collect();
    for _ in 0..20 {
        s.step(DT).expect("step");
    }
    s.reset_to_pose();
    poison(&mut s, 20);
    s.step(DT).expect("step");
    let fault = s.health().last_fault.expect("fault");
    assert_eq!(fault.recovery, ClothRecovery::RestPose);
    let now: Vec<Vec3> = s.particles().iter().map(|p| p.position).collect();
    assert_eq!(now, rest);
}

/// The two-way reaction impulses of a faulted step never reach the rigid bodies: after the
/// fault the solver holds exactly the impulses it held before that step.
#[test]
fn a_faulted_step_drops_its_reaction_impulses() {
    let mut s = ClothSolver::new(ClothConfig::default()).expect("config");
    s.add_grid(&ClothGrid {
        origin: Vec3::new(-0.27, 0.6, -0.27),
        u_axis: Vec3::X,
        v_axis: Vec3::Z,
        columns: 10,
        rows: 10,
        spacing: 0.06,
        particle_mass: 0.05,
        stretch_compliance: 0.0,
        shear_compliance: 1e-4,
        bend_compliance: 1e-2,
    })
    .expect("grid");
    s.set_snapshot(
        WorldSnapshot::new(vec![SnapshotShape {
            key: (0, 0, 7),
            kind: SnapshotKind::Box {
                half_extents: Vec3::new(2.0, 0.5, 2.0),
            },
            prev: Isometry::from_translation(Vec3::ZERO),
            cur: Isometry::from_translation(Vec3::ZERO),
            friction: 0.4,
        }])
        .expect("snapshot"),
    );
    s.set_reaction(Some(5.0)).expect("reaction");
    let mut out = Vec::new();
    for _ in 0..30 {
        s.step(DT).expect("step");
        out.clear();
        s.take_reaction(&mut out);
    }
    // One clean step leaves impulses pending (the cloth rests on the box).
    s.step(DT).expect("step");
    let mut expected = Vec::new();
    s.clone().take_reaction(&mut expected);
    assert!(!expected.is_empty(), "the cloth must rest on the box");
    poison(&mut s, 55);
    s.step(DT).expect("step");
    assert_eq!(
        s.health().last_fault.expect("fault").kind,
        ClothFaultKind::NonFinite
    );
    let mut got = Vec::new();
    s.take_reaction(&mut got);
    assert_eq!(got, expected);
}

/// Runaway: a link over `max_strain` faults with `cloth_fault_runaway` and the strain it saw.
#[test]
fn a_runaway_link_faults_with_its_strain() {
    let mut s = sheet(8, 0.05, 4);
    // Without the stiff limit, which would pull the corner back before the guard looks.
    s.set_guard(ClothGuard {
        max_strain: 3.0,
        stiff_limit: None,
        ..ClothGuard::default()
    })
    .expect("guard");
    for _ in 0..5 {
        s.step(DT).expect("step");
    }
    // A free corner 5 m away: one step cannot pull it back under 3x.
    s.set_position(63, Vec3::new(0.35, 1.5, 5.0)).expect("position");
    s.step(DT).expect("step");
    let fault = s.health().last_fault.expect("fault");
    assert_eq!(fault.kind, ClothFaultKind::Runaway);
    assert_eq!(fault.code(), "cloth_fault_runaway");
    assert!(fault.strain > 3.0, "{}", fault.strain);
    assert_eq!(fault.particle, None);
    assert!(s.health().max_strain <= 3.0);
    assert_eq!(s.health().runaway_faults, 1);
    assert_eq!(s.health().non_finite_faults, 0);
}

/// Rescue review finding 1: tearing runs only after a step the guard passed. A tear-enabled
/// cloth whose free corner spikes past both `tear.strain` and `max_strain` in one step faults
/// and is restored with no link cut — the old order cut the links first, saw the remaining
/// strain pass, and never recorded the runaway it had hidden.
#[test]
fn a_faulted_step_is_never_torn() {
    let mut s = sheet(8, 0.05, 4);
    s.set_guard(ClothGuard {
        stiff_limit: None,
        ..ClothGuard::default()
    })
    .expect("guard");
    s.set_tuning(ClothTuning {
        tear: Some(TearParams {
            strain: 2.0,
            max_per_step: 4,
        }),
        ..ClothTuning::default()
    })
    .expect("tuning");
    for _ in 0..10 {
        s.step(DT).expect("step");
    }
    let good = s.particles()[63].position;
    // 0.6 m is 12 rest lengths (0.05 m): over `max_strain` (10) and over `tear.strain` (2).
    s.set_position(63, good + Vec3::new(0.0, 0.0, 0.6))
        .expect("position");
    s.step(DT).expect("step");
    let fault = s.health().last_fault.expect("fault");
    assert_eq!(fault.kind, ClothFaultKind::Runaway);
    assert_eq!(s.diagnostics().torn, 0, "a faulted step cuts nothing");
    let mut torn = Vec::new();
    s.drain_torn(&mut torn);
    assert!(torn.is_empty(), "{torn:?}");
    // The last good step is restored wholesale: the corner is back where it passed.
    assert_eq!(s.particles()[63].position, good, "restored to the last good step");
    assert!(all_finite(&s));
}

/// Rescue review finding 2: the runaway detector never counts compliant links, so an elastic
/// rope hanging stably far past `max_strain` (its compliance leaves it stretched under
/// gravity; here that is thousands of rest lengths) is not a runaway: no fault, and
/// `health.max_strain` reports the zero-compliance strain only (none on this rope).
#[test]
fn an_elastic_link_stretched_past_max_strain_is_no_fault() {
    let mut s = ClothSolver::new(ClothConfig {
        substeps: 4,
        iterations: 1,
        ..ClothConfig::default()
    })
    .expect("config");
    let a = s.add_particle(Vec3::ZERO, 0.0).expect("pin");
    let b = s.add_particle(Vec3::new(0.0, -0.05, 0.0), 20.0).expect("free");
    s.add_distance(a, b, 3.0).expect("compliant link");
    for step in 0..120 {
        s.step(DT).expect("step");
        assert!(all_finite(&s), "step {step}");
    }
    let ratio = s.particles()[b].position.distance(s.particles()[a].position) / 0.05;
    eprintln!("elastic rope hangs at {ratio}x its rest length");
    assert!(
        ratio > DEFAULT_GUARD_STRAIN,
        "the rope really stretches past {DEFAULT_GUARD_STRAIN}x: {ratio}"
    );
    let health = s.health();
    assert_eq!(health.faults, 0, "{:?}", health.last_fault);
    assert_eq!(health.max_strain, 0.0, "stiff strain only: {}", health.max_strain);
}

/// Rescue review finding 3: the moved-pin excuse is per link. A pin pair parked 21 rest
/// lengths apart keeps its own link excused (no solver can shorten it, and the cloth follows
/// the pin), but it must not lower the runaway bar for the rest of the cloth: a free corner
/// blown past `max_strain` away from the moved pins still faults.
#[test]
fn a_runaway_link_away_from_the_moved_pins_faults() {
    let mut s = sheet(8, 0.05, 4);
    s.set_guard(ClothGuard {
        stiff_limit: None,
        ..ClothGuard::default()
    })
    .expect("guard");
    for _ in 0..30 {
        s.step(DT).expect("step");
    }
    // Park pin 0 1.05 m (21 rest lengths) from its pinned neighbor: the pin-to-pin link stays
    // past `max_strain` (10) for ever.
    let far = s.particles()[1].position + Vec3::new(1.05, 0.0, 0.0);
    s.set_position(0, far).expect("move the pin");
    // The same step blows a free corner 12 rest lengths out: over `max_strain`, and no moved
    // pin is incident, so nothing excuses it.
    let corner = s.particles()[63].position;
    s.set_position(63, corner + Vec3::new(0.0, 0.0, 0.6))
        .expect("position");
    s.step(DT).expect("step");
    let fault = s.health().last_fault.expect("fault");
    assert_eq!(
        fault.kind,
        ClothFaultKind::Runaway,
        "the far pin pair must not excuse an unrelated link"
    );
    assert_eq!(s.health().runaway_faults, 1);
}

/// Guard settings are validated; a rejected setting changes nothing.
#[test]
fn guard_settings_are_validated() {
    let mut s = sheet(4, 0.05, 4);
    assert_eq!(s.guard(), ClothGuard::default());
    assert_eq!(s.guard().max_strain, DEFAULT_GUARD_STRAIN);
    assert_eq!(s.guard().stiff_limit, Some(DEFAULT_STIFF_LIMIT));
    for bad in [1.0, 0.5, f32::NAN, f32::INFINITY, -2.0] {
        for guard in [
            ClothGuard {
                max_strain: bad,
                ..ClothGuard::default()
            },
            ClothGuard {
                stiff_limit: Some(bad),
                ..ClothGuard::default()
            },
        ] {
            let err = s.set_guard(guard).expect_err("rejected");
            assert_eq!(err.code(), "cloth_bad_param");
        }
    }
    let err = s
        .set_guard(ClothGuard {
            extra_iterations: MAX_ITERATIONS + 1,
            ..ClothGuard::default()
        })
        .expect_err("rejected");
    assert_eq!(err.code(), "cloth_bad_param");
    assert_eq!(s.guard(), ClothGuard::default());
}

/// The event queue holds at most `FAULT_EVENTS_MAX` faults; the counters keep counting.
#[test]
fn fault_events_are_bounded() {
    let mut s = sheet(4, 0.05, 2);
    for _ in 0..FAULT_EVENTS_MAX + 5 {
        poison(&mut s, 7);
        s.step(DT).expect("step");
    }
    let mut events = Vec::new();
    s.drain_faults(&mut events);
    assert_eq!(events.len(), FAULT_EVENTS_MAX);
    assert_eq!(s.health().faults, (FAULT_EVENTS_MAX + 5) as u64);
    s.drain_faults(&mut events);
    assert_eq!(events.len(), FAULT_EVENTS_MAX, "drained once");
}

/// An origin rebase moves the last good copy with the particles, so a restore after the shift
/// lands where the cloth is, not where it was.
#[test]
fn the_last_good_copy_follows_an_origin_shift() {
    let mut s = sheet(8, 0.05, 4);
    for _ in 0..5 {
        s.step(DT).expect("step");
    }
    // The last good copy is this state; shift it, then fault.
    let shift = Vec3::new(100.0, 0.0, -50.0);
    s.shift_origin(shift);
    let shifted: Vec<Vec3> = s.particles().iter().map(|p| p.position).collect();
    poison(&mut s, 30);
    s.step(DT).expect("step");
    assert_eq!(
        s.health().last_fault.expect("fault").recovery,
        ClothRecovery::LastGood
    );
    let now: Vec<Vec3> = s.particles().iter().map(|p| p.position).collect();
    assert_eq!(now, shifted);
}

/// Without the stiff limit the spec budget sheet blows up as it did before ADR-3273; the guard
/// alone keeps it finite at the end of every step by restoring it.
#[test]
fn without_the_stiff_limit_the_guard_catches_the_blow_up() {
    let mut s = sheet(32, 0.03, 4);
    s.set_guard(ClothGuard {
        stiff_limit: None,
        ..ClothGuard::default()
    })
    .expect("guard");
    for step in 0..600 {
        s.step(DT).expect("step");
        assert!(all_finite(&s), "non-finite at step {step}");
        assert!(s.health().max_strain <= DEFAULT_GUARD_STRAIN, "step {step}");
    }
    let health = s.health();
    eprintln!("without the stiff limit: {health:?}");
    assert!(health.faults > 0, "the unconverged sheet must trip the guard");
    assert_eq!(health.extra_iterations, 0);
}

/// The spec 55 test 1 sheet (16 x 16, 0.05 m, 8 substeps, damping 0.5, its digest pinned in
/// `cloth_spec55.rs`).
fn spec55_hanging_sheet() -> ClothSolver {
    let mut s = ClothSolver::new(ClothConfig {
        substeps: 8,
        damping: 0.5,
        ..ClothConfig::default()
    })
    .expect("config");
    let h = s
        .add_grid(&ClothGrid {
            origin: Vec3::new(0.0, 2.0, 0.0),
            u_axis: Vec3::X,
            v_axis: Vec3::Z,
            columns: 16,
            rows: 16,
            spacing: 0.05,
            particle_mass: 0.05,
            stretch_compliance: 0.0,
            shear_compliance: 1e-4,
            bend_compliance: 1e-2,
        })
        .expect("grid");
    for c in 0..16 {
        s.pin(h.index(c, 0).expect("row 0")).expect("pin");
    }
    s
}

/// A sheet whose stiff links stay within the limit never iterates more: its digest equals a
/// solver without the limit at every step (the pinned spec 55 digest relies on this).
#[test]
fn a_converged_sheet_never_triggers_the_stiff_limit() {
    let mut with = spec55_hanging_sheet();
    let mut without = spec55_hanging_sheet();
    without
        .set_guard(ClothGuard {
            stiff_limit: None,
            ..ClothGuard::default()
        })
        .expect("guard");
    for step in 0..600 {
        with.step(DT).expect("step");
        without.step(DT).expect("step");
        assert_eq!(with.state_digest(), without.state_digest(), "step {step}");
    }
    assert_eq!(with.health().extra_iterations, 0);
    assert_eq!(with.health().stiff_projections, 0);
}

/// The fix at work on the repro: the stiff projection held it (no extra iterations by
/// default), on some substeps only; with extra iterations switched on it holds too.
#[test]
fn the_spec_budget_sheet_is_held_by_the_stiff_projection() {
    let mut s = sheet(32, 0.03, 4);
    let mut iterating = sheet(32, 0.03, 4);
    iterating
        .set_guard(ClothGuard {
            extra_iterations: 2,
            ..ClothGuard::default()
        })
        .expect("guard");
    let mut worst = 0.0f32;
    for _ in 0..600 {
        s.step(DT).expect("step");
        iterating.step(DT).expect("step");
        assert!(all_finite(&iterating));
        worst = worst.max(iterating.stats().max_stretch);
    }
    let substeps = 600 * 4;
    let health = s.health();
    eprintln!(
        "spec budget sheet: {} stiff projections over {substeps} substeps; with 2 extra iterations: {:?}, worst strain {worst}",
        health.stiff_projections,
        iterating.health()
    );
    assert_eq!(health.extra_iterations, 0);
    assert!(health.stiff_projections > 0 && health.stiff_projections < substeps);
    assert!(iterating.health().extra_iterations > 0);
    assert!(worst < 1.1, "{worst}");
}

/// One matrix cell: the § 10 sheet at `n`, `spacing`, `substeps` for 600 steps.
struct Cell {
    finite: bool,
    peak: f32,
    stiff: f32,
    shear: f32,
    faults: u64,
    extra: u64,
    projections: u64,
}

fn run_cell(n: usize, spacing: f32, substeps: u32) -> Cell {
    let mut s = sheet(n, spacing, substeps);
    let mut cell = Cell {
        finite: true,
        peak: 0.0,
        stiff: 0.0,
        shear: 0.0,
        faults: 0,
        extra: 0,
        projections: 0,
    };
    for _ in 0..600 {
        s.step(DT).expect("step");
        cell.finite &= all_finite(&s);
        cell.peak = cell.peak.max(s.stats().max_stretch);
        let (stiff, shear) = split_strain(&s, n, spacing);
        cell.stiff = cell.stiff.max(stiff);
        cell.shear = cell.shear.max(shear);
    }
    let h = s.health();
    cell.faults = h.faults;
    cell.extra = h.extra_iterations;
    cell.projections = h.stiff_projections;
    cell
}

/// The load number of ADR-3273: rows times the distance gravity moves a particle in one substep,
/// over the link length, `rows * g * h² / spacing`. Above `UNSTABLE_LOAD` one sweep per substep
/// cannot carry the sheet even with the extra iterations.
fn load_number(n: usize, spacing: f32, substeps: u32) -> f32 {
    let h = DT / substeps as f32;
    n as f32 * 9.81 * h * h / spacing
}

/// Hand-run report of the whole ADR-3273 matrix (grids 8 to 64, spacing 0.01 to 0.1 m, substeps
/// 1 to 16, 600 steps each): prints one line per cell. Never part of the gate (minutes in a
/// debug build). `cargo test -p regia_physics --release --test cloth_stability -- --ignored --nocapture`.
#[test]
#[ignore = "hand-run matrix report"]
fn matrix_report() {
    for n in [8usize, 16, 32, 64] {
        for spacing in [0.01f32, 0.03, 0.1] {
            for substeps in [1u32, 2, 4, 8, 16] {
                let c = run_cell(n, spacing, substeps);
                eprintln!(
                    "n {n:>2} spacing {spacing:<4} substeps {substeps:>2} load {:>6.3}: finite {} peak {:.4} stiff {:.4} shear {:.4} faults {} extra/substep {:.2} projections {}",
                    load_number(n, spacing, substeps),
                    c.finite,
                    c.peak,
                    c.stiff,
                    c.shear,
                    c.faults,
                    c.extra as f64 / (600.0 * f64::from(substeps)),
                    c.projections
                );
            }
        }
    }
}

/// Hand-run scan for ADR-3273: per stiff limit and extra-iteration cap, whether the spec 55
/// test 1 sheet triggers the limit and how the repro sheet does.
#[test]
#[ignore = "hand-run scan"]
fn stiff_limit_scan() {
    for (limit, target) in [
        (1.07f32, 1.0f32),
        (1.07, 1.02),
        (1.07, 1.04),
        (1.08, 1.0),
        (1.1, 1.0),
    ] {
        for extra in [0u32, 1, 2] {
            let guard = ClothGuard {
                stiff_limit: Some(limit),
                stiff_target: target,
                extra_iterations: extra,
                ..ClothGuard::default()
            };
            let mut hanging = spec55_hanging_sheet();
            hanging.set_guard(guard).expect("guard");
            let mut repro = sheet(32, 0.03, 4);
            repro.set_guard(guard).expect("guard");
            let mut worst = 0.0f32;
            let mut finite = true;
            for _ in 0..600 {
                hanging.step(DT).expect("step");
                repro.step(DT).expect("step");
                worst = worst.max(repro.stats().max_stretch);
                finite &= all_finite(&repro);
            }
            let (hh, rh) = (hanging.health(), repro.health());
            eprintln!(
                "limit {limit} target {target} extra {extra}: hanging extra {} proj {} | repro worst {worst:.4} finite {finite} faults {} extra/substep {:.2} proj {}",
                hh.extra_iterations,
                hh.stiff_projections,
                rh.faults,
                rh.extra_iterations as f64 / 2400.0,
                rh.stiff_projections
            );
        }
    }
}

/// Load number up to which ADR-3273 measured every matrix cell's zero-compliance links under
/// 1.1x (the heaviest such cell, 32 x 32 at 0.03 m on one substep, load 2.9, reached 1.098).
const STABLE_LOAD: f32 = 2.5;

/// Bound on every matrix cell's zero-compliance links above `STABLE_LOAD` and on every cell's
/// compliant shear diagonals: ADR-3273 measured at most 1.296 (64 x 64 at 0.01 m on one substep)
/// and 1.323 (64 x 64 at 0.1 m on 4 substeps, x86_64). Far below the guard's `DEFAULT_GUARD_STRAIN`, so a
/// regression that only the guard would catch still fails here.
const COARSE_BOUND: f32 = 1.4;

/// The ADR-3273 regression matrix: grids 8 to 64, spacing 0.01 to 0.1 m, substeps 1 to 16, 600
/// steps each (64 x 64 at 16 substeps is left to the hand-run `matrix_report` for time). Every
/// cell must stay finite and fault-free. Cells with a load number up to `STABLE_LOAD` must keep
/// their zero-compliance links under 1.1x; heavier cells are documented as too coarse for that
/// and must stay under `COARSE_BOUND`. The compliant shear diagonals may shear in plane (up to
/// 1.32x on 3 to 6 m sheets at 4 substeps, ADR-3273) and must stay under `COARSE_BOUND` too.
/// Cells run on their own threads; each is deterministic.
#[test]
fn regression_matrix() {
    let mut cells = Vec::new();
    for n in [8usize, 16, 32, 64] {
        for spacing in [0.01f32, 0.03, 0.1] {
            for substeps in [1u32, 4, 16] {
                if n == 64 && substeps == 16 {
                    continue;
                }
                cells.push((n, spacing, substeps));
            }
        }
    }
    let results: Vec<((usize, f32, u32), Cell)> = std::thread::scope(|scope| {
        let handles: Vec<_> = cells
            .iter()
            .map(|&(n, spacing, substeps)| {
                scope.spawn(move || {
                    let mut s = sheet(n, spacing, substeps);
                    let mut cell = Cell {
                        finite: true,
                        peak: 0.0,
                        stiff: 0.0,
                        shear: 0.0,
                        faults: 0,
                        extra: 0,
                        projections: 0,
                    };
                    for _ in 0..600 {
                        s.step(DT).expect("step");
                        cell.finite &= all_finite(&s);
                        cell.peak = cell.peak.max(s.stats().max_stretch);
                        let (stiff, shear) = split_strain(&s, n, spacing);
                        cell.stiff = cell.stiff.max(stiff);
                        cell.shear = cell.shear.max(shear);
                    }
                    let h = s.health();
                    cell.faults = h.faults;
                    cell.extra = h.extra_iterations;
                    cell.projections = h.stiff_projections;
                    ((n, spacing, substeps), cell)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("cell thread"))
            .collect()
    });
    let mut problems = Vec::new();
    for ((n, spacing, substeps), c) in &results {
        let load = load_number(*n, *spacing, *substeps);
        eprintln!(
            "n {n:>2} spacing {spacing:<4} substeps {substeps:>2} load {load:>6.3}: finite {} peak {:.4} stiff {:.4} shear {:.4} faults {} extra {} projections {}",
            c.finite, c.peak, c.stiff, c.shear, c.faults, c.extra, c.projections
        );
        if !c.finite || c.faults > 0 {
            problems.push(format!(
                "n {n} spacing {spacing} substeps {substeps}: finite {} faults {}",
                c.finite, c.faults
            ));
        } else {
            let stiff_bound = if load <= STABLE_LOAD { 1.1 } else { COARSE_BOUND };
            if c.stiff >= stiff_bound {
                problems.push(format!(
                    "n {n} spacing {spacing} substeps {substeps} (load {load:.3}): stiff links {} (bound {stiff_bound})",
                    c.stiff
                ));
            }
            if c.shear >= COARSE_BOUND {
                problems.push(format!(
                    "n {n} spacing {spacing} substeps {substeps} (load {load:.3}): shear links {}",
                    c.shear
                ));
            }
        }
    }
    assert!(problems.is_empty(), "matrix:\n{}", problems.join("\n"));
}

/// Hand-run report for ADR-3273: the cells whose compliant shear diagonals passed 1.1x, with
/// extra iterations switched on.
#[test]
#[ignore = "hand-run report"]
fn shear_cells_with_extra_iterations() {
    for (n, spacing, substeps) in [
        (32usize, 0.01f32, 16u32),
        (32, 0.1, 4),
        (64, 0.03, 4),
        (64, 0.1, 4),
    ] {
        for extra in [0u32, 1, 2, 4] {
            let mut s = sheet(n, spacing, substeps);
            s.set_guard(ClothGuard {
                extra_iterations: extra,
                ..ClothGuard::default()
            })
            .expect("guard");
            let (mut stiff, mut shear) = (0.0f32, 0.0f32);
            for _ in 0..600 {
                s.step(DT).expect("step");
                let (a, b) = split_strain(&s, n, spacing);
                stiff = stiff.max(a);
                shear = shear.max(b);
            }
            eprintln!(
                "n {n} spacing {spacing} substeps {substeps} extra {extra}: stiff {stiff:.4} shear {shear:.4} extra/substep {:.2} faults {}",
                s.health().extra_iterations as f64 / (600.0 * f64::from(substeps)),
                s.health().faults
            );
        }
    }
}
