// SPDX-License-Identifier: MPL-2.0
//! The menu tree, File ▸ Package for platform, Tools, Help and the project icon through the
//! registered agent handlers (ADR-7475, `book/agents/editor-menus.md`).
//!
//! The package and clear-caches jobs run a fake `regia` (a shell script set as
//! `RegiaCliProgram`) that records its arguments, writes into `--out` and either ends with the
//! `--json` object or blocks until killed, so the tests need no built CLI. Waits are on the job's
//! condition variable, never on wall time.

use std::path::{Path, PathBuf};

use regia_agent::extension::AgentExtension;
use regia_agent::protocol::RpcError;
use regia_app::prelude::*;
use serde_json::{Value, json};

fn edit_world(root: Option<&Path>) -> World {
    let mut world = World::new();
    world.init_resource::<regia_editor::command::UndoStack>();
    regia_editor::agent_ext::install(&mut world);
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: root.map(Path::to_path_buf),
    });
    world
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params)
}

fn reason(result: Result<Value, RpcError>) -> String {
    match result {
        Ok(value) => panic!("expected an error, got {value}"),
        Err(error) => error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

fn project(tag: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    std::fs::write(root.join("Regia.toml"), "[project]\nname = \"menus\"\n").expect("manifest");
    root
}

#[test]
fn menu_list_lists_every_item_with_a_registered_agent_path() {
    let mut world = edit_world(None);
    // The editor host's whole method table, as `editor_method_snapshot` renders it: the headless
    // host adds `editor.open_level`, `agent.shutdown` and `gen.*` (ADR-2412).
    regia_editor::headless::install(&mut world);
    let answer = call(&mut world, "editor.menu_list", json!({})).expect("menu_list");
    let menus = answer["menus"].as_array().expect("menus");
    let names: Vec<&str> = menus.iter().filter_map(|m| m["name"].as_str()).collect();
    assert_eq!(
        names,
        [
            "File", "Create", "Edit", "View", "Tools", "Sequence", "Play", "Help", "Window"
        ]
    );
    let window = menus.iter().find(|m| m["name"] == "Window").expect("Window menu");
    let routes: Vec<_> = window["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["method"].as_str().expect("route"))
        .collect();
    assert_eq!(
        routes,
        [
            "editor.command",
            "editor.command",
            "editor.command",
            "editor.command",
            "editor.panel_new_instance",
            "editor.panel_new_instance",
            "editor.panel_new_instance",
            "editor.package_size_report",
            "editor.platform_check",
            "editor.launch_profiles",
            "editor.utility_panels",
        ]
    );
    let extension = world.resource::<AgentExtension>().clone();
    let mut core: Vec<&str> = regia_agent::schema::METHODS.iter().map(|m| m.name).collect();
    core.extend(regia_agent::builtin_extension_methods());
    let mut count = 0;
    for menu in menus {
        let items = menu["items"].as_array().expect("items");
        assert!(!items.is_empty(), "{} has items", menu["name"]);
        for item in items {
            count += 1;
            let method = item["method"].as_str().unwrap_or_default();
            assert!(
                extension.method(method).is_some() || core.contains(&method),
                "{} names {method:?}, which is not an agent method",
                item["path"]
            );
            assert!(item["params"].is_object(), "{}", item["path"]);
            assert!(item["action"].as_str().is_some_and(|a| !a.is_empty()));
        }
    }
    assert_eq!(answer["count"], json!(count));
    let find = |path: &str| {
        menus
            .iter()
            .flat_map(|m| m["items"].as_array().cloned().unwrap_or_default())
            .find(|i| i["path"] == path)
            .unwrap_or_else(|| panic!("no {path}"))
    };
    assert_eq!(
        find("File/Package for platform…")["method"],
        "editor.package_start"
    );
    assert_eq!(
        find("Tools/Rebuild asset database")["method"],
        "editor.rebuild_asset_db"
    );
    assert_eq!(find("Tools/Clear caches…")["params"], json!({ "confirm": true }));
    assert_eq!(find("Play/▶ Simulate")["params"], json!({ "mode": "simulate" }));
    assert_eq!(find("Play/Players/2 players")["params"], json!({ "players": 2 }));
    assert_eq!(
        find("Help/Documentation")["params"],
        json!({ "topic": "documentation" })
    );
    assert_eq!(find("Help/About Regia Engine")["method"], "editor.about");
    assert_eq!(find("Window/Notes")["method"], "editor.command");
    assert_eq!(find("Window/Notes")["params"], json!({"run":"window.open_notes"}));
    assert_eq!(
        find("Window/Activity")["params"],
        json!({"run":"window.open_activity"})
    );
    // No project: packaging is disabled.
    assert_eq!(find("File/Package for platform…")["enabled"], json!(false));
    // Sequence ▸ Render movie… (ADR-7726): its dialog's job is `editor.render_movie`; no project, no movie.
    assert_eq!(find("Sequence/Render movie…")["method"], "editor.render_movie");
    assert_eq!(find("Sequence/Render movie…")["enabled"], json!(false));
}

#[test]
fn package_needs_a_project_and_valid_fields() {
    let mut world = edit_world(None);
    assert_eq!(
        reason(call(
            &mut world,
            "editor.package_start",
            json!({ "target": "linux" })
        )),
        "package_no_project"
    );
    let root = project("menus_package_fields");
    let mut world = edit_world(Some(&root));
    assert_eq!(
        reason(call(
            &mut world,
            "editor.package_start",
            json!({ "target": "macos" })
        )),
        "package_bad_target"
    );
    assert_eq!(
        reason(call(
            &mut world,
            "editor.package_start",
            json!({ "target": "linux", "profile": "--release" })
        )),
        "package_bad_profile"
    );
    let missing = root.join("no-such-regia");
    world.insert_resource(regia_agent::cook_package::RegiaCliProgram(missing));
    assert_eq!(
        reason(call(
            &mut world,
            "editor.package_start",
            json!({ "target": "linux", "confirm": true })
        )),
        "package_spawn_failed"
    );
    assert_eq!(
        call(
            &mut world,
            "editor.package_status",
            json!({ "job": "package:999" })
        )
        .err()
        .and_then(|e| e.data)
        .map(|d| d["reason"].clone()),
        Some(json!("package_not_found"))
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn rebuild_asset_db_and_the_icon_and_help_through_the_handlers() {
    let root = project("menus_tools");
    let mut world = edit_world(Some(&root));
    std::fs::write(root.join("assets/a.png"), b"x").expect("source");
    let answer = call(&mut world, "editor.rebuild_asset_db", json!({})).expect("rebuild");
    assert_eq!(answer["sources"], json!(1));
    assert_eq!(answer["dropped"], json!([]));

    // The icon: copied byte for byte.
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&[0, 0, 0, 16, 0, 0, 0, 16, 8, 6, 0, 0, 0, 0xAA, 0xBB]);
    let source = root.join("picked.png");
    std::fs::write(&source, &png).expect("png");
    let read = call(&mut world, "editor.project_icon", json!({})).expect("read");
    assert_eq!(read["set"], json!(false));
    assert_eq!(read["path"], Value::Null);
    let set = call(
        &mut world,
        "editor.project_icon",
        json!({ "path": source.display().to_string() }),
    )
    .expect("set");
    assert_eq!(
        (set["width"].clone(), set["set"].clone()),
        (json!(16), json!(true))
    );
    assert_eq!(
        std::fs::read(root.join("assets/branding/icon.png")).expect("icon"),
        png
    );
    std::fs::write(root.join("bad.png"), b"GIF89a").expect("gif");
    assert_eq!(
        reason(call(
            &mut world,
            "editor.project_icon",
            json!({ "path": "bad.png" })
        )),
        "icon_not_png"
    );

    // Help: the repository's book and licence are found from the test's working directory.
    let docs = call(
        &mut world,
        "editor.help_open",
        json!({ "topic": "documentation", "dry_run": true }),
    )
    .expect("docs");
    assert_eq!(docs["opened"], json!(false));
    assert!(
        Path::new(docs["path"].as_str().unwrap_or_default()).is_file(),
        "{docs}"
    );
    let guide = call(
        &mut world,
        "editor.help_open",
        json!({ "topic": "agent_guide", "dry_run": true }),
    )
    .expect("guide");
    assert!(
        guide["path"].as_str().unwrap_or_default().contains("protocol"),
        "{guide}"
    );
    assert!(call(&mut world, "editor.help_open", json!({ "topic": "faq" })).is_err());
    let about = call(&mut world, "editor.about", json!({ "open": true })).expect("about");
    assert_eq!(about["version"], json!(env!("CARGO_PKG_VERSION")));
    // The repository's LICENSE is the verbatim MPL-2.0 (ADR-10225).
    assert!(
        about["license_title"]
            .as_str()
            .is_some_and(|t| t.contains("Mozilla Public License")),
        "{about}"
    );
    assert_eq!(about["license_spdx"], json!("MPL-2.0"), "{about}");
    assert!(
        about["license_path"]
            .as_str()
            .is_some_and(|p| p.ends_with("LICENSE")),
        "{about}"
    );
    assert!(
        about["notices_path"]
            .as_str()
            .is_some_and(|p| p.ends_with("THIRD_PARTY_NOTICES.md"))
    );
    assert_eq!(about["open"], json!(true));
    std::fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
mod fake_cli {
    use super::*;
    use regia_editor::package_job::{CliJob, CliJobs};

    /// Writes an executable fake `regia` into `dir`: it records its arguments in `args.txt`,
    /// wipes and rebuilds `Game/` in `--out` as the CLI's `fresh_staging` does (even on a dry run,
    /// so the tests see that the editor keeps a dry run's writes out of the output folder),
    /// writes `Game/data.bin`, prints `READY`, then runs `tail`.
    pub fn fake_regia(dir: &Path, tail: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let script = dir.join("fake-regia.sh");
        let args = dir.join("args.txt");
        let text = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nout=''\nprev=''\nfor a in \"$@\"; do\n  if [ \"$prev\" = '--out' ]; then out=\"$a\"; fi\n  prev=\"$a\"\ndone\nif [ -n \"$out\" ]; then\n  rm -rf \"$out/Game\"\n  mkdir -p \"$out/Game\"\n  echo partial > \"$out/Game/data.bin\"\nfi\necho 'packaging 1/2' >&2\necho READY >&2\n{tail}\n",
            args.display()
        );
        std::fs::write(&script, text).expect("script");
        let mut perms = std::fs::metadata(&script).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod");
        script
    }

    pub fn args(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("args.txt"))
            .expect("args")
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn package_job(world: &World) -> CliJob {
        world
            .resource::<CliJobs>()
            .package
            .clone()
            .expect("a package job")
    }

    const DONE: &str = "echo done > \"$out/game.tar.gz\"\necho \"{\\\"ok\\\":true,\\\"artifact\\\":\\\"$out/game.tar.gz\\\"}\"\nexit 0";

    #[test]
    fn a_package_job_reaches_done_with_its_artifact_and_log() {
        let root = project("menus_package_done");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let mut world = edit_world(Some(&root));
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(fake_regia(&bin, DONE)));
        let started = call(
            &mut world,
            "editor.package_start",
            json!({ "target": "linux", "profile": "dev", "out": "dist/linux", "confirm": true }),
        )
        .expect("start");
        let id = started["job"].as_str().expect("job").to_string();
        assert!(id.starts_with("package:"), "{started}");
        // The background-task row the Cancel button and editor.cancel_task use.
        assert!(
            world
                .resource::<regia_editor::tasks::BackgroundTasks>()
                .rows()
                .iter()
                .any(|row| row.id == "package")
        );
        let outcome = package_job(&world).wait().expect("outcome");
        assert_eq!(outcome["ok"], json!(true), "{outcome}");
        let status = call(&mut world, "editor.package_status", json!({})).expect("status");
        assert_eq!(status["state"], json!("done"), "{status}");
        let out = root.join("dist/linux");
        assert_eq!(
            status["result"]["artifact"],
            json!(format!("{}/game.tar.gz", out.display()))
        );
        assert!(out.join("game.tar.gz").is_file());
        let log = PathBuf::from(status["log"].as_str().expect("log"));
        assert!(std::fs::read_to_string(&log).expect("log").contains("READY"));
        // The run got the CLI's flags.
        let argv = args(&bin);
        assert_eq!(argv[..5], ["package", "--platform", "linux", "--profile", "dev"]);
        assert!(argv.contains(&"--json".to_string()) && argv.contains(&root.display().to_string()));
        // The finished run's task row is gone (no Cancel left to press).
        assert!(
            !world
                .resource::<regia_editor::tasks::BackgroundTasks>()
                .rows()
                .iter()
                .any(|row| row.id == "package")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_dry_run_passes_the_flag_and_writes_no_output() {
        let root = project("menus_package_dry");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let mut world = edit_world(Some(&root));
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(fake_regia(
            &bin,
            "echo '{\"ok\":true,\"dry_run\":true}'",
        )));
        call(
            &mut world,
            "editor.package_start",
            json!({ "target": "windows", "dry_run": true }),
        )
        .expect("start");
        let outcome = package_job(&world).wait().expect("outcome");
        assert_eq!(outcome["ok"], json!(true), "{outcome}");
        assert_eq!(outcome["report"]["dry_run"], json!(true));
        assert!(args(&bin).contains(&"--dry-run".to_string()));
        // Review w8: the fake writes into `--out` even on a dry run; nothing reaches the project.
        assert!(
            !root.join("dist").exists(),
            "a dry run leaves no output folder: {outcome}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cancel_ends_a_running_package_and_leaves_no_partial_output() {
        let root = project("menus_package_cancel");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let mut world = edit_world(Some(&root));
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(fake_regia(
            &bin,
            "exec sleep 600",
        )));

        // Cancel through editor.cancel_task: the output directory the run created goes.
        call(
            &mut world,
            "editor.package_start",
            json!({ "target": "linux", "confirm": true }),
        )
        .expect("start");
        let job = package_job(&world);
        assert!(job.wait_for_line("READY"), "the fake run printed READY");
        let out = root.join("dist/linux");
        // The run writes into its private staging folder inside the output folder (ADR-7496),
        // never into the output folder itself.
        let staging = job.staging.clone().expect("a staging folder");
        assert_eq!(staging.parent(), Some(out.as_path()));
        assert!(
            staging.join("Game/data.bin").is_file(),
            "the run wrote partial output"
        );
        assert!(
            !out.join("Game").exists(),
            "nothing reaches the output folder before the run finishes"
        );
        // A second package while one runs is refused.
        assert_eq!(
            reason(call(
                &mut world,
                "editor.package_start",
                json!({ "target": "linux", "confirm": true })
            )),
            "package_busy"
        );
        call(&mut world, "editor.cancel_task", json!({ "id": "package" })).expect("cancel_task");
        let outcome = job.wait().expect("outcome");
        assert_eq!(outcome["code"], json!("package_cancelled"), "{outcome}");
        let status = call(&mut world, "editor.package_status", json!({})).expect("status");
        assert_eq!(status["state"], json!("cancelled"), "{status}");
        assert!(
            !out.exists(),
            "no partial output is left: {:?}",
            outcome["removed"]
        );

        // Cancel through editor.cancel_job into an existing folder: only the run's files go.
        let keep = root.join("dist/keep");
        std::fs::create_dir_all(&keep).expect("keep");
        std::fs::write(keep.join("old.zip"), b"old").expect("old");
        let started = call(
            &mut world,
            "editor.package_start",
            json!({ "target": "windows", "out": keep.display().to_string(), "confirm": true }),
        )
        .expect("start");
        let job = package_job(&world);
        assert!(job.wait_for_line("READY"));
        call(&mut world, "editor.cancel_job", json!({ "id": started["job"] })).expect("cancel_job");
        let outcome = job.wait().expect("outcome");
        assert_eq!(outcome["code"], json!("package_cancelled"), "{outcome}");
        assert!(keep.join("old.zip").is_file(), "files that were there stay");
        assert!(!keep.join("Game").exists(), "the run's files go");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_failed_package_reports_package_failed() {
        let root = project("menus_package_fail");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let mut world = edit_world(Some(&root));
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(fake_regia(
            &bin,
            "echo 'error: no game crate' >&2\nexit 3",
        )));
        call(
            &mut world,
            "editor.package_start",
            json!({ "target": "linux", "confirm": true }),
        )
        .expect("start");
        let outcome = package_job(&world).wait().expect("outcome");
        assert_eq!(outcome["code"], json!("package_failed"), "{outcome}");
        assert_eq!(outcome["exit_code"], json!(3));
        let status = call(&mut world, "editor.package_status", json!({})).expect("status");
        assert_eq!(status["state"], json!("failed"));
        assert!(
            !root.join("dist").exists(),
            "a failed run leaves no partial output"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn clear_caches_asks_first_then_runs_regia_clean_cache() {
        let root = project("menus_clear_caches");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let mut world = edit_world(Some(&root));
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(fake_regia(
            &bin,
            "echo '{\"ok\":true,\"removed\":[]}'",
        )));
        assert_eq!(
            reason(call(&mut world, "editor.clear_caches", json!({}))),
            "needs_confirmation"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.clear_caches",
                json!({ "confirm": false })
            )),
            "needs_confirmation"
        );
        // Review w8: asking first means nothing ran and no job exists before the confirmation.
        assert!(
            !bin.join("args.txt").exists(),
            "regia clean did not run unconfirmed"
        );
        assert!(
            world
                .get_resource::<CliJobs>()
                .is_none_or(|jobs| jobs.clean.is_none())
        );
        assert!(
            !world
                .get_resource::<regia_editor::tasks::BackgroundTasks>()
                .is_some_and(|tasks| tasks.rows().iter().any(|row| row.id == "clear_caches")),
            "no task row before the confirmation"
        );
        let started = call(&mut world, "editor.clear_caches", json!({ "confirm": true })).expect("clear");
        assert!(started["job"].as_str().is_some_and(|j| j.starts_with("clean:")));
        let job = world.resource::<CliJobs>().clean.clone().expect("clean job");
        let outcome = job.wait().expect("outcome");
        assert_eq!(outcome["ok"], json!(true), "{outcome}");
        assert_eq!(
            args(&bin),
            [
                "clean",
                "--cache",
                "--json",
                "--project",
                &root.display().to_string()
            ]
        );
        let status = call(
            &mut world,
            "editor.package_status",
            json!({ "job": started["job"] }),
        )
        .expect("status");
        assert_eq!(status["state"], json!("done"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// Review w8 finding 1: `out` is a request path (ADR-1816). `..` and an absolute path outside
    /// the project, the temp dir, the working directory and `output_roots` are refused before
    /// anything runs.
    #[test]
    fn package_out_is_confined_to_the_allowed_roots() {
        let root = project("menus_package_confined");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let mut world = edit_world(Some(&root));
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(fake_regia(
            &bin, "exit 0",
        )));
        for out in ["../escaped", "/regia-no-such-root/out"] {
            assert_eq!(
                reason(call(
                    &mut world,
                    "editor.package_start",
                    json!({ "target": "linux", "out": out, "confirm": true })
                )),
                "path_outside_project",
                "{out}"
            );
        }
        assert!(!bin.join("args.txt").exists(), "a refused request runs nothing");
        assert!(
            world
                .get_resource::<CliJobs>()
                .is_none_or(|jobs| jobs.package.is_none())
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Review w8 finding 1: a cancel removes only what the run wrote. A file another program puts
    /// into the output folder while the package runs stays.
    #[test]
    fn cancel_keeps_files_other_programs_wrote_into_the_output_folder() {
        let root = project("menus_package_foreign");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let mut world = edit_world(Some(&root));
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(fake_regia(
            &bin,
            "exec sleep 600",
        )));
        let out = root.join("dist/shared");
        std::fs::create_dir_all(&out).expect("out");
        std::fs::write(out.join("old.zip"), b"old").expect("old");
        call(
            &mut world,
            "editor.package_start",
            json!({ "target": "linux", "out": "dist/shared", "confirm": true }),
        )
        .expect("start");
        let job = package_job(&world);
        assert!(job.wait_for_line("READY"));
        // Another program writes into the same folder while the package runs.
        std::fs::write(out.join("notes-from-another-program.txt"), b"mine").expect("foreign");
        std::fs::create_dir_all(out.join("other-tool")).expect("foreign dir");
        call(&mut world, "editor.cancel_task", json!({ "id": "package" })).expect("cancel");
        let outcome = job.wait().expect("outcome");
        assert_eq!(outcome["code"], json!("package_cancelled"), "{outcome}");
        assert!(
            out.join("notes-from-another-program.txt").is_file(),
            "a file the run did not write stays: removed {}",
            outcome["removed"]
        );
        assert!(out.join("other-tool").is_dir(), "{}", outcome["removed"]);
        assert!(out.join("old.zip").is_file());
        assert!(!out.join("Game").exists(), "the run's own output goes");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Review w8 finding 2: re-packaging into the default `dist/<target>` where the previous build's
    /// `<slug>/` staging folder already is. The CLI wipes and rebuilds `<out>/<slug>`; a cancel must
    /// not leave the half-built folder behind, and the previous build is not lost.
    #[test]
    fn cancel_of_a_repackage_leaves_no_half_built_staging_folder() {
        let root = project("menus_package_repackage");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let mut world = edit_world(Some(&root));
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(fake_regia(
            &bin,
            "exec sleep 600",
        )));
        // The previous good build.
        let out = root.join("dist/linux");
        std::fs::create_dir_all(out.join("Game/content")).expect("previous");
        std::fs::write(out.join("Game/game"), b"previous binary").expect("binary");
        std::fs::write(out.join("Game-linux.tar.gz"), b"previous archive").expect("archive");
        call(
            &mut world,
            "editor.package_start",
            json!({ "target": "linux", "confirm": true }),
        )
        .expect("start");
        let job = package_job(&world);
        assert!(job.wait_for_line("READY"));
        call(&mut world, "editor.cancel_task", json!({ "id": "package" })).expect("cancel");
        let outcome = job.wait().expect("outcome");
        assert_eq!(outcome["code"], json!("package_cancelled"), "{outcome}");
        assert!(
            !out.join("Game/data.bin").exists(),
            "no half-built staging is left: removed {}",
            outcome["removed"]
        );
        assert_eq!(
            std::fs::read(out.join("Game/game")).ok().as_deref(),
            Some(&b"previous binary"[..]),
            "the previous build is still there"
        );
        assert_eq!(
            std::fs::read(out.join("Game-linux.tar.gz")).ok().as_deref(),
            Some(&b"previous archive"[..])
        );
        let leftovers: Vec<String> = std::fs::read_dir(&out)
            .expect("out")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "Game" && name != "Game-linux.tar.gz")
            .collect();
        assert!(leftovers.is_empty(), "nothing else is left: {leftovers:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Review w8 finding 2 (success side): a finished re-package replaces the previous build's
    /// `<slug>/` folder as the CLI does, and the artifact path names the output folder.
    #[test]
    fn a_finished_repackage_replaces_the_previous_build() {
        let root = project("menus_package_replace");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let mut world = edit_world(Some(&root));
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(fake_regia(&bin, DONE)));
        let out = root.join("dist/linux");
        std::fs::create_dir_all(out.join("Game")).expect("previous");
        std::fs::write(out.join("Game/stale.txt"), b"old build").expect("stale");
        std::fs::write(out.join("game.tar.gz"), b"old archive").expect("archive");
        std::fs::write(out.join("unrelated.txt"), b"keep").expect("unrelated");
        call(
            &mut world,
            "editor.package_start",
            json!({ "target": "linux", "confirm": true }),
        )
        .expect("start");
        let outcome = package_job(&world).wait().expect("outcome");
        assert_eq!(outcome["ok"], json!(true), "{outcome}");
        assert_eq!(
            outcome["artifact"],
            json!(format!("{}/game.tar.gz", out.display()))
        );
        assert_eq!(
            std::fs::read(out.join("game.tar.gz")).ok().as_deref(),
            Some(&b"done\n"[..])
        );
        assert!(
            out.join("Game/data.bin").is_file(),
            "the new staging folder is in place"
        );
        assert!(
            !out.join("Game/stale.txt").exists(),
            "the CLI's fresh staging replaces the old folder"
        );
        assert!(out.join("unrelated.txt").is_file());
        let names: Vec<String> = std::fs::read_dir(&out)
            .expect("out")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names.len(),
            3,
            "no staging folder of the editor is left: {names:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Review w8 finding 5: `editor.package_start` writes a shippable game, so like `agent.package`
    /// (spec 23 § 3.7) it needs `confirm: true`; a dry run writes nothing and needs none.
    #[test]
    fn package_start_needs_confirmation_unless_it_is_a_dry_run() {
        let root = project("menus_package_confirm");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let mut world = edit_world(Some(&root));
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(fake_regia(&bin, DONE)));
        assert_eq!(
            reason(call(
                &mut world,
                "editor.package_start",
                json!({ "target": "linux" })
            )),
            "needs_confirmation"
        );
        assert!(!bin.join("args.txt").exists(), "nothing ran");
        call(
            &mut world,
            "editor.package_start",
            json!({ "target": "linux", "dry_run": true }),
        )
        .expect("a dry run needs no confirmation");
        package_job(&world).wait();
        std::fs::remove_dir_all(&root).ok();
    }

    /// Review w8 finding 8: the real `regia clean --json` object has no `ok` key
    /// (`regia_cli/src/commands/clean.rs` `render_json`); the job's result still carries it.
    #[test]
    fn clear_caches_keeps_the_real_clean_json_report() {
        let root = project("menus_clean_report");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let mut world = edit_world(Some(&root));
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(fake_regia(
            &bin,
            "echo '{\"dry_run\":false,\"level\":\"cache\",\"removed\":true,\"paths\":[{\"path\":\".regia/cache\",\"kind\":\"dir\",\"bytes\":12}],\"bytes\":12}'",
        )));
        call(&mut world, "editor.clear_caches", json!({ "confirm": true })).expect("clear");
        let job = world.resource::<CliJobs>().clean.clone().expect("clean job");
        let outcome = job.wait().expect("outcome");
        assert_eq!(outcome["ok"], json!(true), "{outcome}");
        assert_eq!(outcome["report"]["level"], json!("cache"), "{outcome}");
        assert_eq!(outcome["report"]["bytes"], json!(12), "{outcome}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Review w8 finding 3: Rebuild asset database never waits on the cook lock on the UI thread.
    /// While a package runs it is disabled and refused (`tools_busy`); while another process
    /// holds the cook lock it is refused at once.
    #[test]
    fn rebuild_asset_db_is_refused_while_busy_instead_of_blocking() {
        let root = project("menus_rebuild_busy");
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let mut world = edit_world(Some(&root));
        world.insert_resource(regia_agent::cook_package::RegiaCliProgram(fake_regia(
            &bin,
            "exec sleep 600",
        )));
        call(
            &mut world,
            "editor.package_start",
            json!({ "target": "linux", "confirm": true }),
        )
        .expect("start");
        let job = package_job(&world);
        assert!(job.wait_for_line("READY"));
        let listed = call(&mut world, "editor.menu_list", json!({})).expect("menu_list");
        let rebuild = listed["menus"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|m| m["items"].as_array().cloned().unwrap_or_default())
            .find(|i| i["path"] == "Tools/Rebuild asset database")
            .expect("rebuild item");
        assert_eq!(rebuild["enabled"], json!(false), "disabled while a package runs");
        assert_eq!(
            reason(call(&mut world, "editor.rebuild_asset_db", json!({}))),
            "tools_busy"
        );
        call(&mut world, "editor.cancel_task", json!({ "id": "package" })).expect("cancel");
        job.wait();

        // Another process (a cook) holds the cook lock: refused at once, never waited for.
        let state = regia_core::brand::project_state_dir(&root);
        std::fs::create_dir_all(&state).expect("state");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(state.join("cook.lock"))
            .expect("lock file");
        lock.lock().expect("hold the cook lock");
        assert_eq!(
            reason(call(&mut world, "editor.rebuild_asset_db", json!({}))),
            "tools_busy"
        );
        lock.unlock().expect("unlock");
        call(&mut world, "editor.rebuild_asset_db", json!({})).expect("runs once the lock is free");
        std::fs::remove_dir_all(&root).ok();
    }
}

/// Review w8 finding 7: the icon `path` is a request path (ADR-1816): `..` and absolute paths
/// outside the allowed roots are refused, so an agent cannot copy any PNG on disk into the game.
#[test]
fn project_icon_path_is_confined() {
    let root = project("menus_icon_confined");
    let mut world = edit_world(Some(&root));
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&[0, 0, 0, 16, 0, 0, 0, 16, 8, 6, 0, 0, 0, 0xAA, 0xBB]);
    let outside = root.parent().expect("parent").join(format!(
        "{}-outside.png",
        root.file_name().expect("name").to_string_lossy()
    ));
    std::fs::write(&outside, &png).expect("outside png");
    let rel = format!("../{}", outside.file_name().expect("name").to_string_lossy());
    for path in [rel.as_str(), "/regia-no-such-root/icon.png"] {
        assert_eq!(
            reason(call(&mut world, "editor.project_icon", json!({ "path": path }))),
            "path_outside_project",
            "{path}"
        );
    }
    assert!(
        !root.join("assets/branding/icon.png").exists(),
        "nothing was copied"
    );
    std::fs::remove_file(&outside).ok();
    std::fs::remove_dir_all(&root).ok();
}

/// Review w8 finding 10: `editor.help_open` starts an OS viewer and `editor.about { open }` opens
/// a window: both change something outside the read-only set, so they are declared mutating.
#[test]
fn help_open_and_about_are_declared_mutating() {
    let world = edit_world(None);
    let extension = world.resource::<AgentExtension>().clone();
    for method in ["editor.help_open", "editor.about"] {
        let (schema, _) = extension.method(method).expect("registered");
        assert!(schema.mutating, "{method} is declared mutating");
        assert_ne!(
            schema.permission,
            regia_agent::schema::Permission::Read,
            "{method} needs more than read"
        );
    }
}

/// The real `regia` through the editor job (review w8: the fake-CLI tests never run the CLI).
/// Needs a built CLI, so it is opt-in: `cargo build -p regia_cli --bin regia`, then
/// `REGIA_CLI=<target>/debug/regia cargo test -p regia_editor --test editor_menus -- --ignored
/// real_cli`. `regia new` makes a blank project; a dry run through `editor.package_start` must end
/// `ok` with the CLI's `--json` object and leave nothing in the output folder (the run's `--out` is
/// the editor's staging folder, which is removed).
#[test]
#[ignore = "needs a built regia CLI in REGIA_CLI"]
fn real_cli_dry_run_through_the_editor_job() {
    let cli = PathBuf::from(std::env::var_os("REGIA_CLI").expect("set REGIA_CLI to a built regia"));
    let parent = regia_test_utils::unique_temp_dir("menus_real_cli");
    let status = std::process::Command::new(&cli)
        .args(["new", "Menus Real", "--template", "blank", "--path"])
        .arg(&parent)
        .status()
        .expect("regia new runs");
    assert!(status.success(), "regia new: {status}");
    let root = std::fs::read_dir(&parent)
        .expect("parent")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.join("Regia.toml").is_file())
        .expect("regia new made a project");
    let mut world = edit_world(Some(&root));
    world.insert_resource(regia_agent::cook_package::RegiaCliProgram(cli));
    let started = call(
        &mut world,
        "editor.package_start",
        json!({ "target": "linux", "dry_run": true }),
    )
    .expect("start");
    let staging = PathBuf::from(started["staging"].as_str().expect("staging"));
    let job = world
        .resource::<regia_editor::package_job::CliJobs>()
        .package
        .clone()
        .expect("job");
    let outcome = job.wait().expect("outcome");
    let lines = job.lines().join("\n");
    assert_eq!(outcome["ok"], json!(true), "{outcome}\n{lines}");
    assert!(
        outcome["report"].is_object(),
        "the CLI's --json object is kept: {outcome}\n{lines}"
    );
    assert!(!staging.exists(), "the staging folder is gone");
    assert!(
        !root.join("dist").exists(),
        "a dry run leaves no output folder: {outcome}"
    );
    std::fs::remove_dir_all(&parent).ok();
}

/// The real `regia` packages a blank project through the editor job, then a re-package into the
/// same default `dist/linux` is cancelled at once (review w8 findings 1 and 2 against the real
/// CLI's `<slug>/` rebuild and archive). Opt-in like the dry run above, and slow: it builds the
/// project's game (set `CARGO_TARGET_DIR` to a warm engine target).
#[cfg(unix)]
#[test]
#[ignore = "needs a built regia CLI in REGIA_CLI and builds a game"]
fn real_cli_package_then_cancelled_repackage_keeps_the_build() {
    let cli = PathBuf::from(std::env::var_os("REGIA_CLI").expect("set REGIA_CLI to a built regia"));
    let parent = regia_test_utils::unique_temp_dir("menus_real_package");
    let status = std::process::Command::new(&cli)
        .args(["new", "Menus Pack", "--template", "blank", "--path"])
        .arg(&parent)
        .status()
        .expect("regia new runs");
    assert!(status.success(), "regia new: {status}");
    let root = std::fs::read_dir(&parent)
        .expect("parent")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.join("Regia.toml").is_file())
        .expect("regia new made a project");
    let mut world = edit_world(Some(&root));
    world.insert_resource(regia_agent::cook_package::RegiaCliProgram(cli));
    let package_job = |world: &World| {
        world
            .resource::<regia_editor::package_job::CliJobs>()
            .package
            .clone()
            .expect("job")
    };
    call(
        &mut world,
        "editor.package_start",
        json!({ "target": "linux", "confirm": true }),
    )
    .expect("start");
    let job = package_job(&world);
    let outcome = job.wait().expect("outcome");
    let lines = job.lines().join("\n");
    assert_eq!(outcome["ok"], json!(true), "{outcome}\n{lines}");
    let out = root.join("dist/linux");
    let artifact = PathBuf::from(outcome["artifact"].as_str().expect("artifact"));
    assert_eq!(artifact.parent(), Some(out.as_path()), "{outcome}");
    assert!(artifact.is_file(), "{outcome}");
    let names = |dir: &Path| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let built = names(&out);
    assert!(
        !built
            .iter()
            .any(|n| n.starts_with(regia_editor::package_job::STAGING_PREFIX)),
        "{built:?}"
    );
    let archive = std::fs::read(&artifact).expect("archive");

    // Re-package into the same folder and cancel at once: the previous build stays as it was.
    call(
        &mut world,
        "editor.package_start",
        json!({ "target": "linux", "confirm": true }),
    )
    .expect("restart");
    let job = package_job(&world);
    job.cancel();
    let outcome = job.wait().expect("outcome");
    assert_eq!(outcome["code"], json!("package_cancelled"), "{outcome}");
    assert_eq!(names(&out), built, "the previous build's entries are all there");
    assert_eq!(
        std::fs::read(&artifact).ok(),
        Some(archive),
        "the archive is unchanged"
    );
    std::fs::remove_dir_all(&parent).ok();
}
