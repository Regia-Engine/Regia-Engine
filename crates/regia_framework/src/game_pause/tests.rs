// SPDX-License-Identifier: MPL-2.0
//! Tests of the game pause (ADR-10104). Headless: `InputPlugin`, `UiPlugin::code_only` and a fixed
//! step of 60 Hz, driven frame by frame with key messages.

use regia_app::{FixedTime, enable_fixed_step};
use regia_input::{ActionKind, InputConfig, InputPlugin};
use regia_platform::{ButtonState, KeyboardInput};
use regia_ui::a11y::{A11yRole, Accessible};
use regia_ui::text::Text;

use super::*;

/// Fixed steps run so far: the stand-in for physics, animation and every other gameplay system.
#[derive(Resource, Default)]
struct Steps(u32);

/// Frames on which the gameplay action `Jump` read as pressed.
#[derive(Resource, Default)]
struct Jumps(u32);

/// The game's input. Jump is on J, not Space: Space and Enter activate the focused UI button, and the
/// Paused screen focuses Resume, so Space would resume the game (that is the screen working).
fn game_input() -> InputConfig {
    InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("KeyJ")))
}

fn app_with(config: GamePauseConfig, input: InputConfig) -> App {
    let mut app = App::new();
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(regia_ui::UiPlugin::code_only())
        .add_plugins(InputPlugin::new(input))
        .add_plugins(GamePausePlugin {
            config: Some(config),
            ..Default::default()
        });
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::ImeEvent>();
    app.init_resource::<CursorRelease>();
    app.init_resource::<Steps>().init_resource::<Jumps>();
    app.add_systems(FixedUpdate, |mut s: ResMut<Steps>| s.0 += 1);
    app.add_systems(Update, |actions: Res<Actions>, mut j: ResMut<Jumps>| {
        if actions.pressed("Jump") {
            j.0 += 1;
        }
    });
    app
}

fn app(config: GamePauseConfig) -> App {
    app_with(config, game_input())
}

