// SPDX-License-Identifier: MPL-2.0
//! Dynamic obstacles (`docs/specs/22-tier2.md` § T2.2): a set of axis-aligned obstacle boxes that
//! [`crate::NavQuery`] carves out of the navmesh before planning.
//!
//! Carving is a lightweight stand-in for Recast's tile rebuild: the query re-polygonises the mesh
//! from its pristine cell grid with the boxes removed, so an obstacle that does not span the floor
//! leaves a route around it instead of blocking the single merged polygon that covers the whole
//! floor. A full rebuild is always available via [`crate::NavMeshBuilder`].

use std::sync::Mutex;

use regia_app::prelude::*;
use regia_core::math::Vec3;

use crate::links::links_fingerprint;
use crate::mesh::NavMesh;

/// One carved mesh kept for a mesh identity, a content generation and this obstacle set.
struct CarvedMesh {
    mesh_id: u64,
    content_stamp: u64,
    generation: u64,
    links_fp: u64,
    mesh: NavMesh,
}

/// The set of active obstacle boxes, maintained by gameplay.
///
/// Insert as a resource alongside [`crate::NavMesh`]; [`crate::NavQuery`] reads both and carves the
/// boxes out of the mesh when planning. The carved mesh is cached and rebuilt only when the boxes,
/// the mesh content or the off-mesh links change.
#[derive(Resource)]
pub struct NavObstacles {
    /// Active obstacles `(id, min, max)`.
    boxes: Vec<(u64, Vec3, Vec3)>,
    next_id: u64,
    /// Bumped when the boxes change, so a cached carve is not reused.
    generation: u64,
    /// The last carve. Absent from [`Clone`] and [`Debug`]: a clone plans from scratch.
    cache: Mutex<Option<CarvedMesh>>,
}

impl Default for NavObstacles {
    fn default() -> Self {
        Self {
            boxes: Vec::new(),
            next_id: 0,
            generation: 0,
            cache: Mutex::new(None),
        }
    }
}

impl Clone for NavObstacles {
    fn clone(&self) -> Self {
        Self {
            boxes: self.boxes.clone(),
            next_id: self.next_id,
            generation: self.generation,
            cache: Mutex::new(None),
        }
    }
}

impl std::fmt::Debug for NavObstacles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NavObstacles")
            .field("boxes", &self.boxes)
            .field("next_id", &self.next_id)
            .field("generation", &self.generation)
            .finish()
    }
}

impl NavObstacles {
    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// Adds a box obstacle and returns its id (the corners may be given in any order). The box's XZ footprint carves out every cell whose
    /// surface height lies inside `[min.y, max.y]`.
    ///
    /// ```
    /// use regia_core::math::Vec3;
    /// use regia_nav::NavObstacles;
    ///
    /// let mut obstacles = NavObstacles::default();
    /// let wall = obstacles.add_box(Vec3::new(-0.5, -1.0, -3.0), Vec3::new(0.5, 2.0, 3.0));
    /// assert_eq!(obstacles.obstacle_count(), 1);
    /// assert!(obstacles.remove(wall));
    /// assert_eq!(obstacles.obstacle_count(), 0);
    /// ```
    ///
    /// A box with a non-finite corner is refused, as [`Self::shift_origin`] refuses a non-finite
    /// step: nothing is stored and the id returned names no obstacle (ids are still never
    /// reused). [`Self::try_add_box`] returns the error instead.
    pub fn add_box(&mut self, min: Vec3, max: Vec3) -> u64 {
        match self.try_add_box(min, max) {
            Ok(id) => id,
            Err(_) => {
                tracing::warn!(target: "regia::nav", "a non-finite obstacle box was refused");
                self.take_id()
            }
        }
    }

