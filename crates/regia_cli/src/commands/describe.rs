// SPDX-License-Identifier: MPL-2.0
//! `regia describe` (the project map), `regia preflight` (project-side checks: the kind lint) and the
//! cook's kind lint (`docs/specs/101-entity-kinds-and-project-map.md` §§ 5-6, ADR-2488).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_scene::kind::lint::{Finding, Severity, Strictness, has_errors, lint_project};
use regia_scene::kind::map::{ProjectMap, build_map, write_map};
use regia_scene::kind::scan::{KindCatalog, scan_project};
use serde_json::{Value, json};

/// Arguments of `regia describe`.
#[derive(Debug, Clone, Args)]
pub struct DescribeArgs {
    /// Project directory (contains `Regia.toml` and `assets/`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Where to write `project-map.md` and `project-map.json` (default: the project directory).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Take kinds and the systems that read and write each component from the project's running app
    /// (agent method `project.describe`; start it with `regia run` or `regia editor --headless`).
    #[arg(long)]
    pub live: bool,
    /// Print the JSON map to stdout instead of writing the files.
    #[arg(long)]
    pub json: bool,
}

/// Arguments of `regia preflight`.
#[derive(Debug, Clone, Args)]
pub struct PreflightArgs {
    /// Project directory.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Override the project's `[kinds] strictness` (`off`, `warn`, `deny`).
    #[arg(long)]
    pub strictness: Option<String>,
    /// Print the findings as JSON.
    #[arg(long)]
    pub json: bool,
}

/// The project's `[kinds] strictness` (`off` without a manifest or the key).
///
/// # Errors
///
/// The manifest is not valid TOML, or the value is not `off`, `warn` or `deny` (a typo must not
/// silently turn the lint off).
pub fn kinds_strictness(project: &Path) -> Result<Strictness> {
    let Some(path) = regia_core::brand::project_manifest(project) else {
        return Ok(Strictness::Off);
    };
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let table: toml::Table = text
        .parse()
        .with_context(|| format!("{} is not valid TOML", path.display()))?;
    match table.get("kinds").and_then(|k| k.get("strictness")) {
        None => Ok(Strictness::Off),
        Some(value) => value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("[kinds] strictness must be a string"))?
            .parse()
            .map_err(|e: String| anyhow::anyhow!("[kinds] {e}")),
    }
}

/// The project's name (`[project] name`, else the folder name).
fn project_name(project: &Path) -> String {
    crate::manifest::Manifest::load(project)
        .map(|m| m.project.name)
        .unwrap_or_else(|_| {
            project
                .canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default()
        })
}

/// The offline map of `project` (no systems: the CLI does not link the game).
#[must_use]
pub fn offline_map(project: &Path) -> ProjectMap {
    let files = scan_project(project);
    let catalog = KindCatalog::new(None, &files);
    build_map(&project_name(project), &files, &catalog, None, &[])
}

/// How many `project.describe` calls `--live` makes at most while the app says its answer is
/// incomplete. Each call is served on a later frame of the app: the first asks `Last` to record
/// the schedules, the second reads that record, and a schedule that had not run yet when the
/// first call came (the app's very first frame) has run by then. One more covers a call that
/// landed in a frame's `Last`.
const LIVE_DESCRIBE_CALLS: usize = 4;

/// Whether a `project.describe` answer lists every schedule's systems: no record pending and no
/// schedule left unread (`book/agents/entity-kinds.md`: call it again a frame later).
fn describe_answer_is_complete(result: &Value) -> bool {
    let pending = result
        .get("record_pending")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let unread = result
        .get("map")
        .and_then(|map| map.get("unread_schedules"))
        .and_then(Value::as_array)
        .is_some_and(|unread| !unread.is_empty());
    !pending && !unread
}

