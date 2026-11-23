// SPDX-License-Identifier: MIT-0
//! The demo's on-screen panel (`regia_ui`, built in code): the frame-rate overlay in the top-left corner, the
//! controls on the right (time-of-day and weather sliders, weather presets, the tier picker with the detection's
//! words, screenshot, benchmark and credits buttons, a status line, the key help) and the credits page.
//!
//! A widget only queues a [`DemoCommand`]; [`super::apply`] runs it like a key or an agent call, and
//! `refresh` writes the state back into the texts and slider positions, so a change made by an agent shows on
//! the panel too.

use regia::prelude::*;
use regia_render::QualityTier;
use regia_ui::style::{Align, FlexDirection};

use super::fps::FpsMeter;
use super::quality::{picker_tiers, tier_summary};
use super::{DemoCommand, DemoLastError, DemoPage, DemoQueue, DemoState, MAX_WIND_M_PER_S};

/// What an interactive node of the panel does.
#[derive(Component, Clone, Debug, PartialEq)]
pub enum DemoWidget {
    /// The hour slider (`0..=24`).
    Hour,
    /// The day-of-year slider (`1..=365`).
    Day,
    /// The cloud-cover slider (`0..=1`).
    Coverage,
    /// The rain-or-snow slider (`0..=1`).
    Precipitation,
    /// The wind slider (`0..=MAX_WIND_M_PER_S`).
    Wind,
    /// A weather preset button.
    Preset(String),
    /// A tier button.
    Tier(QualityTier),
    /// The "Auto" tier button.
    Auto,
    /// The screenshot button.
    Screenshot,
    /// The benchmark button (start, or stop while one runs).
    Benchmark,
    /// The credits button (and the credits page's "Back").
    Credits,
}

/// A text node the panel refreshes from the state.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemoText {
    /// The frame-rate overlay.
    Fps,
    /// The time-of-day value.
    TimeValue,
    /// The weather values.
    WeatherValue,
    /// The tier and the detection's words.
    Quality,
    /// Screenshots, benchmark and the last refusal.
    Status,
    /// The credits page's body.
    Credits,
}

/// Marks the controls panel.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ControlsPanel;

/// Marks the credits page.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct CreditsPanel;

/// The controls panel's width at the 1080p reference size: every button row fits on one line at 16:9.
pub const PANEL_WIDTH: f32 = 470.0;
/// The tallest the credits list gets at the 1080p reference size; a longer list scrolls.
pub const CREDITS_MAX_HEIGHT: f32 = 760.0;

/// The key help under the controls.
pub const HELP: &str = "Fly: W A S D, up E or Space, down Q or Ctrl, Shift faster, hold the right mouse button to look. \
Gamepad: left stick moves, right stick looks, shoulders and triggers fly up and down, stick click is faster. \
F1 panel, F2 credits, F12 screenshot, F9 benchmark, Esc stops it.";

/// Adds the UI plugin when the app has none, and the panel's systems.
pub fn install(app: &mut App) {
    if !app.has_plugin(std::any::type_name::<UiPlugin>()) {
        // A headless app has no platform plugin: register the window and pointer messages the UI reads (adding
        // a message twice keeps the first).
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::SystemThemeChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::TouchInput>();
        app.add_plugins(UiPlugin::code_only());
    }
    app.add_systems(Startup, spawn_panel)
        .add_systems(Update, widget_events.before(super::apply_queued))
        .add_systems(Update, refresh.after(super::apply_queued));
}

fn color(hex: &str) -> UiColor {
    UiColor::from_hex(hex).unwrap_or(UiColor::WHITE)
}

fn text(content: &str, px: f32, wrap: WrapMode) -> Text {
    Text {
        spans: vec![TextSpan::lit(content.to_owned()).with_style(TextStyle {
            font_px: px,
            color: color("#e8edf2"),
            ..Default::default()
        })],
        wrap,
        ..Default::default()
    }
}

