// SPDX-License-Identifier: MPL-2.0
//! The material graph editor's document: a [`MaterialGraph`] plus canvas positions, the typed
//! edits the canvas and the agent methods make, the deterministic auto-layout, and the
//! compile diagnostics mapped to canvas targets (`docs/specs/80-matgraph-remainder.md` § 2).
//!
//! Pure data: no egui, no world. A graph that does not compile is still a valid document.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use regia_matgraph::{
    FunctionInput, FunctionSource, MATFUNC_FORMAT_VERSION, MaterialFunction, MaterialGraph, MaterialInstance,
    Node, ParamValue, SurfaceOutputs,
};

/// Horizontal distance between auto-layout columns (canvas points).
pub const LAYOUT_COLUMN: f32 = 230.0;
/// Vertical distance between auto-layout rows (canvas points).
pub const LAYOUT_ROW: f32 = 120.0;
/// A graph with more static switches than this gets a permutation warning (spec 80 § 6).
pub const PERMUTATION_WARN_SWITCHES: usize = 4;

/// The prefix of a refusal because the document is read-only (an instance shows its parent's
/// graph); the agent answers these with `matgraph_read_only`.
pub const READ_ONLY: &str = "read-only: ";

/// The open document: the graph and the canvas positions of its nodes.
#[derive(Clone, Debug, PartialEq)]
pub struct MatGraphDoc {
    /// The graph as it will be saved. For a function, its nodes are the function's nodes (the
    /// rest stays empty); for an instance, it is the parent graph (read-only).
    pub graph: MaterialGraph,
    /// Canvas positions by node name (editor-only; saved to the layout sidecar). Nodes without
    /// one are placed by [`MatGraphDoc::positions`].
    pub layout: BTreeMap<String, [f32; 2]>,
    /// What the document is: a material graph, a material function or an instance (ADR-2490).
    pub mode: DocMode,
    /// Comment frames and edge reroute points (editor-only; layout sidecar v2, ADR-9700).
    pub notes: LayoutNotes,
}

/// A comment frame on the canvas: a titled rectangle behind the nodes (layout sidecar v2).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CommentFrame {
    /// The comment text (its title).
    pub text: String,
    /// Top-left corner in canvas points.
    pub min: [f32; 2],
    /// Bottom-right corner in canvas points.
    pub max: [f32; 2],
    /// Fill color (sRGB).
    #[serde(default = "default_comment_color")]
    pub color: [u8; 3],
}

/// The default comment fill.
#[must_use]
pub fn default_comment_color() -> [u8; 3] {
    [70, 90, 110]
}

/// Reroute points of one edge, the wire into input `slot` of node `to` (layout sidecar v2).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Reroute {
    /// The node whose input the wire feeds.
    pub to: String,
    /// The input slot.
    pub slot: String,
    /// The points the wire passes through, in order (canvas points).
    pub points: Vec<[f32; 2]>,
}

/// The canvas annotations a document carries besides node positions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayoutNotes {
    /// Comment frames, in drawing order.
    pub comments: Vec<CommentFrame>,
    /// Edge reroute points, one entry per rerouted edge.
    pub reroutes: Vec<Reroute>,
}

fn finite2(p: [f32; 2]) -> bool {
    p.iter().all(|v| v.is_finite())
}

impl Default for MatGraphDoc {
    fn default() -> Self {
        Self {
            graph: MaterialGraph::new("untitled"),
            layout: BTreeMap::new(),
            mode: DocMode::Graph,
            notes: LayoutNotes::default(),
        }
    }
}

/// What a [`MatGraphDoc`] edits.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum DocMode {
    /// A `.matgraph.ron`.
    #[default]
    Graph,
    /// A `.matfunc.ron`: the function's nodes are `graph.nodes`; this is the rest of it.
    Function(FunctionMeta),
    /// A `.matinst.ron` over its parent graph (`graph`, read-only): only the overrides change.
    Instance(MaterialInstance),
}

/// A material function's name and signature while its nodes are on the canvas.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FunctionMeta {
    /// Display name.
    pub name: String,
    /// Inputs, in order (`Input` nodes read them).
    pub inputs: Vec<FunctionInput>,
    /// Outputs: output name → node name (`""` while unwired).
    pub outputs: BTreeMap<String, String>,
}

impl FunctionMeta {
    /// The function with `nodes`, at the current `.matfunc.ron` version.
    #[must_use]
    pub fn to_function(&self, nodes: &BTreeMap<String, Node>) -> MaterialFunction {
        MaterialFunction {
            format_version: MATFUNC_FORMAT_VERSION,
            name: self.name.clone(),
            inputs: self.inputs.clone(),
            nodes: nodes.clone(),
            outputs: self.outputs.clone(),
        }
    }
}

/// `value` in the variant `declared` uses when both have one type (a `Vec3` override of a
/// `Color` parameter stays a color), else `value`.
#[must_use]
pub fn as_declared(declared: ParamValue, value: ParamValue) -> ParamValue {
    match (declared, value) {
        (ParamValue::Color(_), ParamValue::Vec3(v)) => ParamValue::Color(v),
        (ParamValue::Vec3(_), ParamValue::Color(v)) => ParamValue::Vec3(v),
        _ => value,
    }
}

/// The parent graph as the instance draws it: parameter defaults, texture paths and switch
/// defaults replaced by the instance's overrides. Same code as the instance (overrides of
/// parameters and textures never change it; switch defaults pick the instance's permutation).
#[must_use]
pub fn instance_view(graph: &MaterialGraph, instance: &MaterialInstance) -> MaterialGraph {
    let mut view = graph.clone();
    for p in &mut view.params {
        if let Some(value) = instance.params.get(&p.name) {
            p.value = as_declared(p.value, *value);
        }
    }
    for t in &mut view.textures {
        if let Some(path) = instance.textures.get(&t.name) {
            t.path.clone_from(path);
        }
    }
    for s in &mut view.switches {
        if let Some(on) = instance.switches.get(&s.name) {
            s.default = *on;
        }
    }
    view
}

