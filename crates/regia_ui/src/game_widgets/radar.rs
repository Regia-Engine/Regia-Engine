// SPDX-License-Identifier: MPL-2.0
//! The radar and minimap (moved up from the `ui_gallery` sample, ADR-2958).
//!
//! A [`Radar`] node shows every [`RadarBlip`] entity. [`RadarMode::Radar`] centers on the player
//! with a range in metres and may turn with the player's heading; [`RadarMode::Minimap`] shows a
//! whole `world_rect`, north up. An optional [`RadarMap`] draws an image behind the blips: the
//! whole image in a minimap, a window scrolling with the center in a radar.
//!
//! Blip nodes are pooled children placed by [`UiTransform`]: moving blips cost no layout pass.
//! The nearest [`Radar::max_blips`] are shown, so a crowded match cannot grow the node count.
//! The UI has no rotated image, so a radar with a map stays north up whatever `rotate` says.
//!
//! A [`RadarFog`] component on the radar node adds a fog of war layer (a runtime binding, not a
//! `.ui.ron` field): an RGBA image of the viewer's fog cells ([`radar_fog_rgba`]) drawn over the
//! map and under the blips, as a window of its `world_rect` like the map. The framework's fog
//! presentation (`regia_framework::fog_texture`) keeps the image and the rect up to date.

use std::f32::consts::TAU;

use regia_app::prelude::*;
use regia_core::glam::Vec3;
use regia_scene::GlobalTransform;

use super::math::{RadarFrame, heading_of, map_window, radar_point};
use super::{HudFocus, line, put_state, set_if_changed, set_shown, spawn_child, take_state, theme_colors};
use crate::color::UiColor;
use crate::node::{
    Background, BackgroundKind, Border, ClipChildren, ComputedNode, NodeName, NodeStyle, Opacity,
    PickThrough, UiImage, UiNode,
};
use crate::style::{Dimension, Edges, PositionType, Style};
use crate::tween::UiTransform;

/// Default cap of blips one radar shows.
pub const DEFAULT_MAX_BLIPS: usize = 128;
/// Hard cap of blips one radar shows.
pub const MAX_BLIPS: usize = 1024;

/// What a radar covers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RadarMode {
    /// Centered on the player, `range` metres to the edge.
    Radar {
        /// World metres from the center to the edge.
        range: f32,
    },
    /// A fixed area, north up.
    Minimap {
        /// `[min_x, min_z, max_x, max_z]` in world metres (−Z is north, at the top).
        world_rect: [f32; 4],
    },
}

impl Default for RadarMode {
    fn default() -> Self {
        Self::Radar { range: 50.0 }
    }
}

/// The outline of the radar's area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RadarShape {
    /// A circle inscribed in the node (the usual radar).
    #[default]
    Circle,
    /// The node's rectangle (the usual minimap).
    Rect,
}

impl RadarShape {
    /// `"circle"` or `"rect"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Circle => "circle",
            Self::Rect => "rect",
        }
    }
}

/// An image behind the blips.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadarMap {
    /// Renderer image id ([`crate::render::UiRenderer::queue_image_rgba`]).
    pub image: u64,
    /// `[min_x, min_z, max_x, max_z]` the image covers, in world metres. All zero for a minimap
    /// means "the minimap's `world_rect`".
    pub world_rect: [f32; 4],
}

/// How a [`RadarFog`] layer colours the fog cells (straight alpha; visible cells are clear).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadarFogLook {
    /// Colour of unexplored cells.
    pub unexplored: UiColor,
    /// Colour of explored cells (seen before, not now).
    pub explored: UiColor,
}

impl Default for RadarFogLook {
    fn default() -> Self {
        Self {
            unexplored: UiColor::BLACK.with_alpha(0.94),
            explored: UiColor::BLACK.with_alpha(0.5),
        }
    }
}

/// A fog of war layer on a [`Radar`] (runtime only: put it on the radar node). It draws image
/// [`Self::image`] (a renderer image id, see [`radar_fog_rgba`]) covering [`Self::world_rect`],
/// over the map and under the blips.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct RadarFog {
    /// Renderer image id of the fog image ([`crate::render::UiRenderer::queue_dynamic_image`]).
    pub image: u64,
    /// `[min_x, min_z, max_x, max_z]` the image covers, in world metres. All zero for a minimap
    /// means "the minimap's `world_rect`".
    pub world_rect: [f32; 4],
    /// Colours of the fog cells.
    pub look: RadarFogLook,
}

