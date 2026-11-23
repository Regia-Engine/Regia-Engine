// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep (sprint-test-quality-sweep, ADR-2077): deterministic edge-case tests for
//! the least-covered public functions of `regia_vfx` (crash scaling and sequencing, entry
//! physics and colours, weather blending, exposure, atlas tiles, decal alignment, camera shake,
//! curve baking, metrics text, glass breaches). A test that failed on the old code is the
//! regression test for the fix named next to it in `src`.

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_render::CloudGenus;
use regia_vfx::atlas::{FIRE_ANIM_TILE, FIREBALL_ANIM_TILE, PUFF_ANIM_TILE, tile_index};
use regia_vfx::crash::{
    CrashBurst, CrashEventKind, CrashScale, CrashSequence, CrashTier, FireSize, GroundClass,
    SHOCK_FRONT_SPEED, advance_crash_sequence, blast_arrival_time, ground_frame,
};
use regia_vfx::curves::{CURVE_SAMPLES, CurveLut};
use regia_vfx::decal::{DecalAnchor, align_along};
use regia_vfx::entry::{
    EntryTrack, TrackPoint, air_density, head_temperature_k, heat_color, heating_flux, temperature_color,
};
use regia_vfx::events::scaled_count;
use regia_vfx::format::{
    AttractorDef, ColorKey, DepthCollisionDef, EventSpawnDef, EventTrigger, FlipbookDef, FlipbookMode,
    FloatKey, LodDef, ParticleLightDef, RangeDef, ShapeDef, SpawnMode, SpriteId, VFX_FORMAT_VERSION,
    VelocityMode, VfxSystemDef, VortexDef,
};
use regia_vfx::glass::{BreachAtmosphere, GlassEffectSettings};
use regia_vfx::measure::{EffectMetrics, metrics_line};
use regia_vfx::quality::{EXPOSURE_FLOOR, apply_exposure, spherical_normal};
use regia_vfx::scene::VfxLibrary;
use regia_vfx::{CameraShake, CameraShakeRequest, VfxScene, WeatherState};

fn approx(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol * b.abs().max(1.0)
}

/// Values that hostile callers pass: zero, signed zero, tiny, huge, infinities and NaN.
const HOSTILE: [f32; 9] = [
    0.0,
    -0.0,
    1e-30,
    -1.0,
    1e30,
    -1e30,
    f32::INFINITY,
    f32::NEG_INFINITY,
    f32::NAN,
];

// ---------------------------------------------------------------------------------------------
// crash.rs: GroundClass

#[test]
fn ground_class_from_albedo_picks_snow_grass_mud_and_survives_nan() {
    assert_eq!(GroundClass::from_albedo([0.8, 0.8, 0.82]), GroundClass::Snow);
    assert_eq!(GroundClass::from_albedo([0.2, 0.35, 0.1]), GroundClass::Grass);
    assert_eq!(GroundClass::from_albedo([0.25, 0.18, 0.12]), GroundClass::Mud);
    // Black has no chroma and no luma: mud, not snow.
    assert_eq!(GroundClass::from_albedo([0.0; 3]), GroundClass::Mud);
    for v in HOSTILE {
        // Any class is fine; it must not panic and must be one of the three.
        let c = GroundClass::from_albedo([v, v, v]);
        assert!(GroundClass::ALL.contains(&c));
    }
}

#[test]
fn ground_class_effect_names_carry_class_and_scale() {
    let s = CrashScale::SHOWCASE;
    assert_eq!(GroundClass::Mud.dust_effect(s), "crash_dust_mud@250t");
    assert_eq!(GroundClass::Snow.ejecta_effect(s), "crash_ejecta_snow@250t");
    let mut contacts: Vec<&str> = GroundClass::ALL.iter().map(|c| c.contact_effect()).collect();
    for (c, name) in GroundClass::ALL.iter().zip(&contacts) {
        assert!(name.ends_with(c.name()), "{name} names {}", c.name());
        assert_eq!(GroundClass::from_name(c.name()), Some(*c));
        // The scale tag of the effect name parses back.
        assert_eq!(CrashScale::from_effect_name(&c.dust_effect(s)), Some(s));
        assert_eq!(
            CrashScale::from_effect_name(&c.ejecta_effect(CrashScale::VEHICLE)),
            Some(CrashScale::VEHICLE)
        );
    }
    contacts.sort_unstable();
    contacts.dedup();
    assert_eq!(contacts.len(), 3);
}

// ---------------------------------------------------------------------------------------------
// crash.rs: CrashScale

#[test]
fn crash_scale_is_idempotent_and_its_tag_round_trips_over_the_whole_range() {
    let mut t = 0.01_f32;
    let mut checked = 0;
    while t < 2_000.0 {
        let s = CrashScale::from_tonnes(t);
        assert!(
            (CrashScale::MIN_TONNES..=CrashScale::MAX_TONNES).contains(&s.tonnes()),
            "{t} -> {}",
            s.tonnes()
        );
        assert_eq!(
            CrashScale::from_tonnes(s.tonnes()),
            s,
            "from_tonnes is not idempotent at {t}"
        );
        let name = format!("crash_fireball@{}", s.tag());
        assert_eq!(CrashScale::from_effect_name(&name), Some(s), "{name}");
        // Three significant digits at most.
        let digits: String = s.tag().chars().filter(char::is_ascii_digit).collect();
        assert!(
            digits.trim_start_matches('0').trim_end_matches('0').len() <= 3,
            "{}",
            s.tag()
        );
        t *= 1.013;
        checked += 1;
    }
    assert!(checked > 500);
    for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(CrashScale::from_tonnes(v), CrashScale::VEHICLE);
    }
    assert_eq!(CrashScale::from_tonnes(-5.0).tonnes(), CrashScale::MIN_TONNES);
    assert_eq!(CrashScale::from_tonnes(1e9).tonnes(), CrashScale::MAX_TONNES);
}

#[test]
fn crash_scale_rejects_non_canonical_tags() {
    for name in [
        "crash_fireball@250.0t",
        "crash_fireball@2.5e2t",
        "crash_fireball@inft",
        "crash_fireball@NaNt",
        "crash_fireball@250",
        "crash_fireball@t",
        "crash_fireball@-1t",
        "crash_fireball@251.5t",
        "crash_fireball",
        "",
    ] {
        assert_eq!(CrashScale::from_effect_name(name), None, "{name}");
    }
    // The last `@` names the tag.
    assert_eq!(
        CrashScale::from_effect_name("a@mars@250t"),
        Some(CrashScale::SHOWCASE)
    );
}

