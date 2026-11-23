// SPDX-License-Identifier: MPL-2.0
//! Lightmaps for every package tier from one bake (ADR-3120, ADR-3001 decisions 4 and 5).
//!
//! A game bakes once, at its highest tier's density (ULTRA, 16 texels per metre by default), and the
//! cook derives each lower tier's lightmap from that bake: the same unwrap (UV2 per mesh is kept bit
//! for bit unless the tier's charts are repacked, step 0), each entry's rectangle scaled down by the
//! tier's power-of-two factor, every texel the
//! area mean of the bake's texels **of its own chart** under its footprint, and the gutters filled
//! chart by chart (a gutter texel takes the values of one chart, never a mix of two). Then the
//! tier's encoding (RGB9E5, BC6H, ASTC 4×4 HDR through `regia_asset::hdr_blocks`) and, where the
//! tier says so, no direction section. Each output is a valid current-version `.flmap`.
//!
//! The steps of [`derive_lightmap_tier`]:
//!
//! 0. **Chart padding** (ADR-5125, [`derive_lightmap_tier_padded`]). When the bake's gutter over the
//!    factor leaves fewer whole texels per chart side than the tier's rule asks
//!    ([`LightmapTierRule::padding`]; unset, one texel, or two for a tier stored in a 4 × 4 block
//!    encoding, ADR-5621), every entry's charts are repacked at the
//!    tier's resolution with that padding: each chart keeps its size and moves by whole tier
//!    texels, so UV2 changes by a per-chart translation and every later step reads the bake through
//!    the inverse move (`chart_repack` module docs). From the default 2-texel bake padding at
//!    16 texels per metre, HIGH (factor two) keeps the bake's UV2 and LOW and ULTRA_LOW (factors
//!    four and eight) repack.
//! 1. **Chart maps.** Every entry's unwrap triangles are split into charts (connected through shared
//!    unwrapped vertices) and rasterized at the bake's and at the tier's resolution with the bake's
//!    own rule (a texel belongs to the first triangle that holds its centre). A chart too small to
//!    hold a texel centre at the tier gets the texel under its largest triangle's centroid when that
//!    texel is free, so no chart is left to read a neighbour's light.
//! 2. **Resample.** A tier texel of chart `c` is the mean of the baked texels of chart `c` whose
//!    centres lie under it; when there are none, the nearest baked texel of chart `c`; when chart
//!    `c` has no baked texel at all, the bake's own bilinear lookup at the texel centre. Directions
//!    are averaged weighted by luminance, as the bake accumulates them.
//! 3. **Gutters.** Empty texels inside an entry's rectangle are filled in waves from their filled
//!    neighbours, each from the one chart with the most filled neighbours (ties: the lower chart),
//!    so bilinear lookups near a chart edge read that chart's light. The fill reaches the whole
//!    rectangle: the gutter scales with the factor, and the tier pays nothing for it. The first
//!    ring continues the selected chart's baked gradient, bounded as in the baker (ADR-4930).
//! 4. **Placement.** Each rectangle goes to the bake's position divided by the factor; when two
//!    scaled rectangles would overlap (a position that is not a multiple of the factor), every
//!    rectangle is re-placed on shelves, which only changes `rect` and `scale_offset`.
//! 5. **Seams.** The `.flmap` has no mesh positions, so the stored seam figures (measured at bake
//!    time, ADR-2024) are carried over plus the step the downsample added on the same edges
//!    ([`regia_asset::lightmap::seam_report_by_source_vertex`] on the bake and on the tier).
//! 6. **Border fit** (ADR-4995). Area means move the light a tier lookup reads at a chart border
//!    inward, and a one-texel gutter between two charts holds only one side's continuation, so a
//!    seam the bake stitched opens at the tier. A least-squares fit moves the texels the tier's
//!    border lookups read until they read the bake's own border light, regularised toward the
//!    area means. A texel two charts read is shared only where the bake's two sides agree there;
//!    at a crease or an unstitched cut it keeps its one chart's light (`border_fit` module docs).
//!
//! Everything runs in a fixed order with no threads: the same bake and rule give the same bytes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapLayout, LightmapSeams, LightmapTexelEncoding, seam_report_by_source_vertex,
};

/// The reflect path of a cooked lightmap (the manifest's `type_path` of a `.flmap`).
pub use crate::cook::LIGHTMAP_TYPE_PATH;
use crate::lightmap_cook::{LightmapCookDirections, LightmapCookEncoding, encode_lightmap_directions};

/// The largest density factor a tier may take from the bake (16 texels per metre down to 1).
pub const MAX_TIER_FACTOR: u32 = 16;

/// The tier names of the default table (ADR-3001 decision 4), highest first.
pub const DEFAULT_TIERS: [&str; 4] = ["ultra", "high", "low", "ultra_low"];

/// One tier's lightmap rule (`[lightmaps.tiers.<name>]` in `Regia.toml`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LightmapTierRule {
    /// The tier ships no lightmap (the probe volume lights it).
    pub none: bool,
    /// Texels per metre of the tier's lightmap. The factor from the bake is the bake's density over
    /// this, a power of two; a tier at or above the bake's density takes the bake as it is.
    pub density: Option<f32>,
    /// Or the factor itself (a power of two, 1..=16); `density` wins when both are set.
    pub density_factor: Option<u32>,
    /// How the tier stores its texels; `None` keeps the cooked lightmap's own (its `.meta` settings).
    pub encoding: Option<LightmapCookEncoding>,
    /// Whether the tier keeps the direction section (when the bake has one).
    pub directional: bool,
    /// How a kept direction section is stored; `None` keeps the cooked lightmap's own.
    pub directions: Option<LightmapCookDirections>,
    /// The most bytes a map's lightmap may take at this tier (its load set and memory budget, ADR-3001
    /// decision 5), MiB. A lightmap over it ships without its texels and the cook says so.
    pub budget_mib: Option<f64>,
    /// Gutter texels every chart needs on each side at this tier (1..=16, ADR-5125). When the
    /// bake's padding over the factor leaves fewer whole tier texels, the tier repacks each entry's
    /// charts at its own resolution with this padding: each chart keeps its size and moves by whole
    /// tier texels, so UV2 changes by a per-chart translation and the tier ships its own unwrap.
    /// `None` (unset, the default) takes the padding from the encoding the tier resolves to on the
    /// cook's platform ([`LightmapTierRule::chart_padding`], ADR-5621):
    /// [`BLOCK_TIER_CHART_PADDING`] for a 4 × 4 block encoding (BC6H, ASTC HDR), else
    /// [`DEFAULT_TIER_CHART_PADDING`]. To keep the bake's layout bit for bit, derive with
    /// [`derive_lightmap_tier`].
    pub padding: Option<u32>,
}

/// The gutter texels per chart side a tier keeps by default ([`LightmapTierRule::padding`]): one
/// whole texel, what a derived tier with the bake's layout keeps at factor two from the baker's
/// default padding of two (ADR-5125).
pub const DEFAULT_TIER_CHART_PADDING: u32 = 1;

/// The chart padding of the default table's ULTRA_LOW, the tier that ships a 4 × 4 block encoding
/// (BC6H, ASTC HDR): two texels, so no block holds texels of two charts' borders. With one, the
/// block's shared endpoints spread one chart's border light onto the other's gutter (the gallery's
/// Surface seam 0.253 in BC6H against 0.002 uncompressed; 0.014 at two, ADR-5125).
pub const BLOCK_TIER_CHART_PADDING: u32 = 2;

