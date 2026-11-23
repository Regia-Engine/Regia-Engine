// SPDX-License-Identifier: MPL-2.0
//! Morph targets (blend shapes) applied on the GPU (Tincal engine item B1, `specs/13` § 8,
//! ADR-241).
//!
//! A mesh's sparse targets (cooked mesh format v2: per target the moved vertices and their
//! position, normal and tangent deltas) become one [`MorphTargets`] set, shared by every instance
//! of the mesh through an `Arc`. An instance's weights come from its [`MorphInstance`]; the
//! runtime extracts them from [`MeshMorphTargets`] and the entity's
//! [`regia_scene::components::MorphWeights`] ([`extract_morphs`]).
//!
//! Each frame, an instance whose weights changed is deformed by a compute pre-pass
//! (`shaders/morph.wgsl`): one invocation per moved vertex sums `weight × delta` over the
//! targets that move it (a gather, so no two invocations write one vertex), renormalizes the
//! normal and the tangent, and writes the result into the instance's own copy of the mesh. That
//! copy is an ordinary mesh-cache entry under a key of its own, so every pass (depth, shadows,
//! colour, motion vectors, GPU-driven instancing) draws the deformed vertices, and vertex-shader
//! skinning skins them. Nothing reads a storage buffer in a vertex shader (tile GPUs), the object
//! block and its render-override lanes are untouched, and no buffer is created per frame: an
//! instance's buffers are made once, when it first draws.
//!
//! - [`MorphMode::Game`] keeps the [`MORPH_GAME_MAX_ACTIVE`] largest weights (by magnitude) and
//!   zeroes the rest; [`MorphMode::Creator`] keeps every weight.
//! - Instances whose weights did not change cost nothing on the GPU.
//! - [`MorphTargets::apply`] is the CPU reference (and the fallback for hosts without compute):
//!   the same sums in the same order.
//!
//! The data layout and the gather are Regia Engine-original (`docs/provenance.md`); the delta
//! semantics are glTF 2.0 § 3.7.2.2 (morph targets).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_app::prelude::*;

use crate::lab_frame::LabVertex;
use crate::runtime::ExtractedFrame;

/// Most weights a [`MorphMode::Game`] instance applies (the largest by magnitude).
pub const MORPH_GAME_MAX_ACTIVE: usize = 16;

/// Top byte of the mesh-cache keys the renderer gives morphed instances' deformed copies
/// (`'m'`). Like [`crate::EPHEMERAL_KEY`] and [`crate::SHADOW_LOD_MESH_TAG`], hosts that build
/// [`crate::FrameMesh`]es themselves must not use keys with this top byte.
pub const MORPH_MESH_KEY_TAG: u64 = 0x6D00_0000_0000_0000;

/// `u32` words of one moved-vertex row in the GPU layout.
pub(crate) const ROW_WORDS: usize = 16;
/// `u32` words of one (vertex, target) entry in the GPU layout.
pub(crate) const ENTRY_WORDS: usize = 6;

/// How many of an instance's weights apply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MorphMode {
    /// The [`MORPH_GAME_MAX_ACTIVE`] largest weights (by magnitude, lower target index first on
    /// a tie); the rest read as zero. Expressions and visemes in a shipped game.
    #[default]
    Game,
    /// Every weight (a character creator with hundreds of active targets).
    Creator,
}

/// Why a [`MorphTargets`] set could not be built.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum MorphTargetsError {
    /// A target's index list or delta counts do not match the mesh.
    #[error("morph target {name:?}: {reason}")]
    InvalidTarget {
        /// Target name.
        name: String,
        /// What is wrong.
        reason: String,
    },
    /// The mesh or the target data is too large for the GPU layout (`u32` offsets).
    #[error("morph targets too large: {0}")]
    TooLarge(String),
}

/// The sparse morph targets of one mesh, packed for the GPU (see the module docs).
///
/// Built once per mesh ([`Self::new`]) and shared through an `Arc` by every instance. Deltas
/// are stored as `f16` (the cooked format's precision): values that came from a cooked mesh are
/// kept exactly, others are rounded to nearest even.
pub struct MorphTargets {
    /// Process-unique identity (the renderer's GPU cache key).
    id: u64,
    names: Vec<String>,
    vertex_count: u32,
    /// First moved vertex and one past the last (`[0, 0]` when nothing moves).
    span: [u32; 2],
    row_count: u32,
    entry_count: u32,
    /// Rows (`ROW_WORDS` each: vertex, entry range, base position, normal, tangent), then
    /// entries (`ENTRY_WORDS` each: target, `f16` position, normal and tangent deltas).
    words: Vec<u32>,
    /// Per target: the longest position delta, metres (cull-bound growth).
    max_delta: Vec<f32>,
}

