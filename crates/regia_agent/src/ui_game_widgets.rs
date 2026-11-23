// SPDX-License-Identifier: MPL-2.0
//! `ui.lobby_list`, `ui.scoreboard`, `ui.radar`, `ui.world_markers` and `ui.target_bracket`: the game
//! widgets of `regia_ui::game_widgets` (ADR-2958, `book/agents/ui-game-widgets.md`).
//!
//! Every method reads the widget back in the coordinates it drew with, and validates all parameters
//! before it changes anything. A method without `entity` addresses the widget with the lowest
//! entity bits. Lobby ids are 64-bit, so they are written as decimal strings, as `lobby.*` does.

use regia_core::glam::{Affine3A, Vec3};
use regia_ecs::prelude::*;
use regia_scene::{GlobalTransform, Transform};
use regia_ui::game_widgets::radar::{self, MAX_BLIPS};
use regia_ui::game_widgets::scoreboard::{MAX_SCORE_COLUMNS, MAX_SCORE_ROWS, sort_by};
use regia_ui::game_widgets::{
    CurrentLobby, LobbyList, LobbyListAction, LobbyListDriver, LobbyListRequest, LobbyListStatus, LobbyRow,
    MarkerLayer, MarkerLayerState, Radar, RadarMode, RadarShape, RadarState, ScoreColumn, ScoreLine,
    ScoreRow, ScoreValue, Scoreboard, ScoreboardState, TargetLock, WorldMarker, WorldPlate, WorldPlateRoot,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use crate::ui_parity2::{color_of, entity_of};

/// Most rows `ui.scoreboard` accepts in one `set`.
const MAX_SET_ROWS: usize = 1024;

fn err(class: i64, code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(class, code, message, hint)
}

fn bad_param(message: impl Into<String>) -> RpcError {
    err(
        INVALID_PARAMS,
        "bad_param",
        message,
        "see book/agents/ui-game-widgets.md for the parameters",
    )
}

fn bad_action(method: &str, action: &str, allowed: &str) -> RpcError {
    err(
        INVALID_PARAMS,
        "bad_action",
        format!("{method} has no action `{action}`"),
        format!("use one of: {allowed}"),
    )
}

fn no_widget(kind: &str) -> RpcError {
    err(
        NOT_FOUND,
        "no_game_widget",
        format!("no {kind} node"),
        format!("spawn a {kind} or author one in .ui.ron; ui.query finds the node"),
    )
}

/// The widget named by `entity`, else the one with the lowest bits.
fn widget_of<C: Component>(world: &mut World, params: &Value, kind: &str) -> Result<Entity, RpcError> {
    if params.get("entity").is_some_and(|v| !v.is_null()) {
        let entity = entity_of(world, params)?;
        if world.get::<C>(entity).is_none() {
            return Err(no_widget(kind));
        }
        return Ok(entity);
    }
    world
        .query_filtered::<Entity, With<C>>()
        .iter(world)
        .min_by_key(|e| e.to_bits())
        .ok_or_else(|| no_widget(kind))
}

fn action_of(params: &Value) -> Result<&str, RpcError> {
    match params.get("action") {
        None | Some(Value::Null) => Ok("query"),
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(bad_param("`action` must be a string")),
    }
}

fn finite(v: &Value, field: &str) -> Result<f32, RpcError> {
    // After the narrowing: 1e39 is a finite f64 and an infinite f32.
    v.as_f64()
        .map(|f| f as f32)
        .filter(|f| f.is_finite())
        .ok_or_else(|| bad_param(format!("`{field}` must be a finite number")))
}

fn positive(v: &Value, field: &str) -> Result<f32, RpcError> {
    let f = finite(v, field)?;
    if f <= 0.0 {
        return Err(bad_param(format!("`{field}` must be greater than zero")));
    }
    Ok(f)
}

fn floats<const N: usize>(v: &Value, field: &str) -> Result<[f32; N], RpcError> {
    let a = v
        .as_array()
        .filter(|a| a.len() == N)
        .ok_or_else(|| bad_param(format!("`{field}` must be an array of {N} numbers")))?;
    let mut out = [0.0; N];
    for (slot, value) in out.iter_mut().zip(a) {
        *slot = finite(value, field)?;
    }
    Ok(out)
}

fn param<'a>(params: &'a Value, key: &str) -> Option<&'a Value> {
    params.get(key).filter(|v| !v.is_null())
}

fn bool_param(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match param(params, key) {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| bad_param(format!("`{key}` must be true or false"))),
    }
}

fn string_param(params: &Value, key: &str) -> Result<Option<String>, RpcError> {
    match param(params, key) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| bad_param(format!("`{key}` must be a string"))),
    }
}

// ---- ui.lobby_list ----------------------------------------------------------------------------

fn lobby_row_json(r: &LobbyRow) -> Value {
    json!({
        "id": r.id.to_string(),
        "name": r.name,
        "detail": r.detail,
        "players": r.players,
        "max_players": r.max_players,
        "joinable": r.joinable,
    })
}

fn current_json(c: &CurrentLobby) -> Value {
    let members: Vec<Value> = c
        .members
        .iter()
        .map(|m| {
            json!({
                "id": m.id.to_string(),
                "name": m.name,
                "ready": m.ready,
                "owner": m.owner,
                "local": m.local,
            })
        })
        .collect();
    json!({
        "id": c.id.to_string(),
        "name": c.name,
        "max_players": c.max_players,
        "members": members,
        "is_owner": c.is_owner,
        "can_launch": c.can_launch,
        "launched": c.launched,
    })
}

fn lobby_id(params: &Value) -> Result<u64, RpcError> {
    match param(params, "lobby") {
        Some(Value::String(s)) => s.parse().ok(),
        Some(v) => v.as_u64(),
        None => None,
    }
    .filter(|id| *id != 0)
    .ok_or_else(|| bad_param("`lobby` must be a non-zero lobby id (string or integer)"))
}

