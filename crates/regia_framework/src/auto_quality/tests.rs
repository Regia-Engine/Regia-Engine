// SPDX-License-Identifier: MPL-2.0
use std::path::PathBuf;
use std::time::Duration;

use regia_render::quality::{ProbeSource, QualityProbe};
use regia_render::{RenderProfilePreset, RenderSettings};

use super::*;
use crate::settings::{SettingsPath, SettingsPlugin};

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("regia-autoq-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    dir.join("settings.ron")
}

fn gpu(name: &str, vram_mb: u64) -> AdapterProfile {
    AdapterProfile::named(name, 0x10de, Some(vram_mb))
}

/// A launch: settings at `path`, the adapter pinned, the probe off unless `confirm`.
fn launch(path: Option<PathBuf>, adapter: AdapterProfile, confirm: bool) -> App {
    let mut app = App::new();
    app.insert_resource(SettingsPath(path))
        .insert_resource(AdapterOverride(adapter))
        .insert_resource(AutoQuality {
            confirm,
            ..AutoQuality::default()
        })
        .add_plugins((MinimalPlugins, SettingsPlugin, AutoQualityPlugin));
    if confirm {
        app.init_resource::<QualityProbe>();
    }
    app
}

fn render_tier(app: &App) -> Option<QualityTier> {
    app.world()
        .get_resource::<RenderSettings>()
        .and_then(QualityTier::of_settings)
}

fn graphics(app: &App) -> crate::settings::GraphicsSettings {
    app.world().resource::<GameSettings>().graphics.clone()
}

#[test]
fn first_launch_detects_applies_and_persists() {
    let path = temp_path("first");
    let mut app = launch(
        Some(path.clone()),
        gpu("NVIDIA GeForce RTX 3080 Ti", 12_288),
        false,
    );
    app.update();
    let g = graphics(&app);
    assert_eq!(g.preset, GraphicsPreset::High);
    assert_eq!(g.source(), PresetSource::Detected);
    let detected = g.detected.as_ref().map(|d| d.reason.clone()).unwrap_or_default();
    assert!(
        detected.starts_with("NVIDIA GeForce RTX 3080 Ti, 12 GB → High"),
        "{detected}"
    );
    assert_eq!(render_tier(&app), Some(QualityTier::High));
    assert!(path.exists(), "the detection is saved");
    // Detection runs once per first launch, not every frame.
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 1);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn detection_runs_once_across_launches() {
    let path = temp_path("once");
    {
        let mut app = launch(Some(path.clone()), gpu("NVIDIA GeForce GTX 1080", 8_192), false);
        app.update();
        assert_eq!(graphics(&app).preset, GraphicsPreset::Low);
    }
    // Second launch, even on another GPU: the saved preset stands and nothing is detected.
    let mut app = launch(Some(path.clone()), gpu("NVIDIA GeForce RTX 5090", 32_607), false);
    app.update();
    app.update();
    assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 0);
    let g = graphics(&app);
    assert_eq!(
        (g.preset, g.source()),
        (GraphicsPreset::Low, PresetSource::Detected)
    );
    assert_eq!(
        render_tier(&app),
        Some(QualityTier::Low),
        "the saved preset is applied"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_saved_player_choice_is_never_overridden() {
    let path = temp_path("player");
    let mut chosen = GameSettings::default();
    chosen.graphics.choose_preset(GraphicsPreset::Low);
    let text = chosen.to_ron().unwrap_or_default();
    assert!(std::fs::write(&path, text).is_ok());
    let mut app = launch(Some(path.clone()), gpu("NVIDIA GeForce RTX 5090", 32_607), true);
    for _ in 0..3 {
        app.update();
    }
    let g = graphics(&app);
    assert_eq!(
        (g.preset, g.source()),
        (GraphicsPreset::Low, PresetSource::Player)
    );
    assert!(g.detected.is_none());
    assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 0);
    assert!(
        !app.world().resource::<QualityProbe>().is_active(),
        "no probe either"
    );
    assert_eq!(render_tier(&app), Some(QualityTier::Low));
    let _ = std::fs::remove_file(&path);
}

