// SPDX-License-Identifier: MPL-2.0
//! Ambient life on the CPU: budgets, the activity rules, the boxes, the seamless loop, the
//! determinism of the placements and of the birds' flight.

use regia_core::math::Vec3;
use regia_render::{QualityTier, TimeOfDay};

use super::*;

fn on() -> AmbientLifeSettings {
    AmbientLifeSettings {
        enabled: true,
        ..AmbientLifeSettings::default()
    }
}

fn full_counts() -> AmbientCounts {
    let budget = AmbientBudget::for_quality(QualityTier::High);
    let activity = AmbientActivity {
        motes: 1.0,
        swarms: 1.0,
        butterflies: 1.0,
        fireflies: 1.0,
        leaves: 1.0,
        birds: 1.0,
    };
    AmbientCounts::of(&activity, &on(), &budget)
}

fn frame(eye: [f64; 3], clock_s: f64) -> AmbientFrame {
    let integrator = AmbientIntegrator {
        clock_s,
        ..AmbientIntegrator::default()
    };
    AmbientFrame::new(eye, &integrator, full_counts(), Season::Autumn, 7)
}

fn summer_at(sun: f32) -> AmbientConditions {
    AmbientConditions {
        sun_elevation_sin: sun,
        season: Season::Summer,
        ..AmbientConditions::default()
    }
}

#[test]
fn off_on_low_and_below_and_growing_with_the_tier() {
    for tier in [
        QualityTier::MobileLow,
        QualityTier::MobileHigh,
        QualityTier::Deck,
        QualityTier::Low,
    ] {
        assert!(AmbientBudget::for_quality(tier).is_off(), "{tier:?}");
    }
    let medium = AmbientBudget::for_quality(QualityTier::Medium);
    let high = AmbientBudget::for_quality(QualityTier::High);
    let ultra = AmbientBudget::for_quality(QualityTier::Ultra);
    assert!(medium.motes < high.motes && high.motes < ultra.motes);
    assert!(medium.leaves < high.leaves && high.leaves < ultra.leaves);
    assert!(ultra.birds as usize <= MAX_BIRDS);
    assert_eq!(high.swarm_members % SWARM_MEMBERS, 0, "whole swarms");
}

#[test]
fn off_by_default_draws_nothing() {
    let settings = AmbientLifeSettings::default();
    assert!(!settings.enabled);
    let activity = AmbientActivity::of(&summer_at(0.7));
    let counts = AmbientCounts::of(
        &activity,
        &settings,
        &AmbientBudget::for_quality(QualityTier::Ultra),
    );
    assert!(counts.is_empty(), "{counts:?}");
}

#[test]
fn a_kind_switched_off_draws_nothing_and_the_scale_multiplies() {
    let activity = AmbientActivity::of(&summer_at(0.7));
    let budget = AmbientBudget::for_quality(QualityTier::High);
    let base = AmbientCounts::of(&activity, &on(), &budget);
    let no_motes = AmbientCounts::of(&activity, &AmbientLifeSettings { motes: false, ..on() }, &budget);
    assert!(base.motes > 0.0);
    assert_eq!(no_motes.motes, 0.0);
    assert_eq!(no_motes.leaves, base.leaves);
    let double = AmbientCounts::of(
        &activity,
        &AmbientLifeSettings {
            density_scale: 2.0,
            ..on()
        },
        &budget,
    );
    assert!((double.motes - 2.0 * base.motes).abs() < 1.0e-3);
    assert!(double.birds <= MAX_BIRDS as f32);
}

#[test]
fn fireflies_flash_at_summer_dusk_only() {
    let noon = AmbientActivity::of(&summer_at(0.8));
    let dusk = AmbientActivity::of(&summer_at(-0.08));
    let midnight = AmbientActivity::of(&summer_at(-0.6));
    assert_eq!(noon.fireflies, 0.0);
    assert!(dusk.fireflies > 0.5, "{dusk:?}");
    assert_eq!(midnight.fireflies, 0.0);
    let winter = AmbientActivity::of(&AmbientConditions {
        season: Season::Winter,
        ..summer_at(-0.08)
    });
    assert_eq!(winter.fireflies, 0.0);
    // No butterflies or motes in the dark.
    assert_eq!(dusk.butterflies, 0.0);
    assert_eq!(midnight.motes, 0.0);
}

