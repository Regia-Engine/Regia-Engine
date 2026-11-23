// SPDX-License-Identifier: MPL-2.0
//! Sprite and tilemap components, the loaded 2D asset store, and the view settings.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_scene::Transform;

use crate::atlas::{RgbaImage, SpriteAtlas};
use crate::camera::{OrthoCamera, PixelPerfect};
use crate::tilemap::CookedTileMap;

/// Which part of the atlas a [`Sprite`] shows.
#[derive(Reflect, Clone, Debug, PartialEq)]
pub enum SpriteSource {
    /// The atlas sprite with this name (its pivot, trim and 9-slice apply).
    Named(String),
    /// A pixel rectangle `[x, y, w, h]` (origin top-left) of page 0 — sprite sheets drawn by
    /// [`crate::animation::AnimatedSprite`]. The pivot is [`Sprite::anchor`] or the centre.
    Rect {
        /// `[x, y, width, height]`.
        rect: [u32; 4],
    },
}

impl Default for SpriteSource {
    fn default() -> Self {
        Self::Named(String::new())
    }
}

/// A 2D sprite: a region of an atlas drawn at the entity's world pose, its `GlobalTransform` (local `Transform` composed with the parents', after `TransformPropagate`; x, y in world units; z
/// orders sprites, higher in front; rotation about Z; scale).
///
/// One world unit is one source pixel at scale 1. `atlas` is the asset path of a cooked atlas
/// (`assets/…/name.atlas.ron`) or a name registered with [`SpriteAssets::insert_atlas`].
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
#[require(Transform)]
pub struct Sprite {
    /// Atlas asset path or registered name.
    pub atlas: String,
    /// Region of the atlas.
    pub source: SpriteSource,
    /// Tint multiplied with the texels (sRGB-encoded RGBA, 1 = unchanged).
    /// Drawing caps finite positive alpha at 1 and skips non-finite or non-positive alpha.
    pub color: [f32; 4],
    /// Mirror horizontally.
    pub flip_x: bool,
    /// Mirror vertically.
    pub flip_y: bool,
    /// Drawn size in world units; `None` = the source size. A 9-slice sprite stretches its edges
    /// and centre to this size and keeps its corners; if the size is smaller than the borders added
    /// together, the two borders on that axis shrink proportionally.
    pub size: Option<[f32; 2]>,
    /// Pivot override, `(0, 0)` = bottom-left of the drawn size.
    pub anchor: Option<[f32; 2]>,
    /// Skips drawing when false.
    pub visible: bool,
}

impl Default for Sprite {
    fn default() -> Self {
        Self {
            atlas: String::new(),
            source: SpriteSource::default(),
            color: [1.0; 4],
            flip_x: false,
            flip_y: false,
            size: None,
            anchor: None,
            visible: true,
        }
    }
}

impl Sprite {
    /// The atlas sprite `name` of `atlas`.
    #[must_use]
    pub fn named(atlas: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            atlas: atlas.into(),
            source: SpriteSource::Named(name.into()),
            ..Self::default()
        }
    }

    /// A pixel rectangle of page 0 of `atlas`.
    #[must_use]
    pub fn rect(atlas: impl Into<String>, rect: [u32; 4]) -> Self {
        Self {
            atlas: atlas.into(),
            source: SpriteSource::Rect { rect },
            ..Self::default()
        }
    }
}

/// A tilemap drawn at the entity's world pose, its `GlobalTransform` (the map's bottom-left corner; z orders it among
/// sprites; every layer adds [`TileMap::layer_z_step`]).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
#[require(Transform)]
pub struct TileMap {
    /// Cooked tilemap asset path (`assets/…/level.tmx`, `.tmj` or `.tilemap.ron`) or a name
    /// registered with [`SpriteAssets::insert_tilemap`].
    pub map: String,
    /// Tint multiplied with every tile.
    /// Drawing caps alpha at 1 after layer opacity; non-finite or non-positive results are skipped.
    pub color: [f32; 4],
    /// Z added per layer index, so later layers draw in front.
    pub layer_z_step: f32,
    /// Skips drawing when false.
    pub visible: bool,
}

impl Default for TileMap {
    fn default() -> Self {
        Self {
            map: String::new(),
            color: [1.0; 4],
            layer_z_step: 0.001,
            visible: true,
        }
    }
}

