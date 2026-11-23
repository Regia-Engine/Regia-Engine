// SPDX-License-Identifier: MPL-2.0
//! The ECS bridge of the water bodies.
//!
//! ```text
//! FixedUpdate: advance_water_clock (one tick per fixed step)
//! Update:      ensure_water_renderer (once a RenderDevice exists);
//!              sync_terrain_water (one runtime body per terrain water plane, ADR-1620)
//! Render:      extract_frame → sync_water (bodies, clock, tier, sky; layer installed) → render
//! ```
//!
//! # Terrain water planes in the runtime frame (ADR-1620)
//!
//! A [`crate::Terrain`] with a [`crate::WaterPlane`] (ADR-1237) gets a runtime [`WaterBody`]: a
//! plane over the whole map at the plane's level, with a look derived from the plane's colors and
//! ripples ([`terrain_plane_body`]). It lives on its own entity tagged [`TerrainWater`], so the
//! agent and gameplay see it like any body (`water.state`, `water.surface`, `water.set` for its
//! look or swell). When the terrain's plane changes, the body's shape and look follow and its
//! swell, obstacles and ice stay; when the plane goes, so does the body.

use regia_app::prelude::*;
use regia_ecs::ecs::system::SystemParam;
use regia_render::{ExtractedFrame, RenderDevice, RenderSettings, RuntimeRenderer};
use serde::Serialize;

use std::sync::Arc;

use super::gpu::{WaterDebug, WaterRenderer};
use super::shore::{ShoreField, WaterShore};
use super::{WaterBody, WaterClock, WaterLook, WaterTier, tier_for_preset};
use crate::{Terrain, WaterPlane};

/// Host and agent switches for the water.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WaterSettings {
    /// `false` draws no water (the bodies stay).
    pub enabled: bool,
    /// Forced tier; `None` follows the render preset ([`tier_for_preset`]; a customised profile
    /// gets High when it keeps screen-space reflections on, Low otherwise).
    pub tier: Option<WaterTier>,
    /// Debug view (agent `water.set { debug }`).
    pub debug: WaterDebug,
    /// Each terrain's shoreline water plane gets a runtime body ([`TerrainWater`]); `false`
    /// removes those bodies (agent `water.set { terrain_planes }`).
    pub terrain_planes: bool,
    /// Shore fields ([`super::WaterShore`], spec 87) shoal the swell, add shore and breaker foam
    /// and make dry land answer no water; `false` ignores them everywhere and keeps the bakes
    /// (agent `water.set { shore }`).
    pub shore: bool,
    /// Draw the FFT ocean on High and Ultra ([`super::FftOcean`], ADR-1969); `false` draws the
    /// swell alone. Queries keep answering the ocean, so gameplay does not depend on it (agent
    /// `water.set { ocean_draw }`).
    pub ocean_draw: bool,
    /// The eye under a plane body's surface sees the water column fog the scene and the surface
    /// from below (ADR-2342; off by default); `false` draws nothing from below, as before (agent
    /// `water.set { underwater }`).
    pub underwater: bool,
    /// Time the water draw on the GPU every this many frames (0 = never; agent
    /// `water.set { gpu_timing }`). Each timing is one extra draw into a private target and waits
    /// for the GPU: a diagnostic, not a shipped setting.
    pub gpu_timing: u32,
    /// The ring grid is drawn as index ranges and a range outside the camera's frustum is left
    /// out (ADR-2342; off by default; agent `water.set { ring_culling }`). The picture is the same
    /// either way.
    pub ring_culling: bool,
    /// Each ring of the swell grid snaps on its own lattice, so the far rings move rarely and by
    /// whole steps (ADR-2342; off by default: it moves the far vertices; agent
    /// `water.set { ring_snap }`).
    pub ring_snap: bool,
    /// Surface and ray queries answer the surface the tier draws rather than the full one
    /// (ADR-2843; off by default): on Low, or with `ocean_draw` off, an ocean plane answers its
    /// swell or the ocean's stand-in, and a swell answers only the waves the tier draws, so a
    /// floater rides the drawn water. Off, gameplay is the same on every tier and machine, which
    /// is what a networked game needs (agent `water.set { queries_follow_tier }`).
    pub queries_follow_tier: bool,
}

impl Default for WaterSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            tier: None,
            debug: WaterDebug::Off,
            terrain_planes: true,
            shore: true,
            ocean_draw: true,
            underwater: false,
            gpu_timing: 0,
            ring_culling: false,
            ring_snap: false,
            queries_follow_tier: false,
        }
    }
}

