// SPDX-License-Identifier: MPL-2.0
//! Host settings, contact boundaries, and deterministic candidate ordering.
use regia_detsim::avoid::{
    OrcaBody, OrcaSettings, m_to_mm, mm_s_to_m_s, mm_to_m, orca_velocity, orca_velocity_with_settings,
};

fn body(id: u32, x_mm: i64, z_mm: i64, vx_mm_s: i64) -> OrcaBody {
    OrcaBody {
        id,
        x_mm,
        z_mm,
        vx_mm_s,
        vz_mm_s: 0,
        r_mm: 2_000,
    }
}

#[test]
fn defaults_preserve_the_compatibility_entry_point() {
    let me = body(1, 0, 0, 5_000);
    let others = [body(2, 20_000, 0, -5_000), body(3, 1_000, 2_000, 0)];
    for preferred in [[8_000, 0], [0, 0], [499, 0], [-8_000, 3_000]] {
        assert_eq!(
            orca_velocity_with_settings(&me, preferred, &others, &OrcaSettings::default()),
            orca_velocity(&me, preferred, &others)
        );
    }
}

#[test]
fn each_solver_setting_controls_its_documented_boundary() {
    let me = body(1, -10_000, 0, 5_000);
    let other = body(2, 10_000, 0, -5_000);
    let solve =
        |preferred, settings: OrcaSettings| orca_velocity_with_settings(&me, preferred, &[other], &settings);
    assert_eq!(solve([8_000, 0], OrcaSettings::default()), [4_000, -600]);
    assert_eq!(
        solve(
            [8_000, 0],
            OrcaSettings {
                max_neighbors: 0,
                ..Default::default()
            }
        ),
        [8_000, 0]
    );
    assert_eq!(
        solve(
            [8_000, 0],
            OrcaSettings {
                horizon_ms: 1_999,
                ..Default::default()
            }
        ),
        [8_000, -600]
    );
    assert_eq!(
        solve(
            [8_000, 0],
            OrcaSettings {
                keep_right_mm_s: 0,
                ..Default::default()
            }
        ),
        [4_000, 0]
    );
    assert_eq!(
        solve(
            [8_000, 0],
            OrcaSettings {
                head_on_range_mm: 19_999,
                ..Default::default()
            }
        ),
        [4_000, 0]
    );
    assert_eq!(
        solve(
            [8_000, 0],
            OrcaSettings {
                head_on_range_mm: 20_000,
                ..Default::default()
            }
        ),
        [4_000, -600]
    );
    assert_eq!(solve([499, 0], OrcaSettings::default()), [499, 0]);
    assert_eq!(solve([500, 0], OrcaSettings::default()), [500, -600]);
    assert_eq!(
        solve(
            [500, 0],
            OrcaSettings {
                min_preferred_mm_s: 501,
                ..Default::default()
            }
        ),
        [500, 0]
    );
    // The broadphase look distance is consumed by the host, not by this kernel.
    assert_eq!(
        solve(
            [8_000, 0],
            OrcaSettings {
                look_m: 0.0,
                ..Default::default()
            }
        ),
        [4_000, -600]
    );
}

#[test]
fn equal_distance_ids_break_ties_before_the_configured_cap() {
    let me = body(1, 0, 0, 0);
    let low = body(2, 1_000, 0, 0);
    let high = body(3, -1_000, 0, 0);
    let settings = OrcaSettings {
        max_neighbors: 1,
        ..Default::default()
    };
    assert_eq!(
        orca_velocity_with_settings(&me, [2_000, 0], &[low, high], &settings),
        [0, 0]
    );
    assert_eq!(
        orca_velocity_with_settings(&me, [2_000, 0], &[high, low], &settings),
        [0, 0]
    );
    assert_eq!(
        orca_velocity_with_settings(&me, [2_000, 0], &[high], &settings),
        [2_000, 0]
    );
}

#[test]
fn self_is_removed_before_the_neighbor_cap() {
    let me = body(1, 0, 0, 0);
    let other = body(2, 1_000, 0, 0);
    let settings = OrcaSettings {
        max_neighbors: 1,
        ..Default::default()
    };
    assert_eq!(
        orca_velocity_with_settings(&me, [2_000, 0], &[me, other], &settings),
        [0, 0]
    );
    assert_eq!(orca_velocity(&me, [2_000, 0], &[me]), [2_000, 0]);
}

#[test]
fn coincident_centers_use_the_id_fallback_in_both_directions() {
    let a = body(1, 0, 0, 0);
    let b = body(2, 0, 0, 0);
    assert_eq!(orca_velocity(&a, [2_000, 0], &[b]), [0, 0]);
    assert_eq!(orca_velocity(&b, [-2_000, 0], &[a]), [0, 0]);
    assert_eq!(orca_velocity(&a, [-2_000, 0], &[b]), [-2_000, 0]);
    assert_eq!(orca_velocity(&b, [2_000, 0], &[a]), [2_000, 0]);
}

