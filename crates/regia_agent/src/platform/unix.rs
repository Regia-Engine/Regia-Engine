// SPDX-License-Identifier: MPL-2.0
//! The Unix domain socket of the agent's local transport (spec 23 § 2): the listener thread,
//! the owner-only socket file, and the client stream. Shared by Linux and macOS; selected by
//! `platform/mod.rs` (`docs/04-conventions.md` § 8).

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use crossbeam_channel::bounded;

use crate::local::{self, LocalServer};
use crate::render_out::RenderOutBoard;
use crate::server::{self, ConnectionSlot, INBOX_CAPACITY, Inbound};

/// The socket file's name inside the agent directory.
pub(crate) const SOCKET_NAME: &str = "agent.sock";

/// `sizeof(sun_path)`: a socket path must be shorter than this (its terminating NUL fits in it).
#[cfg(target_os = "macos")]
const SUN_PATH_MAX: usize = 104;
#[cfg(not(target_os = "macos"))]
const SUN_PATH_MAX: usize = 108;

/// The account's private runtime directory a socket can fall back to when the agent directory's
/// path is too long (ADR-7335): `$TMPDIR` on macOS (per user, under `/var/folders`), and
/// `$XDG_RUNTIME_DIR` on Linux. Only an absolute path is taken.
fn private_runtime_dir() -> Option<PathBuf> {
    let name = if cfg!(target_os = "macos") {
        "TMPDIR"
    } else {
        "XDG_RUNTIME_DIR"
    };
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// FNV-1a over the agent directory's path bytes: the fallback directory's name, the same for the
/// same agent directory on every start.
fn dir_key(dir: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    dir.as_os_str()
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
        })
}

/// Where the socket of the agent directory `dir` (absolute) binds: `<dir>/agent.sock` when that
/// path fits in `sun_path`; otherwise `<short>/regia-agent-<key>/agent.sock` in the private runtime
/// directory `short`, returned with that directory (ADR-7335). `key` is `dir_key` of `dir`'s
/// canonical path, so every spelling of one directory (macOS: `/var` and `/private/var`) gets the
/// same fallback. On macOS `sun_path` holds 104
/// bytes, so a project a few folders deep under `$TMPDIR` or `~/Library` could not bind its socket
/// and lost the local transport. `None`: no path fits.
fn socket_path(dir: &Path, key: u64, short: Option<&Path>) -> Option<(PathBuf, Option<PathBuf>)> {
    let fits = |path: &Path| path.as_os_str().len() < SUN_PATH_MAX;
    let direct = dir.join(SOCKET_NAME);
    if fits(&direct) {
        return Some((direct, None));
    }
    let fallback = fallback_dir(short?, key);
    let path = fallback.join(SOCKET_NAME);
    fits(&path).then_some((path, Some(fallback)))
}

/// The fallback directory of the agent directory whose canonical path has `key` (`dir_key`).
fn fallback_dir(base: &Path, key: u64) -> PathBuf {
    base.join(format!("regia-agent-{key:016x}"))
}

/// The fallback directory a server of the agent directory `dir` binds its socket in when
/// `<dir>/agent.sock` is too long, for a client checking an advertisement (ADR-7335). `None`: no
/// private runtime directory, or `dir` does not resolve.
pub(crate) fn fallback_socket_dir(dir: &Path) -> Option<PathBuf> {
    let base = private_runtime_dir()?;
    let key = dir_key(&fs::canonicalize(dir).ok()?);
    Some(fallback_dir(&base, key))
}

/// Creates (or keeps) the fallback directory `fallback` inside `base`, owner-only. `base` must be
/// a directory no other account can write into (mode `0o077` clear), so nobody else can have
/// made `fallback` first or swap it later.
fn prepare_fallback(base: &Path, fallback: &Path) -> io::Result<()> {
    let meta = base.symlink_metadata()?;
    if !meta.is_dir() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{}: not a private directory for the agent socket", base.display()),
        ));
    }
    crate::endpoint::prepare_dir(fallback)?;
    let made = fallback.symlink_metadata()?;
    if !made.is_dir() || made.uid() != meta.uid() || made.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{}: not an owner-only directory", fallback.display()),
        ));
    }
    Ok(())
}

/// Which socket file a listener bound: device and inode, read right after `bind`. A path
/// that later holds another socket (a successor re-bound it after ours was unlinked) is not
/// ours any more, so cleanup leaves it alone (sprint-rev-kimi-9 finding 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    dev: u64,
    ino: u64,
}

impl Identity {
    fn of(path: &Path) -> io::Result<Self> {
        let meta = path.symlink_metadata()?;
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }

