// SPDX-License-Identifier: MPL-2.0
//! ADR-8844 (ADR-7977 build A): the integrated-GPU floor selected by name through agent dispatch.
//! `render.set_profile {"preset": "VeryLow"}` applies the Very Low tier with the live adapter,
//! `render.get_profile` reports `tier: "VeryLow"` with `preset` still `DesktopLow`, and a tier
//! that is a row already (`Low`) stays an unknown name whose error lists `VeryLow`.

use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use regia_agent::protocol::{INVALID_PARAMS, Request, Response};
use regia_agent::server::Inbound;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_framework::auto_quality::AdapterOverride;
use regia_render::quality::DeviceClass;
use regia_render::{AdapterProfile, QualityTier, RenderProfilePreset, RenderSettings, SpatialUpscaler};
use serde_json::{Value, json};

struct Harness {
    app: App,
    inbox: Sender<Inbound>,
    reply: Sender<Response>,
    replies: Receiver<Response>,
}

impl Harness {
    fn new() -> Self {
        let project = regia_test_utils::unique_temp_dir("very_low_profile_name");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)))
            .insert_resource(AgentSettings {
                project_dir: Some(project),
            })
            .add_plugins(AgentPlugin);
        app.finish_plugins();
        let (inbox, requests) = unbounded();
        app.insert_resource(AgentInbox(requests));
        app.insert_resource(AgentPermissions {
            config: AgentConfig {
                rate_limit_per_second: u32::MAX,
                ..AgentConfig::default()
            },
            source: None,
        });
        // The live adapter: the Intel N150 the GPU table's `intel_n_series` row measures.
        let mut n150 = AdapterProfile::named("Intel(R) Graphics (N150)", 0x8086, Some(2_048));
        n150.device = 0x46d4;
        n150.class = DeviceClass::Integrated;
        app.insert_resource(AdapterOverride(n150));
        app.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        app.update();
        let (reply, replies) = unbounded();
        Self {
            app,
            inbox,
            reply,
            replies,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Response {
        self.inbox
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply: self.reply.clone(),
            })
            .expect("queue request");
        for _ in 0..2 {
            self.app.update();
        }
        self.replies.try_recv().expect("reply within two frames")
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let response = self.call(method, params);
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("result")
    }
}

#[test]
fn set_profile_very_low_by_name_reports_the_tier_and_keeps_preset_desktop_low() {
    let mut h = Harness::new();
    let got = h.ok("render.get_profile", json!({}));
    assert_eq!(got["preset"], "DesktopHigh");
    assert_ne!(got["tier"], "VeryLow");
    let names: Vec<&str> = got["profile_names"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(
        names.contains(&"VeryLow") && names.contains(&"Medium"),
        "{names:?}"
    );
    assert!(!names.contains(&"Low"), "{names:?}");

    let out = h.ok("render.set_profile", json!({ "preset": "VeryLow" }));
    assert_eq!(out["before"]["preset"], "DesktopHigh");
    assert_eq!(out["after"]["preset"], "DesktopLow");
    assert_eq!(out["after"]["tier"], "VeryLow");
    assert_eq!(out["requested"], "VeryLow");
    assert_eq!(out["gate"]["applied"], "DesktopLow");
    assert_eq!(out["gate"]["honoured"], true);

    let got = h.ok("render.get_profile", json!({}));
    assert_eq!(got["tier"], "VeryLow");
    assert_eq!(got["preset"], "DesktopLow");
    assert_eq!(got["profile"]["spatial_upscaler"], "Sgsr");
    let settings = h.app.world().resource::<RenderSettings>();
    assert_eq!(QualityTier::of_settings(settings), Some(QualityTier::VeryLow));
    assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopLow));
    assert!((settings.profile.render_scale - QualityTier::VERY_LOW_RENDER_SCALE).abs() < 1e-6);
    assert_eq!(settings.profile.spatial_upscaler, SpatialUpscaler::Sgsr);

    // Spellings as for presets; a preset by name afterwards is its own row again.
    let out = h.ok("render.set_profile", json!({ "preset": "desktop_high" }));
    assert_eq!(out["before"]["tier"], "VeryLow");
    assert_eq!(out["after"]["preset"], "DesktopHigh");
    let out = h.ok("render.set_profile", json!({ "preset": "very-low" }));
    assert_eq!(out["after"]["tier"], "VeryLow");
    let out = h.ok("render.set_profile", json!({ "preset": "Medium" }));
    assert_eq!(out["after"]["tier"], "Medium");

    // A customized profile has no tier.
    h.app
        .world_mut()
        .resource_mut::<RenderSettings>()
        .profile
        .csm_resolution = 777;
    let got = h.ok("render.get_profile", json!({}));
    assert_eq!(got["tier"], Value::Null);
}

#[test]
fn set_profile_low_stays_unknown_and_names_very_low_among_the_choices() {
    let mut h = Harness::new();
    for name in ["Low", "High", "Ultra", "UltraLow"] {
        let response = h.call("render.set_profile", json!({ "preset": name }));
        let error = response.error.expect("an unknown name is an error");
        assert_eq!(error.code, INVALID_PARAMS, "{name}");
        assert!(
            error.message.contains("VeryLow") && error.message.contains("DesktopLow"),
            "{name}: {}",
            error.message
        );
    }
    // Nothing was applied.
    let settings = h.app.world().resource::<RenderSettings>();
    assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopHigh));
}
