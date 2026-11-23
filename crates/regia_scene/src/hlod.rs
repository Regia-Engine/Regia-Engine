// SPDX-License-Identifier: MPL-2.0
//! The HLOD (hierarchical level of detail) tree: coarse proxies of groups of streaming cells.
//!
//! Each streaming cell may have a cheap proxy standing in for its full content when it is far
//! away. [`HlodTree::build`] merges those cell proxies into parent levels, `merge_factor x
//! merge_factor` children per parent on the cell grid, up to `max_levels` merge passes or until one
//! root remains. [`HlodTree::select`] then walks the tree for a set of viewers and returns what to
//! draw: the full cell content where the viewer is close, and the coarsest proxy that is still
//! small enough on screen elsewhere. See `book/agents/world-streaming.md`.
//!
//! # Screen size
//!
//! The screen size of a node is the fraction of the viewport height its bounding sphere covers.
//! For a perspective camera with vertical field of view `fov`, a sphere of radius `r` at distance
//! `d` projects to a diameter of `2r / (2 d tan(fov / 2))` of the viewport height, that is
//! `r / (d tan(fov / 2))`. A node's proxy is drawn instead of its children once that is at most
//! `screen_size_threshold`, which is the same as the viewer being at least
//! `switch_distance = r / (threshold * tan(fov / 2))` away. A parent's sphere contains its
//! children's, so a parent always switches at a distance at least as large as any child.
//!
//! The viewer distance used is the distance to the node's bounding box (zero inside it), which is
//! conservative: a viewer in or next to a group always gets the detailed children.
//!
//! A merge group holding a single node does not get a parent of its own; the node is carried up to
//! the next pass unchanged, so every parent has at least two children.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::stream_grid::{CellCoord, StreamGrid};

/// The most merge passes [`HlodConfig::max_levels`] may ask for.
pub const MAX_HLOD_LEVELS: u32 = 32;

/// Errors from building or walking an HLOD tree.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum HlodError {
    /// The vertical field of view is not in `(0, pi)` radians.
    #[error("field of view must be within (0, pi) radians, got {0}")]
    BadFov(f64),
    /// The screen-size threshold is not in `(0, 1]`.
    #[error("screen size threshold must be within (0, 1], got {0}")]
    BadThreshold(f64),
    /// The merge factor is outside `2..=16`.
    #[error("merge factor must be within 2..=16, got {0}")]
    BadMergeFactor(u32),
    /// More merge passes than [`MAX_HLOD_LEVELS`].
    #[error("max levels must be at most {MAX_HLOD_LEVELS}, got {0}")]
    TooManyLevels(u32),
    /// The proxy byte ratio is not in `(0, 1]`.
    #[error("proxy byte ratio must be within (0, 1], got {0}")]
    BadByteRatio(f64),
    /// A cell proxy has a non-finite or inverted height range.
    #[error("cell {0} has a non-finite or inverted height range")]
    BadBounds(CellCoord),
    /// Two proxies were given for the same cell.
    #[error("cell {0} appears twice")]
    DuplicateCell(CellCoord),
    /// No viewer positions were given.
    #[error("at least one viewer position is required")]
    NoViewers,
    /// A viewer coordinate is NaN or infinite.
    #[error("viewer position is not finite")]
    NonFiniteViewer,
}

/// How the tree is built and how screen size is judged.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HlodConfig {
    /// The camera's vertical field of view, radians, within `(0, pi)`.
    pub fov_y_rad: f64,
    /// A proxy is drawn once its screen size is at or below this fraction of the viewport height, within `(0, 1]`.
    pub screen_size_threshold: f64,
    /// Children per parent along each axis, `2..=16`.
    pub merge_factor: u32,
    /// The most merge passes, at most [`MAX_HLOD_LEVELS`]; 0 builds leaves only.
    pub max_levels: u32,
    /// A parent proxy costs this fraction of its children's proxies combined, within `(0, 1]`.
    pub proxy_byte_ratio: f64,
}

impl Default for HlodConfig {
    fn default() -> Self {
        Self {
            fov_y_rad: std::f64::consts::FRAC_PI_3,
            screen_size_threshold: 0.1,
            merge_factor: 2,
            max_levels: 8,
            proxy_byte_ratio: 0.25,
        }
    }
}

