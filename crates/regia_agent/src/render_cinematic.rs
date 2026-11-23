// SPDX-License-Identifier: MPL-2.0
//! The Cinematic offline profile's part of `render.set_profile` / `render.get_profile`
//! (ADR-1403, book `agents/cinematic.md`): the cloud march overrides, the tier gate and the JSON
//! they report.

use regia_ecs::prelude::*;
use regia_render::cinematic::{PresetGate, gate_preset};
use regia_render::{CloudMarch, QualityTier, RenderProfile, RenderProfilePreset};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Highest `cloud_step_percent` `render.set_profile` accepts (64 × 400 % = 256 steps, the
/// march's ceiling).
pub(crate) const MAX_CLOUD_STEP_PERCENT: u64 = 400;

/// The cloud overrides and the gate switch of one `render.set_profile` call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CloudOverrides {
    /// `cloud_samples` (1..=16).
    pub samples: Option<u32>,
    /// `cloud_step_percent` (1..=400).
    pub step_percent: Option<u32>,
    /// `cloud_reprojection`.
    pub reprojection: Option<bool>,
    /// `cloud_max_divisor` (1..=8; 1 = the march at full resolution).
    pub max_divisor: Option<u32>,
    /// `force`: run Cinematic on a tier below High (never on Android).
    pub force: bool,
}

impl CloudOverrides {
    /// Whether any profile field is overridden (`force` alone changes nothing).
    pub(crate) fn any(&self) -> bool {
        self.samples.is_some()
            || self.step_percent.is_some()
            || self.reprojection.is_some()
            || self.max_divisor.is_some()
    }

    /// Writes the overrides into `profile`; the step percent's march ceiling rises with it, so a
    /// real-time preset asked for more than 96 steps gets them.
    pub(crate) fn apply(&self, profile: &mut RenderProfile) {
        if let Some(samples) = self.samples {
            profile.cloud_march.samples = samples;
        }
        if let Some(percent) = self.step_percent {
            profile.cloud_step_percent = percent;
            profile.cloud_march.max_steps = CloudMarch::STEP_LIMIT;
        }
        if let Some(on) = self.reprojection {
            profile.cloud_reprojection = on;
        }
        if let Some(divisor) = self.max_divisor {
            profile.cloud_march.max_divisor = divisor;
        }
    }
}

fn opt_int(params: &Value, key: &str, lo: u64, hi: u64) -> Result<Option<u32>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(v) if (lo..=hi).contains(&v) => Ok(Some(v as u32)),
            _ => Err(RpcError::invalid_params(format!(
                "`{key}` must be an integer in {lo}..={hi}"
            ))),
        },
    }
}

fn opt_bool(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a boolean"))),
    }
}

/// Reads `cloud_samples`, `cloud_step_percent`, `cloud_reprojection`, `cloud_max_divisor` and
/// `force`.
pub(crate) fn overrides(params: &Value) -> Result<CloudOverrides, RpcError> {
    Ok(CloudOverrides {
        samples: opt_int(params, "cloud_samples", 1, u64::from(CloudMarch::MAX_SAMPLES))?,
        step_percent: opt_int(params, "cloud_step_percent", 1, MAX_CLOUD_STEP_PERCENT)?,
        reprojection: opt_bool(params, "cloud_reprojection")?,
        max_divisor: opt_int(
            params,
            "cloud_max_divisor",
            1,
            u64::from(CloudMarch::DIVISOR_LIMIT),
        )?,
        force: opt_bool(params, "force")?.unwrap_or(false),
    })
}

/// The tier the quality detector gives the running GPU: the app's `AutoQuality` table when it
/// has one, the shipped table otherwise; `None` without an adapter (a headless test world).
pub(crate) fn detected_tier(world: &World) -> Option<QualityTier> {
    let adapter = regia_framework::auto_quality::current_adapter(world)?;
    let detection = match world.get_resource::<regia_framework::auto_quality::AutoQuality>() {
        Some(auto) => auto.detect(&adapter),
        None => regia_render::quality::detect(&adapter, &regia_render::GpuTierTable::builtin()),
    };
    Some(detection.tier)
}

