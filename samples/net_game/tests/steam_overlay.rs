// SPDX-License-Identifier: MIT-0
//! The windowed client's Steam readout on the mock backend (ADR-7317): the client's plugins with
//! `NetGameSteam` build with `SteamPlugin`, an injected `OverlayActivated` reaches the HUD line,
//! the readout resource and the agent method `steam.status`, and the close comes back the same
//! way. No window, no GPU; agent calls go through the app's own dispatcher in-process and are
//! answered on the frame they are made.

use std::sync::Arc;

use net_game::hud::NetGameHud;
use net_game::steam_overlay::{NetGameSteam, STEAM_LINE_NODE, ShiftHeld, SteamOverlayReadout};
use net_game::{Side, headless_app};
use regia::agent::protocol::Request;
use regia::agent::server::Inbound;
use regia::agent::{AgentInbox, AgentPlugin, AgentSettings};
use regia::prelude::*;
use regia::steam::{MockSteam, SteamBackend, SteamEvent, SteamPlugin};
use serde_json::{Value, json};

/// The window's messages, which a real client's runner adds and the UI reads.
fn window_messages(app: &mut App) {
    use regia::platform as p;
    app.add_message::<p::WindowResized>()
        .add_message::<p::WindowScaleFactorChanged>()
        .add_message::<p::CursorMoved>()
        .add_message::<p::MouseButtonInput>()
        .add_message::<p::TouchInput>()
        .add_message::<p::MouseWheel>()
        .add_message::<p::KeyboardInput>()
        .add_message::<p::ImeEvent>();
}

/// The client binary's plugins (less the window and the renderer) with `steam` on `backend`.
fn client(tag: &str, backend: &Arc<MockSteam>) -> App {
    let mut app = headless_app(Side::Client);
    window_messages(&mut app);
    // The game's own input map (`NetGameView`): Tab holds the scoreboard's `Scores` action, so
    // the tests can drive the board the way a player does.
    let input = regia_input::InputConfig::new()
        .action("Scores", regia_input::ActionKind::Button)
        .binding(regia_input::Binding::new(
            "Scores",
            regia_input::InputSource::key("Tab"),
        ));
    app.add_plugins((
        MinimalPlugins,
        regia_input::InputPlugin::new(input),
        NetGameHud::default(),
        NetGameSteam::with_backend(Arc::clone(backend) as Arc<dyn SteamBackend>, "mock"),
    ))
    .insert_resource(AgentSettings {
        project_dir: Some(regia_test_utils::unique_temp_dir(tag)),
    })
    .insert_resource(regia::agent::UiAgentHost::new(regia::ui::agent_bridge::host_call))
    .add_plugins(AgentPlugin);
    app.finish_plugins();
    app
}

/// Calls `method` in-process; the app runs one frame to answer it.
fn agent(app: &mut App, method: &str, params: Value) -> Value {
    let (send, inbox) = crossbeam_channel::unbounded();
    let (reply, answers) = crossbeam_channel::unbounded();
    app.insert_resource(AgentInbox(inbox));
    let request = Request {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params,
    };
    send.send(Inbound { request, reply }).expect("inbox open");
    app.update();
    let answer = answers
        .try_recv()
        .unwrap_or_else(|_| panic!("{method}: no answer on this frame"));
    match (answer.result, answer.error) {
        (Some(value), None) => value,
        (_, error) => panic!("{method}: {error:?}"),
    }
}

