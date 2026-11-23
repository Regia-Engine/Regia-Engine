// SPDX-License-Identifier: MPL-2.0
//! Agent methods for audio zones (`docs/specs/16-audio.md` § 3d): `audio.list_zones` reports
//! every `AudioReverbZone` with its preset, distance scale and mouths; `audio.set_zone` creates
//! or edits one; `audio.probe_zones` answers "what would a listener here hear of an emitter
//! there" (reverb mix, distance scale, rock thickness, mouth route, final gain) without moving
//! anything, so an agent can verify a tunnel from the surface and from inside it. Over cooked world
//! geometry (`regia_audio::GeomSoundField`, W-25) `audio.list_zones` adds the section reverb armed at
//! player 0 and `audio.probe_zones` the portal path a source takes.

use regia_app::prelude::*;
use regia_audio::{
    Audio, AudioReverbZone, AudioRockField, AudioZonePortals, ReverbPreset, ReverbZoneShape, ZoneField,
    ZoneHearing, ZonePortal, placed_zones, reverb_mix, scaled_range,
};
use regia_core::math::Vec3;
use regia_world_geom::portal_sound::SoundCell;
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

fn require_audio(world: &World) -> Result<(), RpcError> {
    if world.get_resource::<Audio>().is_none() {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0013",
            "no audio in this app",
            "add regia_audio::AudioPlugin (regia::DefaultPlugins includes it)",
        ));
    }
    Ok(())
}

fn v3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn shape_json(shape: &ReverbZoneShape) -> Value {
    match shape {
        ReverbZoneShape::Sphere { radius } => json!({ "kind": "sphere", "radius": radius }),
        ReverbZoneShape::Box { half_extents } => json!({ "kind": "box", "half_extents": v3(*half_extents) }),
        ReverbZoneShape::Capsule { radius, half_length } => {
            json!({ "kind": "capsule", "radius": radius, "half_length": half_length })
        }
        ReverbZoneShape::Global => json!({ "kind": "global" }),
    }
}

fn zone_json(entity: Entity, placed: &regia_audio::PlacedZone, name: Option<String>) -> Value {
    let z = &placed.zone;
    json!({
        "entity": entity.to_bits(),
        "name": name,
        "shape": shape_json(&z.shape),
        "preset": z.preset.name(),
        "wet_db": z.send_db,
        "decay": z.feedback,
        "damping": z.damping,
        "stereo_width": z.stereo_width,
        "distance_scale": z.distance_scale,
        "blend_distance": z.fade_distance,
        "cavity": z.cavity,
        "priority": z.priority,
        "centre": v3(placed.centre),
        "portals": placed.portals.iter().map(|(p, r)| json!({ "position": v3(*p), "radius": r })).collect::<Vec<_>>(),
    })
}

/// `audio.list_zones`: every zone, the reverb mix applied to player 0 and whether a rock field
/// is installed.
pub fn list_zones(world: &mut World) -> Result<Value, RpcError> {
    require_audio(world)?;
    let zones = placed_zones(world);
    let rows: Vec<Value> = zones
        .iter()
        .map(|(e, z)| zone_json(*e, z, crate::scene::entity_name(world, *e)))
        .collect();
    let audio = world.resource::<Audio>();
    let mix = audio.reverb_mix().map(|m| {
        json!({
            "weight": m.weight, "send_db": m.send_db, "feedback": m.feedback,
            "damping": m.damping, "stereo_width": m.stereo_width,
        })
    });
    let field = world
        .get_resource::<AudioRockField>()
        .and_then(AudioRockField::get);
    let player0 = audio
        .listeners()
        .first()
        .map(|(_, f)| f.position)
        .unwrap_or(Vec3::ZERO);
    let armed = field.and_then(|f| f.armed_zone(player0)).map(|a| armed_json(&a));
    Ok(json!({
        "zones": rows,
        "reverb": mix,
        "rock_field": field.is_some(),
        "geom_field": field.is_some_and(|f| f.as_geom().is_some()),
        "armed_reverb": armed,
        "count": zones.len(),
    }))
}

fn armed_json(a: &regia_audio::ArmedZone) -> Value {
    json!({
        "section": a.section,
        "preset": a.preset_id,
        "decay": a.zone.feedback,
        "wet_db": a.zone.send_db,
    })
}

fn cell_json(cell: SoundCell) -> Value {
    match cell {
        SoundCell::Outdoor => json!({ "kind": "outdoor" }),
        SoundCell::Rock => json!({ "kind": "rock" }),
        SoundCell::Section(s) => json!({ "kind": "section", "section": s }),
    }
}

