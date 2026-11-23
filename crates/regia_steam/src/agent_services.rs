// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of Steam leaderboards, app and DLC ownership and the game-recording
//! timeline (spec 24 § 2a, ADR-2340, `book/agents/steam-services.md`):
//!
//! - `steam.leaderboard_find`, `steam.leaderboard_upload`, `steam.leaderboard_download`,
//!   `steam.leaderboard_attach_ugc`, `steam.leaderboard_entries`, `steam.leaderboard_list` act on
//!   the framework's `Leaderboard` resource (the same object the game uses), so they work against
//!   Steam, the mock and the local service alike. Work is queued: step a frame (or `wait_until`)
//!   and read `steam.leaderboard_entries`.
//! - `steam.owns_app` reads ISteamApps ownership for one app id.
//! - `steam.timeline_mode`, `steam.timeline_state`, `steam.timeline_event` mark the recording;
//!   `steam.timeline` reads back what the engine sent.
//!
//! Errors are `F0002` with `data.reason`, one of `ERROR_CODES`.

use serde_json::{Value, json};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;
use regia_framework::leaderboard::{
    BoardState, Leaderboard, LeaderboardDisplay, LeaderboardEntry, LeaderboardError, LeaderboardRange,
    LeaderboardSort, LeaderboardSpec, UgcHandle, UploadMethod, UploadOutcome,
};

use crate::backend::{AppId, SteamError};
use crate::services::{TimelineClip, TimelineEvent, TimelineMode};
use crate::{Steam, SteamState};

/// Every `data.reason` these methods answer with (documented on `book/agents/steam-services.md`).
pub(crate) const ERROR_CODES: &[&str] = &[
    "bad_value",
    "leaderboard_not_running",
    "leaderboard_unavailable",
    "name_invalid",
    "leaderboard_not_found",
    "details_too_long",
    "range_invalid",
    "no_entry",
    "ugc_unsupported",
    "leaderboard_platform",
    "steam_unavailable",
    "steam_rejected",
    "app_id_invalid",
];

fn f0002(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    debug_assert!(ERROR_CODES.contains(&reason), "undocumented error code {reason}");
    let mut e = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    e
}

fn bad(message: impl Into<String>) -> RpcError {
    f0002(
        "bad_value",
        message,
        "see book/agents/steam-services.md for the parameters",
    )
}

fn leaderboard_error(e: &LeaderboardError) -> RpcError {
    let hint = match e {
        LeaderboardError::Unavailable => "Steam is not running or SteamLeaderboardPlugin is missing",
        LeaderboardError::NameInvalid => "board names are 1..=128 bytes without NUL",
        LeaderboardError::DetailsTooLong(_) => "at most 64 detail values",
        LeaderboardError::RangeInvalid(_) => "global needs 1 <= start <= end; at most 1000 rows",
        _ => "see book/agents/steam-services.md",
    };
    f0002(e.code(), e.to_string(), hint)
}

fn steam_error(e: &SteamError) -> RpcError {
    match e {
        SteamError::Unavailable => f0002(
            "steam_unavailable",
            e.to_string(),
            "Steam is not running (steam.status shows running: false)",
        ),
        _ => f0002(
            "steam_rejected",
            e.to_string(),
            "check the values against the limits on the book page",
        ),
    }
}

fn not_running() -> RpcError {
    f0002(
        "leaderboard_not_running",
        "the app has no Leaderboard resource",
        "add regia_steam::SteamLeaderboardPlugin (or regia_framework::leaderboard::LeaderboardPlugin)",
    )
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(bad(format!("`{name}` must be a string, got {other}"))),
    }
}

fn required_str<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    str_param(params, name)?.ok_or_else(|| bad(format!("`{name}` (string) is required")))
}

fn int_param(params: &Value, name: &str) -> Result<Option<i64>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .map(Some)
            .ok_or_else(|| bad(format!("`{name}` must be an integer, got {v}"))),
    }
}

