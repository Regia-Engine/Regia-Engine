// SPDX-License-Identifier: MPL-2.0
//! The automatic layered layout of a conversation graph and the editor-side position sidecar
//! (ADR-3298).
//!
//! Layers are the breadth-first depth from the entry node (nodes the entry cannot reach start
//! their own walk, in step order, below the reachable ones). Inside a layer nodes are ordered by
//! the mean order of their parents in the layer above (the barycenter heuristic of layered graph
//! drawing), ties by step order. The layout reads only node ids and edges, never text, so
//! changing a line's text or speaker leaves every position where it was.
//!
//! Positions a writer dragged are kept in a sidecar file the editor owns
//! (`.regia/conversation_layout/<file name>.ron` under the project, or beside the file without
//! one); the conversation asset itself never stores positions.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::graph::ConvGraph;

/// Node width in layout units (points at zoom 1).
pub const NODE_W: f32 = 220.0;
/// Node height in layout units.
pub const NODE_H: f32 = 96.0;
/// Horizontal gap between nodes of one layer.
pub const GAP_X: f32 = 60.0;
/// Vertical gap between layers.
pub const GAP_Y: f32 = 70.0;

/// The current version of the sidecar format.
pub const LAYOUT_SIDECAR_VERSION: u32 = 1;

/// The sidecar file: dragged node positions by node id (top-left corner, y down).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutSidecar {
    /// Format version ([`LAYOUT_SIDECAR_VERSION`]).
    #[serde(default = "sidecar_version")]
    pub format_version: u32,
    /// Node id to position.
    #[serde(default)]
    pub positions: BTreeMap<String, (f32, f32)>,
}

fn sidecar_version() -> u32 {
    LAYOUT_SIDECAR_VERSION
}

impl LayoutSidecar {
    /// Reads a sidecar; a missing file is empty.
    ///
    /// # Errors
    ///
    /// The file exists but cannot be read or parsed, or has a newer version.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let sidecar: Self = ron::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if sidecar.format_version > LAYOUT_SIDECAR_VERSION {
            return Err(format!(
                "{}: format_version {} is newer than {LAYOUT_SIDECAR_VERSION}",
                path.display(),
                sidecar.format_version
            ));
        }
        Ok(sidecar)
    }

    /// The RON text.
    ///
    /// # Errors
    ///
    /// Serialization failed (not expected for this type).
    pub fn to_ron(&self) -> Result<String, String> {
        let mut out = self.clone();
        out.format_version = LAYOUT_SIDECAR_VERSION;
        ron::ser::to_string_pretty(&out, regia_core::ron_pretty()).map_err(|e| e.to_string())
    }
}

/// Where the sidecar of `file` lives: under `<root>/.regia/conversation_layout/` when a project
/// root holds the file, else under `.regia/conversation_layout/` beside the file.
#[must_use]
pub fn sidecar_path(file: &Path, project_root: Option<&Path>) -> PathBuf {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "conversation".into());
    let base = match project_root {
        Some(root) if file.starts_with(root) => root.to_path_buf(),
        _ => file.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    base.join(".regia")
        .join("conversation_layout")
        .join(format!("{name}.layout.ron"))
}

/// A laid-out graph: each node's top-left corner (y down), parallel to `ConvGraph::nodes`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layout {
    /// The positions.
    pub positions: Vec<(f32, f32)>,
    /// Each node's layer.
    pub layers: Vec<usize>,
    /// Whether each position came from the sidecar.
    pub pinned: Vec<bool>,
}

impl Layout {
    /// `[x0, y0, x1, y1]` around every node.
    #[must_use]
    pub fn bounds(&self) -> [f32; 4] {
        if self.positions.is_empty() {
            return [0.0, 0.0, NODE_W, NODE_H];
        }
        let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for &(x, y) in &self.positions {
            b[0] = b[0].min(x);
            b[1] = b[1].min(y);
            b[2] = b[2].max(x + NODE_W);
            b[3] = b[3].max(y + NODE_H);
        }
        b
    }
}

/// Lays `graph` out; positions in `sidecar` override the automatic ones for their nodes.
#[must_use]
pub fn layout(graph: &ConvGraph, sidecar: &LayoutSidecar) -> Layout {
    let _span = tracing::debug_span!(target: "regia_editor", "conversation_layout").entered();
    let n = graph.nodes.len();
    let mut out_edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in &graph.edges {
        if let Some(to) = e.to
            && !out_edges[e.from].contains(&to)
        {
            out_edges[e.from].push(to);
        }
    }
    // Layers: BFS depth from the entry; unreached nodes start their own walk below.
    let mut layer: Vec<Option<usize>> = vec![None; n];
    let mut base = 0;
    for start in 0..n {
        if layer[start].is_some() {
            continue;
        }
        let mut queue = VecDeque::from([start]);
        layer[start] = Some(base);
        let mut deepest = base;
        while let Some(node) = queue.pop_front() {
            let depth = layer[node].unwrap_or(base);
            deepest = deepest.max(depth);
            for &next in &out_edges[node] {
                if layer[next].is_none() {
                    layer[next] = Some(depth + 1);
                    queue.push_back(next);
                }
            }
        }
        base = deepest + 1;
    }
    let layers: Vec<usize> = layer.into_iter().map(|l| l.unwrap_or(0)).collect();
    let depth = layers.iter().copied().max().map_or(0, |d| d + 1);
    // Order inside each layer: barycenter of parents in the layer above, then step order.
    let mut parents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (from, outs) in out_edges.iter().enumerate() {
        for &to in outs {
            if layers[from] + 1 == layers[to] {
                parents[to].push(from);
            }
        }
    }
    let mut order = vec![0usize; n];
    let mut rows: Vec<Vec<usize>> = vec![Vec::new(); depth];
    for (d, out_row) in rows.iter_mut().enumerate() {
        let mut row: Vec<(f32, usize)> = (0..n)
            .filter(|&i| layers[i] == d)
            .map(|i| {
                let p = &parents[i];
                let key = if p.is_empty() {
                    f32::MAX
                } else {
                    p.iter().map(|&q| order[q] as f32).sum::<f32>() / p.len() as f32
                };
                (key, i)
            })
            .collect();
        row.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        for (k, &(_, i)) in row.iter().enumerate() {
            order[i] = k;
        }
        *out_row = row.into_iter().map(|(_, i)| i).collect();
    }
    let widest = rows.iter().map(Vec::len).max().unwrap_or(0) as f32;
    let mut positions = vec![(0.0, 0.0); n];
    for (d, row) in rows.iter().enumerate() {
        // Centre each row under the widest one.
        let offset = (widest - row.len() as f32) * 0.5 * (NODE_W + GAP_X);
        for (k, &i) in row.iter().enumerate() {
            positions[i] = (offset + k as f32 * (NODE_W + GAP_X), d as f32 * (NODE_H + GAP_Y));
        }
    }
    let mut pinned = vec![false; n];
    for (i, node) in graph.nodes.iter().enumerate() {
        if let Some(&p) = sidecar.positions.get(&node.id) {
            positions[i] = p;
            pinned[i] = true;
        }
    }
    Layout {
        positions,
        layers,
        pinned,
    }
}
