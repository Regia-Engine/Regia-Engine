// SPDX-License-Identifier: MPL-2.0
//! Tests of the tilemap write methods (`docs/specs/86-2d-toolkit.md` § 8 item 8): each method's
//! success path and each `tile_*` error code, on a `World` with a loaded map.
#![cfg(test)]

use super::*;
use regia_2d::atlas::RgbaImage;
use regia_2d::tilemap::{CookedTileMap, ObjectLayer, Orientation, TileLayer, TilesetDef};

const RULES: &str = r#"(
    version: 1,
    tileset: "t",
    border: Empty,
    terrains: [
        (name: "wall", layout: Edge16([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15])),
    ],
)"#;

/// A 4×3 map "m" (tileset "t", 16 tiles, GIDs 1..=16) with a tile layer and an object layer.
fn app() -> App {
    app_sized(4, 3)
}

/// The same map at `width`×`height`.
fn app_sized(width: u32, height: u32) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, regia_2d::Sprite2dPlugin));
    let map = TileMapAsset {
        version: 1,
        orientation: Orientation::Orthogonal,
        width,
        height,
        tile_width: 8,
        tile_height: 8,
        background: None,
        tilesets: vec![TilesetDef {
            name: "t".into(),
            first_gid: 1,
            image: "t.png".into(),
            image_width: 32,
            image_height: 32,
            tile_width: 8,
            tile_height: 8,
            columns: 4,
            tile_count: 16,
            spacing: 0,
            margin: 0,
            tiles: Vec::new(),
        }],
        layers: vec![
            MapLayer::Tiles(TileLayer {
                name: "ground".into(),
                visible: true,
                opacity: 1.0,
                offset: [0.0, 0.0],
                data: vec![0; (width * height) as usize],
                properties: Default::default(),
            }),
            MapLayer::Objects(ObjectLayer {
                name: "spawns".into(),
                visible: true,
                objects: Vec::new(),
                properties: Default::default(),
            }),
        ],
        properties: Default::default(),
    };
    app.world_mut().resource_mut::<SpriteAssets>().insert_tilemap(
        "m",
        CookedTileMap {
            version: 1,
            map,
            images: vec![RgbaImage {
                width: 32,
                height: 32,
                rgba8: vec![0; 32 * 32 * 4],
            }],
        },
    );
    app.world_mut().spawn(TileMap::new("m"));
    app.update();
    app
}

