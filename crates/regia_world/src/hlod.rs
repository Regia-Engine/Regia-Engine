// SPDX-License-Identifier: MPL-2.0
//! The HLOD handoff (spec 78 § 11.2): each frame `HlodTree::select` is repaired so that every leaf is
//! covered exactly once by something that is actually ready.
//!
//! [`repair`] is the pure rule: a full cell that is not `Visible` falls back to its own proxy, then
//! to the nearest resident ancestor; a proxy that is not resident rises the same way, or opens into
//! its children when all of them are ready. What is left uncovered is a hole. Root proxies are always
//! resident, so with those loaded there are no holes.

use std::collections::{BTreeMap, BTreeSet};

use regia_scene::hlod::{HlodDraw, HlodDrawKind, HlodTree};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{HlodCut, HlodCutEntry};

/// The parent of every node, by index.
pub fn parents_of(tree: &HlodTree) -> Vec<Option<u32>> {
    let mut parents = vec![None; tree.nodes().len()];
    for (index, node) in tree.nodes().iter().enumerate() {
        for child in &node.children {
            if let Some(slot) = parents.get_mut(*child as usize) {
                *slot = Some(index as u32);
            }
        }
    }
    parents
}

/// The nearest ancestor of `node` with a resident proxy.
fn resident_ancestor(parents: &[Option<u32>], node: u32, resident: &dyn Fn(u32) -> bool) -> Option<u32> {
    let mut at = parents.get(node as usize).copied().flatten();
    while let Some(index) = at {
        if resident(index) {
            return Some(index);
        }
        at = parents.get(index as usize).copied().flatten();
    }
    None
}

/// Whether `node` opens into ready content: its own proxy, or children that all do. Appends the draws.
fn open_into(
    tree: &HlodTree,
    node: u32,
    visible: &dyn Fn(CellCoord) -> bool,
    resident: &dyn Fn(u32) -> bool,
    out: &mut Vec<HlodDraw>,
) -> bool {
    let Some(entry) = tree.node(node) else {
        return false;
    };
    if let Some(cell) = entry.cell {
        if visible(cell) {
            out.push(HlodDraw {
                node,
                kind: HlodDrawKind::Full,
            });
            return true;
        }
        return false;
    }
    entry.children.iter().all(|child| {
        if resident(*child) {
            out.push(HlodDraw {
                node: *child,
                kind: HlodDrawKind::Proxy,
            });
            true
        } else {
            open_into(tree, *child, visible, resident, out)
        }
    })
}

/// The leaf table a repair may use, and how many `leaves_under` walks it took.
pub(crate) struct RepairLeaves<'a> {
    /// Leaves under each node. Empty means a hole is counted by walking.
    pub counts: &'a [u32],
    /// Incremented once per `leaves_under` call.
    pub walks: &'a mut u64,
}

/// Repairs a selection (spec 78 § 11.2) and reports the cut, the holes, and the cells covered by a
/// proxy, whose members get `CellRenderHidden`.
pub fn repair(
    tree: &HlodTree,
    parents: &[Option<u32>],
    selection: &[HlodDraw],
    visible: &dyn Fn(CellCoord) -> bool,
    resident: &dyn Fn(u32) -> bool,
    fade: f32,
) -> HlodCut {
    let mut walks = 0;
    repair_counted(
        tree,
        parents,
        selection,
        visible,
        resident,
        fade,
        &mut RepairLeaves {
            counts: &[],
            walks: &mut walks,
        },
    )
}

/// Leaf cells under each node. Children are stored before their parent, so one forward pass is enough.
pub(crate) fn leaf_counts(tree: &HlodTree) -> Vec<u32> {
    let mut counts = vec![0u32; tree.nodes().len()];
    for (index, node) in tree.nodes().iter().enumerate() {
        if node.cell.is_some() {
            counts[index] = 1;
        } else {
            let mut total = 0u32;
            for child in &node.children {
                total = total.saturating_add(counts.get(*child as usize).copied().unwrap_or(0));
            }
            counts[index] = total;
        }
    }
    counts
}

/// Hole size of `node`. Uses the leaf table when it covers the node, and counts a `leaves_under`
/// walk otherwise.
fn hole_leaves(tree: &HlodTree, leaves: &mut RepairLeaves<'_>, node: u32) -> u64 {
    if let Some(count) = leaves.counts.get(node as usize) {
        u64::from(*count)
    } else {
        *leaves.walks += 1;
        tree.leaves_under(node).len() as u64
    }
}

