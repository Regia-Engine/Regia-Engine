// SPDX-License-Identifier: MPL-2.0
//! § 11 tests 8-9: the CPU mirror of the vertex stage.

use super::*;
use crate::weather::{Weather, WeatherState};

const DT: f32 = 1.0 / 60.0;

fn settings() -> PrecipitationSettings {
    PrecipitationSettings {
        enabled: true,
        ..PrecipitationSettings::default()
    }
}

fn frame_at(integrator: &PrecipIntegrator, state: WeatherState, eye: [f64; 3], clock_s: f64) -> PrecipFrame {
    let config = WeatherConfig::default();
    integrator.frame(
        eye,
        [0.0; 3],
        [0.0, -1.0],
        clock_s,
        &state.rates(&config),
        &config,
        &settings(),
        [3.0, 0.0, 1.0],
        QualityTier::High,
    )
}

/// § 11 test 9.
#[test]
fn tier_budgets_match_the_table() {
    let rows = [
        (QualityTier::MobileLow, 2_000, 1_500, 0, 12.0, 10.0, None),
        (
            QualityTier::MobileHigh,
            6_000,
            4_000,
            256,
            12.0,
            10.0,
            Some((256, 32.0)),
        ),
        (
            QualityTier::Deck,
            8_000,
            5_000,
            256,
            14.0,
            10.0,
            Some((256, 48.0)),
        ),
        (
            QualityTier::Low,
            12_000,
            8_000,
            512,
            16.0,
            12.0,
            Some((512, 64.0)),
        ),
        (
            QualityTier::Medium,
            24_000,
            16_000,
            1_024,
            18.0,
            12.0,
            Some((512, 64.0)),
        ),
        (
            QualityTier::High,
            48_000,
            30_000,
            2_048,
            18.0,
            12.0,
            Some((512, 64.0)),
        ),
        (
            QualityTier::Ultra,
            96_000,
            60_000,
            4_096,
            22.0,
            14.0,
            Some((1024, 96.0)),
        ),
    ];
    for (tier, rain, snow, splashes, half, height, occlusion) in rows {
        let b = PrecipBudget::for_quality(tier);
        assert_eq!(
            (
                b.rain,
                b.snow,
                b.splashes,
                b.half_extent_m,
                b.height_m,
                b.occlusion
            ),
            (rain, snow, splashes, half, height, occlusion),
            "{tier:?}"
        );
    }
}

/// § 11 test 9: `max_drops` caps the tier.
#[test]
fn max_drops_caps_the_budget() {
    let config = WeatherConfig::default();
    let rates = WeatherState::STORM.rates(&config);
    let budget = PrecipBudget::for_quality(QualityTier::High);
    let full = PrecipCounts::of(&rates, &config, &settings(), &budget);
    assert_eq!(full.rain, 48_000.0);
    assert_eq!(full.splashes, 2_048.0);
    let capped = PrecipCounts::of(
        &rates,
        &config,
        &PrecipitationSettings {
            max_drops: Some(1_000),
            ..settings()
        },
        &budget,
    );
    assert_eq!(capped.rain, 1_000.0);
    let none = PrecipCounts::of(
        &rates,
        &config,
        &PrecipitationSettings {
            splashes: false,
            ..settings()
        },
        &budget,
    );
    assert_eq!(none.splashes, 0.0);
    assert_eq!(PrecipCounts::instances(10.2), 11);
    assert_eq!(PrecipCounts::instances(0.0), 0);
}

/// § 11 test 8: every drop lies inside its box.
#[test]
fn every_drop_lies_inside_its_box() {
    let frame = frame_at(
        &PrecipIntegrator::default(),
        WeatherState::STORM,
        [10.0, 2.0, -7.0],
        0.0,
    );
    let shift = frame.center_shift_m();
    let (h, v) = (frame.budget.half_extent_m, frame.budget.height_m);
    for i in 0..5_000 {
        let drop = rain_drop(i, &frame);
        let flake = snow_flake(i, &frame);
        assert!((drop.position_m[0] - shift[0]).abs() <= h + 1.0e-3, "{i}");
        assert!((drop.position_m[1] - shift[1]).abs() <= v + 1.0e-3, "{i}");
        assert!((drop.position_m[2] - shift[2]).abs() <= h + 1.0e-3, "{i}");
        assert!(
            (flake.position_m[0] - shift[0]).abs() <= h + SNOW_SWAY_M + 1.0e-3,
            "{i}"
        );
    }
}

