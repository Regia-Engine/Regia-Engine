// SPDX-License-Identifier: MPL-2.0
//! The directional focus graph for keyboard and gamepad navigation (UI lane 9, ADR-516).
//!
//! [`crate::focus`] owns the input side (keys, the gamepad, repeat) and the geometric score
//! ([`crate::focus::nav_score`]). This module decides where a move goes:
//!
//! - **Explicit links** ([`NavLinks`]) win when their target is navigable now (a link to a
//!   disabled, hidden or out-of-scope node falls through to geometry).
//! - **Focus groups** ([`FocusGroup`]) on any ancestor container keep directional moves inside
//!   the group while a candidate exists there. At the group's edge each direction follows an
//!   [`EdgeRule`]: `Escape` (continue in the enclosing group, the default), `Stop` (no move) or
//!   `Wrap` (jump to the far side of the group). A group can cycle Tab inside itself
//!   ([`TabRule::Cycle`]) and can remember its last focused child, so coming back into the group
//!   lands where the user left it.
//! - **Tab order** ([`TabIndex`]): positive indices first, ascending, then every other node in
//!   reading order; a negative index leaves the node out of Tab (directional moves still reach it).
//! - **Focus repair**: when the focused node is despawned, or hidden after it was visible, while
//!   the user navigates with keys or a gamepad, focus moves to the nearest navigable node so the
//!   focus ring never sits on nothing ([`repair_stranded_focus`]). A navigation press with stale
//!   focus (despawned, hidden, outside the active scope) recovers the same way instead of doing
//!   nothing.
//!
//! The graph is rebuilt on demand (one navigation press, one repair, one agent call); nothing here
//! runs over every node every frame. Agents read it through `ui.nav_graph`, drive it through
//! `ui.nav_move` and check it with `ui.nav_lint` (`book/agents/ui-navigation.md`).

use std::collections::{HashMap, HashSet, VecDeque};

use regia_app::prelude::*;
use regia_ecs::ecs::system::SystemParam;

use crate::focus::{
    DefaultFocus, FocusState, Focusable, NavDirection, NavLinks, UiInputMode, in_scope, nav_score, set_focus,
};
use crate::interaction::{UiEvent, UiEventKind};
use crate::node::{ComputedNode, Disabled, Focused};

/// What a directional move does when no candidate is left inside a [`FocusGroup`] in that
/// direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EdgeRule {
    /// Continue the search in the enclosing group (or the whole scope). The default, and the
    /// behavior of a UI without groups.
    #[default]
    Escape,
    /// Do not move: focus stays on the edge node.
    Stop,
    /// Jump to the node on the far side of the group (the first item of the row or column).
    Wrap,
}

impl EdgeRule {
    /// The agent-protocol name: `escape`, `stop` or `wrap`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Escape => "escape",
            Self::Stop => "stop",
            Self::Wrap => "wrap",
        }
    }
}

/// How Tab and the shoulder buttons treat a [`FocusGroup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabRule {
    /// Tab walks the whole scope's order (the default).
    #[default]
    Pass,
    /// Tab cycles through the group's members only while focus is inside it.
    Cycle,
}

impl TabRule {
    /// The agent-protocol name: `pass` or `cycle`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Cycle => "cycle",
        }
    }
}

/// Navigation rules for a container: its focusable descendants form a group. Groups nest; the
/// innermost group around the focused node decides first.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FocusGroup {
    /// Rule at the group's top edge.
    pub up: EdgeRule,
    /// Rule at the group's bottom edge.
    pub down: EdgeRule,
    /// Rule at the group's left edge.
    pub left: EdgeRule,
    /// Rule at the group's right edge.
    pub right: EdgeRule,
    /// Tab behavior inside the group.
    pub tab: TabRule,
    /// Coming back into the group from outside focuses the child that had focus last.
    pub remember: bool,
}

impl FocusGroup {
    /// A group that prefers its own members but lets every move escape at its edges.
    pub const fn new() -> Self {
        Self {
            up: EdgeRule::Escape,
            down: EdgeRule::Escape,
            left: EdgeRule::Escape,
            right: EdgeRule::Escape,
            tab: TabRule::Pass,
            remember: false,
        }
    }

    /// A trap: no directional move leaves the group and Tab cycles inside it (a pause menu or a
    /// dialog that is not an input layer).
    pub const fn trap() -> Self {
        Self {
            up: EdgeRule::Stop,
            down: EdgeRule::Stop,
            left: EdgeRule::Stop,
            right: EdgeRule::Stop,
            tab: TabRule::Cycle,
            remember: false,
        }
    }

    /// Wraps in every direction and cycles Tab (a carousel or a closed grid).
    pub const fn wrap() -> Self {
        Self {
            up: EdgeRule::Wrap,
            down: EdgeRule::Wrap,
            left: EdgeRule::Wrap,
            right: EdgeRule::Wrap,
            tab: TabRule::Cycle,
            remember: false,
        }
    }

    /// Wraps left and right; up and down escape (a horizontal tab strip or a row of cards).
    pub const fn wrap_horizontal() -> Self {
        Self {
            left: EdgeRule::Wrap,
            right: EdgeRule::Wrap,
            ..Self::new()
        }
    }

    /// Wraps up and down; left and right escape (a vertical menu).
    pub const fn wrap_vertical() -> Self {
        Self {
            up: EdgeRule::Wrap,
            down: EdgeRule::Wrap,
            ..Self::new()
        }
    }

    /// The same group, remembering its last focused child.
    pub const fn remembering(mut self) -> Self {
        self.remember = true;
        self
    }

    /// The same group with `rule` at the edge `dir` points to (`Next`/`Prev` are ignored).
    pub const fn with_edge(mut self, dir: NavDirection, rule: EdgeRule) -> Self {
        match dir {
            NavDirection::Up => self.up = rule,
            NavDirection::Down => self.down = rule,
            NavDirection::Left => self.left = rule,
            NavDirection::Right => self.right = rule,
            NavDirection::Next | NavDirection::Prev => {}
        }
        self
    }

    /// The rule at the edge `dir` points to (`Escape` for `Next`/`Prev`).
    pub fn edge(&self, dir: NavDirection) -> EdgeRule {
        match dir {
            NavDirection::Up => self.up,
            NavDirection::Down => self.down,
            NavDirection::Left => self.left,
            NavDirection::Right => self.right,
            NavDirection::Next | NavDirection::Prev => EdgeRule::Escape,
        }
    }
}

/// Explicit Tab position. Positive values come first in ascending order, then nodes without an
/// index (or index 0) in reading order. A negative value removes the node from Tab and the
/// shoulder buttons; directional moves still reach it.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct TabIndex(pub i32);

/// Why a navigation step went where it did (or nowhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavReason {
    /// Nothing had focus: the first press only reveals focus on the default node.
    Initial,
    /// The focused node was gone, hidden or outside the active scope: focus moved to the nearest
    /// navigable node (or the default).
    Recovered,
    /// An explicit [`NavLinks`] target.
    Explicit,
    /// The best geometric candidate in the direction.
    Geometry,
    /// A [`EdgeRule::Wrap`] jump to the far side of a group.
    Wrap,
    /// Re-entering a remembering [`FocusGroup`] at its last focused child.
    Remembered,
    /// The next or previous node in Tab order.
    Tab,
    /// A [`EdgeRule::Stop`] edge: no move.
    Stop,
    /// No candidate in the direction and nothing to wrap to: no move.
    Edge,
}

impl NavReason {
    /// The agent-protocol name (lowercase).
    pub fn name(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Recovered => "recovered",
            Self::Explicit => "explicit",
            Self::Geometry => "geometry",
            Self::Wrap => "wrap",
            Self::Remembered => "remembered",
            Self::Tab => "tab",
            Self::Stop => "stop",
            Self::Edge => "edge",
        }
    }
}

