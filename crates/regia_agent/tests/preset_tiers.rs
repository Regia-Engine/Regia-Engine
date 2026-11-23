// SPDX-License-Identifier: MPL-2.0
//! Runtime upscaling preserves the profile reported through agent dispatch and the gameplay caps.

use crossbeam_channel::unbounded;
use regia_agent::protocol::Request;
use regia_agent::server::Inbound;
use regia_agent::{AgentInbox, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_framework::ragdoll::{RagdollPlugin, RagdollSettings};
use regia_framework::vehicle_fx::{VehicleFxPlugin, VehicleFxState};
use regia_render::{QualityTier, RenderProfilePreset, RenderSettings, TemporalUpscaler};
use regia_vfx::vehicle_fx::VehicleFxLimits;
use serde_json::json;

#[test]
fn upscaling_keeps_agent_preset_and_gameplay_tiers() {
    let project = regia_test_utils::unique_temp_dir("preset_tiers");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(std::time::Duration::ZERO))
        .insert_resource(AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins((AgentPlugin, RagdollPlugin, VehicleFxPlugin));
    app.finish_plugins();
    let (inbox, requests) = unbounded();
    app.insert_resource(AgentInbox(requests));
    let (reply, replies) = unbounded();

    // First sight of Low must apply its caps; changing to High and back must reapply them.
    for preset in [
        RenderProfilePreset::DesktopLow,
        RenderProfilePreset::DesktopHigh,
        RenderProfilePreset::DesktopLow,
    ] {
        let mut settings = RenderSettings::from_profile(preset);
        settings.profile.temporal_upscaler = TemporalUpscaler::Tsr;
        settings.profile.render_scale = 0.5;
        app.insert_resource(settings);
        inbox
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: "render.get_profile".into(),
                    params: json!({}),
                },
                reply: reply.clone(),
            })
            .expect("queue profile request");
        app.update();
        app.world_mut().run_schedule(FixedUpdate);
        let response = replies.try_recv().expect("profile reply in this frame");
        assert!(response.error.is_none(), "{:?}", response.error);
        let result = response.result.expect("profile result");
        assert_eq!(result["preset"], preset.name());
        assert_eq!(result["profile"]["render_scale"], json!(0.5));
        let tier = QualityTier::of_preset(preset);
        assert_eq!(
            *app.world().resource::<RagdollSettings>(),
            RagdollSettings::for_tier(tier)
        );
        assert_eq!(
            app.world().resource::<VehicleFxState>().config().limits,
            VehicleFxLimits::for_tier(tier)
        );
    }

    // Runtime edits within the tier must not overwrite the game's explicit caps.
    app.world_mut().resource_mut::<RagdollSettings>().max_simulated = 3;
    let mut config = app.world().resource::<VehicleFxState>().config();
    config.limits.max_stamps = 7;
    app.world_mut()
        .resource_mut::<VehicleFxState>()
        .set_config(config);
    {
        let mut settings = app.world_mut().resource_mut::<RenderSettings>();
        settings.profile.temporal_upscaler = TemporalUpscaler::Fsr3;
        settings.profile.render_scale = 0.6;
    }
    app.update();
    app.world_mut().run_schedule(FixedUpdate);
    assert_eq!(app.world().resource::<RagdollSettings>().max_simulated, 3);
    assert_eq!(
        app.world()
            .resource::<VehicleFxState>()
            .config()
            .limits
            .max_stamps,
        7
    );
    drop(app);
    std::fs::remove_dir_all(project).expect("remove test project");
}
