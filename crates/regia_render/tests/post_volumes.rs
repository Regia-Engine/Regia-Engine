// SPDX-License-Identifier: MPL-2.0
//! Post volumes (`docs/specs/30-post-volumes-and-physical-camera.md`, ADR-1464): the weight of a
//! volume at a point, the deterministic tier blend, sanitising, the format version, the scene
//! round trip and the per-camera blend in `extract_frame`. CPU only (no device).

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::post_volumes::{
    EyeRange, POST_VOLUME_FORMAT_VERSION, PostOverrides, PostVolume, PostVolumeShape, VolumeContribution,
    blend_post_volumes, gather_contributions, post_volume_migration_sources, post_volume_schema_fingerprint,
};
use regia_render::{
    Camera, ExtractedFrame, FogSettings, LocalExposure, PostSettings, RenderSettings, RenderTarget,
    TonemapOperator, extract_frame,
};
use regia_scene::{GlobalTransform, Transform};

fn at(translation: [f32; 3]) -> GlobalTransform {
    GlobalTransform(Transform::from_translation(Vec3::from_array(translation)).compute_affine())
}

fn boxed(half_extents: [f32; 3], blend_distance: f32) -> PostVolume {
    PostVolume {
        shape: PostVolumeShape::Box { half_extents },
        blend_distance,
        ..PostVolume::default()
    }
}

fn contribution(weight: f32, priority: i32, key: u64, overrides: PostOverrides) -> VolumeContribution {
    VolumeContribution {
        weight,
        priority,
        key,
        overrides,
    }
}

fn saturation(value: f32) -> PostOverrides {
    PostOverrides {
        saturation: Some(value),
        ..PostOverrides::default()
    }
}

fn blended(base: &PostSettings, contributions: &[VolumeContribution]) -> PostSettings {
    let mut post = base.clone();
    let mut list = contributions.to_vec();
    blend_post_volumes(&mut post, &mut list);
    post
}

#[test]
fn no_volume_and_an_empty_volume_leave_the_post_unchanged() {
    let base = PostSettings::default();
    assert_eq!(blended(&base, &[]), base);
    // A full-weight volume that overrides nothing changes nothing either.
    assert_eq!(
        blended(&base, &[contribution(1.0, 3, 1, PostOverrides::default())]),
        base
    );
    assert!(PostOverrides::default().is_empty());
}

#[test]
fn a_box_weighs_one_inside_smoothstep_in_the_band_and_zero_beyond() {
    let volume = boxed([2.0, 2.0, 2.0], 2.0);
    let world = at([10.0, 0.0, 0.0]);
    assert_eq!(volume.weight_at(&world, [10.0, 0.0, 0.0]), 1.0, "centre");
    assert_eq!(volume.weight_at(&world, [12.0, 0.0, 0.0]), 1.0, "on the surface");
    let mid = volume.weight_at(&world, [13.0, 0.0, 0.0]);
    assert!((mid - 0.5).abs() < 1.0e-5, "half way through the band: {mid}");
    assert_eq!(volume.weight_at(&world, [14.0, 0.0, 0.0]), 0.0, "band edge");
    assert_eq!(volume.weight_at(&world, [40.0, 0.0, 0.0]), 0.0, "far outside");
    // The band rises monotonically toward the surface.
    let mut last = 0.0;
    for step in 0..=40 {
        let x = 14.0 - step as f32 * 0.05;
        let w = volume.weight_at(&world, [x, 0.0, 0.0]);
        assert!(w >= last, "weight fell at x = {x}: {w} < {last}");
        last = w;
    }
    // A hard edge (blend 0): inside 1, outside 0.
    let hard = boxed([2.0, 2.0, 2.0], 0.0);
    assert_eq!(hard.weight_at(&world, [11.9, 0.0, 0.0]), 1.0);
    assert_eq!(hard.weight_at(&world, [12.1, 0.0, 0.0]), 0.0);
}

