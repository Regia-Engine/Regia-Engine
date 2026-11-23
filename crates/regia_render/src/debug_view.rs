// SPDX-License-Identifier: MPL-2.0
//! Debug view modes in a real frame (Tincal engine request F3, `specs/13` § 11).
//!
//! [`DebugViewMode::Wireframe`] draws every extracted mesh's triangle edges as a gizmo overlay on
//! the lit frame: the gizmo pass runs after the scene, post and translucents into the final LDR
//! target, so the line colour is exact (no exposure, no tonemap) and the same path serves the
//! forward and the GPU-driven mesh paths, every quality preset and every platform. Rigid meshes
//! become one batch each (object-space edges, the mesh's own model matrix); skinned meshes are
//! skinned on the CPU with the frame's palette so the lines follow the animation.
//!
//! Game hosts set the [`DebugView`] resource; [`apply_debug_view`] (a `Render` system after
//! [`crate::gizmos::GizmoSystems::Finish`]) appends the batches to
//! [`crate::ExtractedFrame::gizmos`]. The editor pane passes the same batches through
//! [`wireframe_frame`].
//!
//! Edge extraction is Regia Engine-original (`docs/provenance.md`): unique undirected edges of the
//! triangle list, deduplicated by vertex **position** so a UV or normal seam does not draw twice.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use regia_app::prelude::*;
use regia_core::math::Mat4;

use crate::debug::DebugViewMode;
use crate::gizmos::{GizmoBatch, GizmoFrame, GizmoLineConfig, GizmoSegment};
use crate::runtime::{CpuMesh, ExtractedFrame, ExtractedMesh};
use crate::skin::{JointMatrix, skin_position};

/// Depth bias that lets a wire win against its own surface (Bevy's convention: negative is
/// toward the camera).
const WIRE_DEPTH_BIAS: f32 = -0.003;

/// Colour of the wireframe overlay (linear RGBA).
pub const DEFAULT_WIRE_COLOR: [f32; 4] = [0.0, 1.0, 0.25, 1.0];

/// The frame's debug view: which mode the host asks for and how the overlay looks.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct DebugView {
    /// The active mode. [`DebugViewMode::Lit`] draws the frame untouched.
    pub mode: DebugViewMode,
    /// Wire colour (linear RGBA).
    pub wire_color: [f32; 4],
    /// Wire width in pixels.
    pub wire_width: f32,
}

impl Default for DebugView {
    fn default() -> Self {
        Self {
            mode: DebugViewMode::Lit,
            wire_color: DEFAULT_WIRE_COLOR,
            wire_width: 1.0,
        }
    }
}

impl DebugView {
    /// Whether this view mode is one the frame renderer implements. Every [`DebugViewMode`]
    /// variant is implemented since ADR-7211 (the forward shader draws unlit, normals, base color,
    /// roughness, metallic, AO, light complexity, shadow cascades, motion vectors and mip level;
    /// overdraw and LOD coloring swap materials).
    #[must_use]
    pub fn is_supported(mode: DebugViewMode) -> bool {
        matches!(
            mode,
            DebugViewMode::Lit
                | DebugViewMode::Wireframe
                | DebugViewMode::CloudShadowMap
                | DebugViewMode::ImageLight
                | DebugViewMode::ImageLightOcclusion
        ) || crate::debug_surface::DebugSurfaceFrame::handles(mode)
            || crate::lightmap_debug::handles(mode)
            || crate::lights_views::shading_view_code(mode) != 0
            || crate::debug_shading::handles(mode)
    }
}

/// Defaults for the parametrized modes when a caller names only the mode.
pub const DEFAULT_UV_TILE_M: f32 = 0.25;
/// Default texel density target, pixels per meter.
pub const DEFAULT_TARGET_PX_PER_M: f32 = 512.0;
/// Default texel density tolerance (±10 %).
pub const DEFAULT_DENSITY_TOLERANCE: f32 = 0.1;