fn u32_param(params: &Value, name: &str) -> Result<Option<u32>, RpcError> {
    int_param(params, name)?
        .map(|v| u32::try_from(v).map_err(|_| bad(format!("`{name}` must be in 0..=4294967295, got {v}"))))
        .transpose()
}

fn f32_param(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|f| Some(f as f32))
            .ok_or_else(|| bad(format!("`{name}` must be a number, got {v}"))),
    }
}

/// A 64-bit id given as a decimal string or an integer.
fn u64_param(params: &Value, name: &str) -> Result<Option<u64>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => s
            .parse::<u64>()
            .map(Some)
            .map_err(|_| bad(format!("`{name}` must be a 64-bit unsigned id, got `{s}`"))),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| bad(format!("`{name}` must be a 64-bit unsigned id, got {v}"))),
    }
}

fn bool_param(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(bad(format!("`{name}` must be a boolean, got {other}"))),
    }
}

fn entry_json(e: &LeaderboardEntry) -> Value {
    json!({
        "user": e.user.to_string(),
        "rank": e.rank,
        "score": e.score,
        "details": e.details,
        "ugc": e.ugc.map(|u| u.0.to_string()),
    })
}

fn outcome_json(o: &UploadOutcome) -> Value {
    json!({
        "score": o.score,
        "changed": o.changed,
        "rank_new": o.rank_new,
        "rank_previous": o.rank_previous,
    })
}

fn range_json(range: LeaderboardRange) -> Value {
    match range {
        LeaderboardRange::Global { start, end } => json!({ "kind": "global", "start": start, "end": end }),
        LeaderboardRange::AroundUser { before, after } => {
            json!({ "kind": "around_user", "before": before, "after": after })
        }
        LeaderboardRange::Friends => json!({ "kind": "friends" }),
    }
}

fn board_json(name: &str, state: &BoardState) -> Value {
    json!({
        "name": name,
        "lookup": state.lookup.name(),
        "handle": state.info.as_ref().map(|i| i.handle.0.to_string()),
        "sort": state.info.as_ref().map_or(state.spec.sort, |i| i.sort).name(),
        "display": state.info.as_ref().map_or(state.spec.display, |i| i.display).name(),
        "method": state.spec.method.name(),
        "create": state.spec.create,
        "entry_count": state.info.as_ref().map(|i| i.entry_count),
        "pending": state.pending,
        "last_upload": state.last_upload.as_ref().map(outcome_json),
        "attached_ugc": state.attached_ugc.map(|u| u.0.to_string()),
        "error": state.last_error.as_ref().map(|e| json!({ "code": e.code(), "message": e.to_string() })),
    })
}

fn boards_mut(world: &mut World) -> Result<Mut<'_, Leaderboard>, RpcError> {
    world.get_resource_mut::<Leaderboard>().ok_or_else(not_running)
}

const NAME: ParamSchema = ParamSchema {
    name: "name",
    kind: "string",
    required: true,
    description: "the board name (1..=128 bytes)",
};

fn find_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.leaderboard_find",
        tool: "steam_leaderboard_find",
        description: "Declare a leaderboard and look it up (create it when `create`); answers arrive on later frames.",
        params: &[
            NAME,
            ParamSchema {
                name: "create",
                kind: "boolean",
                required: false,
                description: "create the board when missing (default false)",
            },
            ParamSchema {
                name: "sort",
                kind: "string",
                required: false,
                description: "ascending or descending (default) when created",
            },
            ParamSchema {
                name: "display",
                kind: "string",
                required: false,
                description: "numeric (default), time_seconds or time_milliseconds when created",
            },
            ParamSchema {
                name: "method",
                kind: "string",
                required: false,
                description: "keep_best (default) or force_update for later uploads",
            },
        ],
        result: "{ service, board }",
        mutating: true,
        permission: Permission::Play,
    }
}