/// `repair` that records each `leaves_under` walk on `leaves`.
///
/// An empty `leaves.counts` counts holes by walking.
pub(crate) fn repair_counted(
    tree: &HlodTree,
    parents: &[Option<u32>],
    selection: &[HlodDraw],
    visible: &dyn Fn(CellCoord) -> bool,
    resident: &dyn Fn(u32) -> bool,
    fade: f32,
    leaves: &mut RepairLeaves<'_>,
) -> HlodCut {
    let _span = tracing::info_span!("hlod_repair_leaves").entered();
    let mut kept: Vec<HlodDraw> = Vec::with_capacity(selection.len());
    let mut raised: BTreeSet<u32> = BTreeSet::new();
    let mut holes = 0u64;
    for draw in selection {
        let Some(node) = tree.node(draw.node) else {
            continue;
        };
        match draw.kind {
            HlodDrawKind::Full if node.cell.is_some_and(visible) => kept.push(*draw),
            HlodDrawKind::Full if resident(draw.node) => kept.push(HlodDraw {
                node: draw.node,
                kind: HlodDrawKind::Proxy,
            }),
            HlodDrawKind::Proxy if resident(draw.node) => kept.push(*draw),
            HlodDrawKind::Full | HlodDrawKind::Proxy => {
                if let Some(ancestor) = resident_ancestor(parents, draw.node, resident) {
                    raised.insert(ancestor);
                    continue;
                }
                let mut opened = Vec::new();
                if draw.kind == HlodDrawKind::Proxy
                    && open_into(tree, draw.node, visible, resident, &mut opened)
                {
                    kept.append(&mut opened);
                } else {
                    holes += hole_leaves(tree, leaves, draw.node);
                }
            }
        }
    }

    // A raised ancestor replaces everything under it; of two nested ones the higher wins.
    let under_raised = |mut node: u32, raised: &BTreeSet<u32>| {
        while let Some(parent) = parents.get(node as usize).copied().flatten() {
            if raised.contains(&parent) {
                return true;
            }
            node = parent;
        }
        false
    };
    let tops: Vec<u32> = raised
        .iter()
        .copied()
        .filter(|node| !under_raised(*node, &raised))
        .collect();
    let tops_set: BTreeSet<u32> = tops.iter().copied().collect();
    kept.retain(|draw| !under_raised(draw.node, &tops_set) && !tops_set.contains(&draw.node));
    kept.extend(tops.into_iter().map(|node| HlodDraw {
        node,
        kind: HlodDrawKind::Proxy,
    }));
    kept.sort_by_key(|d| d.node);

    let mut covered = BTreeMap::new();
    let mut entries = Vec::with_capacity(kept.len());
    for draw in kept {
        let cell = tree.node(draw.node).and_then(|n| n.cell);
        if draw.kind == HlodDrawKind::Proxy {
            *leaves.walks += 1;
            for leaf in tree.leaves_under(draw.node) {
                covered.insert(leaf, draw.node);
            }
        }
        entries.push(HlodCutEntry {
            node: draw.node,
            proxy: draw.kind == HlodDrawKind::Proxy,
            cell,
            fade,
        });
    }
    HlodCut {
        entries,
        holes,
        hidden: covered,
        leaving: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::hlod::{CellProxy, HlodConfig};
    use regia_scene::stream_grid::StreamGrid;

    fn tree(n: i64) -> HlodTree {
        let grid = StreamGrid::new(128.0).expect("grid");
        let proxies: Vec<CellProxy> = (0..n)
            .flat_map(|x| {
                (0..n).map(move |z| CellProxy {
                    cell: CellCoord::new(x, z),
                    min_y: 0.0,
                    max_y: 10.0,
                    bytes: 1000,
                })
            })
            .collect();
        HlodTree::build(&grid, HlodConfig::default(), &proxies).expect("tree")
    }

    /// Every leaf exactly once, counting holes as covered by nothing.
    fn coverage(tree: &HlodTree, cut: &HlodCut) -> BTreeMap<CellCoord, u32> {
        let mut seen = BTreeMap::new();
        for entry in &cut.entries {
            for leaf in tree.leaves_under(entry.node) {
                *seen.entry(leaf).or_insert(0) += 1;
            }
        }
        seen
    }

    /// A small deterministic generator (Numerical Recipes' LCG), so a failure repeats.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            self.0 >> 33
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn a_ready_selection_passes_through() {
        let tree = tree(4);
        let parents = parents_of(&tree);
        let selection = tree.select(&[[64.0, 0.0, 64.0]]).expect("select");
        let cut = repair(&tree, &parents, &selection, &|_| true, &|_| true, 0.0);
        assert_eq!(cut.entries.len(), selection.len());
        assert_eq!(cut.holes, 0);
        assert!(coverage(&tree, &cut).values().all(|n| *n == 1));
    }

    #[test]
    fn a_full_cell_that_is_not_visible_falls_back_to_its_own_proxy() {
        let tree = tree(4);
        let parents = parents_of(&tree);
        let selection: Vec<_> = tree
            .nodes()
            .iter()
            .enumerate()
            .filter(|(_, n)| n.level == 0)
            .map(|(i, _)| HlodDraw {
                node: i as u32,
                kind: HlodDrawKind::Full,
            })
            .collect();
        let late = CellCoord::new(1, 1);
        let cut = repair(&tree, &parents, &selection, &|c| c != late, &|_| true, 0.0);
        let proxies: Vec<_> = cut.entries.iter().filter(|e| e.proxy).collect();
        assert_eq!(proxies.len(), 1);
        assert_eq!(proxies[0].cell, Some(late));
        assert_eq!(cut.hidden.keys().copied().collect::<Vec<_>>(), vec![late]);
        assert!(coverage(&tree, &cut).values().all(|n| *n == 1));
    }

    #[test]
    fn a_full_cell_with_no_proxy_falls_back_to_the_nearest_resident_ancestor_and_covers_its_siblings() {
        let tree = tree(4);
        let parents = parents_of(&tree);
        let selection: Vec<_> = tree
            .nodes()
            .iter()
            .enumerate()
            .filter(|(_, n)| n.level == 0)
            .map(|(i, _)| HlodDraw {
                node: i as u32,
                kind: HlodDrawKind::Full,
            })
            .collect();
        let late = CellCoord::new(0, 0);
        let leaf = tree.leaf_for(late).expect("leaf");
        let parent = parents[leaf as usize].expect("parent");
        // Only merged nodes have proxies: the 2 x 2 block around the late cell draws one proxy.
        let merged = |n: u32| tree.node(n).is_some_and(|node| node.level > 0);
        let cut = repair(&tree, &parents, &selection, &|c| c != late, &merged, 0.0);
        assert!(cut.entries.iter().any(|e| e.proxy && e.node == parent));
        assert_eq!(cut.hidden.len(), 4, "the siblings hide behind the same proxy");
        assert!(coverage(&tree, &cut).values().all(|n| *n == 1));
        assert_eq!(cut.holes, 0);
    }

    #[test]
    fn a_proxy_that_is_not_resident_rises_or_opens_into_visible_children() {
        let tree = tree(2);
        let parents = parents_of(&tree);
        let root = tree.roots()[0];
        let selection = [HlodDraw {
            node: root,
            kind: HlodDrawKind::Proxy,
        }];
        let cut = repair(&tree, &parents, &selection, &|_| true, &|_| false, 0.0);
        assert!(cut.entries.iter().all(|e| !e.proxy), "opened into full cells");
        assert_eq!(cut.entries.len(), 4);
        assert_eq!(cut.holes, 0);
    }

    #[test]
    fn nothing_ready_counts_every_leaf_as_a_hole() {
        let tree = tree(2);
        let parents = parents_of(&tree);
        let selection = tree.select(&[[64.0, 0.0, 64.0]]).expect("select");
        let cut = repair(&tree, &parents, &selection, &|_| false, &|_| false, 0.0);
        assert!(cut.entries.is_empty());
        assert_eq!(cut.holes, 4);
    }

    #[test]
    fn every_leaf_is_covered_once_on_random_paths_with_roots_resident() {
        let tree = tree(32);
        let parents = parents_of(&tree);
        let roots: BTreeSet<u32> = tree.roots().iter().copied().collect();
        let mut rng = Lcg(0x5eed);
        for path in 0..200 {
            let visible: BTreeSet<CellCoord> = (0..rng.below(600))
                .map(|_| CellCoord::new(rng.below(32) as i64, rng.below(32) as i64))
                .collect();
            let resident: BTreeSet<u32> = (0..rng.below(800))
                .map(|_| rng.below(tree.nodes().len() as u64) as u32)
                .chain(roots.iter().copied())
                .collect();
            let is_visible = |c: CellCoord| visible.contains(&c);
            let is_resident = |n: u32| resident.contains(&n);
            for step in 0..5 {
                let eye = [
                    (path * 37 + step * 113) as f64 % 4096.0,
                    10.0,
                    (path * 91 + step * 59) as f64 % 4096.0,
                ];
                let selection = tree.select(&[eye]).expect("select");
                let cut = repair(&tree, &parents, &selection, &is_visible, &is_resident, 0.0);
                assert_eq!(cut.holes, 0, "path {path} step {step}");
                let seen = coverage(&tree, &cut);
                assert_eq!(seen.len(), tree.leaf_count(), "path {path} step {step}");
                assert!(seen.values().all(|n| *n == 1), "path {path} step {step}");
                for entry in &cut.entries {
                    if entry.proxy {
                        assert!(is_resident(entry.node));
                    } else {
                        assert!(entry.cell.is_some_and(is_visible));
                    }
                }
            }
        }
    }
}