    /// [`Self::add_box`] that reports a refusal. Errors:
    /// [`crate::NavTileCacheError::InvalidObstacle`] (code `nav_invalid_obstacle`) when a corner
    /// is not finite; nothing changes.
    pub fn try_add_box(&mut self, min: Vec3, max: Vec3) -> Result<u64, crate::NavTileCacheError> {
        if !min.is_finite() || !max.is_finite() {
            return Err(crate::NavTileCacheError::InvalidObstacle(
                "box corners must be finite",
            ));
        }
        let id = self.take_id();
        self.boxes.push((id, min.min(max), min.max(max)));
        self.bump_generation();
        Ok(id)
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// Moves every box by `delta` (the floating-origin step of spec 78 § 12.2).
    pub fn shift_origin(&mut self, delta: Vec3) {
        if !delta.is_finite() {
            return;
        }
        for (_, min, max) in &mut self.boxes {
            *min += delta;
            *max += delta;
        }
        self.bump_generation();
    }

    /// Removes the obstacle with `id`, reopening the surface it covered. Returns whether it
    /// existed.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.boxes.len();
        self.boxes.retain(|(bid, _, _)| *bid != id);
        let removed = self.boxes.len() != before;
        if removed {
            self.bump_generation();
        }
        removed
    }

    /// Runs `body` on the mesh with these boxes carved out, rebuilding that mesh only when
    /// `source` or the boxes changed since the last call. An empty set runs `body` on `source`
    /// and does not carve.
    pub(crate) fn with_carved<R>(&self, source: &NavMesh, body: impl FnOnce(&NavMesh) -> R) -> R {
        if self.boxes.is_empty() {
            return body(source);
        }
        let (mesh_id, content_stamp) = source.carve_identity();
        let links_fp = links_fingerprint(&source.off_mesh_links);
        let mut guard = self.cache.lock().unwrap_or_else(|poison| poison.into_inner());
        let stale = guard.as_ref().is_none_or(|cached| {
            cached.mesh_id != mesh_id
                || cached.content_stamp != content_stamp
                || cached.generation != self.generation
                || cached.links_fp != links_fp
        });
        if stale {
            let boxes: Vec<(Vec3, Vec3)> = self.boxes.iter().map(|&(_, min, max)| (min, max)).collect();
            *guard = Some(CarvedMesh {
                mesh_id,
                content_stamp,
                generation: self.generation,
                links_fp,
                mesh: source.carved(&boxes),
            });
        }
        match guard.as_ref() {
            Some(cached) => body(&cached.mesh),
            None => body(source),
        }
    }

