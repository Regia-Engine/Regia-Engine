// SPDX-License-Identifier: MPL-2.0
//! Where a running app advertises its agent server: `<project>/.regia/agent/endpoint.json` plus a
//! bearer token in `<project>/.regia/agent/token` (mode 0600 on Unix).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::protocol::Endpoint;

/// Directory holding `endpoint.json` and `token` for a project. `REGIA_AGENT_DIR`, when set and
/// not empty, replaces the project location so a session harness can give every process of a
/// playthrough its own endpoint (`docs/specs/41` § 2.8).
pub fn agent_dir(project_dir: &Path) -> PathBuf {
    agent_dir_with(project_dir, regia_core::brand::env_var_os("REGIA_AGENT_DIR"))
}

/// [`agent_dir`] with the override passed in (the environment read is the only difference).
fn agent_dir_with(project_dir: &Path, override_dir: Option<std::ffi::OsString>) -> PathBuf {
    match override_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => regia_core::brand::project_state_dir(project_dir).join("agent"),
    }
}

/// Finds the project directory: an explicit setting, else `REGIA_PROJECT_DIR`, else the nearest
/// ancestor of the current directory that contains `Regia.toml`, else `None` (the caller then uses
/// the per-user data directory).
pub fn resolve_project_dir(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = explicit {
        return Some(dir.to_path_buf());
    }
    if let Some(dir) = regia_core::brand::env_var_os("REGIA_PROJECT_DIR") {
        return Some(PathBuf::from(dir));
    }
    let cwd = std::env::current_dir().ok()?;
    cwd.ancestors()
        .find(|d| regia_core::brand::is_project_root(d))
        .map(Path::to_path_buf)
}

/// A random 256-bit token as 64 hex characters.
pub fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Writes the token (owner-only permissions) and the endpoint file atomically.
///
/// Each file is written to a fresh temp file (created exclusively, so a planted file or symlink
/// at the temp name is refused rather than followed) and renamed into place: a symlink planted
/// at `token` or `endpoint.json` is replaced, never written through (audit-agent-sec Medium:
/// the old `open(truncate)` truncated the link's target and chmodded it 0600). The agent
/// directory, when this creates it, is owner-only on Unix. A linked agent directory is refused
/// ([`prepare_dir`]).
pub fn write(dir: &Path, endpoint: &Endpoint, token: &str) -> std::io::Result<()> {
    prepare_dir(dir)?;
    write_owner_only(dir, "token", token.as_bytes())?;
    write_owner_only(
        dir,
        "endpoint.json",
        &serde_json::to_vec_pretty(endpoint).map_err(std::io::Error::other)?,
    )
}

/// Whether `dir`, or the project state folder that holds it, is a symbolic link (a Windows junction
/// counts). A project is often a cloned repository, and git stores links: `.regia/agent -> ~/.config/x`
/// would put the token and the audit log in the link's target, with the renames below replacing
/// files there. A link above the state folder is the user's own layout and is not looked at.
fn linked_agent_dir(dir: &Path) -> bool {
    let is_link = |path: &Path| {
        path.symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
    };
    is_link(dir)
        || dir.parent().is_some_and(|parent| {
            parent.file_name().is_some_and(|name| {
                name == regia_core::brand::PROJECT_STATE_DIR
                    || name == regia_core::brand::LEGACY_PROJECT_STATE_DIR
            }) && is_link(parent)
        })
}

fn refuse_linked_agent_dir(dir: &Path) -> std::io::Result<()> {
    if linked_agent_dir(dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{}: the agent folder must not be a symbolic link", dir.display()),
        ));
    }
    Ok(())
}

