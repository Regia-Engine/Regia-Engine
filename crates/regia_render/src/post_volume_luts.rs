// SPDX-License-Identifier: MPL-2.0
//! Per-volume colour LUTs (`docs/specs/30-post-volumes-and-physical-camera.md` § 10.2, ADR-2323):
//! post volumes that name a 3D LUT ([`crate::PostVolume::lut`]) blend it into the view's LUT, the
//! table the post stack's colour-grade step samples after the parametric grade.
//!
//! **Off by default.** Nothing happens until a host (or `render.set_post_volume_luts`) sets
//! [`PostVolumeLuts::enabled`]; until then every frame is the frame without this module, byte for
//! byte, whatever the volumes name.
//!
//! # Blend
//!
//! The base is the view's LUT before volumes (the level look's `look.lut`, or the identity). The
//! volumes whose LUT loaded are grouped by priority with the weight of [`crate::PostVolume::weight_at`],
//! exactly like the scalar overrides: inside a tier the tables are weight-averaged (`mean = Σ w·L / Σ w`)
//! and the running table moves toward the mean by `α = min(1, Σ w)`, tiers from the lowest
//! priority to the highest. The blend is done on the CPU, entry by entry, on the grid of the
//! largest table taking part (a smaller table is resampled with its own trilinear lookup), and the
//! result is handed to the post stack as an ordinary LUT. Trilinear lookup is linear in the
//! table, so for tables of one size the blended table looks up exactly the weighted blend of the
//! looked-up colours. A full-weight top tier holding one volume hands that volume's table over
//! unchanged (no copy, the same content key as loading it as the level LUT).
//!
//! The last [`MAX_CACHED_BLENDS`] blends are kept by a key of their inputs (table keys, weights,
//! priorities), so a camera standing still, or several cameras at one spot, blends once. A
//! volume's weight is quantized to 1/256 before that key and before the blend, so a camera
//! moving inside a blend band does not rebuild the table for every float bit (ADR-5159).

use std::collections::HashMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_scene::GlobalTransform;

use crate::lut::ColorLut;
use crate::post::PostSettings;
use crate::post_volumes::PostVolume;

/// Blended tables kept for reuse (least recently used goes first).
pub const MAX_CACHED_BLENDS: usize = 4;

/// Distinct LUT paths [`PostVolumeLuts`] keeps; a path beyond them is not loaded (warned once).
pub const MAX_LOADED_LUTS: usize = 64;

/// One volume's table and weight at a point, the input of [`blend_luts`].
#[derive(Clone, Debug)]
pub struct LutContribution {
    /// Effective weight at the point, 0–1 (not finite or not positive counts as 0).
    pub weight: f32,
    /// The volume's priority.
    pub priority: i32,
    /// A stable tie-break key (the entity bits); unique per volume.
    pub key: u64,
    /// The volume's table.
    pub lut: Arc<ColorLut>,
}

/// A weight that is finite and positive, at most 1; anything else is 0.
fn sanitize_weight(weight: f32) -> f32 {
    if weight.is_finite() && weight > 0.0 {
        weight.min(1.0)
    } else {
        0.0
    }
}

/// Blend weights share a table inside one 1/256 step (ADR-5159). 1/256 is exact in `f32`.
const WEIGHT_STEP: f32 = 1.0 / 256.0;

/// [`sanitize_weight`] rounded to [`WEIGHT_STEP`]. A step that rounds to 0 contributes nothing.
fn quantize_weight(weight: f32) -> f32 {
    let weight = sanitize_weight(weight);
    if weight <= 0.0 {
        return 0.0;
    }
    (weight / WEIGHT_STEP).round() * WEIGHT_STEP
}

/// The entry of `lut` at grid point `(r, g, b)` of an `n`-edge grid.
fn entry_at(lut: &ColorLut, n: usize, r: usize, g: usize, b: usize) -> [f32; 3] {
    if lut.size() as usize == n {
        lut.data()[(b * n + g) * n + r]
    } else {
        let f = (n - 1) as f32;
        lut.sample([r as f32 / f, g as f32 / f, b as f32 / f])
    }
}

