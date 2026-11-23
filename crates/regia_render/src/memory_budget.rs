// SPDX-License-Identifier: MPL-2.0
//! Memory budgets per hardware tier, current use by category and per load set, and the
//! admission check "can I preload set X?" (`docs/specs/12-assets.md` § 5b).
//!
//! Current use is [`crate::memory_report`] (every live GPU allocation by category, the process
//! RSS); a load set's cost is the sum of its items' RAM/VRAM estimates
//! ([`regia_asset::LoadSet`]). An admission adds the set's **non-resident** items to the
//! current use and compares with the tier's budget, so preloading a second map behind the menu
//! is a single call: `admit(&budget, &report, &sets, "map:bore_twelve")`.

use regia_asset::{LoadSet, LoadSets};

use crate::memory::{MemoryCategory, MemoryReport};

/// A hardware tier the budgets are set for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BudgetTier {
    /// The discrete-GPU minimum (DO27's): RTX 2080 class, 8 GB VRAM, 16 GB RAM, NVMe.
    Minimum,
    /// Steam Deck: 16 GB unified (the GPU's share is set in the BIOS; 4 GB default carve-out
    /// plus GTT), budgets from `docs/evidence/perf/memory-20261007.md`.
    Deck,
    /// Desktop High: 12 GB+ VRAM, 32 GB RAM.
    DesktopHigh,
    /// The integrated-GPU floor (ADR-7977, ADR-8878): an 8 GB shared-memory laptop at 1280×720,
    /// the tier [`crate::QualityTier::VeryLow`] is sized for. RAM 2.5 GiB, GPU memory 1.5 GiB.
    /// Appended last so the first three entries of [`Self::ALL`] and every report keyed on it keep
    /// their order. Nothing selects it automatically: a host that runs Very Low admits loads
    /// against it.
    IntegratedLow,
}

impl BudgetTier {
    /// Every tier.
    pub const ALL: [Self; 4] = [Self::Minimum, Self::Deck, Self::DesktopHigh, Self::IntegratedLow];

    /// Stable snake-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Minimum => "minimum",
            Self::Deck => "deck",
            Self::DesktopHigh => "desktop_high",
            Self::IntegratedLow => "integrated_low",
        }
    }

    /// The tier named `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }

    /// Every tier's name, in [`Self::ALL`] order, comma-separated (error messages).
    #[must_use]
    pub fn names() -> String {
        Self::ALL.map(Self::name).join(", ")
    }
}

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// What the game may use: GPU memory (every live allocation of the device) and process RSS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryBudget {
    /// Process resident set, bytes.
    pub ram: u64,
    /// GPU memory, bytes.
    pub vram: u64,
}

impl MemoryBudget {
    /// The budget of `tier`.
    ///
    /// - Minimum (8 GB card, 16 GB machine): VRAM 6.5 GiB — the card minus ~1.5 GiB the desktop
    ///   compositor, driver reserves, the Steam overlay and the swapchain keep; RAM 10 GiB — the
    ///   machine minus ~6 GiB for the OS, Steam, a browser and Discord.
    /// - Deck: VRAM 3 GiB, RAM 5 GiB (mem-stream's budgets).
    /// - Desktop High: VRAM 10 GiB, RAM 16 GiB.
    /// - Integrated Low (8 GB machine, integrated GPU, 1280×720 at Very Low): RAM 2.5 GiB and GPU
    ///   memory 1.5 GiB, carved from the same 8 GB; the other 4 GiB are the firmware carve-out, the
    ///   OS, the compositor, the Steam client and the driver's kernel side (not measured). The
    ///   1.5 GiB is about 715 MiB of fixed lines (swapchain 11, render targets at most 32, sun
    ///   cascades 32, local shadow atlas 64, Very Low's texture streaming 384, baked GI at most 192)
    ///   plus at most 820 MiB of meshes, terrain tiles, instance buffers, belts and allocator
    ///   slack; the 2.5 GiB is the measured 920 MiB process of the showcase scene at Desktop Low
    ///   720p on the N150, 768 MiB of host mesh and texture copies at most, and about 870 MiB for
    ///   the game (spec 13 § 3a).
    #[must_use]
    pub fn for_tier(tier: BudgetTier) -> Self {
        match tier {
            BudgetTier::Minimum => Self {
                ram: 10 * GIB,
                vram: 6 * GIB + 512 * MIB,
            },
            BudgetTier::Deck => Self {
                ram: 5 * GIB,
                vram: 3 * GIB,
            },
            BudgetTier::DesktopHigh => Self {
                ram: 16 * GIB,
                vram: 10 * GIB,
            },
            BudgetTier::IntegratedLow => Self {
                ram: 2560 * MIB,
                vram: 1536 * MIB,
            },
        }
    }
}