/// The largest chart padding a tier rule may ask for, texels per side ([`LightmapTierRule::padding`]).
pub const MAX_TIER_CHART_PADDING: u32 = 16;

impl Default for LightmapTierRule {
    fn default() -> Self {
        Self {
            none: false,
            density: None,
            density_factor: None,
            encoding: None,
            directional: true,
            directions: None,
            budget_mib: None,
            padding: None,
        }
    }
}

impl LightmapTierRule {
    /// The factor this rule takes from a bake at `bake_density` texels per metre.
    ///
    /// # Errors
    ///
    /// `lightmap_tier_rule:` when the factor is not a power of two in `1..=MAX_TIER_FACTOR`.
    pub fn factor_for(&self, bake_density: f32) -> Result<u32, String> {
        let factor = match (self.density, self.density_factor) {
            (Some(d), _) => {
                if !(d.is_finite() && d > 0.0) {
                    return Err(format!(
                        "lightmap_tier_rule: density {d} is not a positive number"
                    ));
                }
                let ratio = bake_density / d;
                if ratio <= 1.0 + 1e-4 {
                    1
                } else {
                    let f = ratio.round();
                    if (ratio - f).abs() > 1e-3 * ratio || f > MAX_TIER_FACTOR as f32 {
                        return Err(format!(
                            "lightmap_tier_rule: density {d} is not the bake's {bake_density} over a power of two up to {MAX_TIER_FACTOR}"
                        ));
                    }
                    f as u32
                }
            }
            (None, Some(f)) => f,
            (None, None) => 1,
        };
        if factor == 0 || !factor.is_power_of_two() || factor > MAX_TIER_FACTOR {
            return Err(format!(
                "lightmap_tier_rule: density factor {factor} is not a power of two in 1..={MAX_TIER_FACTOR}"
            ));
        }
        Ok(factor)
    }

    /// Checks the rule's own values that do not depend on the bake: its chart `padding` must be
    /// `1..=16` texels when set (ADR-5621). [`LightmapTierTable::with_overrides`] and
    /// [`cook_lightmap_tier`] call it, the cook before it looks at `none`.
    ///
    /// # Errors
    ///
    /// `lightmap_tier_rule:` naming the padding.
    pub fn validate(&self) -> Result<(), String> {
        match self.padding {
            Some(p) if !(1..=MAX_TIER_CHART_PADDING).contains(&p) => Err(format!(
                "lightmap_tier_rule: padding {p} must be 1..={MAX_TIER_CHART_PADDING} texels"
            )),
            _ => Ok(()),
        }
    }

    /// The chart padding this rule gives a tier stored in `encoding` (the encoding it resolves to
    /// on the cook's platform): the rule's `padding` when set, else [`BLOCK_TIER_CHART_PADDING`]
    /// for a 4 × 4 block encoding (BC6H, ASTC HDR), whose blocks would otherwise hold two charts'
    /// borders, and [`DEFAULT_TIER_CHART_PADDING`] for RGB9E5 (ADR-5621).
    #[must_use]
    pub fn chart_padding(&self, encoding: LightmapTexelEncoding) -> u32 {
        self.padding.unwrap_or(if encoding.block_format().is_some() {
            BLOCK_TIER_CHART_PADDING
        } else {
            DEFAULT_TIER_CHART_PADDING
        })
    }

    /// The budget in bytes, `None` without one.
    #[must_use]
    pub fn budget_bytes(&self) -> Option<u64> {
        self.budget_mib
            .filter(|m| m.is_finite() && *m >= 0.0)
            .map(|m| (m * 1024.0 * 1024.0) as u64)
    }
}

/// The tier table: the defaults of ADR-3001 with the project's rules over them.
#[derive(Clone, Debug, PartialEq)]
pub struct LightmapTierTable {
    /// Every tier by name.
    pub tiers: BTreeMap<String, LightmapTierRule>,
}

impl Default for LightmapTierTable {
    /// ADR-3001's table: ULTRA 16 texels per metre, HIGH 8, LOW 4, ULTRA_LOW 2 with no direction
    /// section in a block encoding (`auto`: BC6H on desktop, ASTC HDR on Android). Budgets are 1/32 of
    /// each tier's VRAM (ADR-3120): 320 MiB (ULTRA, 10 GiB), 208 MiB (HIGH, 6.5 GiB), 96 MiB (LOW,
    /// 3 GiB) and 48 MiB (ULTRA_LOW, 1.5 GiB). Chart padding [`DEFAULT_TIER_CHART_PADDING`], and
    /// [`BLOCK_TIER_CHART_PADDING`] on the block-encoded ULTRA_LOW (ADR-5125).
    fn default() -> Self {
        let rule = |density: f32, budget: f64| LightmapTierRule {
            density: Some(density),
            budget_mib: Some(budget),
            padding: Some(DEFAULT_TIER_CHART_PADDING),
            ..LightmapTierRule::default()
        };
        let mut tiers = BTreeMap::new();
        tiers.insert("ultra".to_string(), rule(16.0, 320.0));
        tiers.insert("high".to_string(), rule(8.0, 208.0));
        tiers.insert("low".to_string(), rule(4.0, 96.0));
        tiers.insert(
            "ultra_low".to_string(),
            LightmapTierRule {
                encoding: Some(LightmapCookEncoding::Auto),
                directional: false,
                padding: Some(BLOCK_TIER_CHART_PADDING),
                ..rule(2.0, 48.0)
            },
        );
        Self { tiers }
    }
}

impl LightmapTierTable {
    /// The default table with `overrides` replacing or adding tiers by name.
    ///
    /// # Errors
    ///
    /// `lightmap_tier_rule:` for a tier name that is not lowercase ASCII letters, digits and `_`, or
    /// a rule that does not validate ([`LightmapTierRule::validate`]).
    pub fn with_overrides(overrides: &BTreeMap<String, LightmapTierRule>) -> Result<Self, String> {
        let mut table = Self::default();
        for (name, rule) in overrides {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            {
                return Err(format!(
                    "lightmap_tier_rule: tier name {name:?} must be lowercase letters, digits and _"
                ));
            }
            rule.validate().map_err(|e| format!("{e} (tier {name})"))?;
            table.tiers.insert(name.clone(), rule.clone());
        }
        Ok(table)
    }