/// Node names: 1–64 characters of `[A-Za-z0-9_]` (the compiler's name rule; `/` is reserved
/// for function expansion).
#[must_use]
pub fn valid_node_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl MatGraphDoc {
    /// A document for `graph` with saved positions.
    #[must_use]
    pub fn new(graph: MaterialGraph, layout: BTreeMap<String, [f32; 2]>) -> Self {
        Self {
            graph,
            layout,
            mode: DocMode::Graph,
            notes: LayoutNotes::default(),
        }
    }

    /// A document for material function `function` with saved positions.
    #[must_use]
    pub fn function(function: MaterialFunction, layout: BTreeMap<String, [f32; 2]>) -> Self {
        let MaterialFunction {
            name,
            inputs,
            nodes,
            outputs,
            ..
        } = function;
        let mut graph = MaterialGraph::new(name.clone());
        graph.nodes = nodes;
        Self {
            graph,
            layout,
            mode: DocMode::Function(FunctionMeta {
                name,
                inputs,
                outputs,
            }),
            notes: LayoutNotes::default(),
        }
    }

    /// A document for `instance` over its `parent` graph (with the parent's positions).
    #[must_use]
    pub fn instance(
        parent: MaterialGraph,
        layout: BTreeMap<String, [f32; 2]>,
        instance: MaterialInstance,
    ) -> Self {
        Self {
            graph: parent,
            layout,
            mode: DocMode::Instance(instance),
            notes: LayoutNotes::default(),
        }
    }

    /// The function being edited (function documents only).
    #[must_use]
    pub fn function_meta(&self) -> Option<&FunctionMeta> {
        match &self.mode {
            DocMode::Function(meta) => Some(meta),
            _ => None,
        }
    }

    /// The material function the document saves (function documents only).
    #[must_use]
    pub fn to_function(&self) -> Option<MaterialFunction> {
        self.function_meta().map(|m| m.to_function(&self.graph.nodes))
    }

    /// The instance being edited (instance documents only).
    #[must_use]
    pub fn instance_meta(&self) -> Option<&MaterialInstance> {
        match &self.mode {
            DocMode::Instance(instance) => Some(instance),
            _ => None,
        }
    }

    /// The graph as it compiles and previews: an instance's parent with its overrides
    /// ([`instance_view`]); otherwise the graph itself.
    #[must_use]
    pub fn view_graph(&self) -> std::borrow::Cow<'_, MaterialGraph> {
        match &self.mode {
            DocMode::Instance(instance) => std::borrow::Cow::Owned(instance_view(&self.graph, instance)),
            _ => std::borrow::Cow::Borrowed(&self.graph),
        }
    }

    /// The names the canvas sink shows: the eight surface outputs, or a function's outputs.
    #[must_use]
    pub fn sink_slots(&self) -> Vec<String> {
        match &self.mode {
            DocMode::Function(meta) => meta.outputs.keys().cloned().collect(),
            _ => SurfaceOutputs::SLOTS.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    /// The node a sink slot reads (`None` when unwired or unknown).
    #[must_use]
    pub fn sink_get(&self, slot: &str) -> Option<&str> {
        match &self.mode {
            DocMode::Function(meta) => meta
                .outputs
                .get(slot)
                .map(String::as_str)
                .filter(|n| !n.is_empty()),
            _ => self.graph.output.get(slot),
        }
    }

    /// Checks a change from `before` to `self` against the mode: an instance's graph and
    /// layout never change (only its overrides do), and a function has no declarations or
    /// surface outputs of its own (they belong to the graph that calls it).
    ///
    /// # Errors
    ///
    /// A [`READ_ONLY`] message for an instance; a refusal for a function.
    pub fn check_mode_change(&self, before: &Self) -> Result<(), String> {
        match (&before.mode, &self.mode) {
            (DocMode::Instance(now), DocMode::Instance(was)) => {
                // A reparent (w12, ADR-7851) brings the new parent's graph and layout along.
                if now.parent != was.parent {
                    return Ok(());
                }
                if self.graph != before.graph || self.layout != before.layout || self.notes != before.notes {
                    return Err(format!(
                        "{READ_ONLY}this is an instance: the graph belongs to its parent; open the parent to edit it, or override its parameters, textures and switches"
                    ));
                }
                Ok(())
            }
            (DocMode::Function(_), DocMode::Function(_)) => {
                let (a, b) = (&self.graph, &before.graph);
                if a.params != b.params
                    || a.textures != b.textures
                    || a.switches != b.switches
                    || a.functions != b.functions
                {
                    return Err("a material function has no declarations: parameters, textures, switches and other functions belong to the graph that calls it".into());
                }
                if a.output != b.output || a.shading != b.shading {
                    return Err("a material function has named outputs, not surface outputs or shading: use its signature".into());
                }
                Ok(())
            }
            (a, b) if std::mem::discriminant(a) != std::mem::discriminant(b) => {
                Err("an edit cannot change what the document is".into())
            }
            _ => Ok(()),
        }
    }

    fn node_mut(&mut self, name: &str) -> Result<&mut Node, String> {
        self.graph
            .nodes
            .get_mut(name)
            .ok_or_else(|| format!("no node `{name}`"))
    }

    /// The first free name `<kind in snake case>_<n>` (n from 1).
    #[must_use]
    pub fn fresh_name(&self, kind: &str) -> String {
        let mut base = String::new();
        for (i, c) in kind.chars().enumerate() {
            if c.is_ascii_uppercase() && i > 0 {
                base.push('_');
            }
            base.push(c.to_ascii_lowercase());
        }
        if base.is_empty() {
            base.push_str("node");
        }
        (1..)
            .map(|n| format!("{base}_{n}"))
            .find(|n| !self.graph.nodes.contains_key(n))
            .unwrap_or(base)
    }

    /// Adds `node` as `name` (or a fresh name) at `pos`. Returns the name.
    ///
    /// # Errors
    ///
    /// The name is invalid or taken.
    pub fn add_node(
        &mut self,
        name: Option<&str>,
        node: Node,
        pos: Option<[f32; 2]>,
    ) -> Result<String, String> {
        let name = match name {
            Some(n) => n.to_string(),
            None => self.fresh_name(node.kind()),
        };
        if !valid_node_name(&name) {
            return Err(format!(
                "bad node name `{name}` (1–64 characters of A-Z, a-z, 0-9, _)"
            ));
        }
        if self.graph.nodes.contains_key(&name) {
            return Err(format!("a node `{name}` already exists"));
        }
        if pos.is_some_and(|p| !p.iter().all(|v| v.is_finite())) {
            return Err("position is not finite".into());
        }
        self.graph.nodes.insert(name.clone(), node);
        if let Some(pos) = pos {
            self.layout.insert(name.clone(), pos);
        }
        Ok(name)
    }

    /// Replaces node `name` with `node` (any kind; wiring is whatever `node` names).
    ///
    /// # Errors
    ///
    /// No such node.
    pub fn set_node(&mut self, name: &str, node: Node) -> Result<(), String> {
        *self.node_mut(name)? = node;
        Ok(())
    }

    /// Removes node `name`: inputs that read it are unwired (`""`, an error on those nodes
    /// until rewired), outputs it drove are cleared. Returns the nodes that were reading it.
    ///
    /// # Errors
    ///
    /// No such node.
    pub fn remove_node(&mut self, name: &str) -> Result<Vec<String>, String> {
        if self.graph.nodes.remove(name).is_none() {
            return Err(format!("no node `{name}`"));
        }
        self.layout.remove(name);
        self.notes.reroutes.retain(|r| r.to != name);
        let mut readers = Vec::new();
        for (other, node) in &mut self.graph.nodes {
            let mut hit = false;
            node.map_inputs(|input| {
                if *input == name {
                    input.clear();
                    hit = true;
                }
            });
            // `Sample`'s optional `lod` unwires to unset, not to "".
            if let Node::Sample { lod, .. } = node
                && lod.as_deref() == Some("")
            {
                *lod = None;
            }
            if hit {
                readers.push(other.clone());
            }
        }
        self.graph.output.rename(name, None);
        if let DocMode::Function(meta) = &mut self.mode {
            for node in meta.outputs.values_mut().filter(|n| *n == name) {
                node.clear();
            }
        }
        Ok(readers)
    }

    /// Unwires every wire of node `name` (w12 node menu "Disconnect all"): its own inputs, the
    /// inputs of nodes that read it and the outputs it drives. The node stays where it is.
    /// Returns how many inputs were unwired (its own plus its readers').
    ///
    /// # Errors
    ///
    /// No such node.
    pub fn disconnect_all(&mut self, name: &str) -> Result<usize, String> {
        let mut node = self
            .graph
            .nodes
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no node `{name}`"))?;
        let pos = self.layout.get(name).copied();
        let mut count = 0usize;
        node.map_inputs(|input| {
            if !input.is_empty() {
                input.clear();
                count += 1;
            }
        });
        if let Node::Sample { lod, .. } = &mut node
            && lod.is_some()
        {
            *lod = None;
        }
        count += self.remove_node(name)?.len();
        self.graph.nodes.insert(name.to_string(), node);
        if let Some(pos) = pos {
            self.layout.insert(name.to_string(), pos);
        }
        Ok(count)
    }

    /// Renames node `from` to `to`; every input and output that read `from` follows.
    ///
    /// # Errors
    ///
    /// No such node, or `to` is invalid or taken.
    pub fn rename_node(&mut self, from: &str, to: &str) -> Result<(), String> {
        if from == to {
            return Ok(());
        }
        if !valid_node_name(to) {
            return Err(format!(
                "bad node name `{to}` (1–64 characters of A-Z, a-z, 0-9, _)"
            ));
        }
        if self.graph.nodes.contains_key(to) {
            return Err(format!("a node `{to}` already exists"));
        }
        let node = self
            .graph
            .nodes
            .remove(from)
            .ok_or_else(|| format!("no node `{from}`"))?;
        self.graph.nodes.insert(to.to_string(), node);
        for node in self.graph.nodes.values_mut() {
            node.map_inputs(|input| {
                if *input == from {
                    *input = to.to_string();
                }
            });
        }
        self.graph.output.rename(from, Some(to));
        if let DocMode::Function(meta) = &mut self.mode {
            for node in meta.outputs.values_mut().filter(|n| *n == from) {
                *node = to.to_string();
            }
        }
        if let Some(pos) = self.layout.remove(from) {
            self.layout.insert(to.to_string(), pos);
        }
        for reroute in self.notes.reroutes.iter_mut().filter(|r| r.to == from) {
            reroute.to = to.to_string();
        }
        Ok(())
    }

    /// Adds a comment frame from `min` to `max` (corners in any order). Returns its index.
    ///
    /// # Errors
    ///
    /// A corner is not finite.
    pub fn add_comment(&mut self, text: &str, min: [f32; 2], max: [f32; 2]) -> Result<usize, String> {
        if !finite2(min) || !finite2(max) {
            return Err("a comment's corners must be finite".into());
        }
        self.notes.comments.push(CommentFrame {
            text: text.to_string(),
            min: [min[0].min(max[0]), min[1].min(max[1])],
            max: [min[0].max(max[0]), min[1].max(max[1])],
            color: default_comment_color(),
        });
        Ok(self.notes.comments.len() - 1)
    }

    /// Changes comment `index`: its text and/or its rectangle.
    ///
    /// # Errors
    ///
    /// No such comment, or a corner is not finite.
    pub fn set_comment(
        &mut self,
        index: usize,
        text: Option<&str>,
        rect: Option<([f32; 2], [f32; 2])>,
    ) -> Result<(), String> {
        let count = self.notes.comments.len();
        let comment = self
            .notes
            .comments
            .get_mut(index)
            .ok_or_else(|| format!("no comment {index} ({count} on the canvas)"))?;
        if let Some(text) = text {
            comment.text = text.to_string();
        }
        if let Some((min, max)) = rect {
            if !finite2(min) || !finite2(max) {
                return Err("a comment's corners must be finite".into());
            }
            comment.min = [min[0].min(max[0]), min[1].min(max[1])];
            comment.max = [min[0].max(max[0]), min[1].max(max[1])];
        }
        Ok(())
    }

    /// The nodes inside comment `index`: those whose canvas position (a node's top-left
    /// corner, [`MatGraphDoc::positions`]) lies in the frame, edges included (ADR-9970).
    /// Empty when there is no such comment.
    #[must_use]
    pub fn nodes_in_comment(&self, index: usize) -> Vec<String> {
        let Some(comment) = self.notes.comments.get(index) else {
            return Vec::new();
        };
        crate::graph_kit::frames::nodes_inside(comment.min, comment.max, self.positions())
    }

    /// Sets comment `index`'s rectangle (and text) and moves the nodes inside its old
    /// rectangle ([`MatGraphDoc::nodes_in_comment`]) by as far as its top-left corner moved.
    /// Returns the nodes moved.
    ///
    /// # Errors
    ///
    /// As [`MatGraphDoc::set_comment`].
    pub fn set_comment_moving_nodes(
        &mut self,
        index: usize,
        text: Option<&str>,
        rect: ([f32; 2], [f32; 2]),
    ) -> Result<Vec<String>, String> {
        let inside = self.nodes_in_comment(index);
        let before = self.notes.comments.get(index).map(|c| c.min);
        self.set_comment(index, text, Some(rect))?;
        let (Some(before), Some(after)) = (before, self.notes.comments.get(index).map(|c| c.min)) else {
            return Ok(Vec::new());
        };
        let delta = [after[0] - before[0], after[1] - before[1]];
        let positions = self.positions();
        for name in &inside {
            if let Some(p) = positions.get(name) {
                self.move_node(name, [p[0] + delta[0], p[1] + delta[1]])?;
            }
        }
        Ok(inside)
    }

    /// Removes comment `index`.
    ///
    /// # Errors
    ///
    /// No such comment.
    pub fn remove_comment(&mut self, index: usize) -> Result<(), String> {
        if index >= self.notes.comments.len() {
            return Err(format!(
                "no comment {index} ({} on the canvas)",
                self.notes.comments.len()
            ));
        }
        self.notes.comments.remove(index);
        Ok(())
    }

    /// Sets the reroute points of the wire into input `slot` of node `to` (an empty list
    /// removes them).
    ///
    /// # Errors
    ///
    /// No such node or slot, or a point is not finite.
    pub fn set_reroute(&mut self, to: &str, slot: &str, points: Vec<[f32; 2]>) -> Result<(), String> {
        let node = self
            .graph
            .nodes
            .get(to)
            .ok_or_else(|| format!("no node `{to}`"))?;
        // `Sample` lists its optional `lod`; a `Call` takes any argument name.
        let has_slot = node.input_slots().iter().any(|(n, _)| n == slot)
            || (matches!(node, Node::Call { .. }) && valid_node_name(slot));
        if !has_slot {
            return Err(format!("`{to}` has no input `{slot}`"));
        }
        if !points.iter().all(|p| finite2(*p)) {
            return Err("reroute points must be finite".into());
        }
        self.notes.reroutes.retain(|r| !(r.to == to && r.slot == slot));
        if !points.is_empty() {
            self.notes.reroutes.push(Reroute {
                to: to.to_string(),
                slot: slot.to_string(),
                points,
            });
            self.notes
                .reroutes
                .sort_by(|a, b| (a.to.as_str(), a.slot.as_str()).cmp(&(b.to.as_str(), b.slot.as_str())));
        }
        Ok(())
    }

    /// Wires input `slot` of node `to` to node `from`.
    ///
    /// # Errors
    ///
    /// Either node is missing, `from == to`, or `to` has no such slot.
    pub fn connect(&mut self, from: &str, to: &str, slot: &str) -> Result<(), String> {
        if !self.graph.nodes.contains_key(from) {
            return Err(format!("no node `{from}`"));
        }
        if from == to {
            return Err(format!("`{from}` cannot read itself"));
        }
        self.node_mut(to)?.set_input(slot, Some(from.to_string()))
    }

    /// Unwires input `slot` of node `to`.
    ///
    /// # Errors
    ///
    /// No such node or slot.
    pub fn disconnect(&mut self, to: &str, slot: &str) -> Result<(), String> {
        self.node_mut(to)?.set_input(slot, None)
    }

    /// Drives surface output `slot` (a function's output `slot`) with node `from`, or clears it.
    ///
    /// # Errors
    ///
    /// Unknown slot or node.
    pub fn set_output(&mut self, slot: &str, from: Option<&str>) -> Result<(), String> {
        if let Some(from) = from
            && !self.graph.nodes.contains_key(from)
        {
            return Err(format!("no node `{from}`"));
        }
        if let DocMode::Function(meta) = &mut self.mode {
            let names = meta.outputs.keys().cloned().collect::<Vec<_>>().join(", ");
            let Some(node) = meta.outputs.get_mut(slot) else {
                return Err(format!(
                    "the function has no output `{slot}` (it has: {names}); change its signature to add one"
                ));
            };
            *node = from.unwrap_or_default().to_string();
            return Ok(());
        }
        self.graph.output.set(slot, from.map(str::to_string))
    }

    /// Replaces a function's signature: `inputs` in order and the output names. Outputs that
    /// stay keep their wiring, new ones start unwired, dropped ones go. A new input gets an
    /// `Input` node of its name when that name is free, so it shows on the canvas.
    ///
    /// # Errors
    ///
    /// Not a function, a bad or repeated name.
    pub fn set_signature(&mut self, inputs: Vec<FunctionInput>, outputs: Vec<String>) -> Result<(), String> {
        let DocMode::Function(meta) = &mut self.mode else {
            return Err("only a material function has a signature".into());
        };
        let mut seen = BTreeSet::new();
        for name in inputs.iter().map(|i| &i.name) {
            if !valid_node_name(name) || !seen.insert(name.clone()) {
                return Err(format!("bad or repeated input name `{name}`"));
            }
        }
        let mut seen = BTreeSet::new();
        for name in &outputs {
            if !valid_node_name(name) || !seen.insert(name.clone()) {
                return Err(format!("bad or repeated output name `{name}`"));
            }
        }
        if outputs.is_empty() {
            return Err("a material function needs at least one output".into());
        }
        let added: Vec<String> = inputs
            .iter()
            .filter(|i| !meta.inputs.iter().any(|old| old.name == i.name))
            .map(|i| i.name.clone())
            .collect();
        meta.outputs = outputs
            .into_iter()
            .map(|name| {
                let node = meta.outputs.get(&name).cloned().unwrap_or_default();
                (name, node)
            })
            .collect();
        meta.inputs = inputs;
        for name in added {
            let read = self
                .graph
                .nodes
                .values()
                .any(|n| matches!(n, Node::Input(i) if *i == name));
            if !read && !self.graph.nodes.contains_key(&name) {
                self.graph.nodes.insert(name.clone(), Node::Input(name));
            }
        }
        Ok(())
    }

    /// Sets (`Some`) or clears (`None`) an instance's override of parameter `name`, checked
    /// against the parent (`MaterialInstance::validate`). A value equal in type to the
    /// declaration is stored as `MaterialInstance::set` stores it.
    ///
    /// # Errors
    ///
    /// Not an instance, or the override does not fit the parent.
    pub fn set_param_override(&mut self, name: &str, value: Option<ParamValue>) -> Result<(), String> {
        let graph = &self.graph;
        let DocMode::Instance(instance) = &mut self.mode else {
            return Err("only a material instance has overrides".into());
        };
        let mut next = instance.clone();
        match value {
            Some(value) => {
                // An override stays inside the parent's range (w13, ADR-8105 (b)).
                let value = graph
                    .params
                    .iter()
                    .find(|p| p.name == name)
                    .map_or(value, |p| p.clamp(value));
                next.set(name, value);
            }
            None => {
                next.params.remove(name);
            }
        }
        next.validate(graph).map_err(|e| e.to_string())?;
        *instance = next;
        Ok(())
    }

    /// Sets or clears an instance's texture override (a project-relative image path).
    ///
    /// # Errors
    ///
    /// Not an instance, an undeclared texture, or a path outside the project.
    pub fn set_texture_override(&mut self, name: &str, path: Option<&str>) -> Result<(), String> {
        let graph = &self.graph;
        let DocMode::Instance(instance) = &mut self.mode else {
            return Err("only a material instance has overrides".into());
        };
        let mut next = instance.clone();
        match path {
            Some(path) => {
                graph_file_path(None, None, path)?;
                next.textures.insert(name.to_string(), path.to_string());
            }
            None => {
                next.textures.remove(name);
            }
        }
        next.validate(graph).map_err(|e| e.to_string())?;
        *instance = next;
        Ok(())
    }

    /// Sets or clears an instance's static switch override.
    ///
    /// # Errors
    ///
    /// Not an instance, or an undeclared switch.
    pub fn set_switch_override(&mut self, name: &str, on: Option<bool>) -> Result<(), String> {
        let graph = &self.graph;
        let DocMode::Instance(instance) = &mut self.mode else {
            return Err("only a material instance has overrides".into());
        };
        let mut next = instance.clone();
        match on {
            Some(on) => {
                next.switches.insert(name.to_string(), on);
            }
            None => {
                next.switches.remove(name);
            }
        }
        next.validate(graph).map_err(|e| e.to_string())?;
        *instance = next;
        Ok(())
    }

    /// Moves node `name` on the canvas.
    ///
    /// # Errors
    ///
    /// No such node, or a non-finite position.
    pub fn move_node(&mut self, name: &str, pos: [f32; 2]) -> Result<(), String> {
        if !self.graph.nodes.contains_key(name) {
            return Err(format!("no node `{name}`"));
        }
        if !pos.iter().all(|v| v.is_finite()) {
            return Err("position is not finite".into());
        }
        self.layout.insert(name.to_string(), pos);
        Ok(())
    }

    /// Every node's canvas position: saved positions win; the rest are laid out by depth from
    /// the surface outputs (column = longest path to an output, right to left; rows by name),
    /// to the left of the saved ones. Deterministic: depends only on the graph and the layout.
    #[must_use]
    pub fn positions(&self) -> BTreeMap<String, [f32; 2]> {
        let depth = self.depths();
        let mut columns: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
        for name in self.graph.nodes.keys() {
            if !self.layout.contains_key(name) {
                columns.entry(depth[name.as_str()]).or_default().push(name);
            }
        }
        let mut out: BTreeMap<String, [f32; 2]> = self
            .layout
            .iter()
            .filter(|(name, _)| self.graph.nodes.contains_key(*name))
            .map(|(name, pos)| (name.clone(), *pos))
            .collect();
        for (column, names) in columns {
            for (row, name) in names.into_iter().enumerate() {
                out.insert(
                    name.to_string(),
                    [-(column as f32 + 1.0) * LAYOUT_COLUMN, row as f32 * LAYOUT_ROW],
                );
            }
        }
        out
    }

    /// Longest-path depth of every node from the surface outputs (outputs' drivers are 0).
    /// Nodes no output reaches sit one column past the deepest reached node. Cycles stop at
    /// the node count.
    fn depths(&self) -> BTreeMap<&str, usize> {
        let nodes = &self.graph.nodes;
        let mut depth: BTreeMap<&str, usize> = BTreeMap::new();
        let mut frontier: Vec<(&str, usize)> = self
            .sink_slots()
            .iter()
            .filter_map(|slot| self.sink_get(slot))
            .filter_map(|name| nodes.get_key_value(name).map(|(k, _)| (k.as_str(), 0)))
            .collect();
        let limit = nodes.len();
        while let Some((name, d)) = frontier.pop() {
            if d > limit || depth.get(name).is_some_and(|&old| old >= d) {
                continue;
            }
            depth.insert(name, d);
            if let Some(node) = nodes.get(name) {
                for input in node.inputs() {
                    if let Some((key, _)) = nodes.get_key_value(input) {
                        frontier.push((key.as_str(), d + 1));
                    }
                }
            }
        }
        let unreached = depth.values().max().map_or(0, |d| d + 1);
        for name in nodes.keys() {
            depth.entry(name.as_str()).or_insert(unreached);
        }
        depth
    }

    /// Number of static-switch permutations the graph can compile to (`2^switches`, saturating).
    #[must_use]
    pub fn permutations(&self) -> u64 {
        1u64.checked_shl(self.graph.switches.len() as u32)
            .unwrap_or(u64::MAX)
    }
}

/// What a compile error belongs to on the canvas.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    /// The whole graph.
    Graph,
    /// A node (an error inside a function expansion maps to its `Call` node).
    Node(String),
    /// A parameter declaration.
    Param(String),
    /// A texture declaration.
    Texture(String),
    /// A static switch declaration.
    Switch(String),
    /// A function definition.
    Function(String),
    /// A surface output slot.
    Output(String),
}

