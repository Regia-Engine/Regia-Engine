// SPDX-License-Identifier: MPL-2.0
//! The display calibration screen (ADR-092, book `user-guide/display-calibration.md`): a
//! reusable framework screen a game shows on first launch and from its options menu.
//!
//! Pages, in order: **brightness** (the black-level logo test: the left logo at
//! [`LOGO_VISIBLE_SIGNAL`] must be barely visible, the right one at [`LOGO_HIDDEN_SIGNAL`] must
//! not; on HDR also the HDR black slider), **contrast** (SDR gamma: a solid square at the 2.2
//! mid-grey code value inside 50 % black/white line stripes; matched, the display decodes like
//! the content was mastered), and on HDR **paper white** (a white card) and **peak brightness**
//! (the display stage draws a ring at the chosen peak on a 10 000 cd/m² background; it vanishes
//! once the peak reaches what the panel can show). Every page keeps a transparent strip through
//! which the live scene shows, so the player sees the game while adjusting.
//!
//! The patterns are ordinary UI, so they go through the display stage like everything else:
//! the player judges exactly the curve being set. Values preview live through the
//! [`DisplayCalibration`] resource; **Done** writes `graphics.calibration` in [`GameSettings`]
//! (persisted by the settings plugin), **Cancel** restores the previous calibration.
//!
//! Input: mouse and touch drive the sliders and buttons through `regia_ui`; keyboard arrows
//! and the gamepad d-pad move focus (up/down) and step the focused slider (left/right); Enter /
//! gamepad A activates, Escape / gamepad B goes back. Strings come from the game's
//! [`Localization`] when it defines a `calibration-*` key, otherwise from the bundled
//! `locales/<lang>/calibration.ftl` (en, de, fr, es).
//!
//! ```no_run
//! use regia_app::prelude::*;
//! use regia_framework::calibration_screen::{CalibrationScreen, CalibrationScreenPlugin};
//!
//! let mut app = App::new();
//! app.add_plugins(CalibrationScreenPlugin { open_on_first_launch: true });
//! // From an options menu:
//! app.world_mut().resource_mut::<CalibrationScreen>().open();
//! ```

use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;
use regia_input::Gamepads;
use regia_platform::{ButtonState, GamepadButton, KeyboardInput};
use regia_render::calibration::{LOGO_HIDDEN_SIGNAL, LOGO_VISIBLE_SIGNAL, NEUTRAL_GAMMA, decode_srgb};
use regia_render::{
    DisplayCalibration, DisplayLuminance, DisplayPlatform, HdrOutput, HdrTestPattern, OutputSettings,
};
use regia_ui::focus::{DefaultFocus, FocusScope, FocusState, Focusable, NavLinks, UiNavActions};
use regia_ui::interaction::{UiEvent, UiEventKind, UiValue};
use regia_ui::node::{
    Background, BackgroundKind, Border, ComputedNode, Focused, Interactable, NodeStyle, UiNode, UiRoot,
};
use regia_ui::style::{Align, Dimension, Edges, FlexDirection, JustifyContent, PositionType, Style};
use regia_ui::text::{Text, TextAlign, TextSpan, TextStyle, WrapMode};
use regia_ui::widgets::{Slider, spawn_button};
use regia_ui::{Classes, Localization, ScaleMode, Themed, UiColor};

use crate::settings::{CalibrationSettings, GameSettings};

/// Bundled strings: `(locale, ftl)`.
const BUNDLED_FTL: &[(&str, &str)] = &[
    ("en", include_str!("../locales/en/calibration.ftl")),
    ("de", include_str!("../locales/de/calibration.ftl")),
    ("fr", include_str!("../locales/fr/calibration.ftl")),
    ("es", include_str!("../locales/es/calibration.ftl")),
];

/// Luminance of the peak test's background, cd/m² (brighter than any display: it clips to the
/// panel's real maximum).
pub const PEAK_TEST_BACKGROUND_NITS: f32 = 10_000.0;

/// One page of the calibration screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationPage {
    /// The black-level logo test (and HDR black on HDR output).
    Brightness,
    /// SDR gamma (stripes vs solid square). Not shown on HDR output.
    Contrast,
    /// HDR paper white.
    PaperWhite,
    /// HDR peak brightness (display-stage ring test).
    Peak,
}

impl CalibrationPage {
    /// The pages for SDR or HDR output, in order.
    #[must_use]
    pub fn sequence(hdr: bool) -> &'static [Self] {
        if hdr {
            &[Self::Brightness, Self::PaperWhite, Self::Peak]
        } else {
            &[Self::Brightness, Self::Contrast]
        }
    }

    fn keys(self) -> (&'static str, &'static str) {
        match self {
            Self::Brightness => ("calibration-title-black", "calibration-help-black"),
            Self::Contrast => ("calibration-title-gamma", "calibration-help-gamma"),
            Self::PaperWhite => ("calibration-title-paper", "calibration-help-paper"),
            Self::Peak => ("calibration-title-peak", "calibration-help-peak"),
        }
    }
}