    /// Whether `address` still names the socket file this identity was read from.
    pub(crate) fn is_current(&self, address: &str) -> bool {
        Self::of(Path::new(address)).is_ok_and(|now| now == *self)
    }
}

/// Removes the socket file when the server is dropped — only while the path still holds this
/// server's socket — so a later start never meets a stale file of its own and a successor's
/// socket is never removed.
pub(crate) struct Guard {
    path: PathBuf,
    identity: Identity,
    /// The fallback directory the socket was bound in (ADR-7335), removed with it when empty.
    fallback: Option<PathBuf>,
    _registration: local::Registration,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(address) = self.path.to_str()
            && self.identity.is_current(address)
        {
            let _ = fs::remove_file(&self.path);
            if let Some(fallback) = &self.fallback {
                // Only an empty directory goes; a successor's socket keeps it.
                let _ = fs::remove_dir(fallback);
            }
        }
    }
}

/// The client side of the local transport.
pub(crate) struct LocalStream(pub(crate) UnixStream);

impl LocalStream {
    pub(crate) fn connect(address: &str) -> io::Result<Self> {
        Ok(Self(UnixStream::connect(address)?))
    }

    /// Sets the socket read timeout: the handshake deadline on the server side (re-armed
    /// before each read with the time left), the caller's read timeout on the client side.
    pub(crate) fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(timeout)
    }
}

impl io::Read for LocalStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.0, buf)
    }
}

impl io::Write for LocalStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut self.0, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.0)
    }
}

/// Binds `<dir>/agent.sock` (owner-only) and accepts connections in the background. When that path
/// is too long for `sun_path`, the socket binds in an owner-only directory of the account's private
/// runtime directory instead (`socket_path`, ADR-7335); the advertised address names it either way.
///
/// A leftover socket file from a crashed run (nothing listens on it: a connect is refused) is
/// removed; a socket another listener still serves is left alone and the start fails with
/// [`io::ErrorKind::AddrInUse`] (a second app in the same project uses its WebSocket URL);
/// anything else — a user's file, a symlink — is refused, never unlinked. The agent directory
/// is created or kept owner-only by [`crate::endpoint::prepare_dir`], so only the owner can
/// reach the socket in the first place.
pub(crate) fn start(
    token: String,
    dir: &Path,
    board: Option<RenderOutBoard>,
) -> io::Result<Option<LocalServer>> {
    crate::endpoint::prepare_dir(dir)?;
    let dir = std::path::absolute(dir)?;
    let key = dir_key(&fs::canonicalize(&dir)?);
    let base = private_runtime_dir();
    let (path, fallback) = socket_path(&dir, key, base.as_deref()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{}: the socket path is too long and no private runtime directory takes it",
                dir.display()
            ),
        )
    })?;
    if let (Some(fallback), Some(base)) = (&fallback, &base) {
        prepare_fallback(base, fallback)?;
    }
    remove_dead_socket(&path)?;
    let listener = UnixListener::bind(&path)?;
    // The directory is 0700; the socket itself is 0600 so tools that list sockets do not
    // advertise it to other accounts either.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let identity = Identity::of(&path)?;
    let address = path.display().to_string();
    let (tx, rx) = bounded(INBOX_CAPACITY);
    let live = Arc::new(AtomicUsize::new(0));
    let spawned = std::thread::Builder::new()
        .name("regia-agent-local-accept".into())
        .spawn({
            let address = address.clone();
            move || accept_loop(listener, token, address, tx, live, board)
        });
    spawned?;
    Ok(Some(LocalServer {
        address: address.clone(),
        inbox: rx,
        _guard: Guard {
            path,
            identity,
            fallback,
            _registration: local::Registration::new(address, identity),
        },
    }))
}

/// Clears the socket path for `bind`: absent is fine; a socket nothing listens on (the
/// connect probe is refused) is a crashed run's and is removed; a live socket is another
/// listener's ([`io::ErrorKind::AddrInUse`]); a non-socket is refused.
fn remove_dead_socket(path: &Path) -> io::Result<()> {
    let meta = match path.symlink_metadata() {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if !meta.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{}: exists and is not a socket", path.display()),
        ));
    }
    match UnixStream::connect(path) {
        // The probe connection closes at once; the live server reads a clean EOF.
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("{}: another listener serves this socket", path.display()),
        )),
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => fs::remove_file(path),
        Err(err) => Err(err),
    }
}

