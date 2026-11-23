// SPDX-License-Identifier: MPL-2.0
//! The world surface query (spec 87 § 4, ADR-1828): one answer over every water body kind.
//!
//! Level planes, swell planes, ribbons and the terrains' shoreline planes ([`super::TerrainWater`])
//! all answer through [`WaterBody::surface_on`]. Where bodies overlap, the highest wet surface
//! wins (ties: the lower entity), so a lake above the sea answers the lake and a hilltop under a
//! terrain plane with a shore field answers nothing.

use std::borrow::Cow;

use regia_app::prelude::*;
use regia_ecs::ecs::change_detection::Tick;
use regia_ecs::ecs::system::SystemParam;
use regia_render::RenderSettings;

use super::plugin::WaterSettings;
use super::shore::{ShoreField, WaterShore};
use super::{WaterBody, WaterClock, WaterHit, WaterSurface, WaterTier};

/// One body as the world query sees it: its entity, the body and its shore field if any.
#[derive(Debug, Clone, Copy)]
pub struct BodyRef<'a> {
    /// The body's entity.
    pub entity: Entity,
    /// The body.
    pub body: &'a WaterBody,
    /// Its baked shore ([`WaterShore`]), if any.
    pub shore: Option<&'a ShoreField>,
}

/// The highest wet surface over world `(x, z)` among `bodies` at the clock's tick; `shore` false
/// ignores every field (the runtime switch `WaterSettings::shore`).
#[must_use]
pub fn surface_at<'a>(
    bodies: impl IntoIterator<Item = BodyRef<'a>>,
    x: f32,
    z: f32,
    clock: &WaterClock,
    shore: bool,
) -> Option<(Entity, WaterSurface)> {
    let mut best: Option<(Entity, WaterSurface)> = None;
    for b in bodies {
        let field = if shore { b.shore } else { None };
        if !b.body.may_contain_xz(x, z, field.is_some()) {
            continue;
        }
        if let Some(s) = b.body.surface_on(x, z, clock, field) {
            let better = match &best {
                None => true,
                Some((e, top)) => s.height > top.height || (s.height == top.height && b.entity < *e),
            };
            if better {
                best = Some((b.entity, s));
            }
        }
    }
    best
}

/// The nearest crossing of the ray over all `bodies` ([`WaterBody::raycast_on`]); ties go to the
/// lower entity.
#[must_use]
pub fn raycast_all<'a>(
    bodies: impl IntoIterator<Item = BodyRef<'a>>,
    origin: [f32; 3],
    direction: [f32; 3],
    max_distance: f32,
    clock: &WaterClock,
    shore: bool,
) -> Option<(Entity, WaterHit)> {
    let mut best: Option<(Entity, WaterHit)> = None;
    for b in bodies {
        let field = if shore { b.shore } else { None };
        if let Some(hit) = b.body.raycast_on(origin, direction, max_distance, clock, field) {
            let better = match &best {
                None => true,
                Some((e, near)) => {
                    hit.distance < near.distance || (hit.distance == near.distance && b.entity < *e)
                }
            };
            if better {
                best = Some((b.entity, hit));
            }
        }
    }
    best
}

/// The bodies whose FFT ocean the renderer left to its swell on the last frame although the tier
/// draws oceans (ADR-3251): a grid too large for the device's limits, or a second spectrum. The
/// water plugin writes it each frame from [`super::WaterRenderer::oceans_drawn_into`]; queries
/// that follow the drawn surface (`WaterSettings::queries_follow_tier`) answer those bodies' swell
/// fallback, as the renderer drew them. Empty without a renderer (a server): the tier alone
/// decides.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct WaterDrawnOceans {
    /// Entities whose ocean the last frame drew as swell, sorted.
    pub swell_fallback: Vec<Entity>,
}

impl WaterDrawnOceans {
    /// Whether `entity`'s ocean is drawn under `tier` and the `ocean_draw` switch: the tier draws
    /// oceans, the switch is on and the renderer did not leave it to its swell.
    #[must_use]
    pub fn ocean_drawn(drawn: Option<&Self>, entity: Entity, tier: WaterTier, ocean_draw: bool) -> bool {
        ocean_draw
            && tier.ocean_size() > 0
            && drawn.is_none_or(|d| d.swell_fallback.binary_search(&entity).is_err())
    }
}

