// SPDX-License-Identifier: MPL-2.0
//! Socket listeners (`specs/60` § 4.2, § 3.2): one acceptor thread per listener and one reader
//! thread per session, no async runtime (ADR-014). Reader threads read, reassemble, decode and
//! validate, then hand the message to [`FeedCore::submit`]; the main thread applies it.
//!
//! The transports are the Unix domain socket (`.regia/dcc_feed/feed.sock`, mode 0600, no token),
//! loopback TCP (token required) and [`super::ws`]. The Windows named pipe is a follow-up (spec § 9.2).

use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use super::core::{FeedCore, SessionWriter};
use super::session::{Phase, SessionId, Transport};
use super::settings::{DccFeedSettings, ListenSpec};
use super::wire::{self, FrameHeader, FrameReader, Limits, Reassembler};

/// How long an acceptor sleeps between polls of a non-blocking listener.
pub(super) const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// Most connections one feed listener serves at once (each holds a reader thread from accept on,
/// before any hello or token check). Past it a new connection is closed before a thread starts,
/// so a flood of silent connections cannot pile up threads and session records
/// (audit-concurrency-1103 agent M2).
pub const MAX_FEED_CONNECTIONS: usize = 32;

/// Largest frame, and largest reassembled message, a session may send before its `hello` is
/// accepted. A hello is a few short strings; `max_message_bytes` applies once the session leaves
/// `negotiating`. Without it a peer with no token made the reader buffer up to
/// `max_message_bytes` per connection (`docs/security/threat-model.md`, ADR-3206).
pub const MAX_PRE_HELLO_BYTES: u32 = 64 * 1024;

/// `limits` as they apply to a session that has not sent an accepted `hello`.
#[must_use]
pub fn pre_hello_limits(limits: Limits) -> Limits {
    Limits {
        max_message_bytes: limits.max_message_bytes.min(MAX_PRE_HELLO_BYTES),
        ..limits
    }
}

/// Counts a connection of one listener; releases its slot when dropped (with its thread).
pub(super) struct ConnectionSlot(Arc<AtomicUsize>);

