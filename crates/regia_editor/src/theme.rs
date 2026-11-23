// SPDX-License-Identifier: MPL-2.0
//! Token-driven editor theme (`docs/specs/19-editor.md`, F11): one [`ThemeTokens`] value
//! holds every color the editor chrome, panels and viewport overlays paint with, so a host can
//! re-skin the whole window with one [`apply`] call.
//!
//! [`ThemeTokens::regia_default`] is Piercing Graphite, the PiercingXX brand's dark look (owner
//! 2026-11-18, ADR-8585): graphite surfaces, Signal white reserved for the one strong action,
//! links, focus and the inverted selection, the brand's functional colors and its fonts
//! (JetBrains Mono, Space Mono for headings, Geist for prose; [`BRAND_FONTS`]). The other built-in
//! looks are [`ThemeTokens::amoled_night`], [`ThemeTokens::paper`] and
//! [`ThemeTokens::high_contrast`]. [`ThemeTokens::regia_classic`] keeps the look the editor had
//! before the brand (egui's dark visuals plus the accents that were inline `Color32::from_rgb`
//! calls; a test checks that [`apply`] with it leaves egui's `Visuals::dark()` untouched): it is
//! what absent keys of a version 1 theme file mean, so saved themes look as they did. Tokens
//! live in the world as a resource ([`tokens`]) for code that has a `World` (overlays, audio
//! gizmos, captures) and are mirrored into the egui context by [`apply`] ([`ui_tokens`]) for
//! panel code that only has a `Ui`. [`sync`] keeps the two in step each frame.
//!
//! Tokens load from JSON ([`ThemeTokens::from_json`]): every key is optional and falls back to
//! the Regia Engine default, colors are `"#rrggbb"` or `"#rrggbbaa"`, a font is a name (`"JetBrains
//! Mono"`, one of [`BRAND_FONTS`] or a family a host installed) or `{ name, file }`, and the brand names `ink`,
//! `graphite` and `slate` are accepted for `ground`, `panel` and `well`. The keys may sit at
//! the top level or under `"colors"`; see `book/agents/theme.md`.

use std::path::PathBuf;
use std::sync::OnceLock;

use egui::Color32;
use serde_json::{Map, Value, json};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;

use crate::ui_roles::{ThemeSource, resolve_ui_theme};

/// Colors of the viewport overlays and gizmos (`crate::overlays`, `crate::audio_gizmo`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayTokens {
    /// Major grid line (the axes through the target).
    pub grid_major: Color32,
    /// Minor grid line.
    pub grid_minor: Color32,
    /// Selection outline.
    pub selection_outline: Color32,
    /// Selection bounds and light ranges.
    pub bounds: Color32,
    /// Light icons.
    pub icon_light: Color32,
    /// Camera icons.
    pub icon_camera: Color32,
    /// Decal icons.
    pub icon_decal: Color32,
    /// Audio emitter icons.
    pub icon_audio: Color32,
    /// Gizmo X axis (a meaning-carrying hue; the brand keeps it RGB).
    pub gizmo_x: Color32,
    /// Gizmo Y axis.
    pub gizmo_y: Color32,
    /// Gizmo Z axis.
    pub gizmo_z: Color32,
    /// Rotate-mode ring.
    pub gizmo_ring: Color32,
    /// Audio emitter minimum distance and marker.
    pub audio_min: Color32,
    /// Audio emitter maximum distance.
    pub audio_max: Color32,
    /// Audio emitter cone.
    pub audio_cone: Color32,
    /// Reverb zone outline.
    pub reverb_zone: Color32,
}

impl OverlayTokens {
    /// The colors the overlays had before tokens (also the `pub const`s in `overlays`,
    /// `viewport` and `audio_gizmo`).
    #[must_use]
    pub const fn regia_default() -> Self {
        Self {
            grid_major: Color32::from_rgb(120, 120, 130),
            grid_minor: Color32::from_rgb(60, 60, 68),
            selection_outline: Color32::from_rgb(255, 150, 20),
            bounds: Color32::from_rgb(255, 236, 140),
            icon_light: Color32::from_rgb(255, 222, 70),
            icon_camera: Color32::from_rgb(90, 200, 255),
            icon_decal: Color32::from_rgb(200, 120, 255),
            icon_audio: Color32::from_rgb(110, 250, 200),
            gizmo_x: Color32::from_rgb(220, 40, 40),
            gizmo_y: Color32::from_rgb(40, 200, 60),
            gizmo_z: Color32::from_rgb(50, 90, 230),
            gizmo_ring: Color32::from_rgb(230, 190, 60),
            audio_min: Color32::from_rgb(80, 200, 230),
            audio_max: Color32::from_rgb(40, 110, 160),
            audio_cone: Color32::from_rgb(240, 140, 220),
            reverb_zone: Color32::from_rgb(170, 120, 240),
        }
    }

    /// Every color the overlays paint, so captures can skip them when measuring the picture.
    #[must_use]
    pub fn colors(&self) -> [Color32; 16] {
        [
            self.grid_major,
            self.grid_minor,
            self.selection_outline,
            self.bounds,
            self.icon_light,
            self.icon_camera,
            self.icon_decal,
            self.icon_audio,
            self.gizmo_x,
            self.gizmo_y,
            self.gizmo_z,
            self.gizmo_ring,
            self.audio_min,
            self.audio_max,
            self.audio_cone,
            self.reverb_zone,
        ]
    }
}

impl Default for OverlayTokens {
    fn default() -> Self {
        Self::regia_default()
    }
}

/// Fills, strokes and text colors of interactive widgets (the states egui draws a button in).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WidgetTokens {
    /// Idle button and checkbox fill.
    pub idle: Color32,
    /// Hovered fill.
    pub hover: Color32,
    /// Pressed fill.
    pub pressed: Color32,
    /// Fill of an open menu button or combo box.
    pub open: Color32,
    /// Idle button text.
    pub button_text: Color32,
    /// Hovered button text.
    pub hover_text: Color32,
    /// Open widget text.
    pub open_text: Color32,
    /// Hovered widget outline.
    pub hover_line: Color32,
    /// Hyperlink text.
    pub link: Color32,
    /// Inline code background.
    pub code: Color32,
    /// The boundary of a control at rest: the outline of buttons, text fields and checkboxes
    /// (WCAG 1.4.11 needs 3:1 against the surface; the brand's `control` / `edge`). Fully
    /// transparent leaves egui's rest outline alone (the classic look). JSON key `widgets.edge`.
    pub edge: Color32,
}

impl WidgetTokens {
    /// egui's dark widget colors (the [`ThemeTokens::regia_classic`] widgets; no rest outline).
    #[must_use]
    pub const fn regia_default() -> Self {
        Self {
            idle: Color32::from_gray(60),
            hover: Color32::from_gray(70),
            pressed: Color32::from_gray(55),
            open: Color32::from_gray(45),
            button_text: Color32::from_gray(180),
            hover_text: Color32::from_gray(240),
            open_text: Color32::from_gray(210),
            hover_line: Color32::from_gray(150),
            link: Color32::from_rgb(90, 170, 255),
            code: Color32::from_gray(64),
            edge: Color32::TRANSPARENT,
        }
    }
}

impl Default for WidgetTokens {
    fn default() -> Self {
        Self::regia_default()
    }
}

/// Accents the editor panels draw with (animation canvases, status labels, log levels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccentTokens {
    /// The active or current item (current state, selected key, running node).
    pub active: Color32,
    /// A selected or focused item (selected node, hovered link).
    pub select: Color32,
    /// A playhead, a failed item or a removed key.
    pub alert: Color32,
    /// A passing or valid item.
    pub ok: Color32,
    /// The "· unsaved changes" label and warning-level status text.
    pub unsaved: Color32,
    /// Hard errors in status lines.
    pub error_text: Color32,
    /// Log level ERROR.
    pub log_error: Color32,
    /// Log level WARN.
    pub log_warn: Color32,
    /// Log level INFO.
    pub log_info: Color32,
    /// The play-mode tint: the border around the play viewport and the play toolbar's tint while a
    /// session runs (ADR-7829). JSON key `play_mode`.
    pub play_mode: Color32,
    /// The tint while the server's world is viewed in a networked play session. JSON key `play_server`.
    pub play_server: Color32,
    /// The tints of clients 1-4 (client N uses entry `(N - 1) % 4`). JSON keys `play_client_1`..`_4`.
    pub play_clients: [Color32; 4],
}

impl AccentTokens {
    /// The accents the panels had inline.
    #[must_use]
    pub const fn regia_default() -> Self {
        Self {
            active: Color32::from_rgb(255, 150, 40),
            select: Color32::from_rgb(110, 170, 255),
            alert: Color32::from_rgb(255, 90, 90),
            ok: Color32::from_rgb(120, 200, 120),
            unsaved: Color32::YELLOW,
            error_text: Color32::RED,
            log_error: Color32::LIGHT_RED,
            log_warn: Color32::YELLOW,
            log_info: Color32::LIGHT_GREEN,
            play_mode: Color32::from_rgb(235, 120, 40),
            play_server: Color32::from_rgb(205, 90, 210),
            play_clients: [
                Color32::from_rgb(70, 170, 235),
                Color32::from_rgb(90, 200, 120),
                Color32::from_rgb(230, 200, 60),
                Color32::from_rgb(170, 140, 245),
            ],
        }
    }
}

impl Default for AccentTokens {
    fn default() -> Self {
        Self::regia_default()
    }
}

/// The brand's Warn on dark grounds (BRAND-GUIDE 3.5).
pub const BRAND_WARN: Color32 = Color32::from_rgb(0xfd, 0xba, 0x74);
/// The brand's Error on dark grounds.
pub const BRAND_ERROR: Color32 = Color32::from_rgb(0xff, 0x67, 0x67);
/// The brand's Success on dark grounds.
pub const BRAND_SUCCESS: Color32 = Color32::from_rgb(0x61, 0xff, 0xca);
/// The brand's Info on dark grounds.
pub const BRAND_INFO: Color32 = Color32::from_rgb(0x7a, 0xa2, 0xff);

/// An opaque color from `0xRRGGBB`.
const fn hex_rgb(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

impl AccentTokens {
    /// The accents of the dark brand looks: Signal white for the active and selected item, the
    /// functional colors (BRAND-GUIDE 3.5) for states, and the people colors (3.9: warm and purple
    /// hues only) for the play-session tints; play mode itself is Warn.
    #[must_use]
    pub const fn brand_dark() -> Self {
        Self {
            active: Color32::WHITE,
            select: Color32::WHITE,
            alert: BRAND_ERROR,
            ok: BRAND_SUCCESS,
            unsaved: BRAND_WARN,
            error_text: BRAND_ERROR,
            log_error: BRAND_ERROR,
            log_warn: BRAND_WARN,
            log_info: BRAND_INFO,
            play_mode: BRAND_WARN,
            play_server: hex_rgb(0xff_8f_b1),
            play_clients: [
                hex_rgb(0xb6_9c_ff),
                hex_rgb(0xff_9e_80),
                hex_rgb(0xf5_8a_e6),
                hex_rgb(0xd9_b3_8c),
            ],
        }
    }
}

/// One optional font family of the theme.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FontToken {
    /// Display name (the key the font gets in egui's font table).
    pub name: String,
    /// Path of a `.ttf`/`.otf` file on disk. `None` with a name from [`BRAND_FONTS`] uses the
    /// face compiled into the editor. A missing or unreadable file falls back to egui's default
    /// family.
    pub file: Option<PathBuf>,
}

impl FontToken {
    /// A font by name only (a [`BRAND_FONTS`] face, or nothing when the name is unknown).
    #[must_use]
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            file: None,
        }
    }
}

/// The theme's font families; `None` keeps egui's default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FontSet {
    /// Body and UI text (egui's `Proportional` family).
    pub proportional: Option<FontToken>,
    /// Code and numbers (egui's `Monospace` family).
    pub monospace: Option<FontToken>,
    /// Headings, registered as the named family [`DISPLAY_FAMILY`].
    pub display: Option<FontToken>,
    /// Prose (tooltips, descriptions, dialog and help text), registered as the named family
    /// [`PROSE_FAMILY`]. JSON key `fonts.prose`.
    pub prose: Option<FontToken>,
}

impl FontSet {
    /// The PiercingXX fonts (BRAND-GUIDE section 4): JetBrains Mono for labels, values and code
    /// (also the proportional family, as the brand kit's editor themes set it), Space Mono for
    /// headings, Geist for prose.
    #[must_use]
    pub fn brand() -> Self {
        Self {
            proportional: Some(FontToken::named(JETBRAINS_MONO)),
            monospace: Some(FontToken::named(JETBRAINS_MONO)),
            display: Some(FontToken::named(SPACE_MONO)),
            prose: Some(FontToken::named(GEIST)),
        }
    }
}

/// Name of the display font family (`egui::FontFamily::Name`); falls back to proportional when
/// the theme sets no display font.
pub const DISPLAY_FAMILY: &str = "display";

/// Name of the prose font family (`egui::FontFamily::Name`); falls back to proportional when the
/// theme sets no prose font.
pub const PROSE_FAMILY: &str = "prose";

/// JetBrains Mono (variable, rendered at its default weight 400).
pub const JETBRAINS_MONO: &str = "JetBrains Mono";
/// Space Mono Regular.
pub const SPACE_MONO: &str = "Space Mono";
/// Geist (variable, rendered at its default weight 400).
pub const GEIST: &str = "Geist";

/// The brand faces compiled into the editor (`crates/regia_editor/assets/fonts`, SIL OFL 1.1;
/// the licence texts ship beside them): a [`FontToken`] with one of these names and no file uses
/// these bytes.
pub const BRAND_FONTS: [(&str, &[u8]); 3] = [
    (
        JETBRAINS_MONO,
        include_bytes!("../assets/fonts/JetBrainsMono-wght.ttf"),
    ),
    (
        SPACE_MONO,
        include_bytes!("../assets/fonts/SpaceMono-Regular.ttf"),
    ),
    (GEIST, include_bytes!("../assets/fonts/Geist-wght.ttf")),
];

/// The bytes of a brand face by name.
#[must_use]
pub fn brand_font(name: &str) -> Option<&'static [u8]> {
    BRAND_FONTS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, bytes)| *bytes)
}

/// Every color and switch of the editor's look. Also a world resource (see [`tokens`]).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct ThemeTokens {
    /// Surfaces of non-interactive widgets and open popups.
    pub ground: Color32,
    /// Window and popup fill.
    pub raised: Color32,
    /// Side panel and central panel fill.
    pub panel: Color32,
    /// Text-edit, dock tab bar and other recessed fills.
    pub well: Color32,
    /// Hairlines: separators, window and panel outlines.
    pub line: Color32,
    /// Faint stripe or shading fill.
    pub shade: Color32,
    /// Secondary text. Equal to egui's derived weak text it leaves the visuals untouched.
    pub muted: Color32,
    /// Strong text and the pressed widget's text and outline.
    pub strong: Color32,
    /// Body text.
    pub text: Color32,
    /// The accent: selection fill (selection inverts to `signal` block + `on_signal` text).
    pub signal: Color32,
    /// Text and outline on a `signal` fill.
    pub on_signal: Color32,
    /// Warning text.
    pub warn: Color32,
    /// Error text.
    pub error: Color32,
    /// The lit edge of raised surfaces (BRAND-GUIDE 3.7): a 1 px line of this color along the top
    /// of windows and popups, the brand's depth cue instead of a shadow (white at 6 % on dark
    /// grounds, 60 % on light ones). Fully transparent draws none. JSON key `lit_edge`.
    pub lit_edge: Color32,
    /// Overlay and gizmo colors.
    pub overlay: OverlayTokens,
    /// Widget state colors.
    pub widgets: WidgetTokens,
    /// Panel accents and log levels.
    pub accents: AccentTokens,
    /// Font families.
    pub fonts: FontSet,
    /// No window or popup shadows.
    pub flat: bool,
    /// Procedural graph pin colors, one per data kind in `regia_procgen::DataKind::ALL` order
    /// (spec 67 § 2.2). JSON key `data_kinds`, an object keyed by [`DATA_KIND_KEYS`].
    pub data_kinds: [Color32; 10],
}

