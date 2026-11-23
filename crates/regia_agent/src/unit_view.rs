// SPDX-License-Identifier: MPL-2.0
//! `terrain.unit_view`: overview flags for one viewer (ADR-8333, book/agents/covered-overview.md).

use regia_app::prelude::*;
use regia_world_geom::compat::sense::satellite_shows;
use regia_world_geom::overview::{self, UnitView};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

fn bad(message: impl Into<String>) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        "unit_view_bad",
        message,
        "pass team, viewer [x, y, z] and units [{id, team, pos, underground, entrance, lit, headlights, ping}]",
    )
}

/// Read overview flags. Does not change the world. `{}` answers `unit_view_bad` and does not panic.
///
/// # Errors
///
/// `unit_view_bad` when `team`, `viewer` or `units` is missing or not the documented shape.
pub fn unit_view(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let team = u32_field(params.get("team"), "team")?;
    let viewer = vec3(params.get("viewer"), "viewer")?;
    let viewer_underground = match params.get("viewer_underground") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(bad("`viewer_underground` must be a boolean")),
    };
    let controlled = match params.get("controlled") {
        None | Some(Value::Null) => None,
        Some(value) => Some(u32_value(value, "controlled")?),
    };
    let Some(rows) = params.get("units").and_then(Value::as_array) else {
        return Err(bad("`units` must be an array"));
    };
    let mut units = Vec::with_capacity(rows.len());
    for row in rows {
        units.push(one_unit(row)?);
    }
    overview::mark_rock_hidden(&mut units, controlled, team, viewer, viewer_underground);
    let out: Vec<Value> = units
        .iter()
        .map(|unit| {
            json!({
                "id": unit.id,
                "flags": unit.flags,
                "shown": satellite_shows(unit.underground, unit.entrance),
            })
        })
        .collect();
    Ok(json!({ "units": out }))
}

fn one_unit(row: &Value) -> Result<UnitView, RpcError> {
    let Some(row) = row.as_object() else {
        return Err(bad("each unit must be an object"));
    };
    let pos = vec3(row.get("pos"), "pos")?;
    Ok(UnitView {
        id: u32_value(row.get("id").unwrap_or(&Value::Null), "id")?,
        team: u32_value(row.get("team").unwrap_or(&Value::Null), "team")?,
        pos,
        flags: 0,
        lit: bool_field(row.get("lit"), "lit")?,
        headlights: bool_field(row.get("headlights"), "headlights")?,
        ping: bool_field(row.get("ping"), "ping")?,
        entrance: bool_field(row.get("entrance"), "entrance")?,
        underground: bool_field(row.get("underground"), "underground")?,
    })
}

fn u32_field(value: Option<&Value>, name: &str) -> Result<u32, RpcError> {
    match value {
        Some(value) => u32_value(value, name),
        None => Err(bad(format!("`{name}` is required"))),
    }
}

fn u32_value(value: &Value, name: &str) -> Result<u32, RpcError> {
    let Some(number) = value.as_u64() else {
        return Err(bad(format!("`{name}` must be an integer")));
    };
    u32::try_from(number).map_err(|_| bad(format!("`{name}` must be an integer")))
}

fn vec3(value: Option<&Value>, name: &str) -> Result<[f32; 3], RpcError> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Err(bad(format!("`{name}` must be [x, y, z]")));
    };
    if items.len() != 3 {
        return Err(bad(format!("`{name}` must be [x, y, z]")));
    }
    let mut out = [0.0f32; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        let Some(number) = item.as_f64() else {
            return Err(bad(format!("`{name}` must be [x, y, z]")));
        };
        if !number.is_finite() {
            return Err(bad(format!("`{name}` must be finite")));
        }
        *slot = number as f32;
    }
    Ok(out)
}

fn bool_field(value: Option<&Value>, name: &str) -> Result<bool, RpcError> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(bad(format!("`{name}` must be a boolean"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrain_unit_view_hides_a_tube_unit_and_shows_an_entrance() {
        let mut world = World::new();
        let params = json!({
            "team": 1,
            "viewer": [1.0, 2.0, 200.0],
            "viewer_underground": false,
            "units": [
                {
                    "id": 2,
                    "team": 2,
                    "pos": [1.0, 2.0, 0.0],
                    "underground": true,
                    "entrance": false,
                    "headlights": true,
                    "ping": true
                },
                {
                    "id": 3,
                    "team": 2,
                    "pos": [1.0, 2.0, 0.0],
                    "underground": true,
                    "entrance": true
                }
            ]
        });
        let result = match unit_view(&mut world, &params) {
            Ok(result) => result,
            Err(err) => panic!("unit view: {err:?}"),
        };
        assert_eq!(result["units"][0]["shown"].as_bool(), Some(false));
        let flags = result["units"][0]["flags"].as_u64().unwrap_or(0);
        assert_ne!(flags & u64::from(overview::ROCK_HIDDEN), 0);
        assert_ne!(flags & u64::from(overview::HEADLIGHT_ON), 0);
        assert_ne!(flags & u64::from(overview::PING_FLASH), 0);
        assert_eq!(result["units"][1]["shown"].as_bool(), Some(true));
        let missing = match unit_view(&mut world, &json!({})) {
            Ok(value) => panic!("empty params returned {value}"),
            Err(err) => err,
        };
        assert_eq!(
            missing
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str),
            Some("unit_view_bad")
        );
        let _method = "terrain.unit_view";
    }
}
