// SPDX-License-Identifier: MPL-2.0
//! The far backdrop ring (ADR-8459): a visual-only ring past the playable bounds with no seam,
//! skipped on the lowest tier, never touching the collider, and rectangular maps extended
//! without padding (ADR-8369).

use regia_render::QualityTier;
use regia_terrain::{
    BackdropRing, BackdropState, RingProfile, TerrainMap, build_visual_map, extend_visual_map,
};

/// A bumpy, sloped map so every border sample differs from its neighbours and from the plateau.
fn bumpy(cols: u32, rows: u32, spacing: f32) -> TerrainMap {
    let mut heights = Vec::with_capacity((cols * rows) as usize);
    for r in 0..rows {
        for c in 0..cols {
            let (x, z) = (c as f32, r as f32);
            heights.push(12.0 + 0.07 * x - 0.03 * z + 3.0 * (x * 0.37).sin() * (z * 0.23).cos());
        }
    }
    let map = TerrainMap::from_heights(cols, rows, heights, (cols - 1) as f32 * spacing, 1.0, 0.0);
    let map = if cols == rows {
        map
    } else {
        map.and_then(|m| m.with_world_depth((rows - 1) as f32 * spacing))
    };
    match map {
        Ok(map) => map,
        Err(e) => panic!("map: {e}"),
    }
}

/// A ring with strong noise and a landform, so any texel the ring computes differs from the edge.
fn rough_ring() -> BackdropRing {
    BackdropRing {
        profile: RingProfile {
            ridge_wavelength_m: 30.0,
            ridge_amplitude_m: 25.0,
            plateau_drop_m: -40.0,
            landform: regia_terrain::Landform::Terraces,
        },
        ..BackdropRing::default()
    }
}

fn extend(inner: &TerrainMap, ring: &BackdropRing) -> TerrainMap {
    match extend_visual_map(inner, ring) {
        Ok(map) => map,
        Err(e) => panic!("extend: {e}"),
    }
}

/// Every seam property on all four sides of `inner` extended to `wide` with `ring` samples a
/// side: the centre block is the inner map, the ring's first texel row holds the inner edge
/// exactly, and the drawn (bilinear) height anywhere between the last inner sample and the first
/// ring sample is the inner edge height: the gap across the seam is 0.
fn assert_seam_gap_zero(inner: &TerrainMap, wide: &TerrainMap, ring: u32) {
    let (w, h) = (inner.width(), inner.height());
    assert_eq!((wide.width(), wide.height()), (w + 2 * ring, h + 2 * ring));
    let inner_at = |c: u32, r: u32| inner.height_at_sample(i64::from(c), i64::from(r));
    let wide_at = |c: i64, r: i64| wide.height_at_sample(c + i64::from(ring), r + i64::from(ring));
    let mut worst = 0.0f32;
    let mut check = |what: &str, got: f32, want: f32| {
        let gap = (got - want).abs();
        worst = worst.max(gap);
        assert!(
            gap == 0.0,
            "{what}: drawn {got} vs inner edge {want} (gap {gap} m)"
        );
    };
    // −Z and +Z sides: row 0 / h−1 of the inner map, the ring row one further out.
    for (edge, out) in [(0i64, -1i64), (i64::from(h) - 1, i64::from(h))] {
        for c in 0..w {
            let want = inner_at(c, edge as u32);
            check(
                &format!("seam sample ({c}, {edge})"),
                wide_at(i64::from(c), edge),
                want,
            );
            check(
                &format!("first ring texel ({c}, {out})"),
                wide_at(i64::from(c), out),
                want,
            );
        }
    }
    // −X and +X sides.
    for (edge, out) in [(0i64, -1i64), (i64::from(w) - 1, i64::from(w))] {
        for r in 0..h {
            let want = inner_at(edge as u32, r);
            check(
                &format!("seam sample ({edge}, {r})"),
                wide_at(edge, i64::from(r)),
                want,
            );
            check(
                &format!("first ring texel ({out}, {r})"),
                wide_at(out, i64::from(r)),
                want,
            );
        }
    }
    // The diagonal ring texel beyond each corner holds the corner.
    for (c, r, oc, or) in [
        (0, 0, -1i64, -1i64),
        (w - 1, 0, i64::from(w), -1),
        (0, h - 1, -1, i64::from(h)),
        (w - 1, h - 1, i64::from(w), i64::from(h)),
    ] {
        check(
            &format!("corner ring texel ({oc}, {or})"),
            wide_at(oc, or),
            inner_at(c, r),
        );
    }
    // Rendered height across the seam band (bilinear between the edge and the first ring row),
    // at sample rows and halfway between them, against the inner map's height on its edge.
    let ring_f = ring as f32;
    for r in 0..h - 1 {
        for dv in [0.0f32, 0.5] {
            let v = r as f32 + dv;
            for (edge_u, sign) in [(0.0f32, -1.0f32), ((w - 1) as f32, 1.0)] {
                let want = inner.sample_bilinear_grid(edge_u, v);
                for du in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
                    let got = wide.sample_bilinear_grid(edge_u + ring_f + sign * du, v + ring_f);
                    check(&format!("band x at u {edge_u}{sign:+}{du}, v {v}"), got, want);
                }
            }
        }
    }
    for c in 0..w - 1 {
        for du in [0.0f32, 0.5] {
            let u = c as f32 + du;
            for (edge_v, sign) in [(0.0f32, -1.0f32), ((h - 1) as f32, 1.0)] {
                let want = inner.sample_bilinear_grid(u, edge_v);
                for dv in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
                    let got = wide.sample_bilinear_grid(u + ring_f, edge_v + ring_f + sign * dv);
                    check(&format!("band z at u {u}, v {edge_v}{sign:+}{dv}"), got, want);
                }
            }
        }
    }
    assert_eq!(worst, 0.0);
}