/// A saved preset is applied at plugin build, before the host's `Startup`: a host tweak of
/// `RenderSettings.profile` made at startup survives the first frames (no silent re-seed).
#[test]
fn a_saved_preset_applies_before_host_startup_tweaks() {
    let path = temp_path("host-tweak");
    let mut chosen = GameSettings::default();
    chosen.graphics.choose_preset(GraphicsPreset::High);
    let text = chosen.to_ron().unwrap_or_default();
    assert!(std::fs::write(&path, text).is_ok());
    let mut app = launch(Some(path.clone()), gpu("NVIDIA GeForce RTX 5090", 32_607), false);
    assert_eq!(
        render_tier(&app),
        Some(QualityTier::High),
        "applied at build, before the first frame"
    );
    app.add_systems(Startup, |mut render: ResMut<RenderSettings>| {
        render.profile.render_scale = 0.67;
    });
    for _ in 0..3 {
        app.update();
    }
    let scale = app.world().resource::<RenderSettings>().profile.render_scale;
    assert!(
        (scale - 0.67).abs() < 1e-6,
        "the host's startup tweak is kept: {scale}"
    );
    assert_eq!(graphics(&app).preset, GraphicsPreset::High);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn older_files_without_a_source_keep_a_chosen_preset() {
    // A file from before detection with a non-default preset: someone chose it.
    let path = temp_path("legacy");
    let old = "(graphics:(preset:Ultra,shadow_quality:None,render_scale:None),display:(resolution:None,vsync:true,frame_cap:None),audio:(master:1.0,music:1.0,sfx:1.0),input:(sensitivity:1.0,invert_y:false),language:\"\",accessibility:(subtitles:true,text_scale:1.0))";
    assert!(std::fs::write(&path, old).is_ok());
    let mut app = launch(Some(path.clone()), gpu("NVIDIA GeForce GTX 1080", 8_192), false);
    app.update();
    assert_eq!(graphics(&app).preset, GraphicsPreset::Ultra);
    assert_eq!(graphics(&app).source(), PresetSource::Player);
    assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 0);

    // The same file with the default preset (Medium) was never touched: detect.
    let medium = old.replace("preset:Ultra", "preset:Medium");
    assert!(std::fs::write(&path, medium).is_ok());
    let mut app = launch(Some(path.clone()), gpu("NVIDIA GeForce GTX 1080", 8_192), false);
    app.update();
    assert_eq!(graphics(&app).preset, GraphicsPreset::Low);
    assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 1);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_menu_change_becomes_the_players_and_redetect_replaces_it() {
    let mut app = launch(None, gpu("NVIDIA GeForce RTX 3080 Ti", 12_288), false);
    app.update();
    assert_eq!(graphics(&app).source(), PresetSource::Detected);
    // The options menu writes the preset directly (as `settings.set` does).
    app.world_mut().resource_mut::<GameSettings>().graphics.preset = GraphicsPreset::Ultra;
    app.update();
    let g = graphics(&app);
    assert_eq!(
        (g.preset, g.source()),
        (GraphicsPreset::Ultra, PresetSource::Player)
    );
    assert_eq!(render_tier(&app), Some(QualityTier::Ultra));
    app.update();
    assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 1);

    // "Auto-detect" in the menu.
    app.world_mut().resource_mut::<AutoQuality>().request_redetect();
    app.update();
    let g = graphics(&app);
    assert_eq!(
        (g.preset, g.source()),
        (GraphicsPreset::High, PresetSource::Detected)
    );
    assert_eq!(render_tier(&app), Some(QualityTier::High));
    assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 2);
}