#[test]
fn the_box_follows_rotation_and_scale() {
    let volume = boxed([1.0, 1.0, 4.0], 0.0);
    // Rotated 90 degrees about Y: the long local Z axis now runs along world X.
    let transform = Transform {
        translation: Vec3::ZERO,
        rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        scale: Vec3::ONE,
    };
    let world = GlobalTransform(transform.compute_affine());
    assert_eq!(volume.weight_at(&world, [3.5, 0.0, 0.0]), 1.0);
    assert_eq!(volume.weight_at(&world, [0.0, 0.0, 3.5]), 0.0);
    // Scaled 2x on X: the half extent along X doubles.
    let scaled = Transform {
        scale: Vec3::new(2.0, 1.0, 1.0),
        ..Transform::default()
    };
    let world = GlobalTransform(scaled.compute_affine());
    assert_eq!(volume.weight_at(&world, [1.9, 0.0, 0.0]), 1.0);
}

#[test]
fn spheres_unbound_disabled_and_weight() {
    let sphere = PostVolume {
        shape: PostVolumeShape::Sphere { radius: 3.0 },
        blend_distance: 2.0,
        ..PostVolume::default()
    };
    let world = at([0.0, 5.0, 0.0]);
    assert_eq!(sphere.weight_at(&world, [0.0, 7.0, 0.0]), 1.0);
    let band = sphere.weight_at(&world, [0.0, 9.0, 0.0]);
    assert!((band - 0.5).abs() < 1.0e-5, "{band}");
    assert_eq!(sphere.weight_at(&world, [0.0, 11.0, 0.0]), 0.0);

    let unbound = PostVolume {
        shape: PostVolumeShape::Unbound,
        ..PostVolume::default()
    };
    assert_eq!(unbound.weight_at(&world, [1.0e6, -3.0, 9.0]), 1.0);
    assert_eq!(
        PostVolume {
            weight: 0.25,
            ..unbound.clone()
        }
        .weight_at(&world, [0.0; 3]),
        0.25
    );
    assert_eq!(
        PostVolume {
            weight: 7.0,
            ..unbound.clone()
        }
        .weight_at(&world, [0.0; 3]),
        1.0,
        "weight is clamped to 1"
    );
    assert_eq!(
        PostVolume {
            weight: f32::NAN,
            ..unbound.clone()
        }
        .weight_at(&world, [0.0; 3]),
        0.0
    );
    assert_eq!(
        PostVolume {
            enabled: false,
            ..unbound.clone()
        }
        .weight_at(&world, [0.0; 3]),
        0.0
    );
    // A non-finite eye is never inside.
    assert_eq!(sphere.weight_at(&world, [f32::NAN, 0.0, 0.0]), 0.0);
}

#[test]
fn a_full_weight_volume_replaces_only_the_fields_it_sets() {
    let base = PostSettings::default();
    let post = blended(&base, &[contribution(1.0, 0, 1, saturation(0.5))]);
    assert_eq!(post.saturation, 0.5);
    assert_eq!(post.contrast, base.contrast);
    assert_eq!(post.exposure_compensation, base.exposure_compensation);
    assert_eq!(post.grade, base.grade);
    assert_eq!(post.tonemap, base.tonemap);
}

#[test]
fn a_partial_weight_lerps_from_the_base() {
    let base = PostSettings::default();
    let overrides = PostOverrides {
        exposure_compensation: Some(2.0),
        ..PostOverrides::default()
    };
    let post = blended(&base, &[contribution(0.25, 0, 1, overrides)]);
    assert!(
        (post.exposure_compensation - 0.5).abs() < 1.0e-6,
        "{}",
        post.exposure_compensation
    );
}

#[test]
fn the_higher_priority_wins_whatever_the_order() {
    let base = PostSettings::default();
    let low = contribution(1.0, 0, 1, saturation(0.2));
    let high = contribution(1.0, 1, 2, saturation(1.8));
    assert_eq!(blended(&base, &[low, high]).saturation, 1.8);
    assert_eq!(blended(&base, &[high, low]).saturation, 1.8);
    // A half-weight higher tier lerps half way from what the lower tier left.
    let half_high = contribution(0.5, 1, 2, saturation(1.8));
    let post = blended(&base, &[half_high, low]);
    assert!((post.saturation - 1.0).abs() < 1.0e-6, "{}", post.saturation);
}