impl Target {
    /// Maps a [`regia_matgraph::CompileError::node`] string to its target.
    #[must_use]
    pub fn of(node: Option<&str>) -> Self {
        let Some(node) = node else {
            return Self::Graph;
        };
        let head = |s: &str| s.split('/').next().unwrap_or(s).to_string();
        if let Some(rest) = node.strip_prefix("param:") {
            Self::Param(rest.to_string())
        } else if let Some(rest) = node.strip_prefix("texture:") {
            Self::Texture(rest.to_string())
        } else if let Some(rest) = node.strip_prefix("switch:") {
            Self::Switch(head(rest))
        } else if let Some(rest) = node.strip_prefix("function:") {
            Self::Function(head(rest))
        } else if let Some(rest) = node.strip_prefix("output:") {
            Self::Output(rest.to_string())
        } else {
            Self::Node(head(node))
        }
    }

    /// The target as the agent protocol names it: `"graph"`, `"node:x"`, `"param:x"`, ...
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Graph => "graph".into(),
            Self::Node(n) => format!("node:{n}"),
            Self::Param(n) => format!("param:{n}"),
            Self::Texture(n) => format!("texture:{n}"),
            Self::Switch(n) => format!("switch:{n}"),
            Self::Function(n) => format!("function:{n}"),
            Self::Output(n) => format!("output:{n}"),
        }
    }
}