#[test]
fn the_probe_steps_down_once_when_over_budget() {
    let path = temp_path("probe");
    let mut app = launch(
        Some(path.clone()),
        gpu("NVIDIA GeForce RTX 3080 Ti", 12_288),
        true,
    );
    app.update();
    assert!(app.world().resource::<AutoQuality>().is_probing());
    assert!(app.world().resource::<QualityProbe>().is_active());
    // The window path would time ~3 s behind the splash; hand it a 1440p result over 16.7 ms.
    app.world_mut()
        .resource_mut::<QualityProbe>()
        .complete_with(ProbeResult {
            frames: 240,
            p50_ms: 14.0,
            p95_ms: 21.5,
            source: ProbeSource::Gpu,
            width: 2560,
            height: 1440,
            timed_out: false,
            aborted: false,
            gpu_busy: None,
        });
    app.update();
    let auto = app.world().resource::<AutoQuality>();
    assert!(!auto.is_probing());
    assert!(auto.last_verdict().is_some_and(|v| v.stepped_down));
    let g = graphics(&app);
    assert_eq!(
        (g.preset, g.source()),
        (GraphicsPreset::Medium, PresetSource::Detected)
    );
    let Some(detected) = g.detected else {
        panic!("detection record");
    };
    assert_eq!(
        (detected.detected_tier.as_str(), detected.tier.as_str()),
        ("High", "Medium")
    );
    let probe = detected.probe.unwrap_or_default();
    assert!(probe.stepped_down && probe.p95_ms == 21.5 && probe.clock == "gpu");
    assert_eq!(render_tier(&app), Some(QualityTier::Medium));
    let medium = app.world().resource::<RenderSettings>();
    assert_eq!(medium.profile.render_scale, QualityTier::MEDIUM_RENDER_SCALE);
    // Persisted with the probe, and the next launch does nothing.
    drop(app);
    let mut again = launch(
        Some(path.clone()),
        gpu("NVIDIA GeForce RTX 3080 Ti", 12_288),
        true,
    );
    again.update();
    assert_eq!(graphics(&again).preset, GraphicsPreset::Medium);
    assert!(!again.world().resource::<QualityProbe>().is_active());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_probe_within_budget_keeps_the_tier() {
    let mut app = launch(None, gpu("NVIDIA GeForce RTX 5080", 16_303), true);
    app.update();
    app.world_mut()
        .resource_mut::<QualityProbe>()
        .complete_with(ProbeResult {
            frames: 400,
            p50_ms: 5.0,
            p95_ms: 7.9,
            source: ProbeSource::Gpu,
            width: 2560,
            height: 1440,
            timed_out: false,
            aborted: false,
            gpu_busy: None,
        });
    app.update();
    assert_eq!(graphics(&app).preset, GraphicsPreset::Ultra);
    assert_eq!(render_tier(&app), Some(QualityTier::Ultra));
}

#[test]
fn disabled_or_without_settings_nothing_changes() {
    let mut app = App::new();
    app.insert_resource(SettingsPath(None))
        .insert_resource(AdapterOverride(gpu("NVIDIA GeForce GTX 1080", 8_192)))
        .insert_resource(AutoQuality {
            enabled: false,
            ..AutoQuality::default()
        })
        .add_plugins((MinimalPlugins, SettingsPlugin, AutoQualityPlugin));
    app.update();
    assert_eq!(graphics(&app).source(), PresetSource::Unset);
    // `RenderSettings` exists from plugin build on (a host `Startup` system may take it), and no
    // preset was applied over it: it is still the untouched default.
    assert_eq!(
        app.world()
            .get_resource::<RenderSettings>()
            .map(|s| s.profile.clone()),
        Some(RenderSettings::default().profile),
        "no preset applied; the default stands"
    );
    // A game's own settings inserted before the plugin are kept as they are.
    let mut app = App::new();
    app.insert_resource(SettingsPath(None))
        .insert_resource(AdapterOverride(gpu("NVIDIA GeForce GTX 1080", 8_192)))
        .insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopUltra))
        .insert_resource(AutoQuality {
            enabled: false,
            ..AutoQuality::default()
        })
        .add_plugins((MinimalPlugins, SettingsPlugin, AutoQualityPlugin));
    app.update();
    assert_eq!(
        render_tier(&app),
        Some(QualityTier::Ultra),
        "the game's settings stand"
    );

    // No GameSettings (no SettingsPlugin): the game's RenderSettings are not touched.
    let mut app = App::new();
    app.insert_resource(AdapterOverride(gpu("NVIDIA GeForce GTX 1080", 8_192)))
        .insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopUltra))
        .add_plugins((MinimalPlugins, AutoQualityPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.update();
    assert_eq!(render_tier(&app), Some(QualityTier::Ultra));
    assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 0);
}

/// v0.1.0 blocker: a first launch (no settings file, preset `Unset`) with a host `Startup`
/// system taking `ResMut<RenderSettings>` must not panic ("Resource does not exist"): the
/// settings plugin inserts the default at build. Detection still applies its preset afterwards.
#[test]
fn first_launch_host_startup_can_take_render_settings() {
    fn host_startup(mut render: ResMut<RenderSettings>) {
        render.profile.render_scale = 0.67;
    }
    let path = temp_path("first-startup");
    assert!(!path.exists(), "no settings file: a first launch");
    let mut app = launch(
        Some(path.clone()),
        gpu("NVIDIA GeForce RTX 3080 Ti", 12_288),
        false,
    );
    app.add_systems(Startup, host_startup);
    // Startup runs inside the first update; it panicked before the fix.
    app.update();
    let g = graphics(&app);
    assert_eq!(
        (g.preset, g.source()),
        (GraphicsPreset::High, PresetSource::Detected),
        "detection still runs on the first launch"
    );
    assert_eq!(
        render_tier(&app),
        Some(QualityTier::High),
        "the detected preset is applied over the default"
    );
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 1);
    let _ = std::fs::remove_file(&path);
}

