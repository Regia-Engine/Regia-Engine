// SPDX-License-Identifier: MPL-2.0
//! A baked GI memory answer before a bake runs (ADR-3006, `book/agents/baked-gi-split.md`): the bytes
//! a volume takes against each [`BudgetTier`], what is over and what would fix it.
//!
//! Two limits apply. One is **the volume's share of the tier's VRAM budget**
//! ([`baked_gi_budget`]): a volume lives in GPU memory for as long as its level is open, so its
//! bytes come off what the map's textures and meshes have. The other is **one storage binding**
//! ([`PORTABLE_VOLUME_BYTES`], every tier: what a volume must fit to install everywhere), which a
//! split volume meets part by part. A split does not shrink a volume (its parts overlap, so it
//! grows a little), so it fixes the binding and never the budget; only coarser probes fix the budget.
//!
//! The report is arithmetic over the layout: no GPU, no probes and no bake. `bytes` are
//! [`VolumeLayout::runtime_buffer_bytes`], the buffer the renderer uploads. A fix's `bytes_after`
//! is an estimate, since it leaves out the brick lookup grid's small change.

use regia_asset::baked_gi::{
    DetailBrick, GI_COMPONENTS, META_RECORD_BYTES, MOMENT_TEXEL_BYTES, PORTABLE_VOLUME_BYTES,
    SH_RECORD_BYTES, VolumeLayout,
};
use regia_asset::baked_gi_split::{MAX_SPLIT_PARTS, split_layout};
use serde::Serialize;

use crate::memory_budget::{BudgetTier, MemoryBudget};

/// Over a tier's baked GI share of its VRAM budget ([`baked_gi_budget`]).
pub const CODE_OVER_BUDGET: &str = "baked_gi_over_budget";
/// One volume or part is above one portable storage binding.
pub const CODE_OVER_BINDING: &str = "baked_gi_part_over_binding";
/// Most fixes a tier names: the largest savings first.
const MAX_FIXES: usize = 8;
/// Contributors a report lists.
const MAX_CONTRIBUTORS: usize = 8;

/// The bytes of a tier's VRAM budget baked GI may take: an eighth (`MemoryBudget::vram / 8`:
/// 832 MiB at minimum, 384 MiB on the Deck, 1.25 GiB at desktop high, 192 MiB at the integrated
/// low floor). The recommended layout of a
/// large map is 51 MiB and the sizing study's ceiling 100 MiB (`docs/research/bake-sizing-bore-twelve.md`),
/// so the share is room for a split volume of several times that, and small enough that a map's
/// textures and meshes keep the rest. ADR-3006 has the numbers.
#[must_use]
pub fn baked_gi_budget(tier: BudgetTier) -> u64 {
    MemoryBudget::for_tier(tier).vram / 8
}

/// What is over a limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BakedGiMemoryOver {
    /// [`CODE_OVER_BUDGET`] or [`CODE_OVER_BINDING`].
    pub code: &'static str,
    /// What is over: `volume` for the whole set, or a part's label.
    pub target: String,
    /// Its bytes.
    pub bytes: u64,
    /// The limit it is over.
    pub limit: u64,
}

/// One change that brings a report closer to a limit.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BakedGiMemoryFix {
    /// `spacing` (coarser probes) or `split` (the bake writes parts).
    pub fix: &'static str,
    /// What to change: `terrain`, a brick's label, or a part's label.
    pub target: String,
    /// The change in words.
    pub action: String,
    /// Total bytes of the set after this fix and the ones listed before it.
    pub bytes_after: u64,
}

/// One tier's answer.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BakedGiTierMemory {
    /// [`BudgetTier::name`].
    pub tier: &'static str,
    /// The tier's VRAM budget.
    pub vram_budget: u64,
    /// Baked GI's share of it ([`baked_gi_budget`]).
    pub budget: u64,
    /// Bytes of the set, every part counted (host memory holds the same bytes again).
    pub bytes: u64,
    /// `bytes` as a share of `budget`.
    pub budget_used: f32,
    /// Nothing is over.
    pub fits: bool,
    /// What is over on this tier.
    pub over: Vec<BakedGiMemoryOver>,
    /// What would fix it, in order, the largest saving first; empty when it fits.
    pub fixes: Vec<BakedGiMemoryFix>,
    /// The fixes bring every limit in. `false` when the list ran out first (a set of many small
    /// bricks): lower the spacing across the layout, or the map's extent.
    pub fixes_suffice: bool,
}