/// `ui.lobby_list { entity?, action?, lobby? }`: `query` (default), or what a button of the widget
/// asks for. The answer of an action arrives on a later frame; `wait_until` on this method sees it.
pub(crate) fn lobby_list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = widget_of::<LobbyList>(world, params, "lobby list")?;
    let action = action_of(params)?;
    let request = match action {
        "query" => None,
        "refresh" => Some(LobbyListAction::Refresh),
        "create" => Some(LobbyListAction::Create),
        "join" => Some(LobbyListAction::Join(lobby_id(params)?)),
        "leave" => Some(LobbyListAction::Leave),
        "ready" => Some(LobbyListAction::SetReady(true)),
        "unready" => Some(LobbyListAction::SetReady(false)),
        "launch" => Some(LobbyListAction::Launch),
        other => {
            return Err(bad_action(
                "ui.lobby_list",
                other,
                "query, refresh, create, join, leave, ready, unready, launch",
            ));
        }
    };
    if let Some(request) = request {
        if !world.contains_resource::<LobbyListDriver>() {
            return Err(err(
                NOT_FOUND,
                "no_lobby_driver",
                "nothing answers lobby list requests in this app",
                "add regia_net::LobbyPlugin (regia_framework::game_ui drives the list from it), or insert \
                 LobbyListDriver and read LobbyListRequest",
            ));
        }
        world.write_message(LobbyListRequest {
            list: entity,
            action: request,
        });
    }
    let driver = world.get_resource::<LobbyListDriver>().map(|d| d.name.clone());
    let Some(list) = world.get::<LobbyList>(entity) else {
        return Err(no_widget("lobby list"));
    };
    let (status, error) = match &list.status {
        LobbyListStatus::Idle => ("idle", None),
        LobbyListStatus::Busy => ("busy", None),
        LobbyListStatus::Error(code) => ("error", Some(code.clone())),
    };
    Ok(json!({
        "entity": entity.to_bits(),
        "rows": list.rows.iter().map(lobby_row_json).collect::<Vec<_>>(),
        "current": list.current.as_ref().map(current_json),
        "status": status,
        "error": error,
        "manual": list.manual,
        "driver": driver,
        "sent": request.map(LobbyListAction::name),
    }))
}

// ---- ui.scoreboard ----------------------------------------------------------------------------

fn value_json(v: &ScoreValue) -> Value {
    match v {
        ScoreValue::Int(n) => json!(n),
        ScoreValue::Text(t) => json!(t),
    }
}

fn parse_columns(v: &Value) -> Result<Vec<ScoreColumn>, RpcError> {
    let a = v
        .as_array()
        .ok_or_else(|| bad_param("`columns` must be an array of { key, title?, width? }"))?;
    if a.len() > MAX_SCORE_COLUMNS {
        return Err(bad_param(format!("at most {MAX_SCORE_COLUMNS} columns")));
    }
    a.iter()
        .map(|c| {
            let key = c
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| bad_param("a column needs a string `key`"))?;
            let width = match param(c, "width") {
                None => 0.0,
                Some(w) => positive(w, "width")?,
            };
            Ok(ScoreColumn {
                key: key.to_owned(),
                title: string_param(c, "title")?.unwrap_or_default(),
                width,
            })
        })
        .collect()
}

fn parse_score_value(v: &Value) -> Result<ScoreValue, RpcError> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .map(ScoreValue::Int)
            .ok_or_else(|| bad_param("a score value must be an integer or a string")),
        Value::String(s) => Ok(ScoreValue::Text(s.clone())),
        _ => Err(bad_param("a score value must be an integer or a string")),
    }
}

fn parse_rows(v: &Value) -> Result<Vec<ScoreRow>, RpcError> {
    let a = v
        .as_array()
        .ok_or_else(|| bad_param("`rows` must be an array of { id, name, team?, local?, values }"))?;
    if a.len() > MAX_SET_ROWS {
        return Err(bad_param(format!("at most {MAX_SET_ROWS} rows")));
    }
    a.iter()
        .map(|r| {
            let id = r
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| bad_param("a row needs an integer `id`"))?;
            let team = match param(r, "team") {
                None => None,
                Some(t) => Some(
                    t.as_u64()
                        .and_then(|n| u8::try_from(n).ok())
                        .ok_or_else(|| bad_param("`team` must be 0 to 255"))?,
                ),
            };
            let values = match param(r, "values") {
                None => Vec::new(),
                Some(vs) => vs
                    .as_array()
                    .ok_or_else(|| bad_param("`values` must be an array"))?
                    .iter()
                    .map(parse_score_value)
                    .collect::<Result<Vec<_>, _>>()?,
            };
            Ok(ScoreRow {
                id,
                name: string_param(r, "name")?.unwrap_or_default(),
                team,
                local: bool_param(r, "local")?.unwrap_or(false),
                values,
            })
        })
        .collect()
}

