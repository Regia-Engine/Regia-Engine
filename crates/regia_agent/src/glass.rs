// SPDX-License-Identifier: MPL-2.0
//! Agent methods for per-instance glass (`docs/book/agents/glass.md`): the own-canopy view mode
//! (`render.glass_view`, `render.set_glass_view`), how viewer-inside glass is shaded
//! (`render.canopy_glass`, `render.set_canopy_glass`, v0.1.17) and damage (`render.glass_damage`:
//! set a level, stamp an impact at a world point, clear).
//!
//! Stamps apply immediately (the page is rebuilt in the handler, so the reply carries the page
//! texel the impact landed on) and level changes go out as [`GlassLevelChanged`] messages, so a
//! failing pressure pane bursts and breaches exactly as in gameplay.

use regia_app::prelude::*;
use regia_core::math::{Mat4, Vec3};
use regia_render::MeshRenderer;
use regia_render::glass::{
    CANOPY_FULL_ENV, CanopyGlass, CanopyShading, GlassCrackSets, GlassDamage, GlassLevelChanged, GlassView,
    GlassViewMode, MaskSpace, apply_one, canopy_full_env, canopy_glass, canopy_glass_setting,
    update_canopy_glass,
};
use regia_scene::GlobalTransform;
use serde_json::{Value, json};

use crate::protocol::RpcError;
use crate::scene::entity_from;

fn mode_name(mode: GlassViewMode) -> &'static str {
    match mode {
        GlassViewMode::Exterior => "exterior",
        GlassViewMode::ViewerInside => "viewer_inside",
    }
}

fn glass_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = entity_from(params)?;
    if world.get::<MeshRenderer>(entity).is_none() {
        return Err(RpcError::not_found(
            format!("entity {} has no MeshRenderer", entity.to_bits()),
            "pick a glass mesh entity from scene.query_entities",
        ));
    }
    Ok(entity)
}

fn view_json(entity: Entity, view: GlassView) -> Value {
    json!({
        "entity": entity.to_bits(),
        "mode": mode_name(view.mode),
        "inside_specular": view.inside_specular,
    })
}

/// `render.glass_view { entity }`: the instance's view mode (exterior when it has none).
pub fn get_view(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = glass_entity(world, params)?;
    let view = world.get::<GlassView>(entity).copied().unwrap_or_default();
    Ok(view_json(entity, view))
}

/// `render.set_glass_view { entity, mode?, inside_specular? }`.
pub fn set_view(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = glass_entity(world, params)?;
    let before = world.get::<GlassView>(entity).copied().unwrap_or_default();
    let mut after = before;
    if let Some(mode) = params.get("mode") {
        after.mode = match mode.as_str() {
            Some("exterior") => GlassViewMode::Exterior,
            Some("viewer_inside") => GlassViewMode::ViewerInside,
            _ => {
                return Err(RpcError::invalid_params(
                    "`mode` must be \"exterior\" or \"viewer_inside\"",
                ));
            }
        };
    }
    if let Some(value) = params.get("inside_specular") {
        let v = value
            .as_f64()
            .filter(|v| (0.0..=1.0).contains(v))
            .ok_or_else(|| RpcError::invalid_params("`inside_specular` must be a number in 0..=1"))?;
        after.inside_specular = v as f32;
    }
    world.entity_mut(entity).insert(after);
    Ok(json!({ "before": view_json(entity, before), "after": view_json(entity, after) }))
}

fn shading_name(shading: CanopyShading) -> &'static str {
    match shading {
        CanopyShading::Cheap => "cheap",
        CanopyShading::Full => "full",
    }
}

/// `stored` (the value of `set_canopy_glass`) and what it resolves to with the environment.
fn canopy_json(stored: CanopyGlass) -> Value {
    let s = if canopy_full_env() { canopy_glass() } else { stored };
    json!({
        "shading": shading_name(s.shading),
        "nearest_layer_only": s.nearest_layer_only,
        "stored": {
            "shading": shading_name(stored.shading),
            "nearest_layer_only": stored.nearest_layer_only,
        },
        "env_full": canopy_full_env(),
        "env": CANOPY_FULL_ENV,
    })
}