impl RadarFog {
    /// A fog layer drawing image `image` (the rect is filled in by the fog presentation).
    #[must_use]
    pub fn new(image: u64) -> Self {
        Self {
            image,
            world_rect: [0.0; 4],
            look: RadarFogLook::default(),
        }
    }
}

/// The RGBA8 image (sRGB, straight alpha, row 0 = north, the smallest Z) of a fog texture's
/// `cols` x `rows` texels (0 unexplored, 128 explored, 255 visible, values between while a cell
/// fades): unexplored cells in `look.unexplored`, explored in `look.explored`, visible clear.
/// Missing texels read as unexplored.
#[must_use]
pub fn radar_fog_rgba(cols: usize, rows: usize, texels: &[u8], look: &RadarFogLook) -> Vec<u8> {
    let clear = look.explored.with_alpha(0.0);
    let mut out = Vec::with_capacity(cols * rows * 4);
    for i in 0..cols * rows {
        let v = f32::from(texels.get(i).copied().unwrap_or(0));
        let c = if v < 128.0 {
            look.unexplored.lerp(look.explored, v / 128.0)
        } else {
            look.explored.lerp(clear, (v - 128.0) / 127.0)
        };
        out.extend_from_slice(&[
            crate::color::linear_to_srgb8(c.0[0]),
            crate::color::linear_to_srgb8(c.0[1]),
            crate::color::linear_to_srgb8(c.0[2]),
            (c.0[3].clamp(0.0, 1.0) * 255.0).round() as u8,
        ]);
    }
    out
}

/// A radar or minimap. Spawn it on a sized node, or with [`spawn_radar`].
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Radar {
    /// What it covers.
    pub mode: RadarMode,
    /// Its outline.
    pub shape: RadarShape,
    /// Entity at the center of a radar, which draws no blip of its own there. `None`: the
    /// [`HudFocus`] entity, else [`Self::center`]. A minimap's center is its rect; it only reads
    /// this for the heading it never turns by, and shows the entity's blip like any other.
    pub follow: Option<Entity>,
    /// World `[x, z]` used when nothing is followed.
    pub center: [f32; 2],
    /// A radar turns so the followed entity's heading points up (ignored with a [`Self::map`] and
    /// for a minimap).
    pub rotate: bool,
    /// Which blips show: a blip shows when `blip.layer & layer_mask != 0`.
    pub layer_mask: u32,
    /// Most blips shown; the nearest win (at most [`MAX_BLIPS`]).
    pub max_blips: usize,
    /// An image behind the blips.
    pub map: Option<RadarMap>,
    /// Draw the "N" tick that marks north on the rim.
    pub show_north: bool,
}

impl Default for Radar {
    fn default() -> Self {
        Self {
            mode: RadarMode::default(),
            shape: RadarShape::Circle,
            follow: None,
            center: [0.0, 0.0],
            rotate: false,
            layer_mask: u32::MAX,
            max_blips: DEFAULT_MAX_BLIPS,
            map: None,
            show_north: true,
        }
    }
}

impl Radar {
    /// A radar of `range` metres.
    #[must_use]
    pub fn with_range(range: f32) -> Self {
        Self {
            mode: RadarMode::Radar { range },
            ..Self::default()
        }
    }

    /// A minimap of `world_rect` (`[min_x, min_z, max_x, max_z]`).
    #[must_use]
    pub fn minimap(world_rect: [f32; 4]) -> Self {
        Self {
            mode: RadarMode::Minimap { world_rect },
            shape: RadarShape::Rect,
            ..Self::default()
        }
    }

    /// With a map image.
    #[must_use]
    pub fn with_map(mut self, image: u64, world_rect: [f32; 4]) -> Self {
        self.map = Some(RadarMap { image, world_rect });
        self
    }
}

/// How a blip draws.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BlipShape {
    /// A round dot.
    #[default]
    Dot,
    /// A square.
    Square,
    /// An image tinted by the blip's color.
    Icon(u64),
}

/// Put this on a world entity (with a `GlobalTransform`) to show it on radars.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct RadarBlip {
    /// Color (the image's tint for an icon).
    pub color: UiColor,
    /// How it draws.
    pub shape: BlipShape,
    /// Size in UI px.
    pub size_px: f32,
    /// A blip out of range stays pinned to the rim (else it disappears).
    pub clamp_to_edge: bool,
    /// Layer bit, matched against [`Radar::layer_mask`].
    pub layer: u32,
    /// Shown by agents and the accessibility tree, not drawn.
    pub label: String,
}

