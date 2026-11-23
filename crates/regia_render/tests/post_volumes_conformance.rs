// SPDX-License-Identifier: MPL-2.0
//! Spec conformance for `docs/specs/30-post-volumes-and-physical-camera.md` (lane
//! spec-12-29-spacemouse, ADR-5631): the numbers and rules of the spec that no other test asserted
//! (the defaults tables, the blend's clamps and when they run, the kelvin rule, the EV input floors,
//! the sphere's scale rule, the hull and outline boundaries). CPU only (no device).

use regia_core::math::{Affine3A, Quat, Vec3};
use regia_render::post_volume_shapes::{MAX_HULL_PLANES, SPHERE_OUTLINE_SEGMENTS, hull_planes_valid};
use regia_render::post_volumes::{
    POST_VOLUME_FORMAT_VERSION, PostOverrides, PostVolume, PostVolumeShape, VolumeContribution,
    blend_post_volumes,
};
use regia_render::{
    CameraExposureMode, LensEffects, PhysicalCamera, PostSettings, ev100_from_camera, post_volume_outline,
};
use regia_scene::GlobalTransform;

fn contribution(weight: f32, priority: i32, key: u64, overrides: PostOverrides) -> VolumeContribution {
    VolumeContribution {
        weight,
        priority,
        key,
        overrides,
    }
}

fn blended(base: &PostSettings, contributions: &[VolumeContribution]) -> PostSettings {
    let mut post = base.clone();
    let mut list = contributions.to_vec();
    blend_post_volumes(&mut post, &mut list);
    post
}

fn scaled(scale: [f32; 3]) -> GlobalTransform {
    GlobalTransform(Affine3A::from_scale_rotation_translation(
        Vec3::from_array(scale),
        Quat::IDENTITY,
        Vec3::ZERO,
    ))
}

/// § 2.1, § 2.3, § 2.4 and § 9: every default in the spec's tables.
#[test]
fn defaults_match_the_spec_tables() {
    let volume = PostVolume::default();
    assert_eq!(volume.format_version, POST_VOLUME_FORMAT_VERSION);
    assert_eq!(POST_VOLUME_FORMAT_VERSION, 3, "§ 2.1: format v3 since ADR-2323");
    assert!(volume.enabled);
    assert_eq!(
        volume.shape,
        PostVolumeShape::Box {
            half_extents: [1.0, 1.0, 1.0]
        }
    );
    assert_eq!(volume.priority, 0);
    assert_eq!(volume.blend_distance, 0.0);
    assert_eq!(volume.weight, 1.0);
    assert_eq!(volume.overrides, PostOverrides::default());
    assert_eq!(volume.lut, None);

    let post = PostSettings::default();
    assert_eq!(post.ao_intensity, 1.0, "§ 2.3");
    assert_eq!(post.fog_tint, [1.0, 1.0, 1.0], "§ 2.3");
    assert_eq!(post.lens_effects, LensEffects::OFF, "§ 9: off by default");

    let lens = PhysicalCamera::default();
    assert_eq!(lens.exposure, CameraExposureMode::Inherit, "§ 2.4");
    assert_eq!(lens.iso, 100.0);
    assert_eq!(lens.shutter_seconds, 1.0 / 60.0);
    assert_eq!(lens.shutter_reference_fps, 30.0);
    assert_eq!(lens.focal_length, None);

    let off = LensEffects::OFF;
    assert_eq!(LensEffects::default(), off);
    assert_eq!(
        (off.flare_intensity, off.dirt_intensity, off.chromatic_aberration),
        (0.0, 0.0, 0.0),
        "§ 9: every strength 0"
    );
}

/// § 2.1: a blend distance of 0, a negative one or one that is not finite is a hard edge.
#[test]
fn a_negative_or_non_finite_blend_distance_is_a_hard_edge() {
    let world = GlobalTransform::IDENTITY;
    for blend in [0.0, -2.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let volume = PostVolume {
            shape: PostVolumeShape::Box {
                half_extents: [1.0, 1.0, 1.0],
            },
            blend_distance: blend,
            ..PostVolume::default()
        };
        assert_eq!(
            volume.weight_at(&world, [0.5, 0.0, 0.0]),
            1.0,
            "blend {blend}: inside"
        );
        assert_eq!(
            volume.weight_at(&world, [1.0, 0.0, 0.0]),
            1.0,
            "blend {blend}: on the surface"
        );
        assert_eq!(
            volume.weight_at(&world, [1.001, 0.0, 0.0]),
            0.0,
            "blend {blend}: just outside"
        );
    }
}

