// SPDX-License-Identifier: MPL-2.0
//! Where cell bytes come from (spec 78 § 9): the [`CellSource`] seam, tickets, cancel tokens and the
//! built-in sources.
//!
//! A source gets a request and answers through the [`CellSink`] it was handed, from any thread. The
//! sink decodes on the async compute pool, never on the main thread, and checks the cancel token
//! before and after, so a cancelled result is never published. A sink dropped without an answer
//! reports the read as abandoned, so a source bug cannot leak an in-flight slot.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crossbeam_channel::Sender;
use regia_asset::{AssetId, PakPayload, PayloadReader};
use regia_scene::level::LevelId;
use regia_scene::stream_grid::CellCoord;

use crate::format::CellBlob;

/// One load of one cell. A result whose generation is not the cell's current one is stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ticket {
    /// The level.
    pub level: LevelId,
    /// The cell.
    pub cell: CellCoord,
    /// Counts up each time the cell is queued.
    pub generation: u64,
}

/// Tells a read and its decode to stop. Reads already inside the OS finish and are dropped.
///
/// Clones share one flag, so the scheduler keeps a clone and the reader checks its own:
///
/// ```
/// use regia_world::CancelToken;
///
/// let token = CancelToken::new();
/// let reader_copy = token.clone();
/// assert!(!reader_copy.is_cancelled());
/// token.cancel();
/// assert!(reader_copy.is_cancelled());
/// ```
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the work to stop.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether the work should stop.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Why a read failed. The spelling of [`Self::reason`] is the cell failure reason of spec 78 § 17.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellReadError {
    /// The blob is not in any mount or cache.
    Missing,
    /// The blob's chunk is not mounted (spec 40).
    Unavailable(String),
    /// The blob is there and could not be read (an unreadable file, a pak entry that does not
    /// decompress): reported as `cell_corrupt`, not as a miss.
    Io(String),
}

impl CellReadError {
    /// The failure reason.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Missing => "cell_missing",
            Self::Io(_) => "cell_corrupt",
            Self::Unavailable(_) => "cell_unavailable",
        }
    }
}

/// What came back for a ticket.
#[derive(Debug)]
pub(crate) enum CellOutcome {
    /// The cell decoded.
    Ready {
        /// The decoded blob.
        blob: Arc<CellBlob>,
        /// File bytes read, the measured size.
        bytes: u64,
    },
    /// It failed with this reason.
    Failed(&'static str),
    /// It was cancelled, or the source dropped the request.
    Abandoned,
}

/// A finished ticket.
#[derive(Debug)]
pub(crate) struct CellResult {
    pub(crate) ticket: Ticket,
    pub(crate) outcome: CellOutcome,
}

/// Where a [`CellSource`] delivers one read. Cloneable sources keep it until the read finishes.
#[derive(Debug)]
pub struct CellSink {
    tx: Sender<CellResult>,
    ticket: Ticket,
    id: AssetId,
    hash: [u8; 32],
    cancel: CancelToken,
    answered: bool,
    #[cfg(test)]
    decode_barrier: Option<tests::DecodeBarrier>,
}

impl CellSink {
    pub(crate) fn new(
        tx: Sender<CellResult>,
        ticket: Ticket,
        id: AssetId,
        hash: [u8; 32],
        cancel: CancelToken,
    ) -> Self {
        Self {
            tx,
            ticket,
            id,
            hash,
            cancel,
            answered: false,
            #[cfg(test)]
            decode_barrier: None,
        }
    }

    fn send(&mut self, outcome: CellOutcome) {
        self.answered = true;
        // The runtime holds the receiver for as long as it can issue reads; a closed channel only
        // means the level is gone.
        let _ = self.tx.send(CellResult {
            ticket: self.ticket,
            outcome,
        });
    }