/// Acceptance: the ring's first texel row equals the inner edge exactly, and the rendered
/// height difference across the seam is 0 on every side and corner.
#[test]
fn backdrop_seam_gap_zero() {
    let inner = bumpy(257, 257, 0.5);
    let ring = rough_ring();
    let wide = extend(&inner, &ring);
    assert_seam_gap_zero(&inner, &wide, 128);
    // Past the first texel the ring is free: the rough profile moves it off the edge height.
    let past = wide.height_at_sample(128 + 257 + 40, 128 + 100);
    assert_ne!(
        past,
        inner.height_at_sample(256, 100),
        "the ring never left the edge height"
    );
}

/// The collider, nav and simulation take the inner map: building the backdrop leaves the
/// inner map, and the collider built from it, unchanged, and the visual map's collider would
/// differ (so the test can tell).
#[test]
fn the_backdrop_never_changes_the_collider() {
    let inner = bumpy(129, 129, 1.0);
    let heights_before = inner.heights().to_vec();
    let before = inner.collision_heightfield(None);
    let build = build_visual_map(&inner, &BackdropRing::default(), Some(QualityTier::High));
    assert!(build.is_ok(), "{build:?}");
    let Ok(build) = build else { return };
    assert_eq!(build.status.state, BackdropState::Built);
    let visual = build.visual_or(&inner);
    assert!(
        !std::ptr::eq(visual, &inner),
        "a High build draws the extended map"
    );
    let after = inner.collision_heightfield(None);
    assert_eq!(inner.heights(), heights_before.as_slice());
    assert_eq!((after.nx, after.nz), (before.nx, before.nz));
    assert_eq!(after.heights, before.heights);
    assert_eq!(after.scale, before.scale);
    assert_eq!(after.center, before.center);
    let drawn = visual.collision_heightfield(None);
    assert_ne!(
        (drawn.nx, drawn.nz),
        (before.nx, before.nz),
        "the drawn map is larger"
    );
    assert_eq!(build.status.inner_size, [129, 129]);
    // Inside the playable square the drawn surface is the collided one (within the collider's
    // 1 mm): the ring adds geometry only outside it.
    for [x, z] in [[0.0f32, 0.0], [-63.5, 12.25], [63.9, -63.9], [10.1, 40.7]] {
        let gap = (visual.sample_bilinear(x, z) - inner.sample_bilinear(x, z)).abs();
        assert!(gap <= 1e-3, "drawn vs collided at ({x}, {z}): {gap} m");
    }
}