impl HlodConfig {
    /// Check every field is in range.
    pub fn validate(&self) -> Result<(), HlodError> {
        if !(self.fov_y_rad.is_finite() && self.fov_y_rad > 0.0 && self.fov_y_rad < std::f64::consts::PI) {
            return Err(HlodError::BadFov(self.fov_y_rad));
        }
        if !(self.screen_size_threshold.is_finite()
            && self.screen_size_threshold > 0.0
            && self.screen_size_threshold <= 1.0)
        {
            return Err(HlodError::BadThreshold(self.screen_size_threshold));
        }
        if !(2..=16).contains(&self.merge_factor) {
            return Err(HlodError::BadMergeFactor(self.merge_factor));
        }
        if self.max_levels > MAX_HLOD_LEVELS {
            return Err(HlodError::TooManyLevels(self.max_levels));
        }
        if !(self.proxy_byte_ratio.is_finite() && self.proxy_byte_ratio > 0.0 && self.proxy_byte_ratio <= 1.0)
        {
            return Err(HlodError::BadByteRatio(self.proxy_byte_ratio));
        }
        Ok(())
    }

    /// The viewer distance at which a node of bounding-sphere `radius` reaches the screen-size threshold.
    pub fn switch_distance(&self, radius: f64) -> f64 {
        radius / (self.screen_size_threshold * (self.fov_y_rad * 0.5).tan())
    }
}

/// The proxy of one streaming cell: its height range and the memory the proxy needs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CellProxy {
    /// The cell this proxy stands in for.
    pub cell: CellCoord,
    /// Lowest point of the cell's content, meters.
    pub min_y: f64,
    /// Highest point of the cell's content, meters.
    pub max_y: f64,
    /// Memory the proxy needs.
    pub bytes: u64,
}

/// A node of the tree: a cell proxy (a leaf) or a merge of children.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HlodNode {
    /// The merge pass that created the node; 0 for leaves.
    pub level: u32,
    /// The cell, for a leaf; `None` for a merged node.
    pub cell: Option<CellCoord>,
    /// Minimum corner of the bounding box, meters `[x, y, z]`.
    pub bounds_min: [f64; 3],
    /// Maximum corner of the bounding box, meters `[x, y, z]`.
    pub bounds_max: [f64; 3],
    /// Radius of the bounding sphere around the box, meters.
    pub radius: f64,
    /// Memory the node's proxy needs.
    pub proxy_bytes: u64,
    /// Indices of the children in [`HlodTree::nodes`], ascending; empty for a leaf.
    pub children: Vec<u32>,
    /// From this distance on, the node's proxy is drawn instead of its children.
    pub switch_distance: f64,
}

impl HlodNode {
    /// Distance from `p` to the node's bounding box; zero inside it.
    pub fn distance_to(&self, p: [f64; 3]) -> f64 {
        (0..3)
            .map(|a| {
                let d = (self.bounds_min[a] - p[a])
                    .max(p[a] - self.bounds_max[a])
                    .max(0.0);
                d * d
            })
            .sum::<f64>()
            .sqrt()
    }
}

/// What to draw for one selected node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HlodDrawKind {
    /// The node's merged proxy.
    Proxy,
    /// The full content of a leaf cell.
    Full,
}

/// One entry of a selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HlodDraw {
    /// Index into [`HlodTree::nodes`].
    pub node: u32,
    /// Proxy or the full cell.
    pub kind: HlodDrawKind,
}

/// A built HLOD tree.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HlodTree {
    nodes: Vec<HlodNode>,
    roots: Vec<u32>,
    config: Option<HlodConfig>,
}

struct Entry {
    index: u32,
    /// The node's address on the current merge level's grid.
    coord: CellCoord,
}

fn union_node(nodes: &[HlodNode], children: &[u32], level: u32, config: &HlodConfig) -> HlodNode {
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    let mut bytes = 0u64;
    for &c in children {
        let n = &nodes[c as usize];
        lo = std::array::from_fn(|a| lo[a].min(n.bounds_min[a]));
        hi = std::array::from_fn(|a| hi[a].max(n.bounds_max[a]));
        bytes = bytes.saturating_add(n.proxy_bytes);
    }
    let radius = half_diagonal(lo, hi);
    HlodNode {
        level,
        cell: None,
        bounds_min: lo,
        bounds_max: hi,
        radius,
        proxy_bytes: (bytes as f64 * config.proxy_byte_ratio).ceil() as u64,
        children: children.to_vec(),
        switch_distance: config.switch_distance(radius),
    }
}

