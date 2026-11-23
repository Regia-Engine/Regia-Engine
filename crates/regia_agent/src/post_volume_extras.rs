// SPDX-License-Identifier: MPL-2.0
//! Post-volume format v3 on the agent protocol (`book/agents/post-volumes.md` § Format v3,
//! ADR-2323):
//!
//! - `render.set_post_volume_luts { enabled?, reload? }` turns the per-volume LUT blend on or off
//!   (off by default) and reports the tables it loaded.
//! - `render.post_volume_gizmos { entity? }` returns the editor gizmo's data, headless: every
//!   volume's shape and blend-band outline as world-space line segments.
//! - `render.post_volumes` gains a `lut_blend` block (built here) and `render.set_post_volume` a
//!   `lut` field (validated here).
//!
//! Error reasons: `invalid_lut`, `not_a_volume` (plus the shared `unknown_field`,
//! `invalid_value`, `entity_missing`).

use std::sync::Arc;

use regia_app::prelude::*;
use regia_render::post_volume_luts::{LutContribution, PostVolumeLuts, blend_luts};
use regia_render::post_volume_shapes::post_volume_outline;
use regia_render::post_volumes::PostVolume;
use regia_scene::GlobalTransform;
use serde_json::{Value, json};

use crate::post_volumes::{base_post, check_fields, entity_param, invalid, missing, object, shape_json};
use crate::protocol::RpcError;

/// Longest LUT path `render.set_post_volume` accepts, bytes.
pub(crate) const MAX_LUT_PATH: usize = 1024;

/// Validates a volume's `lut` value: a non-empty path ending in `.cube` or `.png`, at most
/// [`MAX_LUT_PATH`] bytes, without NUL or `..` components.
pub(crate) fn parse_lut_path(value: &Value) -> Result<String, RpcError> {
    let bad = |message: &str| invalid("invalid_lut", message.to_string());
    let path = value
        .as_str()
        .ok_or_else(|| bad("`lut` must be a path string (or null to clear it)"))?;
    if path.is_empty() || path.len() > MAX_LUT_PATH || path.contains('\0') {
        return Err(bad("`lut` must be a non-empty path of at most 1024 bytes"));
    }
    if path.split(['/', '\\']).any(|part| part == "..") {
        return Err(bad("`lut` may not leave the project (`..`)"));
    }
    let lower = path.to_ascii_lowercase();
    if !(lower.ends_with(".cube") || lower.ends_with(".png")) {
        return Err(bad("`lut` must name a .cube or a PNG-strip .png file"));
    }
    Ok(path.to_string())
}

fn lut_json(lut: Option<&Arc<regia_render::ColorLut>>) -> Value {
    lut.map_or(
        Value::Null,
        |l| json!({ "size": l.size(), "key": format!("{:016x}", l.key()) }),
    )
}

fn state_json(luts: Option<&PostVolumeLuts>) -> Value {
    let Some(luts) = luts else {
        return json!({ "enabled": false, "paths": [], "stats": Value::Null });
    };
    let stats = luts.stats();
    json!({
        "enabled": luts.enabled,
        "paths": luts
            .paths()
            .into_iter()
            .map(|(path, loaded)| json!({ "path": path, "loaded": loaded }))
            .collect::<Vec<_>>(),
        "stats": {
            "loads": stats.loads,
            "failures": stats.failures,
            "blends": stats.blends,
            "cache_hits": stats.cache_hits,
        },
    })
}

