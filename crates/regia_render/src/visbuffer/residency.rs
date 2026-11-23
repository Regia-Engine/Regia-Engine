// SPDX-License-Identifier: MPL-2.0
//! Cluster residency (ADR-3118, `docs/specs/98-visibility-buffer.md` § 8c): a byte budget for
//! the visibility buffer's cluster data, so VRAM follows what is on screen instead of what is
//! loaded.
//!
//! A mesh's clusters split into **bands** by their parent error against a ladder of thresholds
//! `T_1 < … < T_{n-1}`: cluster `c` is in band `#{i : T_i < parent_error(c)}`. Step `k` keeps
//! bands `k..n` resident, which is exactly the clusters whose parent error passes `T_k`: every
//! ancestor of a resident cluster is resident, and the clusters with `lod_error ≤ T_k` among them
//! are the object-space cut at `T_k`, a complete cover of the surface. Band `n - 1` is the base and
//! is always resident.
//!
//! The renderer keeps every cluster record of a managed mesh. An absent cluster's record has
//! `lod_error = f32::MAX` and no vertices or triangles (the cull never selects it, and a selection
//! would draw nothing); a resident cluster on the step's cut has `lod_error = 0`, so only its
//! parent test decides. Child and parent share the parent sphere and error, so the cut stays
//! consistent: per DAG chain the cull draws the full DAG's choice when it is resident, otherwise
//! the step's cut cluster. `vis_cull.wgsl` does not change.
//!
//! Each band's geometry block holds the vertices it owns (a vertex belongs to the coarsest band
//! that uses it), its clusters' vertex lists and their triangles. Bands load coarse to fine, so
//! every vertex a resident cluster names is resident. This module holds the per-mesh plan, the
//! demand test and the public knobs and counters; the scheduling against the cluster arena is in
//! `gpu/resident.rs`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use super::arena::ClusterKey;
use super::meshlet::{Meshlet, MeshletMesh, ROOT_ERROR};
use crate::memory_budget::BudgetTier;
use crate::profile::RenderProfilePreset;

/// Bands a mesh splits into at most; the per-band counters of [`VisBufferResidencyStats`] have
/// this many entries.
pub const RESIDENCY_BANDS: usize = 16;

/// `lod_error` of an absent cluster's record: no view selects it.
pub(super) const ABSENT_ERROR: f32 = f32::MAX;

/// Fold a cooked cluster's errors before they are uploaded.
///
/// A non-finite own error becomes 0, the same value the residency ladder uses when it builds
/// thresholds. A NaN parent becomes `ROOT_ERROR`, so the cull still treats the cluster as a root
/// (`parent_error >= 3.0e38` in `vis_cull.wgsl`). `+inf` and every finite parent stay, including
/// a negative parent: that cluster sorted into the finest band and is not the cover.
pub(super) fn finite_errors(lod_error: f32, parent_error: f32) -> (f32, f32) {
    let lod_error = if lod_error.is_finite() { lod_error } else { 0.0 };
    let parent_error = if parent_error.is_nan() {
        ROOT_ERROR
    } else {
        parent_error
    };
    (lod_error, parent_error)
}

/// Relative margin of the demand bounds (wider than the cull's own `LEVEL_MARGIN`, so the CPU
/// asks for a band a little before the cull can select it).
const DEMAND_MARGIN: f32 = 1.0e-3;

const MIB: u64 = 1024 * 1024;

/// The cluster residency knobs (ADR-3118, disk streaming ADR-3708). Off by default: every
/// resident mesh keeps its whole cluster DAG on the GPU (ADR-353).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClusterResidency {
    /// Budget the cluster data of meshes made resident while this is on (`false`: the whole-DAG
    /// arena of ADR-353, frames bit-identical to a renderer without residency).
    pub enabled: bool,
    /// Bytes of cluster records and geometry the managed meshes may hold on the GPU.
    pub budget_bytes: u64,
    /// Bytes the residency layer may upload per frame (band geometry, rewritten cluster records
    /// and level runs). A new mesh's base is uploaded when it is first drawn and is not counted.
    pub upload_bytes_per_frame: u64,
    /// Bytes of band payloads (delivered cluster lists and triangles) and the retained band plan
    /// (packed vertices, cluster records) the managed meshes may hold in RAM; over budget, the
    /// least recently arrived payload that is not mid-upload is dropped back to the on-disk state
    /// (ADR-3708).
    pub ram_budget_bytes: u64,
    /// Bytes of band payloads the frame may read from the pak per frame (ADR-3708): new read
    /// requests are issued within this bound, estimated from the band's vertex lists and
    /// triangles. A band larger than the whole bound is still issued as the frame's first read.
    pub io_bytes_per_frame: u64,
}

impl ClusterResidency {
    /// Off: no budget, the whole DAG of every mesh resident.
    pub const OFF: Self = Self {
        enabled: false,
        budget_bytes: 0,
        upload_bytes_per_frame: 0,
        ram_budget_bytes: 0,
        io_bytes_per_frame: 0,
    };

    /// On, with the budget of a memory tier (`memory_budget.rs`, master plan § 5.1a; ★ until
    /// measured): Deck 256 MiB VRAM, 4 MiB uploads, 256 MiB RAM, 32 MiB reads per frame;
    /// Minimum 512 MiB, 8 MiB, 384 MiB, 48 MiB; Desktop High 1 GiB, 16 MiB, 512 MiB, 64 MiB;
    /// Integrated Low half of the Deck's row: 128 MiB, 4 MiB, 128 MiB, 16 MiB (ADR-8878).
    #[must_use]
    pub const fn for_tier(tier: BudgetTier) -> Self {
        let (budget, upload, ram, io) = match tier {
            BudgetTier::Deck => (256 * MIB, 4 * MIB, 256 * MIB, 32 * MIB),
            BudgetTier::Minimum => (512 * MIB, 8 * MIB, 384 * MIB, 48 * MIB),
            BudgetTier::DesktopHigh => (1024 * MIB, 16 * MIB, 512 * MIB, 64 * MIB),
            BudgetTier::IntegratedLow => (128 * MIB, 4 * MIB, 128 * MIB, 16 * MIB),
        };
        Self {
            enabled: true,
            budget_bytes: budget,
            upload_bytes_per_frame: upload,
            ram_budget_bytes: ram,
            io_bytes_per_frame: io,
        }
    }