/// The outcome of one navigation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavStep {
    /// The node to focus, `None` for no move.
    pub target: Option<Entity>,
    /// Why.
    pub reason: NavReason,
}

impl NavStep {
    const fn to(target: Entity, reason: NavReason) -> Self {
        Self {
            target: Some(target),
            reason,
        }
    }

    const fn none(reason: NavReason) -> Self {
        Self { target: None, reason }
    }
}

/// The agent-protocol name of a direction: `up`, `down`, `left`, `right`, `next` or `prev`.
pub fn direction_name(dir: NavDirection) -> &'static str {
    match dir {
        NavDirection::Up => "up",
        NavDirection::Down => "down",
        NavDirection::Left => "left",
        NavDirection::Right => "right",
        NavDirection::Next => "next",
        NavDirection::Prev => "prev",
    }
}

/// Parses [`direction_name`] (ASCII case-insensitive; `previous` is accepted for `prev`).
pub fn parse_direction(name: &str) -> Option<NavDirection> {
    match name.to_ascii_lowercase().as_str() {
        "up" => Some(NavDirection::Up),
        "down" => Some(NavDirection::Down),
        "left" => Some(NavDirection::Left),
        "right" => Some(NavDirection::Right),
        "next" => Some(NavDirection::Next),
        "prev" | "previous" => Some(NavDirection::Prev),
        _ => None,
    }
}

/// The opposite direction (`Next` and `Prev` swap).
pub fn opposite(dir: NavDirection) -> NavDirection {
    match dir {
        NavDirection::Up => NavDirection::Down,
        NavDirection::Down => NavDirection::Up,
        NavDirection::Left => NavDirection::Right,
        NavDirection::Right => NavDirection::Left,
        NavDirection::Next => NavDirection::Prev,
        NavDirection::Prev => NavDirection::Next,
    }
}

/// The four directional moves plus Tab forward and back, in that order.
pub const ALL_DIRECTIONS: [NavDirection; 6] = [
    NavDirection::Up,
    NavDirection::Down,
    NavDirection::Left,
    NavDirection::Right,
    NavDirection::Next,
    NavDirection::Prev,
];

const DIRECTIONAL: [NavDirection; 4] = [
    NavDirection::Up,
    NavDirection::Down,
    NavDirection::Left,
    NavDirection::Right,
];

fn link(links: &NavLinks, dir: NavDirection) -> Option<Entity> {
    match dir {
        NavDirection::Up => links.up,
        NavDirection::Down => links.down,
        NavDirection::Left => links.left,
        NavDirection::Right => links.right,
        NavDirection::Next | NavDirection::Prev => None,
    }
}

fn has_area(rect: [f32; 4]) -> bool {
    rect[2] > 0.0 && rect[3] > 0.0
}

fn union(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let x0 = a[0].min(b[0]);
    let y0 = a[1].min(b[1]);
    let x1 = (a[0] + a[2]).max(b[0] + b[2]);
    let y1 = (a[1] + a[3]).max(b[1] + b[3]);
    [x0, y0, x1 - x0, y1 - y0]
}

fn center(r: [f32; 4]) -> [f32; 2] {
    [r[0] + r[2] / 2.0, r[1] + r[3] / 2.0]
}

/// What the graph remembers between frames: the last focused node with a visible rect (focus
/// repair starts from it) and each remembering group's last focused child.
#[derive(Resource, Debug, Default, Clone)]
pub struct FocusMemory {
    last: Option<(Entity, [f32; 4])>,
    groups: HashMap<Entity, Entity>,
}

impl FocusMemory {
    /// The last focused node that had a visible rect, and that rect.
    pub fn last(&self) -> Option<(Entity, [f32; 4])> {
        self.last
    }

    /// The last focused child of the remembering group `group`.
    pub fn remembered(&self, group: Entity) -> Option<Entity> {
        self.groups.get(&group).copied()
    }

    /// Records `focused` at `rect` as the last visible focus.
    pub fn record(&mut self, focused: Entity, rect: [f32; 4]) {
        self.last = Some((focused, rect));
    }

    /// Records `child` as the last focused member of `group`.
    pub fn remember(&mut self, group: Entity, child: Entity) {
        self.groups.insert(group, child);
    }

    /// Forgets everything (a screen change that must not restore old positions).
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// One focusable node handed to [`NavGraph::new`].
#[derive(Debug, Clone, PartialEq)]
pub struct NavInput {
    /// The node.
    pub entity: Entity,
    /// Its computed rect (x, y, w, h).
    pub rect: [f32; 4],
    /// Its explicit links.
    pub links: NavLinks,
    /// Its [`TabIndex`], if any.
    pub tab_index: Option<i32>,
    /// The groups around it, innermost first (the node itself first when it carries a
    /// [`FocusGroup`]): group entity, rules and the group node's rect when it has one.
    pub groups: Vec<(Entity, FocusGroup, Option<[f32; 4]>)>,
}

/// A navigable node in a [`NavGraph`].
#[derive(Debug, Clone, PartialEq)]
pub struct NavNode {
    /// The node.
    pub entity: Entity,
    /// Its rect.
    pub rect: [f32; 4],
    /// Its explicit links.
    pub links: NavLinks,
    /// Its Tab index.
    pub tab_index: Option<i32>,
    /// Indices into [`NavGraph::groups`], innermost first.
    pub groups: Vec<usize>,
}

/// A focus group in a [`NavGraph`].
#[derive(Debug, Clone, PartialEq)]
pub struct NavGroup {
    /// The container carrying the [`FocusGroup`].
    pub entity: Entity,
    /// Its rules.
    pub rules: FocusGroup,
    /// The container's rect, if it has one.
    pub rect: Option<[f32; 4]>,
    /// Indices into [`NavGraph::nodes`], in reading order.
    pub members: Vec<usize>,
}

/// One problem `ui.nav_lint` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavIssueKind {
    /// No sequence of moves from the start node reaches this node.
    Unreachable,
    /// No move leaves this node in any direction, and the graph has other nodes.
    DeadEnd,
    /// An explicit link points to a node that is not navigable now (gone, disabled, hidden or
    /// outside the scope).
    DeadLink,
    /// An explicit link whose target does not lead back by the opposite move.
    OneWayLink,
    /// Two nodes share the same rect, so geometry cannot tell them apart.
    DuplicateRect,
}

impl NavIssueKind {
    /// The agent-protocol code (lowercase).
    pub fn code(self) -> &'static str {
        match self {
            Self::Unreachable => "unreachable",
            Self::DeadEnd => "dead_end",
            Self::DeadLink => "dead_link",
            Self::OneWayLink => "one_way_link",
            Self::DuplicateRect => "duplicate_rect",
        }
    }
}

/// A [`NavGraph::lint`] finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavIssue {
    /// What is wrong.
    pub kind: NavIssueKind,
    /// The node it concerns.
    pub node: Entity,
    /// The other node (link target, or the node sharing the rect).
    pub other: Option<Entity>,
    /// The direction of the link, for link issues.
    pub direction: Option<NavDirection>,
}

/// Where the focus is when a navigation press arrives, as [`NavGraph::resolve`] needs it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FocusOrigin {
    /// Nothing is focused.
    Unfocused,
    /// The focused node is in the graph.
    Candidate(Entity),
    /// The focused node is visible and inside the scope but not navigable (disabled): moves start
    /// from its rect.
    Inert(Entity, [f32; 4]),
    /// The focused node is gone or hidden: recover near where it was.
    Lost,
    /// The focused node is outside the active scope (a dialog opened over it): recover at the
    /// scope's default node.
    OutOfScope,
}

/// The navigable nodes of one scope, in reading order, with their groups.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NavGraph {
    nodes: Vec<NavNode>,
    groups: Vec<NavGroup>,
    index: HashMap<Entity, usize>,
}