fn code(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|d| d["code"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
fn stamp_paste_rejects_coordinates_that_overflow_i64() {
    let mut app = app();
    let world = app.world_mut();
    let revision = world.resource::<SpriteAssets>().revision;
    for (x, y, width, height, ops) in [
        (i64::MAX, 0, 2, 1, vec![]),
        (0, i64::MAX, 1, 2, vec![]),
        (0, i64::MAX, 2, 1, vec!["rotate_cw"]),
        (i64::MAX, 0, 1, 2, vec!["rotate_ccw"]),
    ] {
        let err = stamp_paste(
            world,
            &json!({
                "layer": "ground", "x": x, "y": y, "ops": ops,
                "stamp": { "width": width, "height": height, "cells": [1, 2] },
            }),
        )
        .expect_err("overflowing origin");
        assert_eq!(code(&err), "tile_bad_params");
        assert_eq!(world.resource::<SpriteAssets>().revision, revision);
        assert_eq!(layer(world), vec![0; 12]);
    }
    for (x, y) in [(i64::MAX - 1, 0), (0, i64::MAX), (i64::MIN, i64::MIN)] {
        let out = stamp_paste(
            world,
            &json!({
                "layer": "ground", "x": x, "y": y,
                "stamp": { "width": 2, "height": 1, "cells": [1, 2] },
            }),
        )
        .expect("representable coordinates are clipped");
        assert_eq!(out["count"], 0);
    }
}

fn layer(world: &World) -> Vec<u32> {
    world
        .resource::<SpriteAssets>()
        .tilemap("m")
        .and_then(|m| m.map.tile_layer(0).map(|t| t.data.clone()))
        .unwrap_or_default()
}

#[test]
fn set_tile_writes_flags_and_reports_before_after() {
    let mut app = app();
    let w = app.world_mut();
    let rev = w.resource::<SpriteAssets>().revision;
    let out = set_tile(
        w,
        &json!({ "layer": "ground", "x": 1, "y": 2, "gid": 5, "flip": { "x": true } }),
    )
    .expect("writes");
    assert_eq!(out["before"], 0);
    assert_eq!(out["after"], 5 | FLIP_X_BIT);
    assert_eq!(layer(w)[2 * 4 + 1], 5 | FLIP_X_BIT);
    assert!(w.resource::<SpriteAssets>().revision > rev);
    let erased = set_tile(w, &json!({ "layer": 0, "x": 1, "y": 2, "gid": 0 })).expect("erases");
    assert_eq!(erased["before"], 5 | FLIP_X_BIT);
    assert_eq!(layer(w)[9], 0);
}

/// Spec 86: gid 0 erases. A flip on that erase must not leave the flag bits in the cell.
#[test]
fn set_tile_gid_zero_clears_flip_bits() {
    let mut app = app();
    let w = app.world_mut();
    set_tile(
        w,
        &json!({ "layer": "ground", "x": 0, "y": 0, "gid": 3, "flip": { "x": true, "y": true } }),
    )
    .expect("paints");
    let erased = set_tile(
        w,
        &json!({ "layer": "ground", "x": 0, "y": 0, "gid": 0, "flip": { "x": true, "diagonal": true } }),
    )
    .expect("erases");
    assert_eq!(erased["after"], 0);
    assert_eq!(layer(w)[0], 0);
}

#[test]
fn set_tile_errors_have_codes_and_leave_the_map_alone() {
    let mut app = app();
    let w = app.world_mut();
    let rev = w.resource::<SpriteAssets>().revision;
    for (params, expected) in [
        (
            json!({ "layer": "ground", "x": 9, "y": 0, "gid": 1 }),
            "tile_outside_map",
        ),
        (
            json!({ "layer": "ground", "x": 0, "y": 0, "gid": 99 }),
            "tile_bad_gid",
        ),
        (
            json!({ "layer": "ground", "x": 0, "y": 0, "gid": 2_147_483_649u64 }),
            "tile_bad_gid",
        ),
        (
            json!({ "layer": "spawns", "x": 0, "y": 0, "gid": 1 }),
            "tile_not_tile_layer",
        ),
        (
            json!({ "layer": "nope", "x": 0, "y": 0, "gid": 1 }),
            "tile_not_tile_layer",
        ),
        (
            json!({ "layer": 7, "x": 0, "y": 0, "gid": 1 }),
            "tile_not_tile_layer",
        ),
        (json!({ "x": 0, "y": 0, "gid": 1 }), "tile_bad_params"),
        (json!({ "layer": "ground", "gid": 1 }), "tile_bad_params"),
        (
            json!({ "layer": "ground", "x": 0, "y": 0, "gid": 1, "entity": "x" }),
            "tile_bad_params",
        ),
    ] {
        let err = set_tile(w, &params).expect_err("refused");
        assert_eq!(code(&err), expected, "{params}");
    }
    assert_eq!(w.resource::<SpriteAssets>().revision, rev);
    assert!(layer(w).iter().all(|&g| g == 0));
}

#[test]
fn register_list_paint_erase_and_autotile() {
    let mut app = app();
    let w = app.world_mut();
    let reg = autotile_register(w, &json!({ "name": "walls", "ron": RULES })).expect("registers");
    assert_eq!(reg["terrains"], 1);
    assert_eq!(reg["replaced"], false);
    let list = autotile_list(w, &json!({})).expect("lists");
    assert_eq!(list["rule_sets"][0]["name"], "walls");
    assert_eq!(list["rule_sets"][0]["border"], "empty");
    assert_eq!(list["rule_sets"][0]["terrains"][0]["layout"], "edge16");
    assert_eq!(
        list["rule_sets"][0]["terrains"][0]["members"]
            .as_array()
            .map(Vec::len),
        Some(16)
    );

    // A 3-wide row at y = 1: end caps E (2) and W (8), middle E | W (10). GID = local + 1.
    let out = paint(
        w,
        &json!({ "layer": "ground", "rules": "walls", "terrain": "wall", "rect": [0, 1, 2, 1] }),
    )
    .expect("paints");
    assert_eq!(out["count"], 3);
    assert_eq!(out["truncated"], false);
    assert_eq!(&layer(w)[4..8], &[3, 11, 9, 0]);
    assert_eq!(
        out["changed"][0],
        json!({ "cell": [0, 1], "before": 0, "after": 3 })
    );

    // A cell below the middle: the middle gains S (4).
    paint(
        w,
        &json!({ "layer": "ground", "rules": "walls", "terrain": "wall", "cells": [[1, 2]] }),
    )
    .expect("paints");
    assert_eq!(layer(w)[5], 15);
    assert_eq!(layer(w)[9], 2);

    // Erase it again: the middle goes back.
    let erased = paint(
        w,
        &json!({ "layer": "ground", "rules": "walls", "erase": true, "cells": [[1, 2]] }),
    )
    .expect("erases");
    assert_eq!(erased["count"], 2);
    assert_eq!(layer(w)[5], 11);

    // Hand-placed members re-resolve with tilemap.autotile.
    set_tile(w, &json!({ "layer": "ground", "x": 3, "y": 1, "gid": 1 })).expect("writes");
    let fixed = autotile(w, &json!({ "layer": "ground", "rules": "walls" })).expect("applies");
    assert_eq!(fixed["changed"], 2);
    assert_eq!(&layer(w)[4..8], &[3, 11, 11, 9]);
    assert_eq!(
        autotile(w, &json!({ "layer": "ground", "rules": "walls" })).expect("again")["changed"],
        0
    );

    let again = autotile_register(w, &json!({ "name": "walls", "ron": RULES })).expect("replaces");
    assert_eq!(again["replaced"], true);
}

#[test]
fn paint_errors_have_codes_and_leave_the_map_alone() {
    let mut app = app();
    let w = app.world_mut();
    autotile_register(w, &json!({ "name": "walls", "ron": RULES })).expect("registers");
    let other_set = RULES.replace("tileset: \"t\"", "tileset: \"missing\"");
    autotile_register(w, &json!({ "name": "elsewhere", "ron": other_set })).expect("registers");
    let rev = w.resource::<SpriteAssets>().revision;
    let base = json!({ "layer": "ground", "rules": "walls", "terrain": "wall" });
    let with = |extra: Value| {
        let mut p = base.clone();
        if let (Some(p), Some(extra)) = (p.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                p.insert(k.clone(), v.clone());
            }
        }
        p
    };
    for (params, expected) in [
        (with(json!({ "cells": [[0, 0], [4, 0]] })), "tile_outside_map"),
        (with(json!({ "rect": [0, 0, 300, 300] })), "tile_too_many_cells"),
        (
            with(json!({ "cells": [[0, 0]], "terrain": "lava" })),
            "tile_unknown_terrain",
        ),
        (
            with(json!({ "cells": [[0, 0]], "rules": "nope" })),
            "tile_unknown_rules",
        ),
        (
            with(json!({ "cells": [[0, 0]], "rules": "elsewhere" })),
            "tile_bind_failed",
        ),
        (
            with(json!({ "cells": [[0, 0]], "layer": "spawns" })),
            "tile_not_tile_layer",
        ),
        (
            with(json!({ "cells": [[0, 0]], "erase": true })),
            "tile_bad_params",
        ),
        (with(json!({ "cells": [[0]] })), "tile_bad_params"),
        (with(json!({ "rect": [0, 0, 1] })), "tile_bad_params"),
        (
            with(json!({ "cells": [[0, 0]], "rect": [0, 0, 1, 1] })),
            "tile_bad_params",
        ),
        (with(json!({})), "tile_bad_params"),
    ] {
        let err = paint(w, &params).expect_err("refused");
        assert_eq!(code(&err), expected, "{params}");
    }
    assert_eq!(w.resource::<SpriteAssets>().revision, rev);
    assert!(layer(w).iter().all(|&g| g == 0));
    assert_eq!(
        code(&autotile(w, &json!({ "layer": "ground" })).expect_err("no rules")),
        "tile_bad_params"
    );
    for (params, expected) in [
        (
            json!({ "name": "x", "ron": "(version: 9, tileset: \"t\", terrains: [])" }),
            "tile_bad_rules",
        ),
        (
            json!({ "name": "x", "ron": "(version: 1, tileset: \"t\", terrains: [])" }),
            "tile_bad_rules",
        ),
        (json!({ "name": "", "ron": RULES }), "tile_bad_params"),
        (json!({ "name": "x" }), "tile_bad_params"),
    ] {
        let err = autotile_register(w, &params).expect_err("refused");
        assert_eq!(code(&err), expected, "{params}");
    }
}

#[test]
fn without_the_plugin_lists_are_empty_and_writes_are_not_found() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    let _ = app.world_mut().register_component::<TileMap>();
    let w = app.world_mut();
    assert_eq!(autotile_list(w, &json!({})).expect("ok")["rule_sets"], json!([]));
    let err = paint(w, &json!({})).expect_err("no map");
    assert_eq!(err.code, NOT_FOUND);
    assert_eq!(code(&err), "F0003");
    assert!(autotile_register(w, &json!({ "name": "x", "ron": RULES })).is_err());
}