    /// On, with the budget a preset's hardware class would use: Mobile 128 MiB VRAM, 2 MiB
    /// uploads, 128 MiB RAM, 16 MiB reads per frame; Deck as [`BudgetTier::Deck`], Desktop Low as
    /// [`BudgetTier::Minimum`], Desktop High, Ultra and the offline preset as
    /// [`BudgetTier::DesktopHigh`]. Presets themselves keep residency off
    /// ([`crate::RenderProfile::cluster_residency`]).
    #[must_use]
    pub const fn for_preset(preset: RenderProfilePreset) -> Self {
        match preset {
            RenderProfilePreset::MobileLow | RenderProfilePreset::MobileHigh => Self {
                enabled: true,
                budget_bytes: 128 * MIB,
                upload_bytes_per_frame: 2 * MIB,
                ram_budget_bytes: 128 * MIB,
                io_bytes_per_frame: 16 * MIB,
            },
            RenderProfilePreset::Deck => Self::for_tier(BudgetTier::Deck),
            RenderProfilePreset::DesktopLow => Self::for_tier(BudgetTier::Minimum),
            _ => Self::for_tier(BudgetTier::DesktopHigh),
        }
    }
}

impl Default for ClusterResidency {
    fn default() -> Self {
        Self::OFF
    }
}

/// What cluster residency did ([`super::VisBufferArenaStats::residency`], ADR-3118). Bands are
/// numbered finest first; band counts past [`RESIDENCY_BANDS`] cannot occur.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisBufferResidencyStats {
    /// The knobs in use (the runtime override, else the profile's).
    pub config: ClusterResidency,
    /// Meshes whose cluster data residency manages.
    pub meshes: u32,
    /// Resident meshes made resident while residency was off, or with one band: whole DAGs,
    /// outside the budget.
    pub unmanaged_meshes: u32,
    /// Bytes the managed meshes hold: records, resident bands and bands in flight.
    pub resident_bytes: u64,
    /// The most [`Self::resident_bytes`] has been at the end of a frame.
    pub peak_resident_bytes: u64,
    /// Bytes every band of every managed mesh would take (the whole-DAG layout).
    pub full_bytes: u64,
    /// Per band: managed meshes with that band resident.
    pub bands_resident: [u32; RESIDENCY_BANDS],
    /// Per band: managed meshes that have that band.
    pub bands_total: [u32; RESIDENCY_BANDS],
    /// Bands being uploaded (resident bytes, not drawn yet).
    pub pending_bands: u32,
    /// Bands that finished uploading and were switched on, since the renderer was made.
    pub refinements: u64,
    /// Bands evicted since the renderer was made.
    pub evictions: u64,
    /// Bytes those evictions freed.
    pub evicted_bytes: u64,
    /// Bytes the residency layer uploaded in the last frame (at most
    /// [`ClusterResidency::upload_bytes_per_frame`]).
    pub upload_bytes: u64,
    /// The most bytes it uploaded in one frame.
    pub peak_upload_bytes: u64,
    /// Bytes it uploaded since the renderer was made.
    pub upload_bytes_total: u64,
    /// Frames that went over the bound: a band's record flip larger than the whole bound uploads
    /// alone at the start of a frame (raise the bound above the largest band's records,
    /// 96 bytes per cluster, to keep this at zero).
    pub bound_overruns: u64,
    /// Bytes of new meshes' bases uploaded in the last frame (outside the per-frame bound).
    pub base_upload_bytes: u64,
    /// Meshes whose base did not fit the budget (they draw forward until room is made), or whose
    /// pak would not read or whose base band is corrupt (they draw forward until re-inserted).
    pub refused_meshes: u32,
    /// Meshes that wanted a finer band last frame than the budget let them have: they draw their
    /// finest resident band (a mesh larger than the whole budget stays at its base).
    pub degraded_meshes: u32,
    /// Frames a visible managed mesh drew coarser than its demand (its finer band was loading,
    /// failed, or waited for budget): the coarsest resident level covered it (ADR-3708).
    pub fallbacks: u64,
    /// Band payloads delivered from the pak in the last frame (bytes).
    pub io_bytes: u64,
    /// Band payloads delivered from the pak since the renderer was made (bytes).
    pub io_bytes_total: u64,
    /// Read requests in flight (a band per managed mesh at most).
    pub read_queue_depth: u32,
    /// Bands with no payload in RAM and none in flight (on disk, resident on the GPU or not).
    pub bands_on_disk: u32,
    /// Bands with a read in flight.
    pub bands_loading: u32,
    /// Bands with a payload in RAM awaiting upload.
    pub bands_in_ram: u32,
    /// Bytes the managed meshes hold in RAM: the retained band plans (packed vertices, cluster
    /// records) plus the delivered payloads awaiting upload.
    pub ram_bytes: u64,
    /// The most [`Self::ram_bytes`] has been at the end of a frame (the RAM high-water mark).
    pub peak_ram_bytes: u64,
    /// Delivered payloads dropped back to the on-disk state under the RAM budget (they are
    /// read again when the band is next requested).
    pub ram_evictions: u64,
    /// Band reads that failed (`band_unreadable`, `band_corrupt`, `band_not_found`): the band
    /// stayed on disk and the mesh drew its coarsest resident level.
    pub read_errors: u64,
    /// The code of the last failed read, when one failed.
    pub last_read_error_code: Option<&'static str>,
    /// Frames residency ran.
    pub frames: u64,
}

/// The last main view, as the demand test reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ResidencyView {
    pub(super) eye: [f32; 3],
    pub(super) planes: [[f32; 4]; 6],
    /// Pixels per unit of distance at depth 1 (`height / (2 tan(fov / 2))`).
    pub(super) pixels_per_unit: f32,
    pub(super) near: f32,
    pub(super) threshold_px: f32,
}

impl ResidencyView {
    /// The frustum test of `vis_cull.wgsl` `frustum_inside`.
    pub(super) fn inside(&self, center: [f32; 3], radius: f32) -> bool {
        self.planes
            .iter()
            .all(|p| p[0] * center[0] + p[1] * center[1] + p[2] * center[2] + p[3] >= -radius)
    }

