// SPDX-License-Identifier: MPL-2.0
//! Casters outside the frame's mesh list, and the traced scene's change classes (ADR-2310).
//!
//! The sun trace of ADR-2035 built its scene from the frame's draws only, so a frame with a shared
//! draw (the terrain) or persistent instance sets (scatter) fell back to the screen-space trace.
//! This module holds the CPU half of what closes that gap, every part of it testable without a GPU:
//!
//! - [`TraceCasterMesh`] and [`TraceCasterInstance`]: geometry a host hands the trace as plain
//!   positions and indices. A shared draw gives them through
//!   [`crate::SharedPassDraw::trace_casters`] ([`SharedTraceCasters`]); persistent sets keep a
//!   copy of their mesh when they are installed. The renderer uploads each mesh once per
//!   generation into buffers the acceleration-structure build can read, and feeds the triangles
//!   to the compute BVH on the reference path.
//! - [`SceneSignature`] and [`classify_scene_change`]: whether a frame's casters changed at all,
//!   only in their transforms (a refit: the same instances in the same slots, new matrices), or
//!   in their structure (a rebuild).
//! - The masked-caster proxy: an alpha-masked caster enters the ray-query scene as non-opaque
//!   geometry whose hits are accepted with a fixed coverage ([`MASKED_COVERAGE`]), so a canopy
//!   casts a dappled shadow of roughly the right density instead of none.
//! - [`sun_map_keep_mask`]: which of the sun's shadow cascades a traced frame still needs.

use std::sync::Arc;

/// XOR tag on a shared draw's caster mesh keys, so they never meet a mesh-list key.
pub const SHARED_CASTER_TAG: u64 = 0x5348_0000_0000_0000;
/// XOR tag on persistent instance sets' mesh keys in the traced scene.
pub const SET_CASTER_TAG: u64 = 0x5345_0000_0000_0000;
/// XOR tag on the non-opaque structure of an alpha-masked mesh (its opaque structure, if the
/// same mesh is also drawn opaque, keeps the plain key).
pub const MASKED_CASTER_TAG: u64 = 0x4d41_0000_0000_0000;

/// Instance mask of opaque casters.
pub const OPAQUE_INSTANCE_MASK: u8 = 0x01;
/// Instance mask of alpha-masked casters.
pub const MASKED_INSTANCE_MASK: u8 = 0x02;

/// Fraction of rays an alpha-masked caster's triangles stop: the proxy's documented coverage. A
/// cut-out card has no texture lookup in the ray-query module (that needs a per-instance material
/// table, ADR-2310 "not done"), so its hits are accepted by a hash of the ray and the triangle
/// against this value. Foliage cards are typically half covered.
pub const MASKED_COVERAGE: f32 = 0.5;

/// The 8-bit coverage a masked instance carries in its custom data (`0..=255`, 255 = opaque).
#[must_use]
pub fn coverage_custom_data(coverage: f32) -> u32 {
    (coverage.clamp(0.0, 1.0) * 255.0).round() as u32
}

/// One caster mesh outside the frame's mesh list: world or object positions and a triangle list.
#[derive(Debug, Clone)]
pub struct TraceCasterMesh {
    /// Identity within its source (a shared draw's tile, a set's mesh key).
    pub key: u64,
    /// Content version: a different value re-uploads and rebuilds this mesh's structure.
    pub generation: u64,
    /// Vertex positions, metres.
    pub positions: Arc<[[f32; 3]]>,
    /// Triangle indices into [`Self::positions`] (a multiple of 3). Meshes may share one list
    /// (`Arc::ptr_eq`): the renderer uploads a shared list once.
    pub indices: Arc<[u32]>,
}

impl TraceCasterMesh {
    /// Triangles in the mesh.
    #[must_use]
    pub fn triangle_count(&self) -> u32 {
        (self.indices.len() / 3) as u32
    }