fn half_diagonal(lo: [f64; 3], hi: [f64; 3]) -> f64 {
    let dx = hi[0] - lo[0];
    let dy = hi[1] - lo[1];
    let dz = hi[2] - lo[2];
    0.5 * (dx * dx + dy * dy + dz * dz).sqrt()
}

impl HlodTree {
    /// Build the tree for `proxies` on `grid`.
    ///
    /// The result depends only on the set of proxies, not on their order. No proxies give an empty tree.
    pub fn build(grid: &StreamGrid, config: HlodConfig, proxies: &[CellProxy]) -> Result<Self, HlodError> {
        let _span = tracing::trace_span!("hlod_build", cells = proxies.len()).entered();
        config.validate()?;
        let mut sorted: BTreeMap<CellCoord, CellProxy> = BTreeMap::new();
        for p in proxies {
            if !(p.min_y.is_finite() && p.max_y.is_finite() && p.min_y <= p.max_y) {
                return Err(HlodError::BadBounds(p.cell));
            }
            if sorted.insert(p.cell, *p).is_some() {
                return Err(HlodError::DuplicateCell(p.cell));
            }
        }

        let mut nodes: Vec<HlodNode> = Vec::with_capacity(sorted.len() * 2);
        let mut current: Vec<Entry> = Vec::with_capacity(sorted.len());
        for (cell, p) in &sorted {
            let lo2 = grid.cell_min(*cell);
            let hi2 = grid.cell_max(*cell);
            let lo = [lo2[0], p.min_y, lo2[1]];
            let hi = [hi2[0], p.max_y, hi2[1]];
            let radius = half_diagonal(lo, hi);
            current.push(Entry {
                index: nodes.len() as u32,
                coord: *cell,
            });
            nodes.push(HlodNode {
                level: 0,
                cell: Some(*cell),
                bounds_min: lo,
                bounds_max: hi,
                radius,
                proxy_bytes: p.bytes,
                children: Vec::new(),
                switch_distance: config.switch_distance(radius),
            });
        }

        let factor = i64::from(config.merge_factor);
        for level in 1..=config.max_levels {
            if current.len() <= 1 {
                break;
            }
            let mut groups: BTreeMap<CellCoord, Vec<u32>> = BTreeMap::new();
            for e in &current {
                let key = CellCoord::new(e.coord.x.div_euclid(factor), e.coord.z.div_euclid(factor));
                groups.entry(key).or_default().push(e.index);
            }
            let mut next = Vec::with_capacity(groups.len());
            for (key, members) in groups {
                match members.as_slice() {
                    [only] => next.push(Entry {
                        index: *only,
                        coord: key,
                    }),
                    _ => {
                        let node = union_node(&nodes, &members, level, &config);
                        next.push(Entry {
                            index: nodes.len() as u32,
                            coord: key,
                        });
                        nodes.push(node);
                    }
                }
            }
            current = next;
        }

        let roots = current.iter().map(|e| e.index).collect();
        Ok(Self {
            nodes,
            roots,
            config: Some(config),
        })
    }

    /// All nodes: leaves first in cell order, then each merge pass in order.
    pub fn nodes(&self) -> &[HlodNode] {
        &self.nodes
    }

    /// One node by index.
    pub fn node(&self, index: u32) -> Option<&HlodNode> {
        self.nodes.get(index as usize)
    }

    /// The top-level nodes, in cell order of their position on the top grid.
    pub fn roots(&self) -> &[u32] {
        &self.roots
    }

