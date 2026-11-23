// SPDX-License-Identifier: MPL-2.0
//! `feedback.capture` and `feedback.list` (book `agents/feedback.md`): write a feedback bundle
//! the way the player's prompt does, and list or open the bundles in the feedback folder.
//!
//! Bundles are local files (`regia_framework::feedback`); nothing here uploads anything.

use crossbeam_channel::Sender;
use regia_app::prelude::*;
use regia_framework::feedback::{
    BundleInfo, CaptureOptions, Feedback, FeedbackCategory, bundle::is_bundle_name, feedback_folder,
    list_bundles, read_bundle,
};
use serde_json::{Value, json};

use crate::protocol::{Response, RpcError};

fn no_plugin() -> RpcError {
    RpcError::not_found(
        "no feedback plugin in this app",
        "add regia_framework::feedback::FeedbackPlugin",
    )
}

fn info_json(info: &BundleInfo) -> Value {
    json!({
        "name": info.name,
        "path": info.path.display().to_string(),
        "bytes": info.bytes,
        "category": info.category.as_str(),
        "entries": info.entries,
    })
}

/// `feedback.capture`: writes a bundle now; the reply comes when the file is on disk.
pub(crate) fn capture(
    world: &mut World,
    params: &Value,
    id: Value,
    reply: Sender<Response>,
) -> Result<(), RpcError> {
    let category = match params.get("category") {
        None | Some(Value::Null) => FeedbackCategory::Other,
        Some(Value::String(name)) => FeedbackCategory::parse(name)
            .filter(|c| *c != FeedbackCategory::Crash)
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "unknown category {name:?} (bug, performance, visual or other)"
                ))
            })?,
        Some(_) => return Err(RpcError::invalid_params("`category` must be a string")),
    };
    let note = match params.get("note") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(note)) => note.clone(),
        Some(_) => return Err(RpcError::invalid_params("`note` must be a string")),
    };
    let screenshot = match params.get("screenshot") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(RpcError::invalid_params("`screenshot` must be a boolean")),
    };
    let mut feedback = world.get_resource_mut::<Feedback>().ok_or_else(no_plugin)?;
    feedback.capture(
        CaptureOptions {
            category,
            note,
            screenshot,
        },
        Some(Box::new(move |result| {
            let response = match result {
                Ok(info) => Response::ok(id, info_json(&info)),
                Err(e) => Response::err(
                    id,
                    RpcError::internal(format!("feedback bundle not written: {e}")),
                ),
            };
            let _ = reply.send(response);
        })),
    );
    Ok(())
}