/// Which calibration value a slider edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationControl {
    /// [`DisplayCalibration::black_level`].
    BlackLevel,
    /// [`DisplayCalibration::gamma`].
    Gamma,
    /// [`DisplayCalibration::min_nits`].
    MinNits,
    /// [`DisplayCalibration::paper_white_nits`].
    PaperWhite,
    /// [`DisplayCalibration::peak_nits`].
    Peak,
}

impl CalibrationControl {
    /// `(min, max, step)` of the slider.
    #[must_use]
    pub fn range(self) -> (f32, f32, f32) {
        match self {
            Self::BlackLevel => (-1.0, 1.0, 0.05),
            Self::Gamma => (1.8, 2.6, 0.05),
            Self::MinNits => (0.0, 1.0, 0.01),
            Self::PaperWhite => (80.0, 500.0, 10.0),
            Self::Peak => (
                regia_render::calibration::PEAK_RANGE.0,
                regia_render::calibration::PEAK_RANGE.1,
                50.0,
            ),
        }
    }

    /// The value in `calibration`.
    #[must_use]
    pub fn get(self, c: &DisplayCalibration) -> f32 {
        match self {
            Self::BlackLevel => c.black_level,
            Self::Gamma => c.gamma,
            Self::MinNits => c.min_nits,
            Self::PaperWhite => c.paper_white_nits,
            Self::Peak => c.peak_nits,
        }
    }

    /// `calibration` with this value set (clamped to the player ranges).
    #[must_use]
    pub fn set(self, c: DisplayCalibration, value: f32) -> DisplayCalibration {
        let mut c = c;
        match self {
            Self::BlackLevel => c.black_level = value,
            Self::Gamma => c.gamma = value,
            Self::MinNits => c.min_nits = value,
            Self::PaperWhite => c.paper_white_nits = value,
            Self::Peak => c.peak_nits = value,
        }
        c.clamped()
    }

    fn key(self) -> &'static str {
        match self {
            Self::BlackLevel => "calibration-black-level",
            Self::Gamma => "calibration-gamma",
            Self::MinNits => "calibration-min-nits",
            Self::PaperWhite => "calibration-paper-white",
            Self::Peak => "calibration-peak",
        }
    }

    fn format(self, v: f32) -> String {
        match self {
            Self::BlackLevel => format!("{v:+.2}"),
            Self::Gamma => format!("{v:.2}"),
            Self::MinNits => format!("{v:.2} cd/m²"),
            Self::PaperWhite | Self::Peak => format!("{v:.0} cd/m²"),
        }
    }

    fn for_page(page: CalibrationPage, hdr: bool) -> &'static [Self] {
        match page {
            CalibrationPage::Brightness if hdr => &[Self::BlackLevel, Self::MinNits],
            CalibrationPage::Brightness => &[Self::BlackLevel],
            CalibrationPage::Contrast => &[Self::Gamma],
            CalibrationPage::PaperWhite => &[Self::PaperWhite],
            CalibrationPage::Peak => &[Self::Peak],
        }
    }
}

/// Sent when the screen closes.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalibrationScreenClosed {
    /// `true` when the player pressed Done (the calibration was saved).
    pub saved: bool,
}

/// What the screen's buttons do.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenButton {
    Back,
    Reset,
    Next,
}

/// A calibration slider and its value label.
#[derive(Component, Debug, Clone, Copy)]
struct ControlSlider {
    control: CalibrationControl,
    label: Entity,
}

/// Marks the node the HDR peak ring is drawn into.
#[derive(Component, Debug, Clone, Copy)]
struct PeakPatternArea;

#[derive(Debug, Clone)]
struct OpenState {
    page_index: usize,
    hdr: bool,
    working: DisplayCalibration,
    original: DisplayCalibration,
    root: Option<Entity>,
    /// Locale generation the tree was built with (rebuild on a language switch).
    built_for: Option<(usize, u64)>,
}

/// The calibration screen: [`Self::open`] shows it, the plugin's systems run it.
#[derive(Resource, Default)]
pub struct CalibrationScreen {
    state: Option<OpenState>,
    open_requested: bool,
    close_requested: Option<bool>,
    strings: Option<Localization>,
    /// Read positions, so events sent between frames are seen exactly once.
    ui_cursor: MessageCursor<UiEvent>,
    key_cursor: MessageCursor<KeyboardInput>,
}

impl CalibrationScreen {
    /// Shows the screen from its first page (no-op while open).
    pub fn open(&mut self) {
        if self.state.is_none() {
            self.open_requested = true;
        }
    }

    /// Closes the screen: `save` keeps and persists the values (Done), otherwise the previous
    /// calibration comes back (Cancel).
    pub fn close(&mut self, save: bool) {
        self.close_requested = Some(save);
    }

