// SPDX-License-Identifier: MPL-2.0
//! `render.get_profile` / `render.set_profile`: the active render profile (`specs/13` § 3,
//! guide topic `profile`). Lets an agent A/B the Mobile Low / Mobile High / Desktop tiers on the
//! running app, a phone included, and measure each with `play.hitch_report`.
//!
//! `render.get_sssr_cutoff` / `render.set_sssr_cutoff` (v0.1.14): the stochastic SSR roughness
//! cutoff knob ([`regia_render::set_sssr_roughness_cutoff`]).
//!
//! `render.mesh_lod` (v0.1.20, ADR-260): the mesh LOD tier scales in effect and the switch
//! distances they give a mesh of a given radius on every preset.
//!
//! The offline `Cinematic` preset (v0.2.0, ADR-1403): its tier gate and the cloud march
//! overrides live in `render_cinematic`.

use regia_ecs::prelude::*;
use regia_render::{
    LodViewport, MESH_LOD_DISTANCE_SCALE_RANGE, MESH_LOD_SHADOW_SCALE_RANGE, MeshLodScales, ProfileName,
    QualityTier, RenderProfile, RenderProfilePreset, RenderSettings,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn profile_json(profile: &RenderProfile) -> Value {
    json!({
        "render_scale": profile.render_scale,
        "anti_aliasing": format!("{:?}", profile.anti_aliasing),
        "spatial_upscaler": format!("{:?}", profile.spatial_upscaler),
        "depth_prepass": profile.depth_prepass,
        "bloom": profile.bloom,
        "auto_exposure": profile.auto_exposure,
        "terrain_anisotropy": profile.terrain_budget.anisotropy_level(),
        "cloud_reprojection": profile.cloud_reprojection,
        "cloud_step_percent": profile.cloud_step_percent,
        "cloud_cone_light": profile.cloud_cone_light,
        "cloud_march": crate::render_cinematic::march_json(profile),
        "csm_resolution": profile.csm_resolution,
        "near_sun_tile": profile.near_sun_tile,
        "max_visible_lights": profile.max_visible_lights,
        "mesh_lod_bias": profile.mesh_lod_bias,
        "mesh_lod_distance_scale": profile.mesh_lod_distance_scale,
        "mesh_lod_shadow_scale": profile.mesh_lod_shadow_scale,
    })
}

fn settings_json(settings: &RenderSettings) -> Value {
    json!({
        "preset": settings.preset().map(RenderProfilePreset::name),
        // The quality-ladder tier (ADR-8844): `VeryLow` while `preset` answers `DesktopLow`;
        // null for a customized profile.
        "tier": QualityTier::of_settings(settings).map(QualityTier::name),
        "profile": profile_json(&settings.profile),
        "post_enabled": settings.post.enabled,
    })
}

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RenderSettings resource in this app",
        "the app runs without regia_render's RenderPlugin",
    )
}

fn presets() -> Vec<&'static str> {
    regia_render::cinematic::preset_names()
}

/// The startup choice and the package candidate, shared by profile and quality status.
pub(crate) fn configuration(world: &World) -> Value {
    use regia_framework::packaged_config::PackagedConfig;
    let package = world.get_resource::<PackagedConfig>();
    json!({
        "source": regia_framework::packaged_config::profile_source(world),
        "packaged_profile": package.and_then(PackagedConfig::render_profile_name).map(ProfileName::name),
        "location": package.and_then(PackagedConfig::location),
        "error": package.and_then(PackagedConfig::error),
    })
}