fn steam_leaderboard_find(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = required_str(params, "name")?;
    let mut spec = LeaderboardSpec::new(name);
    spec.create = bool_param(params, "create")?.unwrap_or(false);
    if let Some(sort) = str_param(params, "sort")? {
        spec.sort = LeaderboardSort::from_name(sort)
            .ok_or_else(|| bad(format!("unknown sort `{sort}`: use ascending or descending")))?;
    }
    if let Some(display) = str_param(params, "display")? {
        spec.display = LeaderboardDisplay::from_name(display).ok_or_else(|| {
            bad(format!(
                "unknown display `{display}`: use numeric, time_seconds or time_milliseconds"
            ))
        })?;
    }
    if let Some(method) = str_param(params, "method")? {
        spec.method = UploadMethod::from_name(method).ok_or_else(|| {
            bad(format!(
                "unknown method `{method}`: use keep_best or force_update"
            ))
        })?;
    }
    let mut boards = boards_mut(world)?;
    boards.declare(spec).map_err(|e| leaderboard_error(&e))?;
    boards.find(name).map_err(|e| leaderboard_error(&e))?;
    let board = boards.board(name).map(|s| board_json(name, s));
    Ok(json!({ "service": boards.service_name(), "board": board }))
}

fn upload_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.leaderboard_upload",
        tool: "steam_leaderboard_upload",
        description: "Queue the local user's score upload (keep_best or force_update); read the outcome with steam.leaderboard_entries.",
        params: &[
            NAME,
            ParamSchema {
                name: "score",
                kind: "integer",
                required: true,
                description: "the score (i32)",
            },
            ParamSchema {
                name: "details",
                kind: "array",
                required: false,
                description: "up to 64 game-defined i32 values",
            },
            ParamSchema {
                name: "method",
                kind: "string",
                required: false,
                description: "keep_best or force_update (default: the board's declared method)",
            },
        ],
        result: "{ service, board }",
        mutating: true,
        permission: Permission::Play,
    }
}

fn steam_leaderboard_upload(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = required_str(params, "name")?;
    let score = int_param(params, "score")?.ok_or_else(|| bad("`score` (integer) is required"))?;
    let score = i32::try_from(score).map_err(|_| bad(format!("`score` must fit in i32, got {score}")))?;
    let details: Vec<i32> = match params.get("details") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(values)) => values
            .iter()
            .map(|v| {
                v.as_i64()
                    .and_then(|i| i32::try_from(i).ok())
                    .ok_or_else(|| bad(format!("`details` values must be i32 integers, got {v}")))
            })
            .collect::<Result<_, _>>()?,
        Some(other) => return Err(bad(format!("`details` must be an array, got {other}"))),
    };
    let method = str_param(params, "method")?
        .map(|m| {
            UploadMethod::from_name(m)
                .ok_or_else(|| bad(format!("unknown method `{m}`: use keep_best or force_update")))
        })
        .transpose()?;
    let mut boards = boards_mut(world)?;
    let queued = match method {
        Some(method) => boards.submit_with(name, score, &details, method),
        None => boards.submit(name, score, &details),
    };
    queued.map_err(|e| leaderboard_error(&e))?;
    let board = boards.board(name).map(|s| board_json(name, s));
    Ok(json!({ "service": boards.service_name(), "board": board }))
}

