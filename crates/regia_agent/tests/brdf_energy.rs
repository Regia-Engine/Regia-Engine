// SPDX-License-Identifier: MPL-2.0
//! ADR-2632: `render.set_brdf_energy { "mode": "compensated" }` end to end, through the agent
//! protocol's real dispatch path (permission check, rate limit, handler, reply). The model is
//! process-wide and every forward module composed after the change takes it, so this runs in its
//! own test binary (own process) instead of beside the library's tests, which expect `legacy`.
//! No window, no GPU, no socket, no wall clock: a channel stands in for the WebSocket inbox.

use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use regia_agent::protocol::{Request, Response};
use regia_agent::server::Inbound;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_render::{BrdfEnergy, RenderProfilePreset, RenderSettings};
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

/// A headless app with the agent plugin, a render profile on a shipped preset, and a channel in
/// place of the WebSocket inbox.
struct Harness {
    app: App,
    inbox: Sender<Inbound>,
    replies: Receiver<Response>,
    reply: Sender<Response>,
    project: std::path::PathBuf,
}

impl Harness {
    fn new() -> Self {
        let project = unique_temp_dir("agent-brdf-energy");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(AgentSettings {
                project_dir: Some(project.clone()),
            })
            .add_plugins(AgentPlugin)
            .insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh))
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
        app.update();
        let (reply, replies) = unbounded();
        Self {
            app,
            inbox,
            replies,
            reply,
            project,
        }
    }

    /// Sends one request, runs two frames, and returns the one reply's result or error.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        };
        self.inbox
            .send(Inbound {
                request,
                reply: self.reply.clone(),
            })
            .expect("inbox open");
        for _ in 0..2 {
            self.app.update();
        }
        let response = self.replies.try_recv().expect("one reply per request");
        match (response.result, response.error) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) => Err(serde_json::to_value(error).expect("error serializes")),
            other => panic!("malformed response {other:?}"),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.project);
    }
}

#[test]
fn set_compensated_reaches_the_process_model_and_the_profile_and_keeps_the_preset() {
    let mut harness = Harness::new();

    // The default: legacy everywhere, the preset recognized. A host that sets
    // `REGIA_BRDF_ENERGY` starts from its model instead; the read must report that.
    let got = harness.call("render.brdf_energy", json!({})).expect("read");
    assert_eq!(got["mode"], regia_render::process_brdf_energy().name(), "{got}");
    assert_eq!(got["profile"], "legacy", "{got}");
    assert_eq!(got["preset"], RenderProfilePreset::DesktopHigh.name(), "{got}");
    if got["env"].is_null() {
        assert_eq!(got["mode"], "legacy", "{got}");
        assert_eq!(regia_render::process_brdf_energy(), BrdfEnergy::Legacy);
    }

    // Opt in through the protocol.
    let got = harness
        .call("render.set_brdf_energy", json!({ "mode": "compensated" }))
        .expect("set compensated");
    assert_eq!(got["mode"], "compensated", "{got}");
    assert_eq!(got["profile"], "compensated", "{got}");
    // The field is masked in `preset()`: the shipped preset is still reported.
    assert_eq!(got["preset"], RenderProfilePreset::DesktopHigh.name(), "{got}");
    assert_eq!(regia_render::process_brdf_energy(), BrdfEnergy::Compensated);
    let settings = harness.app.world().resource::<RenderSettings>();
    assert_eq!(settings.profile.brdf_energy, BrdfEnergy::Compensated);
    // (Forward modules compose through `process_brdf_energy`; regia_render's
    // `brdf_energy::tests` prove the define it adds and that every module validates with it.)

    // A read answers the same state.
    let read = harness.call("render.brdf_energy", json!({})).expect("read");
    assert_eq!(read["mode"], "compensated", "{read}");

    // A bad mode is refused with its code and leaves the model alone.
    let err = harness
        .call("render.set_brdf_energy", json!({ "mode": "energy" }))
        .expect_err("bad mode");
    assert_eq!(err["data"]["code"], "brdf_energy_bad_mode", "{err}");
    assert_eq!(regia_render::process_brdf_energy(), BrdfEnergy::Compensated);

    // And back to the default.
    let got = harness
        .call("render.set_brdf_energy", json!({ "mode": "legacy" }))
        .expect("set legacy");
    assert_eq!(got["mode"], "legacy", "{got}");
    assert_eq!(regia_render::process_brdf_energy(), BrdfEnergy::Legacy);
}
