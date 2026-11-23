// SPDX-License-Identifier: MPL-2.0
//! `ai.debug_layers` and `ai.debug_layers_set` (ADR-8463, `book/agents/ai.md`).
//!
//! The get reads [`regia_ai::AiDebugDraw`]. The set replaces only the fields the call names, and
//! writes nothing when a layer name or a filter name is unknown. Neither call enables the
//! `ai_debug` gizmo group: `debug.set_gizmos { group: "ai_debug", enabled: true }` does that.

use regia_ai::debug_gizmos::{
    AiDebugDraw, AiDebugFilter, AiDebugLayers, MAX_WATCH_KEY_BYTES, MAX_WATCH_KEYS,
};
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

fn unknown_layer(name: &str) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        "unknown_layer",
        format!("unknown AI debug layer `{name}`"),
        "layers are bt, state_tree, goap, squad, formation, move_to, blackboard",
    )
}

fn bad_filter(name: &str) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        "bad_filter",
        format!("unknown AI debug filter `{name}`"),
        "filter is all, selected, or radius",
    )
}

fn snapshot(draw: &AiDebugDraw) -> Value {
    json!({
        "layers": draw.layers.enabled_names().collect::<Vec<_>>(),
        "filter": draw.filter.name(),
        "radius": draw.radius,
        "label_cap": draw.label_cap,
        "dropped": draw.dropped,
        "watch_keys": draw.watch_keys,
        "selected": draw.selected.map(Entity::to_bits),
    })
}

/// `ai.debug_layers`: the overlay settings. Inserts the default resource when the plugin did not.
pub fn debug_layers(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let draw = world.get_resource_or_insert_with(AiDebugDraw::default);
    Ok(snapshot(&draw))
}

