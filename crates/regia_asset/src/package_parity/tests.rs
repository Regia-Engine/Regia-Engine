// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests for the parity diff and the open-trace classifier.
//!
//! The assertions follow the requirements cases. Paths such as `assets_regia`
//! stay in these fixtures so the cook-memo vectors match; they are not API names.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use regia_test_utils::TempDir;

use super::{
    FileClass, build_open_report, classify_text, diff_paths, diff_paths_mode, gltf_memo_key, open_trace_main,
};

const OLD: u64 = 1_000_000_000;
const NEW: u64 = 1_700_000_000;
const STAMP: &str = "1500000000";

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|err| panic!("mkdir {parent:?}: {err}"));
    }
    fs::write(path, bytes).unwrap_or_else(|err| panic!("write {path:?}: {err}"));
}

fn set_mtime(path: &Path, secs: u64) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap_or_else(|err| panic!("open {path:?}: {err}"));
    let when = UNIX_EPOCH + Duration::from_secs(secs);
    file.set_modified(when)
        .unwrap_or_else(|err| panic!("mtime {path:?}: {err}"));
}

fn place(root: &Path, rel: &str, data: &[u8], mtime: Option<u64>) -> PathBuf {
    let path = root.join(rel);
    write(&path, data);
    if let Some(secs) = mtime {
        set_mtime(&path, secs);
    }
    path
}

fn write_trace(path: &Path, root: &Path, rows: &[(&str, &str)]) {
    let mut text = String::new();
    for (op, rel) in rows {
        text.push_str(&format!("ok {op} {}/{rel}\n", root.display()));
    }
    write(path, text.as_bytes());
}

fn lines_of(tmp: &Path, dev_rows: &[(&str, &str)], pkg_rows: &[(&str, &str)]) -> Vec<String> {
    let dev = tmp.join("dev");
    let pkg = tmp.join("pkg");
    let dev_trace = tmp.join("dev.trace");
    let pkg_trace = tmp.join("pkg.trace");
    write_trace(&dev_trace, &dev, dev_rows);
    write_trace(&pkg_trace, &pkg, pkg_rows);
    let report = diff_paths(&dev_trace, &dev, &pkg_trace, &pkg).unwrap_or_else(|err| panic!("diff: {err}"));
    report.lines
}

fn quote_path(path: &Path) -> String {
    let text = path
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{text}\"")
}

fn pkg_root(tmp: &Path) -> PathBuf {
    let root = tmp.join("pkg");
    place(&root, "data/audio/a.wav", &[b'x'; 10], None);
    place(&root, "data/audio/b.wav", &[b'x'; 20], None);
    place(&root, "data/img/c.png", &[b'x'; 30], None);
    place(&root, "top.txt", b"xxxxx", None);
    place(&root, "never.bin", b"xxxxxxx", None);
    root
}

fn class_of(root: &Path, text: &str, relative: &str) -> FileClass {
    let report = classify_text(root, text);
    let Some(entry) = report.by_relative(relative) else {
        panic!("no entry {relative}");
    };
    entry.class
}

#[test]
fn file_missing_from_package_reported() {
    let tmp = TempDir::new("parity-missing");
    let dev = tmp.join("dev");
    let pkg = tmp.join("pkg");
    place(&dev, "assets/needed.txt", b"needed", None);
    fs::create_dir_all(&pkg).unwrap_or_else(|err| panic!("pkg: {err}"));
    let dev_trace = tmp.join("dev.trace");
    let pkg_trace = tmp.join("pkg.trace");
    write(
        &dev_trace,
        format!("ok open {}/assets/needed.txt\n", dev.display()).as_bytes(),
    );
    write(&pkg_trace, b"");
    let report = diff_paths(&dev_trace, &dev, &pkg_trace, &pkg).unwrap_or_else(|err| panic!("diff: {err}"));
    assert_eq!(
        report.lines,
        vec!["missing assets/needed.txt".to_string()],
        "a dev open the package never served must be reported"
    );
    assert!(report.gate_fails());
    assert_eq!(report.exit_code, 1);
    assert_eq!(report.missing, vec!["assets/needed.txt".to_string()]);
    assert_eq!(report.json_value()["ok"], false);
}