#[test]
fn equal_priorities_average_independently_of_order() {
    let base = PostSettings::default();
    let a = contribution(1.0, 0, 7, saturation(1.0));
    let b = contribution(1.0, 0, 3, saturation(2.0));
    assert!((blended(&base, &[a, b]).saturation - 1.5).abs() < 1.0e-6);
    assert_eq!(
        blended(&base, &[a, b]).saturation.to_bits(),
        blended(&base, &[b, a]).saturation.to_bits(),
        "bit-identical in either order"
    );
    // Two quarter weights: mean 1.5 at alpha 0.5 from the base 1.4.
    let a = contribution(0.25, 0, 7, saturation(1.0));
    let b = contribution(0.25, 0, 3, saturation(2.0));
    let post = blended(&base, &[a, b]);
    let expected = base.saturation + 0.5 * (1.5 - base.saturation);
    assert!(
        (post.saturation - expected).abs() < 1.0e-6,
        "{} vs {expected}",
        post.saturation
    );
}

#[test]
fn white_balance_blends_in_mired() {
    let base = PostSettings::default();
    let overrides = PostOverrides {
        white_balance_kelvin: Some(3200.0),
        ..PostOverrides::default()
    };
    let post = blended(&base, &[contribution(0.5, 0, 1, overrides)]);
    let mired = 0.5 * (1.0e6 / 6500.0 + 1.0e6 / 3200.0);
    let expected = 1.0e6 / mired;
    assert!(
        (post.grade.white_balance_kelvin - expected).abs() < 0.5,
        "{} vs {expected}",
        post.grade.white_balance_kelvin
    );
}

#[test]
fn the_tonemap_switches_once_at_half_weight() {
    let base = PostSettings::default();
    let neutral = PostOverrides {
        tonemap: Some(TonemapOperator::Neutral),
        ..PostOverrides::default()
    };
    assert_eq!(
        blended(&base, &[contribution(0.49, 0, 1, neutral)]).tonemap,
        TonemapOperator::AgX
    );
    assert_eq!(
        blended(&base, &[contribution(0.5, 0, 1, neutral)]).tonemap,
        TonemapOperator::Neutral
    );
    // Inside a tier the largest weight names the operator; a tie goes to the lowest key.
    let aces = PostOverrides {
        tonemap: Some(TonemapOperator::AcesFitted),
        ..PostOverrides::default()
    };
    let post = blended(
        &base,
        &[contribution(0.4, 0, 9, aces), contribution(0.4, 0, 2, neutral)],
    );
    assert_eq!(post.tonemap, TonemapOperator::Neutral);
}

#[test]
fn non_finite_values_and_weights_are_ignored() {
    let base = PostSettings::default();
    assert_eq!(
        blended(&base, &[contribution(1.0, 0, 1, saturation(f32::NAN))]),
        base
    );
    assert_eq!(
        blended(&base, &[contribution(f32::NAN, 0, 1, saturation(0.1))]),
        base
    );
    assert_eq!(blended(&base, &[contribution(-1.0, 0, 1, saturation(0.1))]), base);
    let lift = PostOverrides {
        lift: Some([0.1, f32::INFINITY, 0.0]),
        ..PostOverrides::default()
    };
    assert_eq!(blended(&base, &[contribution(1.0, 0, 1, lift)]), base);
}

