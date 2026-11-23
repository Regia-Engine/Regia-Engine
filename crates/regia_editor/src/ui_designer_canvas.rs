// SPDX-License-Identifier: MPL-2.0
//! Direct manipulation in the UI designer's preview (ADR-9730, `docs/specs/81-ui-designer.md`
//! § 4.7): a dragged or resized rect becomes the `.ui.ron` edit that lays the node out there, with
//! snapping to the parent's edges, the siblings and a grid; align and distribute; and the canvas
//! view's zoom, pan, rulers and device frame. Headless: the panel and `editor.ui_designer_edit`
//! (`transform`, `align`) plan through the same functions over a laid-out [`PreviewFrame`].
//!
//! Rects are `[x, y, width, height]` in the preview frame's physical pixels; the document is written
//! in UI pixels (`frame.scale`). An absolutely positioned node writes `style.inset` and
//! `style.size`; a node in the flow writes `style.size` and `style.margin`, and a move that crosses
//! a sibling reorders it through [`drop_move`]. A length keeps its kind: points stay points, a
//! percentage stays a percentage of the parent.

use regia_ui::Dimension;
use regia_ui::authoring::UiNodeDef;
use regia_ui::style::{FlexDirection, PositionType, Style};
use serde_json::Value;

use crate::ui_designer::{
    DesignOp, DesignerError, DropZone, NodePath, drop_move, node_at, node_json, path_string,
};
use crate::ui_designer_preview::PreviewFrame;

/// `[x, y, width, height]` in the preview frame's physical pixels.
pub type Rect = [f32; 4];

/// What a move or resize snaps to. Guides are editor state: nothing of this is saved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnapSettings {
    /// The parent's left, right, top and bottom edges and its centre lines.
    pub edges: bool,
    /// The siblings' edges and centre lines.
    pub siblings: bool,
    /// Grid spacing in UI pixels from the parent's corner (`None`: no grid).
    pub grid: Option<f32>,
    /// How close (physical px of the frame) an edge must come to snap.
    pub threshold: f32,
}

impl SnapSettings {
    /// No snapping.
    pub const OFF: Self = Self {
        edges: false,
        siblings: false,
        grid: None,
        threshold: 0.0,
    };

    /// Edges, siblings and an 8 px grid within 6 px.
    pub const ON: Self = Self {
        edges: true,
        siblings: true,
        grid: Some(8.0),
        threshold: 6.0,
    };
}

impl Default for SnapSettings {
    fn default() -> Self {
        Self::ON
    }
}

/// A guide line's direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// A vertical line at `x = at`.
    X,
    /// A horizontal line at `y = at`.
    Y,
}

/// A guide line the panel draws while a snapped drag runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Guide {
    /// Vertical (`X`) or horizontal (`Y`).
    pub axis: Axis,
    /// Where the line is (frame px).
    pub at: f32,
    /// Where it starts and ends along the other axis (frame px).
    pub span: [f32; 2],
    /// What was snapped to: `parent`, `sibling` or `grid`.
    pub source: &'static str,
}

/// Move keeps the size; resize takes the whole rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformMode {
    /// Move to the rect's position (its size is ignored).
    Move,
    /// Take the rect's position and size; edges that did not move stay.
    Resize,
}

/// What the pointer holds in the preview: the node's body or one of its eight resize handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    /// The body: a move.
    Body,
    /// Top edge.
    N,
    /// Bottom edge.
    S,
    /// Right edge.
    E,
    /// Left edge.
    W,
    /// Top-right corner.
    NE,
    /// Top-left corner.
    NW,
    /// Bottom-right corner.
    SE,
    /// Bottom-left corner.
    SW,
}

impl Handle {
    /// The eight resize handles.
    pub const RESIZE: [Self; 8] = [
        Self::NW,
        Self::N,
        Self::NE,
        Self::E,
        Self::SE,
        Self::S,
        Self::SW,
        Self::W,
    ];

    /// Which edges it moves: left, top, right, bottom.
    pub fn edges(self) -> [bool; 4] {
        match self {
            Self::Body => [true; 4],
            Self::N => [false, true, false, false],
            Self::S => [false, false, false, true],
            Self::E => [false, false, true, false],
            Self::W => [true, false, false, false],
            Self::NE => [false, true, true, false],
            Self::NW => [true, true, false, false],
            Self::SE => [false, false, true, true],
            Self::SW => [true, false, false, true],
        }
    }

    /// Where the handle sits on `rect` (frame px).
    pub fn anchor(self, rect: Rect) -> [f32; 2] {
        let [x, y, w, h] = rect;
        let [l, t, r, b] = self.edges();
        let ax = if l && !r {
            x
        } else if r && !l {
            x + w
        } else {
            x + w / 2.0
        };
        let ay = if t && !b {
            y
        } else if b && !t {
            y + h
        } else {
            y + h / 2.0
        };
        [ax, ay]
    }

    /// `start` with the pointer moved by `delta` (frame px) on this handle; a size never goes under
    /// 1 px.
    pub fn drag(self, start: Rect, delta: [f32; 2]) -> Rect {
        let [x, y, w, h] = start;
        let [l, t, r, b] = self.edges();
        if self == Self::Body {
            return [x + delta[0], y + delta[1], w, h];
        }
        let (mut x0, mut y0, mut x1, mut y1) = (x, y, x + w, y + h);
        if l {
            x0 = (x0 + delta[0]).min(x1 - 1.0);
        }
        if r {
            x1 = (x1 + delta[0]).max(x0 + 1.0);
        }
        if t {
            y0 = (y0 + delta[1]).min(y1 - 1.0);
        }
        if b {
            y1 = (y1 + delta[1]).max(y0 + 1.0);
        }
        [x0, y0, x1 - x0, y1 - y0]
    }
}

/// One move or resize of a node.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransformRequest<'a> {
    /// The node (not the root).
    pub node: &'a [usize],
    /// The wanted rect (frame px).
    pub rect: Rect,
    /// Move or resize.
    pub mode: TransformMode,
    /// What to snap to.
    pub snap: SnapSettings,
    /// Resize keeping the node's aspect ratio (Shift in the panel).
    pub keep_aspect: bool,
    /// A move of a node in the flow may reorder it among its siblings (else it writes its margin).
    pub reorder: bool,
}

/// The edit a transform makes and what the panel shows while it runs.
#[derive(Debug, Clone, PartialEq)]
pub struct TransformPlan {
    /// The edit: a `set` of `style`, a `move` (reorder), or nothing when the rect is where it was.
    pub op: Option<DesignOp>,
    /// The rect after snapping and the aspect (frame px).
    pub rect: Rect,
    /// Guide lines of the snaps that applied.
    pub guides: Vec<Guide>,
    /// What it writes: `inset`, `size`, `margin`, `order`.
    pub writes: Vec<&'static str>,
}

/// The rect of the node at `path` in `frame` (the first sample row of a repeat template).
pub fn rect_of(frame: &PreviewFrame, path: &[usize]) -> Option<Rect> {
    let key = path_string(path);
    frame
        .nodes
        .iter()
        .filter(|n| n.path == key)
        .find(|n| n.visible)
        .or_else(|| frame.nodes.iter().find(|n| n.path == key))
        .map(|n| n.rect)
}