fn child(commands: &mut Commands, parent: Entity, bundle: impl Bundle) -> Entity {
    let id = commands.spawn((UiNode, bundle)).id();
    commands.entity(parent).add_child(id);
    id
}

fn label(commands: &mut Commands, parent: Entity, content: &str) -> Entity {
    child(commands, parent, text(content, 15.0, WrapMode::Word))
}

fn button(commands: &mut Commands, parent: Entity, caption: &str, widget: DemoWidget) -> Entity {
    let id = regia_ui::widgets::spawn_button(commands, parent, caption, &[]);
    commands.entity(id).insert(widget);
    id
}

fn slider(commands: &mut Commands, parent: Entity, caption: &str, widget: DemoWidget, slider: Slider) {
    child(commands, parent, text(caption, 13.0, WrapMode::None));
    child(
        commands,
        parent,
        (
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(18.0)],
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            slider,
            Classes(vec!["slider-track".to_owned()]),
            Themed::default(),
            widget,
        ),
    );
}

fn row(commands: &mut Commands, parent: Entity) -> Entity {
    child(
        commands,
        parent,
        NodeStyle::new(Style {
            direction: FlexDirection::Row,
            size: [Dimension::Percent(100.0), Dimension::Auto],
            flex_wrap: true,
            gap: [Dimension::Points(6.0), Dimension::Points(6.0)],
            align_items: Align::Center,
            ..Default::default()
        }),
    )
}

fn panel(commands: &mut Commands, parent: Entity, style: Style, marker: impl Bundle) -> Entity {
    child(
        commands,
        parent,
        (
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                padding: Edges::px(14.0),
                gap: [Dimension::Points(6.0), Dimension::Points(6.0)],
                ..style
            }),
            Background {
                kind: regia_ui::BackgroundKind::Color(color("#0c1117d8")),
            },
            Border {
                color: color("#ffffff22"),
                widths: [1.0; 4],
                radius: [8.0; 4],
            },
            marker,
        ),
    )
}

fn absolute(left: Option<f32>, top: Option<f32>, right: Option<f32>) -> Style {
    let side = |v: Option<f32>| v.map_or(Dimension::Auto, Dimension::Points);
    Style {
        position: PositionType::Absolute,
        inset: Edges {
            left: side(left),
            top: side(top),
            right: side(right),
            bottom: Dimension::Auto,
        },
        ..Default::default()
    }
}