#[test]
fn quoted_asset_trace_line_counts() {
    let tmp = TempDir::new("parity-quoted");
    let dev = tmp.join("dev");
    let pkg = tmp.join("pkg");
    place(&dev, "assets/needed.txt", b"needed", None);
    fs::create_dir_all(&pkg).unwrap_or_else(|err| panic!("pkg: {err}"));
    let dev_trace = tmp.join("dev.trace");
    let pkg_trace = tmp.join("pkg.trace");
    write(
        &dev_trace,
        format!("ok open {:?}\n", dev.join("assets/needed.txt")).as_bytes(),
    );
    write(&pkg_trace, b"");
    let report = diff_paths(&dev_trace, &dev, &pkg_trace, &pkg).unwrap_or_else(|err| panic!("diff: {err}"));
    assert_eq!(report.lines, vec!["missing assets/needed.txt".to_string()]);
    assert_eq!(report.exit_code, 1);
}

#[test]
fn never_touched_files_are_listed() {
    let tmp = TempDir::new("parity-never");
    let dev = tmp.join("dev");
    let pkg = tmp.join("pkg");
    place(&dev, "assets/needed.txt", b"needed", None);
    place(&pkg, "assets/needed.txt", b"needed", None);
    place(&pkg, "assets/idle.txt", b"idle", None);
    let dev_trace = tmp.join("dev.trace");
    let pkg_trace = tmp.join("pkg.trace");
    write_trace(&dev_trace, &dev, &[("open", "assets/needed.txt")]);
    write_trace(&pkg_trace, &pkg, &[("open", "assets/needed.txt")]);
    let report = diff_paths(&dev_trace, &dev, &pkg_trace, &pkg).unwrap_or_else(|err| panic!("diff: {err}"));
    assert_eq!(report.never_touched, vec!["assets/idle.txt".to_string()]);
    assert!(
        !report.gate_fails(),
        "a never-touched file is a report, not a failure"
    );
    assert_eq!(report.exit_code, 0);
    assert!(report.markdown.contains("assets/idle.txt"));
}

#[test]
fn a_cooked_equivalent_counts_as_present() {
    let tmp = TempDir::new("parity-cooked");
    let dev = tmp.join("dev");
    let pkg = tmp.join("pkg");
    let rel = "assets/maps/cave/ghost.gltf";
    let print_hex = "fedcba9876543210";
    let memo = gltf_memo_key(rel, print_hex, "remainder", 0, 2048, false);
    place(&dev, rel, b"{}", None);
    place(
        &pkg,
        "cooked/dir_prints.txt",
        format!("maps/cave\t{print_hex}\n").as_bytes(),
        None,
    );
    place(
        &pkg,
        "cooked/bake_index.txt",
        format!("{memo}\taabbccddeeff0011\n").as_bytes(),
        None,
    );
    place(&pkg, "cooked/gltf/aabbccddeeff0011.gltc", b"gltc", None);
    let dev_trace = tmp.join("dev.trace");
    let pkg_trace = tmp.join("pkg.trace");
    write_trace(&dev_trace, &dev, &[("open", rel)]);
    write_trace(&pkg_trace, &pkg, &[("open", "cooked/gltf/aabbccddeeff0011.gltc")]);
    let report = diff_paths(&dev_trace, &dev, &pkg_trace, &pkg).unwrap_or_else(|err| panic!("diff: {err}"));
    assert_eq!(report.lines, vec![format!("cooked {rel}")]);
    assert!(report.missing.is_empty());
    assert!(!report.gate_fails());
}

#[test]
fn test_memo_key_matches_loader() {
    let rel = "assets_regia/maps/bore/hero.gltf";
    let print_hex = "0123456789abcdef";
    assert_eq!(
        gltf_memo_key(rel, print_hex, "remainder", 0, 2048, false),
        "0bcdc3c9c1f6838a"
    );
    assert_eq!(
        gltf_memo_key(rel, print_hex, "remainder", 1, 512, true),
        "e321bca7d407a977"
    );
    assert_eq!(
        gltf_memo_key(rel, print_hex, "", 0, 2048, false),
        "e72f2a1ff1032f87"
    );
}