fn round2(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}

/// `d` moved by `delta` frame px: points by `delta / scale`, a percentage by its share of
/// `extent`; `Auto` counts as 0 (margins) or the node's offset in its containing block (`auto`).
fn shift(d: Dimension, delta: f32, scale: f32, extent: f32, auto: f32) -> Dimension {
    match d {
        Dimension::Points(p) => Dimension::Points(round2(p + delta / scale)),
        Dimension::Percent(p) if extent > 0.0 => Dimension::Percent(round2(p + delta / extent * 100.0)),
        Dimension::Percent(p) => Dimension::Percent(p),
        Dimension::Auto => Dimension::Points(round2(auto + delta / scale)),
    }
}

/// A size of `px` frame px in `d`'s kind.
fn resize(d: Dimension, px: f32, scale: f32, extent: f32) -> Dimension {
    match d {
        Dimension::Percent(_) if extent > 0.0 => Dimension::Percent(round2(px / extent * 100.0)),
        _ => Dimension::Points(round2(px / scale)),
    }
}

/// The best snap of any of `values` to any of `targets` within `threshold`: `(shift, target,
/// source)`.
fn best_snap(
    values: &[f32],
    targets: &[(f32, &'static str)],
    threshold: f32,
) -> Option<(f32, f32, &'static str)> {
    let mut best: Option<(f32, f32, &'static str)> = None;
    for &v in values {
        for &(t, source) in targets {
            let d = t - v;
            if d.abs() <= threshold && best.is_none_or(|(b, ..)| d.abs() < b.abs()) {
                best = Some((d, t, source));
            }
        }
    }
    best
}

/// Snap targets along one axis: the parent's edges and centre, the siblings', and the grid line
/// nearest to each of `values`.
fn targets(
    axis: Axis,
    parent: Rect,
    siblings: &[Rect],
    snap: &SnapSettings,
    scale: f32,
    values: &[f32],
) -> Vec<(f32, &'static str)> {
    let lines = |r: Rect| match axis {
        Axis::X => [r[0], r[0] + r[2] / 2.0, r[0] + r[2]],
        Axis::Y => [r[1], r[1] + r[3] / 2.0, r[1] + r[3]],
    };
    let mut out = Vec::new();
    if snap.edges {
        out.extend(lines(parent).map(|v| (v, "parent")));
    }
    if snap.siblings {
        for s in siblings {
            out.extend(lines(*s).map(|v| (v, "sibling")));
        }
    }
    if let Some(grid) = snap.grid.filter(|g| *g > 0.0 && g.is_finite()) {
        let step = grid * scale;
        let origin = if axis == Axis::X { parent[0] } else { parent[1] };
        for &v in values {
            out.push((origin + ((v - origin) / step).round() * step, "grid"));
        }
    }
    out
}

fn guide(axis: Axis, at: f32, a: Rect, b: Option<Rect>, source: &'static str) -> Guide {
    let range = |r: Rect| match axis {
        Axis::X => [r[1], r[1] + r[3]],
        Axis::Y => [r[0], r[0] + r[2]],
    };
    let mut span = range(a);
    if let Some(b) = b {
        let o = range(b);
        span = [span[0].min(o[0]), span[1].max(o[1])];
    }
    Guide {
        axis,
        at,
        span,
        source,
    }
}

/// The sibling rect a snap target came from, for the guide's extent.
fn source_rect(axis: Axis, at: f32, parent: Rect, siblings: &[Rect], source: &str) -> Option<Rect> {
    if source != "sibling" {
        return Some(parent);
    }
    siblings.iter().copied().find(|r| {
        let lines = match axis {
            Axis::X => [r[0], r[0] + r[2] / 2.0, r[0] + r[2]],
            Axis::Y => [r[1], r[1] + r[3] / 2.0, r[1] + r[3]],
        };
        lines.iter().any(|v| (v - at).abs() < 1e-3)
    })
}

/// Snaps `rect` (moving the edges `moving`: left, top, right, bottom; all four for a move).
fn snap_rect(
    rect: Rect,
    moving: [bool; 4],
    is_move: bool,
    parent: Rect,
    siblings: &[Rect],
    snap: &SnapSettings,
    scale: f32,
) -> (Rect, Vec<Guide>) {
    let [mut x, mut y, mut w, mut h] = rect;
    let mut guides = Vec::new();
    if snap.threshold <= 0.0 {
        return (rect, guides);
    }
    for axis in [Axis::X, Axis::Y] {
        let (pos, size, lo, hi) = match axis {
            Axis::X => (x, w, moving[0], moving[2]),
            Axis::Y => (y, h, moving[1], moving[3]),
        };
        let values: Vec<f32> = if is_move {
            vec![pos, pos + size / 2.0, pos + size]
        } else {
            [(lo, pos), (hi, pos + size)]
                .iter()
                .filter(|(m, _)| *m)
                .map(|(_, v)| *v)
                .collect()
        };
        if values.is_empty() {
            continue;
        }
        let candidates = targets(axis, parent, siblings, snap, scale, &values);
        let Some((d, at, source)) = best_snap(&values, &candidates, snap.threshold) else {
            continue;
        };
        let (new_pos, new_size) = if is_move {
            (pos + d, size)
        } else if lo && (at - pos).abs() <= (at - (pos + size)).abs() {
            // The near edge snapped; the far edge stays.
            (pos + d, (size - d).max(1.0))
        } else {
            (pos, (size + d).max(1.0))
        };
        match axis {
            Axis::X => (x, w) = (new_pos, new_size),
            Axis::Y => (y, h) = (new_pos, new_size),
        }
        let snapped = [x, y, w, h];
        guides.push(guide(
            axis,
            at,
            snapped,
            source_rect(axis, at, parent, siblings, source),
            source,
        ));
    }
    ([x, y, w, h], guides)
}

/// The visible rects of the node's siblings (its parent's other children).
fn sibling_rects(frame: &PreviewFrame, parent: &[usize], count: usize, own: usize) -> Vec<(usize, Rect)> {
    (0..count)
        .filter(|i| *i != own)
        .filter_map(|i| {
            let mut p = parent.to_vec();
            p.push(i);
            rect_of(frame, &p).map(|r| (i, r))
        })
        .collect()
}

/// Plans one move or resize of a node over a laid-out frame of the same document.
pub fn plan_transform(
    root: &UiNodeDef,
    frame: &PreviewFrame,
    req: &TransformRequest<'_>,
) -> Result<TransformPlan, DesignerError> {
    let bad_node = |m: String| DesignerError::new("bad_node", m);
    let Some((&own, parent_path)) = req.node.split_last() else {
        return Err(bad_node(
            "the root fills the window; it cannot move or resize".to_owned(),
        ));
    };
    let node =
        node_at(root, req.node).ok_or_else(|| bad_node(format!("no node at {}", path_string(req.node))))?;
    let parent = node_at(root, parent_path)
        .ok_or_else(|| bad_node(format!("no node at {}", path_string(parent_path))))?;
    let cur = rect_of(frame, req.node).ok_or_else(|| {
        bad_node(format!(
            "{} is not laid out in the preview",
            path_string(req.node)
        ))
    })?;
    let par = rect_of(frame, parent_path).unwrap_or([0.0, 0.0, frame.size[0], frame.size[1]]);
    if req.rect.iter().any(|v| !v.is_finite()) {
        return Err(DesignerError::new(
            "bad_value",
            "rect is [x, y, width, height] in frame px",
        ));
    }
    let scale = frame.scale.max(0.001);
    let siblings = sibling_rects(frame, parent_path, parent.children.len(), own);
    let sibling_list: Vec<Rect> = siblings.iter().map(|(_, r)| *r).collect();
    let is_move = req.mode == TransformMode::Move;
    let wanted = if is_move {
        [req.rect[0], req.rect[1], cur[2], cur[3]]
    } else {
        [
            req.rect[0],
            req.rect[1],
            req.rect[2].max(1.0),
            req.rect[3].max(1.0),
        ]
    };
    let eps = 0.01;
    let moving = if is_move {
        [true; 4]
    } else {
        [
            (wanted[0] - cur[0]).abs() > eps,
            (wanted[1] - cur[1]).abs() > eps,
            (wanted[0] + wanted[2] - cur[0] - cur[2]).abs() > eps,
            (wanted[1] + wanted[3] - cur[1] - cur[3]).abs() > eps,
        ]
    };
    let (mut rect, guides) = snap_rect(wanted, moving, is_move, par, &sibling_list, &req.snap, scale);
    if req.keep_aspect && !is_move && cur[2] > 0.0 && cur[3] > 0.0 {
        let aspect = cur[2] / cur[3];
        let (dw, dh) = ((rect[2] - cur[2]) / cur[2], (rect[3] - cur[3]) / cur[3]);
        if dw.abs() >= dh.abs() {
            let h = (rect[2] / aspect).max(1.0);
            if moving[1] && !moving[3] {
                rect[1] = rect[1] + rect[3] - h;
            }
            rect[3] = h;
        } else {
            let w = (rect[3] * aspect).max(1.0);
            if moving[0] && !moving[2] {
                rect[0] = rect[0] + rect[2] - w;
            }
            rect[2] = w;
        }
    }
    let mut writes = Vec::new();
    let absolute = node.style.position == PositionType::Absolute;
    // A move of a node in the flow that crosses a sibling's slot reorders it.
    if is_move && !absolute && req.reorder {
        let row = matches!(
            parent.style.direction,
            FlexDirection::Row | FlexDirection::RowReverse
        );
        let main = |r: Rect| if row { (r[0], r[2]) } else { (r[1], r[3]) };
        let (pos, size) = main(rect);
        let centre = pos + size / 2.0;
        let flow_sibling = siblings.iter().find(|(i, r)| {
            let (p, s) = main(*r);
            let in_flow = parent
                .children
                .get(*i)
                .is_some_and(|c| c.style.position != PositionType::Absolute);
            in_flow && s > 0.0 && centre >= p && centre <= p + s
        });
        if let Some(&(i, r)) = flow_sibling {
            let (p, s) = main(r);
            let zone = if centre < p + s / 2.0 {
                DropZone::Before
            } else {
                DropZone::After
            };
            let mut onto = parent_path.to_vec();
            onto.push(i);
            if let Some(op) = drop_move(req.node, &onto, zone) {
                writes.push("order");
                return Ok(TransformPlan {
                    op: Some(op),
                    rect,
                    guides,
                    writes,
                });
            }
        }
    }
    let mut style: Style = node.style.clone();
    let (dl, dt) = (rect[0] - cur[0], rect[1] - cur[1]);
    let (dr, db) = (
        rect[0] + rect[2] - cur[0] - cur[2],
        rect[1] + rect[3] - cur[1] - cur[3],
    );
    let resized = [(rect[2] - cur[2]).abs() > eps, (rect[3] - cur[3]).abs() > eps];
    if absolute {
        // The containing block is the parent's padding box.
        let border = |d: Dimension| match d {
            Dimension::Points(p) => p * scale,
            _ => 0.0,
        };
        let base = [
            par[0] + border(parent.style.border.left),
            par[1] + border(parent.style.border.top),
        ];
        let inset = &mut style.inset;
        for (axis, (lo, hi, d_lo, d_hi, extent, offset)) in [
            (
                0,
                (
                    &mut inset.left,
                    &mut inset.right,
                    dl,
                    dr,
                    par[2],
                    (cur[0] - base[0]) / scale,
                ),
            ),
            (
                1,
                (
                    &mut inset.top,
                    &mut inset.bottom,
                    dt,
                    db,
                    par[3],
                    (cur[1] - base[1]) / scale,
                ),
            ),
        ] {
            let (lo_set, hi_set) = (*lo != Dimension::Auto, *hi != Dimension::Auto);
            if lo_set && d_lo.abs() > eps {
                *lo = shift(*lo, d_lo, scale, extent, 0.0);
            }
            if hi_set && d_hi.abs() > eps {
                *hi = shift(*hi, -d_hi, scale, extent, 0.0);
            }
            if !lo_set && !hi_set && d_lo.abs() > eps {
                *lo = shift(Dimension::Auto, d_lo, scale, extent, offset);
            }
            let stretched = lo_set && hi_set && style.size[axis] == Dimension::Auto;
            if resized[axis] && !stretched {
                let px = if axis == 0 { rect[2] } else { rect[3] };
                style.size[axis] = resize(style.size[axis], px, scale, extent);
            }
        }
        if style.inset != node.style.inset {
            writes.push("inset");
        }
    } else {
        // In the flow: a move writes the margin; a resize writes the size, and a moved left or top
        // edge also the margin, so the far edge stays.
        if dl.abs() > eps {
            style.margin.left = shift(style.margin.left, dl, scale, par[2], 0.0);
        }
        if dt.abs() > eps {
            style.margin.top = shift(style.margin.top, dt, scale, par[2], 0.0);
        }
        for (axis, &changed) in resized.iter().enumerate() {
            if changed {
                let (px, extent) = if axis == 0 {
                    (rect[2], par[2])
                } else {
                    (rect[3], par[3])
                };
                style.size[axis] = resize(style.size[axis], px, scale, extent);
            }
        }
        if style.margin != node.style.margin {
            writes.push("margin");
        }
    }
    if style.size != node.style.size {
        writes.push("size");
    }
    let op = (style != node.style).then(|| {
        let mut moved = node.clone();
        moved.children = Vec::new();
        moved.style = style;
        DesignOp::Set {
            node: req.node.to_vec(),
            key: "style".to_owned(),
            value: node_json(&moved).get("style").cloned().unwrap_or(Value::Null),
        }
    });
    Ok(TransformPlan {
        op,
        rect,
        guides,
        writes,
    })
}

/// The edits of a group move and what the panel shows while it runs (ADR-9980).
#[derive(Debug, Clone, PartialEq)]
pub struct GroupMovePlan {
    /// One edit per moved node (a `set` of `style`); empty when nothing moves.
    pub ops: Vec<DesignOp>,
    /// The nodes that move: the top-level ones of the request, in document order.
    pub nodes: Vec<NodePath>,
    /// The delta after snapping (frame px).
    pub delta: [f32; 2],
    /// The group's bounding rect after the move (frame px).
    pub rect: Rect,
    /// Guide lines of the snaps that applied.
    pub guides: Vec<Guide>,
}

/// The bounding rect of `rects`.
fn bounds(rects: &[Rect]) -> Rect {
    let min_x = rects.iter().map(|r| r[0]).fold(f32::INFINITY, f32::min);
    let min_y = rects.iter().map(|r| r[1]).fold(f32::INFINITY, f32::min);
    let max_x = rects
        .iter()
        .map(|r| r[0] + r[2])
        .fold(f32::NEG_INFINITY, f32::max);
    let max_y = rects
        .iter()
        .map(|r| r[1] + r[3])
        .fold(f32::NEG_INFINITY, f32::max);
    [min_x, min_y, max_x - min_x, max_y - min_y]
}

/// Plans moving several nodes by one `delta` (frame px): every top-level node of `nodes` (a node
/// inside another listed node rides along with it) moves as [`plan_transform`] moves it, without
/// snapping or reordering. The snap runs once, on the group's bounding rect: against the shared
/// parent's edges, its unselected children and the grid, or against the frame alone when the nodes
/// have different parents.
pub fn plan_group_move(
    root: &UiNodeDef,
    frame: &PreviewFrame,
    nodes: &[NodePath],
    delta: [f32; 2],
    snap: &SnapSettings,
) -> Result<GroupMovePlan, DesignerError> {
    if nodes.iter().any(|p| p.is_empty()) {
        return Err(DesignerError::new(
            "bad_node",
            "the root fills the window; it cannot move",
        ));
    }
    if delta.iter().any(|v| !v.is_finite()) {
        return Err(DesignerError::new("bad_value", "delta is [dx, dy] in frame px"));
    }
    let moving = crate::ui_designer_multi::top_level(nodes);
    if moving.is_empty() {
        return Err(DesignerError::new("bad_node", "no node to move"));
    }
    let mut rects = Vec::with_capacity(moving.len());
    for path in &moving {
        if node_at(root, path).is_none() {
            return Err(DesignerError::new(
                "bad_node",
                format!("no node at {}", path_string(path)),
            ));
        }
        rects.push(rect_of(frame, path).ok_or_else(|| {
            DesignerError::new(
                "bad_node",
                format!("{} is not laid out in the preview", path_string(path)),
            )
        })?);
    }
    let group = bounds(&rects);
    let parent_path = moving[0][..moving[0].len() - 1].to_vec();
    let shared = moving.iter().all(|p| p[..p.len() - 1] == parent_path[..]);
    let whole = [0.0, 0.0, frame.size[0], frame.size[1]];
    let (par, siblings) = if shared {
        let count = node_at(root, &parent_path).map_or(0, |p| p.children.len());
        let others: Vec<Rect> = (0..count)
            .filter(|i| {
                !moving
                    .iter()
                    .any(|p| p.last() == Some(i) && p[..p.len() - 1] == parent_path[..])
            })
            .filter_map(|i| {
                let mut p = parent_path.clone();
                p.push(i);
                rect_of(frame, &p)
            })
            .collect();
        (rect_of(frame, &parent_path).unwrap_or(whole), others)
    } else {
        (whole, Vec::new())
    };
    let wanted = [group[0] + delta[0], group[1] + delta[1], group[2], group[3]];
    let (rect, guides) = snap_rect(
        wanted,
        [true; 4],
        true,
        par,
        &siblings,
        snap,
        frame.scale.max(0.001),
    );
    let delta = [rect[0] - group[0], rect[1] - group[1]];
    let mut ops = Vec::new();
    for (path, r) in moving.iter().zip(&rects) {
        let plan = plan_transform(
            root,
            frame,
            &TransformRequest {
                node: path,
                rect: [r[0] + delta[0], r[1] + delta[1], r[2], r[3]],
                mode: TransformMode::Move,
                snap: SnapSettings::OFF,
                keep_aspect: false,
                reorder: false,
            },
        )?;
        ops.extend(plan.op);
    }
    Ok(GroupMovePlan {
        ops,
        nodes: moving,
        delta,
        rect,
        guides,
    })
}

/// How [`align_ops`] lines nodes up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignKind {
    /// Left edges on the leftmost.
    Left,
    /// Horizontal centres on the selection's centre.
    Center,
    /// Right edges on the rightmost.
    Right,
    /// Top edges on the topmost.
    Top,
    /// Vertical centres on the selection's middle.
    Middle,
    /// Bottom edges on the lowest.
    Bottom,
    /// Equal horizontal gaps between the leftmost and the rightmost (3 or more nodes).
    DistributeH,
    /// Equal vertical gaps between the topmost and the lowest (3 or more nodes).
    DistributeV,
}

impl AlignKind {
    /// Every kind, in toolbar order.
    pub const ALL: [Self; 8] = [
        Self::Left,
        Self::Center,
        Self::Right,
        Self::Top,
        Self::Middle,
        Self::Bottom,
        Self::DistributeH,
        Self::DistributeV,
    ];

    /// The wire name (`left`, `distribute_h`, …).
    pub fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
            Self::Top => "top",
            Self::Middle => "middle",
            Self::Bottom => "bottom",
            Self::DistributeH => "distribute_h",
            Self::DistributeV => "distribute_v",
        }
    }

    /// The kind with this wire name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}

/// The edits that align or distribute `nodes` (at least 2, 3 to distribute) by their laid-out
/// rects; each node moves as [`plan_transform`] moves it, without snapping or reordering.
pub fn align_ops(
    root: &UiNodeDef,
    frame: &PreviewFrame,
    nodes: &[NodePath],
    kind: AlignKind,
) -> Result<Vec<DesignOp>, DesignerError> {
    let need = if matches!(kind, AlignKind::DistributeH | AlignKind::DistributeV) {
        3
    } else {
        2
    };
    if nodes.len() < need {
        return Err(DesignerError::new(
            "bad_value",
            format!(
                "{} needs at least {need} nodes ({} given)",
                kind.name(),
                nodes.len()
            ),
        ));
    }
    let mut rects = Vec::with_capacity(nodes.len());
    for path in nodes {
        if path.is_empty() {
            return Err(DesignerError::new("bad_node", "the root cannot be aligned"));
        }
        let r = rect_of(frame, path).ok_or_else(|| {
            DesignerError::new("bad_node", format!("{} is not laid out", path_string(path)))
        })?;
        rects.push(r);
    }
    let min_x = rects.iter().map(|r| r[0]).fold(f32::INFINITY, f32::min);
    let max_x = rects
        .iter()
        .map(|r| r[0] + r[2])
        .fold(f32::NEG_INFINITY, f32::max);
    let min_y = rects.iter().map(|r| r[1]).fold(f32::INFINITY, f32::min);
    let max_y = rects
        .iter()
        .map(|r| r[1] + r[3])
        .fold(f32::NEG_INFINITY, f32::max);
    let mut targets: Vec<Rect> = rects.clone();
    match kind {
        AlignKind::Left => targets.iter_mut().for_each(|r| r[0] = min_x),
        AlignKind::Right => targets.iter_mut().for_each(|r| r[0] = max_x - r[2]),
        AlignKind::Center => targets
            .iter_mut()
            .for_each(|r| r[0] = (min_x + max_x) / 2.0 - r[2] / 2.0),
        AlignKind::Top => targets.iter_mut().for_each(|r| r[1] = min_y),
        AlignKind::Bottom => targets.iter_mut().for_each(|r| r[1] = max_y - r[3]),
        AlignKind::Middle => targets
            .iter_mut()
            .for_each(|r| r[1] = (min_y + max_y) / 2.0 - r[3] / 2.0),
        AlignKind::DistributeH | AlignKind::DistributeV => {
            let a = usize::from(kind == AlignKind::DistributeV);
            let mut order: Vec<usize> = (0..rects.len()).collect();
            order.sort_by(|&i, &j| rects[i][a].total_cmp(&rects[j][a]));
            let (lo, hi) = if a == 0 { (min_x, max_x) } else { (min_y, max_y) };
            let total: f32 = rects.iter().map(|r| r[a + 2]).sum();
            let gap = (hi - lo - total) / (rects.len() - 1) as f32;
            let mut at = lo;
            for i in order {
                targets[i][a] = at;
                at += rects[i][a + 2] + gap;
            }
        }
    }
    let mut ops = Vec::new();
    for (path, rect) in nodes.iter().zip(targets) {
        let plan = plan_transform(
            root,
            frame,
            &TransformRequest {
                node: path,
                rect,
                mode: TransformMode::Move,
                snap: SnapSettings::OFF,
                keep_aspect: false,
                reorder: false,
            },
        )?;
        ops.extend(plan.op);
    }
    Ok(ops)
}

/// Smallest preview zoom.
pub const MIN_ZOOM: f32 = 0.05;
/// Largest preview zoom.
pub const MAX_ZOOM: f32 = 16.0;

/// The preview canvas's view: zoom (screen points per frame px; `None` fits the frame into the
/// panel) and pan (screen points). Panel state, never saved.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CanvasView {
    /// Fixed zoom, or `None` to fit.
    pub zoom: Option<f32>,
    /// Offset from the centred position (screen points).
    pub pan: [f32; 2],
}