impl DebugView {
    /// The implemented mode a stable lower-case name (`lit`, `wireframe`, `uv_checker`,
    /// `texel_density`, `material_id`, `cloud_shadow_map`, `image_light`, `image_light_occlusion`,
    /// `lightmap_density`, `lightmap_only`, `lightmap_invalid`, `unlit`, `normals`, `base_color`,
    /// `roughness`, `metallic`, `ambient_occlusion`, `overdraw`, `light_complexity`,
    /// `shadow_cascades`, `lod_color`, `motion_vectors`, `mip_level`) stands for, with default
    /// parameters.
    #[must_use]
    pub fn mode_from_name(name: &str) -> Option<DebugViewMode> {
        Self::mode_from_params(name, None, None, None)
    }

    /// Like [`Self::mode_from_name`], with the parametrized modes' values: `tile_m` for
    /// `uv_checker`, `target_px_per_m` and `tolerance` for `texel_density`. `None` takes the
    /// default.
    #[must_use]
    pub fn mode_from_params(
        name: &str,
        tile_m: Option<f32>,
        target_px_per_m: Option<f32>,
        tolerance: Option<f32>,
    ) -> Option<DebugViewMode> {
        match name {
            "lit" => Some(DebugViewMode::Lit),
            "wireframe" => Some(DebugViewMode::Wireframe),
            "uv_checker" => Some(DebugViewMode::UvChecker {
                tile_m: tile_m.unwrap_or(DEFAULT_UV_TILE_M),
            }),
            "texel_density" => Some(DebugViewMode::TexelDensity {
                target_px_per_m: target_px_per_m.unwrap_or(DEFAULT_TARGET_PX_PER_M),
                tolerance: tolerance.unwrap_or(DEFAULT_DENSITY_TOLERANCE),
            }),
            "material_id" => Some(DebugViewMode::MaterialId),
            "cloud_shadow_map" => Some(DebugViewMode::CloudShadowMap),
            "image_light" => Some(DebugViewMode::ImageLight),
            "image_light_occlusion" => Some(DebugViewMode::ImageLightOcclusion),
            "lightmap_density" => Some(DebugViewMode::LightmapDensity),
            "lightmap_only" => Some(DebugViewMode::LightmapOnly),
            "lightmap_invalid" => Some(DebugViewMode::LightmapInvalid),
            // ADR-7211.
            "unlit" => Some(DebugViewMode::Unlit),
            "normals" => Some(DebugViewMode::Normals),
            "base_color" => Some(DebugViewMode::BaseColor),
            "roughness" => Some(DebugViewMode::Roughness),
            "metallic" => Some(DebugViewMode::Metallic),
            "ambient_occlusion" => Some(DebugViewMode::AmbientOcclusion),
            "overdraw" => Some(DebugViewMode::Overdraw),
            "light_complexity" => Some(DebugViewMode::LightComplexity),
            "shadow_cascades" => Some(DebugViewMode::ShadowCascades),
            "lod_color" => Some(DebugViewMode::LodColor),
            "motion_vectors" => Some(DebugViewMode::MotionVectors),
            "mip_level" => Some(DebugViewMode::MipLevel),
            _ => None,
        }
    }
}

/// Deduplicated edge cache: mesh + index source → vertex-index pairs.
type EdgeKey = (usize, usize);
/// Rigid segment cache: the edge key. One entry per mesh and index list; a new wire colour
/// replaces it (a colour drag must not add a buffer per mesh per colour).
type RigidKey = EdgeKey;

struct CachedEdges {
    mesh: Weak<CpuMesh>,
    /// The draw's own index list, if any: its address is part of the key, and a freed list's
    /// address can come back for another list, so a hit must still upgrade to this one.
    indices: Option<Weak<Vec<u32>>>,
    edges: Arc<Vec<[u32; 2]>>,
}

struct CachedRigid {
    mesh: Weak<CpuMesh>,
    /// The draw's own index list, if any (as [`CachedEdges::indices`]: a freed list's address
    /// can come back for another list).
    indices: Option<Weak<Vec<u32>>>,
    /// The wire colour's four lanes as bits.
    color: [u32; 4],
    segments: Arc<[GizmoSegment]>,
}

