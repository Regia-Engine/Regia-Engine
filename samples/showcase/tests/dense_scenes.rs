// SPDX-License-Identifier: MIT-0
//! The non-plain desktop forest shot asks for the dense-scene visibility path
//! (`RenderProfile::dense_scenes`). Plain mode and the other presets stay on the preset row.
//! A few headless updates of the example scene must not panic; if the engine then turns the
//! path off, the host leaves that status.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use regia::prelude::App;
use regia_render::{
    RenderDevice, RenderProfile, RenderProfilePreset, RenderSettings, RuntimeRenderer, TemporalUpscaler,
};
use showcase::app::{Options, build_headless_app, render_settings};
use showcase::scene::load_scene;

fn settings(preset: RenderProfilePreset, plain: bool) -> RenderSettings {
    render_settings(&Options {
        plain,
        profile: Some(preset),
        ..Options::default()
    })
}

fn assert_dense(preset: RenderProfilePreset) {
    let got = settings(preset, false);
    let mut expected = RenderProfile::dense_scenes(preset).expect("desktop high and ultra");
    expected.gpu_driven = got.profile.gpu_driven;
    assert!(got.profile.visibility_buffer, "{preset:?}");
    assert_eq!(
        got.profile.visibility_path, expected.visibility_path,
        "{preset:?}"
    );
    assert!(
        matches!(
            got.profile.visibility_path.select,
            regia_render::VisibilitySelect::Dense(_)
        ),
        "{preset:?} {}",
        got.profile.visibility_path.select.as_str()
    );
    assert!(got.profile.visibility_path.single_pass_resolve, "{preset:?}");
    assert_eq!(
        got.profile, expected,
        "{preset:?} keeps the preset apart from the path"
    );
    let row = RenderProfile::from_preset(preset);
    assert!(got.profile.gi_probes, "{preset:?}");
    assert_eq!(got.profile.gi_probes, row.gi_probes);
    assert_eq!(got.profile.temporal_upscaler, TemporalUpscaler::Taau);
    assert_eq!(got.profile.temporal_upscaler, row.temporal_upscaler);
    assert!(
        got.gi_look.is_none(),
        "{preset:?} does not override the lighting option"
    );
    let seeded = RenderSettings::from_profile(preset);
    assert_eq!(got.post.exposure_ev, seeded.post.exposure_ev);
    assert_eq!(got.post.auto_exposure, seeded.post.auto_exposure);
    assert_eq!(got.post.taa, seeded.post.taa);
}

fn assert_preset_row(preset: RenderProfilePreset, plain: bool) {
    let got = settings(preset, plain);
    let row = RenderProfile::from_preset(preset);
    assert!(!got.profile.visibility_buffer, "{preset:?} plain={plain}");
    assert_eq!(
        got.profile.visibility_path, row.visibility_path,
        "{preset:?} plain={plain}"
    );
    assert_eq!(got.profile.gi_probes, row.gi_probes, "{preset:?}");
    assert_eq!(got.profile.temporal_upscaler, row.temporal_upscaler, "{preset:?}");
    assert!(got.gi_look.is_none(), "{preset:?}");
    // Plain mode still pins exposure and turns TAA and bloom off. The profile row otherwise matches.
    if !plain {
        let mut expected = row;
        expected.gpu_driven = got.profile.gpu_driven;
        assert_eq!(got.profile, expected, "{preset:?}");
    }
}

#[test]
fn non_plain_desktop_high_and_ultra_request_the_dense_path() {
    assert_dense(RenderProfilePreset::DesktopHigh);
    assert_dense(RenderProfilePreset::DesktopUltra);
    // The default options are non-plain Desktop High.
    let defaulted = render_settings(&Options::default());
    assert!(defaulted.profile.visibility_buffer);
    assert_eq!(
        defaulted.profile.visibility_path,
        RenderProfile::dense_scenes(RenderProfilePreset::DesktopHigh)
            .expect("desktop high")
            .visibility_path
    );
    assert!(defaulted.profile.gi_probes);
    assert_eq!(defaulted.profile.temporal_upscaler, TemporalUpscaler::Taau);

    let cpu_draw = render_settings(&Options {
        gpu_driven: false,
        ..Options::default()
    });
    assert!(cpu_draw.profile.visibility_buffer);
    assert!(!cpu_draw.profile.gpu_driven);
    assert_eq!(cpu_draw.profile.temporal_upscaler, TemporalUpscaler::Taau);
}

