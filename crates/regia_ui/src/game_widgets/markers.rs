// SPDX-License-Identifier: MPL-2.0
//! World-anchored markers and target brackets with lead (ADR-2958).
//!
//! A [`MarkerLayer`] node (a full-screen overlay, or a node filling a camera's viewport) shows every
//! [`WorldMarker`] and [`TargetLock`] entity at its projected position, through the same view and
//! projection the frame draws with. A marker can pin to the screen edge along its bearing. A bracket
//! frames its target with four corners sized from `radius_m` and draws a pip at the lead point
//! ([`super::math::lead_solution`]): where to aim so a projectile of the given speed meets the
//! target. The target's velocity is [`TargetLock::velocity`] or estimated from its motion.
//!
//! A [`WorldMarker`] with a [`WorldPlate`] instead gets its own world-space root
//! ([`crate::world_target`]): a billboarded, depth-tested, distance-faded name plate that scene
//! geometry can hide, for things that should read as being in the world.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::glam::{Affine3A, Quat, Vec3};
use regia_render::{Camera, RenderTarget};
use regia_scene::{GlobalTransform, Transform};

use super::math::{
    ScreenAnchor, ViewProjection, anchor_on_layer, lead_solution, projected_radius, smooth_toward,
};
use super::{
    HudFocus, label_span, line, put_state, set_if_changed, set_shown, spawn_child, take_state, text_style,
    theme_colors,
};
use crate::color::UiColor;
use crate::node::{
    Background, BackgroundKind, Border, ComputedNode, NodeName, NodeStyle, Opacity, PickThrough, UiImage,
    UiNode, UiRoot, UiTarget,
};
use crate::style::{Align, Dimension, Edges, FlexDirection, JustifyContent, PositionType, Style};
use crate::text::{Text, TextSpan, WrapMode};
use crate::tween::UiTransform;
use crate::world_target::{Billboard, WorldPanel, fade_factor};

/// Most markers one layer shows; the nearest win.
pub const MAX_LAYER_MARKERS: usize = 256;
/// Most brackets one layer shows; the nearest win.
pub const MAX_LAYER_BRACKETS: usize = 32;
/// Time constant of a target's estimated velocity, in seconds: long enough to calm interpolation
/// jitter, short enough that the pip follows a turn within a few frames.
pub const VELOCITY_SMOOTHING_S: f32 = 0.12;

/// The overlay that shows markers and brackets. Put it on a node that fills the screen, or the
/// camera's viewport.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct MarkerLayer {
    /// Camera to project through. `None`: the root's camera for a camera root, else the active
    /// window camera (the lowest `order`, then entity).
    pub camera: Option<Entity>,
    /// Which markers show: `marker.layer & layer_mask != 0`.
    pub layer_mask: u32,
    /// Distance kept from the layer's edge by pinned markers, in UI px.
    pub edge_margin: f32,
    /// Most markers shown (at most [`MAX_LAYER_MARKERS`]).
    pub max_markers: usize,
}

impl Default for MarkerLayer {
    fn default() -> Self {
        Self {
            camera: None,
            layer_mask: u32::MAX,
            edge_margin: 24.0,
            max_markers: MAX_LAYER_MARKERS,
        }
    }
}

/// A world-space name plate instead of an overlay marker (on [`WorldMarker::plate`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldPlate {
    /// Panel size in UI px.
    pub size: [f32; 2],
    /// Panel pixels per metre.
    pub ppm: f32,
    /// Distance fade `[start, end]` in metres.
    pub fade: Option<[f32; 2]>,
    /// Hide the parts behind scene geometry.
    pub depth_test: bool,
}

impl Default for WorldPlate {
    fn default() -> Self {
        Self {
            size: [256.0, 64.0],
            ppm: 128.0,
            fade: Some([40.0, 60.0]),
            depth_test: true,
        }
    }
}

