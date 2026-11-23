// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): list geometry, selection, type-ahead and sticky headers as pure
//! data, with boundary and hostile numbers and seeded properties. No ECS, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_test_utils::fuzz::FuzzRng;
use regia_ui::list::{
    ListGeometry, ListSelection, RowMetrics, ScrollAlign, SelectionMode, TileGrid, TypeAhead,
    offset_for_item, sticky_position, type_ahead_match,
};

#[test]
fn uniform_rows_clamp_their_lookups() {
    let rows = RowMetrics::uniform(10, 20.0);
    assert_eq!((rows.count(), rows.total_height()), (10, 200.0));
    assert_eq!((rows.top(3), rows.top(10), rows.top(99)), (60.0, 200.0, 200.0));
    assert_eq!(
        (rows.height(9), rows.height(10), rows.height(usize::MAX)),
        (20.0, 0.0, 0.0)
    );
    for (y, want) in [
        (-5.0, 0),
        (0.0, 0),
        (19.99, 0),
        (20.0, 1),
        (59.9, 2),
        (60.0, 3),
        (199.9, 9),
        (200.0, 9),
        (1e9, 9),
        (f32::NAN, 0),
    ] {
        assert_eq!(rows.index_at(y), want, "y {y}");
    }
    assert_eq!(rows.index_at(f32::INFINITY), 9);
    assert_eq!(rows.index_at(f32::NEG_INFINITY), 0);
    let empty = RowMetrics::uniform(0, 20.0);
    assert_eq!(
        (
            empty.count(),
            empty.total_height(),
            empty.index_at(50.0),
            empty.top(3),
            empty.height(0)
        ),
        (0, 0.0, 0, 0.0, 0.0)
    );
    // Heights below one pixel, negative or NaN count as one.
    for h in [0.2, -4.0, 0.0, f32::NAN] {
        assert_eq!(RowMetrics::uniform(5, h).total_height(), 5.0, "{h}");
    }
}

#[test]
fn variable_rows_use_prefix_sums_and_defaults() {
    let rows = RowMetrics::variable(4, 10.0, &[5.0, 30.0]);
    assert_eq!((rows.count(), rows.total_height()), (4, 55.0));
    assert_eq!(
        (
            rows.top(0),
            rows.top(1),
            rows.top(2),
            rows.top(3),
            rows.top(4),
            rows.top(9)
        ),
        (0.0, 5.0, 35.0, 45.0, 55.0, 55.0)
    );
    assert_eq!(
        (rows.height(1), rows.height(3), rows.height(4)),
        (30.0, 10.0, 0.0)
    );
    for (y, want) in [
        (-1.0, 0),
        (4.99, 0),
        (5.0, 1),
        (34.9, 1),
        (35.0, 2),
        (54.9, 3),
        (55.0, 3),
        (1e9, 3),
        (f32::NAN, 0),
    ] {
        assert_eq!(rows.index_at(y), want, "y {y}");
    }
    // Hostile heights are raised to one pixel; extra heights beyond `count` are ignored.
    let rows = RowMetrics::variable(3, f32::NAN, &[f32::NAN, -9.0, 0.0, 500.0, 500.0]);
    assert_eq!(rows.total_height(), 3.0);
    let none = RowMetrics::variable(0, 10.0, &[1.0, 2.0]);
    assert_eq!(
        (none.count(), none.total_height(), none.index_at(3.0)),
        (0, 0.0, 0)
    );
}

#[test]
fn every_position_maps_to_the_row_that_contains_it() {
    let mut rng = FuzzRng::new(0x707);
    for _ in 0..200 {
        let heights: Vec<f32> = (0..1 + rng.below(40))
            .map(|_| 1.0 + rng.below(90) as f32)
            .collect();
        let rows = RowMetrics::variable(heights.len(), 20.0, &heights);
        for _ in 0..50 {
            let y = rng.below((rows.total_height() as usize).max(1)) as f32 + rng.below(100) as f32 / 100.0;
            let i = rows.index_at(y);
            assert!(i < rows.count());
            if y < rows.total_height() {
                assert!(
                    rows.top(i) <= y && y < rows.top(i) + rows.height(i),
                    "y {y} row {i}"
                );
            }
        }
    }
}