    /// The rule of tier `name`.
    ///
    /// # Errors
    ///
    /// `lightmap_tier_unknown:` naming the known tiers.
    pub fn rule(&self, name: &str) -> Result<&LightmapTierRule, String> {
        self.tiers.get(name).ok_or_else(|| {
            format!(
                "lightmap_tier_unknown: no lightmap tier `{name}` (known: {})",
                self.tiers.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }
}

/// What [`derive_lightmap_tier`] did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TierDeriveStats {
    /// The density factor.
    pub factor: u32,
    /// Tier atlas size, texels.
    pub width: u32,
    /// Tier atlas height, texels.
    pub height: u32,
    /// True when the scaled rectangles overlapped at the bake's positions and were re-placed.
    pub repacked: bool,
    /// Tier texels averaged from their own chart's baked texels.
    pub resampled: u64,
    /// Tier texels with no baked texel of their chart under them: the nearest one.
    pub nearest: u64,
    /// Tier texels of a chart with no baked texel at all: the bake's bilinear lookup.
    pub looked_up: u64,
    /// Charts too small to hold a texel centre that were given the texel under their centroid.
    pub conservative_charts: u64,
    /// Charts with no texel of their own at the tier (they read the texel of a neighbour).
    pub shared_charts: u64,
    /// Gutter texels filled chart by chart.
    pub dilated: u64,
    /// The bake's gutter over the factor, texels at the tier.
    pub gutter_texels: f32,
    /// Chart-border sample rows the border fit solved (ADR-4995).
    pub border_fit_rows: u64,
    /// Texels the border fit changed.
    pub border_fit_texels: u64,
    /// Texels the border fit kept as they were: those two charts read where the bake's light on the
    /// two sides disagrees (a crease or an unstitched cut), and the on-chart texels it holds on a
    /// steep border (`border_steep`, ADR-5621). Each texel counted once.
    pub border_frozen: u64,
    /// Border samples where the bake's light changes faster than the tier can hold (a contact band
    /// at a joint, a shadow edge on the border): the fit holds their texels on the chart and fits
    /// only the gutter texels they read (ADR-5125).
    pub border_steep: u64,
    /// Charts repacked at the tier's own resolution because the bake's gutter over the factor left
    /// fewer whole texels than the rule's padding (ADR-5125); 0 when the tier keeps the bake's UV2.
    pub repacked_charts: u64,
    /// Gutter texels per chart side at the tier: the rule's padding when its charts were
    /// repacked, else the bake's padding over the factor in whole texels.
    pub chart_padding: u32,
}

/// One unwrap's chart per triangle (connected components over unwrapped vertices, numbered by first
/// triangle) and the chart count.
fn triangle_charts(indices: &[u32], vertices: usize) -> (Vec<u32>, u32) {
    fn find(parent: &mut [u32], mut v: u32) -> u32 {
        while parent[v as usize] != v {
            let up = parent[parent[v as usize] as usize];
            parent[v as usize] = up;
            v = up;
        }
        v
    }
    let mut parent: Vec<u32> = (0..vertices as u32).collect();
    let tris = indices.as_chunks::<3>().0;
    for t in tris {
        if t.iter().any(|&v| v as usize >= vertices) {
            continue;
        }
        let root = find(&mut parent, t[0]);
        for &v in &t[1..] {
            let other = find(&mut parent, v);
            parent[other as usize] = root;
        }
    }
    let mut numbers: BTreeMap<u32, u32> = BTreeMap::new();
    let charts = tris
        .iter()
        .map(|t| {
            if t.iter().any(|&v| v as usize >= vertices) {
                return 0;
            }
            let root = find(&mut parent, t[0]);
            let next = numbers.len() as u32;
            *numbers.entry(root).or_insert(next)
        })
        .collect();
    (charts, numbers.len() as u32)
}

fn edge(a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

/// Rasterizes an unwrap at `w × h` with the bake's rule (a texel belongs to the first triangle that
/// holds its centre): the chart of each texel, `u32::MAX` for none.
fn chart_grid(uv: &[[f32; 2]], indices: &[u32], tri_chart: &[u32], w: u32, h: u32) -> Vec<u32> {
    let (w, h) = (w as usize, h as usize);
    let mut grid = vec![u32::MAX; w * h];
    for (k, t) in indices.as_chunks::<3>().0.iter().enumerate() {
        let Some(p) = triangle_uv(uv, t, w, h) else {
            continue;
        };
        let area = edge(p[0], p[1], p[2]);
        if area.abs() < 1e-12 {
            continue;
        }
        let lo = |c: usize| {
            p.iter()
                .map(|q| q[c])
                .fold(f32::INFINITY, f32::min)
                .floor()
                .max(0.0) as usize
        };
        let hi = |c: usize, n: usize| {
            (p.iter()
                .map(|q| q[c])
                .fold(f32::NEG_INFINITY, f32::max)
                .ceil()
                .max(0.0) as usize)
                .min(n)
        };
        for y in lo(1)..hi(1, h) {
            for x in lo(0)..hi(0, w) {
                let c = [x as f32 + 0.5, y as f32 + 0.5];
                let b0 = edge(p[1], p[2], c) / area;
                let b1 = edge(p[2], p[0], c) / area;
                let b2 = 1.0 - b0 - b1;
                let tol = -1e-5;
                if b0 < tol || b1 < tol || b2 < tol {
                    continue;
                }
                let slot = &mut grid[y * w + x];
                if *slot == u32::MAX {
                    *slot = tri_chart[k];
                }
            }
        }
    }
    grid
}

/// A triangle's UVs in texels of a `w × h` rectangle; `None` for an index past the unwrap.
fn triangle_uv(uv: &[[f32; 2]], t: &[u32; 3], w: usize, h: usize) -> Option<[[f32; 2]; 3]> {
    let mut out = [[0.0f32; 2]; 3];
    for (o, &i) in out.iter_mut().zip(t) {
        let q = uv.get(i as usize)?;
        *o = [q[0] * w as f32, q[1] * h as f32];
    }
    Some(out)
}

/// The point of the triangles of `tris` that `on_chart` takes (by triangle index) nearest `p`, in
/// the unwrap's local UV, distances measured in texels of a `scale` rectangle: `p` itself when a
/// triangle holds it (or none is taken).
fn nearest_on_chart(
    uv: &[[f32; 2]],
    tris: &[[u32; 3]],
    on_chart: impl Fn(usize) -> bool,
    p: [f32; 2],
    scale: [f32; 2],
) -> [f32; 2] {
    let q = [p[0] * scale[0], p[1] * scale[1]];
    let mut best: Option<(f32, [f32; 2])> = None;
    for (k, t) in tris.iter().enumerate() {
        if !on_chart(k) {
            continue;
        }
        let Some(v) = triangle_uv(uv, t, 1, 1) else {
            continue;
        };
        let v = v.map(|a| [a[0] * scale[0], a[1] * scale[1]]);
        let area = edge(v[0], v[1], v[2]);
        if area.abs() > 1e-12 {
            let (b0, b1) = (edge(v[1], v[2], q) / area, edge(v[2], v[0], q) / area);
            if b0 >= 0.0 && b1 >= 0.0 && b0 + b1 <= 1.0 {
                return p;
            }
        }
        for (a, b) in [(v[0], v[1]), (v[1], v[2]), (v[2], v[0])] {
            let d = [b[0] - a[0], b[1] - a[1]];
            let len = d[0] * d[0] + d[1] * d[1];
            let t = if len > 0.0 {
                (((q[0] - a[0]) * d[0] + (q[1] - a[1]) * d[1]) / len).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let c = [a[0] + t * d[0], a[1] + t * d[1]];
            let dist = (c[0] - q[0]).powi(2) + (c[1] - q[1]).powi(2);
            if best.is_none_or(|(bd, _)| dist < bd) {
                best = Some((dist, c));
            }
        }
    }
    best.map_or(p, |(_, c)| [c[0] / scale[0], c[1] / scale[1]])
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The tier rectangles: each entry stays on its bake page (ADR-3300), at the bake's position over
/// the factor, or on shelves when those overlap on its page. The tier page is the largest page any
/// of them needs.
fn place(src: &Lightmap, sizes: &[[u32; 2]], factor: u32) -> ([u32; 2], Vec<[u32; 2]>, bool) {
    let mut at = vec![[0u32; 2]; sizes.len()];
    let (mut w, mut h, mut repacked) = (0u32, 0u32, false);
    for page in 0..src.pages() {
        let members: Vec<usize> = (0..src.entries.len())
            .filter(|&k| src.entries[k].page == page)
            .collect();
        let ([pw, ph], placed, shelved) = place_page(src, &members, sizes, factor);
        for (&k, a) in members.iter().zip(placed) {
            at[k] = a;
        }
        w = w.max(pw);
        h = h.max(ph);
        repacked |= shelved;
    }
    ([w, h], at, repacked)
}

/// [`place`] for the entries `members` of one page, in their order.
fn place_page(
    src: &Lightmap,
    members: &[usize],
    all_sizes: &[[u32; 2]],
    factor: u32,
) -> ([u32; 2], Vec<[u32; 2]>, bool) {
    let round4 = |v: u32| v.max(4).div_ceil(4) * 4;
    let sizes: Vec<[u32; 2]> = members.iter().map(|&k| all_sizes[k]).collect();
    let sizes = sizes.as_slice();
    let mut at: Vec<[u32; 2]> = members
        .iter()
        .map(|&k| {
            let e = &src.entries[k];
            [e.rect[0] / factor, e.rect[1] / factor]
        })
        .collect();
    let mut w = src.width.div_ceil(factor);
    let mut h = src.height.div_ceil(factor);
    for (p, s) in at.iter().zip(sizes) {
        w = w.max(p[0] + s[0]);
        h = h.max(p[1] + s[1]);
    }
    let (w, h) = (round4(w), round4(h));
    let mut owner = vec![false; w as usize * h as usize];
    let mut overlap = false;
    'entries: for (p, s) in at.iter().zip(sizes) {
        for y in p[1]..p[1] + s[1] {
            for x in p[0]..p[0] + s[0] {
                let slot = &mut owner[(y * w + x) as usize];
                if *slot {
                    overlap = true;
                    break 'entries;
                }
                *slot = true;
            }
        }
    }
    if !overlap {
        return ([w, h], at, false);
    }
    // Shelves, tallest first (ties: wider, then entry order), in the bake's width over the factor.
    let width = round4(
        src.width
            .div_ceil(factor)
            .max(sizes.iter().map(|s| s[0]).max().unwrap_or(1)),
    );
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by(|&a, &b| {
        sizes[b][1]
            .cmp(&sizes[a][1])
            .then(sizes[b][0].cmp(&sizes[a][0]))
            .then(a.cmp(&b))
    });
    let (mut x, mut y, mut shelf) = (0u32, 0u32, 0u32);
    for k in order {
        let [sw, sh] = sizes[k];
        if x + sw > width {
            x = 0;
            y += shelf;
            shelf = 0;
        }
        at[k] = [x, y];
        x += sw;
        shelf = shelf.max(sh);
    }
    ([width, round4(y + shelf)], at, true)
}

/// Derives the lightmap of a tier `factor` times coarser than `src` (module docs), keeping the
/// bake's layout (UV2 bit for bit): [`derive_lightmap_tier_padded`] with no padding rule. Factor 1
/// returns the bake unchanged. The result's texels are RGB9E5 (encode them with
/// [`Lightmap::encode_texels`]); its directions, when `src` has them, are RGBA8.
///
/// # Errors
///
/// `lightmap_tier_derive:` for a factor that is not a power of two in `1..=MAX_TIER_FACTOR`, a tier
/// atlas past the format's limit, or a result that does not validate.
pub fn derive_lightmap_tier(src: &Lightmap, factor: u32) -> Result<(Lightmap, TierDeriveStats), String> {
    derive_lightmap_tier_padded(src, factor, None)
}

/// [`derive_lightmap_tier`] with the tier's chart padding ([`LightmapTierRule::padding`],
/// ADR-5125): when the bake's padding over `factor` leaves fewer than `padding` whole gutter texels
/// per chart side, every entry's charts are repacked at the tier's resolution with `padding`
/// gutter texels (each chart moved by whole texels, its UV2 translated; the tier's unwrap is then
/// its own). `None` keeps the bake's layout.
///
/// # Errors
///
/// As [`derive_lightmap_tier`], and `lightmap_tier_rule:` for a padding outside
/// `1..=MAX_TIER_CHART_PADDING`.
pub fn derive_lightmap_tier_padded(
    src: &Lightmap,
    factor: u32,
    padding: Option<u32>,
) -> Result<(Lightmap, TierDeriveStats), String> {
    let _span = tracing::info_span!("derive_lightmap_tier", factor, w = src.width, h = src.height).entered();
    if factor == 0 || !factor.is_power_of_two() || factor > MAX_TIER_FACTOR {
        return Err(format!(
            "lightmap_tier_derive: factor {factor} is not a power of two in 1..={MAX_TIER_FACTOR}"
        ));
    }
    // The rule's own check (`LightmapTierRule::validate`) runs when the table is built and before
    // the cook; this guards direct callers of this function.
    if let Some(p) = padding
        && !(1..=MAX_TIER_CHART_PADDING).contains(&p)
    {
        return Err(format!(
            "lightmap_tier_rule: padding {p} must be 1..={MAX_TIER_CHART_PADDING} texels"
        ));
    }
    let mut stats = TierDeriveStats {
        factor,
        width: src.width,
        height: src.height,
        gutter_texels: src.padding as f32 / factor as f32,
        chart_padding: src.padding / factor,
        ..TierDeriveStats::default()
    };
    if factor == 1 {
        return Ok((src.clone(), stats));
    }
    // Each entry's size at the tier with the bake's layout ("native"), and its charts.
    let native: Vec<[u32; 2]> = src
        .entries
        .iter()
        .map(|e| {
            [
                e.rect[2].div_ceil(factor).max(1),
                e.rect[3].div_ceil(factor).max(1),
            ]
        })
        .collect();
    let charts_of: Vec<(Vec<u32>, u32)> = src
        .unwraps
        .iter()
        .map(|u| triangle_charts(&u.indices, u.uv.len()))
        .collect();
    let repack_padding = padding.filter(|&p| src.padding / factor < p);
    let repacks: Vec<Option<chart_repack::EntryRepack>> = src
        .unwraps
        .iter()
        .zip(&charts_of)
        .zip(&native)
        .zip(&src.entries)
        .map(|(((u, (tri_chart, charts)), &n), entry)| {
            let baked = [entry.rect[2], entry.rect[3]];
            repack_padding.map(|p| chart_repack::repack_entry(u, tri_chart, *charts, n, baked, p))
        })
        .collect();
    if let Some(p) = repack_padding {
        stats.chart_padding = p;
        stats.repacked_charts = repacks.iter().flatten().map(|r| r.charts).sum();
    }
    let sizes: Vec<[u32; 2]> = repacks
        .iter()
        .zip(&native)
        .map(|(r, &n)| r.as_ref().map_or(n, |r| r.size))
        .collect();
    let ([tw_atlas, th_atlas], at, repacked) = place(src, &sizes, factor);
    if tw_atlas > regia_asset::lightmap::MAX_LIGHTMAP_SIZE
        || th_atlas > regia_asset::lightmap::MAX_LIGHTMAP_SIZE
    {
        return Err(format!(
            "lightmap_tier_derive: a {tw_atlas}×{th_atlas} tier atlas is past the format's limit"
        ));
    }
    stats.width = tw_atlas;
    stats.height = th_atlas;
    stats.repacked = repacked;
    let directional = src.is_directional();
    // Page-major, as the bake (ADR-3300): page `p`'s rows start `p × th_atlas` rows down.
    let n = tw_atlas as usize * th_atlas as usize * src.pages() as usize;
    let mut irr = vec![[0.0f32; 3]; n];
    let mut dirs = vec![[0.0f32; 4]; if directional { n } else { 0 }];
    let mut cov = vec![Coverage::EMPTY; n];
    let mut entries = src.entries.clone();
    // The global chart of every tier texel and of every unwrap triangle, for the border fit.
    let mut owner = vec![u32::MAX; n];
    let mut tri_charts: Vec<Vec<u32>> = Vec::with_capacity(src.entries.len());
    let mut chart_base = 0u32;
    for (e, (entry, unwrap)) in src.entries.iter().zip(&src.unwraps).enumerate() {
        let [rx, ry0, rw, rh] = entry.rect;
        // Stacked rows of the entry's page in the bake and in the tier.
        let ry = ry0 + entry.page * src.height;
        let [tw, th] = sizes[e];
        let [nw, nh] = native[e];
        let [qx, qy0] = at[e];
        let qy = qy0 + entry.page * th_atlas;
        let (tri_chart, charts) = (&charts_of[e].0, charts_of[e].1);
        // The tier's own unwrap (the bake's unless its charts were repacked) and, per chart, the
        // whole-texel move from the bake's layout scaled down to the tier's.
        let tier_unwrap = repacks[e].as_ref().map_or(unwrap, |r| &r.unwrap);
        let shift = |c: u32| {
            repacks[e]
                .as_ref()
                .and_then(|r| r.shift.get(c as usize).copied())
                .unwrap_or([0, 0])
        };
        let src_grid = chart_grid(&unwrap.uv, &unwrap.indices, tri_chart, rw, rh);
        let mut grid = chart_grid(&tier_unwrap.uv, &tier_unwrap.indices, tri_chart, tw, th);
        // Charts with no texel centre at the tier: the texel under their largest triangle's centroid.
        let mut has_texel = vec![false; charts as usize];
        for &c in &grid {
            if let Some(slot) = has_texel.get_mut(c as usize) {
                *slot = true;
            }
        }
        let tris = tier_unwrap.indices.as_chunks::<3>().0;
        let mut largest: Vec<Option<(f32, usize)>> = vec![None; charts as usize];
        for (k, t) in tris.iter().enumerate() {
            let Some(p) = triangle_uv(&tier_unwrap.uv, t, tw as usize, th as usize) else {
                continue;
            };
            let area = edge(p[0], p[1], p[2]).abs();
            let slot = &mut largest[tri_chart[k] as usize];
            if area > 0.0 && slot.is_none_or(|(a, _)| area > a) {
                *slot = Some((area, k));
            }
        }
        for (c, best) in largest.iter().enumerate() {
            if has_texel[c] {
                continue;
            }
            let Some((_, k)) = best else {
                continue;
            };
            let Some(p) = triangle_uv(&tier_unwrap.uv, &tris[*k], tw as usize, th as usize) else {
                continue;
            };
            let cx = ((p[0][0] + p[1][0] + p[2][0]) / 3.0)
                .floor()
                .clamp(0.0, (tw - 1) as f32) as usize;
            let cy = ((p[0][1] + p[1][1] + p[2][1]) / 3.0)
                .floor()
                .clamp(0.0, (th - 1) as f32) as usize;
            let slot = &mut grid[cy * tw as usize + cx];
            if *slot == u32::MAX {
                *slot = c as u32;
                stats.conservative_charts += 1;
            } else {
                stats.shared_charts += 1;
            }
        }
        // The baked texels of each chart, for the nearest-texel fallback.
        let mut chart_texels: Vec<Vec<u32>> = vec![Vec::new(); charts as usize];
        for y in 0..rh {
            for x in 0..rw {
                let c = src_grid[(y * rw + x) as usize];
                if c != u32::MAX && src.coverage_at(rx + x, ry + y) == Coverage::BAKED {
                    chart_texels[c as usize].push(y * rw + x);
                }
            }
        }
        let (sx, sy) = (rw as f32 / nw as f32, rh as f32 / nh as f32);
        // Source texel centres `i + 0.5` inside `[a, b)`.
        let span = |a: f32, b: f32, n: u32| {
            let lo = (a - 0.5).ceil().max(0.0) as u32;
            let hi = ((b - 0.5).ceil().max(0.0) as u32).min(n);
            lo..hi
        };
        let mut chart_of = vec![u32::MAX; (tw * th) as usize];
        for ty in 0..th {
            for tx in 0..tw {
                let c = grid[(ty * tw + tx) as usize];
                if c == u32::MAX {
                    continue;
                }
                let t = (qy + ty) as usize * tw_atlas as usize + (qx + tx) as usize;
                // The texel's place in the bake's layout at the tier's scale.
                let [mx, my] = shift(c);
                let (ux, uy) = ((i64::from(tx) - mx) as f32, (i64::from(ty) - my) as f32);
                let (mut sum, mut dsum, mut count, mut wsum) = ([0.0f32; 3], [0.0f32; 4], 0u32, 0.0f32);
                let mut add = |x: u32, y: u32, sum: &mut [f32; 3], dsum: &mut [f32; 4]| {
                    let v = src.irradiance_at(rx + x, ry + y);
                    for (s, v) in sum.iter_mut().zip(v) {
                        *s += v;
                    }
                    if let Some(d) = src.direction_at(rx + x, ry + y) {
                        let wgt = luminance(v).max(1e-6);
                        for (s, d) in dsum.iter_mut().zip(d) {
                            *s += wgt * d;
                        }
                        wsum += wgt;
                    }
                };
                for y in span(uy * sy, (uy + 1.0) * sy, rh) {
                    for x in span(ux * sx, (ux + 1.0) * sx, rw) {
                        if src_grid[(y * rw + x) as usize] == c
                            && src.coverage_at(rx + x, ry + y) == Coverage::BAKED
                        {
                            add(x, y, &mut sum, &mut dsum);
                            count += 1;
                        }
                    }
                }
                if count > 0 {
                    stats.resampled += 1;
                } else if let Some(&best) = {
                    let (cx, cy) = ((ux + 0.5) * sx, (uy + 0.5) * sy);
                    chart_texels[c as usize].iter().min_by(|&&a, &&b| {
                        let d = |i: u32| {
                            let (x, y) = ((i % rw) as f32 + 0.5 - cx, (i / rw) as f32 + 0.5 - cy);
                            x * x + y * y
                        };
                        d(a).total_cmp(&d(b)).then(a.cmp(&b))
                    })
                } {
                    add(best % rw, best / rw, &mut sum, &mut dsum);
                    count = 1;
                    stats.nearest += 1;
                } else {
                    // A chart the bake gave no texel of its own: what the bake's lookup reads on
                    // the chart's own surface nearest the texel centre (ADR-5125). A sliver's texel
                    // centre can lie off the chart, where the bake may hold no light at all.
                    let local = nearest_on_chart(
                        &unwrap.uv,
                        unwrap.indices.as_chunks::<3>().0,
                        |k| tri_chart[k] == c,
                        [(ux + 0.5) / nw as f32, (uy + 0.5) / nh as f32],
                        [nw as f32, nh as f32],
                    );
                    sum = src.sample_entry(e, local);
                    let (x, y) = (
                        ((local[0] * rw as f32) as u32).min(rw - 1),
                        ((local[1] * rh as f32) as u32).min(rh - 1),
                    );
                    if let Some(d) = src.direction_at(rx + x, ry + y) {
                        dsum = d;
                        wsum = 1.0;
                    }
                    count = 1;
                    stats.looked_up += 1;
                }
                irr[t] = sum.map(|s| s / count as f32);
                if directional && wsum > 0.0 {
                    dirs[t] = dsum.map(|s| s / wsum);
                }
                cov[t] = Coverage::BAKED;
                chart_of[(ty * tw + tx) as usize] = c;
            }
        }
        stats.dilated += fill_gutters(
            &mut irr,
            &mut dirs,
            &mut cov,
            &mut chart_of,
            [qx, qy, tw, th],
            tw_atlas,
        );
        for ty in 0..th {
            for tx in 0..tw {
                let c = chart_of[(ty * tw + tx) as usize];
                if c != u32::MAX {
                    owner[(qy + ty) as usize * tw_atlas as usize + (qx + tx) as usize] = chart_base + c;
                }
            }
        }
        tri_charts.push(tri_chart.iter().map(|&c| chart_base + c).collect());
        chart_base += charts;
        let out = &mut entries[e];
        out.rect = [qx, qy0, tw, th];
        out.scale_offset = [
            tw as f32 / tw_atlas as f32,
            th as f32 / th_atlas as f32,
            qx as f32 / tw_atlas as f32,
            qy0 as f32 / th_atlas as f32,
        ];
        out.texels_per_metre = entry.texels_per_metre * nw as f32 / rw as f32;
    }
    let unwraps: Vec<regia_asset::lightmap::MeshUnwrap> = repacks
        .into_iter()
        .zip(&src.unwraps)
        .map(|(r, u)| r.map_or_else(|| u.clone(), |r| r.unwrap))
        .collect();
    let fit = border_fit::fit_borders(
        src,
        &entries,
        &unwraps,
        &native,
        &tri_charts,
        &mut border_fit::TierTexels {
            irr: &mut irr,
            cov: &cov,
            owner: &owner,
            width: tw_atlas,
            height: th_atlas,
        },
    );
    stats.border_fit_rows = fit.rows;
    stats.border_fit_texels = fit.texels;
    stats.border_frozen = fit.frozen;
    stats.border_steep = fit.steep;
    let layout = LightmapLayout {
        width: tw_atlas,
        height: th_atlas,
        density: src.density / factor as f32,
        padding: stats.chart_padding,
        entries,
        unwraps,
    };
    let mut map = Lightmap::from_irradiance(layout, &irr, cov, src.bake)
        .map_err(|e| format!("lightmap_tier_derive: {}: {e}", e.code()))?;
    if directional {
        map.set_directions(&dirs)
            .map_err(|e| format!("lightmap_tier_derive: {}: {e}", e.code()))?;
    }
    map.meshlets = src.meshlets.clone();
    map.denoise = src.denoise;
    // The tier is derived from the same source meshes (a repack moves only the tier's own UV2,
    // which it ships in `unwraps`), so the mesh id the bake stored still names them. An empty list
    // means "no hash"; a list that does not match the entries is left empty rather than failing
    // validation of an otherwise usable tier.
    if src.source_hashes.len() == map.entries.len() {
        map.source_hashes = src.source_hashes.clone();
    }
    let seams: Vec<Option<LightmapSeams>> = (0..map.entries.len())
        .map(|k| carried_seams(src, &map, k))
        .collect();
    for (entry, s) in map.entries.iter_mut().zip(seams) {
        entry.seams = s;
    }
    map.validate()
        .map_err(|e| format!("lightmap_tier_derive: {}: {e}", e.code()))?;
    Ok((map, stats))
}

/// The bake's stored seam figures plus what the downsample added on the same edges (module docs).
fn carried_seams(src: &Lightmap, tier: &Lightmap, entry: usize) -> Option<LightmapSeams> {
    let stored = src.entries.get(entry)?.seams?;
    let (Some(before), Some(after)) = (
        seam_report_by_source_vertex(src, entry).seams,
        seam_report_by_source_vertex(tier, entry).seams,
    ) else {
        return Some(stored);
    };
    Some(LightmapSeams {
        mean: stored.mean + (after.mean - before.mean).max(0.0),
        p95: stored.p95 + (after.p95 - before.p95).max(0.0),
        max: stored.max.max(stored.max + (after.max - before.max)),
        ..stored
    })
}

/// Fills the empty texels of `rect` in waves from filled neighbours of one chart each (module docs).
/// Returns the texels filled.
fn fill_gutters(
    irr: &mut [[f32; 3]],
    dirs: &mut [[f32; 4]],
    cov: &mut [u8],
    chart_of: &mut [u32],
    rect: [u32; 4],
    atlas_w: u32,
) -> u64 {
    let [qx, qy, tw, th] = rect;
    let (tw, th) = (tw as i64, th as i64);
    let at = |x: i64, y: i64| (qy as i64 + y) as usize * atlas_w as usize + (qx as i64 + x) as usize;
    let mut filled = 0u64;
    let mut frontier: Vec<(i64, i64)> = (0..th)
        .flat_map(|y| (0..tw).map(move |x| (x, y)))
        .filter(|&(x, y)| chart_of[(y * tw + x) as usize] != u32::MAX)
        .collect();
    let mut queued = vec![false; (tw * th) as usize];
    while !frontier.is_empty() {
        // The empty neighbours of this wave, in row order.
        let mut wave: Vec<(i64, i64)> = Vec::new();
        for &(x, y) in &frontier {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= tw || ny >= th {
                        continue;
                    }
                    let k = (ny * tw + nx) as usize;
                    if chart_of[k] == u32::MAX && !queued[k] {
                        queued[k] = true;
                        wave.push((nx, ny));
                    }
                }
            }
        }
        wave.sort_by_key(|&(x, y)| (y, x));
        let mut values: Vec<(usize, u32, [f32; 3], [f32; 4])> = Vec::with_capacity(wave.len());
        for &(x, y) in &wave {
            let mut by_chart: BTreeMap<u32, (u32, [f32; 3], [f32; 4])> = BTreeMap::new();
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let (nx, ny) = (x + dx, y + dy);
                    if (dx, dy) == (0, 0) || nx < 0 || ny < 0 || nx >= tw || ny >= th {
                        continue;
                    }
                    let c = chart_of[(ny * tw + nx) as usize];
                    if c == u32::MAX {
                        continue;
                    }
                    let j = at(nx, ny);
                    let slot = by_chart.entry(c).or_insert((0, [0.0; 3], [0.0; 4]));
                    slot.0 += 1;
                    for (s, v) in slot.1.iter_mut().zip(irr[j]) {
                        *s += v;
                    }
                    if let Some(d) = dirs.get(j) {
                        for (s, v) in slot.2.iter_mut().zip(d) {
                            *s += v;
                        }
                    }
                }
            }
            // The chart with the most filled neighbours; ties go to the lower chart (BTreeMap order).
            let Some((&c, &(count, s, d))) = by_chart
                .iter()
                .max_by(|a, b| a.1.0.cmp(&b.1.0).then(b.0.cmp(a.0)))
            else {
                continue;
            };
            // Continue the selected chart's first derivative across its first gutter ring.
            // Only original baked pairs participate; later waves cannot amplify extrapolation.
            let mut extrapolated = [0.0; 3];
            let mut axes = 0;
            for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let (ex, ey) = (x + dx, y + dy);
                let (ix, iy) = (x + 2 * dx, y + 2 * dy);
                if ix < 0 || iy < 0 || ix >= tw || iy >= th {
                    continue;
                }
                let (edge, inner) = (at(ex, ey), at(ix, iy));
                if cov[edge] != Coverage::BAKED
                    || cov[inner] != Coverage::BAKED
                    || chart_of[(ey * tw + ex) as usize] != c
                    || chart_of[(iy * tw + ix) as usize] != c
                {
                    continue;
                }
                for ((sum, edge), inner) in extrapolated.iter_mut().zip(irr[edge]).zip(irr[inner]) {
                    *sum += (2.0 * edge - inner).clamp(0.5 * edge, 1.5 * edge);
                }
                axes += 1;
            }
            let light = if axes > 0 {
                extrapolated.map(|v| v / axes as f32)
            } else {
                s.map(|v| v / count as f32)
            };
            values.push(((y * tw + x) as usize, c, light, d.map(|v| v / count as f32)));
        }
        frontier.clear();
        for (k, c, s, d) in values {
            let (x, y) = (k as i64 % tw, k as i64 / tw);
            let j = at(x, y);
            irr[j] = s;
            if let Some(slot) = dirs.get_mut(j) {
                *slot = d;
            }
            cov[j] = Coverage::DILATED;
            chart_of[k] = c;
            filled += 1;
            frontier.push((x, y));
        }
    }
    filled
}

