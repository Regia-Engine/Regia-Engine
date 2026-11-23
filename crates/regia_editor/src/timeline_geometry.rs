// SPDX-License-Identifier: MPL-2.0
//! Shared geometry of the editor's timelines (ADR-7325): the `.regiatrace` view
//! ([`crate::trace_timeline`]) and the Profiler panel's per-thread Timeline tab
//! ([`crate::thread_timeline`]).
//!
//! - [`merge_subpixel_bars`]: in each row, bars that start in a pixel column another bar already
//!   starts in, and runs of adjacent bars narrower than one pixel, become one bar carrying their
//!   count. A row then holds at most one bar per pixel column, however many spans it has.
//! - [`LayoutCache`]: a layout kept by (data revision, view), so a steady repaint does not rebuild it.
//! - [`zoom_about`] and [`pan_by`]: zoom keeping the time under the pointer fixed, and drag panning,
//!   on the [`TimelineView`] both views share.

use std::sync::Arc;

use crate::trace_timeline::{MAX_ZOOM, TimelineView};

/// One bar before (and after) merging: its row, its extent in points from the view's left (not
/// yet clipped), its time extent, the item it stands for and how many items it covers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelBar {
    /// Row from the top.
    pub row: usize,
    /// Left edge, points.
    pub x0: f64,
    /// Right edge, points.
    pub x1: f64,
    /// Start, ns.
    pub t0: u64,
    /// End, ns.
    pub t1: u64,
    /// The first item (the caller's index) the bar stands for.
    pub item: usize,
    /// Items the bar covers (1 = not merged).
    pub count: u32,
}

impl PixelBar {
    fn narrow(&self) -> bool {
        self.x1 - self.x0 < 1.0
    }
}

/// Merges sub-pixel bars. `bars` must be sorted by row, then `x0`. In a row, a bar joins the
/// previous output bar when it starts in the same pixel column, or when both are narrower than one
/// pixel (or the previous is already a merge) and it starts within one pixel of the previous bar's
/// end. The merged bar keeps the first item, spans both extents and adds the counts. Output bars of
/// one row start in strictly increasing pixel columns.
#[must_use]
pub fn merge_subpixel_bars(bars: &[PixelBar]) -> Vec<PixelBar> {
    let mut out: Vec<PixelBar> = Vec::with_capacity(bars.len().min(4096));
    for bar in bars {
        if let Some(cur) = out.last_mut()
            && cur.row == bar.row
        {
            let same_column = bar.x0.floor() <= cur.x0.floor();
            let adjacent = bar.x0 <= cur.x1 + 1.0;
            let small = bar.narrow() && (cur.count > 1 || cur.narrow());
            if same_column || (small && adjacent) {
                cur.x1 = cur.x1.max(bar.x1);
                cur.t0 = cur.t0.min(bar.t0);
                cur.t1 = cur.t1.max(bar.t1);
                cur.count = cur.count.saturating_add(bar.count);
                continue;
            }
        }
        out.push(*bar);
    }
    out
}

/// A layout kept by key (data revision and view): [`Self::get_or_build`] returns the kept value
/// while the key is unchanged, so a steady repaint reuses it. Counts its hits and builds.
pub struct LayoutCache<K, V> {
    entry: Option<(K, Arc<V>)>,
    hits: u64,
    builds: u64,
}

impl<K, V> Default for LayoutCache<K, V> {
    fn default() -> Self {
        Self {
            entry: None,
            hits: 0,
            builds: 0,
        }
    }
}

impl<K: Clone, V> Clone for LayoutCache<K, V> {
    fn clone(&self) -> Self {
        Self {
            entry: self.entry.as_ref().map(|(k, v)| (k.clone(), Arc::clone(v))),
            hits: self.hits,
            builds: self.builds,
        }
    }
}

impl<K: std::fmt::Debug, V> std::fmt::Debug for LayoutCache<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutCache")
            .field("key", &self.entry.as_ref().map(|(k, _)| k))
            .field("hits", &self.hits)
            .field("builds", &self.builds)
            .finish()
    }
}

impl<K: PartialEq, V> LayoutCache<K, V> {
    /// The kept layout when `key` equals the kept key (a hit), else `build()`'s, which is kept.
    pub fn get_or_build(&mut self, key: K, build: impl FnOnce() -> V) -> Arc<V> {
        if let Some((kept, value)) = &self.entry
            && *kept == key
        {
            self.hits += 1;
            return Arc::clone(value);
        }
        self.builds += 1;
        let value = Arc::new(build());
        self.entry = Some((key, Arc::clone(&value)));
        value
    }

    /// Calls that returned the kept layout.
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Calls that built a layout.
    #[must_use]
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// Drops the kept layout (the next call builds).
    pub fn invalidate(&mut self) {
        self.entry = None;
    }
}

/// The cache key of a view: zoom, pan and width by their bits (an exact match, NaN included).
#[must_use]
pub fn view_key(view: &TimelineView) -> (u32, u32, u32) {
    (view.zoom.to_bits(), view.pan.to_bits(), view.width.to_bits())
}

/// The fraction (0 to 1) of the whole span at the view's left edge, and the visible fraction.
fn window(view: &TimelineView) -> (f64, f64) {
    let v = view.clamped();
    let visible = 1.0 / f64::from(v.zoom);
    (f64::from(v.pan) * (1.0 - visible), visible)
}