impl Default for RadarBlip {
    fn default() -> Self {
        Self {
            color: UiColor::WHITE,
            shape: BlipShape::Dot,
            size_px: 7.0,
            clamp_to_edge: false,
            layer: 1,
            label: String::new(),
        }
    }
}

impl RadarBlip {
    /// A dot of `color`.
    #[must_use]
    pub fn dot(color: UiColor) -> Self {
        Self {
            color,
            ..Self::default()
        }
    }
}

/// One shown blip, as last placed (`ui.radar`).
#[derive(Debug, Clone, PartialEq)]
pub struct RadarBlipReadout {
    /// The world entity.
    pub source: Entity,
    /// Its node.
    pub node: Entity,
    /// Its label.
    pub label: String,
    /// World `[x, z]`.
    pub world: [f32; 2],
    /// Center of the blip on the widget, in UI px from its top-left.
    pub pos: [f32; 2],
    /// Inside the range (false means pinned to the rim).
    pub inside: bool,
}

/// Runtime state of a [`Radar`], kept on its node.
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct RadarState {
    /// The blips drawn last frame, nearest first.
    pub blips: Vec<RadarBlipReadout>,
    /// Blips in the world the radar could not show because of [`Radar::max_blips`].
    pub dropped: usize,
    /// The center used last frame, world `[x, z]`.
    pub center: [f32; 2],
    /// The heading used last frame, radians clockwise from north.
    pub heading: f32,
    /// The widget's size in UI px.
    pub size: [f32; 2],
    pool: Vec<Entity>,
    map_node: Option<Entity>,
    north: Option<Entity>,
    fog_node: Option<Entity>,
}

/// Spawns `radar` on a new node of `size_px` under `parent`.
pub fn spawn_radar(commands: &mut Commands, parent: Entity, radar: Radar, size_px: [f32; 2]) -> Entity {
    let radar_node = commands
        .spawn((
            UiNode,
            NodeName("radar".to_owned()),
            NodeStyle::new(Style {
                size: [Dimension::Points(size_px[0]), Dimension::Points(size_px[1])],
                ..Default::default()
            }),
            radar,
        ))
        .id();
    commands.entity(parent).add_child(radar_node);
    radar_node
}

fn pool_node_style() -> NodeStyle {
    NodeStyle::new(Style {
        position: PositionType::Absolute,
        inset: Edges {
            left: Dimension::Points(0.0),
            top: Dimension::Points(0.0),
            ..Edges::default()
        },
        size: [Dimension::Points(0.0), Dimension::Points(0.0)],
        ..Default::default()
    })
}

fn focus_entity(world: &mut World) -> Option<Entity> {
    world
        .query_filtered::<Entity, With<HudFocus>>()
        .iter(world)
        .min_by_key(|e| e.to_bits())
}

fn world_position(world: &World, entity: Entity) -> Option<Vec3> {
    world
        .get::<GlobalTransform>(entity)
        .map(|g| Vec3::from(g.0.translation))
}

/// `[half_x, half_y]` world metres shown from the center, for a widget of `size` px.
fn ranges_of(radar: &Radar, circular: bool, size: [f32; 2]) -> ([f32; 2], [f32; 2]) {
    match radar.mode {
        RadarMode::Radar { range } => {
            let r = if range.is_finite() && range > 0.0 {
                range
            } else {
                1.0
            };
            // A circle is inscribed in the node; a rect radar keeps metres square.
            if circular {
                ([r, r], [size[0].min(size[1]) * 0.5; 2])
            } else {
                let m = size[0].max(size[1]).max(1.0);
                ([r * size[0] / m, r * size[1] / m], [size[0] * 0.5, size[1] * 0.5])
            }
        }
        RadarMode::Minimap { world_rect } => (
            [
                ((world_rect[2] - world_rect[0]) * 0.5).max(0.0),
                ((world_rect[3] - world_rect[1]) * 0.5).max(0.0),
            ],
            [size[0] * 0.5, size[1] * 0.5],
        ),
    }
}