impl CanvasView {
    /// The zoom that fits `frame` into `avail`.
    pub fn fit_zoom(avail: [f32; 2], frame: [f32; 2]) -> f32 {
        (avail[0] / frame[0].max(1.0))
            .min(avail[1] / frame[1].max(1.0))
            .max(0.001)
    }

    /// Screen points per frame px.
    pub fn scale(&self, avail: [f32; 2], frame: [f32; 2]) -> f32 {
        self.zoom.unwrap_or_else(|| Self::fit_zoom(avail, frame))
    }

    /// Where the frame's corner sits in the canvas (screen points from the canvas's corner).
    pub fn origin(&self, avail: [f32; 2], frame: [f32; 2]) -> [f32; 2] {
        let k = self.scale(avail, frame);
        [
            (avail[0] - frame[0] * k) / 2.0 + self.pan[0],
            (avail[1] - frame[1] * k) / 2.0 + self.pan[1],
        ]
    }

    /// A frame point in canvas coordinates.
    pub fn to_canvas(&self, avail: [f32; 2], frame: [f32; 2], p: [f32; 2]) -> [f32; 2] {
        let k = self.scale(avail, frame);
        let o = self.origin(avail, frame);
        [o[0] + p[0] * k, o[1] + p[1] * k]
    }

    /// A canvas point in frame coordinates.
    pub fn to_frame(&self, avail: [f32; 2], frame: [f32; 2], c: [f32; 2]) -> [f32; 2] {
        let k = self.scale(avail, frame);
        let o = self.origin(avail, frame);
        [(c[0] - o[0]) / k, (c[1] - o[1]) / k]
    }