impl std::fmt::Debug for MorphTargets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MorphTargets")
            .field("targets", &self.names.len())
            .field("vertex_count", &self.vertex_count)
            .field("moved_vertices", &self.row_count)
            .field("entries", &self.entry_count)
            .finish_non_exhaustive()
    }
}

/// `[x, y]` as two `f16` halves in one word (`halves` in `morph.wgsl`: `x` in the low half).
fn pack_halves(x: f32, y: f32) -> u32 {
    u32::from(regia_asset::f32_to_f16_round(x)) | (u32::from(regia_asset::f32_to_f16_round(y)) << 16)
}

/// The low (`x`) and high (`y`) halves of a word.
fn unpack_halves(word: u32) -> [f32; 2] {
    [
        regia_asset::f16_bits_to_f32((word & 0xffff) as u16),
        regia_asset::f16_bits_to_f32((word >> 16) as u16),
    ]
}

impl MorphTargets {
    /// Packs `targets` (in target-index order) against the mesh `base` they deform.
    ///
    /// The base positions, normals and tangents of the moved vertices are stored with the
    /// deltas (the GPU pass reads nothing else), so `base` must be the vertex array the
    /// instances draw (`crate::CpuMesh::vertices`, one vertex per cooked vertex).
    ///
    /// # Errors
    ///
    /// [`MorphTargetsError::InvalidTarget`] when a target's indices are out of range or not
    /// strictly increasing, or its delta counts do not match; [`MorphTargetsError::TooLarge`]
    /// past `u32` offsets.
    pub fn new(
        base: &[LabVertex],
        targets: &[regia_asset::CpuMorphTarget],
    ) -> Result<Self, MorphTargetsError> {
        let vertex_count =
            u32::try_from(base.len()).map_err(|_| MorphTargetsError::TooLarge("vertex count".into()))?;
        let mut per_vertex = vec![0u32; base.len()];
        let mut max_delta = Vec::with_capacity(targets.len());
        for target in targets {
            target
                .validate(base.len())
                .map_err(|error| MorphTargetsError::InvalidTarget {
                    name: target.name.clone(),
                    reason: error.to_string(),
                })?;
            for &vertex in &target.vertex_indices {
                per_vertex[vertex as usize] += 1;
            }
            let longest = target
                .position_deltas
                .iter()
                .map(|d| {
                    let rounded = unpack_halves(pack_halves(d[0], d[1]));
                    let z = unpack_halves(pack_halves(d[2], 0.0))[0];
                    (rounded[0] * rounded[0] + rounded[1] * rounded[1] + z * z).sqrt()
                })
                .fold(0.0f32, |a, b| if b.is_finite() { a.max(b) } else { a });
            max_delta.push(longest);
        }
        // Rows: every vertex at least one target lists, ascending.
        let moved: Vec<u32> = (0..vertex_count)
            .filter(|&v| per_vertex[v as usize] > 0)
            .collect();
        let entry_count: usize = per_vertex.iter().map(|&n| n as usize).sum();
        let row_count = moved.len();
        let words_len = row_count
            .checked_mul(ROW_WORDS)
            .and_then(|rows| {
                entry_count
                    .checked_mul(ENTRY_WORDS)
                    .and_then(|e| e.checked_add(rows))
            })
            .filter(|&n| u32::try_from(n).is_ok())
            .ok_or_else(|| MorphTargetsError::TooLarge(format!("{entry_count} vertex deltas")))?;
        let mut words = vec![0u32; words_len.max(1)];
        // Entry ranges per row (CSR), in vertex order.
        let mut cursor = vec![0u32; base.len()];
        let mut next = 0u32;
        for (row, &vertex) in moved.iter().enumerate() {
            let v = &base[vertex as usize];
            let count = per_vertex[vertex as usize];
            let r = row * ROW_WORDS;
            words[r] = vertex;
            words[r + 1] = next;
            words[r + 2] = next + count;
            for (i, value) in v.position.iter().chain(&v.normal).chain(&v.tangent).enumerate() {
                words[r + 4 + i] = value.to_bits();
            }
            cursor[vertex as usize] = next;
            next += count;
        }
        // Entries: targets in index order, so a row's entries are in target order.
        let entry_base = row_count * ROW_WORDS;
        for (t, target) in targets.iter().enumerate() {
            let zero = [0.0f32; 3];
            for (i, &vertex) in target.vertex_indices.iter().enumerate() {
                let p = target.position_deltas[i];
                let n = target.normal_deltas.get(i).copied().unwrap_or(zero);
                let tg = target.tangent_deltas.get(i).copied().unwrap_or(zero);
                let slot = cursor[vertex as usize];
                cursor[vertex as usize] += 1;
                let e = entry_base + slot as usize * ENTRY_WORDS;
                words[e] = t as u32;
                words[e + 1] = pack_halves(p[0], p[1]);
                words[e + 2] = pack_halves(p[2], n[0]);
                words[e + 3] = pack_halves(n[1], n[2]);
                words[e + 4] = pack_halves(tg[0], tg[1]);
                words[e + 5] = pack_halves(tg[2], 0.0);
            }
        }
        let span = match (moved.first(), moved.last()) {
            (Some(&first), Some(&last)) => [first, last + 1],
            _ => [0, 0],
        };
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Ok(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            names: targets.iter().map(|t| t.name.clone()).collect(),
            vertex_count,
            span,
            row_count: row_count as u32,
            entry_count: entry_count as u32,
            words,
            max_delta,
        })
    }

    /// The set of a cooked mesh drawn as `render` (`crate::CpuMesh::from_cooked` of `cooked`);
    /// `None` when the cooked mesh has no morph targets.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn from_cooked(
        render: &crate::CpuMesh,
        cooked: &regia_asset::CpuMesh,
    ) -> Result<Option<Arc<Self>>, MorphTargetsError> {
        if cooked.morph_targets.is_empty() {
            return Ok(None);
        }
        Self::new(&render.vertices, &cooked.morph_targets).map(|t| Some(Arc::new(t)))
    }

    /// Process-unique identity of this set.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Target names, target-index order.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The index of the target named `name`.
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// Number of targets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether the set has no targets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Vertices of the mesh the set deforms.
    #[must_use]
    pub fn vertex_count(&self) -> u32 {
        self.vertex_count
    }

    /// Vertices moved by at least one target (one GPU invocation each).
    #[must_use]
    pub fn moved_vertex_count(&self) -> u32 {
        self.row_count
    }

    /// `(vertex, target)` delta entries.
    #[must_use]
    pub fn entry_count(&self) -> u32 {
        self.entry_count
    }

    /// First moved vertex and one past the last: the range an instance's deformed copy rewrites.
    #[must_use]
    pub fn moved_span(&self) -> std::ops::Range<u32> {
        self.span[0]..self.span[1]
    }

    /// Bytes of the packed GPU data (uploaded once per set).
    #[must_use]
    pub fn gpu_bytes(&self) -> u64 {
        self.words.len() as u64 * 4
    }

    /// The packed GPU data (`shaders/morph.wgsl` layout).
    pub(crate) fn words(&self) -> &[u32] {
        &self.words
    }

    /// First word of the entries in [`Self::words`].
    pub(crate) fn entry_base_word(&self) -> u32 {
        self.row_count * ROW_WORDS as u32
    }

    /// The weights that apply under `mode`: one per target (missing ones read as zero, extra
    /// ones are ignored), non-finite values as zero, and under [`MorphMode::Game`] only the
    /// [`MORPH_GAME_MAX_ACTIVE`] largest by magnitude (lower index first on a tie).
    #[must_use]
    pub fn effective_weights(&self, weights: &[f32], mode: MorphMode) -> Vec<f32> {
        let mut out: Vec<f32> = (0..self.len())
            .map(|i| weights.get(i).copied().filter(|w| w.is_finite()).unwrap_or(0.0))
            .collect();
        let active = out.iter().filter(|w| **w != 0.0).count();
        if mode == MorphMode::Game && active > MORPH_GAME_MAX_ACTIVE {
            let mut order: Vec<usize> = (0..out.len()).filter(|&i| out[i] != 0.0).collect();
            order.sort_by(|&a, &b| out[b].abs().total_cmp(&out[a].abs()).then(a.cmp(&b)));
            for &dropped in &order[MORPH_GAME_MAX_ACTIVE..] {
                out[dropped] = 0.0;
            }
        }
        out
    }

    /// The farthest any vertex can move from its base position with `weights` (effective
    /// weights, [`Self::effective_weights`]): `Σ |w| × longest delta`, metres. The renderer grows
    /// a morphed instance's cull bound by it.
    #[must_use]
    pub fn max_displacement(&self, weights: &[f32]) -> f32 {
        self.max_delta.iter().zip(weights).map(|(d, w)| d * w.abs()).sum()
    }

    /// CPU reference of the GPU pass: deforms `vertices` (a copy of the base the set was built
    /// against) in place with `weights` (effective weights, [`Self::effective_weights`]).
    ///
    /// Moved vertices get `base + Σ w × delta` (targets in index order, zero weights skipped);
    /// a normal or tangent that changed is renormalized (the tangent keeps its handedness `w`).
    /// Other vertices and attributes are left as they are.
    ///
    /// # Panics
    ///
    /// When `vertices` is shorter than [`Self::vertex_count`].
    pub fn apply(&self, weights: &[f32], vertices: &mut [LabVertex]) {
        assert!(vertices.len() >= self.vertex_count as usize, "vertex count");
        let entry_base = self.entry_base_word() as usize;
        let w = &self.words;
        let f = |word: u32| f32::from_bits(word);
        for row in 0..self.row_count as usize {
            let r = row * ROW_WORDS;
            let vertex = w[r] as usize;
            let base_p = [f(w[r + 4]), f(w[r + 5]), f(w[r + 6])];
            let base_n = [f(w[r + 7]), f(w[r + 8]), f(w[r + 9])];
            let base_t = [f(w[r + 10]), f(w[r + 11]), f(w[r + 12])];
            let (mut p, mut n, mut t) = (base_p, base_n, base_t);
            for e in w[r + 1]..w[r + 2] {
                let e = entry_base + e as usize * ENTRY_WORDS;
                let weight = weights.get(w[e] as usize).copied().unwrap_or(0.0);
                if weight == 0.0 {
                    continue;
                }
                let a = unpack_halves(w[e + 1]);
                let b = unpack_halves(w[e + 2]);
                let c = unpack_halves(w[e + 3]);
                let d = unpack_halves(w[e + 4]);
                let g = unpack_halves(w[e + 5]);
                for (acc, delta) in [
                    (&mut p, [a[0], a[1], b[0]]),
                    (&mut n, [b[1], c[0], c[1]]),
                    (&mut t, [d[0], d[1], g[0]]),
                ] {
                    for k in 0..3 {
                        acc[k] += weight * delta[k];
                    }
                }
            }
            let out = &mut vertices[vertex];
            out.position = p;
            out.normal = renormalized(n, base_n);
            let t = renormalized(t, base_t);
            out.tangent = [t[0], t[1], t[2], out.tangent[3]];
        }
    }
}

