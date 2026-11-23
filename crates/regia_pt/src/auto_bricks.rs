// SPDX-License-Identifier: MPL-2.0
//! Automatic detail bricks (`.bake.ron` v1 `layout.auto_bricks`, ADR-279): fixed boxes (the
//! interiors) with their own spacing, then one brick per cluster of static meshes. Every
//! placement's world box is grown by `margin`, overlapping (or touching) boxes merge until none
//! overlap, and each cluster gets a [`detail_brick`] over its box. When all these bricks together
//! would hold more than `max_probes` probes, the cluster bricks' spacing grows by one factor until
//! they fit in what the interiors leave; only when the interiors alone (plus 8 probes per cluster)
//! exceed the budget does every brick coarsen by one common factor (ADR-357). Every box stays
//! covered. Inputs are sorted first and the clusters are sorted by box, so the result does not
//! depend on the order of the meshes.

#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};

use serde::Serialize;

use regia_asset::baked_gi::DetailBrick;

#[cfg(test)]
thread_local! {
    /// AABB overlap tests the latest [`cluster_boxes`] call performed on this thread (tests).
    static OVERLAP_TESTS: Cell<u64> = const { Cell::new(0) };
}

use crate::probe::detail_brick;

/// A cluster of mesh boxes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MeshCluster {
    /// Cluster box (the members' boxes grown by the margin), minimum corner.
    pub min: [f32; 3],
    /// Maximum corner.
    pub max: [f32; 3],
    /// Indices of the input boxes in the cluster, ascending.
    pub members: Vec<usize>,
}

/// Bricks placed by [`auto_bricks`].
#[derive(Clone, Debug, PartialEq)]
pub struct AutoBricks {
    /// One brick per fixed box (in input order), then one per cluster (in cluster order).
    pub bricks: Vec<DetailBrick>,
    /// The clusters, sorted by box.
    pub clusters: Vec<MeshCluster>,
    /// Spacing multiplier applied to the cluster bricks to fit `max_probes` (1 = as requested).
    pub scale: f32,
    /// Spacing multiplier applied to the fixed bricks (the interiors): 1 unless the interiors
    /// alone, plus 8 probes per cluster, exceed `max_probes`; then it equals [`Self::scale`].
    pub interior_scale: f32,
    /// The cluster spacing used (the requested one times [`Self::scale`]).
    pub spacing: f32,
    /// Probes in all the bricks.
    pub probes: usize,
}

fn overlaps(a: &MeshCluster, b: &MeshCluster) -> bool {
    #[cfg(test)]
    OVERLAP_TESTS.with(|n| n.set(n.get() + 1));
    (0..3).all(|k| a.min[k] <= b.max[k] && b.min[k] <= a.max[k])
}

fn box_order(a: &MeshCluster, b: &MeshCluster) -> std::cmp::Ordering {
    let key = |c: &MeshCluster| [c.min[0], c.min[1], c.min[2], c.max[0], c.max[1], c.max[2]];
    let (ka, kb) = (key(a), key(b));
    ka.iter()
        .zip(&kb)
        .map(|(x, y)| x.total_cmp(y))
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.members.cmp(&b.members))
}

/// Cells of a grown box along one axis before it is scanned against every live cluster instead
/// of the grid. Eight cells a side is 512 cells, a few multiples of the margin for a prop.
const GRID_MAX_SPAN: i32 = 8;

/// Merges `boxes` (`(min, max)` world boxes) grown by `margin` into non-overlapping clusters,
/// sorted by box. Non-finite or inverted boxes are skipped.
///
/// Overlapping boxes share a grid cell (the cell size is the margin, or 1 when there is none),
/// so a box is tested only against the clusters already in its cells. A union that grows is
/// queued again, because the grown box can reach a cluster neither member overlapped. A box
/// that would cover more than `GRID_MAX_SPAN` cells on an axis is tested against every live
/// cluster instead, and its bounds are updated before the next test so a chain merges in one pass.
#[must_use]
pub fn cluster_boxes(boxes: &[([f32; 3], [f32; 3])], margin: f32) -> Vec<MeshCluster> {
    #[cfg(test)]
    OVERLAP_TESTS.with(|n| n.set(0));
    let mut clusters = grown_clusters(boxes, margin);
    if clusters.len() <= 1 {
        return clusters;
    }
    let cell = if margin > 0.0 { margin } else { 1.0 };
    cluster_on_grid(&mut clusters, cell);
    for c in &mut clusters {
        c.members.sort_unstable();
    }
    clusters.sort_by(box_order);
    clusters
}

