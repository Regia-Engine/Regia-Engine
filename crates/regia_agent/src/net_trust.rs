// SPDX-License-Identifier: MPL-2.0
//! `net.known_hosts`, `net.trust` and `net.forget`: the client's trust-on-first-use store of
//! server keys (`regia_net::known_hosts`, ADR-2416, `book/agents/net-trust.md`).
//!
//! The store is `user://known_hosts.ron`, the file a `NetLink` client with
//! `LinkOptions::known_hosts` reads. The methods take no path: an agent can change whom the player
//! trusts, never write files elsewhere.

use regia_app::prelude::*;
use regia_net::ServerKey;
use regia_net::known_hosts::{KNOWN_HOSTS_FILE, KnownHosts, KnownHostsError};
use regia_platform::{MountPoint, Vfs, VfsPath};
use serde_json::{Value, json};

use crate::protocol::{INTERNAL_ERROR, INVALID_PARAMS, RpcError, UNSUPPORTED};

/// The store file, from the app's `user://` mount.
fn store_path(world: &World) -> Result<std::path::PathBuf, RpcError> {
    world
        .get_resource::<Vfs>()
        .and_then(|vfs| vfs.host_path(&VfsPath::root(MountPoint::User)))
        .map(|dir| dir.join(KNOWN_HOSTS_FILE))
        .ok_or_else(|| {
            RpcError::new(
                UNSUPPORTED,
                "net_known_hosts_no_store",
                "this app has no user:// directory for known_hosts.ron",
                "mount user:// (the default app does) and retry",
            )
        })
}

fn store_error(e: &KnownHostsError) -> RpcError {
    let class = match e {
        KnownHostsError::BadHostId(_) | KnownHostsError::Full => INVALID_PARAMS,
        KnownHostsError::WriteFailed { .. } => INTERNAL_ERROR,
        _ => UNSUPPORTED,
    };
    let hint = match e {
        KnownHostsError::Unreadable { .. } | KnownHostsError::Newer { .. } => {
            "the store is left as it is; fix or delete the file by hand"
        }
        KnownHostsError::Full => "forget hosts you no longer use (net.forget)",
        KnownHostsError::BadHostId(_) => {
            "a host id is non-empty text without control characters, at most 256 bytes"
        }
        _ => "see book/agents/net-trust.md",
    };
    RpcError::new(class, e.code(), e.to_string(), hint)
}

fn host_param(params: &Value) -> Result<&str, RpcError> {
    match params.get("host") {
        Some(Value::String(s)) => Ok(s.as_str()),
        _ => Err(RpcError::invalid_params("`host` (string) is required")),
    }
}

fn load(world: &World) -> Result<(std::path::PathBuf, KnownHosts), RpcError> {
    let path = store_path(world)?;
    let store = KnownHosts::load(&path).map_err(|e| store_error(&e))?;
    Ok((path, store))
}

/// `net.known_hosts`: every trusted host with its key and how it was trusted.
pub fn known_hosts(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let (path, store) = load(world)?;
    let hosts: Vec<Value> = store
        .hosts()
        .map(|(host, known)| {
            json!({ "host": host, "key": known.key.to_string(), "source": known.source.as_str() })
        })
        .collect();
    Ok(json!({ "path": path.to_string_lossy(), "hosts": hosts }))
}

/// `net.trust { host, key }`: pins `key` for `host`, replacing an earlier key.
pub fn trust(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let host = host_param(params)?;
    let bad_key = || {
        RpcError::new(
            INVALID_PARAMS,
            "net_bad_server_key",
            "`key` must be a server key: 64 hex digits",
            "copy it from net.crypto (server_key) or the server's `serverkey` console command",
        )
    };
    let key = params.get("key").and_then(Value::as_str).ok_or_else(bad_key)?;
    let key = ServerKey::parse(key).ok_or_else(bad_key)?;
    let (path, mut store) = load(world)?;
    let previous = store.trust(host, key).map_err(|e| store_error(&e))?;
    store.save(&path).map_err(|e| store_error(&e))?;
    tracing::info!(target: "regia_agent", host, %key, "net.trust pinned a server key");
    Ok(json!({
        "host": regia_net::known_hosts::normalize_host_id(host).unwrap_or_default(),
        "key": key.to_string(),
        "previous": previous.map(|p| p.key.to_string()),
        "path": path.to_string_lossy(),
    }))
}