/// Creates the agent directory (owner-only on Unix, its parents keep the umask), refusing one that
/// is, or lies in a state folder that is, a symbolic link. Nothing is created through a refused
/// link. Every writer of the folder calls this first, so the mode holds whichever one runs first.
///
/// # Errors
/// [`std::io::ErrorKind::InvalidInput`] for a link, or the IO error of creating the folders.
pub fn prepare_dir(dir: &Path) -> std::io::Result<()> {
    refuse_linked_agent_dir(dir)?;
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    match builder.create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Creates `path` exclusively (it must not exist) for writing, readable by its owner only: mode
/// 0600 on Unix; on Windows a DACL naming only this account, `SYSTEM` and `Administrators`, set at
/// creation so the file never carries its folder's inherited entries (`BUILTIN\Users`,
/// `Authenticated Users`; ADR-5489). Other targets create a plain file.
pub(crate) fn create_owner_only(path: &Path) -> std::io::Result<fs::File> {
    #[cfg(windows)]
    {
        regia_net::private_file::create_private(path)
    }
    #[cfg(not(windows))]
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    }
}

/// Writes `dir/name` through `dir/name.<pid>.tmp` (created exclusively and owner-only: mode 0600
/// on Unix, an owner-only DACL on Windows) and a rename, which replaces whatever `name` was (a
/// symlink included) instead of following it. The rename keeps the temp file's DACL on Windows.
///
/// Public for other owner-only secrets of `.regia/agent/` (the `regia mcp --http` token, ADR-2346).
///
/// # Errors
/// IO errors creating, writing or renaming the file.
pub fn write_owner_only(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    write_owner_only_os(dir, std::ffi::OsStr::new(name), bytes)
}

/// [`write_owner_only`] for a file name that is not guaranteed to be UTF-8 (a recording
/// path taken from the command line, for example). The temporary sibling is built as an
/// [`std::ffi::OsString`], so non-UTF-8 names round-trip unchanged.
///
/// # Errors
/// IO errors creating, writing or renaming the file.
pub fn write_owner_only_os(dir: &Path, name: &std::ffi::OsStr, bytes: &[u8]) -> std::io::Result<()> {
    refuse_linked_agent_dir(dir)?;
    write_replacing(dir, name, bytes)
}

/// The temp-file-and-rename write of [`write_owner_only_os`] without its agent-folder link check,
/// for files a user places anywhere (a DCC feed recording, ADR-5597): a user's folder may well be,
/// or sit under, a symbolic link. A link planted at the temporary name is still replaced, never
/// followed.
///
/// # Errors
/// IO errors creating, writing or renaming the file.
pub(crate) fn write_replacing(dir: &Path, name: &std::ffi::OsStr, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = dir.join(tmp_name);
    // A leftover from a crashed run (or a planted link) is removed, not opened: `remove_file`
    // unlinks a symlink itself.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = create_owner_only(&tmp)?;
    let written = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Through the descriptor: the umask may have cleared bits of the creation mode.
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.sync_all()
    })();
    drop(file);
    let renamed = written.and_then(|()| fs::rename(&tmp, dir.join(name)));
    if renamed.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    renamed
}

/// Whether `url` is a plain `ws://` URL to the loopback address `127.0.0.1` (the only host the
/// server binds, and the only one a client sends the token to).
///
/// The authority must be exactly `127.0.0.1` with an optional all-digit port: a prefix check let
/// `ws://127.0.0.1:80@evil.example` through, whose host is `evil.example` for a generic client.
fn is_loopback_ws(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("ws://127.0.0.1") else {
        return false;
    };
    let path = match rest.strip_prefix(':') {
        Some(port_and_path) => {
            let end = port_and_path.find('/').unwrap_or(port_and_path.len());
            let port = &port_and_path[..end];
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            &port_and_path[end..]
        }
        None => rest,
    };
    path.is_empty() || path.starts_with('/')
}

/// Reads `endpoint.json` and the token from an agent directory.
///
/// The token is always `<dir>/token`: the `token_file` field of `endpoint.json` is informational and
/// never followed, so a replaced `endpoint.json` cannot make a client read another file. A `url`
/// that is not `ws://127.0.0.1…` is refused for the same reason (the token would be sent there).
/// Both files must be regular files under their limits and the folder must not be a link: a planted
/// `token -> ~/.api-key` would otherwise send that key as the bearer token, and a link to a device
/// would never finish reading. The refusal never quotes what the file holds.
pub fn read(dir: &Path) -> std::io::Result<(Endpoint, String)> {
    refuse_linked_agent_dir(dir)?;
    let endpoint_bytes =
        regia_core::project_path::read_regular_capped(&dir.join("endpoint.json"), MAX_ENDPOINT_BYTES)?;
    let endpoint: Endpoint = serde_json::from_slice(&endpoint_bytes).map_err(std::io::Error::other)?;
    if !is_loopback_ws(&endpoint.url) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "endpoint.json: `url` must be ws://127.0.0.1:<port>",
        ));
    }
    let token = read_token(dir)?;
    Ok((endpoint, token))
}

