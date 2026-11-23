// SPDX-License-Identifier: MPL-2.0
//! Interactive scrollbars (ADR-273 lane 8, ADR-344): vertical and horizontal tracks with a draggable
//! thumb, click-on-track jumps, an auto-hide option and theme classes.
//!
//! Add [`Scrollbars`] to a scroll view. The bars are ordinary UI nodes, so they hit-test, theme and
//! draw like any other node. They are *siblings* of the scroll view (children of its parent,
//! absolutely positioned over its edges), not children: a child would count toward the view's
//! content size and make it scrollable without end. The scroll view therefore needs a parent node
//! that does not scroll itself. Without a parent (or without [`Scrollbars`]) the scroll view keeps
//! its built-in thin, non-interactive thumb.
//!
//! The nodes carry the theme classes `scrollbar-track` and `scrollbar-thumb`. A named preset
//! stores white at the design alphas, so [`ScrollbarStyle::matching`] still sees that preset
//! on the component. Paint tints a preset with the theme's `color.text` at the same alphas.
//! A custom config, or a frame with no theme color, is painted as stored. Set a color to
//! `None` to leave it to the theme.
//!
//! Theme slots (ADR-7208): a theme that sets `color.scrollbar-thumb`, `-thumb-hover`,
//! `-thumb-active` or `color.scrollbar-track` paints a preset's thumb states and track with those
//! colors instead of the `color.text` tints (an overlay preset stays track-less). The scroll
//! view's built-in thin thumb draws in `color.scrollbar-thumb` when it is set. No built-in theme
//! sets them, so the default look is unchanged.
//!
//! The agent surface is `ui.scroll_to` (`book/agents/ui-list.md`).
//!
//! Named looks (ADR-520): [`ScrollbarStyle`] presets build [`Scrollbars`] for the default design's
//! overlay bar (thin, pill-shaped, no track, grows under the pointer, hides when idle), its compact
//! density, a classic always-visible bar with a track, and no bars at all. The agent sets them
//! with `ui.scroll { action: "style" }` (`book/agents/ui-scroll.md`).

use regia_app::prelude::*;

use crate::color::UiColor;
use crate::interaction::{UiEvent, UiEventKind, UiPointer};
use crate::layout::{LayoutDirty, ScrollDirty};
use crate::node::{
    Background, BackgroundKind, Border, Classes, ComputedNode, Interactable, NodeStyle, UiNode,
};
use crate::scroll::{ScrollOffset, ScrollVelocity, ScrollView};
use crate::style::{Dimension, Display, Edges, PositionType, Style};
use crate::theme::Themed;

/// One scroll axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAxis {
    /// Left to right.
    Horizontal,
    /// Top to bottom.
    Vertical,
}

impl ScrollAxis {
    fn index(self) -> usize {
        match self {
            Self::Horizontal => 0,
            Self::Vertical => 1,
        }
    }
}

/// Interactive scrollbars for a [`ScrollView`] (or a list).
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Scrollbars {
    /// Show the vertical bar when the content is taller than the view.
    pub vertical: bool,
    /// Show the horizontal bar when the content is wider than the view.
    pub horizontal: bool,
    /// Bar thickness (UI px).
    pub thickness: f32,
    /// Shortest thumb (UI px).
    pub min_thumb: f32,
    /// Hide the bars after this many idle seconds (no scroll, pointer not on a bar); `None` keeps
    /// them visible whenever the content overflows.
    pub auto_hide: Option<f32>,
    /// Track color; `None` leaves it to the theme class `scrollbar-track`.
    pub track_color: Option<UiColor>,
    /// Thumb color; `None` leaves it to the theme class `scrollbar-thumb`.
    pub thumb_color: Option<UiColor>,
    /// Bar thickness (UI px) while the pointer is on a bar or drags the thumb; `None` keeps
    /// [`Self::thickness`].
    pub hover_thickness: Option<f32>,
    /// Pill-shaped track and thumb (corner radius half the thickness).
    pub rounded: bool,
    /// Thumb color while the pointer is on a bar; `None` keeps [`Self::thumb_color`].
    pub thumb_hover_color: Option<UiColor>,
    /// Thumb color while the thumb is pressed or dragged; `None` falls back to the hover color.
    pub thumb_active_color: Option<UiColor>,
}

/// A named scrollbar look (ADR-520): [`Scrollbars::styled`] turns it into a [`Scrollbars`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarStyle {
    /// The default design's bar (`specs/ui-default-design.md` § 3, `size.scrollbar`): 10 px,
    /// 14 px under the pointer, pill-shaped, no track, thumb at 25 / 40 / 55 % of the text color
    /// (idle / hover / pressed), hidden after one idle second.
    Overlay,
    /// The overlay bar at compact density: 8 px, 10 px under the pointer.
    Compact,
    /// An always-visible 12 px bar with a track and square corners.
    Classic,
    /// No bars: the view still scrolls by wheel, drag, keys and focus.
    Hidden,
}

impl ScrollbarStyle {
    /// Every style, in documentation order.
    pub const ALL: [Self; 4] = [Self::Overlay, Self::Compact, Self::Classic, Self::Hidden];

