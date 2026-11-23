// SPDX-License-Identifier: MPL-2.0
//! ADR-538 robustness: hostile JSON through the agent protocol's dispatch path.
//!
//! The server thread only parses the frame (`serde_json` into a `Request`); everything after that
//! runs on the main thread inside `process_requests`: the permission check, the rate limit, the
//! handler and the audit record. A panic there takes the game down with it. These tests drive
//! that path in-process (the WebSocket inbox is swapped for a channel the test owns, so no socket
//! and no wall clock is involved) and require every request to end in a result or an error that
//! carries a stable protocol code (`F0xxx`, or a method's documented lowercase code, the shape
//! `regia_agent::contract::is_error_code` checks; book `agents/protocol.md`), never a panic:
//!
//! - every method of the schema with whole-params garbage, then each of its parameters set to a
//!   hostile value of every JSON type (huge, negative, fractional, empty, long, non-ASCII, nested);
//! - seed-stable random combinations of parameters;
//! - mutated request text that still parses as JSON-RPC.
//!
//! Methods that write outside the temp project, need a GPU device, measure wall-clock time, end
//! the app or run a whole CPU benchmark scene per call are skipped (`SKIP`); their own tests
//! cover them.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use regia_agent::contract::is_error_code;
use regia_agent::protocol::{Request, Response};
use regia_agent::schema::{METHODS, MethodSchema};
use regia_agent::server::Inbound;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_scene::Transform;
use regia_test_utils::fuzz::{FuzzRng, mutate_text};
use regia_test_utils::{unique_temp_dir, write_atomic};
use serde_json::{Value, json};

/// Methods this fuzz does not call, and why.
const SKIP: &[(&str, &str)] = &[
    ("quit.request", "ends the app"),
    ("save.save", "writes a save slot"),
    (
        "audio.loopgen_render",
        "writes WAV files to a caller-chosen `out_dir`",
    ),
    ("seeing.screenshot", "`offscreen` creates a GPU device"),
    (
        "viewer.render_still",
        "opens a headless GPU device per accepted call (one device per call in one process: \
         73 s of this binary on an RTX 5090 at load 267, ADR-1439)",
    ),
    (
        "looks.render_board",
        "opens a headless GPU device per accepted call, as viewer.render_still (108 s of this \
         binary on an RTX 5090 at load 275 once that was skipped)",
    ),
    ("seeing.contact_sheet", "needs a window and a renderer"),
    ("seeing.capture_sequence", "needs a window and a renderer"),
    (
        "seeing.reference_trace",
        "path traces on a GPU device and writes `out_dir`",
    ),
    ("seeing.compare", "path traces and writes `map_png`"),
    ("quality.detect", "enumerates GPU adapters"),
    ("play.hitch_report", "measures wall-clock frame times"),
    (
        "seeing.gi_denoise_check",
        "runs fixed GI denoise scenarios on the CPU per accepted call (about 1 s in a test \
         build): three quarters of this binary's time, past the pregate's 600 s on a loaded \
         host. The fuzz_targets skip it too (ADR-2084); gi_denoise::tests::errors_carry_their_codes \
         covers its bad parameters",
    ),
];

/// Random parameter combinations per test run.
const RANDOM_CALLS: usize = 3_000;

/// Mutated request texts per test run.
const TEXT_CALLS: usize = 2_000;

/// Frames run after each request, so deferred handlers (`play.step`, `play.wait_until`) and the
/// per-frame agent systems see the state the request left.
const FRAMES_PER_CALL: usize = 2;