    pub(super) fn distance(&self, center: [f32; 3]) -> f32 {
        let d = [
            center[0] - self.eye[0],
            center[1] - self.eye[1],
            center[2] - self.eye[2],
        ];
        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
    }
}

/// The demand bounds of one band (as a level run of ADR-3915, over the band's clusters).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct BandBounds {
    min_error: f32,
    reach: f32,
    max_parent_error: f32,
    parent_reach: f32,
}

/// One band of a mesh's plan.
#[derive(Clone, Debug)]
pub(super) struct Band {
    /// Indices of its clusters in the mesh's list, ascending.
    pub(super) clusters: Vec<u32>,
    /// Source vertices it owns, ascending.
    pub(super) vertices: Vec<u32>,
    /// Words of its block: owned vertices, then the vertex lists, then the triangles.
    pub(super) words: u32,
    bounds: BandBounds,
}

impl Band {
    /// Bytes of its geometry block.
    pub(super) fn bytes(&self) -> u64 {
        u64::from(self.words) * 4
    }
}

fn offset(center: [f32; 4], sphere: [f32; 4]) -> f32 {
    let d = [
        sphere[0] - center[0],
        sphere[1] - center[1],
        sphere[2] - center[2],
    ];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

/// How a mesh's clusters split into bands, and where each cluster's lists sit in its band's
/// block. Built once when the mesh becomes resident.
pub(super) struct MeshPlan {
    pub(super) bands: Vec<Band>,
    /// `T_1..T_{n-1}`, ascending (`thresholds[i - 1]` is `T_i`).
    thresholds: Vec<f32>,
    pub(super) band_of: Vec<u8>,
    /// Per cluster: its vertex list's and its triangles' word offsets in its band's block.
    local: Vec<(u32, u32)>,
    /// Words per vertex in a block.
    vertex_words: u32,
    /// Every source vertex, packed (`vertex_words` each; the mesh's own then its appended ones).
    packed: Vec<u32>,
}

impl MeshPlan {
    /// The plan of `mesh`, whose source vertices `packed` holds at `vertex_words` words each.
    /// `None` when the mesh has one band (nothing to stream; it stays whole) or its clusters are
    /// not stored level by level.
    pub(super) fn build(mesh: &MeshletMesh, packed: Vec<u32>, vertex_words: u32) -> Option<Self> {
        Self::build_with(&mesh.meshlets, mesh.bounds, &mesh.vertices, packed, vertex_words)
    }

    /// [`Self::build`] for a mesh whose vertex lists `lists` supplies separately (the transient
    /// sections of a stream-shaped mesh, ADR-3708).
    pub(super) fn build_with(
        meshlets: &[Meshlet],
        bounds: [f32; 4],
        lists: &[u32],
        packed: Vec<u32>,
        vertex_words: u32,
    ) -> Option<Self> {
        if meshlets.is_empty() || meshlets.windows(2).any(|w| w[1].level < w[0].level) {
            return None;
        }
        // The ladder: the running maximum of each cooker level's largest own error, strictly
        // increasing, finite.
        let mut level_max: BTreeMap<u32, f32> = BTreeMap::new();
        for m in meshlets {
            let e = if m.lod_error.is_finite() {
                m.lod_error.max(0.0)
            } else {
                0.0
            };
            let slot = level_max.entry(m.level).or_insert(0.0);
            *slot = slot.max(e);
        }
        let mut thresholds: Vec<f32> = Vec::new();
        let mut running = f32::NEG_INFINITY;
        for (idx, &e) in level_max.values().enumerate() {
            running = running.max(e);
            if idx == 0 {
                continue;
            }
            if running < ROOT_ERROR * 0.5 && thresholds.last().is_none_or(|&t| running > t) {
                thresholds.push(running);
            }
        }
        if thresholds.len() >= RESIDENCY_BANDS {
            thresholds.drain(..thresholds.len() + 1 - RESIDENCY_BANDS);
        }
        let n = thresholds.len() + 1;
        let band_of_error = |p: f32| -> u8 {
            if p.is_nan() {
                return (n - 1) as u8;
            }
            thresholds.partition_point(|&t| t < p) as u8
        };
        let band_of: Vec<u8> = meshlets.iter().map(|m| band_of_error(m.parent_error)).collect();
        let mut used = vec![false; n];
        for &b in &band_of {
            used[b as usize] = true;
        }
        // A plan needs a base and at least one band under it.
        if n < 2 || !used[n - 1] || used.iter().filter(|u| **u).count() < 2 {
            return None;
        }
        let vertex_count = packed.len() / vertex_words.max(1) as usize;
        // Each vertex belongs to the coarsest band that uses it.
        let mut owner: Vec<i16> = vec![-1; vertex_count];
        for (m, &b) in meshlets.iter().zip(&band_of) {
            let at = m.vertex_offset as usize;
            let list = lists.get(at..at + m.vertex_count as usize)?;
            for &v in list {
                let slot = owner.get_mut(v as usize)?;
                *slot = (*slot).max(i16::from(b));
            }
        }
        let mut bands: Vec<Band> = (0..n)
            .map(|_| Band {
                clusters: Vec::new(),
                vertices: Vec::new(),
                words: 0,
                bounds: BandBounds {
                    min_error: f32::MAX,
                    reach: 0.0,
                    max_parent_error: 0.0,
                    parent_reach: 0.0,
                },
            })
            .collect();
        for (v, &b) in owner.iter().enumerate() {
            if b >= 0 {
                bands[b as usize].vertices.push(v as u32);
            }
        }
        let center = bounds;
        let mut local = vec![(0u32, 0u32); meshlets.len()];
        let mut list_words = vec![0u32; n];
        for (i, (m, &b)) in meshlets.iter().zip(&band_of).enumerate() {
            let band = &mut bands[b as usize];
            band.clusters.push(i as u32);
            local[i].0 = list_words[b as usize];
            list_words[b as usize] += m.vertex_count;
            let bounds = &mut band.bounds;
            bounds.min_error = bounds.min_error.min(m.lod_error.max(0.0));
            bounds.reach = bounds.reach.max(offset(center, m.lod));
            bounds.max_parent_error = if m.parent_error >= ROOT_ERROR || bounds.max_parent_error >= ROOT_ERROR
            {
                ROOT_ERROR
            } else {
                bounds.max_parent_error.max(m.parent_error)
            };
            bounds.parent_reach = bounds
                .parent_reach
                .max(offset(center, m.parent) + m.parent[3].max(0.0));
        }
        let mut tri_words = vec![0u32; n];
        for (i, (m, &b)) in meshlets.iter().zip(&band_of).enumerate() {
            local[i].1 = tri_words[b as usize];
            tri_words[b as usize] += m.triangle_count;
        }
        for (b, band) in bands.iter_mut().enumerate() {
            let head = band.vertices.len() as u64 * u64::from(vertex_words);
            let words = head + u64::from(list_words[b]) + u64::from(tri_words[b]);
            band.words = u32::try_from(words).ok()?;
            for i in band.clusters.iter().map(|&i| i as usize) {
                local[i].0 += head as u32;
                local[i].1 += (head + u64::from(list_words[b])) as u32;
            }
            let bounds = &mut band.bounds;
            if !bounds.min_error.is_finite() || !bounds.reach.is_finite() {
                bounds.min_error = 0.0;
                bounds.reach = 0.0;
            }
            if !bounds.parent_reach.is_finite() || bounds.max_parent_error.is_nan() {
                bounds.max_parent_error = ROOT_ERROR;
            }
        }
        Some(Self {
            bands,
            thresholds,
            band_of,
            local,
            vertex_words,
            packed,
        })
    }

    /// Bands, finest first.
    pub(super) fn band_count(&self) -> usize {
        self.bands.len()
    }

    /// The base band (always resident).
    pub(super) fn base(&self) -> usize {
        self.bands.len() - 1
    }

    /// Bytes of every band and the records: the whole-DAG layout.
    pub(super) fn full_bytes(&self, record_bytes: u64) -> u64 {
        self.bands.iter().map(Band::bytes).sum::<u64>() + self.band_of.len() as u64 * record_bytes
    }

    /// Words of the packed vertex copy (`vertex_words` per source vertex).
    pub(super) fn packed_words(&self) -> usize {
        self.packed.len()
    }

    /// The finest band some cluster of an instance `distance` from the eye with stretch `scale`
    /// can select under `view` (conservative: the cull's level-run test per band, ADR-3915).
    pub(super) fn demand(&self, view: &ResidencyView, distance: f32, scale: f32) -> usize {
        self.demand_for(
            view.pixels_per_unit,
            view.threshold_px,
            Some(distance),
            view.near,
            scale,
        )
    }

    /// [`Self::demand`] for a shadow job (ADR-3708): `distance = None` is an orthographic job
    /// (the error projects without a distance divide, `vis_shadow.wgsl` `projected_error`), and
    /// `near` is the perspective job's nearest distance.
    pub(super) fn demand_shadow(
        &self,
        texels_per_unit: f32,
        threshold_px: f32,
        distance: Option<f32>,
        near: f32,
        scale: f32,
    ) -> usize {
        self.demand_for(texels_per_unit, threshold_px, distance, near, scale)
    }

    /// The conservative per-band demand test shared by the main view and the shadow jobs.
    fn demand_for(
        &self,
        pixels_per_unit: f32,
        threshold_px: f32,
        distance: Option<f32>,
        near: f32,
        scale: f32,
    ) -> usize {
        let k = pixels_per_unit;
        let t = threshold_px;
        let near = near.max(1e-4);
        // Orthographic: no distance divide. Perspective: to the instance's centre.
        let div = |reach: f32, add: bool| match distance {
            None => 1.0,
            Some(d) if add => (d + reach * scale).max(near),
            Some(d) => (d - reach * scale).max(near),
        };
        for (j, band) in self.bands.iter().enumerate() {
            let b = band.bounds;
            let finest = b.min_error * scale * k / div(b.reach, true);
            if finest > t * (1.0 + DEMAND_MARGIN) {
                continue;
            }
            if b.max_parent_error < ROOT_ERROR {
                let coarsest = b.max_parent_error * scale * k / div(b.parent_reach, false);
                if coarsest * (1.0 + DEMAND_MARGIN) < t {
                    continue;
                }
            }
            return j;
        }
        self.base()
    }

    /// The step-`step` cut threshold: resident clusters with `lod_error` at or under it are leaves.
    fn cut(&self, step: usize) -> f32 {
        if step == 0 {
            f32::NEG_INFINITY
        } else {
            self.thresholds[step - 1]
        }
    }

    /// Cluster `i`'s record at step `step`, its band's block at `starts[band]` when resident.
    pub(super) fn record(&self, src: &Meshlet, i: usize, step: usize, starts: &[Option<u32>]) -> Meshlet {
        let band = self.band_of[i] as usize;
        match starts.get(band).copied().flatten() {
            Some(start) if band >= step => {
                let (list, tris) = self.local[i];
                let (mut lod_error, parent_error) = finite_errors(src.lod_error, src.parent_error);
                if lod_error <= self.cut(step) {
                    lod_error = 0.0;
                }
                Meshlet {
                    vertex_offset: start + list,
                    triangle_offset: start + tris,
                    lod_error,
                    parent_error,
                    ..*src
                }
            }
            _ => Meshlet {
                lod_error: ABSENT_ERROR,
                vertex_offset: 0,
                triangle_offset: 0,
                vertex_count: 0,
                triangle_count: 0,
                ..*src
            },
        }
    }

    /// The geometry block of `band` placed at word `start`: its owned vertices get their addresses
    /// in `addr` (one per source vertex), and the vertex lists name the addresses of every vertex
    /// they use, which coarser resident bands have set.
    pub(super) fn block(&self, mesh: &MeshletMesh, band: usize, start: u32, addr: &mut [u32]) -> Vec<u32> {
        let b = &self.bands[band];
        let mut words = Vec::with_capacity(b.words as usize);
        self.block_head(band, start, addr, &mut words);
        for &i in &b.clusters {
            let m = &mesh.meshlets[i as usize];
            let list = mesh
                .vertices
                .get(m.vertex_offset as usize..(m.vertex_offset + m.vertex_count) as usize)
                .unwrap_or(&[]);
            words.extend(list.iter().map(|&v| addr.get(v as usize).copied().unwrap_or(0)));
        }
        for &i in &b.clusters {
            let m = &mesh.meshlets[i as usize];
            let tris = mesh
                .triangles
                .get(m.triangle_offset as usize..(m.triangle_offset + m.triangle_count) as usize)
                .unwrap_or(&[]);
            words.extend_from_slice(tris);
        }
        words.resize(b.words as usize, 0);
        words
    }

    /// [`Self::block`] from a delivered payload (ADR-3708): `lists`/`tris` are the band's
    /// clusters' sections concatenated in [`Band::clusters`] order.
    pub(super) fn block_from_parts(
        &self,
        band: usize,
        start: u32,
        addr: &mut [u32],
        lists: &[u32],
        tris: &[u32],
    ) -> Vec<u32> {
        let b = &self.bands[band];
        let mut words = Vec::with_capacity(b.words as usize);
        self.block_head(band, start, addr, &mut words);
        words.extend(lists.iter().map(|&v| addr.get(v as usize).copied().unwrap_or(0)));
        words.extend_from_slice(tris);
        words.resize(b.words as usize, 0);
        words
    }

    /// The owned-vertex head of `band`'s block at `start` (shared by [`Self::block`] and
    /// [`Self::block_from_parts`]).
    fn block_head(&self, band: usize, start: u32, addr: &mut [u32], words: &mut Vec<u32>) {
        let b = &self.bands[band];
        let vw = self.vertex_words as usize;
        for (n, &v) in b.vertices.iter().enumerate() {
            if let Some(slot) = addr.get_mut(v as usize) {
                *slot = start + (n * vw) as u32;
            }
            let at = v as usize * vw;
            match self.packed.get(at..at + vw) {
                Some(src) => words.extend_from_slice(src),
                None => words.resize(words.len() + vw, 0),
            }
        }
    }

    /// Bytes a read of `band` takes from the pak: its clusters' vertex lists and triangles. Its
    /// owned vertices are already in RAM (the plan's packed vertices), so they are not counted.
    pub(super) fn read_bytes(&self, band: usize) -> u64 {
        let b = &self.bands[band];
        let head = b.vertices.len() as u64 * u64::from(self.vertex_words);
        u64::from(b.words).saturating_sub(head) * 4
    }

    /// The band's clusters' vertex lists and triangles sliced out of the full sections
    /// (transient admission read of a stream-shaped mesh, ADR-3708).
    pub(super) fn band_parts(
        &self,
        meshlets: &[Meshlet],
        sections: &BandSections,
        band: usize,
    ) -> Option<(Vec<u32>, Vec<u32>)> {
        let mut lists = Vec::new();
        let mut tris = Vec::new();
        for &i in &self.bands[band].clusters {
            let m = meshlets.get(i as usize)?;
            lists.extend_from_slice(
                sections
                    .vertices
                    .get(m.vertex_offset as usize..(m.vertex_offset + m.vertex_count) as usize)?,
            );
            tris.extend_from_slice(
                sections
                    .triangles
                    .get(m.triangle_offset as usize..(m.triangle_offset + m.triangle_count) as usize)?,
            );
        }
        Some((lists, tris))
    }
}

/// A band being uploaded: its range, its block and how much of it is written.
pub(super) struct Pending {
    pub(super) band: usize,
    pub(super) key: ClusterKey,
    pub(super) start: u32,
    pub(super) block: Vec<u32>,
    pub(super) written: usize,
}

/// One residency-managed mesh.
pub(super) struct Managed {
    pub(super) plan: MeshPlan,
    /// The cooked mesh, kept in RAM (bands are cut from it; residency saves VRAM, not RAM). For
    /// a stream-shaped mesh (ADR-3708) `vertices`/`triangles` are empty and band payloads are
    /// read from `disk` on demand.
    pub(super) source: Arc<MeshletMesh>,
    /// The on-demand band payload source (`Some` for a stream-shaped mesh).
    pub(super) disk: Option<Arc<dyn BandDisk>>,
    /// Per band: its payload's RAM state (ADR-3708). Entries past the base are `Disk` until
    /// requested; the base is uploaded at admission and stays `Disk`.
    pub(super) band_ram: Vec<BandRam>,
    /// Per band: a read failed (`band_unreadable`, `band_corrupt`, `band_not_found`); sticky
    /// until the mesh is re-inserted, so a bad band does not spam the IO worker.
    pub(super) band_failed: Vec<bool>,
    /// Bytes the retained plan holds in RAM (packed vertices and cluster records).
    pub(super) plan_bytes: u64,
    /// Finest resident band: bands `step..` are resident.
    pub(super) step: usize,
    /// Per band: the start of its block when resident (the base's is in the mesh's own range).
    pub(super) starts: Vec<Option<u32>>,
    /// Per band finer than the base: its geometry range in the arena.
    pub(super) keys: Vec<Option<ClusterKey>>,
    /// Word address of every source vertex owned by a resident or uploading band.
    pub(super) addr: Vec<u32>,
    /// The records as uploaded.
    pub(super) records: Vec<Meshlet>,
    pub(super) pending: Option<Pending>,
    /// Per band: the last frame some visible instance needed it.
    pub(super) last_needed: Vec<u64>,
    /// This frame's finest needed band (the base when no instance is visible).
    pub(super) demand: usize,
    /// Largest projected radius of a visible instance, pixels (request order).
    pub(super) priority: f32,
    /// First cluster record in the arena.
    pub(super) cluster_base: u32,
    /// Bytes held on the GPU: records, resident bands, the band in flight.
    pub(super) bytes: u64,
}

impl Managed {
    /// Takes the payload of `band` out of RAM (back to [`BandRam::Disk`]), returning the
    /// concatenated vertex lists and triangles when it was delivered.
    pub(super) fn take_ram(&mut self, band: usize) -> Option<(Vec<u32>, Vec<u32>)> {
        match std::mem::replace(&mut self.band_ram[band], BandRam::Disk) {
            BandRam::Ram { lists, tris, .. } => Some((lists, tris)),
            other => {
                self.band_ram[band] = other;
                None
            }
        }
    }
}

/// The residency state of a [`super::VisBufferPass`].
#[derive(Default)]
pub(super) struct Residency {
    pub(super) managed: BTreeMap<u64, Managed>,
    /// Meshes whose base did not fit the budget, with the bytes that base needs (cleared
    /// whenever room is made, and per mesh once evictable bands could make room for it).
    pub(super) refused: BTreeMap<u64, u64>,
    /// Budget refusals `residency_retry_refused` lifted, with the bytes their base needs: the
    /// next admission makes that room before it reads a pak or builds a plan, and is refused
    /// again without either when it cannot (the evictions' records did not fit the frame's
    /// upload allowance; corr-render-visbuffer N).
    pub(super) retry_need: BTreeMap<u64, u64>,
    /// Meshes whose pak could not be read or whose base band is corrupt. Sticky until the mesh
    /// is re-inserted or released: making room does not make the pak readable, so these are
    /// kept apart from the budget refusals.
    pub(super) failed: BTreeSet<u64>,
    /// The profile's knobs (`set_profile_residency`).
    pub(super) profile: ClusterResidency,
    pub(super) view: Option<ResidencyView>,
    /// The shadow jobs' per-mesh demand recorded by the last shadow cull (`record_shadow_cull`):
    /// mesh key, finest needed band, priority (ADR-3708). Read by every `residency_frame` until
    /// the next cull replaces it, since a cached shadow map keeps that cut (ADR-7672).
    pub(super) shadow_needs: HashMap<u64, (usize, f32)>,
    pub(super) frame: u64,
    /// Upload bytes left this frame.
    pub(super) upload_left: u64,
    /// This frame's bound (`upload_bytes_per_frame`, or unbounded while off).
    pub(super) bound: u64,
    /// Read request bytes issued this frame (the IO bound, ADR-3708).
    pub(super) io_issued: u64,
    pub(super) resident_bytes: u64,
    /// Bytes the managed meshes hold in RAM (plans plus delivered payloads).
    pub(super) ram_bytes: u64,
    /// Read request serials and their (mesh key, band) (ADR-3708).
    pub(super) req_serial: u64,
    pub(super) req_map: HashMap<u64, (u64, usize)>,
    /// `cluster_base → mesh key` for every managed mesh, maintained at admit/forget so the two
    /// per-frame users (`residency_demand`, `residency_note_shadow_demand`) read it instead of
    /// rebuilding the same map every frame. A base collision keeps the later mesh key, as the
    /// rebuilt map did (insertion in mesh-key order, last wins).
    pub(super) by_base: HashMap<u32, u64>,
    pub(super) stats: VisBufferResidencyStats,
    /// Test hook: every eviction as (mesh, band, that mesh's demand at the time).
    #[cfg(test)]
    pub(super) eviction_log: Vec<(u64, usize, usize)>,
}

impl Residency {
    /// True when a record flip of `bytes` may upload now: it fits what is left of the frame's
    /// bound, or it is larger than the whole bound and nothing was uploaded yet this frame (a
    /// flip cannot be split; that frame goes over the bound once and counts it in
    /// [`VisBufferResidencyStats::bound_overruns`]).
    pub(super) fn fits(&self, bytes: u64) -> bool {
        bytes <= self.upload_left || (self.upload_left == self.bound && bytes > self.bound)
    }

    /// Counts the upload of `bytes` (already checked against [`Self::upload_left`]).
    pub(super) fn charge(&mut self, bytes: u64) {
        if bytes > self.upload_left {
            self.stats.bound_overruns += 1;
        }
        self.upload_left = self.upload_left.saturating_sub(bytes);
        self.stats.upload_bytes += bytes;
        self.stats.upload_bytes_total += bytes;
    }

    /// The managed mesh key owning `cluster_base`, when one holds it (`by_base`, maintained at
    /// admit/forget). A base collision keeps the later-admitted mesh key, as the rebuilt map did.
    pub(super) fn by_base(&self, cluster_base: u32) -> Option<u64> {
        self.by_base.get(&cluster_base).copied()
    }
}

/// Why a band payload could not be read from the cooked pak (ADR-3708). Codes are part of the
/// agent protocol (`book/agents/visbuffer-residency.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandDiskError {
    /// Stable code: `band_unreadable` (IO or decode), `band_corrupt` (a range or index check
    /// failed) or `band_not_found` (the pak or the payload no longer holds the asset).
    pub code: &'static str,
    /// Human detail.
    pub message: String,
}

