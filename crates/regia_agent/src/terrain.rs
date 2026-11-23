// SPDX-License-Identifier: MPL-2.0
//! Terrain on the agent protocol (`specs/26` § 1, `book/agents/terrain-scatter.md`,
//! `book/agents/terrain-editing.md`): `terrain.scatter` reads the [`ScatterControl`] report,
//! `terrain.set_scatter` toggles the field, the contact grounding, or one class (the host
//! applies the control to its [`regia_terrain::ScatterField`] each frame,
//! [`ScatterControl::apply`]). `terrain.stroke` sculpts or paints with a brush stroke.
//!
//! Terrain holes (ADR-088): `terrain.holes` reads the [`TerrainHoles`] handle of the host's
//! terrain draw, `terrain.set_holes` asks it to cut a new mask on its next frame,
//! `terrain.load_holes` cuts a `<map>.holes.ron` document, and `terrain.raycast` casts a ray
//! that passes through the holes (ADR-247).
//!
//! Terrain virtual texture (ADR-117): `terrain.vt` reads the [`TerrainVtHandle`] (settings,
//! counters), `terrain.set_vt` switches it on, off or to other settings on the next frame.
//!
//! Shoreline water (ADR-1237): `terrain.water` reads the first terrain's [`WaterPlane`] and
//! probes points of it ([`WaterPlane::probe`]: depth, wet, distance to the shore, foam,
//! opacity); `terrain.set_water` edits or removes it.

use regia_app::prelude::*;
use regia_core::math::Vec2;
use regia_framework::season::{FoliageKind, SeasonLook, SeasonState};
use regia_terrain::{
    Brush, BrushMode, BrushShape, BrushTarget, HoleShape, HolesMask, ScatterCasterHint, ScatterControl,
    ScatterSeason, Terrain, TerrainEdits, TerrainHoles, TerrainPaint, TerrainStrokeLog, TerrainVtHandle,
    VirtualTextureSettings, WaterPlane, apply_stroke, holes_ron_path, load_holes_ron,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no ScatterControl resource in this app",
        "add regia_terrain::TerrainPlugin (it inserts ScatterControl) and record the field each frame",
    )
}

fn report(control: &ScatterControl) -> Result<Value, RpcError> {
    let report = serde_json::to_value(&control.report)
        .map_err(|e| RpcError::internal(format!("scatter report serialization failed: {e}")))?;
    let overrides: Vec<Value> = control
        .classes
        .iter()
        .map(|(name, enabled)| json!({ "name": name, "enabled": enabled }))
        .collect();
    Ok(json!({
        "enabled": control.enabled,
        "contact": control.contact,
        "impostors": control.impostors,
        "detail": control.detail,
        "shadow_lod": control.shadow_lod_hint.map(|h| json!({
            "max_lod": (h.max_lod != u8::MAX).then_some(h.max_lod),
            "min_lod": h.min_lod,
        })),
        "class_overrides": overrides,
        "foliage_subsurface": control
            .foliage_subsurface
            .iter()
            .map(|(name, on)| json!({ "name": name, "on": on }))
            .collect::<Vec<Value>>(),
        "class_casters": control
            .class_casters
            .iter()
            .map(|(name, hint)| json!({ "name": name, "caster": hint }))
            .collect::<Vec<Value>>(),
        "class_seasons": control
            .class_seasons
            .iter()
            .map(|(name, season)| json!({ "name": name, "season": season }))
            .collect::<Vec<Value>>(),
        "report": report,
    }))
}

/// `terrain.scatter`: the requested state and the last frame's report (instances resident and
/// emitted, tiles, per-LOD counts, per-class residency).
pub fn scatter(world: &mut World) -> Result<Value, RpcError> {
    let control = world.get_resource::<ScatterControl>().ok_or_else(missing)?;
    report(control)
}

/// `shadow_lod`: `null` clears the hint; `{ max_lod?, min_lod? }` sets one (a missing or null
/// `max_lod` is no cap, a missing `min_lod` no floor).
fn parse_shadow_lod(value: &Value) -> Result<Option<regia_render::ShadowLodHint>, RpcError> {
    if value.is_null() {
        return Ok(None);
    }
    let object = value.as_object().ok_or_else(|| {
        RpcError::invalid_params("`shadow_lod` must be null or { max_lod?: integer|null, min_lod?: integer }")
    })?;
    let lod = |name: &str, default: u8| -> Result<u8, RpcError> {
        match object.get(name) {
            None | Some(Value::Null) => Ok(default),
            Some(v) => v
                .as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .filter(|n| *n < u8::MAX)
                .ok_or_else(|| {
                    RpcError::invalid_params(format!("`shadow_lod.{name}` must be an integer 0..=254"))
                }),
        }
    };
    Ok(Some(regia_render::ShadowLodHint {
        max_lod: lod("max_lod", u8::MAX)?,
        min_lod: lod("min_lod", 0)?,
    }))
}

/// `class_caster`: `null` clears the class's hint; `{ from_lod?, max_lod?, min_lod?, leaf_keep_one_in? }`
/// sets one (a missing `from_lod` is 0, a missing or null `max_lod` no cap, a missing `min_lod` no floor,
/// a missing `leaf_keep_one_in` keeps every card).
fn parse_class_caster(value: &Value) -> Result<Option<ScatterCasterHint>, RpcError> {
    if value.is_null() {
        return Ok(None);
    }
    let object = value.as_object().ok_or_else(|| {
        RpcError::invalid_params(
            "`class_caster` must be null or { from_lod?, max_lod?, min_lod?, leaf_keep_one_in? }",
        )
    })?;
    let lod = |name: &str, default: u8, max: u8| -> Result<u8, RpcError> {
        match object.get(name) {
            None | Some(Value::Null) => Ok(default),
            Some(v) => v
                .as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .filter(|n| *n <= max)
                .ok_or_else(|| {
                    RpcError::invalid_params(format!("`class_caster.{name}` must be an integer 0..={max}"))
                }),
        }
    };
    Ok(Some(ScatterCasterHint {
        from_lod: lod("from_lod", 0, u8::MAX - 1)?,
        max_lod: lod("max_lod", u8::MAX, u8::MAX - 1)?,
        min_lod: lod("min_lod", 0, u8::MAX - 1)?,
        leaf_keep_one_in: lod("leaf_keep_one_in", 1, ScatterCasterHint::MAX_KEEP_ONE_IN)?,
    }))
}

/// `class_season`: `null` clears the class's season; `{ density, tint? }` sets one directly (`tint` is a
/// linear `[r, g, b]`, default no tint); `{ kind, year_fraction? }` takes the look of `kind` (`broadleaf`,
/// `evergreen` or `grass`) at that time of year, by default the scene's (`scene.season`).
fn parse_class_season(world: &World, value: &Value) -> Result<Option<ScatterSeason>, RpcError> {
    if value.is_null() {
        return Ok(None);
    }
    const SHAPE: &str = "`class_season` must be null, { density, tint? } or { kind, year_fraction? }";
    let object = value.as_object().ok_or_else(|| RpcError::invalid_params(SHAPE))?;
    let number = |name: &str| -> Result<Option<f32>, RpcError> {
        object
            .get(name)
            .map(|v| {
                v.as_f64()
                    .map(|n| n as f32)
                    .filter(|n| n.is_finite())
                    .ok_or_else(|| {
                        RpcError::invalid_params(format!("`class_season.{name}` must be a number"))
                    })
            })
            .transpose()
    };
    if let Some(kind) = object.get("kind") {
        if object.contains_key("density") || object.contains_key("tint") {
            return Err(RpcError::invalid_params(SHAPE));
        }
        let kind = kind.as_str().and_then(FoliageKind::from_name).ok_or_else(|| {
            RpcError::invalid_params("`class_season.kind` must be broadleaf, evergreen or grass")
        })?;
        let year = match number("year_fraction")? {
            Some(year) => year,
            None => world
                .get_resource::<SeasonState>()
                .map_or(SeasonState::default().year_fraction(), SeasonState::year_fraction),
        };
        let look = SeasonLook::at(kind, year);
        return Ok(Some(ScatterSeason::new(look.tint, look.density)));
    }
    if object.contains_key("year_fraction") {
        return Err(RpcError::invalid_params(SHAPE));
    }
    let density = number("density")?.ok_or_else(|| RpcError::invalid_params(SHAPE))?;
    if !(0.0..=1.0).contains(&density) {
        return Err(RpcError::invalid_params(
            "`class_season.density` must be in 0..=1",
        ));
    }
    let tint = match object.get("tint") {
        None => [1.0; 3],
        Some(v) => {
            let channel = |c: &Value| {
                c.as_f64()
                    .map(|c| c as f32)
                    .filter(|c| (0.0..=ScatterSeason::MAX_TINT).contains(c))
            };
            let channels: Option<Vec<f32>> = v
                .as_array()
                .filter(|a| a.len() == 3)
                .and_then(|a| a.iter().map(channel).collect());
            match channels.as_deref() {
                Some(&[r, g, b]) => [r, g, b],
                _ => {
                    return Err(RpcError::invalid_params(format!(
                        "`class_season.tint` must be [r, g, b], each in 0..={}",
                        ScatterSeason::MAX_TINT
                    )));
                }
            }
        }
    };
    Ok(Some(ScatterSeason::new(tint, density)))
}