/// The live map from the project's running app. Asks again (at most [`LIVE_DESCRIBE_CALLS`]
/// times) while the answer is incomplete, as the protocol page says; an answer still incomplete
/// after that is returned as it is, with its unread schedules named in the map (ADR-7386).
fn live_map(project: &Path) -> Result<ProjectMap> {
    let dir = project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", project.display()))?;
    let mut client = regia_agent::client::Client::connect_project(&dir)
        .context("--live: cannot connect to the project's running app (start it with `regia run` or `regia editor --headless`)")?;
    let params = json!({ "project_dir": dir.display().to_string(), "write": false });
    let mut result = Value::Null;
    for _ in 0..LIVE_DESCRIBE_CALLS {
        result = client
            .call("project.describe", params.clone())
            .context("project.describe failed")?;
        if describe_answer_is_complete(&result) {
            break;
        }
    }
    let map = result
        .get("map")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("project.describe returned no map"))?;
    serde_json::from_value(map).context("project.describe returned a map this build cannot read")
}

/// Runs `regia describe`.
///
/// # Errors
///
/// The project cannot be read, `--live` cannot reach the app, or the files cannot be written.
pub fn run_describe(args: &DescribeArgs) -> Result<()> {
    if !args.project.is_dir() {
        bail!("{} is not a directory", args.project.display());
    }
    let map = if args.live {
        live_map(&args.project)?
    } else {
        offline_map(&args.project)
    };
    if args.json {
        print!("{}", map.to_json()?);
        return Ok(());
    }
    let out = args.out.clone().unwrap_or_else(|| args.project.clone());
    let (md, json_path) = write_map(&out, &map)
        .with_context(|| format!("cannot write the project map into {}", out.display()))?;
    println!(
        "{} kinds, {} prefabs, {} levels ({} systems)\n{}\n{}",
        map.kinds.len(),
        map.prefabs.len(),
        map.levels.len(),
        map.systems_source,
        md.display(),
        json_path.display()
    );
    Ok(())
}

/// The kind lint of `project` at `strictness`.
#[must_use]
pub fn lint(project: &Path, strictness: Strictness) -> Vec<Finding> {
    if strictness == Strictness::Off {
        return Vec::new();
    }
    let files = scan_project(project);
    let catalog = KindCatalog::new(None, &files);
    lint_project(&files, &catalog, strictness)
}

/// Prints findings to stderr, one per line.
fn print_findings(findings: &[Finding]) {
    for finding in findings {
        eprintln!("{finding}");
    }
}

/// Runs `regia preflight`. Returns whether it passed (no error finding).
///
/// # Errors
///
/// The manifest or the `--strictness` value is invalid.
pub fn run_preflight(args: &PreflightArgs) -> Result<bool> {
    let strictness = match &args.strictness {
        Some(text) => text.parse().map_err(|e: String| anyhow::anyhow!(e))?,
        None => kinds_strictness(&args.project)?,
    };
    let findings = lint(&args.project, strictness);
    let passed = !has_errors(&findings);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "strictness": strictness.name(),
                "findings": findings,
                "passed": passed,
            }))?
        );
    } else {
        print_findings(&findings);
        let count = |s: Severity| findings.iter().filter(|f| f.severity == s).count();
        println!(
            "kinds ({}): {} error(s), {} warning(s), {} note(s)",
            strictness.name(),
            count(Severity::Error),
            count(Severity::Warning),
            count(Severity::Note)
        );
    }
    Ok(passed)
}