/// Put this on a world entity (with a `GlobalTransform`) to mark it on every [`MarkerLayer`].
#[derive(Component, Debug, Clone, PartialEq)]
pub struct WorldMarker {
    /// Text beside the marker (`"loc:<key>"` for a localization key). May be empty.
    pub label: String,
    /// An image instead of the dot.
    pub icon: Option<u64>,
    /// Dot or image tint color, and label color.
    pub color: UiColor,
    /// Dot or image size in UI px.
    pub size_px: f32,
    /// World offset from the entity, in metres (above its head).
    pub offset: [f32; 3],
    /// An off-screen marker pins to the edge (else it disappears).
    pub clamp_to_edge: bool,
    /// Add the distance in metres to the label.
    pub show_distance: bool,
    /// Distance fade `[start, end]` in metres.
    pub fade: Option<[f32; 2]>,
    /// Layer bit, matched against [`MarkerLayer::layer_mask`].
    pub layer: u32,
    /// Draw as a world-space plate instead ([`WorldPlate`]); layers skip it.
    pub plate: Option<WorldPlate>,
}

impl Default for WorldMarker {
    fn default() -> Self {
        Self {
            label: String::new(),
            icon: None,
            color: UiColor::WHITE,
            size_px: 12.0,
            offset: [0.0, 0.0, 0.0],
            clamp_to_edge: false,
            show_distance: false,
            fade: None,
            layer: 1,
            plate: None,
        }
    }
}

impl WorldMarker {
    /// A labeled marker of `color`.
    #[must_use]
    pub fn labeled(label: impl Into<String>, color: UiColor) -> Self {
        Self {
            label: label.into(),
            color,
            ..Self::default()
        }
    }
}

/// Put this on a world entity (with a `GlobalTransform`) to bracket it on every [`MarkerLayer`].
#[derive(Component, Debug, Clone, PartialEq)]
pub struct TargetLock {
    /// Radius of the target in metres: the bracket frames a sphere this big.
    pub radius_m: f32,
    /// Smallest half-size of the bracket in UI px, so a distant target stays framed.
    pub min_px: f32,
    /// Bracket and pip color.
    pub color: UiColor,
    /// Text under the bracket (`"loc:<key>"` for a key). May be empty.
    pub label: String,
    /// Add the distance in metres to the label.
    pub show_distance: bool,
    /// Muzzle speed in m/s; `None` draws no lead pip.
    pub projectile_speed: Option<f32>,
    /// Downward acceleration of the projectile in m/s² (`0`: flies flat).
    pub gravity: f32,
    /// The target's velocity; `None` estimates it from its motion.
    pub velocity: Option<[f32; 3]>,
    /// Who shoots. `None`: the [`HudFocus`] entity, else the camera.
    pub shooter: Option<Entity>,
    /// The shooter's velocity; `None` estimates it from its motion.
    pub shooter_velocity: Option<[f32; 3]>,
    /// Layer bit, matched against [`MarkerLayer::layer_mask`].
    pub layer: u32,
}

impl Default for TargetLock {
    fn default() -> Self {
        Self {
            radius_m: 1.0,
            min_px: 14.0,
            color: UiColor::from_hex("#ff5a4f").unwrap_or(UiColor::WHITE),
            label: String::new(),
            show_distance: true,
            projectile_speed: None,
            gravity: 0.0,
            velocity: None,
            shooter: None,
            shooter_velocity: None,
            layer: 1,
        }
    }
}

/// One marker as last placed (`ui.world_markers`).
#[derive(Debug, Clone, PartialEq)]
pub struct MarkerReadout {
    /// The world entity.
    pub source: Entity,
    /// Its node.
    pub node: Entity,
    /// Where it shows, UI px on the layer.
    pub pos: [f32; 2],
    /// The point is on screen.
    pub on_screen: bool,
    /// It is pinned to the edge.
    pub clamped: bool,
    /// The point is behind the camera.
    pub behind: bool,
    /// Metres from the eye.
    pub distance: f32,
    /// Opacity after the fade.
    pub alpha: f32,
    /// It draws this frame.
    pub visible: bool,
}

/// One bracket as last placed (`ui.target_bracket`).
#[derive(Debug, Clone, PartialEq)]
pub struct BracketReadout {
    /// The world entity.
    pub target: Entity,
    /// Its node.
    pub node: Entity,
    /// The target's center on the layer, UI px.
    pub center: [f32; 2],
    /// Half-size of the bracket in UI px.
    pub half_px: f32,
    /// The bracket draws (the target is in view).
    pub on_screen: bool,
    /// Metres from the eye.
    pub distance: f32,
    /// The target's velocity used for the lead.
    pub velocity: [f32; 3],
    /// World point to aim at, when a lead is asked for and exists.
    pub lead_world: Option<[f32; 3]>,
    /// Where the pip shows on the layer, when it does.
    pub lead_screen: Option<[f32; 2]>,
    /// Flight time to the intercept in seconds.
    pub time_to_intercept: Option<f32>,
    /// A lead is asked for and the projectile cannot catch the target.
    pub no_solution: bool,
}