/// The JSON keys of [`ThemeTokens::data_kinds`], in `regia_procgen::DataKind::ALL` order.
pub const DATA_KIND_KEYS: [&str; 10] = [
    "points",
    "table",
    "spline",
    "polygon",
    "shape",
    "ground",
    "mask",
    "mesh",
    "plant_skeleton",
    "plant_mesh",
];

/// The default pin colors: a colorblind-safe set (the Okabe-Ito hues plus neutrals and two
/// greens), the colors the procedural graph panel had before they were tokens.
pub const DATA_KIND_DEFAULTS: [Color32; 10] = [
    Color32::from_rgb(229, 158, 0),
    Color32::from_rgb(86, 181, 232),
    Color32::from_rgb(0, 158, 114),
    Color32::from_rgb(239, 226, 66),
    Color32::from_rgb(0, 114, 178),
    Color32::from_rgb(214, 94, 0),
    Color32::from_rgb(204, 119, 165),
    Color32::from_rgb(153, 153, 153),
    Color32::from_rgb(140, 89, 51),
    Color32::from_rgb(89, 204, 89),
];

impl ThemeTokens {
    /// The default editor look: Piercing Graphite ([`Self::piercing_graphite`]).
    #[must_use]
    pub fn regia_default() -> Self {
        Self::piercing_graphite()
    }

    /// Piercing Graphite (BRAND-GUIDE 3.8, the kit's `piercingxx-graphite`): ground `#131316`,
    /// panels `#18181B`, windows and menus `#1F1F23`, fields `#0F0F12`, hover `#26262B`, hairlines
    /// `#2B2B30`, control outlines `#7C7C84`, text `#E7E7E8`, secondary text `#A8A8B0`. Signal
    /// white is the selection (inverted: white block, ground-colored text), links, focus and the
    /// active item; the functional colors mark states only. Flat, with the lit top edge.
    #[must_use]
    pub fn piercing_graphite() -> Self {
        let signal = Color32::WHITE;
        Self {
            ground: hex_rgb(0x131316),
            raised: hex_rgb(0x1f_1f_23),
            panel: hex_rgb(0x18_18_1b),
            well: hex_rgb(0x0f_0f_12),
            line: hex_rgb(0x2b_2b_30),
            shade: hex_rgb(0x1d_1d_21),
            muted: hex_rgb(0xa8_a8_b0),
            strong: hex_rgb(0xd0_d0_d1),
            text: hex_rgb(0xe7_e7_e8),
            signal,
            on_signal: hex_rgb(0x131316),
            warn: BRAND_WARN,
            error: BRAND_ERROR,
            lit_edge: Color32::from_rgba_unmultiplied(255, 255, 255, 15),
            overlay: OverlayTokens::regia_default(),
            widgets: WidgetTokens {
                idle: hex_rgb(0x1f_1f_23),
                hover: hex_rgb(0x26_26_2b),
                pressed: hex_rgb(0x2e_2e_33),
                open: hex_rgb(0x26_26_2b),
                button_text: hex_rgb(0xe7_e7_e8),
                hover_text: hex_rgb(0xe7_e7_e8),
                open_text: hex_rgb(0xe7_e7_e8),
                hover_line: hex_rgb(0xa8_a8_b0),
                link: signal,
                code: hex_rgb(0x0f_0f_12),
                edge: hex_rgb(0x7c_7c_84),
            },
            accents: AccentTokens::brand_dark(),
            fonts: FontSet::brand(),
            flat: true,
            data_kinds: DATA_KIND_DEFAULTS,
        }
    }

    /// AMOLED Night (BRAND-GUIDE 3.1-3.3): Ink `#000000` ground with the brand's neutral ramp
    /// (`#09090B` panels, `#131316` windows, `#18181B` fields) and white text at the opacity
    /// stops, flattened to opaque colors. Otherwise as [`Self::piercing_graphite`].
    #[must_use]
    pub fn amoled_night() -> Self {
        let signal = Color32::WHITE;
        let text = hex_rgb(0xe6_e6_e6);
        let muted = hex_rgb(0x95_95_97);
        Self {
            ground: Color32::BLACK,
            raised: hex_rgb(0x131316),
            panel: hex_rgb(0x09_09_0b),
            well: hex_rgb(0x18_18_1b),
            line: hex_rgb(0x1a_1a_1c),
            shade: hex_rgb(0x10_10_12),
            muted,
            strong: hex_rgb(0xcd_cd_ce),
            text,
            signal,
            on_signal: Color32::BLACK,
            warn: BRAND_WARN,
            error: BRAND_ERROR,
            lit_edge: Color32::from_rgba_unmultiplied(255, 255, 255, 15),
            overlay: OverlayTokens::regia_default(),
            widgets: WidgetTokens {
                idle: hex_rgb(0x131316),
                hover: hex_rgb(0x20_20_23),
                pressed: hex_rgb(0x29_29_2c),
                open: hex_rgb(0x20_20_23),
                button_text: text,
                hover_text: text,
                open_text: text,
                hover_line: muted,
                link: signal,
                code: hex_rgb(0x18_18_1b),
                edge: hex_rgb(0x6c_6c_6e),
            },
            accents: AccentTokens::brand_dark(),
            fonts: FontSet::brand(),
            flat: true,
            data_kinds: DATA_KIND_DEFAULTS,
        }
    }

    /// Paper (BRAND-GUIDE 3.3, the light look): ground `#F3EEE2`, ink text at the opacity stops
    /// flattened to opaque colors, Ink `#000000` as the Signal (inverted: black block, paper text),
    /// the light functional colors (3.5) and the lit edge at white 60 %.
    #[must_use]
    pub fn paper() -> Self {
        let signal = Color32::BLACK;
        let text = hex_rgb(0x18_18_17);
        let muted = hex_rgb(0x58_56_52);
        let warn = hex_rgb(0x6d5032);
        let error = hex_rgb(0x8f_39_39);
        Self {
            ground: hex_rgb(0xf3_ee_e2),
            raised: hex_rgb(0xe6_e1_d6),
            panel: hex_rgb(0xec_e7_db),
            well: hex_rgb(0xe1_dc_d1),
            line: hex_rgb(0xd6_d1_c7),
            shade: hex_rgb(0xe8_e3_d7),
            muted,
            strong: hex_rgb(0x2c_2b_29),
            text,
            signal,
            on_signal: hex_rgb(0xf3_ee_e2),
            warn,
            error,
            lit_edge: Color32::from_rgba_unmultiplied(255, 255, 255, 153),
            overlay: OverlayTokens::regia_default(),
            widgets: WidgetTokens {
                idle: hex_rgb(0xe6_e1_d6),
                hover: hex_rgb(0xdf_da_ce),
                pressed: hex_rgb(0xd6_d1_c5),
                open: hex_rgb(0xdf_da_ce),
                button_text: text,
                hover_text: text,
                open_text: text,
                hover_line: muted,
                link: signal,
                code: hex_rgb(0xe1_dc_d1),
                edge: hex_rgb(0x7a_77_6f),
            },
            accents: AccentTokens {
                active: signal,
                select: signal,
                alert: error,
                ok: hex_rgb(0x25_5f_4c),
                unsaved: warn,
                error_text: error,
                log_error: error,
                log_warn: warn,
                log_info: hex_rgb(0x3f_54_85),
                play_mode: warn,
                // The people colors (BRAND-GUIDE 3.9) darkened to 3:1 on Paper.
                play_server: hex_rgb(0xa3_38_5c),
                play_clients: [
                    hex_rgb(0x6b_4f_c4),
                    hex_rgb(0xa3_48_2c),
                    hex_rgb(0x9a_3a_8c),
                    hex_rgb(0x7a_5a_36),
                ],
            },
            fonts: FontSet::brand(),
            flat: true,
            data_kinds: DATA_KIND_DEFAULTS,
        }
    }

    /// The editor's look before the PiercingXX brand (ADR-229): egui's dark visuals and the
    /// editor's inline accents. Absent keys of a version 1 theme file mean these values, so a theme
    /// saved before the brand looks the same.
    #[must_use]
    pub fn regia_classic() -> Self {
        Self {
            ground: Color32::from_gray(27),
            raised: Color32::from_gray(27),
            panel: Color32::from_gray(27),
            well: Color32::from_gray(10),
            line: Color32::from_gray(60),
            shade: Color32::from_additive_luminance(5),
            // egui derives weak text as the text color at 0.6 alpha; `apply` keeps `None`
            // while `muted` equals that derivation.
            muted: Color32::from_gray(140).gamma_multiply(0.6),
            strong: Color32::WHITE,
            text: Color32::from_gray(140),
            signal: Color32::from_rgb(0, 92, 128),
            on_signal: Color32::from_rgb(192, 222, 255),
            warn: Color32::from_rgb(255, 143, 0),
            error: Color32::from_rgb(255, 0, 0),
            lit_edge: Color32::TRANSPARENT,
            overlay: OverlayTokens::regia_default(),
            widgets: WidgetTokens::regia_default(),
            accents: AccentTokens::regia_default(),
            fonts: FontSet::default(),
            flat: false,
            data_kinds: DATA_KIND_DEFAULTS,
        }
    }

    /// Parses tokens from JSON (see the module docs). Keys that are absent keep the Regia Engine
    /// default, unknown keys are ignored.
    ///
    /// # Errors
    /// When the text is not a JSON object, or a known key holds an unparsable color.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let root: Value = serde_json::from_str(text).map_err(|e| format!("theme JSON: {e}"))?;
        let Value::Object(root) = root else {
            return Err("theme JSON: expected an object".to_string());
        };
        let mut tokens = Self::regia_default();
        tokens.merge(&root)?;
        Ok(tokens)
    }

    /// Applies the keys present in `object` over `self` (also used by `theme.set`).
    ///
    /// A present `flat` must be a bool. A present `colors`, `overlay`, `widgets`, `accents` or
    /// `fonts` must be an object. Color keys inside `colors` apply first; the same key at the
    /// top level wins. On error `self` is unchanged.
    ///
    /// # Errors
    /// When a known key holds an unparsable value.
    pub fn merge(&mut self, object: &Map<String, Value>) -> Result<(), String> {
        if let Some(value) = object.get("flat")
            && !matches!(value, Value::Bool(_))
        {
            return Err("theme key `flat`: expected a bool".to_string());
        }
        for key in ["colors", "overlay", "widgets", "accents", "fonts", "data_kinds"] {
            if let Some(value) = object.get(key)
                && !matches!(value, Value::Object(_))
            {
                return Err(format!("theme key `{key}`: expected an object"));
            }
        }
        let mut next = self.clone();
        if let Some(Value::Object(colors)) = object.get("colors") {
            next.apply_color_keys(colors)?;
        }
        next.apply_color_keys(object)?;
        if let Some(Value::Bool(flat)) = object.get("flat") {
            next.flat = *flat;
        }
        if let Some(Value::Object(overlay)) = object.get("overlay") {
            next.overlay.merge(overlay)?;
        }
        if let Some(Value::Object(widgets)) = object.get("widgets") {
            next.widgets.merge(widgets)?;
        }
        if let Some(Value::Object(accents)) = object.get("accents") {
            next.accents.merge(accents)?;
        }
        if let Some(Value::Object(fonts)) = object.get("fonts") {
            next.fonts.merge(fonts)?;
        }
        if let Some(Value::Object(kinds)) = object.get("data_kinds") {
            merge_slots(&DATA_KIND_KEYS, next.data_kinds.each_mut(), kinds)?;
        }
        *self = next;
        Ok(())
    }

    /// Writes the color keys present in `source`. The first matching alias wins.
    fn apply_color_keys(&mut self, source: &Map<String, Value>) -> Result<(), String> {
        let get = |names: &[&str], slot: &mut Color32| -> Result<(), String> {
            for name in names {
                if let Some(value) = source.get(*name) {
                    *slot = parse_color(value).map_err(|e| format!("theme key `{name}`: {e}"))?;
                    return Ok(());
                }
            }
            Ok(())
        };
        get(&["ground", "ink"], &mut self.ground)?;
        get(&["raised"], &mut self.raised)?;
        get(&["panel", "graphite"], &mut self.panel)?;
        get(&["well", "slate"], &mut self.well)?;
        get(&["line"], &mut self.line)?;
        get(&["shade"], &mut self.shade)?;
        get(&["muted"], &mut self.muted)?;
        get(&["strong"], &mut self.strong)?;
        get(&["text"], &mut self.text)?;
        get(&["signal"], &mut self.signal)?;
        get(&["on_signal"], &mut self.on_signal)?;
        get(&["warn"], &mut self.warn)?;
        get(&["error"], &mut self.error)?;
        get(&["lit_edge"], &mut self.lit_edge)?;
        Ok(())
    }

    /// The tokens as JSON in the shape [`Self::from_json`] reads.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "ground": hex(self.ground),
            "raised": hex(self.raised),
            "panel": hex(self.panel),
            "well": hex(self.well),
            "line": hex(self.line),
            "shade": hex(self.shade),
            "muted": hex(self.muted),
            "strong": hex(self.strong),
            "text": hex(self.text),
            "signal": hex(self.signal),
            "on_signal": hex(self.on_signal),
            "warn": hex(self.warn),
            "error": hex(self.error),
            "lit_edge": hex(self.lit_edge),
            "flat": self.flat,
            "overlay": colors_json(&OVERLAY_KEYS, &self.overlay.colors()),
            "widgets": colors_json(&WIDGET_KEYS, &self.widgets.colors()),
            "accents": colors_json(&ACCENT_KEYS, &self.accents.colors()),
            "data_kinds": colors_json(&DATA_KIND_KEYS, &self.data_kinds),
            "fonts": {
                "proportional": font_json(self.fonts.proportional.as_ref()),
                "monospace": font_json(self.fonts.monospace.as_ref()),
                "display": font_json(self.fonts.display.as_ref()),
                "prose": font_json(self.fonts.prose.as_ref()),
            },
        })
    }
}

impl Default for ThemeTokens {
    fn default() -> Self {
        Self::regia_default()
    }
}

const OVERLAY_KEYS: [&str; 16] = [
    "grid_major",
    "grid_minor",
    "selection_outline",
    "bounds",
    "icon_light",
    "icon_camera",
    "icon_decal",
    "icon_audio",
    "gizmo_x",
    "gizmo_y",
    "gizmo_z",
    "gizmo_ring",
    "audio_min",
    "audio_max",
    "audio_cone",
    "reverb_zone",
];
const WIDGET_KEYS: [&str; 11] = [
    "idle",
    "hover",
    "pressed",
    "open",
    "button_text",
    "hover_text",
    "open_text",
    "hover_line",
    "link",
    "code",
    "edge",
];
const ACCENT_KEYS: [&str; 15] = [
    "active",
    "select",
    "alert",
    "ok",
    "unsaved",
    "error_text",
    "log_error",
    "log_warn",
    "log_info",
    "play_mode",
    "play_server",
    "play_client_1",
    "play_client_2",
    "play_client_3",
    "play_client_4",
];

impl OverlayTokens {
    fn slots(&mut self) -> [&mut Color32; 16] {
        [
            &mut self.grid_major,
            &mut self.grid_minor,
            &mut self.selection_outline,
            &mut self.bounds,
            &mut self.icon_light,
            &mut self.icon_camera,
            &mut self.icon_decal,
            &mut self.icon_audio,
            &mut self.gizmo_x,
            &mut self.gizmo_y,
            &mut self.gizmo_z,
            &mut self.gizmo_ring,
            &mut self.audio_min,
            &mut self.audio_max,
            &mut self.audio_cone,
            &mut self.reverb_zone,
        ]
    }

    fn merge(&mut self, object: &Map<String, Value>) -> Result<(), String> {
        merge_slots(&OVERLAY_KEYS, self.slots(), object)
    }
}

impl WidgetTokens {
    fn colors(&self) -> [Color32; 11] {
        [
            self.idle,
            self.hover,
            self.pressed,
            self.open,
            self.button_text,
            self.hover_text,
            self.open_text,
            self.hover_line,
            self.link,
            self.code,
            self.edge,
        ]
    }

