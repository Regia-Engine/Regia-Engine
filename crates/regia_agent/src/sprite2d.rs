// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for the 2D toolkit (`specs/22` § T2.10, `book/agents/2d.md`):
//! `sprite.list` / `sprite.get` (sprites, their atlas region and animation state),
//! `tilemap.list` / `tilemap.get_tile` (maps, layers, and the tile at a cell or world point with
//! its flips, collision and properties) and `view2d.get` (the 2D camera and last frame's counts).
//!
//! Read-only. Without `regia_2d::Sprite2dPlugin` the lists are empty and `view2d.get` says so.

use regia_2d::animation::AnimatedSprite;
use regia_2d::sprite::{SpriteAssets, SpriteSource};
use regia_2d::tilemap::{MapLayer, PropertyValue, TileCollision};
use regia_2d::{Sprite, SpritePass, TileMap, View2d};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::GlobalTransform;
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn entity_param(params: &Value) -> Result<Entity, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (an entity id) is required"))?;
    Entity::try_from_bits(bits).ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
}

fn source_json(source: &SpriteSource) -> Value {
    match source {
        SpriteSource::Named(name) => json!({ "name": name }),
        SpriteSource::Rect { rect } => json!({ "rect": rect }),
    }
}

fn sprite_json(world: &World, entity: Entity, sprite: &Sprite) -> Value {
    let position = world
        .get::<GlobalTransform>(entity)
        .map_or(Vec3::ZERO, GlobalTransform::translation);
    let assets = world.get_resource::<SpriteAssets>();
    let atlas = assets.and_then(|a| a.atlas(&sprite.atlas));
    let region = atlas.and_then(|a| match &sprite.source {
        SpriteSource::Named(name) => a.atlas.sprite(name).map(|s| {
            json!({
                "page": s.page,
                "rect": s.rect,
                "source_size": s.source_size,
                "pivot": s.pivot,
                "nine_slice": s.nine_slice.map(|n| [n.left, n.top, n.right, n.bottom]),
            })
        }),
        SpriteSource::Rect { rect } => Some(json!({
            "page": 0,
            "rect": rect,
            "source_size": null,
            "pivot": null,
            "nine_slice": null,
        })),
    });
    let animation = world.get::<AnimatedSprite>(entity).map(|a| {
        json!({
            "animation": a.animation().map(|x| x.name.clone()),
            "index": a.current_animation(),
            "frame": a.frame_index(),
            "playing": a.playing,
            "finished": a.finished(),
            "animations": a.animations.iter().map(|x| x.name.clone()).collect::<Vec<_>>(),
        })
    });
    json!({
        "entity": entity.to_bits(),
        "atlas": sprite.atlas,
        "loaded": atlas.is_some(),
        "source": source_json(&sprite.source),
        "region": region,
        "position": [position.x, position.y, position.z],
        "size": sprite.size,
        "color": sprite.color,
        "flip": [sprite.flip_x, sprite.flip_y],
        "visible": sprite.visible,
        "animation": animation,
    })
}

/// `sprite.list`: every sprite (entity order), up to `limit` (default 500).
pub fn list_sprites(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let limit = crate::params::opt_u64(params, "limit")?.unwrap_or(500) as usize;
    let mut query = world.query::<(Entity, &Sprite)>();
    let mut entries: Vec<(Entity, Sprite)> = query.iter(world).map(|(e, s)| (e, s.clone())).collect();
    entries.sort_by_key(|(e, _)| *e);
    let total = entries.len();
    let sprites: Vec<Value> = entries
        .iter()
        .take(limit)
        .map(|(e, s)| sprite_json(world, *e, s))
        .collect();
    Ok(json!({ "total": total, "sprites": sprites }))
}

/// `sprite.get`: one sprite by entity.
pub fn get_sprite(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params)?;
    let Some(sprite) = world.get::<Sprite>(entity).cloned() else {
        return Err(RpcError::not_found(
            format!("entity {} has no Sprite", entity.to_bits()),
            "list sprites with sprite.list",
        ));
    };
    Ok(sprite_json(world, entity, &sprite))
}

