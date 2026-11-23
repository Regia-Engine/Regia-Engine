// SPDX-License-Identifier: MPL-2.0
//! Audio zones (`docs/specs/16-audio.md` § 3d): the DO27 acceptance test as pure math (an engine
//! in a tunnel is heard from the surface only near the mouths and carries about 1.5× further
//! underground), and the same behaviors through an `App` on the offline backend.

use std::sync::Arc;
use std::time::Duration;

use regia_app::{App, TimeUpdateStrategy};
use regia_audio::{
    Audio, AudioEmitter, AudioListener, AudioPlugin, AudioReverbZone, AudioRockField, AudioSettings,
    AudioZonePortals, FnRockField, PlacedZone, ReverbZoneShape, RockTuning, RolloffCurve, SoundAsset,
    SoundAssets, SoundClip, ZoneField, ZoneHearing, ZonePortal,
};
use regia_core::math::{Quat, Vec3};
use regia_scene::{ScenePlugin, Transform};

const RATE: u32 = 48_000;
/// Final amplitude below which an engine counts as inaudible (−30 dB).
const AUDIBLE: f32 = 0.05;

/// Flat ground at y = 0.
fn flat_ground() -> FnRockField<impl Fn(f32, f32) -> Option<f32> + Send + Sync> {
    FnRockField(|_x: f32, _z: f32| Some(0.0))
}

/// A 200 m tunnel along X at y = −10 (radius 3), ending in mouths at (±100, 0, 0).
fn tunnel(scale: f32, with_portals: bool) -> ZoneField {
    let centre = Vec3::new(0.0, -10.0, 0.0);
    let rotation = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
    let mut zone = AudioReverbZone::tube(ReverbZoneShape::Capsule {
        radius: 3.0,
        half_length: 100.0,
    });
    zone.distance_scale = scale;
    let local = |world: Vec3| rotation.inverse() * (world - centre);
    let portals = if with_portals {
        vec![
            ZonePortal {
                position: local(Vec3::new(100.0, 0.0, 0.0)),
                radius: 5.0,
            },
            ZonePortal {
                position: local(Vec3::new(-100.0, 0.0, 0.0)),
                radius: 5.0,
            },
        ]
    } else {
        Vec::new()
    };
    ZoneField {
        zones: vec![PlacedZone::new(zone, centre, rotation, &portals)],
    }
}

fn engine() -> AudioEmitter {
    AudioEmitter {
        min_distance: 2.0,
        max_distance: 400.0,
        rolloff: RolloffCurve::Inverse,
        ..AudioEmitter::default()
    }
}

/// Listener positions in `xs` (at height `y`) that hear the engine at `engine_at`.
fn audible_xs(field: &ZoneField, engine_at: Vec3, y: f32, xs: impl Iterator<Item = f32>) -> Vec<f32> {
    let rock = flat_ground();
    xs.filter(|&x| {
        ZoneHearing::evaluate(
            field,
            Some(&rock),
            &RockTuning::default(),
            &engine(),
            Vec3::new(x, y, 0.0),
            engine_at,
        )
        .gain
            >= AUDIBLE
    })
    .collect()
}

fn span(xs: &[f32]) -> f32 {
    match (xs.first(), xs.last()) {
        (Some(a), Some(b)) => b - a,
        _ => 0.0,
    }
}

#[test]
fn an_engine_in_a_tunnel_is_heard_from_the_surface_only_near_the_mouths() {
    let engine_at = Vec3::new(70.0, -10.0, 0.0);
    let sweep = || (-200..=200).map(|i| i as f32);

    // With the bake's mouths: audible within a few metres of the east mouth, nowhere else.
    let with = audible_xs(&tunnel(1.5, true), engine_at, 1.7, sweep());
    assert!(with.contains(&100.0), "at the mouth: {with:?}");
    assert!(
        with.iter().all(|x| (88.0..=112.0).contains(x)),
        "only near the mouth: {with:?}"
    );
    // Directly above the engine, 7 m of rock: silent. So is the far (west) mouth.
    assert!(!with.contains(&70.0));
    assert!(!with.iter().any(|x| *x < 0.0));

    // Without mouths (no bake yet): rock alone keeps the surface silent everywhere.
    let without = audible_xs(&tunnel(1.5, false), engine_at, 1.7, sweep());
    assert!(without.is_empty(), "rock blocks the unrouted engine: {without:?}");

    // Without zones at all (the engine is just buried): also silent, so the zone is what
    // makes it audible at the mouth, not leakage.
    let none = audible_xs(&ZoneField::default(), engine_at, 1.7, sweep());
    assert!(none.is_empty(), "{none:?}");
}