fn terrain_vt(app: &App) -> bool {
    app.world()
        .get_resource::<RenderSettings>()
        .is_some_and(|r| r.profile.terrain_budget.virtual_texture)
}

#[test]
fn detected_low_on_an_8_gb_card_turns_the_terrain_virtual_texture_on() {
    // ADR-157 addendum v0.1.2: the GTX 1080 detects Low and gets the terrain virtual texture; an RTX 3080 Ti
    // (High) and a 4 GB card on Low do not. Linux only (the review of v012-vt-horizon).
    for (name, vram, tier, vt) in [
        (
            "NVIDIA GeForce GTX 1080",
            8_192,
            QualityTier::Low,
            cfg!(target_os = "linux"),
        ),
        ("NVIDIA GeForce GTX 1650", 4_096, QualityTier::Low, false),
        ("NVIDIA GeForce RTX 3080 Ti", 12_288, QualityTier::High, false),
    ] {
        let mut app = launch(None, gpu(name, vram), false);
        app.update();
        app.update();
        assert_eq!(render_tier(&app), Some(tier), "{name}");
        assert_eq!(terrain_vt(&app), vt, "{name}");
    }
}

/// ADR-2122: a host's `TerrainVtPolicy { high: true }` resource turns the lever on for a card
/// that detects High with 12 GB (Linux), and AMD with `unmeasured_vendors`; without the resource
/// the same cards keep today's frame.
#[test]
fn the_terrain_vt_policy_resource_extends_the_lever_to_high_and_amd() {
    use regia_render::quality::TerrainVtPolicy;
    let policy = TerrainVtPolicy {
        high: true,
        unmeasured_vendors: true,
        ..TerrainVtPolicy::default()
    };
    let rx_6600 = AdapterProfile::named("AMD Radeon RX 6600", 0x1002, Some(8_176));
    for (adapter, tier, with_policy) in [
        (gpu("NVIDIA GeForce RTX 3080 Ti", 12_288), QualityTier::High, true),
        (rx_6600, QualityTier::Low, true),
        (gpu("NVIDIA GeForce RTX 3080", 10_240), QualityTier::High, false),
    ] {
        let name = adapter.name.clone();
        let mut plain = launch(None, adapter.clone(), false);
        plain.update();
        plain.update();
        assert_eq!(render_tier(&plain), Some(tier), "{name}");
        assert!(!terrain_vt(&plain), "{name} without a policy");

        let mut app = launch(None, adapter, false);
        app.insert_resource(policy);
        app.update();
        app.update();
        assert_eq!(render_tier(&app), Some(tier), "{name}: the lever is masked");
        assert_eq!(
            terrain_vt(&app),
            with_policy && cfg!(target_os = "linux"),
            "{name} with the policy"
        );
        let budget = app.world().resource::<RenderSettings>().profile.terrain_budget;
        assert!(
            !budget.vt_late_depth,
            "{name}: only `windows` sets the late-depth bit"
        );
    }

    // `windows` carries the late-depth bit with the lever (ADR-2122).
    let mut app = launch(None, gpu("NVIDIA GeForce GTX 1080", 8_192), false);
    app.insert_resource(TerrainVtPolicy {
        windows: true,
        ..TerrainVtPolicy::default()
    });
    app.update();
    app.update();
    assert_eq!(render_tier(&app), Some(QualityTier::Low));
    let admitted = cfg!(any(target_os = "linux", target_os = "windows"));
    let budget = app.world().resource::<RenderSettings>().profile.terrain_budget;
    assert_eq!(
        (budget.virtual_texture, budget.vt_late_depth),
        (admitted, admitted)
    );
}

