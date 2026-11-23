// SPDX-License-Identifier: MPL-2.0
//! The local transport of the agent protocol (spec 23 § 2): a Unix domain socket (Linux/macOS)
//! or a Windows named pipe, listening beside the WebSocket server and speaking the same
//! JSON-RPC 2.0 text messages. The wire carries one message per frame: a 4-byte
//! little-endian length and a UTF-8 JSON payload — the same text a WebSocket message holds —
//! and a connection starts with one bearer-token handshake frame, the counterpart of the
//! WebSocket handshake's `Authorization` header (ADR-5708, book/agents/local-transport.md).
//!
//! The transport runs on its own threads and feeds the same [`Inbound`] channel type as the
//! WebSocket path; the world drains it in `dispatch::process_requests` through the
//! [`LocalServer`] resource. Only the owner can connect: the Unix socket is mode 0600 in the
//! 0700 agent directory, and the pipe is created with an owner-only DACL (regia_net
//! `private_file`, ADR-4828) as the first instance of its name, and the client checks the
//! pipe's server process before it sends the token (ADR-5878).
//!
//! The server answers every handshake with one frame, [`HANDSHAKE_ACCEPTED`] or
//! [`HANDSHAKE_REFUSED`] (ADR-5878), so a client learns at connect time whether its token was
//! taken and can fall back to the WebSocket URL when it was not.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
// Derive macros emit `bevy_ecs::...` paths; the prelude re-exports the crate under that name.
use regia_app::prelude::Resource;
use regia_ecs::prelude::bevy_ecs;

use crate::platform::{self, LocalStream};
use crate::render_out::RenderOutBoard;
use crate::server::{self, Inbound};

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod serve;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub(crate) use serve::serve_connection;

/// Largest single frame the local transport accepts — the same cap the
/// WebSocket path applies to one message (`server::MAX_FRAME_BYTES`). A frame over it ends
/// that connection only.
pub const MAX_FRAME_BYTES: usize = server::MAX_FRAME_BYTES;

/// Largest handshake frame (and handshake answer) either side accepts. The handshake is read
/// before authentication, so it is capped far below [`MAX_FRAME_BYTES`]: a bearer token is
/// 64 hex characters and the frame is one small JSON object (ADR-5878).
pub const MAX_HANDSHAKE_BYTES: usize = 256;

/// The server's answer to a handshake with the right token; JSON-RPC frames follow.
pub const HANDSHAKE_ACCEPTED: &str = r#"{"authenticated":true}"#;

/// The server's answer to a refused handshake (a wrong token, a malformed or oversized frame,
/// or no complete frame within the handshake deadline); the server closes after it.
pub const HANDSHAKE_REFUSED: &str = r#"{"authenticated":false}"#;

/// The length field of one frame, in bytes.
const HEADER_BYTES: usize = 4;

/// A running local-transport server.
#[derive(Resource)]
pub struct LocalServer {
    /// Where clients connect: the absolute socket path on Unix, the pipe name on Windows.
    pub address: String,
    /// Requests from every local connection, in arrival order.
    pub inbox: Receiver<Inbound>,
    /// Keeps the listener registered; removes the socket file on drop where one exists and it
    /// is still this server's.
    pub(crate) _guard: platform::Guard,
}

impl std::fmt::Debug for LocalServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalServer")
            .field("address", &self.address)
            .finish()
    }
}

/// Binds the platform's local transport inside `dir` and starts accepting connections that
/// present `token` during the handshake. `board` lets `render.out_progress` polls be answered
/// while the app thread is inside a render-out step, as on the WebSocket path
/// (ADR-2582); tests pass `None`.
///
/// Returns `Ok(None)` on platforms without a local transport (Android: the WebSocket
/// listener reached through `adb forward`, spec 23 § 2).
///
/// # Errors
///
/// The agent directory cannot be prepared; the socket path holds a live socket of another
/// listener ([`io::ErrorKind::AddrInUse`]: a second app in the same project never takes the
/// first one's socket over) or something that is not a socket; or the listener/pipe cannot
/// be created (on Windows also when another process already owns the pipe name).
pub fn start(token: String, dir: &Path, board: Option<RenderOutBoard>) -> io::Result<Option<LocalServer>> {
    platform::start(token, dir, board)
}

/// The listeners this process serves, by address, with the platform's proof that the address
/// still names this listener (`platform::Identity`). Exit cleanup removes a `socket`
/// advertisement only when it names one of these (sprint-rev-kimi-9 finding 6).
static SERVED: Mutex<Vec<(u64, String, platform::Identity)>> = Mutex::new(Vec::new());

