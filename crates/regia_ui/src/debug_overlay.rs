// SPDX-License-Identifier: MPL-2.0
//! Runtime UI layout debug overlay (ADR-273 U02, ADR-1842): outlines every laid-out node and,
//! for one highlighted node, tints its margin, padding and content boxes.
//!
//! Turn it on with the [`UiDebugOverlay`] resource (or `ui.debug_overlay`,
//! `book/agents/ui-debug-overlay.md`). The overlay is extra quads appended after the normal draw
//! list, so it needs no shader and costs nothing while disabled. The geometry
//! ([`BoxModel`], [`ring_bands`], [`overlay_quads_for`]) is pure and tested without a window.

use regia_app::prelude::*;

use crate::color::UiColor;
use crate::draw::UiQuad;
use crate::node::{ComputedNode, NodeStyle, UiNode};
use crate::style::{Dimension, Edges};

/// Draw depth of the overlay: above every normal root.
const OVERLAY_Z: f32 = 1.0e6;
/// Outline width in physical px.
const OUTLINE_PX: f32 = 1.0;

/// What the overlay draws.
#[derive(Resource, Debug, Clone, PartialEq, Default)]
pub struct UiDebugOverlay {
    /// Master switch.
    pub enabled: bool,
    /// Outline every node.
    pub outlines: bool,
    /// The node whose margin, padding and content boxes are tinted.
    pub highlight: Option<Entity>,
}

/// The focus-graph part of the overlay (ADR-273 U02, ADR-2045): outlines every focus
/// candidate of the active scope and, from the focused node, draws a connector to where each
/// arrow press would go. Drawn only while [`UiDebugOverlay::enabled`] is also on.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UiFocusGraphOverlay {
    /// Draw the focus graph.
    pub enabled: bool,
}

/// Where an arrow press in each direction goes from `from` (at `rect`) in `graph`, with no
/// remembered group children (the overlay shows the geometric graph). Directions with no move
/// are left out.
pub fn focus_edges(
    graph: &crate::focus_graph::NavGraph,
    from: Entity,
    rect: [f32; 4],
) -> Vec<(crate::focus::NavDirection, Entity)> {
    use crate::focus::NavDirection;
    let memory = crate::focus_graph::FocusMemory::default();
    [
        NavDirection::Up,
        NavDirection::Down,
        NavDirection::Left,
        NavDirection::Right,
    ]
    .into_iter()
    .filter_map(|d| graph.step(from, rect, d, &memory).target.map(|t| (d, t)))
    .filter(|(_, t)| *t != from)
    .collect()
}

/// The overlay color of a navigation direction.
pub fn direction_color(dir: crate::focus::NavDirection) -> UiColor {
    use crate::focus::NavDirection;
    match dir {
        NavDirection::Up => UiColor([0.3, 0.9, 0.4, 0.9]),
        NavDirection::Down => UiColor([1.0, 0.6, 0.2, 0.9]),
        NavDirection::Left => UiColor([0.9, 0.3, 0.9, 0.9]),
        _ => UiColor([0.3, 0.7, 1.0, 0.9]),
    }
}

/// An axis-aligned connector from the centre of `a` to the centre of `b`: a horizontal then a
/// vertical bar `width` px thick, plus a square marker on `b`'s centre.
pub fn connector_quads(a: [f32; 4], b: [f32; 4], width: f32, color: UiColor) -> Vec<UiQuad> {
    let ca = [a[0] + a[2] * 0.5, a[1] + a[3] * 0.5];
    let cb = [b[0] + b[2] * 0.5, b[1] + b[3] * 0.5];
    let h = width * 0.5;
    let bar = |x0: f32, y0: f32, x1: f32, y1: f32| UiQuad {
        z: OVERLAY_Z,
        ..UiQuad::fill(
            [x0.min(x1) - h, y0.min(y1) - h],
            [(x1 - x0).abs() + width, (y1 - y0).abs() + width],
            color,
        )
    };
    let m = width * 3.0;
    vec![
        bar(ca[0], ca[1], cb[0], ca[1]),
        bar(cb[0], ca[1], cb[0], cb[1]),
        UiQuad {
            z: OVERLAY_Z,
            ..UiQuad::fill([cb[0] - m, cb[1] - m], [2.0 * m, 2.0 * m], color)
        },
    ]
}

/// The four nested boxes of a node, each `[x, y, w, h]` in physical px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxModel {
    /// Margin box (outside the border box).
    pub margin: [f32; 4],
    /// Border box: the laid-out rect.
    pub border: [f32; 4],
    /// Padding box (inside the border).
    pub padding: [f32; 4],
    /// Content box (inside the padding).
    pub content: [f32; 4],
}

