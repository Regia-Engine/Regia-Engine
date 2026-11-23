// SPDX-License-Identifier: MPL-2.0
// Tests of the region graph table and its dirty-cluster repair (ADR-8295).

use std::sync::Arc;

use regia_detsim::golden::scenarios::SplitMix64;

use super::*;
use crate::grid::{ExclusionDisc, GridSettings, HeightCells, NavStatic, Stamp};
use crate::search::region_neighbours;

/// A 256 x 256 grid of 5 m cells at cluster size `k`: rolling ground with a ridge (cliffs for some
/// classes), two closed walls with gaps.
fn world(k: usize) -> NavGrid {
    let (cols, rows) = (256usize, 256usize);
    let mut heights = vec![0.0f32; cols * rows];
    for z in 0..rows {
        for x in 0..cols {
            let ridge = if (100..104).contains(&x) && (40..200).contains(&z) {
                30.0
            } else {
                0.0
            };
            heights[z * cols + x] = ridge + (z % 32) as f32 * 0.5;
        }
    }
    let ground = HeightCells::centered(cols, rows, 5.0, heights);
    let s = Arc::new(GridSettings {
        cluster_cells: k,
        ..GridSettings::hover_ground_jet()
    });
    let stat = Arc::new(NavStatic::build(Some(&ground), s).expect("static"));
    let mut closed = vec![0u8; cols * rows];
    for z in 0..rows {
        if !(120..126).contains(&z) {
            closed[z * cols + 170] = 1;
        }
    }
    for x in 20..240 {
        if !(60..64).contains(&x) {
            closed[30 * cols + x] = 1;
        }
    }
    NavGrid::with_closed_cells(stat, closed).expect("grid")
}

/// Seeded obstacle churn: about 120 live discs; each tick adds, moves or lifts a few.
struct Churn {
    rng: SplitMix64,
    live: Vec<(u32, Stamp)>,
    next_id: u32,
}

impl Churn {
    fn new(seed: u64) -> Churn {
        Churn {
            rng: SplitMix64::new(seed),
            live: Vec::new(),
            next_id: 1,
        }
    }

    fn disc(&mut self) -> Stamp {
        let x = self.rng.signed() * 640.0;
        let z = self.rng.signed() * 640.0;
        let r = 1.0 + self.rng.unit() * 14.0;
        Stamp::from_world(x, z, r)
    }

    fn tick(&mut self) -> Vec<(u32, Stamp)> {
        let moves = 1 + self.rng.next_u64() % 4;
        for _ in 0..moves {
            let roll = self.rng.next_u64() % 10;
            if self.live.len() < 120 && roll < 5 {
                let id = self.next_id;
                self.next_id += 1;
                let d = self.disc();
                self.live.push((id, d));
            } else if !self.live.is_empty() && roll < 8 {
                let i = (self.rng.next_u64() % self.live.len() as u64) as usize;
                self.live[i].1 = self.disc();
            } else if !self.live.is_empty() {
                let i = (self.rng.next_u64() % self.live.len() as u64) as usize;
                self.live.swap_remove(i);
            }
        }
        // Hand the set over in a shuffled order: the grid sorts it.
        let mut out = self.live.clone();
        out.reverse();
        out
    }
}

/// ADR-8295 acceptance: over 600 ticks of seeded stamps, repairing only the clusters
/// `sync_stamps` reports dirty (and their neighbors) gives the table of a full rebuild every tick,
/// and so does [`RegionGraph::sync`], which finds them from the cluster generations. At clusters of
/// 8, 16 and 32.
#[test]
fn dirty_cluster_repair_equals_a_full_rebuild() {
    for k in [8usize, 16, 32] {
        let mut grid = world(k);
        let mut repaired = RegionGraph::build(&grid);
        let mut synced = repaired.clone();
        let mut churn = Churn::new(0x5eed_0600 + k as u64);
        let (mut recomputed, mut changed_ticks) = (0usize, 0usize);
        for tick in 0..600 {
            let sync = grid.sync_stamps(churn.tick());
            changed_ticks += usize::from(sync.changed);
            recomputed += repaired.repair(&grid, &sync.dirty_clusters);
            synced.sync(&grid);
            let fresh = RegionGraph::build(&grid);
            assert!(
                repaired == fresh,
                "cluster {k} tick {tick}: dirty repair differs from a rebuild"
            );
            assert!(
                synced == fresh,
                "cluster {k} tick {tick}: generation sync differs from a rebuild"
            );
            if tick == 300 {
                // A no-go disc relabels every cluster without a dirty list: sync still catches it.
                grid.set_exclusions(&[ExclusionDisc {
                    center_x: 0.0,
                    center_z: 0.0,
                    radius: 40.0,
                    below_y: 100.0,
                }]);
                let all = grid.clusters_x * grid.clusters_z;
                assert_eq!(synced.sync(&grid), all);
                repaired = synced.clone();
                assert!(synced == RegionGraph::build(&grid));
            }
        }
        let clusters = grid.clusters_x * grid.clusters_z;
        assert!(
            changed_ticks > 500,
            "the churn changes most ticks: {changed_ticks}"
        );
        // Local repair: far fewer clusters than a rebuild per changed tick (at 32 the 256 x 256 map
        // has only 64 clusters, so a disc and its neighbors are a larger share).
        let share = if k == 32 { 2 } else { 4 };
        assert!(
            recomputed < changed_ticks * clusters / share,
            "cluster {k}: {recomputed} recomputed over {changed_ticks} ticks of {clusters} clusters"
        );
        assert_eq!(synced.sync(&grid), 0, "nothing moved since the last sync");
    }
}

/// The table's lists are the border scan's lists, region by region, at every cluster size.
#[test]
fn region_graph_lists_equal_the_border_scan() {
    for k in [8usize, 16, 32] {
        let mut grid = world(k);
        let mut churn = Churn::new(7);
        for _ in 0..40 {
            grid.sync_stamps(churn.tick());
        }
        let graph = RegionGraph::build(&grid);
        assert!(graph.fits(&grid));
        let mr = grid.max_regions();
        let mut scan = Vec::new();
        let mut edges = 0usize;
        for (pi, profile) in grid.settings().all_profiles().enumerate() {
            for c in 0..grid.clusters_x * grid.clusters_z {
                let regions = grid.regions[pi][c].reps.len();
                assert_eq!(graph.cluster(profile, c).offsets.len(), regions + 1);
                for r in 0..regions {
                    let node = (c * mr + r) as u32;
                    region_neighbours(&grid, profile, pi, node, &mut scan);
                    assert_eq!(graph.neighbours(pi, node), scan.as_slice(), "k {k} node {node}");
                    edges += scan.len();
                }
            }
        }
        assert_eq!(graph.edge_count(), edges);
        assert!(edges > 0);
    }
}

/// A table handed a grid of another shape rebuilds itself.
#[test]
fn a_graph_of_another_shape_rebuilds() {
    let a = world(8);
    let b = world(16);
    let mut graph = RegionGraph::build(&a);
    assert!(!graph.fits(&b));
    assert_eq!(graph.sync(&b), b.clusters_x * b.clusters_z);
    assert!(graph == RegionGraph::build(&b));
    assert_eq!(graph.generation(), b.generation());
}