/// The budget verdict of one tier's lightmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TierVerdict {
    /// The lightmap fits the tier's budget (or the tier has none): it ships.
    Fits,
    /// The lightmap is over the tier's budget: the tier ships without it.
    OverBudget,
    /// The tier's rule ships no lightmap.
    None,
}

impl TierVerdict {
    /// Whether the tier package carries the lightmap.
    #[must_use]
    pub fn ships(self) -> bool {
        self == Self::Fits
    }
}

/// One tier's lightmap: what the cook writes for that tier's package and what it reports.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LightmapTierReport {
    /// The tier name.
    pub tier: String,
    /// Texels per metre of the tier's lightmap (the bake's over the factor).
    pub density: f32,
    /// The factor from the bake.
    pub factor: u32,
    /// Atlas width, texels.
    pub width: u32,
    /// Atlas height, texels.
    pub height: u32,
    /// The texel encoding (`rgb9e5`, `bc6h`, `astc_hdr`).
    pub encoding: &'static str,
    /// Whether the tier's lightmap keeps the direction section.
    pub directional: bool,
    /// Bytes of the tier's `.flmap` (0 when it ships none).
    pub bytes: u64,
    /// The tier's budget, bytes.
    pub budget_bytes: Option<u64>,
    /// The budget verdict.
    pub verdict: TierVerdict,
    /// The worst entry's seam `p95` at the tier.
    pub seam_p95: Option<f32>,
    /// What the derive did.
    pub derive: TierDeriveStats,
}