/// The old repeated sweep, kept so tests can check the grid merge against it.
#[cfg(test)]
fn cluster_boxes_pairwise(boxes: &[([f32; 3], [f32; 3])], margin: f32) -> Vec<MeshCluster> {
    let mut clusters = grown_clusters(boxes, margin);
    loop {
        let mut merged = false;
        let mut out: Vec<MeshCluster> = Vec::with_capacity(clusters.len());
        for c in clusters {
            if let Some(o) = out.iter_mut().find(|o| overlaps(o, &c)) {
                for k in 0..3 {
                    o.min[k] = o.min[k].min(c.min[k]);
                    o.max[k] = o.max[k].max(c.max[k]);
                }
                o.members.extend(c.members);
                merged = true;
            } else {
                out.push(c);
            }
        }
        clusters = out;
        if !merged {
            break;
        }
    }
    for c in &mut clusters {
        c.members.sort_unstable();
    }
    clusters.sort_by(box_order);
    clusters
}

fn grown_clusters(boxes: &[([f32; 3], [f32; 3])], margin: f32) -> Vec<MeshCluster> {
    let m = margin.max(0.0);
    let mut clusters: Vec<MeshCluster> = boxes
        .iter()
        .enumerate()
        .filter(|(_, (lo, hi))| (0..3).all(|k| lo[k].is_finite() && hi[k].is_finite() && lo[k] <= hi[k]))
        .map(|(i, (lo, hi))| MeshCluster {
            min: [lo[0] - m, lo[1] - m, lo[2] - m],
            max: [hi[0] + m, hi[1] + m, hi[2] + m],
            members: vec![i],
        })
        .collect();
    clusters.sort_by(box_order);
    clusters
}

struct GridSlot {
    cluster: MeshCluster,
    alive: bool,
    cells: Vec<(i32, i32, i32)>,
    wide: bool,
}

fn cluster_on_grid(clusters: &mut Vec<MeshCluster>, cell: f32) {
    let mut slots: Vec<GridSlot> = clusters
        .drain(..)
        .map(|cluster| GridSlot {
            cluster,
            alive: true,
            cells: Vec::new(),
            wide: false,
        })
        .collect();
    let n = slots.len();
    let mut grid: HashMap<(i32, i32, i32), Vec<usize>> = HashMap::new();
    let mut wide: Vec<usize> = Vec::new();
    let mut queued = vec![true; n];
    let mut queue: VecDeque<usize> = (0..n).collect();
    while let Some(i) = queue.pop_front() {
        queued[i] = false;
        if !slots[i].alive {
            continue;
        }
        unlink(&mut slots, i, &mut grid, &mut wide);
        let Some(cells) = grid_cells(slots[i].cluster.min, slots[i].cluster.max, cell) else {
            absorb_wide(&mut slots, i, &mut grid, &mut wide, &mut queue, &mut queued);
            continue;
        };
        if !absorb_cells(
            &mut slots,
            i,
            &cells,
            &mut grid,
            &mut wide,
            &mut queue,
            &mut queued,
        ) {
            continue;
        }
        // Re-check: a merge requeues and the bounds (and therefore the cells) changed.
        if queued[i] || !slots[i].alive {
            continue;
        }
        link_cells(&mut slots, i, cells, &mut grid);
    }
    *clusters = slots.into_iter().filter(|s| s.alive).map(|s| s.cluster).collect();
}