/// A headless app with the agent plugin, a channel standing in for the WebSocket inbox, the
/// default permission profile without its rate limit (so mutating calls reach their handlers),
/// and a few entities for entity-taking methods to find.
struct Harness {
    app: App,
    inbox: Sender<Inbound>,
    replies: Receiver<Response>,
    reply: Sender<Response>,
    /// `Entity::to_bits` of a named entity with a transform.
    rock: u64,
    /// The temp project directory (audit log, endpoint, `.regia/`).
    project: PathBuf,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let project = unique_temp_dir(tag);
        std::fs::create_dir(project.join("power-supply"))
            .unwrap_or_else(|err| panic!("temporary power directory: {err}"));
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .register_type::<Name>()
            .register_type::<Transform>()
            .insert_resource(AgentSettings {
                project_dir: Some(project.clone()),
            })
            .add_plugins(AgentPlugin)
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app.finish_plugins();
        let (inbox, inbox_rx) = unbounded();
        app.insert_resource(AgentInbox(inbox_rx));
        app.insert_resource(AgentPermissions {
            config: AgentConfig {
                rate_limit_per_second: u32::MAX,
                ..AgentConfig::default()
            },
            source: None,
        });
        let rock = app
            .world_mut()
            .spawn((Name::new("Rock"), Transform::default()))
            .id()
            .to_bits();
        app.world_mut().spawn((Name::new("Pebble"), Transform::default()));
        app.update();
        let (reply, replies) = unbounded();
        Self {
            app,
            inbox,
            replies,
            reply,
            rock,
            project,
        }
    }

    /// Sends one request, runs [`FRAMES_PER_CALL`] frames, and returns the panic message if a
    /// frame panicked.
    fn call(&mut self, method: &str, params: Value) -> Result<(), String> {
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        };
        self.send(request)
    }

    fn send(&mut self, mut request: Request) -> Result<(), String> {
        // Power calls must use the throwaway project, never the host sysfs.
        if request.method == "platform.power_state"
            && let Some(params) = request.params.as_object_mut()
            && params.get("dir").is_none_or(Value::is_null)
        {
            params.insert("dir".into(), json!(self.project.join("power-supply")));
        }
        self.inbox
            .send(Inbound {
                request,
                reply: self.reply.clone(),
            })
            .map_err(|e| format!("inbox closed: {e}"))?;
        let app = &mut self.app;
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            for _ in 0..FRAMES_PER_CALL {
                app.update();
            }
        }))
        .map_err(|payload| {
            payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(non-string panic payload)".into())
        })
    }

    /// Checks every response so far (exactly one of result and error; an error carries a
    /// Regia Engine code) and appends what is wrong to `problems`.
    fn check_replies(&self, context: &str, problems: &mut Vec<String>) {
        for response in self.replies.try_iter() {
            match (&response.result, &response.error) {
                (Some(_), None) => {}
                (None, Some(error)) => {
                    let code = error
                        .data
                        .as_ref()
                        .and_then(|d| d.get("code"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !is_error_code(code) {
                        problems.push(format!(
                            "{context}: error without a protocol error code: {error:?}"
                        ));
                    }
                }
                _ => problems.push(format!("{context}: malformed response {response:?}")),
            }
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.project);
    }
}

fn skipped(method: &str) -> bool {
    SKIP.iter().any(|(name, _)| *name == method)
}

fn fuzzed_methods() -> Vec<&'static MethodSchema> {
    METHODS.iter().filter(|m| !skipped(m.name)).collect()
}

/// A value nested `depth` arrays deep (below serde_json's parse limit of 128, so a client can
/// send it).
fn nested(depth: usize) -> Value {
    (0..depth).fold(json!(0), |inner, _| json!([inner]))
}

/// Hostile values of every JSON type. `rock` is a live entity's bits, so entity-taking handlers
/// get past the lookup with some of them.
fn hostile_values(rock: u64) -> Vec<Value> {
    vec![
        Value::Null,
        json!(true),
        json!(0),
        json!(-1),
        json!(1),
        json!(0.5),
        json!(-0.0),
        json!(1e308),
        json!(-1e308),
        json!(1e-308),
        json!(u64::MAX),
        json!(i64::MIN),
        json!(4_294_967_296u64),
        json!(rock),
        json!(""),
        json!("Rock"),
        json!("Transform"),
        json!("💥\u{0}界ß"),
        json!("x".repeat(4096)),
        json!("../../outside"),
        json!([]),
        json!([0.0, 0.0, 0.0]),
        json!([0.0, 0.0, 0.0, 0.0]),
        json!([1e308, -1e308, 1e308]),
        json!([u64::MAX, u64::MAX, u64::MAX, u64::MAX]),
        json!(vec![1; 10_000]),
        json!({}),
        json!({ "x": { "y": { "z": [] } } }),
        json!({ "translation": [1e308, 0, 0], "rotation": [0, 0, 0, 0], "scale": [0, 0, 0] }),
        nested(32),
    ]
}

