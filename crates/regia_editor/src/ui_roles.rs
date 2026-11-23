// SPDX-License-Identifier: MPL-2.0
//! Bridge from the runtime UI's color roles to the editor's [`ThemeTokens`]
//! (`docs/specs/ui-default-design.md` § 7, lane D5).
//!
//! The editor stays egui and keeps the ADR-229 look by default. A `.theme.ron` can skin it by
//! opt-in: [`ThemeTokens::from_ui_roles`] maps role names such as `color.surface` onto the
//! editor tokens, and the [`ThemeSource`] preference (`"default"` or `"ui:<name>"`) records
//! which look is active. `editor.set_theme { ui_theme }` switches it (`book/agents/theme.md`).
//!
//! [`UiRoles`] is deliberately a plain role-name → color map so this lane does not depend on the
//! D1 token types (`regia_ui::theme`, lane `ui-d1-theme-tokens`). When D1 lands, resolve the
//! theme there and feed its resolved `color.*` entries to [`UiRoles::insert`]; nothing else in
//! this module changes. [`UiRoles::builtin`] resolves the built-in theme files and keeps two small
//! tables of only the roles the bridge reads as a fallback if those files fail to load.

use std::collections::BTreeMap;

use egui::Color32;
use regia_ecs::prelude::*;
use serde_json::{Map, Value};

use crate::theme::{ThemeTokens, parse_color};

/// Resolved UI color roles, keyed by role name (`color.surface`, `color.accent`, ...).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UiRoles {
    colors: BTreeMap<String, Color32>,
}

/// The fallback role tables: dark, then light. Only the roles the editor bridge reads from § 2.1
/// (not the full spec table); the full set comes from the built-in theme files via
/// [`UiRoles::builtin`].
const DARK: [(&str, &str); 15] = [
    ("color.surface", "#171c23"),
    ("color.surface-overlay", "#262e3a"),
    ("color.surface-sunken", "#0b0e12"),
    ("color.border", "#2e3744"),
    ("color.text", "#e8edf4"),
    ("color.text-muted", "#9aa4b2"),
    ("color.accent", "#3d9df0"),
    ("color.on-accent", "#0b1220"),
    ("color.danger", "#e5534b"),
    ("color.warning", "#e0a33a"),
    ("color.surface-raised", "#1f2630"),
    ("color.bg", "#0f1318"),
    ("color.border-strong", "#5f6b7c"),
    ("color.success", "#3fb950"),
    ("color.info", "#3d9df0"),
];
const LIGHT: [(&str, &str); 15] = [
    ("color.surface", "#f6f8fa"),
    ("color.surface-overlay", "#ffffff"),
    ("color.surface-sunken", "#e3e8ee"),
    ("color.border", "#c9d1db"),
    ("color.text", "#18202a"),
    ("color.text-muted", "#566170"),
    ("color.accent", "#1d6fc4"),
    ("color.on-accent", "#ffffff"),
    ("color.danger", "#c4372f"),
    ("color.warning", "#975a00"),
    ("color.surface-raised", "#ffffff"),
    ("color.bg", "#e9edf2"),
    ("color.border-strong", "#7c8796"),
    ("color.success", "#1f7a35"),
    ("color.info", "#1d6fc4"),
];

impl UiRoles {
    /// An empty map; missing roles fall back to `regia-dark` in [`ThemeTokens::from_ui_roles`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets one role.
    pub fn insert(&mut self, name: impl Into<String>, color: Color32) {
        self.colors.insert(name.into(), color);
    }

