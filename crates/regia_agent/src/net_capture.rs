// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the client capture (spec 120 § 15.5, ADR-8221, ADR-8890,
//! `book/agents/net-capture.md`): `net.capture_start` attaches a capture ring to the app's
//! [`NetLink`] client, `net.capture_save` writes what the ring holds to a `.netcapture`.
//!
//! Errors are `F0002` with `data.reason`: `capture_no_client` (no `NetLink`, or it hosts a
//! server), `capture_not_started` (save without a start), `capture_config_invalid` (a ring size
//! out of range), `capture_exists`, `capture_io`, `capture_empty` (the ring holds no whole
//! session), and `path_outside_project` for a refused path.

use regia_app::prelude::*;
use regia_net::NetLink;
use regia_net::capture::{CaptureError, CaptureStats, CaptureTap, DEFAULT_CAPTURE_RING_BYTES};
use serde_json::{Value, json};

use crate::net_session::f0002;
use crate::protocol::RpcError;

fn hint(code: &str) -> &'static str {
    match code {
        "capture_no_client" => {
            "run it in a client's world: the app needs NetLinkPlugin and a NetLink that is idle or connects as a client"
        }
        "capture_not_started" => "net.capture_start first",
        "capture_config_invalid" => "ring_bytes is 4096 to 1073741824 (default 16777216)",
        "capture_exists" => "choose another path or pass overwrite: true",
        "capture_io" => "check the path and the disk",
        "capture_empty" => {
            "the ring holds no whole session: start the capture before the client connects, or give it a larger ring"
        }
        _ => "see book/agents/net-capture.md",
    }
}

fn capture_error(e: &CaptureError) -> RpcError {
    f0002(e.code(), e.to_string(), hint(e.code()))
}

fn no_client(detail: &str) -> RpcError {
    f0002("capture_no_client", detail.to_owned(), hint("capture_no_client"))
}

fn stats_json(s: &CaptureStats) -> Value {
    json!({
        "ring_bytes": s.ring_bytes,
        "used_bytes": s.used_bytes,
        "peak_used_bytes": s.peak_used_bytes,
        "records": s.records,
        "written": s.written,
        "evicted": s.evicted,
        "oversized": s.oversized,
        "sessions": s.sessions,
        "head_cut": s.head_cut,
    })
}

/// `net.capture_start { ring_bytes? }`: attaches a fresh capture ring to the app's `NetLink`
/// client (or to an idle link, for the client it starts next). A running capture is replaced and
/// its records dropped (`replaced: true`). The ring is allocated once.
pub(crate) fn capture_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let ring = match params.get("ring_bytes") {
        None | Some(Value::Null) => DEFAULT_CAPTURE_RING_BYTES,
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| RpcError::invalid_params("`ring_bytes` must be a non-negative integer"))?,
    };
    let Some(mut link) = world.get_resource_mut::<NetLink>() else {
        return Err(no_client("the app has no NetLink"));
    };
    if link.role() == "server" {
        return Err(no_client(
            "the NetLink hosts a server; a capture records a client",
        ));
    }
    let tap = CaptureTap::new(ring).map_err(|e| capture_error(&e))?;
    let replaced = link.capture().is_some();
    link.set_capture(Some(tap.clone()));
    Ok(json!({
        "role": link.role(),
        "replaced": replaced,
        "capture": stats_json(&tap.stats()),
    }))
}