/// A value of the parameter's declared kind that a handler would accept as "present", so a
/// call with one hostile parameter gets past the required-parameter checks of the others.
fn plausible(kind: &str, name: &str, rock: u64) -> Value {
    match (kind, name) {
        (_, "entity" | "parent" | "target") => json!(rock),
        ("string", _) => json!("Rock"),
        ("number", _) => json!(1),
        ("boolean", _) => json!(false),
        ("array", _) => json!([0.0, 1.0, 0.0]),
        ("object", _) => json!({}),
        _ => json!(1),
    }
}

/// Runs `calls` against one harness, rebuilding it after a panic (the world may be left
/// half-changed). Returns every panic and protocol problem.
fn run_calls(tag: &str, calls: impl IntoIterator<Item = (String, Value)>) -> Vec<String> {
    let mut harness = Harness::new(tag);
    let mut problems = Vec::new();
    for (method, params) in calls {
        let shown = {
            let text = params.to_string();
            let cut = text.floor_char_boundary(200);
            text[..cut].to_string()
        };
        if let Err(panic) = harness.call(&method, params) {
            problems.push(format!("{method} {shown}: panicked: {panic}"));
            harness = Harness::new(tag);
            continue;
        }
        harness.check_replies(&format!("{method} {shown}"), &mut problems);
    }
    problems
}

