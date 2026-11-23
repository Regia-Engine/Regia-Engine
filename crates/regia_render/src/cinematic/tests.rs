// SPDX-License-Identifier: MPL-2.0
//! Unit tests of the Cinematic profile (ADR-1403).

use super::*;
use crate::{FrameQuality, RenderSettings, TerrainBudget, TextureStreaming};

/// The shader's interleaved gradient noise (`ign` in `clouds.wgsl`, Jimenez 2014), CPU side.
fn ign(px: [f32; 2], frame: f32) -> f32 {
    let p = [px[0] + 5.588_238 * frame, px[1] + 5.588_238 * frame];
    (52.982_92 * (p[0] * 0.067_110_56 + p[1] * 0.005_837_15).fract()).fract()
}

#[test]
fn cinematic_row_is_ultra_with_the_offline_changes() {
    let ultra = RenderProfile::from_preset(RenderProfilePreset::DesktopUltra);
    let cine = RenderProfile::from_preset(RenderProfilePreset::Cinematic);
    assert_eq!(cine, cinematic_profile());
    // Clouds: full march every frame, more steps, full resolution, stratified samples.
    assert!(!cine.cloud_reprojection);
    assert_eq!(cine.cloud_step_percent, 300);
    assert_eq!(cine.cloud_march, CloudMarch::CINEMATIC);
    assert_eq!(cine.cloud_march.steps(64, cine.cloud_step_percent), 192);
    assert_eq!(
        cine.cloud_march.divisor(2),
        1,
        "the default layer divisor 2 runs at full res"
    );
    assert_eq!(cine.cloud_march.sample_count(), 4);
    assert!(cine.volumetric_clouds && cine.cloud_cone_light);
    // Shadows, lights, LOD, filtering: at or above Ultra, each cap at its ceiling.
    assert_eq!(cine.csm_resolution, 8192);
    assert!(cine.csm_resolution > ultra.csm_resolution);
    assert_eq!(cine.near_sun_tile, ultra.near_sun_tile);
    assert!(
        cine.near_sun_tile * 2 <= cine.local_shadow_atlas_size,
        "local lights keep atlas room"
    );
    assert_eq!(cine.max_visible_lights, crate::light_budget::MAX_UPLOADED_LIGHTS);
    assert_eq!(
        cine.max_lights_per_cluster,
        crate::profile::MAX_LIGHTS_PER_CLUSTER
    );
    let (_, hi) = crate::MESH_LOD_DISTANCE_SCALE_RANGE;
    assert!((cine.mesh_lod_bias * cine.mesh_lod_distance_scale - hi).abs() < 1.0e-6);
    assert!((cine.mesh_lod_shadow_scale - 1.0).abs() < f32::EPSILON);
    assert_eq!(
        cine.terrain_budget.anisotropy_level(),
        TerrainBudget::MAX_ANISOTROPY
    );
    assert!((cine.render_scale - 1.0).abs() < f32::EPSILON);
    assert_eq!(cine.upscale_quality, None);
    assert_eq!(cine.frame_cap_fps, None);
    assert_eq!(cine.texture_streaming, TextureStreaming::DESKTOP_ULTRA);
    // Everything else is Ultra's.
    let back_to_ultra = RenderProfile {
        csm_resolution: ultra.csm_resolution,
        max_lights_per_cluster: ultra.max_lights_per_cluster,
        max_visible_lights: ultra.max_visible_lights,
        cloud_reprojection: ultra.cloud_reprojection,
        cloud_step_percent: ultra.cloud_step_percent,
        cloud_march: ultra.cloud_march,
        mesh_lod_bias: ultra.mesh_lod_bias,
        mesh_lod_distance_scale: ultra.mesh_lod_distance_scale,
        mesh_lod_shadow_scale: ultra.mesh_lod_shadow_scale,
        ..cine.clone()
    };
    assert_eq!(back_to_ultra, ultra);
    // The frame quality carries the march; the light caps survive their clamps.
    let q = FrameQuality::from_profile(&cine);
    assert_eq!(q.cloud_march, CloudMarch::CINEMATIC);
    assert!(!q.cloud_reprojection);
    assert_eq!(q.render_scale_permille, 1000);
    assert_eq!(q.max_visible_lights, crate::light_budget::MAX_UPLOADED_LIGHTS);
    assert_eq!(q.csm_resolution, 8192);
}