struct WireCache {
    edges: HashMap<EdgeKey, CachedEdges>,
    rigid: HashMap<RigidKey, CachedRigid>,
}

static EDGE_CACHE: Mutex<Option<WireCache>> = Mutex::new(None);

/// Unique undirected edges of a triangle list, as pairs of vertex indices. Vertices at the same
/// position count as one, so seams contribute a single line. Out-of-range triangles are skipped.
#[must_use]
pub fn unique_edges(mesh: &CpuMesh, indices: &[u32]) -> Vec<[u32; 2]> {
    let mut canonical: HashMap<[u32; 3], u32> = HashMap::with_capacity(mesh.vertices.len());
    let canon: Vec<u32> = mesh
        .vertices
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let key = v.position.map(|c| (c + 0.0).to_bits());
            *canonical.entry(key).or_insert(i as u32)
        })
        .collect();
    let mut seen: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
    let mut edges = Vec::with_capacity(indices.len() / 2);
    for tri in indices.as_chunks::<3>().0 {
        if tri.iter().any(|&i| i as usize >= canon.len()) {
            continue;
        }
        for (a, b) in [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
            let (ca, cb) = (canon[a as usize], canon[b as usize]);
            if ca == cb {
                continue;
            }
            let key = (ca.min(cb), ca.max(cb));
            if seen.insert(key) {
                edges.push([key.0, key.1]);
            }
        }
    }
    edges
}