#[test]
fn a_rect_of_every_cell_is_refused_before_anything_is_allocated() {
    let err = paint_cells(&json!({ "rect": [0, 0, 4294967295u64, 4294967295u64] })).expect_err("too many");
    assert_eq!(code(&err), "tile_too_many_cells");
    let err = paint_cells(&json!({ "rect": [0, 0, 4294967295u64, 4294967294u64] })).expect_err("too many");
    assert_eq!(code(&err), "tile_too_many_cells");
    assert_eq!(
        paint_cells(&json!({ "rect": [0, 0, 1, 1] })).expect("four").len(),
        4
    );
}

#[test]
fn stamp_copy_and_paste_round_trip_with_rotation_and_clipping() {
    let mut app = app();
    let w = app.world_mut();
    for (x, gid) in [(0, 1), (1, 2), (2, 3)] {
        set_tile(w, &json!({ "layer": "ground", "x": x, "y": 0, "gid": gid })).expect("writes");
    }
    let stamp = stamp_copy(w, &json!({ "layer": "ground", "rect": [2, 0, 0, 1] })).expect("copies");
    assert_eq!(
        (stamp["width"].as_u64(), stamp["height"].as_u64()),
        (Some(3), Some(2))
    );
    assert_eq!(stamp["cells"], json!([1, 2, 3, 0, 0, 0]));
    // Paste it rotated a quarter turn at (3, 0): a 2x3 stamp whose right column hangs off the map.
    let out = stamp_paste(
        w,
        &json!({ "layer": "ground", "x": 3, "y": 0, "stamp": stamp, "op": "rotate_cw" }),
    )
    .expect("pastes");
    assert_eq!(out["stamp"], json!({ "width": 2, "height": 3 }));
    // Rotated clockwise [1 2 3 / - - -] is [- 1 / - 2 / - 3]: only the left column (all empty)
    // lands inside the map, and empty cells are skipped.
    assert_eq!(out["count"], 0);
    let out = stamp_paste(
        w,
        &json!({ "layer": "ground", "x": 2, "y": 0, "stamp": stamp, "op": "rotate_cw" }),
    )
    .expect("pastes");
    // Left column empty (skipped), right column 1, 2, 3 at x = 3, rows 0..3.
    assert_eq!(out["count"], 3);
    let cells = layer(w);
    assert_eq!(
        [cells[3] & GID_MASK, cells[7] & GID_MASK, cells[11] & GID_MASK],
        [1, 2, 3]
    );
    assert_ne!(
        cells[3] & !GID_MASK,
        0,
        "the tile's flip bits turned with the stamp"
    );
    // Empty stamp cells leave a tile alone by default and erase it with skip_empty: false.
    set_tile(w, &json!({ "layer": "ground", "x": 1, "y": 1, "gid": 9 })).expect("writes");
    let kept = stamp_paste(w, &json!({ "layer": "ground", "x": 0, "y": 0, "stamp": stamp })).expect("pastes");
    assert_eq!(kept["count"], 0);
    assert_eq!(layer(w)[4 + 1], 9);
    let erased = stamp_paste(
        w,
        &json!({ "layer": "ground", "x": 0, "y": 0, "stamp": stamp, "skip_empty": false }),
    )
    .expect("pastes");
    assert_eq!(erased["count"], 1);
    assert_eq!(layer(w)[4 + 1], 0);
}

