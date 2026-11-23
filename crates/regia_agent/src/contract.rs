// SPDX-License-Identifier: MPL-2.0
//! The per-method contract (ADR-1034): one worked example, the error codes a call can answer
//! with, the size limits the dispatcher enforces before the handler runs, and the rate class.
//!
//! [`crate::schema::METHODS`] says what a method takes and returns; this table says how a call
//! can fail and how big it may be. `agent.schema` publishes both, the generated method reference
//! (`book/reference/agent-methods.md`) prints both, and `cargo xtask agent-coverage` checks that
//! every lowercase error code a contract names is documented on the method's book page.
//!
//! The dispatcher applies `check_params` to every call: `params` must be an object (or absent),
//! each [`Limit`] of the method's contract holds, and every array or string parameter without its
//! own limit stays under [`DEFAULT_MAX_ITEMS`] / [`DEFAULT_MAX_BYTES`]. Handlers keep their own
//! checks; the dispatcher's run first, so a too-large call is refused before any work starts.

use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

/// Codes any method can answer with, so no contract lists them: an internal failure (`F0001`),
/// bad parameters (`F0002`), a bad request envelope (`F0004`), a rate limit (`F0007`) and a
/// missing permission (`F0012`). The server adds `F0005` (not JSON), `request_too_large` (over the
/// parse budget) and `F0006` (no answer in time) before a request reaches any method.
pub const UNIVERSAL_ERRORS: &[&str] = &["F0001", "F0002", "F0004", "F0007", "F0012"];

/// Most items an array parameter may hold when its contract names no [`Limit::Items`].
pub const DEFAULT_MAX_ITEMS: usize = 65_536;

/// Most bytes a string parameter may hold when its contract names no [`Limit::Bytes`].
pub const DEFAULT_MAX_BYTES: usize = 1 << 20;

/// A limit on one top-level parameter, checked by the dispatcher before the handler runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Limit {
    /// A number parameter is accepted only inside `min..=max`. For an array parameter (a
    /// position, a list of points) every number in it, at any nesting depth, must be inside.
    Range {
        /// Parameter name.
        param: &'static str,
        /// Smallest accepted value.
        min: f64,
        /// Largest accepted value.
        max: f64,
    },
    /// An array parameter holds at most `max` items (this replaces [`DEFAULT_MAX_ITEMS`]).
    Items {
        /// Parameter name.
        param: &'static str,
        /// Most items.
        max: usize,
    },
    /// A string parameter holds at most `max` bytes of UTF-8 (this replaces [`DEFAULT_MAX_BYTES`]).
    Bytes {
        /// Parameter name.
        param: &'static str,
        /// Most bytes.
        max: usize,
    },
}

impl Limit {
    /// The parameter the limit applies to.
    pub fn param(&self) -> &'static str {
        match *self {
            Self::Range { param, .. } | Self::Items { param, .. } | Self::Bytes { param, .. } => param,
        }
    }

    /// The limit as `agent.schema` publishes it.
    pub fn to_json(&self) -> Value {
        match *self {
            Self::Range { param, min, max } => {
                json!({ "param": param, "kind": "range", "min": min, "max": max })
            }
            Self::Items { param, max } => json!({ "param": param, "kind": "items", "max": max }),
            Self::Bytes { param, max } => json!({ "param": param, "kind": "bytes", "max": max }),
        }
    }

    /// One line for the method reference, e.g. `` `frames` in 1..=10000 ``.
    pub fn describe(&self) -> String {
        match *self {
            Self::Range { param, min, max } => format!("`{param}` in {min}..={max}"),
            Self::Items { param, max } => format!("`{param}` at most {max} items"),
            Self::Bytes { param, max } => format!("`{param}` at most {max} bytes"),
        }
    }

    /// Why `value` breaks the limit, or `None` when it holds. A value of another JSON type is
    /// left to the handler (it answers `F0002` with the type it wanted).
    fn violation(&self, value: &Value) -> Option<String> {
        match *self {
            Self::Range { param, min, max } => {
                // Arrays nest (a polyline is an array of positions); walk them without recursion.
                let mut pending = vec![value];
                while let Some(value) = pending.pop() {
                    match value {
                        Value::Array(items) => pending.extend(items),
                        number => {
                            if let Some(number) = number.as_f64()
                                && !(min..=max).contains(&number)
                            {
                                return Some(format!("`{param}` must be in {min}..={max} (got {number})"));
                            }
                        }
                    }
                }
                None
            }
            Self::Items { param, max } => {
                let len = value.as_array()?.len();
                (len > max).then(|| format!("`{param}` holds {len} items; at most {max}"))
            }
            Self::Bytes { param, max } => {
                let len = value.as_str()?.len();
                (len > max).then(|| format!("`{param}` is {len} bytes; at most {max}"))
            }
        }
    }
}

/// How a call counts against the rate limits in `.regia/agent.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cost {
    /// Constant or small work; only mutating calls count (`rate_limit_per_second`).
    Light,
    /// Walks the whole world, encodes or decodes images, or spans many frames. Counts against
    /// `heavy_rate_limit_per_second` as well, read-only or not.
    Heavy,
}

impl Cost {
    /// The name `agent.schema` publishes (`"light"` or `"heavy"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Heavy => "heavy",
        }
    }
}

/// One method's contract.
#[derive(Debug, Clone, Copy)]
pub struct MethodContract {
    /// JSON-RPC method name; one row per [`crate::schema::METHODS`] entry.
    pub method: &'static str,
    /// A typical call's `params`, as JSON text. It names only declared params, includes every
    /// required one, and each value has the declared kind.
    pub example: &'static str,
    /// Every `data.code` the method can answer with besides [`UNIVERSAL_ERRORS`], sorted.
    pub errors: &'static [&'static str],
    /// The dispatcher-enforced limits.
    pub limits: &'static [Limit],
    /// The rate class.
    pub cost: Cost,
}

impl MethodContract {
    /// The parsed example (`{}` if the text does not parse; a unit test keeps every example valid).
    pub fn example_json(&self) -> Value {
        serde_json::from_str(self.example).unwrap_or_else(|_| Value::Object(Map::new()))
    }

    /// The contract fields `agent.schema` adds to the method's entry.
    pub fn to_json(&self) -> Value {
        json!({
            "example": self.example_json(),
            "errors": self.errors,
            "limits": self.limits.iter().map(Limit::to_json).collect::<Vec<_>>(),
            "cost": self.cost.name(),
        })
    }
}

const fn c(
    method: &'static str,
    example: &'static str,
    errors: &'static [&'static str],
    limits: &'static [Limit],
) -> MethodContract {
    MethodContract {
        method,
        example,
        errors,
        limits,
        cost: Cost::Light,
    }
}

const fn heavy(
    method: &'static str,
    example: &'static str,
    errors: &'static [&'static str],
    limits: &'static [Limit],
) -> MethodContract {
    MethodContract {
        method,
        example,
        errors,
        limits,
        cost: Cost::Heavy,
    }
}

const fn range(param: &'static str, min: f64, max: f64) -> Limit {
    Limit::Range { param, min, max }
}

const fn items(param: &'static str, max: usize) -> Limit {
    Limit::Items { param, max }
}

const fn bytes(param: &'static str, max: usize) -> Limit {
    Limit::Bytes { param, max }
}

/// Inclusive bounds for the four `anim.clip_report` cook tolerances.
///
/// The minimum is the smallest positive `f32`. Zero is outside the range: the handler stores
/// an `f32` and rejects a non-positive value, so an inclusive minimum of 0 would let the
/// dispatcher accept a call the handler refuses.
pub const CLIP_REPORT_TOLERANCE_MIN: f64 = f32::MIN_POSITIVE as f64;

/// Inclusive maximum for an `anim.clip_report` cook tolerance.
pub const CLIP_REPORT_TOLERANCE_MAX: f64 = 1000.0;

/// Every method's contract, in schema order.
pub const CONTRACTS: &[MethodContract] = &[
    c(
        "agent.hello",
        r#"{ "client": "claude", "model": "opus" }"#,
        &[],
        &[],
    ),
    c("agent.guide", r#"{ "topic": "testing" }"#, &["F0003"], &[]),
    c("agent.schema", r#"{ "method": "scene.spawn" }"#, &["F0003"], &[]),
    c("agent.changes", r#"{ "since": 0, "limit": 200 }"#, &[], &[]),
    c("project.info", r#"{}"#, &[], &[]),
    c(
        "types.list",
        r#"{ "filter": "transform", "component": true }"#,
        &[],
        &[bytes("filter", 256)],
    ),
    c("types.describe", r#"{ "type": "Transform" }"#, &["F0003"], &[]),
    c("resources.list", r#"{}"#, &[], &[]),
    c("errors.explain", r#"{ "code": "F0012" }"#, &["F0003"], &[]),
    c(
        "scene.query_entities",
        r#"{ "name": "Rock*", "component": "Transform", "limit": 50 }"#,
        &["F0003"],
        &[range("limit", 0.0, 10000.0), bytes("name", 256)],
    ),
    c(
        "scene.get_entity",
        r#"{ "entity": 4294967296, "components": ["Transform"] }"#,
        &["F0003"],
        &[items("components", 256)],
    ),
    c(
        "scene.set_property",
        r#"{ "entity": 4294967296, "component": "Transform", "path": "translation.y", "value": 5.0, "dry_run": true }"#,
        &["F0003"],
        &[bytes("path", 1024)],
    ),
    c(
        "scene.spawn",
        r#"{ "name": "Agent Rock", "components": {} }"#,
        &["F0003"],
        &[],
    ),
    c(
        "scene.spawn_primitive",
        r#"{ "shape": "ramp", "params": { "size": [6.0, 1.5, 3.0] }, "transform": { "translation": [0.0, 0.75, -8.0], "euler_deg": [0.0, 90.0, 0.0] }, "name": "Loading ramp", "color": [0.6, 0.6, 0.65] }"#,
        &["F0003"],
        &[],
    ),
    c("scene.despawn", r#"{ "entity": 17 }"#, &["F0003"], &[]),
    c(
        "scene.duplicate",
        r#"{ "entity": 17, "parent": 3 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "scene.reparent",
        r#"{ "entity": 17, "parent": 3 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "scene.add_component",
        r#"{ "entity": 17, "component": "Marker", "value": { "value": 9.0 } }"#,
        &["F0003"],
        &[],
    ),
    c(
        "scene.remove_component",
        r#"{ "entity": 17, "component": "Marker" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "scene.apply_edits",
        r#"{ "edits": [{ "op": "add_component", "entity": 17, "component": "Marker", "value": { "value": 9.0 } }, { "op": "set_property", "entity": 17, "component": "Marker", "path": "value", "value": 4.0 }, { "op": "spawn", "name": "Added" }], "dry_run": false }"#,
        &["F0003", "F0014"],
        &[items("edits", 4096)],
    ),
    heavy("scene.serialize", r#"{}"#, &[], &[]),
    c(
        "assets.list",
        r#"{ "query": "helmet", "kind": "CpuMesh", "folder": "khronos", "limit": 200 }"#,
        &["F0016"],
        &[range("limit", 0.0, 10000.0)],
    ),
    c(
        "assets.describe",
        r#"{ "asset": "assets/khronos/DamagedHelmet.glb" }"#,
        &["F0016"],
        &[],
    ),
    c(
        "assets.load",
        r#"{ "asset": "assets/khronos/DamagedHelmet.glb" }"#,
        &["F0013", "F0016"],
        &[],
    ),
    c(
        "assets.reload",
        r#"{ "asset": "assets/khronos/helmet.glb" }"#,
        &["F0016"],
        &[],
    ),
    heavy(
        "assets.skin_check",
        r#"{ "mesh": "pilot.glb#mesh/body/primitive_0", "skeleton": "pilot.glb#skeleton/hips", "clip": "pilot.glb#clip/run", "time": 0.4 }"#,
        &["F0013", "F0016"],
        &[],
    ),
    c(
        "assets.status",
        r#"{ "asset": "assets/khronos/helmet.glb" }"#,
        &["F0016"],
        &[],
    ),
    c("assets.watch_status", r#"{ "limit": 2 }"#, &[], &[]),
    heavy(
        "seeing.screenshot",
        r#"{"target": "window", "timeout_ms": 30000}"#,
        &[
            "F0003",
            "F0011",
            "F0013",
            "F0016",
            "overlay_needs_editor",
            "screenshot_camera_not_rendered",
            "screenshot_no_ui_layer",
            "screenshot_no_view",
            "screenshot_not_a_camera",
        ],
        &[],
    ),
    heavy(
        "seeing.capture_sequence",
        r#"{"frames": 4, "every_n": 5, "timeout_ms": 60000}"#,
        &["F0003", "F0011", "F0015", "too_many_pending"],
        &[range("every_n", 1.0, 3600.0)],
    ),
    heavy(
        "seeing.contact_sheet",
        r#"{"path": "assets/khronos/DamagedHelmet.glb", "angles": 4}"#,
        &["F0003", "F0011", "F0015", "F0016", "too_many_pending"],
        &[],
    ),
    heavy(
        "seeing.compare_images",
        r#"{"a": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==", "b": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==" }"#,
        &[],
        &[bytes("a", 33554432), bytes("b", 33554432)],
    ),
    heavy(
        "seeing.reference_trace",
        r#"{"scene": "current", "width": 640, "height": 360, "spp": 1024}"#,
        &["F0003", "F0011", "F0013", "too_many_pending"],
        &[
            range("width", 1.0, 4096.0),
            range("height", 1.0, 4096.0),
            range("spp", 1.0, 1048576.0),
            range("max_bounces", 1.0, 1024.0),
            items("aovs", 32),
        ],
    ),
    heavy(
        "seeing.compare",
        r#"{"a": {"job": 1}, "b": "/tmp/frame_hdr.exr", "aov": "beauty", "ev100": 12.5}"#,
        &["F0003", "F0013"],
        &[range("ppd", 1.0, 500.0)],
    ),
    c("seeing.pick_entity", r#"{"x": 640, "y": 360}"#, &["F0013"], &[]),
    heavy("seeing.memory_report", r#"{"top": 20}"#, &["F0011"], &[]),
    c("assets.texture_memory", r#"{ "top": 3 }"#, &["F0003"], &[]),
    c(
        "registry.query",
        r#"{ "tags": ["biome=forest"], "type": "CookedMesh", "limit": 20 }"#,
        &[
            "registry_bad_query",
            "registry_bad_tag",
            "registry_not_loaded",
            "registry_unreadable",
        ],
        &[
            items("tags", 64),
            range("limit", 1.0, 5000.0),
            bytes("path", 1024),
        ],
    ),
    c(
        "registry.describe",
        r#"{ "asset": "assets/props/tree.glb" }"#,
        &[
            "registry_bad_query",
            "registry_not_loaded",
            "registry_unknown_asset",
            "registry_unreadable",
        ],
        &[],
    ),
    c(
        "registry.dependencies",
        r#"{ "asset": "assets/props/tree.glb", "transitive": true }"#,
        &[
            "registry_bad_query",
            "registry_not_loaded",
            "registry_unknown_asset",
            "registry_unreadable",
        ],
        &[],
    ),
    c(
        "registry.referencers",
        r#"{ "asset": "assets/textures/bark.png", "transitive": true }"#,
        &[
            "registry_bad_query",
            "registry_not_loaded",
            "registry_unknown_asset",
            "registry_unreadable",
        ],
        &[],
    ),
    c(
        "registry.tags",
        r#"{ "key": "biome" }"#,
        &["registry_bad_query", "registry_not_loaded", "registry_unreadable"],
        &[],
    ),
    c(
        "assets.load_progress",
        r#"{ "set": "map:switchbacks" }"#,
        &["F0011"],
        &[],
    ),
    c("assets.handles", r#"{ "kind": "Note" }"#, &["F0016"], &[]),
    c("render.boot_timeline", r#"{}"#, &[], &[]),
    c("render.pipeline_warm", r#"{}"#, &["F0011"], &[]),
    c("render.scene_ready", r#"{}"#, &["F0011"], &[]),
    c(
        "render.memory_budget",
        r#"{ "tier": "minimum", "set": "map:bore_twelve" }"#,
        &["F0011"],
        &[],
    ),
    c("render.memory_pressure", r#"{}"#, &[], &[]),
    c("play.pause", r#"{}"#, &["F0018"], &[]),
    c("play.resume", r#"{}"#, &["F0018"], &[]),
    c(
        "play.step",
        r#"{"frames": 10}"#,
        &["F0009", "F0018"],
        &[range("frames", 1.0, 10000.0)],
    ),
    c("play.set_time_scale", r#"{"scale": 0.5}"#, &["F0018"], &[]),
    c(
        "play.inject_input",
        r#"{"events": [{"type": "key", "key": "KeyW", "text": "w", "state": "pressed"}, {"type": "cursor", "x": 400, "y": 300}], "actions": [{"action": "Jump", "value": 1.0}] }"#,
        &["F0003", "F0013", "play_gate_text"],
        &[items("events", 1024), items("actions", 1024)],
    ),
    c("input.spacemouse_status", r#"{}"#, &["F0013"], &[]),
    c(
        "input.rebind_state",
        r#"{}"#,
        &["F0003", "F0013", "input_not_installed", "rebind_unknown_context"],
        &[],
    ),
    c(
        "input.rebind_axis",
        r#"{ "context": "Gameplay", "action": "Move", "slot": "gamepad", "dead_zone": 0.2, "invert_y": true }"#,
        &[
            "F0003",
            "F0013",
            "input_not_installed",
            "rebind_bad_tuning",
            "rebind_not_tunable",
            "rebind_unknown_action",
            "rebind_unknown_context",
        ],
        &[],
    ),
    c(
        "input.rebind_save",
        r#"{ "profile": "left_hand" }"#,
        &[
            "F0013",
            "input_not_installed",
            "rebind_bad_profile",
            "rebind_save_failed",
        ],
        &[],
    ),
    c(
        "input.rebind_load",
        r#"{ "profile": "left_hand" }"#,
        &[
            "F0003",
            "F0013",
            "input_not_installed",
            "rebind_bad_profile",
            "rebind_profile_corrupt",
            "rebind_profile_missing",
        ],
        &[],
    ),
    c(
        "input.rebind_profiles",
        r#"{}"#,
        &["F0013", "input_not_installed"],
        &[],
    ),
    c(
        "input.rebind",
        r#"{ "context": "Gameplay", "action": "Jump", "mode": "single" }"#,
        &[
            "F0003",
            "F0013",
            "input_not_installed",
            "rebind_bad_mode",
            "rebind_unknown_action",
            "rebind_unknown_context",
        ],
        &[],
    ),
    c(
        "input.rebind_cancel",
        r#"{}"#,
        &["F0013", "input_not_installed", "rebind_not_pending"],
        &[],
    ),
    c(
        "input.rebind_reset",
        r#"{ "context": "Gameplay", "action": "Jump" }"#,
        &[
            "F0003",
            "F0013",
            "input_not_installed",
            "rebind_no_default",
            "rebind_unknown_context",
        ],
        &[],
    ),
    c(
        "input.spacemouse_inject",
        r#"{"rotation": [0, 1, 0], "frames": 30}"#,
        &["F0013"],
        &[
            items("translation", 3),
            items("rotation", 3),
            range("frames", 1.0, 4294967295.0),
            items("buttons", 32),
        ],
    ),
    c("input.spacemouse_nav_defaults", r#"{}"#, &[], &[]),
    c(
        "play.wait_until",
        r#"{"condition": {"resource": "Time", "path": "frame", "op": "gte", "value": 1}, "timeout_ms": 5000}"#,
        &["F0014", "too_many_pending"],
        &[],
    ),
    c(
        "play.expect",
        r#"{"assertions": [{"name": "clock runs", "condition": {"resource": "Time", "path": "frame", "op": "gte", "value": 0}}] }"#,
        &[],
        &[items("assertions", 256)],
    ),
    c("play.get_diagnostics", r#"{}"#, &[], &[]),
    heavy(
        "play.hitch_report",
        r#"{"seconds": 10, "warmup_s": 2, "drive": "chase" }"#,
        &["F0017"],
        &[range("seconds", 0.1, 600.0)],
    ),
    c(
        "logs.read",
        r#"{ "since": 0, "level": "info", "target": "regia_agent", "contains": "screenshot", "limit": 200 }"#,
        &[],
        &[bytes("contains", 1024), bytes("target", 1024)],
    ),
    c("events.list", r#"{}"#, &[], &[]),
    c("events.read", r#"{ "since": 0 }"#, &[], &[]),
    c(
        "diagnostics.crash_reports",
        r#"{ "name": "crash-2026-10-06_12-34-56.txt" }"#,
        &["F0003"],
        &[],
    ),
    c("fuzz.targets", r#"{}"#, &[], &[]),
    heavy(
        "fuzz.run",
        r#"{ "target": "statetree", "seed": 7, "iterations": 500 }"#,
        &["F0003", "F0016"],
        &[
            range("iterations", 1.0, 5000.0),
            bytes("target", 256),
            bytes("path", 4096),
        ],
    ),
    heavy(
        "feedback.capture",
        r#"{ "category": "visual", "note": "hull flickers at the gate", "screenshot": true }"#,
        &["F0003"],
        &[bytes("note", 65536)],
    ),
    c(
        "feedback.list",
        r#"{ "name": "feedback-2026-10-11_14-02-31-visual.zip" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "telemetry.get_last_summary",
        r#"{ "session": true }"#,
        &["F0003"],
        &[],
    ),
    c("telemetry.live", r#"{}"#, &["F0003"], &[]),
    c("scripts.list", r#"{}"#, &["F0003"], &[]),
    c("scripts.get", r#"{ "entity": 17 }"#, &["F0003"], &[]),
    c("camera.get", r#"{}"#, &["F0003"], &[]),
    c("camera.set_mode", r#"{ "mode": "overhead" }"#, &["F0003"], &[]),
    c(
        "camera.blend_to",
        r#"{ "translation": [0.0, 2.0, 5.0], "duration": 1.0, "curve": "sine_in_out" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "settings.get",
        r#"{ "path": "input.sensitivity" }"#,
        &["F0003"],
        &[bytes("path", 256)],
    ),
    c(
        "settings.set",
        r#"{ "path": "audio.master", "value": 0.6 }"#,
        &["F0003"],
        &[bytes("path", 256)],
    ),
    c(
        "scene.spawn_prefab",
        r#"{ "prefab": "asset:67e55044-10b1-426f-9247-bb680e5fe0c8" }"#,
        &["F0003", "bad_guid", "spawn_refused"],
        &[bytes("prefab", 128), bytes("root_guid", 128)],
    ),
    c(
        "level.open",
        r#"{ "level": "asset:67e55044-10b1-426f-9247-bb680e5fe0c8", "mode": "additive" }"#,
        &["F0003", "bad_guid", "level_not_found"],
        &[bytes("level", 128)],
    ),
    c(
        "level.unload",
        r#"{ "level": "asset:67e55044-10b1-426f-9247-bb680e5fe0c8" }"#,
        &["F0003", "bad_guid"],
        &[bytes("level", 128)],
    ),
    c(
        "level.status",
        r#"{ "level": "asset:67e55044-10b1-426f-9247-bb680e5fe0c8" }"#,
        &["F0003", "bad_guid", "level_not_found"],
        &[bytes("level", 128)],
    ),
    c("rules.get", r#"{}"#, &["F0003"], &[]),
    c("timers.list", r#"{}"#, &[], &[]),
    c("tweens.list", r#"{}"#, &[], &[]),
    c("perf_overlay.get", r#"{}"#, &[], &[]),
    c(
        "perf_overlay.toggle",
        r#"{ "mode": "detailed" }"#,
        &["F0003"],
        &[],
    ),
    c("render.get_profile", r#"{}"#, &["F0003"], &[]),
    c("quality.get_detected", r#"{}"#, &[], &[]),
    c(
        "quality.detect",
        r#"{"adapter": {"name": "NVIDIA GeForce RTX 4070", "vendor": 4318, "vram_mb": 12282}, "apply": false}"#,
        &["F0003"],
        &[],
    ),
    c("quality.get_terrain_vt_policy", r#"{}"#, &[], &[]),
    c("quality.set_terrain_vt_policy", r#"{ "high": true }"#, &[], &[]),
    c(
        "render.set_profile",
        r#"{ "preset": "MobileLow", "post": true }"#,
        &["F0003"],
        &[
            range("cloud_samples", 1.0, 16.0),
            range("cloud_step_percent", 1.0, 400.0),
            range("cloud_max_divisor", 1.0, 8.0),
        ],
    ),
    c(
        "render.mesh_lod",
        r#"{ "radius": 6.2, "thresholds": [2.4, 0.9, 0.3, 0], "height": 1440 }"#,
        &["F0003"],
        &[items("thresholds", 16), range("height", 1.0, 16384.0)],
    ),
    c("render.get_sssr_cutoff", r#"{}"#, &[], &[]),
    c("render.set_sssr_cutoff", r#"{ "cutoff": 0.7 }"#, &[], &[]),
    c("render.get_calibration", r#"{}"#, &[], &[]),
    c(
        "render.set_calibration",
        r#"{ "black_level": 0.35, "gamma": 2.3 }"#,
        &[],
        &[],
    ),
    c(
        "ui.layers",
        r#"{ "action": "push", "entity": 4294967310, "mode": "ui_only" }"#,
        &["already_layer", "no_layers", "not_a_layer", "unknown_entity"],
        &[],
    ),
    c(
        "ui.accessibility",
        r#"{ "text_scale": 1.25, "reduced_motion": true }"#,
        &["ui_bad_flag", "ui_bad_text_scale"],
        &[],
    ),
    c(
        "ui.fuzzy",
        r#"{ "query": "fb", "items": ["fabric", "foo_bar"], "limit": 5 }"#,
        &["bad_items", "bad_limit", "bad_query"],
        &[],
    ),
    c(
        "ui.set_anchor_preset",
        r#"{ "entity": 4294967310, "preset": "bottom_right", "offsets": [0, 0, 16, 16] }"#,
        &[
            "bad_entity",
            "bad_offsets",
            "bad_preset",
            "size_required",
            "unknown_entity",
        ],
        &[],
    ),
    c(
        "ui.set_disabled",
        r#"{ "entity": 4294967310, "disabled": true, "recursive": true }"#,
        &["bad_entity", "bad_flag", "unknown_entity"],
        &[],
    ),
    c(
        "ui.debug_overlay",
        r#"{ "enabled": true, "outlines": true }"#,
        &["bad_entity", "bad_flag", "unknown_entity"],
        &[],
    ),
    c(
        "ui.play_anim",
        r#"{ "action": "list" }"#,
        &[
            "bad_action",
            "bad_animation",
            "bad_entity",
            "bad_flag",
            "bad_time",
            "unknown_animation",
            "unknown_entity",
        ],
        &[],
    ),
    c(
        "ui.sounds",
        r#"{ "sounds": { "hover": "ui_hover", "press": "ui_press" } }"#,
        &["bad_flag", "bad_max", "bad_sounds"],
        &[],
    ),
    c(
        "ui.tree",
        r#"{ "entity": 4294967310, "action": "query" }"#,
        &[
            "bad_action",
            "bad_entity",
            "bad_key",
            "bad_tree",
            "no_tree",
            "unknown_entity",
            "unknown_node",
        ],
        &[],
    ),
    c(
        "ui.color_picker",
        r##"{ "entity": 4294967310, "hex": "#336699" }"##,
        &["bad_color", "bad_entity", "unknown_entity"],
        &[],
    ),
    c(
        "ui.soft_cursor",
        r#"{ "enabled": true, "speed": 900.0 }"#,
        &["bad_cursor", "bad_flag"],
        &[],
    ),
    c("ui.keyboard", r#"{ "mode": "auto" }"#, &["bad_mode"], &[]),
    c(
        "ui.text_style",
        r##"{ "entity": 4294967310, "effects": [{ "color": "#000000", "width": 2.0 }] }"##,
        &[
            "bad_color",
            "bad_effect",
            "bad_entity",
            "bad_feature",
            "bad_param",
            "bad_span",
            "no_text",
            "unknown_entity",
        ],
        &[],
    ),
    c("ui.touch_live", r#"{ "enabled": true }"#, &["bad_flag"], &[]),
    c(
        "ui.search_box",
        r#"{ "entity": 4294967310, "query": "file" }"#,
        &["bad_entity", "bad_query", "no_search_box", "unknown_entity"],
        &[],
    ),
    c(
        "ui.progress_dialog",
        r#"{ "entity": 4294967310, "progress": 0.5, "status": "Baking" }"#,
        &["bad_entity", "bad_param", "no_progress_dialog", "unknown_entity"],
        &[],
    ),
    c(
        "ui.lobby_list",
        r#"{ "action": "join", "lobby": "42" }"#,
        &[
            "bad_action",
            "bad_entity",
            "bad_param",
            "no_game_widget",
            "no_lobby_driver",
            "unknown_entity",
        ],
        &[],
    ),
    c(
        "ui.scoreboard",
        r#"{ "action": "sort", "column": 0 }"#,
        &[
            "bad_action",
            "bad_entity",
            "bad_param",
            "no_game_widget",
            "unknown_entity",
        ],
        &[],
    ),
    c(
        "ui.radar",
        r#"{ "action": "set", "range": 75.0 }"#,
        &[
            "bad_action",
            "bad_entity",
            "bad_param",
            "no_game_widget",
            "unknown_entity",
        ],
        &[],
    ),
    c(
        "ui.world_markers",
        r#"{ "action": "add", "position": [0.0, 1.0, -10.0], "label": "Objective", "clamp_to_edge": true }"#,
        &[
            "bad_action",
            "bad_color",
            "bad_entity",
            "bad_param",
            "no_game_widget",
            "no_world_position",
            "unknown_entity",
        ],
        &[],
    ),
    c(
        "ui.target_bracket",
        r#"{ "action": "lock", "position": [0.0, 1.0, -100.0], "speed": 300.0, "velocity": [10.0, 0.0, 0.0] }"#,
        &[
            "bad_action",
            "bad_color",
            "bad_entity",
            "bad_param",
            "no_game_widget",
            "no_world_position",
            "unknown_entity",
        ],
        &[],
    ),
    c(
        "ui.data_grid",
        r#"{ "entity": 4294967310, "action": "sort", "column": 0 }"#,
        &[
            "bad_action",
            "bad_entity",
            "bad_grid",
            "bad_param",
            "no_grid",
            "unknown_entity",
        ],
        &[],
    ),
    c(
        "ui.material",
        r#"{ "entity": 4294967310, "kind": "shine" }"#,
        &[
            "bad_color",
            "bad_entity",
            "bad_material",
            "bad_param",
            "unknown_entity",
        ],
        &[],
    ),
    c(
        "ui.touch",
        r#"{ "samples": [{ "id": 1, "stage": "down", "pos": [0, 0], "time": 0.0 }, { "id": 1, "stage": "up", "pos": [0, -150], "time": 0.1 }] }"#,
        &["bad_config", "bad_samples"],
        &[],
    ),
    c(
        "ui.tween",
        r#"{ "action": "start", "entity": 4294967310, "property": "opacity", "from": 0.0, "to": 1.0, "duration": 0.5, "delay": 0.0, "ease": "quad_out" }"#,
        &[
            "bad_action",
            "bad_duration",
            "bad_ease",
            "bad_entity",
            "bad_kind",
            "bad_mode",
            "bad_value",
            "missing_entity",
            "missing_property",
            "no_tweens",
            "too_many_tweens",
            "unknown_entity",
            "unknown_property",
        ],
        &[],
    ),
    c(
        "ui.list",
        r#"{ "entity": 4294967310, "action": "items", "start": 0, "limit": 50 }"#,
        &[
            "index_out_of_range",
            "no_selection",
            "not_a_list",
            "unknown_entity",
        ],
        &[items("indices", 4096)],
    ),
    c(
        "ui.scroll_to",
        r#"{ "entity": 4294967310, "index": 40, "align": "center" }"#,
        &[
            "index_out_of_range",
            "no_selection",
            "not_a_list",
            "not_scrollable",
            "unknown_entity",
        ],
        &[],
    ),
    c(
        "render.camera_cut",
        r#"{"cut": true, "exposure": "snap" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.sequence",
        r#"{"action": "seek", "time": 2.0}"#,
        &["F0003"],
        &[bytes("ron", 16777216)],
    ),
    c(
        "camera.set_path",
        r#"{"path": {"interp": "CatmullRom", "keys": [{"time": 0, "position": [0, 2, 12]}, {"time": 2, "position": [4, 2, 7]}, {"time": 4, "position": [6, 2, 3]}], "look_at": {"Point": [0, 1.5, 0]}}, "fov": [{"time": 0, "value": 40}, {"time": 4, "value": 28, "ease": "Smooth"}], "fade_in": 0.5, "preview": 5}"#,
        &["F0003"],
        &[range("preview", 0.0, 64.0)],
    ),
    c(
        "camera.raycast",
        r#"{"u": 0.5, "v": 0.5}"#,
        &["F0003"],
        &[
            range("u", 0.0, 1.0),
            range("v", 0.0, 1.0),
            items("skip", 1024),
            items("layers", 256),
        ],
    ),
    c(
        "ui.query",
        r#"{ "type": "Button", "text_contains": "play", "limit": 20 }"#,
        &["F0003", "F0013"],
        &[range("limit", 1.0, 1000.0)],
    ),
    c("ui.state", r#"{}"#, &["F0003", "F0013"], &[]),
    c(
        "ui.click",
        r#"{ "name": "play_button", "count": 1 }"#,
        &["F0003", "F0013"],
        &[range("count", 1.0, 8.0)],
    ),
    c("ui.focus", r#"{ "name": "settings" }"#, &["F0003", "F0013"], &[]),
    c(
        "ui.type",
        r#"{ "type": "TextInput", "text": "hello", "keys": ["Enter"] }"#,
        &["F0003", "F0013"],
        &[bytes("text", 65536), items("keys", 256)],
    ),
    c(
        "ui.set_value",
        r#"{ "name": "volume_slider", "value": 0.5 }"#,
        &["F0003", "F0013"],
        &[bytes("value", 65536)],
    ),
    heavy(
        "render.out_start",
        r#"{"out": "/tmp/shot1", "frames": 120, "fps": 30, "width": 1080, "height": 1920, "supersample": 2, "layout": "portrait", "format": "png", "cuts": [60], "warmup_frames": 10}"#,
        &[
            "F0009",
            "F0011",
            "F0013",
            "F0018",
            "F0019",
            "aov_format_unsupported",
            "unknown_aov",
        ],
        &[
            range("frames", 1.0, 1000000.0),
            range("width", 1.0, 8192.0),
            range("height", 1.0, 8192.0),
            range("fps", 1.0, 1000000.0),
            range("fps_den", 1.0, 100000.0),
            range("supersample", 1.0, 4.0),
            range("warmup_frames", 0.0, 10000.0),
            range("settle_frames", 0.0, 10000.0),
            items("cuts", 4096),
            items("time_map", 4096),
            bytes("name", 64),
            range("subframes", 1.0, 64.0),
            range("shutter_angle", 0.001, 360.0),
            range("bit_depth", 8.0, 16.0),
            range("writers", 0.0, 16.0),
            items("aovs", 9),
        ],
    ),
    c(
        "render.out_progress",
        r#"{"job": 1, "hashes": true }"#,
        &["F0003"],
        &[],
    ),
    c("render.out_cancel", r#"{"job": 1}"#, &["F0003"], &[]),
    heavy(
        "render.out_job",
        r#"{"file": "cinematics/trailer.render_job.ron", "date": "2026-10-28", "dry_run": true}"#,
        &[
            "F0009",
            "F0011",
            "F0018",
            "F0019",
            "aov_format_unsupported",
            "render_job_bad_path",
            "render_job_bad_source",
            "render_job_invalid",
            "render_job_no_date",
            "render_job_no_source",
            "render_job_parse",
            "render_job_path_clash",
            "render_job_preroll_cap",
            "render_job_unbound",
            "render_job_unknown_marker",
            "render_job_unknown_shot",
            "render_job_unreadable",
            "render_job_version",
            "unknown_aov",
        ],
        &[bytes("date", 10), bytes("job", 1_048_576), bytes("file", 4096)],
    ),
    c("level.get_look", r#"{}"#, &["F0003"], &[]),
    c(
        "level.set_look",
        r#"{"look": {"exposure_bias_ev": 0.5, "grade": {"white_balance_kelvin": 6900}} }"#,
        &["F0003"],
        &[],
    ),
    c("render.get_forward_lite", r#"{}"#, &["F0003"], &[]),
    c("render.set_forward_lite", r#"{"tier": "auto"}"#, &["F0003"], &[]),
    c("render.get_output_dither", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_output_dither",
        r#"{"enabled": true}"#,
        &["F0003"],
        &[],
    ),
    c("render.get_star_flux", r#"{}"#, &["F0003"], &[]),
    c("render.set_star_flux", r#"{"enabled": true}"#, &["F0003"], &[]),
    c("render.get_sun_shadow_range", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_sun_shadow_range",
        r#"{"range_m": 80}"#,
        &["F0003"],
        &[],
    ),
    c("render.get_gi_mode", r#"{}"#, &["F0003"], &[]),
    c("level.get_baked_gi", r#"{}"#, &[], &[]),
    c("render.problems", r#"{}"#, &["F0003"], &[]),
    c("render.mesh_occluders", r#"{}"#, &["F0003"], &[]),
    c("render.emissive_lights", r#"{}"#, &["F0003"], &[]),
    c("render.set_gi_mode", r#"{"mode": "balanced"}"#, &["F0003"], &[]),
    c(
        "render.set_gi_occluders",
        r#"{"capsules": [{"a": [0, 0, 0], "b": [0, 2, 0], "radius": 0.5, "strength": 1}], "gizmos": false}"#,
        &["F0003"],
        &[items("capsules", 1024)],
    ),
    c("sim.counters", r#"{}"#, &["F0003"], &[]),
    c(
        "sim.check_framerate_independence",
        r#"{ "a": { "ticks": 120, "wall_secs": 2.0 }, "b": { "ticks": 119, "wall_secs": 2.0 }, "tolerance": 0.02 }"#,
        &[],
        &[],
    ),
    c("fog.status", r#"{}"#, &["F0003"], &[]),
    c(
        "fog.set_viewer",
        r#"{ "viewer": 2, "dry_run": true }"#,
        &["F0003"],
        &[],
    ),
    c("sim.host_status", r#"{}"#, &["F0003"], &[]),
    c(
        "sim.host_hold",
        r#"{ "held": true }"#,
        &["F0003", "sim_host_panicked", "sim_host_stopped"],
        &[],
    ),
    c(
        "sim.host_step",
        r#"{ "ticks": 60 }"#,
        &[
            "F0003",
            "sim_host_bad_ticks",
            "sim_host_not_manual",
            "sim_host_panicked",
            "sim_host_stopped",
        ],
        &[],
    ),
    c(
        "sim.tick_profile",
        r#"{ "count": 10 }"#,
        &["bad_count", "no_tick_profile"],
        &[],
    ),
    c(
        "sim.host_snapshot",
        r#"{ "include_bytes": false }"#,
        &[
            "F0003",
            "host_snapshot_bytes_disabled",
            "sim_host_busy",
            "sim_host_no_snapshots",
            "sim_host_panicked",
            "sim_host_snapshot_failed",
            "sim_host_snapshot_tick_passed",
            "sim_host_stopped",
        ],
        &[],
    ),
    c(
        "sim.host_restore",
        r#"{}"#,
        &[
            "F0003",
            "host_restore_disabled",
            "host_snapshot_none",
            "sim_host_busy",
            "sim_host_no_snapshots",
            "sim_host_panicked",
            "sim_host_restore_failed",
            "sim_host_stopped",
        ],
        &[],
    ),
    c("sim.bridge_settings", r#"{}"#, &["F0003"], &[]),
    c(
        "sim.set_bridge_settings",
        r#"{ "handedness": "left_handed_reflect_x", "teleport_distance": 10.0, "kind": "unit", "blend": "hold", "dry_run": true }"#,
        &[
            "F0003",
            "sim_bad_blend",
            "sim_bad_delay",
            "sim_bad_handedness",
            "sim_bad_teleport_distance",
            "sim_kind_unknown",
        ],
        &[],
    ),
    c("fog.texture_stats", r#"{}"#, &["F0003"], &[]),
    c(
        "fog.set_look",
        r#"{ "enabled": true, "dry_run": true }"#,
        &["F0003"],
        &[],
    ),
    c(
        "sim.det_debug_draw",
        r#"{ "what": ["grid", "contacts"] }"#,
        &["bad_what", "no_det_debug"],
        &[],
    ),
    heavy(
        "sim.grid_flow",
        r#"{ "goal": [100.0, 50.0], "profile": 0, "window": { "x0": 40, "z0": 40, "x1": 72, "z1": 72 } }"#,
        &[
            "grid_flow_bad_goal",
            "grid_flow_bad_profile",
            "grid_flow_bad_window",
            "grid_flow_goal_blocked",
            "no_det_flow",
        ],
        &[items("goal", 64)],
    ),
    c("anim.list", r#"{}"#, &[], &[]),
    c("anim.get_state", r#"{ "entity": 4294967310 }"#, &["F0003"], &[]),
    c(
        "anim.set_param",
        r#"{ "entity": 4294967310, "name": "Speed", "value": 6.0 }"#,
        &["F0003"],
        &[],
    ),
    c("anim.spring_list", r#"{}"#, &[], &[]),
    c(
        "anim.spring_set",
        r#"{ "entity": 4294967310, "chain": "ponytail", "stiffness": 0.4, "gravity_power": 4.0 }"#,
        &["F0003"],
        &[
            range("stiffness", 0.0, 1.0),
            range("damping", 0.0, 1.0),
            items("gravity_dir", 3),
        ],
    ),
    c(
        "anim.spring_reset",
        r#"{ "entity": 4294967310 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "anim.rig_attach",
        r#"{ "entity": 4294967310, "ron": "(format_version: 1, drivers: [])", "dry_run": true }"#,
        &["F0003"],
        &[items("morphs", 1024), bytes("ron", 16777216)],
    ),
    c("anim.rig_state", r#"{ "entity": 4294967310 }"#, &["F0003"], &[]),
    c(
        "anim.profile_check",
        r#"{ "entity": 4294967310, "profile": "vrm" }"#,
        &["F0003"],
        &[bytes("ron", 16777216)],
    ),
    c(
        "anim.gait_validate",
        r#"{ "path": "assets/units/walker.gait.ron" }"#,
        &["F0003"],
        &[bytes("text", 1048576)],
    ),
    c(
        "anim.morph_weights",
        r#"{ "entity": 4294967310 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "anim.set_morph_weights",
        r#"{ "entity": 4294967310, "weights": { "blink": 1.0, "0": 0.3 } }"#,
        &["F0003"],
        &[
            items("clamp", 2),
            items("weights", 4096),
            items("normalized_sets", 64),
        ],
    ),
    c("anim.gaze_get", r#"{ "entity": 4294967310 }"#, &["F0003"], &[]),
    c(
        "anim.gaze_set",
        r#"{ "entity": 4294967310, "target": [1.0, 1.7, -2.0] }"#,
        &["F0003"],
        &[],
    ),
    c(
        "anim.bind_pose_get",
        r#"{ "entity": 4294967310 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "anim.bind_pose_set",
        r#"{ "entity": 4294967310, "scale": 1.15, "bone_scales": { "calf_l": 1.1 } }"#,
        &["F0003"],
        &[],
    ),
    heavy(
        "anim.clip_report",
        r#"{ "entity": 4294967301, "compression": true }"#,
        &["F0003"],
        &[
            range(
                "position_tolerance",
                CLIP_REPORT_TOLERANCE_MIN,
                CLIP_REPORT_TOLERANCE_MAX,
            ),
            range(
                "rotation_tolerance",
                CLIP_REPORT_TOLERANCE_MIN,
                CLIP_REPORT_TOLERANCE_MAX,
            ),
            range(
                "scale_tolerance",
                CLIP_REPORT_TOLERANCE_MIN,
                CLIP_REPORT_TOLERANCE_MAX,
            ),
            range(
                "propagated_tolerance",
                CLIP_REPORT_TOLERANCE_MIN,
                CLIP_REPORT_TOLERANCE_MAX,
            ),
            bytes("clip", 1024),
        ],
    ),
    c(
        "anim.motion_match",
        r#"{ "entity": 4294967310, "speed": 1.5 }"#,
        &["F0003"],
        &[range("speed", -100.0, 100.0), items("trajectory", 64)],
    ),
    c(
        "anim.motion_match_set",
        r#"{ "entity": 4294967310, "stay_bias": -2.0, "require": ["walk"] }"#,
        &["F0003"],
        &[
            range("search_interval", 0.0, 60.0),
            range("stay_bias", -100.0, 100.0),
            range("loop_clip_bias", -100.0, 100.0),
            items("require", 32),
            items("forbid", 32),
            items("trajectory", 64),
        ],
    ),
    c(
        "anim.motion_match_database",
        r#"{ "entity": 4294967310 }"#,
        &["F0003"],
        &[],
    ),
    c("anim.motion_match_databases", r#"{}"#, &[], &[]),
    c(
        "anim.mm_predict",
        r#"{ "entity": 4294967310 }"#,
        &["F0003"],
        &[items("times", 16)],
    ),
    c(
        "physics.raycast",
        r#"{ "origin": [0, 5, 0], "direction": [0, -1, 0], "max_distance": 100 }"#,
        &["F0003"],
        &[range("max_distance", 0.0, 1000000.0)],
    ),
    c(
        "physics.overlap",
        r#"{ "shape": { "kind": "sphere", "radius": 1.0 }, "position": [0, 0.5, 0] }"#,
        &["F0003"],
        &[],
    ),
    c(
        "physics.closest_point",
        r#"{ "point": [0, 2, 0] }"#,
        &["F0003"],
        &[],
    ),
    c("physics.stats", r#"{}"#, &["F0003"], &[]),
    // Convex decomposition: F0003 when the entity or its Collider is missing.
    heavy(
        "physics.convex_decompose",
        r#"{ "entity": 4294967296, "max_hulls": 8 }"#,
        &["F0003"],
        &[
            range("max_hulls", 1.0, 64.0),
            range("resolution", 8.0, 256.0),
            range("max_concavity", 0.0, 1.0),
            range("max_depth", 0.0, 16.0),
            range("max_hull_vertices", 0.0, 4096.0),
        ],
    ),
    c("physics.vehicles", r#"{ "entity": 4294967310 }"#, &["F0003"], &[]),
    c(
        "physics.vehicle_input",
        r#"{ "entity": 4294967310, "throttle": 1.0, "brake": 0.0, "steer": -0.5 }"#,
        &["F0003"],
        &[
            range("throttle", -1.0, 1.0),
            range("brake", 0.0, 1.0),
            range("steer", -1.0, 1.0),
        ],
    ),
    c(
        "physics.vehicle_reset",
        r#"{ "entity": 4294967310, "position": [0, 1, 0], "yaw": 0.0 }"#,
        &["F0003"],
        &[range("position", -1000000.0, 1000000.0)],
    ),
    c(
        "physics.vehicle_telemetry",
        r#"{ "entity": 4294967310 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "physics.vehicle_shift",
        r#"{ "entity": 4294967310, "gear": 2 }"#,
        &["F0003"],
        // No range on `gear`: the handler answers a bad one with `vehicle_invalid_gear`.
        &[],
    ),
    c(
        "physics.hovercraft",
        r#"{ "entity": 4294967310 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "physics.vehicle_config_check",
        r#"{ "ron": "(format_version: 1, name: \"car\", config: ())" }"#,
        &[],
        &[],
    ),
    c(
        "physics.vehicle_tire_curve",
        r#"{ "ron": "(format_version: 3, name: \"car\", config: (wheels: [(tire_model: Slip(()))]))", "wheel": 0, "points": 11 }"#,
        &["F0003"],
        &[
            range("load", 0.0, 1.0e9),
            range("max_slip_angle", 0.0, std::f64::consts::FRAC_PI_2),
            range("points", 2.0, 361.0),
        ],
    ),
    c(
        "physics.debug_draw",
        r#"{ "on": true, "kinds": ["contacts", "joints", "com"] }"#,
        &["F0003"],
        &[],
    ),
    c("physics.parented_bodies", r#"{}"#, &["F0003"], &[]),
    c(
        "physics.material_check",
        r#"{ "ron": "(format_version: 1, name: \"ice\", friction: 0.02)" }"#,
        &[],
        &[],
    ),
    c(
        "physics.material_register",
        r#"{ "ron": "(format_version: 1, name: \"ice\", friction: 0.02)" }"#,
        &[],
        &[],
    ),
    c(
        "physics.teleport",
        r#"{ "entity": 4294967296, "position": [0, 1, 0], "velocity": "clear" }"#,
        &["F0003"],
        &[range("position", -1000000.0, 1000000.0)],
    ),
    heavy("nav.get_mesh", r#"{}"#, &["F0003"], &[]),
    c(
        "nav.find_nearest",
        r#"{ "point": [1.2, 5.0, -3.0], "extents": [2, 2, 2] }"#,
        &["F0003"],
        &[range("extents", 0.0, 1000.0)],
    ),
    c(
        "nav.find_path",
        r#"{ "from": [-5, 0, -5], "to": [5, 0, 5] }"#,
        &["F0003"],
        &[],
    ),
    c(
        "nav.plan",
        r#"{ "from": [-5, 0, -5], "to": [5, 0, 5], "tunnel_pct": 75 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "nav.add_obstacle",
        r#"{ "min": [-0.5, -1, -4], "max": [0.5, 2, 4] }"#,
        &["F0003"],
        &[],
    ),
    c("nav.remove_obstacle", r#"{ "id": 0 }"#, &["F0003"], &[]),
    c("functest.list", r#"{}"#, &[], &[]),
    c("functest.run", r#"{ "filter": "smoke*" }"#, &[], &[]),
    c("functest.status", r#"{}"#, &[], &[]),
    c("functest.results", r#"{}"#, &[], &[]),
    c("functest.abort", r#"{}"#, &[], &[]),
    c(
        "functest.create",
        r#"{ "name": "door_opens", "body": "rust:my_game::DoorTest", "tags": ["doors"] }"#,
        &[],
        &[],
    ),
    c("functest.bless", r#"{ "test": "door_opens" }"#, &[], &[]),
    c("smartobject.list", r#"{}"#, &[], &[]),
    c(
        "smartobject.claim",
        r#"{ "slot": 4294967301, "agent": 4294967302 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "smartobject.release",
        r#"{ "slot": 4294967301, "agent": 4294967302 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "smartobject.search",
        r#"{ "region": { "center": [0, 0, 0], "radius": 20 }, "explain": true }"#,
        &[],
        &[],
    ),
    c(
        "smartobject.begin_use",
        r#"{ "handle": { "slot": 4294967301, "generation": 0 } }"#,
        &[],
        &[],
    ),
    c(
        "smartobject.set_enabled",
        r#"{ "entity": 4294967301, "enabled": false, "reason": "locked" }"#,
        &[],
        &[],
    ),
    c(
        "smartobject.tags",
        r#"{ "entity": 4294967301, "add": ["Activity.Sit"] }"#,
        &[],
        &[],
    ),
    c("smartobject.messages", r#"{ "last": 16 }"#, &[], &[]),
    c(
        "smartobject.set_definition",
        r#"{ "entity": 4294967301, "path": "props/chair.smartobject.ron" }"#,
        &[],
        &[],
    ),
    c(
        "statetree.compile",
        r#"{ "path": "ai/guard.statetree.ron" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "statetree.control",
        r#"{ "entity": 4294967301, "op": "pause" }"#,
        &[],
        &[],
    ),
    c(
        "statetree.reload",
        r#"{ "entity": 4294967301, "path": "ai/guard.statetree.ron" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "statetree.debug",
        r#"{ "entity": 4294967301, "disabled": ["Idle"] }"#,
        &[],
        &[],
    ),
    c(
        "logic.compile",
        r#"{ "path": "logic/gatehouse.logic.ron" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "logic.control",
        r#"{ "entity": 4294967301, "op": "pause" }"#,
        &[],
        &[],
    ),
    c(
        "logic.emit",
        r#"{ "entity": 4294967301, "event": "trigger_enter" }"#,
        &[],
        &[],
    ),
    c(
        "logic.reload",
        r#"{ "entity": 4294967301, "path": "logic/gatehouse.logic.ron" }"#,
        &["F0003"],
        &[],
    ),
    c("logic.status", r#"{ "entity": 4294967301 }"#, &[], &[]),
    c(
        "logic.debug",
        r#"{ "entity": 4294967301, "pause_on_rule": ["open_door"] }"#,
        &[],
        &[],
    ),
    c("logic.world", r#"{ "board": "gatehouse" }"#, &[], &[]),
    c(
        "logic.world_set",
        r#"{ "board": "gatehouse", "key": "alarm", "value": true }"#,
        &[],
        &[],
    ),
    c("ai.query_list", r#"{}"#, &["ai_query_unavailable"], &[]),
    heavy(
        "ai.query_run",
        r#"{ "template": "flank_position", "owner": 4294967296, "params": { "radius": 8.0 }, "pick": "best", "limit": 10 }"#,
        &[
            "ai_query_unavailable",
            "sq_bad_param",
            "sq_item_type_mismatch",
            "sq_missing_system",
            "sq_no_navmesh",
            "sq_no_threshold",
            "sq_too_many_items",
            "sq_unknown_owner",
            "sq_unknown_template",
            "sq_unreadable",
        ],
        &[],
    ),
    c(
        "ai.query_submit",
        r#"{ "template": "flank_position", "owner": 4294967296 }"#,
        &[
            "ai_query_unavailable",
            "sq_bad_param",
            "sq_item_type_mismatch",
            "sq_no_threshold",
            "sq_unknown_owner",
            "sq_unknown_template",
            "sq_unreadable",
        ],
        &[],
    ),
    c(
        "ai.query_poll",
        r#"{ "request": 1 }"#,
        &[
            "ai_query_unavailable",
            "sq_bad_param",
            "sq_missing_system",
            "sq_no_navmesh",
            "sq_too_many_items",
            "sq_unknown_request",
        ],
        &[],
    ),
    c(
        "ai.query_abort",
        r#"{ "request": 1 }"#,
        &["ai_query_unavailable", "sq_bad_param", "sq_unknown_owner"],
        &[],
    ),
    c("ai.query_stats", r#"{}"#, &["ai_query_unavailable"], &[]),
    c(
        "ai.query_probe",
        r#"{ "entity": 4294967296 }"#,
        &[
            "F0003",
            "ai_query_unavailable",
            "sq_bad_param",
            "sq_missing_system",
            "sq_no_navmesh",
            "sq_too_many_items",
        ],
        &[],
    ),
    c(
        "ai.perception_get",
        r#"{ "entity": 4294967296 }"#,
        &["F0003", "ai_perception_unavailable"],
        &[],
    ),
    c(
        "ai.attitude_set",
        r#"{ "from": 0, "to": 1, "attitude": "neutral", "symmetric": true }"#,
        &["ai_perception_unavailable"],
        &[],
    ),
    c("ai.utility_list", r#"{}"#, &["ai_utility_unavailable"], &[]),
    c(
        "ai.utility_explain",
        r#"{ "entity": 4294967296 }"#,
        &["F0003", "ai_utility_unavailable"],
        &[],
    ),
    c(
        "ai.goap_plan",
        r#"{ "entity": 4294967296 }"#,
        &["F0003", "ai_goap_unavailable"],
        &[],
    ),
    c(
        "ai.squads",
        r#"{ "squad": 4294967296 }"#,
        &["F0003", "ai_squads_unavailable"],
        &[],
    ),
    c(
        "ai.squad_set",
        r#"{ "create": true, "name": "alpha", "roles": [ { "name": "flanker", "capacity": 2 } ], "add": [ 4294967296 ] }"#,
        &["F0003", "ai_squads_unavailable"],
        &[items("add", 4096), items("remove", 4096), items("roles", 256)],
    ),
    c(
        "ai.tick_budget_get",
        r#"{ "entity": 4294967296 }"#,
        &["F0003", "ai_tick_budget_unavailable"],
        &[],
    ),
    c(
        "ai.tick_budget_set",
        r#"{ "enabled": true, "max_trees_per_step": 10, "tiers": [ { "max_distance": 30.0, "cadence_steps": 1 }, { "cadence_steps": 16 } ] }"#,
        &["F0003", "ai_tick_budget_unavailable"],
        &[items("tiers", 64)],
    ),
    c("ai.query_impls", r#"{}"#, &["ai_query_unavailable"], &[]),
    c(
        "ai.formation_get",
        r#"{ "squad": 1 }"#,
        &["ai_squads_unavailable", "unknown_squad"],
        &[],
    ),
    c(
        "ai.formation_set",
        r#"{ "squad": 1, "shape": "line" }"#,
        &[
            "ai_squads_unavailable",
            "invalid_asset",
            "unknown_shape",
            "unknown_squad",
        ],
        &[bytes("asset", 4096), bytes("shape", 64)],
    ),
    c("nav.region_status", r#"{}"#, &["F0003"], &[]),
    c("nav.area_table", r#"{}"#, &[], &[]),
    c(
        "nav.set_area_table",
        r#"{ "areas": [ { "area": 3, "cost": 4.0, "name": "mud" } ] }"#,
        &[],
        &[items("areas", 256)],
    ),
    heavy(
        "nav.region_rebuild",
        r#"{ "min": [-1, 0, -1], "max": [1, 2, 1] }"#,
        &["F0003"],
        &[],
    ),
    c("nav.stream_status", r#"{}"#, &["F0003"], &[]),
    heavy("nav.stream_begin", r#"{}"#, &["F0003"], &[]),
    c(
        "nav.stream_cook_cell",
        r#"{ "min": [-1, 0, -1], "max": [1, 0, 1] }"#,
        &["F0003"],
        &[],
    ),
    heavy(
        "nav.stream_load_cell",
        r#"{ "cell": 1, "blob": "00" }"#,
        &["F0003"],
        &[],
    ),
    heavy("nav.stream_unload_cell", r#"{ "cell": 1 }"#, &["F0003"], &[]),
    c(
        "nav.stream_find_path",
        r#"{ "start": [-3, 0, 0], "end": [3, 0, 0] }"#,
        &["F0003"],
        &[],
    ),
    c("nav.det_agents", r#"{}"#, &["F0003"], &[]),
    c("nav.area_status", r#"{}"#, &["F0003"], &[]),
    heavy(
        "nav.set_area_cost",
        r#"{ "area": 2, "cost": 3.0 }"#,
        &["F0003"],
        &[range("area", 0.0, 63.0)],
    ),
    heavy(
        "nav.add_area_volume",
        r#"{ "shape": "box", "min": [-1, -1, -1], "max": [1, 1, 1], "area": 2 }"#,
        &["F0003"],
        &[range("area", 0.0, 63.0)],
    ),
    heavy("nav.remove_area_volume", r#"{ "id": 0 }"#, &["F0003"], &[]),
    heavy(
        "nav.crowd_set_filter",
        r#"{ "id": 0, "filter": { "exclude_areas": [2] } }"#,
        &["F0003"],
        &[],
    ),
    heavy(
        "nav.lanes_set",
        r#"{ "lanes": [{ "points": [[0, 0, 0], [4, 0, 0]] }] }"#,
        &[],
        &[],
    ),
    c("nav.lanes_status", r#"{}"#, &["F0003"], &[]),
    c("nav.lane_route", r#"{ "from": 0, "to": 0 }"#, &["F0003"], &[]),
    c("nav.lane_nearest", r#"{ "point": [1, 0, 0] }"#, &["F0003"], &[]),
    c("nav.lane_check", r#"{}"#, &["F0003"], &[]),
    heavy(
        "nav.lane_set_enabled",
        r#"{ "user_id": 1, "enabled": false }"#,
        &["F0003"],
        &[],
    ),
    heavy(
        "nav.lane_add_agent",
        r#"{ "lane": 0, "distance": 1 }"#,
        &["F0003"],
        &[],
    ),
    heavy("nav.lane_remove_agent", r#"{ "id": 0 }"#, &["F0003"], &[]),
    heavy(
        "nav.lane_set_destination",
        r#"{ "id": 0, "lane": 0 }"#,
        &["F0003"],
        &[],
    ),
    heavy("nav.lane_step", r#"{ "steps": 1 }"#, &["F0003"], &[]),
    c("nav.lane_agents", r#"{}"#, &["F0003"], &[]),
    c("nav.tile_cache_status", r#"{}"#, &["F0003"], &[]),
    heavy(
        "nav.tile_cache_add_obstacle",
        r#"{ "shape": "cylinder", "base": [2, 0, 1], "radius": 0.6, "height": 1.8 }"#,
        &["F0003"],
        &[range("radius", 0.0, 1000.0), range("height", 0.0, 1000.0)],
    ),
    heavy(
        "nav.tile_cache_remove_obstacle",
        r#"{ "id": 0 }"#,
        &["F0003"],
        &[],
    ),
    c("nav.crowd_status", r#"{}"#, &["F0003"], &[]),
    c(
        "nav.crowd_add_agent",
        r#"{ "position": [-3, 0, 0], "radius": 0.35 }"#,
        &["F0003"],
        &[range("radius", 0.0, 100.0), range("height", 0.0, 100.0)],
    ),
    c(
        "nav.crowd_set_target",
        r#"{ "id": 0, "target": [3, 0, 0] }"#,
        &["F0003"],
        &[],
    ),
    heavy(
        "nav.crowd_step",
        r#"{ "steps": 60 }"#,
        &["F0003"],
        &[range("steps", 1.0, 3600.0)],
    ),
    c("nav.links", r#"{}"#, &["F0003"], &[]),
    c(
        "nav.set_links",
        r#"{ "links": [ { "from": [-1.5, 0, 0], "to": [1.5, 0, 0], "kind": "jump", "user_id": 5 } ] }"#,
        &["F0003"],
        &[items("links", 65_536)],
    ),
    c(
        "nav.set_link_enabled",
        r#"{ "user_id": 5, "enabled": false }"#,
        &["F0003"],
        &[],
    ),
    heavy(
        "nav.generate_links",
        r#"{ "max_drop": 3, "max_jump_gap": 2, "apply": false }"#,
        &["F0003"],
        &[
            range("max_drop", 0.0, 1000.0),
            range("max_jump_gap", 0.0, 1000.0),
            range("max_jump_up", 0.0, 1000.0),
            range("spacing", 0.0, 1000.0),
            range("radius", 0.0, 1000.0),
        ],
    ),
    heavy(
        "nav.tile_cache_move_obstacle",
        r#"{ "id": 0, "shape": "oriented_box", "center": [2, 0.5, 1], "half_extents": [0.5, 0.5, 0.3], "yaw": 0.6 }"#,
        &["F0003"],
        &[],
    ),
    heavy(
        "nav.crowd_set_link_queue",
        r#"{ "exclusive": true, "timeout_steps": 120 }"#,
        &["F0003"],
        &[],
    ),
    c("nav.crowd_link_queues", r#"{}"#, &["F0003"], &[]),
    heavy(
        "nav.lanes_from_splines",
        r#"{ "width": 7, "lanes_each_way": 1, "join_distance": 1, "apply": false }"#,
        &[],
        &[range("width", 0.0, 1000.0), range("max_error", 0.0, 1000.0)],
    ),
    heavy(
        "nav.cook_triangle_areas",
        r#"{ "triangles": [[[-8, 0, -8], [8, 0, -8], [8, 0, 8]], [[-8, 0, -8], [8, 0, 8], [-8, 0, 8]]], "areas": [3, 0] }"#,
        &["F0003"],
        &[items("triangles", 1_048_576)],
    ),
    heavy(
        "nav.set_link_files",
        r#"{ "links": ["levels/a.navlinks.ron"], "lanes": [] }"#,
        &["F0003"],
        &[],
    ),
    c("nav.authored_links_status", r#"{}"#, &["F0003"], &[]),
    c("nav.link_traversals", r#"{}"#, &["F0003"], &[]),
    c(
        "nav.crowd_set_link_arcs",
        r#"{ "enabled": true, "gravity": 9.81, "max_jump_speed": 6, "apex_clearance": 0.5 }"#,
        &["F0003"],
        &[],
    ),
    c("nav.tile_links", r#"{}"#, &["F0003"], &[]),
    heavy(
        "nav.tile_cache_grow",
        r#"{ "min": [-20, 0, -20], "max": [20, 0, 20], "update": true }"#,
        &["F0003"],
        &[],
    ),
    c(
        "sprite.list",
        r#"{ "limit": 500 }"#,
        &[],
        &[range("limit", 0.0, 10000.0)],
    ),
    c("sprite.get", r#"{ "entity": 4294967301 }"#, &["F0003"], &[]),
    c("tilemap.list", r#"{}"#, &[], &[]),
    c(
        "tilemap.get_tile",
        r#"{ "x": 3, "y": 7, "layer": "ground" }"#,
        &["F0003"],
        &[range("x", 0.0, 4294967295.0), range("y", 0.0, 4294967295.0)],
    ),
    c("view2d.get", r#"{}"#, &["F0003"], &[]),
    c(
        "tilemap.set_tile",
        r#"{ "layer": "ground", "x": 3, "y": 7, "gid": 5, "flip": { "x": true } }"#,
        &[
            "F0003",
            "tile_bad_gid",
            "tile_bad_params",
            "tile_not_tile_layer",
            "tile_outside_map",
        ],
        &[range("x", 0.0, 4294967295.0), range("y", 0.0, 4294967295.0)],
    ),
    c(
        "tilemap.autotile_register",
        r#"{ "name": "walls", "ron": "(version: 1, tileset: \"t\", terrains: [(name: \"wall\", layout: Edge16([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]))])" }"#,
        &["F0003", "tile_bad_params", "tile_bad_rules"],
        &[
            bytes("name", 256),
            bytes("ron", crate::tilemap_edit::MAX_RULES_BYTES),
        ],
    ),
    c("tilemap.autotile_list", r#"{}"#, &[], &[]),
    c(
        "tilemap.paint",
        r#"{ "layer": "ground", "rules": "walls", "terrain": "wall", "rect": [0, 4, 9, 4] }"#,
        &[
            "F0003",
            "tile_bad_params",
            "tile_bind_failed",
            "tile_not_tile_layer",
            "tile_outside_map",
            "tile_too_many_cells",
            "tile_unknown_rules",
            "tile_unknown_terrain",
        ],
        &[items("cells", crate::tilemap_edit::MAX_PAINT_CELLS as usize)],
    ),
    c(
        "tilemap.autotile",
        r#"{ "layer": "ground", "rules": "walls" }"#,
        &[
            "F0003",
            "tile_bad_params",
            "tile_bind_failed",
            "tile_not_tile_layer",
            "tile_unknown_rules",
        ],
        &[],
    ),
    c(
        "tilemap.stamp_copy",
        r#"{ "layer": "ground", "rect": [0, 0, 3, 2] }"#,
        &[
            "F0003",
            "tile_bad_params",
            "tile_bad_stamp",
            "tile_not_tile_layer",
            "tile_outside_map",
        ],
        &[items("rect", 4)],
    ),
    c(
        "tilemap.stamp_paste",
        r#"{ "layer": "ground", "x": 2, "y": 1, "stamp": { "width": 2, "height": 1, "cells": [1, 2] }, "op": "rotate_cw" }"#,
        &[
            "F0003",
            "tile_bad_gid",
            "tile_bad_params",
            "tile_bad_stamp",
            "tile_not_tile_layer",
            "tile_too_many_cells",
        ],
        &[items("ops", 16)],
    ),
    // Import reports: Aseprite and LDtk (book/agents/import-report.md, ADR-3358).
    c(
        "import.report",
        r#"{ "path": "assets/sprites/hero.aseprite" }"#,
        &[
            "F0016",
            "import_bad_path",
            "import_no_report",
            "import_report_invalid",
        ],
        &[bytes("path", 4096)],
    ),
    c("debug.gizmos", r#"{}"#, &["F0003"], &[]),
    c("quick.draw_list", r#"{}"#, &["F0003"], &[]),
    c("quick.draw2d_stats", r#"{}"#, &["F0003"], &[]),
    c(
        "debug.set_gizmos",
        r#"{ "group": "physics", "enabled": true, "on_top": true }"#,
        &["F0003"],
        &[range("line_width", 0.0, 64.0), range("depth_bias", -1.0, 1.0)],
    ),
    c("material.graph_get", r#"{ "name": "paint" }"#, &["F0003"], &[]),
    c(
        "material.graph_set",
        r#"{ "name": "paint", "nodes": { "vcol": "VertexColor", "rgb": { "Swizzle": { "input": "vcol", "channels": "rgb" } }, "base": { "Mul": ["tint", "rgb"] } }, "output": { "base_color": "base" } }"#,
        &["F0003"],
        &[bytes("ron", 16777216)],
    ),
    heavy(
        "material.compile",
        r#"{ "name": "paint", "include_wgsl": false }"#,
        &["F0003"],
        &[],
    ),
    c("material.shading", r#"{ "entity": 4294967301 }"#, &["F0003"], &[]),
    c(
        "material.vertex_ao",
        r#"{ "entity": 4294967301 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "material.set_vertex_ao",
        r#"{ "entity": 4294967301, "enabled": true }"#,
        &["F0003"],
        &[],
    ),
    c(
        "material.thin_film",
        r#"{ "entity": 4294967301 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "material.rough_transmission",
        r#"{ "entity": 4294967301 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "material.set_rough_transmission",
        r#"{ "entity": 4294967301, "roughness": 0.4 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "material.set_thin_film",
        r#"{ "entity": 4294967301, "factor": 1.0, "thickness_max": 520.0 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "material.subsurface",
        r#"{ "entity": 4294967301 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "material.set_subsurface",
        r#"{ "entity": 4294967301, "profile": "skin", "tint": [1.0, 0.95, 0.9] }"#,
        &["F0003"],
        &[],
    ),
    c("render.subsurface", r#"{}"#, &[], &[]),
    c(
        "render.set_subsurface",
        r#"{ "enabled": false }"#,
        &["F0003"],
        &[],
    ),
    c("render.oit", r#"{}"#, &[], &[]),
    c("render.set_oit", r#"{ "enabled": false }"#, &["F0003"], &[]),
    c(
        "material.set_shading",
        r#"{ "entity": 4294967301, "skin": { "scatter_scale": 2.0, "transmission": 0.6 } }"#,
        &["F0003"],
        &[],
    ),
    c("terrain.scatter", r#"{}"#, &["F0003"], &[]),
    c(
        "terrain.set_scatter",
        r#"{ "class": "gravel", "class_enabled": false }"#,
        &["F0003"],
        &[bytes("class", 256)],
    ),
    c(
        "render.set_contact_shadows",
        r#"{ "enabled": false }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.set_translucent_shadows",
        r#"{ "cascades": 0, "local": false }"#,
        &["F0003"],
        &[range("cascades", 0.0, 4.0)],
    ),
    c(
        "render.set_reflection_occlusion",
        r#"{ "enabled": false }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.set_bent_normals",
        r#"{ "enabled": false }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.set_near_sun_shadow",
        r#"{ "tile": 2048 }"#,
        &["F0003"],
        &[range("tile", 0.0, 8192.0)],
    ),
    c(
        "render.set_studio_environment",
        r#"{ "kind": "gradient", "zenith": [2400.0, 4200.0, 8000.0], "horizon": [2600.0, 2800.0, 3000.0], "ground": [600.0, 600.0, 600.0] }"#,
        &["F0003"],
        &[range("falloff", 0.05, 16.0)],
    ),
    c("render.get_studio_environment", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_environment_ground",
        r#"{ "height": 0.0, "albedo": [0.18, 0.18, 0.18] }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.set_studio_background",
        r#"{ "kind": "solid", "color": [40.0, 40.0, 40.0] }"#,
        &["F0003"],
        &[range("falloff", 0.05, 16.0)],
    ),
    c("render.get_debug_view", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_debug_view",
        r#"{ "mode": "image_light" }"#,
        &["F0003"],
        &[],
    ),
    c("render.lights", r#"{}"#, &["F0003"], &[]),
    c(
        "render.place_local_capture",
        r#"{ "id": 1, "center": [0.0, 1.0, 0.0], "half_extent": [3.0, 2.0, 3.0] }"#,
        &["F0003"],
        &[range("weight", 0.0, 1.0)],
    ),
    c("render.bake_local_capture", r#"{ "id": 1 }"#, &["F0003"], &[]),
    c("render.list_local_captures", r#"{}"#, &["F0003"], &[]),
    c("render.remove_local_capture", r#"{ "id": 1 }"#, &["F0003"], &[]),
    c(
        "terrain.stroke",
        r#"{ "mode": "raise", "center": [120.0, -40.0], "radius": 12.0, "strength": 4.0, "falloff": "smooth" }"#,
        &[
            "F0003",
            "edit_layer_hidden",
            "edit_layer_locked",
            "edit_layer_not_found",
            "edit_layer_owned",
        ],
        &[
            range("hardness", 0.0, 1.0),
            range("radius", 0.0, 4096.0),
            range("width", 0.0, 4096.0),
            range("length", 0.0, 4096.0),
            items("path", 256),
            range("path", -1000000.0, 1000000.0),
            range("center", -1000000.0, 1000000.0),
            range("to", -1000000.0, 1000000.0),
        ],
    ),
    c(
        "world.pick",
        r#"{ "origin": [0, 50, 0], "direction": [0, -1, 0], "layers": [0, 1] }"#,
        &[],
        &[
            items("skip", 1024),
            items("layers", 256),
            range("origin", -1000000.0, 1000000.0),
            // Inclusive range: 0 is not a length (`d > 0` in the handler). The minimum is the
            // smallest positive f64 so every length the handler accepts still passes.
            range("max_distance", f64::MIN_POSITIVE, 1000000.0),
        ],
    ),
    c("terrain.holes", r#"{}"#, &["F0003"], &[]),
    c(
        "terrain.set_holes",
        r#"{ "segments": [{ "from": [-142.8, -286.0], "to": [-170.0, -256.0], "width_m": 20.0 }] }"#,
        &["F0003"],
        &[items("shapes", 64), items("segments", 64)],
    ),
    c(
        "terrain.load_holes",
        r#"{ "path": "maps/switchbacks.level.ron" }"#,
        &["F0003"],
        &[bytes("ron", 16777216)],
    ),
    c(
        "terrain.raycast",
        r#"{ "origin": [0, 80, 0], "direction": [0, -1, 0] }"#,
        &["F0003"],
        &[
            range("origin", -1000000.0, 1000000.0),
            range("max_distance", 0.0, 100000.0),
        ],
    ),
    c("terrain.vt", r#"{}"#, &["F0003"], &[]),
    c(
        "terrain.set_vt",
        r#"{ "enabled": true, "settings": { "cache": "relief" } }"#,
        &["F0003"],
        &[],
    ),
    c("players.list", r#"{}"#, &["F0003"], &[]),
    c(
        "players.get",
        r#"{ "index": 0 }"#,
        &["F0003"],
        &[range("index", 0.0, 255.0)],
    ),
    c(
        "scene.possess",
        r#"{ "controller": 4294967297, "entity": 4294967304 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "save.save",
        r#"{ "slot": "Chapter1" }"#,
        &["F0003"],
        &[bytes("slot", 128)],
    ),
    c(
        "save.load",
        r#"{ "slot": "Chapter1" }"#,
        &["F0003"],
        &[bytes("slot", 128)],
    ),
    c("save.list_slots", r#"{}"#, &["F0003"], &[]),
    c("save.quick_save", r#"{}"#, &["F0003"], &[]),
    c("save.quick_load", r#"{}"#, &["F0003"], &[]),
    c(
        "save.delete",
        r#"{ "slot": "Chapter1" }"#,
        &["F0003"],
        &[bytes("slot", 128)],
    ),
    c("save.status", r#"{}"#, &["F0003"], &[]),
    c(
        "character.move_to",
        r#"{ "entity": 4294967304, "x": 4.0, "y": 0.0, "z": -3.0, "accept_radius": 0.5 }"#,
        &["F0003"],
        &[range("accept_radius", 0.0, 1000.0)],
    ),
    c(
        "save.inspect",
        r#"{ "slot": "Chapter1" }"#,
        &["F0003"],
        &[bytes("slot", 128)],
    ),
    c("audio.list_emitters", r#"{}"#, &["F0013"], &[]),
    c(
        "audio.set_emitter",
        r#"{ "entity": 4294967301, "sound": "rain", "volume_db": -6, "max_distance": 25 }"#,
        &["F0003", "F0013"],
        &[
            range("volume_db", -100.0, 24.0),
            range("min_distance", 0.0, 100000.0),
            range("max_distance", 0.001, 100000.0),
            range("spatial_blend", 0.0, 1.0),
            range("doppler_factor", 0.0, 10.0),
            range("cone_inner_degrees", 0.0, 360.0),
            range("cone_outer_degrees", 0.0, 360.0),
            range("cone_outer_gain", 0.0, 1.0),
        ],
    ),
    c("audio.list_zones", r#"{}"#, &["F0013"], &[]),
    c(
        "audio.set_zone",
        r#"{ "entity": 4294967310, "preset": "tube", "shape": { "kind": "capsule", "radius": 3.0, "half_length": 100.0 }, "portals": [{ "position": [0.0, 100.0, 0.0], "radius": 5.0 }] }"#,
        &["F0003", "F0013"],
        &[
            range("wet_db", -80.0, 12.0),
            range("decay", 0.0, 1.0),
            range("damping", 0.0, 1.0),
            range("stereo_width", 0.0, 1.0),
            range("distance_scale", 0.1, 10.0),
            range("blend_distance", 0.0, 10000.0),
            range("priority", -1000.0, 1000.0),
            items("portals", 64),
        ],
    ),
    c(
        "audio.probe_zones",
        r#"{ "listener": [100, 1.7, 0], "emitter": [70, -10, 0], "max_distance": 400 }"#,
        &["F0013"],
        &[
            range("min_distance", 0.0, 100000.0),
            range("max_distance", 0.001, 100000.0),
        ],
    ),
    c("loop.get", r#"{}"#, &[], &[]),
    c(
        "loop.set",
        r#"{ "mode": "run_frames", "frames": 120 }"#,
        &[],
        &[
            range("max_substeps", 1.0, regia_app::MAX_SUBSTEPS_LIMIT as f64),
            range(
                "max_fps",
                regia_app::MIN_PACING_FPS as f64,
                regia_app::MAX_PACING_FPS as f64,
            ),
            range("fixed_hz", 1.0, regia_app::MAX_FIXED_HZ),
            range(
                "throttled_fps",
                regia_app::MIN_THROTTLED_FPS as f64,
                regia_app::MAX_THROTTLED_FPS as f64,
            ),
        ],
    ),
    c("quit.request", r#"{}"#, &[], &[]),
    c("coroutines.list", r#"{}"#, &["F0003"], &[]),
    heavy(
        "replay.load",
        r#"{ "path": "saved/replays/1760000000000-ironreach-tdm.do27replay" }"#,
        &[],
        &[],
    ),
    c(
        "replay.seek",
        r#"{ "tick": 600 }"#,
        &["F0003"],
        &[range("tick", 0.0, 4294967295.0)],
    ),
    heavy(
        "replay.step",
        r#"{ "ticks": 60 }"#,
        &["F0003"],
        &[range("ticks", 0.0, 4294967295.0)],
    ),
    c(
        "replay.play",
        r#"{ "from_tick": 4280, "speed": 0.25, "start": "render_out" }"#,
        &["F0003"],
        &[
            range("from_tick", 0.0, 4294967295.0),
            range("speed", 0.001, 64.0),
            range("max_gap_ticks", 1.0, 600.0),
        ],
    ),
    c("replay.pause", r#"{}"#, &["F0003"], &[]),
    c(
        "moments.list",
        r#"{ "kind": "kill", "limit": 20 }"#,
        &["F0003"],
        &[
            range("entity", 0.0, 4294967295.0),
            range("attacker", 0.0, 4294967295.0),
            range("victim", 0.0, 4294967295.0),
            range("fight", 0.0, 4294967295.0),
            range("round", 0.0, 4294967295.0),
            range("min_importance", 0.0, 4294967295.0),
        ],
    ),
    c("moments.get", r#"{ "index": 0 }"#, &["F0003"], &[]),
    c("state.list", r#"{}"#, &[], &[]),
    c("state.get", r#"{ "state": "GameFlow" }"#, &["F0003"], &[]),
    c(
        "state.set",
        r#"{ "state": "GameFlow", "value": "Paused" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "vfx.spawn_decal",
        r#"{ "position": [1.0, 0.0, 2.0], "normal": [0, 1, 0], "prefab": "scorch" }"#,
        &["F0016"],
        &[
            range("lifetime", 0.0, 3600.0),
            range("position", -1000000.0, 1000000.0),
        ],
    ),
    c(
        "vfx.place_rut",
        r#"{ "points": [[-4, 0, 2], [0, 0, -2], [4, 0, 2]], "curve": true }"#,
        &["F0016"],
        &[
            items("points", 256),
            range("points", -100000.0, 100000.0),
            range("half_width", 0.0, 50.0),
            range("half_depth", 0.0, 50.0),
        ],
    ),
    c("vfx.decal_budget", r#"{}"#, &["F0016"], &[]),
    c("vfx.get_look", r#"{}"#, &["F0016"], &[]),
    c(
        "vfx.set_look",
        r#"{ "heat_haze": false, "fire_light_scale": 2.5 }"#,
        &["F0016"],
        &[range("haze_scale", 0.0, 4.0), range("fire_light_scale", 0.0, 8.0)],
    ),
    c("vfx.medium", r#"{}"#, &["F0016"], &[]),
    c("vfx.hover_washes", r#"{}"#, &[], &[]),
    c(
        "vfx.set_hover_wash",
        r#"{ "entity": 4294967321, "footprint_radius": 3.0, "height": 3.0, "ground": [1.0, 0.0, 2.0] }"#,
        &["F0003"],
        &[range("footprint_radius", 0.1, 50.0)],
    ),
    c(
        "vfx.hit",
        r#"{ "position": [3.1, 1.2, 0.4], "normal": [1, 0, 0], "surface": "glass", "weapon": "chaincannon", "target": 4294967321 }"#,
        &["F0003", "F0016"],
        &[range("position", -1000000.0, 1000000.0)],
    ),
    c("vfx.weapons", r#"{}"#, &[], &[]),
    c("vfx.plumes", r#"{}"#, &[], &[]),
    c(
        "vfx.set_plume",
        r#"{ "entity": 4294967330, "kind": "duct", "throttle": 0.8 }"#,
        &["F0003"],
        &[range("throttle", 0.0, 2.0)],
    ),
    c("vfx.stats", r#"{}"#, &["F0016"], &[]),
    c("vfx.systems", r#"{ "gpu": true }"#, &["F0016"], &[]),
    c(
        "vfx.describe",
        r#"{ "system": "smoke" }"#,
        &["F0003", "F0016"],
        &[],
    ),
    c(
        "vfx.spawn",
        r#"{ "asset": "fx/smoke.vfx.ron", "origin": [0.0, 1.0, 0.0], "forward": [0.0, 0.0, 1.0] }"#,
        &["F0003", "F0016"],
        &[],
    ),
    c(
        "vfx.set_param",
        r#"{ "instance": 3, "param": "intensity", "value": 0.5 }"#,
        &["F0003", "F0016"],
        &[],
    ),
    c(
        "vfx.spawn_probe",
        r#"{ "system": "smoke", "seed": 7, "origin": [0.0, 1.0, 0.0], "frames": 2, "max": 64 }"#,
        &["F0003", "F0016"],
        &[range("frames", 1.0, 600.0), range("max", 1.0, 4096.0)],
    ),
    c("vfx.fire_lights", r#"{}"#, &["F0016"], &[]),
    c(
        "vfx.spawn_crash_fires",
        r#"{ "position": [120.0, 4.0, -60.0], "count": 40, "scatter_radius": 300.0, "seed": 2026 }"#,
        &["F0016"],
        &[range("count", 0.0, 200.0), range("scatter_radius", 0.0, 5000.0)],
    ),
    c(
        "gameplay.explode",
        r#"{ "center": [0.0, 1.0, 0.0], "preset": "grenade" }"#,
        &["F0016"],
        &[range("tnt_kg", 0.0, 1000000.0), range("radius", 0.0, 500.0)],
    ),
    c("gameplay.explosion_stats", r#"{}"#, &["F0016"], &[]),
    c(
        "camera.shock_shake",
        r#"{ "distance_m": 3500.0 }"#,
        &["F0016"],
        &[range("distance_m", 0.0, 1000000000.0)],
    ),
    c(
        "camera.schedule_shock_shake",
        r#"{ "distance_m": 3500.0, "blast_frame": 0 }"#,
        &["F0016"],
        &[range("distance_m", 0.0, 1000000000.0)],
    ),
    c(
        "camera.shake",
        r#"{ "preset": "impact", "origin": [0.0, 0.0, -40.0], "falloff_radius_m": 120.0, "travel_speed_m_s": 343.0 }"#,
        &["F0016"],
        &[
            range("amplitude", 0.0, 10.0),
            range("frequency_hz", 0.0, 1000.0),
            range("duration_s", 0.0, 600.0),
            range("falloff", 0.0, 16.0),
            range("origin", -10000000.0, 10000000.0),
            range("channels", 0.0, 4294967295.0),
            range("falloff_radius_m", 0.0, 10000000.0),
            range("travel_speed_m_s", 0.0, 10000000.0),
            range("yield_kt", 0.0, 1000000.0),
            range("player", 0.0, 255.0),
        ],
    ),
    c("camera.rail_state", r#"{}"#, &["F0003", "F0016"], &[]),
    c(
        "camera.interior_state",
        r#"{}"#,
        &[
            "interior_bad_boom",
            "interior_bad_dt",
            "interior_bad_state",
            "interior_bad_zone",
        ],
        &[],
    ),
    c("camera.crane_state", r#"{}"#, &["F0003", "F0016"], &[]),
    c(
        "camera.set_rail",
        r#"{ "entity": 4294967300, "position": 12.5 }"#,
        &["F0003", "F0016"],
        &[range("position", -1000000.0, 1000000.0)],
    ),
    c(
        "camera.set_crane",
        r#"{ "entity": 4294967300, "yaw": 30.0, "pitch": 10.0, "length": 5.0 }"#,
        &["F0003", "F0016"],
        &[
            range("yaw", -3600.0, 3600.0),
            range("pitch", -3600.0, 3600.0),
            range("length", -1000000.0, 1000000.0),
            range("mount_yaw", -3600.0, 3600.0),
            range("mount_pitch", -3600.0, 3600.0),
        ],
    ),
    c("camera.shake_sensor", r#"{}"#, &["F0003", "F0016"], &[]),
    c(
        "camera.set_shake_sensor",
        r#"{ "entity": 4294967300, "channels": 2, "gain": 0.5, "combine": "max", "axes": "camera" }"#,
        &["F0003", "F0016"],
        &[range("gain", 0.0, 100.0), range("channels", 0.0, 4294967295.0)],
    ),
    c("camera.walk_bob", r#"{}"#, &["F0003", "F0016"], &[]),
    c(
        "camera.set_walk_bob",
        r#"{ "entity": 4294967300, "vertical_m": 0.012, "leg_length_m": 0.9 }"#,
        &["F0003", "F0016"],
        &[range("vertical_m", 0.0, 0.2), range("leg_length_m", 0.2, 3.0)],
    ),
    c(
        "camera.handheld",
        r#"{ "entity": 4294967300, "preset": "walking", "intensity": 0.8 }"#,
        &["F0003", "F0016"],
        &[range("intensity", 0.0, 4.0), range("stride_m", 0.2, 10.0)],
    ),
    c(
        "camera.photographic_walk",
        r#"{ "entity": 4294967300, "enable": true, "pull_focus": { "distance_m": 3.0, "duration_s": 0.8 } }"#,
        &["F0003", "F0016"],
        &[
            range("shutter_fraction", 0.0, 1.0),
            range("aperture_f_stops", 0.7, 64.0),
        ],
    ),
    c(
        "scene.season",
        r#"{ "season": "autumn" }"#,
        &["F0016"],
        &[range("year_fraction", 0.0, 1.0)],
    ),
    c(
        "render.foliage_shadow",
        r#"{ "softness": 0.25, "alpha_tested": true }"#,
        &["F0003"],
        &[range("softness", 0.0, 0.5)],
    ),
    c(
        "render.capture_plan",
        r#"{ "preset": "showcase_4k60", "name": "forest", "duration_s": 20 }"#,
        &[],
        &[range("bitrate_mbps", 1.0, 500.0)],
    ),
    c("camera.assets", r#"{}"#, &["F0016"], &[]),
    c(
        "camera.validate",
        r#"{ "asset": "third_person" }"#,
        &["F0016", "camera_unknown_asset"],
        &[],
    ),
    c(
        "camera.inspect",
        r#"{ "entity": 4294967300 }"#,
        &["F0003", "F0016", "camera_no_driver"],
        &[],
    ),
    c(
        "camera.set_var",
        r#"{ "entity": 4294967300, "name": "aiming", "value": true }"#,
        &[
            "F0003",
            "F0016",
            "camera_asset_invalid",
            "camera_bad_value",
            "camera_no_driver",
            "camera_unknown_name",
        ],
        &[],
    ),
    c(
        "camera.set_param",
        r#"{ "entity": 4294967300, "name": "boom_length", "value": 5.0 }"#,
        &[
            "F0003",
            "F0016",
            "camera_asset_invalid",
            "camera_bad_value",
            "camera_no_driver",
            "camera_unknown_name",
        ],
        &[],
    ),
    c(
        "camera.set_input",
        r#"{ "entity": 4294967300, "slot": "look", "value": [0.5, 0.0] }"#,
        &[
            "F0003",
            "F0016",
            "camera_asset_invalid",
            "camera_bad_value",
            "camera_no_driver",
            "camera_unknown_name",
        ],
        &[],
    ),
    c(
        "camera.reset_input",
        r#"{ "entity": 4294967300, "slot": "look" }"#,
        &[
            "F0003",
            "F0016",
            "camera_asset_invalid",
            "camera_no_driver",
            "camera_unknown_name",
        ],
        &[],
    ),
    c(
        "camera.push_overlay",
        r#"{ "entity": 4294967300, "rig": "aim", "transition": { "blend": "smooth", "seconds": 0.25 } }"#,
        &[
            "F0003",
            "F0016",
            "camera_asset_invalid",
            "camera_bad_value",
            "camera_no_driver",
            "camera_unknown_rig",
        ],
        &[],
    ),
    c(
        "camera.add_additive",
        r#"{ "entity": 4294967300, "rig": "bump", "order": 3 }"#,
        &[
            "F0003",
            "F0016",
            "camera_asset_invalid",
            "camera_no_driver",
            "camera_order_taken",
            "camera_unknown_rig",
        ],
        &[],
    ),
    c(
        "camera.release_layer",
        r#"{ "entity": 4294967300, "id": 1, "instant": false }"#,
        &[
            "F0003",
            "F0016",
            "camera_asset_invalid",
            "camera_no_driver",
            "camera_unknown_layer_id",
            "camera_unknown_rig",
        ],
        &[],
    ),
    c(
        "camera.hold_overlay",
        r#"{ "entity": 4294967300, "id": 1 }"#,
        &[
            "F0003",
            "F0016",
            "camera_asset_invalid",
            "camera_no_driver",
            "camera_unknown_layer_id",
        ],
        &[],
    ),
    c(
        "camera.load",
        r#"{ "path": "camera/player.camera.ron" }"#,
        &[
            "F0003",
            "F0016",
            "camera_asset_invalid",
            "camera_bad_path",
            "camera_file_not_found",
        ],
        &[],
    ),
    c(
        "camera.set_driver",
        r#"{ "entity": 4294967300, "asset": "third_person", "enabled": true }"#,
        &[
            "F0003",
            "F0016",
            "camera_asset_invalid",
            "camera_no_driver",
            "camera_unknown_asset",
            "camera_unknown_hook",
        ],
        &[],
    ),
    c(
        "vfx.debris_burst",
        r#"{ "origin": [0.0, 0.0, 0.0], "seed": 7, "start_frame": 0, "tier": "High" }"#,
        &["F0016"],
        &[
            range("origin", -1000000.0, 1000000.0),
            range("gravity", 0.1, 100.0),
            range("fps", 1.0, 1000.0),
        ],
    ),
    c("vfx.debris_state", r#"{}"#, &["F0016"], &[]),
    c(
        "vfx.debris_clock",
        r#"{ "frame": 120, "auto": false }"#,
        &["F0016"],
        &[],
    ),
    c(
        "vfx.impact_site",
        r#"{ "center": [0.0, 0.0, 0.0], "at_frame": 120 }"#,
        &["F0016"],
        &[
            range("center", -1000000.0, 1000000.0),
            range("radius", 1.0, 5000.0),
            range("depth", 0.1, 2000.0),
            range("rim_height", 0.0, 500.0),
            range("blanket_radius", 1.0, 20000.0),
            range("scorch_radius", 1.0, 50000.0),
        ],
    ),
    c(
        "vfx.entry_state",
        r#"{ "track": [{ "time": 0, "position": [90000, 80000, 107000] }, { "time": 120, "position": [700, 105.7, 5] }], "time": 5.5, "tier": "full" }"#,
        &["F0003"],
        &[items("track", 64), range("time", -100000.0, 100000.0)],
    ),
    c("render.skinning", r#"{}"#, &[], &[]),
    c(
        "render.set_skinning",
        r#"{ "entity": 4294967301, "method": "dual_quaternion" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.glass_view",
        r#"{ "entity": 4294967321 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.set_glass_view",
        r#"{ "entity": 4294967321, "mode": "viewer_inside", "inside_specular": 0.15 }"#,
        &["F0003"],
        &[range("inside_specular", 0.0, 1.0)],
    ),
    c("render.canopy_glass", r#"{}"#, &[], &[]),
    c(
        "render.set_canopy_glass",
        r#"{ "shading": "cheap", "nearest_layer_only": true }"#,
        &[],
        &[],
    ),
    c(
        "render.glass_damage",
        r#"{ "entity": 4294967321, "set": "laminate", "stamp": { "point": [0.0, 1.2, 0.5], "energy": 1.0 } }"#,
        &["F0003"],
        &[range("level", 0.0, 3.0)],
    ),
    c(
        "render.mask_decals",
        r#"{ "entity": 4294967301 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.set_mask_decals",
        r##"{ "entity": 4294967301, "palette": { "primary": "#8A2B1E", "emissive_intensity": 2.0 }, "decals": [{ "uv_rect": [0.1, 0.1, 0.4, 0.3], "cell": 2, "blend": "alpha" }] }"##,
        &["F0003"],
        &[items("decals", 8)],
    ),
    c("render.hair_get", r#"{ "entity": 4294967297 }"#, &["F0003"], &[]),
    c(
        "render.hair_set",
        r#"{ "entity": 4294967297, "alpha": "hashed", "primary_shift": -0.1, "tip_color": [0.3, 0.2, 0.1], "tip_alpha": 0.4 }"#,
        &["F0003"],
        &[
            range("primary_shift", -1.0, 1.0),
            range("secondary_shift", -1.0, 1.0),
            range("primary_exponent", 1.0, 2048.0),
            range("secondary_exponent", 1.0, 2048.0),
            range("primary_strength", 0.0, 8.0),
            range("secondary_strength", 0.0, 8.0),
            range("transmission", 0.0, 1.0),
            range("shift_noise", 0.0, 1.0),
            range("streak_density", 1.0, 1024.0),
            range("root_alpha", 0.0, 1.0),
            range("tip_alpha", 0.0, 1.0),
        ],
    ),
    c("render.draw_integrity", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_skinned_bounds",
        r#"{ "enabled": true }"#,
        &["F0003"],
        &[],
    ),
    c("render.texture_sets", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_texture_set_census",
        r#"{ "enabled": true }"#,
        &["F0003"],
        &[],
    ),
    c("audio.medium", r#"{}"#, &["F0013"], &[]),
    c(
        "audio.propagation",
        r#"{ "position": [500, 0, 0], "category": "explosion" }"#,
        &["F0003", "F0013"],
        &[range("max_distance", 0.001, 100000.0)],
    ),
    heavy(
        "audio.loopgen_render",
        r#"{ "path": "audio/wind.loopgen.ron", "wav": true }"#,
        &["F0003"],
        &[bytes("ron", 16777216)],
    ),
    c("render.get_visbuffer", r#"{}"#, &[], &[]),
    c("render.get_lightmaps", r#"{}"#, &[], &[]),
    c(
        "render.set_lightmaps",
        r#"{ "enabled": true, "intensity": 1.0 }"#,
        &["F0003"],
        &[
            range("intensity", 0.0, 1000.0),
            bytes("path", 4096),
            items("receivers", 4096),
        ],
    ),
    heavy(
        "render.lightmap_encode_report",
        r#"{ "encoding": "bc6h" }"#,
        &["F0003"],
        &[bytes("encoding", 16)],
    ),
    c("audio.mix", r#"{ "reset_meter": false }"#, &["F0013"], &[]),
    c("audio.bus_meters", r#"{ "floor": true }"#, &["F0013"], &[]),
    c(
        "audio.program_loudness",
        r#"{ "enable": true, "reset": false }"#,
        &["F0013"],
        &[],
    ),
    c(
        "audio.set_bus",
        r#"{ "bus": "Music", "volume_db": -6, "fade_ms": 250 }"#,
        &["F0003", "F0013"],
        &[
            range("volume_db", -100.0, 24.0),
            range("fade_ms", 0.0, 10000.0),
            bytes("bus", 256),
        ],
    ),
    c(
        "audio.set_limiter",
        r#"{ "enabled": true, "ceiling_db": -1, "release_ms": 80 }"#,
        &["F0013"],
        &[range("ceiling_db", -24.0, 0.0), range("release_ms", 1.0, 2000.0)],
    ),
    heavy(
        "audio.render_check",
        r#"{ "seconds": 0.5 }"#,
        &["F0013"],
        &[range("seconds", 0.001, 10.0), bytes("out_dir", 4096)],
    ),
    // Voice capture and loopback (book/agents/voice.md, specs/16 § 3b).
    c("audio.voice", r#"{}"#, &["F0013"], &[]),
    c(
        "audio.capture_start",
        r#"{ "sample_rate": 48000 }"#,
        &["F0013", "capture_headless"],
        &[
            bytes("device", 256),
            range("sample_rate", 8_000.0, 192_000.0),
            range("buffer_secs", 0.05, 30.0),
            range("on_threshold_db", -90.0, 0.0),
            range("off_threshold_db", -90.0, 0.0),
            range("hangover_ms", 0.0, 5_000.0),
            range("window_ms", 1.0, 2_000.0),
        ],
    ),
    c("audio.capture_stop", r#"{}"#, &["F0013"], &[]),
    c("audio.set_talking", r#"{ "talking": true }"#, &["F0013"], &[]),
    c(
        "audio.voice_loopback_start",
        r#"{ "prebuffer_ms": 40 }"#,
        &["F0013"],
        &[range("prebuffer_ms", 0.0, 2_000.0)],
    ),
    c("audio.voice_loopback_stop", r#"{}"#, &["F0013"], &[]),
    c(
        "audio.voice_place",
        r#"{ "mode": "spatial", "gain": 0.8, "pan": -0.5, "cutoff_hz": 4000 }"#,
        &["F0013"],
        &[
            range("gain", 0.0, 1.0),
            range("pan", -1.0, 1.0),
            range("cutoff_hz", 1.0, 20_000.0),
        ],
    ),
    // Voice chat (book/agents/voice-chat.md, specs/59).
    c("voice_chat.status", r#"{}"#, &["F0013"], &[]),
    c(
        "voice_chat.set_channel",
        r#"{ "channel": 2, "join": true }"#,
        &["F0013"],
        &[range("channel", 0.0, 65_535.0), range("radius", 0.0, 1_000_000.0)],
    ),
    c(
        "voice_chat.mute",
        r#"{ "target": "self", "muted": true }"#,
        &["F0013"],
        &[range("volume_pct", 0.0, 200.0)],
    ),
    c("voice_chat.loopback", r#"{ "enabled": false }"#, &["F0013"], &[]),
    c("voice_chat.stats", r#"{}"#, &["F0013"], &[]),
    c(
        "voice_chat.test_source",
        r#"{ "kind": "silence" }"#,
        &["F0013"],
        &[
            range("freq_hz", 20.0, 20_000.0),
            range("level_db", -120.0, 0.0),
            range("as_sender", 0.0, 65_535.0),
        ],
    ),
    c(
        "voice_chat.simulate_network",
        r#"{ "loss_pct": 0, "latency_ms": 0, "seed": 1 }"#,
        &["F0013"],
        &[
            range("loss_pct", 0.0, 95.0),
            range("burst_len", 0.0, 1_000.0),
            range("reorder_pct", 0.0, 100.0),
            range("dup_pct", 0.0, 100.0),
            range("latency_ms", 0.0, 5_000.0),
            range("jitter_ms", 0.0, 5_000.0),
        ],
    ),
    c(
        "voice_chat.mic",
        r#"{ "enabled": false }"#,
        &["F0013"],
        &[
            range("rate", 8_000.0, 192_000.0),
            range("channels", 1.0, 8.0),
            range("freq_hz", 20.0, 20_000.0),
            range("level_db", -120.0, 0.0),
        ],
    ),
    c(
        "voice_chat.settings",
        r#"{ "mode": "push_to_talk" }"#,
        &["F0013"],
        &[
            range("input_gain_db", -60.0, 24.0),
            range("output_gain_db", -60.0, 24.0),
        ],
    ),
    c("voice_chat.talk", r#"{ "pressed": false }"#, &["F0013"], &[]),
    c("voice_chat.relay", r#"{}"#, &["F0013"], &[]),
    c("voice_chat.processing", r#"{ "agc": false }"#, &["F0013"], &[]),
    heavy(
        "terrain.parity",
        r#"{ "eye": [0.0, 0.0] }"#,
        &["F0003"],
        &[items("eye", 2), range("eye", -1000000.0, 1000000.0)],
    ),
    // Contracts for the methods merged from the other v0.1.29 integrators (ADR-1363): the
    // example names the required params only and the errors are the protocol codes the
    // handler module can answer with; refine them with the method's book page.
    c("ui.nav_graph", r#"{}"#, &["ui_not_installed"], &[]),
    c(
        "ui.nav_move",
        r#"{ "direction": "down" }"#,
        &["no_focusables", "ui_not_installed", "unknown_direction"],
        &[],
    ),
    c(
        "ui.nav_lint",
        r#"{}"#,
        &["not_navigable", "ui_not_installed"],
        &[],
    ),
    c("render.get_exposure_settings", r#"{}"#, &["F0003"], &[]),
    c("render.set_exposure_settings", r#"{}"#, &["F0003"], &[]),
    c("render.get_clouds", r#"{}"#, &[], &[]),
    c("render.set_clouds", r#"{}"#, &[], &[]),
    c("render.get_valley_mist", r#"{}"#, &[], &[]),
    c("render.set_valley_mist", r#"{}"#, &[], &[]),
    c("render.get_weather", r#"{}"#, &[], &[]),
    c("render.set_weather", r#"{}"#, &[], &[]),
    c("render.scene_wind", r#"{}"#, &[], &[]),
    c(
        "render.set_scene_wind",
        r#"{ "mode": "manual", "wind_m_per_s": [4.0, 1.0] }"#,
        &[],
        &[],
    ),
    c("render.precipitation", r#"{}"#, &[], &[]),
    c("render.set_precipitation", r#"{ "enabled": true }"#, &[], &[]),
    c("render.surface_weather", r#"{}"#, &[], &[]),
    c(
        "render.set_surface_weather",
        r#"{ "mode": "manual", "wetness": 1.0 }"#,
        &[],
        &[],
    ),
    // F0002 with data.reason occlusion_off when the frame has no occlusion view.
    c(
        "render.surface_exposure",
        r#"{ "points": [[0.0, 0.0, 0.0]] }"#,
        &["F0003"],
        &[],
    ),
    c(
        "ui.motion",
        r#"{}"#,
        &["bad_action", "bad_entity", "unknown_entity"],
        &[],
    ),
    c(
        "ui.shortcuts",
        r#"{}"#,
        &["F0003", "F0013", "shortcuts_not_installed"],
        &[],
    ),
    c(
        "ui.shortcut_trigger",
        r#"{ "action": "save" }"#,
        &[
            "F0003",
            "F0013",
            "shortcut_ambiguous",
            "shortcut_inactive",
            "shortcuts_not_installed",
            "unknown_action",
        ],
        &[],
    ),
    c(
        "ui.shortcut_contexts",
        r#"{ "contexts": ["game"] }"#,
        &["F0003", "F0013", "shortcuts_not_installed"],
        &[],
    ),
    c(
        "ui.set_default_font",
        r#"{ "family": null }"#,
        &["bad_family", "ui_not_installed"],
        &[],
    ),
    c("ui.text_shape", r#"{}"#, &["ui_not_installed"], &[]),
    c(
        "ui.text_breaks",
        r#"{ "text": "ฉันกินข้าว" }"#,
        &["bad_locale", "bad_text", "text_too_long", "ui_text_missing"],
        &[],
    ),
    c(
        "ui.inspect",
        r#"{ "node": 4294967310 }"#,
        &[
            "F0003",
            "ui_bad_param",
            "ui_node_ambiguous",
            "ui_node_not_found",
            "ui_node_required",
        ],
        &[],
    ),
    c(
        "ui.pick_at",
        r#"{ "x": 120.0, "y": 48.0, "stack": true }"#,
        &["ui_bad_param"],
        &[],
    ),
    c(
        "ui.highlight",
        r#"{ "node": 4294967310 }"#,
        &[
            "F0003",
            "ui_bad_param",
            "ui_node_ambiguous",
            "ui_node_not_found",
            "ui_node_required",
        ],
        &[],
    ),
    c(
        "ui.a11y_tree",
        r#"{}"#,
        &["bad_entity", "not_in_tree", "ui_not_installed", "unknown_entity"],
        &[],
    ),
    c(
        "ui.a11y_announce",
        r#"{ "text": "Saved" }"#,
        &[
            "a11y_text_missing",
            "bad_priority",
            "bad_text",
            "text_too_long",
            "ui_not_installed",
        ],
        &[],
    ),
    c(
        "ui.a11y_act",
        r#"{ "node": 4294967296, "action": "press" }"#,
        &[
            "a11y_action_missing",
            "a11y_node_missing",
            "a11y_value_missing",
            "bad_action",
            "bad_entity",
            "bad_value",
            "no_scroll_view",
            "node_disabled",
            "node_inert",
            "not_dismissable",
            "text_too_long",
            "ui_not_installed",
            "unknown_node",
            "unsupported_action",
        ],
        &[],
    ),
    c("ui.a11y_platform_tree", "{}", &["platform_bridge_off"], &[]),
    c(
        "ui.a11y_platform_connect",
        r#"{ "connected": true }"#,
        &["bad_connected", "platform_bridge_off"],
        &[],
    ),
    c(
        "ui.a11y_platform_action",
        r#"{ "node": 4294967296, "action": "click" }"#,
        &[
            "a11y_action_missing",
            "a11y_node_missing",
            "a11y_value_missing",
            "bad_action",
            "bad_entity",
            "bad_value",
            "platform_bridge_off",
            "text_too_long",
            "unknown_node",
            "unsupported_action",
        ],
        &[],
    ),
    c(
        "ui.a11y_audit",
        r#"{ "rules": ["a11y_text_contrast"] }"#,
        &[
            "bad_entity",
            "bad_profile",
            "ui_not_installed",
            "unknown_entity",
            "unknown_rule",
        ],
        &[
            Limit::Items {
                param: "rules",
                max: 16,
            },
            Limit::Range {
                param: "min_target",
                min: 1.0,
                max: 512.0,
            },
            Limit::Range {
                param: "text_scale",
                min: 1.0,
                max: 3.0,
            },
        ],
    ),
    c(
        "ui.theme_mode",
        r#"{}"#,
        &[
            "bad_duration",
            "bad_ease",
            "bad_flag",
            "bad_follow",
            "bad_scheme",
            "bad_space",
            "bad_theme",
            "bad_transition",
            "theme_and_follow",
            "ui_not_installed",
            "unknown_theme",
        ],
        &[],
    ),
    c("ui.icons", r#"{}"#, &["ui_not_installed"], &[]),
    c("ui.glyph_atlas", r#"{}"#, &["ui_not_installed"], &[]),
    c(
        "ui.open_popup",
        r#"{ "anchor": 4294967296 }"#,
        &["F0003", "F0013"],
        &[],
    ),
    c("ui.close_popup", r#"{}"#, &["F0003", "F0013"], &[]),
    c("ui.menu", r#"{}"#, &["F0003", "F0013"], &[]),
    c("ui.menu_activate", r#"{ "path": [0] }"#, &["F0003", "F0013"], &[]),
    c("ui.theme_list", r#"{}"#, &["F0003", "F0013"], &[]),
    c("ui.theme_get", r#"{}"#, &["F0003", "F0013"], &[]),
    c("ui.theme_set", r#"{}"#, &["F0003", "F0013"], &[]),
    c(
        "ui.theme_select",
        r#"{ "theme": "dark" }"#,
        &["F0003", "F0013"],
        &[],
    ),
    c("ui.theme_preview", r#"{}"#, &["F0003", "F0013"], &[]),
    c("ui.theme_lint", r#"{}"#, &["F0003", "F0013"], &[]),
    c("ui.theme_reload", r#"{}"#, &["F0003", "F0013"], &[]),
    c("ui.drag_drop", r#"{}"#, &["F0003", "F0013"], &[]),
    c("ui.set_locale", r#"{ "locale": "fr" }"#, &["F0003", "F0013"], &[]),
    c(
        "ui.resolve",
        r#"{ "key": "menu.play" }"#,
        &["F0003", "F0013"],
        &[],
    ),
    c(
        "ui.set_backdrop_blur",
        r#"{ "name": "Panel", "radius": 8 }"#,
        &["F0003", "F0013"],
        &[],
    ),
    c("ui.backdrop_stats", r#"{}"#, &["F0013"], &[]),
    c("level.get_baked_gi_stream", r#"{}"#, &[], &[]),
    c("level.set_baked_gi_stream", r#"{}"#, &["F0003"], &[]),
    c(
        "level.sample_baked_gi",
        r#"{ "points": [[0.0, 1.0, 0.0]] }"#,
        &["F0003"],
        &[],
    ),
    c("level.export_bake", r#"{}"#, &["F0003"], &[]),
    c(
        "render.sample_baked_gi",
        r#"{ "points": [{ "p": [0.0, 1.0, 0.0] }] }"#,
        &["F0003"],
        &[],
    ),
    c("render.get_baked_gi_visibility", r#"{}"#, &["F0003"], &[]),
    c("render.set_baked_gi_visibility", r#"{}"#, &["F0003"], &[]),
    c("render.get_gi_probes", r#"{}"#, &["F0003"], &[]),
    c(
        "render.explain_gi",
        r#"{ "p": [0.0, 1.0, 0.0] }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.set_gi_debug_overlay",
        r#"{ "mode": "validity" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.set_baked_gi_ao",
        r#"{ "probe_relative": true }"#,
        &["F0003"],
        &[],
    ),
    c("render.set_gi_sampling", r#"{ "zh3": true }"#, &["F0003"], &[]),
    c("terrain.water", r#"{}"#, &["F0003"], &[]),
    c("terrain.set_water", r#"{}"#, &["F0003"], &[]),
    c("terrain.grass", r#"{}"#, &["F0003"], &[]),
    c(
        "terrain.set_grass",
        r#"{ "quality": "high", "shadows": true }"#,
        &["F0003"],
        &[
            range("density_scale", 0.0, 1.0),
            range("range_scale", 0.1, 4.0),
            range("sway_scale", 0.0, 4.0),
        ],
    ),
    c("render.get_frame_stats", r#"{}"#, &["F0003"], &[]),
    c("render.get_capabilities", r#"{}"#, &["F0003"], &[]),
    c("render.texture_residency", r#"{}"#, &["F0003"], &[]),
    c("render.bindless_report", r#"{}"#, &["F0003"], &[]),
    c("render.hwrt", r#"{}"#, &[], &[]),
    c("render.blue_noise", r#"{}"#, &[], &[]),
    c(
        "render.set_blue_noise",
        r#"{ "enabled": false }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.set_sun_shadow_trace",
        r#"{ "mode": "off" }"#,
        &["F0003"],
        &[],
    ),
    c("render.frame_pipelining", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_frame_pipelining",
        r#"{ "parallel_shadow_plan": false }"#,
        &["F0003"],
        &[],
    ),
    c("render.get_probe_specular", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_probe_specular",
        r#"{ "mode": "lobe" }"#,
        &["F0003"],
        &[],
    ),
    c("render.probe_specular_lobe", r#"{ "roughness": 0.5 }"#, &[], &[]),
    c("seeing.gi_denoise_check", r#"{}"#, &[], &[]),
    c("render.material_clock", r#"{}"#, &["F0003"], &[]),
    c("render.shadow_stability", r#"{}"#, &["F0003"], &[]),
    c("render.frame_gen", r#"{}"#, &["F0003", "F0013"], &[]),
    c("render.set_frame_gen", r#"{}"#, &["F0003", "F0013"], &[]),
    c("render.get_upscalers", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_upscaler",
        r#"{"backend":"tsr"}"#,
        &["F0003", "F0013"],
        &[],
    ),
    c("render.reference_upscale", r#"{}"#, &[], &[]),
    c("render.upscaler_jitter", r#"{}"#, &[], &[]),
    c("render.set_visbuffer", r#"{}"#, &["F0003"], &[]),
    c("render.get_exposure", r#"{}"#, &["F0003"], &[]),
    c("render.temporal_quality", r#"{}"#, &["unknown_scenario"], &[]),
    c("render.exposure_log", r#"{ "action": "start" }"#, &["F0003"], &[]),
    c("ui.key", r#"{ "keys": "Ctrl+C" }"#, &["F0003"], &[]),
    c("ui.text_state", r#"{}"#, &["F0003"], &[]),
    c("ui.text_select", r#"{ "caret": 3 }"#, &["F0003"], &[]),
    c(
        "ui.scroll",
        r#"{ "entity": 4294967296 }"#,
        &[
            "no_bar_parent",
            "not_scrollable",
            "unknown_entity",
            "unknown_scroll_style",
        ],
        &[],
    ),
    c("lifecycle.get", r#"{}"#, &[], &[]),
    c("lifecycle.drill", r#"{}"#, &[], &[]),
    c("thermal.get", r#"{}"#, &[], &[]),
    c("thermal.set_override", r#"{}"#, &[], &[]),
    c("input.touch_controls", r#"{}"#, &[], &[]),
    c("pacing.get", r#"{}"#, &[], &[]),
    c("pacing.set", r#"{ "mode": "interval", "interval": 2 }"#, &[], &[]),
    c(
        "pacing.simulate",
        r#"{ "refresh_hz": 120, "frame_ms": [8, 8, 20], "frames": 600, "cap_fps": 60 }"#,
        &[],
        &[
            range("refresh_hz", 5.0, 500.0),
            range("frames", 1.0, 20000.0),
            range("cap_fps", 1.0, 10000.0),
            range("interval", 1.0, 8.0),
            range("switch_hz", 5.0, 500.0),
            items("frame_ms", 4096),
            range("frame_ms", 0.0, 10000.0),
            range("swapchain_images", 2.0, 8.0),
            range("gpu_ms", 0.0, 10000.0),
            range("latch_ms", 0.0, 1000.0),
            range("stretch", 0.0, 0.95),
        ],
    ),
    c(
        "pacing.trace",
        r#"{ "frames": 60 }"#,
        &[],
        &[range("frames", 1.0, 1024.0)],
    ),
    c("executor.get", r#"{}"#, &[], &[]),
    c(
        "executor.set",
        r#"{ "executor": "single_threaded" }"#,
        &[],
        &[bytes("executor", 32)],
    ),
    c("xr.get", r#"{}"#, &[], &[]),
    c("xr.drive", r#"{ "action": "ready" }"#, &[], &[]),
    c(
        "xr.set_option",
        r#"{ "key": "render_scale", "value": 1.2 }"#,
        &[],
        &[bytes("key", 64)],
    ),
    c("xr.recenter", r#"{}"#, &[], &[]),
    c("xr.capture_eye", r#"{ "eye": "both" }"#, &[], &[]),
    c("xr.layers", r#"{}"#, &[], &[]),
    c("xr.set_quad", r#"{ "id": 1 }"#, &[], &[]),
    c("xr.walk_start", r#"{ "position": [0, 0] }"#, &[], &[]),
    c("xr.walk_set", r#"{ "locomotion": "teleport" }"#, &[], &[]),
    c("render.swapchain", r#"{}"#, &[], &[]),
    c("platform.paths", r#"{}"#, &[], &[]),
    c("sim.state_hash", r#"{}"#, &[], &[]),
    c("sim.state_diff", r#"{ "against": {} }"#, &[], &[]),
    // Gameplay tags (ADR-1463): F0003 when the app has no `TagPlugin` or the entity is gone.
    c(
        "tags.list",
        r#"{ "prefix": "Combat" }"#,
        &["F0003"],
        &[bytes("prefix", 256)],
    ),
    c("tags.get", r#"{ "entity": 4294967296 }"#, &["F0003"], &[]),
    c(
        "tags.set",
        r#"{ "entity": 4294967296, "tags": ["Combat.Damage.Fire"], "mode": "add" }"#,
        &["F0003"],
        &[items("tags", 256)],
    ),
    c(
        "tags.query",
        r#"{ "entity": 4294967296, "query": { "Has": "Combat.Damage" } }"#,
        &["F0003"],
        &[],
    ),
    c("tags.validate", r#"{}"#, &["F0003"], &[]),
    c("memory.report", r#"{}"#, &[], &[]),
    c("memory.mark", r#"{}"#, &[], &[]),
    // Froxel fog local lights (ADR-1365): F0003 when the app has no `FogLightPick`.
    c("render.fog_lights", r#"{}"#, &["F0003"], &[]),
    c("render.stats", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_skinned_instancing",
        r#"{ "enabled": false }"#,
        &["F0003"],
        &[],
    ),
    c(
        "seeing.list_resources",
        r#"{ "category": "render_targets" }"#,
        &[],
        &[],
    ),
    heavy(
        "seeing.dump_resource",
        r#"{ "label": "frame hdr", "mip": 0, "format": "exr" }"#,
        &[
            "F0011",
            "format_unsupported",
            "readback_failed",
            "resource_not_copyable",
            "resource_not_found",
            "subresource_out_of_range",
        ],
        &[bytes("out_dir", 4096)],
    ),
    c("water.state", r#"{}"#, &[], &[]),
    c(
        "water.probe",
        r#"{ "at": [[12.0, -3.5]] }"#,
        &[],
        &[items("at", 64), range("at", -1000000.0, 1000000.0)],
    ),
    c(
        "water.set",
        r#"{ "tier": "auto", "debug": "off" }"#,
        &["F0003"],
        &[range("tick", 0.0, 1.0e15)],
    ),
    c(
        "water.surface",
        r#"{ "at": [[12.0, -3.5], [40.0, 8.0]] }"#,
        &[],
        &[items("at", 64), range("at", -1000000.0, 1000000.0)],
    ),
    c(
        "water.raycast",
        r#"{ "origin": [0.0, 30.0, -40.0], "direction": [0.0, -0.4, 1.0], "max_distance": 500.0 }"#,
        &[],
        &[
            range("origin", -1000000.0, 1000000.0),
            range("direction", -1.0e6, 1.0e6),
            range("max_distance", 0.0, 100000.0),
        ],
    ),
    c("water.floaters", r#"{}"#, &["F0003"], &[]),
    c("water.ocean", r#"{}"#, &["F0003"], &[]),
    c("water.ocean_check", r#"{}"#, &["F0003"], &[]),
    c(
        "water.set_mode",
        r#"{ "mode": "fft", "grid": 512 }"#,
        &["F0003"],
        &[range("grid", 0.0, 1024.0)],
    ),
    heavy(
        "water.ocean_cost",
        r#"{ "grids": [256, 512, 1024], "frames": 32 }"#,
        &["F0003"],
        &[items("grids", 4), range("frames", 1.0, 256.0)],
    ),
    c(
        "water.set_caustics",
        r#"{ "enabled": true, "strength": 1.0, "shaft_steps": 12 }"#,
        &["F0003"],
        &[
            range("strength", 0.0, 4.0),
            range("scale", 0.1, 10.0),
            range("speed", 0.0, 8.0),
            range("chromatic", 0.0, 1.0),
            range("shaft_steps", 0.0, 16.0),
        ],
    ),
    c(
        "water.caustics_stats",
        r#"{ "depth": 1.0 }"#,
        &["F0003"],
        &[range("depth", 0.0, 20.0)],
    ),
    c(
        "vfx.crash",
        r#"{ "position": [700.0, 105.7, 5.0], "ground": "snow", "yield_t": 250.0, "wind": [3.0, 0.0, 1.0], "impact_in": 1.0 }"#,
        &["F0016"],
        &[
            range("position", -1000000.0, 1000000.0),
            range("yield_t", 0.05, 1000.0),
            range("impact_in", 0.0, 600.0),
            range("contact_lead", 0.0, 1.0),
            items("gust_sites", 256),
            range("gust_sites", -1000000.0, 1000000.0),
        ],
    ),
    c("vfx.crashes", r#"{}"#, &[], &[]),
    c(
        "vfx.crash_profile",
        r#"{ "effect": "crash_column", "yield_t": 250.0, "times": [30.0, 60.0], "wind": [4.0, 0.0, 0.0] }"#,
        &[],
        &[
            items("times", 64),
            range("times", 0.0, 240.0),
            range("yield_t", 0.05, 1000.0),
        ],
    ),
    c(
        "vfx.crash_fireball",
        r#"{ "yield_t": 250.0, "times": [1.0, 1.25, 4.0] }"#,
        &[],
        &[
            items("times", 32),
            range("times", 0.0, 30.0),
            range("yield_t", 0.05, 1000.0),
        ],
    ),
    c(
        "vfx.blast_gust",
        r#"{ "position": [-1752.0, 353.2, -2203.0] }"#,
        &[],
        &[range("position", -1000000.0, 1000000.0)],
    ),
    // Terrain ground look (ADR-1408): F0003 when the host inserted no control.
    c("terrain.get_ground_look", r#"{}"#, &["F0003"], &[]),
    c(
        "terrain.set_ground_look",
        r#"{ "preset": "late_winter" }"#,
        &["F0003"],
        &[],
    ),
    c("render.get_aerosol", r#"{}"#, &[], &[]),
    c("render.set_aerosol", r#"{ "angstrom": 1.3 }"#, &[], &[]),
    // Daylight grey-card calibration (ADR-1425): pure compute, F0002 on bad values.
    c("render.grey_card", r#"{ "elevation_deg": 21.0 }"#, &[], &[]),
    // Terrain debug views (ADR-1447): F0003 when the host inserted no control.
    c("terrain.get_debug_view", r#"{}"#, &["F0003"], &[]),
    c(
        "terrain.set_debug_view",
        r#"{ "view": "sun_split" }"#,
        &["F0003"],
        &[],
    ),
    // Post volumes and the physical camera (ADR-1464): F0003 without a camera or entity.
    c("render.post_volumes", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_post_volume",
        r#"{ "entity": 4294967296, "shape": { "kind": "box", "half_extents": [4.0, 3.0, 6.0] }, "overrides": { "exposure_compensation": 1.0 } }"#,
        &["F0003"],
        &[],
    ),
    c("render.get_physical_camera", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_physical_camera",
        r#"{ "exposure_mode": "physical", "aperture": 8.0, "shutter_seconds": 0.004, "iso": 200.0 }"#,
        &["F0003"],
        &[],
    ),
    // Post-volume format v3 (ADR-2323): the LUT switch never needs an entity; gizmos do for one.
    c("render.set_post_volume_luts", r#"{ "enabled": true }"#, &[], &[]),
    c("render.post_volume_gizmos", r#"{}"#, &["F0003"], &[]),
    // Runtime lens effects (ADR-1863): F0003 without a camera.
    c("render.lens_effects", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_lens_effects",
        r#"{ "flare_intensity": 2.0, "dirt_intensity": 0.5, "chromatic_aberration": 0.003 }"#,
        &["F0003"],
        &[],
    ),
    // Montages, sync groups and motion warping (ADR-1465, book/agents/montages.md).
    c(
        "anim.montage_state",
        r#"{ "entity": 4294967310 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "anim.montage_play",
        r#"{ "entity": 4294967310, "montage": "attack", "rate": 1.0 }"#,
        &["F0003"],
        &[range("rate", 0.0, 100.0), range("start_time", 0.0, 3600.0)],
    ),
    c(
        "anim.montage_stop",
        r#"{ "entity": 4294967310, "blend": 0.2 }"#,
        &["F0003"],
        &[range("blend", 0.0, 60.0)],
    ),
    c(
        "anim.montage_jump",
        r#"{ "entity": 4294967310, "section": "finish" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "anim.montage_set_warp_target",
        r#"{ "entity": 4294967310, "name": "enemy", "position": [1.5, 0.0, -2.5] }"#,
        &["F0003"],
        &[items("position", 3)],
    ),
    c("anim.sync_state", r#"{ "entity": 4294967310 }"#, &["F0003"], &[]),
    // Scripted warps and the root-motion route (spec 34 M3 and M6, ADR-1644).
    c("anim.warp_state", r#"{ "entity": 4294967310 }"#, &["F0003"], &[]),
    // Montage authoring, reverse and time-stretch (spec 34 M8-M10, ADR-1870).
    c(
        "anim.montage_describe",
        r#"{ "asset": "anim/combo.montage.ron" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "anim.montage_edit",
        r#"{ "asset": "anim/combo.montage.ron", "edit": { "op": "move_segment", "track": 0, "segment": 0, "end": 0.8 } }"#,
        &["F0003"],
        &[],
    ),
    c(
        "anim.montage_slots",
        r#"{ "path": "chars/hero.slots.ron" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "anim.montage_slot_edit",
        r#"{ "path": "chars/hero.slots.ron", "op": { "op": "rename", "from": "upper", "to": "upper_body" } }"#,
        &["F0003"],
        &[],
    ),
    c(
        "anim.montage_control",
        r#"{ "entity": 4294967310, "reverse": true, "duration": 0.6 }"#,
        &["F0003"],
        &[],
    ),
    // The montage command hook (spec 34 § 9, ADR-7216).
    c(
        "anim.montage_commands",
        r#"{ "entity": 4294967310, "record": true }"#,
        &["F0003"],
        &[],
    ),
    c(
        "anim.scripted_warp",
        r#"{ "entity": 4294967310, "op": "add", "id": "dash", "target": "enemy", "duration": 0.5 }"#,
        &["F0003"],
        &[
            range("duration", 0.0, 3600.0),
            range("rotation_fraction", 0.0, 1.0),
        ],
    ),
    c(
        "anim.root_motion_route",
        r#"{ "entity": 4294967310, "route": "only_montages" }"#,
        &["F0003"],
        &[],
    ),
    // Data tables (ADR-1466).
    c("tables.list", r#"{}"#, &[], &[]),
    c(
        "tables.describe",
        r#"{ "path": "tables/weapons.table.ron" }"#,
        &["bad_path", "table_unknown"],
        &[],
    ),
    c(
        "tables.get",
        r#"{ "path": "tables/weapons.table.ron", "key": "rifle" }"#,
        &[
            "bad_key",
            "bad_path",
            "table_encode",
            "table_unknown",
            "table_unknown_key",
        ],
        &[Limit::Items {
            param: "keys",
            max: 10_000,
        }],
    ),
    c(
        "tables.set",
        r#"{ "path": "tables/weapons.table.ron", "key": "rifle", "column": "damage", "value": "45" }"#,
        &[
            "bad_column",
            "bad_edit",
            "bad_key",
            "bad_path",
            "bad_value",
            "table_bad_cell",
            "table_duplicate_key",
            "table_empty_key",
            "table_encode",
            "table_rule",
            "table_unknown",
            "table_unknown_column",
            "table_unknown_key",
        ],
        &[],
    ),
    c("tables.validate", r#"{}"#, &["bad_path", "table_unknown"], &[]),
    c(
        "tables.export",
        r#"{ "path": "tables/weapons.table.ron", "format": "csv" }"#,
        &[
            "bad_format",
            "bad_path",
            "table_encode",
            "table_format_mismatch",
            "table_no_file",
            "table_unknown",
            "table_write_failed",
        ],
        &[],
    ),
    // Extensions (book/agents/plugins.md, specs/57 § 5).
    c("plugin.list", r#"{ "state": "active" }"#, &[], &[]),
    c("plugin.info", r#"{ "id": "dialogue" }"#, &["not_found"], &[]),
    c(
        "plugin.check",
        r#"{ "enabled": ["dialogue"], "platform": "android" }"#,
        &[],
        &[],
    ),
    c(
        "plugin.enable",
        r#"{ "id": "dialogue", "grant": ["agent_method"] }"#,
        &[
            "capability_not_granted",
            "io_failed",
            "manifest_invalid",
            "not_found",
            "not_supported",
            "unknown_capability",
        ],
        &[],
    ),
    c(
        "plugin.disable",
        r#"{ "id": "dialogue" }"#,
        &["io_failed", "manifest_invalid", "not_found", "not_supported"],
        &[],
    ),
    c(
        "plugin.reload",
        r#"{ "id": "dialogue" }"#,
        &[
            "abi_mismatch",
            "load_failed",
            "not_an_extension",
            "not_found",
            "not_supported",
            "reload_failed",
            "restart_required",
        ],
        &[],
    ),
    c(
        "plugin.tool_run",
        r#"{ "id": "dialogue", "name": "dialogue-lint", "args": ["--all"] }"#,
        &[
            "capability_not_granted",
            "load_failed",
            "not_found",
            "tool_output_too_large",
            "tool_timeout",
        ],
        &[],
    ),
    c("localization.status", r#"{}"#, &["loc_not_loaded"], &[]),
    c(
        "localization.set_locale",
        r#"{ "locale": "fr-CA" }"#,
        &[
            "bad_flag",
            "bad_locale",
            "loc_not_loaded",
            "loc_unknown_locale",
            "no_settings",
        ],
        &[Limit::Bytes {
            param: "locale",
            max: 64,
        }],
    ),
    c(
        "localization.resolve",
        r#"{ "key": "menu-saves", "args": { "count": 3 } }"#,
        &["bad_args", "bad_key", "loc_not_loaded", "loc_unknown_locale"],
        &[
            Limit::Bytes {
                param: "key",
                max: 256,
            },
            Limit::Bytes {
                param: "locale",
                max: 64,
            },
        ],
    ),
    c(
        "localization.set_pseudo",
        r#"{ "on": true }"#,
        &["bad_flag", "loc_not_loaded"],
        &[],
    ),
    c(
        "localization.validate",
        r#"{ "strict": true }"#,
        &["bad_flag", "loc_not_loaded"],
        &[],
    ),
    c(
        "localization.plural",
        r#"{ "locale": "ru", "numbers": [1, 3, 5, "1.5"] }"#,
        &["bad_locale", "bad_number", "loc_unknown_plural_locale"],
        &[Limit::Items {
            param: "numbers",
            max: 1024,
        }],
    ),
    c(
        "localization.set_key_display",
        r#"{ "on": true }"#,
        &["bad_flag", "loc_not_loaded"],
        &[],
    ),
    c(
        "localization.coverage",
        r#"{ "locale": "fr" }"#,
        &["bad_locale", "loc_not_loaded", "loc_unknown_locale"],
        &[Limit::Bytes {
            param: "locale",
            max: 64,
        }],
    ),
    c(
        "localization.glyph_coverage",
        r#"{ "locale": "fr", "charset": true }"#,
        &["bad_flag", "bad_locale", "loc_not_loaded", "loc_unknown_locale"],
        &[Limit::Bytes {
            param: "locale",
            max: 64,
        }],
    ),
    c(
        "loc.status",
        r#"{}"#,
        &["loc_not_loaded", "loc_unknown_locale"],
        &[Limit::Bytes {
            param: "locale",
            max: 64,
        }],
    ),
    c(
        "loc.check_fit",
        r#"{ "size": "1920x1080" }"#,
        &[
            "fit_bad_document",
            "fit_bad_size",
            "fit_failed",
            "fit_no_documents",
            "fit_unknown_document",
            "loc_not_loaded",
        ],
        &[Limit::Bytes {
            param: "size",
            max: 32,
        }],
    ),
    c(
        "localization.set_audio_locale",
        r#"{ "locale": "ja" }"#,
        &["bad_flag", "bad_locale", "loc_not_loaded", "no_settings"],
        &[Limit::Bytes {
            param: "locale",
            max: 64,
        }],
    ),
    // Ragdoll modes and hits (book/agents/animation.md, ADR-1563 j): F0003 for a missing rig
    // or a rig without a Ragdoll.
    c(
        "anim.ragdoll_set",
        r#"{ "entity": 4294967310, "mode": "limp" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "anim.ragdoll_hit",
        r#"{ "entity": 4294967310, "joint": "spine", "impulse": [0.0, 0.0, -8.0] }"#,
        &["F0003"],
        &[items("impulse", 3), bytes("joint", 256)],
    ),
    c(
        "ui.screens",
        r#"{ "action": "push", "entity": 4294967310 }"#,
        &[
            "already_on_stack",
            "empty_stack",
            "not_a_screen",
            "not_on_top",
            "unknown_entity",
        ],
        &[],
    ),
    // World streaming and HLOD data model (book/agents/world-streaming.md, ADR-1566).
    c(
        "scene.stream_plan",
        r#"{ "cell_size": 100.0, "load_radius": 300.0, "viewers": [[50.0, 50.0]], "cells": [{ "x": 0, "z": 0, "bytes": 1000 }] }"#,
        &[],
        &[
            range("cell_size", 0.001, 10_000_000.0),
            items("viewers", 64),
            items("cells", 100_000),
        ],
    ),
    c(
        "scene.hlod_select",
        r#"{ "cell_size": 100.0, "viewers": [[50.0, 10.0, 50.0]], "cells": [{ "x": 0, "z": 0 }, { "x": 1, "z": 0 }] }"#,
        &[],
        &[
            range("cell_size", 0.001, 10_000_000.0),
            items("viewers", 64),
            items("cells", 100_000),
        ],
    ),
    c(
        "scene.large_position",
        r#"{ "cell_size": 1024.0, "position": [5000000000.0, 12.5, -3000.0] }"#,
        &[],
        &[range("cell_size", 0.001, 10_000_000.0)],
    ),
    // -- Demo recording and the spectator camera (ADR-1569) -----------------------
    heavy(
        "demo.inspect",
        r#"{ "path": "saved/demos/arena-001.regiademo" }"#,
        &[
            "demo_corrupt",
            "demo_invalid",
            "demo_io",
            "demo_parse",
            "demo_too_large",
            "demo_truncated",
            "demo_unsupported_version",
        ],
        &[],
    ),
    heavy(
        "demo.seek_plan",
        r#"{ "path": "saved/demos/arena-001.regiademo", "tick": 600 }"#,
        &[
            "demo_corrupt",
            "demo_invalid",
            "demo_io",
            "demo_no_keyframe",
            "demo_parse",
            "demo_tick_out_of_range",
            "demo_too_large",
            "demo_truncated",
            "demo_unsupported_version",
        ],
        &[range("tick", 0.0, 4294967295.0)],
    ),
    c("spectator.get", r#"{}"#, &[], &[]),
    c(
        "spectator.set",
        r#"{ "position": [0.0, 5.0, 10.0], "yaw": 0.5, "mode": "follow", "target": [0.0, 1.0, 0.0] }"#,
        &[],
        &[range("speed_level", -2147483648.0, 2147483647.0)],
    ),
    c(
        "spectator.step",
        r#"{ "frames": 60, "dt": 0.016, "movement": [0.0, 0.0, 1.0] }"#,
        &[],
        &[range("frames", 0.0, 100000.0)],
    ),
    // The live demo driver and the scrub bar (ADR-1878).
    c("demo.status", r#"{}"#, &["demo_not_installed"], &[]),
    c(
        "demo.record_start",
        r#"{ "level": "arena", "seed": 42, "tick_rate_hz": 60, "keyframe_interval": 60 }"#,
        &["demo_busy", "demo_not_installed"],
        &[
            range("tick_rate_hz", 1.0, 10000.0),
            range("keyframe_interval", 0.0, 4294967295.0),
        ],
    ),
    c(
        "demo.record_stop",
        r#"{ "path": "saved/demos/run-001.frec" }"#,
        &["demo_bad_path", "demo_no_demo", "demo_not_installed"],
        &[],
    ),
    heavy(
        "demo.play",
        r#"{ "path": "saved/demos/run-001.frec" }"#,
        &[
            "demo_busy",
            "demo_corrupt",
            "demo_invalid",
            "demo_io",
            "demo_no_demo",
            "demo_not_installed",
            "demo_parse",
            "demo_too_large",
            "demo_truncated",
            "demo_unsupported_version",
        ],
        &[],
    ),
    c(
        "demo.pause",
        r#"{ "paused": true }"#,
        &["demo_busy", "demo_no_demo", "demo_not_installed"],
        &[],
    ),
    c(
        "demo.seek",
        r#"{ "tick": 600 }"#,
        &[
            "demo_busy",
            "demo_no_demo",
            "demo_no_keyframe",
            "demo_not_installed",
            "demo_tick_out_of_range",
        ],
        &[range("tick", 0.0, 4294967295.0)],
    ),
    c(
        "demo.scrub_bar",
        r#"{ "open": true }"#,
        &["demo_not_installed"],
        &[],
    ),
    c(
        "spectator.bind",
        r#"{ "camera": { "name": "MainCamera" }, "follow": { "name": "Hero" } }"#,
        &["spectator_bad_entity"],
        &[],
    ),
    c(
        "ui.localization",
        r#"{ "action": "pseudo", "text": "Start game" }"#,
        &["no_localization"],
        &[],
    ),
    c(
        "ui.subtitles",
        r#"{ "action": "push", "speaker": "Ana", "text": "Hello", "duration": 2.0 }"#,
        &[
            "subtitle_bad_duration",
            "subtitle_bad_start",
            "subtitle_bad_time",
            "subtitle_empty_text",
            "subtitle_queue_full",
            "unknown_subtitle",
        ],
        &[],
    ),
    c(
        "conversation.play",
        r#"{ "conversation": "intro", "locale": "en" }"#,
        &[
            "channel_busy",
            "conversation_not_found",
            "label_not_found",
            "locale_not_loaded",
            "plugin_missing",
        ],
        &[],
    ),
    c(
        "conversation.say",
        r#"{ "text": "Halt!", "speaker": "guard", "duration_ms": 1500 }"#,
        &[
            "line_not_found",
            "locale_not_loaded",
            "plugin_missing",
            "speaker_not_found",
            "text_too_long",
        ],
        &[],
    ),
    c(
        "conversation.choose",
        r#"{ "run": 1, "index": 0 }"#,
        &[
            "choice_out_of_range",
            "no_choice_pending",
            "plugin_missing",
            "run_not_found",
        ],
        &[],
    ),
    c(
        "conversation.stop",
        r#"{ "run": 1, "fade_ms": 200 }"#,
        &["plugin_missing", "run_not_found"],
        &[],
    ),
    c(
        "conversation.validate",
        r#"{ "conversation": "intro", "strict": true }"#,
        &["conversation_not_found", "locale_not_loaded", "plugin_missing"],
        &[],
    ),
    c(
        "captions.state",
        r#"{ "include_layout": true }"#,
        &["plugin_missing"],
        &[],
    ),
    c(
        "captions.set_options",
        r#"{ "text_scale": 1.5, "caption_mode": true }"#,
        &[
            "bad_flag",
            "invalid_color",
            "invalid_option",
            "no_settings",
            "out_of_range",
            "plugin_missing",
        ],
        &[],
    ),
    c(
        "ui.choice_state",
        r#"{ "widget": 1 }"#,
        &["plugin_missing", "widget_not_found"],
        &[],
    ),
    c(
        "ui.choice_pick",
        r#"{ "index": 0 }"#,
        &[
            "option_disabled",
            "option_out_of_range",
            "plugin_missing",
            "widget_not_found",
        ],
        &[],
    ),
    c("captions.anchor_state", r#"{}"#, &["plugin_missing"], &[]),
    c(
        "captions.set_dialogue_style",
        r#"{ "karaoke": true, "placement": "anchored" }"#,
        &["invalid_placement", "plugin_missing"],
        &[],
    ),
    c(
        "dialogue.list",
        r#"{ "dialogue": "ranger_intro" }"#,
        &["dialogue_unavailable", "dialogue_unknown_asset"],
        &[],
    ),
    c(
        "dialogue.resolve",
        r#"{ "dialogue": "ranger_intro", "line": "greeting", "listeners": ["player"] }"#,
        &[
            "dialogue_no_variant",
            "dialogue_unavailable",
            "dialogue_unknown_asset",
            "dialogue_unknown_line",
            "dialogue_unknown_voice",
        ],
        &[],
    ),
    c(
        "dialogue.play",
        r#"{ "dialogue": "ranger_intro", "line": "greeting" }"#,
        &[
            "dialogue_no_variant",
            "dialogue_unavailable",
            "dialogue_unknown_asset",
            "dialogue_unknown_line",
            "dialogue_unknown_voice",
        ],
        &[],
    ),
    c(
        "dialogue.stop",
        r#"{ "handle": 1 }"#,
        &["dialogue_bad_handle", "dialogue_unavailable"],
        &[],
    ),
    c("subtitle_sets.status", "{}", &["subtitle_sets_unavailable"], &[]),
    c(
        "subtitle_sets.queue",
        r#"{ "cues": [{ "at": 0.0, "until": 2.2, "text": "Hello" }], "duration": 3.0 }"#,
        &["subtitle_sets_bad_cues", "subtitle_sets_unavailable"],
        &[],
    ),
    c(
        "subtitle_sets.stop",
        r#"{ "id": 1 }"#,
        &["subtitle_sets_unavailable", "subtitle_sets_unknown_id"],
        &[],
    ),
    c(
        "subtitle_sets.set_enabled",
        r#"{ "on": true }"#,
        &["subtitle_sets_unavailable"],
        &[],
    ),
    c(
        "subtitle_sets.sync",
        r#"{ "id": 1, "playhead": 2.5 }"#,
        &[
            "subtitle_sets_bad_playhead",
            "subtitle_sets_unavailable",
            "subtitle_sets_unknown_id",
        ],
        &[],
    ),
    // The frame graph (ADR-1552): F0003 without a RuntimeRenderer.
    c("render.graph", r#"{}"#, &["F0003"], &[]),
    // Sequence players (ADR-1614).
    c(
        "sequence.player",
        r#"{ "action": "load", "seq_ron": "(version: 1, name: \"intro\")", "bindings": { "hero": { "name": "Hero" } }, "end": "once", "blend_in": 0.5, "play": true }"#,
        &[
            "bad_action",
            "bad_binding",
            "bad_clip",
            "bad_entity",
            "bad_name",
            "bad_sequence",
            "bad_setting",
            "bad_shots",
            "bad_time",
            "no_player",
            "several_players",
            "too_large",
            "unknown_entity",
            "unknown_player",
        ],
        &[
            bytes("seq_ron", 16777216),
            bytes("shots_ron", 16777216),
            range("blend_in", 0.0, 3600.0),
            range("blend_out", 0.0, 3600.0),
            range("start_delay", 0.0, 86400.0),
        ],
    ),
    // Per-object shadow LOD hints (ADR-1622): F0003 for a missing entity.
    c("render.shadow_lod_hints", r#"{}"#, &[], &[]),
    c(
        "render.set_shadow_lod_hint",
        r#"{ "entity": 4294967301, "max_lod": 1 }"#,
        &["F0003"],
        &[range("max_lod", 0.0, 254.0), range("min_lod", 0.0, 254.0)],
    ),
    heavy("assets.cooked_pack", r#"{ "inspect": true }"#, &["F0016"], &[]),
    // Named terrain holes and runtime height patches (ADR-1595).
    c(
        "terrain.set_hole",
        r#"{ "id": "tunnel_3", "segment": { "from": [-142.8, -286.0], "to": [-170.0, -256.0], "width_m": 20.0 } }"#,
        &[
            "F0003",
            "hole_bad_id",
            "hole_filled_by_layer",
            "hole_too_many",
            "hole_unknown_id",
        ],
        &[bytes("id", 64)],
    ),
    c(
        "terrain.patch_heights",
        r#"{ "col": 512, "row": 640, "width": 32, "height": 16, "fill": 41.5 }"#,
        &[
            "F0003",
            "patch_bad_length",
            "patch_empty",
            "patch_no_target",
            "patch_non_finite",
            "patch_out_of_bounds",
            "patch_queue_full",
            "patch_too_large",
        ],
        &[
            items("heights", 1_048_576),
            items("samples", 1_048_576),
            range("budget", 0.0, 4_194_304.0),
        ],
    ),
    heavy(
        "terrain.apply_operator",
        r#"{ "op": "flatten_pad", "params": { "center": [0, 0], "shape": { "circle": { "radius_m": 20 } }, "feather_m": 10 }, "dry_run": true }"#,
        &[
            "F0003",
            "op_bad_grid",
            "op_bad_param",
            "op_non_finite",
            "op_pad_empty",
            "op_unknown",
            "patch_no_target",
            "patch_queue_full",
        ],
        &[items("rect", 4)],
    ),
    c(
        "terrain.get",
        r#"{ "at": [[0, 0], [12.5, -40]] }"#,
        &["F0003"],
        &[items("at", 64)],
    ),
    // The far backdrop ring (ADR-8459).
    c("terrain.backdrop_status", "{}", &[], &[]),
    // Heightmap export/import, saving holes and the shared wind (ADR-1853).
    heavy(
        "terrain.save_holes",
        r#"{ "path": "maps/arena.level.ron" }"#,
        &["F0003", "holes_write"],
        &[bytes("path", 1024)],
    ),
    heavy(
        "terrain.save_paint",
        r#"{ "path": "maps/arena.level.ron" }"#,
        &["F0003", "height_io_write"],
        &[bytes("path", 1024)],
    ),
    heavy(
        "terrain.load_paint",
        r#"{ "path": "maps/arena.weights.bin" }"#,
        &[
            "F0003",
            "height_io_decode",
            "height_io_read",
            "height_io_size_mismatch",
        ],
        &[bytes("path", 1024)],
    ),
    heavy(
        "terrain.export_heights",
        r#"{ "path": "terrain/sculpted.png" }"#,
        &["F0003", "height_io_bad_format", "height_io_write"],
        &[bytes("path", 1024)],
    ),
    heavy(
        "terrain.import_heights",
        r#"{ "path": "terrain/source.r16", "endian": "little" }"#,
        &[
            "F0003",
            "height_io_bad_format",
            "height_io_decode",
            "height_io_read",
            "height_io_size_mismatch",
        ],
        &[bytes("path", 1024)],
    ),
    c("terrain.wind", r#"{}"#, &[], &[]),
    c(
        "terrain.set_wind",
        r#"{ "direction": [1.0, 0.3], "strength": 1.5, "gust": 0.5 }"#,
        &["F0003", "wind_bad_direction", "wind_bad_style"],
        &[
            range("strength", -1.0e6, 1.0e6),
            range("frequency_scale", -1.0e6, 1.0e6),
            range("gust", -1.0e6, 1.0e6),
        ],
    ),
    heavy(
        "terrain.scatter_stroke",
        r#"{ "class": "pines", "center": [12.0, -30.0], "radius": 20.0, "density": 0.02, "scale_min": 0.8, "scale_max": 1.4, "slope_max": 0.6 }"#,
        &[
            "F0003",
            "scatter_paint_bad_brush",
            "scatter_paint_no_set",
            "scatter_paint_too_large",
            "scatter_paint_unknown_class",
        ],
        &[
            bytes("class", 256),
            range("radius", -1.0e6, 1.0e6),
            range("density", -1.0e6, 1.0e6),
            range("strength", -1.0e6, 1.0e6),
            range("hardness", -1.0e6, 1.0e6),
        ],
    ),
    c("terrain.scatter_paint", r#"{}"#, &["F0003"], &[]),
    heavy(
        "terrain.save_scatter_paint",
        r#"{ "path": "maps/arena.level.ron" }"#,
        &["F0003", "scatter_paint_write"],
        &[bytes("path", 1024)],
    ),
    heavy(
        "terrain.load_scatter_paint",
        r#"{ "path": "maps/arena.scatterpaint.bin" }"#,
        &["F0003", "scatter_paint_decode", "scatter_paint_read"],
        &[bytes("path", 1024)],
    ),
    // Destruction level 2 (ADR-1597).
    c(
        "physics.destruction_define",
        r#"{ "sample": "wall_bridge" }"#,
        &[
            "F0003",
            "destruction_invalid_asset",
            "destruction_invalid_pose",
            "destruction_parse",
            "destruction_version",
        ],
        &[range("position", -1.0e6, 1.0e6), bytes("asset", 4 << 20)],
    ),
    c(
        "physics.destruction_damage",
        r#"{ "kind": "explosion", "center": [0, 1, -4.6], "radius": 1.2, "impulse": 6000 }"#,
        &[
            "F0003",
            "destruction_invalid_damage",
            "destruction_replica",
            "destruction_unknown_chunk",
            "destruction_unknown_id",
        ],
        &[],
    ),
    c(
        "physics.destruction_state",
        r#"{ "events": 16 }"#,
        &["F0003", "destruction_unknown_id"],
        &[range("events", 0.0, 256.0)],
    ),
    c(
        "physics.destruction_budget",
        r#"{ "max_active_chunks": 128 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "physics.destruction_net",
        r#"{ "max_ops_per_tick": 64 }"#,
        &["F0003", "destruction_not_client"],
        &[
            range("max_ops_per_tick", 1.0, 1023.0),
            range("snapshot_parts_per_tick", 1.0, 16.0),
            range("snapshot_part_bytes", 1024.0, 49152.0),
        ],
    ),
    c(
        "physics.destruction_draw",
        r#"{ "debris_lod": 1 }"#,
        &["F0003"],
        &[range("debris_lod", 0.0, 255.0)],
    ),
    c("physics.destruction_playback", r#"{}"#, &["F0003"], &[]),
    c(
        "physics.destruction_load_report",
        r#"{"top":4}"#,
        &["F0003", "destruction_unknown_id"],
        &[range("top", 0.0, 256.0)],
    ),
    c(
        "physics.destruction_fx_table",
        r#"{}"#,
        &[
            "F0003",
            "destruction_fx_invalid",
            "destruction_fx_parse",
            "destruction_fx_version",
        ],
        &[],
    ),
    c("physics.destruction_fx_stats", r#"{}"#, &["F0003"], &[]),
    c("physics.destruction_impact_damage", r#"{}"#, &["F0003"], &[]),
    c(
        "physics.destruction_impact_damage_set",
        r#"{"min_speed":2.5}"#,
        &["F0003", "destruction_impact_invalid", "destruction_unknown_id"],
        &[
            range("max_hits_per_tick", 0.0, 4096.0),
            range("cooldown_ticks", 0.0, 36000.0),
        ],
    ),
    c("net.status", r#"{}"#, &[], &[]),
    c("net.resume", r#"{}"#, &[], &[]),
    c(
        "net.rejections",
        r#"{ "endpoint": "server", "peer": 2 }"#,
        &["net_rejections_bad_peer", "net_unknown_endpoint"],
        &[],
    ),
    c(
        "net.peers",
        r#"{ "endpoint": "server" }"#,
        &["net_unknown_endpoint"],
        &[],
    ),
    c(
        "net.crypto",
        r#"{ "endpoint": "server" }"#,
        &["net_unknown_endpoint"],
        &[],
    ),
    c(
        "net.lockstep",
        r#"{ "endpoint": "host" }"#,
        &["lockstep_unknown_endpoint"],
        &[],
    ),
    // Warm successors (book/agents/net-migration.md, ADR-8853).
    c(
        "net.migration",
        r#"{ "endpoint": "server" }"#,
        &["migration_unknown_endpoint"],
        &[],
    ),
    // Lockstep desync diagnosis (book/agents/net-desync.md, ADR-8455).
    c(
        "net.desync",
        r#"{ "endpoint": "host" }"#,
        &["desync_unknown_endpoint"],
        &[],
    ),
    c(
        "net.desync_inject",
        r#"{ "endpoint": "seat1", "subsystem": "economy", "tick": 120 }"#,
        &[
            "desync_inject_disabled",
            "desync_inject_invalid",
            "desync_no_endpoint",
            "desync_unknown_endpoint",
        ],
        &[],
    ),
    // Lockstep match replays (book/agents/net-lsreplay.md, ADR-8513).
    c(
        "net.replays",
        r#"{ "dir": "saved/replays" }"#,
        &["replay_io"],
        &[],
    ),
    c(
        "net.replay_audit",
        r#"{ "path": "saved/replays/match.lsreplay" }"#,
        &[
            "F0003",
            "replay_game_mismatch",
            "replay_hook_failed",
            "replay_invalid",
            "replay_io",
            "replay_no_hook",
            "replay_not_found",
            "replay_not_lsreplay",
            "replay_too_large",
            "replay_truncated",
            "replay_unknown_record",
            "replay_version_newer",
        ],
        &[],
    ),
    // Published external replication stats (book/agents/net-external-replication.md, ADR-8892).
    c(
        "net.external_stats",
        r#"{ "replication": "world" }"#,
        &["external_bad_params", "external_unknown"],
        &[],
    ),
    c(
        "net.quality",
        r#"{ "endpoint": "server", "peer": 1 }"#,
        &["net_unknown_endpoint", "peer_unknown"],
        &[],
    ),
    c(
        "net.conditioner_set",
        r#"{ "endpoint": "server", "profile": "wan", "config": { "loss": 0.05 } }"#,
        &[
            "net_bad_conditioner",
            "net_no_endpoint",
            "net_unknown_endpoint",
            "net_unknown_profile",
        ],
        &[],
    ),
    c("net.rollback", r#"{}"#, &[], &[]),
    // Net model and replication (book/agents/net-model.md, net-replication.md, ADR-1643):
    // F0002 with data.reason (net_not_running, net_not_server, entity_not_replicated,
    // entity_not_found, peer_unknown, message_type_unknown, component_not_replicated).
    c("net.entity", r#"{ "entity": 4294967310 }"#, &[], &[]),
    c("net.messages", r#"{ "type": "Fire" }"#, &[], &[]),
    c("net.peer_flood", r#"{ "peer": 1 }"#, &[], &[]),
    c(
        "net.set_owner",
        r#"{ "entity": 4294967310, "peer": 1 }"#,
        &[],
        &[],
    ),
    c(
        "net.replicated",
        r#"{ "filter": { "component": "Health", "asleep": false }, "limit": 100 }"#,
        &[],
        &[],
    ),
    c("net.entity_state", r#"{ "entity": 4294967310 }"#, &[], &[]),
    c("net.wake", r#"{ "entity": 4294967310 }"#, &[], &[]),
    c("net.set_asleep", r#"{ "entity": 4294967310 }"#, &[], &[]),
    c("net.resend", r#"{ "entity": 4294967310 }"#, &[], &[]),
    c("net.pending_refs", r#"{}"#, &[], &[]),
    c("net.assets", r#"{ "limit": 10 }"#, &[], &[]),
    c("render.foliage_wind", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_foliage_wind",
        r#"{ "wind_m_per_s": [4.0, 1.5] }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.set_dynamic_caster_shadows",
        r#"{ "mode": "on", "cascades": 2, "local_lights": true }"#,
        &["F0003"],
        &[],
    ),
    c("material.sway", r#"{ "entity": 4294967301 }"#, &["F0003"], &[]),
    c(
        "material.set_sway",
        r#"{ "entity": 4294967301, "sway": { "height_m": 6.0, "stiffness": 0.02 } }"#,
        &["F0003"],
        &[],
    ),
    c("dcc_feed.status", r#"{}"#, &[], &[]),
    c(
        "dcc_feed.items",
        r#"{ "kind": "mesh", "limit": 100 }"#,
        &["F0013"],
        &[bytes("key_glob", 256)],
    ),
    c(
        "dcc_feed.accept",
        r#"{ "session": 1, "remember": true }"#,
        &["F0013"],
        &[],
    ),
    c(
        "dcc_feed.disconnect",
        r#"{ "session": 1, "despawn": false }"#,
        &["F0013"],
        &[],
    ),
    c(
        "dcc_feed.record",
        r#"{ "action": "start", "path": "takes/one.rfdrec" }"#,
        &["F0013"],
        &[bytes("path", 4096)],
    ),
    c(
        "dcc_feed.replay",
        r#"{ "path": "takes/one.rfdrec", "loop": false }"#,
        &["F0013"],
        &[bytes("path", 4096)],
    ),
    c("dcc_feed.listen", r#"{ "enabled": true }"#, &["F0013"], &[]),
    c("dcc_feed.rotate_token", r#"{}"#, &["F0013"], &[]),
    heavy(
        "dcc_feed.bake",
        r#"{ "items": "all", "folder": "live" }"#,
        &["F0013"],
        &[bytes("folder", 4096)],
    ),
    c("dcc_feed.bake_status", r#"{ "job": 1 }"#, &["F0013"], &[]),
    c("procgen.list", r#"{}"#, &["F0003"], &[]),
    c("procgen.get", r#"{ "entity": 4294967301 }"#, &["F0003"], &[]),
    heavy(
        "procgen.run",
        r#"{ "entity": 4294967301, "wait": true }"#,
        &["F0003"],
        &[range("timeout_ms", 0.0, 600_000.0)],
    ),
    c("procgen.cancel", r#"{ "entity": 4294967301 }"#, &["F0003"], &[]),
    c("procgen.clear", r#"{ "entity": 4294967301 }"#, &["F0003"], &[]),
    c("procgen.report", r#"{ "entity": 4294967301 }"#, &["F0003"], &[]),
    heavy(
        "procgen.inspect",
        r#"{ "entity": 4294967301, "node": 2, "limit": 50 }"#,
        &["F0003"],
        &[range("limit", 0.0, 10_000.0), items("attributes", 64)],
    ),
    c(
        "procgen.graph_get",
        r#"{ "path": "procgen/rocks.pgraph.ron" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "procgen.graph_edit",
        r#"{ "path": "procgen/rocks.pgraph.ron", "ops": [{ "op": "set_enabled", "id": 2, "enabled": false }] }"#,
        &["F0003"],
        &[items("ops", 512)],
    ),
    c(
        "procgen.node_types",
        r#"{ "category": "sample" }"#,
        &["F0003"],
        &[],
    ),
    heavy("procgen.bake", r#"{}"#, &["F0003"], &[]),
    heavy("procgen.verify", r#"{}"#, &["F0003"], &[]),
    c("procgen.set_debug", r#"{ "tiles": true }"#, &["F0003"], &[]),
    c("procgen.detach", r#"{ "entity": 4294967301 }"#, &["F0003"], &[]),
    // World streaming runtime (book/agents/streaming-runtime.md, spec 78): F0002 with data.reason
    // (stream_not_enabled, stream_unknown_cell, stream_unknown_source, stream_unknown_pin,
    // stream_source_limit, stream_bad_budget, stream_dev_only, stream_lockstep_stall,
    // stream_format_newer).
    c(
        "stream.open",
        r#"{ "level": "6b1c2f60-0000-4000-8000-000000000001", "mode": "replace" }"#,
        &[],
        &[],
    ),
    c("stream.status", r#"{}"#, &[], &[]),
    c(
        "stream.cells",
        r#"{ "state": "loading", "near": [0.0, 0.0, 0.0], "radius_m": 512.0, "limit": 100 }"#,
        &[],
        &[],
    ),
    c("stream.sources", r#"{}"#, &[], &[]),
    c(
        "stream.pin",
        r#"{ "name": "next stop", "center": [2048.0, 0.0, 512.0], "wait": { "radius_m": 128.0, "readiness": "sim_ready" } }"#,
        &[],
        &[],
    ),
    c("stream.unpin", r#"{ "pin": 1 }"#, &[], &[]),
    c(
        "stream.teleport",
        r#"{ "target": [2048.0, 0.0, 512.0], "wait": "sim_ready" }"#,
        &[],
        &[],
    ),
    c(
        "stream.ready",
        r#"{ "center": [2048.0, 0.0, 512.0], "radius_m": 128.0, "readiness": "sim_ready" }"#,
        &[],
        &[],
    ),
    c(
        "stream.budget_set",
        r#"{ "budgets": { "max_inflight": 4 } }"#,
        &[],
        &[],
    ),
    c(
        "stream.timing_set",
        r#"{ "mode": "lockstep", "latency_steps": 2 }"#,
        &[],
        &[],
    ),
    c("stream.retry", r#"{ "cell": "3_-2" }"#, &[], &[]),
    c("stream.hlod", r#"{}"#, &[], &[]),
    c("stream.origin", r#"{}"#, &[], &[]),
    c("stream.rebase", r#"{ "cell": [2, 0, 0] }"#, &[], &[]),
    c("stream.trace", r#"{ "since_step": 100, "limit": 500 }"#, &[], &[]),
    c("stream.overlay", r#"{ "on": true }"#, &[], &[]),
    c(
        "stream.bounds_set",
        r#"{ "vram_bound": true, "render_residency": true }"#,
        &[],
        &[],
    ),
    c("stream.collision_tiles", r#"{}"#, &[], &[]),
    // Net relevance, priority and budgets (book/agents/net-relevance.md, ADR-1662): F0002 with
    // data.reason (net_not_running, net_not_server, peer_unknown, entity_not_replicated,
    // entity_not_found, profile_invalid, rate_out_of_range, interest_off).
    c("net.relevance", r#"{ "peer": 1, "limit": 100 }"#, &[], &[]),
    c(
        "net.why_not_relevant",
        r#"{ "peer": 1, "entity": 4294967310 }"#,
        &[],
        &[],
    ),
    c("net.bandwidth", r#"{ "peer": 1, "by": "component" }"#, &[], &[]),
    c("net.state_channel", r#"{ "peer": 1 }"#, &[], &[]),
    c("net.entropy_stats", r#"{ "peer": 1 }"#, &[], &[]),
    c(
        "net.set_client_rate",
        r#"{ "peer": 1, "bytes_per_second": 65536 }"#,
        &["net_dev_only"],
        &[],
    ),
    c("net.profiles", r#"{}"#, &[], &[]),
    c(
        "net.grant_relevance",
        r#"{ "entity": 4294967310, "peer": 1, "ticks": 300 }"#,
        &["net_dev_only"],
        &[],
    ),
    // Net proxy interpolation (book/agents/net-interpolation.md, ADR-1662): F0002 with
    // data.reason (net_not_client, entity_not_proxy, delay_out_of_range, net_not_running).
    c("net.interpolation", r#"{}"#, &[], &[]),
    c("net.proxy_timeline", r#"{ "entity": 4294967310 }"#, &[], &[]),
    c("net.set_interp_delay", r#"{ "ms": 100 }"#, &[], &[]),
    // Remote animation (ADR-2551): net_not_running, entity_not_found, entity_not_anim_replicated.
    c(
        "net.anim_state",
        r#"{ "entity": 4294967310 }"#,
        &["entity_not_anim_replicated"],
        &[],
    ),
    // Content chunks (spec 40, ADR-1667).
    c("content.chunks", r#"{}"#, &[], &[]),
    c(
        "content.chunk",
        r#"{ "name": "arena" }"#,
        &["chunk_not_found", "manifest_missing"],
        &[bytes("name", 64)],
    ),
    c(
        "content.why",
        r#"{ "asset": "levels/arena/arena.level.ron" }"#,
        &["asset_not_found", "manifest_missing"],
        &[bytes("asset", 4096)],
    ),
    c(
        "content.request",
        r#"{ "chunks": ["arena"], "priority": "high", "mount_when_done": true }"#,
        &["chunk_cycle", "chunk_not_found", "manifest_missing"],
        &[items("chunks", 1024)],
    ),
    c(
        "content.progress",
        r#"{ "request": 1 }"#,
        &["manifest_missing", "request_not_found"],
        &[],
    ),
    c(
        "content.pause",
        r#"{ "request": 1 }"#,
        &["manifest_missing", "request_finished", "request_not_found"],
        &[],
    ),
    c(
        "content.resume",
        r#"{ "request": 1 }"#,
        &[
            "manifest_missing",
            "request_finished",
            "request_not_found",
            "request_not_paused",
        ],
        &[],
    ),
    c(
        "content.cancel",
        r#"{ "request": 1 }"#,
        &["manifest_missing", "request_finished", "request_not_found"],
        &[],
    ),
    c(
        "content.approve_metered",
        r#"{ "request": 1 }"#,
        &["manifest_missing", "request_finished", "request_not_found"],
        &[],
    ),
    c(
        "content.mount",
        r#"{ "chunk": "arena" }"#,
        &[
            "chunk_base_mismatch",
            "chunk_not_found",
            "chunk_not_installed",
            "manifest_missing",
            "source_not_supported",
        ],
        &[bytes("chunk", 64)],
    ),
    c(
        "content.unmount",
        r#"{ "chunk": "arena", "force": true }"#,
        &[
            "chunk_in_use",
            "chunk_not_found",
            "manifest_missing",
            "source_not_supported",
        ],
        &[bytes("chunk", 64)],
    ),
    c(
        "content.release",
        r#"{ "chunks": ["arena"], "keep": [] }"#,
        &["chunk_in_use", "chunk_not_found", "manifest_missing"],
        &[items("chunks", 1024), items("keep", 1024)],
    ),
    c(
        "content.set_source",
        r#"{ "kind": "dir", "path": "dist/linux/chunks" }"#,
        &["manifest_missing", "source_not_supported"],
        &[bytes("path", 4096), bytes("base_url", 4096)],
    ),
    c(
        "content.check_host",
        r#"{ "base_url": "http://127.0.0.1:9/game" }"#,
        &[
            "chunk_base_mismatch",
            "content_signature_invalid",
            "content_signature_missing",
            "manifest_missing",
            "source_not_supported",
            "source_unreachable",
        ],
        &[bytes("base_url", 4096)],
    ),
    c(
        "content.inject",
        r#"{ "chunk": "arena", "fault": "corrupt" }"#,
        &["chunk_not_found", "manifest_missing"],
        &[range("after_bytes", 0.0, 1.0e15)],
    ),
    c("content.network", r#"{}"#, &[], &[]),
    // Level directories, data layers and placed levels (spec 37).
    c(
        "level.storage",
        r#"{ "level": "levels/forest.level.ron" }"#,
        &[],
        &[],
    ),
    c(
        "level.convert",
        r#"{ "path": "levels/forest.level.ron", "to": "dir" }"#,
        &[],
        &[],
    ),
    c("level.layers", r#"{ "layer": "night_props" }"#, &[], &[]),
    c(
        "level.set_layer_state",
        r#"{ "layer": "night_props", "state": "live", "recursive": false }"#,
        &[],
        &[],
    ),
    c(
        "level.set_membership",
        r#"{ "entities": [4294967298], "add": ["night_props"] }"#,
        &["F0003"],
        &[items("entities", 4096), items("add", 64), items("remove", 64)],
    ),
    c("level.placed", r#"{}"#, &[], &[]),
    c(
        "level.place_level",
        r#"{ "level": "asset:6f1e2b3c-4d5a-4a35-9d20-3f7a1c2e8b64", "transform": { "translation": [0, 0, 0] } }"#,
        &["F0003"],
        &[],
    ),
    c("level.break_placed", r#"{ "entity": 4294967298 }"#, &[], &[]),
    // The level switch with cancel (ADR-8027): F0003 without the LevelSwitch plugin.
    c(
        "level.switch",
        r#"{ "path": "levels/canyon.level.ron" }"#,
        &["F0003", "level_switch_bad_params"],
        &[bytes("path", 1024)],
    ),
    c("level.switch_status", r#"{}"#, &["F0003"], &[]),
    // The shader-permutation policy (ADR-2026): F0003 without a RuntimeRenderer.
    c("render.permutations", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_permutation_policy",
        r#"{ "enforcement": "fallback", "profile": "DesktopHigh", "graphs": [{ "key": "00000000000000aa" }] }"#,
        &[
            "F0003",
            "permutation_bad_enforcement",
            "permutation_manifest_unreadable",
            "permutation_unknown_profile",
        ],
        &[],
    ),
    // Session lifecycle and the dedicated-server host (book/agents/net-session.md,
    // net-dedicated-server.md, ADR-1663): F0002 with data.reason.
    c("session.state", r#"{}"#, &[], &[]),
    c(
        "session.host",
        r#"{ "mode": "listen", "port": 7777, "level": "levels/arena.level.ron" }"#,
        &[],
        &[],
    ),
    c(
        "session.connect",
        r#"{ "address": "127.0.0.1:7777", "players": ["Ann"] }"#,
        &[
            "net_bad_host_id",
            "net_known_hosts_newer",
            "net_known_hosts_unreadable",
        ],
        &[],
    ),
    c("session.disconnect", r#"{}"#, &[], &[]),
    c(
        "session.travel",
        r#"{ "level": "levels/harbor.level.ron" }"#,
        &[],
        &[],
    ),
    c("session.manifest", r#"{}"#, &[], &[]),
    c("net.conditioner_get", r#"{}"#, &[], &[]),
    c(
        "net.conditioner_apply",
        r#"{ "preset": "bad", "seed": 7 }"#,
        &[],
        &[],
    ),
    c(
        "net.sim_clients",
        r#"{ "action": "add", "count": 8, "input": "random", "seed": 1 }"#,
        &[],
        &[],
    ),
    c("net.stats", r#"{}"#, &[], &[]),
    c("net.traffic", r#"{}"#, &[], &[]),
    c("server.status", r#"{}"#, &[], &[]),
    c("server.kick", r#"{ "player": "3.0", "reason": "afk" }"#, &[], &[]),
    c(
        "server.ban",
        r#"{ "player": "3.0", "duration_s": 3600, "reason": "spam" }"#,
        &[],
        &[],
    ),
    c("server.unban", r#"{ "entry_id": 1 }"#, &[], &[]),
    c("server.bans", r#"{}"#, &[], &[]),
    c(
        "server.change_level",
        r#"{ "level": "levels/arena.level.ron" }"#,
        &[],
        &[],
    ),
    c("server.reload_config", r#"{}"#, &[], &[]),
    c(
        "server.shutdown",
        r#"{ "grace_s": 5, "confirm": true }"#,
        &[],
        &[],
    ),
    c("server.rotation_get", r#"{}"#, &["server_no_rotation"], &[]),
    c("server.rotation_skip", r#"{}"#, &["server_no_rotation"], &[]),
    // A dedicated server's kept world (book/agents/net-server-persistence.md, ADR-3354).
    c(
        "server.save_now",
        r#"{}"#,
        &[
            "persistence_disabled",
            "persistence_encode_failed",
            "persistence_write_failed",
            "server_persistence_missing",
        ],
        &[],
    ),
    c(
        "server.persistence_status",
        r#"{}"#,
        &["server_persistence_missing"],
        &[],
    ),
    c(
        "server.restore",
        r#"{ "slot": 12, "confirm": true }"#,
        &[
            "confirmation_required",
            "persistence_disabled",
            "persistence_save_corrupt",
            "persistence_slot_unknown",
            "server_persistence_missing",
        ],
        &[],
    ),
    // A lockstep server's match rows (book/agents/net-lockstep-server.md, ADR-8558).
    c(
        "server.lockstep_status",
        r#"{}"#,
        &["lockstep_not_running", "server_host_missing"],
        &[],
    ),
    // Spline components (specs/64 § 2.3, book/agents/splines.md).
    c(
        "spline.get",
        r#"{ "entity": 4294967310, "world": false }"#,
        &["F0003"],
        &[],
    ),
    c(
        "spline.set",
        r#"{ "entity": 4294967310, "points": [[0, 0, 0], [10, 0, 0]], "closed": false }"#,
        &["F0003"],
        &[items("points", 4096), items("attrs", 64)],
    ),
    c(
        "spline.edit",
        r#"{ "entity": 4294967310, "ops": [{ "op": "move", "index": 0, "position": [1, 0, 0] }] }"#,
        &["F0003"],
        &[items("ops", 4096)],
    ),
    heavy(
        "spline.sample",
        r#"{ "entity": 4294967310, "count": 3 }"#,
        &["F0003"],
        &[items("at", 100_000), range("count", 1.0, 100_000.0)],
    ),
    c(
        "spline.closest",
        r#"{ "entity": 4294967310, "point": [1, 0, 2] }"#,
        &["F0003"],
        &[items("point", 3)],
    ),
    c("biome.areas", r#"{}"#, &["F0003"], &[]),
    c("biome.claim", r#"{ "point": [0, 0, 0] }"#, &["F0003"], &[]),
    c("biome.stats", r#"{ "generator": 4294967301 }"#, &["F0003"], &[]),
    c(
        "biome.def_get",
        r#"{ "path": "biomes/meadow.biome.ron" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "biome.def_edit",
        r#"{ "path": "biomes/meadow.biome.ron", "ops": [{ "op": "set", "pointer": "/priority", "value": 5 }] }"#,
        &["F0003"],
        &[items("ops", 256)],
    ),
    c(
        "biome.preview",
        r#"{ "area": 4294967301, "on": true }"#,
        &["F0003"],
        &[],
    ),
    c(
        "biome.validate_ecology",
        r#"{ "path": "biomes/birch.biome.ron" }"#,
        &["F0003"],
        &[],
    ),
    c("procgen.tiles", r#"{ "entity": 4294967301 }"#, &["F0003"], &[]),
    c("procgen.anchors", r#"{}"#, &["F0003"], &[]),
    heavy(
        "procgen.tiles_wait",
        r#"{ "timeout_ms": 1 }"#,
        &["F0003"],
        &[range("timeout_ms", 0.0, 120_000.0)],
    ),
    c(
        "procgen.tiles_set",
        r#"{ "entity": 4294967301, "tier": 0, "build_m": 200 }"#,
        &["F0003"],
        &[],
    ),
    heavy(
        "procgen.tiles_rebuild",
        r#"{ "entity": 4294967301 }"#,
        &["F0003"],
        &[items("cells", 4096)],
    ),
    heavy(
        "procgen.plant_stats",
        r#"{ "graph": "starter/plant_bush.pgraph.ron", "seed": 3 }"#,
        &["F0003"],
        &[],
    ),
    heavy(
        "procgen.plant_build",
        r#"{ "graph": "starter/plant_bush.pgraph.ron", "out": "generated/plants", "variants": 2 }"#,
        &["F0003"],
        &[],
    ),
    heavy(
        "procgen.wfc_solve",
        r#"{ "size": [8, 8], "tiles": [{ "name": "grass", "weight": 3.0 }, { "name": "road" }], "rules": [{ "tile": 1, "dir": "east", "allowed": [1] }], "seed": 1 }"#,
        &[],
        &[items("tiles", 4096), range("max_backtracks", 0.0, 1_000_000.0)],
    ),
    heavy(
        "procgen.profile",
        r#"{ "entity": 4294967301, "threads": [0, 2] }"#,
        &["F0003"],
        &[items("threads", 8)],
    ),
    // Video playback (ADR-1670): F0002 with data.reason video_* (book/agents/video-playback.md).
    c(
        "video.probe",
        r#"{ "source": "movies/intro.video.ron" }"#,
        &[
            "video_container_invalid",
            "video_container_unsupported",
            "video_io_failed",
            "video_limit_exceeded",
            "video_source_not_found",
        ],
        &[bytes("source", 4096)],
    ),
    c("video.list", r#"{}"#, &[], &[]),
    c(
        "video.status",
        r#"{ "player": "intro" }"#,
        &["video_player_not_found"],
        &[],
    ),
    c(
        "video.frame_hash",
        r#"{ "player": "intro", "region": { "x": 0, "y": 0, "w": 64, "h": 64 } }"#,
        &["video_player_not_found"],
        &[],
    ),
    c(
        "video.open",
        r#"{ "player": "intro", "source": "movies/intro.video.ron", "clock": "lockstep", "autoplay": false }"#,
        &[
            "video_codec_patented",
            "video_codec_unsupported",
            "video_container_invalid",
            "video_container_unsupported",
            "video_decode_failed",
            "video_io_failed",
            "video_limit_exceeded",
            "video_player_not_found",
            "video_source_not_found",
            "video_track_not_found",
        ],
        &[bytes("source", 4096), range("phase_offset", 0.0, 86_400.0)],
    ),
    c(
        "video.play",
        r#"{ "player": "intro" }"#,
        &["video_bad_state", "video_player_not_found"],
        &[],
    ),
    c(
        "video.pause",
        r#"{ "player": "intro" }"#,
        &["video_bad_state", "video_player_not_found"],
        &[],
    ),
    c(
        "video.seek",
        r#"{ "player": "intro", "time": 2.5, "mode": "accurate" }"#,
        &[
            "video_bad_state",
            "video_player_not_found",
            "video_seek_out_of_range",
        ],
        &[range("time", -1.0e9, 1.0e9), range("frame", 0.0, 1.0e12)],
    ),
    c(
        "video.step",
        r#"{ "player": "intro", "frames": 30 }"#,
        &["video_bad_state", "video_player_not_found"],
        &[range("frames", 1.0, 10_000.0)],
    ),
    c(
        "video.set_rate",
        r#"{ "player": "intro", "rate": 0.5 }"#,
        &[
            "video_bad_state",
            "video_player_not_found",
            "video_rate_unsupported",
        ],
        &[],
    ),
    c(
        "video.set_loop",
        r#"{ "player": "intro", "mode": "count", "count": 2 }"#,
        &["video_player_not_found"],
        &[],
    ),
    c(
        "video.set_volume",
        r#"{ "player": "intro", "volume": 0.8 }"#,
        &["video_player_not_found"],
        &[range("volume", 0.0, 4.0)],
    ),
    c(
        "video.select_track",
        r#"{ "player": "intro", "kind": "audio", "index": 1 }"#,
        &[
            "video_decode_failed",
            "video_player_not_found",
            "video_source_not_found",
            "video_track_not_found",
        ],
        &[],
    ),
    c(
        "video.skip",
        r#"{ "player": "intro" }"#,
        &["video_player_not_found"],
        &[],
    ),
    c(
        "video.close",
        r#"{ "player": "intro" }"#,
        &["video_player_not_found"],
        &[],
    ),
    c(
        "video.create_player",
        r#"{ "source": "movies/intro.video.ron", "name": "intro", "clock": "lockstep" }"#,
        &["video_not_installed", "video_rate_unsupported"],
        &[bytes("source", 4096), bytes("name", 256)],
    ),
    c(
        "physics.characters",
        r#"{ "entity": 4294967310 }"#,
        &["F0003", "character_not_found"],
        &[],
    ),
    c(
        "anim.ragdoll_settings",
        r#"{ "joint_limits": true, "max_simulated": 8 }"#,
        &["ragdoll_bad_budget", "ragdoll_bad_flag"],
        &[],
    ),
    c(
        "physics.character_traversal",
        r#"{ "entity": 4294967310 }"#,
        &["F0003", "character_not_found", "traversal_not_enabled"],
        &[],
    ),
    c(
        "physics.character_traversal_probe",
        r#"{ "entity": 4294967310, "position": [0.0, 1.2, 0.0], "forward": [0.0, 0.0, -1.0] }"#,
        &[
            "F0003",
            "character_not_found",
            "traversal_bad_param",
            "traversal_not_enabled",
        ],
        &[],
    ),
    c("render.get_dynamic_gi", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_dynamic_gi",
        r#"{ "tier": "low", "enabled": true }"#,
        &[
            "F0003",
            "dynamic_gi_no_renderer",
            "dynamic_gi_unavailable",
            "unknown_gi_tier",
        ],
        &[bytes("tier", 16)],
    ),
    // Net client prediction (book/agents/net-prediction.md, ADR-1793): F0002 with data.reason
    // (net_not_client, net_not_server, entity_not_predicted, peer_unknown, net_not_running).
    c("net.prediction", r#"{}"#, &[], &[]),
    c("net.input_buffer", r#"{ "peer": 1 }"#, &[], &[]),
    c(
        "net.prediction_history",
        r#"{ "entity": 4294967310, "ticks": 8 }"#,
        &[],
        &[],
    ),
    c(
        "net.force_correction",
        r#"{ "entity": 4294967310, "offset": [0.0, 0.0, 1.0] }"#,
        &[],
        &[],
    ),
    // Net lag compensation (book/agents/net-lag-compensation.md, ADR-1752): F0002 with
    // data.reason (net_not_running, net_not_server, net_not_client, lagcomp_not_installed,
    // peer_unknown, entity_not_found, entity_not_lag_compensated, tick_out_of_history).
    c("lagcomp.stats", r#"{}"#, &[], &[]),
    c("lagcomp.history", r#"{ "entity": 4294967310 }"#, &[], &[]),
    c(
        "lagcomp.raycast",
        r#"{ "peer": 1, "origin": [0, 1.6, 0], "direction": [0, 0, 1], "max_dist": 100 }"#,
        &["lagcomp_dev_only"],
        &[],
    ),
    c("lagcomp.last_queries", r#"{ "limit": 8 }"#, &[], &[]),
    c("lagcomp.echoes", r#"{ "limit": 8 }"#, &[], &[]),
    // Exact-tick joints (ADR-2551): dev builds only (lagcomp_dev_only).
    c(
        "lagcomp.rewound_shapes",
        r#"{ "entity": 4294967310, "tick": 120.5 }"#,
        &["lagcomp_dev_only"],
        &[],
    ),
    // Retargeting and crowds (ADR-1857).
    c(
        "anim.retarget_check",
        r#"{ "source": 4294967310, "target": 4294967311, "clip": "walk" }"#,
        &[
            "F0003",
            "bad_setup",
            "bad_skeleton",
            "missing_joint",
            "no_common_roles",
            "unknown_clip",
        ],
        &[bytes("setup", 65536), bytes("clip", 1024)],
    ),
    c(
        "anim.crowd_status",
        r#"{ "agent": 12 }"#,
        &["F0003", "bad_agent"],
        &[range("agent", 0.0, 4294967295.0)],
    ),
    c("video.codecs", r#"{}"#, &[], &[]),
    // Video renderer half (ADR-2044): F0002 with data.reason media_* (book/agents/media-render.md).
    c(
        "media.state",
        r#"{ "player": "intro" }"#,
        &["media_player_not_found"],
        &[],
    ),
    c(
        "media.play",
        r#"{ "player": "intro" }"#,
        &[
            "media_control_queue_full",
            "media_not_installed",
            "media_player_not_found",
        ],
        &[],
    ),
    c(
        "media.pause",
        r#"{ "player": "intro" }"#,
        &[
            "media_control_queue_full",
            "media_not_installed",
            "media_player_not_found",
        ],
        &[],
    ),
    c(
        "media.seek",
        r#"{ "player": "intro", "time": 1.5 }"#,
        &[
            "media_control_queue_full",
            "media_not_installed",
            "media_player_not_found",
        ],
        &[],
    ),
    c(
        "media.set_loop",
        r#"{ "player": "intro", "loop": true }"#,
        &[
            "media_control_queue_full",
            "media_not_installed",
            "media_player_not_found",
        ],
        &[],
    ),
    c("render.get_occlusion", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_occlusion",
        r#"{ "two_phase": true }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.set_shadow_occlusion",
        r#"{ "on": true, "min_casters": 512 }"#,
        &["F0003"],
        &[],
    ),
    c("render.get_radiance_cache", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_radiance_cache",
        r#"{ "enabled": true, "budget_cells": 2048 }"#,
        &[
            "F0003",
            "radiance_cache_bad_attribution",
            "radiance_cache_bad_budget",
            "radiance_cache_bad_gather",
            "radiance_cache_unavailable",
        ],
        &[],
    ),
    // Lobbies and matchmaking (book/agents/net-lobbies.md, ADR-1877): F0002 with data.reason.
    c("lobby.status", r#"{}"#, &[], &[]),
    c(
        "lobby.list",
        r#"{ "strings": [{ "key": "mode", "op": "eq", "value": "duel" }], "open_slots": 1 }"#,
        &[],
        &[],
    ),
    c(
        "lobby.create",
        r#"{ "visibility": "public", "max_members": 4, "data": { "mode": "duel" } }"#,
        &[],
        &[],
    ),
    c("lobby.join", r#"{ "lobby": "1" }"#, &[], &[]),
    c("lobby.leave", r#"{}"#, &[], &[]),
    c(
        "lobby.set_data",
        r#"{ "key": "map", "value": "harbor" }"#,
        &[],
        &[],
    ),
    c("lobby.ready", r#"{ "ready": true }"#, &[], &[]),
    c("lobby.chat", r#"{ "text": "gl hf" }"#, &[], &[]),
    c(
        "lobby.launch",
        r#"{ "level": "levels/arena.level.ron" }"#,
        &[],
        &[],
    ),
    c(
        "matchmaking.start",
        r#"{ "strings": [{ "key": "mode", "value": "duel" }], "min_players": 2 }"#,
        &[],
        &[],
    ),
    c("matchmaking.cancel", r#"{}"#, &[], &[]),
    c("matchmaking.status", r#"{}"#, &[], &[]),
    // Networked characters (book/agents/net-character.md, ADR-1905): F0002 with data.reason
    // (net_not_running).
    c("net.characters", r#"{}"#, &[], &[]),
    c("net.vehicles", r#"{}"#, &[], &[]),
    c("net.hovers", r#"{}"#, &[], &[]),
    // Loudness at import (book/agents/loudness-import.md, ADR-1855).
    heavy(
        "audio.loudness",
        r#"{ "sound": "door" }"#,
        &["F0003", "F0013", "loudness_decode_failed"],
        &[
            range("target_lufs", -70.0, 0.0),
            bytes("sound", 256),
            bytes("path", 4096),
        ],
    ),
    c(
        "audio.set_loudness_normalization",
        r#"{ "enabled": true, "table": ".regia/cooked/linux/loudness.ron" }"#,
        &[
            "F0003",
            "F0013",
            "loudness_no_table",
            "loudness_table_invalid",
            "loudness_table_unreadable",
            "loudness_unsupported_version",
        ],
        &[bytes("table", 4096)],
    ),
    // Music on the audio clock and the listener (book/agents/audio-music.md, ADR-2517).
    c("audio.music_state", r#"{}"#, &["F0013"], &[]),
    c(
        "audio.music_transition",
        r#"{ "to": "calm", "sync": "next_bar", "crossfade_secs": 2.0 }"#,
        &["F0013", "music_start_failed", "music_unknown_track"],
        &[
            bytes("to", 256),
            bytes("sync", 32),
            range("crossfade_secs", 0.0, 600.0),
        ],
    ),
    c(
        "audio.music_stinger",
        r#"{ "name": "victory", "sync": "next_beat" }"#,
        &[
            "F0013",
            "music_not_playing",
            "music_start_failed",
            "music_unknown_stinger",
        ],
        &[bytes("name", 256), bytes("sync", 32)],
    ),
    c("audio.listener_info", r#"{}"#, &["F0013"], &[]),
    c("ui.view_models", r#"{}"#, &["F0003", "F0013"], &[]),
    c("ui.collection_state", r#"{}"#, &["F0003", "F0013"], &[]),
    c(
        "ui.set_group",
        r#"{ "name": "Panel", "group_opacity": 0.5, "clip": "rounded" }"#,
        &["F0003", "F0013"],
        &[],
    ),
    c("ui.layer_stats", r#"{}"#, &["F0013"], &[]),
    c(
        "ui.set_scale_mode",
        r#"{ "mode": "integer", "reference": [320, 180] }"#,
        &["F0003", "F0013"],
        &[],
    ),
    c(
        "ui.image_assets",
        r#"{ "status": "failed" }"#,
        &["F0003", "F0013"],
        &[],
    ),
    c(
        "render.set_vendor_upscaler",
        r#"{"vendor": "none"}"#,
        &["F0003"],
        &[],
    ),
    // The forward shader's studio axis (ADR-2308): F0003 without a RuntimeRenderer.
    c("render.forward_studio", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_forward_studio",
        r#"{ "mode": "full" }"#,
        &["F0003", "forward_studio_bad_mode"],
        &[],
    ),
    // The terrain's caster proxy for the traced sun (ADR-2310): F0003 without the control.
    c("terrain.trace_proxy", r#"{}"#, &["F0003"], &[]),
    c(
        "terrain.set_trace_proxy",
        r#"{ "spacing_m": 4.0 }"#,
        &["F0003"],
        &[],
    ),
    // Authored collision proxies (ADR-2341): F0016 for an unknown glTF or no AssetServer, F0013
    // for a collision set that does not read.
    c(
        "assets.collision_report",
        r#"{ "mesh": "assets/props/crate.glb" }"#,
        &[
            "F0013",
            "F0016",
            "bad_mesh",
            "collision_set_corrupt",
            "collision_set_invalid",
            "collision_set_unreadable",
            "collision_set_version",
            "collision_set_wrong_type",
        ],
        &[bytes("mesh", 4096)],
    ),
    // Jobs, tests, review, open in editor (book/agents/jobs.md, staged-review.md, ADR-2346): F0002 or
    // F0003 with data.reason.
    c(
        "agent.job_status",
        r#"{ "job": "tests:1", "since": 0 }"#,
        &["F0003", "job_id_invalid", "job_not_found"],
        &[bytes("job", 256), bytes("id", 256)],
    ),
    c(
        "agent.job_cancel",
        r#"{ "job": "tests:1" }"#,
        &["F0003", "job_id_invalid", "job_not_cancellable", "job_not_found"],
        &[bytes("job", 256), bytes("id", 256)],
    ),
    c(
        "agent.run_tests",
        r#"{ "target": "gameplay", "filter": "door_opens" }"#,
        &[
            "run_tests_bad_name",
            "run_tests_busy",
            "run_tests_no_manifest",
            "run_tests_spawn_failed",
        ],
        &[bytes("target", 128), bytes("package", 128), bytes("filter", 128)],
    ),
    // Code methods (book/agents/code.md, ADR-5707): F0002 with data.reason.
    c(
        "agent.build_game",
        r#"{ "profile": "release" }"#,
        &[
            "build_game_bad_name",
            "build_game_busy",
            "build_game_no_manifest",
            "build_game_spawn_failed",
        ],
        &[bytes("profile", 128), bytes("package", 128)],
    ),
    // Pipeline methods (book/agents/cook-package.md, ADR-7217): F0002 with data.reason, F0013 on
    // Android (cook_desktop_only).
    c(
        "agent.cook",
        r#"{ "platform": "linux", "quality": "fast", "dry_run": true }"#,
        &[
            "F0013",
            "cook_bad_platform",
            "cook_bad_quality",
            "cook_busy",
            "cook_desktop_only",
            "cook_no_manifest",
            "cook_spawn_failed",
        ],
        &[],
    ),
    c(
        "agent.package",
        r#"{ "platform": "linux", "profile": "shipping", "dry_run": true }"#,
        &[
            "F0003",
            "F0013",
            "cook_bad_platform",
            "cook_busy",
            "cook_desktop_only",
            "cook_no_manifest",
            "cook_spawn_failed",
            "needs_confirmation",
            "package_bad_container",
            "package_bad_profile",
            "path_outside_project",
        ],
        &[bytes("profile", 128)],
    ),
    c(
        "agent.hot_reload",
        r#"{ "dry_run": true }"#,
        &[
            "editor_not_running",
            "hot_reload_busy",
            "hot_reload_no_game",
            "hot_reload_no_report",
        ],
        &[],
    ),
    c(
        "agent.open_in_editor",
        r#"{ "asset": "prefabs/door.prefab.ron" }"#,
        &[
            "F0003",
            "editor_not_running",
            "open_in_editor_no_code_editor",
            "open_in_editor_unsupported",
        ],
        &[
            bytes("asset", 4096),
            bytes("file", 4096),
            range("line", 1.0, 1.0e7),
        ],
    ),
    c(
        "review.propose",
        r#"{ "changes": [{ "path": "materials/door.material.ron", "text": "(base_color: (0.8, 0.3, 0.2, 1.0))\n" }], "reason": "warmer door color, as asked" }"#,
        &[
            "review_not_text",
            "review_path_denied",
            "review_queue_full",
            "review_too_large",
        ],
        &[items("changes", 64), bytes("reason", 4096), bytes("title", 256)],
    ),
    c("review.list", r#"{ "status": "pending" }"#, &[], &[]),
    c(
        "review.diff",
        r#"{ "id": 1 }"#,
        &["F0003", "review_change_not_found"],
        &[],
    ),
    c(
        "review.accept",
        r#"{ "set": 1, "note": "looks right" }"#,
        &[
            "review_already_decided",
            "review_change_not_found",
            "review_conflict",
            "review_path_denied",
            "review_write_failed",
        ],
        &[items("ids", 256), bytes("note", 4096)],
    ),
    c(
        "review.reject",
        r#"{ "ids": [2], "note": "wrong file" }"#,
        &["review_already_decided", "review_change_not_found"],
        &[items("ids", 256), bytes("note", 4096)],
    ),
    // Golden tools (book/agents/goldens.md, ADR-2821): F0002 or F0003 with data.reason.
    c(
        "agent.golden_test",
        r#"{ "name": "sponza" }"#,
        &[
            "F0003",
            "golden_ambiguous",
            "golden_bad_name",
            "golden_no_test",
            "golden_unknown",
            "run_tests_busy",
            "run_tests_no_manifest",
        ],
        &[bytes("name", 128)],
    ),
    c(
        "agent.bless_golden",
        r#"{ "name": "sponza", "reason": "the new sun angle is intended", "job": "golden:1" }"#,
        &[
            "F0003",
            "golden_already_pending",
            "golden_ambiguous",
            "golden_bad_name",
            "golden_job_mismatch",
            "golden_job_not_found",
            "golden_job_not_run",
            "golden_reason_required",
            "golden_stale",
            "golden_unchanged",
            "golden_unknown",
            "review_queue_full",
        ],
        &[bytes("name", 128), bytes("reason", 4096), bytes("job", 256)],
    ),
    // The frame profiler (ADR-2323): F0003 without a profiler or (capture) a project root.
    c("profiler.set_capture", r#"{ "flame": true }"#, &["F0003"], &[]),
    c(
        "profiler.frame",
        r#"{ "frames": 60, "depth": 8 }"#,
        &["F0003"],
        &[
            range("frames", 1.0, 300.0),
            range("depth", 1.0, 32.0),
            range("max_nodes", 1.0, 5000.0),
            range("entities", 1.0, 200.0),
        ],
    ),
    c(
        "profiler.capture",
        r#"{ "path": "captures/frame.rprof.json", "frames": 60 }"#,
        &["F0003"],
        &[
            range("frames", 1.0, 300.0),
            range("depth", 1.0, 32.0),
            range("max_nodes", 1.0, 5000.0),
        ],
    ),
    // F0003 without a project root or a capture file (F0002, a file that is not a capture, is
    // universal).
    c(
        "profiler.compare",
        r#"{ "a": "captures/before.rprof.json", "b": "captures/after.rprof.json" }"#,
        &["F0003"],
        &[range("top", 1.0, 5000.0)],
    ),
    // F0003 without a profiler or while the timeline is off (ADR-7325).
    c(
        "profiler.timeline",
        r#"{ "max_spans": 1000 }"#,
        &["F0003"],
        &[range("max_spans", 1.0, 100000.0), range("frames", 1.0, 8.0)],
    ),
    c(
        "world.hlod_proxy",
        r#"{ "level": "00000000-0000-0000-0000-000000000abc", "node": 0 }"#,
        &[
            "hlod_index_unreadable",
            "hlod_no_proxies",
            "hlod_node_unknown",
            "hlod_voxels_unreadable",
        ],
        &[],
    ),
    c(
        "render.set_light_profile",
        r#"{ "name": "lights/downlight.ies", "entity": 4294967301 }"#,
        &["F0003"],
        &[],
    ),
    heavy(
        "seeing.temporal_chart",
        r#"{ "pan_px": 2, "frames": 120, "scale": 0.67, "upscaler": "tsr" }"#,
        &[
            "temporal_chart_no_device",
            "temporal_chart_render_failed",
            "temporal_chart_unknown_upscaler",
            "temporal_chart_vendor_upscaler",
        ],
        &[
            range("pan_px", -32.0, 32.0),
            range("orbit_deg", -32.0, 32.0),
            range("frames", 2.0, 480.0),
            range("scale", 0.25, 1.0),
            bytes("upscaler", 32),
        ],
    ),
    // Angular CCD sweep (ADR-2423): F0003 when an `entity` side is missing or has no Collider.
    c(
        "physics.time_of_impact",
        r#"{ "a": { "shape": { "kind": "box", "half_extents": [1.0, 0.02, 0.02] } }, "b": { "shape": { "kind": "box", "half_extents": [2.0, 0.01, 2.0] }, "position": [0, 0.5, 0] }, "motion": { "duration": 0.1, "a": { "angular": [0, 0, 60] } } }"#,
        &["F0003"],
        &[range("max_iterations", 1.0, 100000.0)],
    ),
    // Particle liquid (regia_fluid, ADR-2441): lowercase reasons in data.reason, book/agents/fluid.md.
    c(
        "fluid.create",
        r#"{ "preset": "dam_break", "particle_cap": 20000 }"#,
        &[],
        &[
            range("cell_size", 0.001, 10.0),
            range("particle_cap", 1.0, 2000000.0),
        ],
    ),
    c(
        "fluid.add_emitter",
        r#"{ "id": 1, "kind": "source", "center": [0.2, 0.5, 0.05], "velocity": [1, 0, 0] }"#,
        &["F0003"],
        &[range("radius", 0.0, 1000.0)],
    ),
    heavy(
        "fluid.step",
        r#"{ "id": 1, "steps": 60 }"#,
        &["F0003"],
        &[range("steps", 0.0, 600.0)],
    ),
    c("fluid.stats", r#"{ "id": 1 }"#, &["F0003"], &[]),
    heavy(
        "fluid.bake",
        r#"{ "id": 1, "steps": 120, "path": "fluid/dam.fluidcache" }"#,
        &["F0003"],
        &[range("steps", 1.0, 600.0)],
    ),
    c("fluid.play", r#"{ "id": 1, "frame": 30 }"#, &["F0003"], &[]),
    c("fluid.destroy", r#"{ "id": 1 }"#, &["F0003"], &[]),
    // The known-hosts store (ADR-2416).
    c(
        "net.known_hosts",
        r#"{}"#,
        &[
            "net_known_hosts_newer",
            "net_known_hosts_no_store",
            "net_known_hosts_unreadable",
        ],
        &[],
    ),
    c(
        "net.trust",
        r#"{ "host": "203.0.113.7:7777", "key": "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a" }"#,
        &[
            "net_bad_host_id",
            "net_bad_server_key",
            "net_known_hosts_full",
            "net_known_hosts_newer",
            "net_known_hosts_no_store",
            "net_known_hosts_unreadable",
            "net_known_hosts_write_failed",
        ],
        &[bytes("host", 256)],
    ),
    c(
        "net.forget",
        r#"{ "host": "203.0.113.7:7777" }"#,
        &[
            "net_bad_host_id",
            "net_known_hosts_newer",
            "net_known_hosts_no_store",
            "net_known_hosts_unreadable",
            "net_known_hosts_write_failed",
        ],
        &[bytes("host", 256)],
    ),
    c("net.player_stats", r#"{}"#, &[], &[]),
    c("net.stats_overlay", r#"{ "visible": true }"#, &[], &[]),
    c("net.handoffs", r#"{}"#, &[], &[]),
    // Connectionless server bots (book/agents/net-session-players.md, ADR-3346).
    c(
        "net.bots",
        r#"{ "action": "add", "reaction_ticks": 4, "aim_error_radians": 0.05, "perception_radius": 30 }"#,
        &[
            "bot_limit",
            "bot_unknown",
            "bots_unavailable",
            "invalid_value",
            "net_not_running",
            "net_not_server",
            "player_slot_unavailable",
        ],
        &[],
    ),
    // The terrain editing remainder (ADR-2411, book/agents/level-terrain.md).
    c(
        "terrain.level_terrain",
        r#"{ "level": "maps/arena.level.ron" }"#,
        &["level_terrain_invalid"],
        &[bytes("level", 1024)],
    ),
    c(
        "terrain.set_level_terrain",
        r#"{ "terrain": { "layers": "maps/arena/terrain.layers.ron", "height_scale_m": 120.0 } }"#,
        &["level_terrain_invalid"],
        &[],
    ),
    heavy(
        "terrain.load_level_terrain",
        r#"{ "level": "maps/arena.level.ron" }"#,
        &[
            "level_terrain_decode",
            "level_terrain_invalid",
            "level_terrain_mapping",
            "level_terrain_read",
        ],
        &[bytes("level", 1024)],
    ),
    c(
        "terrain.grass_density_stroke",
        r#"{ "path": [[0.0, 0.0], [12.0, 4.0]], "radius": 6.0, "target": 0.0 }"#,
        &["F0003", "grass_density_bad_stroke", "grass_density_unreadable"],
        &[
            items("path", 4096),
            range("radius", 0.0, 4096.0),
            range("hardness", 0.0, 1.0),
            range("strength", 0.0, 1.0),
            range("target", 0.0, 1.0),
            range("texel_m", 0.0, 4096.0),
        ],
    ),
    c("terrain.undo_grass_density", r#"{}"#, &[], &[]),
    c("terrain.redo_grass_density", r#"{}"#, &[], &[]),
    // Non-destructive terrain edit layers (ADR-7825).
    c(
        "terrain.edit_layers",
        r#"{}"#,
        &["F0003", "edit_layer_no_terrain"],
        &[],
    ),
    c(
        "terrain.edit_layer_create",
        r#"{ "name": "Hills" }"#,
        &[
            "F0003",
            "edit_layer_bad_name",
            "edit_layer_limit",
            "edit_layer_no_terrain",
        ],
        &[],
    ),
    c(
        "terrain.edit_layer_set",
        r#"{ "layer": "Hills", "enabled": false }"#,
        &[
            "F0003",
            "edit_layer_bad_name",
            "edit_layer_bad_opacity",
            "edit_layer_hidden",
            "edit_layer_locked",
            "edit_layer_no_terrain",
            "edit_layer_not_found",
        ],
        &[range("opacity", 0.0, 1.0)],
    ),
    c(
        "terrain.edit_layer_delete",
        r#"{ "layer": "Hills" }"#,
        &[
            "F0003",
            "edit_layer_locked",
            "edit_layer_no_terrain",
            "edit_layer_not_found",
        ],
        &[],
    ),
    c(
        "terrain.edit_layer_reorder",
        r#"{ "layer": "Hills", "index": 0 }"#,
        &["F0003", "edit_layer_no_terrain", "edit_layer_not_found"],
        &[],
    ),
    c(
        "terrain.edit_layer_flatten",
        r#"{}"#,
        &[
            "F0003",
            "edit_layer_locked",
            "edit_layer_no_terrain",
            "edit_layer_size_mismatch",
        ],
        &[],
    ),
    c(
        "terrain.edit_layer_weights",
        r#"{ "col": 1, "row": 1 }"#,
        &["F0003", "edit_layer_no_terrain", "edit_layer_not_found"],
        &[],
    ),
    c(
        "terrain.edit_layer_paint_weights",
        r#"{ "layer": "Hills", "col": 1, "row": 1, "channels": [0.0, 1.0] }"#,
        &[
            "F0003",
            "edit_layer_bad_weight",
            "edit_layer_hidden",
            "edit_layer_locked",
            "edit_layer_no_terrain",
            "edit_layer_not_found",
            "edit_layer_owned",
        ],
        &[],
    ),
    c(
        "terrain.edit_layer_procgen",
        r#"{ "layer": "Hills", "graph": "assets/hills.pgraph.ron", "output": "height" }"#,
        &[
            "F0003",
            "edit_layer_bad_target",
            "edit_layer_no_terrain",
            "edit_layer_not_found",
        ],
        &[],
    ),
    c(
        "terrain.grass_density",
        r#"{}"#,
        &["F0003", "grass_density_none"],
        &[],
    ),
    heavy(
        "terrain.save_grass_density",
        r#"{ "path": "maps/arena.level.ron" }"#,
        &["F0003", "grass_density_none", "grass_density_write"],
        &[bytes("path", 1024)],
    ),
    heavy(
        "terrain.load_grass_density",
        r#"{ "path": "maps/arena.grassdensity.bin" }"#,
        &["F0003", "grass_density_decode", "grass_density_read"],
        &[bytes("path", 1024)],
    ),
    heavy(
        "terrain.scatter_rules",
        r#"{ "path": "maps/arena.scatter.ron" }"#,
        &["F0003", "scatter_rules_invalid", "scatter_rules_read"],
        &[bytes("path", 1024)],
    ),
    heavy(
        "terrain.set_scatter_rule",
        r#"{ "path": "maps/arena.scatter.ron", "class": "rocks", "field": "density", "value": 0.5 }"#,
        &[
            "F0003",
            "scatter_rules_field",
            "scatter_rules_invalid",
            "scatter_rules_read",
            "scatter_rules_write",
        ],
        &[bytes("path", 1024), bytes("class", 256), bytes("field", 256)],
    ),
    // Spec 50 remainder (ADR-2410): F0002 with data.reason media_* / video_* (book/agents/video-boot-and-cues.md).
    c("media.boot_state", r#"{}"#, &[], &[]),
    c("media.boot_skip", r#"{}"#, &["media_not_installed"], &[]),
    c(
        "media.set_visible",
        r#"{ "player": "wall", "visible": false }"#,
        &["media_player_not_found"],
        &[],
    ),
    c(
        "media.cues",
        r#"{ "player": "intro" }"#,
        &["media_player_not_found"],
        &[],
    ),
    c("media.decoders", r#"{}"#, &["media_not_installed"], &[]),
    c(
        "media.validate",
        r#"{ "source": "movies/intro.webm" }"#,
        &[
            "video_codec_patented",
            "video_codec_unsupported",
            "video_container_invalid",
            "video_container_unsupported",
            "video_source_not_found",
        ],
        &[],
    ),
    c("procgen.load_report", r#"{}"#, &["F0003"], &[]),
    c("ai.bt_get", r#"{ "path": "assets/ai/guard.bt.ron" }"#, &[], &[]),
    c(
        "ai.bt_validate",
        r#"{ "path": "assets/ai/guard.bt.ron" }"#,
        &[],
        &[],
    ),
    c(
        "ai.bt_edit",
        r#"{ "path": "assets/ai/guard.bt.ron", "create": true, "save": false, "ops": [ { "op": "add_node", "kind": "selector", "name": "root" }, { "op": "add_node", "kind": "leaf", "parent": 0, "task": "game::Patrol" } ] }"#,
        &[],
        &[items("ops", 512)],
    ),
    c("ai.bt_live", r#"{ "entity": 4294967296 }"#, &[], &[]),
    // Camera-target and world-space UI (ADR-2406, book/agents/ui.md).
    c(
        "ui.world_click",
        r#"{ "origin": [0, 1.6, 4], "direction": [0, 0, -1] }"#,
        &["F0003", "F0013"],
        &[],
    ),
    c(
        "ui.set_target",
        r#"{ "root": "hud", "target": { "world": { "size": [512, 256], "ppm": 256 } } }"#,
        &["F0003", "F0013"],
        &[],
    ),
    c("ui.targets", r#"{}"#, &["F0003", "F0013"], &[]),
    // Entity kinds and the project map (ADR-2488).
    c("kinds.list", r#"{}"#, &[], &[]),
    c(
        "kinds.of",
        r#"{ "entity": 4294967297 }"#,
        &["kind_not_found"],
        &[],
    ),
    c(
        "kinds.lint",
        r#"{ "strictness": "warn" }"#,
        &["describe_no_project", "kind_bad_param"],
        &[],
    ),
    c(
        "project.describe",
        r#"{ "write": false }"#,
        &["describe_no_project", "describe_write_failed"],
        &[],
    ),
    // The `mods://` file system (ADR-2418).
    c("mods.list", r#"{ "conflicts": true }"#, &[], &[]),
    c(
        "mods.mount",
        r#"{ "id": "example.units", "path": "mods/units", "kind": "auto" }"#,
        &[
            "mods_duplicate",
            "mods_id_invalid",
            "mods_io",
            "mods_pak",
            "mods_path_escape",
            "mods_path_invalid",
            "mods_symlink",
            "mods_too_large",
        ],
        &[bytes("id", 64), bytes("path", 4096)],
    ),
    c(
        "mods.unmount",
        r#"{ "id": "example.units" }"#,
        &["mods_unknown_mod"],
        &[],
    ),
    c(
        "mods.order",
        r#"{ "order": ["base", "example.units"] }"#,
        &["mods_order_invalid"],
        &[items("order", 256)],
    ),
    c("mods.hash", r#"{ "publish": false }"#, &[], &[]),
    c(
        "mods.read",
        r#"{ "path": "mods://data/units.ron" }"#,
        &[
            "mods_changed",
            "mods_io",
            "mods_not_found",
            "mods_pak",
            "mods_path_escape",
            "mods_path_invalid",
            "mods_symlink",
            "mods_unknown_mod",
        ],
        &[bytes("path", 1024)],
    ),
    // Sequencer v3 (ADR-2405).
    c(
        "sequencer.spatial_voices",
        r#"{ "all": true }"#,
        &[
            "bad_entity",
            "bad_setting",
            "no_player",
            "several_players",
            "unknown_player",
        ],
        &[],
    ),
    c(
        "sequencer.sub_loops",
        r#"{ "time": 1.5 }"#,
        &[
            "bad_entity",
            "bad_setting",
            "no_player",
            "seq_invalid",
            "seq_loop_too_deep",
            "several_players",
            "unknown_player",
        ],
        &[],
    ),
    // Sequence subtitles (ADR-3334).
    c(
        "sequencer.subtitles",
        r#"{}"#,
        &["bad_entity", "no_player", "several_players", "unknown_player"],
        &[],
    ),
    c(
        "sequencer.subtitle_settings",
        r#"{ "burn_in": true }"#,
        &["bad_setting"],
        &[],
    ),
    c(
        "sequencer.rename_binding",
        r#"{ "from": "hero", "to": "guard" }"#,
        &[
            "bad_entity",
            "bad_name",
            "no_player",
            "seq_binding_empty",
            "seq_binding_in_use",
            "seq_binding_same",
            "seq_binding_table_in_use",
            "seq_binding_unknown",
            "seq_section_out_of_range",
            "several_players",
            "unknown_player",
        ],
        &[],
    ), // Take recorder (ADR-2637, book/agents/sequence-take-recorder.md).
    c(
        "sequence.record_start",
        r#"{ "bindings": [{ "name": "camera", "camera": true }], "countdown": 30 }"#,
        &[
            "bad_entity",
            "bad_setting",
            "take_bad_binding",
            "take_bad_channel",
            "take_bad_tolerance",
            "take_busy",
            "take_no_bindings",
            "take_too_many",
            "take_unsaved",
            "unknown_entity",
        ],
        &[
            range("countdown", 0.0, 100_000.0),
            range("fps", 0.0, 1000.0),
            items("bindings", 256),
        ],
    ),
    c(
        "configurator.contract_check",
        r#"{ "document": { "schema": "jewelry.build/1" }, "interface": "build" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "configurator.contract_schema",
        r#"{ "interface": "build" }"#,
        &[],
        &[],
    ),
    c(
        "configurator.open_product",
        r#"{ "product_key": "house-a/p0001", "root": "/var/lib/jewelry/out", "snapshot_id": "2026-11-04-000000000000", "library_version": "1", "kits": ["kits/shared/shared-basic/kit.json"], "tenant_id": "shop:example-shop", "tenant_houses": ["house-a"] }"#,
        &[
            "configurator_bad_document",
            "configurator_bad_path",
            "configurator_contract",
            "configurator_duplicate",
            "configurator_house_not_allowed",
            "configurator_io",
            "configurator_missing_file",
            "configurator_not_modelled",
            "configurator_not_orderable",
            "configurator_wrong_product",
        ],
        &[],
    ),
    c("configurator.options", r#"{}"#, &["configurator_not_open"], &[]),
    c(
        "configurator.set_pick",
        r#"{ "group": "Material", "value": "Metal B", "adjust": false }"#,
        &[
            "configurator_not_open",
            "configurator_not_sold",
            "configurator_pick_required",
            "configurator_pick_shape",
            "configurator_unknown_group",
            "configurator_unknown_value",
            "configurator_value_unavailable",
        ],
        &[],
    ),
    c(
        "configurator.build",
        r#"{ "wear_preset": "default", "output_gamut": "srgb" }"#,
        &[
            "configurator_build_invalid",
            "configurator_no_recipe",
            "configurator_not_open",
            "configurator_unknown_wear",
        ],
        &[],
    ),
    c(
        "configurator.resolve",
        r#"{ "picks": { "Material": "Metal A" } }"#,
        &["configurator_not_open"],
        &[],
    ),
    c(
        "viewer.view_simulate",
        r#"{ "view_range": { "tilt_max_deg": 30 }, "events": [{ "down": { "id": 1, "x": 100, "y": 100, "t": 0 } }, { "move": { "id": 1, "x": 400, "y": 100, "t": 16 } }, { "up": { "id": 1, "t": 300 } }, { "settle": true }] }"#,
        &[
            "view_bad_event",
            "view_range_bad_axis",
            "view_range_bad_camera",
            "view_range_bad_pan",
            "view_range_bad_rock",
            "view_range_bad_spin",
            "view_range_bad_tilt",
            "view_range_bad_value",
            "view_range_bad_zoom",
            "view_too_many_events",
        ],
        &[],
    ),
    c(
        "viewer.pack_verify",
        r#"{ "dir": "packages/standin-round-prong-end" }"#,
        &[],
        &[],
    ),
    c("viewer.capabilities", r#"{}"#, &[], &[]),
    heavy(
        "viewer.render_still",
        r#"{ "width": 256, "height": 256, "materials": { "stone": { "kind": "gem", "tint": [0.2, 0.3, 0.9] } }, "view": { "tilt_deg": 10.0 } }"#,
        &[
            "F0003",
            "viewer_bad_env",
            "viewer_bad_material",
            "viewer_bad_size",
            "viewer_bad_value",
            "viewer_env_unreadable",
            "viewer_no_adapter",
            "viewer_render_failed",
            "viewer_unknown_field",
            "viewer_write_failed",
        ],
        &[],
    ),
    c(
        "viewer.env_info",
        r#"{ "path": "studio.renv" }"#,
        &["viewer_env_unreadable"],
        &[],
    ),
    c("looks.list", r#"{}"#, &[], &[]),
    c(
        "looks.material",
        r#"{ "material_id": "diamond-white" }"#,
        &[
            "looks_material_missing_field",
            "looks_material_not_drawable",
            "looks_material_unknown",
            "looks_missing_param",
        ],
        &[],
    ),
    c(
        "looks.cut",
        r#"{ "cut_type": "round_brilliant", "width_mm": 3.0 }"#,
        &["looks_bad_size", "looks_missing_param", "looks_unknown_cut"],
        &[],
    ),
    heavy(
        "looks.render_board",
        r#"{ "width": 96, "height": 60, "tilt_deg": 22.0 }"#,
        &[
            "F0003",
            "looks_bad_size",
            "looks_bad_value",
            "looks_no_adapter",
            "looks_render_failed",
            "looks_unknown_field",
            "looks_write_failed",
        ],
        &[],
    ),
    // Ambience that matches the picture (ADR-2442): F0013 without audio.
    c("audio.ambience", r#"{}"#, &["F0013"], &[]),
    c(
        "audio.set_ambience",
        r#"{ "preset": "forest", "layers": [{ "name": "wind", "sound": "amb/wind_loop" }] }"#,
        &["F0013", "ambience_unknown_drive", "ambience_unknown_preset"],
        &[
            range("calm_m_per_s", 0.0, 40.0),
            range("attack_secs", 0.0, 30.0),
            range("release_secs", 0.0, 60.0),
            range("volume_db", -60.0, 12.0),
            range("wind_override_m_per_s", -100.0, 100.0),
            items("layers", 32),
        ],
    ),
    c("audio.ambient_spots", r#"{}"#, &["F0013"], &[]),
    c(
        "audio.set_ambient_spot",
        r#"{ "position": [30, 2, -12], "preset": "birds", "sounds": ["amb/bird_a", "amb/bird_b", "amb/bird_c"] }"#,
        &[
            "F0003",
            "F0013",
            "ambience_unknown_kind",
            "ambience_unknown_preset",
        ],
        &[
            range("volume_db", -80.0, 12.0),
            range("min_distance", 0.0, 100000.0),
            range("max_distance", 0.001, 100000.0),
            range("radius_m", 0.0, 10000.0),
            range("height_m", 0.0, 1000.0),
            items("sounds", 64),
        ],
    ),
    c(
        "audio.probe_ambience",
        r#"{ "listener": [0, 1.7, 0], "position": [40, 0, 0], "preset": "water", "occlusion": 0.5 }"#,
        &[
            "F0003",
            "F0013",
            "ambience_unknown_kind",
            "ambience_unknown_preset",
        ],
        &[
            range("occlusion", 0.0, 1.0),
            range("max_distance", 0.001, 100000.0),
        ],
    ),
    c("audio.footsteps", r#"{}"#, &["F0013"], &[]),
    c(
        "audio.set_footsteps",
        r#"{ "sets": { "grass": { "sounds": ["fs/grass_1", "fs/grass_2", "fs/grass_3", "fs/grass_4"] } }, "layer_surfaces": { "Meadow": "grass" }, "default_surface": "grass" }"#,
        &["F0013"],
        &[
            range("walk_speed_m_per_s", 0.1, 20.0),
            range("reference_weight_kg", 1.0, 100000.0),
            range("max_distance", 0.1, 1000.0),
        ],
    ),
    c(
        "audio.footstep",
        r#"{ "position": [0, 0, 0], "surface": "grass", "speed_m_per_s": 3.0, "weight_kg": 90 }"#,
        &["F0003", "F0013", "footstep_no_set"],
        &[
            range("speed_m_per_s", 0.0, 100.0),
            range("weight_kg", 0.1, 100000.0),
        ],
    ),
    // Font subsets (ADR-2697): what the last cook did to the project's fonts.
    c(
        "assets.font_subset",
        r#"{ "font": "assets/fonts/ui.ttf" }"#,
        &[
            "F0016",
            "font_subset_bad_param",
            "font_subset_bad_report",
            "font_subset_no_report",
            "font_subset_unknown_font",
        ],
        &[],
    ),
    // Texture-set materials (spec 22 T2.14, ADR-3320): F0016 for a missing folder, F0003 for a
    // failed write (F0002, universal, covers a folder escaping the project).
    // The asset id manifest `.assetids.ron` (ADR-8460): ids of sources not in version control.
    c(
        "assets.ids_export",
        r#"{ "out": "asset_ids.assetids.ron" }"#,
        &[
            "asset_ids_bad_meta",
            "asset_ids_write_failed",
            "path_outside_project",
        ],
        &[],
    ),
    c(
        "assets.ids_import",
        r#"{ "file": "asset_ids.assetids.ron" }"#,
        &[
            "asset_id_conflict",
            "asset_ids_invalid",
            "asset_ids_unreadable",
            "asset_ids_write_failed",
            "path_outside_project",
        ],
        &[],
    ),
    c(
        "assets.texture_sets",
        r#"{ "folder": "assets/textures/rocks", "write": true }"#,
        &["F0003", "F0016"],
        &[],
    ),
    // Voxel far proxies (ADR-2445): F0016 for an unknown asset, no AssetServer or no cooked
    // proxy, F0013 for a proxy that does not read.
    c(
        "assets.voxel_far",
        r#"{ "asset": "assets/trees/oak.gltf#voxels/Oak/primitive_0", "viewport_height": 1440 }"#,
        &[
            "F0013",
            "F0016",
            "voxel_far_bad_param",
            "voxel_far_invalid",
            "voxel_far_not_cooked",
            "voxel_far_old_version",
            "voxel_far_unreadable",
        ],
        &[
            range("viewport_height", 1.0, 16384.0),
            range("fov_y", 0.01, 3.1),
            range("threshold_px", 0.01, 1024.0),
            bytes("asset", 4096),
        ],
    ),
    // Stochastic many-light sampling (ADR-2470): F0003 without a runtime renderer.
    c(
        "render.set_many_light_sampling",
        r#"{ "mode": "debug", "view": "estimate", "initial_candidates": 16 }"#,
        &["F0003"],
        &[
            range("initial_candidates", 1.0, 64.0),
            range("spatial_taps", 0.0, 8.0),
            range("spatial_radius_px", 1.0, 64.0),
            range("trace_steps", 0.0, 64.0),
        ],
    ),
    heavy(
        "render.many_light_stats",
        r#"{ "reference": false }"#,
        &[],
        &[
            range("lights", 1.0, 2000.0),
            range("seeds", 2.0, 64.0),
            range("frames", 1.0, 8.0),
        ],
    ),
    c(
        "ragdoll.state",
        r#"{ "entity": 4294967310 }"#,
        &["F0003", "ragdoll_not_found"],
        &[],
    ),
    c(
        "ragdoll.active_set",
        r#"{ "entity": 4294967310, "strength": { "arms": 0.5 }, "gains": [{ "joint": "spine", "kp": 300.0, "kd": 30.0 }] }"#,
        &[
            "F0003",
            "ragdoll_bad_gain",
            "ragdoll_bad_recovery",
            "ragdoll_bad_strength",
            "ragdoll_not_found",
            "ragdoll_unknown_joint",
        ],
        &[],
    ),
    c("render.ssr_stats", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_ssr_cost",
        r#"{ "probe": false, "bound": "off" }"#,
        &[],
        &[],
    ),
    c(
        "fluid.set_solids",
        r#"{ "id": 1, "solids": [{ "shape": "box", "center": [0.3, 0.1, 0.05], "half_extents": [0.03, 0.08, 0.2], "velocity": [0.5, 0, 0] }] }"#,
        &["F0003"],
        &[items("solids", 64)],
    ),
    c("fluid.solids", r#"{ "id": 1 }"#, &["F0003"], &[]),
    // What the voxel far levels add to bridged mesh entities (ADR-2655).
    c("render.voxel_far_stats", r#"{}"#, &[], &[]),
    // The net_relevance gizmo group (ADR-2665).
    c(
        "net.relevance_gizmos",
        r#"{ "limit": 10 }"#,
        &["net_relevance_gizmos_unavailable"],
        &[],
    ),
    // The shared cook cache (book/agents/cook-cache.md, ADR-2612).
    c(
        "cook.cache_stats",
        r#"{ "platform": "linux", "scan": false }"#,
        &["shared_cache_config_invalid"],
        &[bytes("platform", 32)],
    ),
    // Import folder rules (book/agents/import-rules.md, ADR-3321).
    c(
        "assets.import_rules",
        r#"{ "path": "assets/props/crate.gltf" }"#,
        &["invalid_config", "meta_settings_invalid"],
        &[],
    ),
    // Player accessibility assists (ADR-2646).
    c(
        "input.assists",
        r#"{ "set": { "Crouch": { "mode": "toggle" } } }"#,
        &[
            "input_assist_bad_params",
            "input_assist_bad_value",
            "input_assist_unknown_action",
        ],
        &[],
    ),
    c(
        "input.haptics",
        r#"{ "rumble_scale": 0.5 }"#,
        &["input_haptics_bad_params", "input_haptics_bad_scale"],
        &[range("rumble_scale", 0.0, 1.0)],
    ),
    c(
        "audio.mono",
        r#"{ "enabled": true }"#,
        &["audio_mono_bad_params"],
        &[],
    ),
    c(
        "render.color_vision",
        r#"{ "mode": "simulate", "deficiency": "deuteranopia" }"#,
        &[
            "color_vision_bad_deficiency",
            "color_vision_bad_mode",
            "color_vision_bad_params",
            "color_vision_bad_severity",
        ],
        &[range("severity", 0.0, 1.0)],
    ),
    c(
        "render.set_baked_gi_brick_probe",
        r#"{ "on": true }"#,
        &["F0003"],
        &[],
    ),
    c(
        "ui.format_preview",
        r#"{ "kind": "number", "locale": "de-DE", "value": 1234.5, "options": { "style": "currency", "currency": "EUR" } }"#,
        &["format_bad_option", "format_bad_pattern", "format_bad_value"],
        &[items("items", 256)],
    ),
    // Signed content manifests (book/agents/content-trust.md, ADR-2710).
    c(
        "content.update",
        r#"{ "base_url": "http://127.0.0.1:9/game" }"#,
        &[
            "bad_params",
            "chunk_base_mismatch",
            "chunk_in_use",
            "content_signature_invalid",
            "content_signature_missing",
            "manifest_missing",
            "source_not_supported",
            "source_unreachable",
        ],
        &[bytes("base_url", 4096)],
    ),
    c("render.frame_work", r#"{}"#, &[], &[]),
    // Trailers (book/agents/trailers.md, ADR-3281).
    c("physics.trailers", r#"{ "entity": 4294967310 }"#, &["F0003"], &[]),
    c(
        "physics.trailer_couple",
        r#"{ "trailer": 4294967310, "tow": 4294967309 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "physics.trailer_uncouple",
        r#"{ "trailer": 4294967310 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "physics.trailer_config_check",
        r#"{ "ron": "(format_version: 1, name: \"box trailer\", config: ())" }"#,
        &[],
        &[],
    ),
    // The froxel fog's sun-shadow lookup (ADR-3122): F0003 when the app has no `FogSunShadowProbe`.
    c(
        "render.fog_shadow",
        r#"{ "point": [0.0, 2.0, 0.0] }"#,
        &["F0003"],
        &[],
    ),
    // Wheel surface effects (ADR-3274).
    c(
        "vfx.vehicle_fx",
        r#"{}"#,
        &["F0016", "no_vehicle_fx", "unknown_field"],
        &[],
    ),
    c(
        "vfx.vehicle_fx_set",
        r#"{ "spacing": 0.5, "max_stamps": 64, "default_surface": "gravel" }"#,
        &[
            "F0016",
            "invalid_value",
            "no_vehicle_fx",
            "unknown_field",
            "unknown_surface",
        ],
        &[
            range("spacing", 0.05, 5.0),
            range("max_turn", 0.01, 1.0),
            range("smoke_slip_threshold", 0.01, 0.95),
            range("skid_slip_threshold", 0.01, 2.0),
            range("min_speed", 0.0, 20.0),
            range("mark_lifetime_s", 0.5, 3600.0),
            range("max_stamps", 0.0, 8192.0),
            range("effect_scale", 0.0, 2.0),
        ],
    ),
    // Networked rigid bodies (book/agents/net-bodies.md, ADR-2902): F0002 with data.reason
    // (net_bodies_not_installed, limit_out_of_range).
    c(
        "net.bodies",
        r#"{ "limit": 8 }"#,
        &[],
        &[range("limit", 0.0, 10000.0)],
    ),
    // Reusable .ui.ron components (ADR-3147).
    c("ui.components", r#"{}"#, &[], &[]),
    // Contact-scale calibration opt-ins (ADR-3202): F0003 without RenderSettings.
    c("render.get_contact_calibration", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_contact_calibration",
        r#"{ "horizon_ao": true }"#,
        &["F0003"],
        &[],
    ),
    // Fixed-wing aircraft (book/agents/aircraft.md, ADR-3374).
    c("physics.aircraft", r#"{ "entity": 4294967310 }"#, &["F0003"], &[]),
    c(
        "physics.aircraft_input",
        r#"{ "entity": 4294967310, "throttle": 1.0, "pitch": 0.2, "brakes": 0.0 }"#,
        &["F0003"],
        // Controls are clamped to their ranges by the handler; the wind is bounded here.
        &[range("wind", -1000.0, 1000.0)],
    ),
    c(
        "physics.aircraft_trim",
        r#"{ "entity": 4294967310, "speed": 50.0, "altitude": 1000.0 }"#,
        &[
            "F0003",
            "aircraft_invalid_config",
            "aircraft_no_surfaces",
            "aircraft_parse",
            "aircraft_too_many_parts",
            "aircraft_trim_failed",
            "aircraft_trim_invalid_target",
            "aircraft_trim_unreachable",
            "aircraft_version",
        ],
        &[bytes("ron", 1 << 20)],
    ),
    c(
        "physics.aircraft_config_check",
        r#"{ "ron": "(format_version: 1, name: \"trainer\", config: ())" }"#,
        &[
            "aircraft_invalid_config",
            "aircraft_no_surfaces",
            "aircraft_parse",
            "aircraft_too_many_parts",
            "aircraft_version",
        ],
        &[bytes("ron", 1 << 20)],
    ),
    c(
        "vehicle_ai.drive_to",
        r#"{ "entity": 4294967310, "lane": 2, "distance": 40.0 }"#,
        &[
            "vehicle_ai_bad_param",
            "vehicle_ai_no_chassis",
            "vehicle_ai_no_lane_graph",
            "vehicle_ai_no_route",
            "vehicle_ai_not_a_vehicle",
            "vehicle_ai_off_lanes",
            "vehicle_ai_route_too_long",
            "vehicle_ai_unknown_lane",
        ],
        &[],
    ),
    c(
        "vehicle_ai.status",
        r#"{ "entity": 4294967310 }"#,
        &["vehicle_ai_no_driver"],
        &[],
    ),
    c(
        "vehicle_ai.set_params",
        r#"{ "entity": 4294967310, "max_speed": 12.0, "time_headway": 1.5 }"#,
        &["vehicle_ai_bad_param", "vehicle_ai_not_a_vehicle"],
        &[],
    ),
    c(
        "vehicle_ai.stop",
        r#"{ "entity": 4294967310 }"#,
        &["vehicle_ai_no_driver", "vehicle_ai_not_a_vehicle"],
        &[],
    ),
    c(
        "ragdoll.push",
        r#"{ "entity": 4294967310, "impulse": [0.0, 0.0, -40.0] }"#,
        &[
            "F0003",
            "ragdoll_bad_impulse",
            "ragdoll_not_built",
            "ragdoll_not_found",
            "ragdoll_unknown_joint",
        ],
        &[],
    ),
    c(
        "ai.statetree_validate",
        r#"{ "path": "assets/ai/guard.statetree.ron" }"#,
        &[],
        &[],
    ),
    c(
        "ai.statetree_edit",
        r#"{ "path": "assets/ai/guard.statetree.ron", "create": true, "save": false, "ops": [ { "op": "add_state", "name": "Root" }, { "op": "add_state", "name": "Idle", "parent": "Root" } ] }"#,
        &[],
        &[items("ops", 512)],
    ),
    c(
        "render.save_local_captures",
        r#"{ "file": "maps/hall.fcap" }"#,
        &["F0003"],
        &[],
    ),
    c(
        "render.load_local_captures",
        r#"{ "file": "maps/hall.fcap" }"#,
        &["F0003"],
        &[],
    ),
    c("render.get_capture_bake", r#"{}"#, &["F0003"], &[]),
    c("render.restir_gi", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_restir_gi",
        r#"{ "enabled": true, "temporal": true, "spatial_taps": 4, "history_cap": 30 }"#,
        &["F0003"],
        &[range("spatial_taps", 0.0, 8.0), range("history_cap", 1.0, 500.0)],
    ),
    // Secondary rigid debris spawn cap (book/agents/destruction-debris.md, ADR-3929).
    c(
        "physics.debris_budget",
        r#"{ "per_break": 4, "per_frame": 6, "reset_frame": false, "break_id": 1, "count": 3 }"#,
        &["F0003", "debris_budget_bad_cap"],
        &[],
    ),
    // Vehicle audio (book/agents/vehicle-audio.md, ADR-3271).
    c("audio.vehicle_audio", r#"{}"#, &[], &[]),
    c(
        "audio.vehicle_audio_set",
        r#"{ "entity": 4294967297, "preset": "placeholder", "budget": { "full_vehicles": 3 } }"#,
        &[
            "F0003",
            "vehicle_audio_no_plugin",
            "vehicle_audio_not_vehicle",
            "vehicle_audio_parse",
            "vehicle_audio_unknown_preset",
            "vehicle_audio_value",
            "vehicle_audio_version",
        ],
        &[bytes("ron", 262_144)],
    ),
    c(
        "audio.vehicle_audio_probe",
        r#"{ "rpm": 4200, "load": 0.8, "slip_angle_deg": 9, "speed": 20, "surface": "gravel" }"#,
        &[
            "vehicle_audio_parse",
            "vehicle_audio_unknown_preset",
            "vehicle_audio_value",
            "vehicle_audio_version",
        ],
        &[
            range("rpm", 0.0, 30000.0),
            range("load", 0.0, 1.0),
            range("throttle", 0.0, 1.0),
            range("slip_angle_deg", -90.0, 90.0),
            range("slip_ratio", -10.0, 10.0),
            range("speed", 0.0, 1000.0),
            bytes("ron", 262_144),
        ],
    ),
    // Still-cell sleep (ADR-3949). The length check is fluid_sleep_bad_grid.
    c(
        "fluid.sleep_mask",
        r#"{ "nx": 2, "ny": 2, "nz": 2, "k": 3, "epsilon": 0.001, "speeds": [0, 0, 0, 0, 0, 0, 0, 0] }"#,
        &["fluid_sleep_bad_grid"],
        &[
            range("nx", 1.0, 4096.0),
            range("ny", 1.0, 4096.0),
            range("nz", 1.0, 4096.0),
            range("k", 0.0, 1_000_000.0),
            range("epsilon", 0.0, 1_000_000.0),
            items("speeds", 262_144),
            items("still", 262_144),
        ],
    ),
    // Two-wheel balance (book/agents/bike.md, ADR-4017). No range limits: a non-finite steer is
    // the handler's `bike_bad_input`, not the gate's F0002.
    c(
        "physics.bike_step",
        r#"{ "dt": 0.016666668, "steer": 0.08, "speed": 8.0, "drive": 0.0, "front_ray": 0.3, "rear_ray": 0.3 }"#,
        &["bike_bad_input"],
        &[],
    ),
    c("physics.bike_state", r#"{}"#, &[], &[]),
    // Start-up work and the idle ledger (ADR-2987).
    c("app.startup_work", r#"{}"#, &[], &[]),
    c(
        "memory.idle_ledger",
        r#"{ "frames": 120 }"#,
        &["idle_ledger_not_installed", "idle_ledger_running"],
        &[range("frames", 1.0, 100_000.0)],
    ),
    // Host battery and AC supply (book/agents/platform-power.md, ADR-3997).
    c(
        "platform.power_state",
        r#"{}"#,
        &["power_unavailable"],
        &[bytes("dir", 4096)],
    ),
    // Primary window and virtual file system (book/agents/window-and-vfs.md, ADR-7232).
    c("window.get", r#"{}"#, &["window_none"], &[]),
    c(
        "window.set",
        r#"{ "title": "Arena", "width": 1280, "height": 720 }"#,
        &["window_none", "window_size_invalid", "window_value_unknown"],
        &[bytes("title", 4096)],
    ),
    c("vfs.mounts", r#"{}"#, &["vfs_none"], &[]),
    c(
        "vfs.exists",
        r#"{ "path": "assets://config/net.ron" }"#,
        &["vfs_none", "vfs_path_invalid"],
        &[bytes("path", 4096)],
    ),
    c(
        "vfs.list",
        r#"{ "path": "assets://" }"#,
        &[
            "vfs_io",
            "vfs_none",
            "vfs_not_found",
            "vfs_not_mounted",
            "vfs_path_invalid",
        ],
        &[bytes("path", 4096)],
    ),
    c(
        "vfs.read",
        r#"{ "path": "assets://config/net.ron", "max_bytes": 4096 }"#,
        &[
            "vfs_io",
            "vfs_none",
            "vfs_not_found",
            "vfs_not_mounted",
            "vfs_path_invalid",
        ],
        &[bytes("path", 4096)],
    ),
    c(
        "vfs.mount_pak",
        r#"{ "mount": "assets", "pak": "content/base.fpak" }"#,
        &["vfs_none", "vfs_pak_open", "vfs_path_invalid", "vfs_unsupported"],
        &[bytes("pak", 4096)],
    ),
    // Assets pushed into a launched game (book/agents/asset-push.md, ADR-7836). `data` is base64 of
    // at most 16 MiB: (16 MiB / 3 + 1) * 4 characters.
    c(
        "assets.push",
        r#"{ "asset": "0d7c1f6e-2b9a-4c3e-8f51-6a2d9e4b7c10", "data": "eA==" }"#,
        &[
            "push_bad_data",
            "push_bad_path",
            "push_io",
            "push_no_server",
            "push_no_target",
            "push_shader_invalid",
            "push_too_large",
            "push_unknown_asset",
        ],
        &[
            bytes("data", 22_369_624),
            bytes("asset", 4096),
            bytes("source", 4096),
            bytes("shader", 128),
        ],
    ),
    c(
        "renderer.minspec_explain",
        r#"{ "profile": "pascal" }"#,
        &["minspec_profile_unknown"],
        &[],
    ),
    // A walker's band and support over a cooked world geometry (book/agents/world-geometry-queries.md, ADR-8323).
    heavy(
        "geometry.body_probe",
        r#"{ "cooked": "maps/demo.meshlayers", "feet": [0.0, 0.0, 0.0], "radius": 0.35, "height": 1.85, "step_up": 0.55, "step_down": 0.55 }"#,
        &[
            "body_probe_bad_params",
            "body_probe_bad_point",
            "body_probe_corrupt",
            "body_probe_unreadable",
        ],
        &[bytes("cooked", 4096)],
    ),
    // QUIC datagram counters (book/agents/net-quic.md, ADR-3970).
    c("net.quic_status", r#"{}"#, &["quic_bad_params"], &[]),
    // Relay allocations, caps and refusals (book/agents/net-relay.md, ADR-8183).
    c(
        "net.relay_status",
        r#"{}"#,
        &["relay_bad_params", "relay_unknown"],
        &[],
    ),
    // Port mapping, port-forward self-check and capacity (book/agents/net-host-check.md, ADR-8370).
    c(
        "net.host_check",
        r#"{}"#,
        &["host_check_bad_params", "host_check_unknown"],
        &[],
    ),
    // The self-hosted lobby hub (book/agents/net-lobby-hub.md, ADR-8559).
    c(
        "lobby.hub_status",
        r#"{}"#,
        &["hub_bad_params", "hub_unknown"],
        &[],
    ),
    // Join brokering and player identity on the hub (book/agents/net-lobby-hub-join.md, ADR-8607).
    c(
        "lobby.hub_join_status",
        r#"{}"#,
        &["hub_bad_params", "hub_unknown"],
        &[],
    ),
    // Photosensitivity check (book/agents/seeing.md, ADR-3804). fps and frames stay out of
    // Limit::Range so a bad value keeps its own code instead of the universal F0002.
    heavy(
        "seeing.flash_check",
        r#"{ "dir": "frames", "fps": 30 }"#,
        &[
            "F0003",
            "F0011",
            "F0015",
            "bad_png",
            "dir_not_found",
            "empty_frame",
            "fps_out_of_range",
            "frame_size_mismatch",
            "frame_too_large",
            "frames_out_of_range",
            "geometry_out_of_range",
            "no_frames",
            "time_not_monotonic",
            "too_many_frames",
            "too_many_pending",
        ],
        &[bytes("dir", 4096)],
    ),
    // Cluster residency (ADR-3118): F0003 without a runtime renderer.
    c("render.get_visbuffer_residency", r#"{}"#, &[], &[]),
    c(
        "render.set_visbuffer_residency",
        r#"{ "tier": "deck", "upload_bytes_per_frame": 4194304 }"#,
        &["F0003"],
        &[bytes("tier", 64)],
    ),
    c(
        "seeing.capture_plate",
        r#"{ "shot": { "name": "wide_noon", "camera": { "eye": [0, 2, 8], "target": [0, 1, 0] }, "time_of_day": { "hour": 12 }, "width": 1280, "height": 720 }, "out": "plates" }"#,
        &["F0003", "F0011", "F0018"],
        &[],
    ),
    c(
        "render.set_reference_overlay",
        r#"{ "mode": "wipe", "path": "plates/reference.png", "split": 0.5 }"#,
        &["F0003", "F0011"],
        &[],
    ),
    // WAV cue points stored in the file (book/agents/audio.md, ADR-4632).
    c(
        "audio.cues",
        r#"{ "path": "audio/marks.wav" }"#,
        &[
            "F0003",
            "wav_cue_not_riff",
            "wav_cue_not_wave",
            "wav_cue_truncated",
            "wav_cue_unreadable",
        ],
        &[bytes("path", 4096)],
    ),
    // Marker sheet CSV (ADR-4597). `.seq.ron` version unchanged.
    c(
        "sequencer.marker_sheet",
        r#"{ "op": "export" }"#,
        &[
            "bad_entity",
            "bad_setting",
            "no_player",
            "seq_marker_csv",
            "several_players",
            "unknown_player",
        ],
        &[],
    ),
    // The aerial perspective's interior volumes (ADR-4757): stable lowercase errors when the host is missing.
    c(
        "render.aerial_interiors",
        r#"{ "direction": [1.0, 0.0, 0.0], "distance": 100.0 }"#,
        &["aerial_frame_missing", "invalid_aerial_ray"],
        &[],
    ),
    c(
        "render.set_aerial_interiors",
        r#"{ "enabled": false }"#,
        &["aerial_renderer_missing", "invalid_aerial_enabled"],
        &[],
    ),
    // The direct BRDF's energy model (ADR-2632): process-wide, no resource needed.
    c("render.brdf_energy", r#"{}"#, &[], &[]),
    c(
        "render.set_brdf_energy",
        r#"{ "mode": "legacy" }"#,
        &["brdf_energy_bad_mode"],
        &[],
    ),
    // The frame's shading normal and roughness AOVs (ADR-2951): two extra frames.
    heavy(
        "seeing.frame_aov",
        r#"{ "width": 640, "height": 360, "aov": "normal" }"#,
        &[
            "frame_aov_no_device",
            "frame_aov_no_frame",
            "frame_aov_render_failed",
        ],
        &[range("width", 16.0, 4096.0), range("height", 16.0, 4096.0)],
    ),
    // The MaterialX importer's report (book/agents/materialx-import.md, ADR-3341).
    c(
        "import.materialx_report",
        r#"{ "path": "assets/materials/wood.mtlx" }"#,
        &[
            "materialx_importer_unavailable",
            "mtlx_cycle",
            "mtlx_io",
            "mtlx_limit",
            "mtlx_no_material",
            "mtlx_not_materialx",
            "mtlx_not_utf8",
            "mtlx_parse",
            "mtlx_settings",
            "mtlx_structure",
            "mtlx_too_large",
            "mtlx_unsupported_shader",
            "mtlx_version",
            "mtlx_wrong_extension",
        ],
        &[bytes("path", 1024)],
    ),
    // Pak update diffs (book/agents/content-chunks.md § Shipping updates, spec 22 T2.14, ADR-3322).
    heavy(
        "assets.pak_diff",
        r#"{ "old": "dist/linux/old/content", "new": "dist/linux/new/content" }"#,
        &[
            "pak_diff_invalid_chunk_size",
            "pak_diff_no_paks",
            "pak_diff_report",
            "pak_diff_unreadable",
        ],
        &[bytes("old", 4096), bytes("new", 4096)],
    ),
    // Contact sounds from physics (ADR-3270).
    c("audio.impacts", r#"{}"#, &["F0013"], &[]),
    c(
        "audio.impact_probe",
        r#"{ "a": "wood", "b": "concrete", "impulse_ns": 4.0, "normal_speed_m_s": 3.0 }"#,
        &[
            "impactbank_name",
            "impactbank_parse",
            "impactbank_sounds",
            "impactbank_value",
            "impactbank_version",
            "impacts_bad_param",
            "impacts_no_bank",
            "impacts_no_set",
        ],
        &[
            range("impulse_ns", 0.0, 1.0e6),
            range("normal_speed_m_s", 0.0, 1.0e4),
            range("slip_m_s", 0.0, 1.0e4),
            range("rolling_speed_m_s", 0.0, 1.0e4),
            range("normal_force_n", 0.0, 1.0e9),
        ],
    ),
    c(
        "audio.impacts_set",
        r#"{ "ron": "(format_version: 1, materials: { \"wood\": (impacts: [\"wood_1\", \"wood_2\"]) })", "seed": 7 }"#,
        &[
            "F0013",
            "impactbank_name",
            "impactbank_parse",
            "impactbank_sounds",
            "impactbank_value",
            "impactbank_version",
            "impacts_bad_param",
            "impacts_no_bank",
        ],
        &[],
    ),
    // Gameplay force fields (ADR-3348): F0003 for a missing entity; refusals are lowercase `reason`s in the
    // result (book/agents/physics-fields.md), like physics.time_of_impact.
    c("physics.fields", r#"{}"#, &["F0003"], &[]),
    c(
        "physics.field_set",
        r#"{ "kind": { "type": "directional", "vector": [0, 20, 0] }, "volume": { "type": "box", "half_extents": [2, 4, 2] }, "position": [0, 4, 0] }"#,
        &["F0003"],
        &[],
    ),
    c(
        "physics.field_probe",
        r#"{ "point": [1, 0, 0], "mass": 2 }"#,
        &[],
        &[],
    ),
    c(
        "render.vis_lod_hysteresis",
        r#"{}"#,
        &[
            "vis_lod_hysteresis_bad_band",
            "vis_lod_hysteresis_bad_params",
            "vis_lod_hysteresis_no_renderer",
        ],
        &[],
    ),
    // Sight acquire and lose cones (book/agents/perception-hysteresis.md, ADR-4586).
    c(
        "ai.perception_hysteresis",
        r#"{ "entity": 4294967296, "sight_lose_radius": 14.0 }"#,
        &["F0003", "ai_perception_unavailable", "perception_bad_hysteresis"],
        &[],
    ),
    // Cooked terrain tile counts and bcn/astc (ADR-5294).
    c(
        "assets.terrain_tiles",
        r#"{ "asset": "assets/textures/terrain.layers.ron" }"#,
        &[
            "F0016",
            "terrain_tiles_bad_index",
            "terrain_tiles_bad_param",
            "terrain_tiles_missing",
            "terrain_tiles_not_terrain",
            "terrain_tiles_unreadable",
        ],
        &[bytes("asset", 4096)],
    ),
    c(
        "sequence.record_stop",
        r#"{ "sequence": "cinematics/intro.seq.ron", "at": 0.0 }"#,
        &[
            "bad_setting",
            "take_bad_path",
            "take_empty",
            "take_invalid",
            "take_io",
            "take_not_recording",
            "take_numbers_exhausted",
            "take_source_invalid",
        ],
        &[],
    ),
    c("sequence.record_state", r#"{}"#, &[], &[]),
    // The cutscene flow (ADR-2948).
    c(
        "cutscene.start",
        r#"{ "letterbox": 0.1, "skip": { "hold_secs": 0.5, "fade_secs": 0.3 } }"#,
        &[
            "bad_entity",
            "cutscene_bad_settings",
            "cutscene_no_player",
            "cutscene_running",
            "cutscene_several",
        ],
        &[],
    ),
    c(
        "cutscene.skip",
        r#"{}"#,
        &[
            "bad_entity",
            "cutscene_not_running",
            "cutscene_several",
            "cutscene_skip_disabled",
        ],
        &[],
    ),
    c(
        "cutscene.stop",
        r#"{}"#,
        &["bad_entity", "cutscene_not_running", "cutscene_several"],
        &[],
    ),
    c(
        "cutscene.state",
        r#"{}"#,
        &["bad_entity", "cutscene_no_player", "cutscene_several"],
        &[],
    ),
    // The network recorder and replay player (ADR-3021).
    c(
        "net.replay_record_start",
        r#"{ "path": "match.netdemo", "ring_seconds": 30, "ring_bytes": 16777216, "view": "everything", "keyframe_s": 5, "max_bytes": 1073741824, "max_seconds": 3600, "level": "arena", "overwrite": false }"#,
        &[
            "net_not_running",
            "net_not_server",
            "netdemo_exists",
            "netdemo_io",
            "recorder_busy",
            "recorder_config_invalid",
            "recorder_view_conflict",
            "recorder_view_invalid",
        ],
        &[],
    ),
    c(
        "net.replay_record_stop",
        r#"{ "target": "both" }"#,
        &[
            "net_not_running",
            "netdemo_exists",
            "netdemo_io",
            "recorder_idle",
            "ring_idle",
        ],
        &[],
    ),
    c(
        "net.replay_ring_export",
        r#"{ "path": "killcam.netdemo", "seconds": 10, "overwrite": false }"#,
        &[
            "net_not_running",
            "netdemo_exists",
            "netdemo_io",
            "recorder_config_invalid",
            "ring_empty",
            "ring_idle",
        ],
        &[],
    ),
    c(
        "net.replay_open",
        r#"{ "path": "match.netdemo", "paused": false, "interpolate": true }"#,
        &[
            "net_not_running",
            "netdemo_corrupt",
            "netdemo_io",
            "netdemo_not_a_demo",
            "netdemo_protocol_mismatch",
            "netdemo_session_active",
            "netdemo_truncated",
            "netdemo_version",
        ],
        &[],
    ),
    c(
        "net.replay_seek",
        r#"{ "tick": 600 }"#,
        &["net_not_running", "replay_closed", "replay_seek_range"],
        &[],
    ),
    c(
        "net.replay_play",
        r#"{ "playing": true, "speed": 2.0, "follow": 12 }"#,
        &["net_not_running", "replay_closed"],
        &[],
    ),
    c(
        "net.replay_close",
        r#"{}"#,
        &["net_not_running", "replay_closed"],
        &[],
    ),
    c("net.replay_state", r#"{}"#, &["net_not_running"], &[]),
    // The camera body and the startled bird (ADR-1502): F0003 without the host's plugin or a
    // loaded camera path.
    c("camera_body.get", r#"{}"#, &["F0003"], &[]),
    c(
        "camera_body.set",
        r#"{ "spec": { "wake": { "gain": 0.15 } } }"#,
        &[],
        &[],
    ),
    c(
        "camera_body.wake",
        r#"{ "points": [[0.0, 0.5, 2.0]] }"#,
        &["F0003"],
        &[],
    ),
    c(
        "camera_body.proximity",
        r#"{ "radius_m": 20.0 }"#,
        &["F0003"],
        &[],
    ),
    c(
        "camera_body.clearance",
        r#"{ "clearance_m": 5.0 }"#,
        &["F0003"],
        &[],
    ),
    c("camera_body.lift_path", r#"{ "dry_run": true }"#, &["F0003"], &[]),
    c(
        "bird.spawn",
        r#"{ "spec": { "name": "crow", "position": [0.0, 0.0, 0.0], "species": "crow" } }"#,
        &["F0003"],
        &[],
    ),
    c("bird.list", r#"{}"#, &["F0003"], &[]),
    c("bird.remove", r#"{ "name": "crow" }"#, &["F0003"], &[]),
    c(
        "bird.trigger",
        r#"{ "spec": { "name": "crow", "position": [0.0, 0.0, 0.0] } }"#,
        &["F0003"],
        &[],
    ),
    c(
        "bird.place_on_path",
        r#"{ "time_s": 2.0, "right_m": 4.0 }"#,
        &["F0003"],
        &[],
    ),
    // The sun's specular as a disc (ADR-3190): the render profile's field.
    c("render.sun_specular_disc", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_sun_specular_disc",
        r#"{ "enabled": false }"#,
        &["F0003", "sun_specular_disc_bad_enabled"],
        &[],
    ),
    // Forest flythrough (ADR-1503): F0003 without a ForestFlight (or its query, or an unknown class).
    c(
        "terrain.forest_query",
        r#"{ "box_min": [-20, 0, -20], "box_max": [20, 200, 20] }"#,
        &["F0003"],
        &[
            range("box_min", -1000000.0, 1000000.0),
            range("box_max", -1000000.0, 1000000.0),
            range("from", -1000000.0, 1000000.0),
            range("to", -1000000.0, 1000000.0),
            range("radius", 0.0, 1000.0),
            range("limit", 1.0, 20000.0),
            items("classes", 64),
        ],
    ),
    heavy(
        "terrain.forest_corridor",
        r#"{ "points": [[-100, 60, 0], [100, 60, 0]], "clearance_m": 2.0 }"#,
        &["F0003"],
        &[
            range("points", -1000000.0, 1000000.0),
            items("points", 20000),
            items("times", 20000),
            range("clearance_m", 0.0, 200.0),
            range("ground_clearance_m", 0.0, 200.0),
            range("search_m", 0.0, 200.0),
            items("classes", 64),
        ],
    ),
    heavy(
        "terrain.forest_carve",
        r#"{ "points": [[-100, 60, 0], [100, 60, 0]], "clearance_m": 2.0 }"#,
        &["F0003"],
        &[
            range("points", -1000000.0, 1000000.0),
            items("points", 20000),
            items("times", 20000),
            range("clearance_m", 0.0, 200.0),
            range("ground_clearance_m", 0.0, 200.0),
            range("search_m", 0.0, 200.0),
            items("classes", 64),
        ],
    ),
    c(
        "terrain.forest_flight",
        r#"{ "speed_m_s": 100.0 }"#,
        &["F0003"],
        &[
            range("speed_m_s", 0.0, 10000.0),
            range("fps", 1.0, 1000.0),
            range("height_px", 1.0, 100000.0),
            range("fov_deg", 1.0, 170.0),
        ],
    ),
    c(
        "terrain.set_forest_flight",
        r#"{ "motion": true, "sway": true }"#,
        &["F0003"],
        &[range("fps", 1.0, 1000.0), range("pop_threshold_px", 0.0, 10000.0)],
    ),
    // Quarter-resolution screen-space GI gather (book/agents/dynamic-gi.md, ADR-3964).
    // The example clears the override so a coverage call cannot leave quarter on.
    c("render.get_ssgi_gather", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_ssgi_gather",
        r#"{ "step": 2 }"#,
        &["F0003", "ssgi_gather_bad_step"],
        &[],
    ),
    // Rect and tube stand-ins on tiers with no emitter budget (ADR-3247): the render profile's field.
    c("render.area_light_fallback", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_area_light_fallback",
        r#"{ "enabled": false }"#,
        &["F0003", "area_light_fallback_bad_enabled"],
        &[],
    ),
    // Every sized light through the LTC tables (ADR-3247): the render profile's field.
    c("render.sized_light_ltc", r#"{}"#, &["F0003"], &[]),
    c(
        "render.set_sized_light_ltc",
        r#"{ "enabled": false }"#,
        &["F0003", "sized_light_ltc_bad_enabled"],
        &[],
    ),
    // The host memory budget override (ADR-7891).
    c(
        "render.set_host_budget",
        r#"{ "cpu_mesh_bytes": 67108864 }"#,
        &["host_budget_bad_params"],
        &[],
    ),
    // One past the cap (1_048_577) reaches the handler so it can answer
    // procgen_fingerprint_overflow. A longer list is refused here and is not mixed.
    heavy(
        "procgen.fingerprint",
        r#"{ "records": [ { "node": 1, "instance": 0, "position_mm": [1000, 0, -250], "yaw_mdeg": 90000 } ] }"#,
        &["procgen_fingerprint_bad_params", "procgen_fingerprint_overflow"],
        &[items("records", 1_048_577)],
    ),
    // Convolution, time stretch and ambience files (book/agents/audio-convolution.md, ADR-2721).
    c(
        "audio.bus_effects",
        r#"{ "bus": "Reverb" }"#,
        &["F0003", "F0013"],
        &[bytes("bus", 256)],
    ),
    c(
        "audio.bus_effect_set",
        r#"{ "bus": "Reverb", "index": 0, "ir": "hall", "ir_path": "audio/ir/hall.wav", "predelay_secs": 0.02, "fade_secs": 0.25 }"#,
        &[
            "F0003",
            "F0013",
            "convolver_busy",
            "ir_channels",
            "ir_empty",
            "ir_non_finite",
            "ir_sample_rate",
            "ir_silent",
            "ir_tier_disabled",
            "ir_too_long",
            "ir_truncated",
            "ir_unreadable",
        ],
        &[
            bytes("bus", 256),
            bytes("ir", 256),
            bytes("ir_path", 4096),
            bytes("tier", 16),
            range("index", 0.0, 64.0),
            range("predelay_secs", 0.0, 0.5),
            range("fade_secs", 0.0, 10.0),
            range("mix", 0.0, 1.0),
        ],
    ),
    c("audio.convolution_stats", r#"{}"#, &["F0013"], &[]),
    c(
        "audio.ambience_play",
        r#"{ "path": "audio/glade.ambience.ron", "seed": 7 }"#,
        &[
            "F0003",
            "F0013",
            "ambience_invalid",
            "ambience_parse",
            "ambience_unreadable",
            "ambience_version",
        ],
        &[bytes("path", 4096), bytes("text", 1_048_576)],
    ),
    c(
        "audio.ambience_stop",
        r#"{ "name": "glade" }"#,
        &["F0013", "ambience_not_playing"],
        &[bytes("name", 256)],
    ),
    c("audio.ambience_list", r#"{}"#, &["F0013"], &[]),
    heavy(
        "audio.time_stretch_probe",
        r#"{ "rate": 0.75, "hz": 440.0, "secs": 1.0 }"#,
        &["F0013", "stretch_rate", "stretch_voice_not_found"],
        &[
            range("rate", 0.0, 8.0),
            range("hz", 20.0, 8000.0),
            range("secs", 0.1, 4.0),
        ],
    ),
    // Video sections (ADR-2862).
    c(
        "sequencer.video_sections",
        r#"{}"#,
        &["bad_entity", "no_player", "several_players", "unknown_player"],
        &[],
    ),
    c("render.ambient_life", r#"{}"#, &[], &[]),
    c(
        "render.set_ambient_life",
        r#"{ "enabled": true, "season": "summer", "density_scale": 1.0 }"#,
        &[],
        &[range("density_scale", 0.0, 4.0)],
    ),
    c(
        "render.set_ambient_birds",
        r#"{ "position": [0.0, 0.0, -40.0], "preset": "flock", "call_sounds": ["bird_call"] }"#,
        &[],
        &[
            range("position", -10000000.0, 10000000.0),
            range("count", 1.0, 64.0),
            range("radius_m", 2.0, 2000.0),
            range("height_m", 1.0, 2000.0),
            range("wingspan_m", 0.05, 4.0),
            range("speed_m_per_s", 1.0, 40.0),
            items("call_sounds", 16),
        ],
    ),
    // Loaded stream cells for one peer (book/agents/stream-cell-relevance.md, ADR-4011).
    // Universal F0002 carries data.reason: net_not_server, peer_unknown,
    // stream_cell_filter_off or stream_not_enabled.
    c("stream.cell_relevance", r#"{ "peer": 1 }"#, &[], &[]),
    // Alpha-coverage mips and channel-pack recipes (ADR-3090).
    c(
        "assets.texture_coverage",
        r#"{ "asset": "assets/trees/leaves.png", "cutoff": 0.5 }"#,
        &[
            "F0016",
            "texture_coverage_bad_param",
            "texture_coverage_not_texture",
            "texture_coverage_undecodable",
        ],
        &[],
    ),
    c(
        "assets.texpack_dry_run",
        r#"{ "path": "assets/rocks/rock_orm.texpack.ron" }"#,
        &["texpack_dry_run_bad_param"],
        &[],
    ),
    // Skid-steer tracks (book/agents/vehicle-track.md, ADR-4629).
    c(
        "physics.track_step",
        r#"{ "dt": 0.015625, "steps": 1, "left_throttle": 1.0, "right_throttle": 1.0 }"#,
        &["track_bad_input"],
        &[range("dt", 1e-6, 1.0), range("steps", 1.0, 10_000.0)],
    ),
    c(
        "physics.track_state",
        r#"{ "left_speed": 4.0, "right_speed": 0.0 }"#,
        &["track_bad_input"],
        &[],
    ),
    // Scene-wide VFX significance (ADR-3275).
    c(
        "vfx.significance",
        r#"{ "limit": 64 }"#,
        &["F0016"],
        &[range("limit", 1.0, 1000.0)],
    ),
    c(
        "vfx.significance_set",
        r#"{ "tier": "Low", "priority": { "system": "smoke", "value": 4.0 } }"#,
        &["F0003", "F0016"],
        &[],
    ), // Scatter gameplay colliders (ADR-8029).
    heavy(
        "terrain.scatter_colliders",
        r#"{ "path": "maps/arena.scatter.ron", "anchor": [0.0, 0.0], "default_extent": { "footprint": [1.0, 1.0], "height": 1.5 } }"#,
        &[
            "F0003",
            "scatter_colliders_extent",
            "scatter_colliders_place",
            "scatter_rules_invalid",
            "scatter_rules_read",
        ],
        &[bytes("path", 1024), range("limit", 0.0, 4096.0)],
    ),
    c(
        "input.glyph",
        r#"{ "action": "Jump", "device": "xbox" }"#,
        &["input_glyph_bad_params", "input_glyph_unknown"],
        &[],
    ),
    // Parametric audio patches (book/agents/audio-patch.md, ADR-7226).
    c("audio.patch_state", r#"{}"#, &["F0013"], &[]),
    c(
        "audio.patch_trigger",
        r#"{ "id": 1, "layer": "horn" }"#,
        &["F0013", "patch_not_one_shot", "patch_unknown"],
        &[range("id", 0.0, 9_007_199_254_740_991.0), bytes("layer", 256)],
    ),
    // What the last frame drew on screen (ADR-7200): F0003 when the camera entity is not a camera
    // (a bad limit or camera id is the universal F0002).
    c("render.visible_entities", r#"{ "limit": 16 }"#, &["F0003"], &[]),
    heavy(
        "gen.mesh",
        r#"{ "recipe": { "shape": { "box": { "size": [2.0, 1.0, 1.0], "bevel": { "width": 0.05, "segments": 2 } } }, "ops": [ { "uv_box": { "scale": 0.5 } } ] } }"#,
        &[
            "gen_bevel_too_wide",
            "gen_cap_not_closed",
            "gen_invalid_input",
            "gen_invalid_recipe",
            "gen_too_large",
        ],
        &[],
    ),
    heavy(
        "gen.contour",
        r#"{ "grid": { "origin": [0.0, 0.0], "cell": 1.0, "width": 1, "height": 1, "samples": [0.0, 0.0, 1.0, 1.0] }, "levels": [0.5] }"#,
        &["gen_invalid_input", "gen_invalid_recipe", "gen_too_large"],
        &[],
    ),
    // Indoor cells and portals (ADR-7586, book/agents/viscells.md): the write refuses a bad
    // viscells.ron with its code; F0003 when debug_draw has no renderer.
    c("render.viscells", r#"{ "geometry": false }"#, &[], &[]),
    c(
        "render.set_viscells",
        r#"{ "cull": true, "debug_draw": false }"#,
        &[
            "F0003",
            "viscells_bad_box",
            "viscells_cell_no_bounds",
            "viscells_duplicate_cell",
            "viscells_outdoor_bounds",
            "viscells_outdoor_not_zero",
            "viscells_parse",
            "viscells_portal_not_convex",
            "viscells_portal_not_finite",
            "viscells_portal_not_planar",
            "viscells_portal_not_quad",
            "viscells_self_portal",
            "viscells_too_many",
            "viscells_unknown_cell",
            "viscells_version_newer",
            "viscells_version_zero",
        ],
        &[bytes("text", 4194304)],
    ),
    heavy(
        "gen.texture",
        r#"{ "recipe": { "width": 256, "height": 256, "source": { "noise": { "kind": "gradient", "seed": 7, "cells": 4, "octaves": 5, "tile": true } }, "ops": [ { "levels": { "in_low": 0.3, "in_high": 0.7 } } ], "format": "r16" } }"#,
        &[
            "gen_bevel_too_wide",
            "gen_cap_not_closed",
            "gen_invalid_input",
            "gen_invalid_recipe",
            "gen_too_large",
        ],
        &[],
    ),
    heavy(
        "gen.heightfield",
        r#"{ "recipe": { "width": 129, "height": 129, "spacing": 2.0, "ops": [ { "noise": { "noise": { "kind": "gradient", "seed": 1, "octaves": 5 }, "amplitude": 40 } }, { "erode": { "iterations": 20, "talus": 0.7 } } ] } }"#,
        &["gen_invalid_input", "gen_invalid_recipe", "gen_too_large"],
        &[],
    ),
    c(
        "ui.inspect_events",
        r#"{ "node": 4294967310 }"#,
        &[
            "ui_bad_param",
            "ui_node_ambiguous",
            "ui_node_not_found",
            "ui_node_required",
        ],
        &[],
    ),
    c(
        "ui.set_live_style",
        r#"{ "node": 4294967310, "field": "style.flex_grow", "value": 2.0 }"#,
        &[
            "ui_bad_param",
            "ui_live_field",
            "ui_live_value",
            "ui_node_ambiguous",
            "ui_node_not_found",
            "ui_node_required",
        ],
        &[
            Limit::Bytes {
                param: "field",
                max: 128,
            },
            Limit::Bytes {
                param: "value",
                max: 65_536,
            },
        ],
    ),
    // The editor's Game tabs (ADR-7831): a frame from the renderer or a FrameFeed, and mute.
    c(
        "play.frame",
        r#"{ "max_width": 640, "max_height": 360 }"#,
        &["play_frame_no_source"],
        &[
            range("max_width", 16.0, 3840.0),
            range("max_height", 16.0, 3840.0),
        ],
    ),
    c("play.mute", r#"{ "muted": true }"#, &[], &[]),
    c(
        "nav.raycast",
        r#"{ "from": [-5, 0, 0], "to": [5, 0, 0], "extents": [2, 2, 2] }"#,
        &["F0003"],
        &[],
    ),
    c(
        "nav.random_point",
        r#"{ "seed": 7, "count": 16, "center": [0, 0, 0], "radius": 5, "reachable_only": true }"#,
        &["F0003"],
        &[range("count", 1.0, 4096.0)],
    ),
    c(
        "nav.crowd_set_stuck",
        r#"{ "window_steps": 60, "min_progress_m": 0.5 }"#,
        &["F0003"],
        &[range("window_steps", 0.0, 4_294_967_295.0)],
    ),
    // Match rules core (ADR-8102, book/agents/match-rules.md): `match_rules_unavailable` without the
    // MatchRules resource, the rules' own `match_*` codes for a refused input or objective, F0002 for
    // a malformed parameter.
    c("rules.match_get", r#"{}"#, &["match_rules_unavailable"], &[]),
    c(
        "rules.match_setup",
        r#"{ "settings": { "flow": { "start_delay_ticks": 180 }, "objectives": { "capture_limit": 3 } }, "playlist": [{ "map_id": "harbor", "mode": "ctf" }], "between": true }"#,
        // The ruleset_* codes come from the `ruleset` parameter (ADR-8694).
        &[
            "match_phase_settings",
            "match_race_settings",
            "match_shrink_zone_settings",
            "match_victory_settings",
            "match_wave_settings",
            "ruleset_cycle",
            "ruleset_depth",
            "ruleset_invalid",
            "ruleset_option_kind",
            "ruleset_option_range",
            "ruleset_option_unknown",
            "ruleset_unknown",
        ],
        &[items("playlist", 1024), range("no_repeat_last", 0.0, 255.0)],
    ),
    c(
        "rules.match_input",
        r#"{ "input": { "input": "ready", "team": 1, "slot": 0 } }"#,
        &[
            "match_already_paused",
            "match_join_queue_full",
            "match_join_team",
            "match_live",
            "match_no_join_ask",
            "match_not_between",
            "match_not_paused",
            "match_not_permitted",
            "match_pause_budget",
            "match_ready_to_start",
            "match_rules_unavailable",
            "match_seat_bad",
            "match_seat_limit",
            "match_seat_missing",
            "match_surrender_cooldown",
            "match_surrender_off",
            "match_unpause_held",
        ],
        &[items("inputs", 1024)],
    ),
    c(
        "rules.match_objective",
        r#"{ "place_flags": { "teams": [1, 2], "offset": 120.0 } }"#,
        &[
            "match_flag_exists",
            "match_flag_team",
            "match_hill_exists",
            "match_objective_cap",
            "match_objective_id",
            "match_rules_unavailable",
            "match_zone_settings",
            "match_zone_unknown",
        ],
        &[],
    ),
    c(
        "rules.match_step",
        r#"{ "steps": 60, "units": [{ "id": 7, "team": 1, "position": [0.0, 0.0, 0.0] }] }"#,
        &["match_rules_unavailable"],
        &[
            range("steps", 1.0, 100000.0),
            items("units", 4096),
            items("return_zones", 64),
        ],
    ),
    c(
        "rules.end_stats",
        r#"{ "note_kill": { "tick": 10, "killer_team": 1, "victim_team": 2 } }"#,
        &["match_not_permitted", "match_rules_unavailable"],
        &[items("players", 256)],
    ),
    c(
        "rules.series",
        r#"{ "length": 3 }"#,
        &[
            "lives_range",
            "match_not_permitted",
            "match_rules_unavailable",
            "series_length",
        ],
        &[],
    ),
    c(
        "rules.vote_cast",
        r#"{ "voter": 256, "option": 0 }"#,
        &[
            "match_not_permitted",
            "match_rules_unavailable",
            "vote_bad_option",
            "vote_closed",
        ],
        &[],
    ),
    c("rules.vote_status", r#"{}"#, &["match_rules_unavailable"], &[]),
    heavy(
        "terrain.inspect_source",
        r#"{"path":"terrain/source.json"}"#,
        &[
            "F0003",
            "height_io_bad_format",
            "height_io_decode",
            "height_io_read",
            "height_source_allocation",
            "height_source_limit_bytes",
            "height_source_limit_depth",
            "height_source_limit_metadata",
            "height_source_limit_samples",
            "terrain_raster_mask",
            "terrain_raster_missing_data",
            "terrain_raster_transform",
            "terrain_raster_version",
            "terrain_tiles_bounds",
            "terrain_tiles_dimensions",
            "terrain_tiles_gap",
            "terrain_tiles_limit",
            "terrain_tiles_mapping",
            "terrain_tiles_misregistered",
            "terrain_tiles_overlap",
            "terrain_tiles_path",
            "terrain_tiles_seam",
            "terrain_tiles_version",
        ],
        &[bytes("path", 1024)],
    ),
    heavy(
        "terrain.bake_overview",
        r#"{"path":"terrain/source.json","output":"previews/terrain.png"}"#,
        &[
            "F0003",
            "height_io_bad_format",
            "height_io_decode",
            "height_io_read",
            "height_io_write",
            "height_source_allocation",
            "height_source_limit_bytes",
            "height_source_limit_depth",
            "height_source_limit_metadata",
            "height_source_limit_samples",
            "terrain_raster_mask",
            "terrain_raster_missing_data",
            "terrain_raster_transform",
            "terrain_raster_version",
        ],
        &[bytes("path", 1024), bytes("output", 1024)],
    ),
    // Lockstep desync rows (ADR-8080): F0003 without a DesyncHashPlugin; the parameter codes are in the
    // message and on book/agents/desync-hash-ecs.md.
    c("sim.desync_rows", r#"{ "count": 8 }"#, &["F0003"], &[]),
    c("render.scene_hold", r#"{}"#, &["F0003"], &[]),
    c("render.set_scene_hold", r#"{"on": true}"#, &["F0003"], &[]),
    // What the gameplay pause holds (ADR-10105): F0003 without a Time resource.
    c("play.freeze_state", r#"{}"#, &["F0003"], &[]),
    // Frame caps per app state and the dynamic-resolution hold (ADR-7889).
    c("app.pacing", r#"{}"#, &[], &[]),
    c(
        "app.set_pacing_state",
        r#"{ "state": "menu", "caps": { "menu": 60, "loading": 60, "paused": 30 } }"#,
        &["pacing_bad_cap", "pacing_unknown_state"],
        &[],
    ),
    c("render.dynres", r#"{}"#, &[], &[]),
    // The runtime renderer's per-frame load budget (ADR-7884).
    c(
        "render.load_budget",
        r#"{}"#,
        &["load_budget_renderer_missing"],
        &[],
    ),
    c(
        "render.set_load_budget",
        r#"{ "upload_bytes_per_frame": 67108864, "new_pipelines_per_frame": 8, "late": "defer" }"#,
        &["load_budget_bad_param", "load_budget_renderer_missing"],
        &[],
    ),
    // UDP socket buffers and DSCP EF (book/agents/net-socket-buffers.md, ADR-7998).
    // The ceiling is `regia_net::MAX_SOCKET_BUFFER_BYTES` (64 MiB).
    c(
        "net.socket_buffers",
        r#"{ "recv_bytes": 65536, "send_bytes": 65536, "dscp_ef": true }"#,
        &["socket_buffers_bad_params", "socket_buffers_bind"],
        &[
            range("recv_bytes", 1.0, regia_net::MAX_SOCKET_BUFFER_BYTES as f64),
            range("send_bytes", 1.0, regia_net::MAX_SOCKET_BUFFER_BYTES as f64),
        ],
    ),
    heavy("diagnostics.report_sources", r#"{}"#, &[], &[]),
    c(
        "terrain.unit_view",
        r#"{ "team": 1, "viewer": [0.0, 2.0, 0.0], "viewer_underground": false, "units": [{ "id": 2, "team": 2, "pos": [1.0, 2.0, 0.0], "underground": true, "entrance": false }] }"#,
        &["unit_view_bad"],
        &[items("units", 4096)],
    ),
    heavy(
        "editor.asset_library",
        r#"{"action":"list"}"#,
        &[
            "editor_not_running",
            "library_busy",
            "library_io",
            "library_no_project",
            "library_package",
            "library_params",
            "library_path",
            "library_unconfigured",
        ],
        &[bytes("action", 8), bytes("entry", 36), bytes("path", 4096)],
    ),
    c(
        "editor.viewport_overlays",
        r#"{"action":"get","pane":"perspective"}"#,
        &[
            "F0003",
            "editor_not_running",
            "viewport_overlay_invalid",
            "viewport_overlay_preset_missing",
            "viewport_overlay_read_only",
            "viewport_overlay_saved_invalid",
        ],
        &[],
    ),
    c("assets.access_trace", r#"{ "limit": 64 }"#, &[], &[]),
    c(
        "ai.team_knowledge",
        r#"{ "team": 0 }"#,
        &["ai_perception_unavailable", "unknown_team"],
        &[],
    ),
    c(
        "ai.senses",
        r#"{ "name": "scent" }"#,
        &["ai_perception_unavailable", "unknown_sense"],
        &[],
    ),
    c(
        "sim_bridge.presentation_stats",
        "{}",
        &["presentation_physics_unavailable"],
        &[],
    ),
    // Render seam of open mesh-layer mouths (ADR-8291, spec 26 § 2a.11).
    c(
        "terrain.seam_report",
        r#"{ "hole": "north_mouth", "pass": "main" }"#,
        &["hole_not_filled", "unknown_hole"],
        &[bytes("hole", 64), bytes("pass", 16)],
    ),
    // Per-class delivery contract (ADR-8401). A wrong up axis is a finding, not one of these codes.
    c(
        "assets.check_contract",
        r#"{ "contract_ron": "(format_version:1,class_name:\"prop\",scale:(meters_per_unit:1.0,identity_tolerance:0.001),axes:(up:pos_y,forward:pos_z),origin:(ground_offset_m:0.0,ground_tolerance_m:0.05,coarse_floor_rise_m:0.25,center_fraction:0.1),lod:(required:[\"lod0\"],budget_lod:\"lod0\",lod0_min_triangles:1,lod0_max_triangles:100000,chain:[\"lod0\"],require_decreasing:true))", "observation": { "node_scales": [[1.0, 1.0, 1.0]], "authored_up": "pos_y", "bounds_min": [-0.5, 0.0, -0.5], "bounds_max": [0.5, 1.0, 0.5], "bounds_present": true, "coarse_lod": false, "lod_triangles": { "lod0": 12 } } }"#,
        &[
            "bad_contract",
            "contract_invalid",
            "contract_parse",
            "contract_version_newer",
            "observation_invalid",
        ],
        &[bytes("contract_ron", 1_048_576)],
    ),
    c("anim.node_drivers", r#"{"entity":1}"#, &["F0003"], &[]),
    // The host's keyed draws without entities (ADR-8087): a bad limit is the universal F0002.
    c(
        "render.draw_list",
        r#"{ "limit": 16 }"#,
        &[],
        &[range("limit", 0.0, 4096.0)],
    ),
    c(
        "package.parity",
        r#"{"dev_trace":"D","dev_root":"DR","package_trace":"P","package_root":"PR"}"#,
        &[
            "parity_bad_params",
            "parity_not_a_directory",
            "parity_trace_missing",
        ],
        &[
            bytes("dev_trace", 4096),
            bytes("dev_root", 4096),
            bytes("package_root", 4096),
            bytes("package_trace", 4096),
        ],
    ),
    heavy(
        "logs.perf_gate",
        r#"{"logs":["{\"target\":\"regia::summary\",\"phase\":\"play\",\"map\":\"map_a\",\"res_class\":\"1080p\",\"tier\":\"HIGH\",\"frames\":[{\"frame_ms\":16.6}]}"],"thresholds":"[classes.\"1080p\".HIGH.default]\nfps_min = 60.0\nframe_ms_p99_max = 33.0\n","regression_pct":10}"#,
        &[
            "logs_perf_gate_baseline",
            "logs_perf_gate_input",
            "logs_perf_gate_thresholds",
        ],
        &[
            bytes("thresholds", 1_048_576),
            bytes("baseline", 8_388_608),
            items("logs", 64),
        ],
    ),
    heavy(
        "logs.triage",
        r#"{"bundles":[{"name":"sample","files":{"sample.jsonl":"{\"t_ms\":0,\"target\":\"app::start\"}\n"}}]}"#,
        &[
            "logs_triage_empty",
            "logs_triage_input",
            "logs_triage_limit",
            "logs_triage_overlap",
            "logs_triage_path",
            "logs_triage_range",
            "logs_triage_settings",
        ],
        &[items("bundles", 4096)],
    ),
    // Vertex-cluster far LOD (ADR-8464, book/agents/cluster-lod.md): F0003 for a missing entity,
    // the `cluster_lod_*` codes for a piece the grid cannot use or a switch size out of order.
    heavy(
        "render.cluster_lod",
        r#"{ "entities": [4294967301, 4294967302], "target_triangles": 2000, "dry_run": true }"#,
        &[
            "F0003",
            "cluster_lod_bad_mesh",
            "cluster_lod_deforming",
            "cluster_lod_no_mesh",
            "cluster_lod_screen_size",
        ],
        &[
            items("entities", 256),
            range("target_triangles", 1.0, 10_000_000.0),
            range("screen_size", 0.0, 1.0),
        ],
    ),
    // Cook diet report (row C-04, ADR-8461): F0016 when the profile's pak has no report, F0013 for
    // one that does not read.
    c(
        "assets.diet_report",
        r#"{ "profile": "release", "platform": "linux" }"#,
        &[
            "F0013",
            "F0016",
            "diet_report_bad_param",
            "diet_report_not_written",
            "diet_report_unreadable",
        ],
        &[],
    ),
    // The report outbox (ADR-8402, book/agents/report-outbox.md): no logs directory and no ledger.
    heavy("report.outbox_status", r#"{}"#, &["outbox_unavailable"], &[]),
    // Lockstep projectiles and the aim solver (ADR-8509). No range limits: the handler answers
    // `steps_limit` and `bad_params` with the field named.
    c(
        "detsim.projectile_simulate",
        r#"{ "launch": { "pos": [0.0, 1.0, 0.0], "vel": [20.0, 15.0, 0.0] }, "params": { "radius_m": 0.05, "restitution": 0.5, "max_bounces": 2 }, "geometry": { "ground_y": 0.0 }, "steps": 600, "sample_every": 10 }"#,
        &["bad_params", "steps_limit"],
        &[],
    ),
    c(
        "detsim.aim_solve",
        r#"{ "delta": [150.0, 5.0, 40.0], "speed": 80.0, "params": { "drag_per_m": 0.004 }, "drag": true, "iterations": 8 }"#,
        &["bad_params", "steps_limit"],
        &[],
    ),
    c(
        "input.keyboard_layout",
        r#"{ "layout": "colemak" }"#,
        &["keyboard_layout_bad_params"],
        &[bytes("layout", 32)],
    ),
    // Leashed photo mode (ADR-8415, book/agents/photo-mode.md).
    c("photo.enter", r#"{}"#, &["photo_not_allowed"], &[]),
    c("photo.set", r#"{}"#, &[], &[]),
    c(
        "photo.capture",
        r#"{}"#,
        &[
            "path_outside_project",
            "photo_capture_io",
            "photo_inactive",
            "photo_no_directory",
            "photo_not_allowed",
        ],
        &[],
    ),
    c("photo.status", r#"{}"#, &[], &[]),
    // UI pointer claims (book/agents/ui-input.md, ADR-7976).
    c(
        "input.pointer_claims",
        r#"{ "watch": ["Fire"] }"#,
        &[
            "pointer_claims_bad_params",
            "pointer_claims_not_installed",
            "pointer_claims_not_watched",
            "pointer_claims_unknown_action",
        ],
        &[],
    ),
    // Overlay asset roots (book/agents/cook-overlay-roots.md, ADR-8155).
    c(
        "cook.overlay_roots",
        r#"{ "platform": "linux", "path": "tables/weapons.table.ron" }"#,
        &["overlay_path_invalid", "overlay_roots_config_invalid"],
        &[bytes("platform", 32), bytes("path", 1024)],
    ),
    // Grounded placement and the built-in install tiers (ADR-8036).
    c(
        "content.check_placement",
        r#"{ "objects": [{ "id": "marker", "x": 0, "z": 0 }], "half_extent": 100 }"#,
        &["placement_bad_input"],
        &[
            items("objects", 4096),
            range("objects", -10_000_000.0, 10_000_000.0),
            range("ground_y", -1_000_000.0, 1_000_000.0),
            range("half_extent", 0.0, 1_000_000_000.0),
            range("ground_tolerance_m", 0.0, 10_000.0),
            range("edge_tolerance_m", 0.0, 10_000.0),
            range("spawn_ring_m", 0.0, 10_000.0),
            range("spawn_rise_over_run", 0.0, 100.0),
        ],
    ),
    c(
        "content.install_tier",
        r#"{ "tier": "low" }"#,
        &["install_tier_unknown"],
        &[bytes("tier", 64)],
    ),
    // The lockstep hover (book/agents/det-hover.md). `steps` and `dt` are checked by the handler
    // (hover_steps_limit, hover_bad_input), not by a contract limit.
    heavy(
        "detsim.hover_step",
        r#"{ "input": { "throttle": 1.0, "steer": 0.25 }, "steps": 60 }"#,
        &["hover_bad_input", "hover_bad_params", "hover_steps_limit"],
        &[],
    ),
    heavy(
        "detsim.hover_probe",
        r#"{ "script": "accel" }"#,
        &["hover_bad_input", "hover_bad_params"],
        &[],
    ),
    c("detsim.hover_list", r#"{}"#, &[], &[]),
    c(
        "detsim.hover_get",
        r#"{ "entity": 4294967296 }"#,
        &["hover_no_hull"],
        &[],
    ),
    c(
        "detsim.hover_set_input",
        r#"{ "entity": 4294967296, "input": { "throttle": 1.0 } }"#,
        &["hover_bad_input", "hover_no_hull"],
        &[],
    ),
    // A `.hoverhull.ron` checked on the host (ADR-8512). Exactly one of `text` or `path`.
    c(
        "detsim.hover_hull_check",
        r#"{ "text": "(format_version: 1, name: \"scout\")" }"#,
        &[
            "bad_flight",
            "bad_name",
            "bad_version",
            "inverted_range",
            "negative",
            "not_finite",
            "out_of_range",
            "parse_error",
            "zero_rate",
        ],
        &[bytes("text", 1 << 20), bytes("path", 4096)],
    ),
    c(
        "rules.match_assign",
        r#"{ "group": [ { "unit": 1 }, { "unit": 2 } ] }"#,
        &[
            "match_rules_unavailable",
            "match_seat_bad",
            "match_seat_limit",
            "match_seat_missing",
            "match_stat_cap",
            "match_team_switch_refused",
        ],
        &[items("group", 64)],
    ),
    c("ai.debug_layers", "{}", &[], &[]),
    c(
        "ai.debug_layers_set",
        r#"{"layers":["bt","squad"],"filter":"radius","radius":12.5}"#,
        &["bad_filter", "unknown_layer"],
        &[],
    ),
    // Cooked world geometry (spec 121 § 12, ADR-8386, ADR-8398, ADR-8627). Error lists are sorted.
    c(
        "terrain.surface_at",
        r#"{ "points": [[12.0, 4.0, 1.0]], "tolerance_m": 0.0 }"#,
        &[
            "F0003",
            "bad_point",
            "layers_loading",
            "no_world_geom",
            "non_finite",
            "out_of_map",
            "too_many_points",
            "unknown_section",
        ],
        &[bytes("map", 256)],
    ),
    c(
        "terrain.mesh_layers",
        r#"{}"#,
        // No geometry answers `loaded: false`, not `no_world_geom` (ADR-8576 item 6, ADR-8627).
        &["F0003", "layers_loading", "unknown_section"],
        &[bytes("map", 256)],
    ),
    c(
        "terrain.bake_report",
        r#"{ "map": "switchbacks" }"#,
        &["F0003", "layers_loading", "no_bake_report", "no_world_geom"],
        &[bytes("map", 256)],
    ),
    c(
        "terrain.blockers",
        r#"{ "set": 0, "active": true }"#,
        &[
            "F0003",
            "blocker_sim_owned",
            "blocker_unknown",
            "layers_loading",
            "no_world_geom",
            "unknown_section",
        ],
        &[bytes("map", 256)],
    ),
    // Lockstep ground vehicles (book/agents/det-ground-vehicles.md, ADR-8551). Refusals are F0002 with
    // data.reason bad_model, bad_params or steps_limit (the handler checks `steps` itself so that a count
    // past 10000 answers steps_limit).
    c(
        "detsim.ground_vehicle_simulate",
        r#"{ "model": "tracked", "steps": 60, "inputs": [ { "left_throttle": 1.0, "right_throttle": 0.5 } ] }"#,
        &[],
        &[items("inputs", 10_000)],
    ),
    c(
        "rules.spawn_preview",
        r#"{ "player": 4294967297 }"#,
        &["spawn_player_missing"],
        &[],
    ),
    // Rule sets as content (`.rules.ron` v1, ADR-8694). Error lists are sorted.
    c("rules.ruleset_list", r#"{}"#, &[], &[]),
    c(
        "rules.ruleset_get",
        r#"{ "id": "ctf", "options": {} }"#,
        &[
            "ruleset_cycle",
            "ruleset_depth",
            "ruleset_invalid",
            "ruleset_option_kind",
            "ruleset_option_range",
            "ruleset_option_unknown",
            "ruleset_unknown",
        ],
        &[bytes("id", 64)],
    ),
    c(
        "rules.ruleset_validate",
        r#"{ "text": "(format_version: 1, id: \"ctf_long\", extends: \"ctf\", session: (objectives: (capture_limit: 5)))" }"#,
        &[
            "ruleset_cycle",
            "ruleset_depth",
            "ruleset_duplicate",
            "ruleset_invalid",
            "ruleset_option_kind",
            "ruleset_option_range",
            "ruleset_option_unknown",
            "ruleset_parse",
            "ruleset_too_large",
            "ruleset_unknown",
            "ruleset_version",
        ],
        &[bytes("text", 256 * 1024)],
    ),
    c(
        "rules.match_report",
        r#"{ "tagged": [{ "team": 1, "tag": "capital", "count": 0 }], "holders": [{ "objective": 7, "team": 2 }] }"#,
        &["match_rules_unavailable", "match_victory_settings"],
        &[
            items("defeat", 256),
            items("holders", 256),
            items("scores", 256),
            items("tagged", 256),
        ],
    ),
    // Pack provenance records into `.meta` provenance (ADR-8834, book/agents/provenance-import.md).
    c(
        "assets.provenance_import",
        r#"{ "records": ["provenance"], "under": "packs/base", "dry_run": true }"#,
        &[
            "path_outside_project",
            "provenance_import_conflict",
            "provenance_import_invalid",
            "provenance_import_unreadable",
            "provenance_import_write_failed",
        ],
        &[items("records", 256)],
    ),
    // Client capture on a NetLink (book/agents/net-capture.md, ADR-8890). Refusals are F0002 with
    // data.reason.
    c(
        "net.capture_start",
        r#"{ "ring_bytes": 16777216 }"#,
        &["capture_config_invalid", "capture_no_client"],
        &[],
    ),
    c(
        "net.capture_save",
        r#"{ "path": "client.netcapture", "label": "lag report", "overwrite": false, "stop": false }"#,
        &[
            "capture_empty",
            "capture_exists",
            "capture_io",
            "capture_not_started",
            "path_outside_project",
        ],
        &[],
    ),
    c(
        "nav.check_stair",
        r#"{ "x": 0, "z": 0, "direction": "+x", "width_m": 2, "y0_m": 0, "rise_m": 2, "run_m": 0.6 }"#,
        &["nav_stair_input"],
        &[],
    ),
    c(
        "vfx.damage_decal_atlas",
        r#"{ "generate": false }"#,
        &["decal_atlas_input"],
        &[],
    ),
    // Decal footstep surfaces (ADR-8829): F0016 without a decal pool.
    c(
        "vfx.decal_surface_at",
        r#"{ "point": [0.0, 0.0, 0.0] }"#,
        &["F0016"],
        &[range("point", -1000000.0, 1000000.0)],
    ),
    c(
        "rules.match_results",
        r#"{ "since": "2026-11-18", "limit": 5 }"#,
        &["match_results_bad_since", "match_results_unavailable"],
        &[range("limit", 1.0, 200.0)],
    ),
    c(
        "rules.match_result",
        r#"{ "match_id": "6d617463-682d-7265-7375-6c7400000001" }"#,
        &[
            "match_result_bad_id",
            "match_result_malformed",
            "match_result_newer",
            "match_result_not_found",
            "match_result_read_failed",
            "match_result_too_large",
            "match_results_unavailable",
        ],
        &[],
    ),
    // Locomotion quality (ADR-8708, book/agents/locomotion-quality.md). The simulate handlers check
    // `steps` themselves so a count past 20000 answers simulate_too_long.
    c(
        "anim.locomotion_state",
        r#"{ "entity": 4294967310 }"#,
        &["no_locomotion", "no_such_entity"],
        &[],
    ),
    heavy(
        "anim.gait_simulate",
        r#"{ "steps": 600, "ground": { "kind": "rubble", "amplitude": 0.1, "seed": 7 }, "inputs": [[1.0, 0.0, 0.0, 300], [0.0, 0.0, 1.0]] }"#,
        &[
            "F0003",
            "bad_ground",
            "gait_floor",
            "gait_input",
            "gait_io",
            "gait_legs",
            "gait_parse",
            "gait_pattern",
            "gait_settings",
            "gait_state",
            "gait_step_too_large",
            "gait_version",
            "simulate_too_long",
        ],
        &[items("inputs", 20_000), bytes("gait_asset", 1024)],
    ),
    heavy(
        "anim.foot_plant_simulate",
        r#"{ "steps": 180, "speed_scale": 1.15, "ground": { "kind": "flat" } }"#,
        &["bad_ground", "simulate_too_long"],
        &[],
    ),
    c(
        "anim.ik_report",
        r#"{ "entity": 4294967310, "enable": true }"#,
        &["no_animator", "no_such_entity"],
        &[],
    ),
    // The game pause key and its screen (ADR-10104, book/agents/game-pause.md). F0002 data.reason codes.
    c(
        "game.pause",
        r#"{}"#,
        &[
            "F0018",
            "game_pause_not_installed",
            "game_pause_off",
            "networked_session",
        ],
        &[],
    ),
    c(
        "game.resume",
        r#"{}"#,
        &["F0018", "game_pause_not_installed"],
        &[],
    ),
    c("game.pause_state", r#"{}"#, &["game_pause_not_installed"], &[]),
];

/// Contracts of the built-in extension methods (ADR-1809): the `curves.*`, `chooser.*`,
/// `abilities.*`, `physics.cloth_*`, `render.breadcrumbs` and `telemetry.*` trace methods
/// (ADR-2307) that `AgentPlugin` installs on the world's
/// `AgentExtension` (see `crate::install_builtin_extensions`). They are not in
/// [`crate::schema::METHODS`], so they are kept apart from [`CONTRACTS`]; their lowercase codes are
/// documented on their book pages (`book/agents/curve-tables.md`, `choosers.md`, `abilities.md`,
/// `cloth.md`), which `cargo xtask agent-coverage` checks.
pub const EXTENSION_CONTRACTS: &[MethodContract] = &[
    c("curves.list", r#"{}"#, &[], &[]),
    c(
        "curves.describe",
        r#"{ "path": "tables/falloff.curves.ron" }"#,
        &["F0003", "bad_value", "curve_table_not_found"],
        &[],
    ),
    c(
        "curves.get",
        r#"{ "path": "tables/falloff.curves.ron", "name": "falloff" }"#,
        &["F0003", "bad_value", "curve_not_found", "curve_table_not_found"],
        &[],
    ),
    c(
        "curves.sample",
        r#"{ "path": "tables/falloff.curves.ron", "name": "falloff", "x": 12.5 }"#,
        &["F0003", "bad_value", "curve_not_found", "curve_table_not_found"],
        &[],
    ),
    c(
        "curves.set_key",
        r#"{ "path": "tables/falloff.curves.ron", "name": "falloff", "x": 5.0, "y": 0.9 }"#,
        &[
            "F0003",
            "bad_value",
            "curve_bad_number",
            "curve_not_found",
            "curve_table_not_found",
            "validation_failed",
        ],
        &[],
    ),
    c(
        "curves.remove_key",
        r#"{ "path": "tables/falloff.curves.ron", "name": "falloff", "x": 5.0 }"#,
        &[
            "F0003",
            "bad_value",
            "curve_empty",
            "curve_not_found",
            "curve_table_not_found",
            "key_not_found",
        ],
        &[],
    ),
    c(
        "curves.set_curve",
        r#"{ "path": "tables/falloff.curves.ron", "name": "ramp", "keys": [[0, 0], [1, 2]], "post": "linear" }"#,
        &[
            "F0003",
            "bad_value",
            "curve_bad_name",
            "curve_bad_number",
            "curve_duplicate_name",
            "curve_duplicate_x",
            "curve_empty",
            "curve_table_not_found",
            "curve_unsorted_x",
            "validation_failed",
        ],
        &[],
    ),
    c(
        "curves.delete",
        r#"{ "path": "tables/falloff.curves.ron", "name": "ramp" }"#,
        &["F0003", "bad_value", "curve_not_found", "curve_table_not_found"],
        &[],
    ),
    c(
        "curves.validate",
        r#"{ "path": "tables/falloff.curves.ron" }"#,
        &["F0003", "bad_value", "curve_table_not_found"],
        &[],
    ),
    c(
        "curves.export",
        r#"{ "path": "tables/falloff.curves.ron", "format": "csv" }"#,
        &["F0003", "bad_value", "curve_table_not_found", "read_only_asset"],
        &[],
    ),
    c(
        "curves.import",
        r#"{ "path": "tables/falloff.curves.ron", "csv": "name,0,10\na,1,0\n" }"#,
        &[
            "F0003",
            "bad_value",
            "curve_bad_file",
            "curve_csv_ragged",
            "curve_table_not_found",
            "validation_failed",
        ],
        &[],
    ),
    c("chooser.list", r#"{}"#, &[], &[]),
    c(
        "chooser.describe",
        r#"{ "path": "choosers/hit.chooser.ron" }"#,
        &["F0003", "bad_value", "chooser_not_found"],
        &[],
    ),
    c(
        "chooser.get",
        r#"{ "path": "choosers/hit.chooser.ron", "row": "front" }"#,
        &["F0003", "bad_value", "chooser_not_found", "row_not_found"],
        &[],
    ),
    c(
        "chooser.set_cell",
        r#"{ "path": "choosers/hit.chooser.ron", "row": "front", "column": "angle", "value": [-45.0, 45.0] }"#,
        &[
            "F0003",
            "bad_value",
            "chooser_bad_cell",
            "chooser_not_found",
            "column_not_found",
            "row_not_found",
            "validation_failed",
        ],
        &[],
    ),
    c(
        "chooser.set_row",
        r#"{ "path": "choosers/hit.chooser.ron", "row": "back", "result": { "asset": "anim/hit_back.montage.ron" } }"#,
        &[
            "F0003",
            "bad_value",
            "chooser_not_found",
            "chooser_result_type_mismatch",
            "column_not_found",
            "row_not_found",
            "validation_failed",
        ],
        &[],
    ),
    c(
        "chooser.remove_row",
        r#"{ "path": "choosers/hit.chooser.ron", "row": "back" }"#,
        &[
            "F0003",
            "bad_value",
            "chooser_not_found",
            "row_not_found",
            "validation_failed",
        ],
        &[],
    ),
    c(
        "chooser.set_column",
        r#"{ "path": "choosers/hit.chooser.ron", "column": { "id": "armed", "input": "armed", "kind": "Bool" } }"#,
        &[
            "F0003",
            "bad_value",
            "chooser_kind_mismatch",
            "chooser_not_found",
            "chooser_unknown_input",
            "column_not_found",
            "validation_failed",
        ],
        &[],
    ),
    c(
        "chooser.remove_column",
        r#"{ "path": "choosers/hit.chooser.ron", "column": "armed" }"#,
        &[
            "F0003",
            "bad_value",
            "chooser_not_found",
            "column_not_found",
            "validation_failed",
        ],
        &[],
    ),
    c(
        "chooser.set_options",
        r#"{ "path": "choosers/hit.chooser.ron", "mode": "weighted", "recency": { "window": 2, "penalty": 0.25 } }"#,
        &[
            "F0003",
            "bad_value",
            "chooser_bad_number",
            "chooser_not_found",
            "chooser_result_type_mismatch",
            "unknown_input",
            "validation_failed",
        ],
        &[],
    ),
    c(
        "chooser.validate",
        r#"{ "path": "choosers/hit.chooser.ron" }"#,
        &["F0003", "bad_value", "chooser_not_found"],
        &[],
    ),
    c(
        "chooser.evaluate",
        r#"{ "path": "choosers/hit.chooser.ron", "context": { "angle": 30.0 }, "seed": 7, "trace": "brief" }"#,
        &[
            "F0003",
            "bad_context",
            "bad_value",
            "chooser_not_found",
            "unknown_input",
        ],
        &[],
    ),
    c(
        "chooser.evaluate_binding",
        r#"{ "entity": 4294967300 }"#,
        &[
            "F0003",
            "bad_value",
            "binding_not_found",
            "chooser_not_found",
            "provider_not_found",
        ],
        &[],
    ),
    c("chooser.trace", r#"{ "site": "hit", "limit": 8 }"#, &[], &[]),
    c(
        "chooser.export",
        r#"{ "path": "choosers/hit.chooser.ron" }"#,
        &["F0003", "bad_value", "chooser_not_found", "read_only_asset"],
        &[],
    ),
    c("abilities.list", r#"{}"#, &["F0003", "entity_not_found"], &[]),
    c(
        "abilities.inspect",
        r#"{ "entity": 4294967300 }"#,
        &["F0003", "entity_not_found", "unknown_attribute"],
        &[],
    ),
    c(
        "abilities.check",
        r#"{ "entity": 4294967300, "grant": 1 }"#,
        &["F0003", "entity_not_found", "grant_not_found"],
        &[],
    ),
    c(
        "abilities.grant",
        r#"{ "entity": 4294967300, "ability": "abilities/fire_bolt.ability.ron", "level": 2 }"#,
        &[
            "F0003",
            "asset_not_found",
            "bad_value",
            "entity_not_found",
            "grant_limit",
            "reference_missing",
        ],
        &[],
    ),
    c(
        "abilities.revoke",
        r#"{ "entity": 4294967300, "grant": 1 }"#,
        &["F0003", "entity_not_found", "grant_not_found"],
        &[],
    ),
    c(
        "abilities.activate",
        r#"{ "entity": 4294967300, "grant": 1 }"#,
        &[
            "F0003",
            "bad_query",
            "bad_value",
            "entity_not_found",
            "grant_not_found",
        ],
        &[],
    ),
    c(
        "abilities.charge",
        r#"{ "entity": 4294967300, "grant": 1 }"#,
        &["F0003", "entity_not_found", "grant_not_found"],
        &[],
    ),
    c(
        "abilities.end",
        r#"{ "entity": 4294967300, "grant": 1 }"#,
        &["F0003", "entity_not_found", "grant_not_found"],
        &[],
    ),
    c(
        "abilities.cancel",
        r#"{ "entity": 4294967300, "grant": 1 }"#,
        &["F0003", "entity_not_found", "grant_not_found", "not_cancelable"],
        &[],
    ),
    c(
        "abilities.signal",
        r#"{ "entity": 4294967300, "tag": "Event.Hit", "amount": 12.0 }"#,
        &["F0003", "bad_query", "entity_not_found"],
        &[],
    ),
    c(
        "abilities.apply_effect",
        r#"{ "target": 4294967300, "effect": "effects/burn.effect.ron", "level": 1 }"#,
        &[
            "F0003",
            "asset_not_found",
            "effect_not_found",
            "entity_not_found",
            "no_attributes",
            "reference_missing",
        ],
        &[],
    ),
    c(
        "abilities.remove_effect",
        r#"{ "entity": 4294967300, "query": "Has(\"Status.Burning\")" }"#,
        &["F0003", "bad_query", "effect_not_found", "entity_not_found"],
        &[],
    ),
    c(
        "abilities.set_base",
        r#"{ "entity": 4294967300, "attribute": "health", "base": 40.0 }"#,
        &[
            "F0003",
            "attribute_derived",
            "attribute_not_finite",
            "entity_not_found",
            "no_attributes",
            "unknown_attribute",
        ],
        &[],
    ),
    c(
        "abilities.add_attributes",
        r#"{ "entity": 4294967300, "asset": "attributes/hero.attrs.ron" }"#,
        &["F0003", "asset_not_found", "entity_not_found", "format_too_new"],
        &[],
    ),
    c(
        "abilities.cues",
        r#"{ "since_tick": 120, "limit": 16 }"#,
        &[],
        &[],
    ),
    c(
        "abilities.validate",
        r#"{ "path": "abilities/fire_bolt.ability.ron" }"#,
        &["F0003", "asset_not_found"],
        &[],
    ),
    c("abilities.stats", r#"{}"#, &[], &[]),
    c("abilities.state_hash", r#"{}"#, &[], &[]),
    c("physics.cloth_stats", r#"{ "name": "banner" }"#, &["F0003"], &[]),
    c(
        "physics.cloth_inspect",
        r#"{ "target": { "name": "banner" }, "particles": { "from": 0, "count": 8 } }"#,
        &[
            "F0003",
            "cloth_bad_param",
            "cloth_not_found",
            "cloth_particle_out_of_range",
        ],
        &[],
    ),
    c(
        "physics.cloth_set_params",
        r#"{ "target": { "name": "banner" }, "patch": { "damping": 0.1 } }"#,
        &[
            "F0003",
            "cloth_bad_param",
            "cloth_not_found",
            "cloth_plugin_missing",
            "unknown_cloth_preset",
        ],
        &[],
    ),
    c(
        "physics.cloth_pin",
        r#"{ "target": { "name": "banner" }, "particles": [0, 1] }"#,
        &[
            "F0003",
            "cloth_bad_param",
            "cloth_not_found",
            "cloth_particle_out_of_range",
            "cloth_plugin_missing",
        ],
        &[],
    ),
    c(
        "physics.cloth_unpin",
        r#"{ "target": { "name": "banner" }, "particles": [0, 1], "inv_mass": 1.0 }"#,
        &[
            "F0003",
            "cloth_bad_param",
            "cloth_non_finite",
            "cloth_not_found",
            "cloth_particle_out_of_range",
            "cloth_plugin_missing",
        ],
        &[],
    ),
    c(
        "physics.cloth_wind",
        r#"{ "target": { "name": "banner" }, "velocity": [3.0, 0.0, 0.0] }"#,
        &[
            "F0003",
            "cloth_bad_param",
            "cloth_non_finite",
            "cloth_not_found",
            "cloth_plugin_missing",
        ],
        &[],
    ),
    c(
        "physics.cloth_step",
        r#"{ "target": { "name": "banner" }, "count": 60 }"#,
        &[
            "F0003",
            "cloth_bad_param",
            "cloth_not_found",
            "cloth_plugin_missing",
            "cloth_step_count_too_large",
        ],
        &[],
    ),
    c(
        "physics.cloth_reset",
        r#"{ "target": { "name": "banner" } }"#,
        &[
            "F0003",
            "cloth_bad_param",
            "cloth_not_found",
            "cloth_plugin_missing",
        ],
        &[],
    ),
    c(
        "physics.cloth_edit_map",
        r#"{ "asset": "cape", "map": "reach", "edit": { "op": "set", "vertices": [[0, 0.0]] } }"#,
        &[
            "F0003",
            "cloth_bad_param",
            "cloth_map_size_mismatch",
            "cloth_negative",
            "cloth_non_finite",
            "cloth_not_found",
            "cloth_particle_out_of_range",
        ],
        &[],
    ),
    c("physics.cloth_presets", "{}", &[], &[]),
    c(
        "physics.cloth_set_layers",
        r#"{ "target": { "name": "banner" }, "range": { "from": 0, "count": 8 }, "layer": 1 }"#,
        &[
            "F0003",
            "cloth_bad_param",
            "cloth_not_found",
            "cloth_particle_out_of_range",
            "cloth_plugin_missing",
        ],
        &[],
    ),
    c(
        "physics.cloth_health",
        r#"{ "target": { "name": "banner" } }"#,
        &["F0003", "cloth_bad_param", "cloth_not_found"],
        &[],
    ),
    // GPU breadcrumbs and the trace recorder (spec 54, ADR-2307; book/agents/gpu-breadcrumbs-and-trace.md).
    c(
        "render.breadcrumbs",
        r#"{ "frames": 2, "source": "live" }"#,
        &[
            "F0003",
            "breadcrumbs_off",
            "invalid_value",
            "no_bundle",
            "unknown_field",
        ],
        &[],
    ),
    c(
        "telemetry.mark",
        r#"{ "name": "boss door opened" }"#,
        &[
            "data_too_large",
            "invalid_value",
            "name_too_long",
            "unknown_field",
        ],
        &[],
    ),
    c(
        "telemetry.range_open",
        r#"{ "name": "stress", "lane": "main" }"#,
        &["invalid_lane", "invalid_value", "name_too_long", "unknown_field"],
        &[],
    ),
    c(
        "telemetry.range_close",
        r#"{ "id": 1 }"#,
        &["F0003", "invalid_value", "unknown_field", "unknown_range"],
        &[],
    ),
    c(
        "telemetry.trace",
        r#"{ "action": "start", "file": "boss" }"#,
        &[
            "bad_file_name",
            "invalid_value",
            "trace_busy",
            "trace_io",
            "trace_not_writing",
            "unknown_field",
        ],
        &[],
    ),
    c(
        "telemetry.screenshot_mark",
        r#"{ "name": "door open", "size": [640, 360] }"#,
        &[
            "F0003",
            "invalid_value",
            "name_too_long",
            "no_renderer",
            "screenshot_failed",
            "trace_io",
            "trace_not_writing",
            "unknown_field",
        ],
        &[],
    ),
];

// Host-side editor contracts: data only, with no runtime dependency on the editor.
// Their schemas and handlers are installed by regia_editor (ADR-7824).
const FRAME_DEBUGGER_CONTRACTS: &[MethodContract] = &[
    c(
        "editor.frame_capture",
        r#"{"action":"start"}"#,
        &[
            "capture_no_play_window",
            "capture_unknown_pass",
            "frame_capture_action",
            "frame_capture_budget",
        ],
        &[],
    ),
    heavy(
        "editor.frame_pass",
        r#"{"index":0}"#,
        &["frame_capture_missing", "frame_image_encode", "frame_pass_index"],
        &[],
    ),
    // Draw records of a capture (ADR-8224).
    c(
        "editor.frame_draw",
        r#"{"index":0}"#,
        &["frame_capture_missing", "frame_draw_index"],
        &[],
    ),
];

/// Contracts for review methods supplied by the desktop editor (ADR-7841).
/// Their handlers and schema rows stay in the editor crate.
pub const REVIEW_EDITOR_CONTRACTS: &[MethodContract] = &[
    c(
        "editor.note_add",
        r#"{"text":"Check clearance","position":[0,0,0]}"#,
        &[
            "note_edit",
            "note_entity",
            "note_level",
            "note_placement",
            "note_text",
        ],
        &[
            Limit::Bytes {
                param: "text",
                max: 16384,
            },
            Limit::Items {
                param: "position",
                max: 3,
            },
        ],
    ),
    c(
        "editor.note_reply",
        r#"{"entity":4294967310,"text":"Adjusted"}"#,
        &[
            "note_edit",
            "note_entity",
            "note_not_found",
            "note_text",
            "note_thread_full",
        ],
        &[Limit::Bytes {
            param: "text",
            max: 16384,
        }],
    ),
    c(
        "editor.note_resolve",
        r#"{"entity":4294967310,"resolved":true}"#,
        &["note_edit", "note_entity", "note_not_found", "note_resolved"],
        &[],
    ),
    c("editor.note_list", r#"{"filter":"open"}"#, &["note_filter"], &[]),
    c(
        "editor.activity",
        r#"{"since":0,"source":"all","method":""}"#,
        &["activity_filter"],
        &[],
    ),
];

/// Contract of the editor-only hosted session method (registered by regia_editor).
pub const HOSTED_AGENT_CONTRACT: MethodContract = c(
    "editor.agent_sessions",
    r#"{"action":"list"}"#,
    &[
        "session_bad_action",
        "session_bad_commands",
        "session_bad_message",
        "session_bad_mode",
        "session_bad_plan",
        "session_bad_staged",
        "session_busy",
        "session_credentials",
        "session_io",
        "session_limit",
        "session_no_project",
        "session_not_configured",
        "session_not_found",
        "session_owner_only",
        "session_plan_pending",
        "session_propose_live_unsupported",
        "session_spawn",
        "session_staged_busy",
        "session_staged_failed",
        "session_staged_full",
        "session_stopped",
    ],
    &[
        Limit::Bytes {
            param: "text",
            max: 8192,
        },
        // P-369 (ADR-8375): at most 32 calls are ever staged in one session.
        Limit::Items {
            param: "items",
            max: 32,
        },
    ],
);

/// Contracts of the tool-side `release.*` methods (ADR-8557, ADR-8687, ADR-8846). Their schemas
/// and handlers are installed by the editor (`regia_editor`'s `release_agent`), because they read
/// release directories through the pipeline crate `regia_release_chunks`, which a runtime crate
/// must not link (ADR-10356). Data only, like the other host tables: the runtime neither links
/// the editor nor the pipeline. The editor test `release_contracts_match_the_schemas` keeps the
/// rows equal to what it registers; the codes are documented on
/// `book/agents/release-verify-diff.md` and `release-sign.md`.
pub const RELEASE_HOST_CONTRACTS: &[MethodContract] = &[
    c("release.verify", r#"{ "dir": "release" }"#, &["release_dir"], &[]),
    c(
        "release.diff",
        r#"{ "old": "old", "new": "new" }"#,
        &["release_input"],
        &[],
    ),
    c(
        "release.sign_status",
        r#"{ "dir": "release" }"#,
        &["release_dir"],
        &[],
    ),
    c(
        "release.upload_dry_run",
        r#"{ "dir": "release", "config": "host.toml" }"#,
        &["release_dir"],
        &[],
    ),
];

/// Looks a method's contract up by its JSON-RPC name ([`CONTRACTS`], then
/// [`EXTENSION_CONTRACTS`], then [`WORLD_TOOL_HOST_CONTRACTS`], the editor history, frame debugger and
/// review host contracts, [`REVIEW_EDITOR_CONTRACTS`], then [`RELEASE_HOST_CONTRACTS`]).
pub fn contract(method: &str) -> Option<&'static MethodContract> {
    if method == "editor.agent_sessions" {
        return Some(&HOSTED_AGENT_CONTRACT);
    }
    CONTRACTS
        .iter()
        .chain(EXTENSION_CONTRACTS)
        .chain(WORLD_TOOL_HOST_CONTRACTS)
        .chain(EDITOR_HISTORY_CONTRACTS)
        .chain(FRAME_DEBUGGER_CONTRACTS)
        .chain(REVIEW_EDITOR_CONTRACTS)
        .chain(EDITOR_COLLAB_CONTRACTS)
        .chain(RELEASE_HOST_CONTRACTS)
        .find(|c| c.method == method)
}

/// Editor world-tool contracts. Names and limits only: the runtime never links the editor.
pub const WORLD_TOOL_HOST_CONTRACTS: &[MethodContract] = &[
    heavy(
        "editor.terrain_region",
        r#"{"op":"copy","rect":[0,0,16,16]}"#,
        &["terrain_region_invalid"],
        &[items("rect", 4), items("at", 2), bytes("path", 4096)],
    ),
    heavy(
        "editor.kit_paint",
        r#"{"folder":"assets/kit","piece":"assets/kit/wall.gltf","cells":[[0,0],[1,0]],"floor":0,"rotation":0}"#,
        &["kit_paint_invalid"],
        &[
            items("cells", 4096),
            range("cells", -1000000.0, 1000000.0),
            range("floor", -1000000.0, 1000000.0),
            range("grid", 0.01, 10000.0),
            bytes("folder", 4096),
            bytes("piece", 4096),
        ],
    ),
];

/// Test helper: panics unless `method`'s contract lists `code`. A handler test that proves a call
/// answers with `code` calls this too, so a code the handler emits cannot be missing from the
/// contract `agent.schema` publishes.
#[cfg(test)]
pub(crate) fn assert_listed(method: &str, code: &str) {
    let Some(found) = contract(method) else {
        panic!("{method} has no contract");
    };
    assert!(
        found.errors.contains(&code) || UNIVERSAL_ERRORS.contains(&code),
        "{method} answers with `{code}`, but its contract lists only {:?}",
        found.errors
    );
}

/// Test helper: panics unless every key of `answer` is named in `method`'s schema `result`
/// description. Keys are checked at the top level and inside the objects of a field the
/// description shapes itself (`name: {` or `name: [{`); other nested values are opaque.
#[cfg(test)]
pub(crate) fn assert_result_named(method: &str, answer: &Value) {
    fn named(text: &str, key: &str) -> bool {
        let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
        text.match_indices(key).any(|(at, _)| {
            !text[..at].chars().next_back().is_some_and(ident)
                && !text[at + key.len()..].chars().next().is_some_and(ident)
        })
    }
    fn walk(text: &str, value: &Value, path: &str, missing: &mut Vec<String>) {
        let Value::Object(map) = value else {
            return;
        };
        for (key, inner) in map {
            if !named(text, key) {
                missing.push(format!("{path}{key}"));
            }
            if text.contains(&format!("{key}: {{")) {
                walk(text, inner, &format!("{path}{key}."), missing);
            }
            if text.contains(&format!("{key}: [{{"))
                && let Some(first) = inner.as_array().and_then(|items| items.first())
            {
                walk(text, first, &format!("{path}{key}[0]."), missing);
            }
        }
    }
    let Some(schema) = crate::schema::method(method) else {
        panic!("{method} is not a schema method");
    };
    let mut missing = Vec::new();
    walk(schema.result, answer, "", &mut missing);
    assert!(
        missing.is_empty(),
        "{method} answers with {missing:?}, which its schema result does not name: {}",
        schema.result
    );
}

/// The dispatcher's parameter gate: `params` is an object or absent, and every limit holds.
/// Host methods without a registered contract receive the default caps only.
pub(crate) fn check_params(contract: Option<&MethodContract>, params: &Value) -> Result<(), RpcError> {
    let object = match params {
        Value::Null => return Ok(()),
        Value::Object(object) => object,
        other => {
            return Err(limit_error(
                format!("`params` must be a JSON object, not {}", json_type(other)),
                None,
            ));
        }
    };
    let limits = contract.map_or(&[][..], |c| c.limits);
    for limit in limits {
        if let Some(why) = object.get(limit.param()).and_then(|value| limit.violation(value)) {
            return Err(limit_error(why, Some(limit)));
        }
    }
    for (name, value) in object {
        let has = |items: bool| {
            limits.iter().any(|limit| {
                limit.param() == name.as_str()
                    && matches!(
                        (limit, items),
                        (Limit::Items { .. }, true) | (Limit::Bytes { .. }, false)
                    )
            })
        };
        match value {
            Value::Array(list) if list.len() > DEFAULT_MAX_ITEMS && !has(true) => {
                return Err(limit_error(
                    format!("`{name}` holds {} items; at most {DEFAULT_MAX_ITEMS}", list.len()),
                    None,
                ));
            }
            Value::String(text) if text.len() > DEFAULT_MAX_BYTES && !has(false) => {
                return Err(limit_error(
                    format!("`{name}` is {} bytes; at most {DEFAULT_MAX_BYTES}", text.len()),
                    None,
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn limit_error(message: String, limit: Option<&Limit>) -> RpcError {
    let mut data = json!({
        "code": "F0002",
        "hint": "agent.schema lists each method's params and limits",
    });
    if let (Some(limit), Some(map)) = (limit, data.as_object_mut()) {
        map.insert("limit".into(), limit.to_json());
    }
    RpcError {
        code: protocol::INVALID_PARAMS,
        message,
        data: Some(data),
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Whether `code` has the shape of a protocol error code: `F` and four digits, or lowercase
/// ASCII letters, digits and `_` starting with a letter.
pub fn is_error_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    let f_code = bytes.len() == 5 && bytes[0] == b'F' && bytes[1..].iter().all(u8::is_ascii_digit);
    let snake = bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_');
    f_code || snake
}

/// Contracts for the editor host's shared-history and recovery methods.
/// These are published only when the host registers the matching schemas.
pub const EDITOR_HISTORY_CONTRACTS: &[MethodContract] = &[
    c("editor.history", "{}", &[], &[]),
    c(
        "editor.history_jump",
        r#"{"index":0}"#,
        &["history_index", "transaction_active", "transaction_owner"],
        &[],
    ),
    c(
        "editor.transaction_begin",
        r#"{"label":"Arrange scene"}"#,
        &["transaction_nested", "transaction_owner"],
        &[Limit::Bytes {
            param: "label",
            max: 1024,
        }],
    ),
    c(
        "editor.transaction_commit",
        "{}",
        &["transaction_missing", "transaction_owner"],
        &[],
    ),
    c(
        "editor.transaction_abort",
        "{}",
        &["transaction_missing", "transaction_owner"],
        &[],
    ),
    c("editor.safe_mode_state", "{}", &[], &[]),
    c(
        "editor.safe_mode_restart",
        "{}",
        &["safe_mode_inactive", "transaction_owner"],
        &[],
    ),
];

// Editor-host metadata only (ADR-7842): no editor dependency or runtime handler registration.
const EDITOR_COLLAB_CONTRACTS: &[MethodContract] = &[
    c(
        "editor.collab_host",
        r#"{ "action": "start", "address": "127.0.0.1:48100", "name": "Avery" }"#,
        &[
            "collab_bad_address",
            "collab_bad_params",
            "collab_busy",
            "collab_full",
            "collab_io",
            "collab_not_host",
            "collab_not_pending",
        ],
        &[bytes("name", 64), bytes("address", 128)],
    ),
    c(
        "editor.collab_join",
        r#"{ "address": "127.0.0.1:48100", "name": "Morgan" }"#,
        &[
            "collab_bad_address",
            "collab_bad_params",
            "collab_busy",
            "collab_io",
        ],
        &[bytes("name", 64), bytes("address", 128)],
    ),
    c("editor.collab_leave", r#"{}"#, &["collab_bad_params"], &[]),
    c("editor.collab_participants", r#"{}"#, &["collab_bad_params"], &[]),
    c(
        "editor.collab_jump",
        r#"{ "participant": 2 }"#,
        &["collab_bad_params", "collab_unknown_participant"],
        &[],
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::METHODS;

    /// Whether `value` has the schema kind `kind` (`"number"`, `"integer"`, `"string"`, ...).
    fn has_kind(value: &Value, kind: &str) -> bool {
        match kind {
            "number" => value.is_number(),
            "integer" => value.is_i64() || value.is_u64(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => true,
        }
    }

    #[test]
    fn every_schema_method_has_exactly_one_contract() {
        let mut methods: Vec<&str> = METHODS.iter().map(|m| m.name).collect();
        methods.sort_unstable();
        let mut contracts: Vec<&str> = CONTRACTS.iter().map(|c| c.method).collect();
        contracts.sort_unstable();
        let before = contracts.len();
        contracts.dedup();
        assert_eq!(contracts.len(), before, "one contract per method");
        assert_eq!(
            contracts, methods,
            "CONTRACTS and schema::METHODS name the same methods"
        );
    }

    #[test]
    fn examples_match_the_schema() {
        let mut problems = Vec::new();
        for contract in CONTRACTS {
            let Some(schema) = crate::schema::method(contract.method) else {
                continue;
            };
            let example = match serde_json::from_str::<Value>(contract.example) {
                Ok(Value::Object(object)) => object,
                Ok(other) => {
                    problems.push(format!(
                        "{}: example is {}, not an object",
                        contract.method,
                        json_type(&other)
                    ));
                    continue;
                }
                Err(error) => {
                    problems.push(format!("{}: example is not JSON: {error}", contract.method));
                    continue;
                }
            };
            for (name, value) in &example {
                match schema.params.iter().find(|p| p.name == name.as_str()) {
                    None => problems.push(format!("{}: example names undeclared `{name}`", contract.method)),
                    Some(param) if !has_kind(value, param.kind) => problems.push(format!(
                        "{}: example `{name}` is {}, schema says {}",
                        contract.method,
                        json_type(value),
                        param.kind
                    )),
                    Some(_) => {}
                }
            }
            for param in schema.params.iter().filter(|p| p.required) {
                if !example.contains_key(param.name) {
                    problems.push(format!(
                        "{}: example lacks required `{}`",
                        contract.method, param.name
                    ));
                }
            }
            if let Err(error) = check_params(Some(contract), &Value::Object(example)) {
                problems.push(format!(
                    "{}: example breaks a limit: {}",
                    contract.method, error.message
                ));
            }
        }
        assert!(problems.is_empty(), "contract examples:\n{}", problems.join("\n"));
    }

    /// `render.sample_baked_gi` publishes a point object the handler accepts, and `F0003`
    /// for a missing renderer or volume. `level.sample_baked_gi` keeps the bare array.
    #[test]
    fn render_sample_baked_gi_example_is_a_point_object_and_lists_not_found() {
        let render = contract("render.sample_baked_gi").expect("contract");
        assert!(render.errors.contains(&"F0003"), "{:?}", render.errors);
        let example = render.example_json();
        let points = example["points"].as_array().expect("points");
        assert!(
            points
                .iter()
                .all(|point| point.get("p").and_then(Value::as_array).is_some()),
            "{points:?}"
        );
        let level = contract("level.sample_baked_gi").expect("level contract");
        let level_example = level.example_json();
        let bare = level_example["points"][0].as_array().expect("bare array");
        assert_eq!(bare.len(), 3);
    }

    #[test]
    fn errors_are_known_sorted_codes() {
        let mut problems = Vec::new();
        for contract in CONTRACTS {
            let mut sorted = contract.errors.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            if sorted != contract.errors {
                problems.push(format!("{}: errors are not sorted and unique", contract.method));
            }
            for code in contract.errors {
                if !is_error_code(code) {
                    problems.push(format!(
                        "{}: `{code}` is not a protocol error code",
                        contract.method
                    ));
                }
                if UNIVERSAL_ERRORS.contains(code) {
                    problems.push(format!(
                        "{}: `{code}` is universal; do not list it",
                        contract.method
                    ));
                }
                if crate::discovery::errors_explain(&json!({ "code": code })).is_err() {
                    problems.push(format!(
                        "{}: errors.explain does not know `{code}`",
                        contract.method
                    ));
                }
            }
        }
        assert!(problems.is_empty(), "contract errors:\n{}", problems.join("\n"));
    }

    #[test]
    fn limits_name_declared_params_of_the_right_kind() {
        let mut problems = Vec::new();
        for contract in CONTRACTS {
            let Some(schema) = crate::schema::method(contract.method) else {
                continue;
            };
            for limit in contract.limits {
                let Some(param) = schema.params.iter().find(|p| p.name == limit.param()) else {
                    problems.push(format!(
                        "{}: limit on undeclared `{}`",
                        contract.method,
                        limit.param()
                    ));
                    continue;
                };
                let fits = match limit {
                    Limit::Range { min, max, .. } => {
                        min <= max && matches!(param.kind, "number" | "integer" | "array" | "any")
                    }
                    Limit::Items { max, .. } => *max > 0 && matches!(param.kind, "array" | "any"),
                    Limit::Bytes { max, .. } => *max > 0 && matches!(param.kind, "string" | "any"),
                };
                if !fits {
                    problems.push(format!(
                        "{}: {} does not fit a {} param",
                        contract.method,
                        limit.describe(),
                        param.kind
                    ));
                }
            }
        }
        assert!(problems.is_empty(), "contract limits:\n{}", problems.join("\n"));
    }

    #[test]
    fn multi_frame_and_whole_world_calls_are_heavy() {
        // The class only adds a budget on top of the mutating one. Multi-frame captures and
        // whole-world walks must be heavy so a loop of them cannot stall the frame.
        for name in [
            "seeing.capture_sequence",
            "seeing.contact_sheet",
            "scene.serialize",
        ] {
            assert_eq!(contract(name).map(|c| c.cost), Some(Cost::Heavy), "{name}");
        }
    }

    #[test]
    fn check_params_refuses_non_objects_and_oversized_values() {
        assert!(check_params(None, &Value::Null).is_ok());
        assert!(check_params(None, &json!({})).is_ok());
        for bad in [json!([1]), json!("x"), json!(5), json!(true)] {
            let error = check_params(None, &bad).expect_err("non-object params");
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{bad}");
        }
        let long = "x".repeat(DEFAULT_MAX_BYTES + 1);
        assert!(check_params(None, &json!({ "text": long })).is_err());
        assert!(check_params(None, &json!({ "text": "x".repeat(DEFAULT_MAX_BYTES) })).is_ok());
        let many = vec![Value::Null; DEFAULT_MAX_ITEMS + 1];
        assert!(check_params(None, &json!({ "list": many })).is_err());
        assert!(check_params(None, &json!({ "list": vec![Value::Null; DEFAULT_MAX_ITEMS] })).is_ok());
    }

    #[test]
    fn pacing_simulate_cap_fps_matches_the_runner() {
        let pacing = contract("pacing.simulate").expect("contract");
        let accepted = json!({ "refresh_hz": 60, "frame_ms": 2, "frames": 120, "cap_fps": 2000 });
        assert!(
            check_params(Some(pacing), &accepted).is_ok(),
            "2000 is inside 1..=10000"
        );
        let over = json!({ "refresh_hz": 60, "frame_ms": 2, "frames": 120, "cap_fps": 10001 });
        let err = check_params(Some(pacing), &over).expect_err("above the runner max");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(|code| code.as_str()),
            Some("F0002")
        );
        assert!(err.message.contains("10000"), "{err:?}");
    }

    #[test]
    fn world_pick_max_distance_zero_is_outside_the_published_range() {
        let pick = contract("world.pick").expect("contract");
        let zero = json!({ "origin": [0.0, 50.0, 0.0], "direction": [0.0, -1.0, 0.0], "max_distance": 0 });
        let err = check_params(Some(pick), &zero).expect_err("0 is not a length");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(|code| code.as_str()),
            Some("F0002")
        );
        let tiny = json!({ "origin": [0.0, 50.0, 0.0], "direction": [0.0, -1.0, 0.0], "max_distance": 1e-6 });
        assert!(check_params(Some(pick), &tiny).is_ok());
    }

    #[test]
    fn a_contract_limit_replaces_the_default_cap_and_reports_itself() {
        const WIDE: MethodContract = c(
            "test.wide",
            "{}",
            &[],
            &[
                items("list", DEFAULT_MAX_ITEMS * 2),
                range("n", 1.0, 10.0),
                bytes("s", 4),
            ],
        );
        let many = vec![Value::Null; DEFAULT_MAX_ITEMS + 1];
        assert!(check_params(Some(&WIDE), &json!({ "list": many })).is_ok());
        assert!(check_params(Some(&WIDE), &json!({ "n": 10 })).is_ok());
        assert!(
            check_params(Some(&WIDE), &json!({ "n": "not a number" })).is_ok(),
            "type errors are the handler's"
        );
        assert!(check_params(Some(&WIDE), &json!({ "n": [[1, 2], [3, 10]] })).is_ok());
        let nested = check_params(Some(&WIDE), &json!({ "n": [[1, 2], [3, 11]] }))
            .expect_err("a nested number is out");
        assert_eq!(nested.data.unwrap_or_default()["limit"]["param"], "n");
        for (params, param) in [
            (json!({ "n": 0 }), "n"),
            (json!({ "n": 10.5 }), "n"),
            (json!({ "n": u64::MAX }), "n"),
            (json!({ "s": "hello" }), "s"),
        ] {
            let error = check_params(Some(&WIDE), &params).expect_err("over the limit");
            assert_eq!(error.code, protocol::INVALID_PARAMS);
            let data = error.data.unwrap_or_default();
            assert_eq!(data["code"], "F0002");
            assert_eq!(data["limit"]["param"], param, "{params}");
        }
    }

    #[test]
    fn error_code_shapes() {
        for good in ["F0002", "F0019", "unknown_entity", "ui_bad_flag", "a1"] {
            assert!(is_error_code(good), "{good}");
        }
        for bad in ["", "F002", "F00021", "Unknown", "bad-code", "_x", "1x", "é"] {
            assert!(!is_error_code(bad), "{bad}");
        }
    }

    /// ADR-1809: every built-in extension method has one contract in [`EXTENSION_CONTRACTS`],
    /// whose example fits the extension's schema row and whose codes are well formed.
    #[test]
    fn every_builtin_extension_method_has_one_extension_contract() {
        let mut world = regia_app::prelude::World::new();
        crate::install_builtin_extensions(&mut world);
        let extension = world.resource::<crate::extension::AgentExtension>();
        let mut methods = crate::builtin_extension_methods();
        methods.sort_unstable();
        let mut contracts: Vec<&str> = EXTENSION_CONTRACTS.iter().map(|c| c.method).collect();
        contracts.sort_unstable();
        assert_eq!(
            methods, contracts,
            "EXTENSION_CONTRACTS names exactly the built-in extension methods"
        );
        assert!(methods.contains(&"curves.set_curve") && methods.contains(&"physics.cloth_stats"));
        let mut problems = Vec::new();
        for contract in EXTENSION_CONTRACTS {
            if crate::schema::method(contract.method).is_some() {
                problems.push(format!("{}: also a schema method", contract.method));
            }
            let Some(schema) = extension.method_schema(contract.method) else {
                problems.push(format!("{}: not an extension method", contract.method));
                continue;
            };
            match serde_json::from_str::<Value>(contract.example) {
                Ok(Value::Object(example)) => {
                    for key in example.keys() {
                        if !schema.params.iter().any(|p| p.name == key.as_str()) {
                            problems.push(format!("{}: example names undeclared `{key}`", contract.method));
                        }
                    }
                    for param in schema.params.iter().filter(|p| p.required) {
                        if !example.contains_key(param.name) {
                            problems.push(format!("{}: example lacks `{}`", contract.method, param.name));
                        }
                    }
                }
                _ => problems.push(format!("{}: example is not a JSON object", contract.method)),
            }
            let mut sorted = contract.errors.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            if sorted != contract.errors {
                problems.push(format!("{}: errors are not sorted and unique", contract.method));
            }
            for code in contract.errors {
                if !is_error_code(code) || UNIVERSAL_ERRORS.contains(code) {
                    problems.push(format!("{}: bad code `{code}`", contract.method));
                }
            }
        }
        assert!(
            problems.is_empty(),
            "extension contracts:\n{}",
            problems.join("\n")
        );
        assert_eq!(
            contract("curves.set_curve").map(|c| c.method),
            Some("curves.set_curve"),
            "the lookup falls back to the extension table"
        );
    }

    /// ADR-10356: the tool-side `release.*` contracts are host data. The runtime neither links
    /// the editor nor the pipeline crate, so the rows are checked here against what the runtime
    /// does know: the code shapes, `errors.explain`, and that each method left the runtime schema
    /// and the built-in extension table.
    #[test]
    fn release_host_contracts_are_well_formed_and_outside_the_runtime_surface() {
        let mut registered: Vec<&str> = RELEASE_HOST_CONTRACTS
            .iter()
            .map(|contract| contract.method)
            .collect();
        registered.sort_unstable();
        let unique = registered.len();
        registered.dedup();
        assert_eq!(registered.len(), unique, "one row per method");
        assert_eq!(
            registered,
            [
                "release.diff",
                "release.sign_status",
                "release.upload_dry_run",
                "release.verify",
            ],
            "the four tool-side release methods and no other"
        );
        let mut problems = Vec::new();
        for contract in RELEASE_HOST_CONTRACTS {
            if !serde_json::from_str::<Value>(contract.example).is_ok_and(|value| value.is_object()) {
                problems.push(format!("{}: example is not a JSON object", contract.method));
            }
            let mut sorted = contract.errors.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            if sorted != contract.errors {
                problems.push(format!("{}: errors are not sorted and unique", contract.method));
            }
            for code in contract.errors {
                if !is_error_code(code) || UNIVERSAL_ERRORS.contains(code) {
                    problems.push(format!("{}: bad code `{code}`", contract.method));
                }
                if crate::discovery::errors_explain(&json!({ "code": code })).is_err() {
                    problems.push(format!(
                        "{}: errors.explain does not know `{code}`",
                        contract.method
                    ));
                }
            }
            if crate::schema::method(contract.method).is_some() {
                problems.push(format!("{}: is still a runtime schema method", contract.method));
            }
            if EXTENSION_CONTRACTS.iter().any(|c| c.method == contract.method) {
                problems.push(format!("{}: is not a built-in extension method", contract.method));
            }
        }
        assert!(
            problems.is_empty(),
            "release host contracts:\n{}",
            problems.join("\n")
        );
        assert_eq!(
            contract("release.verify").map(|c| c.method),
            Some("release.verify"),
            "the lookup reaches the host table"
        );
    }
}
