// SPDX-License-Identifier: MPL-2.0
//! PIE client worlds: join, replication, focused input, teardown, determinism (ADR-2623).
//!
//! A small test game stands in for a game crate: `register` reads [`NetMode`] (the play world has
//! none, so it serves; a client world has `Client`), replicates a `Crate` and an `Avatar`, and
//! predicts the owner's avatar from a `Pad` command fed by the `KeyD` key. Every world steps on
//! the editor's frame, so all numbers here are exact and repeat run to run.

use std::cell::{Cell, RefCell};

use regia_core::math::Vec3;
use regia_net::replication::NetPredicted;
use regia_net::{
    InputCommand, LinkInterpolationPlugin, NetInput, NetInputAppExt, NetOwner, NetPlayer, NetRole, NetSet,
    PredictAppExt, PredictedScheduleExt, PredictionCx, Replicate, ReplicateAppExt, ReplicateOptions,
    Tolerance, TransformReplication,
};
use regia_scene::level::{Level, LevelLibrary, LoadMode, open_level};
use regia_scene::prelude::{LevelId, Name, Transform};

use super::*;
use crate::engine::app::FixedUpdate;
use crate::hot_reload::GameExports;
use crate::pie::{PieRequest, apply_request, start_play, stop_play};

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Crate {
    id: u32,
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Avatar {
    slot: u32,
}

#[derive(Clone, Debug, PartialEq, Default, Replicate)]
struct Pad {
    move_x: f32,
}

impl NetInput for Pad {
    fn on_missing(last: &Self) -> Self {
        last.clone()
    }
}

/// Client: `KeyD` held.
#[derive(Resource, Default)]
struct Held(bool);

/// Server: the next avatar slot.
#[derive(Resource, Default)]
struct Slots(u32);

/// Meters an avatar runs per tick at full input.
const STEP_M: f32 = 0.1;

fn register(app: &mut App) {
    let mode = app
        .world()
        .get_resource::<NetMode>()
        .copied()
        .unwrap_or(NetMode::ListenServer);
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    app.add_message::<regia_platform::KeyboardInput>();
    app.replicate::<Crate>(ReplicateOptions::default())
        .replicate::<Avatar>(ReplicateOptions::default())
        .add_net_input::<Pad>()
        .predict::<Transform>(Tolerance::position(0.001))
        .add_predicted_systems(move_avatars);
    app.init_resource::<Held>();
    app.add_systems(FixedUpdate, feed_pad.after(NetSet::Apply).before(NetSet::Predict));
    if mode == NetMode::Client {
        regia_net::interp::install_interpolation(app.world_mut(), 60.0, 1.0);
        app.add_plugins(LinkInterpolationPlugin);
    } else {
        app.init_resource::<Slots>();
        app.add_systems(
            FixedUpdate,
            sync_avatars.after(NetSet::Apply).before(NetSet::Predict),
        );
    }
}

thread_local! {
    static UNLOADS: Cell<u32> = const { Cell::new(0) };
    /// Every game load and unload on this thread, in order.
    static GAME_LOG: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    /// Client worlds whose `register` ran on this thread.
    static CLIENT_REGISTERS: Cell<u32> = const { Cell::new(0) };
}

fn on_unload(_: &mut World) {
    UNLOADS.with(|n| n.set(n.get() + 1));
    GAME_LOG.with(|l| l.borrow_mut().push("unload"));
}

fn on_load(_: &mut World) {
    GAME_LOG.with(|l| l.borrow_mut().push("load"));
}

/// Counts the client worlds, then fails in the second one.
fn register_fails_in_the_second_client(app: &mut App) {
    if app.world().get_resource::<NetMode>() == Some(&NetMode::Client) {
        let built = CLIENT_REGISTERS.with(|n| n.replace(n.get() + 1));
        assert!(built == 0, "the second client's register fails");
    }
    register(app);
}

/// Starts its own link in the play world, which PIE cannot host on.
fn register_with_its_own_link(app: &mut App) {
    let client = app.world().get_resource::<NetMode>() == Some(&NetMode::Client);
    if client {
        CLIENT_REGISTERS.with(|n| n.set(n.get() + 1));
    }
    register(app);
    if !client {
        let socket = LoopbackNet::new()
            .bind(PIE_SERVER_ADDR)
            .expect("a fresh network has every address");
        let opts = link_options(60.0, "own".to_owned(), "own".to_owned());
        app.world_mut()
            .resource_mut::<NetLink>()
            .host::<PieWire>(Box::new(socket), &opts)
            .expect("the game's own link");
    }
}

fn game_with(register: fn(&mut App)) -> CurrentGame {
    let mut current = game();
    current.exports.register = register;
    current
}

fn game() -> CurrentGame {
    CurrentGame {
        exports: GameExports {
            register,
            on_unload,
            on_load,
        },
        generation: 1,
        package: "pie_worlds_test".to_owned(),
    }
}

fn feed_pad(
    mut keys: MessageReader<regia_platform::KeyboardInput>,
    mut held: ResMut<Held>,
    mode: Res<NetMode>,
    mut commands: Query<&mut InputCommand<Pad>>,
) {
    for key in keys.read() {
        if key.physical_key == "KeyD" {
            held.0 = key.state == regia_platform::ButtonState::Pressed;
        }
    }
    if mode.is_server() {
        return;
    }
    let move_x = if held.0 { 1.0 } else { 0.0 };
    for mut command in &mut commands {
        command.value = Pad { move_x };
    }
}

type AvatarMotion = (
    &'static mut Transform,
    Option<&'static NetOwner>,
    Option<&'static NetRole>,
);

fn move_avatars(
    cx: PredictionCx,
    inputs: Query<(&NetPlayer, &InputCommand<Pad>)>,
    mut avatars: Query<AvatarMotion, With<Avatar>>,
) {
    for (mut t, owner, role) in &mut avatars {
        if !cx.simulates(role) {
            continue;
        }
        let Some(player) = cx.owner_player(owner) else {
            continue;
        };
        if let Some((_, c)) = inputs.iter().find(|(p, _)| p.id == player) {
            t.translation.x += c.value.move_x * STEP_M;
        }
    }
}

fn sync_avatars(world: &mut World) {
    let peers = world.resource::<NetNode>().peers();
    let mut q = world.query_filtered::<&NetOwner, With<Avatar>>();
    let owned: Vec<regia_net::PeerId> = q.iter(world).map(|o| o.0).collect();
    for peer in peers {
        if owned.contains(&peer) {
            continue;
        }
        let slot = {
            let mut slots = world.resource_mut::<Slots>();
            slots.0 += 1;
            slots.0 - 1
        };
        world.spawn((
            Name::new(format!("Avatar{slot}")),
            Replicated {
                transform: TransformReplication::PositionOnly,
                ..Replicated::default()
            },
            Transform::from_translation(Vec3::new(slot as f32 * 3.0, 0.0, 0.0)),
            NetPredicted,
            NetOwner(peer),
            Avatar { slot },
        ));
    }
}

/// The test level: one rock.
const LEVEL_ROCK: &str = r#"
    (
        format_version: 1,
        entities: [
            (guid: "8b6f2a11-0000-4000-8000-0000000000a1", name: "Rock",
             components: { "regia_scene::transform::Transform": () }),
        ],
    )
    "#;

/// The rock and a level-authored replicated entity, the way a saved level holds it.
const LEVEL_BEACON: &str = r#"
    (
        format_version: 1,
        entities: [
            (guid: "8b6f2a11-0000-4000-8000-0000000000a1", name: "Rock",
             components: { "regia_scene::transform::Transform": () }),
            (guid: "8b6f2a11-0000-4000-8000-0000000000a2", name: "Beacon",
             components: {
                "regia_scene::transform::Transform": (
                    translation: (5.0, 6.0, 7.0),
                    rotation: (0.0, 0.0, 0.0, 1.0),
                    scale: (1.0, 1.0, 1.0),
                ),
                "regia_net::replication::Replicated": (),
             }),
        ],
    )
    "#;

/// The edit world: a level with one rock, the editor's agent methods, the Net tab configured.
fn edit_app(mode: PieNetMode, clients: u32) -> App {
    edit_app_with(mode, clients, LEVEL_ROCK)
}

fn edit_app_with(mode: PieNetMode, clients: u32, level_text: &str) -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    // As the editor does: the level types are known before a level opens.
    crate::mesh_source::register_level_types(app.world_mut());
    let id = LevelId(regia_core::id::Guid::from_u128(
        0x8b6f_2a11_0000_4000_8000_0000_0000_00a0,
    ));
    let level = Level::from_ron_str(id, level_text).expect("parse");
    app.world_mut().resource_mut::<LevelLibrary>().insert(level);
    open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
    app.world_mut().init_resource::<crate::command::UndoStack>();
    app.world_mut().init_resource::<crate::anim::AnimEditor>();
    crate::agent_ext::install(app.world_mut());
    let mut net = PieNet::default();
    net.configure(Some(mode), Some(clients), None, None)
        .expect("config");
    app.insert_resource(net);
    app
}