impl ConnectionSlot {
    /// Takes a slot, or `None` when [`MAX_FEED_CONNECTIONS`] are in use.
    pub(super) fn take(live: &Arc<AtomicUsize>) -> Option<Self> {
        if live.fetch_add(1, Ordering::AcqRel) >= MAX_FEED_CONNECTIONS {
            live.fetch_sub(1, Ordering::AcqRel);
            tracing::warn!(target: "regia_agent", "dcc feed: connection refused, {MAX_FEED_CONNECTIONS} already open");
            return None;
        }
        Some(Self(Arc::clone(live)))
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
/// Write timeout of a session socket, so a client that stops reading cannot stall the main
/// thread's flush for long (the session is closed instead).
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);

/// The feed's state directory: `<project>/.regia/dcc_feed`.
#[must_use]
pub fn feed_dir(project_dir: &Path) -> PathBuf {
    regia_core::brand::project_state_dir(project_dir).join("dcc_feed")
}

/// Locks the core, recovering from a poisoned lock (a panicking reader thread must not take the
/// editor down with it).
pub fn lock(core: &Mutex<FeedCore>) -> MutexGuard<'_, FeedCore> {
    core.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One open listener, as `dcc_feed.status` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerInfo {
    /// `unix` or `tcp`.
    pub transport: &'static str,
    /// Socket path or `ip:port`.
    pub address: String,
}

/// The running listeners. Dropping it stops the acceptors and closes every session.
#[derive(Debug)]
pub struct Listeners {
    /// What is open.
    pub info: Vec<ListenerInfo>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    unix_path: Option<PathBuf>,
}

impl Drop for Listeners {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        if let Some(p) = &self.unix_path {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// A random 256-bit token, base64url without padding (spec § 4.2).
#[must_use]
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64url(&bytes)
}

/// Base64url (RFC 4648 § 5) without padding.
#[must_use]
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

/// Writes `token` (mode 0600) and `endpoint.json` listing `listeners` into `dir`.
///
/// # Errors
///
/// The I/O error.
pub fn write_endpoint(dir: &Path, listeners: &[ListenerInfo], token: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // Through a fresh, exclusively created temp file and a rename (the agent endpoint's writer): a
    // file or symlink planted at a temp name is never written through.
    crate::endpoint::write_owner_only(dir, "token", token.as_bytes())?;
    let transports: Vec<serde_json::Value> = listeners
        .iter()
        .map(|l| serde_json::json!({ "transport": l.transport, "address": l.address }))
        .collect();
    let doc = serde_json::json!({
        "protocol": "regia-dcc-feed",
        "version": wire::PROTOCOL_VERSION,
        "transports": transports,
    });
    let text = serde_json::to_vec_pretty(&doc).map_err(std::io::Error::other)?;
    crate::endpoint::write_owner_only(dir, "endpoint.json", &text)
}

/// Opens the listeners of `settings`. `feed_dir` holds the Unix socket and the endpoint files;
/// without it only TCP listeners open and nothing is written.
///
/// # Errors
///
/// A message for an invalid listener set (a remote bind without `allow_remote`) or a bind error.
pub fn start(
    settings: &DccFeedSettings,
    core: &Arc<Mutex<FeedCore>>,
    feed_dir: Option<&Path>,
) -> Result<Listeners, String> {
    settings.validate()?;
    let stop = Arc::new(AtomicBool::new(false));
    let mut out = Listeners {
        info: Vec::new(),
        stop: stop.clone(),
        threads: Vec::new(),
        unix_path: None,
    };
    for spec in settings.listeners()? {
        match spec {
            ListenSpec::WebSocket(addr) => {
                let listener =
                    TcpListener::bind(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
                let local = listener.local_addr().map_err(|e| e.to_string())?;
                listener.set_nonblocking(true).map_err(|e| e.to_string())?;
                let (core, stop) = (core.clone(), stop.clone());
                let origins = settings.allowed_origins.clone();
                let limits = settings.limits();
                let thread = std::thread::Builder::new()
                    .name("dcc-feed-ws-accept".into())
                    .spawn(move || super::ws::accept_loop(&listener, &core, &stop, &origins, limits))
                    .map_err(|e| e.to_string())?;
                out.threads.push(thread);
                out.info.push(ListenerInfo {
                    transport: "websocket",
                    address: format!("ws://{local}{}", super::ws::PATH),
                });
            }
            ListenSpec::Tcp(addr) => {
                let listener =
                    TcpListener::bind(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
                let local = listener.local_addr().map_err(|e| e.to_string())?;
                listener.set_nonblocking(true).map_err(|e| e.to_string())?;
                let (core, stop) = (core.clone(), stop.clone());
                let thread = std::thread::Builder::new()
                    .name("dcc-feed-tcp".into())
                    .spawn(move || accept_tcp(&listener, &core, &stop))
                    .map_err(|e| e.to_string())?;
                out.threads.push(thread);
                out.info.push(ListenerInfo {
                    transport: "tcp",
                    address: local.to_string(),
                });
            }
            ListenSpec::Unix => {
                // A socket path has a short length limit, so a deep project may not fit; the
                // other listeners still serve it.
                #[cfg(unix)]
                if let Some(dir) = feed_dir {
                    match start_unix(dir, core, &stop, &mut out.threads) {
                        Ok(path) => {
                            out.info.push(ListenerInfo {
                                transport: "unix",
                                address: path.display().to_string(),
                            });
                            out.unix_path = Some(path);
                        }
                        Err(err) => tracing::warn!(target: "regia_agent", "dcc feed: no Unix socket: {err}"),
                    }
                }
            }
        }
    }
    if let Some(dir) = feed_dir {
        let token = lock(core).token().to_owned();
        write_endpoint(dir, &out.info, &token)
            .map_err(|e| format!("cannot write the endpoint files: {e}"))?;
    }
    Ok(out)
}

#[cfg(unix)]
fn start_unix(
    dir: &Path,
    core: &Arc<Mutex<FeedCore>>,
    stop: &Arc<AtomicBool>,
    threads: &mut Vec<JoinHandle<()>>,
) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    // The agent folder's rule (ADR-5869): a planted `.regia/dcc_feed` link (a cloned project) is
    // refused before anything is changed through it; before, the link's target was chmodded, a
    // `feed.sock` in it deleted and a socket bound there, and only the endpoint write refused.
    crate::endpoint::prepare_dir(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot restrict {}: {e}", dir.display()))?;
    let path = dir.join("feed.sock");
    // A socket left by a crashed run would make bind fail.
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).map_err(|e| format!("cannot listen on {}: {e}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot restrict {}: {e}", path.display()))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let (core, stop) = (core.clone(), stop.clone());
    let thread = std::thread::Builder::new()
        .name("dcc-feed-unix".into())
        .spawn(move || {
            let slots = Arc::new(AtomicUsize::new(0));
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let Some(slot) = ConnectionSlot::take(&slots) else {
                            continue; // dropping the stream closes it
                        };
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
                        let (Ok(w), Ok(s)) = (stream.try_clone(), stream.try_clone()) else {
                            continue;
                        };
                        let writer = SessionWriter {
                            out: Box::new(w),
                            shutdown: Box::new(move || {
                                let _ = s.shutdown(std::net::Shutdown::Both);
                            }),
                        };
                        let id = lock(&core).open(Transport::Unix, "unix", Some(writer));
                        spawn_reader(core.clone(), id, stream, slot);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
                    Err(_) => std::thread::sleep(ACCEPT_POLL),
                }
            }
        })
        .map_err(|e| e.to_string())?;
    threads.push(thread);
    Ok(path)
}

fn accept_tcp(listener: &TcpListener, core: &Arc<Mutex<FeedCore>>, stop: &AtomicBool) {
    let slots = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if let Some(slot) = ConnectionSlot::take(&slots) {
                    open_tcp(core, stream, peer, slot);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
            Err(_) => std::thread::sleep(ACCEPT_POLL),
        }
    }
}