#[test]
fn tile_grids_fit_columns_and_wrap() {
    let grid = TileGrid::new(10, 100.0, [30.0, 20.0], [5.0, 5.0], None);
    assert_eq!((grid.columns(), grid.rows(), grid.total_height()), (3, 4, 95.0));
    assert_eq!(grid.item_rect(0), [0.0, 0.0, 30.0, 20.0]);
    assert_eq!(grid.item_rect(4), [35.0, 25.0, 30.0, 20.0]);
    assert_eq!(grid.item_rect(9), [0.0, 75.0, 30.0, 20.0]);
    // A fixed column count wins over the width; zero counts as one.
    assert_eq!(
        TileGrid::new(10, 1000.0, [30.0, 20.0], [0.0, 0.0], Some(4)).columns(),
        4
    );
    assert_eq!(
        TileGrid::new(10, 1000.0, [30.0, 20.0], [0.0, 0.0], Some(0)).columns(),
        1
    );
    // A width that fits less than one tile, or is NaN or negative, still gives one column.
    for width in [0.0, 10.0, -50.0, f32::NAN] {
        assert_eq!(
            TileGrid::new(3, width, [30.0, 20.0], [5.0, 5.0], None).columns(),
            1,
            "{width}"
        );
    }
    // Tiny or negative tile and gap values are raised to 1 px and 0 px.
    let grid = TileGrid::new(4, 100.0, [0.0, -3.0], [-5.0, -5.0], Some(2));
    assert_eq!(grid.item_rect(3), [1.0, 1.0, 1.0, 1.0]);
    let empty = TileGrid::new(0, 100.0, [30.0, 20.0], [5.0, 5.0], None);
    assert_eq!((empty.rows(), empty.total_height()), (0, 0.0));
}

#[test]
fn visible_ranges_cover_the_viewport_and_never_leave_the_list() {
    let rows = ListGeometry::Rows(RowMetrics::uniform(100, 10.0));
    assert_eq!(rows.visible_range(0.0, 25.0, 0), 0..3);
    assert_eq!(rows.visible_range(0.0, 25.0, 2), 0..5);
    assert_eq!(rows.visible_range(500.0, 30.0, 0), 50..54);
    assert_eq!(rows.visible_range(500.0, 30.0, 3), 47..57);
    assert_eq!(
        rows.visible_range(1e6, 30.0, 0),
        99..100,
        "past the end shows the last row"
    );
    assert_eq!(
        rows.visible_range(500.0, -50.0, 0),
        50..51,
        "a negative viewport counts as zero"
    );
    assert_eq!(rows.visible_range(0.0, 1e9, 0), 0..100);
    assert_eq!(
        rows.visible_range(0.0, 25.0, 1_000_000),
        0..100,
        "huge overscan is clamped"
    );
    assert_eq!(
        ListGeometry::Rows(RowMetrics::uniform(0, 10.0)).visible_range(0.0, 25.0, 3),
        0..0
    );
    let tiles = ListGeometry::Tiles(TileGrid::new(10, 100.0, [30.0, 20.0], [5.0, 5.0], None));
    assert_eq!(tiles.visible_range(0.0, 30.0, 0), 0..6);
    assert_eq!(tiles.visible_range(60.0, 10.0, 0), 6..9);
    assert_eq!(tiles.visible_range(1e6, 10.0, 0).end, 10);
    assert!(tiles.visible_range(1e6, 10.0, 0).start <= 10);
    // Seeded: the range always contains every item the viewport touches.
    let mut rng = FuzzRng::new(31);
    for _ in 0..500 {
        let offset = rng.below(1100) as f32;
        let viewport = rng.below(200) as f32;
        let range = rows.visible_range(offset, viewport, 0);
        assert!(range.start <= range.end && range.end <= 100);
        for i in 0..100usize {
            let (top, bottom) = (i as f32 * 10.0, i as f32 * 10.0 + 10.0);
            if top <= offset + viewport && bottom > offset {
                assert!(
                    range.contains(&i),
                    "item {i} is in view at {offset}+{viewport}: {range:?}"
                );
            }
        }
    }
}

#[test]
fn pages_hold_at_least_one_row() {
    let rows = ListGeometry::Rows(RowMetrics::uniform(100, 10.0));
    assert_eq!(rows.page_items(0.0, 100.0), 10);
    assert_eq!(rows.page_items(0.0, 5.0), 1);
    assert_eq!(rows.page_items(0.0, -5.0), 1);
    assert_eq!(rows.page_items(0.0, 0.0), 1);
    let tiles = ListGeometry::Tiles(TileGrid::new(30, 100.0, [30.0, 20.0], [5.0, 5.0], None));
    assert_eq!(tiles.page_items(0.0, 60.0), 2 * 3);
    assert_eq!(tiles.page_items(0.0, 1.0), 3, "one row of tiles at least");
}

