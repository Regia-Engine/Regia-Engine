// SPDX-License-Identifier: MPL-2.0
//! Ragdoll caps follow the active quality tier (ADR-2424): `RagdollSettings::for_tier` is applied
//! from the app's `RenderSettings` preset, so a phone no longer simulates 16 ragdolls.

use regia_app::prelude::*;
use regia_framework::ragdoll::{RagdollPlugin, RagdollSettings};
use regia_render::{QualityTier, RenderProfilePreset, RenderSettings};

fn app_with(preset: RenderProfilePreset) -> App {
    let mut app = App::new();
    app.insert_resource(RenderSettings::from_profile(preset))
        .add_plugins(RagdollPlugin);
    app
}

#[test]
fn each_tier_gets_its_own_caps() {
    for (preset, tier, cap) in [
        (RenderProfilePreset::MobileLow, QualityTier::MobileLow, 2),
        (RenderProfilePreset::MobileHigh, QualityTier::MobileHigh, 4),
        (RenderProfilePreset::Deck, QualityTier::Deck, 6),
        (RenderProfilePreset::DesktopLow, QualityTier::Low, 8),
        (RenderProfilePreset::DesktopHigh, QualityTier::High, 16),
        (RenderProfilePreset::DesktopUltra, QualityTier::Ultra, 24),
    ] {
        let mut app = app_with(preset);
        app.update();
        let settings = *app.world().resource::<RagdollSettings>();
        assert_eq!(settings, RagdollSettings::for_tier(tier), "{preset:?}");
        assert_eq!(settings.max_simulated, cap, "{preset:?}");
    }
}

#[test]
fn a_tier_change_re_applies_and_an_explicit_edit_sticks_until_then() {
    let mut app = app_with(RenderProfilePreset::DesktopHigh);
    app.update();
    assert_eq!(app.world().resource::<RagdollSettings>().max_simulated, 16);
    // A game's own tuning survives further frames at the same tier.
    app.world_mut().resource_mut::<RagdollSettings>().max_simulated = 5;
    app.update();
    assert_eq!(app.world().resource::<RagdollSettings>().max_simulated, 5);
    // The tier moves (the quality probe stepped down): the new tier's caps apply.
    app.insert_resource(RenderSettings::from_profile(RenderProfilePreset::MobileLow));
    app.update();
    assert_eq!(app.world().resource::<RagdollSettings>().max_simulated, 2);
}

#[test]
fn settings_a_host_inserted_first_are_kept_and_no_render_settings_changes_nothing() {
    let mut app = App::new();
    app.insert_resource(RagdollSettings {
        joint_limits: false,
        max_simulated: 3,
    })
    .insert_resource(RenderSettings::from_profile(RenderProfilePreset::MobileLow))
    .add_plugins(RagdollPlugin);
    app.update();
    assert_eq!(app.world().resource::<RagdollSettings>().max_simulated, 3);

    let mut headless = App::new();
    headless.add_plugins(RagdollPlugin);
    headless.update();
    assert_eq!(
        *headless.world().resource::<RagdollSettings>(),
        RagdollSettings::default()
    );
}