#[cfg(test)]
thread_local! {
    static EDGE_LOCKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static SEGMENT_BUFFERS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static SKIN_EVALS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn note_edge_lock() {
    #[cfg(test)]
    EDGE_LOCKS.with(|locks| locks.set(locks.get().saturating_add(1)));
}

fn note_segment_buffer() {
    #[cfg(test)]
    SEGMENT_BUFFERS.with(|buffers| buffers.set(buffers.get().saturating_add(1)));
}

fn edge_key(mesh: &Arc<CpuMesh>, draw_indices: Option<&Arc<Vec<u32>>>) -> EdgeKey {
    (
        Arc::as_ptr(mesh) as usize,
        draw_indices.map_or(0, |indices| Arc::as_ptr(indices) as usize),
    )
}

fn cached_edges(
    cache: &mut WireCache,
    mesh: &Arc<CpuMesh>,
    draw_indices: Option<&Arc<Vec<u32>>>,
) -> Arc<Vec<[u32; 2]>> {
    let key = edge_key(mesh, draw_indices);
    if let Some(entry) = cache.edges.get(&key)
        && entry
            .mesh
            .upgrade()
            .is_some_and(|cached| Arc::ptr_eq(&cached, mesh))
        && match (&entry.indices, draw_indices) {
            (None, None) => true,
            (Some(cached), Some(drawn)) => cached.upgrade().is_some_and(|i| Arc::ptr_eq(&i, drawn)),
            _ => false,
        }
    {
        return Arc::clone(&entry.edges);
    }
    let indices = draw_indices.map_or(&mesh.indices, |indices| indices.as_ref());
    let edges = Arc::new(unique_edges(mesh, indices));
    cache.edges.insert(
        key,
        CachedEdges {
            mesh: Arc::downgrade(mesh),
            indices: draw_indices.map(Arc::downgrade),
            edges: Arc::clone(&edges),
        },
    );
    edges
}

fn rigid_segments(
    cache: &mut WireCache,
    mesh: &Arc<CpuMesh>,
    draw_indices: Option<&Arc<Vec<u32>>>,
    edges: &[[u32; 2]],
    color: [f32; 4],
) -> Arc<[GizmoSegment]> {
    let key = edge_key(mesh, draw_indices);
    let color_bits = color.map(f32::to_bits);
    if let Some(entry) = cache.rigid.get(&key)
        && entry.color == color_bits
        && entry
            .mesh
            .upgrade()
            .is_some_and(|cached| Arc::ptr_eq(&cached, mesh))
        && match (&entry.indices, draw_indices) {
            (None, None) => true,
            (Some(cached), Some(drawn)) => cached.upgrade().is_some_and(|i| Arc::ptr_eq(&i, drawn)),
            _ => false,
        }
    {
        return Arc::clone(&entry.segments);
    }
    note_segment_buffer();
    let segments: Arc<[GizmoSegment]> = edges
        .iter()
        .map(|[a, b]| {
            segment(
                mesh.vertices[*a as usize].position,
                mesh.vertices[*b as usize].position,
                color,
            )
        })
        .collect();
    cache.rigid.insert(
        key,
        CachedRigid {
            mesh: Arc::downgrade(mesh),
            indices: draw_indices.map(Arc::downgrade),
            color: color_bits,
            segments: Arc::clone(&segments),
        },
    );
    segments
}

fn segment(a: [f32; 3], b: [f32; 3], color: [f32; 4]) -> GizmoSegment {
    GizmoSegment {
        a,
        b,
        color_a: color,
        color_b: color,
    }
}

/// The skinned world-space position of vertex `i`.
fn skinned_position(mesh: &CpuMesh, palette: &[JointMatrix], i: usize) -> [f32; 3] {
    #[cfg(test)]
    SKIN_EVALS.with(|evals| evals.set(evals.get().saturating_add(1)));
    let v = &mesh.vertices[i];
    let pick = |lane: usize| {
        palette
            .get(v.joints[lane] as usize)
            .copied()
            .unwrap_or(JointMatrix::identity())
    };
    skin_position(v.position, &[pick(0), pick(1), pick(2), pick(3)], v.weights)
}

/// The wireframe batches of a frame's extracted meshes, one per mesh.
#[must_use]
pub fn wireframe_batches(meshes: &[ExtractedMesh], view: &DebugView) -> Vec<GizmoBatch> {
    let _span = tracing::info_span!("wireframe_batches", meshes = meshes.len()).entered();
    let line = GizmoLineConfig {
        width: view.wire_width,
        ..GizmoLineConfig::default()
    };
    let color = view.wire_color;
    note_edge_lock();
    let mut guard = EDGE_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = guard.get_or_insert_with(|| WireCache {
        edges: HashMap::new(),
        rigid: HashMap::new(),
    });
    // Entries whose mesh is gone are dropped once per batch, not once per mesh.
    // Entries whose mesh or index list is gone are dropped.
    cache.edges.retain(|_, entry| {
        entry.mesh.strong_count() > 0 && entry.indices.as_ref().is_none_or(|i| i.strong_count() > 0)
    });
    cache.rigid.retain(|_, entry| {
        entry.mesh.strong_count() > 0 && entry.indices.as_ref().is_none_or(|i| i.strong_count() > 0)
    });
    let mut batches = Vec::with_capacity(meshes.len());
    let mut skinned_pos = Vec::new();
    for extracted in meshes {
        let mesh = &extracted.mesh;
        let edges = cached_edges(cache, mesh, extracted.draw_indices.as_ref());
        if edges.is_empty() {
            continue;
        }
        let skinned = !extracted.joints.is_empty();
        let segments = if skinned {
            skinned_pos.clear();
            for (index, _) in mesh.vertices.iter().enumerate() {
                skinned_pos.push(skinned_position(mesh, &extracted.joints, index));
            }
            note_segment_buffer();
            edges
                .iter()
                .map(|[a, b]| segment(skinned_pos[*a as usize], skinned_pos[*b as usize], color))
                .collect::<Arc<[GizmoSegment]>>()
        } else {
            rigid_segments(cache, mesh, extracted.draw_indices.as_ref(), &edges, color)
        };
        batches.push(GizmoBatch {
            // A skinned mesh's palette is already world space; the renderer ignores its model.
            world_from_local: if skinned {
                Mat4::IDENTITY
            } else {
                Mat4::from_cols_array_2d(&extracted.model)
            },
            line: line.clone(),
            depth_bias: WIRE_DEPTH_BIAS,
            on_top: false,
            segments,
            joints: Arc::from(Vec::new()),
        });
    }
    batches
}

/// `existing` plus the wireframe of `meshes` when `view` asks for it; `existing` untouched
/// otherwise.
#[must_use]
pub fn wireframe_frame(
    existing: &Arc<GizmoFrame>,
    meshes: &[ExtractedMesh],
    view: &DebugView,
) -> Arc<GizmoFrame> {
    if view.mode != DebugViewMode::Wireframe {
        return Arc::clone(existing);
    }
    let mut batches = existing.batches.clone();
    batches.extend(wireframe_batches(meshes, view));
    Arc::new(GizmoFrame {
        batches,
        labels: existing.labels.clone(),
    })
}

/// The resources [`apply_debug_view`] reads for the lightmap views.
type LightmapViewInputs<'w> = (
    Option<Res<'w, crate::lightmap::InstalledLightmap>>,
    Option<Res<'w, crate::lightmap::LightmapSettings>>,
);