    fn merge(&mut self, object: &Map<String, Value>) -> Result<(), String> {
        let slots = [
            &mut self.idle,
            &mut self.hover,
            &mut self.pressed,
            &mut self.open,
            &mut self.button_text,
            &mut self.hover_text,
            &mut self.open_text,
            &mut self.hover_line,
            &mut self.link,
            &mut self.code,
            &mut self.edge,
        ];
        merge_slots(&WIDGET_KEYS, slots, object)
    }
}

impl AccentTokens {
    fn colors(&self) -> [Color32; 15] {
        [
            self.active,
            self.select,
            self.alert,
            self.ok,
            self.unsaved,
            self.error_text,
            self.log_error,
            self.log_warn,
            self.log_info,
            self.play_mode,
            self.play_server,
            self.play_clients[0],
            self.play_clients[1],
            self.play_clients[2],
            self.play_clients[3],
        ]
    }

    fn merge(&mut self, object: &Map<String, Value>) -> Result<(), String> {
        let slots = [
            &mut self.active,
            &mut self.select,
            &mut self.alert,
            &mut self.ok,
            &mut self.unsaved,
            &mut self.error_text,
            &mut self.log_error,
            &mut self.log_warn,
            &mut self.log_info,
            &mut self.play_mode,
            &mut self.play_server,
        ];
        let [c1, c2, c3, c4] = &mut self.play_clients;
        let slots = {
            let [a, b, c, d, e, f, g, h, i, j, k] = slots;
            [a, b, c, d, e, f, g, h, i, j, k, c1, c2, c3, c4]
        };
        merge_slots(&ACCENT_KEYS, slots, object)
    }
}

impl FontSet {
    fn merge(&mut self, object: &Map<String, Value>) -> Result<(), String> {
        for (key, slot) in [
            ("proportional", &mut self.proportional),
            ("monospace", &mut self.monospace),
            ("display", &mut self.display),
            ("prose", &mut self.prose),
        ] {
            match object.get(key) {
                None => {}
                Some(Value::Null) => *slot = None,
                Some(Value::String(name)) => {
                    *slot = Some(FontToken {
                        name: name.clone(),
                        file: None,
                    });
                }
                Some(Value::Object(font)) => {
                    let name = font
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("theme font `{key}`: missing `name`"))?;
                    *slot = Some(FontToken {
                        name: name.to_string(),
                        file: font.get("file").and_then(Value::as_str).map(PathBuf::from),
                    });
                }
                Some(_) => return Err(format!("theme font `{key}`: expected a name or object")),
            }
        }
        Ok(())
    }
}

fn merge_slots<const N: usize>(
    keys: &[&str; N],
    slots: [&mut Color32; N],
    object: &Map<String, Value>,
) -> Result<(), String> {
    for (key, slot) in keys.iter().zip(slots) {
        if let Some(value) = object.get(*key) {
            *slot = parse_color(value).map_err(|e| format!("theme key `{key}`: {e}"))?;
        }
    }
    Ok(())
}

fn colors_json(keys: &[&str], colors: &[Color32]) -> Value {
    let mut map = Map::new();
    for (key, color) in keys.iter().zip(colors) {
        map.insert((*key).to_string(), Value::String(hex(*color)));
    }
    Value::Object(map)
}

fn font_json(font: Option<&FontToken>) -> Value {
    match font {
        None => Value::Null,
        Some(font) => json!({
            "name": font.name,
            "file": font.file.as_ref().map(|p| p.display().to_string()),
        }),
    }
}

/// `"#rrggbb"` (opaque) or `"#rrggbbaa"` (unmultiplied alpha).
fn hex(color: Color32) -> String {
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

pub(crate) fn parse_color(value: &Value) -> Result<Color32, String> {
    let text = match value {
        Value::String(text) => text.as_str(),
        // The brand file nests `{ "hex": "#..." }`.
        Value::Object(inner) => inner
            .get("hex")
            .or_else(|| inner.get("value"))
            .and_then(Value::as_str)
            .ok_or("expected a hex string")?,
        _ => return Err("expected a hex string".to_string()),
    };
    let digits = text.strip_prefix('#').ok_or("expected `#rrggbb`")?;
    if !digits.is_ascii() || (digits.len() != 6 && digits.len() != 8) {
        return Err(format!("`{text}` is not `#rrggbb` or `#rrggbbaa`"));
    }
    let byte =
        |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).map_err(|_| format!("`{text}` is not hex"));
    let (r, g, b) = (byte(0)?, byte(2)?, byte(4)?);
    if digits.len() == 8 {
        Ok(Color32::from_rgba_unmultiplied(r, g, b, byte(6)?))
    } else {
        Ok(Color32::from_rgb(r, g, b))
    }
}

/// The outline of a control at rest that is filled with the panel color: the theme's `line`, or
/// the hovered outline (`widgets.hover_line`) when `line` is a hairline under WCAG's 3:1 against
/// the panel (the UI themes' `color.border`; ADR-8589).
#[must_use]
pub fn idle_outline(tokens: &ThemeTokens) -> Color32 {
    let panel = tokens.panel;
    if crate::a11y_audit::contrast_ratio(tokens.line, panel) >= crate::a11y_audit::NON_TEXT_AA {
        tokens.line
    } else {
        tokens.widgets.hover_line
    }
}

/// The egui `Visuals` for `tokens`.
#[must_use]
pub fn visuals(tokens: &ThemeTokens) -> egui::Visuals {
    use egui::{Stroke, epaint::Shadow};
    let mut v = egui::Visuals::dark();
    let w = &tokens.widgets;
    // A light look (Paper, a light UI theme) tells egui so (it picks light defaults for what the
    // tokens do not set, such as image tints and the text cursor's preview).
    v.dark_mode =
        crate::a11y_audit::relative_luminance(crate::a11y_audit::over(tokens.panel, Color32::BLACK)) < 0.5;
    v.panel_fill = tokens.panel;
    v.window_fill = tokens.raised;
    v.extreme_bg_color = tokens.well;
    v.faint_bg_color = tokens.shade;
    v.window_stroke = Stroke::new(v.window_stroke.width, tokens.line);
    v.warn_fg_color = tokens.warn;
    v.error_fg_color = tokens.error;
    v.hyperlink_color = w.link;
    v.code_bg_color = w.code;
    v.selection.bg_fill = tokens.signal;
    v.selection.stroke.color = tokens.on_signal;
    // `weak_text_color` stays `None` (derived from the text color) while `muted` is that
    // derivation, so the default theme leaves the visuals identical to egui's.
    if tokens.muted != tokens.text.gamma_multiply(v.weak_text_alpha) {
        v.weak_text_color = Some(tokens.muted);
    }
    let set =
        |slot: &mut egui::style::WidgetVisuals, fill: Option<Color32>, line: Option<Color32>, fg: Color32| {
            if let Some(fill) = fill {
                slot.bg_fill = fill;
                slot.weak_bg_fill = fill;
            }
            if let Some(line) = line {
                slot.bg_stroke.color = line;
            }
            slot.fg_stroke.color = fg;
        };
    let widgets = &mut v.widgets;
    set(
        &mut widgets.noninteractive,
        Some(tokens.ground),
        Some(tokens.line),
        tokens.text,
    );
    set(&mut widgets.inactive, Some(w.idle), None, w.button_text);
    // A control filled with the panel color (the high-contrast theme and the UI themes,
    // ADR-8589) keeps a visible boundary (WCAG 1.4.11): [`idle_outline`] as its outline. Other
    // themes are unchanged.
    if w.idle == tokens.panel {
        widgets.inactive.bg_stroke = Stroke::new(1.0, idle_outline(tokens));
        // egui draws a slider's rail with `bg_fill` alone (no outline), and checkbox and radio
        // interiors with it: the recessed `well` keeps them off the panel. Buttons, fields and
        // dropdowns use `weak_bg_fill`, which stays the panel color.
        widgets.inactive.bg_fill = tokens.well;
    }
    set(
        &mut widgets.hovered,
        Some(w.hover),
        Some(w.hover_line),
        w.hover_text,
    );
    set(
        &mut widgets.active,
        Some(w.pressed),
        Some(tokens.strong),
        tokens.strong,
    );
    set(&mut widgets.open, None, Some(tokens.line), w.open_text);
    widgets.open.weak_bg_fill = w.open;
    widgets.open.bg_fill = tokens.ground;
    if w.edge.a() > 0 {
        // The brand looks: every control at rest has its 3:1 outline (buttons, fields,
        // checkboxes), and a control with keyboard focus (egui draws a focused control in its
        // `active` style; a click does not focus a button) gets the 2 px Signal focus ring.
        widgets.inactive.bg_stroke = Stroke::new(1.0, w.edge);
        widgets.active.bg_stroke = Stroke::new(FOCUS_RING_WIDTH, tokens.signal);
        v.text_cursor.stroke.color = tokens.text;
    }
    if tokens.flat {
        v.window_shadow = Shadow::NONE;
        v.popup_shadow = Shadow::NONE;
        // Flat looks use the brand's radii: 2 on controls, 4 on containers.
        for slot in [
            &mut widgets.noninteractive,
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
            &mut widgets.open,
        ] {
            slot.corner_radius = egui::CornerRadius::same(CONTROL_RADIUS);
        }
        v.window_corner_radius = egui::CornerRadius::same(CONTAINER_RADIUS);
        v.menu_corner_radius = egui::CornerRadius::same(CONTAINER_RADIUS);
    }
    if tokens.lit_edge.a() > 0 {
        // The lit edge (BRAND-GUIDE 3.7) is not a shadow: a 1 px band of `lit_edge` right above
        // the top edge of each window and popup, with no blur or spread (egui paints the "shadow"
        // shape behind the window, so only that band shows).
        let edge = Shadow {
            offset: [0, -1],
            blur: 0,
            spread: 0,
            color: tokens.lit_edge,
        };
        v.window_shadow = edge;
        v.popup_shadow = edge;
    }
    v
}

/// Width of the keyboard focus ring, points (the brand's 2 px).
pub const FOCUS_RING_WIDTH: f32 = 2.0;
/// Corner radius of controls in the flat looks (`radius.control`).
pub const CONTROL_RADIUS: u8 = 2;
/// Corner radius of windows, menus and popups in the flat looks (`radius.container`).
pub const CONTAINER_RADIUS: u8 = 4;

/// Paints the 2 px Signal focus ring around a focused text field. Buttons, checkboxes and other
/// controls get theirs from their `active` style ([`visuals`]); a text field draws its focus frame
/// with the selection stroke, which in the brand looks is the ground-colored text of the inverted
/// selection and so does not show on a field. Call once per frame after the panels are drawn. Does
/// nothing when the theme has no control outline (`widgets.edge` transparent: the classic look).
pub fn paint_focus_ring(ctx: &egui::Context) {
    let tokens = ui_tokens(ctx);
    if tokens.widgets.edge.a() == 0 {
        return;
    }
    let Some(id) = ctx.memory(|m| m.focused()) else {
        return;
    };
    if egui::text_edit::TextEditState::load(ctx, id).is_none() {
        return;
    }
    let Some(response) = ctx.read_response(id) else {
        return;
    };
    let rect = response.interact_rect;
    if !rect.is_positive() {
        return;
    }
    ctx.layer_painter(response.layer_id).rect_stroke(
        rect,
        egui::CornerRadius::same(CONTROL_RADIUS),
        egui::Stroke::new(FOCUS_RING_WIDTH, tokens.signal),
        egui::StrokeKind::Outside,
    );
}

/// The `egui_dock` style for an egui style that [`apply`] already themed (tab bar, separators
/// and tab colors derive from the themed `Visuals`).
#[must_use]
pub fn dock_style(style: &egui::Style) -> egui_dock::Style {
    let mut dock = egui_dock::Style::from_egui(style);
    // Tab titles grow with `appearance.text_scale` (ADR-7838): the bar is at least the button
    // text's line plus padding, so a scaled title is never cut. At egui's stock sizes this is below
    // egui_dock's 24-point default, which stays.
    let button = style
        .text_styles
        .get(&egui::TextStyle::Button)
        .map_or(0.0, |font| font.size);
    let needed = (button * 1.4 + 4.0).ceil();
    if needed > dock.tab_bar.height {
        dock.tab_bar.height = needed;
    }
    dock
}

fn font_data(font: &FontToken) -> Option<egui::FontData> {
    let Some(path) = font.file.as_ref() else {
        // A name only: one of the faces compiled into the editor, or nothing (egui's default).
        return brand_font(&font.name).map(egui::FontData::from_static);
    };
    match std::fs::read(path) {
        Ok(bytes) => Some(egui::FontData::from_owned(bytes)),
        Err(error) => {
            tracing::warn!(font = %font.name, path = %path.display(), %error, "theme font not readable; using the egui default");
            None
        }
    }
}

/// Fonts appended to the end of the proportional and the monospace family so every character the
/// editor writes has a glyph (ADR-8589): egui's own monospace face (`Hack`, already in egui's
/// default table) carries the arrows, triangles and shapes its proportional face lacks (`←` `→`
/// `▸` `▾` `●`), then the regular Inter face and the script subsets the runtime UI embeds
/// (`regia::ui::text::embedded_font_data`: `✓` `①` `⎇` `⤓`, Arabic, Hebrew, Devanagari, Thai,
/// Chinese, Japanese kana, Korean). The bold and italic Inter faces add no glyphs, and the color
/// emoji subset is skipped (egui draws outline glyphs only). No font file is added to the repo.
fn add_fallback_fonts(defs: &mut egui::FontDefinitions) {
    let mut fallbacks: Vec<String> = Vec::new();
    if defs.font_data.contains_key(EGUI_MONOSPACE_FONT) {
        fallbacks.push(EGUI_MONOSPACE_FONT.to_owned());
    }
    for (file, bytes) in regia::ui::text::embedded_font_data() {
        if file.contains("Bold") || file.contains("Italic") || file.contains("ColorEmoji") {
            continue;
        }
        let key = format!("regia_ui:{file}");
        defs.font_data.insert(
            key.clone(),
            std::sync::Arc::new(egui::FontData::from_static(bytes)),
        );
        fallbacks.push(key);
    }
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = defs.families.entry(family).or_default();
        for key in &fallbacks {
            if !list.contains(key) {
                list.push(key.clone());
            }
        }
    }
}

/// The key of egui's bundled monospace face in its default font table.
const EGUI_MONOSPACE_FONT: &str = "Hack";

/// egui's font table for `fonts`: the defaults, the symbol and script fallbacks
/// (`add_fallback_fonts`, ADR-8589), and every theme font that loads (from disk, or a
/// [`BRAND_FONTS`] face by name). A theme font goes first in its family and the family's existing
/// chain stays behind it, so a glyph the theme font lacks (symbols, CJK, Arabic) still comes from
/// the fallbacks. The named families [`DISPLAY_FAMILY`] and [`PROSE_FAMILY`] always exist: a theme
/// font, then the whole proportional chain. Same as [`font_definitions_over`] over egui's default
/// table.
#[must_use]
pub fn font_definitions(fonts: &FontSet) -> egui::FontDefinitions {
    font_definitions_over(egui::FontDefinitions::default(), fonts)
}

/// Prefix of the font-table keys of the theme's own faces (`theme:<font name>`).
const THEME_FONT_KEY: &str = "theme:";