    /// Zooms by `factor` keeping the frame point under the canvas point `at` where it is.
    pub fn zoom_about(&mut self, avail: [f32; 2], frame: [f32; 2], at: [f32; 2], factor: f32) {
        let p = self.to_frame(avail, frame, at);
        let k = (self.scale(avail, frame) * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.zoom = Some(k);
        self.pan = [0.0, 0.0];
        let now = self.to_canvas(avail, frame, p);
        self.pan = [at[0] - now[0], at[1] - now[1]];
    }

    /// Back to fit.
    pub fn fit(&mut self) {
        *self = Self::default();
    }

    /// One frame px per screen point, centred.
    pub fn actual_size(&mut self) {
        self.zoom = Some(1.0);
        self.pan = [0.0, 0.0];
    }
}

/// The ruler's tick spacing in frame px at `scale` screen points per frame px: 1, 2 or 5 times a
/// power of ten, at least 48 points apart.
pub fn ruler_step(scale: f32) -> f32 {
    let scale = scale.max(1e-4);
    let mut step = 1.0;
    loop {
        for m in [1.0, 2.0, 5.0] {
            if step * m * scale >= 48.0 {
                return step * m;
            }
        }
        step *= 10.0;
        if step > 1e6 {
            return step;
        }
    }
}

/// The device drawn around a phone or handheld size preset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeviceFrame {
    /// Bezel width around the screen (frame px).
    pub bezel: f32,
    /// Corner radius of the device body (frame px).
    pub corner: f32,
    /// The camera cutout (frame px), in the safe-area inset it explains.
    pub cutout: Option<Rect>,
}

