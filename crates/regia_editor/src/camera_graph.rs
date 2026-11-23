// SPDX-License-Identifier: MPL-2.0
//! The rig graph of a camera asset (`book/agents/editor-gameplay-panels.md`, ADR-2030): a plain model
//! of a [`CameraAsset`]'s rigs, their node pipelines and the transitions between them, and its layout
//! on a canvas, so the panel's drawing and `editor.camera_rig_graph` read one thing.
//!
//! A rig is a row of boxes in evaluation order. A `Group` or `When` carries its nested nodes in rows
//! below it. Transitions are arcs in a gutter on the left, from the rows that can leave to the rows
//! that can enter.

use regia_framework::camera::asset::{
    Blend, CameraAsset, Condition, DirectorDef, Node, RigDef, RigMatch, StartFacing, TransitionDef,
    Value as NodeValue,
};
use serde_json::{Value, json};

/// Width of a node's box on the canvas.
pub const NODE_W: f32 = 132.0;
/// Height of a node's box on the canvas.
pub const NODE_H: f32 = 44.0;
const GAP_X: f32 = 12.0;
const GAP_Y: f32 = 10.0;
const RIG_GAP: f32 = 28.0;
const RIG_LABEL_H: f32 = 22.0;
/// Width of one transition's lane in the gutter.
const LANE: f32 = 14.0;

/// One node of a rig.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphNode {
    /// The position in evaluation order, nested nodes included: what the driver's trace calls `index`.
    pub index: usize,
    /// Child indices from the rig's root list down to this node.
    pub path: Vec<usize>,
    /// Nesting below the root list.
    pub depth: usize,
    /// The node kind (`pivot`, `boom`, ...).
    pub kind: &'static str,
    /// One line of what is set on it.
    pub summary: String,
    /// A `Group` switched off with a literal.
    pub switched_off: bool,
}

/// One rig.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphRig {
    /// The name.
    pub name: String,
    /// The tags.
    pub tags: Vec<String>,
    /// Its nodes in evaluation order.
    pub nodes: Vec<GraphNode>,
    /// The director can choose this rig by name.
    pub director_choice: bool,
    /// Seconds a layer of this rig blends out over.
    pub blend_out: Option<f32>,
}

/// One transition.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphEdge {
    /// The outgoing end as written: `any`, a rig name, or `tag:<tag>`.
    pub from: String,
    /// The incoming end as written.
    pub to: String,
    /// The rigs the outgoing end matches.
    pub from_rigs: Vec<usize>,
    /// The rigs the incoming end matches.
    pub to_rigs: Vec<usize>,
    /// The rig whose own list holds it; `None` for the asset's shared list.
    pub owner: Option<String>,
    /// `cut`, `linear`, `smooth`, `ease_in` or `ease_out`.
    pub blend: &'static str,
    /// The blend's seconds.
    pub seconds: f32,
    /// `own_seed`, `target_facing`, `carry_over` or `continue_aim`.
    pub start_facing: &'static str,
    /// While blending out, the outgoing rig stops updating.
    pub hold_outgoing: bool,
    /// Same-structure rigs merge node states.
    pub share_state: bool,
    /// A new primary rig may interrupt the blend.
    pub interruptible: bool,
    /// Entry conditions as text.
    pub conditions: Vec<String>,
}

/// The whole graph.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RigGraph {
    /// The director as a line of text.
    pub director: String,
    /// The rigs in file order.
    pub rigs: Vec<GraphRig>,
    /// Transitions: the rigs' own lists in file order, then the shared list. That is the order the
    /// driver searches them in.
    pub edges: Vec<GraphEdge>,
}

/// A rectangle: `[x, y, width, height]`.
pub type Rect = [f32; 4];

/// A node's place on the canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NodeBox {
    /// Index into [`RigGraph::rigs`].
    pub rig: usize,
    /// Index into that rig's nodes.
    pub node: usize,
    /// Where it is.
    pub rect: Rect,
}

/// An arc in the gutter between two rigs' rows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeArc {
    /// Index into [`RigGraph::edges`].
    pub edge: usize,
    /// The x of its lane.
    pub x: f32,
    /// The y where it leaves.
    pub from_y: f32,
    /// The y where it enters.
    pub to_y: f32,
}