/// `net.capture_save { path, label?, overwrite?, stop? }`: writes the ring's whole sessions to a
/// `.netcapture` (atomically: a temporary file, synced, then renamed). The capture keeps running
/// unless `stop` is true.
pub(crate) fn capture_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` (a string) is required"))?;
    let label = match params.get("label") {
        None | Some(Value::Null) => "net.capture_save",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err(RpcError::invalid_params("`label` must be a string")),
    };
    let flag = |key: &str| match params.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a boolean"))),
    };
    let overwrite = flag("overwrite")?;
    let stop = flag("stop")?;
    let file = crate::req_path::allowed_path(world, "path", path)?;
    let Some(tap) = world.get_resource::<NetLink>().and_then(|l| l.capture().cloned()) else {
        return Err(f0002(
            "capture_not_started",
            "no capture runs on this app's NetLink",
            hint("capture_not_started"),
        ));
    };
    let stats = tap.stats();
    let summary = tap.save(&file, label, overwrite).map_err(|e| capture_error(&e))?;
    if stop && let Some(mut link) = world.get_resource_mut::<NetLink>() {
        link.set_capture(None);
    }
    Ok(json!({
        "path": summary.path.display().to_string(),
        "bytes": summary.bytes,
        "records": summary.records,
        "sessions": summary.sessions,
        "stopped": stop,
        "capture": stats_json(&stats),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::NetLinkPlugin;
    use regia_net::capture::CaptureFile;
    use regia_net::link::LinkOptions;
    use regia_net::socket::LoopbackNet;
    use regia_net::{NetGame, NetMode, NetPlugin, bridge::NetFrame};
    use std::net::SocketAddr;

    struct Game;
    impl NetGame for Game {
        type Message = NetFrame;
        type Input = u8;
        type JoinData = ();
        const GAME_ID: &'static str = "regia.test.agent_net_capture";
        const PROTOCOL_VERSION: u32 = 1;
    }

    fn reason(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    fn app(mode: NetMode) -> App {
        let mut app = App::new();
        regia_app::enable_fixed_step(&mut app, 60.0);
        app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
        app
    }

    fn options() -> LinkOptions {
        LinkOptions {
            clock: regia_net::LinkClock::Manual {
                start_ms: 0,
                step_us: 16_667,
            },
            ..LinkOptions::default()
        }
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "regia-agent-net-capture-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn start_then_save_writes_a_capture_of_the_client_session() {
        let net = LoopbackNet::new();
        let mut server = app(NetMode::DedicatedServer);
        server
            .world_mut()
            .resource_mut::<NetLink>()
            .host::<Game>(Box::new(net.bind(addr(7811)).expect("bind")), &options())
            .expect("host");
        let mut client = app(NetMode::Client);
        // Before the connect: the idle link keeps the capture for the client it starts.
        let started = capture_start(client.world_mut(), &json!({ "ring_bytes": 1 << 20 })).expect("start");
        assert_eq!(started["role"], "idle");
        assert_eq!(started["replaced"], false);
        assert_eq!(started["capture"]["ring_bytes"], 1 << 20);
        client
            .world_mut()
            .resource_mut::<NetLink>()
            .connect::<Game>(Box::new(net.bind(addr(0)).expect("bind")), addr(7811), &options())
            .expect("connect");
        for _ in 0..120 {
            server.update();
            client.update();
        }
        assert!(client.world().resource::<NetLink>().is_connected());
        // The server's world refuses a capture.
        let refused = capture_start(server.world_mut(), &json!({})).expect_err("server");
        assert_eq!(reason(&refused), "capture_no_client");
        let dir = temp_dir("save");
        let path = dir.join("client.netcapture");
        let path_text = path.display().to_string();
        let saved = capture_save(
            client.world_mut(),
            &json!({ "path": path_text, "label": "agent test" }),
        )
        .expect("save");
        assert_eq!(saved["sessions"], 1);
        assert_eq!(saved["stopped"], false);
        assert!(saved["records"].as_u64().unwrap_or(0) > 100, "{saved}");
        let file = CaptureFile::load(&path).expect("a .netcapture");
        assert_eq!(file.label, "agent test");
        assert_eq!(file.sessions().len(), 1);
        // A second save to the same path is refused unless overwrite is on.
        let exists = capture_save(client.world_mut(), &json!({ "path": path_text })).expect_err("exists");
        assert_eq!(reason(&exists), "capture_exists");
        let again = capture_save(
            client.world_mut(),
            &json!({ "path": path_text, "overwrite": true, "stop": true }),
        )
        .expect("overwrite");
        assert_eq!(again["stopped"], true);
        assert!(client.world().resource::<NetLink>().capture().is_none());
        let not_started =
            capture_save(client.world_mut(), &json!({ "path": path_text })).expect_err("stopped");
        assert_eq!(reason(&not_started), "capture_not_started");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bad_params_and_missing_links_have_their_codes() {
        let mut bare = App::new();
        let none = capture_start(bare.world_mut(), &json!({})).expect_err("no link");
        assert_eq!(reason(&none), "capture_no_client");
        let mut client = app(NetMode::Client);
        let small = capture_start(client.world_mut(), &json!({ "ring_bytes": 100 })).expect_err("small");
        assert_eq!(reason(&small), "capture_config_invalid");
        assert!(capture_start(client.world_mut(), &json!({ "ring_bytes": "big" })).is_err());
        assert!(
            capture_save(client.world_mut(), &json!({})).is_err(),
            "path is required"
        );
        capture_start(client.world_mut(), &json!({})).expect("start");
        let again = capture_start(client.world_mut(), &json!({})).expect("restart");
        assert_eq!(again["replaced"], true);
        let dir = temp_dir("empty");
        let path = dir.join("empty.netcapture").display().to_string();
        // An idle link's ring holds no session yet.
        let empty = capture_save(client.world_mut(), &json!({ "path": path })).expect_err("empty");
        assert_eq!(reason(&empty), "capture_empty");
        let outside =
            capture_save(client.world_mut(), &json!({ "path": "../out.netcapture" })).expect_err("..");
        assert_eq!(reason(&outside), "path_outside_project");
        std::fs::remove_dir_all(&dir).ok();
    }
}
