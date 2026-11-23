// SPDX-License-Identifier: MPL-2.0
//! The golden tools through the real dispatcher (ADR-2821): names, jobs, reports, proposals, and the
//! rule that no sequence of agent calls under any profile or scope changes a golden. Temp projects
//! only: nothing here creates or touches a real golden. Every wait is on a job's state or a process,
//! bounded by a hang guard, never on a timer.
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use regia_render::CapturedImage;

use super::*;
use crate::dispatch::{AgentInbox, RateLimit, process_requests, process_scoped_requests};
use crate::protocol::{Request, Response};
use crate::safety::{AgentConfig, AgentSession, AuditLog};
use crate::scope::{ScopedInbound, ScopedInbox, ToolScope, ToolTokens};
use crate::server::Inbound;

const GOLDEN_REL: &str = "crates/demo/goldens/sponza.png";
const NAME: &str = "demo:sponza";

const EVERY_PERMISSION: &[Permission] = &[
    Permission::Read,
    Permission::EditScene,
    Permission::Play,
    Permission::Assets,
    Permission::Tools,
    Permission::Build,
    Permission::Review,
];

const ALL_PERMISSIONS: &[&str] = &[
    "read",
    "edit_scene",
    "assets",
    "play",
    "build",
    "tools",
    "review",
    "cook",
    "package",
    "delete",
    "bless",
];

fn image(width: u32, height: u32, shade: u8) -> CapturedImage {
    let mut rgba = Vec::new();
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&[shade, (x * 8) as u8, (y * 8) as u8, 255]);
        }
    }
    CapturedImage { width, height, rgba }
}

fn write_png(path: &Path, picture: &CapturedImage) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, crate::image::png_bytes(picture).expect("png")).expect("write png");
}

/// A workspace with one package, one golden and the test that compares it.
fn project(tag: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/demo\"]\n",
    )
    .expect("root");
    let demo = root.join("crates/demo");
    std::fs::create_dir_all(demo.join("tests")).expect("tests");
    std::fs::write(
        demo.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    std::fs::write(
        demo.join("tests/golden_scene.rs"),
        "fn helper() {}\n\n/// Doc.\n#[test]\nfn sponza_matches() {\n    check(&golden_path(\"sponza\"));\n}\n",
    )
    .expect("test");
    write_png(&root.join(GOLDEN_REL), &image(16, 8, 40));
    root
}

fn app(root: &Path, permissions: &[&str]) -> App {
    let config = AgentConfig {
        permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
        rate_limit_per_second: u32::MAX,
        heavy_rate_limit_per_second: u32::MAX,
        ..AgentConfig::default()
    };
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentPermissions { config, source: None })
        .insert_resource(crate::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        })
        .init_resource::<AuditLog>()
        .insert_resource(AgentSession {
            client: "golden-test".into(),
            model: "test-model".into(),
            session: "s1".into(),
        })
        .init_resource::<crate::playtest::AgentTimeControl>()
        .init_resource::<RateLimit>();
    app
}

fn request(method: &str, params: Value) -> Request {
    Request {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params,
    }
}

fn call(app: &mut App, method: &str, params: Value) -> Response {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
    inbox_tx
        .send(Inbound {
            request: request(method, params),
            reply: tx,
        })
        .expect("queued");
    app.world_mut().insert_resource(AgentInbox(inbox_rx));
    process_requests(app.world_mut());
    rx.recv_timeout(Duration::from_secs(5)).expect("answered")
}

fn ok(app: &mut App, method: &str, params: Value) -> Value {
    let response = call(app, method, params);
    response
        .result
        .unwrap_or_else(|| panic!("{method}: {:?}", response.error))
}

fn reason(response: &Response) -> String {
    response
        .error
        .as_ref()
        .and_then(|e| e.data.as_ref())
        .and_then(|d| d.get("reason").or_else(|| d.get("code")))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The bytes of every file under `dir`, by relative path.
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(folder) = pending.pop() {
        for entry in std::fs::read_dir(&folder).expect("read dir").flatten() {
            if entry.file_type().expect("kind").is_dir() {
                pending.push(entry.path());
            } else {
                files.insert(
                    rel_of(dir, &entry.path()),
                    std::fs::read(entry.path()).expect("read file"),
                );
            }
        }
    }
    files
}