#[test]
fn crash_scale_laws_agree_with_each_other_and_the_documented_showcase_numbers() {
    const G: f32 = 9.81;
    let s = CrashScale::SHOWCASE;
    // √(R/g)·√(gR) = R and τ²g = R.
    assert!(approx(
        s.time_scale() * s.speed_scale(),
        s.fireball_radius(),
        1e-4
    ));
    assert!(approx(s.time_scale().powi(2) * G, s.fireball_radius(), 2e-2));
    // The module docs' 250 t numbers: 2 km ring, 400 m lift, 200 m shell, 1.1 km column, 300 m fires.
    assert!(approx(s.blast_reach(), 2_000.0, 0.02), "{}", s.blast_reach());
    assert!(approx(s.lift_reach(), 400.0, 0.02), "{}", s.lift_reach());
    assert!(approx(s.shell_radius(), 200.0, 0.02), "{}", s.shell_radius());
    assert!(approx(s.column_height(), 1_100.0, 0.03), "{}", s.column_height());
    assert!(approx(s.fire_zone(), 300.0, 0.02), "{}", s.fire_zone());
    // Every length grows with the yield.
    let small = CrashScale::VEHICLE;
    let big = CrashScale::LARGEST;
    for (name, f) in [
        ("blast_reach", CrashScale::blast_reach as fn(CrashScale) -> f32),
        ("lift_reach", CrashScale::lift_reach),
        ("shell_radius", CrashScale::shell_radius),
        ("column_height", CrashScale::column_height),
        ("fire_zone", CrashScale::fire_zone),
        ("time_scale", CrashScale::time_scale),
        ("speed_scale", CrashScale::speed_scale),
    ] {
        assert!(f(small) < f(s) && f(s) < f(big), "{name} is not monotonic");
    }
    // Ejecta launch envelope.
    let e = s.ejecta_launch();
    assert!(e.min_speed > 0.0 && e.min_speed < e.max_speed);
    assert!(approx(e.min_speed / s.speed_scale(), 1.2, 1e-4));
    // Blast shake: at most 1, never increasing with distance, finite for any distance.
    let mut last = f32::INFINITY;
    for d in [0.0, 1.0, 10.0, 100.0, 1_000.0, 10_000.0, 1e7] {
        let k = s.blast_shake(d);
        assert!((0.0..=1.0).contains(&k) && k <= last, "{d}: {k}");
        last = k;
    }
    for d in HOSTILE {
        let k = s.blast_shake(d);
        assert!(
            k.is_finite() && (0.0..=1.0).contains(&k),
            "blast_shake({d}) = {k}"
        );
    }
}

#[test]
fn fire_sizes_burn_longer_when_bigger() {
    let secs: Vec<f32> = FireSize::ALL.iter().map(|s| s.burn_seconds()).collect();
    assert!(secs.windows(2).all(|w| w[0] < w[1]), "{secs:?}");
    assert!(secs.iter().all(|s| *s > 0.0 && s.is_finite()));
    for s in FireSize::ALL {
        assert!(s.effect().ends_with(s.name()));
    }
}

// ---------------------------------------------------------------------------------------------
// crash.rs: ground_frame, blast_arrival_time, CrashSequence

fn assert_orthonormal_frame(normal: Vec3, n: Vec3) {
    let (t, b) = ground_frame(normal);
    assert!(t.is_finite() && b.is_finite(), "{normal}: {t} {b}");
    assert!((t.length() - 1.0).abs() < 1e-3, "{normal}: |t| = {}", t.length());
    assert!((b.length() - 1.0).abs() < 1e-3, "{normal}: |b| = {}", b.length());
    assert!(
        t.dot(n).abs() < 1e-3 && b.dot(n).abs() < 1e-3 && t.dot(b).abs() < 1e-3,
        "{normal}"
    );
}

#[test]
fn ground_frame_is_orthonormal_for_every_normal_and_y_up_for_degenerate_ones() {
    assert_eq!(ground_frame(Vec3::Y), (Vec3::X, Vec3::Z));
    let mut normals = vec![
        Vec3::X,
        -Vec3::X,
        Vec3::Y,
        -Vec3::Y,
        Vec3::Z,
        -Vec3::Z,
        Vec3::new(1.0, 1e-4, 0.0),
    ];
    for i in 0..64 {
        let a = i as f32 * 0.37;
        normals.push(Vec3::new(a.cos(), (a * 1.7).sin(), (a * 0.3).cos() - 0.5) * (1.0 + i as f32));
    }
    for n in normals {
        assert_orthonormal_frame(n, n.normalize());
    }
    // Zero, NaN and infinite normals fall back to the +Y frame.
    for n in [Vec3::ZERO, Vec3::NAN, Vec3::new(f32::INFINITY, 0.0, 0.0)] {
        assert_eq!(ground_frame(n), (Vec3::X, Vec3::Z), "{n}");
    }
}

#[test]
fn blast_arrival_time_is_distance_over_the_front_speed_and_symmetric() {
    let a = Vec3::new(10.0, 0.0, -5.0);
    let b = Vec3::new(-300.0, 40.0, 1_000.0);
    assert!(approx(
        blast_arrival_time(a, b) * SHOCK_FRONT_SPEED,
        a.distance(b),
        1e-5
    ));
    assert_eq!(blast_arrival_time(a, b), blast_arrival_time(b, a));
    assert_eq!(blast_arrival_time(a, a), 0.0);
}

#[test]
fn crash_sequence_flags_follow_the_clock_and_the_front_reaches_the_listener_on_time() {
    let crash = CrashBurst {
        contact_lead: 0.5,
        ..CrashBurst::default()
    };
    let mut scene = VfxScene::new(VfxLibrary::new());
    let mut seq = CrashSequence::new(crash, 1.0);
    let listener = Vec3::new(0.0, 0.0, 1.5 * SHOCK_FRONT_SPEED);
    let arrive_at = 1.0 + blast_arrival_time(crash.position, listener);
    let dt = 0.25_f32;
    let mut contact_at = None;
    let mut detonated_at = None;
    let mut arrived_at = None;
    assert!(!seq.contact_fired() && !seq.detonated() && !seq.arrived());
    assert_eq!(seq.front_radius(), 0.0);
    for step in 0..20 {
        let now = step as f32 * dt;
        let before_radius = seq.front_radius();
        let events = advance_crash_sequence(&mut scene, &mut seq, dt, CrashTier::Lite, Some(listener));
        for e in &events {
            match e {
                CrashEventKind::Contact => contact_at = Some(now),
                CrashEventKind::Detonation => detonated_at = Some(now),
                CrashEventKind::BlastArrived { distance } => {
                    assert!(approx(*distance, listener.length(), 1e-5));
                    arrived_at = Some(now);
                }
                CrashEventKind::Crater(c) => assert_eq!(*c, crash.scale.crater()),
            }
        }
        // The flags never go back, the front never shrinks.
        assert!(seq.front_radius() >= before_radius);
        if contact_at.is_some() {
            assert!(seq.contact_fired());
        }
        if detonated_at.is_some() {
            assert!(seq.detonated());
        }
        if arrived_at.is_some() {
            assert!(seq.arrived());
        }
    }
    assert_eq!(contact_at, Some(0.5));
    assert_eq!(detonated_at, Some(1.0));
    assert_eq!(arrived_at, Some(arrive_at));
    assert!(!seq.instances().is_empty());
    // Ten steps after the detonation (4 s at 0.25 s), less the stepped-over frame.
    assert!(
        approx(seq.front_radius(), (5.0 - 1.0) * SHOCK_FRONT_SPEED, 1e-4),
        "{}",
        seq.front_radius()
    );
}

#[test]
fn crash_sequence_ignores_hostile_times() {
    for impact in HOSTILE {
        let seq = CrashSequence::new(CrashBurst::default(), impact);
        assert!(
            seq.impact_in.is_finite() && seq.impact_in >= 0.0,
            "{impact} -> {}",
            seq.impact_in
        );
    }
    let mut scene = VfxScene::new(VfxLibrary::new());
    let mut seq = CrashSequence::new(
        CrashBurst {
            contact: false,
            ..CrashBurst::default()
        },
        10.0,
    );
    for dt in [f32::NAN, f32::INFINITY, -1.0, 0.0, -0.0] {
        let events = advance_crash_sequence(&mut scene, &mut seq, dt, CrashTier::Lite, None);
        assert!(events.is_empty());
        assert_eq!(seq.clock(), 0.0, "dt {dt} moved the clock");
    }
    assert!(!seq.contact_fired() && !seq.detonated());
}