/// The canvas.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphLayout {
    /// Each rig's label, by rig: `[x, y, width, height]`.
    pub rig_labels: Vec<Rect>,
    /// Every node.
    pub nodes: Vec<NodeBox>,
    /// Every arc.
    pub arcs: Vec<EdgeArc>,
    /// Canvas width.
    pub width: f32,
    /// Canvas height.
    pub height: f32,
}

fn cond_text(condition: &Condition) -> String {
    match condition {
        Condition::Elapsed(seconds) => format!("elapsed >= {seconds}"),
        Condition::VarBool(name, flag) => format!("{name} == {flag}"),
        Condition::VarCompare(name, op, value) => format!("{name} {op:?} {value}"),
        Condition::HasTag(tag) => format!("tag {tag}"),
        Condition::RigIs(rig) => format!("rig {rig}"),
        Condition::All(items) => {
            let parts: Vec<String> = items.iter().map(cond_text).collect();
            format!("({})", parts.join(" & "))
        }
        Condition::Any(items) => {
            let parts: Vec<String> = items.iter().map(cond_text).collect();
            format!("({})", parts.join(" | "))
        }
        Condition::Not(inner) => format!("!{}", cond_text(inner)),
    }
}

fn value_text<T: std::fmt::Debug>(value: &NodeValue<T>) -> String {
    match value {
        NodeValue::Literal(v) => format!("{v:?}"),
        NodeValue::Param(name) => format!("param {name}"),
        NodeValue::Var(name) => format!("var {name}"),
    }
}

fn node_summary(node: &Node) -> String {
    match node {
        Node::LookInput { slot, .. } => format!("slot {slot}"),
        Node::Boom { length, .. } => format!("length {}", value_text(length)),
        Node::Collision { radius, .. } => format!("radius {radius}"),
        Node::Fov { degrees } => format!("{} deg", value_text(degrees)),
        Node::Rail { spline, drive, .. } => {
            use regia_framework::camera::rail::RailDrive;
            let how = match drive {
                RailDrive::Value(v) => format!("at {}", value_text(v)),
                RailDrive::Time { speed, .. } => format!("{speed} m/s"),
                RailDrive::Follow { lead, .. } => format!("follow, lead {lead}"),
            };
            format!("{spline} {how}")
        }
        Node::Crane { length, .. } => format!("arm {} m", value_text(length)),
        Node::Group { enabled, nodes } => format!("{} nodes, on {}", nodes.len(), value_text(enabled)),
        Node::When { cond, nodes } => format!("{} nodes if {}", nodes.len(), cond_text(cond)),
        other => {
            // The other nodes have no one setting that stands for them: the start of their fields.
            let text = format!("{other:?}");
            let fields = text
                .split_once('{')
                .map_or(text.as_str(), |(_, rest)| rest)
                .trim();
            fields.chars().take(40).collect()
        }
    }
}

fn collect_nodes(nodes: &[Node], prefix: &[usize], next: &mut usize, out: &mut Vec<GraphNode>) {
    for (i, node) in nodes.iter().enumerate() {
        let mut path = prefix.to_vec();
        path.push(i);
        out.push(GraphNode {
            index: *next,
            depth: prefix.len(),
            kind: node.kind(),
            summary: node_summary(node),
            switched_off: matches!(
                node,
                Node::Group {
                    enabled: NodeValue::Literal(false),
                    ..
                }
            ),
            path: path.clone(),
        });
        *next += 1;
        collect_nodes(node.children(), &path, next, out);
    }
}

fn match_text(end: &RigMatch) -> String {
    match end {
        RigMatch::Any => "any".to_owned(),
        RigMatch::Rig(name) => name.clone(),
        RigMatch::Tag(tag) => format!("tag:{tag}"),
    }
}

fn matching(end: &RigMatch, rigs: &[RigDef]) -> Vec<usize> {
    rigs.iter()
        .enumerate()
        .filter(|(_, rig)| match end {
            RigMatch::Any => true,
            RigMatch::Rig(name) => rig.name == *name,
            RigMatch::Tag(tag) => rig.tags.iter().any(|t| t == tag),
        })
        .map(|(i, _)| i)
        .collect()
}

