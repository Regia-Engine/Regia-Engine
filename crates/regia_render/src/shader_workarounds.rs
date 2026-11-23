// SPDX-License-Identifier: MPL-2.0
//! Driver workarounds that change how engine shaders are composed (ADR-1749).
//!
//! Decided once per process from the adapter the render device picked (`note_adapter`), so
//! every module composed afterwards on that device agrees. Today there is one, on Qualcomm Adreno:
//! [`SUN_SHADOW_PCF_ONLY_DEF`], with [`STUDIO_IBL_OFF_DEF`] (ADR-2042) and [`FORWARD_TRIM_DEF`]
//! (ADR-3145) beside it. The Adreno 643 driver of a Fairphone 5
//! (Vulkan 1.1, driver 512.530, compiler EV031.35.01.12) crashed inside `vkCreateGraphicsPipelines`
//! on the forward shader's opaque pipeline at every launch (a null dereference in
//! `libllvm-glnext.so`), and with only part of the trigger removed it failed to link instead.
//! A bisect on the phone found the sun shadow: the near tile's filter beside the cascades'
//! filters. With the near tile compiled out and the cascades on PCF (no PCSS), the full shader
//! compiles and renders the same scene as a Mali-G78.
//!
//! `REGIA_SUN_SHADOW_PCF_ONLY=1` turns the workaround on for any adapter (to look at it on a
//! desktop), `=0` turns it off on Adreno (to re-test a new driver).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use naga_oil::compose::ShaderDefValue;

/// PCI vendor id of Qualcomm (Adreno GPUs) as Vulkan reports it.
pub const QUALCOMM_VENDOR: u32 = 0x5143;

/// The forward shader define that drops the near sun tile and the PCSS cascade filter
/// (`lab_forward.wgsl` `sample_csm` / `sample_cascade`): the sun shadow is PCF on the cascades.
pub const SUN_SHADOW_PCF_ONLY_DEF: &str = "SUN_SHADOW_PCF_ONLY";

/// The forward shader define that compiles out the studio image-light extras (the ground plane,
/// local reflection captures and the image-light debug views): with them in, the Adreno 643
/// driver fails to link the full shader's transmission pipelines, and the device is lost a few
/// seconds after launch (android-pass-v0210, ADR-2042). Set together with
/// [`SUN_SHADOW_PCF_ONLY_DEF`] by the same Qualcomm check.
pub const STUDIO_IBL_OFF_DEF: &str = "STUDIO_IBL_OFF";

/// The forward shader define that compiles out fitted-emitter and LTC sphere light, surface
/// weather, the aerial haze of the weather in-scatter and the pane reflection test: with them
/// in, the Adreno 643 (Fairphone 5) and Adreno 504 (Galaxy Tab A 8.0) compilers crash on the
/// full shader's opaque and GPU-driven colour pipelines, and with part of them out they fail to
/// link it instead (an on-phone bisect, ADR-3145). Set together with
/// [`SUN_SHADOW_PCF_ONLY_DEF`] by the same Qualcomm check.
pub const FORWARD_TRIM_DEF: &str = "FORWARD_TRIM";

/// The environment variable that forces the workaround on (`1`) or off (`0`).
pub const SUN_SHADOW_PCF_ONLY_ENV: &str = "REGIA_SUN_SHADOW_PCF_ONLY";

static SUN_SHADOW_PCF_ONLY: AtomicBool = AtomicBool::new(false);

/// Whether an adapter of `vendor` gets [`SUN_SHADOW_PCF_ONLY_DEF`]; `android` is whether the
/// process runs on Android (the Adreno driver bug is a phone driver, so a Qualcomm GPU on
/// Windows or Linux keeps PCSS and the studio image lighting, ADR-2275); `env` is the value of
/// [`SUN_SHADOW_PCF_ONLY_ENV`] (`1`/`true` on, `0`/`false` off, anything else ignored).
#[must_use]
pub fn sun_shadow_pcf_only_for(vendor: u32, android: bool, env: Option<&str>) -> bool {
    match env.map(str::trim) {
        Some("1" | "true") => true,
        Some("0" | "false") => false,
        _ => android && vendor == QUALCOMM_VENDOR,
    }
}