/// `terrain.set_scatter`: `enabled` (whole field), `contact` (grounding pool), `impostors`
/// (the octahedral impostor far field, where the host bound a layer), `detail` (the dense-scatter
/// detail limits: screen-size fade and small casters), `shadow_lod` (a field-wide shadow LOD
/// hint, `null` clears it), and/or `class` + `class_enabled` (one class) and/or `class` +
/// `class_subsurface` (`foliage` or `off`: the foliage subsurface profile on the class's
/// materials, ADR-3170) and/or `class` + `class_caster` (the class's shadow casters past a drawn LOD,
/// ADR-3146) and/or `class` + `class_season` (the class's season, ADR-3146). Returns the state before
/// and after.
pub fn set_scatter(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = params.get("enabled").map(|v| {
        v.as_bool()
            .ok_or_else(|| RpcError::invalid_params("`enabled` must be a boolean"))
    });
    let contact = params.get("contact").map(|v| {
        v.as_bool()
            .ok_or_else(|| RpcError::invalid_params("`contact` must be a boolean"))
    });
    let impostors = params.get("impostors").map(|v| {
        v.as_bool()
            .ok_or_else(|| RpcError::invalid_params("`impostors` must be a boolean"))
    });
    let detail = params.get("detail").map(|v| {
        v.as_bool()
            .ok_or_else(|| RpcError::invalid_params("`detail` must be a boolean"))
    });
    let class = params.get("class").map(|v| {
        v.as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| RpcError::invalid_params("`class` must be a class name from the .scatter.ron"))
    });
    let class_enabled = params.get("class_enabled").map(|v| {
        v.as_bool()
            .ok_or_else(|| RpcError::invalid_params("`class_enabled` must be a boolean"))
    });
    let class_subsurface = params.get("class_subsurface").map(|v| match v.as_str() {
        Some("foliage") => Ok(true),
        Some("off") => Ok(false),
        _ => Err(RpcError::invalid_params(
            "`class_subsurface` must be foliage or off",
        )),
    });
    let shadow_lod = params.get("shadow_lod").map(parse_shadow_lod);
    let class_caster = params.get("class_caster").map(parse_class_caster);
    let class_season = params.get("class_season").map(|v| parse_class_season(world, v));
    // Every parameter is checked before the first change, so a refused call changes nothing.
    let enabled = enabled.transpose()?;
    let contact = contact.transpose()?;
    let impostors = impostors.transpose()?;
    let detail = detail.transpose()?;
    let class = class.transpose()?;
    let class_enabled = class_enabled.transpose()?;
    let class_subsurface = class_subsurface.transpose()?;
    let shadow_lod = shadow_lod.transpose()?;
    let class_caster = class_caster.transpose()?;
    let class_season = class_season.transpose()?;
    if class_subsurface.is_some() && class.is_none() {
        return Err(RpcError::invalid_params("`class_subsurface` needs `class`"));
    }
    if (class_caster.is_some() || class_season.is_some()) && class.is_none() {
        return Err(RpcError::invalid_params(
            "`class_caster` and `class_season` need `class`",
        ));
    }
    if enabled.is_none()
        && contact.is_none()
        && impostors.is_none()
        && detail.is_none()
        && shadow_lod.is_none()
        && class.is_none()
    {
        return Err(RpcError::invalid_params(
            "pass at least one of `enabled`, `contact`, `impostors`, `detail`, `shadow_lod`, or `class` + `class_enabled`",
        ));
    }
    let mut control = world.get_resource_mut::<ScatterControl>().ok_or_else(missing)?;
    let before = report(&control)?;
    if let Some(name) = class {
        if class_enabled.is_none()
            && class_subsurface.is_none()
            && class_caster.is_none()
            && class_season.is_none()
        {
            return Err(RpcError::invalid_params(
                "`class` needs `class_enabled`, `class_subsurface`, `class_caster` or `class_season`",
            ));
        }
        let known = control.report.classes.iter().any(|c| c.name == name);
        if control.report.installed && !known {
            let names: Vec<&str> = control.report.classes.iter().map(|c| c.name.as_str()).collect();
            return Err(RpcError::not_found(
                format!("no scatter class `{name}`"),
                format!("classes: {}", names.join(", ")),
            ));
        }
        if let Some(on) = class_enabled {
            control.set_class(name, on);
        }
        if let Some(on) = class_subsurface {
            control.set_class_foliage_subsurface(name, on);
        }
        if let Some(hint) = class_caster {
            control.set_class_caster(name, hint);
        }
        if let Some(season) = class_season {
            control.set_class_season(name, season);
        }
    }
    if let Some(on) = enabled {
        control.enabled = on;
    }
    if let Some(on) = contact {
        control.contact = on;
    }
    if let Some(on) = impostors {
        control.impostors = on;
    }
    if let Some(on) = detail {
        control.detail = on;
    }
    if let Some(hint) = shadow_lod {
        control.shadow_lod_hint = hint;
    }
    let after = report(&control)?;
    Ok(json!({ "before": before, "after": after }))
}

fn xz(value: &Value, name: &str) -> Result<Vec2, RpcError> {
    let arr = value
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be [x, z] in meters")))?;
    let x = arr[0]
        .as_f64()
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}`[0] must be a number")))?;
    let z = arr[1]
        .as_f64()
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}`[1] must be a number")))?;
    Ok(Vec2::new(x as f32, z as f32))
}

fn number(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    params
        .get(name)
        .map(|v| {
            v.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a number")))
        })
        .transpose()
}

/// The brush a `terrain.stroke` call describes.
fn stroke_brush(params: &Value) -> Result<Brush, RpcError> {
    let mode = params.get("mode").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params(
            "`mode` is required: raise | lower | flatten | assign | smooth | noise | ramp | paint",
        )
    })?;
    let strength = number(params, "strength")?.unwrap_or(1.0);
    let radius = number(params, "radius")?;
    let shape = match crate::params::opt_str(params, "shape")?.unwrap_or("circle") {
        "circle" => BrushShape::Circle {
            radius: radius
                .ok_or_else(|| RpcError::invalid_params("`radius` (meters) is required for a circle"))?,
        },
        "rectangle" | "rect" => BrushShape::Rectangle {
            width: number(params, "width")?
                .or(radius.map(|r| r * 2.0))
                .ok_or_else(|| {
                    RpcError::invalid_params("a rectangle needs `width` and `length` (or `radius`)")
                })?,
            length: number(params, "length")?
                .or(radius.map(|r| r * 2.0))
                .ok_or_else(|| {
                    RpcError::invalid_params("a rectangle needs `width` and `length` (or `radius`)")
                })?,
        },
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown shape `{other}`: circle | rectangle"
            )));
        }
    };
    let opacity = |s: f32| s.clamp(0.0, 1.0);
    let mut brush = Brush {
        shape,
        hardness: number(params, "hardness")?.unwrap_or(0.0),
        falloff: match crate::params::opt_str(params, "falloff")?.unwrap_or("smooth") {
            "smooth" => regia_terrain::brush::BrushFalloff::Smooth,
            "linear" => regia_terrain::brush::BrushFalloff::Linear,
            "sphere" => regia_terrain::brush::BrushFalloff::Sphere,
            "tip" => regia_terrain::brush::BrushFalloff::Tip,
            _ => {
                return Err(RpcError::invalid_params(
                    "falloff must be smooth, linear, sphere or tip",
                ));
            }
        },
        ..Brush::default()
    };
    match mode {
        "raise" => brush.mode = BrushMode::Raise { amount: strength },
        "lower" => brush.mode = BrushMode::Lower { amount: strength },
        "flatten" => {
            brush.mode = BrushMode::Flatten;
            brush.alpha = opacity(strength);
        }
        "assign" => {
            let value = number(params, "value")?.ok_or_else(|| {
                RpcError::invalid_params("assign needs `value` (meters, or 0..1 when painting)")
            })?;
            brush.mode = BrushMode::Assign { value };
            brush.alpha = opacity(strength);
        }
        "smooth" => {
            let kernel = crate::params::opt_u64(params, "kernel_radius")?.unwrap_or(2);
            brush.mode = BrushMode::Smooth {
                kernel_radius: kernel.min(64) as u32,
            };
            brush.alpha = opacity(strength);
        }
        "paint" => {
            brush.mode = BrushMode::Assign { value: 1.0 };
            brush.alpha = opacity(strength);
        }
        // Noise and Ramp (ADR-1853): the wavelength (metres) and the ramp's ends need the map,
        // so `stroke` finishes them with `map_dependent_mode`.
        "noise" => {
            let seed = crate::params::opt_u64(params, "seed")?.unwrap_or(0);
            brush.mode = BrushMode::Noise {
                amount: strength,
                wavelength: NOISE_DEFAULT_WAVELENGTH,
                seed: seed as u32,
            };
        }
        "ramp" => {
            brush.mode = BrushMode::Ramp {
                start: [0.0; 2],
                end: [0.0; 2],
                start_value: 0.0,
                end_value: 0.0,
            };
            brush.alpha = opacity(strength);
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown mode `{other}`: raise | lower | flatten | assign | smooth | noise | ramp | paint"
            )));
        }
    }
    if let Some(layer) = params.get("layer") {
        let layer = layer
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`layer` must be a layer index"))?;
        brush.target = BrushTarget::LayerWeight {
            layer: layer as usize,
        };
    } else if mode == "paint" {
        return Err(RpcError::invalid_params(
            "paint needs `layer` (the material layer index)",
        ));
    }
    Ok(brush)
}

