// SPDX-License-Identifier: MPL-2.0
//! ADR-2622: a wrong-typed optional parameter is `invalid_params`, never a silent default.
//!
//! For every method of the schema and every optional parameter of a scalar kind (`string`,
//! `number`, `integer`, `boolean`, `array`, `object`) one call sends a value of the wrong JSON type
//! (the required parameters get plausible values) and the reply must be an error, never a success
//! and never a panic. Replies that are `invalid_params` and name the parameter in `data.param` are
//! counted as precise; the rest are listed by the report test.
//!
//! The harness is the one of `tests/dispatch_fuzz.rs` (a headless app, a channel for the inbox, no
//! rate limit). Methods that write outside the temp project, need a GPU, measure wall-clock time
//! or end the app are skipped (`SKIP`), as there.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use regia_agent::protocol::INVALID_PARAMS;
use regia_agent::protocol::{Request, Response};
use regia_agent::schema::METHODS;
use regia_agent::server::Inbound;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_scene::Transform;
use regia_test_utils::unique_temp_dir;
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
    ("seeing.contact_sheet", "needs a window and a renderer"),
    ("seeing.capture_sequence", "needs a window and a renderer"),
    (
        "seeing.reference_trace",
        "path traces on a GPU device and writes `out_dir`",
    ),
    ("seeing.compare", "path traces and writes `map_png`"),
    ("quality.detect", "enumerates GPU adapters"),
    ("play.hitch_report", "measures wall-clock frame times"),
];

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

    fn send(&mut self, request: Request) -> Result<(), String> {
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
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.project);
    }
}

fn skipped(method: &str) -> bool {
    SKIP.iter().any(|(name, _)| *name == method)
}

/// Wrong-typed optional parameters that a call is allowed to ignore, and why: every one is read only
/// for an `action`, `mode` or `kind` other than the default one this test sends (or by a method
/// that gets the value from a path the test cannot reach), so the default call never looks at it.
/// The list can only shrink: a parameter that starts refusing must be removed (`stale entry` check).
const IGNORED_WHEN_UNUSED: &[(&str, &str)] = &[
    ("play.step", "timeout_ms"),
    ("render.set_valley_mist", "fps"),
    ("render.set_valley_mist", "grid_cell_m"),
    ("render.set_valley_mist", "concavity_radius_m"),
    ("ui.layers", "entity"),
    ("ui.layers", "mode"),
    ("ui.play_anim", "name"),
    ("ui.play_anim", "animation"),
    ("ui.play_anim", "entity"),
    ("ui.play_anim", "reverse"),
    ("ui.play_anim", "speed"),
    ("ui.play_anim", "time"),
    ("ui.tween", "property"),
    ("ui.tween", "duration"),
    ("ui.tween", "delay"),
    ("ui.tween", "tweens"),
    ("ui.tween", "mode"),
    ("ui.tween", "kind"),
    ("ui.tween", "offset"),
    ("ui.tween", "translate"),
    ("ui.tween", "scale"),
    ("ui.tween", "pivot_ratio"),
    ("ui.tween", "hit_test"),
    ("ui.tween", "reset"),
    ("ui.motion", "preset"),
    ("ui.motion", "speed"),
    ("ui.motion", "exit_action"),
    ("ui.motion", "enter_on_add"),
    ("ui.motion", "trigger"),
    ("render.sequence", "ron"),
    ("render.sequence", "sequence"),
    ("render.sequence", "time"),
    ("render.sequence", "frame"),
    ("anim.set_morph_weights", "weight_param"),
    ("loop.set", "frames"),
    ("xr.drive", "count"),
    ("xr.drive", "position"),
    ("xr.drive", "yaw"),
    ("xr.drive", "hand"),
    ("xr.drive", "amplitude"),
    ("xr.drive", "duration_ms"),
    ("xr.drive", "head"),
    ("xr.drive", "left"),
    ("xr.drive", "right"),
    ("xr.drive", "fov_deg"),
    ("xr.drive", "ipd_m"),
    ("xr.drive", "name"),
    ("xr.drive", "sub"),
    ("xr.drive", "state"),
    ("xr.drive", "ms"),
    ("xr.set_quad", "remove"),
    ("xr.set_quad", "space"),
    ("xr.set_quad", "position"),
    ("xr.set_quad", "size_m"),
    ("xr.set_quad", "priority"),
    ("xr.set_quad", "color"),
    ("ui.screens", "entity"),
    ("ui.subtitles", "speaker"),
    ("ui.subtitles", "text"),
    ("ui.subtitles", "key"),
    ("ui.subtitles", "args"),
    ("ui.subtitles", "start"),
    ("ui.subtitles", "duration"),
    ("ui.subtitles", "priority"),
    ("ui.subtitles", "to"),
    ("ui.subtitles", "by"),
    ("ui.subtitles", "id"),
    ("ui.subtitles", "max_concurrent"),
    ("sequence.player", "seq_ron"),
    ("sequence.player", "sequence"),
    ("sequence.player", "shots_ron"),
    ("sequence.player", "shots"),
    ("sequence.player", "time"),
    ("sequence.player", "frame"),
    ("sequence.player", "name"),
    ("sequence.player", "clip"),
    ("sequence.player", "asset"),
    ("sequence.player", "remove"),
    ("media.state", "player"),
];