impl NavGraph {
    /// Builds a graph. Nodes without area are dropped; the rest are sorted in reading order (top
    /// to bottom, then left to right, then entity id), so every tie breaks the same way on every
    /// run.
    pub fn new(mut inputs: Vec<NavInput>) -> Self {
        inputs.retain(|i| has_area(i.rect));
        inputs.sort_by(|a, b| {
            (a.rect[1], a.rect[0])
                .partial_cmp(&(b.rect[1], b.rect[0]))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.entity.to_bits().cmp(&b.entity.to_bits()))
        });
        let mut graph = Self::default();
        let mut group_index: HashMap<Entity, usize> = HashMap::new();
        for (i, input) in inputs.into_iter().enumerate() {
            let mut chain = Vec::with_capacity(input.groups.len());
            for (entity, rules, rect) in input.groups {
                let g = *group_index.entry(entity).or_insert_with(|| {
                    graph.groups.push(NavGroup {
                        entity,
                        rules,
                        rect: rect.filter(|r| has_area(*r)),
                        members: Vec::new(),
                    });
                    graph.groups.len() - 1
                });
                graph.groups[g].members.push(i);
                chain.push(g);
            }
            graph.index.insert(input.entity, i);
            graph.nodes.push(NavNode {
                entity: input.entity,
                rect: input.rect,
                links: input.links,
                tab_index: input.tab_index,
                groups: chain,
            });
        }
        graph
    }

    /// The nodes in reading order.
    pub fn nodes(&self) -> &[NavNode] {
        &self.nodes
    }

    /// The groups, in order of first appearance.
    pub fn groups(&self) -> &[NavGroup] {
        &self.groups
    }

    /// Whether `entity` is navigable in this graph.
    pub fn contains(&self, entity: Entity) -> bool {
        self.index.contains_key(&entity)
    }

    /// The node record of `entity`.
    pub fn node(&self, entity: Entity) -> Option<&NavNode> {
        self.index.get(&entity).map(|&i| &self.nodes[i])
    }

    /// Whether the graph has no navigable node.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The node a first press reveals: the first of `defaults` that is navigable, else the first
    /// node in Tab order, else the first node in reading order.
    pub fn initial(&self, defaults: &[Entity]) -> Option<Entity> {
        defaults
            .iter()
            .copied()
            .find(|d| self.contains(*d))
            .or_else(|| self.tab_order(None).first().map(|&i| self.nodes[i].entity))
            .or_else(|| self.nodes.first().map(|n| n.entity))
    }

    /// The node whose center is closest to the center of `rect` (ties: reading order).
    pub fn nearest(&self, rect: [f32; 4]) -> Option<Entity> {
        let c = center(rect);
        self.nodes
            .iter()
            .map(|n| {
                let m = center(n.rect);
                ((m[0] - c[0]).powi(2) + (m[1] - c[1]).powi(2), n.entity)
            })
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, e)| e)
    }

    /// Tab order of the whole graph, or of group `group`'s members: indices into
    /// [`Self::nodes`].
    pub fn tab_order(&self, group: Option<usize>) -> Vec<usize> {
        let mut order: Vec<usize> = match group.and_then(|g| self.groups.get(g)) {
            Some(g) => g.members.clone(),
            None => (0..self.nodes.len()).collect(),
        };
        order.retain(|&i| self.nodes[i].tab_index.is_none_or(|t| t >= 0));
        // Stable sort: reading order (the node order) breaks ties inside each class.
        order.sort_by_key(|&i| match self.nodes[i].tab_index {
            Some(t) if t > 0 => (0, t),
            _ => (1, 0),
        });
        order
    }

    /// Where a navigation press in `dir` goes from `origin`. `defaults` are the
    /// [`DefaultFocus`] nodes; `memory` supplies remembered group children and the last visible
    /// focus.
    pub fn resolve(
        &self,
        origin: FocusOrigin,
        dir: NavDirection,
        defaults: &[Entity],
        memory: &FocusMemory,
    ) -> NavStep {
        match origin {
            FocusOrigin::Unfocused => match self.initial(defaults) {
                Some(e) => NavStep::to(e, NavReason::Initial),
                None => NavStep::none(NavReason::Edge),
            },
            FocusOrigin::Candidate(e) => match self.node(e) {
                Some(n) => self.step(e, n.rect, dir, memory),
                None => self.recover(false, defaults, memory),
            },
            FocusOrigin::Inert(e, rect) => self.step(e, rect, dir, memory),
            FocusOrigin::Lost => self.recover(false, defaults, memory),
            FocusOrigin::OutOfScope => self.recover(true, defaults, memory),
        }
    }

    /// Recovery target: near the last visible focus (or the default first when
    /// `prefer_default`).
    pub fn recover(&self, prefer_default: bool, defaults: &[Entity], memory: &FocusMemory) -> NavStep {
        let near = || memory.last().and_then(|(_, r)| self.nearest(r));
        let target = if prefer_default {
            defaults
                .iter()
                .copied()
                .find(|d| self.contains(*d))
                .or_else(near)
                .or_else(|| self.initial(defaults))
        } else {
            near().or_else(|| self.initial(defaults))
        };
        match target {
            Some(t) => NavStep::to(t, NavReason::Recovered),
            None => NavStep::none(NavReason::Edge),
        }
    }

    /// One step in `dir` from `from` (at `from_rect`; `from` need not be in the graph).
    pub fn step(
        &self,
        from: Entity,
        from_rect: [f32; 4],
        dir: NavDirection,
        memory: &FocusMemory,
    ) -> NavStep {
        let me = self.index.get(&from).copied();
        let chain: &[usize] = match me {
            Some(i) => self.nodes[i].groups.as_slice(),
            None => &[],
        };
        if matches!(dir, NavDirection::Next | NavDirection::Prev) {
            return self.tab_step(me, chain, dir);
        }
        if let Some(i) = me
            && let Some(target) = link(&self.nodes[i].links, dir)
        {
            if target == from {
                // A link to itself blocks the direction (a slider that steps its own value).
                return NavStep::none(NavReason::Stop);
            }
            if self.contains(target) {
                return NavStep::to(target, NavReason::Explicit);
            }
        }
        let mut level = 0;
        loop {
            let best = match chain.get(level) {
                Some(&g) => self.best_in(self.groups[g].members.iter().copied(), from, from_rect, dir),
                None => self.best_in(0..self.nodes.len(), from, from_rect, dir),
            };
            if let Some(b) = best {
                return self.entering(chain, b, memory, NavReason::Geometry);
            }
            let Some(&g) = chain.get(level) else {
                return NavStep::none(NavReason::Edge);
            };
            match self.groups[g].rules.edge(dir) {
                EdgeRule::Stop => return NavStep::none(NavReason::Stop),
                EdgeRule::Wrap => {
                    return match self.wrap_in(g, from, from_rect, dir) {
                        Some(t) => self.entering(chain, t, memory, NavReason::Wrap),
                        None => NavStep::none(NavReason::Edge),
                    };
                }
                EdgeRule::Escape => level += 1,
            }
        }
    }

    fn best_in(
        &self,
        pool: impl Iterator<Item = usize>,
        from: Entity,
        from_rect: [f32; 4],
        dir: NavDirection,
    ) -> Option<usize> {
        pool.filter(|&i| self.nodes[i].entity != from)
            .filter_map(|i| nav_score(from_rect, self.nodes[i].rect, dir).map(|s| (s, i)))
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, i)| i)
    }

    /// The wrap target inside group `g`: the best candidate seen from a copy of `from_rect`
    /// placed just outside the group's opposite edge.
    fn wrap_in(&self, g: usize, from: Entity, from_rect: [f32; 4], dir: NavDirection) -> Option<usize> {
        let group = &self.groups[g];
        let bounds = group
            .members
            .iter()
            .map(|&i| self.nodes[i].rect)
            .chain(group.rect)
            .reduce(union)?;
        let [fx, fy, fw, fh] = from_rect;
        let ghost = match dir {
            NavDirection::Down => [fx, bounds[1] - fh, fw, fh],
            NavDirection::Up => [fx, bounds[1] + bounds[3], fw, fh],
            NavDirection::Right => [bounds[0] - fw, fy, fw, fh],
            NavDirection::Left => [bounds[0] + bounds[2], fy, fw, fh],
            NavDirection::Next | NavDirection::Prev => return None,
        };
        let best = group
            .members
            .iter()
            .filter_map(|&i| nav_score(ghost, self.nodes[i].rect, dir).map(|s| (s, i)))
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, i)| i)?;
        (self.nodes[best].entity != from).then_some(best)
    }

    /// Redirects a move into a remembering group (one `from` is not in) to that group's last
    /// focused child.
    fn entering(
        &self,
        from_chain: &[usize],
        target: usize,
        memory: &FocusMemory,
        reason: NavReason,
    ) -> NavStep {
        let node = &self.nodes[target];
        for &g in node.groups.iter().rev() {
            if from_chain.contains(&g) || !self.groups[g].rules.remember {
                continue;
            }
            if let Some(m) = memory.remembered(self.groups[g].entity)
                && m != node.entity
                && self.node(m).is_some_and(|n| n.groups.contains(&g))
            {
                return NavStep::to(m, NavReason::Remembered);
            }
        }
        NavStep::to(node.entity, reason)
    }

    fn tab_step(&self, me: Option<usize>, chain: &[usize], dir: NavDirection) -> NavStep {
        let cycle = chain
            .iter()
            .copied()
            .find(|&g| self.groups[g].rules.tab == TabRule::Cycle);
        let order = self.tab_order(cycle);
        let n = order.len();
        let forward = dir == NavDirection::Next;
        let pick = match me.and_then(|i| order.iter().position(|&o| o == i)) {
            Some(_) if n < 2 => None,
            Some(p) => Some(order[if forward { (p + 1) % n } else { (p + n - 1) % n }]),
            None if forward => order.first().copied(),
            None => order.last().copied(),
        };
        match pick {
            Some(i) if Some(i) != me => NavStep::to(self.nodes[i].entity, NavReason::Tab),
            _ => NavStep::none(NavReason::Edge),
        }
    }

    /// Structural problems, without remembered positions: unreachable nodes (from `start`, else
    /// the initial node), dead ends, dead and one-way explicit links, duplicate rects.
    pub fn lint(&self, start: Option<Entity>, defaults: &[Entity]) -> Vec<NavIssue> {
        let memory = FocusMemory::default();
        let mut issues = Vec::new();
        let step = |from: &NavNode, dir| self.step(from.entity, from.rect, dir, &memory).target;
        let mut seen = HashSet::new();
        if let Some(start) = start
            .filter(|s| self.contains(*s))
            .or_else(|| self.initial(defaults))
        {
            let mut queue = VecDeque::from([start]);
            seen.insert(start);
            while let Some(e) = queue.pop_front() {
                let Some(node) = self.node(e) else { continue };
                for dir in ALL_DIRECTIONS {
                    if let Some(t) = step(node, dir)
                        && seen.insert(t)
                    {
                        queue.push_back(t);
                    }
                }
            }
        }
        for node in &self.nodes {
            if !seen.contains(&node.entity) {
                issues.push(NavIssue {
                    kind: NavIssueKind::Unreachable,
                    node: node.entity,
                    other: None,
                    direction: None,
                });
            }
            if self.nodes.len() > 1 && ALL_DIRECTIONS.iter().all(|&d| step(node, d).is_none()) {
                issues.push(NavIssue {
                    kind: NavIssueKind::DeadEnd,
                    node: node.entity,
                    other: None,
                    direction: None,
                });
            }
            for dir in DIRECTIONAL {
                let Some(target) = link(&node.links, dir) else {
                    continue;
                };
                if target == node.entity {
                    continue;
                }
                match self.node(target) {
                    None => issues.push(NavIssue {
                        kind: NavIssueKind::DeadLink,
                        node: node.entity,
                        other: Some(target),
                        direction: Some(dir),
                    }),
                    Some(t) if step(t, opposite(dir)) != Some(node.entity) => issues.push(NavIssue {
                        kind: NavIssueKind::OneWayLink,
                        node: node.entity,
                        other: Some(target),
                        direction: Some(dir),
                    }),
                    Some(_) => {}
                }
            }
        }
        for (i, a) in self.nodes.iter().enumerate() {
            if let Some(b) = self.nodes[i + 1..].iter().find(|b| b.rect == a.rect) {
                issues.push(NavIssue {
                    kind: NavIssueKind::DuplicateRect,
                    node: a.entity,
                    other: Some(b.entity),
                    direction: None,
                });
            }
        }
        issues
    }
}