/// The editor loop's PIE part, frame by frame.
struct Rig {
    edit: App,
    pie: Option<PieSession>,
}

impl Rig {
    fn new(mode: PieNetMode, clients: u32, game: Option<CurrentGame>) -> Self {
        Self::new_after(mode, clients, game, std::time::Duration::ZERO)
    }

    /// [`Rig::new`] with `stall` of wall time between Play and the first frame (a loaded machine).
    fn new_after(
        mode: PieNetMode,
        clients: u32,
        game: Option<CurrentGame>,
        stall: std::time::Duration,
    ) -> Self {
        let mut edit = edit_app(mode, clients);
        let pie = Some(start_play(edit.world_mut(), game).expect("play"));
        // Not a synchronization: it stands in for a slow first frame, which must not change a tick.
        std::thread::sleep(stall);
        let mut rig = Self { edit, pie };
        rig.frame();
        rig
    }

    /// [`Rig::new`] on another level.
    fn with_level(mode: PieNetMode, clients: u32, level_text: &str) -> Self {
        let mut edit = edit_app_with(mode, clients, level_text);
        let pie = Some(start_play(edit.world_mut(), Some(game())).expect("play"));
        let mut rig = Self { edit, pie };
        rig.frame();
        rig
    }

    /// A toolbar or agent request, as the editor loops apply them, before the frame's pump.
    fn request(&mut self, request: PieRequest) -> String {
        apply_request(self.edit.world_mut(), &mut self.pie, request, Some(game()))
    }

    /// `editor.pie_net_clients`'s `frames` per client.
    fn client_frames(&mut self) -> Vec<u64> {
        let listed = self.call("editor.pie_net_clients", json!({})).expect("list");
        listed["clients"]
            .as_array()
            .map(|a| a.iter().filter_map(|c| c["frames"].as_u64()).collect())
            .unwrap_or_default()
    }

    /// The editor loops' order: client worlds, the play world's frame, then the `pie_net` hooks.
    fn frame(&mut self) {
        pump(self.edit.world_mut(), self.pie.as_mut());
        if let Some(session) = self.pie.as_mut() {
            assert_eq!(session.frame(), None, "no panic in the play world");
        }
        let world = self.edit.world_mut();
        let paused = self
            .pie
            .as_ref()
            .is_some_and(|s| s.mode == crate::pie::PlayMode::Paused);
        crate::pie_net::sync_with_play(world, self.pie.is_some());
        crate::pie_net::step_frame(world, paused);
    }

    fn frames(&mut self, n: u32) {
        for _ in 0..n {
            self.frame();
        }
    }

    fn server(&mut self) -> &mut World {
        self.pie.as_mut().expect("playing").app.world_mut()
    }

    fn client<R>(&mut self, index: usize, f: impl FnOnce(&mut World) -> R) -> R {
        with_client(self.edit.world_mut(), index, |_, c| f(c.world_mut())).expect("client world")
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let world = self.edit.world_mut();
        let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
        let (_, handler) = extension
            .method(method)
            .unwrap_or_else(|| panic!("{method} is not registered"));
        handler(world, &params)
    }

    /// Steps until every client is connected and sees one avatar per client; returns the frames.
    fn settle(&mut self, clients: usize) -> u32 {
        for n in 0..600 {
            let ready = (0..clients).all(|i| {
                self.client(i, |w| {
                    let connected = w.resource::<NetLink>().is_connected();
                    let avatars = w.query_filtered::<(), With<Avatar>>().iter(w).count();
                    let owned = w
                        .query_filtered::<(), (With<Avatar>, With<NetOwnedLocally>)>()
                        .iter(w)
                        .count();
                    connected && avatars == clients && owned == 1
                })
            });
            if ready {
                return n;
            }
            self.frame();
        }
        panic!("the clients did not settle in 600 frames");
    }
}

