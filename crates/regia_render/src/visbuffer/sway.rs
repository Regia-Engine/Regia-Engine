// SPDX-License-Identifier: MPL-2.0
//! Foliage sway on the visibility path (ADR-3916, `specs/98` § 8d).
//!
//! [`crate::FoliageSway`] used to keep a plant on the forward path: its stage moves vertices the
//! clusters did not know about. The visibility path now moves them with the same function
//! (`regia::foliage_sway`, `shaders/foliage_sway.wgsl`) in its rasters, its resolve and its velocity
//! pass. What this module holds is the part a host or a tool can ask about:
//!
//! - **Which materials.** [`sway_gate`]: a plain material adopted with [`crate::FoliageSway::adopt`]
//!   takes the path. Its surface is the standard one written as a graph ([`crate::FOLIAGE_SURFACE`]),
//!   so the resolve shades the standard copy [`crate::FoliageSway::plain`] gives. A sway on any other
//!   surface graph stays forward, since the resolve has no graph surfaces ([`SwaySkip`] says why).
//! - **Far levels.** Voxel far levels never sway. An instance whose mesh has them fades its sway out
//!   before they can be selected ([`sway_fade`], [`voxel_switch`]), so a plant arrives at the switch
//!   at rest.
//!
//! The clock, the wind and the reach the cull grows its bounds by are per-frame GPU state in
//! `visbuffer/gpu/sway.rs`.

use super::meshlet::{MeshletMesh, ROOT_ERROR};
use crate::LabMaterial;
use crate::foliage_sway_material::FOLIAGE_SURFACE;
use crate::foliage_wind::FOLIAGE_SWAY_VERTEX;

/// Distinct sways the path holds in a frame (`SwayTable.records` in `shaders/foliage_sway.wgsl`). A
/// sway past it stays forward ([`SwaySkip::TableFull`]).
pub const SWAY_TABLE_RECORDS: usize = 256;

/// Fraction of the voxel switch distance up to which an instance keeps its whole sway ([`sway_fade`]).
pub const SWAY_FADE_START: f32 = 0.7;

/// Fraction of the voxel switch distance from which an instance is at rest ([`sway_fade`]): the
/// amplitude reaches zero 5 % before the first voxel level can be selected.
pub const SWAY_FADE_END: f32 = 0.95;

/// How much of its sway an instance keeps at `distance` from the eye when its first voxel far level
/// can be selected from `switch_distance` on: 1 up to [`SWAY_FADE_START`] of that distance, 0 from
/// [`SWAY_FADE_END`], linear between. CPU mirror of `regia::foliage_sway::fade`.
#[must_use]
pub fn sway_fade(distance: f32, switch_distance: f32) -> f32 {
    if switch_distance <= 0.0 {
        return 0.0;
    }
    ((SWAY_FADE_END * switch_distance - distance) / ((SWAY_FADE_END - SWAY_FADE_START) * switch_distance))
        .clamp(0.0, 1.0)
}

/// Why a swaying material draws on the forward path this frame (`render.get_visbuffer`
/// `last_frame.sway.forward`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SwaySkip {
    /// The sway sits on a surface graph other than [`crate::FOLIAGE_SURFACE`]: the resolve has no
    /// graph surfaces.
    GraphSurface,
    /// The material blends, transmits, or is hair or parallax, which the path does not draw.
    Translucent,
    /// A masked material on a frame that hashes its alpha test (temporal upscaling with hashed alpha).
    HashedAlpha,
    /// A masked material on a device without the texture table.
    NoTextureTable,
    /// The frame already holds [`SWAY_TABLE_RECORDS`] distinct sways.
    TableFull,
    /// Anything else that keeps a mesh forward: skinned, morphing, a LOD cross-fade, a render
    /// override, a thin film, subsurface scattering, or no clusters.
    Other,
}

impl SwaySkip {
    /// Every reason, in report order.
    pub const ALL: [Self; 6] = [
        Self::GraphSurface,
        Self::Translucent,
        Self::HashedAlpha,
        Self::NoTextureTable,
        Self::TableFull,
        Self::Other,
    ];

    /// The stable code the agent protocol reports.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::GraphSurface => "graph_surface",
            Self::Translucent => "translucent",
            Self::HashedAlpha => "hashed_alpha",
            Self::NoTextureTable => "no_texture_table",
            Self::TableFull => "table_full",
            Self::Other => "other",
        }
    }

    /// The reason's place in [`Self::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::GraphSurface => 0,
            Self::Translucent => 1,
            Self::HashedAlpha => 2,
            Self::NoTextureTable => 3,
            Self::TableFull => 4,
            Self::Other => 5,
        }
    }
}

