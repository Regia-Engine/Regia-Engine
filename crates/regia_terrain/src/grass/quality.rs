// SPDX-License-Identifier: MPL-2.0
//! Grass quality tiers: what each render profile can afford.
//!
//! Every tier draws the same clumps near the camera; lower tiers thin them sooner, stop
//! sooner, draw fewer blades with fewer segments, and cast no shadows. [`GrassQuality::Minimal`]
//! is the cheap fallback for phones: a short ring of two-blade, single-segment clumps (cards cut
//! to a blade outline, no alpha test), no shadows. The terrain's own grass layer carries the
//! look past every ring.

use regia_render::RenderProfilePreset;
use serde::{Deserialize, Serialize};

/// A grass quality tier ([`GrassQuality::budget`] gives its numbers).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrassQuality {
    /// No grass at all.
    Off,
    /// The cheap fallback (Mobile): 30 m of sparse two-blade clumps, no shadows.
    Minimal,
    /// Steam Deck and Desktop Low: 70 m, fewer blades, no shadows.
    Low,
    /// Mid-range desktops: 150 m, near shadows off.
    Medium,
    /// Desktop High: 250 m, shadows inside 25 m.
    #[default]
    High,
    /// Desktop Ultra and offline renders: up to 600 m (each class's own range caps it), shadows
    /// inside 40 m, full blades.
    Ultra,
}

/// The numbers of one tier. Distances are metres from the eye.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrassBudget {
    /// Multiplies every class's density, `0..=1`.
    pub density_scale: f32,
    /// Full density out to here; past it the clump density falls as `(full / d)²`, which keeps
    /// the clumps per screen area constant, and the blades widen to keep the cover.
    pub full_density_m: f32,
    /// Farthest any clump draws (caps each class's `range`).
    pub range_m: f32,
    /// Geometry LOD switch distances: near → mid, mid → far.
    pub lod_m: [f32; 2],
    /// Most blades per clump in the near LOD (the class's own count is the other cap).
    pub max_blades: u32,
    /// Segments per blade in the near, mid and far LOD (`1..=4`).
    pub segments: [u32; 3],
    /// Widest a blade grows to keep the cover where the density thins, × its width.
    pub max_widen: f32,
    /// Grass casts sun shadows inside this distance (0 = none).
    pub shadow_m: f32,
    /// Sun cascades the grass draws into, finest first (0 = none).
    pub shadow_cascades: u32,
    /// Most clumps per frame across every class and LOD (the instance buffer's size).
    pub max_instances: u32,
}

impl GrassQuality {
    /// Every tier, cheapest first.
    pub const ALL: [Self; 6] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Ultra,
    ];

    /// The tier's name on the agent protocol (`"high"`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Ultra => "ultra",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.name() == name)
    }

    /// The tier a render profile preset runs: Mobile Low/High → Minimal, Deck and Desktop Low →
    /// Low, Desktop High → High, Desktop Ultra and Cinematic (offline, ADR-1403) → Ultra.
    #[must_use]
    pub const fn for_preset(preset: RenderProfilePreset) -> Self {
        match preset {
            RenderProfilePreset::MobileLow | RenderProfilePreset::MobileHigh => Self::Minimal,
            RenderProfilePreset::DesktopLow | RenderProfilePreset::Deck => Self::Low,
            RenderProfilePreset::DesktopHigh => Self::High,
            RenderProfilePreset::DesktopUltra | RenderProfilePreset::Cinematic => Self::Ultra,
        }
    }

    /// The tier the frame's terrain budget implies when the host chose none: the presets'
    /// terrain anisotropy levels are distinct per tier (Mobile 0–2, Desktop Low and Deck 4,
    /// High 8, Ultra 16), so the level stands for the profile until the frame carries a grass
    /// lever of its own (ADR-1400).
    #[must_use]
    pub fn from_terrain_budget(budget: &regia_render::TerrainBudget) -> Self {
        match budget.anisotropy_level() {
            0..=2 => Self::Minimal,
            3..=4 => Self::Low,
            5..=8 => Self::High,
            _ => Self::Ultra,
        }
    }

    /// The tier's numbers.
    #[must_use]
    pub fn budget(self) -> GrassBudget {
        match self {
            Self::Off => GrassBudget {
                density_scale: 0.0,
                full_density_m: 0.0,
                range_m: 0.0,
                lod_m: [0.0, 0.0],
                max_blades: 0,
                segments: [1, 1, 1],
                max_widen: 1.0,
                shadow_m: 0.0,
                shadow_cascades: 0,
                max_instances: 0,
            },
            Self::Minimal => GrassBudget {
                density_scale: 0.3,
                full_density_m: 6.0,
                range_m: 30.0,
                lod_m: [0.0, 0.0],
                max_blades: 2,
                segments: [1, 1, 1],
                max_widen: 3.0,
                shadow_m: 0.0,
                shadow_cascades: 0,
                max_instances: 16_384,
            },
            Self::Low => GrassBudget {
                density_scale: 0.45,
                full_density_m: 10.0,
                range_m: 70.0,
                lod_m: [6.0, 25.0],
                max_blades: 5,
                segments: [3, 2, 1],
                max_widen: 4.0,
                shadow_m: 0.0,
                shadow_cascades: 0,
                max_instances: 65_536,
            },
            Self::Medium => GrassBudget {
                density_scale: 0.65,
                full_density_m: 16.0,
                range_m: 150.0,
                lod_m: [10.0, 45.0],
                max_blades: 7,
                segments: [3, 2, 1],
                max_widen: 5.0,
                shadow_m: 0.0,
                shadow_cascades: 0,
                max_instances: 196_608,
            },
            Self::High => GrassBudget {
                density_scale: 0.85,
                full_density_m: 25.0,
                range_m: 250.0,
                lod_m: [12.0, 60.0],
                max_blades: 12,
                segments: [4, 2, 1],
                max_widen: 6.0,
                shadow_m: 25.0,
                shadow_cascades: 1,
                max_instances: 393_216,
            },
            Self::Ultra => GrassBudget {
                density_scale: 1.0,
                full_density_m: 40.0,
                range_m: 600.0,
                lod_m: [16.0, 80.0],
                max_blades: 16,
                segments: [4, 3, 1],
                max_widen: 8.0,
                shadow_m: 40.0,
                shadow_cascades: 2,
                max_instances: 786_432,
            },
        }
    }
}