impl TileMap {
    /// A tilemap component for `map`.
    #[must_use]
    pub fn new(map: impl Into<String>) -> Self {
        Self {
            map: map.into(),
            ..Self::default()
        }
    }
}

/// Source of unique GPU texture keys (a reloaded image gets a new key, so the renderer uploads it
/// again).
static NEXT_IMAGE_KEY: AtomicU64 = AtomicU64::new(1);

/// An image ready for the GPU: pixels plus a process-unique key the renderer caches by.
#[derive(Debug)]
pub struct GpuImage {
    /// Unique key.
    pub key: u64,
    /// The pixels.
    pub image: RgbaImage,
}

impl GpuImage {
    /// Wraps `image` with a fresh key.
    #[must_use]
    pub fn new(image: RgbaImage) -> Arc<Self> {
        Arc::new(Self {
            key: NEXT_IMAGE_KEY.fetch_add(1, Ordering::Relaxed),
            image,
        })
    }
}

/// A loaded atlas: the cooked data with its pages wrapped for the GPU.
#[derive(Debug)]
pub struct LoadedAtlas {
    /// The atlas (page pixels moved into `pages`; `atlas.pages` is empty).
    pub atlas: SpriteAtlas,
    /// The pages.
    pub pages: Vec<Arc<GpuImage>>,
}

impl LoadedAtlas {
    /// Wraps a cooked atlas.
    #[must_use]
    pub fn new(mut atlas: SpriteAtlas) -> Self {
        let pages = std::mem::take(&mut atlas.pages)
            .into_iter()
            .map(GpuImage::new)
            .collect();
        Self { atlas, pages }
    }
}

/// A loaded tilemap: the map with its tileset images wrapped for the GPU.
#[derive(Debug)]
pub struct LoadedTileMap {
    /// The map.
    pub map: crate::tilemap::TileMapAsset,
    /// One image per tileset.
    pub images: Vec<Arc<GpuImage>>,
    /// Animation prefix sums, one slot per tileset, rebuilt when the tileset's animation
    /// fingerprint changes (a paint or an in-memory edit of the animation tables).
    anim: Mutex<Vec<AnimSlot>>,
}

/// One tileset's cached animation prefixes.
#[derive(Clone, Debug)]
struct AnimSlot {
    fingerprint: u64,
    prefixes: Arc<[crate::tilemap::TileAnimPrefix]>,
}

impl LoadedTileMap {
    /// Wraps a cooked tilemap and caches each tileset's animation prefixes.
    #[must_use]
    pub fn new(cooked: CookedTileMap) -> Self {
        let map = cooked.map;
        let anim = anim_slots(&map);
        Self {
            map,
            images: cooked.images.into_iter().map(GpuImage::new).collect(),
            anim: Mutex::new(anim),
        }
    }

    /// Prefix sums for tileset `set_index`. Rebuilt when the tileset's animation fingerprint
    /// no longer matches the cache, so an edit through [`SpriteAssets::tilemap_mut`] is visible
    /// on the next extraction without scanning frames per cell.
    pub(crate) fn anim_prefix(&self, set_index: usize) -> Arc<[crate::tilemap::TileAnimPrefix]> {
        let mut cache = self
            .anim
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() != self.map.tilesets.len() {
            *cache = anim_slots(&self.map);
        }
        let Some(set) = self.map.tilesets.get(set_index) else {
            return Arc::from([]);
        };
        let fingerprint = set.anim_fingerprint();
        let stale = cache
            .get(set_index)
            .is_none_or(|slot| slot.fingerprint != fingerprint);
        if stale {
            if set_index >= cache.len() {
                cache.resize(
                    self.map.tilesets.len(),
                    AnimSlot {
                        fingerprint: 0,
                        prefixes: Arc::from([]),
                    },
                );
            }
            cache[set_index] = AnimSlot {
                fingerprint,
                prefixes: Arc::from(set.anim_prefixes()),
            };
        }
        Arc::clone(&cache[set_index].prefixes)
    }
}

fn anim_slots(map: &crate::tilemap::TileMapAsset) -> Vec<AnimSlot> {
    map.tilesets
        .iter()
        .map(|set| AnimSlot {
            fingerprint: set.anim_fingerprint(),
            prefixes: Arc::from(set.anim_prefixes()),
        })
        .collect()
}

