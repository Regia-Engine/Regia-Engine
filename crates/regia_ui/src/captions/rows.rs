// SPDX-License-Identifier: MPL-2.0
//! The caption block's UI nodes (`docs/specs/56-dialogue-and-subtitles.md` § 3.3): one `UiRoot`
//! named `CaptionBlock` above gameplay UI with an absolutely positioned child per visible row.
//! The rows follow the board: a row appears when its entry becomes visible, its text and rectangle
//! follow page changes and reflow, and it is despawned when the entry leaves. The block is an
//! accessibility live region (role `Status`, polite), so a screen reader announces new captions.
//!
//! Theme (spec 36 § 8.3, ADR-7208): the active theme's `subtitle` style paints the rows. Its
//! `subtitle.text` colors the text while the player keeps the default text color (white; a color
//! the player picks wins), `subtitle.plate` is the box plate color (its alpha multiplies the
//! player's box opacity) and `subtitle.plate-opacity` is the lowest box opacity. The high-contrast
//! themes set the plate opacity to 1, so turning high contrast on (`UiAccessibility` or
//! `ActiveTheme`) swaps the captions to an opaque plate. A player who turned the box off keeps it
//! off.
//!
//! Font, size and outline (ADR-9340): `subtitle.font` names the caption font family,
//! `subtitle.size` multiplies the text size on top of the player's `text_scale` (the board lays
//! the rows out at that size), and the `Outline` background draws a `subtitle.outline` colored
//! stroke `subtitle.outline-width` UI px wide around the glyphs (a text effect layer,
//! [`crate::text_effects::TextEffect::outline`]) instead of a plate.

use std::cell::Cell;
use std::collections::BTreeMap;

use regia_app::caption::CaptionId;
use regia_app::prelude::*;

use super::anchor::{AnchorMode, AnchorPlacement, CaptionAnchorState, arrow_glyph};
use super::board::{CaptionBoard, LabelSide, RowView};
use super::karaoke::{highlighted_prefix, word_timings};
use super::options::{CaptionBackground, CaptionOptions, Rgb};
use crate::a11y::{A11yRole, Accessible, Politeness};
use crate::color::UiColor;
use crate::layout::LayoutDirty;
use crate::node::{Background, BackgroundKind, NodeName, NodeStyle, UiNode, UiRoot};
use crate::scale::ScaleMode;
use crate::style::{Dimension, Edges, PositionType, Style};
use crate::text::{Text, TextAlign, TextContent, TextSpan, TextStyle, WrapMode};

/// Marks the `CaptionBlock` root.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct CaptionBlock;

/// Marks a row node and names the caption it shows.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptionRow(pub CaptionId);

/// Sort order of the block: above gameplay UI, below system menus.
pub const CAPTION_SORT_ORDER: i32 = 10_000;

thread_local! {
    /// Times [`sync_caption_rows`] did row work (past the empty-board return).
    ///
    /// Per test thread, so a parallel `cargo test` cannot bump another test's count.
    static ROW_SYNC_BODIES: Cell<u64> = const { Cell::new(0) };
    /// Times a row `Text` value was built. A held row must not increase this.
    static ROW_TEXT_BUILDS: Cell<u64> = const { Cell::new(0) };
}

/// How many times this thread ran the row-sync body.
#[cfg(test)]
pub(super) fn row_sync_bodies() -> u64 {
    ROW_SYNC_BODIES.with(Cell::get)
}

/// How many row `Text` values this thread has built.
#[cfg(test)]
pub(super) fn row_text_builds() -> u64 {
    ROW_TEXT_BUILDS.with(Cell::get)
}

/// Paint and layout generation last written into caption row nodes.
#[derive(Resource)]
struct CaptionRowsSynced {
    content: u64,
    rows: usize,
    text_color: String,
    background: CaptionBackground,
    opacity_bits: u32,
    speaker_colors: bool,
    theme: CaptionThemeStyle,
}

/// Outline width (UI px) when the theme sets no `subtitle.outline-width`.
pub const CAPTION_OUTLINE_WIDTH: f32 = 2.0;