/// `ui.scoreboard { entity?, action?, column?, visible?, columns?, rows? }`.
pub(crate) fn scoreboard(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = widget_of::<Scoreboard>(world, params, "scoreboard")?;
    let action = action_of(params)?;
    match action {
        "query" => {}
        "sort" => {
            let count = world
                .get::<Scoreboard>(entity)
                .map_or(0, |b| b.table.columns.len());
            let column = param(params, "column")
                .and_then(Value::as_u64)
                .map(|c| c as usize)
                .filter(|c| *c < count)
                .ok_or_else(|| bad_param(format!("`column` must be an index below {count}")))?;
            if let Some(mut board) = world.get_mut::<Scoreboard>(entity) {
                sort_by(&mut board, column);
            }
        }
        "show" | "hide" => {
            if let Some(mut board) = world.get_mut::<Scoreboard>(entity) {
                board.visible = action == "show";
            }
        }
        "set" => {
            let columns = param(params, "columns").map(parse_columns).transpose()?;
            let rows = param(params, "rows").map(parse_rows).transpose()?;
            if columns.is_none() && rows.is_none() {
                return Err(bad_param("set needs `columns` or `rows`"));
            }
            if let Some(mut board) = world.get_mut::<Scoreboard>(entity) {
                board.from_rules = false;
                if let Some(columns) = columns {
                    board.table.columns = columns;
                }
                if let Some(rows) = rows {
                    board.table.rows = rows;
                }
            }
        }
        other => {
            return Err(bad_action("ui.scoreboard", other, "query, sort, show, hide, set"));
        }
    }
    let Some(board) = world.get::<Scoreboard>(entity) else {
        return Err(no_widget("scoreboard"));
    };
    let state = world.get::<ScoreboardState>(entity);
    let sort = board
        .sort_column
        .map(|c| json!({ "column": c, "descending": board.descending }));
    // The order the board draws next, computed now so a `sort` answers with its own result.
    let sort_column = board.sort_column.filter(|c| *c < board.table.columns.len());
    let lines =
        regia_ui::game_widgets::score_lines(&board.table, sort_column, board.descending, board.group_by_team);
    // What the board draws, so at most what it can draw; `dropped` reports the rest.
    let lines: Vec<Value> = lines
        .iter()
        .take(MAX_SCORE_ROWS)
        .map(|l| match l {
            ScoreLine::Team { id, name, totals, .. } => {
                json!({ "kind": "team", "team": id, "name": name, "totals": totals })
            }
            ScoreLine::Player { row } => {
                let r = &board.table.rows[*row];
                json!({
                    "kind": "player",
                    "id": r.id,
                    "name": r.name,
                    "team": r.team,
                    "local": r.local,
                    "values": r.values.iter().map(value_json).collect::<Vec<_>>(),
                })
            }
        })
        .collect();
    let columns: Vec<Value> = board
        .table
        .columns
        .iter()
        .map(|c| json!({ "key": c.key, "title": c.title, "width": c.width }))
        .collect();
    Ok(json!({
        "entity": entity.to_bits(),
        "title": board.title,
        "shown": state.is_some_and(|s| s.shown),
        "visible": board.visible,
        "hold_action": board.hold_action,
        "from_rules": board.from_rules,
        "group_by_team": board.group_by_team,
        "sort": sort,
        "columns": columns,
        "lines": lines,
        "dropped": state.map_or(0, |s| s.dropped),
    }))
}

// ---- ui.radar ---------------------------------------------------------------------------------

/// `ui.radar { entity?, action?, range?, world_rect?, rotate?, center?, layer_mask?, max_blips? }`.
pub(crate) fn radar(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = widget_of::<Radar>(world, params, "radar")?;
    match action_of(params)? {
        "query" => {}
        "set" => {
            let range = param(params, "range").map(|v| positive(v, "range")).transpose()?;
            let rect = param(params, "world_rect")
                .map(|v| floats::<4>(v, "world_rect"))
                .transpose()?;
            if let Some(r) = rect
                && !(r[2] > r[0] && r[3] > r[1])
            {
                return Err(bad_param(
                    "`world_rect` is [min_x, min_z, max_x, max_z] with max > min",
                ));
            }
            if range.is_some() && rect.is_some() {
                return Err(bad_param(
                    "give `range` (a radar) or `world_rect` (a minimap), not both",
                ));
            }
            let rotate = bool_param(params, "rotate")?;
            let center = param(params, "center")
                .map(|v| floats::<2>(v, "center"))
                .transpose()?;
            let layer_mask = param(params, "layer_mask")
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| bad_param("`layer_mask` must be a 32-bit mask"))
                })
                .transpose()?;
            let max_blips = param(params, "max_blips")
                .map(|v| {
                    v.as_u64()
                        .map(|n| n as usize)
                        .filter(|n| *n <= MAX_BLIPS)
                        .ok_or_else(|| bad_param(format!("`max_blips` must be 0 to {MAX_BLIPS}")))
                })
                .transpose()?;
            if let Some(mut radar) = world.get_mut::<Radar>(entity) {
                // The outline goes with the mode, as `Radar::with_range` and `Radar::minimap` have it; a
                // radar already square stays so when only its range changes.
                if let Some(range) = range {
                    if matches!(radar.mode, RadarMode::Minimap { .. }) {
                        radar.shape = RadarShape::Circle;
                    }
                    radar.mode = RadarMode::Radar { range };
                }
                if let Some(world_rect) = rect {
                    radar.shape = RadarShape::Rect;
                    radar.mode = RadarMode::Minimap { world_rect };
                }
                if let Some(rotate) = rotate {
                    radar.rotate = rotate;
                }
                if let Some(center) = center {
                    radar.center = center;
                }
                if let Some(mask) = layer_mask {
                    radar.layer_mask = mask;
                }
                if let Some(max) = max_blips {
                    radar.max_blips = max;
                }
            }
        }
        other => return Err(bad_action("ui.radar", other, "query, set")),
    }
    let Some(widget) = world.get::<Radar>(entity).cloned() else {
        return Err(no_widget("radar"));
    };
    let state = world.get::<RadarState>(entity).cloned().unwrap_or_default();
    let (mode, range, rect) = match widget.mode {
        RadarMode::Radar { range } => ("radar", Some(range), None),
        RadarMode::Minimap { world_rect } => ("minimap", None, Some(world_rect)),
    };
    let blips: Vec<Value> = state
        .blips
        .iter()
        .map(|b| {
            json!({
                "source": b.source.to_bits(),
                "node": b.node.to_bits(),
                "label": b.label,
                "world": b.world,
                "pos": b.pos,
                "inside": b.inside,
            })
        })
        .collect();
    Ok(json!({
        "entity": entity.to_bits(),
        "mode": mode,
        "range": range,
        "world_rect": rect,
        "shape": widget.shape.as_str(),
        "rotate": widget.rotate,
        "has_map": widget.map.is_some(),
        "center": state.center,
        "heading": state.heading,
        "size": state.size,
        "blips": blips,
        "dropped": state.dropped,
        "default_max_blips": radar::DEFAULT_MAX_BLIPS,
    }))
}

// ---- ui.world_markers and ui.target_bracket ---------------------------------------------------

/// Marks an anchor entity an agent spawned for a marker on a bare position, so `clear` removes it.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct AgentAnchor;

/// Where a marker or lock goes, read before the action runs (ADR-5957): `target` as entity bits and
/// `position` as `[x, y, z]`, each checked whether or not the action uses it.
#[derive(Debug, Clone, Copy)]
struct Placement {
    target: Option<Entity>,
    position: Option<Vec3>,
}