/// A body's drawn surface, cached for queries that follow the drawn surface (ADR-3251).
///
/// The water plugin keeps one on every body while `WaterSettings::queries_follow_tier` is on
/// ([`cache_drawn_water`]); [`WaterQuery`] reads it instead of rebuilding the drawn body on each
/// call, so steady queries allocate nothing for it. A cache that no longer matches (the body was
/// changed, the tier or the renderer's answer moved) is ignored and the body is rebuilt for that
/// call.
#[derive(Component, Debug, Clone)]
pub struct DrawnWaterCache {
    /// The tier, whether the ocean is drawn, and the body's last change it was built for.
    key: (WaterTier, bool, Tick),
    /// The drawn body when it differs from the authored one.
    body: Option<WaterBody>,
}

impl DrawnWaterCache {
    /// The drawn body when it differs from the authored one (`None`: the body draws as authored).
    #[must_use]
    pub fn body(&self) -> Option<&WaterBody> {
        self.body.as_ref()
    }
}

/// Rebuilds of drawn bodies [`cache_drawn_water`] made since the process started (ADR-3251).
static CACHE_REBUILDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Drawn bodies [`cache_drawn_water`] rebuilt since the process started: a steady scene adds none.
#[must_use]
pub fn drawn_cache_rebuilds() -> u64 {
    CACHE_REBUILDS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Keeps every body's [`DrawnWaterCache`] current while queries follow the drawn surface
/// (ADR-3251); a body whose key did not move is left alone (no allocation, no change).
pub fn cache_drawn_water(
    mut commands: Commands,
    settings: Option<Res<WaterSettings>>,
    render: Option<Res<RenderSettings>>,
    drawn: Option<Res<WaterDrawnOceans>>,
    mut bodies: Query<(Entity, Ref<WaterBody>, Option<&mut DrawnWaterCache>)>,
) {
    let Some((tier, draw)) = settings.and_then(|s| s.query_tier(render.as_deref())) else {
        return;
    };
    for (entity, body, cache) in &mut bodies {
        let ocean = WaterDrawnOceans::ocean_drawn(drawn.as_deref(), entity, tier, draw);
        let key = (tier, ocean, body.last_changed());
        if cache.as_ref().is_some_and(|c| c.key == key) {
            continue;
        }
        CACHE_REBUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let fresh = DrawnWaterCache {
            key,
            body: match body.drawn_with(tier, ocean) {
                Cow::Borrowed(_) => None,
                Cow::Owned(drawn) => Some(drawn),
            },
        };
        match cache {
            Some(mut cache) => *cache = fresh,
            None => {
                commands.entity(entity).insert(fresh);
            }
        }
    }
}

/// `bodies` as `tier` draws them with the `ocean_draw` switch ([`WaterBody::drawn_for`]), for
/// queries that follow the drawn surface (`WaterSettings::queries_follow_tier`, ADR-2843).
#[must_use]
pub fn drawn_bodies<'a>(
    bodies: impl IntoIterator<Item = BodyRef<'a>>,
    tier: WaterTier,
    ocean_draw: bool,
) -> Vec<(Entity, Cow<'a, WaterBody>, Option<&'a ShoreField>)> {
    bodies
        .into_iter()
        .map(|b| (b.entity, b.body.drawn_for(tier, ocean_draw), b.shore))
        .collect()
}

fn drawn_refs<'a>(
    drawn: &'a [(Entity, Cow<'_, WaterBody>, Option<&'a ShoreField>)],
) -> impl Iterator<Item = BodyRef<'a>> {
    drawn.iter().map(|(entity, body, shore)| BodyRef {
        entity: *entity,
        body: body.as_ref(),
        shore: *shore,
    })
}

/// One body as [`WaterQuery`] reads it: the entity, the body (with its change tick), its shore and
/// its drawn-surface cache.
type QueryBody = (
    Entity,
    Ref<'static, WaterBody>,
    Option<&'static WaterShore>,
    Option<&'static DrawnWaterCache>,
);

/// Gameplay's view of the water: the world surface and ray queries at the current water tick,
/// with the shore switch in effect (`WaterSettings::shore`; on without the plugin), and, with
/// `WaterSettings::queries_follow_tier`, the surface the tier draws.
#[derive(SystemParam)]
pub struct WaterQuery<'w, 's> {
    bodies: Query<'w, 's, QueryBody>,
    clock: Option<Res<'w, WaterClock>>,
    settings: Option<Res<'w, WaterSettings>>,
    origin: Option<Res<'w, regia_scene::streaming::OriginOffset>>,
    render: Option<Res<'w, RenderSettings>>,
    drawn: Option<Res<'w, WaterDrawnOceans>>,
}