/// `net.forget { host }`: forgets `host`, so its next connection pins whatever key it shows.
pub fn forget(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let host = host_param(params)?;
    let (path, mut store) = load(world)?;
    let previous = store.forget(host).map_err(|e| store_error(&e))?;
    if previous.is_some() {
        store.save(&path).map_err(|e| store_error(&e))?;
        tracing::info!(target: "regia_agent", host, "net.forget removed a pinned server key");
    }
    Ok(json!({
        "host": regia_net::known_hosts::normalize_host_id(host).unwrap_or_default(),
        "forgotten": previous.is_some(),
        "previous": previous.map(|p| p.key.to_string()),
        "path": path.to_string_lossy(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a";
    const KEY_B: &str = "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f";

    fn world(tag: &str) -> (World, std::path::PathBuf) {
        let dir = regia_test_utils::unique_temp_dir(tag);
        let mut vfs = Vfs::default();
        vfs.mount(MountPoint::User, regia_platform::DirMount::writable(dir.clone()));
        let mut world = World::new();
        world.insert_resource(vfs);
        (world, dir)
    }

    fn code(err: &RpcError) -> String {
        format!("{err:?}")
    }

    #[test]
    fn trust_list_and_forget_round_trip_through_the_file() {
        let (mut w, dir) = world("net-trust");
        let empty = known_hosts(&mut w, &json!({})).expect("list");
        assert_eq!(empty["hosts"], json!([]));
        let first = trust(&mut w, &json!({ "host": "Game.Example:7777", "key": KEY_A })).expect("trust");
        assert_eq!(first["host"], "game.example:7777");
        assert_eq!(first["previous"], Value::Null);
        let second = trust(&mut w, &json!({ "host": "game.example:7777", "key": KEY_B })).expect("trust");
        assert_eq!(second["previous"], KEY_A);
        let listed = known_hosts(&mut w, &json!({})).expect("list");
        assert_eq!(
            listed["hosts"],
            json!([{ "host": "game.example:7777", "key": KEY_B, "source": "manual" }])
        );
        let store = KnownHosts::load(&dir.join(KNOWN_HOSTS_FILE)).expect("the file a link reads");
        assert_eq!(
            store.get("game.example:7777").map(|h| h.key.to_string()),
            Some(KEY_B.to_owned())
        );
        let gone = forget(&mut w, &json!({ "host": "GAME.example:7777" })).expect("forget");
        assert_eq!(gone["forgotten"], true);
        assert_eq!(gone["previous"], KEY_B);
        let again = forget(&mut w, &json!({ "host": "game.example:7777" })).expect("forget");
        assert_eq!(again["forgotten"], false);
        assert_eq!(known_hosts(&mut w, &json!({})).expect("list")["hosts"], json!([]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_input_and_a_damaged_store_answer_with_codes() {
        let (mut w, dir) = world("net-trust-bad");
        let err = trust(&mut w, &json!({ "host": "h", "key": "zz" })).expect_err("bad key");
        assert!(code(&err).contains("net_bad_server_key"), "{err:?}");
        let err = trust(&mut w, &json!({ "host": " ", "key": KEY_A })).expect_err("bad host");
        assert!(code(&err).contains("net_bad_host_id"), "{err:?}");
        let err = forget(&mut w, &json!({})).expect_err("no host");
        assert!(
            code(&err).contains("F0") || code(&err).contains("host"),
            "{err:?}"
        );
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join(KNOWN_HOSTS_FILE), "broken").expect("write");
        let err = trust(&mut w, &json!({ "host": "h", "key": KEY_A })).expect_err("damaged");
        assert!(code(&err).contains("net_known_hosts_unreadable"), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join(KNOWN_HOSTS_FILE)).expect("read"),
            "broken",
            "never overwritten"
        );
        let mut bare = World::new();
        let err = known_hosts(&mut bare, &json!({})).expect_err("no user dir");
        assert!(code(&err).contains("net_known_hosts_no_store"), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