/// Entity bits under `key`, if given; refused unless they decode to an entity id.
fn entity_bits(params: &Value, key: &str) -> Result<Option<Entity>, RpcError> {
    param(params, key)
        .map(|v| {
            v.as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| bad_param(format!("`{key}` must be entity bits")))
        })
        .transpose()
}

fn placement_of(params: &Value) -> Result<Placement, RpcError> {
    Ok(Placement {
        target: entity_bits(params, "target")?,
        position: param(params, "position")
            .map(|v| floats::<3>(v, "position").map(Vec3::from_array))
            .transpose()?,
    })
}

/// The entity a marker or lock goes on: `target`, or a new anchor at `position`.
fn target_or_anchor(world: &mut World, place: Placement) -> Result<Entity, RpcError> {
    if let Some(target) = place.target {
        if world.get_entity(target).is_err() {
            return Err(err(
                NOT_FOUND,
                "unknown_entity",
                "the target does not exist",
                "take entity ids from scene.query_entities",
            ));
        }
        if world.get::<GlobalTransform>(target).is_none() {
            return Err(err(
                INVALID_PARAMS,
                "no_world_position",
                "the target has no GlobalTransform",
                "mark an entity placed in the world, or pass `position`",
            ));
        }
        return Ok(target);
    }
    let Some(at) = place.position else {
        return Err(bad_param("give `target` (entity bits) or `position` ([x, y, z])"));
    };
    Ok(world
        .spawn((
            AgentAnchor,
            Transform::from_translation(at),
            GlobalTransform(Affine3A::from_translation(at)),
        ))
        .id())
}

fn fade_param(params: &Value) -> Result<Option<[f32; 2]>, RpcError> {
    param(params, "fade").map(|v| floats::<2>(v, "fade")).transpose()
}

/// The keys `plate` takes.
const PLATE_KEYS: &[&str] = &["size", "ppm", "fade", "depth_test"];

fn marker_of(params: &Value) -> Result<WorldMarker, RpcError> {
    let mut marker = WorldMarker {
        label: string_param(params, "label")?.unwrap_or_default(),
        clamp_to_edge: bool_param(params, "clamp_to_edge")?.unwrap_or(false),
        show_distance: bool_param(params, "show_distance")?.unwrap_or(false),
        fade: fade_param(params)?,
        ..WorldMarker::default()
    };
    if let Some(v) = param(params, "color") {
        marker.color = color_of(v, "color")?;
    }
    if let Some(v) = param(params, "size") {
        marker.size_px = positive(v, "size")?;
    }
    if let Some(v) = param(params, "offset") {
        marker.offset = floats::<3>(v, "offset")?;
    }
    if let Some(v) = param(params, "icon") {
        marker.icon = Some(
            v.as_u64()
                .ok_or_else(|| bad_param("`icon` must be an image id"))?,
        );
    }
    if let Some(v) = param(params, "plate") {
        let keys = v
            .as_object()
            .ok_or_else(|| bad_param("`plate` must be an object"))?;
        if let Some(unknown) = keys.keys().find(|k| !PLATE_KEYS.contains(&k.as_str())) {
            return Err(crate::params::unknown_param(
                "ui.world_markers",
                &format!("plate.{unknown}"),
            ));
        }
        let mut plate = WorldPlate::default();
        if let Some(size) = param(v, "size") {
            let [w, h] = floats::<2>(size, "plate.size")?;
            if w <= 0.0 || h <= 0.0 {
                return Err(bad_param("`plate.size` must be positive"));
            }
            plate.size = [w, h];
        }
        if let Some(ppm) = param(v, "ppm") {
            plate.ppm = positive(ppm, "plate.ppm")?;
        }
        if let Some(depth) = bool_param(v, "depth_test")? {
            plate.depth_test = depth;
        }
        plate.fade = fade_param(v)?.or(plate.fade);
        marker.plate = Some(plate);
    }
    Ok(marker)
}

/// The lock `ui.target_bracket` would put on a target, and the `shooter` it names (checked to
/// exist only when the lock is placed).
fn lock_of(params: &Value) -> Result<(TargetLock, Option<Entity>), RpcError> {
    let mut lock = TargetLock {
        label: string_param(params, "label")?.unwrap_or_default(),
        ..TargetLock::default()
    };
    if let Some(v) = param(params, "radius") {
        lock.radius_m = positive(v, "radius")?;
    }
    if let Some(v) = param(params, "speed") {
        lock.projectile_speed = Some(positive(v, "speed")?);
    }
    if let Some(v) = param(params, "gravity") {
        lock.gravity = finite(v, "gravity")?.max(0.0);
    }
    if let Some(v) = param(params, "velocity") {
        lock.velocity = Some(floats::<3>(v, "velocity")?);
    }
    if let Some(v) = param(params, "shooter_velocity") {
        lock.shooter_velocity = Some(floats::<3>(v, "shooter_velocity")?);
    }
    if let Some(v) = param(params, "color") {
        lock.color = color_of(v, "color")?;
    }
    Ok((lock, entity_bits(params, "shooter")?))
}

fn layer_of(world: &mut World, params: &Value) -> Result<Option<Entity>, RpcError> {
    if param(params, "entity").is_some() {
        return widget_of::<MarkerLayer>(world, params, "marker layer").map(Some);
    }
    Ok(world
        .query_filtered::<Entity, With<MarkerLayer>>()
        .iter(world)
        .min_by_key(|e| e.to_bits()))
}

fn remove_marker(world: &mut World, target: Entity) {
    if world.get::<AgentAnchor>(target).is_some() && world.get::<TargetLock>(target).is_none() {
        world.despawn(target);
    } else {
        world.entity_mut(target).remove::<WorldMarker>();
    }
}

fn target_param(world: &World, place: Placement, what: &str) -> Result<Entity, RpcError> {
    let target = place
        .target
        .ok_or_else(|| bad_param(format!("{what} needs `target` (entity bits)")))?;
    if world.get_entity(target).is_err() {
        return Err(err(
            NOT_FOUND,
            "unknown_entity",
            "the target does not exist",
            "take entity ids from the reply's lists",
        ));
    }
    Ok(target)
}