impl GrassBudget {
    /// Whether this budget draws anything.
    #[must_use]
    pub fn draws(&self) -> bool {
        self.density_scale > 0.0 && self.range_m > 0.0 && self.max_instances > 0 && self.max_blades > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_get_cheaper_downward() {
        let budgets: Vec<GrassBudget> = GrassQuality::ALL.iter().map(|q| q.budget()).collect();
        for pair in budgets.windows(2) {
            let (lo, hi) = (pair[0], pair[1]);
            assert!(lo.density_scale <= hi.density_scale);
            assert!(lo.range_m <= hi.range_m);
            assert!(lo.max_instances <= hi.max_instances);
            assert!(lo.shadow_m <= hi.shadow_m);
            assert!(lo.max_blades <= hi.max_blades);
        }
        assert!(!GrassQuality::Off.budget().draws());
        assert!(GrassQuality::Minimal.budget().draws());
        assert_eq!(GrassQuality::Minimal.budget().shadow_cascades, 0);
        assert_eq!(GrassQuality::Low.budget().shadow_cascades, 0);
    }

    #[test]
    fn presets_map_to_tiers() {
        assert_eq!(
            GrassQuality::for_preset(RenderProfilePreset::Deck),
            GrassQuality::Low
        );
        assert_eq!(
            GrassQuality::for_preset(RenderProfilePreset::MobileHigh),
            GrassQuality::Minimal
        );
        assert_eq!(
            GrassQuality::for_preset(RenderProfilePreset::DesktopUltra),
            GrassQuality::Ultra
        );
        // The terrain budget of each preset implies the preset's own tier.
        for preset in RenderProfilePreset::ALL {
            let profile = regia_render::RenderProfile::from_preset(preset);
            let quality = regia_render::FrameQuality::from_profile(&profile);
            assert_eq!(
                GrassQuality::from_terrain_budget(&quality.terrain_budget),
                GrassQuality::for_preset(preset),
                "{}",
                preset.name()
            );
        }
    }

    #[test]
    fn names_round_trip() {
        for q in GrassQuality::ALL {
            assert_eq!(GrassQuality::from_name(q.name()), Some(q));
        }
        assert_eq!(GrassQuality::from_name("extreme"), None);
    }

    #[test]
    fn lod_distances_are_ordered_and_segments_in_range() {
        for q in GrassQuality::ALL {
            let b = q.budget();
            assert!(b.lod_m[0] <= b.lod_m[1]);
            assert!(b.segments.iter().all(|s| (1..=4).contains(s)));
            assert!(b.segments.windows(2).all(|w| w[0] >= w[1]));
        }
    }
}