// ---------------------------------------------------------------------------------------------
// entry.rs

#[test]
fn air_density_falls_with_altitude_and_stays_finite() {
    assert!(approx(air_density(0.0), 1.225, 1e-6));
    assert_eq!(air_density(-500.0), air_density(0.0));
    let mut last = f32::INFINITY;
    for km in 0..120 {
        let rho = air_density(km as f32 * 1_000.0);
        assert!(rho > 0.0 && rho < last, "{km} km");
        last = rho;
    }
    for v in HOSTILE {
        let rho = air_density(v);
        assert!(rho.is_finite() && rho >= 0.0, "air_density({v}) = {rho}");
    }
    assert_eq!(air_density(f32::INFINITY), 0.0);
}

#[test]
fn heating_flux_is_rho_v_cubed_and_never_negative() {
    // Normalised to about 1 at 40 km and 11 km/s (the reference density is rounded; 2 %).
    assert!(
        approx(heating_flux(40_000.0, 11_000.0), 1.0, 0.02),
        "{}",
        heating_flux(40_000.0, 11_000.0)
    );
    let q1 = heating_flux(50_000.0, 5_000.0);
    let q2 = heating_flux(50_000.0, 10_000.0);
    assert!(approx(q2 / q1, 8.0, 1e-4));
    assert_eq!(heating_flux(30_000.0, -7_000.0), 0.0);
    for a in HOSTILE {
        for v in HOSTILE {
            let q = heating_flux(a, v);
            assert!(q >= 0.0 || q.is_nan(), "heating_flux({a}, {v}) = {q}");
        }
    }
    assert_eq!(heating_flux(30_000.0, f32::NAN), 0.0);
}

fn track() -> EntryTrack {
    let p = |time: f32, x: f32, y: f32| TrackPoint {
        time,
        position: Vec3::new(x, y, 0.0),
    };
    EntryTrack::new(vec![
        p(0.0, 0.0, 1_000.0),
        p(2.0, 200.0, 600.0),
        p(3.0, 200.0, 0.0),
    ])
    .expect("valid track")
}

#[test]
fn entry_track_velocity_is_the_segment_slope_and_held_at_the_ends() {
    let t = track();
    let v0 = Vec3::new(100.0, -200.0, 0.0);
    let v1 = Vec3::new(0.0, -600.0, 0.0);
    assert_eq!(t.velocity_at(1.0), v0);
    assert_eq!(t.velocity_at(-50.0), v0);
    assert_eq!(t.velocity_at(2.5), v1);
    assert_eq!(t.velocity_at(99.0), v1);
    // A knot belongs to the segment that starts there; the end belongs to the last segment.
    assert_eq!(t.velocity_at(2.0), v1);
    assert_eq!(t.velocity_at(3.0), v1);
    for time in HOSTILE {
        assert!(t.velocity_at(time).is_finite(), "velocity_at({time})");
    }
}

#[test]
fn entry_track_position_is_finite_for_hostile_times() {
    let t = track();
    for time in HOSTILE {
        let p = t.position_at(time);
        assert!(p.is_finite(), "position_at({time}) = {p}");
    }
    assert_eq!(t.position_at(f32::NEG_INFINITY), Vec3::new(0.0, 1_000.0, 0.0));
    assert_eq!(t.position_at(f32::INFINITY), Vec3::new(200.0, 0.0, 0.0));
}

