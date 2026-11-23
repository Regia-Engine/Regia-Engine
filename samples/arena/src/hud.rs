// SPDX-License-Identifier: MIT-0
//! HUD and menus (`docs/specs/14` M20 `samples/arena`): a score/timer/phase readout, a
//! crosshair, a touch stick and fire button, and a pause menu with a resume button — all built
//! with `regia_ui`.
//!
//! Pause toggles on Escape (keyboard) or Start (gamepad). With the engine pause on (`[game] pause = true`
//! in the sample's `Regia.toml`, ADR-10104) the engine owns it: Escape and Start are the `TogglePause`
//! bindings, gameplay input is ignored and the cursor is freed while paused, and this menu is the game's
//! own pause screen (`pause_screen = "none"`): it follows `GamePause`, and its Resume button carries the
//! `pause.resume` UI command. Without the engine pause the sample toggles `Time::paused` itself. The
//! menu's buttons are `Focusable` and
//! `Interactable`, so `regia_ui`'s focus navigation reaches them on a gamepad and the pointer
//! activates them on touch. The virtual stick and fire button write axis and button values;
//! a bridge copies those into [`regia_input::InputInjection`] so the same `Move` and `Fire`
//! actions the keyboard and gamepad drive also move and shoot.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::game_pause::{GamePause, GamePauseSet, RESUME_COMMAND};
use regia_input::Gamepads;
use regia_platform::{GamepadButton, KeyboardInput};
use regia_ui::node::{Background, BackgroundKind, NodeStyle, UiNode, UiRoot};
use regia_ui::style::{Align, Dimension, Display, Edges, FlexDirection, JustifyContent, Style};
use regia_ui::text::{Text, TextSpan, TextStyle};
use regia_ui::widgets::{VirtualButton, VirtualStick, spawn_button};
use regia_ui::{Interactable, UiColor, UiEvent, UiEventKind, WrapMode};

use crate::match_flow::{ArenaState, Phase};

/// The HUD text fields.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum HudText {
    /// The score line.
    Score,
    /// The timer line.
    Timer,
    /// The phase line (warm-up / round / match over / paused).
    Phase,
}

/// The pause menu's root node (toggled visible/hidden).
#[derive(Resource, Debug, Clone, Copy)]
pub struct PauseMenu(pub Entity);

/// Marks the resume button so its click can be routed.
#[derive(Component, Clone, Copy)]
pub struct ResumeButton;

/// Whether the match is paused (drives [`Time::paused`]).
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct PauseState {
    /// Paused.
    pub paused: bool,
}

/// Installs the HUD, pause menu and their systems.
pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<UiEvent>()
            .init_resource::<PauseState>()
            .add_systems(Startup, spawn_hud)
            .add_systems(
                Update,
                (
                    update_hud_text,
                    toggle_pause,
                    handle_menu,
                    follow_engine_pause,
                    apply_pause,
                )
                    .chain()
                    .after(GamePauseSet),
            )
            .add_systems(Update, bridge_touch_controls.after(regia_ui::widgets::ui_widgets));
    }
}

