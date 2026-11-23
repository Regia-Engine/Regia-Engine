// SPDX-License-Identifier: MPL-2.0
//! Editor accessibility: accessible names for icon and unlabeled widgets, the WCAG 2.x contrast of
//! the editor theme, and an AccessKit audit for tests (ADR-9810;
//! `docs/evidence/editor-a11y-2026-11.md`, `book/user-guide/editor-guide/accessibility.md`).
//!
//! egui gives every widget an AccessKit node with a role and, for a button or a checkbox, its
//! visible text as the name. A glyph button (`x`, `−`, `↻`, `▾`) is named by its glyph and a
//! checkbox drawn with an empty text after a separate label has no name at all; [`name`] gives
//! such a widget a spoken name and the same hover text, and [`unnamed_widgets`] lists the
//! interactive nodes of a frame's AccessKit tree that still have none.
//!
//! [`editor_contrast`] measures text and control colors of a [`ThemeTokens`] against the
//! surfaces they are drawn on: 4.5:1 for text (WCAG 2.x 1.4.3, level AA) and 3:1 for the outline
//! of a control (1.4.11). `editor.a11y_contrast` answers the same table for a theme.

use egui::Color32;
use egui::accesskit::{Node, Role, TreeUpdate};
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::theme::ThemeTokens;

/// An editor agent handler.
type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// WCAG 2.x AA contrast for body text.
pub const TEXT_AA: f32 = 4.5;
/// WCAG 2.x AA contrast for a control's boundary or a graphical object (1.4.11).
pub const NON_TEXT_AA: f32 = 3.0;
/// WCAG 2.x AAA contrast for body text (1.4.6), the high-contrast theme's bar.
pub const TEXT_AAA: f32 = 7.0;
/// WCAG 2.x AAA contrast for large text (1.4.6: 18 pt, or 14 pt bold).
pub const LARGE_TEXT_AAA: f32 = 4.5;

/// Gives `response`'s widget the accessible name `name` (its AccessKit label) and `name` as hover
/// text, keeping its role. Use it for glyph buttons and for a checkbox or field whose visible
/// label is a separate widget.
pub fn name(response: egui::Response, kind: egui::WidgetType, name: &str) -> egui::Response {
    let enabled = response.enabled();
    response.widget_info(|| egui::WidgetInfo::labeled(kind, enabled, name));
    response.on_hover_text(name)
}

/// A small glyph button (`glyph` drawn, `spoken` its accessible name and hover text).
pub fn glyph_button(ui: &mut egui::Ui, glyph: &str, spoken: &str) -> egui::Response {
    let response = ui.small_button(glyph);
    name(response, egui::WidgetType::Button, spoken)
}

/// A checkbox with no visible text, named `spoken` (the row's label beside it).
pub fn bare_checkbox(ui: &mut egui::Ui, checked: &mut bool, spoken: &str) -> egui::Response {
    let response = ui.checkbox(checked, "");
    let enabled = response.enabled();
    let selected = *checked;
    response
        .widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, selected, spoken));
    response.on_hover_text(spoken)
}

/// A colored status glyph ("⚠") whose meaning is `text`: the glyph stays, and the spoken text
/// and hover text carry the message instead of hover alone (FU-7: the matgraph and procedural
/// graph headers). The node keeps the `Label` role: it is static text that a screen reader reads
/// in place (browse mode, or Tab when egui's `screen_reader` option makes labels focusable), not
/// a button that does nothing when activated (WCAG 4.1.2, ADR-8870). [`label_names`] lists it.
pub fn warning_glyph(ui: &mut egui::Ui, color: Color32, text: &str) -> egui::Response {
    let response = ui.colored_label(color, "\u{26a0}");
    name(response, egui::WidgetType::Label, text)
}

/// Names a drag value (FU-8: "Half extent x"), keeping the number it shows so a screen reader
/// reads name and value.
pub fn named_drag_value(response: egui::Response, name: &str, value: f64) -> egui::Response {
    let enabled = response.enabled();
    response.widget_info(|| {
        let mut info = egui::WidgetInfo::drag_value(enabled, value);
        info.label = Some(name.to_owned());
        info
    });
    response.on_hover_text(name)
}

/// Names `response`'s widget for screen readers without hover text: a pane-sized tooltip would
/// follow the pointer across the whole widget (FU-9: the focusable viewport panes).
pub fn spoken_only(response: egui::Response, kind: egui::WidgetType, name: &str) -> egui::Response {
    let enabled = response.enabled();
    response.widget_info(|| egui::WidgetInfo::labeled(kind, enabled, name.to_owned()));
    response
}

/// `fg` drawn over the opaque `bg`: egui colors are premultiplied, blended in gamma space.
#[must_use]
pub fn over(fg: Color32, bg: Color32) -> Color32 {
    let keep = 255 - u16::from(fg.a());
    let mix = |f: u8, b: u8| -> u8 { (u16::from(f) + (u16::from(b) * keep + 127) / 255).min(255) as u8 };
    Color32::from_rgb(mix(fg.r(), bg.r()), mix(fg.g(), bg.g()), mix(fg.b(), bg.b()))
}

/// WCAG 2.x relative luminance of an opaque sRGB color.
#[must_use]
pub fn relative_luminance(color: Color32) -> f32 {
    let channel = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
}

