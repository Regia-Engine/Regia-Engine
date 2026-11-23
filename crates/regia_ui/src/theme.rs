// SPDX-License-Identifier: MPL-2.0
//! Themes and style classes (`specs/18` § 5): palettes, named classes with state variants
//! (hover/pressed/focused/disabled) and colour transitions. The [`UiThemes`] resource holds the
//! `.theme.ron` themes (built-ins `regia-dark`, `regia-light`, `regia-high-contrast`,
//! `regia-light-high-contrast`) and
//! the legacy dark and light palettes; [`apply_themes`] resolves each node's look
//! (`crate::theme_resolve`, ADR-289).

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use crate::color::UiColor;
use crate::text::TextStyle;
use crate::theme_file::{ResolvedTheme, ThemeSet};
use crate::widget_style::{DISABLED_OPACITY, StyleState, WidgetSize};

pub use crate::theme_resolve::apply_themes;

/// The palette of one theme.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeColors {
    /// Window background.
    pub background: UiColor,
    /// Card/panel background.
    pub surface: UiColor,
    /// Brighter surface (hover, popups).
    pub surface_hi: UiColor,
    /// Primary accent (buttons, sliders, focus).
    pub primary: UiColor,
    /// Text on `primary`.
    pub on_primary: UiColor,
    /// Body text.
    pub text: UiColor,
    /// Dimmed text.
    pub text_dim: UiColor,
    /// Divider/border strokes.
    pub border: UiColor,
    /// Overlay scrim behind modals.
    pub overlay: UiColor,
    /// Danger actions.
    pub danger: UiColor,
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self::from_hex(
            "#10141a",
            "#181e27",
            "#212a37",
            "#3d9df0",
            "#0b1220",
            "#e8edf4",
            "#8b95a6",
            "#2c3644",
            "#000000aa",
            "#e2544c",
        )
        .unwrap_or_default()
    }
}

impl ThemeColors {
    /// Builds a palette from hex strings (see [`UiColor::from_hex`]).
    #[allow(clippy::too_many_arguments)]
    pub fn from_hex(
        background: &str,
        surface: &str,
        surface_hi: &str,
        primary: &str,
        on_primary: &str,
        text: &str,
        text_dim: &str,
        border: &str,
        overlay: &str,
        danger: &str,
    ) -> Option<Self> {
        Some(Self {
            background: UiColor::from_hex(background)?,
            surface: UiColor::from_hex(surface)?,
            surface_hi: UiColor::from_hex(surface_hi)?,
            primary: UiColor::from_hex(primary)?,
            on_primary: UiColor::from_hex(on_primary)?,
            text: UiColor::from_hex(text)?,
            text_dim: UiColor::from_hex(text_dim)?,
            border: UiColor::from_hex(border)?,
            overlay: UiColor::from_hex(overlay)?,
            danger: UiColor::from_hex(danger)?,
        })
    }
}

/// A complete theme.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiTheme {
    /// Theme name ("dark", "light", or a game theme).
    pub name: String,
    /// Palette.
    pub colors: ThemeColors,
    /// Default corner radius (UI px).
    pub radius: f32,
    /// Base font size (UI px).
    pub font_px: f32,
}

impl Default for UiTheme {
    fn default() -> Self {
        Self::dark()
    }
}

impl UiTheme {
    /// The built-in dark theme.
    pub fn dark() -> Self {
        Self {
            name: "dark".to_owned(),
            colors: ThemeColors::default(),
            radius: 8.0,
            font_px: 18.0,
        }
    }

    /// The built-in light theme.
    pub fn light() -> Self {
        Self {
            name: "light".to_owned(),
            colors: ThemeColors::from_hex(
                "#e9edf2",
                "#f4f6f9",
                "#ffffff",
                "#1d7ad4",
                "#ffffff",
                "#1a222c",
                "#5c6675",
                "#c8d0da",
                "#00000055",
                "#c0392f",
            )
            .unwrap_or_default(),
            radius: 8.0,
            font_px: 18.0,
        }
    }