/// `render.get_profile`: the active preset (`null` when customised), its key settings, the
/// platform default and the preset names `render.set_profile` accepts.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let settings = world.get_resource::<RenderSettings>().ok_or_else(no_renderer)?;
    let mut out = settings_json(settings);
    out["configuration"] = configuration(world);
    out["platform_default"] = json!(RenderProfilePreset::platform_default().name());
    // What `RenderSettings::default()` picks on this device: `Deck` on a Steam Deck (ADR-3289).
    out["runtime_default"] = json!(RenderProfilePreset::runtime_default().name());
    out["presets"] = json!(presets());
    // Every name `render.set_profile` takes: the presets, then the tiers with no row of their own
    // (ADR-8844: `VeryLow`, `Medium`).
    out["profile_names"] = json!(ProfileName::names());
    out["offline_presets"] = json!(crate::render_cinematic::offline_presets());
    // Android thermal state and the quality governor that reacts to it (idle on desktop).
    let state = world
        .get_resource::<regia_platform::ThermalState>()
        .copied()
        .unwrap_or_default();
    let governor = world.get_resource::<regia_framework::thermal::ThermalGovernor>();
    out["thermal"] = json!({
        "status": state.status.token(),
        "headroom": state.headroom,
        "governor_enabled": governor.is_some_and(|g| g.enabled),
        "governor_level": governor.map_or(0, |g| g.level()),
        "base_render_scale": governor.and_then(|g| g.base_scale()),
    });
    Ok(out)
}

/// An optional positive number parameter.
fn opt_scale(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_f64() {
            Some(v) if v.is_finite() && v > 0.0 => Ok(Some(v as f32)),
            _ => Err(RpcError::invalid_params(format!(
                "`{key}` must be a positive number"
            ))),
        },
    }
}

/// `render.set_profile { preset?, post?, mesh_lod_distance_scale?, mesh_lod_shadow_scale?,
/// cloud_samples?, cloud_step_percent?, cloud_reprojection?, cloud_max_divisor?, force? }`: switches the live profile
/// (applied on the next frame); `post` turns the post stack on or off; the two mesh LOD tier
/// scales override the preset's (clamped to their ranges); the four cloud fields override the
/// cloud march (ADR-1403). `preset` is required unless an override is given. `Cinematic` is
/// gated by platform and GPU tier (`force` runs it on a desktop tier below High). `preset` also
/// takes the quality-ladder tiers with no row of their own (`VeryLow`, `Medium`; ADR-8844), applied
/// with the live adapter. Returns `{ before, after }`, plus `gate` and `requested` when a preset
/// was asked for.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let distance = opt_scale(params, "mesh_lod_distance_scale")?;
    let shadow = opt_scale(params, "mesh_lod_shadow_scale")?;
    let clouds = crate::render_cinematic::overrides(params)?;
    let preset = match params.get("preset") {
        None | Some(Value::Null) if distance.is_some() || shadow.is_some() || clouds.any() => None,
        value => {
            let name = value.and_then(Value::as_str).ok_or_else(|| {
                RpcError::invalid_params(
                    "`preset` (string) is required unless a mesh LOD scale or a cloud override is given",
                )
            })?;
            Some(ProfileName::parse(name).ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "unknown preset `{name}`; one of {}",
                    ProfileName::names().join(", ")
                ))
            })?)
        }
    };
    let post = match params.get("post") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(RpcError::invalid_params("`post` must be a boolean")),
    };
    if world.get_resource::<RenderSettings>().is_none() {
        return Err(no_renderer());
    }
    // A tier name gates as its preset (Very Low as Desktop Low, which is never gated).
    let gated = preset.map(|name| crate::render_cinematic::gate(world, name.preset(), clouds.force));
    // A tier applies with the live adapter and the host's terrain policy, as auto quality does.
    let adapter = regia_framework::auto_quality::current_adapter(world);
    let policy = world
        .get_resource::<regia_render::quality::TerrainVtPolicy>()
        .copied()
        .unwrap_or_default();
    let mut settings = world
        .get_resource_mut::<RenderSettings>()
        .ok_or_else(no_renderer)?;
    let before = settings_json(&settings);
    if let (Some(name), Some((gate, _))) = (preset, &gated) {
        if gate.honoured() {
            name.apply_for_gpu_with(&mut settings, adapter.as_ref(), &policy);
        } else {
            settings.apply_preset(gate.applied);
        }
    }
    if let Some(enabled) = post {
        settings.post.enabled = enabled;
    }
    if let Some(scale) = distance {
        let (lo, hi) = MESH_LOD_DISTANCE_SCALE_RANGE;
        settings.profile.mesh_lod_distance_scale = scale.clamp(lo, hi);
    }
    if let Some(scale) = shadow {
        let (lo, hi) = MESH_LOD_SHADOW_SCALE_RANGE;
        settings.profile.mesh_lod_shadow_scale = scale.clamp(lo, hi);
    }
    clouds.apply(&mut settings.profile);
    let mut out = json!({ "before": before, "after": settings_json(&settings) });
    if let Some((gate, tier)) = &gated {
        out["gate"] = crate::render_cinematic::gate_json(gate, *tier);
    }
    if let Some(name) = preset {
        out["requested"] = json!(name.name());
    }
    Ok(out)
}