/// `Startup`: builds the overlay, the controls panel and the credits page.
pub fn spawn_panel(mut commands: Commands, state: Res<DemoState>) {
    let root = commands
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ScaleWithScreen {
                    reference: [1920.0, 1080.0],
                    match_width: 0.5,
                },
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    // The frame-rate overlay.
    let overlay = panel(&mut commands, root, absolute(Some(16.0), Some(16.0), None), ());
    child(
        &mut commands,
        overlay,
        (text("measuring frame rate", 18.0, WrapMode::None), DemoText::Fps),
    );

    // The controls.
    let controls = panel(
        &mut commands,
        root,
        Style {
            size: [Dimension::Points(PANEL_WIDTH), Dimension::Auto],
            ..absolute(None, Some(16.0), Some(16.0))
        },
        ControlsPanel,
    );
    label(&mut commands, controls, "Time of day");
    child(
        &mut commands,
        controls,
        (text("", 14.0, WrapMode::None), DemoText::TimeValue),
    );
    let tod = state.time_of_day;
    slider(
        &mut commands,
        controls,
        "Hour",
        DemoWidget::Hour,
        Slider {
            value: tod.hour,
            min: 0.0,
            max: 24.0,
            step: 0.0,
        },
    );
    slider(
        &mut commands,
        controls,
        "Day of the year",
        DemoWidget::Day,
        Slider {
            value: f32::from(tod.day_of_year),
            min: 1.0,
            max: 365.0,
            step: 1.0,
        },
    );

    label(&mut commands, controls, "Weather");
    child(
        &mut commands,
        controls,
        (text("", 14.0, WrapMode::None), DemoText::WeatherValue),
    );
    let presets = row(&mut commands, controls);
    // The player's weather range (ADR-5419): clear to cloudy, no rain or snow presets.
    for name in super::sun::PLAYER_WEATHER_PRESETS {
        if regia_vfx::WeatherState::preset(name).is_some() {
            button(&mut commands, presets, name, DemoWidget::Preset(name.to_owned()));
        }
    }
    let weather = &state.weather;
    slider(
        &mut commands,
        controls,
        "Cloud cover",
        DemoWidget::Coverage,
        Slider {
            value: weather.coverage.min(super::sun::PLAYER_MAX_COVERAGE),
            min: 0.0,
            max: super::sun::PLAYER_MAX_COVERAGE,
            step: 0.0,
        },
    );
    slider(
        &mut commands,
        controls,
        "Rain or snow",
        DemoWidget::Precipitation,
        Slider {
            value: weather.precipitation,
            min: 0.0,
            max: 1.0,
            step: 0.0,
        },
    );
    slider(
        &mut commands,
        controls,
        "Wind",
        DemoWidget::Wind,
        Slider {
            value: weather.wind_m_per_s,
            min: 0.0,
            max: MAX_WIND_M_PER_S,
            step: 0.0,
        },
    );

    label(&mut commands, controls, "Quality");
    let tiers = row(&mut commands, controls);
    button(&mut commands, tiers, "Auto", DemoWidget::Auto);
    for tier in picker_tiers() {
        button(&mut commands, tiers, tier.name(), DemoWidget::Tier(*tier));
    }
    child(
        &mut commands,
        controls,
        (text("", 14.0, WrapMode::Word), DemoText::Quality),
    );

    let actions = row(&mut commands, controls);
    button(&mut commands, actions, "Screenshot", DemoWidget::Screenshot);
    button(&mut commands, actions, "Benchmark", DemoWidget::Benchmark);
    button(&mut commands, actions, "Credits", DemoWidget::Credits);
    child(
        &mut commands,
        controls,
        (text("", 14.0, WrapMode::Word), DemoText::Status),
    );
    child(&mut commands, controls, text(HELP, 13.0, WrapMode::Word));

    // The credits page.
    let credits = panel(
        &mut commands,
        root,
        Style {
            size: [Dimension::Points(900.0), Dimension::Auto],
            display: Display::None,
            ..absolute(Some(510.0), Some(60.0), None)
        },
        CreditsPanel,
    );
    label(&mut commands, credits, "Credits");
    let list = commands
        .spawn(regia_ui::scroll_container(
            Style {
                size: [Dimension::Percent(100.0), Dimension::Auto],
                max_size: [Dimension::Auto, Dimension::Points(CREDITS_MAX_HEIGHT)],
                ..Default::default()
            },
            ScrollView::default(),
        ))
        .id();
    commands.entity(credits).add_child(list);
    child(
        &mut commands,
        list,
        (text("", 14.0, WrapMode::Word), DemoText::Credits),
    );
    button(&mut commands, credits, "Back (F2)", DemoWidget::Credits);
}

/// `Update`: widget clicks and slider moves queue their commands.
pub fn widget_events(
    mut events: MessageReader<UiEvent>,
    widgets: Query<&DemoWidget>,
    state: Res<DemoState>,
    mut queue: ResMut<DemoQueue>,
) {
    for event in events.read() {
        let Ok(widget) = widgets.get(event.target) else {
            continue;
        };
        if let Some(command) = widget_command(widget, &event.kind, &state) {
            queue.0.push(command);
        }
    }
}

