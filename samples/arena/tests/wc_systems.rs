// SPDX-License-Identifier: MIT-0
//! The arena on the gameplay systems (ADR-2134): state-tree bots that fire through abilities picked
//! by the weapon chooser, run their magazine dry and restock at smart-object stations; team tags;
//! the spectator view of a bot-only match. The last test drives the same match through the agent
//! protocol (`book/agents/arena.md` § Gameplay systems), the dogfood check of rule 8.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_ai::{StateTreeRunStatus, StateTreeStatus};
use regia_audio::AudioSettings;
use regia_framework::abilities::{CueKind, CueLog};
use regia_framework::settings::SettingsPath;
use regia_framework::spectator::SpectatorCamera;
use regia_framework::tags::{Tag, TagContainer};
use serde_json::{Value, json};

use arena::config::ArenaBrainStatus;
use arena::loadout::{self, Loadout};
use arena::spectate::SpectatorFocus;
use arena::{ArenaConfig, ArenaSample, ArenaState, BotBrainKind, MatchRng, Pawn};

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(90);

/// A headless state-tree match: `bots` bots, no local player, no warm-up, a score limit the match
/// never reaches.
fn arena(seed: u64, bots: usize) -> App {
    arena_with_warmup(seed, bots, 0.0)
}

/// [`arena`] with a warm-up of `warmup_secs` before the round goes live.
fn arena_with_warmup(seed: u64, bots: usize, warmup_secs: f32) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    app.insert_resource(ArenaConfig {
        bot_count: bots,
        local_player: false,
        score_limit: 1_000_000,
        warmup_secs,
        match_secs: 600.0,
        use_navmesh: false,
        brain: BotBrainKind::StateTree,
    });
    app.insert_resource(MatchRng::new(seed));
    app.add_plugins((MinimalPlugins, ArenaSample));
    // The HUD and input systems read these platform messages even headless (no window plugin).
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    enable_fixed_step(&mut app, 60.0);
    app
}

fn bots(app: &mut App) -> Vec<Entity> {
    let mut query = app.world_mut().query::<(Entity, &Pawn)>();
    let mut bots: Vec<Entity> = query
        .iter(app.world())
        .filter(|(_, pawn)| pawn.is_bot)
        .map(|(entity, _)| entity)
        .collect();
    bots.sort_by_key(|e| e.index());
    bots
}

fn brain_status(app: &mut App) -> ArenaBrainStatus {
    let mut query = app.world_mut().query::<&ArenaBrainStatus>();
    query
        .iter(app.world())
        .next()
        .cloned()
        .expect("the match entity has a brain status")
}

fn resupply_cues(app: &App) -> usize {
    let resupply = Tag::new("Cue.Resupply").expect("valid tag");
    app.world()
        .resource::<CueLog>()
        .since(0, usize::MAX)
        .iter()
        .filter(|(_, cue)| cue.tag == resupply && cue.kind == CueKind::Burst)
        .count()
}

#[test]
fn the_default_match_runs_state_tree_bots_with_stations_and_loadouts() {
    // A quarter-second warm-up (15 ticks at 60 Hz). Since ADR-8485 (108b68d3a) a new session opens
    // in its first phase, so with no warm-up the round is live from tick 1 and the trees never sit
    // in Idle; the warm-up is what keeps them out of the round here. The trees start in Idle and
    // stay there for every tick the match is still warming up; once the round is live, the next
    // tick hands Idle over to Fight.
    let mut app = arena_with_warmup(7, 3, 0.25);
    let mut warmup_ticks = 0;
    loop {
        app.update();
        let phase = arena::match_flow::arena_state(app.world())
            .expect("the match state")
            .phase;
        if phase != arena::match_flow::Phase::Warmup {
            assert_eq!(
                phase,
                arena::match_flow::Phase::Round,
                "the warm-up ends in the round"
            );
            break;
        }
        warmup_ticks += 1;
        assert!(warmup_ticks <= 60, "the warm-up ended within a second");
        for bot in bots(&mut app) {
            let tree = app.world().get::<StateTreeStatus>(bot).expect("a ticking tree");
            assert_eq!(
                tree.active_path,
                vec!["Bot".to_string(), "Idle".to_string()],
                "warm-up tick {warmup_ticks} holds the tree in Idle"
            );
        }
    }
    assert!(warmup_ticks > 0, "the match warmed up before the round");
    app.update();
    let status = brain_status(&mut app);
    assert_eq!(status.brain, "state_tree");
    assert_eq!(status.trees, 3);
    assert_eq!(status.stations, 2);
    assert!(status.errors.is_empty(), "{:?}", status.errors);
    let team = Tag::new("Team.Bot").expect("valid tag");
    for bot in bots(&mut app) {
        let world = app.world();
        assert!(
            world.get::<Loadout>(bot).is_some(),
            "bot {bot:?} carries the weapon grants"
        );
        assert_eq!(loadout::ammo(world, bot), 6.0, "a full magazine");
        assert!(
            world.get::<TagContainer>(bot).is_some_and(|tags| tags.has(team)),
            "bot {bot:?} is tagged Team.Bot"
        );
        let tree = world.get::<StateTreeStatus>(bot).expect("a ticking tree");
        assert_eq!(tree.status, StateTreeRunStatus::Running);
        assert_eq!(
            tree.active_path,
            vec!["Bot".to_string(), "Fight".to_string()],
            "the first tick after the warm-up enters Fight"
        );
    }
}