/// One tier's lightmap from `src` (a cooked or baked lightmap) under `rule`, for `platform`: the
/// `.flmap` bytes when the tier ships it, and the report.
///
/// # Errors
///
/// `lightmap_tier_rule:`, `lightmap_tier_derive:` or `lightmap_direction_encode:` messages, or the
/// format's own codes when the result does not encode.
pub fn cook_lightmap_tier(
    src: &Lightmap,
    tier: &str,
    rule: &LightmapTierRule,
    platform: &str,
) -> Result<(Option<Vec<u8>>, LightmapTierReport), String> {
    let _span = tracing::info_span!("cook_lightmap_tier", tier).entered();
    rule.validate()?;
    let factor = rule.factor_for(src.density)?;
    let mut report = LightmapTierReport {
        tier: tier.to_string(),
        density: src.density / factor as f32,
        factor,
        width: 0,
        height: 0,
        encoding: "",
        directional: false,
        bytes: 0,
        budget_bytes: rule.budget_bytes(),
        verdict: TierVerdict::None,
        seam_p95: None,
        derive: TierDeriveStats::default(),
    };
    if rule.none {
        return Ok((None, report));
    }
    let encoding = rule
        .encoding
        .map_or_else(|| src.texel_encoding(), |e| e.for_platform(platform));
    let padding = rule.chart_padding(encoding);
    let (mut map, stats) = derive_lightmap_tier_padded(src, factor, Some(padding))?;
    if !rule.directional {
        map.directions.clear();
        map.direction_blocks = None;
    }
    if map.texel_encoding() != encoding {
        map.encode_texels(encoding)
            .map_err(|e| format!("{}: {e}", e.code()))?;
    }
    if map.is_directional() {
        let directions = rule.directions.map_or_else(
            || {
                src.direction_encoding()
                    .unwrap_or(regia_asset::lightmap::LightmapDirectionEncoding::Rgba8)
            },
            |d| d.for_platform(platform),
        );
        if map.direction_encoding() != Some(directions) {
            encode_lightmap_directions(&mut map, directions)?;
        }
    }
    let bytes = map.to_bytes().map_err(|e| format!("{}: {e}", e.code()))?;
    report.width = map.width;
    report.height = map.height;
    report.encoding = map.texel_encoding().name();
    report.directional = map.is_directional();
    report.bytes = bytes.len() as u64;
    report.seam_p95 = map
        .entries
        .iter()
        .filter_map(|e| e.seams.map(|s| s.p95))
        .reduce(f32::max);
    report.derive = stats;
    report.verdict = match report.budget_bytes {
        Some(budget) if report.bytes > budget => TierVerdict::OverBudget,
        _ => TierVerdict::Fits,
    };
    if report.verdict.ships() {
        Ok((Some(bytes), report))
    } else {
        Ok((None, report))
    }
}