/// The command a widget event asks for.
#[must_use]
pub fn widget_command(widget: &DemoWidget, kind: &UiEventKind, state: &DemoState) -> Option<DemoCommand> {
    let weather = |coverage, precipitation, wind_m_per_s| DemoCommand::SetWeather {
        preset: None,
        coverage,
        precipitation,
        wind_m_per_s,
        transition_s: 0.0,
    };
    match (widget, kind) {
        // The player's clock runs from sunrise to sunset (ADR-5419): the slider's hour is held there, and a new day
        // moves the hour into its own daylight.
        (DemoWidget::Hour, UiEventKind::ValueChanged(UiValue::Number(v))) => {
            Some(DemoCommand::SetTimeOfDay {
                hour: Some(super::sun::player_hour(&state.time_of_day, *v)),
                day_of_year: None,
                latitude_deg: None,
            })
        }
        (DemoWidget::Day, UiEventKind::ValueChanged(UiValue::Number(v))) => {
            let day = v.round().clamp(1.0, 365.0) as u16;
            let clock = regia_scene::level::LevelTimeOfDay {
                day_of_year: day,
                ..state.time_of_day
            };
            Some(DemoCommand::SetTimeOfDay {
                hour: Some(super::sun::player_hour(&clock, clock.hour)),
                day_of_year: Some(day),
                latitude_deg: None,
            })
        }
        (DemoWidget::Coverage, UiEventKind::ValueChanged(UiValue::Number(v))) => {
            Some(weather(Some(v.min(super::sun::PLAYER_MAX_COVERAGE)), None, None))
        }
        (DemoWidget::Precipitation, UiEventKind::ValueChanged(UiValue::Number(v))) => {
            Some(weather(None, Some(*v), None))
        }
        (DemoWidget::Wind, UiEventKind::ValueChanged(UiValue::Number(v))) => {
            Some(weather(None, None, Some(*v)))
        }
        (DemoWidget::Preset(name), UiEventKind::Click) => Some(DemoCommand::SetWeather {
            preset: Some(name.clone()),
            coverage: None,
            precipitation: None,
            wind_m_per_s: None,
            transition_s: 3.0,
        }),
        (DemoWidget::Tier(tier), UiEventKind::Click) => Some(DemoCommand::SetTier(*tier)),
        (DemoWidget::Auto, UiEventKind::Click) => Some(DemoCommand::DetectTier),
        (DemoWidget::Screenshot, UiEventKind::Click) => Some(DemoCommand::Screenshot(None)),
        (DemoWidget::Benchmark, UiEventKind::Click) => Some(if state.bench.running() {
            DemoCommand::CancelBenchmark
        } else {
            DemoCommand::StartBenchmark {
                warmup: None,
                frames: None,
                out: None,
            }
        }),
        (DemoWidget::Credits, UiEventKind::Click) => {
            Some(DemoCommand::ShowPage(if state.page == DemoPage::Credits {
                DemoPage::Controls
            } else {
                DemoPage::Credits
            }))
        }
        _ => None,
    }
}

/// The texts the panel shows for a state.
#[must_use]
pub fn panel_text(
    which: DemoText,
    state: &DemoState,
    meter: &FpsMeter,
    error: Option<&super::DemoError>,
) -> String {
    match which {
        DemoText::Fps => format!("{} | {}", meter.overlay_text(), state.tier.name()),
        DemoText::TimeValue => {
            let tod = state.time_of_day;
            let minutes = (tod.hour * 60.0).round() as u32;
            format!(
                "{:02}:{:02}, day {}, latitude {:.0}°",
                minutes / 60,
                minutes % 60,
                tod.day_of_year,
                tod.latitude_deg
            )
        }
        DemoText::WeatherValue => {
            let w = &state.weather;
            format!(
                "{}clouds {:.0} %, rain or snow {:.0} %, wind {:.0} m/s",
                w.preset.as_ref().map_or_else(String::new, |p| format!("{p}: ")),
                w.coverage * 100.0,
                w.precipitation * 100.0,
                w.wind_m_per_s
            )
        }
        DemoText::Quality => {
            let head = format!(
                "{}: {} ({})",
                state.tier.name(),
                tier_summary(state.tier),
                state.tier_source.name()
            );
            let mut out = match &state.detection {
                Some(words) => format!("{head}\n{}", words.words),
                None => head,
            };
            if let Some(note) = super::sun::sky_note(state.tier) {
                out.push('\n');
                out.push_str(note);
            }
            out
        }
        DemoText::Status => {
            let shots = match (&state.last_screenshot, &state.screenshot_error) {
                (_, Some(error)) => format!("Screenshot failed: {error}"),
                (Some(path), None) => format!("Screenshot {}: {}", state.screenshots, path.display()),
                (None, None) => "No screenshot yet (F12)".to_owned(),
            };
            let mut out = format!("{shots}\n{}", state.bench.summary());
            if let Some(error) = error {
                out.push_str(&format!("\nRefused: {error}"));
            }
            out
        }
        DemoText::Credits => state.credits.lines().join("\n"),
    }
}