/// One mapped compile error.
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    /// Where the canvas shows it.
    pub target: Target,
    /// The compiler's own target string (`"worn/out"`, `"param:tint"`), if any.
    pub source: Option<String>,
    /// What is wrong.
    pub message: String,
}

/// The result of compiling the document (the fast path: no shader composition).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Diagnostics {
    /// The graph compiled.
    pub ok: bool,
    /// Errors, in compiler order.
    pub errors: Vec<Diagnostic>,
    /// Pipeline key of the default permutation, when it compiled.
    pub key: Option<u64>,
    /// Structural hash of the default permutation, when it compiled.
    pub graph_hash: Option<u64>,
    /// Warnings that do not stop the compile (permutation count, stale calls).
    pub warnings: Vec<String>,
    /// The signature of every function the graph can read (inline, or a readable file):
    /// what each `Call` node's pins follow (ADR-2490).
    pub signatures: BTreeMap<String, super::function::Signature>,
    /// `Call` nodes that do not fit their function's signature any more.
    pub stale: Vec<super::function::StaleCall>,
}

impl Diagnostics {
    /// The messages that belong to `target`.
    pub fn for_target<'a>(&'a self, target: &'a Target) -> impl Iterator<Item = &'a str> + 'a {
        self.errors
            .iter()
            .filter(move |d| &d.target == target)
            .map(|d| d.message.as_str())
    }

    /// Nodes with at least one error.
    #[must_use]
    pub fn nodes_with_errors(&self) -> BTreeSet<&str> {
        self.errors
            .iter()
            .filter_map(|d| match &d.target {
                Target::Node(n) => Some(n.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// Where a file a graph names (a texture, a `.matfunc.ron`) is read from: next to the graph
/// (`dir`), else under the project (`root`, then its `assets/`). Graph paths are
/// project-relative: an absolute path, a root or drive prefix, or `..` is refused, so a graph,
/// or an agent setting its textures, cannot read files outside the project (ADR-1816).
///
/// # Errors
///
/// The path is absolute or leads up with `..`.
pub fn graph_file_path(dir: Option<&Path>, root: Option<&Path>, path: &str) -> Result<PathBuf, String> {
    let normalized = path.replace('\\', "/");
    let p = Path::new(&normalized);
    let escapes = p.has_root()
        || p.is_absolute()
        || normalized.contains(':')
        || p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        });
    if escapes {
        return Err(format!(
            "`{path}` must be a project-relative path (no `..`, no absolute or drive path)"
        ));
    }
    let sibling = dir.map(|d| d.join(p)).filter(|f| f.exists());
    Ok(sibling.unwrap_or_else(|| crate::mesh_source::resolve_asset_path(root, &normalized)))
}

/// A copy of `graph` with its `.matfunc.ron` files read (relative to `dir`, then to the
/// project's `assets/`), ready to compile. The document keeps the file references.
///
/// # Errors
///
/// A function file cannot be read or parsed.
pub fn resolved(
    graph: &MaterialGraph,
    dir: Option<&Path>,
    root: Option<&Path>,
) -> Result<MaterialGraph, String> {
    let mut graph = graph.clone();
    let mut read = |path: &str| -> Result<String, String> {
        let full = graph_file_path(dir, root, path)?;
        std::fs::read_to_string(&full).map_err(|e| format!("{}: {e}", full.display()))
    };
    graph
        .resolve_function_files(&mut read)
        .map_err(|e| e.to_string())?;
    // Code nodes' `#include` files from the project's shaders/include/ (ADR-7852): only this
    // compile copy holds their text; a missing one is reported on its line.
    if let Some(root) = root {
        graph.resolve_code_includes(&mut regia_matgraph::code::project_include_reader(root));
    }
    Ok(graph)
}

/// Function definitions open in the editor's function tabs, by the file each tab saves to
/// (w11, ADR-7730): a graph that calls one of these files compiles against the tab's definition,
/// saved or not.
pub type LiveFunctions = BTreeMap<PathBuf, MaterialFunction>;

/// The live definition of the function file at `full`, matched by path (canonical when both
/// exist, so `assets/m/../m/x` and the absolute path agree).
fn live_function<'a>(live: &'a LiveFunctions, full: &Path) -> Option<&'a MaterialFunction> {
    if let Some(f) = live.get(full) {
        return Some(f);
    }
    let canonical = std::fs::canonicalize(full).ok()?;
    live.iter()
        .find(|(path, _)| std::fs::canonicalize(path).ok().as_ref() == Some(&canonical))
        .map(|(_, f)| f)
}