/// `render.canopy_glass`: how viewer-inside glass (own canopy) is shaded for every renderer
/// without its own override: `cheap` (default) or `full`, the nearest-layer option and whether
/// `REGIA_CANOPY_FULL` forces the full shader. With a renderer, `last_frame` counts the last
/// frame's viewer-inside draws by the shader they took (`cheap`, `full` by setting,
/// `eye_outside`: full because the camera was outside the glass's bounds).
pub fn get_canopy(world: &World) -> Result<Value, RpcError> {
    let mut reply = canopy_json(canopy_glass_setting());
    reply["eye_margin_m"] = json!(regia_render::CANOPY_EYE_MARGIN_M);
    reply["last_frame"] = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .map_or(Value::Null, |r| {
            let d = r.0.canopy_draws();
            json!({ "cheap": d.cheap, "full": d.full, "eye_outside": d.eye_outside })
        });
    Ok(reply)
}

/// `render.set_canopy_glass { shading?, nearest_layer_only? }`: sets the process-wide value from
/// the next frame (omitted fields keep their stored value; one atomic read-modify-write, so a
/// concurrent change of the other field is kept). Not persisted; the environment override still
/// wins while set (`after.shading` stays `full`, `after.stored` shows the value).
/// Returns `{ before, after }`.
pub fn set_canopy(params: &Value) -> Result<Value, RpcError> {
    let shading = match params.get("shading") {
        None => None,
        Some(shading) => Some(match shading.as_str() {
            Some("cheap") => CanopyShading::Cheap,
            Some("full") => CanopyShading::Full,
            _ => {
                return Err(RpcError::invalid_params(
                    "`shading` must be \"cheap\" or \"full\"",
                ));
            }
        }),
    };
    let nearest_layer_only = match params.get("nearest_layer_only") {
        None => None,
        Some(value) => Some(
            value
                .as_bool()
                .ok_or_else(|| RpcError::invalid_params("`nearest_layer_only` must be a boolean"))?,
        ),
    };
    let (before, after) = update_canopy_glass(|mut stored| {
        if let Some(shading) = shading {
            stored.shading = shading;
        }
        if let Some(nearest) = nearest_layer_only {
            stored.nearest_layer_only = nearest;
        }
        stored
    });
    Ok(json!({ "before": canopy_json(before), "after": canopy_json(after) }))
}

fn vec3(params: &Value, field: &str) -> Result<Option<Vec3>, RpcError> {
    let Some(v) = params.get(field) else {
        return Ok(None);
    };
    let a = v
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be [x, y, z]")))?;
    let mut out = [0.0f32; 3];
    for (o, x) in out.iter_mut().zip(a) {
        *o = x
            .as_f64()
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be three numbers")))?
            as f32;
    }
    Ok(Some(Vec3::from_array(out)))
}

fn number(params: &Value, field: &str) -> Result<Option<f32>, RpcError> {
    match params.get(field) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| Some(v as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number >= 0"))),
    }
}