/// The device frame for a window `size` with these safe-area insets: phones (an inset on one side)
/// get a body with a camera cutout in the inset, the 16:10 handheld size a wide bezel; desktop
/// sizes none.
pub fn device_frame(size: [f32; 2], safe_area: [f32; 4]) -> Option<DeviceFrame> {
    let [w, h] = size;
    let [l, t, ..] = safe_area;
    let short = w.min(h);
    if l > 0.0 {
        let d = (l * 0.4).min(h * 0.05);
        return Some(DeviceFrame {
            bezel: short * 0.04,
            corner: short * 0.08,
            cutout: Some([l / 2.0 - d / 2.0, h / 2.0 - d / 2.0, d, d]),
        });
    }
    if t > 0.0 {
        let d = (t * 0.4).min(w * 0.05);
        return Some(DeviceFrame {
            bezel: short * 0.04,
            corner: short * 0.08,
            cutout: Some([w / 2.0 - d / 2.0, t / 2.0 - d / 2.0, d, d]),
        });
    }
    ((w - 1280.0).abs() < 0.5 && (h - 800.0).abs() < 0.5).then_some(DeviceFrame {
        bezel: short * 0.06,
        corner: short * 0.05,
        cutout: None,
    })
}

/// An anchor edit shared by the preset picker and the agent. The root itself has no parent.
/// Keep-position uses the runtime anchor inverse on the current measured rectangle; snap uses
/// zero offsets and measures automatic sizes only when centering requires a fixed size.
pub fn anchor_preset_op(
    root: &UiNodeDef,
    frame: &PreviewFrame,
    path: &[usize],
    preset: regia_ui::anchors::AnchorPreset,
    keep_position: bool,
) -> Result<DesignOp, DesignerError> {
    let (_, parent_path) = path.split_last().ok_or_else(|| {
        DesignerError::new("bad_node", "the root fills the window and has no anchor parent")
    })?;
    let node = node_at(root, path).ok_or_else(|| DesignerError::new("bad_node", "no such node"))?;
    let parent = node_at(root, parent_path).ok_or_else(|| DesignerError::new("bad_node", "no parent"))?;
    let rect = rect_of(frame, path).ok_or_else(|| DesignerError::new("bad_node", "node has no layout"))?;
    let parent_rect =
        rect_of(frame, parent_path).ok_or_else(|| DesignerError::new("bad_node", "parent has no layout"))?;
    let scale = frame.scale;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(DesignerError::new(
            "bad_value",
            "preview scale must be positive and finite",
        ));
    }
    let edge = |d| match d {
        Dimension::Points(v) => v,
        _ => 0.0,
    };
    let parent_name = path_string(parent_path);
    let node_name = path_string(path);
    let parent_rtl = frame
        .nodes
        .iter()
        .find(|node| node.path == parent_name)
        .is_some_and(|node| node.rtl);
    let rtl = frame
        .nodes
        .iter()
        .find(|node| node.path == node_name)
        .is_some_and(|node| node.rtl);
    let border = if parent_rtl {
        parent.style.mirrored().border
    } else {
        parent.style.border
    };
    let origin = [
        parent_rect[0] / scale + edge(border.left),
        parent_rect[1] / scale + edge(border.top),
    ];
    let size = [
        parent_rect[2] / scale - edge(border.left) - edge(border.right),
        parent_rect[3] / scale - edge(border.top) - edge(border.bottom),
    ];
    let mut local = [
        rect[0] / scale - origin[0],
        rect[1] / scale - origin[1],
        rect[2] / scale,
        rect[3] / scale,
    ];
    if rtl {
        local[0] = size[0] - local[0] - local[2];
    }
    let mut style = node.style.clone();
    let result = if keep_position {
        preset.apply_preserving_rect(&mut style, local, size)
    } else {
        for (axis, measured) in local[2..].iter().enumerate() {
            if style.size[axis] == Dimension::Auto && preset.pivot_ratio()[axis] == 0.5 {
                // Centering needs a fixed size. Stretch also has a 0.5 pivot but apply clears
                // its size again; edge-pinned automatic sizes keep their content behavior.
                style.size[axis] = Dimension::Points(*measured);
            }
        }
        preset.apply(&mut style, [0.0; 4])
    };
    result.map_err(|e| DesignerError::new("bad_value", format!("anchor: {e:?}")))?;
    Ok(DesignOp::Set {
        node: path.to_vec(),
        key: "style".to_owned(),
        value: serde_json::to_value(style).map_err(|e| DesignerError::new("bad_value", e.to_string()))?,
    })
}