#[test]
fn test_gltf_served_from_cooked() {
    let tmp = TempDir::new("parity-served");
    let dev = tmp.join("dev");
    let pkg = tmp.join("pkg");
    let bore = "0123456789abcdef";
    let cave = "fedcba9876543210";
    let ghost = "assets_regia/maps/cave/ghost.gltf";
    let meshonly = "assets_regia/maps/cave/meshonly.gltf";
    let ghost_memo = gltf_memo_key(ghost, cave, "remainder", 0, 2048, false);
    let mesh_memo = gltf_memo_key(meshonly, cave, "remainder", 0, 2048, false);
    for rel in [
        "assets_regia/maps/bore/hero.gltf",
        "assets_regia/maps/bore/hero.bin",
        ghost,
        "assets_regia/maps/cave/ghost.bin",
        meshonly,
    ] {
        place(&dev, rel, b"{}", None);
    }
    place(&pkg, "assets_regia/maps/bore/hero.gltf", b"{}", None);
    place(&pkg, "assets_regia/maps/bore/hero.bin", b"bin", None);
    place(
        &pkg,
        "cooked/dir_prints.txt",
        format!("maps/bore\t{bore}\nmaps/cave\t{cave}\n").as_bytes(),
        None,
    );
    place(
        &pkg,
        "cooked/bake_index.txt",
        format!(
            "not-the-memo\tffffffffffffffff\n{ghost_memo}\taabbccddeeff0011\n{mesh_memo}\tmeshcache00000001\n"
        )
        .as_bytes(),
        None,
    );
    place(&pkg, "cooked/gltf/ffffffffffffffff.gltc", b"gltc", None);
    place(&pkg, "cooked/gltf/aabbccddeeff0011.gltc", b"gltc", None);
    place(&pkg, "cooked/gltf/meshcache00000001.gltc", b"gltc", None);
    place(&pkg, "cooked/mesh/00112233445566778899aabb.bms", b"bms", None);
    let lines = lines_of(
        tmp.path(),
        &[
            ("open", "assets_regia/maps/bore/hero.gltf"),
            ("open", "assets_regia/maps/bore/hero.bin"),
            ("open", ghost),
            ("open", "assets_regia/maps/cave/ghost.bin"),
            ("open", meshonly),
        ],
        &[
            ("open", "cooked/gltf/ffffffffffffffff.bltc"),
            ("open", "cooked/gltf/aabbccddeeff0011.gltc"),
            ("open", "cooked/mesh/00112233445566778899aabb.bms"),
        ],
    );
    assert_eq!(
        lines,
        vec![
            "cooked assets_regia/maps/bore/hero.bin".to_string(),
            "cooked assets_regia/maps/bore/hero.gltf".to_string(),
            "cooked assets_regia/maps/cave/ghost.bin".to_string(),
            "cooked assets_regia/maps/cave/ghost.gltf".to_string(),
            "cooked assets_regia/maps/cave/meshonly.gltf".to_string(),
        ]
    );
    assert!(!lines.iter().any(|line| line.starts_with("missing ")));
}

