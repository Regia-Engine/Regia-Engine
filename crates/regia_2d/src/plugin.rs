// SPDX-License-Identifier: MPL-2.0
//! [`Sprite2dPlugin`]: animation, asset resolution (with hot reload), frame extraction and the
//! sprite overlay pass.
//!
//! | Schedule | System | Role |
//! |---|---|---|
//! | `Update` | [`crate::animation::animate_sprites`] | Advance sprite-sheet animations |
//! | `PostUpdate` (after `TransformPropagate`) | [`load_2d_assets`] | Load every atlas/tilemap a component names; reload on a hot-reload generation change |
//! | `PostUpdate` (after `load_2d_assets`) | [`extract_2d_frame`] | Build and sort this frame's quads into the shared pass state |
//! | `Startup` | — | Install the sprite pass as a `WindowOverlay` layer (order [`SPRITE_OVERLAY_ORDER`], before the UI) |

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_asset::{AssetServer, decode_cooked_file};
use regia_core::id::TypePathHash;
use regia_core::math::{UVec2, Vec2};
use regia_render::{OverlayDraw, RenderDevice, WindowOverlay};
use regia_scene::GlobalTransform;

use crate::animation::{AnimatedSprite, Animation, animate_sprites};
use crate::atlas::{ATLAS_TYPE_PATH, SpriteAtlas};
use crate::batch::{Frame2d, PreparedFrame};
use crate::camera::Rect2;
use crate::render::{DrawSettings, SpriteRenderer};
use crate::sprite::{
    AssetOrigin, LoadedAtlas, LoadedTileMap, Sprite, SpriteAssets, SpriteSource, TileMap, View2d,
};
use crate::tilemap::{CookedTileMap, TILEMAP_TYPE_PATH};

/// Order of the sprite pass among `WindowOverlay` layers (the UI always draws after layers).
pub const SPRITE_OVERLAY_ORDER: i32 = -100;

/// Per-frame statistics of the 2D pass (agent surface).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frame2dStats {
    /// Sprites drawn.
    pub sprites: u32,
    /// Tiles drawn.
    pub tiles: u32,
    /// Instances after sorting.
    pub instances: u32,
    /// Texture batches (draw calls).
    pub batches: u32,
}

/// What the ECS hands the render thread each frame.
#[derive(Default)]
pub struct SpritePassState {
    /// The sorted quads.
    pub prepared: PreparedFrame,
    /// Camera and mode.
    pub settings: DrawSettings,
    /// The window size the next extraction culls tiles for: the per-axis maximum of every window
    /// that drew the current extracted frame, so with several windows the cull covers all of them.
    pub last_window: Option<UVec2>,
    /// The GPU state, created on the first draw.
    pub renderer: Option<SpriteRenderer>,
    /// Statistics of the last extracted frame.
    pub stats: Frame2dStats,
    /// The frame being filled. Taken out for the extract and put back, so the quads of one
    /// frame reuse the previous frame's item storage.
    scratch: Frame2d,
    /// The prepared buffer the next extract writes into. It is the frame published before last,
    /// cleared, so the renderer can still read `prepared` while this one is filled.
    recycle: PreparedFrame,
    /// The generation published by the next extract. `0` until the first extract; published
    /// values start at 1 and skip 0 so they never match an overlay that has not drawn yet.
    frame_seq: u64,
    /// How many times extraction has grown an instance buffer. The count lives on the pass,
    /// not a thread-local: the schedule may run the system on a worker thread.
    instance_grows: u64,
}

/// The shared pass state (ECS writes, the overlay pass reads).
#[derive(Resource, Clone, Default)]
pub struct SpritePass(pub Arc<Mutex<SpritePassState>>);

impl SpritePass {
    /// Statistics of the last extracted frame.
    #[must_use]
    pub fn stats(&self) -> Frame2dStats {
        lock_pass(&self.0).stats
    }

    /// The largest instance-buffer capacity kept for extraction.
    ///
    /// After the third extract of a steady scene this is the capacity of a reused buffer.
    /// The first two extracts each allocate one buffer (the published frame and the recycle
    /// buffer); later extracts leave it unchanged when the quad count fits.
    #[must_use]
    pub fn retained_instance_capacity(&self) -> usize {
        let state = lock_pass(&self.0);
        state
            .prepared
            .instances
            .capacity()
            .max(state.recycle.instances.capacity())
    }
}

