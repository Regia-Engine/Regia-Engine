// SPDX-License-Identifier: MPL-2.0
//! `camera.raycast`: what is under a point of the camera's image. Turns a screen point into a
//! world ray with the camera's pose and lens, then casts it with the same CPU query as
//! `world.pick` (terrain and mesh layers). Needs no GPU and no window (book
//! `agents/camera-paths.md`).

use regia_app::prelude::*;
use regia_core::math::{Quat, Ray, Vec3};
use regia_platform::{PrimaryWindow, Window};
use regia_render::Camera;
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// The aspect ratio assumed for `u`/`v` points when neither `aspect` nor a window gives one.
const DEFAULT_ASPECT: f32 = 16.0 / 9.0;

/// The world ray through the point `(ndc_x, ndc_y)` of a camera's image, both in `-1..=1` with +Y
/// up on screen. `rotation` is the camera's world orientation (it looks down −Z); `aspect` is
/// width over height.
#[must_use]
pub(crate) fn screen_ray(
    eye: Vec3,
    rotation: Quat,
    camera: &Camera,
    aspect: f32,
    ndc_x: f32,
    ndc_y: f32,
) -> Ray {
    let right = rotation * Vec3::X;
    let up = rotation * Vec3::Y;
    let forward = rotation * Vec3::NEG_Z;
    if camera.orthographic {
        let half_height = camera.ortho_size;
        let origin = eye + right * (ndc_x * half_height * aspect) + up * (ndc_y * half_height);
        return Ray::new(origin, forward);
    }
    let half_height = (camera.fov_y * 0.5).tan();
    let direction = forward + right * (ndc_x * half_height * aspect) + up * (ndc_y * half_height);
    Ray::new(eye, direction)
}

fn number(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| Some(f as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a finite number"))),
    }
}

fn positive(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match number(params, name)? {
        Some(v) if v <= 0.0 => Err(RpcError::invalid_params(format!(
            "`{name}` must be greater than zero"
        ))),
        other => Ok(other),
    }
}

/// The screen point of a request, resolved against an image size.
struct ScreenPoint {
    ndc: (f32, f32),
    pixel: (f32, f32),
    uv: (f32, f32),
    size: Option<(f32, f32)>,
    aspect: f32,
}

/// Reads `x`/`y` (pixels) or `u`/`v` (0..1) plus the image size. `window` is the primary window's
/// size, used when the request gives no `width`/`height`.
fn screen_point(params: &Value, window: Option<(f32, f32)>) -> Result<ScreenPoint, RpcError> {
    let (x, y) = (number(params, "x")?, number(params, "y")?);
    let (u, v) = (number(params, "u")?, number(params, "v")?);
    let width = positive(params, "width")?;
    let height = positive(params, "height")?;
    let size = match (width, height) {
        (Some(w), Some(h)) => Some((w, h)),
        (None, None) => window,
        _ => {
            return Err(RpcError::invalid_params(
                "give both `width` and `height`, or neither",
            ));
        }
    };
    let aspect = match positive(params, "aspect")? {
        Some(a) => a,
        None => size.map_or(DEFAULT_ASPECT, |(w, h)| w / h),
    };
    let (uv, pixel) = match ((x, y), (u, v)) {
        ((Some(x), Some(y)), (None, None)) => {
            let (w, h) = size.ok_or_else(|| {
                RpcError::invalid_params(
                    "pixel `x`/`y` need `width` and `height` (the app has no primary window)",
                )
            })?;
            ((x / w, y / h), (x, y))
        }
        ((None, None), (Some(u), Some(v))) => {
            let pixel = size.map_or((u, v), |(w, h)| (u * w, v * h));
            ((u, v), pixel)
        }
        _ => {
            return Err(RpcError::invalid_params(
                "give a screen point as `x` and `y` (pixels, top-left origin) or `u` and `v` (0..1)",
            ));
        }
    };
    if !(0.0..=1.0).contains(&uv.0) || !(0.0..=1.0).contains(&uv.1) {
        return Err(RpcError::invalid_params(format!(
            "the point is outside the image (u {:.4}, v {:.4}; both must be within 0..=1)",
            uv.0, uv.1
        )));
    }
    Ok(ScreenPoint {
        ndc: (uv.0 * 2.0 - 1.0, 1.0 - uv.1 * 2.0),
        pixel,
        uv,
        size,
        aspect,
    })
}