/// Builds the [`NavInput`]s of one scope: each focusable with area whose ancestor chain reaches
/// `scope` (every focusable when `scope` is `None`), with the [`FocusGroup`]s on that chain,
/// innermost first, up to and including the scope root.
pub fn collect_inputs(
    focusables: impl Iterator<Item = (Entity, [f32; 4], NavLinks, Option<i32>)>,
    parent_of: impl Fn(Entity) -> Option<Entity>,
    group_of: impl Fn(Entity) -> Option<(FocusGroup, Option<[f32; 4]>)>,
    scope: Option<Entity>,
) -> Vec<NavInput> {
    let mut out = Vec::new();
    for (entity, rect, links, tab_index) in focusables {
        if !has_area(rect) {
            continue;
        }
        let mut groups = Vec::new();
        let mut current = Some(entity);
        let mut inside = scope.is_none();
        // Depth guard against a malformed (cyclic) hierarchy.
        for _ in 0..1024 {
            let Some(e) = current else { break };
            if let Some((rules, group_rect)) = group_of(e) {
                groups.push((e, rules, group_rect));
            }
            if scope == Some(e) {
                inside = true;
                break;
            }
            current = parent_of(e);
        }
        if !inside {
            continue;
        }
        out.push(NavInput {
            entity,
            rect,
            links,
            tab_index,
            groups,
        });
    }
    out
}

/// The focusable nodes navigation can move to (the query [`crate::focus::ui_focus_navigation`]
/// already holds).
pub type FocusableQuery<'w, 's> =
    Query<'w, 's, (Entity, &'static ComputedNode, &'static Focusable), Without<Disabled>>;

/// The rest of the ECS view the focus graph is built from: groups, Tab indices, the hierarchy
/// and [`FocusMemory`].
#[derive(SystemParam)]
pub struct NavWorld<'w, 's> {
    groups: Query<'w, 's, (&'static FocusGroup, Option<&'static ComputedNode>)>,
    tab_indices: Query<'w, 's, &'static TabIndex>,
    parents: Query<'w, 's, &'static ChildOf>,
    nodes: Query<'w, 's, &'static ComputedNode>,
    entities: Query<'w, 's, Entity>,
    defaults: Query<'w, 's, Entity, With<DefaultFocus>>,
    memory: Option<Res<'w, FocusMemory>>,
    disabled_subtrees: Query<'w, 's, (), With<crate::node::DisabledSubtree>>,
}

impl NavWorld<'_, '_> {
    pub(crate) fn subtree_disabled(&self, entity: Entity) -> bool {
        crate::node::subtree_disabled(entity, &self.parents, &self.disabled_subtrees)
    }