#[test]
fn test_gltf_absent_fails_gate() {
    let tmp = TempDir::new("parity-absent");
    let dev = tmp.join("dev");
    let pkg = tmp.join("pkg");
    let dev_files = [
        "assets_regia/maps/bore/fresh.gltf",
        "assets_regia/maps/bore/gone.gltf",
        "assets_regia/maps/bore/gone.bin",
        "assets_regia/maps/bore/rock_LOD0.gltf",
        "assets_regia/maps/bore_twelve/textures/detail/sand.png",
        "assets_regia/maps/other/plain.gltf",
        "assets_regia/maps/other/plain.bin",
        "assets_regia/tex/a.png",
        "assets_regia/ui/icon.png",
    ];
    for rel in dev_files {
        place(&dev, rel, b"{}", Some(OLD));
    }
    place(&dev, "assets_regia/maps/bore/fresh.gltf", b"{}", Some(NEW));
    place(&pkg, "assets_regia/maps/bore/fresh.gltf", b"{}", None);
    place(&pkg, "assets_regia/maps/bore/rock_LOD0.gltf", b"{}", None);
    place(
        &pkg,
        "assets_regia/maps/bore_twelve/cooked/terrain.tpak",
        b"tpak",
        None,
    );
    place(&pkg, "assets_regia/maps/other/plain.gltf", b"{}", None);
    place(
        &pkg,
        "cooked/dir_prints.txt",
        b"maps/bore\t0123456789abcdef\n",
        None,
    );
    place(
        &pkg,
        "cooked/bake_index.txt",
        b"not-the-memo\tffffffffffffffff\n",
        None,
    );
    place(
        &pkg,
        "build-info.toml",
        format!("staged_at = \"{STAMP}\"\n").as_bytes(),
        None,
    );
    place(&pkg, "cooked/gltf/ffffffffffffffff.gltc", b"gltc", None);
    place(&pkg, "cooked/mesh/eeeeeeeeeeeeeeeeeeeeeeee.bms", b"bms", None);
    let dev_rows: Vec<(&str, &str)> = dev_files.iter().map(|rel| ("open", *rel)).collect();
    let lines = lines_of(
        tmp.path(),
        &dev_rows,
        &[
            ("open", "assets_regia/tex/a.jpg"),
            ("open", "cooked/gltf/ffffffffffffffff.gltc"),
            ("open", "cooked/mesh/eeeeeeeeeeeeeeeeeeeeeeee.bms"),
        ],
    );
    assert_eq!(
        lines,
        vec![
            "drift assets_regia/maps/bore/fresh.gltf".to_string(),
            "missing assets_regia/maps/bore/gone.bin".to_string(),
            "missing assets_regia/maps/bore/gone.gltf".to_string(),
            "dieted assets_regia/maps/bore/rock_LOD0.gltf".to_string(),
            "dieted assets_regia/maps/bore_twelve/textures/detail/sand.png".to_string(),
            "missing assets_regia/maps/other/plain.bin".to_string(),
            "missing assets_regia/maps/other/plain.gltf".to_string(),
            "dieted assets_regia/tex/a.png".to_string(),
            "missing assets_regia/ui/icon.png".to_string(),
        ]
    );
    assert!(lines.iter().any(|line| line.starts_with("missing ")));
    let dev_trace = tmp.join("dev.trace");
    let pkg_trace = tmp.join("pkg.trace");
    let report = diff_paths(&dev_trace, &dev, &pkg_trace, &pkg).unwrap_or_else(|err| panic!("diff: {err}"));
    assert!(report.gate_fails());
    let strict = diff_paths_mode(&dev_trace, &dev, &pkg_trace, &pkg, true)
        .unwrap_or_else(|err| panic!("strict: {err}"));
    assert_eq!(strict.exit_code, 1);
}

#[test]
fn test_existing_bin_unchanged() {
    let rel_bin = "assets_regia/props/a.bin";
    let rel_gltf = "assets_regia/props/a.gltf";
    let tmp = TempDir::new("parity-bin");
    let dev = tmp.join("dev");
    let pkg = tmp.join("pkg");
    place(&dev, rel_bin, b"bin", None);
    place(&pkg, rel_bin, b"bin", None);
    place(&pkg, rel_gltf, b"{}", None);
    let lines = lines_of(tmp.path(), &[("open", rel_bin)], &[("stat", rel_gltf)]);
    assert_eq!(lines, vec![format!("cooked {rel_bin}")]);
    assert!(!lines.iter().any(|line| line.starts_with("missing ")));

    let tmp = TempDir::new("parity-bin-miss");
    let dev = tmp.join("dev");
    let pkg = tmp.join("pkg");
    place(&dev, rel_bin, b"bin", None);
    place(&pkg, rel_bin, b"bin", None);
    place(&pkg, rel_gltf, b"{}", None);
    let lines = lines_of(tmp.path(), &[("open", rel_bin)], &[]);
    assert_eq!(lines, vec![format!("missing {rel_bin}")]);
}

#[test]
fn open_without_pid_prefix_classifies_opened() {
    let tmp = TempDir::new("ot-openat");
    let root = pkg_root(tmp.path());
    let target = root.join("data/audio/a.wav");
    let text = format!(
        "openat(AT_FDCWD, {}, O_RDONLY|O_CLOEXEC) = 3\n",
        quote_path(&target)
    );
    assert_eq!(class_of(&root, &text, "data/audio/a.wav"), FileClass::Opened);
}

#[test]
fn open_syscall_variant_classifies_opened() {
    let tmp = TempDir::new("ot-open");
    let root = pkg_root(tmp.path());
    let target = root.join("top.txt");
    let text = format!("open({}, O_RDONLY) = 3\n", quote_path(&target));
    assert_eq!(class_of(&root, &text, "top.txt"), FileClass::Opened);
}