fn code(result: Result<Value, RpcError>) -> String {
    result
        .expect_err("refused")
        .data
        .and_then(|d| d["code"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn names(world: &mut World) -> Vec<String> {
    world
        .query::<&Name>()
        .iter(world)
        .map(|n| n.as_str().to_owned())
        .collect()
}

fn crate_seen(world: &mut World, id: u32) -> bool {
    world.query::<&Crate>().iter(world).any(|c| c.id == id)
}

fn tick(world: &World) -> u64 {
    u64::from(world.resource::<NetNode>().tick.0)
}

/// What one join-and-spawn run saw: frames to settle, the server tick at the spawn, and per
/// client the frame and the applied tick at which the crate appeared.
#[derive(Debug, PartialEq)]
struct SpawnRun {
    settle_frames: u32,
    spawn_tick: u64,
    send_every: u64,
    seen: Vec<(u32, u64)>,
    rows: Vec<(usize, bool, usize, usize, u64, u64)>,
}

fn join_and_spawn(stall: std::time::Duration) -> SpawnRun {
    join_and_spawn_with(2, stall)
}

fn join_and_spawn_with(clients: usize, stall: std::time::Duration) -> SpawnRun {
    let count = u32::try_from(clients).expect("a small client count");
    let mut rig = Rig::new_after(PieNetMode::Listen, count, Some(game()), stall);
    let settle_frames = rig.settle(clients);
    let server = rig.server();
    let spawn_tick = tick(server);
    let send_every = u64::from(
        server
            .resource::<regia_net::NetSettings>()
            .replication
            .send_every_ticks,
    );
    server.spawn((
        Name::new("Crate7"),
        Replicated::default(),
        Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)),
        Crate { id: 7 },
    ));
    let mut seen: Vec<Option<(u32, u64)>> = vec![None; clients];
    for frame in 1..=120 {
        rig.frame();
        for (i, slot) in seen.iter_mut().enumerate() {
            if slot.is_none() && rig.client(i, |w| crate_seen(w, 7)) {
                *slot = Some((frame, rig.client(i, |w| tick(w))));
            }
        }
        if seen.iter().all(Option::is_some) {
            break;
        }
    }
    let edit = rig.edit.world_mut();
    let mut worlds = edit.remove_non_send::<PieWorlds>().expect("worlds");
    let rows = worlds
        .clients
        .iter_mut()
        .map(|c| {
            let r = c.row();
            (
                r.index,
                r.connected,
                r.replicated,
                r.owned,
                r.last_applied_tick,
                r.frames,
            )
        })
        .collect();
    edit.insert_non_send(worlds);
    SpawnRun {
        settle_frames,
        spawn_tick,
        send_every,
        seen: seen
            .into_iter()
            .map(|s| s.expect("the crate reached every client"))
            .collect(),
        rows,
    }
}

#[test]
fn two_clients_join_and_a_server_spawn_reaches_both_worlds_at_the_same_tick() {
    let run = join_and_spawn(std::time::Duration::ZERO);
    let (frame0, tick0) = run.seen[0];
    assert_eq!(
        run.seen[1],
        (frame0, tick0),
        "both clients apply it in the same frame and tick"
    );
    // Between frames the server's tick is the one its next frame simulates: an entity spawned now
    // goes out with the first send at or after it.
    assert!(
        tick0 >= run.spawn_tick && tick0 < run.spawn_tick + run.send_every,
        "applied at tick {tick0}: the first send at or after the spawn at {} (every {} ticks)",
        run.spawn_tick,
        run.send_every
    );
    for (index, connected, replicated, owned, last_tick, frames) in &run.rows {
        assert!(*connected, "client {index} connected");
        assert_eq!(*replicated, 3, "client {index}: two avatars and the crate");
        assert_eq!(*owned, 1, "client {index} owns its own avatar");
        assert!(*last_tick >= tick0);
        assert!(*frames > 0);
    }
}

/// The Net tab's maximum. All clients of one session share a server, so they must not count as one
/// machine against the server's per-address limit (8 by default).
#[test]
fn the_maximum_of_sixteen_client_worlds_all_join_and_see_a_server_spawn() {
    assert_eq!(
        crate::pie_net::MAX_PIE_CLIENTS,
        16,
        "this test covers the documented maximum"
    );
    let run = join_and_spawn_with(16, std::time::Duration::ZERO);
    assert_eq!(run.rows.len(), 16);
    let (frame0, tick0) = run.seen[0];
    for (index, seen) in run.seen.iter().enumerate() {
        assert_eq!(
            *seen,
            (frame0, tick0),
            "client {index} applies it with the others"
        );
    }
    for (index, connected, replicated, owned, _, _) in &run.rows {
        assert!(*connected, "client {index} connected");
        assert_eq!(*replicated, 17, "client {index}: sixteen avatars and the crate");
        assert_eq!(*owned, 1, "client {index} owns its own avatar");
    }
}

#[test]
fn the_same_session_repeats_exactly_whatever_the_wall_clock_does() {
    let quick = join_and_spawn(std::time::Duration::ZERO);
    assert_eq!(quick, join_and_spawn(std::time::Duration::ZERO));
    // A slow first frame (a loaded machine) must not change a frame or a tick.
    assert_eq!(quick, join_and_spawn(std::time::Duration::from_millis(80)));
}

#[test]
fn client_worlds_get_the_level_but_not_its_replicated_entities() {
    let mut rig = Rig::new(PieNetMode::Dedicated, 1, Some(game()));
    rig.settle(1);
    let client_names = rig.client(0, names);
    assert!(client_names.contains(&"Rock".to_owned()), "{client_names:?}");
    assert_eq!(rig.client(0, |w| *w.resource::<NetMode>()), NetMode::Client);
    assert_eq!(
        *rig.server().resource::<NetMode>(),
        NetMode::DedicatedServer,
        "the play world serves"
    );
}

fn own_x(world: &mut World) -> f32 {
    world
        .query_filtered::<&Transform, (With<Avatar>, With<NetOwnedLocally>)>()
        .iter(world)
        .map(|t| t.translation.x)
        .next()
        .expect("an owned avatar")
}

fn server_x(world: &mut World) -> Vec<(u32, f32)> {
    let mut v: Vec<(u32, f32)> = world
        .query::<(&Avatar, &Transform)>()
        .iter(world)
        .map(|(a, t)| (a.slot, t.translation.x))
        .collect();
    v.sort_by_key(|(slot, _)| *slot);
    v
}

#[test]
fn focused_input_moves_only_that_clients_predicted_character() {
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game()));
    rig.settle(2);
    assert_eq!(
        code(rig.call("editor.pie_net_key", json!({ "key": "KeyD" }))),
        "pie_net_no_focus",
        "no client has the focus yet"
    );
    assert_eq!(
        code(rig.call("editor.pie_net_focus", json!({ "client": 5 }))),
        "pie_net_client_unknown"
    );
    let start = [rig.client(0, own_x), rig.client(1, own_x)];
    let server_start = server_x(rig.server());
    let focus = rig
        .call("editor.pie_net_focus", json!({ "client": 0 }))
        .expect("focus");
    assert_eq!(focus["focus"], 0);
    let key = rig
        .call("editor.pie_net_key", json!({ "key": "KeyD", "down": true }))
        .expect("key");
    assert_eq!(key["client"], 0);
    rig.frames(30);
    let moved = rig.client(0, own_x) - start[0];
    let still = rig.client(1, own_x) - start[1];
    assert!(moved > 2.0, "client 0's predicted avatar ran: {moved} m");
    assert!(still.abs() < 1e-6, "client 1's avatar did not move: {still} m");
    // The server agrees: exactly one avatar moved, the one client 0 owns.
    let server_now = server_x(rig.server());
    let deltas: Vec<f32> = server_now
        .iter()
        .zip(&server_start)
        .map(|((_, now), (_, then))| now - then)
        .collect();
    assert_eq!(deltas.iter().filter(|d| d.abs() > 1.0).count(), 1, "{deltas:?}");
    assert_eq!(deltas.iter().filter(|d| d.abs() < 1e-6).count(), 1, "{deltas:?}");
    // Releasing the key stops it.
    rig.call("editor.pie_net_key", json!({ "key": "KeyD", "down": false }))
        .expect("release");
    rig.frames(5);
    let after_release = rig.client(0, own_x);
    rig.frames(10);
    assert!((rig.client(0, own_x) - after_release).abs() < 1e-6);
}

/// The owned row of `editor.pie_net_entities`: what an agent sees of a client's own character.
fn owned_row(rig: &mut Rig, client: usize) -> Value {
    let list = rig
        .call("editor.pie_net_entities", json!({ "client": client }))
        .expect("entities");
    assert_eq!(list["client"], client);
    let rows = list["entities"].as_array().expect("rows");
    let owned: Vec<&Value> = rows.iter().filter(|r| r["owned"] == true).collect();
    assert_eq!(owned.len(), 1, "one owned avatar: {rows:?}");
    owned[0].clone()
}

fn row_x(row: &Value) -> f64 {
    row["translation"][0].as_f64().expect("translation")
}