    /// Whether the mesh is a well-formed triangle list: indices a non-empty multiple of 3, each
    /// in range, and every position finite.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.indices.is_empty()
            && self.indices.len().is_multiple_of(3)
            && self.indices.iter().all(|&i| (i as usize) < self.positions.len())
            && self.positions.iter().flatten().all(|c| c.is_finite())
    }

    /// The mesh's triangles through `transform` (column-major), in index order.
    pub fn world_triangles(&self, transform: &[[f32; 4]; 4]) -> impl Iterator<Item = [[f32; 3]; 3]> + '_ {
        let m = regia_core::glam::Mat4::from_cols_array_2d(transform);
        let (triangles, _) = self.indices.as_chunks::<3>();
        triangles.iter().filter_map(move |t| {
            let p = |i: u32| {
                self.positions.get(i as usize).map(|v| {
                    m.transform_point3(regia_core::glam::Vec3::from_array(*v))
                        .to_array()
                })
            };
            Some([p(t[0])?, p(t[1])?, p(t[2])?])
        })
    }
}

/// One placement of a [`TraceCasterMesh`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceCasterInstance {
    /// [`TraceCasterMesh::key`] of the placed mesh.
    pub mesh: u64,
    /// Object-to-world matrix, column-major.
    pub transform: [[f32; 4]; 4],
}

/// The casters a shared draw adds to the traced scene ([`crate::SharedPassDraw::trace_casters`]).
///
/// Positions are in the draw's world-anchored space, the space
/// [`crate::SharedFrameInfo::in_world_space`] describes: the renderer moves every instance by
/// `-origin_offset` into the frame's space, so a floating-origin rebase moves no caster data.
#[derive(Debug, Clone, Copy)]
pub struct SharedTraceCasters<'a> {
    /// Every mesh the instances reference (keys unique).
    pub meshes: &'a [TraceCasterMesh],
    /// The placements.
    pub instances: &'a [TraceCasterInstance],
}

/// What changed between two frames' traced scenes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SceneChange {
    /// Nothing: the frame traces the structures it has.
    Unchanged,
    /// The same instances, in the same slots, over the same meshes, moved: the top level is
    /// refitted (its slots rewritten in place, no bottom-level work, no reallocation).
    Refit,
    /// Instances or meshes changed: the plan runs again (new or changed meshes build).
    Rebuild,
}

impl SceneChange {
    /// `"unchanged"`, `"refit"` or `"rebuild"` (agent protocol).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Refit => "refit",
            Self::Rebuild => "rebuild",
        }
    }
}

/// Two hashes of a traced scene: its structure (which mesh, at which content version, in which
/// slot, with which flags) and its transforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SceneSignature {
    /// FNV-1a over the slots' mesh keys, generations and flags, and the instance count.
    pub structure: u64,
    /// FNV-1a over every slot's matrix bits.
    pub transforms: u64,
}

/// Builds a [`SceneSignature`] slot by slot, allocation-free.
#[derive(Debug, Clone, Copy)]
pub struct SignatureBuilder {
    structure: u64,
    transforms: u64,
    slots: u64,
}