/// What takes the bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BakedGiMemoryContributor {
    /// `terrain` or a brick's label.
    pub target: String,
    /// Its bytes in the buffer.
    pub bytes: u64,
}

/// The answer: bytes per tier, what is over, what fixes it ([`layout_report`], [`volumes_report`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BakedGiMemoryReport {
    /// Total bytes: every part, each in its own buffer.
    pub bytes: u64,
    /// Each part's or volume's bytes, in order (one entry for a whole volume).
    pub parts: Vec<u64>,
    /// [`PORTABLE_VOLUME_BYTES`].
    pub binding_limit: u64,
    /// The largest contributors, largest first.
    pub contributors: Vec<BakedGiMemoryContributor>,
    /// One entry per [`BudgetTier::ALL`].
    pub tiers: Vec<BakedGiTierMemory>,
}

impl BakedGiMemoryReport {
    /// The tier named `name` ([`BudgetTier::name`]).
    #[must_use]
    pub fn tier(&self, name: &str) -> Option<&BakedGiTierMemory> {
        self.tiers.iter().find(|t| t.tier == name)
    }
}

/// Bytes per probe of `layout` in the renderer's buffer.
fn per_probe(layout: &VolumeLayout) -> u64 {
    let texels = u64::from(layout.moment_resolution) * u64::from(layout.moment_resolution);
    (META_RECORD_BYTES + GI_COMPONENTS * SH_RECORD_BYTES) as u64 + texels * MOMENT_TEXEL_BYTES as u64
}

/// A count after doubling the spacing: every other sample, keeping both ends where it can.
fn coarser(count: u32) -> u64 {
    u64::from(count.div_ceil(2))
}

fn brick_probes(b: &DetailBrick) -> u64 {
    b.probe_count() as u64
}

/// What one spacing change would save.
struct Saving {
    target: String,
    bytes: u64,
    after: u64,
    action: String,
}

/// Every terrain layer and brick of `layout`, with what doubling its spacing saves. `label` names
/// brick `b`.
fn savings(layout: &VolumeLayout, label: &dyn Fn(usize) -> String) -> Vec<Saving> {
    let probe = per_probe(layout);
    let mut out = Vec::with_capacity(layout.bricks.len() + 1);
    if let Some(t) = &layout.terrain {
        let levels = t.levels.len() as u64;
        let columns = u64::from(t.counts[0]) * u64::from(t.counts[1]);
        let after_columns = coarser(t.counts[0]) * coarser(t.counts[1]);
        out.push(Saving {
            target: "terrain".to_string(),
            bytes: columns * (levels * probe + 4),
            after: after_columns * (levels * probe + 4),
            action: format!(
                "double terrain_spacing ({} m to {} m)",
                t.spacing[0],
                2.0 * t.spacing[0]
            ),
        });
    }
    for (index, b) in layout.bricks.iter().enumerate() {
        let spacing = b.spacing.iter().copied().fold(f32::INFINITY, f32::min);
        let after: u64 = b.counts.iter().map(|c| coarser(*c)).product();
        out.push(Saving {
            target: label(index),
            bytes: brick_probes(b) * probe,
            after: after * probe,
            action: format!("double this brick's spacing ({spacing} m to {} m)", 2.0 * spacing),
        });
    }
    out
}