#[test]
fn an_agent_sees_each_clients_own_state_by_net_id() {
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game()));
    rig.settle(2);
    let start = [owned_row(&mut rig, 0), owned_row(&mut rig, 1)];
    assert_ne!(
        start[0]["net_id"], start[1]["net_id"],
        "each client owns a different avatar"
    );
    rig.call("editor.pie_net_focus", json!({ "client": 1 }))
        .expect("focus");
    rig.call("editor.pie_net_key", json!({ "key": "KeyD" }))
        .expect("key");
    rig.frames(30);
    let now = [owned_row(&mut rig, 0), owned_row(&mut rig, 1)];
    assert!(row_x(&now[1]) - row_x(&start[1]) > 2.0, "{now:?}");
    assert!((row_x(&now[0]) - row_x(&start[0])).abs() < 1e-6, "{now:?}");

    // The replicated list is in net id order, with the client's tick; `all` adds local entities.
    let list = rig
        .call("editor.pie_net_entities", json!({ "client": 0 }))
        .expect("entities");
    let ids: Vec<u64> = list["entities"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| r["net_id"].as_u64().expect("a net id"))
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids[0] < ids[1], "{ids:?}");
    assert_eq!(list["total"], 2);
    assert!(list["last_applied_tick"].as_u64().is_some_and(|t| t > 0));
    let all = rig
        .call("editor.pie_net_entities", json!({ "client": 0, "all": true }))
        .expect("all");
    let local = all["entities"]
        .as_array()
        .expect("rows")
        .iter()
        .filter(|r| r["net_id"].is_null())
        .count();
    assert!(local > 0, "the level's Rock at least: {all}");
    let limited = rig
        .call("editor.pie_net_entities", json!({ "client": 0, "limit": 1 }))
        .expect("limit");
    assert_eq!(limited["count"], 1);
    assert_eq!(limited["total"], 2);

    assert_eq!(
        code(rig.call("editor.pie_net_entities", json!({ "client": 9 }))),
        "pie_net_client_unknown"
    );
    assert!(rig.call("editor.pie_net_entities", json!({})).is_err());
}

#[test]
fn moving_the_focus_releases_the_keys_held_in_the_old_client() {
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game()));
    rig.settle(2);
    rig.call("editor.pie_net_focus", json!({ "client": 0 }))
        .expect("focus");
    rig.call("editor.pie_net_key", json!({ "key": "KeyD" }))
        .expect("key");
    rig.frames(10);
    rig.call("editor.pie_net_focus", json!({ "client": 1 }))
        .expect("move the focus");
    rig.frames(5);
    let stopped = rig.client(0, own_x);
    let other = rig.client(1, own_x);
    rig.frames(10);
    assert!(
        (rig.client(0, own_x) - stopped).abs() < 1e-6,
        "client 0 no longer runs: the focus move released its key"
    );
    assert!(
        (rig.client(1, own_x) - other).abs() < 1e-6,
        "the release did not press anything in client 1"
    );
    // Refocusing the same client keeps a held key: nothing changed hands.
    rig.call("editor.pie_net_key", json!({ "key": "KeyD" }))
        .expect("key");
    rig.call("editor.pie_net_focus", json!({ "client": 1 }))
        .expect("same focus");
    rig.frames(10);
    assert!(rig.client(1, own_x) - other > 0.5, "client 1 runs");
}

/// One frame of the Net tab's client part in a headless egui context (no device: the view draws
/// its no-GPU path), with the pointer at `pointer` and these key events.
fn panel_frame(rig: &mut Rig, ctx: &egui::Context, pointer: egui::Pos2, keys: &[(egui::Key, bool)]) {
    panel_frame_in(rig, ctx, pointer, keys, true);
}

/// [`panel_frame`] with the editor window focused or not.
fn panel_frame_in(
    rig: &mut Rig,
    ctx: &egui::Context,
    pointer: egui::Pos2,
    keys: &[(egui::Key, bool)],
    window_focused: bool,
) {
    let mut events = vec![egui::Event::PointerMoved(pointer)];
    events.extend(keys.iter().map(|(key, pressed)| egui::Event::Key {
        key: *key,
        physical_key: None,
        pressed: *pressed,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }));
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 900.0),
        )),
        events,
        focused: window_focused,
        ..egui::RawInput::default()
    };
    let world = rig.edit.world_mut();
    let mut output = ctx.run_ui(input, |ui| clients_panel(ui, world, None));
    output.textures_delta.clear();
    rig.frame();
}

#[test]
fn the_net_tab_routes_keys_over_the_view_and_releases_them_when_the_pointer_leaves() {
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game()));
    rig.settle(2);
    let ctx = egui::Context::default();
    // Far below the picker and the numbers, inside the 16:9 view of an 800 wide panel.
    let over_view = egui::pos2(400.0, 300.0);
    let outside = egui::pos2(400.0, 880.0);
    panel_frame(&mut rig, &ctx, outside, &[]);
    let start = [rig.client(0, own_x), rig.client(1, own_x)];

    // No focus yet: keys over the view reach nobody.
    panel_frame(&mut rig, &ctx, over_view, &[(egui::Key::D, true)]);
    rig.frames(10);
    assert!(
        (rig.client(0, own_x) - start[0]).abs() < 1e-6,
        "no focus, no input"
    );
    panel_frame(&mut rig, &ctx, over_view, &[(egui::Key::D, false)]);

    // Focus on the picked client: the pressed key runs it and only it.
    rig.call("editor.pie_net_focus", json!({ "client": 0 }))
        .expect("focus");
    panel_frame(&mut rig, &ctx, over_view, &[(egui::Key::D, true)]);
    for _ in 0..20 {
        panel_frame(&mut rig, &ctx, over_view, &[]);
    }
    assert!(rig.client(0, own_x) - start[0] > 1.0, "the view's key arrived");
    assert!((rig.client(1, own_x) - start[1]).abs() < 1e-6);

    // The pointer leaves with the key still down: it must not stay held.
    panel_frame(&mut rig, &ctx, outside, &[]);
    rig.frames(5);
    let stopped = rig.client(0, own_x);
    rig.frames(10);
    assert!((rig.client(0, own_x) - stopped).abs() < 1e-6, "released on leave");

    // A key an agent holds is not the view's: drawing the tab with the pointer away keeps it.
    rig.call("editor.pie_net_key", json!({ "key": "KeyD" }))
        .expect("key");
    panel_frame(&mut rig, &ctx, outside, &[]);
    let before = rig.client(0, own_x);
    rig.frames(10);
    assert!(rig.client(0, own_x) - before > 0.5, "the agent's key stays down");
}

/// A key pressed over the view, the tab drawn this far: the avatar is moving.
fn hold_key_over_the_view(rig: &mut Rig, ctx: &egui::Context, over_view: egui::Pos2) {
    rig.call("editor.pie_net_focus", json!({ "client": 0 }))
        .expect("focus");
    let start = rig.client(0, own_x);
    // egui learns where the pointer is from the frame before.
    panel_frame(rig, ctx, over_view, &[]);
    panel_frame(rig, ctx, over_view, &[(egui::Key::D, true)]);
    for _ in 0..10 {
        panel_frame(rig, ctx, over_view, &[]);
    }
    assert!(rig.client(0, own_x) - start > 0.3, "the key is held and moving");
}