fn key(app: &mut App, name: &str, state: ButtonState) {
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: name.to_owned(),
        logical_key: name.to_owned(),
        state,
        repeat: false,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

/// Presses and releases `name` over two frames.
fn tap(app: &mut App, name: &str) {
    key(app, name, ButtonState::Pressed);
    app.update();
    key(app, name, ButtonState::Released);
    app.update();
}

fn span_text(span: &regia_ui::text::TextSpan) -> String {
    match &span.content {
        regia_ui::text::TextContent::Lit(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

fn paused(app: &App) -> bool {
    app.world().resource::<GamePause>().is_paused()
}

fn screen_roots(app: &mut App) -> usize {
    app.world_mut()
        .query_filtered::<Entity, With<PauseScreenRoot>>()
        .iter(app.world())
        .count()
}

fn messages<M: Message + Clone>(app: &mut App) -> Vec<M> {
    let messages = app.world().resource::<Messages<M>>();
    messages.iter_current_update_messages().cloned().collect()
}

#[test]
fn the_pause_key_freezes_time_and_resumes() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    app.update();
    let steps = app.world().resource::<Steps>().0;
    assert!(steps >= 1, "the fixed loop runs before the pause");
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(paused(&app));
    assert!(app.world().resource::<Time>().paused);
    assert_eq!(
        app.world().resource::<GamePause>().source(),
        Some(PauseSource::Input)
    );
    // Frozen: no fixed step, no gameplay time, however many frames pass.
    let (steps, elapsed) = (
        app.world().resource::<Steps>().0,
        app.world().resource::<Time>().elapsed(),
    );
    for _ in 0..30 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<Steps>().0,
        steps,
        "no fixed step while paused"
    );
    assert_eq!(
        app.world().resource::<Time>().elapsed(),
        elapsed,
        "gameplay time stood still"
    );
    assert!(app.world().resource::<Time>().delta().is_zero());
    // The same key resumes.
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(!paused(&app));
    assert!(!app.world().resource::<Time>().paused);
    app.update();
    app.update();
    assert!(
        app.world().resource::<Steps>().0 > steps,
        "the fixed loop runs again"
    );
    assert_eq!(app.world().resource::<GamePause>().counts(), (1, 1, 0));
}

#[test]
fn pause_and_resume_send_messages() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    key(&mut app, DEFAULT_PAUSE_KEY, ButtonState::Pressed);
    app.update();
    assert_eq!(
        messages::<GamePaused>(&mut app),
        vec![GamePaused {
            source: PauseSource::Input
        }]
    );
    key(&mut app, DEFAULT_PAUSE_KEY, ButtonState::Released);
    app.update();
    app.world_mut().resource_mut::<GamePause>().resume();
    app.update();
    assert_eq!(
        messages::<GameResumed>(&mut app),
        vec![GameResumed {
            source: PauseSource::Code
        }]
    );
}

#[test]
fn off_by_default_the_key_does_nothing() {
    let mut app = app(GamePauseConfig::default());
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(!paused(&app));
    assert!(!app.world().resource::<Time>().paused);
    // Not even bound: the action is not added to a game that did not turn the pause on.
    let input = app.world().resource::<InputConfig>();
    assert!(!input.contexts.contains_key(PAUSE_CONTEXT));
    // Code is refused too, with the reason.
    assert_eq!(
        pause_now(app.world_mut(), PauseSource::Code),
        Err(PauseRefusal::Disabled)
    );
}

#[test]
fn gameplay_input_is_ignored_while_paused() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    key(&mut app, "KeyJ", ButtonState::Pressed);
    app.update();
    app.update();
    let held = app.world().resource::<Jumps>().0;
    assert!(held >= 1, "Jump reads while running");
    key(&mut app, "KeyJ", ButtonState::Released);
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(paused(&app));
    let before = app.world().resource::<Jumps>().0;
    key(&mut app, "KeyJ", ButtonState::Pressed);
    let mut trace = Vec::new();
    for _ in 0..5 {
        app.update();
        let jump = app
            .world()
            .resource::<Actions>()
            .player(0)
            .states
            .get("Jump")
            .copied();
        trace.push(format!(
            "paused {} contexts {:?} jump {:?} jumps {}",
            paused(&app),
            app.world().resource::<InputContexts>().enabled(0),
            jump.map(|s| (s.phase, s.value, s.actuated())),
            app.world().resource::<Jumps>().0
        ));
    }
    assert_eq!(
        app.world().resource::<Jumps>().0,
        before,
        "a gameplay action stays idle while paused: {trace:#?}"
    );
    assert!(!app.world().resource::<Actions>().pressed("Jump"));
    // Only the pause context is enabled; gameplay comes back on resume.
    let contexts = app.world().resource::<InputContexts>();
    assert_eq!(contexts.enabled(0), [PAUSE_CONTEXT.to_owned()]);
    key(&mut app, "KeyJ", ButtonState::Released);
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(!paused(&app));
    let contexts = app.world().resource::<InputContexts>();
    assert!(contexts.is_enabled(0, regia_input::GAMEPLAY_CONTEXT));
    assert!(contexts.is_enabled(0, PAUSE_CONTEXT));
    key(&mut app, "KeyJ", ButtonState::Pressed);
    app.update();
    app.update();
    assert!(
        app.world().resource::<Jumps>().0 > before,
        "Jump reads again after resume"
    );
}

#[test]
fn kept_contexts_stay_live_while_paused() {
    let mut config = GamePauseConfig::enabled();
    config.keep_contexts = vec!["Menu".to_owned()];
    let input = game_input()
        .action("MenuBack", ActionKind::Button)
        .context("Menu", InputContext::new(10))
        .binding_in("Menu", Binding::new("MenuBack", InputSource::key("Backspace")));
    let mut app = app_with(config, input);
    app.world_mut().resource_mut::<InputContexts>().push(0, "Menu");
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(paused(&app));
    let contexts = app.world().resource::<InputContexts>();
    assert!(contexts.is_enabled(0, "Menu"));
    assert!(!contexts.is_enabled(0, regia_input::GAMEPLAY_CONTEXT));
    key(&mut app, "Backspace", ButtonState::Pressed);
    app.update();
    assert!(app.world().resource::<Actions>().pressed("MenuBack"));
}

#[test]
fn the_cursor_is_released_and_given_back() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    assert!(!app.world().resource::<CursorRelease>().0);
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(
        app.world().resource::<CursorRelease>().0,
        "free, visible cursor while paused"
    );
    // An overlay that clears the release while paused does not take the cursor back.
    app.world_mut().resource_mut::<CursorRelease>().0 = false;
    app.update();
    assert!(app.world().resource::<CursorRelease>().0);
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(
        !app.world().resource::<CursorRelease>().0,
        "the game's grab applies again"
    );
}