/// § 3.1: the sphere's radius scales with the mean of the entity's X and Z scale, and it stays a
/// sphere in world meters (the Y scale does not stretch it).
#[test]
fn a_sphere_scales_with_the_mean_of_x_and_z_and_stays_round() {
    let world = scaled([2.0, 1.0, 4.0]); // radius 2 · (2 + 4) / 2 = 6 m
    let volume = PostVolume {
        shape: PostVolumeShape::Sphere { radius: 2.0 },
        blend_distance: 2.0,
        ..PostVolume::default()
    };
    for direction in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]] {
        let at = |d: f32| direction.map(|c: f32| c * d);
        assert_eq!(volume.weight_at(&world, at(5.99)), 1.0, "{direction:?}: inside");
        let mid = volume.weight_at(&world, at(7.0));
        assert!(
            (mid - 0.5).abs() < 1.0e-3,
            "{direction:?}: middle of the band: {mid}"
        );
        assert_eq!(
            volume.weight_at(&world, at(8.01)),
            0.0,
            "{direction:?}: past the band"
        );
    }
}

/// § 3.4 and the code's extra ranges (ADR-5631): every clamp the blend applies, at full weight.
#[test]
fn the_blend_clamps_every_field_into_its_range() {
    let high = PostOverrides {
        exposure_compensation: Some(1000.0),
        exposure_min_ev100: Some(-1000.0),
        exposure_max_ev100: Some(1000.0),
        meter_center_weight: Some(3.0),
        contrast: Some(0.0),
        saturation: Some(-1.0),
        bloom_intensity: Some(5.0),
        white_balance_kelvin: Some(90_000.0),
        tint: Some(7.0),
        lift: Some([-3.0, 0.5, 3.0]),
        gamma: Some([0.0, 1.0, 9.0]),
        gain: Some([-1.0, 1.0, 2.0]),
        vignette: Some(-1.0),
        film_grain: Some(2.0),
        local_shadow_contrast: Some(5.0),
        local_highlight_contrast: Some(-1.0),
        local_detail_strength: Some(9.0),
        ao_intensity: Some(9.0),
        fog_tint: Some([-1.0, 0.5, 3.0]),
        lens_flare_intensity: Some(1.0e9),
        lens_flare_threshold: Some(1.0e9),
        lens_dirt_intensity: Some(1.0e3),
        chromatic_aberration: Some(1.0),
        ..PostOverrides::default()
    };
    let post = blended(&PostSettings::default(), &[contribution(1.0, 0, 1, high)]);
    assert_eq!(post.exposure_compensation, 64.0, "EV fields are kept within ±64");
    assert_eq!(post.auto_exposure_min_ev100, -64.0);
    assert_eq!(post.auto_exposure_max_ev100, 64.0);
    assert_eq!(post.meter.center_weight, 1.0);
    assert_eq!(post.contrast, 0.05);
    assert_eq!(post.saturation, 0.0);
    assert_eq!(post.bloom_intensity, 1.0);
    assert!(
        (post.grade.white_balance_kelvin - 25_000.0).abs() < 0.5,
        "{}",
        post.grade.white_balance_kelvin
    );
    assert_eq!(post.grade.tint, 1.0, "tint −1..1");
    assert_eq!(post.grade.lift, [-1.0, 0.5, 1.0], "lift −1..1 per channel");
    assert_eq!(post.grade.gamma, [0.05, 1.0, 9.0]);
    assert_eq!(post.grade.gain, [0.0, 1.0, 2.0]);
    assert_eq!(post.vignette, 0.0);
    assert_eq!(post.film_grain, 1.0);
    let local = post
        .local_exposure
        .expect("a non-identity local exposure stays on");
    assert_eq!(
        (
            local.shadow_contrast,
            local.highlight_contrast,
            local.detail_strength
        ),
        (1.0, 0.0, 4.0)
    );
    assert_eq!(post.ao_intensity, 4.0);
    assert_eq!(post.fog_tint, [0.0, 0.5, 3.0]);
    assert_eq!(post.lens_effects.flare_intensity, 1000.0, "§ 9: 0–1000");
    assert_eq!(post.lens_effects.flare_threshold, 65_504.0, "§ 9: 0–65 504");
    assert_eq!(post.lens_effects.dirt_intensity, 10.0, "§ 9: 0–10");
    assert_eq!(post.lens_effects.chromatic_aberration, 0.05, "§ 9: 0–0.05");

    let low = PostOverrides {
        white_balance_kelvin: Some(100.0),
        ..PostOverrides::default()
    };
    let post = blended(&PostSettings::default(), &[contribution(1.0, 0, 1, low)]);
    assert!(
        (post.grade.white_balance_kelvin - 1667.0).abs() < 0.05,
        "{}",
        post.grade.white_balance_kelvin
    );
}