/// Locks the pass state, recovering it from a poisoned lock: the state is plain per-frame data
/// that the next extraction overwrites, so a panic elsewhere while it was locked must not stop
/// 2D for the rest of the run (concurrency audit 11-03 H1).
fn lock_pass(state: &Mutex<SpritePassState>) -> std::sync::MutexGuard<'_, SpritePassState> {
    state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The `WindowOverlay` adapter drawing the shared pass state.
struct SpriteOverlay {
    state: Arc<Mutex<SpritePassState>>,
    /// The frame being drawn, copied out of `state` so the draw runs unlocked.
    frame: PreparedFrame,
    /// [`PreparedFrame::generation`] of the extracted frame drawn last, and the largest window
    /// that drew it. A reused instance buffer still gets a new generation, so the cull window
    /// resets instead of staying at the largest window ever seen.
    frame_id: u64,
    frame_window: Option<UVec2>,
}

/// The window the next extract should cull for.
///
/// A new `generation` starts the maximum over. Further windows of the same generation widen it
/// per axis. Generation `0` is the unpublished default and must not be used for a live frame:
/// the first published generation is 1, which does not match an overlay that still holds 0.
fn track_frame_window(
    frame_id: &mut u64,
    frame_window: &mut Option<UVec2>,
    generation: u64,
    size: UVec2,
) -> UVec2 {
    if generation != *frame_id {
        *frame_id = generation;
        *frame_window = None;
    }
    let window = frame_window.map_or(size, |seen| seen.max(size));
    *frame_window = Some(window);
    window
}

impl OverlayDraw for SpriteOverlay {
    fn draw(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        // Hold the lock only to hand the frame over: texture uploads, pipeline creation and the
        // encode run unlocked, so extraction and `stats()` never wait on GPU work, and a panic
        // in the draw cannot poison the shared state (concurrency audit 11-03 H1).
        // The frame is copied into this overlay's own buffers (capacity reused, no per-frame
        // allocation once warm) and the renderer is taken out while it draws.
        let (renderer, settings) = {
            let mut state = lock_pass(&self.state);
            let size = UVec2::new(width, height);
            let window = track_frame_window(
                &mut self.frame_id,
                &mut self.frame_window,
                state.prepared.generation,
                size,
            );
            state.last_window = Some(window);
            self.frame.instances.clone_from(&state.prepared.instances);
            self.frame.batches.clone_from(&state.prepared.batches);
            let renderer = state
                .renderer
                .take()
                .filter(|r| r.device_id() == device.device_id());
            (renderer, state.settings.clone())
        };
        let mut renderer = renderer.unwrap_or_else(|| SpriteRenderer::new(device));
        renderer.draw(
            device,
            encoder,
            view,
            format,
            width,
            height,
            &self.frame,
            &settings,
        );
        // Keep the capacity, not the images: a frame that is never drawn again holds no texture.
        self.frame.batches.clear();
        let mut state = lock_pass(&self.state);
        if state.renderer.is_none() {
            state.renderer = Some(renderer);
        }
    }
}

/// Builds a [`SpritePass`] overlay layer for `regia_render::WindowOverlay` (installed by the
/// plugin; tests and tools that drive `WindowOverlay` directly use it too).
#[must_use]
pub fn sprite_overlay(pass: &SpritePass) -> regia_render::SharedOverlay {
    Arc::new(Mutex::new(Box::new(SpriteOverlay {
        state: Arc::clone(&pass.0),
        frame: PreparedFrame::default(),
        frame_id: 0,
        frame_window: None,
    })))
}

/// The 2D toolkit plugin (`docs/specs/22-tier2.md` § T2.10). Add it after `RenderPlugin` (the
/// sprite pass needs `WindowOverlay`; without it the plugin still loads, animates and extracts,
/// which is what headless tests use).
#[derive(Default)]
pub struct Sprite2dPlugin;