    /// Whether the screen is showing.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.state.is_some()
    }

    /// The page showing, when open.
    #[must_use]
    pub fn page(&self) -> Option<CalibrationPage> {
        self.state
            .as_ref()
            .and_then(|s| CalibrationPage::sequence(s.hdr).get(s.page_index).copied())
    }

    /// The values being adjusted (live in [`DisplayCalibration`] while open).
    #[must_use]
    pub fn working(&self) -> Option<DisplayCalibration> {
        self.state.as_ref().map(|s| s.working)
    }

    /// True when the player has never calibrated (show the screen on first launch).
    #[must_use]
    pub fn needs_first_launch(settings: &GameSettings) -> bool {
        settings.graphics.calibration.is_none()
    }
}

/// Adds the calibration screen ([`CalibrationScreen`]). Needs `regia_ui::UiPlugin`; works
/// with or without the framework's settings plugin (without it, Done keeps the values for the
/// session only).
#[derive(Debug, Default, Clone, Copy)]
pub struct CalibrationScreenPlugin {
    /// Open the screen on the first frame when [`GameSettings`] holds no calibration yet.
    pub open_on_first_launch: bool,
}

impl Plugin for CalibrationScreenPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CalibrationScreen>()
            .add_message::<CalibrationScreenClosed>()
            .add_systems(
                Update,
                (
                    calibration_screen_input.before(regia_ui::focus::ui_focus_navigation),
                    calibration_screen_update.after(regia_ui::widgets::ui_widgets),
                ),
            );
        if self.open_on_first_launch {
            app.add_systems(Startup, open_on_first_launch);
        }
    }
}

fn open_on_first_launch(settings: Option<Res<GameSettings>>, mut screen: ResMut<CalibrationScreen>) {
    if settings.is_some_and(|s| CalibrationScreen::needs_first_launch(&s)) {
        screen.open();
    }
}

/// Resolves a string: the game's localization first, then the bundled table in the game's
/// language (or English).
fn tr(world: &mut World, key: &str, args: &[(String, String)]) -> String {
    if let Some(game) = world.get_resource::<Localization>()
        && game.has(key)
    {
        return game.resolve(key, args);
    }
    let locale = world
        .get_resource::<Localization>()
        .map(|l| l.locale().to_owned())
        .unwrap_or_default();
    let mut screen = world.resource_mut::<CalibrationScreen>();
    let strings = screen.strings.get_or_insert_with(|| {
        let sources = BUNDLED_FTL
            .iter()
            .map(|(l, s)| ((*l).to_owned(), (*s).to_owned()))
            .collect();
        Localization::new(sources, Some("en".to_owned())).unwrap_or_default()
    });
    let lang = locale.split(['-', '_']).next().unwrap_or("en");
    if !strings.set_locale(&locale) && strings.locale() != lang && !strings.set_locale(lang) {
        strings.set_locale("en");
    }
    strings.resolve(key, args)
}

/// `Update`, before focus navigation: gamepad to UI navigation, left/right on a focused
/// calibration slider steps its value.
fn calibration_screen_input(world: &mut World) {
    let keys: Vec<KeyboardInput> = world.resource_scope(|world, mut screen: Mut<CalibrationScreen>| {
        world
            .get_resource::<Messages<KeyboardInput>>()
            .map(|m| screen.key_cursor.read(m).cloned().collect())
            .unwrap_or_default()
    });
    if !world.resource::<CalibrationScreen>().is_open() {
        return;
    }
    let mut step = 0i32;
    for e in keys.iter().filter(|e| e.state == ButtonState::Pressed) {
        match e.logical_key.as_str() {
            "ArrowLeft" | "Left" => step -= 1,
            "ArrowRight" | "Right" => step += 1,
            _ => {}
        }
    }
    if let Some(pads) = world.get_resource::<Gamepads>() {
        let pressed = |b| pads.just_pressed(b).is_some();
        if pressed(GamepadButton::DPadLeft) {
            step -= 1;
        }
        if pressed(GamepadButton::DPadRight) {
            step += 1;
        }
        let (up, down, submit, cancel) = (
            pressed(GamepadButton::DPadUp),
            pressed(GamepadButton::DPadDown),
            pressed(GamepadButton::South),
            pressed(GamepadButton::East),
        );
        if let Some(mut nav) = world.get_resource_mut::<UiNavActions>() {
            nav.up |= up;
            nav.down |= down;
            nav.submit |= submit;
            nav.cancel |= cancel;
        }
    }
    if step == 0 {
        return;
    }
    let Some(focused) = world.get_resource::<FocusState>().and_then(|f| f.focused) else {
        return;
    };
    let (Some(control), Some(slider)) = (
        world.get::<ControlSlider>(focused).copied(),
        world.get::<Slider>(focused).copied(),
    ) else {
        return;
    };
    let value = (slider.value + step as f32 * slider.step).clamp(slider.min, slider.max);
    if let Some(mut s) = world.get_mut::<Slider>(focused) {
        s.value = value;
    }
    set_value(world, control, value);
}