/// `Update`, after the commands: texts, slider positions and which page shows.
fn refresh(
    state: Res<DemoState>,
    meter: Res<FpsMeter>,
    error: Res<DemoLastError>,
    mut texts: Query<(&DemoText, &mut Text)>,
    mut sliders: Query<(&DemoWidget, &mut Slider)>,
    mut controls: Query<&mut NodeStyle, (With<ControlsPanel>, Without<CreditsPanel>)>,
    mut credits: Query<&mut NodeStyle, (With<CreditsPanel>, Without<ControlsPanel>)>,
) {
    let _span = tracing::info_span!("demo_panel_refresh").entered();
    for (which, mut text) in &mut texts {
        if *which != DemoText::Fps && !state.is_changed() && !error.is_changed() {
            continue;
        }
        let content = panel_text(*which, &state, &meter, error.0.as_ref());
        let same = matches!(
            text.spans.first().map(|s| &s.content),
            Some(regia_ui::text::TextContent::Lit(current)) if *current == content
        );
        if !same && let Some(span) = text.spans.first_mut() {
            span.content = regia_ui::text::TextContent::Lit(content);
        }
    }
    if !state.is_changed() {
        return;
    }
    for (widget, mut slider) in &mut sliders {
        let value = match widget {
            DemoWidget::Hour => state.time_of_day.hour,
            DemoWidget::Day => f32::from(state.time_of_day.day_of_year),
            DemoWidget::Coverage => state.weather.coverage.min(super::sun::PLAYER_MAX_COVERAGE),
            DemoWidget::Precipitation => state.weather.precipitation,
            DemoWidget::Wind => state.weather.wind_m_per_s,
            _ => continue,
        };
        if (slider.value - value).abs() > 1e-4 {
            slider.value = value;
        }
    }
    let show = |style: &mut NodeStyle, on: bool| {
        let display = if on { Display::Flex } else { Display::None };
        if style.0.display != display {
            style.0.display = display;
        }
    };
    for mut style in &mut controls {
        show(&mut style, state.page == DemoPage::Controls);
    }
    for mut style in &mut credits {
        show(&mut style, state.page == DemoPage::Credits);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widgets_map_to_the_commands_the_keys_and_agents_use() {
        let state = crate::demo::tests_support::state();
        let number = |v| UiEventKind::ValueChanged(UiValue::Number(v));
        assert_eq!(
            widget_command(&DemoWidget::Hour, &number(18.5), &state),
            Some(DemoCommand::SetTimeOfDay {
                hour: Some(18.5),
                day_of_year: None,
                latitude_deg: None
            })
        );
        assert!(matches!(
            widget_command(&DemoWidget::Day, &number(40.4), &state),
            Some(DemoCommand::SetTimeOfDay {
                day_of_year: Some(40),
                ..
            })
        ));
        assert!(matches!(
            widget_command(&DemoWidget::Wind, &number(9.0), &state),
            Some(DemoCommand::SetWeather {
                wind_m_per_s: Some(_),
                preset: None,
                ..
            })
        ));
        assert_eq!(
            widget_command(&DemoWidget::Tier(QualityTier::Low), &UiEventKind::Click, &state),
            Some(DemoCommand::SetTier(QualityTier::Low))
        );
        assert_eq!(
            widget_command(&DemoWidget::Auto, &UiEventKind::Click, &state),
            Some(DemoCommand::DetectTier)
        );
        assert_eq!(
            widget_command(&DemoWidget::Screenshot, &UiEventKind::Click, &state),
            Some(DemoCommand::Screenshot(None))
        );
        assert_eq!(
            widget_command(&DemoWidget::Credits, &UiEventKind::Click, &state),
            Some(DemoCommand::ShowPage(DemoPage::Credits))
        );
        assert_eq!(
            widget_command(&DemoWidget::Hour, &UiEventKind::Click, &state),
            None
        );
    }

    /// ADR-5419: the panel keeps the player between sunrise and sunset and between clear and cloudy skies.
    #[test]
    fn the_panel_holds_the_clock_in_daylight_and_the_sky_under_the_cover_cap() {
        let mut state = crate::demo::tests_support::state();
        state.time_of_day = regia_scene::level::LevelTimeOfDay {
            hour: 8.0,
            day_of_year: 80,
            latitude_deg: 46.0,
        };
        let number = |v| UiEventKind::ValueChanged(UiValue::Number(v));
        let (rise, set) = super::super::sun::daylight_hours(80, 46.0).expect("daylight");
        let hour = |command: Option<DemoCommand>| match command {
            Some(DemoCommand::SetTimeOfDay { hour: Some(h), .. }) => h,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            hour(widget_command(&DemoWidget::Hour, &number(21.0), &state)),
            set
        );
        assert_eq!(
            hour(widget_command(&DemoWidget::Hour, &number(0.5), &state)),
            rise
        );
        assert_eq!(hour(widget_command(&DemoWidget::Hour, &number(9.5), &state)), 9.5);
        // A late hour on a long day: moving to a short day pulls it back to that day's sunset.
        state.time_of_day.day_of_year = 172;
        state.time_of_day.hour = 19.5;
        let winter = widget_command(&DemoWidget::Day, &number(355.0), &state);
        let (_, winter_set) = super::super::sun::daylight_hours(355, 46.0).expect("winter daylight");
        assert!(matches!(
            winter,
            Some(DemoCommand::SetTimeOfDay {
                day_of_year: Some(355),
                ..
            })
        ));
        assert_eq!(hour(winter), winter_set);
        match widget_command(&DemoWidget::Coverage, &number(1.0), &state) {
            Some(DemoCommand::SetWeather {
                coverage: Some(c), ..
            }) => {
                assert_eq!(c, super::super::sun::PLAYER_MAX_COVERAGE);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn panel_texts_read_in_words() {
        let mut state = crate::demo::tests_support::state();
        state.time_of_day.hour = 18.5;
        let meter = FpsMeter::default();
        assert!(panel_text(DemoText::TimeValue, &state, &meter, None).starts_with("18:30, day"));
        state.time_of_day.hour = 18.999;
        assert!(
            panel_text(DemoText::TimeValue, &state, &meter, None).starts_with("19:00, day"),
            "rounding the minutes carries into the hour"
        );
        assert!(panel_text(DemoText::Fps, &state, &meter, None).starts_with("measuring frame rate | High"));
        let error = super::super::DemoError::new(super::super::DemoErrorCode::NoViews, "none");
        assert!(panel_text(DemoText::Status, &state, &meter, Some(&error)).contains("demo_no_views"));
        assert!(panel_text(DemoText::Credits, &state, &meter, None).contains("Regia Engine"));
        assert!(!panel_text(DemoText::Quality, &state, &meter, None).contains("fixed sky"));
        state.tier = QualityTier::Low;
        assert!(panel_text(DemoText::Quality, &state, &meter, None).contains("fixed sky"));
    }
}