#[test]
fn results_are_sanitised_after_the_blend() {
    let base = PostSettings::default();
    let wild = PostOverrides {
        contrast: Some(-3.0),
        saturation: Some(-1.0),
        vignette: Some(4.0),
        film_grain: Some(-2.0),
        bloom_intensity: Some(9.0),
        meter_center_weight: Some(3.0),
        ao_intensity: Some(100.0),
        fog_tint: Some([-1.0, 0.5, 2.0]),
        white_balance_kelvin: Some(100.0),
        gamma: Some([0.0, 1.0, 1.0]),
        gain: Some([-1.0, 1.0, 1.0]),
        ..PostOverrides::default()
    };
    let post = blended(&base, &[contribution(1.0, 0, 1, wild)]);
    assert_eq!(post.contrast, 0.05);
    assert_eq!(post.saturation, 0.0);
    assert_eq!(post.vignette, 1.0);
    assert_eq!(post.film_grain, 0.0);
    assert_eq!(post.bloom_intensity, 1.0);
    assert_eq!(post.meter.center_weight, 1.0);
    assert_eq!(post.ao_intensity, 4.0);
    assert_eq!(post.fog_tint, [0.0, 0.5, 2.0]);
    assert_eq!(post.grade.white_balance_kelvin, 1667.0);
    assert_eq!(post.grade.gamma[0], 0.05);
    assert_eq!(post.grade.gain[0], 0.0);
}

#[test]
fn local_exposure_starts_from_the_identity_and_ev_bands_blend() {
    let base = PostSettings::default();
    assert_eq!(base.local_exposure, None);
    let overrides = PostOverrides {
        local_shadow_contrast: Some(0.5),
        exposure_min_ev100: Some(4.0),
        exposure_max_ev100: Some(8.0),
        meter_center_weight: Some(0.5),
        ..PostOverrides::default()
    };
    let post = blended(&base, &[contribution(1.0, 0, 1, overrides)]);
    assert_eq!(
        post.local_exposure,
        Some(LocalExposure {
            shadow_contrast: 0.5,
            ..LocalExposure::default()
        })
    );
    assert_eq!(post.auto_exposure_min_ev100, 4.0);
    assert_eq!(post.auto_exposure_max_ev100, 8.0);
    assert_eq!(post.meter.center_weight, 0.5);
    // A volume that brings local exposure back to the identity turns the pass off.
    let identity = PostOverrides {
        local_shadow_contrast: Some(1.0),
        ..PostOverrides::default()
    };
    let with_local = PostSettings {
        local_exposure: Some(LocalExposure {
            shadow_contrast: 0.8,
            ..LocalExposure::default()
        }),
        ..PostSettings::default()
    };
    assert_eq!(
        blended(&with_local, &[contribution(1.0, 0, 1, identity)]).local_exposure,
        None
    );
}

#[test]
fn the_blend_is_continuous_across_a_band() {
    let volume = PostVolume {
        overrides: PostOverrides {
            exposure_compensation: Some(3.0),
            saturation: Some(0.0),
            fog_tint: Some([1.0, 0.6, 0.3]),
            ..PostOverrides::default()
        },
        ..boxed([2.0, 2.0, 2.0], 4.0)
    };
    let world = at([0.0; 3]);
    let base = PostSettings::default();
    let mut scratch = Vec::new();
    let mut last: Option<PostSettings> = None;
    // 1 cm steps from 8 m outside to the centre: no field jumps by more than a smooth step.
    for step in 0..=800 {
        let x = 8.0 - step as f32 * 0.01;
        let entity = Entity::from_raw_u32(5).expect("entity index");
        gather_contributions([(entity, &volume, &world)], [x, 0.0, 0.0], &mut scratch);
        let mut post = base.clone();
        blend_post_volumes(&mut post, &mut scratch);
        if let Some(prev) = &last {
            assert!(
                (post.exposure_compensation - prev.exposure_compensation).abs() < 0.02,
                "jump at x = {x}"
            );
            assert!(
                (post.saturation - prev.saturation).abs() < 0.01,
                "jump at x = {x}"
            );
            assert!(
                (post.fog_tint[2] - prev.fog_tint[2]).abs() < 0.01,
                "jump at x = {x}"
            );
        }
        last = Some(post);
    }
    let inside = last.expect("walked");
    assert_eq!(inside.exposure_compensation, 3.0);
    assert_eq!(inside.saturation, 0.0);
}