    /// The legacy palette view of a resolved `.theme.ron` theme (the § 2.1 slot mapping:
    /// `color.bg` → `background`, `color.accent` → `primary`, `color.scrim` → `overlay`, ...;
    /// `radius.md` → `radius`, `type.body.size` → `font_px`). Missing tokens keep the
    /// [`UiTheme::dark`] values.
    pub fn from_resolved(name: &str, theme: &ResolvedTheme) -> Self {
        let d = Self::dark();
        let c = |key: &str, fallback: UiColor| theme.color(key).unwrap_or(fallback);
        Self {
            name: name.to_owned(),
            colors: ThemeColors {
                background: c("color.bg", d.colors.background),
                surface: c("color.surface", d.colors.surface),
                surface_hi: c("color.surface-raised", d.colors.surface_hi),
                primary: c("color.accent", d.colors.primary),
                on_primary: c("color.on-accent", d.colors.on_primary),
                text: c("color.text", d.colors.text),
                text_dim: c("color.text-muted", d.colors.text_dim),
                border: c("color.border", d.colors.border),
                overlay: c("color.scrim", d.colors.overlay),
                danger: c("color.danger", d.colors.danger),
            },
            radius: theme.number("radius.md").unwrap_or(d.radius),
            font_px: theme.number("type.body.size").unwrap_or(d.font_px),
        }
    }
}

/// Visual overrides of one class or state variant (the `.theme.ron` widget property set,
/// `specs/ui-default-design.md` § 6.2; every field is optional and later states win per field).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VisualOverrides {
    /// Background fill.
    pub background: Option<UiColor>,
    /// Border colour.
    pub border_color: Option<UiColor>,
    /// Border width (uniform, UI px).
    pub border_width: Option<f32>,
    /// Corner radius (UI px).
    pub radius: Option<f32>,
    /// Text colour override.
    pub text_color: Option<UiColor>,
    /// Focus ring colour (focused state ring).
    pub focus_ring: Option<UiColor>,
    /// Focus ring width (UI px).
    pub focus_ring_width: Option<f32>,
    /// Focus ring offset outside the border box (UI px).
    pub focus_ring_offset: Option<f32>,
    /// Inline padding (UI px, left and right).
    pub padding_x: Option<f32>,
    /// Control height (UI px).
    pub height: Option<f32>,
    /// Minimum width (UI px).
    pub min_width: Option<f32>,
    /// Gap between children (UI px).
    pub gap: Option<f32>,
    /// Text size (UI px).
    pub text_px: Option<f32>,
    /// Icon tint.
    pub icon_color: Option<UiColor>,
    /// Elevation step (shadow token, 0 = none).
    pub elevation: Option<u8>,
    /// Whole-widget opacity, 0..=1.
    pub opacity: Option<f32>,
    /// Visual-only translation (UI px), e.g. the pressed `(0, 1)` nudge.
    pub translate: Option<[f32; 2]>,
    /// Visual-only uniform scale.
    pub scale: Option<f32>,
    /// Full padding (UI px: left, top, right, bottom). Applied to layout over the node's own
    /// [`crate::style::Style::padding`] (ADR-7208). The `.theme.ron` `padding` property takes one
    /// number (all edges), two (vertical, horizontal) or four (top, right, bottom, left, CSS order).
    pub padding: Option<[f32; 4]>,
    /// Minimum height (UI px), applied to layout. `min_width` is applied to layout too (ADR-7208).
    pub min_height: Option<f32>,
    /// Maximum width (UI px), applied to layout.
    pub max_width: Option<f32>,
    /// Maximum height (UI px), applied to layout.
    pub max_height: Option<f32>,
}

/// Applies `$f` to the pair of same-named fields of every visual property.
macro_rules! for_each_visual_field {
    ($f:ident, $out:expr, $o:expr) => {
        $f!($out, $o, background);
        $f!($out, $o, border_color);
        $f!($out, $o, border_width);
        $f!($out, $o, radius);
        $f!($out, $o, text_color);
        $f!($out, $o, focus_ring);
        $f!($out, $o, focus_ring_width);
        $f!($out, $o, focus_ring_offset);
        $f!($out, $o, padding_x);
        $f!($out, $o, height);
        $f!($out, $o, min_width);
        $f!($out, $o, gap);
        $f!($out, $o, text_px);
        $f!($out, $o, icon_color);
        $f!($out, $o, elevation);
        $f!($out, $o, opacity);
        $f!($out, $o, translate);
        $f!($out, $o, scale);
        $f!($out, $o, padding);
        $f!($out, $o, min_height);
        $f!($out, $o, max_width);
        $f!($out, $o, max_height);
    };
}

