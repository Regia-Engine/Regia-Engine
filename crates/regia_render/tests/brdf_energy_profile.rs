// SPDX-License-Identifier: MPL-2.0
//! `RenderProfile::brdf_energy` reaches the process-wide model (ADR-2632): a profile that opts in
//! sets it at startup, and a `Legacy` profile never undoes a model chosen elsewhere. The model is
//! process-wide, so this has its own test binary and one test.
//!
//! `cargo test -p regia_render --test brdf_energy_profile`

use regia_app::prelude::*;
use regia_asset::{AssetPlugin, AssetServerSettings};
use regia_render::{
    BrdfEnergy, RenderPlugin, RenderProfilePreset, RenderSettings, process_brdf_energy,
    set_process_brdf_energy,
};

/// Runs one headless app's startup with a profile that asks for `mode`.
fn start_with(mode: BrdfEnergy) {
    let dir = regia_test_utils::unique_temp_dir("brdf-energy-profile");
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.profile.brdf_energy = mode;
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AssetPlugin {
            settings: AssetServerSettings {
                project_root: dir.clone(),
                platform: "linux".into(),
                content_dir: None,
            },
        })
        .add_plugins(RenderPlugin { headless: true })
        .insert_resource(settings);
    app.finish_plugins();
    app.update();
    drop(app);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_profile_that_opts_in_sets_the_process_model_and_legacy_leaves_it() {
    set_process_brdf_energy(BrdfEnergy::Legacy);
    start_with(BrdfEnergy::Legacy);
    assert_eq!(
        process_brdf_energy(),
        BrdfEnergy::Legacy,
        "the default stays legacy"
    );

    start_with(BrdfEnergy::Compensated);
    assert_eq!(
        process_brdf_energy(),
        BrdfEnergy::Compensated,
        "a compensated profile reaches the modules composed after startup"
    );

    start_with(BrdfEnergy::Legacy);
    assert_eq!(
        process_brdf_energy(),
        BrdfEnergy::Compensated,
        "a legacy profile does not undo a model chosen by the variable or the host"
    );
    set_process_brdf_energy(BrdfEnergy::Legacy);
}