#[test]
fn bots_fight_run_dry_restock_at_a_station_and_fight_again() {
    let mut app = arena(11, 2);
    let mut saw_resupply = false;
    let mut saw_fight_after_resupply = false;
    let mut min_ammo = f32::MAX;
    for _ in 0..(60 * 40) {
        app.update();
        for bot in bots(&mut app) {
            let world = app.world();
            min_ammo = min_ammo.min(loadout::ammo(world, bot));
            let path = &world.get::<StateTreeStatus>(bot).expect("tree").active_path;
            if path.last().is_some_and(|s| s == "Resupply") {
                saw_resupply = true;
            } else if saw_resupply && path.last().is_some_and(|s| s == "Fight") {
                saw_fight_after_resupply = true;
            }
        }
        if saw_fight_after_resupply && resupply_cues(&app) > 0 {
            break;
        }
    }
    let shots = app.world().resource::<arena::weapons::ShotLog>();
    assert!(shots.rifles > 0, "rifle shots through the rifle ability");
    assert!(shots.rockets > 0, "rockets through the rocket ability");
    assert!(
        min_ammo < 1.0,
        "a bot ran its magazine dry (lowest ammo {min_ammo})"
    );
    assert!(saw_resupply, "a dry bot entered Resupply");
    assert!(
        resupply_cues(&app) > 0,
        "the refill effect fired its cue at a station"
    );
    assert!(saw_fight_after_resupply, "the restocked bot went back to Fight");
    // Nobody holds a station once every bot is back in the fight.
    let all_bots_fighting = {
        let mut query = app.world_mut().query::<&StateTreeStatus>();
        query
            .iter(app.world())
            .all(|s| s.active_path.last().is_some_and(|leaf| leaf == "Fight"))
    };
    if all_bots_fighting {
        let mut slots = app.world_mut().query::<&regia_ai::SmartObjectSlot>();
        assert!(slots.iter(app.world()).all(|slot| slot.claimed_by.is_none()));
    }
}