/// The lowest tier (Regia's Very Low row) skips the ring: no extended map is built and the
/// status says why; every other tier builds it.
#[test]
fn very_low_tier_skips_the_ring() {
    let inner = bumpy(129, 129, 1.0);
    let ring = BackdropRing::default();
    let low = build_visual_map(&inner, &ring, Some(QualityTier::MobileLow));
    assert!(low.is_ok(), "{low:?}");
    let Ok(low) = low else { return };
    assert!(low.visual.is_none());
    assert_eq!(low.status.state, BackdropState::TierOff);
    assert_eq!(low.status.visual_size, [129, 129]);
    assert_eq!(low.status.ring_texels, 0);
    assert!(std::ptr::eq(low.visual_or(&inner), &inner));
    for tier in QualityTier::ALL
        .into_iter()
        .filter(|t| *t != QualityTier::MobileLow)
    {
        let build = build_visual_map(&inner, &ring, Some(tier));
        assert!(
            build
                .as_ref()
                .is_ok_and(|b| b.status.state == BackdropState::Built && b.visual.is_some()),
            "{tier:?}: {build:?}"
        );
    }
    // The tier list is a setting: a host can drop the ring on more rows.
    let more = BackdropRing {
        off_tiers: vec![QualityTier::MobileLow, QualityTier::Low],
        ..BackdropRing::default()
    };
    let build = build_visual_map(&inner, &more, Some(QualityTier::Low));
    assert!(build.is_ok_and(|b| b.status.state == BackdropState::TierOff && b.visual.is_none()));
}

/// With ADR-8369's rectangular maps: the ring goes round a rectangle and the result is a
/// rectangle at the same spacing on both axes, never padded to a square, with no seam.
#[test]
fn a_rectangular_map_extends_without_padding() {
    let (cols, rows, spacing) = (301u32, 133u32, 0.5f32);
    let inner = bumpy(cols, rows, spacing);
    assert_eq!(inner.world_extent(), [150.0, 66.0]);
    let ring = rough_ring();
    let wide = extend(&inner, &ring);
    // Ring: half the longer side (150 samples) on every side.
    let r = 150u32;
    assert_eq!((wide.width(), wide.height()), (cols + 2 * r, rows + 2 * r));
    assert_ne!(wide.width(), wide.height(), "padded to a square");
    assert_eq!(wide.heights().len(), ((cols + 2 * r) * (rows + 2 * r)) as usize);
    assert_eq!(wide.sample_spacing(), spacing);
    assert_eq!(wide.sample_spacing_z(), spacing);
    assert_eq!(
        wide.world_extent(),
        [
            (cols + 2 * r - 1) as f32 * spacing,
            (rows + 2 * r - 1) as f32 * spacing
        ]
    );
    // The centre lands on the inner samples' world positions.
    let near = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() <= 1e-4 && (a[1] - b[1]).abs() <= 1e-4;
    for (c, rr) in [(0u32, 0u32), (cols - 1, rows - 1), (17, 90), (cols - 1, 0)] {
        let (a, b) = (wide.sample_to_world(c + r, rr + r), inner.sample_to_world(c, rr));
        assert!(near(a, b), "({c}, {rr}): {a:?} vs {b:?}");
    }
    assert_seam_gap_zero(&inner, &wide, r);
    // A moved rectangle (ADR-8179) keeps its origin, so the centre still lands in place.
    let moved = inner.clone().with_grid_origin([-0.25, -0.25]);
    assert!(moved.is_ok(), "{moved:?}");
    let Ok(moved) = moved else { return };
    let wide = extend(&moved, &ring);
    assert_eq!(wide.grid_origin(), [-0.25, -0.25]);
    for (c, rr) in [(0u32, 0u32), (cols - 1, rows - 1), (150, 66)] {
        let (a, b) = (wide.sample_to_world(c + r, rr + r), moved.sample_to_world(c, rr));
        assert!(near(a, b), "({c}, {rr}): {a:?} vs {b:?}");
    }
    assert_seam_gap_zero(&moved, &wide, r);
}
