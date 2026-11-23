// SPDX-License-Identifier: MPL-2.0
//! Agent methods for projected decals (spawn a prefab on a hit, lay a rut strip, read the
//! budget) and the fire look (heat haze and fire light: query, toggle, intensity).
//!
//! Handlers mutate [`regia_vfx::DecalPool`] directly. A world without a pool returns
//! `UNSUPPORTED`. They do not also call [`regia_vfx::place_decal`] (that would double-spawn).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_vfx::lights::VfxFrameLights;
use regia_vfx::{
    DECAL_TILE_PX, DecalCullStats, DecalLibrary, DecalPool, DecalStrip, VfxLook, VfxView, bin_decals,
    sample_quadratic,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Most decals one `vfx.place_rut` stamps (eight times the GPU decal budget): a longer strip
/// would allocate without bound, and past f32 precision its stamping loop would never end.
const MAX_RUT_STAMPS: usize = 4096;

fn require_pool(world: &mut World) -> Result<(), RpcError> {
    if world.get_resource::<DecalPool>().is_none() {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0016",
            "no decal pool in this app",
            "add regia_vfx::VfxPlugin so vfx.spawn_decal has a pool",
        ));
    }
    Ok(())
}

fn vec3_param(params: &Value, field: &str) -> Result<Vec3, RpcError> {
    let value = params
        .get(field)
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` ([x, y, z]) is required")))?;
    let components = match value {
        Value::Array(items) if items.len() == 3 => {
            let mut out = [0.0f64; 3];
            for (i, item) in items.iter().enumerate() {
                out[i] = item
                    .as_f64()
                    .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be three numbers")))?;
            }
            out
        }
        Value::Object(map) => {
            let g = |k: &str| map.get(k).and_then(Value::as_f64);
            let invalid = || RpcError::invalid_params(format!("`{field}` must be three numbers"));
            [
                g("x").ok_or_else(invalid)?,
                g("y").ok_or_else(invalid)?,
                g("z").ok_or_else(invalid)?,
            ]
        }
        _ => return Err(RpcError::invalid_params(format!("`{field}` must be [x, y, z]"))),
    };
    let v = Vec3::new(components[0] as f32, components[1] as f32, components[2] as f32);
    // A JSON number past f32's range becomes infinity (an infinite rut never ends).
    if !v.is_finite() {
        return Err(RpcError::invalid_params(format!(
            "`{field}` must be three finite numbers within f32 range"
        )));
    }
    Ok(v)
}

fn optional_vec3(params: &Value, field: &str, default: Vec3) -> Result<Vec3, RpcError> {
    if params.get(field).is_none() {
        return Ok(default);
    }
    vec3_param(params, field)
}

fn optional_f32(params: &Value, field: &str, default: f32) -> Result<f32, RpcError> {
    match params.get(field) {
        None => Ok(default),
        Some(value) => {
            let n = value
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number")))?;
            if !(n.is_finite() && (n as f32).is_finite()) {
                return Err(RpcError::invalid_params(format!("`{field}` must be finite")));
            }
            Ok(n as f32)
        }
    }
}

fn optional_lifetime(params: &Value) -> Result<Option<f32>, RpcError> {
    match params.get("lifetime") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let n = value
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params("`lifetime` must be a number"))?;
            Ok(Some(n as f32))
        }
    }
}

fn library(world: &World) -> DecalLibrary {
    world
        .get_resource::<DecalLibrary>()
        .cloned()
        .unwrap_or_else(DecalLibrary::builtin)
}

fn point_list(params: &Value) -> Result<Vec<Vec3>, RpcError> {
    let items = params
        .get("points")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("`points` (at least two [x, y, z]) is required"))?;
    if items.len() < 2 {
        return Err(RpcError::invalid_params("`points` needs at least two positions"));
    }
    let mut points = Vec::with_capacity(items.len());
    for item in items {
        points.push(vec3_param(&json!({ "point": item }), "point")?);
    }
    Ok(points)
}

/// `vfx.spawn_decal`: place a prefab on a hit, local +Z along the surface normal.
pub fn spawn_decal(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_pool(world)?;
    let position = vec3_param(params, "position")?;
    let normal = optional_vec3(params, "normal", Vec3::Y)?;
    if normal.length_squared() < 1e-8 {
        return Err(RpcError::invalid_params("`normal` must not be zero"));
    }
    let yaw = optional_f32(params, "yaw", 0.0)?;
    let prefab_name = crate::params::opt_str(params, "prefab")?.unwrap_or("scorch");
    let lifetime = optional_lifetime(params)?;
    let prefab = library(world)
        .get(prefab_name)
        .cloned()
        .ok_or_else(|| RpcError::invalid_params(format!("unknown decal prefab '{prefab_name}'")))?;
    let (decal, surface) = prefab
        .instantiate(position, normal, yaw, lifetime)
        .map_err(|err| RpcError::invalid_params(format!("decal prefab '{prefab_name}': {err}")))?;
    let axis = decal.rotation * Vec3::Z;
    let id = {
        let mut pool = world.resource_mut::<DecalPool>();
        let id = pool.spawn_with(decal, surface);
        pool.set_footstep_surface(id, prefab.surface.as_deref());
        id
    };
    let pool = world.resource::<DecalPool>();
    Ok(json!({
        "index": pool.index_of(id),
        "slot": id.slot,
        "generation": id.generation,
        "live": pool.len(),
        "capacity": pool.capacity(),
        "prefab": prefab_name,
        "texture": prefab.texture,
        "surface": prefab.surface,
        "atlas_layer": surface.atlas_layer,
        "normal": [axis.x, axis.y, axis.z],
    }))
}

/// `vfx.place_rut`: stamp a decal strip along `points`.
///
/// Spacing below 0.05 m is raised to 0.05, and `half_width` / `half_depth` below 0.01 m are
/// raised to 0.01. The length check sums every supplied segment before sampling. With
/// `curve: true` and at least three points, the strip follows the quadratic of the first three
/// (12 samples). Later points are not stamped, but they already counted, so a far unused tail
/// is refused.
pub fn place_rut(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_pool(world)?;
    let mut points = point_list(params)?;
    let prefab_name = crate::params::opt_str(params, "prefab")?.unwrap_or("rut");
    let spacing = optional_f32(params, "spacing", 0.45)?.max(0.05);
    let half_width = optional_f32(params, "half_width", 0.18)?.max(0.01);
    let half_depth = optional_f32(params, "half_depth", 0.15)?.max(0.01);
    // The strip stamps every `spacing` metres along the polyline (a curve through the same
    // points is never longer): bound the count before stamping, in f64 so long legs cannot
    // overflow.
    let length: f64 = points
        .windows(2)
        .map(|w| (w[1].as_dvec3() - w[0].as_dvec3()).length())
        .sum();
    if length / f64::from(spacing) > MAX_RUT_STAMPS as f64 {
        return Err(RpcError::invalid_params(format!(
            "the rut is {length:.1} m long; at spacing {spacing} m that is more than {MAX_RUT_STAMPS} stamps"
        )));
    }
    let prefab = library(world)
        .get(prefab_name)
        .cloned()
        .ok_or_else(|| RpcError::invalid_params(format!("unknown decal prefab '{prefab_name}'")))?;
    let (_, surface) = prefab
        .instantiate(Vec3::ZERO, Vec3::Y, 0.0, None)
        .map_err(|err| RpcError::invalid_params(format!("decal prefab '{prefab_name}': {err}")))?;
    if crate::params::opt_bool(params, "curve")?.unwrap_or(false) && points.len() >= 3 {
        points = sample_quadratic(points[0], points[1], points[2], 12);
    }
    // The strip places one stamp per `spacing` along the path; a long path (or one whose f32
    // length stops growing by `spacing`) would never finish, so the stamp count is capped first.
    let length: f32 = points.windows(2).map(|pair| pair[0].distance(pair[1])).sum();
    if !length.is_finite() || length / spacing > MAX_RUT_STAMPS as f32 {
        return Err(RpcError::invalid_params(format!(
            "the rut path is {length} m long: more than {MAX_RUT_STAMPS} stamps at spacing {spacing} m; \
             split it into shorter ruts or widen `spacing`"
        )));
    }
    let stamps = DecalStrip {
        points,
        normal: Vec3::Y,
        half_width,
        half_depth,
        spacing,
        surface,
        lifetime: None,
    }
    .stamps();
    let placed = stamps.len();
    {
        let mut pool = world.resource_mut::<DecalPool>();
        for (decal, surface) in stamps {
            pool.spawn_with(decal, surface);
        }
    }
    let pool = world.resource::<DecalPool>();
    Ok(json!({
        "placed": placed,
        "live": pool.len(),
        "capacity": pool.capacity(),
    }))
}

/// `vfx.decal_budget`: capacity, live count, and a CPU cull against [`VfxView`].
///
/// The cull runs here, on the call, so a match that never opens a view does not pay for tiled
/// binning every frame. With no [`VfxView`], `culled` and `tiles_touched` stay 0 (or the last
/// value a renderer stored on [`DecalCullStats`]).
///
/// `shading` (ADR-3255) says which path shades each decal (`post`, `forward` or `split`, in spawn
/// order) under the forward-decal switch, and, with a [`VfxView`], the forward pass's tile counts
/// for that view (`regia_render::forward_decals::pack_forward_decals`).
pub fn decal_budget(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    require_pool(world)?;
    let view = world.get_resource::<VfxView>().copied();
    let stored = world.get_resource::<DecalCullStats>().copied();
    let pool = world.resource::<DecalPool>();
    let capacity = pool.capacity();
    let live = u32::try_from(pool.len()).unwrap_or(u32::MAX);
    let (culled, tiles_touched) = if let Some(view) = view {
        let aspect = view.width.max(1) as f32 / view.height.max(1) as f32;
        let (_, _, view_proj) = view.camera.matrices(aspect);
        // What the draw uploads: hidden static decals are left out (ADR-7255).
        let hidden = world
            .get_resource::<regia_vfx::HiddenStaticDecals>()
            .map_or(&[][..], |hidden| hidden.ids());
        let mut scratch = regia_vfx::DecalDrawScratch::default();
        let (staged, staged_surfaces) = pool.visible_in_spawn_order(hidden, &mut scratch);
        let bin = bin_decals(
            &staged,
            &staged_surfaces,
            view_proj,
            view.camera.eye,
            view.width,
            view.height,
            DECAL_TILE_PX,
        );
        (
            u32::try_from(bin.culled).unwrap_or(u32::MAX),
            u32::try_from(bin.tiles_touched()).unwrap_or(u32::MAX),
        )
    } else {
        stored
            .map(|stats| (stats.culled, stats.tiles_touched))
            .unwrap_or((0, 0))
    };
    let shading = decal_shading_json(pool, view);
    Ok(json!({
        "capacity": capacity,
        "live": live,
        "culled": culled,
        "tiles_touched": tiles_touched,
        "shading": shading,
    }))
}

/// The `shading` object of `vfx.decal_budget` (ADR-3255).
fn decal_shading_json(pool: &DecalPool, view: Option<VfxView>) -> Value {
    use regia_render::forward_decals as fd;
    let switch = regia_render::process_forward_decals();
    let (staged, staged_surfaces) = pool.in_spawn_order();
    let route = regia_vfx::route_decals(&staged, &staged_surfaces, switch);
    let (post, forward, split) = route.counts();
    let tiles = view.filter(|_| !route.forward.is_empty()).map(|view| {
        let aspect = view.width.max(1) as f32 / view.height.max(1) as f32;
        let (view_m, _, view_proj) = view.camera.matrices(aspect);
        let pack = fd::pack_forward_decals(
            &route.forward,
            &fd::ForwardDecalView {
                view_proj,
                view: view_m,
                eye: view.camera.eye,
                width: view.width,
                height: view.height,
            },
        );
        let s = pack.stats;
        json!({
            "packed": s.packed,
            "dropped": s.dropped,
            "culled": s.culled,
            "tile_px": s.tile_px,
            "tiles_x": s.tiles_x,
            "tiles_y": s.tiles_y,
            "tiles_touched": s.tiles_touched,
            "max_per_tile": s.max_per_tile,
            "tile_entries": s.tile_entries,
        })
    });
    json!({
        "forward_switch": switch,
        "paths": route.paths.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        "post": post,
        "forward": forward,
        "split": split,
        "forward_max": fd::FORWARD_DECAL_MAX,
        "forward_tiles": tiles,
    })
}

fn look_json(world: &World, look: VfxLook) -> Value {
    let lights = world.get_resource::<VfxFrameLights>();
    let live = lights.map_or(0, |l| l.lights.len());
    let strongest = lights.and_then(|l| l.lights.first()).map(|l| {
        json!({
            "position": [l.position.x, l.position.y, l.position.z],
            "color": l.color,
            "radius": l.radius,
        })
    });
    json!({
        "heat_haze": look.heat_haze,
        "haze_scale": look.haze_scale,
        "fire_light": look.fire_light,
        "fire_light_scale": look.fire_light_scale,
        "fire_lights_live": live,
        "strongest_light": strongest,
        "soft_particle_res": look.soft_particle_res.map(regia_vfx::gpu::SoftParticleRes::name),
        "soft_particle_res_profile": world
            .get_resource::<regia_vfx::VfxSettings>()
            .map(|s| s.profile.soft_particle_res.name()),
    })
}

fn require_look(world: &World) -> Result<VfxLook, RpcError> {
    world.get_resource::<VfxLook>().copied().ok_or_else(|| {
        RpcError::new(
            protocol::UNSUPPORTED,
            "F0016",
            "no VFX look in this app",
            "add regia_vfx::VfxPlugin so vfx.get_look / vfx.set_look have a VfxLook",
        )
    })
}

/// `vfx.get_look`: heat haze and fire light switches, their scales, and this frame's
/// particle lights (count and the strongest one).
pub fn get_look(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let look = require_look(world)?;
    Ok(look_json(world, look))
}

fn optional_bool(params: &Value, field: &str) -> Result<Option<bool>, RpcError> {
    match params.get(field) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a boolean"))),
    }
}

fn optional_scale(params: &Value, field: &str, current: f32, max: f32) -> Result<f32, RpcError> {
    let value = optional_f32(params, field, current)?;
    if !(0.0..=max).contains(&value) {
        return Err(RpcError::invalid_params(format!(
            "`{field}` must be in 0..={max}"
        )));
    }
    Ok(value)
}

/// `vfx.medium`: the air effects spawn in (from the level's atmosphere preset and gravity) and
/// which effect names play as a medium variant (`explosion` → `explosion@airless`).
pub fn medium(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(scene) = world.get_resource::<regia_vfx::VfxScene>() else {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0016",
            "no VFX scene in this app",
            "add regia_vfx::VfxPlugin so vfx.medium has a scene",
        ));
    };
    let m = scene.medium();
    let mut variants = Vec::new();
    if let Some(suffix) = m.variant_suffix() {
        for def in scene.library().defs() {
            if let Some(base) = def.name.strip_suffix(suffix)
                && scene.library().index(base).is_some()
                && scene.resolve(base) == def.name
            {
                variants.push(json!({ "effect": base, "plays": def.name }));
            }
        }
    }
    Ok(json!({
        "atmosphere": m.atmosphere.name(),
        "gravity": m.gravity,
        "wind": [m.wind.x, m.wind.y, m.wind.z],
        "variant_suffix": m.variant_suffix(),
        "variants": variants,
    }))
}

fn hover_json(world: &World, entity: Entity, wash: &regia_vfx::HoverWash) -> Value {
    let medium = world
        .get_resource::<regia_vfx::VfxScene>()
        .map(regia_vfx::VfxScene::medium)
        .unwrap_or_default();
    json!({
        "entity": entity.to_bits(),
        "surface": wash.surface.name(),
        "resolved_surface": wash.surface.resolve(&medium).name(),
        "footprint_radius": wash.footprint_radius,
        "height": wash.height,
        "ground": [wash.ground.x, wash.ground.y, wash.ground.z],
        "ground_normal": [wash.ground_normal.x, wash.ground_normal.y, wash.ground_normal.z],
        "enabled": wash.enabled,
        "intensity": wash.intensity(),
        "expected_intensity": if wash.enabled {
            regia_vfx::hover_intensity(wash.height, wash.footprint_radius)
        } else {
            0.0
        },
        "playing": wash.playing(),
    })
}

/// `vfx.hover_washes`: every hull's hover wash (surface, height, footprint, the intensity the
/// height gives, and the effect playing, e.g. `hover_wash_ice@airless`).
pub fn hover_washes(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut query = world.query::<(Entity, &regia_vfx::HoverWash)>();
    let list: Vec<(Entity, regia_vfx::HoverWash)> = query.iter(world).map(|(e, w)| (e, w.clone())).collect();
    let washes: Vec<Value> = list.iter().map(|(e, w)| hover_json(world, *e, w)).collect();
    Ok(json!({ "count": washes.len(), "washes": washes }))
}

/// `vfx.set_hover_wash`: adds or changes the hover wash on `entity`; omitted fields keep their
/// value (a new wash starts from the defaults). Returns `{ before, after }` (`before` is null
/// for a new wash). The effect follows on the next frame.
pub fn set_hover_wash(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "use scene.query_entities",
        ));
    }
    let before = world.get::<regia_vfx::HoverWash>(entity).cloned();
    let mut after = before.clone().unwrap_or_default();
    if let Some(name) = params.get("surface") {
        let name = name
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`surface` must be a string"))?;
        after.surface = regia_vfx::HoverSurface::from_name(name)
            .ok_or_else(|| RpcError::invalid_params(format!("`surface` {name}: auto, dust, ice or frost")))?;
    }
    after.footprint_radius = optional_f32(params, "footprint_radius", after.footprint_radius)?;
    if !(0.1..=50.0).contains(&after.footprint_radius) {
        return Err(RpcError::invalid_params(
            "`footprint_radius` must be in 0.1..=50 m",
        ));
    }
    after.height = optional_f32(params, "height", after.height)?;
    if params.get("ground").is_some() {
        after.ground = vec3_param(params, "ground")?;
    }
    if params.get("ground_normal").is_some() {
        after.ground_normal = vec3_param(params, "ground_normal")?.normalize_or(Vec3::Y);
    }
    if let Some(on) = optional_bool(params, "enabled")? {
        after.enabled = on;
    }
    world.entity_mut(entity).insert(after.clone());
    Ok(json!({
        "before": before.as_ref().map(|w| hover_json(world, entity, w)),
        "after": hover_json(world, entity, &after),
    }))
}

/// `vfx.hit`: plays one round landing (`regia_vfx::WeaponHit`): the surface's hit effect in the
/// level's medium, its decals (on `target` when given) and, on glass, an impact on the target's
/// `GlassDamage`. The calibre comes from `calibre` or a DO27 `weapon` id.
pub fn hit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<regia_vfx::VfxScene>().is_none() {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0016",
            "no VFX scene in this app",
            "add regia_vfx::VfxPlugin so vfx.hit has a scene",
        ));
    }
    let position = vec3_param(params, "position")?;
    let normal = vec3_param(params, "normal")?;
    let surface_name = params
        .get("surface")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`surface` (metal, concrete or glass) is required"))?;
    let surface = regia_vfx::HitSurface::from_name(surface_name).ok_or_else(|| {
        RpcError::invalid_params(format!("`surface` {surface_name}: metal, concrete or glass"))
    })?;
    let calibre = match (
        crate::params::opt_str(params, "calibre")?,
        crate::params::opt_str(params, "weapon")?,
    ) {
        (Some(c), _) => regia_vfx::Calibre::from_name(c)
            .ok_or_else(|| RpcError::invalid_params(format!("`calibre` {c}: light, medium or heavy")))?,
        (None, Some(w)) => regia_vfx::combat::weapon_calibre(w).ok_or_else(|| {
            RpcError::invalid_params(format!("`weapon` {w} is not a DO27 weapon (see vfx.weapons)"))
        })?,
        (None, None) => regia_vfx::Calibre::Medium,
    };
    let target = match params.get("target") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let bits = v
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`target` must be an entity id"))?;
            let e = Entity::try_from_bits(bits)
                .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
            if world.get_entity(e).is_err() {
                return Err(RpcError::not_found(
                    format!("entity {bits} does not exist"),
                    "use scene.query_entities",
                ));
            }
            Some(e)
        }
    };
    let out = regia_vfx::apply_weapon_hit(
        world,
        &regia_vfx::WeaponHit {
            position,
            normal,
            surface,
            calibre,
            target,
            seed: crate::params::opt_u64(params, "seed")?,
        },
    );
    Ok(json!({
        "effect": out.effect,
        "calibre": calibre.name(),
        "decals": out.decals,
        "glass_impact": out.glass_impact,
    }))
}

fn plume_json(entity: Entity, plume: &regia_vfx::ThrusterPlume) -> Value {
    json!({
        "entity": entity.to_bits(),
        "kind": plume.kind.name(),
        "throttle": plume.throttle,
        "exhaust": [plume.exhaust.x, plume.exhaust.y, plume.exhaust.z],
        "enabled": plume.enabled,
        "playing": plume.playing(),
    })
}

/// `vfx.plumes`: every thruster plume (kind, throttle, exhaust direction, the effect playing,
/// e.g. `plume_duct@airless`), plus DO27's thruster table.
pub fn plumes(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut query = world.query::<(Entity, &regia_vfx::ThrusterPlume)>();
    let list: Vec<Value> = query.iter(world).map(|(e, p)| plume_json(e, p)).collect();
    let units: Vec<Value> = regia_vfx::plume::DO27_THRUSTERS
        .iter()
        .map(|t| {
            json!({
                "unit": t.unit,
                "kind": t.kind.name(),
                "sockets": t.sockets,
                "exhaust": [t.exhaust.0, t.exhaust.1, t.exhaust.2],
            })
        })
        .collect();
    Ok(json!({ "count": list.len(), "plumes": list, "do27_units": units }))
}

/// `vfx.set_plume`: adds or changes the thruster plume on `entity` (a nozzle entity); omitted
/// fields keep their value. Returns `{ before, after }`.
pub fn set_plume(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "use scene.query_entities",
        ));
    }
    let before = world.get::<regia_vfx::ThrusterPlume>(entity).cloned();
    let mut after = before.clone().unwrap_or_default();
    if let Some(name) = params.get("kind") {
        let name = name
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`kind` must be a string"))?;
        after.kind = regia_vfx::PlumeKind::from_name(name)
            .ok_or_else(|| RpcError::invalid_params(format!("`kind` {name}: duct or jetpack")))?;
    }
    after.throttle = optional_f32(params, "throttle", after.throttle)?;
    if !(0.0..=2.0).contains(&after.throttle) {
        return Err(RpcError::invalid_params("`throttle` must be in 0..=2"));
    }
    if params.get("exhaust").is_some() {
        after.exhaust = vec3_param(params, "exhaust")?.normalize_or(Vec3::NEG_Y);
    }
    if let Some(on) = optional_bool(params, "enabled")? {
        after.enabled = on;
    }
    world.entity_mut(entity).insert(after.clone());
    Ok(json!({
        "before": before.as_ref().map(|p| plume_json(entity, p)),
        "after": plume_json(entity, &after),
    }))
}

/// `vfx.weapons`: DO27's weapons and the muzzle, tracer and hit each plays.
pub fn weapons(_world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    use regia_vfx::combat::{DO27_WEAPONS, HitKind};
    let list: Vec<Value> = DO27_WEAPONS
        .iter()
        .map(|w| {
            let hit = match w.hit {
                HitKind::Bullet(c) => json!({ "kind": "bullet", "calibre": c.name() }),
                HitKind::Explosive(scale) => json!({ "kind": "explosive", "scale": scale }),
                HitKind::Beam => json!({ "kind": "beam", "calibre": "light" }),
            };
            json!({ "weapon": w.weapon, "muzzle": w.muzzle, "tracer": w.tracer, "hit": hit })
        })
        .collect();
    Ok(json!({ "weapons": list }))
}

/// `vfx.set_look`: toggles heat haze and the fire light and sets their intensity scales.
/// Fields left out keep their value. Returns `{ before, after }`.
pub fn set_look(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let before = require_look(world)?;
    let mut after = before;
    if let Some(on) = optional_bool(params, "heat_haze")? {
        after.heat_haze = on;
    }
    after.haze_scale = optional_scale(params, "haze_scale", before.haze_scale, 4.0)?;
    if let Some(on) = optional_bool(params, "fire_light")? {
        after.fire_light = on;
    }
    after.fire_light_scale = optional_scale(params, "fire_light_scale", before.fire_light_scale, 8.0)?;
    if let Some(v) = params.get("soft_particle_res") {
        after.soft_particle_res = match v {
            Value::Null => None,
            Value::String(name) => {
                Some(regia_vfx::gpu::SoftParticleRes::from_name(name).ok_or_else(|| {
                    RpcError::invalid_params(format!("`soft_particle_res` {name}: full, half or quarter"))
                })?)
            }
            _ => {
                return Err(RpcError::invalid_params(
                    "`soft_particle_res` must be full, half, quarter or null",
                ));
            }
        };
    }
    world.insert_resource(after);
    Ok(json!({
        "before": look_json(world, before),
        "after": look_json(world, after),
    }))
}

/// `vfx.stats`: the particle budgets this frame. The profile's pool capacity and smoke
/// budget, what the CPU scheduler counts against them (live, alpha, mesh, pending uploads),
/// and each live instance against its system budget.
pub fn stats(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(scene) = world.get_resource::<regia_vfx::VfxScene>() else {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0016",
            "no VFX scene in this app",
            "add regia_vfx::VfxPlugin so vfx.stats has a scene",
        ));
    };
    let profile = world
        .get_resource::<regia_vfx::VfxSettings>()
        .map(|settings| settings.profile)
        .unwrap_or_default();
    let stats = scene.budget_stats();
    let instances: Vec<Value> = stats
        .instances
        .iter()
        .map(|i| {
            json!({
                "id": i.id,
                "effect": i.effect,
                "live": i.live,
                "budget": i.budget,
                "active": i.active,
                "age": i.age,
            })
        })
        .collect();
    Ok(json!({
        "capacity": profile.particle_capacity,
        "smoke_budget": profile.smoke_budget,
        "sorted": profile.sort,
        "live": stats.live,
        "live_alpha": stats.live_alpha,
        "live_mesh": stats.live_mesh,
        "headroom": profile.particle_capacity.saturating_sub(stats.live),
        "smoke_headroom": profile.smoke_budget.saturating_sub(stats.live_alpha),
        "last_scheduled": stats.last_scheduled,
        "pending_jobs": stats.pending_jobs,
        "instances": instances,
    }))
}

/// `vfx.entry_state`: evaluates the atmospheric entry effect (`regia_vfx::entry`) for a track at
/// a time. Read-only and pure: the same params always return the same state.
pub fn entry_state(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_vfx::entry::{EntryProfile, EntryTier, EntryTrack, TrackPoint, evaluate};
    let list = params
        .get("track")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("`track` ([{ time, position }, ...]) is required"))?;
    let mut points = Vec::with_capacity(list.len());
    for item in list {
        let time = optional_f32(item, "time", f32::NAN)?;
        points.push(TrackPoint {
            time,
            position: vec3_param(item, "position")?,
        });
    }
    let track = EntryTrack::new(points).map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let time = optional_f32(params, "time", track.start_time())?;
    let tier = match params.get("tier").map(Value::as_str) {
        None | Some(Some("full")) => EntryTier::Full,
        Some(Some("cheap")) => EntryTier::Cheap,
        _ => return Err(RpcError::invalid_params("`tier` must be full or cheap")),
    };
    let defaults = if crate::params::opt_bool(params, "legacy_dust")? == Some(true) {
        EntryProfile::legacy_dust()
    } else {
        EntryProfile::default()
    };
    let profile = EntryProfile {
        dust_opacity_scale: optional_f32(params, "dust_opacity_scale", defaults.dust_opacity_scale)?
            .clamp(0.0, 4.0),
        dust_life_s: optional_f32(params, "dust_life_s", defaults.dust_life_s)?.clamp(0.25, 3600.0),
        ground_y: optional_f32(params, "ground_y", defaults.ground_y)?,
        wind: optional_vec3(params, "wind", defaults.wind)?,
        shed_count: optional_f32(params, "shed_count", defaults.shed_count as f32)?.clamp(0.0, 64.0) as u32,
        seed: optional_f32(params, "seed", 0.0)?.max(0.0) as u64 ^ defaults.seed,
        ..defaults
    };
    let frame = evaluate(&track, &profile, tier, time);
    let head = &frame.head;
    // Each piece relative to the head: how far back along its heading, and how far off that line.
    let piece_streaks: Vec<Value> = frame
        .pieces
        .iter()
        .map(|piece| {
            let offset = head.position - piece.position;
            let behind = offset.dot(head.direction);
            json!({
                "behind_m": behind,
                "aside_m": (offset - head.direction * behind).length(),
                "speed": piece.speed,
                "age_s": piece.age_s,
                "life_s": piece.life_s,
                "glow": piece.glow,
                "glow_share": if head.glow > 0.0 { piece.glow / head.glow } else { 0.0 },
                "temperature_k": piece.temperature_k,
            })
        })
        .collect();
    Ok(json!({
        "time": frame.time_s,
        "head": {
            "position": [head.position.x, head.position.y, head.position.z],
            "altitude_m": head.altitude_m,
            "speed": head.speed,
            "glow": head.glow,
            "radiance": head.radiance,
            "color": head.color,
            "temperature_k": regia_vfx::entry::head_temperature_k(head.speed),
            "dark": head.glow <= 0.0,
        },
        "glow_ceiling_m": regia_vfx::entry::GLOW_CEILING_M,
        "ion_points": frame.ion.len(),
        "train_points": frame.train.len(),
        "train_peak_opacity": frame.train.iter().map(|p| p.opacity).fold(0.0f32, f32::max),
        "ion_tail_temperature_k": frame.ion.last().map(|p| p.temperature_k),
        "ember_points": frame.train.iter().filter(|p| p.ember > 0.0).count(),
        "train_max_half_width_m": frame.train.iter().map(|p| p.half_width_m).fold(0.0f32, f32::max),
        "glow_range_k": [regia_vfx::entry::VISIBLE_MIN_K, regia_vfx::entry::VISIBLE_MAX_K],
        "pieces": frame.pieces.len(),
        "piece_streaks": piece_streaks,
    }))
}

/// `vfx.damage_decal_atlas`: the cook-side damage atlas size, and the pixels only when asked.
///
/// `generate` defaults to false, so an empty call does not build the image.
pub fn damage_decal_atlas(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let generate = crate::params::opt_bool(params, "generate")
        .map_err(|err| decal_atlas_input(err.message))?
        .unwrap_or(false);
    let mut value = json!({
        "width": regia_vfx::DAMAGE_DECAL_ATLAS_SIZE,
        "height": regia_vfx::DAMAGE_DECAL_ATLAS_SIZE,
        "cells": regia_vfx::DAMAGE_DECAL_CELLS,
        "cell_px": regia_vfx::DAMAGE_DECAL_CELL,
        "generated": generate,
    });
    if generate {
        let bytes = regia_vfx::generate_damage_decal_atlas();
        let hash = regia_vfx::damage_decal_atlas_fnv1a64(&bytes);
        value["bytes"] = json!(bytes.len());
        value["hash"] = json!(format!("{hash:016x}"));
    }
    Ok(value)
}

/// `vfx.decal_surface_at`: the footstep surface a live decal sets at `point` (decal v3 `surface`,
/// ADR-8829), or null when no surface decal covers it. Read only; never touches navigation.
pub fn decal_surface_at(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_pool(world)?;
    let point = vec3_param(params, "point")?;
    let pool = world.resource::<DecalPool>();
    Ok(json!({
        "surface": pool.footstep_surface_at(point),
        "live": pool.len(),
    }))
}

fn decal_atlas_input(message: impl Into<String>) -> RpcError {
    RpcError {
        code: crate::protocol::INVALID_PARAMS,
        message: message.into(),
        data: Some(json!({
            "code": "F0002",
            "reason": "decal_atlas_input",
            "hint": "see book/agents/decal-atlas-walkability.md",
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;
    use serde_json::json;

    #[test]
    fn damage_decal_atlas_skips_pixels_until_asked() {
        let mut app = App::new();
        let value = damage_decal_atlas(app.world_mut(), &json!({})).expect("size only");
        assert_eq!(value["generated"], json!(false));
        assert_eq!(value["width"], json!(1024));
        assert_eq!(value["cells"], json!(16));
        assert!(
            value.get("bytes").is_none(),
            "an empty call does not build pixels"
        );
        let err = damage_decal_atlas(app.world_mut(), &json!({ "generate": "yes" })).expect_err("bool");
        assert_eq!(err.data.as_ref().expect("data")["reason"], "decal_atlas_input");
        assert_eq!(err.data.as_ref().expect("data")["code"], "F0002");
    }

    #[test]
    fn stats_report_the_budgets() {
        let mut lib = regia_vfx::scene::VfxLibrary::new();
        lib.add(regia_vfx::showcase::explosion()).expect("explosion");
        let mut scene = regia_vfx::VfxScene::new(lib);
        let id = scene.spawn("explosion", Vec3::ZERO, Vec3::Y, 1).expect("spawn");
        let lod = regia_vfx::sim::LodInput {
            camera_pos: Vec3::new(0.0, 2.0, -10.0),
        };
        let settings = regia_vfx::VfxSettings::default();
        let (capacity, smoke_budget) = (settings.profile.particle_capacity, settings.profile.smoke_budget);
        for _ in 0..6 {
            scene.update_budgets(1.0 / 60.0, &lod, capacity, smoke_budget);
        }
        let mut app = App::new();
        app.insert_resource(scene);
        app.insert_resource(settings);
        let out = stats(app.world_mut(), &json!({})).expect("stats");
        assert_eq!(out["capacity"].as_u64(), Some(u64::from(capacity)));
        assert_eq!(out["smoke_budget"].as_u64(), Some(u64::from(smoke_budget)));
        let live = out["live"].as_u64().expect("live");
        assert!(live > 0, "the explosion is live: {out}");
        assert_eq!(out["headroom"].as_u64(), Some(u64::from(capacity) - live));
        let instance = &out["instances"][0];
        assert_eq!(instance["id"].as_u64(), Some(id));
        assert_eq!(instance["effect"], "explosion");
        assert_eq!(instance["active"], true);
        assert!(instance["live"].as_u64() <= instance["budget"].as_u64());
        assert_eq!(
            instance["live"].as_u64(),
            Some(live),
            "one instance holds every particle"
        );
        let err = stats(App::new().world_mut(), &json!({})).expect_err("no scene");
        assert_eq!(err.code, protocol::UNSUPPORTED);
    }

    #[test]
    fn hover_wash_and_hit_methods_drive_the_effects() {
        let mut app = App::new();
        app.add_plugins(regia_vfx::VfxPlugin);
        let mut level = regia_scene::level::LevelSettings::default();
        level.atmosphere.preset = regia_scene::level::AtmospherePreset::Airless;
        level.gravity = Vec3::new(0.0, -1.315, 0.0);
        app.insert_resource(level);
        app.update();
        let hull = app.world_mut().spawn_empty().id();
        let set = set_hover_wash(
            app.world_mut(),
            &json!({ "entity": hull.to_bits(), "footprint_radius": 3.0, "height": 3.0, "ground": [1.0, 0.0, 2.0] }),
        )
        .expect("set");
        assert!(set["before"].is_null());
        assert_eq!(set["after"]["resolved_surface"], "ice");
        app.update();
        let list = hover_washes(app.world_mut(), &json!({})).expect("list");
        assert_eq!(list["count"], 1);
        let wash = &list["washes"][0];
        assert_eq!(wash["playing"], "hover_wash_ice@airless");
        assert!((wash["intensity"].as_f64().unwrap_or_default() - 0.25).abs() < 1e-5);
        assert!(
            set_hover_wash(
                app.world_mut(),
                &json!({ "entity": hull.to_bits(), "surface": "lava" })
            )
            .is_err()
        );
        let out = hit(
            app.world_mut(),
            &json!({ "position": [0.0, 1.0, 0.0], "normal": [0.0, 0.0, 1.0], "surface": "metal", "weapon": "guntower" }),
        )
        .expect("hit");
        assert_eq!(out["effect"], "hit_metal@airless");
        assert_eq!(out["calibre"], "heavy");
        assert_eq!(out["decals"], 2);
        assert!(
            hit(
                app.world_mut(),
                &json!({ "position": [0, 0, 0], "normal": [0, 1, 0], "surface": "wood" })
            )
            .is_err()
        );
        let w = weapons(app.world_mut(), &json!({})).expect("weapons");
        let names: Vec<&str> = w["weapons"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x["weapon"].as_str()).collect())
            .unwrap_or_default();
        assert!(names.contains(&"chaincannon") && names.contains(&"cutterbeam"));
    }

    #[test]
    fn plume_methods_follow_the_level_air() {
        let mut app = App::new();
        app.add_plugins(regia_vfx::VfxPlugin);
        let mut level = regia_scene::level::LevelSettings::default();
        level.atmosphere.preset = regia_scene::level::AtmospherePreset::Mars;
        level.gravity = Vec3::new(0.0, -3.71, 0.0);
        app.insert_resource(level);
        app.update();
        let nozzle = app.world_mut().spawn_empty().id();
        let set = set_plume(
            app.world_mut(),
            &json!({ "entity": nozzle.to_bits(), "kind": "jetpack", "throttle": 0.7 }),
        )
        .expect("set");
        assert!(set["before"].is_null());
        app.update();
        let list = plumes(app.world_mut(), &json!({})).expect("list");
        assert_eq!(list["count"], 1);
        assert_eq!(list["plumes"][0]["playing"], "plume_jetpack@mars");
        assert!(list["do27_units"].as_array().is_some_and(|a| a.len() >= 3));
        assert!(
            set_plume(
                app.world_mut(),
                &json!({ "entity": nozzle.to_bits(), "throttle": 5.0 })
            )
            .is_err()
        );
        assert!(
            set_plume(
                app.world_mut(),
                &json!({ "entity": nozzle.to_bits(), "kind": "ion" })
            )
            .is_err()
        );
    }

    #[test]
    fn medium_reports_the_level_air_and_its_variants() {
        let mut app = App::new();
        let mut lib = regia_vfx::scene::VfxLibrary::new();
        lib.add(regia_vfx::showcase::explosion()).expect("base");
        let airless = regia_vfx::VfxMedium::airless(1.315);
        for def in regia_vfx::showcase::variants_in(&airless) {
            lib.add(def).expect("variant");
        }
        let mut scene = regia_vfx::VfxScene::new(lib);
        scene.set_medium(airless);
        app.insert_resource(scene);
        let out = medium(app.world_mut(), &json!({})).expect("medium");
        assert_eq!(out["atmosphere"], "airless");
        assert!((out["gravity"].as_f64().unwrap_or(0.0) - 1.315).abs() < 1e-4);
        assert_eq!(out["variants"][0]["plays"], "explosion@airless");
        let empty = App::new()
            .world_mut()
            .get_resource::<regia_vfx::VfxScene>()
            .is_none();
        assert!(empty);
        assert!(medium(App::new().world_mut(), &json!({})).is_err());
    }

    fn app_with_pool() -> App {
        let mut app = App::new();
        app.insert_resource(DecalPool::new(32));
        app.insert_resource(DecalLibrary::builtin());
        app.insert_resource(DecalCullStats::default());
        app
    }

    #[test]
    fn custom_decal_texture_dispatch_reports_resolution_and_refuses_unloaded_images() {
        let mut app = app_with_pool();
        app.init_resource::<crate::AgentPermissions>()
            .init_resource::<crate::dispatch::RateLimit>()
            .init_resource::<crate::safety::AgentSession>();
        let prefab = regia_vfx::DecalPrefab::from_ron(
            r#"(version: 3, name: "paint", half_size: (1.0, 1.0, 0.2), texture: Some("paint.fckd"))"#,
        )
        .expect("prefab");
        let image = regia_asset::CookedImage {
            width: 1,
            height: 1,
            rgba8: vec![255, 0, 0, 255],
        };
        app.world_mut()
            .resource_mut::<DecalLibrary>()
            .add_textured(prefab.clone(), &image)
            .expect("image");
        let params = json!({"position": [0, 0, 0], "prefab": "paint"});
        let result = crate::dispatch::call_in_process(app.world_mut(), "vfx.spawn_decal", params.clone())
            .expect("synchronous")
            .expect("spawn through dispatcher");
        assert_eq!(result["texture"], "paint.fckd");
        assert_eq!(result["atlas_layer"], 4);
        crate::contract::assert_result_named("vfx.spawn_decal", &result);
        assert_eq!(app.world().resource::<DecalPool>().surfaces()[0].atlas_layer, 4);
        app.world_mut().resource_mut::<DecalLibrary>().add(prefab);
        let error = crate::dispatch::call_in_process(app.world_mut(), "vfx.spawn_decal", params)
            .expect("synchronous")
            .expect_err("unresolved image");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        assert_eq!(
            app.world().resource::<DecalPool>().len(),
            1,
            "failure adds no decal"
        );
    }

    /// ADR-8829: a v3 prefab's footstep `surface` reaches the pool through `vfx.spawn_decal`, and
    /// `vfx.decal_surface_at` answers it inside the decal's box and null outside it.
    #[test]
    fn decal_surface_at_answers_the_footstep_surface_of_a_spawned_v3_decal() {
        let mut app = app_with_pool();
        app.init_resource::<crate::AgentPermissions>()
            .init_resource::<crate::dispatch::RateLimit>()
            .init_resource::<crate::safety::AgentSession>();
        let prefab = regia_vfx::DecalPrefab::from_ron(
            r#"(version: 3, name: "puddle", half_size: (1.0, 1.0, 0.3), surface: Some("puddle"))"#,
        )
        .expect("prefab");
        app.world_mut().resource_mut::<DecalLibrary>().add(prefab);
        let result = crate::dispatch::call_in_process(
            app.world_mut(),
            "vfx.spawn_decal",
            json!({"position": [2.0, 0.0, 0.0], "prefab": "puddle"}),
        )
        .expect("synchronous")
        .expect("spawn");
        assert_eq!(result["surface"], "puddle");
        crate::contract::assert_result_named("vfx.spawn_decal", &result);
        let call = |app: &mut App, point: Value| {
            crate::dispatch::call_in_process(
                app.world_mut(),
                "vfx.decal_surface_at",
                json!({ "point": point }),
            )
            .expect("synchronous")
        };
        let inside = call(&mut app, json!([2.5, 0.0, -0.5])).expect("inside");
        assert_eq!(inside["surface"], "puddle");
        assert_eq!(inside["live"], 1);
        crate::contract::assert_result_named("vfx.decal_surface_at", &inside);
        let outside = call(&mut app, json!([3.5, 0.0, 0.0])).expect("outside");
        assert_eq!(outside["surface"], Value::Null);
        let scorch = crate::dispatch::call_in_process(
            app.world_mut(),
            "vfx.spawn_decal",
            json!({"position": [0.0, 0.0, 0.0], "prefab": "scorch"}),
        )
        .expect("synchronous")
        .expect("spawn");
        assert_eq!(scorch["surface"], Value::Null, "built-ins carry no surface");
        assert_eq!(
            call(&mut app, json!([0.0, 0.0, 0.0])).expect("scorch")["surface"],
            Value::Null
        );
        let bad = call(&mut app, json!([0.0, "x", 0.0])).expect_err("bad point");
        assert_eq!(bad.code, protocol::INVALID_PARAMS);
        let none =
            decal_surface_at(App::new().world_mut(), &json!({ "point": [0, 0, 0] })).expect_err("no pool");
        assert_eq!(none.code, protocol::UNSUPPORTED);
    }

    #[test]
    fn spawn_aligns_projection_to_the_normal() {
        let mut app = app_with_pool();
        let result = spawn_decal(
            app.world_mut(),
            &json!({ "position": [1.0, 0.0, 2.0], "normal": [0.0, 1.0, 0.0], "prefab": "scorch" }),
        )
        .expect("spawn");
        let normal = result["normal"].as_array().expect("normal");
        let axis = Vec3::new(
            normal[0].as_f64().expect("x") as f32,
            normal[1].as_f64().expect("y") as f32,
            normal[2].as_f64().expect("z") as f32,
        );
        assert!(axis.dot(Vec3::Y) > 0.99, "local +Z should be +Y, got {axis}");
        assert_eq!(result["prefab"], "scorch");
        assert!(result["live"].as_u64().expect("live") >= 1);
        assert!(result.get("slot").is_some());
        assert!(result.get("generation").is_some());
        let schema = crate::schema::method("vfx.spawn_decal").expect("schema");
        assert!(schema.result.contains("slot") && schema.result.contains("generation"));
    }

    #[test]
    fn rut_places_more_than_one_stamp() {
        let mut app = app_with_pool();
        let result = place_rut(
            app.world_mut(),
            &json!({
                "points": [[-2.0, 0.0, 1.0], [0.0, 0.0, -1.0], [2.0, 0.0, 1.0]],
                "curve": true
            }),
        )
        .expect("rut");
        assert!(result["placed"].as_u64().expect("placed") > 1);
        assert!(result["live"].as_u64().expect("live") > 1);
    }

    /// A path whose stamp count is past the cap, or whose f32 length is not finite, is refused
    /// before the strip is built (an endless stamp loop before ADR-1034).
    #[test]
    fn overlong_ruts_are_refused_before_stamping() {
        let mut app = app_with_pool();
        for points in [
            json!([[0.0, 0.0, 0.0], [2.0e6, 0.0, 0.0]]),
            json!([[0.0, 0.0, 0.0], [1.0e300, 0.0, 0.0]]),
            json!([[0.0, 0.0, 0.0], [10_000.0, 0.0, 0.0]]),
        ] {
            let error = place_rut(app.world_mut(), &json!({ "points": points, "spacing": 0.05 }))
                .expect_err("too long");
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{points}");
        }
        assert_eq!(
            app.world().resource::<DecalPool>().len(),
            0,
            "nothing was stamped"
        );
    }

    /// A far unused tail is refused. A short fourth point is not stamped. Small sizes are raised.
    #[test]
    fn place_rut_counts_dropped_points_and_raises_small_sizes() {
        let mut far = app_with_pool();
        let error = place_rut(
            far.world_mut(),
            &json!({
                "points": [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0], [100000.0, 0.0, 0.0]],
                "curve": true,
                "spacing": 0.45
            }),
        )
        .expect_err("unused tail");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        assert_eq!(far.world().resource::<DecalPool>().len(), 0);

        let three = {
            let mut app = app_with_pool();
            place_rut(
                app.world_mut(),
                &json!({
                    "points": [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]],
                    "curve": true
                }),
            )
            .expect("three")["placed"]
                .as_u64()
                .expect("placed")
        };
        let four = {
            let mut app = app_with_pool();
            place_rut(
                app.world_mut(),
                &json!({
                    "points": [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0], [3.0, 0.0, 0.0]],
                    "curve": true
                }),
            )
            .expect("four")["placed"]
                .as_u64()
                .expect("placed")
        };
        assert_eq!(four, three, "the fourth point is not stamped");

        let placed = |spacing: f32| {
            let mut app = app_with_pool();
            place_rut(
                app.world_mut(),
                &json!({
                    "points": [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]],
                    "spacing": spacing
                }),
            )
            .expect("spacing")["placed"]
                .as_u64()
                .expect("placed")
        };
        assert_eq!(placed(0.01), placed(0.05));

        let mut app = app_with_pool();
        place_rut(
            app.world_mut(),
            &json!({
                "points": [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]],
                "half_width": 0.001,
                "half_depth": 0.001
            }),
        )
        .expect("extents");
        let decals = app.world().resource::<DecalPool>().decals();
        assert!(!decals.is_empty());
        assert!(decals.iter().all(|decal| (decal.half_size.x - 0.01).abs() < 1e-6));
        assert!(decals.iter().all(|decal| (decal.half_size.z - 0.01).abs() < 1e-6));
    }

    #[test]
    fn unknown_prefab_is_invalid_and_a_missing_pool_is_unsupported() {
        let mut app = app_with_pool();
        let err = spawn_decal(
            app.world_mut(),
            &json!({ "position": [0, 0, 0], "prefab": "nope" }),
        )
        .expect_err("unknown");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        let mut empty = App::new();
        let err = decal_budget(empty.world_mut(), &json!({})).expect_err("no pool");
        assert_eq!(err.code, protocol::UNSUPPORTED);
        let budget = decal_budget(app.world_mut(), &json!({})).expect("budget");
        assert_eq!(budget["capacity"], 32);
    }

    #[test]
    fn decal_budget_reports_a_shading_path_per_live_decal() {
        let mut app = app_with_pool();
        spawn_decal(
            app.world_mut(),
            &json!({ "position": [0, 0, 0], "prefab": "scorch" }),
        )
        .expect("spawn");
        let budget = decal_budget(app.world_mut(), &json!({})).expect("budget");
        let shading = &budget["shading"];
        let switch = regia_render::process_forward_decals();
        assert_eq!(shading["forward_switch"], switch);
        assert_eq!(shading["paths"].as_array().map(Vec::len), Some(1));
        // Scorch writes albedo, normal and roughness on world receivers: with the switch on it
        // splits (meshes forward, terrain post); off, it stays on the post pass.
        assert_eq!(shading["paths"][0], if switch { "split" } else { "post" });
        assert_eq!(shading["forward_max"], 64);
        assert!(shading["forward_tiles"].is_null(), "no view, no tile counts");
    }

    #[test]
    fn look_round_trips_and_validates() {
        let mut app = App::new();
        let err = get_look(app.world_mut(), &json!({})).expect_err("no look");
        assert_eq!(err.code, protocol::UNSUPPORTED);
        app.insert_resource(VfxLook::default());
        let mut lights = VfxFrameLights::default();
        lights.gather(
            vec![regia_vfx::lights::ParticleLight {
                position: Vec3::new(1.0, 2.0, 3.0),
                color: [8.0, 5.0, 2.0],
                radius: 9.0,
            }],
            4,
        );
        app.insert_resource(lights);
        let look = get_look(app.world_mut(), &json!({})).expect("get");
        assert_eq!(look["heat_haze"], true);
        assert_eq!(look["fire_lights_live"], 1);
        assert_eq!(look["strongest_light"]["radius"], 9.0);
        let changed = set_look(
            app.world_mut(),
            &json!({ "heat_haze": false, "fire_light_scale": 2.5 }),
        )
        .expect("set");
        assert_eq!(changed["before"]["heat_haze"], true);
        assert_eq!(changed["after"]["heat_haze"], false);
        assert_eq!(changed["after"]["fire_light_scale"], 2.5);
        assert_eq!(changed["after"]["fire_light"], true, "untouched field kept");
        let stored = *app.world().resource::<VfxLook>();
        assert!(!stored.heat_haze && (stored.fire_light_scale - 2.5).abs() < 1e-6);
        for bad in [
            json!({ "haze_scale": -1.0 }),
            json!({ "fire_light_scale": 100.0 }),
            json!({ "fire_light": "yes" }),
        ] {
            let err = set_look(app.world_mut(), &bad).expect_err("invalid");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{bad}");
        }
    }

    #[test]
    fn zero_normal_is_rejected() {
        let mut app = app_with_pool();
        let err = spawn_decal(
            app.world_mut(),
            &json!({ "position": [0, 0, 0], "normal": [0, 0, 0] }),
        )
        .expect_err("zero");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
    }

    /// Showcase shot 3's track at 27 s: the shed pieces trail behind the head along its heading,
    /// barely off that line, each dimmer than the head.
    #[test]
    fn entry_state_reports_pieces_trailing_down_the_path() {
        let mut app = App::new();
        let track = json!([
            { "time": 0.0, "position": [193600.0, 90000.0, 229800.0] },
            { "time": 10.0, "position": [126100.0, 70000.0, 149400.0] },
            { "time": 20.0, "position": [61800.0, 45000.0, 72800.0] },
            { "time": 28.0, "position": [23200.0, 25000.0, 26800.0] },
            { "time": 32.0, "position": [10300.0, 18000.0, 11500.0] },
        ]);
        let out = entry_state(app.world_mut(), &json!({ "track": track, "time": 27.0 })).expect("state");
        let streaks = out["piece_streaks"].as_array().expect("piece_streaks");
        assert!(!streaks.is_empty());
        assert_eq!(out["pieces"].as_u64(), Some(streaks.len() as u64));
        for s in streaks {
            let num = |key: &str| s[key].as_f64().expect(key);
            assert!(num("behind_m") > 0.0, "{s}");
            assert!(num("glow_share") <= 0.6 + 1e-6, "{s}");
            assert!(num("age_s") < num("life_s"), "{s}");
            if num("age_s") > 1.0 {
                assert!(num("aside_m") < 0.25 * num("behind_m"), "{s}");
            }
        }
        // Dark flight: no pieces at all.
        let dark = entry_state(app.world_mut(), &json!({ "track": track, "time": 32.0 })).expect("dark");
        assert_eq!(dark["piece_streaks"].as_array().map(Vec::len), Some(0));
    }

    /// A bad track or tier is invalid params (`F0002`, JSON-RPC -32602), not a missing-entity error.
    #[test]
    fn entry_state_rejects_bad_params_as_f0002() {
        let mut app = App::new();
        let assert_invalid = |err: RpcError| {
            assert_eq!(err.code, protocol::INVALID_PARAMS);
            assert_eq!(
                err.data.as_ref().and_then(|data| data["code"].as_str()),
                Some("F0002")
            );
        };
        assert_invalid(entry_state(app.world_mut(), &json!({})).expect_err("missing track"));
        let stuck = json!({
            "track": [
                { "time": 1.0, "position": [0.0, 0.0, 0.0] },
                { "time": 1.0, "position": [1.0, 0.0, 0.0] }
            ]
        });
        assert_invalid(entry_state(app.world_mut(), &stuck).expect_err("not increasing"));
        let tier = json!({
            "track": [
                { "time": 0.0, "position": [0.0, 0.0, 0.0] },
                { "time": 1.0, "position": [1.0, 0.0, 0.0] }
            ],
            "tier": "ultra"
        });
        assert_invalid(entry_state(app.world_mut(), &tier).expect_err("unknown tier"));
    }
}
