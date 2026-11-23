// SPDX-License-Identifier: MPL-2.0
//! The Windows named pipe of the agent's local transport (spec 23 § 2). The pipe is created
//! with an owner-only DACL — the current user, `SYSTEM` and `Administrators`, protected from
//! inheritance — through `regia_net::private_file` (ADR-4828), as the first instance of its
//! name (`FILE_FLAG_FIRST_PIPE_INSTANCE`), so a name another account created first fails the
//! start instead of being joined. The client opens it for identification only (no
//! impersonation) and checks the pipe's server process against the pid the name carries
//! before it sends the token (ADR-5878). Selected by `platform/mod.rs`
//! (`docs/04-conventions.md` § 8).
//!
//! Every handle is opened for overlapped IO, so reads honor a timeout — the server's
//! handshake deadline and the client's `Client::set_read_timeout` — through
//! `WaitForSingleObject` and `CancelIoEx` (Microsoft Learn, "Synchronous and Asynchronous
//! I/O", "Canceling Pending I/O Operations"). The decisions live in `pipe_rules.rs`.
#![allow(unsafe_code)] // Win32 pipe FFI (CreateNamedPipeW, ReadFile, ...); every block states its invariant.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use crossbeam_channel::bounded;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_OPERATION_ABORTED,
    ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, SetNamedPipeHandleState,
    WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, ResetEvent, WaitForSingleObject};

use super::pipe_rules::{self, ConnectStep};
use crate::local::{self, LocalServer};
use crate::render_out::RenderOutBoard;
use crate::server::{self, ConnectionSlot, INBOX_CAPACITY, Inbound};

// The rules mirror the Win32 codes as plain numbers so they are testable on every host.
const _: () = {
    assert!(pipe_rules::ERROR_BROKEN_PIPE == ERROR_BROKEN_PIPE as i32);
    assert!(pipe_rules::ERROR_NO_DATA == ERROR_NO_DATA as i32);
    assert!(pipe_rules::ERROR_PIPE_NOT_CONNECTED == ERROR_PIPE_NOT_CONNECTED as i32);
    assert!(pipe_rules::ERROR_PIPE_CONNECTED == ERROR_PIPE_CONNECTED as i32);
    assert!(pipe_rules::INFINITE_MS == INFINITE);
};

/// How long a client waits for a free instance when every instance is busy (the server makes
/// the next one right after each connect, so this is a short race, not a queue).
const PIPE_BUSY_WAIT_MS: u32 = 2_000;

/// The pipe is per server: the process id (like the OS-picked WebSocket port, two apps of one
/// user never collide) plus a per-process serial (two servers in one process never share a
/// name). The name is all the `socket` advertisement carries.
fn pipe_address() -> String {
    static SERIAL: AtomicU32 = AtomicU32::new(0);
    crate::endpoint::pipe_name(std::process::id(), SERIAL.fetch_add(1, Ordering::Relaxed))
}

/// A pipe name is never reused while this process runs, so an address this process serves is
/// always still its own.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Identity;

impl Identity {
    pub(crate) fn is_current(&self, _address: &str) -> bool {
        true
    }
}

/// Keeps the server in the served set (`crate::local::serves`); the pipe name itself vanishes
/// with its last handle.
pub(crate) struct Guard {
    _registration: local::Registration,
}

/// One end of a pipe instance, opened for overlapped IO: the client's handle, or a server
/// instance waiting for or serving one client. Owns the handle and the event its operations
/// signal.
pub(crate) struct LocalStream {
    handle: HANDLE,
    event: HANDLE,
    timeout: Option<Duration>,
}

// SAFETY: a pipe handle and its event are owned, unique kernel objects: handing the stream to the
// connection thread (or the client's caller) transfers ownership, and every use goes through
// `&mut self`, so one thread at a time.
unsafe impl Send for LocalStream {}

