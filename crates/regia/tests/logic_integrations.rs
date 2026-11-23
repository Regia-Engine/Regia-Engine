// SPDX-License-Identifier: MPL-2.0
//! Spec 102 § 13 P3 acceptance in the umbrella crate (ADR-7268): one logic graph per integration,
//! run through `AiPlugin` + `regia::logic_bridge::LogicBridgePlugin`. Fixed steps and counters only.
//!
//! Run: `cargo test -p regia --features logic,lua,net --test logic_integrations`.
#![cfg(feature = "logic")]

use regia::ai::logic::{LogicGraphStatus, LogicWorldBlackboards};
use regia::ai::{AiPlugin, BbValue, Blackboard, BtTaskRegistry, LogicGraph, LogicGraphDef, LogicInbox};
use regia::logic_bridge::LogicBridgePlugin;
use regia_app::prelude::*;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AiPlugin)
        .add_plugins(LogicBridgePlugin);
    app
}

fn spawn(app: &mut App, text: &str) -> Entity {
    let def = LogicGraphDef::from_ron(text).expect("parses");
    let graph = LogicGraph::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles");
    app.world_mut()
        .spawn((graph, Blackboard::new(), LogicInbox::default()))
        .id()
}

fn step(app: &mut App) {
    app.world_mut().run_schedule(FixedUpdate);
}

fn emit(app: &mut App, e: Entity, name: &str) {
    app.world_mut()
        .get_mut::<LogicInbox>(e)
        .expect("inbox")
        .push(name);
}

fn fires(app: &App, e: Entity, rule: &str) -> u64 {
    app.world()
        .get::<LogicGraphStatus>(e)
        .and_then(|s| s.rules.iter().find(|r| r.name == rule).map(|r| r.fires))
        .expect("rule status")
}

fn local(app: &App, e: Entity, name: &str) -> Option<BbValue> {
    app.world()
        .get::<LogicGraph>(e)
        .and_then(|g| g.locals().get(name).cloned())
}

fn trace(app: &App, e: Entity) -> Vec<String> {
    app.world()
        .get::<LogicGraphStatus>(e)
        .map(|s| s.trace.clone())
        .unwrap_or_default()
}

// ---- P3.4 world scope, through the app ----------------------------------------------------------------------

#[test]
fn p3_4_world_scope_runs_through_the_ai_plugin() {
    let mut app = app();
    let a = spawn(
        &mut app,
        r#"(format_version: 1, world: ["gatehouse"], rules: [(name: "raise", event: EntityEvent("go"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("world/gatehouse/alarm")), "value": Literal(Bool(true)) })])])"#,
    );
    let b = spawn(
        &mut app,
        r#"(format_version: 1, world: ["gatehouse"], locals: [(name: "n", default: Int(0))], rules: [
            (name: "respond", event: BlackboardChanged(key: "world/gatehouse/alarm"), condition: Key("world/gatehouse/alarm"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) })])])"#,
    );
    step(&mut app);
    emit(&mut app, a, "go");
    step(&mut app);
    step(&mut app);
    assert_eq!(local(&app, b, "n"), Some(BbValue::Int(1)));
    assert_eq!(
        app.world()
            .resource::<LogicWorldBlackboards>()
            .get("gatehouse")
            .and_then(|g| g.get("alarm")),
        Some(&BbValue::Bool(true))
    );
}

// ---- P3 input actions ------------------------------------------------------------------------------------------