#[test]
fn a_release_someone_else_holds_is_left_alone() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    app.world_mut().resource_mut::<CursorRelease>().0 = true;
    tap(&mut app, DEFAULT_PAUSE_KEY);
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(!paused(&app));
    assert!(
        app.world().resource::<CursorRelease>().0,
        "the overlay's release stays"
    );
}

#[test]
fn an_earlier_time_pause_survives_the_resume() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    // The agent's play.pause (or the focus-loss pause) froze time first.
    app.world_mut().resource_mut::<Time>().paused = true;
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(paused(&app));
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(!paused(&app));
    assert!(
        app.world().resource::<Time>().paused,
        "only the game pause's own freeze is lifted"
    );
}

#[test]
fn the_default_screen_shows_and_hides() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    assert_eq!(screen_roots(&mut app), 0);
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert_eq!(screen_roots(&mut app), 1);
    let pause = app.world().resource::<GamePause>();
    assert_eq!(pause.screen_shown(), Some("default"));
    let root = pause.screen_root().expect("root");
    // A dialog for screen readers, named "Paused".
    let a11y = app.world().get::<Accessible>(root).expect("accessible");
    assert_eq!(a11y.role, Some(A11yRole::Dialog));
    assert_eq!(a11y.label.as_deref(), Some("Paused"));
    // On a UI-only input layer.
    assert_eq!(
        app.world().get::<InputLayer>(root).map(|l| l.mode),
        Some(LayerInputMode::UiOnly)
    );
    // The hint names the bound key.
    let hint = app
        .world_mut()
        .query_filtered::<&Text, With<PauseScreenHint>>()
        .single(app.world())
        .expect("one hint")
        .spans
        .iter()
        .map(span_text)
        .collect::<String>();
    assert_eq!(hint, "Press Pause to resume");
    // Resume and Quit-to-menu buttons with their commands.
    let mut commands: Vec<String> = app
        .world_mut()
        .query::<&UiCommandName>()
        .iter(app.world())
        .map(|c| c.0.clone())
        .collect();
    commands.sort();
    assert_eq!(commands, [QUIT_TO_MENU_COMMAND, RESUME_COMMAND]);
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert_eq!(screen_roots(&mut app), 0, "the screen goes with the pause");
    assert_eq!(app.world().resource::<GamePause>().screen_shown(), None);
}

/// Sends a UI command as a click on a node with that command would.
fn command(app: &mut App, name: &str) {
    let node = app
        .world_mut()
        .query::<(Entity, &UiCommandName)>()
        .iter(app.world())
        .find(|(_, c)| c.0 == name)
        .map(|(e, _)| e)
        .expect("a node with the command");
    app.world_mut().write_message(UiCommand {
        name: name.to_owned(),
        node,
        value: regia_ui::interaction::UiValue::None,
        item: None,
    });
    app.update();
}

#[test]
fn screen_buttons_resume_and_quit_to_menu() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    command(&mut app, RESUME_COMMAND);
    assert!(!paused(&app));
    assert_eq!(
        messages::<PauseScreenEvent>(&mut app),
        vec![PauseScreenEvent::Resume]
    );
    assert_eq!(screen_roots(&mut app), 0);
    tap(&mut app, DEFAULT_PAUSE_KEY);
    command(&mut app, QUIT_TO_MENU_COMMAND);
    assert!(!paused(&app), "quitting ends the pause so the menu level runs");
    assert_eq!(
        messages::<PauseScreenEvent>(&mut app),
        vec![PauseScreenEvent::QuitToMenu]
    );
}

#[test]
fn a_custom_screen_replaces_the_default() {
    let mut config = GamePauseConfig::enabled();
    config.screen = PauseScreen::custom(|world: &mut World| {
        world
            .spawn((UiRoot::default(), UiNode, Name::new("my-pause")))
            .id()
    });
    let mut app = app(config);
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert_eq!(app.world().resource::<GamePause>().screen_shown(), Some("custom"));
    let root = app.world().resource::<GamePause>().screen_root().expect("root");
    assert_eq!(
        app.world().get::<Name>(root).map(|n| n.as_str().to_owned()),
        Some("my-pause".to_owned())
    );
    assert_eq!(screen_roots(&mut app), 1, "only the game's screen shows");
    assert!(
        app.world_mut()
            .query::<&PauseScreenHint>()
            .iter(app.world())
            .next()
            .is_none()
    );
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(
        app.world().get_entity(root).is_err(),
        "the game's screen is despawned on resume"
    );
}