    /// The graph of `focusables` inside `scope` (all of them when `None`).
    pub fn graph(&self, focusables: &FocusableQuery, scope: Option<Entity>) -> NavGraph {
        let _span = tracing::trace_span!("ui_nav_graph").entered();
        NavGraph::new(collect_inputs(
            focusables
                .iter()
                .filter(|(entity, ..)| !self.subtree_disabled(*entity))
                .map(|(e, c, f)| (e, c.rect, f.nav, self.tab_indices.get(e).ok().map(|t| t.0))),
            |e| self.parents.get(e).ok().map(|p| p.0),
            |e| self.groups.get(e).ok().map(|(g, c)| (*g, c.map(|c| c.rect))),
            scope,
        ))
    }

    /// The [`DefaultFocus`] nodes.
    pub fn defaults(&self) -> Vec<Entity> {
        self.defaults.iter().collect()
    }

    /// Where `focused` stands relative to `graph`.
    pub fn origin(&self, graph: &NavGraph, focused: Option<Entity>, scope: Option<Entity>) -> FocusOrigin {
        let Some(f) = focused else {
            return FocusOrigin::Unfocused;
        };
        if graph.contains(f) {
            return FocusOrigin::Candidate(f);
        }
        if !self.entities.contains(f) {
            return FocusOrigin::Lost;
        }
        if scope.is_some_and(|s| !in_scope(&self.parents, s, f)) {
            return FocusOrigin::OutOfScope;
        }
        match self.nodes.get(f) {
            Ok(c) if has_area(c.rect) => FocusOrigin::Inert(f, c.rect),
            _ => FocusOrigin::Lost,
        }
    }

    fn memory(&self) -> FocusMemory {
        self.memory.as_deref().cloned().unwrap_or_default()
    }
}

/// One navigation press in `dir` through the focus graph, applied to `state` (the
/// [`crate::focus::ui_focus_navigation`] path). `defaults` are the [`DefaultFocus`] nodes.
/// Returns the step taken.
pub fn navigate_graph(
    dir: NavDirection,
    focusables: &FocusableQuery,
    defaults: &Query<Entity, With<DefaultFocus>>,
    nav: &NavWorld,
    state: &mut FocusState,
    commands: &mut Commands,
    events: &mut MessageWriter<UiEvent>,
) -> NavStep {
    let graph = nav.graph(focusables, state.scope);
    let origin = nav.origin(&graph, state.focused, state.scope);
    let defaults: Vec<Entity> = defaults.iter().collect();
    let step = graph.resolve(origin, dir, &defaults, &nav.memory());
    if let Some(target) = step.target
        && Some(target) != state.focused
    {
        if state.focused.is_some_and(|f| !nav.entities.contains(f)) {
            // The old node is gone: nothing to un-focus, no event for a dead entity.
            state.focused = None;
        }
        set_focus(Some(target), state, commands, events);
    }
    step
}

/// `Update` (after [`crate::focus::ui_focus_navigation`]): keeps the focus ring on a live node.
/// While the user navigates with keys or a gamepad ([`UiInputMode::Navigation`]), a focused node
/// that was despawned, disabled, or hidden after it had been visible, hands focus to the nearest
/// navigable node in the active scope (else the default). In pointer mode a despawned focus is
/// cleared instead; a hidden one is kept (the ring does not show there) and recovers on the next
/// press. A scope left empty this way is refilled by [`crate::layers::sync_layers`].
pub fn repair_stranded_focus(
    mode: Res<UiInputMode>,
    focusables: FocusableQuery,
    nav: NavWorld,
    mut state: ResMut<FocusState>,
    mut commands: Commands,
    mut events: MessageWriter<UiEvent>,
) {
    let Some(f) = state.focused else { return };
    let alive = nav.entities.contains(f);
    // "In the navigable set" (focusable, laid out, not disabled) is stricter than alive: a
    // focused node that gains `Disabled` or loses `Focusable` strands focus the same way a
    // despawn does, while a node that never laid out yet keeps it.
    let candidate = focusables.get(f).ok().filter(|_| !nav.subtree_disabled(f));
    let visible = candidate.is_some_and(|(_, node, _)| has_area(node.rect));
    if alive && visible {
        return;
    }
    let memory = nav.memory();
    // A node focused before its first layout has no rect yet: only one that was seen visible
    // counts as hidden.
    let was_visible = memory.last().is_some_and(|(e, _)| e == f);
    if alive && candidate.is_none() {
        // Not a navigation candidate: laid out (has a computed rect) but disabled or
        // unfocusable — stranded, repair below. No computed rect yet — keep focus.
        if nav.nodes.get(f).is_err() {
            return;
        }
    } else if alive && !was_visible {
        return;
    }
    if *mode != UiInputMode::Navigation {
        if !alive {
            state.focused = None;
        }
        return;
    }
    let _span = tracing::trace_span!("ui_focus_repair").entered();
    let graph = nav.graph(&focusables, state.scope);
    let step = graph.recover(false, &nav.defaults(), &memory);
    if !alive {
        state.focused = None;
    }
    match step.target {
        Some(t) => set_focus(Some(t), &mut state, &mut commands, &mut events),
        None if alive => set_focus(None, &mut state, &mut commands, &mut events),
        None => {}
    }
}

/// `Update` (after [`repair_stranded_focus`]): records the last visible focus and each
/// remembering group's last focused child in [`FocusMemory`]. Costs one parent walk when focus
/// changed, nothing otherwise.
pub fn track_focus_memory(
    state: Res<FocusState>,
    nodes: Query<&ComputedNode>,
    groups: Query<&FocusGroup>,
    parents: Query<&ChildOf>,
    mut memory: ResMut<FocusMemory>,
) {
    let _span = tracing::debug_span!("track_focus_memory").entered();
    let Some(f) = state.focused else { return };
    let Ok(node) = nodes.get(f) else { return };
    if !has_area(node.rect) || memory.last == Some((f, node.rect)) {
        return;
    }
    let moved = memory.last.map(|(e, _)| e) != Some(f);
    memory.record(f, node.rect);
    if !moved {
        return;
    }
    memory.groups.retain(|g, _| groups.contains(*g));
    let mut current = Some(f);
    for _ in 0..1024 {
        let Some(e) = current else { break };
        if groups.get(e).is_ok_and(|g| g.remember) {
            memory.remember(e, f);
        }
        current = parents.get(e).ok().map(|p| p.0);
    }
}

/// Installs [`FocusMemory`], [`repair_stranded_focus`] and [`track_focus_memory`] (added by
/// [`crate::UiPlugin`]).
pub struct FocusGraphPlugin;

impl Plugin for FocusGraphPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FocusMemory>().add_systems(
            Update,
            (repair_stranded_focus, track_focus_memory)
                .chain()
                .after(crate::focus::ui_focus_navigation)
                .before(crate::widgets::ui_widgets),
        );
    }
}

/// Errors of the world-level navigation calls (`ui.nav_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavError {
    /// The app has no [`FocusState`] (no `UiPlugin`).
    NotInstalled,
    /// The active scope has no navigable node.
    NoFocusables,
}

impl NavError {
    /// The agent-protocol code (lowercase).
    pub fn code(self) -> &'static str {
        match self {
            Self::NotInstalled => "ui_not_installed",
            Self::NoFocusables => "no_focusables",
        }
    }
}

impl std::fmt::Display for NavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotInstalled => "the app has no runtime UI (FocusState is missing)",
            Self::NoFocusables => "the active scope has no navigable node",
        })
    }
}

impl std::error::Error for NavError {}

/// The navigation scope agents see: the top input layer, else the scope focus navigation last
/// used.
pub fn world_scope(world: &World) -> Option<Entity> {
    world
        .get_resource::<crate::layers::InputLayers>()
        .and_then(|l| l.top_root())
        .or_else(|| world.get_resource::<FocusState>().and_then(|s| s.scope))
}