/// `Render` system: appends the wireframe to the extracted frame's gizmos for the game's
/// window views.
pub fn apply_debug_view(
    view: Res<DebugView>,
    labels: Query<&crate::debug_surface::MaterialLabel>,
    sink: Option<Res<crate::debug_surface::DebugLegendSink>>,
    runtime: Option<ResMut<crate::RuntimeRenderer>>,
    mut extracted: ResMut<ExtractedFrame>,
    (installed, settings): LightmapViewInputs<'_>,
    receivers: Query<&crate::lightmap::AppliedLightmapReceiver>,
) {
    let _span = tracing::debug_span!("apply_debug_view").entered();
    if let Some(mut runtime) = runtime {
        runtime.0.set_image_light_view(view.mode);
    }
    // The forward shader's views (ADR-7211): every render path reads the code from the frame.
    extracted.lights_views.shading_view = view.mode;
    if let Some(meshes) = crate::debug_shading::debug_shading_meshes(&extracted.meshes, view.mode) {
        extracted.meshes = meshes;
        return;
    }
    if crate::lightmap_debug::handles(view.mode) {
        let map = installed.as_deref().and_then(|i| i.lightmap.clone());
        let entry_of = |entity| {
            receivers
                .get(entity)
                .ok()
                .map(crate::lightmap::AppliedLightmapReceiver::entry)
        };
        crate::lightmap_debug::apply_lightmap_debug_view(
            &mut extracted,
            view.mode,
            map.as_ref(),
            settings.as_deref().copied().unwrap_or_default(),
            &entry_of,
        );
        return;
    }
    if view.mode == DebugViewMode::CloudShadowMap {
        // Drawn by the outdoor composite over the lit frame (ADR-320).
        extracted.outdoor.clouds.shadow_map_debug = true;
        return;
    }
    if crate::debug_surface::DebugSurfaceFrame::handles(view.mode) {
        let label = |entity| labels.get(entity).ok().map(|l| l.0.clone());
        if let Some(frame) = crate::debug_surface::debug_surface_meshes(&extracted.meshes, view.mode, &label)
        {
            extracted.meshes = frame.meshes;
            if view.mode == DebugViewMode::MaterialId
                && let Some(sink) = sink
            {
                (sink.0)(&frame.legend);
            }
        }
        return;
    }
    if view.mode != DebugViewMode::Wireframe {
        return;
    }
    let frame = wireframe_frame(&extracted.gizmos, &extracted.meshes, &view);
    extracted.gizmos = frame;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LabVertex;

    fn vertex(position: [f32; 3]) -> LabVertex {
        LabVertex {
            position,
            normal: [0.0, 0.0, 1.0],
            uv: [0.0; 2],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        }
    }

    #[test]
    fn a_quad_has_five_unique_edges_even_with_split_seam_vertices() {
        // Two triangles sharing the diagonal, the diagonal's vertices duplicated (a UV seam).
        let mesh = CpuMesh {
            vertices: vec![
                vertex([0.0, 0.0, 0.0]),
                vertex([1.0, 0.0, 0.0]),
                vertex([1.0, 1.0, 0.0]),
                vertex([0.0, 0.0, 0.0]),
                vertex([1.0, 1.0, 0.0]),
                vertex([0.0, 1.0, 0.0]),
            ],
            indices: vec![0, 1, 2, 3, 4, 5],
        };
        assert_eq!(unique_edges(&mesh, &mesh.indices).len(), 5);
    }

    /// The edge cache is keyed by the index list's address. A dropped index `Arc` whose address
    /// a new list reuses (same allocation size) must not hand back the old list's edges
    /// (audit render10 #8).
    #[test]
    fn a_reused_index_address_does_not_return_the_old_edges() {
        let mesh = Arc::new(CpuMesh {
            vertices: vec![
                vertex([0.0, 0.0, 0.0]),
                vertex([1.0, 0.0, 0.0]),
                vertex([1.0, 1.0, 0.0]),
                vertex([0.0, 1.0, 0.0]),
            ],
            indices: vec![0, 1, 2, 0, 2, 3],
        });
        let mut cache = WireCache {
            edges: HashMap::new(),
            rigid: HashMap::new(),
        };
        let one_triangle = Arc::new(vec![0u32, 1, 2]);
        let old_address = Arc::as_ptr(&one_triangle) as usize;
        assert_eq!(cached_edges(&mut cache, &mesh, Some(&one_triangle)).len(), 3);
        drop(one_triangle);
        // Same-size allocations: the allocator hands the freed block back sooner or later.
        // Keep every candidate alive so each try gets a fresh address.
        let mut candidates = Vec::new();
        let mut reused = false;
        for _ in 0..256 {
            let quad = Arc::new(vec![0u32, 2, 3]);
            reused = Arc::as_ptr(&quad) as usize == old_address;
            candidates.push(quad);
            if reused {
                break;
            }
        }
        let quad = candidates.last().expect("one candidate");
        let edges = cached_edges(&mut cache, &mesh, Some(quad));
        assert_eq!(
            *edges,
            vec![[0, 2], [2, 3], [0, 3]],
            "the edges of [0, 2, 3] (address reused: {reused})"
        );
    }

    /// The rigid segment cache holds one entry per mesh and index list: a wire colour drag
    /// replaces it, and a reused index-list address does not return the old list's segments.
    #[test]
    fn rigid_segments_follow_the_index_list_and_keep_one_colour() {
        let mesh = Arc::new(CpuMesh {
            vertices: vec![
                vertex([0.0, 0.0, 0.0]),
                vertex([1.0, 0.0, 0.0]),
                vertex([1.0, 1.0, 0.0]),
                vertex([0.0, 1.0, 0.0]),
            ],
            indices: vec![0, 1, 2, 0, 2, 3],
        });
        let mut cache = WireCache {
            edges: HashMap::new(),
            rigid: HashMap::new(),
        };
        let edges = cached_edges(&mut cache, &mesh, None);
        for k in 0..64 {
            let color = [k as f32 / 64.0, 0.5, 0.5, 1.0];
            let segments = rigid_segments(&mut cache, &mesh, None, &edges, color);
            assert_eq!(segments[0].color_a, color);
        }
        assert_eq!(cache.rigid.len(), 1, "a colour drag grew the segment cache");
        let color = [1.0; 4];
        let one_triangle = Arc::new(vec![0u32, 1, 2]);
        let old_address = Arc::as_ptr(&one_triangle) as usize;
        let edges = cached_edges(&mut cache, &mesh, Some(&one_triangle));
        let _ = rigid_segments(&mut cache, &mesh, Some(&one_triangle), &edges, color);
        drop(one_triangle);
        let mut candidates = Vec::new();
        let mut reused = false;
        for _ in 0..256 {
            let quad = Arc::new(vec![0u32, 2, 3]);
            reused = Arc::as_ptr(&quad) as usize == old_address;
            candidates.push(quad);
            if reused {
                break;
            }
        }
        let quad = candidates.last().expect("one candidate");
        let edges = cached_edges(&mut cache, &mesh, Some(quad));
        let segments = rigid_segments(&mut cache, &mesh, Some(quad), &edges, color);
        let want: Vec<_> = edges
            .iter()
            .map(|[a, b]| {
                (
                    mesh.vertices[*a as usize].position,
                    mesh.vertices[*b as usize].position,
                )
            })
            .collect();
        let got: Vec<_> = segments.iter().map(|s| (s.a, s.b)).collect();
        assert_eq!(got, want, "the segments of [0, 2, 3] (address reused: {reused})");
    }

    #[test]
    fn degenerate_and_out_of_range_triangles_are_skipped() {
        let mesh = CpuMesh {
            vertices: vec![vertex([0.0; 3]), vertex([1.0, 0.0, 0.0])],
            indices: vec![0, 1, 7, 0, 0, 1],
        };
        assert_eq!(unique_edges(&mesh, &mesh.indices), vec![[0, 1]]);
    }

    /// ADR-7211: the twelve modes spec 13 § 11 lists that were reserved are implemented and named.
    #[test]
    fn every_formerly_reserved_mode_is_supported_and_named() {
        for (name, mode) in [
            ("unlit", DebugViewMode::Unlit),
            ("normals", DebugViewMode::Normals),
            ("base_color", DebugViewMode::BaseColor),
            ("roughness", DebugViewMode::Roughness),
            ("metallic", DebugViewMode::Metallic),
            ("ambient_occlusion", DebugViewMode::AmbientOcclusion),
            ("overdraw", DebugViewMode::Overdraw),
            ("light_complexity", DebugViewMode::LightComplexity),
            ("shadow_cascades", DebugViewMode::ShadowCascades),
            ("lod_color", DebugViewMode::LodColor),
            ("motion_vectors", DebugViewMode::MotionVectors),
            ("mip_level", DebugViewMode::MipLevel),
        ] {
            assert!(DebugView::is_supported(mode), "{name} must be supported");
            assert_eq!(DebugView::mode_from_name(name), Some(mode), "{name}");
        }
    }

    #[test]
    fn the_surface_modes_are_implemented() {
        assert!(DebugView::is_supported(DebugViewMode::Wireframe));
        assert!(DebugView::is_supported(DebugViewMode::MaterialId));
        assert_eq!(
            DebugView::mode_from_params("uv_checker", Some(0.5), None, None),
            Some(DebugViewMode::UvChecker { tile_m: 0.5 })
        );
        for (name, mode) in [
            ("lightmap_density", DebugViewMode::LightmapDensity),
            ("lightmap_only", DebugViewMode::LightmapOnly),
            ("lightmap_invalid", DebugViewMode::LightmapInvalid),
        ] {
            assert_eq!(DebugView::mode_from_name(name), Some(mode));
            assert!(DebugView::is_supported(mode), "{name}");
        }
    }

    fn extracted(mesh: &Arc<CpuMesh>, joints: Vec<JointMatrix>) -> ExtractedMesh {
        ExtractedMesh {
            is_static: false,
            entity: Entity::from_raw_u32(1).expect("entity"),
            mesh: Arc::clone(mesh),
            material: Arc::new(crate::CpuMaterial::default()),
            layer: 1,
            model: Mat4::from_translation(regia_core::glam::Vec3::new(3.0, 0.0, 0.0)).to_cols_array_2d(),
            joints,
            decal_layer: 0,
            uv1: None,
            shadow: crate::ShadowCast::All,
            draw_indices: None,
            lod_key: 0,
            object_key_xor: 0,
            gi_proxy: false,
            lod_dither: 0.0,
        }
    }

    fn reset_wire_counts() {
        EDGE_LOCKS.with(|locks| locks.set(0));
        SEGMENT_BUFFERS.with(|buffers| buffers.set(0));
        SKIN_EVALS.with(|evals| evals.set(0));
    }

    #[test]
    fn a_wireframe_batch_locks_once_and_reuses_rigid_segments() {
        let quad = Arc::new(CpuMesh {
            vertices: vec![
                vertex([0.0, 0.0, 0.0]),
                vertex([1.0, 0.0, 0.0]),
                vertex([0.0, 1.0, 0.0]),
                vertex([1.0, 1.0, 0.0]),
            ],
            indices: vec![0, 1, 2, 2, 1, 3],
        });
        let other = Arc::new(CpuMesh {
            vertices: vec![vertex([0.0; 3]), vertex([2.0, 0.0, 0.0]), vertex([0.0, 2.0, 0.0])],
            indices: vec![0, 1, 2],
        });
        let meshes = vec![
            extracted(&quad, Vec::new()),
            extracted(&quad, Vec::new()),
            extracted(&other, Vec::new()),
        ];
        let view = DebugView {
            mode: DebugViewMode::Wireframe,
            ..DebugView::default()
        };
        reset_wire_counts();
        let first = wireframe_batches(&meshes, &view);
        assert_eq!(first.len(), 3);
        assert_eq!(
            EDGE_LOCKS.with(std::cell::Cell::get),
            1,
            "one batch takes EDGE_CACHE once"
        );
        // Local positions: the model is applied at draw time, not baked into the segments.
        assert_eq!(first[0].segments[0].a, quad.vertices[0].position);
        assert_eq!(first[0].world_from_local.w_axis.x, 3.0);
        assert!(
            Arc::ptr_eq(&first[0].segments, &first[1].segments),
            "two instances of one rigid mesh share the segment buffer"
        );

        reset_wire_counts();
        let second = wireframe_batches(&meshes[..1], &view);
        assert_eq!(EDGE_LOCKS.with(std::cell::Cell::get), 1);
        assert_eq!(
            SEGMENT_BUFFERS.with(std::cell::Cell::get),
            0,
            "the second call must reuse the rigid segment buffer"
        );
        assert!(Arc::ptr_eq(&second[0].segments, &first[0].segments));
        assert_eq!(second[0].segments, first[0].segments);

        let mut tinted = view.clone();
        tinted.wire_color = [1.0, 0.0, 0.0, 1.0];
        reset_wire_counts();
        let tinted_batches = wireframe_batches(&meshes[..1], &tinted);
        assert_eq!(
            SEGMENT_BUFFERS.with(std::cell::Cell::get),
            1,
            "a new colour builds one buffer"
        );
        assert!(!Arc::ptr_eq(&tinted_batches[0].segments, &first[0].segments));
        assert_eq!(tinted_batches[0].segments[0].color_a, tinted.wire_color);
    }

    #[test]
    fn a_skinned_wireframe_skins_each_vertex_once() {
        let mut mesh = CpuMesh {
            vertices: vec![
                vertex([0.0, 0.0, 0.0]),
                vertex([1.0, 0.0, 0.0]),
                vertex([0.0, 1.0, 0.0]),
            ],
            indices: vec![0, 1, 2],
        };
        for vertex in &mut mesh.vertices {
            vertex.weights = [1.0, 0.0, 0.0, 0.0];
        }
        let mesh = Arc::new(mesh);
        let extracted = extracted(&mesh, vec![JointMatrix::translation(0.0, 2.0, 0.0)]);
        let view = DebugView {
            mode: DebugViewMode::Wireframe,
            ..DebugView::default()
        };
        reset_wire_counts();
        let batches = wireframe_batches(std::slice::from_ref(&extracted), &view);
        assert_eq!(batches.len(), 1);
        assert_eq!(
            SKIN_EVALS.with(std::cell::Cell::get),
            mesh.vertices.len() as u32,
            "each vertex is skinned once, not once per edge"
        );
        assert_eq!(batches[0].world_from_local, Mat4::IDENTITY);
        assert_eq!(batches[0].segments[0].a[1], 2.0);
    }
}