#[test]
fn screen_none_shows_nothing() {
    let mut config = GamePauseConfig::enabled();
    config.screen = PauseScreen::None;
    let mut app = app(config);
    app.update();
    let count = |app: &mut App| app.world_mut().query::<Entity>().iter(app.world()).count();
    let entities = count(&mut app);
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(paused(&app));
    assert_eq!(screen_roots(&mut app), 0);
    assert_eq!(app.world().resource::<GamePause>().screen_shown(), None);
    assert_eq!(count(&mut app), entities, "nothing spawned");
}

#[test]
fn an_asset_screen_spawns_its_template_and_a_missing_one_falls_back() {
    let mut config = GamePauseConfig::enabled();
    config.screen = PauseScreen::parse("ui/pause_menu.ui.ron");
    let mut app = app(config);
    app.update();
    // Not loaded yet: the built-in screen shows, with the reason recorded.
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert_eq!(
        app.world().resource::<GamePause>().screen_shown(),
        Some("default")
    );
    assert!(
        app.world()
            .resource::<GamePause>()
            .screen_error()
            .is_some_and(|e| e.starts_with("pause_screen_missing"))
    );
    tap(&mut app, DEFAULT_PAUSE_KEY);
    // Loaded: the template spawns, and its command button resumes.
    let file = regia_ui::authoring::parse_ui_ron(
        r#"(
            version: 11,
            root: (
                name: "pause-menu",
                children: [
                    (
                        name: "back",
                        widget: button,
                        interactable: true,
                        command: "pause.resume",
                        text: (spans: ["Back to the game"]),
                    ),
                ],
            ),
        )"#,
        "ui/pause_menu.ui.ron",
        &Default::default(),
    )
    .expect("template parses");
    let diagnostics =
        regia_ui::authoring::insert_ui_template(app.world_mut(), "pause_menu", "ui/pause_menu.ui.ron", file);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert_eq!(app.world().resource::<GamePause>().screen_shown(), Some("asset"));
    assert_eq!(app.world().resource::<GamePause>().screen_error(), None);
    assert!(
        app.world_mut()
            .query::<&PauseScreenHint>()
            .iter(app.world())
            .next()
            .is_none()
    );
    command(&mut app, RESUME_COMMAND);
    assert!(!paused(&app));
    assert_eq!(screen_roots(&mut app), 0);
}

#[test]
fn a_screen_set_while_paused_replaces_the_one_showing() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert_eq!(
        app.world().resource::<GamePause>().screen_shown(),
        Some("default")
    );
    app.world_mut()
        .resource_mut::<GamePause>()
        .set_screen(PauseScreen::None);
    app.update();
    assert_eq!(screen_roots(&mut app), 0);
}

#[test]
fn a_networked_session_refuses_the_pause_with_the_reason() {
    let mut app = app(GamePauseConfig::enabled());
    app.insert_resource(NetworkedSession(true));
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(!paused(&app));
    assert!(!app.world().resource::<Time>().paused, "nobody's world froze");
    assert_eq!(screen_roots(&mut app), 0);
    let pause = app.world().resource::<GamePause>();
    assert_eq!(pause.last_refusal(), Some(PauseRefusal::NetworkedSession));
    assert_eq!(pause.counts(), (0, 0, 1));
    assert_eq!(PauseRefusal::NetworkedSession.code(), "networked_session");
    assert_eq!(
        pause_now(app.world_mut(), PauseSource::Agent),
        Err(PauseRefusal::NetworkedSession)
    );
    assert_eq!(
        messages::<PauseRefused>(&mut app).last(),
        Some(&PauseRefused {
            source: PauseSource::Agent,
            reason: PauseRefusal::NetworkedSession
        })
    );
}

