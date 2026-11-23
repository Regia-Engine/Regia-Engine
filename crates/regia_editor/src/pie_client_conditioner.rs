// SPDX-License-Identifier: MPL-2.0
//! Per-client play-in-editor conditioner presets (ADR-3996, `book/agents/pie-net.md`).
//!
//! Each client stores a preset name. `None` follows the session preset, so a session that never
//! sets a client preset joins every client the way it does today. Names come from
//! `SESSION_CONDITIONER_PRESETS`. The name is applied when that client is added to the loopback
//! session: the session conditioner covers clients added from then on, and a client already in a
//! running session keeps the preset it joined with until the next start. Nothing here is saved.

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_net::session::NetSession;
use regia_net::session::conditioner::{SESSION_CONDITIONER_PRESETS, SessionConditioner};
use regia_net::session::hub::SimInput;
use serde_json::{Value, json};

use crate::pie_net::{PieNet, PieNetError};
use crate::viewport_agent::Handler;

/// Grows or shrinks the per-client slots to `clients`. New slots follow the session preset.
pub(crate) fn resize_slots(slots: &mut Vec<Option<String>>, clients: u32) {
    let n = clients as usize;
    if slots.len() > n {
        slots.truncate(n);
    } else {
        slots.resize(n, None);
    }
}

/// True when `name` is a row of `SESSION_CONDITIONER_PRESETS` that builds a conditioner.
pub(crate) fn preset_known(name: &str) -> bool {
    SESSION_CONDITIONER_PRESETS.contains(&name) && SessionConditioner::preset(name).is_some()
}

/// The preset client `index` joins with: its own name, or the session preset (`off` when empty).
pub(crate) fn resolved_preset(net: &PieNet, index: usize) -> &str {
    match net.client_presets.get(index).and_then(Option::as_deref) {
        Some(name) if !name.is_empty() => name,
        _ if net.preset.is_empty() => "off",
        _ => net.preset.as_str(),
    }
}

fn named(name: &str, seed: u64) -> SessionConditioner {
    let mut conditioner = SessionConditioner::preset(name).unwrap_or_default();
    conditioner.seed = seed;
    conditioner
}

fn host_failed(message: String) -> PieNetError {
    PieNetError {
        code: "pie_net_host_failed",
        message,
    }
}

/// Joins `net.clients` loopback clients. With no per-client name, one add keeps today's path.
/// Otherwise each client is added under its own preset, then the session preset is restored so
/// a later simulated client still follows the session.
pub(crate) fn add_clients(session: &mut NetSession, net: &PieNet) -> Result<(), PieNetError> {
    let n = net.clients as usize;
    let any_explicit = (0..n).any(|i| net.client_presets.get(i).is_some_and(Option::is_some));
    if !any_explicit {
        return session
            .add_loopback_clients(n, SimInput::Idle)
            .map(|_| ())
            .map_err(|e| host_failed(e.to_string()));
    }
    for i in 0..n {
        session.set_conditioner(named(resolved_preset(net, i), net.seed));
        session
            .add_loopback_clients(1, SimInput::Idle)
            .map_err(|e| host_failed(e.to_string()))?;
    }
    session.set_conditioner(net.conditioner());
    Ok(())
}

fn refuse(code: &'static str, message: impl Into<String>, hint: &'static str) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

/// `editor.pie_net_client_conditioner`: the resolved presets of the configured clients.
fn view(net: &PieNet, client: u64) -> Value {
    let presets: Vec<&str> = (0..net.clients as usize)
        .map(|i| resolved_preset(net, i))
        .collect();
    let index = client as usize;
    json!({
        "client": client,
        "preset": presets.get(index).copied().unwrap_or("off"),
        "explicit": net.client_presets.get(index).is_some_and(Option::is_some),
        "session_preset": if net.preset.is_empty() { "off" } else { net.preset.as_str() },
        "presets": presets,
    })
}

