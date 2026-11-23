// SPDX-License-Identifier: MPL-2.0
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

#[derive(Clone, Copy)]
enum Expectation {
    Success,
    Failure(&'static str),
}

/// Cargo names a proc-macro `lib<crate>-<hash>.{so,dylib}` on Unix and `<crate>-<hash>.dll`
/// on Windows (no `lib` prefix).
fn is_proc_macro_artifact(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some((stem, suffix)) = name.rsplit_once('.') else {
        return false;
    };
    if !matches!(suffix, "so" | "dylib" | "dll") {
        return false;
    }
    let stem = stem.strip_prefix("lib").unwrap_or(stem);
    let Some(hash) = stem.strip_prefix("regia_macros-") else {
        return false;
    };
    !hash.is_empty()
}

/// The newest matching dylib. Equal modification times break toward the later path so the
/// choice does not follow `read_dir` order.
fn select_proc_macro_artifact(files: &[(PathBuf, SystemTime)]) -> Option<PathBuf> {
    files
        .iter()
        .filter(|(path, _)| path.file_name().is_some_and(is_proc_macro_artifact))
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)))
        .map(|(path, _)| path.clone())
}

fn macro_artifact(deps_dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(deps_dir).ok()?;
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((entry.path(), modified));
    }
    select_proc_macro_artifact(&files)
}

fn compile_fixture(name: &str, expectation: Expectation) -> Result<(), String> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsStr::new("rustc").to_owned());
    compile_fixture_with_rustc(name, expectation, rustc.as_ref())
}

fn compile_fixture_with_rustc(name: &str, expectation: Expectation, rustc: &OsStr) -> Result<(), String> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let test_exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let deps_dir = test_exe
        .parent()
        .ok_or_else(|| "test executable has no parent".to_owned())?;
    let temp = std::env::temp_dir().join(format!(
        "regia-macro-compile-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&temp).map_err(|e| format!("create {}: {e}", temp.display()))?;
    compile_in_dir(&temp, deps_dir, name, expectation, rustc)
}

/// Removes `path` when dropped, including when a `?` returns early.
struct RemoveDir<'a>(&'a Path);

impl Drop for RemoveDir<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0);
    }
}

/// Runs one fixture in `temp` and removes `temp` on every return, including a failed spawn.
fn compile_in_dir(
    temp: &Path,
    deps_dir: &Path,
    name: &str,
    expectation: Expectation,
    rustc: &OsStr,
) -> Result<(), String> {
    let _cleanup = RemoveDir(temp);
    let dylib = proc_macro_dylib(deps_dir, temp, rustc)?;
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.rs"));
    let executable = temp.join(if cfg!(windows) { "fixture.exe" } else { "fixture" });
    let compile = Command::new(rustc)
        .arg("--edition=2024")
        .arg("--test")
        .arg(&source)
        .arg("--extern")
        .arg(format!("regia_macros={}", dylib.display()))
        .arg("-L")
        .arg(format!("dependency={}", deps_dir.display()))
        .arg("-o")
        .arg(&executable)
        .output()
        .map_err(|e| format!("start rustc for {name}: {e}"))?;

    let diagnostics = || {
        format!(
            "{}{}",
            String::from_utf8_lossy(&compile.stderr),
            String::from_utf8_lossy(&compile.stdout)
        )
    };
    match expectation {
        Expectation::Success if compile.status.success() => {
            let run = Command::new(&executable)
                .output()
                .map_err(|e| format!("run {name}: {e}"))?;
            if run.status.success() {
                Ok(())
            } else {
                // libtest prints failures to stdout, so a run-time assertion failure would
                // otherwise surface as an empty message.
                Err(format!(
                    "fixture {name} compiled but failed when run:\n{}{}",
                    String::from_utf8_lossy(&run.stderr),
                    String::from_utf8_lossy(&run.stdout)
                ))
            }
        }
        Expectation::Success => Err(format!(
            "fixture {name} unexpectedly failed to compile:\n{}",
            diagnostics()
        )),
        Expectation::Failure(_) if compile.status.success() => {
            Err(format!("fixture {name} unexpectedly compiled"))
        }
        Expectation::Failure(needle) => {
            let rendered = diagnostics();
            if rendered.contains(needle) {
                Ok(())
            } else {
                Err(format!(
                    "fixture {name} failed without diagnostic {needle:?}; output was:\n{rendered}"
                ))
            }
        }
    }
}