impl WaterQuery<'_, '_> {
    /// The clock the queries answer at (the default clock without the plugin).
    #[must_use]
    pub fn clock(&self) -> WaterClock {
        self.clock.as_deref().copied().unwrap_or_default()
    }

    /// Whether shore fields are used.
    #[must_use]
    pub fn shore_enabled(&self) -> bool {
        self.settings.as_deref().is_none_or(|s| s.shore)
    }

    fn refs(&self) -> impl Iterator<Item = BodyRef<'_>> {
        self.bodies.iter().map(|(entity, body, shore, _)| BodyRef {
            entity,
            body: body.into_inner(),
            shore: shore.map(|s| s.field.as_ref()),
        })
    }

    /// Whether every body's [`DrawnWaterCache`] matches `tier` and `draw` (ADR-3251).
    fn caches_current(&self, tier: WaterTier, draw: bool) -> bool {
        self.bodies.iter().all(|(entity, body, _, cache)| {
            let ocean = WaterDrawnOceans::ocean_drawn(self.drawn.as_deref(), entity, tier, draw);
            cache.is_some_and(|c| c.key == (tier, ocean, body.last_changed()))
        })
    }

    /// The bodies as drawn, from their caches (check [`Self::caches_current`] first).
    fn cached_refs(&self) -> impl Iterator<Item = BodyRef<'_>> {
        self.bodies.iter().map(|(entity, body, shore, cache)| BodyRef {
            entity,
            body: cache
                .and_then(DrawnWaterCache::body)
                .unwrap_or_else(|| body.into_inner()),
            shore: shore.map(|s| s.field.as_ref()),
        })
    }

    /// The bodies as drawn, built for this call (no cache): each `ocean_drawn` from the
    /// renderer's report.
    fn drawn_now(
        &self,
        tier: WaterTier,
        draw: bool,
    ) -> Vec<(Entity, Cow<'_, WaterBody>, Option<&ShoreField>)> {
        self.bodies
            .iter()
            .map(|(entity, body, shore, _)| {
                let ocean = WaterDrawnOceans::ocean_drawn(self.drawn.as_deref(), entity, tier, draw);
                (
                    entity,
                    body.into_inner().drawn_with(tier, ocean),
                    shore.map(|s| s.field.as_ref()),
                )
            })
            .collect()
    }

    /// The local origin in the water's space. Bodies keep their authored world-anchored
    /// positions; queries speak local coordinates like the rest of the frame (spec 78 § 12.2,
    /// ADR-2022). Zero without a floating-origin rebase.
    fn origin(&self) -> [f32; 3] {
        self.origin
            .as_deref()
            .map_or([0.0; 3], |o| o.0.map(|axis| axis as f32))
    }

    /// The tier and `ocean_draw` switch the queries follow, when they follow one (ADR-2843).
    #[must_use]
    pub fn drawn_tier(&self) -> Option<(WaterTier, bool)> {
        self.settings
            .as_deref()
            .and_then(|s| s.query_tier(self.render.as_deref()))
    }

    /// The highest wet surface over `(x, z)` ([`surface_at`]).
    #[must_use]
    pub fn surface(&self, x: f32, z: f32) -> Option<(Entity, WaterSurface)> {
        let at = self.origin();
        let (clock, shore) = (self.clock(), self.shore_enabled());
        let found = match self.drawn_tier() {
            None => surface_at(self.refs(), x + at[0], z + at[2], &clock, shore),
            Some((tier, draw)) if self.caches_current(tier, draw) => {
                surface_at(self.cached_refs(), x + at[0], z + at[2], &clock, shore)
            }
            Some((tier, draw)) => {
                let drawn = self.drawn_now(tier, draw);
                surface_at(drawn_refs(&drawn), x + at[0], z + at[2], &clock, shore)
            }
        };
        found.map(|(entity, surface)| (entity, surface_to_local(surface, at)))
    }

    /// The nearest crossing of a ray ([`raycast_all`]).
    #[must_use]
    pub fn raycast(
        &self,
        origin: [f32; 3],
        direction: [f32; 3],
        max_distance: f32,
    ) -> Option<(Entity, WaterHit)> {
        let at = self.origin();
        let start = std::array::from_fn(|a| origin[a] + at[a]);
        let (clock, shore) = (self.clock(), self.shore_enabled());
        let found = match self.drawn_tier() {
            None => raycast_all(self.refs(), start, direction, max_distance, &clock, shore),
            Some((tier, draw)) if self.caches_current(tier, draw) => {
                raycast_all(self.cached_refs(), start, direction, max_distance, &clock, shore)
            }
            Some((tier, draw)) => {
                let drawn = self.drawn_now(tier, draw);
                raycast_all(drawn_refs(&drawn), start, direction, max_distance, &clock, shore)
            }
        };
        found.map(|(entity, mut hit)| {
            for (value, amount) in hit.point.iter_mut().zip(at) {
                *value -= amount;
            }
            hit.surface = surface_to_local(hit.surface, at);
            (entity, hit)
        })
    }
}