#[test]
fn slow_preferences_still_project_contact_and_zero_preference_stays_zero() {
    let me = body(1, 0, 0, 0);
    let other = body(2, 1_000, 0, 0);
    assert_eq!(orca_velocity(&me, [499, 0], &[other]), [0, 0]);
    assert_eq!(orca_velocity(&me, [0, 0], &[body(2, 1_000, 0, -1_000)]), [0, 0]);
}

#[test]
fn receding_and_tangent_pairs_preserve_the_preference() {
    let me = body(1, 0, 0, 5_000);
    assert_eq!(
        orca_velocity(&me, [8_000, 0], &[body(2, 20_000, 0, 10_000)]),
        [8_000, 0]
    );
    assert_eq!(
        orca_velocity(&me, [8_000, 0], &[body(2, 20_000, 4_000, -5_000)]),
        [8_000, 0]
    );
}

#[test]
fn meter_conversions_keep_half_away_from_zero() {
    assert_eq!(m_to_mm(0.0625), 63);
    assert_eq!(m_to_mm(-0.0625), -63);
    assert_eq!(m_to_mm(0.0), 0);
    assert_eq!(m_to_mm(-0.0), 0);
    assert_eq!(mm_to_m(125), 0.125);
    assert_eq!(mm_s_to_m_s(-125), -0.125);
}

#[test]
fn host_can_raise_the_neighbor_cap_above_the_compatibility_default() {
    let me = body(1, 0, 0, 0);
    // These eight closer contact planes constrain Z only.
    let mut others: Vec<_> = (1..=8).map(|i| body(i + 1, 0, i64::from(i) * 100, 0)).collect();
    // The ninth contact plane constrains forward X motion.
    others.push(body(20, 1_500, 0, 0));
    assert_eq!(orca_velocity(&me, [2_000, 0], &others), [2_000, 0]);
    let settings = OrcaSettings {
        max_neighbors: 9,
        ..Default::default()
    };
    assert_eq!(
        orca_velocity_with_settings(&me, [2_000, 0], &others, &settings),
        [0, 0]
    );
    others.reverse();
    assert_eq!(
        orca_velocity_with_settings(&me, [2_000, 0], &others, &settings),
        [0, 0]
    );
}

#[test]
fn disabling_prediction_does_not_disable_contact_projection() {
    let me = body(1, 0, 0, 0);
    let settings = OrcaSettings {
        horizon_ms: 0,
        keep_right_mm_s: 0,
        ..Default::default()
    };
    assert_eq!(
        orca_velocity_with_settings(&me, [2_000, 0], &[body(2, 1_000, 0, 0)], &settings),
        [0, 0]
    );
}

#[test]
fn settings_domain_accepts_defaults_and_names_each_out_of_range_field() {
    use regia_detsim::avoid::{
        ORCA_MAX_HORIZON_MS, ORCA_MAX_RANGE_MM, ORCA_MAX_SPEED_MM_S, OrcaDomainError, try_orca_velocity,
    };
    let ok = OrcaSettings::default();
    assert_eq!(ok.validate(), Ok(()));
    let edge = OrcaSettings {
        max_neighbors: usize::MAX,
        horizon_ms: ORCA_MAX_HORIZON_MS,
        keep_right_mm_s: ORCA_MAX_SPEED_MM_S,
        min_preferred_mm_s: ORCA_MAX_SPEED_MM_S,
        head_on_range_mm: ORCA_MAX_RANGE_MM,
        look_m: -1.0,
        ..OrcaSettings::default()
    };
    // The look distance belongs to the neighbor query, not the solver settings.
    assert_eq!(edge.validate(), Ok(()));
    for (settings, code) in [
        (OrcaSettings { horizon_ms: -1, ..ok }, "orca_horizon"),
        (
            OrcaSettings {
                keep_right_mm_s: -1,
                ..ok
            },
            "orca_keep_right",
        ),
        (
            OrcaSettings {
                min_preferred_mm_s: ORCA_MAX_SPEED_MM_S + 1,
                ..ok
            },
            "orca_min_preferred",
        ),
        (
            OrcaSettings {
                head_on_range_mm: -1,
                ..ok
            },
            "orca_head_on_range",
        ),
    ] {
        assert_eq!(settings.validate().map_err(OrcaDomainError::code), Err(code));
        let me = body(1, 0, 0, 0);
        assert_eq!(
            try_orca_velocity(&me, [2_000, 0], &[], &settings).map_err(OrcaDomainError::code),
            Err(code)
        );
        // The plain form returns the clamped preference instead.
        assert_eq!(
            orca_velocity_with_settings(&me, [2_000, 0], &[], &settings),
            [2_000, 0]
        );
    }
}