/// Records the adapter the render device picked; logs when a workaround is on.
pub(crate) fn note_adapter(info: &wgpu::AdapterInfo) {
    let env = regia_core::brand::env_var_os(SUN_SHADOW_PCF_ONLY_ENV);
    let on = sun_shadow_pcf_only_for(
        info.vendor,
        cfg!(target_os = "android"),
        env.as_ref().and_then(|v| v.to_str()),
    );
    SUN_SHADOW_PCF_ONLY.store(on, Ordering::Relaxed);
    if on {
        tracing::info!(
            target: "regia_render",
            "sun shadow: PCF on the cascades only, no near tile or PCSS ({} vendor {:#06x}; {}=0 turns it off)",
            info.name,
            info.vendor,
            SUN_SHADOW_PCF_ONLY_ENV
        );
    }
}

/// Whether this process composes the forward shader with [`SUN_SHADOW_PCF_ONLY_DEF`].
#[must_use]
pub fn sun_shadow_pcf_only() -> bool {
    SUN_SHADOW_PCF_ONLY.load(Ordering::Relaxed)
}

/// The forward shader defines the workaround sets when it is `on` (none when off).
#[must_use]
pub fn workaround_defs(on: bool) -> &'static [&'static str] {
    if on {
        &[SUN_SHADOW_PCF_ONLY_DEF, STUDIO_IBL_OFF_DEF, FORWARD_TRIM_DEF]
    } else {
        &[]
    }
}

/// Adds the workaround defines that are on to `defs`.
pub(crate) fn add_workaround_defs(defs: &mut HashMap<String, ShaderDefValue>) {
    for name in workaround_defs(sun_shadow_pcf_only()) {
        defs.insert((*name).to_string(), ShaderDefValue::Bool(true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adreno_gets_the_sun_shadow_workaround_and_the_environment_overrides() {
        assert!(sun_shadow_pcf_only_for(QUALCOMM_VENDOR, true, None));
        assert!(!sun_shadow_pcf_only_for(0x13B5, true, None), "Arm Mali");
        assert!(!sun_shadow_pcf_only_for(0x10DE, true, None), "NVIDIA");
        assert!(sun_shadow_pcf_only_for(0x10DE, false, Some("1")));
        assert!(!sun_shadow_pcf_only_for(QUALCOMM_VENDOR, true, Some("0")));
        assert!(
            sun_shadow_pcf_only_for(QUALCOMM_VENDOR, true, Some("maybe")),
            "garbage is ignored"
        );
    }

    #[test]
    fn the_workaround_trims_the_forward_shader_with_declared_axes() {
        assert!(workaround_defs(false).is_empty());
        let on = workaround_defs(true);
        assert_eq!(
            on,
            [SUN_SHADOW_PCF_ONLY_DEF, STUDIO_IBL_OFF_DEF, FORWARD_TRIM_DEF]
        );
        for name in on {
            assert!(
                crate::forward_permutations::FORWARD_AXES
                    .iter()
                    .any(|axis| axis.name == *name),
                "{name} is a declared forward axis"
            );
        }
        let source = include_str!("../shaders/lab_forward.wgsl");
        assert!(
            source.matches("#ifdef FORWARD_TRIM").count() >= 4 && source.contains("#ifndef FORWARD_TRIM"),
            "the forward shader reads the define"
        );
    }

    #[test]
    fn qualcomm_off_android_keeps_pcss_unless_forced() {
        assert!(!sun_shadow_pcf_only_for(QUALCOMM_VENDOR, false, None));
        assert!(sun_shadow_pcf_only_for(QUALCOMM_VENDOR, false, Some("1")));
    }
}