/// Most thresholds `render.mesh_lod` accepts (a chain longer than this is not a real chain).
const MAX_THRESHOLDS: usize = 16;

fn scales_json(scales: &MeshLodScales) -> Value {
    json!({
        "bias": scales.bias,
        "distance_scale": scales.distance,
        "shadow_scale": scales.shadow,
        "color_factor": scales.color_factor(1.0),
    })
}

fn distances_json(scales: &MeshLodScales, radius: f32, thresholds: &[f32], height: u32, fov_y: f32) -> Value {
    let row = |shadow: bool| -> Vec<Option<f32>> {
        thresholds
            .iter()
            .take(thresholds.len().saturating_sub(1))
            .map(|t| scales.switch_distance(radius, *t, height, fov_y, 1.0, shadow))
            .collect()
    };
    json!({ "color_m": row(false), "shadow_m": row(true) })
}

/// `render.mesh_lod { radius?, thresholds?, height?, fov_y? }`: the tier scales in effect and,
/// for a mesh of bounding radius `radius` with the stored `thresholds` (LOD 0 first), the
/// distances where its colour and shadow LOD switch, for the active profile and every preset.
pub(crate) fn mesh_lod(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let settings = world.get_resource::<RenderSettings>().ok_or_else(no_renderer)?;
    let radius = match params.get("radius") {
        None | Some(Value::Null) => 1.0,
        Some(v) => match v.as_f64() {
            Some(r) if r.is_finite() && r > 0.0 && r < 1.0e6 => r as f32,
            _ => {
                return Err(RpcError::invalid_params(
                    "`radius` must be a positive number of metres",
                ));
            }
        },
    };
    let thresholds: Vec<f32> = match params.get("thresholds") {
        None | Some(Value::Null) => vec![1.0, 0.5, 0.25, 0.0],
        Some(Value::Array(items)) if !items.is_empty() && items.len() <= MAX_THRESHOLDS => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.as_f64() {
                    Some(t) if t.is_finite() && t >= 0.0 => out.push(t as f32),
                    _ => {
                        return Err(RpcError::invalid_params(
                            "`thresholds` must hold non-negative numbers",
                        ));
                    }
                }
            }
            out
        }
        Some(_) => {
            return Err(RpcError::invalid_params(format!(
                "`thresholds` must be an array of 1 to {MAX_THRESHOLDS} numbers"
            )));
        }
    };
    let viewport = world.get_resource::<LodViewport>().copied().unwrap_or_default();
    let height = match params.get("height") {
        None | Some(Value::Null) => viewport.height,
        Some(v) => match v.as_u64() {
            Some(h) if (1..=16_384).contains(&h) => h as u32,
            _ => return Err(RpcError::invalid_params("`height` must be 1..=16384 pixels")),
        },
    };
    let fov_y = match params.get("fov_y") {
        None | Some(Value::Null) => 1.0,
        Some(v) => match v.as_f64() {
            Some(f) if f.is_finite() && f > 0.0 && f < std::f64::consts::PI => f as f32,
            _ => return Err(RpcError::invalid_params("`fov_y` must be in radians, 0..π")),
        },
    };
    let active = MeshLodScales::of_profile(&settings.profile);
    let presets: Vec<Value> = RenderProfilePreset::ALL
        .iter()
        .map(|preset| {
            let scales = MeshLodScales::of_profile(&RenderProfile::from_preset(*preset));
            json!({
                "preset": preset.name(),
                "scales": scales_json(&scales),
                "switch": distances_json(&scales, radius, &thresholds, height, fov_y),
            })
        })
        .collect();
    Ok(json!({
        "preset": settings.preset().map(RenderProfilePreset::name),
        "scales": scales_json(&active),
        "ranges": {
            "bias": [0.25, 4.0],
            "color_factor": [MESH_LOD_DISTANCE_SCALE_RANGE.0, MESH_LOD_DISTANCE_SCALE_RANGE.1],
            "shadow_scale": [MESH_LOD_SHADOW_SCALE_RANGE.0, MESH_LOD_SHADOW_SCALE_RANGE.1],
        },
        "radius": radius,
        "thresholds": thresholds,
        "height": height,
        "fov_y": fov_y,
        "switch": distances_json(&active, radius, &thresholds, height, fov_y),
        "presets": presets,
    }))
}