#[test]
fn pid_prefixes_are_stripped() {
    let tmp = TempDir::new("ot-pid");
    let root = pkg_root(tmp.path());
    let first = root.join("data/audio/a.wav");
    let second = root.join("data/audio/b.wav");
    let text = format!(
        "1234  openat(AT_FDCWD, {}, O_RDONLY) = 3\n5678 open({}, O_RDWR) = 4\n",
        quote_path(&first),
        quote_path(&second)
    );
    assert_eq!(class_of(&root, &text, "data/audio/a.wav"), FileClass::Opened);
    assert_eq!(class_of(&root, &text, "data/audio/b.wav"), FileClass::Opened);
}

#[test]
fn resumed_line_with_pending_unfinished() {
    let tmp = TempDir::new("ot-resume");
    let root = pkg_root(tmp.path());
    let target = root.join("data/audio/a.wav");
    let text = format!(
        "42 openat(AT_FDCWD, {}, O_RDONLY <unfinished ...>\n42 <... openat resumed> ) = 3\n",
        quote_path(&target)
    );
    assert_eq!(class_of(&root, &text, "data/audio/a.wav"), FileClass::Opened);
}

#[test]
fn resumed_line_without_pending_contains_path() {
    let tmp = TempDir::new("ot-fragment");
    let root = pkg_root(tmp.path());
    let target = root.join("data/img/c.png");
    let text = format!("99 <... openat resumed> {}, O_RDONLY) = 3\n", quote_path(&target));
    assert_eq!(class_of(&root, &text, "data/img/c.png"), FileClass::Opened);
}

#[test]
fn quoted_path_with_escaped_quote() {
    let tmp = TempDir::new("ot-quote");
    let root = pkg_root(tmp.path());
    let target = place(&root, "data/audio/a\"b.wav", &[b'x'; 11], None);
    let text = format!("openat(AT_FDCWD, {}, O_RDONLY) = 3\n", quote_path(&target));
    assert_eq!(class_of(&root, &text, "data/audio/a\"b.wav"), FileClass::Opened);
}

#[test]
fn octal_escape_in_quoted_path() {
    let tmp = TempDir::new("ot-octal");
    let root = pkg_root(tmp.path());
    let target = place(&root, "data/my file.txt", &[b'x'; 12], None);
    let escaped = target.display().to_string().replace(' ', "\\040");
    let text = format!("openat(AT_FDCWD, \"{escaped}\", O_RDONLY) = 3\n");
    assert_eq!(class_of(&root, &text, "data/my file.txt"), FileClass::Opened);
}

#[test]
fn enoent_failed_open_is_missing_tried() {
    let tmp = TempDir::new("ot-enoent");
    let root = pkg_root(tmp.path());
    let gone = root.join("data/audio/gone.wav");
    let text = format!(
        "openat(AT_FDCWD, {}, O_RDONLY) = -1 ENOENT (No such file or directory)\n",
        quote_path(&gone)
    );
    let report = classify_text(&root, &text);
    let missing: Vec<_> = report.missing_tried.iter().map(|e| e.relative.as_str()).collect();
    assert_eq!(missing, vec!["data/audio/gone.wav"]);
    assert_eq!(report.class_count(FileClass::MissingTried), 1);
    assert_eq!(report.class_bytes(FileClass::MissingTried), 0);
    assert_eq!(report.class_count(FileClass::Opened), 0);
}

#[test]
fn stat_and_access_only_are_stat_only() {
    let tmp = TempDir::new("ot-stat");
    let root = pkg_root(tmp.path());
    let stated = root.join("data/audio/a.wav");
    let accessed = root.join("data/audio/b.wav");
    let text = format!(
        "stat({}, {{st_mode=S_IFREG|0644, st_size=10}}) = 0\naccess({}, R_OK) = 0\n",
        quote_path(&stated),
        quote_path(&accessed)
    );
    assert_eq!(class_of(&root, &text, "data/audio/a.wav"), FileClass::StatOnly);
    assert_eq!(class_of(&root, &text, "data/audio/b.wav"), FileClass::StatOnly);
    let report = classify_text(&root, &text);
    let names: Vec<_> = report
        .stat_only_files
        .iter()
        .map(|e| e.relative.as_str())
        .collect();
    assert_eq!(names, vec!["data/audio/a.wav", "data/audio/b.wav"]);
}

