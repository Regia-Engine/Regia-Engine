// SPDX-License-Identifier: MPL-2.0
//! GPU breadcrumbs on the agent protocol (`book/agents/gpu-breadcrumbs-and-trace.md`,
//! `docs/specs/54-gpu-breadcrumbs-and-trace-markers.md`):
//!
//! - `render.breadcrumbs { frames?, passes?, source? }` reads the live ring, or the report inside
//!   the newest crash bundle.
//! - `render.fault_inject { kind, ... }` (only in builds with the `test-hooks` feature) provokes a
//!   device loss, a stall or an out-of-memory signal through the real loss path.
//!
//! Both are extension methods. `AgentPlugin` installs `render.breadcrumbs` in every agent app
//! ([`install_breadcrumb_read_methods`], through `crate::install_builtin_extensions`, ADR-2307);
//! a test host adds `render.fault_inject` with [`install_breadcrumb_methods`].
//!
//! Error reasons (`data.reason`): `breadcrumbs_off`, `no_bundle`, `unknown_pass`, `test_hooks_off`,
//! `invalid_value`, `unknown_field`.

use regia_app::prelude::*;
use regia_render::breadcrumbs::{self, BreadcrumbReport};
use serde_json::{Value, json};

use crate::extension::AgentExtension;
use crate::protocol::{self, RpcError};
use crate::schema::{MethodSchema, ParamSchema, Permission};

pub(crate) fn error(code: i64, regia_code: &str, reason: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({
            "code": regia_code,
            "reason": reason,
            "hint": "see book/agents/gpu-breadcrumbs-and-trace.md",
        })),
    }
}

pub(crate) fn invalid(reason: &str, message: impl Into<String>) -> RpcError {
    error(protocol::INVALID_PARAMS, "F0002", reason, message)
}

pub(crate) fn not_found(reason: &str, message: impl Into<String>) -> RpcError {
    error(protocol::NOT_FOUND, "F0003", reason, message)
}

fn breadcrumbs_schema() -> MethodSchema {
    MethodSchema {
        name: "render.breadcrumbs",
        tool: "render_breadcrumbs",
        description: "GPU breadcrumbs: the evidence level, submission counters, suspect passes and the newest frames' pass lists.",
        params: &[
            ParamSchema {
                name: "frames",
                kind: "integer",
                required: false,
                description: "newest frames to return (default 4, at most the ring size)",
            },
            ParamSchema {
                name: "passes",
                kind: "boolean",
                required: false,
                description: "include each frame's pass names (default true)",
            },
            ParamSchema {
                name: "source",
                kind: "string",
                required: false,
                description: "`live` (default) reads the ring; `last_bundle` reads the newest crash bundle",
            },
        ],
        result: "{ source, level_number, report: { format_version, level, adapter, lost, counters, suspects, frames, truncated } }",
        mutating: false,
        permission: Permission::Read,
    }
}

#[cfg(feature = "test-hooks")]
fn fault_inject_schema() -> MethodSchema {
    MethodSchema {
        name: "render.fault_inject",
        tool: "render_fault_inject",
        description: "Test builds only: provoke a device loss, a submission stall then loss, or an out-of-memory signal.",
        params: &[
            ParamSchema {
                name: "kind",
                kind: "string",
                required: true,
                description: "`device_lost`, `stall_at_submission` or `oom`",
            },
            ParamSchema {
                name: "submission",
                kind: "integer",
                required: false,
                description: "for `stall_at_submission`: the first submission that stops completing",
            },
            ParamSchema {
                name: "at_frame",
                kind: "integer",
                required: false,
                description: "fire on the first submit of this frame or later (default: the next submit)",
            },
            ParamSchema {
                name: "mode",
                kind: "string",
                required: false,
                description: "`scripted` (default) or `destroy` (Device::destroy on the render device)",
            },
            ParamSchema {
                name: "pass",
                kind: "string",
                required: false,
                description: "for scripted `device_lost`: lose the device inside this pass (a name `render.breadcrumbs` has listed)",
            },
        ],
        result: "{ armed: true, fires_on_frame, pass? }",
        mutating: true,
        permission: Permission::Play,
    }
}

