// SPDX-License-Identifier: MPL-2.0
//! UI nodes: the ECS component model of the widget tree (`specs/18` § 1).
//!
//! A [`UiRoot`] entity anchors a tree (its children, via the ECS hierarchy). Every descendant is a
//! [`UiNode`] with a [`Style`]; visual components (`Background`, `Border`, `Shadow`, `UiImage`,
//! `Text`, `Opacity`) and widget components attach to the same entities. Layout writes
//! [`ComputedNode`] every frame it runs.

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use crate::color::UiColor;
use crate::style::Style;

/// Anchor of a UI tree.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct UiRoot {
    /// Where this tree renders.
    pub target: UiTarget,
    /// How UI pixels map to physical pixels.
    pub scale_mode: crate::scale::ScaleMode,
    /// Draw order between roots (lower first).
    pub sort_order: i32,
}

/// Where a [`UiRoot`] renders (`specs/18` § 3). Camera and world-space targets are drawn by
/// [`crate::world_target`] (ADR-2406).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum UiTarget {
    /// Fill a window (screen-space overlay). `None` = the primary window.
    Window(Option<Entity>),
    /// Draw into a camera's target: a texture camera's image, or a window camera's viewport rect.
    Camera(Entity),
    /// World-space panel of `size` UI px at `ppm` pixels per metre, at the root's `GlobalTransform`
    /// ([`crate::world_target::WorldPanel`] for billboarding, fade and depth test).
    WorldSpace {
        /// Panel size in UI px.
        size: [f32; 2],
        /// Pixels per metre.
        ppm: f32,
    },
}

impl Default for UiTarget {
    fn default() -> Self {
        Self::Window(None)
    }
}

/// Marker: this entity is part of a UI tree.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UiNode;

/// Styleable node. Defaults to an auto-sized flex item.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct NodeStyle(pub Style);

impl NodeStyle {
    /// Wraps a [`Style`].
    pub fn new(style: Style) -> Self {
        Self(style)
    }
}

/// Layout result for one node, in physical pixels relative to the root's top-left (y down).
/// Written by the layout system (`specs/18` § 1).
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct ComputedNode {
    /// Final rect: x, y (top-left), width, height.
    pub rect: [f32; 4],
    /// Rect used for clipping children (ancestors' clip intersection); equals `rect` when not
    /// clipping.
    pub clip_rect: [f32; 4],
    /// Draw depth (root `sort_order` + `ZOffset` + tree depth fraction).
    pub z: f32,
    /// Content size (before scrolling), for scroll views.
    pub content_size: [f32; 2],
    /// The UI scale this node was laid out at.
    pub scale: f32,
    /// Preorder index across all roots (later paints on top); breaks hit-test ties between nodes
    /// of equal `z`, independent of ECS archetype order.
    pub paint_order: u32,
}

/// Explicit draw-depth offset within a root (higher draws later / on top).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ZOffset(pub i32);

/// Theme classes applied to this node, e.g. `["button", "primary"]` (`specs/18` § 5).
#[derive(Component, Debug, Clone, PartialEq, Eq, Default)]
pub struct Classes(pub Vec<String>);

impl Classes {
    /// Single class.
    pub fn one(name: &str) -> Self {
        Self(vec![name.to_owned()])
    }
}

/// Fill behind a node's content.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Background {
    /// What fills the node's rect.
    pub kind: BackgroundKind,
}

impl Default for Background {
    fn default() -> Self {
        Self {
            kind: BackgroundKind::Color(UiColor::TRANSPARENT),
        }
    }
}

/// A background fill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BackgroundKind {
    /// Solid colour.
    Color(UiColor),
    /// Two-stop linear gradient from `start` to `end` (Tier 1 gradient, `specs/18` § 1). The
    /// angle is degrees clockwise from "top to bottom".
    Gradient {
        /// Colour at the gradient start.
        start: UiColor,
        /// Colour at the gradient end.
        end: UiColor,
        /// Angle in degrees (0 = vertical top→bottom).
        angle: f32,
    },
    /// Image fill (registered via [`crate::render::UiRenderer::add_image_rgba`]); `uv` selects a
    /// sub-rect; `nine_slice` (left, top, right, bottom insets in px) stretches edges instead of
    /// the whole image when set.
    Image {
        /// Renderer image id.
        id: u64,
        /// Sub-rect of the image in UV space.
        uv: [f32; 4],
        /// 9-slice insets (l, t, r, b) in source pixels; `None` stretches uniformly.
        nine_slice: Option<[f32; 4]>,
    },
}