    /// Delivers the blob's bytes; decoding moves to the async compute pool.
    pub fn finish(mut self, bytes: PakPayload) {
        if self.cancel.is_cancelled() {
            self.send(CellOutcome::Abandoned);
            return;
        }
        self.answered = true;
        let (tx, ticket, id, hash, cancel) = (
            self.tx.clone(),
            self.ticket,
            self.id,
            self.hash,
            self.cancel.clone(),
        );
        #[cfg(test)]
        let barrier = self.decode_barrier.take();
        regia_core::tasks::spawn(async move {
            #[cfg(test)]
            if let Some(barrier) = &barrier {
                barrier.wait(tests::DecodePoint::BeforeDecode);
            }
            let outcome = decode(
                &bytes,
                id,
                &hash,
                &cancel,
                #[cfg(test)]
                barrier.as_ref(),
            );
            let _ = tx.send(CellResult { ticket, outcome });
        })
        .detach();
    }

    /// Delivers a read failure.
    pub fn fail(mut self, error: CellReadError) {
        tracing::debug!(target: "regia_world", "cell {}: read failed: {error:?}", self.ticket.cell);
        self.send(CellOutcome::Failed(error.reason()));
    }

    /// Says the read stopped because it was cancelled.
    pub fn abandon(mut self) {
        self.send(CellOutcome::Abandoned);
    }
}

impl Drop for CellSink {
    fn drop(&mut self) {
        if !self.answered {
            self.send(CellOutcome::Abandoned);
        }
    }
}

fn decode(
    bytes: &[u8],
    id: AssetId,
    hash: &[u8; 32],
    cancel: &CancelToken,
    #[cfg(test)] barrier: Option<&tests::DecodeBarrier>,
) -> CellOutcome {
    if cancel.is_cancelled() {
        return CellOutcome::Abandoned;
    }
    let _span = tracing::trace_span!("stream_decode", bytes = bytes.len()).entered();
    let outcome = match CellBlob::from_bytes(bytes, id, hash) {
        Ok(blob) => CellOutcome::Ready {
            blob: Arc::new(blob),
            bytes: bytes.len() as u64,
        },
        Err(error) => {
            tracing::debug!(target: "regia_world", "cell blob {id:?}: {error}");
            CellOutcome::Failed(error.cell_reason())
        }
    };
    #[cfg(test)]
    if let Some(barrier) = barrier {
        barrier.wait(tests::DecodePoint::BeforePublish);
    }
    if cancel.is_cancelled() {
        return CellOutcome::Abandoned;
    }
    outcome
}

/// Reads cell blobs. `read` must not block the caller: completion arrives through the sink.
pub trait CellSource: Send + Sync + 'static {
    /// Starts reading one blob. Check `cancel` before starting real work.
    fn read(&self, id: AssetId, ticket: Ticket, cancel: CancelToken, sink: CellSink);

    /// One fixed step passed. Only sources that count delays in steps need this.
    fn on_step(&self, _step: u64) {}
}

/// Reads through a [`PayloadReader`] on the IO pool: mounted paks first, then the cooked cache, the
/// same precedence as the asset server. A cloned reader never blocks the main thread, which
/// `AssetServer::read_cooked` would.
pub struct PayloadCellSource {
    reader: PayloadReader,
}

impl PayloadCellSource {
    /// A source over `reader` (`AssetServer::payload_reader`).
    pub fn new(reader: PayloadReader) -> Self {
        Self { reader }
    }
}

impl CellSource for PayloadCellSource {
    fn read(&self, id: AssetId, _ticket: Ticket, cancel: CancelToken, sink: CellSink) {
        let reader = self.reader.clone();
        regia_core::tasks::spawn_io(async move {
            if cancel.is_cancelled() {
                sink.abandon();
                return;
            }
            match reader.read(id) {
                Ok((bytes, _)) => sink.finish(bytes),
                Err(error) => {
                    tracing::debug!(target: "regia_world", "cell blob {id:?}: {error}");
                    sink.fail(payload_error(&error));
                }
            }
        })
        .detach();
    }
}