    /// The agent name (`"overlay"`, `"compact"`, `"classic"`, `"hidden"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Overlay => "overlay",
            Self::Compact => "compact",
            Self::Classic => "classic",
            Self::Hidden => "hidden",
        }
    }

    /// Parses [`Self::name`].
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The style `config` was built from, when it is still exactly a preset.
    pub fn matching(config: &Scrollbars) -> Option<Self> {
        Self::ALL.into_iter().find(|s| Scrollbars::styled(*s) == *config)
    }
}

/// `text` at `alpha`. Preset thumbs are tints of the theme's `color.text`.
fn text_tint(text: UiColor, alpha: f32) -> UiColor {
    UiColor([text.0[0], text.0[1], text.0[2], alpha])
}

/// The theme colors a preset is painted with: `color.text` and the scrollbar slots.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct ScrollbarPaint {
    /// Theme `color.text`.
    pub(crate) text: Option<UiColor>,
    /// `color.scrollbar-track`.
    pub(crate) track: Option<UiColor>,
    /// `color.scrollbar-thumb`.
    pub(crate) thumb: Option<UiColor>,
    /// `color.scrollbar-thumb-hover`.
    pub(crate) thumb_hover: Option<UiColor>,
    /// `color.scrollbar-thumb-active`.
    pub(crate) thumb_active: Option<UiColor>,
}

impl ScrollbarPaint {
    /// The paint of a resolved theme.
    pub(crate) fn of(theme: &crate::theme_file::ResolvedTheme) -> Self {
        Self {
            text: theme.color("color.text"),
            track: theme.color("color.scrollbar-track"),
            thumb: theme.color("color.scrollbar-thumb"),
            thumb_hover: theme.color("color.scrollbar-thumb-hover"),
            thumb_active: theme.color("color.scrollbar-thumb-active"),
        }
    }

    fn any_slot(&self) -> bool {
        self.track.is_some()
            || self.thumb.is_some()
            || self.thumb_hover.is_some()
            || self.thumb_active.is_some()
    }
}

/// Preset colors tinted with the theme's text color, then replaced by the theme's scrollbar
/// slots where it sets them (ADR-7208; a track-less preset keeps no track). A custom config, or
/// a frame with no theme color, is returned unchanged.
fn colors_for_theme(config: &Scrollbars, paint: ScrollbarPaint) -> Scrollbars {
    if paint.text.is_none() && !paint.any_slot() {
        return config.clone();
    }
    let Some(style) = ScrollbarStyle::matching(config) else {
        return config.clone();
    };
    let mut out = Scrollbars::styled_with_text(style, paint.text.unwrap_or(UiColor::WHITE));
    let has_track = out.track_color.is_some_and(|c| c.0[3] > 0.0);
    if has_track && let Some(track) = paint.track {
        out.track_color = Some(track);
    }
    if let Some(thumb) = paint.thumb {
        out.thumb_color = Some(thumb);
    }
    if let Some(hover) = paint.thumb_hover {
        out.thumb_hover_color = Some(hover);
    }
    if let Some(active) = paint.thumb_active {
        out.thumb_active_color = Some(active);
    }
    out
}

/// The point a track click uses: the active touch when one is down, otherwise the cursor.
fn track_pointer(pointer: &UiPointer) -> Option<[f32; 2]> {
    pointer.position()
}

impl Scrollbars {
    /// Bars in a named look, on both axes (a bar shows only when its axis overflows).
    ///
    /// Colors are white at the preset alphas. `colors_for_theme` tints a preset with the
    /// active theme's text color at paint time, so [`ScrollbarStyle::matching`] still sees this
    /// white preset on the stored component.
    pub fn styled(style: ScrollbarStyle) -> Self {
        Self::styled_with_text(style, UiColor::WHITE)
    }

    fn styled_with_text(style: ScrollbarStyle, text: UiColor) -> Self {
        let tint = |alpha: f32| text_tint(text, alpha);
        let overlay = Self {
            vertical: true,
            horizontal: true,
            thickness: 10.0,
            min_thumb: 24.0,
            auto_hide: Some(1.0),
            track_color: Some(UiColor::TRANSPARENT),
            thumb_color: Some(tint(0.25)),
            hover_thickness: Some(14.0),
            rounded: true,
            thumb_hover_color: Some(tint(0.40)),
            thumb_active_color: Some(tint(0.55)),
        };
        match style {
            ScrollbarStyle::Overlay => overlay,
            ScrollbarStyle::Compact => Self {
                thickness: 8.0,
                hover_thickness: Some(10.0),
                min_thumb: 16.0,
                ..overlay
            },
            ScrollbarStyle::Classic => Self {
                thickness: 12.0,
                auto_hide: None,
                track_color: Some(tint(0.08)),
                thumb_color: Some(tint(0.35)),
                hover_thickness: None,
                rounded: false,
                thumb_hover_color: Some(tint(0.45)),
                thumb_active_color: Some(tint(0.55)),
                ..overlay
            },
            ScrollbarStyle::Hidden => Self {
                vertical: false,
                horizontal: false,
                ..overlay
            },
        }
    }

    /// The default design's overlay bar ([`ScrollbarStyle::Overlay`]).
    pub fn overlay() -> Self {
        Self::styled(ScrollbarStyle::Overlay)
    }

