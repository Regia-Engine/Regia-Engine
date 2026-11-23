// SPDX-License-Identifier: MPL-2.0
//! Ported minimap bake cases (requirements of `tools/maps/test_bake_minimap.py`, regia-absorb-3).
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_world_geom::overview::{self, BakeOptions, FULL_SIZE, HIDDEN_RGB, MINI_DIVISOR, SIGNAL_WHITE};

const SIZE: u32 = 64;

fn temp_dir() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "regia-minimap-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_map(root: &std::path::Path, surface: &str, water: Option<f64>, tunnels: bool) {
    let cells = 16;
    let grid: Vec<Vec<f64>> = (0..cells)
        .map(|r| (0..cells).map(|c| (r * 2 + c) as f64).collect())
        .collect();
    std::fs::write(
        root.join("heightfield.json"),
        serde_json::to_string(&serde_json::json!({
            "size_m": 320.0,
            "cells": cells,
            "heights": grid
        }))
        .unwrap(),
    )
    .unwrap();
    let mut layout = serde_json::json!({
        "surface": surface,
        "geysers": [{"x": -80.0, "z": 40.0}],
        "scrap_pools": [{"x": 60.0, "z": -60.0, "count": 4}],
        "spawns": [{"x": -120.0, "z": -120.0, "team": 1}],
        "bases": [{"x": 120.0, "z": 120.0, "team": 2}]
    });
    if let Some(level) = water {
        layout["water_level_m"] = serde_json::json!(level);
    } else {
        layout["water_level_m"] = serde_json::Value::Null;
    }
    std::fs::write(root.join("layout.json"), serde_json::to_string(&layout).unwrap()).unwrap();
    if tunnels {
        std::fs::write(
            root.join("tunnels.json"),
            serde_json::to_string(&serde_json::json!({"portals": [{"x": 0.0, "z": 0.0}]})).unwrap(),
        )
        .unwrap();
    }
}

fn bake(
    root: &std::path::Path,
    surface: &str,
    water: Option<f64>,
    tunnels: bool,
    contour: f64,
) -> (PathBuf, PathBuf) {
    let maps = root.join("maps");
    let map = maps.join("tiny");
    std::fs::create_dir_all(&map).unwrap();
    write_map(&map, surface, water, tunnels);
    let out = root.join("out");
    let report = overview::bake_directory(
        &map,
        &out,
        &BakeOptions {
            side: SIZE,
            contour_step_m: contour,
            force: true,
        },
        None,
    )
    .unwrap();
    (report.overview, report.minimap)
}

fn channel_mean(pixels: &[[u8; 3]], side: usize, rows: std::ops::Range<usize>, channel: usize) -> f64 {
    let mut total = 0u64;
    let mut count = 0u64;
    for y in rows {
        for x in 0..side {
            total += u64::from(pixels[y * side + x][channel]);
            count += 1;
        }
    }
    total as f64 / count as f64
}

