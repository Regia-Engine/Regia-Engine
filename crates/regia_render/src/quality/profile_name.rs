// SPDX-License-Identifier: MPL-2.0
//! [`ProfileName`]: the names a host, `Regia.toml` or the agent may give where a render profile
//! is chosen (ADR-7977 build A, ADR-8844, `specs/13` § 3a). A name is a row of the preset table
//! or one of the quality-ladder tiers that has no row of its own ([`ProfileName::TIERS`]: Very
//! Low and Medium), so the integrated-GPU floor can be selected by name without the engine adding
//! a preset row.

use crate::{RenderProfilePreset, RenderSettings};

use super::{AdapterProfile, QualityTier, TerrainVtPolicy};

/// A name given where a render profile is chosen (ADR-7977): a row of the preset table, or a
/// quality-ladder tier that has no row of its own and is its preset with some columns rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileName {
    /// A preset row ([`RenderProfilePreset::from_name`]).
    Preset(RenderProfilePreset),
    /// A tier of [`ProfileName::TIERS`].
    Tier(QualityTier),
}

impl ProfileName {
    /// The tiers accepted as profile names besides the presets: the ones with no row of their
    /// own. `Low`, `High` and `Ultra` are rows already (Desktop Low, High, Ultra) and stay
    /// unknown names, so no row gets a second name.
    pub const TIERS: [QualityTier; 2] = [QualityTier::VeryLow, QualityTier::Medium];

    /// Parses `name` as [`RenderProfilePreset::from_name`] does (case-insensitive, `_`/`-`
    /// ignored, surrounding blanks trimmed); failing that, one of [`Self::TIERS`] by
    /// [`QualityTier::from_name`]. Preset names win. `Low`, `High`, `Ultra` and `""` are `None`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim();
        if let Some(preset) = RenderProfilePreset::from_name(name) {
            return Some(Self::Preset(preset));
        }
        QualityTier::from_name(name)
            .filter(|tier| Self::TIERS.contains(tier))
            .map(Self::Tier)
    }

    /// The canonical name (`"DesktopLow"`, `"VeryLow"`): what `Regia.toml` and the agent write.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Preset(preset) => preset.name(),
            Self::Tier(tier) => tier.name(),
        }
    }

    /// The preset row it starts from (Very Low: Desktop Low; Medium: Desktop High). Systems that
    /// gate or pick their own tier by preset (the Cinematic gate, water, foliage) use this one.
    #[must_use]
    pub const fn preset(self) -> RenderProfilePreset {
        match self {
            Self::Preset(preset) => preset,
            Self::Tier(tier) => tier.preset(),
        }
    }

    /// The quality tier it names, `None` for a preset row.
    #[must_use]
    pub const fn tier(self) -> Option<QualityTier> {
        match self {
            Self::Preset(_) => None,
            Self::Tier(tier) => Some(tier),
        }
    }

    /// Applies it live: [`RenderSettings::apply_preset`] for a preset,
    /// [`QualityTier::apply_for_gpu`] for a tier (`adapter` decides the GPU-dependent levers;
    /// `None` = not known yet).
    pub fn apply_for_gpu(self, settings: &mut RenderSettings, adapter: Option<&AdapterProfile>) {
        self.apply_for_gpu_with(settings, adapter, &TerrainVtPolicy::default());
    }

    /// [`Self::apply_for_gpu`] under a host's [`TerrainVtPolicy`] (a tier's lever follows
    /// [`QualityTier::apply_for_gpu_with`]; a preset ignores it).
    pub fn apply_for_gpu_with(
        self,
        settings: &mut RenderSettings,
        adapter: Option<&AdapterProfile>,
        policy: &TerrainVtPolicy,
    ) {
        match self {
            Self::Preset(preset) => settings.apply_preset(preset),
            Self::Tier(tier) => tier.apply_for_gpu_with(settings, adapter, policy),
        }
    }

    /// Every accepted name, presets first ([`RenderProfilePreset::WITH_OFFLINE`], then
    /// [`Self::TIERS`]).
    #[must_use]
    pub fn names() -> Vec<&'static str> {
        RenderProfilePreset::WITH_OFFLINE
            .iter()
            .map(|p| p.name())
            .chain(Self::TIERS.iter().map(|t| t.name()))
            .collect()
    }
}

impl From<RenderProfilePreset> for ProfileName {
    fn from(preset: RenderProfilePreset) -> Self {
        Self::Preset(preset)
    }
}