/// `render.glass_damage { entity, set?, level?, stamp?: { point, size?, energy? }, clear? }`.
pub fn damage(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = glass_entity(world, params)?;
    let sets = world
        .get_resource::<GlassCrackSets>()
        .cloned()
        .unwrap_or_default();
    let mut damage = world.get::<GlassDamage>(entity).cloned().unwrap_or_default();
    if let Some(set) = params.get("set") {
        let name = set
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`set` must be a string"))?;
        if sets.get(name).is_none() {
            return Err(RpcError::not_found(
                format!("crack set `{name}` is not loaded"),
                format!("loaded sets: {}", sets.names().join(", ")),
            ));
        }
        if name != damage.set {
            // Same impacts, re-stamped from the new set.
            let mut switched = GlassDamage::new(name);
            switched.level = damage.level;
            switched.energy = damage.energy;
            switched.impacts = damage.impacts.clone();
            damage = switched;
        }
    }
    if crate::params::opt_bool(params, "clear")? == Some(true) {
        damage = GlassDamage::new(&damage.set);
    }
    let Some(cooked) = sets.get(&damage.set).cloned() else {
        return Err(RpcError::not_found(
            format!("crack set `{}` is not loaded", damage.set),
            "use set: \"laminate\"",
        ));
    };
    let (mesh, model) = {
        // `glass_entity` checked the MeshRenderer; nothing in between removes it.
        let Some(renderer) = world.get::<MeshRenderer>(entity) else {
            return Err(RpcError::not_found(
                format!("entity {} has no MeshRenderer", entity.to_bits()),
                "render.glass_damage needs a mesh entity",
            ));
        };
        let model = world
            .get::<GlobalTransform>(entity)
            .map_or(Mat4::IDENTITY, |t| Mat4::from(t.0));
        (renderer.mesh.clone(), model)
    };
    let space = MaskSpace::from_mesh(&mesh.vertices, cooked.manifest.damage_space);
    let level = match params.get("level") {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|l| *l <= 3)
                .ok_or_else(|| RpcError::invalid_params("`level` must be 0..=3"))? as u8,
        ),
    };
    let mut stamped = Value::Null;
    if let Some(stamp) = params.get("stamp") {
        let point =
            vec3(stamp, "point")?.ok_or_else(|| RpcError::invalid_params("`stamp.point` is required"))?;
        let size = number(stamp, "size")?.unwrap_or(0.0);
        let energy = number(stamp, "energy")?.unwrap_or(1.0);
        let local = model.inverse().transform_point3(point);
        let index = match level {
            Some(l) if l > 0 => damage.impact_at_level(local, energy, l, size),
            _ => {
                let i = damage.impact(local, energy);
                damage.impacts[i].size_m = size;
                i
            }
        };
        let uv = space
            .hit_uv(&mesh.vertices, &mesh.indices, local)
            .map(|(uv, _)| uv);
        let page = cooked.manifest.page_size as f32;
        stamped = json!({
            "impact": index,
            "local": local.to_array(),
            "uv": uv.map(|uv| uv.to_array()),
            "texel": uv.map(|uv| [(uv.x * page).floor(), (uv.y * page).floor()]),
        });
    } else if let Some(l) = level {
        if l == 0 {
            damage = GlassDamage::new(&damage.set);
        } else if damage.level < l {
            // A level without a hit point: one impact at the page centre so the level shows.
            let mut centre = [0.0f32; 3];
            let (lo, hi) =
                mesh.vertices
                    .iter()
                    .fold(([f32::MAX; 3], [f32::MIN; 3]), |(mut lo, mut hi), v| {
                        for k in 0..3 {
                            lo[k] = lo[k].min(v.position[k]);
                            hi[k] = hi[k].max(v.position[k]);
                        }
                        (lo, hi)
                    });
            for k in 0..3 {
                centre[k] = (lo[k] + hi[k]) * 0.5;
            }
            damage.impact_at_level(Vec3::from_array(centre), 5.0, l, 0.0);
        }
    }
    let mesh_id = regia_render::mesh_key(&mesh) as usize;
    let changes = apply_one(
        &mut damage,
        &cooked,
        &mesh.vertices,
        &mesh.indices,
        mesh_id,
        model,
    );
    let (u, v) = space.world_axes(model);
    let normal = u.cross(v).normalize_or_zero();
    for (from, to, local) in changes {
        world.write_message(GlassLevelChanged {
            entity,
            from,
            to,
            glass_type: cooked.manifest.glass_type,
            point: model.transform_point3(local),
            normal,
            set: cooked.manifest.name.clone(),
        });
    }
    let reply = json!({
        "entity": entity.to_bits(),
        "set": damage.set,
        "glass_type": cooked.manifest.glass_type.name(),
        "level": damage.level,
        "energy": damage.energy,
        "impacts": damage.impacts.len(),
        "applied": damage.applied(),
        "page_size": cooked.manifest.page_size,
        "metres_per_uv": damage.metres_per_uv(),
        "stamp": stamped,
    });
    world.entity_mut(entity).insert(damage);
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{CpuMaterial, CpuMesh};
    use regia_scene::Transform;
    use std::sync::Arc;

    fn world_with_pane() -> (World, Entity) {
        let mut world = World::new();
        world.init_resource::<Messages<GlassLevelChanged>>();
        let (vertices, indices) = regia_render::box_mesh([1.0, 0.005, 0.5]);
        let transform = Transform {
            translation: Vec3::new(2.0, 1.0, -3.0),
            rotation: regia_core::math::Quat::from_rotation_y(0.8),
            scale: Vec3::ONE,
        };
        let entity = world
            .spawn((
                MeshRenderer::new(
                    Arc::new(CpuMesh { vertices, indices }),
                    Arc::new(CpuMaterial::default()),
                ),
                GlobalTransform(transform.compute_affine()),
            ))
            .id();
        (world, entity)
    }

    #[test]
    fn view_round_trips_and_validates() {
        let (mut world, e) = world_with_pane();
        let got = get_view(&mut world, &json!({ "entity": e.to_bits() })).expect("get");
        assert_eq!(got["mode"], "exterior");
        let set = set_view(
            &mut world,
            &json!({ "entity": e.to_bits(), "mode": "viewer_inside", "inside_specular": 0.1 }),
        )
        .expect("set");
        assert_eq!(set["after"]["mode"], "viewer_inside");
        assert!(
            world
                .get::<GlassView>(e)
                .is_some_and(|v| v.mode == GlassViewMode::ViewerInside)
        );
        assert!(set_view(&mut world, &json!({ "entity": e.to_bits(), "mode": "sideways" })).is_err());
        assert!(
            set_view(
                &mut world,
                &json!({ "entity": e.to_bits(), "inside_specular": 2.0 })
            )
            .is_err()
        );
    }

    #[test]
    fn a_stamp_lands_on_the_hit_texel_of_a_moved_rotated_pane() {
        let (mut world, e) = world_with_pane();
        let model = Mat4::from(world.get::<GlobalTransform>(e).expect("transform").0);
        let local = Vec3::new(0.3, 0.005, -0.2);
        let point = model.transform_point3(local);
        let reply = damage(
            &mut world,
            &json!({ "entity": e.to_bits(), "set": "pressure_pane", "level": 1, "stamp": { "point": point.to_array(), "size": 0.2 } }),
        )
        .expect("stamp");
        // Planar page over the pane's x/z, a 2 m square centred on it: uv = ((x+1)/2, (z+1)/2).
        let page = 512.0;
        let want = [
            ((0.3 + 1.0) / 2.0 * page as f32).floor(),
            ((-0.2 + 1.0) / 2.0 * page as f32).floor(),
        ];
        let texel = reply["stamp"]["texel"]
            .as_array()
            .expect("texel")
            .iter()
            .map(|v| v.as_f64().unwrap_or(-9.0) as f32)
            .collect::<Vec<_>>();
        assert!(
            (texel[0] - want[0]).abs() <= 1.0 && (texel[1] - want[1]).abs() <= 1.0,
            "{texel:?} vs {want:?}"
        );
        // The page has crack texels around that spot.
        let d = world.get::<GlassDamage>(e).expect("damage");
        let mask = d.mask().expect("page");
        let mut best = (0u8, 0u32, 0u32);
        for y in want[1] as u32 - 6..want[1] as u32 + 6 {
            for x in want[0] as u32 - 6..want[0] as u32 + 6 {
                let t = mask.texel(x, y);
                if t[0] > best.0 {
                    best = (t[0], x, y);
                }
            }
        }
        assert!(best.0 > 128, "no crack at the hit texel: {best:?}");
        assert_eq!(reply["level"], 1);
        assert_eq!(reply["applied"], 1);
    }

    #[test]
    fn levels_escalate_report_and_clear() {
        let (mut world, e) = world_with_pane();
        let r = damage(
            &mut world,
            &json!({ "entity": e.to_bits(), "set": "pressure_pane", "level": 3 }),
        )
        .expect("d3");
        assert_eq!(r["level"], 3);
        let changes: Vec<_> = world
            .resource_mut::<Messages<GlassLevelChanged>>()
            .drain()
            .collect();
        assert_eq!(changes.len(), 1);
        assert_eq!((changes[0].from, changes[0].to), (0, 3));
        let r = damage(&mut world, &json!({ "entity": e.to_bits(), "clear": true })).expect("clear");
        assert_eq!(r["level"], 0);
        assert_eq!(r["impacts"], 0);
        assert!(damage(&mut world, &json!({ "entity": e.to_bits(), "set": "nope" })).is_err());
        assert!(damage(&mut world, &json!({ "entity": e.to_bits(), "level": 7 })).is_err());
    }

    /// Tests in this binary that change the process-wide canopy setting take turns.
    static CANOPY_SETTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Puts the stored setting back as it was when made, also when the test panics.
    struct RestoreCanopy(CanopyGlass);

    impl Drop for RestoreCanopy {
        fn drop(&mut self) {
            regia_render::set_canopy_glass(self.0);
        }
    }

    fn canopy_turn() -> (std::sync::MutexGuard<'static, ()>, RestoreCanopy) {
        let turn = CANOPY_SETTING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (turn, RestoreCanopy(canopy_glass_setting()))
    }

    #[test]
    fn canopy_shading_reads_sets_and_restores() {
        let (_turn, restore) = canopy_turn();
        let world = World::new();
        let got = get_canopy(&world).expect("get");
        assert_eq!(got["env"], "REGIA_CANOPY_FULL");
        assert_eq!(got["last_frame"], Value::Null, "no renderer in this world");
        let env = got["env_full"].as_bool().unwrap_or(false);
        assert!(set_canopy(&json!({ "shading": "fast" })).is_err());
        assert!(set_canopy(&json!({ "nearest_layer_only": 1 })).is_err());
        let r = set_canopy(&json!({ "shading": "full", "nearest_layer_only": true })).expect("set");
        assert_eq!(r["after"]["stored"]["shading"], "full");
        assert_eq!(r["after"]["stored"]["nearest_layer_only"], true);
        assert_eq!(r["after"]["shading"], "full");
        // An omitted field keeps its stored value.
        let r = set_canopy(&json!({ "shading": "cheap" })).expect("cheap");
        assert_eq!(r["before"]["stored"]["shading"], "full");
        assert_eq!(r["after"]["stored"]["nearest_layer_only"], true);
        assert_eq!(r["after"]["shading"], if env { "full" } else { "cheap" });
        // Restore the value the test found (not a hard-coded default) and check it took.
        let previous = restore.0;
        let r = set_canopy(&json!({
            "shading": shading_name(previous.shading),
            "nearest_layer_only": previous.nearest_layer_only,
        }))
        .expect("restore");
        assert_eq!(r["after"]["stored"]["shading"], shading_name(previous.shading));
        assert_eq!(canopy_glass_setting(), previous);
    }

    #[test]
    fn concurrent_canopy_sets_of_different_fields_both_stick() {
        // One thread sets only `shading`, the other only `nearest_layer_only`. Each must find its
        // field as it last left it: a read-then-store `set_canopy` let the other thread's store
        // write back a stale copy of it.
        let (_turn, _restore) = canopy_turn();
        let rounds = 4000;
        let barrier = std::sync::Barrier::new(2);
        let lost = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                for i in 0..rounds {
                    let full = i % 2 == 0;
                    let shading = if full { "full" } else { "cheap" };
                    if set_canopy(&json!({ "shading": shading })).is_err() {
                        lost.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if (canopy_glass_setting().shading == CanopyShading::Full) != full {
                        lost.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            });
            scope.spawn(|| {
                barrier.wait();
                for i in 0..rounds {
                    let on = i % 2 == 0;
                    if set_canopy(&json!({ "nearest_layer_only": on })).is_err() {
                        lost.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if canopy_glass_setting().nearest_layer_only != on {
                        lost.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            });
        });
        assert_eq!(
            lost.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a concurrent set of the other field reverted this thread's field"
        );
    }
}