/// The priority tiers of a sorted list.
fn tiers(contributions: &[LutContribution]) -> impl Iterator<Item = &[LutContribution]> {
    contributions.chunk_by(|a, b| a.priority == b.priority)
}

fn tier_weight(tier: &[LutContribution]) -> f32 {
    tier.iter().map(|c| sanitize_weight(c.weight)).sum()
}

/// Blends `contributions` over `base` (`None` = the identity) as the module docs describe and
/// returns the view's table (`None` = the identity, when there is neither a base nor a
/// contribution). Sorts `contributions` by priority and key.
#[must_use]
pub fn blend_luts(
    base: Option<&Arc<ColorLut>>,
    contributions: &mut [LutContribution],
) -> Option<Arc<ColorLut>> {
    contributions.sort_unstable_by(|a, b| a.priority.cmp(&b.priority).then(a.key.cmp(&b.key)));
    let mut table = Vec::new();
    blend_sorted(base, contributions, &mut table)
}

/// `contributions` is already sorted by priority and key. `table` is scratch: a computed blend
/// refills it, then moves the entries out and leaves the capacity behind.
fn blend_sorted(
    base: Option<&Arc<ColorLut>>,
    contributions: &[LutContribution],
    table: &mut Vec<[f32; 3]>,
) -> Option<Arc<ColorLut>> {
    let live: usize = contributions
        .iter()
        .filter(|c| sanitize_weight(c.weight) > 0.0)
        .count();
    if live == 0 {
        return base.cloned();
    }
    // A full tier (Σ w ≥ 1) replaces everything below it: start there.
    let mut start = 0;
    let mut full_from = None;
    for tier in tiers(contributions) {
        if tier_weight(tier) >= 1.0 {
            full_from = Some(start);
        }
        start += tier.len();
    }
    let rest = &contributions[full_from.unwrap_or(0)..];
    if full_from.is_some() {
        let mut groups = tiers(rest);
        if let (Some(only), None) = (groups.next(), groups.next()) {
            let mut live = only.iter().filter(|c| sanitize_weight(c.weight) > 0.0);
            if let (Some(one), None) = (live.next(), live.next()) {
                return Some(one.lut.clone());
            }
        }
    }
    let base = if full_from.is_some() { None } else { base };
    let n = fill_blend_table(table, base, rest);
    let mut owned = Vec::new();
    owned.append(table);
    ColorLut::from_entries(n, owned).map(Arc::new)
}

/// Writes the blended grid into `table` (cleared first; capacity kept) and returns its edge.
fn fill_blend_table(
    table: &mut Vec<[f32; 3]>,
    base: Option<&Arc<ColorLut>>,
    rest: &[LutContribution],
) -> u32 {
    let n = rest
        .iter()
        .filter(|c| sanitize_weight(c.weight) > 0.0)
        .map(|c| c.lut.size())
        .chain(base.map(|b| b.size()))
        .max()
        .unwrap_or(2) as usize;
    let f = (n - 1) as f32;
    table.clear();
    table.reserve(n * n * n);
    for b in 0..n {
        for g in 0..n {
            for r in 0..n {
                table.push(match base {
                    Some(base) => entry_at(base, n, r, g, b),
                    None => [r as f32 / f, g as f32 / f, b as f32 / f],
                });
            }
        }
    }
    for tier in tiers(rest) {
        let total = tier_weight(tier);
        if total <= 0.0 {
            continue;
        }
        let alpha = total.min(1.0);
        let keep = 1.0 - alpha;
        for v in table.iter_mut() {
            *v = v.map(|c| c * keep);
        }
        for c in tier {
            let share = alpha * sanitize_weight(c.weight) / total;
            if share <= 0.0 {
                continue;
            }
            for (i, entry) in table.iter_mut().enumerate() {
                let e = entry_at(&c.lut, n, i % n, (i / n) % n, i / (n * n));
                for (v, sample) in entry.iter_mut().zip(e) {
                    *v += share * sample;
                }
            }
        }
    }
    n as u32
}

