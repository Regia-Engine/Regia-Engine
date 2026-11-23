// SPDX-License-Identifier: MPL-2.0
//! Hole-aware terrain ray casts for the runtime: game picking, AI line of sight, tools, the
//! agent protocol (`terrain.raycast`). A ray that crosses the surface inside a hole
//! ([`HolesMask`], a tunnel mouth) falls through it, as the rendered terrain and the physics
//! heightfield (NaN cells, [`HolesMask::cut_heights`]) do. Below the surface there is no rock:
//! like the collider, the heightfield is a surface, so a ray that went down a hole only hits
//! the terrain again by coming back up through it. Tunnel and lip meshes are the host's to test.
//!
//! The editor's `pick_terrain` and the brush pick call these, so the editor and a game agree.

use glam::Vec3;
use regia_app::prelude::*;

use crate::Terrain;
use crate::holes::{HolesMask, TerrainHoles};
use crate::sampler::TerrainMap;

/// Where a ray met a terrain ([`pick_terrain`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerrainRayHit {
    /// The [`Terrain`] entity hit.
    pub entity: Entity,
    /// World-space hit point.
    pub position: Vec3,
    /// Distance from the ray origin along the normalised direction, metres.
    pub distance: f32,
}

// March steps `raycast_with_holes` took on this thread. A rejected ray stays at 0.
#[cfg(test)]
thread_local! {
    static RAYCAST_STEPS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// First hit of the ray `origin + t · direction` with the terrain surface (the bilinear
/// heightfield) outside `holes`, searched up to `max_distance` metres; `None` when it misses
/// the map or every crossing lies in a hole. Marches at half the finer of the X and Z sample
/// spacings and bisects each crossing; a crossing inside a hole ([`HolesMask::contains`], the
/// same exact mask the renderer clips per pixel and `regia_pick` tests) is a miss. The
/// physics collider removes a wider ring of cells (any cell touching a cut sample), so a ray
/// can hit drawn ground the collider has no rock for; collider matching belongs in a
/// collider query, not the visual pick. `None` or an empty mask cuts nothing
/// ([`crate::raycast_terrain`]). A ray that misses the map's XZ extent and cached height
/// range returns without marching, and the march stops once the ray has left that box.
#[must_use]
pub fn raycast_with_holes(
    map: &TerrainMap,
    holes: Option<&HolesMask>,
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
) -> Option<Vec3> {
    let holes = holes.filter(|h| !h.is_empty());
    let cut = |x: f32, z: f32| holes.is_some_and(|h| h.contains(x, z));
    raycast_with_cut(map, &cut, origin, direction, max_distance)
}

/// [`raycast_with_holes`] with the cut test as a function of world XZ (`true`: the ray goes on
/// through). The march, the bisection and the sample grid are the same.
fn raycast_with_cut(
    map: &TerrainMap,
    cut: &dyn Fn(f32, f32) -> bool,
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
) -> Option<Vec3> {
    let dir = direction.try_normalize()?;
    let _span = tracing::info_span!(target: "regia_terrain", "raycast_with_holes").entered();
    // The map's rectangle (its grid origin and Z depth, ADR-8179 and ADR-8369); `±half` on a
    // square map on the world origin.
    let ([lo_x, lo_z], [hi_x, hi_z]) = (map.world_min(), map.world_max());
    let inside = |p: Vec3| lo_x <= p.x && p.x <= hi_x && lo_z <= p.z && p.z <= hi_z;
    let above = |p: Vec3| p.y - map.sample_bilinear(p.x, p.z);
    // Half the coarser spacing steps over a ridge that is only one fine-axis sample wide.
    let step = (map.sample_spacing().min(map.sample_spacing_z()) * 0.5).max(1e-3);
    let steps = ((max_distance / step).ceil() as u32).min(1 << 20);
    if steps == 0 {
        return None;
    }
    let (min_h, max_h) = map.min_max_height();
    // One step of slack so a crossing on the boundary stays on the same sample grid.
    let pad = step;
    let (t_enter, t_exit) = ray_box(
        origin,
        dir,
        max_distance,
        [lo_x - pad, min_h - pad, lo_z - pad],
        [hi_x + pad, max_h + pad, hi_z + pad],
    )?;
    // Same sample positions as a march from the origin (`i * step`). Skip the prefix
    // before the box and stop one step after leaving it.
    let i0 = ((t_enter / step).floor() as u32)
        .saturating_sub(1)
        .clamp(1, steps);
    let i1 = (((t_exit / step).ceil() as u32).saturating_add(1)).clamp(i0, steps);
    let mut prev_t = if i0 == 1 {
        0.0
    } else {
        ((i0 - 1) as f32 * step).min(max_distance)
    };
    let mut prev = above(origin + dir * prev_t);
    // A ray that starts on the surface (a returned hit, which bisection leaves a hair below
    // it) has not crossed it: neither side counts until the march leaves it.
    if prev_t == 0.0 && prev.abs() <= on_surface_tolerance(origin) {
        prev = 0.0;
    }
    for i in i0..=i1 {
        #[cfg(test)]
        RAYCAST_STEPS.with(|steps| steps.set(steps.get().saturating_add(1)));
        // The last step stops at `max_distance`: a crossing farther than that is not a hit.
        let t = (i as f32 * step).min(max_distance);
        let p = origin + dir * t;
        let d = above(p);
        // Above-to-below, or below-to-above: a ray that fell through a hole hits again
        // by coming back up through the surface.
        let downward = d <= 0.0 && prev > 0.0;
        let upward = d >= 0.0 && prev < 0.0;
        if inside(p) && (downward || upward) {
            let (mut lo, mut hi) = (prev_t, t);
            for _ in 0..24 {
                let mid = 0.5 * (lo + hi);
                let above_mid = above(origin + dir * mid) > 0.0;
                if downward {
                    if above_mid {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                } else if above_mid {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            let hit = origin + dir * hi;
            // Through a hole: the ray goes on. The pick follows the exact drawn mask, not
            // the collider's wider removed cells (see the fn docs).
            if !cut(hit.x, hit.z) {
                return Some(hit);
            }
        }
        prev_t = t;
        prev = d;
    }
    None
}

/// How far from the surface (metres) a ray origin still counts as on it: 1 mm, plus a few
/// `f32` steps of the origin's magnitude so a hit far from the world origin re-casts cleanly.
fn on_surface_tolerance(origin: Vec3) -> f32 {
    1e-3 + 8.0 * f32::EPSILON * origin.abs().max_element()
}

/// Enter and exit distances of `origin + t · dir` against an AABB, or `None` when the
/// segment misses. The slab test matches `regia_core::math::Ray::intersect_aabb`
/// (Kay & Kajiya, SIGGRAPH 1986) and also returns the exit.
fn ray_box(origin: Vec3, dir: Vec3, max_distance: f32, min: [f32; 3], max: [f32; 3]) -> Option<(f32, f32)> {
    // +∞ is a real reach (the march is still capped at 1 << 20 steps). NaN and a negative
    // reach miss. A zero direction never arrives here after `try_normalize`.
    if dir == Vec3::ZERO || max_distance.is_nan() || max_distance < 0.0 {
        return None;
    }
    let mut t_near = 0.0f32;
    let mut t_far = max_distance;
    for axis in 0..3 {
        let (o, d) = (origin[axis], dir[axis]);
        let (lo, hi) = (min[axis], max[axis]);
        let inv = d.recip();
        if d == 0.0 || !inv.is_finite() {
            if o < lo || o > hi {
                return None;
            }
            continue;
        }
        let (t1, t2) = ((lo - o) * inv, (hi - o) * inv);
        t_near = t_near.max(t1.min(t2));
        t_far = t_far.min(t1.max(t2));
        if t_far < t_near {
            return None;
        }
    }
    (t_far >= 0.0).then_some((t_near.max(0.0), t_far))
}

/// The holes [`terrain_holes`] found: the draw's applied mask (shared) or the component's own
/// (borrowed). Derefs to the [`HolesMask`]; neither copies it.
#[derive(Clone, Debug)]
pub enum TerrainHolesRef<'a> {
    /// The mask the terrain draw cuts with ([`TerrainHoles::applied_mask`]).
    Applied(std::sync::Arc<HolesMask>),
    /// The terrain's [`Terrain::holes`].
    Component(&'a HolesMask),
}

impl std::ops::Deref for TerrainHolesRef<'_> {
    type Target = HolesMask;

    fn deref(&self) -> &HolesMask {
        match self {
            Self::Applied(mask) => mask,
            Self::Component(mask) => mask,
        }
    }
}

/// The holes cut through `terrain`: the mask the terrain draw applied once it applied one
/// ([`TerrainHoles`] resource: `TerrainSharedDraw::set_holes`, `terrain.set_holes`,
/// `terrain.load_holes`), which is what is drawn, collided and lit, else its
/// [`Terrain::holes`]. `None` when that cuts nothing. Neither mask is copied.
#[must_use]
pub fn terrain_holes<'a>(world: &World, terrain: &'a Terrain) -> Option<TerrainHolesRef<'a>> {
    let holes = match world
        .get_resource::<TerrainHoles>()
        .and_then(TerrainHoles::applied_mask)
    {
        Some(applied) => TerrainHolesRef::Applied(applied),
        None => TerrainHolesRef::Component(terrain.holes.as_ref()?),
    };
    (!holes.is_empty()).then_some(holes)
}

/// Nearest hit of the ray with every [`Terrain`] in the world, passing through holes
/// ([`terrain_holes`], [`raycast_with_holes`]); `None` when it hits none within
/// `max_distance` metres. Terrains are world centred, per their maps.
#[must_use]
pub fn pick_terrain(
    world: &World,
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
) -> Option<TerrainRayHit> {
    let _span = tracing::info_span!(target: "regia_terrain", "pick_terrain").entered();
    let dir = direction.try_normalize()?;
    let mut best: Option<TerrainRayHit> = None;
    for entity_ref in world.iter_entities() {
        let Some(terrain) = entity_ref.get::<Terrain>() else {
            continue;
        };
        let holes = terrain_holes(world, terrain);
        let limit = best.map_or(max_distance, |b| b.distance);
        // A terrain with mesh layers cuts filled holes on the lattice and adds the open mouths'
        // bands (spec 26 § 2a.6 "Pick", ADR-8537); without layers the ray is the base's.
        let layers = entity_ref.get::<crate::mesh_layer::TerrainMeshLayers>();
        let found = match layers {
            None => raycast_with_holes(&terrain.map, holes.as_deref(), origin, dir, limit),
            Some(layers) => {
                let lattice = layers.lattice_hole_cut(&terrain.map);
                let mask = holes.as_deref().filter(|h| !h.is_empty());
                let cut = |x: f32, z: f32| mask.is_some_and(|h| h.contains_drawn(x, z, lattice.as_ref()));
                let ground = raycast_with_cut(&terrain.map, &cut, origin, dir, limit);
                let ground_t = ground.map(|p| (p - origin).length());
                match layers.raycast_bands(origin.to_array(), dir.to_array(), ground_t.unwrap_or(limit)) {
                    Some(band) if ground_t.is_none_or(|g| band.distance < g) => {
                        Some(Vec3::from_array(band.point))
                    }
                    _ => ground,
                }
            }
        };
        if let Some(position) = found {
            let distance = (position - origin).length();
            if best.is_none_or(|b| distance < b.distance) {
                best = Some(TerrainRayHit {
                    entity: entity_ref.id(),
                    position,
                    distance,
                });
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CdlodConfig, HoleShape, TerrainLayers};

    /// 256 m of 1 m samples: a 0.2 slope along x.
    fn sloped() -> Terrain {
        let n = 257u32;
        let heights = (0..n * n).map(|i| ((i % n) as f32 - 128.0) * 0.2).collect();
        let map = TerrainMap::from_heights(n, n, heights, 256.0, 100.0, -50.0).expect("map");
        let heightfield = map.to_heightfield();
        Terrain::new(
            &heightfield,
            256.0,
            100.0,
            -50.0,
            CdlodConfig::default(),
            4,
            TerrainLayers::default(),
        )
        .expect("terrain")
    }

    /// A 30 m mouth around (10, 0).
    fn mouth() -> HolesMask {
        HolesMask::from_shapes(vec![HoleShape::Polygon {
            points: vec![[-5.0, -12.0], [25.0, -14.0], [24.0, 13.0], [-4.0, 15.0]],
        }])
        .expect("mask")
    }

    #[test]
    fn a_ray_falls_through_the_hole_and_hits_the_ground_beyond() {
        let t = sloped();
        let down = Vec3::new(0.0, -1.0, 0.0);
        let origin = Vec3::new(10.0, 50.0, 0.0);
        let solid = raycast_with_holes(&t.map, None, origin, down, 200.0).expect("hits the slope");
        assert!((solid.y - 2.0).abs() < 1e-2, "{solid}");
        assert!(raycast_with_holes(&t.map, Some(&mouth()), origin, down, 200.0).is_none());
        // A slanted ray through the mouth: no hit at the mouth, and none below (no rock).
        let slant = Vec3::new(0.2, -1.0, 0.1);
        assert!(raycast_with_holes(&t.map, Some(&mouth()), origin, slant, 400.0).is_none());
        // Outside the mouth the cut changes nothing.
        let beside = Vec3::new(60.0, 50.0, 0.0);
        let a = raycast_with_holes(&t.map, None, beside, down, 200.0);
        assert_eq!(a, raycast_with_holes(&t.map, Some(&mouth()), beside, down, 200.0));
        assert_eq!(a, crate::raycast_terrain(&t.map, beside, down, 200.0));
    }

    #[test]
    fn a_ray_into_the_mouth_never_hits_the_slope_from_below() {
        // From the low side, a shallow ray meets the surface just inside the mouth. Cut, it
        // goes on under the rising slope and never crosses it downward again: no hit (no rock
        // below the surface, as the physics heightfield).
        let t = sloped();
        let mask = mouth();
        let origin = Vec3::new(-20.0, 0.0, 0.0);
        let dir = Vec3::new(1.0, -0.05, 0.0);
        let uncut = raycast_with_holes(&t.map, None, origin, dir, 300.0).expect("uncut hit");
        assert!(
            mask.contains(uncut.x, uncut.z),
            "the uncut hit is inside the mouth: {uncut}"
        );
        assert!(raycast_with_holes(&t.map, Some(&mask), origin, dir, 300.0).is_none());
        // Aimed down beyond the mouth, a ray hits the slope there.
        let far = raycast_with_holes(
            &t.map,
            Some(&mask),
            Vec3::new(40.0, 30.0, 0.0),
            Vec3::new(1.0, -1.0, 0.0),
            300.0,
        )
        .expect("beyond the mouth");
        assert!(!mask.contains(far.x, far.z), "{far}");
    }

    /// A ray that starts under a flat surface and travels up crosses it (ADR-5083).
    #[test]
    fn a_ray_from_below_hits_the_surface_on_the_way_up() {
        let map = TerrainMap::from_heights(2, 2, vec![0.0; 4], 8.0, 1.0, 0.0).expect("map");
        let hit =
            raycast_with_holes(&map, None, Vec3::new(0.0, -5.0, 0.0), Vec3::Y, 20.0).expect("upward hit");
        assert!(hit.x.abs() < 1e-3 && hit.z.abs() < 1e-3, "{hit}");
        assert!(hit.y.abs() < 1e-3, "the flat surface is y = 0, hit {hit}");
    }

    /// A ray cast from a returned hit, away from the ground, starts on the surface: it does
    /// not hit that same point again at distance zero.
    #[test]
    fn a_ray_from_a_returned_hit_does_not_hit_its_own_start() {
        let t = sloped();
        for x in [-90.0f32, -3.7, 0.0, 10.0, 41.3, 100.0] {
            let p = raycast_with_holes(&t.map, None, Vec3::new(x, 100.0, 5.0), Vec3::NEG_Y, 300.0)
                .expect("down hit");
            let up = raycast_with_holes(&t.map, None, p, Vec3::Y, 50.0);
            assert!(up.is_none(), "x {x}: the ray from {p} hit {up:?}");
            // Away from the slope, uphill above the ground: still no self-hit.
            let away = Vec3::new(-0.3, 1.0, 0.2);
            let back = raycast_with_holes(&t.map, None, p, away, 50.0);
            assert!(back.is_none(), "x {x}: the ray from {p} hit {back:?}");
        }
        // A ray from just under the surface still records the upward crossing (ADR-5083).
        let p = raycast_with_holes(&t.map, None, Vec3::new(10.0, 100.0, 0.0), Vec3::NEG_Y, 300.0)
            .expect("down hit");
        let below = p - Vec3::Y * 0.5;
        let hit = raycast_with_holes(&t.map, None, below, Vec3::Y, 5.0).expect("upward hit");
        assert!((hit - p).length() < 1e-2, "{hit} vs {p}");
    }

    /// 3×17 samples over 16 m: X spacing 8 m, Z spacing 1 m. A 10 m ridge one row wide
    /// at z = 0 must stop a ray at y = 1. Half the X spacing (4 m) steps from z = −2 to
    /// z = 2, both still above the flats, and never sees the ridge.
    #[test]
    fn a_fine_z_ridge_is_not_stepped_over() {
        let (width, height) = (3u32, 17u32);
        let mut heights = vec![0.0f32; (width * height) as usize];
        for col in 0..width {
            heights[8 * width as usize + col as usize] = 10.0;
        }
        let map = TerrainMap::from_heights(width, height, heights, 16.0, 20.0, 0.0).expect("map");
        assert!(
            (map.sample_spacing() - 8.0).abs() < 1e-4,
            "{}",
            map.sample_spacing()
        );
        assert!(
            (map.sample_spacing_z() - 1.0).abs() < 1e-4,
            "{}",
            map.sample_spacing_z()
        );
        let hit = raycast_with_holes(&map, None, Vec3::new(0.0, 1.0, -6.0), Vec3::Z, 20.0)
            .expect("the ray at y = 1 meets the 10 m ridge");
        assert!(
            (-1.0..0.0).contains(&hit.z),
            "the crossing is on the rising face, hit {hit}"
        );
        assert!((hit.y - 1.0).abs() < 1e-2, "{hit}");
        assert!(hit.x.abs() < 1e-3, "{hit}");
    }

    /// Fix rev 2 (audfix2-a terrain-g finding 2): on a square grid the collider's cut is
    /// wider than the drawn hole, but the visual ray must follow the rendered mask exactly
    /// (like `regia_pick` and the per-pixel clip), not the collider's removed cell.
    #[test]
    fn a_ray_hits_visible_ground_beside_a_hole_rim() {
        // 1 m square grid; a 0.2 m hole at the origin cuts its sample's cell from the
        // collider, but the ground 0.5 m from the rim is drawn and must be pickable.
        let map = TerrainMap::from_heights(3, 3, vec![0.0; 9], 2.0, 1.0, 0.0).expect("map");
        assert!((map.sample_spacing() - 1.0).abs() < 1e-5);
        let hole = HolesMask::from_shapes(vec![HoleShape::Rect {
            center: [0.0, 0.0],
            half_extents: [0.1, 0.1],
            rotation_radians: 0.0,
        }])
        .expect("hole");
        // (0.5, 0.5): inside the cell the collider removed (corner (0, 0) is cut), outside
        // the hole itself.
        let hit = raycast_with_holes(&map, Some(&hole), Vec3::new(0.5, 10.0, 0.5), Vec3::NEG_Y, 20.0)
            .expect("visible ground beside the rim is hit");
        assert!((hit.x - 0.5).abs() < 1e-3 && (hit.z - 0.5).abs() < 1e-3, "{hit}");
        assert!(hit.y.abs() < 1e-3, "{hit}");
        // Straight down the hole still falls through.
        assert!(
            raycast_with_holes(&map, Some(&hole), Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y, 20.0).is_none(),
            "a ray down the hole falls through"
        );
        // Without holes the same rays hit the same ground.
        assert_eq!(
            raycast_with_holes(&map, None, Vec3::new(0.5, 10.0, 0.5), Vec3::NEG_Y, 20.0),
            Some(hit)
        );
    }

    #[test]
    fn pick_terrain_reads_the_component_then_the_draw_holes() {
        let mut app = App::new();
        let mut terrain = sloped();
        let entity = app.world_mut().spawn(terrain.clone()).id();
        let origin = Vec3::new(10.0, 50.0, 0.0);
        let down = Vec3::NEG_Y;
        let hit = pick_terrain(app.world(), origin, down, 500.0).expect("hit");
        assert_eq!(hit.entity, entity);
        assert!((hit.distance - 48.0).abs() < 1e-2, "{hit:?}");
        // Before a draw applied a mask, the component's own holes cut.
        let handle = TerrainHoles::default();
        app.world_mut().insert_resource(handle.clone());
        terrain.holes = Some(mouth());
        app.world_mut().entity_mut(entity).insert(terrain.clone());
        assert!(pick_terrain(app.world(), origin, down, 500.0).is_none());
        // Review holes #4: once the draw applied a mask, that is what is drawn and collided,
        // so it wins over a stale component (here the draw cut a trench elsewhere).
        handle.applied(Some(
            &HolesMask::from_shapes(vec![HoleShape::segment([100.0, 0.0], [110.0, 0.0], 4.0)]).expect("mask"),
        ));
        assert!(pick_terrain(app.world(), origin, down, 500.0).is_some());
        // The draw cuts the mouth (an unlinked handle still reports what was applied), the
        // component has none.
        handle.applied(Some(&mouth()));
        terrain.holes = None;
        app.world_mut().entity_mut(entity).insert(terrain);
        assert!(pick_terrain(app.world(), origin, down, 500.0).is_none());
        // An applied empty mask (holes removed) cuts nothing.
        handle.applied(None);
        assert!(pick_terrain(app.world(), origin, down, 500.0).is_some());
    }

    /// Review holes #3: the draw's mask reaches a pick shared, not copied per call.
    #[test]
    fn terrain_holes_shares_the_applied_mask() {
        let mut app = App::new();
        let terrain = sloped();
        let handle = TerrainHoles::default();
        handle.applied(Some(&mouth()));
        app.world_mut().insert_resource(handle.clone());
        let Some(TerrainHolesRef::Applied(first)) = terrain_holes(app.world(), &terrain) else {
            panic!("the applied mask");
        };
        let Some(TerrainHolesRef::Applied(second)) = terrain_holes(app.world(), &terrain) else {
            panic!("the applied mask");
        };
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "one shared mask, no copies"
        );
        let mut component = sloped();
        component.holes = Some(mouth());
        let mut bare = App::new();
        assert!(matches!(
            terrain_holes(bare.world_mut(), &component),
            Some(TerrainHolesRef::Component(m)) if std::ptr::eq(m, component.holes.as_ref().expect("holes"))
        ));
    }

    #[test]
    fn a_ray_above_the_height_slab_pointing_up_does_not_march() {
        let t = sloped();
        let (min_h, max_h) = t.map.min_max_height();
        assert!(max_h > min_h);
        let origin = Vec3::new(0.0, max_h + 10.0, 0.0);
        let step = (t.map.sample_spacing() * 0.5).max(1e-3);
        let budget = (4.0 * t.map.world_width()) / step;
        RAYCAST_STEPS.with(|steps| steps.set(0));
        assert!(raycast_with_holes(&t.map, None, origin, Vec3::Y, 4.0 * t.map.world_width()).is_none());
        let steps = RAYCAST_STEPS.with(std::cell::Cell::get);
        assert!(
            u64::from(steps) * 8 < budget as u64,
            "steps {steps}, full march {budget}"
        );
        // The same map still hits when the ray points down onto the slope.
        RAYCAST_STEPS.with(|steps| steps.set(0));
        let hit = raycast_with_holes(&t.map, None, origin, Vec3::NEG_Y, 4.0 * t.map.world_width())
            .expect("down hits");
        assert!(hit.y <= max_h + 1e-2 && hit.y >= min_h - 1e-2, "{hit}");
        let down_steps = RAYCAST_STEPS.with(std::cell::Cell::get);
        assert!(
            down_steps > 0 && u64::from(down_steps) * 4 < budget as u64,
            "{down_steps} of {budget}"
        );
    }
}