/// Largest `endpoint.json` a client reads.
const MAX_ENDPOINT_BYTES: u64 = 64 * 1024;
/// Longest token a client reads, and more than any token this engine mints.
const MAX_TOKEN_BYTES: u64 = 256;

/// The token file: at most [`MAX_TOKEN_BYTES`] of `[A-Za-z0-9_-]` around optional whitespace.
fn read_token(dir: &Path) -> std::io::Result<String> {
    let bad = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: not an agent token", dir.join("token").display()),
        )
    };
    let bytes =
        regia_core::project_path::read_regular_capped(&dir.join("token"), MAX_TOKEN_BYTES).map_err(|e| {
            if e.kind() == std::io::ErrorKind::InvalidData {
                bad()
            } else {
                e
            }
        })?;
    let text = std::str::from_utf8(&bytes).map_err(|_| bad())?.trim();
    let plain = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if plain { Ok(text.to_string()) } else { Err(bad()) }
}

/// Removes the endpoint file when the app exits (the token stays; it is replaced on next
/// start). Also removes the local-transport advertisement when it names a listener this
/// process serves (spec 23 § 2, [`remove_socket_if_ours`]).
pub fn remove(dir: &Path) {
    let _ = fs::remove_file(dir.join("endpoint.json"));
    remove_socket_if_ours(dir);
}

/// Removes the local-transport advertisement file (`.regia/agent/socket`) only while it names
/// a listener this process still serves (`crate::local::start`, not dropped, and on Unix the
/// path still holds its socket). Another app's advertisement — a successor's, or a live app's
/// beside an app that fell back to WebSocket — stays (sprint-rev-kimi-9 finding 6, ADR-5878).
/// Idempotent. The socket/pipe itself goes with the app's listener.
pub fn remove_socket_if_ours(dir: &Path) {
    if let Ok(Some(address)) = read_socket(dir)
        && crate::local::serves(&address)
    {
        let _ = fs::remove_file(dir.join(SOCKET_FILE));
    }
}

/// Name of the local-transport advertisement inside the agent directory (spec 23 § 2): one
/// line, the absolute socket path (Unix) or the pipe name (Windows).
pub const SOCKET_FILE: &str = "socket";

/// Writes the local-transport address (owner-only, the way `token` is written) for clients to
/// find next to `endpoint.json`.
///
/// # Errors
///
/// [`std::io::ErrorKind::InvalidInput`] when `address` is not a well-formed local-transport
/// address (see `valid_socket_address`), or the IO error of the owner-only write.
pub fn write_socket(dir: &Path, address: &str) -> std::io::Result<()> {
    if !valid_socket_address(address, cfg!(windows)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a local transport address",
        ));
    }
    write_owner_only(dir, SOCKET_FILE, address.as_bytes())
}