/// `editor.pie_net_client_conditioner`.
fn pie_net_client_conditioner(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let client = match params.get("client") {
        Some(v) => v
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`client` must be a whole number"))?,
        None => return Err(RpcError::invalid_params("`client` is required")),
    };
    let preset = match params.get("preset") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| RpcError::invalid_params("`preset` must be a string"))?,
        ),
    };
    let mut net = world.get_resource_or_insert_with(PieNet::default);
    if client >= u64::from(net.clients) {
        return Err(refuse(
            "pie_client_index",
            format!("client index {client} is past the session size {}", net.clients),
            "client is a 0-based index below the configured client count",
        ));
    }
    if let Some(name) = preset {
        if !preset_known(name) {
            return Err(refuse(
                "pie_conditioner_unknown",
                format!("unknown conditioner preset `{name}`"),
                "use a name from SESSION_CONDITIONER_PRESETS: off, lan, good, average, bad, terrible",
            ));
        }
        let index = client as usize;
        let count = net.clients;
        if net.client_presets.len() <= index {
            resize_slots(&mut net.client_presets, count);
        }
        net.client_presets[index] = Some(name.to_owned());
    }
    Ok(view(&net, client))
}

/// The `editor.pie_net_client_conditioner` method.
pub(crate) fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.pie_net_client_conditioner",
            tool: "pie_net_client_conditioner",
            description: "Set or read one play-in-editor client's conditioner preset. The default is the session preset from editor.pie_net_config. A name must be in SESSION_CONDITIONER_PRESETS. Setting client N does not change client M or the session preset. The name applies when the client joins; a running link keeps the preset it joined with until the next start. In memory only.",
            params: &[
                ParamSchema {
                    name: "client",
                    kind: "integer",
                    required: true,
                    description: "0-based client index, below the configured client count.",
                },
                ParamSchema {
                    name: "preset",
                    kind: "string",
                    required: false,
                    description: "off, lan, good, average, bad or terrible. Omit to read the resolved preset.",
                },
            ],
            result: "{ client, preset, explicit, session_preset, presets }",
            mutating: true,
            permission: Permission::Play,
        },
        pie_net_client_conditioner,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::session::conditioner::ShaperSettings;

    use crate::pie_net::PieNetMode;

    fn code(r: Result<Value, RpcError>) -> String {
        r.expect_err("refused")
            .data
            .and_then(|d| d["code"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    fn call(world: &mut World, params: Value) -> Result<Value, RpcError> {
        let (schema, handler) = crate::pie_net::methods()
            .into_iter()
            .find(|(schema, _)| schema.name == "editor.pie_net_client_conditioner")
            .expect("registered PIE method");
        assert_eq!(schema.name, "editor.pie_net_client_conditioner");
        handler(world, &params)
    }

    #[test]
    fn client_preset_defaults_to_the_session_and_setting_one_client_leaves_the_other() {
        let mut world = World::new();
        world.insert_resource(PieNet::default());
        world
            .resource_mut::<PieNet>()
            .configure(Some(PieNetMode::Listen), Some(2), Some("lan"), Some(4))
            .expect("config");
        let read = call(&mut world, json!({ "client": 0 })).expect("read");
        assert_eq!(read["preset"], "lan");
        assert_eq!(read["explicit"], false);
        assert_eq!(read["presets"], json!(["lan", "lan"]));
        let set = call(&mut world, json!({ "client": 1, "preset": "bad" })).expect("set");
        assert_eq!(set["preset"], "bad");
        assert_eq!(set["explicit"], true);
        assert_eq!(set["presets"], json!(["lan", "bad"]));
        assert_eq!(set["session_preset"], "lan");
        assert_eq!(world.resource::<PieNet>().preset, "lan");
        world
            .resource_mut::<PieNet>()
            .configure(Some(PieNetMode::Listen), Some(2), Some("good"), None)
            .expect("session preset");
        let after = call(&mut world, json!({ "client": 0 })).expect("read 0");
        assert_eq!(after["presets"], json!(["good", "bad"]));
        assert_eq!(world.resource::<PieNet>().preset, "good");
        let before = world.resource::<PieNet>().client_presets.clone();
        world
            .resource_mut::<PieNet>()
            .configure(Some(PieNetMode::Listen), Some(2), Some("nope"), None)
            .expect_err("session preset refused");
        assert_eq!(world.resource::<PieNet>().client_presets, before);
        assert_eq!(world.resource::<PieNet>().preset, "good");
    }

    #[test]
    fn unknown_preset_and_a_past_index_change_nothing() {
        let mut world = World::new();
        world.insert_resource(PieNet::default());
        world
            .resource_mut::<PieNet>()
            .configure(Some(PieNetMode::Listen), Some(2), Some("off"), None)
            .expect("config");
        assert_eq!(
            code(call(&mut world, json!({ "client": 1, "preset": "nope" }))),
            "pie_conditioner_unknown"
        );
        assert_eq!(
            code(call(&mut world, json!({ "client": 2, "preset": "bad" }))),
            "pie_client_index"
        );
        assert_eq!(
            code(call(&mut world, json!({ "client": 99 }))),
            "pie_client_index"
        );
        let view = call(&mut world, json!({ "client": 1 })).expect("unchanged");
        assert_eq!(view["presets"], json!(["off", "off"]));
        assert_eq!(view["explicit"], false);
        assert_eq!(world.resource::<PieNet>().preset, "off");
        assert!(
            world
                .resource::<PieNet>()
                .client_presets
                .iter()
                .all(|slot| slot.is_none())
        );
    }

    #[test]
    fn resizing_keeps_surviving_clients_and_new_clients_follow_the_session() {
        let mut world = World::new();
        world.insert_resource(PieNet::default());
        world
            .resource_mut::<PieNet>()
            .configure(Some(PieNetMode::Listen), Some(3), Some("lan"), None)
            .expect("config");
        call(&mut world, json!({ "client": 0, "preset": "good" })).expect("client 0");
        call(&mut world, json!({ "client": 2, "preset": "bad" })).expect("client 2");
        let before = call(&mut world, json!({ "client": 0 })).expect("before");
        assert_eq!(
            code(call(&mut world, json!({ "client": 0, "preset": "unknown" }))),
            "pie_conditioner_unknown"
        );
        assert_eq!(call(&mut world, json!({ "client": 0 })).expect("after"), before);
        world
            .resource_mut::<PieNet>()
            .configure(None, Some(1), None, None)
            .expect("shrink");
        world
            .resource_mut::<PieNet>()
            .configure(None, Some(3), Some("average"), None)
            .expect("grow");
        let view = call(&mut world, json!({ "client": 2 })).expect("new client");
        assert_eq!(view["presets"], json!(["good", "average", "average"]));
        assert_eq!(view["explicit"], false);
    }

    #[test]
    fn bad_preset_on_client_1_delays_more_than_client_0_on_the_same_steps() {
        // The real id from SESSION_CONDITIONER_PRESETS. `off` is loss 0; `bad` is a higher delay.
        let bad_id = "bad";
        assert!(
            SESSION_CONDITIONER_PRESETS.contains(&bad_id),
            "preset id {bad_id} is not in the table: {SESSION_CONDITIONER_PRESETS:?}"
        );
        let off = ShaperSettings::preset("off").expect("off preset");
        let bad = ShaperSettings::preset(bad_id).expect("bad preset");
        assert_eq!(off.loss_pct, 0.0, "client 0's default preset loses nothing");
        assert!(
            bad.latency_ms > off.latency_ms,
            "bad latency {} vs off {}",
            bad.latency_ms,
            off.latency_ms
        );

        let mut net = PieNet::default();
        net.configure(Some(PieNetMode::Listen), Some(2), Some("off"), Some(11))
            .expect("config");
        {
            let mut world = World::new();
            world.insert_resource(net);
            call(&mut world, json!({ "client": 1, "preset": bad_id })).expect("set client 1");
            let untouched = call(&mut world, json!({ "client": 0 })).expect("client 0");
            assert_eq!(untouched["preset"], "off");
            assert_eq!(untouched["explicit"], false);
            assert_eq!(untouched["session_preset"], "off");
            net = world.remove_resource::<PieNet>().expect("net");
        }
        net.start().expect("start");
        assert_eq!(
            net.session().expect("session").conditioner(),
            &net.conditioner(),
            "later joins must still inherit the session default"
        );
        net.step(400).expect("step");
        assert_eq!(net.in_game(), 2, "both clients join on one step count");
        let rows = net.session().expect("session").connection_stats();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "client-1");
        assert_eq!(rows[1].label, "client-2");
        assert_eq!(rows[0].loss_out, 0.0, "client 0 keeps loss 0");
        assert_eq!(rows[0].loss_in, 0.0, "client 0 keeps loss 0");
        assert!(
            rows[0].rtt_ms < 5.0,
            "client 0 stayed on the off preset, rtt {}",
            rows[0].rtt_ms
        );
        assert!(
            rows[1].rtt_ms > rows[0].rtt_ms,
            "client 1 delay {} vs client 0 {} after {} frames",
            rows[1].rtt_ms,
            rows[0].rtt_ms,
            net.frames
        );
        assert!(
            rows[1].rtt_ms >= 100.0,
            "bad preset did not raise client 1 delay: {}",
            rows[1].rtt_ms
        );
    }

    #[test]
    fn a_preset_set_while_running_waits_until_the_next_start() {
        let mut world = World::new();
        world.insert_resource(PieNet::default());
        world
            .resource_mut::<PieNet>()
            .configure(Some(PieNetMode::Listen), Some(2), Some("off"), Some(11))
            .expect("config");
        let mut net = world.remove_resource::<PieNet>().expect("net");
        net.start().expect("start");
        net.step(200).expect("step");
        assert_eq!(net.in_game(), 2);
        world.insert_resource(net);
        let set = call(&mut world, json!({ "client": 1, "preset": "bad" })).expect("stored");
        assert_eq!(set["preset"], "bad");
        assert_eq!(set["explicit"], true);
        let mut net = world.remove_resource::<PieNet>().expect("net");
        net.step(200).expect("still the join preset");
        let rows = net.session().expect("session").connection_stats();
        assert!(rows[0].rtt_ms < 5.0, "client 0 {}", rows[0].rtt_ms);
        assert!(
            rows[1].rtt_ms < 5.0,
            "a running link keeps the preset it joined with, rtt {}",
            rows[1].rtt_ms
        );
        net.start().expect("restart");
        net.step(400).expect("step");
        assert_eq!(net.in_game(), 2);
        let rows = net.session().expect("session").connection_stats();
        assert_eq!(rows[0].loss_out, 0.0);
        assert_eq!(rows[0].loss_in, 0.0);
        assert!(rows[0].rtt_ms < 5.0, "client 0 {}", rows[0].rtt_ms);
        assert!(
            rows[1].rtt_ms >= 100.0,
            "the next start applies bad: {}",
            rows[1].rtt_ms
        );
    }

    #[test]
    fn the_same_per_client_presets_report_the_same_numbers() {
        let run = || {
            let mut world = World::new();
            world.insert_resource(PieNet::default());
            world
                .resource_mut::<PieNet>()
                .configure(Some(PieNetMode::Dedicated), Some(2), Some("off"), Some(3))
                .expect("config");
            call(&mut world, json!({ "client": 1, "preset": "bad" })).expect("set");
            let mut net = world.remove_resource::<PieNet>().expect("net");
            net.start().expect("start");
            net.step(400).expect("step");
            let session = net.session().expect("session");
            (session.connection_stats(), session.bandwidth_view())
        };
        assert_eq!(run(), run());
    }
}