/// One load set's estimated cost and residency.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SetUse {
    /// Set name.
    pub name: String,
    /// Estimated RAM of all items.
    pub ram_bytes: u64,
    /// Estimated VRAM of all items.
    pub vram_bytes: u64,
    /// Estimated RAM of the items not resident yet.
    pub ram_pending: u64,
    /// Estimated VRAM of the items not resident yet.
    pub vram_pending: u64,
    /// Fraction resident ([`regia_asset::LoadProgress::fraction`]).
    pub fraction: f32,
}

/// Current memory use: by category (measured) and per load set (estimated).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemoryUse {
    /// Measured GPU bytes by category.
    pub vram_by_category: Vec<(MemoryCategory, u64)>,
    /// Measured GPU bytes in total: `MemoryReport::device_allocated_bytes` (the allocator total on
    /// Vulkan / D3D12, the device's allocated size on Metal); `0` when the backend reports neither.
    pub vram: u64,
    /// Measured process RSS (`0` where the OS does not report it).
    pub ram: u64,
    /// Whether the GPU total is measured (Vulkan / D3D12 / Metal; the categories are Vulkan and
    /// D3D12 only).
    pub vram_measured: bool,
    /// Every defined load set.
    pub sets: Vec<SetUse>,
}

/// The cost of `set` against `sets`' residency.
#[must_use]
pub fn set_use(set: &LoadSet, sets: &LoadSets) -> SetUse {
    let mut out = SetUse {
        name: set.name.clone(),
        ..SetUse::default()
    };
    // Saturating, like `LoadSet::bytes`: a wrapped total would read as a small set.
    for item in &set.items {
        out.ram_bytes = out.ram_bytes.saturating_add(item.ram_bytes);
        out.vram_bytes = out.vram_bytes.saturating_add(item.vram_bytes);
        if !sets.item_resident(item.kind, &item.key) {
            out.ram_pending = out.ram_pending.saturating_add(item.ram_bytes);
            out.vram_pending = out.vram_pending.saturating_add(item.vram_bytes);
        }
    }
    out.fraction = sets.load_progress(&set.name).map_or(0.0, |p| p.fraction);
    out
}

/// Current use from a memory report plus every load set.
#[must_use]
pub fn memory_use(report: &MemoryReport, sets: &LoadSets) -> MemoryUse {
    MemoryUse {
        vram_by_category: report.categories.iter().map(|(c, t)| (*c, t.bytes)).collect(),
        // The whole device's figure (ADR-5860): the allocator total on Vulkan and D3D12, the Metal
        // device's allocated size on Metal; unmeasured only on GL and WebGPU.
        vram: report.device_allocated_bytes.unwrap_or(report.allocated_bytes),
        ram: report.rss_bytes.unwrap_or(0),
        vram_measured: report.device_allocated_bytes.is_some() || report.available,
        sets: sets
            .names()
            .iter()
            .filter_map(|n| sets.get(n))
            .map(|s| set_use(&s, sets))
            .collect(),
    }
}

/// The answer to "can I preload this set now?".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Admission {
    /// Set name.
    pub name: String,
    /// Both budgets hold after the set's pending items are resident (false while a side is
    /// not measured).
    pub fits: bool,
    /// GPU bytes now (measured).
    pub vram_now: u64,
    /// GPU bytes after the set (now + its pending estimate).
    pub vram_after: u64,
    /// RSS now (measured).
    pub ram_now: u64,
    /// RSS after the set.
    pub ram_after: u64,
    /// The budget checked against.
    pub budget_vram: u64,
    /// The budget checked against.
    pub budget_ram: u64,
    /// Why it does not fit, if it does not.
    pub reason: Option<String>,
}

