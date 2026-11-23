// SPDX-License-Identifier: MPL-2.0
use super::*;
use crate::decal_strip::DecalStrip;

const DT: f32 = 1.0 / 120.0;

fn sample(key: u64, contact: Vec3, heading: Vec3, roll: f32, surface: WheelSurface) -> WheelSample {
    WheelSample {
        key,
        grounded: true,
        contact,
        normal: Vec3::Y,
        heading,
        roll_speed: roll,
        surface,
        half_width: 0.12,
    }
}

fn config(tier: QualityTier) -> VehicleFxConfig {
    VehicleFxConfig {
        limits: VehicleFxLimits::for_tier(tier),
        ..VehicleFxConfig::default()
    }
}

/// Everything a run produced: stamp centres in order, the peak smoke rate, outputs, retirements.
#[derive(Default)]
struct Trace {
    stamps: Vec<Vec3>,
    peak_smoke_rate: f32,
    peak_live: usize,
    retired: Vec<u64>,
    last: WheelOutput,
    mean_dust_rate: f32,
}

fn run(fx: &mut VehicleFx, steps: usize, mut at: impl FnMut(usize) -> WheelSample) -> Trace {
    let mut t = Trace::default();
    let mut out = Vec::new();
    let mut dust = 0.0;
    for i in 0..steps {
        let s = at(i);
        fx.step(DT, &[s], &mut out);
        t.stamps.extend(fx.new_stamps().iter().map(|n| n.decal.position));
        t.retired.extend_from_slice(fx.retired());
        t.peak_smoke_rate = t.peak_smoke_rate.max(out[0].smoke_rate);
        t.peak_live = t.peak_live.max(fx.stats().live);
        dust += out[0].dust_rate;
        t.last = out[0];
    }
    t.mean_dust_rate = dust / steps.max(1) as f32;
    t
}

#[test]
fn a_straight_cruise_on_asphalt_makes_no_smoke_and_no_skid_stamps() {
    let mut fx = VehicleFx::new(config(QualityTier::High));
    let v = 20.0;
    let t = run(&mut fx, 600, |i| {
        sample(
            1,
            Vec3::new(v * DT * i as f32, 0.0, 0.0),
            Vec3::X,
            v,
            WheelSurface::Asphalt,
        )
    });
    assert_eq!(t.peak_smoke_rate, 0.0);
    assert!(t.stamps.is_empty());
    assert_eq!(fx.stats().emitted, 0);
    assert!(t.last.slip < 0.01, "slip {}", t.last.slip);
}

#[test]
fn a_locked_wheel_stop_from_20_ms_leaves_a_strip_the_length_of_the_stop_and_smokes() {
    let mut fx = VehicleFx::new(config(QualityTier::High));
    let decel = 8.0;
    let stop = 20.0 * 20.0 / (2.0 * decel);
    let steps = (20.0 / decel / DT) as usize + 1;
    let t = run(&mut fx, steps, |i| {
        let time = (i as f32 * DT).min(20.0 / decel);
        let x = 20.0 * time - 0.5 * decel * time * time;
        sample(1, Vec3::new(x, 0.0, 0.0), Vec3::X, 0.0, WheelSurface::Asphalt)
    });
    assert!(t.peak_smoke_rate > 20.0, "smoke {}", t.peak_smoke_rate);
    let first = t.stamps.first().expect("stamps");
    let last = t.stamps.last().expect("stamps");
    let length = last.x - first.x;
    assert!(
        (length - stop).abs() <= stop * 0.1,
        "strip {length} m against a {stop} m stop"
    );
    // Sampled at the spacing, the stamp count follows from the length.
    let spacing = fx.config().spacing;
    assert!((t.stamps.len() as f32 - length / spacing - 1.0).abs() <= 1.0);
}

#[test]
fn a_constant_slip_circle_on_low_never_passes_the_cap_and_retires_the_oldest_first() {
    let cfg = config(QualityTier::Low);
    let cap = cfg.limits.max_stamps;
    assert_eq!(cap, 48);
    let mut fx = VehicleFx::new(cfg);
    let (radius, speed) = (20.0f32, 15.0f32);
    let omega = speed / radius;
    let t = run(&mut fx, 120 * 30, |i| {
        let a = omega * DT * i as f32;
        let pos = Vec3::new(radius * a.sin(), 0.0, radius * (1.0 - a.cos()));
        let tangent = Vec3::new(a.cos(), 0.0, a.sin());
        // The tyre points 30 degrees off the path: a steady slip angle.
        let heading = Vec3::new((a + 0.52).cos(), 0.0, (a + 0.52).sin());
        let roll = speed * tangent.dot(heading);
        sample(1, pos, heading, roll, WheelSurface::Asphalt)
    });
    assert!(t.peak_live <= cap, "live {} over cap {cap}", t.peak_live);
    assert_eq!(t.peak_live, cap);
    let stats = fx.stats();
    assert!(stats.emitted as usize > cap * 5, "emitted {}", stats.emitted);
    assert_eq!(stats.retired_by_cap, stats.emitted - cap as u64);
    // The oldest go first: retirements are serials 0, 1, 2, ... in order, without a gap.
    for (n, serial) in t.retired.iter().enumerate() {
        assert_eq!(*serial, n as u64);
    }
}