impl Plugin for Sprite2dPlugin {
    fn build(&self, app: &mut App) {
        let pass = SpritePass::default();
        // Equal-z draws are ordered by when the component arrived, not by table row (a despawn
        // swaps the last row into the hole) or entity id.
        regia_ecs::spawn_order::track_spawn_order::<Sprite>(app.world_mut());
        regia_ecs::spawn_order::track_spawn_order::<TileMap>(app.world_mut());
        app.init_resource::<SpriteAssets>()
            .init_resource::<crate::autotile::AutoTiles>()
            .init_resource::<View2d>()
            .insert_resource(pass)
            .register_type::<Sprite>()
            .register_type::<SpriteSource>()
            .register_type::<TileMap>()
            .register_type::<AnimatedSprite>()
            .register_type::<Animation>()
            .add_systems(Update, animate_sprites)
            .add_systems(
                PostUpdate,
                (load_2d_assets, extract_2d_frame)
                    .chain()
                    .after(PostUpdateSet::TransformPropagate),
            )
            .add_systems(
                Startup,
                |pass: Res<SpritePass>, overlay: Option<ResMut<WindowOverlay>>| {
                    if let Some(mut overlay) = overlay {
                        overlay.insert_shared(SPRITE_OVERLAY_ORDER, "sprites2d", sprite_overlay(&pass));
                    } else {
                        tracing::debug!(target: "regia_2d", "no WindowOverlay (RenderPlugin missing): sprites will not draw");
                    }
                },
            );
    }
}

/// Decodes a cooked file holding an atlas.
pub fn decode_atlas(bytes: &[u8]) -> Result<SpriteAtlas, String> {
    let (header, payload) = decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    if header.type_tag != TypePathHash::of(ATLAS_TYPE_PATH) {
        return Err("not a cooked sprite atlas".into());
    }
    SpriteAtlas::from_postcard(payload).map_err(|e| e.to_string())
}

/// Decodes a cooked file holding a tilemap.
pub fn decode_tilemap(bytes: &[u8]) -> Result<CookedTileMap, String> {
    let (header, payload) = decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    if header.type_tag != TypePathHash::of(TILEMAP_TYPE_PATH) {
        return Err("not a cooked tilemap".into());
    }
    CookedTileMap::from_postcard(payload).map_err(|e| e.to_string())
}

/// The `failed` reason for a path the cooked manifest does not list (cleared once it appears).
const NOT_IN_MANIFEST: &str = "not in the cooked manifest";

/// What kind of 2D asset a path is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Atlas,
    Map,
}

/// `PostUpdate`: makes sure every atlas and tilemap a [`Sprite`] or [`TileMap`] names is loaded,
/// and reloads one whose asset-server hot-reload generation moved on (`specs/12` § 8: the server
/// bumps [`AssetServer::generation`] for ids registered with `track_external`).
///
/// Without an [`AssetServer`] only registered content resolves. A failed path is reported once
/// per generation (warn) and not retried until the asset changes.
#[tracing::instrument(skip_all)]
pub fn load_2d_assets(
    sprites: Query<&Sprite>,
    maps: Query<&TileMap>,
    server: Option<ResMut<AssetServer>>,
    mut assets: ResMut<SpriteAssets>,
) {
    let Some(mut server) = server else { return };
    // Borrowed: this runs every `PostUpdate`, and nearly always nothing changed.
    let mut wanted: BTreeSet<(&str, bool)> = BTreeSet::new();
    for s in &sprites {
        if !s.atlas.is_empty() {
            wanted.insert((s.atlas.as_str(), true));
        }
    }
    for m in &maps {
        if !m.map.is_empty() {
            wanted.insert((m.map.as_str(), false));
        }
    }
    assets.release_unwanted(&wanted);
    for (path, is_atlas) in wanted {
        let kind = if is_atlas { Kind::Atlas } else { Kind::Map };
        let origin = match kind {
            Kind::Atlas => assets.atlases.get(path).map(|(_, o)| o.clone()),
            Kind::Map => assets.tilemaps.get(path).map(|(_, o)| o.clone()),
        };
        // Registered (in-memory) content never reloads from the server.
        if origin.as_ref().is_some_and(|o| o.id.is_none()) {
            continue;
        }
        let Some(id) = origin
            .as_ref()
            .and_then(|o| o.id)
            .or_else(|| server.id_for_path(path))
        else {
            if let std::collections::hash_map::Entry::Vacant(slot) = assets.failed.entry(path.to_owned()) {
                tracing::warn!(target: "regia_2d", path = %slot.key(), "2D asset is not in the cooked manifest");
                slot.insert((0, NOT_IN_MANIFEST.into()));
            }
            continue;
        };
        // The manifest gained the path since it was reported missing: forget that report, or a
        // generation-0 asset would match the placeholder and never load.
        if assets
            .failed
            .get(path)
            .is_some_and(|(_, why)| why == NOT_IN_MANIFEST)
        {
            assets.failed.remove(path);
        }
        let generation = server.generation(id);
        if origin.as_ref().is_some_and(|o| o.generation == generation) {
            continue;
        }
        if assets.failed_at(path, is_atlas, generation) {
            continue;
        }
        server.track_external(id);
        let origin = AssetOrigin {
            id: Some(id),
            generation,
        };
        let loaded = server
            .read_cooked(id)
            .map_err(|e| e.to_string())
            .and_then(|(bytes, _)| match kind {
                Kind::Atlas => decode_atlas(&bytes).map(|a| {
                    assets
                        .atlases
                        .insert(path.to_owned(), (Arc::new(LoadedAtlas::new(a)), origin.clone()));
                }),
                Kind::Map => decode_tilemap(&bytes).map(|m| {
                    assets
                        .tilemaps
                        .insert(path.to_owned(), (Arc::new(LoadedTileMap::new(m)), origin.clone()));
                }),
            });
        match loaded {
            Ok(()) => {
                assets.failed.remove(path);
                assets.failed_generation.remove(&(path.to_owned(), is_atlas));
                assets.revision += 1;
                tracing::info!(target: "regia_2d", %path, generation, "2D asset loaded");
            }
            Err(err) => {
                // A broken reload keeps the previous version (it stays in the map).
                tracing::warn!(target: "regia_2d", %path, %err, "2D asset failed to load");
                assets.record_failure(path, is_atlas, generation, err);
            }
        }
    }
}