/// The focus graph of the active scope, built from `world` (the agent path).
pub fn world_graph(world: &mut World) -> NavGraph {
    let scope = world_scope(world);
    let focusables: Vec<(Entity, [f32; 4], NavLinks, Option<i32>)> = world
        .query_filtered::<(Entity, &ComputedNode, &Focusable, Option<&TabIndex>), Without<Disabled>>()
        .iter(world)
        .filter(|(e, ..)| !crate::node::is_disabled(world, *e))
        .map(|(e, c, f, t)| (e, c.rect, f.nav, t.map(|t| t.0)))
        .collect();
    let world_ref: &World = world;
    NavGraph::new(collect_inputs(
        focusables.into_iter(),
        |e| world_ref.get::<ChildOf>(e).map(|p| p.0),
        |e| {
            world_ref
                .get::<FocusGroup>(e)
                .map(|g| (*g, world_ref.get::<ComputedNode>(e).map(|c| c.rect)))
        },
        scope,
    ))
}

/// The [`DefaultFocus`] nodes of `world`.
pub fn world_defaults(world: &mut World) -> Vec<Entity> {
    world
        .query_filtered::<Entity, With<DefaultFocus>>()
        .iter(world)
        .collect()
}

/// Where the focus of `world` stands relative to `graph`.
pub fn world_origin(world: &World, graph: &NavGraph) -> FocusOrigin {
    let Some(f) = world.get_resource::<FocusState>().and_then(|s| s.focused) else {
        return FocusOrigin::Unfocused;
    };
    if graph.contains(f) {
        return FocusOrigin::Candidate(f);
    }
    if world.get_entity(f).is_err() {
        return FocusOrigin::Lost;
    }
    if let Some(scope) = world_scope(world) {
        let mut current = Some(f);
        let mut inside = false;
        for _ in 0..1024 {
            let Some(e) = current else { break };
            if e == scope {
                inside = true;
                break;
            }
            current = world.get::<ChildOf>(e).map(|p| p.0);
        }
        if !inside {
            return FocusOrigin::OutOfScope;
        }
    }
    match world.get::<ComputedNode>(f) {
        Some(c) if has_area(c.rect) => FocusOrigin::Inert(f, c.rect),
        _ => FocusOrigin::Lost,
    }
}

/// The result of [`nav_move`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavMove {
    /// Focus before the move.
    pub from: Option<Entity>,
    /// The step taken.
    pub step: NavStep,
    /// Focus after the move.
    pub focused: Option<Entity>,
}

/// Applies one navigation press in `dir` to `world` at once, exactly as a key or gamepad press
/// would move focus (same graph, same rules), and switches the input mode to navigation so the
/// focus ring shows. Writes the `Focused` marker and `FocusChanged` events like a real press.
pub fn nav_move(world: &mut World, dir: NavDirection) -> Result<NavMove, NavError> {
    if world.get_resource::<FocusState>().is_none() {
        return Err(NavError::NotInstalled);
    }
    let graph = world_graph(world);
    if graph.is_empty() {
        return Err(NavError::NoFocusables);
    }
    let defaults = world_defaults(world);
    let origin = world_origin(world, &graph);
    let memory = world.get_resource::<FocusMemory>().cloned().unwrap_or_default();
    let step = graph.resolve(origin, dir, &defaults, &memory);
    if let Some(mut mode) = world.get_resource_mut::<UiInputMode>()
        && *mode != UiInputMode::Navigation
    {
        *mode = UiInputMode::Navigation;
    }
    let from = world.resource::<FocusState>().focused;
    if let Some(target) = step.target
        && Some(target) != from
    {
        apply_world_focus(world, from, target);
    }
    Ok(NavMove {
        from,
        step,
        focused: world.resource::<FocusState>().focused,
    })
}