/// Reads the local-transport advertisement. `Ok(None)` means no local transport is advertised
/// (an older server, or a platform without one); a present-but-invalid address is an error,
/// never silently skipped to the WebSocket fallback — a planted address must not choose where
/// a client sends the bearer token.
///
/// On Unix the address must name a socket directly inside `dir`, or (Linux/macOS) inside the
/// fallback directory a server of `dir` binds in when `<dir>/agent.sock` is too long for the
/// platform's socket path limit. Both sides are compared resolved: on macOS `/var` is a link to
/// `/private/var`, and a client that spelled the project one way while the app spelled it the
/// other refused every advertisement (ADR-7335).
///
/// # Errors
///
/// [`std::io::ErrorKind::InvalidData`] for a malformed address, [`std::io::ErrorKind::NotFound`]
/// when `dir` does not exist, or the IO error of reading the file.
pub fn read_socket(dir: &Path) -> std::io::Result<Option<String>> {
    let path = dir.join(SOCKET_FILE);
    let bytes = match regia_core::project_path::read_regular_capped(&path, MAX_SOCKET_BYTES) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let bad = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: not a local transport address", path.display()),
        )
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| bad())?;
    let address = text.trim();
    if !valid_socket_address(address, cfg!(windows)) {
        return Err(bad());
    }
    #[cfg(all(unix, not(target_os = "android")))]
    {
        let socket = Path::new(address);
        let parent = fs::canonicalize(socket.parent().ok_or_else(bad)?).map_err(|_| bad())?;
        let inside = fs::canonicalize(dir).is_ok_and(|dir| dir == parent);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let inside = inside
            || crate::platform::fallback_socket_dir(dir)
                .and_then(|fallback| fs::canonicalize(fallback).ok())
                .is_some_and(|fallback| fallback == parent);
        if !inside {
            return Err(bad());
        }
    }
    Ok(Some(address.to_string()))
}

/// The pipe name of a local-transport server: the server's process id and a per-process
/// serial, so two servers in one process never share a name (ADR-5878).
#[cfg(any(windows, test))]
pub(crate) fn pipe_name(pid: u32, serial: u32) -> String {
    format!(r"\\.\pipe\regia-agent-{pid}-{serial}")
}

/// The server process id a well-formed pipe name carries: `\\.\pipe\regia-agent-<pid>`, with
/// an optional `-<serial>`, both decimal `u32`s. The Windows client checks the pipe's real
/// server process against it before it sends the token (sprint-rev-kimi-9 finding 4).
pub(crate) fn pipe_server_pid(address: &str) -> Option<u32> {
    let rest = address.strip_prefix(r"\\.\pipe\regia-agent-")?;
    let (pid, serial) = match rest.split_once('-') {
        Some((pid, serial)) => (pid, Some(serial)),
        None => (rest, None),
    };
    let decimal = |digits: &str| {
        (!digits.is_empty() && digits.len() <= 10 && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse::<u32>().ok())
            .flatten()
    };
    if let Some(serial) = serial {
        decimal(serial)?;
    }
    decimal(pid)
}

/// Largest `socket` advertisement a client reads (a path or a pipe name is far below this).
const MAX_SOCKET_BYTES: u64 = 4096;

/// Whether `address` is a well-formed local-transport address: on Unix an absolute path with
/// no parent-directory component; on Windows the pipe name shape
/// `\\.\pipe\regia-agent-<pid>[-<serial>]` (see `pipe_server_pid`). `windows_pipe` selects
/// the shape so both are testable on any host (spec 23 § 2).
pub(crate) fn valid_socket_address(address: &str, windows_pipe: bool) -> bool {
    if windows_pipe {
        pipe_server_pid(address).is_some()
    } else {
        // By string, not `Path`: on Windows `/tmp/x.sock` is not absolute (ADR-7566).
        address.starts_with('/') && !address.split('/').any(|segment| segment == "..")
    }
}

