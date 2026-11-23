// SPDX-License-Identifier: MPL-2.0
//! Chunk sources: where a chunk file comes from at run time (spec 40 § 2.6).
//!
//! A [`ChunkSource`] reports whether it can provide a chunk and starts a [`Transfer`] into a
//! `.part` file. Transfers are stepped with a byte budget, so the manager (and tests) decide how
//! far each one moves per step; a source that works on its own thread (HTTP) reports progress
//! from that thread and ignores the budget.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::manifest::ChunkEntry;

/// The `.part` files a transfer is writing, and the wait for one to end.
static PART_LEASES: (
    parking_lot::Mutex<std::collections::BTreeSet<PathBuf>>,
    parking_lot::Condvar,
) = (
    parking_lot::Mutex::new(std::collections::BTreeSet::new()),
    parking_lot::Condvar::new(),
);

/// One transfer's hold on a `.part` file. Dropping an HTTP transfer only asks its thread to stop,
/// and that thread may sit in a read for a whole timeout, so a new transfer for the same chunk, from
/// any source, waits for the lease: two writers would corrupt the file and fail the hash (audit
/// concurrency-1103 H1). The key is the path with its directory canonicalised, so two spellings of
/// one install directory share a lease (L5; the lease is per process).
pub(crate) struct PartLease(PathBuf);

impl PartLease {
    fn key(path: &Path) -> PathBuf {
        match (path.parent(), path.file_name()) {
            (Some(dir), Some(name)) => {
                let dir = if dir.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    dir
                };
                dir.canonicalize()
                    .map_or_else(|_| path.to_path_buf(), |dir| dir.join(name))
            }
            _ => path.to_path_buf(),
        }
    }

    /// Takes the lease for `path` once it is free, waiting on this thread; `None` when `cancel`
    /// was set meanwhile.
    pub(crate) fn acquire(path: &Path, cancel: &std::sync::atomic::AtomicBool) -> Option<Self> {
        let key = Self::key(path);
        let (held, released) = &PART_LEASES;
        let mut held = held.lock();
        while held.contains(&key) {
            if cancel.load(Ordering::Acquire) {
                return None;
            }
            released.wait_for(&mut held, std::time::Duration::from_millis(100));
        }
        held.insert(key.clone());
        Some(Self(key))
    }

    /// Takes the lease for `path` if it is free now (stepped transfers, which must not block).
    pub(crate) fn try_acquire(path: &Path) -> Option<Self> {
        let key = Self::key(path);
        let mut held = PART_LEASES.0.lock();
        if held.contains(&key) {
            return None;
        }
        held.insert(key.clone());
        Some(Self(key))
    }
}

impl Drop for PartLease {
    fn drop(&mut self) {
        let (held, released) = &PART_LEASES;
        held.lock().remove(&self.0);
        released.notify_all();
    }
}

/// Size of the resume blocks the optional manifest `blocks` hashes cover.
pub const RESUME_BLOCK: u64 = 1 << 20;

/// BLAKE3 hex of each [`RESUME_BLOCK`] of `reader` (the last block may be short): the manifest's
/// `blocks` list.
///
/// # Errors
/// The reader fails.
pub fn block_hashes(reader: &mut impl Read) -> std::io::Result<Vec<String>> {
    let mut hashes = Vec::new();
    let mut block = vec![0u8; RESUME_BLOCK as usize];
    loop {
        let filled = read_block(reader, &mut block)?;
        if filled == 0 {
            return Ok(hashes);
        }
        hashes.push(blake3::hash(&block[..filled]).to_hex().to_string());
    }
}