/// Runtime state of a [`MarkerLayer`], kept on its node.
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct MarkerLayerState {
    /// The markers placed last frame, by entity bits.
    pub markers: Vec<MarkerReadout>,
    /// The brackets placed last frame, by entity bits.
    pub brackets: Vec<BracketReadout>,
    /// The camera used last frame.
    pub camera: Option<Entity>,
    /// The layer's size in UI px.
    pub size: [f32; 2],
    marker_slots: BTreeMap<u64, MarkerSlot>,
    bracket_slots: BTreeMap<u64, BracketSlot>,
}

#[derive(Debug, Clone, PartialEq)]
struct MarkerSlot {
    node: Entity,
    dot: Entity,
    label: Entity,
}

#[derive(Debug, Clone, PartialEq)]
struct BracketSlot {
    node: Entity,
    /// Top-left, top-right, bottom-right, bottom-left.
    corners: [Entity; 4],
    pip: Entity,
    label: Entity,
    last_position: Option<Vec3>,
    velocity: Vec3,
    shooter_last: Option<Vec3>,
    shooter_velocity: Vec3,
}

/// Spawns a full-size [`MarkerLayer`] under `parent`.
pub fn spawn_marker_layer(commands: &mut Commands, parent: Entity, layer: MarkerLayer) -> Entity {
    let node = commands
        .spawn((
            UiNode,
            NodeName("marker_layer".to_owned()),
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(0.0),
                    top: Dimension::Points(0.0),
                    ..Edges::default()
                },
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
            PickThrough,
            layer,
        ))
        .id();
    commands.entity(parent).add_child(node);
    node
}

fn anchored_style() -> NodeStyle {
    NodeStyle::new(Style {
        position: PositionType::Absolute,
        inset: Edges {
            left: Dimension::Points(0.0),
            top: Dimension::Points(0.0),
            ..Edges::default()
        },
        ..Default::default()
    })
}

fn place(world: &mut World, node: Entity, translate: [f32; 2]) {
    if let Some(mut t) = world.get_mut::<UiTransform>(node)
        && t.translate != translate
    {
        t.translate = translate;
    }
}

fn root_camera(world: &World, node: Entity) -> Option<Entity> {
    let mut at = node;
    loop {
        if let Some(root) = world.get::<UiRoot>(at) {
            return match root.target {
                UiTarget::Camera(camera) => Some(camera),
                _ => None,
            };
        }
        at = world.get::<ChildOf>(at)?.parent();
    }
}

/// The camera a layer projects through (see [`MarkerLayer::camera`]).
fn layer_camera(world: &mut World, node: Entity, layer: &MarkerLayer) -> Option<Entity> {
    if let Some(camera) = layer.camera.filter(|c| world.get::<Camera>(*c).is_some()) {
        return Some(camera);
    }
    if let Some(camera) = root_camera(world, node) {
        return Some(camera);
    }
    world
        .query::<(Entity, &Camera)>()
        .iter(world)
        .filter(|(_, c)| matches!(c.target, RenderTarget::Window))
        .min_by_key(|(e, c)| (c.order, e.to_bits()))
        .map(|(e, _)| e)
}

fn spawn_marker_slot(world: &mut World, layer: Entity, source: Entity) -> MarkerSlot {
    let node = spawn_child(
        world,
        layer,
        (
            NodeName(format!("marker.{}", source.to_bits())),
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(0.0),
                    top: Dimension::Points(0.0),
                    ..Edges::default()
                },
                direction: FlexDirection::Column,
                align_items: Align::Center,
                gap: [Dimension::Points(2.0), Dimension::Points(2.0)],
                ..Default::default()
            }),
            UiTransform::default(),
            PickThrough,
        ),
    );
    let dot = spawn_child(world, node, (NodeStyle::default(), PickThrough));
    let label = spawn_child(
        world,
        node,
        (
            Text {
                wrap: WrapMode::None,
                ..Default::default()
            },
            PickThrough,
        ),
    );
    MarkerSlot { node, dot, label }
}

