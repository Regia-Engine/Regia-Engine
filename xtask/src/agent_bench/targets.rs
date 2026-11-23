// SPDX-License-Identifier: MPL-2.0
//! The headless targets of the agent benchmark: an app with the agent server and the plugins a
//! task needs, hosted in this process (no window, no GPU), and the sessions that drive it.
//!
//! Each task gets a fresh app on its own thread, so one task cannot leak state into the next and a
//! panic in a target is a task error, not a crashed run. A client thread plays the calls over the
//! real WebSocket protocol (the same one `regia mcp` forwards); the app thread steps frames.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::prelude::{App, MinimalPlugins, TimeUpdateStrategy};
use serde_json::{Value, json};

use super::{Baselines, Loaded, Session, TaskOutcome, play_task, prepare};

/// One named headless target.
pub struct Target {
    /// The name tasks give in `setup.target`.
    pub name: &'static str,
    /// What the app has, for `docs/formats/agent-task.md`.
    pub description: &'static str,
    /// Adds the target's plugins to an app that already has `MinimalPlugins`.
    pub build: fn(&mut App),
}

/// A flat 8 m by 8 m floor as a navigation mesh, built by the real builder.
fn flat_floor_navmesh() -> Option<regia_nav::NavMesh> {
    use regia_core::math::Vec3;
    let c = 4.0;
    let triangles = vec![
        [
            Vec3::new(-c, 0.0, -c),
            Vec3::new(c, 0.0, -c),
            Vec3::new(c, 0.0, c),
        ],
        [
            Vec3::new(-c, 0.0, -c),
            Vec3::new(c, 0.0, c),
            Vec3::new(-c, 0.0, c),
        ],
    ];
    regia_nav::NavMeshBuilder::default().build(&triangles).ok()
}

/// Every target a task may name.
pub const TARGETS: &[Target] = &[
    Target {
        name: "scene",
        description: "Minimal plugins, the scene plugin and the agent server: entities, components, spawn and edit, play and expect.",
        build: |_| {},
    },
    Target {
        name: "physics",
        description: "The scene target plus the physics plugin: bodies, colliders, ray casts, overlaps.",
        build: |app| {
            app.add_plugins(regia_physics::PhysicsPlugin);
        },
    },
    Target {
        name: "nav",
        description: "The scene target plus a flat 8 m by 8 m navigation mesh (x and z from -4 to 4) and its obstacle set.",
        build: |app| {
            if let Some(mesh) = flat_floor_navmesh() {
                app.insert_resource(mesh);
            }
            app.insert_resource(regia_nav::NavObstacles::default());
        },
    },
    Target {
        name: "audio",
        description: "The scene target plus the audio plugin on a headless device: buses, emitters, zones.",
        build: |app| {
            app.insert_resource(regia_audio::AudioSettings {
                headless: true,
                ..Default::default()
            });
            app.add_plugins(regia_audio::AudioPlugin);
        },
    },
    Target {
        name: "sequencer",
        description: "The scene target plus sequence players (`sequence.player`).",
        build: |app| {
            app.add_plugins(regia_framework::sequencer::SequencerPlugin);
        },
    },
    Target {
        name: "lighting",
        description: "The scene target plus the level settings (`level.get_look` / `level.set_look`) and the directional, point and spot light components.",
        build: |app| {
            app.insert_resource(regia_scene::level::LevelSettings::default());
            // The render plugin registers these; the headless target only needs the types.
            app.register_type::<regia_render::DirectionalLight>()
                .register_type::<regia_render::PointLight>()
                .register_type::<regia_render::SpotLight>();
        },
    },
    Target {
        name: "materials",
        description: "The scene target plus the material graph library: `material.graph_*`, `material.compile`.",
        build: |app| {
            app.add_plugins(regia_matgraph::MaterialGraphPlugin);
        },
    },
    Target {
        name: "terrain",
        description: "The scene target plus the terrain plugin and a flat 64 m by 64 m terrain (65 by 65 samples, 0 to 20 m of height range) at the origin.",
        build: |app| {
            app.add_plugins(regia_terrain::TerrainPlugin);
            let field = regia_asset::Heightfield {
                width: 65,
                height: 65,
                samples: vec![0; 65 * 65],
            };
            let built = regia_terrain::Terrain::new(
                &field,
                64.0,
                20.0,
                0.0,
                regia_terrain::CdlodConfig::default(),
                0,
                regia_terrain::TerrainLayers::default(),
            );
            match built {
                Ok(terrain) => {
                    app.world_mut().spawn(terrain);
                }
                Err(e) => eprintln!("agent-bench: the terrain target could not build its terrain: {e}"),
            }
        },
    },
    Target {
        name: "ui",
        description: "The scene target plus the runtime UI (code-only, no window) hooked to the agent: `ui.*`.",
        build: |app| {
            app.add_message::<regia_platform::WindowResized>()
                .add_message::<regia_platform::WindowScaleFactorChanged>()
                .add_message::<regia_platform::CursorMoved>()
                .add_message::<regia_platform::MouseButtonInput>()
                .add_message::<regia_platform::TouchInput>()
                .add_message::<regia_platform::MouseWheel>()
                .add_message::<regia_platform::KeyboardInput>()
                .add_message::<regia_platform::ImeEvent>();
            app.add_plugins(regia_ui::UiPlugin::code_only());
            app.insert_resource(regia_agent::UiAgentHost::new(regia_ui::agent_bridge::host_call));
        },
    },
    Target {
        name: "scripting",
        description: "The scene target plus script slots, tags, coroutines and the functional test runner (`scripts.*`, `tags.*`, `coroutines.list`, `functest.*`).",
        build: |app| {
            app.add_plugins((
                regia_framework::script::ScriptPlugin,
                regia_framework::tags::TagPlugin,
                regia_framework::coroutine::CoroutinePlugin,
                regia_framework::functional::FunctionalTestPlugin,
            ));
        },
    },
];

