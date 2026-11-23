// SPDX-License-Identifier: MPL-2.0
//! Copy, cut and paste of a node selection, and align / distribute (spec 80 § 12, ADR-9700).
//!
//! The clipboard is RON text ([`MatGraphClip`]): the copied nodes and their positions relative
//! to the selection's top-left corner. Pasting renames a name the document already has
//! (`name_2`, `name_3`, …) and the copies' references follow the renames; a reference to a node
//! outside the copied set is left unwired (`""`), so a paste never wires into whatever node
//! happens to share a name in the target tab.

use std::collections::{BTreeMap, BTreeSet};

use regia_matgraph::Node;
use serde::{Deserialize, Serialize};

use super::model::{MatGraphDoc, valid_node_name};

/// The clipboard text's version.
pub const CLIP_FORMAT_VERSION: u32 = 1;

/// What copy puts on the clipboard (as RON).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatGraphClip {
    /// [`CLIP_FORMAT_VERSION`].
    pub format_version: u32,
    /// The copied nodes by name.
    pub nodes: BTreeMap<String, Node>,
    /// Each node's position relative to the selection's top-left corner.
    pub offsets: BTreeMap<String, [f32; 2]>,
}

/// The RON clipboard text of nodes `names` of `doc`.
///
/// # Errors
///
/// Nothing picked, or a name the document does not have.
pub fn copy_nodes(doc: &MatGraphDoc, names: &BTreeSet<String>) -> Result<String, String> {
    if names.is_empty() {
        return Err("nothing to copy: pick nodes first".into());
    }
    let positions = doc.positions();
    let mut nodes = BTreeMap::new();
    for name in names {
        let node = doc
            .graph
            .nodes
            .get(name)
            .ok_or_else(|| format!("no node `{name}`"))?;
        nodes.insert(name.clone(), node.clone());
    }
    let at = |name: &String| positions.get(name).copied().unwrap_or([0.0, 0.0]);
    let min = names
        .iter()
        .map(at)
        .fold([f32::MAX, f32::MAX], |m, p| [m[0].min(p[0]), m[1].min(p[1])]);
    let offsets = names
        .iter()
        .map(|n| {
            let p = at(n);
            (n.clone(), [p[0] - min[0], p[1] - min[1]])
        })
        .collect();
    let clip = MatGraphClip {
        format_version: CLIP_FORMAT_VERSION,
        nodes,
        offsets,
    };
    ron::ser::to_string_pretty(&clip, regia_core::ron_pretty()).map_err(|e| e.to_string())
}

/// A free name for `wanted` in `taken`: itself when free, else `wanted_2`, `wanted_3`, … (kept
/// within the 64-character name rule).
fn free_name(wanted: &str, taken: &BTreeSet<String>) -> String {
    if !taken.contains(wanted) && valid_node_name(wanted) {
        return wanted.to_string();
    }
    let base: String = wanted
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(56)
        .collect();
    let base = if base.is_empty() { "node".to_string() } else { base };
    (2u32..)
        .map(|n| format!("{base}_{n}"))
        .find(|n| !taken.contains(n))
        .unwrap_or(base)
}

/// Pastes clipboard `text` into `doc` with the selection's top-left corner at `at`. Returns
/// the copied name → the pasted name.
///
/// # Errors
///
/// The text is not a clipboard of this version, or a pasted position is not finite.
pub fn paste_nodes(
    doc: &mut MatGraphDoc,
    text: &str,
    at: [f32; 2],
) -> Result<BTreeMap<String, String>, String> {
    let clip: MatGraphClip =
        ron::from_str(text).map_err(|e| format!("the clipboard holds no material graph nodes: {e}"))?;
    if clip.format_version > CLIP_FORMAT_VERSION {
        return Err(format!(
            "the clipboard was written by a newer editor (version {})",
            clip.format_version
        ));
    }
    if !at.iter().all(|v| v.is_finite()) {
        return Err("paste position is not finite".into());
    }
    let mut taken: BTreeSet<String> = doc.graph.nodes.keys().cloned().collect();
    let mut renamed = BTreeMap::new();
    for name in clip.nodes.keys() {
        let new = free_name(name, &taken);
        taken.insert(new.clone());
        renamed.insert(name.clone(), new);
    }
    for (name, node) in &clip.nodes {
        let mut node = node.clone();
        node.map_inputs(|input| {
            *input = renamed.get(input.as_str()).cloned().unwrap_or_default();
        });
        if let Node::Sample { lod, .. } = &mut node
            && lod.as_deref() == Some("")
        {
            *lod = None;
        }
        let offset = clip.offsets.get(name).copied().unwrap_or([0.0, 0.0]);
        let pos = [at[0] + offset[0], at[1] + offset[1]];
        if !pos.iter().all(|v| v.is_finite()) {
            return Err(format!("`{name}` has no finite position"));
        }
        let new = &renamed[name];
        doc.graph.nodes.insert(new.clone(), node);
        doc.layout.insert(new.clone(), pos);
    }
    Ok(renamed)
}

