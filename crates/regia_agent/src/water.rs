// SPDX-License-Identifier: MPL-2.0
//! Water bodies on the agent protocol (ADR-1399, `book/agents/water.md`): `water.state` reports the
//! bodies, the tier, the clock and what the last frame drew; `water.probe` says what the water is
//! at points (surface height, flow, Froude number, standing-wave length); `water.set` switches the
//! water, its tier and debug view, holds or steps the clock, and edits one body's look and ice.
//!
//! Swell (ADR-1620): `water.surface` answers the visible surface at points (height, normal,
//! velocity, crest foam) through the same CPU surface the shader mirrors, `water.raycast` the
//! first crossing of a ray, and `water.set { swell }` puts trochoidal waves on a plane body;
//! `water.set { terrain_planes }` switches the runtime bodies of the terrains' shoreline planes.
//!
//! Water finish (spec 87, ADR-1828): `water.state` reports each plane's baked shore field,
//! `water.set { shore }` switches the shore fields, and `water.surface` and `water.raycast` answer
//! the highest wet surface over every body kind (the bed taken into account). `water.surface`
//! adds depth, shoreline distance, breaker and shore foam. `water.raycast` adds depth, breaker
//! and shore foam, and does not include shoreline distance (spec 87 § 6). `water.floaters`
//! lists the floating bodies.
//!
//! FFT ocean (ADR-1969, `book/agents/water-ocean.md`): `water.set { ocean }` puts a spectrum on a
//! plane body, `water.set { ocean_draw }` switches its drawing, `water.ocean` reports its sea
//! state and bands, and `water.ocean_check` compares the compute pass with the CPU mirror on the
//! GPU in use. `water.set_mode` switches one plane between its swell and its ocean and picks the
//! drawn grid, and `water.ocean_cost` times the compute pass at each grid (ADR-2126).

use regia_app::prelude::*;
use regia_terrain::water_body::float::Floater;
use regia_terrain::water_body::ocean::{self, FftOcean, OCEAN_GRIDS, OCEAN_LOOP_SECONDS, OCEAN_QUERY_SIZE};
use regia_terrain::water_body::swell::{MAX_RAY_DISTANCE, MAX_SWELL_WAVES};
use regia_terrain::water_body::{
    BodyRef, FloaterState, MIN_FETCH, SeaWind, ShoreField, TerrainWater, WaterDebug, WaterPoint, WaterShore,
    WaterSurface, WaveMode, WeatherSea, raycast_all, surface_at,
};
use regia_terrain::{
    IceShelf, Swell, WaterBody, WaterCaustics, WaterClock, WaterLook, WaterSettings, WaterShape, WaterState,
    WaterTier,
};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::protocol::RpcError;

/// Points one `water.probe` call answers at most.
const PROBES_MAX: usize = 64;

/// The grids `water.ocean_cost` times unless told otherwise.
const COST_GRIDS: [u32; 3] = [256, 512, 1024];

/// Evolves per grid `water.ocean_cost` times unless told otherwise, and at most.
const COST_FRAMES: (u64, u64) = (32, 256);

fn no_plugin() -> RpcError {
    RpcError::not_found(
        "no WaterSettings resource in this app",
        "add regia_terrain::WaterPlugin (it inserts WaterSettings and WaterClock)",
    )
}

/// Every water body, in entity order (the order the renderer draws them).
fn bodies(world: &mut World) -> Vec<(Entity, WaterBody)> {
    let mut query = world.query::<(Entity, &WaterBody)>();
    let mut list: Vec<(Entity, WaterBody)> = query.iter(world).map(|(e, b)| (e, b.clone())).collect();
    list.sort_by_key(|(entity, _)| *entity);
    list
}

/// Every water body with its shore field (spec 87), in entity order.
fn bodies_with_shore(world: &mut World) -> Vec<(Entity, WaterBody, Option<Arc<ShoreField>>)> {
    let mut query = world.query::<(Entity, &WaterBody, Option<&WaterShore>)>();
    let mut list: Vec<_> = query
        .iter(world)
        .map(|(e, b, s)| (e, b.clone(), s.map(|s| Arc::clone(&s.field))))
        .collect();
    list.sort_by_key(|(entity, _, _)| *entity);
    list
}

fn refs(list: &[(Entity, WaterBody, Option<Arc<ShoreField>>)]) -> impl Iterator<Item = BodyRef<'_>> {
    list.iter().map(|(entity, body, shore)| BodyRef {
        entity: *entity,
        body,
        shore: shore.as_deref(),
    })
}

/// The shore switch in effect (on without the plugin, as `WaterQuery` does).
fn shore_on(world: &World) -> bool {
    world.get_resource::<WaterSettings>().is_none_or(|s| s.shore)
}

/// The bodies the queries answer with: as the tier draws them when
/// `WaterSettings::queries_follow_tier` is on (ADR-2843), as authored otherwise.
fn query_bodies(world: &mut World) -> Vec<(Entity, WaterBody, Option<Arc<ShoreField>>)> {
    let follow = world
        .get_resource::<WaterSettings>()
        .and_then(|s| s.query_tier(world.get_resource::<regia_render::RenderSettings>()));
    let list = bodies_with_shore(world);
    // The renderer's own fallbacks (a grid the device refused, a second spectrum; ADR-3251).
    let drawn = world.get_resource::<regia_terrain::water_body::WaterDrawnOceans>();
    match follow {
        None => list,
        Some((tier, draw)) => list
            .into_iter()
            .map(|(e, b, s)| {
                let ocean = regia_terrain::water_body::WaterDrawnOceans::ocean_drawn(drawn, e, tier, draw);
                (e, b.drawn_with(tier, ocean).into_owned(), s)
            })
            .collect(),
    }
}

/// The sea-state table's state of a body with a [`WeatherSea`] (ADR-2342), or `null`.
fn sea_json(world: &World, entity: Entity) -> Value {
    let Some(sea) = world.get::<WeatherSea>(entity) else {
        return Value::Null;
    };
    let wind = world.get_resource::<SeaWind>().copied().unwrap_or_default();
    let state = sea.state_for(wind);
    let row = state.row();
    let (height, wavelength) = state.height_and_wavelength();
    let drives = if world.get::<WaterBody>(entity).is_some_and(|b| b.ocean.is_some()) {
        "ocean_wind"
    } else {
        "swell"
    };
    json!({
        "drives": drives,
        "fetch": sea.fetch,
        "waves": sea.waves,
        "wind_m_s": state.wind_m_per_s,
        "force": row.force,
        "height_m": height,
        "wavelength_m": wavelength,
    })
}

fn shore_json(world: &World, entity: Entity) -> Value {
    match world.get::<WaterShore>(entity) {
        None => Value::Null,
        Some(shore) => {
            let st = shore.field.stats();
            json!({
                "size": st.size,
                "cell_m": st.cell,
                "level": st.level,
                "wet_fraction": st.wet_fraction,
                "max_depth_m": finite_or_null(st.max_depth),
                "shoreline_texels": st.shoreline_texels,
            })
        }
    }
}

fn finite_or_null(v: f32) -> Value {
    if v.is_finite() { json!(v) } else { Value::Null }
}

fn swell_json(body: &WaterBody) -> Value {
    match &body.swell {
        None => Value::Null,
        Some(swell) => json!({
            "waves": swell.waves.len(),
            "fold_sum": swell.fold_sum(),
            "max_rise_m": swell.max_rise(),
            "max_shift_m": swell.max_shift(),
            "longest_m": swell.waves.iter().map(|w| w.wavelength).fold(0.0f32, f32::max),
            "crest_foam": swell.crest_foam,
            "foam_onset": swell.foam_onset,
            "active": body.active_swell().is_some(),
        }),
    }
}

fn ocean_json(body: &WaterBody) -> Value {
    body.active_ocean().map_or(Value::Null, ocean_json_of)
}

fn ocean_json_of(o: &FftOcean) -> Value {
    json!({
        "seed": o.seed,
        "spectrum": o.spectrum.name(),
        "wind_speed_m_s": o.wind_speed,
        "wave_height_m": o.expected_height(),
        "peak_wavelength_m": o.peak_wavelength(),
        "tiles_m": o.tiles,
        "grid": o.grid,
        "foam_decay_s": o.foam_decay,
        "foam_growth_s": o.foam_growth,
        "whitecap_rescale": o.whitecap_rescale,
    })
}

fn body_json(entity: Entity, body: &WaterBody, terrain: Option<Entity>) -> Result<Value, RpcError> {
    let look = serde_json::to_value(body.look)
        .map_err(|e| RpcError::internal(format!("water look serialization failed: {e}")))?;
    let ice = serde_json::to_value(body.ice)
        .map_err(|e| RpcError::internal(format!("water ice serialization failed: {e}")))?;
    let shape = match &body.shape {
        WaterShape::Plane {
            center,
            half_extent,
            flow,
        } => json!({
            "kind": "plane",
            "center": center,
            "half_extent": half_extent,
            "flow": flow,
        }),
        WaterShape::Ribbon { points } => {
            let speeds = body.point_speeds();
            let span = |lo: f32, hi: f32, v: f32| (lo.min(v), hi.max(v));
            let (w_lo, w_hi) = points
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), p| {
                    span(lo, hi, p.width)
                });
            let (s_lo, s_hi) = speeds
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| span(lo, hi, *v));
            let drop =
                points.first().map_or(0.0, |a| a.position[1]) - points.last().map_or(0.0, |b| b.position[1]);
            json!({
                "kind": "ribbon",
                "points": points.len(),
                "length_m": body.length(),
                "drop_m": drop,
                "width_m": [finite_or_null(w_lo), finite_or_null(w_hi)],
                "speed_m_s": [finite_or_null(s_lo), finite_or_null(s_hi)],
            })
        }
    };
    Ok(json!({
        "entity": entity.to_bits(),
        "shape": shape,
        "obstacles": body.obstacles.len(),
        "ice": ice,
        "manning_n": body.manning_n,
        "look": look,
        "swell": swell_json(body),
        "ocean": ocean_json(body),
        "mode": body.wave_mode().map(WaveMode::name),
        "parked_ocean": body.parked_ocean.is_some(),
        "terrain": terrain.map(Entity::to_bits),
    }))
}

/// The terrain each runtime body of a shoreline plane draws ([`TerrainWater`]).
fn terrain_of(world: &World, entity: Entity) -> Option<Entity> {
    world.get::<TerrainWater>(entity).map(|tag| tag.terrain)
}

/// `water.state`: the settings, the clock, the tier in effect, the last frame's draw and every
/// body. Works without the plugin (`plugin: false`, settings `null`).
pub(crate) fn state(world: &mut World) -> Result<Value, RpcError> {
    let settings = world.get_resource::<WaterSettings>().copied();
    let clock = world.get_resource::<WaterClock>().copied();
    let tier = settings.map(|s| s.effective_tier(world.get_resource::<regia_render::RenderSettings>()));
    let stats = world
        .get_resource::<WaterState>()
        .map(|state| state.renderer.stats());
    let last_frame = serde_json::to_value(stats)
        .map_err(|e| RpcError::internal(format!("water stats serialization failed: {e}")))?;
    let list = bodies(world)
        .iter()
        .map(|(entity, body)| {
            let terrain = terrain_of(world, *entity);
            let mut v = body_json(*entity, body, terrain)?;
            v["shore"] = shore_json(world, *entity);
            v["sea"] = sea_json(world, *entity);
            Ok(v)
        })
        .collect::<Result<Vec<_>, RpcError>>()?;
    Ok(json!({
        "plugin": settings.is_some(),
        "enabled": settings.map(|s| s.enabled),
        "tier": tier.map(WaterTier::name),
        "forced_tier": settings.and_then(|s| s.tier).map(WaterTier::name),
        "debug": settings.map(|s| s.debug.name()),
        "terrain_planes": settings.map(|s| s.terrain_planes),
        "shore": settings.map(|s| s.shore),
        "ocean_draw": settings.map(|s| s.ocean_draw),
        "underwater": settings.map(|s| s.underwater),
        "ring_culling": settings.map(|s| s.ring_culling),
        "ring_snap": settings.map(|s| s.ring_snap),
        "queries_follow_tier": settings.map(|s| s.queries_follow_tier),
        "gpu_timing": settings.map(|s| s.gpu_timing),
        "sea_wind": world.get_resource::<SeaWind>().map(|w| w.wind_m_per_s),
        "clock": clock.map(|c| json!({
            "tick": c.tick,
            "step_seconds": c.step_seconds,
            "seconds": c.seconds(),
            "running": c.running,
        })),
        "renderer": stats.is_some(),
        "last_frame": last_frame,
        "bodies": list,
    }))
}