#[test]
fn scroll_to_item_alignments() {
    // Item at 100..140 in a 50 px viewport scrolled to 0.
    let at = |align, offset| offset_for_item(100.0, 40.0, offset, 50.0, align);
    assert_eq!(at(ScrollAlign::Start, 0.0), 100.0);
    assert_eq!(at(ScrollAlign::End, 0.0), 90.0);
    assert_eq!(at(ScrollAlign::Center, 0.0), 95.0);
    assert_eq!(
        at(ScrollAlign::Nearest, 0.0),
        90.0,
        "below the view: bottom edge aligns"
    );
    assert_eq!(
        at(ScrollAlign::Nearest, 200.0),
        100.0,
        "above the view: top edge aligns"
    );
    assert_eq!(
        at(ScrollAlign::Nearest, 95.0),
        95.0,
        "fully visible: unchanged... "
    );
    assert_eq!(
        at(ScrollAlign::Nearest, 90.0),
        90.0,
        "item exactly fills the bottom: unchanged"
    );
    assert_eq!(
        at(ScrollAlign::Nearest, 100.0),
        100.0,
        "item exactly at the top: unchanged"
    );
    for (name, want) in [
        ("nearest", ScrollAlign::Nearest),
        ("start", ScrollAlign::Start),
        ("center", ScrollAlign::Center),
        ("end", ScrollAlign::End),
    ] {
        assert_eq!(ScrollAlign::parse(name), Some(want));
    }
    assert_eq!(ScrollAlign::parse("Start"), None);
}

#[test]
fn selection_modes_gate_every_mutator() {
    let mut none = ListSelection::new(SelectionMode::None);
    assert!(
        !none.select_only(1) && !none.toggle(1) && !none.select_range(0, 3, false) && !none.select_all(5)
    );
    assert!(!none.click(2, true, true) && !none.step(2, true, true));
    assert!(none.is_empty());
    assert!(
        !none.set(&[1, 2]) && none.is_empty(),
        "set in none mode selects nothing"
    );
    let mut single = ListSelection::new(SelectionMode::Single);
    assert!(single.select_only(3));
    assert!(!single.select_only(3), "the same item again changes nothing");
    assert!(single.click(5, true, false), "shift is ignored in single mode");
    assert_eq!(single.to_vec(), [5]);
    assert!(
        single.toggle(5) && single.is_empty(),
        "toggling the selected item deselects it"
    );
    assert_eq!(single.anchor(), None);
    assert!(
        single.set(&[1, 2, 9]) && single.to_vec() == [9],
        "single keeps the last"
    );
    assert!(!single.select_all(4), "select_all is multi only");
    assert!(
        single.select_range(0, 7, false) && single.to_vec() == [7],
        "a range in single mode selects the end"
    );
}