/// A theme's caption `subtitle` style (spec 36 § 8.3, ADR-7208, ADR-9340), as the caption rows
/// read it; a token the theme leaves unset is `None` (`ui.theme_get` reports it as `caption`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CaptionThemeStyle {
    /// `subtitle.text`: the text color while the player keeps the default (white).
    pub text: Option<UiColor>,
    /// `subtitle.plate`: the box plate color (its alpha multiplies the player's box opacity).
    pub plate: Option<UiColor>,
    /// `subtitle.plate-opacity`: the lowest box opacity.
    pub plate_opacity: Option<f32>,
    /// `subtitle.font`: the caption font family (`None` = the UI font).
    pub font: Option<String>,
    /// `subtitle.size`: multiplies the caption text size on top of the player's `text_scale`.
    pub size: Option<f32>,
    /// `subtitle.outline`: the `Outline` background's stroke color (black when unset).
    pub outline: Option<UiColor>,
    /// `subtitle.outline-width`: the stroke width in UI px ([`CAPTION_OUTLINE_WIDTH`] when
    /// unset).
    pub outline_width: Option<f32>,
}

impl CaptionThemeStyle {
    /// The style `theme` defines.
    pub fn of_theme(theme: &crate::theme_file::ResolvedTheme) -> Self {
        Self {
            text: theme.color("subtitle.text"),
            plate: theme.color("subtitle.plate"),
            plate_opacity: theme.number("subtitle.plate-opacity"),
            font: theme
                .text("subtitle.font")
                .filter(|f| !f.trim().is_empty())
                .map(str::to_owned),
            size: theme.number("subtitle.size"),
            outline: theme.color("subtitle.outline"),
            outline_width: theme.number("subtitle.outline-width"),
        }
    }

    /// The active theme's style (the default when no theme is resolved).
    fn of(world: &World) -> Self {
        world
            .get_resource::<crate::theme_resolve::ThemeResolver>()
            .and_then(|r| r.resolved_theme())
            .map(Self::of_theme)
            .unwrap_or_default()
    }

    /// The text effect layers of a row with `background` in force: one outline for `Outline`.
    fn effects(&self, background: CaptionBackground) -> Vec<crate::text_effects::TextEffect> {
        if background != CaptionBackground::Outline {
            return Vec::new();
        }
        let width = self
            .outline_width
            .filter(|w| w.is_finite())
            .unwrap_or(CAPTION_OUTLINE_WIDTH)
            .clamp(0.0, crate::text_effects::TEXT_EFFECT_MAX_PX);
        let color = self.outline.unwrap_or(UiColor([0.0, 0.0, 0.0, 1.0]));
        vec![crate::text_effects::TextEffect::outline(width, color)]
    }

    /// The row text color: the theme's while the player keeps the default text color.
    fn text_color(&self, opts: &CaptionOptions) -> UiColor {
        match self.text {
            Some(c) if opts.text_color == CaptionOptions::default().text_color => c,
            _ => ui_color(opts.text_rgb()),
        }
    }

    /// The plate fill of a row with `background` in force.
    fn plate(&self, opts: &CaptionOptions, background: CaptionBackground) -> UiColor {
        match background {
            CaptionBackground::Box => {
                let plate = self.plate.unwrap_or(UiColor([0.0, 0.0, 0.0, 1.0]));
                let floor = self.plate_opacity.unwrap_or(0.0).clamp(0.0, 1.0);
                let opacity = opts.background_opacity.clamp(0.0, 1.0).max(floor);
                // The alpha is quantized to 8 bits as the plate always was, so a black plate at
                // the player's opacity is bit for bit the historic one.
                let alpha = (plate.0[3].clamp(0.0, 1.0) * opacity * 255.0).round() as u8;
                UiColor([plate.0[0], plate.0[1], plate.0[2], f32::from(alpha) / 255.0])
            }
            CaptionBackground::None | CaptionBackground::Outline => UiColor::TRANSPARENT,
        }
    }
}

impl CaptionRowsSynced {
    fn matches(&self, opts: &CaptionOptions, content: u64, rows: usize, theme: &CaptionThemeStyle) -> bool {
        self.theme == *theme
            && self.content == content
            && self.rows == rows
            && self.text_color == opts.text_color
            && self.background == opts.effective_background()
            && self.opacity_bits == opts.background_opacity.to_bits()
            && self.speaker_colors == opts.speaker_colors
    }
}