fn edge_px(d: Dimension, scale: f32) -> f32 {
    match d {
        Dimension::Points(p) if p.is_finite() => p * scale,
        _ => 0.0,
    }
}

fn edges_px(e: Edges, scale: f32) -> [f32; 4] {
    [
        edge_px(e.left, scale),
        edge_px(e.top, scale),
        edge_px(e.right, scale),
        edge_px(e.bottom, scale),
    ]
}

fn grow(rect: [f32; 4], e: [f32; 4]) -> [f32; 4] {
    [
        rect[0] - e[0],
        rect[1] - e[1],
        rect[2] + e[0] + e[2],
        rect[3] + e[1] + e[3],
    ]
}

fn shrink(rect: [f32; 4], e: [f32; 4]) -> [f32; 4] {
    [
        rect[0] + e[0],
        rect[1] + e[1],
        (rect[2] - e[0] - e[2]).max(0.0),
        (rect[3] - e[1] - e[3]).max(0.0),
    ]
}

impl BoxModel {
    /// The boxes of a node laid out at `rect` with `scale`, from its style's point-valued
    /// margin, border and padding (percent and auto count as 0).
    pub fn new(rect: [f32; 4], scale: f32, style: &crate::style::Style) -> Self {
        let margin = grow(rect, edges_px(style.margin, scale));
        let inner = shrink(rect, edges_px(style.border, scale));
        let content = shrink(inner, edges_px(style.padding, scale));
        Self {
            margin,
            border: rect,
            padding: inner,
            content,
        }
    }
}

/// The four rects of the band between `outer` and `inner` (top, bottom, left, right), without
/// overlap. `inner` is clamped into `outer`. Empty bands are dropped.
pub fn ring_bands(outer: [f32; 4], inner: [f32; 4]) -> Vec<[f32; 4]> {
    // `f32::clamp` aborts on NaN or reversed bounds: a negative margin can make the margin box
    // negative in size, and authored data may hold non-finite edges. Draw nothing for those.
    if !outer.iter().chain(inner.iter()).all(|v| v.is_finite()) || outer[2] < 0.0 || outer[3] < 0.0 {
        return Vec::new();
    }
    let (ox1, oy1) = (outer[0] + outer[2], outer[1] + outer[3]);
    let ix0 = inner[0].clamp(outer[0], ox1);
    let iy0 = inner[1].clamp(outer[1], oy1);
    let ix1 = (inner[0] + inner[2]).clamp(ix0, ox1);
    let iy1 = (inner[1] + inner[3]).clamp(iy0, oy1);
    [
        [outer[0], outer[1], outer[2], iy0 - outer[1]],
        [outer[0], iy1, outer[2], oy1 - iy1],
        [outer[0], iy0, ix0 - outer[0], iy1 - iy0],
        [ix1, iy0, ox1 - ix1, iy1 - iy0],
    ]
    .into_iter()
    .filter(|r| r[2] > 0.0 && r[3] > 0.0)
    .collect()
}

fn outline(rect: [f32; 4], color: UiColor) -> UiQuad {
    UiQuad {
        color: UiColor::TRANSPARENT,
        color2: color,
        border_width: [OUTLINE_PX; 4],
        z: OVERLAY_Z,
        ..UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], UiColor::TRANSPARENT)
    }
}

fn tint(rect: [f32; 4], color: UiColor) -> UiQuad {
    UiQuad {
        z: OVERLAY_Z,
        ..UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], color)
    }
}

/// Outline color of ordinary nodes.
const OUTLINE: UiColor = UiColor([0.1, 0.6, 1.0, 0.8]);
/// Margin tint.
const MARGIN: UiColor = UiColor([0.9, 0.55, 0.1, 0.35]);
/// Padding tint.
const PADDING: UiColor = UiColor([0.3, 0.8, 0.3, 0.35]);
/// Content tint.
const CONTENT: UiColor = UiColor([0.2, 0.4, 0.9, 0.35]);

/// The quads for one highlighted node: margin, padding and content tints plus its outline.
pub fn overlay_quads_for(model: &BoxModel) -> Vec<UiQuad> {
    let mut out = Vec::new();
    out.extend(
        ring_bands(model.margin, model.border)
            .into_iter()
            .map(|r| tint(r, MARGIN)),
    );
    out.extend(
        ring_bands(model.padding, model.content)
            .into_iter()
            .map(|r| tint(r, PADDING)),
    );
    if model.content[2] > 0.0 && model.content[3] > 0.0 {
        out.push(tint(model.content, CONTENT));
    }
    out.push(outline(model.border, UiColor([1.0, 1.0, 1.0, 0.9])));
    out
}