#[test]
fn a_key_held_over_the_view_is_released_when_the_tab_stops_drawing() {
    let mut rig = Rig::new(PieNetMode::Listen, 1, Some(game()));
    rig.settle(1);
    let ctx = egui::Context::default();
    hold_key_over_the_view(&mut rig, &ctx, egui::pos2(400.0, 300.0));
    // The Net tab is hidden with the key still down: no later frame draws the view.
    rig.frames(3);
    let stopped = rig.client(0, own_x);
    rig.frames(10);
    assert!(
        (rig.client(0, own_x) - stopped).abs() < 1e-6,
        "a hidden view holds no key"
    );
}

#[test]
fn a_key_held_over_the_view_is_released_when_the_window_loses_focus() {
    let mut rig = Rig::new(PieNetMode::Listen, 1, Some(game()));
    rig.settle(1);
    let ctx = egui::Context::default();
    let over_view = egui::pos2(400.0, 300.0);
    hold_key_over_the_view(&mut rig, &ctx, over_view);
    // Alt-tab: the pointer is still over the view and no release event will come.
    panel_frame_in(&mut rig, &ctx, over_view, &[], false);
    panel_frame_in(&mut rig, &ctx, over_view, &[], false);
    let stopped = rig.client(0, own_x);
    for _ in 0..10 {
        panel_frame_in(&mut rig, &ctx, over_view, &[], false);
    }
    assert!(
        (rig.client(0, own_x) - stopped).abs() < 1e-6,
        "an unfocused window holds no key"
    );
}

/// More frames than the silence timeout (10 s, 600 frames at 60 Hz): every link must keep talking
/// through the step, as it does through the same number of single frames.
#[test]
fn a_long_step_keeps_every_client_joined_and_in_step() {
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game()));
    rig.settle(2);
    rig.request(PieRequest::Pause);
    let before = rig.client_frames();
    rig.request(PieRequest::Step(700));
    rig.frame();
    let after = rig.client_frames();
    assert_eq!(
        rig.server().resource::<NetNode>().peers().len(),
        2,
        "nobody timed out"
    );
    for i in 0..2 {
        assert!(
            rig.client(i, |w| w.resource::<NetLink>().is_connected()),
            "client {i}"
        );
    }
    // Between frames a client is one frame behind the play world (the pump runs first): the frame
    // the play world had ahead when paused, plus the step.
    assert_eq!(after.len(), 2);
    assert_eq!(after[0], before[0] + 701, "client 1 stepped with the server");
    assert_eq!(after[1], before[1] + 701, "client 2 stepped with the server");
}

/// Client 1 holds `KeyD` through `frames` paused frames, stepped `chunk` at a time.
fn step_with_key_held(frames: u32, chunk: u32) -> (Vec<(u32, f32)>, f32, f32, u64, u64) {
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game()));
    rig.settle(2);
    rig.call("editor.pie_net_focus", json!({ "client": 0 }))
        .expect("focus");
    rig.call("editor.pie_net_key", json!({ "key": "KeyD" }))
        .expect("key");
    rig.frames(2);
    rig.request(PieRequest::Pause);
    for _ in 0..frames / chunk {
        rig.request(PieRequest::Step(chunk));
        rig.frame();
    }
    let server = server_x(rig.server());
    (
        server,
        rig.client(0, own_x),
        rig.client(1, own_x),
        rig.client(0, |w| tick(w)),
        rig.client(1, |w| tick(w)),
    )
}

#[test]
fn a_step_of_n_frames_is_the_same_as_n_single_steps() {
    let one_by_one = step_with_key_held(30, 1);
    assert_eq!(
        step_with_key_held(30, 30),
        one_by_one,
        "the same inputs, the same world"
    );
    let start = step_with_key_held(0, 1);
    assert!(
        one_by_one.1 - start.1 > 1.0,
        "the held key moved client 1 during the step"
    );
    assert!(
        one_by_one.0[0].1 - start.0[0].1 > 1.0,
        "and the server saw its input during the step"
    );
}

#[test]
fn a_replicated_entity_of_the_level_comes_from_the_server() {
    let mut rig = Rig::with_level(PieNetMode::Listen, 2, LEVEL_BEACON);
    rig.settle(2);
    rig.frames(10);
    let server = rig.server();
    let beacons = server
        .query_filtered::<&Name, With<Replicated>>()
        .iter(server)
        .filter(|n| n.as_str() == "Beacon")
        .count();
    assert_eq!(beacons, 1, "the play world holds it as a real replicated entity");
    for i in 0..2 {
        let names = rig.client(i, names);
        assert!(names.contains(&"Rock".to_owned()), "{names:?}");
        assert!(
            !names.contains(&"Beacon".to_owned()),
            "not the client's own copy of the level: {names:?}"
        );
        let seen: Vec<Vec3> = rig.client(i, |w| {
            w.query_filtered::<&Transform, (With<NetId>, Without<Avatar>)>()
                .iter(w)
                .map(|t| t.translation)
                .collect()
        });
        assert_eq!(
            seen,
            vec![Vec3::new(5.0, 6.0, 7.0)],
            "client {i} got it from the server"
        );
    }
}

/// The editor keeps a level's replicated entities (opening and saving it loses nothing), with or
/// without client worlds; its play world holds them as a shipped game's would.
#[test]
fn offline_play_keeps_a_levels_replicated_entity_and_makes_no_client_worlds() {
    let mut rig = Rig::with_level(PieNetMode::Offline, 2, LEVEL_BEACON);
    rig.frames(2);
    let edit = rig.edit.world_mut();
    assert_eq!(
        edit.query_filtered::<(), With<Replicated>>().iter(edit).count(),
        1
    );
    let scene = snapshot_scene(edit, &SnapshotFilter::default()).expect("snapshot");
    assert!(
        scene
            .entities
            .iter()
            .any(|e| e.components.contains_key("regia_net::replication::Replicated")),
        "a save keeps it"
    );
    let server = rig.server();
    assert_eq!(
        server
            .query_filtered::<(), With<Replicated>>()
            .iter(server)
            .count(),
        1
    );
    let listed = rig.call("editor.pie_net_clients", json!({})).expect("list");
    assert_eq!(listed["reason"], "offline");
}

#[test]
fn a_client_that_fails_to_build_leaves_the_play_world_as_it_was() {
    UNLOADS.with(|n| n.set(0));
    CLIENT_REGISTERS.with(|n| n.set(0));
    let mut rig = Rig::new(
        PieNetMode::Dedicated,
        3,
        Some(game_with(register_fails_in_the_second_client)),
    );
    rig.frames(2);
    assert_eq!(
        CLIENT_REGISTERS.with(Cell::get),
        2,
        "the second client failed, no third, no retry"
    );
    assert_eq!(
        UNLOADS.with(Cell::get),
        1,
        "the first client, which had loaded, unloaded"
    );
    let listed = rig.call("editor.pie_net_clients", json!({})).expect("list");
    assert_eq!(listed["running"], false);
    assert_eq!(listed["reason"], "pie_net_host_failed");
    let server = rig.server();
    assert!(
        server.resource::<NetLink>().status().is_none(),
        "nothing is hosted"
    );
    assert_eq!(
        *server.resource::<NetMode>(),
        NetMode::ListenServer,
        "the game's mode stands"
    );
}

