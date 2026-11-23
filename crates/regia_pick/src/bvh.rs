// SPDX-License-Identifier: MPL-2.0
//! A bounding-volume hierarchy over a rigid mesh's triangles (object space), built once per
//! mesh and cached by its `Arc` ([`bvh_for`]). Median split on the longest axis of the triangle
//! centroids, leaves of up to four triangles (a textbook BVH, see `docs/provenance.md`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use regia_core::math::Vec3;
use regia_render::CpuMesh;

use crate::mesh::{FaceCull, RayTriangle, ray_triangle};

/// Meshes with fewer triangles are cast brute force: a BVH costs more than it saves.
pub const BVH_MIN_TRIANGLES: usize = 64;

const LEAF_TRIANGLES: usize = 4;

#[derive(Debug, Clone, Copy)]
struct Node {
    lo: Vec3,
    hi: Vec3,
    /// Inner: index of the left child. Leaf: first entry of `order`.
    a: u32,
    /// Inner: index of the right child. Leaf: number of triangles.
    b: u32,
    leaf: bool,
}

/// A triangle BVH for one mesh.
#[derive(Debug, Clone)]
pub struct MeshBvh {
    nodes: Vec<Node>,
    /// Triangle indices (into the index list's triples) in leaf order.
    order: Vec<u32>,
    /// A copy of the mesh's index list, so a hit can name its vertices.
    indices: Vec<u32>,
}

impl MeshBvh {
    /// Builds the hierarchy over every triangle of `mesh` whose indices are in range and whose
    /// vertex positions are finite. `None` when there is no such triangle.
    #[must_use]
    pub fn build(mesh: &CpuMesh) -> Option<Self> {
        let count = mesh.vertices.len();
        let position = |i: u32| Vec3::from_array(mesh.vertices[i as usize].position);
        let mut bounds = Vec::new();
        for (t, tri) in mesh.indices.as_chunks::<3>().0.iter().enumerate() {
            if tri.iter().any(|&i| i as usize >= count) {
                continue;
            }
            let (a, b, c) = (position(tri[0]), position(tri[1]), position(tri[2]));
            if !(a.is_finite() && b.is_finite() && c.is_finite()) {
                continue;
            }
            bounds.push((t as u32, a.min(b).min(c), a.max(b).max(c)));
        }
        if bounds.is_empty() {
            return None;
        }
        let mut bvh = Self {
            nodes: Vec::with_capacity(bounds.len() / 2),
            order: Vec::with_capacity(bounds.len()),
            indices: mesh.indices.clone(),
        };
        bvh.build_node(&mut bounds);
        Some(bvh)
    }

