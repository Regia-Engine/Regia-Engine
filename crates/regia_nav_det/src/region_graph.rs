// SPDX-License-Identifier: MPL-2.0
//! The coarse search's abstract graph as a table: per profile, per cluster, the regions next to each
//! region across the four cluster borders. Original code (ADR-8295), after Botea, Müller and
//! Schaeffer, "Near Optimal Hierarchical Path-Finding" (Journal of Game Development, 2004): the
//! abstract graph is built once and repaired locally when obstacles change.
//!
//! [`crate::search`] reads region neighbors straight off the cluster borders on every coarse
//! expansion. A [`RegionGraph`] holds the same lists, in the same order, so a search that reads
//! them pops the same nodes. When [`NavGrid::sync_stamps`] relabels clusters, only those clusters
//! and their four neighbors are recomputed ([`RegionGraph::repair`], or [`RegionGraph::sync`],
//! which finds them from [`NavGrid::cluster_generation`]); the result equals a fresh
//! [`RegionGraph::build`].
//!
//! ```
//! use std::sync::Arc;
//! use regia_nav_det::grid::{GridSettings, NavGrid, NavStatic, Stamp};
//! use regia_nav_det::region_graph::RegionGraph;
//!
//! # fn main() -> Result<(), regia_nav_det::grid::GridError> {
//! let settings = Arc::new(GridSettings::hover_ground_jet());
//! let mut grid = NavGrid::new(Arc::new(NavStatic::build(None, settings)?))?;
//! let mut graph = RegionGraph::build(&grid);
//! let sync = grid.sync_stamps(vec![(1, Stamp::from_world(0.0, 0.0, 12.0))]);
//! graph.repair(&grid, &sync.dirty_clusters);
//! assert_eq!(graph, RegionGraph::build(&grid));
//! # Ok(())
//! # }
//! ```

use super::grid::{NavGrid, Profile};
use super::search::border_pair;

/// One cluster's region lists for one profile: region `r`'s neighbors are
/// `targets[offsets[r]..offsets[r + 1]]` (coarse node ids `cluster * max_regions + label`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClusterEdges {
    /// Start of each region's list in `targets`, plus the end (regions + 1 entries).
    pub offsets: Vec<u32>,
    /// Neighbor node ids, region by region.
    pub targets: Vec<u32>,
}

/// The abstract (region) graph of a [`NavGrid`], kept current by local repair.
///
/// Not sim state: a host can rebuild it from the grid at any time and gets the same table. One
/// graph per grid; a graph handed a grid of another shape rebuilds itself in [`Self::sync`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionGraph {
    clusters_x: usize,
    clusters_z: usize,
    cluster_cells: usize,
    /// Per profile, per cluster.
    edges: Vec<Vec<ClusterEdges>>,
    /// Per cluster: the grid's cluster generation the lists were built at.
    seen: Vec<u64>,
    /// The grid generation at the last build, repair or sync.
    generation: u64,
}

impl RegionGraph {
    /// The full table for the grid's current labels.
    pub fn build(grid: &NavGrid) -> RegionGraph {
        let _span = tracing::trace_span!("det_region_graph_build").entered();
        let clusters = grid.clusters_x * grid.clusters_z;
        let profiles = grid.settings().profiles.len();
        let mut graph = RegionGraph {
            clusters_x: grid.clusters_x,
            clusters_z: grid.clusters_z,
            cluster_cells: grid.cluster_cells(),
            edges: vec![vec![ClusterEdges::default(); clusters]; profiles],
            seen: vec![0; clusters],
            generation: grid.generation(),
        };
        for c in 0..clusters {
            graph.recompute(grid, c);
        }
        graph
    }

    /// True when the table has the grid's shape (cluster counts and size, profiles).
    pub fn fits(&self, grid: &NavGrid) -> bool {
        self.clusters_x == grid.clusters_x
            && self.clusters_z == grid.clusters_z
            && self.cluster_cells == grid.cluster_cells()
            && self.edges.len() == grid.settings().profiles.len()
    }

    /// Recompute the given clusters (for example [`crate::grid::StampSync::dirty_clusters`])
    /// and their four neighbors, whose border lists name the relabeled regions. Returns how many
    /// clusters were recomputed. Ids past the grid are ignored. A graph that does not fit the grid
    /// is rebuilt whole.
    pub fn repair(&mut self, grid: &NavGrid, dirty_clusters: &[u32]) -> usize {
        if !self.fits(grid) {
            *self = RegionGraph::build(grid);
            return grid.clusters_x * grid.clusters_z;
        }
        let _span = tracing::trace_span!("det_region_graph_repair").entered();
        let (cx_n, cz_n) = (self.clusters_x, self.clusters_z);
        let mut mark = vec![false; cx_n * cz_n];
        for &c in dirty_clusters {
            let c = c as usize;
            if c >= mark.len() {
                continue;
            }
            let (x, z) = (c % cx_n, c / cx_n);
            mark[c] = true;
            if x + 1 < cx_n {
                mark[c + 1] = true;
            }
            if x > 0 {
                mark[c - 1] = true;
            }
            if z + 1 < cz_n {
                mark[c + cx_n] = true;
            }
            if z > 0 {
                mark[c - cx_n] = true;
            }
        }
        let mut done = 0usize;
        for (c, m) in mark.iter().enumerate() {
            if *m {
                self.recompute(grid, c);
                done += 1;
            }
        }
        self.generation = grid.generation();
        done
    }