fn edge(transition: &TransitionDef, owner: Option<&str>, rigs: &[RigDef]) -> GraphEdge {
    let blend = match transition.blend {
        Blend::Cut => "cut",
        Blend::Linear(_) => "linear",
        Blend::Smooth(_) => "smooth",
        Blend::EaseIn(_) => "ease_in",
        Blend::EaseOut(_) => "ease_out",
    };
    GraphEdge {
        from: match_text(&transition.from),
        to: match_text(&transition.to),
        from_rigs: matching(&transition.from, rigs),
        to_rigs: matching(&transition.to, rigs),
        owner: owner.map(str::to_owned),
        blend,
        seconds: transition.blend.duration(),
        start_facing: match transition.start_facing {
            StartFacing::OwnSeed => "own_seed",
            StartFacing::TargetFacing => "target_facing",
            StartFacing::CarryOver => "carry_over",
            StartFacing::ContinueAim => "continue_aim",
        },
        hold_outgoing: transition.hold_outgoing,
        share_state: transition.share_state,
        interruptible: transition.interruptible,
        conditions: transition.conditions.iter().map(cond_text).collect(),
    }
}

fn director_text(director: &DirectorDef) -> String {
    match director {
        DirectorDef::Fixed(rig) => format!("fixed: {rig}"),
        DirectorDef::Rules { default, rules } => {
            let parts: Vec<String> = rules
                .iter()
                .map(|rule| format!("{} if {}", rule.rig, cond_text(&rule.when)))
                .collect();
            format!("rules: {}, else {default}", parts.join("; "))
        }
        DirectorDef::Ranked(ranks) => {
            let parts: Vec<String> = ranks
                .iter()
                .map(|rank| format!("{}: {}", rank.rank, director_text(&rank.director)))
                .collect();
            format!("ranked: [{}]", parts.join(", "))
        }
        DirectorDef::Hook(name) => format!("hook: {name}"),
    }
}

fn director_rigs(director: &DirectorDef, out: &mut Vec<String>) {
    match director {
        DirectorDef::Fixed(rig) => out.push(rig.clone()),
        DirectorDef::Rules { default, rules } => {
            out.push(default.clone());
            out.extend(rules.iter().map(|rule| rule.rig.clone()));
        }
        DirectorDef::Ranked(ranks) => {
            for rank in ranks {
                director_rigs(&rank.director, out);
            }
        }
        DirectorDef::Hook(_) => {}
    }
}

/// The graph of `asset`.
#[must_use]
pub fn rig_graph(asset: &CameraAsset) -> RigGraph {
    let mut named = Vec::new();
    director_rigs(&asset.director, &mut named);
    let rigs = asset
        .rigs
        .iter()
        .map(|rig| {
            let mut nodes = Vec::new();
            collect_nodes(&rig.root, &[], &mut 0, &mut nodes);
            GraphRig {
                name: rig.name.clone(),
                tags: rig.tags.clone(),
                nodes,
                director_choice: named.contains(&rig.name),
                blend_out: rig.blend_out,
            }
        })
        .collect();
    let own = asset
        .rigs
        .iter()
        .flat_map(|rig| rig.transitions.iter().map(|t| (t, Some(rig.name.as_str()))));
    let shared = asset.transitions.iter().map(|t| (t, None));
    let edges = own
        .chain(shared)
        .map(|(transition, owner)| edge(transition, owner, &asset.rigs))
        .collect();
    RigGraph {
        director: director_text(&asset.director),
        rigs,
        edges,
    }
}