    /// One role, if set.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Color32> {
        self.colors.get(name).copied()
    }

    /// The roles of built-in theme `name` (`regia-dark`, `regia-light`, `regia-high-contrast`,
    /// `regia-light-high-contrast`), if any.
    #[must_use]
    pub fn builtin(name: &str) -> Option<Self> {
        // Every resolved `color.*` token of the D1 theme file (lane D6 wiring); the tables below
        // remain the fallback if the built-in files ever fail to load.
        if let Ok(theme) = regia::ui::theme_resolve::builtin_theme_set().resolve(name, None) {
            let mut roles = Self::new();
            for (key, value) in &theme.values {
                if let Some(c) = value.as_color()
                    && let Ok(color) = parse_color(&Value::String(regia::ui::theme_color_ops::to_hex(c)))
                {
                    roles.insert(key.clone(), color);
                }
            }
            return Some(roles);
        }
        let table = match name {
            "regia-dark" => &DARK,
            "regia-light" => &LIGHT,
            _ => return None,
        };
        let mut roles = Self::new();
        for (key, hex) in table {
            let color = parse_color(&Value::String((*hex).to_string())).ok()?;
            roles.insert(*key, color);
        }
        Some(roles)
    }

    /// Roles from a JSON object of `"role.name": "#rrggbb[aa]"`.
    ///
    /// # Errors
    /// When a value is not a color string; the message names the role.
    pub fn from_json(object: &Map<String, Value>) -> Result<Self, String> {
        let mut roles = Self::new();
        for (key, value) in object {
            let color = parse_color(value).map_err(|e| format!("ui theme role `{key}`: {e}"))?;
            roles.insert(key.clone(), color);
        }
        Ok(roles)
    }
}

/// Which look the editor wears: today's ADR-229 tokens, or a UI theme through the bridge.
/// Preference key `theme_source`: `"default"` or `"ui:<name>"`. Also a world resource.
#[derive(Resource, Debug, Clone, PartialEq, Eq, Default)]
pub enum ThemeSource {
    /// The ADR-229 look (the default; the owner opts in to the bridge).
    #[default]
    Default,
    /// A UI theme by name (`ui:regia-dark`).
    Ui(String),
}

impl ThemeSource {
    /// Parses `"default"` or `"ui:<name>"`.
    ///
    /// # Errors
    /// For any other text, or `ui:` with an empty name.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "default" => Ok(Self::Default),
            _ => match text.strip_prefix("ui:") {
                Some(name) if !name.is_empty() => Ok(Self::Ui(name.to_string())),
                _ => Err(format!("theme_source `{text}` is not `default` or `ui:<name>`")),
            },
        }
    }

    /// The preference text, the inverse of [`Self::parse`].
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::Default => "default".to_string(),
            Self::Ui(name) => format!("ui:{name}"),
        }
    }
}

/// OKLab lightness shift of an sRGB color (`lighten` of the theme grammar), alpha kept.
/// Ottosson, "A perceptual color space for image processing" (2020, public domain).
#[must_use]
pub fn lighten(color: Color32, amount: f32) -> Color32 {
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    let lin = |v: u8| {
        let c = f64::from(v) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(r), lin(g), lin(b));
    let l = (0.412_221_47 * r + 0.536_332_54 * g + 0.051_445_99 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_55 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_84 * g + 0.629_978_7 * b).cbrt();
    let big_l = 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s;
    let aa = 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s;
    let bb = 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s;
    let big_l = (big_l + f64::from(amount)).clamp(0.0, 1.0);
    let l = (big_l + 0.396_337_78 * aa + 0.215_803_76 * bb).powi(3);
    let m = (big_l - 0.105_561_346 * aa - 0.063_854_17 * bb).powi(3);
    let s = (big_l - 0.089_484_18 * aa - 1.291_485_5 * bb).powi(3);
    let out = [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_93 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_4 * s,
        -0.004_196_086 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ];
    let enc = |c: f64| {
        let c = c.clamp(0.0, 1.0);
        let v = if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        (v * 255.0).round() as u8
    };
    Color32::from_rgba_unmultiplied(enc(out[0]), enc(out[1]), enc(out[2]), a)
}

/// Flattens a translucent role color over the opaque `ground` (the editor's contrast and fill
/// colors are opaque).
fn flatten(color: Color32, ground: Color32) -> Color32 {
    if color.a() == 255 {
        color
    } else {
        crate::a11y_audit::over(color, ground)
    }
}