fn marker_text(marker: &WorldMarker, distance: f32) -> Text {
    let style = text_style(13.0, marker.color, true);
    let mut spans: Vec<TextSpan> = Vec::new();
    if !marker.label.is_empty() {
        spans.push(label_span(&marker.label, style.clone()));
    }
    if marker.show_distance && distance.is_finite() {
        let sep = if spans.is_empty() { "" } else { "  " };
        spans.push(TextSpan::lit(format!("{sep}{distance:.0} m")).with_style(style));
    }
    Text {
        spans,
        wrap: WrapMode::None,
        ..Default::default()
    }
}

fn style_marker_dot(world: &mut World, dot: Entity, marker: &WorldMarker, px: f32) {
    if let Some(mut style) = world.get_mut::<NodeStyle>(dot) {
        let size = [Dimension::Points(px), Dimension::Points(px)];
        if style.0.size != size {
            style.0.size = size;
        }
    }
    match marker.icon {
        Some(id) => {
            set_if_changed(
                world,
                dot,
                UiImage {
                    id,
                    tint: marker.color,
                    ..Default::default()
                },
            );
            if world.get::<Background>(dot).is_some() {
                world.entity_mut(dot).remove::<(Background, Border)>();
            }
        }
        None => {
            set_if_changed(
                world,
                dot,
                Background {
                    kind: BackgroundKind::Color(marker.color),
                },
            );
            set_if_changed(
                world,
                dot,
                Border {
                    color: UiColor([0.0, 0.0, 0.0, 0.66]),
                    widths: [1.0; 4],
                    radius: [px * 0.5; 4],
                },
            );
            if world.get::<UiImage>(dot).is_some() {
                world.entity_mut(dot).remove::<UiImage>();
            }
        }
    }
}

fn spawn_bracket_slot(world: &mut World, layer: Entity, target: Entity) -> BracketSlot {
    let node = spawn_child(
        world,
        layer,
        (
            NodeName(format!("bracket.{}", target.to_bits())),
            // The layer's whole area: everything in it is placed by hand, and the draw pass skips
            // what is under a node with no area.
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..anchored_style().0
            }),
            UiTransform::default(),
            PickThrough,
        ),
    );
    let corner = |world: &mut World, name: &str| {
        spawn_child(
            world,
            node,
            (
                NodeName(format!("bracket.{}.{name}", target.to_bits())),
                NodeStyle::new(Style {
                    position: PositionType::Absolute,
                    inset: Edges {
                        left: Dimension::Points(0.0),
                        top: Dimension::Points(0.0),
                        ..Edges::default()
                    },
                    size: [Dimension::Points(CORNER_PX), Dimension::Points(CORNER_PX)],
                    ..Default::default()
                }),
                UiTransform::default(),
                PickThrough,
            ),
        )
    };
    let corners = [
        corner(world, "tl"),
        corner(world, "tr"),
        corner(world, "br"),
        corner(world, "bl"),
    ];
    let pip = spawn_child(
        world,
        node,
        (
            NodeName(format!("bracket.{}.pip", target.to_bits())),
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(0.0),
                    top: Dimension::Points(0.0),
                    ..Edges::default()
                },
                size: [Dimension::Points(PIP_PX), Dimension::Points(PIP_PX)],
                ..Default::default()
            }),
            UiTransform::default(),
            PickThrough,
        ),
    );
    let label = spawn_child(
        world,
        node,
        (
            anchored_style(),
            Text {
                wrap: WrapMode::None,
                ..Default::default()
            },
            UiTransform::default(),
            PickThrough,
        ),
    );
    BracketSlot {
        node,
        corners,
        pip,
        label,
        last_position: None,
        velocity: Vec3::ZERO,
        shooter_last: None,
        shooter_velocity: Vec3::ZERO,
    }
}

/// Length of a bracket corner's arms in UI px.
const CORNER_PX: f32 = 12.0;
/// Width of a bracket corner's arms in UI px.
const CORNER_WIDTH: f32 = 2.0;
/// Diameter of the lead pip in UI px.
const PIP_PX: f32 = 12.0;

fn lock_text(lock: &TargetLock, distance: f32) -> Text {
    let style = text_style(12.0, lock.color, true);
    let mut spans: Vec<TextSpan> = Vec::new();
    if !lock.label.is_empty() {
        spans.push(label_span(&lock.label, style.clone()));
    }
    if lock.show_distance && distance.is_finite() {
        let sep = if spans.is_empty() { "" } else { "  " };
        spans.push(TextSpan::lit(format!("{sep}{distance:.0} m")).with_style(style));
    }
    Text {
        spans,
        wrap: WrapMode::None,
        ..Default::default()
    }
}