impl WaterSettings {
    /// The tier and the `ocean_draw` switch queries answer for: `None` unless
    /// [`Self::queries_follow_tier`] is on (ADR-2843).
    #[must_use]
    pub fn query_tier(&self, render: Option<&RenderSettings>) -> Option<(WaterTier, bool)> {
        self.queries_follow_tier
            .then(|| (self.effective_tier(render), self.ocean_draw))
    }

    /// The tier in effect under `render` (or High without render settings).
    #[must_use]
    pub fn effective_tier(&self, render: Option<&RenderSettings>) -> WaterTier {
        if let Some(tier) = self.tier {
            return tier;
        }
        match render {
            Some(settings) => match settings.preset() {
                Some(preset) => tier_for_preset(preset),
                None if settings.profile.ssr && settings.profile.screen_space_refraction => WaterTier::High,
                None => WaterTier::Low,
            },
            None => WaterTier::High,
        }
    }
}

/// The water's GPU renderer, inserted once the render device exists.
#[derive(Resource, Clone, Debug)]
pub struct WaterState {
    /// The renderer whose layer the runtime frame draws.
    pub renderer: WaterRenderer,
    /// The tier the last frame used.
    pub tier: WaterTier,
}

/// Adds the water bodies to an app with `regia_render`'s runtime path.
///
/// Spawn entities with a [`WaterBody`]; the plugin draws every one of them (up to
/// [`super::MAX_BODIES`], in entity order) from the first frame after the render device exists.
#[derive(Default)]
pub struct WaterPlugin;

impl Plugin for WaterPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WaterSettings>()
            .init_resource::<WaterClock>()
            .init_resource::<super::SeaWind>()
            .init_resource::<super::WaterDrawnOceans>()
            .add_systems(FixedUpdate, advance_water_clock)
            // Queries that follow the drawn surface read a cached drawn body (ADR-3251).
            .add_systems(PreUpdate, super::cache_drawn_water)
            .add_systems(
                Update,
                (
                    ensure_water_renderer,
                    sync_terrain_water,
                    super::apply_weather_sea.after(sync_terrain_water),
                ),
            )
            .add_systems(
                Render,
                sync_water
                    .after(regia_render::extract_frame)
                    .before(regia_render::render_texture_views),
            );
    }
}

/// One tick per fixed step, at the fixed step's length.
pub fn advance_water_clock(mut clock: ResMut<WaterClock>, fixed: Option<Res<FixedTime>>) {
    if let Some(fixed) = fixed {
        let step = fixed.step().as_secs_f64();
        if step.is_finite() && step > 0.0 {
            clock.step_seconds = step;
        }
    }
    if clock.running {
        clock.tick = clock.tick.wrapping_add(1);
    }
}

/// Marks the runtime [`WaterBody`] made from a terrain's shoreline plane ([`sync_terrain_water`]).
#[derive(Component, Debug, Clone, PartialEq)]
pub struct TerrainWater {
    /// The terrain entity whose plane this body draws.
    pub terrain: Entity,
    /// The plane the body was last built from.
    pub source: WaterPlane,
    /// The map width the body was last built for (m).
    pub world_width: f32,
}

/// The runtime body of a terrain's shoreline plane: a level rectangle over the whole map (the map
/// is centered on the origin, [`crate::TerrainMap::world_width`] on a side) at the plane's level,
/// with the look [`look_from_plane`] gives.
#[must_use]
pub fn terrain_plane_body(plane: &WaterPlane, world_width: f32) -> WaterBody {
    let half = if world_width.is_finite() {
        (0.5 * world_width).max(0.5)
    } else {
        0.5
    };
    WaterBody::plane([0.0, plane.level, 0.0], [half, half], look_from_plane(plane))
}