/// The cache key of a blend: the base's key and every contribution's table key, weight bits and
/// priority, in the sorted order.
fn blend_key(base: Option<&Arc<ColorLut>>, sorted: &[LutContribution]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    base.map(|b| b.key()).hash(&mut hasher);
    for c in sorted {
        c.lut.key().hash(&mut hasher);
        sanitize_weight(c.weight).to_bits().hash(&mut hasher);
        c.priority.hash(&mut hasher);
    }
    hasher.finish()
}

/// Counters of [`PostVolumeLuts`] (the agent protocol reports them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PostVolumeLutStats {
    /// Tables loaded from disk (or inserted by the host).
    pub loads: u64,
    /// Paths that failed to load.
    pub failures: u64,
    /// Blends computed.
    pub blends: u64,
    /// Blends served from the cache.
    pub cache_hits: u64,
}

/// The per-volume LUT switch, the loaded tables by path and the blend cache (a resource the
/// render plugin inserts; see the module docs).
#[derive(Resource, Default)]
pub struct PostVolumeLuts {
    /// Volumes blend their LUTs into the view's (default `false`: volume LUTs are ignored).
    pub enabled: bool,
    tables: HashMap<String, Option<Arc<ColorLut>>>,
    blends: Vec<(u64, Arc<ColorLut>)>,
    scratch: Vec<LutContribution>,
    /// Working grid for [`blend_sorted`]. Capacity stays across misses of one size.
    table_scratch: Vec<[f32; 3]>,
    stats: PostVolumeLutStats,
    warned_full: bool,
}

impl PostVolumeLuts {
    /// Adds (or replaces) the table for `path` without reading a file (hosts that load LUTs
    /// themselves, tests).
    pub fn insert(&mut self, path: impl Into<String>, lut: Arc<ColorLut>) {
        self.tables.insert(path.into(), Some(lut));
        self.stats.loads += 1;
    }

    /// The loaded table for `path` (`None` when it is not loaded or failed).
    #[must_use]
    pub fn table(&self, path: &str) -> Option<&Arc<ColorLut>> {
        self.tables.get(path).and_then(Option::as_ref)
    }

    /// `path` was tried and did not load (it is not retried until [`Self::clear`]).
    #[must_use]
    pub fn failed(&self, path: &str) -> bool {
        matches!(self.tables.get(path), Some(None))
    }

    /// Every path seen, sorted, with whether it loaded.
    #[must_use]
    pub fn paths(&self) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = self
            .tables
            .iter()
            .map(|(path, lut)| (path.clone(), lut.is_some()))
            .collect();
        out.sort();
        out
    }

    /// Forgets every table and blend, so the next frame reloads the files (after an edit).
    pub fn clear(&mut self) {
        self.tables.clear();
        self.blends.clear();
        self.warned_full = false;
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> PostVolumeLutStats {
        self.stats
    }

    /// Blends the LUTs of the `volumes` that reach `eye` into `post.lut`. Does nothing while
    /// [`Self::enabled`] is off, or when no volume with a loaded table reaches the eye.
    pub fn apply<'a>(
        &mut self,
        volumes: impl IntoIterator<Item = (Entity, &'a PostVolume, &'a GlobalTransform)>,
        eye: [f32; 3],
        post: &mut PostSettings,
    ) {
        if !self.enabled {
            return;
        }
        self.scratch.clear();
        for (entity, volume, world) in volumes {
            if !volume.is_supported() {
                continue;
            }
            let Some(lut) = volume
                .lut
                .as_deref()
                .and_then(|p| self.tables.get(p))
                .and_then(Option::as_ref)
            else {
                continue;
            };
            let weight = quantize_weight(volume.weight_at(world, eye));
            if weight > 0.0 {
                self.scratch.push(LutContribution {
                    weight,
                    priority: volume.priority,
                    key: entity.to_bits(),
                    lut: lut.clone(),
                });
            }
        }
        if self.scratch.is_empty() {
            return;
        }
        let _span = tracing::info_span!("post_volume_luts").entered();
        self.scratch
            .sort_unstable_by(|a, b| a.priority.cmp(&b.priority).then(a.key.cmp(&b.key)));
        let key = blend_key(post.lut.as_ref(), &self.scratch);
        if let Some(i) = self.blends.iter().position(|(k, _)| *k == key) {
            let hit = self.blends.remove(i);
            post.lut = Some(hit.1.clone());
            self.blends.push(hit);
            self.stats.cache_hits += 1;
            return;
        }
        let blended = blend_sorted(post.lut.as_ref(), &self.scratch, &mut self.table_scratch);
        self.stats.blends += 1;
        if let Some(lut) = &blended {
            if self.blends.len() >= MAX_CACHED_BLENDS {
                self.blends.remove(0);
            }
            self.blends.push((key, lut.clone()));
        }
        post.lut = blended;
    }
}