fn despawn(world: &mut World, node: Entity) {
    if let Ok(e) = world.get_entity_mut(node) {
        e.despawn();
    }
}

fn world_position(world: &World, entity: Entity) -> Option<Vec3> {
    world
        .get::<GlobalTransform>(entity)
        .map(|g| Vec3::from(g.0.translation))
        .filter(|p| p.is_finite())
}

/// `PostUpdate`, before layout: projects every marker and bracket onto every layer.
pub fn marker_layers_update(world: &mut World) {
    let _span = tracing::trace_span!("ui_marker_layers").entered();
    let mut layers: Vec<Entity> = world
        .query_filtered::<Entity, With<MarkerLayer>>()
        .iter(world)
        .collect();
    layers.sort_by_key(|e| e.to_bits());
    let markers: Vec<(Entity, WorldMarker, Vec3)> = world
        .query::<(Entity, &WorldMarker, &GlobalTransform)>()
        .iter(world)
        .filter(|(_, m, _)| m.plate.is_none())
        .map(|(e, m, g)| (e, m.clone(), Vec3::from(g.0.translation)))
        .filter(|(_, _, p)| p.is_finite())
        .collect();
    let locks: Vec<(Entity, TargetLock, Vec3)> = world
        .query::<(Entity, &TargetLock, &GlobalTransform)>()
        .iter(world)
        .map(|(e, l, g)| (e, l.clone(), Vec3::from(g.0.translation)))
        .filter(|(_, _, p)| p.is_finite())
        .collect();
    let focus = world
        .query_filtered::<Entity, With<HudFocus>>()
        .iter(world)
        .min_by_key(|e| e.to_bits());
    let dt = world.get_resource::<Time>().map_or(0.0, Time::delta_secs);
    let colors = theme_colors(world);
    for layer_entity in layers {
        let Some(layer) = world.get::<MarkerLayer>(layer_entity).cloned() else {
            continue;
        };
        let mut state: MarkerLayerState = take_state(world, layer_entity);
        let computed = world
            .get::<ComputedNode>(layer_entity)
            .copied()
            .unwrap_or_default();
        let scale = if computed.scale > 0.0 { computed.scale } else { 1.0 };
        let size = [computed.rect[2] / scale, computed.rect[3] / scale];
        state.size = size;
        let camera = layer_camera(world, layer_entity, &layer);
        state.camera = camera;
        let view = camera.and_then(|c| {
            let cam = world.get::<Camera>(c)?;
            let transform = world.get::<GlobalTransform>(c)?;
            Some(ViewProjection::new(cam, &transform.0, size[0] / size[1].max(1.0)))
        });
        update_markers(
            world,
            layer_entity,
            &layer,
            &mut state,
            &markers,
            view.as_ref(),
            size,
        );
        update_brackets(
            world,
            layer_entity,
            &layer,
            &mut state,
            &locks,
            view.as_ref(),
            size,
            focus,
            dt,
            &colors,
        );
        put_state(world, layer_entity, state);
    }
}