/// `tilemap.list`: every tilemap entity with its size, tile size and layers.
pub fn list_tilemaps(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut query = world.query::<(Entity, &TileMap)>();
    let mut entries: Vec<(Entity, TileMap)> = query.iter(world).map(|(e, m)| (e, m.clone())).collect();
    entries.sort_by_key(|(e, _)| *e);
    let assets = world.get_resource::<SpriteAssets>();
    let maps: Vec<Value> = entries
        .iter()
        .map(|(entity, component)| {
            let position = world
                .get::<GlobalTransform>(*entity)
                .map_or(Vec3::ZERO, GlobalTransform::translation);
            let loaded = assets.and_then(|a| a.tilemap(&component.map));
            let detail = loaded.map(|m| {
                let map = &m.map;
                json!({
                    "size": [map.width, map.height],
                    "tile_size": [map.tile_width, map.tile_height],
                    "orientation": map.orientation.name(),
                    "tilesets": map.tilesets.iter().map(|t| json!({
                        "name": t.name, "first_gid": t.first_gid, "tile_count": t.tile_count,
                    })).collect::<Vec<_>>(),
                    "layers": map.layers.iter().enumerate().map(|(i, l)| match l {
                        MapLayer::Tiles(t) => json!({
                            "index": i, "name": t.name, "kind": "tiles", "visible": t.visible,
                            "opacity": t.opacity,
                            "tiles": t.data.iter().filter(|g| **g != 0).count(),
                        }),
                        MapLayer::Objects(o) => json!({
                            "index": i, "name": o.name, "kind": "objects", "visible": o.visible,
                            "objects": o.objects.iter().map(|ob| json!({
                                "id": ob.id, "name": ob.name, "class": ob.class, "position": ob.position,
                                "size": ob.size,
                            })).collect::<Vec<_>>(),
                        }),
                    }).collect::<Vec<_>>(),
                })
            });
            json!({
                "entity": entity.to_bits(),
                "map": component.map,
                "loaded": loaded.is_some(),
                "visible": component.visible,
                "position": [position.x, position.y, position.z],
                "detail": detail,
            })
        })
        .collect();
    Ok(json!({ "tilemaps": maps }))
}

fn property_json(value: &PropertyValue) -> Value {
    match value {
        PropertyValue::Bool(b) => json!(b),
        PropertyValue::Int(i) => json!(i),
        PropertyValue::Float(f) => json!(f),
        PropertyValue::String(s) | PropertyValue::Color(s) | PropertyValue::File(s) => json!(s),
        PropertyValue::Object(id) => json!({ "object": id }),
    }
}

/// `tilemap.get_tile`: the tile at `x`, `y` (row 0 = top) or under `world` `[x, y]`, in `layer`
/// (name or index; default: the topmost tile layer with a tile there).
///
/// A hit returns `cell`, `layer`, `tile`, `map_rect`, `orientation` and `outline`. An empty
/// in-range cell, and an `x`/`y` past the map, return `{ cell, tile: null }`. A world point
/// outside the map returns `{ tile: null, reason: "outside the map" }`.
pub fn get_tile(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // Which map: an explicit entity, else the only/first tilemap.
    let entity = match params.get("entity") {
        Some(_) => entity_param(params)?,
        None => {
            let mut query = world.query_filtered::<Entity, With<TileMap>>();
            let mut all: Vec<Entity> = query.iter(world).collect();
            all.sort();
            *all.first().ok_or_else(|| {
                RpcError::not_found("no TileMap in this world", "spawn a regia_2d::TileMap entity")
            })?
        }
    };
    let Some(component) = world.get::<TileMap>(entity).cloned() else {
        return Err(RpcError::not_found(
            format!("entity {} has no TileMap", entity.to_bits()),
            "list maps with tilemap.list",
        ));
    };
    let Some(loaded) = world
        .get_resource::<SpriteAssets>()
        .and_then(|a| a.tilemap(&component.map))
        .cloned()
    else {
        return Err(RpcError::not_found(
            format!("tilemap `{}` is not loaded", component.map),
            "check the path is cooked (assets.*) and Sprite2dPlugin is added",
        ));
    };
    let map = &loaded.map;
    let (x, y) = if let Some(point) = crate::params::opt_array(params, "world")? {
        let coord = |i: usize| point.get(i).and_then(Value::as_f64).map(|v| v as f32);
        let (Some(wx), Some(wy)) = (coord(0), coord(1)) else {
            return Err(RpcError::invalid_params("`world` must be [x, y]"));
        };
        // Into map space (the map entity's transform; its origin is the bottom-left corner).
        let local = world
            .get::<GlobalTransform>(entity)
            .map(|g| g.0.inverse().transform_point3(Vec3::new(wx, wy, 0.0)))
            .unwrap_or(Vec3::new(wx, wy, 0.0));
        match map.cell_at_world([local.x, local.y]) {
            Some(cell) => cell,
            None => return Ok(json!({ "tile": null, "reason": "outside the map" })),
        }
    } else {
        // Saturate (outside every map) rather than wrap a huge cell onto a real one.
        let cell = |k: &str| {
            params
                .get(k)
                .and_then(Value::as_u64)
                .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
        };
        match (cell("x"), cell("y")) {
            (Some(x), Some(y)) => (x, y),
            _ => {
                return Err(RpcError::invalid_params(
                    "give `x` and `y` (cell, row 0 = top) or `world` [x, y]",
                ));
            }
        }
    };
    let layers: Vec<usize> = match params.get("layer") {
        Some(Value::String(name)) => vec![
            map.layer_index(name)
                .ok_or_else(|| RpcError::invalid_params(format!("no layer named `{name}`")))?,
        ],
        Some(v) => vec![
            v.as_u64()
                .ok_or_else(|| RpcError::invalid_params("`layer` is a name or an index"))?
                as usize,
        ],
        None => (0..map.layers.len()).rev().collect(),
    };
    let found = layers.iter().find_map(|&l| map.tile_at(l, x, y).map(|t| (l, t)));
    let Some((layer, tile)) = found else {
        return Ok(json!({ "cell": [x, y], "tile": null }));
    };
    let def = tile.def();
    let collision = match tile.collision() {
        TileCollision::Empty => "empty",
        TileCollision::Solid => "solid",
        TileCollision::OneWay => "one_way",
    };
    let (min, max) = map.tile_world_rect(x, y);
    Ok(json!({
        "cell": [x, y],
        "layer": { "index": layer, "name": map.layers[layer].name() },
        "tile": {
            "gid": tile.gid,
            "tileset": tile.set.name,
            "local_id": tile.local_id,
            "flip": { "x": tile.flags.flip_x, "y": tile.flags.flip_y, "diagonal": tile.flags.diagonal },
            "collision": collision,
            "class": def.map_or_else(String::new, |d| d.class.clone()),
            "animated": def.is_some_and(|d| !d.animation.is_empty()),
            "properties": def.map_or_else(serde_json::Map::new, |d| d.properties.iter().map(|(k, v)| (k.clone(), property_json(v))).collect::<serde_json::Map<_, _>>()),
        },
        "map_rect": { "min": min, "max": max },
        "orientation": map.orientation.name(),
        "outline": map.tile_world_outline(x, y),
    }))
}