/// `feedback.list`: the bundles in the feedback folder, or one bundle's entries and
/// `feedback.json` by name.
pub(crate) fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<Feedback>().is_none() {
        return Err(no_plugin());
    }
    let Some(dir) = feedback_folder(world) else {
        return Err(RpcError::not_found(
            "no feedback folder",
            "mount user:// (PlatformPlugin) or set FeedbackConfig::dir",
        ));
    };
    if params.get("name").is_some_and(|v| !v.is_null() && !v.is_string()) {
        return Err(RpcError::invalid_params("`name` must be a string"));
    }
    if let Some(name) = crate::params::opt_str(params, "name")? {
        if !is_bundle_name(name) {
            return Err(RpcError::invalid_params(format!(
                "{name:?} is not a bundle file name"
            )));
        }
        let path = dir.join(name);
        let entries = read_bundle(&path).map_err(|e| {
            RpcError::not_found(
                format!("bundle {name}: {e}"),
                "call feedback.list without a name to list the bundles",
            )
        })?;
        let meta = entries
            .iter()
            .find(|e| e.name == "feedback.json")
            .and_then(|e| serde_json::from_slice::<Value>(&e.data).ok())
            .unwrap_or(Value::Null);
        let files: Vec<Value> = entries
            .iter()
            .map(|e| json!({ "name": e.name, "bytes": e.data.len() }))
            .collect();
        return Ok(json!({
            "name": name,
            "path": path.display().to_string(),
            "files": files,
            "feedback": meta,
        }));
    }
    let bundles: Vec<Value> = list_bundles(&dir)
        .into_iter()
        .map(|b| {
            json!({
                "name": b.name,
                "path": b.path.display().to_string(),
                "bytes": b.bytes,
                "modified_unix_ms": b.modified_unix_ms,
                "category": b.category.map(FeedbackCategory::as_str),
            })
        })
        .collect();
    let count = bundles.len();
    Ok(json!({ "folder": dir.display().to_string(), "bundles": bundles, "count": count }))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_framework::feedback::{FeedbackConfig, FeedbackPlugin};

    use super::*;

    #[test]
    fn feedback_pipeline_cache_capture_and_list_use_the_dispatcher() {
        use crate::{LocalReply, call_local};
        use regia_framework::feedback::PipelineCacheAttachment;
        use regia_render::pipeline_cache::{PipelineCacheKey, WGPU_VERSION, encode};

        let root = regia_test_utils::unique_temp_dir("agent-feedback-pipeline-cache");
        let key = PipelineCacheKey {
            backend: "Vulkan".into(),
            vendor: 1,
            device: 2,
            driver: "test-driver".into(),
            driver_info: "1".into(),
            wgpu_version: WGPU_VERSION.into(),
            shader_hash: "test-content".into(),
        };
        let path = root.join("saved.fpc");
        regia_test_utils::write_atomic(&path, &encode(&key, &[7; 256])).expect("saved cache");
        let entry = format!("pipelines/{}", key.shipped_file_name());
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
        app.add_plugins(FeedbackPlugin {
            config: FeedbackConfig {
                dir: Some(root.join("reports")),
                crash_bundles: false,
                ..FeedbackConfig::default()
            },
        });
        app.insert_resource(PipelineCacheAttachment { path, key });
        for enabled in [true, false] {
            if !enabled {
                app.world_mut()
                    .resource_mut::<FeedbackConfig>()
                    .pipeline_cache_max_bytes = 0;
            }
            let LocalReply::Pending(reply) = call_local(
                app.world_mut(),
                "feedback.capture",
                json!({ "category": "performance", "screenshot": false }),
                None,
            ) else {
                panic!("capture must dispatch to the feedback worker");
            };
            app.update();
            // Completion is a channel rendezvous, with no sleeps or elapsed-time assertion.
            let response = serde_json::to_value(reply.recv().expect("capture reply")).expect("json");
            let result = &response["result"];
            assert!(result["name"].is_string(), "{response}");
            let LocalReply::Done(Ok(opened)) = call_local(
                app.world_mut(),
                "feedback.list",
                json!({"name": result["name"]}),
                None,
            ) else {
                panic!("list must open the completed bundle");
            };
            let attached = opened["feedback"]["pipeline_cache"].as_str();
            assert_eq!(attached, enabled.then_some(entry.as_str()));
            assert_eq!(
                result["entries"]
                    .as_array()
                    .expect("entries")
                    .iter()
                    .any(|n| n == &entry),
                enabled
            );
            app.update();
        }
        drop(app);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn capture_then_list_and_open() {
        let dir = std::env::temp_dir().join(format!("regia-agent-feedback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
        app.add_plugins(FeedbackPlugin {
            config: FeedbackConfig {
                dir: Some(dir.clone()),
                crash_bundles: false,
                ..FeedbackConfig::default()
            },
        });

        assert!(
            capture(
                app.world_mut(),
                &json!({ "category": "crash" }),
                json!(0),
                crossbeam_channel::bounded(1).0
            )
            .is_err()
        );
        let (tx, rx) = crossbeam_channel::bounded(1);
        capture(
            app.world_mut(),
            &json!({ "category": "bug", "note": "door stuck", "screenshot": false }),
            json!(1),
            tx,
        )
        .expect("capture starts");
        let mut response = None;
        for _ in 0..400 {
            app.update();
            if let Ok(r) = rx.try_recv() {
                response = Some(r);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let response = serde_json::to_value(response.expect("reply")).expect("json");
        let result = &response["result"];
        assert_eq!(result["category"], "bug");
        let name = result["name"].as_str().expect("name").to_owned();
        assert!(
            result["entries"]
                .as_array()
                .is_some_and(|e| e.iter().any(|n| n == "feedback.json"))
        );

        let listed = list(app.world_mut(), &json!({})).expect("feedback.list");
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["bundles"][0]["name"], name.as_str());
        let opened = list(app.world_mut(), &json!({ "name": name })).expect("open");
        assert_eq!(opened["feedback"]["note"], "door stuck");
        assert!(opened["files"].as_array().is_some_and(|f| f.len() >= 10));
        assert!(list(app.world_mut(), &json!({ "name": "../etc/passwd" })).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn argument_types_and_names_are_checked_before_anything_is_written() {
        let dir = regia_test_utils::unique_temp_dir("agent-feedback-args");
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
        app.add_plugins(FeedbackPlugin {
            config: FeedbackConfig {
                dir: Some(dir.clone()),
                crash_bundles: false,
                ..FeedbackConfig::default()
            },
        });
        let (tx, rx) = crossbeam_channel::bounded(1);
        for bad in [
            json!({ "category": 5 }),
            json!({ "category": "Bug-ish" }),
            json!({ "note": 5 }),
            json!({ "screenshot": "no" }),
        ] {
            let err = capture(app.world_mut(), &bad, json!(0), tx.clone()).expect_err("refused");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{bad}");
        }
        for _ in 0..4 {
            app.update();
        }
        assert!(rx.try_recv().is_err(), "nothing was captured");
        assert_eq!(list(app.world_mut(), &json!({})).expect("list")["count"], 0);
        for bad in [
            json!({ "name": 5 }),
            json!({ "name": "" }),
            json!({ "name": "a/b.zip" }),
            json!({ "name": "missing.zip" }),
        ] {
            assert!(list(app.world_mut(), &bad).is_err(), "{bad}");
        }
        // Without the plugin both report not-found.
        let mut bare = World::new();
        assert_eq!(
            list(&mut bare, &json!({})).expect_err("bare").code,
            crate::protocol::NOT_FOUND
        );
        assert_eq!(
            capture(&mut bare, &json!({}), json!(0), tx)
                .expect_err("bare")
                .code,
            crate::protocol::NOT_FOUND
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