/// Checks whether `set` fits in `budget` on top of `current` (its resident items are already
/// part of `current`). An unknown set is refused, and so is any set while a side of `current`
/// is not measured ([`MemoryUse::vram_measured`] false, or [`MemoryUse::ram`] 0): the reason
/// says which. Byte totals saturate at `u64::MAX`.
#[must_use]
pub fn admit(budget: &MemoryBudget, current: &MemoryUse, sets: &LoadSets, set: &str) -> Admission {
    let mut out = Admission {
        name: set.to_string(),
        vram_now: current.vram,
        ram_now: current.ram,
        budget_vram: budget.vram,
        budget_ram: budget.ram,
        ..Admission::default()
    };
    let Some(definition) = sets.get(set) else {
        out.reason = Some(format!("no load set named {set:?}"));
        return out;
    };
    let cost = set_use(&definition, sets);
    // A saturated total is `u64::MAX`, over any budget below it.
    out.vram_after = current.vram.saturating_add(cost.vram_pending);
    out.ram_after = current.ram.saturating_add(cost.ram_pending);
    let mut reasons = Vec::new();
    // A side that was not measured has no current use to add to: the set cannot be shown to
    // fit (GL, Metal and WebGPU have no allocator report; RSS is read on Linux, Android and
    // Windows only, and `MemoryUse::ram` is 0 elsewhere).
    if !current.vram_measured {
        reasons.push("VRAM use is not measured on this backend (no allocator report)".to_owned());
    } else if out.vram_after > budget.vram {
        reasons.push(format!(
            "VRAM {:.2} GiB after the set exceeds the {:.2} GiB budget",
            out.vram_after as f64 / GIB as f64,
            budget.vram as f64 / GIB as f64
        ));
    }
    if current.ram == 0 {
        reasons.push("RAM use is not measured on this OS (no resident-set sample)".to_owned());
    } else if out.ram_after > budget.ram {
        reasons.push(format!(
            "RAM {:.2} GiB after the set exceeds the {:.2} GiB budget",
            out.ram_after as f64 / GIB as f64,
            budget.ram as f64 / GIB as f64
        ));
    }
    out.fits = reasons.is_empty();
    out.reason = (!reasons.is_empty()).then(|| reasons.join("; "));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::LoadKind;

    fn report(vram: u64, rss: u64) -> MemoryReport {
        let mut r = MemoryReport::from_allocations("Vulkan", [("frame hdr", vram)], vram);
        r.rss_bytes = Some(rss);
        r
    }

    #[test]
    fn admission_counts_only_pending_items() {
        let sets = LoadSets::default();
        sets.define(
            LoadSet::new("map:a")
                .with(LoadKind::Texture, "a.ktx2", 0, 2 * GIB)
                .with(LoadKind::AudioBank, "a.bank", GIB, 0),
        );
        let budget = MemoryBudget::for_tier(BudgetTier::Minimum);
        let now = memory_use(&report(3 * GIB, 4 * GIB), &sets);
        let a = admit(&budget, &now, &sets, "map:a");
        assert!(a.fits, "{a:?}");
        assert_eq!(a.vram_after, 5 * GIB);
        assert_eq!(a.ram_after, 5 * GIB);
        // Once resident, the set costs nothing more.
        sets.mark_resident(LoadKind::Texture, "a.ktx2");
        let a = admit(&budget, &now, &sets, "map:a");
        assert_eq!(a.vram_after, 3 * GIB);
        // Too much VRAM.
        let full = memory_use(&report(5 * GIB, 4 * GIB), &sets);
        sets.mark_evicted(LoadKind::Texture, "a.ktx2");
        let a = admit(&budget, &full, &sets, "map:a");
        assert!(!a.fits);
        assert!(a.reason.as_deref().is_some_and(|r| r.contains("VRAM")));
        let unknown = admit(&budget, &full, &sets, "map:nope");
        assert!(!unknown.fits);
    }

    /// Without a device figure (GL, WebGPU) current VRAM is unknown, and without an
    /// RSS sample current RAM is: an admission cannot claim the set fits (audit render10 #5).
    #[test]
    fn an_unmeasured_side_is_not_admitted() {
        let sets = LoadSets::default();
        sets.define(LoadSet::new("map:a").with(LoadKind::Texture, "a.ktx2", MIB, 2 * GIB));
        let budget = MemoryBudget::for_tier(BudgetTier::Minimum);
        // GL: no allocator report, no device figure and no RSS sample.
        let blind = memory_use(&MemoryReport::default(), &sets);
        assert!(!blind.vram_measured);
        let a = admit(&budget, &blind, &sets, "map:a");
        assert!(!a.fits, "{a:?}");
        let reason = a.reason.unwrap_or_default();
        assert!(reason.contains("VRAM") && reason.contains("RAM"), "{reason}");
        // WebGPU: RSS is real, VRAM is not.
        let web = MemoryReport {
            rss_bytes: Some(GIB),
            ..MemoryReport::default()
        };
        let a = admit(&budget, &memory_use(&web, &sets), &sets, "map:a");
        assert!(!a.fits, "{a:?}");
        assert!(a.reason.as_deref().is_some_and(|r| r.contains("VRAM")), "{a:?}");
        // Measured VRAM, no RSS sample.
        let mut no_rss = report(GIB, 0);
        no_rss.rss_bytes = None;
        let a = admit(&budget, &memory_use(&no_rss, &sets), &sets, "map:a");
        assert!(!a.fits, "{a:?}");
        // Both measured: admitted as before.
        let a = admit(&budget, &memory_use(&report(GIB, GIB), &sets), &sets, "map:a");
        assert!(a.fits, "{a:?}");
    }

    /// Metal (ADR-5860): no allocator report, but the device's allocated size is a measured VRAM
    /// figure, so an admission can fit, and it counts that figure, not the empty allocator total.
    #[test]
    fn the_metal_device_size_is_measured_vram() {
        let sets = LoadSets::default();
        sets.define(LoadSet::new("map:a").with(LoadKind::Texture, "a.ktx2", MIB, 2 * GIB));
        let budget = MemoryBudget::for_tier(BudgetTier::Minimum);
        let metal = MemoryReport {
            backend: "Metal".to_owned(),
            rss_bytes: Some(GIB),
            device_allocated_bytes: Some(GIB),
            ..MemoryReport::default()
        };
        let now = memory_use(&metal, &sets);
        assert!(now.vram_measured);
        assert_eq!(now.vram, GIB);
        let a = admit(&budget, &now, &sets, "map:a");
        assert!(a.fits, "{a:?}");
        assert_eq!(a.vram_after, 3 * GIB);
        let full = MemoryReport {
            device_allocated_bytes: Some(5 * GIB),
            ..metal
        };
        let a = admit(&budget, &memory_use(&full, &sets), &sets, "map:a");
        assert!(!a.fits, "{a:?}");
        assert!(a.reason.as_deref().is_some_and(|r| r.contains("VRAM")), "{a:?}");
    }

    /// Item sizes add without wrapping: a set whose estimate passes `u64::MAX` never fits
    /// (audit render10 #6; `LoadSet::bytes` saturates the same way).
    #[test]
    fn huge_estimates_saturate_and_do_not_fit() {
        let sets = LoadSets::default();
        sets.define(
            LoadSet::new("map:huge")
                .with(LoadKind::Texture, "a", 0, 1 << 63)
                .with(LoadKind::Texture, "b", 0, 1 << 63),
        );
        sets.define(LoadSet::new("map:max").with(LoadKind::Texture, "c", u64::MAX, u64::MAX));
        let budget = MemoryBudget::for_tier(BudgetTier::Minimum);
        let now = memory_use(&report(0, GIB), &sets);
        let huge = &now.sets.iter().find(|s| s.name == "map:huge").expect("set");
        assert_eq!(huge.vram_pending, u64::MAX);
        assert_eq!(huge.vram_bytes, u64::MAX);
        let a = admit(&budget, &now, &sets, "map:huge");
        assert!(!a.fits, "{a:?}");
        assert_eq!(a.vram_after, u64::MAX);
        // One resident byte plus a pending u64::MAX does not wrap to zero either.
        let one = memory_use(&report(1, 1), &sets);
        let a = admit(&budget, &one, &sets, "map:max");
        assert!(!a.fits, "{a:?}");
        assert_eq!((a.vram_after, a.ram_after), (u64::MAX, u64::MAX));
    }

    #[test]
    fn use_lists_categories_and_sets() {
        let sets = LoadSets::default();
        sets.define(LoadSet::new("menu").with(LoadKind::Texture, "logo", 0, MIB));
        let u = memory_use(&report(100, 200), &sets);
        assert_eq!(u.vram, 100);
        assert_eq!(u.ram, 200);
        assert!(u.vram_measured);
        assert_eq!(u.sets.len(), 1);
        assert_eq!(u.sets[0].vram_pending, MIB);
        for tier in BudgetTier::ALL {
            assert_eq!(BudgetTier::from_name(tier.name()), Some(tier));
            let b = MemoryBudget::for_tier(tier);
            assert!(b.vram > 0 && b.ram > b.vram / 2);
        }
    }

    /// The integrated-GPU floor row (ADR-7977 build B, ADR-8878): named `integrated_low`, appended
    /// after the first three tiers, 2.5 GiB RAM and 1.5 GiB GPU memory, below the Deck and the
    /// discrete-GPU minimum on both lines; baked GI's eighth is 192 MiB.
    #[test]
    fn integrated_low_is_the_floor_row() {
        let floor = BudgetTier::from_name("integrated_low").expect("a tier named integrated_low");
        assert_eq!(floor.name(), "integrated_low");
        assert_eq!(BudgetTier::ALL.len(), 4, "{:?}", BudgetTier::ALL);
        assert_eq!(
            BudgetTier::ALL[..3],
            [BudgetTier::Minimum, BudgetTier::Deck, BudgetTier::DesktopHigh],
            "the first three tiers keep their order"
        );
        assert_eq!(BudgetTier::ALL.get(3), Some(&floor), "appended last");
        let budget = MemoryBudget::for_tier(floor);
        assert_eq!(budget.ram, 2560 * MIB);
        assert_eq!(budget.vram, 1536 * MIB);
        for above in [BudgetTier::Deck, BudgetTier::Minimum] {
            let b = MemoryBudget::for_tier(above);
            assert!(budget.ram < b.ram && budget.vram < b.vram, "{above:?}");
        }
        assert_eq!(crate::baked_gi::memory::baked_gi_budget(floor), 192 * MIB);
    }
}