    fn build_node(&mut self, items: &mut [(u32, Vec3, Vec3)]) -> u32 {
        let (lo, hi) = items.iter().fold(
            (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
            |(lo, hi), (_, l, h)| (lo.min(*l), hi.max(*h)),
        );
        let index = self.nodes.len() as u32;
        if items.len() <= LEAF_TRIANGLES {
            let first = self.order.len() as u32;
            self.order.extend(items.iter().map(|(t, _, _)| *t));
            self.nodes.push(Node {
                lo,
                hi,
                a: first,
                b: items.len() as u32,
                leaf: true,
            });
            return index;
        }
        self.nodes.push(Node {
            lo,
            hi,
            a: 0,
            b: 0,
            leaf: false,
        });
        let (clo, chi) = items.iter().fold(
            (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
            |(lo, hi), (_, l, h)| {
                let c = (*l + *h) * 0.5;
                (lo.min(c), hi.max(c))
            },
        );
        let extent = chi - clo;
        let axis = if extent.x >= extent.y && extent.x >= extent.z {
            0
        } else if extent.y >= extent.z {
            1
        } else {
            2
        };
        items.sort_by(|x, y| {
            (x.1[axis] + x.2[axis])
                .total_cmp(&(y.1[axis] + y.2[axis]))
                .then(x.0.cmp(&y.0))
        });
        let mid = items.len() / 2;
        let (left_items, right_items) = items.split_at_mut(mid);
        let left = self.build_node(left_items);
        let right = self.build_node(right_items);
        self.nodes[index as usize].a = left;
        self.nodes[index as usize].b = right;
        index
    }

    /// The three vertex indices of `triangle`, or `None` when `triangle` is past the index list
    /// this BVH was built from.
    #[must_use]
    pub fn triangle_indices(&self, triangle: u32) -> Option<[u32; 3]> {
        let at = (triangle as usize).checked_mul(3)?;
        let tri = self.indices.get(at..at.checked_add(3)?)?;
        Some([tri[0], tri[1], tri[2]])
    }

    /// The nearest triangle an object-space ray hits (`flip`: a mirroring model swaps which
    /// winding is the front), with the lowest triangle index winning ties as the brute-force
    /// cast does.
    #[must_use]
    pub fn nearest(
        &self,
        mesh: &CpuMesh,
        origin: Vec3,
        direction: Vec3,
        flip: bool,
        cull: FaceCull,
    ) -> Option<(u32, RayTriangle)> {
        self.nearest_within(mesh, origin, direction, flip, cull, f32::INFINITY)
    }

    /// [`MeshBvh::nearest`] with a farthest-hit distance: nodes and triangles beyond `max_t`
    /// are pruned (a hit at exactly `max_t` still counts).
    #[must_use]
    pub fn nearest_within(
        &self,
        mesh: &CpuMesh,
        origin: Vec3,
        direction: Vec3,
        flip: bool,
        cull: FaceCull,
        max_t: f32,
    ) -> Option<(u32, RayTriangle)> {
        // A mesh other than the one the BVH was built from can lack a referenced vertex: skip
        // such triangles rather than index out of range (the process aborts on panic).
        let position = |i: u32| {
            mesh.vertices
                .get(i as usize)
                .map(|v| Vec3::from_array(v.position))
        };
        let inv = Vec3::new(
            safe_recip(direction.x),
            safe_recip(direction.y),
            safe_recip(direction.z),
        );
        let mut best: Option<(u32, RayTriangle)> = None;
        let mut stack = vec![0_u32];
        while let Some(node) = stack.pop() {
            crate::stats::note_bvh_node();
            let n = &self.nodes[node as usize];
            let limit = best.map_or(max_t, |(_, hit)| hit.t.min(max_t));
            if !slab(n.lo, n.hi, origin, inv, limit) {
                continue;
            }
            if n.leaf {
                for &t in &self.order[n.a as usize..n.a as usize + n.b as usize] {
                    let Some(tri) = self.triangle_indices(t) else {
                        continue;
                    };
                    let (Some(a), Some(b), Some(c)) = (position(tri[0]), position(tri[1]), position(tri[2]))
                    else {
                        continue;
                    };
                    let Some(mut hit) = ray_triangle(origin, direction, a, b, c, FaceCull::None) else {
                        continue;
                    };
                    hit.front_face ^= flip;
                    if cull == FaceCull::Back && !hit.front_face {
                        continue;
                    }
                    if hit.t > max_t {
                        continue;
                    }
                    if best.is_none_or(|(bt, b)| hit.t < b.t || (hit.t == b.t && t < bt)) {
                        best = Some((t, hit));
                    }
                }
            } else {
                stack.push(n.a);
                stack.push(n.b);
            }
        }
        best
    }
}

fn safe_recip(d: f32) -> f32 {
    if d.abs() < 1e-20 {
        f32::INFINITY.copysign(d)
    } else {
        1.0 / d
    }
}

/// Slab test against a box padded by a relative epsilon (edge hits count, see `ray_triangle`),
/// for `t` in `[0, limit]`.
fn slab(lo: Vec3, hi: Vec3, origin: Vec3, inv: Vec3, limit: f32) -> bool {
    let pad = Vec3::splat((hi - lo).max_element() * 1e-4 + 1e-6);
    let (lo, hi) = (lo - pad, hi + pad);
    let mut near = 0.0_f32;
    let mut far = limit;
    for axis in 0..3 {
        if inv[axis].is_infinite() {
            if origin[axis] < lo[axis] || origin[axis] > hi[axis] {
                return false;
            }
            continue;
        }
        let (a, b) = (
            (lo[axis] - origin[axis]) * inv[axis],
            (hi[axis] - origin[axis]) * inv[axis],
        );
        near = near.max(a.min(b));
        far = far.min(a.max(b));
        if near > far {
            return false;
        }
    }
    true
}

/// Cached tree plus the vertex and index counts it was built for.
type BvhEntry = ([usize; 2], Option<Arc<MeshBvh>>);
type BvhCache = HashMap<usize, (Weak<CpuMesh>, BvhEntry)>;

/// Removes every entry whose mesh has been freed and returns the removed values, so the caller
/// drops them after releasing its lock. Gives the bucket memory back once the map is mostly
/// empty.
pub(crate) fn sweep_dead<V>(map: &mut HashMap<usize, (Weak<CpuMesh>, V)>) -> Vec<V> {
    let dead: Vec<usize> = map
        .iter()
        .filter(|(_, (weak, _))| weak.strong_count() == 0)
        .map(|(key, _)| *key)
        .collect();
    let removed: Vec<V> = dead
        .iter()
        .filter_map(|key| map.remove(key))
        .map(|(_, v)| v)
        .collect();
    if !removed.is_empty() && map.len() * 4 < map.capacity() {
        map.shrink_to_fit();
    }
    removed
}

/// The cached BVH of `mesh` (`None` when it has fewer than [`BVH_MIN_TRIANGLES`] triangles or
/// none usable). Keyed by the `Arc`'s address and checked against a weak handle, so a freed and
/// reused address never serves a stale tree. The cache's weak handle makes `Arc::get_mut` fail,
/// so an edited mesh is a new `Arc` with its own entry; the vertex and index counts are checked
/// as well, so a tree is never served for lists of another length. Entries of freed meshes are
/// released at most once per query scope (or on every call outside one). The tree is built on first use; a host that wants the cost at load
/// calls this then.
#[must_use]
pub fn bvh_for(mesh: &Arc<CpuMesh>) -> Option<Arc<MeshBvh>> {
    static CACHE: Mutex<Option<BvhCache>> = Mutex::new(None);
    let key = Arc::as_ptr(mesh) as usize;
    let small = mesh.indices.len() / 3 < BVH_MIN_TRIANGLES;
    let counts = [mesh.vertices.len(), mesh.indices.len()];
    // Dead and replaced trees are dropped after the lock is released.
    let mut graveyard: Vec<BvhEntry> = Vec::new();
    {
        let mut guard = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let cache = guard.get_or_insert_with(HashMap::new);
        // Freed entries release at most once per query scope (see `pick_cache_scope`).
        if crate::stats::sweep_due_bvh() {
            crate::stats::note_cache_sweep();
            graveyard.extend(sweep_dead(cache));
        }
        if small {
            graveyard.extend(cache.remove(&key).map(|(_, entry)| entry));
        } else if let Some((weak, (built, bvh))) = cache.get(&key)
            // A tree built for other list lengths would index out of range.
            && *built == counts
            && weak.upgrade().is_some_and(|m| Arc::ptr_eq(&m, mesh))
        {
            return bvh.clone();
        }
    }
    drop(graveyard);
    if small {
        return None;
    }
    let _span =
        tracing::info_span!(target: "regia_pick", "bvh_build", triangles = mesh.indices.len() / 3).entered();
    let bvh = MeshBvh::build(mesh).map(Arc::new);
    let old = {
        let mut guard = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .get_or_insert_with(HashMap::new)
            .insert(key, (Arc::downgrade(mesh), (counts, bvh.clone())))
    };
    drop(old);
    bvh
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::box_mesh;

    fn grid(triangles: usize) -> Arc<CpuMesh> {
        let template = box_mesh([0.5; 3]).0[0];
        let vertices = (0..triangles + 2)
            .map(|i| {
                let mut v = template;
                v.position = [i as f32, (i % 3) as f32, 0.0];
                v
            })
            .collect();
        let indices = (0..triangles as u32).flat_map(|t| [t, t + 1, t + 2]).collect();
        Arc::new(CpuMesh { vertices, indices })
    }

    #[test]
    fn a_freed_mesh_releases_its_tree_on_the_next_call_of_any_size() {
        let big = grid(BVH_MIN_TRIANGLES * 2);
        let tree = bvh_for(&big).expect("tree");
        let probe = Arc::downgrade(&tree);
        drop(tree);
        assert!(probe.upgrade().is_some(), "cached while the mesh lives");
        drop(big);
        // A small mesh never builds a tree but still sweeps.
        assert!(bvh_for(&grid(3)).is_none());
        assert!(probe.upgrade().is_none());
    }

    #[test]
    fn shrinking_a_live_mesh_drops_the_old_tree() {
        let mut big = grid(BVH_MIN_TRIANGLES * 2);
        let tree = bvh_for(&big).expect("tree");
        let probe = Arc::downgrade(&tree);
        drop(tree);
        // The cache's weak handle blocks `Arc::get_mut`: an edit takes the mesh out of its `Arc`.
        assert!(Arc::get_mut(&mut big).is_none());
        let mut edited = Arc::into_inner(big).expect("only strong handle");
        edited.indices.truncate(9);
        let edited = Arc::new(edited);
        assert!(bvh_for(&edited).is_none());
        assert!(probe.upgrade().is_none());
    }

    #[test]
    fn a_resized_live_mesh_is_rebuilt() {
        let mut big = grid(BVH_MIN_TRIANGLES * 2);
        let first = bvh_for(&big).expect("tree");
        assert!(Arc::get_mut(&mut big).is_none());
        let mut edited = Arc::into_inner(big).expect("only strong handle");
        edited.indices.truncate(BVH_MIN_TRIANGLES * 3);
        let edited = Arc::new(edited);
        let second = bvh_for(&edited).expect("tree");
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn sweep_dead_returns_the_dead_values() {
        let live = grid(3);
        let mut map: HashMap<usize, (Weak<CpuMesh>, u32)> = HashMap::new();
        map.insert(1, (Arc::downgrade(&live), 10));
        map.insert(2, (Arc::downgrade(&grid(3)), 20));
        assert_eq!(sweep_dead(&mut map), vec![20]);
        assert_eq!(map.len(), 1);
    }

    fn vertex(position: [f32; 3]) -> regia_render::LabVertex {
        regia_render::LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], [0.0, 0.0])
    }

    /// Copies of the z = 0 triangle (0,0)-(2,0)-(0,2), one per depth.
    fn layers(zs: &[f32]) -> CpuMesh {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        for &z in zs {
            let base = vertices.len() as u32;
            vertices.extend([
                vertex([0.0, 0.0, z]),
                vertex([2.0, 0.0, z]),
                vertex([0.0, 2.0, z]),
            ]);
            indices.extend([base, base + 1, base + 2]);
        }
        CpuMesh { vertices, indices }
    }

    #[test]
    fn triangle_indices_follows_the_copied_list_not_the_leaves() {
        let mut mesh = layers(&[0.0]);
        // The NaN triple is stored in the copied list but not traversed; the trailing pair is not a triangle.
        mesh.vertices.push(vertex([f32::NAN, 0.0, 0.0]));
        mesh.vertices.push(vertex([1.0, 0.0, 0.0]));
        mesh.vertices.push(vertex([0.0, 1.0, 0.0]));
        mesh.indices.extend([3, 4, 5, 0, 1]);
        let bvh = MeshBvh::build(&mesh).expect("the finite triangle remains");
        assert_eq!(bvh.triangle_indices(0), Some([0, 1, 2]));
        assert_eq!(bvh.triangle_indices(1), Some([3, 4, 5]));
        // Triangle 2 would start at index 6, one past the trailing pair.
        assert_eq!(bvh.triangle_indices(2), None);
        assert_eq!(bvh.triangle_indices(u32::MAX), None);

        let origin = Vec3::new(0.5, 0.5, 5.0);
        let (triangle, hit) = bvh
            .nearest(&mesh, origin, Vec3::NEG_Z, false, FaceCull::None)
            .expect("finite triangle");
        assert_eq!(triangle, 0);
        assert_eq!(hit.t, 5.0);
        assert_eq!(hit.barycentric, Vec3::new(0.5, 0.25, 0.25));

        // The copy is what a later index edit would miss: the tree keeps the list it was built from.
        mesh.indices = vec![0, 0, 0];
        assert_eq!(bvh.triangle_indices(0), Some([0, 1, 2]));
        let again = bvh
            .nearest(&mesh, origin, Vec3::NEG_Z, false, FaceCull::None)
            .expect("still the copied triple");
        assert_eq!(again.0, 0);
        assert_eq!(again.1.t, 5.0);

        mesh.indices = vec![0, 1];
        assert!(MeshBvh::build(&mesh).is_none());
    }

    #[test]
    fn nearest_keeps_the_closest_and_the_lower_index_on_a_tie() {
        // Five coplanar copies force a median split (leaves hold four). Equal t keeps index 0,
        // which sits in the child visited second.
        let tied = layers(&[0.0, 0.0, 0.0, 0.0, 0.0]);
        let bvh = MeshBvh::build(&tied).expect("split");
        let origin = Vec3::new(0.5, 0.5, 5.0);
        let (triangle, hit) = bvh
            .nearest(&tied, origin, Vec3::NEG_Z, false, FaceCull::None)
            .expect("tie");
        assert_eq!(triangle, 0);
        assert_eq!(hit.t, 5.0);
        assert_eq!(hit.barycentric, Vec3::new(0.5, 0.25, 0.25));
        assert!(hit.front_face);

        // `nearest` does not normalise: t is the parameter along the given direction.
        let (scaled, scaled_hit) = bvh
            .nearest(&tied, origin, Vec3::new(0.0, 0.0, -2.0), false, FaceCull::None)
            .expect("scaled");
        assert_eq!(scaled, 0);
        assert_eq!(scaled_hit.t, 2.5);

        let (edge, edge_hit) = bvh
            .nearest(
                &tied,
                Vec3::new(1.0, 0.0, 4.0),
                Vec3::NEG_Z,
                false,
                FaceCull::None,
            )
            .expect("edge");
        assert_eq!(edge, 0);
        assert_eq!(edge_hit.t, 4.0);
        assert_eq!(edge_hit.barycentric, Vec3::new(0.5, 0.5, 0.0));

        assert!(
            bvh.nearest(
                &tied,
                Vec3::new(5.0, 5.0, 5.0),
                Vec3::NEG_Z,
                false,
                FaceCull::None
            )
            .is_none()
        );
        assert!(
            bvh.nearest(&tied, origin, Vec3::Z, false, FaceCull::None)
                .is_none()
        );
        assert!(
            bvh.nearest(
                &tied,
                Vec3::new(0.5, 0.5, 0.0),
                Vec3::NEG_Z,
                false,
                FaceCull::None
            )
            .is_none()
        );
        assert!(
            bvh.nearest(&tied, origin, Vec3::ZERO, false, FaceCull::None)
                .is_none()
        );

        // Seen from behind, back-face culling drops the hit; a mirror flips a front hit into one.
        let behind = Vec3::new(0.5, 0.5, -4.0);
        assert!(
            bvh.nearest(&tied, behind, Vec3::Z, false, FaceCull::Back)
                .is_none()
        );
        let back = bvh
            .nearest(&tied, behind, Vec3::Z, false, FaceCull::None)
            .expect("back");
        assert_eq!(back.0, 0);
        assert_eq!(back.1.t, 4.0);
        assert!(!back.1.front_face);
        assert!(
            bvh.nearest(&tied, origin, Vec3::NEG_Z, true, FaceCull::Back)
                .is_none()
        );
        let flipped = bvh
            .nearest(&tied, origin, Vec3::NEG_Z, true, FaceCull::None)
            .expect("flipped front");
        assert!(!flipped.1.front_face);
        assert_eq!(flipped.1.t, 5.0);

        // Index 1 is the only close layer, and it sorts into the child visited first. Closer
        // still beats a lower index.
        let stacked = layers(&[0.0, 8.0, 0.0, 0.0, 0.0]);
        let bvh = MeshBvh::build(&stacked).expect("stacked");
        let (closest, close_hit) = bvh
            .nearest(
                &stacked,
                Vec3::new(0.5, 0.5, 20.0),
                Vec3::NEG_Z,
                false,
                FaceCull::None,
            )
            .expect("closest");
        assert_eq!(closest, 1);
        assert_eq!(close_hit.t, 12.0);
        assert_eq!(close_hit.barycentric, Vec3::new(0.5, 0.25, 0.25));
        assert!(close_hit.front_face);
    }

    #[test]
    fn a_degenerate_triangle_is_stored_and_a_short_mesh_skips_missing_vertices() {
        let flat = CpuMesh {
            vertices: vec![
                vertex([0.0, 0.0, 0.0]),
                vertex([1.0, 0.0, 0.0]),
                vertex([2.0, 0.0, 0.0]),
            ],
            indices: vec![0, 1, 2],
        };
        let bvh = MeshBvh::build(&flat).expect("finite, even with no area");
        assert_eq!(bvh.triangle_indices(0), Some([0, 1, 2]));
        assert!(
            bvh.nearest(
                &flat,
                Vec3::new(1.0, 0.0, 5.0),
                Vec3::NEG_Z,
                false,
                FaceCull::None
            )
            .is_none()
        );

        let mut wide = layers(&[0.0]);
        let base = wide.vertices.len() as u32;
        wide.vertices.extend([
            vertex([10.0, 0.0, 0.0]),
            vertex([12.0, 0.0, 0.0]),
            vertex([10.0, 2.0, 0.0]),
        ]);
        wide.indices.extend([base, base + 1, base + 2]);
        let bvh = MeshBvh::build(&wide).expect("two triangles");
        let at_second = Vec3::new(10.5, 0.5, 5.0);
        let (triangle, hit) = bvh
            .nearest(&wide, at_second, Vec3::NEG_Z, false, FaceCull::None)
            .expect("second triangle");
        assert_eq!(triangle, 1);
        assert_eq!(hit.t, 5.0);
        assert_eq!(hit.barycentric, Vec3::new(0.5, 0.25, 0.25));

        let short = CpuMesh {
            vertices: wide.vertices[..3].to_vec(),
            indices: wide.indices.clone(),
        };
        assert!(
            bvh.nearest(&short, at_second, Vec3::NEG_Z, false, FaceCull::None)
                .is_none()
        );
        let (kept, kept_hit) = bvh
            .nearest(
                &short,
                Vec3::new(0.5, 0.5, 5.0),
                Vec3::NEG_Z,
                false,
                FaceCull::None,
            )
            .expect("first triangle's vertices remain");
        assert_eq!(kept, 0);
        assert_eq!(kept_hit.t, 5.0);
    }
}