/// § 11 test 8: a drop's world position does not depend on where the box is, while it is inside.
#[test]
fn a_drop_stays_where_it_is_when_the_box_moves() {
    let integrator = PrecipIntegrator::default();
    let a = frame_at(&integrator, WeatherState::STORM, [0.0, 2.0, 0.0], 0.0);
    let b = frame_at(&integrator, WeatherState::STORM, [1.5, 2.0, -0.5], 0.0);
    let mut same = 0;
    for i in 0..2_000 {
        let pa = rain_drop(i, &a).position_m;
        let pb = rain_drop(i, &b).position_m;
        let world_a = [
            a.eye[0] as f32 + pa[0],
            a.eye[1] as f32 + pa[1],
            a.eye[2] as f32 + pa[2],
        ];
        let world_b = [
            b.eye[0] as f32 + pb[0],
            b.eye[1] as f32 + pb[1],
            b.eye[2] as f32 + pb[2],
        ];
        let gap = (world_a[0] - world_b[0])
            .hypot(world_a[1] - world_b[1])
            .hypot(world_a[2] - world_b[2]);
        if gap < 1.0e-3 {
            same += 1;
        } else {
            // It wrapped: one box size away.
            assert!(gap > 5.0, "drop {i} slid by {gap}");
        }
    }
    assert!(same > 1_500, "{same}");
}

/// § 11 test 8: between frames a drop moves by `|v| dt`, except across a box face, at alpha 0.
#[test]
fn a_drop_moves_by_its_speed_or_wraps_invisibly() {
    let config = WeatherConfig::default();
    let state = WeatherState::STORM;
    let rates = state.rates(&config);
    let budget = PrecipBudget::for_quality(QualityTier::High);
    let mut integrator = PrecipIntegrator::default();
    integrator.step(&rates, &config, [3.0, 1.0], &budget, DT);
    let mut last = frame_at(&integrator, state, [0.0, 2.0, 0.0], 0.0);
    for frame_index in 0..600 {
        integrator.step(&rates, &config, [3.0, 1.0], &budget, DT);
        let now = frame_at(&integrator, state, [0.0, 2.0, 0.0], 0.0);
        for i in 0..300 {
            let (a, b) = (rain_drop(i, &last), rain_drop(i, &now));
            let step = (b.position_m[0] - a.position_m[0])
                .hypot(b.position_m[1] - a.position_m[1])
                .hypot(b.position_m[2] - a.position_m[2]);
            let speed = (b.velocity_m_per_s[0].powi(2)
                + b.velocity_m_per_s[1].powi(2)
                + b.velocity_m_per_s[2].powi(2))
            .sqrt();
            if step > speed * DT + 1.0e-3 {
                // The last step before a face: the edge fade has it at under 1 % of a full drop.
                assert!(
                    a.alpha < 0.01 || b.alpha < 0.01,
                    "frame {frame_index} drop {i} jumped {step} at alpha {} {}",
                    a.alpha,
                    b.alpha
                );
            }
        }
        last = now;
    }
}

/// § 11 test 8: alpha is continuous in the count.
#[test]
fn count_changes_fade() {
    let mut last = count_fade(960, 1000.0);
    for k in 0..=1000 {
        let n = 1000.0 - k as f32 * 0.1;
        let a = count_fade(960, n);
        assert!(a <= last + 1.0e-6, "{k}");
        assert!((last - a).abs() < 0.05, "{k}");
        last = a;
    }
    assert_eq!(count_fade(10, 1000.0), 1.0);
    assert_eq!(count_fade(1000, 1000.0), 0.0);
}

/// § 11 test 8: rain to snow fades out of one draw and into the other, with no step.
#[test]
fn a_kind_change_moves_the_counts_smoothly() {
    let config = WeatherConfig::default();
    let budget = PrecipBudget::for_quality(QualityTier::High);
    let mut weather = Weather::new(WeatherState::STORM);
    weather.transition_to(
        WeatherState {
            precipitation: 1.0,
            ..WeatherState::BLIZZARD
        },
        10.0,
    );
    let mut last = PrecipCounts::of(&weather.rates(&config), &config, &settings(), &budget);
    for frame in 0..620 {
        weather.step(DT);
        let now = PrecipCounts::of(&weather.rates(&config), &config, &settings(), &budget);
        assert!((now.rain - last.rain).abs() < 1_000.0, "{frame}");
        assert!((now.snow - last.snow).abs() < 300.0, "{frame}");
        last = now;
    }
    assert_eq!(last.rain, 0.0);
    assert_eq!(last.snow, budget.snow as f32);
}

