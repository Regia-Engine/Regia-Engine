// SPDX-License-Identifier: MIT-0
//! The HUD's game widgets in a real match (ADR-2958, Rule 8): a server and two bots play on the
//! in-memory network; client 0 runs the HUD and an agent reads every widget over the real agent
//! protocol. The lobby test takes two clients through the lobby list's own requests, from an empty
//! list to a match. No window, no GPU; time is the links' manual clock.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use net_game::hud::{LEAD_SPEED, NetGameHud, RADAR_RANGE};
use net_game::lobby::{LobbyLinkOptions, NetGameLobbyPlugin};
use net_game::view::NetGameView;
use net_game::{Bot, NetGameWire, PadSource, Side, headless_app, link_options};
use regia::agent::client::Client;
use regia::agent::endpoint;
use regia::net::link::{LinkClock, LinkOptions, NetLink};
use regia::net::{ConditionerConfig, Lobbies, LocalLobbyHub, LoopbackNet};
use regia::prelude::*;
use regia::ui::game_widgets::{LobbyList, LobbyListAction, LobbyListRequest, Scoreboard};
use serde_json::{Value, json};

/// Hang guard only.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(180);

fn clock() -> LinkClock {
    LinkClock::Manual {
        start_ms: 0,
        step_us: 16_667,
    }
}

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

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

