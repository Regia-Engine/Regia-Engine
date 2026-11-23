// SPDX-License-Identifier: MPL-2.0
//! Background pak streaming (`docs/specs/12-assets.md` § 5b): per-asset reads from mapped paks on
//! a worker thread, with read-ahead, back-pressure and progress.
//!
//! A [`PakStreamer`] takes [`StreamRequest`]s (asset id + the load-set item it fills), reads each
//! asset's bytes from the memory-mapped paks — never the whole file — and decompresses it on its
//! own thread. The next few requests are prefetched (`madvise(WILLNEED)`) while the current one
//! decompresses, so the page cache fills from NVMe in parallel. Finished assets wait in a bounded
//! channel (default 8): a consumer that uploads under a per-frame budget
//! (`regia_render::UploadQueue`) slows the reader instead of letting decoded bytes pile up in
//! RAM. Failures are marked in the [`LoadSets`] registry; residency is marked by whoever makes
//! the asset usable (the uploader), not by the reader.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::id::AssetId;
use crate::load_set::{LoadKind, LoadSets};
use crate::pak::{PakPayload, PakReader};

/// One asset to stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamRequest {
    /// Asset id in one of the streamer's paks.
    pub id: AssetId,
    /// The load-set item it fills.
    pub kind: LoadKind,
    /// The load-set item key.
    pub key: String,
}

/// An asset read and decompressed by the streamer.
#[derive(Debug)]
pub struct StreamedAsset {
    /// The request.
    pub request: StreamRequest,
    /// Payload: a range of the mapped pak for an entry stored uncompressed (no heap copy; the
    /// asset holds the pak alive), decompressed bytes otherwise.
    pub bytes: PakPayload,
    /// Time spent reading and decompressing, microseconds.
    pub read_us: u64,
}

/// Counters of a streamer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamProgress {
    /// Requests received.
    pub requested: u64,
    /// Assets read and handed over.
    pub delivered: u64,
    /// Requests that failed (not found, corrupt).
    pub failed: u64,
    /// Decompressed bytes handed over.
    pub bytes: u64,
    /// Stored (on-disk) bytes read.
    pub stored_bytes: u64,
    /// Total read + decompress time, microseconds.
    pub busy_us: u64,
}

impl StreamProgress {
    /// Requests still queued or in flight.
    #[must_use]
    pub fn outstanding(&self) -> u64 {
        self.requested.saturating_sub(self.delivered + self.failed)
    }
}

#[derive(Default)]
struct Counters {
    requested: AtomicU64,
    delivered: AtomicU64,
    failed: AtomicU64,
    bytes: AtomicU64,
    stored_bytes: AtomicU64,
    busy_us: AtomicU64,
}

