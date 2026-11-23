// SPDX-License-Identifier: MPL-2.0
//! The `dcc_feed.*` rows of the method table, compiled with or without the feature, so a game
//! build without it answers `F0013` (reason `dcc_feed_disabled`) instead of "method not found"
//! (`specs/60` § 5). `dcc_feed.status` always answers.

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Runs a `dcc_feed.*` method.
#[cfg(feature = "dcc-feed")]
pub(crate) fn call(world: &mut World, method: &str, params: &Value) -> Result<Value, RpcError> {
    use crate::dcc_feed::methods as m;
    if world.get_resource::<crate::dcc_feed::DccFeedSettings>().is_none() {
        return disabled(method);
    }
    match method {
        "dcc_feed.status" => m::status(world, params),
        "dcc_feed.items" => m::items(world, params),
        "dcc_feed.accept" => m::accept(world, params),
        "dcc_feed.disconnect" => m::disconnect(world, params),
        "dcc_feed.record" => m::record(world, params),
        "dcc_feed.replay" => m::replay(world, params),
        "dcc_feed.listen" => m::listen(world, params),
        "dcc_feed.rotate_token" => m::rotate_token(world, params),
        // The editor registers these on its extension, which is consulted first.
        _ => disabled(method),
    }
}

/// Runs a `dcc_feed.*` method.
#[cfg(not(feature = "dcc-feed"))]
pub(crate) fn call(_world: &mut World, method: &str, _params: &Value) -> Result<Value, RpcError> {
    disabled(method)
}

fn disabled(method: &str) -> Result<Value, RpcError> {
    if method == "dcc_feed.status" {
        return Ok(json!({
            "enabled": false,
            "listeners": [],
            "sessions": [],
            "settings": Value::Null,
            "reason": "dcc_feed_disabled"
        }));
    }
    let mut error = RpcError::new(
        protocol::UNSUPPORTED,
        "F0013",
        "the DCC feed is not in this build",
        "build with the `dcc-feed` feature (dev builds only)",
    );
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!("dcc_feed_disabled"));
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_status_includes_settings() {
        let mut world = World::new();
        let status = call(&mut world, "dcc_feed.status", &json!({})).expect("status");
        assert_eq!(status["enabled"], false);
        assert!(status["settings"].is_null());
        assert_eq!(status["reason"], "dcc_feed_disabled");
        assert!(status["listeners"].as_array().is_some_and(|l| l.is_empty()));
        assert!(status["sessions"].as_array().is_some_and(|s| s.is_empty()));
        let err = call(&mut world, "dcc_feed.items", &json!({})).expect_err("still disabled");
        assert_eq!(err.code, protocol::UNSUPPORTED);
    }
}