/// A listener's entry in the served set; removed on drop.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub(crate) struct Registration(u64);

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
impl Registration {
    /// Records that this process serves `address`.
    pub(crate) fn new(address: String, identity: platform::Identity) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let key = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut served) = SERVED.lock() {
            served.push((key, address, identity));
        }
        Self(key)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut served) = SERVED.lock() {
            served.retain(|(key, _, _)| *key != self.0);
        }
    }
}

/// Whether this process runs the live listener `address` names: a server started here and not
/// dropped, whose socket the path still holds (a successor may have re-bound it).
pub(crate) fn serves(address: &str) -> bool {
    SERVED.lock().is_ok_and(|served| {
        served
            .iter()
            .any(|(_, served, identity)| served == address && identity.is_current(address))
    })
}

/// A byte stream with a settable read timeout: the local stream of every platform, and the
/// scripted streams of the unit tests.
pub(crate) trait TimedRead: Read {
    /// Sets (or clears, with `None`) the timeout of the next reads.
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;
}

impl TimedRead for LocalStream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        LocalStream::set_read_timeout(self, timeout)
    }
}

/// Reads one frame: the 4-byte little-endian length, then exactly that many UTF-8 bytes.
/// `Ok(None)` is a clean close at a frame boundary; a zero or over-cap length and malformed
/// UTF-8 are `Err` and end the connection. Reads keep whatever read timeout the stream has.
/// Also the client's read path.
pub(crate) fn read_frame(stream: &mut impl TimedRead, max: usize) -> io::Result<Option<String>> {
    read_frame_with(stream, max, None, Instant::now)
}

/// [`read_frame`] with a total `deadline` for the whole frame, not per read: each read gets
/// only the time left, so a peer trickling one byte at a time cannot hold the connection past
/// the deadline ([`io::ErrorKind::TimedOut`]). The stream's read timeout is left set; the
/// caller clears it.
pub(crate) fn read_frame_before(
    stream: &mut impl TimedRead,
    max: usize,
    deadline: Instant,
) -> io::Result<Option<String>> {
    read_frame_with(stream, max, Some(deadline), Instant::now)
}

/// The frame reader with the clock passed in, so the deadline is testable without waiting.
fn read_frame_with(
    stream: &mut impl TimedRead,
    max: usize,
    deadline: Option<Instant>,
    now: impl Fn() -> Instant,
) -> io::Result<Option<String>> {
    let mut header = [0u8; HEADER_BYTES];
    let first = loop {
        arm(stream, deadline, &now)?;
        match stream.read(&mut header[..1]) {
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            other => break other?,
        }
    };
    if first == 0 {
        return Ok(None);
    }
    fill(stream, &mut header[1..], deadline, &now)?;
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 || len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes over the {max}-byte cap"),
        ));
    }
    let mut payload = vec![0u8; len];
    fill(stream, &mut payload, deadline, &now)?;
    String::from_utf8(payload)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame is not UTF-8"))
}

/// Sets the read timeout to the time left before `deadline`; an expired deadline is
/// [`io::ErrorKind::TimedOut`]. No deadline leaves the stream's timeout alone.
fn arm(stream: &mut impl TimedRead, deadline: Option<Instant>, now: &impl Fn() -> Instant) -> io::Result<()> {
    let Some(deadline) = deadline else {
        return Ok(());
    };
    let left = deadline.saturating_duration_since(now());
    if left.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "handshake deadline passed",
        ));
    }
    stream.set_read_timeout(Some(left))
}

/// Reads exactly `buf.len()` bytes; an EOF inside is [`io::ErrorKind::UnexpectedEof`].
fn fill(
    stream: &mut impl TimedRead,
    buf: &mut [u8],
    deadline: Option<Instant>,
    now: &impl Fn() -> Instant,
) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        arm(stream, deadline, now)?;
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Writes one frame.
pub(crate) fn write_frame(stream: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame over the cap"));
    }
    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(payload)?;
    stream.flush()
}