/// Sorts a payload read failure: no file is a miss; anything else means the blob is present and
/// unreadable.
fn payload_error(error: &regia_asset::AssetLoadError) -> CellReadError {
    match error {
        regia_asset::AssetLoadError::Cooked(regia_asset::CookedIoError::Io(io))
            if io.kind() == std::io::ErrorKind::NotFound =>
        {
            CellReadError::Missing
        }
        other => CellReadError::Io(other.to_string()),
    }
}

/// A read waiting for its step.
struct Delayed {
    id: AssetId,
    cancel: CancelToken,
    sink: CellSink,
}

#[derive(Default)]
struct MemoryInner {
    blobs: HashMap<AssetId, Vec<u8>>,
    errors: HashMap<AssetId, CellReadError>,
    delays: HashMap<AssetId, u64>,
    held: HashMap<AssetId, Vec<Delayed>>,
    waiting: BTreeMap<u64, Vec<Delayed>>,
    step: u64,
}

/// An in-memory source for tests, with injected failures and delays counted in steps, or held until
/// the test releases them.
#[derive(Clone, Default)]
pub struct MemoryCellSource {
    inner: Arc<Mutex<MemoryInner>>,
    reads: Arc<AtomicU64>,
    cancelled_before_read: Arc<AtomicU64>,
}

impl MemoryCellSource {
    /// An empty source. Fill it with [`Self::insert`] and hand it to
    /// `WorldStreaming::open_level` as an `Arc<dyn CellSource>`.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use regia_core::id::Guid;
    /// use regia_scene::level::LevelId;
    /// use regia_scene::stream_grid::CellCoord;
    /// use regia_world::format::cell_blob_id;
    /// use regia_world::{CellSource, MemoryCellSource};
    ///
    /// let source = MemoryCellSource::new();
    /// let id = cell_blob_id(LevelId(Guid::from_u128(7)), CellCoord::new(0, 0));
    /// source.insert(id, vec![1, 2, 3]);
    /// assert_eq!(source.reads(), 0);
    /// let _as_trait_object: Arc<dyn CellSource> = Arc::new(source);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, MemoryInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores a blob file.
    pub fn insert(&self, id: AssetId, bytes: Vec<u8>) {
        self.inner().blobs.insert(id, bytes);
    }

    /// Removes a blob file.
    pub fn remove(&self, id: AssetId) {
        self.inner().blobs.remove(&id);
    }

    /// Makes reads of `id` fail, or work again with `None`.
    pub fn set_error(&self, id: AssetId, error: Option<CellReadError>) {
        let mut inner = self.inner();
        match error {
            Some(error) => inner.errors.insert(id, error),
            None => inner.errors.remove(&id),
        };
    }

    /// Delays reads of `id` by this many steps.
    pub fn set_delay(&self, id: AssetId, steps: u64) {
        self.inner().delays.insert(id, steps);
    }

    /// Holds reads of `id` until [`Self::release`].
    pub fn hold(&self, id: AssetId) {
        self.inner().held.entry(id).or_default();
    }

    /// Lets the held reads of `id` finish.
    pub fn release(&self, id: AssetId) {
        let reads = self.inner().held.remove(&id).unwrap_or_default();
        for read in reads {
            self.answer(read);
        }
    }

    /// Reads that really started.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Reads skipped because their token was already cancelled.
    pub fn cancelled_before_read(&self) -> u64 {
        self.cancelled_before_read.load(Ordering::Relaxed)
    }

    fn answer(&self, read: Delayed) {
        let Delayed { id, cancel, sink } = read;
        if cancel.is_cancelled() {
            self.cancelled_before_read.fetch_add(1, Ordering::Relaxed);
            sink.abandon();
            return;
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        let result = {
            let inner = self.inner();
            match inner.errors.get(&id) {
                Some(error) => Err(error.clone()),
                None => inner.blobs.get(&id).cloned().ok_or(CellReadError::Missing),
            }
        };
        match result {
            Ok(bytes) => sink.finish(PakPayload::from(bytes)),
            Err(error) => sink.fail(error),
        }
    }
}

impl CellSource for MemoryCellSource {
    fn read(&self, id: AssetId, _ticket: Ticket, cancel: CancelToken, sink: CellSink) {
        let read = Delayed { id, cancel, sink };
        let mut inner = self.inner();
        if let Some(held) = inner.held.get_mut(&id) {
            held.push(read);
            return;
        }
        let delay = inner.delays.get(&id).copied().unwrap_or(0);
        if delay > 0 {
            let due = inner.step + delay;
            inner.waiting.entry(due).or_default().push(read);
            return;
        }
        drop(inner);
        self.answer(read);
    }