/// `ui.world_markers { entity?, action?, target?, position?, label?, color?, size?, offset?,
/// clamp_to_edge?, show_distance?, fade?, icon?, plate? }`. Every parameter is checked under every
/// action (ADR-5957): an unknown key or a wrong value is refused even by `query`.
pub(crate) fn world_markers(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_known_and_types("ui.world_markers", params, &["color"])?;
    let action = action_of(params)?;
    let marker = marker_of(params)?;
    let place = placement_of(params)?;
    let layer = layer_of(world, params)?;
    let mut added = None;
    match action {
        "query" => {}
        "add" => {
            let target = target_or_anchor(world, place)?;
            world.entity_mut(target).insert(marker);
            added = Some(target.to_bits());
        }
        "remove" => {
            let target = target_param(world, place, "remove")?;
            if world.get::<WorldMarker>(target).is_none() {
                return Err(err(
                    NOT_FOUND,
                    "no_game_widget",
                    "the target has no world marker",
                    "the reply's markers list the marked entities",
                ));
            }
            remove_marker(world, target);
        }
        "clear" => {
            let marked: Vec<Entity> = world
                .query_filtered::<Entity, With<WorldMarker>>()
                .iter(world)
                .collect();
            for target in marked {
                remove_marker(world, target);
            }
        }
        other => {
            return Err(bad_action("ui.world_markers", other, "query, add, remove, clear"));
        }
    }
    let state = layer.and_then(|l| world.get::<MarkerLayerState>(l)).cloned();
    let markers: Vec<Value> = state
        .iter()
        .flat_map(|s| s.markers.iter())
        .map(|m| {
            json!({
                "source": m.source.to_bits(),
                "node": m.node.to_bits(),
                "pos": m.pos,
                "on_screen": m.on_screen,
                "clamped": m.clamped,
                "behind": m.behind,
                "distance": m.distance,
                "alpha": m.alpha,
                "visible": m.visible,
            })
        })
        .collect();
    let plates: Vec<Value> = world
        .query::<(Entity, &WorldPlateRoot, &GlobalTransform)>()
        .iter(world)
        .map(|(e, p, g)| {
            json!({
                "root": e.to_bits(),
                "source": p.source.to_bits(),
                "position": Vec3::from(g.0.translation).to_array(),
            })
        })
        .collect();
    Ok(json!({
        "entity": layer.map(Entity::to_bits),
        "camera": state.as_ref().and_then(|s| s.camera).map(Entity::to_bits),
        "size": state.as_ref().map_or([0.0, 0.0], |s| s.size),
        "markers": markers,
        "plates": plates,
        "added": added,
    }))
}