    /// Bring the table up to the grid: repair every cluster whose
    /// [`NavGrid::cluster_generation`] moved since the table last saw it (whatever relabeled it:
    /// [`NavGrid::sync_stamps`], [`NavGrid::set_exclusions`] or a host's own
    /// [`NavGrid::relabel_cluster`]). Returns how many clusters were recomputed (0 when the grid's
    /// generation is unchanged).
    pub fn sync(&mut self, grid: &NavGrid) -> usize {
        if !self.fits(grid) {
            *self = RegionGraph::build(grid);
            return grid.clusters_x * grid.clusters_z;
        }
        if self.generation == grid.generation() {
            return 0;
        }
        let dirty: Vec<u32> = (0..self.seen.len())
            .filter(|&c| self.seen[c] != grid.cluster_generation(c))
            .map(|c| c as u32)
            .collect();
        self.repair(grid, &dirty)
    }

    /// The grid generation the table was last brought to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// One profile's lists for one cluster (an empty set past the table).
    pub fn cluster(&self, profile: Profile, cluster: usize) -> &ClusterEdges {
        static EMPTY: ClusterEdges = ClusterEdges {
            offsets: Vec::new(),
            targets: Vec::new(),
        };
        let pi = usize::from(profile.0).min(self.edges.len().saturating_sub(1));
        self.edges.get(pi).and_then(|p| p.get(cluster)).unwrap_or(&EMPTY)
    }

    /// Neighbors of a region node for a profile index, in the search's order.
    pub(crate) fn neighbours(&self, pi: usize, node: u32) -> &[u32] {
        let mr = self.cluster_cells * self.cluster_cells;
        let (c, r) = (node as usize / mr, node as usize % mr);
        let Some(e) = self.edges.get(pi).and_then(|p| p.get(c)) else {
            return &[];
        };
        match (e.offsets.get(r), e.offsets.get(r + 1)) {
            (Some(&a), Some(&b)) => &e.targets[a as usize..b as usize],
            _ => &[],
        }
    }

    /// Edges in the whole table (every profile), for counters and logs.
    pub fn edge_count(&self) -> usize {
        self.edges.iter().flatten().map(|e| e.targets.len()).sum()
    }

    /// Recompute one cluster's lists for every profile from the grid's labels: one pass over the
    /// four borders (E, W, S, N, border cell order) appending each region's new neighbors, which is
    /// the order `region_neighbours` in the search produces per region.
    fn recompute(&mut self, grid: &NavGrid, c: usize) {
        let side = grid.cluster_cells();
        let mr = grid.max_regions();
        let (cs, cz_n) = (grid.clusters_x, grid.clusters_z);
        let (stride, w, h) = (grid.width(), grid.width(), grid.height());
        let (ccx, ccz) = (c % cs, c / cs);
        for (pi, profile) in grid.settings().all_profiles().enumerate() {
            let regions = grid.regions[pi][c].reps.len();
            let labels = grid.labels_ref(pi);
            let blocked = labels.blocked();
            let mut lists: Vec<Vec<u32>> = vec![Vec::new(); regions];
            for (dx, dz) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                let ncx = ccx as i32 + dx;
                let ncz = ccz as i32 + dz;
                if ncx < 0 || ncz < 0 || ncx >= cs as i32 || ncz >= cz_n as i32 {
                    continue;
                }
                let nc = ncz as usize * cs + ncx as usize;
                for k in 0..side {
                    let (ax, az, bx, b_z) = border_pair(ccx, ccz, side, k, dx, dz);
                    if ax >= w || az >= h || bx >= w || b_z >= h {
                        continue;
                    }
                    let la = labels.raw(az * stride + ax);
                    if la == blocked || la as usize >= regions {
                        continue;
                    }
                    let lb = labels.raw(b_z * stride + bx);
                    if lb == blocked || !grid.can_step(profile, (az * stride + ax) as u32, dx, dz) {
                        continue;
                    }
                    let id = (nc * mr + lb as usize) as u32;
                    let list = &mut lists[la as usize];
                    if !list.contains(&id) {
                        list.push(id);
                    }
                }
            }
            let mut e = ClusterEdges {
                offsets: Vec::with_capacity(regions + 1),
                targets: Vec::new(),
            };
            for list in &lists {
                e.offsets.push(e.targets.len() as u32);
                e.targets.extend_from_slice(list);
            }
            e.offsets.push(e.targets.len() as u32);
            self.edges[pi][c] = e;
        }
        self.seen[c] = grid.cluster_generation(c);
    }
}

#[cfg(test)]
mod tests;