/// The overlay's quads for the current world; empty while disabled. Called by the extract pass.
pub fn collect_overlay(world: &mut World) -> Vec<UiQuad> {
    let Some(settings) = world.get_resource::<UiDebugOverlay>().cloned() else {
        return Vec::new();
    };
    if !settings.enabled {
        return Vec::new();
    }
    let _span = tracing::trace_span!("ui_debug_overlay").entered();
    let mut out = Vec::new();
    if settings.outlines {
        let mut q = world.query_filtered::<&ComputedNode, With<UiNode>>();
        for node in q.iter(world) {
            if node.rect[2] > 0.0 && node.rect[3] > 0.0 {
                out.push(outline(node.rect, OUTLINE));
            }
        }
    }
    if let Some(e) = settings.highlight
        && let (Some(node), Some(style)) = (world.get::<ComputedNode>(e), world.get::<NodeStyle>(e))
    {
        let scale = if node.scale > 0.0 { node.scale } else { 1.0 };
        out.extend(overlay_quads_for(&BoxModel::new(node.rect, scale, &style.0)));
    }
    if world
        .get_resource::<UiFocusGraphOverlay>()
        .is_some_and(|f| f.enabled)
    {
        let graph = crate::focus_graph::world_graph(world);
        for n in graph.nodes() {
            out.push(outline(n.rect, FOCUS_OUTLINE));
        }
        let focused = world
            .get_resource::<crate::focus::FocusState>()
            .and_then(|f| f.focused);
        if let Some(from) = focused
            && let Some(rect) = world.get::<ComputedNode>(from).map(|c| c.rect)
        {
            for (dir, to) in focus_edges(&graph, from, rect) {
                if let Some(target) = world.get::<ComputedNode>(to).map(|c| c.rect) {
                    out.extend(connector_quads(rect, target, 2.0, direction_color(dir)));
                }
            }
        }
    }
    out
}