fn update_markers(
    world: &mut World,
    layer_entity: Entity,
    layer: &MarkerLayer,
    state: &mut MarkerLayerState,
    markers: &[(Entity, WorldMarker, Vec3)],
    view: Option<&ViewProjection>,
    size: [f32; 2],
) {
    // The nearest `max_markers`, ties by entity.
    let eye = view.map_or(Vec3::ZERO, |v| v.eye);
    let mut wanted: Vec<(&Entity, &WorldMarker, Vec3)> = markers
        .iter()
        .filter(|(_, m, _)| m.layer & layer.layer_mask != 0)
        .map(|(e, m, p)| (e, m, *p + Vec3::from_array(m.offset)))
        .collect();
    wanted.sort_by(|a, b| {
        a.2.distance_squared(eye)
            .total_cmp(&b.2.distance_squared(eye))
            .then(a.0.to_bits().cmp(&b.0.to_bits()))
    });
    wanted.truncate(layer.max_markers.min(MAX_LAYER_MARKERS));
    let keep: Vec<u64> = wanted.iter().map(|(e, ..)| e.to_bits()).collect();
    let gone: Vec<u64> = state
        .marker_slots
        .keys()
        .filter(|k| !keep.contains(k))
        .copied()
        .collect();
    for key in gone {
        if let Some(slot) = state.marker_slots.remove(&key) {
            despawn(world, slot.node);
        }
    }
    state.markers.clear();
    let margin = if layer.edge_margin.is_finite() {
        layer.edge_margin.max(0.0)
    } else {
        0.0
    };
    for (source, marker, point) in wanted {
        let key = source.to_bits();
        let slot = state
            .marker_slots
            .entry(key)
            .or_insert_with(|| spawn_marker_slot(world, layer_entity, *source))
            .clone();
        let anchor = view.and_then(|v| anchor_on_layer(v, point, size, margin, marker.clamp_to_edge));
        let alpha = match (marker.fade, anchor) {
            (Some([start, end]), Some(a)) => fade_factor(a.distance, start, end),
            _ => 1.0,
        };
        let px = if marker.size_px.is_finite() {
            marker.size_px.clamp(1.0, 256.0)
        } else {
            12.0
        };
        let shown = anchor.is_some_and(|a| a.on_screen || a.clamped) && alpha > 0.0;
        set_shown(world, slot.node, shown);
        let distance = anchor.map_or(f32::NAN, |a| a.distance);
        if shown {
            style_marker_dot(world, slot.dot, marker, px);
            set_if_changed(world, slot.label, marker_text(marker, distance));
            // Centered on its dot. The width is the laid-out one: the label's text sets it, so a new
            // marker waits one frame unseen rather than flash beside its place.
            let computed = world.get::<ComputedNode>(slot.node).copied().unwrap_or_default();
            let scale = if computed.scale > 0.0 { computed.scale } else { 1.0 };
            let width = computed.rect[2] / scale;
            set_if_changed(world, slot.node, Opacity(if width > 0.0 { alpha } else { 0.0 }));
            if let Some(a) = anchor {
                place(world, slot.node, [a.pos[0] - width * 0.5, a.pos[1] - px * 0.5]);
            }
        }
        state.markers.push(MarkerReadout {
            source: *source,
            node: slot.node,
            pos: anchor.map_or([0.0, 0.0], |a| a.pos),
            on_screen: anchor.is_some_and(|a| a.on_screen),
            clamped: anchor.is_some_and(|a| a.clamped),
            behind: anchor.is_some_and(|a| a.behind),
            distance: if distance.is_finite() { distance } else { 0.0 },
            alpha,
            visible: shown,
        });
    }
}