/// What [`sway_gate`] says about a material.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwayGate {
    /// No foliage sway on it: the other gates decide.
    NotSwaying,
    /// A sway the path draws (per frame, the path may still be unable to: [`SwaySkip`]).
    Clusters,
    /// A sway that stays forward whatever the frame, and why.
    Forward(SwaySkip),
}

/// Whether the visibility path can draw `material`'s foliage sway.
#[must_use]
pub fn sway_gate(material: &LabMaterial) -> SwayGate {
    let Some(graph) = material.surface_graph.as_deref() else {
        return SwayGate::NotSwaying;
    };
    if graph.vertex_wgsl.as_deref() != Some(FOLIAGE_SWAY_VERTEX) {
        return SwayGate::NotSwaying;
    }
    if &*graph.wgsl != FOLIAGE_SURFACE {
        return SwayGate::Forward(SwaySkip::GraphSurface);
    }
    let alpha = crate::hair::effective_alpha_mode(material);
    let drawn =
        alpha == crate::ALPHA_OPAQUE || (alpha == crate::ALPHA_MASK && super::gpu::alpha_tests(material));
    if !drawn || material.transmission > 0.0 {
        return SwayGate::Forward(SwaySkip::Translucent);
    }
    if material.thin_film.is_on() {
        return SwayGate::Forward(SwaySkip::Other);
    }
    SwayGate::Clusters
}

/// True when a masked `material` is one the path's alpha-tested raster draws: `ALPHA_MASK`, no hair
/// cards and no parallax (ADR-3915). Whether a frame takes it also depends on its device and its
/// hashed alpha.
#[must_use]
pub fn takes_alpha_test(material: &LabMaterial) -> bool {
    super::gpu::alpha_tests(material)
}

/// Where a mesh's voxel far levels start (`VoxelFarProxy::extend_meshlets`), found from what that
/// call leaves in the cluster list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoxelSwitch {
    /// The first voxel level in the cluster list ([`crate::visbuffer::Meshlet::level`]).
    pub level: u32,
    /// That level's error, object units.
    pub error: f32,
    /// The sphere its LOD test takes the distance to: every voxel cluster's and every root's.
    pub sphere: [f32; 4],
}