#[test]
fn stamp_errors_have_codes_and_leave_the_map_alone() {
    let mut app = app();
    let w = app.world_mut();
    let rev = w.resource::<SpriteAssets>().revision;
    let good = json!({ "width": 1, "height": 1, "cells": [1] });
    let cases = [
        (stamp_copy(w, &json!({ "layer": "ground" })), "tile_bad_params"),
        (
            stamp_copy(w, &json!({ "layer": "ground", "rect": [0, 0, 9, 9] })),
            "tile_outside_map",
        ),
        (
            stamp_copy(w, &json!({ "layer": "spawns", "rect": [0, 0, 1, 1] })),
            "tile_not_tile_layer",
        ),
        (
            stamp_paste(
                w,
                &json!({ "layer": "ground", "x": 0, "y": 0, "stamp": { "width": 2, "height": 2, "cells": [1] } }),
            ),
            "tile_bad_stamp",
        ),
        (
            stamp_paste(
                w,
                &json!({ "layer": "ground", "x": 0, "y": 0, "stamp": { "width": 1, "height": 1, "cells": [999] } }),
            ),
            "tile_bad_gid",
        ),
        (
            stamp_paste(w, &json!({ "layer": "ground", "x": 0, "y": 0 })),
            "tile_bad_stamp",
        ),
        (
            stamp_paste(w, &json!({ "layer": "ground", "y": 0, "stamp": good })),
            "tile_bad_params",
        ),
        (
            stamp_paste(
                w,
                &json!({ "layer": "ground", "x": 0, "y": 0, "stamp": good, "op": "spin" }),
            ),
            "tile_bad_params",
        ),
        (
            stamp_paste(
                w,
                &json!({ "layer": "ground", "x": 0, "y": 0, "stamp": good, "op": "flip_x", "ops": [] }),
            ),
            "tile_bad_params",
        ),
        (
            stamp_paste(w, &json!({ "layer": "spawns", "x": 0, "y": 0, "stamp": good })),
            "tile_not_tile_layer",
        ),
        (
            stamp_paste(
                w,
                &json!({ "layer": "ground", "x": 0, "y": 0, "stamp": good, "ops": vec!["flip_x"; 17] }),
            ),
            "tile_bad_params",
        ),
    ];
    for (i, (result, expected)) in cases.into_iter().enumerate() {
        let error = result.expect_err("refused");
        assert_eq!(code(&error), expected, "case {i}");
    }
    assert_eq!(layer(w), vec![0; 12]);
    assert_eq!(
        w.resource::<SpriteAssets>().revision,
        rev,
        "a refused call leaves the revision alone"
    );
}