#[test]
fn rain_and_gales_stop_insects_and_birds_at_the_audio_limits() {
    let dry = AmbientActivity::of(&summer_at(0.7));
    assert!(dry.swarms > 0.5 && dry.butterflies > 0.5 && dry.birds > 0.9 && dry.motes > 0.5);
    let rain = AmbientActivity::of(&AmbientConditions {
        rain_mm_per_h: BIRDS_QUIET_RAIN_MM_PER_H,
        ..summer_at(0.7)
    });
    assert_eq!(rain.birds, 0.0, "birds stop where their calls stop");
    assert_eq!((rain.swarms, rain.butterflies, rain.motes), (0.0, 0.0, 0.0));
    let gale = AmbientActivity::of(&AmbientConditions {
        wind_m_per_s: [BIRDS_QUIET_WIND_M_PER_S, 0.0],
        ..summer_at(0.7)
    });
    assert_eq!(gale.birds, 0.0);
    assert_eq!(gale.swarms, 0.0);
    assert!(gale.leaves > dry.leaves, "wind strips more leaves");
}

#[test]
fn leaves_fall_by_season() {
    let leaves = |season| {
        AmbientActivity::of(&AmbientConditions {
            season,
            ..summer_at(0.7)
        })
        .leaves
    };
    assert!(leaves(Season::Autumn) > 5.0 * leaves(Season::Summer));
    assert!(leaves(Season::Summer) > 0.0 && leaves(Season::Winter) > 0.0);
    assert!(leaves(Season::Winter) < leaves(Season::Spring) + 0.05);
}

#[test]
fn the_season_follows_the_day_and_the_hemisphere() {
    let at = |day, latitude_deg| {
        Season::of_time(&TimeOfDay {
            hour: 12.0,
            day_of_year: day,
            latitude_deg,
        })
    };
    assert_eq!(at(100, 45.0), Season::Spring);
    assert_eq!(at(200, 45.0), Season::Summer);
    assert_eq!(at(300, 45.0), Season::Autumn);
    assert_eq!(at(10, 45.0), Season::Winter);
    assert_eq!(at(360, 45.0), Season::Winter);
    assert_eq!(at(200, -35.0), Season::Winter);
    assert_eq!(at(10, -35.0), Season::Summer);
    for season in Season::ALL {
        assert_eq!(Season::parse(season.name()), Some(season));
    }
    assert_eq!(Season::parse("fall"), Some(Season::Autumn));
    assert_eq!(Season::parse("monsoon"), None);
}

fn inside(sample: &AmbientSample, bx: &AmbientBox, margin: f32) -> bool {
    let p = sample.position_m;
    p[0].abs() <= bx.half_extent_m + margin
        && p[2].abs() <= bx.half_extent_m + margin
        && (p[1] - bx.center_y_m).abs() <= bx.half_height_m + margin
}

#[test]
fn every_creature_lies_in_its_box() {
    for (eye, clock) in [([0.0, 1.7, 0.0], 0.0), ([1234.5, -40.25, -9876.125], 77.7)] {
        let f = frame(eye, clock);
        for i in 0..512 {
            assert!(inside(&mote_at(i, &f), &AmbientBox::MOTES, 0.1), "mote {i}");
            assert!(
                inside(&swarm_member_at(i, &f), &AmbientBox::SWARMS, 1.5),
                "swarm {i}"
            );
            assert!(
                inside(&butterfly_at(i, &f), &AmbientBox::BUTTERFLIES, 4.3),
                "butterfly {i}"
            );
            assert!(
                inside(&firefly_at(i, &f), &AmbientBox::FIREFLIES, 0.7),
                "firefly {i}"
            );
            assert!(inside(&leaf_at(i, &f), &AmbientBox::LEAVES, 0.8), "leaf {i}");
        }
    }
}

/// The boxes wrap around the eye, but a creature does not move with it: as the eye walks, an
/// interior creature keeps its world position (it only wraps at the far edge, faded out).
#[test]
fn a_creature_stays_put_when_the_eye_moves() {
    let a = frame([10.0, 1.7, 20.0], 3.0);
    let b = frame([10.6, 1.7, 19.5], 3.0);
    let mut kept = 0;
    for i in 0..256 {
        let (sa, sb) = (leaf_at(i, &a), leaf_at(i, &b));
        if sa.alpha < 0.5 || sb.alpha < 0.5 {
            continue;
        }
        let wa = [sa.position_m[0] + 10.0, sa.position_m[2] + 20.0];
        let wb = [sb.position_m[0] + 10.6, sb.position_m[2] + 19.5];
        assert!(
            (wa[0] - wb[0]).abs() < 1.0e-3 && (wa[1] - wb[1]).abs() < 1.0e-3,
            "leaf {i}"
        );
        kept += 1;
    }
    assert!(kept > 150, "{kept}");
}

