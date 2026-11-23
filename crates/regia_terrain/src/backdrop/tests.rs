// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/world/backdrop.rs, commit c9513623f), ADR-8012.
//! The backdrop tests ported from DO27 (`backdrop.rs` tests). Map names became profile fixtures:
//! DO27's per-map rows are DO27 data and ship with DO27, not here.

use super::*;

/// DO27's alpha map: 4096 samples over 1280 m.
const INNER_SIDE: u32 = 4096;
/// Its extended side: 2× the inner grid at the same texel.
const EXTENDED_SIDE: u32 = 8192;
/// Ring samples on each side of the 4096 map.
const RING_TEXELS: u32 = (EXTENDED_SIDE - INNER_SIDE) / 2;
/// The extended world width, chosen so the central 4096 samples align with the inner grid.
const EXTENDED_WORLD_M: f32 = 1280.0 * (EXTENDED_SIDE as f32 - 1.0) / (INNER_SIDE as f32 - 1.0);

/// DO27's ice map row (fixture data, as its JSON file held it).
const ICE_ROW: &str = r#"{ "ridge_wavelength_m": 260.0, "ridge_amplitude_m": 14.0, "plateau_drop_m": -30.0, "landform": "chaos_blocks" }"#;
/// DO27's canyon map row.
const CANYON_ROW: &str = r#"{ "ridge_wavelength_m": 420.0, "ridge_amplitude_m": 5.0, "plateau_drop_m": -20.0, "landform": "terraces" }"#;

fn ring_with(row: &str) -> BackdropRing {
    let profile = RingProfile::default().overlaid_json(row);
    assert!(profile.is_ok(), "{profile:?}");
    BackdropRing {
        profile: profile.unwrap_or_default(),
        ..BackdropRing::default()
    }
}

fn extend(inner: &TerrainMap, ring: &BackdropRing) -> TerrainMap {
    match extend_visual_map(inner, ring) {
        Ok(map) => map,
        Err(e) => panic!("wide map: {e}"),
    }
}

/// The lowest tier (DO27's ULTRA LOW ship profile) never loads the ring; every other tier keeps it.
#[test]
fn ultra_low_preset_never_loads_the_ring() {
    let ring = BackdropRing::default();
    assert!(
        !ring.loads_for_tier(Some(QualityTier::MobileLow)),
        "the lowest tier must drop the ring"
    );
    for tier in QualityTier::ALL
        .into_iter()
        .filter(|t| *t != QualityTier::MobileLow)
    {
        assert!(ring.loads_for_tier(Some(tier)), "{tier:?} keeps the ring");
    }
    assert!(ring.loads_for_tier(None), "no tier known: the ring loads");
    let off = BackdropRing {
        enabled: false,
        ..BackdropRing::default()
    };
    assert!(!off.loads_for_tier(Some(QualityTier::Ultra)));
}

fn inner_map() -> TerrainMap {
    // 4096², heights rising with column so the border has a real slope.
    let n = INNER_SIDE as usize;
    let mut heights = vec![0.0f32; n * n];
    for row in 0..n {
        for col in 0..n {
            heights[row * n + col] = (col as f32 * 0.1 + row as f32 * 0.05) - 240.0;
        }
    }
    match TerrainMap::from_heights(INNER_SIDE, INNER_SIDE, heights, 1280.0, 2048.0, -512.0) {
        Ok(map) => map,
        Err(e) => panic!("inner map: {e}"),
    }
}

/// The lowest tier carries no ring at all: the drawn map is the playable block.
#[test]
fn ultra_low_drops_the_ring_to_the_playable_block() {
    let inner = inner_map();
    let build = build_visual_map(&inner, &BackdropRing::default(), Some(QualityTier::MobileLow));
    assert!(build.is_ok(), "{build:?}");
    let Ok(build) = build else { return };
    assert!(build.visual.is_none());
    assert_eq!(build.status.state, BackdropState::TierOff);
    assert_eq!(build.status.visual_size, [INNER_SIDE, INNER_SIDE]);
    assert!(std::ptr::eq(build.visual_or(&inner), &inner));
}

