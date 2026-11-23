// SPDX-License-Identifier: MPL-2.0
//! In-world AI debug draw (ADR-8463).
//!
//! The `ai_debug` gizmo group is off until `debug.set_gizmos` or a viewport Show flag turns it on.
//! [`AiDebugDraw`] chooses which layers run, which agents they include, and how many text labels
//! one frame may emit. The behavior-tree layer names the deepest running leaf. The move layer
//! draws [`regia_framework::character_control::MoveTo`] and the waypoints still ahead on
//! [`regia_nav::NavAgentState`]. A separate path-provider status is not read.

use regia_app::prelude::*;
use regia_core::math::{Color, Vec2, Vec3};

use crate::BtStatus;
use crate::tree::{BehaviorTree, DefKind, MAX_BT_DEPTH};

/// World size of one debug label (cap height).
pub const LABEL_SIZE: f32 = 0.35;
/// Metres above the agent where stack 0 sits.
pub const LABEL_LIFT: f32 = 2.0;
/// Extra metres of lift for each stacked label on the same agent.
pub const LABEL_STEP: f32 = 0.45;
/// Text-box anchor: `(0, 0)` is the centre.
pub const LABEL_ANCHOR: Vec2 = Vec2::ZERO;
/// Half-length of a debug cross, in metres.
pub const CROSS: f32 = 0.35;
/// Default labels kept per frame.
pub const DEFAULT_LABEL_CAP: u32 = 64;
/// Default radius filter, in metres.
pub const DEFAULT_RADIUS: f32 = 20.0;
/// Most blackboard keys one `ai.debug_layers_set` may store.
pub const MAX_WATCH_KEYS: usize = 32;
/// Most bytes in one watched key.
pub const MAX_WATCH_KEY_BYTES: usize = 64;

/// Stack slot of the behavior-tree leaf name.
pub const STACK_BT: u32 = 0;
/// Stack slot of the state-tree state name.
pub const STACK_STATE: u32 = 1;
/// Stack slot of the GOAP plan step.
pub const STACK_GOAP: u32 = 2;
/// First stack slot of GOAP goal facts.
pub const STACK_GOAL: u32 = 3;
/// First stack slot of blackboard watch keys.
pub const STACK_BLACKBOARD: u32 = 4;
/// Stack slot of a squad role letter.
pub const STACK_ROLE: u32 = 6;

/// Behavior-tree label color.
pub const BT_COLOR: Color = Color::rgb(0.25, 0.95, 0.45);
/// State-tree label and cross color.
pub const STATE_COLOR: Color = Color::rgb(0.35, 0.75, 1.0);
/// GOAP label and cross color.
pub const GOAP_COLOR: Color = Color::rgb(1.0, 0.78, 0.2);
/// Squad line and role-letter color.
pub const SQUAD_COLOR: Color = Color::rgb(1.0, 0.45, 0.75);
/// Formation slot color.
pub const FORMATION_COLOR: Color = Color::rgb(0.62, 0.45, 1.0);
/// Move target and remaining-path color.
pub const MOVE_COLOR: Color = Color::rgb(1.0, 0.55, 0.15);
/// Blackboard watch-key color.
pub const BB_COLOR: Color = Color::rgb(0.9, 0.9, 0.95);

/// Which in-world AI layers draw. Every flag starts off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AiDebugLayers {
    /// The running behavior-tree leaf's name.
    pub behavior_tree: bool,
    /// The active state-tree state.
    pub state_tree: bool,
    /// The current GOAP step, goal facts, and a cross.
    pub goap: bool,
    /// Squad lines and role letters.
    pub squad: bool,
    /// Formation slot crosses and member lines.
    pub formation: bool,
    /// MoveTo target, remaining nav waypoints, and target crosses.
    pub move_to: bool,
    /// Watched blackboard keys.
    pub blackboard: bool,
}