#[test]
fn on_gravel_the_dust_rate_rises_with_speed_and_ruts_have_no_gap_over_1_5_spacing() {
    let mut rates = Vec::new();
    for speed in [3.0f32, 6.0, 12.0, 20.0] {
        let mut fx = VehicleFx::new(config(QualityTier::High));
        let t = run(&mut fx, 240, |i| {
            sample(
                1,
                Vec3::new(speed * DT * i as f32, 0.0, 0.0),
                Vec3::X,
                speed,
                WheelSurface::Gravel,
            )
        });
        rates.push(t.mean_dust_rate);
        assert!(fx.stats().emitted > 0, "gravel makes ruts");
    }
    assert!(rates.windows(2).all(|w| w[1] > w[0]), "dust rates {rates:?}");
    assert!(rates[0] > 0.0);

    // A curved rut path: gaps between consecutive stamp centres stay within 1.5 spacing.
    let mut fx = VehicleFx::new(config(QualityTier::Ultra));
    let spacing = fx.config().spacing;
    let (radius, speed) = (9.0f32, 8.0f32);
    let t = run(&mut fx, 120 * 12, |i| {
        let a = speed / radius * DT * i as f32;
        let pos = Vec3::new(radius * a.sin(), 0.0, radius * (1.0 - a.cos()));
        sample(
            1,
            pos,
            Vec3::new(a.cos(), 0.0, a.sin()),
            speed,
            WheelSurface::Gravel,
        )
    });
    assert!(t.stamps.len() > 100);
    let worst = t
        .stamps
        .windows(2)
        .map(|w| w[0].distance(w[1]))
        .fold(0.0, f32::max);
    assert!(worst <= spacing * 1.5, "gap {worst} against spacing {spacing}");
}

#[test]
fn a_straight_100_m_run_at_spacing_0_5_lays_200_stamps_plus_or_minus_1() {
    let mut fx = VehicleFx::new(config(QualityTier::Ultra));
    let speed = 6.0;
    let steps = (100.0 / (speed * DT)) as usize;
    let t = run(&mut fx, steps + 1, |i| {
        sample(
            1,
            Vec3::new(speed * DT * i as f32, 0.0, 0.0),
            Vec3::X,
            speed,
            WheelSurface::Dirt,
        )
    });
    let n = t.stamps.len() as i64;
    assert!((n - 200).abs() <= 1, "{n} stamps");
}

#[test]
fn two_runs_are_identical() {
    let go = || {
        let mut fx = VehicleFx::new(config(QualityTier::Medium));
        let t = run(&mut fx, 1500, |i| {
            let a = 0.01 * i as f32 * DT * 20.0;
            sample(
                7,
                Vec3::new(12.0 * a.sin(), 0.0, 12.0 * (1.0 - a.cos())),
                Vec3::new((a + 0.4).cos(), 0.0, (a + 0.4).sin()),
                9.0,
                WheelSurface::Asphalt,
            )
        });
        (fx.state_digest(), t.stamps, t.retired)
    };
    let a = go();
    let b = go();
    assert_eq!(a.0, b.0);
    assert_eq!(a.1, b.1);
    assert_eq!(a.2, b.2);
    assert!(!a.1.is_empty());
}

#[test]
fn stamps_match_the_decal_strip_layout() {
    let mut fx = VehicleFx::new(config(QualityTier::Ultra));
    let speed = 6.0;
    let mut stamps = Vec::new();
    let mut out = Vec::new();
    for i in 0..=100 {
        let s = sample(
            1,
            Vec3::new(speed * DT * i as f32, 0.0, 0.0),
            Vec3::X,
            speed,
            WheelSurface::Dirt,
        );
        fx.step(DT, &[s], &mut out);
        stamps.extend_from_slice(fx.new_stamps());
    }
    // The first stamp waits for a velocity (one step), so the strip starts there.
    let start = stamps[0].decal.position;
    let end = Vec3::new(speed * DT * 100.0, 0.0, 0.0);
    let strip = DecalStrip {
        points: vec![start, end],
        normal: Vec3::Y,
        half_width: 0.12,
        half_depth: fx.config().half_depth,
        spacing: fx.config().spacing,
        surface: stamps[0].surface,
        lifetime: stamps[0].decal.lifetime,
    };
    let reference = strip.stamps();
    assert!(stamps.len() >= reference.len() - 1);
    for (mine, (theirs, _)) in stamps.iter().zip(reference.iter()) {
        assert!(
            mine.decal.position.distance(theirs.position) < 1e-3,
            "{:?} against {:?}",
            mine.decal.position,
            theirs.position
        );
        assert_eq!(mine.decal.half_size.y, theirs.half_size.y);
        assert!(mine.decal.rotation.dot(theirs.rotation).abs() > 0.9999);
    }
}