#[test]
fn centre_block_is_copied_exactly() {
    let inner = inner_map();
    let wide = extend(&inner, &ring_with(ICE_ROW));
    assert_eq!(wide.width(), EXTENDED_SIDE);
    assert!((wide.world_width() - EXTENDED_WORLD_M).abs() < 1e-3);
    for row in 0..INNER_SIDE {
        for col in 0..INNER_SIDE {
            let a = inner.height_at_sample(i64::from(col), i64::from(row));
            let b = wide.height_at_sample(i64::from(col + RING_TEXELS), i64::from(row + RING_TEXELS));
            assert_eq!(a, b, "centre sample ({col},{row}) differs");
        }
    }
}

#[test]
fn ring_is_continuous_at_the_seam() {
    let inner = inner_map();
    let ring = ring_with(CANYON_ROW);
    let wide = extend(&inner, &ring);
    // The central block is cols RING_TEXELS..RING_TEXELS+INNER_SIDE (x = -640..+640).
    // Probe the +X side: the last central column is RING_TEXELS+INNER_SIDE-1 (x=+640),
    // the first ring column is RING_TEXELS+INNER_SIDE (x just past +640).
    let inner_half = inner.world_width() * 0.5;
    let first_ring = RING_TEXELS + INNER_SIDE;
    let probe_row = EXTENDED_SIDE / 2; // z = 0
    let [x, _] = wide.sample_to_world(first_ring, probe_row);
    let edge_h = inner.sample_bilinear(inner_half, 0.0);
    let ring_h = wide.height_at_sample(i64::from(first_ring), i64::from(probe_row));
    // The ring texel centre sits just past the border, so it is a small step from
    // the border height (a continuation), never a jump.
    assert!(
        (ring_h - edge_h).abs() < 2.0,
        "ring {ring_h} vs edge {edge_h} at world x {x}"
    );
    // The ring may not jump by more than the ridge amplitude + plateau drop.
    let max_jump = ring.profile.ridge_amplitude_m + ring.profile.plateau_drop_m.abs() + 40.0;
    let mut last = edge_h;
    for c in first_ring..EXTENDED_SIDE {
        let h = wide.height_at_sample(i64::from(c), i64::from(probe_row));
        assert!(
            (h - last).abs() < max_jump,
            "ring step {} m at col {c}",
            (h - last).abs()
        );
        last = h;
    }
}

#[test]
fn a_pit_cut_by_the_border_does_not_streak_out_into_the_ring() {
    // A 60 m pit 100 m deep sits on the middle of the east and west edges.
    // A clamped edge sample carried it hundreds of metres out as a channel
    // (a striped streak in top views); the widening border average spreads it out.
    let n = INNER_SIDE as usize;
    let half = 640.0f32;
    let cell = 1280.0 / (n as f32 - 1.0);
    let mut heights = vec![0.0f32; n * n];
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 * cell - half;
            let z = row as f32 * cell - half;
            let r = ((x.abs() - half).powi(2) + z * z).sqrt();
            if r < 60.0 {
                heights[row * n + col] = -100.0 * (1.0 - r / 60.0);
            }
        }
    }
    let inner = TerrainMap::from_heights(INNER_SIDE, INNER_SIDE, heights, 1280.0, 2048.0, -512.0);
    assert!(inner.is_ok(), "{inner:?}");
    let Ok(inner) = inner else { return };
    let wide = extend(&inner, &BackdropRing::default());
    let at = |x: f32, z: f32| wide.sample_bilinear(x, z);
    let channel = at(half + 300.0, 0.0);
    let beside = 0.5 * (at(half + 300.0, 200.0) + at(half + 300.0, -200.0));
    assert!(
        (channel - beside).abs() < 25.0,
        "pit streaks into the ring: {channel} on its line vs {beside} beside it"
    );
    // and the seam itself stays continuous on the pit
    assert!((at(half + 0.2, 0.0) - inner.sample_bilinear(half, 0.0)).abs() < 2.0);
}