/// Removes the endpoint file only while it still advertises `url` (this app's server).
///
/// During a restart overlap the successor may already have written its own `endpoint.json`;
/// an exiting app must not delete it (clients would find no app until the successor restarted;
/// audit-concurrency-1103 agent M4). An unreadable file is left alone too.
pub fn remove_if_advertising(dir: &Path, url: &str) {
    let path = dir.join("endpoint.json");
    let Ok(bytes) = fs::read(&path) else {
        return;
    };
    match serde_json::from_slice::<Endpoint>(&bytes) {
        Ok(endpoint) if endpoint.url == url => {
            let _ = fs::remove_file(&path);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    /// Audit ADR-3206 finding 3: a symbolic link at the agent folder itself. The file-level fix
    /// replaces a link at `token`; a link one level up put the token in the link's target.
    #[cfg(unix)]
    #[test]
    fn a_linked_agent_folder_or_state_folder_is_refused_and_nothing_lands_in_the_target() {
        let base = regia_test_utils::unique_temp_dir("endpoint-linked-dir");
        let target = base.join("victim");
        fs::create_dir_all(&target).expect("target");
        let endpoint = test_endpoint("ws://127.0.0.1:1", String::new());

        // `.regia/agent -> victim`.
        let project = base.join("one");
        fs::create_dir_all(project.join(regia_core::brand::PROJECT_STATE_DIR)).expect("state");
        let agent = project.join(regia_core::brand::PROJECT_STATE_DIR).join("agent");
        std::os::unix::fs::symlink(&target, &agent).expect("link");
        let refused = write(&agent, &endpoint, "tok").expect_err("linked agent folder");
        assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            prepare_dir(&agent).map_err(|e| e.kind()),
            Err(std::io::ErrorKind::InvalidInput)
        );
        assert_eq!(
            write_owner_only(&agent, "mcp_http_token", b"x").map_err(|e| e.kind()),
            Err(std::io::ErrorKind::InvalidInput)
        );
        assert_eq!(
            read(&agent).map_err(|e| e.kind()).err(),
            Some(std::io::ErrorKind::InvalidInput)
        );

        // `.regia -> victim`, so `.regia/agent` would be `victim/agent`.
        let project = base.join("two");
        fs::create_dir_all(&project).expect("project");
        let state = project.join(regia_core::brand::PROJECT_STATE_DIR);
        std::os::unix::fs::symlink(&target, &state).expect("state link");
        assert_eq!(
            write(&state.join("agent"), &endpoint, "tok").map_err(|e| e.kind()),
            Err(std::io::ErrorKind::InvalidInput)
        );
        assert_eq!(fs::read_dir(&target).expect("target").count(), 0);
        let _ = fs::remove_dir_all(&base);
    }

    /// Audit ADR-3206 finding 4: the folder was created elsewhere first (the plugin does, for the
    /// audit log), so the owner-only mode was never applied. It now comes from one function.
    #[cfg(unix)]
    #[test]
    fn prepare_dir_makes_a_new_folder_owner_only_and_keeps_an_existing_one() {
        use std::os::unix::fs::PermissionsExt;
        let base = regia_test_utils::unique_temp_dir("endpoint-prepare");
        let dir = base.join("state").join("agent");
        prepare_dir(&dir).expect("creates");
        let mode = fs::metadata(&dir).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{mode:o}");
        prepare_dir(&dir).expect("already there");
        let _ = fs::remove_dir_all(&base);
    }

    /// Audit ADR-3206 finding 5: the client read `token` through a link with no limit, and sent what
    /// it found as a bearer token. Only a small regular file of token characters is a token.
    #[test]
    fn only_a_plain_regular_token_file_is_read_and_a_refusal_never_quotes_it() {
        let dir = regia_test_utils::unique_temp_dir("endpoint-token-file");
        write(
            &dir,
            &test_endpoint("ws://127.0.0.1:1", String::new()),
            "abcDEF012_-",
        )
        .expect("write");
        assert_eq!(read(&dir).expect("plain").1, "abcDEF012_-");
        fs::write(dir.join("token"), "abcDEF012_-\n").expect("newline");
        assert_eq!(read(&dir).expect("trimmed").1, "abcDEF012_-");
        let long = "a".repeat(257);
        for bad in [
            "",
            "   \n",
            "sk-live key",
            "line1\nline2",
            "pa$$word",
            long.as_str(),
        ] {
            fs::write(dir.join("token"), bad).expect("token");
            let err = read(&dir).expect_err("refused").to_string();
            assert!(err.contains("not an agent token"), "{bad:?}: {err}");
            let held = bad.trim();
            assert!(held.is_empty() || !err.contains(held), "{bad:?}: {err}");
        }
        fs::write(dir.join("token"), [0xff_u8, 0xfe]).expect("binary");
        assert!(read(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_token_or_endpoint_is_not_read() {
        let dir = regia_test_utils::unique_temp_dir("endpoint-token-link");
        let secret = dir.join("api-key");
        fs::write(&secret, "skliveabcdef").expect("secret");
        write(&dir, &test_endpoint("ws://127.0.0.1:1", String::new()), "tok").expect("write");
        fs::remove_file(dir.join("token")).expect("rm");
        std::os::unix::fs::symlink(&secret, dir.join("token")).expect("link");
        let err = read(&dir).expect_err("a link is not a token").to_string();
        assert!(!err.contains("skliveabcdef"), "{err}");
        fs::remove_file(dir.join("token")).expect("rm");
        write(&dir, &test_endpoint("ws://127.0.0.1:1", String::new()), "tok").expect("write");
        let real = dir.join("real.json");
        fs::rename(dir.join("endpoint.json"), &real).expect("mv");
        std::os::unix::fs::symlink(&real, dir.join("endpoint.json")).expect("link");
        assert!(read(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn read_rejects_a_token_file_outside_the_agent_dir() {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "regia-agent-escape-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let dir = root.join("agent");
        fs::create_dir_all(&dir).expect("dir");
        let secret = root.join("secret.txt");
        fs::write(&secret, "SECRET").expect("secret");
        let endpoint = Endpoint {
            url: "ws://127.0.0.1:1".into(),
            pid: 1,
            protocol: "0.1".into(),
            token_file: secret.display().to_string(),
            app: "test".into(),
        };
        fs::write(
            dir.join("endpoint.json"),
            serde_json::to_vec(&endpoint).expect("json"),
        )
        .expect("endpoint");
        let err = read(&dir).expect_err("outside the agent dir");
        assert!(!err.to_string().contains("SECRET"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_then_read() {
        let dir = std::env::temp_dir().join(format!("regia-agent-endpoint-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let token = new_token();
        assert_eq!(token.len(), 64);
        let endpoint = Endpoint {
            url: "ws://127.0.0.1:1".into(),
            pid: 1,
            protocol: "0.1".into(),
            token_file: dir.join("token").display().to_string(),
            app: "test".into(),
        };
        write(&dir, &endpoint, &token).expect("written");
        let (back, back_token) = read(&dir).expect("read");
        assert_eq!(back, endpoint);
        assert_eq!(back_token, token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join("token"))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        remove(&dir);
        assert!(read(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A fresh directory per call (pid + counter), never shared between parallel tests.
    fn unique_dir(tag: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("regia-{tag}-{}-{n}", std::process::id()))
    }

    fn test_endpoint(url: &str, token_file: String) -> Endpoint {
        Endpoint {
            url: url.into(),
            pid: 1,
            protocol: "0.1".into(),
            token_file,
            app: "test".into(),
        }
    }

    #[test]
    fn a_forged_token_file_is_never_followed() {
        let dir = unique_dir("agent-endpoint-forged");
        let secret = dir.join("secret.txt");
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(&secret, "do-not-send").expect("secret");
        let endpoint = test_endpoint("ws://127.0.0.1:1", secret.display().to_string());
        write(&dir, &endpoint, "real-token").expect("written");
        let (_, token) = read(&dir).expect("read");
        assert_eq!(token, "real-token");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_loopback_urls_are_refused() {
        for url in [
            "ws://evil.example:80",
            "ws://127.0.0.1.evil.example:80",
            "ws://127.0.0.10:1",
            "wss://127.0.0.1:1",
            "http://127.0.0.1:1",
            "ws://localhost:1",
            // audit-agent-sec (Low): userinfo makes the host the part after `@`.
            "ws://127.0.0.1:80@evil.example",
            "ws://127.0.0.1:@evil.example/",
            "ws://127.0.0.1:",
            "ws://127.0.0.1:80?x=@evil.example",
            "ws://127.0.0.1@evil.example",
        ] {
            let dir = unique_dir("agent-endpoint-url");
            write(&dir, &test_endpoint(url, String::new()), "t").expect("written");
            let err = read(&dir).expect_err(url);
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{url}");
            let _ = fs::remove_dir_all(&dir);
        }
        assert!(is_loopback_ws("ws://127.0.0.1:35871"));
        assert!(is_loopback_ws("ws://127.0.0.1:35871/"));
        assert!(is_loopback_ws("ws://127.0.0.1"));
        assert!(is_loopback_ws("ws://127.0.0.1/agent@x"));
    }

    /// audit-agent-sec (Medium): `.regia/agent/token` planted as a symlink made startup truncate
    /// the link's target (and chmod it 0600). The write now replaces the link instead.
    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_at_the_token_or_endpoint_is_replaced_not_followed() {
        use std::os::unix::fs::PermissionsExt;
        let base = unique_dir("agent-endpoint-symlink");
        let dir = base.join("agent");
        fs::create_dir_all(&dir).expect("mkdir");
        let victim = base.join("authorized_keys");
        fs::write(&victim, "ssh-ed25519 KEEP").expect("victim");
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).expect("chmod");
        let other = base.join("other.json");
        fs::write(&other, "KEEP TOO").expect("other");
        std::os::unix::fs::symlink(&victim, dir.join("token")).expect("token link");
        std::os::unix::fs::symlink(&other, dir.join("endpoint.json")).expect("endpoint link");
        // A planted file at the temp name is removed, not written through.
        std::os::unix::fs::symlink(&victim, dir.join(format!("token.{}.tmp", std::process::id())))
            .expect("tmp link");
        write(
            &dir,
            &test_endpoint("ws://127.0.0.1:1", String::new()),
            "fresh-token",
        )
        .expect("written");
        assert_eq!(fs::read_to_string(&victim).expect("victim"), "ssh-ed25519 KEEP");
        assert_eq!(
            fs::metadata(&victim).expect("meta").permissions().mode() & 0o777,
            0o644,
            "the target's mode is untouched"
        );
        assert_eq!(fs::read_to_string(&other).expect("other"), "KEEP TOO");
        assert!(
            !fs::symlink_metadata(dir.join("token"))
                .expect("token")
                .file_type()
                .is_symlink()
        );
        let (_, token) = read(&dir).expect("read");
        assert_eq!(token, "fresh-token");
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("list")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_agent_dir_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let base = unique_dir("agent-endpoint-dirmode");
        let dir = base.join("agent");
        write(&dir, &test_endpoint("ws://127.0.0.1:1", String::new()), "t").expect("written");
        let mode = fs::metadata(&dir).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_json_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_dir("agent-endpoint-mode");
        write(&dir, &test_endpoint("ws://127.0.0.1:1", String::new()), "t").expect("written");
        let mode = fs::metadata(dir.join("endpoint.json"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    /// ADR-5616 (review sprint-rev-v05-windows-close finding 9): on Windows the token and
    /// `endpoint.json` were left with the folder's inherited ACL; they now come from the shared
    /// owner-only helper, whose DACL names no one but this account, `SYSTEM` and `Administrators`.
    #[cfg(windows)]
    #[test]
    fn endpoint_files_are_owner_only_on_windows() {
        let dir = unique_dir("agent-endpoint-dacl");
        write(&dir, &test_endpoint("ws://127.0.0.1:1", String::new()), "t").expect("written");
        for name in ["token", "endpoint.json"] {
            let path = dir.join(name);
            let meta = fs::metadata(&path).expect("meta");
            assert!(
                !regia_net::private_file::exposed_to_other_users(&path, &meta).expect("dacl read"),
                "{name} is owner-only"
            );
        }
        let (_, token) = read(&dir).expect("this account reads its own files back");
        assert_eq!(token, "t");
        let _ = fs::remove_dir_all(&dir);
    }

    /// M5, 2026-11-12 (ADR-7335): macOS's `/var` is a link to `/private/var`. The app wrote its
    /// socket address under one spelling and `regia describe --live` read the project under the
    /// other, so the lexical "inside the agent dir" check refused a valid advertisement
    /// (`not a local transport address`). The check compares resolved paths: one directory under
    /// two spellings is accepted, a socket in another real directory is still refused.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_socket_advertisement_matches_the_agent_dir_under_any_spelling() {
        let base = regia_test_utils::unique_temp_dir("endpoint-spelling");
        let real = base.join("real");
        let agent = real.join("agent");
        prepare_dir(&agent).expect("agent dir");
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("a second spelling of the same directory");
        let address = agent.join("agent.sock");
        write_socket(&agent, address.to_str().expect("utf-8")).expect("advertised");
        assert_eq!(
            read_socket(&link.join("agent")).expect("the same directory, spelled through the link"),
            Some(address.display().to_string())
        );
        // A socket in a sibling directory is not inside the agent directory, however spelled.
        let elsewhere = base.join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("sibling");
        let outside = elsewhere.join("agent.sock");
        write_socket(&agent, outside.to_str().expect("utf-8")).expect("written");
        let err = read_socket(&link.join("agent")).expect_err("outside the agent dir");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let _ = fs::remove_dir_all(&base);
    }

    /// spec 23 § 2: both platform shapes of the `socket` advertisement are validated on any
    /// host, and a refusal never silently falls back to WebSocket.
    #[test]
    fn socket_addresses_validate_per_platform_shape() {
        // Unix: absolute, no parent-dir escapes.
        for good in ["/tmp/regia/agent.sock", "/x/.regia/agent/agent.sock"] {
            assert!(valid_socket_address(good, false), "{good}");
        }
        for bad in [
            "relative/agent.sock",
            "/tmp/../agent.sock",
            "/tmp/x/../../agent.sock",
            "",
            "agent.sock",
        ] {
            assert!(!valid_socket_address(bad, false), "{bad}");
        }
        // Windows: the engine's own pipe name only.
        assert!(valid_socket_address(r"\\.\pipe\regia-agent-1234", true));
        assert!(valid_socket_address(r"\\.\pipe\regia-agent-1234-0", true));
        for bad in [
            r"\\.\pipe\regia-agent-",
            r"\\.\pipe\other-1234",
            r"C:\temp\agent.sock",
            r"\\.\pipe\regia-agent-12x4",
            r"\\.\pipe\regia-agent-1234\..\x",
            r"\\.\pipe\regia-agent-1234-",
            r"\\.\pipe\regia-agent-1234-5-6",
            r"\\.\pipe\regia-agent-1234-x",
            r"\\.\pipe\regia-agent-99999999999",
            r"\\.\pipe\regia-agent-4294967296",
        ] {
            assert!(!valid_socket_address(bad, true), "{bad}");
        }
    }

    /// sprint-rev-kimi-9 finding 4: the pid the Windows client checks the pipe's server
    /// against comes from the name, and every name a server makes parses back to its pid.
    #[test]
    fn pipe_names_carry_the_server_pid() {
        assert_eq!(pipe_server_pid(r"\\.\pipe\regia-agent-1234"), Some(1234));
        assert_eq!(pipe_server_pid(r"\\.\pipe\regia-agent-1234-7"), Some(1234));
        for (pid, serial) in [(0, 0), (1, 1), (4242, 17), (u32::MAX, u32::MAX)] {
            let name = pipe_name(pid, serial);
            assert_eq!(pipe_server_pid(&name), Some(pid), "{name}");
            assert!(valid_socket_address(&name, true), "{name}");
        }
        assert_ne!(
            pipe_name(7, 0),
            pipe_name(7, 1),
            "two servers in one process differ"
        );
        assert_eq!(pipe_server_pid(r"\\.\pipe\regia-agent-"), None);
    }

    #[test]
    fn explicit_project_dir_wins() {
        assert_eq!(
            resolve_project_dir(Some(Path::new("/x"))),
            Some(PathBuf::from("/x"))
        );
    }

    #[test]
    fn agent_dir_override_replaces_the_project_location() {
        let project = Path::new("/p");
        let default = agent_dir_with(project, None);
        assert_eq!(
            default,
            regia_core::brand::project_state_dir(project).join("agent")
        );
        assert_eq!(
            agent_dir_with(project, Some("".into())),
            default,
            "an empty override is ignored"
        );
        assert_eq!(
            agent_dir_with(project, Some("/sessions/a/server-0/agent".into())),
            PathBuf::from("/sessions/a/server-0/agent")
        );
    }
}