/// Registers `render.breadcrumbs` (and `render.fault_inject` in `test-hooks` builds) on the
/// world's [`AgentExtension`]. Test hosts call this; `AgentPlugin` installs only the read method
/// ([`install_breadcrumb_read_methods`]), so no app gets the fault injector by default.
pub fn install_breadcrumb_methods(world: &mut World) {
    install_breadcrumb_read_methods(world);
    #[cfg(feature = "test-hooks")]
    {
        let mut extension = world
            .get_resource::<AgentExtension>()
            .cloned()
            .unwrap_or_default();
        extension.register(fault_inject_schema(), fault_inject);
        world.insert_resource(extension);
    }
}

/// Registers `render.breadcrumbs` on the world's [`AgentExtension`] (`AgentPlugin` calls this
/// through `crate::install_builtin_extensions`, ADR-2307).
pub fn install_breadcrumb_read_methods(world: &mut World) {
    let mut extension = world
        .get_resource::<AgentExtension>()
        .cloned()
        .unwrap_or_default();
    extension.register(breadcrumbs_schema(), render_breadcrumbs);
    world.insert_resource(extension);
}

fn check_fields(params: &Value, allowed: &[&str]) -> Result<(), RpcError> {
    match params {
        Value::Null => Ok(()),
        Value::Object(map) => match map.keys().find(|k| !allowed.contains(&k.as_str())) {
            Some(key) => Err(invalid(
                "unknown_field",
                format!("unknown field `{key}`; one of {}", allowed.join(", ")),
            )),
            None => Ok(()),
        },
        _ => Err(invalid("invalid_value", "params must be an object")),
    }
}

/// The report inside the newest crash bundle, parsed.
fn last_bundle_report() -> Result<BreadcrumbReport, RpcError> {
    let dir = regia_framework::feedback::crash::crash_dir().ok_or_else(|| {
        not_found(
            "no_bundle",
            "no feedback folder is configured, so there is no crash bundle",
        )
    })?;
    let newest = regia_framework::feedback::list_bundles(&dir)
        .into_iter()
        .filter(|b| b.category == Some(regia_framework::feedback::FeedbackCategory::Crash))
        .max_by_key(|b| b.modified_unix_ms)
        .ok_or_else(|| not_found("no_bundle", "there is no crash bundle yet"))?;
    let entries = regia_framework::feedback::read_bundle(&newest.path).map_err(|e| {
        not_found(
            "no_bundle",
            format!("the newest crash bundle cannot be read: {e}"),
        )
    })?;
    let entry = entries
        .iter()
        .find(|e| e.name == "gpu_breadcrumbs.json")
        .ok_or_else(|| not_found("no_bundle", "the newest crash bundle has no gpu_breadcrumbs.json"))?;
    let text = String::from_utf8_lossy(&entry.data);
    BreadcrumbReport::from_json(&text)
        .map_err(|e| not_found("no_bundle", format!("gpu_breadcrumbs.json does not parse: {e}")))
}

fn render_breadcrumbs(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_fields(params, &["frames", "passes", "source"])?;
    let source = crate::params::opt_str(params, "source")?.unwrap_or("live");
    let passes = match params.get("passes") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(invalid("invalid_value", "`passes` must be a boolean")),
    };
    let frames = match params.get("frames") {
        None | Some(Value::Null) => 4,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=64).contains(n))
            .ok_or_else(|| invalid("invalid_value", "`frames` must be an integer from 1 to 64"))?
            as usize,
    };
    let mut report = match source {
        "live" => {
            let snapshot = breadcrumbs::snapshot();
            if !snapshot.enabled {
                return Err(not_found(
                    "breadcrumbs_off",
                    "breadcrumbs are disabled (BreadcrumbConfig.enabled is false)",
                ));
            }
            BreadcrumbReport::capture(&snapshot, regia_render::gpu_health::device_lost_info().as_ref())
        }
        "last_bundle" => last_bundle_report()?,
        other => {
            return Err(invalid(
                "invalid_value",
                format!("`source` must be `live` or `last_bundle`, not `{other}`"),
            ));
        }
    };
    report.frames.truncate(frames);
    if !passes {
        for f in &mut report.frames {
            f.passes.clear();
            f.submissions.clear();
        }
    }
    Ok(json!({
        "source": source,
        "level_number": report.level.number(),
        "report": serde_json::to_value(&report).map_err(|e| error(protocol::INTERNAL_ERROR, "F0001", "internal", e.to_string()))?,
    }))
}