/// The `lut_blend` block of `render.post_volumes`: the switch, the loaded tables, the volumes
/// whose LUT reaches `eye` and the view's resulting LUT there (computed without touching the
/// renderer's cache).
pub(crate) fn lut_blend_json(
    world: &World,
    rows: &[(Entity, PostVolume, GlobalTransform)],
    eye: [f32; 3],
) -> Value {
    let luts = world.get_resource::<PostVolumeLuts>();
    let mut state = state_json(luts);
    let base = base_post(world).lut.or_else(|| {
        world
            .get_resource::<regia_render::LevelColorLut>()
            .and_then(|l| l.lut.clone())
    });
    let mut contributions = Vec::new();
    let mut contributing = Vec::new();
    if let Some(luts) = luts.filter(|l| l.enabled) {
        for (entity, volume, placement) in rows {
            let Some(path) = volume.lut.as_deref() else {
                continue;
            };
            let weight = if volume.is_supported() {
                volume.weight_at(placement, eye)
            } else {
                0.0
            };
            let loaded = luts.table(path);
            contributing.push(json!({
                "entity": entity.to_bits(),
                "priority": volume.priority,
                "weight": weight,
                "path": path,
                "loaded": loaded.is_some(),
            }));
            if let Some(lut) = loaded
                && weight > 0.0
            {
                contributions.push(LutContribution {
                    weight,
                    priority: volume.priority,
                    key: entity.to_bits(),
                    lut: lut.clone(),
                });
            }
        }
    }
    let resolved = if contributions.is_empty() {
        base.clone()
    } else {
        blend_luts(base.as_ref(), &mut contributions)
    };
    state["base_lut"] = lut_json(base.as_ref());
    state["volumes"] = Value::Array(contributing);
    state["resolved_lut"] = lut_json(resolved.as_ref());
    state
}

/// `render.set_post_volume_luts { enabled?, reload? }`: sets the switch (inserting the resource
/// when the world has none) and, with `reload: true`, forgets the loaded tables so the next frame
/// reads the files again. Returns `{ before, after }` (each `{ enabled, paths, stats }`).
pub(crate) fn set_post_volume_luts(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = object(params)?;
    check_fields(map, &["enabled", "reload"])?;
    let flag = |key: &str| match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(invalid("invalid_value", format!("`{key}` must be a boolean"))),
    };
    let enabled = flag("enabled")?;
    let reload = flag("reload")?.unwrap_or(false);
    let before = state_json(world.get_resource::<PostVolumeLuts>());
    if !world.contains_resource::<PostVolumeLuts>() {
        world.init_resource::<PostVolumeLuts>();
    }
    let mut luts = world.resource_mut::<PostVolumeLuts>();
    if let Some(enabled) = enabled {
        luts.enabled = enabled;
    }
    if reload {
        luts.clear();
    }
    let after = state_json(Some(&*luts));
    Ok(json!({ "before": before, "after": after }))
}

fn segments_json(segments: &[[[f32; 3]; 2]]) -> Value {
    Value::Array(segments.iter().map(|s| json!([s[0], s[1]])).collect())
}

/// `render.post_volume_gizmos { entity? }`: every volume's outline (or one volume's), sorted by
/// entity: `{ volumes: [{ entity, enabled, supported, priority, shape, blend_distance, outline:
/// { shape, band, bounded } | null }] }`. An unbound volume or an invalid shape has a null
/// outline.
pub(crate) fn post_volume_gizmos(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = object(params)?;
    check_fields(map, &["entity"])?;
    let only = entity_param(map, "entity")?;
    if let Some(entity) = only {
        if world.get_entity(entity).is_err() {
            return Err(missing(
                "entity_missing",
                format!("no entity {}", entity.to_bits()),
                "pass an entity that exists, or omit `entity` for every volume",
            ));
        }
        if world.get::<PostVolume>(entity).is_none() {
            return Err(missing(
                "not_a_volume",
                format!("entity {} has no PostVolume", entity.to_bits()),
                "add one with render.set_post_volume",
            ));
        }
    }
    let mut query = world.query::<(Entity, &PostVolume, Option<&GlobalTransform>)>();
    let mut keyed: Vec<(u64, Value)> = query
        .iter(world)
        .filter(|(entity, _, _)| only.is_none_or(|o| o == *entity))
        .map(|(entity, volume, placement)| {
            let placement = placement.copied().unwrap_or_default();
            let outline = post_volume_outline(volume, &placement).map(|o| {
                json!({
                    "shape": segments_json(&o.shape),
                    "band": segments_json(&o.band),
                    "bounded": o.bounded,
                })
            });
            (
                entity.to_bits(),
                json!({
                    "entity": entity.to_bits(),
                    "enabled": volume.enabled,
                    "supported": volume.is_supported(),
                    "priority": volume.priority,
                    "shape": shape_json(&volume.shape),
                    "blend_distance": volume.blend_distance,
                    "outline": outline,
                }),
            )
        })
        .collect();
    keyed.sort_by_key(|(key, _)| *key);
    let rows: Vec<Value> = keyed.into_iter().map(|(_, row)| row).collect();
    Ok(json!({ "volumes": rows }))
}