/// The portal path over cooked world geometry (W-25), or null without a geometry field.
fn portal_path_json(field: Option<&dyn regia_audio::RockField>, listener: Vec3, emitter: Vec3) -> Value {
    let Some(geom) = field.and_then(|f| f.as_geom()) else {
        return Value::Null;
    };
    let path = geom.path(listener, emitter);
    json!({
        "kind": path.kind.name(),
        "source_cell": cell_json(path.source_cell),
        "listener_cell": cell_json(path.listener_cell),
        "portals": path.portals,
        "points": path.points,
        "length_m": path.length_m,
        "direct_m": path.direct_m,
        "arrival": path.arrival,
        "apparent": path.apparent,
        "occlusion_m": path.occlusion_m,
        "range_scale": path.range_scale,
    })
}

fn num(params: &Map<String, Value>, field: &str, range: (f32, f32)) -> Result<Option<f32>, RpcError> {
    let Some(value) = params.get(field) else {
        return Ok(None);
    };
    let n = value
        .as_f64()
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number")))? as f32;
    if !n.is_finite() || n < range.0 || n > range.1 {
        return Err(RpcError::invalid_params(format!(
            "`{field}` must be within {}..={}",
            range.0, range.1
        )));
    }
    Ok(Some(n))
}

fn vec3(value: &Value, field: &str) -> Result<Vec3, RpcError> {
    let bad = || RpcError::invalid_params(format!("`{field}` must be [x, y, z] numbers"));
    let a = value.as_array().filter(|a| a.len() == 3).ok_or_else(bad)?;
    let mut out = [0.0f32; 3];
    for (slot, v) in out.iter_mut().zip(a) {
        let n = v.as_f64().filter(|n| n.is_finite()).ok_or_else(bad)?;
        *slot = n as f32;
    }
    Ok(Vec3::from_array(out))
}

fn parse_shape(value: &Value) -> Result<ReverbZoneShape, RpcError> {
    let obj = value
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("`shape` must be an object with a `kind`"))?;
    let kind = crate::params::opt_str(obj, "kind")?.unwrap_or("");
    let positive = |name: &str| -> Result<f32, RpcError> {
        num(obj, name, (0.0, 100_000.0))?
            .ok_or_else(|| RpcError::invalid_params(format!("shape `{kind}` needs `{name}`")))
    };
    match kind {
        "sphere" => Ok(ReverbZoneShape::Sphere {
            radius: positive("radius")?,
        }),
        "capsule" => Ok(ReverbZoneShape::Capsule {
            radius: positive("radius")?,
            half_length: positive("half_length")?,
        }),
        "box" => {
            let h = obj
                .get("half_extents")
                .ok_or_else(|| RpcError::invalid_params("shape `box` needs `half_extents`"))?;
            Ok(ReverbZoneShape::Box {
                half_extents: vec3(h, "half_extents")?.abs(),
            })
        }
        "global" => Ok(ReverbZoneShape::Global),
        other => Err(RpcError::invalid_params(format!(
            "shape kind `{other}` is not box, capsule, sphere or global"
        ))),
    }
}

