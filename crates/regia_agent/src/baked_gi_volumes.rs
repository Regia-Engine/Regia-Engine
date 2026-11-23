// SPDX-License-Identifier: MPL-2.0
//! Split baked GI volumes and the brick-loop probe on the agent surface (ADR-2618,
//! `book/agents/baked-gi-split.md`): `render.get_gi_mode`'s `baked.volumes` (every volume the
//! last frame blended, with its storage slot and split part), `baked.volume_slots`,
//! `baked.brick_visits` (the probe's histogram), and `render.set_baked_gi_brick_probe`.

use regia_app::prelude::*;
use regia_render::{BakedGiStatus, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Edge names of `BakedGiVolumeEntry::joined`, in its order.
const EDGES: [&str; 4] = ["-x", "-z", "+x", "+z"];

/// `baked.volumes`: one object per volume the last frame blended, in blend order.
pub(crate) fn volumes_json(s: &BakedGiStatus) -> Value {
    json!(
        s.volume_list
            .iter()
            .map(|v| {
                let joined: Vec<&str> = EDGES
                    .iter()
                    .zip(v.joined)
                    .filter(|(_, j)| *j)
                    .map(|(e, _)| *e)
                    .collect();
                let split = v.split.map(|(index, count, group)| {
                    json!({
                        "index": index,
                        "count": count,
                        "group": format!("{group:016x}"),
                        "joined": joined,
                    })
                });
                json!({
                    "probes": v.probes,
                    "bytes": v.bytes,
                    "slot": v.slot,
                    "priority": v.priority,
                    "weight": v.weight,
                    "split": split,
                })
            })
            .collect::<Vec<_>>()
    )
}

/// `baked.brick_visits`: the probe's last histogram, `null` while it is off or not read back yet.
pub(crate) fn brick_visits_json(s: &BakedGiStatus) -> Value {
    s.brick_visits.as_ref().map_or(Value::Null, |b| {
        json!({
            "histogram": b.histogram,
            "texels": b.texels,
            "mean": b.mean,
            "max": b.max,
            "captures": b.captures,
            "bins": regia_render::baked_gi::BRICK_VISIT_BINS,
        })
    })
}

/// `render.set_baked_gi_brick_probe {on}`: switches the brick-loop probe (bricks tested per
/// screen texel, as a histogram read back a few frames later into `render.get_gi_mode`
/// `baked.brick_visits`). Returns the setting before and after and the last histogram.
pub(crate) fn set_brick_probe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let on = params
        .get("on")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`on` is required: true or false"))?;
    let mut renderer = world.get_resource_mut::<RuntimeRenderer>().ok_or_else(|| {
        RpcError::not_found(
            "no RuntimeRenderer resource: this app has no runtime renderer",
            "add regia_render::RenderPlugin",
        )
    })?;
    let mut settings = renderer.0.baked_gi_settings();
    let before = settings.brick_visit_probe;
    settings.brick_visit_probe = on;
    renderer.0.set_baked_gi_settings(settings);
    let status = renderer.0.baked_gi_status();
    Ok(json!({
        "before": before,
        "on": on,
        "volume_installed": status.installed,
        "brick_visits": brick_visits_json(&status),
        "note": "counts from the next frame while a baked GI volume lights it; the histogram reaches \
                 render.get_gi_mode baked.brick_visits about three frames later (no stall). The light \
                 is unchanged",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::baked_gi::{BakedGiVolumeEntry, BrickVisits};

    #[test]
    fn volumes_and_visits_serialize_with_their_split_parts() {
        let status = BakedGiStatus {
            volume_list: vec![
                BakedGiVolumeEntry {
                    probes: 10,
                    bytes: 4096,
                    slot: 0,
                    priority: 0,
                    weight: 1.0,
                    split: Some((0, 2, 0xabc)),
                    joined: [false, false, true, false],
                },
                BakedGiVolumeEntry {
                    probes: 12,
                    slot: 1,
                    ..BakedGiVolumeEntry::default()
                },
            ],
            brick_visits: Some(BrickVisits::of(vec![3, 1, 0, 2], 5)),
            ..BakedGiStatus::default()
        };
        let v = volumes_json(&status);
        assert_eq!(v[0]["split"]["count"], 2);
        assert_eq!(v[0]["split"]["group"], "0000000000000abc");
        assert_eq!(v[0]["split"]["joined"], json!(["+x"]));
        assert!(v[1]["split"].is_null());
        assert_eq!(v[1]["slot"], 1);
        let b = brick_visits_json(&status);
        assert_eq!(b["texels"], 6);
        assert_eq!(b["max"], 3);
        assert_eq!(b["captures"], 5);
        assert!((b["mean"].as_f64().expect("mean") - 7.0 / 6.0).abs() < 1e-6);
        assert!(brick_visits_json(&BakedGiStatus::default()).is_null());
    }

    #[test]
    fn the_probe_switch_needs_on_and_a_renderer() {
        let mut world = World::new();
        assert!(set_brick_probe(&mut world, &json!({})).is_err());
        assert!(set_brick_probe(&mut world, &json!({ "on": true })).is_err());
    }
}