fn grid_cells(min: [f32; 3], max: [f32; 3], cell: f32) -> Option<Vec<(i32, i32, i32)>> {
    let mut ranges = [(0i32, 0i32); 3];
    for k in 0..3 {
        let a = min[k] / cell;
        let b = max[k] / cell;
        if !a.is_finite() || !b.is_finite() || a.abs() > 1.0e6 || b.abs() > 1.0e6 {
            return None;
        }
        let ia = a.floor() as i32;
        let ib = b.floor() as i32;
        if ib.saturating_sub(ia) >= GRID_MAX_SPAN {
            return None;
        }
        ranges[k] = (ia, ib);
    }
    let mut out = Vec::new();
    for z in ranges[2].0..=ranges[2].1 {
        for y in ranges[1].0..=ranges[1].1 {
            for x in ranges[0].0..=ranges[0].1 {
                out.push((x, y, z));
            }
        }
    }
    Some(out)
}

fn unlink(
    slots: &mut [GridSlot],
    index: usize,
    grid: &mut HashMap<(i32, i32, i32), Vec<usize>>,
    wide: &mut Vec<usize>,
) {
    let slot = &mut slots[index];
    if slot.wide {
        wide.retain(|&other| other != index);
        slot.wide = false;
    }
    for cell in slot.cells.drain(..) {
        if let Some(list) = grid.get_mut(&cell) {
            list.retain(|&other| other != index);
        }
    }
}

fn absorb(slots: &mut [GridSlot], keep: usize, drop: usize) {
    for k in 0..3 {
        slots[keep].cluster.min[k] = slots[keep].cluster.min[k].min(slots[drop].cluster.min[k]);
        slots[keep].cluster.max[k] = slots[keep].cluster.max[k].max(slots[drop].cluster.max[k]);
    }
    let taken = std::mem::take(&mut slots[drop].cluster.members);
    slots[keep].cluster.members.extend(taken);
    slots[drop].alive = false;
}

fn enqueue(queue: &mut VecDeque<usize>, queued: &mut [bool], index: usize) {
    if !queued[index] {
        queued[index] = true;
        queue.push_back(index);
    }
}

/// Merges `i` with any overlapping occupant of `cells` or the wide list.
/// `false` when `i` was merged away or must be scanned again with new bounds.
fn absorb_cells(
    slots: &mut [GridSlot],
    i: usize,
    cells: &[(i32, i32, i32)],
    grid: &mut HashMap<(i32, i32, i32), Vec<usize>>,
    wide: &mut Vec<usize>,
    queue: &mut VecDeque<usize>,
    queued: &mut [bool],
) -> bool {
    let mut hits = Vec::new();
    for cell in cells {
        if let Some(list) = grid.get(cell) {
            hits.extend_from_slice(list);
        }
    }
    hits.extend_from_slice(wide);
    hits.sort_unstable();
    hits.dedup();
    for j in hits {
        if j == i || !slots[i].alive || !slots[j].alive || !overlaps(&slots[i].cluster, &slots[j].cluster) {
            continue;
        }
        let keep = i.min(j);
        let drop = i.max(j);
        unlink(slots, drop, grid, wide);
        absorb(slots, keep, drop);
        if keep != i {
            unlink(slots, keep, grid, wide);
            enqueue(queue, queued, keep);
        } else {
            enqueue(queue, queued, i);
        }
        return false;
    }
    true
}

fn absorb_wide(
    slots: &mut [GridSlot],
    i: usize,
    grid: &mut HashMap<(i32, i32, i32), Vec<usize>>,
    wide: &mut Vec<usize>,
    queue: &mut VecDeque<usize>,
    queued: &mut [bool],
) {
    let mut j = 0;
    while j < slots.len() {
        if j != i && slots[j].alive && slots[i].alive && overlaps(&slots[i].cluster, &slots[j].cluster) {
            let keep = i.min(j);
            let drop = i.max(j);
            unlink(slots, drop, grid, wide);
            absorb(slots, keep, drop);
            if keep != i {
                unlink(slots, keep, grid, wide);
                enqueue(queue, queued, keep);
                return;
            }
            // The grown box can now reach a cluster this pass already skipped. Start over.
            j = 0;
            continue;
        }
        j += 1;
    }
    if slots[i].alive && !queued[i] {
        slots[i].wide = true;
        wide.push(i);
    }
}