/// The longest a task may take before the run gives up on it. A guard against a hung target, not a
/// performance budget.
const TASK_TIMEOUT: Duration = Duration::from_secs(180);

/// Per-process counter for unique project directories.
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// A unique scratch project directory.
fn scratch_project(label: &str) -> PathBuf {
    let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("regia-agent-bench-{}-{n}-{label}", std::process::id()))
}

/// Builds the app of `target` with the agent server advertising under `project`.
fn build_app(target: &str, project: &Path) -> Result<App> {
    let Some(entry) = TARGETS.iter().find(|t| t.name == target) else {
        bail!("unknown target {target:?}");
    };
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    (entry.build)(&mut app);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.to_path_buf()),
    })
    .add_plugins(regia_agent::AgentPlugin)
    .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.finish_plugins();
    Ok(app)
}

/// A live protocol client as a [`Session`].
pub struct Live(pub Client);

impl Session for Live {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.0.call(method, params).map_err(|e| e.to_string())
    }
}

/// Waits for the app to advertise, then connects.
fn connect(project: &Path, stop: &AtomicBool) -> Result<Live> {
    let dir = endpoint::agent_dir(project);
    for _ in 0..3000 {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Ok((advertised, token)) = endpoint::read(&dir)
            && let Ok(mut client) = Client::connect(&advertised.url, &token)
        {
            let _ = client.set_read_timeout(Some(Duration::from_secs(120)));
            return Ok(Live(client));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    bail!("the app did not advertise an endpoint in {}", dir.display())
}

/// Runs one task on a fresh in-process target. `oracle`: play the reference calls; otherwise run
/// the checks on the bare setup.
#[must_use]
pub fn run_in_process(item: &Loaded, oracle: bool) -> TaskOutcome {
    let failed = |message: String| TaskOutcome {
        id: item.task.id.clone(),
        title: item.task.title.clone(),
        pass: false,
        oracle_calls: 0,
        call_budget: item.task.call_budget,
        error: Some(message),
        checks: Vec::new(),
    };
    let project = scratch_project(&item.task.id);
    if let Err(e) = std::fs::create_dir_all(&project) {
        return failed(format!("scratch dir {}: {e}", project.display()));
    }
    let outcome = std::thread::scope(|scope| {
        let handle = scope.spawn(|| host_and_play(item, oracle, &project));
        handle.join()
    });
    let _ = std::fs::remove_dir_all(&project);
    match outcome {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(e)) => failed(format!("{e:#}")),
        Err(_) => failed("the target panicked".to_string()),
    }
}

/// Hosts the app on this thread and plays the task from a client thread.
fn host_and_play(item: &Loaded, oracle: bool, project: &Path) -> Result<TaskOutcome> {
    let mut app = build_app(&item.task.setup.target, project)?;
    let done = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        let client_done = done.clone();
        let client = scope.spawn(move || {
            let outcome = connect(project, &client_done).map(|mut live| play_task(&mut live, item, oracle));
            client_done.store(true, Ordering::Relaxed);
            outcome
        });
        let deadline = Instant::now() + TASK_TIMEOUT;
        while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
            app.update();
        }
        let timed_out = !done.load(Ordering::Relaxed);
        done.store(true, Ordering::Relaxed);
        let joined = client.join();
        if timed_out {
            bail!("the task did not finish in {} s", TASK_TIMEOUT.as_secs());
        }
        match joined {
            Ok(outcome) => outcome,
            Err(_) => bail!("the client thread panicked"),
        }
    })
}