/// `PreUpdate`: loads the `.cube` / PNG-strip LUT of every volume that names one not seen yet
/// (a relative path resolves through the asset server first, then as a loose file under the
/// project root, as the level LUT does; `\` is accepted as a separator). Only while
/// [`PostVolumeLuts::enabled`]; a failed load warns once per path and is not retried.
pub fn load_post_volume_luts(
    volumes: Query<&PostVolume>,
    mut assets: Option<ResMut<regia_asset::AssetServer>>,
    luts: Option<ResMut<PostVolumeLuts>>,
) {
    let _mem = regia_core::memory::enter_owner(&crate::heap_owners::COLOR_LUTS);
    let Some(mut luts) = luts else {
        return;
    };
    if !luts.enabled {
        return;
    }
    for volume in &volumes {
        let Some(path) = volume.lut.as_deref() else {
            continue;
        };
        if luts.tables.contains_key(path) {
            continue;
        }
        if luts.tables.len() >= MAX_LOADED_LUTS {
            if !luts.warned_full {
                luts.warned_full = true;
                tracing::warn!(
                    target: "regia_render",
                    "post volumes name more than {MAX_LOADED_LUTS} LUTs; {path} is not loaded"
                );
            }
            continue;
        }
        let _span = tracing::info_span!("load_post_volume_lut").entered();
        let loaded = match crate::lut::load_named(path, assets.as_deref_mut()) {
            Ok(lut) => {
                luts.stats.loads += 1;
                Some(lut)
            }
            Err(err) => {
                luts.stats.failures += 1;
                tracing::warn!(target: "regia_render", "post volume LUT {path}: {err}; the volume's LUT is ignored");
                None
            }
        };
        luts.tables.insert(path.to_string(), loaded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post_volumes::PostVolumeShape;

    fn grade() -> Arc<ColorLut> {
        Arc::new(ColorLut::from_fn(4, |c| {
            [c[0] * 0.25, 0.5 * c[1] + 0.1, 1.0 - c[2]]
        }))
    }

    fn volume(weight: f32) -> PostVolume {
        PostVolume {
            shape: PostVolumeShape::Unbound,
            weight,
            lut: Some("grade.cube".into()),
            ..PostVolume::default()
        }
    }

    /// A camera creeping through a blend band must not rebuild the table for every float bit.
    #[test]
    fn a_sub_quantum_weight_keeps_the_blend_and_a_step_changes_it() {
        let mut luts = PostVolumeLuts {
            enabled: true,
            ..PostVolumeLuts::default()
        };
        luts.insert("grade.cube", grade());
        let entity = Entity::from_raw_u32(3).expect("entity");
        let world = GlobalTransform::IDENTITY;
        let mut post = PostSettings::default();
        let at = volume(0.5);
        luts.apply([(entity, &at, &world)], [0.0; 3], &mut post);
        let key = post.lut.as_ref().expect("blended").key();
        assert_eq!(luts.stats().blends, 1);

        let nudged = volume(0.5 + 1.0 / 1024.0);
        let mut again = PostSettings::default();
        luts.apply([(entity, &nudged, &world)], [0.0; 3], &mut again);
        assert_eq!(
            luts.stats().blends,
            1,
            "a weight change below 1/256 must not blend again"
        );
        assert_eq!(again.lut.as_ref().expect("cached").key(), key);

        let stepped = volume(0.5 + 1.0 / 256.0);
        let mut next = PostSettings::default();
        luts.apply([(entity, &stepped, &world)], [0.0; 3], &mut next);
        assert_eq!(luts.stats().blends, 2, "one step across 1/256 blends once");
        assert_ne!(next.lut.as_ref().expect("reblended").key(), key);
    }
}