// SAFETY: `LocalStream` has no method taking `&self` (reads, writes, timeouts and drop all take
// `&mut self`), so a shared reference gives no access to the handles; sharing one between
// threads cannot race. `Sync` lets a `Client` live in an ECS resource, as on Unix (integ-v049a,
// ADR-5923).
unsafe impl Sync for LocalStream {}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

impl LocalStream {
    /// Takes ownership of an open overlapped pipe `handle` and gives it its event.
    fn adopt(handle: HANDLE) -> io::Result<Self> {
        // SAFETY: no attributes, manual reset, initially unsignaled, unnamed; checked below.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: `handle` is live and owned here.
            unsafe { CloseHandle(handle) };
            return Err(err);
        }
        Ok(Self {
            handle,
            event,
            timeout: None,
        })
    }

    /// Opens the pipe at `address` and checks that its server is the process the name
    /// carries before anything is written to it.
    pub(crate) fn connect(address: &str) -> io::Result<Self> {
        let expected = crate::endpoint::pipe_server_pid(address)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a Regia agent pipe name"))?;
        let name = wide(address);
        let handle = match open_client(&name) {
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                // Every instance is busy for a moment: wait for one, once.
                // SAFETY: `name` is NUL-terminated.
                if unsafe { WaitNamedPipeW(name.as_ptr(), PIPE_BUSY_WAIT_MS) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                open_client(&name)?
            }
            other => other?,
        };
        let stream = Self::adopt(handle)?;
        let mode = PIPE_READMODE_BYTE;
        // SAFETY: `stream.handle` is a live pipe handle; `mode` a valid in-pointer.
        let ok =
            unsafe { SetNamedPipeHandleState(stream.handle, &mode, std::ptr::null(), std::ptr::null_mut()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut server_pid = 0u32;
        // SAFETY: `stream.handle` is a live client pipe handle; `server_pid` a valid out-pointer.
        if unsafe { GetNamedPipeServerProcessId(stream.handle, &mut server_pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        pipe_rules::check_server_pid(expected, server_pid)?;
        Ok(stream)
    }

    /// Creates one server instance of the pipe `address` (owner-only DACL, local clients
    /// only); `first` demands that no instance of the name exists yet.
    fn create_instance(address: &str, first: bool) -> io::Result<Self> {
        let name = wide(address);
        let desc = regia_net::private_file::owner_only_descriptor()
            .map_err(|e| io::Error::other(format!("owner-only pipe DACL: {e}")))?;
        let attrs = desc.security_attributes();
        let open_mode =
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        // SAFETY: `name` is NUL-terminated; `attrs` names a live security descriptor for the
        // duration of the call; the returned handle is checked below.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                open_mode,
                // Byte framing, and no network clients: local accounts only. (PIPE_WAIT is the
                // blocking mode flag; overlapped IO is what bounds the waits.)
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                // Buffer sizes: the largest frame plus its header, each way.
                (local::MAX_FRAME_BYTES + 64) as u32,
                (local::MAX_FRAME_BYTES + 64) as u32,
                0,
                &attrs,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Self::adopt(handle)
    }

    /// Sets the read timeout of the next reads: `None` waits forever; zero is refused, as on
    /// `std` sockets. Works on both ends (the handles are overlapped).
    pub(crate) fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        pipe_rules::check_read_timeout(timeout)?;
        self.timeout = timeout;
        Ok(())
    }

    /// Runs one overlapped operation `start` on this handle and waits up to `wait_ms` for it.
    /// On a timeout the operation is cancelled and its end awaited before this returns, so
    /// the kernel never writes to `OVERLAPPED` or a buffer after they are gone; a timed-out
    /// operation is [`io::ErrorKind::TimedOut`]. Returns the bytes transferred.
    fn overlapped(
        &mut self,
        wait_ms: u32,
        start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32,
    ) -> io::Result<u32> {
        // SAFETY: `self.event` is a live event handle owned by `self`.
        unsafe { ResetEvent(self.event) };
        let mut ov = OVERLAPPED {
            hEvent: self.event,
            ..Default::default()
        };
        let started = start(self.handle, &mut ov);
        if started == 0 {
            // SAFETY: plain thread-local error read right after the failed call.
            let code = unsafe { GetLastError() };
            if code != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            // SAFETY: `self.event` is live; the wait has a bound or is INFINITE by request.
            let waited = unsafe { WaitForSingleObject(self.event, wait_ms) };
            if waited != WAIT_OBJECT_0 {
                // SAFETY: `ov` is the pending operation's OVERLAPPED on this handle.
                unsafe { CancelIoEx(self.handle, &ov) };
                let mut done = 0u32;
                // SAFETY: waits (bWait = TRUE) until the cancelled or completed operation is
                // over; `ov` and the caller's buffer outlive it.
                if unsafe { GetOverlappedResult(self.handle, &ov, &mut done, 1) } != 0 {
                    return Ok(done); // it completed just before the cancel
                }
                // SAFETY: as above.
                let code = unsafe { GetLastError() };
                if waited == WAIT_TIMEOUT && code == ERROR_OPERATION_ABORTED {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "pipe read timed out"));
                }
                return Err(io::Error::from_raw_os_error(code as i32));
            }
        }
        let mut done = 0u32;
        // SAFETY: the operation has completed (synchronously or signaled); `ov` is its
        // OVERLAPPED.
        if unsafe { GetOverlappedResult(self.handle, &ov, &mut done, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(done)
    }

    /// Waits (forever) for a client on this server instance.
    fn accept_client(&mut self) -> io::Result<()> {
        // SAFETY: `handle` is this unconnected server instance; `ov` is valid for the
        // operation (awaited inside `overlapped`).
        self.overlapped(INFINITE, |handle, ov| unsafe { ConnectNamedPipe(handle, ov) })
            .map(drop)
    }

    /// Returns the instance to the unconnected state after its client left.
    fn disconnect(&mut self) {
        // SAFETY: `handle` is a live server instance owned by `self`.
        unsafe { DisconnectNamedPipe(self.handle) };
    }
}

/// Opens the client end: overlapped, and identification-only so the pipe's server can never
/// act as the caller (SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION).
fn open_client(name: &[u16]) -> io::Result<HANDLE> {
    // SAFETY: `name` is NUL-terminated; the returned handle is checked below.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

impl Drop for LocalStream {
    fn drop(&mut self) {
        // SAFETY: both handles are live and owned by `self`; nothing is pending on them
        // (`overlapped` awaits every operation it starts).
        unsafe {
            CloseHandle(self.handle);
            CloseHandle(self.event);
        }
    }
}

impl io::Read for LocalStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let wait = pipe_rules::timeout_millis(self.timeout);
        let ptr = buf.as_mut_ptr();
        // SAFETY: `buf` is writable for `len` bytes and outlives the operation (awaited inside
        // `overlapped`). A byte-mode pipe read completes as soon as any data is available.
        let read = self.overlapped(wait, |handle, ov| unsafe {
            ReadFile(handle, ptr, len, std::ptr::null_mut(), ov)
        });
        match read {
            Ok(n) => Ok(n as usize),
            // The peer closed: a clean EOF.
            Err(err) if pipe_rules::read_error_is_eof(err.raw_os_error()) => Ok(0),
            Err(err) => Err(err),
        }
    }
}

impl io::Write for LocalStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let ptr = buf.as_ptr();
        // SAFETY: `buf` is readable for `len` bytes and outlives the operation (awaited inside
        // `overlapped`).
        let written = self.overlapped(INFINITE, |handle, ov| unsafe {
            WriteFile(handle, ptr, len, std::ptr::null_mut(), ov)
        })?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Creates the pipe and accepts connections in the background. There is no clean shutdown
/// (the listener threads live for the process, as the WebSocket accept thread does); the
/// handles close at process exit.
pub(crate) fn start(
    token: String,
    _dir: &Path,
    board: Option<RenderOutBoard>,
) -> io::Result<Option<LocalServer>> {
    let address = pipe_address();
    // The first instance must create the name: if another process already holds it, fail
    // here (the app keeps its WebSocket listener) rather than join that process's pipe.
    let first = LocalStream::create_instance(&address, true)?;
    let (tx, rx) = bounded(INBOX_CAPACITY);
    let live = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("regia-agent-local-accept".into())
        .spawn({
            let address = address.clone();
            move || accept_loop(first, token, address, tx, live, board)
        })?;
    Ok(Some(LocalServer {
        address: address.clone(),
        inbox: rx,
        _guard: Guard {
            _registration: local::Registration::new(address, Identity),
        },
    }))
}

/// Accepts clients forever. A failed connect never ends the loop (sprint-rev-kimi-9 finding
/// 5): a client that left before the connect is cleared and the instance reused; other
/// failures replace the instance — the new one is made before the old one closes, so the name
/// never lapses while the server runs — with back-off.
fn accept_loop(
    first: LocalStream,
    token: String,
    address: String,
    tx: crossbeam_channel::Sender<Inbound>,
    live: Arc<AtomicUsize>,
    board: Option<RenderOutBoard>,
) {
    let mut pending = Some(first);
    let mut failures = 0u32;
    loop {
        let mut instance = match pending.take() {
            Some(instance) => instance,
            None => match LocalStream::create_instance(&address, false) {
                Ok(instance) => instance,
                Err(err) => {
                    failures = failures.saturating_add(1);
                    tracing::warn!(target: "regia_agent", "cannot create a pipe instance of {address}: {err}");
                    if let Some(pause) = pipe_rules::accept_backoff(failures) {
                        std::thread::sleep(pause);
                    }
                    continue;
                }
            },
        };
        let result = instance.accept_client();
        match pipe_rules::connect_step(result.as_ref().err().and_then(io::Error::raw_os_error)) {
            ConnectStep::Serve => {}
            ConnectStep::Reuse => {
                instance.disconnect();
                pending = Some(instance);
                continue;
            }
            ConnectStep::Replace => {
                failures = failures.saturating_add(1);
                if let Err(err) = &result {
                    tracing::warn!(target: "regia_agent", "local accept on {address} failed: {err}");
                }
                if let Some(pause) = pipe_rules::accept_backoff(failures) {
                    std::thread::sleep(pause);
                }
                // The replacement first, then the old instance closes (on drop).
                match LocalStream::create_instance(&address, false) {
                    Ok(next) => pending = Some(next),
                    Err(_) => {
                        instance.disconnect();
                        pending = Some(instance);
                    }
                }
                continue;
            }
        }
        failures = 0;
        // The next instance before this one is handed off, so a client arriving now finds one.
        match LocalStream::create_instance(&address, false) {
            Ok(next) => pending = Some(next),
            Err(err) => tracing::warn!(target: "regia_agent", "cannot create the next pipe instance: {err}"),
        }
        let Some(slot) = ConnectionSlot::take(&live) else {
            tracing::warn!(target: "regia_agent", "local connection refused: {} already open", server::MAX_CONNECTIONS);
            instance.disconnect();
            continue; // dropping the instance closes it
        };
        let (tx, token, board) = (tx.clone(), token.clone(), board.clone());
        let spawned = std::thread::Builder::new()
            .name("regia-agent-local-conn".into())
            .spawn(move || {
                let _slot = slot;
                // serve_connection owns the instance; its Drop closes the handle.
                local::serve_connection(instance, &token, &tx, board.as_ref());
            });
        if let Err(err) = spawned {
            tracing::warn!(target: "regia_agent", "cannot start local connection thread: {err}");
        }
    }
}