/// `PostUpdate`: builds this frame's sorted quads from every visible [`Sprite`] and [`TileMap`]
/// and hands them to the sprite pass with the current [`View2d`].
#[tracing::instrument(skip_all)]
#[allow(clippy::too_many_arguments)] // a system's parameters are its data access
pub fn extract_2d_frame(
    time: Res<Time>,
    view: Res<View2d>,
    assets: Res<SpriteAssets>,
    pass: Res<SpritePass>,
    sprites: Query<(Entity, &Sprite, &GlobalTransform)>,
    maps: Query<(Entity, &TileMap, &GlobalTransform)>,
    sprite_order: Res<regia_ecs::spawn_order::SpawnOrder<Sprite>>,
    map_order: Res<regia_ecs::spawn_order::SpawnOrder<TileMap>>,
) {
    let settings = DrawSettings {
        camera: view.camera,
        pixel_perfect: view.pixel_perfect,
        snap: view.snap,
        clear: view.clear,
    };
    let (mut frame, mut recycle, last_window) = {
        let mut state = lock_pass(&pass.0);
        (
            std::mem::take(&mut state.scratch),
            std::mem::take(&mut state.recycle),
            state.last_window,
        )
    };
    frame.clear();
    // The presentation clock, as ADR-1524 places all animation on the render clock: sprite
    // sheets (`animate_sprites`) and animated tiles advance on the same clock, so a render-out
    // `time_map` (which holds simulation time via `SimStep`, ADR-1524) can never freeze one 2D
    // animation kind while the other keeps playing (determinism review fixrev-03-k M3). Without a
    // `SimStep` this is identical to the sim clock.
    frame.time_ms = time.elapsed().as_millis() as u64;
    if let Some(window) = last_window {
        let size = settings.render_size(window);
        let camera = settings.effective_camera(size);
        let rect = camera.visible_rect(Vec2::new(size.x as f32, size.y as f32));
        // A rotated view sees the rectangle's rotated bounds: grow it to the circumscribed square.
        frame.visible = Some(if camera.rotation == 0.0 {
            rect
        } else {
            let r = rect.size.length() * 0.5;
            Rect2 {
                min: rect.center() - Vec2::splat(r),
                size: Vec2::splat(2.0 * r),
            }
        });
    }
    let mut stats = Frame2dStats::default();
    let mut maps: Vec<_> = maps.iter().collect();
    map_order.sort_by_spawn(&mut maps, |item| item.0);
    let mut sprites: Vec<_> = sprites.iter().collect();
    sprite_order.sort_by_spawn(&mut sprites, |item| item.0);
    for (_, map, transform) in maps {
        if !map.visible {
            continue;
        }
        if let Some(loaded) = assets.tilemap(&map.map) {
            let drawn = frame.push_tilemap(loaded, map, &transform.0, map.color);
            stats.tiles = stats
                .tiles
                .saturating_add(u32::try_from(drawn).unwrap_or(u32::MAX));
        }
    }
    for (_, sprite, transform) in sprites {
        if !sprite.visible {
            continue;
        }
        if let Some(atlas) = assets.atlas(&sprite.atlas)
            && frame.push_sprite(atlas, sprite, &transform.0, sprite.color)
        {
            stats.sprites += 1;
        }
    }
    let capacity_before = recycle.instances.capacity();
    frame.prepare_into(&mut recycle);
    let grew = recycle.instances.capacity() > capacity_before;
    stats.instances = recycle.instances.len() as u32;
    stats.batches = recycle.batches.len() as u32;
    let mut state = lock_pass(&pass.0);
    let mut generation = state.frame_seq.wrapping_add(1);
    if generation == 0 {
        generation = 1;
    }
    state.frame_seq = generation;
    recycle.generation = generation;
    // Keep the buffer we just replaced: the next extract writes into it, and `prepared` stays
    // complete for a draw that overlaps this extract.
    let previous = std::mem::replace(&mut state.prepared, recycle);
    state.recycle = previous;
    state.recycle.instances.clear();
    state.recycle.batches.clear();
    state.scratch = frame;
    state.settings = settings;
    state.stats = stats;
    if grew {
        state.instance_grows = state.instance_grows.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_atlas, decode_tilemap};
    use crate::atlas::{ATLAS_TYPE_PATH, RgbaImage, SpriteAtlas};
    use crate::tilemap::{
        CookedTileMap, MapLayer, Orientation, TILEMAP_TYPE_PATH, TileLayer, TileMapAsset, TilesetDef,
    };
    use regia_asset::{AssetId, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file};
    use regia_core::id::TypePathHash;

    fn wrap(type_path: &str, payload: &[u8]) -> Vec<u8> {
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: TypePathHash::of(type_path),
            asset_id: AssetId::NIL,
            importer_version: 0,
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        encode_cooked_file(&header, payload).expect("cooked wrapper")
    }

    fn level() -> CookedTileMap {
        CookedTileMap {
            version: 1,
            map: TileMapAsset {
                version: 1,
                orientation: Orientation::Orthogonal,
                width: 1,
                height: 1,
                tile_width: 1,
                tile_height: 1,
                background: None,
                tilesets: vec![TilesetDef {
                    name: "t".into(),
                    first_gid: 1,
                    image: "t.png".into(),
                    image_width: 1,
                    image_height: 1,
                    tile_width: 1,
                    tile_height: 1,
                    columns: 1,
                    tile_count: 1,
                    spacing: 0,
                    margin: 0,
                    tiles: Vec::new(),
                }],
                layers: vec![MapLayer::Tiles(TileLayer {
                    name: "ground".into(),
                    visible: true,
                    opacity: 1.0,
                    offset: [0.0, 0.0],
                    data: vec![0],
                    properties: Default::default(),
                })],
                properties: Default::default(),
            },
            images: vec![RgbaImage {
                width: 1,
                height: 1,
                rgba8: vec![4, 5, 6, 7],
            }],
        }
    }

    #[test]
    fn decode_atlas_and_decode_tilemap_round_trip_and_reject_the_wrong_bytes() {
        let atlas = SpriteAtlas::single(
            "dot",
            RgbaImage {
                width: 1,
                height: 1,
                rgba8: vec![1, 2, 3, 4],
            },
        );
        let atlas_bytes = wrap(ATLAS_TYPE_PATH, &atlas.to_postcard().expect("atlas payload"));
        assert_eq!(decode_atlas(&atlas_bytes).expect("atlas"), atlas);

        let map = level();
        let map_bytes = wrap(TILEMAP_TYPE_PATH, &map.to_postcard().expect("map payload"));
        assert_eq!(decode_tilemap(&map_bytes).expect("tilemap"), map);

        assert_eq!(
            decode_atlas(&map_bytes).expect_err("atlas tag"),
            "not a cooked sprite atlas"
        );
        assert_eq!(
            decode_tilemap(&atlas_bytes).expect_err("tilemap tag"),
            "not a cooked tilemap"
        );
        let bad_magic = "not a Regia Engine cooked file (bad magic or truncated header)";
        assert_eq!(decode_atlas(&[]).expect_err("empty"), bad_magic);
        assert_eq!(decode_tilemap(&[1, 2, 3, 4]).expect_err("magic"), bad_magic);

        let mut newer = atlas.clone();
        newer.version = 2;
        assert_eq!(
            decode_atlas(&wrap(ATLAS_TYPE_PATH, &newer.to_postcard().expect("versioned")))
                .expect_err("version"),
            "atlas version 2 is newer than supported version 1"
        );
        let mut outside = atlas;
        outside.sprites[0].rect = [0, 0, 2, 1];
        assert_eq!(
            decode_atlas(&wrap(ATLAS_TYPE_PATH, &outside.to_postcard().expect("outside"))).expect_err("rect"),
            "invalid atlas: sprite `dot` lies outside its page"
        );

        let mut images = map;
        images.images.clear();
        assert_eq!(
            decode_tilemap(&wrap(TILEMAP_TYPE_PATH, &images.to_postcard().expect("images")))
                .expect_err("images"),
            "invalid tilemap: 0 images for 1 tilesets"
        );
    }

    /// A 1×1 map whose only cell is a tile with a 100 ms / 100 ms two-frame animation.
    fn animated_level() -> CookedTileMap {
        let mut map = level();
        let set = &mut map.map.tilesets[0];
        set.image_width = 2;
        set.columns = 2;
        set.tile_count = 2;
        set.tiles = vec![crate::tilemap::TileDef {
            id: 0,
            class: String::new(),
            collision: Default::default(),
            animation: vec![
                crate::tilemap::TileFrame {
                    tile: 0,
                    duration_ms: 100,
                },
                crate::tilemap::TileFrame {
                    tile: 1,
                    duration_ms: 100,
                },
            ],
            properties: Default::default(),
        }];
        map.map.layers[0] = MapLayer::Tiles(TileLayer {
            data: vec![1],
            ..match map.map.layers[0].clone() {
                MapLayer::Tiles(layer) => layer,
                MapLayer::Objects(_) => unreachable!("level() has a tile layer"),
            }
        });
        map.images[0].width = 2;
        map.images[0].rgba8 = vec![4, 5, 6, 7, 8, 9, 10, 11];
        map
    }

    /// Animated tiles and sprite sheets advance on the same clock (ADR-1524: animation follows
    /// the presentation/render clock). Under a `SimStep` that sets the clocks apart, animated
    /// tiles keep following the presentation clock — the regression this pins is the tile clock
    /// moving alone to `sim_elapsed`, which froze water tiles while sprite-sheet characters kept
    /// animating under a render-out `time_map` (determinism review fixrev-03-k M3).
    #[test]
    fn animated_tiles_follow_the_presentation_clock_with_the_sprites() {
        use super::{Sprite2dPlugin, SpriteAssets, SpritePass, lock_pass};
        use crate::sprite::TileMap;
        use regia_app::prelude::*;
        use regia_scene::Transform;
        use std::time::Duration;

        // The UV of the one extracted quad: a 100/100 ms animation at 100 ms shows its second
        // frame (local tile 1, the right half of the 2×1 tileset image) — and at 250 ms its
        // third (local tile 0, the left half), whether or not a SimStep holds the sim clock.
        let run = |frame: Duration, sim_step: Option<Duration>| {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, Sprite2dPlugin));
            app.insert_resource(TimeUpdateStrategy::ManualDuration(frame));
            if let Some(step) = sim_step {
                app.insert_resource(SimStep(step));
            }
            app.world_mut()
                .resource_mut::<SpriteAssets>()
                .insert_tilemap("m", animated_level());
            app.world_mut().spawn((TileMap::new("m"), Transform::default()));
            for _ in 0..5 {
                app.update();
            }
            let pass = app.world().resource::<SpritePass>();
            let state = lock_pass(&pass.0);
            assert_eq!(state.prepared.instances.len(), 1, "one tile quad");
            state.prepared.instances[0].uv_min
        };
        // 100 ms into the animation is frame 2 in every configuration.
        let default_clock = run(Duration::from_millis(100), None);
        let sim_step = run(Duration::from_millis(100), Some(Duration::from_millis(20)));
        assert_eq!(default_clock, [0.5, 0.0], "100 ms is frame 2");
        assert_eq!(
            sim_step, default_clock,
            "a SimStep must not move animated tiles off the sprite clock"
        );
        // 250 ms into the animation is frame 3 in every configuration: the tile clock tracks the
        // presentation time, hitching included (ADR-1524).
        let hitched = run(Duration::from_millis(250), None);
        let hitched_sim = run(Duration::from_millis(250), Some(Duration::from_millis(20)));
        assert_eq!(hitched, [0.0, 0.0], "250 ms wraps to frame 3");
        assert_eq!(hitched_sim, hitched, "a SimStep must not change the tile clock");
    }

    /// Three extracts of one tile allocate two instance buffers (published and recycle). The
    /// third reuses one. A new generation drops the previous window instead of keeping the
    /// largest size the pointer-identity id used to keep when the `Vec` was reused.
    #[test]
    fn extraction_reuses_instance_capacity_and_resets_the_cull_window() {
        use super::{Sprite2dPlugin, SpriteAssets, SpritePass, lock_pass, track_frame_window};
        use crate::sprite::TileMap;
        use regia_app::prelude::*;
        use regia_core::math::UVec2;
        use regia_scene::Transform;

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, Sprite2dPlugin));
        app.world_mut()
            .resource_mut::<SpriteAssets>()
            .insert_tilemap("m", animated_level());
        app.world_mut().spawn((TileMap::new("m"), Transform::default()));

        let mut generations = Vec::new();
        let mut grows = Vec::new();
        for _ in 0..3 {
            app.update();
            let pass = app.world().resource::<SpritePass>();
            // Drop the guard before `retained_instance_capacity`: it locks the same mutex.
            let (generation, grew) = {
                let state = lock_pass(&pass.0);
                assert_eq!(state.prepared.instances.len(), 1);
                (state.prepared.generation, state.instance_grows)
            };
            generations.push(generation);
            grows.push(grew);
            assert!(pass.retained_instance_capacity() >= 1);
        }
        assert_eq!(generations, vec![1, 2, 3]);
        assert_eq!(
            grows,
            vec![1, 2, 2],
            "extract used to allocate a fresh Vec every frame; the third extract reuses one of two buffers"
        );

        let mut frame_id = 0u64;
        let mut seen = None;
        assert_eq!(
            track_frame_window(&mut frame_id, &mut seen, 1, UVec2::splat(100)),
            UVec2::splat(100)
        );
        assert_eq!(
            track_frame_window(&mut frame_id, &mut seen, 1, UVec2::splat(400)),
            UVec2::splat(400)
        );
        assert_eq!(
            track_frame_window(&mut frame_id, &mut seen, 2, UVec2::splat(100)),
            UVec2::splat(100),
            "a new frame must not keep the previous window"
        );
    }

    /// A sprite whose atlas is not cooked scans the manifest once across two `PostUpdate` runs.
    #[test]
    fn asset1_perf_missing_sprite_scans_the_manifest_once() {
        use super::load_2d_assets;
        use crate::sprite::{Sprite, SpriteAssets};
        use regia_app::prelude::*;
        use regia_asset::{
            AssetId, AssetManifest, AssetServer, AssetServerSettings, ManifestEntry, cooked_cache_dir,
            manifest_negative_hits, manifest_path_compares, reset_manifest_lookup_counts,
        };
        use regia_scene::Transform;

        let root = regia_test_utils::unique_temp_dir("asset1_perf_sprite");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let manifest = AssetManifest {
            entries: (0..40)
                .map(|i| {
                    let path = format!("assets/tex/{i}.ktx2");
                    ManifestEntry {
                        id: AssetId::sub_asset(AssetId::NIL, &path),
                        path,
                        type_path: "t".into(),
                    }
                })
                .collect(),
        };
        manifest.write_to_cache(&cache).expect("manifest");
        let server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("server");

        let mut world = World::new();
        world.insert_resource(server);
        world.init_resource::<SpriteAssets>();
        world.spawn((
            Sprite::named("sprites/hero.atlas.ron", "idle"),
            Transform::default(),
        ));
        let mut schedule = Schedule::default();
        // The path counters are thread-local. The default executor is multi-threaded, so this
        // one system would record the walk on a worker the test cannot see.
        schedule.set_executor(regia_ecs::ecs::schedule::SingleThreadedExecutor::new());
        schedule.add_systems(load_2d_assets);

        reset_manifest_lookup_counts();
        schedule.run(&mut world);
        let compares = manifest_path_compares();
        assert!(compares >= 40, "the first frame walked {compares} entries");
        let hits = manifest_negative_hits();
        schedule.run(&mut world);
        let second = manifest_path_compares();
        let second_hits = manifest_negative_hits();
        assert!(
            second == compares && second_hits > hits,
            "sprite miss compares {compares}->{second}, negative hits {hits}->{second_hits}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