/// Fills `block` as far as the reader goes.
fn read_block(reader: &mut impl Read, block: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < block.len() {
        match reader.read(&mut block[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Where a transfer resumes in a partial file of `len` bytes: the end of the last whole block that
/// still matches `blocks` (spec § 2.6). Without block hashes the length is trusted and the file's
/// hash catches corruption at the end.
pub(crate) fn resume_point(file: &mut std::fs::File, len: u64, blocks: &[String]) -> std::io::Result<u64> {
    if blocks.is_empty() {
        return Ok(len);
    }
    file.seek(SeekFrom::Start(0))?;
    let mut block = vec![0u8; RESUME_BLOCK as usize];
    let mut good = 0;
    for hash in blocks
        .iter()
        .take(usize::try_from(len / RESUME_BLOCK).unwrap_or(usize::MAX))
    {
        file.read_exact(&mut block)?;
        if blake3::hash(&block).to_hex().as_str() != hash {
            break;
        }
        good += RESUME_BLOCK;
    }
    Ok(good)
}

/// What a source knows about one chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceStatus {
    /// The file is already on the device at this path; no transfer needed.
    Present(PathBuf),
    /// The source can transfer the chunk.
    Available,
    /// The source does not have the chunk.
    Missing,
}

/// One step of a transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferStep {
    /// Bytes now in the `.part` file and the file size.
    Progress {
        /// Bytes on disk.
        done: u64,
        /// Total bytes.
        total: u64,
    },
    /// The `.part` file is complete.
    Done,
    /// The transfer failed; the reason is shown to the user and the agent.
    Failed(String),
}

/// A running transfer into a `.part` file.
pub trait Transfer: Send + Sync {
    /// Moves up to `budget` bytes. A threaded transfer ignores the budget and reports its state.
    fn step(&mut self, budget: u64) -> TransferStep;

    /// Blocks until a threaded transfer has finished (tests and tools; never per frame). A
    /// stepped transfer has nothing to wait for.
    fn wait(&mut self) {}
}

/// Where a chunk's file comes from.
pub trait ChunkSource: Send + Sync {
    /// Source kind: `"local"`, `"dir"`, `"http"`, `"android_pack"`.
    fn name(&self) -> &'static str;
    /// Whether this source has `chunk`.
    fn status(&self, chunk: &ChunkEntry) -> SourceStatus;
    /// Starts (or resumes) a transfer of `chunk` into `dest` (the `.part` path).
    fn start(&self, chunk: &ChunkEntry, dest: &Path) -> Box<dyn Transfer>;
    /// Whether a transfer from this source writes its bytes into the install directory, so the
    /// manager checks free space first. The Android store keeps a pack on its own storage.
    fn uses_install_space(&self) -> bool {
        true
    }
    /// Whether a file this source calls [`SourceStatus::Present`] must pass its hash before it is
    /// used. The packaged content directory ships with the game; a store's pack was written by
    /// another process, and is hashed where it lies.
    fn verify_present(&self) -> bool {
        false
    }
    /// `chunk` was released from the device: drop anything the source keeps for it.
    fn release(&self, _chunk: &ChunkEntry) {}
}

/// The local content directory: a chunk file already there is present; nothing transfers.
#[derive(Clone, Debug)]
pub struct LocalSource {
    dir: PathBuf,
}

impl LocalSource {
    /// A source over `dir` (the package's content directory).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

impl ChunkSource for LocalSource {
    fn name(&self) -> &'static str {
        "local"
    }

    fn status(&self, chunk: &ChunkEntry) -> SourceStatus {
        let path = self.dir.join(&chunk.file);
        if path.is_file() {
            SourceStatus::Present(path)
        } else {
            SourceStatus::Missing
        }
    }

    fn start(&self, chunk: &ChunkEntry, _dest: &Path) -> Box<dyn Transfer> {
        Box::new(FailedTransfer(format!(
            "{} is not in the content directory",
            chunk.file
        )))
    }
}

struct FailedTransfer(String);

impl Transfer for FailedTransfer {
    fn step(&mut self, _budget: u64) -> TransferStep {
        TransferStep::Failed(self.0.clone())
    }
}

/// A directory laid out like a static file host (`dist/<platform>/chunks`, or a mounted share):
/// chunks are copied from it into the install directory, resuming from an existing `.part`.
/// Counts every byte it serves ([`DirSource::served`]) so tests can prove a resume fetched only
/// the missing tail.
#[derive(Clone, Debug)]
pub struct DirSource {
    root: PathBuf,
    served: Arc<AtomicU64>,
}

impl DirSource {
    /// A source over `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            served: Arc::default(),
        }
    }

    /// Bytes served so far by every transfer of this source.
    #[must_use]
    pub fn served(&self) -> u64 {
        self.served.load(Ordering::Relaxed)
    }
}