#[cfg(feature = "net")]
#[test]
fn a_regia_net_session_refuses_the_pause() {
    let mut app = app(GamePauseConfig::enabled());
    app.insert_resource(regia_net::NetMode::Client);
    app.update();
    assert_eq!(pause_refusal(app.world()), Some(PauseRefusal::NetworkedSession));
    app.insert_resource(regia_net::NetMode::Offline);
    assert_eq!(pause_refusal(app.world()), None);
}

#[test]
fn a_rebinding_moves_the_pause_and_the_hint() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    // The player rebinds TogglePause to P (what the rebinding UI writes).
    {
        let mut input = app.world_mut().resource_mut::<InputConfig>();
        let context = input.contexts.get_mut(PAUSE_CONTEXT).expect("pause context");
        context.bindings = vec![Binding::new(TOGGLE_PAUSE_ACTION, InputSource::key("KeyP"))];
    }
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(!paused(&app), "the old key no longer pauses");
    tap(&mut app, "KeyP");
    assert!(paused(&app));
    let hint = app
        .world_mut()
        .query_filtered::<&Text, With<PauseScreenHint>>()
        .single(app.world())
        .expect("one hint")
        .spans
        .iter()
        .map(span_text)
        .collect::<String>();
    assert_eq!(hint, "Press P to resume");
}

#[test]
fn a_project_binding_wins_and_a_gamepad_button_is_opt_in() {
    let input = game_input()
        .action(TOGGLE_PAUSE_ACTION, ActionKind::Button)
        .binding(Binding::new(TOGGLE_PAUSE_ACTION, InputSource::key("Escape")));
    let mut app = app_with(GamePauseConfig::enabled(), input);
    app.update();
    let config = app.world().resource::<InputConfig>();
    assert!(
        !config.contexts.contains_key(PAUSE_CONTEXT),
        "no default added over the project's"
    );
    tap(&mut app, "Escape");
    assert!(paused(&app));

    let mut input = InputConfig::new();
    assert!(add_pause_bindings(&mut input, &GamePauseConfig::enabled()));
    let bindings = &input.contexts[PAUSE_CONTEXT].bindings;
    assert_eq!(bindings.len(), 1, "no gamepad button by default");
    let mut with_pad = GamePauseConfig::enabled();
    with_pad.gamepad = Some(GamepadButton::Start);
    let mut input = InputConfig::new();
    add_pause_bindings(&mut input, &with_pad);
    assert!(
        input.contexts[PAUSE_CONTEXT]
            .bindings
            .iter()
            .any(|b| b.input == InputSource::gamepad_button(GamepadButton::Start))
    );
    assert!(input.contexts[PAUSE_CONTEXT].consume_lower);
}

#[test]
fn a_saved_rebinding_of_the_pause_survives_a_restart() {
    let mut overrides = InputOverrides::default();
    overrides.contexts.insert(
        PAUSE_CONTEXT.to_owned(),
        [(
            TOGGLE_PAUSE_ACTION.to_owned(),
            Binding::new(TOGGLE_PAUSE_ACTION, InputSource::key("F10")),
        )]
        .into_iter()
        .collect(),
    );
    let mut app = app(GamePauseConfig::enabled());
    app.insert_resource(overrides);
    app.update();
    tap(&mut app, "F10");
    assert!(paused(&app));
    // Reset-to-defaults knows the default key.
    let defaults = &app.world().resource::<InputDefaults>().0;
    assert!(
        defaults.contexts[PAUSE_CONTEXT]
            .bindings
            .iter()
            .any(|b| b.input == InputSource::key(DEFAULT_PAUSE_KEY))
    );
}

#[test]
fn code_requests_and_turning_the_pause_off() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    app.world_mut().resource_mut::<GamePause>().toggle();
    app.update();
    assert!(paused(&app));
    assert_eq!(
        app.world().resource::<GamePause>().source(),
        Some(PauseSource::Code)
    );
    app.world_mut().resource_mut::<GamePause>().set_enabled(false);
    app.update();
    assert!(!paused(&app), "turning the pause off resumes");
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(!paused(&app), "and the key no longer pauses");
}