/// Noise wavelength in samples when `wavelength` (metres) is not given.
const NOISE_DEFAULT_WAVELENGTH: f32 = 8.0;

/// Finishes the modes that depend on the map: a noise `wavelength` in metres becomes samples,
/// and a ramp runs from the first path point to the last, at `from_height` / `to_height` or the
/// map's heights there.
fn map_dependent_mode(
    mode: BrushMode,
    map: &regia_terrain::TerrainMap,
    path: &[Vec2],
    params: &Value,
) -> Result<BrushMode, RpcError> {
    match mode {
        BrushMode::Noise { amount, seed, .. } => {
            let wavelength = match number(params, "wavelength")? {
                Some(m) if m.is_finite() && m > 0.0 => m / map.sample_spacing().max(f32::EPSILON),
                Some(m) => {
                    return Err(RpcError::invalid_params(format!(
                        "`wavelength` must be a positive length in meters, not {m}"
                    )));
                }
                None => NOISE_DEFAULT_WAVELENGTH,
            };
            Ok(BrushMode::Noise {
                amount,
                wavelength: wavelength.max(0.5),
                seed,
            })
        }
        BrushMode::Ramp { .. } => {
            let (Some(&from), Some(&to)) = (path.first(), path.last()) else {
                return Err(RpcError::invalid_params(
                    "ramp needs `center` and `to` (or `path`)",
                ));
            };
            if path.len() < 2 {
                return Err(RpcError::invalid_params(
                    "ramp needs an end point: pass `to` (or `path`)",
                ));
            }
            Ok(regia_terrain::ramp_mode(
                map,
                from,
                to,
                number(params, "from_height")?,
                number(params, "to_height")?,
            ))
        }
        other => Ok(other),
    }
}

/// `terrain.stroke`: one brush stroke on a terrain (`entity`, or the first [`Terrain`]). A stamp at
/// `center`, smeared through `to` / `path` when given. Heights change in place; a linked draw gets
/// the dirty rectangle from [`TerrainEdits`] and the editor turns the logged [`TerrainStrokeLog`]
/// entry into one undo step.
pub fn stroke(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let brush = stroke_brush(params)?;
    let center = xz(
        params
            .get("center")
            .ok_or_else(|| RpcError::invalid_params("`center` [x, z] is required"))?,
        "center",
    )?;
    let mut path = vec![center];
    if let Some(to) = params.get("to") {
        path.push(xz(to, "to")?);
    }
    if let Some(points) = params.get("path") {
        let points = points
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("`path` must be a list of [x, z]"))?;
        for p in points {
            path.push(xz(p, "path")?);
        }
    }
    let entity = if let Some(bits) = crate::params::opt_u64(params, "entity")? {
        Entity::try_from_bits(bits)
            .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?
    } else {
        let mut query = world.query_filtered::<Entity, With<Terrain>>();
        query.iter(world).min_by_key(|e| e.to_bits()).ok_or_else(|| {
            RpcError::not_found(
                "no Terrain in this world",
                "spawn an entity with regia_terrain::Terrain (the level's heightfield)",
            )
        })?
    };
    {
        let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
            return Err(RpcError::not_found(
                format!("entity {} does not exist", entity.to_bits()),
                "pass a Terrain entity",
            ));
        };
        if !entity_mut.contains::<Terrain>() {
            return Err(RpcError::not_found(
                format!("entity {} has no Terrain", entity.to_bits()),
                "omit `entity` to use the lowest-entity-id terrain",
            ));
        }
        if !entity_mut.contains::<TerrainEdits>() {
            entity_mut.insert(TerrainEdits::default());
        }
    }
    // Non-destructive edit layers (ADR-7825): `edit_layer` picks the layer a height stroke writes
    // (an id, a name or "base"); without it the stroke writes the terrain's active layer.
    let edit_layer = crate::terrain_edit_layers::layer_ref(world, entity, params, "edit_layer")?;
    match (
        world.get::<regia_terrain::TerrainEditLayers>(entity),
        brush.target,
    ) {
        (Some(stack), BrushTarget::Height) => {
            stack
                .check_target(edit_layer.unwrap_or(stack.active()))
                .map_err(|e| crate::terrain_edit_layers::layer_error(&e))?;
        }
        (_, BrushTarget::LayerWeight { .. }) if edit_layer.is_some() => {
            return Err(RpcError::invalid_params(
                "`edit_layer` applies to height strokes, not to painted weights (`layer`)",
            ));
        }
        _ => {}
    }
    let mut query = world.query::<(&mut Terrain, Option<&mut TerrainPaint>, &mut TerrainEdits)>();
    let Ok((mut terrain, paint, mut edits)) = query.get_mut(world, entity) else {
        return Err(RpcError::internal("terrain components vanished mid-call"));
    };
    let mut brush = brush;
    brush.mode = map_dependent_mode(brush.mode, &terrain.map, &path, params)?;
    let probe = |t: &Terrain| t.map.sample_bilinear(center.x, center.y);
    let before = probe(&terrain);
    let undo = apply_stroke(
        &mut terrain,
        paint.map(Mut::into_inner),
        &mut edits,
        &brush,
        &path,
    )
    .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let dirty = undo.dirty();
    let cells: Vec<[u32; 2]> = undo.tiles().iter().map(|&(x, y)| [x, y]).collect();
    let samples = undo.len();
    let target = match brush.target {
        BrushTarget::Height => json!("height"),
        BrushTarget::LayerWeight { layer } => json!({ "layer": layer }),
    };
    let (undo, layer_undo) = regia_terrain::absorb_world_stroke(world, entity, edit_layer, undo)
        .map_err(|e| crate::terrain_edit_layers::layer_error(&e))?;
    let after = world.get::<Terrain>(entity).map_or(before, probe);
    let written_layer = layer_undo
        .as_ref()
        .map(|l| l.layer.map_or(json!("base"), |id| json!(id)));
    if !undo.is_empty() {
        // Under an editor every stroke becomes an undo step, however many arrive in one frame.
        match layer_undo {
            Some(layer) => world
                .get_resource_or_insert_with::<regia_terrain::TerrainEditLayerLog>(
                    regia_terrain::TerrainEditLayerLog::default,
                )
                .push(regia_terrain::EditLayerLogEntry::Stroke {
                    entity,
                    stroke: undo,
                    layer,
                }),
            None => world
                .get_resource_or_insert_with::<TerrainStrokeLog>(TerrainStrokeLog::default)
                .push(entity, undo),
        }
    }
    Ok(json!({
        "entity": entity.to_bits(),
        "mode": brush.mode.name(),
        "target": target,
        "samples": samples,
        "dirty": { "col_start": dirty.col_start, "col_end": dirty.col_end,
                   "row_start": dirty.row_start, "row_end": dirty.row_end },
        "cells": cells,
        "cell_size": regia_terrain::EDIT_CELL_SIZE,
        "height_before": before,
        "height_after": after,
        "edit_layer": written_layer,
    }))
}

fn holes_handle(world: &World) -> Result<TerrainHoles, RpcError> {
    let handle = world.get_resource::<TerrainHoles>().cloned().ok_or_else(|| {
        RpcError::not_found(
            "no TerrainHoles resource in this app",
            "add regia_terrain::TerrainPlugin and insert the terrain draw's holes_handle() as a resource",
        )
    })?;
    if !handle.state().linked {
        return Err(RpcError::not_found(
            "the TerrainHoles resource is not linked to a terrain draw",
            "insert TerrainSharedDraw::holes_handle() as the TerrainHoles resource",
        ));
    }
    Ok(handle)
}