    /// A classic bar with a track ([`ScrollbarStyle::Classic`]).
    pub fn classic() -> Self {
        Self::styled(ScrollbarStyle::Classic)
    }
}

/// What the pointer is doing with a view's bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BarState {
    /// Not on a bar.
    #[default]
    Idle,
    /// On a track or thumb.
    Hover,
    /// Pressing or dragging the thumb.
    Active,
}

/// The thumb color for `state`: active falls back to hover, hover to the idle color.
pub fn thumb_color_for(config: &Scrollbars, state: BarState) -> Option<UiColor> {
    let hover = config.thumb_hover_color.or(config.thumb_color);
    match state {
        BarState::Idle => config.thumb_color,
        BarState::Hover => hover,
        BarState::Active => config.thumb_active_color.or(hover),
    }
}

/// The bar thickness (UI px) for `state`: hover and active grow to `hover_thickness`.
pub fn thickness_for(config: &Scrollbars, state: BarState) -> f32 {
    match (state, config.hover_thickness) {
        (BarState::Hover | BarState::Active, Some(t)) => t,
        _ => config.thickness,
    }
}

/// The rounded-corner border of a bar part, or `None` for square bars.
fn bar_border(config: &Scrollbars, thickness: f32) -> Option<Border> {
    config.rounded.then(|| Border {
        color: UiColor::TRANSPARENT,
        widths: [0.0; 4],
        radius: [thickness.max(0.0) * 0.5; 4],
    })
}

impl Default for Scrollbars {
    fn default() -> Self {
        Self {
            vertical: true,
            horizontal: false,
            thickness: 8.0,
            min_thumb: 24.0,
            auto_hide: None,
            track_color: Some(UiColor([1.0, 1.0, 1.0, 0.08])),
            thumb_color: Some(UiColor([1.0, 1.0, 1.0, 0.35])),
            hover_thickness: None,
            rounded: false,
            thumb_hover_color: None,
            thumb_active_color: None,
        }
    }
}

/// On a bar node: the scroll view it belongs to and which part it is.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarPart {
    /// The scroll view entity.
    pub view: Entity,
    /// The axis the bar scrolls.
    pub axis: ScrollAxis,
    /// `true` for the thumb, `false` for the track.
    pub thumb: bool,
}

/// Runtime state of a scroll view's bars. Inserted automatically.
#[derive(Component, Debug, Clone, Default)]
pub struct ScrollbarNodes {
    // Indexed by axis: [horizontal, vertical]; each is (track, thumb).
    bars: [Option<(Entity, Entity)>; 2],
    idle: f32,
    last_offset: [f32; 2],
    bar_state: BarState,
    /// Theme colors last used to paint a preset (`color.text` and the scrollbar slots).
    paint: ScrollbarPaint,
}

impl ScrollbarNodes {
    /// The `(track, thumb)` entities of an axis, once spawned.
    pub fn bar(&self, axis: ScrollAxis) -> Option<(Entity, Entity)> {
        self.bars[axis.index()]
    }

    /// Seconds since the view last scrolled or the pointer last touched a bar.
    pub fn idle_secs(&self) -> f32 {
        self.idle
    }

    /// What the pointer did with the bars on the last update (hover grows and tints them).
    pub fn bar_state(&self) -> BarState {
        self.bar_state
    }
}

/// Where a thumb sits on its track, in the same unit as the inputs.
///
/// `track` is the track length, `viewport` the visible extent, `content` the full extent and `offset`
/// the scroll position. Returns `(start, length)` of the thumb along the track: the length is the
/// visible fraction of the track (at least `min_thumb`, at most the track), the start follows the
/// scroll fraction. `None` when the content fits.
pub fn thumb_span(
    track: f32,
    viewport: f32,
    content: f32,
    offset: f32,
    min_thumb: f32,
) -> Option<(f32, f32)> {
    // Written so that a NaN or infinite input gives `None` (a comparison with NaN is false): no
    // bar beats a NaN rect handed to layout.
    if !(track > 0.0
        && track.is_finite()
        && viewport.is_finite()
        && content.is_finite()
        && content > viewport + 1.0)
    {
        return None;
    }
    let len = (track * viewport / content).max(min_thumb).min(track).max(0.0);
    let max_scroll = (content - viewport).max(1.0);
    // A NaN offset counts as the start.
    let t = if offset.is_nan() {
        0.0
    } else {
        (offset / max_scroll).clamp(0.0, 1.0)
    };
    Some((t * (track - len), len))
}

/// The scroll offset a thumb drag of `delta` (same unit as `track`) adds: the content moves
/// `max_scroll / (track - thumb_len)` per unit of thumb travel.
pub fn drag_to_scroll(delta: f32, track: f32, thumb_len: f32, max_scroll: f32) -> f32 {
    let travel = track - thumb_len;
    if travel.is_nan() || travel <= 0.0 || !delta.is_finite() {
        return 0.0;
    }
    delta * max_scroll / travel
}

/// The scroll offset for a click `pos` along the track (from the track start): the thumb centers
/// on the click.
pub fn track_click_to_offset(pos: f32, track: f32, thumb_len: f32, max_scroll: f32) -> f32 {
    let travel = track - thumb_len;
    if travel.is_nan() || travel <= 0.0 || pos.is_nan() {
        return 0.0;
    }
    ((pos - thumb_len * 0.5) / travel).clamp(0.0, 1.0) * max_scroll
}

