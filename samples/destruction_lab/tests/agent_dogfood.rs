// SPDX-License-Identifier: MIT-0
//! Rule 8 dogfooding (ADR-2573): an agent drives the lab over the real agent protocol, at fixed
//! steps. Client 0 renders headless (lavapipe in CI), the agent pauses it, fires two blasts on the
//! server, steps the whole lab, and reads what each client plays: the same broken bonds and the
//! same chunk pose digest. It also takes a screenshot; no golden, the test checks pixels moved.
//! `DESTRUCTION_LAB_PNG=<dir>` keeps the before and after frames for a report.

#[path = "../kit/mod.rs"]
mod kit;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use destruction_lab::agent::install_lab_methods;
use destruction_lab::view::LabView;
use destruction_lab::{TOWER_X, WALL_X, lab_app};
use regia::agent::client::Client;
use regia::agent::endpoint;
use regia::framework::destruction_draw::DestructionDrawStats;
use regia::prelude::*;
use regia_render::RenderPlugin;
use serde_json::{Value, json};

type LabAgentCommand = (String, Value, mpsc::Sender<Result<Value, String>>);
type LabAgentCommands = mpsc::Sender<LabAgentCommand>;

/// Hang guard only (device creation on a loaded lavapipe host can take a while).
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);
/// Per-call protocol timeout.
const CALL_TIMEOUT_MS: u64 = 120_000;

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn step(client: &mut Client, frames: u64) {
    let _ = call(client, "play.step", json!({ "frames": frames }));
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// A frame's RGB pixels.
fn pixels(png_bytes: &[u8]) -> Vec<[u8; 3]> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    let channels = info.color_type.samples();
    buf.truncate(info.buffer_size());
    buf.chunks(channels).map(|p| [p[0], p[1], p[2]]).collect()
}

fn screenshot(agent: &mut Client, name: &str) -> Vec<[u8; 3]> {
    let shot = call(
        agent,
        "seeing.screenshot",
        json!({ "target": "offscreen", "width": 480, "height": 270, "timeout_ms": CALL_TIMEOUT_MS }),
    );
    let bytes = base64_decode(shot["png_base64"].as_str().expect("png_base64")).expect("base64");
    if let Some(dir) = std::env::var_os("DESTRUCTION_LAB_PNG") {
        let _ = std::fs::write(std::path::Path::new(&dir).join(name), &bytes);
    }
    pixels(&bytes)
}