fn holes_report(handle: &TerrainHoles) -> Result<Value, RpcError> {
    let state = handle.state();
    let shapes: Vec<HoleShape> = state.applied.all_shapes().collect();
    let shapes = serde_json::to_value(&shapes)
        .map_err(|e| RpcError::internal(format!("holes serialization failed: {e}")))?;
    Ok(json!({
        "shapes": shapes,
        "count": state.applied.all_shapes().count(),
        "generation": state.generation,
        "pending": state.requested.is_some(),
    }))
}

/// `terrain.holes`: the footprints cut through the terrain, and whether a request is pending.
pub fn holes(world: &mut World) -> Result<Value, RpcError> {
    holes_report(&holes_handle(world)?)
}

/// `terrain.set_holes`: `shapes` (hole footprints: `rect`, `polygon`, `ellipse`, `raster`)
/// and/or `segments` (`{ from: [x, z], to: [x, z], width_m }` ramp trenches) replace the
/// mask; `clear: true` with neither removes every hole. Applied on the next frame.
pub fn set_holes(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut shapes: Vec<HoleShape> = match params.get("shapes") {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            RpcError::invalid_params(format!(
                "`shapes` must be an array of {{rect|polygon|ellipse|raster: {{...}}}}: {e}"
            ))
        })?,
        None => Vec::new(),
    };
    if let Some(segments) = params.get("segments") {
        let segments = segments
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("`segments` must be an array"))?;
        for seg in segments {
            let point = |key: &str| -> Result<[f32; 2], RpcError> {
                serde_json::from_value(seg.get(key).cloned().unwrap_or(Value::Null))
                    .map_err(|_| RpcError::invalid_params(format!("segment `{key}` must be [x, z]")))
            };
            let width = seg
                .get("width_m")
                .and_then(Value::as_f64)
                .ok_or_else(|| RpcError::invalid_params("segment `width_m` must be a number"))?;
            shapes.push(HoleShape::segment(point("from")?, point("to")?, width as f32));
        }
    }
    let clear = crate::params::opt_bool(params, "clear")?.unwrap_or(false);
    if shapes.is_empty() && !clear {
        return Err(RpcError::invalid_params(
            "pass `shapes` and/or `segments`, or `clear: true` to remove every hole",
        ));
    }
    let mask = HolesMask::from_shapes(shapes).map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let handle = holes_handle(world)?;
    let before = holes_report(&handle)?;
    handle.request(mask);
    Ok(json!({ "before": before, "requested": holes_report(&handle)?["pending"] }))
}

/// True when `path` already names a holes document (`.holes.ron`, any ASCII case).
pub(crate) fn has_holes_suffix(path: &str) -> bool {
    const SUFFIX: &str = ".holes.ron";
    path.len() >= SUFFIX.len()
        && path
            .as_bytes()
            .get(path.len() - SUFFIX.len()..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(SUFFIX.as_bytes()))
}

/// `terrain.load_holes`: cuts the holes of a `<map>.holes.ron` document (`ron`: the text, or
/// `path`: a file under the project root, or a map file whose `<map>.holes.ron` sits next to it)
/// through the terrain draw on the next frame. The document is validated first (at most 64
/// shapes; a newer format version is refused).
pub fn load_holes(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (mask, source) = if let Some(text) = crate::params::opt_str(params, "ron")? {
        let mask = HolesMask::from_holes_ron(text).map_err(|e| RpcError::invalid_params(e.to_string()))?;
        (mask, "ron".to_string())
    } else if let Some(path) = crate::params::opt_str(params, "path")? {
        let relative = std::path::Path::new(path);
        // The shared rules: no root, no `..` in any spelling, no Windows prefix or device.
        if regia_core::project_path::check_relative(path).is_err() {
            return Err(RpcError::invalid_params(
                "`path` must be relative to the project root, without `..`",
            ));
        }
        let root = world
            .get_resource::<regia_asset::AssetServer>()
            .map(|s| s.project_root().to_path_buf())
            .ok_or_else(|| {
                RpcError::not_found(
                    "no AssetServer resource: no project root to read from",
                    "pass the document text as `ron` instead",
                )
            })?;
        let file = if has_holes_suffix(path) {
            root.join(relative)
        } else {
            holes_ron_path(&root.join(relative))
        };
        // A symlink under the root may point outside it: the resolved file must stay inside.
        // A file that does not resolve is not opened by its unresolved name later (a symlink
        // created in between would be followed; audit-agent-sec Low).
        let file = match (file.canonicalize(), root.canonicalize()) {
            (Ok(resolved), Ok(root)) if !resolved.starts_with(&root) => {
                return Err(RpcError::invalid_params(format!(
                    "{path} resolves outside the project root"
                )));
            }
            (Ok(resolved), Ok(_)) => Some(resolved),
            // Missing: reported below as not found.
            (Err(e), _) if e.kind() == std::io::ErrorKind::NotFound => None,
            (Err(e), _) | (Ok(_), Err(e)) => {
                return Err(RpcError::invalid_params(format!("cannot read {path}: {e}")));
            }
        };
        let loaded = match &file {
            Some(file) => load_holes_ron(file).map_err(|e| {
                RpcError::invalid_params(e.to_string().replace(&root.display().to_string(), "."))
            })?,
            None => None,
        };
        let mask = loaded.ok_or_else(|| {
            RpcError::not_found(
                format!("{path} does not exist"),
                "write the map's <map>.holes.ron next to it, or pass the text as `ron`",
            )
        })?;
        (mask, path.to_string())
    } else {
        return Err(RpcError::invalid_params(
            "pass `ron` (a <map>.holes.ron document) or `path` (a file under the project root)",
        ));
    };
    let handle = holes_handle(world)?;
    let count = mask.all_shapes().count();
    handle.request(mask);
    Ok(json!({ "source": source, "count": count, "requested": holes_report(&handle)?["pending"] }))
}

/// `terrain.raycast`: the first terrain hit along a ray, passing through holes as the drawn
/// terrain does ([`regia_terrain::pick_terrain`]): `origin`, `direction` ([x, y, z]) and
/// `max_distance` (metres, default 10 000). `hit: null` when the ray misses every terrain.
pub fn raycast(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let origin = crate::physics::vec3_param(params, "origin")?;
    let direction = crate::physics::vec3_param(params, "direction")?;
    // `try_normalize` is what the pick uses: a zero direction, or one so long its length
    // overflows (`[1e30, 0, 0]`), is an error here, not a silent miss.
    if direction.try_normalize().is_none() {
        return Err(RpcError::invalid_params(
            "`direction` must be a non-zero vector of finite length",
        ));
    }
    let max_distance = crate::physics::distance_param(params, "max_distance", 10_000.0)?;
    let terrains = world.query::<&Terrain>().iter(world).count();
    if terrains == 0 {
        return Err(RpcError::not_found(
            "no Terrain in the world",
            "spawn an entity with regia_terrain::Terrain (the level's heightfield)",
        ));
    }
    let hit = regia_terrain::pick_terrain(world, origin, direction, max_distance).map(|hit| {
        let holes = world
            .get::<Terrain>(hit.entity)
            .and_then(|t| regia_terrain::terrain_holes(world, t))
            .map_or(0, |m| m.shape_count());
        json!({
            "entity": hit.entity.to_bits(),
            "position": hit.position.to_array(),
            "distance": hit.distance,
            "holes": holes,
        })
    });
    Ok(json!({ "hit": hit, "terrains": terrains }))
}

fn vt_handle(world: &World) -> Result<TerrainVtHandle, RpcError> {
    let handle = world.get_resource::<TerrainVtHandle>().cloned().ok_or_else(|| {
        RpcError::not_found(
            "no TerrainVtHandle resource in this app",
            "add regia_terrain::TerrainPlugin and insert the terrain draw's vt_handle() as a resource",
        )
    })?;
    if !handle.is_linked() {
        return Err(RpcError::not_found(
            "the TerrainVtHandle resource is not linked to a terrain draw",
            "insert TerrainSharedDraw::vt_handle() as the TerrainVtHandle resource",
        ));
    }
    Ok(handle)
}

fn vt_report(handle: &TerrainVtHandle) -> Result<Value, RpcError> {
    let to_json = |v: Result<Value, serde_json::Error>| {
        v.map_err(|e| RpcError::internal(format!("virtual texture report serialization failed: {e}")))
    };
    let settings = handle.settings();
    Ok(json!({
        "supported": handle.is_supported(),
        "depth_test": handle.depth_test().name(),
        "enabled": settings.is_some(),
        "settings": to_json(serde_json::to_value(settings))?,
        "stats": to_json(serde_json::to_value(handle.stats()))?,
        "resident_pages": handle.resident_pages().len(),
        "pending": handle.is_pending(),
        "error": handle.error(),
    }))
}