#[test]
fn p3_an_input_action_of_the_possessed_entity_raises_its_arm() {
    use regia::framework::player::PlayerController;
    use regia_input::{ActionKind, InputConfig, InputInjection, InputPlugin};
    let mut app = app();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(50),
    ))
    .add_plugins(InputPlugin::new(
        InputConfig::new().action("use", ActionKind::Button),
    ));
    let pawn = spawn(
        &mut app,
        r#"(format_version: 1, locals: [(name: "n", default: Int(0))], rules: [
            (name: "use", event: InputAction(action: "use"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) })])])"#,
    );
    // Another graph that no controller possesses never hears the action.
    let other = spawn(
        &mut app,
        r#"(format_version: 1, rules: [(name: "use", event: InputAction(action: "use"))])"#,
    );
    app.world_mut().spawn(PlayerController {
        player_index: 0,
        possessed: pawn,
        view_target: pawn,
        context_stack: Vec::new(),
    });
    app.update();
    // Held over frames 3 and 4 (several fixed steps): one press edge.
    app.world_mut()
        .resource_mut::<InputInjection>()
        .schedule("use", regia_core::math::Vec3::X, 3, 2);
    for _ in 0..6 {
        app.update();
    }
    assert_eq!(
        fires(&app, pawn, "use"),
        1,
        "one press, one event: {:?}",
        trace(&app, pawn)
    );
    assert_eq!(local(&app, pawn, "n"), Some(BbValue::Int(1)));
    assert_eq!(fires(&app, other, "use"), 0);
}

// ---- P3.1 sequencer, P3.2 audio (client) -----------------------------------------------------------------------

#[cfg(feature = "client")]
#[test]
fn p3_1_a_rule_starts_a_sequence_on_a_bound_target_and_its_markers_come_back() {
    use regia::framework::sequencer::{
        Binding, Sequence, SequenceBindings, SequenceLibrary, SequencePlayer, SequencerPlugin,
    };
    use regia::logic_bridge::LogicSequenceSource;
    use std::time::Duration;
    let mut app = app();
    app.add_plugins((
        regia_scene::ScenePlugin,
        regia_input::InputPlugin::new(regia_input::InputConfig::new()),
        SequencerPlugin,
    ))
    .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    let sequence = Sequence::from_ron(
        r#"(version: 5, name: "door", markers: [(name: "creak", time: 0.15), (name: "slam", time: 0.35)])"#,
    )
    .expect("sequence parses");
    app.world_mut()
        .get_resource_or_init::<SequenceLibrary>()
        .insert("door.seq.ron", sequence);
    let door = app.world_mut().spawn_empty().id();
    let graph = spawn(
        &mut app,
        r#"(format_version: 1, locals: [(name: "creaks", default: Int(0))], rules: [
            (name: "open", event: EntityEvent("open"), actions: [
                (task: "logic::PlaySequence", set: { "sequence": Literal(String("door.seq.ron")), "target": BbRef("door"), "bind": Literal(String("door")) })]),
            (name: "creaked", event: OnSequenceMarker(sequence: "door.seq.ron", marker: "creak"), actions: [
                (task: "logic::SetBlackboard", set: { "key": Literal(String("local/creaks")), "op": Literal(String("increment")) })]),
            (name: "slammed", event: OnSequenceMarker(sequence: "door.seq.ron", marker: "slam")),
        ])"#,
    );
    app.world_mut()
        .get_mut::<Blackboard>(graph)
        .expect("bb")
        .insert("door", door);
    app.update();
    emit(&mut app, graph, "open");
    for _ in 0..12 {
        app.update();
    }
    let mut players = app
        .world_mut()
        .query::<(&SequencePlayer, &SequenceBindings, &LogicSequenceSource)>();
    let started: Vec<_> = players
        .iter(app.world())
        .map(|(_, b, s)| (b.get("door"), s.clone()))
        .collect();
    assert_eq!(started.len(), 1, "one player started");
    assert_eq!(started[0].0, Some(Binding::Entity(door)), "bound to the target");
    assert_eq!(started[0].1.graph, graph);
    assert_eq!(fires(&app, graph, "creaked"), 1, "{:?}", trace(&app, graph));
    assert_eq!(fires(&app, graph, "slammed"), 1, "{:?}", trace(&app, graph));
    assert_eq!(local(&app, graph, "creaks"), Some(BbValue::Int(1)));
}