#[test]
fn plain_desktop_high_and_the_other_presets_stay_on_the_preset_row() {
    assert_preset_row(RenderProfilePreset::DesktopHigh, true);
    assert_preset_row(RenderProfilePreset::DesktopUltra, true);
    for preset in [
        RenderProfilePreset::MobileLow,
        RenderProfilePreset::MobileHigh,
        RenderProfilePreset::DesktopLow,
        RenderProfilePreset::Deck,
    ] {
        assert!(RenderProfile::dense_scenes(preset).is_none(), "{preset:?}");
        assert_preset_row(preset, false);
        let got = settings(preset, false);
        assert!(
            got.profile.gi_probes || got.gi_look.is_none(),
            "{preset:?} must not take an explicit depth look with gi_probes off"
        );
    }
}

/// What the renderer reports after the updates. Printed, and left as the engine set it.
#[derive(Clone, Debug)]
struct PathStatus {
    enabled: bool,
    active: bool,
    select: &'static str,
    fallback: Option<&'static str>,
    dense: bool,
}

fn path_status(app: &App) -> PathStatus {
    let settings = app.world().resource::<RenderSettings>();
    let renderer = app.world().get_resource::<RuntimeRenderer>();
    let report = renderer.map(|runtime| runtime.0.visbuffer_resolve_report());
    let gate = renderer.map(|runtime| runtime.0.visbuffer_dense_report());
    PathStatus {
        enabled: settings.profile.visibility_buffer,
        active: renderer.is_some_and(|runtime| runtime.0.visbuffer_active()),
        select: settings.profile.visibility_path.select.as_str(),
        fallback: report.and_then(|report| report.fallback.map(|reason| reason.as_str())),
        dense: gate.is_some_and(|gate| gate.dense),
    }
}

/// Fixed updates of the example scene. Not a wall-clock budget.
const HEADLESS_UPDATES: u32 = 4;

#[test]
fn non_plain_desktop_high_headless_updates_leave_the_engines_status() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase dense-scenes headless updates: {err}");
        return;
    }
    let scene = Arc::new(
        load_scene(&Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene")).expect("example scene"),
    );
    let mut app = build_headless_app(
        scene,
        Options {
            plain: false,
            profile: Some(RenderProfilePreset::DesktopHigh),
            size: (320, 180),
            ..Options::default()
        },
        None,
    )
    .expect("headless app");
    assert!(
        app.world().resource::<RenderSettings>().profile.visibility_buffer,
        "the shot requests the path before the first update"
    );
    for _ in 0..HEADLESS_UPDATES {
        app.update();
    }
    let status = path_status(&app);
    // The engine may turn the path off (the device, a sparse scene). Record that and do not
    // write the switch back on.
    eprintln!(
        "visibility status after {HEADLESS_UPDATES} updates: enabled={} active={} select={} fallback={:?} dense={}",
        status.enabled, status.active, status.select, status.fallback, status.dense
    );
    let profile = &app.world().resource::<RenderSettings>().profile;
    assert!(profile.gi_probes);
    assert_eq!(profile.temporal_upscaler, TemporalUpscaler::Taau);
    if !status.enabled {
        app.update();
        let again = path_status(&app);
        eprintln!("visibility status left off: {again:?}");
        assert!(
            !again.enabled,
            "the host does not force the path back on: {again:?}"
        );
        return;
    }
    // The engine left the request on. A later disable must stick for one more update.
    app.world_mut()
        .resource_mut::<RenderSettings>()
        .profile
        .visibility_buffer = false;
    app.update();
    let again = path_status(&app);
    eprintln!("visibility status after a disable: {again:?}");
    assert!(
        !again.enabled,
        "the host does not force the path back on: {again:?}"
    );
}