/// `Startup`: builds the HUD (score, timer, phase, crosshair) and the (hidden) pause menu.
fn spawn_hud(mut commands: Commands) {
    let root = commands
        .spawn((
            UiRoot {
                scale_mode: regia_ui::ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                padding: Edges::px(16.0),
                justify_content: JustifyContent::SpaceBetween,
                ..Default::default()
            }),
        ))
        .id();

    // Top readout on a dark panel so the white glyphs survive a bright frame.
    let top = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Points(360.0), Dimension::Auto],
                gap: [Dimension::Points(4.0), Dimension::Points(4.0)],
                padding: Edges::px(10.0),
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor::linear(0.05, 0.06, 0.08, 0.92)),
            },
        ))
        .id();
    commands.entity(root).add_child(top);
    spawn_text(&mut commands, top, "Score: 0", HudText::Score, 24.0);
    spawn_text(&mut commands, top, "Time: 0:00", HudText::Timer, 18.0);
    spawn_text(&mut commands, top, "Warm-up", HudText::Phase, 18.0);

    // Centre crosshair.
    let cross = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Auto, Dimension::Auto],
                align_self: Align::Center,
                ..Default::default()
            }),
            Text {
                spans: vec![TextSpan::lit("+").with_style(TextStyle {
                    font_px: 28.0,
                    ..Default::default()
                })],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    commands.entity(root).add_child(cross);

    // Touch controls: a stick (Move) on the left, a fire button on the right.
    let controls = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                size: [Dimension::Percent(100.0), Dimension::Points(140.0)],
                justify_content: JustifyContent::SpaceBetween,
                align_items: Align::End,
                ..Default::default()
            }),
        ))
        .id();
    commands.entity(root).add_child(controls);
    let mut stick_widget = VirtualStick::default();
    stick_widget.action = "Move".to_string();
    stick_widget.radius = 56.0;
    let stick = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(128.0), Dimension::Points(128.0)],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor::linear(0.12, 0.14, 0.18, 0.55)),
            },
            stick_widget,
        ))
        .id();
    commands.entity(controls).add_child(stick);
    let fire = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(88.0), Dimension::Points(88.0)],
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor::linear(0.75, 0.2, 0.14, 0.9)),
            },
            Interactable::default(),
            VirtualButton {
                action: "Fire".to_string(),
            },
        ))
        .id();
    commands.entity(controls).add_child(fire);
    let fire_label = commands
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit("Fire").with_style(TextStyle {
                    font_px: 18.0,
                    color: UiColor::WHITE,
                    ..Default::default()
                })],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    commands.entity(fire).add_child(fire_label);

    // Pause menu (hidden until paused): a column with the resume button.
    let menu = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                display: Display::None,
                size: [Dimension::Auto, Dimension::Auto],
                align_self: Align::Center,
                justify_content: JustifyContent::Center,
                gap: [Dimension::Points(12.0), Dimension::Points(12.0)],
                padding: Edges::px(24.0),
                ..Default::default()
            }),
        ))
        .id();
    commands.entity(root).add_child(menu);
    let title = commands
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit("PAUSED").with_style(TextStyle {
                    font_px: 32.0,
                    ..Default::default()
                })],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    commands.entity(menu).add_child(title);
    let resume = spawn_button(&mut commands, menu, "Resume", &[]);
    // The engine pause resumes on this command (ADR-10104); the sample's own path reads the click.
    commands.entity(resume).insert((
        ResumeButton,
        regia_ui::viewmodel::UiCommandName(RESUME_COMMAND.to_owned()),
    ));
    commands.insert_resource(PauseMenu(menu));
}

/// Spawns a HUD text node under `parent`.
fn spawn_text(commands: &mut Commands, parent: Entity, label: &str, kind: HudText, font_px: f32) {
    let node = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Auto, Dimension::Auto],
                ..Default::default()
            }),
            kind,
            Text {
                spans: vec![TextSpan::lit(label).with_style(TextStyle {
                    font_px,
                    color: UiColor::WHITE,
                    ..Default::default()
                })],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    commands.entity(parent).add_child(node);
}

/// `Update`: refreshes the score, timer and phase lines from [`ArenaState`] and [`PauseState`].
fn update_hud_text(
    state: Option<Single<&ArenaState>>,
    pause: Res<PauseState>,
    mut texts: Query<(&HudText, &mut Text)>,
) {
    for (kind, mut text) in &mut texts {
        let line = match kind {
            HudText::Score => {
                let Some(state) = state.as_ref() else {
                    continue;
                };
                let leader = state.leader().map(|l| l as i32).unwrap_or(-1);
                format!(
                    "You: {}  Leader: {} ({})",
                    state.score(0),
                    state.leading_score(),
                    leader
                )
            }
            HudText::Timer => {
                let Some(state) = state.as_ref() else {
                    continue;
                };
                let secs = (state.match_time_left().max(0.0)) as u32;
                format!("Time: {}:{:02}", secs / 60, secs % 60)
            }
            HudText::Phase => {
                if pause.paused {
                    "PAUSED".to_string()
                } else {
                    match state.as_ref().map(|s| s.phase).unwrap_or(Phase::Warmup) {
                        Phase::Warmup => "Warm-up".to_string(),
                        Phase::Round => "Round".to_string(),
                        Phase::MatchOver => "Match over".to_string(),
                    }
                }
            }
        };
        let font_px = text.spans.first().map(|span| span.style.font_px).unwrap_or(18.0);
        text.spans = vec![TextSpan::lit(line).with_style(TextStyle {
            font_px,
            color: UiColor::WHITE,
            ..Default::default()
        })];
    }
}