#[test]
fn bakes_matching_sizes_and_format() {
    let root = temp_dir();
    let (overview, minimap) = bake(&root, "rock", None, true, 5.0);
    let overview_bytes = std::fs::read(&overview).unwrap();
    let (w, pixels) = overview::decode_rgb_png(&overview_bytes).unwrap();
    assert_eq!(w, SIZE);
    assert_eq!(pixels.len(), (SIZE as usize) * (SIZE as usize));
    let mini_bytes = std::fs::read(&minimap).unwrap();
    let (w, pixels) = overview::decode_rgb_png(&mini_bytes).unwrap();
    assert_eq!(w, SIZE / MINI_DIVISOR);
    assert_eq!(pixels.len(), (w as usize) * (w as usize));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn minimap_bake_twice_identical_bytes() {
    let root = temp_dir();
    let (overview, minimap) = bake(&root, "rock", None, true, 5.0);
    let first_overview = std::fs::read(&overview).unwrap();
    let first_mini = std::fs::read(&minimap).unwrap();
    let (again_overview, again_mini) = bake(&root, "rock", None, true, 5.0);
    assert_eq!(std::fs::read(again_overview).unwrap(), first_overview);
    assert_eq!(std::fs::read(again_mini).unwrap(), first_mini);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn markers_paint_signal_accents() {
    let root = temp_dir();
    let (overview, _) = bake(&root, "rock", None, true, 5.0);
    let (_, pixels) = overview::decode_rgb_png(&std::fs::read(overview).unwrap()).unwrap();
    let green = pixels
        .iter()
        .filter(|p| p[0] < 120 && p[1] > 200 && p[2] > 150)
        .count();
    assert!(green > 4, "geyser ring paints green pixels");
    let bright = pixels
        .iter()
        .filter(|p| p[0] > 220 && p[1] > 220 && p[2] > 220)
        .count();
    assert!(bright > 20, "portals, scrap and frame paint bright pixels");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn north_is_up() {
    let root = temp_dir();
    let map = root.join("maps").join("tiny");
    std::fs::create_dir_all(&map).unwrap();
    let cells = 8;
    let grid: Vec<Vec<f64>> = (0..cells).map(|r| vec![r as f64; cells]).collect();
    std::fs::write(
        map.join("heightfield.json"),
        serde_json::to_string(&serde_json::json!({"size_m": 160.0, "cells": cells, "heights": grid}))
            .unwrap(),
    )
    .unwrap();
    std::fs::write(map.join("layout.json"), "{}").unwrap();
    let out = root.join("out");
    let report = overview::bake_directory(
        &map,
        &out,
        &BakeOptions {
            side: SIZE,
            contour_step_m: 0.0,
            force: true,
        },
        None,
    )
    .unwrap();
    let (_, pixels) = overview::decode_rgb_png(&std::fs::read(report.overview).unwrap()).unwrap();
    let side = SIZE as usize;
    let top = channel_mean(&pixels, side, 0..8, 0);
    let bottom = channel_mean(&pixels, side, side - 8..side, 0);
    assert!(
        top > bottom,
        "higher ground (north input rows) renders at the top, brighter"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn world_to_px_matches_hud_projection() {
    let (x, y) = overview::world_to_px(0.0, 640.0, 640.0, 100);
    assert!((x - 49.5).abs() < 0.01 && y.abs() < 0.01);
    let (_x, y) = overview::world_to_px(0.0, -640.0, 640.0, 100);
    assert!((y - 99.0).abs() < 0.01);
    let (x, y) = overview::world_to_px(900.0, 900.0, 640.0, 100);
    assert!((x - 99.0).abs() < 0.01 && y.abs() < 0.01);
}

#[test]
fn water_tint_cools_low_ground() {
    let root = temp_dir();
    let (dry, _) = bake(&root, "rock", None, true, 5.0);
    let (_, dry_px) = overview::decode_rgb_png(&std::fs::read(dry).unwrap()).unwrap();
    let (wet, _) = bake(&root, "rock", Some(20.0), true, 5.0);
    let (_, wet_px) = overview::decode_rgb_png(&std::fs::read(wet).unwrap()).unwrap();
    let side = SIZE as usize;
    assert!(channel_mean(&wet_px, side, 0..side, 2) > channel_mean(&dry_px, side, 0..side, 2));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn lava_and_ice_surfaces_tint() {
    let root = temp_dir();
    let (plain, _) = bake(&root, "rock", None, true, 5.0);
    let (_, plain_px) = overview::decode_rgb_png(&std::fs::read(plain).unwrap()).unwrap();
    let (lava, _) = bake(&root, "lava", None, true, 5.0);
    let (_, lava_px) = overview::decode_rgb_png(&std::fs::read(lava).unwrap()).unwrap();
    let side = SIZE as usize;
    assert!(channel_mean(&lava_px, side, 0..side, 0) > channel_mean(&plain_px, side, 0..side, 0));

    let map = root.join("maps").join("flat");
    std::fs::create_dir_all(&map).unwrap();
    let grid = vec![vec![10.0; 8]; 8];
    std::fs::write(
        map.join("heightfield.json"),
        serde_json::to_string(&serde_json::json!({"size_m": 160.0, "cells": 8, "heights": grid})).unwrap(),
    )
    .unwrap();
    let out = root.join("out");
    let mut means = [0.0, 0.0];
    for (i, surface) in ["rock", "ice"].into_iter().enumerate() {
        std::fs::write(
            map.join("layout.json"),
            serde_json::to_string(&serde_json::json!({"surface": surface})).unwrap(),
        )
        .unwrap();
        let report = overview::bake_directory(
            &map,
            &out,
            &BakeOptions {
                side: SIZE,
                contour_step_m: 0.0,
                force: true,
            },
            None,
        )
        .unwrap();
        let (_, pixels) = overview::decode_rgb_png(&std::fs::read(report.overview).unwrap()).unwrap();
        means[i] = channel_mean(&pixels, side, 0..side, 1);
    }
    assert!(means[1] > means[0]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn missing_tunnels_means_no_portals_but_still_bakes() {
    let root = temp_dir();
    let (overview, _) = bake(&root, "rock", None, false, 5.0);
    assert!(overview.is_file());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn missing_heightfield_is_exit_2() {
    let root = temp_dir();
    let map = root.join("maps").join("tiny");
    std::fs::create_dir_all(&map).unwrap();
    std::fs::write(map.join("layout.json"), "{}").unwrap();
    let err = overview::bake_directory(
        &map,
        &root.join("out"),
        &BakeOptions {
            side: SIZE,
            contour_step_m: 5.0,
            force: true,
        },
        None,
    )
    .unwrap_err();
    assert_eq!(err.exit_code(), 2);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn bad_surface_is_exit_2() {
    let root = temp_dir();
    let map = root.join("maps").join("tiny");
    std::fs::create_dir_all(&map).unwrap();
    std::fs::write(
        map.join("heightfield.json"),
        serde_json::to_string(&serde_json::json!({
            "size_m": 320.0,
            "cells": 2,
            "heights": [[0.0, 1.0], [2.0, 3.0]]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(map.join("layout.json"), r#"{"surface":"cheese"}"#).unwrap();
    let err = overview::load_layout(&map.join("layout.json")).unwrap_err();
    assert_eq!(err.exit_code(), 2);
    let err = overview::bake_directory(
        &map,
        &root.join("out"),
        &BakeOptions {
            side: SIZE,
            contour_step_m: 5.0,
            force: true,
        },
        None,
    )
    .unwrap_err();
    assert_eq!(err.exit_code(), 2);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn default_side_is_the_ported_full_size() {
    assert_eq!(FULL_SIZE, 1024);
    assert_eq!(MINI_DIVISOR, 4);
    assert_ne!(HIDDEN_RGB, SIGNAL_WHITE);
}