#[test]
fn realtime_presets_and_baselines_keep_the_realtime_march() {
    for preset in RenderProfilePreset::ALL {
        let profile = RenderProfile::from_preset(preset);
        assert_eq!(profile.cloud_march, CloudMarch::REALTIME, "{preset:?}");
        assert!(!preset.is_offline());
        // The step count is the v0.1 one for every layer step count and percent the presets use.
        for steps in [0, 1, 16, 32, 64, 65, 96, 128, 200, 1000] {
            let old = (u64::from(steps) * u64::from(profile.cloud_step_percent.max(1)) / 100).clamp(1, 96);
            assert_eq!(
                u64::from(profile.cloud_march.steps(steps, profile.cloud_step_percent)),
                old,
                "{preset:?} {steps}"
            );
        }
        for divisor in 0..=12 {
            assert_eq!(profile.cloud_march.divisor(divisor), divisor.clamp(1, 8));
        }
    }
    assert_eq!(FrameQuality::LAB.cloud_march, CloudMarch::REALTIME);
    assert_eq!(FrameQuality::EDITOR.cloud_march, CloudMarch::REALTIME);
    assert_eq!(CloudMarch::default(), CloudMarch::REALTIME);
}

#[test]
fn cinematic_is_named_offline_and_on_the_ultra_tier() {
    use RenderProfilePreset as P;
    for name in ["Cinematic", "cinematic", "CINEMATIC"] {
        assert_eq!(P::from_name(name), Some(P::Cinematic));
    }
    assert_eq!(P::Cinematic.name(), "Cinematic");
    assert!(P::Cinematic.is_offline());
    assert!(
        !P::ALL.contains(&P::Cinematic),
        "no game menu or detector lists it"
    );
    assert_eq!(P::WITH_OFFLINE[..6], P::ALL);
    assert_eq!(P::WITH_OFFLINE[6], P::Cinematic);
    assert_eq!(QualityTier::of_preset(P::Cinematic), QualityTier::Ultra);
    let settings = RenderSettings::from_profile(P::Cinematic);
    assert_eq!(settings.preset(), Some(P::Cinematic));
    assert!(settings.post.taa);
    assert_eq!(preset_names().len(), 7);
    assert_eq!(preset_names()[6], "Cinematic");
}

#[test]
fn cloud_march_clamps() {
    let wild = CloudMarch {
        samples: 0,
        max_steps: 10_000,
        max_divisor: 0,
    };
    assert_eq!(wild.sample_count(), 1);
    assert_eq!(wild.steps(64, 10_000), CloudMarch::STEP_LIMIT);
    assert_eq!(wild.steps(0, 0), 1);
    assert_eq!(wild.divisor(4), 1);
    let many = CloudMarch {
        samples: 99,
        ..CloudMarch::CINEMATIC
    };
    assert_eq!(many.sample_count(), CloudMarch::MAX_SAMPLES);
    assert_eq!(CloudMarch::CINEMATIC.steps(64, 400), 256);
}

#[test]
fn strata_and_sub_texel_points_cover_the_texel() {
    // March 0 of 1 is the old centred march with the texel's own jitter.
    assert_eq!(CloudMarch::sub_texel(0), [0.5, 0.5]);
    for base in [0.0, 0.25, 0.731, 0.999_9] {
        assert!((CloudMarch::stratum_jitter(base, 0, 1) - base).abs() < 1.0e-6);
    }
    // Each of n marches lands in its own stratum.
    for n in 1..=CloudMarch::MAX_SAMPLES {
        for k in 0..n {
            for base in [0.0, 0.5, 0.999_9] {
                let j = CloudMarch::stratum_jitter(base, k, n);
                let lo = k as f32 / n as f32;
                let hi = (k + 1) as f32 / n as f32;
                assert!(j >= lo - 1.0e-6 && j < hi, "n {n} k {k} base {base}: {j}");
            }
        }
    }
    // The R2 points stay inside the texel and do not repeat.
    let points: Vec<[f32; 2]> = (0..CloudMarch::MAX_SAMPLES).map(CloudMarch::sub_texel).collect();
    for (i, a) in points.iter().enumerate() {
        assert!((0.0..1.0).contains(&a[0]) && (0.0..1.0).contains(&a[1]));
        for b in &points[i + 1..] {
            assert!((a[0] - b[0]).abs() + (a[1] - b[1]).abs() > 1.0e-3);
        }
    }
}