#[test]
fn autotile_register_from_a_missing_path_is_a_rules_error() {
    let mut app = app();
    let w = app.world_mut();
    let error = autotile_register(w, &json!({ "name": "x", "path": "assets/none.autotile.ron" }))
        .expect_err("no asset server / not in the manifest");
    assert_eq!(code(&error), "tile_bad_rules");
    let both = autotile_register(w, &json!({ "name": "x", "path": "p", "ron": RULES }));
    assert_eq!(code(&both.expect_err("both")), "tile_bad_params");
    // The registry is back in the world after the failed load.
    assert!(w.get_resource::<AutoTiles>().is_some());
}

/// Spec 86 § 5: `tilemap.paint` lists at most 1 000 changes and says so with `truncated`, while
/// `count` stays the full number; the layer holds every change.
#[test]
fn a_paint_with_more_than_a_thousand_changes_is_truncated_in_the_answer() {
    let mut app = app_sized(40, 40);
    let w = app.world_mut();
    autotile_register(w, &json!({ "name": "walls", "ron": RULES })).expect("registers");
    let out = paint(
        w,
        &json!({ "layer": "ground", "rules": "walls", "terrain": "wall", "rect": [0, 0, 39, 39] }),
    )
    .expect("paints");
    assert_eq!(out["count"], 1600);
    assert_eq!(out["truncated"], true);
    assert_eq!(out["changed"].as_array().map(Vec::len), Some(1000));
    // Sorted by (y, x): the first listed change is the top-left corner, the last is cell 999.
    assert_eq!(out["changed"][0]["cell"], json!([0, 0]));
    assert_eq!(out["changed"][999]["cell"], json!([39, 24]));
    assert!(layer(w).iter().all(|&g| g != 0), "every cell was painted");
}