/// WCAG 2.x contrast ratio (1 to 21) of `fg` drawn over the opaque `bg`.
#[must_use]
pub fn contrast_ratio(fg: Color32, bg: Color32) -> f32 {
    let a = relative_luminance(over(fg, bg));
    let b = relative_luminance(bg);
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// One measured color pair of the editor theme.
#[derive(Debug, Clone, PartialEq)]
pub struct ContrastRow {
    /// `foreground on background` in token names (`text on panel`).
    pub pair: &'static str,
    /// Foreground as `#rrggbbaa` (premultiplied, as stored).
    pub fg: String,
    /// Background as `#rrggbb`.
    pub bg: String,
    /// The measured ratio.
    pub ratio: f32,
    /// The AA requirement ([`TEXT_AA`] or [`NON_TEXT_AA`]).
    pub required: f32,
}

impl ContrastRow {
    /// Whether the pair meets its requirement.
    #[must_use]
    pub fn passes(&self) -> bool {
        self.ratio >= self.required
    }
}

fn hex(c: Color32) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", c.r(), c.g(), c.b(), c.a())
}

/// Picks one color of a theme.
type Pick = fn(&ThemeTokens) -> Color32;

/// What a control's boundary is drawn with (`theme::visuals`): its `widgets.edge` outline; in a
/// theme without one, the outline of a panel-filled control (`theme::idle_outline`, ADR-8589) or
/// else the button fill itself (egui buttons have no outline at rest).
fn control_boundary(t: &ThemeTokens) -> Color32 {
    let w = &t.widgets;
    if w.edge.a() > 0 {
        w.edge
    } else if w.idle == t.panel {
        crate::theme::idle_outline(t)
    } else {
        w.idle
    }
}

/// The keyboard focus ring: Signal in a theme with control outlines (`theme::visuals`), else egui's
/// pressed outline (`strong`).
fn focus_ring(t: &ThemeTokens) -> Color32 {
    if t.widgets.edge.a() > 0 {
        t.signal
    } else {
        t.strong
    }
}

/// Number of rows of [`editor_contrast`].
pub const CONTRAST_PAIRS: usize = 83;