/// One tier's cooked `.flmap` bytes (`None` when the tier ships none) and its report.
pub type TierOutput = (Option<Vec<u8>>, LightmapTierReport);

/// Every tier of `table` from `src`, in tier-name order.
///
/// # Errors
///
/// The first tier that fails ([`cook_lightmap_tier`]).
pub fn cook_lightmap_tiers(
    src: &Lightmap,
    table: &LightmapTierTable,
    platform: &str,
) -> Result<Vec<TierOutput>, String> {
    table
        .tiers
        .iter()
        .map(|(name, rule)| {
            cook_lightmap_tier(src, name, rule, platform).map_err(|e| format!("tier {name}: {e}"))
        })
        .collect()
}

/// What one tier package does with the cooked lightmaps it ships ([`plan_tier_pak`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TierPakPlan {
    /// Cooked files (`FCKD` wrapper and the tier's `.flmap`) that replace the cache's, by asset.
    pub overrides: BTreeMap<regia_asset::AssetId, Vec<u8>>,
    /// Lightmaps the tier ships without (its rule says none, or over its budget).
    pub dropped: std::collections::BTreeSet<regia_asset::AssetId>,
    /// One report per lightmap, in asset order.
    pub reports: Vec<(regia_asset::AssetId, LightmapTierReport)>,
}