/// `ai.debug_layers_set`: replace the named fields. Unknown names change nothing.
pub fn debug_layers_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let layers = match crate::params::opt_array(params, "layers")? {
        None => None,
        Some(values) => {
            let mut layers = AiDebugLayers::default();
            for value in values {
                let Some(name) = value.as_str() else {
                    return Err(RpcError::invalid_params("`layers` must be an array of strings"));
                };
                if !layers.enable(name) {
                    return Err(unknown_layer(name));
                }
            }
            Some(layers)
        }
    };
    let filter = match crate::params::opt_str(params, "filter")? {
        None => None,
        Some(name) => Some(AiDebugFilter::parse(name).ok_or_else(|| bad_filter(name))?),
    };
    let radius = match crate::params::opt_f64(params, "radius")? {
        None => None,
        Some(value) => {
            let radius = value as f32;
            if !value.is_finite() || value < 0.0 || !radius.is_finite() {
                return Err(RpcError::invalid_params("`radius` must be a finite number >= 0"));
            }
            Some(radius)
        }
    };
    let label_cap = match crate::params::opt_u64(params, "label_cap")? {
        None => None,
        Some(value) => Some(
            u32::try_from(value)
                .map_err(|_| RpcError::invalid_params("`label_cap` must fit in a 32-bit count"))?,
        ),
    };
    let selected = match params.get("selected") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => {
            let Some(bits) = value.as_u64() else {
                return Err(RpcError::invalid_params("`selected` must be an entity id"));
            };
            let Some(entity) = Entity::try_from_bits(bits) else {
                return Err(RpcError::invalid_params("`selected` must be an entity id"));
            };
            Some(Some(entity))
        }
    };
    let watch_keys = match params.get("watch_keys") {
        None | Some(Value::Null) => None,
        Some(Value::Array(keys)) => {
            if keys.len() > MAX_WATCH_KEYS {
                return Err(RpcError::invalid_params(format!(
                    "`watch_keys` holds at most {MAX_WATCH_KEYS} keys"
                )));
            }
            let mut stored = Vec::with_capacity(keys.len());
            for key in keys {
                let Some(text) = key.as_str() else {
                    return Err(RpcError::invalid_params(
                        "`watch_keys` must be an array of strings",
                    ));
                };
                if text.len() > MAX_WATCH_KEY_BYTES {
                    return Err(RpcError::invalid_params(format!(
                        "`watch_keys` entries are at most {MAX_WATCH_KEY_BYTES} bytes"
                    )));
                }
                stored.push(text.to_string());
            }
            Some(stored)
        }
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`watch_keys` must be an array of strings",
            ));
        }
    };
    let mut draw = world.get_resource_or_insert_with(AiDebugDraw::default);
    if let Some(layers) = layers {
        draw.layers = layers;
    }
    if let Some(filter) = filter {
        draw.filter = filter;
    }
    if let Some(radius) = radius {
        draw.radius = radius;
    }
    if let Some(label_cap) = label_cap {
        draw.label_cap = label_cap;
    }
    if let Some(selected) = selected {
        draw.selected = selected;
    }
    if let Some(watch_keys) = watch_keys {
        draw.watch_keys = watch_keys;
    }
    Ok(snapshot(&draw))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::dispatch::{AgentInbox, RateLimit, process_requests};
    use crate::playtest::AgentTimeControl;
    use crate::protocol::Request;
    use crate::safety::{AgentConfig, AgentPermissions};
    use crate::server::Inbound;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(AgentPermissions {
                config: AgentConfig {
                    rate_limit_per_second: 10_000,
                    heavy_rate_limit_per_second: 10_000,
                    ..AgentConfig::default()
                },
                source: None,
            })
            .init_resource::<crate::safety::AuditLog>()
            .init_resource::<crate::safety::AgentSession>()
            .init_resource::<AgentTimeControl>()
            .init_resource::<RateLimit>()
            .init_resource::<AppTypeRegistry>();
        app
    }

    fn call(app: &mut App, method: &str, params: Value) -> Value {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        inbox_tx
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply: tx,
            })
            .expect("queued");
        app.world_mut().insert_resource(AgentInbox(inbox_rx));
        process_requests(app.world_mut());
        drop(inbox_tx);
        let response = rx.try_recv().expect("answered on this frame");
        serde_json::to_value(response).expect("response json")
    }

    fn code(body: &Value) -> Option<&str> {
        body["error"]["data"]["code"].as_str()
    }

    #[test]
    fn ai_debug_layers_set_round_trip() {
        let mut app = app();
        let got = call(&mut app, "ai.debug_layers", json!({}));
        assert_eq!(got["result"]["layers"], json!([]));
        assert_eq!(got["result"]["filter"], "all");
        assert_eq!(got["result"]["radius"], 20.0);
        assert_eq!(got["result"]["label_cap"], 64);
        assert_eq!(got["result"]["dropped"], 0);
        assert_eq!(got["result"]["watch_keys"], json!([]));
        assert!(got["result"]["selected"].is_null());

        let set = call(
            &mut app,
            "ai.debug_layers_set",
            json!({ "layers": ["bt", "squad"], "filter": "radius", "radius": 12.5 }),
        );
        assert_eq!(set["result"]["layers"], json!(["bt", "squad"]));
        assert_eq!(set["result"]["filter"], "radius");
        assert_eq!(set["result"]["radius"], 12.5);

        let refused = call(&mut app, "ai.debug_layers_set", json!({ "layers": ["nope"] }));
        assert_eq!(code(&refused), Some("unknown_layer"));
        let refused = call(&mut app, "ai.debug_layers_set", json!({ "filter": "nearby" }));
        assert_eq!(code(&refused), Some("bad_filter"));

        let again = call(&mut app, "ai.debug_layers", json!({}));
        assert_eq!(again["result"]["layers"], json!(["bt", "squad"]));
        assert_eq!(again["result"]["filter"], "radius");
        assert_eq!(again["result"]["radius"], 12.5);
    }
}
