// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane: `ClothSet`, config limits and frame-length extremes of the cloth solver.

use regia_core::math::Vec3;
use regia_physics::cloth::{
    ClothConfig, ClothError, ClothRope, ClothSet, ClothSolver, MAX_ITERATIONS, MAX_SUBSTEPS,
};

fn rope_solver(config: ClothConfig) -> ClothSolver {
    let mut s = ClothSolver::new(config).expect("config");
    let rope = ClothRope {
        start: Vec3::new(0.0, 3.0, 0.0),
        end: Vec3::new(2.0, 3.0, 0.0),
        segments: 8,
        particle_mass: 0.05,
        stretch_compliance: 0.0,
        bend_compliance: 1.0,
    };
    let handle = s.add_rope(&rope).expect("rope");
    s.pin(handle.first).expect("pin");
    s
}

#[test]
fn config_limits_are_inclusive_at_the_top_and_exclusive_at_zero() {
    let ok = |f: fn(&mut ClothConfig)| {
        let mut c = ClothConfig::default();
        f(&mut c);
        c.validate()
    };
    assert!(ok(|c| c.substeps = MAX_SUBSTEPS).is_ok());
    assert!(ok(|c| c.substeps = MAX_SUBSTEPS + 1).is_err());
    assert!(ok(|c| c.substeps = 0).is_err());
    assert!(ok(|c| c.iterations = MAX_ITERATIONS).is_ok());
    assert!(ok(|c| c.iterations = MAX_ITERATIONS + 1).is_err());
    assert!(ok(|c| c.iterations = 0).is_err());
    assert!(ok(|c| c.max_steps_per_call = 0).is_err());
    assert!(ok(|c| c.time_step = 0.0).is_err());
    assert!(ok(|c| c.time_step = -0.01).is_err());
    // A step so short that 1 / substep^2 overflows would fill the particles with NaN: refused.
    assert!(matches!(
        ok(|c| c.time_step = f32::MIN_POSITIVE),
        Err(ClothError::InvalidConfig { what: "time step" })
    ));
    assert!(ok(|c| c.time_step = 1.0e-6).is_ok());
    assert!(ok(|c| c.damping = 0.0).is_ok());
    assert!(ok(|c| c.thickness = 0.0).is_ok());
    assert!(matches!(
        ok(|c| c.damping = -0.0001),
        Err(ClothError::Negative { what: "damping" })
    ));
    assert!(matches!(
        ok(|c| c.gravity.z = f32::NAN),
        Err(ClothError::NonFinite { what: "gravity" })
    ));
    assert!(matches!(
        ok(|c| c.thickness = f32::INFINITY),
        Err(ClothError::NonFinite { .. })
    ));
}

#[test]
fn a_tiny_time_step_with_a_long_frame_drops_the_excess_without_hanging() {
    let config = ClothConfig {
        time_step: 1.0e-6,
        ..ClothConfig::default()
    };
    let mut s = rope_solver(config);
    let ran = s.step(1.0).expect("finite");
    assert_eq!(ran, config.max_steps_per_call);
    assert!(s.stats().dropped_steps > 0);
    let ran = s.step(f32::MAX).expect("finite");
    assert_eq!(ran, config.max_steps_per_call);
    // The accumulator stays usable: a zero-length frame runs nothing.
    assert_eq!(s.step(0.0).expect("zero"), 0);
    assert!(
        s.particles()
            .iter()
            .all(|p| p.position.is_finite() && p.velocity.is_finite())
    );
}

#[test]
fn rejected_step_lengths_change_nothing() {
    let mut s = rope_solver(ClothConfig::default());
    s.step(1.0 / 60.0).expect("warm up");
    let digest = s.state_digest();
    for dt in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.001] {
        assert!(s.step(dt).is_err(), "{dt}");
        assert_eq!(s.state_digest(), digest, "{dt}");
    }
}

#[test]
fn cloth_set_step_all_validates_dt_before_any_solver_runs() {
    let mut set = ClothSet::new();
    set.insert("a", rope_solver(ClothConfig::default()));
    set.insert("b", rope_solver(ClothConfig::default()));
    let (da, db) = (
        set.get("a").expect("a").state_digest(),
        set.get("b").expect("b").state_digest(),
    );
    assert!(set.step_all(f32::NAN).is_err());
    assert!(set.step_all(-1.0).is_err());
    assert_eq!(set.get("a").expect("a").state_digest(), da);
    assert_eq!(set.get("b").expect("b").state_digest(), db);
    assert_eq!(
        set.step_all(2.5 / 60.0).expect("ok"),
        4,
        "two solvers, two fixed steps each"
    );
    assert_ne!(set.get("a").expect("a").state_digest(), da);
    assert_eq!(set.step_all(0.0).expect("zero"), 0);
}

#[test]
fn cloth_set_replace_remove_and_lookup() {
    let mut set = ClothSet::new();
    assert!(set.is_empty());
    assert!(set.insert("rope", rope_solver(ClothConfig::default())).is_none());
    let mut other = rope_solver(ClothConfig::default());
    other.step(1.0 / 30.0).expect("step");
    let steps_before = other.stats().steps;
    let replaced = set.insert("rope", other).expect("replaces");
    assert_eq!(replaced.stats().steps, 0, "the old solver comes back");
    assert_eq!(set.len(), 1);
    assert_eq!(set.get("rope").expect("rope").stats().steps, steps_before);
    assert!(set.get("nothing").is_none());
    assert!(set.get_mut("nothing").is_none());
    assert!(set.remove("nothing").is_none());
    assert_eq!(set.iter().map(|(n, _)| n).collect::<Vec<_>>(), vec!["rope"]);
    assert!(set.remove("rope").is_some());
    assert!(set.is_empty());
    assert_eq!(set.step_all(1.0).expect("empty set"), 0);
}