/// The tier answers for a set of `parts` (label, bytes) with `spacing` savings to offer and the
/// `split` answer for a part above one binding.
fn tier_answers(
    parts: &[(String, u64)],
    spacing: &[Saving],
    split: &dyn Fn(&str) -> Option<String>,
) -> Vec<BakedGiTierMemory> {
    let total: u64 = parts.iter().map(|(_, b)| *b).sum();
    BudgetTier::ALL
        .into_iter()
        .map(|tier| {
            let budget = baked_gi_budget(tier);
            let mut over = Vec::new();
            let mut fixes = Vec::new();
            let mut bytes_after = total;
            let mut suffice = true;
            // The binding first: a split does not change the total.
            for (label, bytes) in parts.iter().filter(|(_, b)| *b > PORTABLE_VOLUME_BYTES) {
                over.push(BakedGiMemoryOver {
                    code: CODE_OVER_BINDING,
                    target: label.clone(),
                    bytes: *bytes,
                    limit: PORTABLE_VOLUME_BYTES,
                });
                match split(label) {
                    Some(action) => fixes.push(BakedGiMemoryFix {
                        fix: "split",
                        target: label.clone(),
                        action,
                        bytes_after,
                    }),
                    None => suffice = false,
                }
            }
            if total > budget {
                over.push(BakedGiMemoryOver {
                    code: CODE_OVER_BUDGET,
                    target: "volume".to_string(),
                    bytes: total,
                    limit: budget,
                });
                let mut order: Vec<&Saving> = spacing.iter().filter(|s| s.after < s.bytes).collect();
                order.sort_by_key(|s| std::cmp::Reverse(s.bytes - s.after));
                for s in order.into_iter().take(MAX_FIXES) {
                    if bytes_after <= budget {
                        break;
                    }
                    bytes_after -= s.bytes - s.after;
                    fixes.push(BakedGiMemoryFix {
                        fix: "spacing",
                        target: s.target.clone(),
                        action: s.action.clone(),
                        bytes_after,
                    });
                }
                suffice = suffice && bytes_after <= budget;
            }
            BakedGiTierMemory {
                tier: tier.name(),
                vram_budget: MemoryBudget::for_tier(tier).vram,
                budget,
                bytes: total,
                budget_used: total as f32 / budget.max(1) as f32,
                fits: over.is_empty(),
                over,
                fixes,
                fixes_suffice: suffice,
            }
        })
        .collect()
}

fn contributors(spacing: &[Saving]) -> Vec<BakedGiMemoryContributor> {
    let mut list: Vec<BakedGiMemoryContributor> = spacing
        .iter()
        .map(|s| BakedGiMemoryContributor {
            target: s.target.clone(),
            bytes: s.bytes,
        })
        .collect();
    list.sort_by_key(|c| std::cmp::Reverse(c.bytes));
    list.truncate(MAX_CONTRIBUTORS);
    list
}

/// The words for a layout a split fits, or `None` when none does.
fn split_action(layout: &VolumeLayout) -> Option<String> {
    let parts = split_layout(layout, PORTABLE_VOLUME_BYTES).ok()?;
    let largest = parts.iter().map(|p| p.runtime_buffer_bytes()).max().unwrap_or(0);
    Some(format!(
        "rebake: `regia bake-gi` writes it as {} parts, the largest {:.1} MiB",
        parts.len(),
        largest as f64 / f64::from(1u32 << 20)
    ))
}

/// The answer for a layout about to be baked: the parts it would bake as (one when it fits one
/// binding, [`split_layout`] otherwise; a layout no split fits counts as one, over the binding, with
/// no split fix). `label` names brick `b` for the fixes.
#[must_use]
pub fn layout_report(layout: &VolumeLayout, label: &dyn Fn(usize) -> String) -> BakedGiMemoryReport {
    let whole = layout.runtime_buffer_bytes();
    let parts: Vec<(String, u64)> = if whole > PORTABLE_VOLUME_BYTES {
        match split_layout(layout, PORTABLE_VOLUME_BYTES) {
            Ok(parts) => parts
                .iter()
                .enumerate()
                .map(|(k, p)| (format!("part {k}"), p.runtime_buffer_bytes()))
                .collect(),
            Err(_) => vec![("volume".to_string(), whole)],
        }
    } else {
        vec![("volume".to_string(), whole)]
    };
    let spacing = savings(layout, label);
    // A layout a split fits is already below the binding part by part; one none fits has no answer.
    let no_split = |_: &str| -> Option<String> { None };
    build(parts, &spacing, &no_split)
}