#[cfg(feature = "client")]
#[test]
fn p3_2_play_cue_plays_the_named_cue_through_the_cue_path() {
    use regia::audio::{Audio, AudioSettings, CueEntry, SoundAsset, SoundClip, SoundCue};
    let mut app = app();
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..Default::default()
    });
    let clip = SoundClip::new(vec![0.1_f32; 4800], 48_000).expect("clip");
    audio.register_cue(SoundCue::new(
        "creak",
        vec![CueEntry {
            asset: SoundAsset::from_clip(&clip),
            weight: 1.0,
        }],
    ));
    app.insert_resource(audio);
    let graph = spawn(
        &mut app,
        r#"(format_version: 1, rules: [
            (name: "by_path", event: EntityEvent("a"), actions: [(task: "logic::PlayCue", set: { "cue": Literal(String("audio/door/creak.cue.ron")) })]),
            (name: "by_name", event: EntityEvent("b"), actions: [(task: "logic::PlayCue", set: { "cue": Literal(String("creak")) })]),
            (name: "missing", event: EntityEvent("c"), actions: [(task: "logic::PlayCue", set: { "cue": Literal(String("nothing")) })]),
        ])"#,
    );
    step(&mut app);
    let before = app.world().resource::<Audio>().played_count();
    emit(&mut app, graph, "a");
    emit(&mut app, graph, "b");
    emit(&mut app, graph, "c");
    step(&mut app);
    assert_eq!(app.world().resource::<Audio>().played_count(), before + 2);
    let failed: Vec<String> = trace(&app, graph)
        .into_iter()
        .filter(|l| l.contains("action_failed"))
        .collect();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(failed[0].contains("missing 0 logic::PlayCue"), "{failed:?}");
    let registry = app.world().resource::<BtTaskRegistry>();
    assert!(registry.is_presentation("logic::PlayCue"));
    assert!(registry.is_presentation("logic::PlaySequence"));
}

// ---- P3.3 Lua ---------------------------------------------------------------------------------------------------

#[cfg(feature = "lua")]
mod lua {
    use super::*;
    use regia::logic_bridge::{LogicLuaUnrouted, LogicModHost};
    use regia::lua::{EcsModWorld, LuaLimits, LuaMods, ModFiles, ModStatus};

    const MAIN: &str = r#"
        function door(args)
            regia.emit("door_called", args.speed * 2)
            if args.speed > 5 then return false end
            return true
        end
        function boom(args) error("kaboom") end
        function ping(args)
            regia.emit("pinged", 1, args.who)
            regia.emit("mine", 2)
            regia.emit("stray", 3, args.nobody)
        end
    "#;

    fn lua_app() -> App {
        let mut app = app();
        let files = ModFiles::from_map([
            (
                "luamod.ron",
                r#"(format_version: 1, id: "doors", name: "Doors", version: "1.0.0", entry: "main.lua", api_version: 1)"#,
            ),
            ("main.lua", MAIN),
        ]);
        let mut mods = LuaMods::new(7, LuaLimits::default());
        mods.load(files, &mut EcsModWorld::new(app.world_mut()))
            .expect("the mod loads");
        app.insert_resource(mods);
        app
    }