/// `--probe`: runs JSONL calls from `text` against a fresh `target` and prints every answer.
///
/// # Errors
/// A target that would not start or a line that does not parse.
pub fn probe(target: &str, text: &str) -> Result<ExitCode> {
    let steps = super::parse_oracle(text)?;
    let project = scratch_project("probe");
    std::fs::create_dir_all(&project)?;
    let mut app = build_app(target, &project)?;
    let done = Arc::new(AtomicBool::new(false));
    let lines = std::thread::scope(|scope| {
        let client_done = done.clone();
        let project = &project;
        let client = scope.spawn(move || {
            let lines = connect(project, &client_done).map(|mut live| {
                let mut vars = std::collections::BTreeMap::new();
                let mut lines = Vec::new();
                for (i, step) in steps.iter().enumerate() {
                    let params = super::substitute(&step.params, &vars).unwrap_or(Value::Null);
                    let answer = live.call(&step.method, params);
                    if let Ok(result) = &answer {
                        for (name, pointer) in &step.save {
                            if let Some(v) = result.pointer(pointer) {
                                vars.insert(name.clone(), v.clone());
                            }
                        }
                    }
                    lines.push(match answer {
                        Ok(v) => format!("{} {} -> {v}", i + 1, step.method),
                        Err(e) => format!("{} {} -> ERROR {e}", i + 1, step.method),
                    });
                }
                lines
            });
            client_done.store(true, Ordering::Relaxed);
            lines
        });
        let deadline = Instant::now() + TASK_TIMEOUT;
        while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
            app.update();
        }
        done.store(true, Ordering::Relaxed);
        client.join()
    });
    let _ = std::fs::remove_dir_all(&project);
    match lines {
        Ok(Ok(lines)) => {
            for line in lines {
                println!("{line}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("the client thread panicked"),
    }
}

/// The file `--serve` writes the baselines to, next to the endpoint.
fn baselines_path(project: &Path) -> PathBuf {
    endpoint::agent_dir(project).join("bench_baselines.json")
}

/// `--serve`: starts the target of `item`, applies its setup, records the baselines and keeps the
/// app running until the file `stop` appears next to the endpoint (or the process is killed).
///
/// # Errors
/// A target that would not start, or a failed setup.
pub fn serve(item: &Loaded) -> Result<ExitCode> {
    let project = scratch_project(&format!("serve-{}", item.task.id));
    std::fs::create_dir_all(&project)?;
    let mut app = build_app(&item.task.setup.target, &project)?;
    let ready = Arc::new(AtomicBool::new(false));
    let failure = Arc::new(std::sync::Mutex::new(None::<String>));
    let stop_file = endpoint::agent_dir(&project).join("stop");
    std::thread::scope(|scope| {
        let ready_flag = ready.clone();
        let failure_slot = failure.clone();
        let project_ref = &project;
        scope.spawn(move || {
            let never = AtomicBool::new(false);
            let outcome = connect(project_ref, &never).and_then(|mut live| prepare(&mut live, &item.task));
            match outcome {
                Ok((vars, baselines)) => {
                    let text = json!({ "task": item.task.id, "baselines": baselines, "vars": vars });
                    if let Err(e) = std::fs::write(baselines_path(project_ref), text.to_string()) {
                        *failure_slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(format!("writing the baselines: {e}"));
                    }
                }
                Err(e) => {
                    *failure_slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(format!("{e:#}"));
                }
            }
            ready_flag.store(true, Ordering::Relaxed);
        });
        let mut announced = false;
        loop {
            app.update();
            if ready.load(Ordering::Relaxed) && !announced {
                announced = true;
                let failed = failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let Some(message) = failed {
                    eprintln!("agent-bench: setup failed: {message}");
                    break;
                }
                println!("task {}: {}", item.task.id, item.task.title);
                println!("prompt: {}", item.task.prompt);
                println!("project: {}", project.display());
                println!(
                    "drive it with: regia mcp --project {} --attach",
                    project.display()
                );
                println!(
                    "then check: cargo xtask agent-bench --check-only --attach {}",
                    project.display()
                );
                println!(
                    "stop this process (Ctrl-C) when done, or create {}",
                    stop_file.display()
                );
            }
            if announced && stop_file.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
    });
    let message = failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let _ = std::fs::remove_dir_all(&project);
    Ok(if message.is_some() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Connects to an engine the client has driven: a project directory or a `ws://` URL.
///
/// # Errors
/// No endpoint, an unreadable token, or a refused connection.
pub fn attach(address: &str, token_file: Option<&Path>) -> Result<Live> {
    let client = if address.starts_with("ws://") || address.starts_with("wss://") {
        let token_path = token_file.context("a ws:// address needs --token-file")?;
        let token = std::fs::read_to_string(token_path).with_context(|| token_path.display().to_string())?;
        Client::connect(address, token.trim()).map_err(|e| anyhow::anyhow!("{e}"))?
    } else {
        Client::connect_project(Path::new(address)).map_err(|e| anyhow::anyhow!("{e}"))?
    };
    let mut live = Live(client);
    let _ = live.0.set_read_timeout(Some(Duration::from_secs(120)));
    Ok(live)
}

/// What `--serve` saved for `task_id` under the attached project: the baselines and the setup's
/// variables (both empty for a URL, or for another task).
#[must_use]
pub fn read_served(address: &str, task_id: &str) -> (Baselines, BTreeMap<String, Value>) {
    let none = (Baselines::new(), BTreeMap::new());
    let Ok(text) = std::fs::read_to_string(baselines_path(Path::new(address))) else {
        return none;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return none;
    };
    if value["task"].as_str() != Some(task_id) {
        return none;
    }
    let baselines = value["baselines"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let vars = value["vars"]
        .as_object()
        .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    (baselines, vars)
}

/// The task id a served project was prepared for, so `--check-only` can default to it.
#[must_use]
pub fn served_task(address: &str) -> Option<String> {
    let text = std::fs::read_to_string(baselines_path(Path::new(address))).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value["task"].as_str().map(str::to_string)
}
