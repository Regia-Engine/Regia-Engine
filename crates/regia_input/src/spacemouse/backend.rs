// SPDX-License-Identifier: MPL-2.0
//! Where SpaceMouse samples come from: the [`SpaceMouseBackend`] trait, the real HID backend
//! (a reader thread per device over `regia_platform::hid`) and [`FakeSpaceMouse`] for tests.
//!
//! A backend only reports events; the plugin keeps the per-device state, merges it and applies
//! the config. Every backend must tolerate missing, unreadable or busy devices (§ 2.6): it
//! reports them as [`BackendEvent::Unavailable`] and keeps running.

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use regia_app::prelude::*;

use super::SpaceMouseState;
use super::report::{self, DeviceDecoder};

/// Identity of one connected device.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpaceMouseDeviceInfo {
    /// Backend-assigned id, unique for the life of the app (a reconnect gets a new id).
    pub id: u64,
    /// The product name the device reports.
    pub name: String,
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id (0 when the source does not report it).
    pub product_id: u16,
    /// Where the samples come from: `"spacenavd"` (Linux daemon socket), `"hid"` (raw HID) or
    /// `"fake"` (tests).
    pub source: String,
}

/// What a backend reports.
#[derive(Debug, Clone, PartialEq)]
pub enum BackendEvent {
    /// A device was opened and will send samples.
    Connected(SpaceMouseDeviceInfo),
    /// A device went away (unplugged or failed).
    Disconnected {
        /// The id from [`BackendEvent::Connected`].
        id: u64,
    },
    /// The latest full state of one device (spec frame, normalised).
    Sample {
        /// The id from [`BackendEvent::Connected`].
        id: u64,
        /// The device's state.
        state: SpaceMouseState,
    },
    /// A SpaceMouse is present but cannot be opened (§ 2.6).
    Unavailable {
        /// The product name, when known.
        name: String,
        /// Why: shown by `input.spacemouse.status`.
        reason: String,
        /// True when the cause is a missing OS permission (Linux: the hidraw udev rule).
        permission: bool,
    },
}

/// A source of SpaceMouse events, polled once per frame by the plugin.
pub trait SpaceMouseBackend: Send + 'static {
    /// A short name for diagnostics (`"hid"`, `"fake"`).
    fn name(&self) -> &'static str;
    /// Appends every event since the last poll to `out`. Must not block.
    fn poll(&mut self, out: &mut Vec<BackendEvent>);
}

/// A scripted backend for tests (hot-plug without hardware): push events through the handle,
/// install [`FakeSpaceMouse::backend`] in the app.
///
/// ```
/// use regia_input::spacemouse::{FakeSpaceMouse, SpaceMouseState};
///
/// let fake = FakeSpaceMouse::default();
/// let backend = fake.backend(); // install with SpaceMouseBackendSlot::new(backend)
/// fake.connect(1, "SpaceMouse Compact");
/// fake.sample(1, SpaceMouseState { translation: [0.5, 0.0, 0.0], ..Default::default() });
/// fake.disconnect(1);
/// # drop(backend);
/// ```
#[derive(Debug, Clone, Default)]
pub struct FakeSpaceMouse {
    queue: Arc<Mutex<VecDeque<BackendEvent>>>,
}

impl FakeSpaceMouse {
    fn push(&self, event: BackendEvent) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(event);
    }

    /// The backend half, reading what this handle pushes.
    pub fn backend(&self) -> Box<dyn SpaceMouseBackend> {
        Box::new(FakeBackend {
            queue: Arc::clone(&self.queue),
        })
    }

    /// A device `id` appears (vendor 3Dconnexion, product 0).
    pub fn connect(&self, id: u64, name: &str) {
        self.push(BackendEvent::Connected(SpaceMouseDeviceInfo {
            id,
            name: name.to_string(),
            vendor_id: report::VENDOR_3DCONNEXION,
            product_id: 0,
            source: "fake".to_string(),
        }));
    }

    /// Device `id` is unplugged.
    pub fn disconnect(&self, id: u64) {
        self.push(BackendEvent::Disconnected { id });
    }

    /// Device `id` reports `state`.
    pub fn sample(&self, id: u64, state: SpaceMouseState) {
        self.push(BackendEvent::Sample { id, state });
    }

    /// A device is present but cannot be opened.
    pub fn unavailable(&self, name: &str, reason: &str, permission: bool) {
        self.push(BackendEvent::Unavailable {
            name: name.to_string(),
            reason: reason.to_string(),
            permission,
        });
    }
}