fn link_cells(
    slots: &mut [GridSlot],
    index: usize,
    cells: Vec<(i32, i32, i32)>,
    grid: &mut HashMap<(i32, i32, i32), Vec<usize>>,
) {
    slots[index].cells = cells;
    for cell in &slots[index].cells {
        grid.entry(*cell).or_default().push(index);
    }
}

/// A box to brick: minimum corner, maximum corner, probe spacing.
pub type BrickBox = ([f32; 3], [f32; 3], f32);

fn bricks_at(specs: &[BrickBox], scale: f32) -> (Vec<DetailBrick>, usize) {
    let bricks: Vec<DetailBrick> = specs
        .iter()
        .map(|(lo, hi, s)| detail_brick(*lo, *hi, s * scale))
        .collect();
    let probes = bricks
        .iter()
        .fold(0usize, |acc, b| acc.saturating_add(b.probe_count()));
    (bricks, probes)
}

/// Places one brick per `fixed` box at its own spacing, then one per cluster of `boxes`
/// ([`cluster_boxes`] with `margin`) at `spacing`. Over `max_probes`, the cluster spacing is
/// coarsened first, so the fixed bricks (interiors) keep theirs; when the fixed bricks alone leave
/// fewer than 8 probes per cluster, every spacing is coarsened by one common factor instead.
///
/// # Errors
///
/// A non-positive spacing (requested or fixed) or `max_probes`, or more bricks than `max_probes`
/// can hold at two probes per axis (8 each).
pub fn auto_bricks(
    boxes: &[([f32; 3], [f32; 3])],
    spacing: f32,
    margin: f32,
    max_probes: usize,
    fixed: &[BrickBox],
) -> Result<AutoBricks, String> {
    let _span = tracing::info_span!("pt_auto_bricks", boxes = boxes.len(), fixed = fixed.len()).entered();
    let positive = |s: f32| s > 0.0 && s.is_finite();
    if !positive(spacing) || !fixed.iter().all(|f| positive(f.2)) {
        return Err("spacing must be positive".into());
    }
    if max_probes == 0 {
        return Err("max_probes must be positive".into());
    }
    let clusters = cluster_boxes(boxes, margin);
    let specs: Vec<BrickBox> = fixed
        .iter()
        .copied()
        .chain(clusters.iter().map(|c| (c.min, c.max, spacing)))
        .collect();
    if specs.len().saturating_mul(8) > max_probes {
        return Err(format!(
            "{} bricks need at least {} probes (8 each); max_probes is {max_probes}",
            specs.len(),
            specs.len() * 8
        ));
    }
    // Interiors first (ADR-357): they keep their spacing while the clusters, coarsened alone, fit
    // in what the interiors leave. Only when they cannot (the interiors alone, plus 8 probes per
    // cluster, exceed the budget) does every brick coarsen by one common factor.
    let (fixed_bricks, fixed_probes) = bricks_at(fixed, 1.0);
    let cluster_specs = &specs[fixed.len()..];
    let cluster_floor = cluster_specs.len() * 8;
    if fixed_probes.saturating_add(cluster_floor) <= max_probes {
        let (cluster_bricks, cluster_probes, scale) = fit(cluster_specs, max_probes - fixed_probes)?;
        let mut bricks = fixed_bricks;
        bricks.extend(cluster_bricks);
        return Ok(AutoBricks {
            bricks,
            clusters,
            scale,
            interior_scale: 1.0,
            spacing: spacing * scale,
            probes: fixed_probes + cluster_probes,
        });
    }
    let (bricks, probes, scale) = fit(&specs, max_probes)?;
    Ok(AutoBricks {
        bricks,
        clusters,
        scale,
        interior_scale: scale,
        spacing: spacing * scale,
        probes,
    })
}