#[test]
fn a_game_with_its_own_link_keeps_its_play_world_and_gets_no_clients() {
    UNLOADS.with(|n| n.set(0));
    CLIENT_REGISTERS.with(|n| n.set(0));
    let mut rig = Rig::new(
        PieNetMode::Dedicated,
        2,
        Some(game_with(register_with_its_own_link)),
    );
    rig.frames(2);
    assert_eq!(CLIENT_REGISTERS.with(Cell::get), 0, "no client world was started");
    assert_eq!(UNLOADS.with(Cell::get), 0);
    let listed = rig.call("editor.pie_net_clients", json!({})).expect("list");
    assert_eq!(listed["reason"], "pie_net_link_busy");
    let server = rig.server();
    assert_eq!(
        *server.resource::<NetMode>(),
        NetMode::ListenServer,
        "the game's mode stands"
    );
    assert!(
        server.resource::<NetLink>().status().is_some(),
        "the game's link still runs"
    );
}

#[test]
fn a_config_change_during_play_applies_at_the_next_play() {
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game()));
    rig.settle(2);
    for params in [
        json!({ "mode": "offline" }),
        json!({ "mode": "listen", "clients": 0 }),
    ] {
        rig.call("editor.pie_net_config", params).expect("config");
        let before = rig.client_frames();
        rig.frames(5);
        let after = rig.client_frames();
        assert_eq!(
            after,
            before.iter().map(|f| f + 5).collect::<Vec<_>>(),
            "the worlds keep running"
        );
        assert_eq!(rig.server().resource::<NetNode>().peers().len(), 2);
    }
    rig.call("editor.pie_net_config", json!({ "mode": "listen", "clients": 3 }))
        .expect("config");
    rig.frames(2);
    assert_eq!(rig.client_frames().len(), 2, "this session keeps its two");
    rig.request(PieRequest::Stop);
    rig.frame();
    rig.request(PieRequest::Play);
    rig.frame();
    rig.settle(3);
    assert_eq!(rig.client_frames().len(), 3, "the next session has three");

    // And the other way: a session that started offline stays without client worlds.
    let mut rig = Rig::new(PieNetMode::Offline, 2, Some(game()));
    rig.frames(2);
    rig.call("editor.pie_net_config", json!({ "mode": "listen" }))
        .expect("config");
    rig.frames(2);
    let listed = rig.call("editor.pie_net_clients", json!({})).expect("list");
    assert_eq!(listed["running"], false);
    assert_eq!(listed["reason"], "offline");
}

#[test]
fn restarting_play_ends_the_old_client_worlds_before_the_new_session_loads() {
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game()));
    rig.settle(2);
    GAME_LOG.with(|l| l.borrow_mut().clear());
    rig.request(PieRequest::Play);
    rig.frame();
    let log = GAME_LOG.with(|l| l.borrow().clone());
    assert_eq!(
        log,
        vec!["unload", "unload", "unload", "load", "load", "load"],
        "the old play world and clients end before anything of the new session loads"
    );
    rig.settle(2);
}

#[test]
fn leaving_play_ends_the_client_worlds() {
    UNLOADS.with(|n| n.set(0));
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game()));
    rig.settle(2);
    let listed = rig.call("editor.pie_net_clients", json!({})).expect("list");
    assert_eq!(listed["running"], true);
    assert_eq!(listed["clients"].as_array().map(Vec::len), Some(2));
    assert_eq!(listed["clients"][1]["label"], "client-2");
    assert!(
        listed["clients"][0]["last_applied_tick"]
            .as_u64()
            .is_some_and(|t| t > 0)
    );
    if let Some(session) = rig.pie.take() {
        stop_play(session);
    }
    rig.frame();
    assert!(rig.edit.world().get_non_send::<PieWorlds>().is_none());
    assert_eq!(
        UNLOADS.with(Cell::get),
        3,
        "on_unload ran in the play world (stop_play) and in each client world"
    );
    let listed = rig.call("editor.pie_net_clients", json!({})).expect("list");
    assert_eq!(listed["running"], false);
    assert_eq!(listed["reason"], "not_playing");
    assert_eq!(
        code(rig.call("editor.pie_net_focus", json!({ "client": 0 }))),
        "pie_net_no_client_worlds"
    );
    // The next play builds fresh worlds.
    rig.pie = Some(start_play(rig.edit.world_mut(), Some(game())).expect("play"));
    rig.frame();
    let listed = rig.call("editor.pie_net_clients", json!({})).expect("list");
    assert_eq!(listed["running"], true);
    assert_eq!(
        listed["clients"][0]["frames"], 0,
        "built this frame, stepped from the next"
    );
    rig.frame();
    let listed = rig.call("editor.pie_net_clients", json!({})).expect("list");
    assert_eq!(listed["clients"][0]["frames"], 1);
}

#[test]
fn paused_play_holds_the_clients_and_a_step_moves_them_with_the_server() {
    let mut rig = Rig::new(PieNetMode::Listen, 1, Some(game()));
    rig.settle(1);
    if let Some(session) = rig.pie.as_mut() {
        session.mode = crate::pie::PlayMode::Paused;
    }
    // The clients catch up the play world's last frame at the start of the next editor frame.
    rig.frame();
    let before = rig.client(0, |w| w.resource::<NetLink>().frames());
    rig.frames(10);
    assert_eq!(rig.client(0, |w| w.resource::<NetLink>().frames()), before);
    if let Some(session) = rig.pie.as_mut() {
        assert_eq!(session.step(3), None);
    }
    rig.frame();
    assert_eq!(
        rig.client(0, |w| w.resource::<NetLink>().frames()),
        before + 3,
        "the client steps as many frames as the play world did"
    );
}

#[test]
fn no_client_worlds_without_a_game_a_mode_or_clients() {
    for (mode, clients, game, reason) in [
        (PieNetMode::Listen, 2, None, "pie_net_no_game"),
        (PieNetMode::Offline, 2, Some(game()), "offline"),
        (PieNetMode::Listen, 0, Some(game()), "no_clients"),
    ] {
        let mut rig = Rig::new(mode, clients, game);
        rig.frames(3);
        let listed = rig.call("editor.pie_net_clients", json!({})).expect("list");
        assert_eq!(listed["running"], false, "{reason}");
        assert_eq!(listed["reason"], reason);
        assert_eq!(
            code(rig.call("editor.hierarchy", json!({ "client": 0 }))),
            "pie_net_no_client_worlds",
            "{reason}"
        );
    }
}

#[test]
fn the_client_parameter_routes_inspection_to_the_client_world() {
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game()));
    rig.settle(2);
    let edit = rig.call("editor.hierarchy", json!({})).expect("edit hierarchy");
    let client = rig
        .call("editor.hierarchy", json!({ "client": 1 }))
        .expect("client hierarchy");
    assert_eq!(client["client"], 1);
    // Names are not replicated: the avatars are found by entity id in the client's tree.
    let avatars: Vec<u64> = rig.client(1, |w| {
        w.query_filtered::<Entity, With<Avatar>>()
            .iter(w)
            .map(Entity::to_bits)
            .collect()
    });
    assert_eq!(avatars.len(), 2);
    let ids: Vec<u64> = client["roots"]
        .as_array()
        .map(|roots| roots.iter().filter_map(|r| r["entity"].as_u64()).collect())
        .unwrap_or_default();
    assert!(avatars.iter().all(|a| ids.contains(a)), "{avatars:?} in {client}");
    assert!(
        client.to_string().contains("Rock"),
        "the client has the level: {client}"
    );
    assert!(
        client["count"].as_u64() > edit["count"].as_u64(),
        "the client world holds the level and the replicated entities: {edit} vs {client}"
    );
    assert_eq!(
        code(rig.call("editor.hierarchy", json!({ "client": 9 }))),
        "pie_net_client_unknown"
    );
    assert_eq!(
        code(rig.call("editor.hierarchy", json!({ "client": "one" }))),
        "F0002",
        "a client that is not an index is an invalid parameter"
    );
}