fn open_tcp(core: &Arc<Mutex<FeedCore>>, stream: TcpStream, peer: SocketAddr, slot: ConnectionSlot) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let (Ok(w), Ok(s)) = (stream.try_clone(), stream.try_clone()) else {
        return;
    };
    let writer = SessionWriter {
        out: Box::new(w),
        shutdown: Box::new(move || {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }),
    };
    // The peer's IP is the rate-limit key (one hello per second, lockout after failures).
    let id = lock(core).open(Transport::Tcp, peer.ip().to_string(), Some(writer));
    spawn_reader(core.clone(), id, stream, slot);
}

fn spawn_reader<R: Read + Send + 'static>(
    core: Arc<Mutex<FeedCore>>,
    id: SessionId,
    stream: R,
    slot: ConnectionSlot,
) {
    let reader_core = core.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("dcc-feed-session-{id}"))
        .spawn(move || {
            let _slot = slot;
            read_session(&reader_core, id, stream);
        });
    if spawned.is_err() {
        let mut c = lock(&core);
        let now = c.now_s;
        c.sessions.begin_close(id, "internal", now);
    }
}

/// Joins fragments of one session and hands whole messages to the core.
#[derive(Debug)]
pub(super) struct MessageAssembler {
    limits: Limits,
    reassembler: Reassembler,
    message_bytes: u32,
    /// True until the session leaves `negotiating`: frames are held to [`MAX_PRE_HELLO_BYTES`].
    negotiating: bool,
}

impl MessageAssembler {
    pub(super) fn new(limits: Limits) -> Self {
        Self {
            limits,
            reassembler: Reassembler::new(),
            message_bytes: 0,
            negotiating: true,
        }
    }

    /// The limits for the next frame: [`pre_hello_limits`] until a hello is accepted.
    pub(super) fn frame_limits(&self) -> Limits {
        if self.negotiating {
            pre_hello_limits(self.limits)
        } else {
            self.limits
        }
    }

    /// Handles one frame. False: the session is over.
    pub(super) fn deliver(
        &mut self,
        core: &Mutex<FeedCore>,
        id: SessionId,
        header: FrameHeader,
        payload: Vec<u8>,
    ) -> bool {
        let frame_bytes = u32::try_from(wire::HEADER_BYTES + payload.len()).unwrap_or(u32::MAX);
        self.message_bytes = self.message_bytes.saturating_add(frame_bytes);
        let seq = header.seq;
        let frame_limits = self.frame_limits();
        let (h, body) = match self.reassembler.push(header, payload, &frame_limits) {
            Ok(None) => return true,
            Ok(Some(complete)) => complete,
            Err(e) => {
                lock(core).refuse_fatal(id, seq, &e);
                return false;
            }
        };
        let decoded = {
            let _span = tracing::trace_span!("dcc_feed_decode", bytes = body.len()).entered();
            wire::decode_with_warnings(h, &body, &self.limits)
        };
        let mut c = lock(core);
        match decoded {
            Ok((msg, renormalized)) => {
                if renormalized > 0 {
                    c.warn(
                        id,
                        "quaternion_renormalized",
                        format!("{renormalized} off-unit quaternions in frame {}", h.seq),
                    );
                }
                c.submit(id, h.seq, self.message_bytes, msg);
            }
            Err(e) if e.code.is_fatal() => c.refuse_fatal(id, h.seq, &e),
            Err(e) => c.send_error(id, h.seq, 0, &e),
        }
        self.message_bytes = 0;
        c.flush_replies(id);
        let phase = c.sessions.phase(id);
        if self.negotiating && phase != Some(Phase::Negotiating) {
            self.negotiating = false;
        }
        !matches!(phase, Some(Phase::Closing | Phase::Closed) | None)
    }
}