/// `set_focus` for a `&mut World`: moves the `Focused` marker, writes `FocusChanged` events and
/// updates [`FocusState`]. A despawned previous node gets no event.
fn apply_world_focus(world: &mut World, prev: Option<Entity>, next: Entity) {
    let prev = prev.filter(|p| world.get_entity(*p).is_ok());
    if let Some(p) = prev
        && let Ok(mut entity) = world.get_entity_mut(p)
    {
        entity.remove::<Focused>();
    }
    if let Ok(mut entity) = world.get_entity_mut(next) {
        entity.insert(Focused);
    }
    if let Some(p) = prev {
        let _ = world.write_message(UiEvent::new(p, UiEventKind::FocusChanged(false)));
    }
    let _ = world.write_message(UiEvent::new(next, UiEventKind::FocusChanged(true)));
    world.resource_mut::<FocusState>().focused = Some(next);
    // Record the rect now: a node hidden in the same frame it was focused (a mutation between
    // updates) must repair at once; waiting for `track_focus_memory` would mistake it for a
    // node that never had a rect.
    if let Some(rect) = world
        .get::<ComputedNode>(next)
        .filter(|node| has_area(node.rect))
        .map(|node| node.rect)
        && let Some(mut memory) = world.get_resource_mut::<FocusMemory>()
    {
        memory.record(next, rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<Entity> {
        let mut world = World::new();
        (0..n).map(|_| world.spawn_empty().id()).collect()
    }

    fn input(entity: Entity, rect: [f32; 4]) -> NavInput {
        NavInput {
            entity,
            rect,
            links: NavLinks::default(),
            tab_index: None,
            groups: Vec::new(),
        }
    }

    fn grouped(entity: Entity, rect: [f32; 4], group: Entity, rules: FocusGroup) -> NavInput {
        NavInput {
            groups: vec![(group, rules, None)],
            ..input(entity, rect)
        }
    }

    fn go(graph: &NavGraph, from: Entity, dir: NavDirection) -> NavStep {
        let rect = graph.node(from).expect("in graph").rect;
        graph.step(from, rect, dir, &FocusMemory::default())
    }

    /// A 3 x 3 grid of 100 x 40 cells, 20 px apart, row by row.
    fn grid(e: &[Entity], group: Option<(Entity, FocusGroup)>) -> NavGraph {
        NavGraph::new(
            (0..9)
                .map(|i| {
                    let rect = [(i % 3) as f32 * 120.0, (i / 3) as f32 * 60.0, 100.0, 40.0];
                    match group {
                        Some((g, rules)) => grouped(e[i], rect, g, rules),
                        None => input(e[i], rect),
                    }
                })
                .collect(),
        )
    }

    #[test]
    fn without_groups_moves_follow_geometry_and_stop_at_the_edge() {
        let e = ids(9);
        let g = grid(&e, None);
        assert_eq!(
            go(&g, e[4], NavDirection::Up),
            NavStep::to(e[1], NavReason::Geometry)
        );
        assert_eq!(go(&g, e[4], NavDirection::Down).target, Some(e[7]));
        assert_eq!(go(&g, e[4], NavDirection::Left).target, Some(e[3]));
        assert_eq!(go(&g, e[4], NavDirection::Right).target, Some(e[5]));
        assert_eq!(go(&g, e[8], NavDirection::Down), NavStep::none(NavReason::Edge));
        assert_eq!(go(&g, e[2], NavDirection::Right), NavStep::none(NavReason::Edge));
    }

    #[test]
    fn wrap_jumps_to_the_same_row_or_column_on_the_far_side() {
        let e = ids(10);
        let g = grid(&e, Some((e[9], FocusGroup::wrap())));
        assert_eq!(
            go(&g, e[5], NavDirection::Right),
            NavStep::to(e[3], NavReason::Wrap)
        );
        assert_eq!(go(&g, e[3], NavDirection::Left).target, Some(e[5]));
        assert_eq!(go(&g, e[7], NavDirection::Down).target, Some(e[1]));
        assert_eq!(go(&g, e[1], NavDirection::Up).target, Some(e[7]));
        // Inside the grid nothing changes.
        assert_eq!(go(&g, e[4], NavDirection::Right).target, Some(e[5]));
    }

    #[test]
    fn a_single_column_wraps_to_its_top_and_a_lone_node_does_not_move() {
        let e = ids(4);
        let rules = FocusGroup::wrap_vertical();
        let g = NavGraph::new(
            (0..3)
                .map(|i| grouped(e[i], [0.0, i as f32 * 50.0, 100.0, 40.0], e[3], rules))
                .collect(),
        );
        assert_eq!(go(&g, e[2], NavDirection::Down).target, Some(e[0]));
        assert_eq!(go(&g, e[0], NavDirection::Up).target, Some(e[2]));
        // Left/right escape, and there is nothing outside.
        assert_eq!(go(&g, e[1], NavDirection::Left), NavStep::none(NavReason::Edge));
        let lone = NavGraph::new(vec![grouped(e[0], [0.0, 0.0, 10.0, 10.0], e[3], rules)]);
        assert_eq!(
            go(&lone, e[0], NavDirection::Down),
            NavStep::none(NavReason::Edge)
        );
    }

    #[test]
    fn stop_keeps_focus_and_escape_reaches_the_neighbor_group() {
        let e = ids(6);
        // Two side-by-side columns; the left one stops at its right edge, the right one escapes.
        let left = e[4];
        let right = e[5];
        let build = |left_rules: FocusGroup| {
            NavGraph::new(vec![
                grouped(e[0], [0.0, 0.0, 100.0, 40.0], left, left_rules),
                grouped(e[1], [0.0, 60.0, 100.0, 40.0], left, left_rules),
                grouped(e[2], [300.0, 0.0, 100.0, 40.0], right, FocusGroup::new()),
                grouped(e[3], [300.0, 60.0, 100.0, 40.0], right, FocusGroup::new()),
            ])
        };
        let trapped = build(FocusGroup::trap());
        assert_eq!(
            go(&trapped, e[0], NavDirection::Right),
            NavStep::none(NavReason::Stop)
        );
        assert_eq!(
            go(&trapped, e[0], NavDirection::Down).target,
            Some(e[1]),
            "inside still moves"
        );
        assert_eq!(
            go(&trapped, e[2], NavDirection::Left).target,
            Some(e[0]),
            "the other group escapes in"
        );
        let open = build(FocusGroup::new());
        assert_eq!(go(&open, e[1], NavDirection::Right).target, Some(e[3]));
    }

    #[test]
    fn a_group_prefers_its_own_members_over_a_closer_outsider() {
        let e = ids(4);
        // From a (in the group) moving right: b is in the group but lower; c is outside, aligned.
        let g = NavGraph::new(vec![
            grouped(e[0], [0.0, 0.0, 100.0, 40.0], e[3], FocusGroup::new()),
            grouped(e[1], [150.0, 100.0, 100.0, 40.0], e[3], FocusGroup::new()),
            input(e[2], [120.0, 0.0, 100.0, 40.0]),
        ]);
        assert_eq!(go(&g, e[0], NavDirection::Right).target, Some(e[1]));
        let flat = NavGraph::new(vec![
            input(e[0], [0.0, 0.0, 100.0, 40.0]),
            input(e[1], [150.0, 100.0, 100.0, 40.0]),
            input(e[2], [120.0, 0.0, 100.0, 40.0]),
        ]);
        assert_eq!(go(&flat, e[0], NavDirection::Right).target, Some(e[2]));
    }

    #[test]
    fn nested_groups_ask_the_innermost_first() {
        let e = ids(6);
        let (inner, outer) = (e[4], e[5]);
        let chain = |n: Entity, rect, inner_too: bool| NavInput {
            groups: if inner_too {
                vec![
                    (inner, FocusGroup::wrap_horizontal(), None),
                    (outer, FocusGroup::trap(), None),
                ]
            } else {
                vec![(outer, FocusGroup::trap(), None)]
            },
            ..input(n, rect)
        };
        let g = NavGraph::new(vec![
            chain(e[0], [0.0, 0.0, 100.0, 40.0], true),
            chain(e[1], [120.0, 0.0, 100.0, 40.0], true),
            chain(e[2], [0.0, 60.0, 100.0, 40.0], false),
            input(e[3], [0.0, 200.0, 100.0, 40.0]),
        ]);
        assert_eq!(
            go(&g, e[1], NavDirection::Right),
            NavStep::to(e[0], NavReason::Wrap)
        );
        assert_eq!(
            go(&g, e[0], NavDirection::Down).target,
            Some(e[2]),
            "escapes the inner row"
        );
        assert_eq!(
            go(&g, e[2], NavDirection::Down),
            NavStep::none(NavReason::Stop),
            "the outer trap holds"
        );
    }

    #[test]
    fn explicit_links_win_only_when_the_target_is_navigable() {
        let e = ids(4);
        let mut a = input(e[0], [0.0, 0.0, 100.0, 40.0]);
        a.links.down = Some(e[2]);
        a.links.right = Some(e[3]); // not in the graph
        let g = NavGraph::new(vec![
            a,
            input(e[1], [0.0, 60.0, 100.0, 40.0]),
            input(e[2], [0.0, 400.0, 100.0, 40.0]),
            input(e[3], [0.0, 0.0, 0.0, 0.0]), // no area: dropped
        ]);
        assert!(!g.contains(e[3]));
        assert_eq!(
            go(&g, e[0], NavDirection::Down),
            NavStep::to(e[2], NavReason::Explicit)
        );
        assert_eq!(go(&g, e[0], NavDirection::Right), NavStep::none(NavReason::Edge));
    }

    #[test]
    fn a_link_to_itself_blocks_the_direction() {
        // The calibration screen links a slider's left/right to itself so they never move focus.
        let e = ids(2);
        let mut slider = input(e[0], [0.0, 0.0, 100.0, 40.0]);
        slider.links.left = Some(e[0]);
        slider.links.right = Some(e[0]);
        let g = NavGraph::new(vec![slider, input(e[1], [200.0, 0.0, 100.0, 40.0])]);
        assert_eq!(go(&g, e[0], NavDirection::Right), NavStep::none(NavReason::Stop));
        assert_eq!(
            go(&g, e[1], NavDirection::Left).target,
            Some(e[0]),
            "others still reach it"
        );
    }

    #[test]
    fn tab_order_puts_positive_indices_first_and_skips_negative_ones() {
        let e = ids(5);
        let with = |n: Entity, y: f32, t: Option<i32>| NavInput {
            tab_index: t,
            ..input(n, [0.0, y, 100.0, 40.0])
        };
        let g = NavGraph::new(vec![
            with(e[0], 0.0, None),
            with(e[1], 50.0, Some(2)),
            with(e[2], 100.0, Some(-1)),
            with(e[3], 150.0, Some(1)),
            with(e[4], 200.0, Some(0)),
        ]);
        let order: Vec<Entity> = g.tab_order(None).iter().map(|&i| g.nodes()[i].entity).collect();
        assert_eq!(order, vec![e[3], e[1], e[0], e[4]]);
        assert_eq!(
            go(&g, e[4], NavDirection::Next),
            NavStep::to(e[3], NavReason::Tab),
            "wraps"
        );
        assert_eq!(go(&g, e[3], NavDirection::Prev).target, Some(e[4]));
        assert_eq!(
            go(&g, e[2], NavDirection::Next).target,
            Some(e[3]),
            "a skipped node tabs to the first"
        );
        assert_eq!(
            go(&g, e[2], NavDirection::Down).target,
            Some(e[3]),
            "but arrows still reach it"
        );
    }

    #[test]
    fn a_cycling_group_keeps_tab_inside() {
        let e = ids(5);
        let g = NavGraph::new(vec![
            input(e[0], [0.0, 0.0, 100.0, 40.0]),
            grouped(e[1], [0.0, 50.0, 100.0, 40.0], e[4], FocusGroup::trap()),
            grouped(e[2], [0.0, 100.0, 100.0, 40.0], e[4], FocusGroup::trap()),
            input(e[3], [0.0, 150.0, 100.0, 40.0]),
        ]);
        assert_eq!(go(&g, e[2], NavDirection::Next).target, Some(e[1]));
        assert_eq!(go(&g, e[1], NavDirection::Prev).target, Some(e[2]));
        assert_eq!(
            go(&g, e[0], NavDirection::Next).target,
            Some(e[1]),
            "from outside Tab enters"
        );
    }

    #[test]
    fn a_remembering_group_is_re_entered_at_its_last_child() {
        let e = ids(5);
        let group = e[4];
        let rules = FocusGroup::new().remembering();
        let g = NavGraph::new(vec![
            input(e[0], [0.0, 0.0, 100.0, 40.0]),
            grouped(e[1], [200.0, 0.0, 100.0, 40.0], group, rules),
            grouped(e[2], [200.0, 60.0, 100.0, 40.0], group, rules),
            grouped(e[3], [200.0, 120.0, 100.0, 40.0], group, rules),
        ]);
        let mut memory = FocusMemory::default();
        let rect = g.node(e[0]).expect("a").rect;
        assert_eq!(
            g.step(e[0], rect, NavDirection::Right, &memory).target,
            Some(e[1])
        );
        memory.remember(group, e[3]);
        assert_eq!(
            g.step(e[0], rect, NavDirection::Right, &memory),
            NavStep::to(e[3], NavReason::Remembered)
        );
        // Moving inside the group is not redirected.
        let r2 = g.node(e[2]).expect("c").rect;
        assert_eq!(g.step(e[2], r2, NavDirection::Up, &memory).target, Some(e[1]));
    }

    #[test]
    fn resolve_reveals_first_then_recovers_lost_focus_near_where_it_was() {
        let e = ids(4);
        let g = NavGraph::new(vec![
            input(e[0], [0.0, 0.0, 100.0, 40.0]),
            input(e[1], [0.0, 60.0, 100.0, 40.0]),
            input(e[2], [0.0, 120.0, 100.0, 40.0]),
        ]);
        let mut memory = FocusMemory::default();
        let down = NavDirection::Down;
        assert_eq!(
            g.resolve(FocusOrigin::Unfocused, down, &[e[1]], &memory),
            NavStep::to(e[1], NavReason::Initial)
        );
        assert_eq!(
            g.resolve(FocusOrigin::Unfocused, down, &[e[3]], &memory).target,
            Some(e[0]),
            "a default outside the graph falls back to Tab order"
        );
        memory.record(e[3], [0.0, 125.0, 100.0, 40.0]);
        assert_eq!(
            g.resolve(FocusOrigin::Lost, down, &[e[0]], &memory),
            NavStep::to(e[2], NavReason::Recovered)
        );
        assert_eq!(
            g.resolve(FocusOrigin::OutOfScope, down, &[e[0]], &memory).target,
            Some(e[0]),
            "a new scope starts at its default"
        );
        assert_eq!(
            g.resolve(
                FocusOrigin::Inert(e[3], [0.0, 30.0, 100.0, 20.0]),
                down,
                &[],
                &memory
            )
            .target,
            Some(e[1]),
            "a disabled focus moves from its own rect"
        );
        assert_eq!(
            NavGraph::default().resolve(FocusOrigin::Unfocused, down, &[], &memory),
            NavStep::none(NavReason::Edge)
        );
    }

    #[test]
    fn collect_inputs_keeps_the_scope_and_lists_groups_innermost_first() {
        let e = ids(6);
        let (root, panel, row, outside, a, b) = (e[0], e[1], e[2], e[3], e[4], e[5]);
        let parent = |x: Entity| {
            if x == a {
                Some(row)
            } else if x == row {
                Some(panel)
            } else if x == panel || x == b {
                Some(root)
            } else {
                None
            }
        };
        let group = |x: Entity| {
            if x == row {
                Some((FocusGroup::wrap_horizontal(), None))
            } else if x == panel {
                Some((FocusGroup::trap(), Some([0.0, 0.0, 500.0, 500.0])))
            } else if x == root {
                Some((FocusGroup::new(), None))
            } else {
                None
            }
        };
        let rect = [0.0, 0.0, 10.0, 10.0];
        let items = vec![
            (a, rect, NavLinks::default(), None),
            (b, rect, NavLinks::default(), None),
            (outside, rect, NavLinks::default(), None),
        ];
        let scoped = collect_inputs(items.clone().into_iter(), parent, group, Some(panel));
        assert_eq!(scoped.len(), 1, "b and the unparented node are outside the panel");
        let chain: Vec<Entity> = scoped[0].groups.iter().map(|g| g.0).collect();
        assert_eq!(chain, vec![row, panel], "groups above the scope do not apply");
        let all = collect_inputs(items.into_iter(), parent, group, None);
        assert_eq!(all.len(), 3);
        let chain: Vec<Entity> = all[0].groups.iter().map(|g| g.0).collect();
        assert_eq!(chain, vec![row, panel, root]);
    }

    #[test]
    fn lint_finds_unreachable_dead_ends_and_bad_links() {
        let e = ids(7);
        let mut a = input(e[0], [0.0, 0.0, 100.0, 40.0]);
        a.links.right = Some(e[5]); // not navigable
        a.links.down = Some(e[2]); // c's up goes to b, not a
        let hidden_from_tab = NavInput {
            tab_index: Some(-1),
            groups: vec![(e[6], FocusGroup::trap(), None)],
            ..input(e[3], [600.0, 600.0, 100.0, 40.0])
        };
        let g = NavGraph::new(vec![
            a,
            input(e[1], [0.0, 60.0, 100.0, 40.0]),
            input(e[2], [0.0, 120.0, 100.0, 40.0]),
            hidden_from_tab,
            input(e[4], [0.0, 120.0, 100.0, 40.0]),
        ]);
        let issues = g.lint(Some(e[0]), &[]);
        let has = |kind: NavIssueKind, node: Entity| issues.iter().any(|i| i.kind == kind && i.node == node);
        assert!(has(NavIssueKind::DeadLink, e[0]), "{issues:?}");
        assert!(has(NavIssueKind::OneWayLink, e[0]), "{issues:?}");
        assert!(
            has(NavIssueKind::DeadEnd, e[3]),
            "a trapped, untabbable node: {issues:?}"
        );
        assert!(has(NavIssueKind::DuplicateRect, e[2]) || has(NavIssueKind::DuplicateRect, e[4]));
        assert!(!has(NavIssueKind::Unreachable, e[1]));
        let codes: Vec<&str> = issues.iter().map(|i| i.kind.code()).collect();
        assert!(
            codes
                .iter()
                .all(|c| c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
        );
    }

    #[test]
    fn names_round_trip_and_are_protocol_safe() {
        for dir in ALL_DIRECTIONS {
            assert_eq!(parse_direction(direction_name(dir)), Some(dir));
            assert_eq!(opposite(opposite(dir)), dir);
        }
        assert_eq!(parse_direction("PREVIOUS"), Some(NavDirection::Prev));
        assert_eq!(parse_direction("sideways"), None);
        for code in [
            NavError::NotInstalled.code(),
            NavError::NoFocusables.code(),
            EdgeRule::Wrap.name(),
            TabRule::Cycle.name(),
            NavReason::Remembered.name(),
        ] {
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
        }
        assert_eq!(
            FocusGroup::new().with_edge(NavDirection::Up, EdgeRule::Stop).up,
            EdgeRule::Stop
        );
        assert_eq!(FocusGroup::trap().edge(NavDirection::Next), EdgeRule::Escape);
    }
}