/// Every periodic motion has whole cycles per loop: the step across the wrap is one frame's
/// motion, like the step before it, not a jump.
#[test]
fn the_animation_loop_wraps_seamlessly() {
    let dt = 1.0 / 60.0;
    let two_before = frame([0.0; 3], AMBIENT_LOOP_S - 2.0 * dt);
    let before = frame([0.0; 3], AMBIENT_LOOP_S - dt);
    let after = frame([0.0; 3], 0.0);
    let gap = |a: [f32; 3], b: [f32; 3]| {
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    };
    let smooth = |name: &str, i: u32, at: &dyn Fn(u32, &AmbientFrame) -> AmbientSample| {
        let normal = gap(at(i, &two_before).position_m, at(i, &before).position_m);
        let across = gap(at(i, &before).position_m, at(i, &after).position_m);
        assert!(
            across <= 2.0 * normal + 1.0e-3,
            "{name} {i}: {across} m across the wrap, {normal} m before"
        );
    };
    for i in 0..64 {
        smooth("butterfly", i, &butterfly_at);
        smooth("swarm member", i, &swarm_member_at);
        smooth("mote", i, &mote_at);
        smooth("firefly", i, &firefly_at);
        smooth("leaf", i, &leaf_at);
        let (wa, wb) = (butterfly_wing_angle(i, &before), butterfly_wing_angle(i, &after));
        assert!((wa - wb).abs() < 0.8, "wing {i}: {wa} {wb}");
    }
}

#[test]
fn the_same_inputs_give_the_same_bits_and_the_seed_changes_them() {
    let a = frame([5.0, 2.0, -3.0], 11.0);
    let b = frame([5.0, 2.0, -3.0], 11.0);
    let mut other = a;
    other.seed = 8;
    let mut moved = 0;
    for i in 0..128 {
        assert_eq!(mote_at(i, &a), mote_at(i, &b));
        assert_eq!(leaf_at(i, &a), leaf_at(i, &b));
        assert_eq!(firefly_at(i, &a), firefly_at(i, &b));
        moved += usize::from(leaf_at(i, &a).position_m != leaf_at(i, &other).position_m);
    }
    assert!(moved > 120, "{moved}");
}

#[test]
fn a_swarm_keeps_together_and_flies_are_few() {
    let f = frame([0.0, 1.7, 0.0], 40.0);
    let mut gnat_swarms = 0;
    for swarm in 0..16u32 {
        let center = {
            let mut c = [0.0f32; 3];
            for m in 0..SWARM_MEMBERS {
                let p = swarm_member_at(swarm * SWARM_MEMBERS + m, &f).position_m;
                for axis in 0..3 {
                    c[axis] += p[axis] / SWARM_MEMBERS as f32;
                }
            }
            c
        };
        if swarm_is_flies(swarm, f.seed) {
            let live = (0..SWARM_MEMBERS)
                .filter(|&m| swarm_member_at(swarm * SWARM_MEMBERS + m, &f).alpha > 0.0)
                .count();
            assert!(live <= FLY_SWARM_MEMBERS as usize);
            continue;
        }
        gnat_swarms += 1;
        for m in 0..SWARM_MEMBERS {
            let s = swarm_member_at(swarm * SWARM_MEMBERS + m, &f);
            if s.alpha <= 0.0 {
                continue;
            }
            let d = s
                .position_m
                .iter()
                .zip(center)
                .map(|(p, c)| (p - c).powi(2))
                .sum::<f32>()
                .sqrt();
            assert!(d < 1.6, "swarm {swarm} member {m}: {d} m from the ball");
        }
    }
    assert!(gnat_swarms > 0);
}

#[test]
fn a_firefly_is_mostly_dark_and_flashes_fully() {
    let mut lit = 0;
    let mut peak = 0.0f32;
    let steps = 6_000;
    for step in 0..steps {
        let f = frame([0.0; 3], f64::from(step) * 0.01);
        let s = firefly_at(3, &f);
        peak = peak.max(s.glow);
        lit += usize::from(s.glow > 0.05);
    }
    assert!(peak > 0.9, "{peak}");
    let duty = lit as f32 / steps as f32;
    assert!(duty > 0.01 && duty < 0.2, "{duty}");
}