/// Waits for a job to end (a hang guard, not a speed check).
#[cfg(unix)]
fn finished(app: &mut App, job: &str) -> Value {
    for _ in 0..60_000 {
        let status = ok(app, "agent.job_status", json!({ "job": job }));
        if status["finished"] == true {
            return status;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("job {job} never finished");
}

/// How a fake cargo behaves.
#[cfg(unix)]
#[derive(Clone, Copy)]
struct Fake {
    /// The test passes (else it fails, as a compare under threshold does).
    pass: bool,
    /// The compare recorded a plate.
    plate: bool,
    /// Any test ran at all (else a build error).
    ran: bool,
}

#[cfg(unix)]
const FAILING: Fake = Fake {
    pass: false,
    plate: true,
    ran: true,
};

/// A "cargo" that behaves like a golden compare: it obeys `REGIA_BLESS` by overwriting the golden,
/// obeys `REGIA_SKIP_GOLDENS` by passing unseen, and otherwise records the plate and reports.
#[cfg(unix)]
fn fake_cargo(root: &Path, fake: Fake) -> PathBuf {
    let actual = root.join("actual-frame.png");
    write_png(&actual, &image(16, 8, 200));
    let golden = root.join(GOLDEN_REL);
    let mut script = format!(
        "#!/bin/sh\nfor v in \"$REGIA_BLESS\" \"$FERRITE_BLESS\"; do\n  if [ \"$v\" = 1 ]; then cp '{actual}' '{golden}'; echo 'running 1 test'; echo 'test sponza_matches ... ok'; exit 0; fi\ndone\nfor v in \"$REGIA_SKIP_GOLDENS\" \"$FERRITE_SKIP_GOLDENS\"; do\n  if [ \"$v\" = 1 ]; then echo 'running 1 test'; echo 'test sponza_matches ... ok'; exit 0; fi\ndone\n", // rename-keep: the legacy spelling is read too
        actual = actual.display(),
        golden = golden.display(),
    );
    if !fake.ran {
        script.push_str("echo 'error: could not compile `demo`'\nexit 101\n");
    } else {
        if fake.plate {
            script.push_str(&format!(
                "cp '{}' \"$REGIA_PLATE_DIR/demo-sponza.png\"\n",
                actual.display()
            ));
        }
        script.push_str("echo 'running 1 test'\n");
        if fake.pass {
            script.push_str("echo 'test sponza_matches ... ok'\nexit 0\n");
        } else {
            script.push_str("echo 'test sponza_matches ... FAILED'\nexit 101\n");
        }
    }
    let path = root.join(format!("fake-cargo-{}.sh", crate::safety::unix_ms_now()));
    // Written by a child process: no sibling test's fork can hold it open for writing when it
    // runs (ETXTBSY, ADR-5959).
    regia_test_utils::process::write_executable_script(&path, &script).expect("script");
    path
}

/// An app with the fake cargo, and a finished `golden_test` job of it.
#[cfg(unix)]
fn tested(root: &Path, permissions: &[&str], fake: Fake) -> (App, String, Value) {
    let script = fake_cargo(root, fake);
    let mut agent = app(root, permissions);
    agent
        .world_mut()
        .insert_resource(crate::run_tests::RunTestsProgram(script));
    let started = ok(&mut agent, "agent.golden_test", json!({ "name": "sponza" }));
    let job = started["job"].as_str().expect("job").to_string();
    assert!(job.starts_with("golden:"), "{started}");
    let status = finished(&mut agent, &job);
    (agent, job, status)
}

#[test]
fn golden_paths_are_seen_through_spelling() {
    for path in [
        "goldens/a.png",
        "crates/x/goldens/a.png",
        "crates\\x\\GOLDENS\\a.png",
        "crates/x/./goldens/a.png",
        "crates/y/../x/goldens/a.png",
        "goldens",
        "goldens/",
        "x/goldens. /a.png",
    ] {
        assert!(names_golden(path), "{path}");
    }
    for path in [
        "book/agents/goldens.md",
        "golden/a.png",
        "my_goldens/a.png",
        "a.png",
        "",
    ] {
        assert!(!names_golden(path), "{path}");
    }
}

#[test]
fn the_guard_reads_nested_strings_and_skips_free_text() {
    let denied = |params: Value| guard_params("assets.create", &params).is_err();
    assert!(denied(json!({ "path": GOLDEN_REL })));
    assert!(denied(
        json!({ "edits": [{ "out": { "file": "x/goldens/y.png" } }] })
    ));
    assert!(denied(json!({ "list": ["ok.png", "goldens/y.png"] })));
    assert!(!denied(
        json!({ "path": "assets/a.png", "reason": "see goldens/y.png" })
    ));
    assert!(guard_params("agent.golden_test", &json!({ "name": "goldens/y" })).is_ok());
    // A target named `goldens` is no path, and base64 has `/` in its alphabet.
    assert!(guard_params("agent.run_tests", &json!({ "target": "goldens" })).is_ok());
    assert!(guard_params("seeing.compare_images", &json!({ "a": "x/goldens/y" })).is_ok());
    // Text holding a path, as a job passed inline, one line or many, or a line break inside a path.
    assert!(denied(
        json!({ "job": "(out:\"crates/demo/goldens\",takes:[(name:\"a\")])" })
    ));
    assert!(denied(
        json!({ "job": "(\n    out: \"crates/demo/goldens\",\n)\n" })
    ));
    assert!(denied(json!({ "job": "(path:\"goldens/{take}\")" })));
    assert!(denied(json!({ "path": "crates/x/goldens/a\n.png" })));
    assert!(denied(json!({ "path": "C:\\regia-work\\goldens\\a.png" })));
    assert!(denied(json!({ "path": "my dir/goldens/a.png" })));
    assert!(!denied(
        json!({ "job": "(out:\"renders/take\",takes:[(name:\"golden_hour\")])" })
    ));
}

#[test]
fn stats_match_the_test_crate_measure() {
    let mut picture = image(32, 16, 250);
    for pixel in picture.rgba.as_chunks_mut::<4>().0.iter_mut().take(100) {
        *pixel = [0, 0, 0, 255];
    }
    for pixel in picture.rgba.as_chunks_mut::<4>().0.iter_mut().skip(100).take(50) {
        *pixel = [255, 255, 255, 255];
    }
    let stats = frame_stats(&picture);
    let measured = regia_test_utils::golden::measure_golden(&picture.rgba, 32, 16);
    assert_eq!(stats["mean_lum"], measured.mean_lum);
    assert_eq!(stats["near_white_pct"], measured.near_white_pct);
    assert_eq!(stats["near_black_pct"], measured.near_black_pct);
    assert_eq!(stats["object_pct"], measured.object_pct);
}

#[test]
fn psnr_matches_the_test_crate() {
    let golden = image(8, 8, 40);
    let actual = image(8, 8, 43);
    assert_eq!(
        psnr_rgb(&actual, &golden),
        regia_test_utils::golden::psnr_rgb8(&actual.rgba, &golden.rgba)
    );
    assert_eq!(psnr_rgb(&golden, &golden), None, "identical frames have none");
}

#[test]
fn the_scan_finds_goldens_and_skips_build_output() {
    let root = project("golden-scan");
    write_png(&root.join("target/x/goldens/built.png"), &image(4, 4, 1));
    write_png(&root.join(".regia/goldens/state.png"), &image(4, 4, 1));
    write_png(&root.join("crates/demo/goldens/lens/fog.png"), &image(4, 4, 1));
    std::fs::write(root.join("crates/demo/goldens/notes.txt"), "not an image").expect("notes");
    let entries = scan_goldens(&root);
    let names: Vec<String> = entries.iter().map(GoldenEntry::qualified).collect();
    assert_eq!(names, ["lens:fog", NAME], "{entries:?}");
    assert_eq!(entries[0].plate_names()[0], "lens-fog");
    assert_eq!(entries[1].plate_names()[0], "demo-sponza");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unknown_names_answer_close_matches() {
    let root = project("golden-unknown");
    write_png(
        &root.join("crates/demo/goldens/sponza_night.png"),
        &image(4, 4, 1),
    );
    write_png(&root.join("crates/demo/goldens/zzz.png"), &image(4, 4, 1));
    let mut agent = app(&root, ALL_PERMISSIONS);
    let response = call(&mut agent, "agent.golden_test", json!({ "name": "sponzza" }));
    assert_eq!(reason(&response), "golden_unknown");
    let data = response.error.and_then(|e| e.data).expect("data");
    let close: Vec<&str> = data["close"]
        .as_array()
        .expect("close")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(close[0], "sponza", "{data}");
    assert!(!close.contains(&"zzz"), "{data}");
    for bad in ["", "-x", "a/b", "a b", "../goldens/sponza"] {
        let refused = call(&mut agent, "agent.golden_test", json!({ "name": bad }));
        assert_eq!(reason(&refused), "golden_bad_name", "{bad:?}");
    }
    let missing = call(&mut agent, "agent.golden_test", json!({}));
    assert_eq!(
        missing.error.map(|e| e.code),
        Some(INVALID_PARAMS),
        "name is required"
    );
    let wrong_type = call(&mut agent, "agent.golden_test", json!({ "name": 3 }));
    assert_eq!(wrong_type.error.map(|e| e.code), Some(INVALID_PARAMS));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_stem_in_two_folders_needs_its_owner() {
    let root = project("golden-ambiguous");
    write_png(&root.join("crates/other/goldens/sponza.png"), &image(4, 4, 1));
    let mut agent = app(&root, ALL_PERMISSIONS);
    let response = call(&mut agent, "agent.golden_test", json!({ "name": "sponza" }));
    assert_eq!(reason(&response), "golden_ambiguous");
    let data = response.error.and_then(|e| e.data).expect("data");
    assert_eq!(data["matches"], json!([NAME, "other:sponza"]));
    let dry = ok(
        &mut agent,
        "agent.golden_test",
        json!({ "name": NAME, "dry_run": true }),
    );
    assert_eq!(dry["path"], GOLDEN_REL);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_test_is_the_enclosing_test_of_the_literal() {
    let root = project("golden-resolve");
    let entries = scan_goldens(&root);
    let test = find_test(&root, pick_golden(&entries, "sponza").expect("entry")).expect("test");
    assert_eq!(
        test,
        GoldenTest {
            package: Some("demo".into()),
            target: "golden_scene".into(),
            filter: Some("sponza_matches".into()),
            file: "crates/demo/tests/golden_scene.rs".into(),
        }
    );
    assert_eq!(
        test.args().expect("args"),
        [
            "test",
            "--color",
            "never",
            "--package",
            "demo",
            "--test",
            "golden_scene",
            "--offline",
            "--",
            "sponza_matches",
            "--exact"
        ]
    );
    // A literal in a helper runs the whole target.
    std::fs::write(
        root.join("crates/demo/tests/golden_scene.rs"),
        "const NAME: &str = \"sponza\";\n#[test]\nfn t() {}\n",
    )
    .expect("test");
    let test = find_test(&root, &entries[0]).expect("test");
    assert_eq!(test.filter, None);
    // No test names it: the job fails with a documented code.
    std::fs::write(
        root.join("crates/demo/tests/golden_scene.rs"),
        "#[test]\nfn t() {}\n",
    )
    .expect("test");
    assert_eq!(
        find_test(&root, &entries[0]).map_err(|e| e.0),
        Err("golden_no_test")
    );
    // A library test of the owning package counts, last.
    std::fs::create_dir_all(root.join("crates/demo/src")).expect("src");
    std::fs::write(
        root.join("crates/demo/src/lib.rs"),
        "#[cfg(test)]\nmod t {\n    #[test]\n    fn inner() { c(\"sponza\"); }\n}\n",
    )
    .expect("lib");
    let test = find_test(&root, &entries[0]).expect("lib test");
    assert_eq!(
        (test.target.as_str(), test.filter.as_deref()),
        ("lib", Some("inner"))
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn enclosing_functions_are_read_from_a_plain_source() {
    let source = "#[test]\nfn one() {\n    a(\"x\");\n}\n\nfn helper() {\n    a(\"y\");\n}\n\n#[tokio::test]\nasync fn two() {\n    a(\"z\");\n}\n#[test]\n#[ignore = \"slow\"]\npub fn three() {\n    a(\"w\");\n}\n";
    let at = |needle: &str| source.find(needle).expect("needle");
    assert_eq!(enclosing_test_fn(source, at("\"x\"")).as_deref(), Some("one"));
    assert_eq!(enclosing_test_fn(source, at("\"y\"")), None);
    assert_eq!(enclosing_test_fn(source, at("\"z\"")).as_deref(), Some("two"));
    assert_eq!(enclosing_test_fn(source, at("\"w\"")).as_deref(), Some("three"));
    assert_eq!(enclosing_test_fn(source, source.len() + 1), None);
}

#[test]
fn a_numbered_name_is_found_by_its_format_string() {
    let source = "#[test]\nfn shadows() {\n    for i in 0..3 { c(&format!(\"shadow_cam{i}\")); }\n}\n";
    let offset = literal_offset(source, "shadow_cam2").expect("found");
    assert_eq!(enclosing_test_fn(source, offset).as_deref(), Some("shadows"));
    assert_eq!(literal_offset(source, "shadow_sun2"), None);
    assert_eq!(literal_offset(source, "shadow_cam"), None, "not numbered");
    // Too short a stem says nothing.
    assert_eq!(literal_offset("c(&format!(\"a{i}\"))", "a1"), None);
}

/// Every golden in this repository resolves to a test. A new golden no test names as a literal fails
/// here: name it in its test, or list it with why.
#[test]
fn the_repository_goldens_resolve_to_tests() {
    const BUILT_AT_RUN_TIME: &[&str] = &[];
    let root = regia_test_utils::book::workspace_root();
    let entries = scan_goldens(&root);
    assert!(entries.len() >= 60, "the scan found {} goldens", entries.len());
    let unresolved: Vec<String> = entries
        .iter()
        .filter(|entry| find_test(&root, entry).is_err())
        .map(GoldenEntry::qualified)
        .filter(|name| !BUILT_AT_RUN_TIME.contains(&name.as_str()))
        .collect();
    assert!(unresolved.is_empty(), "no test found for: {unresolved:?}");
}

/// The plate this module looks for is the one the real compare writes. A child process, so the
/// compare sees its own environment and a temp directory that does not hold the golden.
#[test]
fn the_plate_name_is_the_one_the_real_compare_writes() {
    let root = project("golden-plate-contract");
    let plates = root.join("plates");
    let status = Command::new(std::env::current_exe().expect("exe"))
        .args([
            "--exact",
            "golden_tools::tests::plate_child",
            "--ignored",
            "--nocapture",
        ])
        .env("GOLDEN_PLATE_CHILD", root.join(GOLDEN_REL))
        .env("REGIA_PLATE_DIR", &plates)
        .env("TMPDIR", root.join("tmp"))
        .env("TMP", root.join("tmp"))
        .env("TEMP", root.join("tmp"))
        .env_remove("REGIA_BLESS")
        .env_remove("FERRITE_BLESS") // rename-keep: the legacy spelling is read too
        .status()
        .expect("child");
    assert!(status.success(), "{status}");
    let entries = scan_goldens(&root);
    let [plate, _] = entries[0].plate_names();
    assert!(
        plates.join(format!("{plate}.png")).is_file(),
        "{plate}.png not in {:?}",
        tree(&plates).keys()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The child of [`the_plate_name_is_the_one_the_real_compare_writes`].
#[test]
#[ignore = "run by its parent test, with its environment"]
fn plate_child() {
    let Some(golden) = std::env::var_os("GOLDEN_PLATE_CHILD").map(PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(std::env::temp_dir()).expect("temp");
    let picture = image(16, 8, 40);
    regia_test_utils::golden::compare_rgba8_to_png(&picture.rgba, 16, 8, &golden, 0.5).expect("compare");
}

#[cfg(unix)]
#[test]
fn a_finished_job_reports_expected_actual_diff_and_stats() {
    let root = project("golden-report");
    let before = tree(&root.join("crates/demo/goldens"));
    let (mut agent, _, status) = tested(&root, ALL_PERMISSIONS, FAILING);
    assert_eq!(status["state"], "done", "{status}");
    let result = &status["result"];
    assert_eq!(result["ok"], false);
    assert_eq!(result["failed"], 1);
    let golden = &result["golden"];
    assert_eq!(golden["name"], NAME);
    assert_eq!(golden["expected"], GOLDEN_REL);
    assert_eq!(golden["test"]["filter"], "sponza_matches");
    for key in ["actual", "diff"] {
        let rel = golden[key].as_str().unwrap_or_else(|| panic!("{key}: {golden}"));
        assert!(root.join(rel).is_file(), "{rel}");
        assert!(rel.starts_with(".regia/agent/golden_jobs/"), "{rel}");
    }
    let stats = &golden["stats"];
    let measured = regia_test_utils::golden::measure_golden(&image(16, 8, 200).rgba, 16, 8);
    assert_eq!(stats["mean_lum"], measured.mean_lum);
    assert_eq!(stats["object_pct"], measured.object_pct);
    assert!(stats["ssim"].as_f64().expect("ssim") < 1.0, "{stats}");
    assert_eq!(stats["max_channel_diff"], 160);
    assert_eq!(stats["identical"], false);
    assert!(stats["psnr"].is_f64(), "{stats}");
    assert_eq!(
        tree(&root.join("crates/demo/goldens")),
        before,
        "a test writes no golden"
    );
    // The events carry the resolved test.
    let events = ok(&mut agent, "agent.job_status", json!({ "job": status["job"] }));
    assert_eq!(events["events"][0]["kind"], "resolved");
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn a_job_with_no_plate_says_so() {
    let root = project("golden-no-plate");
    let (_, _, status) = tested(
        &root,
        ALL_PERMISSIONS,
        Fake {
            pass: true,
            plate: false,
            ran: true,
        },
    );
    assert_eq!(status["result"]["ok"], true, "{status}");
    assert_eq!(status["result"]["golden"]["actual"], Value::Null);
    assert!(status["result"]["golden"]["note"].is_string(), "{status}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn golden_test_needs_read_and_build() {
    let root = project("golden-permissions");
    let mut only_read = app(&root, &["read", "assets"]);
    let denied = call(&mut only_read, "agent.golden_test", json!({ "name": "sponza" }));
    assert_eq!(reason(&denied), "F0012", "build is needed");
    let mut only_build = app(&root, &["build"]);
    let denied = call(&mut only_build, "agent.golden_test", json!({ "name": "sponza" }));
    assert_eq!(reason(&denied), "F0012", "read is needed too");
    let schema = crate::schema::method("agent.golden_test").expect("schema");
    assert_eq!(schema.permission, Permission::Build);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn no_tool_scope_can_run_a_golden_test() {
    let root = project("golden-scope");
    let tokens = ToolTokens::default();
    let scope = ToolScope::new("fixture", EVERY_PERMISSION.iter().copied(), vec!["*".to_string()]);
    let (_, scope) = tokens.issue(scope);
    let mut agent = app(&root, ALL_PERMISSIONS);
    let answer = scoped_call(
        &mut agent,
        &tokens,
        &scope,
        "agent.golden_test",
        json!({ "name": "sponza" }),
    );
    assert_eq!(reason(&answer), crate::scope::TOOL_SCOPE_DENIED);
    let _ = std::fs::remove_dir_all(&root);
}

fn scoped_call(
    app: &mut App,
    tokens: &ToolTokens,
    scope: &Arc<ToolScope>,
    method: &str,
    params: Value,
) -> Response {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
    inbox_tx
        .send(ScopedInbound {
            request: request(method, params),
            reply: tx,
            scope: Arc::clone(scope),
        })
        .expect("queued");
    app.world_mut().insert_resource(ScopedInbox {
        inbox: inbox_rx,
        tokens: tokens.clone(),
    });
    process_scoped_requests(app.world_mut());
    rx.recv_timeout(Duration::from_secs(5)).expect("answered")
}

#[cfg(unix)]
#[test]
fn bless_refuses_a_missing_reason() {
    let root = project("golden-no-reason");
    let (mut agent, job, _) = tested(&root, ALL_PERMISSIONS, FAILING);
    for params in [
        json!({ "name": "sponza", "job": job }),
        json!({ "name": "sponza", "job": job, "reason": "" }),
        json!({ "name": "sponza", "job": job, "reason": "   \n" }),
    ] {
        let refused = call(&mut agent, "agent.bless_golden", params);
        assert_eq!(reason(&refused), "golden_reason_required");
    }
    assert_eq!(pending_proposals(&root).expect("queue").len(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn bless_refuses_a_job_that_did_not_run() {
    let root = project("golden-no-job");
    let mut agent = app(&root, ALL_PERMISSIONS);
    let bless = |agent: &mut App, job: &str| {
        reason(&call(
            agent,
            "agent.bless_golden",
            json!({ "name": "sponza", "reason": "intended", "job": job }),
        ))
    };
    // Never started.
    assert_eq!(bless(&mut agent, "golden:99"), "golden_job_not_found");
    // Another kind of job.
    let (other, handle) = agent
        .world_mut()
        .get_resource_or_insert_with::<JobRegistry>(Default::default)
        .start("tests", "unit", false);
    handle.finish(json!({ "golden": { "name": NAME, "actual": "x.png" }, "passed": 1 }));
    assert_eq!(bless(&mut agent, &other), "golden_job_mismatch");
    // Still running.
    let (running, handle) = agent
        .world_mut()
        .get_resource_or_insert_with::<JobRegistry>(Default::default)
        .start("golden", NAME, true);
    assert_eq!(bless(&mut agent, &running), "golden_job_not_run");
    // Cancelled.
    handle.request_cancel();
    handle.finish(json!({ "golden": { "name": NAME, "actual": "x.png" }, "passed": 1 }));
    assert_eq!(bless(&mut agent, &running), "golden_job_not_run");
    // Failed before running a test.
    let (failed, handle) = agent
        .world_mut()
        .get_resource_or_insert_with::<JobRegistry>(Default::default)
        .start("golden", NAME, true);
    handle.fail("golden_no_test", "none");
    assert_eq!(bless(&mut agent, &failed), "golden_job_not_run");
    // The build failed: no test ran and no frame exists.
    let (mut built, job, status) = tested(
        &root,
        ALL_PERMISSIONS,
        Fake {
            pass: false,
            plate: false,
            ran: false,
        },
    );
    assert_eq!(status["result"]["build_failed"], true, "{status}");
    assert_eq!(bless(&mut built, &job), "golden_job_not_run");
    // A test ran but recorded no frame.
    let (mut no_plate, job, _) = tested(
        &root,
        ALL_PERMISSIONS,
        Fake {
            pass: true,
            plate: false,
            ran: true,
        },
    );
    assert_eq!(bless(&mut no_plate, &job), "golden_job_not_run");
    // A job of another golden.
    write_png(&root.join("crates/demo/goldens/other.png"), &image(4, 4, 9));
    let (mut wrong, job, _) = tested(&root, ALL_PERMISSIONS, FAILING);
    let refused = call(
        &mut wrong,
        "agent.bless_golden",
        json!({ "name": "other", "reason": "intended", "job": job }),
    );
    assert_eq!(reason(&refused), "golden_job_mismatch");
    assert_eq!(
        pending_proposals(&root).expect("queue").len(),
        0,
        "nothing staged"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn bless_stages_before_after_and_diff_and_writes_no_golden() {
    let root = project("golden-stage");
    let goldens = root.join("crates/demo/goldens");
    let before = tree(&goldens);
    let (mut agent, job, _) = tested(&root, &["read", "build", "assets"], FAILING);
    let params = json!({ "name": "sponza", "reason": "the new sun angle is intended", "job": job });
    let dry = ok(
        &mut agent,
        "agent.bless_golden",
        json!({ "name": "sponza", "reason": "x", "job": job, "dry_run": true }),
    );
    assert_eq!(dry["staged"], false);
    assert_eq!(pending_proposals(&root).expect("queue").len(), 0);
    let staged = ok(&mut agent, "agent.bless_golden", params.clone());
    assert_eq!(staged["status"], "pending");
    assert_eq!(staged["persisted"], true);
    assert_eq!(staged["accept"], format!("regia golden accept {}", staged["id"]));
    for key in ["before", "after", "diff"] {
        let rel = staged[key].as_str().expect("path");
        assert!(root.join(rel).is_file(), "{rel}");
        assert!(
            !names_golden(rel),
            "the staging folder is not a goldens folder: {rel}"
        );
    }
    assert_eq!(
        std::fs::read(root.join(staged["before"].as_str().expect("before"))).expect("before"),
        before["sponza.png"]
    );
    assert_eq!(tree(&goldens), before, "bless_golden writes no golden");
    // A second proposal of the same golden waits for the first.
    let again = call(&mut agent, "agent.bless_golden", params);
    assert_eq!(reason(&again), "golden_already_pending");
    // The queue shows it, with the images and how it is accepted.
    let listed = ok(&mut agent, "review.list", json!({}));
    let row = &listed["changes"][0];
    assert_eq!(row["path"], GOLDEN_REL);
    assert_eq!(row["reason"], "the new sun angle is intended");
    assert_eq!(row["client"], "golden-test");
    assert_eq!(row["golden"]["name"], NAME);
    assert_eq!(row["golden"]["diff"], staged["diff"]);
    assert_eq!(row["images"]["after"], staged["after"]);
    let diff = ok(&mut agent, "review.diff", json!({ "id": staged["id"] }));
    let text = diff["diff"].as_str().expect("diff");
    assert!(text.contains("mean_lum"), "{text}");
    assert!(text.contains("regia golden accept"), "{text}");
    // The proposer withdraws it: the images go, the golden stays.
    let withdrawn = ok(&mut agent, "review.reject", json!({ "ids": [staged["id"]] }));
    assert_eq!(withdrawn["decided"], 1);
    assert!(!root.join(staged["after"].as_str().expect("after")).exists());
    assert_eq!(tree(&goldens), before);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn bless_refuses_an_identical_image_and_a_moved_on_golden() {
    let root = project("golden-stale");
    let (mut agent, job, _) = tested(&root, ALL_PERMISSIONS, FAILING);
    let params = json!({ "name": "sponza", "reason": "intended", "job": job });
    // The actual image was replaced after the test.
    let golden = ok(&mut agent, "agent.job_status", json!({ "job": job }))["result"]["golden"].clone();
    let actual = root.join(golden["actual"].as_str().expect("actual"));
    let frame = std::fs::read(&actual).expect("frame");
    write_png(&actual, &image(16, 8, 9));
    assert_eq!(
        reason(&call(&mut agent, "agent.bless_golden", params.clone())),
        "golden_stale"
    );
    std::fs::write(&actual, &frame).expect("restore");
    // The golden moved on after the test.
    let original = std::fs::read(root.join(GOLDEN_REL)).expect("golden");
    write_png(&root.join(GOLDEN_REL), &image(16, 8, 77));
    assert_eq!(
        reason(&call(&mut agent, "agent.bless_golden", params.clone())),
        "golden_stale"
    );
    std::fs::write(root.join(GOLDEN_REL), &original).expect("restore");
    // An identical frame has nothing to bless.
    let same = root.join("crates/demo/goldens/sponza.png");
    std::fs::copy(&same, root.join("actual-frame-same.png")).expect("copy");
    let (mut identical, job, _) = {
        let script = fake_cargo(&root, FAILING);
        let text = std::fs::read_to_string(&script)
            .expect("script")
            .replace("actual-frame.png", "actual-frame-same.png");
        std::fs::write(&script, text).expect("script");
        let mut agent = app(&root, ALL_PERMISSIONS);
        agent
            .world_mut()
            .insert_resource(crate::run_tests::RunTestsProgram(script));
        let started = ok(&mut agent, "agent.golden_test", json!({ "name": "sponza" }));
        let job = started["job"].as_str().expect("job").to_string();
        let status = finished(&mut agent, &job);
        (agent, job, status)
    };
    let refused = call(
        &mut identical,
        "agent.bless_golden",
        json!({ "name": "sponza", "reason": "intended", "job": job }),
    );
    assert_eq!(reason(&refused), "golden_unchanged");
    let _ = std::fs::remove_dir_all(&root);
}

/// A job file the guard never saw: its `out`, or a take folder from a template, still stops at the
/// render-out sink, as do the other path helpers handlers build on.
#[test]
fn paths_from_files_and_built_paths_stop_at_the_sinks() {
    let root = project("golden-sinks");
    let refused = |result: Result<PathBuf, RpcError>| {
        let error = result.expect_err("refused");
        error.data.as_ref().map(|d| d["reason"].clone()) == Some(json!("golden_path_denied"))
    };
    assert!(refused(crate::req_path::project_path_in(
        &root,
        "path",
        "crates/demo/goldens/x.png"
    )));
    let agent = app(&root, ALL_PERMISSIONS);
    assert!(refused(crate::req_path::allowed_path_in(
        agent.world(),
        &root,
        "out",
        "crates/demo/goldens/x.png"
    )));
    let derived = crate::req_path::check_derived(&root, "path", &root.join("crates/demo/goldens/x.png"));
    assert!(refused(derived.map(|()| PathBuf::new())));
    // A project that itself lives under a folder named so is fine: only the part below it counts.
    assert!(!under_goldens(
        &root.join("goldens/project"),
        &root.join("goldens/project/renders/a")
    ));
    let _ = std::fs::remove_dir_all(&root);
}

/// A relative project root (`regia-editor --project .`): cargo runs a test in its package's folder,
/// so the plate folder in its environment has to be absolute. A child process runs in the project.
#[cfg(unix)]
#[test]
fn a_relative_project_root_still_finds_the_frame() {
    let root = project("golden-relative-root");
    let status = Command::new(std::env::current_exe().expect("exe"))
        .args([
            "--exact",
            "golden_tools::tests::relative_root_child",
            "--ignored",
            "--nocapture",
        ])
        .current_dir(&root)
        .env("GOLDEN_RELATIVE_CHILD", "1")
        .status()
        .expect("child");
    assert!(status.success(), "{status}");
    let _ = std::fs::remove_dir_all(&root);
}

/// The child of [`a_relative_project_root_still_finds_the_frame`].
#[cfg(unix)]
#[test]
#[ignore = "run by its parent test, in the project folder"]
fn relative_root_child() {
    if std::env::var_os("GOLDEN_RELATIVE_CHILD").is_none() {
        return;
    }
    let script = fake_cargo(Path::new("."), FAILING);
    // As cargo does: the test runs in its package's folder.
    let text = std::fs::read_to_string(&script).expect("script");
    let absolute = std::path::absolute(".").expect("cwd");
    let text = text
        .replace("cp './", &format!("cp '{}/", absolute.display()))
        .replace("#!/bin/sh\n", "#!/bin/sh\ncd crates/demo\n");
    std::fs::write(&script, text).expect("script");
    let mut agent = app(Path::new("."), ALL_PERMISSIONS);
    agent
        .world_mut()
        .insert_resource(crate::run_tests::RunTestsProgram(
            std::path::absolute(&script).expect("script path"),
        ));
    let started = ok(&mut agent, "agent.golden_test", json!({ "name": "sponza" }));
    let status = finished(&mut agent, started["job"].as_str().expect("job"));
    let golden = &status["result"]["golden"];
    let actual = golden["actual"]
        .as_str()
        .unwrap_or_else(|| panic!("no frame: {status}"));
    assert!(Path::new(actual).is_file(), "{actual}");
    let staged = ok(
        &mut agent,
        "agent.bless_golden",
        json!({ "name": "sponza", "reason": "intended", "job": status["job"] }),
    );
    assert_eq!(staged["status"], "pending", "{staged}");
}

#[test]
fn stopping_running_jobs_cancels_them_and_leaves_the_rest() {
    let mut registry = JobRegistry::default();
    let (_, cancellable) = registry.start("tests", "a", true);
    let (_, fixed) = registry.start("tests", "b", false);
    let worker = cancellable.clone();
    let thread = std::thread::spawn(move || {
        while !worker.cancel_requested() {
            std::thread::yield_now();
        }
        worker.finish(json!({}));
    });
    assert_eq!(registry.stop_running(Duration::from_secs(600)), 0);
    thread.join().expect("worker");
    assert_eq!(cancellable.state(), JobState::Cancelled);
    assert_eq!(
        fixed.state(),
        JobState::Running,
        "a job without a cancel is left alone"
    );
}

#[test]
fn agent_text_is_shown_not_sent_to_the_terminal() {
    assert_eq!(plain_text("plain Ünicode"), "plain Ünicode");
    assert_eq!(
        plain_text("a\u{1b}[2Jb\nReason: forged"),
        "a\\u{1b}[2Jb\\nReason: forged"
    );
}

/// The hard rule of ADR-2821. Every method of the schema, under every profile and under a tool scope
/// holding everything it can, is called with the golden's path in each string parameter and in its
/// contract example's strings. Then the legitimate sequence runs and every accept an agent has is
/// tried. The golden directory never changes, and the proposal stays pending.
#[cfg(unix)]
#[test]
fn no_sequence_of_agent_calls_under_any_scope_changes_a_golden() {
    let root = project("golden-hard-rule");
    let goldens = root.join("crates/demo/goldens");
    let before = tree(&goldens);
    let tokens = ToolTokens::default();
    let (_, full_scope) = tokens.issue(ToolScope::new(
        "fixture",
        EVERY_PERMISSION.iter().copied(),
        vec!["*".to_string()],
    ));
    let spellings = [
        GOLDEN_REL.to_string(),
        "crates/demo/goldens".to_string(),
        "crates\\demo\\goldens\\sponza.png".to_string(),
        "crates/demo/../demo/GOLDENS/sponza.png".to_string(),
        root.join(GOLDEN_REL).display().to_string(),
    ];
    // Replaces every string under `value` with `path`, keeping free text.
    fn fill(value: &Value, key: Option<&str>, path: &str) -> Value {
        match value {
            Value::String(_) if key.is_some_and(|k| FREE_TEXT_KEYS.contains(&k)) => value.clone(),
            Value::String(_) => json!(path),
            Value::Array(items) => Value::Array(items.iter().map(|v| fill(v, key, path)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), fill(v, Some(k), path)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    let mut reached_guard = 0usize;
    let mut unguarded = Vec::new();
    for profile in [
        ALL_PERMISSIONS,
        &["read", "edit_scene", "assets", "play"],
        &["read"],
    ] {
        let mut agent = app(&root, profile);
        agent
            .world_mut()
            .insert_resource(crate::run_tests::RunTestsProgram(PathBuf::from(
                "/nonexistent/fake-cargo",
            )));
        for method in crate::schema::METHODS {
            if GUARD_EXEMPT.contains(&method.name) {
                continue;
            }
            for path in &spellings {
                let mut calls: Vec<Value> = method
                    .params
                    .iter()
                    .filter(|p| p.kind == "string" && !FREE_TEXT_KEYS.contains(&p.name))
                    .map(|p| {
                        let mut params = serde_json::Map::new();
                        params.insert(p.name.to_string(), json!(path));
                        Value::Object(params)
                    })
                    .collect();
                if let Some(example) = crate::contract::contract(method.name)
                    .and_then(|c| serde_json::from_str::<Value>(c.example).ok())
                {
                    let filled = fill(&example, None, path);
                    if filled != example {
                        calls.push(filled);
                    }
                }
                for params in calls {
                    let response = call(&mut agent, method.name, params.clone());
                    match reason(&response).as_str() {
                        "golden_path_denied" => reached_guard += 1,
                        // Refused earlier: the permission profile, or a contract limit on the value.
                        "F0012" | "F0002" => {}
                        other => unguarded.push(format!("{} {params}: {other:?}", method.name)),
                    }
                }
            }
        }
    }
    assert!(
        reached_guard > 1000,
        "the sweep reached the guard {reached_guard} times"
    );
    assert!(
        unguarded.is_empty(),
        "reached a handler:\n{}",
        unguarded.join("\n")
    );
    // The same through a tool scope.
    let mut tool = app(&root, ALL_PERMISSIONS);
    let mut scoped_guard = 0usize;
    for method in crate::schema::METHODS {
        if GUARD_EXEMPT.contains(&method.name) {
            continue;
        }
        for param in method
            .params
            .iter()
            .filter(|p| p.kind == "string" && !FREE_TEXT_KEYS.contains(&p.name))
        {
            let mut params = serde_json::Map::new();
            params.insert(param.name.to_string(), json!(GOLDEN_REL));
            let response = scoped_call(
                &mut tool,
                &tokens,
                &full_scope,
                method.name,
                Value::Object(params),
            );
            match reason(&response).as_str() {
                "golden_path_denied" => scoped_guard += 1,
                "F0002" | crate::scope::TOOL_SCOPE_DENIED => {}
                other => panic!("{} reached a handler under a scope: {other:?}", method.name),
            }
        }
    }
    assert!(scoped_guard > 100, "{scoped_guard}");
    assert_eq!(tree(&goldens), before, "the sweep changed a golden");

    // The legitimate sequence, then every way an agent could try to accept.
    let (mut agent, job, _) = tested(&root, ALL_PERMISSIONS, FAILING);
    let staged = ok(
        &mut agent,
        "agent.bless_golden",
        json!({ "name": "sponza", "reason": "intended", "job": job }),
    );
    let id = staged["id"].as_u64().expect("id");
    for selection in [
        json!({ "ids": [id] }),
        json!({ "set": staged["set"] }),
        json!({ "all": true }),
    ] {
        let answer = ok(&mut agent, "review.accept", selection);
        assert_eq!(answer["applied"], 0, "{answer}");
        assert_eq!(answer["results"][0]["reason"], "golden_owner_only", "{answer}");
    }
    // The in-process path the editor panel takes, as "owner".
    let panel = crate::review::decide_selected(agent.world_mut(), &json!({ "ids": [id] }), true, "owner")
        .expect("decide");
    assert_eq!(panel["results"][0]["reason"], "golden_owner_only", "{panel}");
    // A client calling itself the owner changes nothing.
    agent.world_mut().resource_mut::<AgentSession>().client = "owner".into();
    let named = ok(&mut agent, "review.accept", json!({ "ids": [id] }));
    assert_eq!(named["applied"], 0);
    // A tool scope cannot hold `review` at all.
    let scoped = scoped_call(
        &mut agent,
        &tokens,
        &full_scope,
        "review.accept",
        json!({ "ids": [id] }),
    );
    assert_eq!(reason(&scoped), crate::scope::TOOL_SCOPE_DENIED);
    // A text proposal aimed at the golden stops at the guard.
    let aimed = call(
        &mut agent,
        "review.propose",
        json!({ "changes": [{ "path": GOLDEN_REL, "text": "x" }], "reason": "r" }),
    );
    assert_eq!(reason(&aimed), "golden_path_denied");
    assert_eq!(tree(&goldens), before, "an accept changed a golden");
    assert_eq!(
        pending_proposals(&root).expect("queue").len(),
        1,
        "the proposal is still pending"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `REGIA_BLESS` in the app's own environment reaches neither `agent.golden_test` nor
/// `agent.run_tests`: the fake compare would overwrite the golden if it did. A child process holds
/// the variables, as the workspace forbids changing this process's environment.
#[cfg(unix)]
#[test]
fn regia_bless_in_the_app_environment_does_not_bypass() {
    let root = project("golden-bless-env");
    let before = tree(&root.join("crates/demo/goldens"));
    let status = Command::new(std::env::current_exe().expect("exe"))
        .args([
            "--exact",
            "golden_tools::tests::bless_env_child",
            "--ignored",
            "--nocapture",
        ])
        .env("GOLDEN_ENV_CHILD", &root)
        .env("REGIA_BLESS", "1")
        .env("FERRITE_BLESS", "1") // rename-keep
        .env("REGIA_SKIP_GOLDENS", "1")
        .status()
        .expect("child");
    assert!(status.success(), "{status}");
    assert_eq!(tree(&root.join("crates/demo/goldens")), before);
    let _ = std::fs::remove_dir_all(&root);
}

/// The child of [`regia_bless_in_the_app_environment_does_not_bypass`].
#[cfg(unix)]
#[test]
#[ignore = "run by its parent test, with its environment"]
fn bless_env_child() {
    let Some(root) = std::env::var_os("GOLDEN_ENV_CHILD").map(PathBuf::from) else {
        return;
    };
    assert_eq!(
        regia_core::brand::env_var("REGIA_BLESS").as_deref(),
        Ok("1"),
        "the app runs with the variable set"
    );
    let goldens = root.join("crates/demo/goldens");
    let before = tree(&goldens);
    let (mut agent, job, status) = tested(&root, ALL_PERMISSIONS, FAILING);
    // The test compared, it did not bless or skip: the fake only fails when it compares.
    assert_eq!(status["result"]["failed"], 1, "{status}");
    assert_eq!(tree(&goldens), before, "golden_test blessed");
    // A proposal from that job is still only a proposal.
    let staged = ok(
        &mut agent,
        "agent.bless_golden",
        json!({ "name": "sponza", "reason": "intended", "job": job }),
    );
    let accepted = ok(&mut agent, "review.accept", json!({ "ids": [staged["id"]] }));
    assert_eq!(accepted["applied"], 0);
    assert_eq!(tree(&goldens), before);
    // The plain test runner scrubs the same variables.
    let started = ok(
        &mut agent,
        "agent.run_tests",
        json!({ "filter": "sponza_matches" }),
    );
    let run = finished(&mut agent, started["job"].as_str().expect("job"));
    assert_eq!(run["result"]["failed"], 1, "{run}");
    assert_eq!(tree(&goldens), before, "agent.run_tests blessed");
}

/// What the owner's path does, with the proof of intent the CLI collects at its prompt.
#[cfg(unix)]
#[test]
fn the_owner_accepts_with_the_reason_in_the_commit_message() {
    let root = project("golden-owner");
    let goldens = root.join("crates/demo/goldens");
    let (mut agent, job, _) = tested(&root, ALL_PERMISSIONS, FAILING);
    let staged = ok(
        &mut agent,
        "agent.bless_golden",
        json!({ "name": "sponza", "reason": "the new sun angle is intended", "job": job }),
    );
    let id = staged["id"].as_u64().expect("id");
    let wrong = owner_accept(&root, id, &OwnerConfirmation::typed("yes"));
    assert_eq!(wrong.map_err(|e| e.code), Err("golden_confirmation_mismatch"));
    assert_eq!(
        proposal(&root, id).expect("pending").status,
        ChangeStatus::Pending
    );
    let accepted = owner_accept(&root, id, &OwnerConfirmation::typed(" sponza\n")).expect("accept");
    assert_eq!(accepted.reason, "the new sun angle is intended");
    assert_eq!(
        std::fs::read(goldens.join("sponza.png")).expect("golden"),
        std::fs::read(root.join("actual-frame.png")).expect("frame"),
        "the golden holds the proposal's exact bytes"
    );
    // The reason is where a commit message can use it.
    let message = std::fs::read_to_string(commit_message_path(&root)).expect("message file");
    assert!(
        message.contains("Reason: the new sun angle is intended"),
        "{message}"
    );
    assert!(message.contains(GOLDEN_REL), "{message}");
    assert!(message.contains("golden-test (test-model)"), "{message}");
    assert!(message.contains("ssim"), "{message}");
    assert_eq!(accepted.commit_message.trim(), message.trim());
    // The queue records who decided; a running editor's queue sees it without a restart.
    let listed = ok(&mut agent, "review.list", json!({ "status": "accepted" }));
    assert_eq!(listed["changes"][0]["decided_by"], "owner", "{listed}");
    assert_eq!(listed["changes"][0]["reason"], "the new sun angle is intended");
    let twice = owner_accept(&root, id, &OwnerConfirmation::typed("sponza"));
    assert_eq!(twice.map_err(|e| e.code), Err("review_already_decided"));
    let unknown = owner_accept(&root, 9999, &OwnerConfirmation::typed("sponza"));
    assert_eq!(unknown.map_err(|e| e.code), Err("review_change_not_found"));
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn the_owner_accept_checks_the_golden_and_the_staged_image() {
    let root = project("golden-owner-conflict");
    let (mut agent, job, _) = tested(&root, ALL_PERMISSIONS, FAILING);
    let staged = ok(
        &mut agent,
        "agent.bless_golden",
        json!({ "name": "sponza", "reason": "intended", "job": job }),
    );
    let id = staged["id"].as_u64().expect("id");
    let typed = OwnerConfirmation::typed("sponza");
    let golden = root.join(GOLDEN_REL);
    let original = std::fs::read(&golden).expect("golden");
    write_png(&golden, &image(16, 8, 90));
    assert_eq!(
        owner_accept(&root, id, &typed).map_err(|e| e.code),
        Err("review_conflict")
    );
    std::fs::write(&golden, &original).expect("restore");
    let after = root.join(staged["after"].as_str().expect("after"));
    let frame = std::fs::read(&after).expect("after");
    write_png(&after, &image(16, 8, 91));
    assert_eq!(
        owner_accept(&root, id, &typed).map_err(|e| e.code),
        Err("golden_staged_changed")
    );
    std::fs::write(&after, &frame).expect("restore");
    assert_eq!(
        std::fs::read(&golden).expect("golden"),
        original,
        "refusals wrote nothing"
    );
    assert!(owner_accept(&root, id, &typed).is_ok());
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn the_owner_rejects_and_only_proposals_count() {
    let root = project("golden-owner-reject");
    let before = tree(&root.join("crates/demo/goldens"));
    let (mut agent, job, _) = tested(&root, ALL_PERMISSIONS, FAILING);
    let staged = ok(
        &mut agent,
        "agent.bless_golden",
        json!({ "name": "sponza", "reason": "intended", "job": job }),
    );
    let id = staged["id"].as_u64().expect("id");
    let text = ok(
        &mut agent,
        "review.propose",
        json!({ "changes": [{ "path": "notes.txt", "text": "x\n" }], "reason": "notes" }),
    );
    let text_id = text["changes"][0]["id"].as_u64().expect("text id");
    assert_eq!(
        owner_accept(&root, text_id, &OwnerConfirmation::typed("x")).map_err(|e| e.code),
        Err("golden_not_a_proposal")
    );
    owner_reject(&root, id, Some("not this one")).expect("reject");
    assert_eq!(tree(&root.join("crates/demo/goldens")), before);
    assert!(!root.join(staged["after"].as_str().expect("after")).exists());
    let row = proposal(&root, id).map_err(|e| e.code);
    assert_eq!(row.err(), Some("review_already_decided"));
    let _ = std::fs::remove_dir_all(&root);
}

/// A queue written by hand (or by an older tool) naming a golden path as a text change is refused
/// where it would be written.
#[test]
fn a_handwritten_text_change_to_a_golden_is_refused_when_applied() {
    let root = project("golden-handwritten");
    let before = tree(&root.join("crates/demo/goldens"));
    let change = StagedChange {
        id: 1,
        set: 1,
        path: GOLDEN_REL.into(),
        before: None,
        after: "not an image".into(),
        reason: "r".into(),
        title: "t".into(),
        client: "c".into(),
        session: "s".into(),
        model: String::new(),
        images: BTreeMap::new(),
        created_ms: 0,
        status: ChangeStatus::Pending,
        decided_ms: None,
        decided_by: None,
        note: None,
        added: 0,
        removed: 0,
        texts_dropped: false,
        golden: None,
    };
    let file = crate::review::QueueFile {
        format_version: crate::review::REVIEW_QUEUE_FORMAT_VERSION,
        next_id: 1,
        next_set: 1,
        changes: vec![change],
    };
    let path = crate::review::queue_path(&root);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(&path, file.to_json().expect("json")).expect("queue");
    let mut owner = app(&root, ALL_PERMISSIONS);
    let answer = ok(&mut owner, "review.accept", json!({ "ids": [1] }));
    assert_eq!(answer["results"][0]["reason"], "review_path_denied", "{answer}");
    assert_eq!(tree(&root.join("crates/demo/goldens")), before);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn queue_files_of_either_version_load_and_a_newer_one_is_refused() {
    let v1 = r#"{ "format_version": 1, "next_id": 1, "next_set": 1, "changes": [{
        "id": 1, "set": 1, "path": "a.txt", "before": null, "after": "x", "reason": "r", "title": "t",
        "client": "c", "session": "s", "model": "", "created_ms": 0, "status": "pending" }] }"#;
    let file = crate::review::QueueFile::from_json(v1).expect("v1 loads");
    assert_eq!(file.changes[0].golden, None);
    let fixture = crate::review::fuzz_fixture();
    let parsed = crate::review::QueueFile::from_json(&fixture).expect("fixture");
    assert!(parsed.changes.iter().any(|c| c.golden.is_some()));
    // A change without a proposal writes no `golden` key, so its row stays as it was.
    let text_row = parsed
        .changes
        .iter()
        .find(|c| c.golden.is_none())
        .expect("text row");
    assert!(!serde_json::to_string(text_row).expect("json").contains("golden"));
    let newer = crate::review::QueueFile::from_json(
        r#"{ "format_version": 3, "next_id": 0, "next_set": 0, "changes": [] }"#,
    );
    assert!(newer.is_err());
}

/// `regia golden accept` writes the queue while an editor holds it: the editor's next look reads the
/// file again, so its next save does not undo the decision.
#[cfg(unix)]
#[test]
fn a_running_app_sees_a_decision_made_outside_it() {
    let root = project("golden-reload");
    let (mut agent, job, _) = tested(&root, ALL_PERMISSIONS, FAILING);
    let staged = ok(
        &mut agent,
        "agent.bless_golden",
        json!({ "name": "sponza", "reason": "intended", "job": job }),
    );
    let id = staged["id"].as_u64().expect("id");
    assert_eq!(ok(&mut agent, "review.list", json!({}))["pending"], 1);
    owner_accept(&root, id, &OwnerConfirmation::typed("sponza")).expect("accept");
    assert_eq!(ok(&mut agent, "review.list", json!({}))["pending"], 0);
    // A later change saved by the app keeps the owner's decision.
    ok(
        &mut agent,
        "review.propose",
        json!({ "changes": [{ "path": "notes.txt", "text": "x\n" }], "reason": "notes" }),
    );
    let on_disk = ReviewQueue::load(&root);
    let row = on_disk.file.changes.iter().find(|c| c.id == id).expect("row");
    assert_eq!(row.status, ChangeStatus::Accepted);
    assert_eq!(row.decided_by.as_deref(), Some("owner"));
    let _ = std::fs::remove_dir_all(&root);
}

/// Spec 23 keeps a rule here worth testing as text: only this module and the CLI name the accept.
#[test]
fn nothing_in_the_engine_calls_the_owner_accept() {
    let root = regia_test_utils::book::workspace_root();
    let mut callers = Vec::new();
    for crate_dir in ["crates/regia_agent/src", "crates/regia_editor/src"] {
        for dir in project_dirs(&root.join(crate_dir)) {
            for file in std::fs::read_dir(&dir).expect("dir").flatten() {
                let path = file.path();
                if path.extension().is_none_or(|e| e != "rs")
                    || path.ends_with("golden_tools.rs")
                    || path.ends_with("golden_tools/tests.rs")
                {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("source");
                if text.contains("owner_accept") || text.contains("OwnerConfirmation") {
                    callers.push(path.display().to_string());
                }
            }
        }
    }
    assert!(callers.is_empty(), "owner_accept is the CLI's alone: {callers:?}");
}

#[test]
fn close_matches_stay_close_and_bounded() {
    let entries: Vec<GoldenEntry> = [
        "alpha_modes",
        "alpha_modes_b",
        "beta",
        "lens_bokeh",
        "lens_wheel",
        "post_bloom",
        "x1",
        "x2",
        "x3",
    ]
    .iter()
    .map(|stem| GoldenEntry {
        stem: (*stem).to_string(),
        owner: "o".into(),
        rel: format!("goldens/{stem}.png"),
        path: PathBuf::from(format!("goldens/{stem}.png")),
    })
    .collect();
    assert_eq!(close_matches(&entries, "alpha_mode")[0], "alpha_modes");
    assert!(close_matches(&entries, "lens").contains(&"lens_bokeh".to_string()));
    assert!(close_matches(&entries, "o:beto").contains(&"beta".to_string()));
    assert!(close_matches(&entries, "x").len() <= MAX_CLOSE_MATCHES);
    assert!(close_matches(&entries, "completely_unrelated_name_here").is_empty());
    assert_eq!(edit_distance("kitten", "sitting"), 3);
}