#[test]
fn multi_selection_ranges_toggles_and_anchors() {
    let mut s = ListSelection::new(SelectionMode::Multi);
    assert!(s.click(2, false, false));
    assert_eq!((s.to_vec(), s.anchor()), (vec![2], Some(2)));
    assert!(s.click(5, true, false), "shift extends from the anchor");
    assert_eq!(s.to_vec(), [2, 3, 4, 5]);
    assert_eq!(s.anchor(), Some(2), "a range leaves the anchor");
    assert!(s.click(0, true, false), "a shift range replaces the old range");
    assert_eq!(s.to_vec(), [0, 1, 2]);
    assert!(s.click(8, true, true), "ctrl+shift adds the range");
    assert_eq!(s.to_vec(), [0, 1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(s.click(4, false, true) && !s.is_selected(4), "ctrl toggles");
    assert_eq!(s.anchor(), Some(4));
    assert!(
        !s.step(1, false, true),
        "ctrl+arrow moves the cursor without selecting"
    );
    assert!(
        s.step(1, false, false) && s.to_vec() == [1],
        "a plain step selects only the item"
    );
    assert!(s.step(4, true, false) && s.to_vec() == [1, 2, 3, 4]);
    // A reversed range is the same range.
    let mut t = ListSelection::new(SelectionMode::Multi);
    assert!(t.select_range(6, 2, false));
    assert_eq!(t.to_vec(), [2, 3, 4, 5, 6]);
    assert!(
        !t.select_range(6, 2, true),
        "adding what is there changes nothing"
    );
    assert_eq!(t.len(), 5);
}

#[test]
fn select_all_and_shrinking_lists() {
    let mut s = ListSelection::new(SelectionMode::Multi);
    assert!(s.select_all(4) && s.to_vec() == [0, 1, 2, 3]);
    assert!(!s.select_all(4), "already everything");
    assert!(s.select_all(0) && s.is_empty(), "an empty list selects nothing");
    assert!(!s.select_all(0));
    // Same count, different items: that is a change.
    assert!(s.set(&[5, 6]));
    assert!(s.select_all(2), "{{5, 6}} becomes {{0, 1}}");
    assert_eq!(s.to_vec(), [0, 1]);
    // retain_below drops the items and the anchor that fell off the end.
    let mut s = ListSelection::new(SelectionMode::Multi);
    s.set(&[1, 7, 9]);
    assert_eq!(s.anchor(), Some(9));
    assert!(s.retain_below(8));
    assert_eq!((s.to_vec(), s.anchor()), (vec![1, 7], None));
    assert!(!s.retain_below(8));
    assert!(s.retain_below(0) && s.is_empty());
    assert!(!s.clear() && s.anchor().is_none());
}

#[test]
fn type_ahead_buffer_expires_and_matching_wraps() {
    let mut t = TypeAhead::default();
    assert_eq!(t.push('a', 0.0), "a");
    assert_eq!(t.push('b', 0.4), "ab");
    assert_eq!(t.push('c', 0.4 + 10.0), "c", "a long pause starts over");
    assert_eq!(t.buffer(), "c");
    t.clear();
    assert_eq!(t.buffer(), "");
    assert_eq!(t.push('é', 100.0), "é");
    let labels = ["Apple", "avocado", "Banana", "apricot"];
    let label = |i: usize| labels[i].to_owned();
    assert_eq!(type_ahead_match(4, None, "a", label), Some(0));
    assert_eq!(
        type_ahead_match(4, Some(0), "a", label),
        Some(1),
        "one letter starts after the cursor"
    );
    assert_eq!(type_ahead_match(4, Some(1), "a", label), Some(3));
    assert_eq!(type_ahead_match(4, Some(3), "a", label), Some(0), "wraps");
    assert_eq!(
        type_ahead_match(4, Some(0), "ap", label),
        Some(0),
        "longer prefixes may stay on the cursor"
    );
    assert_eq!(
        type_ahead_match(4, Some(1), "AP", label),
        Some(3),
        "case-insensitive"
    );
    assert_eq!(
        type_ahead_match(4, Some(99), "b", label),
        Some(2),
        "a stale cursor past the end still searches"
    );
    assert_eq!(type_ahead_match(4, None, "z", label), None);
    assert_eq!(type_ahead_match(4, None, "", label), None);
    assert_eq!(type_ahead_match(0, None, "a", label), None);
    assert_eq!(
        type_ahead_match(1, Some(0), "a", label),
        Some(0),
        "the only match is the cursor: a single item cycles to itself"
    );
}

#[test]
fn sticky_headers_stick_and_get_pushed_out() {
    let rows = ListGeometry::Rows(RowMetrics::uniform(100, 10.0));
    let headers = [0, 10, 20];
    let at = |offset: f32| sticky_position(&headers, &rows, offset, 200.0);
    assert_eq!(at(0.0), Some((0, 0.0)));
    assert_eq!(at(55.0), Some((0, 55.0)), "sticks to the top of the view");
    assert_eq!(at(95.0), Some((0, 90.0)), "the next header pushes it up");
    assert_eq!(at(100.0), Some((10, 100.0)));
    assert_eq!(at(-50.0), Some((0, 0.0)), "overscroll above the first row");
    assert_eq!(
        at(1e6),
        Some((20, 1e6)),
        "past the end: the last section's header"
    );
    assert_eq!(sticky_position(&[], &rows, 10.0, 200.0), None);
    assert_eq!(
        sticky_position(&[5], &rows, 0.0, 200.0),
        None,
        "before the first header there is none"
    );
    assert_eq!(
        sticky_position(
            &headers,
            &ListGeometry::Rows(RowMetrics::uniform(0, 10.0)),
            0.0,
            200.0
        ),
        None
    );
    let tiles = ListGeometry::Tiles(TileGrid::new(10, 100.0, [30.0, 20.0], [5.0, 5.0], None));
    assert_eq!(sticky_position(&headers, &tiles, 0.0, 200.0), None);
}