/// Outline color of focus candidates in the focus-graph overlay.
const FOCUS_OUTLINE: UiColor = UiColor([0.3, 0.9, 1.0, 0.8]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Style;

    fn area(r: &[f32; 4]) -> f32 {
        r[2] * r[3]
    }

    #[test]
    fn ring_bands_survive_negative_and_non_finite_outer() {
        let inner = [10.0, 10.0, 50.0, 20.0];
        assert!(ring_bands([0.0, 0.0, -100.0, 40.0], inner).is_empty());
        assert!(ring_bands([0.0, 0.0, 100.0, -1.0], inner).is_empty());
        assert!(ring_bands([f32::NAN, 0.0, 100.0, 40.0], inner).is_empty());
        assert!(ring_bands([0.0, 0.0, 100.0, 40.0], [f32::INFINITY, 0.0, 1.0, 1.0]).is_empty());
        assert_eq!(ring_bands([0.0, 0.0, 100.0, 40.0], inner).len(), 4);
    }

    #[test]
    fn negative_margin_overlay_does_not_abort() {
        let style = Style {
            margin: Edges::all(Dimension::Points(-200.0)),
            ..Style::default()
        };
        let m = BoxModel::new([0.0, 0.0, 100.0, 40.0], 1.0, &style);
        let _ = overlay_quads_for(&m);
    }

    #[test]
    fn box_model_nests_and_scales() {
        let style = Style {
            margin: Edges::px(10.0),
            border: Edges::px(2.0),
            padding: Edges::px(5.0),
            ..Style::default()
        };
        let m = BoxModel::new([100.0, 100.0, 200.0, 100.0], 2.0, &style);
        assert_eq!(m.margin, [80.0, 80.0, 240.0, 140.0]);
        assert_eq!(m.border, [100.0, 100.0, 200.0, 100.0]);
        assert_eq!(m.padding, [104.0, 104.0, 192.0, 92.0]);
        assert_eq!(m.content, [114.0, 114.0, 172.0, 72.0]);
    }

    #[test]
    fn percent_auto_and_oversized_edges_are_safe() {
        let style = Style {
            padding: Edges::all(Dimension::Percent(50.0)),
            margin: Edges::all(Dimension::Auto),
            border: Edges::px(500.0),
            ..Style::default()
        };
        let m = BoxModel::new([0.0, 0.0, 20.0, 20.0], 1.0, &style);
        assert_eq!(m.margin, m.border);
        assert_eq!(m.content[2], 0.0);
        assert!(
            overlay_quads_for(&m)
                .iter()
                .all(|q| q.size[0] >= 0.0 && q.size[1] >= 0.0)
        );
    }

    #[test]
    fn bands_tile_the_ring_exactly() {
        let outer = [0.0, 0.0, 100.0, 80.0];
        let inner = [10.0, 20.0, 50.0, 30.0];
        let bands = ring_bands(outer, inner);
        let sum: f32 = bands.iter().map(area).sum();
        assert_eq!(sum, area(&outer) - area(&inner));
        assert_eq!(bands.len(), 4);
        assert!(ring_bands(outer, outer).is_empty(), "equal boxes leave no ring");
        let clamped = ring_bands(outer, [-50.0, -50.0, 500.0, 500.0]);
        assert!(clamped.is_empty());
    }

    #[test]
    fn disabled_overlay_draws_nothing_and_enabled_draws_outlines() {
        let mut world = World::new();
        assert!(collect_overlay(&mut world).is_empty(), "no resource");
        world.insert_resource(UiDebugOverlay::default());
        world.spawn((
            UiNode,
            ComputedNode {
                rect: [0.0, 0.0, 10.0, 10.0],
                ..ComputedNode::default()
            },
        ));
        assert!(collect_overlay(&mut world).is_empty(), "disabled");
        world.insert_resource(UiDebugOverlay {
            enabled: true,
            outlines: true,
            highlight: None,
        });
        let quads = collect_overlay(&mut world);
        assert_eq!(quads.len(), 1);
        assert_eq!(quads[0].size, [10.0, 10.0]);
    }

    #[test]
    fn highlight_tints_margin_padding_and_content() {
        let mut world = World::new();
        let e = world
            .spawn((
                UiNode,
                NodeStyle(Style {
                    margin: Edges::px(4.0),
                    padding: Edges::px(4.0),
                    ..Style::default()
                }),
                ComputedNode {
                    rect: [20.0, 20.0, 60.0, 40.0],
                    scale: 1.0,
                    ..ComputedNode::default()
                },
            ))
            .id();
        world.insert_resource(UiDebugOverlay {
            enabled: true,
            outlines: false,
            highlight: Some(e),
        });
        let quads = collect_overlay(&mut world);
        // 4 margin bands, 4 padding bands, content, outline.
        assert_eq!(quads.len(), 10);
    }

    #[test]
    fn focus_graph_overlay_links_the_focused_node_to_its_neighbors() {
        use crate::focus::{FocusState, Focusable, NavDirection};
        let mut world = World::new();
        let node = |world: &mut World, rect: [f32; 4]| {
            world
                .spawn((
                    UiNode,
                    Focusable::default(),
                    ComputedNode {
                        rect,
                        scale: 1.0,
                        ..ComputedNode::default()
                    },
                ))
                .id()
        };
        let a = node(&mut world, [0.0, 0.0, 40.0, 20.0]);
        let right = node(&mut world, [100.0, 0.0, 40.0, 20.0]);
        let below = node(&mut world, [0.0, 100.0, 40.0, 20.0]);
        world.insert_resource(FocusState {
            focused: Some(a),
            ..FocusState::default()
        });
        world.insert_resource(UiDebugOverlay {
            enabled: true,
            outlines: false,
            highlight: None,
        });
        assert!(
            collect_overlay(&mut world).is_empty(),
            "focus graph off by default"
        );
        world.insert_resource(UiFocusGraphOverlay { enabled: true });
        let graph = crate::focus_graph::world_graph(&mut world);
        let edges = focus_edges(&graph, a, [0.0, 0.0, 40.0, 20.0]);
        assert!(edges.contains(&(NavDirection::Right, right)), "{edges:?}");
        assert!(edges.contains(&(NavDirection::Down, below)), "{edges:?}");
        assert!(!edges.iter().any(|(d, _)| *d == NavDirection::Up));
        let quads = collect_overlay(&mut world);
        // Three candidate outlines plus three quads per edge.
        assert_eq!(quads.len(), 3 + 3 * edges.len());
        // The connector to the right runs along the centre line y = 10.
        let c = connector_quads(
            [0.0, 0.0, 40.0, 20.0],
            [100.0, 0.0, 40.0, 20.0],
            2.0,
            UiColor::WHITE,
        );
        assert_eq!(c[0].pos, [19.0, 9.0]);
        assert_eq!(c[0].size, [102.0, 2.0]);
    }
}
