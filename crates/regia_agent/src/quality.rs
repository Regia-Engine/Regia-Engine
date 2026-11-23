// SPDX-License-Identifier: MPL-2.0
//! `quality.get_detected` / `quality.detect`: first-launch quality detection (guide topic
//! `quality`, book `agents/auto-quality.md`). `quality.get_terrain_vt_policy` /
//! `quality.set_terrain_vt_policy`: where the detector may turn the terrain virtual texture on
//! (ADR-2122, book `agents/terrain-vt-tiers.md`).

use regia_ecs::prelude::*;
use regia_framework::auto_quality::{AutoQuality, current_adapter};
use regia_framework::settings::{GameSettings, PresetSource};
use regia_render::quality::{AdapterProfile, GpuTierTable, QualityTier, TerrainVtPolicy, detect};
use regia_render::{QualityProbe, RenderSettings};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// A host's own tier detection, answered as `host` by `quality.get_detected` (ADR-5131): apps without
/// `GameSettings` (the Regia Forest Demo picks its tier itself) keep their detection here, so an agent
/// that reads the generic quality method gets it. `None` (the default) answers `host: null`.
#[derive(Resource, Clone, Debug, Default, PartialEq)]
pub struct HostQualityDetection(pub Option<Value>);

fn source_name(source: PresetSource) -> &'static str {
    match source {
        PresetSource::Unset => "unset",
        PresetSource::Detected => "detected",
        PresetSource::Player => "player",
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// `quality.get_detected`: the saved preset and who set it, the saved detection (tier, reason,
/// GPU, probe), what ran in this launch, the render tier in effect, the live adapter and the
/// host's own detection ([`HostQualityDetection`]).
pub(crate) fn get_detected(world: &mut World) -> Result<Value, RpcError> {
    let settings = world.get_resource::<GameSettings>();
    let setting = settings.map(|s| {
        json!({
            "preset": format!("{:?}", s.graphics.preset),
            "source": source_name(s.graphics.source()),
        })
    });
    let detected = settings.and_then(|s| s.graphics.detected.as_ref()).map(|d| {
        json!({
            "tier": d.tier,
            "detected_tier": d.detected_tier,
            "reason": d.reason,
            "rule": d.rule,
            "evidence": d.evidence,
            "gpu": d.gpu,
            "pci_id": [format!("{:#06x}", d.pci_id[0]), format!("{:#06x}", d.pci_id[1])],
            "vram_mb": d.vram_mb,
            "driver": d.driver,
            "probe": d.probe.as_ref().map(|p| json!({
                "p50_ms": p.p50_ms,
                "p95_ms": p.p95_ms,
                "budget_ms": p.budget_ms,
                "frames": p.frames,
                "clock": p.clock,
                "size": p.size,
                "stepped_down": p.stepped_down,
                "verdict": p.verdict,
                "monitor_size": p.monitor_size,
                "window_mode": p.window_mode,
                "size_mismatch": p.size_mismatch,
            })),
        })
    });
    let auto = world.get_resource::<AutoQuality>();
    let this_launch = auto.map(|a| {
        json!({
            "detections": a.detections_run(),
            "probing": a.is_probing(),
            "redetect_pending": a.redetect_pending(),
            "reprobe_pending": a.reprobe_pending(),
            "reason": a.last_detection().map(|d| d.reason.clone()),
            "verdict": a.last_verdict().map(|v| v.reason.clone()),
            "enabled": a.enabled,
            "confirm": a.confirm,
            "target_fps": a.target_fps,
            "probe_seconds": a.probe_seconds,
        })
    });
    let render_tier = world
        .get_resource::<RenderSettings>()
        .and_then(QualityTier::of_settings)
        .map(QualityTier::name);
    let probe_active = world
        .get_resource::<QualityProbe>()
        .is_some_and(QualityProbe::is_active);
    let step_down = settings
        .and_then(|s| s.graphics.detected.as_ref())
        .and_then(step_down_report);
    Ok(json!({
        "setting": setting,
        "configuration": crate::render_profile::configuration(world),
        "detected": detected,
        "step_down": step_down,
        "this_launch": this_launch,
        "render_tier": render_tier,
        "probe_active": probe_active,
        "adapter": current_adapter(world).as_ref().map(to_json),
        "host": world.get_resource::<HostQualityDetection>().and_then(|h| h.0.clone()),
    }))
}

/// `quality.get_detected`'s `step_down` (ADR-7882): why the saved tier is below the table's pick or
/// below Low, `None` when it is neither. `by` is `probe` (the confirmation probe stepped down; `why`
/// is its verdict, with the budget it missed) or `table` (a GPU table row picked
/// [`QualityTier::VeryLow`]; `why` is the detection's reason). `below_low` is whether the tier is
/// Very Low.
fn step_down_report(detected: &regia_framework::settings::DetectedQuality) -> Option<Value> {
    let below_low = QualityTier::from_name(&detected.tier) == Some(QualityTier::VeryLow);
    match detected.probe.as_ref() {
        Some(probe) if probe.stepped_down => Some(json!({
            "from": detected.detected_tier,
            "to": detected.tier,
            "by": "probe",
            "why": probe.verdict,
            "budget_ms": probe.budget_ms,
            "below_low": below_low,
        })),
        _ if below_low => Some(json!({
            "from": detected.detected_tier,
            "to": detected.tier,
            "by": "table",
            "why": detected.reason,
            "budget_ms": Value::Null,
            "below_low": true,
        })),
        _ => None,
    }
}

/// Parses an `adapter` param into a profile (`class` defaults to `Discrete`).
fn adapter_param(value: &Value) -> Result<AdapterProfile, RpcError> {
    let mut value = value.clone();
    let Some(map) = value.as_object_mut() else {
        return Err(RpcError::invalid_params("`adapter` must be an object"));
    };
    if !map.contains_key("name") {
        return Err(RpcError::invalid_params("`adapter.name` (string) is required"));
    }
    let class = match map.get("class") {
        None | Some(Value::Null) => "discrete".to_string(),
        Some(Value::String(class)) => class.to_lowercase(),
        Some(_) => return Err(RpcError::invalid_params("`adapter.class` must be a string")),
    };
    let class = match class.as_str() {
        "discrete" => "Discrete",
        "integrated" => "Integrated",
        "virtual" => "Virtual",
        "cpu" => "Cpu",
        "other" => "Other",
        _ => {
            return Err(RpcError::invalid_params(
                "`adapter.class` is one of discrete, integrated, virtual, cpu, other",
            ));
        }
    };
    map.insert("class".to_string(), json!(class));
    serde_json::from_value(value).map_err(|e| RpcError::invalid_params(format!("`adapter`: {e}")))
}

/// `quality.detect { adapter?, apply? }`: what the table picks for the running GPU (or for
/// `adapter`, a dry run that changes nothing). With `apply: true` (the default without `adapter`)
/// it re-detects on the next frame like the options menu's "Auto-detect": `graphics.preset` is
/// replaced, saved, and the confirmation probe runs when enabled.
pub(crate) fn detect_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dry_adapter = match params.get("adapter") {
        None | Some(Value::Null) => None,
        Some(v) => Some(adapter_param(v)?),
    };
    let apply = match params.get("apply") {
        None | Some(Value::Null) => dry_adapter.is_none(),
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(RpcError::invalid_params("`apply` must be a boolean")),
    };
    if apply && dry_adapter.is_some() {
        return Err(RpcError::invalid_params(
            "`apply` needs the running GPU: drop `adapter`, or pass `apply: false` for a dry run",
        ));
    }
    let adapter = match dry_adapter {
        Some(a) => a,
        None => current_adapter(world).ok_or_else(|| {
            RpcError::not_found(
                "no render device yet",
                "detection reads the GPU; wait for the window, or pass `adapter` for a dry run",
            )
        })?,
    };
    let detection = match world.get_resource::<AutoQuality>() {
        Some(auto) => auto.detect(&adapter),
        None => detect(&adapter, &GpuTierTable::builtin()),
    };
    let mut out = json!({
        "tier": detection.tier.name(),
        "reason": detection.reason,
        "rule": detection.rule,
        "evidence": detection.evidence,
        "note": detection.note,
        "caps": detection.caps,
        "adapter": to_json(&adapter),
        "applied": false,
    });
    if apply {
        let replaces = world.get_resource::<GameSettings>().map(|s| {
            json!({
                "preset": format!("{:?}", s.graphics.preset),
                "source": source_name(s.graphics.source()),
            })
        });
        let Some(replaces) = replaces else {
            return Err(RpcError::not_found(
                "no GameSettings resource in this app",
                "add regia_framework::settings::SettingsPlugin; detection persists there",
            ));
        };
        let mut auto = world.get_resource_mut::<AutoQuality>().ok_or_else(|| {
            RpcError::not_found(
                "no AutoQuality resource in this app",
                "add regia_framework::auto_quality::AutoQualityPlugin (in regia::DefaultPlugins)",
            )
        })?;
        auto.enabled = true;
        auto.request_redetect();
        out["applied"] = json!("next_frame");
        out["replaces"] = replaces;
        out["probe"] = json!(auto.confirm);
    }
    Ok(out)
}