/// A copy of `graph` whose function files that are open in a function tab (`live`) are replaced
/// by the tab's definition, inline; the other files stay references (read when it compiles).
/// The document keeps its references: this is only what the graph compiles and previews.
#[must_use]
pub fn with_live_functions(
    graph: &MaterialGraph,
    dir: Option<&Path>,
    root: Option<&Path>,
    live: &LiveFunctions,
) -> MaterialGraph {
    let mut graph = graph.clone();
    if live.is_empty() {
        return graph;
    }
    for source in graph.functions.values_mut() {
        if let FunctionSource::File(rel) = source
            && let Ok(full) = graph_file_path(dir, root, rel)
            && let Some(f) = live_function(live, &full)
        {
            *source = FunctionSource::Inline(f.clone());
        }
    }
    graph
}

/// [`resolved`] with the function tabs' definitions first ([`with_live_functions`]).
///
/// # Errors
///
/// A function file that is not open cannot be read or parsed.
pub fn resolved_live(
    graph: &MaterialGraph,
    dir: Option<&Path>,
    root: Option<&Path>,
    live: &LiveFunctions,
) -> Result<MaterialGraph, String> {
    resolved(&with_live_functions(graph, dir, root, live), dir, root)
}

/// Compiles `graph` (default switches, fast path) and maps the errors to canvas targets.
/// `dir` / `root` locate `.matfunc.ron` files.
#[must_use]
pub fn diagnose(graph: &MaterialGraph, dir: Option<&Path>, root: Option<&Path>) -> Diagnostics {
    let mut warnings = Vec::new();
    if graph.switches.len() > PERMUTATION_WARN_SWITCHES {
        warnings.push(format!(
            "{} static switches compile to up to {} permutations (more than {} switches is a large share of the forward pipeline budget)",
            graph.switches.len(),
            1u64.checked_shl(graph.switches.len() as u32).unwrap_or(u64::MAX),
            PERMUTATION_WARN_SWITCHES
        ));
    }
    let graph = match resolved(graph, dir, root) {
        Ok(g) => g,
        Err(message) => {
            return Diagnostics {
                ok: false,
                errors: vec![Diagnostic {
                    target: Target::Graph,
                    source: None,
                    message,
                }],
                warnings,
                ..Diagnostics::default()
            };
        }
    };
    match regia_matgraph::compile_unvalidated(&graph) {
        Ok(compiled) => Diagnostics {
            ok: true,
            errors: Vec::new(),
            key: Some(compiled.key),
            graph_hash: Some(compiled.graph_hash),
            warnings,
            ..Diagnostics::default()
        },
        Err(errors) => Diagnostics {
            ok: false,
            errors: errors
                .0
                .into_iter()
                .map(|e| Diagnostic {
                    target: Target::of(e.node.as_deref()),
                    source: e.node,
                    message: e.message,
                })
                .collect(),
            key: None,
            graph_hash: None,
            warnings,
            ..Diagnostics::default()
        },
    }
}