/// Stroke around the node's rect.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Border {
    /// Border colour.
    pub color: UiColor,
    /// Widths (l, t, r, b) in UI px.
    pub widths: [f32; 4],
    /// Corner radii (tl, tr, br, bl) in UI px.
    pub radius: [f32; 4],
}

impl Default for Border {
    fn default() -> Self {
        Self {
            color: UiColor::WHITE,
            widths: [0.0; 4],
            radius: [0.0; 4],
        }
    }
}

/// Soft drop shadow.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    /// Shadow colour (usually a translucent black).
    pub color: UiColor,
    /// Blur radius in UI px.
    pub blur: f32,
    /// Offset (x, y) in UI px.
    pub offset: [f32; 2],
}

impl Default for Shadow {
    fn default() -> Self {
        Self {
            color: UiColor([0.0, 0.0, 0.0, 0.4]),
            blur: 8.0,
            offset: [0.0, 4.0],
        }
    }
}

/// Frosted-glass backdrop: blurs whatever is already drawn under the node's rect (the scene and
/// earlier UI) with a Gaussian before the node's own background paints (ADR-1836). The blur
/// follows the node's corner radius. Needs a readable render target; otherwise only the node's
/// own translucent background shows.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct BackdropBlur {
    /// Gaussian σ in UI px (clamped to `0..=64` at draw time). 0 draws nothing.
    pub radius: f32,
}

impl Default for BackdropBlur {
    fn default() -> Self {
        Self { radius: 8.0 }
    }
}

/// How an image is placed in its node's rect (`.ui.ron` v11 `image.fit`, ADR-9980).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFit {
    /// Stretched over the whole rect (the aspect is not kept).
    Stretch,
    /// Scaled to fit inside the rect, aspect kept, centred (letterboxed).
    #[default]
    Contain,
    /// Scaled to cover the rect, aspect kept, centred; the overflow is cropped from the UVs.
    Cover,
    /// Repeated at its native size (source pixels times the UI scale) from the rect's top-left
    /// corner, clipped to the rect (`.ui.ron` v11 fold, ADR-7751).
    Tile,
}

impl ImageFit {
    /// The `.ui.ron` / agent spelling (`stretch`, `contain`, `cover`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Stretch => "stretch",
            Self::Contain => "contain",
            Self::Cover => "cover",
            Self::Tile => "tile",
        }
    }

    /// Every fit, in declaration order (designer pickers, agent error text).
    pub const ALL: [ImageFit; 4] = [Self::Stretch, Self::Contain, Self::Cover, Self::Tile];

    /// The fit with the spelling [`Self::name`] gives.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.name() == s)
    }
}

/// How the node's [`UiImage`] keeps its aspect (ADR-9980). Without this component a
/// `preserve_aspect` image is letterboxed ([`ImageFit::Contain`]); with [`ImageFit::Cover`] it fills
/// the rect and its UV rect is cropped to the rect's aspect, centred. Applies only while the image
/// keeps its aspect (`UiImage::preserve_aspect`); a stretched image ignores it. [`ImageFit::Tile`]
/// (ADR-7751) repeats the picture at its native size whatever `preserve_aspect` says.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UiImageFit(pub ImageFit);

/// An image drawn in the node's rect (`specs/18` § 1).
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct UiImage {
    /// Renderer image id (see [`crate::render::UiRenderer::add_image_rgba`]).
    pub id: u64,
    /// Sub-rect in UV space (default the whole image).
    pub uv: [f32; 4],
    /// Multiplicative tint.
    pub tint: UiColor,
    /// Keep the image's aspect ratio inside the rect.
    pub preserve_aspect: bool,
}

impl Default for UiImage {
    fn default() -> Self {
        Self {
            id: 0,
            uv: [0.0, 0.0, 1.0, 1.0],
            tint: UiColor::WHITE,
            preserve_aspect: true,
        }
    }
}

/// Multiplies the node's (and its subtree's) alpha.
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct Opacity(pub f32);

impl Opacity {
    /// Opaque.
    pub const OPAQUE: Self = Self(1.0);
}