#[test]
fn gather_keeps_only_contributing_supported_volumes() {
    let world = at([0.0; 3]);
    let near = boxed([1.0, 1.0, 1.0], 0.0);
    let far = PostVolume {
        priority: 4,
        ..boxed([1.0, 1.0, 1.0], 0.0)
    };
    let far_world = at([50.0, 0.0, 0.0]);
    let newer = PostVolume {
        format_version: POST_VOLUME_FORMAT_VERSION + 1,
        shape: PostVolumeShape::Unbound,
        ..PostVolume::default()
    };
    let e = |i: u32| Entity::from_raw_u32(i).expect("entity index");
    let mut out = vec![contribution(1.0, 0, 0, PostOverrides::default())];
    gather_contributions(
        [
            (e(1), &near, &world),
            (e(2), &far, &far_world),
            (e(3), &newer, &world),
        ],
        [0.0; 3],
        &mut out,
    );
    assert_eq!(
        out.len(),
        1,
        "the old contents are cleared, far and newer skipped"
    );
    assert_eq!(out[0].key, e(1).to_bits());
    assert_eq!(out[0].weight, 1.0);
}

#[test]
fn ao_and_fog_tint_default_to_neutral() {
    let post = PostSettings::default();
    assert_eq!(post.ao_intensity, 1.0);
    assert_eq!(post.fog_tint, [1.0; 3]);
    assert_eq!(PostSettings::golden_deterministic().ao_intensity, 1.0);
    assert_eq!(PostSettings::golden_deterministic().fog_tint, [1.0; 3]);
    let fog = FogSettings::aerial();
    assert_eq!(
        post.tinted_fog(fog),
        fog,
        "the neutral tint leaves the fog bit-identical"
    );
    let warm = PostSettings {
        fog_tint: [1.0, 0.5, 0.25],
        ..PostSettings::default()
    };
    let tinted = warm.tinted_fog(fog);
    assert_eq!(
        tinted.color,
        [fog.color[0], fog.color[1] * 0.5, fog.color[2] * 0.25]
    );
    assert_eq!(tinted.density, fog.density);
}

#[test]
fn format_version_migrates_and_newer_is_refused() {
    let current = PostVolume::default();
    assert_eq!(current.format_version, POST_VOLUME_FORMAT_VERSION);
    assert_eq!(
        current.migrate().map(|v| v.format_version),
        Ok(POST_VOLUME_FORMAT_VERSION)
    );
    let hand_written = PostVolume {
        format_version: 0,
        ..PostVolume::default()
    };
    assert!(hand_written.is_supported());
    assert_eq!(
        hand_written.migrate().map(|v| v.format_version),
        Ok(POST_VOLUME_FORMAT_VERSION)
    );
    // Version 2 (ADR-1863) only added the optional lens-effect overrides: a version-1 volume
    // upgrades with its overrides untouched and the new ones unset. Version 3 (ADR-2323) only
    // added the convex-hull shape and the optional LUT, so the same holds for it.
    assert_eq!(POST_VOLUME_FORMAT_VERSION, 3);
    let v1 = PostVolume {
        format_version: 1,
        overrides: saturation(1.2),
        ..PostVolume::default()
    };
    let upgraded = v1.migrate().expect("v1 upgrades");
    assert_eq!(upgraded.format_version, POST_VOLUME_FORMAT_VERSION);
    assert_eq!(upgraded.overrides, saturation(1.2));
    assert_eq!(upgraded.overrides.lens_flare_intensity, None);
    let newer = PostVolume {
        format_version: POST_VOLUME_FORMAT_VERSION + 1,
        ..PostVolume::default()
    };
    assert!(!newer.is_supported());
    let err = newer.migrate().expect_err("newer refused");
    assert!(err.to_string().contains("newer"), "{err}");
    assert_eq!(post_volume_migration_sources(), vec![0, 1, 2]);
}