/// The HUD's Steam line as the agent reads it (`ui.query` by node name).
fn steam_line(app: &mut App) -> String {
    let found = agent(app, "ui.query", json!({ "name": STEAM_LINE_NODE }));
    let nodes = found["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("ui.query answer: {found}"));
    assert_eq!(nodes.len(), 1, "one Steam line: {found}");
    nodes[0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("the Steam line has text: {found}"))
        .to_owned()
}

fn readout(app: &App) -> SteamOverlayReadout {
    app.world().resource::<SteamOverlayReadout>().clone()
}

#[test]
fn an_injected_overlay_shows_on_the_hud_and_in_steam_status() {
    let mock = MockSteam::shared();
    let mut app = client("steam-overlay-hud", &mock);
    assert!(
        app.has_plugin(std::any::type_name::<SteamPlugin>()),
        "the client's Steam build runs SteamPlugin"
    );
    app.update();
    app.update();

    let status = agent(&mut app, "steam.status", json!({}));
    assert_eq!(status["plugin"], true, "{status}");
    assert_eq!(status["running"], true, "{status}");
    assert_eq!(status["overlay_open"], false, "{status}");
    let line = steam_line(&mut app);
    assert!(
        line.starts_with("STEAM OVERLAY: closed (opened 0, closed 0)\nSteam (mock): running"),
        "the headline is the first row: {line}"
    );
    assert!(line.contains("overlay closed"), "{line}");
    assert!(line.contains("last: no overlay message yet"), "{line}");

    // Shift+Tab: Steam sends GameOverlayActivated(active), repeated callbacks are one edge.
    mock.inject(SteamEvent::OverlayActivated { active: true });
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    let seen = readout(&app);
    assert!(seen.overlay_open);
    assert_eq!((seen.opened, seen.closed), (1, 0));
    assert_eq!(seen.last_event, Some("OverlayOpened"));
    let status = agent(&mut app, "steam.status", json!({}));
    assert_eq!(status["overlay_open"], true, "{status}");
    let line = steam_line(&mut app);
    assert!(
        line.starts_with("STEAM OVERLAY: OPEN (opened 1, closed 0)\n"),
        "{line}"
    );
    assert!(line.contains("overlay OPEN"), "{line}");
    assert!(
        line.contains("last: OverlayOpened (opened 1, closed 0)"),
        "{line}"
    );

    // Shift+Tab again: closed.
    mock.inject(SteamEvent::OverlayActivated { active: false });
    app.update();
    let seen = readout(&app);
    assert!(!seen.overlay_open);
    assert_eq!((seen.opened, seen.closed), (1, 1));
    assert_eq!(seen.last_event, Some("OverlayClosed"));
    let status = agent(&mut app, "steam.status", json!({}));
    assert_eq!(status["overlay_open"], false, "{status}");
    let line = steam_line(&mut app);
    assert!(
        line.starts_with("STEAM OVERLAY: closed (opened 1, closed 1)\n"),
        "{line}"
    );
    assert!(line.contains("overlay closed"), "{line}");
    assert!(
        line.contains("last: OverlayClosed (opened 1, closed 1)"),
        "{line}"
    );
}

#[test]
fn open_and_close_in_one_frame_end_closed() {
    let mock = MockSteam::shared();
    let mut app = client("steam-overlay-batch", &mock);
    app.update();
    mock.inject(SteamEvent::OverlayActivated { active: true });
    mock.inject(SteamEvent::OverlayActivated { active: false });
    app.update();
    let seen = readout(&app);
    assert!(!seen.overlay_open);
    assert_eq!((seen.opened, seen.closed), (1, 1));
    assert_eq!(seen.last_event, Some("OverlayClosed"));
}

#[test]
fn without_a_steam_client_the_line_says_so_and_the_game_runs() {
    let mock = MockSteam::shared_unavailable();
    let mut app = client("steam-overlay-absent", &mock);
    app.update();
    let status = agent(&mut app, "steam.status", json!({}));
    assert_eq!(status["plugin"], true, "{status}");
    assert_eq!(status["running"], false, "{status}");
    assert_eq!(status["overlay_open"], false, "{status}");
    let line = steam_line(&mut app);
    assert!(line.contains("\nSteam (mock): not running"), "{line}");
    // A callback without a client changes nothing.
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    assert_eq!(readout(&app).opened, 0);
}

/// The board's hold action, or `None` without a board.
fn scores_hold_action(app: &mut App) -> Option<String> {
    let mut boards = app.world_mut().query::<&regia::ui::game_widgets::Scoreboard>();
    boards
        .iter(app.world())
        .next()
        .map(|board| board.hold_action.clone())
}

/// Whether the board is shown this frame (`ScoreboardState::shown`, what `scoreboard_update`
/// decides from `hold_action` and the held action).
fn scores_shown(app: &mut App) -> bool {
    let mut boards = app
        .world_mut()
        .query::<&regia::ui::game_widgets::ScoreboardState>();
    boards
        .iter(app.world())
        .next()
        .map(|state| state.shown)
        .unwrap_or(false)
}

/// A key event for the client's placeholder window.
fn key(physical: &str, logical: &str, state: regia::platform::ButtonState) -> regia::platform::KeyboardInput {
    regia::platform::KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: physical.to_owned(),
        logical_key: logical.to_owned(),
        state,
        repeat: false,
        timestamp: regia::platform::InputTimestamp::now(),
    }
}