/// § 3.4 as built (ADR-5631): a field is clamped right after each tier that sets it, so a later
/// partial tier starts from the clamped value; a field no volume sets keeps its base value as is.
#[test]
fn sanitising_runs_after_each_tier_that_sets_the_field() {
    let below = PostOverrides {
        contrast: Some(-3.0),
        ..PostOverrides::default()
    };
    let normal = PostOverrides {
        contrast: Some(1.0),
        ..PostOverrides::default()
    };
    let post = blended(
        &PostSettings::default(),
        &[contribution(1.0, 0, 1, below), contribution(0.5, 1, 2, normal)],
    );
    // Tier 0 clamps −3 to 0.05; tier 1 moves half way to 1.0: 0.525 (not 0.05 from −1).
    assert!((post.contrast - 0.525).abs() < 1.0e-6, "{}", post.contrast);

    let base = PostSettings {
        contrast: 0.0,
        ..PostSettings::default()
    };
    let saturation_only = PostOverrides {
        saturation: Some(1.2),
        ..PostOverrides::default()
    };
    let post = blended(&base, &[contribution(1.0, 0, 1, saturation_only)]);
    assert_eq!(post.contrast, 0.0, "untouched fields are not re-sanitised");
    assert_eq!(post.saturation, 1.2);
}

/// § 2.2 / § 3.4 as built (ADR-5631): each volume's kelvin is clamped into 1667–25 000 before the
/// mired mean; a kelvin of 0 or below is ignored like a non-finite one.
#[test]
fn kelvin_is_clamped_per_volume_before_the_mired_mean() {
    let kelvin = |k: f32| PostOverrides {
        white_balance_kelvin: Some(k),
        ..PostOverrides::default()
    };
    let post = blended(
        &PostSettings::default(),
        &[
            contribution(1.0, 0, 1, kelvin(100.0)),
            contribution(1.0, 0, 2, kelvin(6500.0)),
        ],
    );
    let want = 1.0e6 / (0.5 * (1.0e6 / 1667.0 + 1.0e6 / 6500.0));
    assert!(
        (post.grade.white_balance_kelvin - want).abs() < 0.5,
        "{} vs {want}",
        post.grade.white_balance_kelvin
    );
    let base = PostSettings::default();
    for ignored in [0.0, -500.0] {
        let post = blended(&base, &[contribution(1.0, 0, 1, kelvin(ignored))]);
        assert_eq!(
            post.grade.white_balance_kelvin, base.grade.white_balance_kelvin,
            "{ignored} K"
        );
    }
}

/// § 5 as built: the EV100 formula floors its inputs (f-stop ≥ 0.1, shutter ≥ 1 µs, ISO ≥ 1), so a
/// zeroed or negative lens gives a finite exposure, never a NaN or an infinity.
#[test]
fn ev100_floors_its_inputs_and_stays_finite() {
    let floor = ev100_from_camera(0.1, 1.0e-6, 1.0);
    assert!(floor.is_finite());
    assert_eq!(ev100_from_camera(0.0, 0.0, 0.0), floor);
    assert_eq!(ev100_from_camera(-4.0, -1.0, -100.0), floor);
    // Sunny 16 still holds above the floors.
    assert!((ev100_from_camera(16.0, 1.0 / 125.0, 100.0) - 15.0).abs() < 0.05);
}

/// § 2.3: the SSAO strength factor is `ao_intensity` clamped to 0–4; a non-finite value is 1.
#[test]
fn ao_intensity_scale_clamps_and_ignores_non_finite_values() {
    let scale = |ao_intensity: f32| {
        PostSettings {
            ao_intensity,
            ..PostSettings::default()
        }
        .ao_intensity_scale()
    };
    assert_eq!(scale(1.0), 1.0);
    assert_eq!(scale(2.5), 2.5);
    assert_eq!(scale(9.0), 4.0);
    assert_eq!(scale(-1.0), 0.0);
    assert_eq!(scale(f32::NAN), 1.0);
    assert_eq!(scale(f32::INFINITY), 1.0);
}