/// Rects (`[x, y, w, h]`, parent-local UI px) of one axis's track and thumb.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarRects {
    /// The track.
    pub track: [f32; 4],
    /// The thumb.
    pub thumb: [f32; 4],
}

/// Lays out one axis's bar. `view` is the scroll view's rect relative to the bars' parent and
/// `content` its content size, both in physical px, as is `offset`. Returns UI px (divided by
/// `scale`), or `None` when that axis does not overflow or is turned off.
pub fn bar_rects(
    config: &Scrollbars,
    axis: ScrollAxis,
    view: [f32; 4],
    content: [f32; 2],
    offset: [f32; 2],
    scale: f32,
    other_axis_visible: bool,
) -> Option<BarRects> {
    let on = match axis {
        ScrollAxis::Vertical => config.vertical,
        ScrollAxis::Horizontal => config.horizontal,
    };
    if !on {
        return None;
    }
    let thickness = config.thickness.max(1.0) * scale;
    let inset = if other_axis_visible { thickness } else { 0.0 };
    let (track_start, track_len, thumb) = match axis {
        ScrollAxis::Vertical => {
            let track_len = view[3] - inset;
            let (start, len) = thumb_span(
                track_len,
                view[3],
                content[1],
                offset[1],
                config.min_thumb * scale,
            )?;
            (view[1], track_len, (start, len))
        }
        ScrollAxis::Horizontal => {
            let track_len = view[2] - inset;
            let (start, len) = thumb_span(
                track_len,
                view[2],
                content[0],
                offset[0],
                config.min_thumb * scale,
            )?;
            (view[0], track_len, (start, len))
        }
    };
    let rects = match axis {
        ScrollAxis::Vertical => BarRects {
            track: [view[0] + view[2] - thickness, track_start, thickness, track_len],
            thumb: [
                view[0] + view[2] - thickness,
                track_start + thumb.0,
                thickness,
                thumb.1,
            ],
        },
        ScrollAxis::Horizontal => BarRects {
            track: [track_start, view[1] + view[3] - thickness, track_len, thickness],
            thumb: [
                track_start + thumb.0,
                view[1] + view[3] - thickness,
                thumb.1,
                thickness,
            ],
        },
    };
    let ui = |r: [f32; 4]| [r[0] / scale, r[1] / scale, r[2] / scale, r[3] / scale];
    Some(BarRects {
        track: ui(rects.track),
        thumb: ui(rects.thumb),
    })
}

fn abs_style(rect: [f32; 4]) -> Style {
    Style {
        position: PositionType::Absolute,
        inset: Edges {
            left: Dimension::Points(rect[0]),
            top: Dimension::Points(rect[1]),
            right: Dimension::Auto,
            bottom: Dimension::Auto,
        },
        size: [Dimension::Points(rect[2]), Dimension::Points(rect[3])],
        ..Default::default()
    }
}

fn hidden_style() -> Style {
    Style {
        display: Display::None,
        position: PositionType::Absolute,
        ..Default::default()
    }
}

fn spawn_bar(
    commands: &mut Commands,
    parent: Entity,
    view: Entity,
    axis: ScrollAxis,
    rects: BarRects,
    config: &Scrollbars,
) -> (Entity, Entity) {
    let mut part = |thumb: bool, rect: [f32; 4], class: &str, color: Option<UiColor>| {
        let mut e = commands.spawn((
            UiNode,
            NodeStyle::new(abs_style(rect)),
            Classes::one(class),
            Themed::default(),
            Interactable::default(),
            ScrollbarPart { view, axis, thumb },
            // A pointer convenience: the wheel, a drag and the reader's paging do the same (ADR-3025).
            crate::a11y::Accessible::decorative(),
            crate::a11y_audit::TargetExemption::Equivalent,
            ChildOf(parent),
        ));
        if let Some(color) = color {
            e.insert(Background {
                kind: BackgroundKind::Color(color),
            });
        }
        if let Some(border) = bar_border(config, config.thickness) {
            e.insert(border);
        }
        e.id()
    };
    let track = part(false, rects.track, "scrollbar-track", config.track_color);
    let thumb = part(true, rects.thumb, "scrollbar-thumb", config.thumb_color);
    (track, thumb)
}

