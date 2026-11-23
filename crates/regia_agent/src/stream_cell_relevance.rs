// SPDX-License-Identifier: MPL-2.0
//! `stream.cell_relevance`: one peer's loaded stream cells, the one-cell margin, and how many
//! entities the loaded-cell filter dropped on the last snapshot.
//!
//! The engine crate installs [`CellRelevanceHost`] when world streaming and the agent are both
//! on. This crate does not depend on that crate. Book page `agents/stream-cell-relevance.md`.
//!
//! Errors are `F0002`. A missing or non-integer `peer` has no `data.reason`. The other reasons
//! are `net_not_server`, `stream_not_enabled`, `stream_cell_filter_off` and `peer_unknown`.

use regia_app::prelude::*;
use regia_net::model::NetMode;
use regia_world::WorldStreaming;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// One peer's loaded-cell relevance, the shape `stream.cell_relevance` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellRelevanceReply {
    /// The peer id the call asked about.
    pub peer: u32,
    /// Loaded stream cells, `"<x>_<z>"`, in cell order.
    pub loaded: Vec<String>,
    /// Cells this far from a loaded cell stay relevant.
    pub margin: i64,
    /// Entities the filter dropped for this peer on the last published snapshot.
    pub dropped: u64,
}

/// Reads one peer's loaded-cell relevance from the world.
pub type CellRelevanceCall = fn(&World, u32) -> Result<CellRelevanceReply, &'static str>;

/// The host's hook. Without it the method answers `stream_cell_filter_off` once streaming is on.
#[derive(Resource, Clone, Copy)]
pub struct CellRelevanceHost {
    call: CellRelevanceCall,
}

impl CellRelevanceHost {
    /// Registers `call`, normally the engine's loaded-cell report.
    #[must_use]
    pub fn new(call: CellRelevanceCall) -> Self {
        Self { call }
    }

    /// Runs the host for `peer`.
    ///
    /// # Errors
    ///
    /// The host's reason, `peer_unknown` when the peer is not connected and has no loaded-cell entry.
    pub fn query(&self, world: &World, peer: u32) -> Result<CellRelevanceReply, &'static str> {
        (self.call)(world, peer)
    }
}

fn f0002(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn reason_message(reason: &str) -> String {
    match reason {
        "net_not_server" => "stream.cell_relevance runs on the server; this process is a client".to_string(),
        "stream_not_enabled" => "no streamed level is open".to_string(),
        "stream_cell_filter_off" => "the loaded-cell relevance filter is not installed".to_string(),
        "peer_unknown" => "no streaming source or connected peer has that id".to_string(),
        other => format!("stream.cell_relevance refused: {other}"),
    }
}

fn reason_hint(reason: &str) -> &'static str {
    match reason {
        "net_not_server" => "call it on the server, the listen host or an offline process",
        "stream_not_enabled" => "open a cooked streamed level (regia_world::open_streamed_level)",
        "stream_cell_filter_off" => {
            "a dedicated server adds StreamCellRelevancePlugin with the server feature; a listen server on a client build adds the plugin itself"
        }
        "peer_unknown" => {
            "net.status lists connected peers; a peer also counts once it owns a streaming source"
        }
        _ => "see book/agents/stream-cell-relevance.md",
    }
}

/// The method's answer for one already-parsed peer id.
///
/// # Errors
///
/// `net_not_server`, `stream_not_enabled`, `stream_cell_filter_off` or `peer_unknown`.
pub fn cell_relevance_peer(world: &World, peer: u32) -> Result<CellRelevanceReply, &'static str> {
    let mode = world.get_resource::<NetMode>().copied().unwrap_or_default();
    if !mode.is_server() {
        return Err("net_not_server");
    }
    if !world.get_resource::<WorldStreaming>().is_some_and(|streaming| {
        streaming
            .levels()
            .any(|level| streaming.settings(level).is_some_and(|settings| settings.enabled))
    }) {
        return Err("stream_not_enabled");
    }
    let Some(host) = world.get_resource::<CellRelevanceHost>().copied() else {
        return Err("stream_cell_filter_off");
    };
    host.query(world, peer)
}