    #[test]
    fn p3_3_call_lua_passes_its_set_map_and_a_lua_error_fails_only_the_action() {
        let mut app = lua_app();
        let graph = spawn(
            &mut app,
            r#"(format_version: 1, locals: [(name: "after", default: Int(0))], rules: [
                (name: "slow", event: EntityEvent("slow"), actions: [
                    (task: "logic::CallLua", set: { "mod": Literal(String("doors")), "function": Literal(String("door")), "speed": Literal(Int(2)) }),
                    (task: "logic::SetBlackboard", set: { "key": Literal(String("local/after")), "op": Literal(String("increment")) })]),
                (name: "fast", event: EntityEvent("fast"), actions: [
                    (task: "logic::CallLua", set: { "mod": Literal(String("doors")), "function": Literal(String("door")), "speed": Literal(Int(9)) }),
                    (task: "logic::SetBlackboard", set: { "key": Literal(String("local/after")), "op": Literal(String("increment")) })]),
                (name: "boom", event: EntityEvent("boom"), actions: [
                    (task: "logic::CallLua", set: { "mod": Literal(String("doors")), "function": Literal(String("boom")) })]),
            ])"#,
        );
        step(&mut app);
        emit(&mut app, graph, "slow");
        emit(&mut app, graph, "fast");
        emit(&mut app, graph, "boom");
        step(&mut app);
        assert_eq!(
            local(&app, graph, "after"),
            Some(BbValue::Int(1)),
            "true succeeds, false fails"
        );
        let lines = trace(&app, graph);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("action_failed fast 0 logic::CallLua")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("action_failed boom 0 logic::CallLua")),
            "{lines:?}"
        );
        let mods = app.world().resource::<LuaMods>();
        assert_eq!(
            mods.get("doors").map(|m| m.status().clone()),
            Some(ModStatus::Active),
            "a Lua error leaves the mod enabled"
        );
        // The set map reached Lua as a table: the emitted value is speed * 2 (drained by the route
        // into the unrouted list, as no graph hosts the mod).
        step(&mut app);
        let unrouted = app.world().resource::<LogicLuaUnrouted>();
        let values: Vec<_> = unrouted
            .events
            .iter()
            .filter(|e| e.name == "door_called")
            .map(|e| e.value.clone())
            .collect();
        assert_eq!(
            values,
            [
                regia::lua::ScriptValue::Number(4.0),
                regia::lua::ScriptValue::Number(18.0)
            ]
        );
        // The graph still runs.
        emit(&mut app, graph, "slow");
        step(&mut app);
        assert_eq!(fires(&app, graph, "slow"), 2);
    }

    #[test]
    fn p3_3_regia_emit_with_a_target_reaches_that_graph_and_without_one_the_mod_host() {
        let mut app = lua_app();
        let target = spawn(
            &mut app,
            r#"(format_version: 1, rules: [(name: "pinged", event: EntityEvent("pinged"))])"#,
        );
        let host = spawn(
            &mut app,
            r#"(format_version: 1, rules: [(name: "mine", event: EntityEvent("mine")), (name: "pinged", event: EntityEvent("pinged"))])"#,
        );
        app.world_mut().entity_mut(host).insert(LogicModHost {
            mod_id: "doors".into(),
        });
        let caller = spawn(
            &mut app,
            r#"(format_version: 1, rules: [(name: "call", event: EntityEvent("go"), actions: [
                (task: "logic::CallLua", set: { "mod": Literal(String("doors")), "function": Literal(String("ping")), "who": BbRef("who"), "nobody": BbRef("nobody") })])])"#,
        );
        app.world_mut()
            .get_mut::<Blackboard>(caller)
            .expect("bb")
            .insert("who", target);
        // An entity without a logic graph: an event aimed at it is kept for the game.
        let nobody = app.world_mut().spawn_empty().id();
        app.world_mut()
            .get_mut::<Blackboard>(caller)
            .expect("bb")
            .insert("nobody", nobody);
        step(&mut app);
        emit(&mut app, caller, "go");
        step(&mut app); // the call emits; the route drains before the next AI step
        step(&mut app);
        assert_eq!(fires(&app, target, "pinged"), 1, "targeted");
        assert_eq!(
            fires(&app, host, "pinged"),
            0,
            "a targeted event goes to its target only"
        );
        assert_eq!(fires(&app, host, "mine"), 1, "untargeted: the mod's host entity");
        let unrouted = app.world().resource::<LogicLuaUnrouted>();
        assert!(
            unrouted
                .events
                .iter()
                .all(|e| e.name != "mine" && e.name != "pinged")
        );
        assert!(
            unrouted
                .events
                .iter()
                .any(|e| e.name == "stray" && e.target == Some(nobody.to_bits())),
            "an event aimed at an entity without a graph stays for the game"
        );
        // ADR-7268 addendum (intended v1 behaviour): only the event name crosses into the inbox —
        // the routed values (1, 2) are dropped, not written to any blackboard. The mod's Lua state
        // stays the source of truth; the game reads a value with `LuaMods::call_function` or from
        // `LogicLuaUnrouted` (unrouted events keep theirs, asserted above).
        let target_bb = app.world().get::<Blackboard>(target).expect("bb");
        assert!(
            target_bb.try_get_int("pinged").is_none() && target_bb.try_get_float("pinged").is_none(),
            "a routed Lua event stamps no payload key into the target blackboard"
        );
        let host_bb = app.world().get::<Blackboard>(host).expect("bb");
        assert!(
            host_bb.try_get_int("mine").is_none() && host_bb.try_get_float("mine").is_none(),
            "an untargeted routed Lua event stamps no payload key either"
        );
    }
}