/// `Update` (after `ui_scroll`): thumb drags and track clicks move the offset; the bars are spawned,
/// positioned, sized from the content and shown or hidden.
pub fn scrollbars_update(
    time: Res<Time>,
    pointer: Res<UiPointer>,
    mut events: MessageReader<UiEvent>,
    mut views: Query<(
        Entity,
        Ref<Scrollbars>,
        &ComputedNode,
        &mut ScrollOffset,
        &mut ScrollView,
        Option<&mut ScrollVelocity>,
        Option<&mut ScrollbarNodes>,
    )>,
    parts: Query<(Entity, &ScrollbarPart)>,
    part_nodes: Query<&ComputedNode, With<ScrollbarPart>>,
    nodes: Query<&ComputedNode>,
    parents: Query<&ChildOf>,
    mut styles: Query<&mut NodeStyle>,
    mut commands: Commands,
    mut dirty: ResMut<LayoutDirty>,
    mut scrolled: ResMut<ScrollDirty>,
    themes: Option<Res<crate::theme_resolve::ThemeResolver>>,
) {
    let theme_paint = themes
        .as_ref()
        .and_then(|resolver| resolver.resolved_theme().map(ScrollbarPaint::of))
        .unwrap_or_default();
    let _span = tracing::trace_span!("ui_scrollbars").entered();
    let dt = time.delta_secs();
    // Bars whose scroll view is gone or no longer asks for them.
    for (entity, part) in &parts {
        if views.get_mut(part.view).is_err() {
            commands.entity(entity).despawn();
        }
    }
    // Pointer: thumb drags and track clicks, as (view, axis, value, value is an absolute offset).
    let mut moves: Vec<(Entity, ScrollAxis, f32, bool)> = Vec::new();
    for event in events.read() {
        let Ok((_, part)) = parts.get(event.target) else {
            continue;
        };
        let Ok((_, config, view_node, offset, _, _, state)) = views.get_mut(part.view) else {
            continue;
        };
        let Some((track, _)) = state.as_deref().and_then(|s| s.bar(part.axis)) else {
            continue;
        };
        let Ok(track_node) = part_nodes.get(track) else {
            continue;
        };
        let i = part.axis.index();
        let viewport = view_node.rect[2 + i];
        let content = view_node.content_size[i];
        let max_scroll = (content - viewport).max(0.0);
        let track_len = track_node.rect[2 + i];
        let min_thumb = config.min_thumb * view_node.scale.max(0.001);
        let thumb_len = thumb_span(track_len, viewport, content, offset.0[i], min_thumb)
            .map_or(track_len, |(_, len)| len);
        match event.kind {
            UiEventKind::Drag(delta) if part.thumb => {
                moves.push((
                    part.view,
                    part.axis,
                    drag_to_scroll(delta[i], track_len, thumb_len, max_scroll),
                    false,
                ));
            }
            UiEventKind::Click if !part.thumb => {
                if let Some(at) = track_pointer(&pointer) {
                    let pos = at[i] - track_node.rect[i];
                    let target = track_click_to_offset(pos, track_len, thumb_len, max_scroll);
                    moves.push((part.view, part.axis, target, true));
                }
            }
            _ => {}
        }
    }
    let mut any_moved = false;
    for (entity, config, computed, mut offset, mut view, velocity, state) in &mut views {
        let scale = computed.scale.max(0.001);
        let mut touched = false;
        for (target, axis, value, absolute) in &moves {
            if *target != entity {
                continue;
            }
            touched = true;
            let i = axis.index();
            let max = (computed.content_size[i] - computed.rect[2 + i]).max(0.0);
            let next = if *absolute { *value } else { offset.0[i] + *value };
            let next = next.clamp(0.0, max);
            if next != offset.0[i] {
                offset.0[i] = next;
                any_moved = true;
            }
        }
        if touched && let Some(mut v) = velocity {
            v.v = [0.0; 2];
        }
        // The bars live in the view's parent.
        let Ok(parent) = parents.get(entity).map(|p| p.0) else {
            continue;
        };
        let Ok(parent_node) = nodes.get(parent) else {
            continue;
        };
        let view_rect = [
            computed.rect[0] - parent_node.rect[0],
            computed.rect[1] - parent_node.rect[1],
            computed.rect[2],
            computed.rect[3],
        ];
        // What the pointer does with this view's bars: hovering grows and tints them.
        let bar_state = if [pointer.pressed, pointer.dragging]
            .into_iter()
            .flatten()
            .any(|e| parts.get(e).is_ok_and(|(_, p)| p.view == entity && p.thumb))
        {
            BarState::Active
        } else if pointer
            .hovered
            .is_some_and(|e| parts.get(e).is_ok_and(|(_, p)| p.view == entity))
        {
            BarState::Hover
        } else {
            BarState::Idle
        };
        let painted = colors_for_theme(&config, theme_paint);
        let thickness = thickness_for(&painted, bar_state);
        let sized = Scrollbars { thickness, ..painted };
        let config_changed = config.is_changed();
        let config = &sized;
        // Whole-pixel offsets, as the draw pass scrolls content.
        let rounded = [offset.0[0].round(), offset.0[1].round()];
        let content = computed.content_size;
        let v_probe = bar_rects(
            config,
            ScrollAxis::Vertical,
            view_rect,
            content,
            rounded,
            scale,
            false,
        );
        let h_probe = bar_rects(
            config,
            ScrollAxis::Horizontal,
            view_rect,
            content,
            rounded,
            scale,
            false,
        );
        let vertical = bar_rects(
            config,
            ScrollAxis::Vertical,
            view_rect,
            content,
            rounded,
            scale,
            h_probe.is_some(),
        );
        let horizontal = bar_rects(
            config,
            ScrollAxis::Horizontal,
            view_rect,
            content,
            rounded,
            scale,
            v_probe.is_some(),
        );
        let Some(mut state) = state else {
            commands.entity(entity).insert(ScrollbarNodes {
                last_offset: offset.0,
                ..Default::default()
            });
            continue;
        };
        // The built-in thin thumb steps aside once interactive bars own the job.
        if view.show_scrollbar {
            view.show_scrollbar = false;
        }
        // Idle tracking for auto-hide.
        let on_bar = [pointer.hovered, pointer.pressed, pointer.dragging]
            .into_iter()
            .flatten()
            .any(|e| parts.get(e).is_ok_and(|(_, p)| p.view == entity));
        if offset.0 != state.last_offset || on_bar {
            state.idle = 0.0;
        } else {
            state.idle += dt;
        }
        state.last_offset = offset.0;
        let shown = config.auto_hide.is_none_or(|limit| state.idle <= limit);
        let restyle = config_changed || state.bar_state != bar_state || state.paint != theme_paint;
        state.paint = theme_paint;
        state.bar_state = bar_state;
        for (axis, rects) in [
            (ScrollAxis::Vertical, vertical),
            (ScrollAxis::Horizontal, horizontal),
        ] {
            let slot = axis.index();
            match (state.bars[slot], rects) {
                (None, Some(rects)) => {
                    state.bars[slot] = Some(spawn_bar(&mut commands, parent, entity, axis, rects, config));
                    dirty.mark();
                }
                (Some((track, thumb)), found) => {
                    if styles.get_mut(track).is_err() {
                        // The bars were despawned (the component was removed and re-added).
                        state.bars[slot] = None;
                        continue;
                    }
                    let (track_style, thumb_style) = match found {
                        Some(r) if shown => (abs_style(r.track), abs_style(r.thumb)),
                        _ => (hidden_style(), hidden_style()),
                    };
                    for (bar, style) in [(track, track_style), (thumb, thumb_style)] {
                        if let Ok(mut node_style) = styles.get_mut(bar)
                            && node_style.0 != style
                        {
                            node_style.0 = style;
                            dirty.mark();
                        }
                    }
                    if restyle {
                        let thumb_color = thumb_color_for(config, bar_state);
                        for (bar, color) in [(track, config.track_color), (thumb, thumb_color)] {
                            match color {
                                Some(color) => {
                                    commands.entity(bar).insert(Background {
                                        kind: BackgroundKind::Color(color),
                                    });
                                }
                                None => {
                                    commands.entity(bar).remove::<Background>();
                                }
                            }
                            match bar_border(config, thickness) {
                                Some(border) => {
                                    commands.entity(bar).insert(border);
                                }
                                None => {
                                    commands.entity(bar).remove::<Border>();
                                }
                            }
                        }
                    }
                }
                (None, None) => {}
            }
        }
    }
    if any_moved {
        // Only offsets moved: a scroll-only layout walk (ADR-520).
        scrolled.mark();
    }
}