fn terrain_vt_policy(world: &World) -> TerrainVtPolicy {
    world
        .get_resource::<TerrainVtPolicy>()
        .copied()
        .unwrap_or_default()
}

/// The policy, its fixed thresholds, the running tier and lever, and what the policy decides for
/// the running GPU (`null` before the render device exists).
fn terrain_vt_policy_report(world: &World) -> Value {
    let policy = terrain_vt_policy(world);
    let render = world.get_resource::<RenderSettings>();
    let tier = render.and_then(QualityTier::of_settings);
    let adapter = current_adapter(world);
    let decides = match (tier, adapter.as_ref()) {
        (Some(tier), Some(adapter)) => {
            Some(tier.terrain_vt_default_with(adapter, std::env::consts::OS, &policy))
        }
        _ => None,
    };
    json!({
        "policy": to_json(&policy),
        "excluded_vendors": QualityTier::TERRAIN_VT_EXCLUDED_VENDORS
            .iter()
            .map(|v| format!("{v:#06x}"))
            .collect::<Vec<_>>(),
        "min_vram_mb": {
            "low": QualityTier::TERRAIN_VT_MIN_VRAM_MB,
            "high": QualityTier::TERRAIN_VT_HIGH_MIN_VRAM_MB,
        },
        "high_budget_bytes": QualityTier::TERRAIN_VT_HIGH_BUDGET_BYTES,
        "os": std::env::consts::OS,
        "render_tier": tier.map(QualityTier::name),
        "lever": render.map(|r| r.profile.terrain_budget.virtual_texture),
        "late_depth": render.map(|r| r.profile.terrain_budget.vt_late_depth),
        "decides": decides,
    })
}