/// Plans one atomic anchor edit, accounting for an automatic parent's reflow when its child
/// leaves normal flow. The second inverse uses the new parent box and the original child box.
pub(crate) fn anchor_preset_edit(
    file: &regia_ui::UiFile,
    frame: &PreviewFrame,
    path: &[usize],
    preset: regia_ui::anchors::AnchorPreset,
    keep_position: bool,
    mut measure: impl FnMut(&regia_ui::UiFile) -> Result<PreviewFrame, DesignerError>,
) -> Result<DesignOp, DesignerError> {
    let _span = tracing::info_span!("ui_designer_anchor_edit").entered();
    let mut op = anchor_preset_op(&file.root, frame, path, preset, keep_position)?;
    if !keep_position {
        return Ok(op);
    }
    let wanted = rect_of(frame, path).ok_or_else(|| DesignerError::new("bad_node", "node has no layout"))?;
    let name = path_string(path);
    let mut candidate = file.clone();
    for attempt in 0..2 {
        let DesignOp::Set { value, .. } = &op else {
            return Err(DesignerError::new(
                "bad_value",
                "anchor edit did not produce a style",
            ));
        };
        let node = path
            .iter()
            .try_fold(&mut candidate.root, |node, &index| node.children.get_mut(index))
            .ok_or_else(|| DesignerError::new("bad_node", "no such node"))?;
        node.style = serde_json::from_value(value.clone())
            .map_err(|e| DesignerError::new("bad_value", e.to_string()))?;
        let mut measured = measure(&candidate)?;
        let actual = measured
            .nodes
            .iter_mut()
            .find(|node| node.path == name)
            .ok_or_else(|| DesignerError::new("bad_node", "node has no layout"))?;
        if actual.rect.iter().zip(wanted).all(|(a, b)| (a - b).abs() <= 1.0) {
            return Ok(op);
        }
        if attempt == 0 {
            actual.rect = wanted;
            op = anchor_preset_op(&candidate.root, &measured, path, preset, true)?;
        }
    }
    Err(DesignerError::new(
        "bad_value",
        "this layout cannot preserve the node rectangle with the chosen anchor",
    ))
}

