// SPDX-License-Identifier: MPL-2.0
//! Covered overview acceptance (ADR-8333). Behavior only: no query-bit or hash pins.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_world_geom::WorldGeom;
use regia_world_geom::compat::sense::satellite_shows;
use regia_world_geom::cook::{CookInputs, GroundInput, cook};
use regia_world_geom::cooked::{CookedTile, CookedTriangle, TriKind, encode_canonical};
use regia_world_geom::overview::{self, BakeOptions, HIDDEN_RGB, ROCK_HIDDEN, SIGNAL_WHITE, UnitView};

fn flat(triangles: Vec<CookedTriangle>) -> WorldGeom {
    let input = GroundInput {
        lattice_n: 17,
        width_m: 32.0,
        offset_m: 0.0,
        step_m: 0.03125,
        counts: vec![0; 17 * 17],
    };
    let mut cooked = cook(&CookInputs {
        document: b"(version: 1, ground_tile_cells: 16, layers: [])",
        ground: &input,
        terrain: None,
        read_file: &|_| Err("no sources".into()),
    })
    .unwrap()
    .file
    .canonical;
    if !triangles.is_empty() {
        cooked.tiles = vec![CookedTile {
            origin_mm: [0, 0],
            triangles,
        }];
    }
    WorldGeom::from_canonical_bytes(&encode_canonical(&cooked).unwrap()).unwrap()
}

fn floor(index: u32, y: i32) -> CookedTriangle {
    CookedTriangle {
        v: [[-10000, y, -10000], [0, y, 10000], [10000, y, -10000]],
        kind: TriKind::Floor,
        flags: 0,
        layer: 0,
        index,
        section: None,
        material: 0,
    }
}

fn roof(index: u32, y: i32) -> CookedTriangle {
    CookedTriangle {
        kind: TriKind::Roof,
        ..floor(index, y)
    }
}

fn temp_dir() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "regia-covered-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn unit(id: u32, entrance: bool, underground: bool) -> UnitView {
    UnitView {
        id,
        team: 2,
        pos: [1.0, 2.0, 0.0],
        flags: 0,
        lit: false,
        headlights: true,
        ping: true,
        entrance,
        underground,
    }
}

#[test]
fn unit_in_tube_hidden_entrance_shown_on_overview() {
    let geom = flat(vec![floor(0, 3000), roof(1, 7000)]);
    let state = geom.new_state();
    let tube_under = geom.covered_at(&state, 1.0, 0.0, 2.0);
    assert!(tube_under, "a clear roof covers a standing eye");
    assert!(!geom.covered_at(&state, 1.0, 0.0, 20.0), "above the roof is open");
    let open = flat(vec![]);
    assert!(!open.covered_at(&open.new_state(), 1.0, 0.0, 2.0));

    let mut units = [unit(2, false, tube_under), unit(3, true, tube_under)];
    overview::mark_rock_hidden(&mut units, None, 1, [1.0, 2.0, 200.0], false);
    assert_ne!(
        units[0].flags & ROCK_HIDDEN,
        0,
        "a far unit in the tube is rock-hidden"
    );
    assert_eq!(units[0].flags & overview::HEADLIGHT_ON, overview::HEADLIGHT_ON);
    assert_eq!(units[0].flags & overview::PING_FLASH, overview::PING_FLASH);
    assert!(!satellite_shows(units[0].underground, units[0].entrance));
    assert!(
        satellite_shows(units[1].underground, units[1].entrance),
        "the entrance marker shows"
    );

    let eye = 6.8;
    let half = 0.4;
    let clamped = geom.clamp_under_ceiling(&state, 1.0, eye, 0.0, 3.0, half);
    let lid = geom.support(&state, 1.0, 0.0, eye).ceiling.unwrap();
    assert!(clamped < lid.y, "the camera center stays under the lid");
    assert!(
        clamped + half <= lid.y + 1.0e-4,
        "the camera body does not enter the rock"
    );
}

#[test]
fn covered_ground_is_hidden_in_the_minimap_bake() {
    let geom = flat(vec![floor(0, 3000), roof(1, 7000)]);
    let state = geom.new_state();
    let root = temp_dir();
    let map = root.join("roof");
    std::fs::create_dir_all(&map).unwrap();
    let cells = 8;
    let grid = vec![vec![4.0; cells]; cells];
    std::fs::write(
        map.join("heightfield.json"),
        serde_json::to_string(&serde_json::json!({"size_m": 40.0, "cells": cells, "heights": grid})).unwrap(),
    )
    .unwrap();
    std::fs::write(map.join("layout.json"), r#"{"surface":"rock"}"#).unwrap();
    std::fs::write(map.join("tunnels.json"), r#"{"portals":[{"x":1.0,"z":0.0}]}"#).unwrap();
    let pred = |x: f64, z: f64| geom.covered_at(&state, x as f32, z as f32, 1.0);
    let report = overview::bake_directory(
        &map,
        &root.join("out"),
        &BakeOptions {
            side: 64,
            contour_step_m: 0.0,
            force: true,
        },
        Some(&pred),
    )
    .unwrap();
    let (side, pixels) = overview::decode_rgb_png(&std::fs::read(&report.overview).unwrap()).unwrap();
    let side_n = side as usize;
    let mask = overview::entrance_mask(side_n, 40.0, &[(1.0, 0.0)]);
    let mut covered = 0usize;
    let mut mouths = 0usize;
    for py in 0..side_n {
        for px in 0..side_n {
            let i = py * side_n + px;
            let (x, z) = overview::pixel_world(px, py, 40.0, side_n);
            if mask[i] {
                assert_eq!(pixels[i], SIGNAL_WHITE, "mouth pixel {px},{py} is an entrance");
                mouths += 1;
            } else if pred(x, z) {
                assert_eq!(pixels[i], HIDDEN_RGB, "covered pixel {px},{py} is hidden");
                covered += 1;
            }
        }
    }
    assert!(mouths > 0, "the mouth draws at least one entrance pixel");
    assert!(covered > 0, "the roof hides at least one overview pixel");
    let (_, mini) = overview::decode_rgb_png(&std::fs::read(report.minimap).unwrap()).unwrap();
    assert!(mini.contains(&HIDDEN_RGB));
    let _ = std::fs::remove_dir_all(root);
}