/// `stream.cell_relevance`: loaded cells, margin and last-snapshot drops for one peer.
pub fn cell_relevance(world: &World, params: &Value) -> Result<Value, RpcError> {
    let Some(peer) = crate::params::opt_u64(params, "peer")?.and_then(|peer| u32::try_from(peer).ok()) else {
        return Err(RpcError::invalid_params("`peer` must be a peer id (an integer)"));
    };
    cell_relevance_peer(world, peer)
        .map(|reply| {
            json!({
                "peer": reply.peer,
                "loaded": reply.loaded,
                "margin": reply.margin,
                "dropped": reply.dropped,
            })
        })
        .map_err(|reason| f0002(reason, reason_message(reason), reason_hint(reason)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_core::id::Guid;
    use regia_net::model::NetMode;
    use regia_scene::level::LevelId;
    use regia_scene::stream_grid::CellCoord;
    use regia_scene::streaming::LevelStreaming;
    use regia_world::format::{WORLD_INDEX_FORMAT_VERSION, WorldIndex};
    use regia_world::{MemoryCellSource, WorldStreaming};
    use serde_json::json;

    use super::*;

    fn reason(error: &RpcError) -> Option<&str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str)
    }

    fn code(error: &RpcError) -> Option<&str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
    }

    fn streaming_world() -> World {
        streaming_world_enabled(true)
    }

    fn streaming_world_enabled(enabled: bool) -> World {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        world.init_resource::<WorldStreaming>();
        let settings = LevelStreaming {
            enabled,
            cell_size_m: 100.0,
            load_radius_m: 40.0,
            unload_radius_m: 60.0,
            hlod: None,
            ..LevelStreaming::default()
        };
        let index = WorldIndex {
            format_version: WORLD_INDEX_FORMAT_VERSION,
            level: Guid::from_u128(0xce11),
            content_hash: [0; 32],
            settings,
            cells: Vec::new(),
            bounds: [CellCoord::new(0, 0), CellCoord::new(0, 0)],
            hlod: None,
            replicated: Vec::new(),
            nav: None,
        };
        let source = Arc::new(MemoryCellSource::new());
        world
            .resource_mut::<WorldStreaming>()
            .open_level(LevelId(Guid::from_u128(0xce11)), index, source)
            .expect("open empty streamed level");
        assert!(world.resource::<WorldStreaming>().is_active());
        world
    }

    fn host_ok(_world: &World, peer: u32) -> Result<CellRelevanceReply, &'static str> {
        Ok(CellRelevanceReply {
            peer,
            loaded: vec!["0_0".to_string(), "1_0".to_string()],
            margin: 1,
            dropped: 4,
        })
    }

    fn host_unknown(_world: &World, _peer: u32) -> Result<CellRelevanceReply, &'static str> {
        Err("peer_unknown")
    }

    #[test]
    fn missing_or_non_integer_peer_is_invalid_params() {
        let world = World::new();
        let cases = [
            json!({}),
            json!(null),
            json!([]),
            json!("x"),
            json!(5),
            json!(true),
            json!({ "peer": null }),
            json!({ "peer": -1 }),
            json!({ "peer": u64::MAX }),
            json!({ "peer": 1.5 }),
            json!({ "peer": "1" }),
            json!({ "peer": [] }),
        ];
        for params in cases {
            let error = cell_relevance(&world, &params).expect_err("refused");
            assert_eq!(code(&error), Some("F0002"), "{params}");
            assert_eq!(reason(&error), None, "{params}");
        }
    }

    #[test]
    fn client_is_refused_before_streaming() {
        let mut world = streaming_world();
        world.insert_resource(NetMode::Client);
        let error = cell_relevance(&world, &json!({ "peer": 1 })).expect_err("client");
        assert_eq!(reason(&error), Some("net_not_server"));
    }

    #[test]
    fn server_without_streaming_is_stream_not_enabled() {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        let error = cell_relevance(&world, &json!({ "peer": 1 })).expect_err("no runtime");
        assert_eq!(reason(&error), Some("stream_not_enabled"));
        world.init_resource::<WorldStreaming>();
        let error = cell_relevance(&world, &json!({ "peer": 1 })).expect_err("inactive");
        assert_eq!(reason(&error), Some("stream_not_enabled"));
    }

    #[test]
    fn active_streaming_without_a_host_is_stream_cell_filter_off() {
        let world = streaming_world();
        let error = cell_relevance(&world, &json!({ "peer": 1 })).expect_err("no host");
        assert_eq!(reason(&error), Some("stream_cell_filter_off"));
    }

    #[test]
    fn the_host_reply_is_the_method_result() {
        let mut world = streaming_world();
        world.insert_resource(CellRelevanceHost::new(host_unknown));
        let error = cell_relevance(&world, &json!({ "peer": 99 })).expect_err("unknown");
        assert_eq!(reason(&error), Some("peer_unknown"));
        world.insert_resource(CellRelevanceHost::new(host_ok));
        let value = cell_relevance(&world, &json!({ "peer": 1 })).expect("reply");
        assert_eq!(value["peer"], json!(1));
        assert_eq!(value["loaded"], json!(["0_0", "1_0"]));
        assert_eq!(value["margin"], json!(1));
        assert_eq!(value["dropped"], json!(4));
    }

    #[test]
    fn request_dispatch_returns_the_stream_cell_report() {
        let mut world = streaming_world();
        world.init_resource::<Time>();
        world.insert_resource(CellRelevanceHost::new(host_ok));
        let (send, receive) = crossbeam_channel::unbounded();
        let (reply, response) = crossbeam_channel::unbounded();
        world.insert_resource(crate::AgentInbox(receive));
        send.send(crate::server::Inbound {
            request: crate::protocol::Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(17)),
                method: "stream.cell_relevance".into(),
                params: json!({"peer": 1}),
            },
            reply,
        })
        .expect("request channel");
        crate::dispatch::process_requests(&mut world);
        let response = response.try_recv().expect("synchronous reply");
        assert_eq!(response.id, json!(17));
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(
            response.result,
            Some(json!({
                "peer": 1, "loaded": ["0_0", "1_0"], "margin": 1, "dropped": 4,
            }))
        );
    }

    #[test]
    fn a_disabled_index_is_stream_not_enabled_even_with_a_host() {
        let mut world = streaming_world_enabled(false);
        world.insert_resource(CellRelevanceHost::new(host_ok));
        let error = cell_relevance(&world, &json!({"peer": 1})).expect_err("disabled index");
        assert_eq!(reason(&error), Some("stream_not_enabled"));
    }
}