impl AiDebugLayers {
    /// Layer names, in flag order. `ai.debug_layers` returns the enabled ones in this order.
    pub const NAMES: &'static [&'static str] = &[
        "bt",
        "state_tree",
        "goap",
        "squad",
        "formation",
        "move_to",
        "blackboard",
    ];

    /// Whether `name` is one of [`AiDebugLayers::NAMES`].
    #[must_use]
    pub fn known(name: &str) -> bool {
        Self::NAMES.contains(&name)
    }

    /// Turns `name` on. Returns false when `name` is not a layer.
    pub fn enable(&mut self, name: &str) -> bool {
        let slot = match name {
            "bt" => &mut self.behavior_tree,
            "state_tree" => &mut self.state_tree,
            "goap" => &mut self.goap,
            "squad" => &mut self.squad,
            "formation" => &mut self.formation,
            "move_to" => &mut self.move_to,
            "blackboard" => &mut self.blackboard,
            _ => return false,
        };
        *slot = true;
        true
    }

    /// Every layer off.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// True when any layer is on.
    #[must_use]
    pub const fn any(self) -> bool {
        self.behavior_tree
            || self.state_tree
            || self.goap
            || self.squad
            || self.formation
            || self.move_to
            || self.blackboard
    }

    /// Builds the flags from `[bt, state_tree, goap, squad, formation, move_to, blackboard]`.
    #[must_use]
    pub const fn from_flags(flags: [bool; 7]) -> Self {
        Self {
            behavior_tree: flags[0],
            state_tree: flags[1],
            goap: flags[2],
            squad: flags[3],
            formation: flags[4],
            move_to: flags[5],
            blackboard: flags[6],
        }
    }

    /// The flags in [`AiDebugLayers::NAMES`] order.
    #[must_use]
    pub const fn flags(self) -> [bool; 7] {
        [
            self.behavior_tree,
            self.state_tree,
            self.goap,
            self.squad,
            self.formation,
            self.move_to,
            self.blackboard,
        ]
    }

    /// Enabled layer names, in [`AiDebugLayers::NAMES`] order.
    pub fn enabled_names(self) -> impl Iterator<Item = &'static str> {
        Self::NAMES
            .iter()
            .copied()
            .zip(self.flags())
            .filter_map(|(name, on)| on.then_some(name))
    }
}

/// Which agents a layer includes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AiDebugFilter {
    /// Every agent that is not parked.
    #[default]
    All,
    /// Only [`AiDebugDraw::selected`].
    Selected,
    /// Agents within [`AiDebugDraw::radius`] of the selected pose, or of [`AiDebugDraw::focus`].
    Radius,
}

impl AiDebugFilter {
    /// The protocol name: `all`, `selected`, or `radius`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Selected => "selected",
            Self::Radius => "radius",
        }
    }

    /// Parses a protocol name. Empty and unknown names are `None`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "all" => Some(Self::All),
            "selected" => Some(Self::Selected),
            "radius" => Some(Self::Radius),
            _ => None,
        }
    }
}

/// Runtime settings for the in-world AI overlay. Not reflected and not saved.
///
/// [`AiPlugin`](crate::AiPlugin) inserts the default (every layer off). `ai.debug_layers` reads it
/// and `ai.debug_layers_set` replaces the fields that the call names. Turning layers on does not
/// enable the gizmo group; `debug.set_gizmos { group: "ai_debug", enabled: true }` does that.
#[derive(Resource, Clone, Debug)]
pub struct AiDebugDraw {
    /// Per-layer switches.
    pub layers: AiDebugLayers,
    /// Who is included.
    pub filter: AiDebugFilter,
    /// Radius of [`AiDebugFilter::Radius`], in metres.
    pub radius: f32,
    /// Text labels emitted per frame. Further labels increment [`AiDebugDraw::dropped`].
    pub label_cap: u32,
    /// Labels skipped because of [`AiDebugDraw::label_cap`] during the last draw. Shapes do not count.
    pub dropped: u32,
    /// The entity [`AiDebugFilter::Selected`] draws, and the focus of [`AiDebugFilter::Radius`] when
    /// that entity has a finite pose.
    pub selected: Option<Entity>,
    /// Blackboard keys the blackboard layer labels, in order.
    pub watch_keys: Vec<String>,
    /// Radius-filter focus when [`AiDebugDraw::selected`] has no finite pose.
    pub focus: Vec3,
}