#[test]
fn a_run_ends_in_the_air_and_the_next_stamp_starts_a_new_run() {
    let mut fx = VehicleFx::new(config(QualityTier::High));
    let mut out = Vec::new();
    for i in 0..60 {
        fx.step(
            DT,
            &[sample(
                1,
                Vec3::new(0.1 * i as f32, 0.0, 0.0),
                Vec3::X,
                12.0,
                WheelSurface::Dirt,
            )],
            &mut out,
        );
    }
    let before = fx.stats().emitted;
    let mut air = sample(1, Vec3::new(50.0, 1.0, 0.0), Vec3::X, 12.0, WheelSurface::Dirt);
    air.grounded = false;
    fx.step(DT, &[air], &mut out);
    assert_eq!(out[0].mark, MarkKind::None);
    // Landing 40 m on: a fresh stamp at the landing, not a 40 m smear.
    fx.step(
        DT,
        &[sample(
            1,
            Vec3::new(90.0, 0.0, 0.0),
            Vec3::X,
            12.0,
            WheelSurface::Dirt,
        )],
        &mut out,
    );
    // The first grounded step has no velocity yet; the next one starts the run.
    assert_eq!(fx.stats().emitted, before);
    fx.step(
        DT,
        &[sample(
            1,
            Vec3::new(90.1, 0.0, 0.0),
            Vec3::X,
            12.0,
            WheelSurface::Dirt,
        )],
        &mut out,
    );
    assert_eq!(fx.stats().emitted, before + 1);
}

#[test]
fn stamps_retire_by_age_and_clear_retires_the_rest() {
    let mut cfg = config(QualityTier::High);
    cfg.mark_lifetime_s = 1.0;
    let mut fx = VehicleFx::new(cfg);
    let mut out = Vec::new();
    for i in 0..360 {
        fx.step(
            DT,
            &[sample(
                1,
                Vec3::new(0.1 * i as f32, 0.0, 0.0),
                Vec3::X,
                12.0,
                WheelSurface::Dirt,
            )],
            &mut out,
        );
    }
    let stats = fx.stats();
    assert!(stats.retired > 0 && stats.retired_by_cap == 0);
    assert!(stats.live > 0);
    fx.clear();
    assert_eq!(fx.stats().live, 0);
    assert_eq!(fx.retired().len() as u64, stats.live as u64);
}

#[test]
fn a_non_finite_sample_is_ignored() {
    let mut fx = VehicleFx::new(config(QualityTier::High));
    let mut out = Vec::new();
    let mut bad = sample(1, Vec3::new(f32::NAN, 0.0, 0.0), Vec3::X, 1.0, WheelSurface::Dirt);
    fx.step(DT, &[bad], &mut out);
    bad.contact = Vec3::ZERO;
    bad.roll_speed = f32::INFINITY;
    fx.step(f32::NAN, &[bad], &mut out);
    assert_eq!(out[0].mark, MarkKind::None);
    assert_eq!(fx.stats().emitted, 0);
}

#[test]
fn surfaces_map_from_names_and_each_has_a_consistent_effect_set() {
    assert_eq!(
        WheelSurface::from_material_name("Rough_Gravel_01"),
        Some(WheelSurface::Gravel)
    );
    assert_eq!(
        WheelSurface::from_material_name("road"),
        Some(WheelSurface::Asphalt)
    );
    assert_eq!(WheelSurface::from_material_name("steel"), None);
    for s in WheelSurface::ALL {
        assert_eq!(WheelSurface::from_name(s.name()), Some(s));
        let fx = s.fx();
        // Smoke belongs to skid surfaces, dust and spray to the soft ones.
        assert_eq!(fx.smoke, fx.mark == MarkKind::Skid, "{s:?}");
        assert!(fx.mark == MarkKind::None || fx.mark_opacity > 0.0);
    }
    assert_eq!(WheelSurface::Water.fx().mark, MarkKind::None);
}

#[test]
fn tier_limits_grow_with_the_tier() {
    use QualityTier::*;
    let caps: Vec<usize> = [MobileLow, MobileHigh, Low, Medium, High, Ultra]
        .into_iter()
        .map(|t| VehicleFxLimits::for_tier(t).max_stamps)
        .collect();
    assert!(caps.windows(2).all(|w| w[0] <= w[1]), "{caps:?}");
    let mut cfg = config(QualityTier::Ultra);
    cfg.limits.max_stamps = usize::MAX;
    assert_eq!(VehicleFx::new(cfg).config().limits.max_stamps, MAX_STAMP_CAP);
}

#[test]
fn the_built_in_effects_validate_and_register() {
    for name in [TIRE_SMOKE, WHEEL_DUST, WHEEL_SPRAY] {
        let def = vehicle_effect(name).expect("built in");
        assert_eq!(def.name, name);
        def.validate().expect("valid");
        let mut scene = crate::scene::VfxScene::new(crate::scene::VfxLibrary::new());
        assert!(ensure_vehicle_effect(&mut scene, name));
        assert!(scene.library().index(name).is_some());
    }
    assert!(vehicle_effect("explosion").is_none());
}