/// ADR-7349 (N05): the owner saw something that showed only while Shift+Tab was held. This HUD's
/// scoreboard shows while Tab is held, so in a Steam build Shift (and an open overlay) parks the
/// board's hold action and the board cannot show; releasing Shift or closing the overlay puts it
/// back. w9-net-vehicle-steam-z 5: the asserts are the behaviour — `ScoreboardState::shown` with
/// Tab genuinely held through the game's own input map — not only the parked string.
#[test]
fn shift_and_an_open_overlay_keep_the_tab_scoreboard_hidden() {
    use regia::platform::ButtonState;
    let mock = MockSteam::shared();
    let mut app = client("steam-overlay-scores", &mock);
    app.update();
    app.update();
    assert_eq!(scores_hold_action(&mut app).as_deref(), Some("Scores"));
    let shift = |state| key("ShiftLeft", "Shift", state);
    let tab = |state| key("Tab", "Tab", state);

    // With Tab held the board shows: the behaviour the parking must break.
    app.world_mut().write_message(tab(ButtonState::Pressed));
    app.update();
    app.update();
    assert!(scores_shown(&mut app), "Tab held: the board shows");

    // Shift goes down on top of it: parked, and the board cannot show even with Tab held.
    app.world_mut().write_message(shift(ButtonState::Pressed));
    app.update();
    app.update();
    assert_eq!(
        scores_hold_action(&mut app).as_deref(),
        Some(""),
        "Shift down: Tab cannot show the board"
    );
    assert!(
        !scores_shown(&mut app),
        "Shift down: the board is hidden with Tab held"
    );
    app.world_mut().write_message(shift(ButtonState::Released));
    app.update();
    app.update();
    assert_eq!(scores_hold_action(&mut app).as_deref(), Some("Scores"));
    assert!(scores_shown(&mut app), "Shift up: the board shows again");

    // A focus loss with Shift down clears it (the key-up goes elsewhere).
    app.world_mut().write_message(shift(ButtonState::Pressed));
    app.update();
    app.world_mut().write_message(regia::platform::WindowFocused {
        window: Entity::PLACEHOLDER,
        focused: false,
    });
    app.update();
    app.update();
    assert_eq!(scores_hold_action(&mut app).as_deref(), Some("Scores"));
    assert!(!app.world().resource::<ShiftHeld>().0, "focus loss clears Shift");

    // The overlay open: parked in the same frame (the readout and the parking are ordered), and
    // hidden even with Tab held. Held until it closes.
    app.world_mut().write_message(tab(ButtonState::Pressed));
    app.update();
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    assert_eq!(
        scores_hold_action(&mut app).as_deref(),
        Some(""),
        "the open overlay parks the board on the frame it opens"
    );
    assert!(!scores_shown(&mut app), "overlay open: hidden with Tab held");
    mock.inject(SteamEvent::OverlayActivated { active: false });
    app.update();
    app.update();
    assert_eq!(scores_hold_action(&mut app).as_deref(), Some("Scores"));
    assert!(scores_shown(&mut app), "overlay closed: the board shows again");
    app.world_mut().write_message(tab(ButtonState::Released));
    app.update();
}

/// w9-net-vehicle-steam-z 4: the overlay draws inside the game's window, so the window keeps
/// focus and the Shift key-up goes to the overlay — `ShiftHeld` would stick and keep the
/// scoreboard parked for ever after the close. An opening overlay clears it, as a focus loss does.
#[test]
fn an_opening_overlay_clears_a_held_shift_key() {
    use regia::platform::ButtonState;
    let mock = MockSteam::shared();
    let mut app = client("steam-overlay-sticky-shift", &mock);
    app.update();
    app.update();
    let shift = |state| key("ShiftLeft", "Shift", state);

    // Shift down, no key-up, no focus loss: only the overlay can clear it.
    app.world_mut().write_message(shift(ButtonState::Pressed));
    app.update();
    app.update();
    assert!(app.world().resource::<ShiftHeld>().0, "shift held");
    mock.inject(SteamEvent::OverlayActivated { active: true });
    app.update();
    assert!(
        !app.world().resource::<ShiftHeld>().0,
        "the opening overlay clears the held shift (its key-up went to the overlay)"
    );
    // The board is not left parked once the overlay closes: nothing holds the block.
    mock.inject(SteamEvent::OverlayActivated { active: false });
    app.update();
    app.update();
    assert_eq!(scores_hold_action(&mut app).as_deref(), Some("Scores"));
}