fn conversation(agent: &mut Client) {
    let _ = call(agent, "play.pause", json!({}));
    step(agent, 10);
    let joined = call(agent, "lab.report", json!({}));
    assert_eq!(joined["clients"][0]["in_step"], true, "{joined}");
    assert_eq!(joined["clients"][1]["in_step"], true, "{joined}");
    assert_eq!(joined["clients_agree"], true, "{joined}");
    // Instances came from the server; nothing broke yet.
    assert_eq!(
        joined["clients"][0]["playback"]["instances"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    assert_eq!(joined["clients"][0]["playback"]["broken_bond_count"], 0);
    let whole = screenshot(agent, "before.png");

    let fired = call(
        agent,
        "lab.fire",
        json!({ "center": [WALL_X, 1.2, 1.0], "preset": "barrel", "radius": 3.0, "impulse": 9000.0, "fx": false }),
    );
    assert!(fired["queued_for_tick"].as_u64().is_some(), "{fired}");
    step(agent, 12);
    let _ = call(
        agent,
        "lab.fire",
        json!({ "center": [WALL_X, 1.5, -3.0], "preset": "grenade", "radius": 4.0, "impulse": 3000.0, "fx": false }),
    );
    step(agent, 20);

    let report = call(agent, "lab.report", json!({}));
    let (a, b) = (&report["clients"][0], &report["clients"][1]);
    // The asserts of the brief: the same broken bond set, the same chunk pose digest, at one tick.
    assert_eq!(a["playback"]["tick"], b["playback"]["tick"], "{report}");
    assert!(
        a["playback"]["broken_bond_count"].as_u64().is_some_and(|n| n > 0),
        "{report}"
    );
    assert_eq!(
        a["playback"]["broken_bond_digest"], b["playback"]["broken_bond_digest"],
        "{report}"
    );
    assert_eq!(a["playback"]["instances"], b["playback"]["instances"], "{report}");
    assert_eq!(
        a["playback"]["chunk_pose_digest"], b["playback"]["chunk_pose_digest"],
        "{report}"
    );
    assert!(
        a["playback"]["debris_count"].as_u64().is_some_and(|n| n > 0),
        "{report}"
    );
    assert_eq!(report["clients_agree"], true, "{report}");
    assert_eq!(
        a["playback"]["broken_bond_digest"], report["server"]["playback"]["broken_bond_digest"],
        "{report}"
    );
    assert!(
        a["chunks_pushed"].as_u64().is_some_and(|n| n > 0),
        "the second blast: {report}"
    );
    assert_eq!(a["explosions"], 2, "{report}");

    // The built-in method sees client 0 and tells the same story, with or without the list.
    let own = call(agent, "physics.destruction_playback", json!({}));
    assert_eq!(own, a["playback"], "{report}");
    let short = call(
        agent,
        "physics.destruction_playback",
        json!({ "list_bonds": false }),
    );
    assert_eq!(short["chunk_pose_digest"], a["playback"]["chunk_pose_digest"]);
    assert!(short["instances"][0].get("broken_bonds").is_none(), "{short}");
    let state = call(agent, "physics.destruction_state", json!({}));
    assert_eq!(state["authority"], "replica", "{state}");
    let net = call(agent, "physics.destruction_net", json!({}));
    assert_eq!(net["in_step"], true, "{net}");
    let stats = call(agent, "gameplay.explosion_stats", json!({}));
    assert_eq!(stats["skipped_replica"], 2, "{stats}");
    assert_eq!(stats["fx_played"], 0, "both blasts asked for no effects: {stats}");

    // The drawing: pieces instead of the whole wall, with no asset missing a mesh set.
    let drawn = call(agent, "physics.destruction_draw", json!({}));
    assert!(
        drawn["stats"]["instances_drawn"].as_u64().is_some_and(|n| n >= 1),
        "{drawn}"
    );
    assert!(
        drawn["stats"]["debris_draws"].as_u64().is_some_and(|n| n > 0),
        "{drawn}"
    );
    assert_eq!(drawn["stats"]["missing_sets"], 0, "{drawn}");
    let broken = screenshot(agent, "after.png");
    assert_eq!(whole.len(), broken.len());
    let moved = whole
        .iter()
        .zip(&broken)
        .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 24))
        .count();
    assert!(
        moved > whole.len() / 200,
        "only {moved} of {} pixels changed",
        whole.len()
    );
    // One more blast, at the tower, with its fire and smoke; it plays on client 0, the one that draws.
    // Headless, particles spawn on a rendered frame and the engine drops spawn jobs that waited
    // `STALE_UPLOADS` frames with none, paused ones too (counted from the blast, ADR-3307). So the
    // shot comes right after the blast, and a loaded host that lets those frames pass between two
    // calls gets another blast to look at.
    let flame = |frame: &[[u8; 3]]| {
        frame
            .iter()
            .filter(|p| p[0] > 200 && u16::from(p[0]) > u16::from(p[2]) + 60)
            .count()
    };
    let mut seen = 0;
    for _ in 0..4 {
        let _ = call(
            agent,
            "lab.fire",
            json!({ "center": [TOWER_X, 0.8, 1.6], "preset": "barrel", "radius": 3.0, "impulse": 20000.0 }),
        );
        step(agent, 3);
        seen = flame(&screenshot(agent, "blast.png"));
        if seen > broken.len() / 20 {
            break;
        }
    }
    assert!(
        seen > broken.len() / 20 && seen > flame(&broken) * 2,
        "no fireball: {seen} flame pixels of {}, {} before it",
        broken.len(),
        flame(&broken)
    );
    let stats = call(agent, "gameplay.explosion_stats", json!({}));
    assert!(
        stats["fx_played"].as_u64().is_some_and(|n| n > 0),
        "the fire and smoke: {stats}"
    );
    step(agent, 17);
    let late = call(agent, "lab.report", json!({}));
    let tower = &late["clients"][0]["playback"]["instances"][1];
    eprintln!("tower after its blast: {tower}");
    assert!(
        tower["detached"].as_u64().is_some_and(|n| n >= 20),
        "the tower comes down: {tower}"
    );
    // A method that does not fit is refused with a code the guide lists.
    let err = agent
        .call("lab.fire", json!({ "center": [0.0, 1.0], "preset": "barrel" }))
        .expect_err("refused");
    assert!(format!("{err:?}").contains("bad_center"), "{err:?}");
}