/// The claim behind the profile: the dither of the per-texel start jitter shrinks with the strata.
/// A march with step `dt` that crosses a cloud edge at `edge` counts the samples past it; one
/// march's error spans a whole step from texel to texel (the visible dither), `n` stratified marches
/// keep it within `dt / n`.
#[test]
fn stratified_marches_cut_the_start_jitter_dither() {
    let dt = 1.0_f32;
    let steps = 32;
    let worst = |n: u32| -> f32 {
        let mut worst = 0.0_f32;
        for y in 0..64 {
            for x in 0..64 {
                let base = ign([x as f32 + 0.5, y as f32 + 0.5], 7.0);
                let edge = 10.0 + 0.37 * dt; // the cloud starts inside step 10
                let truth = steps as f32 * dt - edge;
                let mut covered = 0.0;
                for k in 0..n {
                    let jitter = CloudMarch::stratum_jitter(base, k, n);
                    let hits = (0..steps).filter(|i| (*i as f32 + jitter) * dt >= edge).count();
                    covered += hits as f32 * dt;
                }
                worst = worst.max((covered / n as f32 - truth).abs());
            }
        }
        worst
    };
    let one = worst(1);
    let four = worst(4);
    assert!(one > 0.4 * dt, "one march dithers by about a step: {one}");
    assert!(
        four <= dt / 4.0 + 1.0e-4,
        "four strata stay within a quarter step: {four}"
    );
    assert!(worst(16) <= dt / 16.0 + 1.0e-4);
}

#[test]
fn shader_matches_the_cpu_constants() {
    let wgsl = include_str!("../../shaders/clouds.wgsl");
    assert!(wgsl.contains(&format!(
        "const MAX_MARCH_SAMPLES: i32 = {};",
        CloudMarch::MAX_SAMPLES
    )));
    assert!(wgsl.contains(&format!("for (var i = 0; i < {}; i++)", CloudMarch::STEP_LIMIT)));
    assert!(wgsl.contains("vec2<f32>(0.7548777, 0.5698403)"));
    assert!(wgsl.contains("let stratum = (jitter + f32(k)) / f32(n);"));
}

#[test]
fn the_gate_keeps_cinematic_off_small_tiers_and_android() {
    use RenderProfilePreset as P;
    // Only Cinematic is gated.
    for preset in P::ALL {
        for tier in QualityTier::ALL {
            assert!(gate_preset(preset, Some(tier), false, false).honoured());
            assert!(gate_preset(preset, Some(tier), true, false).honoured());
        }
    }
    // High and Ultra run it; no detected tier runs it.
    for tier in [Some(QualityTier::High), Some(QualityTier::Ultra), None] {
        let gate = gate_preset(P::Cinematic, tier, false, false);
        assert!(gate.honoured() && gate.reason.is_none(), "{tier:?}");
    }
    // Below High: the tier's own preset, unless forced.
    for (tier, fallback) in [
        (QualityTier::Medium, P::DesktopHigh),
        (QualityTier::Low, P::DesktopLow),
        (QualityTier::Deck, P::Deck),
        (QualityTier::MobileHigh, P::DesktopLow),
        (QualityTier::MobileLow, P::DesktopLow),
    ] {
        let gate = gate_preset(P::Cinematic, Some(tier), false, false);
        assert_eq!(gate.applied, fallback, "{tier:?}");
        assert_eq!(gate.reason, Some("cinematic_tier_too_low"));
        assert!(gate_preset(P::Cinematic, Some(tier), false, true).honoured());
    }
    // Android never runs it, forced or not.
    for force in [false, true] {
        let gate = gate_preset(P::Cinematic, Some(QualityTier::Ultra), true, force);
        assert_eq!(gate.applied, P::MobileHigh);
        assert_eq!(gate.reason, Some("cinematic_needs_desktop"));
    }
}