fn assert_no_problems(problems: &[String]) {
    assert!(
        problems.is_empty(),
        "{} problems, the first ones:\n{}",
        problems.len(),
        problems.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn the_skip_list_names_real_methods() {
    for (name, why) in SKIP {
        assert!(
            METHODS.iter().any(|m| m.name == *name),
            "{name} ({why}) is not in the schema"
        );
    }
}

#[test]
fn every_method_survives_whole_params_garbage() {
    let rock = Harness::new("agent-fuzz-probe").rock;
    let garbage = [
        Value::Null,
        json!([]),
        json!(1),
        json!("x"),
        json!(true),
        json!({}),
        json!([{}, null]),
        nested(32),
        json!({ "dry_run": true }),
        json!({ "entity": rock }),
    ];
    let calls = fuzzed_methods()
        .into_iter()
        .flat_map(|m| garbage.iter().map(move |g| (m.name.to_string(), g.clone())));
    assert_no_problems(&run_calls("agent-fuzz-garbage", calls));
}

#[test]
fn every_parameter_survives_hostile_values() {
    let rock = Harness::new("agent-fuzz-probe").rock;
    let hostile = hostile_values(rock);
    let mut calls = Vec::new();
    for method in fuzzed_methods() {
        for (i, param) in method.params.iter().enumerate() {
            for value in &hostile {
                let mut params = serde_json::Map::new();
                for (j, other) in method.params.iter().enumerate() {
                    if j != i && other.required {
                        params.insert(other.name.into(), plausible(other.kind, other.name, rock));
                    }
                }
                params.insert(param.name.into(), value.clone());
                calls.push((method.name.to_string(), Value::Object(params)));
            }
        }
    }
    assert!(!calls.is_empty());
    assert_no_problems(&run_calls("agent-fuzz-params", calls));
}

#[test]
fn random_parameter_combinations_never_panic() {
    let rock = Harness::new("agent-fuzz-probe").rock;
    let hostile = hostile_values(rock);
    let methods = fuzzed_methods();
    let mut rng = FuzzRng::new(0x0005_3821);
    let calls: Vec<(String, Value)> = (0..RANDOM_CALLS)
        .map(|_| {
            let method = methods[rng.below(methods.len())];
            let mut params = serde_json::Map::new();
            for param in method.params {
                match rng.below(3) {
                    0 => {}
                    1 => {
                        params.insert(param.name.into(), plausible(param.kind, param.name, rock));
                    }
                    _ => {
                        params.insert(param.name.into(), hostile[rng.below(hostile.len())].clone());
                    }
                }
            }
            if rng.chance(4) {
                params.insert("unexpected_key".into(), hostile[rng.below(hostile.len())].clone());
            }
            (method.name.to_string(), Value::Object(params))
        })
        .collect();
    assert_no_problems(&run_calls("agent-fuzz-random", calls));
}

/// Request texts as a client sends them; the mutations that still parse as a `Request` go
/// through dispatch like any other frame.
#[test]
fn mutated_request_text_never_panics() {
    let fixtures = [
        r#"{"jsonrpc":"2.0","id":1,"method":"scene.spawn","params":{"name":"Crate","components":{"Transform":{"translation":[1.0,2.0,3.0],"rotation":[0.0,0.0,0.0,1.0],"scale":[1.0,1.0,1.0]}}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"scene.apply_edits","params":{"dry_run":false,"edits":[{"op":"spawn","name":"A"},{"op":"set_property","entity":1,"component":"Transform","path":"translation","value":[0.0,1.0,0.0]}]}}"#,
        r#"{"jsonrpc":"2.0","id":"s","method":"play.inject_input","params":{"actions":[{"action":"Jump","value":1.0,"at_frame":3,"duration_frames":2}]}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"loop.set","params":{"max_fps":60.0,"fixed_hz":60.0,"max_substeps":4,"throttled_fps":10.0}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"anim.set_morph_weights","params":{"entity":1,"weights":{"0":0.5,"smile":1.0}}}"#,
        r#"{"jsonrpc":"2.0","id":5,"method":"scene.query_entities","params":{"with":["Name"],"limit":10}}"#,
    ];
    let mut rng = FuzzRng::new(0x0005_3822);
    let mut parsed = 0usize;
    let mut calls = Vec::new();
    for i in 0..TEXT_CALLS {
        let text = mutate_text(fixtures[i % fixtures.len()], &mut rng);
        // The server answers a frame that does not parse with a parse error itself.
        if let Ok(request) = serde_json::from_str::<Request>(&text)
            && !skipped(&request.method)
        {
            parsed += 1;
            calls.push((request.method, request.params));
        }
    }
    assert!(parsed > 0, "no mutation parsed: dispatch was never reached");
    assert_no_problems(&run_calls("agent-fuzz-text", calls));
}

/// `scene.apply_edits` validates every edit before applying any; the validation of a `despawn`
/// edit used to despawn the entity, so a dry run deleted it.
#[test]
fn apply_edits_dry_run_keeps_despawn_targets() {
    let mut harness = Harness::new("agent-fuzz-dry-run");
    let rock = harness.rock;
    let params = json!({ "dry_run": true, "edits": [{ "op": "despawn", "entity": rock }] });
    harness.call("scene.apply_edits", params).expect("no panic");
    let response = harness.replies.try_recv().expect("answered");
    let result = response.result.expect("validated");
    assert_eq!(result["applied"], false);
    let entity = Entity::try_from_bits(rock).expect("entity bits");
    assert!(
        harness.app.world().get_entity(entity).is_ok(),
        "a dry run despawned the entity"
    );

    // Applied for real, the same transaction removes it.
    let params = json!({ "edits": [{ "op": "despawn", "entity": rock }] });
    harness.call("scene.apply_edits", params).expect("no panic");
    let response = harness.replies.try_recv().expect("answered");
    assert!(response.error.is_none(), "{response:?}");
    assert!(
        harness.app.world().get_entity(entity).is_err(),
        "applied despawn kept the entity"
    );
}

#[test]
fn power_defaults_read_the_temporary_supply_in_the_fuzz_harness() {
    let mut harness = Harness::new("agent-fuzz-power");
    let directory = harness.project.join("power-supply");
    for (name, bytes) in [
        ("type", b"Battery\n".as_slice()),
        ("capacity", b"15\n".as_slice()),
        ("status", b"Discharging\n".as_slice()),
    ] {
        write_atomic(&directory.join(name), bytes)
            .unwrap_or_else(|err| panic!("temporary power attribute: {err}"));
    }
    for params in [json!({}), json!({ "dir": null })] {
        harness
            .call("platform.power_state", params)
            .unwrap_or_else(|err| panic!("power request: {err}"));
        let response = harness
            .replies
            .try_recv()
            .unwrap_or_else(|err| panic!("power reply: {err}"));
        assert!(response.error.is_none(), "{response:?}");
        assert_eq!(
            response.result,
            Some(json!({ "source": "battery", "percent": 15, "low_power": true }))
        );
    }
}