#[test]
fn the_fingerprint_names_every_field_and_variant() {
    let fingerprint = post_volume_schema_fingerprint();
    for needle in [
        "format_version:u32",
        "blend_distance:f32",
        "priority:i32",
        "Box(",
        "Sphere(",
        "Unbound",
        "exposure_compensation",
        "white_balance_kelvin",
        "fog_tint",
        "ao_intensity",
        "eye",
        "lens_flare_intensity",
        "chromatic_aberration",
        "ev_min:f32",
        "adapt_down_speed:f32",
        "AgX",
        "AcesFitted",
        "Neutral",
    ] {
        assert!(
            fingerprint.contains(needle),
            "fingerprint lacks {needle}: {fingerprint}"
        );
    }
    for name in PostOverrides::FIELDS {
        assert!(
            fingerprint.contains(&format!("{name}:")),
            "fingerprint lacks {name}"
        );
    }
}

#[test]
fn overrides_report_the_fields_they_set() {
    let overrides = PostOverrides {
        contrast: Some(1.0),
        fog_tint: Some([1.0; 3]),
        eye: Some(EyeRange::default()),
        ..PostOverrides::default()
    };
    assert_eq!(overrides.set_fields(), vec!["contrast", "fog_tint", "eye"]);
    // 21 fields in format v1, plus the four lens-effect overrides of v2 (ADR-1863).
    assert_eq!(PostOverrides::FIELDS.len(), 25);
    let lens = PostOverrides {
        lens_flare_intensity: Some(1.0),
        chromatic_aberration: Some(0.002),
        ..PostOverrides::default()
    };
    assert_eq!(
        lens.set_fields(),
        vec!["lens_flare_intensity", "chromatic_aberration"]
    );
}

#[test]
fn a_volume_eye_becomes_an_exposure_zone() {
    let volume = PostVolume {
        priority: 3,
        overrides: PostOverrides {
            eye: Some(EyeRange {
                ev_min: 6.0,
                ev_max: 8.0,
                adapt_up_speed: 1.0,
                adapt_down_speed: 0.5,
            }),
            ..PostOverrides::default()
        },
        ..PostVolume::default()
    };
    let zone = volume.eye_zone().expect("an eye range");
    assert_eq!((zone.ev_min, zone.ev_max), (6.0, 8.0));
    assert_eq!((zone.adapt_up_speed, zone.adapt_down_speed), (1.0, 0.5));
    assert_eq!(zone.priority, 3);
    assert!(PostVolume::default().eye_zone().is_none());
}