/// Margin and padding bands, using the runtime UI Inspector's box-model geometry.
/// Like that inspector, point-valued spacing is shown; auto and percentage spacing is omitted.
pub fn margin_padding_bands(rect: [f32; 4], scale: f32, style: &regia_ui::Style) -> [Vec<[f32; 4]>; 2] {
    let model = regia_ui::debug_overlay::BoxModel::new(rect, scale, style);
    [
        regia_ui::debug_overlay::ring_bands(model.margin, model.border),
        regia_ui::debug_overlay::ring_bands(model.padding, model.content),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_designer::UiDesigner;
    use crate::ui_designer_preview::preview;

    /// Two absolutely placed panels side by side and one column of three flow panels.
    const DOC: &str = r#"(
    version: 11,
    root: (
        style: (size: (Percent(100), Percent(100))),
        children: [
            // The anchor the resize snaps to.
            (name: "a", style: (position: absolute, inset: (50, 60, Auto, Auto), size: (100, 40))),
            (name: "b", style: (position: absolute, inset: (300, 60, Auto, Auto), size: (120, 40))),
            (name: "col", style: (direction: column, size: (200, 300), margin: (0, 200, 0, 0)), children: [
                (name: "r0", style: (size: (200, 50))),
                (name: "r1", style: (size: (200, 50))),
                (name: "r2", style: (size: (200, 50))),
            ]),
        ],
    ),
)
"#;

    fn open() -> UiDesigner {
        let mut designer = UiDesigner::default();
        designer.open_text(DOC, None).expect("open");
        designer
    }

    fn frame(designer: &UiDesigner) -> PreviewFrame {
        let doc = designer.doc().expect("doc");
        preview(doc.file(), &doc.preview, None).expect("preview")
    }

    fn req(node: &[usize], rect: Rect, mode: TransformMode, snap: SnapSettings) -> TransformRequest<'_> {
        TransformRequest {
            node,
            rect,
            mode,
            snap,
            keep_aspect: false,
            reorder: true,
        }
    }

    /// ADR-9980 acceptance: a group drag of three nodes (two absolute, one in the flow, plus a
    /// child of the flow one that rides along) moves every one by the delta in one undo step; one
    /// undo restores them all. A snapped group lands its bounding box, not a member, on the edge.
    #[test]
    fn a_group_move_of_three_nodes_moves_all_by_the_delta_in_one_undo_step() {
        let mut designer = open();
        let before_text = designer.doc().expect("doc").text().expect("text");
        let f = frame(&designer);
        let paths: Vec<NodePath> = vec![vec![0], vec![1], vec![2], vec![2, 1]];
        let before: Vec<Rect> = paths.iter().map(|p| rect_of(&f, p).expect("laid out")).collect();
        let doc = designer.doc_mut().expect("doc");
        let plan =
            plan_group_move(&doc.file().root, &f, &paths, [10.0, 20.0], &SnapSettings::OFF).expect("plan");
        assert_eq!(
            plan.nodes,
            vec![vec![0], vec![1], vec![2]],
            "the child rides along"
        );
        assert_eq!(plan.delta, [10.0, 20.0]);
        assert_eq!(plan.ops.len(), 3);
        let undo = doc.undo_len();
        doc.apply_batch(plan.ops).expect("apply");
        assert_eq!(doc.undo_len(), undo + 1, "one undo step");
        let after = frame(&designer);
        for (path, was) in paths.iter().zip(&before) {
            let now = rect_of(&after, path).expect("laid out");
            assert!(
                (now[0] - was[0] - 10.0).abs() < 0.01 && (now[1] - was[1] - 20.0).abs() < 0.01,
                "{path:?}: {was:?} -> {now:?}"
            );
        }
        let doc = designer.doc_mut().expect("doc");
        doc.undo().expect("undo");
        assert_eq!(
            doc.text().expect("text"),
            before_text,
            "one undo restores all three"
        );

        // a (50..150) and b (300..420): a drag that brings the box's left edge to 3 px snaps it to 0.
        let f = frame(&designer);
        let doc = designer.doc().expect("doc");
        let snap = SnapSettings {
            grid: None,
            ..SnapSettings::ON
        };
        let plan =
            plan_group_move(&doc.file().root, &f, &[vec![0], vec![1]], [-47.0, 0.0], &snap).expect("plan");
        assert!((plan.rect[0]).abs() < 0.01, "{:?}", plan.rect);
        assert!((plan.delta[0] + 50.0).abs() < 0.01, "{:?}", plan.delta);
        assert!(plan.guides.iter().any(|g| g.axis == Axis::X && g.at.abs() < 0.01));
        assert_eq!(
            plan_group_move(&doc.file().root, &f, &[vec![]], [1.0, 1.0], &snap)
                .expect_err("root")
                .code,
            "bad_node"
        );
    }

    /// Spec 81 § 4.7 acceptance: dragging an absolutely placed node by (+10, +20) writes its inset,
    /// the runtime layout rect moves by exactly that in the preview frame, and one undo restores
    /// the saved bytes.
    #[test]
    fn dragging_an_absolute_node_writes_its_inset_and_moves_its_rect() {
        let mut designer = open();
        let before = frame(&designer);
        let start = rect_of(&before, &[0]).expect("a");
        let saved = designer.doc().expect("doc").save_text().expect("text").0;
        assert_eq!(saved, DOC, "the source round-trips");
        let plan = plan_transform(
            &designer.doc().expect("doc").file().root,
            &before,
            &req(
                &[0],
                Handle::Body.drag(start, [10.0, 20.0]),
                TransformMode::Move,
                SnapSettings::OFF,
            ),
        )
        .expect("plan");
        assert_eq!(plan.writes, vec!["inset"]);
        let doc = designer.doc_mut().expect("doc");
        doc.apply_merged(plan.op.expect("an edit")).expect("apply");
        doc.end_merge();
        let after = frame(&designer);
        let moved = rect_of(&after, &[0]).expect("a");
        assert!(
            (moved[0] - start[0] - 10.0).abs() < 0.01 && (moved[1] - start[1] - 20.0).abs() < 0.01,
            "{start:?} -> {moved:?}"
        );
        assert_eq!(
            [moved[2], moved[3]],
            [start[2], start[3]],
            "a move keeps the size"
        );
        let doc = designer.doc_mut().expect("doc");
        let inset = doc.file().root.children[0].style.inset;
        assert_eq!(
            (inset.left, inset.top),
            (Dimension::Points(60.0), Dimension::Points(80.0)),
            "the inset is written"
        );
        assert!(
            doc.save_text().expect("text").0.contains("// The anchor"),
            "comments stay"
        );
        assert_eq!(doc.undo_len(), 1, "one undo step");
        doc.undo().expect("undo");
        assert_eq!(
            doc.save_text().expect("text").0,
            saved,
            "one undo restores the bytes"
        );
    }

    /// Spec 81 § 4.7 acceptance: a resize with snapping lands on the sibling's edge (b's left
    /// edge dragged to 3 px right of a's right edge snaps onto it), with a guide on that line.
    #[test]
    fn a_snapped_resize_lands_on_the_siblings_edge() {
        let mut designer = open();
        let before = frame(&designer);
        let a = rect_of(&before, &[0]).expect("a");
        let b = rect_of(&before, &[1]).expect("b");
        let a_right = a[0] + a[2];
        let wanted = Handle::W.drag(b, [a_right + 3.0 - b[0], 0.0]);
        let snap = SnapSettings {
            grid: None,
            ..SnapSettings::ON
        };
        let plan = plan_transform(
            &designer.doc().expect("doc").file().root,
            &before,
            &req(&[1], wanted, TransformMode::Resize, snap),
        )
        .expect("plan");
        assert!((plan.rect[0] - a_right).abs() < 0.01, "{:?}", plan.rect);
        assert!(
            plan.guides
                .iter()
                .any(|g| g.axis == Axis::X && (g.at - a_right).abs() < 0.01 && g.source == "sibling"),
            "{:?}",
            plan.guides
        );
        designer
            .doc_mut()
            .expect("doc")
            .apply(plan.op.expect("edit"))
            .expect("apply");
        let after = frame(&designer);
        let b2 = rect_of(&after, &[1]).expect("b");
        assert!((b2[0] - a_right).abs() < 0.01, "laid out on the edge: {b2:?}");
        assert!(
            (b2[0] + b2[2] - (b[0] + b[2])).abs() < 0.01,
            "the right edge stays: {b:?} -> {b2:?}"
        );
    }

    /// A flow node dragged over the next sibling's lower half reorders (one `move`); a small drag
    /// that crosses nothing writes the margin; a resize writes the size; Shift keeps the aspect.
    #[test]
    fn flow_nodes_reorder_take_margins_and_resize() {
        let designer = open();
        let f = frame(&designer);
        let root = &designer.doc().expect("doc").file().root;
        let r0 = rect_of(&f, &[2, 0]).expect("r0");
        let r1 = rect_of(&f, &[2, 1]).expect("r1");
        let over_r1 = [r0[0], r1[1] + r1[3] * 0.6 - r0[3] / 2.0, r0[2], r0[3]];
        let plan = plan_transform(
            root,
            &f,
            &req(&[2, 0], over_r1, TransformMode::Move, SnapSettings::OFF),
        )
        .expect("plan");
        assert_eq!(plan.writes, vec!["order"]);
        assert_eq!(
            plan.op,
            Some(DesignOp::Move {
                node: vec![2, 0],
                parent: vec![2],
                index: Some(1),
            })
        );
        let nudge = [r0[0] + 4.0, r0[1] + 2.0, r0[2], r0[3]];
        let mut no_reorder = req(&[2, 0], nudge, TransformMode::Move, SnapSettings::OFF);
        no_reorder.reorder = false;
        let plan = plan_transform(root, &f, &no_reorder).expect("plan");
        assert_eq!(plan.writes, vec!["margin"]);
        let plan = plan_transform(
            root,
            &f,
            &req(
                &[2, 0],
                [r0[0], r0[1], 150.0, 50.0],
                TransformMode::Resize,
                SnapSettings::OFF,
            ),
        )
        .expect("plan");
        assert_eq!(plan.writes, vec!["size"]);
        let mut aspect = req(
            &[2, 0],
            [r0[0], r0[1], 100.0, 50.0],
            TransformMode::Resize,
            SnapSettings::OFF,
        );
        aspect.keep_aspect = true;
        let plan = plan_transform(root, &f, &aspect).expect("plan");
        assert!(
            (plan.rect[2] / plan.rect[3] - r0[2] / r0[3]).abs() < 1e-3,
            "{:?}",
            plan.rect
        );
    }

    /// Align left puts both absolute nodes' left edges on the leftmost; distribute needs three.
    #[test]
    fn align_left_and_distribute() {
        let mut designer = open();
        let f = frame(&designer);
        let root = designer.doc().expect("doc").file().root.clone();
        let ops = align_ops(&root, &f, &[vec![0], vec![1]], AlignKind::Left).expect("align");
        designer.doc_mut().expect("doc").apply_batch(ops).expect("apply");
        let g = frame(&designer);
        assert_eq!(rect_of(&g, &[0]).map(|r| r[0]), rect_of(&g, &[1]).map(|r| r[0]));
        let e = align_ops(&root, &f, &[vec![0], vec![1]], AlignKind::DistributeH).expect_err("two");
        assert_eq!(e.code, "bad_value");
        let three = [vec![2, 0], vec![2, 1], vec![2, 2]];
        let ops = align_ops(&root, &f, &three, AlignKind::DistributeV).expect("distribute");
        assert!(ops.is_empty(), "already evenly spaced: {ops:?}");
    }

    /// Zoom about a point keeps it under the pointer; fit and 100 % reset; ruler steps are 1-2-5.
    #[test]
    fn canvas_view_zoom_pan_and_rulers() {
        let avail = [800.0, 500.0];
        let frame = [1920.0, 1080.0];
        let mut view = CanvasView::default();
        let at = [300.0, 200.0];
        let p = view.to_frame(avail, frame, at);
        view.zoom_about(avail, frame, at, 2.5);
        let q = view.to_canvas(avail, frame, p);
        assert!(
            (q[0] - at[0]).abs() < 1e-3 && (q[1] - at[1]).abs() < 1e-3,
            "{q:?}"
        );
        view.actual_size();
        assert_eq!(view.scale(avail, frame), 1.0);
        view.fit();
        assert!((view.scale(avail, frame) - 800.0 / 1920.0).abs() < 1e-6);
        assert_eq!(ruler_step(1.0), 50.0);
        assert_eq!(ruler_step(0.4), 200.0);
        assert_eq!(ruler_step(10.0), 5.0);
        assert!(device_frame([2400.0, 1080.0], [110.0, 0.0, 60.0, 0.0]).is_some_and(|d| d.cutout.is_some()));
        assert!(device_frame([1280.0, 800.0], [0.0; 4]).is_some());
        assert!(device_frame([1920.0, 1080.0], [0.0; 4]).is_none());
    }
}