/// Every pair [`editor_contrast`] measures: name, foreground, background, requirement. The first 18
/// are the November 2026 audit's rows (same names); the rest port the pair list of the PiercingXX
/// UI kit's contrast check (`piercingxx-branding/ui/scripts/pxx/core.py`, `pairs()`; ADR-8585)
/// onto the editor's tokens: each text role on each surface it is drawn on (the ground, panels,
/// windows, fields and the rest, pressed and open control fills; a hovered control draws only its
/// own `hover_text`, so the hover fill pairs with that alone), Signal as link and active text, the inverted
/// selection, every functional color on every surface, and the non-text marks (control outlines,
/// the focus ring, the selection block, the play-session tints) at 3:1.
const PAIRS: [(&str, Pick, Pick, f32); CONTRAST_PAIRS] = [
    ("text on panel", |t| t.text, |t| t.panel, TEXT_AA),
    ("strong on panel", |t| t.strong, |t| t.panel, TEXT_AA),
    ("muted on panel", |t| t.muted, |t| t.panel, TEXT_AA),
    ("text on well", |t| t.text, |t| t.well, TEXT_AA),
    (
        "button_text on idle",
        |t| t.widgets.button_text,
        |t| t.widgets.idle,
        TEXT_AA,
    ),
    (
        "hover_text on hover",
        |t| t.widgets.hover_text,
        |t| t.widgets.hover,
        TEXT_AA,
    ),
    (
        "open_text on open",
        |t| t.widgets.open_text,
        |t| t.widgets.open,
        TEXT_AA,
    ),
    ("on_signal on signal", |t| t.on_signal, |t| t.signal, TEXT_AA),
    ("link on panel", |t| t.widgets.link, |t| t.panel, TEXT_AA),
    ("warn on panel", |t| t.warn, |t| t.panel, TEXT_AA),
    ("error on panel", |t| t.error, |t| t.panel, TEXT_AA),
    (
        "accents.error_text on panel",
        |t| t.accents.error_text,
        |t| t.panel,
        TEXT_AA,
    ),
    ("accents.ok on panel", |t| t.accents.ok, |t| t.panel, TEXT_AA),
    (
        "accents.unsaved on panel",
        |t| t.accents.unsaved,
        |t| t.panel,
        TEXT_AA,
    ),
    (
        "accents.log_error on panel",
        |t| t.accents.log_error,
        |t| t.panel,
        TEXT_AA,
    ),
    (
        "accents.log_warn on panel",
        |t| t.accents.log_warn,
        |t| t.panel,
        TEXT_AA,
    ),
    (
        "accents.log_info on panel",
        |t| t.accents.log_info,
        |t| t.panel,
        TEXT_AA,
    ),
    ("idle on panel", control_boundary, |t| t.panel, NON_TEXT_AA),
    ("text on ground", |t| t.text, |t| t.ground, TEXT_AA),
    ("text on raised", |t| t.text, |t| t.raised, TEXT_AA),
    ("text on idle", |t| t.text, |t| t.widgets.idle, TEXT_AA),
    ("text on pressed", |t| t.text, |t| t.widgets.pressed, TEXT_AA),
    ("text on open", |t| t.text, |t| t.widgets.open, TEXT_AA),
    ("strong on ground", |t| t.strong, |t| t.ground, TEXT_AA),
    ("strong on raised", |t| t.strong, |t| t.raised, TEXT_AA),
    ("strong on well", |t| t.strong, |t| t.well, TEXT_AA),
    ("strong on idle", |t| t.strong, |t| t.widgets.idle, TEXT_AA),
    ("strong on pressed", |t| t.strong, |t| t.widgets.pressed, TEXT_AA),
    ("strong on open", |t| t.strong, |t| t.widgets.open, TEXT_AA),
    ("muted on ground", |t| t.muted, |t| t.ground, TEXT_AA),
    ("muted on raised", |t| t.muted, |t| t.raised, TEXT_AA),
    ("muted on well", |t| t.muted, |t| t.well, TEXT_AA),
    ("muted on idle", |t| t.muted, |t| t.widgets.idle, TEXT_AA),
    ("muted on pressed", |t| t.muted, |t| t.widgets.pressed, TEXT_AA),
    ("muted on open", |t| t.muted, |t| t.widgets.open, TEXT_AA),
    ("signal on ground", |t| t.signal, |t| t.ground, TEXT_AA),
    ("signal on panel", |t| t.signal, |t| t.panel, TEXT_AA),
    ("signal on raised", |t| t.signal, |t| t.raised, TEXT_AA),
    ("signal on well", |t| t.signal, |t| t.well, TEXT_AA),
    ("link on raised", |t| t.widgets.link, |t| t.raised, TEXT_AA),
    ("warn on ground", |t| t.warn, |t| t.ground, TEXT_AA),
    ("warn on raised", |t| t.warn, |t| t.raised, TEXT_AA),
    ("warn on well", |t| t.warn, |t| t.well, TEXT_AA),
    ("error on ground", |t| t.error, |t| t.ground, TEXT_AA),
    ("error on raised", |t| t.error, |t| t.raised, TEXT_AA),
    ("error on well", |t| t.error, |t| t.well, TEXT_AA),
    ("accents.ok on ground", |t| t.accents.ok, |t| t.ground, TEXT_AA),
    ("accents.ok on raised", |t| t.accents.ok, |t| t.raised, TEXT_AA),
    ("accents.ok on well", |t| t.accents.ok, |t| t.well, TEXT_AA),
    (
        "accents.log_info on ground",
        |t| t.accents.log_info,
        |t| t.ground,
        TEXT_AA,
    ),
    (
        "accents.log_info on raised",
        |t| t.accents.log_info,
        |t| t.raised,
        TEXT_AA,
    ),
    (
        "accents.log_info on well",
        |t| t.accents.log_info,
        |t| t.well,
        TEXT_AA,
    ),
    (
        "accents.alert on ground",
        |t| t.accents.alert,
        |t| t.ground,
        TEXT_AA,
    ),
    (
        "accents.alert on panel",
        |t| t.accents.alert,
        |t| t.panel,
        TEXT_AA,
    ),
    (
        "accents.alert on raised",
        |t| t.accents.alert,
        |t| t.raised,
        TEXT_AA,
    ),
    ("accents.alert on well", |t| t.accents.alert, |t| t.well, TEXT_AA),
    (
        "accents.error_text on ground",
        |t| t.accents.error_text,
        |t| t.ground,
        TEXT_AA,
    ),
    (
        "accents.error_text on raised",
        |t| t.accents.error_text,
        |t| t.raised,
        TEXT_AA,
    ),
    (
        "accents.error_text on well",
        |t| t.accents.error_text,
        |t| t.well,
        TEXT_AA,
    ),
    (
        "accents.unsaved on ground",
        |t| t.accents.unsaved,
        |t| t.ground,
        TEXT_AA,
    ),
    (
        "accents.unsaved on raised",
        |t| t.accents.unsaved,
        |t| t.raised,
        TEXT_AA,
    ),
    (
        "accents.unsaved on well",
        |t| t.accents.unsaved,
        |t| t.well,
        TEXT_AA,
    ),
    (
        "accents.log_error on ground",
        |t| t.accents.log_error,
        |t| t.ground,
        TEXT_AA,
    ),
    (
        "accents.log_error on raised",
        |t| t.accents.log_error,
        |t| t.raised,
        TEXT_AA,
    ),
    (
        "accents.log_error on well",
        |t| t.accents.log_error,
        |t| t.well,
        TEXT_AA,
    ),
    (
        "accents.log_warn on ground",
        |t| t.accents.log_warn,
        |t| t.ground,
        TEXT_AA,
    ),
    (
        "accents.log_warn on raised",
        |t| t.accents.log_warn,
        |t| t.raised,
        TEXT_AA,
    ),
    (
        "accents.log_warn on well",
        |t| t.accents.log_warn,
        |t| t.well,
        TEXT_AA,
    ),
    ("edge on ground", control_boundary, |t| t.ground, NON_TEXT_AA),
    ("edge on raised", control_boundary, |t| t.raised, NON_TEXT_AA),
    ("edge on well", control_boundary, |t| t.well, NON_TEXT_AA),
    ("focus on ground", focus_ring, |t| t.ground, NON_TEXT_AA),
    ("focus on panel", focus_ring, |t| t.panel, NON_TEXT_AA),
    ("focus on raised", focus_ring, |t| t.raised, NON_TEXT_AA),
    ("focus on well", focus_ring, |t| t.well, NON_TEXT_AA),
    ("selection block on panel", |t| t.signal, |t| t.panel, NON_TEXT_AA),
    (
        "hover_line on panel",
        |t| t.widgets.hover_line,
        |t| t.panel,
        NON_TEXT_AA,
    ),
    (
        "accents.play_mode on panel",
        |t| t.accents.play_mode,
        |t| t.panel,
        NON_TEXT_AA,
    ),
    (
        "accents.play_server on panel",
        |t| t.accents.play_server,
        |t| t.panel,
        NON_TEXT_AA,
    ),
    (
        "accents.play_client_1 on panel",
        |t| t.accents.play_clients[0],
        |t| t.panel,
        NON_TEXT_AA,
    ),
    (
        "accents.play_client_2 on panel",
        |t| t.accents.play_clients[1],
        |t| t.panel,
        NON_TEXT_AA,
    ),
    (
        "accents.play_client_3 on panel",
        |t| t.accents.play_clients[2],
        |t| t.panel,
        NON_TEXT_AA,
    ),
    (
        "accents.play_client_4 on panel",
        |t| t.accents.play_clients[3],
        |t| t.panel,
        NON_TEXT_AA,
    ),
];