/// Where a loaded 2D asset came from and which asset-server generation it reflects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssetOrigin {
    /// The asset id, when loaded through the asset server.
    pub id: Option<regia_asset::AssetId>,
    /// The server's hot-reload generation when it was read.
    pub generation: u64,
}

/// The loaded atlases and tilemaps, by path or registered name.
///
/// [`crate::Sprite2dPlugin`] fills it from the asset server for every path a [`Sprite`] or
/// [`TileMap`] names, and reloads an entry when the server's hot-reload generation for its id
/// changes. Games and tests can also register in-memory content.
#[derive(Resource, Default, Debug)]
pub struct SpriteAssets {
    /// Atlases by path/name.
    pub atlases: HashMap<String, (Arc<LoadedAtlas>, AssetOrigin)>,
    /// Tilemaps by path/name.
    pub tilemaps: HashMap<String, (Arc<LoadedTileMap>, AssetOrigin)>,
    /// Paths that failed to load, with the generation that failed (not retried until it changes).
    pub failed: HashMap<String, (u64, String)>,
    /// The generation that failed per `(path, is_atlas)`: the retry gate. `failed` keeps the latest
    /// message per path for reporting, but a failed tilemap decode must not block the atlas
    /// of the same path.
    pub failed_generation: HashMap<(String, bool), u64>,
    /// Bumped whenever an entry is added or replaced.
    pub revision: u64,
}

impl SpriteAssets {
    /// Registers an in-memory atlas under `name`.
    pub fn insert_atlas(&mut self, name: impl Into<String>, atlas: SpriteAtlas) {
        self.atlases.insert(
            name.into(),
            (Arc::new(LoadedAtlas::new(atlas)), AssetOrigin::default()),
        );
        self.revision += 1;
    }

    /// Registers an in-memory tilemap under `name`.
    pub fn insert_tilemap(&mut self, name: impl Into<String>, map: CookedTileMap) {
        self.tilemaps.insert(
            name.into(),
            (Arc::new(LoadedTileMap::new(map)), AssetOrigin::default()),
        );
        self.revision += 1;
    }

    /// Removes the atlas at `path`; true when it existed.
    pub fn remove_atlas(&mut self, path: &str) -> bool {
        let removed = self.atlases.remove(path).is_some();
        self.revision += u64::from(removed);
        removed
    }

    /// Removes the tilemap at `path`; true when it existed.
    pub fn remove_tilemap(&mut self, path: &str) -> bool {
        let removed = self.tilemaps.remove(path).is_some();
        self.revision += u64::from(removed);
        removed
    }

    /// Drops asset-server-backed atlases and tilemaps, and failure records, whose `(path,
    /// is_atlas)` is not in `wanted`, so streamed-out levels free their tilesets. Registered
    /// in-memory content (no asset id) stays until removed explicitly. An entry that is wanted
    /// again reloads from the server.
    pub fn release_unwanted(&mut self, wanted: &std::collections::BTreeSet<(&str, bool)>) {
        let before = self.atlases.len() + self.tilemaps.len();
        self.atlases
            .retain(|p, (_, o)| o.id.is_none() || wanted.contains(&(p.as_str(), true)));
        self.tilemaps
            .retain(|p, (_, o)| o.id.is_none() || wanted.contains(&(p.as_str(), false)));
        self.failed
            .retain(|p, _| wanted.contains(&(p.as_str(), true)) || wanted.contains(&(p.as_str(), false)));
        self.failed_generation
            .retain(|(p, atlas), _| wanted.contains(&(p.as_str(), *atlas)));
        if self.atlases.len() + self.tilemaps.len() != before {
            self.revision += 1;
        }
    }

    /// True when `(path, is_atlas)` already failed at `generation` (not retried until it moves).
    #[must_use]
    pub fn failed_at(&self, path: &str, is_atlas: bool, generation: u64) -> bool {
        self.failed_generation
            .get(&(path.to_owned(), is_atlas))
            .is_some_and(|g| *g == generation)
    }

    /// Records that `(path, is_atlas)` failed at `generation` with `why`.
    pub fn record_failure(&mut self, path: &str, is_atlas: bool, generation: u64, why: String) {
        self.failed_generation
            .insert((path.to_owned(), is_atlas), generation);
        self.failed.insert(path.to_owned(), (generation, why));
    }

