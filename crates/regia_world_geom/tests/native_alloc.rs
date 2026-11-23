// SPDX-License-Identifier: MPL-2.0
//! Allocation counters use the repository's audited allocator directly, without pulling its renderer dependencies.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]
use regia_world_geom::allocation_counter as alloc_counter;
pub mod native_support;
use native_support::*;
use regia_world_geom::FaceMask;
#[global_allocator]
static GLOBAL: alloc_counter::CountingAlloc = alloc_counter::CountingAlloc::new();
#[test]
fn support_allocates_nothing() {
    let _exclusive = alloc_counter::exclusive();
    let file = read("switchbacks_mesh");
    let geom = native(&file, "switchbacks_mesh", 16.0);
    let state = geom.new_state();
    let points: Vec<_> = file["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| [hex(&r["x"]), hex(&r["z"])])
        .collect();
    let (_, count) = alloc_counter::measure(|| {
        for &[x, z] in &points {
            std::hint::black_box(geom.support(&state, x, z, -275.0));
        }
    });
    assert_eq!(count.allocs, 0, "{count:?}");
}
#[test]
fn segment_allocates_nothing() {
    let _exclusive = alloc_counter::exclusive();
    let file = read("switchbacks_mesh");
    let geom = native(&file, "switchbacks_mesh", 16.0);
    let state = geom.new_state();
    let points: Vec<_> = file["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| [hex(&r["x"]), hex(&r["z"])])
        .collect();
    let (_, count) = alloc_counter::measure(|| {
        for &[x, z] in &points {
            std::hint::black_box(geom.segment(
                &state,
                [x, -275.0, z],
                [x + 30.0, -300.0, z + 30.0],
                FaceMask::SOLID,
            ));
        }
    });
    assert_eq!(count.allocs, 0, "{count:?}");
}
/// Queries on a state with D1 edits read the copy-on-write override blocks and allocate nothing
/// (spec 121 § 2 item 4; ADR-8313).
#[test]
fn edited_state_queries_allocate_nothing() {
    let _exclusive = alloc_counter::exclusive();
    let file = read("switchbacks_mesh");
    let geom = native(&file, "switchbacks_mesh", 16.0);
    let mut state = geom.new_state();
    let points: Vec<_> = file["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| [hex(&r["x"]), hex(&r["z"])])
        .collect();
    for &[x, z] in points.iter().step_by(97) {
        let edit = regia_world_geom::GroundEdit::Crater {
            x_mm: regia_world_geom::m_to_mm(x),
            z_mm: regia_world_geom::m_to_mm(z),
            radius_mm: 30_000,
            depth_mm: 450,
        };
        state.apply_ground_edit(&geom, &edit).unwrap();
    }
    assert!(state.ground_edits().override_count() > 0);
    let mut spans = regia_world_geom::SpanBuf::default();
    let (_, count) = alloc_counter::measure(|| {
        for &[x, z] in &points {
            std::hint::black_box(geom.support(&state, x, z, -275.0));
            geom.floors_at(&state, x, z, &mut spans);
            std::hint::black_box(geom.segment(
                &state,
                [x, -275.0, z],
                [x + 30.0, -300.0, z + 30.0],
                FaceMask::SOLID,
            ));
        }
    });
    assert_eq!(count.allocs, 0, "{count:?}");
}

/// A per-tick clone of an edited state copies the block list (one pointer per touched ground tile),
/// never the overrides: its allocation count does not grow with the number of overrides (review W8).
#[test]
fn state_clone_shares_override_blocks() {
    let _exclusive = alloc_counter::exclusive();
    let file = read("switchbacks_mesh");
    let geom = native(&file, "switchbacks_mesh", 16.0);
    let mut state = geom.new_state();
    let mut last = None;
    for k in 0..40 {
        let edit = regia_world_geom::GroundEdit::Crater {
            // The mesh-only fixture's lattice pitch is 20 m: 120 m craters hold about 110 vertices.
            x_mm: -400_000 + k * 20_000,
            z_mm: 50_000,
            radius_mm: 120_000,
            depth_mm: 300,
        };
        state.apply_ground_edit(&geom, &edit).unwrap();
        let (copy, count) = alloc_counter::measure(|| state.clone());
        std::hint::black_box(&copy);
        // The block list and the blocker words: never one allocation per override.
        assert!(count.allocs <= 3, "edit {k}: {count:?}");
        last = Some((state.ground_edits().override_count(), count));
    }
    println!("clone after 40 craters: {last:?}");
    assert!(last.unwrap().0 > 500);
}