#[test]
fn the_alpha_map_spec_is_the_old_constants() {
    let spec = BackdropSpec::for_inner(&inner_map(), &BackdropRing::default());
    assert!(spec.is_some());
    let Some(spec) = spec else { return };
    assert_eq!(spec.extended_size, [EXTENDED_SIDE, EXTENDED_SIDE]);
    assert_eq!(spec.ring_texels, RING_TEXELS);
    assert_eq!(spec.extended_world, [EXTENDED_WORLD_M, EXTENDED_WORLD_M]);
}

/// The spec scales with the inner map: smaller maps get a ring half their size, the 2048 m map
/// is capped at the 8192 ceiling, and a map already at the ceiling has no ring.
#[test]
fn the_spec_follows_the_inner_map_and_caps_at_8192() {
    let map = |side: u32, world: f32| match TerrainMap::from_heights(
        side,
        side,
        vec![0.0; (side * side) as usize],
        world,
        2048.0,
        -512.0,
    ) {
        Ok(map) => map,
        Err(e) => panic!("map: {e}"),
    };
    let ring = BackdropRing::default();
    let small = BackdropSpec::for_inner(&map(2048, 640.0), &ring);
    assert!(small.is_some());
    let Some(small) = small else { return };
    assert_eq!((small.extended_size, small.ring_texels), ([4096, 4096], 1024));
    assert!((small.extended_world[0] - 640.0 * 4095.0 / 2047.0).abs() < 1e-3);
    let mid = BackdropSpec::for_inner(&map(6554, 2048.0), &ring);
    assert!(mid.is_some());
    let Some(mid) = mid else { return };
    assert_eq!(mid.extended_size, [DEFAULT_MAX_SIDE, DEFAULT_MAX_SIDE]);
    assert_eq!(mid.ring_texels, (DEFAULT_MAX_SIDE - 6554) / 2);
    let full = map(8192, 5120.0);
    assert!(BackdropSpec::for_inner(&full, &ring).is_none());
    assert!(matches!(
        extend_visual_map(&full, &ring),
        Err(BackdropError::NoRoom { .. })
    ));
    // The cap and the floor are settings.
    let tight = BackdropRing {
        max_side: 2048 + 2 * 100,
        min_ring_texels: 101,
        ..BackdropRing::default()
    };
    assert!(BackdropSpec::for_inner(&map(2048, 640.0), &tight).is_none());
    let texels = BackdropRing {
        ring_width: RingWidth::Texels(300),
        ..BackdropRing::default()
    };
    let spec = BackdropSpec::for_inner(&map(2048, 640.0), &texels);
    assert_eq!(spec.map(|s| s.ring_texels), Some(300));
}

#[test]
fn a_non_alpha_map_extends_with_its_centre_copied() {
    let n = 2048u32;
    let heights: Vec<f32> = (0..n * n).map(|i| (i % n) as f32 * 0.05 - 20.0).collect();
    let inner = TerrainMap::from_heights(n, n, heights, 640.0, 2048.0, -512.0);
    assert!(inner.is_ok(), "{inner:?}");
    let Ok(inner) = inner else { return };
    let ring = BackdropRing::default();
    let wide = extend(&inner, &ring);
    let spec = BackdropSpec::for_inner(&inner, &ring);
    assert!(spec.is_some());
    let Some(spec) = spec else { return };
    assert_eq!(wide.width(), spec.extended_size[0]);
    for (col, row) in [(0u32, 0u32), (17, 900), (n - 1, n - 1), (1000, 5)] {
        let a = inner.height_at_sample(i64::from(col), i64::from(row));
        let b = wide.height_at_sample(
            i64::from(col + spec.ring_texels),
            i64::from(row + spec.ring_texels),
        );
        assert_eq!(a, b, "centre sample ({col},{row})");
    }
}