    fn on_step(&self, step: u64) {
        let due = {
            let mut inner = self.inner();
            inner.step = step;
            let later = inner.waiting.split_off(&(step + 1));
            std::mem::replace(&mut inner.waiting, later)
        };
        for read in due.into_values().flatten() {
            self.answer(read);
        }
    }
}

#[cfg(test)]
mod payload_error_tests {
    use super::*;
    use regia_asset::{AssetLoadError, CookedIoError};

    /// A missing file is a miss; an unreadable one is not reported as missing.
    #[test]
    fn payload_failures_are_sorted_into_missing_and_unreadable() {
        let missing = AssetLoadError::Cooked(CookedIoError::Io(std::io::Error::from(
            std::io::ErrorKind::NotFound,
        )));
        assert_eq!(payload_error(&missing).reason(), "cell_missing");
        let denied = AssetLoadError::Cooked(CookedIoError::Io(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )));
        assert_eq!(payload_error(&denied).reason(), "cell_corrupt");
        let bad = AssetLoadError::Cooked(CookedIoError::BadMagic);
        assert_eq!(payload_error(&bad).reason(), "cell_corrupt");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crossbeam_channel::{Receiver, RecvTimeoutError, bounded};
    use regia_core::id::Guid;
    use regia_ecs::prelude::{Messages, World};
    use regia_scene::cooked::{COOKED_SCENE_VERSION, CookedScene};
    use regia_scene::floating_origin::{FloatingOrigin, LargeSpace};
    use regia_scene::stream_plan::{SourceId, SourceView};
    use regia_scene::streaming::{
        CellFailed, CellLoaded, CellReady, CellUnloading, LevelStreaming, StreamBudgets, StreamTuning,
    };

    use crate::format::{
        CELL_BLOB_FORMAT_VERSION, CellRecord, WORLD_INDEX_FORMAT_VERSION, WorldIndex, cell_blob_id,
    };
    use crate::{CellState, TraceKind, WorldStreaming};

    // Timeouts diagnose broken handshakes; no assertion depends on execution speed or a race.
    const HANDSHAKE_LIMIT: Duration = Duration::from_secs(30);
    // Each history may hold one worker while the other cell decodes. Do not let two histories
    // occupy both workers of the documented two-thread test pool at the same time.
    static HISTORY_LOCK: Mutex<()> = Mutex::new(());
    const A: CellCoord = CellCoord::new(0, 0);
    const B: CellCoord = CellCoord::new(4, 0);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum DecodePoint {
        BeforeDecode,
        BeforePublish,
    }

    // Per-sink, never global: parallel tests and unrelated pool jobs cannot take this barrier.
    #[derive(Debug)]
    pub(super) struct DecodeBarrier {
        point: DecodePoint,
        entered: Sender<()>,
        release: Receiver<()>,
    }

    impl DecodeBarrier {
        pub(super) fn wait(&self, point: DecodePoint) {
            if self.point == point {
                self.entered
                    .send_timeout((), HANDSHAKE_LIMIT)
                    .expect("announce decode checkpoint");
                self.release
                    .recv_timeout(HANDSHAKE_LIMIT)
                    .expect("release decode checkpoint");
            }
        }
    }

    struct PausedDecode {
        entered: Receiver<()>,
        release: Sender<()>,
    }