/// A runtime [`WaterLook`] for a shoreline plane, from the open-sea preset:
///
/// - extinction `σ = 6 / max_depth` (the optical depth ADR-1237 reaches at `max_depth`), split
///   half into scattering and, per channel, absorption `σ · (1.2 − shallow)` (the channels the
///   shallow color lacks are absorbed first);
/// - in-scatter albedo `deep_color` (deep water shows its own color);
/// - the wet band is `foam_width` wide;
/// - ripple tile `8 · 2π / wave_frequency` (the finest ripple is about an eighth of the tile)
///   and ripple strength in proportion to the plane's ripple slope (`amplitude · frequency`, 1 at
///   the default plane's 0.045).
///
/// `alpha` and `foam_color` have no runtime counterpart (the runtime body's opacity follows from
/// the depth through Beer–Lambert). Every value is clamped into [`WaterLook::validate`]'s ranges.
#[must_use]
pub fn look_from_plane(plane: &WaterPlane) -> WaterLook {
    let sigma = (6.0 / plane.max_depth.max(0.06)).min(100.0);
    let unit = |c: f32| if c.is_finite() { c.clamp(0.0, 1.0) } else { 0.0 };
    let ocean = WaterLook::ocean();
    let tile = std::f32::consts::TAU * 8.0 / plane.wave_frequency.max(1.0e-3);
    let slope = plane.wave_amplitude * plane.wave_frequency;
    WaterLook {
        absorption: plane
            .shallow_color
            .map(|c| (sigma * (1.2 - unit(c))).clamp(0.0, 100.0)),
        scattering: (0.5 * sigma).clamp(0.0, 100.0),
        scatter_color: plane.deep_color.map(unit),
        ripple_scale: if tile.is_finite() {
            tile.clamp(0.1, 1000.0)
        } else {
            ocean.ripple_scale
        },
        normal_strength: if slope.is_finite() {
            (slope / 0.045).clamp(0.0, 8.0)
        } else {
            ocean.normal_strength
        },
        wet_width: if plane.foam_width.is_finite() {
            plane.foam_width.clamp(0.0, 50.0)
        } else {
            ocean.wet_width
        },
        ..ocean
    }
}

/// Frame counter and terrains whose bed changed since the last deformation rebake.
///
/// [`sync_terrain_water`] keeps one as system-local state. Hosts do not construct it.
#[derive(Default)]
pub struct ShoreBakeGate {
    frame: u32,
    /// Terrains that still need a shore rebake after a height edit.
    dirty: Vec<Entity>,
}

/// Keeps one runtime body per terrain water plane (see the module docs); removes them all when
/// [`WaterSettings::terrain_planes`] is off.
pub fn sync_terrain_water(
    mut commands: Commands,
    settings: Res<WaterSettings>,
    terrains: Query<(Entity, Ref<Terrain>)>,
    mut derived: Query<(Entity, &mut TerrainWater, &mut WaterBody, Option<&WaterShore>)>,
    mut gate: Local<ShoreBakeGate>,
) {
    gate.frame = gate.frame.wrapping_add(1);
    let mut covered: Vec<Entity> = Vec::new();
    for (entity, mut tag, mut body, shore) in &mut derived {
        let found = terrains.get(tag.terrain).ok();
        let plane = found
            .as_ref()
            .and_then(|(_, t)| t.water.as_ref().map(|w| (w, t.map.world_width())));
        let Some((plane, width)) = plane.filter(|_| settings.terrain_planes) else {
            commands.entity(entity).despawn();
            continue;
        };
        if covered.contains(&tag.terrain) {
            // A second body for the same terrain (should not happen): keep the first.
            commands.entity(entity).despawn();
            continue;
        }
        covered.push(tag.terrain);
        let reshaped = tag.source != *plane || tag.world_width != width;
        if reshaped {
            let fresh = terrain_plane_body(plane, width);
            body.shape = fresh.shape;
            body.look = fresh.look;
            tag.source = plane.clone();
            tag.world_width = width;
        }
        // The shore bake (spec 87 § 2.2): on a new or changed plane, a missing field, or a
        // bed marked dirty since the last deformation rebake. A height edit is remembered
        // and baked on the next multiple of SHORE_REBAKE_FRAMES, from the terrain as it is
        // on that frame, so a sculpt that does not land on the multiple still lands.
        let changed = found.as_ref().is_some_and(|(_, t)| t.is_changed());
        if changed && !gate.dirty.contains(&tag.terrain) {
            gate.dirty.push(tag.terrain);
        }
        let deformed = gate.frame.is_multiple_of(SHORE_REBAKE_FRAMES) && gate.dirty.contains(&tag.terrain);
        let stale = shore.is_none_or(|s| s.field.level != plane.level);
        if settings.shore
            && (reshaped || stale || deformed)
            && let Some((_, terrain)) = &found
            && let Some(field) = bake_terrain_shore(&terrain.map, plane.level, width)
        {
            commands.entity(entity).insert(WaterShore {
                field: Arc::new(field),
            });
            gate.dirty.retain(|id| *id != tag.terrain);
        }
    }
    gate.dirty.retain(|id| covered.contains(id));
    if !settings.terrain_planes {
        return;
    }
    for (entity, terrain) in &terrains {
        if covered.contains(&entity) {
            continue;
        }
        if let Some(plane) = &terrain.water {
            let width = terrain.map.world_width();
            let mut spawned = commands.spawn((
                terrain_plane_body(plane, width),
                TerrainWater {
                    terrain: entity,
                    source: plane.clone(),
                    world_width: width,
                },
            ));
            if settings.shore
                && let Some(field) = bake_terrain_shore(&terrain.map, plane.level, width)
            {
                spawned.insert(WaterShore {
                    field: Arc::new(field),
                });
            }
        }
    }
}