/// Installs the scrollbar system. [`crate::UiPlugin`] adds it.
pub struct ScrollbarPlugin;

impl Plugin for ScrollbarPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, scrollbars_update.after(crate::scroll::ui_scroll));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_span_tracks_the_scroll_fraction() {
        // 200 visible of 1000: the thumb is a fifth of the track.
        let (start, len) = thumb_span(200.0, 200.0, 1000.0, 0.0, 10.0).unwrap();
        assert_eq!((start, len), (0.0, 40.0));
        // Fully scrolled: the thumb touches the end of the track.
        let (start, len) = thumb_span(200.0, 200.0, 1000.0, 800.0, 10.0).unwrap();
        assert_eq!(start + len, 200.0);
        // Halfway through the scroll range.
        let (start, _) = thumb_span(200.0, 200.0, 1000.0, 400.0, 10.0).unwrap();
        assert_eq!(start, 80.0);
    }

    #[test]
    fn thumb_has_a_minimum_length() {
        let (_, len) = thumb_span(200.0, 200.0, 1.0e6, 0.0, 24.0).unwrap();
        assert_eq!(len, 24.0);
        // Never longer than the track.
        let (_, len) = thumb_span(20.0, 200.0, 220.0, 0.0, 50.0).unwrap();
        assert_eq!(len, 20.0);
    }

    #[test]
    fn no_thumb_when_the_content_fits() {
        assert_eq!(thumb_span(200.0, 200.0, 200.0, 0.0, 10.0), None);
        assert_eq!(thumb_span(200.0, 200.0, 200.5, 0.0, 10.0), None);
        assert_eq!(thumb_span(0.0, 200.0, 1000.0, 0.0, 10.0), None);
    }

    #[test]
    fn dragging_the_thumb_scrolls_proportionally() {
        // Track 200, thumb 40, scroll range 800: 160 px of travel covers 800 px of content.
        assert_eq!(drag_to_scroll(160.0, 200.0, 40.0, 800.0), 800.0);
        assert_eq!(drag_to_scroll(16.0, 200.0, 40.0, 800.0), 80.0);
        assert_eq!(drag_to_scroll(-16.0, 200.0, 40.0, 800.0), -80.0);
        assert_eq!(drag_to_scroll(5.0, 40.0, 40.0, 800.0), 0.0);
    }

    #[test]
    fn dragging_matches_the_thumb_position() {
        // Moving the offset by drag_to_scroll(d) moves the thumb by exactly d.
        let (track, viewport, content) = (300.0, 300.0, 1500.0);
        let (start0, len) = thumb_span(track, viewport, content, 100.0, 10.0).unwrap();
        let delta = 25.0;
        let moved = 100.0 + drag_to_scroll(delta, track, len, content - viewport);
        let (start1, _) = thumb_span(track, viewport, content, moved, 10.0).unwrap();
        assert!((start1 - start0 - delta).abs() < 1.0e-3);
    }

    #[test]
    fn track_click_centers_the_thumb() {
        // Track 200, thumb 40, range 800. Clicking the middle of the track lands halfway.
        assert_eq!(track_click_to_offset(100.0, 200.0, 40.0, 800.0), 400.0);
        // Near the ends it clamps.
        assert_eq!(track_click_to_offset(5.0, 200.0, 40.0, 800.0), 0.0);
        assert_eq!(track_click_to_offset(199.0, 200.0, 40.0, 800.0), 800.0);
        assert_eq!(track_click_to_offset(50.0, 40.0, 40.0, 800.0), 0.0);
    }

    #[test]
    fn bars_sit_on_the_view_edges_in_ui_px() {
        let config = Scrollbars {
            horizontal: true,
            ..Default::default()
        };
        // A 400x300 view at (20, 10) in its parent, scale 2 (physical px in, UI px out).
        let view = [40.0, 20.0, 800.0, 600.0];
        let content = [800.0, 3000.0];
        let v = bar_rects(
            &config,
            ScrollAxis::Vertical,
            view,
            content,
            [0.0, 0.0],
            2.0,
            false,
        )
        .unwrap();
        assert_eq!(v.track, [(40.0 + 800.0 - 16.0) / 2.0, 10.0, 8.0, 300.0]);
        assert_eq!(v.thumb[0], v.track[0]);
        assert_eq!(v.thumb[1], v.track[1]);
        // Content fits horizontally: no horizontal bar.
        assert_eq!(
            bar_rects(
                &config,
                ScrollAxis::Horizontal,
                view,
                content,
                [0.0; 2],
                2.0,
                false
            ),
            None
        );
        // Wide content: the horizontal bar sits at the bottom, and a visible other axis shortens it.
        let wide = [2000.0, 3000.0];
        let h = bar_rects(&config, ScrollAxis::Horizontal, view, wide, [0.0; 2], 2.0, true).unwrap();
        assert_eq!(
            h.track,
            [20.0, (20.0 + 600.0 - 16.0) / 2.0, (800.0 - 16.0) / 2.0, 8.0]
        );
    }

    #[test]
    fn axes_can_be_turned_off() {
        let config = Scrollbars {
            vertical: false,
            ..Default::default()
        };
        let view = [0.0, 0.0, 100.0, 100.0];
        assert_eq!(
            bar_rects(
                &config,
                ScrollAxis::Vertical,
                view,
                [100.0, 900.0],
                [0.0; 2],
                1.0,
                false
            ),
            None
        );
    }

    #[test]
    fn styles_build_the_default_design_bars() {
        let overlay = Scrollbars::overlay();
        assert_eq!((overlay.thickness, overlay.hover_thickness), (10.0, Some(14.0)));
        assert!(overlay.rounded && overlay.vertical && overlay.horizontal);
        assert_eq!(
            overlay.track_color,
            Some(UiColor::TRANSPARENT),
            "the overlay bar has no track"
        );
        assert_eq!(overlay.auto_hide, Some(1.0));
        let compact = Scrollbars::styled(ScrollbarStyle::Compact);
        assert_eq!((compact.thickness, compact.hover_thickness), (8.0, Some(10.0)));
        let classic = Scrollbars::classic();
        assert!(!classic.rounded && classic.auto_hide.is_none() && classic.hover_thickness.is_none());
        let hidden = Scrollbars::styled(ScrollbarStyle::Hidden);
        assert!(!hidden.vertical && !hidden.horizontal);
        for style in ScrollbarStyle::ALL {
            assert_eq!(ScrollbarStyle::parse(style.name()), Some(style));
            assert_eq!(ScrollbarStyle::matching(&Scrollbars::styled(style)), Some(style));
        }
        // The pre-ADR-520 default is not a preset, and an edited preset no longer matches.
        assert_eq!(ScrollbarStyle::matching(&Scrollbars::default()), None);
        let edited = Scrollbars {
            thickness: 11.0,
            ..Scrollbars::overlay()
        };
        assert_eq!(ScrollbarStyle::matching(&edited), None);
        assert_eq!(ScrollbarStyle::parse("fat"), None);
    }

    #[test]
    fn thumbs_tint_and_grow_with_the_pointer() {
        let overlay = Scrollbars::overlay();
        let alpha = |state| thumb_color_for(&overlay, state).unwrap().0[3];
        assert_eq!(alpha(BarState::Idle), 0.25);
        assert_eq!(alpha(BarState::Hover), 0.40);
        assert_eq!(alpha(BarState::Active), 0.55);
        assert_eq!(thickness_for(&overlay, BarState::Idle), 10.0);
        assert_eq!(thickness_for(&overlay, BarState::Hover), 14.0);
        assert_eq!(thickness_for(&overlay, BarState::Active), 14.0);
        // Without hover settings every state looks the same (the pre-ADR-520 default).
        let plain = Scrollbars::default();
        for state in [BarState::Idle, BarState::Hover, BarState::Active] {
            assert_eq!(thumb_color_for(&plain, state), plain.thumb_color);
            assert_eq!(thickness_for(&plain, state), plain.thickness);
        }
        // Active falls back to hover.
        let hover_only = Scrollbars {
            thumb_hover_color: Some(UiColor::WHITE),
            ..Scrollbars::default()
        };
        assert_eq!(
            thumb_color_for(&hover_only, BarState::Active),
            Some(UiColor::WHITE)
        );
    }

    #[test]
    fn rounded_bars_are_pills() {
        let border = bar_border(&Scrollbars::overlay(), 14.0).unwrap();
        assert_eq!(border.radius, [7.0; 4]);
        assert_eq!(border.widths, [0.0; 4]);
        assert_eq!(bar_border(&Scrollbars::classic(), 12.0), None);
    }

    #[test]
    fn overlay_thumb_uses_the_theme_text_color() {
        let light = UiColor::from_hex("#18202a").expect("hex");
        let painted = colors_for_theme(
            &Scrollbars::overlay(),
            ScrollbarPaint {
                text: Some(light),
                ..Default::default()
            },
        );
        let [r, g, b, a] = painted.thumb_color.expect("thumb").0;
        assert!((a - 0.25).abs() < 1e-5, "{a}");
        assert!(
            (r - light.0[0]).abs() < 1e-5 && (g - light.0[1]).abs() < 1e-5 && (b - light.0[2]).abs() < 1e-5
        );
        assert!((painted.thumb_hover_color.expect("hover").0[3] - 0.40).abs() < 1e-5);
        assert!((painted.thumb_active_color.expect("active").0[3] - 0.55).abs() < 1e-5);
        // The stored preset stays white, so matching still names it.
        assert_eq!(
            ScrollbarStyle::matching(&Scrollbars::overlay()),
            Some(ScrollbarStyle::Overlay)
        );
        let dark = UiColor::from_hex("#e8edf4").expect("hex");
        let classic = colors_for_theme(&Scrollbars::classic(), text_paint(dark));
        let [r, g, b, a] = classic.thumb_color.expect("thumb").0;
        assert!((a - 0.35).abs() < 1e-5, "{a}");
        assert!((r - dark.0[0]).abs() < 1e-5 && (g - dark.0[1]).abs() < 1e-5 && (b - dark.0[2]).abs() < 1e-5);
        // No theme color, and a config that is no longer a preset, stay as stored.
        assert_eq!(
            colors_for_theme(&Scrollbars::overlay(), ScrollbarPaint::default()).thumb_color,
            Scrollbars::overlay().thumb_color
        );
        let custom = Scrollbars {
            thickness: 11.0,
            ..Scrollbars::overlay()
        };
        assert_eq!(
            colors_for_theme(&custom, text_paint(light)).thumb_color,
            custom.thumb_color
        );
    }

    fn text_paint(text: UiColor) -> ScrollbarPaint {
        ScrollbarPaint {
            text: Some(text),
            ..Default::default()
        }
    }

    /// ADR-7208: the scrollbar slots replace the text tints of a preset; an overlay preset keeps
    /// no track; a custom config ignores them.
    #[test]
    fn scrollbar_slots_replace_preset_tints() {
        let red = UiColor([1.0, 0.0, 0.0, 1.0]);
        let green = UiColor([0.0, 1.0, 0.0, 1.0]);
        let blue = UiColor([0.0, 0.0, 1.0, 1.0]);
        let paint = ScrollbarPaint {
            text: Some(UiColor::WHITE),
            track: Some(green),
            thumb: Some(red),
            thumb_hover: None,
            thumb_active: Some(blue),
        };
        let classic = colors_for_theme(&Scrollbars::classic(), paint);
        assert_eq!(classic.track_color, Some(green));
        assert_eq!(classic.thumb_color, Some(red));
        assert_eq!(thumb_color_for(&classic, BarState::Active), Some(blue));
        let hover = thumb_color_for(&classic, BarState::Hover).expect("hover");
        assert!(
            (hover.0[3] - 0.45).abs() < 1e-5,
            "unset hover slot keeps the tint"
        );
        let overlay = colors_for_theme(&Scrollbars::overlay(), paint);
        assert_eq!(
            overlay.track_color,
            Some(UiColor::TRANSPARENT),
            "overlay stays track-less"
        );
        assert_eq!(overlay.thumb_color, Some(red));
        let custom = Scrollbars {
            thickness: 11.0,
            ..Scrollbars::overlay()
        };
        assert_eq!(colors_for_theme(&custom, paint), custom);
    }

    #[test]
    fn track_click_uses_the_active_touch() {
        let mut pointer = UiPointer::default();
        pointer.cursor = None;
        pointer.touches.insert(1, [40.0, 10.0]);
        pointer.active_touch = Some(1);
        assert_eq!(track_pointer(&pointer), Some([40.0, 10.0]));
        pointer.active_touch = None;
        pointer.cursor = Some([0.0, 0.0]);
        assert_eq!(track_pointer(&pointer), Some([0.0, 0.0]));
        pointer.cursor = None;
        assert_eq!(track_pointer(&pointer), None);
    }
}