/// Applies a slider value to the working calibration (live preview) and its label.
fn set_value(world: &mut World, control: ControlSlider, value: f32) {
    let Some(working) = world.resource::<CalibrationScreen>().working() else {
        return;
    };
    let working = control.control.set(working, value);
    if let Some(state) = world.resource_mut::<CalibrationScreen>().state.as_mut() {
        state.working = working;
    }
    world.insert_resource(working);
    if let Some(mut text) = world.get_mut::<Text>(control.label) {
        text.spans = vec![TextSpan::lit(
            control.control.format(control.control.get(&working)),
        )];
    }
}

/// `Update`, after the widgets: open/close requests, slider and button events, page builds and
/// the HDR peak pattern.
fn calibration_screen_update(world: &mut World) {
    let _span = tracing::trace_span!("calibration_screen").entered();
    if std::mem::take(&mut world.resource_mut::<CalibrationScreen>().open_requested) {
        open_state(world);
    }
    if let Some(save) = world.resource_mut::<CalibrationScreen>().close_requested.take() {
        close_state(world, save);
        return;
    }
    let events: Vec<UiEvent> = world.resource_scope(|world, mut screen: Mut<CalibrationScreen>| {
        world
            .get_resource::<Messages<UiEvent>>()
            .map(|m| screen.ui_cursor.read(m).cloned().collect())
            .unwrap_or_default()
    });
    if !world.resource::<CalibrationScreen>().is_open() {
        return;
    }
    let root = world
        .resource::<CalibrationScreen>()
        .state
        .as_ref()
        .and_then(|s| s.root);
    for event in events {
        match event.kind {
            UiEventKind::ValueChanged(UiValue::Number(v)) => {
                if let Some(control) = world.get::<ControlSlider>(event.target).copied() {
                    set_value(world, control, v);
                }
            }
            UiEventKind::Click => {
                let button = world.get::<ScreenButton>(event.target).copied();
                let action = if Some(event.target) == root {
                    Some(ScreenButton::Back)
                } else {
                    button
                };
                match action {
                    Some(ScreenButton::Back) => go_to(world, -1),
                    Some(ScreenButton::Next) => go_to(world, 1),
                    Some(ScreenButton::Reset) => reset_page(world),
                    None => {}
                }
                if !world.resource::<CalibrationScreen>().is_open() {
                    return;
                }
            }
            _ => {}
        }
    }
    let generation = world.get_resource::<Localization>().map_or(0, |l| l.generation.0);
    let (page_index, built_for) = {
        let state = world
            .resource::<CalibrationScreen>()
            .state
            .as_ref()
            .map(|s| (s.page_index, s.built_for));
        match state {
            Some(s) => s,
            None => return,
        }
    };
    if built_for != Some((page_index, generation)) {
        build_page(world, generation);
    }
    place_peak_pattern(world);
}

fn open_state(world: &mut World) {
    let hdr = world
        .get_resource::<OutputSettings>()
        .is_some_and(|o| o.hdr != HdrOutput::Sdr);
    let current = world
        .get_resource::<DisplayCalibration>()
        .copied()
        .unwrap_or_else(|| {
            DisplayCalibration::platform_default(
                world
                    .get_resource::<DisplayPlatform>()
                    .copied()
                    .unwrap_or_else(DisplayPlatform::current),
            )
        });
    world.resource_mut::<CalibrationScreen>().state = Some(OpenState {
        page_index: 0,
        hdr,
        working: current,
        original: current,
        root: None,
        built_for: None,
    });
    world.insert_resource(current);
}

fn close_state(world: &mut World, save: bool) {
    let Some(state) = world.resource_mut::<CalibrationScreen>().state.take() else {
        return;
    };
    if let Some(root) = state.root {
        world.despawn(root);
    }
    world.remove_resource::<HdrTestPattern>();
    if save {
        world.insert_resource(state.working);
        if let Some(mut settings) = world.get_resource_mut::<GameSettings>() {
            settings.graphics.calibration =
                Some(CalibrationSettings::from_calibration(&state.working, state.hdr));
        }
    } else {
        world.insert_resource(state.original);
    }
    world.write_message(CalibrationScreenClosed { saved: save });
}

fn go_to(world: &mut World, delta: i32) {
    let Some((index, hdr)) = world
        .resource::<CalibrationScreen>()
        .state
        .as_ref()
        .map(|s| (s.page_index as i32, s.hdr))
    else {
        return;
    };
    let next = index + delta;
    let count = CalibrationPage::sequence(hdr).len() as i32;
    if next < 0 {
        close_state(world, false);
    } else if next >= count {
        close_state(world, true);
    } else if let Some(state) = world.resource_mut::<CalibrationScreen>().state.as_mut() {
        state.page_index = next as usize;
    }
}

fn reset_page(world: &mut World) {
    let screen = world.resource::<CalibrationScreen>();
    let (Some(page), Some(state)) = (screen.page(), screen.state.as_ref()) else {
        return;
    };
    let hdr = state.hdr;
    let mut working = state.working;
    let platform = world
        .get_resource::<DisplayPlatform>()
        .copied()
        .unwrap_or_else(DisplayPlatform::current);
    let mut default = DisplayCalibration::platform_default(platform);
    if let Some(display) = world.get_resource::<DisplayLuminance>() {
        default = default.with_display(display);
    }
    for control in CalibrationControl::for_page(page, hdr) {
        working = control.set(working, control.get(&default));
    }
    if let Some(state) = world.resource_mut::<CalibrationScreen>().state.as_mut() {
        state.working = working;
        state.built_for = None;
    }
    world.insert_resource(working);
}