// ---- P3.5 networking ---------------------------------------------------------------------------------------------

#[cfg(feature = "net")]
mod net {
    use super::*;
    use regia::logic_bridge::{net_logic_authority, route_net_messages};
    use regia::net::{NetRole, NetTick};

    /// The graph text uses the client-only `logic::PlayCue`, so this test needs `client` as well
    /// as `net` (a server-only `net` build registers no `PlayCue` and `spawn` fails with
    /// `unknown_task`; ADR-7268, review fixrev8).
    #[cfg(all(feature = "net", feature = "client"))]
    #[test]
    fn p3_5_on_a_proxy_sim_actions_skip_and_presentation_actions_run() {
        let mut app = app();
        // The plugin installed the NetRole-based authority.
        assert!(app.world().contains_resource::<regia::ai::LogicAuthority>());
        let text = r#"(format_version: 1, locals: [(name: "n", default: Int(0))], rules: [
            (name: "hit", event: EntityEvent("hit"), actions: [
                (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) }),
                (task: "logic::PlayCue", set: { "cue": Literal(String("creak")) })])])"#;
        let authority = spawn(&mut app, text);
        let proxy = spawn(&mut app, text);
        let local_only = spawn(&mut app, text);
        app.world_mut().entity_mut(authority).insert(NetRole::Authority);
        app.world_mut().entity_mut(proxy).insert(NetRole::Proxy);
        assert!(net_logic_authority(authority, app.world()));
        assert!(!net_logic_authority(proxy, app.world()));
        assert!(net_logic_authority(local_only, app.world()), "no role: local");
        step(&mut app);
        for e in [authority, proxy, local_only] {
            emit(&mut app, e, "hit");
        }
        step(&mut app);
        assert_eq!(local(&app, authority, "n"), Some(BbValue::Int(1)));
        assert_eq!(
            local(&app, proxy, "n"),
            Some(BbValue::Int(0)),
            "a proxy skips the sim action"
        );
        assert_eq!(local(&app, local_only, "n"), Some(BbValue::Int(1)));
        // The presentation action ran on the proxy too: without an `Audio` resource it fails, which
        // the trace records (a skipped action would leave no line).
        #[cfg(feature = "client")]
        for e in [authority, proxy] {
            assert!(
                trace(&app, e)
                    .iter()
                    .any(|l| l.contains("action_failed hit 1 logic::PlayCue")),
                "{:?}",
                trace(&app, e)
            );
        }
    }

    /// A test message (spec 70 § 6): the door bell, with the stable id `Ring`.
    #[derive(Debug, Clone, PartialEq, regia_net::Replicate)]
    struct Ring {
        loud: bool,
    }
    impl regia_net::NetMessage for Ring {
        const DIRECTION: regia_net::Direction = regia_net::Direction::ToServer;
        fn message_id() -> &'static str {
            "Ring"
        }
    }
    impl regia_net::ToServerMessage for Ring {}

    /// Ticks every logic graph of `world` once (the harness has no `App`, so no `AiPlugin`): the
    /// inbox moves into the graph, then one tick of one fixed step.
    fn tick_graphs(world: &mut World) {
        let mut query = world.query_filtered::<Entity, With<LogicGraph>>();
        let entities: Vec<Entity> = query.iter(world).collect();
        for e in entities {
            let Some(mut graph) = world.entity_mut(e).take::<LogicGraph>() else {
                continue;
            };
            let events = world
                .get_mut::<LogicInbox>(e)
                .map(|mut inbox| std::mem::take(&mut inbox.events))
                .unwrap_or_default();
            for event in events {
                graph.send_event(event);
            }
            let mut ctx = regia::ai::BtContext {
                world,
                entity: e,
                dt: 1.0 / 60.0,
            };
            graph.tick(&mut ctx, 1);
            world.entity_mut(e).insert(graph);
        }
    }

    /// Spec 70 § 10 test 1's harness: offline and listen-host runs of one graph that reacts to a
    /// net message give the same trace, tick by tick.
    #[test]
    fn p3_5_graph_traces_match_offline_and_on_the_listen_host() {
        use regia_ecs::ecs::system::RunSystemOnce;
        use regia_net::testing::LoopbackHub;
        use regia_net::{NetMode, NetSender, Replicated};
        fn setup(world: &mut World) {
            regia_net::message::register_net_message::<Ring>(world);
        }
        fn run(mode: NetMode) -> Vec<String> {
            let mut hub = LoopbackHub::new(mode, 7, setup);
            if mode == NetMode::ListenServer {
                hub.add_client();
            }
            let registry = {
                let mut r = BtTaskRegistry::default();
                regia::logic_bridge::register_tasks(&mut r);
                r
            };
            let def = LogicGraphDef::from_ron(
                r#"(format_version: 1, locals: [(name: "n", default: Int(0))], rules: [
                    (name: "rang", event: OnNetMessage(message: "Ring"), condition: Compare(key: "net_message_tick", op: Gt, value: 0.0), actions: [
                    (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) })])])"#,
            )
            .expect("parses");
            let graph = LogicGraph::compile(&def, &registry).expect("compiles");
            let s = &mut hub.server;
            s.gameplay
                .add_systems((route_net_messages::<Ring>, tick_graphs).chain());
            let door = s
                .world
                .spawn((
                    Replicated::default(),
                    graph,
                    Blackboard::new(),
                    LogicInbox::default(),
                ))
                .id();
            for step in 0..12 {
                if step == 3 || step == 7 {
                    hub.server
                        .world
                        .run_system_once(move |mut net: NetSender| {
                            net.send_to_server(door, Ring { loud: step == 7 })
                        })
                        .expect("system runs")
                        .expect("host rings");
                }
                hub.step();
            }
            hub.server
                .world
                .get::<LogicGraph>(door)
                .map(|g| g.trace().collect())
                .unwrap_or_default()
        }
        let offline = run(NetMode::Offline);
        let listen = run(NetMode::ListenServer);
        let fired: Vec<&String> = offline
            .iter()
            .filter(|l| l.contains("rule_matched rang"))
            .collect();
        assert_eq!(fired.len(), 2, "{offline:?}");
        assert_eq!(offline, listen, "the same packets give the same graph trace");
    }

    #[test]
    fn p3_5_applied_net_messages_reach_the_target_graph_in_application_order() {
        use regia::net::NetMessages;
        use regia::net::message::Received;
        use regia::net::session::PeerId;
        let run = |order: &[(u32, bool)]| -> (Vec<String>, i64) {
            let mut world = World::new();
            let registry = {
                let mut r = BtTaskRegistry::default();
                regia::logic_bridge::register_tasks(&mut r);
                r
            };
            let def = LogicGraphDef::from_ron(
                r#"(format_version: 1, locals: [(name: "n", default: Int(0))], rules: [
                    (name: "rang", event: OnNetMessage(message: "Ring"), actions: [
                    (task: "logic::SetBlackboard", set: { "key": Literal(String("local/n")), "op": Literal(String("increment")) })])])"#,
            )
            .expect("parses");
            let graph = LogicGraph::compile(&def, &registry).expect("compiles");
            let door = world
                .spawn((graph, Blackboard::new(), LogicInbox::default()))
                .id();
            let mut messages = NetMessages::<Ring>::default();
            for &(tick, loud) in order {
                messages.push(Received {
                    msg: Ring { loud },
                    target: Some(door),
                    from: PeerId(1),
                    sent_tick: NetTick(tick),
                    late: false,
                });
            }
            world.insert_resource(messages);
            route_net_messages::<Ring>(&mut world);
            let inbox = world.get::<LogicInbox>(door).expect("inbox").events.clone();
            let tick = world
                .get::<Blackboard>(door)
                .expect("bb")
                .get_int("net_message_tick");
            (inbox, tick)
        };
        let (inbox, tick) = run(&[(3, false), (5, true)]);
        assert_eq!(inbox, ["net_message:Ring", "net_message:Ring"]);
        assert_eq!(
            tick, 5,
            "the last applied message's tick, written before the events"
        );
        assert_eq!(
            run(&[(3, false), (5, true)]),
            run(&[(3, false), (5, true)]),
            "same packets, same result"
        );
    }
}

