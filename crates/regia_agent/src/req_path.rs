// SPDX-License-Identifier: MPL-2.0
//! Confining file paths that arrive in agent requests (ADR-1816).
//!
//! Requests are untrusted: `root.join(p)` replaces the root when `p` is absolute (and on Windows
//! when it is `C:x` or `\x`), `..` climbs out, and a symlink inside the project can lead anywhere.
//! Handlers resolve request paths through one of two rules:
//!
//! - [`project_path`]: a project-relative path (levels, prefabs, graphs, scripts). Absolute paths,
//!   drive and UNC prefixes, `..`, NUL, device names and symlinks leading out of the project are
//!   refused (`regia_core::project_path`).
//! - [`allowed_path`]: the output-root rule of `render.out_start`'s `out`: a relative path joins
//!   the project root; an absolute one must lie under the project root, the system temp
//!   directory or an `output_roots` entry of `.regia/agent.toml`; `..` is
//!   refused and the deepest existing ancestor is checked with symlinks resolved.
//!
//! Both answer `F0002` with `data.reason` `path_outside_project` and `data.path_reason` naming the
//! rule (`path_absolute`, `path_prefix`, `path_parent`, `path_escapes`, ...).

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use serde_json::json;

use crate::protocol::{INVALID_PARAMS, RpcError};

/// The `path_outside_project` error for request parameter `key`.
fn refused(key: &str, path_reason: &str, detail: &str) -> RpcError {
    refused_with(
        format!("`{key}` {detail}"),
        path_reason,
        "use a project-relative path without `..`; see book/agents/safety.md (Request paths)",
    )
}

/// The `path_outside_project` error with its own `message` and `hint`, `path_reason` naming the
/// rule the path broke (`render.out_start` `out` and the parameters that share its roots).
pub(crate) fn refused_with(message: impl Into<String>, path_reason: &str, hint: &str) -> RpcError {
    RpcError {
        code: INVALID_PARAMS,
        message: message.into(),
        data: Some(json!({
            "code": "F0002",
            "reason": "path_outside_project",
            "path_reason": path_reason,
            "hint": hint,
        })),
    }
}

/// Adds `data.allowed_roots` to a [`refused_with`] error: the kinds of directory the parameter may
/// lie under (`project`, `temp_dir`, `output_roots`, `replay_roots`), never a host
/// path (ADR-1965).
pub(crate) fn name_allowed_roots(error: &mut RpcError, kinds: &[&str]) {
    if let Some(data) = error.data.as_mut().and_then(serde_json::Value::as_object_mut) {
        data.insert("allowed_roots".into(), json!(kinds));
    }
}