impl std::fmt::Display for ProfileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SpatialUpscaler;
    use crate::quality::DeviceClass;

    /// The Intel N150 the `intel_n_series` row of the GPU table measures (PCI 8086:46d4).
    fn n150() -> AdapterProfile {
        let mut adapter = AdapterProfile::named("Intel(R) Graphics (N150)", 0x8086, Some(2_048));
        adapter.device = 0x46d4;
        adapter.class = DeviceClass::Integrated;
        adapter
    }

    #[test]
    fn profile_name_parses_tiers_without_renaming_presets() {
        for name in ["VeryLow", "very_low", "very-low", "VERYLOW", " VeryLow "] {
            assert_eq!(
                ProfileName::parse(name),
                Some(ProfileName::Tier(QualityTier::VeryLow)),
                "{name:?}"
            );
        }
        assert_eq!(
            ProfileName::parse("Medium"),
            Some(ProfileName::Tier(QualityTier::Medium))
        );
        assert_eq!(
            ProfileName::parse("DesktopLow"),
            Some(ProfileName::Preset(RenderProfilePreset::DesktopLow))
        );
        // The tiers that are rows already keep one name only; the mobile tiers and the Deck are
        // preset names, so they resolve to the preset.
        for name in ["Low", "High", "Ultra", "", "  ", "UltraLow", "phone"] {
            assert_eq!(ProfileName::parse(name), None, "{name:?}");
        }
        for (name, preset) in [
            ("MobileLow", RenderProfilePreset::MobileLow),
            ("MobileHigh", RenderProfilePreset::MobileHigh),
            ("Deck", RenderProfilePreset::Deck),
        ] {
            assert_eq!(ProfileName::parse(name), Some(ProfileName::Preset(preset)));
        }
        for preset in RenderProfilePreset::WITH_OFFLINE {
            let name = ProfileName::parse(preset.name());
            assert_eq!(name, Some(ProfileName::Preset(preset)));
            assert_eq!(name.map(ProfileName::name), Some(preset.name()));
            assert_eq!(name.map(ProfileName::preset), Some(preset));
        }
        for tier in ProfileName::TIERS {
            let name = ProfileName::Tier(tier);
            assert_eq!(ProfileName::parse(name.name()), Some(name), "round trip");
            assert_eq!(name.preset(), tier.preset());
            assert_eq!(name.tier(), Some(tier));
        }
        let names = ProfileName::names();
        assert_eq!(
            names.len(),
            RenderProfilePreset::WITH_OFFLINE.len() + ProfileName::TIERS.len()
        );
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "no duplicates: {names:?}");
        assert_eq!(names.last(), Some(&"Medium"));
        assert!(names.contains(&"VeryLow"));
        assert!(!names.contains(&"Low"));
        assert_eq!(ProfileName::from(RenderProfilePreset::Deck).to_string(), "Deck");
    }

    #[test]
    fn very_low_by_name_applies_the_tier_and_keeps_preset_desktop_low() {
        let name = ProfileName::parse("VeryLow");
        assert_eq!(
            name.map(ProfileName::preset),
            Some(RenderProfilePreset::DesktopLow)
        );
        let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
        let adapter = n150();
        if let Some(name) = name {
            name.apply_for_gpu(&mut settings, Some(&adapter));
        }
        assert_eq!(QualityTier::of_settings(&settings), Some(QualityTier::VeryLow));
        assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopLow));
        assert!((settings.profile.render_scale - 1.0 / 3.0).abs() < 1e-6);
        assert_eq!(settings.profile.spatial_upscaler, SpatialUpscaler::Sgsr);
        // An integrated GPU never gets the terrain virtual texture lever.
        assert!(!settings.profile.terrain_budget.virtual_texture);
        // The same through the tier itself: the name adds nothing of its own.
        let mut direct = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
        QualityTier::VeryLow.apply_for_gpu(&mut direct, Some(&adapter));
        assert_eq!(direct.profile, settings.profile);
    }

    /// Memory "a runtime-set RenderProfile field makes preset() None": every built-in preset
    /// applied by name, then a tier, then the preset again, still answers its own row.
    #[test]
    fn every_preset_still_names_itself_after_a_tier_was_set_by_name() {
        let adapter = n150();
        for preset in RenderProfilePreset::WITH_OFFLINE {
            let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
            ProfileName::Tier(QualityTier::VeryLow).apply_for_gpu(&mut settings, Some(&adapter));
            ProfileName::Preset(preset).apply_for_gpu(&mut settings, Some(&adapter));
            assert_eq!(settings.preset(), Some(preset), "{}", preset.name());
            assert_ne!(
                QualityTier::of_settings(&settings),
                Some(QualityTier::VeryLow),
                "{}: the tier's columns are gone",
                preset.name()
            );
        }
        // Medium by name is Desktop High at 0.67 with its lighting default.
        let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopLow);
        ProfileName::Tier(QualityTier::Medium).apply_for_gpu(&mut settings, None);
        assert_eq!(QualityTier::of_settings(&settings), Some(QualityTier::Medium));
    }
}