#[test]
fn statx_and_newfstatat_are_stat_only() {
    let tmp = TempDir::new("ot-statx");
    let root = pkg_root(tmp.path());
    let first = root.join("data/img/c.png");
    let second = root.join("top.txt");
    let text = format!(
        "statx(AT_FDCWD, {}, AT_SYMLINK_NOFOLLOW, STATX_BASIC_STATS, {{stx_mask=0x7ff}}) = 0\nnewfstatat(AT_FDCWD, {}, {{st_mode=S_IFREG|0644}}, 0) = 0\n",
        quote_path(&first),
        quote_path(&second)
    );
    assert_eq!(class_of(&root, &text, "data/img/c.png"), FileClass::StatOnly);
    assert_eq!(class_of(&root, &text, "top.txt"), FileClass::StatOnly);
}

#[test]
fn never_touched_and_classes_sum_to_total() {
    let tmp = TempDir::new("ot-sum");
    let root = pkg_root(tmp.path());
    let target = root.join("data/audio/a.wav");
    let report = classify_text(
        &root,
        &format!("openat(AT_FDCWD, {}, O_RDONLY) = 3\n", quote_path(&target)),
    );
    assert_eq!(report.total_files, 5);
    let partition = report.class_count(FileClass::Opened)
        + report.class_count(FileClass::StatOnly)
        + report.class_count(FileClass::NeverTouched);
    assert_eq!(partition, report.total_files);
    let bytes = report.class_bytes(FileClass::Opened)
        + report.class_bytes(FileClass::StatOnly)
        + report.class_bytes(FileClass::NeverTouched);
    assert_eq!(bytes, report.total_bytes);
    assert_eq!(report.total_bytes, 72);
    let names: Vec<_> = report
        .never_touched_files
        .iter()
        .map(|e| e.relative.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["data/audio/b.wav", "data/img/c.png", "never.bin", "top.txt"]
    );
}

#[test]
fn failed_open_on_existing_file_is_not_missing() {
    let tmp = TempDir::new("ot-eacces");
    let root = pkg_root(tmp.path());
    let target = root.join("data/audio/a.wav");
    let text = format!(
        "openat(AT_FDCWD, {}, O_RDONLY) = -1 EACCES (Permission denied)\n",
        quote_path(&target)
    );
    let report = classify_text(&root, &text);
    assert!(report.missing_tried.is_empty());
    assert_eq!(class_of(&root, &text, "data/audio/a.wav"), FileClass::StatOnly);
}

#[test]
fn write_only_open_is_not_a_read_open() {
    let tmp = TempDir::new("ot-write");
    let root = pkg_root(tmp.path());
    let target = root.join("data/audio/a.wav");
    let text = format!(
        "openat(AT_FDCWD, {}, O_WRONLY|O_CREAT|O_TRUNC, 0666) = 3\n",
        quote_path(&target)
    );
    assert_eq!(class_of(&root, &text, "data/audio/a.wav"), FileClass::StatOnly);
}

#[test]
fn opened_wins_over_stat_only() {
    let tmp = TempDir::new("ot-wins");
    let root = pkg_root(tmp.path());
    let target = root.join("data/audio/a.wav");
    let text = format!(
        "stat({}, {{st_mode=S_IFREG|0644}}) = 0\nopenat(AT_FDCWD, {}, O_RDONLY) = 3\n",
        quote_path(&target),
        quote_path(&target)
    );
    assert_eq!(class_of(&root, &text, "data/audio/a.wav"), FileClass::Opened);
    let report = classify_text(&root, &text);
    assert!(report.stat_only_files.is_empty());
}

#[test]
fn relative_path_resolved_against_known_cwd() {
    let tmp = TempDir::new("ot-cwd");
    let root = pkg_root(tmp.path());
    let text = format!(
        "chdir({}) = 0\nopenat(AT_FDCWD, \"data/audio/a.wav\", O_RDONLY) = 3\n",
        quote_path(&root)
    );
    assert_eq!(class_of(&root, &text, "data/audio/a.wav"), FileClass::Opened);
}