    /// Number of leaf nodes (cells with a proxy).
    pub fn leaf_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.cell.is_some()).count()
    }

    /// The highest merge pass that created a node; 0 when there are only leaves.
    pub fn max_level(&self) -> u32 {
        self.nodes.iter().map(|n| n.level).max().unwrap_or(0)
    }

    /// The index of the leaf for `cell`.
    pub fn leaf_for(&self, cell: CellCoord) -> Option<u32> {
        // Leaves are first and sorted by cell.
        let leaves = self.nodes.partition_point(|n| n.cell.is_some());
        self.nodes[..leaves]
            .binary_search_by(|n| n.cell.cmp(&Some(cell)))
            .ok()
            .map(|i| i as u32)
    }

    /// The configuration the tree was built with (`None` for an empty default tree).
    pub fn config(&self) -> Option<&HlodConfig> {
        self.config.as_ref()
    }

    /// Choose what to draw for viewers at the given world positions `[x, y, z]`.
    ///
    /// Walks from the roots: a node whose nearest-viewer distance has reached its switch distance
    /// is drawn as a proxy; otherwise a leaf is drawn in full and a merged node is replaced by its
    /// children. Every leaf is covered by exactly one entry. The order is depth first in node
    /// order, so equal inputs give equal output.
    pub fn select(&self, viewers: &[[f64; 3]]) -> Result<Vec<HlodDraw>, HlodError> {
        let _span =
            tracing::trace_span!("hlod_select", nodes = self.nodes.len(), viewers = viewers.len()).entered();
        if viewers.is_empty() {
            return Err(HlodError::NoViewers);
        }
        if viewers.iter().any(|v| v.iter().any(|c| !c.is_finite())) {
            return Err(HlodError::NonFiniteViewer);
        }
        let mut out = Vec::new();
        let mut stack: Vec<u32> = self.roots.iter().rev().copied().collect();
        while let Some(index) = stack.pop() {
            let Some(node) = self.nodes.get(index as usize) else {
                continue;
            };
            let d = viewers
                .iter()
                .map(|v| node.distance_to(*v))
                .fold(f64::INFINITY, f64::min);
            if d >= node.switch_distance {
                out.push(HlodDraw {
                    node: index,
                    kind: HlodDrawKind::Proxy,
                });
            } else if node.children.is_empty() {
                out.push(HlodDraw {
                    node: index,
                    kind: HlodDrawKind::Full,
                });
            } else {
                stack.extend(node.children.iter().rev());
            }
        }
        Ok(out)
    }

    /// Memory the proxies named by `draws` need (full cells are not counted).
    pub fn proxy_bytes_of(&self, draws: &[HlodDraw]) -> u64 {
        draws
            .iter()
            .filter(|d| d.kind == HlodDrawKind::Proxy)
            .filter_map(|d| self.node(d.node))
            .fold(0u64, |acc, n| acc.saturating_add(n.proxy_bytes))
    }

    /// The leaf cells covered by a node, in node order.
    pub fn leaves_under(&self, index: u32) -> Vec<CellCoord> {
        let mut out = Vec::new();
        let mut stack = vec![index];
        while let Some(i) = stack.pop() {
            let Some(n) = self.nodes.get(i as usize) else {
                continue;
            };
            match n.cell {
                Some(c) => out.push(c),
                None => stack.extend(n.children.iter().rev()),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> StreamGrid {
        StreamGrid::new(100.0).expect("grid")
    }

    fn proxy(x: i64, z: i64) -> CellProxy {
        CellProxy {
            cell: CellCoord::new(x, z),
            min_y: 0.0,
            max_y: 50.0,
            bytes: 1000,
        }
    }

    fn block(w: i64, h: i64) -> Vec<CellProxy> {
        (0..w).flat_map(|x| (0..h).map(move |z| proxy(x, z))).collect()
    }

    fn build(cfg: HlodConfig, proxies: &[CellProxy]) -> HlodTree {
        HlodTree::build(&grid(), cfg, proxies).expect("build")
    }

    /// 90 degree field of view: tan(fov / 2) = 1, so switch distance = radius / threshold.
    fn cfg90() -> HlodConfig {
        HlodConfig {
            fov_y_rad: std::f64::consts::FRAC_PI_2,
            screen_size_threshold: 0.1,
            ..HlodConfig::default()
        }
    }

    #[test]
    fn config_validation_names_each_bad_field() {
        let ok = HlodConfig::default();
        assert!(ok.validate().is_ok());
        for fov in [0.0, -1.0, std::f64::consts::PI, f64::NAN] {
            assert!(matches!(
                HlodConfig { fov_y_rad: fov, ..ok }.validate(),
                Err(HlodError::BadFov(_))
            ));
        }
        for t in [0.0, 1.5, -0.1, f64::NAN] {
            assert!(matches!(
                HlodConfig {
                    screen_size_threshold: t,
                    ..ok
                }
                .validate(),
                Err(HlodError::BadThreshold(_))
            ));
        }
        for f in [0, 1, 17] {
            assert_eq!(
                HlodConfig {
                    merge_factor: f,
                    ..ok
                }
                .validate(),
                Err(HlodError::BadMergeFactor(f))
            );
        }
        assert_eq!(
            HlodConfig { max_levels: 33, ..ok }.validate(),
            Err(HlodError::TooManyLevels(33))
        );
        for r in [0.0, 1.1, f64::INFINITY] {
            assert!(matches!(
                HlodConfig {
                    proxy_byte_ratio: r,
                    ..ok
                }
                .validate(),
                Err(HlodError::BadByteRatio(_))
            ));
        }
        assert!(
            HlodTree::build(
                &grid(),
                HlodConfig {
                    merge_factor: 1,
                    ..ok
                },
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn empty_input_gives_an_empty_tree() {
        let t = build(HlodConfig::default(), &[]);
        assert!(t.nodes().is_empty() && t.roots().is_empty());
        assert_eq!(t.select(&[[0.0, 0.0, 0.0]]), Ok(vec![]));
        assert_eq!(t.max_level(), 0);
    }

    #[test]
    fn bad_proxies_are_refused() {
        let cfg = HlodConfig::default();
        let dup = [proxy(1, 1), proxy(1, 1)];
        assert_eq!(
            HlodTree::build(&grid(), cfg, &dup),
            Err(HlodError::DuplicateCell(CellCoord::new(1, 1)))
        );
        let inverted = CellProxy {
            min_y: 5.0,
            max_y: 1.0,
            ..proxy(0, 0)
        };
        assert_eq!(
            HlodTree::build(&grid(), cfg, &[inverted]),
            Err(HlodError::BadBounds(CellCoord::new(0, 0)))
        );
        let nan = CellProxy {
            max_y: f64::NAN,
            ..proxy(0, 0)
        };
        assert!(matches!(
            HlodTree::build(&grid(), cfg, &[nan]),
            Err(HlodError::BadBounds(_))
        ));
    }

    #[test]
    fn a_leaf_switches_at_radius_over_threshold() {
        let t = build(cfg90(), &[proxy(0, 0)]);
        assert_eq!(t.nodes().len(), 1);
        let n = &t.nodes()[0];
        let radius = 0.5 * (100.0f64 * 100.0 + 50.0 * 50.0 + 100.0 * 100.0).sqrt();
        assert!((n.radius - radius).abs() < 1e-9);
        assert!((n.switch_distance - radius / 0.1).abs() < 1e-9);
        // Just inside: full; at the distance: proxy. The cell's x range is 0..100.
        let inside = [100.0 + n.switch_distance - 1.0, 25.0, 50.0];
        let at = [100.0 + n.switch_distance + 1e-6, 25.0, 50.0];
        assert_eq!(t.select(&[inside]).expect("ok")[0].kind, HlodDrawKind::Full);
        assert_eq!(t.select(&[at]).expect("ok")[0].kind, HlodDrawKind::Proxy);
    }

    #[test]
    fn a_two_by_two_block_merges_into_one_parent() {
        let t = build(cfg90(), &block(2, 2));
        assert_eq!(t.nodes().len(), 5);
        assert_eq!(t.roots(), &[4]);
        let p = &t.nodes()[4];
        assert_eq!(p.level, 1);
        assert_eq!(p.cell, None);
        assert_eq!(p.children, vec![0, 1, 2, 3]);
        assert_eq!(p.bounds_min, [0.0, 0.0, 0.0]);
        assert_eq!(p.bounds_max, [200.0, 50.0, 200.0]);
        assert_eq!(p.proxy_bytes, 1000, "4 x 1000 x 0.25");
        assert!(p.switch_distance > t.nodes()[0].switch_distance);
        assert_eq!(t.leaf_count(), 4);
        assert_eq!(t.max_level(), 1);
    }

    #[test]
    fn negative_cells_group_by_floor_division() {
        // With factor 2: -2 and -1 share parent -1; 0 and 1 share parent 0.
        let proxies = [proxy(-2, 0), proxy(-1, 0), proxy(0, 0), proxy(1, 0)];
        let t = build(
            HlodConfig {
                max_levels: 1,
                ..cfg90()
            },
            &proxies,
        );
        assert_eq!(t.roots().len(), 2);
        let left = t.node(t.roots()[0]).expect("root");
        assert_eq!(
            t.leaves_under(t.roots()[0]),
            vec![CellCoord::new(-2, 0), CellCoord::new(-1, 0)]
        );
        assert_eq!(left.bounds_min[0], -200.0);
        assert_eq!(
            t.leaves_under(t.roots()[1]),
            vec![CellCoord::new(0, 0), CellCoord::new(1, 0)]
        );
    }

    #[test]
    fn a_lone_cell_is_carried_up_until_it_has_a_partner() {
        let proxies = [proxy(0, 0), proxy(1, 0), proxy(8, 0)];
        let t = build(
            HlodConfig {
                max_levels: 8,
                ..cfg90()
            },
            &proxies,
        );
        // (0,0)+(1,0) merge at pass 1; (8,0) is alone until pass 4, where both sit under address 0.
        assert_eq!(t.roots().len(), 1, "everything ends under one root");
        let root = t.node(t.roots()[0]).expect("root");
        assert_eq!(root.children.len(), 2);
        assert!(root.level > 1);
        // The lone leaf is a direct child of the root: no single-child chain was created.
        let direct_leaf = root
            .children
            .iter()
            .filter_map(|c| t.node(*c))
            .filter(|n| n.cell.is_some())
            .count();
        assert_eq!(direct_leaf, 1);
        assert!(
            t.nodes()
                .iter()
                .all(|n| n.cell.is_some() || n.children.len() >= 2)
        );
    }

    #[test]
    fn max_levels_limits_the_merging() {
        let t = build(
            HlodConfig {
                max_levels: 0,
                ..cfg90()
            },
            &block(4, 4),
        );
        assert_eq!(t.nodes().len(), 16);
        assert_eq!(t.roots().len(), 16);
        let t = build(
            HlodConfig {
                max_levels: 1,
                ..cfg90()
            },
            &block(4, 4),
        );
        assert_eq!(t.roots().len(), 4);
        let t = build(
            HlodConfig {
                max_levels: 2,
                ..cfg90()
            },
            &block(4, 4),
        );
        assert_eq!(t.roots().len(), 1);
    }

    #[test]
    fn node_counts_follow_the_merge_factor() {
        let t = build(cfg90(), &block(16, 16));
        assert_eq!(t.nodes().len(), 256 + 64 + 16 + 4 + 1);
        assert_eq!(t.roots().len(), 1);
        let t = build(
            HlodConfig {
                merge_factor: 4,
                ..cfg90()
            },
            &block(16, 16),
        );
        assert_eq!(t.nodes().len(), 256 + 16 + 1);
        assert_eq!(t.max_level(), 2);
    }

    #[test]
    fn parents_never_switch_sooner_than_children() {
        let t = build(cfg90(), &block(8, 8));
        for n in t.nodes() {
            for c in &n.children {
                let child = t.node(*c).expect("child");
                assert!(n.switch_distance >= child.switch_distance);
                assert!(n.radius >= child.radius);
            }
        }
    }

    #[test]
    fn selection_goes_from_full_to_one_root_proxy_with_distance() {
        let t = build(cfg90(), &block(8, 8));
        let near = t.select(&[[50.0, 25.0, 50.0]]).expect("ok");
        assert!(near.iter().any(|d| d.kind == HlodDrawKind::Full));
        assert!(
            near.iter().any(|d| d.kind == HlodDrawKind::Proxy),
            "the far side of the block is merged"
        );
        let far = t.select(&[[400.0, 25.0, 400.0 + 1.0e6]]).expect("ok");
        assert_eq!(far.len(), 1);
        assert_eq!(far[0].kind, HlodDrawKind::Proxy);
        assert_eq!(far[0].node, t.roots()[0]);
        assert_eq!(
            t.proxy_bytes_of(&far),
            t.nodes()[t.roots()[0] as usize].proxy_bytes
        );
    }

    #[test]
    fn every_leaf_is_covered_exactly_once_wherever_the_viewer_is() {
        let t = build(cfg90(), &block(8, 8));
        for i in 0..60 {
            let v = [
                i as f64 * 37.0 - 300.0,
                (i % 7) as f64 * 40.0,
                i as f64 * 23.0 - 200.0,
            ];
            let draws = t.select(&[v]).expect("ok");
            let mut covered: Vec<CellCoord> = draws.iter().flat_map(|d| t.leaves_under(d.node)).collect();
            covered.sort();
            let all: Vec<CellCoord> = t.nodes().iter().filter_map(|n| n.cell).collect();
            assert_eq!(covered, all, "viewer {v:?}");
            for d in &draws {
                let n = t.node(d.node).expect("node");
                if d.kind == HlodDrawKind::Full {
                    assert!(n.cell.is_some(), "only leaves are drawn in full");
                }
            }
        }
    }

    #[test]
    fn detail_never_increases_as_the_viewer_recedes_along_a_line() {
        let t = build(cfg90(), &block(8, 8));
        let mut last = usize::MAX;
        // Recede from the block's near corner along a diagonal; draw count must not grow.
        for i in 0..80 {
            let d = i as f64 * 60.0;
            let n = t.select(&[[-d, 25.0, -d]]).expect("ok").len();
            assert!(n <= last, "distance {d}: {n} draws after {last}");
            last = n;
        }
        assert_eq!(last, 1);
    }

    #[test]
    fn the_nearest_viewer_decides() {
        let t = build(cfg90(), &block(8, 8));
        let far = [1.0e6, 25.0, 1.0e6];
        let solo = t.select(&[far]).expect("ok");
        let both = t.select(&[far, [50.0, 25.0, 50.0]]).expect("ok");
        assert_eq!(solo.len(), 1);
        assert!(both.len() > 1);
    }

    #[test]
    fn select_rejects_bad_viewers() {
        let t = build(cfg90(), &block(2, 2));
        assert_eq!(t.select(&[]), Err(HlodError::NoViewers));
        assert_eq!(t.select(&[[0.0, f64::NAN, 0.0]]), Err(HlodError::NonFiniteViewer));
        assert_eq!(
            t.select(&[[f64::INFINITY, 0.0, 0.0]]),
            Err(HlodError::NonFiniteViewer)
        );
    }

    #[test]
    fn the_tree_does_not_depend_on_input_order() {
        let mut proxies = block(5, 6);
        proxies.push(proxy(-7, 11));
        let a = build(cfg90(), &proxies);
        proxies.reverse();
        let b = build(cfg90(), &proxies);
        assert_eq!(a, b);
        let v = [[123.0, 10.0, 77.0]];
        assert_eq!(a.select(&v), b.select(&v));
    }

    #[test]
    fn leaf_lookup_finds_cells() {
        let t = build(cfg90(), &block(3, 3));
        for p in block(3, 3) {
            let i = t.leaf_for(p.cell).expect("present");
            assert_eq!(t.nodes()[i as usize].cell, Some(p.cell));
        }
        assert_eq!(t.leaf_for(CellCoord::new(9, 9)), None);
        assert_eq!(HlodTree::default().leaf_for(CellCoord::new(0, 0)), None);
    }

    #[test]
    fn a_larger_threshold_switches_to_proxies_sooner() {
        let near = build(
            HlodConfig {
                screen_size_threshold: 0.05,
                ..cfg90()
            },
            &block(4, 4),
        );
        let coarse = build(
            HlodConfig {
                screen_size_threshold: 0.4,
                ..cfg90()
            },
            &block(4, 4),
        );
        let v = [[-500.0, 25.0, -500.0]];
        let a = near.select(&v).expect("ok").len();
        let b = coarse.select(&v).expect("ok").len();
        assert!(b <= a);
        assert!(coarse.nodes()[0].switch_distance < near.nodes()[0].switch_distance);
    }
}