/// The cook's kind lint: prints findings at `warn`, stops the cook at `deny` with an error.
///
/// # Errors
///
/// The strictness is `deny` and the lint found errors, or the manifest is invalid.
pub fn cook_kind_lint(project: &Path) -> Result<()> {
    let strictness = kinds_strictness(project)?;
    let findings = lint(project, strictness);
    print_findings(&findings);
    if has_errors(&findings) {
        bail!(
            "kind lint: {} error(s) at [kinds] strictness = \"deny\" (run `regia preflight` for the list)",
            findings.iter().filter(|f| f.severity == Severity::Error).count()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-7386: `--live` asks again while the app's answer is incomplete. The M5 run's first
    /// call landed in the app's first frame, before `Update` had ever run: `Update` was unread and
    /// the map listed no writer of `Transform`.
    #[test]
    fn a_describe_answer_is_complete_only_with_nothing_pending_or_unread() {
        let answer = |pending: bool, unread: &[&str]| json!({ "record_pending": pending, "map": { "unread_schedules": unread } });
        assert!(describe_answer_is_complete(&answer(false, &[])));
        assert!(
            !describe_answer_is_complete(&answer(true, &[])),
            "a record is pending"
        );
        assert!(
            !describe_answer_is_complete(&answer(false, &["Update"])),
            "a schedule had not run yet"
        );
        assert!(!describe_answer_is_complete(&answer(true, &["PreUpdate"])));
        // An older app without the fields: nothing says it is incomplete.
        assert!(describe_answer_is_complete(&json!({ "map": {} })));
    }

    fn project(strictness: Option<&str>) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir("cli_describe");
        std::fs::create_dir_all(root.join("assets/levels")).expect("mkdir");
        let mut manifest =
            "[project]\nname = \"Lint\"\nid = \"com.example.lint\"\nversion = \"0.1.0\"\nbuild_number = 1\n"
                .to_string();
        if let Some(s) = strictness {
            manifest.push_str(&format!("\n[kinds]\nstrictness = \"{s}\"\n"));
        }
        std::fs::write(root.join("Regia.toml"), manifest).expect("manifest");
        std::fs::write(
            root.join("assets/levels/a.level.ron"),
            r#"SceneFile(format_version: 8, entities: [
                (guid: "00000000-0000-4000-8000-000000000001", name: "Loose",
                    components: { "my_game::Ammo": () }),
            ])"#,
        )
        .expect("level");
        root
    }

    #[test]
    fn strictness_defaults_to_off_and_typos_are_refused() {
        assert_eq!(kinds_strictness(&project(None)).expect("off"), Strictness::Off);
        assert_eq!(
            kinds_strictness(&project(Some("warn"))).expect("warn"),
            Strictness::Warn
        );
        assert!(kinds_strictness(&project(Some("strict"))).is_err());
    }

    #[test]
    fn the_cook_lint_passes_at_off_and_warn_and_stops_at_deny() {
        assert!(cook_kind_lint(&project(None)).is_ok());
        assert!(cook_kind_lint(&project(Some("warn"))).is_ok());
        let err = cook_kind_lint(&project(Some("deny"))).expect_err("deny stops the cook");
        assert!(err.to_string().contains("1 error"), "{err}");
    }

    #[test]
    fn preflight_reports_pass_and_fail() {
        let off = PreflightArgs {
            project: project(Some("deny")),
            strictness: Some("off".into()),
            json: true,
        };
        assert!(run_preflight(&off).expect("runs"));
        let deny = PreflightArgs {
            strictness: None,
            ..off
        };
        assert!(!run_preflight(&deny).expect("runs"));
    }

    #[test]
    fn describe_writes_the_same_bytes_twice() {
        let root = project(None);
        let out = root.join("map");
        let args = DescribeArgs {
            project: root.clone(),
            out: Some(out.clone()),
            live: false,
            json: false,
        };
        run_describe(&args).expect("describe");
        let first = std::fs::read(out.join("project-map.json")).expect("json");
        let first_md = std::fs::read(out.join("project-map.md")).expect("md");
        run_describe(&args).expect("describe again");
        assert_eq!(std::fs::read(out.join("project-map.json")).expect("json"), first);
        assert_eq!(std::fs::read(out.join("project-map.md")).expect("md"), first_md);
        let map: ProjectMap = serde_json::from_slice(&first).expect("parses");
        assert_eq!(map.project, "Lint");
        assert_eq!(map.systems_source, "offline");
    }

    /// Spec 101 § 6.3: `regia describe --live` connects to the project's running app (agent
    /// method `project.describe`) and takes kinds and the system access from it. The app runs
    /// headless on the main thread with manual frames; the client thread polls the advertised
    /// endpoint (the same wait the agent smoke tests use), runs `run_describe` over it and
    /// checks the written map.
    #[test]
    fn describe_live_reads_kinds_and_systems_from_the_running_app() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let root = project(None);
        std::fs::create_dir_all(root.join("assets/kinds")).expect("mkdir");
        std::fs::write(
            root.join("assets/kinds/turret.kind.ron"),
            r##"(format_version: 1, name: "Turret", icon: "gear", color: "#30c050", description: "An aimed turret", required: ["regia_scene::transform::Transform"])"##,
        )
        .expect("kind file");
        let out = root.join("live-map");
        let agent_dir = regia_agent::endpoint::agent_dir(&root);
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let client_root = root.clone();
        let client_out = out.clone();
        let client_agent_dir = agent_dir.clone();

        let client_thread = std::thread::Builder::new()
            .name("describe-live-client".into())
            .spawn(move || {
                // Wait until the app's agent server is up and accepting connections.
                let up = {
                    let mut probe = None;
                    for _ in 0..4000 {
                        if let Ok((advertised, token)) = regia_agent::endpoint::read(&client_agent_dir)
                            && let Ok(connected) =
                                regia_agent::client::Client::connect(&advertised.url, &token)
                        {
                            probe = Some(connected);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    probe.is_some()
                };
                assert!(up, "the running app advertised its agent endpoint");
                let args = DescribeArgs {
                    project: client_root.clone(),
                    out: Some(client_out.clone()),
                    live: true,
                    json: false,
                };
                run_describe(&args).expect("describe --live against the running app");
                let map: ProjectMap = serde_json::from_slice(
                    &std::fs::read(client_out.join("project-map.json")).expect("map written"),
                )
                .expect("parses");
                // Kinds and the system access came from the live app, not the offline scan.
                assert_eq!(map.systems_source, "live", "{map:#?}");
                let turret = map
                    .kinds
                    .iter()
                    .find(|k| k.name == "Turret")
                    .expect("the app's loaded data kind is in the map");
                assert!(
                    matches!(
                        &turret.source,
                        regia_scene::kind::KindSource::Data { path, .. } if path == "assets/kinds/turret.kind.ron"
                    ),
                    "{:?}",
                    turret.source
                );
                let transform = map
                    .components
                    .iter()
                    .find(|c| c.type_path == "regia_scene::transform::Transform")
                    .expect("the kind's required component is in the map");
                assert_eq!(transform.required_by, ["Turret"]);
                assert!(
                    transform
                        .writers
                        .iter()
                        .any(|w| w.schedule == "Update" && w.system.ends_with("nudge")),
                    "the live system access lists the Update system: {transform:#?}"
                );
                flag.store(true, Ordering::Relaxed);
            })
            .expect("spawns the client");

        // The headless app: the same minimal recipe as the agent smoke tests, plus the scene
        // plugin for the kind registry and a project-loaded data kind.
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_app::prelude::MinimalPlugins)
            .insert_resource(regia_agent::AgentSettings {
                project_dir: Some(root.clone()),
            })
            .add_plugins((regia_agent::AgentPlugin, regia_scene::ScenePlugin))
            .insert_resource(regia_app::prelude::TimeUpdateStrategy::ManualDuration(
                Duration::from_millis(16),
            ));
        regia_scene::kind::load_project_kinds(app.world_mut(), &root);
        fn nudge(mut q: regia_ecs::prelude::Query<&mut regia_scene::transform::Transform>) {
            // The access metadata records the write; no entity exists to visit.
            let _ = q.iter_mut().count();
        }
        app.add_systems(regia_app::prelude::Update, nudge);
        app.finish_plugins();
        let deadline = Instant::now() + Duration::from_secs(90);
        while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
            app.update();
        }
        client_thread
            .join()
            .expect("the client thread finished without a failed assertion");
        assert!(done.load(Ordering::Relaxed), "the describe did not complete");
        let _ = std::fs::remove_dir_all(&root);
    }
}