impl TierPakPlan {
    /// Bytes of the tier's lightmaps (the `.flmap` payloads it ships).
    #[must_use]
    pub fn lightmap_bytes(&self) -> u64 {
        self.reports
            .iter()
            .filter(|(_, r)| r.verdict.ships())
            .map(|(_, r)| r.bytes)
            .sum()
    }
}

/// Reads the cooked lightmap `id` from `cooked_dir`: its wrapper header and the lightmap.
fn read_cooked_lightmap(
    cooked_dir: &std::path::Path,
    id: regia_asset::AssetId,
) -> Result<(regia_asset::CookedHeader, Lightmap), String> {
    let path = cooked_dir.join(regia_asset::cooked_file_name(id));
    let bytes = std::fs::read(&path).map_err(|e| format!("lightmap_tier_read: {}: {e}", path.display()))?;
    let (header, payload) = regia_asset::decode_cooked_file(&bytes)
        .map_err(|e| format!("lightmap_tier_read: {}: {e}", path.display()))?;
    let map = Lightmap::from_bytes(payload)
        .map_err(|e| format!("lightmap_tier_read: {}: {}: {e}", path.display(), e.code()))?;
    Ok((header, map))
}

/// The tier `tier` of every cooked lightmap in `ids` (the cooked cache `cooked_dir`): what replaces
/// each one in that tier's pak, or which ships none.
///
/// # Errors
///
/// A cooked lightmap that does not read, or a tier that does not cook ([`cook_lightmap_tier`]).
pub fn plan_tier_pak(
    cooked_dir: &std::path::Path,
    ids: &[regia_asset::AssetId],
    tier: &str,
    rule: &LightmapTierRule,
    platform: &str,
) -> Result<TierPakPlan, String> {
    let _span = tracing::info_span!("plan_tier_pak", tier, lightmaps = ids.len()).entered();
    let mut plan = TierPakPlan::default();
    let mut sorted = ids.to_vec();
    sorted.sort();
    sorted.dedup();
    for id in sorted {
        let (header, map) = read_cooked_lightmap(cooked_dir, id)?;
        let (bytes, report) = cook_lightmap_tier(&map, tier, rule, platform)?;
        match bytes {
            Some(payload) => {
                let header = regia_asset::CookedHeader {
                    payload_len: payload.len() as u64,
                    ..header
                };
                let file = regia_asset::encode_cooked_file(&header, &payload)
                    .map_err(|e| format!("lightmap_tier_write: {e}"))?;
                plan.overrides.insert(id, file);
            }
            None => {
                plan.dropped.insert(id);
            }
        }
        plan.reports.push((id, report));
    }
    Ok(plan)
}