/// A disabled station is skipped: the dry bot claims the other one. With both disabled it waits in
/// `Resupply` holding no claim, and restocks as soon as one comes back.
#[test]
fn a_dry_bot_skips_a_disabled_station_and_waits_when_none_is_free() {
    use regia_ai::{SmartObject, SmartObjectSlot};
    use regia_core::math::Vec3;
    use regia_framework::abilities::{AttrId, Attributes};
    use regia_scene::GlobalTransform;

    let mut app = arena(21, 2);
    for _ in 0..3 {
        app.update();
    }
    let bot = bots(&mut app)[0];
    // Out of the fight's way: a huge health pool (no respawn refill) and an empty magazine.
    if let Some(mut health) = app.world_mut().get_mut::<regia_framework::damage::Health>(bot) {
        health.max = 1.0e6;
        health.current = 1.0e6;
    }
    let empty = |app: &mut App| {
        if let Some(mut attrs) = app.world_mut().get_mut::<Attributes>(bot) {
            let _ = attrs.set_base(AttrId::of("ammo"), 0.0);
        }
    };
    // The stations by distance from the bot: (block, slot).
    let bot_pos = app
        .world()
        .get::<regia_scene::Transform>(bot)
        .expect("bot")
        .translation;
    let mut stations: Vec<(f32, Entity, Entity)> = {
        let mut query = app
            .world_mut()
            .query::<(Entity, &SmartObjectSlot, &GlobalTransform, &ChildOf)>();
        query
            .iter(app.world())
            .map(|(slot, _, transform, parent)| {
                let p = transform.translation();
                let d = (Vec3::new(p.x, bot_pos.y, p.z) - bot_pos).length();
                (d, parent.parent(), slot)
            })
            .collect()
    };
    stations.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert_eq!(stations.len(), 2);
    let (near_block, far_slot) = (stations[0].1, stations[1].2);
    let set_enabled = |app: &mut App, block: Entity, enabled: bool| {
        if let Some(mut object) = app.world_mut().get_mut::<SmartObject>(block) {
            object.enabled = enabled;
        }
    };
    set_enabled(&mut app, near_block, false);
    empty(&mut app);
    let mut claimed_far = false;
    for _ in 0..600 {
        app.update();
        let held = app
            .world()
            .get::<SmartObjectSlot>(far_slot)
            .is_some_and(|slot| slot.claimed_by == Some(bot));
        claimed_far |= held;
        if claimed_far && loadout::ammo(app.world(), bot) >= 1.0 {
            break;
        }
    }
    assert!(
        claimed_far,
        "the bot claimed the far station while the near one was disabled"
    );
    assert!(loadout::ammo(app.world(), bot) >= 1.0, "and restocked there");

    // Both disabled: the dry bot waits in Resupply and holds nothing.
    let far_block = stations[1].1;
    set_enabled(&mut app, far_block, false);
    for _ in 0..30 {
        app.update();
    }
    empty(&mut app);
    for _ in 0..120 {
        app.update();
    }
    let leaf = app
        .world()
        .get::<StateTreeStatus>(bot)
        .and_then(|s| s.active_path.last().cloned());
    assert_eq!(leaf.as_deref(), Some("Resupply"));
    let held_any = {
        let mut query = app.world_mut().query::<&SmartObjectSlot>();
        query.iter(app.world()).any(|slot| slot.claimed_by == Some(bot))
    };
    assert!(!held_any, "no station is free, so nothing is claimed");
    assert!(loadout::ammo(app.world(), bot) < 1.0);

    // One comes back: the bot restocks and fights again.
    set_enabled(&mut app, near_block, true);
    let mut back = false;
    for _ in 0..900 {
        app.update();
        let leaf = app
            .world()
            .get::<StateTreeStatus>(bot)
            .and_then(|s| s.active_path.last().cloned());
        if leaf.as_deref() == Some("Fight") {
            back = true;
            break;
        }
    }
    assert!(back, "a station came back and the bot restocked");
}

#[test]
fn a_seed_replays_the_state_tree_match() {
    let run = |seed: u64| {
        let mut app = arena(seed, 3);
        for _ in 0..(60 * 20) {
            app.update();
        }
        let scores = {
            let mut query = app.world_mut().query::<&ArenaState>();
            query.iter(app.world()).next().map(|s| s.scores.clone())
        };
        let ammo: Vec<f32> = bots(&mut app)
            .into_iter()
            .map(|bot| loadout::ammo(app.world(), bot))
            .collect();
        let positions: Vec<[f32; 3]> = bots(&mut app)
            .into_iter()
            .map(|bot| {
                app.world()
                    .get::<regia_scene::Transform>(bot)
                    .map(|t| t.translation.to_array())
                    .unwrap_or_default()
            })
            .collect();
        (scores, ammo, positions, resupply_cues(&app))
    };
    assert_eq!(run(5), run(5), "same seed, same match");
}

#[test]
fn a_bot_only_match_is_watched_through_the_spectator_camera() {
    let mut app = arena(3, 2);
    for _ in 0..120 {
        app.update();
    }
    assert!(app.world().contains_resource::<SpectatorCamera>());
    let (pose, anchor) = {
        let camera = app.world().resource::<SpectatorCamera>();
        (camera.pose(), camera.anchor())
    };
    let mut cameras = app
        .world_mut()
        .query::<(&SpectatorFocus, &regia_scene::Transform)>();
    let (focus, transform) = cameras.iter(app.world()).next().expect("the main camera");
    assert!(focus.enabled, "no local player: the spectator view is on");
    assert_eq!(transform.translation, pose.0);
    assert_eq!(transform.rotation, pose.1);
    let bot_1 = {
        let mut query = app.world_mut().query::<(&Pawn, &regia_scene::Transform)>();
        query
            .iter(app.world())
            .find(|(pawn, _)| pawn.slot == 1)
            .map(|(_, t)| t.translation)
            .expect("bot 1")
    };
    assert!(
        (anchor - bot_1).length() < 3.0,
        "the spectator follows bot 1 (anchor {anchor}, bot {bot_1})"
    );
}

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn read_token(project: &std::path::Path) -> String {
    std::fs::read_to_string(endpoint::agent_dir(project).join("token")).expect("token file")
}