/// Gates `preset` for this app's platform and GPU ([`gate_preset`]).
pub(crate) fn gate(
    world: &World,
    preset: RenderProfilePreset,
    force: bool,
) -> (PresetGate, Option<QualityTier>) {
    let tier = if preset == RenderProfilePreset::Cinematic {
        detected_tier(world)
    } else {
        None
    };
    (
        gate_preset(preset, tier, cfg!(target_os = "android"), force),
        tier,
    )
}

/// `{ requested, applied, honoured, reason, tier }` of a gated preset.
pub(crate) fn gate_json(gate: &PresetGate, tier: Option<QualityTier>) -> Value {
    json!({
        "requested": gate.requested.name(),
        "applied": gate.applied.name(),
        "honoured": gate.honoured(),
        "reason": gate.reason,
        "tier": tier.map(QualityTier::name),
    })
}

/// The cloud march fields of a profile for `render.get_profile`.
pub(crate) fn march_json(profile: &RenderProfile) -> Value {
    json!({
        "samples": profile.cloud_march.sample_count(),
        "max_steps": profile.cloud_march.max_steps.clamp(1, CloudMarch::STEP_LIMIT),
        "max_divisor": profile.cloud_march.max_divisor.clamp(1, CloudMarch::DIVISOR_LIMIT),
    })
}

/// The preset names that are offline only (`Cinematic`).
pub(crate) fn offline_presets() -> Vec<&'static str> {
    RenderProfilePreset::WITH_OFFLINE
        .iter()
        .filter(|p| p.is_offline())
        .map(|p| p.name())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_parse_and_refuse_out_of_range() -> Result<(), RpcError> {
        let none = overrides(&json!({}))?;
        assert_eq!(none, CloudOverrides::default());
        assert!(!none.any());
        let all = overrides(&json!({
            "cloud_samples": 8, "cloud_step_percent": 400, "cloud_reprojection": false, "cloud_max_divisor": 1,
            "force": true
        }))?;
        assert_eq!(all.max_divisor, Some(1));
        assert_eq!(all.samples, Some(8));
        assert_eq!(all.step_percent, Some(400));
        assert_eq!(all.reprojection, Some(false));
        assert!(all.force && all.any());
        for bad in [
            json!({ "cloud_samples": 0 }),
            json!({ "cloud_samples": 17 }),
            json!({ "cloud_samples": 2.5 }),
            json!({ "cloud_step_percent": 0 }),
            json!({ "cloud_step_percent": 401 }),
            json!({ "cloud_reprojection": "off" }),
            json!({ "cloud_max_divisor": 0 }),
            json!({ "cloud_max_divisor": 9 }),
            json!({ "force": 1 }),
        ] {
            assert!(overrides(&bad).is_err(), "{bad}");
        }
        // `force` alone overrides no profile field.
        assert!(!overrides(&json!({ "force": true }))?.any());
        Ok(())
    }

    #[test]
    fn a_step_override_lifts_the_realtime_ceiling() {
        let mut ultra = RenderProfile::from_preset(RenderProfilePreset::DesktopUltra);
        CloudOverrides {
            step_percent: Some(300),
            samples: Some(2),
            max_divisor: Some(1),
            ..CloudOverrides::default()
        }
        .apply(&mut ultra);
        assert_eq!(ultra.cloud_march.divisor(2), 1, "full resolution");
        assert_eq!(ultra.cloud_step_percent, 300);
        assert_eq!(ultra.cloud_march.max_steps, CloudMarch::STEP_LIMIT);
        assert_eq!(ultra.cloud_march.steps(64, ultra.cloud_step_percent), 192);
        assert_eq!(ultra.cloud_march.samples, 2);
        assert_eq!(march_json(&ultra)["samples"], 2);
    }

    #[test]
    fn a_headless_world_runs_cinematic_and_reports_no_tier() {
        let world = World::new();
        let (gated, tier) = gate(&world, RenderProfilePreset::Cinematic, false);
        assert_eq!(tier, None);
        if cfg!(target_os = "android") {
            assert_eq!(gated.applied, RenderProfilePreset::MobileHigh);
        } else {
            assert!(gated.honoured());
        }
        let out = gate_json(&gated, tier);
        assert_eq!(out["requested"], "Cinematic");
        assert_eq!(offline_presets(), vec!["Cinematic"]);
    }
}