fn sssr_cutoff_json() -> Value {
    json!({
        "cutoff": regia_render::sssr_roughness_cutoff(),
        "default": regia_render::SSSR_ROUGHNESS_CUTOFF,
        "env_override": regia_render::sssr_roughness_cutoff_env(),
        "env": regia_render::SSSR_ROUGHNESS_CUTOFF_ENV,
        "range": [regia_render::SSSR_ROUGHNESS_CUTOFF_RANGE.0, regia_render::SSSR_ROUGHNESS_CUTOFF_RANGE.1],
        "fade_band": regia_render::SSSR_ROUGHNESS_BAND,
    })
}

/// `render.get_sssr_cutoff`: the stochastic SSR roughness cutoff in effect, its default, the
/// environment override (null when unset) and the clamp range.
pub(crate) fn get_sssr_cutoff() -> Result<Value, RpcError> {
    Ok(sssr_cutoff_json())
}

/// `render.set_sssr_cutoff { cutoff }`: sets the host value for every renderer from the next
/// frame, clamped (`get`'s `default`, 0.55, restores it). Not persisted. Returns
/// `{ before, after, stored }`; `after.cutoff` differs from `stored` while the environment override
/// is set.
pub(crate) fn set_sssr_cutoff(params: &Value) -> Result<Value, RpcError> {
    let value = params
        .get("cutoff")
        .and_then(Value::as_f64)
        .ok_or_else(|| RpcError::invalid_params("`cutoff` (number) is required"))? as f32;
    let before = sssr_cutoff_json();
    let stored = regia_render::set_sssr_roughness_cutoff(value);
    Ok(json!({ "before": before, "after": sssr_cutoff_json(), "stored": stored }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn packaged_profile_status_reports_origin() {
        const CHILD: &str = "REGIA_TEST_PROFILE_ORIGIN_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let mut app = regia_app::App::new();
            app.insert_resource(regia_framework::settings::SettingsPath(None));
            app.add_plugins((
                regia_app::MinimalPlugins,
                regia_framework::auto_quality::AutoQualityPlugin,
                regia_framework::settings::SettingsPlugin,
            ));
            let profile = get(app.world_mut()).expect("profile");
            assert_eq!(profile["preset"], "DesktopLow");
            assert_eq!(profile["configuration"]["source"], "packaged");
            assert_eq!(profile["configuration"]["packaged_profile"], "DesktopLow");
            assert!(
                profile["configuration"]["location"]
                    .as_str()
                    .expect("location")
                    .ends_with("build_info.json")
            );
            let quality = crate::quality::get_detected(app.world_mut()).expect("quality");
            assert_eq!(quality["configuration"], profile["configuration"]);
            app.world_mut()
                .resource_mut::<regia_framework::settings::GameSettings>()
                .graphics
                .choose_preset(regia_framework::settings::GraphicsPreset::High);
            app.update();
            let player = get(app.world_mut()).expect("player profile");
            assert_eq!(player["preset"], "DesktopHigh");
            assert_eq!(player["configuration"]["source"], "player");
            assert_eq!(player["configuration"]["packaged_profile"], "DesktopLow");
            let quality = crate::quality::get_detected(app.world_mut()).expect("player quality");
            assert_eq!(quality["configuration"], player["configuration"]);
            return;
        }
        let root = regia_test_utils::unique_temp_dir("agent-packaged-profile");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("build_info.json"), r#"{"render_profile":"DesktopLow"}"#).expect("metadata");
        let exe = root.join("game");
        std::fs::copy(std::env::current_exe().expect("exe"), &exe).expect("stage");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "render_profile::tests::packaged_profile_status_reports_origin",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("launch");
        std::fs::remove_dir_all(root).expect("cleanup");
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn sssr_cutoff_reads_sets_and_restores() -> Result<(), RpcError> {
        let env = regia_render::sssr_roughness_cutoff_env();
        let got = get_sssr_cutoff()?;
        assert_eq!(got["default"], json!(0.55f32));
        assert_eq!(got["range"][1], json!(1.0));
        assert!(set_sssr_cutoff(&json!({})).is_err());
        assert!(set_sssr_cutoff(&json!({ "cutoff": "high" })).is_err());
        let out = set_sssr_cutoff(&json!({ "cutoff": 0.7 }))?;
        assert_eq!(out["stored"], json!(0.7f32));
        assert_eq!(out["after"]["cutoff"], json!(env.unwrap_or(0.7)));
        let out = set_sssr_cutoff(&json!({ "cutoff": 9 }))?;
        assert_eq!(out["stored"], json!(1.0), "clamped");
        let out = set_sssr_cutoff(&json!({ "cutoff": got["default"] }))?;
        assert_eq!(out["stored"], json!(0.55f32));
        Ok(())
    }

    #[test]
    fn switches_presets_and_post() -> Result<(), RpcError> {
        let mut world = World::new();
        assert!(get(&mut world).is_err());
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        let got = get(&mut world)?;
        assert_eq!(got["preset"], "DesktopHigh");
        // ADR-3289: the runtime default sits beside the platform default; it is `Deck` only on a
        // Steam Deck, so it is either the platform default or `Deck`, and a real preset name.
        let runtime = got["runtime_default"].as_str().unwrap_or_default();
        assert_eq!(runtime, RenderProfilePreset::runtime_default().name());
        assert!(
            runtime == got["platform_default"].as_str().unwrap_or_default() || runtime == "Deck",
            "{runtime}"
        );
        // The six real-time presets and the offline Cinematic (ADR-1403).
        assert_eq!(got["presets"].as_array().map(Vec::len), Some(7));
        assert_eq!(got["offline_presets"], json!(["Cinematic"]));
        assert_eq!(got["profile"]["cloud_march"]["samples"], 1);
        assert_eq!(got["profile"]["cloud_march"]["max_steps"], 96);
        assert_eq!(got["profile"]["terrain_anisotropy"], 8);
        assert_eq!(got["thermal"]["status"], "none");
        assert_eq!(got["thermal"]["governor_level"], 0);
        assert_eq!(got["profile"]["cloud_reprojection"], true);
        assert_eq!(got["profile"]["cloud_step_percent"], 100);
        assert_eq!(got["profile"]["cloud_cone_light"], true);
        let ultra = set(&mut world, &json!({ "preset": "DesktopUltra" }))?;
        assert_eq!(ultra["after"]["profile"]["cloud_step_percent"], 150);
        assert_eq!(ultra["after"]["profile"]["cloud_cone_light"], true);

        let out = set(&mut world, &json!({ "preset": "mobile_low", "post": true }))?;
        assert_eq!(out["before"]["preset"], "DesktopUltra");
        assert_eq!(out["after"]["preset"], "MobileLow");
        assert_eq!(out["after"]["post_enabled"], true);
        let settings = world.resource::<RenderSettings>();
        assert_eq!(settings.preset(), Some(RenderProfilePreset::MobileLow));
        // Mobile Low runs a fixed exposure and no bloom; the seeded toggles follow the preset.
        assert!(!settings.post.auto_exposure && !settings.post.bloom);
        assert_eq!(out["after"]["profile"]["terrain_anisotropy"], 0);
        assert_eq!(out["after"]["profile"]["cloud_cone_light"], false);

        assert!(set(&mut world, &json!({ "preset": "phone" })).is_err());
        assert!(set(&mut world, &json!({ "preset": "MobileHigh", "post": 1 })).is_err());
        Ok(())
    }

    /// ADR-1403: the offline Cinematic preset through `render.set_profile`, its cloud overrides
    /// and its tier gate (a software rasteriser falls back to its own preset unless forced; an
    /// RTX 5090 runs it).
    #[test]
    fn cinematic_switches_overrides_and_gates_by_tier() -> Result<(), RpcError> {
        use regia_framework::auto_quality::AdapterOverride;
        use regia_render::{AdapterProfile, quality::DeviceClass};

        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopUltra));
        // Headless (no adapter): Cinematic runs everywhere but Android.
        let out = set(&mut world, &json!({ "preset": "cinematic" }))?;
        let android = cfg!(target_os = "android");
        if android {
            assert_eq!(out["after"]["preset"], "MobileHigh");
            assert_eq!(out["gate"]["reason"], "cinematic_needs_desktop");
            return Ok(());
        }
        assert_eq!(out["gate"]["honoured"], true);
        assert_eq!(out["gate"]["tier"], Value::Null);
        assert_eq!(out["after"]["preset"], "Cinematic");
        let after = &out["after"]["profile"];
        assert_eq!(after["cloud_reprojection"], false);
        assert_eq!(after["cloud_step_percent"], 300);
        assert_eq!(
            after["cloud_march"],
            json!({ "samples": 4, "max_steps": 256, "max_divisor": 1 })
        );
        assert_eq!(after["csm_resolution"], 8192);
        assert_eq!(after["max_visible_lights"], 512);
        assert_eq!(after["terrain_anisotropy"], 16);
        assert_eq!(after["render_scale"], json!(1.0));

        // Cloud overrides alone customise the live profile.
        let out = set(&mut world, &json!({ "cloud_samples": 8 }))?;
        assert_eq!(out["after"]["preset"], Value::Null);
        assert_eq!(out["after"]["profile"]["cloud_march"]["samples"], 8);
        assert!(out.get("gate").is_none(), "no preset, no gate");
        let out = set(
            &mut world,
            &json!({ "preset": "DesktopUltra", "cloud_reprojection": false, "cloud_step_percent": 200 }),
        )?;
        assert_eq!(out["after"]["profile"]["cloud_reprojection"], false);
        assert_eq!(out["after"]["profile"]["cloud_march"]["max_steps"], 256);
        assert!(set(&mut world, &json!({ "cloud_samples": 64 })).is_err());
        // The Cinematic cloud march on a real-time preset, from the agent alone.
        let out = set(
            &mut world,
            &json!({ "preset": "DesktopHigh", "cloud_samples": 4, "cloud_step_percent": 300,
                     "cloud_reprojection": false, "cloud_max_divisor": 1 }),
        )?;
        assert_eq!(
            out["after"]["profile"]["cloud_march"],
            json!({ "samples": 4, "max_steps": 256, "max_divisor": 1 })
        );

        // A software rasteriser is the Low tier: Cinematic falls back to Desktop Low ...
        let mut lavapipe = AdapterProfile::named("llvmpipe (LLVM 19.1.7, 256 bits)", 0x10005, Some(16_384));
        lavapipe.class = DeviceClass::Cpu;
        world.insert_resource(AdapterOverride(lavapipe));
        let out = set(&mut world, &json!({ "preset": "Cinematic" }))?;
        assert_eq!(out["gate"]["applied"], "DesktopLow");
        assert_eq!(out["gate"]["reason"], "cinematic_tier_too_low");
        assert_eq!(out["gate"]["tier"], "Low");
        assert_eq!(out["after"]["preset"], "DesktopLow");
        // ... unless forced.
        let out = set(&mut world, &json!({ "preset": "Cinematic", "force": true }))?;
        assert_eq!(out["gate"]["honoured"], true);
        assert_eq!(out["after"]["preset"], "Cinematic");

        // An RTX 5090 is Ultra: Cinematic runs unforced.
        world.insert_resource(AdapterOverride(AdapterProfile::named(
            "NVIDIA GeForce RTX 5090",
            0x10de,
            Some(32_607),
        )));
        let out = set(&mut world, &json!({ "preset": "Cinematic" }))?;
        assert_eq!(out["gate"]["tier"], "Ultra");
        assert_eq!(out["gate"]["honoured"], true);
        assert!(set(&mut world, &json!({ "preset": "Cinematic", "force": "yes" })).is_err());
        Ok(())
    }

    #[test]
    fn mesh_lod_scales_read_set_and_clamp() -> Result<(), RpcError> {
        let mut world = World::new();
        assert!(mesh_lod(&mut world, &json!({})).is_err());
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopUltra));
        let got = get(&mut world)?;
        assert_eq!(got["profile"]["mesh_lod_distance_scale"], json!(6.0));
        assert_eq!(got["profile"]["mesh_lod_shadow_scale"], json!(0.15f32));

        // Scales alone keep the preset's other fields; the profile reads as customised.
        let out = set(&mut world, &json!({ "mesh_lod_distance_scale": 3.0 }))?;
        assert_eq!(out["before"]["preset"], "DesktopUltra");
        assert_eq!(out["after"]["preset"], Value::Null);
        assert_eq!(out["after"]["profile"]["mesh_lod_distance_scale"], json!(3.0));
        assert_eq!(out["after"]["profile"]["cloud_step_percent"], 150);
        let out = set(
            &mut world,
            &json!({ "mesh_lod_distance_scale": 100.0, "mesh_lod_shadow_scale": 0.001 }),
        )?;
        assert_eq!(
            out["after"]["profile"]["mesh_lod_distance_scale"],
            json!(16.0),
            "clamped"
        );
        assert_eq!(
            out["after"]["profile"]["mesh_lod_shadow_scale"],
            json!(0.05f32),
            "clamped"
        );
        assert!(set(&mut world, &json!({ "mesh_lod_distance_scale": -1.0 })).is_err());
        assert!(set(&mut world, &json!({ "mesh_lod_shadow_scale": "low" })).is_err());
        assert!(
            set(&mut world, &json!({})).is_err(),
            "neither a preset nor a scale"
        );
        // A preset re-seeds the scales; a scale in the same call wins.
        let out = set(
            &mut world,
            &json!({ "preset": "DesktopLow", "mesh_lod_shadow_scale": 0.25 }),
        )?;
        assert_eq!(out["after"]["profile"]["mesh_lod_distance_scale"], json!(1.0));
        assert_eq!(out["after"]["profile"]["mesh_lod_shadow_scale"], json!(0.25));

        set(&mut world, &json!({ "preset": "DesktopUltra" }))?;
        let lod = mesh_lod(
            &mut world,
            &json!({ "radius": 5.0, "thresholds": [1.0, 0.5, 0.0], "height": 1080, "fov_y": 1.0 }),
        )?;
        assert_eq!(lod["preset"], "DesktopUltra");
        assert_eq!(lod["presets"].as_array().map(Vec::len), Some(6));
        let colour = lod["switch"]["color_m"].as_array().expect("colour row").clone();
        assert_eq!(colour.len(), 2, "LOD0→1 and LOD1→2");
        let low = lod["presets"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["preset"] == "DesktopLow"))
            .expect("DesktopLow row");
        let ultra0 = colour[0].as_f64().expect("ultra");
        let low0 = low["switch"]["color_m"][0].as_f64().expect("low");
        // Ultra: bias 1.15 × 6; Low: 0.85 × 1. Distances scale linearly.
        assert!(
            (ultra0 / low0 - (1.15 * 6.0) / 0.85).abs() < 1.0e-3,
            "{ultra0} vs {low0}"
        );
        let shadow0 = lod["switch"]["shadow_m"][0].as_f64().expect("shadow");
        assert!((shadow0 / ultra0 - 0.15).abs() < 1.0e-4);
        // Hostile parameters are refused, not clamped into nonsense.
        for bad in [
            json!({ "radius": -1.0 }),
            json!({ "thresholds": [] }),
            json!({ "thresholds": [1.0, -0.5] }),
            json!({ "thresholds": vec![1.0; 17] }),
            json!({ "height": 0 }),
            json!({ "fov_y": 4.0 }),
        ] {
            assert!(mesh_lod(&mut world, &bad).is_err(), "{bad}");
        }
        Ok(())
    }
}