#[test]
fn scene_files_round_trip_a_post_volume() {
    use regia_core::id::Guid;
    use regia_scene::components::{EntityGuid, Name};
    use regia_scene::format::{SCENE_FORMAT_VERSION, SceneFile};
    use regia_scene::spawn::{SnapshotFilter, SpawnOptions, snapshot_scene, spawn_scene};

    fn scene_world() -> World {
        let mut world = World::new();
        world.insert_resource(AppTypeRegistry::default());
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        regia_scene::components::register_primitives(&mut registry);
        registry.register::<Transform>();
        registry.register::<Name>();
        registry.register::<EntityGuid>();
        regia_render::post_volumes::register_post_volume_types(&mut registry);
        drop(registry);
        world
    }

    let guid = Guid::new_random();
    let text = format!(
        r#"(
    format_version: {SCENE_FORMAT_VERSION},
    entities: [
        (guid: "{guid}", name: "Cellar", components: {{
            "regia_scene::transform::Transform": (translation: (0.0, 1.0, 0.0)),
            "regia_render::post_volumes::PostVolume": (
                format_version: 1,
                enabled: true,
                shape: Box(half_extents: (6.0, 3.0, 10.0)),
                priority: 1,
                blend_distance: 2.0,
                weight: 1.0,
                overrides: (
                    exposure_compensation: Some(0.5),
                    tonemap: Some(Neutral),
                    white_balance_kelvin: Some(3600.0),
                    fog_tint: Some((1.0, 0.85, 0.7)),
                    eye: Some((ev_min: 6.0, ev_max: 8.0, adapt_up_speed: 2.0, adapt_down_speed: 1.5)),
                ),
            ),
        }}),
    ],
    resources: {{}},
)"#
    );
    let file = SceneFile::from_ron_str(&text).expect("scene parses");
    let mut world = scene_world();
    let spawned = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("spawn");
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
    let mut query = world.query::<&PostVolume>();
    let volume = query.single(&world).expect("one volume").clone();
    assert_eq!(
        volume.shape,
        PostVolumeShape::Box {
            half_extents: [6.0, 3.0, 10.0]
        }
    );
    assert_eq!(volume.priority, 1);
    assert_eq!(volume.overrides.exposure_compensation, Some(0.5));
    assert_eq!(volume.overrides.tonemap, Some(TonemapOperator::Neutral));
    assert_eq!(volume.overrides.fog_tint, Some([1.0, 0.85, 0.7]));
    assert_eq!(volume.overrides.saturation, None, "unset fields stay unset");
    assert_eq!(volume.overrides.eye.map(|e| e.ev_max), Some(8.0));
    // A version-1 volume (before the lens overrides, ADR-1863) loads with them unset.
    assert_eq!(volume.format_version, 1);
    assert_eq!(volume.overrides.lens_flare_intensity, None);
    assert_eq!(volume.overrides.chromatic_aberration, None);
    assert_eq!(
        volume.clone().migrate().map(|v| v.format_version),
        Ok(POST_VOLUME_FORMAT_VERSION)
    );

    // Save and load again: the same component, and byte-identical scene text.
    let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
    let saved = snapshot.to_ron_string().expect("write");
    let mut again = scene_world();
    spawn_scene(&mut again, &snapshot, &SpawnOptions::default()).expect("respawn");
    let mut query = again.query::<&PostVolume>();
    assert_eq!(*query.single(&again).expect("one volume"), volume);
    let resaved = snapshot_scene(&mut again, &SnapshotFilter::default())
        .expect("snapshot 2")
        .to_ron_string()
        .expect("write 2");
    assert_eq!(resaved, saved);
}

/// `extract_frame` with two window cameras and a texture camera; returns the extracted views'
/// post settings in spawn order of `eyes`.
fn extract_views(eyes: &[([f32; 3], RenderTarget)], volumes: &[(PostVolume, [f32; 3])]) -> Vec<PostSettings> {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.insert_resource(RenderSettings::default());
    for (volume, position) in volumes {
        app.world_mut().spawn((volume.clone(), at(*position)));
    }
    for (order, (eye, target)) in eyes.iter().enumerate() {
        app.world_mut().spawn((
            at(*eye),
            Camera {
                order: order as i32,
                target: target.clone(),
                ..Camera::default()
            },
        ));
    }
    app.add_systems(Render, extract_frame);
    app.update();
    let frame = app.world().resource::<ExtractedFrame>();
    assert_eq!(frame.views.len(), eyes.len());
    frame.views.iter().map(|v| v.post.clone()).collect()
}

#[test]
fn every_camera_blends_the_volumes_at_its_own_eye() {
    let cellar = PostVolume {
        priority: 1,
        overrides: PostOverrides {
            saturation: Some(0.25),
            ao_intensity: Some(2.0),
            ..PostOverrides::default()
        },
        ..boxed([3.0, 3.0, 3.0], 0.0)
    };
    let world_look = PostVolume {
        shape: PostVolumeShape::Unbound,
        overrides: PostOverrides {
            saturation: Some(1.0),
            contrast: Some(1.1),
            ..PostOverrides::default()
        },
        ..PostVolume::default()
    };
    let posts = extract_views(
        &[
            ([0.0, 0.0, 0.0], RenderTarget::Window),
            ([30.0, 0.0, 0.0], RenderTarget::Window),
            ([1.0, 1.0, 1.0], RenderTarget::Texture { asset_id: 9 }),
        ],
        &[(cellar, [0.0; 3]), (world_look, [0.0; 3])],
    );
    // Inside the cellar: its priority beats the unbound look for saturation; contrast comes from
    // the unbound look (the cellar does not set it).
    assert_eq!(posts[0].saturation, 0.25);
    assert_eq!(posts[0].contrast, 1.1);
    assert_eq!(posts[0].ao_intensity, 2.0);
    // Outside: only the unbound look.
    assert_eq!(posts[1].saturation, 1.0);
    assert_eq!(posts[1].contrast, 1.1);
    assert_eq!(posts[1].ao_intensity, 1.0);
    // A texture view inside the cellar gets the cellar too.
    assert_eq!(posts[2].saturation, 0.25);
}