#[test]
fn the_flag_is_taken_out_of_the_arguments() -> Result<(), PresetChoiceError> {
    let args = [
        "scene",
        "--render-profile",
        "cinematic",
        "--plain",
        "--render-profile=DesktopUltra",
        "--size",
        "8x8",
    ];
    let (preset, rest) = take_preset_arg(args)?;
    assert_eq!(
        preset,
        Some(RenderProfilePreset::DesktopUltra),
        "the last one wins"
    );
    assert_eq!(rest, vec!["scene", "--plain", "--size", "8x8"]);
    let (preset, rest) = take_preset_arg(["a", "b"])?;
    assert_eq!(preset, None);
    assert_eq!(rest, vec!["a", "b"]);
    let missing = take_preset_arg(["--render-profile"]);
    assert_eq!(
        missing.as_ref().map_err(PresetChoiceError::code),
        Err("render_profile_missing_value")
    );
    let empty = take_preset_arg(["--render-profile="]);
    assert_eq!(
        empty.as_ref().map_err(PresetChoiceError::code),
        Err("render_profile_missing_value")
    );
    let unknown = take_preset_arg(["--render-profile", "film"]);
    let err = unknown.err().map(|e| (e.code(), e.to_string()));
    assert!(
        matches!(&err, Some((code, text)) if *code == "render_profile_unknown" && text.contains("Cinematic")),
        "{err:?}"
    );
    // A look-alike flag is left for the host.
    let (preset, rest) = take_preset_arg(["--render-profiles", "x"])?;
    assert_eq!(preset, None);
    assert_eq!(rest.len(), 2);
    Ok(())
}

#[test]
fn the_scene_file_and_the_precedence() -> Result<(), PresetChoiceError> {
    assert_eq!(parse_preset_file("")?, None);
    assert_eq!(
        parse_preset_file("# offline\n\n  Cinematic  # the film\n")?,
        Some(RenderProfilePreset::Cinematic)
    );
    assert_eq!(
        parse_preset_file("nope").map_err(|e| e.code()),
        Err("render_profile_unknown")
    );

    let dir = regia_test_utils::unique_temp_dir("cinematic-scene");
    assert_eq!(preset_from_scene_dir(&dir)?, None, "no file, no preset");
    let default = RenderProfilePreset::DesktopHigh;
    let pick = |cli, env| choose_preset(cli, env, Some(dir.as_path()), default);
    assert_eq!(pick(None, None)?.source, PresetSource::Default);
    regia_test_utils::write_atomic(&dir.join(RENDER_PROFILE_FILE), b"cinematic\n").map_err(|e| {
        PresetChoiceError::Unreadable {
            path: dir.clone(),
            reason: e.to_string(),
        }
    })?;
    let scene = pick(None, None)?;
    assert_eq!(
        (scene.preset, scene.source),
        (RenderProfilePreset::Cinematic, PresetSource::SceneDir)
    );
    let env = pick(None, Some("desktop_ultra"))?;
    assert_eq!(
        (env.preset, env.source),
        (RenderProfilePreset::DesktopUltra, PresetSource::Environment)
    );
    assert_eq!(
        pick(None, Some("  "))?.source,
        PresetSource::SceneDir,
        "blank env is unset"
    );
    let cli = pick(Some(RenderProfilePreset::Deck), Some("DesktopUltra"))?;
    assert_eq!(
        (cli.preset, cli.source),
        (RenderProfilePreset::Deck, PresetSource::CommandLine)
    );
    assert_eq!(
        pick(None, Some("film")).map_err(|e| e.code()),
        Err("render_profile_unknown")
    );
    assert_eq!(PresetSource::SceneDir.name(), "scene_dir");
    // A directory where the file should be cannot be read as one.
    let odd = regia_test_utils::unique_temp_dir("cinematic-odd");
    let _ = std::fs::create_dir_all(odd.join(RENDER_PROFILE_FILE));
    assert_eq!(
        preset_from_scene_dir(&odd).map_err(|e| e.code()),
        Err("render_profile_unreadable")
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&odd);
    Ok(())
}