#[cfg(test)]
mod tests {
    use regia_core::math::Vec3;
    use regia_render::post_volumes::PostVolumeShape;
    use regia_render::{Camera, ColorLut};
    use regia_scene::Transform;

    use super::*;

    fn at(x: f32) -> GlobalTransform {
        GlobalTransform(Transform::from_translation(Vec3::new(x, 0.0, 0.0)).compute_affine())
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn lut_paths_are_validated() {
        assert_eq!(
            parse_lut_path(&json!("looks/cellar.cube")).ok().as_deref(),
            Some("looks/cellar.cube")
        );
        assert!(parse_lut_path(&json!("looks/strip.PNG")).is_ok());
        for bad in [
            json!(""),
            json!(3),
            json!("looks/cellar.txt"),
            json!("../outside.cube"),
            json!("a\\..\\b.cube"),
            json!("x".repeat(MAX_LUT_PATH) + ".cube"),
        ] {
            let err = parse_lut_path(&bad).expect_err("bad path");
            assert_eq!(reason(&err), "invalid_lut", "{bad}");
        }
    }

    #[test]
    fn the_lut_switch_and_blend_report() -> Result<(), RpcError> {
        let mut world = World::new();
        world.spawn((at(0.0), Camera::default()));
        let warm = Arc::new(ColorLut::from_fn(4, |c| [c[0], c[1] * 0.9, c[2] * 0.8]));
        let volume = world
            .spawn((
                at(0.0),
                PostVolume {
                    lut: Some("looks/warm.cube".into()),
                    ..PostVolume::default()
                },
            ))
            .id();
        // Off by default: the report says so, and the view keeps no LUT.
        let out = crate::post_volumes::post_volumes(&mut world, &json!({}))?;
        assert_eq!(out["lut_blend"]["enabled"], false);
        assert_eq!(out["lut_blend"]["resolved_lut"], Value::Null);
        assert_eq!(out["volumes"][0]["lut"], "looks/warm.cube");

        let set = set_post_volume_luts(&mut world, &json!({ "enabled": true }))?;
        assert_eq!(set["before"]["enabled"], false);
        assert_eq!(set["after"]["enabled"], true);
        world
            .resource_mut::<PostVolumeLuts>()
            .insert("looks/warm.cube", warm.clone());
        let out = crate::post_volumes::post_volumes(&mut world, &json!({}))?;
        assert_eq!(out["lut_blend"]["volumes"][0]["entity"], volume.to_bits());
        assert_eq!(out["lut_blend"]["volumes"][0]["loaded"], true);
        // A full-weight single volume hands its own table over.
        assert_eq!(
            out["lut_blend"]["resolved_lut"]["key"],
            format!("{:016x}", warm.key())
        );
        let reload = set_post_volume_luts(&mut world, &json!({ "reload": true }))?;
        assert_eq!(reload["after"]["paths"], json!([]));
        for (params, why) in [
            (json!({ "enabled": 1 }), "invalid_value"),
            (json!({ "on": true }), "unknown_field"),
        ] {
            let err = set_post_volume_luts(&mut world, &params).expect_err("bad");
            assert_eq!(reason(&err), why, "{params}");
        }
        Ok(())
    }

    #[test]
    fn set_post_volume_takes_a_hull_and_a_lut() -> Result<(), RpcError> {
        let mut world = World::new();
        let entity = world.spawn(at(0.0)).id();
        let planes = regia_render::post_volume_shapes::box_hull_planes([1.0, 2.0, 3.0]);
        crate::post_volumes::set_post_volume(
            &mut world,
            &json!({ "entity": entity.to_bits(), "shape": { "kind": "convex_hull", "planes": planes },
                     "lut": "looks/cellar.cube" }),
        )?;
        let volume = world.get::<PostVolume>(entity).cloned().unwrap_or_default();
        assert_eq!(volume.shape, PostVolumeShape::ConvexHull { planes });
        assert_eq!(volume.lut.as_deref(), Some("looks/cellar.cube"));
        crate::post_volumes::set_post_volume(
            &mut world,
            &json!({ "entity": entity.to_bits(), "lut": null }),
        )?;
        assert_eq!(world.get::<PostVolume>(entity).and_then(|v| v.lut.clone()), None);
        let seventeen: Vec<[f32; 4]> = (0..17).map(|i| [1.0, i as f32, 0.0, 1.0]).collect();
        for (shape, why) in [
            (json!({ "kind": "convex_hull", "planes": [] }), "invalid_shape"),
            (
                json!({ "kind": "convex_hull", "planes": seventeen }),
                "invalid_shape",
            ),
            (
                json!({ "kind": "convex_hull", "planes": [[0, 0, 0, 1]] }),
                "invalid_shape",
            ),
            (
                json!({ "kind": "convex_hull", "planes": [[1, 0, 0]] }),
                "invalid_shape",
            ),
            (json!({ "kind": "convex_hull" }), "invalid_shape"),
        ] {
            let err = crate::post_volumes::set_post_volume(
                &mut world,
                &json!({ "entity": entity.to_bits(), "shape": shape }),
            )
            .expect_err("bad hull");
            assert_eq!(reason(&err), why, "{shape}");
        }
        let err = crate::post_volumes::set_post_volume(
            &mut world,
            &json!({ "entity": entity.to_bits(), "lut": "x.exr" }),
        )
        .expect_err("bad lut");
        assert_eq!(reason(&err), "invalid_lut");
        Ok(())
    }

    #[test]
    fn gizmo_data_outlines_every_volume() -> Result<(), RpcError> {
        let mut world = World::new();
        let boxed = world
            .spawn((
                at(0.0),
                PostVolume {
                    blend_distance: 0.5,
                    ..PostVolume::default()
                },
            ))
            .id();
        let unbound = world
            .spawn((
                at(0.0),
                PostVolume {
                    shape: PostVolumeShape::Unbound,
                    ..PostVolume::default()
                },
            ))
            .id();
        let out = post_volume_gizmos(&mut world, &json!({}))?;
        let rows = out["volumes"].as_array().cloned().unwrap_or_default();
        assert_eq!(rows.len(), 2);
        let row = |entity: Entity| {
            rows.iter()
                .find(|r| r["entity"] == entity.to_bits())
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(row(boxed)["outline"]["shape"].as_array().map(Vec::len), Some(12));
        assert_eq!(row(boxed)["outline"]["band"].as_array().map(Vec::len), Some(12));
        assert_eq!(row(boxed)["outline"]["bounded"], true);
        assert_eq!(row(unbound)["outline"], Value::Null);
        let one = post_volume_gizmos(&mut world, &json!({ "entity": boxed.to_bits() }))?;
        assert_eq!(one["volumes"].as_array().map(Vec::len), Some(1));
        let plain = world.spawn(at(1.0)).id();
        let err =
            post_volume_gizmos(&mut world, &json!({ "entity": plain.to_bits() })).expect_err("no volume");
        assert_eq!(reason(&err), "not_a_volume");
        let err =
            post_volume_gizmos(&mut world, &json!({ "entity": 999_999_999_u64 })).expect_err("no entity");
        assert_eq!(reason(&err), "entity_missing");
        Ok(())
    }
}