#[test]
fn a_scene_without_volumes_extracts_the_same_post_as_before() {
    let posts = extract_views(&[([0.0; 3], RenderTarget::Window)], &[]);
    assert_eq!(posts[0], RenderSettings::default().resolve_post());
}

#[test]
fn the_frame_scene_carries_the_view_fog_tint() {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.insert_resource(RenderSettings {
        fog: FogSettings::outdoor_haze(),
        ..RenderSettings::default()
    });
    let tint = PostVolume {
        shape: PostVolumeShape::Unbound,
        overrides: PostOverrides {
            fog_tint: Some([1.0, 0.5, 0.25]),
            ..PostOverrides::default()
        },
        ..PostVolume::default()
    };
    app.world_mut().spawn((tint, at([0.0; 3])));
    app.world_mut().spawn((at([0.0, 1.0, 0.0]), Camera::default()));
    app.add_systems(Render, extract_frame);
    app.update();
    let frame = app.world().resource::<ExtractedFrame>().frame_scene(0, 32, 32);
    let haze = FogSettings::outdoor_haze().color;
    assert_eq!(frame.fog.color, [haze[0], haze[1] * 0.5, haze[2] * 0.25]);
    assert_eq!(frame.post.map(|p| p.fog_tint), Some([1.0, 0.5, 0.25]));
}

/// The lens-effect overrides (format v2, ADR-1863) blend like every scalar: toward the tier's
/// mean by the tier's weight, from the camera's strengths, clamped to each field's range.
#[test]
fn lens_effect_overrides_blend_from_the_cameras_strengths() {
    let mut base = PostSettings::default();
    regia_render::LensEffects::CINEMATIC.apply_camera(&mut base);
    let fringe = PostOverrides {
        chromatic_aberration: Some(0.01),
        lens_flare_intensity: Some(3.0),
        ..PostOverrides::default()
    };
    let full = blended(&base, &[contribution(1.0, 0, 1, fringe)]);
    assert_eq!(full.lens_effects.chromatic_aberration, 0.01);
    assert_eq!(full.lens_effects.flare_intensity, 3.0);
    assert_eq!(
        full.lens_effects.dirt_intensity, base.lens_effects.dirt_intensity,
        "unset fields keep the camera's value"
    );
    let half = blended(&base, &[contribution(0.5, 0, 1, fringe)]);
    let expected = 0.5 * (base.lens_effects.chromatic_aberration + 0.01);
    assert!((half.lens_effects.chromatic_aberration - expected).abs() < 1e-7);
    // A volume can turn the effects on over a view that asked for none, and out-of-range
    // values clamp.
    let wild = PostOverrides {
        chromatic_aberration: Some(9.0),
        lens_dirt_intensity: Some(-1.0),
        lens_flare_threshold: Some(2.0),
        ..PostOverrides::default()
    };
    let off = blended(&PostSettings::default(), &[contribution(1.0, 0, 1, wild)]);
    assert_eq!(
        off.lens_effects.chromatic_aberration,
        regia_render::lens_effects::CHROMATIC_ABERRATION_RANGE.1
    );
    assert_eq!(off.lens_effects.dirt_intensity, 0.0);
    assert_eq!(off.lens_effects.flare_threshold, 2.0);
    assert!(off.lens_effects.is_active());
    assert_eq!(
        wild.set_fields(),
        vec![
            "lens_flare_threshold",
            "lens_dirt_intensity",
            "chromatic_aberration"
        ]
    );
}