impl Default for SignatureBuilder {
    fn default() -> Self {
        Self {
            structure: FNV_OFFSET,
            transforms: FNV_OFFSET,
            slots: 0,
        }
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv(hash: &mut u64, value: u64) {
    *hash ^= value;
    *hash = hash.wrapping_mul(FNV_PRIME);
}

impl SignatureBuilder {
    /// A builder with no slots.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one slot: its mesh key, the mesh's content generation, its flags, its matrix.
    pub fn slot(&mut self, mesh: u64, generation: u64, flags: u64, transform: &[[f32; 4]; 4]) {
        fnv(&mut self.structure, mesh);
        fnv(&mut self.structure, generation);
        fnv(&mut self.structure, flags);
        for value in transform.iter().flatten() {
            fnv(&mut self.transforms, u64::from(value.to_bits()));
        }
        self.slots += 1;
    }

    /// Adds a value to the structure alone (a source's generation counter, say).
    pub fn structure(&mut self, value: u64) {
        fnv(&mut self.structure, value);
    }

    /// The signature of the slots added.
    #[must_use]
    pub fn finish(self) -> SceneSignature {
        let mut structure = self.structure;
        fnv(&mut structure, self.slots);
        SceneSignature {
            structure,
            transforms: self.transforms,
        }
    }
}

/// The change from `previous` (the scene built last, `None` before any) to `next`.
#[must_use]
pub fn classify_scene_change(previous: Option<SceneSignature>, next: SceneSignature) -> SceneChange {
    match previous {
        Some(p) if p == next => SceneChange::Unchanged,
        Some(p) if p.structure == next.structure => SceneChange::Refit,
        _ => SceneChange::Rebuild,
    }
}

/// What still reads the sun's cascaded shadow maps in a traced frame ([`sun_map_keep_mask`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SunMapReaders {
    /// A drawn surface keeps the shadow map under the traced forward module: blended,
    /// transmissive, x-ray or ghost (`SUN_SHADOW_RT` keeps the map for them), or hair.
    pub blended_receivers: bool,
    /// A shared draw is installed that cannot take its sun from the trace alone
    /// ([`crate::SharedPassDraw::takes_traced_sun`]).
    pub shared_draw: bool,
    /// A caster the trace leaves out (skinned, morphed, vertex-animated; masked on the compute
    /// reference) still casts through the maps onto the shared draw and blended surfaces.
    pub missing_casters: bool,
    /// The cascade the outdoor composite (fog, atmosphere, clouds) samples for its light shafts,
    /// when it may run this frame.
    pub fog_cascade: Option<u8>,
}

/// The sun cascades a frame still draws (bit `i` = cascade `i`): every one unless the variant
/// traces scene geometry, the trace wrote the visibility texture, and no surface, shared draw or
/// missing caster needs the maps; then only the outdoor composite's cascade, if any.
/// Conservative: any reader in [`SunMapReaders`] other than the composite keeps every cascade.
#[must_use]
pub fn sun_map_keep_mask(variant: super::SunShadowVariant, applied: bool, readers: SunMapReaders) -> u8 {
    if !(variant.traces_rays() && applied)
        || readers.blended_receivers
        || readers.shared_draw
        || readers.missing_casters
    {
        return u8::MAX;
    }
    readers.fog_cascade.map_or(0, |cascade| 1u8 << cascade.min(7))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwrt::SunShadowVariant;

    const IDENTITY: [[f32; 4]; 4] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    fn moved(x: f32) -> [[f32; 4]; 4] {
        let mut m = IDENTITY;
        m[3][0] = x;
        m
    }

    fn signature(slots: &[(u64, u64, f32)]) -> SceneSignature {
        let mut b = SignatureBuilder::new();
        for &(mesh, generation, x) in slots {
            b.slot(mesh, generation, 0, &moved(x));
        }
        b.finish()
    }

    #[test]
    fn a_steady_scene_is_unchanged_and_the_first_build_rebuilds() {
        let s = signature(&[(1, 1, 0.0), (2, 1, 3.0)]);
        assert_eq!(classify_scene_change(None, s), SceneChange::Rebuild);
        assert_eq!(classify_scene_change(Some(s), s), SceneChange::Unchanged);
    }

    #[test]
    fn moved_instances_refit_and_anything_structural_rebuilds() {
        let base = signature(&[(1, 1, 0.0), (2, 1, 3.0)]);
        let shifted = signature(&[(1, 1, 0.5), (2, 1, 3.0)]);
        assert_eq!(classify_scene_change(Some(base), shifted), SceneChange::Refit);
        // A mesh's new content, a swapped slot order, one more instance, a new flag.
        let regenerated = signature(&[(1, 2, 0.0), (2, 1, 3.0)]);
        let swapped = signature(&[(2, 1, 3.0), (1, 1, 0.0)]);
        let grown = signature(&[(1, 1, 0.0), (2, 1, 3.0), (2, 1, 6.0)]);
        let mut flagged = SignatureBuilder::new();
        flagged.slot(1, 1, 1, &moved(0.0));
        flagged.slot(2, 1, 0, &moved(3.0));
        for next in [regenerated, swapped, grown, flagged.finish()] {
            assert_eq!(classify_scene_change(Some(base), next), SceneChange::Rebuild);
        }
        // Empty scenes of different sizes differ too.
        assert_ne!(signature(&[]), signature(&[(1, 1, 0.0)]));
    }

    #[test]
    fn mesh_validity_and_world_triangles() {
        let mesh = TraceCasterMesh {
            key: 1,
            generation: 1,
            positions: Arc::from(vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]),
            indices: Arc::from(vec![0, 1, 2]),
        };
        assert!(mesh.is_valid());
        assert_eq!(mesh.triangle_count(), 1);
        let tri: Vec<_> = mesh.world_triangles(&moved(2.0)).collect();
        assert_eq!(tri, [[[2.0, 0.0, 0.0], [3.0, 0.0, 0.0], [2.0, 0.0, 1.0]]]);
        let bad_index = TraceCasterMesh {
            indices: Arc::from(vec![0, 1, 3]),
            ..mesh.clone()
        };
        let ragged = TraceCasterMesh {
            indices: Arc::from(vec![0, 1]),
            ..mesh.clone()
        };
        let nan = TraceCasterMesh {
            positions: Arc::from(vec![[f32::NAN, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]),
            ..mesh.clone()
        };
        for m in [bad_index, ragged, nan] {
            assert!(!m.is_valid());
        }
    }

    #[test]
    fn coverage_packs_into_eight_bits() {
        assert_eq!(coverage_custom_data(0.0), 0);
        assert_eq!(coverage_custom_data(1.0), 255);
        assert_eq!(coverage_custom_data(2.0), 255);
        assert_eq!(coverage_custom_data(MASKED_COVERAGE), 128);
    }

    #[test]
    fn the_tags_keep_sources_apart() {
        let tags = [SHARED_CASTER_TAG, SET_CASTER_TAG, MASKED_CASTER_TAG];
        for (i, a) in tags.iter().enumerate() {
            for b in &tags[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_ne!(OPAQUE_INSTANCE_MASK & MASKED_INSTANCE_MASK, 0xFF);
        assert_eq!(OPAQUE_INSTANCE_MASK & MASKED_INSTANCE_MASK, 0);
    }

    #[test]
    fn the_maps_are_skipped_only_when_nothing_reads_them() {
        let none = SunMapReaders::default();
        assert_eq!(sun_map_keep_mask(SunShadowVariant::RayQuery, true, none), 0);
        assert_eq!(
            sun_map_keep_mask(SunShadowVariant::ComputeFallback, true, none),
            0
        );
        assert_eq!(
            sun_map_keep_mask(SunShadowVariant::RayQuery, false, none),
            u8::MAX
        );
        for variant in [SunShadowVariant::ShadowMap, SunShadowVariant::ScreenTrace] {
            assert_eq!(sun_map_keep_mask(variant, true, none), u8::MAX, "{variant:?}");
        }
        let fog = SunMapReaders {
            fog_cascade: Some(2),
            ..none
        };
        assert_eq!(sun_map_keep_mask(SunShadowVariant::RayQuery, true, fog), 0b100);
        let readers = [
            SunMapReaders {
                blended_receivers: true,
                ..fog
            },
            SunMapReaders {
                shared_draw: true,
                ..fog
            },
            SunMapReaders {
                missing_casters: true,
                ..none
            },
        ];
        for r in readers {
            assert_eq!(
                sun_map_keep_mask(SunShadowVariant::RayQuery, true, r),
                u8::MAX,
                "{r:?}"
            );
        }
    }
}