impl VisualOverrides {
    /// Non-empty when any field is set.
    pub fn any(&self) -> bool {
        macro_rules! check {
            ($out:expr, $o:expr, $field:ident) => {
                if $o.$field.is_some() {
                    return true;
                }
            };
        }
        let o = self;
        for_each_visual_field!(check, (), o);
        false
    }

    /// Lays `other` over `self`: every field `other` sets wins (the "later state wins per
    /// property" rule).
    pub fn apply(&mut self, other: &VisualOverrides) {
        macro_rules! take {
            ($out:expr, $o:expr, $field:ident) => {
                if $o.$field.is_some() {
                    $out.$field = $o.$field;
                }
            };
        }
        for_each_visual_field!(take, self, other);
    }
}

/// One named style class with state variants, sizes, parts and a colour transition time.
///
/// States compose in the fixed order `base → size → selected → hover → pressed → error →
/// focused → focus_visible → disabled` (later wins per field); see [`StyleClass::resolve_state`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleClass {
    /// Base visuals.
    pub base: VisualOverrides,
    /// Hover state.
    pub hover: VisualOverrides,
    /// Pressed state.
    pub pressed: VisualOverrides,
    /// Focused state (any focus: pointer, keyboard or gamepad).
    pub focused: VisualOverrides,
    /// Keyboard or gamepad focus only (ADR-7827): applies over `focused` while
    /// [`crate::widget_style::StyleState::focus_visible`] is set, so a clicked control can keep a
    /// quieter look than one reached by navigation.
    pub focus_visible: VisualOverrides,
    /// Disabled state.
    pub disabled: VisualOverrides,
    /// Selected/checked/open state.
    pub selected: VisualOverrides,
    /// Error (invalid value) state.
    pub error: VisualOverrides,
    /// Small size (`md` is the base).
    pub sm: VisualOverrides,
    /// Large size.
    pub lg: VisualOverrides,
    /// Sub-elements addressed by name (`thumb`, `track`, `fill`, `knob`, `indicator`, `row`,
    /// `item`), each with its own states.
    pub parts: Vec<(String, StyleClass)>,
    /// Colour transition duration (seconds); 0 = instant.
    pub transition: f32,
    /// Control points `[x1, y1, x2, y2]` of the transition's cubic Bézier easing; `None` = linear.
    /// Set by the `.theme.ron` resolver; not part of the legacy serde shape.
    #[serde(skip)]
    pub transition_ease: Option<[f32; 4]>,
}

impl StyleClass {
    /// Resolves the overrides for a node's current interaction state (legacy four-flag form;
    /// no selected/error, default size).
    pub fn resolve(&self, hovered: bool, pressed: bool, focused: bool, disabled: bool) -> VisualOverrides {
        self.resolve_state(&StyleState {
            hovered,
            pressed,
            focused,
            disabled,
            ..Default::default()
        })
    }

    /// Composes every state in the § 3 order. `disabled` suppresses hover and pressed
    /// and applies last, so it wins a contested property. The focused ring still applies.
    /// Hover is skipped for touch input; `md` applies no size layer.
    pub fn resolve_state(&self, state: &StyleState) -> VisualOverrides {
        let mut out = self.base;
        match state.size {
            WidgetSize::Sm => out.apply(&self.sm),
            WidgetSize::Md => {}
            WidgetSize::Lg => out.apply(&self.lg),
        }
        if state.selected {
            out.apply(&self.selected);
        }
        if !state.disabled {
            if state.hovered && !state.touch {
                out.apply(&self.hover);
            }
            if state.pressed {
                out.apply(&self.pressed);
            }
        }
        if state.error {
            out.apply(&self.error);
        }
        if state.focused {
            out.apply(&self.focused);
            if state.focus_visible {
                out.apply(&self.focus_visible);
            }
        }
        if state.disabled {
            out.apply(&self.disabled);
        }
        out
    }

    /// One named part of this class.
    pub fn part(&self, name: &str) -> Option<&StyleClass> {
        self.parts.iter().find(|(n, _)| n == name).map(|(_, c)| c)
    }
}