    /// The atlas at `path`.
    #[must_use]
    pub fn atlas(&self, path: &str) -> Option<&Arc<LoadedAtlas>> {
        self.atlases.get(path).map(|(a, _)| a)
    }

    /// The tilemap at `path`.
    #[must_use]
    pub fn tilemap(&self, path: &str) -> Option<&Arc<LoadedTileMap>> {
        self.tilemaps.get(path).map(|(m, _)| m)
    }

    /// Edits the tilemap at `path` (copy-on-write: the tileset images stay shared; the renderer
    /// sees the change next frame).
    pub fn tilemap_mut(&mut self, path: &str) -> Option<&mut crate::tilemap::TileMapAsset> {
        let (map, _) = self.tilemaps.get_mut(path)?;
        self.revision += 1;
        if Arc::get_mut(map).is_none() {
            let map_clone = map.map.clone();
            let images = map.images.clone();
            let anim = Mutex::new(anim_slots(&map_clone));
            *map = Arc::new(LoadedTileMap {
                map: map_clone,
                images,
                anim,
            });
        }
        Arc::get_mut(map).map(|loaded| &mut loaded.map)
    }
}

/// How the 2D pass views the world. `regia_framework::camera::ortho` writes it from the active
/// `Camera2d`; without one the default shows world units as pixels around the origin.
#[derive(Resource, Clone, Debug, PartialEq)]
pub struct View2d {
    /// The camera.
    pub camera: OrthoCamera,
    /// Draw at a virtual resolution and scale by an integer factor with letterbox bars.
    pub pixel_perfect: Option<PixelPerfect>,
    /// Snap the camera centre to whole (virtual) pixels.
    pub snap: bool,
    /// Clear the 2D target to this colour first (`None` draws over the 3D scene / UI clear).
    pub clear: Option<[f32; 4]>,
}