/// `terrain.vt`: whether the terrain virtual texture is on, its settings and last frame's
/// counters (resident pages, bakes, re-bakes, missing pages, how long the oldest request has
/// waited, hit rate, bake and reduction GPU ms), and a pending request or refusal.
pub fn vt(world: &mut World) -> Result<Value, RpcError> {
    vt_report(&vt_handle(world)?)
}

/// `terrain.set_vt`: `enabled` (false turns it off) and/or `settings` (any subset of
/// [`VirtualTextureSettings`], over the defaults or the settings in effect; `cache`:
/// `lighting`, `relief` or `surface`). Applied on the next frame; read `terrain.vt` for
/// `pending` and `error`.
pub fn set_vt(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = match params.get("enabled") {
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`enabled` must be a boolean"))?,
        ),
        None => None,
    };
    let patch = params.get("settings");
    if enabled.is_none() && patch.is_none() {
        return Err(RpcError::invalid_params("pass `enabled` and/or `settings`"));
    }
    let handle = vt_handle(world)?;
    let before = vt_report(&handle)?;
    let request = if enabled == Some(false) {
        None
    } else {
        let base = handle.settings().unwrap_or_default();
        let mut merged = serde_json::to_value(base)
            .map_err(|e| RpcError::internal(format!("settings serialization failed: {e}")))?;
        if let (Some(Value::Object(patch)), Value::Object(target)) = (patch, &mut merged) {
            for (k, v) in patch {
                if !target.contains_key(k) {
                    return Err(RpcError::invalid_params(format!(
                        "unknown setting `{k}` (known: {})",
                        target.keys().cloned().collect::<Vec<_>>().join(", ")
                    )));
                }
                target.insert(k.clone(), v.clone());
            }
        } else if patch.is_some() {
            return Err(RpcError::invalid_params("`settings` must be an object"));
        }
        let settings: VirtualTextureSettings = serde_json::from_value(merged)
            .map_err(|e| RpcError::invalid_params(format!("invalid settings: {e}")))?;
        settings
            .validate()
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        Some(settings)
    };
    handle.request(request);
    Ok(json!({ "before": before, "pending": true }))
}

/// Points one `terrain.water` call probes at most.
const WATER_PROBES_MAX: usize = 64;

fn no_terrain() -> RpcError {
    RpcError::not_found(
        "no Terrain in the world",
        "spawn an entity with regia_terrain::Terrain (the level's heightfield)",
    )
}

/// The first terrain entity in query order (the one the water calls read and edit) and how
/// many terrains the world has.
fn first_terrain(world: &mut World) -> Result<(Entity, usize), RpcError> {
    let mut query = world.query::<(Entity, &Terrain)>();
    let count = query.iter(world).count();
    let entity = query
        .iter(world)
        .next()
        .map(|(entity, _)| entity)
        .ok_or_else(no_terrain)?;
    Ok((entity, count))
}

fn water_json(water: Option<&WaterPlane>) -> Result<Value, RpcError> {
    serde_json::to_value(water).map_err(|e| RpcError::internal(format!("water serialization failed: {e}")))
}

/// `terrain.water`: the first terrain's shoreline water plane (`null` when it has none) and,
/// for each `at` point ([x, z], at most 64), what the water is there at `time` seconds
/// (default 0): surface and ground height, depth, whether the plane is drawn there, bed
/// slope, distance to the shoreline, the foam band and the opacity seen from straight above.
pub fn water(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let time = number(params, "time")?.unwrap_or(0.0);
    if !time.is_finite() {
        return Err(RpcError::invalid_params(
            "`time` must be a finite number of seconds",
        ));
    }
    let points = match params.get("at") {
        None => Vec::new(),
        Some(Value::Array(items)) => {
            if items.len() > WATER_PROBES_MAX {
                return Err(RpcError::invalid_params(format!(
                    "`at` holds {} points; at most {WATER_PROBES_MAX} per call",
                    items.len()
                )));
            }
            let points = items
                .iter()
                .map(|item| xz(item, "at[]"))
                .collect::<Result<Vec<_>, _>>()?;
            if points.iter().any(|p| !p.is_finite()) {
                return Err(RpcError::invalid_params("`at` points must be finite"));
            }
            points
        }
        Some(_) => return Err(RpcError::invalid_params("`at` must be an array of [x, z] points")),
    };
    let (entity, terrains) = first_terrain(world)?;
    let terrain = world.get::<Terrain>(entity).ok_or_else(no_terrain)?;
    let probes: Vec<Value> = match &terrain.water {
        Some(plane) => points
            .iter()
            .map(|p| {
                let probe = plane.probe(&terrain.map, p.x, p.y, time);
                json!({
                    "at": [p.x, p.y],
                    "surface_height": probe.surface_height,
                    "ground_height": probe.ground_height,
                    "depth": probe.depth,
                    "wet": probe.wet,
                    "slope": probe.slope,
                    "shore_distance": probe.shore_distance,
                    "foam": probe.foam,
                    "opacity_from_above": probe.opacity_from_above,
                })
            })
            .collect(),
        None => Vec::new(),
    };
    Ok(json!({
        "entity": entity.to_bits(),
        "terrains": terrains,
        "water": water_json(terrain.water.as_ref())?,
        "time": time,
        "probes": probes,
    }))
}

/// `terrain.set_water`: `water` (any subset of [`WaterPlane`]'s fields, over the plane in
/// effect or the defaults) and/or `enabled` (false removes the plane) on the first terrain.
/// The result is validated before it replaces the old plane. Returns both.
pub fn set_water(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = match params.get("enabled") {
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`enabled` must be a boolean"))?,
        ),
        None => None,
    };
    let patch = params.get("water");
    if enabled.is_none() && patch.is_none() {
        return Err(RpcError::invalid_params("pass `water` and/or `enabled`"));
    }
    if enabled == Some(false) && patch.is_some() {
        return Err(RpcError::invalid_params(
            "`enabled: false` removes the water; pass `water` without it to edit the plane",
        ));
    }
    let (entity, _) = first_terrain(world)?;
    let before = world.get::<Terrain>(entity).ok_or_else(no_terrain)?.water.clone();
    let after = if enabled == Some(false) {
        None
    } else {
        let base = before.clone().unwrap_or_default();
        let mut merged = serde_json::to_value(&base)
            .map_err(|e| RpcError::internal(format!("water serialization failed: {e}")))?;
        if let Some(patch) = patch {
            let (Value::Object(patch), Value::Object(target)) = (patch, &mut merged) else {
                return Err(RpcError::invalid_params(
                    "`water` must be an object of water plane fields",
                ));
            };
            for (k, v) in patch {
                if !target.contains_key(k) {
                    return Err(RpcError::invalid_params(format!(
                        "unknown water field `{k}` (known: {})",
                        target.keys().cloned().collect::<Vec<_>>().join(", ")
                    )));
                }
                target.insert(k.clone(), v.clone());
            }
        }
        let plane: WaterPlane = serde_json::from_value(merged)
            .map_err(|e| RpcError::invalid_params(format!("invalid water: {e}")))?;
        plane
            .validate()
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        Some(plane)
    };
    let result = json!({
        "entity": entity.to_bits(),
        "before": water_json(before.as_ref())?,
        "after": water_json(after.as_ref())?,
    });
    let mut terrain = world.get_mut::<Terrain>(entity).ok_or_else(no_terrain)?;
    terrain.water = after;
    Ok(result)
}

#[cfg(test)]
mod tests {
    #[test]
    fn holes_suffix_ignores_case() {
        assert!(super::has_holes_suffix("maps/Level.Holes.RON"));
        assert!(super::has_holes_suffix("a.holes.ron"));
        assert!(!super::has_holes_suffix("a.ron"));
        assert!(!super::has_holes_suffix("é"));
    }

    use super::*;

    /// A 128 m terrain whose ground rises along +X through y = 0 at x = 0 (1:20).
    fn ramp_terrain() -> Terrain {
        let n = 129u32;
        let heights = (0..n * n).map(|i| ((i % n) as f32 - 64.0) * 0.05).collect();
        let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 128.0, 10.0, -5.0).expect("map");
        Terrain::new(
            &map.to_heightfield(),
            128.0,
            10.0,
            -5.0,
            regia_terrain::CdlodConfig::default(),
            3,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain")
    }