// ---- One graph, every integration, through the agent dispatcher ------------------------------------------------

#[cfg(all(feature = "agent", feature = "client", feature = "lua", feature = "net"))]
mod dispatcher {
    use super::*;
    use crossbeam_channel::Sender;
    use regia::agent::protocol::Request;
    use regia::agent::server::Inbound;
    use regia::audio::{Audio, AudioSettings, CueEntry, SoundAsset, SoundClip, SoundCue};
    use regia::framework::sequencer::{Sequence, SequenceLibrary};
    use regia::logic_bridge::LogicSequenceSource;
    use regia::lua::{EcsModWorld, LuaLimits, LuaMods, ModFiles};
    use serde_json::{Value, json};

    struct Client {
        app: App,
        inbox: Sender<Inbound>,
        next_id: u64,
    }

    impl Client {
        fn new() -> Self {
            let mut app = App::new();
            app.add_plugins(MinimalPlugins)
                .add_plugins(regia_scene::ScenePlugin)
                .add_plugins(regia_input::InputPlugin::new(regia_input::InputConfig::new()))
                .add_plugins(regia::agent::AgentPlugin)
                .add_plugins(AiPlugin)
                .add_plugins(LogicBridgePlugin)
                .add_plugins(regia::framework::sequencer::SequencerPlugin)
                .insert_resource(TimeUpdateStrategy::ManualDuration(
                    std::time::Duration::from_millis(50),
                ));
            app.finish_plugins();
            app.update();
            let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
            app.world_mut()
                .insert_resource(regia::agent::AgentInbox(requests));
            let mut permissions = regia::agent::AgentPermissions::default();
            permissions.config.rate_limit_per_second = 10_000;
            app.world_mut().insert_resource(permissions);
            Self {
                app,
                inbox,
                next_id: 0,
            }
        }