/// The theme's font table merged over `base`, a host's table (ADR-8888, amending ADR-8589): every
/// face and family of `base` stays, the fallbacks are appended to the proportional and monospace
/// chains, each theme font goes first in the family it sets (the family's own chain stays behind
/// it), and [`DISPLAY_FAMILY`] and [`PROSE_FAMILY`], the two families the theme defines itself, are
/// replaced. Faces a previous theme installed (keys starting `theme:`) are taken out first, so a
/// theme change replaces them instead of stacking.
#[must_use]
pub fn font_definitions_over(base: egui::FontDefinitions, fonts: &FontSet) -> egui::FontDefinitions {
    let mut defs = base;
    defs.font_data.retain(|key, _| !key.starts_with(THEME_FONT_KEY));
    for list in defs.families.values_mut() {
        list.retain(|key| !key.starts_with(THEME_FONT_KEY));
    }
    add_fallback_fonts(&mut defs);
    let proportional_chain = defs
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    for name in [DISPLAY_FAMILY, PROSE_FAMILY] {
        defs.families
            .insert(egui::FontFamily::Name(name.into()), proportional_chain.clone());
    }
    let mut install = |font: &Option<FontToken>, families: &[egui::FontFamily]| {
        let Some(font) = font else { return };
        let Some(data) = font_data(font) else { return };
        let key = format!("{THEME_FONT_KEY}{}", font.name);
        defs.font_data.insert(key.clone(), std::sync::Arc::new(data));
        for family in families {
            let list = defs.families.entry(family.clone()).or_default();
            list.retain(|k| *k != key);
            list.insert(0, key.clone());
        }
    };
    let named = |name: &str| egui::FontFamily::Name(name.into());
    // The proportional font also heads the named families, so a theme with a proportional font
    // but no display or prose font draws headings and prose in it.
    install(
        &fonts.proportional,
        &[
            egui::FontFamily::Proportional,
            named(DISPLAY_FAMILY),
            named(PROSE_FAMILY),
        ],
    );
    install(&fonts.monospace, &[egui::FontFamily::Monospace]);
    install(&fonts.display, &[named(DISPLAY_FAMILY)]);
    install(&fonts.prose, &[named(PROSE_FAMILY)]);
    defs
}

/// Id of the tokens mirrored into an egui context.
fn ctx_key() -> egui::Id {
    egui::Id::new("regia_editor::theme::tokens")
}

/// Themes `ctx` with `tokens`: egui `Visuals` (which `egui_dock` styles derive from, see
/// [`dock_style`]), the font table, and the tokens themselves for [`ui_tokens`].
pub fn apply(ctx: &egui::Context, tokens: &ThemeTokens) {
    let previous: Option<std::sync::Arc<ThemeTokens>> = ctx.data(|d| d.get_temp(ctx_key()));
    ctx.set_visuals(visuals(tokens));
    // The first apply installs the editor's font table even for the default fonts: egui's own
    // table has no glyph for symbols the editor writes (ADR-8589). It is merged over the host's
    // table, so fonts the host installed stay (ADR-8888).
    if previous.as_deref().map(|p| &p.fonts) != Some(&tokens.fonts) {
        install_fonts(ctx, &tokens.fonts);
    }
    heading_family(ctx, tokens);
    ctx.data_mut(|d| d.insert_temp(ctx_key(), std::sync::Arc::new(tokens.clone())));
}

/// Installs the theme's font table for `fonts` over [`font_base`].
fn install_fonts(ctx: &egui::Context, fonts: &FontSet) {
    ctx.set_fonts(font_definitions_over(font_base(ctx), fonts));
    // egui installs new fonts at the start of the next pass; the display family exists only
    // from then on (`heading_family`).
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| d.insert_temp(fonts_pass_key(), pass));
}

/// Id of the host's font table in an egui context ([`set_host_fonts`]).
fn host_fonts_key() -> egui::Id {
    egui::Id::new("regia_editor::theme::host_fonts")
}

/// The table the theme's fonts are merged over (ADR-8888): the host's table from
/// [`set_host_fonts`] when there is one; otherwise, once a pass has run (egui builds its font
/// table at the start of the first pass, and reading it earlier panics), the table the context
/// draws with now, which holds whatever the host installed with `egui::Context::set_fonts` in an
/// earlier pass; otherwise egui's default table.
fn font_base(ctx: &egui::Context) -> egui::FontDefinitions {
    let host: Option<std::sync::Arc<egui::FontDefinitions>> = ctx.data(|d| d.get_temp(host_fonts_key()));
    if let Some(host) = host {
        return (*host).clone();
    }
    if ctx.cumulative_pass_nr() > 0 {
        return ctx.fonts(|fonts| fonts.definitions().clone());
    }
    egui::FontDefinitions::default()
}

/// Gives the theme the host's own font table (ADR-8888): every family and face in `defs` stays in
/// the table [`apply`] installs, and the theme only adds its faces and fallbacks and defines
/// [`DISPLAY_FAMILY`] and [`PROSE_FAMILY`] ([`font_definitions_over`]). Call it instead of
/// `egui::Context::set_fonts`, before or after the first [`apply`]: before, it installs `defs` and
/// the theme merges over them later; after, it reinstalls the merged table at once. A host that
/// calls `set_fonts` itself keeps its families only if it did so in an earlier pass than the first
/// [`apply`]: `set_fonts` takes effect at the next pass, and the theme cannot read a table egui has
/// not installed yet.
pub fn set_host_fonts(ctx: &egui::Context, defs: egui::FontDefinitions) {
    let defs = std::sync::Arc::new(defs);
    ctx.data_mut(|d| d.insert_temp(host_fonts_key(), defs.clone()));
    let applied: Option<std::sync::Arc<ThemeTokens>> = ctx.data(|d| d.get_temp(ctx_key()));
    match applied {
        Some(tokens) => install_fonts(ctx, &tokens.fonts),
        None => ctx.set_fonts((*defs).clone()),
    }
}

/// Applies the world's tokens to `ctx` when they differ from what the context last got.
pub fn sync(ctx: &egui::Context, world: &World) {
    let current = tokens(world);
    let stored: Option<std::sync::Arc<ThemeTokens>> = ctx.data(|d| d.get_temp(ctx_key()));
    if stored.as_deref() != Some(current) {
        apply(ctx, current);
    } else {
        heading_family(ctx, current);
    }
}

/// The pass in which [`apply`] last installed the theme's font table.
fn fonts_pass_key() -> egui::Id {
    egui::Id::new("regia_editor::theme::fonts_pass")
}

/// Points egui's heading text style at the display face (Space Mono in the brand looks, regular
/// weight: egui draws no synthetic bold) once the theme's font table is live, or back at the
/// proportional family when the theme has no display font. egui panics on a family its active font
/// table lacks, so the display family is used only from the pass after [`apply`] installed it.
fn heading_family(ctx: &egui::Context, tokens: &ThemeTokens) {
    let installed: Option<u64> = ctx.data(|d| d.get_temp(fonts_pass_key()));
    let want = match installed {
        Some(pass) if tokens.fonts.display.is_some() && ctx.cumulative_pass_nr() > pass => {
            egui::FontFamily::Name(DISPLAY_FAMILY.into())
        }
        _ => egui::FontFamily::Proportional,
    };
    let current = ctx
        .global_style()
        .text_styles
        .get(&egui::TextStyle::Heading)
        .map(|font| font.family.clone());
    if current.is_some_and(|family| family != want) {
        ctx.all_styles_mut(|style| {
            if let Some(font) = style.text_styles.get_mut(&egui::TextStyle::Heading) {
                font.family = want.clone();
            }
        });
    }
}

/// The tokens editor panels and overlays read instead of inline colors: the world's
/// [`ThemeTokens`] resource, or [`ThemeTokens::regia_default`] when none was inserted.
#[must_use]
pub fn tokens(world: &World) -> &ThemeTokens {
    match world.get_resource::<ThemeTokens>() {
        Some(tokens) => tokens,
        None => default_tokens(),
    }
}

fn default_tokens() -> &'static ThemeTokens {
    static DEFAULT: OnceLock<ThemeTokens> = OnceLock::new();
    DEFAULT.get_or_init(ThemeTokens::regia_default)
}

/// The tokens [`apply`] last gave `ctx` (the defaults before any), for panel code that has an
/// `egui::Ui` but no `World`.
#[must_use]
pub fn ui_tokens(ctx: &egui::Context) -> std::sync::Arc<ThemeTokens> {
    ctx.data(|d| d.get_temp(ctx_key()))
        .unwrap_or_else(|| std::sync::Arc::new(ThemeTokens::regia_default()))
}

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// The agent-protocol surface: `editor.get_theme`, `editor.set_theme`, the saved-theme methods
/// (`editor.theme_save`, `editor.theme_import`, `editor.theme_export`, `editor.theme_list`;
/// `book/agents/theme.md`) and `editor.color_vision` (`crate::chrome_color_vision`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    let mut methods = vec![
        (
            MethodSchema {
                name: "editor.get_theme",
                tool: "get_theme",
                description: "The editor's theme tokens (every color the chrome, panels and viewport \
                    overlays paint with) as JSON, in the shape editor.set_theme reads.",
                params: &[],
                result: "{ tokens: { ground, raised, panel, well, line, shade, muted, strong, text, signal, \
                    on_signal, warn, error, lit_edge, flat, overlay, widgets, accents, data_kinds, fonts }, is_default, theme_source }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_get_theme as Handler,
        ),
        (
            MethodSchema {
                name: "editor.set_theme",
                tool: "set_theme",
                description: "Change the editor's theme tokens. Keys not named keep their current value; \
                    `reset` first returns to the Regia Engine default; `from_prefs` first returns to the theme the preferences compose. `ui_theme` skins the editor from a UI theme \
                    (opt-in; `tokens` then apply on top). The window restyles on the next frame.",
                params: &[
                    ParamSchema {
                        name: "tokens",
                        kind: "object",
                        required: false,
                        description: "Token keys to change (colors as \"#rrggbb\" or \"#rrggbbaa\"; sections overlay, widgets, accents, data_kinds, fonts; flat).",
                    },
                    ParamSchema {
                        name: "reset",
                        kind: "boolean",
                        required: false,
                        description: "Start from the Regia Engine default instead of the current tokens.",
                    },
                    ParamSchema {
                        name: "from_prefs",
                        kind: "boolean",
                        required: false,
                        description: "Start from the theme the preferences compose (appearance.theme, appearance.custom_theme, the diagnostic colors), dropping unsaved token edits (the Theme page's Revert edits).",
                    },
                    ParamSchema {
                        name: "ui_theme",
                        kind: "string|object",
                        required: false,
                        description: "A UI theme name (regia-dark, regia-light, optionally `ui:`-prefixed) or an object of role colors (\"color.accent\": \"#rrggbb\"); maps its roles onto the editor tokens and sets theme_source to ui:<name>. Null or \"default\" returns to the ADR-229 look.",
                    },
                ],
                result: "{ tokens, is_default, theme_source }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_set_theme as Handler,
        ),
    ];
    methods.extend(theme_file_methods());
    methods.extend(crate::chrome_color_vision::methods());
    methods
}

fn theme_reply(world: &World) -> Value {
    let current = tokens(world);
    let source = world
        .get_resource::<ThemeSource>()
        .map_or_else(|| ThemeSource::Default.as_text(), ThemeSource::as_text);
    json!({
        "tokens": current.to_json(),
        "is_default": *current == ThemeTokens::regia_default(),
        "theme_source": source,
    })
}

/// `editor.get_theme`.
fn editor_get_theme(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(theme_reply(world))
}

/// `editor.set_theme`.
fn editor_set_theme(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if params.get("from_prefs").and_then(Value::as_bool) == Some(true) {
        crate::editor_prefs::apply_appearance(world);
    }
    let mut source = None;
    let mut next = if params.get("reset").and_then(Value::as_bool) == Some(true) {
        source = Some(ThemeSource::Default);
        ThemeTokens::regia_default()
    } else {
        tokens(world).clone()
    };
    match params.get("ui_theme") {
        None => {}
        Some(Value::Null) => {
            next = ThemeTokens::regia_default();
            source = Some(ThemeSource::Default);
        }
        Some(Value::String(name)) if name == "default" => {
            next = ThemeTokens::regia_default();
            source = Some(ThemeSource::Default);
        }
        Some(value) => {
            let (roles, picked) = resolve_ui_theme(value).map_err(RpcError::invalid_params)?;
            next = ThemeTokens::from_ui_roles(&roles).map_err(RpcError::invalid_params)?;
            source = Some(picked);
        }
    }
    match params.get("tokens") {
        None | Some(Value::Null) => {}
        Some(Value::Object(object)) => next.merge(object).map_err(RpcError::invalid_params)?,
        Some(_) => return Err(RpcError::invalid_params("tokens must be an object".to_string())),
    }
    if let Some(source) = source {
        world.insert_resource(source);
    }
    world.insert_resource(next);
    Ok(theme_reply(world))
}

// ---------------------------------------------------------------------------------------------
// Built-in editor themes, saved custom themes and diagnostic colors (spec addendum § 21,
// ADR-7838).

/// The name of the built-in high-contrast editor theme (an `appearance.theme` choice).
pub const HIGH_CONTRAST: &str = "high-contrast";

/// The single accent of [`ThemeTokens::high_contrast`]: pure yellow, 19.6:1 on black.
pub const HIGH_CONTRAST_ACCENT: Color32 = Color32::from_rgb(255, 255, 0);

impl ThemeTokens {
    /// The built-in high-contrast editor theme. Its colors follow one mechanical rule: pure black
    /// surfaces, pure white text and lines, and one accent ([`HIGH_CONTRAST_ACCENT`]) for the
    /// selection, hover, links and warning-level text; every text pair is far above WCAG AAA (7:1).
    /// Controls at rest are black with a white outline (see [`visuals`]), a hovered control is black
    /// with accent text and an accent outline, and keyboard focus is a 2 px accent ring; the lit
    /// edge above windows is white. It uses the brand fonts. Viewport overlay colors
    /// and graph pin colors keep the default set: they carry meaning over the 3D view, not over
    /// the chrome.
    #[must_use]
    pub fn high_contrast() -> Self {
        let black = Color32::BLACK;
        let white = Color32::WHITE;
        let accent = HIGH_CONTRAST_ACCENT;
        Self {
            ground: black,
            raised: black,
            panel: black,
            well: black,
            line: white,
            shade: black,
            muted: white,
            strong: white,
            text: white,
            signal: accent,
            on_signal: black,
            warn: accent,
            error: accent,
            // The lit edge is a white line like every other edge here (ADR-8679), drawn above the
            // windows' own white outline.
            lit_edge: white,
            overlay: OverlayTokens::regia_default(),
            widgets: WidgetTokens {
                // A hovered control stays black with accent text and an accent outline: egui draws
                // the hovered text of frameless buttons, links and headers straight on the black
                // panel, where black text on an accent block would vanish (ADR-8679).
                idle: black,
                hover: black,
                pressed: black,
                open: black,
                button_text: white,
                hover_text: accent,
                open_text: white,
                hover_line: accent,
                link: accent,
                code: black,
                edge: white,
            },
            accents: AccentTokens {
                active: accent,
                select: accent,
                alert: accent,
                ok: white,
                unsaved: accent,
                error_text: accent,
                log_error: accent,
                log_warn: accent,
                log_info: white,
                // The play tints of 3247 (ADR-7829): the one high-contrast accent.
                play_mode: accent,
                play_server: accent,
                play_clients: [accent; 4],
            },
            fonts: FontSet::brand(),
            flat: true,
            data_kinds: DATA_KIND_DEFAULTS,
        }
    }
}

impl ThemeTokens {
    /// Every color token with its dotted key (`signal`, `widgets.idle`, `accents.ok`,
    /// `overlay.grid_major`, `data_kinds.points`), in the Theme page's order. The keys name the
    /// same slots as the JSON shape of [`ThemeTokens::to_json`].
    pub fn color_slots_mut(&mut self) -> Vec<(String, &mut Color32)> {
        let mut out: Vec<(String, &mut Color32)> = vec![
            ("signal".into(), &mut self.signal),
            ("on_signal".into(), &mut self.on_signal),
            ("text".into(), &mut self.text),
            ("strong".into(), &mut self.strong),
            ("muted".into(), &mut self.muted),
            ("panel".into(), &mut self.panel),
            ("ground".into(), &mut self.ground),
            ("raised".into(), &mut self.raised),
            ("well".into(), &mut self.well),
            ("line".into(), &mut self.line),
            ("shade".into(), &mut self.shade),
            ("warn".into(), &mut self.warn),
            ("error".into(), &mut self.error),
            ("lit_edge".into(), &mut self.lit_edge),
        ];
        let w = &mut self.widgets;
        let widgets = [
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
        ];
        out.extend(
            WIDGET_KEYS
                .iter()
                .zip(widgets)
                .map(|(k, slot)| (format!("widgets.{k}"), slot)),
        );
        let a = &mut self.accents;
        let accents = [
            &mut a.active,
            &mut a.select,
            &mut a.alert,
            &mut a.ok,
            &mut a.unsaved,
            &mut a.error_text,
            &mut a.log_error,
            &mut a.log_warn,
            &mut a.log_info,
        ];
        out.extend(
            ACCENT_KEYS
                .iter()
                .zip(accents)
                .map(|(k, slot)| (format!("accents.{k}"), slot)),
        );
        out.extend(
            OVERLAY_KEYS
                .iter()
                .zip(self.overlay.slots())
                .map(|(k, slot)| (format!("overlay.{k}"), slot)),
        );
        out.extend(
            DATA_KIND_KEYS
                .iter()
                .zip(self.data_kinds.each_mut())
                .map(|(k, slot)| (format!("data_kinds.{k}"), slot)),
        );
        out
    }
}

