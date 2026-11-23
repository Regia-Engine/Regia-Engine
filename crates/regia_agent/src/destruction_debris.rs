// SPDX-License-Identifier: MPL-2.0
//! `physics.debris_budget`: the secondary rigid-debris spawn cap
//! (`regia_framework::destruction_debris`, book `agents/destruction-debris.md`, ADR-3929).
//!
//! The method does not spawn bodies. A caller that has a granted count spawns that many.

use regia_app::prelude::*;
use regia_framework::destruction_debris::{DEBRIS_BUDGET_BAD_CAP, DebrisBudget};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn bad_cap(which: &str) -> RpcError {
    RpcError::new(
        crate::protocol::INVALID_PARAMS,
        DEBRIS_BUDGET_BAD_CAP,
        format!("`{which}` must be a finite integer from 1 to 4294967295"),
        "see book/agents/destruction-debris.md",
    )
}

fn not_installed() -> RpcError {
    RpcError::not_found(
        "no debris budget in this app",
        "call physics.debris_budget with per_break and per_frame before a push",
    )
}

/// A present cap. `None` when the key is absent or null. A present value that is
/// not a finite integer of at least 1 is [`DEBRIS_BUDGET_BAD_CAP`].
fn read_cap(params: &Value, key: &str) -> Result<Option<f64>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            let value = number.as_f64().ok_or_else(|| bad_cap(key))?;
            regia_framework::destruction_debris::positive_cap(value).map_err(|_| bad_cap(key))?;
            Ok(Some(value))
        }
        Some(_) => Err(bad_cap(key)),
    }
}

fn read_bool(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be true or false"))),
    }
}

fn read_break_id(params: &Value) -> Result<Option<u64>, RpcError> {
    match params.get("break_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params("`break_id` must be an integer from 0")),
        Some(_) => Err(RpcError::invalid_params("`break_id` must be an integer from 0")),
    }
}

fn read_count(params: &Value) -> Result<Option<u32>, RpcError> {
    match params.get("count") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            let value = number
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`count` must be an integer from 0"))?;
            u32::try_from(value)
                .map(Some)
                .map_err(|_| RpcError::invalid_params("`count` must be an integer from 0 to 4294967295"))
        }
        Some(_) => Err(RpcError::invalid_params("`count` must be an integer from 0")),
    }
}

fn report(budget: Option<&DebrisBudget>, break_id: Option<u64>, spawned: Option<u32>) -> Value {
    let Some(budget) = budget else {
        return json!({
            "installed": false,
            "per_break": Value::Null,
            "per_frame": Value::Null,
            "frame_spawned": 0,
            "frame_remaining": 0,
            "dropped": 0,
            "spawned": Value::Null,
            "break_id": Value::Null,
            "break_spawned": Value::Null,
            "digest": "0",
        });
    };
    json!({
        "installed": true,
        "per_break": budget.per_break_cap(),
        "per_frame": budget.per_frame_cap(),
        "frame_spawned": budget.frame_spawned(),
        "frame_remaining": budget.frame_remaining(),
        "dropped": budget.dropped(),
        "spawned": spawned,
        "break_id": break_id,
        "break_spawned": break_id.map(|id| budget.break_spawned(id)),
        "digest": budget.digest().to_string(),
    })
}