/// An align or distribute operation over picked nodes' canvas positions (their top-left
/// corners).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrange {
    /// Every x to the smallest x.
    AlignLeft,
    /// Every x to the largest x.
    AlignRight,
    /// Every y to the smallest y.
    AlignTop,
    /// Every y to the largest y.
    AlignBottom,
    /// Every x to the mean x.
    AlignCenterX,
    /// Every y to the mean y.
    AlignCenterY,
    /// Even x spacing between the leftmost and the rightmost (three or more nodes).
    DistributeX,
    /// Even y spacing between the top and the bottom (three or more nodes).
    DistributeY,
}

impl Arrange {
    /// Every operation with its protocol name, in menu order.
    pub const ALL: [(Self, &'static str); 8] = [
        (Self::AlignLeft, "align_left"),
        (Self::AlignRight, "align_right"),
        (Self::AlignTop, "align_top"),
        (Self::AlignBottom, "align_bottom"),
        (Self::AlignCenterX, "align_center_x"),
        (Self::AlignCenterY, "align_center_y"),
        (Self::DistributeX, "distribute_x"),
        (Self::DistributeY, "distribute_y"),
    ];

    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(op, _)| *op == self)
            .map_or("align_left", |(_, n)| n)
    }

    /// Parses a protocol name.
    ///
    /// # Errors
    ///
    /// Unknown name (the message lists the valid ones).
    pub fn parse(name: &str) -> Result<Self, String> {
        Self::ALL
            .iter()
            .find(|(_, n)| *n == name)
            .map(|(op, _)| *op)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|(_, n)| *n).collect();
                format!("unknown arrange op `{name}` ({})", names.join(", "))
            })
    }
}

/// Aligns or distributes nodes `names` of `doc` (positions only).
///
/// # Errors
///
/// Fewer than two nodes (three to distribute), or a name the document does not have.
pub fn arrange(doc: &mut MatGraphDoc, names: &BTreeSet<String>, op: Arrange) -> Result<(), String> {
    let need = if matches!(op, Arrange::DistributeX | Arrange::DistributeY) {
        3
    } else {
        2
    };
    if names.len() < need {
        return Err(format!("{} needs at least {need} nodes", op.name()));
    }
    let positions = doc.positions();
    let mut picked = Vec::new();
    for name in names {
        let pos = positions
            .get(name)
            .copied()
            .ok_or_else(|| format!("no node `{name}`"))?;
        picked.push((name.clone(), pos));
    }
    let axis = usize::from(matches!(
        op,
        Arrange::AlignTop | Arrange::AlignBottom | Arrange::AlignCenterY | Arrange::DistributeY
    ));
    let values: Vec<f32> = picked.iter().map(|(_, p)| p[axis]).collect();
    let lo = values.iter().copied().fold(f32::MAX, f32::min);
    let hi = values.iter().copied().fold(f32::MIN, f32::max);
    let mean = values.iter().sum::<f32>() / values.len() as f32;
    match op {
        Arrange::DistributeX | Arrange::DistributeY => {
            picked.sort_by(|a, b| a.1[axis].total_cmp(&b.1[axis]).then_with(|| a.0.cmp(&b.0)));
            let step = (hi - lo) / (picked.len() - 1) as f32;
            for (i, (_, pos)) in picked.iter_mut().enumerate() {
                pos[axis] = lo + step * i as f32;
            }
        }
        _ => {
            let target = match op {
                Arrange::AlignLeft | Arrange::AlignTop => lo,
                Arrange::AlignRight | Arrange::AlignBottom => hi,
                _ => mean,
            };
            for (_, pos) in &mut picked {
                pos[axis] = target;
            }
        }
    }
    for (name, pos) in picked {
        doc.move_node(&name, pos)?;
    }
    Ok(())
}