impl std::fmt::Display for BandDiskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// The cooked vertex-list and triangle sections, read transiently (admission, materialize).
#[derive(Debug, Default)]
pub struct BandSections {
    /// Source-vertex index of every cluster-local vertex.
    pub vertices: Vec<u32>,
    /// One entry per triangle: three cluster-local vertex indices in bytes 0, 1 and 2.
    pub triangles: Vec<u32>,
}

/// One band's payload: the vertex lists and triangles of its clusters, concatenated in the
/// band's cluster order (the band's `MeshPlan` `clusters`).
#[derive(Debug)]
pub struct BandPayload {
    /// Vertex lists (source-vertex indices).
    pub lists: Vec<u32>,
    /// Triangles (cluster-local vertex indices).
    pub tris: Vec<u32>,
}

/// A finished band read ([`BandDisk::try_deliver`]): the request's tag and its outcome.
#[derive(Debug)]
pub struct BandDelivery {
    /// The caller's tag of [`BandDisk::request_band`].
    pub tag: u64,
    /// The payload, or why the read failed.
    pub result: Result<BandPayload, BandDiskError>,
}

/// On-demand band payload reads from the cooked pak (ADR-3708). A residency-managed mesh whose
/// `MeshletMesh` carries a `BandDisk` keeps its cluster lists and triangles out of RAM; the pass
/// asks for one band at a time and the implementation reads on an IO worker
/// (`regia_framework`'s `PakBandDisk` uses `regia_asset::PayloadReader` +
/// `regia_core::tasks::spawn_io`, the `regia_world` cell-source pair). Tests drive a memory
/// implementation.
pub trait BandDisk: Send + Sync {
    /// Reads the vertex-list and triangle sections now (admission's plan build and base block,
    /// and [`MeshletMesh::materialize`]). Blocking; the caller drops the result.
    fn read_sections(&self) -> Result<BandSections, BandDiskError>;