impl Default for AiDebugDraw {
    fn default() -> Self {
        Self {
            layers: AiDebugLayers::default(),
            filter: AiDebugFilter::All,
            radius: DEFAULT_RADIUS,
            label_cap: DEFAULT_LABEL_CAP,
            dropped: 0,
            selected: None,
            watch_keys: Vec::new(),
            focus: Vec3::ZERO,
        }
    }
}

impl AiDebugDraw {
    /// Clears [`AiDebugDraw::dropped`] and reports whether any layer is on.
    ///
    /// The draw system returns before walking agents when this is false. The call itself does not
    /// allocate.
    #[must_use]
    pub fn begin_frame(&mut self) -> bool {
        self.dropped = 0;
        self.layers.any()
    }
}

/// World position of a label stacked above `origin`.
#[must_use]
pub fn label_anchor(origin: Vec3, stack: u32) -> Vec3 {
    origin + Vec3::Y * (LABEL_LIFT + stack as f32 * LABEL_STEP)
}

/// The name of the deepest running leaf.
///
/// Starts at the tree root. A node that is not [`BtStatus::Running`] stops the walk. Otherwise the
/// walk follows the first child in definition order whose status is `Running`, until a
/// [`DefKind::Leaf`] or a node with no children. A repeated index, a root outside the arena, or a
/// status list that does not match the node list returns `None`. The walk stops at
/// [`MAX_BT_DEPTH`].
///
/// Compile fills every status with `Running`, so a tree that has not ticked reports its first leaf.
/// [`crate::AiPlugin`] writes the statuses the overlay reads.
#[must_use]
pub fn active_leaf_name(tree: &BehaviorTree) -> Option<&str> {
    let nodes = tree.def.nodes.as_slice();
    let statuses = tree.node_statuses();
    if statuses.len() != nodes.len() || tree.def.root >= nodes.len() {
        return None;
    }
    let mut index = tree.def.root;
    let mut seen = [usize::MAX; MAX_BT_DEPTH];
    for depth in 0..MAX_BT_DEPTH {
        if seen[..depth].contains(&index) {
            return None;
        }
        seen[depth] = index;
        if statuses[index] != BtStatus::Running {
            return None;
        }
        let node = &nodes[index];
        if node.kind == DefKind::Leaf || node.children.is_empty() {
            return Some(node.name.as_str());
        }
        let mut next = None;
        for &child in &node.children {
            if child < statuses.len() && statuses[child] == BtStatus::Running {
                next = Some(child);
                break;
            }
        }
        index = next?;
    }
    None
}

#[cfg(feature = "debug_draw")]
mod draw {
    use std::fmt::Write;

    use regia_app::prelude::*;
    use regia_core::math::{Color, Vec3};
    use regia_ecs::ecs::system::SystemParam;
    use regia_framework::character_control::MoveTo;
    use regia_nav::NavAgentState;
    use regia_render::gizmos::{AiDebugGizmos, GizmoBuffer, Gizmos};
    use regia_scene::layers::LayerParked;
    use regia_scene::{GlobalTransform, Transform};