/// The label span text: `Label: ` before the text on the reading-start side, ` \u{2014} Label`
/// after it on the reading-end side (`Speaker.side`, spec 56 § 2.1).
fn label_text(label: &str, side: LabelSide) -> String {
    match side {
        LabelSide::Start => format!("{label}: "),
        LabelSide::End => format!(" \u{2014} {label}"),
    }
}

fn label_span(row: &RowView, size: &TextStyle, label_color: Option<UiColor>) -> Option<TextSpan> {
    let label = row.label.as_ref()?;
    let mut style = size.clone();
    if let Some(color) = label_color {
        style.color = color;
    }
    style.bold = true;
    Some(TextSpan::lit(label_text(label, row.label_side)).with_style(style))
}

fn row_align(row: &RowView) -> TextAlign {
    if row.rtl {
        TextAlign::Start
    } else {
        TextAlign::Center
    }
}

/// The row text: the body between the label spans of the row's label side. The label stays in
/// logical order, so for a right-to-left row the start side is the right.
pub(super) fn row_text(row: &RowView, size: &TextStyle, label_color: Option<UiColor>) -> Text {
    assemble_row(
        row,
        size,
        label_color,
        vec![TextSpan::lit(row.lines.join("\n")).with_style(size.clone())],
    )
}

fn assemble_row(row: &RowView, size: &TextStyle, label_color: Option<UiColor>, body: Vec<TextSpan>) -> Text {
    let label = label_span(row, size, label_color);
    let mut spans = Vec::with_capacity(body.len() + 1);
    let end = row.label_side == LabelSide::End;
    if !end {
        spans.extend(label.clone());
    }
    spans.extend(body);
    if end {
        spans.extend(label);
    }
    Text {
        spans,
        align: row_align(row),
        wrap: WrapMode::None,
        ..Default::default()
    }
}

/// The stored row text is already `row` (label, lines, size, color, direction), with no extra spans.
fn row_text_matches(text: &Text, row: &RowView, body: &TextStyle, label_color: Option<UiColor>) -> bool {
    if text.align != row_align(row) || text.wrap != WrapMode::None {
        return false;
    }
    let label = label_span(row, body, label_color);
    let label_matches = |span: Option<&TextSpan>| match (&label, span) {
        (Some(expected), Some(span)) => {
            span.style == expected.style
                && matches!((&span.content, &expected.content), (TextContent::Lit(a), TextContent::Lit(b)) if a == b)
        }
        _ => false,
    };
    let end = row.label_side == LabelSide::End;
    let mut spans = text.spans.iter();
    if label.is_some() && !end && !label_matches(spans.next()) {
        return false;
    }
    let Some(span) = spans.next() else {
        return false;
    };
    if label.is_some() && end && !label_matches(spans.next()) {
        return false;
    }
    if spans.next().is_some() || span.style != *body {
        return false;
    }
    let TextContent::Lit(lit) = &span.content else {
        return false;
    };
    let mut rest = lit.as_str();
    for (i, line) in row.lines.iter().enumerate() {
        if i > 0 {
            let Some(next) = rest.strip_prefix('\n') else {
                return false;
            };
            rest = next;
        }
        let Some(next) = rest.strip_prefix(line.as_str()) else {
            return false;
        };
        rest = next;
    }
    rest.is_empty()
}

fn ui_color(rgb: Rgb) -> UiColor {
    UiColor::from_srgb8(rgb.0, rgb.1, rgb.2, 255)
}

fn block_root(world: &mut World) -> Entity {
    let found = world
        .query_filtered::<Entity, With<CaptionBlock>>()
        .iter(world)
        .min_by_key(|e| e.to_bits());
    if let Some(root) = found {
        return root;
    }
    world
        .spawn((
            CaptionBlock,
            NodeName("CaptionBlock".to_string()),
            UiRoot {
                scale_mode: ScaleMode::ConstantPhysicalSize,
                sort_order: CAPTION_SORT_ORDER,
                ..Default::default()
            },
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(1.0), Dimension::Percent(1.0)],
                ..Default::default()
            }),
            Accessible::default()
                .with_role(A11yRole::Status)
                .with_live(Politeness::Polite),
        ))
        .id()
}