/// `PostUpdate`, before layout: centers each radar, places its blips, keeps the pool's size.
pub fn radar_update(world: &mut World) {
    let _span = tracing::trace_span!("ui_radar").entered();
    let mut radars: Vec<Entity> = world
        .query_filtered::<Entity, With<Radar>>()
        .iter(world)
        .collect();
    radars.sort_by_key(|e| e.to_bits());
    let sources: Vec<(Entity, RadarBlip, Vec3)> = world
        .query::<(Entity, &RadarBlip, &GlobalTransform)>()
        .iter(world)
        .map(|(e, b, g)| (e, b.clone(), Vec3::from(g.0.translation)))
        .filter(|(_, _, p)| p.is_finite())
        .collect();
    let focus = focus_entity(world);
    let colors = theme_colors(world);
    for entity in radars {
        let Some(radar) = world.get::<Radar>(entity).cloned() else {
            continue;
        };
        let mut state: RadarState = take_state(world, entity);
        if !world.entity(entity).contains::<ClipChildren>() {
            world.entity_mut(entity).insert(ClipChildren);
        }
        let computed = world.get::<ComputedNode>(entity).copied().unwrap_or_default();
        let scale = if computed.scale > 0.0 { computed.scale } else { 1.0 };
        let size = [computed.rect[2] / scale, computed.rect[3] / scale];
        state.size = size;
        // The center and the heading.
        let followed = radar.follow.or(focus);
        let center_world = followed.and_then(|e| world_position(world, e));
        let center = match radar.mode {
            RadarMode::Minimap { world_rect } => [
                (world_rect[0] + world_rect[2]) * 0.5,
                (world_rect[1] + world_rect[3]) * 0.5,
            ],
            RadarMode::Radar { .. } => center_world.map_or(radar.center, |p| [p.x, p.z]),
        };
        let center = center.map(|c| if c.is_finite() { c } else { 0.0 });
        let can_rotate = radar.rotate && radar.map.is_none() && matches!(radar.mode, RadarMode::Radar { .. });
        let heading = if can_rotate {
            followed
                .and_then(|e| world.get::<GlobalTransform>(e))
                .map_or(0.0, |g| heading_of(g.0.to_scale_rotation_translation().1))
        } else {
            0.0
        };
        let circular = radar.shape == RadarShape::Circle;
        let (range, half_px) = ranges_of(&radar, circular, size);
        let frame = RadarFrame {
            center,
            heading,
            range,
            half_px,
            circular,
        };
        // Offset of the inscribed circle inside the node (zero for a rect).
        let origin = [size[0] * 0.5 - half_px[0], size[1] * 0.5 - half_px[1]];
        sync_map(world, entity, &radar, &mut state, center, range);
        sync_fog(world, entity, &radar, &mut state, center, range);
        sync_north(world, entity, &radar, &mut state, &colors, &frame, origin);
        state.center = center;
        state.heading = heading;

        // Pick and place: nearest first, ties by entity so the kept set is deterministic.
        let mut picked: Vec<(f32, Entity, &RadarBlip, Vec3, [f32; 2], bool)> = Vec::new();
        for (source, blip, position) in &sources {
            let at_center = matches!(radar.mode, RadarMode::Radar { .. }) && Some(*source) == followed;
            if blip.layer & radar.layer_mask == 0 || at_center {
                continue;
            }
            let Some(point) = radar_point(&frame, [position.x, position.z], blip.clamp_to_edge) else {
                continue;
            };
            if !point.inside && !blip.clamp_to_edge {
                continue;
            }
            let (dx, dz) = (position.x - center[0], position.z - center[1]);
            picked.push((
                dx * dx + dz * dz,
                *source,
                blip,
                *position,
                point.pos,
                point.inside,
            ));
        }
        picked.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.to_bits().cmp(&b.1.to_bits())));
        let cap = radar.max_blips.min(MAX_BLIPS);
        state.dropped = picked.len().saturating_sub(cap);
        picked.truncate(cap);
        // By entity from here: a blip keeps its node as distances change, so blips of different
        // sizes and colors do not trade styles (and a layout pass) every time they pass each other.
        picked.sort_by_key(|p| p.1.to_bits());
        // Grow the pool; hide what is left over (nodes are kept: blips come and go every frame).
        while state.pool.len() < picked.len() {
            let node = spawn_child(
                world,
                entity,
                (pool_node_style(), UiTransform::default(), PickThrough),
            );
            state.pool.push(node);
        }
        state.blips.clear();
        for (i, (_, source, blip, position, pos, inside)) in picked.iter().enumerate() {
            let node = state.pool[i];
            let at = [pos[0] + origin[0], pos[1] + origin[1]];
            let px = if blip.size_px.is_finite() {
                blip.size_px.clamp(1.0, 256.0)
            } else {
                7.0
            };
            style_blip(world, node, blip, px);
            place(world, node, [at[0] - px * 0.5, at[1] - px * 0.5]);
            state.blips.push(RadarBlipReadout {
                source: *source,
                node,
                label: blip.label.clone(),
                world: [position.x, position.z],
                pos: at,
                inside: *inside,
            });
        }
        for node in state.pool.iter().skip(picked.len()) {
            set_shown(world, *node, false);
        }
        put_state(world, entity, state);
    }
}