/// The default profile is the values DO27's code held for every map without a row, and DO27's two
/// rows read back through the overlay.
#[test]
fn the_shipped_profiles_are_the_values_the_code_used_to_hold() {
    let ice = ring_with(ICE_ROW).profile;
    assert_eq!(
        (ice.ridge_wavelength_m, ice.ridge_amplitude_m, ice.plateau_drop_m),
        (260.0, 14.0, -30.0)
    );
    assert_eq!(ice.landform, Landform::ChaosBlocks);
    let canyon = ring_with(CANYON_ROW).profile;
    assert_eq!(
        (
            canyon.ridge_wavelength_m,
            canyon.ridge_amplitude_m,
            canyon.plateau_drop_m
        ),
        (420.0, 5.0, -20.0)
    );
    assert_eq!(canyon.landform, Landform::Terraces);
    // Every other map: the default.
    let other = RingProfile::default();
    assert_eq!(
        (
            other.ridge_wavelength_m,
            other.ridge_amplitude_m,
            other.plateau_drop_m
        ),
        (420.0, 5.0, -20.0)
    );
    assert_eq!(other.landform, Landform::None);
}

#[test]
fn a_map_file_overlays_only_the_keys_it_names() {
    let base = ring_with(ICE_ROW).profile;
    let p = base.overlaid_json(r#"{"ridge_amplitude_m": 30.0, "landform": "terraces", "_comment": "x"}"#);
    assert!(p.is_ok(), "{p:?}");
    let Ok(p) = p else { return };
    assert_eq!(p.ridge_amplitude_m, 30.0);
    assert_eq!(p.landform, Landform::Terraces);
    assert_eq!(p.ridge_wavelength_m, 260.0);
    for bad in [
        r#"{"landform": "dunes"}"#,
        r#"{"ridge_wavelength_m": 0}"#,
        r#"{"plateau_drop_m": "deep"}"#,
    ] {
        assert!(base.overlaid_json(bad).is_err(), "{bad}");
    }
    // The same overlay as RON data.
    let r = base.overlaid_ron("(ridge_amplitude_m: 30.0, landform: terraces)");
    assert_eq!(r.ok(), Some(p));
    for bad in [
        "(landform: dunes)",
        "(ridge_wavelength_m: 0.0)",
        "(plateau_drop_m: \"deep\")",
    ] {
        assert!(base.overlaid_ron(bad).is_err(), "{bad}");
    }
}

/// Bad settings are refused before any work.
#[test]
fn bad_settings_are_refused() {
    let inner = match TerrainMap::from_heights(256, 256, vec![0.0; 256 * 256], 100.0, 1.0, 0.0) {
        Ok(map) => map,
        Err(e) => panic!("map: {e}"),
    };
    for ring in [
        BackdropRing {
            ring_width: RingWidth::InnerFraction(0.0),
            ..BackdropRing::default()
        },
        BackdropRing {
            ring_width: RingWidth::Texels(0),
            ..BackdropRing::default()
        },
        BackdropRing {
            min_ring_texels: 1,
            ..BackdropRing::default()
        },
        BackdropRing {
            seam_ease_m: f32::NAN,
            ..BackdropRing::default()
        },
        BackdropRing {
            profile: RingProfile {
                ridge_wavelength_m: -1.0,
                ..RingProfile::default()
            },
            ..BackdropRing::default()
        },
    ] {
        assert!(build_visual_map(&inner, &ring, None).is_err(), "{ring:?}");
    }
}

/// The band split of the fill does not change a sample: one thread and many give the same map.
#[test]
fn the_fill_does_not_depend_on_the_band_split() {
    let mut a = vec![0.0f32; 37 * 53];
    let mut b = vec![0.0f32; 37 * 53];
    let f = |row: u32, out: &mut [f32]| {
        for (c, v) in out.iter_mut().enumerate() {
            *v = fbm(c as f32 * 0.37, row as f32 * 0.21, 3);
        }
    };
    fill_rows(&mut a, 37, f);
    for row in 0..53u32 {
        f(row, &mut b[row as usize * 37..(row as usize + 1) * 37]);
    }
    assert_eq!(a, b);
}