#[test]
fn a_client_view_captures_headless_on_the_adapter() {
    let Ok(device) = crate::render::headless_device() else {
        eprintln!("no adapter: the client capture is checked on the lavapipe run");
        return;
    };
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game()));
    rig.edit.world_mut().insert_resource(device);
    rig.settle(2);
    let shot = rig
        .call(
            "editor.capture_viewport",
            json!({ "client": 1, "width": 64, "height": 36 }),
        )
        .expect("capture");
    assert_eq!(shot["client"], 1);
    assert_eq!(shot["width"], 64);
    assert_eq!(shot["height"], 36);
    assert!(shot["png_base64"].as_str().is_some_and(|s| s.len() > 100));
}

/// `editor.capture_viewport { client }`, decoded.
fn client_shot(rig: &mut Rig, client: u32) -> image::RgbaImage {
    let shot = rig
        .call(
            "editor.capture_viewport",
            json!({ "client": client, "width": 128, "height": 72 }),
        )
        .expect("capture");
    let bytes = crate::reference::base64_decode(shot["png_base64"].as_str().expect("png")).expect("base64");
    image::load_from_memory(&bytes).expect("png").to_rgba8()
}

fn mean_channel_delta(a: &image::RgbaImage, b: &image::RgbaImage) -> f64 {
    assert_eq!(a.dimensions(), b.dimensions());
    let sum: u64 = a
        .as_raw()
        .iter()
        .zip(b.as_raw())
        .map(|(x, y)| u64::from(x.abs_diff(*y)))
        .sum();
    sum as f64 / a.as_raw().len().max(1) as f64
}

/// What one client's world holds shows in that client's capture and in no other's.
#[test]
fn a_client_capture_shows_that_clients_world() {
    let Ok(device) = crate::render::headless_device() else {
        eprintln!("no adapter: the client capture is checked on the lavapipe run");
        return;
    };
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game()));
    rig.edit.world_mut().insert_resource(device);
    rig.settle(2);
    // A settled pair of captures per client, so what a first render still accumulates is out.
    let _ = (client_shot(&mut rig, 0), client_shot(&mut rig, 1));
    let before = [client_shot(&mut rig, 0), client_shot(&mut rig, 1)];
    rig.client(1, |w| {
        w.spawn((
            Name::new("OnlyClient2"),
            Transform {
                scale: Vec3::splat(4.0),
                ..Transform::from_translation(Vec3::new(0.0, 1.0, 0.0))
            },
            regia_render::Primitive {
                color: [1.0, 0.1, 0.1, 1.0],
                ..regia_render::Primitive::default()
            },
        ));
    });
    rig.frames(2);
    let after = [client_shot(&mut rig, 0), client_shot(&mut rig, 1)];
    let other = mean_channel_delta(&before[0], &after[0]);
    let own = mean_channel_delta(&before[1], &after[1]);
    assert!(own > 2.0, "client 2's capture shows its new entity: {own}");
    assert!(
        own > other * 10.0,
        "client 1's does not: own {own}, other {other}"
    );
}

#[test]
fn egui_keys_map_to_physical_names() {
    assert_eq!(physical_key_name(egui::Key::W), "KeyW");
    assert_eq!(physical_key_name(egui::Key::Num1), "Digit1");
    assert_eq!(physical_key_name(egui::Key::ArrowUp), "ArrowUp");
    assert_eq!(physical_key_name(egui::Key::Space), "Space");
    assert_eq!(physical_key_name(egui::Key::F5), "F5");
}

/// Set by the test's panic hook when a `client backtrace fixture` panic ran under the PIE
/// boundary (`crash_reports::with_pie_kind`, the condition that files the crash report as `pie`).
static CLIENT_PANIC_IN_PIE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[inline(never)]
fn client_backtrace_fixture_panics() {
    panic!("client backtrace fixture");
}

fn panicking_client_system() {
    client_backtrace_fixture_panics();
}

/// Clients panic on their first update; the play world serves as usual.
fn register_panicking_client(app: &mut App) {
    if app.world().get_resource::<NetMode>() == Some(&NetMode::Client) {
        app.add_systems(crate::engine::app::Update, panicking_client_system);
    } else {
        register(app);
    }
}