/// The mesh's first voxel far level, or `None` when it has none. A voxel level's clusters carry no
/// normal cone (`[0, 0, 1, 1]`) and share one LOD sphere that is also their parent sphere, and the DAG's
/// root clusters name that same sphere as their parent with that level's error: no cooked level has
/// all of that (`tests/visbuffer_wind_voxel.rs` pins it against the real extension).
#[must_use]
pub fn voxel_switch(mesh: &MeshletMesh) -> Option<VoxelSwitch> {
    // The levels present, not every number up to the highest: a cooked level number is not
    // bounded, so a corrupt `u32::MAX` would walk four billion empty levels.
    let mut levels: Vec<u32> = mesh
        .meshlets
        .iter()
        .map(|m| m.level)
        .filter(|&l| l >= 1)
        .collect();
    levels.sort_unstable();
    levels.dedup();
    levels.into_iter().find_map(|level| {
        let mut clusters = mesh.meshlets.iter().filter(|m| m.level == level);
        let first = clusters.next()?;
        let voxel_like = |m: &super::meshlet::Meshlet| {
            m.cone == [0.0, 0.0, 1.0, 1.0] && m.lod == first.lod && m.parent == first.lod
        };
        if !voxel_like(first) || !clusters.all(voxel_like) {
            return None;
        }
        let parents_name_it = mesh
            .meshlets
            .iter()
            .any(|m| m.level < level && m.parent == first.lod && m.parent_error == first.lod_error);
        let below_roots = !mesh
            .meshlets
            .iter()
            .any(|m| m.level < level && m.parent_error >= ROOT_ERROR);
        (parents_name_it && below_roots).then_some(VoxelSwitch {
            level,
            error: first.lod_error,
            sphere: first.lod,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ALPHA_BLEND, ALPHA_MASK, FoliageSway};

    fn adopted(alpha_mode: u32) -> LabMaterial {
        FoliageSway::default()
            .adopt(&LabMaterial {
                alpha_mode,
                alpha_cutoff: 0.4,
                ..LabMaterial::default()
            })
            .expect("a plain material")
    }

    #[test]
    fn the_fade_is_whole_near_and_zero_before_the_switch() {
        let switch = 400.0;
        assert_eq!(sway_fade(0.0, switch), 1.0);
        assert_eq!(sway_fade(0.7 * switch, switch), 1.0);
        let middle = sway_fade(0.825 * switch, switch);
        assert!((middle - 0.5).abs() < 1.0e-3, "halfway {middle}");
        assert_eq!(sway_fade(0.95 * switch, switch), 0.0);
        assert_eq!(
            sway_fade(switch, switch),
            0.0,
            "at the switch the plant is at rest"
        );
        assert_eq!(sway_fade(10.0, 0.0), 0.0, "no room to fade: no sway");
        assert_eq!(SWAY_FADE_START, 0.7);
        assert_eq!(SWAY_FADE_END, 0.95);
        let shader = include_str!("../../shaders/foliage_sway.wgsl");
        assert!(shader.contains("clamp((0.95 * switch_distance - distance) / (0.25 * switch_distance)"));
    }

    #[test]
    fn adopted_opaque_and_masked_plants_take_the_path() {
        assert_eq!(sway_gate(&adopted(crate::ALPHA_OPAQUE)), SwayGate::Clusters);
        assert_eq!(sway_gate(&adopted(ALPHA_MASK)), SwayGate::Clusters);
    }

    #[test]
    fn other_materials_say_why() {
        assert_eq!(sway_gate(&LabMaterial::default()), SwayGate::NotSwaying);
        assert_eq!(
            sway_gate(&adopted(ALPHA_BLEND)),
            SwayGate::Forward(SwaySkip::Translucent)
        );
        let custom = LabMaterial {
            surface_graph: Some(std::sync::Arc::new(
                FoliageSway::default().graph(crate::surface_graph::SAMPLE_SURFACE),
            )),
            ..LabMaterial::default()
        };
        assert_eq!(sway_gate(&custom), SwayGate::Forward(SwaySkip::GraphSurface));
        let other_stage = LabMaterial {
            surface_graph: Some(std::sync::Arc::new(
                crate::surface_graph::SurfaceGraph::from_code(&crate::surface_graph::GraphCode {
                    surface: crate::surface_graph::SAMPLE_SURFACE.into(),
                    vertex: Some(crate::surface_graph::SAMPLE_VERTEX.into()),
                    uv1: false,
                }),
            )),
            ..LabMaterial::default()
        };
        assert_eq!(sway_gate(&other_stage), SwayGate::NotSwaying);
        let still = FoliageSway::still(&adopted(crate::ALPHA_OPAQUE));
        assert_eq!(sway_gate(&still), SwayGate::NotSwaying);
    }

    #[test]
    fn skip_codes_are_stable_and_indexed() {
        for (i, skip) in SwaySkip::ALL.iter().enumerate() {
            assert_eq!(skip.index(), i);
            assert!(skip.code().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
    }

    fn plain_mesh() -> MeshletMesh {
        let positions: Vec<[f32; 3]> = (0..9).map(|i| [(i % 3) as f32, (i / 3) as f32, 0.0]).collect();
        let indices: Vec<u32> = (0..2u32)
            .flat_map(|j| {
                (0..2u32).flat_map(move |i| {
                    let a = j * 3 + i;
                    [a, a + 1, a + 3, a + 1, a + 4, a + 3]
                })
            })
            .collect();
        MeshletMesh::single_level(&positions, &indices)
    }

    #[test]
    fn a_mesh_without_voxel_levels_has_no_switch() {
        assert_eq!(voxel_switch(&plain_mesh()), None);
        // A coarser level with a real cone or its own spheres is not a voxel level, even when its
        // parent's error matches.
        let mut mesh = plain_mesh();
        let mut coarse = mesh.meshlets[0];
        coarse.level = 1;
        coarse.lod_error = 2.0;
        coarse.parent_error = ROOT_ERROR;
        for root in &mut mesh.meshlets {
            root.parent_error = 2.0;
            root.parent = [9.0, 9.0, 9.0, 9.0];
        }
        coarse.lod = [9.0, 9.0, 9.0, 9.0];
        coarse.parent = [9.0, 9.0, 9.0, 9.0];
        coarse.cone = [0.0, 1.0, 0.0, 0.5];
        mesh.meshlets.push(coarse);
        assert_eq!(voxel_switch(&mesh), None);
    }

    /// A cooked level number is not bounded (the parser only checks the order). The search walks
    /// the levels present: before, a cluster at level `u32::MAX` made it scan every level number
    /// up to that one, each a pass over the clusters, on the frame thread (24 s for a 9-cluster
    /// mesh in the optimized dev profile; with these 4096 clusters, hours: the test hung).
    #[test]
    fn corr_visbuffer_a_huge_level_number_does_not_walk_every_level() {
        let mut mesh = plain_mesh();
        let first = mesh.meshlets[0];
        mesh.meshlets.resize(4096, first);
        let mut far = first;
        far.level = u32::MAX;
        mesh.meshlets.push(far);
        assert_eq!(voxel_switch(&mesh), None);
    }
}
