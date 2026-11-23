// SPDX-License-Identifier: MPL-2.0
//! Terrain's side of world streaming (`docs/specs/78-world-streaming.md` § 13.1). The terrain is not
//! partitioned into cell blobs; its tiles stream through [`TileCache`] with their own budget. Asking
//! around the source views instead of the camera alone means a teleport target's ground tiles load
//! with its cells.
//!
//! **Across a floating-origin rebase** the terrain keeps its own world-anchored space: selection,
//! height and virtual-texture lookups, scatter, grass and water stay put, and `regia_render`'s
//! `SharedFrameInfo::in_world_space` moves the camera and the cascades into that space at the one
//! boundary. What it reads that is local (the froxel lights, the cloud map) goes through
//! `EngineCsm::origin` in the shaders; the probes' screen lookup reads by screen texel and eye
//! distance, which a rebase leaves alone. Terrain therefore no longer blocks a rebase (ADR-2022).

use std::sync::{Mutex, PoisonError};

use regia_app::prelude::*;
use regia_scene::stream_plan::SourceView;
use regia_scene::streaming::StreamViews;

use crate::stream::TileCache;

/// A tile cache the engine drives from the streaming views (spec 78 § 13.1): [`TerrainPlugin`](crate::TerrainPlugin)
/// asks around every source each frame and drains finished loads, so a teleport target's ground
/// tiles load with its cells. Insert it as a resource; without it nothing runs.
#[derive(Resource)]
pub struct TerrainTileStream {
    cache: Mutex<TileCache>,
    /// The texture slots to request, in order.
    pub slots: Vec<u8>,
    /// The terrain's center in world meters.
    pub center: [f64; 2],
    /// The terrain's width in meters.
    pub world_width: f32,
    /// Tiles whose footprint is inside this radius of a view are wanted, meters.
    pub visible_radius: f32,
    /// Tiles enqueued per frame and per slot, shared between the views.
    pub max_requests: usize,
}

/// What a [`TerrainTileStream`] holds (`stream.status`'s `terrain_tiles`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerrainTileStats {
    /// Tiles in the cache.
    pub resident: usize,
    /// Tiles the loader has not finished.
    pub pending: usize,
    /// Tiles held back after a failed load.
    pub failed: usize,
    /// Bytes in the cache.
    pub bytes: u64,
    /// The cache's byte budget.
    pub budget: u64,
}

impl TerrainTileStream {
    /// A stream over `cache` for a terrain centered at the world origin, asking for `slots`.
    #[must_use]
    pub fn new(
        cache: TileCache,
        slots: Vec<u8>,
        world_width: f32,
        visible_radius: f32,
        max_requests: usize,
    ) -> Self {
        Self {
            cache: Mutex::new(cache),
            slots,
            center: [0.0; 2],
            world_width,
            visible_radius,
            max_requests,
        }
    }

    /// Runs `f` on the cache (a texture registration, a residency query).
    pub fn with_cache<R>(&mut self, f: impl FnOnce(&mut TileCache) -> R) -> R {
        f(self.cache.get_mut().unwrap_or_else(PoisonError::into_inner))
    }

    /// The cache's counters.
    #[must_use]
    pub fn stats(&self) -> TerrainTileStats {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        TerrainTileStats {
            resident: cache.resident_count(),
            pending: cache.pending_count(),
            failed: cache.failed_count(),
            bytes: cache.current_bytes(),
            budget: cache.budget(),
        }
    }

    /// The loads the stream has asked its cache for so far ([`TileCache::loads_issued`]). It
    /// only grows, so it shows a request even when the load finished within the same frame and
    /// [`TerrainTileStats::pending`] reads 0 again.
    #[must_use]
    pub fn loads_issued(&self) -> u64 {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .loads_issued()
    }
}

/// Requests the stream's tiles around the views and drains finished loads, once a frame.
pub(crate) fn drive_tile_stream(stream: Option<ResMut<TerrainTileStream>>, views: Option<Res<StreamViews>>) {
    let Some(mut stream) = stream else {
        return;
    };
    let _span = tracing::trace_span!("terrain_tile_stream").entered();
    let stream = &mut *stream;
    let cache = stream.cache.get_mut().unwrap_or_else(PoisonError::into_inner);
    if let Some(views) = views {
        for &slot in &stream.slots {
            request_around_views(
                cache,
                slot,
                &views.0,
                stream.center,
                stream.world_width,
                stream.visible_radius,
                stream.max_requests,
            );
        }
    }
    cache.tick();
}