#[test]
fn relative_path_resolved_against_dirfd() {
    let tmp = TempDir::new("ot-dirfd");
    let root = pkg_root(tmp.path());
    let data_dir = root.join("data");
    let text = format!(
        "openat(AT_FDCWD, {}, O_RDONLY|O_DIRECTORY) = 3\nopenat(3, \"audio/a.wav\", O_RDONLY) = 4\n",
        quote_path(&data_dir)
    );
    assert_eq!(class_of(&root, &text, "data/audio/a.wav"), FileClass::Opened);
}

#[test]
fn relative_path_without_known_cwd_matches_root_relative() {
    let tmp = TempDir::new("ot-rel");
    let root = pkg_root(tmp.path());
    let text = "openat(AT_FDCWD, \"data/audio/b.wav\", O_RDONLY) = 3\n";
    assert_eq!(class_of(&root, text, "data/audio/b.wav"), FileClass::Opened);
}

#[test]
fn outside_root_and_unrelated_lines_are_ignored() {
    let tmp = TempDir::new("ot-out");
    let root = pkg_root(tmp.path());
    let text = "openat(AT_FDCWD, \"/etc/passwd\", O_RDONLY) = 3\n1234 +++ exited with 0 +++\n--- SIGCHLD {si_signo=SIGCHLD} ---\nstrace: Process 1234 attached\n";
    let report = classify_text(&root, text);
    assert_eq!(report.class_count(FileClass::Opened), 0);
    assert_eq!(report.total_files, 5);
}

#[test]
fn grouping_by_first_two_path_segments() {
    let tmp = TempDir::new("ot-group");
    let root = pkg_root(tmp.path());
    let first = root.join("data/audio/a.wav");
    let second = root.join("data/img/c.png");
    let text = format!(
        "openat(AT_FDCWD, {}, O_RDONLY) = 3\naccess({}, F_OK) = 0\n",
        quote_path(&first),
        quote_path(&second)
    );
    let report = classify_text(&root, &text);
    assert_eq!(report.group_count("data/audio", FileClass::Opened), 1);
    assert_eq!(report.group_bytes("data/audio", FileClass::Opened), 10);
    assert_eq!(report.group_count("data/img", FileClass::StatOnly), 1);
    assert_eq!(report.group_count("top.txt", FileClass::NeverTouched), 1);
}

#[test]
fn merge_several_traces_unions_touches() {
    let tmp = TempDir::new("ot-merge");
    let root = pkg_root(tmp.path());
    let first = root.join("data/audio/a.wav");
    let second = root.join("data/audio/b.wav");
    let one = tmp.join("one.strace");
    let two = tmp.join("two.strace");
    write(
        &one,
        format!("openat(AT_FDCWD, {}, O_RDONLY) = 3\n", quote_path(&first)).as_bytes(),
    );
    write(
        &two,
        format!("stat({}, {{st_mode=S_IFREG}}) = 0\n", quote_path(&second)).as_bytes(),
    );
    let report = build_open_report(&root, &[one, two], None).unwrap_or_else(|err| panic!("{err}"));
    let Some(a) = report.by_relative("data/audio/a.wav") else {
        panic!("a");
    };
    let Some(b) = report.by_relative("data/audio/b.wav") else {
        panic!("b");
    };
    assert_eq!(a.class, FileClass::Opened);
    assert_eq!(b.class, FileClass::StatOnly);
}

#[test]
fn manifest_per_chunk() {
    let tmp = TempDir::new("ot-chunk");
    let root = pkg_root(tmp.path());
    let first = root.join("data/audio/a.wav");
    let trace = tmp.join("run.strace");
    write(
        &trace,
        format!("openat(AT_FDCWD, {}, O_RDONLY) = 3\n", quote_path(&first)).as_bytes(),
    );
    let manifest = tmp.join("manifest.json");
    write(
        &manifest,
        br#"{"chunks":[{"name":"boot","files":["data/audio/a.wav","data/audio/b.wav"]},{"name":"media","files":["data/img/c.png","top.txt"]}]}"#,
    );
    let report = build_open_report(&root, &[trace], Some(&manifest)).unwrap_or_else(|err| panic!("{err}"));
    let Some(chunks) = &report.per_chunk else {
        panic!("chunks");
    };
    let Some(boot) = chunks.get("boot") else {
        panic!("boot");
    };
    assert_eq!(boot.total_files, 2);
    assert_eq!(boot.class_count(FileClass::Opened), 1);
    assert_eq!(boot.class_count(FileClass::NeverTouched), 1);
    let names: Vec<_> = boot
        .never_touched_files
        .iter()
        .map(|e| e.relative.as_str())
        .collect();
    assert_eq!(names, vec!["data/audio/b.wav"]);
    let Some(media) = chunks.get("media") else {
        panic!("media");
    };
    assert_eq!(media.class_count(FileClass::NeverTouched), 2);
}