/// A readable label for a token key from [`ThemeTokens::color_slots_mut`]
/// (`accents.log_warn` → `Accents: log warn`).
#[must_use]
pub fn token_label(key: &str) -> String {
    let (section, name) = match key.split_once('.') {
        Some((section, name)) => (Some(section), name),
        None => (None, key),
    };
    let name = name.replace('_', " ");
    match section {
        None => {
            let mut chars = name.chars();
            chars
                .next()
                .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        }
        Some(section) => {
            let section = match section {
                "widgets" => "Widgets",
                "accents" => "Accents",
                "overlay" => "Overlay",
                "data_kinds" => "Graph pins",
                other => other,
            };
            format!("{section}: {name}")
        }
    }
}

/// The name of the built-in AMOLED Night editor theme (an `appearance.theme` choice).
pub const AMOLED_NIGHT: &str = "amoled-night";
/// The name of the built-in Paper editor theme (an `appearance.theme` choice).
pub const PAPER: &str = "paper";

/// The editor's own built-in looks, in menu order, with the names a person sees: the `default`
/// choice is Piercing Graphite. (The bridged UI themes `regia-dark` and `regia-light` are
/// `appearance.theme` choices too; they come from `regia_ui`.)
pub const BUILTIN_EDITOR_THEMES: [(&str, &str); 4] = [
    ("default", "Piercing Graphite"),
    (AMOLED_NIGHT, "AMOLED Night"),
    (PAPER, "Paper"),
    (HIGH_CONTRAST, "High contrast"),
];

/// The name a person sees for an `appearance.theme` choice (`default` is "Piercing Graphite").
#[must_use]
pub fn theme_display_name(choice: &str) -> String {
    BUILTIN_EDITOR_THEMES
        .iter()
        .find(|(name, _)| *name == choice)
        .map_or_else(|| choice.to_string(), |(_, label)| (*label).to_string())
}

/// The tokens of an `appearance.theme` choice: `default` (Piercing Graphite), `amoled-night`,
/// `paper`, `high-contrast` ([`ThemeTokens::high_contrast`]) or a built-in UI theme (`regia-dark`,
/// `regia-light`, with or without `ui:`), and the [`ThemeSource`] it reports.
///
/// # Errors
/// An unknown name.
pub fn builtin_editor_theme(name: &str) -> Result<(ThemeTokens, ThemeSource), String> {
    match name {
        "default" => Ok((ThemeTokens::regia_default(), ThemeSource::Default)),
        HIGH_CONTRAST => Ok((
            ThemeTokens::high_contrast(),
            ThemeSource::Ui(HIGH_CONTRAST.to_string()),
        )),
        AMOLED_NIGHT => Ok((
            ThemeTokens::amoled_night(),
            ThemeSource::Ui(AMOLED_NIGHT.to_string()),
        )),
        PAPER => Ok((ThemeTokens::paper(), ThemeSource::Ui(PAPER.to_string()))),
        _ => {
            let (roles, source) = resolve_ui_theme(&Value::String(name.to_string()))?;
            Ok((ThemeTokens::from_ui_roles(&roles)?, source))
        }
    }
}

/// User overrides of the diagnostic colors (Preferences ▸ Appearance; `None` keeps the theme's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiagnosticColors {
    /// Errors: the error text, status-line errors and ERROR log lines.
    pub error: Option<Color32>,
    /// Warnings: warning text (and the content browser's warning badges), the unsaved-changes
    /// label and WARN log lines.
    pub warning: Option<Color32>,
    /// INFO log lines.
    pub info: Option<Color32>,
    /// Passing or valid items.
    pub success: Option<Color32>,
}

impl DiagnosticColors {
    /// Writes the overrides into `tokens`: every token of the same meaning gets the color, so the
    /// log, the Problems panel, status lines and badges agree.
    pub fn apply_to(&self, tokens: &mut ThemeTokens) {
        if let Some(c) = self.error {
            tokens.error = c;
            tokens.accents.error_text = c;
            tokens.accents.log_error = c;
        }
        if let Some(c) = self.warning {
            tokens.warn = c;
            tokens.accents.unsaved = c;
            tokens.accents.log_warn = c;
        }
        if let Some(c) = self.info {
            tokens.accents.log_info = c;
        }
        if let Some(c) = self.success {
            tokens.accents.ok = c;
        }
    }

    /// Puts every token [`Self::apply_to`] may write back to its value in `base` (the theme before
    /// any override), so a cleared override restores the theme's color.
    pub fn restore_slots(base: &ThemeTokens, tokens: &mut ThemeTokens) {
        tokens.error = base.error;
        tokens.accents.error_text = base.accents.error_text;
        tokens.accents.log_error = base.accents.log_error;
        tokens.warn = base.warn;
        tokens.accents.unsaved = base.accents.unsaved;
        tokens.accents.log_warn = base.accents.log_warn;
        tokens.accents.log_info = base.accents.log_info;
        tokens.accents.ok = base.accents.ok;
    }
}

/// The severity of a diagnostic, for [`diagnostic_color`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticLevel {
    /// An error.
    Error,
    /// A warning.
    Warning,
    /// Information.
    Info,
    /// A success or a passing check.
    Success,
}

/// The theme color of a diagnostic level (the one the Preferences diagnostic colors override).
#[must_use]
pub fn diagnostic_color(tokens: &ThemeTokens, level: DiagnosticLevel) -> Color32 {
    match level {
        DiagnosticLevel::Error => tokens.accents.log_error,
        DiagnosticLevel::Warning => tokens.accents.log_warn,
        DiagnosticLevel::Info => tokens.accents.log_info,
        DiagnosticLevel::Success => tokens.accents.ok,
    }
}

/// Current version of a saved editor theme file (`themes/<name>.editor_theme.ron`).
pub const EDITOR_THEME_FORMAT_VERSION: u32 = 1;
/// The directory of saved themes inside the user config directory.
pub const THEMES_DIR: &str = "themes";
/// The file name suffix of a saved editor theme.
pub const THEME_FILE_SUFFIX: &str = ".editor_theme.ron";
/// Longest theme name, bytes.
pub const MAX_THEME_NAME: usize = 64;
/// Most bytes a theme file may have (a theme is a few KiB).
const MAX_THEME_BYTES: u64 = 1 << 20;

/// Version 1 of a theme file: a name and the tokens in the `editor.get_theme` JSON shape.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct ThemeFileV1 {
    #[serde(default = "crate::editor_prefs::version_missing")]
    format_version: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    tokens: Value,
}

/// Why a theme file could not be named, read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeFileError {
    /// The name is empty, too long, or not letters, digits, spaces, `-` and `_`.
    BadName(String),
    /// The text is not a theme file, or its tokens do not parse.
    Parse(String),
    /// The file was written by a newer editor.
    Newer {
        /// The file's version.
        found: u32,
    },
    /// No saved theme has the name.
    NotFound(String),
    /// Reading or writing failed, or no user config directory is set.
    Io(String),
}

impl ThemeFileError {
    /// The agent error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadName(_) => "bad_theme_name",
            Self::Parse(_) => "theme_parse",
            Self::Newer { .. } => "theme_newer",
            Self::NotFound(_) => "theme_not_found",
            Self::Io(_) => "theme_io",
        }
    }
}

impl std::fmt::Display for ThemeFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName(name) => write!(
                f,
                "theme name {name:?} must be 1 to {MAX_THEME_NAME} letters, digits, spaces, `-` or `_`"
            ),
            Self::Parse(m) | Self::Io(m) => f.write_str(m),
            Self::Newer { found } => write!(
                f,
                "written by a newer editor (format_version {found}, this editor reads up to {EDITOR_THEME_FORMAT_VERSION})"
            ),
            Self::NotFound(name) => write!(f, "no saved theme is called {name:?}"),
        }
    }
}

impl std::error::Error for ThemeFileError {}

/// Whether `name` can name a saved theme (it becomes the file name).
#[must_use]
pub fn valid_theme_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= MAX_THEME_NAME
        && name.trim() == name
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'))
}

/// The `from` versions of the registered theme file migration steps (none: version 1 is the
/// first).
#[must_use]
pub fn theme_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape of a saved theme file (ADR-7902: the `editor-theme` format). `tokens` is a JSON
/// map in the `editor.get_theme` shape, so its keys are not part of the shape.
#[must_use]
pub fn theme_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<ThemeFileV1>()])
}

/// A theme file's text for `tokens` under `name` (current version).
#[must_use]
pub fn theme_file_to_text(name: &str, tokens: &ThemeTokens) -> String {
    let mut json = tokens.to_json();
    drop_unrepresentable_defaults(tokens, &mut json);
    let file = ThemeFileV1 {
        format_version: EDITOR_THEME_FORMAT_VERSION,
        name: name.to_string(),
        tokens: json,
    };
    ron::ser::to_string_pretty(&file, regia_core::ron_pretty()).unwrap_or_else(|_| {
        format!("(format_version: {EDITOR_THEME_FORMAT_VERSION}, name: {name:?}, tokens: {{}})")
    })
}

/// Leaves out of a theme file's `tokens` every color that still holds its file default
/// ([`ThemeTokens::regia_classic`], what an absent key of a version 1 file means) and that
/// `#rrggbb[aa]` cannot carry exactly (the classic `shade` is an additive color, alpha 0 with
/// nonzero light; the classic `muted` is gamma-multiplied). An absent key reads back as that
/// default, so the file round-trips; writing the hex would turn the additive shade into
/// transparent black on the second save.
fn drop_unrepresentable_defaults(tokens: &ThemeTokens, json: &mut Value) {
    let mut ours = tokens.clone();
    let mut base = ThemeTokens::regia_classic();
    for ((key, value), (_, default)) in ours.color_slots_mut().into_iter().zip(base.color_slots_mut()) {
        let exact = parse_color(&Value::String(hex(*value))).is_ok_and(|back| back == *value);
        if exact || *value != *default {
            continue;
        }
        match key.split_once('.') {
            Some((group, leaf)) => {
                if let Some(object) = json.get_mut(group).and_then(Value::as_object_mut) {
                    object.remove(leaf);
                }
            }
            None => {
                if let Some(object) = json.as_object_mut() {
                    object.remove(key.as_str());
                }
            }
        }
    }
}

/// Reads a theme file's text: its name and tokens. Absent keys keep the values they had when
/// version 1 was defined, [`ThemeTokens::regia_classic`] (ADR-8585: the default look became
/// Piercing Graphite, but a theme saved before that, or one without the newer keys `lit_edge` and
/// `widgets.edge`, must look as it did).
///
/// # Errors
/// [`ThemeFileError::Parse`] or [`ThemeFileError::Newer`].
pub fn theme_file_from_text(text: &str) -> Result<(String, ThemeTokens), ThemeFileError> {
    use crate::editor_prefs::{VersionedError, read_versioned};
    let file = read_versioned(text, EDITOR_THEME_FORMAT_VERSION, |f: &ThemeFileV1| {
        f.format_version
    })
    .map_err(|e| match e {
        VersionedError::Parse(m) => ThemeFileError::Parse(format!("theme file: {m}")),
        VersionedError::Newer(found) => ThemeFileError::Newer { found },
    })?;
    let mut tokens = ThemeTokens::regia_classic();
    match &file.tokens {
        Value::Object(object) => tokens.merge(object).map_err(ThemeFileError::Parse)?,
        Value::Null => {}
        _ => {
            return Err(ThemeFileError::Parse(
                "theme file: `tokens` must be a map".to_string(),
            ));
        }
    }
    Ok((file.name, tokens))
}

/// The file of the saved theme `name` under the user config directory `config_dir`.
#[must_use]
pub fn theme_file_path(config_dir: &std::path::Path, name: &str) -> PathBuf {
    config_dir
        .join(THEMES_DIR)
        .join(format!("{name}{THEME_FILE_SUFFIX}"))
}

/// Writes `tokens` as the saved theme `name` (atomically); returns the file.
///
/// # Errors
/// [`ThemeFileError::BadName`] or [`ThemeFileError::Io`].
pub fn save_custom_theme(
    config_dir: &std::path::Path,
    name: &str,
    tokens: &ThemeTokens,
) -> Result<PathBuf, ThemeFileError> {
    if !valid_theme_name(name) {
        return Err(ThemeFileError::BadName(name.to_string()));
    }
    let path = theme_file_path(config_dir, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ThemeFileError::Io(format!("cannot create {}: {e}", parent.display())))?;
    }
    crate::level_io::write_atomic(&path, theme_file_to_text(name, tokens).as_bytes())
        .map_err(ThemeFileError::Io)?;
    Ok(path)
}

/// Reads a theme file at `path` (at most 1 MiB).
///
/// # Errors
/// [`ThemeFileError::Io`] (including a missing file), [`ThemeFileError::Parse`] or
/// [`ThemeFileError::Newer`].
pub fn read_theme_file(path: &std::path::Path) -> Result<(String, ThemeTokens), ThemeFileError> {
    let bytes = regia_agent::req_path::read_file_capped(path, MAX_THEME_BYTES)
        .map_err(|e| ThemeFileError::Io(format!("cannot read {}: {e}", path.display())))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ThemeFileError::Parse(format!("{} is not UTF-8 text", path.display())))?;
    theme_file_from_text(&text)
}

/// Reads the saved theme `name`.
///
/// # Errors
/// [`ThemeFileError::BadName`], [`ThemeFileError::NotFound`] or what [`read_theme_file`] reports.
pub fn load_custom_theme(config_dir: &std::path::Path, name: &str) -> Result<ThemeTokens, ThemeFileError> {
    if !valid_theme_name(name) {
        return Err(ThemeFileError::BadName(name.to_string()));
    }
    let path = theme_file_path(config_dir, name);
    if !path.is_file() {
        return Err(ThemeFileError::NotFound(name.to_string()));
    }
    read_theme_file(&path).map(|(_, tokens)| tokens)
}

/// The names of the saved themes in `config_dir`, sorted.
#[must_use]
pub fn list_custom_themes(config_dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(config_dir.join(THEMES_DIR)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(THEME_FILE_SUFFIX))
                .filter(|n| valid_theme_name(n))
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names
}

/// The user config directory of the editor files, when the host set one.
fn config_dir(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::editor_prefs::EditorConfigPaths>()
        .and_then(|p| p.dir.clone())
}

fn theme_error(error: &ThemeFileError) -> RpcError {
    use regia_agent::protocol::INVALID_PARAMS;
    let hint = match error {
        ThemeFileError::BadName(_) => "letters, digits, spaces, - and _, at most 64 bytes",
        ThemeFileError::NotFound(_) => "editor.theme_save first, or editor.theme_import a file",
        ThemeFileError::Newer { .. } => "open it with the newer editor",
        ThemeFileError::Parse(_) => "a *.editor_theme.ron file: (format_version: 1, name, tokens: { .. })",
        ThemeFileError::Io(_) => "",
    };
    RpcError::new(INVALID_PARAMS, error.code(), error.to_string(), hint)
}

fn no_config_dir() -> RpcError {
    theme_error(&ThemeFileError::Io(
        "no user config directory: saved themes are off for this session".to_string(),
    ))
}