fn paint_unchanged(world: &mut World, content: u64, rows: usize) -> bool {
    if world.get_resource::<CaptionRowsSynced>().is_none() || world.get_resource::<CaptionOptions>().is_none()
    {
        return false;
    }
    let theme = CaptionThemeStyle::of(world);
    world.resource_scope(|world, synced: Mut<CaptionRowsSynced>| {
        world
            .get_resource::<CaptionOptions>()
            .is_some_and(|opts| synced.matches(opts, content, rows, &theme))
    })
}

/// The karaoke split of a row body: (spoken prefix, rest, spoken color), or `None` when karaoke is
/// off or the row has no timing. The text is unchanged (the two pieces concatenate to `body`), and
/// only the color differs, so the glyph advances and the line breaks stay those of the plain row.
fn karaoke_split(world: &World, id: CaptionId, body: &str, text: Rgb) -> Option<(String, String, Rgb)> {
    if !world.get_resource::<super::DialogueCaptionSettings>()?.karaoke {
        return None;
    }
    let board = world.get_resource::<CaptionBoard>()?;
    let (from, to) = board.page_window(id)?;
    let words = word_timings(body, from, to);
    let prefix = highlighted_prefix(&words, board.clock_us());
    if prefix == 0 {
        return None;
    }
    // Yellow on the dark plate; cyan when the text color already is yellow-ish.
    let spoken = if text.0 > 200 && text.1 > 200 && text.2 < 120 {
        Rgb(80, 220, 255)
    } else {
        Rgb(255, 214, 64)
    };
    Some((body[..prefix].to_string(), body[prefix..].to_string(), spoken))
}

/// Row text with the dialogue decorations (ADR-3234): an edge arrow before the body when the
/// anchored caption points off screen, and the karaoke split of the body. Plain rows use
/// [`row_text`].
fn decorated_row_text(
    row: &RowView,
    size: &TextStyle,
    label_color: Option<UiColor>,
    arrow: Option<f32>,
    karaoke: Option<(String, String, Rgb)>,
) -> Text {
    let mut body = Vec::with_capacity(3);
    if let Some(angle) = arrow {
        body.push(TextSpan::lit(format!("{} ", arrow_glyph(angle))).with_style(size.clone()));
    }
    match karaoke {
        Some((spoken, rest, spoken_color)) => {
            let mut hi = size.clone();
            hi.color = ui_color(spoken_color);
            body.push(TextSpan::lit(spoken).with_style(hi));
            if !rest.is_empty() {
                body.push(TextSpan::lit(rest).with_style(size.clone()));
            }
        }
        None => body.push(TextSpan::lit(row.lines.join("\n")).with_style(size.clone())),
    }
    assemble_row(row, size, label_color, body)
}