/// The project root request paths resolve against: the asset server's root, else the agent's
/// `project_dir`, else the working directory.
pub fn project_root(world: &World) -> PathBuf {
    world
        .get_resource::<regia_asset::AssetServer>()
        .map(|s| s.project_root().to_path_buf())
        .or_else(|| {
            world
                .get_resource::<crate::AgentSettings>()
                .and_then(|s| s.project_dir.clone())
        })
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

/// Resolves a project-relative request path `rel` (parameter `key`) under `root`.
///
/// # Errors
/// `path_outside_project` when `rel` breaks a rule of `regia_core::project_path`.
pub fn project_path_in(root: &Path, key: &str, rel: &str) -> Result<PathBuf, RpcError> {
    // ADR-2821: a path a handler read from a file or built reaches this too.
    if crate::golden_tools::names_golden(rel) {
        return Err(crate::golden_tools::golden_path_denied(rel));
    }
    regia_core::project_path::resolve_write(root, rel).map_err(|e| {
        refused(
            key,
            e.code(),
            &format!("must be a project-relative path inside the project: {e}"),
        )
    })
}

/// Resolves a project-relative request path `rel` (parameter `key`) under [`project_root`].
///
/// # Errors
/// `path_outside_project` (see [`project_path_in`]).
pub fn project_path(world: &World, key: &str, rel: &str) -> Result<PathBuf, RpcError> {
    project_path_in(&project_root(world), key, rel)
}

/// Resolves a project-relative request path `rel` (parameter `key`) to an existing file under `root`,
/// for a handler that reads it. A FIFO, a socket or a device is refused here, so a read cannot block
/// the world thread on a pipe a project planted. A folder passes: reading one fails at once, and the
/// handler answers with its own code.
///
/// # Errors
/// `path_outside_project` when `rel` breaks a rule of `regia_core::project_path`; `F0003` (not found)
/// when nothing is there, or what is there is neither a file nor a folder.
pub fn project_file_in(root: &Path, key: &str, rel: &str) -> Result<PathBuf, RpcError> {
    use regia_core::project_path::{ProjectPathError, resolve_read};
    let not_found =
        |why: String| RpcError::not_found(format!("{rel}: {why}"), "check the path with assets.list or ls");
    let path = resolve_read(root, rel).map_err(|e| match e {
        ProjectPathError::Io(why) => not_found(why),
        e => refused(
            key,
            e.code(),
            &format!("must be a project-relative path inside the project: {e}"),
        ),
    })?;
    let kind = std::fs::metadata(&path)
        .map_err(|e| not_found(e.to_string()))?
        .file_type();
    if !kind.is_file() && !kind.is_dir() {
        return Err(not_found("not a regular file".to_string()));
    }
    Ok(path)
}

/// [`project_file_in`] with [`project_root`].
///
/// # Errors
/// See [`project_file_in`].
pub fn project_file(world: &World, key: &str, rel: &str) -> Result<PathBuf, RpcError> {
    project_file_in(&project_root(world), key, rel)
}

/// Checks a path derived from a resolved request path (a sibling file or directory) under `root`.
///
/// # Errors
/// `path_outside_project` when it leads out of `root` through a symlink.
pub fn check_derived(root: &Path, key: &str, path: &Path) -> Result<(), RpcError> {
    if crate::golden_tools::under_goldens(root, path) {
        return Err(crate::golden_tools::golden_path_denied(
            &path.display().to_string(),
        ));
    }
    regia_core::project_path::check_under_root(root, path).map_err(|e| {
        refused(
            key,
            e.code(),
            &format!("leads outside the project ({}): {e}", path.display()),
        )
    })
}

/// Resolves a request path under the allowed output roots (see the module docs), `roots[0]` being
/// `root`: relative paths join `root`.
///
/// # Errors
/// `path_outside_project` when the path lies outside every allowed root.
pub fn allowed_path_in(world: &World, root: &Path, key: &str, path: &str) -> Result<PathBuf, RpcError> {
    let mut roots = vec![root.to_path_buf(), std::env::temp_dir()];
    roots.extend(
        world
            .get_resource::<crate::AgentPermissions>()
            .map(|p| p.config.output_roots.clone())
            .unwrap_or_default(),
    );
    let resolved = regia_core::project_path::resolve_in_roots(&roots, path).map_err(|e| {
        refused(
            key,
            e.code(),
            &format!(
                "must be relative to the project or under the project, the temp directory or an \
                 `output_roots` entry, without `..` or a symlink leading out: {e}"
            ),
        )
    })?;
    if crate::golden_tools::under_goldens(root, &resolved) {
        return Err(crate::golden_tools::golden_path_denied(path));
    }
    Ok(resolved)
}

/// [`allowed_path_in`] with [`project_root`].
///
/// # Errors
/// `path_outside_project` (see [`allowed_path_in`]).
pub fn allowed_path(world: &World, key: &str, path: &str) -> Result<PathBuf, RpcError> {
    allowed_path_in(world, &project_root(world), key, path)
}

/// Resolves a request path naming a document a tool opens, reads or saves (ADR-7388): the one rule
/// every editor document and importer path parameter goes through.
///
/// A relative path joins `root` under the rules of `regia_core::project_path` (no `..` after
/// normalization, no drive letter or `:`, no leading `/` or `\`, no NUL, no device name). An absolute
/// path must lie under `root` or the system temp directory; unlike [`allowed_path_in`],
/// `output_roots` do not count, because they name where renders go, not which files a
/// tool may read or overwrite (an editor started from the home folder would otherwise open all of it).
/// A UNC or verbatim form (`\\\\server\\share`, `\\\\?\\`) outside those roots is refused before the
/// filesystem is touched. Either way the deepest existing ancestor, symlinks resolved, must stay under
/// one of the roots; a golden image path is denied.
///
/// # Errors
/// `path_outside_project` (`F0002`) with `data.path_reason` naming the broken rule, or the golden
/// denial.
pub fn document_path_in(root: &Path, key: &str, path: &str) -> Result<PathBuf, RpcError> {
    let roots = [root.to_path_buf(), std::env::temp_dir()];
    let resolved = regia_core::project_path::resolve_in_roots(&roots, path).map_err(|e| {
        let mut error = refused(
            key,
            e.code(),
            &format!(
                "must be relative to the project, or an absolute path under the project or the temp \
                 directory, without `..` or a symlink leading out: {e}"
            ),
        );
        name_allowed_roots(&mut error, &["project", "temp_dir"]);
        error
    })?;
    if crate::golden_tools::under_goldens(root, &resolved) {
        return Err(crate::golden_tools::golden_path_denied(path));
    }
    Ok(resolved)
}

/// [`document_path_in`] with [`project_root`].
///
/// # Errors
/// `path_outside_project` (see [`document_path_in`]).
pub fn document_path(world: &World, key: &str, path: &str) -> Result<PathBuf, RpcError> {
    document_path_in(&project_root(world), key, path)
}

/// Most bytes an agent method reads from a project file into memory in one call.
pub const MAX_READ_BYTES: u64 = 256 << 20;

/// Reads a regular file of at most `max` bytes. A non-regular file (a FIFO or device, which could
/// block the main loop forever) or one above `max` is an `InvalidInput` error; nothing is read.
///
/// # Errors
///
/// An I/O error from the metadata or the read, or `InvalidInput` as described.
pub fn read_file_capped(file: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let meta = std::fs::metadata(file)?;
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    if meta.len() > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("file is {} bytes, above the {max}-byte limit", meta.len()),
        ));
    }
    let mut data = Vec::new();
    std::fs::File::open(file)?.take(max + 1).read_to_end(&mut data)?;
    if data.len() as u64 > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file grew past the limit",
        ));
    }
    Ok(data)
}