fn update_brackets(
    world: &mut World,
    layer_entity: Entity,
    layer: &MarkerLayer,
    state: &mut MarkerLayerState,
    locks: &[(Entity, TargetLock, Vec3)],
    view: Option<&ViewProjection>,
    size: [f32; 2],
    focus: Option<Entity>,
    dt: f32,
    colors: &crate::theme::ThemeColors,
) {
    let eye = view.map_or(Vec3::ZERO, |v| v.eye);
    let mut wanted: Vec<(&Entity, &TargetLock, Vec3)> = locks
        .iter()
        .filter(|(_, l, _)| l.layer & layer.layer_mask != 0)
        .map(|(e, l, p)| (e, l, *p))
        .collect();
    wanted.sort_by(|a, b| {
        a.2.distance_squared(eye)
            .total_cmp(&b.2.distance_squared(eye))
            .then(a.0.to_bits().cmp(&b.0.to_bits()))
    });
    wanted.truncate(MAX_LAYER_BRACKETS);
    let keep: Vec<u64> = wanted.iter().map(|(e, ..)| e.to_bits()).collect();
    let gone: Vec<u64> = state
        .bracket_slots
        .keys()
        .filter(|k| !keep.contains(k))
        .copied()
        .collect();
    for key in gone {
        if let Some(slot) = state.bracket_slots.remove(&key) {
            despawn(world, slot.node);
        }
    }
    state.brackets.clear();
    for (target, lock, position) in wanted {
        let key = target.to_bits();
        let mut slot = state
            .bracket_slots
            .entry(key)
            .or_insert_with(|| spawn_bracket_slot(world, layer_entity, *target))
            .clone();
        // The velocities: given, or estimated from the motion.
        if dt > 0.0 {
            if let Some(last) = slot.last_position {
                slot.velocity =
                    smooth_toward(slot.velocity, (position - last) / dt, dt, VELOCITY_SMOOTHING_S);
            }
            slot.last_position = Some(position);
        }
        let velocity = lock
            .velocity
            .map(Vec3::from_array)
            .filter(|v| v.is_finite())
            .unwrap_or(slot.velocity);
        let shooter_entity = lock.shooter.or(focus).filter(|e| *e != *target);
        let shooter_position = shooter_entity
            .and_then(|e| world_position(world, e))
            .unwrap_or(eye);
        if dt > 0.0 {
            if let Some(last) = slot.shooter_last {
                slot.shooter_velocity = smooth_toward(
                    slot.shooter_velocity,
                    (shooter_position - last) / dt,
                    dt,
                    VELOCITY_SMOOTHING_S,
                );
            }
            slot.shooter_last = Some(shooter_position);
        }
        let shooter_velocity = lock
            .shooter_velocity
            .map(Vec3::from_array)
            .filter(|v| v.is_finite())
            .unwrap_or(slot.shooter_velocity);
        let solution = lock.projectile_speed.and_then(|speed| {
            lead_solution(
                shooter_position,
                shooter_velocity,
                position,
                velocity,
                speed,
                lock.gravity,
            )
        });
        let anchor: Option<ScreenAnchor> = view.and_then(|v| anchor_on_layer(v, position, size, 0.0, false));
        let on_screen = anchor.is_some_and(|a| a.on_screen);
        let distance = anchor.map_or(0.0, |a| a.distance);
        let half_px = match (view, anchor) {
            (Some(v), Some(a)) => {
                let r = if lock.radius_m.is_finite() {
                    lock.radius_m
                } else {
                    1.0
                };
                projected_radius(v, r, a.depth, size[1]).max(lock.min_px.max(1.0))
            }
            _ => 0.0,
        };
        // The pip shows wherever the aim point is on screen, even with the target off it: the frame
        // and its label hide alone.
        let pip_at = solution
            .and_then(|s| view.and_then(|v| anchor_on_layer(v, s.aim_point, size, 0.0, false)))
            .filter(|p| p.on_screen);
        set_shown(world, slot.node, on_screen || pip_at.is_some());
        for corner in slot.corners {
            set_shown(world, corner, on_screen);
        }
        set_shown(world, slot.label, on_screen);
        if on_screen && let Some(a) = anchor {
            place_bracket(world, &slot, lock, a.pos, half_px, distance);
        }
        set_shown(world, slot.pip, pip_at.is_some());
        let mut lead_screen = None;
        if let Some(p) = pip_at {
            lead_screen = Some(p.pos);
            place(
                world,
                slot.pip,
                [p.pos[0] - PIP_PX * 0.5, p.pos[1] - PIP_PX * 0.5],
            );
            set_if_changed(
                world,
                slot.pip,
                Border {
                    color: lock.color,
                    widths: [2.0; 4],
                    radius: [PIP_PX * 0.5; 4],
                },
            );
            set_if_changed(
                world,
                slot.pip,
                Background {
                    kind: BackgroundKind::Color(colors.background.with_alpha(0.25)),
                },
            );
        }
        state.brackets.push(BracketReadout {
            target: *target,
            node: slot.node,
            center: anchor.map_or([0.0, 0.0], |a| a.pos),
            half_px,
            on_screen,
            distance,
            velocity: velocity.to_array(),
            lead_world: solution.map(|s| s.aim_point.to_array()),
            lead_screen,
            time_to_intercept: solution.map(|s| s.time),
            no_solution: lock.projectile_speed.is_some() && solution.is_none(),
        });
        state.bracket_slots.insert(key, slot);
    }
}

fn place_bracket(
    world: &mut World,
    slot: &BracketSlot,
    lock: &TargetLock,
    center: [f32; 2],
    half: f32,
    distance: f32,
) {
    let [tl, tr, br, bl] = slot.corners;
    let w = CORNER_WIDTH;
    // Each corner is a square node with two border sides on; its outer corner is on the frame.
    let arms: [(Entity, [f32; 2], [f32; 4]); 4] = [
        (tl, [center[0] - half, center[1] - half], [w, w, 0.0, 0.0]),
        (
            tr,
            [center[0] + half - CORNER_PX, center[1] - half],
            [0.0, w, w, 0.0],
        ),
        (
            br,
            [center[0] + half - CORNER_PX, center[1] + half - CORNER_PX],
            [0.0, 0.0, w, w],
        ),
        (
            bl,
            [center[0] - half, center[1] + half - CORNER_PX],
            [w, 0.0, 0.0, w],
        ),
    ];
    for (corner, at, widths) in arms {
        place(world, corner, at);
        set_if_changed(
            world,
            corner,
            Border {
                color: lock.color,
                widths,
                radius: [0.0; 4],
            },
        );
    }
    set_if_changed(world, slot.label, lock_text(lock, distance));
    // Under the frame, centered by its laid-out width.
    let computed = world.get::<ComputedNode>(slot.label).copied().unwrap_or_default();
    let scale = if computed.scale > 0.0 { computed.scale } else { 1.0 };
    place(
        world,
        slot.label,
        [center[0] - computed.rect[2] / scale * 0.5, center[1] + half + 4.0],
    );
}