/// The proc-macro dylib `rustc --extern` should load. `cargo test -p regia_macros --lib` on a
/// fresh target builds the test harness only, so when Cargo has not left a dylib in `deps`,
/// this compiles one from the crate source.
fn proc_macro_dylib(deps_dir: &Path, dest: &Path, rustc: &OsStr) -> Result<PathBuf, String> {
    if let Some(found) = macro_artifact(deps_dir) {
        return Ok(found);
    }
    // Cargo sets `RUSTC` to a real compiler path. A missing path (the scratch-dir test) must
    // fail its own spawn and must not wait on, or publish, the shared build.
    if Path::new(rustc).is_file() {
        return shared_proc_macro(deps_dir, rustc);
    }
    compile_proc_macro(deps_dir, dest, rustc)
}

/// One proc-macro dylib for every fixture in this process. Building it per fixture repeats the
/// same `rustc` and races if they share an output path.
fn shared_proc_macro(deps_dir: &Path, rustc: &OsStr) -> Result<PathBuf, String> {
    static BUILT: Mutex<Option<PathBuf>> = Mutex::new(None);
    let mut slot = BUILT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(path) = slot.clone() {
        return Ok(path);
    }
    let dir = std::env::temp_dir().join(format!("regia-macros-selfbuild-{}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = compile_proc_macro(deps_dir, &dir, rustc)?;
    *slot = Some(path.clone());
    Ok(path)
}

fn compile_proc_macro(deps_dir: &Path, dest: &Path, rustc: &OsStr) -> Result<PathBuf, String> {
    let output_name = if cfg!(windows) {
        "regia_macros_fixture.dll"
    } else if cfg!(target_os = "macos") {
        "libregia_macros_fixture.dylib"
    } else {
        "libregia_macros_fixture.so"
    };
    let dylib = dest.join(output_name);
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
    let syn = newest_rlib(deps_dir, "syn")?;
    let quote_lib = newest_rlib(deps_dir, "quote")?;
    let proc_macro2 = newest_rlib(deps_dir, "proc_macro2")?;
    let compile = Command::new(rustc)
        .arg("--edition=2024")
        .arg("--crate-name")
        .arg("regia_macros")
        .arg("--crate-type")
        .arg("proc-macro")
        .arg(&src)
        // `proc_macro` is a sysroot crate. Naming it with no path is how `rustc` links it;
        // `--crate-type proc-macro` alone does not.
        .arg("--extern")
        .arg("proc_macro")
        .arg("--extern")
        .arg(format!("syn={}", syn.display()))
        .arg("--extern")
        .arg(format!("quote={}", quote_lib.display()))
        .arg("--extern")
        .arg(format!("proc_macro2={}", proc_macro2.display()))
        .arg("-L")
        .arg(format!("dependency={}", deps_dir.display()))
        .arg("--cap-lints")
        .arg("allow")
        .arg("-o")
        .arg(&dylib)
        .output()
        .map_err(|e| format!("start rustc for the proc-macro: {e}"))?;
    if compile.status.success() {
        Ok(dylib)
    } else {
        Err(format!(
            "building the regia_macros proc-macro failed:\n{}{}",
            String::from_utf8_lossy(&compile.stderr),
            String::from_utf8_lossy(&compile.stdout)
        ))
    }
}

fn newest_rlib(deps_dir: &Path, crate_name: &str) -> Result<PathBuf, String> {
    let prefix = format!("lib{crate_name}-");
    let entries = fs::read_dir(deps_dir).map_err(|e| format!("read {}: {e}", deps_dir.display()))?;
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with(&prefix) && name.ends_with(".rlib") {
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            files.push((path, modified));
        }
    }
    files
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)))
        .map(|(path, _)| path)
        .ok_or_else(|| format!("missing {crate_name} rlib in {}", deps_dir.display()))
}