/// The camera to cast from: `entity` when given, else the one with the highest `order` (ties to
/// the lowest entity id), the same choice shot sequences make.
fn camera_entity(world: &mut World, params: &Value) -> Result<Entity, RpcError> {
    if let Some(bits) = params.get("entity").filter(|v| !v.is_null()) {
        let bits = bits
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id"))?;
        let entity = Entity::try_from_bits(bits)
            .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
        return if world.get::<Camera>(entity).is_some() {
            Ok(entity)
        } else {
            Err(RpcError::not_found(
                format!("entity {bits} has no Camera"),
                "pass the id of an entity with a Camera component, or omit `entity` for the top camera",
            ))
        };
    }
    let mut query = world.query::<(Entity, &Camera)>();
    query
        .iter(world)
        .min_by_key(|(entity, camera)| (std::cmp::Reverse(camera.order), entity.to_bits()))
        .map(|(entity, _)| entity)
        .ok_or_else(|| {
            RpcError::not_found(
                "no Camera in this app",
                "spawn an entity with regia_render::Camera and a Transform",
            )
        })
}

fn window_size(world: &mut World) -> Option<(f32, f32)> {
    let mut query = world.query_filtered::<&Window, With<PrimaryWindow>>();
    let (w, h) = query.iter(world).next()?.physical_size;
    (w > 0 && h > 0).then_some((w as f32, h as f32))
}