    /// Starts an asynchronous read of `clusters`' payload; the result arrives through
    /// [`Self::try_deliver`] in request order.
    fn request_band(&self, tag: u64, clusters: &[u32]);

    /// The next finished read, if one is ready (never blocks; call on the frame thread).
    fn try_deliver(&self) -> Option<BandDelivery>;
}

/// A band payload's RAM state (ADR-3708).
#[derive(Clone)]
pub(super) enum BandRam {
    /// Nothing in RAM (the band may also be resident on the GPU, or not loaded at all).
    Disk,
    /// A read is in flight on the disk's IO worker.
    Reading,
    /// The payload was delivered and is awaiting upload: the concatenated vertex lists and
    /// triangles, and when it arrived (the LRU stamp).
    Ram {
        /// The concatenated vertex lists of the band's clusters.
        lists: Vec<u32>,
        /// The concatenated triangles of the band's clusters.
        tris: Vec<u32>,
        /// The frame the payload arrived.
        arrived: u64,
    },
}

impl BandRam {
    /// Bytes the payload holds in RAM.
    pub(super) fn bytes(&self) -> u64 {
        match self {
            Self::Ram { lists, tris, .. } => (lists.len() + tris.len()) as u64 * 4,
            _ => 0,
        }
    }
}

/// Validates a delivered band payload against the cluster records: every cluster's counts fit
/// the parts, every list entry names a source vertex, and every triangle names a vertex inside
/// its cluster. `lists`/`tris` are the concatenated sections in `clusters` order. Returns the
/// `band_corrupt` reason.
pub(super) fn validate_band(
    meshlets: &[Meshlet],
    source_vertex_count: u32,
    clusters: &[u32],
    lists: &[u32],
    tris: &[u32],
) -> Result<(), &'static str> {
    let mut at_l = 0usize;
    let mut at_t = 0usize;
    for &c in clusters {
        let Some(m) = meshlets.get(c as usize) else {
            return Err("cluster index past the records");
        };
        let vc = m.vertex_count as usize;
        let tc = m.triangle_count as usize;
        let Some(list) = lists.get(at_l..at_l + vc) else {
            return Err("vertex list truncated");
        };
        if list.iter().any(|&v| v >= source_vertex_count) {
            return Err("vertex index past the source buffer");
        }
        let Some(tri) = tris.get(at_t..at_t + tc) else {
            return Err("triangles truncated");
        };
        for &packed in tri {
            for shift in [0u32, 8, 16] {
                if (packed >> shift) & 0xff >= vc as u32 {
                    return Err("triangle index past the cluster's vertices");
                }
            }
        }
        at_l += vc;
        at_t += tc;
    }
    if at_l != lists.len() || at_t != tris.len() {
        return Err("payload larger than the clusters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meshlet(level: u32, lod_error: f32, parent_error: f32, verts: &[u32]) -> (Meshlet, Vec<u32>) {
        (
            Meshlet {
                lod: [0.0, 0.0, 0.0, 1.0],
                parent: [0.0, 0.0, 0.0, 1.0],
                lod_error,
                parent_error,
                level,
                vertex_count: verts.len() as u32,
                triangle_count: 1,
                ..bytemuck::Zeroable::zeroed()
            },
            verts.to_vec(),
        )
    }

    /// A three-level DAG with a stuck level-0 cluster whose parent sits at level 2.
    fn three_level() -> MeshletMesh {
        let parts = [
            meshlet(0, 0.0, 1.0, &[0, 1, 2]),
            meshlet(0, 0.0, 1.0, &[2, 3, 4]),
            // Stuck at level 0: its parent group is built at level 2.
            meshlet(0, 0.0, 3.0, &[4, 5, 6]),
            meshlet(1, 1.0, 3.0, &[0, 2, 4]),
            meshlet(2, 3.0, ROOT_ERROR, &[0, 4, 6]),
        ];
        let mut mesh = MeshletMesh {
            source_vertex_count: 7,
            source_index_count: 3,
            bounds: [0.0, 0.0, 0.0, 1.0],
            ..MeshletMesh::default()
        };
        for (m, verts) in parts {
            mesh.meshlets.push(Meshlet {
                vertex_offset: mesh.vertices.len() as u32,
                triangle_offset: mesh.triangles.len() as u32,
                ..m
            });
            mesh.vertices.extend(verts);
            mesh.triangles.push(0x0002_0100);
        }
        mesh
    }

    #[test]
    fn bands_follow_parent_errors_and_vertices_go_to_the_coarsest_user() {
        let mesh = three_level();
        let packed: Vec<u32> = (0..7 * 2).collect();
        let plan = MeshPlan::build(&mesh, packed, 2).expect("three bands");
        assert_eq!(plan.thresholds, vec![1.0, 3.0]);
        assert_eq!(plan.band_count(), 3);
        // Parent error 1 → band 0; 3 → band 1; the root → band 2.
        assert_eq!(plan.band_of, vec![0, 0, 1, 1, 2]);
        // The root uses 0, 4, 6; band 1 uses 0, 2, 4, 5, 6 (not owned elsewhere: 2, 5); band 0
        // owns what nobody coarser uses.
        assert_eq!(plan.bands[2].vertices, vec![0, 4, 6]);
        assert_eq!(plan.bands[1].vertices, vec![2, 5]);
        assert_eq!(plan.bands[0].vertices, vec![1, 3]);
        // Block words: owned vertices, lists, triangles.
        assert_eq!(plan.bands[0].words, 2 * 2 + 6 + 2);
    }

    #[test]
    fn records_absent_resident_and_leaf_per_step() {
        let mesh = three_level();
        let plan = MeshPlan::build(&mesh, vec![0; 14], 2).expect("plan");
        let starts = [None, Some(100), Some(200)];
        let rec = |i: usize, step| plan.record(&mesh.meshlets[i], i, step, &starts);
        // Step 1: band 0 absent; the stuck level-0 cluster (e = 0) and the level-1 cluster (e = 1
        // ≤ T_1) are the cut's leaves; the root keeps its error.
        assert_eq!(rec(0, 1).lod_error, ABSENT_ERROR);
        assert_eq!(rec(0, 1).triangle_count, 0);
        assert_eq!(rec(2, 1).lod_error, 0.0);
        assert_eq!(rec(3, 1).lod_error, 0.0);
        assert_eq!(rec(4, 1).lod_error, 3.0);
        assert_eq!(rec(3, 1).vertex_offset, 100 + plan.local[3].0);
        // Step 2: only the root, a leaf.
        assert_eq!(rec(3, 2).lod_error, ABSENT_ERROR);
        assert_eq!(rec(4, 2).lod_error, 0.0);
        // Step 0 with every band placed: the cooked errors.
        let all = [Some(0), Some(100), Some(200)];
        for (i, m) in mesh.meshlets.iter().enumerate() {
            assert_eq!(plan.record(m, i, 0, &all).lod_error, m.lod_error);
        }
    }

    /// The resident set of every step, with its leaves, covers each DAG chain exactly once at any
    /// object-space threshold: the clamped cut has no hole and no overlap.
    #[test]
    fn every_step_cuts_each_chain_once() {
        let mesh = three_level();
        let plan = MeshPlan::build(&mesh, vec![0; 14], 2).expect("plan");
        let all = [Some(0), Some(100), Some(200)];
        // The chains of the fixture: leaf → ... → root.
        let chains: [&[usize]; 3] = [&[0, 3, 4], &[1, 3, 4], &[2, 4]];
        for step in 0..3 {
            for &t in &[0.0f32, 0.5, 1.0, 2.0, 3.0, 10.0] {
                for chain in chains {
                    let picked = chain
                        .iter()
                        .filter(|&&i| {
                            let r = plan.record(&mesh.meshlets[i], i, step, &all);
                            r.lod_error <= t && r.parent_error > t && r.lod_error != ABSENT_ERROR
                        })
                        .count();
                    assert_eq!(picked, 1, "step {step}, threshold {t}, chain {chain:?}");
                }
            }
        }
    }

    #[test]
    fn single_band_and_unordered_meshes_stay_whole() {
        let one = MeshletMesh::single_level(&[[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]], &[0, 1, 2]);
        assert!(MeshPlan::build(&one, vec![0; 6], 2).is_none());
        let mut unordered = three_level();
        unordered.meshlets.swap(0, 4);
        assert!(MeshPlan::build(&unordered, vec![0; 14], 2).is_none());
    }

    #[test]
    fn demand_moves_finer_as_the_instance_nears() {
        let mesh = three_level();
        let plan = MeshPlan::build(&mesh, vec![0; 14], 2).expect("plan");
        let view = ResidencyView {
            eye: [0.0; 3],
            planes: [[0.0, 0.0, 0.0, 1.0]; 6],
            pixels_per_unit: 100.0,
            near: 0.1,
            threshold_px: 1.0,
        };
        let far = plan.demand(&view, 1000.0, 1.0);
        let mid = plan.demand(&view, 150.0, 1.0);
        let near = plan.demand(&view, 2.0, 1.0);
        assert_eq!(far, 2, "far: the base");
        assert!(mid <= far && near <= mid, "{far} {mid} {near}");
        assert_eq!(near, 0, "near: the finest band");
    }

    /// A cooked cluster can carry a non-finite error (a bad simplification, a corrupt `.fmlt`
    /// word). The ladder already bins a NaN parent into the base band and folds a non-finite own
    /// error to 0. The uploaded record used to keep the raw values. `vis_cull.wgsl` `lod_decision`
    /// selects a cluster only when its own error is not above the threshold and its parent is
    /// (`parent_error >= ROOT`, `ROOT` = 3.0e38). NaN fails that comparison, and `+inf` never
    /// counts as fine enough, so the chain's cover is skipped and the surface has a hole.
    #[test]
    fn non_finite_errors_stay_selectable() {
        let mut mesh = three_level();
        mesh.meshlets[4].parent_error = f32::NAN;
        mesh.meshlets[0].lod_error = f32::INFINITY;
        let plan = MeshPlan::build(&mesh, vec![0; 14], 2).expect("non-finite errors still plan");
        assert_eq!(
            plan.band_of[4] as usize,
            plan.base(),
            "a NaN parent is binned with the roots"
        );
        let starts = vec![Some(10u32); plan.band_count()];
        let root = plan.record(&mesh.meshlets[4], 4, plan.base(), &starts);
        assert_ne!(root.vertex_count, 0, "the base cluster stays resident");
        assert!(
            root.parent_error >= ROOT_ERROR,
            "uploaded parent {} is not a root; the cull drops the cluster",
            root.parent_error
        );
        assert_eq!(root.lod_error, 0.0, "the base cut makes the root a leaf");
        // Step 0 keeps cooked errors, except a non-finite own error, which the ladder folded to 0.
        // `+inf <= T` is false for every finite T, so the leaf and its parent (error 1) are both
        // rejected and that chain draws nothing.
        let leaf = plan.record(&mesh.meshlets[0], 0, 0, &starts);
        assert_eq!(leaf.lod_error, 0.0, "a non-finite own error uploads as 0");
        assert!(
            leaf.lod_error <= 0.5 && leaf.parent_error > 0.5,
            "the leaf must be the cut at threshold 0.5; parent {}",
            leaf.parent_error
        );
        // A finite negative parent stays: it sorted into the finest band, and "parent always
        // finer" means this cluster is not the cover. Only NaN is rewritten to the root sentinel.
        let (lod, parent) = finite_errors(f32::NEG_INFINITY, f32::NEG_INFINITY);
        assert_eq!(lod, 0.0);
        assert_eq!(parent, f32::NEG_INFINITY);
        let (lod, parent) = finite_errors(1.5, f32::INFINITY);
        assert_eq!(lod, 1.5);
        assert_eq!(parent, f32::INFINITY, "+inf already passes the root comparison");
    }

    #[test]
    fn tier_budgets_grow_with_the_tier() {
        let deck = ClusterResidency::for_tier(BudgetTier::Deck);
        let min = ClusterResidency::for_tier(BudgetTier::Minimum);
        let high = ClusterResidency::for_tier(BudgetTier::DesktopHigh);
        assert!(deck.enabled && deck.budget_bytes < min.budget_bytes && min.budget_bytes < high.budget_bytes);
        let floor = ClusterResidency::for_tier(BudgetTier::IntegratedLow);
        assert!(
            floor.enabled
                && floor.budget_bytes < deck.budget_bytes
                && floor.ram_budget_bytes < deck.ram_budget_bytes
        );
        assert!(deck.upload_bytes_per_frame < high.upload_bytes_per_frame);
        assert_eq!(ClusterResidency::default(), ClusterResidency::OFF);
        assert!(
            ClusterResidency::for_preset(RenderProfilePreset::MobileLow).budget_bytes < deck.budget_bytes
        );
    }
}