/// Places `graph` on a canvas. Deterministic, and no two boxes overlap.
#[must_use]
pub fn layout_graph(graph: &RigGraph) -> GraphLayout {
    let mut layout = GraphLayout::default();
    let gutter = graph.edges.len() as f32 * LANE + GAP_X;
    let mut y = 0.0f32;
    let mut width = gutter;
    let mut centers = Vec::with_capacity(graph.rigs.len());
    for (rig_index, rig) in graph.rigs.iter().enumerate() {
        layout.rig_labels.push([gutter, y, NODE_W, RIG_LABEL_H]);
        let top = y + RIG_LABEL_H;
        centers.push(top + RIG_LABEL_H / 2.0);
        // Each depth keeps its own running x, so a nested list starts under its parent and never
        // runs into an earlier list at the same depth.
        let mut next_x: Vec<f32> = Vec::new();
        let mut bottom = top;
        for (node_index, node) in rig.nodes.iter().enumerate() {
            if next_x.len() <= node.depth {
                next_x.resize(node.depth + 1, gutter);
            }
            // A node starts no earlier than where its parent stands.
            let floor = if node.depth > 0 {
                next_x[node.depth - 1] - (NODE_W + GAP_X)
            } else {
                gutter
            };
            let x = next_x[node.depth].max(floor);
            let row_y = top + node.depth as f32 * (NODE_H + GAP_Y);
            layout.nodes.push(NodeBox {
                rig: rig_index,
                node: node_index,
                rect: [x, row_y, NODE_W, NODE_H],
            });
            next_x[node.depth] = x + NODE_W + GAP_X;
            width = width.max(x + NODE_W);
            bottom = bottom.max(row_y + NODE_H);
        }
        y = bottom + RIG_GAP;
    }
    for (edge_index, edge) in graph.edges.iter().enumerate() {
        let x = edge_index as f32 * LANE + LANE / 2.0;
        for &from in &edge.from_rigs {
            for &to in &edge.to_rigs {
                if from != to {
                    layout.arcs.push(EdgeArc {
                        edge: edge_index,
                        x,
                        from_y: centers[from] - 4.0,
                        to_y: centers[to] + 4.0,
                    });
                }
            }
        }
    }
    layout.width = width;
    layout.height = (y - RIG_GAP).max(0.0);
    layout
}