#[test]
fn run_conditions_follow_the_pause() {
    #[derive(Resource, Default)]
    struct Counts(u32, u32);
    let mut app = app(GamePauseConfig::enabled());
    app.init_resource::<Counts>();
    app.add_systems(Update, (|mut c: ResMut<Counts>| c.0 += 1).run_if(game_running));
    app.add_systems(Update, (|mut c: ResMut<Counts>| c.1 += 1).run_if(game_paused));
    app.update();
    assert_eq!(pause_now(app.world_mut(), PauseSource::Agent), Ok(true));
    assert_eq!(
        pause_now(app.world_mut(), PauseSource::Agent),
        Ok(false),
        "already paused"
    );
    app.update();
    app.update();
    let counts = app.world().resource::<Counts>();
    assert_eq!((counts.0, counts.1), (1, 2));
    assert!(resume_now(app.world_mut(), PauseSource::Agent));
    assert!(!resume_now(app.world_mut(), PauseSource::Agent));
}

#[test]
fn settings_round_trip() {
    let text = "[project]\nname = \"g\"\n\n[game]\npause = true\npause_key = \"F12\"\npause_gamepad = \"Start\"\npause_screen = \"ui/pause.ui.ron\"\n";
    let config = GamePauseConfig::from_manifest(text).expect("parses");
    assert!(config.enabled);
    assert_eq!(config.key.as_deref(), Some("F12"));
    assert_eq!(config.gamepad, Some(GamepadButton::Start));
    assert_eq!(config.screen, PauseScreen::Asset("ui/pause.ui.ron".to_owned()));
    assert_eq!(
        GamePauseConfig::from_manifest(&config.to_toml()),
        Ok(config.clone())
    );
    // Defaults: off, the Pause key, no gamepad button, the built-in screen.
    let none = GamePauseConfig::from_manifest("[project]\nname = \"g\"\n").expect("parses");
    assert_eq!(none, GamePauseConfig::default());
    assert!(!none.enabled);
    assert_eq!(none.key.as_deref(), Some(DEFAULT_PAUSE_KEY));
    assert_eq!(none.gamepad, None);
    assert_eq!(none.screen, PauseScreen::Default);
    assert_eq!(GamePauseConfig::from_manifest(&none.to_toml()), Ok(none));
    // `none` values and every screen kind round trip.
    let off = GamePauseConfig::from_manifest(
        "[game]\npause = true\npause_key = \"none\"\npause_gamepad = \"none\"\npause_screen = \"none\"\n",
    )
    .expect("parses");
    assert_eq!(off.key, None);
    assert_eq!(off.gamepad, None);
    assert_eq!(off.screen, PauseScreen::None);
    assert_eq!(GamePauseConfig::from_manifest(&off.to_toml()), Ok(off));
    for (name, button) in GAMEPAD_BUTTONS {
        assert_eq!(
            parse_gamepad_button(gamepad_button_name(*button)),
            Some(*button),
            "{name}"
        );
    }
    // Errors name the table; unknown keys are other features' and are ignored.
    assert!(
        GamePauseConfig::from_manifest("[game]\npause = 3\n")
            .is_err_and(|e| e.starts_with("Regia.toml [game]"))
    );
    assert!(
        GamePauseConfig::from_manifest("[game]\npause_gamepad = \"Turbo\"\n")
            .is_err_and(|e| e.contains("unknown pause_gamepad"))
    );
    assert!(GamePauseConfig::from_manifest("[game]\npause = true\nother = 1\n").is_ok());
}

#[test]
fn settings_load_from_a_project_root() {
    let root = regia_test_utils::unique_temp_dir("game-pause-settings");
    std::fs::create_dir_all(&root).expect("dir");
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = \"g\"\n[game]\npause = true\npause_screen = \"none\"\n",
    )
    .expect("manifest");
    let config = GamePauseConfig::load(&root).expect("loads");
    assert!(config.enabled);
    assert_eq!(config.screen, PauseScreen::None);
    let mut app = App::new();
    app.add_plugins(GamePausePlugin {
        project_root: Some(root.clone()),
        config: None,
    });
    assert!(app.world().resource::<GamePause>().is_enabled());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn template_names_from_paths() {
    for path in [
        "assets://ui/pause.ui.ron",
        "ui/pause.ui.ron",
        "pause",
        " pause.ui.ron ",
    ] {
        assert_eq!(ui_template_name(path), "pause", "{path}");
    }
    assert_eq!(PauseScreen::parse(""), PauseScreen::Default);
    assert_eq!(PauseScreen::parse("default").setting(), "default");
}