    #[test]
    fn water_reads_edits_and_probes_the_plane() {
        let mut world = World::new();
        assert!(water(&mut world, &json!({})).is_err(), "no terrain");
        assert!(
            set_water(&mut world, &json!({ "enabled": true })).is_err(),
            "no terrain"
        );
        world.spawn(ramp_terrain());
        let out = water(&mut world, &json!({ "at": [[-10.0, 0.0]] })).expect("water");
        assert!(out["water"].is_null() && out["probes"] == json!([]), "{out}");
        assert_eq!(out["terrains"], json!(1));

        // A plane at y = 0: the ground is under water for x < 0.
        let set = set_water(&mut world, &json!({ "water": { "level": 0.0 } })).expect("set");
        assert!(set["before"].is_null(), "{set}");
        assert_eq!(set["after"]["level"], json!(0.0));
        assert_eq!(set["after"]["alpha"], json!(WaterPlane::default().alpha));
        let out = water(
            &mut world,
            &json!({ "at": [[-20.0, 0.0], [20.0, 0.0]], "time": 2.5 }),
        )
        .expect("water");
        let wet = &out["probes"][0];
        let dry = &out["probes"][1];
        assert_eq!(wet["wet"], json!(true), "{out}");
        assert!(
            (wet["depth"].as_f64().expect("depth") - 1.0).abs() < 0.01,
            "{out}"
        );
        assert!(
            (wet["shore_distance"].as_f64().expect("shore") - 20.0).abs() < 0.2,
            "{out}"
        );
        assert!(
            wet["opacity_from_above"].as_f64().expect("opacity") > 0.0,
            "{out}"
        );
        assert_eq!(dry["wet"], json!(false), "{out}");
        assert_eq!(dry["foam"], json!(0.0), "{out}");
        assert_eq!(out["time"], json!(2.5));

        // Edits are validated and keep the other fields.
        assert!(set_water(&mut world, &json!({ "water": { "alpha": 2.0 } })).is_err());
        assert!(set_water(&mut world, &json!({ "water": { "nope": 1.0 } })).is_err());
        assert!(set_water(&mut world, &json!({ "water": [1.0] })).is_err());
        assert!(set_water(&mut world, &json!({})).is_err());
        assert!(
            set_water(
                &mut world,
                &json!({ "enabled": false, "water": { "level": 1.0 } })
            )
            .is_err()
        );
        let out = water(&mut world, &json!({})).expect("water");
        assert_eq!(
            out["water"]["level"],
            json!(0.0),
            "a refused edit changes nothing: {out}"
        );
        let set = set_water(&mut world, &json!({ "water": { "foam_width": 3.0 } })).expect("set");
        assert_eq!(set["after"]["level"], json!(0.0));
        assert_eq!(set["after"]["foam_width"], json!(3.0));

        // Bad probes.
        let many: Vec<Value> = (0..65).map(|i| json!([i as f64, 0.0])).collect();
        assert!(water(&mut world, &json!({ "at": many })).is_err());
        assert!(water(&mut world, &json!({ "at": [[1.0]] })).is_err());
        assert!(water(&mut world, &json!({ "at": [0.0, 1.0] })).is_err());
        assert!(water(&mut world, &json!({ "at": [[1e300, 0.0]] })).is_err());
        assert!(water(&mut world, &json!({ "at": "here" })).is_err());

        // Removing it.
        let set = set_water(&mut world, &json!({ "enabled": false })).expect("remove");
        assert_eq!(set["before"]["foam_width"], json!(3.0));
        assert!(set["after"].is_null());
        assert!(water(&mut world, &json!({})).expect("water")["water"].is_null());
    }

    #[test]
    fn set_vt_needs_a_linked_draw_and_valid_settings() {
        let mut world = World::new();
        assert!(vt(&mut world).is_err());
        world.insert_resource(TerrainVtHandle::default());
        assert!(vt(&mut world).is_err(), "an unlinked handle is not a draw");
        assert!(
            set_vt(&mut world, &json!({ "enabled": true })).is_err(),
            "no draw"
        );
        assert!(set_vt(&mut world, &json!({})).is_err());
    }

    #[test]
    fn load_holes_validates_the_document_and_needs_a_draw() {
        let mut world = World::new();
        let doc = "(version: 1, shapes: [rect(center: (0.0, 0.0), half_extents: (15.0, 10.0), rotation_radians: 0.0)])";
        assert!(load_holes(&mut world, &json!({ "ron": doc })).is_err(), "no draw");
        world.insert_resource(TerrainHoles::default());
        assert!(
            load_holes(&mut world, &json!({ "ron": doc })).is_err(),
            "unlinked"
        );
        assert!(load_holes(&mut world, &json!({})).is_err());
        assert!(load_holes(&mut world, &json!({ "ron": "(version: 9, shapes: [])" })).is_err());
        assert!(load_holes(&mut world, &json!({ "path": "/etc/passwd" })).is_err());
        assert!(load_holes(&mut world, &json!({ "path": "../x.holes.ron" })).is_err());
        assert!(
            load_holes(&mut world, &json!({ "path": "maps/a.holes.ron" })).is_err(),
            "no project"
        );
    }

    #[test]
    fn raycast_passes_through_the_applied_holes() {
        let mut world = World::new();
        assert!(
            raycast(
                &mut world,
                &json!({ "origin": [0.0, 50.0, 0.0], "direction": [0.0, -1.0, 0.0] })
            )
            .is_err()
        );
        let n = 129u32;
        let map =
            regia_terrain::TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 128.0, 10.0, -5.0)
                .expect("map");
        let terrain = Terrain::new(
            &map.to_heightfield(),
            128.0,
            10.0,
            -5.0,
            regia_terrain::CdlodConfig::default(),
            3,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain");
        world.spawn(terrain.clone());
        let down = json!({ "origin": [0.0, 50.0, 0.0], "direction": [0.0, -1.0, 0.0] });
        let out = raycast(&mut world, &down).expect("ray");
        assert!(
            (out["hit"]["distance"].as_f64().expect("distance") - 50.0).abs() < 1e-2,
            "{out}"
        );
        assert_eq!(out["hit"]["holes"], json!(0));
        // A 30 m mouth on the component: the ray falls through.
        let mut cut = terrain;
        cut.holes = Some(
            HolesMask::from_shapes(vec![HoleShape::segment([-15.0, 0.0], [15.0, 0.0], 20.0)]).expect("mask"),
        );
        world.clear_entities();
        world.spawn(cut);
        let out = raycast(&mut world, &down).expect("ray");
        assert!(out["hit"].is_null(), "{out}");
        let beside = json!({ "origin": [40.0, 50.0, 0.0], "direction": [0.0, -1.0, 0.0] });
        let out = raycast(&mut world, &beside).expect("ray");
        assert_eq!(out["hit"]["holes"], json!(1), "{out}");
        assert!(
            raycast(
                &mut world,
                &json!({ "origin": [0.0, 1.0, 0.0], "direction": [0.0, 0.0, 0.0] })
            )
            .is_err()
        );
        // Review holes #13: a direction whose length overflows is an error, not `hit: null`.
        assert!(
            raycast(
                &mut world,
                &json!({ "origin": [0.0, 1.0, 0.0], "direction": [1e30, 0.0, 0.0] })
            )
            .is_err()
        );
    }