fn point_json(at: [f32; 2], hit: Option<(Entity, WaterPoint)>) -> Value {
    match hit {
        None => json!({ "at": at, "body": Value::Null }),
        Some((entity, p)) => json!({
            "at": at,
            "body": entity.to_bits(),
            "surface_height": p.surface_height,
            "flow": p.flow,
            "speed": p.speed(),
            "lateral": p.lateral,
            "half_width": finite_or_null(p.half_width),
            "depth": p.depth,
            "distance_along": p.distance_along,
            "froude": p.froude(),
            "standing_wavelength_m": p.standing_wavelength(),
        }),
    }
}

/// `at: [[x, z], ...]` (absent: none), at most [`PROBES_MAX`] finite points.
fn points_param(params: &Value) -> Result<Vec<[f32; 2]>, RpcError> {
    Ok(match params.get("at") {
        None => Vec::new(),
        Some(Value::Array(items)) => {
            if items.len() > PROBES_MAX {
                return Err(RpcError::invalid_params(format!(
                    "`at` holds {} points; at most {PROBES_MAX} per call",
                    items.len()
                )));
            }
            items
                .iter()
                .map(|item| {
                    let pair = item
                        .as_array()
                        .filter(|a| a.len() == 2)
                        .ok_or_else(|| RpcError::invalid_params("`at[]` must be [x, z] in meters"))?;
                    let x = pair[0].as_f64().map(|v| v as f32);
                    let z = pair[1].as_f64().map(|v| v as f32);
                    match (x, z) {
                        (Some(x), Some(z)) if x.is_finite() && z.is_finite() => Ok([x, z]),
                        _ => Err(RpcError::invalid_params("`at[]` must hold two finite numbers")),
                    }
                })
                .collect::<Result<_, _>>()?
        }
        Some(_) => return Err(RpcError::invalid_params("`at` must be an array of [x, z] points")),
    })
}

/// `water.probe { at: [[x, z], ...] }`: per point, the first body (entity order) that holds it and
/// what the water is there; `body: null` outside every body.
pub(crate) fn probe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let points = points_param(params)?;
    let list = bodies(world);
    let probes: Vec<Value> = points
        .iter()
        .map(|at| {
            let hit = list
                .iter()
                .find_map(|(entity, body)| body.probe(at[0], at[1]).map(|p| (*entity, p)));
            point_json(*at, hit)
        })
        .collect();
    Ok(json!({ "bodies": list.len(), "probes": probes }))
}

fn surface_json(at: [f32; 2], hit: Option<(Entity, WaterSurface)>) -> Value {
    match hit {
        None => json!({ "at": at, "body": Value::Null }),
        Some((entity, s)) => json!({
            "at": at,
            "body": entity.to_bits(),
            "height": s.height,
            "normal": s.normal,
            "velocity": s.velocity,
            "crest_foam": s.crest_foam,
            "rest": s.rest,
            "miss_m": s.miss,
            "depth_m": s.depth.map(finite_or_null),
            "shore_m": s.shore_distance.map(finite_or_null),
            "breaking": s.breaking,
            "shore_foam": s.shore_foam,
        }),
    }
}

/// `water.surface { at: [[x, z], ...] }`: per point, the highest wet surface over it among every
/// body (spec 87 § 4; ties: the lower entity), at the water clock's tick: height, normal, velocity
/// (orbital plus drift or flow), crest foam, the rest point, the inverse's horizontal miss, and
/// the depth, shoreline distance, breaker and shore foam where a shore field or a ribbon knows
/// them. A swell plane answers with every wave, whatever the tier draws; with a shore field (and
/// `shore` on) its waves are shoaled and dry land answers no body.
pub(crate) fn surface(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let points = points_param(params)?;
    let clock = world.get_resource::<WaterClock>().copied().unwrap_or_default();
    let shore = shore_on(world);
    let list = query_bodies(world);
    let answers: Vec<Value> = points
        .iter()
        .map(|at| surface_json(*at, surface_at(refs(&list), at[0], at[1], &clock, shore)))
        .collect();
    Ok(json!({
        "shore": shore,
        "bodies": list.len(),
        "tick": clock.tick,
        "seconds": clock.seconds(),
        "points": answers,
    }))
}

fn vec3_param(params: &Value, name: &str, required: bool) -> Result<Option<[f32; 3]>, RpcError> {
    let Some(v) = params.get(name) else {
        return if required {
            Err(RpcError::invalid_params(format!(
                "`{name}` is required: [x, y, z]"
            )))
        } else {
            Ok(None)
        };
    };
    let a = v
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be [x, y, z]")))?;
    let mut out = [0.0f32; 3];
    for (o, item) in out.iter_mut().zip(a) {
        *o = item
            .as_f64()
            .map(|x| x as f32)
            .filter(|x| x.is_finite())
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must hold three finite numbers")))?;
    }
    Ok(Some(out))
}

/// `water.raycast { origin, direction, max_distance }`: the nearest crossing of the ray with any
/// body's surface at the water clock's tick; `hit: false` when there is none.
pub(crate) fn raycast(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let origin = vec3_param(params, "origin", true)?.unwrap_or_default();
    let direction = vec3_param(params, "direction", true)?.unwrap_or_default();
    if direction.iter().map(|c| c * c).sum::<f32>() < 1.0e-12 {
        return Err(RpcError::invalid_params("`direction` must not be zero"));
    }
    let max_distance = match params.get("max_distance") {
        None => 1000.0,
        Some(v) => v
            .as_f64()
            .map(|x| x as f32)
            .filter(|x| x.is_finite() && *x >= 0.0 && *x <= MAX_RAY_DISTANCE)
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "`max_distance` must be a number in 0..={MAX_RAY_DISTANCE}"
                ))
            })?,
    };
    let clock = world.get_resource::<WaterClock>().copied().unwrap_or_default();
    let shore = shore_on(world);
    let list = query_bodies(world);
    let best = raycast_all(refs(&list), origin, direction, max_distance, &clock, shore);
    Ok(match best {
        None => json!({ "hit": false, "bodies": list.len(), "tick": clock.tick }),
        Some((entity, h)) => json!({
            "hit": true,
            "bodies": list.len(),
            "tick": clock.tick,
            "body": entity.to_bits(),
            "distance": h.distance,
            "point": h.point,
            "normal": h.normal,
            "from_below": h.from_below,
            "step_m": h.step,
            "velocity": h.surface.velocity,
            "crest_foam": h.surface.crest_foam,
            "depth_m": h.surface.depth.map(finite_or_null),
            "breaking": h.surface.breaking,
            "shore_foam": h.surface.shore_foam,
        }),
    })
}

/// `water.floaters { entity? }`: every floating body (spec 87 § 5) in entity order, or one: its
/// spheres, the fluid density and drag, and the last fixed step's buoyancy (`state`: force,
/// torque, submerged volume and fraction, wet spheres, tick; `null` before the first step or
/// without `regia_framework::buoyancy::BuoyancyPlugin`).
pub(crate) fn floaters(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let only = match params.get("entity") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id (u64 bits)"))?,
        ),
    };
    let mut query = world.query::<(Entity, &Floater, Option<&FloaterState>)>();
    let mut list: Vec<(Entity, Value)> = query
        .iter(world)
        .filter(|(e, _, _)| only.is_none_or(|o| o == *e))
        .map(|(e, f, st)| {
            let state = st.map(|st| {
                json!({
                    "tick": st.tick,
                    "force": st.forces.force,
                    "torque": st.forces.torque,
                    "submerged_volume_m3": st.forces.submerged_volume,
                    "submerged_fraction": st.forces.submerged_fraction,
                    "wet_points": st.forces.wet_points,
                })
            });
            (
                e,
                json!({
                    "entity": e.to_bits(),
                    "points": f.points.len(),
                    "volume_m3": f.volume(),
                    "fluid_density": f.fluid_density,
                    "linear_drag": f.linear_drag,
                    "angular_drag": f.angular_drag,
                    "state": state,
                }),
            )
        })
        .collect();
    if let Some(entity) = only
        && list.is_empty()
    {
        return Err(RpcError::not_found(
            format!("entity {} has no Floater", entity.to_bits()),
            "water.floaters without `entity` lists every floating body",
        ));
    }
    list.sort_by_key(|(e, _)| *e);
    Ok(json!({
        "count": list.len(),
        "floaters": list.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
    }))
}

/// The ocean bodies of `list`, or the one `entity` names (`F0003` when it has no ocean).
fn ocean_bodies(
    params: &Value,
    list: Vec<(Entity, WaterBody)>,
) -> Result<Vec<(Entity, WaterBody)>, RpcError> {
    let only = match params.get("entity") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id (u64 bits)"))?,
        ),
    };
    let found: Vec<(Entity, WaterBody)> = list
        .into_iter()
        .filter(|(e, b)| b.active_ocean().is_some() && only.is_none_or(|o| o == *e))
        .collect();
    match only {
        Some(entity) if found.is_empty() => Err(RpcError::not_found(
            format!("entity {} has no FFT ocean", entity.to_bits()),
            "water.ocean without `entity` lists every ocean; water.set { ocean: {} } adds one",
        )),
        _ => Ok(found),
    }
}

/// `water.ocean { entity? }`: every plane with an FFT ocean (ADR-1969), in entity order: the
/// authored fields; the sea state (significant height, peak wavelength and period, the loop); each
/// band's tile, wave numbers, modes, variance and mean square slope, on the query grid; and the
/// surface at this tick: its reach, the inverse's share of compressed texels, and what the tier
/// draws.
pub(crate) fn ocean_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let list = ocean_bodies(params, bodies(world))?;
    let clock = world.get_resource::<WaterClock>().copied().unwrap_or_default();
    let tier = world
        .get_resource::<WaterSettings>()
        .map(|s| s.effective_tier(world.get_resource::<regia_render::RenderSettings>()));
    let draw = world.get_resource::<WaterSettings>().is_none_or(|s| s.ocean_draw);
    let report = list
        .iter()
        .map(|(entity, body)| {
            let o = body.active_ocean().copied().unwrap_or_default();
            let tables = ocean::tables_for(&o, OCEAN_QUERY_SIZE);
            let frame = ocean::frame_at(&o, clock.seconds());
            let spec = serde_json::to_value(o)
                .map_err(|e| RpcError::internal(format!("ocean serialization failed: {e}")))?;
            let bands: Vec<Value> = tables
                .bands
                .iter()
                .map(|b| {
                    json!({
                        "tile_m": b.tile,
                        "k_min": b.limits[0],
                        "k_max": finite_or_null(b.limits[1] as f32),
                        "modes": b.modes.len(),
                        "variance_m2": b.variance,
                        "mean_square_slope": b.slope_variance,
                        "rms_wavelength_m": finite_or_null((std::f64::consts::TAU / b.rms_k) as f32),
                    })
                })
                .collect();
            let peak = o.peak_frequency();
            Ok(json!({
                "entity": entity.to_bits(),
                "ocean": spec,
                "wave_height_m": tables.significant_height(),
                "expected_height_m": o.expected_height(),
                "peak_wavelength_m": o.peak_wavelength(),
                "peak_period_s": std::f64::consts::TAU / peak,
                "loop_s": OCEAN_LOOP_SECONDS,
                "query_size": OCEAN_QUERY_SIZE,
                "bands": bands,
                "max_rise_m": frame.max_rise(),
                "max_shift_m": frame.max_shift(),
                "folded_share": frame.compressed_share(0.0, 64),
                "drawn_size": drawn_size(&o, draw, tier),
            }))
        })
        .collect::<Result<Vec<_>, RpcError>>()?;
    Ok(json!({
        "count": report.len(),
        "tick": clock.tick,
        "seconds": clock.seconds(),
        "tier": tier.map(WaterTier::name),
        "ocean_draw": draw,
        "oceans": report,
    }))
}

/// Texels per side of the maps `ocean` draws now (0: the swell alone).
fn drawn_size(ocean: &FftOcean, draw: bool, tier: Option<WaterTier>) -> u32 {
    match (draw, tier) {
        (true, Some(tier)) => ocean.drawn_size(tier),
        _ => 0,
    }
}