/// Places the display stage's peak ring over the pattern node (HDR peak page only).
fn place_peak_pattern(world: &mut World) {
    let screen = world.resource::<CalibrationScreen>();
    let on_peak = screen.page() == Some(CalibrationPage::Peak);
    let (root, peak) = match screen.state.as_ref() {
        Some(s) => (s.root, s.working.peak_nits),
        None => (None, 0.0),
    };
    if !on_peak {
        if world.contains_resource::<HdrTestPattern>() {
            world.remove_resource::<HdrTestPattern>();
        }
        return;
    }
    let area = world
        .query_filtered::<&ComputedNode, With<PeakPatternArea>>()
        .iter(world)
        .next()
        .copied();
    let window = root.and_then(|r| world.get::<ComputedNode>(r).copied());
    let (Some(area), Some(window)) = (area, window) else {
        return;
    };
    let (w, h) = (window.rect[2].max(1.0), window.rect[3].max(1.0));
    let pattern = HdrTestPattern {
        rect: [
            (area.rect[0] - window.rect[0]) / w,
            (area.rect[1] - window.rect[1]) / h,
            area.rect[2] / w,
            area.rect[3] / h,
        ],
        logo_nits: peak,
        background_nits: PEAK_TEST_BACKGROUND_NITS,
    };
    if world.get_resource::<HdrTestPattern>() != Some(&pattern) {
        world.insert_resource(pattern);
    }
}

/// A UI colour showing display signal `signal` (sRGB-encoded 0..1) on an SDR surface.
fn signal_color(signal: f32) -> UiColor {
    let l = decode_srgb(signal);
    UiColor::linear(l, l, l, 1.0)
}

fn text(world: &mut World, s: String, px: f32, color: UiColor, align: TextAlign) -> Entity {
    world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan {
                    style: TextStyle {
                        font_px: px,
                        color,
                        ..Default::default()
                    },
                    ..TextSpan::lit(s)
                }],
                align,
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id()
}

fn node(world: &mut World, style: Style, color: Option<UiColor>) -> Entity {
    let mut e = world.spawn((UiNode, NodeStyle::new(style)));
    if let Some(color) = color {
        e.insert(Background {
            kind: BackgroundKind::Color(color),
        });
    }
    e.id()
}

fn px(v: f32) -> Dimension {
    Dimension::Points(v)
}

fn add_children(world: &mut World, parent: Entity, children: &[Entity]) {
    let mut parent = world.entity_mut(parent);
    for child in children {
        parent.add_child(*child);
    }
}

/// The black-level logo: a ring with a centre dot at `signal` on black.
fn logo(world: &mut World, signal: f32, caption: String) -> Entity {
    let color = signal_color(signal);
    let ring = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [px(150.0), px(150.0)],
                border: Edges::px(22.0),
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            Border {
                color,
                widths: [22.0; 4],
                radius: [75.0; 4],
            },
        ))
        .id();
    let dot = node(
        world,
        Style {
            size: [px(40.0), px(40.0)],
            ..Default::default()
        },
        Some(color),
    );
    add_children(world, ring, &[dot]);
    let label = text(world, caption, 15.0, signal_color(0.35), TextAlign::Center);
    let column = node(
        world,
        Style {
            direction: FlexDirection::Column,
            align_items: Align::Center,
            gap: [px(12.0), px(12.0)],
            ..Default::default()
        },
        None,
    );
    add_children(world, column, &[ring, label]);
    column
}

/// The gamma pattern: 1 px black/white line pairs (50 % of white on any display) around a
/// solid square at the code value a 2.2 display shows as 50 %.
fn gamma_pattern(world: &mut World) -> Entity {
    let (w, h) = (300.0, 180.0);
    let stripes = node(
        world,
        Style {
            direction: FlexDirection::Column,
            size: [px(w), px(h)],
            ..Default::default()
        },
        None,
    );
    let lines: Vec<Entity> = (0..h as usize)
        .map(|i| {
            let color = if i % 2 == 0 {
                UiColor::WHITE
            } else {
                UiColor::BLACK
            };
            node(
                world,
                Style {
                    size: [px(w), px(1.0)],
                    flex_shrink: 0.0,
                    ..Default::default()
                },
                Some(color),
            )
        })
        .collect();
    add_children(world, stripes, &lines);
    let solid = node(
        world,
        Style {
            position: PositionType::Absolute,
            inset: Edges {
                left: px((w - 100.0) / 2.0),
                top: px((h - 100.0) / 2.0),
                ..Edges::default()
            },
            size: [px(100.0), px(100.0)],
            ..Default::default()
        },
        Some(signal_color(0.5_f32.powf(1.0 / NEUTRAL_GAMMA))),
    );
    add_children(world, stripes, &[solid]);
    stripes
}