/// The reader loop of one session: read, reassemble, decode, validate, submit. Ends at end of
/// stream, a fatal error, or when the session closes.
pub fn read_session<R: Read>(core: &Mutex<FeedCore>, id: SessionId, stream: R) {
    let limits = lock(core).limits();
    let mut reader = FrameReader::new(stream);
    let mut assembler = MessageAssembler::new(limits);
    loop {
        match reader.next(&assembler.frame_limits()) {
            Ok(None) => {
                let mut c = lock(core);
                let now = c.now_s;
                c.sessions.begin_close(id, "end of stream", now);
                return;
            }
            // A read error after the main thread cut the socket is the normal way out; the
            // refusal is a no-op then, since the session is already closing.
            Err(e) => {
                lock(core).refuse_fatal(id, 0, &e);
                return;
            }
            Ok(Some((header, payload))) => {
                if !assembler.deliver(core, id, header, payload) {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_vectors() {
        // RFC 4648 § 10 vectors, unpadded.
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(new_token().len(), 43);
        assert_ne!(new_token(), new_token());
    }

    /// audit-v034b-conc L7: the token was written through the fixed name `token.tmp` opened with
    /// create + truncate, which follows a planted symlink and overwrites its target.
    #[cfg(unix)]
    #[test]
    fn a_planted_temp_symlink_is_not_written_through() {
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_tmp_link");
        std::fs::create_dir_all(&dir).expect("dir");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, b"keep me").expect("victim");
        std::os::unix::fs::symlink(&victim, dir.join("token.tmp")).expect("plant link");
        write_endpoint(&dir, &[], "secret-token").expect("write");
        assert_eq!(std::fs::read(&victim).expect("victim"), b"keep me");
        assert_eq!(
            std::fs::read_to_string(dir.join("token")).expect("token"),
            "secret-token"
        );
        assert!(!dir.join("token").is_symlink(), "the token is a file of its own");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn endpoint_files_are_written_private() {
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_endpoint");
        let info = vec![ListenerInfo {
            transport: "tcp",
            address: "127.0.0.1:4000".to_owned(),
        }];
        write_endpoint(&dir, &info, "tok").expect("write");
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("endpoint.json")).expect("read")).expect("json");
        assert_eq!(doc["transports"][0]["address"], "127.0.0.1:4000");
        assert_eq!(std::fs::read_to_string(dir.join("token")).expect("token"), "tok");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("token"))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every accepted connection used to get a reader thread and a session record before any
    /// hello or token check, with no cap (audit-concurrency-1103 agent M2): past
    /// `MAX_FEED_CONNECTIONS` a connection is closed at once, the earlier ones stay open.
    #[test]
    fn connections_past_the_cap_are_closed_before_a_thread_starts() {
        use std::io::Read as _;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr");
        let core = Arc::new(Mutex::new(FeedCore::new(&DccFeedSettings::default(), "t")));
        let stop = Arc::new(AtomicBool::new(false));
        let acceptor = {
            let (core, stop) = (core.clone(), stop.clone());
            std::thread::spawn(move || accept_tcp(&listener, &core, &stop))
        };
        let mut open: Vec<TcpStream> = (0..32)
            .map(|_| TcpStream::connect(addr).expect("connect"))
            .collect();
        // The 33rd connection: the server closes it (EOF) instead of serving it.
        let mut extra = TcpStream::connect(addr).expect("connect");
        extra
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("timeout");
        let mut byte = [0u8; 1];
        let read = extra.read(&mut byte);
        assert!(
            matches!(read, Ok(0)),
            "a connection past the cap is closed by the server: {read:?}"
        );
        // A served connection is still open: it gets no EOF without a hello.
        open[0].set_nonblocking(true).expect("nonblocking");
        let first = open[0].read(&mut byte);
        assert!(
            matches!(&first, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "a connection under the cap is served: {first:?}"
        );
        stop.store(true, Ordering::SeqCst);
        open.clear();
        let _ = acceptor.join();
    }

    /// A symlink planted at the fixed temp name (e.g. in an untrusted checked-out project's
    /// `.regia/dcc_feed/`) used to be followed: its target was truncated, overwritten with the
    /// token and chmodded 0600 (audit-concurrency-1103 agent L3).
    #[cfg(unix)]
    #[test]
    fn a_planted_temp_symlink_is_not_followed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_endpoint_link");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "precious").expect("victim");
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        for planted in ["token.tmp", "endpoint.tmp"] {
            std::os::unix::fs::symlink(&victim, dir.join(planted)).expect("plant link");
        }
        write_endpoint(&dir, &[], "tok").expect("write");
        assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "precious");
        let mode = std::fs::metadata(&victim).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "the link's target keeps its mode");
        assert_eq!(std::fs::read_to_string(dir.join("token")).expect("token"), "tok");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