/// The water renderer (`F0003` without one).
fn water_renderer(world: &World) -> Result<regia_terrain::WaterRenderer, RpcError> {
    world
        .get_resource::<WaterState>()
        .map(|state| state.renderer.clone())
        .ok_or_else(|| {
            RpcError::not_found(
                "no water renderer in this app",
                "a render device must exist: add regia_terrain::WaterPlugin to a rendered app",
            )
        })
}

/// `water.ocean_check {}`: reads the frame's ocean maps back from the GPU and compares every
/// field of every band with the CPU mirror at the same tick and map size: the largest difference,
/// its field and band, and the largest height. Blocks until the GPU is done. `F0003` without a
/// renderer; `drawn: false` when no ocean is drawn (Low, the switch off, no ocean body).
pub(crate) fn ocean_check(world: &mut World) -> Result<Value, RpcError> {
    let clock = world.get_resource::<WaterClock>().copied().unwrap_or_default();
    let renderer = water_renderer(world)?;
    let maps = renderer
        .read_ocean_maps()
        .map_err(|e| RpcError::internal(format!("ocean readback failed: {e}")))?;
    let Some(maps) = maps else {
        return Ok(json!({ "drawn": false, "tick": clock.tick }));
    };
    // Not through the frame memo: a frame of the 1024 grid would stay in memory after the check.
    let cpu = ocean::OceanFrame::at(
        &ocean::tables_for(&maps.ocean, maps.size),
        ocean::loop_fraction(clock.seconds()),
    );
    let names = ["dx", "h", "dz", "dxz", "hx", "hz", "dxx", "dzz"];
    let mut worst = (0.0f32, 0usize, 0usize);
    let mut peak = 0.0f32;
    for (b, g) in cpu.grids.iter().enumerate() {
        let fields = [&g.dx, &g.h, &g.dz, &g.dxz, &g.hx, &g.hz, &g.dxx, &g.dzz];
        for row in 0..maps.size {
            for column in 0..maps.size {
                let i = (row * maps.size + column) as usize;
                let (t0, t1) = (maps.texel(b, 0, column, row), maps.texel(b, 1, column, row));
                let gpu = t0.into_iter().chain(t1);
                for (f, (field, g)) in fields.iter().zip(gpu).enumerate() {
                    let e = (field[i] - g).abs();
                    if e > worst.0 {
                        worst = (e, f, b);
                    }
                }
                peak = peak.max(t0[1].abs());
            }
        }
    }
    Ok(json!({
        "drawn": true,
        "tick": clock.tick,
        "size": maps.size,
        "worst_error": worst.0,
        "worst_field": names[worst.1],
        "worst_band": worst.2,
        "peak_height_m": peak,
    }))
}

/// `water.ocean_cost { grids?, frames?, entity? }`: times the compute pass at each grid on the
/// device in use, on a simulation of its own (the frame's maps are not disturbed): GPU time by
/// timestamps where the device has them, wall time from submit to done, modes and memory. The
/// ocean is `entity`'s, else the first body's, else the default. `F0003` without a renderer.
/// Blocks until the GPU is done, for every frame. No time here is a promise.
pub(crate) fn ocean_cost(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let grids: Vec<u32> = match params.get("grids") {
        None => COST_GRIDS.to_vec(),
        Some(Value::Array(items)) if !items.is_empty() && items.len() <= OCEAN_GRIDS.len() => items
            .iter()
            .map(|item| {
                item.as_u64()
                    .and_then(|g| u32::try_from(g).ok())
                    .filter(|g| OCEAN_GRIDS.contains(g))
                    .ok_or_else(|| {
                        RpcError::invalid_params(format!("`grids[]` must be one of {OCEAN_GRIDS:?}"))
                    })
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(RpcError::invalid_params(format!(
                "`grids` must be an array of 1 to {} of {OCEAN_GRIDS:?}",
                OCEAN_GRIDS.len()
            )));
        }
    };
    let frames = match params.get("frames") {
        None => COST_FRAMES.0,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=COST_FRAMES.1).contains(n))
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "`frames` must be a whole number in 1..={}",
                    COST_FRAMES.1
                ))
            })?,
    } as u32;
    let renderer = water_renderer(world)?;
    let list = ocean_bodies(params, bodies(world))?;
    let spec = list
        .first()
        .and_then(|(_, body)| body.active_ocean().copied())
        .unwrap_or_default();
    let costs = grids
        .iter()
        .map(|grid| {
            // A grid the device's limits refuse is named, not just skipped (ADR-3251).
            if let Err(refused) = renderer.check_ocean_grid(&spec, *grid) {
                return Ok(json!({
                    "size": grid,
                    "ran": false,
                    "refused": refused,
                    "error": refused.to_string(),
                }));
            }
            let cost = renderer
                .measure_ocean_cost(&spec, *grid, frames)
                .map_err(|e| RpcError::internal(format!("ocean cost measurement failed: {e}")))?;
            let Some(cost) = cost else {
                return Ok(json!({ "size": grid, "ran": false }));
            };
            let mut v = serde_json::to_value(cost)
                .map_err(|e| RpcError::internal(format!("ocean cost serialization failed: {e}")))?;
            v["ran"] = json!(true);
            Ok(v)
        })
        .collect::<Result<Vec<_>, RpcError>>()?;
    Ok(json!({
        "ocean": ocean_json_of(&spec),
        "frames": frames,
        "costs": costs,
    }))
}

/// A number field of an object, inside `min..=max`, or `default` when absent.
fn number_in(
    obj: &serde_json::Map<String, Value>,
    name: &str,
    min: f64,
    max: f64,
    default: Option<f64>,
) -> Result<f64, RpcError> {
    match obj.get(name) {
        None => {
            default.ok_or_else(|| RpcError::invalid_params(format!("`swell.open_sea.{name}` is required")))
        }
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite() && *x >= min && *x <= max)
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "`swell.open_sea.{name}` must be a number in {min}..={max}"
                ))
            }),
    }
}

/// The swell a `water.set { swell }` patch gives: `{ open_sea: { direction, height, wavelength,
/// count } }` starts from [`Swell::open_sea`], otherwise from the body's swell (or the default);
/// the other fields patch over that.
fn swell_from_patch(current: Option<&Swell>, patch: &Value) -> Result<Swell, RpcError> {
    let Value::Object(fields) = patch else {
        return Err(RpcError::invalid_params("`swell` must be an object, or false"));
    };
    let mut rest = fields.clone();
    let base = match rest.remove("open_sea") {
        None => current.cloned().unwrap_or_default(),
        Some(Value::Object(sea)) => {
            let direction = match sea.get("direction") {
                None => [1.0, 0.0],
                Some(v) => v
                    .as_array()
                    .filter(|a| a.len() == 2)
                    .and_then(|a| Some([a[0].as_f64()? as f32, a[1].as_f64()? as f32]))
                    .filter(|d| d.iter().all(|c| c.is_finite()) && d[0] * d[0] + d[1] * d[1] > 1.0e-12)
                    .ok_or_else(|| {
                        RpcError::invalid_params("`swell.open_sea.direction` must be a non-zero [x, z]")
                    })?,
            };
            let height = number_in(&sea, "height", 0.0, 40.0, None)? as f32;
            let wavelength = number_in(&sea, "wavelength", 0.25, 2000.0, None)? as f32;
            // A whole number: `as usize` truncated `count: 1.9` to one wave and still answered
            // success (audit-seq Low).
            let count = number_in(&sea, "count", 1.0, 32.0, Some(16.0))?;
            if count.fract() != 0.0 {
                return Err(RpcError::invalid_params(
                    "`swell.open_sea.count` must be a whole number in 1..=32",
                ));
            }
            let count = count as usize;
            Swell::open_sea(direction, height, wavelength, count)
        }
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`swell.open_sea` must be { direction, height, wavelength, count }",
            ));
        }
    };
    patched::<Swell>(&base, &Value::Object(rest), "swell")
}

fn bool_param(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    params
        .get(name)
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a boolean")))
        })
        .transpose()
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    params
        .get(name)
        .map(|v| {
            v.as_str()
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a string")))
        })
        .transpose()
}