/// Reads the function file `path` of a graph in `dir` (project `root`); `None` when it
/// cannot be read or parsed (the compile reports why).
#[must_use]
pub fn read_function_file(
    dir: Option<&Path>,
    root: Option<&Path>,
    path: &str,
) -> Option<regia_matgraph::MaterialFunction> {
    let full = graph_file_path(dir, root, path).ok()?;
    let text = std::fs::read_to_string(full).ok()?;
    regia_matgraph::MaterialFunction::from_ron(&text).ok()
}

/// Diagnostics of any document: a graph ([`diagnose`] plus stale `Call` nodes), a function
/// (compiled in its harness against `context`, `function::diagnose_function`), or an instance
/// (its parent with the overrides, compiled with its switches, plus overrides that do not fit).
#[must_use]
pub fn diagnose_doc(
    doc: &MatGraphDoc,
    dir: Option<&Path>,
    root: Option<&Path>,
    context: Option<&MaterialGraph>,
) -> Diagnostics {
    use super::function::{diagnose_function, signatures, stale_calls};
    match &doc.mode {
        DocMode::Function(_) => diagnose_function(doc, context),
        DocMode::Graph | DocMode::Instance(_) => {
            let graph = doc.view_graph();
            let mut d = diagnose(&graph, dir, root);
            if let DocMode::Instance(instance) = &doc.mode
                && let Err(e) = instance.validate(&doc.graph)
            {
                use regia_matgraph::InstanceError as E;
                let target = match &e {
                    E::UnknownParam(n) | E::TypeMismatch { name: n, .. } | E::NotFinite(n) => {
                        Target::Param(n.clone())
                    }
                    E::UnknownTexture(n) => Target::Texture(n.clone()),
                    E::UnknownSwitch(n) => Target::Switch(n.clone()),
                };
                d.ok = false;
                d.errors.push(Diagnostic {
                    target,
                    source: None,
                    message: e.to_string(),
                });
            }
            d.signatures = signatures(&doc.graph, &mut |path| read_function_file(dir, root, path));
            d.stale = stale_calls(&doc.graph, &d.signatures);
            for stale in &d.stale {
                d.warnings.push(format!("{}: {}", stale.node, stale.message()));
            }
            d
        }
    }
}

/// Most switch permutations [`policy_cost`] compiles (`2^6`): a graph with more switches is
/// counted on its first 64 permutations, in switch-bit order, and reports how many it skipped.
pub const POLICY_MAX_PERMUTATIONS: u64 = 64;

/// The render profiles [`policy_cost`] checks: every real-time preset a desktop or Android
/// package runs (`regia_render::permutation_policy::platform_presets`), each once.
#[must_use]
pub fn policy_presets() -> Vec<regia_render::RenderProfilePreset> {
    let mut presets = regia_render::permutation_policy::platform_presets("linux");
    for preset in regia_render::permutation_policy::platform_presets("android") {
        if !presets.contains(&preset) {
            presets.push(preset);
        }
    }
    presets
}

/// The shipped shader-permutation policy (ADR-2026) applied to one graph alone: what the cook
/// would list for it on each profile, against that profile's default budget.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyCost {
    /// Switch permutations the graph has (`2^switches`, saturating).
    pub permutations: u64,
    /// Permutations compiled and counted (at most [`POLICY_MAX_PERMUTATIONS`]).
    pub enumerated: u64,
    /// Distinct pipeline keys among them (switches that do not change the code share one).
    pub distinct_codes: usize,
    /// One enumeration per profile of [`policy_presets`], in that order.
    pub profiles: Vec<regia_render::permutation_policy::ProfilePermutations>,
}