/// `Update`: copies the virtual stick and fire button into [`regia_input::InputInjection`].
///
/// `regia_ui` writes [`regia_ui::VirtualAxes`] / [`regia_ui::VirtualButtons`] but the input
/// crate does not read them. Holding the action here makes the next frame's `evaluate_actions`
/// (PreUpdate) see the touch. A near-zero stick releases `Move` so a physical binding can win.
#[tracing::instrument(skip_all, target = "arena", name = "touch_bridge")]
fn bridge_touch_controls(
    axes: Option<Res<regia_ui::VirtualAxes>>,
    buttons: Option<Res<regia_ui::VirtualButtons>>,
    mut injection: Option<ResMut<regia_input::InputInjection>>,
) {
    let Some(injection) = injection.as_mut() else {
        return;
    };
    if let Some(axes) = axes.as_ref()
        && let Some(value) = axes.0.get("Move")
    {
        if value[0].abs() > 1.0e-3 || value[1].abs() > 1.0e-3 {
            injection.hold("Move", Vec3::new(value[0], value[1], 0.0));
        } else {
            injection.release("Move");
        }
    }
    if let Some(buttons) = buttons.as_ref()
        && let Some(pressed) = buttons.0.get("Fire")
    {
        if *pressed {
            injection.hold("Fire", Vec3::X);
        } else {
            injection.release("Fire");
        }
    }
}

/// Whether the engine pause is on for this game (it then owns Escape, Start and `Time::paused`).
fn engine_pause_on(engine: Option<&GamePause>) -> bool {
    engine.is_some_and(GamePause::is_enabled)
}

/// `Update`: toggles pause on Escape (keyboard) or Start (gamepad), unless the engine pause owns
/// them.
fn toggle_pause(
    mut keys: MessageReader<KeyboardInput>,
    gamepads: Option<Res<Gamepads>>,
    mut pause: ResMut<PauseState>,
    engine: Option<Res<GamePause>>,
) {
    if engine_pause_on(engine.as_deref()) {
        keys.clear();
        return;
    }
    let escape = keys
        .read()
        .any(|k| k.state == regia_platform::ButtonState::Pressed && k.physical_key.as_str() == "Escape");
    let start = gamepads
        .as_ref()
        .is_some_and(|pads| pads.pads().any(|(_, pad)| pad.just_pressed(GamepadButton::Start)));
    if escape || start {
        pause.paused = !pause.paused;
    }
}

/// `Update`: with the engine pause on, [`PauseState`] mirrors it.
fn follow_engine_pause(engine: Option<Res<GamePause>>, mut pause: ResMut<PauseState>) {
    if let Some(engine) = engine.as_deref()
        && engine.is_enabled()
        && pause.paused != engine.is_paused()
    {
        pause.paused = engine.is_paused();
    }
}

/// `Update`: shows/hides the pause menu to match [`PauseState`] (and, without the engine pause,
/// drives `Time::paused`).
fn apply_pause(
    pause: Res<PauseState>,
    menu: Option<Res<PauseMenu>>,
    mut nodes: Query<&mut NodeStyle>,
    mut time: ResMut<Time>,
    engine: Option<Res<GamePause>>,
) {
    if !engine_pause_on(engine.as_deref()) {
        time.paused = pause.paused;
    }
    let Some(menu) = menu else {
        return;
    };
    if let Ok(mut style) = nodes.get_mut(menu.0) {
        style.0.display = if pause.paused {
            Display::Flex
        } else {
            Display::None
        };
    }
}