fn place(world: &mut World, node: Entity, translate: [f32; 2]) {
    if let Some(mut t) = world.get_mut::<UiTransform>(node)
        && t.translate != translate
    {
        t.translate = translate;
    }
}

fn style_blip(world: &mut World, node: Entity, blip: &RadarBlip, px: f32) {
    set_shown(world, node, true);
    if let Some(mut style) = world.get_mut::<NodeStyle>(node) {
        let size = [Dimension::Points(px), Dimension::Points(px)];
        if style.0.size != size {
            style.0.size = size;
        }
    }
    match blip.shape {
        BlipShape::Dot | BlipShape::Square => {
            let radius = if blip.shape == BlipShape::Dot {
                px * 0.5
            } else {
                0.0
            };
            set_if_changed(
                world,
                node,
                Background {
                    kind: BackgroundKind::Color(blip.color),
                },
            );
            set_if_changed(
                world,
                node,
                Border {
                    color: UiColor::from_hex("#000000aa").unwrap_or(UiColor([0.0, 0.0, 0.0, 0.66])),
                    widths: [1.0; 4],
                    radius: [radius; 4],
                },
            );
            if world.get::<UiImage>(node).is_some() {
                world.entity_mut(node).remove::<UiImage>();
            }
        }
        BlipShape::Icon(id) => {
            set_if_changed(
                world,
                node,
                UiImage {
                    id,
                    tint: blip.color,
                    ..Default::default()
                },
            );
            if world.get::<Background>(node).is_some() {
                world.entity_mut(node).remove::<(Background, Border)>();
            }
        }
    }
}

/// Keeps the map image's node: first child, filling the widget, showing the right part of the image.
fn sync_map(
    world: &mut World,
    entity: Entity,
    radar: &Radar,
    state: &mut RadarState,
    center: [f32; 2],
    range: [f32; 2],
) {
    let Some(map) = radar.map else {
        if let Some(node) = state.map_node.take()
            && let Ok(e) = world.get_entity_mut(node)
        {
            e.despawn();
        }
        return;
    };
    let node = match state.map_node {
        Some(node) if world.get_entity(node).is_ok() => node,
        _ => {
            let node = world
                .spawn((
                    UiNode,
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
                ))
                .id();
            // First child: it paints under whatever the game put inside the radar.
            world.entity_mut(entity).insert_child(0, node);
            state.map_node = Some(node);
            node
        }
    };
    let whole = map.world_rect == [0.0; 4] && matches!(radar.mode, RadarMode::Minimap { .. });
    let uv = if whole {
        [0.0, 0.0, 1.0, 1.0]
    } else {
        map_window(center, range, map.world_rect)
    };
    let radius = match radar.shape {
        RadarShape::Circle => 10_000.0,
        RadarShape::Rect => 0.0,
    };
    set_if_changed(
        world,
        node,
        UiImage {
            id: map.image,
            uv,
            preserve_aspect: false,
            ..Default::default()
        },
    );
    set_if_changed(
        world,
        node,
        Border {
            radius: [radius; 4],
            ..Default::default()
        },
    );
}