/// One filled rectangle of a [`UiCanvas`], in the node's unit square: `(0, 0)` is the node's
/// top-left corner and `(1, 1)` its bottom-right.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasRect {
    /// Top-left corner, unit coordinates.
    pub min: [f32; 2],
    /// Bottom-right corner, unit coordinates.
    pub max: [f32; 2],
    /// Fill colour.
    pub color: UiColor,
}

/// Plain filled rectangles drawn over the node's background, laid out in the node's unit square
/// (graphs, meters, sparklines). Changing it redraws only the node's subtree and never re-runs
/// layout: the cheap way to animate many small bars (the performance overlay's frame graph).
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct UiCanvas {
    /// Rectangles in paint order.
    pub rects: Vec<CanvasRect>,
}

/// Clip children to this node's rect when rendering (also implied by `overflow: clip`).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClipChildren;

/// Marker: pointer events pass through this node to what is behind it.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PickThrough;

/// The node receives pointer events (`specs/18` § 4). Nodes without this are transparent to
/// interaction.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Interactable {
    /// When true, hit testing skips this node and picks what is behind it (click-through overlays).
    pub pick_through: bool,
}

/// Marker written by the interaction system: pointer is over the node.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Hovered;

/// Marker written by the interaction system: primary button/finger is down on the node.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pressed;

/// Marker written by the focus system: node has keyboard/gamepad focus.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Focused;

/// Widget is disabled: not interactable, drawn with the theme's disabled state.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Disabled;

/// Widget is selected, checked or open: the theme's `selected` state composes (`specs/ui-default-design.md` § 3).
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Selected;

/// Widget holds an invalid value: the theme's `error` state composes.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Invalid;

/// Respect the platform safe area by padding this node's layout (`specs/18` § 3).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SafeAreaPadding;

/// Node name (authoring/debugging; also used by bindings and tests).
#[derive(Component, Debug, Clone, PartialEq, Eq, Default)]
pub struct NodeName(pub String);

/// Disables pointer and focus input for this node and every descendant, including children
/// added later. Removing this flag restores inherited input without changing local [`Disabled`]
/// flags. A `.ui.ron` node sets it with `disabled_subtree` (v11 fold, ADR-8252).
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DisabledSubtree;

impl Disabled {
    /// The recursive form of disabled input. Insert the returned component on the subtree root.
    pub const fn recursive() -> DisabledSubtree {
        DisabledSubtree
    }
}

/// Effective input-disabled state, read directly from the current hierarchy. A local
/// [`Disabled`] does not affect children; [`DisabledSubtree`] on any ancestor does.
#[must_use]
pub fn is_disabled(world: &World, entity: Entity) -> bool {
    if world.get::<Disabled>(entity).is_some() {
        return true;
    }
    let mut current = Some(entity);
    for _ in 0..4096 {
        let Some(node) = current else {
            return false;
        };
        if world.get::<DisabledSubtree>(node).is_some() {
            return true;
        }
        current = world.get::<ChildOf>(node).map(|p| p.0);
    }
    // A malformed or excessively deep input tree fails closed.
    true
}

pub(crate) fn subtree_disabled(
    entity: Entity,
    parents: &Query<&ChildOf>,
    disabled: &Query<(), With<DisabledSubtree>>,
) -> bool {
    if disabled.is_empty() {
        return false;
    }
    let mut current = Some(entity);
    for _ in 0..4096 {
        let Some(node) = current else {
            return false;
        };
        if disabled.contains(node) {
            return true;
        }
        current = parents.get(node).ok().map(|p| p.0);
    }
    true
}

/// Sets or clears only the requested disabled flag. Returns false for an absent entity.
/// Local and recursive flags are independent, so enabling a subtree does not enable a child
/// that was disabled locally. Focus on a newly disabled node is released immediately.
pub fn set_disabled(world: &mut World, entity: Entity, disabled: bool, recursive: bool) -> bool {
    let Ok(mut node) = world.get_entity_mut(entity) else {
        return false;
    };
    match (disabled, recursive) {
        (true, true) => {
            node.insert(DisabledSubtree);
        }
        (false, true) => {
            node.remove::<DisabledSubtree>();
        }
        (true, false) => {
            node.insert(Disabled);
        }
        (false, false) => {
            node.remove::<Disabled>();
        }
    }
    if world
        .get_resource::<crate::focus::FocusState>()
        .and_then(|s| s.focused)
        .is_some_and(|focused| is_disabled(world, focused))
    {
        crate::layers::set_focus_world(world, None);
    }
    true
}