/// A value of a JSON type that is none of the `kind` the schema declares.
fn wrong_value(kind: &str) -> Option<Value> {
    match kind {
        "string" => Some(json!(12345)),
        "number" | "integer" => Some(json!("5")),
        "boolean" => Some(json!("true")),
        "array" => Some(json!("not an array")),
        "object" => Some(json!("not an object")),
        _ => None, // `any` accepts every type
    }
}

/// A value of the parameter's declared kind that a handler accepts as "present", so a call with
/// one wrong-typed parameter gets past the required-parameter checks of the others.
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

/// What one wrong-typed call answered.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// `invalid_params` with `data.param` naming the parameter.
    Precise,
    /// Some other error (another parameter or a state check refused first, or a handler with its
    /// own code).
    OtherError,
    /// `invalid_params` that does not name the parameter in `data.param`.
    UnnamedInvalidParams,
    /// A result: the wrong value was accepted or ignored.
    Success,
    /// The frame panicked.
    Panic,
}

/// Serializes the harness runs of this binary's tests (ADR-1439). A render-out or capture call
/// opens a headless device in its harness's world (`seeing::ensure_device`: one per call site in
/// the shipped crate), and a harness dropped or rebuilt destroys it. Two tests doing that on
/// parallel threads create and destroy Vulkan devices while the other names objects, the loader
/// race of ADR-2113: pregate 0f89ab970 card 3 died with SIGSEGV -11 in the
/// `precise_refusals_do_not_regress` thread at instruction 0 beside other binaries (green alone).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `(method, param, answer)` for every optional scalar parameter of every callable method.
/// Holds [`SERIAL_GPU`] for the whole run (taken first, so the harness drops while it is held).
fn run_all() -> Vec<(String, String, Answer)> {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut harness = Harness::new("typed-params-negative");
    let rock = harness.rock;
    let mut out = Vec::new();
    for method in METHODS.iter().filter(|m| !skipped(m.name)) {
        for param in method.params.iter().filter(|p| !p.required) {
            let Some(wrong) = wrong_value(param.kind) else {
                continue;
            };
            let mut params = serde_json::Map::new();
            for other in method.params.iter().filter(|o| o.required) {
                params.insert(other.name.into(), plausible(other.kind, other.name, rock));
            }
            params.insert(param.name.into(), wrong);
            let answer = match harness.call(method.name, Value::Object(params)) {
                Err(_) => {
                    harness = Harness::new("typed-params-negative");
                    Answer::Panic
                }
                Ok(()) => match harness.replies.try_iter().last() {
                    Some(Response {
                        error: Some(error), ..
                    }) => {
                        let named = error
                            .data
                            .as_ref()
                            .and_then(|d| d.get("param"))
                            .and_then(Value::as_str)
                            == Some(param.name);
                        if error.code == INVALID_PARAMS && named {
                            Answer::Precise
                        } else if error.code == INVALID_PARAMS {
                            Answer::UnnamedInvalidParams
                        } else {
                            Answer::OtherError
                        }
                    }
                    Some(_) => Answer::Success,
                    // Deferred handlers answer after later frames; none did within the budget.
                    None => Answer::OtherError,
                },
            };
            out.push((method.name.to_string(), param.name.to_string(), answer));
        }
    }
    out
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
fn a_wrong_typed_optional_parameter_is_refused_never_accepted() {
    let all = run_all();
    let count = |a: Answer| all.iter().filter(|(_, _, x)| *x == a).count();
    eprintln!(
        "typed params: {} calls: precise {}, other error {}, unnamed invalid_params {}, success {}, panic {}",
        all.len(),
        count(Answer::Precise),
        count(Answer::OtherError),
        count(Answer::UnnamedInvalidParams),
        count(Answer::Success),
        count(Answer::Panic)
    );
    let ignored = |m: &str, p: &str| IGNORED_WHEN_UNUSED.iter().any(|(im, ip)| *im == m && *ip == p);
    let mut problems = Vec::new();
    for (m, p, a) in &all {
        match a {
            Answer::Panic => problems.push(format!("{m} {p}: panicked")),
            Answer::Success if !ignored(m, p) => problems.push(format!(
                "{m} {p}: a wrong-typed value was accepted (read it with crate::params)"
            )),
            Answer::Success => {}
            _ if ignored(m, p) => problems.push(format!(
                "{m} {p}: refused now, remove it from IGNORED_WHEN_UNUSED (stale entry)"
            )),
            _ => {}
        }
    }
    for (m, p) in IGNORED_WHEN_UNUSED {
        if !all.iter().any(|(am, ap, _)| am == m && ap == p) {
            problems.push(format!(
                "{m} {p}: in IGNORED_WHEN_UNUSED but not a callable optional parameter"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{} problems:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// Parameters that name themselves in `data.param` when refused: a floor so the precise answers
/// cannot silently regress (the count printed by the test above was 125 before the second pass).
#[test]
fn precise_refusals_do_not_regress() {
    let all = run_all();
    let precise = all.iter().filter(|(_, _, a)| *a == Answer::Precise).count();
    assert!(
        precise >= 120,
        "only {precise} wrong-typed parameters named themselves in data.param"
    );
}