impl ThemeTokens {
    /// The editor tokens for a UI theme's color roles (§ 7 table). Roles the map lacks come from
    /// `regia-dark`. Widget states and text accents follow the roles too (ADR-8589): a control at
    /// rest is the panel's color, hovered and open controls the surface that stands out more,
    /// pressed the sunken surface; links and the selected item use `color.accent`, the active item
    /// and warnings `color.warning`, errors `color.danger`, passes `color.success` and information
    /// `color.info`. ADR-8585 adds the control outline (`color.border-strong`, which also turns on the
    /// 2 px focus ring), the lit edge for a dark or light panel, and flattens translucent roles over
    /// the panel. Fonts and the flat look are the default look's (the brand's). Overlay, gizmo, graph
    /// pin and play-tint colors keep their defaults, and `editor.<section>.<key>` entries override
    /// any token (`editor.overlay.grid-major`; sections `overlay`, `widgets`, `accents`; `-` stands
    /// for `_`).
    ///
    /// # Errors
    /// When an `editor.*` role names a known section but the merge rejects it (cannot happen for
    /// colors; kept so a future non-color key reports instead of panicking).
    pub fn from_ui_roles(roles: &UiRoles) -> Result<Self, String> {
        let dark = UiRoles::builtin("regia-dark").unwrap_or_default();
        let role = |name: &str| {
            roles
                .get(name)
                .or_else(|| dark.get(name))
                .unwrap_or(Color32::MAGENTA)
        };
        let mut tokens = Self::regia_default();
        tokens.ground = role("color.surface-overlay");
        tokens.raised = role("color.surface-overlay");
        tokens.panel = role("color.surface");
        tokens.well = role("color.surface-sunken");
        tokens.line = role("color.border");
        tokens.muted = role("color.text-muted");
        tokens.text = role("color.text");
        tokens.strong = lighten(role("color.text"), 0.05);
        tokens.signal = role("color.accent");
        tokens.on_signal = role("color.on-accent");
        tokens.warn = role("color.warning");
        tokens.error = role("color.danger");
        // Widget states and accents follow the theme's roles too (ADR-8589): with the ADR-229
        // constants a light theme drew dark buttons, fields and dropdowns with light-gray text,
        // and yellow and orange text on white. A control at rest is the panel's color with the
        // strong border as its outline (`theme::visuals` outlines a control filled with the panel
        // color); hovered and open controls take the surface that stands out more from the panel.
        let panel = tokens.panel;
        let stand_out = |a: Color32, b: Color32| {
            let gap = |c: Color32| crate::a11y_audit::contrast_ratio(c, panel);
            if gap(a) >= gap(b) { a } else { b }
        };
        let lifted = stand_out(role("color.surface-overlay"), role("color.surface-sunken"));
        tokens.widgets.idle = panel;
        tokens.widgets.hover = lifted;
        tokens.widgets.pressed = role("color.surface-sunken");
        tokens.widgets.open = lifted;
        tokens.widgets.button_text = tokens.text;
        tokens.widgets.hover_text = tokens.strong;
        tokens.widgets.open_text = tokens.text;
        tokens.widgets.hover_line = role("color.border-strong");
        tokens.widgets.link = role("color.accent");
        tokens.widgets.code = role("color.surface-sunken");
        tokens.accents.active = role("color.warning");
        tokens.accents.select = role("color.accent");
        tokens.accents.alert = role("color.danger");
        tokens.accents.ok = role("color.success");
        tokens.accents.unsaved = role("color.warning");
        tokens.accents.error_text = role("color.danger");
        tokens.accents.log_error = role("color.danger");
        tokens.accents.log_warn = role("color.warning");
        tokens.accents.log_info = roles
            .get("color.info")
            .or_else(|| roles.get("color.accent"))
            .unwrap_or_else(|| role("color.info"));
        // ADR-8585 on top: the brand look's control outline, focus ring and lit edge, and every
        // translucent role (the brand kit's light themes use ink at opacity stops) flattened over
        // the panel, so fills and contrast use the color a person sees.
        tokens.widgets.edge = role("color.border-strong");
        let light = crate::a11y_audit::relative_luminance(flatten(panel, Color32::BLACK)) > 0.5;
        tokens.lit_edge = if light {
            Color32::from_rgba_unmultiplied(255, 255, 255, 153)
        } else {
            Color32::from_rgba_unmultiplied(255, 255, 255, 15)
        };
        let panel = flatten(panel, Color32::BLACK);
        tokens.panel = panel;
        let w = &mut tokens.widgets;
        let a = &mut tokens.accents;
        for slot in [
            &mut tokens.ground,
            &mut tokens.raised,
            &mut tokens.well,
            &mut tokens.line,
            &mut tokens.muted,
            &mut tokens.text,
            &mut tokens.strong,
            &mut tokens.signal,
            &mut tokens.warn,
            &mut tokens.error,
            &mut w.idle,
            &mut w.hover,
            &mut w.pressed,
            &mut w.open,
            &mut w.button_text,
            &mut w.hover_text,
            &mut w.open_text,
            &mut w.hover_line,
            &mut w.link,
            &mut w.code,
            &mut w.edge,
            &mut a.active,
            &mut a.select,
            &mut a.alert,
            &mut a.ok,
            &mut a.unsaved,
            &mut a.error_text,
            &mut a.log_error,
            &mut a.log_warn,
            &mut a.log_info,
        ] {
            *slot = flatten(*slot, panel);
        }
        tokens.on_signal = flatten(tokens.on_signal, tokens.signal);
        let mut groups: Map<String, Value> = Map::new();
        for (name, color) in &roles.colors {
            let Some(rest) = name.strip_prefix("editor.") else {
                continue;
            };
            let Some((section, key)) = rest.split_once('.') else {
                continue;
            };
            if !matches!(section, "overlay" | "widgets" | "accents") {
                continue;
            }
            let [r, g, b, a] = color.to_srgba_unmultiplied();
            let entry = groups
                .entry(section.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(map) = entry {
                map.insert(
                    key.replace('-', "_"),
                    Value::String(format!("#{r:02x}{g:02x}{b:02x}{a:02x}")),
                );
            }
        }
        tokens.merge(&groups)?;
        Ok(tokens)
    }
}

/// Resolves the `ui_theme` argument of `editor.set_theme`: a built-in theme name or an object of
/// role colors. Returns the roles and the [`ThemeSource`] they stand for (`ui:custom` for an
/// object). D1 plugs its theme registry in here.
///
/// # Errors
/// For an unknown name, a bad color, or a value that is neither a string nor an object.
pub fn resolve_ui_theme(value: &Value) -> Result<(UiRoles, ThemeSource), String> {
    match value {
        Value::String(name) => {
            let name = name.strip_prefix("ui:").unwrap_or(name);
            let roles = UiRoles::builtin(name)
                .ok_or_else(|| format!("unknown ui theme `{name}` (built-ins: regia-dark, regia-light)"))?;
            Ok((roles, ThemeSource::Ui(name.to_string())))
        }
        Value::Object(object) => Ok((UiRoles::from_json(object)?, ThemeSource::Ui("custom".to_string()))),
        _ => Err("ui_theme must be a theme name or an object of role colors".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Color32 {
        parse_color(&Value::String(s.to_string())).expect("hex")
    }

    #[test]
    fn dark_roles_map_onto_the_editor_tokens() {
        let roles = UiRoles::builtin("regia-dark").expect("dark");
        let t = ThemeTokens::from_ui_roles(&roles).expect("map");
        assert_eq!(t.ground, hex("#262e3a"));
        assert_eq!(t.raised, hex("#262e3a"));
        assert_eq!(t.panel, hex("#171c23"));
        assert_eq!(t.well, hex("#0b0e12"));
        assert_eq!(t.line, hex("#2e3744"));
        assert_eq!(t.muted, hex("#9aa4b2"));
        assert_eq!(t.text, hex("#e8edf4"));
        assert_eq!(t.signal, hex("#3d9df0"));
        assert_eq!(t.on_signal, hex("#0b1220"));
        assert_eq!(t.warn, hex("#e0a33a"));
        assert_eq!(t.error, hex("#e5534b"));
        assert_ne!(t.strong, t.text, "strong is a lightened text");
    }

    #[test]
    fn overlays_keep_their_defaults_and_widgets_and_accents_follow_the_roles() {
        let t = ThemeTokens::from_ui_roles(&UiRoles::builtin("regia-dark").expect("dark")).expect("map");
        let d = ThemeTokens::regia_default();
        assert_eq!(t.overlay, d.overlay);
        assert_eq!(t.shade, d.shade);
        assert_eq!(t.flat, d.flat);
        assert_eq!(t.data_kinds, d.data_kinds);
        // The play tints carry session meaning, not theme roles.
        assert_eq!(t.accents.play_mode, d.accents.play_mode);
        assert_eq!(t.accents.play_server, d.accents.play_server);
        assert_eq!(t.accents.play_clients, d.accents.play_clients);
        // ADR-8585: controls have the strong border as their outline (and so the focus ring).
        assert_eq!(t.widgets.edge, hex("#5f6b7c"));
    }

    /// ADR-8589 (bug 8 of the 2026-11-16 all-host smoke run): widget states and text accents map
    /// from the theme's roles, in both built-in themes, so `regia-light` no longer keeps the dark
    /// ADR-229 widget fills and yellow/orange text.
    #[test]
    fn widget_states_and_accents_map_from_the_roles() {
        for (name, hover) in [("regia-dark", "#262e3a"), ("regia-light", "#e3e8ee")] {
            let roles = UiRoles::builtin(name).expect("theme");
            let t = ThemeTokens::from_ui_roles(&roles).expect("map");
            let role = |r: &str| roles.get(r).expect(r);
            assert_eq!(
                t.widgets.idle, t.panel,
                "{name}: a control at rest is the panel's color"
            );
            assert_eq!(
                t.widgets.hover,
                hex(hover),
                "{name}: hovered stands out from the panel"
            );
            assert_eq!(t.widgets.open, t.widgets.hover, "{name}");
            assert_eq!(t.widgets.pressed, role("color.surface-sunken"), "{name}");
            assert_eq!(t.widgets.button_text, t.text, "{name}");
            assert_eq!(t.widgets.open_text, t.text, "{name}");
            assert_eq!(t.widgets.hover_text, t.strong, "{name}");
            assert_eq!(t.widgets.hover_line, role("color.border-strong"), "{name}");
            assert_eq!(t.widgets.link, role("color.accent"), "{name}");
            assert_eq!(t.widgets.code, role("color.surface-sunken"), "{name}");
            assert_eq!(t.accents.active, role("color.warning"), "{name}");
            assert_eq!(t.accents.select, role("color.accent"), "{name}");
            assert_eq!(t.accents.alert, role("color.danger"), "{name}");
            assert_eq!(t.accents.ok, role("color.success"), "{name}");
            assert_eq!(t.accents.unsaved, role("color.warning"), "{name}");
            assert_eq!(t.accents.error_text, role("color.danger"), "{name}");
            assert_eq!(t.accents.log_error, role("color.danger"), "{name}");
            assert_eq!(t.accents.log_warn, role("color.warning"), "{name}");
            assert_eq!(t.accents.log_info, role("color.info"), "{name}");
        }
    }

    /// FU-3: the brand kit's Paper runtime theme (`piercingxx-paper.theme.ron`, theme format 1,
    /// extends `regia-light`; a byte copy in tests/fixtures/brand-kit) loads through regia_ui's
    /// v1 -> v2 migration with no unknown-key warning, and bridged onto the editor it draws light
    /// controls and dark accents on its light panel: every pair of the contrast audit but the play
    /// tints meets AA.
    #[test]
    fn the_kit_paper_ui_theme_bridges_to_light_accents() {
        let text = include_str!("../tests/fixtures/brand-kit/piercingxx-paper.theme.ron");
        let mut set = regia::ui::theme_resolve::builtin_theme_set();
        let name = set
            .load_ron(text, "piercingxx-paper.theme.ron")
            .expect("the kit theme loads");
        assert_eq!(name, "piercingxx-paper");
        assert_eq!(
            set.get(&name).map(|f| f.format_version),
            Some(regia_ui::theme_file::THEME_FORMAT_VERSION),
            "a v1 file reads as the current version"
        );
        assert_eq!(set.unknown_token_keys(&name), Vec::<String>::new(), "no warning");
        let resolved = set.resolve(&name, None).expect("resolves");
        let mut roles = UiRoles::new();
        for (key, value) in &resolved.values {
            if let Some(c) = value.as_color()
                && let Ok(color) = parse_color(&Value::String(regia::ui::theme_color_ops::to_hex(c)))
            {
                roles.insert(key.clone(), color);
            }
        }
        let t = ThemeTokens::from_ui_roles(&roles).expect("map");
        assert_eq!(t.panel, hex("#ece7db"));
        assert_eq!(t.signal, Color32::BLACK, "Paper's accent is ink");
        assert_eq!(t.accents.log_error, hex("#8f3939"));
        assert_eq!(t.accents.ok, hex("#255f4c"));
        assert_eq!(t.accents.log_info, hex("#3f5485"));
        assert!(
            crate::a11y_audit::relative_luminance(t.widgets.idle) > 0.5,
            "light controls on a light panel, not the dark defaults: {:?}",
            t.widgets.idle
        );
        // The play-session tints keep the default look's colors in a bridged theme (ADR-8589:
        // they carry session meaning, not theme roles), so they are not measured here.
        let failing: Vec<String> = crate::a11y_audit::editor_contrast(&t)
            .into_iter()
            .filter(|r| !r.passes() && !r.pair.starts_with("accents.play_"))
            .map(|r| format!("{} {:.2} {} on {}", r.pair, r.ratio, r.fg, r.bg))
            .collect();
        assert!(failing.is_empty(), "{failing:#?}");
    }

    #[test]
    fn missing_roles_fall_back_to_dark_and_editor_group_overrides_overlays() {
        let mut roles = UiRoles::new();
        roles.insert("color.accent", hex("#e8793a"));
        roles.insert("editor.overlay.grid-major", hex("#102030"));
        roles.insert("editor.nonsense.x", hex("#ffffff"));
        let t = ThemeTokens::from_ui_roles(&roles).expect("map");
        assert_eq!(t.signal, hex("#e8793a"));
        assert_eq!(t.panel, hex("#171c23"));
        assert_eq!(t.overlay.grid_major, hex("#102030"));
        assert_eq!(
            t.overlay.grid_minor,
            ThemeTokens::regia_default().overlay.grid_minor
        );
    }

    #[test]
    fn lighten_moves_lightness_only_within_gamut_and_keeps_alpha() {
        assert_eq!(lighten(Color32::WHITE, 0.05), Color32::WHITE);
        let c = hex("#e8edf4");
        assert_eq!(lighten(c, 0.0), c);
        let lit = lighten(c, 0.05);
        assert!(lit.r() >= c.r() && lit.g() >= c.g() && lit.b() >= c.b() && lit != c);
        let black = lighten(Color32::BLACK, 0.2);
        assert!(
            black.r() > 0 && black.r() == black.g() && black.g() == black.b(),
            "grey stays grey"
        );
        let translucent = Color32::from_rgba_unmultiplied(10, 20, 30, 77);
        assert_eq!(lighten(translucent, 0.1).a(), 77);
    }

    #[test]
    fn theme_source_text_round_trips() {
        assert_eq!(ThemeSource::default(), ThemeSource::Default);
        assert_eq!(ThemeSource::parse("default"), Ok(ThemeSource::Default));
        let ui = ThemeSource::parse("ui:my-game").expect("ui");
        assert_eq!(ui, ThemeSource::Ui("my-game".to_string()));
        assert_eq!(ui.as_text(), "ui:my-game");
        assert!(ThemeSource::parse("ui:").is_err());
        assert!(ThemeSource::parse("dark").is_err());
    }

    #[test]
    fn resolve_accepts_names_objects_and_rejects_the_rest() {
        let (_, source) = resolve_ui_theme(&Value::String("regia-light".into())).expect("light");
        assert_eq!(source, ThemeSource::Ui("regia-light".to_string()));
        let (_, source) = resolve_ui_theme(&Value::String("ui:regia-dark".into())).expect("prefixed");
        assert_eq!(source.as_text(), "ui:regia-dark");
        assert!(resolve_ui_theme(&Value::String("nope".into())).is_err());
        assert!(resolve_ui_theme(&serde_json::json!({ "color.accent": "red" })).is_err());
        assert!(resolve_ui_theme(&serde_json::json!(5)).is_err());
        let (roles, source) = resolve_ui_theme(&serde_json::json!({
            "color.accent": "#ff0000",
            "color.on-accent": "#00ff00"
        }))
        .expect("object");
        assert_eq!(source, ThemeSource::Ui("custom".to_string()));
        assert_eq!(source.as_text(), "ui:custom");
        assert_eq!(roles.get("color.accent"), Some(hex("#ff0000")));
        assert_eq!(roles.get("color.on-accent"), Some(hex("#00ff00")));
        // Missing roles come from regia-dark when mapped onto the editor tokens.
        let t = ThemeTokens::from_ui_roles(&roles).expect("map");
        let dark = ThemeTokens::from_ui_roles(&UiRoles::builtin("regia-dark").expect("dark")).expect("map");
        assert_eq!(t.panel, dark.panel);
        assert_eq!(t.signal, hex("#ff0000"));
    }
}
