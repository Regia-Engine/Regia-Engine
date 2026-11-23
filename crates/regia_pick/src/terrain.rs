// SPDX-License-Identifier: MPL-2.0
//! The CPU terrain ray: a fixed-step march of the heightfield with a bisection on the bracketing
//! step, passing through holes.

use regia_core::math::Vec3;
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::World;
use regia_terrain::{LatticeHoleCut, Terrain, TerrainMeshLayers, terrain_holes};

/// A ray hit on a [`Terrain`] heightfield.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerrainHit {
    /// The terrain entity.
    pub entity: Entity,
    /// Distance along the unit ray, metres.
    pub distance: f32,
    /// Hit point in local coordinates (the ray's frame). The march adds `OriginOffset` and
    /// subtracts it from this point (spec 78 § 12.2). With a zero offset this is world space.
    pub point: Vec3,
    /// Unit surface normal of the heightfield (a direction, so a rebase does not change it).
    pub normal: Vec3,
}

/// Whether the terrain surface is cut at world XZ: the mask the host's linked draw applies
/// (`TerrainHoles`, once a draw applied one), else the component's own [`Terrain::holes`]. A hole
/// a mesh layer fills is cut on its lattice triangles, as drawn (`lattice`, ADR-8537).
fn cut_at(world: &World, terrain: &Terrain, lattice: Option<&LatticeHoleCut>, x: f32, z: f32) -> bool {
    terrain_holes(world, terrain).is_some_and(|mask| mask.contains_drawn(x, z, lattice))
}

/// Ray-marches every [`Terrain`] heightfield (world-centred, per its map) and returns the nearest
/// hit within `max_distance`: fixed steps of half the finer sample spacing (at least 1 mm, and
/// never less than one `f32` ulp of the ray distance; the step grows so one terrain takes at most
/// 2^20 steps across the part of the ray inside the map's bounding box, which can exceed half a
/// sample spacing on a very fine, very wide map), then a bisection on the bracketing step; the hit
/// counts only when the bisected point lies inside the map square. A crossing inside a hole is skipped, so a ray over a tunnel mouth passes
/// through to what lies below. Only crossings from above count (the surface has no underside).
#[must_use]
pub fn raycast_terrain(
    world: &World,
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
) -> Option<TerrainHit> {
    let direction = direction.normalize_or_zero();
    if direction == Vec3::ZERO || !origin.is_finite() {
        return None;
    }
    let mut best: Option<TerrainHit> = None;
    for entity_ref in world.iter_entities() {
        if !entity_ref.contains::<Terrain>() {
            continue;
        }
        let limit = best.map_or(max_distance, |h| h.distance);
        if let Some(hit) = raycast_terrain_entity(world, entity_ref.id(), origin, direction, limit) {
            // Equal distances (coplanar terrains) go to the earlier arrival, not storage order.
            let wins = best.is_none_or(|b| {
                hit.distance < b.distance
                    || (hit.distance == b.distance
                        && crate::world::terrain_key(world, hit.entity)
                            < crate::world::terrain_key(world, b.entity))
            });
            if wins {
                best = Some(hit);
            }
        }
    }
    best
}

/// [`raycast_terrain`] for one terrain entity.
#[must_use]
pub fn raycast_terrain_entity(
    world: &World,
    entity: Entity,
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
) -> Option<TerrainHit> {
    let direction = direction.normalize_or_zero();
    if direction == Vec3::ZERO || !origin.is_finite() || max_distance.is_nan() {
        return None;
    }
    let terrain = world.get::<Terrain>(entity)?;
    // The heightfield keeps its world-anchored space; the ray and the answer are local (spec 78
    // § 12.2, ADR-2022). Zero without a floating-origin rebase.
    let offset = world
        .get_resource::<regia_scene::streaming::OriginOffset>()
        .map_or(Vec3::ZERO, |o| Vec3::from_array(o.0.map(|axis| axis as f32)));
    let at = origin + offset;
    // Mesh layers: filled holes cut on the lattice, open mouths' bands are terrain surface
    // (spec 26 § 2a.6 "Pick", ADR-8537). Without the component the march is the base's.
    let layers = world.get::<TerrainMeshLayers>(entity);
    let lattice = layers.and_then(|l| l.lattice_hole_cut(&terrain.map));
    let ground = march(world, terrain, lattice.as_ref(), at, direction, max_distance);
    let band = layers.and_then(|l| {
        l.raycast_bands(
            at.to_array(),
            direction.to_array(),
            ground.unwrap_or(max_distance),
        )
        .filter(|b| ground.is_none_or(|g| b.distance < g))
    });
    if let Some(band) = band {
        return Some(TerrainHit {
            entity,
            distance: band.distance,
            point: Vec3::from_array(band.point) - offset,
            normal: Vec3::from_array(band.normal),
        });
    }
    let t = ground?;
    let point = at + direction * t;
    Some(TerrainHit {
        entity,
        distance: t,
        point: point - offset,
        normal: Vec3::from_array(terrain.map.normal_at(point.x, point.z)),
    })
}