#[cfg(feature = "test-hooks")]
fn fault_inject(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_render::breadcrumbs::fault::{self, Fault, FaultKind};
    check_fields(params, &["kind", "submission", "at_frame", "mode", "pass"])?;
    let pass = match params.get("pass") {
        None | Some(Value::Null) => None,
        Some(Value::String(p)) => Some(p.clone()),
        Some(_) => return Err(invalid("invalid_value", "`pass` must be a string")),
    };
    let at_frame = match params.get("at_frame") {
        None | Some(Value::Null) => 0,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| invalid("invalid_value", "`at_frame` must be a non-negative integer"))?,
    };
    let mode = crate::params::opt_str(params, "mode")?.unwrap_or("scripted");
    let kind = params.get("kind").and_then(Value::as_str).ok_or_else(|| {
        invalid(
            "invalid_value",
            "`kind` is required: device_lost, stall_at_submission or oom",
        )
    })?;
    if let Some(p) = pass.as_deref() {
        if (kind, mode) != ("device_lost", "scripted") {
            return Err(invalid(
                "invalid_value",
                "`pass` works with `kind: device_lost` in `mode: scripted` only",
            ));
        }
        if !breadcrumbs::snapshot().names.iter().any(|n| n == p) {
            return Err(not_found(
                "unknown_pass",
                format!("no pass named `{p}` has run yet; render.breadcrumbs lists the frame's passes"),
            ));
        }
    }
    let kind = match (kind, mode) {
        ("device_lost", "scripted") => {
            if let Some(p) = pass.as_deref() {
                fault::arm_in_pass(p, at_frame);
                return Ok(json!({ "armed": true, "fires_on_frame": at_frame, "pass": p }));
            }
            FaultKind::DeviceLost
        }
        ("device_lost", "destroy") => {
            let device = world
                .get_resource::<regia_render::RenderDevice>()
                .ok_or_else(|| not_found("test_hooks_off", "there is no render device to destroy"))?;
            FaultKind::Destroy(device.device.clone())
        }
        ("stall_at_submission", "scripted") => {
            let at = crate::params::opt_u64(params, "submission")?
                .filter(|n| *n >= 1)
                .ok_or_else(|| invalid("invalid_value", "`submission` must be an integer from 1"))?;
            FaultKind::StallAtSubmission(at)
        }
        ("oom", "scripted") => FaultKind::Oom,
        ("device_lost" | "stall_at_submission" | "oom", other) => {
            return Err(invalid(
                "invalid_value",
                format!(
                    "`mode` `{other}` does not fit this `kind`; use scripted (or destroy for device_lost)"
                ),
            ));
        }
        (other, _) => {
            return Err(invalid(
                "invalid_value",
                format!("unknown `kind` `{other}`; one of device_lost, stall_at_submission, oom"),
            ));
        }
    };
    fault::arm(Fault { kind, at_frame });
    Ok(json!({ "armed": true, "fires_on_frame": at_frame }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::breadcrumbs::CrumbList;

    fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
        let handler = world
            .resource::<AgentExtension>()
            .method(name)
            .map(|(_, h)| h)
            .expect("registered");
        handler(world, &params)
    }

    fn world() -> World {
        let mut w = World::new();
        install_breadcrumb_methods(&mut w);
        w
    }

    #[test]
    fn registers_a_read_method() {
        let w = world();
        let (schema, _) = w
            .resource::<AgentExtension>()
            .method("render.breadcrumbs")
            .expect("row");
        assert!(!schema.mutating);
        assert_eq!(schema.permission.name(), "read");
        assert_eq!(schema.tool, "render_breadcrumbs");
    }

    #[test]
    fn live_report_has_level_counters_and_frames() {
        let mut w = world();
        let mut list = CrumbList::new();
        drop(breadcrumbs::global().pass(&mut list, "agent_test_pass"));
        breadcrumbs::note_submit(&[&list], 77);
        let r = call(&mut w, "render.breadcrumbs", json!({ "frames": 1 })).expect("ok");
        assert_eq!(r["source"], "live");
        assert!(r.get("level").is_none(), "level lives on report");
        assert!(r.get("frames").is_none(), "frames live on report");
        assert!(
            r.get("format_version").is_none(),
            "format_version lives on report"
        );
        assert_eq!(r["report"]["format_version"], 1);
        assert_eq!(r["report"]["frames"].as_array().map(Vec::len), Some(1));
        let level = r["report"]["level"].as_str().expect("report.level");
        let number = match level {
            "timestamped" => 3,
            "fenced" => 2,
            "recorded" => 1,
            "none" => 0,
            other => panic!("unknown evidence level {other}"),
        };
        assert_eq!(r["level_number"], number);
        assert!(r["report"]["counters"]["submitted"].as_u64().unwrap_or(0) >= 1);
        let no_passes = call(&mut w, "render.breadcrumbs", json!({ "passes": false })).expect("ok");
        assert_eq!(no_passes["report"]["frames"][0]["passes"], json!([]));
    }

    /// `render.fault_inject` exists only in `test-hooks` builds; there it is a play method that
    /// refuses a missing `kind` before arming anything.
    #[test]
    fn fault_inject_is_registered_only_with_test_hooks() {
        let mut w = world();
        let found = w
            .resource::<AgentExtension>()
            .method("render.fault_inject")
            .map(|(schema, _)| (schema.mutating, schema.permission.name()));
        if cfg!(feature = "test-hooks") {
            assert_eq!(found, Some((true, "play")));
            let err = call(&mut w, "render.fault_inject", json!({})).expect_err("kind required");
            assert_eq!(err.data.expect("data")["reason"], "invalid_value");
        } else {
            assert_eq!(found, None);
        }
    }

    /// ADR-2307: `pass` names a pass that ran; an unknown one is `unknown_pass`, and `pass` with
    /// another kind or mode is refused before anything is armed.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn fault_inject_in_a_pass_needs_a_known_pass() {
        let mut w = world();
        let mut list = CrumbList::new();
        drop(breadcrumbs::global().pass(&mut list, "agent_fault_pass"));
        let err = call(
            &mut w,
            "render.fault_inject",
            json!({ "kind": "device_lost", "pass": "never_ran_pass" }),
        )
        .expect_err("unknown");
        let data = err.data.expect("data");
        assert_eq!(
            (data["code"].clone(), data["reason"].clone()),
            (json!("F0003"), json!("unknown_pass"))
        );
        let err = call(
            &mut w,
            "render.fault_inject",
            json!({ "kind": "oom", "pass": "agent_fault_pass" }),
        )
        .expect_err("kind");
        assert_eq!(err.data.expect("data")["reason"], "invalid_value");
        let ok = call(
            &mut w,
            "render.fault_inject",
            json!({ "kind": "device_lost", "pass": "agent_fault_pass", "at_frame": 1_000_000 }),
        )
        .expect("armed");
        assert_eq!(ok["pass"], "agent_fault_pass");
        regia_render::breadcrumbs::fault::disarm();
    }

    #[test]
    fn bad_params_carry_a_reason() {
        let mut w = world();
        for (params, reason) in [
            (json!({ "frames": 0 }), "invalid_value"),
            (json!({ "frames": 65 }), "invalid_value"),
            (json!({ "passes": "yes" }), "invalid_value"),
            (json!({ "source": "cloud" }), "invalid_value"),
            (json!({ "wat": 1 }), "unknown_field"),
        ] {
            let err = call(&mut w, "render.breadcrumbs", params).expect_err("rejected");
            let data = err.data.expect("data");
            assert_eq!(data["code"], "F0002");
            assert_eq!(data["reason"], reason);
        }
    }

    #[test]
    fn last_bundle_without_a_folder_says_no_bundle() {
        let mut w = world();
        // No crash folder is configured in this test process.
        let err = call(&mut w, "render.breadcrumbs", json!({ "source": "last_bundle" })).expect_err("none");
        let data = err.data.expect("data");
        assert_eq!(data["code"], "F0003");
        assert_eq!(data["reason"], "no_bundle");
    }
}