/// `v` normalized when it differs from `base` and has a length; otherwise `base` unchanged
/// (the shader's rule).
fn renormalized(v: [f32; 3], base: [f32; 3]) -> [f32; 3] {
    if v == base {
        return base;
    }
    let len2 = v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
    if len2 > 1e-12 {
        let inv = 1.0 / len2.sqrt();
        [v[0] * inv, v[1] * inv, v[2] * inv]
    } else {
        base
    }
}

/// One morphed instance of a frame: the object it draws as ([`crate::FrameMesh::object_key`]),
/// its mesh's targets and its weights. Pass the frame's instances to
/// [`crate::FrameRenderer::set_morph_instances`].
#[derive(Clone, Debug)]
pub struct MorphInstance {
    /// [`crate::FrameMesh::object_key`] of the draw (the runtime's is
    /// `entity.to_bits() ^ object_key_xor`).
    pub object_key: u64,
    /// The targets of the mesh the object draws.
    pub targets: Arc<MorphTargets>,
    /// Weights, target-index order (missing ones read as zero).
    pub weights: Vec<f32>,
    /// How many weights apply.
    pub mode: MorphMode,
}

/// The morph targets of a [`crate::MeshRenderer`] entity's mesh. With a
/// [`regia_scene::components::MorphWeights`] on the same entity, [`extract_morphs`] submits the
/// instance; without one the mesh draws undeformed.
#[derive(Component, Clone, Debug)]
pub struct MeshMorphTargets {
    /// The packed targets (shared by every instance of the mesh).
    pub targets: Arc<MorphTargets>,
    /// How many weights apply.
    pub mode: MorphMode,
}