#[test]
fn leaves_fall_and_the_integrator_wraps() {
    let mut integrator = AmbientIntegrator::default();
    let mut previous: Vec<f32> = Vec::new();
    let mut fell = 0;
    let mut total = 0;
    for step in 0..120 {
        integrator.step([1.0, 0.0], 1.0 / 30.0);
        let f = AmbientFrame::new([0.0, 1.7, 0.0], &integrator, full_counts(), Season::Autumn, 1);
        let ys: Vec<f32> = (0..64).map(|i| leaf_at(i, &f).position_m[1]).collect();
        if step > 0 {
            for (now, before) in ys.iter().zip(&previous) {
                total += 1;
                fell += usize::from(now < before);
            }
        }
        previous = ys;
    }
    assert!(fell * 10 > total * 7, "{fell}/{total} leaf steps went down");
    let lb = AmbientBox::LEAVES.size();
    for (fall, _) in integrator.leaf_fall_m.iter().zip(LEAF_FALL_SPEEDS_M_PER_S) {
        assert!((0.0..f64::from(lb[1])).contains(fall));
    }
    // A bad step leaves it alone.
    let kept = integrator;
    integrator.step([f32::NAN, 0.0], f32::NAN);
    integrator.step([0.0, 0.0], -1.0);
    assert_eq!(integrator, kept);
}

#[test]
fn far_from_the_origin_the_placement_keeps_its_precision() {
    let near = frame([0.25, 1.0, 0.5], 9.0);
    let far = frame([1.0e6 + 0.25, 1.0, -2.0e6 + 0.5], 9.0);
    for i in 0..64 {
        let (a, b) = (mote_at(i, &near), mote_at(i, &far));
        assert!(inside(&b, &AmbientBox::MOTES, 0.1));
        assert!(a.alpha.is_finite() && b.alpha.is_finite());
    }
}

fn flock(seed: u32) -> AmbientBirds {
    AmbientBirds {
        seed,
        ..AmbientBirds::flock()
    }
}

#[test]
fn a_flock_stays_around_its_anchor_at_bird_speed() {
    let anchor = Vec3::new(100.0, 20.0, -50.0);
    let config = flock(3);
    let mut sim = BirdFlockSim::new(anchor, &config);
    let mut lowest = f32::INFINITY;
    for _ in 0..(60 * 60) {
        sim.step(1.0 / 60.0);
        for i in 0..sim.len() {
            let p = sim.position(i).unwrap_or(anchor);
            let r = (p - anchor).truncate_y();
            assert!(r < config.radius_m * 1.6 + 10.0, "bird {i} wandered {r} m");
            lowest = lowest.min(p.y - anchor.y);
            let v = sim.velocity(i).map_or(0.0, Vec3::length);
            assert!(
                v <= config.speed_m_per_s * 1.31 && v >= config.speed_m_per_s * 0.59,
                "{v}"
            );
        }
    }
    assert!(lowest > 0.25 * config.height_m, "{lowest}");
    assert_eq!(sim.steps(), 3600);
}

trait Horizontal {
    fn truncate_y(self) -> f32;
}

impl Horizontal for Vec3 {
    fn truncate_y(self) -> f32 {
        self.x.hypot(self.z)
    }
}

#[test]
fn birds_flap_in_bursts_and_soaring_birds_mostly_glide() {
    let count_flapping = |config: AmbientBirds| {
        let mut sim = BirdFlockSim::new(Vec3::ZERO, &config);
        let mut out = [BirdInstance::default(); MAX_BIRDS];
        let mut moving = 0;
        let mut last = None;
        for _ in 0..1_200 {
            sim.step(1.0 / 60.0);
            sim.write_instances([0.0; 3], 1.0, &mut out);
            let wing = out[0].heading_wing[3];
            if let Some(prev) = last
                && (wing - prev as f32).abs() > 0.02
            {
                moving += 1;
            }
            last = Some(f64::from(wing));
        }
        moving
    };
    let flock_moves = count_flapping(flock(1));
    let soar_moves = count_flapping(AmbientBirds::soaring());
    assert!(flock_moves > 400, "{flock_moves}");
    assert!(soar_moves < flock_moves / 2, "{soar_moves} vs {flock_moves}");
}

#[test]
fn the_flight_is_deterministic_and_seeded() {
    let run = |seed| {
        let mut sim = BirdFlockSim::new(Vec3::new(1.0, 2.0, 3.0), &flock(seed));
        for step in 0..600 {
            // An uneven frame cadence: the fixed step makes it the same flight.
            sim.step(if step % 3 == 0 { 0.02 } else { 0.015 });
        }
        sim
    };
    assert_eq!(run(5), run(5));
    assert_ne!(run(5).position(0), run(6).position(0));
}