/// Marks the world-space root that draws a marker's [`WorldPlate`].
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldPlateRoot {
    /// The marked entity.
    pub source: Entity,
    /// The plate's text node.
    pub text: Entity,
}

/// `PostUpdate`, before layout: keeps one world-space root per [`WorldMarker`] with a plate, at the
/// marked entity plus its offset; a root whose marker or plate went away goes too.
pub fn world_plates_update(world: &mut World) {
    let _span = tracing::trace_span!("ui_world_plates").entered();
    let wanted: Vec<(Entity, WorldMarker, Vec3)> = world
        .query::<(Entity, &WorldMarker, &GlobalTransform)>()
        .iter(world)
        .filter(|(_, m, _)| m.plate.is_some())
        .map(|(e, m, g)| (e, m.clone(), Vec3::from(g.0.translation)))
        .collect();
    let roots: Vec<(Entity, WorldPlateRoot)> = world
        .query::<(Entity, &WorldPlateRoot)>()
        .iter(world)
        .map(|(e, r)| (e, *r))
        .collect();
    for (root, plate) in &roots {
        if !wanted.iter().any(|(e, ..)| *e == plate.source) {
            despawn(world, *root);
        }
    }
    let colors = theme_colors(world);
    for (source, marker, position) in wanted {
        let Some(plate) = marker.plate else { continue };
        let at = position + Vec3::from_array(marker.offset);
        if !at.is_finite() {
            continue;
        }
        let existing = roots
            .iter()
            .find(|(_, r)| r.source == source)
            .map(|(e, r)| (*e, *r));
        let (root, text_node) = match existing {
            Some((root, r)) => (root, r.text),
            None => spawn_plate(world, source, &plate, &colors.surface),
        };
        // A changed plate applies in place.
        let target = UiTarget::WorldSpace {
            size: plate.size,
            ppm: plate.ppm,
        };
        if let Some(mut ui_root) = world.get_mut::<UiRoot>(root)
            && ui_root.target != target
        {
            ui_root.target = target;
        }
        if let Some(mut panel) = world.get_mut::<WorldPanel>(root)
            && (panel.fade != plate.fade || panel.depth_test != plate.depth_test)
        {
            panel.fade = plate.fade;
            panel.depth_test = plate.depth_test;
        }
        // No parent: the root's transform is its world transform.
        let transform = Transform::from_translation(at);
        set_if_changed(world, root, transform);
        set_if_changed(
            world,
            root,
            GlobalTransform(Affine3A::from_rotation_translation(Quat::IDENTITY, at)),
        );
        // The distance is the camera's business; a plate's text is its label.
        set_if_changed(
            world,
            text_node,
            line(
                &marker.label,
                crate::world_target::sanitize_panel_size(plate.size)[1] * 0.5,
                marker.color,
                true,
            ),
        );
    }
}

fn spawn_plate(world: &mut World, source: Entity, plate: &WorldPlate, surface: &UiColor) -> (Entity, Entity) {
    let root = world
        .spawn((
            UiRoot::world_space(plate.size, plate.ppm),
            WorldPanel {
                billboard: Billboard::Full,
                pivot: [0.5, 1.0],
                fade: plate.fade,
                depth_test: plate.depth_test,
                ..Default::default()
            },
            Transform::default(),
            GlobalTransform::default(),
            NodeName(format!("plate.{}", source.to_bits())),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(surface.with_alpha(0.7)),
            },
        ))
        .id();
    let text = spawn_child(world, root, Text::default());
    world.entity_mut(root).insert(WorldPlateRoot { source, text });
    (root, text)
}