/// Makes the saved theme `name` the active one through the `appearance.custom_theme`
/// preference (so it persists and applies on the next frame), and applies it now.
fn select_custom_theme(world: &mut World, name: &str) -> Result<(), RpcError> {
    use crate::editor_prefs::{EditorPreferences, PrefValue};
    world
        .get_resource_or_insert_with(EditorPreferences::default)
        .set(
            crate::editor_prefs::CUSTOM_THEME_PREF,
            &PrefValue::Text(name.to_string()),
        )
        .map_err(|e| RpcError::new(regia_agent::protocol::INVALID_PARAMS, e.code(), e.to_string(), ""))?;
    crate::editor_prefs::apply_appearance(world);
    Ok(())
}

fn name_param(params: &Value) -> Result<Option<String>, RpcError> {
    match params.get("name") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => Ok(Some(name.clone())),
        Some(_) => Err(theme_error(&ThemeFileError::BadName(
            "(not a string)".to_string(),
        ))),
    }
}

/// `editor.theme_save { name }`.
fn editor_theme_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = name_param(params)?.unwrap_or_default();
    let dir = config_dir(world).ok_or_else(no_config_dir)?;
    let tokens = tokens(world).clone();
    let path = save_custom_theme(&dir, &name, &tokens).map_err(|e| theme_error(&e))?;
    select_custom_theme(world, &name)?;
    Ok(json!({
        "name": name,
        "path": path.display().to_string(),
        "themes": list_custom_themes(&dir),
    }))
}

/// `editor.theme_import { path | text, name? }`.
fn editor_theme_import(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (file_name, tokens, stem) = match params.get("text").and_then(Value::as_str) {
        Some(text) => {
            let (name, tokens) = theme_file_from_text(text).map_err(|e| theme_error(&e))?;
            (name, tokens, None)
        }
        None => {
            let path = crate::baseline_agent::agent_path(world, params)?;
            let (name, tokens) = read_theme_file(&path).map_err(|e| theme_error(&e))?;
            let stem = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.strip_suffix(THEME_FILE_SUFFIX).unwrap_or(n).to_string());
            (name, tokens, stem)
        }
    };
    let name = name_param(params)?
        .or_else(|| Some(file_name).filter(|n| !n.is_empty()))
        .or(stem)
        .unwrap_or_default();
    if !valid_theme_name(&name) {
        return Err(theme_error(&ThemeFileError::BadName(name)));
    }
    let dir = config_dir(world).ok_or_else(no_config_dir)?;
    let path = save_custom_theme(&dir, &name, &tokens).map_err(|e| theme_error(&e))?;
    select_custom_theme(world, &name)?;
    Ok(json!({
        "name": name,
        "path": path.display().to_string(),
        "themes": list_custom_themes(&dir),
    }))
}

/// `editor.theme_export { path, name? }`.
fn editor_theme_export(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = crate::baseline_agent::agent_path(world, params)?;
    if !path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(THEME_FILE_SUFFIX))
    {
        return Err(RpcError::new(
            regia_agent::protocol::INVALID_PARAMS,
            "bad_path",
            format!(
                "`path` must name a `*{THEME_FILE_SUFFIX}` file: {}",
                path.display()
            ),
            "for example my.editor_theme.ron",
        ));
    }
    let (name, tokens) = match name_param(params)? {
        Some(name) => {
            let dir = config_dir(world).ok_or_else(no_config_dir)?;
            let tokens = load_custom_theme(&dir, &name).map_err(|e| theme_error(&e))?;
            (name, tokens)
        }
        None => {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(THEME_FILE_SUFFIX))
                .unwrap_or("exported")
                .to_string();
            (name, tokens(world).clone())
        }
    };
    crate::level_io::write_atomic(&path, theme_file_to_text(&name, &tokens).as_bytes())
        .map_err(|e| theme_error(&ThemeFileError::Io(e)))?;
    Ok(json!({ "name": name, "path": path.display().to_string() }))
}

/// `editor.theme_list`: the saved themes and the active choice.
fn editor_theme_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let prefs = world
        .get_resource::<crate::editor_prefs::EditorPreferences>()
        .cloned()
        .unwrap_or_default();
    let dir = config_dir(world);
    Ok(json!({
        "builtin": crate::editor_prefs::THEME_CHOICES,
        "theme": prefs.text("appearance.theme"),
        "custom_theme": prefs.text(crate::editor_prefs::CUSTOM_THEME_PREF),
        "themes": dir.as_deref().map(list_custom_themes).unwrap_or_default(),
        "dir": dir.map(|d| d.join(THEMES_DIR).display().to_string()),
        "format_version": EDITOR_THEME_FORMAT_VERSION,
    }))
}

