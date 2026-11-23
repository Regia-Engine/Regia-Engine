// SPDX-License-Identifier: MPL-2.0
//! The 2D toolkit (`docs/specs/22-tier2.md` § T2.10): sprites from packed atlases (pivot, trim,
//! 9-slice), sprite-sheet animation, Tiled tilemaps, the orthographic pixel-perfect camera math and
//! an instanced sprite pass.
//!
//! - [`atlas`]: the `.atlas.ron` source format (frozen v1) and the cooked [`atlas::SpriteAtlas`]
//!   (`regia_asset_pipeline` packs it with `texture_packer`).
//! - [`tilemap`]: the `.tilemap.ron` format (frozen, v2 since ADR-2597; what the Tiled importer produces), tile
//!   queries, flips, animated tiles and the collision grid the platformer reads.
//! - [`sprite`]: the [`Sprite`] and [`TileMap`] components, the [`SpriteAssets`] store and the
//!   [`View2d`] settings.
//! - [`autotile`]: the `.autotile.ron` rules (frozen, ADR-1889; v2 since ADR-2597) and the terrain
//!   painter (Edge16, Blob47, Wang corner and 3×3 pattern rules; `docs/specs/86-2d-toolkit.md`).
//! - [`animation`]: [`AnimatedSprite`] (port of macroquad's sheet animation).
//! - [`camera`]: [`camera::Camera2dView`] conversions (port of macroquad's `Camera2D`),
//!   [`camera::OrthoCamera`] and the [`camera::PixelPerfect`] integer-scale letterbox.
//!   `regia_framework::camera::ortho` drives it from an ECS camera component.
//! - [`batch`] + [`render`]: quads sorted back to front, one instanced draw per texture, drawn as a
//!   `regia_render::WindowOverlay` layer before the UI.
//!
//! The world is +X right, +Y up; one world unit is one source pixel at scale 1 and zoom 1. A
//! sprite's world pose (`GlobalTransform`, after `TransformPropagate`) z orders it (higher in front).
//!
//! Books: `book/user-guide/2d.md` (workflow) and `book/agents/2d.md` (agent methods).

pub mod animation;
pub mod atlas;
pub mod autotile;
pub mod batch;
pub mod camera;
pub mod grid;
mod plugin;
pub mod render;
pub mod sprite;
pub mod stamp;
pub mod tilemap;
pub mod tilepatterns;

pub use animation::{AnimatedSprite, Animation, AnimationFrame, animate_sprites};
pub use atlas::{
    ATLAS_FORMAT_VERSION, AtlasSource, NineSlice, RgbaImage, SpriteAtlas, atlas_migration_sources,
    atlas_schema_fingerprint,
};
pub use autotile::{
    AUTOTILE_FORMAT_VERSION, AUTOTILE_TYPE_PATH, AutoTileSet, AutoTiles, COOKED_AUTOTILE_VERSION,
    CookedAutoTileSet, autotile_migration_sources, autotile_schema_fingerprint,
};
pub use camera::{Camera2dView, Letterbox, OrthoCamera, PixelPerfect, Rect2};
pub use plugin::{
    Frame2dStats, SPRITE_OVERLAY_ORDER, Sprite2dPlugin, SpritePass, SpritePassState, decode_atlas,
    decode_tilemap, extract_2d_frame, load_2d_assets, sprite_overlay,
};
pub use sprite::{Sprite, SpriteAssets, SpriteSource, TileMap, View2d};
pub use tilemap::{
    CookedTileMap, TILEMAP_FORMAT_VERSION, TileCollision, TileMapAsset, tilemap_migration_sources,
    tilemap_schema_fingerprint,
};