impl Default for View2d {
    fn default() -> Self {
        Self {
            camera: OrthoCamera::default(),
            pixel_perfect: None,
            snap: true,
            clear: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn dot() -> SpriteAtlas {
        SpriteAtlas::single(
            "dot",
            RgbaImage {
                width: 1,
                height: 1,
                rgba8: vec![255; 4],
            },
        )
    }

    fn insert_served(assets: &mut SpriteAssets, name: &str) {
        assets.insert_atlas(name, dot());
        if let Some((_, origin)) = assets.atlases.get_mut(name) {
            origin.id = Some(regia_asset::AssetId::new_random());
        }
    }

    #[test]
    fn release_unwanted_drops_served_entries_and_keeps_registered_ones() {
        let mut assets = SpriteAssets::default();
        assets.insert_atlas("memory", dot());
        insert_served(&mut assets, "kept");
        insert_served(&mut assets, "gone");
        assets.failed.insert("gone".into(), (1, "x".into()));
        assets.failed.insert("kept".into(), (1, "x".into()));
        let rev = assets.revision;
        let wanted: BTreeSet<(&str, bool)> = [("kept", true)].into();
        assets.release_unwanted(&wanted);
        assert!(assets.atlas("memory").is_some(), "registered content stays");
        assert!(assets.atlas("kept").is_some());
        assert!(assets.atlas("gone").is_none());
        assert!(!assets.failed.contains_key("gone"));
        assert!(assets.failed.contains_key("kept"));
        assert!(assets.revision > rev);
        let rev = assets.revision;
        assets.release_unwanted(&wanted);
        assert_eq!(assets.revision, rev, "nothing left to release");
    }

    #[test]
    fn remove_atlas_reports_whether_it_existed() {
        let mut assets = SpriteAssets::default();
        assets.insert_atlas("a", dot());
        assert!(assets.remove_atlas("a"));
        assert!(!assets.remove_atlas("a"));
        assert!(!assets.remove_tilemap("a"));
    }

    fn cooked(width: u32) -> CookedTileMap {
        CookedTileMap {
            version: 1,
            map: crate::tilemap::TileMapAsset {
                version: 1,
                orientation: Default::default(),
                width,
                height: 1,
                tile_width: 1,
                tile_height: 1,
                background: None,
                tilesets: Vec::new(),
                layers: Vec::new(),
                properties: Default::default(),
            },
            images: vec![RgbaImage {
                width: 1,
                height: 1,
                rgba8: vec![1, 2, 3, 4],
            }],
        }
    }

    #[test]
    fn tilemap_mut_copies_on_write_and_remove_tilemap_reports_presence() {
        let mut assets = SpriteAssets::default();
        assert!(assets.tilemap_mut("level").is_none());
        assert_eq!(assets.revision, 0);

        assets.insert_tilemap("level", cooked(2));
        let rev = assets.revision;
        assert_eq!(assets.tilemap("level").expect("inserted").map.width, 2);
        {
            let map = assets.tilemap_mut("level").expect("unique");
            map.width = 5;
        }
        assert_eq!(assets.revision, rev + 1);
        assert_eq!(assets.tilemap("level").expect("edited").map.width, 5);
        assert_eq!(assets.tilemap("level").expect("edited").map.height, 1);

        let shared = Arc::clone(assets.tilemap("level").expect("shared"));
        let image = Arc::clone(&shared.images[0]);
        let rev = assets.revision;
        {
            let map = assets.tilemap_mut("level").expect("copied");
            map.width = 7;
            map.background = Some("red".to_string());
        }
        assert_eq!(assets.revision, rev + 1);
        assert_eq!(shared.map.width, 5);
        assert_eq!(shared.map.background, None);
        let stored = assets.tilemap("level").expect("stored");
        assert_eq!(stored.map.width, 7);
        assert_eq!(stored.map.background.as_deref(), Some("red"));
        assert!(!Arc::ptr_eq(&shared, stored));
        assert!(Arc::ptr_eq(&image, &stored.images[0]));

        let rev = assets.revision;
        assert!(assets.remove_tilemap("level"));
        assert_eq!(assets.revision, rev + 1);
        assert!(!assets.remove_tilemap("level"));
        assert_eq!(assets.revision, rev + 1);
        assert!(assets.tilemap("level").is_none());
    }

    #[test]
    fn release_unwanted_distinguishes_atlases_from_tilemaps() {
        let mut assets = SpriteAssets::default();
        assets.insert_tilemap("memory", cooked(1));
        assets.insert_tilemap("served-kept", cooked(1));
        assets.insert_tilemap("served-gone", cooked(1));
        assets.insert_atlas("served-atlas", dot());
        for name in ["served-kept", "served-gone"] {
            assets.tilemaps.get_mut(name).expect(name).1.id = Some(regia_asset::AssetId::NIL);
        }
        assets.atlases.get_mut("served-atlas").expect("atlas").1.id = Some(regia_asset::AssetId::NIL);
        assets.failed.insert("map-only".to_string(), (4, "e".to_string()));
        assets.failed.insert("stale".to_string(), (4, "e".to_string()));

        // A tilemap is kept only by `(path, false)` and an atlas only by `(path, true)`.
        let wanted = BTreeSet::from([
            ("served-kept", false),
            ("served-gone", true),
            ("served-atlas", false),
            ("map-only", false),
        ]);
        let rev = assets.revision;
        assets.release_unwanted(&wanted);
        assert_eq!(assets.revision, rev + 1);
        assert!(assets.tilemap("memory").is_some());
        assert!(assets.tilemap("served-kept").is_some());
        assert!(assets.tilemap("served-gone").is_none());
        assert!(assets.atlas("served-atlas").is_none());
        assert!(assets.failed.contains_key("map-only"));
        assert!(!assets.failed.contains_key("stale"));

        let rev = assets.revision;
        assets
            .failed
            .insert("only-failed".to_string(), (1, "e".to_string()));
        assets.release_unwanted(&wanted);
        assert_eq!(assets.revision, rev);
        assert!(!assets.failed.contains_key("only-failed"));
        assert!(assets.failed.contains_key("map-only"));
    }

    #[test]
    fn a_failed_tilemap_does_not_block_the_atlas_of_the_same_path() {
        let mut assets = SpriteAssets::default();
        assets.record_failure("hero.atlas", false, 3, "wrong type".into());
        assert!(assets.failed_at("hero.atlas", false, 3));
        assert!(!assets.failed_at("hero.atlas", true, 3));
        assert!(!assets.failed_at("hero.atlas", false, 4));
    }
}