/// [`crate::write_pak`] with the cooked files of `overrides` in place of the cache's.
///
/// # Errors
///
/// As [`crate::write_pak`].
pub fn write_pak_with_overrides(
    out_path: &std::path::Path,
    cooked_dir: &std::path::Path,
    entries: &[(regia_asset::AssetId, regia_core::id::TypePathHash)],
    manifest: &regia_asset::AssetManifest,
    options: &crate::PakWriteOptions,
    overrides: &BTreeMap<regia_asset::AssetId, Vec<u8>>,
) -> Result<(), crate::pak_writer::PakWriteError> {
    let mut cache = crate::pak_writer::read_cooked_from(cooked_dir);
    crate::pak_writer::write_pak_with(out_path, entries, manifest, options, false, |id| match overrides
        .get(&id)
    {
        Some(bytes) => Ok(bytes.clone()),
        None => cache(id),
    })
}

/// One cooked lightmap's tiers ([`cook_project_lightmap_tiers`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProjectLightmapTiers {
    /// The lightmap's asset path.
    pub path: String,
    /// Its texel density, texels per metre (the bake's).
    pub density: f32,
    /// Bytes of the cooked lightmap.
    pub cooked_bytes: u64,
    /// Every tier, in tier-name order.
    pub tiers: Vec<LightmapTierReport>,
}

/// The file [`cook_project_lightmap_tiers`] writes beside the tier lightmaps.
pub const TIER_REPORT_FILE: &str = "lightmap_tiers.json";

/// The directory under a platform's cooked cache that holds each tier's cooked lightmaps
/// (`lightmap_tiers/<tier>/<cooked file>`).
pub const TIER_DIR: &str = "lightmap_tiers";

/// Every tier of every cooked lightmap of the cache `cooked_dir` (`manifest` lists them): writes the
/// cooked tier files under `cooked_dir/lightmap_tiers/<tier>/` (a tier that ships none has no file)
/// and the report `cooked_dir/lightmap_tiers.json` (atomically), and returns the report.
///
/// # Errors
///
/// A lightmap that does not read or cook, or a file that cannot be written.
pub fn cook_project_lightmap_tiers(
    cooked_dir: &std::path::Path,
    manifest: &regia_asset::AssetManifest,
    table: &LightmapTierTable,
    platform: &str,
) -> Result<Vec<ProjectLightmapTiers>, String> {
    let _span = tracing::info_span!("cook_project_lightmap_tiers", platform).entered();
    let mut lightmaps: Vec<(&str, regia_asset::AssetId)> = manifest
        .entries
        .iter()
        .filter(|e| e.type_path == LIGHTMAP_TYPE_PATH)
        .map(|e| (e.path.as_str(), e.id))
        .collect();
    lightmaps.sort();
    let root = cooked_dir.join(TIER_DIR);
    if root.exists() {
        std::fs::remove_dir_all(&root)
            .map_err(|e| format!("lightmap_tier_write: {}: {e}", root.display()))?;
    }
    let mut out = Vec::with_capacity(lightmaps.len());
    for (path, id) in lightmaps {
        let (_, map) = read_cooked_lightmap(cooked_dir, id)?;
        let cooked_bytes =
            std::fs::metadata(cooked_dir.join(regia_asset::cooked_file_name(id))).map_or(0, |m| m.len());
        let mut tiers = Vec::with_capacity(table.tiers.len());
        for (name, rule) in &table.tiers {
            let plan = plan_tier_pak(cooked_dir, &[id], name, rule, platform)
                .map_err(|e| format!("{path}: tier {name}: {e}"))?;
            if let Some(file) = plan.overrides.get(&id) {
                let dir = root.join(name);
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("lightmap_tier_write: {}: {e}", dir.display()))?;
                write_atomic(&dir.join(regia_asset::cooked_file_name(id)), file)?;
            }
            tiers.extend(plan.reports.into_iter().map(|(_, r)| r));
        }
        out.push(ProjectLightmapTiers {
            path: path.to_string(),
            density: map.density,
            cooked_bytes,
            tiers,
        });
    }
    let json = serde_json::to_vec_pretty(
        &serde_json::json!({ "schema": 1, "platform": platform, "lightmaps": out }),
    )
    .map_err(|e| format!("lightmap_tier_write: {e}"))?;
    write_atomic(&cooked_dir.join(TIER_REPORT_FILE), &json)?;
    Ok(out)
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("lightmap_tier_write: {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("lightmap_tier_write: {}: {e}", path.display()))
}

mod border_fit;
mod chart_repack;
#[cfg(test)]
mod tests;
