// SPDX-License-Identifier: MPL-2.0
//! CPU frame pipelining switches (ADR-2074, `specs/22` T2.12, `book/agents/frame-pipelining.md`).
//!
//! Measured first (`tests/frame_pipelining_s0.rs`): with a moving camera the sun page cache's
//! per-caster update is the largest single cost of the frame thread on a fast GPU, and pass
//! recording is a small one. So the frame's switch moves that update off the frame thread; a frame
//! is byte-identical with it on or off.
//!
//! The switch lives in [`crate::RenderProfile::frame_pipelining`] and is off in every preset.
//! `REGIA_FRAME_PIPELINING=1` turns it on for every frame of the process, whatever the profile says,
//! so the golden suites can run against it without any test knowing about it.

use std::sync::OnceLock;

/// CPU pipelining of a frame ([`crate::RenderProfile::frame_pipelining`]). Off in every preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FramePipelining {
    /// The caster footprints and the sun page cache's update run on one compute-pool task per
    /// cascade instead of on the frame thread. The result is the same; only the thread differs.
    pub parallel_shadow_plan: bool,
}

impl FramePipelining {
    /// Every switch off (the value of every preset).
    pub const OFF: Self = Self {
        parallel_shadow_plan: false,
    };

    /// Every switch on.
    pub const ON: Self = Self {
        parallel_shadow_plan: true,
    };

    /// What a frame applies: this value, or [`Self::ON`] under `REGIA_FRAME_PIPELINING=1`.
    #[must_use]
    pub fn effective(self) -> Self {
        static FORCED: OnceLock<bool> = OnceLock::new();
        let forced = *FORCED.get_or_init(|| {
            regia_core::brand::env_var("REGIA_FRAME_PIPELINING").is_ok_and(|value| value.trim() == "1")
        });
        if forced { Self::ON } else { self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameQuality, RenderProfile, RenderProfilePreset};

    #[test]
    fn every_preset_and_baseline_is_off() {
        for preset in RenderProfilePreset::ALL
            .into_iter()
            .chain([RenderProfilePreset::Cinematic])
        {
            let profile = RenderProfile::from_preset(preset);
            assert_eq!(profile.frame_pipelining, FramePipelining::OFF, "{preset:?}");
            assert_eq!(
                FrameQuality::from_profile(&profile).frame_pipelining,
                FramePipelining::OFF,
                "{preset:?}"
            );
        }
        assert_eq!(RenderProfile::default().frame_pipelining, FramePipelining::OFF);
        assert_eq!(FrameQuality::LAB.frame_pipelining, FramePipelining::OFF);
        assert_eq!(FrameQuality::EDITOR.frame_pipelining, FramePipelining::OFF);
        assert_eq!(FramePipelining::default(), FramePipelining::OFF);
    }

    /// Other systems choose their tier by the preset (water on Desktop Ultra), so a switch that
    /// promises the same frames must not make the profile a customised one.
    #[test]
    fn the_switch_leaves_the_profile_on_its_preset() {
        for preset in RenderProfilePreset::ALL {
            let mut settings = crate::RenderSettings {
                profile: RenderProfile::from_preset(preset),
                ..crate::RenderSettings::default()
            };
            assert_eq!(settings.preset(), Some(preset));
            settings.profile.frame_pipelining = FramePipelining::ON;
            assert_eq!(
                settings.preset(),
                Some(preset),
                "the switch moved {preset:?} off its row"
            );
        }
    }

    #[test]
    fn the_profile_reaches_the_frame() {
        let profile = RenderProfile {
            frame_pipelining: FramePipelining::ON,
            ..RenderProfile::default()
        };
        assert_eq!(
            FrameQuality::from_profile(&profile).frame_pipelining,
            FramePipelining::ON
        );
    }

    /// Opting in to a feature no preset turns on keeps the preset other systems take their tier
    /// from: the switch moving water from Ultra to High is the bug this mask exists for (ADR-2106).
    #[test]
    fn opt_in_features_keep_the_preset() {
        type Opt = fn(&mut crate::RenderProfile);
        let opts: [(&str, Opt); 12] = [
            ("subsurface_scattering", |p| p.subsurface_scattering = true),
            // `render.set_many_light_sampling {"mode":"production"}` (ADR-2652).
            ("many_light_sampling", |p| p.many_light_sampling = true),
            ("blue_noise", |p| p.blue_noise = true),
            ("order_independent_transparency", |p| {
                p.order_independent_transparency = true;
            }),
            ("radiance_cache", |p| p.radiance_cache = true),
            ("visibility_path", |p| {
                p.visibility_path.single_pass_resolve = true;
            }),
            // Audit v0.2.12: `render.set_visbuffer { enabled: true }` and the RT sun moved water
            // from Ultra (ocean 256) to the custom-profile High arm (ocean 128).
            ("visibility_buffer", |p| p.visibility_buffer = true),
            ("sun_shadow_trace on", |p| {
                p.sun_shadow_trace = crate::hwrt::RtShadowRequest::On;
            }),
            ("sun_shadow_trace compute", |p| {
                p.sun_shadow_trace = crate::hwrt::RtShadowRequest::Compute;
            }),
            ("sun_shadow_trace screen", |p| {
                p.sun_shadow_trace = crate::hwrt::RtShadowRequest::Screen;
            }),
            // ADR-2482: the shadow casters' cull, a runtime switch before it is a preset column.
            ("shadow_occlusion", |p| p.shadow_occlusion = true),
            // ADR-3294: the ground's haze and the weather from the atmosphere tables.
            ("aerial_from_tables", |p| p.aerial_from_tables = true),
        ];
        for preset in crate::RenderProfilePreset::WITH_OFFLINE {
            for (name, opt) in opts {
                let mut settings = crate::RenderSettings::from_profile(preset);
                assert_eq!(settings.preset(), Some(preset));
                opt(&mut settings.profile);
                assert_eq!(settings.preset(), Some(preset), "{name} on {preset:?}");
            }
        }
        // The selectable dense-scene profile is still its preset's row.
        for preset in [
            crate::RenderProfilePreset::DesktopHigh,
            crate::RenderProfilePreset::DesktopUltra,
        ] {
            let mut settings = crate::RenderSettings::from_profile(preset);
            settings.profile = crate::RenderProfile::dense_scenes(preset).expect("dense profile");
            assert_eq!(settings.preset(), Some(preset), "dense_scenes on {preset:?}");
        }
        // A real customisation still reads as custom.
        let mut settings = crate::RenderSettings::from_profile(crate::RenderProfilePreset::DesktopUltra);
        settings.profile.ssr = !settings.profile.ssr;
        assert_eq!(settings.preset(), None);
    }
}