/// The graph and its layout as the agent reads them.
#[must_use]
pub fn graph_json(graph: &RigGraph, layout: &GraphLayout) -> Value {
    let rect_of = |rig: usize, node: usize| {
        layout
            .nodes
            .iter()
            .find(|b| b.rig == rig && b.node == node)
            .map(|b| b.rect)
    };
    json!({
        "director": graph.director,
        "rigs": graph.rigs.iter().enumerate().map(|(r, rig)| json!({
            "name": rig.name,
            "tags": rig.tags,
            "director_choice": rig.director_choice,
            "blend_out": rig.blend_out,
            "label_rect": layout.rig_labels.get(r),
            "nodes": rig.nodes.iter().enumerate().map(|(n, node)| json!({
                "index": node.index,
                "path": node.path,
                "depth": node.depth,
                "kind": node.kind,
                "summary": node.summary,
                "switched_off": node.switched_off,
                "rect": rect_of(r, n),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "edges": graph.edges.iter().map(|edge| json!({
            "from": edge.from,
            "to": edge.to,
            "from_rigs": edge.from_rigs,
            "to_rigs": edge.to_rigs,
            "owner": edge.owner,
            "blend": edge.blend,
            "seconds": edge.seconds,
            "start_facing": edge.start_facing,
            "hold_outgoing": edge.hold_outgoing,
            "share_state": edge.share_state,
            "interruptible": edge.interruptible,
            "conditions": edge.conditions,
        })).collect::<Vec<_>>(),
        "width": layout.width,
        "height": layout.height,
    })
}

/// What a running driver adds to the drawing: rig weights and each node's state, from `camera.inspect`.
#[must_use]
pub fn rig_weight(live: Option<&Value>, rig: &str) -> Option<f64> {
    live?["rigs"]
        .as_array()?
        .iter()
        .find(|entry| entry["name"] == rig)?["weight"]
        .as_f64()
}

/// `Some(skipped)` when the live driver ran node `index` of `rig` this frame.
#[must_use]
pub fn node_live(live: Option<&Value>, rig: &str, index: usize) -> Option<bool> {
    let entry = live?["rigs"].as_array()?.iter().find(|e| e["name"] == rig)?;
    entry["trace"]
        .as_array()?
        .iter()
        .find(|t| t["index"].as_u64() == Some(index as u64))?["skipped"]
        .as_bool()
}

/// The node of `asset` at `path`, for the panel's details.
#[must_use]
pub fn node_at<'a>(rig: &'a RigDef, path: &[usize]) -> Option<&'a Node> {
    let (first, rest) = path.split_first()?;
    let mut node = rig.root.get(*first)?;
    for step in rest {
        node = node.children().get(*step)?;
    }
    Some(node)
}

/// The node of `rig` at `path`, to change it.
pub fn node_at_mut<'a>(rig: &'a mut RigDef, path: &[usize]) -> Option<&'a mut Node> {
    let (first, rest) = path.split_first()?;
    let mut node = rig.root.get_mut(*first)?;
    for step in rest {
        node = match node {
            Node::Group { nodes, .. } | Node::When { nodes, .. } => nodes.get_mut(*step)?,
            _ => return None,
        };
    }
    Some(node)
}

/// The rig nodes in Nodes list order: `(rig index, node)` for every rig's nodes in evaluation
/// order.
#[must_use]
pub fn node_rows(graph: &RigGraph) -> Vec<(usize, &GraphNode)> {
    graph
        .rigs
        .iter()
        .enumerate()
        .flat_map(|(r, rig)| rig.nodes.iter().map(move |n| (r, n)))
        .collect()
}

/// The Nodes list row (and accessible) name: "walk node 2: boom, length param len".
#[must_use]
pub fn node_row_name(graph: &RigGraph, rig: usize, node: &GraphNode) -> String {
    let rig_name = graph.rigs.get(rig).map_or("", |r| r.name.as_str());
    let depth = if node.depth > 0 {
        format!(", level {}", node.depth + 1)
    } else {
        String::new()
    };
    format!(
        "{rig_name} node {}: {}, {}{depth}",
        node.index + 1,
        node.kind,
        node.summary
    )
}

/// The memory id of the Nodes list's keyboard state.
fn list_id() -> egui::Id {
    egui::Id::new("camera-rig-graph-nodes")
}

/// The node whose Nodes list row has the keyboard focus, as `(rig, path)`: the canvas draws its
/// focus ring around it.
#[must_use]
pub fn focused_node(ctx: &egui::Context, graph: &RigGraph) -> Option<(usize, Vec<usize>)> {
    let keys: crate::canvas_keys::ListKeys = ctx.data(|d| d.get_temp(list_id())).unwrap_or_default();
    let row = keys.focused()?;
    node_rows(graph).get(row).map(|(r, n)| (*r, n.path.clone()))
}

/// The Nodes list under the rig graph (FU-5, the convention of `canvas_keys`): one focusable row
/// per node; Up / Down move the focus, Enter selects the node as a click on its box does (its
/// fields show below). The graph is laid out automatically and its nodes are edited in the fields
/// below, so the list has no move, delete or wire keys. The list's state lives in egui memory.
pub fn node_list_ui(ui: &mut egui::Ui, graph: &RigGraph, selected: &mut Option<(usize, Vec<usize>)>) {
    let rows = node_rows(graph);
    let names: Vec<String> = rows.iter().map(|(r, n)| node_row_name(graph, *r, n)).collect();
    let mut keys: crate::canvas_keys::ListKeys = ui.data(|d| d.get_temp(list_id())).unwrap_or_default();
    let at = selected
        .as_ref()
        .and_then(|(r, path)| rows.iter().position(|(rr, n)| rr == r && n.path == *path));
    ui.label(format!("Nodes ({})", rows.len()));
    let name = |i: usize| names.get(i).cloned().unwrap_or_default();
    for event in crate::canvas_keys::list_rows(
        ui,
        "camera-rig-graph-node-rows",
        &mut keys,
        rows.len(),
        crate::canvas_keys::LIST_HEIGHT / 2.0,
        at,
        &name,
    ) {
        if let crate::canvas_keys::RowKey::Activate(i) = event
            && let Some((r, n)) = rows.get(i)
        {
            *selected = Some((*r, n.path.clone()));
        }
    }
    ui.data_mut(|d| d.insert_temp(list_id(), keys));
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASSET: &str = r#"(format_version: 1, name: "g",
        params: { "len": Float(4.0) },
        variables: [(name: "aiming", default: Bool(false))],
        rigs: [
            (name: "walk", tags: ["ground"], root: [
                Fov(degrees: Literal(60.0)),
                Boom(length: Param("len")),
                Group(enabled: Literal(false), nodes: [
                    Fov(degrees: Literal(70.0)),
                    When(cond: VarBool("aiming", true), nodes: [Fov(degrees: Literal(40.0))]),
                ]),
                Fov(degrees: Literal(65.0)),
            ], transitions: [(from: Rig("walk"), to: Rig("aim"), blend: Smooth(0.25))]),
            (name: "aim", tags: ["ground"], root: [Fov(degrees: Var("aim_fov"))]),
            (name: "orbit", root: [Fov(degrees: Literal(50.0))]),
        ],
        transitions: [
            (from: Any, to: Tag("ground"), blend: Cut, start_facing: CarryOver, conditions: [Elapsed(1.0)]),
        ],
        director: Rules(default: "walk", rules: [(when: VarBool("aiming", true), rig: "aim")]))"#;

    fn asset() -> CameraAsset {
        CameraAsset::from_ron(ASSET).expect("parses")
    }

    #[test]
    fn nodes_are_numbered_in_evaluation_order_with_their_paths() {
        let graph = rig_graph(&asset());
        let walk = &graph.rigs[0];
        let kinds: Vec<&str> = walk.nodes.iter().map(|n| n.kind).collect();
        assert_eq!(kinds, ["fov", "boom", "group", "fov", "when", "fov", "fov"]);
        let indices: Vec<usize> = walk.nodes.iter().map(|n| n.index).collect();
        assert_eq!(indices, [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(walk.nodes[5].path, [2, 1, 0]);
        assert_eq!(walk.nodes[5].depth, 2);
        assert_eq!(walk.nodes[6].path, [3]);
        assert!(walk.nodes[2].switched_off);
        assert_eq!(walk.nodes[1].summary, "length param len");
        assert_eq!(walk.nodes[4].summary, "1 nodes if aiming == true");
        assert_eq!(graph.rigs[1].nodes[0].summary, "var aim_fov deg");
    }

    #[test]
    fn rail_and_crane_nodes_show_in_the_graph_with_a_summary() {
        let asset = CameraAsset::from_ron(
            r#"(format_version: 2, name: "rc",
                rigs: [(name: "dolly", root: [
                    Rail(spline: "track", drive: Time(speed: 2.5)),
                    Rail(spline: "track", drive: Follow(target: DriveTarget, lead: 3.0)),
                    Crane(base: Ground((0.0, 0.0, 0.0)), length: Literal(4.0)),
                ])],
                director: Fixed("dolly"))"#,
        )
        .expect("parses");
        let nodes = &rig_graph(&asset).rigs[0].nodes;
        let kinds: Vec<&str> = nodes.iter().map(|n| n.kind).collect();
        assert_eq!(kinds, ["rail", "rail", "crane"]);
        assert_eq!(nodes[0].summary, "track 2.5 m/s");
        assert_eq!(nodes[1].summary, "track follow, lead 3");
        assert_eq!(nodes[2].summary, "arm 4.0 m");
    }

    #[test]
    fn the_path_finds_the_node_the_numbering_names() {
        let asset = asset();
        let graph = rig_graph(&asset);
        for (rig, def) in graph.rigs.iter().zip(&asset.rigs) {
            for node in &rig.nodes {
                assert_eq!(
                    node_at(def, &node.path).map(Node::kind),
                    Some(node.kind),
                    "{:?}",
                    node.path
                );
            }
        }
        let mut asset = asset;
        let walk = &mut asset.rigs[0];
        assert_eq!(node_at_mut(walk, &[2, 1, 0]).map(|n| n.kind()), Some("fov"));
        assert!(node_at_mut(walk, &[0, 0]).is_none(), "a Fov has no children");
        assert!(node_at(walk, &[]).is_none());
        assert!(node_at(walk, &[9]).is_none());
    }

    #[test]
    fn edges_resolve_their_ends_and_keep_the_search_order() {
        let graph = rig_graph(&asset());
        assert_eq!(graph.edges.len(), 2);
        let own = &graph.edges[0];
        assert_eq!(own.owner.as_deref(), Some("walk"));
        assert_eq!(
            (own.from_rigs.as_slice(), own.to_rigs.as_slice()),
            (&[0][..], &[1][..])
        );
        assert_eq!((own.blend, own.seconds), ("smooth", 0.25));
        let shared = &graph.edges[1];
        assert_eq!(shared.owner, None);
        assert_eq!((shared.from.as_str(), shared.to.as_str()), ("any", "tag:ground"));
        assert_eq!(shared.from_rigs, [0, 1, 2]);
        assert_eq!(shared.to_rigs, [0, 1]);
        assert_eq!((shared.blend, shared.start_facing), ("cut", "carry_over"));
        assert_eq!(shared.conditions, ["elapsed >= 1"]);
    }

    #[test]
    fn the_director_marks_the_rigs_it_can_name() {
        let graph = rig_graph(&asset());
        assert!(graph.rigs[0].director_choice && graph.rigs[1].director_choice);
        assert!(!graph.rigs[2].director_choice);
        assert_eq!(graph.director, "rules: aim if aiming == true, else walk");
    }

    #[test]
    fn the_layout_is_deterministic_and_boxes_never_overlap() {
        let graph = rig_graph(&asset());
        let layout = layout_graph(&graph);
        assert_eq!(layout, layout_graph(&graph));
        assert_eq!(layout.nodes.len(), 9);
        for (i, a) in layout.nodes.iter().enumerate() {
            for b in &layout.nodes[i + 1..] {
                let [ax, ay, aw, ah] = a.rect;
                let [bx, by, bw, bh] = b.rect;
                let apart = ax + aw <= bx || bx + bw <= ax || ay + ah <= by || by + bh <= ay;
                assert!(apart, "{a:?} and {b:?} overlap");
            }
            let [x, y, w, h] = a.rect;
            assert!(
                x + w <= layout.width && y + h <= layout.height,
                "{a:?} leaves the canvas"
            );
        }
        // Own transition walk -> aim, and the shared any -> ground to every other rig.
        assert_eq!(layout.arcs.len(), 1 + 3 + 1);
        assert!(layout.arcs.iter().all(|arc| arc.x < layout.nodes[0].rect[0]));
    }

    #[test]
    fn a_rig_with_no_nodes_still_gets_a_row() {
        let graph = RigGraph {
            rigs: vec![GraphRig {
                name: "empty".into(),
                tags: Vec::new(),
                nodes: Vec::new(),
                director_choice: false,
                blend_out: None,
            }],
            ..RigGraph::default()
        };
        let layout = layout_graph(&graph);
        assert_eq!(layout.rig_labels.len(), 1);
        assert!(layout.height > 0.0);
        assert!(layout_graph(&RigGraph::default()).nodes.is_empty());
    }

    #[test]
    fn the_live_overlay_reads_the_inspection() {
        let live = json!({ "rigs": [
            { "name": "walk", "weight": 0.25, "trace": [
                { "index": 0, "skipped": false }, { "index": 1, "skipped": true },
            ]},
        ]});
        assert_eq!(rig_weight(Some(&live), "walk"), Some(0.25));
        assert_eq!(rig_weight(Some(&live), "aim"), None);
        assert_eq!(rig_weight(None, "walk"), None);
        assert_eq!(node_live(Some(&live), "walk", 1), Some(true));
        assert_eq!(node_live(Some(&live), "walk", 0), Some(false));
        assert_eq!(
            node_live(Some(&live), "walk", 5),
            None,
            "this frame it did not run"
        );
    }

    /// FU-5: the Nodes list reaches every node by keyboard (Tab, Down) and Enter selects the
    /// node as a click on its box does: the same `(rig, path)` the canvas click stores.
    #[test]
    fn the_nodes_list_selects_by_keyboard_what_a_click_selects() {
        use crate::canvas_keys::test_input::{focused_name, frame, key, tab_to};
        use egui::{Key, Modifiers};
        let graph = rig_graph(&asset());
        let rows = node_rows(&graph);
        assert_eq!(rows.len(), 9, "7 walk nodes, 1 aim, 1 orbit");
        let ctx = egui::Context::default();
        let mut selected = None;
        let first = node_row_name(&graph, 0, &graph.rigs[0].nodes[0]);
        assert_eq!(first, "walk node 1: fov, 60.0 deg");
        tab_to(&ctx, [600.0, 600.0], &first, false, 10, |ui| {
            node_list_ui(ui, &graph, &mut selected)
        });
        for _ in 0..5 {
            frame(
                &ctx,
                [600.0, 600.0],
                vec![key(Key::ArrowDown, Modifiers::NONE)],
                |ui| node_list_ui(ui, &graph, &mut selected),
            );
        }
        let at = frame(&ctx, [600.0, 600.0], vec![], |ui| {
            node_list_ui(ui, &graph, &mut selected)
        });
        assert_eq!(
            focused_name(&at),
            Some(node_row_name(&graph, 0, &graph.rigs[0].nodes[5])),
            "a nested node is a row"
        );
        assert_eq!(
            focused_node(&ctx, &graph),
            Some((0, vec![2, 1, 0])),
            "the canvas rings it"
        );
        frame(
            &ctx,
            [600.0, 600.0],
            vec![key(Key::Enter, Modifiers::NONE)],
            |ui| node_list_ui(ui, &graph, &mut selected),
        );
        assert_eq!(selected, Some((0, vec![2, 1, 0])), "the click's selection");
    }
}