/// `ui.target_bracket { entity?, action?, target?, position?, radius?, speed?, gravity?, velocity?,
/// shooter?, shooter_velocity?, label?, color? }`. Every parameter is checked under every action
/// (ADR-5957): an unknown key or a wrong value is refused even by `query`.
pub(crate) fn target_bracket(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_known_and_types("ui.target_bracket", params, &["color"])?;
    let action = action_of(params)?;
    let (mut lock, shooter) = lock_of(params)?;
    let place = placement_of(params)?;
    let layer = layer_of(world, params)?;
    let mut locked = None;
    match action {
        "query" => {}
        "lock" => {
            if let Some(shooter) = shooter {
                if world.get_entity(shooter).is_err() {
                    return Err(bad_param("`shooter` must be the bits of an existing entity"));
                }
                lock.shooter = Some(shooter);
            }
            let target = target_or_anchor(world, place)?;
            world.entity_mut(target).insert(lock);
            locked = Some(target.to_bits());
        }
        "release" => {
            let target = target_param(world, place, "release")?;
            if world.get::<TargetLock>(target).is_none() {
                return Err(err(
                    NOT_FOUND,
                    "no_game_widget",
                    "the target has no bracket",
                    "the reply's brackets list the locked entities",
                ));
            }
            if world.get::<AgentAnchor>(target).is_some() && world.get::<WorldMarker>(target).is_none() {
                world.despawn(target);
            } else {
                world.entity_mut(target).remove::<TargetLock>();
            }
        }
        other => return Err(bad_action("ui.target_bracket", other, "query, lock, release")),
    }
    let state = layer.and_then(|l| world.get::<MarkerLayerState>(l)).cloned();
    let brackets: Vec<Value> = state
        .iter()
        .flat_map(|s| s.brackets.iter())
        .map(|b| {
            json!({
                "target": b.target.to_bits(),
                "node": b.node.to_bits(),
                "center": b.center,
                "half_px": b.half_px,
                "on_screen": b.on_screen,
                "distance": b.distance,
                "velocity": b.velocity,
                "lead_world": b.lead_world,
                "lead_screen": b.lead_screen,
                "time_to_intercept": b.time_to_intercept,
                "no_solution": b.no_solution,
            })
        })
        .collect();
    Ok(json!({
        "entity": layer.map(Entity::to_bits),
        "brackets": brackets,
        "locked": locked,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use regia_app::prelude::*;
    use regia_render::Camera;
    use regia_ui::UiPlugin;
    use regia_ui::game_widgets::{LobbyListRequest, LobbyRow, RadarBlip, ScoreTable};
    use regia_ui::prelude::*;

    use super::*;

    fn code_of(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or("")
            .to_owned()
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, UiPlugin::code_only()));
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        app
    }

    fn root(app: &mut App) -> Entity {
        app.world_mut()
            .spawn((
                UiRoot {
                    scale_mode: ScaleMode::ConstantPixelSize,
                    ..Default::default()
                },
                NodeStyle::new(Style {
                    size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                    ..Default::default()
                }),
            ))
            .id()
    }

    fn node(app: &mut App, parent: Entity, w: f32, h: f32, bundle: impl Bundle) -> Entity {
        let n = app
            .world_mut()
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Points(w), Dimension::Points(h)],
                    ..Default::default()
                }),
                bundle,
            ))
            .id();
        app.world_mut().entity_mut(parent).add_child(n);
        n
    }

    #[test]
    fn every_method_says_when_its_widget_is_missing() {
        let mut app = app();
        for call in [lobby_list, scoreboard, radar] {
            let e = call(app.world_mut(), &json!({})).unwrap_err();
            assert_eq!(code_of(&e), "no_game_widget");
        }
        // Marker methods answer with empty lists: markers may exist before the layer does.
        let answer = world_markers(app.world_mut(), &json!({})).unwrap();
        assert_eq!(answer["entity"], Value::Null);
        assert!(answer["markers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_named_entity_must_be_that_widget() {
        let mut app = app();
        let r = root(&mut app);
        let other = node(&mut app, r, 10.0, 10.0, ());
        let e = radar(app.world_mut(), &json!({ "entity": other.to_bits() })).unwrap_err();
        assert_eq!(code_of(&e), "no_game_widget");
        // A value that is not entity bits is refused by the typed parameter reader (integ's
        // `params::opt_u64`, which answers before the widget lookup's own `bad_entity`).
        let e = radar(app.world_mut(), &json!({ "entity": "no" })).unwrap_err();
        assert_eq!(code_of(&e), "F0002");
        assert_eq!(e.data.as_ref().and_then(|d| d["param"].as_str()), Some("entity"));
    }

    #[test]
    fn the_lobby_list_refuses_actions_without_a_driver_and_sends_them_with_one() {
        let mut app = app();
        let r = root(&mut app);
        let list = node(
            &mut app,
            r,
            400.0,
            300.0,
            LobbyList {
                rows: vec![LobbyRow {
                    id: u64::MAX,
                    name: "big id".into(),
                    players: 1,
                    max_players: 2,
                    joinable: true,
                    ..Default::default()
                }],
                manual: true,
                ..Default::default()
            },
        );
        let q = lobby_list(app.world_mut(), &json!({})).unwrap();
        assert_eq!(q["rows"][0]["id"], u64::MAX.to_string());
        assert_eq!(q["status"], "idle");
        let e = lobby_list(app.world_mut(), &json!({ "action": "refresh" })).unwrap_err();
        assert_eq!(code_of(&e), "no_lobby_driver");
        app.world_mut()
            .insert_resource(LobbyListDriver { name: "test".into() });
        let sent = lobby_list(
            app.world_mut(),
            &json!({ "action": "join", "lobby": u64::MAX.to_string() }),
        )
        .unwrap();
        assert_eq!(sent["sent"], "join");
        let mut messages = app.world_mut().resource_mut::<Messages<LobbyListRequest>>();
        let got: Vec<LobbyListRequest> = messages.drain().collect();
        assert_eq!(
            got,
            [LobbyListRequest {
                list,
                action: LobbyListAction::Join(u64::MAX)
            }]
        );
        let e = lobby_list(app.world_mut(), &json!({ "action": "join" })).unwrap_err();
        assert_eq!(code_of(&e), "bad_param");
        let e = lobby_list(app.world_mut(), &json!({ "action": "dance" })).unwrap_err();
        assert_eq!(code_of(&e), "bad_action");
    }

    #[test]
    fn the_scoreboard_sets_sorts_and_reports_in_draw_order() {
        let mut app = app();
        let r = root(&mut app);
        let b = node(
            &mut app,
            r,
            400.0,
            300.0,
            Scoreboard {
                table: ScoreTable::default(),
                ..Scoreboard::default()
            },
        );
        let set = scoreboard(
            app.world_mut(),
            &json!({
                "action": "set",
                "columns": [{ "key": "tags", "title": "Tags" }],
                "rows": [
                    { "id": 1, "name": "Low", "values": [1] },
                    { "id": 2, "name": "High", "values": [9], "local": true },
                ],
            }),
        )
        .unwrap();
        assert_eq!(set["from_rules"], false);
        assert_eq!(set["lines"][0]["name"], "High");
        assert_eq!(set["lines"][0]["values"][0], 9);
        let flipped = scoreboard(app.world_mut(), &json!({ "action": "sort", "column": 0 })).unwrap();
        assert_eq!(flipped["sort"]["descending"], false);
        assert_eq!(flipped["lines"][0]["name"], "Low");
        let e = scoreboard(app.world_mut(), &json!({ "action": "sort", "column": 3 })).unwrap_err();
        assert_eq!(code_of(&e), "bad_param");
        // A failed set changes nothing.
        let e = scoreboard(
            app.world_mut(),
            &json!({ "action": "set", "columns": [], "rows": [{ "name": "no id" }] }),
        )
        .unwrap_err();
        assert_eq!(code_of(&e), "bad_param");
        assert_eq!(app.world().get::<Scoreboard>(b).unwrap().table.columns.len(), 1);
        scoreboard(app.world_mut(), &json!({ "action": "hide" })).unwrap();
        app.update();
        let hidden = scoreboard(app.world_mut(), &json!({})).unwrap();
        assert_eq!(hidden["shown"], false);
        let e = scoreboard(app.world_mut(), &json!({ "action": "x" })).unwrap_err();
        assert_eq!(code_of(&e), "bad_action");
    }

    #[test]
    fn the_scoreboard_reports_only_what_it_can_draw() {
        let mut app = app();
        let r = root(&mut app);
        node(&mut app, r, 400.0, 400.0, Scoreboard::default());
        let rows: Vec<Value> = (0..300).map(|i| json!({ "id": i, "name": "p" })).collect();
        let reply = scoreboard(
            app.world_mut(),
            &json!({ "action": "set", "columns": [{ "key": "k" }], "rows": rows }),
        )
        .unwrap();
        assert_eq!(reply["lines"].as_array().unwrap().len(), MAX_SCORE_ROWS);
        app.update();
        let reply = scoreboard(app.world_mut(), &json!({})).unwrap();
        assert_eq!(reply["dropped"], 300 - MAX_SCORE_ROWS);
    }

    #[test]
    fn the_radar_reads_its_blips_and_validates_a_set() {
        let mut app = app();
        let r = root(&mut app);
        node(&mut app, r, 200.0, 200.0, Radar::with_range(50.0));
        let blip = app
            .world_mut()
            .spawn((
                RadarBlip {
                    label: "ally".into(),
                    ..RadarBlip::default()
                },
                GlobalTransform(Affine3A::from_translation(Vec3::new(25.0, 0.0, 0.0))),
            ))
            .id();
        for _ in 0..3 {
            app.update();
        }
        let q = radar(app.world_mut(), &json!({})).unwrap();
        assert_eq!(q["mode"], "radar");
        assert_eq!(q["blips"][0]["source"], blip.to_bits());
        assert_eq!(q["blips"][0]["label"], "ally");
        assert_eq!(q["blips"][0]["pos"][0], 150.0);
        for bad in [
            json!({ "action": "set", "range": 0 }),
            json!({ "action": "set", "world_rect": [1, 1, 0, 0] }),
            json!({ "action": "set", "range": 5, "world_rect": [0, 0, 1, 1] }),
            json!({ "action": "set", "max_blips": 99999 }),
            json!({ "action": "set", "center": [1] }),
        ] {
            let e = radar(app.world_mut(), &bad).unwrap_err();
            assert_eq!(code_of(&e), "bad_param", "{bad}");
        }
        let map = radar(
            app.world_mut(),
            &json!({ "action": "set", "world_rect": [0, 0, 100, 100] }),
        )
        .unwrap();
        assert_eq!(map["mode"], "minimap");
        assert_eq!(map["shape"], "rect", "a minimap covers its whole area: {map}");
        // A corner of that area is inside the minimap.
        let corner = app
            .world_mut()
            .spawn((
                RadarBlip::default(),
                GlobalTransform(Affine3A::from_translation(Vec3::new(95.0, 0.0, 95.0))),
            ))
            .id();
        for _ in 0..2 {
            app.update();
        }
        let q = radar(app.world_mut(), &json!({})).unwrap();
        let shown = q["blips"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["source"] == corner.to_bits())
            .unwrap_or_else(|| panic!("the corner is shown: {q}"));
        assert_eq!(shown["inside"], true, "{q}");
        // A number too large for the UI's floats is refused, not stored as infinity.
        let e = radar(app.world_mut(), &json!({ "action": "set", "range": 1e39 })).unwrap_err();
        assert_eq!(code_of(&e), "bad_param");
        let e = radar(app.world_mut(), &json!({ "action": "spin" })).unwrap_err();
        assert_eq!(code_of(&e), "bad_action");
    }

    #[test]
    fn markers_and_brackets_go_on_targets_or_bare_positions() {
        let mut app = app();
        let r = root(&mut app);
        node(&mut app, r, 800.0, 450.0, MarkerLayer::default());
        app.world_mut()
            .spawn((Camera::default(), GlobalTransform(Affine3A::IDENTITY)));
        let added = world_markers(
            app.world_mut(),
            &json!({ "action": "add", "position": [0, 0, -10], "label": "Objective" }),
        )
        .unwrap();
        let anchor = Entity::try_from_bits(added["added"].as_u64().unwrap()).unwrap();
        for _ in 0..3 {
            app.update();
        }
        let q = world_markers(app.world_mut(), &json!({})).unwrap();
        assert_eq!(q["markers"][0]["on_screen"], true);
        assert_eq!(q["markers"][0]["pos"][0], 400.0);
        assert!(q["camera"].is_u64());

        let lock = target_bracket(
            app.world_mut(),
            &json!({ "action": "lock", "target": anchor.to_bits(), "speed": 100, "velocity": [5, 0, 0] }),
        )
        .unwrap();
        assert_eq!(lock["locked"], anchor.to_bits());
        for _ in 0..2 {
            app.update();
        }
        let b = target_bracket(app.world_mut(), &json!({})).unwrap();
        let t = b["brackets"][0]["time_to_intercept"].as_f64().unwrap();
        assert!((t - 0.1).abs() < 0.01, "{t}");
        assert!(b["brackets"][0]["lead_screen"].is_array());
        // An anchor with a lock survives losing its marker, and goes with the last of them.
        world_markers(
            app.world_mut(),
            &json!({ "action": "remove", "target": anchor.to_bits() }),
        )
        .unwrap();
        assert!(app.world().get_entity(anchor).is_ok());
        target_bracket(
            app.world_mut(),
            &json!({ "action": "release", "target": anchor.to_bits() }),
        )
        .unwrap();
        assert!(app.world().get_entity(anchor).is_err());
        // Errors.
        let e = world_markers(app.world_mut(), &json!({ "action": "add" })).unwrap_err();
        assert_eq!(code_of(&e), "bad_param");
        let bare = app.world_mut().spawn_empty().id();
        let e = world_markers(
            app.world_mut(),
            &json!({ "action": "add", "target": bare.to_bits() }),
        )
        .unwrap_err();
        assert_eq!(code_of(&e), "no_world_position");
        let e = target_bracket(
            app.world_mut(),
            &json!({ "action": "lock", "position": [0, 0, 0], "speed": -1 }),
        )
        .unwrap_err();
        assert_eq!(code_of(&e), "bad_param");
        let e = target_bracket(
            app.world_mut(),
            &json!({ "action": "release", "target": bare.to_bits() }),
        )
        .unwrap_err();
        assert_eq!(code_of(&e), "no_game_widget");
        // `clear` removes every marker, and a position marker's anchor with it.
        let added = world_markers(
            app.world_mut(),
            &json!({ "action": "add", "position": [1, 0, -5], "plate": { "size": [64, 16] } }),
        )
        .unwrap();
        let second = Entity::try_from_bits(added["added"].as_u64().unwrap()).unwrap();
        app.update();
        let q = world_markers(app.world_mut(), &json!({})).unwrap();
        assert_eq!(q["plates"][0]["source"], second.to_bits());
        world_markers(app.world_mut(), &json!({ "action": "clear" })).unwrap();
        assert!(app.world().get_entity(second).is_err());
    }

    /// ADR-5957: the default `query` action used to skip every parameter but `entity` and `action`,
    /// so a misspelt or wrong-typed one was accepted silently. Every action now checks them all.
    #[test]
    fn markers_and_brackets_check_every_parameter_under_query_too() {
        let mut app = app();
        let r = root(&mut app);
        node(&mut app, r, 800.0, 450.0, MarkerLayer::default());
        let count = |app: &mut App| {
            let mut all = app.world_mut().query::<Entity>();
            all.iter(app.world()).count()
        };
        let entities = count(&mut app);
        // Unknown keys and wrong JSON types name the parameter in `data.param`.
        for (params, named) in [
            (json!({ "label": 5 }), "label"),
            (json!({ "size": "4" }), "size"),
            (json!({ "clamp_to_edge": "yes" }), "clamp_to_edge"),
            (json!({ "target": "1" }), "target"),
            (json!({ "position": "here" }), "position"),
            (json!({ "plate": "big" }), "plate"),
            (json!({ "lable": "A" }), "lable"),
            (json!({ "plate": { "sise": [1, 1] } }), "plate.sise"),
        ] {
            let e = world_markers(app.world_mut(), &params).unwrap_err();
            assert_eq!(e.code, INVALID_PARAMS, "{params}");
            assert_eq!(e.data.as_ref().unwrap()["param"], named, "{params}");
        }
        for (params, named) in [
            (json!({ "speed": "fast" }), "speed"),
            (json!({ "shooter": "me" }), "shooter"),
            (json!({ "velocity": 3 }), "velocity"),
            (json!({ "label": true }), "label"),
            (json!({ "lead": true }), "lead"),
        ] {
            let e = target_bracket(app.world_mut(), &params).unwrap_err();
            assert_eq!(e.code, INVALID_PARAMS, "{params}");
            assert_eq!(e.data.as_ref().unwrap()["param"], named, "{params}");
        }
        // Values of the right JSON type that the action would refuse are refused by `query` too.
        for params in [
            json!({ "size": -1 }),
            json!({ "position": [1, 2] }),
            json!({ "color": "red" }),
            json!({ "color": 7 }),
            json!({ "fade": [1] }),
            json!({ "target": -1 }),
            json!({ "plate": { "size": [0, 1] } }),
        ] {
            let e = world_markers(app.world_mut(), &params).unwrap_err();
            assert_eq!(e.code, INVALID_PARAMS, "{params}");
        }
        for params in [
            json!({ "radius": 0 }),
            json!({ "velocity": [1] }),
            json!({ "shooter": -5 }),
            json!({ "color": [1, 0] }),
            json!({ "action": "release", "position": [0, 0] }),
        ] {
            let e = target_bracket(app.world_mut(), &params).unwrap_err();
            assert_eq!(e.code, INVALID_PARAMS, "{params}");
        }
        assert_eq!(count(&mut app), entities, "a refused call spawns nothing");
        // Valid parameters an action does not use still pass, and so does the server's `timeout_ms`.
        world_markers(
            app.world_mut(),
            &json!({ "timeout_ms": 50, "label": "A", "color": [1, 0, 0, 1], "plate": { "ppm": 64 } }),
        )
        .unwrap();
        target_bracket(
            app.world_mut(),
            &json!({ "timeout_ms": 50, "speed": 300, "color": "#FF0000", "shooter": null }),
        )
        .unwrap();
    }

    /// GR-1.3: a scoreboard showing the rules reads the hosted match session through
    /// `MatchSessionRules`: each team heading carries the board score, and each seat's ledger row
    /// is a player line.
    #[test]
    fn scoreboard_from_match_session_lists_team_scores_and_player_rows() {
        use regia_framework::rules::match_flow::SeatId;
        use regia_framework::rules::teams::SeatRequest;
        use regia_framework::rules::{GameRulesResource, MatchRules, MatchSessionPlugin, MatchSessionRules};

        let mut app = app();
        app.add_plugins((regia_framework::game_ui::GameUiBridgePlugin, MatchSessionPlugin));
        app.insert_resource(GameRulesResource {
            rules: Box::new(MatchSessionRules),
        });
        let r = root(&mut app);
        node(&mut app, r, 400.0, 300.0, Scoreboard::default());
        {
            let mut rules = app.world_mut().resource_mut::<MatchRules>();
            let session = &mut rules.session;
            for team in [1u8, 2, 1] {
                let request = SeatRequest {
                    team: Some(team),
                    ..SeatRequest::default()
                };
                session.assign_team(&request, &[]).unwrap();
            }
            session.credit_kill(SeatId::new(1, 0), SeatId::new(2, 0));
            session.credit_kill(SeatId::new(1, 1), SeatId::new(2, 0));
            session.credit_kill(SeatId::new(1, 0), SeatId::new(2, 0));
            session.board.team_scores = vec![0, 2, 1, 0, 0];
        }
        app.update();
        let answer = scoreboard(app.world_mut(), &json!({ "action": "query" })).unwrap();
        assert_eq!(answer["from_rules"], true);
        let lines = answer["lines"].as_array().unwrap();
        let team_name = |id: u64| {
            lines
                .iter()
                .find(|line| line["kind"] == "team" && line["team"] == id)
                .map(|line| line["name"].clone())
        };
        assert_eq!(team_name(1), Some(json!("Team 1: 2")));
        assert_eq!(team_name(2), Some(json!("Team 2: 1")));
        let player = |id: u64| {
            lines
                .iter()
                .find(|line| line["kind"] == "player" && line["id"] == id)
                .cloned()
                .unwrap()
        };
        // A row's id is its ledger row's id (`generation << 16 | team << 8 | slot`, ADR-8657), so
        // it is read from the session rather than written as `team << 8 | slot`.
        let seat_row = |team: u8, slot: u8| {
            app.world()
                .resource::<MatchRules>()
                .session
                .ledgers
                .rows
                .iter()
                .find(|row| row.team == team && row.slot == slot)
                .map(|row| row.id)
                .unwrap()
        };
        // Columns: score, kills, deaths, assists, objective.
        let lead = player(seat_row(1, 0));
        assert_eq!(lead["team"], 1);
        assert_eq!(lead["values"][0], 2);
        assert_eq!(lead["values"][1], 2);
        assert_eq!(player(seat_row(1, 1))["values"][1], 1);
        assert_eq!(
            player(seat_row(2, 0))["values"][2],
            3,
            "the victim row has three deaths"
        );
        let players = lines.iter().filter(|line| line["kind"] == "player").count();
        assert_eq!(players, 3);
        let columns: Vec<&str> = answer["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|column| column["key"].as_str().unwrap())
            .collect();
        assert_eq!(columns, ["score", "kills", "deaths", "assists", "objective"]);
    }
}