/// `view2d.get`: the 2D camera, pixel-perfect mode and the last frame's counts.
pub fn get_view(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(view) = world.get_resource::<View2d>() else {
        return Err(RpcError::not_found(
            "no 2D view in this app",
            "add regia_2d::Sprite2dPlugin (and regia_framework::camera::ortho::Camera2dPlugin)",
        ));
    };
    let stats = world
        .get_resource::<SpritePass>()
        .map(SpritePass::stats)
        .unwrap_or_default();
    Ok(json!({
        "camera": {
            "center": [view.camera.center.x, view.camera.center.y],
            "pixels_per_unit": view.camera.pixels_per_unit,
            "rotation": view.camera.rotation,
        },
        "pixel_perfect": view.pixel_perfect.map(|p| json!({
            "virtual_size": [p.virtual_size.x, p.virtual_size.y],
            "bar_color": p.bar_color,
        })),
        "snap": view.snap,
        "clear": view.clear,
        "last_frame": {
            "sprites": stats.sprites, "tiles": stats.tiles,
            "instances": stats.instances, "batches": stats.batches,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_2d::atlas::{RgbaImage, SpriteAtlas};
    use regia_2d::tilemap::{
        CookedTileMap, FLIP_X_BIT, Orientation, TileDef, TileLayer, TileMapAsset, TilesetDef,
    };
    use regia_scene::Transform;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, regia_2d::Sprite2dPlugin));
        let mut assets = app.world_mut().resource_mut::<SpriteAssets>();
        assets.insert_atlas(
            "a",
            SpriteAtlas::single(
                "hero",
                RgbaImage {
                    width: 2,
                    height: 2,
                    rgba8: vec![255; 16],
                },
            ),
        );
        let map = TileMapAsset {
            version: 1,
            orientation: Orientation::Orthogonal,
            width: 2,
            height: 2,
            tile_width: 8,
            tile_height: 8,
            background: None,
            tilesets: vec![TilesetDef {
                name: "t".into(),
                first_gid: 1,
                image: "t.png".into(),
                image_width: 16,
                image_height: 8,
                tile_width: 8,
                tile_height: 8,
                columns: 2,
                tile_count: 2,
                spacing: 0,
                margin: 0,
                tiles: vec![TileDef {
                    id: 1,
                    collision: TileCollision::Solid,
                    properties: [("kind".to_string(), PropertyValue::String("rock".into()))].into(),
                    ..TileDef::default()
                }],
            }],
            layers: vec![MapLayer::Tiles(TileLayer {
                name: "ground".into(),
                visible: true,
                opacity: 1.0,
                offset: [0.0, 0.0],
                data: vec![0, 0, 1, 2 | FLIP_X_BIT],
                properties: Default::default(),
            })],
            properties: Default::default(),
        };
        assets.insert_tilemap(
            "m",
            CookedTileMap {
                version: 1,
                map,
                images: vec![RgbaImage {
                    width: 16,
                    height: 8,
                    rgba8: vec![0; 16 * 8 * 4],
                }],
            },
        );
        app
    }

    #[test]
    fn sprites_and_tiles_are_queryable() {
        let mut app = app();
        let hero = app
            .world_mut()
            .spawn((
                Sprite::named("a", "hero"),
                Transform::from_translation(Vec3::new(3.0, 4.0, 1.0)),
            ))
            .id();
        let map = app
            .world_mut()
            .spawn((
                TileMap::new("m"),
                Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)),
            ))
            .id();
        app.update();
        let w = app.world_mut();

        let list = list_sprites(w, &json!({})).expect("lists");
        assert_eq!(list["total"], 1);
        assert_eq!(list["sprites"][0]["position"], json!([3.0, 4.0, 1.0]));
        assert_eq!(list["sprites"][0]["region"]["rect"], json!([0, 0, 2, 2]));
        let one = get_sprite(w, &json!({ "entity": hero.to_bits() })).expect("gets");
        assert_eq!(one["loaded"], true);
        assert!(get_sprite(w, &json!({ "entity": map.to_bits() })).is_err());

        let maps = list_tilemaps(w, &json!({})).expect("lists");
        assert_eq!(maps["tilemaps"][0]["detail"]["size"], json!([2, 2]));
        assert_eq!(maps["tilemaps"][0]["detail"]["layers"][0]["tiles"], 2);

        // Cell (1, 1) = bottom-right: GID 2, x-flipped, solid with a property.
        let tile = get_tile(w, &json!({ "x": 1, "y": 1 })).expect("tile");
        assert_eq!(tile["tile"]["local_id"], 1);
        assert_eq!(tile["tile"]["flip"]["x"], true);
        assert_eq!(tile["tile"]["collision"], "solid");
        assert_eq!(tile["tile"]["properties"]["kind"], "rock");
        // The same tile through a world point (the map sits at x = 100).
        let by_world = get_tile(
            w,
            &json!({ "entity": map.to_bits(), "world": [112.0, 3.0], "layer": "ground" }),
        )
        .expect("tile");
        assert_eq!(by_world["cell"], json!([1, 1]));
        let empty = get_tile(w, &json!({ "x": 0, "y": 0 })).expect("empty");
        assert_eq!(empty["cell"], json!([0, 0]));
        assert_eq!(empty["tile"], Value::Null);
        assert!(empty.get("layer").is_none(), "{empty}");
        assert!(empty.get("map_rect").is_none(), "{empty}");
        assert!(empty.get("outline").is_none(), "{empty}");
        assert!(empty.get("orientation").is_none(), "{empty}");
        let outside = get_tile(w, &json!({ "world": [-1000.0, -1000.0] })).expect("outside");
        assert_eq!(outside["tile"], Value::Null);
        assert_eq!(outside["reason"], "outside the map");
        assert!(outside.get("cell").is_none(), "{outside}");
        assert!(get_tile(w, &json!({ "x": 0, "y": 0, "layer": "nope" })).is_err());

        let view = get_view(w, &json!({})).expect("view");
        assert_eq!(view["last_frame"]["sprites"], 1);
        assert_eq!(view["last_frame"]["tiles"], 2);
    }

    #[test]
    fn a_plain_tile_reports_empty_class_and_properties() {
        let mut app = app();
        app.world_mut().spawn(TileMap::new("m"));
        app.update();
        // Cell (0, 1) holds GID 1, which has no `TileDef`.
        let tile = get_tile(app.world_mut(), &json!({ "x": 0, "y": 1 })).expect("tile");
        assert_eq!(tile["tile"]["gid"], 1);
        assert_eq!(tile["tile"]["class"], "");
        assert_eq!(tile["tile"]["properties"], json!({}));
    }

    #[test]
    fn a_rect_sprite_region_has_every_documented_key() {
        let mut app = app();
        let sprite = app
            .world_mut()
            .spawn((Sprite::rect("a", [0, 0, 2, 2]), Transform::default()))
            .id();
        app.update();
        let one = get_sprite(app.world_mut(), &json!({ "entity": sprite.to_bits() })).expect("gets");
        let region = &one["region"];
        assert_eq!(region["page"], 0);
        assert_eq!(region["rect"], json!([0, 0, 2, 2]));
        for key in ["source_size", "pivot", "nine_slice"] {
            assert!(region.get(key).is_some_and(Value::is_null), "{key}");
        }
    }

    #[test]
    fn without_the_plugin_lists_are_empty_and_view_is_not_found() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let w = app.world_mut();
        assert_eq!(list_sprites(w, &json!({})).expect("ok")["total"], 0);
        assert!(get_view(w, &json!({})).is_err());
        assert!(get_tile(w, &json!({ "x": 0, "y": 0 })).is_err());
    }
}