/// `audio.set_zone`: creates or edits an entity's `AudioReverbZone`. A `preset` is applied
/// first, then the explicit fields override it. `portals` replaces the mouth list.
pub fn set_zone(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "use scene.query_entities or audio.list_zones",
        ));
    }
    let Some(fields) = params.as_object() else {
        return Err(RpcError::invalid_params("params must be an object"));
    };
    let existed = world.get::<AudioReverbZone>(entity).is_some();
    let mut zone = world.get::<AudioReverbZone>(entity).cloned().unwrap_or_default();
    if let Some(shape) = fields.get("shape") {
        zone.shape = parse_shape(shape)?;
    }
    if let Some(name) = fields.get("preset") {
        let name = name
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`preset` must be a name"))?;
        let preset = ReverbPreset::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown preset `{name}`; use custom, tube, mine_gallery or open"
            ))
        })?;
        zone = AudioReverbZone {
            priority: zone.priority,
            ..AudioReverbZone::with_preset(preset, zone.shape)
        };
    }
    if let Some(v) = num(fields, "wet_db", (-80.0, 12.0))? {
        zone.send_db = v;
    }
    if let Some(v) = num(fields, "decay", (0.0, 1.0))? {
        zone.feedback = v;
    }
    if let Some(v) = num(fields, "damping", (0.0, 1.0))? {
        zone.damping = v;
    }
    if let Some(v) = num(fields, "stereo_width", (0.0, 1.0))? {
        zone.stereo_width = v;
    }
    if let Some(v) = num(fields, "distance_scale", (0.1, 10.0))? {
        zone.distance_scale = v;
    }
    if let Some(v) = num(fields, "blend_distance", (0.0, 10_000.0))? {
        zone.fade_distance = v;
    }
    if let Some(v) = num(fields, "priority", (-1000.0, 1000.0))? {
        zone.priority = v as i32;
    }
    if let Some(v) = fields.get("cavity") {
        zone.cavity = v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`cavity` must be true or false"))?;
    }
    let portals = match fields.get("portals") {
        None => None,
        Some(list) => {
            let list = list.as_array().ok_or_else(|| {
                RpcError::invalid_params("`portals` must be a list of { position, radius }")
            })?;
            let mut out = Vec::new();
            for item in list {
                let obj = item.as_object().ok_or_else(|| {
                    RpcError::invalid_params("each portal is { position: [x,y,z], radius }")
                })?;
                let position = vec3(obj.get("position").unwrap_or(&Value::Null), "position")?;
                let radius = num(obj, "radius", (0.0, 10_000.0))?.unwrap_or(4.0);
                out.push(ZonePortal { position, radius });
            }
            Some(out)
        }
    };
    world.entity_mut(entity).insert(zone);
    if let Some(portals) = portals {
        world.entity_mut(entity).insert(AudioZonePortals { portals });
    }
    let (_, placed) = placed_zones(world)
        .into_iter()
        .find(|(e, _)| *e == entity)
        .ok_or_else(|| RpcError::invalid_params("zone vanished"))?;
    Ok(
        json!({ "created": !existed, "zone": zone_json(entity, &placed, crate::scene::entity_name(world, entity)) }),
    )
}