fn download_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.leaderboard_download",
        tool: "steam_leaderboard_download",
        description: "Queue a download of global ranks, rows around the local user, or friends; read rows with steam.leaderboard_entries.",
        params: &[
            NAME,
            ParamSchema {
                name: "range",
                kind: "string",
                required: false,
                description: "global (default), around_user or friends",
            },
            ParamSchema {
                name: "start",
                kind: "integer",
                required: false,
                description: "global: first rank, 1-based (default 1)",
            },
            ParamSchema {
                name: "end",
                kind: "integer",
                required: false,
                description: "global: last rank (default start + 9)",
            },
            ParamSchema {
                name: "before",
                kind: "integer",
                required: false,
                description: "around_user: rows above the user (default 5)",
            },
            ParamSchema {
                name: "after",
                kind: "integer",
                required: false,
                description: "around_user: rows below the user (default 5)",
            },
        ],
        result: "{ service, board, range }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn parse_range(params: &Value) -> Result<LeaderboardRange, RpcError> {
    let kind = str_param(params, "range")?.unwrap_or("global");
    Ok(match kind {
        "global" => {
            let start = u32_param(params, "start")?.unwrap_or(1);
            let end = u32_param(params, "end")?.unwrap_or_else(|| start.saturating_add(9));
            LeaderboardRange::Global { start, end }
        }
        "around_user" => LeaderboardRange::AroundUser {
            before: u32_param(params, "before")?.unwrap_or(5),
            after: u32_param(params, "after")?.unwrap_or(5),
        },
        "friends" => LeaderboardRange::Friends,
        other => {
            return Err(bad(format!(
                "unknown range `{other}`: use global, around_user or friends"
            )));
        }
    })
}

fn steam_leaderboard_download(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = required_str(params, "name")?;
    let range = parse_range(params)?;
    let mut boards = boards_mut(world)?;
    boards.request(name, range).map_err(|e| leaderboard_error(&e))?;
    let board = boards.board(name).map(|s| board_json(name, s));
    Ok(json!({ "service": boards.service_name(), "board": board, "range": range_json(range) }))
}

fn attach_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.leaderboard_attach_ugc",
        tool: "steam_leaderboard_attach_ugc",
        description: "Queue attaching a shared UGC file handle to the local user's entry (the entry must exist).",
        params: &[
            NAME,
            ParamSchema {
                name: "ugc",
                kind: "string",
                required: true,
                description: "the 64-bit UGC handle, decimal string or integer",
            },
        ],
        result: "{ service, board }",
        mutating: true,
        permission: Permission::Play,
    }
}

fn steam_leaderboard_attach_ugc(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = required_str(params, "name")?;
    let ugc = u64_param(params, "ugc")?.ok_or_else(|| bad("`ugc` (64-bit id) is required"))?;
    let mut boards = boards_mut(world)?;
    boards
        .attach_ugc(name, UgcHandle(ugc))
        .map_err(|e| leaderboard_error(&e))?;
    let board = boards.board(name).map(|s| board_json(name, s));
    Ok(json!({ "service": boards.service_name(), "board": board }))
}

fn entries_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.leaderboard_entries",
        tool: "steam_leaderboard_entries",
        description: "One board's state: lookup, handle, last upload outcome, last downloaded rows, pending work, last error.",
        params: &[NAME],
        result: "{ service, local_user, board, range, entries: [{ user, rank, score, details, ugc }] }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn steam_leaderboard_entries(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = required_str(params, "name")?;
    let boards = world.get_resource::<Leaderboard>().ok_or_else(not_running)?;
    let state = boards.board(name).ok_or_else(|| {
        f0002(
            "leaderboard_not_found",
            format!("no board `{name}` is declared"),
            "steam.leaderboard_find first",
        )
    })?;
    let (range, entries) = match &state.last_entries {
        Some((range, rows)) => (range_json(*range), rows.iter().map(entry_json).collect()),
        None => (Value::Null, Vec::new()),
    };
    Ok(json!({
        "service": boards.service_name(),
        "local_user": boards.local_user().map(|u| u.to_string()),
        "board": board_json(name, state),
        "range": range,
        "entries": entries,
    }))
}

fn list_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.leaderboard_list",
        tool: "steam_leaderboard_list",
        description: "Every declared leaderboard with its lookup state and pending work.",
        params: &[],
        result: "{ plugin, service, pending, boards: [{ name, lookup, handle, sort, display, method, entry_count, pending, error }] }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn steam_leaderboard_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(boards) = world.get_resource::<Leaderboard>() else {
        return Ok(json!({ "plugin": false }));
    };
    let list: Vec<Value> = boards.boards().map(|(name, s)| board_json(name, s)).collect();
    Ok(json!({
        "plugin": true,
        "service": boards.service_name(),
        "pending": boards.pending(),
        "boards": list,
    }))
}