/// Update frames between two re-bakes of a deformed terrain's shore (spec 87 § 2.2).
pub const SHORE_REBAKE_FRAMES: u32 = 32;

/// The shore field of a terrain's water plane over the whole map (`None`, with a warning, when the
/// map cannot be baked).
fn bake_terrain_shore(map: &crate::TerrainMap, level: f32, width: f32) -> Option<ShoreField> {
    let half = 0.5 * width;
    match ShoreField::bake_terrain(map, level, [-half, -half], [half, half]) {
        Ok(field) => Some(field),
        Err(error) => {
            tracing::warn!(target: "regia_terrain", %error, "terrain water: shore bake failed");
            None
        }
    }
}

fn ensure_water_renderer(world: &mut World) {
    if world.contains_resource::<WaterState>() {
        return;
    }
    let Some(device) = world.get_resource::<RenderDevice>().cloned() else {
        return;
    };
    let renderer = WaterRenderer::new(&device);
    world.insert_resource(WaterState {
        renderer,
        tier: WaterTier::High,
    });
    tracing::info!(target: "regia_terrain", "water renderer created");
}

/// The entities the renderer was last given, its per-body ocean answer and the fallback list being
/// built (kept between frames so the report allocates nothing per frame).
#[derive(Default)]
pub struct DrawnScratch {
    last: Vec<Entity>,
    flags: Vec<bool>,
    fallback: Vec<Entity>,
}

/// What [`sync_water`] needs to publish the renderer's ocean fallbacks (ADR-3251).
#[derive(SystemParam)]
pub struct DrawnOceansReport<'w, 's> {
    drawn: Option<ResMut<'w, super::WaterDrawnOceans>>,
    scratch: Local<'s, DrawnScratch>,
}