/// § 3.4: an auto-exposure band whose ends cross is used as the ordered band.
#[test]
fn a_crossed_exposure_band_is_used_in_order() {
    let band = |min: f32, max: f32| PostSettings {
        auto_exposure_min_ev100: min,
        auto_exposure_max_ev100: max,
        ..PostSettings::default()
    };
    let (ordered, crossed) = (band(2.0, 10.0), band(10.0, 2.0));
    for luminance in [1.0e-4, 0.05, 1.0, 40.0, 1.0e5] {
        assert_eq!(
            crossed.auto_exposure_ev100(luminance),
            ordered.auto_exposure_ev100(luminance),
            "luminance {luminance}"
        );
    }
}

/// § 9: `LensEffects::sanitized` clamps each strength to its range (upper bounds included) and
/// replaces a non-finite value with the off value.
#[test]
fn lens_effects_sanitize_to_the_spec_ranges() {
    let wild = LensEffects {
        flare_intensity: 1.0e9,
        flare_threshold: 1.0e9,
        coating: 3.0,
        dirt_intensity: 1.0e3,
        chromatic_aberration: 1.0,
        ..LensEffects::OFF
    }
    .sanitized();
    assert_eq!(
        (
            wild.flare_intensity,
            wild.flare_threshold,
            wild.coating,
            wild.dirt_intensity,
            wild.chromatic_aberration
        ),
        (1000.0, 65_504.0, 1.0, 10.0, 0.05)
    );
    let nan = LensEffects {
        flare_intensity: f32::NAN,
        flare_threshold: f32::NAN,
        coating: f32::NAN,
        dirt_intensity: f32::INFINITY,
        chromatic_aberration: f32::NEG_INFINITY,
        ..LensEffects::OFF
    }
    .sanitized();
    assert_eq!(nan, LensEffects::OFF);
}

/// § 10.1: 1 to 16 planes; zero, seventeen, a non-finite value or a normal shorter than 1e-6 make
/// the hull invalid.
#[test]
fn hull_validity_follows_the_plane_count_and_normal_rules() {
    assert_eq!(MAX_HULL_PLANES, 16);
    let plane = [0.0, 1.0, 0.0, 1.0];
    assert!(!hull_planes_valid(&[]));
    assert!(hull_planes_valid(&[plane]));
    assert!(hull_planes_valid(&[plane; 16]));
    assert!(!hull_planes_valid(&[plane; 17]));
    assert!(!hull_planes_valid(&[[0.0, f32::NAN, 0.0, 1.0]]));
    assert!(!hull_planes_valid(&[[0.0, 1.0, 0.0, f32::INFINITY]]));
    assert!(!hull_planes_valid(&[[0.0, 1.0e-7, 0.0, 1.0]]));
    assert!(
        hull_planes_valid(&[[0.0, 1.0e-5, 0.0, 1.0]]),
        "any length above the floor"
    );
}

/// § 10.4: a hull of fewer than four planes is unbounded (`bounded` false, the rays not drawn), the
/// sphere uses 24 segments per great circle, and unbound or invalid shapes have no outline.
#[test]
fn outlines_mark_open_hulls_and_skip_unbound_and_invalid_shapes() {
    assert_eq!(SPHERE_OUTLINE_SEGMENTS, 24);
    let corner = PostVolume {
        shape: PostVolumeShape::ConvexHull {
            planes: vec![[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]],
        },
        blend_distance: 0.5,
        ..PostVolume::default()
    };
    // Three planes meet at one vertex; every edge line runs to infinity.
    let outline =
        post_volume_outline(&corner, &GlobalTransform::IDENTITY).expect("a valid open hull has an outline");
    assert!(!outline.bounded, "a three-plane hull is open: {outline:?}");
    for shape in [
        PostVolumeShape::Unbound,
        PostVolumeShape::ConvexHull { planes: Vec::new() },
        PostVolumeShape::ConvexHull {
            planes: vec![[0.0, 0.0, 0.0, 1.0]],
        },
    ] {
        let volume = PostVolume {
            shape: shape.clone(),
            ..PostVolume::default()
        };
        assert!(
            post_volume_outline(&volume, &GlobalTransform::IDENTITY).is_none(),
            "{shape:?} has no outline"
        );
    }
}