fn owns_app_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.owns_app",
        tool: "steam_owns_app",
        description: "App or DLC ownership for the signed-in user: subscribed, installed, dlc_installed, install_dir.",
        params: &[ParamSchema {
            name: "app_id",
            kind: "integer",
            required: true,
            description: "the Steam app id of the game, demo or DLC",
        }],
        result: "{ app_id, subscribed, installed, dlc_installed, install_dir }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn steam_owns_app(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let app = u32_param(params, "app_id")?.ok_or_else(|| bad("`app_id` (integer) is required"))?;
    if app == 0 {
        return Err(f0002(
            "app_id_invalid",
            "app id 0 is not an app",
            "pass a Steam app id",
        ));
    }
    let steam = world.get_resource::<Steam>().ok_or_else(|| {
        f0002(
            "steam_unavailable",
            "the app has no SteamPlugin",
            "add regia_steam::SteamPlugin",
        )
    })?;
    let owned = steam.app_ownership(AppId(app)).map_err(|e| steam_error(&e))?;
    Ok(json!({
        "app_id": app,
        "subscribed": owned.subscribed,
        "installed": owned.installed,
        "dlc_installed": owned.dlc_installed,
        "install_dir": owned.install_dir,
    }))
}

fn steam_ready(world: &World) -> Result<&Steam, RpcError> {
    let steam = world.get_resource::<Steam>().ok_or_else(|| {
        f0002(
            "steam_unavailable",
            "the app has no SteamPlugin",
            "add regia_steam::SteamPlugin",
        )
    })?;
    if steam.state() != SteamState::Ready {
        return Err(steam_error(&SteamError::Unavailable));
    }
    Ok(steam)
}

fn timeline_mode_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.timeline_mode",
        tool: "steam_timeline_mode",
        description: "Set the game-recording timeline's game mode (bar color).",
        params: &[ParamSchema {
            name: "mode",
            kind: "string",
            required: true,
            description: "playing, staging, menus or loading_screen",
        }],
        result: "{ mode }",
        mutating: true,
        permission: Permission::Play,
    }
}

fn steam_timeline_mode(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = required_str(params, "mode")?;
    let mode = TimelineMode::from_name(name).ok_or_else(|| {
        bad(format!(
            "unknown mode `{name}`: use playing, staging, menus or loading_screen"
        ))
    })?;
    steam_ready(world)?
        .timeline_mode(mode)
        .map_err(|e| steam_error(&e))?;
    Ok(json!({ "mode": mode.name() }))
}

fn timeline_state_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.timeline_state",
        tool: "steam_timeline_state",
        description: "Describe the game state on the timeline from `delta` seconds from now; no description clears it.",
        params: &[
            ParamSchema {
                name: "description",
                kind: "string",
                required: false,
                description: "1..=1024 bytes; absent or empty clears the state",
            },
            ParamSchema {
                name: "delta",
                kind: "number",
                required: false,
                description: "seconds from now, 0..=3600 (default 0)",
            },
        ],
        result: "{ state }",
        mutating: true,
        permission: Permission::Play,
    }
}

fn steam_timeline_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let description = str_param(params, "description")?.filter(|d| !d.is_empty());
    let delta = f32_param(params, "delta")?.unwrap_or(0.0);
    let steam = steam_ready(world)?;
    let sent = match description {
        Some(text) => steam.timeline_state(text, delta),
        None => steam.timeline_clear_state(delta),
    };
    sent.map_err(|e| steam_error(&e))?;
    Ok(json!({ "state": description }))
}