struct FakeBackend {
    queue: Arc<Mutex<VecDeque<BackendEvent>>>,
}

impl SpaceMouseBackend for FakeBackend {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn poll(&mut self, out: &mut Vec<BackendEvent>) {
        out.extend(
            self.queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .drain(..),
        );
    }
}

/// Called by the HID backend's threads after every new sample or hot-plug event, so a host whose
/// event loop sleeps between OS events (the editor) can wake up and run a frame. Insert it as a
/// resource before the first update; without it the samples wait for the next frame.
#[derive(Resource, Clone)]
pub struct SpaceMouseWaker(pub Arc<dyn Fn() + Send + Sync>);

impl SpaceMouseWaker {
    /// A waker calling `wake`.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(wake))
    }
}

impl std::fmt::Debug for SpaceMouseWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpaceMouseWaker")
    }
}

/// How often the HID backend looks for new devices.
const SCAN_INTERVAL: Duration = Duration::from_secs(1);
/// Largest input report read (SpaceMouse reports are 7 to 13 bytes).
const REPORT_BUFFER: usize = 256;

/// The real backend: a scanner thread finds SpaceMouse HID interfaces (hot-plug by rescanning
/// every second) and starts one blocking reader thread per device. Dropping the backend stops
/// the scanner and closes the spacenavd connection (its reader ends at once); a raw HID reader
/// ends with its device or at its next report, changed or not (the blocking HID read itself
/// cannot be interrupted yet).
pub(crate) struct HidBackend {
    events: Receiver<BackendEvent>,
    stop: Arc<AtomicBool>,
    /// The spacenavd connection while one is up, shut down on drop to end its reader.
    #[cfg(target_os = "linux")]
    spnav: spnav::Connection,
}

impl HidBackend {
    /// Starts the scanner thread. `None` when the thread cannot be spawned.
    pub(crate) fn start(waker: Option<SpaceMouseWaker>) -> Option<Self> {
        let (tx, events) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let scanner_stop = Arc::clone(&stop);
        #[cfg(target_os = "linux")]
        let spnav = spnav::Connection::default();
        #[cfg(target_os = "linux")]
        let scanner_spnav = spnav.clone();
        std::thread::Builder::new()
            .name("regia-spacemouse".into())
            .spawn(move || {
                scan_loop(
                    &Notifier { tx, waker },
                    &scanner_stop,
                    #[cfg(target_os = "linux")]
                    &scanner_spnav,
                );
            })
            .map_err(|err| {
                tracing::warn!(target: "regia_input", "SpaceMouse backend not started: {err}");
            })
            .ok()?;
        Some(Self {
            events,
            stop,
            #[cfg(target_os = "linux")]
            spnav,
        })
    }
}

impl Drop for HidBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        #[cfg(target_os = "linux")]
        self.spnav.close();
    }
}

impl SpaceMouseBackend for HidBackend {
    fn name(&self) -> &'static str {
        "hid"
    }

    fn poll(&mut self, out: &mut Vec<BackendEvent>) {
        out.extend(self.events.try_iter());
    }
}

/// The channel to the plugin plus the optional host waker.
#[derive(Clone)]
pub(crate) struct Notifier {
    tx: Sender<BackendEvent>,
    waker: Option<SpaceMouseWaker>,
}