fn accept_loop(
    listener: UnixListener,
    token: String,
    address: String,
    tx: crossbeam_channel::Sender<Inbound>,
    live: Arc<AtomicUsize>,
    board: Option<RenderOutBoard>,
) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let Some(slot) = ConnectionSlot::take(&live) else {
                    tracing::warn!(target: "regia_agent", "local connection refused: {} already open", server::MAX_CONNECTIONS);
                    continue; // dropping the socket closes it before any thread starts
                };
                let (tx, token, board) = (tx.clone(), token.clone(), board.clone());
                let spawned = std::thread::Builder::new()
                    .name("regia-agent-local-conn".into())
                    .spawn(move || {
                        let _slot = slot;
                        local::serve_connection(LocalStream(stream), &token, &tx, board.as_ref());
                    });
                if let Err(err) = spawned {
                    tracing::warn!(target: "regia_agent", "cannot start local connection thread: {err}");
                }
            }
            Err(err) => {
                tracing::warn!(target: "regia_agent", "local accept on {address} failed: {err}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M5, 2026-11-12 (ADR-7335): `$TMPDIR` is 49 bytes there, so a test's agent directory
    /// `$TMPDIR/regia-agent-local-transport-timeout-<pid>-<n>` put `agent.sock` past macOS's 104-byte
    /// `sun_path` and seven protocol_transport tests failed to bind ("path must be shorter than
    /// SUN_LEN"). A short path stays in the agent directory; a long one falls back.
    #[test]
    fn a_socket_path_too_long_for_sun_path_falls_back_to_the_runtime_dir() {
        let short = Path::new("/run/user/1000");
        let near = Path::new("/home/me/game/.regia/agent");
        assert_eq!(
            socket_path(near, dir_key(near), Some(short)),
            Some((near.join(SOCKET_NAME), None)),
            "a short path binds in the agent directory"
        );
        let deep = PathBuf::from(format!("/{}/.regia/agent", "d".repeat(SUN_PATH_MAX)));
        let key = dir_key(&deep);
        let (path, fallback) = socket_path(&deep, key, Some(short)).expect("the fallback fits");
        let fallback = fallback.expect("a fallback directory");
        assert_eq!(fallback, fallback_dir(short, key));
        assert_eq!(path, fallback.join(SOCKET_NAME));
        assert!(fallback.starts_with(short));
        assert!(path.as_os_str().len() < SUN_PATH_MAX);
        assert!(path.to_string_lossy().ends_with("agent.sock"));
        // The same agent directory always gets the same path; another one gets another.
        assert_eq!(
            socket_path(&deep, key, Some(short)),
            Some((path.clone(), Some(fallback)))
        );
        let other = PathBuf::from(format!("/{}/.regia/agent", "e".repeat(SUN_PATH_MAX)));
        assert_ne!(
            socket_path(&other, dir_key(&other), Some(short)).map(|(p, _)| p),
            Some(path)
        );
        // No runtime directory, or one too long itself: nothing fits.
        assert_eq!(socket_path(&deep, key, None), None);
        let long_base = PathBuf::from(format!("/{}", "r".repeat(SUN_PATH_MAX)));
        assert_eq!(socket_path(&deep, key, Some(&long_base)), None);
    }

    /// The boundary: a path of `SUN_PATH_MAX - 1` bytes binds where it is, one byte more falls back.
    #[test]
    fn the_sun_path_boundary_counts_the_terminating_nul() {
        let short = Path::new("/run/user/1000");
        let at = |len: usize| {
            let dir_len = len - SOCKET_NAME.len() - 1;
            PathBuf::from(format!("/{}", "a".repeat(dir_len - 1)))
        };
        let fits = at(SUN_PATH_MAX - 1);
        assert_eq!(fits.join(SOCKET_NAME).as_os_str().len(), SUN_PATH_MAX - 1);
        assert_eq!(socket_path(&fits, 1, Some(short)).and_then(|(_, f)| f), None);
        let over = at(SUN_PATH_MAX);
        assert!(socket_path(&over, 1, Some(short)).and_then(|(_, f)| f).is_some());
    }

    #[test]
    fn the_fallback_needs_a_private_base_directory() {
        let root = regia_test_utils::unique_temp_dir("agent-sock-base");
        let base = root.join("b");
        fs::create_dir_all(&base).expect("base");
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).expect("chmod");
        let fallback = base.join("regia-agent-0");
        let err = prepare_fallback(&base, &fallback).expect_err("a shared base is refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(!fallback.exists(), "nothing is created in a shared base");
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).expect("chmod");
        prepare_fallback(&base, &fallback).expect("a private base takes the fallback");
        let mode = fallback.symlink_metadata().expect("meta").mode();
        assert_eq!(mode & 0o077, 0, "the fallback directory is owner-only");
        let _ = fs::remove_dir_all(&root);
    }
}