#[test]
fn json_output_via_main() {
    let tmp = TempDir::new("ot-json");
    let root = pkg_root(tmp.path());
    let target = root.join("data/audio/a.wav");
    let trace = tmp.join("run.strace");
    write(
        &trace,
        format!("7 openat(AT_FDCWD, {}, O_RDONLY) = 3\n", quote_path(&target)).as_bytes(),
    );
    let args = vec![
        "--trace".to_string(),
        trace.display().to_string(),
        "--root".to_string(),
        root.display().to_string(),
        "--json".to_string(),
    ];
    let (code, stdout, stderr) = open_trace_main(&args);
    assert_eq!(code, 0, "{stderr}");
    let payload: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(payload["total_files"], 5);
    assert_eq!(
        payload["classes"]["OPENED"],
        serde_json::json!({"count": 1, "bytes": 10})
    );
    assert_eq!(payload["classes"]["NEVER_TOUCHED"]["count"], 4);
    assert_eq!(payload["groups"]["data/audio"]["OPENED"]["count"], 1);
    assert_eq!(payload["stat_only_files"], serde_json::json!([]));
    assert_eq!(payload["opened_files"][0]["relative"], "data/audio/a.wav");
}

#[test]
fn text_and_markdown_output_list_candidates() {
    let tmp = TempDir::new("ot-text");
    let root = pkg_root(tmp.path());
    let stated = root.join("data/audio/a.wav");
    let trace = tmp.join("run.strace");
    write(
        &trace,
        format!("stat({}, {{st_mode=S_IFREG}}) = 0\n", quote_path(&stated)).as_bytes(),
    );
    let base = vec![
        "--trace".to_string(),
        trace.display().to_string(),
        "--root".to_string(),
        root.display().to_string(),
    ];
    let (code, stdout, _) = open_trace_main(&base);
    assert_eq!(code, 0);
    assert!(stdout.contains("STAT_ONLY: 1 files, 10 bytes"));
    assert!(stdout.contains("data/audio/a.wav"));
    assert!(stdout.contains("never.bin"));
    let mut md_args = base;
    md_args.push("--markdown".to_string());
    let (code, markdown, _) = open_trace_main(&md_args);
    assert_eq!(code, 0);
    assert!(markdown.contains("# open_trace report"));
    assert!(markdown.contains("| STAT_ONLY | 1 | 10 |"));
    assert!(markdown.contains("data/audio/a.wav"));
}

#[test]
fn missing_trace_file_returns_error_code() {
    let tmp = TempDir::new("ot-notrace");
    let root = pkg_root(tmp.path());
    let args = vec![
        "--trace".to_string(),
        tmp.join("nope.strace").display().to_string(),
        "--root".to_string(),
        root.display().to_string(),
    ];
    let (code, _, stderr) = open_trace_main(&args);
    assert_eq!(code, 2);
    assert!(stderr.contains("trace file not found"));
}

#[test]
fn missing_root_returns_error_code() {
    let tmp = TempDir::new("ot-noroot");
    let trace = tmp.join("run.strace");
    write(&trace, b"");
    let args = vec![
        "--trace".to_string(),
        trace.display().to_string(),
        "--root".to_string(),
        tmp.join("absent").display().to_string(),
    ];
    let (code, _, stderr) = open_trace_main(&args);
    assert_eq!(code, 2);
    assert!(stderr.contains("not a directory"));
}

#[test]
fn help_lists_examples_and_exit_codes() {
    let (code, stdout, _) = open_trace_main(&["--help".to_string()]);
    assert_eq!(code, 0);
    assert!(stdout.contains("usage:"));
    assert!(stdout.contains("Examples:"));
    assert!(stdout.contains("Exit codes:"));
}