/// `physics.debris_budget { per_break?, per_frame?, reset_frame?, break_id?, count? }`.
///
/// A call is checked before anything changes. Caps are applied, then `reset_frame`,
/// then one push when both `break_id` and `count` are set.
pub(crate) fn debris_budget(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let per_break = read_cap(params, "per_break")?;
    let per_frame = read_cap(params, "per_frame")?;
    let reset_frame = read_bool(params, "reset_frame")?.unwrap_or(false);
    let break_id = read_break_id(params)?;
    let count = read_count(params)?;
    match (break_id, count) {
        (None, None) | (Some(_), Some(_)) => {}
        (Some(_), None) => {
            return Err(RpcError::invalid_params(
                "`count` is required when `break_id` is set",
            ));
        }
        (None, Some(_)) => {
            return Err(RpcError::invalid_params(
                "`break_id` is required when `count` is set",
            ));
        }
    }

    let mut budget = match world.get_resource::<DebrisBudget>().cloned() {
        Some(mut budget) => {
            if per_break.is_some() || per_frame.is_some() {
                let next_break = per_break.unwrap_or_else(|| f64::from(budget.per_break_cap()));
                let next_frame = per_frame.unwrap_or_else(|| f64::from(budget.per_frame_cap()));
                budget.set_caps(next_break, next_frame).map_err(|_| {
                    bad_cap(if per_break.is_some() {
                        "per_break"
                    } else {
                        "per_frame"
                    })
                })?;
            }
            budget
        }
        None => match (per_break, per_frame) {
            (None, None) => {
                if break_id.is_some() {
                    return Err(not_installed());
                }
                return Ok(report(None, None, None));
            }
            (Some(per_break), Some(per_frame)) => {
                DebrisBudget::try_new(per_break, per_frame).map_err(|_| bad_cap("per_break"))?
            }
            (Some(_), None) => {
                return Err(RpcError::invalid_params(
                    "set `per_frame` as well when installing a debris budget",
                ));
            }
            (None, Some(_)) => {
                return Err(RpcError::invalid_params(
                    "set `per_break` as well when installing a debris budget",
                ));
            }
        },
    };

    if reset_frame {
        budget.reset_frame();
    }
    let spawned = match (break_id, count) {
        (Some(id), Some(requested)) => Some(budget.push(id, requested)),
        _ => None,
    };
    let body = report(Some(&budget), break_id, spawned);
    world.insert_resource(budget);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(error: &RpcError) -> Option<&str> {
        error.data.as_ref().and_then(|data| data["code"].as_str())
    }

    fn install(world: &mut World) {
        debris_budget(world, &json!({ "per_break": 4, "per_frame": 6 })).expect("install");
    }

    #[test]
    fn three_pushes_then_a_new_break_on_the_next_frame() {
        let mut world = World::new();
        let absent = debris_budget(&mut world, &json!({})).expect("read");
        assert_eq!(absent["installed"], false);

        install(&mut world);
        let first = debris_budget(&mut world, &json!({ "break_id": 1, "count": 3 })).expect("first");
        let second = debris_budget(&mut world, &json!({ "break_id": 1, "count": 3 })).expect("second");
        let third = debris_budget(&mut world, &json!({ "break_id": 1, "count": 3 })).expect("third");
        assert_eq!(first["spawned"], 3);
        assert_eq!(second["spawned"], 1);
        assert_eq!(third["spawned"], 0);
        assert_eq!(third["dropped"], 5);
        assert_eq!(third["frame_remaining"], 2);

        let next = debris_budget(
            &mut world,
            &json!({ "reset_frame": true, "break_id": 2, "count": 4 }),
        )
        .expect("next frame");
        assert_eq!(next["spawned"], 4);
        assert_eq!(next["frame_spawned"], 4);
        assert_eq!(next["frame_remaining"], 2);
        assert_eq!(next["per_frame"], 6);
        assert_eq!(next["break_spawned"], 4);
        assert_eq!(world.resource::<DebrisBudget>().break_spawned(1), 4);
    }

    #[test]
    fn a_zero_cap_returns_debris_budget_bad_cap_and_keeps_the_counters() {
        let mut world = World::new();
        install(&mut world);
        debris_budget(&mut world, &json!({ "break_id": 1, "count": 3 })).expect("push");
        let before = world.resource::<DebrisBudget>().clone();

        let err = debris_budget(&mut world, &json!({ "per_break": 0 })).expect_err("zero");
        assert_eq!(code(&err), Some(DEBRIS_BUDGET_BAD_CAP));
        assert_eq!(world.resource::<DebrisBudget>(), &before);

        let err = debris_budget(&mut world, &json!({ "per_frame": 0, "break_id": 9, "count": 3 }))
            .expect_err("zero with a push");
        assert_eq!(code(&err), Some("debris_budget_bad_cap"));
        assert_eq!(world.resource::<DebrisBudget>(), &before);

        let err = debris_budget(&mut world, &json!({ "per_break": 4.5 })).expect_err("fraction");
        assert_eq!(code(&err), Some(DEBRIS_BUDGET_BAD_CAP));
        assert_eq!(world.resource::<DebrisBudget>().digest(), before.digest());
    }

    #[test]
    fn two_agent_runs_match() {
        fn run() -> Value {
            let mut world = World::new();
            install(&mut world);
            let spawned = [
                debris_budget(&mut world, &json!({ "break_id": 1, "count": 3 })).expect("a")["spawned"]
                    .clone(),
                debris_budget(&mut world, &json!({ "break_id": 1, "count": 3 })).expect("b")["spawned"]
                    .clone(),
                debris_budget(&mut world, &json!({ "break_id": 1, "count": 3 })).expect("c")["spawned"]
                    .clone(),
            ];
            let dropped = debris_budget(&mut world, &json!({})).expect("read")["dropped"].clone();
            let next = debris_budget(
                &mut world,
                &json!({ "reset_frame": true, "break_id": 2, "count": 4 }),
            )
            .expect("next");
            json!({
                "spawned": spawned,
                "dropped": dropped,
                "next": next["spawned"],
                "digest": next["digest"],
            })
        }
        assert_eq!(run(), run());
    }

    #[test]
    fn a_push_without_a_budget_is_not_found_and_installs_nothing() {
        let mut world = World::new();
        let err = debris_budget(&mut world, &json!({ "break_id": 1, "count": 1 })).expect_err("missing");
        assert_eq!(err.code, crate::protocol::NOT_FOUND);
        assert_eq!(code(&err), Some("F0003"));
        assert!(world.get_resource::<DebrisBudget>().is_none());
    }

    #[test]
    fn debris_budget_acceptance_through_the_protocol_dispatcher() {
        use crate::dispatch::{AgentInbox, process_requests};
        use crate::protocol::{Request, Response};
        use crate::safety::{AgentConfig, AgentPermissions};
        use crate::server::Inbound;

        fn call(world: &mut World, params: Value) -> Response {
            let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
            let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
            inbox_tx
                .send(Inbound {
                    request: Request {
                        jsonrpc: "2.0".into(),
                        id: Some(json!(1)),
                        method: "physics.debris_budget".into(),
                        params,
                    },
                    reply: reply_tx,
                })
                .expect("queue request");
            world.insert_resource(AgentInbox(inbox_rx));
            process_requests(world);
            // Dispatch is synchronous; no clock or sleeping is needed to observe its reply.
            reply_rx.try_recv().expect("dispatcher answered")
        }

        fn run() -> Value {
            let mut app = App::new();
            app.add_plugins(MinimalPlugins)
                .insert_resource(AgentPermissions {
                    config: AgentConfig {
                        rate_limit_per_second: 10_000,
                        ..AgentConfig::default()
                    },
                    source: None,
                })
                .init_resource::<crate::safety::AuditLog>()
                .init_resource::<crate::safety::AgentSession>();
            let world = app.world_mut();
            let installed = call(world, json!({"per_break": 4, "per_frame": 6}));
            assert!(installed.error.is_none(), "{:?}", installed.error);
            for expected in [3, 1, 0] {
                let reply = call(world, json!({"break_id": 1, "count": 3}));
                assert!(reply.error.is_none(), "{:?}", reply.error);
                assert_eq!(reply.result.expect("grant")["spawned"], expected);
            }
            assert_eq!(world.resource::<DebrisBudget>().dropped(), 5);
            let before = world.resource::<DebrisBudget>().clone();
            let invalid = call(
                world,
                json!({"per_frame": 0, "reset_frame": true, "break_id": 2, "count": 4}),
            );
            assert_eq!(
                code(&invalid.error.expect("zero cap refused")),
                Some(DEBRIS_BUDGET_BAD_CAP)
            );
            assert_eq!(world.resource::<DebrisBudget>(), &before);
            let next = call(world, json!({"reset_frame": true, "break_id": 2, "count": 4}));
            assert!(next.error.is_none(), "{:?}", next.error);
            let next = next.result.expect("next frame grant");
            assert_eq!(next["spawned"], 4);
            assert_eq!(next["per_frame"], 6);
            assert_eq!(next["dropped"], 5);
            assert_eq!(world.resource::<DebrisBudget>().break_spawned(1), 4);
            next
        }

        assert_eq!(run(), run());
    }
}