#[test]
fn only_visible_birds_are_drawn_and_bound() {
    let mut sim = BirdFlockSim::new(Vec3::ZERO, &flock(2));
    sim.step(0.5);
    let mut out = [BirdInstance::default(); MAX_BIRDS];
    assert_eq!(sim.write_instances([0.0; 3], 3.5, &mut out), 4);
    assert_eq!(out[3].color_alpha[3], 0.5, "the last one fades");
    assert_eq!(sim.write_instances([0.0; 3], 0.0, &mut out), 0);
    let mut small = [BirdInstance::default(); 2];
    assert_eq!(
        sim.write_instances([0.0; 3], 12.0, &mut small),
        2,
        "stops at the slice"
    );
    let far = Vec3::new(0.0, 500.0, 0.0);
    let (index, _) = sim.nearest_visible(far, 2.0).expect("a visible bird");
    assert!(index < 2);
    assert!(sim.nearest_visible(far, 0.0).is_none());
    sim.set_visible(f32::NAN);
    assert_eq!(sim.visible(), 0.0);
    sim.set_visible(1.0e9);
    assert_eq!(sim.visible(), sim.len() as f32);
}

#[test]
fn the_state_moves_birds_to_the_eye_and_an_inactive_state_draws_nothing() {
    let mut state = AmbientState {
        active: true,
        counts: full_counts(),
        bird_count: 1,
        ..AmbientState::default()
    };
    state.birds[0].position_size = [100.0, 50.0, -20.0, 0.3];
    let frame = state.frame_at([90.0, 48.0, -25.0]);
    assert_eq!(frame.bird_count, 1);
    assert_eq!(frame.birds[0].position_size, [10.0, 2.0, 5.0, 0.3]);
    state.active = false;
    assert!(state.frame_at([0.0; 3]).is_empty());
}

#[test]
fn presets_and_names() {
    for name in BIRD_PRESETS {
        assert!(AmbientBirds::named(name).is_some(), "{name}");
    }
    assert_eq!(
        AmbientBirds::named("single").map(|b| b.mode),
        Some(BirdMode::Soaring)
    );
    assert!(AmbientBirds::named("dragon").is_none());
    assert!(AmbientBirds::flock().call_reach_m() > AmbientBirds::flock().radius_m);
    // Out-of-range configurations are clamped, not trusted.
    let wild = AmbientBirds {
        count: 10_000,
        radius_m: f32::NAN,
        speed_m_per_s: -4.0,
        ..AmbientBirds::flock()
    };
    let sim = BirdFlockSim::new(Vec3::ZERO, &wild);
    assert_eq!(sim.len(), MAX_BIRDS);
    assert!(sim.config().radius_m.is_finite() && sim.config().speed_m_per_s >= 1.0);
}

#[test]
fn the_shader_takes_the_rust_constants() {
    let (_, text) = ambient_shader_source();
    assert!(
        text.contains("const MOTE_BOX: vec3<f32> = vec3<f32>(7.0, 3.5, 0.0);"),
        "{}",
        &text[..400]
    );
    assert!(text.contains("const LEAF_BOX: vec3<f32> = vec3<f32>(14.0, 9.0, 4.0);"));
    assert!(text.contains("const LOOP_S: f32 = 256.0;"));
    assert_eq!(text.matches("@vertex").count(), 6);
}

/// The first creature fades in with the count, and a full count leaves it whole.
#[test]
fn the_first_creature_fades_in_too() {
    assert_eq!(count_fade(0, 0.0), 0.0);
    assert!(count_fade(0, 0.01) <= 0.01, "{}", count_fade(0, 0.01));
    assert!(count_fade(1, 1.05) <= 0.05, "{}", count_fade(1, 1.05));
    assert!((count_fade(0, 0.5) - 0.5).abs() < 1.0e-6);
    assert_eq!(count_fade(0, 100.0), 1.0);
    assert_eq!(count_fade(50, 100.0), 1.0);
    assert_eq!(count_fade(100, 100.0), 0.0);
}

/// The sun is out of the creatures' light by the elevation the renderer stops giving a view along
/// it, so the shade cannot vanish in one frame just before sunset.
#[test]
fn the_sun_is_out_before_the_shade_view_goes() {
    use super::gpu::sun_through_cloud;
    assert_eq!(
        sun_through_cloud(0.02, 0.0),
        0.0,
        "no view along the sun from here down"
    );
    assert_eq!(sun_through_cloud(-0.2, 0.0), 0.0);
    assert_eq!(
        sun_through_cloud(0.17, 0.0),
        1.0,
        "a golden hour's 10° is untouched"
    );
    assert!((sun_through_cloud(1.0, 1.0) - 0.2).abs() < 1.0e-6, "overcast");
    let mut last = 0.0;
    for step in 0..=50 {
        let light = sun_through_cloud(0.02 + 0.001 * step as f32, 0.0);
        assert!(light >= last && light - last < 0.05, "smooth: {last} to {light}");
        last = light;
    }
}