/// `audio.probe_zones { listener, emitter, [min_distance, max_distance] }`: what a listener at
/// `listener` hears of an emitter at `emitter` through the world's zones and rock field.
pub fn probe_zones(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let listener = vec3(
        params
            .get("listener")
            .ok_or_else(|| RpcError::invalid_params("`listener` [x, y, z] is required"))?,
        "listener",
    )?;
    let emitter = vec3(
        params
            .get("emitter")
            .ok_or_else(|| RpcError::invalid_params("`emitter` [x, y, z] is required"))?,
        "emitter",
    )?;
    let Some(fields) = params.as_object() else {
        return Err(RpcError::invalid_params("params must be an object"));
    };
    let mut base = regia_audio::AudioEmitter::default();
    if let Some(v) = num(fields, "min_distance", (0.0, 100_000.0))? {
        base.min_distance = v;
    }
    if let Some(v) = num(fields, "max_distance", (0.001, 100_000.0))? {
        base.max_distance = v;
    }
    let placed = placed_zones(world);
    let field = ZoneField {
        zones: placed.into_iter().map(|(_, z)| z).collect(),
    };
    let tuning = world
        .get_resource::<regia_audio::AudioOcclusion>()
        .map(|o| o.rock)
        .unwrap_or_default();
    let rock = world.get_resource::<AudioRockField>();
    let hearing = ZoneHearing::evaluate(
        &field,
        rock.and_then(AudioRockField::get),
        &tuning,
        &base,
        listener,
        emitter,
    );
    let (route, scale, thickness) = (hearing.route, hearing.distance_scale, hearing.rock_m);
    let rock_gain = thickness.map_or(1.0, |t| tuning.gain(t));
    let scaled = scaled_range(&base, scale);
    let distance = hearing.distance;
    let armed = rock
        .and_then(AudioRockField::get)
        .and_then(|f| f.armed_zone(listener));
    let mix = reverb_mix(
        listener,
        field.zones.iter().map(|z| (&z.zone, z.centre, z.rotation)).chain(
            armed
                .as_ref()
                .map(|a| (&a.zone, listener, regia_core::math::Quat::IDENTITY)),
        ),
    );
    let gain = hearing.gain;
    Ok(json!({
        "distance": distance,
        "distance_scale": scale,
        "effective_max_distance": scaled.max_distance,
        "in_cavity": { "listener": field.in_cavity(listener), "emitter": field.in_cavity(emitter) },
        "route": route.map(|r| json!({ "mouth": v3(r.mouth), "path_length": r.path_length, "virtual_position": v3(r.position) })),
        "rock_thickness_m": thickness,
        "rock_gain": rock_gain,
        "cutoff_hz": thickness.map(|t| tuning.cutoff_hz(t)),
        "gain": gain,
        "gain_db": regia_audio::gain_to_db(gain),
        "audible": gain > 0.003,
        "reverb": { "weight": mix.weight, "send_db": mix.send_db, "feedback": mix.feedback, "damping": mix.damping, "stereo_width": mix.stereo_width },
        "portal_path": portal_path_json(rock.and_then(AudioRockField::get), listener, emitter),
        "armed_reverb": rock.and_then(AudioRockField::get).and_then(|f| f.armed_zone(listener)).map(|a| armed_json(&a)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_audio::{AudioPlugin, AudioSettings, FnRockField};
    use regia_scene::Transform;
    use std::sync::Arc;

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.add_plugins(AudioPlugin);
        app
    }

    #[test]
    fn set_zone_applies_preset_then_overrides_and_lists() {
        let mut app = app();
        let e = app
            .world_mut()
            .spawn(Transform::from_translation(Vec3::new(0.0, -10.0, 0.0)))
            .id();
        let out = set_zone(
            app.world_mut(),
            &json!({
                "entity": e.to_bits(),
                "shape": { "kind": "capsule", "radius": 3.0, "half_length": 100.0 },
                "preset": "tube",
                "distance_scale": 1.8,
                "portals": [{ "position": [0.0, 100.0, 0.0], "radius": 5.0 }],
            }),
        )
        .expect("set");
        assert_eq!(out["created"], json!(true));
        assert_eq!(out["zone"]["preset"], json!("tube"));
        assert_eq!(out["zone"]["distance_scale"], json!(1.8f32));
        assert_eq!(out["zone"]["cavity"], json!(true));
        assert_eq!(out["zone"]["portals"].as_array().map(Vec::len), Some(1));
        let list = list_zones(app.world_mut()).expect("list");
        assert_eq!(list["count"], json!(1));
        assert_eq!(list["rock_field"], json!(false));
    }

    #[test]
    fn set_zone_validates() {
        let mut app = app();
        let e = app.world_mut().spawn_empty().id();
        let bad = |app: &mut App, p: Value| set_zone(app.world_mut(), &p).expect_err("rejected").code;
        let b = e.to_bits();
        assert_eq!(
            bad(&mut app, json!({ "entity": b, "preset": "cathedral" })),
            protocol::INVALID_PARAMS
        );
        assert_eq!(
            bad(&mut app, json!({ "entity": b, "distance_scale": 0.0 })),
            protocol::INVALID_PARAMS
        );
        assert_eq!(
            bad(
                &mut app,
                json!({ "entity": b, "shape": { "kind": "capsule", "radius": 1.0 } })
            ),
            protocol::INVALID_PARAMS
        );
        assert!(app.world().get::<AudioReverbZone>(e).is_none());
    }

    #[test]
    fn probe_reports_scale_rock_and_gain() {
        let mut app = app();
        let e = app
            .world_mut()
            .spawn(Transform::from_translation(Vec3::new(0.0, -10.0, 0.0)))
            .id();
        set_zone(
            app.world_mut(),
            &json!({ "entity": e.to_bits(), "preset": "tube",
                     "shape": { "kind": "capsule", "radius": 3.0, "half_length": 100.0 } }),
        )
        .expect("zone");
        // The capsule's axis is local Y: rotate it along X.
        app.world_mut().entity_mut(e).insert(Transform {
            translation: Vec3::new(0.0, -10.0, 0.0),
            rotation: regia_core::math::Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            ..Transform::default()
        });
        app.world_mut()
            .resource_mut::<AudioRockField>()
            .set(Arc::new(FnRockField(|_x: f32, _z: f32| Some(0.0f32))));
        let inside = probe_zones(
            app.world_mut(),
            &json!({ "listener": [-40.0, -10.0, 0.0], "emitter": [40.0, -10.0, 0.0], "max_distance": 60.0 }),
        )
        .expect("probe");
        assert_eq!(inside["distance_scale"], json!(1.5));
        assert_eq!(
            inside["audible"],
            json!(true),
            "80 m apart in the tube with a 60 m range: {inside}"
        );
        let surface = probe_zones(
            app.world_mut(),
            &json!({ "listener": [40.0, 2.0, 0.0], "emitter": [40.0, -10.0, 0.0], "max_distance": 60.0 }),
        )
        .expect("probe");
        let thickness = surface["rock_thickness_m"].as_f64().expect("thickness");
        assert!(
            (6.0..=8.0).contains(&thickness),
            "7 m of rock above the tube: {surface}"
        );
        assert!(surface["rock_gain"].as_f64().expect("gain") < 0.2, "{surface}");
        assert_eq!(
            surface["distance_scale"],
            json!(1.0),
            "the surface is outside the tube"
        );
    }
}