    use super::{
        AiDebugDraw, AiDebugFilter, BB_COLOR, BT_COLOR, CROSS, FORMATION_COLOR, GOAP_COLOR, LABEL_ANCHOR,
        LABEL_SIZE, MOVE_COLOR, SQUAD_COLOR, STACK_BLACKBOARD, STACK_BT, STACK_GOAL, STACK_GOAP, STACK_ROLE,
        STACK_STATE, STATE_COLOR, active_leaf_name, label_anchor,
    };
    use crate::blackboard::{BbValue, Blackboard};
    use crate::formation::AiFormation;
    use crate::goap_ecs::{GoapAgent, GoapStatus};
    use crate::squad::{AiSquad, AiSquadMember};
    use crate::state_tree::{StateTree, StateTreeStatus};
    use crate::tree::BehaviorTree;

    type PoseQuery<'w, 's> =
        Query<'w, 's, (&'static Transform, Option<&'static GlobalTransform>), Without<LayerParked>>;
    type MemberQuery<'w, 's> = Query<'w, 's, &'static AiSquadMember, Without<LayerParked>>;
    type StateOnlyQuery<'w, 's> =
        Query<'w, 's, (Entity, &'static StateTree), (Without<StateTreeStatus>, Without<LayerParked>)>;

    #[derive(SystemParam)]
    struct AiDebugQueries<'w, 's> {
        poses: PoseQuery<'w, 's>,
        trees: Query<'w, 's, (Entity, &'static BehaviorTree), Without<LayerParked>>,
        states: Query<
            'w,
            's,
            (Entity, Option<&'static StateTree>, &'static StateTreeStatus),
            Without<LayerParked>,
        >,
        state_only: StateOnlyQuery<'w, 's>,
        plans: Query<'w, 's, (Entity, &'static GoapStatus, Option<&'static GoapAgent>), Without<LayerParked>>,
        squads: Query<'w, 's, (Entity, &'static AiSquad), Without<LayerParked>>,
        members: MemberQuery<'w, 's>,
        formations: Query<'w, 's, (Entity, &'static AiFormation), Without<LayerParked>>,
        moves: Query<'w, 's, (Entity, &'static MoveTo), Without<LayerParked>>,
        navs: Query<'w, 's, (Entity, &'static NavAgentState), Without<LayerParked>>,
        boards: Query<'w, 's, (Entity, &'static Blackboard), Without<LayerParked>>,
    }

    struct FilterView {
        filter: AiDebugFilter,
        selected: Option<Entity>,
        focus: Vec3,
        radius: f32,
    }

    impl FilterView {
        fn passes(&self, poses: &PoseQuery, entity: Entity) -> bool {
            match self.filter {
                AiDebugFilter::All => true,
                AiDebugFilter::Selected => self.selected == Some(entity),
                AiDebugFilter::Radius => {
                    entity_pose(poses, entity).is_some_and(|pos| (pos - self.focus).length() <= self.radius)
                }
            }
        }
    }

    struct Pen<'a> {
        buffer: &'a mut GizmoBuffer<AiDebugGizmos>,
        left: u32,
        dropped: u32,
    }

    impl Pen<'_> {
        fn label(&mut self, origin: Vec3, stack: u32, text: &str, color: Color) {
            if text.is_empty() {
                return;
            }
            if self.left == 0 {
                self.dropped = self.dropped.saturating_add(1);
                return;
            }
            self.left -= 1;
            self.buffer
                .text(label_anchor(origin, stack), text, LABEL_SIZE, LABEL_ANCHOR, color);
        }

        fn sections(&mut self, origin: Vec3, stack: u32, sections: &[(&str, Color)]) {
            if sections.iter().all(|(text, _)| text.is_empty()) {
                return;
            }
            if self.left == 0 {
                self.dropped = self.dropped.saturating_add(1);
                return;
            }
            self.left -= 1;
            self.buffer
                .text_sections(label_anchor(origin, stack), sections, LABEL_SIZE, LABEL_ANCHOR);
        }

        fn cross(&mut self, pos: Vec3, color: Color) {
            if !pos.is_finite() {
                return;
            }
            self.buffer
                .line(pos + Vec3::X * CROSS, pos - Vec3::X * CROSS, color);
            self.buffer
                .line(pos + Vec3::Z * CROSS, pos - Vec3::Z * CROSS, color);
        }

        fn segment(&mut self, start: Vec3, end: Vec3, color: Color) {
            if !start.is_finite() || !end.is_finite() || start == end {
                return;
            }
            self.buffer.line(start, end, color);
        }
    }

    fn entity_pose(poses: &PoseQuery, entity: Entity) -> Option<Vec3> {
        let (local, global) = poses.get(entity).ok()?;
        let pos = global
            .map(GlobalTransform::translation)
            .unwrap_or(local.translation);
        pos.is_finite().then_some(pos)
    }

    /// `PostUpdate`: one frame of the `ai_debug` overlay. `register` runs it only while that gizmo
    /// group is enabled. Layers that are off are not walked.
    fn draw_ai_debug(
        mut settings: ResMut<AiDebugDraw>,
        mut gizmos: Gizmos<AiDebugGizmos>,
        queries: AiDebugQueries,
    ) {
        if !settings.begin_frame() {
            return;
        }
        let span = tracing::debug_span!("ai_debug_gizmos");
        let _entered = span.enter();
        let layers = settings.layers;
        let view = FilterView {
            filter: settings.filter,
            selected: settings.selected,
            focus: settings
                .selected
                .and_then(|entity| entity_pose(&queries.poses, entity))
                .unwrap_or(settings.focus),
            radius: settings.radius,
        };
        let mut pen = Pen {
            buffer: &mut gizmos,
            left: settings.label_cap,
            dropped: 0,
        };
        if layers.behavior_tree {
            draw_trees(&queries, &view, &mut pen);
        }
        if layers.state_tree {
            draw_states(&queries, &view, &mut pen);
        }
        if layers.goap {
            draw_goap(&queries, &view, &mut pen);
        }
        if layers.squad {
            draw_squads(&queries, &view, &mut pen);
        }
        if layers.formation {
            draw_formations(&queries, &view, &mut pen);
        }
        if layers.move_to {
            draw_moves(&queries, &view, &mut pen);
        }
        if layers.blackboard && !settings.watch_keys.is_empty() {
            draw_boards(&queries, &view, &mut pen, &settings.watch_keys);
        }
        settings.dropped = pen.dropped;
    }

    fn draw_trees(queries: &AiDebugQueries, view: &FilterView, pen: &mut Pen<'_>) {
        for (entity, tree) in &queries.trees {
            if !view.passes(&queries.poses, entity) {
                continue;
            }
            let Some(name) = active_leaf_name(tree) else {
                continue;
            };
            let Some(pos) = entity_pose(&queries.poses, entity) else {
                continue;
            };
            pen.label(pos, STACK_BT, name, BT_COLOR);
        }
    }

    fn draw_states(queries: &AiDebugQueries, view: &FilterView, pen: &mut Pen<'_>) {
        for (entity, tree, status) in &queries.states {
            paint_state(&queries.poses, view, pen, entity, tree, Some(status));
        }
        for (entity, tree) in &queries.state_only {
            paint_state(&queries.poses, view, pen, entity, Some(tree), None);
        }
    }

    fn paint_state(
        poses: &PoseQuery,
        view: &FilterView,
        pen: &mut Pen<'_>,
        entity: Entity,
        tree: Option<&StateTree>,
        status: Option<&StateTreeStatus>,
    ) {
        if !view.passes(poses, entity) {
            return;
        }
        let Some(pos) = entity_pose(poses, entity) else {
            return;
        };
        pen.cross(pos, STATE_COLOR);
        let name = if let Some(tree) = tree {
            tree.active_leaf()
        } else {
            status.and_then(|status| status.active_path.last().map(String::as_str))
        };
        if let Some(name) = name {
            pen.label(pos, STACK_STATE, name, STATE_COLOR);
        }
    }

    fn draw_goap(queries: &AiDebugQueries, view: &FilterView, pen: &mut Pen<'_>) {
        for (entity, status, agent) in &queries.plans {
            if !view.passes(&queries.poses, entity) {
                continue;
            }
            let Some(pos) = entity_pose(&queries.poses, entity) else {
                continue;
            };
            pen.cross(pos, GOAP_COLOR);
            if let Some(step) = status.steps.get(status.next_step as usize) {
                pen.label(pos, STACK_GOAP, step, GOAP_COLOR);
            }
            if let Some(agent) = agent {
                paint_goal(pen, pos, agent);
            }
        }
    }

    fn paint_goal(pen: &mut Pen<'_>, pos: Vec3, agent: &GoapAgent) {
        let conditions = agent.goal().bool_conditions();
        let mut stack = STACK_GOAL;
        for (index, name) in agent.facts().bool_names().iter().enumerate() {
            let Some(value) = conditions.get(index) else {
                continue;
            };
            let text = if value { "true" } else { "false" };
            pen.sections(
                pos,
                stack,
                &[(name.as_str(), GOAP_COLOR), ("=", GOAP_COLOR), (text, GOAP_COLOR)],
            );
            stack = stack.saturating_add(1);
        }
    }

    fn draw_squads(queries: &AiDebugQueries, view: &FilterView, pen: &mut Pen<'_>) {
        for (entity, squad) in &queries.squads {
            if !squad_passes(&queries.poses, view, entity, squad) {
                continue;
            }
            if let Some(leader) = squad.leader
                && let Some(leader_pos) = entity_pose(&queries.poses, leader)
            {
                for member in &squad.members {
                    if *member == leader {
                        continue;
                    }
                    if let Some(member_pos) = entity_pose(&queries.poses, *member) {
                        pen.segment(leader_pos, member_pos, SQUAD_COLOR);
                    }
                }
            }
            for member in &squad.members {
                let Some((bytes, len)) = role_glyph(&queries.members, squad, *member) else {
                    continue;
                };
                let Ok(text) = std::str::from_utf8(&bytes[..len]) else {
                    continue;
                };
                let Some(pos) = entity_pose(&queries.poses, *member) else {
                    continue;
                };
                pen.label(pos, STACK_ROLE, text, SQUAD_COLOR);
            }
        }
    }

    fn squad_passes(poses: &PoseQuery, view: &FilterView, entity: Entity, squad: &AiSquad) -> bool {
        if view.passes(poses, entity) {
            return true;
        }
        if squad.leader.is_some_and(|leader| view.passes(poses, leader)) {
            return true;
        }
        squad.members.iter().any(|member| view.passes(poses, *member))
    }

    fn role_glyph(members: &MemberQuery, squad: &AiSquad, entity: Entity) -> Option<([u8; 4], usize)> {
        let from_member = members
            .get(entity)
            .ok()
            .and_then(|member| member.role.as_deref())
            .filter(|role| !role.is_empty())
            .and_then(glyph_bytes);
        if from_member.is_some() {
            return from_member;
        }
        squad
            .roles
            .iter()
            .find(|role| role.holders.contains(&entity))
            .and_then(|role| glyph_bytes(&role.name))
    }

    fn glyph_bytes(name: &str) -> Option<([u8; 4], usize)> {
        let ch = name.chars().find(|ch| ch.is_ascii_alphanumeric())?;
        let mut bytes = [0u8; 4];
        let len = ch.to_ascii_uppercase().encode_utf8(&mut bytes).len();
        Some((bytes, len))
    }

    fn draw_formations(queries: &AiDebugQueries, view: &FilterView, pen: &mut Pen<'_>) {
        for (entity, formation) in &queries.formations {
            if !formation_passes(&queries.poses, view, entity, formation) {
                continue;
            }
            for slot in &formation.slots {
                pen.cross(slot.position, FORMATION_COLOR);
                if let Some(member_pos) = entity_pose(&queries.poses, slot.member) {
                    pen.segment(slot.position, member_pos, FORMATION_COLOR);
                }
            }
        }
    }

    fn formation_passes(
        poses: &PoseQuery,
        view: &FilterView,
        entity: Entity,
        formation: &AiFormation,
    ) -> bool {
        if view.passes(poses, entity) {
            return true;
        }
        formation.slots.iter().any(|slot| {
            view.passes(poses, slot.member)
                || (view.filter == AiDebugFilter::Radius
                    && slot.position.is_finite()
                    && (slot.position - view.focus).length() <= view.radius)
        })
    }

    fn draw_moves(queries: &AiDebugQueries, view: &FilterView, pen: &mut Pen<'_>) {
        for (entity, move_to) in &queries.moves {
            let nav = queries.navs.get(entity).ok().map(|(_, nav)| nav);
            paint_move(&queries.poses, view, pen, entity, Some(move_to), nav);
        }
        for (entity, nav) in &queries.navs {
            if queries.moves.get(entity).is_ok() {
                continue;
            }
            paint_move(&queries.poses, view, pen, entity, None, Some(nav));
        }
    }

    fn paint_move(
        poses: &PoseQuery,
        view: &FilterView,
        pen: &mut Pen<'_>,
        entity: Entity,
        move_to: Option<&MoveTo>,
        nav: Option<&NavAgentState>,
    ) {
        if !view.passes(poses, entity) {
            return;
        }
        let origin = entity_pose(poses, entity);
        if let Some(move_to) = move_to {
            if let Some(origin) = origin {
                pen.segment(origin, move_to.target, MOVE_COLOR);
            }
            pen.cross(move_to.target, MOVE_COLOR);
        }
        let Some(nav) = nav else {
            return;
        };
        let already = move_to.map(|move_to| move_to.target);
        if let Some(target) = nav.target
            && already != Some(target)
        {
            pen.cross(target, MOVE_COLOR);
        }
        let start = nav.next_waypoint.min(nav.waypoints.len());
        let mut finite = 0u32;
        for point in &nav.waypoints[start..] {
            if point.is_finite() {
                finite += 1;
                if finite >= 2 {
                    break;
                }
            }
        }
        if finite >= 2 {
            pen.buffer.linestrip(
                nav.waypoints[start..]
                    .iter()
                    .copied()
                    .filter(|point| point.is_finite()),
                MOVE_COLOR,
            );
        }
    }

    fn draw_boards(queries: &AiDebugQueries, view: &FilterView, pen: &mut Pen<'_>, keys: &[String]) {
        for (entity, board) in &queries.boards {
            if !view.passes(&queries.poses, entity) {
                continue;
            }
            let Some(pos) = entity_pose(&queries.poses, entity) else {
                continue;
            };
            let mut stack = STACK_BLACKBOARD;
            for key in keys {
                let Some(value) = board.get(key) else {
                    continue;
                };
                label_value(pen, pos, stack, key, value);
                stack = stack.saturating_add(1);
            }
        }
    }

    fn label_value(pen: &mut Pen<'_>, pos: Vec3, stack: u32, key: &str, value: &BbValue) {
        match value {
            BbValue::Bool(flag) => {
                let text = if *flag { "true" } else { "false" };
                sections_kv(pen, pos, stack, key, text);
            }
            BbValue::String(text) => sections_kv(pen, pos, stack, key, text),
            BbValue::Int(n) => {
                let mut scratch = String::new();
                let _ = write!(scratch, "{n}");
                sections_kv(pen, pos, stack, key, &scratch);
            }
            BbValue::Float(n) => {
                let mut scratch = String::new();
                let _ = write!(scratch, "{n}");
                sections_kv(pen, pos, stack, key, &scratch);
            }
            BbValue::Vec3(value) => {
                let mut scratch = String::new();
                let _ = write!(scratch, "({}, {}, {})", value.x, value.y, value.z);
                sections_kv(pen, pos, stack, key, &scratch);
            }
            BbValue::Entity(bits) => {
                let mut scratch = String::new();
                let _ = write!(scratch, "{bits}");
                sections_kv(pen, pos, stack, key, &scratch);
            }
        }
    }

    fn sections_kv(pen: &mut Pen<'_>, pos: Vec3, stack: u32, key: &str, value: &str) {
        pen.sections(pos, stack, &[(key, BB_COLOR), ("=", BB_COLOR), (value, BB_COLOR)]);
    }

    /// Registers the overlay on `PostUpdate`, after transforms, while `ai_debug` is enabled.
    pub(super) fn register(app: &mut App) {
        app.add_systems(
            PostUpdate,
            draw_ai_debug
                .run_if(regia_render::gizmos::gizmo_group_enabled::<regia_render::gizmos::AiDebugGizmos>)
                .after(PostUpdateSet::TransformPropagate),
        );
    }
}

/// Registers the in-world overlay on `PostUpdate`, after transforms, while `ai_debug` is enabled.
#[cfg(feature = "debug_draw")]
pub(crate) fn register_draw(app: &mut App) {
    draw::register(app);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BtStatus;
    use crate::task::{BtContext, BtTask};
    use crate::tree::{BehaviorTree, BehaviorTreeBuilder, BtTaskRegistry, DefKind, DefNode};

    struct Hold;

    impl BtTask for Hold {
        fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
            BtStatus::Running
        }
    }

    struct Fail;

    impl BtTask for Fail {
        fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
            BtStatus::Failure
        }
    }

    fn patrol() -> (BtTaskRegistry, crate::tree::BehaviorTreeDef) {
        let mut registry = BtTaskRegistry::default();
        registry.register_factory("test::Hold", || Box::new(Hold));
        registry.register_factory("test::Fail", || Box::new(Fail));
        let mut builder = BehaviorTreeBuilder::new();
        let skip = builder.add(DefNode {
            name: "skip".into(),
            kind: DefKind::Leaf,
            task: "test::Fail".into(),
            ..Default::default()
        });
        let patrol = builder.add(DefNode {
            name: "patrol".into(),
            kind: DefKind::Leaf,
            task: "test::Hold".into(),
            ..Default::default()
        });
        let root = builder.selector("root", vec![skip, patrol]);
        (registry, builder.build(root))
    }

    #[test]
    fn active_leaf_follows_the_running_selector_child() {
        let (registry, def) = patrol();
        let mut tree = BehaviorTree::compile(&def, &registry).expect("compiles");
        assert_eq!(active_leaf_name(&tree), Some("skip"));
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let entity = app.world_mut().spawn_empty().id();
        tree.tick(&mut BtContext {
            world: app.world_mut(),
            entity,
            dt: 1.0 / 60.0,
        });
        assert_eq!(active_leaf_name(&tree), Some("patrol"));
    }

    #[test]
    fn begin_frame_resets_drops_and_stays_off_the_heap() {
        let mut draw = AiDebugDraw {
            dropped: 4,
            ..AiDebugDraw::default()
        };
        assert!(!draw.begin_frame());
        assert_eq!(draw.dropped, 0);
        draw.layers.behavior_tree = true;
        assert!(draw.begin_frame());
        assert_eq!(draw.dropped, 0);
    }

    #[test]
    fn layer_and_filter_names_match_the_protocol() {
        assert!(AiDebugLayers::known("blackboard"));
        assert!(!AiDebugLayers::known("nearby"));
        let mut layers = AiDebugLayers::default();
        assert!(layers.enable("bt"));
        assert!(!layers.enable(""));
        assert!(layers.behavior_tree);
        assert!(!layers.squad);
        assert_eq!(AiDebugFilter::parse("selected"), Some(AiDebugFilter::Selected));
        assert_eq!(AiDebugFilter::parse("nearby"), None);
        assert!(AiDebugLayers::from_flags([false, true, false, false, false, false, false]).state_tree);
    }
}