    /// Review holes #13: `terrain.load_holes` refuses a path whose symlink leaves the project
    /// root, and Windows-style prefixed paths.
    #[cfg(unix)]
    #[test]
    fn load_holes_stays_inside_the_project_root() {
        let base = std::env::temp_dir().join(format!("regia-load-holes-{}", std::process::id()));
        let root = base.join("project");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("maps")).expect("root");
        std::fs::create_dir_all(&outside).expect("outside");
        let doc = "(version: 1, shapes: [rect(center: (0.0, 0.0), half_extents: (15.0, 10.0), rotation_radians: 0.0)])";
        std::fs::write(outside.join("secret.holes.ron"), doc).expect("write");
        std::fs::write(root.join("maps/inside.holes.ron"), doc).expect("write");
        let _ = std::fs::remove_file(root.join("maps/escape.holes.ron"));
        std::os::unix::fs::symlink(
            outside.join("secret.holes.ron"),
            root.join("maps/escape.holes.ron"),
        )
        .expect("symlink");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_asset::AssetPlugin {
            settings: regia_asset::AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".into(),
                content_dir: None,
            },
        });
        let world = app.world_mut();
        // An unlinked handle: a path that passes every check fails later, on the missing draw.
        world.insert_resource(TerrainHoles::default());
        let err = load_holes(world, &json!({ "path": "maps/escape.holes.ron" })).expect_err("escapes");
        assert!(
            err.message.contains("outside the project root"),
            "{}",
            err.message
        );
        let err = load_holes(world, &json!({ "path": "C:maps/inside.holes.ron" }));
        // On Unix `C:maps` is a plain directory name: it is simply missing there.
        assert!(err.is_err());
        let err = load_holes(world, &json!({ "path": "maps/inside.holes.ron" })).expect_err("no draw");
        assert!(
            !err.message.contains("outside") && err.message.contains("linked"),
            "the inside file passed the path checks: {}",
            err.message
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn set_holes_needs_a_linked_draw_and_valid_shapes() {
        let mut world = World::new();
        assert!(holes(&mut world).is_err());
        world.insert_resource(TerrainHoles::default());
        assert!(holes(&mut world).is_err(), "an unlinked handle is not a draw");
        let ramp = json!({ "segments": [{ "from": [0.0, 0.0], "to": [0.0, 25.0], "width_m": 20.0 }] });
        assert!(set_holes(&mut world, &ramp).is_err(), "no draw to cut");
        assert!(set_holes(&mut world, &json!({})).is_err());
        assert!(set_holes(&mut world, &json!({ "segments": [{ "from": [0.0, 0.0] }] })).is_err());
        assert!(
            set_holes(
                &mut world,
                &json!({ "shapes": [{ "polygon": { "points": [[0.0, 0.0]] } }] })
            )
            .is_err()
        );
        assert!(set_holes(&mut world, &json!({ "shapes": [{ "blob": {} }] })).is_err());
    }

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(ScatterControl::default());
        world
    }

    #[test]
    fn toggles_round_trip() {
        let mut world = world();
        let out = set_scatter(&mut world, &json!({ "enabled": false, "contact": false })).expect("set");
        assert_eq!(out["before"]["enabled"], json!(true));
        assert_eq!(out["after"]["enabled"], json!(false));
        assert_eq!(out["after"]["contact"], json!(false));
        let read = scatter(&mut world).expect("read");
        assert_eq!(read["enabled"], json!(false));
        let out =
            set_scatter(&mut world, &json!({ "class": "gravel", "class_enabled": false })).expect("class");
        assert_eq!(out["after"]["class_overrides"][0]["name"], json!("gravel"));
        // The impostor far field is opt-in.
        assert_eq!(read["impostors"], json!(false));
        let out = set_scatter(&mut world, &json!({ "impostors": true })).expect("impostors");
        assert_eq!(out["before"]["impostors"], json!(false));
        assert_eq!(out["after"]["impostors"], json!(true));
        assert!(set_scatter(&mut world, &json!({ "impostors": 1 })).is_err());
        // The dense-scatter detail limits are opt-in too.
        assert_eq!(read["detail"], json!(false));
        let out = set_scatter(&mut world, &json!({ "detail": true })).expect("detail");
        assert_eq!(out["before"]["detail"], json!(false));
        assert_eq!(out["after"]["detail"], json!(true));
        assert!(set_scatter(&mut world, &json!({ "detail": "yes" })).is_err());
        // The scatter shadow LOD hint is off until set, and null clears it.
        assert_eq!(read["shadow_lod"], json!(null));
        let out = set_scatter(&mut world, &json!({ "shadow_lod": { "max_lod": 1 } })).expect("hint");
        assert_eq!(out["after"]["shadow_lod"], json!({ "max_lod": 1, "min_lod": 0 }));
        assert!(set_scatter(&mut world, &json!({ "shadow_lod": { "max_lod": "far" } })).is_err());
        let out = set_scatter(&mut world, &json!({ "shadow_lod": null })).expect("clear");
        assert_eq!(out["after"]["shadow_lod"], json!(null));
    }

    #[test]
    fn bad_params_are_rejected() {
        let mut world = world();
        assert!(set_scatter(&mut world, &json!({})).is_err());
        assert!(set_scatter(&mut world, &json!({ "enabled": "yes" })).is_err());
        assert!(set_scatter(&mut world, &json!({ "class": "gravel" })).is_err());
        let mut empty = World::new();
        assert!(scatter(&mut empty).is_err());
    }

    #[test]
    fn unknown_class_is_not_found_once_a_field_reported() {
        let mut world = world();
        world.resource_mut::<ScatterControl>().report = regia_terrain::ScatterReport {
            installed: true,
            classes: vec![regia_terrain::ScatterClassReport {
                name: "boulders".into(),
                enabled: true,
                resident: 10,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(set_scatter(&mut world, &json!({ "class": "nope", "class_enabled": false })).is_err());
        assert!(
            set_scatter(
                &mut world,
                &json!({ "class": "boulders", "class_enabled": false })
            )
            .is_ok()
        );
    }

    /// ADR-3170: a class takes the foliage subsurface profile, alone or with its toggle.
    #[test]
    fn a_class_caster_hint_sets_clears_and_refuses() {
        let mut world = world();
        let out = set_scatter(
            &mut world,
            &json!({ "class": "ferns", "class_caster": { "from_lod": 1, "max_lod": 3, "leaf_keep_one_in": 4 } }),
        )
        .expect("caster");
        assert_eq!(
            out["after"]["class_casters"],
            json!([{ "name": "ferns", "caster": {
                "from_lod": 1, "max_lod": 3, "min_lod": 0, "leaf_keep_one_in": 4,
            } }])
        );
        let out = set_scatter(&mut world, &json!({ "class": "ferns", "class_caster": null })).expect("clear");
        assert_eq!(
            out["after"]["class_casters"],
            json!([{ "name": "ferns", "caster": null }])
        );
        for bad in [
            json!({ "class_caster": { "from_lod": 1 } }),
            json!({ "class": "ferns", "class_caster": 3 }),
            json!({ "class": "ferns", "class_caster": { "from_lod": 255 } }),
            json!({ "class": "ferns", "class_caster": { "leaf_keep_one_in": 17 } }),
            json!({ "class": "ferns", "class_caster": { "max_lod": "far" } }),
        ] {
            assert!(set_scatter(&mut world, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_class_season_sets_clears_and_refuses() {
        let mut world = world();
        let out = set_scatter(
            &mut world,
            &json!({ "class": "ferns", "class_season": { "density": 0.25, "tint": [1.5, 1.0, 0.5] } }),
        )
        .expect("direct");
        let season = &out["after"]["class_seasons"][0]["season"];
        assert_eq!(season["density"], json!(0.25));
        assert_eq!(season["tint"], json!([1.5, 1.0, 0.5]));
        // A kind and a time of year: the framework's look, whose winter broadleaf is bare.
        let out = set_scatter(
            &mut world,
            &json!({ "class": "ferns", "class_season": { "kind": "broadleaf", "year_fraction": 0.75 } }),
        )
        .expect("kind");
        assert_eq!(out["after"]["class_seasons"][0]["season"]["density"], json!(0.0));
        let out = set_scatter(&mut world, &json!({ "class": "ferns", "class_season": null })).expect("clear");
        assert_eq!(
            out["after"]["class_seasons"],
            json!([{ "name": "ferns", "season": null }])
        );
        for bad in [
            json!({ "class_season": { "density": 0.5 } }),
            json!({ "class": "ferns", "class_season": "winter" }),
            json!({ "class": "ferns", "class_season": {} }),
            json!({ "class": "ferns", "class_season": { "density": 1.5 } }),
            json!({ "class": "ferns", "class_season": { "density": 0.5, "tint": [1.0, 1.0] } }),
            json!({ "class": "ferns", "class_season": { "density": 0.5, "tint": [1.0, 1.0, 9.0] } }),
            json!({ "class": "ferns", "class_season": { "kind": "cactus" } }),
            json!({ "class": "ferns", "class_season": { "kind": "grass", "density": 0.5 } }),
            json!({ "class": "ferns", "class_season": { "density": 0.5, "year_fraction": 0.5 } }),
        ] {
            assert!(set_scatter(&mut world, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn class_subsurface_is_an_override() {
        let mut world = world();
        let out = set_scatter(
            &mut world,
            &json!({ "class": "ferns", "class_subsurface": "foliage" }),
        )
        .expect("foliage");
        assert_eq!(
            out["after"]["foliage_subsurface"],
            json!([{ "name": "ferns", "on": true }])
        );
        assert_eq!(out["after"]["class_overrides"], json!([]), "no toggle asked");
        let out = set_scatter(
            &mut world,
            &json!({ "class": "ferns", "class_subsurface": "off", "class_enabled": true }),
        )
        .expect("both");
        assert_eq!(
            out["after"]["foliage_subsurface"],
            json!([{ "name": "ferns", "on": false }])
        );
        assert_eq!(out["after"]["class_overrides"][0]["enabled"], json!(true));
        for bad in [
            json!({ "class": "ferns" }),
            json!({ "class": "ferns", "class_subsurface": "skin" }),
            json!({ "class": "ferns", "class_subsurface": true }),
            json!({ "class_subsurface": "foliage" }),
            json!({ "enabled": true, "class_subsurface": "skin" }),
        ] {
            assert!(set_scatter(&mut world, &bad).is_err(), "{bad}");
        }
    }

    /// A swaying class says why it does not scatter.
    #[test]
    fn the_report_names_why_a_class_cannot_scatter() {
        let mut world = world();
        world.resource_mut::<ScatterControl>().report = regia_terrain::ScatterReport {
            installed: true,
            classes: vec![regia_terrain::ScatterClassReport {
                name: "ferns".into(),
                subsurface: "foliage".into(),
                subsurface_unsupported: Some("material_graph"),
                ..Default::default()
            }],
            ..Default::default()
        };
        let got = scatter(&mut world).expect("report");
        assert_eq!(got["report"]["classes"][0]["subsurface"], json!("foliage"));
        assert_eq!(
            got["report"]["classes"][0]["subsurface_unsupported"],
            json!("material_graph")
        );
    }

    /// A refused call changes nothing: the class is not stored and `enabled` not applied when a
    /// later parameter is bad.
    #[test]
    fn a_refused_scatter_call_changes_nothing() {
        let mut world = world();
        let before = scatter(&mut world).expect("report");
        for bad in [
            json!({ "class": "ferns", "class_subsurface": "foliage", "contact": "no" }),
            json!({ "class": "ferns", "class_enabled": false, "impostors": 1 }),
            json!({ "enabled": false, "detail": "no" }),
            json!({ "enabled": false, "shadow_lod": 3 }),
        ] {
            assert!(set_scatter(&mut world, &bad).is_err(), "{bad}");
            assert_eq!(scatter(&mut world).expect("report"), before, "{bad}");
        }
    }

    fn terrain_world() -> (World, Entity) {
        let mut world = World::new();
        let n = 65u32;
        let map =
            regia_terrain::TerrainMap::from_heights(n, n, vec![50.0; (n * n) as usize], 64.0, 100.0, 0.0)
                .expect("map");
        let terrain = Terrain::new(
            &map.to_heightfield(),
            64.0,
            100.0,
            0.0,
            regia_terrain::CdlodConfig::default(),
            3,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain");
        let paint = TerrainPaint(regia_terrain::LayerWeightMap::new(n, n, 2).expect("paint"));
        let e = world.spawn((terrain, paint)).id();
        (world, e)
    }

    #[test]
    fn stroke_falloff_curves_change_the_shoulder_and_reject_unknown_names() {
        let mut shoulders = Vec::new();
        for falloff in ["smooth", "linear", "sphere", "tip"] {
            let (mut world, e) = terrain_world();
            let before = world
                .get::<Terrain>(e)
                .expect("terrain")
                .map
                .sample_bilinear(6.0, 0.0);
            stroke(
                &mut world,
                &json!({
                    "mode": "raise", "center": [0.0, 0.0], "radius": 8.0,
                    "strength": 4.0, "falloff": falloff
                }),
            )
            .expect("stroke");
            let after = world
                .get::<Terrain>(e)
                .expect("terrain")
                .map
                .sample_bilinear(6.0, 0.0);
            shoulders.push(after - before);
        }
        assert!(
            shoulders[2] > shoulders[1] && shoulders[1] > shoulders[0] && shoulders[0] > shoulders[3],
            "different shoulders: {shoulders:?}"
        );
        let (mut world, _) = terrain_world();
        assert!(
            stroke(
                &mut world,
                &json!({
                    "mode": "raise", "center": [0.0, 0.0], "radius": 8.0, "falloff": "unknown"
                })
            )
            .is_err()
        );
    }

    #[test]
    fn stroke_raises_and_logs_one_undo_entry() {
        let (mut world, e) = terrain_world();
        let out = stroke(
            &mut world,
            &json!({ "mode": "raise", "center": [0.0, 0.0], "radius": 6.0, "strength": 3.0, "hardness": 1.0 }),
        )
        .expect("stroke");
        assert_eq!(out["mode"], json!("raise"));
        let before = out["height_before"].as_f64().expect("number");
        let after = out["height_after"].as_f64().expect("number");
        assert!((after - before - 3.0).abs() < 1e-3, "{out}");
        // Sample (32, 32) ± 6 straddles the four 32-sample edit cells around it.
        assert_eq!(out["cells"], json!([[0, 0], [0, 1], [1, 0], [1, 1]]));
        assert_eq!(out["cell_size"], json!(32));
        assert_eq!(world.resource::<TerrainStrokeLog>().strokes.len(), 1);
        assert!(
            world
                .get::<TerrainEdits>(e)
                .is_some_and(|ed| !ed.unsaved_tiles().is_empty())
        );
        let painted = stroke(
            &mut world,
            &json!({ "mode": "paint", "layer": 1, "center": [0.0, 0.0], "to": [10.0, 0.0], "radius": 3.0 }),
        )
        .expect("paint");
        assert_eq!(painted["target"], json!({ "layer": 1 }));
        assert!(
            world
                .get::<TerrainPaint>(e)
                .is_some_and(|p| p.0.get(1, 32, 32) > 200)
        );
        assert_eq!(world.resource::<TerrainStrokeLog>().strokes.len(), 2);
    }

    /// Under an editor every stroke becomes an undo step; without one only the newest are kept.
    #[test]
    fn the_stroke_log_drops_nothing_an_editor_will_drain() {
        for (drained, kept) in [(true, 300), (false, regia_terrain::STROKE_LOG_CAP)] {
            let (mut world, _) = terrain_world();
            world.insert_resource(TerrainStrokeLog {
                drained,
                ..TerrainStrokeLog::default()
            });
            for _ in 0..300 {
                stroke(
                    &mut world,
                    &json!({ "mode": "raise", "center": [0.0, 0.0], "radius": 6.0, "strength": 1.0 }),
                )
                .expect("stroke");
            }
            assert_eq!(world.resource::<TerrainStrokeLog>().strokes.len(), kept);
        }
    }

    /// Noise and Ramp strokes (ADR-1853): seeded noise reproduces, a ramp lands its end heights.
    #[test]
    fn stroke_noise_and_ramp() {
        let (mut world, e) = terrain_world();
        let noise = json!({ "mode": "noise", "center": [0.0, 0.0], "radius": 20.0, "strength": 2.0,
                            "hardness": 1.0, "wavelength": 6.0, "seed": 3 });
        let out = stroke(&mut world, &noise).expect("noise");
        assert_eq!(out["mode"], json!("noise"));
        assert!(out["samples"].as_u64().unwrap_or(0) > 100, "{out}");
        let heights = |w: &World| -> Vec<u32> {
            w.get::<Terrain>(e)
                .map(|t| t.map.heights().iter().map(|h| h.to_bits()).collect())
                .unwrap_or_default()
        };
        let first = heights(&world);
        let (mut again, _) = terrain_world();
        stroke(&mut again, &noise).expect("noise again");
        assert_eq!(heights(&again), first, "the same seed gives the same field");
        assert!(
            stroke(
                &mut world,
                &json!({ "mode": "noise", "center": [0.0, 0.0], "radius": 4.0, "wavelength": -1.0 })
            )
            .is_err()
        );

        let (mut world, e) = terrain_world();
        let ramp = json!({ "mode": "ramp", "center": [-20.0, 0.0], "to": [20.0, 0.0], "radius": 4.0,
                           "hardness": 1.0, "from_height": 40.0, "to_height": 60.0 });
        let out = stroke(&mut world, &ramp).expect("ramp");
        assert_eq!(out["mode"], json!("ramp"));
        let t = world.get::<Terrain>(e).expect("terrain");
        assert!((t.map.sample_bilinear(-20.0, 0.0) - 40.0).abs() < 1e-3);
        assert!((t.map.sample_bilinear(0.0, 0.0) - 50.0).abs() < 1e-3);
        assert!((t.map.sample_bilinear(20.0, 0.0) - 60.0).abs() < 1e-3);
        assert!(
            stroke(
                &mut world,
                &json!({ "mode": "ramp", "center": [0.0, 0.0], "radius": 4.0 })
            )
            .is_err(),
            "a ramp needs an end point"
        );
    }

    #[test]
    fn stroke_rejects_bad_params() {
        let (mut world, _) = terrain_world();
        assert!(stroke(&mut world, &json!({ "center": [0.0, 0.0], "radius": 2.0 })).is_err());
        assert!(
            stroke(
                &mut world,
                &json!({ "mode": "melt", "center": [0.0, 0.0], "radius": 2.0 })
            )
            .is_err()
        );
        assert!(stroke(&mut world, &json!({ "mode": "raise", "radius": 2.0 })).is_err());
        assert!(
            stroke(
                &mut world,
                &json!({ "mode": "raise", "center": [0.0], "radius": 2.0 })
            )
            .is_err()
        );
        assert!(stroke(&mut world, &json!({ "mode": "raise", "center": [0.0, 0.0] })).is_err());
        assert!(
            stroke(
                &mut world,
                &json!({ "mode": "paint", "center": [0.0, 0.0], "radius": 2.0 })
            )
            .is_err()
        );
        assert!(
            stroke(
                &mut world,
                &json!({ "mode": "paint", "layer": 5, "center": [0.0, 0.0], "radius": 2.0 })
            )
            .is_err()
        );
        let mut empty = World::new();
        assert!(
            stroke(
                &mut empty,
                &json!({ "mode": "raise", "center": [0.0, 0.0], "radius": 2.0 })
            )
            .is_err()
        );
    }
}