#[test]
fn an_agent_checks_that_both_clients_play_the_same_chunks() {
    let project = regia_test_utils::temp::unique_temp_dir("destruction-lab-agent");
    let kit = kit::fracture_kit().expect("kit");
    let mut app = lab_app(&kit, Vec::new()).expect("lab");
    install_lab_methods(app.world_mut());
    app.add_plugins((MinimalPlugins, RenderPlugin { headless: true }, LabView { kit }))
        .insert_resource(regia::agent::AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins(regia::agent::AgentPlugin);

    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("destruction-lab-agent".into())
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
            conversation(&mut agent);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("script thread");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !done.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    let stats = app.world().get_resource::<DestructionDrawStats>().cloned();
    script.join().expect("the script passed");
    assert!(
        done.load(Ordering::Relaxed),
        "the conversation did not finish: {stats:?}"
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn agent_fire_and_report_are_scoped_to_their_endpoint_session() {
    fn launch(
        kit: &destruction_lab::LabKit,
    ) -> (
        App,
        std::path::PathBuf,
        LabAgentCommands,
        std::thread::JoinHandle<()>,
    ) {
        let project = regia_test_utils::temp::unique_temp_dir("destruction-session-agent");
        let mut app = lab_app(kit, Vec::new()).expect("lab");
        install_lab_methods(app.world_mut());
        app.insert_resource(regia::agent::AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins((MinimalPlugins, regia::agent::AgentPlugin));
        app.world_mut().resource_mut::<Time>().paused = true;
        app.finish_plugins();

        let agent_dir = endpoint::agent_dir(&project);
        let (commands, command_rx): (LabAgentCommands, mpsc::Receiver<LabAgentCommand>) = mpsc::channel();
        let (connected_tx, connected_rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("destruction-session-agent".into())
            .spawn(move || {
                let mut client = Client::connect_project(&project).expect("unique endpoint ready");
                connected_tx.send(()).expect("test still waiting");
                while let Ok((method, params, reply)) = command_rx.recv() {
                    if method == "__stop" {
                        break;
                    }
                    let result = client.call(&method, params).map_err(|error| format!("{error:?}"));
                    let _ = reply.send(result);
                }
            })
            .expect("agent worker");
        connected_rx.recv().expect("agent connected");
        (app, agent_dir, commands, worker)
    }

    fn call(app: &mut App, commands: &LabAgentCommands, method: &str, params: Value) -> Value {
        let (reply_tx, reply_rx) = mpsc::channel();
        commands
            .send((method.to_owned(), params, reply_tx))
            .expect("agent worker alive");
        for _ in 0..100_000 {
            app.update();
            if let Ok(result) = reply_rx.try_recv() {
                return result.unwrap_or_else(|error| panic!("{method}: {error}"));
            }
        }
        panic!("{method} was not answered while fixed-step app updates were pumped");
    }

    fn stop(commands: LabAgentCommands, worker: std::thread::JoinHandle<()>, endpoint_dir: &std::path::Path) {
        let (reply, _unused) = mpsc::channel();
        commands
            .send(("__stop".into(), Value::Null, reply))
            .expect("worker alive");
        worker.join().expect("agent worker exits");
        let project = endpoint_dir
            .parent()
            .and_then(std::path::Path::parent)
            .expect("unique project directory");
        std::fs::remove_dir_all(project).expect("remove unique endpoint directory");
    }

    let kit = kit::fracture_kit().expect("kit");
    let (mut first, first_dir, first_commands, first_worker) = launch(&kit);
    let first_report = call(&mut first, &first_commands, "lab.report", json!({}));
    assert_eq!(first_report["server"]["explosions"], 0);
    let fired = call(
        &mut first,
        &first_commands,
        "lab.fire",
        json!({ "center": [WALL_X, 1.2, 1.0], "preset": "barrel", "fx": false }),
    );
    assert_eq!(fired["queued_for_tick"], 1);
    let still_pending = call(&mut first, &first_commands, "lab.report", json!({}));
    assert_eq!(still_pending["ticks"], 0);
    assert_eq!(still_pending["server"]["explosions"], 0);
    assert_eq!(still_pending["clients_agree"], true);
    drop(first);
    stop(first_commands, first_worker, &first_dir);

    let (mut second, second_dir, second_commands, second_worker) = launch(&kit);
    assert_ne!(
        first_dir, second_dir,
        "each session gets a distinct endpoint directory"
    );
    let stepped = call(&mut second, &second_commands, "play.step", json!({ "frames": 1 }));
    assert_eq!(stepped["paused"], true, "one requested frame completed");
    let report = call(&mut second, &second_commands, "lab.report", json!({}));
    assert_eq!(report["ticks"], 1);
    assert_eq!(report["server"]["explosions"], 0);
    assert_eq!(report["server"]["playback"]["broken_bond_count"], 0);
    assert_eq!(report["server"]["playback"]["debris_count"], 0);
    assert_eq!(
        report["server"]["playback"]["instances"].as_array().map(Vec::len),
        Some(2)
    );
    for client in report["clients"].as_array().expect("two clients") {
        assert_eq!(client["playback"]["broken_bond_count"], 0);
        assert_eq!(client["playback"]["debris_count"], 0);
        assert_eq!(client["playback"]["instances"].as_array().map(Vec::len), Some(2));
        assert_eq!(client["in_step"], true);
    }
    assert_eq!(report["clients_agree"], true);
    drop(second);
    stop(second_commands, second_worker, &second_dir);
}