/// `patch` (an object of known fields) over `base`, validated by deserializing.
fn patched<T: serde::Serialize + serde::de::DeserializeOwned>(
    base: &T,
    patch: &Value,
    what: &str,
) -> Result<T, RpcError> {
    let mut merged = serde_json::to_value(base)
        .map_err(|e| RpcError::internal(format!("{what} serialization failed: {e}")))?;
    let (Value::Object(patch), Value::Object(target)) = (patch, &mut merged) else {
        return Err(RpcError::invalid_params(format!("`{what}` must be an object")));
    };
    for (k, v) in patch {
        if !target.contains_key(k) {
            return Err(RpcError::invalid_params(format!(
                "unknown {what} field `{k}` (known: {})",
                target.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }
        target.insert(k.clone(), v.clone());
    }
    serde_json::from_value(merged).map_err(|e| RpcError::invalid_params(format!("invalid {what}: {e}")))
}

/// `entity` of a body edit (`None` when absent).
fn entity_param(params: &Value) -> Result<Option<Entity>, RpcError> {
    params
        .get("entity")
        .map(|v| {
            v.as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id (u64 bits)"))
        })
        .transpose()
}

/// The body `entity` names, or the first one (`F0003` when there is none).
fn body_to_edit(world: &mut World, entity: Option<Entity>) -> Result<(Entity, WaterBody), RpcError> {
    let list = bodies(world);
    match entity {
        Some(entity) => list.into_iter().find(|(e, _)| *e == entity).ok_or_else(|| {
            RpcError::not_found(
                format!("entity {} has no WaterBody", entity.to_bits()),
                "water.state lists the bodies and their entities",
            )
        }),
        None => list.into_iter().next().ok_or_else(|| {
            RpcError::not_found(
                "no WaterBody in the world",
                "spawn an entity with regia_terrain::WaterBody",
            )
        }),
    }
}

/// `water.set_mode { mode, entity?, grid?, ocean? }`: switches one plane (`entity`, default the
/// first body) between its trochoidal swell and its FFT ocean, keeping both setups (ADR-2126).
/// `fft` brings a parked ocean back, or the default one, with `ocean` (a field patch) and `grid`
/// over it. `swell` parks the ocean, and queries and drawing go back to the swell. Validated before
/// anything changes. Returns the mode before, the mode now and the map size that draws.
pub(crate) fn set_mode(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !params.is_object() {
        return Err(RpcError::invalid_params("params must be an object"));
    }
    let name = str_param(params, "mode")?
        .ok_or_else(|| RpcError::invalid_params("`mode` is required: swell | fft"))?;
    let mode = WaveMode::from_name(name)
        .ok_or_else(|| RpcError::invalid_params(format!("unknown mode `{name}` (swell | fft)")))?;
    let grid = params
        .get("grid")
        .map(|v| {
            v.as_u64()
                .and_then(|g| u32::try_from(g).ok())
                .ok_or_else(|| RpcError::invalid_params("`grid` must be 0 or one of 128, 256, 512, 1024"))
        })
        .transpose()?;
    let patch = params.get("ocean");
    if mode == WaveMode::Swell && (grid.is_some() || patch.is_some()) {
        return Err(RpcError::invalid_params(
            "`grid` and `ocean` go with mode fft; a parked ocean keeps its own",
        ));
    }
    let entity = entity_param(params)?;
    let settings = world
        .get_resource::<WaterSettings>()
        .copied()
        .ok_or_else(no_plugin)?;
    let (entity, body) = body_to_edit(world, entity)?;
    let mut edited = body.clone();
    edited
        .set_wave_mode(mode)
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    if let Some(patch) = patch {
        edited.ocean = Some(patched::<FftOcean>(
            &edited.ocean.unwrap_or_default(),
            patch,
            "ocean",
        )?);
    }
    if let (Some(grid), Some(ocean)) = (grid, edited.ocean.as_mut()) {
        ocean.grid = grid;
    }
    edited
        .validate()
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    if let Some(mut slot) = world.get_mut::<WaterBody>(entity) {
        *slot = edited.clone();
    }
    let tier = settings.effective_tier(world.get_resource::<regia_render::RenderSettings>());
    Ok(json!({
        "entity": entity.to_bits(),
        "before": body.wave_mode().map(WaveMode::name),
        "mode": edited.wave_mode().map(WaveMode::name),
        "tier": tier.name(),
        "ocean_draw": settings.ocean_draw,
        "drawn_size": edited
            .active_ocean()
            .map_or(0, |o| drawn_size(o, settings.ocean_draw, Some(tier))),
        "ocean": ocean_json(&edited),
        "swell": swell_json(&edited),
        "parked_ocean": edited.parked_ocean.is_some(),
    }))
}

/// `water.set_caustics { entity?, body?, enabled?, strength?, scale?, speed?, chromatic?,
/// shaft_steps?, tier_cost?, shaft_accumulate?, shafts_from_above? }` (ADR-2432, ADR-3251): a field patch over one body's caustics (`entity`, or
/// its alias `body`, default the first body). Validated before anything changes; `F0002` on an
/// unknown or bad field, a `low` tier cost or a step count outside 0 / 8..=16; `F0003` without the
/// plugin or without the body. Returns the setting before and after, whether it draws at the
/// effective tier, and whether the tier compiles it out.
pub(crate) fn set_caustics(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !params.is_object() {
        return Err(RpcError::invalid_params("params must be an object"));
    }
    let mut patch = params.clone();
    let mut entity = None;
    if let Value::Object(map) = &mut patch {
        for key in ["entity", "body"] {
            if let Some(v) = map.remove(key) {
                entity = Some(v.as_u64().and_then(Entity::try_from_bits).ok_or_else(|| {
                    RpcError::invalid_params(format!("`{key}` must be an entity id (u64 bits)"))
                })?);
            }
        }
    }
    let settings = world
        .get_resource::<WaterSettings>()
        .copied()
        .ok_or_else(no_plugin)?;
    let (entity, body) = body_to_edit(world, entity)?;
    let before = body.look.caustics;
    let after = patched::<WaterCaustics>(&before, &patch, "caustics")?;
    let mut edited = body.clone();
    edited.look.caustics = after;
    edited
        .validate()
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    if let Some(mut slot) = world.get_mut::<WaterBody>(entity) {
        *slot = edited;
    }
    let tier = settings.effective_tier(world.get_resource::<regia_render::RenderSettings>());
    Ok(json!({
        "entity": entity.to_bits(),
        "before": before,
        "caustics": after,
        "tier": tier.name(),
        "active": after.active_at(tier),
        "compiled_out": matches!(tier, WaterTier::Low),
    }))
}

/// `water.caustics_stats { depth? }` (ADR-2432): what the caustic passes did (bodies on, tile
/// size, passes recorded, the shaft target), and, once a tile pass has run, the tile read back
/// from the GPU and summarized at `depth` metres (default 1): the mean and largest relative
/// irradiance over the tile (the mean stays near 1 by energy conservation; the largest is the
/// focusing). `F0002` on a bad depth; `F0003` without a water renderer.
pub(crate) fn caustics_stats(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let depth = match params.get("depth") {
        None => 1.0,
        Some(v) => v
            .as_f64()
            .filter(|d| d.is_finite() && (0.0..=20.0).contains(d))
            .ok_or_else(|| RpcError::invalid_params("`depth` must be a number in 0..=20 (metres)"))?
            as f32,
    };
    let renderer = water_renderer(world)?;
    let settings = world.get_resource::<WaterSettings>().copied();
    let tier = settings.map(|s| s.effective_tier(world.get_resource::<regia_render::RenderSettings>()));
    let list = bodies(world);
    let enabled = list.iter().filter(|(_, b)| b.look.caustics.enabled).count();
    let active = tier.map_or(0, |t| {
        list.iter().filter(|(_, b)| b.look.caustics.active_at(t)).count()
    });
    let first = tier.and_then(|t| list.iter().find(|(_, b)| b.look.caustics.active_at(t)));
    let stats = renderer.caustic_stats();
    let tile = renderer.read_caustic_tile().map(|texels| {
        // Depth in tile lengths of the first active body.
        let length = first
            .map_or(1.5, |(_, b)| b.look.ripple_scale * b.look.caustics.scale)
            .max(0.1);
        let (mean, max) = regia_terrain::water_body::caustics::tile_density_summary(&texels, depth / length);
        json!({ "depth": depth, "tile_length": length, "mean_density": mean, "max_density": max })
    });
    Ok(json!({
        "tier": tier.map(WaterTier::name),
        "compiled_out": tier.is_some_and(|t| matches!(t, WaterTier::Low)),
        "enabled_bodies": enabled,
        "active_bodies": active,
        "passes": stats,
        "tile": tile,
    }))
}

/// `water.set`: any of `enabled`, `tier` (auto | low | high | ultra), `debug`, `running`, `tick`,
/// `terrain_planes`; and, for one body (`entity`, default the first), `look` (a field patch),
/// `ice` (a field patch over the current or default shelf, or `false` to remove it) and `swell`
/// (a field patch over the current or default swell, `waves` replaced whole, or `false` to remove
/// it). Every change is validated before any is applied. Returns the state before and after.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !params.is_object() && !params.is_null() {
        return Err(RpcError::invalid_params("params must be an object"));
    }
    let enabled = bool_param(params, "enabled")?;
    let running = bool_param(params, "running")?;
    let terrain_planes = bool_param(params, "terrain_planes")?;
    let shore = bool_param(params, "shore")?;
    let ocean_draw = bool_param(params, "ocean_draw")?;
    let underwater = bool_param(params, "underwater")?;
    let ring_culling = bool_param(params, "ring_culling")?;
    let ring_snap = bool_param(params, "ring_snap")?;
    let queries_follow_tier = bool_param(params, "queries_follow_tier")?;
    let gpu_timing = match params.get("gpu_timing") {
        None => None,
        Some(v) => Some(v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| {
            RpcError::invalid_params("`gpu_timing` must be a whole number of frames ≥ 0 (0 stops)")
        })?),
    };
    let tier = match str_param(params, "tier")? {
        None => None,
        Some("auto") => Some(None),
        Some(name) => Some(Some(WaterTier::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!("unknown tier `{name}` (auto | low | high | ultra)"))
        })?)),
    };
    let debug = match str_param(params, "debug")? {
        None => None,
        Some(name) => Some(WaterDebug::from_name(name).ok_or_else(|| {
            let names: Vec<&str> = WaterDebug::ALL.iter().map(|d| d.name()).collect();
            RpcError::invalid_params(format!("unknown debug view `{name}` ({})", names.join(" | ")))
        })?),
    };
    let tick = match params.get("tick") {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or_else(|| RpcError::invalid_params("`tick` must be a whole number ≥ 0"))?,
        ),
    };
    let look_patch = params.get("look");
    let ice_patch = params.get("ice");
    let swell_patch = params.get("swell");
    let ocean_patch = params.get("ocean");
    let sea_patch = params.get("sea");
    let entity_param = entity_param(params)?;
    let body_edit = look_patch.is_some()
        || ice_patch.is_some()
        || swell_patch.is_some()
        || ocean_patch.is_some()
        || sea_patch.is_some();
    if entity_param.is_some() && !body_edit {
        return Err(RpcError::invalid_params(
            "`entity` selects the body `look`, `ice`, `swell`, `ocean` or `sea` edits",
        ));
    }
    if [
        enabled.is_some(),
        running.is_some(),
        tier.is_some(),
        debug.is_some(),
        tick.is_some(),
        terrain_planes.is_some(),
        shore.is_some(),
        ocean_draw.is_some(),
        underwater.is_some(),
        ring_culling.is_some(),
        ring_snap.is_some(),
        queries_follow_tier.is_some(),
        gpu_timing.is_some(),
        body_edit,
    ]
    .iter()
    .all(|set| !set)
    {
        return Err(RpcError::invalid_params(
            "pass any of enabled, tier, debug, running, tick, terrain_planes, shore, ocean_draw, underwater, ring_culling, ring_snap, queries_follow_tier, gpu_timing, look, ice, swell, ocean, sea",
        ));
    }
    let settings = world
        .get_resource::<WaterSettings>()
        .copied()
        .ok_or_else(no_plugin)?;
    let before = state(world)?;

    // Validate the body edit before applying anything.
    let mut new_body: Option<(Entity, WaterBody)> = None;
    let mut new_sea: Option<(Entity, Option<WeatherSea>)> = None;
    if body_edit {
        let (entity, body) = body_to_edit(world, entity_param)?;
        let mut edited = body.clone();
        if let Some(patch) = look_patch {
            edited.look = patched::<WaterLook>(&body.look, patch, "look")?;
        }
        match ice_patch {
            None => {}
            Some(Value::Bool(false)) => edited.ice = None,
            Some(patch) => {
                edited.ice = Some(patched::<IceShelf>(&body.ice.unwrap_or_default(), patch, "ice")?)
            }
        }
        match swell_patch {
            None => {}
            Some(Value::Bool(false)) => edited.swell = None,
            Some(patch) => edited.swell = Some(swell_from_patch(body.swell.as_ref(), patch)?),
        }
        match ocean_patch {
            None => {}
            Some(Value::Bool(false)) => edited.ocean = None,
            Some(patch) => {
                edited.ocean = Some(patched::<FftOcean>(
                    &body.ocean.unwrap_or_default(),
                    patch,
                    "ocean",
                )?)
            }
        }
        edited
            .validate()
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        match sea_patch {
            None => {}
            Some(Value::Bool(false)) => new_sea = Some((entity, None)),
            Some(patch) => {
                if !matches!(body.shape, WaterShape::Plane { .. }) {
                    return Err(RpcError::invalid_params("`sea` needs a plane body"));
                }
                let current = world.get::<WeatherSea>(entity).copied().unwrap_or_default();
                let sea = patched::<WeatherSea>(&current, patch, "sea")?;
                if !(sea.fetch.is_finite() && (MIN_FETCH..=1.0).contains(&sea.fetch)) {
                    return Err(RpcError::invalid_params(format!(
                        "`sea.fetch` must be {MIN_FETCH}..=1"
                    )));
                }
                if !(1..=MAX_SWELL_WAVES).contains(&sea.waves) {
                    return Err(RpcError::invalid_params(format!(
                        "`sea.waves` must be 1..={MAX_SWELL_WAVES}"
                    )));
                }
                new_sea = Some((entity, Some(sea)));
            }
        }
        new_body = Some((entity, edited));
    }

    let mut next = settings;
    if let Some(enabled) = enabled {
        next.enabled = enabled;
    }
    if let Some(tier) = tier {
        next.tier = tier;
    }
    if let Some(debug) = debug {
        next.debug = debug;
    }
    if let Some(terrain_planes) = terrain_planes {
        next.terrain_planes = terrain_planes;
    }
    if let Some(shore) = shore {
        next.shore = shore;
    }
    if let Some(ocean_draw) = ocean_draw {
        next.ocean_draw = ocean_draw;
    }
    if let Some(underwater) = underwater {
        next.underwater = underwater;
    }
    if let Some(ring_culling) = ring_culling {
        next.ring_culling = ring_culling;
    }
    if let Some(ring_snap) = ring_snap {
        next.ring_snap = ring_snap;
    }
    if let Some(queries_follow_tier) = queries_follow_tier {
        next.queries_follow_tier = queries_follow_tier;
    }
    if let Some(gpu_timing) = gpu_timing {
        next.gpu_timing = gpu_timing;
    }
    world.insert_resource(next);
    if running.is_some() || tick.is_some() {
        let mut clock = world.get_resource::<WaterClock>().copied().unwrap_or_default();
        if let Some(running) = running {
            clock.running = running;
        }
        if let Some(tick) = tick {
            clock.tick = tick;
        }
        world.insert_resource(clock);
    }
    if let Some((entity, body)) = new_body
        && let Some(mut slot) = world.get_mut::<WaterBody>(entity)
    {
        *slot = body;
    }
    if let Some((entity, sea)) = new_sea {
        match sea {
            Some(sea) => {
                world.entity_mut(entity).insert(sea);
            }
            None => {
                world.entity_mut(entity).remove::<WeatherSea>();
            }
        }
    }
    let after = state(world)?;
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_terrain::{RibbonPoint, WaterPlugin, WaterRenderer};

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(WaterPlugin);
        let points = (0..4)
            .map(|i| RibbonPoint {
                position: [i as f32 * 2.0, 5.0 - 0.05 * i as f32, 0.0],
                width: 1.4,
                depth: 0.12,
                speed: None,
            })
            .collect();
        app.world_mut()
            .spawn(WaterBody::ribbon(points, WaterLook::clear_stream()));
        app
    }

    #[test]
    fn state_lists_the_stream_and_the_clock() {
        let mut app = app();
        let state = state(app.world_mut()).expect("state");
        assert_eq!(state["plugin"], json!(true));
        assert_eq!(state["tier"], json!("high"));
        assert_eq!(state["bodies"][0]["shape"]["kind"], json!("ribbon"));
        assert_eq!(state["bodies"][0]["shape"]["points"], json!(4));
        let drop = state["bodies"][0]["shape"]["drop_m"].as_f64().expect("drop");
        assert!((drop - 0.15).abs() < 1.0e-4);
        assert_eq!(state["clock"]["tick"], json!(0));
    }

    #[test]
    fn probe_reports_flow_inside_and_null_outside() {
        let mut app = app();
        let out = probe(app.world_mut(), &json!({ "at": [[3.0, 0.0], [3.0, 5.0]] })).expect("probe");
        let inside = &out["probes"][0];
        assert!(inside["body"].is_u64());
        assert!(inside["speed"].as_f64().expect("speed") > 0.1);
        assert!(inside["froude"].as_f64().is_some());
        assert!(out["probes"][1]["body"].is_null());
        assert!(probe(app.world_mut(), &json!({ "at": [[f64::MAX, "x"]] })).is_err());
        let too_many: Vec<Value> = (0..65).map(|_| json!([0.0, 0.0])).collect();
        assert!(probe(app.world_mut(), &json!({ "at": too_many })).is_err());
    }

    #[test]
    fn set_switches_tier_debug_clock_and_edits_the_look() {
        let mut app = app();
        let out = set(
            app.world_mut(),
            &json!({ "tier": "low", "debug": "normal", "running": false, "tick": 600, "look": { "roughness": 0.2 }, "ice": {} }),
        )
        .expect("set");
        assert_eq!(out["after"]["tier"], json!("low"));
        assert_eq!(out["after"]["forced_tier"], json!("low"));
        assert_eq!(out["after"]["debug"], json!("normal"));
        assert_eq!(out["after"]["clock"]["tick"], json!(600));
        assert_eq!(out["after"]["clock"]["running"], json!(false));
        let rough = out["after"]["bodies"][0]["look"]["roughness"]
            .as_f64()
            .expect("roughness");
        assert!((rough - 0.2).abs() < 1.0e-6);
        assert!(out["after"]["bodies"][0]["ice"].is_object());
        // Back to auto, ice off.
        let out = set(app.world_mut(), &json!({ "tier": "auto", "ice": false })).expect("set");
        assert_eq!(out["after"]["forced_tier"], Value::Null);
        assert!(out["after"]["bodies"][0]["ice"].is_null());
        // Bad values change nothing.
        assert!(set(app.world_mut(), &json!({ "look": { "roughness": -1.0 } })).is_err());
        assert!(set(app.world_mut(), &json!({ "look": { "colour": 1.0 } })).is_err());
        assert!(set(app.world_mut(), &json!({ "tier": "max" })).is_err());
        assert!(set(app.world_mut(), &json!({})).is_err());
        let state = state(app.world_mut()).expect("state");
        let rough = state["bodies"][0]["look"]["roughness"]
            .as_f64()
            .expect("roughness");
        assert!((rough - 0.2).abs() < 1.0e-6);
    }

    fn sea_app() -> App {
        let mut app = App::new();
        app.add_plugins(WaterPlugin);
        app.world_mut().spawn(WaterBody::plane(
            [0.0, 2.0, 0.0],
            [200.0, 200.0],
            WaterLook::ocean(),
        ));
        app
    }

    /// A swell put on by `water.set` shows in `water.state`, moves the surface `water.surface`
    /// answers, and a fold-over swell is refused without changing anything.
    #[test]
    fn swell_set_state_and_surface() {
        let mut app = sea_app();
        let flat = surface(app.world_mut(), &json!({ "at": [[3.0, 4.0]] })).expect("surface");
        assert_eq!(flat["points"][0]["height"], json!(2.0));
        assert_eq!(flat["points"][0]["normal"], json!([0.0, 1.0, 0.0]));
        let out = set(
            app.world_mut(),
            &json!({ "tick": 300, "swell": { "waves": [
                { "wavelength": 30.0, "height": 1.5, "direction": [1.0, 0.0], "steepness": 0.7 },
                { "wavelength": 11.0, "height": 0.4, "direction": [0.3, 1.0] }
            ] } }),
        )
        .expect("set swell");
        let swell = &out["after"]["bodies"][0]["swell"];
        assert_eq!(swell["waves"], json!(2));
        assert_eq!(swell["active"], json!(true));
        assert!(out["before"]["bodies"][0]["swell"].is_null());
        let at: Vec<Value> = (0..8).map(|i| json!([i as f64 * 3.7, 1.0])).collect();
        let waved = surface(app.world_mut(), &json!({ "at": at })).expect("surface");
        let heights: Vec<f64> = waved["points"]
            .as_array()
            .expect("points")
            .iter()
            .map(|p| p["height"].as_f64().expect("height"))
            .collect();
        assert!(heights.iter().any(|h| (h - 2.0).abs() > 0.2), "{heights:?}");
        assert!(
            heights.iter().all(|h| (h - 2.0).abs() <= 0.95 + 1.0e-3),
            "{heights:?}"
        );
        assert!(waved["points"][0]["miss_m"].as_f64().expect("miss") < 1.0e-3);
        // Folding crests are refused; the swell stays.
        let err = set(
            app.world_mut(),
            &json!({ "swell": { "waves": [{ "wavelength": 5.0, "height": 4.0, "direction": [1.0, 0.0], "steepness": 1.0 }] } }),
        );
        assert!(err.is_err());
        let state = state(app.world_mut()).expect("state");
        assert_eq!(state["bodies"][0]["swell"]["waves"], json!(2));
        // Unknown swell fields are refused; `false` removes the swell.
        assert!(set(app.world_mut(), &json!({ "swell": { "chop": 1.0 } })).is_err());
        let off = set(app.world_mut(), &json!({ "swell": false })).expect("off");
        assert!(off["after"]["bodies"][0]["swell"].is_null());
        // `open_sea` builds a spread; other fields patch over it; bad numbers are refused.
        let sea = set(
            app.world_mut(),
            &json!({ "swell": { "open_sea": { "direction": [1.0, 0.5], "height": 2.0, "wavelength": 45.0, "count": 12 }, "crest_foam": 2.0 } }),
        )
        .expect("open sea");
        let swell = &sea["after"]["bodies"][0]["swell"];
        assert_eq!(swell["waves"], json!(12));
        assert_eq!(swell["crest_foam"], json!(2.0));
        assert!(
            set(
                app.world_mut(),
                &json!({ "swell": { "open_sea": { "height": 2.0 } } })
            )
            .is_err()
        );
        assert!(
            set(
                app.world_mut(),
                &json!({ "swell": { "open_sea": { "height": 99.0, "wavelength": 10.0 } } })
            )
            .is_err()
        );
        assert!(set(app.world_mut(), &json!({ "swell": { "open_sea": 3 } })).is_err());
        // audit-seq (Low): a fractional count was truncated (1.9 built one wave) and succeeded.
        for count in [json!(1.9), json!(12.5), json!(0.5)] {
            let error = set(
                app.world_mut(),
                &json!({ "swell": { "open_sea": { "height": 2.0, "wavelength": 45.0, "count": count } } }),
            )
            .expect_err("fractional count");
            assert!(error.message.contains("count"), "{}", error.message);
        }
        let whole = set(
            app.world_mut(),
            &json!({ "swell": { "open_sea": { "height": 2.0, "wavelength": 45.0, "count": 2.0 } } }),
        )
        .expect("2.0 is whole");
        assert_eq!(whole["after"]["bodies"][0]["swell"]["waves"], json!(2));
    }

    /// A ray from above hits the level sea at the expected distance; one pointing away misses;
    /// a zero direction is refused.
    #[test]
    fn raycast_hits_the_surface() {
        let mut app = sea_app();
        let hit = raycast(
            app.world_mut(),
            &json!({ "origin": [0.0, 12.0, 0.0], "direction": [0.0, -1.0, 0.0] }),
        )
        .expect("raycast");
        assert_eq!(hit["hit"], json!(true));
        let d = hit["distance"].as_f64().expect("distance");
        assert!((d - 10.0).abs() < 1.0e-3, "{hit}");
        assert_eq!(hit["from_below"], json!(false));
        let miss = raycast(
            app.world_mut(),
            &json!({ "origin": [0.0, 12.0, 0.0], "direction": [0.0, 1.0, 0.0] }),
        )
        .expect("raycast");
        assert_eq!(miss["hit"], json!(false));
        assert!(
            raycast(
                app.world_mut(),
                &json!({ "origin": [0.0, 1.0, 0.0], "direction": [0.0, 0.0, 0.0] })
            )
            .is_err()
        );
        assert!(raycast(app.world_mut(), &json!({ "direction": [0.0, -1.0, 0.0] })).is_err());
        assert!(
            raycast(
                app.world_mut(),
                &json!({ "origin": [0.0, 1.0, 0.0], "direction": [0.0, -1.0, 0.0], "max_distance": -1.0 })
            )
            .is_err()
        );
    }

    /// Water extras (ADR-2342): `underwater`, `ring_culling`, `ring_snap` and `gpu_timing` are off
    /// by default, show in the state and validate; `sea` adds, patches and removes the weather sea
    /// of a plane and refuses values out of range.
    #[test]
    fn extras_switch_and_sea_component() {
        let mut app = sea_app();
        let st = state(app.world_mut()).expect("state");
        assert_eq!(st["underwater"], json!(false));
        assert_eq!(st["ring_culling"], json!(false));
        assert_eq!(st["ring_snap"], json!(false));
        assert_eq!(st["gpu_timing"], json!(0));
        let out = set(
            app.world_mut(),
            &json!({ "underwater": true, "ring_culling": true, "ring_snap": true, "gpu_timing": 30 }),
        )
        .expect("set");
        assert_eq!(out["after"]["underwater"], json!(true));
        assert_eq!(out["after"]["ring_culling"], json!(true));
        assert_eq!(out["after"]["ring_snap"], json!(true));
        assert_eq!(out["after"]["gpu_timing"], json!(30));
        assert!(set(app.world_mut(), &json!({ "gpu_timing": -1 })).is_err());
        assert!(set(app.world_mut(), &json!({ "underwater": 3 })).is_err());
        assert!(set(app.world_mut(), &json!({ "ring_snap": "yes" })).is_err());
        assert!(state(app.world_mut()).expect("state")["bodies"][0]["sea"].is_null());
        let out = set(app.world_mut(), &json!({ "sea": { "fetch": 0.5, "waves": 6 } })).expect("sea");
        let sea = &out["after"]["bodies"][0]["sea"];
        assert_eq!(sea["fetch"], json!(0.5));
        assert_eq!(sea["waves"], json!(6));
        assert_eq!(sea["drives"], json!("swell"));
        assert!(
            sea["wavelength_m"].is_number() && sea["force"].is_number(),
            "{sea}"
        );
        // A patch keeps what it does not name.
        let out = set(app.world_mut(), &json!({ "sea": { "waves": 9 } })).expect("sea");
        assert_eq!(out["after"]["bodies"][0]["sea"]["fetch"], json!(0.5));
        assert_eq!(out["after"]["bodies"][0]["sea"]["waves"], json!(9));
        assert!(set(app.world_mut(), &json!({ "sea": { "fetch": 2.0 } })).is_err());
        assert!(set(app.world_mut(), &json!({ "sea": { "waves": 0 } })).is_err());
        assert!(set(app.world_mut(), &json!({ "sea": { "waves": 99 } })).is_err());
        assert!(set(app.world_mut(), &json!({ "sea": { "bogus": 1 } })).is_err());
        let out = set(app.world_mut(), &json!({ "sea": false })).expect("sea off");
        assert!(out["after"]["bodies"][0]["sea"].is_null());
    }

    /// `terrain_planes` toggles the setting and shows in the state.
    #[test]
    fn terrain_planes_switch() {
        let mut app = sea_app();
        assert_eq!(
            state(app.world_mut()).expect("state")["terrain_planes"],
            json!(true)
        );
        let out = set(app.world_mut(), &json!({ "terrain_planes": false })).expect("set");
        assert_eq!(out["after"]["terrain_planes"], json!(false));
        assert!(out["after"]["bodies"][0]["terrain"].is_null());
    }

    /// A sea with a baked beach (spec 87): `water.state` reports the field, `water.surface` is
    /// dry on the beach and reports depth and shoreline distance offshore, `water.set { shore:
    /// false }` makes the beach answer the sea again, a lake above the sea wins over it.
    #[test]
    fn shore_field_dries_land_and_switches() {
        use regia_terrain::water_body::{ShoreBakeDesc, ShoreField, WaterShore};
        let mut app = sea_app();
        let sea = app
            .world_mut()
            .query_filtered::<Entity, With<WaterBody>>()
            .iter(app.world())
            .next()
            .expect("sea");
        // Bed rising toward +x: the shoreline at x = 50.
        let field = ShoreField::bake(
            &ShoreBakeDesc {
                level: 2.0,
                min: [-200.0, -200.0],
                max: [200.0, 200.0],
                cell: 2.0,
            },
            |x, _| 2.0 + (x - 50.0) / 10.0,
        )
        .expect("bake");
        app.world_mut().entity_mut(sea).insert(WaterShore {
            field: Arc::new(field),
        });
        app.world_mut().spawn(WaterBody::plane(
            [100.0, 9.0, 0.0],
            [10.0, 10.0],
            WaterLook::lake(),
        ));
        let state = state(app.world_mut()).expect("state");
        assert_eq!(state["shore"], json!(true));
        // Bodies are listed in entity order, which is not spawn order: find the sea by its id.
        let sea_state = state["bodies"]
            .as_array()
            .and_then(|b| b.iter().find(|b| b["entity"] == json!(sea.to_bits())))
            .expect("sea listed");
        assert!(sea_state["shore"]["size"].is_array(), "{state}");
        // Spec 87 § 6: the whole shore object. 400 m at 2 m: 201 texels a side; wet where
        // x < 50 (125 of 201 columns); deepest at x = -200: 2 - (2 - 25) = 25 m.
        let shore = &sea_state["shore"];
        assert_eq!(shore["size"], json!([201, 201]), "{shore}");
        assert_eq!(shore["cell_m"], json!(2.0), "{shore}");
        assert_eq!(shore["level"], json!(2.0), "{shore}");
        let wet_fraction = shore["wet_fraction"].as_f64().expect("wet_fraction");
        assert!((wet_fraction - 125.0 / 201.0).abs() < 1e-3, "{shore}");
        let deepest = shore["max_depth_m"].as_f64().expect("max_depth_m");
        assert!((deepest - 25.0).abs() < 1e-3, "{shore}");
        assert!(
            shore["shoreline_texels"].as_u64().is_some_and(|n| n > 0),
            "{shore}"
        );
        let out = surface(
            app.world_mut(),
            &json!({ "at": [[0.0, 0.0], [80.0, 0.0], [100.0, 0.0]] }),
        )
        .expect("surface");
        let sea_pt = &out["points"][0];
        assert_eq!(sea_pt["body"], json!(sea.to_bits()));
        let depth = sea_pt["depth_m"].as_f64().expect("depth");
        assert!((depth - 5.0).abs() < 0.1, "{sea_pt}");
        let shore_m = sea_pt["shore_m"].as_f64().expect("shore");
        assert!((shore_m - 50.0).abs() < 2.0, "{sea_pt}");
        // Breaker and shore foam are numbers in 0..=1; 50 m offshore is past the foam band.
        for key in ["breaking", "shore_foam"] {
            let v = sea_pt[key].as_f64().expect(key);
            assert!((0.0..=1.0).contains(&v), "{key}: {sea_pt}");
        }
        assert_eq!(sea_pt["shore_foam"], json!(0.0), "{sea_pt}");
        // Spec 87 § 6 puts shoreline distance on `water.surface` only. The hit still carries
        // the field's depth.
        let wet = raycast(
            app.world_mut(),
            &json!({ "origin": [0.0, 20.0, 0.0], "direction": [0.0, -1.0, 0.0] }),
        )
        .expect("raycast");
        assert_eq!(wet["hit"], json!(true), "{wet}");
        assert_eq!(wet["body"], json!(sea.to_bits()), "{wet}");
        let ray_depth = wet["depth_m"].as_f64().expect("ray depth");
        assert!((ray_depth - depth).abs() < 1.0, "{wet}");
        assert!(wet.get("shore_m").is_none(), "raycast omits shore_m: {wet}");
        for key in ["breaking", "shore_foam"] {
            let v = wet[key].as_f64().expect(key);
            assert!((0.0..=1.0).contains(&v), "{key}: {wet}");
        }
        assert!(
            out["points"][1]["body"].is_null(),
            "dry beach: {}",
            out["points"][1]
        );
        assert_ne!(out["points"][2]["body"], json!(sea.to_bits()), "the lake wins");
        assert_eq!(out["points"][2]["height"], json!(9.0));
        let off = set(app.world_mut(), &json!({ "shore": false })).expect("set");
        assert_eq!(off["after"]["shore"], json!(false));
        let out = surface(app.world_mut(), &json!({ "at": [[80.0, 0.0]] })).expect("surface");
        assert_eq!(out["points"][0]["body"], json!(sea.to_bits()));
        assert!(out["points"][0]["depth_m"].is_null());
        // A ray onto the beach misses with the field and hits the sea without it.
        let hit = raycast(
            app.world_mut(),
            &json!({ "origin": [80.0, 20.0, 0.0], "direction": [0.0, -1.0, 0.0] }),
        )
        .expect("raycast");
        assert_eq!(hit["hit"], json!(true));
        set(app.world_mut(), &json!({ "shore": true })).expect("set");
        let miss = raycast(
            app.world_mut(),
            &json!({ "origin": [80.0, 20.0, 0.0], "direction": [0.0, -1.0, 0.0] }),
        )
        .expect("raycast");
        assert_eq!(miss["hit"], json!(false));
    }

    /// `water.floaters` lists floaters with their state, filters by entity and refuses an entity
    /// without a `Floater`.
    #[test]
    fn floaters_are_listed() {
        let mut app = sea_app();
        let none = floaters(app.world_mut(), &json!({})).expect("floaters");
        assert_eq!(none["count"], json!(0));
        let ball = app.world_mut().spawn(Floater::sphere(0.5)).id();
        let crate_box = app
            .world_mut()
            .spawn((
                Floater::box_points([0.5, 0.5, 0.5], [2, 2, 2]),
                FloaterState {
                    tick: 7,
                    ..FloaterState::default()
                },
            ))
            .id();
        let all = floaters(app.world_mut(), &json!({})).expect("floaters");
        assert_eq!(all["count"], json!(2));
        // Listed in entity order, which is not spawn order: look each one up by its id.
        let listed = |e: Entity| {
            all["floaters"]
                .as_array()
                .and_then(|f| f.iter().find(|f| f["entity"] == json!(e.to_bits())))
                .cloned()
                .expect("listed")
        };
        assert!(listed(ball)["state"].is_null());
        assert_eq!(listed(ball)["points"], json!(1));
        assert_eq!(listed(crate_box)["points"], json!(8));
        assert_eq!(listed(crate_box)["state"]["tick"], json!(7));
        let ids: Vec<u64> = all["floaters"]
            .as_array()
            .map(|f| f.iter().filter_map(|f| f["entity"].as_u64()).collect())
            .unwrap_or_default();
        assert!(ids.is_sorted(), "entity order: {ids:?}");
        let one = floaters(app.world_mut(), &json!({ "entity": crate_box.to_bits() })).expect("one");
        assert_eq!(one["count"], json!(1));
        let empty = app.world_mut().spawn_empty().id();
        let err = floaters(app.world_mut(), &json!({ "entity": empty.to_bits() })).expect_err("no floater");
        let code = err
            .data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str);
        assert_eq!(code, Some("F0003"));
        assert!(floaters(app.world_mut(), &json!({ "entity": "x" })).is_err());
    }

    /// An ocean put on by `water.set` shows in `water.state` and `water.ocean`, moves the surface
    /// `water.surface` answers on the same clock, a bad field or a ribbon is refused without
    /// changing anything, and `false` removes it.
    #[test]
    fn ocean_set_state_report_and_surface() {
        let mut app = sea_app();
        let sea = app
            .world_mut()
            .query_filtered::<Entity, With<WaterBody>>()
            .iter(app.world())
            .next()
            .expect("sea");
        let none = ocean_report(app.world_mut(), &json!({})).expect("report");
        assert_eq!(none["count"], json!(0));
        let out = set(
            app.world_mut(),
            &json!({ "tick": 600, "ocean": { "wind_speed": 15.0, "seed": 3 } }),
        )
        .expect("set ocean");
        assert!(out["before"]["bodies"][0]["ocean"].is_null());
        let state = &out["after"]["bodies"][0]["ocean"];
        assert_eq!(state["seed"], json!(3));
        assert_eq!(state["spectrum"], json!("jonswap"));
        assert_eq!(out["after"]["ocean_draw"], json!(true));
        let report = ocean_report(app.world_mut(), &json!({ "entity": sea.to_bits() })).expect("report");
        assert_eq!(report["count"], json!(1));
        let one = &report["oceans"][0];
        assert_eq!(one["bands"].as_array().map(Vec::len), Some(3));
        assert_eq!(one["query_size"], json!(128));
        assert_eq!(one["ocean"]["wind_speed"], json!(15.0));
        let hs = one["wave_height_m"].as_f64().expect("height");
        assert!(hs > 1.0 && hs < 8.0, "{one}");
        assert!(one["max_rise_m"].as_f64().expect("rise") > 0.5, "{one}");
        assert!(one["peak_period_s"].as_f64().expect("period") > 3.0, "{one}");
        assert_eq!(one["drawn_size"], json!(128), "the default tier is High");
        // `water.surface` answers the mirror, deterministically.
        let at = json!({ "at": [[3.0, 4.0], [40.0, -11.0]] });
        let a = surface(app.world_mut(), &at).expect("surface");
        let b = surface(app.world_mut(), &at).expect("surface");
        assert_eq!(a, b);
        let height = a["points"][0]["height"].as_f64().expect("height");
        assert!((height - 2.0).abs() > 1.0e-3 && (height - 2.0).abs() < 8.0, "{a}");
        let ray = raycast(
            app.world_mut(),
            &json!({ "origin": [3.0, 20.0, 4.0], "direction": [0.0, -1.0, 0.0] }),
        )
        .expect("raycast");
        assert_eq!(ray["hit"], json!(true));
        assert!(
            (ray["point"][1].as_f64().expect("y") - height).abs() < 0.02,
            "{ray}"
        );
        // Refused edits change nothing.
        assert!(set(app.world_mut(), &json!({ "ocean": { "wind_speed": 400.0 } })).is_err());
        assert!(set(app.world_mut(), &json!({ "ocean": { "waves": 3 } })).is_err());
        assert!(
            set(
                app.world_mut(),
                &json!({ "ocean": { "tiles": [10.0, 10.0, 10.0] } })
            )
            .is_err()
        );
        let state_now = state_fn(&mut app);
        assert_eq!(state_now["bodies"][0]["ocean"]["seed"], json!(3));
        // The drawing switch leaves queries alone.
        let off = set(app.world_mut(), &json!({ "ocean_draw": false })).expect("off");
        assert_eq!(off["after"]["ocean_draw"], json!(false));
        assert_eq!(surface(app.world_mut(), &at).expect("surface"), a);
        let report = ocean_report(app.world_mut(), &json!({})).expect("report");
        assert_eq!(report["oceans"][0]["drawn_size"], json!(0));
        // No renderer in this app.
        let err = ocean_check(app.world_mut()).expect_err("no renderer");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("F0003")
        );
        let gone = set(app.world_mut(), &json!({ "ocean": false })).expect("off");
        assert!(gone["after"]["bodies"][0]["ocean"].is_null());
        let err = ocean_report(app.world_mut(), &json!({ "entity": sea.to_bits() })).expect_err("gone");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("F0003")
        );
        assert!(ocean_report(app.world_mut(), &json!({ "entity": "x" })).is_err());
    }

    /// `water.ocean_check` on a real GPU (lavapipe in CI): the compute pass agrees with the CPU
    /// mirror on High and Ultra, a body's own spectrum included, and Low draws nothing to compare.
    #[test]
    fn ocean_check_compares_the_gpu_with_the_mirror() {
        let Ok(device) = crate::seeing::headless_render_device() else {
            eprintln!("skipping: no GPU adapter (lavapipe) available");
            return;
        };
        let mut app = sea_app();
        let plain = WaterBody::plane([0.0, 2.0, 0.0], [200.0, 200.0], WaterLook::ocean());
        let mut sea = plain.clone();
        sea.ocean = Some(FftOcean {
            seed: 11,
            wind_speed: 16.0,
            ..FftOcean::default()
        });
        let renderer = WaterRenderer::new(&device);
        renderer.set_bodies(&[&sea]);
        renderer.set_clock(WaterClock {
            tick: 900,
            ..WaterClock::default()
        });
        app.world_mut().insert_resource(WaterClock {
            tick: 900,
            ..WaterClock::default()
        });
        app.world_mut().insert_resource(WaterState {
            renderer: renderer.clone(),
            tier: WaterTier::High,
        });
        for (tier, size) in [(WaterTier::High, 128), (WaterTier::Ultra, 256)] {
            renderer.set_tier(tier);
            let got = ocean_check(app.world_mut()).expect("check");
            assert_eq!(got["drawn"], json!(true), "{got}");
            assert_eq!(got["size"], json!(size), "{got}");
            assert_eq!(got["tick"], json!(900), "{got}");
            assert!(got["worst_error"].as_f64().expect("error") < 1.0e-3, "{got}");
            assert!(got["peak_height_m"].as_f64().expect("peak") > 0.2, "{got}");
        }
        renderer.set_tier(WaterTier::Low);
        let low = ocean_check(app.world_mut()).expect("check");
        assert_eq!(low, json!({ "drawn": false, "tick": 900 }));
        renderer.set_tier(WaterTier::High);
        renderer.set_bodies(&[&plain]);
        let none = ocean_check(app.world_mut()).expect("check");
        assert_eq!(none["drawn"], json!(false), "{none}");
    }

    fn code(err: &RpcError) -> Option<&str> {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
    }

    /// `water.set_mode` switches a plane to the ocean and back: the surface goes with it, the swell
    /// comes back to the same bits, and the ocean comes back as it was tuned.
    #[test]
    fn set_mode_switches_a_plane_and_keeps_both_setups() {
        let mut app = sea_app();
        set(
            app.world_mut(),
            &json!({ "tick": 600, "swell": { "open_sea": { "height": 1.2, "wavelength": 24.0, "count": 8 } } }),
        )
        .expect("swell");
        let at = json!({ "at": [[3.0, 4.0], [40.0, -11.0]] });
        let swell_answer = surface(app.world_mut(), &at).expect("surface");
        let start = state_fn(&mut app);
        assert_eq!(start["bodies"][0]["mode"], json!("swell"), "off by default");
        assert_eq!(start["bodies"][0]["parked_ocean"], json!(false));
        assert!(start["bodies"][0]["ocean"].is_null());

        let on = set_mode(
            app.world_mut(),
            &json!({ "mode": "fft", "grid": 512, "ocean": { "seed": 5, "wind_speed": 18.0 } }),
        )
        .expect("fft");
        assert_eq!(on["before"], json!("swell"), "{on}");
        assert_eq!(on["mode"], json!("fft"), "{on}");
        assert_eq!(on["drawn_size"], json!(512), "{on}");
        assert_eq!(on["ocean"]["seed"], json!(5), "{on}");
        assert_eq!(on["ocean"]["grid"], json!(512), "{on}");
        let ocean_answer = surface(app.world_mut(), &at).expect("surface");
        assert_ne!(
            ocean_answer["points"], swell_answer["points"],
            "the ocean answers now"
        );
        let report = ocean_report(app.world_mut(), &json!({})).expect("report");
        assert_eq!(report["oceans"][0]["drawn_size"], json!(512), "{report}");
        assert_eq!(report["oceans"][0]["ocean"]["grid"], json!(512), "{report}");
        assert_eq!(report["oceans"][0]["query_size"], json!(128), "{report}");

        let off = set_mode(app.world_mut(), &json!({ "mode": "swell" })).expect("swell");
        assert_eq!(off["before"], json!("fft"), "{off}");
        assert_eq!(off["mode"], json!("swell"), "{off}");
        assert_eq!(off["drawn_size"], json!(0), "{off}");
        assert_eq!(off["parked_ocean"], json!(true), "{off}");
        assert_eq!(
            surface(app.world_mut(), &at).expect("surface")["points"],
            swell_answer["points"],
            "the swell answers exactly as before"
        );

        let back = set_mode(app.world_mut(), &json!({ "mode": "fft" })).expect("fft");
        assert_eq!(
            back["ocean"]["seed"],
            json!(5),
            "the tuned ocean came back: {back}"
        );
        assert_eq!(back["ocean"]["grid"], json!(512), "{back}");
        assert_eq!(back["parked_ocean"], json!(false), "{back}");
        assert_eq!(
            surface(app.world_mut(), &at).expect("surface")["points"],
            ocean_answer["points"]
        );

        // Grid 0 hands the choice back to the tier, and the tier gates the drawing.
        let auto = set_mode(app.world_mut(), &json!({ "mode": "fft", "grid": 0 })).expect("auto");
        assert_eq!(auto["drawn_size"], json!(128), "{auto}");
        set(app.world_mut(), &json!({ "tier": "ultra" })).expect("ultra");
        let ultra = set_mode(app.world_mut(), &json!({ "mode": "fft" })).expect("fft");
        assert_eq!(ultra["drawn_size"], json!(256), "{ultra}");
        set(app.world_mut(), &json!({ "tier": "low" })).expect("low");
        let low = set_mode(app.world_mut(), &json!({ "mode": "fft", "grid": 1024 })).expect("fft");
        assert_eq!(low["drawn_size"], json!(0), "Low never computes: {low}");
        assert_eq!(low["ocean"]["grid"], json!(1024), "{low}");
        set(app.world_mut(), &json!({ "tier": "auto", "ocean_draw": false })).expect("switch");
        let unswitched = set_mode(app.world_mut(), &json!({ "mode": "fft" })).expect("fft");
        assert_eq!(unswitched["drawn_size"], json!(0), "{unswitched}");
    }

    /// `water.set_caustics` patches one body's caustics, refuses bad values with `F0002` and changes
    /// nothing, says when the tier compiles the pass out, and leaves the default off.
    #[test]
    fn set_caustics_patches_validates_and_defaults_off() {
        let mut app = sea_app();
        let start = state_fn(&mut app);
        assert!(
            start["bodies"][0]["look"].get("caustics").is_none(),
            "the default look carries no caustics: {start}"
        );
        set(app.world_mut(), &json!({ "tier": "high" })).expect("high");
        let on = set_caustics(
            app.world_mut(),
            &json!({ "enabled": true, "strength": 2.5, "shaft_steps": 12, "tier_cost": "ultra" }),
        )
        .expect("set");
        assert_eq!(on["before"]["enabled"], json!(false), "{on}");
        assert_eq!(on["caustics"]["strength"], json!(2.5), "{on}");
        assert_eq!(on["caustics"]["tier_cost"], json!("ultra"), "{on}");
        assert_eq!(
            on["active"],
            json!(false),
            "ultra-only is not drawn on high: {on}"
        );
        let ultra = set(app.world_mut(), &json!({ "tier": "ultra" }));
        assert!(ultra.is_ok());
        let body_bits = bodies(app.world_mut())[0].0.to_bits();
        let again = set_caustics(app.world_mut(), &json!({ "body": body_bits, "scale": 2.0 }))
            .expect("alias and a second patch");
        assert_eq!(again["caustics"]["scale"], json!(2.0), "{again}");
        assert_eq!(
            again["caustics"]["strength"],
            json!(2.5),
            "a patch keeps the other fields: {again}"
        );
        assert_eq!(again["active"], json!(true), "{again}");
        assert_eq!(again["compiled_out"], json!(false));
        set(app.world_mut(), &json!({ "tier": "low" })).expect("low");
        let low = set_caustics(app.world_mut(), &json!({})).expect("empty patch");
        assert_eq!(low["active"], json!(false), "{low}");
        assert_eq!(
            low["compiled_out"],
            json!(true),
            "Low compiles the pass out: {low}"
        );
        let after = bodies(app.world_mut())[0].1.look.caustics;
        for (bad, why) in [
            (json!({ "strength": 9.0 }), "strength"),
            (json!({ "shaft_steps": 5 }), "steps"),
            (json!({ "tier_cost": "low" }), "tier"),
            (json!({ "chromatic": -1.0 }), "chromatic"),
            (json!({ "colour": 1 }), "unknown field"),
            (json!({ "enabled": "yes" }), "type"),
            (json!({ "entity": "x" }), "entity"),
        ] {
            let err = set_caustics(app.world_mut(), &bad).expect_err(why);
            assert_eq!(code(&err), Some("F0002"), "{why}: {err:?}");
        }
        assert_eq!(
            bodies(app.world_mut())[0].1.look.caustics,
            after,
            "a refused call changed nothing"
        );
        let ghost = app.world_mut().spawn_empty().id();
        let err =
            set_caustics(app.world_mut(), &json!({ "entity": ghost.to_bits() })).expect_err("no such body");
        assert_eq!(code(&err), Some("F0003"), "{err:?}");
        let mut bare = App::new();
        let err = set_caustics(bare.world_mut(), &json!({})).expect_err("no plugin");
        assert_eq!(code(&err), Some("F0003"), "{err:?}");
    }

    /// `water.caustics_stats` refuses a bad depth, needs a renderer, and reports no tile and no
    /// passes before one has run.
    #[test]
    fn caustics_stats_reports_nothing_before_a_pass() {
        let mut app = sea_app();
        let err = caustics_stats(app.world_mut(), &json!({ "depth": -1.0 })).expect_err("bad depth");
        assert_eq!(code(&err), Some("F0002"), "{err:?}");
        let err = caustics_stats(app.world_mut(), &json!({})).expect_err("no renderer");
        assert_eq!(code(&err), Some("F0003"), "{err:?}");
        let Ok(device) = crate::seeing::headless_render_device() else {
            eprintln!("skipping: no GPU adapter (lavapipe) available");
            return;
        };
        let renderer = WaterRenderer::new(&device);
        app.world_mut().insert_resource(WaterState {
            renderer: renderer.clone(),
            tier: WaterTier::High,
        });
        set_caustics(app.world_mut(), &json!({ "enabled": true })).expect("on");
        let stats = caustics_stats(app.world_mut(), &json!({ "depth": 2.0 })).expect("stats");
        assert_eq!(stats["enabled_bodies"], json!(1), "{stats}");
        assert_eq!(stats["passes"]["tile_passes"], json!(0), "{stats}");
        assert!(stats["tile"].is_null(), "{stats}");
        // ADR-3251: the occluder, accumulation and view-from-above fields, all idle.
        for (key, idle) in [
            ("shadow_bound", json!(false)),
            ("from_above", json!(false)),
            ("resolves", json!(0)),
            ("history_resets", json!(0)),
            ("history_frames", json!(0)),
        ] {
            assert_eq!(stats["passes"][key], idle, "{key}: {stats}");
        }
        let out = set_caustics(
            app.world_mut(),
            &json!({ "shaft_accumulate": true, "shafts_from_above": true }),
        )
        .expect("patch");
        assert_eq!(out["before"]["shaft_accumulate"], json!(false), "{out}");
        assert_eq!(out["caustics"]["shaft_accumulate"], json!(true), "{out}");
        assert_eq!(out["caustics"]["shafts_from_above"], json!(true), "{out}");
    }

    /// Bad calls are refused with their code and change nothing.
    #[test]
    fn set_mode_refuses_bad_calls() {
        let mut app = sea_app();
        let before = state_fn(&mut app);
        for bad in [
            json!({}),
            json!(null),
            json!({ "mode": 3 }),
            json!({ "mode": "gerstner" }),
            json!({ "mode": "fft", "grid": 100 }),
            json!({ "mode": "fft", "grid": 2048 }),
            json!({ "mode": "fft", "grid": -1 }),
            json!({ "mode": "fft", "grid": "large" }),
            json!({ "mode": "fft", "ocean": { "wind_speed": 400.0 } }),
            json!({ "mode": "fft", "ocean": { "waves": 3 } }),
            json!({ "mode": "fft", "ocean": 3 }),
            json!({ "mode": "swell", "grid": 512 }),
            json!({ "mode": "swell", "ocean": {} }),
            json!({ "mode": "fft", "entity": "x" }),
        ] {
            let err = set_mode(app.world_mut(), &bad).expect_err("refused");
            assert_eq!(code(&err), Some("F0002"), "{bad}: {err:?}");
        }
        assert_eq!(state_fn(&mut app), before, "nothing changed");
        // Asking for an entity that is no body, a world without bodies, an app without the plugin.
        let gone = app.world_mut().spawn_empty().id();
        let err = set_mode(
            app.world_mut(),
            &json!({ "mode": "fft", "entity": gone.to_bits() }),
        )
        .expect_err("no body");
        assert_eq!(code(&err), Some("F0003"), "{err:?}");
        let mut empty = App::new();
        empty.add_plugins(WaterPlugin);
        let err = set_mode(empty.world_mut(), &json!({ "mode": "fft" })).expect_err("no body");
        assert_eq!(code(&err), Some("F0003"), "{err:?}");
        let mut bare = App::new();
        let err = set_mode(bare.world_mut(), &json!({ "mode": "fft" })).expect_err("no plugin");
        assert_eq!(code(&err), Some("F0003"), "{err:?}");
        // A ribbon has no waves to switch.
        let mut river = self::app();
        let err = set_mode(river.world_mut(), &json!({ "mode": "fft" })).expect_err("ribbon");
        assert_eq!(code(&err), Some("F0002"), "{err:?}");
        assert!(state_fn(&mut river)["bodies"][0]["mode"].is_null());
    }

    /// `water.ocean_cost` on a real GPU (lavapipe in CI) reports the pass's shape at each grid and
    /// leaves the frame's maps alone. Its times are reported, never compared.
    #[test]
    fn ocean_cost_reports_each_grid() {
        let mut app = sea_app();
        let err = ocean_cost(app.world_mut(), &json!({})).expect_err("no renderer");
        assert_eq!(code(&err), Some("F0003"), "{err:?}");
        let Ok(device) = crate::seeing::headless_render_device() else {
            eprintln!("skipping: no GPU adapter (lavapipe) available");
            return;
        };
        let renderer = WaterRenderer::new(&device);
        app.world_mut().insert_resource(WaterState {
            renderer: renderer.clone(),
            tier: WaterTier::High,
        });
        for bad in [
            json!({ "grids": [] }),
            json!({ "grids": [100] }),
            json!({ "grids": "all" }),
            json!({ "grids": [128, 256, 512, 1024, 128] }),
            json!({ "frames": 0 }),
            json!({ "frames": 1000 }),
            json!({ "frames": "many" }),
        ] {
            let err = ocean_cost(app.world_mut(), &bad).expect_err("refused");
            assert_eq!(code(&err), Some("F0002"), "{bad}: {err:?}");
        }
        let out = ocean_cost(app.world_mut(), &json!({ "grids": [128, 256], "frames": 2 })).expect("cost");
        assert_eq!(out["frames"], json!(2), "{out}");
        for (cost, (size, stages)) in out["costs"]
            .as_array()
            .expect("costs")
            .iter()
            .zip([(128u64, 14u64), (256, 16)])
        {
            assert_eq!(cost["ran"], json!(true), "{cost}");
            assert_eq!(cost["size"], json!(size), "{cost}");
            assert_eq!(cost["dispatches"], json!(3 + 3 * stages), "{cost}");
            assert!(cost["modes"].as_u64().expect("modes") > 1000, "{cost}");
            assert!(
                cost["gpu_bytes"].as_u64().expect("bytes") >= 4 * 6 * size * size * 16,
                "{cost}"
            );
            assert!(cost["submit_ms"].as_f64().expect("wall") >= 0.0, "{cost}");
        }
        assert_eq!(
            renderer.stats().ocean_evolves,
            0,
            "the frame's ocean did not move"
        );
        // ADR-2843: lingering foam adds its dispatches and memory, and only when it is on.
        for cost in out["costs"].as_array().expect("costs") {
            assert_eq!(cost["history_bytes"], json!(0), "{cost}");
            assert_eq!(cost["history_dispatches"], json!(0), "{cost}");
        }
        set(app.world_mut(), &json!({ "ocean": { "foam_decay": 2.0 } })).expect("lingering foam");
        let on = ocean_cost(app.world_mut(), &json!({ "grids": [128], "frames": 2 })).expect("cost");
        let cost = &on["costs"][0];
        assert_eq!(cost["history_dispatches"], json!(3), "{cost}");
        assert_eq!(cost["dispatches"], json!(3 + 3 * 14 + 3), "{cost}");
        assert_eq!(cost["history_bytes"], json!(2 * 3 * 128 * 128 * 16), "{cost}");
    }

    /// ADR-3251: on a device with a 128 MiB storage binding (the mobile min-spec profile) a 1024
    /// grid with a lingering-foam history is refused by name, without running; the same grid
    /// without the history is not refused.
    #[test]
    fn ocean_cost_names_a_grid_the_device_refuses() {
        let mut app = sea_app();
        let Ok(device) =
            regia_render::RenderDevice::headless_with_profile(regia_render::minspec::MinspecProfile::Mobile)
        else {
            eprintln!("skipping: no GPU adapter (lavapipe) available");
            return;
        };
        // Its own device (the profile): kept, not destroyed under other tests (ADR-7531).
        crate::seeing::keep_until_exit(&device);
        app.world_mut().insert_resource(WaterState {
            renderer: WaterRenderer::new(&device),
            tier: WaterTier::High,
        });
        set(app.world_mut(), &json!({ "ocean": { "foam_decay": 2.0 } })).expect("lingering foam");
        let out = ocean_cost(app.world_mut(), &json!({ "grids": [1024], "frames": 1 })).expect("cost");
        let cost = &out["costs"][0];
        assert_eq!(cost["ran"], json!(false), "{cost}");
        assert_eq!(cost["refused"]["code"], json!("storage_binding"), "{cost}");
        assert_eq!(cost["refused"]["limit"], json!(128u64 << 20), "{cost}");
        assert_eq!(cost["refused"]["needed"], json!(150_994_944u64), "{cost}");
        assert!(
            cost["error"]
                .as_str()
                .is_some_and(|e| e.contains("storage binding")),
            "{cost}"
        );
        let renderer = &app.world().resource::<WaterState>().renderer;
        let no_history = FftOcean::default();
        assert_eq!(renderer.check_ocean_grid(&no_history, 1024), Ok(()));
    }

    /// `water.ocean_check` follows the grid: the map size in use is the one it compares.
    #[test]
    fn ocean_check_follows_the_grid() {
        let Ok(device) = crate::seeing::headless_render_device() else {
            eprintln!("skipping: no GPU adapter (lavapipe) available");
            return;
        };
        let mut app = sea_app();
        let mut sea = WaterBody::plane([0.0, 2.0, 0.0], [200.0, 200.0], WaterLook::ocean());
        sea.ocean = Some(FftOcean {
            seed: 11,
            grid: 512,
            ..FftOcean::default()
        });
        let renderer = WaterRenderer::new(&device);
        renderer.set_bodies(&[&sea]);
        renderer.set_tier(WaterTier::High);
        renderer.set_clock(WaterClock {
            tick: 900,
            ..WaterClock::default()
        });
        app.world_mut().insert_resource(WaterClock {
            tick: 900,
            ..WaterClock::default()
        });
        app.world_mut().insert_resource(WaterState {
            renderer,
            tier: WaterTier::High,
        });
        let got = ocean_check(app.world_mut()).expect("check");
        assert_eq!(got["size"], json!(512), "{got}");
        assert!(got["worst_error"].as_f64().expect("error") < 1.0e-3, "{got}");
    }

    fn state_fn(app: &mut App) -> Value {
        state(app.world_mut()).expect("state")
    }

    /// ADR-2843: the ocean's foam fields go through `water.set` and come back in `water.state`,
    /// out-of-range ones are refused, and `queries_follow_tier` makes `water.surface` and
    /// `water.raycast` answer the stand-in swell Low draws instead of the ocean.
    #[test]
    fn foam_fields_and_queries_that_follow_the_tier() {
        let mut app = sea_app();
        let out = set(
            app.world_mut(),
            &json!({ "tick": 600, "ocean": { "wind_speed": 14.0, "foam_decay": 3.0, "foam_growth": 0.2, "whitecap_rescale": true } }),
        )
        .expect("set ocean");
        let ocean = &out["after"]["bodies"][0]["ocean"];
        assert_eq!(ocean["foam_decay_s"], json!(3.0), "{ocean}");
        assert!((ocean["foam_growth_s"].as_f64().expect("growth") - 0.2).abs() < 1.0e-6);
        assert_eq!(ocean["whitecap_rescale"], json!(true));
        assert_eq!(out["after"]["queries_follow_tier"], json!(false));
        for bad in [
            json!({ "ocean": { "foam_decay": -1.0 } }),
            json!({ "ocean": { "foam_decay": 1000.0 } }),
            json!({ "ocean": { "foam_growth": 31.0 } }),
            json!({ "ocean": { "whitecap_rescale": 2 } }),
            json!({ "queries_follow_tier": "yes" }),
        ] {
            let err = set(app.world_mut(), &bad).expect_err("refused");
            assert_eq!(code(&err), Some("F0002"), "{bad}: {err:?}");
        }
        assert_eq!(
            state_fn(&mut app)["bodies"][0]["ocean"]["foam_decay_s"],
            json!(3.0)
        );
        let at = json!({ "at": [[3.0, 4.0], [40.0, -11.0], [-70.0, 25.0]] });
        let ocean_answer = surface(app.world_mut(), &at).expect("surface");
        // Low, queries still on the ocean (the default): the same answer as on High.
        set(app.world_mut(), &json!({ "tier": "low" })).expect("low");
        assert_eq!(surface(app.world_mut(), &at).expect("surface"), ocean_answer);
        let on = set(app.world_mut(), &json!({ "queries_follow_tier": true })).expect("follow");
        assert_eq!(on["after"]["queries_follow_tier"], json!(true));
        let drawn = surface(app.world_mut(), &at).expect("surface");
        assert_ne!(drawn, ocean_answer, "Low answers its stand-in swell");
        let body = app
            .world_mut()
            .query::<&WaterBody>()
            .iter(app.world())
            .next()
            .cloned()
            .expect("sea");
        let clock = *app.world().resource::<WaterClock>();
        let stand_in = body.drawn_for(WaterTier::Low, true);
        assert!(stand_in.ocean.is_none() && stand_in.swell.is_some());
        let want = stand_in.surface(3.0, 4.0, &clock).expect("wet").height;
        let got = drawn["points"][0]["height"].as_f64().expect("height") as f32;
        assert!((got - want).abs() < 1.0e-6, "{got} vs {want}");
        let ray = raycast(
            app.world_mut(),
            &json!({ "origin": [3.0, 20.0, 4.0], "direction": [0.0, -1.0, 0.0] }),
        )
        .expect("raycast");
        assert!(
            (ray["point"][1].as_f64().expect("y") as f32 - want).abs() < 0.02,
            "{ray}"
        );
        // High draws the ocean: following the tier answers the ocean again.
        set(app.world_mut(), &json!({ "tier": "high" })).expect("high");
        assert_eq!(surface(app.world_mut(), &at).expect("surface"), ocean_answer);
    }

    #[test]
    fn set_without_the_plugin_is_not_found() {
        let mut app = App::new();
        let err = set(app.world_mut(), &json!({ "enabled": false })).expect_err("no plugin");
        let code = err
            .data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str);
        assert_eq!(code, Some("F0003"));
        assert_eq!(state(app.world_mut()).expect("state")["plugin"], json!(false));
    }

    /// Without `WaterSettings`, the settings fields are null. `renderer` stays a boolean and
    /// `bodies` still lists any `WaterBody`.
    #[test]
    fn state_without_the_plugin_nulls_settings_and_still_lists_bodies() {
        let mut app = App::new();
        let empty = state(app.world_mut()).expect("empty");
        assert_eq!(empty["plugin"], json!(false));
        assert!(empty["enabled"].is_null());
        assert!(empty["tier"].is_null());
        assert!(empty["forced_tier"].is_null());
        assert!(empty["debug"].is_null());
        assert!(empty["terrain_planes"].is_null());
        assert!(empty["shore"].is_null());
        assert!(empty["clock"].is_null());
        assert!(empty["sea_wind"].is_null());
        assert!(empty["last_frame"].is_null());
        assert_eq!(empty["renderer"], json!(false));
        assert_eq!(empty["bodies"], json!([]));
        app.world_mut()
            .spawn(WaterBody::plane([0.0, 0.0, 0.0], [10.0, 10.0], WaterLook::lake()));
        let listed = state(app.world_mut()).expect("body");
        assert_eq!(listed["plugin"], json!(false));
        assert_eq!(listed["renderer"], json!(false));
        assert!(listed["enabled"].is_null());
        assert_eq!(listed["bodies"].as_array().map(Vec::len), Some(1));
    }
}