/// Keeps the fog layer's node: right above the map node (under the blips), showing the right part
/// of the fog image.
fn sync_fog(
    world: &mut World,
    entity: Entity,
    radar: &Radar,
    state: &mut RadarState,
    center: [f32; 2],
    range: [f32; 2],
) {
    let Some(fog) = world.get::<RadarFog>(entity).cloned() else {
        if let Some(node) = state.fog_node.take()
            && let Ok(e) = world.get_entity_mut(node)
        {
            e.despawn();
        }
        return;
    };
    let node = match state.fog_node {
        Some(node) if world.get_entity(node).is_ok() => node,
        _ => {
            let node = world
                .spawn((
                    UiNode,
                    NodeName("radar_fog".to_owned()),
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
                ))
                .id();
            // Right after the map (which is the first child when there is one), before the blips.
            let at = usize::from(state.map_node.is_some_and(|m| world.get_entity(m).is_ok()));
            world.entity_mut(entity).insert_child(at, node);
            state.fog_node = Some(node);
            node
        }
    };
    let whole = fog.world_rect == [0.0; 4] && matches!(radar.mode, RadarMode::Minimap { .. });
    let uv = if whole {
        [0.0, 0.0, 1.0, 1.0]
    } else {
        map_window(center, range, fog.world_rect)
    };
    let radius = match radar.shape {
        RadarShape::Circle => 10_000.0,
        RadarShape::Rect => 0.0,
    };
    set_if_changed(
        world,
        node,
        UiImage {
            id: fog.image,
            uv,
            preserve_aspect: false,
            ..Default::default()
        },
    );
    set_if_changed(
        world,
        node,
        Border {
            radius: [radius; 4],
            ..Default::default()
        },
    );
}

/// Keeps the "N" tick on the rim at north's bearing.
fn sync_north(
    world: &mut World,
    entity: Entity,
    radar: &Radar,
    state: &mut RadarState,
    colors: &crate::theme::ThemeColors,
    frame: &RadarFrame,
    origin: [f32; 2],
) {
    if !radar.show_north || matches!(radar.mode, RadarMode::Minimap { .. }) {
        if let Some(node) = state.north.take()
            && let Ok(e) = world.get_entity_mut(node)
        {
            e.despawn();
        }
        return;
    }
    let node = match state.north {
        Some(node) if world.get_entity(node).is_ok() => node,
        _ => {
            let node = spawn_child(
                world,
                entity,
                (
                    NodeStyle::new(Style {
                        position: PositionType::Absolute,
                        inset: Edges {
                            left: Dimension::Points(0.0),
                            top: Dimension::Points(0.0),
                            ..Edges::default()
                        },
                        ..Default::default()
                    }),
                    line("N", 12.0, colors.text, true),
                    UiTransform::default(),
                    PickThrough,
                    Opacity(0.9),
                ),
            );
            state.north = Some(node);
            node
        }
    };
    // North sits `-heading` clockwise from the widget's up.
    let angle = (-frame.heading).rem_euclid(TAU);
    let (sin, cos) = angle.sin_cos();
    let reach = [frame.half_px[0] - 9.0, frame.half_px[1] - 9.0];
    let at = [
        origin[0] + frame.half_px[0] + sin * reach[0],
        origin[1] + frame.half_px[1] - cos * reach[1],
    ];
    place(world, node, [at[0] - 4.5, at[1] - 7.0]);
}

/// Invented data for the designer's preview: an empty radar gets a follow-free center and a few
/// blips, as world entities that go away with the preview's app.
pub(crate) fn fill_preview(world: &mut World) {
    let radars: Vec<Entity> = world
        .query_filtered::<Entity, With<Radar>>()
        .iter(world)
        .collect();
    if radars.is_empty() || world.query::<&RadarBlip>().iter(world).next().is_some() {
        return;
    }
    let colors = theme_colors(world);
    let rect = radars
        .iter()
        .find_map(|e| match world.get::<Radar>(*e)?.mode {
            RadarMode::Minimap { world_rect } => Some(world_rect),
            RadarMode::Radar { .. } => None,
        })
        .unwrap_or([-40.0, -40.0, 40.0, 40.0]);
    let (w, h) = (rect[2] - rect[0], rect[3] - rect[1]);
    for (i, (u, v)) in [(0.3, 0.6), (0.4, 0.45), (0.7, 0.3), (0.62, 0.72)]
        .iter()
        .enumerate()
    {
        let at = Vec3::new(rect[0] + u * w, 0.0, rect[1] + v * h);
        world.spawn((
            RadarBlip {
                label: format!("sample {}", i + 1),
                ..RadarBlip::dot(if i < 2 { colors.primary } else { colors.danger })
            },
            GlobalTransform(regia_core::glam::Affine3A::from_translation(at)),
        ));
    }
}