    impl PausedDecode {
        fn install(sink: &mut CellSink, point: DecodePoint) -> Self {
            let (entered_tx, entered) = bounded(1);
            let (release, release_rx) = bounded(1);
            sink.decode_barrier = Some(DecodeBarrier {
                point,
                entered: entered_tx,
                release: release_rx,
            });
            Self { entered, release }
        }

        fn entered(&self) {
            self.entered
                .recv_timeout(HANDSHAKE_LIMIT)
                .expect("worker reached checkpoint");
        }

        fn release(self) {
            self.release
                .send_timeout((), HANDSHAKE_LIMIT)
                .expect("release worker");
        }
    }

    struct Request {
        ticket: Ticket,
        cancel: CancelToken,
        sink: CellSink,
    }

    struct ControlledSource(Sender<Request>);

    impl CellSource for ControlledSource {
        fn read(&self, _id: AssetId, ticket: Ticket, cancel: CancelToken, sink: CellSink) {
            self.0
                .send_timeout(Request { ticket, cancel, sink }, HANDSHAKE_LIMIT)
                .expect("capture read");
        }
    }

    // Observe the actual sink channel, then deliver its single result to the real runtime channel.
    // Waiting for disconnection proves the producer is done, so a delayed duplicate cannot escape
    // a try_recv check. Neither this observer nor the source creates another terminal outcome.
    struct Terminal {
        rx: Receiver<CellResult>,
        runtime_tx: Sender<CellResult>,
        ticket: Ticket,
    }

    impl Terminal {
        fn observe(request: &mut Request) -> Self {
            let (tx, rx) = bounded(4);
            let runtime_tx = std::mem::replace(&mut request.sink.tx, tx);
            Self {
                rx,
                runtime_tx,
                ticket: request.ticket,
            }
        }

        fn one(&self) -> CellResult {
            let result = self
                .rx
                .recv_timeout(HANDSHAKE_LIMIT)
                .expect("one terminal outcome");
            assert_eq!(result.ticket, self.ticket, "completion retains its exact ticket");
            assert_eq!(
                self.rx
                    .recv_timeout(HANDSHAKE_LIMIT)
                    .expect_err("at most one terminal outcome"),
                RecvTimeoutError::Disconnected,
                "the producer finished; there can be no late duplicate"
            );
            result
        }

        fn deliver(&self, result: CellResult, history: &mut History) {
            self.runtime_tx
                .send_timeout(result, HANDSHAKE_LIMIT)
                .expect("runtime receives completion");
            history.runtime.complete_async();
            history.apply();
        }
    }

    struct History {
        runtime: WorldStreaming,
        world: World,
        requests: Receiver<Request>,
        bytes: BTreeMap<CellCoord, Vec<u8>>,
    }

    fn level() -> LevelId {
        LevelId(Guid::from_u128(0xce11))
    }

    fn view(cell: CellCoord) -> SourceView {
        SourceView::at(
            SourceId::Guid(cell.x as u128 + 100),
            [cell.x as f64 * 100.0 + 50.0, 0.0, 50.0],
        )
    }