/// The built-in class set, keyed by class name (single names like `"button"`, `"primary"`;
/// state resolution combines every class on the node in order).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ThemeClasses(pub Vec<(String, StyleClass)>);

impl ThemeClasses {
    /// Looks up one class.
    pub fn get(&self, name: &str) -> Option<&StyleClass> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, c)| c)
    }

    /// Resolves one part (e.g. `"thumb"`) for a node carrying `classes`: every class that
    /// defines the part contributes, later classes win per field.
    pub fn resolve_part(&self, classes: &[String], part: &str, state: &StyleState) -> VisualOverrides {
        let mut out = VisualOverrides::default();
        for name in classes {
            if let Some(p) = self.get(name).and_then(|c| c.part(part)) {
                out.apply(&p.resolve_state(state));
            }
        }
        out
    }

    /// The pre-v1 default class set. The resolver uses the theme's widget types instead and
    /// layers only game classes that differ from these (`UiThemes::classes`); the v0 migrator
    /// still maps them (`crate::theme_migrate`).
    pub fn builtin() -> Self {
        let button = StyleClass {
            base: VisualOverrides {
                border_width: Some(1.0),
                radius: Some(8.0),
                ..Default::default()
            },
            hover: VisualOverrides {
                border_width: Some(1.0),
                ..Default::default()
            },
            pressed: VisualOverrides {
                border_width: Some(1.0),
                ..Default::default()
            },
            focused: VisualOverrides {
                border_width: Some(2.0),
                ..Default::default()
            },
            transition: 0.08,
            ..Default::default()
        };
        let surface = StyleClass {
            base: VisualOverrides {
                border_width: Some(1.0),
                radius: Some(8.0),
                ..Default::default()
            },
            ..Default::default()
        };
        Self(vec![
            ("button".to_owned(), button.clone()),
            ("tab".to_owned(), button.clone()),
            ("nav".to_owned(), button),
            ("surface".to_owned(), surface.clone()),
            ("panel".to_owned(), surface),
            (
                "toggle".to_owned(),
                StyleClass {
                    base: VisualOverrides {
                        border_width: Some(1.0),
                        radius: Some(16.0),
                        ..Default::default()
                    },
                    transition: 0.08,
                    ..Default::default()
                },
            ),
            (
                "slider-track".to_owned(),
                StyleClass {
                    base: VisualOverrides {
                        border_width: Some(1.0),
                        radius: Some(8.0),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
        ])
    }
}

/// The registry of themes and the active selection.
///
/// `set` holds every `.theme.ron` theme by name; [`crate::theme_resolve::ActiveTheme`] selects
/// the one nodes resolve against. `themes`/`active` are the legacy palette view: the built-in
/// `dark` and `light` entries mirror `regia-dark` and `regia-light`, and
/// [`UiThemes::select`] / [`UiThemes::toggle`] still switch the resolved theme (a game palette
/// with no theme of its name is migrated into `set` when selected).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct UiThemes {
    /// Every legacy palette, by name.
    pub themes: Vec<UiTheme>,
    /// Index of the active legacy palette.
    pub active: usize,
    /// Game style classes. Entries that differ from [`ThemeClasses::builtin`] layer over the
    /// theme's widget types; unchanged built-in classes are superseded by them.
    pub classes: ThemeClasses,
    /// Opacity of a disabled widget over its normal look: [`DISABLED_OPACITY`] (spec § 3); `1.0`
    /// turns the rule off.
    pub disabled_opacity: f32,
    /// Every loaded `.theme.ron` theme (the built-ins plus game themes).
    pub set: ThemeSet,
}

impl Default for UiThemes {
    fn default() -> Self {
        let set = crate::theme_resolve::builtin_theme_set();
        let palette = |name: &str, file: &str, fallback: fn() -> UiTheme| {
            set.resolve(file, None)
                .map(|t| UiTheme::from_resolved(name, &t))
                .unwrap_or_else(|_| fallback())
        };
        let themes = vec![
            palette("dark", "regia-dark", UiTheme::dark),
            palette("light", "regia-light", UiTheme::light),
        ];
        Self {
            themes,
            active: 0,
            classes: ThemeClasses::builtin(),
            disabled_opacity: DISABLED_OPACITY,
            set,
        }
    }
}

impl UiThemes {
    /// The active theme.
    pub fn current(&self) -> &UiTheme {
        self.themes
            .get(self.active)
            .unwrap_or_else(|| self.themes.first().unwrap_or(&DARK_FALLBACK))
    }

    /// Switches theme by name. Returns `true` on success.
    pub fn select(&mut self, name: &str) -> bool {
        if let Some(idx) = self.themes.iter().position(|t| t.name == name) {
            self.active = idx;
            true
        } else {
            false
        }
    }

    /// Cycles dark ↔ light; a no-op unless exactly the two built-in themes are loaded.
    pub fn toggle(&mut self) {
        let builtin = self.themes.len() == 2
            && self.themes[0].name == UiTheme::dark().name
            && self.themes[1].name == UiTheme::light().name;
        if builtin {
            self.active = 1 - self.active;
        }
    }
}

static DARK_FALLBACK: UiTheme = UiTheme {
    name: String::new(),
    colors: FALLBACK_COLORS,
    radius: 8.0,
    font_px: 18.0,
};

const FALLBACK_COLORS: ThemeColors = ThemeColors {
    background: UiColor([0.03, 0.04, 0.05, 1.0]),
    surface: UiColor([0.06, 0.07, 0.09, 1.0]),
    surface_hi: UiColor([0.09, 0.11, 0.14, 1.0]),
    primary: UiColor([0.18, 0.5, 0.9, 1.0]),
    on_primary: UiColor([0.95, 0.97, 1.0, 1.0]),
    text: UiColor([0.92, 0.94, 0.97, 1.0]),
    text_dim: UiColor([0.5, 0.55, 0.62, 1.0]),
    border: UiColor([0.15, 0.18, 0.23, 1.0]),
    overlay: UiColor([0.0, 0.0, 0.0, 0.66]),
    danger: UiColor([0.85, 0.3, 0.28, 1.0]),
};

/// Theme-resolved visuals of one node, written by [`apply_themes`] when the node's inputs or the
/// theme change (read by extract; explicit components win over class-resolved values).
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct Themed {
    /// Resolved background colour (None = transparent).
    pub background: Option<UiColor>,
    /// Resolved border colour.
    pub border_color: Option<UiColor>,
    /// Resolved uniform border width (UI px).
    pub border_width: Option<f32>,
    /// Resolved corner radius (UI px).
    pub radius: Option<f32>,
    /// Resolved text colour.
    pub text_color: Option<UiColor>,
    /// Focus ring colour when focused.
    pub focus_ring: Option<UiColor>,
    /// Resolved whole-widget opacity (None = opaque).
    pub opacity: Option<f32>,
    /// Resolved control height (UI px): the widget type's `height`, else `size.control.<size>`
    /// on nodes carrying a [`WidgetSize`].
    pub height: Option<f32>,
    /// Resolved inline padding (UI px): the widget type's `padding_x`, else
    /// `size.pad-x.<size>` on nodes carrying a [`WidgetSize`].
    pub padding_x: Option<f32>,
    /// The theme's own `height` property (a widget type, class, state or [`crate::ThemeOverride`]),
    /// without the `size.control.*` token fallback: the height layout applies (ADR-9340).
    /// Widgets that carry a [`WidgetSize`] size themselves from [`crate::UiDensity`].
    pub layout_height: Option<f32>,
    /// The theme's own `padding_x` property, without the `size.pad-x.*` token fallback: the left
    /// and right padding layout applies (ADR-9340). A full `padding` wins over it.
    pub layout_padding_x: Option<f32>,
    /// Resolved visual-only translation (UI px).
    pub translate: Option<[f32; 2]>,
    /// Resolved visual-only scale.
    pub scale: Option<f32>,
    /// Resolved focus ring width (UI px).
    pub focus_ring_width: Option<f32>,
    /// Resolved focus ring offset (UI px).
    pub focus_ring_offset: Option<f32>,
    /// Resolved minimum width (UI px); applied to layout (ADR-7208).
    pub min_width: Option<f32>,
    /// Resolved full padding (UI px: left, top, right, bottom); applied to layout over the node's
    /// own padding (ADR-7208).
    pub padding: Option<[f32; 4]>,
    /// Resolved minimum height (UI px); applied to layout.
    pub min_height: Option<f32>,
    /// Resolved maximum width (UI px); applied to layout.
    pub max_width: Option<f32>,
    /// Resolved maximum height (UI px); applied to layout.
    pub max_height: Option<f32>,
    /// Resolved gap between children (UI px); applied to layout as the row and column gap
    /// (ADR-9340).
    pub gap: Option<f32>,
    /// Resolved text size (UI px).
    pub text_px: Option<f32>,
    /// Resolved icon tint.
    pub icon_color: Option<UiColor>,
    /// Resolved elevation step.
    pub elevation: Option<u8>,
    /// Theme background being animated (current lerped value).
    pub anim_background: UiColor,
    /// Transition progress target.
    pub anim_target: UiColor,
    /// Total transition duration (seconds).
    pub anim_total: f32,
    /// Transition remaining seconds.
    pub anim_remaining: f32,
    /// Easing control points of the running transition (`None` = linear).
    pub anim_ease: Option<[f32; 4]>,
    /// Whether the node is focused (the `focused` state layer applies; the ring draws only when
    /// [`Self::focus_visible`] is set too).
    pub focused: bool,
    /// Whether the focus ring draws: the node is focused and [`crate::focus::FocusRingPolicy`]
    /// shows the ring in the current [`crate::focus::UiInputMode`] (gap I05).
    pub focus_visible: bool,
    /// The node's theme `color.accent` (slider, toggle and progress fills), resolved through its
    /// [`crate::theme_resolve::ThemeScope`].
    pub accent: Option<UiColor>,
    /// Theme generation this look was resolved for (0 = never resolved).
    pub generation: u64,
    /// Colours at the start of the running animation: background, border, text.
    pub anim_from: [UiColor; 3],
    /// Colours the animation ends on: background, border, text (`None` = not themed).
    pub anim_to: [Option<UiColor>; 3],
}

/// Applies a theme's default text style to a text node (used when authoring with classes).
pub fn theme_text_style(theme: &UiTheme) -> TextStyle {
    TextStyle {
        font_px: theme.font_px,
        color: theme.colors.text,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_state_resolution() {
        let class = StyleClass {
            base: VisualOverrides {
                background: Some(UiColor::linear(0.1, 0.1, 0.1, 1.0)),
                ..Default::default()
            },
            hover: VisualOverrides {
                background: Some(UiColor::linear(0.2, 0.2, 0.2, 1.0)),
                ..Default::default()
            },
            disabled: VisualOverrides {
                text_color: Some(UiColor::linear(0.5, 0.5, 0.5, 1.0)),
                ..Default::default()
            },
            ..Default::default()
        };
        let base = class.resolve(false, false, false, false);
        assert_eq!(base.background, Some(UiColor::linear(0.1, 0.1, 0.1, 1.0)));
        let hover = class.resolve(true, false, false, false);
        assert_eq!(hover.background, Some(UiColor::linear(0.2, 0.2, 0.2, 1.0)));
        let disabled = class.resolve(true, false, false, true);
        assert_eq!(disabled.background, class.base.background);
        assert_eq!(
            disabled.text_color, class.disabled.text_color,
            "disabled wins over hover"
        );
    }

    #[test]
    fn theme_toggle_and_select() {
        let mut t = UiThemes::default();
        assert_eq!(t.current().name, "dark");
        t.toggle();
        assert_eq!(t.current().name, "light");
        assert!(t.select("dark"));
        assert_eq!(t.current().name, "dark");
        assert!(!t.select("nope"));
    }

    #[test]
    fn toggle_is_a_no_op_unless_the_two_built_ins_are_loaded() {
        let mut custom = UiThemes::default();
        custom.themes = vec![
            UiTheme {
                name: "a".to_owned(),
                ..UiTheme::dark()
            },
            UiTheme {
                name: "b".to_owned(),
                ..UiTheme::dark()
            },
        ];
        custom.active = 0;
        custom.toggle();
        assert_eq!(custom.current().name, "a");
    }

    #[test]
    fn legacy_palettes_mirror_the_theme_files_and_disabled_opacity_is_on() {
        let t = UiThemes::default();
        let dark = t.set.resolve("regia-dark", None).unwrap();
        let light = t.set.resolve("regia-light", None).unwrap();
        assert_eq!(t.themes[0].colors.border, dark.color("color.border").unwrap());
        assert_eq!(t.themes[0].colors.primary, dark.color("color.accent").unwrap());
        assert_eq!(t.themes[1].colors.background, light.color("color.bg").unwrap());
        assert_eq!(t.themes[0].radius, 8.0);
        assert_eq!(t.disabled_opacity, 0.45);
        assert_eq!(t.set.names().count(), 4);
    }

    fn c(v: f32) -> UiColor {
        UiColor::linear(v, v, v, 1.0)
    }

    /// One class where every state sets `background` to a distinct value, so the winner of any
    /// combination identifies the order.
    fn marked_class() -> StyleClass {
        let bg = |v: f32| VisualOverrides {
            background: Some(c(v)),
            ..Default::default()
        };
        StyleClass {
            base: bg(0.0),
            sm: bg(0.05),
            lg: bg(0.06),
            selected: bg(0.1),
            hover: bg(0.2),
            pressed: bg(0.3),
            error: bg(0.4),
            focused: bg(0.5),
            disabled: bg(0.6),
            ..Default::default()
        }
    }

    fn bg_of(state: StyleState) -> UiColor {
        marked_class().resolve_state(&state).background.unwrap()
    }

    #[test]
    fn state_order_every_pair() {
        // normal < size < selected < hover < pressed < error < focused < disabled
        let order = ["selected", "hover", "pressed", "error", "focused", "disabled"];
        let flag = |name: &str, st: &mut StyleState| match name {
            "selected" => st.selected = true,
            "hover" => st.hovered = true,
            "pressed" => st.pressed = true,
            "error" => st.error = true,
            "focused" => st.focused = true,
            _ => st.disabled = true,
        };
        let value = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        for i in 0..order.len() {
            for j in (i + 1)..order.len() {
                let mut st = StyleState::default();
                flag(order[i], &mut st);
                flag(order[j], &mut st);
                let got = bg_of(st);
                // disabled suppresses hover and pressed and applies last, so a contested
                // background still resolves to the disabled value.
                let expect = if order[i] == "disabled" || order[j] == "disabled" {
                    0.6
                } else {
                    value[j]
                };
                assert_eq!(got, c(expect), "{} vs {}", order[i], order[j]);
            }
        }
        // size arm: every state beats the size layer, for sm and lg alike.
        for size in [WidgetSize::Sm, WidgetSize::Lg] {
            for (name, expect) in order.iter().zip(value) {
                let mut st = StyleState {
                    size,
                    ..Default::default()
                };
                flag(name, &mut st);
                assert_eq!(bg_of(st), c(expect), "{size:?} vs {name}");
            }
        }
    }

    #[test]
    fn disabled_suppresses_hover_pressed_and_focus() {
        let class = StyleClass {
            hover: VisualOverrides {
                border_width: Some(9.0),
                ..Default::default()
            },
            pressed: VisualOverrides {
                translate: Some([0.0, 1.0]),
                ..Default::default()
            },
            focused: VisualOverrides {
                focus_ring_width: Some(3.0),
                ..Default::default()
            },
            disabled: VisualOverrides {
                opacity: Some(0.45),
                ..Default::default()
            },
            ..Default::default()
        };
        let r = class.resolve_state(&StyleState {
            hovered: true,
            pressed: true,
            focused: true,
            disabled: true,
            ..Default::default()
        });
        assert_eq!(r.border_width, None);
        assert_eq!(r.translate, None);
        assert_eq!(r.focus_ring_width, Some(3.0));
        assert_eq!(r.opacity, Some(0.45));
    }

    #[test]
    fn disabled_still_shows_selected_and_error() {
        let r = marked_class().resolve_state(&StyleState {
            disabled: true,
            selected: true,
            error: true,
            ..Default::default()
        });
        // disabled wins the contested property, but non-contested ones from lower states survive
        assert_eq!(r.background, Some(c(0.6)));
        let class = StyleClass {
            selected: VisualOverrides {
                border_width: Some(2.0),
                ..Default::default()
            },
            ..Default::default()
        };
        let r = class.resolve_state(&StyleState {
            disabled: true,
            selected: true,
            ..Default::default()
        });
        assert_eq!(r.border_width, Some(2.0));
    }

    #[test]
    fn touch_input_skips_hover() {
        let st = StyleState {
            hovered: true,
            touch: true,
            ..Default::default()
        };
        assert_eq!(bg_of(st), c(0.0));
        assert_eq!(bg_of(StyleState { touch: false, ..st }), c(0.2));
    }

    #[test]
    fn sizes_layer_under_states() {
        assert_eq!(bg_of(StyleState::default()), c(0.0), "md applies no size layer");
        let sm = StyleState {
            size: WidgetSize::Sm,
            ..Default::default()
        };
        let lg = StyleState {
            size: WidgetSize::Lg,
            ..Default::default()
        };
        assert_eq!(bg_of(sm), c(0.05));
        assert_eq!(bg_of(lg), c(0.06));
        assert_eq!(
            bg_of(StyleState { hovered: true, ..sm }),
            c(0.2),
            "hover beats size"
        );
    }

    #[test]
    fn legacy_resolve_matches_resolve_state() {
        let class = marked_class();
        for bits in 0..16u8 {
            let (h, p, f, d) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
            let st = StyleState {
                hovered: h,
                pressed: p,
                focused: f,
                disabled: d,
                ..Default::default()
            };
            assert_eq!(class.resolve(h, p, f, d), class.resolve_state(&st));
        }
    }

    #[test]
    fn apply_overlays_every_field_and_any_sees_them() {
        let full = VisualOverrides {
            background: Some(c(0.1)),
            border_color: Some(c(0.2)),
            border_width: Some(1.0),
            radius: Some(2.0),
            text_color: Some(c(0.3)),
            focus_ring: Some(c(0.4)),
            focus_ring_width: Some(3.0),
            focus_ring_offset: Some(2.0),
            padding_x: Some(16.0),
            height: Some(44.0),
            min_width: Some(80.0),
            gap: Some(8.0),
            text_px: Some(14.0),
            icon_color: Some(c(0.5)),
            elevation: Some(2),
            opacity: Some(0.45),
            translate: Some([0.0, 1.0]),
            scale: Some(1.02),
            padding: Some([1.0, 2.0, 3.0, 4.0]),
            min_height: Some(20.0),
            max_width: Some(400.0),
            max_height: Some(300.0),
        };
        let mut out = VisualOverrides::default();
        assert!(!out.any());
        out.apply(&full);
        assert_eq!(out, full);
        out.apply(&VisualOverrides::default());
        assert_eq!(out, full, "an empty layer changes nothing");
        // each single-field overlay is visible to any()
        assert!(
            VisualOverrides {
                scale: Some(1.0),
                ..Default::default()
            }
            .any()
        );
    }

    #[test]
    fn old_ron_shape_still_deserializes() {
        let class: StyleClass = ron::from_str("(base: (border_width: Some(1.0)), transition: 0.08)").unwrap();
        assert_eq!(class.base.border_width, Some(1.0));
        assert!(class.parts.is_empty() && !class.selected.any());
    }

    #[test]
    fn parts_resolve_per_class_and_state() {
        let thumb = StyleClass {
            base: VisualOverrides {
                height: Some(20.0),
                ..Default::default()
            },
            hover: VisualOverrides {
                height: Some(22.0),
                ..Default::default()
            },
            ..Default::default()
        };
        let slider = StyleClass {
            parts: vec![("thumb".to_owned(), thumb)],
            ..Default::default()
        };
        let classes = ThemeClasses(vec![("slider".to_owned(), slider)]);
        let names = vec!["slider".to_owned(), "other".to_owned()];
        let rest = classes.resolve_part(&names, "thumb", &StyleState::default());
        assert_eq!(rest.height, Some(20.0));
        let hot = classes.resolve_part(
            &names,
            "thumb",
            &StyleState {
                hovered: true,
                ..Default::default()
            },
        );
        assert_eq!(hot.height, Some(22.0));
        assert!(
            !classes
                .resolve_part(&names, "track", &StyleState::default())
                .any()
        );
    }
}