/// `camera.raycast { x, y | u, v, width?, height?, entity?, ray_only?, ...pick filters }`.
///
/// `x`/`y` are pixels from the image's top-left corner (continuous: a pixel centre is at +0.5);
/// `u`/`v` are the same point as fractions of the image. The image size is `width`/`height`, else
/// the primary window. Returns the ray and, unless `ray_only`, what it hits (`world.pick`'s
/// `hit`/`hits`, `count`).
pub(crate) fn camera_raycast(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let window = window_size(world);
    let point = screen_point(params, window)?;
    let ray_only = match params.get("ray_only").filter(|v| !v.is_null()) {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`ray_only` must be a boolean"))?,
    };
    let entity = camera_entity(world, params)?;
    let camera = world
        .get::<Camera>(entity)
        .cloned()
        .ok_or_else(|| RpcError::internal("the camera vanished while resolving it"))?;
    // `Transform` requires a `GlobalTransform` that propagation fills in only at the end of a
    // frame, so a root camera's `Transform` is the fresher pose (a seek or a spawn is visible at
    // once). A parented camera needs the propagated world pose.
    let is_root = world.get::<ChildOf>(entity).is_none();
    let (eye, rotation) = match (
        world.get::<GlobalTransform>(entity),
        world.get::<Transform>(entity),
    ) {
        (_, Some(local)) if is_root => (local.translation, local.rotation),
        (Some(global), _) => {
            let (_, rotation, translation) = global.0.to_scale_rotation_translation();
            (translation, rotation)
        }
        (None, Some(local)) => (local.translation, local.rotation),
        (None, None) => {
            return Err(RpcError::not_found(
                format!("camera {} has no Transform", entity.to_bits()),
                "give the camera a Transform",
            ));
        }
    };
    let ray = screen_ray(eye, rotation, &camera, point.aspect, point.ndc.0, point.ndc.1);
    let mut out = json!({
        "camera": entity.to_bits(),
        "origin": ray.origin.to_array(),
        "direction": ray.direction.to_array(),
        "screen": {
            "x": point.pixel.0,
            "y": point.pixel.1,
            "u": point.uv.0,
            "v": point.uv.1,
            "width": point.size.map(|s| s.0),
            "height": point.size.map(|s| s.1),
            "aspect": point.aspect,
        },
    });
    if !ray_only {
        let cast = crate::pick::cast(world, ray, params)?;
        if let (Some(out), Some(cast)) = (out.as_object_mut(), cast.as_object()) {
            out.extend(cast.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::f32::consts::FRAC_PI_2;
    use std::sync::Arc;

    use regia_core::glam::Affine3A;
    use regia_render::{CpuMesh, LabMaterial, MeshRenderer, box_mesh};

    use super::*;

    fn camera(fov_y: f32) -> Camera {
        Camera {
            fov_y,
            ..Camera::default()
        }
    }

    #[test]
    fn the_centre_looks_straight_ahead_and_the_edges_follow_the_field_of_view() {
        let cam = camera(FRAC_PI_2); // 90 degrees: tan(45) = 1
        let centre = screen_ray(Vec3::ZERO, Quat::IDENTITY, &cam, 2.0, 0.0, 0.0);
        assert!(centre.direction.abs_diff_eq(Vec3::NEG_Z, 1.0e-6));
        let top = screen_ray(Vec3::ZERO, Quat::IDENTITY, &cam, 2.0, 0.0, 1.0);
        assert!(
            top.direction
                .abs_diff_eq(Vec3::new(0.0, 1.0, -1.0).normalize(), 1.0e-6)
        );
        let right = screen_ray(Vec3::ZERO, Quat::IDENTITY, &cam, 2.0, 1.0, 0.0);
        assert!(
            right
                .direction
                .abs_diff_eq(Vec3::new(2.0, 0.0, -1.0).normalize(), 1.0e-6),
            "the horizontal half-angle widens with the aspect ratio"
        );
    }

    #[test]
    fn the_camera_rotation_turns_the_rays() {
        let cam = camera(FRAC_PI_2);
        // Turned 90 degrees to the left (about +Y): forward is −X.
        let q = Quat::from_rotation_y(FRAC_PI_2);
        let ray = screen_ray(Vec3::new(1.0, 2.0, 3.0), q, &cam, 1.0, 0.0, 0.0);
        assert!(
            ray.direction.abs_diff_eq(Vec3::NEG_X, 1.0e-6),
            "{:?}",
            ray.direction
        );
        assert_eq!(ray.origin, Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn orthographic_rays_are_parallel_and_offset() {
        let cam = Camera {
            orthographic: true,
            ortho_size: 5.0,
            ..Camera::default()
        };
        let ray = screen_ray(Vec3::ZERO, Quat::IDENTITY, &cam, 2.0, 1.0, -1.0);
        assert!(ray.direction.abs_diff_eq(Vec3::NEG_Z, 1.0e-6));
        assert!(
            ray.origin.abs_diff_eq(Vec3::new(10.0, -5.0, 0.0), 1.0e-6),
            "{:?}",
            ray.origin
        );
    }

    #[test]
    fn screen_points_resolve_pixels_and_fractions_and_reject_bad_input() {
        let p = screen_point(
            &json!({ "x": 960, "y": 270, "width": 1920, "height": 1080 }),
            None,
        )
        .expect("pixels");
        assert_eq!(p.uv, (0.5, 0.25));
        assert_eq!(p.ndc, (0.0, 0.5));
        assert!((p.aspect - 16.0 / 9.0).abs() < 1.0e-6);
        let p =
            screen_point(&json!({ "u": 0.0, "v": 1.0 }), Some((100.0, 50.0))).expect("fractions + window");
        assert_eq!(p.ndc, (-1.0, -1.0));
        assert_eq!(p.pixel, (0.0, 50.0));
        assert_eq!(p.aspect, 2.0);
        let p = screen_point(&json!({ "u": 0.5, "v": 0.5 }), None).expect("fractions alone");
        assert_eq!(p.aspect, DEFAULT_ASPECT);
        assert_eq!(p.size, None);

        for bad in [
            json!({}),
            json!({ "x": 1 }),
            json!({ "x": 1, "y": 1 }), // pixels need a size and there is no window
            json!({ "x": 1, "y": 1, "u": 0.5, "v": 0.5, "width": 10, "height": 10 }),
            json!({ "x": 1, "y": 1, "width": 10 }),
            json!({ "x": 11, "y": 1, "width": 10, "height": 10 }),
            json!({ "u": -0.1, "v": 0.5 }),
            json!({ "x": "a", "y": 1, "width": 10, "height": 10 }),
            json!({ "x": 1, "y": 1, "width": 0, "height": 10 }),
        ] {
            assert!(screen_point(&bad, None).is_err(), "{bad} should be rejected");
        }
    }

    fn deck(world: &mut World, top: f32) {
        let (vertices, indices) = box_mesh([20.0, 0.1, 20.0]);
        world.spawn((
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(LabMaterial::default()),
            ),
            GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, top - 0.1, 0.0))),
        ));
    }

    #[test]
    fn the_centre_ray_hits_the_deck_below_a_camera_looking_down() {
        let mut world = World::new();
        deck(&mut world, 0.0);
        // 10 m up, looking straight down (rotate −Z onto −Y: a quarter turn about X).
        let down = Quat::from_rotation_x(-FRAC_PI_2);
        world.spawn((
            Camera::default(),
            Transform {
                translation: Vec3::new(0.0, 10.0, 0.0),
                rotation: down,
                ..Transform::default()
            },
        ));
        let out = camera_raycast(&mut world, &json!({ "u": 0.5, "v": 0.5 })).expect("raycast");
        assert_eq!(out["count"], 1, "{out}");
        assert!(
            (out["hit"]["distance"].as_f64().expect("distance") - 10.0).abs() < 1.0e-3,
            "{out}"
        );
        assert!((out["hit"]["point"][1].as_f64().expect("y")).abs() < 1.0e-3);
        assert_eq!(out["screen"]["u"], 0.5);

        let far_edge =
            camera_raycast(&mut world, &json!({ "u": 1.0, "v": 0.5, "aspect": 1.0 })).expect("edge");
        let hit = &far_edge["hit"];
        assert!(!hit.is_null(), "{far_edge}");
        // Half the vertical field of view (22.5 degrees) at 10 m: x = 10 tan(22.5 deg).
        let expect = 10.0 * (std::f32::consts::FRAC_PI_4 * 0.5).tan();
        assert!(
            (hit["point"][0].as_f64().expect("x") as f32 - expect).abs() < 1.0e-3,
            "{far_edge}"
        );
    }

    #[test]
    fn ray_only_skips_the_cast_and_a_miss_reports_no_hit() {
        let mut world = World::new();
        world.spawn((Camera::default(), Transform::default()));
        let ray = camera_raycast(&mut world, &json!({ "u": 0.5, "v": 0.5, "ray_only": true })).expect("ray");
        assert!(ray.get("hit").is_none() && ray.get("count").is_none());
        assert!((ray["direction"][2].as_f64().expect("z") + 1.0).abs() < 1.0e-6);
        let miss = camera_raycast(&mut world, &json!({ "u": 0.5, "v": 0.5 })).expect("miss");
        assert_eq!(miss["count"], 0);
        assert!(miss["hit"].is_null());
    }

    #[test]
    fn camera_choice_and_errors() {
        let mut world = World::new();
        let err = camera_raycast(&mut world, &json!({ "u": 0.5, "v": 0.5 })).expect_err("no camera");
        assert!(err.message.contains("no Camera"));
        let low = world.spawn((Camera::default(), Transform::default())).id();
        let high = world
            .spawn((
                Camera {
                    order: 3,
                    ..Camera::default()
                },
                Transform::default(),
            ))
            .id();
        let top = camera_raycast(&mut world, &json!({ "u": 0.5, "v": 0.5, "ray_only": true })).expect("top");
        assert_eq!(top["camera"], high.to_bits());
        let named = camera_raycast(
            &mut world,
            &json!({ "u": 0.5, "v": 0.5, "ray_only": true, "entity": low.to_bits() }),
        )
        .expect("named");
        assert_eq!(named["camera"], low.to_bits());
        let not_camera = world.spawn(Transform::default()).id();
        assert!(
            camera_raycast(
                &mut world,
                &json!({ "u": 0.5, "v": 0.5, "entity": not_camera.to_bits() })
            )
            .is_err()
        );
        assert!(camera_raycast(&mut world, &json!({ "u": 0.5, "v": 0.5, "entity": "x" })).is_err());
        assert!(camera_raycast(&mut world, &json!({ "u": 0.5, "v": 0.5, "ray_only": 1 })).is_err());
    }

    // ---- Error codes, edges and determinism (agent-tests lane) ------------------------------

    #[test]
    fn error_classes_are_stable() {
        let mut world = World::new();
        // No camera: NOT_FOUND, and a bad point is INVALID_PARAMS even before the camera lookup.
        let none = camera_raycast(&mut world, &json!({ "u": 0.5, "v": 0.5 })).expect_err("no camera");
        assert_eq!(none.code, crate::protocol::NOT_FOUND);
        let bad_point = camera_raycast(&mut world, &json!({})).expect_err("no point");
        assert_eq!(bad_point.code, crate::protocol::INVALID_PARAMS);
        world.spawn((Camera::default(), Transform::default()));
        let plain = world.spawn(Transform::default()).id();
        let not_camera = camera_raycast(
            &mut world,
            &json!({ "u": 0.5, "v": 0.5, "entity": plain.to_bits() }),
        )
        .expect_err("not a camera");
        assert_eq!(not_camera.code, crate::protocol::NOT_FOUND);
        for entity in [json!("x"), json!(-1), json!(1.5)] {
            let err = camera_raycast(&mut world, &json!({ "u": 0.5, "v": 0.5, "entity": entity }))
                .expect_err("bad entity");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{entity}");
        }
        // A well-formed id of an entity that does not exist is "not a camera", not a panic.
        let gone = world.spawn_empty().id();
        assert!(world.despawn(gone));
        let err = camera_raycast(
            &mut world,
            &json!({ "u": 0.5, "v": 0.5, "entity": gone.to_bits() }),
        )
        .expect_err("despawned");
        assert_eq!(err.code, crate::protocol::NOT_FOUND);
    }

    #[test]
    fn point_edges_are_inclusive_and_just_outside_is_rejected() {
        let mut world = World::new();
        world.spawn((Camera::default(), Transform::default()));
        for (u, v) in [(0.0, 0.0), (1.0, 1.0), (0.0, 1.0), (1.0, 0.0)] {
            let out =
                camera_raycast(&mut world, &json!({ "u": u, "v": v, "ray_only": true })).expect("corner");
            assert_eq!(out["screen"]["u"], u);
            assert_eq!(out["screen"]["v"], v);
        }
        for (u, v) in [(1.0001, 0.5), (0.5, 1.0001), (-0.0001, 0.5), (0.5, -1.0)] {
            let err = camera_raycast(&mut world, &json!({ "u": u, "v": v })).expect_err("outside");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{u},{v}");
        }
        // Pixel coordinates: the far edge (x == width) is inside, one pixel beyond is not.
        let edge = json!({ "x": 100, "y": 50, "width": 100, "height": 50, "ray_only": true });
        assert!(camera_raycast(&mut world, &edge).is_ok());
        let beyond = json!({ "x": 101, "y": 50, "width": 100, "height": 50 });
        assert!(camera_raycast(&mut world, &beyond).is_err());
    }

    #[test]
    fn numeric_parameters_reject_wrong_types_and_non_positive_sizes() {
        let mut world = World::new();
        world.spawn((Camera::default(), Transform::default()));
        for (extra, why) in [
            (json!({ "aspect": 0 }), "zero aspect"),
            (json!({ "aspect": -1.0 }), "negative aspect"),
            (json!({ "aspect": "wide" }), "string aspect"),
            (json!({ "width": 100 }), "width without height"),
            (json!({ "height": 100 }), "height without width"),
            (json!({ "width": -5, "height": 5 }), "negative width"),
            (json!({ "x": 1 }), "u/v mixed with a lone x"),
        ] {
            let mut params = json!({ "u": 0.5, "v": 0.5 });
            params
                .as_object_mut()
                .expect("object")
                .extend(extra.as_object().expect("object").clone());
            let err = camera_raycast(&mut world, &params).expect_err(why);
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{why}");
        }
    }

    #[test]
    fn raycast_is_deterministic_and_does_not_mutate_the_world() {
        let mut world = World::new();
        deck(&mut world, 0.0);
        let camera = world
            .spawn((
                Camera::default(),
                Transform {
                    translation: Vec3::new(1.0, 10.0, 2.0),
                    rotation: Quat::from_rotation_x(-FRAC_PI_2),
                    ..Transform::default()
                },
            ))
            .id();
        let pose = |world: &World| {
            world
                .get::<Transform>(camera)
                .map(|t| (t.translation, t.rotation))
        };
        let before = pose(&world);
        let params = json!({ "u": 0.3, "v": 0.6, "aspect": 1.5 });
        let a = camera_raycast(&mut world, &params).expect("a");
        let b = camera_raycast(&mut world, &params).expect("b");
        assert_eq!(a, b);
        assert_eq!(before, pose(&world));
    }

    #[test]
    fn pixel_and_fraction_forms_give_the_same_ray() {
        let mut world = World::new();
        world.spawn((Camera::default(), Transform::default()));
        let px = camera_raycast(
            &mut world,
            &json!({ "x": 480, "y": 135, "width": 1920, "height": 540, "ray_only": true }),
        )
        .expect("pixels");
        let uv = camera_raycast(
            &mut world,
            &json!({ "u": 0.25, "v": 0.25, "aspect": 1920.0 / 540.0, "ray_only": true }),
        )
        .expect("fractions");
        assert_eq!(px["direction"], uv["direction"]);
        assert_eq!(px["origin"], uv["origin"]);
    }
}