    impl History {
        fn new(max_inflight: u32) -> Self {
            // Documented async setup: init the global pools and let their workers run detached
            // jobs. block_on alone does not drive a pool (regia_core::tasks module docs).
            regia_core::tasks::init(regia_core::tasks::TaskPoolOptions {
                compute_threads: Some(1),
                async_compute_threads: Some(2),
                io_threads: Some(1),
                ..Default::default()
            });
            let mut bytes = BTreeMap::new();
            let mut cells = Vec::new();
            for coord in [A, B] {
                let id = cell_blob_id(level(), coord);
                let blob = CellBlob {
                    format_version: CELL_BLOB_FORMAT_VERSION,
                    coord,
                    anchor: [0; 3],
                    scene: CookedScene {
                        format_version: COOKED_SCENE_VERSION,
                        strings: vec![],
                        entities: vec![],
                        resources: vec![],
                    },
                    nav: None,
                };
                let data = blob.to_bytes(id, vec![]).expect("tiny valid cell blob");
                cells.push(CellRecord {
                    coord,
                    blob: id,
                    blob_hash: *blake3::hash(&data).as_bytes(),
                    entity_count: 0,
                    own_bytes: data.len() as u64,
                    dep_bytes: 0,
                    vram_bytes: 0,
                    min_y: 0.0,
                    max_y: 1.0,
                    priority: 0,
                    deps: vec![],
                    nav_tiles: 0,
                    has_terrain_collision: false,
                });
                bytes.insert(coord, data);
            }
            let index = WorldIndex {
                format_version: WORLD_INDEX_FORMAT_VERSION,
                level: level().0,
                content_hash: WorldIndex::hash_cells(&cells),
                cells,
                bounds: [A, B],
                settings: LevelStreaming {
                    enabled: true,
                    cell_size_m: 100.0,
                    load_radius_m: 20.0,
                    unload_radius_m: 30.0,
                    budgets: Some(StreamBudgets {
                        max_inflight,
                        max_loads_per_step: 0,
                        ..Default::default()
                    }),
                    tuning: StreamTuning {
                        max_retries: 0,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                hlod: None,
                replicated: vec![],
                nav: None,
            };
            let (tx, requests) = bounded(8);
            let mut runtime = WorldStreaming::default();
            runtime
                .open_level(level(), index, Arc::new(ControlledSource(tx)))
                .expect("open");
            let mut world = World::new();
            world.init_resource::<Messages<CellLoaded>>();
            world.init_resource::<Messages<CellReady>>();
            world.init_resource::<Messages<CellFailed>>();
            world.init_resource::<Messages<CellUnloading>>();
            Self {
                runtime,
                world,
                requests,
                bytes,
            }
        }

        fn plan(&mut self, cells: &[CellCoord]) {
            self.runtime.begin_step();
            self.runtime
                .plan_step(&cells.iter().copied().map(view).collect::<Vec<_>>(), &|_| None);
            self.apply();
        }

        fn apply(&mut self) {
            let origin =
                FloatingOrigin::new(LargeSpace::new(1024.0).expect("valid test space"), [0; 3], 512.0)
                    .expect("valid test origin");
            self.runtime.apply_step(&mut self.world, &origin);
        }

        fn request(&self, cell: CellCoord) -> Request {
            let request = self
                .requests
                .recv_timeout(HANDSHAKE_LIMIT)
                .expect("issued request");
            assert_eq!(request.ticket.cell, cell);
            request
        }

        fn state(&self, cell: CellCoord, expected: CellState) {
            assert_eq!(self.runtime.state(level(), cell), Some(expected));
        }

        fn accounting(&self, inflight: u32, pending: u64) {
            assert_eq!(self.runtime.stats().inflight, inflight, "slot ledger");
            assert_eq!(self.runtime.stats().pending_bytes, pending, "byte ledger");
            assert_eq!(self.runtime.stats().main_thread_io_waits, 0);
        }

        fn size(&self, cell: CellCoord) -> u64 {
            self.bytes[&cell].len() as u64
        }

        fn finish(&self, sink: CellSink, cell: CellCoord) {
            sink.finish(PakPayload::from(self.bytes[&cell].clone()));
        }

        fn applied(&self, cell: CellCoord) -> usize {
            self.runtime
                .trace()
                .filter(|e| e.cell == Some(cell) && e.kind == TraceKind::Applied)
                .count()
        }

        fn healthy(&mut self, cell: CellCoord) {
            let mut request = self.request(cell);
            let terminal = Terminal::observe(&mut request);
            self.finish(request.sink, cell);
            let result = terminal.one();
            match &result.outcome {
                CellOutcome::Ready { blob, bytes } => {
                    assert_eq!(blob.coord, cell);
                    assert_eq!(*bytes, self.size(cell));
                }
                other => panic!("healthy cell must decode: {other:?}"),
            }
            terminal.deliver(result, self);
            self.state(cell, CellState::Spawned);
            assert_eq!(self.applied(cell), 1);
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Action {
        CancelBeforeAnswer,
        DropUnanswered,
        Abandon,
        ReadFailure,
        HashMismatch,
        DecodeFailure,
    }

    #[test]
    fn cell_sink_terminal_histories_release_one_slot_and_the_next_cell_uses_it() {
        let _history = HISTORY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        for answer in [
            Action::CancelBeforeAnswer,
            Action::DropUnanswered,
            Action::Abandon,
            Action::ReadFailure,
            Action::HashMismatch,
            Action::DecodeFailure,
        ] {
            let mut h = History::new(1);
            h.plan(&[A]);
            let mut first = h.request(A);
            let terminal = Terminal::observe(&mut first);
            h.plan(&[A, B]);
            h.state(B, CellState::Queued);
            h.accounting(1, h.size(A));
            assert!(h.requests.is_empty(), "capacity is full: {answer:?}");
            if matches!(answer, Action::CancelBeforeAnswer) {
                h.plan(&[B]);
                assert!(first.cancel.is_cancelled());
                h.state(A, CellState::Unloaded);
            }
            match answer {
                Action::CancelBeforeAnswer => h.finish(first.sink, A),
                Action::DropUnanswered => drop(first.sink),
                Action::Abandon => first.sink.abandon(),
                Action::ReadFailure => first.sink.fail(CellReadError::Unavailable("test_chunk".into())),
                Action::HashMismatch => first.sink.finish(PakPayload::from(vec![0])),
                Action::DecodeFailure => {
                    // Keep the cooked header, id, hash and format version valid, but omit the
                    // blob's remaining fields so this reaches postcard rather than hash checking.
                    let (mut header, _) =
                        regia_asset::decode_cooked_file(&h.bytes[&A]).expect("valid fixture header");
                    let body = postcard::to_allocvec(&CELL_BLOB_FORMAT_VERSION).expect("version");
                    header.payload_len = body.len() as u64;
                    let malformed = regia_asset::encode_cooked_file(&header, &body)
                        .expect("valid header around incomplete cell");
                    first.sink.hash = *blake3::hash(&malformed).as_bytes();
                    first.sink.finish(PakPayload::from(malformed));
                }
            }
            let result = terminal.one();
            match (&answer, &result.outcome) {
                (
                    Action::CancelBeforeAnswer | Action::DropUnanswered | Action::Abandon,
                    CellOutcome::Abandoned,
                )
                | (Action::ReadFailure, CellOutcome::Failed("cell_unavailable"))
                | (Action::HashMismatch, CellOutcome::Failed("cell_corrupt"))
                | (Action::DecodeFailure, CellOutcome::Failed("cell_decode")) => {}
                _ => panic!("wrong terminal for {answer:?}: {:?}", result.outcome),
            }
            terminal.deliver(result, &mut h);
            if matches!(answer, Action::CancelBeforeAnswer) {
                h.state(A, CellState::Unloaded);
                assert_eq!(h.runtime.stats().stale_discards, 1);
                assert_eq!(h.runtime.stats().failures, 0);
            } else {
                let reason = match answer {
                    Action::ReadFailure => "cell_unavailable",
                    Action::HashMismatch => "cell_corrupt",
                    Action::DecodeFailure => "cell_decode",
                    _ => "cell_missing",
                };
                h.state(A, CellState::Failed(reason.into()));
                assert_eq!(h.runtime.stats().failures, 1);
                assert_eq!(h.runtime.stats().stale_discards, 0);
            }
            assert_eq!(h.applied(A), 0);
            h.state(B, CellState::Loading);
            h.accounting(1, h.size(B));
            h.healthy(B);
            h.accounting(0, 0);
            assert_eq!(h.runtime.stats().loads_issued, 2);
        }
    }

    #[test]
    fn cell_sink_cancel_at_decode_checkpoints_does_not_suppress_another_cell() {
        let _history = HISTORY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        for point in [DecodePoint::BeforeDecode, DecodePoint::BeforePublish] {
            let mut h = History::new(2);
            h.plan(&[A]);
            let mut first = h.request(A);
            let terminal = Terminal::observe(&mut first);
            let barrier = PausedDecode::install(&mut first.sink, point);
            h.finish(first.sink, A);
            barrier.entered();
            h.plan(&[B]);
            assert!(first.cancel.is_cancelled());
            h.state(A, CellState::Unloaded);
            h.accounting(2, h.size(A) + h.size(B));
            // B finishes while A is still held; cancelling A cannot cancel B's decoder/token.
            h.healthy(B);
            h.accounting(1, h.size(A));
            barrier.release();
            let result = terminal.one();
            assert!(
                matches!(result.outcome, CellOutcome::Abandoned),
                "{point:?}: {:?}",
                result.outcome
            );
            terminal.deliver(result, &mut h);
            h.accounting(0, 0);
            h.state(A, CellState::Unloaded);
            h.state(B, CellState::Spawned);
            assert_eq!(h.applied(A), 0);
            assert_eq!(h.runtime.stats().stale_discards, 1);
            assert_eq!(h.runtime.stats().loads_cancelled, 1);
            assert_eq!(h.runtime.stats().failures, 0);
            // Capacity is reusable by the canceled cell's next generation too.
            h.plan(&[A]);
            h.accounting(1, h.size(A));
            h.healthy(A);
            h.accounting(0, 0);
        }
    }

    #[test]
    fn cell_sink_old_completion_cannot_replace_an_active_generation_or_release_its_slot() {
        let _history = HISTORY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        // Both a ready result queued before cancellation and a decoder finishing after cancellation
        // must leave the replacement's slot/bytes and state intact.
        for ready_before_cancel in [false, true] {
            let mut h = History::new(2);
            h.plan(&[A]);
            let mut old = h.request(A);
            let terminal = Terminal::observe(&mut old);
            let barrier = PausedDecode::install(&mut old.sink, DecodePoint::BeforePublish);
            h.finish(old.sink, A);
            barrier.entered();
            let mut barrier = Some(barrier);
            let early_result = if ready_before_cancel {
                barrier.take().expect("held checkpoint").release();
                Some(terminal.one())
            } else {
                None
            };
            // Use a source outside both cells to cancel A, then want it again.
            h.plan(&[CellCoord::new(20, 0)]);
            assert!(old.cancel.is_cancelled());
            h.state(A, CellState::Unloaded);
            h.plan(&[A, B]);
            let mut replacement = h.request(A);
            assert!(replacement.ticket.generation > old.ticket.generation);
            assert!(!replacement.cancel.is_cancelled());
            let replacement_terminal = Terminal::observe(&mut replacement);
            h.state(A, CellState::Loading);
            h.state(B, CellState::Queued);
            h.accounting(2, h.size(A) * 2);
            let result = if let Some(result) = early_result {
                assert!(matches!(result.outcome, CellOutcome::Ready { .. }));
                result
            } else {
                barrier.take().expect("held checkpoint").release();
                let result = terminal.one();
                assert!(matches!(result.outcome, CellOutcome::Abandoned));
                result
            };
            terminal.deliver(result, &mut h);
            h.state(A, CellState::Loading);
            assert!(!replacement.cancel.is_cancelled());
            assert_eq!(h.applied(A), 0);
            assert_eq!(h.runtime.stats().stale_discards, 1);
            h.accounting(2, h.size(A) + h.size(B));
            h.healthy(B);
            h.accounting(1, h.size(A));
            h.finish(replacement.sink, A);
            let result = replacement_terminal.one();
            assert!(matches!(result.outcome, CellOutcome::Ready { .. }));
            replacement_terminal.deliver(result, &mut h);
            h.state(A, CellState::Spawned);
            h.accounting(0, 0);
            assert_eq!(h.applied(A), 1);
            assert_eq!(h.runtime.stats().failures, 0);
            assert_eq!(h.runtime.stats().loads_issued, 3);
        }
    }
}