#[test]
fn bundled_strings_are_localized_and_complete() {
    let keys: Vec<&str> = BUNDLED_FTL[0]
        .1
        .lines()
        .filter_map(|l| l.split_once(" = ").map(|(k, _)| k))
        .collect();
    assert_eq!(keys.len(), 6);
    for (locale, ftl) in BUNDLED_FTL {
        for key in &keys {
            assert!(
                ftl.lines().any(|l| l.starts_with(&format!("{key} = "))),
                "{locale} lacks {key}"
            );
        }
    }
    let sources = BUNDLED_FTL
        .iter()
        .map(|(l, s)| ((*l).to_owned(), (*s).to_owned()))
        .collect();
    let mut loc = Localization::new(sources, Some("en".to_owned())).expect("bundled ftl parses");
    loc.set_locale("de");
    assert_eq!(loc.resolve("pause-title", &[]), "Pausiert");
}

#[test]
fn the_game_localization_overrides_the_bundled_strings() {
    let mut app = app(GamePauseConfig::enabled());
    let game = Localization::new(
        vec![("en".to_owned(), "pause-title = Time out\n".to_owned())],
        Some("en".to_owned()),
    )
    .expect("parses");
    app.insert_resource(game);
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    let root = app.world().resource::<GamePause>().screen_root().expect("root");
    assert_eq!(
        app.world().get::<Accessible>(root).and_then(|a| a.label.clone()),
        Some("Time out".to_owned())
    );
}

#[test]
fn without_input_or_ui_the_pause_still_freezes_time() {
    let mut app = App::new();
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(GamePausePlugin {
        config: Some(GamePauseConfig::enabled()),
        ..Default::default()
    });
    app.init_resource::<Steps>();
    app.add_systems(FixedUpdate, |mut s: ResMut<Steps>| s.0 += 1);
    app.update();
    app.world_mut().resource_mut::<GamePause>().pause();
    app.update();
    let steps = app.world().resource::<Steps>().0;
    app.update();
    app.update();
    assert_eq!(app.world().resource::<Steps>().0, steps);
    assert!(app.world().get_resource::<FixedTime>().is_some());
}

/// The `[game]` table is user text: byte flips, truncations and insertions only ever parse or answer an
/// error (deterministic mutations, no randomness from the clock).
#[test]
fn mutated_game_table_never_panics() {
    let seed_text = "[project]\nname = \"g\"\n[game]\npause = true\npause_key = \"F12\"\npause_gamepad = \"Start\"\npause_screen = \"ui/pause.ui.ron\"\n";
    let bytes = seed_text.as_bytes();
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let alphabet = b"[]=\"\\\n #.truefalsenone0123456789{}";
    let (mut parsed, mut refused) = (0u32, 0u32);
    for _ in 0..2000 {
        let mut text = bytes.to_vec();
        for _ in 0..(1 + next() % 4) {
            let at = (next() % text.len().max(1) as u64) as usize;
            match next() % 3 {
                0 => {
                    let last = text.len() - 1;
                    text[at.min(last)] = alphabet[(next() % alphabet.len() as u64) as usize];
                }
                1 => text.truncate(at),
                _ => text.insert(
                    at.min(text.len()),
                    alphabet[(next() % alphabet.len() as u64) as usize],
                ),
            }
            if text.is_empty() {
                text.push(b'#');
            }
        }
        let text = String::from_utf8_lossy(&text);
        match GamePauseConfig::from_manifest(&text) {
            Ok(config) => {
                parsed += 1;
                // Whatever parsed writes back to a table that reads as the same settings.
                assert_eq!(GamePauseConfig::from_manifest(&config.to_toml()), Ok(config));
            }
            Err(error) => {
                refused += 1;
                assert!(error.starts_with("Regia.toml [game]"), "{error}");
            }
        }
    }
    assert!(
        parsed > 0 && refused > 0,
        "the mutations reach both outcomes ({parsed}, {refused})"
    );
}