/// Spec 86 § 5: the `tile_*` errors use JSON-RPC `-32602`, except `tile_unknown_rules` (`-32001`).
#[test]
fn tile_errors_carry_the_documented_json_rpc_codes() {
    let mut app = app();
    let w = app.world_mut();
    autotile_register(w, &json!({ "name": "walls", "ron": RULES })).expect("registers");
    let other_set = RULES.replace("tileset: \"t\"", "tileset: \"missing\"");
    autotile_register(w, &json!({ "name": "elsewhere", "ron": other_set })).expect("registers");
    let unknown = paint(
        w,
        &json!({ "layer": "ground", "rules": "nope", "terrain": "wall", "cells": [[0, 0]] }),
    )
    .expect_err("unknown rules");
    assert_eq!(code(&unknown), "tile_unknown_rules");
    assert_eq!(unknown.code, NOT_FOUND);
    assert_eq!(NOT_FOUND, -32001);
    let unknown = autotile(w, &json!({ "layer": "ground", "rules": "nope" })).expect_err("unknown");
    assert_eq!(
        (code(&unknown).as_str(), unknown.code),
        ("tile_unknown_rules", -32001)
    );
    for (err, expected) in [
        (
            paint(
                w,
                &json!({ "layer": "ground", "rules": "walls", "terrain": "lava", "cells": [[0, 0]] }),
            ),
            "tile_unknown_terrain",
        ),
        (
            paint(
                w,
                &json!({ "layer": "ground", "rules": "walls", "terrain": "wall", "cells": [[9, 9]] }),
            ),
            "tile_outside_map",
        ),
        (
            paint(
                w,
                &json!({ "layer": "ground", "rules": "walls", "terrain": "wall", "rect": [0, 0, 300, 300] }),
            ),
            "tile_too_many_cells",
        ),
        (
            set_tile(w, &json!({ "layer": "spawns", "x": 0, "y": 0, "gid": 1 })),
            "tile_not_tile_layer",
        ),
        (
            set_tile(w, &json!({ "layer": "ground", "x": 0, "y": 0, "gid": 99 })),
            "tile_bad_gid",
        ),
        (
            autotile_register(w, &json!({ "name": "bad", "ron": "(" })),
            "tile_bad_rules",
        ),
        (set_tile(w, &json!({ "layer": "ground" })), "tile_bad_params"),
        (
            paint(
                w,
                &json!({ "layer": "ground", "rules": "elsewhere", "terrain": "wall", "cells": [[0, 0]] }),
            ),
            "tile_bind_failed",
        ),
    ] {
        let err = err.expect_err(expected);
        assert_eq!(code(&err), expected);
        assert_eq!(err.code, INVALID_PARAMS, "{expected}");
        assert_eq!(INVALID_PARAMS, -32602);
    }
}