/// The editor theme's text and control pairs (`PAIRS`), measured. Text pairs need [`TEXT_AA`]
/// (body and large text alike: the editor has no large-text exception); control boundaries, the
/// focus ring and marks need [`NON_TEXT_AA`].
#[must_use]
pub fn editor_contrast(t: &ThemeTokens) -> Vec<ContrastRow> {
    PAIRS
        .iter()
        .map(|&(pair, fg, bg, required)| {
            let (fg, bg) = (fg(t), over(bg(t), Color32::BLACK));
            ContrastRow {
                pair,
                fg: hex(fg),
                bg: hex(bg),
                ratio: contrast_ratio(fg, bg),
                required,
            }
        })
        .collect()
}

/// The egui widget states and the editor's text-bearing color slots of a theme, measured against
/// the panel fill (ADR-8589). For each of egui's widget states (`noninteractive`, `inactive`,
/// `hovered`, `active`, `open`) the text (`fg_stroke`) must read on the panel (frameless buttons,
/// menu items and selectable labels draw it straight on the panel) and on the state's own fills
/// ([`TEXT_AA`]); an interactive state's boundary (its fill, or its outline when it has one,
/// whichever stands out more) must stand out from the panel ([`NON_TEXT_AA`]). Then weak text,
/// warning, error and link colors, every accent the panels write text in, and the audio meter bars
/// over their track (`accents.ok`, `accents.unsaved`, `accents.alert` on `well`). Viewport overlay,
/// graph pin and play-tint colors are drawn over the 3D view or a canvas, not over the panel, and
/// are not measured here.
#[must_use]
pub fn widget_contrast(t: &ThemeTokens) -> Vec<ContrastRow> {
    let v = crate::theme::visuals(t);
    let panel = over(v.panel_fill, Color32::BLACK);
    let w = &v.widgets;
    let boundary = |state: &egui::style::WidgetVisuals| {
        let fill = over(state.weak_bg_fill, panel);
        let line = over(state.bg_stroke.color, panel);
        if state.bg_stroke.width > 0.0 && contrast_ratio(line, panel) > contrast_ratio(fill, panel) {
            line
        } else {
            fill
        }
    };
    let a = &t.accents;
    let weak = v.weak_text_color();
    let pairs: [(&'static str, Color32, Color32, f32); 37] = [
        (
            "noninteractive text on panel",
            w.noninteractive.fg_stroke.color,
            panel,
            TEXT_AA,
        ),
        (
            "noninteractive text on its fill",
            w.noninteractive.fg_stroke.color,
            over(w.noninteractive.bg_fill, panel),
            TEXT_AA,
        ),
        (
            "inactive text on panel",
            w.inactive.fg_stroke.color,
            panel,
            TEXT_AA,
        ),
        (
            "inactive text on its fill",
            w.inactive.fg_stroke.color,
            over(w.inactive.weak_bg_fill, panel),
            TEXT_AA,
        ),
        (
            "inactive text on its bg_fill",
            w.inactive.fg_stroke.color,
            over(w.inactive.bg_fill, panel),
            TEXT_AA,
        ),
        (
            "inactive boundary on panel",
            boundary(&w.inactive),
            panel,
            NON_TEXT_AA,
        ),
        ("hovered text on panel", w.hovered.fg_stroke.color, panel, TEXT_AA),
        (
            "hovered text on its fill",
            w.hovered.fg_stroke.color,
            over(w.hovered.weak_bg_fill, panel),
            TEXT_AA,
        ),
        (
            "hovered text on its bg_fill",
            w.hovered.fg_stroke.color,
            over(w.hovered.bg_fill, panel),
            TEXT_AA,
        ),
        (
            "hovered boundary on panel",
            boundary(&w.hovered),
            panel,
            NON_TEXT_AA,
        ),
        ("active text on panel", w.active.fg_stroke.color, panel, TEXT_AA),
        (
            "active text on its fill",
            w.active.fg_stroke.color,
            over(w.active.weak_bg_fill, panel),
            TEXT_AA,
        ),
        (
            "active text on its bg_fill",
            w.active.fg_stroke.color,
            over(w.active.bg_fill, panel),
            TEXT_AA,
        ),
        (
            "active boundary on panel",
            boundary(&w.active),
            panel,
            NON_TEXT_AA,
        ),
        ("open text on panel", w.open.fg_stroke.color, panel, TEXT_AA),
        (
            "open text on its fill",
            w.open.fg_stroke.color,
            over(w.open.weak_bg_fill, panel),
            TEXT_AA,
        ),
        ("weak text on panel", weak, panel, TEXT_AA),
        ("weak text on window", weak, over(v.window_fill, panel), TEXT_AA),
        (
            "text on text field",
            v.text_color(),
            over(v.extreme_bg_color, panel),
            TEXT_AA,
        ),
        ("warn_fg on panel", v.warn_fg_color, panel, TEXT_AA),
        ("error_fg on panel", v.error_fg_color, panel, TEXT_AA),
        ("hyperlink on panel", v.hyperlink_color, panel, TEXT_AA),
        (
            "selection text on selection",
            v.selection.stroke.color,
            over(v.selection.bg_fill, panel),
            TEXT_AA,
        ),
        ("strong on panel", t.strong, panel, TEXT_AA),
        ("muted on panel", t.muted, panel, TEXT_AA),
        ("accents.active on panel", a.active, panel, TEXT_AA),
        ("accents.select on panel", a.select, panel, TEXT_AA),
        ("accents.alert on panel", a.alert, panel, TEXT_AA),
        ("accents.ok on panel", a.ok, panel, TEXT_AA),
        ("accents.unsaved on panel", a.unsaved, panel, TEXT_AA),
        ("accents.error_text on panel", a.error_text, panel, TEXT_AA),
        ("accents.log_error on panel", a.log_error, panel, TEXT_AA),
        ("accents.log_warn on panel", a.log_warn, panel, TEXT_AA),
        ("accents.log_info on panel", a.log_info, panel, TEXT_AA),
        ("meter accents.ok on well", a.ok, over(t.well, panel), NON_TEXT_AA),
        (
            "meter accents.unsaved on well",
            a.unsaved,
            over(t.well, panel),
            NON_TEXT_AA,
        ),
        (
            "meter accents.alert on well",
            a.alert,
            over(t.well, panel),
            NON_TEXT_AA,
        ),
    ];
    pairs
        .into_iter()
        .map(|(pair, fg, bg, required)| ContrastRow {
            pair,
            fg: hex(fg),
            bg: hex(bg),
            ratio: contrast_ratio(fg, bg),
            required,
        })
        .collect()
}

/// The tokens of an editor theme choice: `default` (the ADR-229 look) or a built-in UI theme
/// (`regia-dark`, `regia-light`, with or without `ui:`).
///
/// # Errors
/// An unknown theme name.
pub fn theme_tokens(choice: &str) -> Result<ThemeTokens, String> {
    crate::theme::builtin_editor_theme(choice).map(|(tokens, _)| tokens)
}

/// The theme's pairs measured against WCAG AAA ([`editor_contrast`] with every text pair raised to
/// [`TEXT_AAA`]; the non-text pairs keep [`NON_TEXT_AA`], which AAA does not raise). The
/// high-contrast theme passes every row. Large text needs only [`LARGE_TEXT_AAA`], so a body-text
/// pass covers headings too.
#[must_use]
pub fn editor_contrast_aaa(t: &ThemeTokens) -> Vec<ContrastRow> {
    editor_contrast(t)
        .into_iter()
        .map(|mut row| {
            if row.required == TEXT_AA {
                row.required = TEXT_AAA;
            }
            row
        })
        .collect()
}

/// One text run drawn partly outside its clip rectangle (cut off on screen).
#[derive(Debug, Clone, PartialEq)]
pub struct ClippedText {
    /// The text.
    pub text: String,
    /// The text's bounds, points.
    pub bounds: egui::Rect,
    /// The clip rectangle it was drawn in, points.
    pub clip: egui::Rect,
}

/// The text shapes of a frame's output that extend past their clip rectangle by more than
/// `tolerance` points: text a person cannot read whole (the measure behind the text-scale check,
/// WCAG 1.4.4 "Resize text"). Text inside a scroll area that is scrolled out of view is clipped
/// on purpose and also counts, so measure panels at their top.
#[must_use]
pub fn clipped_text(shapes: &[egui::epaint::ClippedShape], tolerance: f32) -> Vec<ClippedText> {
    fn visit(shape: &egui::Shape, clip: egui::Rect, tolerance: f32, out: &mut Vec<ClippedText>) {
        match shape {
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    visit(shape, clip, tolerance, out);
                }
            }
            egui::Shape::Text(text) => {
                let bounds = text.visual_bounding_rect();
                let text_str = text.galley.text();
                if text_str.trim().is_empty() || !bounds.is_positive() {
                    return;
                }
                // Text fully outside its clip is not drawn at all (scrolled away, a closed
                // region); only text cut by the clip edge is unreadable.
                if !clip.intersects(bounds) {
                    return;
                }
                if !clip.expand(tolerance).contains_rect(bounds) {
                    out.push(ClippedText {
                        text: text_str.to_string(),
                        bounds,
                        clip,
                    });
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for clipped in shapes {
        visit(&clipped.shape, clipped.clip_rect, tolerance, &mut out);
    }
    out
}

/// Whether an AccessKit role is one a person operates (it must have a name).
fn interactive(role: Role) -> bool {
    matches!(
        role,
        Role::Button
            | Role::CheckBox
            | Role::RadioButton
            | Role::TextInput
            | Role::MultilineTextInput
            | Role::SpinButton
            | Role::Slider
            | Role::ComboBox
            | Role::Link
            | Role::Tab
            | Role::MenuItem
            | Role::Switch
    )
}

/// Whether `node` has a name: a non-empty label, or another node that labels it.
fn named(node: &Node) -> bool {
    node.label().is_some_and(|l| !l.trim().is_empty()) || !node.labelled_by().is_empty()
}

/// The interactive nodes of `update` with no accessible name, as `role` strings (`Button`,
/// `CheckBox`, ...), in node order.
#[must_use]
pub fn unnamed_widgets(update: &TreeUpdate) -> Vec<String> {
    update
        .nodes
        .iter()
        .filter(|(_, node)| interactive(node.role()) && !named(node))
        .map(|(_, node)| format!("{:?}", node.role()))
        .collect()
}

/// The names of the interactive nodes of `update` (empty string when unnamed), in node order.
#[must_use]
pub fn widget_names(update: &TreeUpdate) -> Vec<(String, String)> {
    update
        .nodes
        .iter()
        .filter(|(_, node)| interactive(node.role()))
        .map(|(_, node)| {
            (
                format!("{:?}", node.role()),
                node.label().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// The spoken text of the label nodes of `update` (AccessKit `Label` role), in node order. egui
/// stores a label's text as the node's value, so this reads the value and falls back to the
/// label. Static text that carries meaning (a "⚠" status glyph named by [`warning_glyph`]) is
/// checked here, not in [`widget_names`]: it is read in place, never operated (ADR-8870).
#[must_use]
pub fn label_names(update: &TreeUpdate) -> Vec<String> {
    update
        .nodes
        .iter()
        .filter(|(_, node)| node.role() == Role::Label)
        .map(|(_, node)| {
            node.value()
                .or_else(|| node.label())
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

/// `editor.a11y_contrast`.
fn editor_a11y_contrast(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (choice, tokens) = match params.get("theme").and_then(Value::as_str) {
        Some(choice) => {
            let tokens = theme_tokens(choice).map_err(|message| {
                RpcError::new(
                    INVALID_PARAMS,
                    "unknown_theme",
                    message,
                    "use default, amoled-night, paper, high-contrast, regia-dark or regia-light",
                )
            })?;
            (choice.to_string(), tokens)
        }
        None => (
            "active".to_string(),
            world
                .get_resource::<ThemeTokens>()
                .cloned()
                .unwrap_or_else(ThemeTokens::regia_default),
        ),
    };
    let rows = match params.get("level").and_then(Value::as_str) {
        None | Some("aa") => editor_contrast(&tokens),
        Some("aaa") => editor_contrast_aaa(&tokens),
        Some(other) => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "bad_level",
                format!("level `{other}` is not aa or aaa"),
                "use aa or aaa",
            ));
        }
    };
    let failing: Vec<&str> = rows.iter().filter(|r| !r.passes()).map(|r| r.pair).collect();
    let rows: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "pair": r.pair,
                "fg": r.fg,
                "bg": r.bg,
                "ratio": (r.ratio * 100.0).round() / 100.0,
                "required": r.required,
                "passes": r.passes(),
            })
        })
        .collect();
    let level = params.get("level").and_then(Value::as_str).unwrap_or("aa");
    Ok(json!({ "theme": choice, "level": level, "rows": rows, "failing": failing }))
}

/// The accessibility methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.a11y_contrast",
            tool: "a11y_contrast",
            description: "WCAG 2.x contrast of the editor theme: each text color against every surface it is drawn on (4.5:1, AA; 7:1 with level aaa) and the control outlines, focus ring and marks (3:1). Without `theme` the active editor theme; `default` (Piercing Graphite), `amoled-night`, `paper`, `high-contrast`, `regia-dark` or `regia-light` measures that one. Unknown names are unknown_theme.",
            params: &[
                ParamSchema {
                    name: "theme",
                    kind: "string",
                    required: false,
                    description: "\"default\" | \"amoled-night\" | \"paper\" | \"high-contrast\" | \"regia-dark\" | \"regia-light\" (also with \"ui:\"); default: the active theme.",
                },
                ParamSchema {
                    name: "level",
                    kind: "string",
                    required: false,
                    description: "\"aa\" (default) or \"aaa\" (text pairs need 7:1). Anything else is bad_level.",
                },
            ],
            result: "{ theme, level, rows: [{ pair, fg, bg, ratio, required, passes }], failing: [pair] }",
            mutating: false,
            permission: Permission::Read,
        },
        editor_a11y_contrast as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_matches_the_wcag_reference_values() {
        assert!((contrast_ratio(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 1e-3);
        assert!((contrast_ratio(Color32::WHITE, Color32::WHITE) - 1.0).abs() < 1e-6);
        // #767676 on white is the classic 4.54:1 boundary gray.
        let ratio = contrast_ratio(Color32::from_gray(0x76), Color32::WHITE);
        assert!((ratio - 4.54).abs() < 0.01, "{ratio}");
        // Half-transparent white over black is gray 128 (premultiplied 128, alpha 128).
        assert_eq!(
            over(Color32::from_white_alpha(128), Color32::BLACK),
            Color32::from_gray(128)
        );
    }

    /// FU-2 (docs/evidence/editor-a11y-2026-11.md § Contrast, ADR-8585): every built-in editor
    /// look (Piercing Graphite, AMOLED Night, Paper, high contrast) meets WCAG 2.x AA on every pair
    /// the editor draws (`PAIRS`, the UI kit's pair list on the editor tokens): 4.5:1 for text, 3:1
    /// for control outlines, the focus ring and marks. The report prints every row.
    #[test]
    fn every_built_in_editor_theme_meets_wcag_aa() {
        let mut report = String::new();
        let mut failed = false;
        for (theme, label) in crate::theme::BUILTIN_EDITOR_THEMES {
            let tokens = theme_tokens(theme).expect("built-in theme");
            // The token pairs, then every egui widget state as `theme::visuals` draws it (ADR-8589).
            for row in editor_contrast(&tokens)
                .into_iter()
                .chain(widget_contrast(&tokens))
            {
                let mark = if row.passes() { "pass" } else { "FAIL" };
                failed |= !row.passes();
                report.push_str(&format!(
                    "{label}: {} {:.2} (needs {}) {} on {} {mark}\n",
                    row.pair, row.ratio, row.required, row.fg, row.bg
                ));
            }
        }
        eprintln!("{report}");
        assert!(!failed, "{report}");
    }

    /// The bridged UI themes (FU-3): with accents and widget colors mapped from the UI roles, a pair
    /// that passes must keep passing; the failing pairs are listed here and in the evidence file.
    /// Fixing one makes this test fail on purpose: move the pair to the passing side.
    #[test]
    fn the_editor_themes_failing_pairs_are_the_listed_follow_ups() {
        // ADR-8589 mapped the UI themes' widget states and accents from their roles; the pairs
        // still failing on the full pair list (ADR-8585) are listed here (ADR-8679): regia-dark's
        // red (#E5534B) and outline (#5F6B7C) on its raised overlay surface (#262E3A, which the
        // bridge gives `ground` and `raised`); regia-light's link blue, red, green and outline on its
        // sunken field (#E3E8EE, 4.13-4.38:1 and 2.96:1); and the play-session tints, which keep the
        // default look's light people colors on the light panel. The brand looks pass every pair.
        let expected: [(&str, &[&str]); 2] = [
            (
                "regia-dark",
                &[
                    "error on ground",
                    "error on raised",
                    "accents.alert on ground",
                    "accents.alert on raised",
                    "accents.error_text on ground",
                    "accents.error_text on raised",
                    "accents.log_error on ground",
                    "accents.log_error on raised",
                    "edge on ground",
                    "edge on raised",
                ],
            ),
            (
                "regia-light",
                &[
                    "signal on well",
                    "error on well",
                    "accents.ok on well",
                    "accents.log_info on well",
                    "accents.alert on well",
                    "accents.error_text on well",
                    "accents.log_error on well",
                    "edge on well",
                    "accents.play_mode on panel",
                    "accents.play_server on panel",
                    "accents.play_client_1 on panel",
                    "accents.play_client_2 on panel",
                    "accents.play_client_3 on panel",
                    "accents.play_client_4 on panel",
                ],
            ),
        ];
        let mut report = String::new();
        let mut mismatched = false;
        for (theme, want) in expected {
            let tokens = theme_tokens(theme).expect("built-in theme");
            let rows = editor_contrast(&tokens);
            let failing: Vec<&str> = rows.iter().filter(|r| !r.passes()).map(|r| r.pair).collect();
            for row in &rows {
                report.push_str(&format!(
                    "{theme}: {} {:.2} (needs {}) {} on {}\n",
                    row.pair, row.ratio, row.required, row.fg, row.bg
                ));
            }
            if failing != want {
                mismatched = true;
                report.push_str(&format!("{theme}: failing {failing:?}, listed {want:?}\n"));
            }
        }
        assert!(!mismatched, "{report}");
    }

    /// Bug 8 of the 2026-11-16 all-host smoke run (ADR-8589): `regia-light` kept the dark theme's
    /// button, number-field and dropdown fills and text on white panels, and yellow "unsaved",
    /// orange warnings and the meters were near invisible. Every egui widget state and every
    /// text-bearing slot of both built-in UI themes meets WCAG AA against the panel.
    #[test]
    fn every_widget_state_and_slot_of_the_ui_themes_meets_aa() {
        let mut report = String::new();
        let mut failing = 0;
        for theme in ["regia-light", "regia-dark"] {
            let tokens = theme_tokens(theme).expect("built-in theme");
            for row in widget_contrast(&tokens) {
                if !row.passes() {
                    failing += 1;
                    report.push_str(&format!(
                        "{theme}: {} {:.2} (needs {}) {} on {}\n",
                        row.pair, row.ratio, row.required, row.fg, row.bg
                    ));
                }
            }
        }
        assert_eq!(failing, 0, "pairs under WCAG AA:\n{report}");
    }

    #[test]
    fn named_widgets_carry_their_spoken_name() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut checked = false;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.checkbox(&mut checked, "");
                bare_checkbox(ui, &mut checked, "Snap to grid");
                glyph_button(ui, "x", "Dismiss the error");
            });
        });
        output.textures_delta.clear();
        let update = output.platform_output.accesskit_update.expect("accesskit is on");
        let names = widget_names(&update);
        assert!(
            names.contains(&("CheckBox".to_string(), "Snap to grid".to_string())),
            "{names:?}"
        );
        assert!(
            names.contains(&("Button".to_string(), "Dismiss the error".to_string())),
            "{names:?}"
        );
        assert_eq!(
            unnamed_widgets(&update),
            ["CheckBox"],
            "only the bare checkbox: {names:?}"
        );
    }

    /// A "⚠" status glyph is static text: a screen reader reads its message as a label and never
    /// announces a button that does nothing (ADR-8870, WCAG 4.1.2).
    #[test]
    fn a_warning_glyph_is_a_label_that_speaks_its_message() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                warning_glyph(ui, Color32::YELLOW, "Texture slot 2 is empty");
            });
        });
        output.textures_delta.clear();
        let update = output.platform_output.accesskit_update.expect("accesskit is on");
        let labels = label_names(&update);
        assert!(
            labels.iter().any(|l| l == "Texture slot 2 is empty"),
            "the message is the label's spoken text: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l == "\u{26a0}"),
            "the glyph itself is not what is spoken: {labels:?}"
        );
        let names = widget_names(&update);
        assert!(names.is_empty(), "no interactive node: {names:?}");
    }

    #[test]
    fn the_contrast_method_answers_rows_and_refuses_unknown_themes() {
        let mut world = World::new();
        let (_, handler) = methods().remove(0);
        let out = handler(&mut world, &json!({})).expect("active theme");
        assert_eq!(out["theme"], "active");
        assert_eq!(out["rows"].as_array().map(Vec::len), Some(CONTRAST_PAIRS));
        assert_eq!(out["failing"], json!([]), "the default look meets AA");
        let light = handler(&mut world, &json!({ "theme": "ui:regia-light" })).expect("light");
        assert_eq!(light["theme"], "ui:regia-light");
        let err = handler(&mut world, &json!({ "theme": "neon" })).expect_err("unknown");
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("code")),
            Some(&json!("unknown_theme"))
        );
    }

    /// ADR-7838: every text pair of the high-contrast theme meets AAA (7:1, which also covers the
    /// 4.5:1 large-text bar) and every control boundary 3:1; the approved themes are not AAA.
    #[test]
    fn the_high_contrast_theme_passes_every_aaa_pair() {
        let tokens = theme_tokens("high-contrast").expect("built in");
        let rows = editor_contrast_aaa(&tokens);
        assert_eq!(rows.len(), CONTRAST_PAIRS);
        for row in &rows {
            assert!(
                row.passes(),
                "{} {:.2} needs {}",
                row.pair,
                row.ratio,
                row.required
            );
            if row.required == TEXT_AAA {
                assert!(row.ratio >= LARGE_TEXT_AAA);
            }
        }
        assert!(rows.iter().filter(|r| r.required == TEXT_AAA).count() >= 17);
        let default = editor_contrast_aaa(&ThemeTokens::regia_default());
        assert!(
            default.iter().any(|r| !r.passes()),
            "the default look is not an AAA theme"
        );
        let mut world = World::new();
        let (_, handler) = methods().remove(0);
        let out = handler(&mut world, &json!({ "theme": "high-contrast", "level": "aaa" })).expect("aaa");
        assert_eq!(out["failing"], json!([]));
        assert_eq!(out["level"], "aaa");
        let err = handler(&mut world, &json!({ "level": "aaaa" })).expect_err("bad level");
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("code")),
            Some(&json!("bad_level"))
        );
    }

    /// The clipped-text measure: a label wider than its clip is found, one that fits is not, and
    /// text scrolled fully out of its clip is not counted.
    #[test]
    fn clipped_text_finds_text_cut_by_its_clip_edge() {
        let ctx = egui::Context::default();
        let raw = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.label("fits");
                ui.allocate_ui(egui::vec2(60.0, 20.0), |ui| {
                    ui.set_clip_rect(ui.max_rect());
                    ui.add(
                        egui::Label::new("a much longer label than sixty points")
                            .wrap_mode(egui::TextWrapMode::Extend),
                    );
                });
            });
        });
        output.textures_delta.clear();
        let clipped = clipped_text(&output.shapes, 0.5);
        assert_eq!(clipped.len(), 1, "{clipped:?}");
        assert!(clipped[0].text.starts_with("a much longer"));
        assert!(!clipped_text(&output.shapes, 0.5).iter().any(|c| c.text == "fits"));
    }
}