/// Hands this frame's bodies, clock, tier and ambient light to the renderer and keeps its layer
/// installed first among the frame renderer's translucent layers (under the effects).
pub fn sync_water(
    state: Option<ResMut<WaterState>>,
    settings: Res<WaterSettings>,
    clock: Res<WaterClock>,
    bodies: Query<(Entity, &WaterBody, Option<&WaterShore>)>,
    render: Option<Res<RenderSettings>>,
    (frame, origin): (
        Option<Res<ExtractedFrame>>,
        Option<Res<regia_scene::streaming::OriginOffset>>,
    ),
    (renderer, mut report): (Option<ResMut<RuntimeRenderer>>, DrawnOceansReport),
) {
    let Some(mut state) = state else { return };
    let _span = tracing::info_span!("water_sync").entered();
    let mut list: Vec<(Entity, &WaterBody, Option<&WaterShore>)> = bodies.iter().collect();
    list.sort_by_key(|(entity, _, _)| *entity);
    // The oceans the last frame left to their swell (ADR-3251), by the entities it was given.
    let DrawnOceansReport { drawn, scratch } = &mut report;
    if let Some(drawn) = drawn.as_deref_mut() {
        let DrawnScratch {
            last,
            flags,
            fallback,
        } = &mut **scratch;
        state.renderer.oceans_drawn_into(flags);
        fallback.clear();
        fallback.extend(last.iter().zip(flags.iter()).filter_map(|(entity, drew)| {
            let ocean = bodies
                .get(*entity)
                .is_ok_and(|(_, body, _)| body.active_ocean().is_some());
            (ocean && !*drew).then_some(*entity)
        }));
        fallback.sort_unstable();
        if drawn.swell_fallback != *fallback {
            drawn.swell_fallback.clone_from(fallback);
        }
        last.clear();
        last.extend(list.iter().map(|(entity, _, _)| *entity));
    }
    let refs: Vec<&WaterBody> = list.iter().map(|(_, body, _)| *body).collect();
    let shores: Vec<Option<Arc<ShoreField>>> = list
        .iter()
        .map(|(_, _, shore)| shore.filter(|_| settings.shore).map(|s| Arc::clone(&s.field)))
        .collect();
    let tier = settings.effective_tier(render.as_deref());
    state.tier = tier;
    let water = &state.renderer;
    water.set_origin_offset(origin.map_or([0.0; 3], |o| o.0.map(|axis| axis as f32)));
    water.set_bodies(&refs);
    water.set_shores(&shores);
    water.set_clock(*clock);
    water.set_tier(tier);
    water.set_enabled(settings.enabled);
    water.set_ocean_draw(settings.ocean_draw);
    water.set_debug(settings.debug);
    water.set_underwater(settings.underwater);
    water.set_gpu_timing(settings.gpu_timing);
    water.set_ring_culling(settings.ring_culling);
    water.set_ring_snap(settings.ring_snap);
    if let Some(frame) = frame.as_deref() {
        water.set_ambient_lux(frame.environment.ambient_lux);
    }
    if let Some(mut renderer) = renderer {
        // A camera cut restarts the ocean's lingering foam (ADR-2843).
        water.set_camera_cuts(renderer.0.camera_cut_count());
        if !renderer
            .0
            .translucent_layers()
            .iter()
            .any(|layer| water.owns_layer(layer))
        {
            renderer.0.insert_translucent_layer(0, water.layer());
        }
        // The compute pass joins the graph with the first ocean: a frame without one stays free
        // of extensions. Asking the renderer, like the layer above, also covers a new renderer.
        if refs.iter().any(|body| body.active_ocean().is_some())
            && !renderer
                .0
                .registered_passes()
                .iter()
                .any(|name| name == super::OCEAN_PASS_NAME)
            && let Err(error) = renderer.0.register_pass(water.ocean_pass())
        {
            tracing::warn!(target: "regia_terrain", %error, "water ocean pass not registered");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::RenderProfilePreset;

    #[test]
    fn tier_follows_the_preset_unless_forced() {
        let settings = WaterSettings::default();
        let deck = RenderSettings::from_profile(RenderProfilePreset::Deck);
        assert_eq!(settings.effective_tier(Some(&deck)), WaterTier::Low);
        let ultra = RenderSettings::from_profile(RenderProfilePreset::DesktopUltra);
        assert_eq!(settings.effective_tier(Some(&ultra)), WaterTier::Ultra);
        assert_eq!(settings.effective_tier(None), WaterTier::High);
        let forced = WaterSettings {
            tier: Some(WaterTier::Low),
            ..settings
        };
        assert_eq!(forced.effective_tier(Some(&ultra)), WaterTier::Low);
    }

    /// Mobile, the Deck and Desktop Low draw no ocean; Desktop High and Ultra do (ADR-1969).
    #[test]
    fn only_high_and_ultra_presets_draw_the_ocean() {
        let settings = WaterSettings::default();
        let size = |preset| {
            settings
                .effective_tier(Some(&RenderSettings::from_profile(preset)))
                .ocean_size()
        };
        for preset in [
            RenderProfilePreset::MobileLow,
            RenderProfilePreset::MobileHigh,
            RenderProfilePreset::DesktopLow,
            RenderProfilePreset::Deck,
        ] {
            assert_eq!(size(preset), 0, "{preset:?}");
        }
        assert_eq!(size(RenderProfilePreset::DesktopHigh), 128);
        assert_eq!(size(RenderProfilePreset::DesktopUltra), 256);
    }

    /// The runtime path adds the compute pass with the first ocean, once, and a world without an
    /// ocean adds none (ADR-1969). The runtime renderer carries its own passes from the start
    /// (the lens effects pass, ADR-1863, records nothing unless a view asks), so the ocean is
    /// measured against those.
    #[test]
    fn the_ocean_pass_joins_the_graph_with_the_first_ocean() {
        // The process's one test device (ADR-7557): a device of its own would be dropped at the
        // end of this test while other test threads name objects (the loader race of ADR-2113).
        let Ok(device) = RenderDevice::shared_headless() else {
            eprintln!("skipping: no GPU adapter (lavapipe) available");
            return;
        };
        let mut app = App::new();
        app.init_resource::<WaterSettings>()
            .init_resource::<WaterClock>()
            .insert_resource(WaterState {
                renderer: WaterRenderer::new(&device),
                tier: WaterTier::High,
            })
            .insert_resource(RuntimeRenderer::default())
            .add_systems(Update, sync_water);
        let passes = |app: &App| -> Vec<String> {
            let report = app.world().resource::<RuntimeRenderer>().0.graph_report();
            report.extensions.iter().map(|p| p.name.clone()).collect()
        };
        let own = passes(&app);
        assert!(
            !own.iter().any(|name| name == super::super::OCEAN_PASS_NAME),
            "{own:?}"
        );
        let body = WaterBody::plane([0.0; 3], [100.0, 100.0], WaterLook::ocean());
        let plain = app.world_mut().spawn(body.clone()).id();
        app.update();
        assert_eq!(passes(&app), own, "a plane without an ocean adds no pass");
        let mut sea = body;
        sea.ocean = Some(super::super::FftOcean::default());
        app.world_mut().entity_mut(plain).insert(sea);
        app.update();
        app.update();
        let mut want = own;
        want.push(super::super::OCEAN_PASS_NAME.to_string());
        assert_eq!(passes(&app), want, "the ocean pass joins once");
    }

    fn terrain(water: Option<WaterPlane>) -> Terrain {
        use crate::{CdlodConfig, TerrainLayers, TerrainMap};
        let n = 33u32;
        let heights = (0..n * n).map(|i| (i % n) as f32 * 0.5).collect();
        let map = TerrainMap::from_heights(n, n, heights, 128.0, 100.0, 0.0).expect("map");
        let mut t = Terrain::new(
            &map.to_heightfield(),
            128.0,
            100.0,
            0.0,
            CdlodConfig::default(),
            3,
            TerrainLayers::default(),
        )
        .expect("terrain");
        t.water = water;
        t
    }

    fn terrain_bodies(app: &mut App) -> Vec<(Entity, WaterBody, TerrainWater)> {
        let mut q = app.world_mut().query::<(Entity, &WaterBody, &TerrainWater)>();
        q.iter(app.world())
            .map(|(e, b, t)| (e, b.clone(), t.clone()))
            .collect()
    }

    /// DO-5: a terrain's shoreline plane becomes a runtime water body over the whole map at its
    /// level; it follows the plane's edits, keeps an added swell, and goes with the plane or the
    /// switch.
    #[test]
    fn terrain_water_plane_becomes_a_runtime_body() {
        let mut app = App::new();
        app.add_plugins(WaterPlugin);
        let plane = WaterPlane {
            level: 12.5,
            ..WaterPlane::default()
        };
        let dry = app.world_mut().spawn(terrain(None)).id();
        let wet = app.world_mut().spawn(terrain(Some(plane.clone()))).id();
        app.update();
        app.update();
        let found = terrain_bodies(&mut app);
        assert_eq!(found.len(), 1, "one body, for the terrain with water");
        let (body_entity, body, tag) = &found[0];
        assert_eq!(tag.terrain, wet);
        assert_ne!(tag.terrain, dry);
        assert!(body.validate().is_ok(), "{:?}", body.validate());
        assert_eq!(
            body.shape,
            crate::water_body::WaterShape::Plane {
                center: [0.0, 12.5, 0.0],
                half_extent: [64.0, 64.0],
                flow: [0.0, 0.0],
            }
        );
        assert!(body.probe(63.0, -63.0).is_some() && body.probe(65.0, 0.0).is_none());
        // A swell added to the runtime body survives a level change; the level follows.
        let swell = crate::water_body::Swell::open_sea([1.0, 0.0], 0.5, 12.0, 4);
        app.world_mut()
            .get_mut::<WaterBody>(*body_entity)
            .expect("body")
            .swell = Some(swell.clone());
        app.world_mut()
            .get_mut::<Terrain>(wet)
            .expect("terrain")
            .water
            .as_mut()
            .expect("plane")
            .level = 13.0;
        app.update();
        let found = terrain_bodies(&mut app);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, *body_entity, "the same entity is updated");
        let crate::water_body::WaterShape::Plane { center, .. } = found[0].1.shape else {
            panic!("a plane");
        };
        assert_eq!(center[1], 13.0);
        assert_eq!(found[0].1.swell.as_ref(), Some(&swell));
        // The switch removes it and brings it back.
        app.world_mut().resource_mut::<WaterSettings>().terrain_planes = false;
        app.update();
        app.update();
        assert!(terrain_bodies(&mut app).is_empty());
        app.world_mut().resource_mut::<WaterSettings>().terrain_planes = true;
        app.update();
        app.update();
        assert_eq!(terrain_bodies(&mut app).len(), 1);
        // No plane, no body.
        app.world_mut().get_mut::<Terrain>(wet).expect("terrain").water = None;
        app.update();
        app.update();
        assert!(terrain_bodies(&mut app).is_empty());
    }

    fn shore_depth(app: &mut App, x: f32, z: f32) -> f32 {
        let mut q = app.world_mut().query::<&WaterShore>();
        let shore = q.iter(app.world()).next().expect("a shore field");
        shore.field.sample(x, z).expect("inside the field").depth
    }

    fn raise_center(app: &mut App, terrain: Entity, height: f32) {
        let mut t = app.world_mut().get_mut::<Terrain>(terrain).expect("terrain");
        let w = t.map.width() as usize;
        let index = (t.map.height() as usize / 2) * w + w / 2;
        t.map.set_height(index, height);
    }

    /// Spec 87 § 8 (world query): a terrain's map-wide plane over its own hill, with the field
    /// the plugin baked, answers nothing on the hill and the level offshore; with the switch off
    /// the hill answers the level again.
    #[test]
    fn a_terrain_plane_over_its_hill_is_dry_with_the_field_and_wet_without() {
        use regia_ecs::ecs::system::SystemState;
        let mut app = App::new();
        app.add_plugins(WaterPlugin);
        let level = 12.5;
        let wet = app
            .world_mut()
            .spawn(terrain(Some(WaterPlane {
                level,
                ..WaterPlane::default()
            })))
            .id();
        app.update();
        app.update();
        let (hill, sea) = {
            let map = &app.world().get::<Terrain>(wet).expect("terrain").map;
            (map.sample_bilinear(60.0, 0.0), map.sample_bilinear(-60.0, 0.0))
        };
        assert!(
            hill > level + 1.0 && sea < level - 1.0,
            "the bed rises along x: {sea} .. {hill}"
        );
        let surface = |app: &mut App, x: f32| {
            let mut state: SystemState<crate::water_body::query::WaterQuery> =
                SystemState::new(app.world_mut());
            state
                .get(app.world())
                .expect("query")
                .surface(x, 0.0)
                .map(|(_, s)| s)
        };
        assert!(
            surface(&mut app, 60.0).is_none(),
            "the hill under the plane is dry"
        );
        let offshore = surface(&mut app, -60.0).expect("offshore is water");
        assert!((offshore.height - level).abs() < 1.0e-3, "{offshore:?}");
        assert!(
            offshore.depth.is_some_and(|d| (d - (level - sea)).abs() < 0.5),
            "{offshore:?}"
        );
        app.world_mut().resource_mut::<WaterSettings>().shore = false;
        let flat = surface(&mut app, 60.0).expect("switch off: the plane answers everywhere");
        assert!((flat.height - level).abs() < 1.0e-3, "{flat:?}");
        assert!(flat.depth.is_none(), "no field, no depth: {flat:?}");
    }

    /// Spec 87 § 2.2: with `WaterSettings::shore` off the plugin bakes nothing, and a field baked
    /// while it was on is kept (not rebaked after a deformation) until the switch comes back.
    #[test]
    fn the_shore_switch_off_bakes_nothing_and_keeps_existing_fields() {
        let mut app = App::new();
        app.add_plugins(WaterPlugin);
        app.world_mut().resource_mut::<WaterSettings>().shore = false;
        let plane = WaterPlane {
            level: 12.5,
            ..WaterPlane::default()
        };
        let wet = app.world_mut().spawn(terrain(Some(plane))).id();
        app.update();
        app.update();
        assert_eq!(terrain_bodies(&mut app).len(), 1, "the plane body exists");
        let fields = |app: &mut App| app.world_mut().query::<&WaterShore>().iter(app.world()).count();
        assert_eq!(fields(&mut app), 0, "switch off: no bake");
        app.world_mut().resource_mut::<WaterSettings>().shore = true;
        app.update();
        assert_eq!(fields(&mut app), 1, "switch on: the missing field bakes");
        let before = shore_depth(&mut app, 0.0, 0.0);
        app.world_mut().resource_mut::<WaterSettings>().shore = false;
        raise_center(&mut app, wet, 40.0);
        for _ in 0..(2 * SHORE_REBAKE_FRAMES) {
            app.update();
        }
        assert_eq!(fields(&mut app), 1, "switch off keeps the baked field");
        assert_eq!(
            shore_depth(&mut app, 0.0, 0.0),
            before,
            "and does not rebake it after a deformation"
        );
        app.world_mut().resource_mut::<WaterSettings>().shore = true;
        for _ in 0..SHORE_REBAKE_FRAMES {
            app.update();
        }
        assert_ne!(
            shore_depth(&mut app, 0.0, 0.0),
            before,
            "back on: the remembered edit bakes on the next multiple"
        );
    }

    /// A height edit that misses a multiple of [`SHORE_REBAKE_FRAMES`] is remembered and baked
    /// on the next multiple, from the terrain as it is then. A drag that continues past a
    /// multiple ends on the last heights, not the heights at that multiple.
    #[test]
    fn a_deformed_shore_rebakes_on_the_next_multiple_not_only_that_frame() {
        let mut app = App::new();
        app.add_plugins(WaterPlugin);
        let plane = WaterPlane {
            level: 12.5,
            ..WaterPlane::default()
        };
        let wet = app.world_mut().spawn(terrain(Some(plane))).id();
        app.update();
        app.update();
        let before = shore_depth(&mut app, 0.0, 0.0);
        raise_center(&mut app, wet, 40.0);
        app.update();
        assert_eq!(
            shore_depth(&mut app, 0.0, 0.0),
            before,
            "an edit off the 32-frame multiple does not rebake on that frame"
        );
        // Frames so far: 3. Stay dirty, with no further edit, through frame 31.
        for _ in 0..28 {
            app.update();
        }
        assert_eq!(shore_depth(&mut app, 0.0, 0.0), before);
        app.update();
        assert_ne!(
            shore_depth(&mut app, 0.0, 0.0),
            before,
            "the remembered edit bakes on the next multiple"
        );

        // A drag from frame 33 through 70 crosses the multiple at 64 and keeps going.
        // The shore after frame 70 is still the frame-64 bed; frame 96 bakes the final bed.
        for height in 33..=70 {
            raise_center(&mut app, wet, height as f32);
            app.update();
        }
        let at_seventy = shore_depth(&mut app, 0.0, 0.0);
        for _ in 0..25 {
            app.update();
        }
        assert_eq!(shore_depth(&mut app, 0.0, 0.0), at_seventy);
        app.update();
        let settled = shore_depth(&mut app, 0.0, 0.0);
        assert_ne!(
            settled, at_seventy,
            "the drag's final heights bake on the next multiple"
        );
        let expected = {
            let map = &app.world().get::<Terrain>(wet).expect("terrain").map;
            let half = 0.5 * map.world_width();
            super::ShoreField::bake_terrain(map, 12.5, [-half, -half], [half, half])
                .expect("bake")
                .sample(0.0, 0.0)
                .expect("sample")
                .depth
        };
        assert_eq!(settled, expected);
    }

    /// The runtime look follows the plane: deeper `max_depth` means clearer water, ripples scale
    /// with the plane's, and the look always validates.
    #[test]
    fn look_from_plane_is_valid_and_monotonic() {
        let base = WaterPlane::default();
        let look = look_from_plane(&base);
        assert!(look.validate().is_ok(), "{:?}", look.validate());
        assert!((look.normal_strength - 1.0).abs() < 1.0e-5);
        let deep = look_from_plane(&WaterPlane {
            max_depth: 40.0,
            ..base.clone()
        });
        assert!(deep.scattering < look.scattering);
        assert!(deep.absorption.iter().zip(look.absorption).all(|(d, l)| *d < l));
        let wild = look_from_plane(&WaterPlane {
            max_depth: 0.0,
            wave_frequency: 0.0,
            wave_amplitude: 1.0e9,
            foam_width: f32::NAN,
            ..base
        });
        assert!(wild.validate().is_ok(), "{:?}", wild.validate());
    }

    /// `docs/specs/water.md` § 2 by value: extinction `6 / max_depth`, half of it scattering,
    /// absorption `σ (1.2 − shallow_color)` per channel, in-scatter albedo `deep_color`, wet band
    /// `foam_width`, ripple tile `8 · 2π / wave_frequency`, ripple strength in proportion to
    /// `wave_amplitude · wave_frequency`.
    #[test]
    fn look_from_plane_follows_the_spec_formulas() {
        let plane = WaterPlane {
            max_depth: 12.0,
            shallow_color: [0.2, 0.5, 0.7],
            deep_color: [0.1, 0.2, 0.3],
            foam_width: 2.0,
            wave_frequency: 0.5,
            wave_amplitude: 0.03,
            ..WaterPlane::default()
        };
        let look = look_from_plane(&plane);
        let close = |a: f32, b: f32| (a - b).abs() < 1.0e-5;
        let sigma = 6.0 / 12.0;
        for (got, shallow) in look.absorption.iter().zip(plane.shallow_color) {
            assert!(close(*got, sigma * (1.2 - shallow)), "{:?}", look.absorption);
        }
        assert!(close(look.scattering, 0.5 * sigma));
        assert_eq!(look.scatter_color, plane.deep_color);
        assert_eq!(look.wet_width, 2.0);
        assert!(close(look.ripple_scale, 8.0 * std::f32::consts::TAU / 0.5));
        // Twice the slope, twice the strength.
        let steeper = look_from_plane(&WaterPlane {
            wave_amplitude: 0.06,
            ..plane.clone()
        });
        assert!(close(steeper.normal_strength, 2.0 * look.normal_strength));
        assert!(look.normal_strength > 0.0);
    }

    /// The clock advances once per fixed step, by the fixed step's length; a stopped clock holds.
    #[test]
    fn clock_ticks_once_per_fixed_step() {
        let mut app = App::new();
        regia_app::enable_fixed_step(&mut app, 30.0);
        app.add_plugins(WaterPlugin);
        for _ in 0..5 {
            app.update();
        }
        let clock = *app.world().resource::<WaterClock>();
        assert_eq!(clock.tick, 5, "{clock:?}");
        assert!((clock.step_seconds - 1.0 / 30.0).abs() < 1.0e-9);
        app.world_mut().resource_mut::<WaterClock>().running = false;
        app.update();
        assert_eq!(app.world().resource::<WaterClock>().tick, 5);
    }
}