/// Requests the tiles of `slot` around every view, splitting `max_requests` evenly so a far pin
/// cannot take the camera's share. Views are walked in slice order, not by distance to the
/// terrain, so the first view's tiles are queued first. Every view is asked even when the
/// earlier ones used up the requests: its tiles stay in the round's wanted set, so a trailing
/// view's queued loads are not cancelled and its resident tiles are not evicted as unwanted.
/// `center` is the terrain's center in world meters; positions go to the cache relative to it,
/// as `f32`. Returns the tiles enqueued.
pub fn request_around_views(
    cache: &mut TileCache,
    slot: u8,
    views: &[SourceView],
    center: [f64; 2],
    world_width: f32,
    visible_radius: f32,
    max_requests: usize,
) -> usize {
    if views.is_empty() {
        return 0;
    }
    let share = max_requests.div_ceil(views.len());
    let mut enqueued = 0;
    for view in views {
        let left = max_requests.saturating_sub(enqueued);
        let at = [(view.pos[0] - center[0]) as f32, (view.pos[2] - center[1]) as f32];
        enqueued += cache.request(slot, at, world_width, visible_radius, share.min(left));
    }
    enqueued
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{MemoryTileSource, TileFormat, TileKey, TiledTexture};
    use regia_scene::stream_plan::SourceId;

    fn cache() -> TileCache {
        let mut cache = TileCache::new(1 << 30, MemoryTileSource::new());
        cache.register_texture(0, TiledTexture::new("height", 4096, 256, TileFormat::Bc4));
        cache
    }

    fn view(n: u64, x: f64, z: f64) -> SourceView {
        SourceView::at(SourceId::Pin(n), [x, 0.0, z])
    }

    /// A cache over a 4096 m height texture whose source holds every tile and the tail.
    fn full_cache() -> TileCache {
        use crate::stream::tail_key;
        let texture = TiledTexture::new("height", 4096, 256, TileFormat::Bc4);
        let mut source = MemoryTileSource::new();
        for mip in 0..=texture.tail_start_mip() {
            for y in 0..texture.tiles_per_side(mip) {
                for x in 0..texture.tiles_per_side(mip) {
                    source.insert(
                        TileKey {
                            texture: 0,
                            mip: mip as u8,
                            x,
                            y,
                        },
                        vec![0; 64],
                    );
                }
            }
        }
        source.insert(tail_key(0), vec![0; 64]);
        let mut cache = TileCache::new(1 << 30, source);
        cache.register_texture(0, texture);
        cache
    }

    /// Waits for every load the frame issued: the schedule of a fast host, where the loader
    /// answers before `drive_tile_stream` reaches its `tick` and the frame drains them all.
    fn finish_loads_in_frame(mut stream: ResMut<TerrainTileStream>) {
        stream.with_cache(TileCache::flush);
    }

    /// The flake of `the_stream_loads_tiles_around_every_view` (fixrev6, rev-claude-25-k f7) made
    /// deterministic: when every load finishes inside the frame that asked for it, no read after
    /// `app.update()` sees `pending > 0`, although tiles were requested and loaded. The
    /// cumulative [`TerrainTileStream::loads_issued`] still counts every one of them.
    #[test]
    fn a_load_finished_inside_its_frame_still_counts_as_asked() {
        let mut app = App::new();
        app.insert_resource(TerrainTileStream::new(full_cache(), vec![0], 4096.0, 300.0, 4))
            .insert_resource(StreamViews(vec![view(0, 1500.0, -1500.0)]))
            .add_systems(Update, (drive_tile_stream, finish_loads_in_frame).chain());
        let mut saw_pending = false;
        for _ in 0..8 {
            app.update();
            let stats = app.world().resource::<TerrainTileStream>().stats();
            saw_pending |= stats.pending > 0;
        }
        let stream = app.world().resource::<TerrainTileStream>();
        let stats = stream.stats();
        let issued = stream.loads_issued();
        assert!(stats.resident > 1, "tiles were requested and loaded: {stats:?}");
        assert!(
            !saw_pending,
            "this schedule drains every load in its frame, so the instantaneous sample reads 0: {stats:?}"
        );
        // A static view and a 1 GiB budget: nothing is cancelled or evicted, so every load
        // the stream issued is resident.
        assert_eq!(issued, stats.resident as u64, "{stats:?}");
    }

    /// The plugin's system asks around the views: a far source gets its ground tiles without the
    /// camera going there, and finished loads arrive with no host calling the cache.
    #[test]
    fn the_stream_loads_tiles_around_every_view() {
        let cache = full_cache();
        let mut app = App::new();
        app.insert_resource(TerrainTileStream::new(cache, vec![0], 4096.0, 300.0, 4))
            .insert_resource(StreamViews(vec![view(0, 1500.0, -1500.0)]))
            .add_systems(Update, drive_tile_stream);
        for _ in 0..2000 {
            app.update();
            let stats = app.world().resource::<TerrainTileStream>().stats();
            if stats.resident > 1 && stats.pending == 0 {
                break;
            }
            std::thread::yield_now();
        }
        let stream = app.world().resource::<TerrainTileStream>();
        let stats = stream.stats();
        // Asked: a cumulative count, not a `pending > 0` sample. A load can finish and be
        // drained inside the frame that asked for it, so `pending` may never read above 0
        // after an update (`a_load_finished_inside_its_frame_still_counts_as_asked`).
        assert!(
            stream.loads_issued() > 1,
            "the tail and tiles near the view: {stats:?}"
        );
        assert!(stats.resident > 1, "the tail and tiles near the view: {stats:?}");
        assert!(stats.bytes > 0 && stats.budget == 1 << 30, "{stats:?}");
    }

    /// Without the resource, or without views, the system does nothing and asks for nothing.
    #[test]
    fn no_stream_no_work() {
        let mut app = App::new();
        app.add_systems(Update, drive_tile_stream);
        app.update();
        let mut cache = TileCache::new(1 << 30, MemoryTileSource::new());
        cache.register_texture(0, TiledTexture::new("height", 4096, 256, TileFormat::Bc4));
        app.insert_resource(TerrainTileStream::new(cache, vec![0], 4096.0, 300.0, 4));
        app.update();
        let stream = app.world().resource::<TerrainTileStream>();
        assert_eq!(stream.stats().pending, 0);
        // `pending == 0` alone would also hold for a load issued and drained within the frame.
        assert_eq!(stream.loads_issued(), 0, "no views, no requests");
    }

    #[test]
    fn every_view_gets_its_share_of_the_requests() {
        let mut a = cache();
        let near = request_around_views(&mut a, 0, &[view(0, 0.0, 0.0)], [0.0, 0.0], 4096.0, 300.0, 4);
        assert_eq!(near, 4);
        let mut b = cache();
        let both = request_around_views(
            &mut b,
            0,
            &[view(0, 0.0, 0.0), view(1, 1900.0, 1900.0)],
            [0.0, 0.0],
            4096.0,
            300.0,
            4,
        );
        assert_eq!(both, 4);
        assert_eq!(b.pending_count(), a.pending_count());
    }

    /// The audit M1 scenario through the frame entry point: `[camera, pin]` in one
    /// `request_around_views` call must protect both footprints, not only the last view's.
    #[test]
    fn two_views_share_the_wanted_set_when_the_budget_is_tight() {
        let texture = TiledTexture::new("height", 128, 32, TileFormat::Bc4);
        let mut source = MemoryTileSource::new();
        for mip in 0..texture.tail_start_mip() {
            for y in 0..texture.tiles_per_side(mip) {
                for x in 0..texture.tiles_per_side(mip) {
                    source.insert(
                        TileKey {
                            texture: 0,
                            mip: mip as u8,
                            x,
                            y,
                        },
                        vec![0; 8],
                    );
                }
            }
        }
        source.insert(crate::stream::tail_key(0), vec![0; 4]);
        // One view's mip-0 footprint (three tiles) plus the pinned tail.
        let mut cache = TileCache::new(3 * 8 + 4, source);
        cache.register_texture(0, texture);
        request_around_views(
            &mut cache,
            0,
            &[view(0, -48.0, -48.0), view(1, 48.0, 48.0)],
            [0.0, 0.0],
            128.0,
            8.0,
            32,
        );
        cache.flush();
        assert!(
            cache.is_resident(TileKey {
                texture: 0,
                mip: 0,
                x: 0,
                y: 0
            }),
            "the camera's nearest tile stays resident"
        );
        assert!(
            !cache.is_resident(TileKey {
                texture: 0,
                mip: 0,
                x: 2,
                y: 3
            }),
            "the far view's edge tile is the eviction victim, not the camera's"
        );
    }

    /// A view walked after the request budget ran out still marks its tiles wanted: the
    /// first request of a round replaces the wanted set, so a skipped view would lose its
    /// queued loads to `cancel_stale` and its resident tiles to eviction.
    #[test]
    fn a_view_past_the_request_budget_keeps_its_tiles_wanted() {
        let views = [
            view(0, -1500.0, -1500.0),
            view(1, 1500.0, -1500.0),
            view(2, 1500.0, 1500.0),
        ];
        let mut alone = cache();
        request_around_views(&mut alone, 0, &views[2..], [0.0, 0.0], 4096.0, 300.0, 64);
        let own = alone.wanted_keys();
        assert!(!own.is_empty());
        let mut c = cache();
        let enqueued = request_around_views(&mut c, 0, &views, [0.0, 0.0], 4096.0, 300.0, 2);
        assert_eq!(enqueued, 2, "the first two views take the whole budget");
        let wanted = c.wanted_keys();
        let lost: Vec<_> = own.iter().filter(|k| !wanted.contains(k)).collect();
        assert!(
            lost.is_empty(),
            "the third view's tiles dropped out of the wanted set: {lost:?}"
        );
    }

    #[test]
    fn no_views_ask_for_nothing_and_the_center_offsets_positions() {
        let mut c = cache();
        assert_eq!(
            request_around_views(&mut c, 0, &[], [0.0, 0.0], 4096.0, 300.0, 4),
            0
        );
        assert_eq!(c.pending_count(), 0);
        // Far outside the terrain once the center is taken off: no tile is near.
        let far = request_around_views(
            &mut c,
            0,
            &[view(0, 0.0, 0.0)],
            [100_000.0, 0.0],
            4096.0,
            300.0,
            4,
        );
        assert_eq!(far, 0);
    }
}