/// The first bot's entity id, through `scene.query_entities`.
fn first_bot(client: &mut Client) -> u64 {
    let found = call(
        client,
        "scene.query_entities",
        json!({ "component": "StateTreeStatus" }),
    );
    let mut ids: Vec<u64> = found["entities"]
        .as_array()
        .map(|list| list.iter().filter_map(|e| e["entity"].as_u64()).collect())
        .unwrap_or_default();
    ids.sort_unstable();
    *ids.first()
        .unwrap_or_else(|| panic!("no state-tree bot in {found}"))
}

#[test]
fn an_agent_drives_the_gameplay_systems_of_the_arena() {
    let project: PathBuf = regia_test_utils_temp("regia-wc-arena");
    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let script_project = project.clone();
    let agent_dir = endpoint::agent_dir(&project);

    let script = std::thread::Builder::new()
        .name("wc-arena-agent-client".into())
        .spawn(move || {
            let mut client = None;
            for _ in 0..2000 {
                if let Ok((advertised, _token)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &read_token(&script_project))
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut client) = client else {
                panic!("no endpoint at {}", agent_dir.display());
            };
            call(&mut client, "agent.hello", json!({ "client": "wc-arena", "model": "worker" }));

            // The bot tree compiles; the match entity says which brain runs.
            let compiled = call(
                &mut client,
                "statetree.compile",
                json!({ "text": arena::brain::BOT_TREE_TEXT }),
            );
            assert_eq!(compiled["ok"], true, "{compiled}");
            assert_eq!(compiled["states"], 4, "{compiled}");
            let expect = call(
                &mut client,
                "play.expect",
                json!({ "assertions": [
                    { "name": "state-tree brain",
                      "condition": { "entity": { "component": "ArenaBrainStatus" }, "component": "ArenaBrainStatus",
                                     "path": "brain", "op": "eq", "value": "state_tree" } },
                    { "name": "two stations",
                      "condition": { "entity": { "component": "ArenaBrainStatus" }, "component": "ArenaBrainStatus",
                                     "path": "stations", "op": "eq", "value": 2 } }
                ]}),
            );
            assert_eq!(expect["pass"], true, "{expect}");

            let bot = first_bot(&mut client);

            // Reloading the same tree keeps the running state (every active state still exists).
            let reload = call(
                &mut client,
                "statetree.reload",
                json!({ "entity": bot, "text": arena::brain::BOT_TREE_TEXT }),
            );
            assert_eq!(reload["outcome"], "kept", "{reload}");

            // Tags: the bot is on the bot team.
            let tags = call(&mut client, "tags.query", json!({ "entity": bot, "query": { "Has": "Team" } }));
            assert_eq!(tags["matches"], true, "{tags}");

            // Abilities: two grants, the rifle ready.
            let listed = call(&mut client, "abilities.list", json!({ "entity": bot }));
            let grants = listed["entities"][0]["grants"].as_array().cloned().unwrap_or_default();
            assert_eq!(grants.len(), 2, "{listed}");
            let rifle = grants
                .iter()
                .find(|g| g["asset"] == "abilities/rifle.ability.ron")
                .and_then(|g| g["id"].as_u64())
                .unwrap_or_else(|| panic!("a rifle grant in {listed}"));
            // The bots have been firing since the round began: the rifle is ready or cooling.
            let check = call(&mut client, "abilities.check", json!({ "entity": bot, "grant": rifle }));
            assert!(
                check["ok"] == true || check["cause"] == "on_cooldown",
                "{check}"
            );

            // The weapon chooser: a ready rocket at 10 m, the rifle up close, nothing when dry.
            let pick = |client: &mut Client, distance: f64, ammo: i64| {
                call(
                    client,
                    "chooser.evaluate",
                    json!({ "path": "choosers/weapon.chooser.ron", "trace": "brief",
                            "context": { "distance": distance, "ammo": ammo,
                                         "rifle_ready": true, "rocket_ready": true } }),
                )
            };
            assert_eq!(pick(&mut client, 10.0, 12)["row"], "rocket");
            assert_eq!(pick(&mut client, 2.0, 12)["row"], "rifle");
            let dry = pick(&mut client, 10.0, 0);
            assert_eq!(dry["used_catch_all"], true, "{dry}");

            // Smart objects: two free resupply slots.
            let objects = call(&mut client, "smartobject.list", json!({}));
            let slots = objects["slots"].as_array().cloned().unwrap_or_default();
            assert_eq!(slots.iter().filter(|s| s["role"] == "resupply").count(), 2, "{objects}");
            let slot_ids: Vec<u64> = slots
                .iter()
                .filter(|s| s["role"] == "resupply")
                .filter_map(|s| s["entity"].as_u64())
                .collect();

            // Spatial queries: the station ranking `Restock` uses, asked for this bot. Both slots
            // come back, nearest (highest score) first, and they are the smart-object slots.
            let queries = call(&mut client, "ai.query_list", json!({}));
            let listed = queries["templates"].as_array().is_some_and(|list| {
                list.iter()
                    .any(|t| t["name"] == arena::brain::STATION_QUERY && t["item_type"] == "entity")
            });
            assert!(listed, "{queries}");
            let ranked = call(
                &mut client,
                "ai.query_run",
                json!({ "template": arena::brain::STATION_QUERY, "owner": bot, "pick": "all" }),
            );
            assert_eq!(ranked["status"], "success", "{ranked}");
            assert_eq!(ranked["item_count"], 2, "{ranked}");
            let items = ranked["items"].as_array().cloned().unwrap_or_default();
            let scores: Vec<f64> = items.iter().filter_map(|i| i["score"].as_f64()).collect();
            assert!(scores.len() == 2 && scores[0] >= scores[1], "{ranked}");
            for item in &items {
                let entity = item["entity"].as_u64().unwrap_or_default();
                assert!(slot_ids.contains(&entity), "{item} is a resupply slot of {slot_ids:?}");
            }
            let again = call(
                &mut client,
                "ai.query_run",
                json!({ "template": arena::brain::STATION_QUERY, "owner": bot, "pick": "all" }),
            );
            assert_eq!(again["items"], ranked["items"], "same world, same answer");

            // Empty the magazine through the protocol: the tree must restock at a station. The
            // host holds the clock paused, so every frame below is an explicit `play.step` and the
            // run is the same each time. A huge health pool keeps a respawn (which refills) out of
            // the way.
            for path in ["max", "current"] {
                call(
                    &mut client,
                    "scene.set_property",
                    json!({ "entity": bot, "component": "Health", "path": path, "value": 1.0e6 }),
                );
            }
            call(
                &mut client,
                "abilities.set_base",
                json!({ "entity": bot, "attribute": "ammo", "base": 0.0 }),
            );
            let leaf = |client: &mut Client| {
                let read = call(
                    client,
                    "scene.get_entity",
                    json!({ "entity": bot, "components": ["StateTreeStatus"] }),
                );
                read["components"]["StateTreeStatus"]["value"]["active_path"][1]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            };
            let mut seen = Vec::new();
            for _ in 0..80 {
                call(&mut client, "play.step", json!({ "frames": 15 }));
                let now = leaf(&mut client);
                if seen.last() != Some(&now) {
                    seen.push(now.clone());
                }
                if seen.iter().any(|s| s == "Resupply") && now == "Fight" {
                    break;
                }
            }
            assert!(
                seen.windows(2).any(|w| w[0] == "Resupply" && w[1] == "Fight"),
                "the dry bot restocked and went back to Fight: {seen:?}"
            );
            let dry = call(&mut client, "abilities.check", json!({ "entity": bot, "grant": rifle }));
            assert_ne!(dry["cause"], "cannot_afford", "restocked: {dry}");
            let cues = call(&mut client, "abilities.cues", json!({}));
            let restocked = cues["cues"]
                .as_array()
                .is_some_and(|list| list.iter().any(|c| c["tag"] == "Cue.Resupply"));
            assert!(restocked, "{cues}");

            // The spectator view follows the bot the agent picks.
            let spectator = call(&mut client, "spectator.get", json!({}));
            assert_eq!(spectator["mode"], "follow", "{spectator}");
            flag.store(true, Ordering::Relaxed);
        })
        .expect("client thread");

    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    let mut app = arena(0x5EED, 2);
    app.add_plugins(regia_agent::AgentPlugin)
        .insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.clone()),
        });
    app.finish_plugins();
    // Startup and the first round tick, then the clock stops: the script steps every frame.
    for _ in 0..3 {
        app.update();
    }
    app.world_mut().resource_mut::<regia_app::Time>().paused = true;
    while !script_ok.load(Ordering::Relaxed) && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("script finished");
    assert!(
        script_ok.load(Ordering::Relaxed),
        "the agent script did not finish within {} s",
        SCRIPT_TIMEOUT.as_secs()
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// A fresh, unique temp project directory (pid + counter, never a fixed name).
fn regia_test_utils_temp(prefix: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(prefix);
    let _ = std::fs::create_dir_all(&dir);
    dir
}