/// § 11 test 8: a 10 km teleport shows a field as full as a steady one, on its first frame.
#[test]
fn a_teleport_shows_a_full_field() {
    let integrator = PrecipIntegrator::default();
    let histogram = |eye: [f64; 3]| {
        let frame = frame_at(&integrator, WeatherState::STORM, eye, 0.0);
        let shift = frame.center_shift_m();
        let h = frame.budget.half_extent_m;
        let mut bins = [0_u32; 8];
        for i in 0..24_000 {
            let p = rain_drop(i, &frame).position_m;
            let x = (((p[0] - shift[0]) / (2.0 * h) + 0.5) * 4.0).clamp(0.0, 3.99) as usize;
            let z = usize::from(p[2] > shift[2]);
            bins[x + 4 * z] += 1;
        }
        bins
    };
    let steady = histogram([0.0, 2.0, 0.0]);
    let jumped = histogram([10_000.0, 2.0, 3_000.0]);
    for (a, b) in steady.into_iter().zip(jumped) {
        let (a, b) = (a as f32, b as f32);
        assert!((a - b).abs() / a < 0.1, "{steady:?} {jumped:?}");
    }
}

/// § 11 test 8: 100 km out, the eye-relative position keeps millimeter precision.
#[test]
fn far_from_the_origin_the_position_keeps_its_precision() {
    let integrator = PrecipIntegrator::default();
    let origin = frame_at(&integrator, WeatherState::STORM, [36.0, 24.0, 36.0], 0.0);
    // The same place in the box grid, 100 km out.
    let far = frame_at(
        &integrator,
        WeatherState::STORM,
        [36.0 + 36.0 * 2778.0, 24.0, 36.0 + 36.0 * 2778.0],
        0.0,
    );
    for i in 0..1_000 {
        let (a, b) = (rain_drop(i, &origin).position_m, rain_drop(i, &far).position_m);
        for axis in 0..3 {
            assert!((a[axis] - b[axis]).abs() < 1.0e-3, "{i}: {a:?} {b:?}");
        }
    }
}

#[test]
fn the_integrator_wraps_and_a_rate_change_does_not_jump() {
    let config = WeatherConfig::default();
    let budget = PrecipBudget::for_quality(QualityTier::High);
    let mut integrator = PrecipIntegrator::default();
    let storm = WeatherState::STORM.rates(&config);
    let drizzle = WeatherState::OVERCAST.rates(&config);
    for _ in 0..10_000 {
        integrator.step(&storm, &config, [2.0, 1.0], &budget, DT);
    }
    let frame = frame_at(&integrator, WeatherState::STORM, [0.0; 3], 0.0);
    for offset in frame.rain_offsets_m {
        assert!((0.0..=budget.box_size()[1]).contains(&offset), "{offset}");
    }
    let before = integrator.rain_fall_m[3];
    integrator.step(&drizzle, &config, [2.0, 1.0], &budget, DT);
    let moved = (integrator.rain_fall_m[3] - before).rem_euclid(f64::from(budget.box_size()[1]));
    assert!(moved < 0.2, "the offset carries on: {moved}");
    let frozen = integrator.clone();
    integrator.step(&drizzle, &config, [2.0, 1.0], &budget, 0.0);
    integrator.step(&drizzle, &config, [2.0, 1.0], &budget, f32::NAN);
    assert_eq!(integrator, frozen, "no time, no motion");
}

#[test]
fn splashes_stay_in_their_disk_and_fade_over_their_life() {
    let frame = frame_at(
        &PrecipIntegrator::default(),
        WeatherState::STORM,
        [5.0, 2.0, 5.0],
        1.0,
    );
    let radius = 0.5 * frame.budget.half_extent_m + 1.5;
    for i in 0..500 {
        let s = splash(i, &frame);
        assert!((s.xz_m[0] - 5.0).hypot(s.xz_m[1] - 5.0) <= radius, "{i}");
        assert!((0.0..1.0).contains(&s.phase));
        assert!((0.0..=1.0).contains(&s.alpha));
    }
}

#[test]
fn the_same_inputs_give_the_same_bits() {
    let frame = frame_at(
        &PrecipIntegrator::default(),
        WeatherState::SNOW,
        [1.0, 2.0, 3.0],
        7.5,
    );
    for i in 0..1_000 {
        assert_eq!(snow_flake(i, &frame), snow_flake(i, &frame));
        assert_eq!(hash4(i), hash4(i));
    }
    assert_ne!(hash4(1), hash4(2));
}