/// Upper bound on march steps per terrain (as `regia_terrain::raycast_with_holes`): a ray over a
/// very fine map at a huge distance ends instead of spinning.
const MAX_STEPS: u32 = 1 << 20;

/// One terrain march. The pyramid walk and the leaf march share this so neither function takes
/// more arguments than clippy allows.
struct March<'a> {
    world: &'a World,
    terrain: &'a Terrain,
    /// The lattice cut of the terrain's filled holes (mesh layers), if any.
    lattice: Option<&'a LatticeHoleCut>,
    origin: Vec3,
    direction: Vec3,
    /// The sample grid starts at the ray's entry into the map box; every leaf snaps onto it.
    grid_near: f32,
    step: f32,
    max_distance: f32,
    /// The nearest crossing so far; a node the ray enters at or past it cannot improve.
    best: Option<f32>,
    /// Steps spent across every leaf of this terrain.
    steps: u32,
}

fn march(
    world: &World,
    terrain: &Terrain,
    lattice: Option<&LatticeHoleCut>,
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
) -> Option<f32> {
    let map = &terrain.map;
    // The map's square, moved by its grid origin (ADR-8179).
    let ([min_x, min_z], [max_x, max_z]) = (map.world_min(), map.world_max());
    // The grid may be rectangular (both axes span `world_width`): step by the finer axis.
    let spacing = map
        .sample_spacing()
        .min(map.world_width() / (map.height() - 1) as f32);
    let step = (spacing * 0.5).max(1e-3);
    // Only the part of the ray inside the terrain's bounding box can cross the surface: this also
    // bounds an unbounded `max_distance`.
    let (low, high) = map.min_max_height();
    let pad = 1e-3;
    let lo = Vec3::new(min_x - pad, low - pad, min_z - pad);
    let hi = Vec3::new(max_x + pad, high + pad, max_z + pad);
    let (mut near, mut far) = (0.0_f32, max_distance);
    for axis in 0..3 {
        let (o, d) = (origin[axis], direction[axis]);
        if d.abs() < 1e-12 {
            if o < lo[axis] || o > hi[axis] {
                return None;
            }
            continue;
        }
        let (a, b) = ((lo[axis] - o) / d, (hi[axis] - o) / d);
        near = near.max(a.min(b));
        far = far.min(a.max(b));
        if near > far {
            return None;
        }
    }
    let limit = far;
    // Bound the work for a huge map or an unbounded ray: the step grows so `MAX_STEPS` samples
    // cover the whole span inside the box (the leaf loops below also stop at `MAX_STEPS`).
    let step = step.max((limit - near) / MAX_STEPS as f32);
    let mut march = March {
        world,
        terrain,
        lattice,
        origin,
        direction,
        grid_near: near,
        step,
        max_distance,
        best: None,
        steps: 0,
    };
    // Walk the min/max pyramid: a node whose height range the ray cannot meet is skipped
    // without sampling; only leaves the ray crosses are marched, on the shared grid.
    let mut stack = vec![(regia_terrain::QuadNode::root(terrain.root_level), near, limit)];
    while let Some((node, t0, t1)) = stack.pop() {
        crate::stats::note_terrain_node();
        if march.best.is_some_and(|best| t0 >= best) {
            continue;
        }
        let (min_x, min_z, size) = node.draw_rect(map, terrain.root_level, terrain.cdlod.grid_size);
        let [hmin, hmax] = terrain.minmax.node_min_max(node);
        let pad = 1e-3;
        let node_lo = Vec3::new(min_x - pad, hmin - pad, min_z - pad);
        let node_hi = Vec3::new(min_x + size + pad, hmax + pad, min_z + size + pad);
        let Some((enter, exit)) = segment_aabb(origin, direction, node_lo, node_hi, t0, t1) else {
            continue;
        };
        if march.best.is_some_and(|best| enter >= best) {
            continue;
        }
        if node.level == 0 {
            march_leaf(&mut march, enter.max(near), exit.min(limit));
        } else {
            for child in node.children() {
                stack.push((child, enter, exit));
            }
        }
    }
    march.best
}

