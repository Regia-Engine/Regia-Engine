// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask rename-brand` end to end on a small tree (ADR-1388): crate moves, text rules,
//! environment routing, binary files, the keep list, idempotency, a branch that is mid-way, the
//! dry run and the cleanroom refusal.
//!
//! This file is kept byte for byte by the tool (`xtask/rename-keep.txt`), so it uses no engine
//! crate: only `xtask::rename_brand` and a local temp-dir helper.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use xtask::rename_brand::{KeepList, convert};

fn temp_root(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rename-brand-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp root");
    dir
}

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, bytes).expect("write");
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Every file under `root` (relative path → bytes).
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .expect("rel")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, std::fs::read(&path).expect("read"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

const CORE_LIB: &str = "//! Core.\npub mod brand;\npub fn level() -> Option<String> {\n    std::env::var(\"FERRITE_LOG\").ok()\n}\n";
const BRAND: &str =
    "pub const BRAND_RENAMED: bool = false;\npub const LEGACY_ENV_PREFIX: &str = \"FERRITE_\";\n";
const RENDER_LIB: &str = "//! Ferrite's renderer.\nuse ferrite_core::level;\n\n/// Reads `FERRITE_GPU_INDEX`.\npub fn gpu() -> Option<std::ffi::OsString> {\n    std::env::var_os(\"FERRITE_GPU_INDEX\")\n}\n";
const LOCK: &str = "version = 4\n\n[[package]]\nname = \"ferrite_core\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"ferrite_render\"\nversion = \"0.1.0\"\ndependencies = [\n \"ferrite_core\",\n \"glam\",\n]\n\n[[package]]\nname = \"glam\"\nversion = \"0.33.0\"\n";

fn build_tree(root: &Path) {
    put(root, "CLEANROOM-DENY.txt", b"/nonexistent/proprietary/engine/\n");
    put(root, "Cargo.toml", b"[workspace.dependencies]\nferrite_core = { path = \"crates/ferrite_core\" }\nferrite_render = { path = \"crates/ferrite_render\" }\n");
    put(root, "Cargo.lock", LOCK.as_bytes());
    put(
        root,
        "crates/ferrite_core/Cargo.toml",
        b"[package]\nname = \"ferrite_core\"\n",
    );
    put(root, "crates/ferrite_core/src/lib.rs", CORE_LIB.as_bytes());
    put(root, "crates/ferrite_core/src/brand.rs", BRAND.as_bytes());
    put(
        root,
        "crates/ferrite_render/Cargo.toml",
        b"[package]\nname = \"ferrite_render\"\n\n[dependencies]\nferrite_core = { workspace = true }\n",
    );
    put(root, "crates/ferrite_render/src/lib.rs", RENDER_LIB.as_bytes());
    put(
        root,
        "crates/ferrite_render/goldens/a.png",
        b"\x89PNG\r\n\x1a\n\0ferrite bytes",
    );
    put(
        root,
        "samples/arena/Ferrite.toml",
        b"[project]\nname = \"Arena\"\nid = \"com.ferrite.samples.arena\"\n",
    );
    put(
        root,
        "docs/decisions.md",
        b"# Architecture decision records\n\n## ADR-001: Build on ferrite_ecs\nFerrite uses bevy.\n",
    );
    put(root, "docs/evidence/ferrite-shot.png", b"\x89PNG\0");
    put(root, "docs/guide.md", b"# Ferrite\n\nRun `ferrite new`. Saves live in `~/.local/share/Ferrite/<game>`.\nBuild out: /srv/build-bulk/ferrite/x\n");
}

#[test]
fn converts_a_tree_and_a_second_run_changes_nothing() {
    let root = temp_root("tree");
    build_tree(&root);
    let keep = KeepList::builtin().expect("keep list");

    let outcome = convert(&root, &keep, false).expect("convert");
    assert!(outcome.conflicts.is_empty(), "{:?}", outcome.conflicts);
    assert!(outcome.leftovers.is_empty(), "{:?}", outcome.leftovers);

    // Crate directories, the manifest and binaries moved; the binary bytes are untouched.
    assert!(!root.join("crates/ferrite_core").exists() && !root.join("crates/ferrite_render").exists());
    assert_eq!(
        std::fs::read(root.join("crates/regia_render/goldens/a.png")).expect("png"),
        b"\x89PNG\r\n\x1a\n\0ferrite bytes"
    );
    assert!(root.join("samples/arena/Regia.toml").is_file());
    assert_eq!(
        read(&root, "samples/arena/Regia.toml"),
        "[project]\nname = \"Arena\"\nid = \"com.regiaengine.samples.arena\"\n"
    );

    // Code: crate paths, env names, and env reads routed through the brand helpers.
    assert_eq!(
        read(&root, "crates/regia_render/src/lib.rs"),
        "//! Regia Engine's renderer.\nuse regia_core::level;\n\n/// Reads `REGIA_GPU_INDEX`.\npub fn gpu() -> Option<std::ffi::OsString> {\n    regia_core::brand::env_var_os(\"REGIA_GPU_INDEX\")\n}\n"
    );
    assert!(read(&root, "crates/regia_core/src/lib.rs").contains("crate::brand::env_var(\"REGIA_LOG\")"));
    // The brand module keeps its legacy names; only its flag flips.
    assert_eq!(
        read(&root, "crates/regia_core/src/brand.rs"),
        BRAND.replace("= false;", "= true;")
    );
    assert!(read(&root, "Cargo.toml").contains("regia_core = { path = \"crates/regia_core\" }"));

    // Cargo.lock: renamed and re-sorted (glam now sorts before the regia_* packages).
    let lock = read(&root, "Cargo.lock");
    let names: Vec<&str> = lock.lines().filter_map(|l| l.strip_prefix("name = ")).collect();
    assert_eq!(names, ["\"glam\"", "\"regia_core\"", "\"regia_render\""]);
    assert!(
        lock.contains("dependencies = [\n \"glam\",\n \"regia_core\",\n]"),
        "{lock}"
    );

    // History keeps its text (plus the one-line note); evidence keeps its name.
    let decisions = read(&root, "docs/decisions.md");
    assert!(decisions.contains("## ADR-001: Build on ferrite_ecs\nFerrite uses bevy."));
    assert!(
        decisions
            .starts_with("# Architecture decision records\n\nThe project was named Ferrite before ADR-1388")
    );
    assert!(root.join("docs/evidence/ferrite-shot.png").is_file());

    // Prose: product name, CLI, per-user directory; host paths in the keep list stay.
    assert_eq!(
        read(&root, "docs/guide.md"),
        "# Regia Engine\n\nRun `regia new`. Saves live in `~/.local/share/RegiaEngine/<game>`.\nBuild out: /srv/build-bulk/ferrite/x\n"
    );

    // Idempotent: a second run rewrites and moves nothing.
    let before = snapshot(&root);
    let again = convert(&root, &keep, false).expect("second run");
    assert!(again.is_noop(), "{}", again.render(true));
    assert_eq!(snapshot(&root), before);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dry_run_writes_nothing_and_reports_the_same_changes() {
    let root = temp_root("dry");
    build_tree(&root);
    let keep = KeepList::builtin().expect("keep list");
    let before = snapshot(&root);
    let dry = convert(&root, &keep, true).expect("dry run");
    assert_eq!(snapshot(&root), before, "a dry run writes nothing");
    let real = convert(&root, &keep, false).expect("convert");
    assert_eq!(dry.moved, real.moved);
    assert_eq!(dry.rewritten, real.rewritten);
    assert_eq!(dry.counts, real.counts);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_branch_that_is_mid_way_converts_to_the_same_result() {
    // `main` converted, then a lane adds files with old names (and one duplicate of a converted
    // file): one more run finishes the job, folding the duplicate, never overwriting a different file.
    let root = temp_root("midway");
    build_tree(&root);
    let keep = KeepList::builtin().expect("keep list");
    convert(&root, &keep, false).expect("first conversion");
    let converted = snapshot(&root);

    put(
        &root,
        "crates/ferrite_render/src/new_pass.rs",
        b"use ferrite_core::level;\nconst ENV: &str = \"FERRITE_NEW_PASS\";\n",
    );
    put(
        &root,
        "crates/ferrite_render/goldens/a.png",
        b"\x89PNG\r\n\x1a\n\0ferrite bytes",
    );
    let outcome = convert(&root, &keep, false).expect("second conversion");
    assert!(outcome.conflicts.is_empty(), "{:?}", outcome.conflicts);
    assert_eq!(
        read(&root, "crates/regia_render/src/new_pass.rs"),
        "use regia_core::level;\nconst ENV: &str = \"REGIA_NEW_PASS\";\n"
    );
    assert!(
        !root.join("crates/ferrite_render").exists(),
        "old directories are removed once empty"
    );
    let mut expected = converted;
    expected.insert(
        "crates/regia_render/src/new_pass.rs".to_owned(),
        b"use regia_core::level;\nconst ENV: &str = \"REGIA_NEW_PASS\";\n".to_vec(),
    );
    assert_eq!(snapshot(&root), expected);

    // A different file at the target is never overwritten.
    put(&root, "crates/ferrite_render/goldens/a.png", b"\x89PNG\0other");
    let clash = convert(&root, &keep, false).expect("third conversion");
    assert_eq!(clash.conflicts.len(), 1, "{:?}", clash.conflicts);
    assert_eq!(
        std::fs::read(root.join("crates/regia_render/goldens/a.png")).expect("png"),
        b"\x89PNG\r\n\x1a\n\0ferrite bytes"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_root_inside_a_cleanroom_deny_path_is_refused() {
    let root = temp_root("deny");
    build_tree(&root);
    let canonical = root.canonicalize().expect("canonical");
    put(
        &root,
        "CLEANROOM-DENY.txt",
        format!("{}/\n", canonical.display()).as_bytes(),
    );
    let before = snapshot(&root);
    let err = convert(&root, &KeepList::builtin().expect("keep list"), false).expect_err("refused");
    assert!(err.to_string().contains("cleanroom"), "{err}");
    assert_eq!(snapshot(&root), before);
    let _ = std::fs::remove_dir_all(&root);
}

/// ADR-8670: the rename check (`rename-brand --check`) passes on a tree whose only old-name hits
/// are kept, and fails on a new hit outside the keep list.
#[test]
fn the_check_fails_on_an_old_name_hit_outside_the_keep_list() {
    let root = temp_root("check");
    put(&root, "README.md", b"Regia Engine, read with regia_core.\n");
    put(
        &root,
        "notes.md",
        b"Lock /tmp/ferrite-gpu-0.lock (a kept host path).\nold = 1 // rename-keep: legacy key\n",
    );
    // An environment read the tool would only route through the brand helpers is no hit.
    put(
        &root,
        "crates/regia_a/Cargo.toml",
        b"[package]\nname = \"regia_a\"\n\n[dependencies]\nregia_core = { path = \"../regia_core\" }\n",
    );
    put(
        &root,
        "crates/regia_a/src/lib.rs",
        b"//! A.\npub fn home() -> Option<String> {\n    std::env::var(\"HOME\").ok()\n}\n",
    );
    let keep = KeepList::builtin().expect("keep list");
    let clean = convert(&root, &keep, true).expect("dry run");
    assert_eq!(
        clean.rewritten.keys().collect::<Vec<_>>(),
        ["crates/regia_a/src/lib.rs"]
    );
    assert!(clean.is_clean(), "{}", clean.render(true));
    put(&root, "new.md", b"Calls ferrite_render::draw.\n");
    let dirty = convert(&root, &keep, true).expect("dry run");
    assert!(!dirty.is_clean());
    assert_eq!(dirty.name_hits.keys().collect::<Vec<_>>(), ["new.md"]);
    let _ = std::fs::remove_dir_all(&root);
}

/// ADR-8670: this repository passes the rename check: every remaining old-name hit is history,
/// legacy data a reader still accepts, or a host name, and is listed in `xtask/rename-keep.txt`
/// (or marked in its file). A failure lists each file and hit: write the new name there (reading
/// both where old data carries the old one), or keep the hit with its reason.
#[test]
fn the_workspace_passes_the_rename_check() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let keep = KeepList::builtin().expect("keep list");
    let mut outcome = convert(root, &keep, true).expect("dry run");
    // The merge gate's untracked scratch directory in a staged checkout is not the repository's.
    outcome.name_hits.retain(|path, _| !path.starts_with(".fgate/"));
    outcome.moved.retain(|path, _| !path.starts_with(".fgate/"));
    outcome.leftovers.retain(|hit| !hit.starts_with(".fgate/"));
    assert!(
        outcome.is_clean(),
        "old-name hits outside the keep list (details: `cargo xtask rename-brand --check`)\nfiles: {:?}\nmoved: {:?}\nleftovers: {:?}",
        outcome.name_hits,
        outcome.moved,
        outcome.leftovers
    );
}