/// Compiles every switch permutation of `graph` (functions already resolved, see [`resolved`];
/// at most [`POLICY_MAX_PERMUTATIONS`]) and runs the shipped per-profile enumeration
/// (`regia_render::permutation_policy::enumerate_profile`) over the codes, as the cook does for a
/// project's graphs. Deterministic: the same graph gives the same numbers.
///
/// # Errors
///
/// The first permutation that does not compile (its errors).
pub fn policy_cost(graph: &MaterialGraph) -> Result<PolicyCost, regia_matgraph::CompileErrors> {
    use regia_render::permutation_policy::{
        CompileCostModel, GraphPermutationInput, PermutationBudget, enumerate_profile,
    };
    let permutations = 1u64.checked_shl(graph.switches.len() as u32).unwrap_or(u64::MAX);
    let enumerated = permutations.min(POLICY_MAX_PERMUTATIONS);
    let mut inputs = Vec::new();
    let mut keys = BTreeSet::new();
    for bits in 0..enumerated {
        let switches: BTreeMap<String, bool> = graph
            .switches
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.clone(), i < 64 && bits & (1u64 << i) != 0))
            .collect();
        let compiled = regia_matgraph::compile_with(graph, &switches)?;
        let code = compiled.code();
        keys.insert(code.key());
        inputs.push(GraphPermutationInput::from_code(
            format!("{}#{bits}", graph.name),
            &code,
        ));
    }
    let profiles = policy_presets()
        .into_iter()
        .map(|preset| {
            enumerate_profile(
                preset,
                &inputs,
                PermutationBudget::for_preset(preset),
                CompileCostModel::for_preset(preset),
            )
        })
        .collect();
    Ok(PolicyCost {
        permutations,
        enumerated,
        distinct_codes: keys.len(),
        profiles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> MatGraphDoc {
        let graph = MaterialGraph::from_ron(
            r#"(
                format_version: 2,
                params: [(name: "tint", value: Color((0.8, 0.2, 0.1)))],
                nodes: {
                    "tint": Param("tint"),
                    "vcol": VertexColor,
                    "rgb": Swizzle(input: "vcol", channels: "rgb"),
                    "base": Mul("tint", "rgb"),
                },
                output: (base_color: Some("base")),
            )"#,
        )
        .expect("fixture parses");
        MatGraphDoc::new(graph, BTreeMap::new())
    }

    #[test]
    fn the_fixture_compiles() {
        let d = diagnose(&doc().graph, None, None);
        assert!(d.ok, "{:?}", d.errors);
        assert!(d.key.is_some());
    }

    #[test]
    fn add_connect_and_output_build_a_working_graph() {
        let mut d = MatGraphDoc::default();
        let c = d
            .add_node(None, Node::Vec3([0.2, 0.4, 0.6]), Some([0.0, 0.0]))
            .expect("add");
        assert_eq!(c, "vec3_1");
        let sat = d
            .add_node(Some("sat"), Node::default_of("Saturate").expect("kind"), None)
            .expect("add");
        // Unwired: an error on `sat`, once it is reachable.
        d.set_output("base_color", Some(&sat)).expect("output");
        let diag = diagnose(&d.graph, None, None);
        assert!(!diag.ok);
        assert!(diag.nodes_with_errors().contains("sat"), "{:?}", diag.errors);
        d.connect(&c, &sat, "x").expect("connect");
        let diag = diagnose(&d.graph, None, None);
        assert!(diag.ok, "{:?}", diag.errors);
        // Refusals.
        assert!(d.add_node(Some("sat"), Node::Time, None).is_err(), "taken");
        assert!(d.add_node(Some("a/b"), Node::Time, None).is_err(), "bad name");
        assert!(d.connect("nope", &sat, "x").is_err());
        assert!(d.connect(&sat, &sat, "x").is_err(), "self loop");
        assert!(d.connect(&c, &sat, "q").is_err(), "no such slot");
        assert!(d.set_output("nope", Some(&c)).is_err());
    }

    #[test]
    fn disconnect_reports_the_node() {
        let mut d = doc();
        d.disconnect("base", "b").expect("disconnect");
        assert_eq!(d.graph.nodes["base"], Node::Mul("tint".into(), String::new()));
        let diag = diagnose(&d.graph, None, None);
        let base = Target::Node("base".into());
        assert_eq!(
            diag.for_target(&base).collect::<Vec<_>>(),
            ["an input is not connected"]
        );
    }

    #[test]
    fn remove_unwires_readers_and_clears_outputs() {
        let mut d = doc();
        d.move_node("rgb", [1.0, 2.0]).expect("move");
        let readers = d.remove_node("rgb").expect("remove");
        assert_eq!(readers, ["base"]);
        assert!(!d.layout.contains_key("rgb"));
        assert_eq!(d.graph.nodes["base"], Node::Mul("tint".into(), String::new()));
        d.remove_node("base").expect("remove");
        assert_eq!(d.graph.output.base_color, None);
        assert!(d.remove_node("base").is_err());
    }

    #[test]
    fn remove_unsets_an_optional_lod() {
        let mut d = doc();
        d.add_node(Some("uv"), Node::TexCoord, None).expect("uv");
        d.add_node(Some("mip"), Node::Float(2.0), None).expect("mip");
        d.add_node(
            Some("s"),
            Node::Sample {
                texture: "t".into(),
                uv: "uv".into(),
                lod: Some("mip".into()),
            },
            None,
        )
        .expect("s");
        d.remove_node("mip").expect("remove");
        assert!(matches!(&d.graph.nodes["s"], Node::Sample { lod: None, .. }));
    }

    #[test]
    fn rename_rewrites_inputs_outputs_and_layout() {
        let mut d = doc();
        d.move_node("base", [5.0, 6.0]).expect("move");
        d.rename_node("base", "albedo").expect("rename");
        assert_eq!(d.graph.output.base_color.as_deref(), Some("albedo"));
        assert_eq!(d.layout.get("albedo"), Some(&[5.0, 6.0]));
        d.rename_node("rgb", "color").expect("rename");
        assert_eq!(d.graph.nodes["albedo"], Node::Mul("tint".into(), "color".into()));
        assert!(diagnose(&d.graph, None, None).ok);
        assert!(d.rename_node("color", "tint").is_err(), "taken");
        assert!(d.rename_node("nope", "x").is_err());
        assert!(d.rename_node("color", "bad name").is_err());
    }

    #[test]
    fn function_errors_map_to_the_call_node() {
        assert_eq!(Target::of(Some("worn/out")), Target::Node("worn".into()));
        assert_eq!(Target::of(Some("param:tint")), Target::Param("tint".into()));
        assert_eq!(
            Target::of(Some("texture:albedo")),
            Target::Texture("albedo".into())
        );
        assert_eq!(Target::of(Some("output:normal")), Target::Output("normal".into()));
        assert_eq!(
            Target::of(Some("function:weather/k")),
            Target::Function("weather".into())
        );
        assert_eq!(Target::of(Some("switch:wet")), Target::Switch("wet".into()));
        assert_eq!(Target::of(None), Target::Graph);
        // End to end: a broken node inside an inline function is reported on the call.
        let graph = MaterialGraph::from_ron(
            r#"(
                format_version: 2,
                functions: {
                    "f": Inline((
                        inputs: [(name: "c", ty: Vec3)],
                        nodes: { "c": Input("c"), "bad": Saturate("missing") },
                        outputs: { "out": "bad" },
                    )),
                },
                nodes: { "v": Vec3((1.0, 0.0, 0.0)), "worn": Call(function: "f", args: { "c": "v" }) },
                output: (base_color: Some("worn")),
            )"#,
        )
        .expect("parses");
        let diag = diagnose(&graph, None, None);
        assert!(!diag.ok);
        assert!(
            diag.errors
                .iter()
                .any(|e| e.target == Target::Node("worn".into()) || e.target == Target::Function("f".into())),
            "{:?}",
            diag.errors
        );
    }

    #[test]
    fn auto_layout_is_deterministic_and_keeps_saved_positions() {
        let mut d = doc();
        let a = d.positions();
        assert_eq!(a, doc().positions(), "same graph, same layout");
        // `base` drives the output: column 0; `tint` and `rgb` column 1; `vcol` column 2.
        assert_eq!(a["base"], [-LAYOUT_COLUMN, 0.0]);
        assert_eq!(a["rgb"][0], -2.0 * LAYOUT_COLUMN);
        assert_eq!(a["tint"][0], -2.0 * LAYOUT_COLUMN);
        assert_ne!(a["rgb"][1], a["tint"][1]);
        assert_eq!(a["vcol"][0], -3.0 * LAYOUT_COLUMN);
        d.move_node("vcol", [9.0, 9.0]).expect("move");
        assert_eq!(d.positions()["vcol"], [9.0, 9.0]);
        // An unreached node goes one column past the deepest.
        d.add_node(Some("lonely"), Node::Time, None).expect("add");
        assert_eq!(d.positions()["lonely"][0], -4.0 * LAYOUT_COLUMN);
    }

    #[test]
    fn layout_survives_a_cycle() {
        let mut d = doc();
        d.set_node("rgb", Node::Saturate("base".into())).expect("set");
        let p = d.positions();
        assert_eq!(p.len(), 4);
        assert!(!diagnose(&d.graph, None, None).ok, "the cycle is a compile error");
    }

    #[test]
    fn many_switches_warn_and_count_permutations() {
        let mut d = doc();
        for i in 0..5 {
            d.graph.switches.push(regia_matgraph::SwitchDecl {
                name: format!("s{i}"),
                default: false,
            });
        }
        assert_eq!(d.permutations(), 32);
        let diag = diagnose(&d.graph, None, None);
        assert_eq!(diag.warnings.len(), 1, "{:?}", diag.warnings);
        assert!(diag.ok, "unused switches still compile: {:?}", diag.errors);
    }

    #[test]
    fn policy_cost_runs_the_shipped_enumeration_per_profile() {
        let mut d = doc();
        d.graph.switches.push(regia_matgraph::SwitchDecl {
            name: "tinted".into(),
            default: true,
        });
        d.graph.switches.push(regia_matgraph::SwitchDecl {
            name: "unused".into(),
            default: false,
        });
        d.graph.nodes.insert(
            "pick".into(),
            Node::Switch {
                switch: "tinted".into(),
                on: "base".into(),
                off: "rgb".into(),
            },
        );
        d.graph.output.base_color = Some("pick".into());
        let cost = policy_cost(&d.graph).expect("compiles");
        assert_eq!(cost.permutations, 4);
        assert_eq!(cost.enumerated, 4);
        assert_eq!(cost.distinct_codes, 2, "the unused switch shares the code");
        let presets = policy_presets();
        assert_eq!(cost.profiles.len(), presets.len());
        assert!(presets.contains(&regia_render::RenderProfilePreset::MobileLow));
        assert!(presets.contains(&regia_render::RenderProfilePreset::DesktopHigh));
        for p in &cost.profiles {
            assert_eq!(p.entries.len(), 2, "{}: one set per distinct code", p.profile);
            let passes =
                regia_render::permutation_policy::graph_set_passes(p.target.instanced, false).len() as u32;
            assert_eq!(p.pipelines, 2 * passes, "{}", p.profile);
            assert!(!p.over_budget, "{}", p.profile);
            assert!(p.estimated_compile_ms > 0);
        }
        assert_eq!(policy_cost(&d.graph).expect("again"), cost, "deterministic");
    }

    #[test]
    fn policy_cost_caps_the_enumeration_and_reports_compile_errors() {
        let mut d = doc();
        for i in 0..7 {
            d.graph.switches.push(regia_matgraph::SwitchDecl {
                name: format!("s{i}"),
                default: false,
            });
        }
        let cost = policy_cost(&d.graph).expect("compiles");
        assert_eq!(cost.permutations, 128);
        assert_eq!(cost.enumerated, POLICY_MAX_PERMUTATIONS);
        assert_eq!(cost.distinct_codes, 1);
        d.graph.output.base_color = Some("missing".into());
        assert!(policy_cost(&d.graph).is_err());
    }

    #[test]
    fn graph_file_paths_stay_in_the_project() {
        let root = Path::new("proj");
        for bad in [
            "/etc/passwd",
            "../x.png",
            "a/../../x.png",
            "..\\x.png",
            "C:x.png",
            "C:\\x.png",
        ] {
            assert!(graph_file_path(None, Some(root), bad).is_err(), "{bad}");
        }
        let ok = graph_file_path(None, Some(root), "textures/a.png").expect("relative");
        assert!(ok.starts_with(root), "{}", ok.display());
        let mut d = doc();
        d.graph.functions.insert(
            "f".into(),
            regia_matgraph::FunctionSource::File("../outside.matfunc.ron".into()),
        );
        let diag = diagnose(&d.graph, None, Some(root));
        assert!(!diag.ok);
        assert!(
            diag.errors[0].message.contains("project-relative"),
            "{:?}",
            diag.errors
        );
    }

    #[test]
    fn fresh_names_are_snake_case_and_unique() {
        let mut d = MatGraphDoc::default();
        assert_eq!(d.fresh_name("UvTransform"), "uv_transform_1");
        d.add_node(None, Node::default_of("UvTransform").expect("k"), None)
            .expect("add");
        assert_eq!(d.fresh_name("UvTransform"), "uv_transform_2");
    }
    #[test]
    fn signature_edits_keep_wires_and_add_input_nodes() {
        let f = regia_matgraph::MaterialFunction::from_ron(
            r#"(format_version: 3, inputs: [(name: "a", ty: Float)],
                nodes: { "a": Input("a"), "s": Saturate("a") }, outputs: { "o": "s", "gone": "a" })"#,
        )
        .expect("function");
        let mut d = MatGraphDoc::function(f, BTreeMap::new());
        assert_eq!(d.sink_slots(), ["gone", "o"]);
        d.set_signature(
            vec![
                FunctionInput {
                    name: "a".into(),
                    ty: regia_matgraph::ValueType::Float,
                },
                FunctionInput {
                    name: "b".into(),
                    ty: regia_matgraph::ValueType::Vec2,
                },
            ],
            vec!["o".into(), "extra".into()],
        )
        .expect("signature");
        let meta = d.function_meta().expect("function");
        assert_eq!(meta.outputs["o"], "s", "a kept output keeps its wire");
        assert_eq!(meta.outputs["extra"], "", "a new output starts unwired");
        assert!(!meta.outputs.contains_key("gone"));
        assert_eq!(
            d.graph.nodes["b"],
            Node::Input("b".into()),
            "a new input gets its Input node"
        );
        // Rename and remove follow into the outputs.
        d.rename_node("s", "sat").expect("rename");
        assert_eq!(d.function_meta().expect("f").outputs["o"], "sat");
        d.remove_node("sat").expect("remove");
        assert_eq!(d.function_meta().expect("f").outputs["o"], "");
        assert!(
            d.set_signature(Vec::new(), Vec::new()).is_err(),
            "at least one output"
        );
        assert!(
            MatGraphDoc::default()
                .set_signature(Vec::new(), vec!["o".into()])
                .is_err()
        );
        let back = d.to_function().expect("function");
        assert_eq!(back.format_version, MATFUNC_FORMAT_VERSION);
    }

    #[test]
    fn mode_guards_refuse_graph_edits_of_an_instance_and_declarations_of_a_function() {
        let base = doc();
        let inst = MatGraphDoc::instance(base.graph.clone(), BTreeMap::new(), MaterialInstance::new("p"));
        let mut edited = inst.clone();
        edited
            .add_node(Some("t"), Node::Time, None)
            .expect("the model op itself works");
        let err = edited.check_mode_change(&inst).expect_err("read-only");
        assert!(err.starts_with(READ_ONLY), "{err}");
        let mut over = inst.clone();
        over.set_param_override("tint", Some(ParamValue::Vec3([1.0, 0.0, 0.0])))
            .expect("override");
        assert!(
            over.check_mode_change(&inst).is_ok(),
            "overrides are the instance's own"
        );
        assert!(
            over.set_param_override("tint", Some(ParamValue::Float(1.0)))
                .is_err(),
            "type"
        );
        // The view keeps the declared color variant and the override value.
        let view = instance_view(&over.graph, over.instance_meta().expect("instance"));
        assert_eq!(view.params[0].value, ParamValue::Color([1.0, 0.0, 0.0]));
        let f = MatGraphDoc::function(
            regia_matgraph::MaterialFunction {
                format_version: MATFUNC_FORMAT_VERSION,
                name: "f".into(),
                inputs: Vec::new(),
                nodes: BTreeMap::new(),
                outputs: BTreeMap::from([("o".to_string(), String::new())]),
            },
            BTreeMap::new(),
        );
        let mut g = f.clone();
        g.graph.params = base.graph.params.clone();
        assert!(g.check_mode_change(&f).is_err());
        let mut h = f.clone();
        h.add_node(Some("k"), Node::Float(1.0), None).expect("add");
        h.set_output("o", Some("k")).expect("function output");
        assert!(h.check_mode_change(&f).is_ok());
        assert!(h.set_output("base_color", Some("k")).is_err(), "no surface slots");
    }
}