/// A surface answer in local coordinates.
fn surface_to_local(mut surface: WaterSurface, at: [f32; 3]) -> WaterSurface {
    surface.height -= at[1];
    surface.rest[0] -= at[0];
    surface.rest[1] -= at[2];
    surface
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::water_body::shore::ShoreBakeDesc;
    use crate::water_body::{RibbonPoint, Swell, WaterClock, WaterLook};

    fn e(i: u32) -> Entity {
        Entity::from_raw_u32(i).expect("entity index")
    }

    /// Six disjoint lakes: a sample inside one of them calls `surface_on` once.
    #[test]
    fn many_bodies_are_not_all_sampled_per_sphere() {
        let clock = WaterClock::default();
        let bodies: Vec<WaterBody> = (0..6)
            .map(|i| WaterBody::plane([i as f32 * 100.0, 0.0, 0.0], [10.0, 10.0], WaterLook::lake()))
            .collect();
        let refs = bodies.iter().enumerate().map(|(i, body)| BodyRef {
            entity: e(i as u32 + 1),
            body,
            shore: None,
        });
        crate::water_body::SURFACE_ON_CALLS.with(|calls| calls.set(0));
        let hit = surface_at(refs, 0.0, 0.0, &clock, false);
        assert!(hit.is_some(), "the sample sits in the first lake");
        assert_eq!(
            crate::water_body::SURFACE_ON_CALLS.with(std::cell::Cell::get),
            1,
            "bodies outside the sample's rectangle are not evaluated"
        );
    }

    /// The call counter is the calling thread's: `surface_on` calls made on another thread (every
    /// other test of this binary, under the parallel harness) are not counted. With a process-wide
    /// counter `many_bodies_are_not_all_sampled_per_sphere` failed at random (11 calls for 1).
    #[test]
    fn the_call_counter_ignores_other_threads() {
        let clock = WaterClock::default();
        let body = WaterBody::plane([0.0, 0.0, 0.0], [10.0, 10.0], WaterLook::lake());
        crate::water_body::SURFACE_ON_CALLS.with(|calls| calls.set(0));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..10 {
                    let _ = body.surface_on(0.0, 0.0, &clock, None);
                }
            });
        });
        assert!(body.surface_on(0.0, 0.0, &clock, None).is_some());
        assert_eq!(
            crate::water_body::SURFACE_ON_CALLS.with(std::cell::Cell::get),
            1,
            "calls made on another thread are not this test's"
        );
    }

    /// Spec 78 § 12.2: queries speak local coordinates, bodies stay in their authored space.
    #[test]
    fn a_rebase_keeps_the_query_in_local_coordinates() {
        use regia_ecs::ecs::system::SystemState;
        let mut world = World::new();
        world.spawn(WaterBody::plane(
            [0.0, 2.0, 0.0],
            [100.0, 100.0],
            WaterLook::lake(),
        ));
        let mut state: SystemState<WaterQuery> = SystemState::new(&mut world);
        let at_world = state
            .get(&world)
            .expect("query")
            .surface(30.0, -20.0)
            .map(|(_, s)| s)
            .expect("wet");
        // A local point this far away from the origin would be dry without the offset.
        world.insert_resource(regia_scene::streaming::OriginOffset([1024.0, 0.0, -1024.0]));
        let mut state: SystemState<WaterQuery> = SystemState::new(&mut world);
        let query = state.get(&world).expect("query");
        assert!(query.surface(30.0, -20.0).is_none());
        let local = query
            .surface(30.0 - 1024.0, -20.0 + 1024.0)
            .map(|(_, s)| s)
            .expect("wet");
        assert_eq!(local.height, at_world.height);
        assert_eq!(local.rest, [at_world.rest[0] - 1024.0, at_world.rest[1] + 1024.0]);
        let (_, hit) = query
            .raycast([30.0 - 1024.0, 12.0, -20.0 + 1024.0], [0.0, -1.0, 0.0], 100.0)
            .expect("hit");
        assert!((hit.point[0] - (30.0 - 1024.0)).abs() < 1e-3, "{:?}", hit.point);
        assert!((hit.point[1] - 2.0).abs() < 1e-3, "{:?}", hit.point);
    }

    /// The sea over a map with a 40 m hill in the middle and a lake on the hill.
    fn scene() -> (WaterBody, ShoreField, WaterBody, WaterBody) {
        let mut sea = WaterBody::plane([0.0, 0.0, 0.0], [200.0, 200.0], WaterLook::ocean());
        sea.swell = Some(Swell::open_sea([1.0, 0.0], 1.2, 30.0, 6));
        // Bed: 40 m deep at the edge, rising to +40 m at the center (radius 100 m shoreline).
        let bed = |x: f32, z: f32| 40.0 - 0.4 * (x * x + z * z).sqrt();
        let field = ShoreField::bake(
            &ShoreBakeDesc {
                level: 0.0,
                min: [-200.0, -200.0],
                max: [200.0, 200.0],
                cell: 1.0,
            },
            bed,
        )
        .expect("bake");
        let lake = WaterBody::plane([0.0, 30.0, 0.0], [10.0, 10.0], WaterLook::lake());
        let points = vec![
            RibbonPoint {
                position: [-150.0, 5.0, 120.0],
                width: 4.0,
                depth: 0.5,
                speed: Some(1.0),
            },
            RibbonPoint {
                position: [-130.0, 4.9, 120.0],
                width: 4.0,
                depth: 0.5,
                speed: Some(1.0),
            },
        ];
        let stream = WaterBody::ribbon(points, WaterLook::clear_stream());
        (sea, field, lake, stream)
    }

    #[test]
    fn highest_wet_surface_wins_on_every_kind() {
        let (sea, field, lake, stream) = scene();
        let clock = WaterClock {
            tick: 1234,
            ..WaterClock::default()
        };
        let refs = || {
            [
                BodyRef {
                    entity: e(1),
                    body: &sea,
                    shore: Some(&field),
                },
                BodyRef {
                    entity: e(2),
                    body: &lake,
                    shore: None,
                },
                BodyRef {
                    entity: e(3),
                    body: &stream,
                    shore: None,
                },
            ]
        };
        // The lake on the hill.
        let (who, s) = surface_at(refs(), 2.0, 3.0, &clock, true).expect("lake");
        assert_eq!(who, e(2));
        assert_eq!(s.height, 30.0);
        // A dry hillside: the sea's field says the bed is above the level.
        assert!(surface_at(refs(), 50.0, 0.0, &clock, true).is_none());
        // Without the field (switch off) the terrain-wide sea answers there.
        let (who, _) = surface_at(refs(), 50.0, 0.0, &clock, false).expect("sea, unshoaled");
        assert_eq!(who, e(1));
        // Open water far out: the sea, with depth and shoreline distance.
        let (who, s) = surface_at(refs(), 180.0, 0.0, &clock, true).expect("sea");
        assert_eq!(who, e(1));
        let depth = s.depth.expect("field depth");
        assert!((depth - (0.4 * 180.0 - 40.0)).abs() < 2.0, "{s:?}");
        assert!(s.shore_distance.is_some_and(|d| (d - 80.0).abs() < 4.0), "{s:?}");
        // The stream runs 5 m above the sea: it wins over the sea there.
        let (who, s) = surface_at(refs(), -140.0, 120.5, &clock, true).expect("stream");
        assert_eq!(who, e(3));
        assert_eq!(s.depth, Some(0.5));
        assert!(
            s.shore_distance.is_some_and(|d| (d - 1.5).abs() < 1.0e-3),
            "{s:?}"
        );
        // A ray straight down onto the lake hits the lake, not the sea under the hill.
        let (who, hit) =
            raycast_all(refs(), [1.0, 60.0, 1.0], [0.0, -1.0, 0.0], 200.0, &clock, true).expect("hit");
        assert_eq!(who, e(2));
        assert!((hit.distance - 30.0).abs() < 1.0e-3);
        // Over the dry hillside the ray finds no water with the field.
        assert!(raycast_all(refs(), [50.0, 60.0, 0.0], [0.0, -1.0, 0.0], 200.0, &clock, true).is_none());
    }

    /// Shoaled waves die out toward the shoreline and stay continuous; in deep water the
    /// shoaled surface equals the unshoaled one bit for bit.
    #[test]
    fn shoaling_fades_waves_at_the_shore() {
        let (sea, field, _, _) = scene();
        let clock = WaterClock {
            tick: 777,
            ..WaterClock::default()
        };
        // Far out (0.4 · 195 − 40 = 38 m, most of the longest wavelength) the field changes the
        // surface by well under a millimeter.
        let deep_x = 195.0;
        let a = sea.surface_on(deep_x, 3.0, &clock, None).expect("sea");
        let b = sea.surface_on(deep_x, 3.0, &clock, Some(&field)).expect("sea");
        assert!((a.height - b.height).abs() < 1.0e-3, "{a:?} {b:?}");
        // Over a field deeper than every wavelength the answer is the unshoaled one bit for bit.
        let abyss = ShoreField::bake(
            &ShoreBakeDesc {
                level: 0.0,
                min: [-200.0, -200.0],
                max: [200.0, 200.0],
                cell: 4.0,
            },
            |_, _| -500.0,
        )
        .expect("bake");
        let c = sea.surface_on(deep_x, 3.0, &clock, Some(&abyss)).expect("sea");
        assert_eq!(a.height, c.height);
        assert_eq!(a.normal, c.normal);
        // Near the shoreline the swell is capped by the depth.
        let mut last: Option<f32> = None;
        for i in 0..200 {
            let x = 100.05 + i as f32 * 0.05;
            let Some(s) = sea.surface_on(x, 0.0, &clock, Some(&field)) else {
                continue;
            };
            let h = s.depth.expect("depth");
            assert!(s.height.abs() <= 0.5 * crate::water_body::shore::BREAKING_INDEX * h * 6.0 + 1.0e-3);
            if let Some(prev) = last {
                assert!(
                    (s.height - prev).abs() < 0.05,
                    "jump at {x}: {prev} -> {}",
                    s.height
                );
            }
            last = Some(s.height);
            assert!(s.shore_foam > 0.0 || s.shore_distance.is_some_and(|d| d > sea.look.wet_width));
        }
        assert!(last.is_some());
    }

    /// Spec 87 § 4: two wet surfaces at the same height go to the lower entity, whatever order
    /// the bodies come in; a ray that crosses both at the same distance does too. "Lower" is
    /// `Entity`'s order, which is the order of `to_bits()` (the id agents see), not of the index:
    /// bevy 0.20 stores the index inverted, so here entity 7 is the lower one.
    #[test]
    fn an_equal_height_tie_goes_to_the_lower_entity() {
        let clock = WaterClock::default();
        let a = WaterBody::plane([0.0, 1.5, 0.0], [20.0, 20.0], WaterLook::lake());
        let b = WaterBody::plane([5.0, 1.5, 5.0], [20.0, 20.0], WaterLook::lake());
        let forward = [
            BodyRef {
                entity: e(7),
                body: &a,
                shore: None,
            },
            BodyRef {
                entity: e(3),
                body: &b,
                shore: None,
            },
        ];
        let backward = [forward[1], forward[0]];
        let lower = e(3).min(e(7));
        assert!(
            lower.to_bits() < e(3).max(e(7)).to_bits(),
            "the order is the bits' order"
        );
        for refs in [forward, backward] {
            let (entity, s) = surface_at(refs, 2.0, 2.0, &clock, false).expect("both are wet here");
            assert_eq!(entity, lower, "the lower entity wins the tie");
            assert!((s.height - 1.5).abs() < 1e-4, "{}", s.height);
            let (entity, hit) =
                raycast_all(refs, [2.0, 10.0, 2.0], [0.0, -1.0, 0.0], 100.0, &clock, false).expect("hit");
            assert_eq!(entity, lower, "the lower entity wins an equal-distance ray tie");
            assert!((hit.point[1] - 1.5).abs() < 1e-4, "{:?}", hit.point);
        }
        // Not a tie: the higher surface wins even with the higher entity.
        let c = WaterBody::plane([0.0, 1.75, 0.0], [20.0, 20.0], WaterLook::lake());
        let refs = [
            BodyRef {
                entity: e(3),
                body: &b,
                shore: None,
            },
            BodyRef {
                entity: e(9),
                body: &c,
                shore: None,
            },
        ];
        assert_eq!(
            surface_at(refs, 2.0, 2.0, &clock, false).map(|(e, _)| e),
            Some(e(9))
        );
    }
}
