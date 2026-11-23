// SPDX-License-Identifier: MPL-2.0
//! The canvas's fuzzy node search (spec 80 § 12, ADR-9700): Space or Tab on the canvas opens
//! it; dropping a wire from a pin into empty space opens it filtered to the kinds that can take
//! that wire, and the picked node is added already wired.

use regia_matgraph::Node;

use super::model::{DocMode, MatGraphDoc};

/// The pin a wire was dragged from when the search opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireFrom {
    /// The output of node `0`: the new node reads it on its first input.
    Output(String),
    /// Input `slot` of node `node`: the new node drives it.
    Input {
        /// The node whose input it is.
        node: String,
        /// The input slot.
        slot: String,
    },
    /// Surface output (or a function's output) `0`: the new node drives it.
    Surface(String),
}

/// The toolkit's fuzzy scorer (`crate::graph_kit::search::fuzzy_score`), under its material
/// graph name.
pub use crate::graph_kit::search::fuzzy_score;

/// Whether a node of `kind` can take a wire dragged from `from`: from an output it needs an
/// input slot (a `Call` takes arguments only once it has a function, so it is left out); into
/// an input or a surface output any kind works (every node has an output).
#[must_use]
pub fn accepts(kind: &str, from: Option<&WireFrom>) -> bool {
    match from {
        Some(WireFrom::Output(_)) => Node::default_of(kind).is_some_and(|n| !n.input_slots().is_empty()),
        _ => true,
    }
}

/// The node kinds matching `query`, best first (ties by the add menu's order), filtered to the
/// kinds that can take the wire `from` (see [`accepts`]). `Input` is offered only in a material
/// function (`function_doc`).
#[must_use]
pub fn search_kinds(query: &str, from: Option<&WireFrom>, function_doc: bool) -> Vec<&'static str> {
    crate::graph_kit::search::rank(
        Node::KINDS.iter().copied(),
        query,
        |k| (function_doc || *k != "Input") && accepts(k, from),
        |k| vec![(*k).to_string()],
    )
}

/// Adds a node of `kind` at `pos` and wires it to `from` (one change of `doc`). Returns its
/// name.
///
/// # Errors
///
/// Unknown kind, a kind that cannot take the wire, `Input` outside a function, or a wire the
/// document refuses (no such node or slot).
pub fn add_wired(
    doc: &mut MatGraphDoc,
    kind: &str,
    pos: [f32; 2],
    from: Option<&WireFrom>,
) -> Result<String, String> {
    let node = Node::default_of(kind).ok_or_else(|| format!("unknown node kind `{kind}`"))?;
    if kind == "Input" && !matches!(doc.mode, DocMode::Function(_)) {
        return Err("an `Input` node only exists inside a material function".into());
    }
    if !accepts(kind, from) {
        return Err(format!("a `{kind}` node has no input to take the wire"));
    }
    let first_slot = node.input_slots().first().map(|(slot, _)| slot.clone());
    let name = doc.add_node(None, node, Some(pos))?;
    match from {
        None => {}
        Some(WireFrom::Output(source)) => {
            let slot = first_slot.unwrap_or_default();
            doc.connect(source, &name, &slot)?;
        }
        Some(WireFrom::Input { node, slot }) => doc.connect(&name, node, slot)?,
        Some(WireFrom::Surface(slot)) => doc.set_output(slot, Some(&name))?,
    }
    Ok(name)
}