#[test]
fn underground_the_engine_carries_one_and_a_half_times_further() {
    let engine_at = Vec3::new(-20.0, -10.0, 0.0);
    let sweep = || (-100..=100).map(|i| i as f32);
    let plain = audible_xs(&tunnel(1.0, false), engine_at, -10.0, sweep());
    let scaled = audible_xs(&tunnel(1.5, false), engine_at, -10.0, sweep());
    // Inverse rolloff, min 2 m, threshold 0.05: 40 m audible radius, 60 m with the scale.
    let radius = |xs: &[f32]| xs.iter().map(|x| (x - engine_at.x).abs()).fold(0.0f32, f32::max);
    let (r1, r2) = (radius(&plain), radius(&scaled));
    assert!((r1 - 40.0).abs() <= 1.0, "{r1}");
    assert!((r2 / r1 - 1.5).abs() < 0.05, "ratio {}", r2 / r1);
    assert!(span(&scaled) > span(&plain));
}

#[test]
fn the_distance_scale_blends_out_across_the_blend_distance() {
    let field = tunnel(1.5, false);
    let engine_at = Vec3::new(0.0, -10.0, 0.0);
    // Listener walking up out of the tube through its 6 m blend distance: the scale falls from
    // 1.5 to 1 monotonically.
    let scales: Vec<f32> = (0..=12)
        .map(|i| field.distance_scale(Vec3::new(0.0, -10.0 + 3.0 + i as f32 * 0.5, 0.0), engine_at))
        .collect();
    assert!((scales[0] - 1.5).abs() < 1e-5 && (scales[12] - 1.0).abs() < 1e-5);
    assert!(scales.windows(2).all(|w| w[1] <= w[0] + 1e-6), "{scales:?}");
}

// ---------------------------------------------------------------------------- through an App

fn app(dt: f64) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(dt)));
    app.add_plugins((ScenePlugin, AudioPlugin));
    let len = RATE as usize * 2;
    let mut state = 0x1234_5678u32;
    let samples: Vec<f32> = (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * 0.5
        })
        .collect();
    app.world_mut().resource_mut::<SoundAssets>().add(
        "hum",
        SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("noise")),
    );
    app
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

/// Runs the app and returns the RMS of the last half second.
fn settle(app: &mut App, dt: f64) -> f32 {
    let per = (f64::from(RATE) * dt).round() as usize;
    let mut out = Vec::new();
    for _ in 0..60 {
        app.update();
        let r = app
            .world_mut()
            .resource_mut::<Audio>()
            .render_offline(per)
            .expect("render");
        out.extend_from_slice(&r);
    }
    rms(&out[out.len() / 2..])
}

fn scene(
    dt: f64,
    zone: Option<AudioReverbZone>,
    rock: bool,
    listener: Vec3,
    source: Vec3,
) -> (App, regia_ecs::prelude::Entity) {
    let mut app = app(dt);
    app.world_mut()
        .spawn((AudioListener::default(), Transform::from_translation(listener)));
    let e = app
        .world_mut()
        .spawn((
            AudioEmitter {
                sound_name: "hum".into(),
                min_distance: 1.0,
                max_distance: 40.0,
                rolloff: RolloffCurve::Linear,
                spatial_blend: 1.0,
                ..AudioEmitter::default()
            },
            Transform::from_translation(source),
        ))
        .id();
    if let Some(zone) = zone {
        app.world_mut().spawn((
            zone,
            Transform {
                translation: Vec3::new(0.0, -10.0, 0.0),
                rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
                ..Transform::default()
            },
        ));
    }
    if rock {
        app.world_mut()
            .resource_mut::<AudioRockField>()
            .set(Arc::new(flat_ground()));
    }
    (app, e)
}

fn tube(scale: f32) -> AudioReverbZone {
    AudioReverbZone {
        distance_scale: scale,
        send_db: -80.0,
        ..AudioReverbZone::tube(ReverbZoneShape::Capsule {
            radius: 3.0,
            half_length: 100.0,
        })
    }
}

#[test]
fn a_zone_scale_makes_a_distant_emitter_audible_in_the_rendered_signal() {
    let dt = 1.0 / 60.0;
    // 50 m apart, a 40 m range: silent without the scale, audible with 1.5 (60 m).
    let (mut plain, _) = scene(
        dt,
        Some(tube(1.0)),
        false,
        Vec3::new(-25.0, -10.0, 0.0),
        Vec3::new(25.0, -10.0, 0.0),
    );
    let (mut scaled, e) = scene(
        dt,
        Some(tube(1.5)),
        false,
        Vec3::new(-25.0, -10.0, 0.0),
        Vec3::new(25.0, -10.0, 0.0),
    );
    let quiet = settle(&mut plain, dt);
    let loud = settle(&mut scaled, dt);
    assert!(quiet < 1e-4, "beyond max_distance: {quiet}");
    assert!(loud > 0.003, "carried by the zone: {loud}");
    let status = scaled.world().resource::<Audio>().emitter_status();
    let s = status.iter().find(|s| s.entity == e).expect("status");
    assert!((s.zone_scale - 1.5).abs() < 1e-4, "{}", s.zone_scale);
}