fn pattern(world: &mut World, page: CalibrationPage) -> Entity {
    let row = node(
        world,
        Style {
            direction: FlexDirection::Row,
            justify_content: JustifyContent::Center,
            align_items: Align::Center,
            gap: [px(120.0), px(120.0)],
            min_size: [Dimension::Auto, px(220.0)],
            ..Default::default()
        },
        None,
    );
    let children = match page {
        CalibrationPage::Brightness => {
            let visible = tr(world, "calibration-visible", &[]);
            let hidden = tr(world, "calibration-hidden", &[]);
            vec![
                logo(world, LOGO_VISIBLE_SIGNAL, visible),
                logo(world, LOGO_HIDDEN_SIGNAL, hidden),
            ]
        }
        CalibrationPage::Contrast => vec![gamma_pattern(world)],
        CalibrationPage::PaperWhite => {
            // A white card next to an 18 % grey card: UI white is paper white on HDR.
            let card = |world: &mut World, color| {
                node(
                    world,
                    Style {
                        size: [px(200.0), px(140.0)],
                        ..Default::default()
                    },
                    Some(color),
                )
            };
            vec![
                card(world, UiColor::WHITE),
                card(world, UiColor::linear(0.18, 0.18, 0.18, 1.0)),
            ]
        }
        CalibrationPage::Peak => {
            let area = world
                .spawn((
                    UiNode,
                    NodeStyle::new(Style {
                        size: [px(240.0), px(240.0)],
                        ..Default::default()
                    }),
                    PeakPatternArea,
                ))
                .id();
            vec![area]
        }
    };
    add_children(world, row, &children);
    row
}

/// A labelled slider row; returns `(row, slider)`.
fn slider_row(world: &mut World, control: CalibrationControl, value: f32, first: bool) -> (Entity, Entity) {
    let name = tr(world, control.key(), &[]);
    let label = text(world, name, 18.0, UiColor::WHITE, TextAlign::End);
    world.entity_mut(label).insert(NodeStyle::new(Style {
        size: [px(200.0), Dimension::Auto],
        ..Default::default()
    }));
    let value_label = text(
        world,
        control.format(value),
        18.0,
        UiColor::WHITE,
        TextAlign::Start,
    );
    world.entity_mut(value_label).insert(NodeStyle::new(Style {
        size: [px(130.0), Dimension::Auto],
        ..Default::default()
    }));
    let (min, max, step) = control.range();
    let max = if control == CalibrationControl::Peak {
        max.max(value)
    } else {
        max
    };
    let mut slider = world.spawn((
        UiNode,
        NodeStyle::new(Style {
            size: [px(420.0), px(28.0)],
            ..Default::default()
        }),
        Interactable::default(),
        Focusable::default(),
        // The screen steps its sliders itself (`calibration_screen_input`).
        regia_ui::focus::ManualValueNav,
        Slider {
            value,
            min,
            max,
            step,
        },
        ControlSlider {
            control,
            label: value_label,
        },
        Classes(vec!["slider-track".into()]),
        Themed::default(),
    ));
    if first {
        slider.insert(DefaultFocus);
    }
    let slider = slider.id();
    // Left/right step the value (see `calibration_screen_input`), never move focus.
    world.entity_mut(slider).insert(Focusable {
        nav: NavLinks {
            left: Some(slider),
            right: Some(slider),
            ..NavLinks::default()
        },
    });
    let row = node(
        world,
        Style {
            direction: FlexDirection::Row,
            justify_content: JustifyContent::Center,
            align_items: Align::Center,
            gap: [px(16.0), px(16.0)],
            ..Default::default()
        },
        None,
    );
    add_children(world, row, &[label, slider, value_label]);
    (row, slider)
}