impl Notifier {
    /// Sends `event` and wakes the host; false once the plugin is gone.
    fn send(&self, event: BackendEvent) -> bool {
        let sent = self.tx.send(event).is_ok();
        if sent && let Some(waker) = &self.waker {
            (waker.0)();
        }
        sent
    }
}

fn is_candidate(info: &regia_platform::hid::HidDeviceInfo) -> bool {
    let multi_axis = match (&info.report_descriptor, info.usage) {
        (Some(descriptor), _) => Some(report::declares_multi_axis(descriptor)),
        // Windows reports the top-level collection: Generic Desktop / Multi-axis Controller.
        (None, Some(usage)) => Some(usage == (0x01, 0x08)),
        (None, None) => None,
    };
    report::is_spacemouse(info.vendor_id, info.product_id, multi_axis)
}

/// Device ids, unique for the process.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn scan_loop(
    tx: &Notifier,
    stop: &Arc<AtomicBool>,
    #[cfg(target_os = "linux")] connection: &spnav::Connection,
) {
    use regia_platform::hid::{self, HidDevice, HidError};

    let open: Arc<Mutex<BTreeSet<String>>> = Arc::default();
    let mut reported: BTreeSet<String> = BTreeSet::new();
    #[cfg(target_os = "linux")]
    let spnav_active = Arc::new(AtomicBool::new(false));
    while !stop.load(Ordering::Acquire) {
        // Linux: the spacenavd socket first (§ 3: no permissions needed, shares the device with
        // other apps); while it is connected, the HID scan is skipped and the HID readers stop
        // (see `read_loop`), so one device is never merged twice.
        #[cfg(target_os = "linux")]
        if spnav::attach(tx, &spnav_active, connection, stop) {
            if !idle(stop) {
                return;
            }
            continue;
        }
        let devices = match hid::enumerate(|vendor| {
            vendor == report::VENDOR_3DCONNEXION || vendor == report::VENDOR_LOGITECH
        }) {
            Ok(devices) => devices,
            Err(HidError::Unsupported) => return,
            Err(err) => {
                tracing::debug!(target: "regia_input", "SpaceMouse scan failed: {err}");
                Vec::new()
            }
        };
        for info in devices.into_iter().filter(is_candidate) {
            if open
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&info.path)
            {
                continue;
            }
            match HidDevice::open(&info.path) {
                Ok(mut device) => {
                    reported.remove(&info.path);
                    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                    let name = if info.product.is_empty() {
                        "SpaceMouse".to_string()
                    } else {
                        info.product.clone()
                    };
                    let connected = BackendEvent::Connected(SpaceMouseDeviceInfo {
                        id,
                        name,
                        vendor_id: info.vendor_id,
                        product_id: info.product_id,
                        source: "hid".to_string(),
                    });
                    if !tx.send(connected) {
                        return;
                    }
                    open.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(info.path.clone());
                    let decoder = DeviceDecoder::for_device(
                        info.report_descriptor.as_deref(),
                        info.vendor_id,
                        info.product_id,
                    );
                    let (reader_tx, reader_open, path) = (tx.clone(), Arc::clone(&open), info.path.clone());
                    let reader_stop = Arc::clone(stop);
                    #[cfg(target_os = "linux")]
                    let reader_spnav = Arc::clone(&spnav_active);
                    let halt = move || {
                        #[cfg(target_os = "linux")]
                        if reader_spnav.load(Ordering::Acquire) {
                            return true;
                        }
                        reader_stop.load(Ordering::Acquire)
                    };
                    let spawned = std::thread::Builder::new()
                        .name(format!("regia-spacemouse-{id}"))
                        .spawn(move || {
                            read_loop(
                                id,
                                |buf| device.read(buf).map_err(|err| err.to_string()),
                                decoder,
                                &reader_tx,
                                halt,
                            );
                            reader_open
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .remove(&path);
                        });
                    if spawned.is_err() {
                        // Thread exhaustion: forget the device; the next scan retries it.
                        open.lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(&info.path);
                        tx.send(BackendEvent::Disconnected { id });
                    }
                }
                Err(err) => {
                    // Report each unopenable device once until it opens or disappears.
                    if reported.insert(info.path.clone()) {
                        let event = BackendEvent::Unavailable {
                            name: info.product.clone(),
                            permission: matches!(err, HidError::PermissionDenied(_)),
                            reason: err.to_string(),
                        };
                        if !tx.send(event) {
                            return;
                        }
                    }
                }
            }
        }
        if !idle(stop) {
            return;
        }
    }
}