#[test]
fn a_saved_low_preset_gets_the_terrain_vt_once_the_device_appears() {
    // At plugin build there is no render device: the lever waits for the first frame that
    // has the adapter, then applies once.
    let path = temp_path("vt-late");
    let mut chosen = GameSettings::default();
    chosen.graphics.choose_preset(GraphicsPreset::Low);
    assert!(std::fs::write(&path, chosen.to_ron().unwrap_or_default()).is_ok());
    let mut app = App::new();
    app.insert_resource(SettingsPath(Some(path.clone())))
        .add_plugins((MinimalPlugins, SettingsPlugin, AutoQualityPlugin));
    assert_eq!(render_tier(&app), Some(QualityTier::Low));
    assert!(!terrain_vt(&app), "no adapter yet");
    app.update();
    assert!(!terrain_vt(&app), "still no adapter");
    app.insert_resource(AdapterOverride(gpu("NVIDIA GeForce GTX 1080", 8_192)));
    app.update();
    // (The default is Linux only: ADR-157 addendum v0.1.2.)
    assert_eq!(terrain_vt(&app), cfg!(target_os = "linux"));
    assert_eq!(render_tier(&app), Some(QualityTier::Low));
    // A host that turns it off afterwards keeps its choice: the lever applies once.
    app.world_mut()
        .resource_mut::<RenderSettings>()
        .profile
        .terrain_budget
        .virtual_texture = false;
    app.update();
    assert!(!terrain_vt(&app));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_infinite_probe_length_does_not_abort_the_detect_system() {
    let mut app = launch(None, gpu("NVIDIA GeForce RTX 3080 Ti", 12_288), true);
    app.world_mut().resource_mut::<AutoQuality>().probe_seconds = f32::INFINITY;
    app.update();
    app.world_mut().resource_mut::<AutoQuality>().probe_seconds = 1.0e20;
    app.update();
    assert!(
        AutoQuality::default()
            .with_probe_seconds(f32::INFINITY)
            .probe_seconds
            .is_finite()
    );
}

/// ADR-7882: a Low pick now runs the probe (Low has a step down), and a Low that misses 30 fps on sustained GPU time
/// ends on Very Low, saved as the detected preset with the step and its budget; one that holds 30 fps keeps Low.
#[test]
fn a_low_pick_that_misses_30_fps_steps_down_to_very_low() {
    let iris = || {
        let mut a = AdapterProfile::named("Intel(R) Iris(R) Xe Graphics", 0x8086, None);
        a.class = DeviceClass::Integrated;
        a
    };
    let probe = |p50_ms: f32, p95_ms: f32| ProbeResult {
        frames: 120,
        p50_ms,
        p95_ms,
        source: ProbeSource::Gpu,
        width: 1920,
        height: 1080,
        timed_out: false,
        aborted: false,
        gpu_busy: Some(0.95),
    };
    let path = temp_path("very-low");
    let mut app = launch(Some(path.clone()), iris(), true);
    app.update();
    assert_eq!(graphics(&app).preset, GraphicsPreset::Low);
    assert!(
        app.world().resource::<AutoQuality>().is_probing(),
        "Low is probed now"
    );
    app.world_mut()
        .resource_mut::<QualityProbe>()
        .complete_with(probe(68.0, 75.0));
    app.update();
    let g = graphics(&app);
    assert_eq!(
        (g.preset, g.source()),
        (GraphicsPreset::VeryLow, PresetSource::Detected)
    );
    let Some(detected) = g.detected else {
        panic!("detection record");
    };
    assert_eq!(
        (detected.detected_tier.as_str(), detected.tier.as_str()),
        ("Low", "VeryLow")
    );
    let record = detected.probe.unwrap_or_default();
    assert!(record.stepped_down);
    assert!(
        (record.budget_ms - 1000.0 / 30.0).abs() < 1e-3,
        "{}",
        record.budget_ms
    );
    assert!(detected.reason.contains("Low → VeryLow"), "{}", detected.reason);
    assert_eq!(render_tier(&app), Some(QualityTier::VeryLow));
    let render = app.world().resource::<RenderSettings>();
    assert_eq!(render.profile.render_scale, QualityTier::VERY_LOW_RENDER_SCALE);
    assert_eq!(render.preset(), Some(RenderProfilePreset::DesktopLow));
    // The next launch reads it back and probes nothing.
    drop(app);
    let mut again = launch(Some(path.clone()), iris(), true);
    again.update();
    assert_eq!(graphics(&again).preset, GraphicsPreset::VeryLow);
    assert_eq!(render_tier(&again), Some(QualityTier::VeryLow));
    assert!(!again.world().resource::<QualityProbe>().is_active());
    drop(again);
    // The player's choice is never overridden: Low chosen in the menu stays Low.
    let mut chosen = GameSettings::default();
    chosen.graphics.choose_preset(GraphicsPreset::Low);
    assert!(std::fs::write(&path, chosen.to_ron().unwrap_or_default()).is_ok());
    let mut player = launch(Some(path.clone()), iris(), true);
    player.update();
    assert_eq!(
        (graphics(&player).preset, graphics(&player).source()),
        (GraphicsPreset::Low, PresetSource::Player)
    );
    assert!(!player.world().resource::<QualityProbe>().is_active());
    let _ = std::fs::remove_file(&path);

    // Between 30 and 60 fps (33.3 ms budget not missed): Low is kept.
    let mut app = launch(None, iris(), true);
    app.update();
    app.world_mut()
        .resource_mut::<QualityProbe>()
        .complete_with(probe(24.0, 28.0));
    app.update();
    assert_eq!(graphics(&app).preset, GraphicsPreset::Low);
    assert_eq!(render_tier(&app), Some(QualityTier::Low));
}

/// Execute the actual test binary beside staged build info, from an unrelated working directory.
#[test]
#[cfg(target_os = "linux")]
fn packaged_profile_precedes_auto_at_startup() {
    const CHILD: &str = "REGIA_TEST_PACKAGED_PROFILE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let mut app = launch(None, gpu("NVIDIA GeForce RTX 5090", 32_607), false);
        #[cfg(feature = "media")]
        {
            app.add_plugins(crate::boot_movies::BootMoviesPlugin::default());
            assert_eq!(
                app.world().resource::<regia_media::BootMovies>().movies,
                ["movies/intro.webm"]
            );
            assert_eq!(
                app.world()
                    .resource::<crate::boot_movies::BootMoviesOrigin>()
                    .source,
                "packaged"
            );
        }
        assert_eq!(
            app.world().resource::<RenderSettings>().preset(),
            Some(RenderProfilePreset::MobileLow),
            "the exact packaged preset is applied before Startup, without a desktop-tier conversion"
        );
        app.update();
        app.update();
        assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 0);
        assert_eq!(
            app.world().resource::<RenderSettings>().preset(),
            Some(RenderProfilePreset::MobileLow)
        );
        // Explicit Auto is a deliberate change for this launch, including repeated updates.
        app.world_mut().resource_mut::<AutoQuality>().request_redetect();
        app.update();
        app.update();
        assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 1);
        assert_eq!(render_tier(&app), Some(QualityTier::Ultra));

        let root = regia_test_utils::unique_temp_dir("packaged-profile-settings");
        std::fs::create_dir_all(&root).expect("settings dir");
        let path = root.join("settings.ron");
        for source in [PresetSource::Detected, PresetSource::Player] {
            let mut saved = GameSettings::default();
            saved.graphics.preset = GraphicsPreset::High;
            saved.graphics.preset_source = Some(source);
            let bytes = ron::to_string(&saved).expect("serialize settings");
            std::fs::write(&path, &bytes).expect("settings");
            let mut app = launch(Some(path.clone()), gpu("NVIDIA GeForce RTX 5090", 32_607), false);
            let expected = if source == PresetSource::Player {
                RenderProfilePreset::DesktopHigh
            } else {
                RenderProfilePreset::MobileLow
            };
            assert_eq!(app.world().resource::<RenderSettings>().preset(), Some(expected));
            app.update();
            app.update();
            assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 0);
            assert_eq!(app.world().resource::<RenderSettings>().preset(), Some(expected));
            assert_eq!(std::fs::read_to_string(&path).expect("saved bytes"), bytes);
            app.world_mut()
                .resource_mut::<GameSettings>()
                .graphics
                .choose_preset(GraphicsPreset::Ultra);
            app.update();
            assert_eq!(render_tier(&app), Some(QualityTier::Ultra));
        }
        std::fs::remove_dir_all(root).expect("settings cleanup");
        // DefaultPlugins has AutoQualityPlugin even when the game has no SettingsPlugin.
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AutoQualityPlugin));
        assert_eq!(
            app.world().resource::<RenderSettings>().preset(),
            Some(RenderProfilePreset::MobileLow)
        );
        let exe = std::env::current_exe().expect("child exe");
        let package = exe.parent().expect("package dir");
        for expected in RenderProfilePreset::WITH_OFFLINE {
            let bytes =
                serde_json::to_vec(&serde_json::json!({"render_profile": expected.name()})).expect("json");
            std::fs::write(package.join("build_info.tmp"), bytes).expect("write metadata");
            std::fs::rename(package.join("build_info.tmp"), package.join("build_info.json"))
                .expect("publish metadata");
            let mut app = launch(None, gpu("NVIDIA GeForce RTX 5090", 32_607), false);
            assert_eq!(app.world().resource::<RenderSettings>().preset(), Some(expected));
            app.update();
            app.world_mut()
                .resource_mut::<GameSettings>()
                .graphics
                .render_scale = Some(0.5);
            app.update();
            assert_eq!(app.world().resource::<RenderSettings>().profile.render_scale, 0.5);
            assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 0);
        }
        // ADR-8844: `regia package` writes a tier name (`VeryLow`); the package starts on that tier, its preset
        // still Desktop Low, and Auto does not run.
        std::fs::write(package.join("build_info.tmp"), br#"{"render_profile":"VeryLow"}"#)
            .expect("tier metadata");
        std::fs::rename(package.join("build_info.tmp"), package.join("build_info.json"))
            .expect("publish metadata");
        let mut app = launch(None, gpu("NVIDIA GeForce RTX 5090", 32_607), false);
        assert_eq!(render_tier(&app), Some(QualityTier::VeryLow));
        assert_eq!(
            app.world().resource::<RenderSettings>().preset(),
            Some(RenderProfilePreset::DesktopLow)
        );
        app.update();
        app.update();
        assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 0);
        assert_eq!(render_tier(&app), Some(QualityTier::VeryLow));
        for invalid in [
            r#"{"render_profile":"unknown"}"#,
            r#"{"render_profile":"Low"}"#,
            r#"{"render_profile":4}"#,
            "{broken",
        ] {
            std::fs::write(package.join("build_info.tmp"), invalid).expect("invalid metadata");
            std::fs::rename(package.join("build_info.tmp"), package.join("build_info.json"))
                .expect("publish metadata");
            let mut app = launch(None, gpu("NVIDIA GeForce RTX 5090", 32_607), false);
            app.update();
            assert_eq!(app.world().resource::<AutoQuality>().detections_run(), 1);
            assert_eq!(render_tier(&app), Some(QualityTier::Ultra));
        }
        return;
    }
    let root = regia_test_utils::unique_temp_dir("packaged-render-profile");
    let package = root.join("package");
    let unrelated = root.join("unrelated");
    std::fs::create_dir_all(&package).expect("package");
    std::fs::create_dir_all(&unrelated).expect("working directory");
    std::fs::write(
        package.join("build_info.json"),
        r#"{"render_profile":"MobileLow","boot":{"movies":["movies/intro.webm"]}}"#,
    )
    .expect("build info");
    let exe = package.join("game");
    std::fs::copy(std::env::current_exe().expect("test exe"), &exe).expect("stage executable");
    let output = std::process::Command::new(exe)
        .args([
            "--exact",
            "auto_quality::tests::packaged_profile_precedes_auto_at_startup",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .current_dir(unrelated)
        .output()
        .expect("launch package");
    std::fs::remove_dir_all(root).expect("cleanup");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// v0.5.1 (DO27 tester B: the bench recorded 1920x1050 after the game's guard left
/// fullscreen): a probe that drew 1920x1050 on a 1920x1080 monitor with fullscreen asked for
/// records the monitor and the mode, marks `size_mismatch`, and probes again on the first frame
/// the window fills the monitor; one that drew at the monitor size marks nothing.
#[test]
fn a_probe_smaller_than_the_fullscreen_monitor_is_marked_and_runs_again() {
    let mut app = launch(None, gpu("NVIDIA GeForce RTX 3080 Ti", 12_288), true);
    let display = |window: [u32; 2]| ProbeDisplay {
        fullscreen: true,
        window_size: Some(window),
        monitor_size: Some([1920, 1080]),
    };
    app.insert_resource(DisplayOverride(display([1920, 1050])));
    app.update();
    assert!(app.world().resource::<AutoQuality>().is_probing());
    let result = |height: u32| ProbeResult {
        frames: 240,
        p50_ms: 9.0,
        p95_ms: 11.0,
        source: ProbeSource::Gpu,
        width: 1920,
        height,
        timed_out: false,
        aborted: false,
        gpu_busy: None,
    };
    app.world_mut()
        .resource_mut::<QualityProbe>()
        .complete_with(result(1050));
    app.update();
    let probe = graphics(&app).detected.and_then(|d| d.probe).unwrap_or_default();
    assert_eq!(probe.size, [1920, 1050]);
    assert_eq!(probe.monitor_size, Some([1920, 1080]));
    assert_eq!(probe.window_mode, "borderless_fullscreen");
    assert!(probe.size_mismatch, "{probe:?}");
    let auto = app.world().resource::<AutoQuality>();
    assert!(auto.reprobe_pending() && !auto.is_probing());
    // Still smaller than the monitor: no new probe.
    app.update();
    assert!(!app.world().resource::<QualityProbe>().is_active());
    assert!(app.world().resource::<AutoQuality>().reprobe_pending());
    // The window fills the monitor: the probe runs again and its record matches.
    app.insert_resource(DisplayOverride(display([1920, 1080])));
    app.update();
    assert!(app.world().resource::<AutoQuality>().is_probing());
    assert!(app.world().resource::<QualityProbe>().is_active());
    assert!(!app.world().resource::<AutoQuality>().reprobe_pending());
    app.world_mut()
        .resource_mut::<QualityProbe>()
        .complete_with(result(1080));
    app.update();
    let probe = graphics(&app).detected.and_then(|d| d.probe).unwrap_or_default();
    assert_eq!(probe.size, [1920, 1080]);
    assert!(!probe.size_mismatch, "{probe:?}");
    assert!(!app.world().resource::<AutoQuality>().reprobe_pending());

    // A windowed host never marks a mismatch.
    let windowed = ProbeDisplay {
        fullscreen: false,
        ..display([1280, 720])
    };
    assert!(!windowed.size_mismatch([1280, 720]));
    assert_eq!(windowed.mode_name(), "windowed");
    assert_eq!(ProbeDisplay::default().mode_name(), "");
}

/// v0.5.1 review (rev-e row 3): a slow probe that drew smaller than the fullscreen monitor is not
/// the monitor's number, so it steps nothing down: the preset, the detected tier and the render
/// tier stay the table's pick, and the probe again at the monitor size starts from that pick. A
/// fast second probe then keeps it (confirmation only steps down, so a step taken on the
/// mismatched sample would have stuck).
#[test]
fn a_slow_probe_smaller_than_the_fullscreen_monitor_steps_nothing_down() {
    let mut app = launch(None, gpu("NVIDIA GeForce RTX 3080 Ti", 12_288), true);
    let display = |window: [u32; 2]| ProbeDisplay {
        fullscreen: true,
        window_size: Some(window),
        monitor_size: Some([1920, 1080]),
    };
    app.insert_resource(DisplayOverride(display([1920, 1050])));
    app.update();
    assert_eq!(graphics(&app).preset, GraphicsPreset::High);
    assert!(app.world().resource::<AutoQuality>().is_probing());
    let result = |height: u32, p50_ms: f32, p95_ms: f32| ProbeResult {
        frames: 240,
        p50_ms,
        p95_ms,
        source: ProbeSource::Gpu,
        width: 1920,
        height,
        timed_out: false,
        aborted: false,
        gpu_busy: Some(0.95),
    };
    // Slow enough to step High down at the monitor size, but drawn 1920x1050.
    app.world_mut()
        .resource_mut::<QualityProbe>()
        .complete_with(result(1050, 30.0, 40.0));
    app.update();
    let g = graphics(&app);
    assert_eq!(
        (g.preset, g.source()),
        (GraphicsPreset::High, PresetSource::Detected),
        "a mismatched probe changed the preset"
    );
    let detected = g.detected.unwrap_or_default();
    assert_eq!(detected.tier, "High", "{detected:?}");
    let probe = detected.probe.unwrap_or_default();
    assert!(probe.size_mismatch, "{probe:?}");
    assert!(
        !probe.stepped_down,
        "a mismatched probe is recorded as not applied: {probe:?}"
    );
    assert_eq!(render_tier(&app), Some(QualityTier::High));
    assert!(app.world().resource::<AutoQuality>().reprobe_pending());
    // The window fills the monitor: the second probe runs from High and holds 60 fps.
    app.insert_resource(DisplayOverride(display([1920, 1080])));
    app.update();
    assert!(app.world().resource::<QualityProbe>().is_active());
    app.world_mut()
        .resource_mut::<QualityProbe>()
        .complete_with(result(1080, 9.0, 11.0));
    app.update();
    let g = graphics(&app);
    assert_eq!(
        g.preset,
        GraphicsPreset::High,
        "the second probe started below the table's pick"
    );
    assert_eq!(g.detected.unwrap_or_default().tier, "High");
    assert_eq!(render_tier(&app), Some(QualityTier::High));
}