/// Scripted streams for the frame reader's unit tests: every platform, no OS objects.
#[cfg(test)]
mod scripted_tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    /// Hands out `data` `chunk` bytes per read and advances a shared fake clock by `step` per
    /// read, recording every read timeout it is given.
    struct Trickle {
        data: Vec<u8>,
        at: usize,
        chunk: usize,
        step: Duration,
        clock: Rc<Cell<Instant>>,
        timeouts: Vec<Option<Duration>>,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.clock.set(self.clock.get() + self.step);
            let n = self.chunk.min(buf.len()).min(self.data.len() - self.at);
            buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    impl TimedRead for Trickle {
        fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
            self.timeouts.push(timeout);
            Ok(())
        }
    }

    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_frame(&mut out, payload).expect("in-memory write");
        out
    }

    fn trickle(data: Vec<u8>, chunk: usize, step: Duration) -> (Trickle, Rc<Cell<Instant>>) {
        let clock = Rc::new(Cell::new(Instant::now()));
        let stream = Trickle {
            data,
            at: 0,
            chunk,
            step,
            clock: clock.clone(),
            timeouts: Vec::new(),
        };
        (stream, clock)
    }

    /// sprint-rev-kimi-9 finding 7: the handshake deadline is a total deadline. A peer that
    /// sends one byte per (fake) second, each well inside a per-read timeout, is cut off once
    /// the 10 s total has passed, and each read was given only the time left.
    #[test]
    fn a_trickling_peer_hits_the_total_deadline() {
        let hello = br#"{"bearer":"0123456789abcdef0123456789abcdef"}"#;
        let (mut stream, clock) = trickle(framed(hello), 1, Duration::from_secs(1));
        let deadline = clock.get() + Duration::from_secs(10);
        let clock_now = clock.clone();
        let err = read_frame_with(&mut stream, MAX_HANDSHAKE_BYTES, Some(deadline), move || {
            clock_now.get()
        })
        .expect_err("the frame takes 50 s; the deadline is 10 s");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert_eq!(stream.at, 10, "one byte per second for 10 s, then no more reads");
        let given: Vec<u64> = stream
            .timeouts
            .iter()
            .map(|t| t.map_or(0, |t| t.as_secs()))
            .collect();
        assert_eq!(
            given,
            (1..=10).rev().collect::<Vec<u64>>(),
            "each read gets only the time left"
        );
    }

    /// The same trickle within the deadline reads the whole frame.
    #[test]
    fn a_frame_inside_the_deadline_is_read() {
        let hello = br#"{"bearer":"t"}"#;
        let (mut stream, clock) = trickle(framed(hello), 3, Duration::from_millis(10));
        let deadline = clock.get() + Duration::from_secs(10);
        let clock_now = clock.clone();
        let text = read_frame_with(&mut stream, MAX_HANDSHAKE_BYTES, Some(deadline), move || {
            clock_now.get()
        })
        .expect("read")
        .expect("a frame");
        assert_eq!(text.as_bytes(), hello);
    }

    /// sprint-rev-kimi-9 finding 7: a handshake header announcing more than
    /// `MAX_HANDSHAKE_BYTES` is refused from the header alone: no body byte is read and no
    /// buffer of the announced size is allocated.
    #[test]
    fn an_oversized_handshake_header_is_refused_before_its_body() {
        let mut data = ((MAX_HANDSHAKE_BYTES + 1) as u32).to_le_bytes().to_vec();
        data.extend(std::iter::repeat_n(b' ', MAX_HANDSHAKE_BYTES + 1));
        let (mut stream, clock) = trickle(data, 64, Duration::ZERO);
        let deadline = clock.get() + Duration::from_secs(10);
        let clock_now = clock.clone();
        let err = read_frame_with(&mut stream, MAX_HANDSHAKE_BYTES, Some(deadline), move || {
            clock_now.get()
        })
        .expect_err("over the handshake cap");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert_eq!(stream.at, HEADER_BYTES, "only the header was read");
    }

    /// Without a deadline the reader never touches the stream's read timeout (the client's
    /// own `set_read_timeout` stays in force).
    #[test]
    fn no_deadline_leaves_the_read_timeout_alone() {
        let (mut stream, _clock) = trickle(framed(b"{}"), 1, Duration::from_secs(100));
        assert_eq!(
            read_frame(&mut stream, MAX_FRAME_BYTES).expect("read").as_deref(),
            Some("{}")
        );
        assert!(stream.timeouts.is_empty(), "{:?}", stream.timeouts);
    }

    /// The handshake answers fit the handshake cap, so the client may read them with it.
    #[test]
    fn handshake_answers_fit_the_handshake_cap() {
        for answer in [HANDSHAKE_ACCEPTED, HANDSHAKE_REFUSED] {
            assert!(answer.len() <= MAX_HANDSHAKE_BYTES, "{answer}");
            let parsed: serde_json::Value = serde_json::from_str(answer).expect("JSON");
            assert!(parsed["authenticated"].is_boolean(), "{answer}");
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    fn connected_pair() -> (LocalStream, LocalStream) {
        let dir = regia_test_utils::unique_temp_dir("agent-local-pair");
        let listener = std::os::unix::net::UnixListener::bind(dir.join("s.sock")).expect("bind");
        let client = LocalStream::connect(dir.join("s.sock").to_str().expect("utf-8")).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (LocalStream(server), client)
    }

    /// The frame codec round-trips the largest well-formed frame and refuses a zero-length,
    /// an over-cap and a non-UTF-8 frame.
    #[test]
    fn frames_round_trip_and_bad_lengths_are_refused() {
        let (mut server, mut client) = connected_pair();
        // The big frame is far larger than the socket buffer: it must be written from a
        // second thread while this one reads.
        let big = "x".repeat(MAX_FRAME_BYTES - 64);
        let writing = std::thread::spawn({
            let big = big.clone();
            move || {
                write_frame(&mut client, big.as_bytes()).expect("write");
                client
            }
        });
        assert_eq!(read_frame(&mut server, MAX_FRAME_BYTES).expect("read"), Some(big));
        let mut client = writing.join().expect("writer");

        write_frame(&mut client, b"{}").expect("write");
        let mut zero = [0u8; 4];
        server.read_exact(&mut zero).expect("header");
        assert_eq!(u32::from_le_bytes(zero), 2, "little-endian length first");
        let mut body = [0u8; 2];
        server.read_exact(&mut body).expect("body");
        assert_eq!(&body, b"{}");

        for len in [0u32, MAX_FRAME_BYTES as u32 + 1] {
            client.write_all(&len.to_le_bytes()).expect("header");
            assert!(read_frame(&mut server, MAX_FRAME_BYTES).is_err(), "{len}");
        }
        client.write_all(&3u32.to_le_bytes()).expect("header");
        client.write_all(&[0xff, 0xfe, 0xfd]).expect("body");
        assert!(read_frame(&mut server, MAX_FRAME_BYTES).is_err(), "not UTF-8");
    }

    /// ADR-2396 on the local transport: a queued `render.out_cancel` or `agent.job_cancel` wakes an
    /// app thread waiting on a stalled render-out output, as it does over the WebSocket. The note
    /// comes before the connection waits for the answer, so it has happened once the answer is in.
    #[test]
    fn a_cancel_over_a_local_connection_wakes_the_stalled_app_thread() {
        let (server, mut client) = connected_pair();
        let (inbox_tx, inbox_rx) = crossbeam_channel::bounded::<Inbound>(4);
        let serving = std::thread::spawn(move || serve_connection(server, "tok", &inbox_tx, None));
        write_frame(&mut client, br#"{"bearer":"tok"}"#).expect("handshake");
        assert_eq!(
            read_frame(&mut client, MAX_HANDSHAKE_BYTES)
                .expect("answer")
                .as_deref(),
            Some(HANDSHAKE_ACCEPTED)
        );
        for (id, method) in [(1, "render.out_cancel"), (2, "agent.job_cancel")] {
            let before = crate::render_out::cancel_notes();
            let request = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{}}}}"#);
            write_frame(&mut client, request.as_bytes()).expect("request");
            let inbound = inbox_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("queued for the world");
            assert_eq!(inbound.request.method, method);
            inbound
                .reply
                .send(crate::protocol::Response::ok(
                    serde_json::json!(id),
                    serde_json::json!({}),
                ))
                .expect("answer");
            assert!(
                read_frame(&mut client, MAX_FRAME_BYTES)
                    .expect("response")
                    .is_some(),
                "{method} answered"
            );
            assert!(
                crate::render_out::cancel_notes() > before,
                "{method} over the local transport noted no cancel"
            );
        }
        drop(client);
        serving.join().expect("serve thread");
    }

    /// A clean close at a frame boundary is `Ok(None)`, not an error.
    #[test]
    fn a_clean_close_mid_frame_is_an_error_but_at_a_boundary_is_eof() {
        let (mut server, client) = connected_pair();
        drop(client);
        assert_eq!(read_frame(&mut server, MAX_FRAME_BYTES).expect("eof"), None);

        let (mut server, mut client) = connected_pair();
        client.write_all(&8u32.to_le_bytes()).expect("header");
        drop(client);
        assert!(
            read_frame(&mut server, MAX_FRAME_BYTES).is_err(),
            "the body never arrived"
        );
    }
}