/// Sleeps one scan interval in short slices; false when the backend was dropped meanwhile.
fn idle(stop: &AtomicBool) -> bool {
    let steps = (SCAN_INTERVAL.as_millis() / 100).max(1);
    for _ in 0..steps {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !stop.load(Ordering::Relaxed)
}

/// The spacenavd client (Linux): the daemon's original socket protocol, written from its public
/// protocol notes (no libspnav code). Each event is eight native-endian `i32`s: type (0 motion,
/// 1 button press, 2 button release), then `x y z rx ry rz period` for motion or the 0-based
/// button number. Motion arrives already scaled (about ±350 at full deflection at the default
/// sensitivity) in spacenavd's frame: +x right, +y up, +z away from the user (left-handed).
#[cfg(target_os = "linux")]
pub(crate) mod spnav {
    use std::io::Read;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use super::{BackendEvent, NEXT_ID, Notifier, SpaceMouseDeviceInfo};
    use crate::spacemouse::SpaceMouseState;
    use crate::spacemouse::report::VENDOR_3DCONNEXION;

    /// Where spacenavd listens.
    pub(crate) const SOCKETS: [&str; 2] = ["/run/spnav.sock", "/var/run/spnav.sock"];
    /// Bytes per event.
    pub(crate) const EVENT_BYTES: usize = 32;

    /// One decoded spacenavd event.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Event {
        /// `x y z rx ry rz` in spacenavd's frame.
        Motion([i32; 6]),
        /// Button `index` (0-based) pressed or released.
        Button { index: u32, pressed: bool },
    }

    /// Decodes one 32-byte event; `None` for an unknown type.
    pub(crate) fn parse(bytes: &[u8; EVENT_BYTES]) -> Option<Event> {
        let word = |i: usize| {
            i32::from_ne_bytes([bytes[i * 4], bytes[i * 4 + 1], bytes[i * 4 + 2], bytes[i * 4 + 3]])
        };
        match word(0) {
            0 => Some(Event::Motion([
                word(1),
                word(2),
                word(3),
                word(4),
                word(5),
                word(6),
            ])),
            kind @ (1 | 2) => Some(Event::Button {
                index: u32::try_from(word(1)).ok()?,
                pressed: kind == 1,
            }),
            _ => None,
        }
    }

    /// spacenavd's frame (+z away, left-handed rotations) to the spec frame (§ 2.1).
    pub(crate) fn to_spec(motion: [i32; 6]) -> ([i32; 3], [i32; 3]) {
        let [x, y, z, rx, ry, rz] = motion;
        // Left-handed about +x tips the top away (spec +rx); about +y turns clockwise seen from
        // above (spec −ry); about +z (into the screen) rolls counter-clockwise as seen (spec +rz).
        ([x, y, z.saturating_neg()], [rx, ry.saturating_neg(), rz])
    }

    /// Applies one event to a running state; true when it changed.
    pub(crate) fn apply(event: Event, raw: &mut ([i32; 3], [i32; 3], u32)) -> bool {
        let before = *raw;
        match event {
            Event::Motion(motion) => {
                let (t, r) = to_spec(motion);
                raw.0 = t;
                raw.1 = r;
            }
            Event::Button { index, pressed } if index < 32 => {
                if pressed {
                    raw.2 |= 1 << index;
                } else {
                    raw.2 &= !(1 << index);
                }
            }
            Event::Button { .. } => {}
        }
        *raw != before
    }

    /// The live spacenavd connection, shared with the backend so dropping it can shut the
    /// socket down: the blocked reader then returns at once instead of holding the connection.
    #[derive(Debug, Clone, Default)]
    pub(crate) struct Connection(Arc<Mutex<Option<UnixStream>>>);

    impl Connection {
        /// Keeps a handle on `stream` (a clone of the reader's socket).
        fn set(&self, stream: Option<UnixStream>) {
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) = stream;
        }

        /// Shuts the connection down, ending its reader.
        pub(crate) fn close(&self) {
            if let Some(stream) = self.0.lock().unwrap_or_else(PoisonError::into_inner).take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }

    /// Connects to spacenavd when it is not connected yet and starts the reader; true while a
    /// connection is up (the caller then skips the HID scan). The device is reported connected
    /// with the daemon's first event, so a daemon running without a device shows nothing.
    pub(crate) fn attach(
        tx: &Notifier,
        active: &Arc<AtomicBool>,
        connection: &Connection,
        stop: &AtomicBool,
    ) -> bool {
        if active.load(Ordering::Acquire) {
            return true;
        }
        let Some(stream) = SOCKETS.iter().find_map(|path| UnixStream::connect(path).ok()) else {
            return false;
        };
        connection.set(stream.try_clone().ok());
        // The backend may have been dropped while this connected: do not leave a reader behind.
        if stop.load(Ordering::Acquire) {
            connection.close();
            return false;
        }
        active.store(true, Ordering::Release);
        let (reader_tx, reader_active) = (tx.clone(), Arc::clone(active));
        let spawned = std::thread::Builder::new()
            .name("regia-spacenavd".into())
            .spawn(move || {
                read_loop(stream, &reader_tx);
                reader_active.store(false, Ordering::Release);
            });
        if let Err(err) = spawned {
            tracing::warn!(target: "regia_input", "spacenavd reader not started: {err}");
            active.store(false, Ordering::Release);
            connection.close();
            return false;
        }
        true
    }

    fn read_loop(mut stream: UnixStream, tx: &Notifier) {
        let mut raw = ([0; 3], [0; 3], 0u32);
        let mut buf = [0u8; EVENT_BYTES];
        let mut id = None;
        loop {
            if let Err(err) = stream.read_exact(&mut buf) {
                tracing::debug!(target: "regia_input", "spacenavd connection closed: {err}");
                if let Some(id) = id {
                    tx.send(BackendEvent::Disconnected { id });
                }
                return;
            }
            let Some(event) = parse(&buf) else { continue };
            let id = match id {
                Some(id) => id,
                None => {
                    let new = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                    let connected = BackendEvent::Connected(SpaceMouseDeviceInfo {
                        id: new,
                        name: "SpaceMouse (spacenavd)".to_string(),
                        vendor_id: VENDOR_3DCONNEXION,
                        product_id: 0,
                        source: "spacenavd".to_string(),
                    });
                    if !tx.send(connected) {
                        return;
                    }
                    *id.insert(new)
                }
            };
            if apply(event, &mut raw)
                && !tx.send(BackendEvent::Sample {
                    id,
                    state: SpaceMouseState::from_raw(raw.0, raw.1, raw.2),
                })
            {
                return;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn to_spec_survives_i32_min() {
            let (t, r) = to_spec([0, 0, i32::MIN, 0, i32::MIN, 0]);
            assert_eq!((t[2], r[1]), (i32::MAX, i32::MAX));
        }

        fn event(words: [i32; 8]) -> [u8; EVENT_BYTES] {
            let mut out = [0u8; EVENT_BYTES];
            for (i, w) in words.iter().enumerate() {
                out[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
            }
            out
        }

        #[test]
        fn motion_and_buttons_decode_to_the_spec_frame() {
            let mut raw = ([0; 3], [0; 3], 0u32);
            // Push right, lift, push away; tilt away a little, twist clockwise, tilt right (spacenavd frame).
            let motion = parse(&event([0, 350, 175, 350, 35, 350, -350, 8])).expect("motion");
            assert!(apply(motion, &mut raw));
            let s = SpaceMouseState::from_raw(raw.0, raw.1, raw.2);
            assert_eq!(s.translation, [1.0, 0.5, -1.0], "push away = spec −z");
            assert_eq!(s.rotation, [0.1, -1.0, -1.0]);
            assert!(apply(
                parse(&event([1, 1, 0, 0, 0, 0, 0, 0])).expect("press"),
                &mut raw
            ));
            assert_eq!(raw.2, 0b10, "0-based button 1 = spec button 2");
            assert!(apply(
                parse(&event([2, 1, 0, 0, 0, 0, 0, 0])).expect("release"),
                &mut raw
            ));
            assert_eq!(raw.2, 0);
            assert_eq!(parse(&event([7, 0, 0, 0, 0, 0, 0, 0])), None, "unknown type");
            assert_eq!(parse(&event([1, -1, 0, 0, 0, 0, 0, 0])), None, "bad button");
        }
    }
}

/// One HID device's reader: decodes every report and sends changed states. Ends (reporting the
/// device disconnected) when the read fails, when the plugin is gone, or at the first report
/// after `halt` turns true (the backend was dropped, or spacenavd took the device over: § 3,
/// the backends are exclusive).
fn read_loop(
    id: u64,
    mut read: impl FnMut(&mut [u8]) -> Result<usize, String>,
    mut decoder: DeviceDecoder,
    tx: &Notifier,
    halt: impl Fn() -> bool,
) {
    let mut buf = [0u8; REPORT_BUFFER];
    loop {
        match read(&mut buf) {
            Ok(_) if halt() => {
                tx.send(BackendEvent::Disconnected { id });
                return;
            }
            Ok(len) => {
                let len = len.min(buf.len());
                if decoder.feed(&buf[..len])
                    && !tx.send(BackendEvent::Sample {
                        id,
                        state: decoder.state(),
                    })
                {
                    return;
                }
            }
            Err(err) => {
                tracing::debug!(target: "regia_input", "SpaceMouse {id} closed: {err}");
                tx.send(BackendEvent::Disconnected { id });
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notifier() -> (Notifier, Receiver<BackendEvent>) {
        let (tx, rx) = channel();
        (Notifier { tx, waker: None }, rx)
    }

    /// A raw HID reader stops at its next report once halted (backend dropped, or spacenavd took
    /// the device over), changed report or not, and reports the device gone; before, it only
    /// noticed a dropped backend on a *changed* report and never noticed spacenavd at all, so
    /// one device was merged twice (concurrency audit 11-03 M1, H2).
    #[test]
    fn a_halted_reader_ends_at_its_next_report() {
        let (tx, rx) = notifier();
        let reads = std::cell::Cell::new(0u32);
        read_loop(
            5,
            |buf: &mut [u8]| {
                reads.set(reads.get() + 1);
                buf[0] = 0;
                Ok(1)
            },
            DeviceDecoder::new(None, 0),
            &tx,
            || reads.get() >= 3,
        );
        assert_eq!(reads.get(), 3, "the reader kept reading after the halt");
        let events: Vec<BackendEvent> = rx.try_iter().collect();
        assert_eq!(events.last(), Some(&BackendEvent::Disconnected { id: 5 }));
    }

    #[test]
    fn a_failed_read_reports_the_device_gone() {
        let (tx, rx) = notifier();
        read_loop(
            6,
            |_: &mut [u8]| Err("gone".to_string()),
            DeviceDecoder::new(None, 0),
            &tx,
            || false,
        );
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![BackendEvent::Disconnected { id: 6 }]
        );
    }
}