impl ChunkSource for DirSource {
    fn name(&self) -> &'static str {
        "dir"
    }

    fn status(&self, chunk: &ChunkEntry) -> SourceStatus {
        if self.root.join(&chunk.file).is_file() {
            SourceStatus::Available
        } else {
            SourceStatus::Missing
        }
    }

    fn start(&self, chunk: &ChunkEntry, dest: &Path) -> Box<dyn Transfer> {
        Box::new(DirTransfer {
            src: self.root.join(&chunk.file),
            dest: dest.to_path_buf(),
            total: chunk.size,
            blocks: chunk.blocks.clone(),
            served: Arc::clone(&self.served),
            open: None,
            lease: None,
            buf: Vec::new(),
        })
    }
}

struct DirTransfer {
    src: PathBuf,
    dest: PathBuf,
    total: u64,
    blocks: Vec<String>,
    served: Arc<AtomicU64>,
    open: Option<(std::fs::File, std::fs::File, u64)>,
    /// Held while this transfer may write the `.part` file.
    lease: Option<PartLease>,
    buf: Vec<u8>,
}

impl DirTransfer {
    /// Opens the files; `Ok(false)` while another transfer (a parked HTTP job still in a read)
    /// holds the `.part` file's lease.
    fn open(&mut self) -> std::io::Result<bool> {
        if self.open.is_some() {
            return Ok(true);
        }
        if let Some(parent) = self.dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if self.lease.is_none() {
            self.lease = PartLease::try_acquire(&self.dest);
            if self.lease.is_none() {
                return Ok(false);
            }
        }
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.dest)?;
        let len = out.metadata()?.len().min(self.total);
        let done = resume_point(&mut out, len, &self.blocks)?;
        out.set_len(done)?;
        out.seek(SeekFrom::Start(done))?;
        let mut src = std::fs::File::open(&self.src)?;
        src.seek(SeekFrom::Start(done))?;
        self.open = Some((src, out, done));
        Ok(true)
    }
}

impl Transfer for DirTransfer {
    fn step(&mut self, budget: u64) -> TransferStep {
        let _span = tracing::debug_span!(target: "regia_asset", "content_transfer").entered();
        match self.open() {
            Err(e) => return TransferStep::Failed(format!("{}: {e}", self.src.display())),
            // Another writer still holds the `.part` file: wait for it, write nothing.
            Ok(false) => {
                return TransferStep::Progress {
                    done: 0,
                    total: self.total,
                };
            }
            Ok(true) => {}
        }
        let total = self.total;
        let Some((src, out, done)) = self.open.as_mut() else {
            return TransferStep::Failed("transfer not open".into());
        };
        let want = budget.min(total - *done);
        // One reusable buffer of at most 64 KiB: no allocation per step after warm-up.
        let chunk = usize::try_from(want.min(64 * 1024)).unwrap_or(64 * 1024);
        if self.buf.len() < chunk {
            self.buf.resize(chunk, 0);
        }
        let mut moved = 0u64;
        while moved < want {
            let len = usize::try_from((want - moved).min(chunk as u64)).unwrap_or(chunk);
            let n = match src.read(&mut self.buf[..len]) {
                Ok(0) => {
                    return TransferStep::Failed(format!(
                        "{} is shorter than the manifest",
                        self.src.display()
                    ));
                }
                Ok(n) => n,
                Err(e) => return TransferStep::Failed(e.to_string()),
            };
            if let Err(e) = out.write_all(&self.buf[..n]) {
                return TransferStep::Failed(e.to_string());
            }
            moved += n as u64;
        }
        *done += moved;
        self.served.fetch_add(moved, Ordering::Relaxed);
        if *done >= total {
            if let Err(e) = out.sync_all() {
                return TransferStep::Failed(e.to_string());
            }
            self.open = None;
            self.lease = None;
            TransferStep::Done
        } else {
            TransferStep::Progress { done: *done, total }
        }
    }
}