/// The saved-theme methods: `editor.theme_save`, `editor.theme_import`, `editor.theme_export`,
/// `editor.theme_list` (`book/agents/theme.md`).
fn theme_file_methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.theme_save",
                tool: "theme_save",
                description: "Save the editor's current theme tokens as a custom theme in the user config directory \
                    (themes/<name>.editor_theme.ron) and make it the active theme (appearance.custom_theme), so it \
                    persists across sessions. Errors: bad_theme_name, theme_io.",
                params: &[ParamSchema {
                    name: "name",
                    kind: "string",
                    required: true,
                    description: "1 to 64 letters, digits, spaces, `-` or `_`.",
                }],
                result: "{ name, path, themes: [name] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_theme_save as Handler,
        ),
        (
            MethodSchema {
                name: "editor.theme_import",
                tool: "theme_import",
                description: "Import an editor theme file (*.editor_theme.ron) into the saved themes and make it active. \
                    Errors: bad_path, path_outside_project, theme_parse, theme_newer, bad_theme_name, theme_io.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "The file: relative to the project, or absolute under the project or the temp directory. Required unless text is given.",
                    },
                    ParamSchema {
                        name: "text",
                        kind: "string",
                        required: false,
                        description: "The file's text instead of a path.",
                    },
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: false,
                        description: "The name to save it under; default: the file's name field, then the file name.",
                    },
                ],
                result: "{ name, path, themes: [name] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_theme_import as Handler,
        ),
        (
            MethodSchema {
                name: "editor.theme_export",
                tool: "theme_export",
                description: "Write the current theme (or the saved theme `name`) to a *.editor_theme.ron file another \
                    editor can import. Errors: bad_path, path_outside_project, theme_not_found, theme_io.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "The file to write, ending in .editor_theme.ron: relative to the project, or absolute under the project or the temp directory.",
                    },
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: false,
                        description: "A saved theme to export instead of the current tokens.",
                    },
                ],
                result: "{ name, path }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_theme_export as Handler,
        ),
        (
            MethodSchema {
                name: "editor.theme_list",
                tool: "theme_list",
                description: "The built-in editor themes, the saved custom themes, and which one is active.",
                params: &[],
                result: "{ builtin: [name], theme, custom_theme, themes: [name], dir, format_version }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_theme_list as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_tokens_leave_egui_dark_untouched() {
        assert_eq!(visuals(&ThemeTokens::regia_classic()), egui::Visuals::dark());
    }

    /// Every non-ASCII character inside a string literal of the editor's sources, with the first
    /// `file:line` that has it. Line comments and test code (`tests.rs`, `*_tests.rs`, and a file's
    /// lines from its `#[cfg(test)]` module on) are skipped; `\u{..}` escapes are decoded.
    fn source_literal_chars() -> std::collections::BTreeMap<char, String> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && !path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n == "tests.rs" || n.ends_with("_tests.rs"))
                {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&root, &mut files);
        files.sort();
        assert!(
            files.len() > 100,
            "the editor sources are readable: {}",
            root.display()
        );
        let mut found = std::collections::BTreeMap::new();
        for file in files {
            let text = std::fs::read_to_string(&file).expect("read an editor source");
            let name = file.strip_prefix(&root).unwrap_or(&file).display().to_string();
            let lines: Vec<&str> = text.lines().collect();
            for (n, line) in lines.iter().enumerate() {
                // An inline `#[cfg(test)] mod tests {`: the rest of the file is test code.
                if line.trim() == "#[cfg(test)]"
                    && lines.get(n + 1).is_some_and(|next| {
                        let next = next.trim();
                        next.trim_start_matches("pub(crate) ").starts_with("mod ") && next.ends_with('{')
                    })
                {
                    break;
                }
                let mut chars = line.chars().peekable();
                let mut prev = ' ';
                let mut literal: Option<String> = None;
                while let Some(c) = chars.next() {
                    match &mut literal {
                        None if c == '/' && chars.peek() == Some(&'/') => break,
                        // `'"'` is a char literal, not the start of a string.
                        None if c == '"' && prev != '\'' => literal = Some(String::new()),
                        None => {}
                        Some(s) if c == '\\' => {
                            if chars.next() == Some('u') {
                                let hex: String = chars.by_ref().skip(1).take_while(|&h| h != '}').collect();
                                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                                {
                                    s.push(ch);
                                }
                            }
                        }
                        Some(s) if c == '"' => {
                            for ch in s.chars().filter(|ch| !ch.is_ascii()) {
                                found.entry(ch).or_insert_with(|| format!("{name}:{}", n + 1));
                            }
                            literal = None;
                        }
                        Some(s) => s.push(c),
                    }
                    prev = c;
                }
            }
        }
        found
    }

    /// Bug 7 of the 2026-11-16 all-host smoke run (ADR-8589): "←"/"→" (the Content browser's
    /// Back/Forward glyph buttons), "▸" (help and menu-path strings) and "●" (the changed-setting
    /// marker before "Theme" in Preferences) drew as empty boxes because no font of the table egui
    /// gets has them. Every character any string literal of the editor holds must have a glyph in
    /// the proportional and the monospace family of the context [`apply`] themed with the default
    /// tokens (the table the editor actually draws with).
    ///
    /// "Has a glyph" means: egui draws the character with a glyph of its own, not with the
    /// replacement box. epaint 0.36's `Fonts::has_glyph` alone cannot say that: it answers `false`
    /// for every character whose owning face is the family's *replacement face* (the first face in
    /// the chain that has `◻`), although egui draws those characters from that face correctly
    /// (ADR-8679). In the editor's chains that face is NotoEmoji (Proportional: the emoji icons) or
    /// Hack (Monospace: `↺` `▣` `◐` `⬚`), and no other face of the table has `◻`, so no order of the
    /// chain makes `has_glyph` exact. A character `has_glyph` refuses therefore counts as drawn only
    /// when its laid-out glyph uses a different atlas cell than the replacement box itself; the
    /// controls prove the check sees a real box (a hieroglyph no face has) and real glyphs.
    #[test]
    fn every_character_the_editor_writes_has_a_glyph() {
        let ctx = egui::Context::default();
        apply(&ctx, &ThemeTokens::regia_default());
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        let chars = source_literal_chars();
        for named in ['←', '→', '▸', '●'] {
            assert!(chars.contains_key(&named), "the walk sees {named:?}");
        }
        /// The atlas cell (`uv_rect` min and max) the single character `ch` draws with, if it lays
        /// out as a glyph at all.
        fn cell(
            fonts: &mut egui::epaint::text::FontsView<'_>,
            font: &egui::FontId,
            ch: char,
        ) -> Option<[u16; 4]> {
            let galley = fonts.layout_no_wrap(ch.to_string(), font.clone(), egui::Color32::WHITE);
            let glyph = galley.rows.first().and_then(|row| row.row.glyphs.first())?;
            let uv = glyph.uv_rect;
            Some([uv.min[0], uv.min[1], uv.max[0], uv.max[1]])
        }
        /// Whether `ch` draws with a glyph of its own in `font`'s family.
        fn drawn(fonts: &mut egui::epaint::text::FontsView<'_>, font: &egui::FontId, ch: char) -> bool {
            if fonts.has_glyph(font, ch) {
                return true;
            }
            let replacement = cell(fonts, font, '◻');
            cell(fonts, font, ch).is_some_and(|own| Some(own) != replacement)
        }
        let mut missing = Vec::new();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.ctx().fonts_mut(|fonts| {
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    let font = egui::FontId::new(14.0, family.clone());
                    // Controls: a character no face of the table has draws as the box; letters,
                    // a brand-face symbol and an emoji-face icon draw as themselves.
                    assert!(
                        !drawn(fonts, &font, '\u{13000}'),
                        "{family:?}: the check must see the replacement box"
                    );
                    for ch in ['A', '←', '🔥'] {
                        assert!(drawn(fonts, &font, ch), "{family:?}: control {ch:?} draws");
                    }
                    for (ch, at) in &chars {
                        if !drawn(fonts, &font, *ch) {
                            missing.push(format!("{ch:?} U+{:04X} ({family:?}; {at})", u32::from(*ch)));
                        }
                    }
                }
            });
        });
        out.textures_delta.clear();
        assert!(missing.is_empty(), "no glyph for:\n{}", missing.join("\n"));
    }

    #[test]
    fn default_overlay_tokens_are_the_old_constants() {
        let o = OverlayTokens::regia_default();
        assert_eq!(o.selection_outline, crate::overlays::SELECTION_OUTLINE);
        assert_eq!(o.bounds, crate::overlays::BOUNDS);
        assert_eq!(o.icon_light, crate::overlays::ICON_LIGHT);
        assert_eq!(o.icon_camera, crate::overlays::ICON_CAMERA);
        assert_eq!(o.icon_decal, crate::overlays::ICON_DECAL);
        assert_eq!(o.icon_audio, crate::overlays::ICON_AUDIO);
        assert_eq!(o.grid_major, egui::Color32::from_rgb(120, 120, 130));
        assert_eq!(o.grid_minor, egui::Color32::from_rgb(60, 60, 68));
        assert_eq!(o.gizmo_x, egui::Color32::from_rgb(220, 40, 40));
        assert_eq!(o.gizmo_y, egui::Color32::from_rgb(40, 200, 60));
        assert_eq!(o.gizmo_z, egui::Color32::from_rgb(50, 90, 230));
        assert_eq!(o.gizmo_ring, egui::Color32::from_rgb(230, 190, 60));
        assert_eq!(o.audio_min, egui::Color32::from_rgb(80, 200, 230));
        assert_eq!(o.audio_max, egui::Color32::from_rgb(40, 110, 160));
        assert_eq!(o.audio_cone, egui::Color32::from_rgb(240, 140, 220));
        assert_eq!(o.reverb_zone, egui::Color32::from_rgb(170, 120, 240));
    }

    #[test]
    fn default_accents_are_the_old_inline_colors() {
        let a = AccentTokens::regia_default();
        assert_eq!(a.active, egui::Color32::from_rgb(255, 150, 40));
        assert_eq!(a.select, egui::Color32::from_rgb(110, 170, 255));
        assert_eq!(a.alert, egui::Color32::from_rgb(255, 90, 90));
        assert_eq!(a.ok, egui::Color32::from_rgb(120, 200, 120));
        assert_eq!(a.unsaved, egui::Color32::YELLOW);
        assert_eq!(a.error_text, egui::Color32::RED);
        assert_eq!(a.log_error, egui::Color32::LIGHT_RED);
        assert_eq!(a.log_warn, egui::Color32::YELLOW);
        assert_eq!(a.log_info, egui::Color32::LIGHT_GREEN);
    }

    #[test]
    fn classic_dock_style_matches_the_old_derivation() {
        let ctx = egui::Context::default();
        let tokens = ThemeTokens::regia_classic();
        apply(&ctx, &tokens);
        let themed = ctx.global_style();
        let old = egui::Style {
            visuals: egui::Visuals::dark(),
            ..(*themed).clone()
        };
        assert_eq!(
            format!("{:?}", dock_style(&themed)),
            format!("{:?}", egui_dock::Style::from_egui(&old))
        );
    }

    #[test]
    fn json_overrides_only_the_keys_it_names() {
        let tokens = ThemeTokens::from_json(
            r##"{ "colors": { "ink": "#000000", "signal": "#ffffff", "on_signal": "#000000" },
                  "flat": true, "overlay": { "selection_outline": "#ffffff" },
                  "fonts": { "monospace": { "name": "Mono", "file": "/nope.ttf" } } }"##,
        )
        .expect("valid theme");
        let default = ThemeTokens::regia_default();
        assert_eq!(tokens.ground, Color32::BLACK);
        assert_eq!(tokens.signal, Color32::WHITE);
        assert!(tokens.flat);
        assert_eq!(tokens.overlay.selection_outline, Color32::WHITE);
        assert_eq!(tokens.overlay.grid_major, default.overlay.grid_major);
        assert_eq!(tokens.panel, default.panel);
        assert_eq!(
            tokens.fonts.monospace.as_ref().map(|f| f.name.as_str()),
            Some("Mono")
        );
        let v = visuals(&tokens);
        assert_eq!(v.selection.bg_fill, Color32::WHITE);
        // Flat: no drop shadow. The default look's lit edge is a 1 px band above the top edge
        // (no blur, no spread, no downward offset); without a lit edge there is no shadow at all.
        let s = v.window_shadow;
        assert_eq!((s.blur, s.spread, s.offset), (0, 0, [0, -1]), "{s:?}");
        assert_eq!(s.color, ThemeTokens::regia_default().lit_edge);
        let no_edge =
            ThemeTokens::from_json(r##"{ "flat": true, "lit_edge": "#00000000" }"##).expect("valid theme");
        assert_eq!(visuals(&no_edge).window_shadow, egui::epaint::Shadow::NONE);
        assert_eq!(visuals(&no_edge).popup_shadow, egui::epaint::Shadow::NONE);
    }

    #[test]
    fn bad_json_is_an_error_not_a_panic() {
        assert!(ThemeTokens::from_json("[]").is_err());
        assert!(ThemeTokens::from_json("{").is_err());
        assert!(ThemeTokens::from_json(r##"{ "signal": "#12" }"##).is_err());
        assert!(ThemeTokens::from_json(r#"{ "signal": 5 }"#).is_err());
        assert!(ThemeTokens::from_json(r##"{ "signal": "#éé0000" }"##).is_err());
    }

    #[test]
    fn to_json_round_trips() {
        let mut tokens = ThemeTokens::regia_classic();
        tokens.signal = Color32::from_rgb(1, 2, 3);
        tokens.overlay.bounds = Color32::from_rgba_unmultiplied(9, 8, 7, 200);
        // The default additive shade and gamma-multiplied muted are not exactly representable.
        tokens.shade = Color32::from_gray(30);
        tokens.muted = Color32::from_gray(100);
        let back = ThemeTokens::from_json(&tokens.to_json().to_string()).expect("round trip");
        assert_eq!(back.signal, tokens.signal);
        assert_eq!(back.overlay.bounds, tokens.overlay.bounds);
        let mut expected = tokens.clone();
        expected.fonts = FontSet::default();
        assert_eq!(back, expected);
    }

    #[test]
    fn missing_font_files_fall_back_to_the_default_table() {
        let fonts = FontSet {
            proportional: Some(FontToken {
                name: "Missing".into(),
                file: Some("/definitely/not/here.ttf".into()),
            }),
            monospace: None,
            display: None,
            prose: None,
        };
        let defs = font_definitions(&fonts);
        // The editor's table without theme fonts (egui's defaults plus the fallbacks, ADR-8589).
        let stock = font_definitions(&FontSet::default());
        assert_eq!(defs.font_data.len(), stock.font_data.len());
        assert!(
            defs.families
                .contains_key(&egui::FontFamily::Name(DISPLAY_FAMILY.into()))
        );
    }

    #[test]
    fn agent_methods_get_set_and_reset() {
        let mut world = World::new();
        let got = editor_get_theme(&mut world, &json!({})).expect("get");
        assert_eq!(got["is_default"], json!(true));
        let set = editor_set_theme(
            &mut world,
            &json!({ "tokens": { "signal": "#ffffff", "overlay": { "grid_major": "#102030" } } }),
        )
        .expect("set");
        assert_eq!(set["is_default"], json!(false));
        assert_eq!(set["tokens"]["signal"], json!("#ffffff"));
        assert_eq!(
            tokens(&world).overlay.grid_major,
            Color32::from_rgb(0x10, 0x20, 0x30)
        );
        // A second call keeps the earlier change.
        editor_set_theme(&mut world, &json!({ "tokens": { "flat": true } })).expect("set");
        assert_eq!(tokens(&world).signal, Color32::WHITE);
        assert!(tokens(&world).flat);
        assert!(editor_set_theme(&mut world, &json!({ "tokens": { "signal": "red" } })).is_err());
        assert!(editor_set_theme(&mut world, &json!({ "tokens": 5 })).is_err());
        // A bad known key, including one beside a good color, changes nothing.
        let before = tokens(&world).clone();
        assert!(
            editor_set_theme(
                &mut world,
                &json!({ "tokens": { "colors": { "ground": "#010203" }, "flat": "yes" } }),
            )
            .is_err()
        );
        assert!(
            editor_set_theme(
                &mut world,
                &json!({ "tokens": { "overlay": 1, "text": "#040506" } })
            )
            .is_err()
        );
        assert_eq!(*tokens(&world), before);
        // A color inside `colors` applies, and the same key at the top level wins.
        editor_set_theme(
            &mut world,
            &json!({ "tokens": { "colors": { "ground": "#010203", "signal": "#000000" }, "signal": "#ffffff", "text": "#040506" } }),
        )
        .expect("colors");
        assert_eq!(tokens(&world).ground, Color32::from_rgb(0x01, 0x02, 0x03));
        assert_eq!(tokens(&world).signal, Color32::WHITE);
        assert_eq!(tokens(&world).text, Color32::from_rgb(0x04, 0x05, 0x06));
        assert!(tokens(&world).flat);
        let reset = editor_set_theme(&mut world, &json!({ "reset": true })).expect("reset");
        assert_eq!(reset["is_default"], json!(true));
    }

    #[test]
    fn world_resource_wins_and_the_default_is_the_fallback() {
        let mut world = World::new();
        assert_eq!(tokens(&world), &ThemeTokens::regia_default());
        let mut custom = ThemeTokens::regia_default();
        custom.overlay.grid_major = Color32::WHITE;
        world.insert_resource(custom);
        assert_eq!(tokens(&world).overlay.grid_major, Color32::WHITE);
    }

    #[test]
    fn sync_applies_changed_tokens_to_the_context() {
        let ctx = egui::Context::default();
        let mut world = World::new();
        sync(&ctx, &world);
        assert_eq!(*ui_tokens(&ctx), ThemeTokens::regia_default());
        let mut custom = ThemeTokens::regia_default();
        custom.signal = Color32::WHITE;
        world.insert_resource(custom.clone());
        sync(&ctx, &world);
        assert_eq!(*ui_tokens(&ctx), custom);
        assert_eq!(ctx.global_style().visuals.selection.bg_fill, Color32::WHITE);
    }

    #[test]
    fn ui_theme_is_opt_in_and_reset_returns_to_the_default_look() {
        let mut world = World::new();
        let got = editor_get_theme(&mut world, &json!({})).expect("get");
        assert_eq!(got["theme_source"], json!("default"));
        assert_eq!(got["is_default"], json!(true));
        let set = editor_set_theme(&mut world, &json!({ "ui_theme": "regia-dark" })).expect("ui");
        assert_eq!(set["theme_source"], json!("ui:regia-dark"));
        assert_eq!(set["is_default"], json!(false));
        assert_eq!(set["tokens"]["signal"], json!("#3d9df0"));
        // Tokens named in the same call apply on top of the UI theme.
        let both = editor_set_theme(
            &mut world,
            &json!({ "ui_theme": "regia-light", "tokens": { "flat": true } }),
        )
        .expect("both");
        assert_eq!(both["tokens"]["panel"], json!("#f6f8fa"));
        assert_eq!(both["tokens"]["flat"], json!(true));
        // A bad theme changes nothing.
        assert!(editor_set_theme(&mut world, &json!({ "ui_theme": "nope" })).is_err());
        assert_eq!(tokens(&world).panel, Color32::from_rgb(0xf6, 0xf8, 0xfa));
        let back = editor_set_theme(&mut world, &json!({ "ui_theme": "default" })).expect("default");
        assert_eq!(back["theme_source"], json!("default"));
        assert_eq!(back["is_default"], json!(true));
        editor_set_theme(&mut world, &json!({ "ui_theme": "regia-dark" })).expect("ui");
        let reset = editor_set_theme(&mut world, &json!({ "reset": true })).expect("reset");
        assert_eq!(reset["theme_source"], json!("default"));
        assert_eq!(reset["is_default"], json!(true));
    }

    #[test]
    fn ui_theme_restyles_the_egui_context() {
        let ctx = egui::Context::default();
        let mut world = World::new();
        editor_set_theme(&mut world, &json!({ "ui_theme": "regia-dark" })).expect("ui");
        sync(&ctx, &world);
        let visuals = ctx.global_style().visuals.clone();
        assert_eq!(visuals.panel_fill, Color32::from_rgb(0x17, 0x1c, 0x23));
        assert_eq!(visuals.selection.bg_fill, Color32::from_rgb(0x3d, 0x9d, 0xf0));
    }

    #[test]
    fn null_ui_theme_returns_to_the_default_look() {
        let mut world = World::new();
        editor_set_theme(&mut world, &json!({ "ui_theme": "regia-light" })).expect("ui");
        let back = editor_set_theme(&mut world, &json!({ "ui_theme": null })).expect("null");
        assert_eq!(back["theme_source"], json!("default"));
        assert_eq!(back["is_default"], json!(true));
        // An absent ui_theme leaves the current skin alone.
        editor_set_theme(&mut world, &json!({ "ui_theme": "regia-light" })).expect("ui");
        let kept = editor_set_theme(&mut world, &json!({})).expect("absent");
        assert_eq!(kept["theme_source"], json!("ui:regia-light"));
    }

    /// The offscreen editor paint with a UI theme applied: a panel-filled surface carries the
    /// bridged `color.surface` (skips without a GPU).
    #[test]
    fn ui_theme_paints_the_bridged_panel_color_offscreen() {
        let device = match crate::render::headless_device() {
            Ok(d) => d,
            Err(err) => {
                eprintln!("skip ui_theme_paints_the_bridged_panel_color_offscreen: {err}");
                return;
            }
        };
        let mut world = World::new();
        editor_set_theme(&mut world, &json!({ "ui_theme": "regia-dark" })).expect("ui");
        let size = [64, 64];
        let image = crate::ui_capture::render_ui_offscreen(&device, size, 1.0, 3, |ui| {
            sync(ui.ctx(), &world);
            let fill = ui.visuals().panel_fill;
            ui.painter().rect_filled(ui.max_rect(), 0.0, fill);
        })
        .expect("capture");
        let px = image.pixels[(32 * size[0] + 32) as usize];
        let want = Color32::from_rgb(0x17, 0x1c, 0x23);
        for (got, want) in [(px.r(), want.r()), (px.g(), want.g()), (px.b(), want.b())] {
            assert!(got.abs_diff(want) <= 3, "panel pixel {px:?}, want {want:?}");
        }
    }

    /// ADR-7838: the high-contrast theme's chrome uses pure black, pure white and one accent, and
    /// nothing else (overlay and graph pin colors keep their meaning-carrying defaults).
    #[test]
    fn high_contrast_chrome_is_black_white_and_one_accent() {
        let mut t = ThemeTokens::high_contrast();
        let allowed = [Color32::BLACK, Color32::WHITE, HIGH_CONTRAST_ACCENT];
        for (key, slot) in t.color_slots_mut() {
            if key.starts_with("overlay.") || key.starts_with("data_kinds.") {
                continue;
            }
            assert!(allowed.contains(&*slot), "{key} is {slot:?}");
        }
        let t = ThemeTokens::high_contrast();
        assert_eq!(t.overlay, OverlayTokens::regia_default());
        assert!(t.flat);
        // Controls at rest are panel-colored, so they get the line color as an outline.
        let v = visuals(&t);
        assert_eq!(v.widgets.inactive.bg_stroke.color, Color32::WHITE);
        assert!(v.widgets.inactive.bg_stroke.width > 0.0);
        // The classic look keeps its outline as it was.
        assert_eq!(
            visuals(&ThemeTokens::regia_classic()).widgets.inactive.bg_stroke,
            egui::Visuals::dark().widgets.inactive.bg_stroke
        );
        let (_, source) = builtin_editor_theme(HIGH_CONTRAST).expect("built in");
        assert_eq!(source.as_text(), "ui:high-contrast");
        assert!(builtin_editor_theme("neon").is_err());
    }

    #[test]
    fn color_slots_cover_every_color_key_of_the_json_shape() {
        let mut t = ThemeTokens::regia_default();
        let keys: Vec<String> = t.color_slots_mut().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys.len(), 14 + 11 + 9 + 16 + 10);
        let json = ThemeTokens::regia_default().to_json();
        for key in &keys {
            let found = match key.split_once('.') {
                Some((section, name)) => json[section][name].is_string(),
                None => json[key.as_str()].is_string(),
            };
            assert!(found, "{key} is not a color key of to_json");
        }
        assert_eq!(token_label("accents.log_warn"), "Accents: log warn");
        assert_eq!(token_label("on_signal"), "On signal");
        assert_eq!(token_label("data_kinds.plant_mesh"), "Graph pins: plant mesh");
    }

    #[test]
    fn theme_files_round_trip_refuse_newer_and_keep_unknown_keys_harmless() {
        let mut tokens = ThemeTokens::high_contrast();
        tokens.signal = Color32::from_rgb(1, 2, 3);
        let text = theme_file_to_text("Night", &tokens);
        assert!(text.contains("format_version: 1"), "{text}");
        let (name, back) = theme_file_from_text(&text).expect("round trip");
        assert_eq!(name, "Night");
        assert_eq!(back, tokens);
        // A file without a version reads as version 1; absent tokens are the defaults.
        let (_, partial) =
            theme_file_from_text("(name: \"P\", tokens: { \"signal\": \"#ffffff\", \"future_key\": 3 })")
                .expect("partial");
        assert_eq!(partial.signal, Color32::WHITE);
        // Absent keys of a version 1 file keep the classic values (ADR-8585).
        assert_eq!(partial.panel, ThemeTokens::regia_classic().panel);
        assert_eq!(
            theme_file_from_text("(format_version: 2, name: \"N\", tokens: {})"),
            Err(ThemeFileError::Newer { found: 2 })
        );
        assert_eq!(
            theme_file_from_text("(format_version: 7, brand_new_field: [1, 2])").map(|_| ()),
            Err(ThemeFileError::Newer { found: 7 }),
            "a newer shape is refused by its version"
        );
        assert_eq!(
            theme_file_from_text("(tokens: { \"signal\": \"red\" })")
                .map(|_| ())
                .map_err(|e| e.code()),
            Err("theme_parse")
        );
        assert_eq!(
            theme_file_from_text("not ron").map(|_| ()).map_err(|e| e.code()),
            Err("theme_parse")
        );
        assert!(theme_migration_sources().is_empty(), "version 1 is the first");
    }

    /// The classic theme's additive `shade` (and gamma-multiplied `muted`) cannot be written as
    /// `#rrggbb[aa]`: the file leaves them out, so they read back as the defaults and a second
    /// save writes the same text (the `editor.theme` baseline fuzz target's re-encoding check).
    #[test]
    fn a_classic_theme_file_round_trips_its_unrepresentable_colors() {
        let tokens = ThemeTokens::regia_classic();
        let text = theme_file_to_text("Default", &tokens);
        assert!(!text.contains("\"shade\""), "{text}");
        let (name, back) = theme_file_from_text(&text).expect("reads");
        assert_eq!(name, "Default");
        assert_eq!(back.shade, tokens.shade, "the additive shade survives");
        assert_eq!(back, tokens);
        assert_eq!(
            theme_file_to_text(&name, &back),
            text,
            "a second save is the same"
        );
        // A non-default shade is a plain hex color and is written.
        let mut custom = tokens.clone();
        custom.shade = Color32::from_rgb(5, 6, 7);
        let custom_text = theme_file_to_text("Custom", &custom);
        assert!(custom_text.contains("\"shade\": \"#050607\""), "{custom_text}");
        assert_eq!(
            theme_file_from_text(&custom_text).expect("reads").1.shade,
            custom.shade
        );
    }

    #[test]
    fn saved_themes_list_by_name_and_names_are_file_safe() {
        let dir = regia_test_utils::unique_temp_dir("theme-files-list");
        for good in ["Night", "high contrast 2", "a-b_c"] {
            assert!(valid_theme_name(good), "{good}");
        }
        let long = "n".repeat(65);
        for bad in ["", " lead", "trail ", "../x", "a/b", "a\\b", "x.y", long.as_str()] {
            assert!(!valid_theme_name(bad), "{bad:?}");
        }
        assert_eq!(
            save_custom_theme(&dir, "../x", &ThemeTokens::regia_default()).map_err(|e| e.code()),
            Err("bad_theme_name")
        );
        save_custom_theme(&dir, "Zed", &ThemeTokens::regia_default()).expect("save");
        save_custom_theme(&dir, "Alpha", &ThemeTokens::high_contrast()).expect("save");
        std::fs::write(dir.join(THEMES_DIR).join("notes.txt"), "x").expect("stray file");
        assert_eq!(list_custom_themes(&dir), ["Alpha", "Zed"]);
        assert_eq!(
            load_custom_theme(&dir, "Alpha").expect("load"),
            ThemeTokens::high_contrast()
        );
        assert_eq!(
            load_custom_theme(&dir, "Missing").map_err(|e| e.code()),
            Err("theme_not_found")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-8585 (owner 2026-11-18): the default editor look is Piercing Graphite with the values of
    /// BRAND-GUIDE 3.8, Signal white reserved, the inverted selection, the 3:1 control outline, the
    /// 2 px Signal focus ring, flat with radius 2 / 4 and the lit edge, and the brand fonts.
    #[test]
    fn the_default_look_is_piercing_graphite() {
        let t = ThemeTokens::regia_default();
        assert_eq!(t, ThemeTokens::piercing_graphite());
        let rgb = |v: u32| hex_rgb(v);
        assert_eq!(t.ground, rgb(0x131316));
        assert_eq!(t.panel, rgb(0x18181b));
        assert_eq!(t.raised, rgb(0x1f1f23));
        assert_eq!(t.well, rgb(0x0f0f12));
        assert_eq!(t.widgets.hover, rgb(0x26262b));
        assert_eq!(t.line, rgb(0x2b2b30));
        assert_eq!(t.widgets.edge, rgb(0x7c7c84));
        assert_eq!(t.text, rgb(0xe7e7e8));
        assert_eq!(t.muted, rgb(0xa8a8b0));
        assert_eq!(t.signal, Color32::WHITE);
        assert_eq!((t.warn, t.error), (rgb(0xfdba74), rgb(0xff6767)));
        assert_eq!((t.accents.ok, t.accents.log_info), (rgb(0x61ffca), rgb(0x7aa2ff)));
        // Pure white is Signal's alone: no text, fill or line of the chrome is white except the
        // Signal roles (signal, link, active, selected).
        let mut probe = t.clone();
        for (key, slot) in probe.color_slots_mut() {
            if key.starts_with("overlay.") || key.starts_with("data_kinds.") {
                continue;
            }
            let signal_role = matches!(
                key.as_str(),
                "signal" | "widgets.link" | "accents.active" | "accents.select"
            );
            assert!(
                signal_role || *slot != Color32::WHITE,
                "{key} uses the reserved white"
            );
        }
        assert_eq!(t.fonts, FontSet::brand());
        let v = visuals(&t);
        assert_eq!(v.panel_fill, rgb(0x18181b));
        assert_eq!(v.window_fill, rgb(0x1f1f23));
        assert_eq!(v.extreme_bg_color, rgb(0x0f0f12));
        // Selection inverts: a Signal block with ground-colored text.
        assert_eq!(v.selection.bg_fill, Color32::WHITE);
        assert_eq!(v.selection.stroke.color, rgb(0x131316));
        assert_eq!(
            v.widgets.inactive.bg_stroke,
            egui::Stroke::new(1.0, rgb(0x7c7c84))
        );
        assert_eq!(
            v.widgets.active.bg_stroke,
            egui::Stroke::new(FOCUS_RING_WIDTH, Color32::WHITE),
            "keyboard focus draws the 2 px Signal ring"
        );
        assert_eq!(v.widgets.inactive.corner_radius, egui::CornerRadius::same(2));
        assert_eq!(v.window_corner_radius, egui::CornerRadius::same(4));
        assert_eq!(v.menu_corner_radius, egui::CornerRadius::same(4));
        assert_eq!(v.window_shadow.blur, 0, "no drop shadow");
        assert_eq!(
            v.window_shadow.color,
            Color32::from_rgba_unmultiplied(255, 255, 255, 15)
        );
    }

    /// The other built-in looks: AMOLED Night on Ink, Paper light with an ink Signal; neither has
    /// a blue or green ground (BRAND-GUIDE 3.8, product surfaces), and the names resolve.
    #[test]
    fn amoled_night_and_paper_are_built_in() {
        let (amoled, source) = builtin_editor_theme(AMOLED_NIGHT).expect("amoled");
        assert_eq!(source.as_text(), "ui:amoled-night");
        assert_eq!(amoled.ground, Color32::BLACK);
        assert_eq!(amoled.signal, Color32::WHITE);
        let (paper, _) = builtin_editor_theme(PAPER).expect("paper");
        assert_eq!(paper.ground, hex_rgb(0xf3eee2));
        assert_eq!(
            (paper.signal, paper.on_signal),
            (Color32::BLACK, hex_rgb(0xf3eee2))
        );
        assert_eq!(
            paper.lit_edge,
            Color32::from_rgba_unmultiplied(255, 255, 255, 153)
        );
        for (name, _) in BUILTIN_EDITOR_THEMES {
            let (t, _) = builtin_editor_theme(name).expect("built in");
            for surface in [t.ground, t.panel, t.raised, t.well] {
                let (r, g, b) = (surface.r(), surface.g(), surface.b());
                assert!(
                    !(b > r.saturating_add(8) || g > r.saturating_add(8)),
                    "{name}: {surface:?} is a blue or green ground"
                );
            }
            assert!(crate::editor_prefs::THEME_CHOICES.contains(&name), "{name}");
        }
        assert_eq!(theme_display_name("default"), "Piercing Graphite");
        assert_eq!(theme_display_name("regia-dark"), "regia-dark");
        for retired in ["forest-night", "ocean-drift", "mist", "burgundy"] {
            assert!(builtin_editor_theme(retired).is_err(), "{retired}");
        }
    }

    /// The kit's editor output loads as it is (`ui/dist/regia/editor/piercingxx-graphite.json`, a
    /// byte copy in tests/fixtures/brand-kit): `#rrggbbaa` colors and fonts as plain names, which
    /// resolve to the faces compiled into the editor.
    #[test]
    fn the_kit_graphite_editor_json_loads_as_is() {
        let text = include_str!("../tests/fixtures/brand-kit/piercingxx-graphite.json");
        let t = ThemeTokens::from_json(text).expect("the kit file loads");
        assert_eq!(t.ground, hex_rgb(0x131316));
        assert_eq!(t.signal, Color32::WHITE);
        assert_eq!(t.on_signal, hex_rgb(0x131316));
        assert!(t.flat);
        assert_eq!(t.fonts.proportional, Some(FontToken::named(JETBRAINS_MONO)));
        assert_eq!(t.fonts.monospace, Some(FontToken::named(JETBRAINS_MONO)));
        assert_eq!(t.fonts.display, Some(FontToken::named(SPACE_MONO)));
        let defs = font_definitions(&t.fonts);
        for key in ["theme:JetBrains Mono", "theme:Space Mono"] {
            assert!(defs.font_data.contains_key(key), "{key} is installed");
        }
    }

    /// The brand faces go first in their families and every egui fallback stays behind them, so
    /// a glyph the brand faces lack still renders; an unknown name without a file installs
    /// nothing; the faces parse and lay out text (no panic) and headings switch to the display
    /// face once the font table is live.
    #[test]
    fn brand_fonts_lead_their_families_and_keep_the_fallback_chain() {
        // The editor's table without theme fonts (egui's defaults plus the ADR-8589 fallbacks).
        let stock = font_definitions(&FontSet::default());
        let defs = font_definitions(&FontSet::brand());
        let family = |f: egui::FontFamily| defs.families.get(&f).cloned().unwrap_or_default();
        let proportional = family(egui::FontFamily::Proportional);
        assert_eq!(
            proportional.first().map(String::as_str),
            Some("theme:JetBrains Mono")
        );
        let stock_chain = stock
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        assert!(proportional.ends_with(&stock_chain), "{proportional:?}");
        let mono = family(egui::FontFamily::Monospace);
        assert_eq!(mono.first().map(String::as_str), Some("theme:JetBrains Mono"));
        let display = family(egui::FontFamily::Name(DISPLAY_FAMILY.into()));
        assert_eq!(
            display.iter().take(2).map(String::as_str).collect::<Vec<_>>(),
            ["theme:Space Mono", "theme:JetBrains Mono"]
        );
        assert!(display.ends_with(&stock_chain));
        let prose = family(egui::FontFamily::Name(PROSE_FAMILY.into()));
        assert_eq!(prose.first().map(String::as_str), Some("theme:Geist"));
        assert!(prose.ends_with(&stock_chain));
        let unknown = font_definitions(&FontSet {
            proportional: Some(FontToken::named("Not A Brand Face")),
            ..FontSet::default()
        });
        assert_eq!(unknown.font_data.len(), stock.font_data.len());
        for (name, bytes) in BRAND_FONTS {
            assert_eq!(brand_font(name), Some(bytes));
            assert!(bytes.len() > 50_000, "{name}");
        }

        let ctx = egui::Context::default();
        let mut tokens_seen = Vec::new();
        for _ in 0..3 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                apply_if_needed(ui.ctx());
                ui.heading("Regia ✓ → 日本語 مرحبا");
                ui.label("JetBrains Mono 0123456789");
                tokens_seen.push(
                    ui.style()
                        .text_styles
                        .get(&egui::TextStyle::Heading)
                        .map(|f| f.family.clone()),
                );
            });
            // No renderer here: drop the font atlas uploads (egui panics on unapplied deltas).
            out.textures_delta.clear();
        }
        assert_eq!(
            tokens_seen.last().cloned().flatten(),
            Some(egui::FontFamily::Name(DISPLAY_FAMILY.into())),
            "{tokens_seen:?}"
        );
    }

    /// ADR-8888: the theme's table over a host's keeps the host's faces and families, puts each
    /// theme face in front of the host's own chain, replaces only the display and prose families,
    /// and a second merge with other fonts replaces the first one's faces instead of stacking them.
    #[test]
    fn theme_fonts_merge_over_the_host_table() {
        let mut host = egui::FontDefinitions::default();
        let face = host
            .font_data
            .get("Ubuntu-Light")
            .cloned()
            .expect("egui ships Ubuntu Light");
        host.font_data.insert("host-face".into(), face);
        let light = egui::FontFamily::Name("light".into());
        host.families.insert(light.clone(), vec!["host-face".into()]);
        if let Some(list) = host.families.get_mut(&egui::FontFamily::Proportional) {
            list.insert(0, "host-face".into());
        }
        host.families.insert(
            egui::FontFamily::Name(DISPLAY_FAMILY.into()),
            vec!["host-face".into()],
        );
        let merged = font_definitions_over(host.clone(), &FontSet::brand());
        assert!(merged.font_data.contains_key("host-face"));
        assert_eq!(merged.families.get(&light), host.families.get(&light));
        let proportional = merged
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            proportional
                .iter()
                .take(2)
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["theme:JetBrains Mono", "host-face"]
        );
        let display = merged
            .families
            .get(&egui::FontFamily::Name(DISPLAY_FAMILY.into()))
            .cloned()
            .unwrap_or_default();
        assert_eq!(display.first().map(String::as_str), Some("theme:Space Mono"));
        assert!(display.ends_with(&proportional[1..]), "{display:?}");

        let again = font_definitions_over(merged, &FontSet::default());
        assert!(!again.font_data.keys().any(|k| k.starts_with(THEME_FONT_KEY)));
        assert!(
            !again
                .families
                .values()
                .flatten()
                .any(|k| k.starts_with(THEME_FONT_KEY))
        );
        assert_eq!(again, font_definitions_over(host, &FontSet::default()));
        assert_eq!(
            font_definitions(&FontSet::brand()),
            font_definitions_over(egui::FontDefinitions::default(), &FontSet::brand())
        );
    }

    fn apply_if_needed(ctx: &egui::Context) {
        let world = World::new();
        sync(ctx, &world);
    }

    /// A focused text field gets the 2 px Signal ring (its own focus frame is the selection's
    /// ground-colored text stroke, which does not show); nothing is painted in the classic look.
    #[test]
    fn a_focused_text_field_gets_the_signal_focus_ring() {
        let ring = |tokens: &ThemeTokens| {
            let ctx = egui::Context::default();
            let id = egui::Id::new("focus-ring-field");
            let mut text = String::from("abc");
            let mut found = false;
            for pass in 0..3 {
                let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                    apply(ui.ctx(), tokens);
                    let response = ui.add(egui::TextEdit::singleline(&mut text).id(id));
                    if pass == 0 {
                        response.request_focus();
                    }
                    paint_focus_ring(ui.ctx());
                });
                found = out.shapes.iter().any(|clipped| {
                    matches!(&clipped.shape, egui::Shape::Rect(r)
                        if r.stroke.width == FOCUS_RING_WIDTH && r.stroke.color == tokens.signal)
                });
                // No renderer here: drop the font atlas uploads (egui panics on unapplied deltas).
                out.textures_delta.clear();
            }
            found
        };
        assert!(ring(&ThemeTokens::regia_default()));
        assert!(!ring(&ThemeTokens::regia_classic()));
    }

    /// A theme file saved before the brand (no `lit_edge`, no `widgets.edge`, no `fonts.prose`)
    /// reads back as it looked: absent keys are the classic values, not the new default.
    #[test]
    fn a_pre_brand_theme_file_keeps_its_look() {
        let mut old = ThemeTokens::regia_classic();
        old.signal = Color32::from_rgb(1, 2, 3);
        let mut json = old.to_json();
        if let Some(top) = json.as_object_mut() {
            // The classic shade and muted are not exactly `#rrggbbaa`; a saved file leaves them
            // out (`drop_unrepresentable_defaults`), and so does a pre-brand file.
            for key in ["lit_edge", "shade", "muted"] {
                top.remove(key);
            }
        }
        if let Some(widgets) = json["widgets"].as_object_mut() {
            widgets.remove("edge");
        }
        if let Some(fonts) = json["fonts"].as_object_mut() {
            fonts.remove("prose");
        }
        let text = format!("(format_version: 1, name: \"Old\", tokens: {json})");
        let (_, back) = theme_file_from_text(&text).expect("reads");
        assert_eq!(back, old);
        assert_eq!(visuals(&back), egui::Visuals::dark().tap_selection(&old));
        // A brand theme round-trips through a file exactly.
        for (name, _) in BUILTIN_EDITOR_THEMES {
            let (t, _) = builtin_editor_theme(name).expect("built in");
            let text = theme_file_to_text(name, &t);
            assert_eq!(theme_file_from_text(&text).expect("reads").1, t, "{name}");
        }
    }

    trait TapSelection {
        fn tap_selection(self, t: &ThemeTokens) -> Self;
    }

    impl TapSelection for egui::Visuals {
        fn tap_selection(mut self, t: &ThemeTokens) -> Self {
            self.selection.bg_fill = t.signal;
            self
        }
    }

    #[test]
    fn diagnostic_colors_override_every_token_of_their_meaning() {
        let mut t = ThemeTokens::regia_default();
        let c = Color32::from_rgb(10, 20, 30);
        DiagnosticColors {
            error: Some(c),
            ..DiagnosticColors::default()
        }
        .apply_to(&mut t);
        assert_eq!((t.error, t.accents.error_text, t.accents.log_error), (c, c, c));
        assert_eq!(diagnostic_color(&t, DiagnosticLevel::Error), c);
        assert_eq!(t.warn, ThemeTokens::regia_default().warn, "others untouched");
        let mut u = ThemeTokens::regia_default();
        DiagnosticColors::default().apply_to(&mut u);
        assert_eq!(u, ThemeTokens::regia_default(), "no override, no change");
    }
}