/// Height above the bilinear surface at `t`. Only downward crossings (positive, then not) hit.
fn above_surface(march: &March<'_>, t: f32) -> f32 {
    crate::stats::note_terrain_sample();
    let point = march.origin + march.direction * t;
    point.y - march.terrain.map.sample_bilinear(point.x, point.z)
}

/// Ray segment against a padded box, clipped to `[t0, t1]`. The box is padded so a grazing ray
/// still enters a node whose height range it can cross.
fn segment_aabb(origin: Vec3, direction: Vec3, lo: Vec3, hi: Vec3, t0: f32, t1: f32) -> Option<(f32, f32)> {
    let pad = 1e-3;
    let (lo, hi) = (lo - Vec3::splat(pad), hi + Vec3::splat(pad));
    let mut near = t0;
    let mut far = t1;
    for axis in 0..3 {
        let (o, d) = (origin[axis], direction[axis]);
        if d.abs() < 1e-12 {
            if o < lo[axis] || o > hi[axis] {
                return None;
            }
            continue;
        }
        let (a, b) = ((lo[axis] - o) / d, (hi[axis] - o) / d);
        near = near.max(a.min(b));
        far = far.min(a.max(b));
        if near > far {
            return None;
        }
    }
    (far >= t0 && near <= t1).then_some((near.max(t0), far.min(t1)))
}