#[test]
fn rock_between_the_listener_and_a_buried_emitter_muffles_it() {
    let dt = 1.0 / 60.0;
    let listener = Vec3::new(0.0, 1.7, 0.0);
    let source = Vec3::new(0.0, -10.0, 0.0);
    let open = |rock| {
        let (mut app, e) = scene(dt, None, rock, listener, source);
        let level = settle(&mut app, dt);
        let thickness = app
            .world()
            .resource::<Audio>()
            .emitter_status()
            .iter()
            .find(|s| s.entity == e)
            .map_or(0.0, |s| s.rock_m);
        (level, thickness)
    };
    let (free, free_m) = open(false);
    let (blocked, blocked_m) = open(true);
    assert_eq!(free_m, 0.0, "no rock field, no rock");
    assert!(
        blocked_m > 8.0 && blocked_m < 12.0,
        "10 m below ground: {blocked_m}"
    );
    assert!(free > 0.01, "{free}");
    assert!(
        blocked < free * 0.1,
        "rock quietens it by well over 20 dB: {blocked} vs {free}"
    );
}

#[test]
fn a_zone_with_portals_routes_a_surface_listener_through_the_mouth() {
    let dt = 1.0 / 60.0;
    let (mut app, e) = scene(
        dt,
        Some(tube(1.5)),
        true,
        Vec3::new(104.0, 1.7, 0.0),
        Vec3::new(75.0, -10.0, 0.0),
    );
    // Mouth at the east end, at the surface (world), given in the zone's local frame.
    let rotation = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
    let zone = {
        let mut q = app
            .world_mut()
            .query::<(regia_ecs::prelude::Entity, &AudioReverbZone)>();
        q.iter(app.world()).next().map(|(e, _)| e).expect("zone")
    };
    app.world_mut().entity_mut(zone).insert(AudioZonePortals {
        portals: vec![ZonePortal {
            position: rotation.inverse() * (Vec3::new(100.0, 0.0, 0.0) - Vec3::new(0.0, -10.0, 0.0)),
            radius: 5.0,
        }],
    });
    settle(&mut app, dt);
    let status = app.world().resource::<Audio>().emitter_status();
    let s = status.iter().find(|s| s.entity == e).expect("status");
    assert!(s.routed, "heard through the mouth");
    assert!(s.rock_m < 0.5, "the mouth leg is open air: {}", s.rock_m);
}

#[test]
fn clearing_the_rock_field_drops_the_reported_thickness() {
    let dt = 1.0 / 60.0;
    let (mut app, e) = scene(
        dt,
        None,
        true,
        Vec3::new(0.0, 1.7, 0.0),
        Vec3::new(0.0, -10.0, 0.0),
    );
    let rock_m = |app: &App| {
        app.world()
            .resource::<Audio>()
            .emitter_status()
            .iter()
            .find(|s| s.entity == e)
            .map_or(0.0, |s| s.rock_m)
    };
    settle(&mut app, dt);
    assert!(rock_m(&app) > 8.0, "rock reported: {}", rock_m(&app));
    app.world_mut().resource_mut::<AudioRockField>().clear();
    settle(&mut app, dt);
    assert_eq!(rock_m(&app), 0.0, "no rock field, no rock");
}

#[test]
fn equal_specificity_zones_blend_in_the_probe_order() {
    let dt = 1.0 / 60.0;
    let at = Vec3::new(0.0, -10.0, 0.0);
    let (mut app, e) = scene(dt, Some(tube(2.0)), false, at, at + Vec3::X);
    // A second zone of the same shape and priority with another scale.
    app.world_mut().spawn((
        tube(0.5),
        Transform {
            translation: Vec3::new(0.0, -10.0, 0.0),
            rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            ..Transform::default()
        },
    ));
    settle(&mut app, dt);
    let heard = app
        .world()
        .resource::<Audio>()
        .emitter_status()
        .iter()
        .find(|s| s.entity == e)
        .expect("status")
        .zone_scale;
    let placed = regia_audio::placed_zones(app.world_mut());
    let field = ZoneField {
        zones: placed.into_iter().map(|(_, z)| z).collect(),
    };
    let probed = field.distance_scale(at, at + Vec3::X);
    assert!((heard - probed).abs() < 1e-5, "{heard} vs {probed}");
}