#[test]
fn a_packaged_game_reads_the_table_from_build_info() {
    let root = regia_test_utils::unique_temp_dir("game-pause-packaged");
    std::fs::create_dir_all(&root).expect("dir");
    std::fs::write(
        root.join("build_info.json"),
        r#"{"game":{"pause":true,"pause_key":"F10","pause_gamepad":"Start","pause_screen":"none"}}"#,
    )
    .expect("build info");
    let mut app = App::new();
    app.add_plugins(GamePausePlugin {
        project_root: Some(root.clone()),
        config: None,
    });
    let pause = app.world().resource::<GamePause>();
    assert!(pause.is_enabled());
    assert_eq!(pause.config().key.as_deref(), Some("F10"));
    assert_eq!(pause.config().gamepad, Some(GamepadButton::Start));
    assert_eq!(pause.config().screen, PauseScreen::None);
    // A bad object is ignored with the reason, and the pause stays off.
    std::fs::write(root.join("build_info.json"), r#"{"game":{"pause":"yes"}}"#).expect("build info");
    let package = crate::packaged_config::PackagedConfig::read_directory(&root);
    assert!(GamePauseConfig::from_package(&package).is_err_and(|e| e.starts_with("Regia.toml [game]")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn turning_the_pause_on_at_run_time_binds_the_key() {
    // A game's options menu turns it on after start-up.
    let mut app = app(GamePauseConfig::default());
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(!paused(&app));
    app.world_mut().resource_mut::<GamePause>().set_enabled(true);
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    assert!(paused(&app), "the key works once the pause is on");
}

#[test]
fn a_game_pause_inserted_before_the_plugin_is_kept() {
    // `DefaultPlugins` adds the plugin; a game sets the pause up in code by inserting the resource first.
    let mut config = GamePauseConfig::enabled();
    config.key = Some("F12".to_owned());
    let mut app = App::new();
    app.insert_resource(GamePause::new(config));
    app.add_plugins(GamePausePlugin::default());
    let pause = app.world().resource::<GamePause>();
    assert!(pause.is_enabled());
    assert_eq!(pause.config().key.as_deref(), Some("F12"));
}

#[test]
fn without_the_action_layer_the_raw_key_pauses() {
    // A game that reads raw devices (no `InputPlugin`, no `InputConfig`), like `third_person`.
    let mut config = GamePauseConfig::enabled();
    config.key = Some("F12".to_owned());
    let mut app = App::new();
    enable_fixed_step(&mut app, 60.0);
    app.add_message::<KeyboardInput>();
    app.add_plugins(GamePausePlugin {
        config: Some(config),
        ..Default::default()
    });
    app.update();
    tap(&mut app, "F11");
    assert!(!paused(&app), "another key does nothing");
    tap(&mut app, "F12");
    assert!(paused(&app));
    assert_eq!(
        app.world().resource::<GamePause>().source(),
        Some(PauseSource::Input)
    );
    tap(&mut app, "F12");
    assert!(!paused(&app));
}

#[test]
fn the_key_pauses_again_after_a_button_resume() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    for round in 0..3 {
        tap(&mut app, DEFAULT_PAUSE_KEY);
        assert!(paused(&app), "round {round}: the key pauses");
        command(&mut app, RESUME_COMMAND);
        assert!(!paused(&app), "round {round}: Resume resumes");
        app.update();
    }
    assert_eq!(app.world().resource::<GamePause>().counts(), (3, 3, 0));
}

#[test]
fn ui_navigation_stays_live_while_paused() {
    let input = game_input().with_menu_context();
    let mut app = app_with(GamePauseConfig::enabled(), input);
    app.world_mut()
        .resource_mut::<InputContexts>()
        .push(0, regia_input::MENU_CONTEXT);
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    let contexts = app.world().resource::<InputContexts>();
    assert!(
        contexts.is_enabled(0, regia_input::MENU_CONTEXT),
        "menu navigation stays on"
    );
    assert!(!contexts.is_enabled(0, regia_input::GAMEPLAY_CONTEXT));
}

#[test]
fn focus_starts_on_resume_and_enter_resumes() {
    let mut app = app(GamePauseConfig::enabled());
    app.update();
    tap(&mut app, DEFAULT_PAUSE_KEY);
    app.update();
    let resume = app
        .world_mut()
        .query::<(Entity, &UiCommandName)>()
        .iter(app.world())
        .find(|(_, c)| c.0 == RESUME_COMMAND)
        .map(|(e, _)| e)
        .expect("the Resume button");
    assert_eq!(
        app.world().resource::<regia_ui::focus::FocusState>().focused,
        Some(resume),
        "keyboard and gamepad navigation start on Resume"
    );
    tap(&mut app, "Enter");
    app.update();
    assert!(!paused(&app), "Enter on the focused Resume button resumes");
}