/// Creates the temp file `tmp` of an atomic write and fills it with `bytes`, never following a link.
///
/// ADR-10066: temp names beside a project file are predictable (`x.ron.new`, `.x.<pid>.tmp`), so a
/// cloned project can already hold a symlink there; `std::fs::write` would follow it and put the
/// bytes outside the project while the call reports success. Like `audio.loopgen_render`
/// (ADR-3206), a leftover entry at `tmp` is unlinked (a symlink itself, never its target) and the
/// file is opened with `create_new`, which refuses any entry that reappears in between.
///
/// # Errors
///
/// An I/O error from the unlink (other than not found), the exclusive create or the write.
pub(crate) fn write_new_temp(tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    match std::fs::remove_file(tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)?;
    file.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-10066: a symlink planted at the temp name is replaced, not written through; a stale
    /// regular file is replaced too; a directory there is an error, not a write elsewhere.
    #[cfg(unix)]
    #[test]
    fn write_new_temp_never_writes_through_a_planted_link() {
        let base = regia_test_utils::unique_temp_dir("req-path-temp-link");
        let project = base.join("project");
        let outside = base.join("outside");
        std::fs::create_dir_all(&project).expect("project");
        std::fs::create_dir_all(&outside).expect("outside");
        let victim = outside.join("victim.ron");
        let tmp = project.join("a.ron.new");
        std::os::unix::fs::symlink(&victim, &tmp).expect("link");
        write_new_temp(&tmp, b"new").expect("writes");
        assert!(!victim.exists(), "the link target was created");
        assert!(
            !std::fs::symlink_metadata(&tmp)
                .expect("tmp")
                .file_type()
                .is_symlink(),
            "the temp is a regular file now"
        );
        assert_eq!(std::fs::read(&tmp).expect("read"), b"new");

        // A link to an existing outside file: the file keeps its bytes.
        std::fs::write(&victim, "keep").expect("victim");
        std::fs::remove_file(&tmp).expect("rm");
        std::os::unix::fs::symlink(&victim, &tmp).expect("link 2");
        write_new_temp(&tmp, b"again").expect("writes again");
        assert_eq!(std::fs::read(&victim).expect("victim"), b"keep");
        assert_eq!(std::fs::read(&tmp).expect("read"), b"again");

        // A stale regular temp is replaced; a directory is an error.
        write_new_temp(&tmp, b"third").expect("stale temp");
        assert_eq!(std::fs::read(&tmp).expect("read"), b"third");
        let dir_tmp = project.join("d.ron.new");
        std::fs::create_dir_all(&dir_tmp).expect("dir");
        assert!(write_new_temp(&dir_tmp, b"x").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Audit ADR-3206 finding 11: the read helpers had their own component check, so on Linux `..\\x`,
    /// `C:\\x` and `NUL` looked like plain file names, and a project written here broke on Windows.
    /// They now share the rule; a FIFO cannot block the world thread.
    #[test]
    fn the_read_helper_keeps_the_shared_rule_and_refuses_a_pipe() {
        let root = regia_test_utils::unique_temp_dir("req-path-read");
        std::fs::write(root.join("a.ron"), "x").expect("file");
        assert_eq!(
            project_file_in(&root, "path", "a.ron").expect("a file"),
            root.join("a.ron")
        );
        for (input, why) in [
            ("..\\x", "path_parent"),
            ("a\\..\\..\\b", "path_parent"),
            ("C:\\x", "path_prefix"),
            ("C:x", "path_prefix"),
            ("\\\\server\\share", "path_absolute"),
            ("NUL", "path_device"),
            ("con.txt", "path_device"),
            ("a.ron.", "path_trailing"),
            ("a.ron::$DATA", "path_prefix"),
        ] {
            let refused = project_file_in(&root, "path", input).expect_err(input);
            assert_eq!(reason(&refused).1, why, "{input:?}");
        }
        // A missing file is "not found", which is not a refusal of the path. A folder passes: a read
        // of one fails at once, and the handler has its own code for that.
        let error = project_file_in(&root, "path", "nope.ron").expect_err("missing");
        assert_eq!(error.code, crate::protocol::NOT_FOUND);
        std::fs::create_dir_all(root.join("dir")).expect("dir");
        assert_eq!(
            project_file_in(&root, "path", "dir").expect("a folder"),
            root.join("dir")
        );
        #[cfg(unix)]
        {
            let fifo = root.join("pipe.ron");
            let made = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .is_ok_and(|s| s.success());
            if made {
                let error = project_file_in(&root, "path", "pipe.ron").expect_err("a pipe");
                assert_eq!(error.code, crate::protocol::NOT_FOUND);
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Audit ADR-3206 finding 10: a cloned `agent.toml` could list the whole filesystem as an output
    /// root, and a `\\\\server\\share` root was resolved, which connects to it.
    #[test]
    fn output_roots_cannot_widen_to_a_whole_filesystem() {
        let root = regia_test_utils::unique_temp_dir("req-path-wide");
        let outside = regia_test_utils::unique_temp_dir("req-path-wide-outside");
        let mut world = World::new();
        world.insert_resource(crate::AgentPermissions {
            config: crate::AgentConfig {
                output_roots: vec![PathBuf::from("/"), PathBuf::from("//evil/share")],
                ..crate::AgentConfig::default()
            },
            source: None,
        });
        // The temp dir stays allowed, so name a path outside all of them.
        let target = if cfg!(windows) {
            "C:\\Windows\\x"
        } else {
            "/etc/regia-threat-model"
        };
        let refused = allowed_path_in(&world, &root, "out", target).expect_err("not under an allowed root");
        assert_eq!(reason(&refused).0, "path_outside_project");
        // The render-out rule, which adds the temp dir on its own.
        assert!(
            crate::render_out::resolve_under_roots(
                &root,
                &[PathBuf::from("/"), PathBuf::from("//evil/share")],
                Path::new(target),
            )
            .is_err()
        );
        for dir in [root, outside] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn reason(e: &RpcError) -> (String, String) {
        let d = e.data.as_ref().expect("data");
        (
            d["reason"].as_str().unwrap_or_default().to_string(),
            d["path_reason"].as_str().unwrap_or_default().to_string(),
        )
    }

    #[test]
    fn request_paths_stay_in_the_project_or_the_allowed_roots() {
        let base = regia_test_utils::unique_temp_dir("req-path");
        let root = base.join("project");
        std::fs::create_dir_all(&root).expect("root");
        let world = World::new();
        assert_eq!(
            project_path_in(&root, "path", "a/b.ron").expect("rel"),
            root.join("a/b.ron")
        );
        for (p, why) in [
            ("/etc/passwd", "path_absolute"),
            ("C:\\x", "path_prefix"),
            ("\\\\server\\share", "path_absolute"),
            ("../x", "path_parent"),
        ] {
            let e = project_path_in(&root, "path", p).expect_err(p);
            assert_eq!(reason(&e), ("path_outside_project".into(), why.into()), "{p}");
        }
        // The allowed roots take an absolute path under the project or the temp dir.
        let inside = root.join("out/x.png").display().to_string();
        assert!(allowed_path_in(&world, &root, "out", &inside).is_ok());
        assert!(allowed_path_in(&world, &root, "out", "renders/x.png").is_ok());
        let e = allowed_path_in(&world, &root, "out", "C:x").expect_err("drive-relative");
        assert_eq!(reason(&e).1, "path_prefix");
        #[cfg(unix)]
        {
            let e = allowed_path_in(&world, &root, "out", "/etc/passwd").expect_err("etc");
            assert_eq!(reason(&e).1, "path_escapes");
            std::os::unix::fs::symlink("/", root.join("sys")).expect("link");
            let e = project_path_in(&root, "path", "sys/etc/passwd").expect_err("link");
            assert_eq!(reason(&e).1, "path_escapes");
            let e = allowed_path_in(&world, &root, "out", "sys/etc/x").expect_err("link");
            assert_eq!(reason(&e).1, "path_escapes");
            assert!(check_derived(&root, "path", &root.join("sys/etc")).is_err());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// ADR-7388 (audit rc1-sec-paths-z): the document rule every editor tool path goes through.
    /// Windows-style strings run on every host.
    #[test]
    fn document_paths_refuse_every_escape_class() {
        let base = regia_test_utils::unique_temp_dir("req-path-doc");
        let root = base.join("project");
        std::fs::create_dir_all(root.join("assets")).expect("root");
        assert_eq!(
            document_path_in(&root, "path", "assets/a.ui.ron").expect("relative"),
            root.join("assets/a.ui.ron")
        );
        assert_eq!(
            document_path_in(&root, "path", "assets\\b.ui.ron").expect("backslash relative"),
            root.join("assets").join("b.ui.ron")
        );
        let inside = root.join("assets/c.ui.ron").display().to_string();
        assert!(
            document_path_in(&root, "path", &inside).is_ok(),
            "absolute in the project"
        );
        let temp = base.join("scratch.ui.ron").display().to_string();
        assert!(
            document_path_in(&root, "path", &temp).is_ok(),
            "absolute in the temp dir"
        );
        for (input, why) in [
            ("../x.ui.ron", "path_parent"),
            ("a/../../x.ui.ron", "path_parent"),
            ("a\\..\\..\\x.ui.ron", "path_parent"),
            ("C:\\Users\\x.ui.ron", "path_prefix"),
            ("C:x.ui.ron", "path_prefix"),
            ("\\\\server\\share\\x.ui.ron", "path_absolute"),
            ("\\\\?\\C:\\x.ui.ron", "path_absolute"),
            ("\\Windows\\x.ui.ron", "path_absolute"),
            ("a\0b.ui.ron", "path_nul"),
            ("nul.ui.ron", "path_device"),
            ("", "path_empty"),
        ] {
            // ADR-7566: three rows are absolute paths on Windows, under no root there: a drive path
            // is outside every root, and a UNC or verbatim form is refused before the filesystem
            // is touched.
            #[cfg(windows)]
            let why = match input {
                "C:\\Users\\x.ui.ron" => "path_escapes",
                "\\\\server\\share\\x.ui.ron" | "\\\\?\\C:\\x.ui.ron" => "path_remote",
                _ => why,
            };
            let e = document_path_in(&root, "path", input).expect_err(input);
            assert_eq!(
                reason(&e),
                ("path_outside_project".into(), why.into()),
                "{input:?}"
            );
        }
        #[cfg(unix)]
        {
            let e = document_path_in(&root, "path", "//server/share/x.ui.ron").expect_err("unc");
            assert_eq!(reason(&e).1, "path_remote");
            let e = document_path_in(&root, "path", "/etc/passwd").expect_err("etc");
            assert_eq!(reason(&e).1, "path_escapes");
            let e = document_path_in(&root, "path", &format!("{}/../x", root.display())).expect_err("abs ..");
            assert_eq!(reason(&e).1, "path_parent");
            std::os::unix::fs::symlink("/", root.join("sys")).expect("link");
            let e = document_path_in(&root, "path", "sys/etc/x.ui.ron").expect_err("link");
            assert_eq!(reason(&e).1, "path_escapes");
            // Only explicitly configured `output_roots` widen the output rule.
            let cwd = std::env::current_dir().expect("cwd");
            if !cwd.starts_with(std::env::temp_dir()) && !cwd.starts_with(&root) {
                let manifest = cwd.join("Cargo.toml").display().to_string();
                assert!(allowed_path_in(&World::new(), &root, "path", &manifest).is_err());
                let e = document_path_in(&root, "path", &manifest).expect_err("cwd");
                assert_eq!(reason(&e).1, "path_escapes");
            }
        }
        #[cfg(windows)]
        {
            let e = document_path_in(&root, "path", "C:\\Windows\\win.ini").expect_err("drive");
            assert_eq!(reason(&e).0, "path_outside_project");
            let e = document_path_in(&root, "path", "\\\\server\\share\\x").expect_err("unc");
            assert_eq!(reason(&e).0, "path_outside_project");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn read_file_capped_refuses_big_and_non_regular_files() {
        let dir = regia_test_utils::unique_temp_dir("req_path_cap");
        let f = dir.join("a.bin");
        std::fs::write(&f, [1u8; 10]).expect("write");
        assert_eq!(super::read_file_capped(&f, 10).expect("fits").len(), 10);
        let err = super::read_file_capped(&f, 9).expect_err("over cap");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err = super::read_file_capped(&dir, 100).expect_err("directory");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