/// `Update`: a click on the resume button unpauses. Other clicks (the fire button) do not. With the
/// engine pause on, the button's `pause.resume` command resumes instead.
fn handle_menu(
    mut events: MessageReader<UiEvent>,
    resume: Query<Entity, With<ResumeButton>>,
    mut pause: ResMut<PauseState>,
    engine: Option<Res<GamePause>>,
) {
    if engine_pause_on(engine.as_deref()) {
        events.clear();
        return;
    }
    let resume = resume.iter().next();
    for event in events.read() {
        if event.kind == UiEventKind::Click && Some(event.target) == resume {
            pause.paused = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_platform::{ButtonState, GamepadButtonChanged, TouchInput, TouchPhase};
    use regia_ui::node::ComputedNode;

    fn register_window_messages(app: &mut App) {
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::ImeEvent>();
    }

    fn hud_app() -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            regia_input::InputPlugin::new(
                regia_input::InputConfig::new()
                    .action("Move", regia_input::ActionKind::Axis2D)
                    .action("Fire", regia_input::ActionKind::Button),
            ),
            regia_ui::UiPlugin::code_only(),
            HudPlugin,
        ));
        register_window_messages(&mut app);
        app
    }

    #[test]
    fn pause_state_defaults_to_unpaused() {
        let p = PauseState::default();
        assert!(!p.paused);
    }

    #[test]
    fn start_opens_the_pause_menu_and_resume_closes_it() {
        let mut app = hud_app();
        app.update();
        let menu = app.world().resource::<PauseMenu>().0;
        assert_eq!(
            app.world().get::<NodeStyle>(menu).expect("menu").0.display,
            Display::None
        );

        app.world_mut().write_message(GamepadButtonChanged {
            device: 1,
            button: GamepadButton::Start,
            state: ButtonState::Pressed,
            value: 1.0,
        });
        app.update();
        assert!(app.world().resource::<PauseState>().paused);
        assert_eq!(
            app.world().get::<NodeStyle>(menu).expect("menu").0.display,
            Display::Flex,
            "Start opens the pause menu"
        );

        let resume = {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<ResumeButton>>();
            query.iter(world).next().expect("resume button")
        };
        app.world_mut()
            .write_message(UiEvent::new(resume, UiEventKind::Click));
        app.update();
        assert!(!app.world().resource::<PauseState>().paused);
        assert_eq!(
            app.world().get::<NodeStyle>(menu).expect("menu").0.display,
            Display::None,
            "Resume closes the pause menu"
        );
    }

    /// The sample as shipped (ADR-10104): the engine pause owns Escape and Start, freezes time, and this
    /// menu is its screen; the Resume button's command resumes.
    #[test]
    fn the_engine_pause_drives_the_arena_menu() {
        let mut app = hud_app();
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Regia.toml"))
            .expect("the sample's Regia.toml");
        let config =
            regia_framework::game_pause::GamePauseConfig::from_manifest(&manifest).expect("[game] parses");
        assert!(config.enabled, "the sample turns the engine pause on");
        assert_eq!(config.screen, regia_framework::game_pause::PauseScreen::None);
        app.add_plugins(regia_framework::game_pause::GamePausePlugin {
            config: Some(config),
            ..Default::default()
        });
        app.update();
        let menu = app.world().resource::<PauseMenu>().0;
        app.world_mut().write_message(GamepadButtonChanged {
            device: 1,
            button: GamepadButton::Start,
            state: ButtonState::Pressed,
            value: 1.0,
        });
        app.update();
        app.update();
        assert!(
            app.world().resource::<GamePause>().is_paused(),
            "Start pauses through TogglePause"
        );
        assert!(app.world().resource::<Time>().paused, "time is frozen");
        assert!(app.world().resource::<PauseState>().paused);
        assert_eq!(
            app.world().get::<NodeStyle>(menu).expect("menu").0.display,
            Display::Flex,
            "the sample's menu is the pause screen"
        );
        assert_eq!(
            app.world().resource::<GamePause>().screen_shown(),
            None,
            "no built-in screen"
        );
        // The sample's own toggle no longer writes Time: a frame later it is still paused.
        app.update();
        assert!(app.world().resource::<Time>().paused);
        let resume = {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<ResumeButton>>();
            query.iter(world).next().expect("resume button")
        };
        app.world_mut()
            .write_message(UiEvent::new(resume, UiEventKind::Click));
        app.update();
        app.update();
        assert!(!app.world().resource::<GamePause>().is_paused(), "Resume resumes");
        assert!(!app.world().resource::<Time>().paused);
        assert_eq!(
            app.world().get::<NodeStyle>(menu).expect("menu").0.display,
            Display::None
        );
    }

    #[test]
    fn touch_stick_moves_and_touch_fire_shoots() {
        let mut app = hud_app();
        app.update();
        app.update();

        let stick = {
            let world = app.world_mut();
            let mut sticks = world.query::<(&VirtualStick, &ComputedNode)>();
            let (_, node) = sticks.iter(world).next().expect("stick laid out");
            assert!(node.rect[2] > 16.0, "stick has a layout rect {:?}", node.rect);
            node.rect
        };
        let fire = {
            let world = app.world_mut();
            let mut buttons = world.query::<(&VirtualButton, &ComputedNode)>();
            let (_, node) = buttons.iter(world).next().expect("fire button laid out");
            assert!(
                node.rect[2] > 16.0,
                "fire button has a layout rect {:?}",
                node.rect
            );
            node.rect
        };

        // Finger down, off the stick's centre, so the deflection is non-zero on the next frame
        // (the widget latches the touch id first, then samples it).
        let touch_x = f64::from(stick[0] + stick[2] * 0.5 + 36.0);
        let touch_y = f64::from(stick[1] + stick[3] * 0.5);
        app.world_mut().write_message(TouchInput {
            window: Entity::PLACEHOLDER,
            id: 1,
            phase: TouchPhase::Started,
            position: (touch_x, touch_y),
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        // Frame 1 latches the touch, frame 2 samples the deflection and holds Move,
        // frame 3's PreUpdate evaluation writes Actions.
        let mut move_axis = regia_core::math::Vec2::ZERO;
        for _ in 0..4 {
            app.update();
            move_axis = app.world().resource::<regia_input::Actions>().axis2d("Move");
            if move_axis.length() > 0.2 {
                break;
            }
        }
        let stick_value = {
            let world = app.world_mut();
            let mut sticks = world.query::<&VirtualStick>();
            sticks
                .iter(world)
                .next()
                .map(|stick| stick.value)
                .unwrap_or([0.0, 0.0])
        };
        assert!(
            move_axis.length() > 0.2,
            "touch stick drives Move, got {move_axis:?} stick {stick_value:?} touch ({touch_x}, {touch_y}) rect {stick:?}"
        );

        // Lift the stick finger, then press the fire button with a new touch.
        app.world_mut().write_message(TouchInput {
            window: Entity::PLACEHOLDER,
            id: 1,
            phase: TouchPhase::Ended,
            position: (touch_x, touch_y),
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.update();
        let fire_x = f64::from(fire[0] + fire[2] * 0.5);
        let fire_y = f64::from(fire[1] + fire[3] * 0.5);
        app.world_mut().write_message(TouchInput {
            window: Entity::PLACEHOLDER,
            id: 2,
            phase: TouchPhase::Started,
            position: (fire_x, fire_y),
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        let mut fired = false;
        for _ in 0..4 {
            app.update();
            if app.world().resource::<regia_input::Actions>().pressed("Fire") {
                fired = true;
                break;
            }
        }
        assert!(fired, "touch on the fire button presses Fire");
    }
}