/// The view's pan for a window that starts at fraction `start` with `visible` of the span shown.
fn pan_for(start: f64, visible: f64) -> f32 {
    let room = 1.0 - visible;
    if room <= 0.0 {
        0.0
    } else {
        (start / room).clamp(0.0, 1.0) as f32
    }
}

/// The fraction (0 to 1) of the whole span at `x` points from the view's left.
#[must_use]
pub fn fraction_at(view: &TimelineView, x: f32) -> f64 {
    let (start, visible) = window(view);
    let width = f64::from(view.clamped().width.max(1.0));
    start + f64::from(x) / width * visible
}

/// `view` zoomed by `factor` about `pointer_x` (points from the view's left): the time under the
/// pointer stays under it unless the zoom or the pan reaches its limit.
#[must_use]
pub fn zoom_about(view: TimelineView, factor: f32, pointer_x: f32) -> TimelineView {
    let v = view.clamped();
    let width = v.width.max(1.0);
    let x = pointer_x.clamp(0.0, width);
    let under = fraction_at(&v, x);
    let factor = if factor.is_finite() && factor > 0.0 {
        factor
    } else {
        1.0
    };
    let zoom = (v.zoom * factor).clamp(1.0, MAX_ZOOM);
    let visible = 1.0 / f64::from(zoom);
    let start = under - f64::from(x / width) * visible;
    TimelineView {
        zoom,
        pan: pan_for(start, visible),
        width: v.width,
    }
}

/// `view` panned by a drag of `dx` points (positive: the content moves right, earlier time shows).
#[must_use]
pub fn pan_by(view: TimelineView, dx: f32) -> TimelineView {
    let v = view.clamped();
    let (start, visible) = window(&v);
    let width = f64::from(v.width.max(1.0));
    let start = start - f64::from(dx) / width * visible;
    TimelineView {
        pan: pan_for(start, visible),
        ..v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(row: usize, x0: f64, x1: f64, item: usize) -> PixelBar {
        PixelBar {
            row,
            x0,
            x1,
            t0: item as u64 * 10,
            t1: item as u64 * 10 + 5,
            item,
            count: 1,
        }
    }

    #[test]
    fn narrow_neighbours_merge_and_wide_bars_stay() {
        let bars = [
            bar(0, 0.0, 10.0, 0),
            bar(0, 10.2, 10.4, 1),
            bar(0, 10.5, 10.7, 2),
            bar(0, 11.1, 11.3, 3),
            bar(0, 20.0, 20.5, 4),
            bar(0, 25.0, 40.0, 5),
            bar(1, 25.1, 25.2, 6),
        ];
        let out = merge_subpixel_bars(&bars);
        let summary: Vec<(usize, usize, u32)> = out.iter().map(|b| (b.row, b.item, b.count)).collect();
        assert_eq!(
            summary,
            [(0, 0, 1), (0, 1, 3), (0, 4, 1), (0, 5, 1), (1, 6, 1)],
            "{out:?}"
        );
        assert_eq!(out[1].x1, 11.3);
        assert_eq!((out[1].t0, out[1].t1), (10, 35));
    }

    #[test]
    fn a_bar_starting_in_a_used_column_joins_it() {
        // A narrow bar then a wide one starting in the same pixel column.
        let out = merge_subpixel_bars(&[bar(0, 3.1, 3.4, 0), bar(0, 3.6, 9.0, 1)]);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].count, out[0].x1), (2, 9.0));
    }

    #[test]
    fn the_cache_hits_on_the_same_key_and_builds_on_a_new_one() {
        let mut cache: LayoutCache<(u64, u32), Vec<u32>> = LayoutCache::default();
        let a = cache.get_or_build((1, 2), || vec![1]);
        let b = cache.get_or_build((1, 2), || vec![2]);
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!((cache.builds(), cache.hits()), (1, 1));
        let c = cache.get_or_build((2, 2), || vec![3]);
        assert_eq!(*c, vec![3]);
        assert_eq!((cache.builds(), cache.hits()), (2, 1));
        cache.invalidate();
        let _ = cache.get_or_build((2, 2), || vec![4]);
        assert_eq!(cache.builds(), 3);
        assert!(format!("{cache:?}").contains("builds: 3"));
    }

    #[test]
    fn zoom_keeps_the_time_under_the_pointer_and_pan_moves_the_window() {
        let view = TimelineView {
            zoom: 2.0,
            pan: 0.5,
            width: 1000.0,
        };
        let before = fraction_at(&view, 300.0);
        let zoomed = zoom_about(view, 4.0, 300.0);
        assert_eq!(zoomed.zoom, 8.0);
        assert!((fraction_at(&zoomed, 300.0) - before).abs() < 1e-6, "{zoomed:?}");
        let out = zoom_about(zoomed, 0.25, 300.0);
        assert!((fraction_at(&out, 300.0) - before).abs() < 1e-6, "{out:?}");
        // Fully zoomed out, the pan is 0.
        assert_eq!(zoom_about(view, 0.01, 10.0).pan, 0.0);
        // A drag of a whole width moves the window by one visible span.
        let left = pan_by(zoomed, 1000.0);
        let shift = fraction_at(&zoomed, 0.0) - fraction_at(&left, 0.0);
        assert!((shift - 1.0 / 8.0).abs() < 1e-6, "{shift}");
        // Panning stops at the ends.
        assert_eq!(pan_by(zoomed, 1.0e9).pan, 0.0);
        assert_eq!(pan_by(zoomed, -1.0e9).pan, 1.0);
        // Bad input keeps the view usable.
        assert_eq!(zoom_about(view, f32::NAN, 0.0).zoom, 2.0);
    }
}