        fn call(&mut self, method: &str, params: Value) -> Value {
            self.next_id += 1;
            let (reply, answer) = crossbeam_channel::bounded(1);
            let request = Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(self.next_id)),
                method: method.into(),
                params: params.clone(),
            };
            self.inbox.send(Inbound { request, reply }).expect("inbox open");
            self.app.update();
            let response = answer
                .try_recv()
                .expect("the dispatcher answers within the frame");
            match (response.result, response.error) {
                (Some(value), None) => value,
                (_, error) => panic!("{method} {params}: {error:?}"),
            }
        }
    }

    /// Every phase-3 integration in one graph: a world board, a presentation sequence and cue, a Lua
    /// call, and the sequence-marker, input and net arms.
    const GRAPH: &str = r#"(format_version: 1, world: ["gatehouse"], locals: [(name: "marks", default: Int(0)), (name: "net", default: Int(0)), (name: "input", default: Int(0))], rules: [
        (name: "open", event: EntityEvent("open"), actions: [
            (task: "logic::PlaySequence", set: { "sequence": Literal(String("door.seq.ron")) }),
            (task: "logic::PlayCue", set: { "cue": Literal(String("audio/creak.cue.ron")) }),
            (task: "logic::CallLua", set: { "mod": Literal(String("doors")), "function": Literal(String("opened")), "speed": Literal(Int(3)), "self": BbRef("me") }),
            (task: "logic::SetBlackboard", set: { "key": Literal(String("world/gatehouse/open")), "value": Literal(Bool(true)) })]),
        (name: "marker", event: OnSequenceMarker(sequence: "door.seq.ron", marker: "creak"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/marks")), "op": Literal(String("increment")) })]),
        (name: "lua_said", event: EntityEvent("lua_opened")),
        (name: "rang", event: OnNetMessage(message: "Ring"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/net")), "op": Literal(String("increment")) })]),
        (name: "used", event: InputAction(action: "use"), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("local/input")), "op": Literal(String("increment")) })]),
        (name: "alarm", event: BlackboardChanged(key: "world/gatehouse/alarm"), condition: Key("world/gatehouse/alarm")),
    ])"#;

    #[test]
    fn p3_a_graph_using_each_integration_runs_through_the_dispatcher() {
        let mut client = Client::new();
        let world = client.app.world_mut();
        world.get_resource_or_init::<SequenceLibrary>().insert(
            "door.seq.ron",
            Sequence::from_ron(r#"(version: 5, name: "door", markers: [(name: "creak", time: 0.1)])"#)
                .expect("sequence"),
        );
        let mut audio = Audio::new(&AudioSettings {
            headless: true,
            ..Default::default()
        });
        let clip = SoundClip::new(vec![0.1_f32; 4800], 48_000).expect("clip");
        audio.register_cue(SoundCue::new(
            "creak",
            vec![CueEntry {
                asset: SoundAsset::from_clip(&clip),
                weight: 1.0,
            }],
        ));
        world.insert_resource(audio);
        let mut mods = LuaMods::new(7, LuaLimits::default());
        mods.load(
            ModFiles::from_map([
                (
                    "luamod.ron",
                    r#"(format_version: 1, id: "doors", name: "Doors", version: "1.0.0", entry: "main.lua", api_version: 1)"#,
                ),
                (
                    "main.lua",
                    "function opened(args) regia.emit('lua_opened', args.speed, args.self) return true end",
                ),
            ]),
            &mut EcsModWorld::new(world),
        )
        .expect("mod loads");
        world.insert_resource(mods);

        // Compile through the agent, then spawn the graph (a host does that, as with scene loads).
        let compiled = client.call("logic.compile", json!({ "text": GRAPH }));
        assert_eq!(compiled["ok"], true, "{compiled}");
        let e = spawn(&mut client.app, GRAPH);
        // The Lua function targets its event back at the graph through `args.self`.
        client
            .app
            .world_mut()
            .get_mut::<Blackboard>(e)
            .expect("bb")
            .insert("me", e);
        let bits = e.to_bits();
        let played_before = client.app.world().resource::<Audio>().played_count();

        client.call("logic.emit", json!({ "entity": bits, "event": "open" }));
        for _ in 0..8 {
            client.app.update();
        }
        // The net and input arms through their inbox spellings (the bridges are tested above).
        client.call(
            "logic.emit",
            json!({ "entity": bits, "event": "net_message:Ring" }),
        );
        client.call(
            "logic.emit",
            json!({ "entity": bits, "event": "input_action:use" }),
        );
        client.call(
            "logic.world_set",
            json!({ "board": "gatehouse", "key": "alarm", "value": true }),
        );
        for _ in 0..3 {
            client.app.update();
        }
        let status = client.call("logic.status", json!({ "entity": bits }));
        let fires = |name: &str| {
            status["rules"]
                .as_array()
                .and_then(|r| r.iter().find(|r| r["name"] == name))
                .map(|r| r["fires"].clone())
                .unwrap_or_default()
        };
        assert_eq!(fires("open"), 1, "{status}");
        assert_eq!(fires("marker"), 1, "the sequence's marker came back: {status}");
        assert_eq!(
            fires("lua_said"),
            1,
            "the Lua call's targeted emit came back: {status}"
        );
        assert_eq!(fires("rang"), 1, "{status}");
        assert_eq!(fires("used"), 1, "{status}");
        assert_eq!(
            fires("alarm"),
            1,
            "the agent's world write woke the watch: {status}"
        );
        assert_eq!(status["locals"]["marks"], 1);
        assert_eq!(status["world"]["gatehouse/open"], true);
        assert_eq!(status["world"]["gatehouse/alarm"], true);
        let boards = client.call("logic.world", json!({ "board": "gatehouse" }));
        assert_eq!(boards["boards"]["gatehouse"]["open"], true);
        assert_eq!(
            client.app.world().resource::<Audio>().played_count(),
            played_before + 1,
            "the cue played once"
        );
        let mut players = client.app.world_mut().query::<&LogicSequenceSource>();
        assert_eq!(players.iter(client.app.world()).count(), 1);
        assert!(
            !status["trace"].as_array().is_some_and(|t| t
                .iter()
                .any(|l| l.as_str().is_some_and(|l| l.contains("action_failed")))),
            "{status}"
        );
    }
}