fn timeline_event_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.timeline_event",
        tool: "steam_timeline_event",
        description: "Mark an instant or ranged event on the game-recording timeline.",
        params: &[
            ParamSchema {
                name: "title",
                kind: "string",
                required: true,
                description: "1..=255 bytes",
            },
            ParamSchema {
                name: "icon",
                kind: "string",
                required: false,
                description: "Steam icon name (default steam_marker)",
            },
            ParamSchema {
                name: "description",
                kind: "string",
                required: false,
                description: "0..=1024 bytes",
            },
            ParamSchema {
                name: "priority",
                kind: "integer",
                required: false,
                description: "0..=1000 (default 0)",
            },
            ParamSchema {
                name: "start_offset",
                kind: "number",
                required: false,
                description: "seconds from now, -3600..=3600; negative marks the past (default 0)",
            },
            ParamSchema {
                name: "duration",
                kind: "number",
                required: false,
                description: "seconds, 0..=3600; 0 is an instant (default 0)",
            },
            ParamSchema {
                name: "clip",
                kind: "string",
                required: false,
                description: "none, standard (default) or featured",
            },
        ],
        result: "{ events }",
        mutating: true,
        permission: Permission::Play,
    }
}

fn steam_timeline_event(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut event = TimelineEvent::new(required_str(params, "title")?);
    if let Some(icon) = str_param(params, "icon")? {
        event.icon = icon.to_owned();
    }
    if let Some(description) = str_param(params, "description")? {
        event.description = description.to_owned();
    }
    if let Some(priority) = u32_param(params, "priority")? {
        event.priority = priority;
    }
    event.start_offset = f32_param(params, "start_offset")?.unwrap_or(0.0);
    event.duration = f32_param(params, "duration")?.unwrap_or(0.0);
    if let Some(clip) = str_param(params, "clip")? {
        event.clip = TimelineClip::from_name(clip)
            .ok_or_else(|| bad(format!("unknown clip `{clip}`: use none, standard or featured")))?;
    }
    let steam = steam_ready(world)?;
    steam.timeline_event(&event).map_err(|e| steam_error(&e))?;
    Ok(json!({ "events": steam.timeline_log().events }))
}

fn timeline_schema() -> MethodSchema {
    MethodSchema {
        name: "steam.timeline",
        tool: "steam_timeline",
        description: "What the engine last sent to the game-recording timeline (Steam has no read back).",
        params: &[],
        result: "{ plugin, running, mode, state, events, last_event, refused }",
        mutating: false,
        permission: Permission::Read,
    }
}

fn steam_timeline(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(steam) = world.get_resource::<Steam>() else {
        return Ok(json!({ "plugin": false }));
    };
    let log = steam.timeline_log();
    Ok(json!({
        "plugin": true,
        "running": steam.state() == SteamState::Ready,
        "mode": log.mode.map(TimelineMode::name),
        "state": log.state,
        "events": log.events,
        "last_event": log.last_event.map(|e| json!({
            "icon": e.icon,
            "title": e.title,
            "description": e.description,
            "priority": e.priority,
            "start_offset": e.start_offset,
            "duration": e.duration,
            "clip": e.clip.name(),
        })),
        "refused": log.refused,
    }))
}

/// Registers the methods on `extension`.
pub(crate) fn register(extension: &mut regia_agent::extension::AgentExtension) {
    extension.register(find_schema(), steam_leaderboard_find);
    extension.register(upload_schema(), steam_leaderboard_upload);
    extension.register(download_schema(), steam_leaderboard_download);
    extension.register(attach_schema(), steam_leaderboard_attach_ugc);
    extension.register(entries_schema(), steam_leaderboard_entries);
    extension.register(list_schema(), steam_leaderboard_list);
    extension.register(owns_app_schema(), steam_owns_app);
    extension.register(timeline_mode_schema(), steam_timeline_mode);
    extension.register(timeline_state_schema(), steam_timeline_state);
    extension.register(timeline_event_schema(), steam_timeline_event);
    extension.register(timeline_schema(), steam_timeline);
}