/// `quality.get_terrain_vt_policy`: where the quality detector may turn the terrain virtual
/// texture on (ADR-2122; every opt-in off = Desktop Low, Linux, not AMD), the thresholds, the
/// running tier, the lever and what the policy decides for the running GPU.
pub(crate) fn get_terrain_vt_policy(world: &mut World) -> Result<Value, RpcError> {
    Ok(terrain_vt_policy_report(world))
}

/// `quality.set_terrain_vt_policy { high?, windows?, unmeasured_vendors? }`: changes the given
/// opt-ins (booleans; the others keep their value), stores the policy as a resource for every
/// later preset change, and re-decides the lever on the running tier now when the render device
/// exists (`lever` in the result; `null` = decided when the device appears). Not persisted.
pub(crate) fn set_terrain_vt_policy(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(fields) = params.as_object().filter(|m| !m.is_empty()) else {
        return Err(RpcError::invalid_params(
            "pass `high`, `windows` and/or `unmeasured_vendors` (booleans)",
        ));
    };
    let mut policy = terrain_vt_policy(world);
    for (key, value) in fields {
        let on = value
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a boolean")))?;
        match key.as_str() {
            "high" => policy.high = on,
            "windows" => policy.windows = on,
            "unmeasured_vendors" => policy.unmeasured_vendors = on,
            other => {
                return Err(RpcError::invalid_params(format!(
                    "unknown field `{other}` (known: high, windows, unmeasured_vendors)"
                )));
            }
        }
    }
    let before = terrain_vt_policy_report(world);
    let old_policy = terrain_vt_policy(world);
    world.insert_resource(policy);
    let tier = world
        .get_resource::<RenderSettings>()
        .and_then(QualityTier::of_settings);
    let lever = match (tier, current_adapter(world)) {
        (Some(tier), Some(adapter)) => {
            let os = std::env::consts::OS;
            let on = tier.terrain_vt_default_with(&adapter, os, &policy);
            let was = tier.terrain_vt_default_with(&adapter, os, &old_policy);
            world.get_resource_mut::<RenderSettings>().map(|mut render| {
                let budget = &mut render.profile.terrain_budget;
                // Only a lever the old policy decided follows the new one: a host that set it
                // against the detector (on or off) keeps its choice.
                if budget.virtual_texture == was {
                    policy.set_lever(budget, on);
                }
                budget.virtual_texture
            })
        }
        _ => None,
    };
    Ok(json!({ "before": before, "policy": to_json(&policy), "lever": lever }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::prelude::*;
    use regia_framework::auto_quality::{AdapterOverride, AutoQualityPlugin};
    use regia_framework::settings::{SettingsPath, SettingsPlugin};

    #[test]
    fn dry_run_and_redetect() -> Result<(), RpcError> {
        let mut app = App::new();
        app.insert_resource(SettingsPath(None))
            .insert_resource(AdapterOverride(AdapterProfile::named(
                "NVIDIA GeForce RTX 3080 Ti",
                0x10de,
                Some(12_288),
            )))
            .insert_resource(AutoQuality::default().with_confirm(false))
            .add_plugins((MinimalPlugins, SettingsPlugin, AutoQualityPlugin));
        app.update();
        let world = app.world_mut();
        let got = get_detected(world)?;
        assert_eq!(got["setting"]["preset"], "High");
        assert_eq!(got["setting"]["source"], "detected");
        assert_eq!(got["render_tier"], "High");
        let reason = got["detected"]["reason"].as_str().unwrap_or_default();
        assert!(
            reason.starts_with("NVIDIA GeForce RTX 3080 Ti, 12 GB → High"),
            "{reason}"
        );

        // Dry run for another GPU: nothing changes.
        let dry = detect_method(
            world,
            &json!({"adapter": {"name": "NVIDIA GeForce GTX 1080", "vendor": 4318, "vram_mb": 8192}}),
        )?;
        assert_eq!(dry["tier"], "Low");
        assert_eq!(dry["applied"], false);
        assert_eq!(dry["evidence"], "measured");
        let integrated = detect_method(
            world,
            &json!({"adapter": {"name": "AMD Radeon Graphics", "class": "integrated"}}),
        )?;
        assert_eq!(integrated["tier"], "Low");
        assert!(detect_method(world, &json!({"adapter": {"vram_mb": 1}})).is_err());
        assert!(detect_method(world, &json!({"adapter": {"name": "x"}, "apply": true})).is_err());

        // The player picks Ultra; quality.detect re-detects and replaces it on the next frame.
        world.resource_mut::<GameSettings>().graphics.preset =
            regia_framework::settings::GraphicsPreset::Ultra;
        app.update();
        let out = detect_method(app.world_mut(), &json!({}))?;
        assert_eq!(out["applied"], "next_frame");
        assert_eq!(out["replaces"]["source"], "player");
        app.update();
        let got = get_detected(app.world_mut())?;
        assert_eq!(got["setting"]["preset"], "High");
        assert_eq!(got["this_launch"]["detections"], 2);
        Ok(())
    }

    #[test]
    fn adapter_params_are_validated_and_dry_runs_are_repeatable() {
        let mut world = World::new();
        let ok = |adapter: Value| detect_method(&mut World::new(), &json!({ "adapter": adapter }));
        for bad in [
            json!("gpu"),
            json!([]),
            json!({}),
            json!({ "name": "x", "class": 5 }),
            json!({ "name": "x", "class": "quantum" }),
            json!({ "name": "x", "vram_mb": "lots" }),
            json!({ "name": 7 }),
        ] {
            assert!(ok(bad.clone()).is_err(), "{bad}");
        }
        assert!(detect_method(&mut world, &json!({ "apply": "yes" })).is_err());
        assert!(detect_method(&mut world, &json!({ "adapter": { "name": "x" }, "apply": null })).is_ok());
        // Case-insensitive class, null class is discrete.
        let a = ok(json!({ "name": "Some GPU", "class": "INTEGRATED" })).expect("upper");
        let b = ok(json!({ "name": "Some GPU", "class": "integrated" })).expect("lower");
        assert_eq!(a, b);
        let c = ok(json!({ "name": "Some GPU", "class": null })).expect("null");
        let d = ok(json!({ "name": "Some GPU" })).expect("absent");
        assert_eq!(c, d);
        // No device and no adapter: a refusal, not a panic; same with apply and no settings.
        assert_eq!(
            detect_method(&mut World::new(), &json!({}))
                .expect_err("no device")
                .code,
            crate::protocol::NOT_FOUND
        );
        // get_detected on a bare world reports nothing instead of failing.
        let bare = get_detected(&mut World::new()).expect("bare");
        assert!(bare["setting"].is_null() && bare["detected"].is_null() && bare["this_launch"].is_null());
        assert_eq!(bare["probe_active"], json!(false));
    }

    /// ADR-2122: the policy reads as all-off by default; setting `high` re-decides the lever on
    /// a running High tier at once (Linux), and the resource persists for later preset changes;
    /// bad params are refused with the documented messages.
    #[test]
    fn terrain_vt_policy_get_and_set() -> Result<(), RpcError> {
        let mut app = App::new();
        app.insert_resource(SettingsPath(None))
            .insert_resource(AdapterOverride(AdapterProfile::named(
                "NVIDIA GeForce RTX 3080 Ti",
                0x10de,
                Some(12_288),
            )))
            .add_plugins((MinimalPlugins, SettingsPlugin, AutoQualityPlugin));
        app.update();
        app.update();
        let world = app.world_mut();
        let got = get_terrain_vt_policy(world)?;
        assert_eq!(
            got["policy"],
            json!({"high": false, "windows": false, "unmeasured_vendors": false})
        );
        assert_eq!(got["excluded_vendors"], json!(["0x1002"]));
        assert_eq!(got["render_tier"], json!("High"));
        assert_eq!(got["lever"], json!(false));
        assert_eq!(got["decides"], json!(false));

        let set = set_terrain_vt_policy(world, &json!({"high": true}))?;
        let linux = cfg!(target_os = "linux");
        assert_eq!(set["lever"], json!(linux));
        assert_eq!(set["before"]["policy"]["high"], json!(false));
        assert_eq!(
            world.get_resource::<TerrainVtPolicy>().copied(),
            Some(TerrainVtPolicy {
                high: true,
                ..TerrainVtPolicy::default()
            })
        );
        let got = get_terrain_vt_policy(world)?;
        assert_eq!(got["lever"], json!(linux));
        assert_eq!(got["render_tier"], json!("High"), "the lever is masked");
        assert_eq!(
            got["late_depth"],
            json!(false),
            "only `windows` admits the late path"
        );

        // The Windows opt-in carries the late-depth path with the lever it decides.
        set_terrain_vt_policy(world, &json!({"windows": true}))?;
        let got = get_terrain_vt_policy(world)?;
        let admitted = cfg!(any(target_os = "linux", target_os = "windows"));
        assert_eq!(got["lever"], json!(admitted));
        assert_eq!(got["late_depth"], got["lever"]);
        assert_eq!(got["render_tier"], json!("High"), "late_depth is masked");

        // A host that turned the lever off against the detector keeps its choice.
        if let Some(mut render) = world.get_resource_mut::<RenderSettings>() {
            render.profile.terrain_budget.virtual_texture = false;
            render.profile.terrain_budget.vt_late_depth = false;
        }
        let set = set_terrain_vt_policy(world, &json!({"windows": false}))?;
        assert_eq!(set["lever"], json!(false));
        assert_eq!(get_terrain_vt_policy(world)?["decides"], json!(linux));

        for bad in [
            json!(null),
            json!({}),
            json!({"high": "yes"}),
            json!({"ultra": true}),
        ] {
            assert!(set_terrain_vt_policy(world, &bad).is_err(), "{bad}");
        }
        // A refused call changes nothing.
        assert!(
            world
                .get_resource::<TerrainVtPolicy>()
                .is_some_and(|p| p.high && !p.windows)
        );
        Ok(())
    }

    /// ADR-5131 (demo defect 5): an app without `GameSettings` answered `detected: null` and
    /// nothing else of its pick; its own detection is answered as `host`.
    #[test]
    fn a_hosts_own_detection_is_answered_as_host() -> Result<(), RpcError> {
        let mut world = World::new();
        assert_eq!(get_detected(&mut world)?["host"], Value::Null);
        let picked =
            json!({ "tier": "Low", "source": "detected", "detection": { "rule": "intel_integrated" } });
        world.insert_resource(HostQualityDetection(Some(picked.clone())));
        let got = get_detected(&mut world)?;
        assert_eq!(got["host"], picked);
        assert_eq!(got["detected"], Value::Null, "no GameSettings record");
        Ok(())
    }

    /// ADR-7882: `step_down` says why the saved tier is below the table's pick (the probe's verdict
    /// and budget) or below Low (the table row), and is `null` otherwise.
    #[test]
    fn step_down_reports_the_probe_or_the_table_and_why() {
        use regia_framework::settings::{DetectedQuality, ProbeRecord};
        let low = DetectedQuality {
            tier: "Low".into(),
            detected_tier: "Low".into(),
            reason: "Intel(R) Iris(R) Xe Graphics, integrated → Low (intel_integrated, by_class)".into(),
            ..DetectedQuality::default()
        };
        assert_eq!(step_down_report(&low), None, "kept tier: nothing to explain");
        let verdict = "p95 75.0 ms > 33.3 ms budget: Low misses 30 fps, Low → VeryLow";
        let probed = DetectedQuality {
            tier: "VeryLow".into(),
            probe: Some(ProbeRecord {
                p50_ms: 68.0,
                p95_ms: 75.0,
                budget_ms: 33.3,
                stepped_down: true,
                verdict: verdict.into(),
                ..ProbeRecord::default()
            }),
            ..low
        };
        let got = step_down_report(&probed);
        assert_eq!(
            got,
            Some(json!({
                "from": "Low", "to": "VeryLow", "by": "probe", "why": verdict,
                "budget_ms": 33.3_f32, "below_low": true,
            }))
        );
        let table = DetectedQuality {
            tier: "VeryLow".into(),
            detected_tier: "VeryLow".into(),
            reason: "Intel(R) Graphics, integrated → VeryLow (intel_n_series, measured)".into(),
            ..DetectedQuality::default()
        };
        let got = step_down_report(&table).unwrap_or(Value::Null);
        assert_eq!(got["by"], "table");
        assert_eq!(got["below_low"], true);
        assert_eq!(got["why"], table.reason.as_str());
        assert_eq!(got["budget_ms"], Value::Null);
        // A probe step that stays above Low is reported too.
        let high = DetectedQuality {
            tier: "Medium".into(),
            detected_tier: "High".into(),
            probe: Some(ProbeRecord {
                stepped_down: true,
                verdict: "p95 21.9 ms > 16.7 ms budget: High → Medium".into(),
                ..ProbeRecord::default()
            }),
            ..DetectedQuality::default()
        };
        let got = step_down_report(&high).unwrap_or(Value::Null);
        assert_eq!(
            (got["from"].as_str(), got["to"].as_str()),
            (Some("High"), Some("Medium"))
        );
        assert_eq!(got["below_low"], false);
        // get_detected carries it: null without GameSettings.
        let mut world = World::new();
        assert_eq!(
            get_detected(&mut world).map(|v| v["step_down"].clone()).ok(),
            Some(Value::Null)
        );
    }
}