    /// The active obstacle boxes `(min, max)`.
    pub(crate) fn boxes(&self) -> impl ExactSizeIterator<Item = (Vec3, Vec3)> + '_ {
        self.boxes.iter().map(|&(_, min, max)| (min, max))
    }

    /// Whether any obstacle is active.
    pub(crate) fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    /// How many obstacle boxes are active.
    pub fn obstacle_count(&self) -> usize {
        self.boxes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NavMeshBuildParams;
    use crate::heightfield::WalkableGrid;
    use crate::query::NavQuery;

    fn flat_mesh(size: usize) -> crate::mesh::NavMesh {
        let mut cells = vec![None; size * size];
        for z in 0..size {
            for x in 0..size {
                cells[z * size + x] = Some(0.0);
            }
        }
        let grid = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: size,
            height: size,
            cells,
            areas: Vec::new(),
        };
        crate::mesh::NavMesh::from_grid(&grid, NavMeshBuildParams::default())
    }

    /// Whether the open segment `a → b` passes through the interior of the axis-aligned box
    /// `[min, max]` (open in X and Z). Samples the segment densely so a path can be asserted to
    /// detour around an obstacle rather than cut through it.
    fn segment_crosses_box(a: Vec3, b: Vec3, min: Vec3, max: Vec3) -> bool {
        const N: usize = 64;
        for i in 1..N {
            let t = i as f32 / N as f32;
            let p = a + (b - a) * t;
            if p.x > min.x && p.x < max.x && p.z > min.z && p.z < max.z {
                return true;
            }
        }
        false
    }

    #[test]
    fn a_box_obstacle_adds_and_removes_cleanly() {
        let mut obstacles = NavObstacles::default();
        assert!(obstacles.is_empty());
        let id = obstacles.add_box(Vec3::new(2.0, -1.0, 2.0), Vec3::new(4.0, 2.0, 4.0));
        assert_eq!(obstacles.obstacle_count(), 1);
        assert!(!obstacles.is_empty());
        assert!(obstacles.remove(id));
        assert!(obstacles.is_empty());
        assert_eq!(obstacles.obstacle_count(), 0);
    }

    #[test]
    fn reversed_corners_still_carve() {
        let mesh = flat_mesh(10);
        let mut obstacles = NavObstacles::default();
        // Corners given max-first: must behave like the ordered box.
        obstacles.add_box(Vec3::new(6.0, 2.0, 6.0), Vec3::new(3.0, -1.0, 3.0));
        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&obstacles),
        };
        assert!(
            q.find_nearest(Vec3::new(4.5, 0.0, 4.5), Vec3::splat(0.4))
                .is_none(),
            "the carved centre is gone"
        );
    }

    #[test]
    fn removing_an_unknown_id_is_a_noop() {
        let mut obstacles = NavObstacles::default();
        obstacles.add_box(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0));
        let count = obstacles.obstacle_count();
        assert!(!obstacles.remove(999));
        assert_eq!(obstacles.obstacle_count(), count);
    }

    #[test]
    fn a_box_that_does_not_span_the_floor_leaves_a_path_around_it() {
        // A single merged 10x10 floor. A 3x3 box in the middle must not block the whole floor:
        // the query carves it out and routes around it.
        let mesh = flat_mesh(10);
        let mut obstacles = NavObstacles::default();
        obstacles.add_box(Vec3::new(3.0, -1.0, 3.0), Vec3::new(6.0, 2.0, 6.0));
        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&obstacles),
        };
        let path = q.find_path(
            Vec3::new(1.0, 0.0, 1.0),
            Vec3::new(8.0, 0.0, 8.0),
            Vec3::splat(2.0),
        );
        let path = path.expect("a route around the box exists");
        assert!(!path.waypoints.is_empty());
        // The path detours: the straight start→end diagonal would cut the box, so it must have an
        // intermediate corner (a 2-point path [(1,1),(8,8)] is the bug).
        assert!(
            path.waypoints.len() >= 3,
            "the path bends around the box instead of cutting it: {:?}",
            path.waypoints
        );
        // The gap beside the box is the portal (6,3)–(10,3). The straight line crosses z=3 at
        // x=3, so the funnel must bend at the near end (6, 3). The far end (10, 3) misses the
        // box interior and used to satisfy the checks below.
        assert!(
            path.waypoints
                .iter()
                .any(|w| (w.x - 6.0).abs() < 1e-2 && (w.z - 3.0).abs() < 1e-2),
            "the path must hug the near corner (6, 3), got {:?}",
            path.waypoints
        );
        assert!(
            path.waypoints
                .iter()
                .all(|w| (w.x - 10.0).abs() > 1e-2 || (w.z - 3.0).abs() > 1e-2),
            "the path must not lock the far portal end, got {:?}",
            path.waypoints
        );
        // No waypoint may fall inside the box footprint, and no segment may cross its interior.
        for w in &path.waypoints {
            let inside = w.x > 3.0 && w.x < 6.0 && w.z > 3.0 && w.z < 6.0;
            assert!(!inside, "waypoint inside the obstacle: {w:?}");
        }
        for pair in path.waypoints.windows(2) {
            assert!(
                !segment_crosses_box(
                    pair[0],
                    pair[1],
                    Vec3::new(3.0, 0.0, 3.0),
                    Vec3::new(6.0, 0.0, 6.0),
                ),
                "segment crosses the obstacle interior: {pair:?}"
            );
        }
        // Every sample stays in the union of the corridor polygons on the carved mesh. The
        // polygon indices in the path are those of this carve (same boxes, deterministic).
        let carved = mesh.carved(&obstacles.boxes().collect::<Vec<_>>());
        crate::query::assert_waypoints_in_corridor(&carved, &path.polys, &path.waypoints);
    }

    #[test]
    fn a_box_that_spans_the_floor_blocks_every_route() {
        // A full-width wall across the middle blocks the only corridor.
        let mesh = flat_mesh(10);
        let mut obstacles = NavObstacles::default();
        obstacles.add_box(Vec3::new(-1.0, -1.0, 4.0), Vec3::new(11.0, 2.0, 6.0));
        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&obstacles),
        };
        assert!(
            q.find_path(
                Vec3::new(5.0, 0.0, 1.0),
                Vec3::new(5.0, 0.0, 9.0),
                Vec3::splat(2.0)
            )
            .is_none(),
            "a spanning box blocks the corridor"
        );
        // Removing it reopens the route.
        obstacles.remove(0);
        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&obstacles),
        };
        assert!(
            q.find_path(
                Vec3::new(5.0, 0.0, 1.0),
                Vec3::new(5.0, 0.0, 9.0),
                Vec3::splat(2.0)
            )
            .is_some()
        );
    }

    #[test]
    fn a_box_above_the_floor_does_not_carve_it() {
        // The box footprint sits on the middle of the floor but spans y = 5..6, far above it.
        let mesh = flat_mesh(10);
        let mut obstacles = NavObstacles::default();
        obstacles.add_box(Vec3::new(3.0, 5.0, 3.0), Vec3::new(6.0, 6.0, 6.0));
        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&obstacles),
        };
        let path = q
            .find_path(
                Vec3::new(1.0, 0.0, 1.0),
                Vec3::new(8.0, 0.0, 8.0),
                Vec3::splat(2.0),
            )
            .expect("the floor is open");
        assert_eq!(path.waypoints.len(), 2, "straight through: {:?}", path.waypoints);
    }

    /// audit sprint-audit-g-nav-perf H3: two queries against one obstacle set carve once, and
    /// removing the box invalidates that carve.
    #[test]
    fn repeated_queries_carve_once() {
        let mesh = flat_mesh(10);
        let mut obstacles = NavObstacles::default();
        let id = obstacles.add_box(Vec3::new(3.0, -1.0, 3.0), Vec3::new(6.0, 2.0, 6.0));
        crate::mesh::reset_carve_builds();
        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&obstacles),
        };
        let _ = q.find_nearest(Vec3::new(1.0, 0.0, 1.0), Vec3::splat(1.0));
        let _ = q.find_nearest(Vec3::new(1.5, 0.0, 1.5), Vec3::splat(1.0));
        let carved = crate::mesh::carve_builds();
        assert_eq!(carved, 1, "two queries with one box carved {carved} times");

        assert!(obstacles.remove(id));
        let before = crate::mesh::carve_builds();
        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&obstacles),
        };
        assert!(
            q.find_nearest(Vec3::new(4.5, 0.0, 4.5), Vec3::splat(0.4))
                .is_some(),
            "the former hole is walkable again"
        );
        assert_eq!(
            crate::mesh::carve_builds(),
            before,
            "an empty obstacle set does not carve"
        );

        obstacles.add_box(Vec3::new(3.0, -1.0, 3.0), Vec3::new(6.0, 2.0, 6.0));
        let q = NavQuery {
            mesh: &mesh,
            obstacles: Some(&obstacles),
        };
        assert!(
            q.find_nearest(Vec3::new(4.5, 0.0, 4.5), Vec3::splat(0.4))
                .is_none()
        );
        assert_eq!(
            crate::mesh::carve_builds(),
            before + 1,
            "putting the box back carves once more, got {}",
            crate::mesh::carve_builds()
        );
    }

    #[test]
    fn try_add_box_refuses_a_non_finite_corner_with_the_obstacle_code() {
        let mut obstacles = NavObstacles::default();
        let generation = obstacles.generation;
        for (min, max) in [
            (Vec3::NAN, Vec3::ONE),
            (Vec3::ZERO, Vec3::new(1.0, f32::INFINITY, 1.0)),
            (Vec3::new(f32::NEG_INFINITY, 0.0, 0.0), Vec3::ONE),
        ] {
            let e = obstacles.try_add_box(min, max).expect_err("non-finite");
            assert_eq!(e.code(), "nav_invalid_obstacle");
        }
        assert_eq!(obstacles.obstacle_count(), 0);
        assert_eq!(obstacles.generation, generation, "a refusal changes nothing");
        let id = obstacles
            .try_add_box(Vec3::splat(2.0), Vec3::ZERO)
            .expect("finite corners in any order");
        assert_eq!(obstacles.boxes().next(), Some((Vec3::ZERO, Vec3::splat(2.0))));
        assert!(obstacles.remove(id));
    }
}