/// The bricks of `specs` with every spacing grown by one common factor until they hold at most
/// `budget` probes: `(bricks, probes, factor)`. Each round scales the spacings by the cube root
/// of the overshoot (at least 2 %); the count is monotone in the spacing and reaches 8 per
/// brick, which the caller checked fits.
fn fit(specs: &[BrickBox], budget: usize) -> Result<(Vec<DetailBrick>, usize, f32), String> {
    let mut scale = 1.0f32;
    let (mut bricks, mut probes) = bricks_at(specs, scale);
    for _ in 0..256 {
        if probes <= budget {
            break;
        }
        let ratio = (probes as f64 / budget.max(1) as f64).cbrt().max(1.02) as f32;
        scale *= ratio;
        (bricks, probes) = bricks_at(specs, scale);
    }
    if probes > budget {
        return Err(format!(
            "{probes} probes at {scale}x the spacing still exceed the budget of {budget}"
        ));
    }
    Ok((bricks, probes, scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn covers(b: &DetailBrick, lo: [f32; 3], hi: [f32; 3]) -> bool {
        (0..3).all(|k| {
            let end = b.origin[k] + b.spacing[k] * (b.counts[k] - 1) as f32;
            b.origin[k] <= lo[k] + 1e-4 && end >= hi[k] - 1e-4
        })
    }

    fn sample_boxes() -> Vec<([f32; 3], [f32; 3])> {
        vec![
            ([0.0, 0.0, 0.0], [2.0, 2.0, 2.0]),
            ([2.5, 0.0, 0.0], [4.0, 1.0, 1.0]), // within 2 × margin of the first: merges
            ([50.0, 0.0, 50.0], [52.0, 3.0, 51.0]),
            ([-30.0, 0.0, 5.0], [-29.0, 1.0, 6.0]),
            ([4.5, 0.0, 0.0], [5.0, 0.5, 0.5]), // chains onto the merged pair
        ]
    }

    #[test]
    fn overlapping_boxes_merge_and_far_ones_stay_apart() {
        let clusters = cluster_boxes(&sample_boxes(), 0.5);
        assert_eq!(clusters.len(), 3);
        // Sorted by box: the box at x = -30 first.
        assert_eq!(clusters[0].members, [3]);
        assert_eq!(clusters[1].members, [0, 1, 4]);
        assert_eq!(clusters[1].min, [-0.5, -0.5, -0.5]);
        assert_eq!(clusters[1].max, [5.5, 2.5, 2.5]);
        assert_eq!(clusters[2].members, [2]);
        // With no margin the 0.5 m gaps keep them apart.
        assert_eq!(cluster_boxes(&sample_boxes(), 0.0).len(), 5);
    }

    /// A bridge box is sorted between two clusters and only overlaps the second after the first
    /// has absorbed it. The member set is still one cluster.
    #[test]
    fn a_bridge_that_overlaps_only_after_growing_merges() {
        let boxes = [
            ([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]),
            ([0.0, 5.0, 0.0], [0.5, 6.0, 1.0]),
            ([0.6, 0.5, 0.0], [2.0, 5.5, 1.0]),
        ];
        let clusters = cluster_boxes(&boxes, 0.0);
        assert_eq!(clusters.len(), 1, "{clusters:?}");
        assert_eq!(clusters[0].members, vec![0, 1, 2]);
        assert_eq!(clusters[0].min, [0.0, 0.0, 0.0]);
        assert_eq!(clusters[0].max, [2.0, 6.0, 1.0]);
    }

    /// Disjoint props must not test every pair. The budget is linearithmic, not a timer.
    #[test]
    fn disjoint_boxes_overlap_tests_stay_linearithmic() {
        let n = 1024usize;
        let boxes: Vec<([f32; 3], [f32; 3])> = (0..n)
            .map(|i| {
                let x = i as f32 * 3.0;
                ([x, 0.0, 0.0], [x + 0.2, 0.2, 0.2])
            })
            .collect();
        OVERLAP_TESTS.with(|c| c.set(0));
        let clusters = cluster_boxes(&boxes, 0.5);
        let tests = OVERLAP_TESTS.with(|c| c.get());
        assert_eq!(clusters.len(), n, "no two grown boxes overlap");
        for (i, c) in clusters.iter().enumerate() {
            assert_eq!(c.members, vec![i]);
        }
        let budget = (n as u64) * 32;
        assert!(
            tests <= budget,
            "overlap tests {tests} exceed the {budget} budget for {n} disjoint boxes"
        );
    }

    #[test]
    fn grid_clusters_match_the_pairwise_oracle() {
        let samples = sample_boxes();
        assert_eq!(
            cluster_boxes(&samples, 0.5),
            cluster_boxes_pairwise(&samples, 0.5)
        );
        assert_eq!(
            cluster_boxes(&samples, 0.0),
            cluster_boxes_pairwise(&samples, 0.0)
        );
        let bridge = [
            ([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]),
            ([0.0, 5.0, 0.0], [0.5, 6.0, 1.0]),
            ([0.6, 0.5, 0.0], [2.0, 5.5, 1.0]),
        ];
        assert_eq!(cluster_boxes(&bridge, 0.0), cluster_boxes_pairwise(&bridge, 0.0));
        // Each box overlaps only the next; the union is one cluster either way.
        let chain: Vec<([f32; 3], [f32; 3])> = (0..24)
            .map(|i| {
                let x = i as f32 * 0.75;
                ([x, 0.0, 0.0], [x + 1.0, 1.0, 1.0])
            })
            .collect();
        assert_eq!(cluster_boxes(&chain, 0.0), cluster_boxes_pairwise(&chain, 0.0));
    }

    /// Placement is deterministic (any input order gives the same bricks) and every mesh box is
    /// inside a brick.
    #[test]
    fn auto_bricks_are_deterministic_and_cover_every_mesh_box() {
        let boxes = sample_boxes();
        let a = auto_bricks(&boxes, 1.0, 0.5, 1 << 20, &[]).expect("bricks");
        let mut reversed = boxes.clone();
        reversed.reverse();
        let b = auto_bricks(&reversed, 1.0, 0.5, 1 << 20, &[]).expect("bricks");
        assert_eq!(a.bricks, b.bricks);
        assert_eq!(a.spacing, 1.0);
        for (lo, hi) in &boxes {
            assert!(a.bricks.iter().any(|br| covers(br, *lo, *hi)), "{lo:?}..{hi:?}");
        }
        assert_eq!(
            a.probes,
            a.bricks.iter().map(DetailBrick::probe_count).sum::<usize>()
        );
    }

    #[test]
    fn an_unrepresentable_probe_count_is_over_budget() {
        // 4_194_303 steps at spacing 1 gives 2^22 probes per axis; the product wraps in usize.
        let boxes = vec![([0.0; 3], [4_194_303.0; 3])];
        let a = auto_bricks(&boxes, 1.0, 0.0, 1 << 20, &[]).expect("coarsened to fit");
        assert!(a.probes <= 1 << 20);
        assert!(a.probes > 0);
        assert!(a.scale > 1.0);
    }

    #[test]
    fn max_probes_coarsens_every_brick_and_keeps_coverage() {
        let boxes = sample_boxes();
        let fine = auto_bricks(&boxes, 0.25, 0.5, 1 << 20, &[]).expect("fine");
        let capped = auto_bricks(&boxes, 0.25, 0.5, 200, &[]).expect("capped");
        assert!(fine.probes > 200);
        assert!(capped.probes <= 200, "{}", capped.probes);
        assert!(capped.spacing > 0.25);
        assert_eq!(capped.bricks.len(), fine.bricks.len());
        for (lo, hi) in &boxes {
            assert!(capped.bricks.iter().any(|br| covers(br, *lo, *hi)));
        }
        // Three clusters need 24 probes at least.
        assert!(auto_bricks(&boxes, 1.0, 0.5, 23, &[]).is_err());
        assert!(auto_bricks(&boxes, 1.0, 0.5, 24, &[]).is_ok());
        assert!(auto_bricks(&boxes, 0.0, 0.5, 100, &[]).is_err());
        assert!(auto_bricks(&boxes, 1.0, 0.5, 0, &[]).is_err());
        assert!(
            auto_bricks(&[], 1.0, 0.5, 8, &[])
                .expect("empty")
                .bricks
                .is_empty()
        );
    }

    /// Fixed boxes (interiors) come first with their own spacing and share the probe budget:
    /// over `max_probes`, they coarsen by the same factor as the clusters and stay covered.
    #[test]
    fn fixed_boxes_come_first_and_share_the_budget() {
        let boxes = sample_boxes();
        let room: BrickBox = ([-10.0, 0.0, -10.0], [10.0, 5.0, 10.0], 0.5);
        let free = auto_bricks(&boxes, 1.0, 0.5, 1 << 20, &[room]).expect("free");
        assert_eq!(free.bricks.len(), 4);
        assert_eq!(free.bricks[0].counts, [41, 11, 41]);
        assert_eq!(free.scale, 1.0);
        let capped = auto_bricks(&boxes, 1.0, 0.5, 2000, &[room]).expect("capped");
        assert!(capped.probes <= 2000, "{}", capped.probes);
        assert!(capped.scale > 1.0);
        assert!((capped.spacing - capped.scale).abs() < 1e-6);
        assert!(covers(&capped.bricks[0], room.0, room.1));
        for (lo, hi) in &boxes {
            assert!(capped.bricks[1..].iter().any(|br| covers(br, *lo, *hi)));
        }
        // Four bricks need 32 probes at least.
        assert!(auto_bricks(&boxes, 1.0, 0.5, 31, &[room]).is_err());
        assert!(auto_bricks(&boxes, 1.0, 0.5, 100, &[(room.0, room.1, 0.0)]).is_err());
    }

    /// Review baked-light #4: a set piece added next to rooms that already use most of the
    /// budget coarsens only the clusters; the rooms keep their spacing (and their probe count)
    /// as long as the clusters can shrink to fit.
    #[test]
    fn clusters_coarsen_before_interiors() {
        let boxes = sample_boxes();
        let room: BrickBox = ([-10.0, 0.0, -10.0], [10.0, 5.0, 10.0], 0.5);
        let room_probes = 41 * 11 * 41;
        let free = auto_bricks(&boxes, 1.0, 0.5, 1 << 20, &[room]).expect("free");
        let cluster_probes = free.probes - room_probes;
        assert!(cluster_probes > 100, "{cluster_probes}");
        // 100 probes left for three clusters (24 at least): only they coarsen.
        let tight = auto_bricks(&boxes, 1.0, 0.5, room_probes + 100, &[room]).expect("tight");
        assert_eq!(tight.interior_scale, 1.0);
        assert_eq!(tight.bricks[0], free.bricks[0], "the room keeps its spacing");
        assert!(tight.scale > 1.0);
        assert!(tight.probes <= room_probes + 100, "{}", tight.probes);
        for (lo, hi) in &boxes {
            assert!(tight.bricks[1..].iter().any(|br| covers(br, *lo, *hi)));
        }
        // Exactly the 24-probe floor left: still only the clusters coarsen, to 8 probes each.
        let floor = auto_bricks(&boxes, 1.0, 0.5, room_probes + 24, &[room]).expect("floor");
        assert_eq!(floor.interior_scale, 1.0);
        assert!(floor.bricks[1..].iter().all(|b| b.probe_count() == 8));
        // One probe short of the floor: everything coarsens by the common factor.
        let shared = auto_bricks(&boxes, 1.0, 0.5, room_probes + 23, &[room]).expect("shared");
        assert!(shared.interior_scale > 1.0);
        assert_eq!(shared.interior_scale, shared.scale);
        assert!(shared.probes <= room_probes + 23);
        assert!(covers(&shared.bricks[0], room.0, room.1));
    }
}
