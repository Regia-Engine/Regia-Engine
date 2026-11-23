// SPDX-License-Identifier: MPL-2.0
//! Spec 94 (ADR-1852) end to end through the `regia` binary: packaged releases, `regia new
//! --engine-source`, and `regia upgrade-project --engine` moving every template from a previous
//! release to this engine's release.
//!
//! The releases are small engine-shaped trees (a workspace `Cargo.toml` with a version and one
//! `crates/<name>/Cargo.toml` per engine crate), packaged by `regia engine package`: the test
//! checks what the commands write. `every_template_compiles_against_this_engine` is the cargo build
//! of a result (slow, `--ignored`, nightly); § 9 of the spec gives the two-tag build check.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use regia_test_utils::unique_temp_dir;
use serde_json::Value;

const TEMPLATES: &[&str] = regia_templates::TEMPLATE_IDS;

/// Templates the nightly compile job generates and checks. The net-game template splits into three
/// crates (ADR-2039) and its server is not a default member: it is checked on its own, because
/// `regia` built with both `client` and `server` hosts a server.
const COMPILED_TEMPLATES: &[&str] = regia_templates::TEMPLATE_IDS;

fn regia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(args)
        .output()
        .expect("spawn the regia binary")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not JSON ({err}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn s(path: &Path) -> &str {
    path.to_str().expect("temp paths are UTF-8")
}

/// `path` with `/` separators: the tool writes and reports paths that way on every host.
fn slashed(path: &Path) -> String {
    s(path).replace('\\', "/")
}

/// The engine crate names of this checkout (`crates/*`).
fn engine_crates() -> Vec<String> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut names: Vec<String> = std::fs::read_dir(&crates)
        .expect("crates dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().join("Cargo.toml").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Packages an engine-shaped tree at `version` with `regia engine package` and returns the release.
fn release(version: &str, out: &Path) -> PathBuf {
    let tree = unique_temp_dir("dist_e2e_tree");
    std::fs::write(
        tree.join("Cargo.toml"),
        format!("[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nversion = \"{version}\"\nrust-version = \"1.98\"\n"),
    )
    .expect("workspace");
    // The release's tested third-party versions: serde's patch tracks the whole engine version (not
    // its last digit: 0.1.9 and 0.2.9 must differ too), so two releases disagree on one package.
    let patch = version
        .split('.')
        .fold(0u64, |acc, part| acc * 1000 + part.parse::<u64>().unwrap_or(0));
    std::fs::write(
        tree.join("Cargo.lock"),
        format!(
            "version = 4\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.{patch}\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n"
        ),
    )
    .expect("lock");
    for name in engine_crates() {
        let dir = tree.join("crates").join(&name);
        std::fs::create_dir_all(dir.join("src")).expect("crate dir");
        std::fs::write(dir.join("Cargo.toml"), format!("[package]\nname = \"{name}\"\n")).expect("crate");
        std::fs::write(dir.join("src/lib.rs"), "").expect("lib");
    }
    let packaged = regia(&[
        "engine",
        "package",
        "--engine",
        s(&tree),
        "--out",
        s(out),
        "--json",
    ]);
    assert!(packaged.status.success(), "{}", stderr(&packaged));
    let doc = json(&packaged);
    assert_eq!(doc["kind"], "regia_engine_package");
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["version"], version);
    std::fs::remove_dir_all(&tree).ok();
    PathBuf::from(doc["dir"].as_str().expect("dir"))
}

/// Every `Cargo.toml` and `Regia.toml` of a project, by relative path.
fn manifests(project: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for entry in walk(project) {
        let name = entry
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name == "Cargo.toml" || name == "Regia.toml" {
            let rel = entry
                .strip_prefix(project)
                .expect("inside")
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel, std::fs::read_to_string(&entry).expect("read"));
        }
    }
    out
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).expect("read dir").filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

fn line() -> String {
    regia_core::engine_version::CURRENT.line().to_string()
}