/// (Re)builds the current page's widget tree.
fn build_page(world: &mut World, generation: u64) {
    let Some(state) = world.resource::<CalibrationScreen>().state.clone() else {
        return;
    };
    if let Some(root) = state.root {
        world.despawn(root);
    }
    let pages = CalibrationPage::sequence(state.hdr);
    let Some(&page) = pages.get(state.page_index) else {
        return;
    };
    let black = UiColor::BLACK;
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                sort_order: 1000,
                ..Default::default()
            },
            UiNode,
            FocusScope,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    // The panel: pure black (the logo test needs true black around the logos).
    let panel = node(
        world,
        Style {
            direction: FlexDirection::Column,
            align_items: Align::Center,
            padding: Edges::px(28.0),
            gap: [px(18.0), px(18.0)],
            flex_grow: 1.0,
            ..Default::default()
        },
        Some(black),
    );
    let (title_key, help_key) = page.keys();
    let step = tr(
        world,
        "calibration-step",
        &[
            ("page".to_owned(), (state.page_index + 1).to_string()),
            ("count".to_owned(), pages.len().to_string()),
        ],
    );
    let step = text(world, step, 14.0, signal_color(0.55), TextAlign::Center);
    let title = tr(world, title_key, &[]);
    let title = text(world, title, 30.0, UiColor::WHITE, TextAlign::Center);
    let help = tr(world, help_key, &[]);
    let help = text(world, help, 18.0, signal_color(0.8), TextAlign::Center);
    if let Some(mut t) = world.get_mut::<Text>(help) {
        t.wrap = WrapMode::Word;
    }
    world.entity_mut(help).insert(NodeStyle::new(Style {
        max_size: [px(760.0), Dimension::Auto],
        ..Default::default()
    }));
    let pattern = pattern(world, page);
    let mut rows = Vec::new();
    let mut first_slider = None;
    for (i, control) in CalibrationControl::for_page(page, state.hdr).iter().enumerate() {
        let (row, slider) = slider_row(world, *control, control.get(&state.working), i == 0);
        rows.push(row);
        first_slider.get_or_insert(slider);
    }
    let buttons = node(
        world,
        Style {
            direction: FlexDirection::Row,
            justify_content: JustifyContent::Center,
            gap: [px(16.0), px(16.0)],
            ..Default::default()
        },
        None,
    );
    let back_key = if state.page_index == 0 {
        "calibration-cancel"
    } else {
        "calibration-back"
    };
    let next_key = if state.page_index + 1 == pages.len() {
        "calibration-done"
    } else {
        "calibration-next"
    };
    for (button, key, classes) in [
        (ScreenButton::Back, back_key, &[][..]),
        (ScreenButton::Reset, "calibration-reset", &[][..]),
        (ScreenButton::Next, next_key, &["primary"][..]),
    ] {
        let label = tr(world, key, &[]);
        let mut commands = world.commands();
        let entity = spawn_button(&mut commands, buttons, &label, classes);
        commands.entity(entity).insert(button);
        world.flush();
    }
    let mut children = vec![step, title, help, pattern];
    children.extend(rows);
    children.push(buttons);
    add_children(world, panel, &children);
    // The live scene preview: a transparent strip along the bottom.
    let strip = node(
        world,
        Style {
            size: [Dimension::Percent(100.0), Dimension::Percent(22.0)],
            flex_shrink: 0.0,
            padding: Edges::px(8.0),
            ..Default::default()
        },
        None,
    );
    let caption = tr(world, "calibration-preview", &[]);
    let caption = text(world, caption, 14.0, UiColor::WHITE, TextAlign::Start);
    add_children(world, strip, &[caption]);
    let divider = node(
        world,
        Style {
            size: [Dimension::Percent(100.0), px(2.0)],
            flex_shrink: 0.0,
            ..Default::default()
        },
        Some(signal_color(0.3)),
    );
    add_children(world, root, &[panel, divider, strip]);
    // Focus starts on the page's first slider, so arrows and the d-pad work at once.
    if let Some(mut focus) = world.get_resource_mut::<FocusState>() {
        focus.focused = first_slider;
    }
    if let Some(slider) = first_slider {
        world.entity_mut(slider).insert(Focused);
    }
    if let Some(state) = world.resource_mut::<CalibrationScreen>().state.as_mut() {
        state.root = Some(root);
        state.built_for = Some((state.page_index, generation));
    }
    if let Some(mut dirty) = world.get_resource_mut::<regia_ui::LayoutDirty>() {
        dirty.mark();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_slider_reaches_the_render_peak_range() {
        let (min, max, step) = CalibrationControl::Peak.range();
        assert_eq!(min, regia_render::calibration::PEAK_RANGE.0);
        assert_eq!(max, regia_render::calibration::PEAK_RANGE.1);
        assert_eq!(max, 10_000.0);
        assert_eq!(step, 50.0);
    }

    fn app(hdr: bool) -> App {
        let mut app = App::new();
        app.add_plugins(regia_ui::UiPlugin::code_only())
            .add_plugins(CalibrationScreenPlugin::default());
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        app.insert_resource(DisplayCalibration::NEUTRAL);
        app.insert_resource(GameSettings::default());
        app.insert_resource(OutputSettings {
            hdr: if hdr { HdrOutput::Hdr10 } else { HdrOutput::Sdr },
            ..OutputSettings::default()
        });
        app
    }

    fn key(app: &mut App, name: &str) {
        let window = Entity::PLACEHOLDER;
        app.world_mut().write_message(KeyboardInput {
            window,
            physical_key: name.to_owned(),
            logical_key: name.to_owned(),
            state: ButtonState::Pressed,
            repeat: false,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
    }

    fn focused_control(app: &mut App) -> Option<CalibrationControl> {
        let focused = app.world().resource::<FocusState>().focused?;
        app.world().get::<ControlSlider>(focused).map(|c| c.control)
    }

    #[test]
    fn keyboard_walks_the_sdr_pages_and_saves() {
        let mut app = app(false);
        app.world_mut().resource_mut::<CalibrationScreen>().open();
        for _ in 0..3 {
            app.update();
        }
        let screen = app.world().resource::<CalibrationScreen>();
        assert_eq!(screen.page(), Some(CalibrationPage::Brightness));
        // Focus starts on the black-level slider.
        assert_eq!(focused_control(&mut app), Some(CalibrationControl::BlackLevel));
        for _ in 0..4 {
            key(&mut app, "ArrowRight");
            app.update();
        }
        let live = *app.world().resource::<DisplayCalibration>();
        assert!((live.black_level - 0.2).abs() < 1.0e-4, "live preview {live:?}");
        app.world_mut().resource_mut::<CalibrationScreen>().close(true);
        app.update();
        assert!(!app.world().resource::<CalibrationScreen>().is_open());
        let saved = app
            .world()
            .resource::<GameSettings>()
            .graphics
            .calibration
            .clone();
        assert!(saved.is_some_and(|c| (c.black_level - 0.2).abs() < 1.0e-4));
    }

    #[test]
    fn buttons_page_through_and_cancel_restores() {
        let mut app = app(false);
        app.world_mut().resource_mut::<CalibrationScreen>().open();
        app.update();
        app.update();
        // Click Next via a UI event, as the pointer or Submit would.
        let next = app
            .world_mut()
            .query::<(Entity, &ScreenButton)>()
            .iter(app.world())
            .find(|(_, b)| **b == ScreenButton::Next)
            .map(|(e, _)| e)
            .expect("next button");
        app.world_mut()
            .write_message(UiEvent::new(next, UiEventKind::Click));
        app.update();
        app.update();
        assert_eq!(
            app.world().resource::<CalibrationScreen>().page(),
            Some(CalibrationPage::Contrast)
        );
        // Drag the gamma slider through a ValueChanged, then cancel with Escape (twice: back,
        // then cancel on the first page).
        let gamma = app
            .world_mut()
            .query::<(Entity, &ControlSlider)>()
            .iter(app.world())
            .find(|(_, c)| c.control == CalibrationControl::Gamma)
            .map(|(e, _)| e)
            .expect("gamma slider");
        app.world_mut().write_message(UiEvent::new(
            gamma,
            UiEventKind::ValueChanged(UiValue::Number(2.5)),
        ));
        app.update();
        assert_eq!(app.world().resource::<DisplayCalibration>().gamma, 2.5);
        key(&mut app, "Escape");
        app.update();
        app.update();
        key(&mut app, "Escape");
        app.update();
        assert!(!app.world().resource::<CalibrationScreen>().is_open());
        assert_eq!(
            *app.world().resource::<DisplayCalibration>(),
            DisplayCalibration::NEUTRAL,
            "cancel restores"
        );
        assert_eq!(app.world().resource::<GameSettings>().graphics.calibration, None);
    }

    #[test]
    fn hdr_pages_drive_the_peak_pattern() {
        let mut app = app(true);
        app.world_mut().resource_mut::<CalibrationScreen>().open();
        app.update();
        assert_eq!(
            CalibrationPage::sequence(true),
            &[
                CalibrationPage::Brightness,
                CalibrationPage::PaperWhite,
                CalibrationPage::Peak
            ]
        );
        for _ in 0..2 {
            let next = app
                .world_mut()
                .query::<(Entity, &ScreenButton)>()
                .iter(app.world())
                .find(|(_, b)| **b == ScreenButton::Next)
                .map(|(e, _)| e)
                .expect("next button");
            app.world_mut()
                .write_message(UiEvent::new(next, UiEventKind::Click));
            app.update();
            app.update();
        }
        assert_eq!(
            app.world().resource::<CalibrationScreen>().page(),
            Some(CalibrationPage::Peak)
        );
        app.update();
        let pattern = app.world().get_resource::<HdrTestPattern>().copied();
        let pattern = pattern.expect("peak pattern placed");
        assert_eq!(pattern.background_nits, PEAK_TEST_BACKGROUND_NITS);
        assert_eq!(pattern.logo_nits, DisplayCalibration::NEUTRAL.peak_nits);
        app.world_mut().resource_mut::<CalibrationScreen>().close(true);
        app.update();
        assert!(app.world().get_resource::<HdrTestPattern>().is_none());
        assert!(
            app.world()
                .resource::<GameSettings>()
                .graphics
                .calibration
                .as_ref()
                .is_some_and(|c| c.hdr)
        );
    }

    #[test]
    fn bundled_strings_are_localised_and_complete() {
        let en = BUNDLED_FTL[0].1;
        let keys: Vec<&str> = en
            .lines()
            .filter_map(|l| l.split_once(" = ").map(|(k, _)| k))
            .collect();
        assert!(keys.len() >= 20);
        let sources = BUNDLED_FTL
            .iter()
            .map(|(l, s)| ((*l).to_owned(), (*s).to_owned()))
            .collect();
        let mut loc = Localization::new(sources, Some("en".to_owned())).expect("bundled ftl parses");
        for (locale, _) in BUNDLED_FTL {
            assert!(loc.set_locale(locale) || loc.locale() == *locale);
            for key in &keys {
                assert!(loc.has(key), "{locale} lacks {key}");
            }
        }
        loc.set_locale("de");
        assert_eq!(loc.resolve("calibration-done", &[]), "Fertig");
    }
}