/// ADR-8844 (ADR-7977 build A): every source a host reads its profile from takes a tier name.
#[test]
fn choose_profile_reads_tier_names_from_every_source() -> Result<(), PresetChoiceError> {
    let very_low = ProfileName::Tier(QualityTier::VeryLow);
    // The command line, both spellings; the flag leaves the host's own arguments alone.
    let (profile, rest) = take_profile_arg(["scene", "--render-profile", "VeryLow", "--plain"])?;
    assert_eq!(profile, Some(very_low));
    assert_eq!(rest, vec!["scene", "--plain"]);
    let (profile, _) = take_profile_arg(["--render-profile=very-low"])?;
    assert_eq!(profile, Some(very_low));
    let (profile, _) = take_profile_arg(["--render-profile", "DesktopLow"])?;
    assert_eq!(
        profile,
        Some(ProfileName::Preset(RenderProfilePreset::DesktopLow))
    );
    assert_eq!(
        take_profile_arg(["--render-profile"]).map_err(|e| e.code()),
        Err("render_profile_missing_value")
    );
    let low = take_profile_arg(["--render-profile", "Low"])
        .err()
        .map(|e| (e.code(), e.to_string()));
    assert!(
        matches!(&low, Some((code, text)) if *code == "render_profile_unknown" && text.contains("VeryLow")),
        "{low:?}"
    );

    // The scene file, with a comment line.
    assert_eq!(
        parse_profile_file("# the integrated-GPU floor\n\n  VeryLow  # N150\n")?,
        Some(very_low)
    );
    assert_eq!(
        parse_profile_file("Medium")?,
        Some(ProfileName::Tier(QualityTier::Medium))
    );
    assert_eq!(
        parse_profile_file("High").map_err(|e| e.code()),
        Err("render_profile_unknown")
    );

    let dir = regia_test_utils::unique_temp_dir("profile-name-scene");
    let default = RenderProfilePreset::DesktopHigh;
    let pick = |cli, env| choose_profile(cli, env, Some(dir.as_path()), default);
    let none = pick(None, None)?;
    assert_eq!(
        (none.profile, none.source),
        (ProfileName::Preset(default), PresetSource::Default)
    );
    regia_test_utils::write_atomic(&dir.join(RENDER_PROFILE_FILE), b"# floor\nvery_low\n").map_err(|e| {
        PresetChoiceError::Unreadable {
            path: dir.clone(),
            reason: e.to_string(),
        }
    })?;
    assert_eq!(profile_from_scene_dir(&dir)?, Some(very_low));
    let scene = pick(None, None)?;
    assert_eq!((scene.profile, scene.source), (very_low, PresetSource::SceneDir));
    // The environment beats the scene file; blank counts as unset.
    let env = pick(None, Some("Medium"))?;
    assert_eq!(
        (env.profile, env.source),
        (ProfileName::Tier(QualityTier::Medium), PresetSource::Environment)
    );
    assert_eq!(pick(None, Some(" "))?.source, PresetSource::SceneDir);
    // The command line beats both.
    let cli = pick(
        Some(ProfileName::Preset(RenderProfilePreset::Deck)),
        Some("VeryLow"),
    )?;
    assert_eq!(
        (cli.profile, cli.source),
        (
            ProfileName::Preset(RenderProfilePreset::Deck),
            PresetSource::CommandLine
        )
    );
    assert_eq!(
        pick(None, Some("Ultra")).map_err(|e| e.code()),
        Err("render_profile_unknown")
    );
    // Very Low gates as Desktop Low: it runs as asked.
    let gate = gate_preset(very_low.preset(), Some(QualityTier::VeryLow), false, false);
    assert!(gate.honoured());
    assert_eq!(gate.applied, RenderProfilePreset::DesktopLow);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// `choose_preset` and its readers are unchanged: a tier name is still no preset.
#[test]
fn choose_preset_still_refuses_tier_names() {
    let dir = regia_test_utils::unique_temp_dir("profile-name-preset");
    for name in ["VeryLow", "very_low", "Medium"] {
        assert_eq!(
            choose_preset(None, Some(name), None, RenderProfilePreset::DesktopHigh).map_err(|e| e.code()),
            Err("render_profile_unknown"),
            "{name}"
        );
        assert_eq!(
            take_preset_arg(["--render-profile", name]).map_err(|e| e.code()),
            Err("render_profile_unknown")
        );
        assert_eq!(
            parse_preset_file(name).map_err(|e| e.code()),
            Err("render_profile_unknown")
        );
    }
    let text = choose_preset(
        None,
        Some("VeryLow"),
        Some(dir.as_path()),
        RenderProfilePreset::DesktopHigh,
    )
    .err()
    .map(|e| e.to_string())
    .unwrap_or_default();
    assert!(
        text.contains("DesktopLow") && !text.contains("VeryLow, "),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