#[test]
fn derives_compile_and_run_in_isolated_rustc_crates() {
    for name in [
        "script_valid",
        "bt_task_valid",
        "replicate_valid",
        "replicate_enum_valid",
        "replicate_max_len_valid",
        "net_message_valid",
        "net_message_enum_valid",
    ] {
        compile_fixture(name, Expectation::Success).unwrap_or_else(|err| panic!("{err}"));
    }
}

#[test]
fn derives_reject_bad_inputs_with_stable_diagnostic_fragments() {
    for (name, fragment) in [
        ("script_duplicate", "duplicate `#[script]` attribute"),
        ("script_misplaced", "`#[script]` applies to the struct"),
        ("script_bad_number", "order must be an i32"),
        ("bt_task_unsupported", "cannot be derived for the union"),
        ("replicate_duplicate", "duplicate `#[replicate]` attribute"),
        ("replicate_misplaced", "`#[replicate]` goes on fields"),
        ("replicate_unsupported", "expected `skip`"),
        ("replicate_bad_number", "expected `usize`"),
        ("net_message_duplicate", "duplicate `version`"),
        ("net_message_unsupported", "expected `skip`"),
        ("net_message_bad_number", "`version` is a u32"),
    ] {
        compile_fixture(name, Expectation::Failure(fragment)).unwrap_or_else(|err| panic!("{err}"));
    }
}

#[test]
fn a_deliberately_invalid_fixture_cannot_be_accepted_as_a_pass() {
    let incorrectly_expected_to_pass = compile_fixture("script_bad_number", Expectation::Success);
    assert!(
        incorrectly_expected_to_pass.is_err(),
        "the harness accepted a fixture with an invalid script order"
    );
}

#[test]
fn proc_macro_artifact_name_matches_unix_dylibs() {
    assert!(is_proc_macro_artifact(OsStr::new("libregia_macros-abc123.so")));
    assert!(is_proc_macro_artifact(OsStr::new("libregia_macros-abc123.dylib")));
    assert!(!is_proc_macro_artifact(OsStr::new("libregia_macros-abc123.rlib")));
    assert!(!is_proc_macro_artifact(OsStr::new("libregia_macros-abc123.so.d")));
    assert!(!is_proc_macro_artifact(OsStr::new("libother-abc123.so")));
}

#[test]
fn proc_macro_artifact_name_matches_windows_dlls() {
    assert!(
        is_proc_macro_artifact(OsStr::new("regia_macros-abc123.dll")),
        "Windows proc-macro dlls have no lib prefix"
    );
}

#[test]
fn proc_macro_artifact_prefers_the_newest_dylib() {
    use std::time::Duration;
    let older = PathBuf::from("libregia_macros-old.so");
    let newer = PathBuf::from("libregia_macros-new.so");
    let picked = select_proc_macro_artifact(&[
        (older, SystemTime::UNIX_EPOCH),
        (newer.clone(), SystemTime::UNIX_EPOCH + Duration::from_secs(10)),
    ]);
    assert_eq!(picked.as_ref(), Some(&newer));
}

#[test]
fn rustc_spawn_failure_removes_the_scratch_dir() {
    static NEXT_LEAK: AtomicU64 = AtomicU64::new(0);
    let test_exe = std::env::current_exe().expect("current_exe");
    let deps_dir = test_exe.parent().expect("test executable has a parent");
    let temp = std::env::temp_dir().join(format!(
        "regia-macro-compile-leak-{}-{}",
        std::process::id(),
        NEXT_LEAK.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&temp).unwrap_or_else(|e| panic!("create {}: {e}", temp.display()));
    let err = compile_in_dir(
        &temp,
        deps_dir,
        "script_valid",
        Expectation::Success,
        OsStr::new("/no/such/regia-rustc"),
    );
    let leaked = temp.exists();
    if leaked {
        let _ = fs::remove_dir_all(&temp);
    }
    assert!(err.is_err(), "missing rustc must fail");
    assert!(!leaked, "scratch dir survived a failed rustc spawn: {err:?}");
}