/// § 8: every template, pinned to a previous release on line 0.1, moves to this engine's release
/// in one `--write`; a dry run first changes nothing; a second run is a no-op.
#[test]
fn every_template_upgrades_from_a_previous_release() {
    let releases = unique_temp_dir("dist_e2e_releases");
    let old = release("0.1.9", &releases);
    let new = release(env!("CARGO_PKG_VERSION"), &releases);
    let parent = unique_temp_dir("dist_e2e_projects");
    for template in TEMPLATES {
        let created = regia(&[
            "new",
            &format!("Up {template}"),
            "--template",
            template,
            "--path",
            s(&parent),
            "--engine-source",
            &format!("path:{}", s(&old)),
        ]);
        assert!(created.status.success(), "{template}: {}", stderr(&created));
        let project = PathBuf::from(String::from_utf8_lossy(&created.stdout).trim());
        assert_eq!(
            std::fs::read(project.join("Cargo.lock")).expect("seeded lock"),
            std::fs::read(old.join("Cargo.lock")).expect("release lock"),
            "{template}: `new` seeds the release's Cargo.lock"
        );
        let has_manifest = project.join("Regia.toml").is_file();
        if has_manifest {
            // Projects made before spec 94 say "0.1".
            let path = project.join("Regia.toml");
            let text = std::fs::read_to_string(&path).expect("manifest");
            assert!(
                text.contains(&format!("engine = \"{}\"", line())),
                "{template}: {text}"
            );
            std::fs::write(
                &path,
                text.replace(&format!("engine = \"{}\"", line()), "engine = \"0.1\""),
            )
            .expect("age the project");
        }

        // The pin names the old release, and info calls it stale.
        let info = regia(&["engine", "info", "--project", s(&project), "--json"]);
        assert!(info.status.success(), "{template}: {}", stderr(&info));
        let doc = json(&info);
        assert_eq!(doc["kind"], "regia_engine_info");
        let old_source = format!("path:{}", slashed(&old));
        assert_eq!(
            doc["project"]["pin"]["sources"],
            serde_json::json!([old_source]),
            "{template}"
        );
        let codes: Vec<&str> = doc["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .filter_map(|p| p["code"].as_str())
            .collect();
        assert!(codes.contains(&"engine_pin_stale"), "{template}: {codes:?}");

        // Unknown binary assets and user files must survive both the content migration and pin rewrite.
        let user_bytes = b"\0user data\xff\x80\r\n";
        std::fs::write(project.join("notes.bin"), user_bytes).expect("user file");
        let cue = b"( // version: 1 is only a comment\n version : 2, name: \"intro\", sounds: [], )";
        std::fs::write(project.join("intro.cue.ron"), cue).expect("old cue");
        let before = manifests(&project);
        let target = format!("path:{}", slashed(&new));
        let dry = regia(&[
            "upgrade-project",
            "--project",
            s(&project),
            "--engine",
            &target,
            "--json",
        ]);
        assert!(dry.status.success(), "{template}: {}", stderr(&dry));
        if has_manifest {
            let notes = format!("release notes to read: docs/releases/v{}.*.md", line());
            assert!(stderr(&dry).contains(&notes), "{template}: {}", stderr(&dry));
        }
        let plan = json(&dry);
        assert_eq!(plan["kind"], "regia_upgrade_project");
        assert_eq!(plan["ok"], true, "{template}: {plan}");
        assert_eq!(plan["write"], false);
        assert_eq!(plan["pin"]["lock_drift"], 1, "{template}: serde differs");
        let codes: Vec<&str> = plan["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .filter_map(|p| p["code"].as_str())
            .collect();
        assert_eq!(codes, vec!["engine_lock_differs"], "{template}");
        assert!(
            !plan["pin"]["edits"].as_array().expect("edits").is_empty(),
            "{template}"
        );
        if has_manifest {
            assert_eq!(plan["project"]["status"], "upgrade_needed", "{template}");
            assert_eq!(plan["stamp"], line().as_str());
        }
        assert_eq!(
            manifests(&project),
            before,
            "{template}: the dry run wrote something"
        );

        assert_eq!(
            std::fs::read(project.join("intro.cue.ron")).expect("dry cue"),
            cue
        );
        assert!(!project.join("intro.cue.ron.bak").exists());
        let write = regia(&[
            "upgrade-project",
            "--project",
            s(&project),
            "--engine",
            &target,
            "--write",
        ]);
        assert!(write.status.success(), "{template}: {}", stderr(&write));
        assert_eq!(
            std::fs::read(project.join("notes.bin")).expect("user bytes"),
            user_bytes
        );
        assert_eq!(
            std::fs::read(project.join("intro.cue.ron.bak")).expect("original cue"),
            cue
        );
        let migrated = std::fs::read_to_string(project.join("intro.cue.ron")).expect("new cue");
        assert_eq!(
            regia_core::version::peek_ron_version(&migrated),
            Ok(regia::audio::CUE_VERSION)
        );
        regia::audio::SoundCueDef::from_ron(&migrated).expect("migrated cue loads");
        let after = manifests(&project);
        for (file, text) in &after {
            assert!(
                !text.contains(s(&old)),
                "{template}: {file} still names the old release:\n{text}"
            );
            if let Some(original) = before.get(file)
                && original != text
            {
                assert_eq!(
                    std::fs::read_to_string(project.join(format!("{file}.bak"))).expect("backup"),
                    *original,
                    "{template}: {file}.bak is the original"
                );
            }
        }
        if has_manifest {
            assert!(
                after["Regia.toml"].contains(&format!("engine = \"{}\"", line())),
                "{template}"
            );
        }
        let info = json(&regia(&["engine", "info", "--project", s(&project), "--json"]));
        assert_eq!(info["ok"], true, "{template}: {info}");
        assert_eq!(
            info["project"]["pin"]["sources"],
            serde_json::json!([target]),
            "{template}"
        );
        assert!(
            info["problems"].as_array().expect("problems").is_empty(),
            "{template}: {info}"
        );
        if has_manifest {
            assert_eq!(info["project"]["engine"]["status"], "current", "{template}");
        }

        let again = regia(&[
            "upgrade-project",
            "--project",
            s(&project),
            "--engine",
            &target,
            "--write",
            "--json",
        ]);
        assert!(again.status.success(), "{template}: {}", stderr(&again));
        let doc = json(&again);
        assert!(
            doc["pin"]["edits"].as_array().expect("edits").is_empty(),
            "{template}: {doc}"
        );
        assert!(doc["stamp"].is_null(), "{template}");
        assert_eq!(
            manifests(&project),
            after,
            "{template}: the second run changed something"
        );
    }
    std::fs::remove_dir_all(&parent).ok();
    std::fs::remove_dir_all(&releases).ok();
}

/// A vendored release inside the project is written relative to each manifest and is never
/// scanned as project content.
#[test]
fn a_vendored_release_is_pinned_relatively_and_not_scanned() {
    let releases = unique_temp_dir("dist_e2e_vendor_rel");
    let new = release(env!("CARGO_PKG_VERSION"), &releases);
    let parent = unique_temp_dir("dist_e2e_vendor");
    let created = regia(&["new", "Vend", "--template", "blank", "--path", s(&parent)]);
    assert!(created.status.success(), "{}", stderr(&created));
    let project = parent.join("vend");
    let vendored = project.join("vendor").join(new.file_name().expect("name"));
    for file in walk(&new) {
        let to = vendored.join(file.strip_prefix(&new).expect("inside"));
        std::fs::create_dir_all(to.parent().expect("parent")).expect("dir");
        std::fs::copy(&file, &to).expect("copy");
    }
    // An old-version cue inside the vendored engine must not be "migrated".
    std::fs::write(vendored.join("fixture.cue.ron"), "( name: \"x\", sounds: [] )").expect("fixture");
    let rel = format!("path:vendor/{}", new.file_name().expect("name").to_string_lossy());
    let out = regia(&[
        "upgrade-project",
        "--project",
        s(&project),
        "--engine",
        &rel,
        "--write",
        "--json",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let doc = json(&out);
    assert!(
        !doc["files"]
            .as_array()
            .expect("files")
            .iter()
            .any(|f| f["path"].as_str().is_some_and(|p| p.starts_with("vendor/"))),
        "{doc}"
    );
    let game = std::fs::read_to_string(project.join("game/Cargo.toml")).expect("game");
    let expected = format!(
        "path = \"../vendor/{}/crates/regia\"",
        new.file_name().expect("name").to_string_lossy()
    );
    assert!(game.contains(&expected), "{game}");
    assert!(!vendored.join("fixture.cue.ron.bak").exists());
    let verify = regia(&["engine", "verify", s(&vendored), "--json"]);
    let report = json(&verify);
    // The fixture is an extra file: a warning, not a failure.
    assert!(verify.status.success(), "{}", stderr(&verify));
    assert_eq!(report["problems"][0]["code"], "release_file_extra");
    std::fs::remove_dir_all(&parent).ok();
    std::fs::remove_dir_all(&releases).ok();
}

/// Every template compiles against this engine: `regia new`, then `cargo check` of the project
/// (`docs/06` `templates`, spec 94 § 8). A cold check takes minutes, so the nightly job runs it:
/// `cargo test -p regia_cli --test distribution_upgrade -- --ignored`. The first real run of this
/// found that the First Person template did not compile on v0.2.5.
#[test]
#[ignore = "runs cargo check on a generated project of each template (minutes)"]
fn every_template_compiles_against_this_engine() {
    // Use a real packaged copy, rather than accidentally proving only checkout dependencies.
    let releases = unique_temp_dir("dist_compile_release");
    let packaged = regia(&["engine", "package", "--out", s(&releases), "--json"]);
    assert!(packaged.status.success(), "{}", stderr(&packaged));
    let engine = PathBuf::from(json(&packaged)["dir"].as_str().expect("release dir"));
    let verified = regia(&["engine", "verify", s(&engine), "--json"]);
    assert!(verified.status.success(), "{}", stderr(&verified));
    let source = format!("path:{}", s(&engine));
    let parent = unique_temp_dir("dist_e2e_compile");
    for template in COMPILED_TEMPLATES {
        let created = regia(&[
            "new",
            &format!("C {template}"),
            "--template",
            template,
            "--path",
            s(&parent),
            "--engine-source",
            &source,
        ]);
        assert!(created.status.success(), "{template}: {}", stderr(&created));
        let project = PathBuf::from(String::from_utf8_lossy(&created.stdout).trim());
        let check = Command::new(env!("CARGO"))
            .args(["check", "--manifest-path"])
            .arg(project.join("Cargo.toml"))
            .output()
            .expect("spawn cargo");
        assert!(check.status.success(), "{template}: {}", stderr(&check));
        let server = project.join("server").join("Cargo.toml");
        if server.is_file() {
            let check = Command::new(env!("CARGO"))
                .args(["check", "--manifest-path"])
                .arg(&server)
                .output()
                .expect("spawn cargo");
            assert!(check.status.success(), "{template} server: {}", stderr(&check));
        }
    }
    std::fs::remove_dir_all(&parent).ok();
    std::fs::remove_dir_all(&releases).ok();
}

/// A game may be called `regia_something`: its launcher's dependency on it stays a local path.
#[test]
fn a_game_named_like_an_engine_crate_upgrades() {
    let releases = unique_temp_dir("dist_e2e_named_rel");
    let parent = unique_temp_dir("dist_e2e_named_proj");
    let old = release("0.1.9", &releases);
    let new = release(env!("CARGO_PKG_VERSION"), &releases);
    let created = regia(&[
        "new",
        "Regia Arena",
        "--template",
        "first_person",
        "--path",
        s(&parent),
        "--engine-source",
        &format!("path:{}", s(&old)),
    ]);
    assert!(created.status.success(), "{}", stderr(&created));
    let project = PathBuf::from(String::from_utf8_lossy(&created.stdout).trim());
    let launcher = project.join("launcher/Cargo.toml");
    assert!(
        std::fs::read_to_string(&launcher)
            .expect("launcher")
            .contains("regia_arena = { path = \"../game\" }")
    );

    let target = format!("path:{}", s(&new));
    let out = regia(&[
        "upgrade-project",
        "--project",
        s(&project),
        "--write",
        "--engine",
        &target,
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = std::fs::read_to_string(&launcher).expect("launcher");
    assert!(text.contains("regia_arena = { path = \"../game\" }"), "{text}");
    assert!(
        text.contains(&format!("path = \"{}/crates/regia\"", slashed(&new))),
        "{text}"
    );
    std::fs::remove_dir_all(&parent).ok();
    std::fs::remove_dir_all(&releases).ok();
}

fn project_with_engine(label: &str, engine: &str) -> PathBuf {
    let dir = unique_temp_dir(label);
    std::fs::write(
        dir.join("Regia.toml"),
        format!("[project]\nname = \"T\"\nid = \"com.example.t\"\nengine = \"{engine}\"\n"),
    )
    .expect("manifest");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"t\"\n\n[dependencies]\nregia = { path = \"/nowhere/crates/regia\" }\n",
    )
    .expect("cargo");
    std::fs::write(dir.join("impact.cue.ron"), "( name: \"impact\", sounds: [] )").expect("cue");
    dir
}

/// A project on a newer engine, outside the support window, or a target of another version is
/// refused before any file is written (the old cue is not migrated either).
#[test]
fn refused_upgrades_write_nothing() {
    let releases = unique_temp_dir("dist_e2e_refuse_rel");
    let other = release("0.1.9", &releases);
    let current = regia_core::engine_version::CURRENT;
    let newer_patch = format!("{}.{}.{}", current.major, current.minor, current.patch + 1);
    for (label, engine, extra, code) in [
        ("dist_e2e_newer", "9.0", None, "engine_too_old"),
        ("dist_e2e_window", "0.0.1", None, "upgrade_unsupported"),
        ("dist_e2e_bad", "latest", None, "engine_requirement_invalid"),
        (
            "dist_e2e_mismatch",
            "0.1",
            Some(format!("path:{}", s(&other))),
            "engine_source_version_mismatch",
        ),
        (
            "dist_e2e_missing",
            "0.1",
            Some("path:/nowhere/at/all".to_string()),
            "engine_source_missing",
        ),
        (
            "dist_e2e_source",
            "0.1",
            Some("ftp:x".to_string()),
            "engine_source_invalid",
        ),
        // Spec 94 § 3.3: a registry version or a git tag naming another engine is the same refusal
        // as a path of another version, so neither can downgrade a project.
        (
            "dist_e2e_registry",
            "0.1",
            Some("registry:studio@0.1.9".to_string()),
            "engine_source_version_mismatch",
        ),
        (
            "dist_e2e_tag",
            "0.1",
            Some("git:https://example.com/regia.git#v0.1.9".to_string()),
            "engine_source_version_mismatch",
        ),
        // Same line, a newer patch than this engine: never a downgrade either.
        ("dist_e2e_patch", newer_patch.as_str(), None, "engine_too_old"),
    ] {
        let project = project_with_engine(label, engine);
        let before = manifests(&project);
        let mut args = vec!["upgrade-project", "--project", s(&project), "--write", "--json"];
        if let Some(extra) = &extra {
            args.extend(["--engine", extra.as_str()]);
        }
        let out = regia(&args);
        assert!(!out.status.success(), "{label} must fail");
        let doc = json(&out);
        assert_eq!(doc["ok"], false, "{label}");
        let codes: Vec<&str> = doc["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .filter_map(|p| p["code"].as_str())
            .collect();
        assert!(codes.contains(&code), "{label}: {codes:?}");
        assert!(
            stderr(&out).contains("nothing written"),
            "{label}: {}",
            stderr(&out)
        );
        assert_eq!(manifests(&project), before, "{label}");
        assert!(
            !project.join("impact.cue.ron.bak").exists(),
            "{label}: migrated despite the refusal"
        );
        std::fs::remove_dir_all(&project).ok();
    }
    std::fs::remove_dir_all(&releases).ok();
}

/// `engine verify` fails on a changed file and names it; git and registry sources are written by
/// `regia new`.
#[test]
fn verify_fails_on_a_changed_file_and_new_writes_git_pins() {
    let releases = unique_temp_dir("dist_e2e_verify");
    let dir = release(env!("CARGO_PKG_VERSION"), &releases);
    let ok = regia(&["engine", "verify", s(&dir), "--json"]);
    assert!(ok.status.success(), "{}", stderr(&ok));
    assert_eq!(json(&ok)["ok"], true);
    std::fs::write(dir.join("crates/regia/src/lib.rs"), "// patched\n").expect("tamper");
    let bad = regia(&["engine", "verify", s(&dir), "--json"]);
    assert!(!bad.status.success());
    let doc = json(&bad);
    assert_eq!(doc["problems"][0]["code"], "release_file_changed");
    assert!(
        doc["problems"][0]["message"]
            .as_str()
            .expect("message")
            .contains("crates/regia/src/lib.rs")
    );

    let parent = unique_temp_dir("dist_e2e_git");
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    let created = regia(&[
        "new",
        "Gitty",
        "--template",
        "first_person",
        "--path",
        s(&parent),
        "--engine-source",
        &format!("git:https://example.com/regia.git#{tag}"),
    ]);
    assert!(created.status.success(), "{}", stderr(&created));
    let game = std::fs::read_to_string(parent.join("gitty/game/Cargo.toml")).expect("game");
    assert!(
        game.contains(&format!(
            "regia = {{ git = \"https://example.com/regia.git\", tag = \"{tag}\" }}"
        )),
        "{game}"
    );
    assert!(game.contains("optional = true"), "{game}");
    let bad_source = regia(&["new", "Nope", "--path", s(&parent), "--engine-source", "nope"]);
    assert!(!bad_source.status.success());
    assert!(
        stderr(&bad_source).contains("engine_source_invalid"),
        "{}",
        stderr(&bad_source)
    );
    assert!(!parent.join("nope").exists(), "a bad source creates nothing");
    std::fs::remove_dir_all(&parent).ok();
    std::fs::remove_dir_all(&releases).ok();
}

#[test]
fn engine_info_reports_unsupported_pin_layout_as_a_warning() {
    let project = unique_temp_dir("dist_info_layout");
    std::fs::write(
        project.join("Regia.toml"),
        "[project]\nname = \"G\"\nengine = \"0.2\"\n",
    )
    .expect("manifest");
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"g\"\n[dependencies.regia]\npath = \"/engine/crates/regia\"\n",
    )
    .expect("cargo");
    let out = regia(&["engine", "info", "--project", s(&project), "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let doc = json(&out);
    let problem = doc["problems"]
        .as_array()
        .expect("problems")
        .iter()
        .find(|p| p["code"] == "engine_dep_layout")
        .expect("warning");
    assert_eq!(problem["severity"], "warning");
    std::fs::remove_dir_all(project).expect("cleanup");
}

fn problem_codes(doc: &Value) -> Vec<String> {
    doc["problems"]
        .as_array()
        .expect("problems")
        .iter()
        .filter_map(|p| p["code"].as_str().map(str::to_string))
        .collect()
}

/// Spec 94 § 2 `unpinned` row: a `Regia.toml` without `engine` is treated as the oldest line and
/// `--write` stamps this engine's line (with a backup); `--engine` on a project with no engine
/// dependency warns `engine_pin_empty` and still succeeds.
#[test]
fn an_unpinned_project_is_stamped_and_an_empty_pin_warns() {
    let project = unique_temp_dir("dist_e2e_unpinned");
    let manifest = "[project]\nname = \"U\"\nid = \"com.example.u\"\n";
    std::fs::write(project.join("Regia.toml"), manifest).expect("manifest");
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"u\"\n\n[dependencies]\nregia = { path = \"/nowhere/crates/regia\" }\n",
    )
    .expect("cargo");

    let dry = regia(&["upgrade-project", "--project", s(&project), "--json"]);
    assert!(dry.status.success(), "{}", stderr(&dry));
    let plan = json(&dry);
    assert_eq!(plan["project"]["status"], "unpinned", "{plan}");
    assert!(plan["project"]["requirement"].is_null(), "{plan}");
    let crossed = plan["project"]["lines_crossed"].as_array().expect("lines");
    assert_eq!(crossed.first().and_then(Value::as_str), Some("0.1"), "{plan}");
    assert_eq!(
        crossed.last().and_then(Value::as_str),
        Some(line().as_str()),
        "{plan}"
    );
    assert_eq!(plan["stamp"], line().as_str());
    assert_eq!(
        std::fs::read_to_string(project.join("Regia.toml")).expect("manifest"),
        manifest,
        "the dry run wrote nothing"
    );

    let write = regia(&["upgrade-project", "--project", s(&project), "--write"]);
    assert!(write.status.success(), "{}", stderr(&write));
    let stamped = std::fs::read_to_string(project.join("Regia.toml")).expect("manifest");
    assert!(stamped.contains(&format!("engine = \"{}\"", line())), "{stamped}");
    assert!(stamped.starts_with(manifest), "only a line is added: {stamped}");
    assert_eq!(
        std::fs::read_to_string(project.join("Regia.toml.bak")).expect("backup"),
        manifest
    );
    let info = json(&regia(&["engine", "info", "--project", s(&project), "--json"]));
    assert_eq!(info["project"]["engine"]["status"], "current", "{info}");

    // A project with no engine dependency: `--engine` has nothing to move.
    let releases = unique_temp_dir("dist_e2e_empty_rel");
    let new = release(env!("CARGO_PKG_VERSION"), &releases);
    std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"u\"\n").expect("cargo");
    let out = regia(&[
        "upgrade-project",
        "--project",
        s(&project),
        "--engine",
        &format!("path:{}", s(&new)),
        "--json",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let doc = json(&out);
    assert_eq!(doc["ok"], true, "{doc}");
    assert!(
        problem_codes(&doc).iter().any(|c| c == "engine_pin_empty"),
        "{doc}"
    );
    std::fs::remove_dir_all(&project).ok();
    std::fs::remove_dir_all(&releases).ok();
}

/// Spec 94 § 3.3: engine crates from two sources warn `engine_sources_mixed`, and a registry pin
/// naming another engine version warns `engine_pin_stale`; both are warnings (`ok`, exit 0).
#[test]
fn engine_info_warns_about_mixed_and_stale_pins() {
    let project = unique_temp_dir("dist_e2e_mixed");
    std::fs::write(
        project.join("Regia.toml"),
        format!("[project]\nname = \"M\"\nengine = \"{}\"\n", line()),
    )
    .expect("manifest");
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"m\"\n\n[dependencies]\n\
         regia = { path = \"/nowhere/crates/regia\" }\n\
         regia_core = { version = \"=0.1.9\", registry = \"studio\" }\n",
    )
    .expect("cargo");
    let out = regia(&["engine", "info", "--project", s(&project), "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let doc = json(&out);
    assert_eq!(doc["ok"], true, "{doc}");
    let codes = problem_codes(&doc);
    assert!(codes.iter().any(|c| c == "engine_sources_mixed"), "{codes:?}");
    assert!(codes.iter().any(|c| c == "engine_pin_stale"), "{codes:?}");
    assert!(
        doc["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .all(|p| p["severity"] == "warning"),
        "{doc}"
    );
    let sources = doc["project"]["pin"]["sources"].as_array().expect("sources");
    assert_eq!(sources.len(), 2, "{doc}");
    std::fs::remove_dir_all(&project).ok();
}

/// ADR-7871: packaging refuses a tracked link before any release tree is produced.
#[cfg(unix)]
#[test]
fn package_tracked_external_symlink_is_a_named_error() {
    let tree = unique_temp_dir("dist-e2e-source-link");
    let outside = unique_temp_dir("dist-e2e-link-target");
    let out = unique_temp_dir("dist-e2e-link-output");
    std::fs::write(
        tree.join("Cargo.toml"),
        "[workspace.package]\nversion = \"0.2.5\"\nrust-version = \"1.99\"\n",
    )
    .expect("workspace");
    std::fs::write(outside.join("private.txt"), "must not be packaged").expect("outside file");
    std::os::unix::fs::symlink(outside.join("private.txt"), tree.join("private.txt")).expect("link");
    for args in [
        vec!["init", "-q"],
        vec!["add", "Cargo.toml", "private.txt"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&tree)
                .args(args)
                .status()
                .expect("git")
                .success()
        );
    }
    let result = regia(&[
        "engine",
        "package",
        "--engine",
        s(&tree),
        "--out",
        s(&out),
        "--json",
    ]);
    assert!(!result.status.success(), "a source link must be refused");
    assert_eq!(json(&result)["problems"][0]["code"], "release_file_link");
    assert!(!out.join("regia-engine-0.2.5").exists());
    for dir in [tree, outside, out] {
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