impl MeshMorphTargets {
    /// Targets drawn in [`MorphMode::Game`].
    #[must_use]
    pub fn new(targets: Arc<MorphTargets>) -> Self {
        Self {
            targets,
            mode: MorphMode::Game,
        }
    }
}

/// `PreUpdate`: gives an entity whose [`MeshMorphTargets`] were added or replaced the matching
/// [`regia_scene::components::MorphTargetNames`], so animation and agents can address its
/// targets by name. Names the entity already has that match are left alone.
pub fn name_morph_targets(
    mut commands: Commands,
    meshes: Query<
        (
            Entity,
            &MeshMorphTargets,
            Option<&regia_scene::components::MorphTargetNames>,
        ),
        Changed<MeshMorphTargets>,
    >,
) {
    for (entity, targets, names) in &meshes {
        let same = names.is_some_and(|n| n.0.iter().eq(targets.targets.names()));
        if !same {
            commands
                .entity(entity)
                .insert(regia_scene::components::MorphTargetNames::new(
                    targets.targets.names().iter().cloned(),
                ));
        }
    }
}

/// The renderer's morph counters ([`crate::FrameRenderer::morph_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MorphStats {
    /// Instances set ([`crate::FrameRenderer::set_morph_instances`]).
    pub instances: usize,
    /// Instances the last frame drew deformed.
    pub drawn: usize,
    /// Instances the last frame deformed on the GPU (their weights changed).
    pub dispatched: usize,
    /// Moved vertices those dispatches covered.
    pub dispatched_vertices: u64,
    /// Instances skipped because their mesh does not have the targets' vertex count.
    pub mismatched: usize,
    /// Instances waiting for the morph pipeline (built in the background mid-play).
    pub waiting_pipeline: usize,
    /// Bytes of morph data, per-instance weights and deformed spans resident on the GPU.
    pub gpu_bytes: u64,
    /// Buffers created for morphs since the renderer started (once per set and instance, and two
    /// motion copies when an instance's weights start changing).
    pub buffers_created: u64,
    /// Instances the last frame drew with motion vectors: their weights changed from a
    /// deformation already drawn (ADR-2652).
    pub motion_instances: usize,
    /// Of [`Self::drawn`], the instances drawn from a counted mesh's upload: their deformed copy
    /// is a GPU copy of the adopted vertex buffer, with no CPU vertices (ADR-7721).
    pub counted: usize,
}