/// Streams assets out of mapped paks on a worker thread.
pub struct PakStreamer {
    requests: Sender<StreamRequest>,
    out: Mutex<Receiver<StreamedAsset>>,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl PakStreamer {
    /// Readahead depth (requests prefetched while one decompresses).
    pub const READAHEAD: usize = 4;
    /// Finished assets buffered before the reader waits for the consumer.
    pub const BUFFERED: usize = 8;

    /// Starts a streamer over `paks` (mount order: a later pak overrides an earlier one, like `AssetServer`). Failures are marked in `sets`.
    ///
    /// # Errors
    ///
    /// The worker thread could not be spawned.
    pub fn new(paks: Vec<Arc<PakReader>>, sets: Option<LoadSets>) -> std::io::Result<Self> {
        Self::with_buffer(paks, sets, Self::BUFFERED)
    }

    /// [`Self::new`] with `buffered` finished assets before back-pressure (at least 1).
    ///
    /// # Errors
    ///
    /// The worker thread could not be spawned.
    pub fn with_buffer(
        paks: Vec<Arc<PakReader>>,
        sets: Option<LoadSets>,
        buffered: usize,
    ) -> std::io::Result<Self> {
        let (requests, request_rx) = std::sync::mpsc::channel();
        let (out_tx, out) = std::sync::mpsc::sync_channel(buffered.max(1));
        let counters = Arc::new(Counters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let counters = counters.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("regia-pak-stream".into())
                .spawn(move || run(&paks, sets.as_ref(), &request_rx, &out_tx, &counters, &stop))?
        };
        Ok(Self {
            requests,
            out: Mutex::new(out),
            counters,
            stop,
            worker: Some(worker),
        })
    }

    /// Queues requests (served in order).
    pub fn request(&self, requests: impl IntoIterator<Item = StreamRequest>) {
        for request in requests {
            self.counters.requested.fetch_add(1, Ordering::Relaxed);
            if self.requests.send(request).is_err() {
                // The worker is gone (stopped); count the request as failed so progress ends.
                self.counters.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// A finished asset, if one is ready (never blocks: call from the frame).
    #[must_use]
    pub fn try_next(&self) -> Option<StreamedAsset> {
        let out = self.out.lock().ok()?;
        out.try_recv().ok()
    }

    /// Waits up to `timeout` for a finished asset (tools and load screens, not the frame).
    ///
    /// Polls the output under a brief lock and waits between polls without it, so a frame's
    /// [`Self::try_next`] never waits behind this call (audit concurrency-1103 M3).
    #[must_use]
    pub fn next_timeout(&self, timeout: Duration) -> Option<StreamedAsset> {
        const SLICE: Duration = Duration::from_millis(1);
        let deadline = Instant::now().checked_add(timeout);
        loop {
            match self.out.lock().ok()?.try_recv() {
                Ok(asset) => return Some(asset),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            let wait = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    left.min(SLICE)
                }
                None => SLICE,
            };
            std::thread::sleep(wait);
        }
    }

    /// Counters so far.
    #[must_use]
    pub fn progress(&self) -> StreamProgress {
        let c = &self.counters;
        StreamProgress {
            requested: c.requested.load(Ordering::Relaxed),
            delivered: c.delivered.load(Ordering::Relaxed),
            failed: c.failed.load(Ordering::Relaxed),
            bytes: c.bytes.load(Ordering::Relaxed),
            stored_bytes: c.stored_bytes.load(Ordering::Relaxed),
            busy_us: c.busy_us.load(Ordering::Relaxed),
        }
    }
}

impl Drop for PakStreamer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock a worker waiting on a full output channel.
        if let Ok(out) = self.out.lock() {
            while out.try_recv().is_ok() {}
        }
        let (dead, _) = std::sync::mpsc::channel();
        drop(std::mem::replace(&mut self.requests, dead));
        if let Some(worker) = self.worker.take() {
            // Drain until the worker notices the closed request channel.
            while !worker.is_finished() {
                if let Ok(out) = self.out.lock() {
                    while out.try_recv().is_ok() {}
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let _ = worker.join();
        }
    }
}

fn find(paks: &[Arc<PakReader>], id: AssetId) -> Option<&Arc<PakReader>> {
    paks.iter().rev().find(|p| p.contains(id))
}

/// Faults a mapped payload's pages in on this (the streamer's) thread: a mapped payload is not
/// copied, so without this its first touch — and any disk read behind it — would happen in the
/// uploader's `write_texture` on the frame thread. Reads one byte per page; returns their xor so
/// the loads are not optimised away.
fn fault_in(payload: &PakPayload) -> u8 {
    const PAGE: usize = 4096;
    if !payload.is_mapped() {
        return 0;
    }
    let bytes: &[u8] = payload;
    let mut acc = bytes.last().copied().unwrap_or(0);
    for &b in bytes.iter().step_by(PAGE) {
        acc ^= b;
    }
    std::hint::black_box(acc)
}

fn run(
    paks: &[Arc<PakReader>],
    sets: Option<&LoadSets>,
    requests: &Receiver<StreamRequest>,
    out: &SyncSender<StreamedAsset>,
    counters: &Counters,
    stop: &AtomicBool,
) {
    let mut queue: VecDeque<StreamRequest> = VecDeque::new();
    let mut hinted = 0usize;
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Take everything queued; block only when idle.
        if queue.is_empty() {
            match requests.recv() {
                Ok(r) => queue.push_back(r),
                Err(_) => return,
            }
        }
        while let Ok(r) = requests.try_recv() {
            queue.push_back(r);
        }
        let Some(request) = queue.pop_front() else {
            continue;
        };
        // Read-ahead: hint the next requests' pages while this one decompresses.
        hinted = hinted.saturating_sub(1);
        while hinted < PakStreamer::READAHEAD.min(queue.len()) {
            if let Some(pak) = find(paks, queue[hinted].id) {
                let _ = pak.prefetch(queue[hinted].id);
            }
            hinted += 1;
        }
        let _span = tracing::info_span!("pak_stream_read").entered();
        let start = Instant::now();
        let result = match find(paks, request.id) {
            Some(pak) => pak.read_shared(request.id).map(|bytes| {
                fault_in(&bytes);
                let stored = pak.entry(request.id).map_or(0, |e| e.compressed_size);
                (bytes, stored)
            }),
            None => Err(crate::pak::PakError::NotFound(request.id)),
        };
        let read_us = start.elapsed().as_micros() as u64;
        counters.busy_us.fetch_add(read_us, Ordering::Relaxed);
        match result {
            Ok((bytes, stored)) => {
                counters.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                counters.stored_bytes.fetch_add(stored, Ordering::Relaxed);
                let asset = StreamedAsset {
                    request,
                    bytes,
                    read_us,
                };
                if out.send(asset).is_err() {
                    return;
                }
                // Counted once the send completed: the consumer can receive this asset before
                // this thread resumes, so a reader may briefly see one less (see the test).
                counters.delivered.fetch_add(1, Ordering::Relaxed);
            }
            Err(err) => {
                tracing::warn!(target: "regia_asset", "pak stream: {} ({}) failed: {err}", request.key, request.id);
                if let Some(sets) = sets {
                    // `mark_failed` runs the host's progress callbacks on this thread: one that
                    // panics must not end the streamer with this request uncounted and every later
                    // one refused (audit concurrency-1103 M2).
                    let marked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        sets.mark_failed(request.kind, &request.key, err.to_string());
                    }));
                    if marked.is_err() {
                        tracing::error!(target: "regia_asset", "pak stream: a load-set callback panicked while {} was marked failed", request.key);
                    }
                }
                counters.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_set::LoadSet;

    /// A minimal uncompressed v1 pak with `entries` (same layout as the cooker's writer).
    fn pak(entries: &[(AssetId, Vec<u8>)]) -> Vec<u8> {
        let mut data = vec![0u8; 60];
        let mut toc_entries = Vec::new();
        for (id, payload) in entries {
            let rem = data.len() % crate::pak::FPAK_BLOB_ALIGNMENT as usize;
            if rem != 0 {
                data.resize(data.len() + crate::pak::FPAK_BLOB_ALIGNMENT as usize - rem, 0);
            }
            toc_entries.push(crate::pak::PakEntry {
                id: *id,
                type_tag: regia_core::id::TypePathHash(0),
                offset: data.len() as u64,
                size: payload.len() as u64,
                compressed_size: payload.len() as u64,
                block_table_offset: 0,
            });
            data.extend_from_slice(payload);
        }
        let toc_offset = data.len() as u64;
        let toc = crate::pak::PakToc {
            entries: toc_entries,
            manifest: Vec::new(),
            dependencies: Vec::new(),
        };
        let toc_bytes = postcard::to_allocvec(&toc).expect("toc");
        data.extend_from_slice(&toc_bytes);
        let header = crate::pak::encode_pak_header(
            crate::pak::FPAK_VERSION,
            0,
            toc_offset,
            toc_bytes.len() as u64,
            &[0; 32],
        );
        data[..60].copy_from_slice(&header);
        data
    }

    #[test]
    fn a_later_pak_overrides_an_earlier_one() {
        let id = AssetId::new_random();
        let open = |bytes: Vec<u8>, name: &str| {
            let len = bytes.len() as u64;
            Arc::new(PakReader::from_bytes(bytes, name, len).expect("pak"))
        };
        let base = open(pak(&[(id, vec![1u8; 64])]), "base.fpak");
        let patch = open(pak(&[(id, vec![2u8; 64])]), "patch.fpak");
        let streamer = PakStreamer::new(vec![base, patch], None).expect("streamer");
        streamer.request(vec![StreamRequest {
            id,
            kind: LoadKind::Texture,
            key: "t".into(),
        }]);
        let asset = streamer.next_timeout(Duration::from_secs(10)).expect("asset");
        assert!(asset.bytes.iter().all(|&b| b == 2));
    }

    #[test]
    fn streams_in_order_with_backpressure_and_marks_failures() {
        let ids: Vec<AssetId> = (0..20).map(|_| AssetId::new_random()).collect();
        let entries: Vec<(AssetId, Vec<u8>)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, vec![i as u8; 5000 + i]))
            .collect();
        let bytes = pak(&entries);
        let len = bytes.len() as u64;
        let reader = Arc::new(PakReader::from_bytes(bytes, "stream.fpak", len).expect("pak"));
        let sets = LoadSets::default();
        let missing = AssetId::new_random();
        let mut set = LoadSet::new("map");
        for (i, _) in entries.iter().enumerate() {
            set.push(LoadKind::Texture, format!("tex{i}"), 0, 1);
        }
        set.push(LoadKind::Mesh, "missing", 0, 1);
        sets.define(set);
        let streamer = PakStreamer::with_buffer(vec![reader], Some(sets.clone()), 2).expect("streamer");
        let mut requests: Vec<StreamRequest> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| StreamRequest {
                id: *id,
                kind: LoadKind::Texture,
                key: format!("tex{i}"),
            })
            .collect();
        requests.insert(
            7,
            StreamRequest {
                id: missing,
                kind: LoadKind::Mesh,
                key: "missing".into(),
            },
        );
        streamer.request(requests);
        let mut got = Vec::new();
        while got.len() < ids.len() {
            let asset = streamer.next_timeout(Duration::from_secs(10)).expect("asset");
            // Back-pressure: never more than the buffer plus the one being handed over.
            let p = streamer.progress();
            assert!(p.delivered as usize <= got.len() + 3, "{p:?}");
            sets.mark_resident(asset.request.kind, &asset.request.key);
            got.push(asset);
        }
        for (i, asset) in got.iter().enumerate() {
            assert_eq!(asset.request.key, format!("tex{i}"));
            assert_eq!(asset.bytes, entries[i].1);
        }
        // The worker counts a delivery after its send returns, which can be after this thread
        // received the asset: wait (bounded) for the last count before checking the totals.
        let settle = std::time::Instant::now();
        let mut p = streamer.progress();
        while p.delivered < 20 && settle.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
            p = streamer.progress();
        }
        assert_eq!(
            (p.requested, p.delivered, p.failed, p.outstanding()),
            (21, 20, 1, 0)
        );
        let progress = sets.load_progress("map").expect("set");
        assert_eq!(progress.resident, 20);
        assert_eq!(progress.failed.len(), 1);
        assert!(!sets.is_resident("map"));
    }

    #[test]
    fn dropping_a_busy_streamer_stops_the_worker() {
        let ids: Vec<AssetId> = (0..32).map(|_| AssetId::new_random()).collect();
        let entries: Vec<(AssetId, Vec<u8>)> = ids.iter().map(|id| (*id, vec![1u8; 70_000])).collect();
        let bytes = pak(&entries);
        let len = bytes.len() as u64;
        let reader = Arc::new(PakReader::from_bytes(bytes, "drop.fpak", len).expect("pak"));
        let streamer = PakStreamer::with_buffer(vec![reader], None, 1).expect("streamer");
        streamer.request(ids.iter().map(|id| StreamRequest {
            id: *id,
            kind: LoadKind::Other,
            key: String::new(),
        }));
        assert!(streamer.next_timeout(Duration::from_secs(10)).is_some());
        drop(streamer); // must not hang with the worker blocked on a full channel
    }

    #[test]
    fn raw_entries_stream_from_the_mapping_and_outlive_the_streamer_and_reader() {
        let ids: Vec<AssetId> = (0..3).map(|_| AssetId::new_random()).collect();
        let entries: Vec<(AssetId, Vec<u8>)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, vec![i as u8 + 1; 3 * 4096 + i]))
            .collect();
        let dir = std::env::temp_dir().join(format!("regia-pak-stream-map-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("map.fpak");
        std::fs::write(&path, pak(&entries)).expect("write pak");
        let reader = Arc::new(PakReader::open(&path).expect("open"));
        assert!(reader.is_mapped());
        let raw: Vec<usize> = ids
            .iter()
            .map(|id| reader.read_raw(*id).expect("raw").as_ptr() as usize)
            .collect();
        let streamer = PakStreamer::new(vec![reader.clone()], None).expect("streamer");
        streamer.request(ids.iter().map(|id| StreamRequest {
            id: *id,
            kind: LoadKind::Texture,
            key: String::new(),
        }));
        let got: Vec<StreamedAsset> = (0..ids.len())
            .map(|_| streamer.next_timeout(Duration::from_secs(10)).expect("asset"))
            .collect();
        // Dropping the streamer and every other handle of the reader leaves the assets valid.
        drop(streamer);
        drop(reader);
        let _ = std::fs::remove_dir_all(&dir);
        for (i, asset) in got.iter().enumerate() {
            assert!(asset.bytes.is_mapped());
            assert_eq!(asset.bytes.as_ptr() as usize, raw[i], "asset {i} was copied");
            assert_eq!(asset.bytes, entries[i].1);
        }
    }

    fn one_asset_reader() -> (AssetId, Arc<PakReader>) {
        let id = AssetId::new_random();
        let bytes = pak(&[(id, vec![9u8; 128])]);
        let len = bytes.len() as u64;
        (
            id,
            Arc::new(PakReader::from_bytes(bytes, "one.fpak", len).expect("pak")),
        )
    }

    /// Audit concurrency-1103 asset M2: a load-set callback that panics inside the worker's
    /// `mark_failed` killed the streamer thread, so the failed request was never counted and
    /// every later request was refused. The worker survives, counts the failure and serves on.
    #[test]
    fn a_panicking_failure_callback_does_not_kill_the_streamer() {
        let (id, reader) = one_asset_reader();
        let sets = LoadSets::default();
        sets.define(LoadSet::new("map").with(LoadKind::Mesh, "missing", 0, 1));
        sets.on_progress(|_| panic!("a host callback panicked"));
        let streamer = PakStreamer::new(vec![reader], Some(sets)).expect("streamer");
        streamer.request([
            StreamRequest {
                id: AssetId::new_random(),
                kind: LoadKind::Mesh,
                key: "missing".into(),
            },
            StreamRequest {
                id,
                kind: LoadKind::Texture,
                key: "t".into(),
            },
        ]);
        let asset = streamer
            .next_timeout(Duration::from_secs(20))
            .expect("the request after the panicking callback is still served");
        assert_eq!(asset.request.key, "t");
        assert_eq!(streamer.progress().failed, 1, "the failed request is counted");
    }

    /// Audit concurrency-1103 asset M3: `try_next` never blocks, even while another thread
    /// waits in `next_timeout` (which used to hold the output lock for its whole timeout).
    #[test]
    fn try_next_does_not_wait_for_another_threads_next_timeout() {
        let (id, reader) = one_asset_reader();
        let streamer = Arc::new(PakStreamer::new(vec![reader], None).expect("streamer"));
        let waiter = {
            let streamer = Arc::clone(&streamer);
            std::thread::spawn(move || streamer.next_timeout(Duration::from_secs(60)))
        };
        // Give the waiter every chance to be inside `next_timeout` (on the old code it then holds
        // the output lock); bounded by spins, not time.
        for _ in 0..200_000 {
            if streamer.out.try_lock().is_err() {
                break;
            }
            std::thread::yield_now();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let poller = {
            let streamer = Arc::clone(&streamer);
            std::thread::spawn(move || {
                let _ = tx.send(streamer.try_next().is_none());
            })
        };
        let polled = rx.recv_timeout(Duration::from_secs(10));
        // Release the waiter either way so the test ends.
        streamer.request([StreamRequest {
            id,
            kind: LoadKind::Texture,
            key: "t".into(),
        }]);
        assert_eq!(
            polled,
            Ok(true),
            "try_next blocked behind another thread's next_timeout"
        );
        poller.join().expect("poller");
        assert!(
            waiter.join().expect("waiter").is_some(),
            "the waiter got the asset"
        );
    }
}