#[test]
fn entry_colors_stay_in_the_unit_cube_for_any_input() {
    let mut inputs: Vec<f32> = HOSTILE.to_vec();
    inputs.extend((0..200).map(|i| i as f32 * 75.0));
    for x in inputs {
        for (what, c) in [
            ("heat_color", heat_color(x)),
            ("temperature_color", temperature_color(x)),
        ] {
            assert!(
                c.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)),
                "{what}({x}) = {c:?}"
            );
        }
        let k = head_temperature_k(x);
        assert!(
            k.is_finite() && (3_000.0..=9_000.0).contains(&k),
            "head_temperature_k({x}) = {k}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// weather.rs

#[test]
fn weather_lerp_hits_both_ends_clamps_and_treats_nan_as_the_start() {
    let a = WeatherState::of_genus(CloudGenus::FairCumulus);
    let b = WeatherState::of_genus(CloudGenus::Cirrus);
    let at0 = WeatherState::lerp(a, b, 0.0);
    assert_eq!(at0.coverage, a.coverage);
    assert_eq!(at0.density, a.density);
    assert_eq!(WeatherState::lerp(a, b, 1.0), b);
    assert_eq!(WeatherState::lerp(a, b, 7.0), b);
    assert_eq!(WeatherState::lerp(a, b, -3.0).coverage, a.coverage);
    let n = WeatherState::lerp(a, b, f32::NAN);
    assert_eq!(n.coverage, a.coverage, "NaN progress must not poison the sky");
    assert_eq!(n.density, a.density);
    assert_eq!(n.wind_m_per_s, a.wind_m_per_s);
    let mid = WeatherState::lerp(a, b, 0.5);
    assert!(approx(mid.coverage, 0.5 * (a.coverage + b.coverage), 1e-6));
}

#[test]
fn weather_of_genus_is_sane_and_quantized_is_idempotent_and_in_range() {
    for g in CloudGenus::ALL {
        let w = WeatherState::of_genus(g);
        assert_eq!(w.genus, Some(g));
        assert_eq!(w.precipitation, 0.0);
        assert_eq!(w.sanitized(), w, "{g:?} is not already sane");
        assert_eq!(WeatherState::preset(g.name()), Some(w));
        let q = w.quantized();
        assert_eq!(q.quantized(), q, "{g:?}");
        assert!((0.0..=1.0).contains(&q.coverage) && (0.0..=1.0).contains(&q.cloud_type));
        assert!(q.density > 0.0 && q.density.is_finite());
        assert_eq!(q.genus, w.genus);
        assert_eq!(q.wind_m_per_s, w.wind_m_per_s);
    }
    let mut w = WeatherState::of_genus(CloudGenus::Cumulus);
    for d in [0.0, -1.0, f32::NAN] {
        w.density = d;
        assert_eq!(w.quantized().density, 0.0, "density {d}");
    }
    for v in HOSTILE {
        let mut w = WeatherState::of_genus(CloudGenus::Cumulus);
        w.coverage = v;
        w.cloud_type = v;
        w.density = v;
        let q = w.sanitized().quantized();
        assert!(q.coverage.is_finite() && q.cloud_type.is_finite(), "{v}: {q:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// quality.rs

#[test]
fn apply_exposure_divides_by_the_floored_exposure() {
    let rgb = [0.5, 2.0, 100.0];
    let e = 0.25;
    let out = apply_exposure(rgb, e);
    for i in 0..3 {
        assert!(approx(out[i] * e, rgb[i], 1e-6));
    }
    for e in [0.0, -1.0, f32::NAN, f32::NEG_INFINITY, 1e-20] {
        let out = apply_exposure([1.0; 3], e);
        assert!(out.iter().all(|v| v.is_finite()), "exposure {e}: {out:?}");
        assert_eq!(out[0], 1.0 / EXPOSURE_FLOOR);
    }
}

#[test]
fn spherical_normal_is_unit_inside_the_disc_and_flat_outside() {
    assert_eq!(spherical_normal([0.0, 0.0]), [0.0, 0.0, 1.0]);
    for i in 0..=20 {
        for j in 0..=20 {
            let uv = [i as f32 / 10.0 - 1.0, j as f32 / 10.0 - 1.0];
            let n = spherical_normal(uv);
            assert!(n[2] >= 0.0);
            let r2 = uv[0] * uv[0] + uv[1] * uv[1];
            if r2 <= 1.0 {
                let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                assert!((len - 1.0).abs() < 1e-5, "{uv:?}");
            } else {
                assert_eq!(n[2], 0.0);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// atlas.rs

#[test]
fn tile_index_is_unique_and_matches_the_sheet_layout() {
    let all = [
        SpriteId::SoftDisc,
        SpriteId::HardDisc,
        SpriteId::Ring,
        SpriteId::Spark,
        SpriteId::Puff,
        SpriteId::Flare,
        SpriteId::Shard,
        SpriteId::PuffAnim,
        SpriteId::FireAnim,
        SpriteId::FireballAnim,
    ];
    let mut tiles: Vec<u32> = all.iter().map(|s| tile_index(*s)).collect();
    for (s, t) in all.iter().zip(&tiles).take(7) {
        assert!(
            *t < PUFF_ANIM_TILE,
            "identity sprite {s:?} at {t} overlaps the puff sheet"
        );
    }
    assert_eq!(tile_index(SpriteId::PuffAnim), PUFF_ANIM_TILE);
    assert_eq!(tile_index(SpriteId::FireAnim), FIRE_ANIM_TILE);
    assert_eq!(tile_index(SpriteId::FireballAnim), FIREBALL_ANIM_TILE);
    tiles.sort_unstable();
    tiles.dedup();
    assert_eq!(tiles.len(), all.len());
}

// ---------------------------------------------------------------------------------------------
// decal.rs

fn assert_unit(q: Quat, what: &str) {
    assert!(q.is_finite() && (q.length() - 1.0).abs() < 1e-4, "{what}: {q}");
}

#[test]
fn align_along_points_z_at_the_normal_and_y_along_the_tangent() {
    let cases = [
        (Vec3::Y, Vec3::X),
        (Vec3::X, Vec3::Y),
        (Vec3::new(0.3, 1.0, -0.2), Vec3::new(1.0, 0.0, 0.5)),
        (-Vec3::Z, Vec3::new(1.0, 1.0, 1.0)),
    ];
    for (n, t) in cases {
        let q = align_along(n, t);
        assert_unit(q, "align_along");
        let z = n.normalize();
        assert!((q * Vec3::Z).distance(z) < 1e-4, "{n} {t}");
        let ty = (t - z * t.dot(z)).normalize();
        assert!((q * Vec3::Y).distance(ty) < 1e-4, "{n} {t}");
        // Right-handed: X = Y × Z.
        assert!((q * Vec3::X).distance((q * Vec3::Y).cross(q * Vec3::Z)) < 1e-4);
    }
}

#[test]
fn align_along_survives_degenerate_and_non_finite_tangents() {
    for n in [Vec3::X, Vec3::Y, -Vec3::Z, Vec3::new(1.0, 2.0, 3.0)] {
        let z = n.normalize();
        for t in [
            Vec3::ZERO,
            n * 3.0,
            Vec3::NAN,
            Vec3::new(f32::INFINITY, 0.0, 0.0),
            Vec3::new(0.0, f32::NEG_INFINITY, 1.0),
            Vec3::new(0.0, 3e19, 3e19),
        ] {
            let q = align_along(n, t);
            assert_unit(q, &format!("align_along({n}, {t})"));
            assert!(
                (q * Vec3::Z).distance(z) < 1e-4,
                "align_along({n}, {t}) lost the normal"
            );
        }
    }
    // A degenerate normal falls back to +Y.
    let q = align_along(Vec3::ZERO, Vec3::X);
    assert!((q * Vec3::Z).distance(Vec3::Y) < 1e-4);
}

#[test]
fn decal_anchor_from_world_round_trips_through_the_model_transform() {
    let rot = Quat::from_rotation_y(0.7) * Quat::from_rotation_x(-0.3);
    let pos = Vec3::new(5.0, -2.0, 11.0);
    // Scale is ignored by the anchor, as documented.
    let model = Mat4::from_scale_rotation_translation(Vec3::splat(3.0), rot, pos);
    let world_p = Vec3::new(7.0, 1.0, 9.5);
    let world_r = Quat::from_rotation_z(1.1);
    let a = DecalAnchor::from_world(Entity::PLACEHOLDER, model, world_p, world_r);
    assert_eq!(a.entity, Entity::PLACEHOLDER);
    assert!((pos + rot * a.local_position).distance(world_p) < 1e-4);
    assert!((rot * a.local_rotation).angle_between(world_r) < 1e-3);
    // Identity model: local equals world.
    let id = DecalAnchor::from_world(Entity::PLACEHOLDER, Mat4::IDENTITY, world_p, world_r);
    assert!(id.local_position.distance(world_p) < 1e-6);
}

// ---------------------------------------------------------------------------------------------
// shake.rs

#[test]
fn camera_shake_trauma_stays_in_unit_range_for_hostile_steps() {
    for dt in [-1.0, -100.0, f32::NEG_INFINITY, f32::NAN, f32::INFINITY, 0.0, 0.1] {
        let mut shake = CameraShake::default();
        shake.add_trauma(0.5);
        shake.update(dt, 1.2, 0.35, 0.02);
        let t = shake.trauma();
        assert!((0.0..=1.0).contains(&t), "update({dt}) left trauma {t}");
        assert!(
            shake.offset().is_finite() && shake.roll().is_finite(),
            "update({dt})"
        );
    }
    // A negative decay never pumps trauma past 1 either.
    let mut shake = CameraShake::default();
    shake.add_trauma(0.9);
    shake.update(1.0, -5.0, 0.35, 0.02);
    assert!(shake.trauma() <= 1.0, "{}", shake.trauma());
    // add_trauma ignores NaN and negative amounts.
    let mut shake = CameraShake::default();
    for amount in [f32::NAN, -1.0, f32::NEG_INFINITY] {
        shake.add_trauma(amount);
        assert_eq!(shake.trauma(), 0.0, "{amount}");
    }
}

#[test]
fn camera_shake_keeps_shaking_after_a_hostile_step() {
    // One NaN or negative frame time must not freeze the noise clock: the offset still changes
    // from frame to frame afterwards.
    for bad in [f32::NAN, -0.5, f32::INFINITY] {
        let mut shake = CameraShake::default();
        shake.update(bad, 1.2, 0.35, 0.02);
        shake.add_trauma(1.0);
        shake.update(0.1, 0.0, 0.35, 0.02);
        let a = shake.offset();
        shake.update(0.1, 0.0, 0.35, 0.02);
        let b = shake.offset();
        assert!(a.is_finite() && b.is_finite());
        assert_ne!(a, b, "after update({bad}) the shake froze at {a}");
    }
}

#[test]
fn camera_shake_apply_rotation_keeps_a_unit_quaternion() {
    let mut shake = CameraShake::default();
    shake.add_trauma(1.0);
    shake.update(0.01, 0.0, 0.35, 0.3);
    assert!(shake.roll() != 0.0, "test setup: no roll");
    let base = Quat::from_rotation_y(0.4);
    let rolled = shake.apply_rotation(base, Vec3::NEG_Z);
    assert_unit(rolled, "apply_rotation");
    assert!((rolled.angle_between(base) - shake.roll().abs()).abs() < 1e-3);
    for forward in [Vec3::ZERO, Vec3::NAN, Vec3::new(f32::INFINITY, 0.0, 0.0)] {
        let q = shake.apply_rotation(base, forward);
        assert_unit(q, &format!("apply_rotation(.., {forward})"));
    }
    // No roll: unchanged.
    assert_eq!(CameraShake::default().apply_rotation(base, Vec3::NEG_Z), base);
}

#[test]
fn camera_shake_trauma_from_has_the_documented_falloff() {
    let req = CameraShakeRequest {
        position: Vec3::ZERO,
        strength: 0.8,
        radius: 10.0,
    };
    assert_eq!(CameraShake::trauma_from(&req, Vec3::new(0.0, 0.0, 10.0)), 0.8);
    assert!(approx(
        CameraShake::trauma_from(&req, Vec3::new(15.0, 0.0, 0.0)),
        0.4,
        1e-6
    ));
    assert_eq!(CameraShake::trauma_from(&req, Vec3::new(20.0, 0.0, 0.0)), 0.0);
    assert_eq!(CameraShake::trauma_from(&req, Vec3::new(500.0, 0.0, 0.0)), 0.0);
    let point = CameraShakeRequest { radius: 0.0, ..req };
    assert_eq!(CameraShake::trauma_from(&point, Vec3::ZERO), 0.8);
    assert_eq!(CameraShake::trauma_from(&point, Vec3::X), 0.0);
}

// ---------------------------------------------------------------------------------------------
// curves.rs

#[test]
fn bake_color_dedupes_identical_curves_and_packs_new_ones() {
    let mut lut = CurveLut::default();
    let per = CURVE_SAMPLES as usize * 4;
    let base_len = lut.colors().len();
    let white = lut.bake_color(&[]);
    assert_eq!(lut.bake_color(&[]), white);
    let red = vec![
        ColorKey {
            t: 0.0,
            color: "#ff0000".into(),
        },
        ColorKey {
            t: 1.0,
            color: "#000000".into(),
        },
    ];
    let len_before = lut.colors().len();
    let r1 = lut.bake_color(&red);
    let r2 = lut.bake_color(&red);
    assert_eq!(r1, r2);
    assert_ne!(r1, white);
    assert!(lut.colors().len() <= len_before + per);
    assert_eq!((lut.colors().len() - base_len) % per, 0);
    // The empty curve is white everywhere.
    let mut fresh = CurveLut::default();
    let _ = fresh.bake_color(&[]);
    let n = fresh.colors().len();
    assert!(n >= per);
    for px in fresh.colors()[n - per..].chunks(4) {
        assert_eq!(px, [1.0, 1.0, 1.0, 1.0]);
    }
}

// ---------------------------------------------------------------------------------------------
// measure.rs, glass.rs

#[test]
fn metrics_line_names_the_effect_and_its_bbox() {
    let m = EffectMetrics {
        coverage: 0.125,
        mean_luma: 0.5,
        blown: 0.01,
        dark: 0.0,
        distinct_colors: 42,
        warmth: -0.25,
        mean_delta: 0.1,
        bbox: None,
    };
    let line = metrics_line("sparks", &m);
    assert!(line.starts_with("sparks"));
    assert!(line.contains("coverage  12.50%"), "{line}");
    assert!(line.contains("colors  42"), "{line}");
    assert!(line.contains("warmth -0.25"), "{line}");
    assert!(line.ends_with("bbox none"), "{line}");
    let boxed = metrics_line(
        "x",
        &EffectMetrics {
            bbox: Some((0.1, 0.2, 0.3, 0.4)),
            ..m
        },
    );
    assert!(boxed.ends_with("bbox [0.10,0.20]-[0.30,0.40]"), "{boxed}");
    assert!(!line.contains('\n'));
}

#[test]
fn glass_breach_effect_and_outside_pressure_per_atmosphere() {
    let s = GlassEffectSettings::default();
    let all = [
        BreachAtmosphere::Earth,
        BreachAtmosphere::Mars,
        BreachAtmosphere::Airless,
    ];
    let kpa: Vec<f32> = all.iter().map(|a| a.outside_kpa()).collect();
    assert!(kpa[0] > kpa[1] && kpa[1] > kpa[2] && kpa[2] == 0.0, "{kpa:?}");
    for a in all {
        let effect = s.breach_effect(a);
        assert!(effect.is_none_or(|e| e.contains(a.name())), "{a:?}: {effect:?}");
    }
    let none = GlassEffectSettings {
        breach_earth: None,
        breach_mars: None,
        breach_airless: None,
        ..s
    };
    assert!(all.iter().all(|a| none.breach_effect(*a).is_none()));
}

// ---------------------------------------------------------------------------------------------
// format.rs: VfxSystemDef::validate refuses non-finite numbers from a `.vfx.ron` file

/// A named edit of an effect definition.
type DefEdit = (&'static str, fn(&mut VfxSystemDef));

fn minimal_def() -> VfxSystemDef {
    VfxSystemDef::from_ron(&format!(
        "(version: {VFX_FORMAT_VERSION}, name: \"x\", emitters: [(name: \"e\", \
         spawn: (mode: Rate(10.0), lifetime: (min: 0.5, max: 1.0)), \
         init: (size: (min: 1.0, max: 2.0)), render: (mode: Billboard))])"
    ))
    .expect("a valid minimal effect")
}

#[test]
fn vfx_validate_refuses_non_finite_numbers() {
    let base = minimal_def();
    assert!(base.validate().is_ok());
    let cases: Vec<DefEdit> = vec![
        ("NaN rate", |d| {
            d.emitters[0].spawn.mode = SpawnMode::Rate(f32::NAN)
        }),
        ("infinite rate", |d| {
            d.emitters[0].spawn.mode = SpawnMode::Rate(f32::INFINITY)
        }),
        ("NaN every", |d| {
            d.emitters[0].spawn.mode = SpawnMode::PerDistance { every: f32::NAN }
        }),
        ("infinite every", |d| {
            d.emitters[0].spawn.mode = SpawnMode::PerDistance { every: f32::INFINITY }
        }),
        ("NaN size", |d| {
            d.emitters[0].init.size = RangeDef {
                min: f32::NAN,
                max: 1.0,
            }
        }),
        ("infinite size", |d| {
            d.emitters[0].init.size = RangeDef {
                min: 1.0,
                max: f32::INFINITY,
            }
        }),
        ("NaN spin", |d| {
            d.emitters[0].init.spin = RangeDef {
                min: 0.0,
                max: f32::NAN,
            }
        }),
        ("NaN emissive", |d| d.emitters[0].render.emissive = f32::NAN),
        ("infinite emissive", |d| {
            d.emitters[0].render.emissive = f32::INFINITY
        }),
        ("NaN screen floor", |d| {
            d.emitters[0].render.screen_floor_px = f32::NAN
        }),
        ("NaN delay", |d| d.emitters[0].delay = f32::NAN),
        ("negative delay", |d| d.emitters[0].delay = -1.0),
        ("NaN duration", |d| d.emitters[0].duration = Some(f32::NAN)),
        ("negative duration", |d| d.emitters[0].duration = Some(-2.0)),
        ("NaN LOD distance", |d| {
            d.system.lod = Some(LodDef {
                distances: vec![f32::NAN],
                rate_scale: vec![1.0, 0.5],
            })
        }),
        ("NaN LOD rate scale", |d| {
            d.system.lod = Some(LodDef {
                distances: vec![10.0],
                rate_scale: vec![1.0, f32::NAN],
            })
        }),
        ("negative LOD rate scale", |d| {
            d.system.lod = Some(LodDef {
                distances: vec![10.0],
                rate_scale: vec![1.0, -0.5],
            })
        }),
        // The rest of the float fields close the set (deep-zai-vfx finding 1, v0.5.1): forces,
        // scales, curve keys, shapes and lights all reach the GPU or the CPU sim, so a NaN or
        // inf in any of them is refused like a NaN rate above.
        ("NaN gravity", |d| {
            d.emitters[0].update.gravity = (0.0, f32::NAN, 0.0)
        }),
        ("infinite gravity", |d| {
            d.emitters[0].update.gravity = (f32::NEG_INFINITY, 0.0, 0.0)
        }),
        ("NaN drag", |d| d.emitters[0].update.drag = f32::NAN),
        ("negative drag", |d| d.emitters[0].update.drag = -1.0),
        ("NaN curl amount", |d| {
            d.emitters[0].update.curl = Some((f32::NAN, 1.0, 1.0))
        }),
        ("infinite curl frequency", |d| {
            d.emitters[0].update.curl = Some((1.0, f32::INFINITY, 1.0))
        }),
        ("NaN curl speed", |d| {
            d.emitters[0].update.curl = Some((1.0, 1.0, f32::NAN))
        }),
        ("NaN vortex axis", |d| {
            d.emitters[0].update.vortex = Some(VortexDef {
                axis: (0.0, f32::NAN, 1.0),
                strength: 1.0,
                radial: 0.0,
            })
        }),
        ("NaN vortex strength", |d| {
            d.emitters[0].update.vortex = Some(VortexDef {
                axis: (0.0, 1.0, 0.0),
                strength: f32::NAN,
                radial: 0.0,
            })
        }),
        ("NaN vortex radial", |d| {
            d.emitters[0].update.vortex = Some(VortexDef {
                axis: (0.0, 1.0, 0.0),
                strength: 1.0,
                radial: f32::NAN,
            })
        }),
        ("NaN attractor position", |d| {
            d.emitters[0].update.attractor = Some(AttractorDef {
                position: (0.0, f32::NAN, 0.0),
                strength: 1.0,
            })
        }),
        ("NaN attractor strength", |d| {
            d.emitters[0].update.attractor = Some(AttractorDef {
                position: (0.0, 1.0, 0.0),
                strength: f32::NAN,
            })
        }),
        ("NaN kill_below", |d| {
            d.emitters[0].update.kill_below = Some(f32::NAN)
        }),
        ("NaN depth offset", |d| {
            d.emitters[0].update.depth_collision = Some(DepthCollisionDef {
                kill: true,
                offset: f32::NAN,
            })
        }),
        ("NaN count_scale", |d| d.emitters[0].spawn.count_scale = f32::NAN),
        ("negative count_scale", |d| d.emitters[0].spawn.count_scale = -2.0),
        ("NaN size_over_life value", |d| {
            d.emitters[0].update.size_over_life = vec![FloatKey { t: 0.0, v: f32::NAN }]
        }),
        ("NaN alpha_over_life time", |d| {
            d.emitters[0].update.alpha_over_life = vec![FloatKey { t: f32::NAN, v: 1.0 }]
        }),
        ("NaN rotation_over_life value", |d| {
            d.emitters[0].update.rotation_over_life = vec![FloatKey {
                t: 0.5,
                v: f32::INFINITY,
            }]
        }),
        ("NaN init color time", |d| {
            d.emitters[0].init.color = vec![ColorKey {
                t: f32::NAN,
                color: "#ffffff".into(),
            }]
        }),
        ("NaN color_over_life time", |d| {
            d.emitters[0].update.color_over_life = vec![ColorKey {
                t: f32::NAN,
                color: "#ffffff".into(),
            }]
        }),
        ("NaN inherit", |d| d.emitters[0].init.velocity.inherit = f32::NAN),
        ("NaN sphere radius", |d| {
            d.emitters[0].init.shape = ShapeDef::Sphere {
                radius: RangeDef {
                    min: f32::NAN,
                    max: 1.0,
                },
                surface: false,
            }
        }),
        ("NaN box half extents", |d| {
            d.emitters[0].init.shape = ShapeDef::Box {
                half_extents: (1.0, f32::NAN, 1.0),
                surface: false,
            }
        }),
        ("NaN cone length", |d| {
            d.emitters[0].init.shape = ShapeDef::Cone {
                angle: 10.0,
                length: RangeDef {
                    min: 0.0,
                    max: f32::NAN,
                },
            }
        }),
        ("NaN radial speed", |d| {
            d.emitters[0].init.velocity.mode = VelocityMode::Radial {
                speed: RangeDef {
                    min: f32::NAN,
                    max: 1.0,
                },
            }
        }),
        ("infinite directional speed", |d| {
            d.emitters[0].init.velocity.mode = VelocityMode::Directional {
                dir: (0.0, 0.0, 1.0),
                spread: 0.0,
                speed: RangeDef {
                    min: 0.0,
                    max: f32::INFINITY,
                },
            }
        }),
        ("NaN directional dir", |d| {
            d.emitters[0].init.velocity.mode = VelocityMode::Directional {
                dir: (0.0, f32::NAN, 0.0),
                spread: 0.0,
                speed: RangeDef { min: 1.0, max: 2.0 },
            }
        }),
        ("NaN alpha_scale", |d| d.emitters[0].render.alpha_scale = f32::NAN),
        ("negative alpha_scale", |d| {
            d.emitters[0].render.alpha_scale = -0.5
        }),
        ("NaN softness", |d| d.emitters[0].render.softness = f32::NAN),
        ("negative softness", |d| d.emitters[0].render.softness = -0.1),
        ("NaN stretch velocity_scale", |d| {
            d.emitters[0].render.stretch.velocity_scale = f32::NAN
        }),
        ("negative stretch velocity_scale", |d| {
            d.emitters[0].render.stretch.velocity_scale = -0.05
        }),
        ("NaN stretch min_length", |d| {
            d.emitters[0].render.stretch.min_length = f32::NAN
        }),
        ("NaN heat haze", |d| d.emitters[0].render.heat_haze = f32::NAN),
        ("negative heat haze", |d| d.emitters[0].render.heat_haze = -1.0),
        ("NaN flipbook fps", |d| {
            d.emitters[0].render.flipbook = Some(flipbook(FlipbookMode::Fps(f32::NAN)))
        }),
        ("negative flipbook fps", |d| {
            d.emitters[0].render.flipbook = Some(flipbook(FlipbookMode::Fps(-5.0)))
        }),
        ("NaN light intensity", |d| {
            d.emitters[0].render.light = Some(light(f32::NAN, 1.0))
        }),
        ("negative light intensity", |d| {
            d.emitters[0].render.light = Some(light(-1.0, 1.0))
        }),
        ("NaN light radius", |d| {
            d.emitters[0].render.light = Some(light(1.0, f32::NAN))
        }),
        ("negative light radius", |d| {
            d.emitters[0].render.light = Some(light(1.0, -1.0))
        }),
        ("NaN ribbon width time", |d| {
            d.emitters[0].render.ribbon.width = vec![FloatKey { t: f32::NAN, v: 1.0 }]
        }),
        ("NaN ribbon width value", |d| {
            d.emitters[0].render.ribbon.width = vec![FloatKey { t: 0.0, v: f32::NAN }]
        }),
        ("NaN ribbon color time", |d| {
            d.emitters[0].render.ribbon.color = vec![ColorKey {
                t: f32::NAN,
                color: "#ffffff".into(),
            }]
        }),
    ];
    for (what, edit) in cases {
        let mut def = base.clone();
        edit(&mut def);
        assert!(def.validate().is_err(), "{what} passed validation");
        // The file form is refused too (RON spells NaN and inf).
        if let Ok(text) = def.to_ron() {
            assert!(VfxSystemDef::from_ron(&text).is_err(), "{what} loaded from RON");
        }
    }
    // Legal values next to the refused ones still pass.
    let mut ok = base.clone();
    ok.emitters[0].delay = 0.0;
    ok.emitters[0].duration = Some(0.0);
    ok.emitters[0].render.emissive = 0.0;
    ok.emitters[0].spawn.mode = SpawnMode::Rate(0.0);
    ok.system.lod = Some(LodDef {
        distances: vec![10.0, 20.0],
        rate_scale: vec![1.0, 0.5, 0.0],
    });
    // Signs the closed set still allows: any-direction forces and sizes, a size-multiple
    // stretch floor, downward kill planes and zero scales.
    ok.emitters[0].update.gravity = (0.0, -9.81, 0.0);
    ok.emitters[0].update.drag = 0.0;
    ok.emitters[0].update.curl = Some((-0.5, 0.2, 1.0));
    ok.emitters[0].update.kill_below = Some(-1.0);
    ok.emitters[0].update.vortex = Some(VortexDef {
        axis: (0.0, 1.0, 0.0),
        strength: -2.0,
        radial: -0.25,
    });
    ok.emitters[0].update.attractor = Some(AttractorDef {
        position: (0.0, 2.0, 0.0),
        strength: -4.0,
    });
    ok.emitters[0].update.depth_collision = Some(DepthCollisionDef {
        kill: false,
        offset: 0.0,
    });
    ok.emitters[0].spawn.count_scale = 0.0;
    ok.emitters[0].init.velocity.inherit = 2.0;
    ok.emitters[0].render.alpha_scale = 0.0;
    ok.emitters[0].render.softness = 0.0;
    ok.emitters[0].render.stretch.velocity_scale = 0.0;
    ok.emitters[0].render.stretch.min_length = -1.0;
    ok.emitters[0].render.heat_haze = 0.0;
    ok.emitters[0].render.flipbook = Some(flipbook(FlipbookMode::Fps(0.0)));
    ok.emitters[0].render.light = Some(light(0.0, 0.0));
    ok.validate().expect("legal values still validate");
}

/// A flipbook definition playing back in `mode`.
fn flipbook(mode: FlipbookMode) -> FlipbookDef {
    FlipbookDef {
        cols: 4,
        rows: 4,
        frames: 16,
        mode,
        random_start: false,
        blend_frames: false,
    }
}

/// A particle light of `intensity` and `radius`.
fn light(intensity: f32, radius: f32) -> ParticleLightDef {
    ParticleLightDef {
        color: "#ffffff".into(),
        intensity,
        radius,
    }
}

// ---------------------------------------------------------------------------------------------
// format.rs / events.rs: the packed event-link child count cannot exceed its 24-bit field
// (deep-zai-vfx finding 2)

/// A burst parent with a death link to an event child whose `count_scale` the caller sets.
fn linked_def(count_scale: f32) -> VfxSystemDef {
    let mut def = minimal_def();
    let mut child = def.emitters[0].clone();
    child.name = "child".into();
    child.spawn.mode = SpawnMode::Event;
    child.spawn.count_scale = count_scale;
    def.emitters[0].update.events = vec![EventSpawnDef {
        on: EventTrigger::Death,
        child: "child".into(),
        count: 64,
        probability: 1.0,
        inherit_velocity: 0.0,
    }];
    def.emitters.push(child);
    def
}

#[test]
fn vfx_event_links_refuse_counts_above_the_packed_limit() {
    // The GPU link packs the scaled child count into 24 bits of `trigger_count`: a count of
    // exactly 2^24 packs to zero (the link never fires) and anything above wraps to
    // `scaled - 2^24` children. The loader refuses the authored worst case: count x
    // count_scale x the largest LOD rate scale x the intensity cap (4).
    let def = linked_def(262_144.0);
    let reason = def
        .validate()
        .expect_err("64 children x 262144 x intensity 4 exceeds the packed limit")
        .to_string();
    assert!(reason.contains("packed"), "{reason}");
    // Without LOD the boundary is exact: 64 x 65535.99609375 x 4 = 2^24 - 1, the largest
    // packable count (65_535.996 is the shortest literal spelling of that f32). It validates;
    // one ulp of count_scale more does not.
    let at_limit = linked_def(65_535.996);
    at_limit.validate().expect("the exact packable worst case");
    let over = linked_def(65_536.0);
    over.validate().expect_err("2^24 children cannot be packed");
    // A LOD band multiplies the worst case: the at-limit scale is refused behind a x4 band.
    let mut banded = linked_def(65_535.996);
    banded.system.lod = Some(LodDef {
        distances: vec![10.0],
        rate_scale: vec![1.0, 4.0],
    });
    banded
        .validate()
        .expect_err("the LOD band pushes the worst case past the limit");
    // A file that went through validation is refused by `from_ron` too.
    let text = linked_def(1e9_f32).to_ron().expect("writes");
    assert!(
        VfxSystemDef::from_ron(&text).is_err(),
        "an absurd scale loads from RON"
    );
}

#[test]
fn vfx_scaled_event_count_never_wraps_the_packed_field() {
    // `scaled_count` packs into the same 24-bit field; whatever a caller hands it (a param set
    // after load), the result is clamped to the packable maximum instead of wrapping.
    let child = |count_scale: f32| {
        let mut def = minimal_def();
        def.emitters[0].spawn.count_scale = count_scale;
        def.emitters[0].clone()
    };
    // Exactly 2^24 scaled children used to pack to zero (the link never fired).
    assert_eq!(
        scaled_count(64, &child(262_144.0), 1.0, 1.0),
        16_777_215,
        "exactly 2^24 wraps the packed count to zero"
    );
    // Above the field, and past u32 range entirely: the packable maximum, not a wrap.
    assert_eq!(scaled_count(64, &child(300_000.0), 1.0, 1.0), 16_777_215);
    assert_eq!(scaled_count(64, &child(1e9), 1.0, 1.0), 16_777_215);
    // In range the scale keeps its exact meaning.
    assert_eq!(scaled_count(3, &child(2.0), 0.5, 4.0), 12);
    assert_eq!(scaled_count(1, &child(1.0), 1.0, 1.0), 1);
}

#[test]
fn every_showcase_effect_still_validates() {
    for def in regia_vfx::showcase_effects() {
        assert!(def.validate().is_ok(), "{}: {:?}", def.name, def.validate());
        let text = def.to_ron().expect("writes");
        assert!(
            VfxSystemDef::from_ron(&text).is_ok(),
            "{} does not round-trip",
            def.name
        );
    }
}

// ---------------------------------------------------------------------------------------------
// decal_format.rs: DecalPrefab::from_ron / instantiate refuse non-finite numbers;
// migrate_decal_ron

/// A named edit of a decal prefab.
type PrefabEdit = (&'static str, fn(&mut regia_vfx::decal_format::DecalPrefab));

#[test]
fn decal_prefab_refuses_non_finite_numbers() {
    use regia_vfx::decal_format::{DecalPrefab, builtin_prefab};
    let base = builtin_prefab("scorch").expect("built-in scorch");
    assert!(DecalPrefab::from_ron(&base.to_ron().expect("writes")).is_ok());
    let cases: Vec<PrefabEdit> = vec![
        ("NaN roughness", |p| p.roughness = f32::NAN),
        ("NaN normal strength", |p| p.normal_strength = f32::NAN),
        ("NaN normal xy", |p| p.normal_xy = [f32::NAN, 0.0]),
        ("infinite normal xy", |p| p.normal_xy = [0.0, f32::INFINITY]),
        ("NaN angle fade", |p| p.angle_fade = [f32::NAN, 0.5]),
        ("NaN distance fade", |p| p.distance_fade = [0.0, f32::NAN]),
        ("NaN edge softness", |p| p.edge_softness = f32::NAN),
        ("NaN lifetime", |p| p.lifetime = Some(f32::NAN)),
        ("negative lifetime", |p| p.lifetime = Some(-1.0)),
        ("NaN opacity", |p| p.opacity = f32::NAN),
    ];
    for (what, edit) in cases {
        let mut p = base.clone();
        edit(&mut p);
        let text = p.to_ron().expect("writes");
        assert!(DecalPrefab::from_ron(&text).is_err(), "{what} loaded");
        assert!(
            p.instantiate(Vec3::ZERO, Vec3::Y, 0.0, None).is_err(),
            "{what} instantiated"
        );
    }
    // A disabled fade (end <= start) and no lifetime are fine.
    let mut off = base.clone();
    off.distance_fade = [0.0, 0.0];
    off.lifetime = None;
    assert!(off.instantiate(Vec3::ZERO, Vec3::Y, 0.0, Some(3.0)).is_ok());
}

#[test]
fn migrate_decal_ron_passes_current_files_and_refuses_others() {
    use regia_vfx::decal_format::{DECAL_FORMAT_VERSION, migrate_decal_ron};
    let current = format!("(version: {DECAL_FORMAT_VERSION}, name: \"x\")");
    assert_eq!(migrate_decal_ron(&current).expect("current"), current);
    let newer = format!("(version: {}, name: \"x\")", DECAL_FORMAT_VERSION + 1);
    assert!(migrate_decal_ron(&newer).is_err());
    assert!(
        migrate_decal_ron("(version: 0, name: \"x\")").is_err(),
        "no migrator to v1 from v0"
    );
    assert!(migrate_decal_ron("(name: \"x\")").is_err(), "no version field");
    assert!(migrate_decal_ron("").is_err());
}

// ---------------------------------------------------------------------------------------------
// crash.rs: crash_fire_sites / crash_fire_placements; weather.rs: Weather::genus_layer

#[test]
fn crash_fire_sites_are_seeded_inside_the_fire_ring_and_mostly_small() {
    use regia_vfx::crash::crash_fire_sites;
    let mut counts = [0usize; 3];
    for scale in [
        CrashScale::VEHICLE,
        CrashScale::SHOWCASE,
        CrashScale::LARGEST,
        CrashScale::from_tonnes(0.05),
    ] {
        let inner = 0.65 * scale.crater().diameter;
        let outer = scale.fire_zone().max(inner + 1.0);
        for seed in 0..40_u64 {
            let sites = crash_fire_sites(scale, seed);
            assert_eq!(sites, crash_fire_sites(scale, seed), "not deterministic");
            assert_eq!(sites.len() as u32, scale.fire_count());
            assert!((3..=80).contains(&sites.len()));
            for site in &sites {
                let r = (site.offset[0].powi(2) + site.offset[1].powi(2)).sqrt();
                assert!(
                    r >= inner * 0.999 && r <= outer * 1.001,
                    "{r} outside {inner}..{outer}"
                );
                counts[FireSize::ALL
                    .iter()
                    .position(|s| *s == site.size)
                    .expect("a size")] += 1;
            }
        }
        assert_ne!(
            crash_fire_sites(scale, 1),
            crash_fire_sites(scale, 2),
            "seeds differ"
        );
    }
    // Half small, a third medium, the rest large (wide bounds over thousands of sites).
    let total = counts.iter().sum::<usize>() as f32;
    let share = |i: usize| counts[i] as f32 / total;
    assert!((share(0) - 0.5).abs() < 0.05, "{counts:?}");
    assert!((share(1) - 0.35).abs() < 0.05, "{counts:?}");
    assert!((share(2) - 0.15).abs() < 0.05, "{counts:?}");
}

#[test]
fn crash_fire_placements_lie_on_the_ground_plane_and_honor_the_ground_callback() {
    use regia_vfx::crash::crash_fire_placements;
    let normal = Vec3::new(0.3, 1.0, -0.2).normalize();
    let crash = CrashBurst {
        position: Vec3::new(100.0, 5.0, -40.0),
        normal,
        scale: CrashScale::SHOWCASE,
        ..CrashBurst::default()
    };
    let all = crash_fire_placements(&crash, &|p| Some(p));
    assert_eq!(all.len() as u32, crash.scale.fire_count());
    for f in &all {
        let rel = f.position - crash.position;
        assert!(
            rel.dot(normal).abs() < 1e-2 * rel.length().max(1.0),
            "{rel} is off the plane"
        );
    }
    let mut seeds: Vec<u64> = all.iter().map(|f| f.seed).collect();
    seeds.sort_unstable();
    seeds.dedup();
    assert_eq!(seeds.len(), all.len(), "every fire has its own seed");
    // A ground callback that refuses half the points skips those fires, keeping the others' seeds.
    let some = crash_fire_placements(&crash, &|p| (p.x > crash.position.x).then_some(p));
    assert!(some.len() < all.len());
    assert!(some.iter().all(|f| all.contains(f)));
    // Fires off: nothing.
    let off = CrashBurst {
        fires: false,
        ..crash
    };
    assert!(crash_fire_placements(&off, &|p| Some(p)).is_empty());
    // A NaN normal places the fires on the +Y plane.
    let flat = CrashBurst {
        normal: Vec3::NAN,
        ..crash
    };
    for f in crash_fire_placements(&flat, &|p| Some(p)) {
        assert!((f.position.y - crash.position.y).abs() < 1e-3);
    }
}

#[test]
fn weather_genus_layer_blends_between_genera_and_settles_on_the_target() {
    use regia_render::CloudGenusLayer;
    use regia_vfx::Weather;
    let cumulus = CloudGenus::Cumulus.layer();
    let cirrus = CloudGenus::Cirrus.layer();
    let mut w = Weather::new(WeatherState::of_genus(CloudGenus::Cumulus));
    assert_eq!(w.genus_layer(), Some(cumulus));
    w.transition_to(WeatherState::of_genus(CloudGenus::Cirrus), 10.0);
    assert!(w.transitioning());
    assert_eq!(
        w.genus_layer(),
        Some(cumulus),
        "the blend starts at the old layer"
    );
    w.step(5.0);
    assert_eq!(w.genus_layer(), Some(CloudGenusLayer::lerp(cumulus, cirrus, 0.5)));
    w.step(5.0);
    assert!(!w.transitioning());
    assert_eq!(w.genus_layer(), Some(cirrus));
    // A weather without a genus keeps the renderer's layer.
    let mut plain = WeatherState::of_genus(CloudGenus::Cumulus);
    plain.genus = None;
    assert_eq!(Weather::new(plain).genus_layer(), None);
    // Non-finite or non-positive transition times switch at once.
    for secs in [f32::NAN, f32::INFINITY, -1.0, 0.0] {
        let mut w = Weather::new(WeatherState::of_genus(CloudGenus::Cumulus));
        w.transition_to(WeatherState::of_genus(CloudGenus::Cirrus), secs);
        assert!(!w.transitioning(), "{secs}");
        assert_eq!(w.genus_layer(), Some(cirrus), "{secs}");
    }
}