/// `Render`: adds the frame's morphed instances to [`ExtractedFrame::morphs`]: every extracted
/// mesh whose entity has [`MeshMorphTargets`] and [`regia_scene::components::MorphWeights`].
pub fn extract_morphs(
    mut extracted: ResMut<ExtractedFrame>,
    morphs: Query<(&MeshMorphTargets, &regia_scene::components::MorphWeights)>,
) {
    let _span = tracing::debug_span!("extract_morphs").entered();
    let frame = &mut *extracted;
    frame.morphs.clear();
    if morphs.is_empty() {
        return;
    }
    for mesh in &frame.meshes {
        let Ok((targets, weights)) = morphs.get(mesh.entity) else {
            continue;
        };
        frame.morphs.push(MorphInstance {
            object_key: mesh.entity.to_bits() ^ mesh.object_key_xor,
            targets: Arc::clone(&targets.targets),
            weights: weights.0.clone(),
            mode: targets.mode,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(n: usize) -> Vec<LabVertex> {
        (0..n)
            .map(|i| LabVertex::from_pos_normal_uv([i as f32 * 0.1, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]))
            .collect()
    }

    fn target(name: &str, indices: Vec<u32>, delta: [f32; 3]) -> regia_asset::CpuMorphTarget {
        let n = indices.len();
        regia_asset::CpuMorphTarget {
            name: name.into(),
            vertex_indices: indices,
            position_deltas: vec![delta; n],
            normal_deltas: vec![[0.5, 0.0, 0.0]; n],
            tangent_deltas: Vec::new(),
        }
    }

    #[test]
    fn rows_are_the_union_of_moved_vertices_in_order() {
        let base = grid(10);
        let set = MorphTargets::new(
            &base,
            &[
                target("a", vec![2, 5], [0.0, 1.0, 0.0]),
                target("b", vec![5, 7], [1.0, 0.0, 0.0]),
            ],
        )
        .expect("set");
        assert_eq!(set.moved_vertex_count(), 3);
        assert_eq!(set.entry_count(), 4);
        assert_eq!(set.moved_span(), 2..8);
        assert_eq!(set.index_of("b"), Some(1));
        assert!((set.max_displacement(&[0.5, -2.0]) - 2.5).abs() < 1e-6);
    }

    #[test]
    fn cpu_reference_sums_weighted_deltas_and_renormalizes() {
        let base = grid(10);
        let set = MorphTargets::new(
            &base,
            &[
                target("a", vec![2, 5], [0.0, 1.0, 0.0]),
                target("b", vec![5, 7], [1.0, 0.0, 0.0]),
            ],
        )
        .expect("set");
        let mut out = base.clone();
        set.apply(&[0.5, 0.25], &mut out);
        assert_eq!(out[2].position, [0.2, 0.5, 0.0]);
        let p5 = out[5].position;
        assert!(
            (p5[0] - 0.75).abs() < 1e-6 && (p5[1] - 0.5).abs() < 1e-6,
            "{p5:?}"
        );
        // normal (0,0,1) + 0.75 × (0.5,0,0): renormalized.
        let n = out[5].normal;
        assert!(((n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt() - 1.0).abs() < 1e-6);
        assert!(n[0] > 0.3);
        assert_eq!(out[3].position, base[3].position, "unmoved vertex");
        assert_eq!(out[2].tangent, base[2].tangent, "no tangent deltas: tangent kept");
        let mut zero = base.clone();
        set.apply(&[0.0, 0.0], &mut zero);
        assert_eq!(zero[5].position, base[5].position);
        assert_eq!(zero[5].normal, base[5].normal);
    }

    #[test]
    fn game_mode_keeps_the_largest_weights() {
        let base = grid(40);
        let targets: Vec<_> = (0..20)
            .map(|i| target(&format!("t{i}"), vec![i], [0.0, 1.0, 0.0]))
            .collect();
        let set = MorphTargets::new(&base, &targets).expect("set");
        let weights: Vec<f32> = (0..20).map(|i| i as f32 * 0.05).collect();
        let game = set.effective_weights(&weights, MorphMode::Game);
        assert_eq!(game.iter().filter(|w| **w != 0.0).count(), MORPH_GAME_MAX_ACTIVE);
        // 19 non-zero weights (index 0 is zero): the three smallest (1–3) are dropped.
        assert_eq!(&game[1..4], &[0.0; 3], "smallest dropped");
        assert_eq!(game[4], weights[4]);
        assert_eq!(game[19], weights[19]);
        let creator = set.effective_weights(&weights, MorphMode::Creator);
        assert_eq!(creator, weights);
        let short = set.effective_weights(&[f32::NAN, 1.0], MorphMode::Creator);
        assert_eq!(short.len(), 20);
        assert_eq!((short[0], short[1], short[2]), (0.0, 1.0, 0.0));
    }

    #[test]
    fn invalid_targets_are_refused() {
        let base = grid(4);
        let error =
            MorphTargets::new(&base, &[target("far", vec![9], [1.0, 0.0, 0.0])]).expect_err("refused");
        assert!(
            matches!(error, MorphTargetsError::InvalidTarget { .. }),
            "{error}"
        );
    }

    #[test]
    fn deltas_round_to_f16() {
        let base = grid(2);
        let set = MorphTargets::new(&base, &[target("a", vec![1], [0.1, 0.0, 0.0])]).expect("set");
        let mut out = base.clone();
        set.apply(&[1.0], &mut out);
        let half = regia_asset::f16_bits_to_f32(regia_asset::f32_to_f16_round(0.1));
        assert_eq!(out[1].position[0], 0.1 + half);
    }
}