/// `specs/19` § 7 (ADR-7340, on ADR-7205's boundary): a several-client PIE world's panic runs
/// through `pie::catch_in_pie` like the play world's: the crash report is filed as `pie`, the
/// client keeps the backtrace taken while the stack existed, and its log text names the panicking
/// frame with a `file:line` link.
#[test]
fn a_client_system_panic_captures_a_backtrace_with_file_line_links() {
    // The PIE flag is process-wide: a crash-report test running meanwhile would file its own
    // panic as `pie`.
    let _guard = crate::crash_reports::tests::lock();
    CLIENT_PANIC_IN_PIE.store(false, std::sync::atomic::Ordering::SeqCst);
    // A recorder in the panic-hook chain (it stays, chained, like the capture hook does), chained
    // under the editor's hook lock: a bare take + set here dropped the capture hook when the
    // panic_trace tests installed it at the same moment (ADR-8847).
    crate::panic_trace::chain_hook(|previous| {
        Box::new(move |info| {
            if info.payload().downcast_ref::<&str>() == Some(&"client backtrace fixture") {
                CLIENT_PANIC_IN_PIE.store(
                    crate::crash_reports::tests::in_pie(),
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
            previous(info);
        })
    });
    let mut rig = Rig::new(PieNetMode::Listen, 2, Some(game_with(register_panicking_client)));
    rig.frames(2);
    let listed = rig.call("editor.pie_net_clients", json!({})).expect("list");
    assert!(
        listed["clients"][0]["panic"].is_string(),
        "the client world stopped with its panic: {listed}"
    );
    assert!(
        CLIENT_PANIC_IN_PIE.load(std::sync::atomic::Ordering::SeqCst),
        "the client panic ran under the PIE boundary (the hook saw the pie kind)"
    );
    let worlds = rig.edit.world().get_non_send::<PieWorlds>().expect("worlds");
    let client = &worlds.clients[0];
    let trace = client
        .panic_backtrace
        .as_deref()
        .expect("the client kept the backtrace of its panic");
    assert!(
        trace.contains("client_backtrace_fixture_panics"),
        "the panicking frame is in it:\n{trace}"
    );
    let caught = crate::panic_trace::CaughtPanic {
        message: client.panic.clone().unwrap_or_default(),
        backtrace: client.panic_backtrace.clone(),
    };
    // A file:line needs the build's line tables (the workspace dev and test profiles keep them,
    // ADR-7449). A build with debuginfo 0 (the build server pregate's `CARGO_PROFILE_TEST_DEBUG=0`)
    // symbolizes frames by name only, so this test's own backtrace is the control: when it
    // has no file:line for this file either, the build cannot show one and the link check is
    // left to the line-tables builds (the merge gate's). The frame check above still runs.
    let control = std::backtrace::Backtrace::force_capture().to_string();
    if !control.contains("pie_worlds/tests.rs:") && !control.contains("pie_worlds\\tests.rs:") {
        eprintln!(
            "no line tables in this build (debuginfo 0): the file:line link is checked by a \
             line-tables build"
        );
        return;
    }
    let links = crate::log_links::parse_links(&caught.log_text());
    assert!(
        links.iter().any(|l| l.path().ends_with("tests.rs")
            && matches!(l.target, crate::log_links::LinkTarget::File { .. })),
        "the panic location is a file:line link: {}",
        caught.log_text()
    );
}

/// Memory and step cost per client (ADR-2623 item 4). Prints one JSON line per client count;
/// numbers are reported, never asserted. Run on the lavapipe host:
/// `cargo test -p regia_editor --lib pie_worlds::tests::measure_client_cost -- --ignored --nocapture`.
///
/// Compare RSS only between fresh processes: later counts in one process reuse the memory the
/// earlier rigs freed and the renderers they cached, which hides the cost. Set
/// `REGIA_PIE_MEASURE_CLIENTS=n` to run one count, and read the slope between counts (the first
/// client also pays for one-time set-up).
#[test]
#[ignore = "measurement, prints numbers for the report"]
fn measure_client_cost() {
    fn rss_bytes() -> u64 {
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()))
            .map_or(0, |pages| pages * 4096)
    }
    let device = crate::render::headless_device().ok();
    let counts: Vec<u32> = std::env::var("REGIA_PIE_MEASURE_CLIENTS")
        .ok()
        .and_then(|n| n.parse().ok())
        .map_or_else(|| vec![1, 2, 4, 8], |n| vec![n]);
    for clients in counts {
        let base = rss_bytes();
        let mut rig = Rig::new(PieNetMode::Listen, clients, Some(game()));
        let settle = rig.settle(clients as usize);
        rig.frames(120);
        let after = rss_bytes();
        let listed = rig.call("editor.pie_net_clients", json!({})).expect("list");
        let mean_us: Vec<u64> = listed["clients"]
            .as_array()
            .map(|a| a.iter().filter_map(|c| c["mean_update_us"].as_u64()).collect())
            .unwrap_or_default();
        let entities: Vec<u64> = listed["clients"]
            .as_array()
            .map(|a| a.iter().filter_map(|c| c["entities"].as_u64()).collect())
            .unwrap_or_default();
        let mut render_us = Vec::new();
        let mut rss_per_view = Vec::new();
        if let Some(device) = &device {
            rig.edit.world_mut().insert_resource(device.clone());
            for i in 0..clients {
                // The first capture builds the renderer; the second is the steady cost.
                for _ in 0..2 {
                    let _ = rig.call(
                        "editor.capture_viewport",
                        json!({ "client": i, "width": 640, "height": 360 }),
                    );
                }
                if let Ok(shot) = rig.call(
                    "editor.capture_viewport",
                    json!({ "client": i, "width": 640, "height": 360 }),
                ) {
                    render_us.push(shot["render_us"].as_u64().unwrap_or(0));
                }
                rss_per_view.push(rss_bytes().saturating_sub(base) / (1 << 20));
            }
        }
        let rss_after_views = rss_bytes();
        println!(
            "{}",
            json!({
                "clients": clients,
                "settle_frames": settle,
                "rss_delta_bytes": after.saturating_sub(base),
                "rss_delta_per_client": after.saturating_sub(base) / u64::from(clients),
                "rss_delta_with_views": rss_after_views.saturating_sub(base),
                "entities": entities,
                "mean_step_us": mean_us,
                "capture_640x360_us": render_us,
                "rss_delta_mib_after_each_view": rss_per_view,
                "adapter": device.as_ref().map(|d| d.adapter.get_info().name),
            })
        );
    }
}

/// The network view switch (ADR-7829): the Outliner and Inspector show the viewed role's world, the
/// play tint uses that role's theme color, focusing a client's input views it, and the switch
/// answers by name.
#[test]
fn the_view_switch_shows_the_chosen_world_and_tints_with_its_role_color() {
    let mut rig = Rig::new(PieNetMode::Dedicated, 2, Some(game()));
    rig.settle(2);
    crate::pie::sync_status(rig.edit.world_mut(), rig.pie.as_ref());
    let accents = crate::theme::ThemeTokens::default().accents;
    assert_eq!(viewed_role(rig.edit.world()), Some(PieViewRole::Server));
    assert_eq!(
        crate::pie_controls::play_tint(rig.edit.world()),
        Some(accents.play_server)
    );
    let viewed_mode = |rig: &mut Rig| {
        let mut seen = None;
        let play = rig.pie.as_mut().map(|s| s.app.world_mut());
        with_viewed_world(rig.edit.world_mut(), play, |world| {
            seen = world.get_resource::<NetMode>().copied();
        });
        seen
    };
    assert_eq!(
        viewed_mode(&mut rig),
        Some(NetMode::DedicatedServer),
        "the panels show the server"
    );

    let answer = rig
        .call("editor.pie_view", json!({ "role": "client", "client": 1 }))
        .expect("view client 2");
    assert_eq!(answer["label"], "Client 2");
    assert_eq!(answer["clients"], 2);
    assert_eq!(
        viewed_mode(&mut rig),
        Some(NetMode::Client),
        "the panels show client 2"
    );
    assert_eq!(
        crate::pie_controls::play_tint(rig.edit.world()),
        Some(accents.play_clients[1])
    );
    assert_eq!(
        code(rig.call("editor.pie_view", json!({ "role": "spectator" }))),
        "pie_view_role_unknown"
    );
    assert_eq!(
        code(rig.call("editor.pie_view", json!({ "role": "client", "client": 2 }))),
        "pie_net_client_unknown"
    );
    // Focusing client 1's input views it.
    rig.call("editor.pie_net_focus", json!({ "client": 0 }))
        .expect("focus");
    assert_eq!(viewed_role(rig.edit.world()), Some(PieViewRole::Client(0)));
    let read = rig.call("editor.pie_view", json!({})).expect("read");
    assert_eq!(read["role"], "client");
    assert_eq!(read["client"], 0);
    rig.call("editor.pie_view", json!({ "role": "server" }))
        .expect("server");
    assert_eq!(viewed_mode(&mut rig), Some(NetMode::DedicatedServer));

    // Play stops: one world again.
    rig.request(PieRequest::Stop);
    rig.frame();
    assert_eq!(viewed_role(rig.edit.world()), None);
    assert_eq!(
        code(rig.call("editor.pie_view", json!({ "role": "server" }))),
        "pie_net_no_client_worlds"
    );
}

/// Keyboard path (accessibility bar): Tab reaches a client's view button and Space views it.
#[test]
fn the_view_switch_works_from_the_keyboard() {
    use crate::canvas_keys::test_input::{frame, key, tab_to};
    let mut rig = Rig::new(PieNetMode::Dedicated, 2, Some(game()));
    rig.settle(2);
    let ctx = egui::Context::default();
    let size = [900.0, 120.0];
    let world = rig.edit.world_mut();
    let mut draw = |ui: &mut egui::Ui| {
        ui.horizontal(|ui| view_switch(ui, world));
    };
    tab_to(&ctx, size, "Client 2", false, 6, &mut draw);
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Space, egui::Modifiers::NONE)],
        &mut draw,
    );
    drop(draw);
    assert_eq!(viewed_role(rig.edit.world()), Some(PieViewRole::Client(1)));
}