#[cfg(test)]
mod layout_band_tests {
    #[test]
    fn margin_padding_bands_match_runtime_at_preview_scales() {
        let style = regia_ui::Style {
            margin: regia_ui::style::Edges::all(regia_ui::Dimension::Points(3.0)),
            padding: regia_ui::style::Edges::all(regia_ui::Dimension::Points(5.0)),
            ..Default::default()
        };
        for scale in [0.5, 1.0, 1.5] {
            let rect = [20.0, 30.0, 100.0, 80.0];
            let bands = super::margin_padding_bands(rect, scale, &style);
            let model = regia_ui::debug_overlay::BoxModel::new(rect, scale, &style);
            let quads = regia_ui::debug_overlay::overlay_quads_for(&model);
            let expected: Vec<[f32; 4]> = quads
                .iter()
                .take(8)
                .map(|q| [q.pos[0], q.pos[1], q.size[0], q.size[1]])
                .collect();
            assert_eq!(bands.into_iter().flatten().collect::<Vec<_>>(), expected);
        }
    }
    #[test]
    fn anchors_keep_nested_bordered_rects_in_both_layout_directions() {
        use regia_ui::authoring::{UiFile, UiNodeDef};
        use regia_ui::style::{Dimension::Points, Edges, TextDir};
        for direction in [TextDir::Ltr, TextDir::Rtl] {
            let child = UiNodeDef {
                style: regia_ui::Style {
                    size: [Points(80.0), Points(40.0)],
                    margin: Edges {
                        left: Points(7.0),
                        top: Points(9.0),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            };
            let parent = UiNodeDef {
                style: regia_ui::Style {
                    size: [Points(400.0), Points(300.0)],
                    border: Edges {
                        left: Points(4.0),
                        top: Points(8.0),
                        right: Points(12.0),
                        bottom: Points(16.0),
                    },
                    padding: Edges::all(Points(10.0)),
                    ..Default::default()
                },
                children: vec![child],
                ..Default::default()
            };
            let file = UiFile {
                version: regia_ui::authoring::UI_RON_VERSION,
                component: None,
                variants: Vec::new(),
                root: UiNodeDef {
                    style: regia_ui::Style {
                        text_dir: direction,
                        ..Default::default()
                    },
                    children: vec![parent],
                    ..Default::default()
                },
                back: None,
            };
            let mut designer = crate::ui_designer::UiDesigner::default();
            designer
                .open_text(
                    &regia_ui::authoring::write_ui_ron(&file).expect("valid test fixture"),
                    None,
                )
                .expect("valid test fixture");
            let doc = designer.doc_mut().expect("valid test fixture");
            doc.preview.scale = Some(1.5);
            let before = crate::ui_designer_preview::preview(doc.file(), &doc.preview, None)
                .expect("valid test fixture");
            let original = super::rect_of(&before, &[0, 0]).expect("valid test fixture");
            for preset in regia_ui::anchors::AnchorPreset::ALL {
                let op = super::anchor_preset_op(&doc.file().root, &before, &[0, 0], preset, true)
                    .expect("valid test fixture");
                doc.apply(op).expect("valid test fixture");
                let after = crate::ui_designer_preview::preview(doc.file(), &doc.preview, None)
                    .expect("valid test fixture");
                let actual = super::rect_of(&after, &[0, 0]).expect("valid test fixture");
                for (a, b) in original.into_iter().zip(actual) {
                    assert!(
                        (a - b).abs() <= 1.0,
                        "{direction:?} {preset:?}: {original:?} -> {actual:?}"
                    );
                }
                doc.undo().expect("valid test fixture");
            }
        }
    }
}