/// The answer for volumes that are installed or on disk (`level.get_baked_gi`): each in its own
/// buffer, as a part or a blended layer. A volume above one binding gets the split answer of its
/// own layout, and the brick labels name it: `<volume> brick <b>`.
#[must_use]
pub fn volumes_report(volumes: &[(String, VolumeLayout)]) -> BakedGiMemoryReport {
    let parts: Vec<(String, u64)> = volumes
        .iter()
        .map(|(label, layout)| (label.clone(), layout.runtime_buffer_bytes()))
        .collect();
    let spacing: Vec<Saving> = volumes
        .iter()
        .flat_map(|(label, layout)| {
            savings(layout, &|b| format!("{label} brick {b}"))
                .into_iter()
                .map(|mut s| {
                    if s.target == "terrain" {
                        s.target = format!("{label} terrain");
                    }
                    s
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let split = |target: &str| {
        volumes
            .iter()
            .find(|(label, _)| label == target)
            .and_then(|(_, layout)| split_action(layout))
    };
    build(parts, &spacing, &split)
}

fn build(
    parts: Vec<(String, u64)>,
    spacing: &[Saving],
    split: &dyn Fn(&str) -> Option<String>,
) -> BakedGiMemoryReport {
    BakedGiMemoryReport {
        bytes: parts.iter().map(|(_, b)| *b).sum(),
        tiers: tier_answers(&parts, spacing, split),
        parts: parts.into_iter().map(|(_, b)| b).collect(),
        binding_limit: PORTABLE_VOLUME_BYTES,
        contributors: contributors(spacing),
    }
}

/// Most parts a bake writes ([`MAX_SPLIT_PARTS`]) at one portable binding each: the largest volume
/// that installs on every device.
#[must_use]
pub fn largest_portable_set() -> u64 {
    u64::from(MAX_SPLIT_PARTS) * PORTABLE_VOLUME_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::baked_gi::TerrainLayer;

    const MIB: u64 = 1 << 20;

    /// A terrain-only layout: `columns`² columns of 8 m with `levels` levels, 4×4 moments.
    fn terrain(columns: u32, levels: usize) -> VolumeLayout {
        VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0, 0.0],
                spacing: [8.0, 8.0],
                counts: [columns, columns],
                levels: (0..levels).map(|l| 1.0 + l as f32).collect(),
                ground: vec![0.0; (columns * columns) as usize],
            }),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 16.0,
        }
    }

    fn with_brick(mut layout: VolumeLayout, counts: [u32; 3]) -> VolumeLayout {
        layout.bricks.push(DetailBrick {
            origin: [0.0, 1.0, 0.0],
            spacing: [1.0; 3],
            counts,
            first_probe: 0,
            flags: 0,
        });
        layout.assign_brick_offsets();
        layout
    }

    fn label(b: usize) -> String {
        format!("brick:{b}")
    }

    #[test]
    fn the_shares_follow_the_tier_budgets() {
        assert_eq!(baked_gi_budget(BudgetTier::Minimum), 832 * MIB);
        assert_eq!(baked_gi_budget(BudgetTier::Deck), 384 * MIB);
        assert_eq!(baked_gi_budget(BudgetTier::DesktopHigh), 1280 * MIB);
        assert_eq!(largest_portable_set(), 512 * MIB);
    }

    /// A small volume fits every tier, one part, nothing over, no fix.
    #[test]
    fn a_small_volume_fits_every_tier() {
        let layout = terrain(64, 5);
        let report = layout_report(&layout, &label);
        assert_eq!(report.parts, [layout.runtime_buffer_bytes()]);
        assert_eq!(report.tiers.len(), BudgetTier::ALL.len());
        for tier in &report.tiers {
            assert!(tier.fits, "{tier:?}");
            assert!(tier.over.is_empty() && tier.fixes.is_empty() && tier.fixes_suffice);
            assert!(tier.bytes <= tier.budget);
        }
        assert_eq!(report.contributors[0].target, "terrain");
    }

    /// A layout above one binding that a split fits: its parts are what count, and they are within
    /// the binding, so nothing is over; the parts together are more than the whole (the overlap).
    #[test]
    fn a_layout_a_split_fits_is_within_the_binding() {
        let layout = terrain(600, 5);
        let whole = layout.runtime_buffer_bytes();
        assert!(whole > PORTABLE_VOLUME_BYTES && whole < 2 * PORTABLE_VOLUME_BYTES);
        let report = layout_report(&layout, &label);
        assert!(report.parts.len() >= 2, "{:?}", report.parts);
        assert!(report.parts.iter().all(|p| *p <= PORTABLE_VOLUME_BYTES));
        assert!(report.bytes > whole);
        // Every tier whose share holds the parts fits; the integrated-GPU floor's 192 MiB share
        // (ADR-8878) is below this layout's ~233 MiB, so only its budget is over, never the binding.
        for tier in &report.tiers {
            assert_eq!(tier.fits, tier.bytes <= tier.budget, "{tier:?}");
            assert!(tier.over.iter().all(|o| o.code != CODE_OVER_BINDING), "{tier:?}");
        }
        for name in ["minimum", "deck", "desktop_high"] {
            assert!(report.tier(name).is_some_and(|t| t.fits), "{name}");
        }
        assert!(report.tier("integrated_low").is_some_and(|t| !t.fits));
    }

    /// Over the Deck's share, not the minimum tier's: the Deck names the budget and a spacing fix
    /// on the largest contributor; applying that fix for real lands where the report said.
    #[test]
    fn a_volume_over_a_tier_names_the_limit_and_the_spacing_that_fixes_it() {
        // 5 levels × 800² columns × 136 B = 415 MiB: past the Deck's 384, within the minimum's 832,
        // and within four parts of one binding.
        let layout = terrain(800, 5);
        let report = layout_report(&layout, &label);
        let deck = report.tier("deck").expect("deck");
        assert!(!deck.fits);
        assert_eq!(deck.over.len(), 1, "{:?}", deck.over);
        assert_eq!(deck.over[0].code, CODE_OVER_BUDGET);
        assert_eq!(deck.over[0].limit, 384 * MIB);
        assert_eq!(deck.fixes.len(), 1);
        assert_eq!(deck.fixes[0].fix, "spacing");
        assert_eq!(deck.fixes[0].target, "terrain");
        assert!(
            deck.fixes[0].action.contains("terrain_spacing"),
            "{}",
            deck.fixes[0].action
        );
        assert!(deck.fixes_suffice);
        assert!(deck.fixes[0].bytes_after <= 384 * MIB);
        assert!(report.tier("minimum").is_some_and(|t| t.fits));
        assert!(report.tier("desktop_high").is_some_and(|t| t.fits));

        let mut fixed = terrain(400, 5);
        if let Some(t) = fixed.terrain.as_mut() {
            t.spacing = [16.0, 16.0];
        }
        let after = layout_report(&fixed, &label);
        assert!(after.tier("deck").is_some_and(|t| t.fits), "{after:?}");
        // The estimate lands near the layout the fix describes.
        let real = fixed.runtime_buffer_bytes();
        let said = deck.fixes[0].bytes_after;
        assert!(
            real.abs_diff(said) * 2 < report.bytes.abs_diff(real),
            "{said} vs the fixed layout's {real}"
        );
    }

    /// Past every tier: the fixes are the largest savings first, and the list says when it is not
    /// enough.
    #[test]
    fn fixes_come_largest_saving_first_and_say_when_they_run_out() {
        let layout = with_brick(with_brick(terrain(32, 2), [40, 40, 40]), [300, 300, 300]);
        let report = layout_report(&layout, &label);
        // brick 1 is 27 M probes (3.4 GiB), over every tier's share, past any split.
        for tier in &report.tiers {
            assert!(!tier.fits, "{tier:?}");
            assert!(tier.over.iter().any(|o| o.code == CODE_OVER_BUDGET));
            assert!(tier.over.iter().any(|o| o.code == CODE_OVER_BINDING));
            assert_eq!(tier.fixes[0].target, "brick:1", "{:?}", tier.fixes);
            assert!(
                tier.fixes
                    .windows(2)
                    .all(|w| w[1].bytes_after <= w[0].bytes_after)
            );
        }
        assert_eq!(report.contributors[0].target, "brick:1");
        let deck = report.tier("deck").expect("deck");
        assert!(
            !deck.fixes_suffice,
            "no split fits a brick past the binding: {deck:?}"
        );
    }

    /// A whole installed volume past the binding is told a rebake splits it, and that does not
    /// change the total; a tier the total fits has only that fix.
    #[test]
    fn an_installed_volume_past_the_binding_gets_the_split_fix() {
        let volumes = vec![
            ("map".to_string(), terrain(600, 5)),
            ("tile".to_string(), terrain(16, 5)),
        ];
        let report = volumes_report(&volumes);
        assert_eq!(report.parts.len(), 2);
        let tier = report.tier("minimum").expect("minimum");
        assert!(!tier.fits);
        assert_eq!(tier.over.len(), 1);
        assert_eq!(
            (tier.over[0].code, tier.over[0].target.as_str()),
            (CODE_OVER_BINDING, "map")
        );
        assert_eq!(tier.fixes.len(), 1);
        assert_eq!(tier.fixes[0].fix, "split");
        assert!(tier.fixes[0].action.contains("parts"), "{}", tier.fixes[0].action);
        assert_eq!(tier.fixes[0].bytes_after, report.bytes);
        assert!(tier.fixes_suffice);
    }

    /// The largest set that installs everywhere is past the Deck's share: four full parts do not
    /// answer every tier, which is why a report names both limits (ADR-3006).
    #[test]
    fn four_full_parts_are_past_the_decks_share() {
        assert!(largest_portable_set() > baked_gi_budget(BudgetTier::Deck));
        assert!(largest_portable_set() < baked_gi_budget(BudgetTier::Minimum));
    }
}