/// Polls `method` until `done` holds (frames pass between calls), under the 50 calls per second the
/// agent server admits: the widget methods change things, so reads count too.
fn wait(
    client: &mut Client,
    method: &str,
    params: Value,
    what: &str,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let mut last = Value::Null;
    for _ in 0..2000 {
        last = call(client, method, params.clone());
        if done(&last) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("{what}: never true; last {method}: {last}");
}

#[test]
fn an_agent_reads_every_widget_of_a_running_match() {
    let project = regia_test_utils::temp::unique_temp_dir("net-game-hud");
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(([127, 0, 0, 1], 7777));
    let mut server = headless_app(Side::Server);
    let opts = link_options(clock(), ConditionerConfig::default());
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<NetGameWire>(Box::new(net.bind(server_addr).expect("bind")), &opts)
        .expect("host");
    let client_opts = LinkOptions {
        server_key: server.world().resource::<NetLink>().server_key(),
        ..opts
    };
    let mut clients: Vec<App> = (0..2)
        .map(|i| {
            let mut app = headless_app(Side::Client);
            app.insert_resource(PadSource::Bot(Bot::new(i as f32, 20, u32::MAX)));
            app.world_mut()
                .resource_mut::<NetLink>()
                .connect::<NetGameWire>(
                    Box::new(net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind")),
                    server_addr,
                    &client_opts,
                )
                .expect("connect");
            app
        })
        .collect();
    window_messages(&mut clients[0]);
    clients[0]
        .add_plugins((
            MinimalPlugins,
            NetGameView { input: false },
            NetGameHud::default(),
        ))
        .insert_resource(regia::agent::AgentSettings {
            project_dir: Some(project.clone()),
        })
        .insert_resource(regia::agent::UiAgentHost::new(regia::ui::agent_bridge::host_call))
        .add_plugins(regia::agent::AgentPlugin);

    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("net-game-hud".into())
        .spawn(move || {
            let mut agent = None;
            for _ in 0..6000 {
                if let Ok((advertised, token)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &token)
                {
                    agent = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut agent) = agent else {
                panic!("no endpoint at {}", agent_dir.display());
            };

            // The scoreboard: the board is held (hidden), an agent shows it, the rules fill it.
            let hidden = call(&mut agent, "ui.scoreboard", json!({}));
            assert_eq!(hidden["visible"], false);
            assert_eq!(hidden["hold_action"], "Scores");
            call(&mut agent, "ui.scoreboard", json!({ "action": "show" }));
            let board = wait(
                &mut agent,
                "ui.scoreboard",
                json!({}),
                "two players on the board and it shows",
                |b| b["shown"] == true && b["lines"].as_array().is_some_and(|l| l.len() == 2),
            );
            assert_eq!(board["from_rules"], true);
            assert_eq!(board["columns"][0]["key"], "tags");
            let locals = board["lines"]
                .as_array()
                .expect("lines")
                .iter()
                .filter(|l| l["local"] == true)
                .count();
            assert_eq!(locals, 1, "{board}");
            // The bots shoot: tags from the replicated scores reach the board, largest first.
            let tagged = wait(
                &mut agent,
                "ui.scoreboard",
                json!({}),
                "a tag reaches the board",
                |b| b["lines"][0]["values"][0].as_i64().is_some_and(|t| t > 0),
            );
            let first = tagged["lines"][0]["values"][0].as_i64().expect("tags");
            let second = tagged["lines"][1]["values"][0].as_i64().expect("tags");
            assert!(first >= second, "sorted largest first: {tagged}");

            // The minimap and the radar, by name: without `entity` a call takes the lowest entity bits.
            let id_of = |agent: &mut Client, name: &str| {
                call(agent, "ui.query", json!({ "name": name }))["nodes"][0]["id"].clone()
            };
            let minimap_id = id_of(&mut agent, "minimap");
            let minimap = wait(
                &mut agent,
                "ui.radar",
                json!({ "entity": minimap_id }),
                "both avatars on the minimap",
                |r| r["blips"].as_array().is_some_and(|b| b.len() == 2),
            );
            assert_eq!(minimap["mode"], "minimap");
            let size = minimap["size"][0].as_f64().expect("size");
            assert!(size > 100.0, "{minimap}");
            for blip in minimap["blips"].as_array().expect("blips") {
                assert_eq!(blip["inside"], true, "the arena is inside the minimap: {blip}");
            }
            let radar_id = id_of(&mut agent, "radar");
            let radar = wait(
                &mut agent,
                "ui.radar",
                json!({ "entity": radar_id }),
                "the radar centers on the player",
                |r| r["center"].as_array().is_some_and(|c| c[0] != 0.0 || c[1] != 0.0),
            );
            assert_eq!(radar["mode"], "radar");
            assert_eq!(radar["range"], f64::from(RADAR_RANGE));
            // The player is at its center, so only the other player has a blip on it.
            assert_eq!(radar["blips"].as_array().map(Vec::len), Some(1), "{radar}");

            // Markers: the other player on the layer, the player's own as a plate in the world.
            let markers = wait(
                &mut agent,
                "ui.world_markers",
                json!({}),
                "a marker and a plate",
                |m| {
                    m["markers"].as_array().is_some_and(|a| a.len() == 1)
                        && m["plates"].as_array().is_some_and(|a| a.len() == 1)
                },
            );
            assert!(markers["camera"].is_u64(), "{markers}");
            assert!(markers["markers"][0]["distance"].as_f64().expect("distance") > 1.0);
            assert_eq!(markers["markers"][0]["visible"], true, "{markers}");

            // The bracket: on the other player, with a lead for the template's muzzle speed.
            let bracket = wait(
                &mut agent,
                "ui.target_bracket",
                json!({}),
                "a bracket with a lead",
                |b| b["brackets"][0]["time_to_intercept"].is_f64(),
            );
            let b = &bracket["brackets"][0];
            assert_eq!(b["on_screen"], true, "{bracket}");
            assert_eq!(b["no_solution"], false);
            assert!(b["half_px"].as_f64().expect("half") >= 14.0);
            let flight = b["time_to_intercept"].as_f64().expect("time");
            assert!(flight > 0.0 && flight < 60.0 / f64::from(LEAD_SPEED), "{bracket}");
            assert!(b["lead_world"].is_array());

            // This app has no lobbies.
            let refused = agent.call("ui.lobby_list", json!({}));
            assert!(refused.is_err(), "{refused:?}");

            flag.store(true, Ordering::Relaxed);
        })
        .expect("script thread");

    for app in &mut clients {
        app.finish_plugins();
    }
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !done.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        server.update();
        for app in &mut clients {
            app.update();
        }
    }
    script.join().expect("the script passed");
    assert!(done.load(Ordering::Relaxed), "the script did not finish");
    let _ = std::fs::remove_dir_all(&project);
}

fn request(app: &mut App, list: Entity, action: LobbyListAction) {
    app.world_mut().write_message(LobbyListRequest { list, action });
}

fn list_of(app: &mut App) -> (Entity, LobbyList) {
    let world = app.world_mut();
    world
        .query::<(Entity, &LobbyList)>()
        .iter(world)
        .next()
        .map(|(e, l)| (e, l.clone()))
        .expect("the HUD spawned a lobby list")
}

#[test]
fn the_lobby_list_takes_two_clients_from_nothing_to_a_match() {
    // The lobby's launch sends clients to a UDP address, so this server is on real UDP.
    let mut server = headless_app(Side::Server);
    let opts = link_options(clock(), ConditionerConfig::default());
    net_game::host_udp(&mut server, SocketAddr::from(([127, 0, 0, 1], 0)), &opts).expect("bind");
    let server_addr = server.world().resource::<NetLink>().local_addr().expect("bound");
    let client_opts = LinkOptions {
        server_key: server.world().resource::<NetLink>().server_key(),
        ..opts
    };
    let hub = LocalLobbyHub::default();
    let mut clients: Vec<App> = (1..=2u64)
        .map(|id| {
            let mut app = headless_app(Side::Client);
            app.add_plugins(MinimalPlugins);
            window_messages(&mut app);
            app.insert_resource(Lobbies::new(Arc::new(hub.member(id, &format!("p{id}")))))
                .insert_resource(LobbyLinkOptions(client_opts.clone()))
                .add_plugins((
                    NetGameLobbyPlugin,
                    NetGameView { input: false },
                    NetGameHud {
                        lobby_server: Some(server_addr),
                    },
                ));
            app.finish_plugins();
            app
        })
        .collect();
    fn step(server: &mut App, clients: &mut [App], n: usize) {
        for _ in 0..n {
            server.update();
            for app in clients.iter_mut() {
                app.update();
            }
        }
    }
    step(&mut server, &mut clients, 3);
    let (owner_list, list) = list_of(&mut clients[0]);
    let (guest_list, _) = list_of(&mut clients[1]);
    assert!(list.rows.is_empty() && list.current.is_none());

    // Create: the owner sees its lobby.
    request(&mut clients[0], owner_list, LobbyListAction::Create);
    step(&mut server, &mut clients, 4);
    let (_, list) = list_of(&mut clients[0]);
    assert!(list.current.as_ref().is_some_and(|c| c.is_owner), "{list:?}");

    // Refresh and join: the guest finds it in the list.
    request(&mut clients[1], guest_list, LobbyListAction::Refresh);
    step(&mut server, &mut clients, 4);
    let (_, seen) = list_of(&mut clients[1]);
    assert_eq!(seen.rows.len(), 1, "{seen:?}");
    assert!(seen.rows[0].joinable);
    request(
        &mut clients[1],
        guest_list,
        LobbyListAction::Join(seen.rows[0].id),
    );
    step(&mut server, &mut clients, 4);
    let (_, list) = list_of(&mut clients[0]);
    assert_eq!(list.current.as_ref().expect("in").members.len(), 2);
    assert!(!list.current.as_ref().expect("in").can_launch, "nobody is ready");

    // Ready, then start: both links connect to the dedicated server.
    request(&mut clients[0], owner_list, LobbyListAction::SetReady(true));
    request(&mut clients[1], guest_list, LobbyListAction::SetReady(true));
    step(&mut server, &mut clients, 4);
    let (_, list) = list_of(&mut clients[0]);
    assert!(list.current.as_ref().expect("in").can_launch, "{list:?}");
    assert!(
        matches!(list.status, regia::ui::game_widgets::LobbyListStatus::Idle),
        "{list:?}"
    );
    request(&mut clients[0], owner_list, LobbyListAction::Launch);
    step(&mut server, &mut clients, 600);
    for app in &mut clients {
        assert!(app.world().resource::<NetLink>().is_connected());
        let (list, _) = list_of(app);
        let style = app.world().get::<NodeStyle>(list).expect("style");
        assert_eq!(style.0.display, Display::None, "the list has done its job");
        // The match is on: the board has both players.
        let world = app.world_mut();
        let board = world
            .query::<&Scoreboard>()
            .iter(world)
            .next()
            .cloned()
            .expect("board");
        assert_eq!(board.table.rows.len(), 2, "{board:?}");
    }
}