/// Exclusive system: makes the nodes under `CaptionBlock` match the board's visible rows.
pub fn sync_caption_rows(world: &mut World) {
    let _span = tracing::trace_span!("caption_rows").entered();
    let (content, visible) = world
        .get_resource::<CaptionBoard>()
        .map(|board| (board.content_gen(), board.visible_count()))
        .unwrap_or((0, 0));
    let existing_count = world.query::<&CaptionRow>().iter(world).count();
    // Nothing to show and nothing spawned: stay inert (no root, no options clone).
    if visible == 0 && existing_count == 0 {
        return;
    }
    // Karaoke and anchored placement move rows without a board content change (ADR-3234).
    let decorated = world
        .get_resource::<super::DialogueCaptionSettings>()
        .is_some_and(|s| s.karaoke)
        || world
            .get_resource::<CaptionAnchorState>()
            .is_some_and(|s| s.active);
    if !decorated && paint_unchanged(world, content, existing_count) {
        return;
    }
    ROW_SYNC_BODIES.with(|n| n.set(n.get().wrapping_add(1)));
    let Some(opts) = world.get_resource::<CaptionOptions>().cloned() else {
        return;
    };
    let rows = match world.get_resource::<CaptionBoard>() {
        Some(board) => board.visible_rows(&opts),
        None => return,
    };
    let existing: BTreeMap<CaptionId, Entity> = world
        .query::<(Entity, &CaptionRow)>()
        .iter(world)
        .map(|(e, r)| (r.0, e))
        .collect();
    if rows.is_empty() && existing.is_empty() {
        return;
    }
    let root = block_root(world);
    let mut changed = false;
    for (id, entity) in &existing {
        if !rows.iter().any(|r| r.id == *id) {
            world.entity_mut(*entity).despawn();
            changed = true;
        }
    }
    let text_rgb = opts.text_rgb();
    let background = opts.effective_background();
    let theme = CaptionThemeStyle::of(world);
    let text_color = theme.text_color(&opts);
    let fill = theme.plate(&opts, background);
    let effects = theme.effects(background);
    let anchored = world
        .get_resource::<CaptionAnchorState>()
        .filter(|s| s.active)
        .cloned();
    for row in &rows {
        let placed = anchored.as_ref().and_then(|s| s.get(row.id)).copied();
        let mut row = row.clone();
        if let Some(p) = placed {
            row.rect = p.rect;
        }
        let row = &row;
        let size = TextStyle {
            font_px: row.size_px,
            color: text_color,
            family: theme.font.clone(),
            effects: effects.clone(),
            ..TextStyle::default()
        };
        let label_color = row.label_color.map(ui_color);
        let arrow = match placed {
            Some(AnchorPlacement {
                mode: AnchorMode::EdgeArrow(arrow),
                ..
            }) => Some(arrow.angle),
            _ => None,
        };
        let karaoke = karaoke_split(world, row.id, &row.lines.join("\n"), text_rgb);
        // A decorated row (edge arrow, karaoke split) is rebuilt every pass; a plain row keeps its
        // stored text when it already matches (the ui1 early-out).
        let plain = arrow.is_none() && karaoke.is_none();
        let kept = plain
            && existing.get(&row.id).is_some_and(|&entity| {
                let child = world
                    .get::<Children>(entity)
                    .and_then(|children| children.iter().next());
                child.is_some_and(|child| {
                    world
                        .get::<Text>(child)
                        .is_some_and(|text| row_text_matches(text, row, &size, label_color))
                })
            });
        let text = if kept {
            None
        } else {
            ROW_TEXT_BUILDS.with(|n| n.set(n.get().wrapping_add(1)));
            Some(if plain {
                row_text(row, &size, label_color)
            } else {
                decorated_row_text(row, &size, label_color, arrow, karaoke)
            })
        };
        let style = Style {
            position: PositionType::Absolute,
            inset: Edges {
                left: Dimension::Points(row.rect.x),
                top: Dimension::Points(row.rect.y),
                right: Dimension::Auto,
                bottom: Dimension::Auto,
            },
            size: [Dimension::Points(row.rect.w), Dimension::Points(row.rect.h)],
            padding: Edges::px(row.size_px * 0.25),
            ..Default::default()
        };
        match existing.get(&row.id) {
            Some(&entity) => {
                let mut entity_ref = world.entity_mut(entity);
                let mut touched = false;
                if let Some(mut s) = entity_ref.get_mut::<NodeStyle>()
                    && s.0 != style
                {
                    s.0 = style;
                    touched = true;
                }
                if let Some(mut b) = entity_ref.get_mut::<Background>() {
                    let want = BackgroundKind::Color(fill);
                    if b.kind != want {
                        b.kind = want;
                    }
                }
                let children: Vec<Entity> = entity_ref
                    .get::<Children>()
                    .map(|c| c.iter().collect())
                    .unwrap_or_default();
                if let Some(text) = text
                    && let Some(child) = children.first().copied()
                    && let Some(mut t) = world.get_mut::<Text>(child)
                {
                    *t = text;
                    touched = true;
                }
                changed |= touched;
            }
            None => {
                let text = text.unwrap_or_else(|| row_text(row, &size, label_color));
                let entity = world
                    .spawn((
                        CaptionRow(row.id),
                        UiNode,
                        NodeStyle::new(style),
                        Background {
                            kind: BackgroundKind::Color(fill),
                        },
                    ))
                    .with_children(|r| {
                        r.spawn((UiNode, text));
                    })
                    .id();
                world.entity_mut(root).add_child(entity);
                changed = true;
            }
        }
    }
    if changed && let Some(mut dirty) = world.get_resource_mut::<LayoutDirty>() {
        dirty.mark();
    }
    world.insert_resource(CaptionRowsSynced {
        content,
        rows: rows.len(),
        text_color: opts.text_color,
        background,
        opacity_bits: opts.background_opacity.to_bits(),
        speaker_colors: opts.speaker_colors,
        theme,
    });
}