/// Half-step marches one leaf the ray crosses, on the grid anchored at the map-box entry, then
/// bisects the bracketing step. The hit counts only when the bisected point lies inside the map
/// square and outside a hole; only crossings from above count.
fn march_leaf(march: &mut March<'_>, enter: f32, exit: f32) {
    if enter >= exit {
        return;
    }
    let step = march.step;
    // The crossing can straddle the leaf's entry: start from the grid point at or just below
    // `enter` so the bracket (t, t + step) is checked the same way the whole-span march did.
    let k = ((enter - march.grid_near) / step).floor().max(0.0);
    let mut t = march.grid_near + k * step;
    let mut previous = above_surface(march, t);
    while t < exit && march.steps < MAX_STEPS {
        if march.best.is_some_and(|best| t >= best) {
            return;
        }
        // At a large `t` a 1 mm step vanishes in f32 rounding: always advance at least one ulp.
        let next = (t + step).max(t.next_up());
        let value = above_surface(march, next);
        march.steps += 1;
        // Any sign change brackets a crossing; the bisected point must lie in the map square (checked
        // below), so an exit-edge crossing between the last inside sample and the first outside one stays.
        if previous > 0.0 && value <= 0.0 {
            let (mut lo, mut hi) = (t, next);
            for _ in 0..24 {
                let mid = 0.5 * (lo + hi);
                if mid <= lo || mid >= hi {
                    // Adjacent floats: the bracket cannot narrow further.
                    break;
                }
                if above_surface(march, mid) > 0.0 {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            let hit = march.origin + march.direction * hi;
            // The heightfield clamps outside its square, so a crossing there is not a surface.
            let map = &march.terrain.map;
            let ([min_x, min_z], [max_x, max_z]) = (map.world_min(), map.world_max());
            let on_map = (min_x..=max_x).contains(&hit.x) && (min_z..=max_z).contains(&hit.z);
            if on_map
                && hi <= march.max_distance
                && !cut_at(march.world, march.terrain, march.lattice, hit.x, hit.z)
            {
                march.best = Some(match march.best {
                    None => hi,
                    Some(best) => best.min(hi),
                });
            }
        }
        previous = value;
        t = next;
    }
}

#[cfg(test)]
mod tests {
    use regia_core::math::Vec3;
    use regia_ecs::prelude::*;
    use regia_terrain::{
        CdlodConfig, Crater, HoleShape, HolesMask, MinMaxPyramid, Terrain, TerrainLayers, TerrainMap,
        apply_crater,
    };

    use super::{raycast_terrain, raycast_terrain_entity};

    fn flat(world: &mut World, height: f32, holes: Option<HolesMask>) -> Entity {
        let map = TerrainMap::from_heights(2, 2, vec![height; 4], 16.0, 1.0, 0.0).expect("map");
        let minmax = MinMaxPyramid::build(&map, 0).expect("pyramid");
        world
            .spawn(Terrain {
                map,
                cdlod: CdlodConfig::default(),
                root_level: 0,
                minmax,
                layers: TerrainLayers::default(),
                holes,
                water: None,
            })
            .id()
    }

    /// The documented public crater path deforms `Terrain::map` directly
    /// (`regia_terrain::apply_crater`), which leaves `Terrain::minmax` stale: the march prunes
    /// nodes by the pyramid's height range, so a pick straight into the crater is skipped until
    /// the pyramid is refreshed over the dirty rect (the second step `apply_crater`'s docs
    /// require). The engine's own edit paths always do both; this pins the contract for hosts
    /// that call the public path themselves.
    #[test]
    fn a_crater_needs_a_minmax_refresh_before_picks_see_it() {
        let mut world = World::new();
        let n = 129usize;
        let map =
            TerrainMap::from_heights(n as u32, n as u32, vec![10.0; n * n], 256.0, 1.0, 0.0).expect("map");
        let minmax = MinMaxPyramid::build(&map, 4).expect("pyramid");
        let ground = world
            .spawn(Terrain {
                map,
                cdlod: CdlodConfig::default(),
                root_level: 4,
                minmax,
                layers: TerrainLayers::default(),
                holes: None,
                water: None,
            })
            .id();
        let crater = Crater {
            center: [0.0, 0.0],
            radius: 16.0,
            depth: 6.0,
            rim_height: 1.0,
        };
        let dirty = {
            // The public path a host follows: deform the component's map directly.
            let mut terrain = world.get_mut::<Terrain>(ground).expect("terrain");
            apply_crater(&mut terrain.map, &crater).expect("valid crater")
        };
        let down = Vec3::new(0.0, 30.0, 0.0);
        let stale = raycast_terrain_entity(&world, ground, down, Vec3::NEG_Y, 100.0);
        assert!(
            stale.is_none(),
            "the stale pyramid prunes the leaf, so the pick misses the crater: {stale:?}"
        );
        {
            // The documented second step: refresh the min/max pyramid over the dirty rect.
            let mut terrain = world.get_mut::<Terrain>(ground).expect("terrain");
            let Terrain { map, minmax, .. } = &mut *terrain;
            minmax.update_region(map, &dirty);
        }
        let hit = raycast_terrain_entity(&world, ground, down, Vec3::NEG_Y, 100.0)
            .expect("hit after the documented refresh");
        assert!(
            (hit.point.y - 4.0).abs() < 0.1,
            "the pick finds the crater floor 6 m below the old surface: {:?}",
            hit.point
        );
    }

    /// Spec 78 § 12.2: rays are local, the heightfield stays in its world-anchored space.
    #[test]
    fn a_rebase_keeps_the_hit_in_local_coordinates() {
        let mut world = World::new();
        let ground = flat(&mut world, 2.0, None);
        let before = raycast_terrain_entity(&world, ground, Vec3::new(1.0, 12.0, -2.0), Vec3::NEG_Y, 100.0)
            .expect("hit before the rebase");
        let offset = Vec3::new(16.0, 0.0, -32.0);
        world.insert_resource(regia_scene::streaming::OriginOffset(offset.as_dvec3().to_array()));
        let after = raycast_terrain_entity(
            &world,
            ground,
            Vec3::new(1.0, 12.0, -2.0) - offset,
            Vec3::NEG_Y,
            100.0,
        )
        .expect("hit after the rebase");
        assert!(
            (after.point + offset - before.point).length() < 1e-3,
            "{:?}",
            after.point
        );
        assert_eq!(after.distance, before.distance);
    }

    /// A rectangular grid (2 columns, 17 rows): the Z spacing is far finer than X, and a ridge
    /// narrower than half the X spacing must still be hit.
    #[test]
    fn a_rectangular_grid_marches_by_the_finer_axis() {
        let mut world = World::new();
        let mut heights = vec![0.0_f32; 2 * 17];
        heights[8 * 2] = 10.0;
        heights[8 * 2 + 1] = 10.0;
        let map = TerrainMap::from_heights(2, 17, heights, 16.0, 1.0, 0.0).expect("map");
        let minmax = MinMaxPyramid::build(&map, 0).expect("pyramid");
        let ground = world
            .spawn(Terrain {
                map,
                cdlod: CdlodConfig::default(),
                root_level: 0,
                minmax,
                layers: TerrainLayers::default(),
                holes: None,
                water: None,
            })
            .id();
        let hit = raycast_terrain_entity(&world, ground, Vec3::new(0.0, 5.0, -4.0), Vec3::Z, 100.0);
        assert!(hit.is_some(), "the ridge between z = -0.5 and 0.5 is crossed");
    }

    /// A map matched to a cell-spaced simulation grid (ADR-8179: 64 samples over 320 m, 5 m
    /// spacing, grid origin −2.5 m): a ray straight down at a simulation sample hits that
    /// sample's stored height, and the moved map's edges bound the hits.
    #[test]
    fn a_ray_at_a_sim_sample_hits_its_height_on_a_moved_grid() {
        let mut world = World::new();
        let (n, sim) = (64u32, 320.0f32);
        let heights: Vec<f32> = (0..n)
            .flat_map(|r| (0..n).map(move |c| ((c * 7 + r * 13) % 23) as f32 * 0.5))
            .collect();
        let map = TerrainMap::from_heights(n, n, heights, sim * (n - 1) as f32 / n as f32, 1.0, 0.0)
            .and_then(|m| m.with_grid_origin([-0.5 * sim / n as f32; 2]))
            .expect("map");
        let minmax = MinMaxPyramid::build(&map, 3).expect("pyramid");
        let stored = |c: u32, r: u32| map.height_at_sample(i64::from(c), i64::from(r));
        let probes = [(0u32, 0u32), (32, 32), (17, 40), (63, 63), (5, 60)];
        let want: Vec<f32> = probes.iter().map(|&(c, r)| stored(c, r)).collect();
        let ground = world
            .spawn(Terrain {
                map,
                cdlod: CdlodConfig::default(),
                root_level: 3,
                minmax,
                layers: TerrainLayers::default(),
                holes: None,
                water: None,
            })
            .id();
        let sim_x = |i: u32| (i as f32 - (n / 2) as f32) * (sim / n as f32);
        for (&(c, r), want) in probes.iter().zip(want) {
            let (x, z) = (sim_x(c), sim_x(r));
            let hit = raycast_terrain_entity(&world, ground, Vec3::new(x, 50.0, z), Vec3::NEG_Y, 100.0)
                .unwrap_or_else(|| panic!("no hit at sample ({c}, {r})"))
                .point
                .y;
            assert!(
                (hit - want).abs() < 1e-3,
                "sample ({c}, {r}) at ({x}, {z}): hit {hit}, stored {want}"
            );
        }
        // The moved map spans -160 ..= 155 m: a ray just past the +X edge misses.
        assert!(
            raycast_terrain_entity(&world, ground, Vec3::new(155.5, 50.0, 0.0), Vec3::NEG_Y, 100.0).is_none()
        );
        assert!(
            raycast_terrain_entity(&world, ground, Vec3::new(-159.5, 50.0, 0.0), Vec3::NEG_Y, 100.0)
                .is_some()
        );
    }

    #[test]
    fn raycast_terrain_entity_rejects_missing_entities_and_bad_rays() {
        let mut world = World::new();
        let stray = world.spawn_empty().id();
        let ground = flat(&mut world, 0.0, None);
        let origin = Vec3::new(0.0, 12.0, 0.0);

        assert!(raycast_terrain_entity(&world, Entity::PLACEHOLDER, origin, Vec3::NEG_Y, 100.0).is_none());
        assert!(raycast_terrain_entity(&world, stray, origin, Vec3::NEG_Y, 100.0).is_none());
        assert!(raycast_terrain_entity(&world, ground, origin, Vec3::ZERO, 100.0).is_none());
        assert!(raycast_terrain_entity(&world, ground, origin, Vec3::NAN, 100.0).is_none());
        assert!(raycast_terrain_entity(&world, ground, Vec3::NAN, Vec3::NEG_Y, 100.0).is_none());
        assert!(raycast_terrain_entity(&world, ground, Vec3::INFINITY, Vec3::NEG_Y, 100.0).is_none());
        assert!(raycast_terrain_entity(&world, ground, origin, Vec3::NEG_Y, f32::NAN).is_none());
        assert!(raycast_terrain_entity(&world, ground, origin, Vec3::NEG_Y, 0.0).is_none());
        assert!(raycast_terrain_entity(&world, ground, origin, Vec3::NEG_Y, -1.0).is_none());
        // A direction whose length overflows f32 normalises to zero and misses.
        assert!(
            raycast_terrain_entity(&world, ground, origin, Vec3::new(0.0, -1.0e30, 0.0), 100.0).is_none()
        );

        let hit = raycast_terrain_entity(&world, ground, origin, Vec3::NEG_Y, 100.0).expect("hit");
        assert_eq!(hit.entity, ground);
        assert_eq!(hit.normal, Vec3::Y);
        assert_eq!(hit.point.x, 0.0);
        assert_eq!(hit.point.z, 0.0);
        assert_eq!(hit.point, origin + Vec3::NEG_Y * hit.distance);
        assert!(hit.distance > 11.0 && hit.distance < 13.0);
    }

    #[test]
    fn the_named_entity_is_hit_even_when_a_higher_terrain_is_closer() {
        let mut world = World::new();
        let low = flat(&mut world, 0.0, None);
        let high = flat(&mut world, 6.0, None);
        let origin = Vec3::new(1.0, 20.0, -2.0);

        let nearest = raycast_terrain(&world, origin, Vec3::NEG_Y, f32::INFINITY).expect("nearest");
        let low_hit = raycast_terrain_entity(&world, low, origin, Vec3::NEG_Y, f32::INFINITY).expect("low");
        let high_hit =
            raycast_terrain_entity(&world, high, origin, Vec3::NEG_Y, f32::INFINITY).expect("high");

        assert_eq!(nearest.entity, high);
        assert_eq!(low_hit.entity, low);
        assert_eq!(high_hit.entity, high);
        assert!(high_hit.distance < low_hit.distance);
        assert_eq!(low_hit.normal, Vec3::Y);
        assert_eq!(high_hit.normal, Vec3::Y);
        assert_eq!(low_hit.point.x, 1.0);
        assert_eq!(low_hit.point.z, -2.0);
        assert_eq!(high_hit.point.x, 1.0);
        assert_eq!(high_hit.point.z, -2.0);
        // A scaled direction is cast as the unit ray: same hit, bit for bit.
        let scaled = raycast_terrain_entity(&world, high, origin, Vec3::new(0.0, -4.0, 0.0), 100.0);
        assert_eq!(scaled, Some(high_hit));
        // The cap is the unit-ray distance, so a cap between the two surfaces keeps only the high one.
        assert!(raycast_terrain_entity(&world, low, origin, Vec3::NEG_Y, high_hit.distance).is_none());
        assert_eq!(
            raycast_terrain(&world, origin, Vec3::NEG_Y, high_hit.distance + 0.5).map(|h| h.entity),
            Some(high)
        );
    }

    #[test]
    fn only_a_downward_crossing_inside_the_map_square_hits() {
        let mut world = World::new();
        let ground = flat(&mut world, 0.0, None);
        let cast = |origin: Vec3, direction: Vec3, max_distance: f32| {
            raycast_terrain_entity(&world, ground, origin, direction, max_distance)
        };

        // From below, both upward (the surface has no underside) and further downward miss.
        assert!(cast(Vec3::new(0.0, -5.0, 0.0), Vec3::Y, 100.0).is_none());
        assert!(cast(Vec3::new(0.0, -5.0, 0.0), Vec3::NEG_Y, 100.0).is_none());
        // Starting exactly on the surface is not a crossing from above.
        assert!(cast(Vec3::ZERO, Vec3::NEG_Y, 100.0).is_none());
        // Parallel to an axis and outside the padded box, or above the surface and horizontal.
        assert!(cast(Vec3::new(20.0, 10.0, 0.0), Vec3::NEG_Y, 100.0).is_none());
        assert!(cast(Vec3::new(0.0, 10.0, 0.0), Vec3::X, 100.0).is_none());
        // Shorter than the gap from y = 12 down to the plane.
        assert!(cast(Vec3::new(0.0, 12.0, 0.0), Vec3::NEG_Y, 1.0).is_none());

        let hit = cast(Vec3::new(3.0, 12.0, -1.0), Vec3::NEG_Y, 100.0).expect("hit");
        assert_eq!(hit.entity, ground);
        assert_eq!((hit.point.x, hit.point.z), (3.0, -1.0));
        assert_eq!(hit.normal, Vec3::Y);
        assert!(hit.point.y.abs() < 1.0e-4);
    }

    #[test]
    fn a_hole_is_skipped_and_an_empty_mask_is_solid() {
        let mask = HolesMask::from_shapes(vec![HoleShape::Rect {
            center: [0.0, 0.0],
            half_extents: [2.0, 2.0],
            rotation_radians: 0.0,
        }])
        .expect("mask");
        let mut cut = World::new();
        let ground = flat(&mut cut, 0.0, Some(mask));
        let origin = Vec3::new(0.0, 12.0, 0.0);
        assert!(raycast_terrain_entity(&cut, ground, origin, Vec3::NEG_Y, 100.0).is_none());
        // The same ray through `raycast_terrain` agrees: the hole is not a surface.
        assert!(raycast_terrain(&cut, origin, Vec3::NEG_Y, 100.0).is_none());

        let beside = Vec3::new(5.0, 12.0, 0.0);
        let hit = raycast_terrain_entity(&cut, ground, beside, Vec3::NEG_Y, 100.0).expect("beside the hole");
        assert_eq!(hit.entity, ground);
        assert_eq!((hit.point.x, hit.point.z), (5.0, 0.0));
        assert_eq!(hit.normal, Vec3::Y);
        assert_eq!(hit.point, beside + Vec3::NEG_Y * hit.distance);

        let mut solid = World::new();
        let bare = flat(&mut solid, 0.0, None);
        let mut empty = World::new();
        let masked = flat(&mut empty, 0.0, Some(HolesMask::default()));
        let bare_hit = raycast_terrain_entity(&solid, bare, beside, Vec3::NEG_Y, 100.0).expect("bare");
        let empty_hit =
            raycast_terrain_entity(&empty, masked, beside, Vec3::NEG_Y, 100.0).expect("empty mask");
        assert_eq!(
            (empty_hit.distance, empty_hit.point, empty_hit.normal),
            (bare_hit.distance, bare_hit.point, bare_hit.normal)
        );
        assert_eq!(
            (hit.distance, hit.point, hit.normal),
            (bare_hit.distance, bare_hit.point, bare_hit.normal)
        );
    }

    #[test]
    fn a_slope_reports_the_uphill_normal_at_the_hit() {
        let mut world = World::new();
        // Row-major, row 0 = −Z. Height rises with +X, constant along Z, so the centre is y = 2.
        let map = TerrainMap::from_heights(2, 2, vec![0.0, 4.0, 0.0, 4.0], 16.0, 1.0, 0.0).expect("map");
        let minmax = MinMaxPyramid::build(&map, 0).expect("pyramid");
        let entity = world
            .spawn(Terrain {
                map,
                cdlod: CdlodConfig::default(),
                root_level: 0,
                minmax,
                layers: TerrainLayers::default(),
                holes: None,
                water: None,
            })
            .id();
        let origin = Vec3::new(0.0, 10.0, 0.0);
        let hit = raycast_terrain_entity(&world, entity, origin, Vec3::NEG_Y, 100.0).expect("slope");
        assert_eq!(hit.entity, entity);
        assert_eq!(hit.point.x, 0.0);
        assert_eq!(hit.point.z, 0.0);
        assert!((hit.point.y - 2.0).abs() < 1.0e-4);
        assert_eq!(hit.normal.z, 0.0);
        assert!(hit.normal.x < 0.0 && hit.normal.y > 0.0);
        let stored = world.get::<Terrain>(entity).expect("terrain");
        assert_eq!(
            hit.normal,
            Vec3::from_array(stored.map.normal_at(hit.point.x, hit.point.z))
        );
        assert_eq!(hit.point, origin + Vec3::NEG_Y * hit.distance);
    }
}
