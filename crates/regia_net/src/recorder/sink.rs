// SPDX-License-Identifier: MPL-2.0
//! The file end of a recording (spec 103 § 6): a writer thread that owns the tmp file, pooled blocks between it and
//! the tick, and an atomic rename at close.
//!
//! The tick copies chunks into the current block and hands a full block over under a mutex; nothing on that path
//! allocates once the blocks have grown to their working size, and a slow disk costs the tick nothing until every
//! block is with the writer. The sync and the rename run on the writer thread too: [`FileSink::begin_finish`] returns
//! at once, so a cap that ends a recording never waits for the disk on the tick.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::format::NetDemoError;

/// One block's working size.
pub const BLOCK_BYTES: usize = 256 * 1024;
/// Blocks in the pool.
pub const BLOCKS: usize = 4;
/// How long the tick waits for a free block before the recording ends (`disk_slow`).
pub const MAX_STALL_MS: u64 = 1000;

/// What the writer does once the blocks are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Close {
    /// Sync and rename onto the target.
    Commit,
    /// Remove the tmp file.
    Discard,
}

#[derive(Debug)]
struct Shared {
    free: Vec<Vec<u8>>,
    full: VecDeque<Vec<u8>>,
    close: Option<Close>,
    error: Option<String>,
}

#[derive(Debug)]
struct Pool {
    state: Mutex<Shared>,
    /// A block went to `free`, or the writer stopped.
    freed: Condvar,
    /// A block went to `full`, or closing was set.
    filled: Condvar,
}

impl Pool {
    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        // Nothing panics while holding it, and the state stays consistent if something did.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Where a recording goes.
#[derive(Debug, Clone)]
struct Paths {
    tmp: PathBuf,
    target: PathBuf,
    overwrite: bool,
}

type Writer = std::thread::JoinHandle<Result<(), NetDemoError>>;

/// A file being written: see the module docs.
#[derive(Debug)]
pub struct FileSink {
    pool: Arc<Pool>,
    current: Vec<u8>,
    written: u64,
    thread: Option<Writer>,
    /// For a test that swaps the writer's file; the writer thread holds its own.
    #[cfg(all(test, target_os = "linux"))]
    paths: Paths,
}

/// A recording whose last bytes went to the writer thread, which syncs and renames it. Dropping this waits for the
/// thread, so a file closing as the process exits still arrives.
#[derive(Debug)]
pub struct ClosingSink {
    thread: Option<Writer>,
    written: u64,
}

impl ClosingSink {
    /// Waits for the sync and the rename.
    ///
    /// # Errors
    /// As [`FileSink::finish`].
    pub fn wait(mut self) -> Result<u64, NetDemoError> {
        let result = join_writer(self.thread.take());
        result.map(|()| self.written)
    }
}

impl Drop for ClosingSink {
    fn drop(&mut self) {
        let _ = join_writer(self.thread.take());
    }
}

fn join_writer(thread: Option<Writer>) -> Result<(), NetDemoError> {
    match thread.map(std::thread::JoinHandle::join) {
        Some(Ok(result)) => result,
        Some(Err(_)) => Err(NetDemoError::Io("the writer thread stopped".to_string())),
        None => Ok(()),
    }
}

impl FileSink {
    /// Creates a tmp file next to `target`, named `<target>.<pid>.<n>.tmp`, writes `header` as the first bytes and starts the writer thread.
    ///
    /// # Errors
    /// [`NetDemoError::Exists`] when `target` exists and `overwrite` is off; [`NetDemoError::Io`].
    pub fn create(target: &Path, overwrite: bool, header: &[u8]) -> Result<Self, NetDemoError> {
        if !overwrite && target.exists() {
            return Err(NetDemoError::Exists(target.display().to_string()));
        }
        let (tmp, mut file) = create_tmp(target)?;
        let io = |e: std::io::Error| NetDemoError::Io(format!("{}: {e}", tmp.display()));
        if let Err(e) = file.write_all(header) {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(e));
        }
        let pool = Arc::new(Pool {
            state: Mutex::new(Shared {
                free: (1..BLOCKS).map(|_| Vec::with_capacity(BLOCK_BYTES)).collect(),
                full: VecDeque::with_capacity(BLOCKS),
                close: None,
                error: None,
            }),
            freed: Condvar::new(),
            filled: Condvar::new(),
        });
        let paths = Paths {
            tmp: tmp.clone(),
            target: target.to_path_buf(),
            overwrite,
        };
        let thread = {
            let pool = Arc::clone(&pool);
            #[cfg(all(test, target_os = "linux"))]
            let paths = paths.clone();
            std::thread::Builder::new()
                .name("netdemo-writer".to_string())
                .spawn(move || writer(file, &pool, &paths))
                .map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    NetDemoError::Io(format!("writer thread: {e}"))
                })?
        };
        Ok(Self {
            pool,
            current: Vec::with_capacity(BLOCK_BYTES),
            written: header.len() as u64,
            thread: Some(thread),
            #[cfg(all(test, target_os = "linux"))]
            paths,
        })
    }

    /// Bytes accepted so far, including the header.
    #[must_use]
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Copies `bytes` in. `Ok(false)` when the disk is too slow: every block stayed with the writer for
    /// [`MAX_STALL_MS`]; nothing was taken, and the caller ends the recording.
    ///
    /// # Errors
    /// [`NetDemoError::Io`] from a write that failed earlier.
    pub fn write(&mut self, bytes: &[u8]) -> Result<bool, NetDemoError> {
        if !self.current.is_empty() && self.current.len() + bytes.len() > BLOCK_BYTES && !self.hand_over()? {
            return Ok(false);
        }
        self.current.extend_from_slice(bytes);
        self.written += bytes.len() as u64;
        Ok(true)
    }

    /// Passes the current block to the writer and takes a free one.
    fn hand_over(&mut self) -> Result<bool, NetDemoError> {
        let mut state = self.pool.lock();
        if let Some(e) = state.error.as_ref() {
            return Err(NetDemoError::Io(e.clone()));
        }
        if state.free.is_empty() {
            let (next, timed_out) = self
                .pool
                .freed
                .wait_timeout_while(state, Duration::from_millis(MAX_STALL_MS), |s| {
                    s.free.is_empty() && s.error.is_none()
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if let Some(e) = state.error.as_ref() {
                return Err(NetDemoError::Io(e.clone()));
            }
            if timed_out.timed_out() && state.free.is_empty() {
                return Ok(false);
            }
        }
        let Some(next) = state.free.pop() else {
            return Ok(false);
        };
        let block = std::mem::replace(&mut self.current, next);
        state.full.push_back(block);
        drop(state);
        self.pool.filled.notify_one();
        Ok(true)
    }

    /// Hands `tail` to the writer thread and returns at once; the thread syncs and renames onto the target.
    #[must_use]
    pub fn begin_finish(mut self, tail: &[u8]) -> ClosingSink {
        let mut block = std::mem::take(&mut self.current);
        block.extend_from_slice(tail);
        self.written += tail.len() as u64;
        self.request_close(block, Close::Commit);
        ClosingSink {
            thread: self.thread.take(),
            written: self.written,
        }
    }

    /// Writes `tail`, syncs and renames onto the target: [`Self::begin_finish`], then the wait.
    ///
    /// # Errors
    /// [`NetDemoError::Io`]; a write that failed removes the tmp file, and no file appears at the target. A whole
    /// recording that could not take its place stays at its tmp path, which the error names: [`NetDemoError::Exists`]
    /// when a file appeared at the target meanwhile and `overwrite` is off, [`NetDemoError::Io`] when the rename failed.
    pub fn finish(self, tail: &[u8]) -> Result<u64, NetDemoError> {
        self.begin_finish(tail).wait()
    }

    /// Drops the recording: the tmp file goes and nothing appears at the target.
    pub fn abandon(mut self) {
        self.request_close(Vec::new(), Close::Discard);
        let _ = join_writer(self.thread.take());
    }

    fn request_close(&mut self, last: Vec<u8>, close: Close) {
        {
            let mut state = self.pool.lock();
            state.full.push_back(last);
            state.close = Some(close);
        }
        self.pool.filled.notify_all();
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        // A sink dropped without a finish: nothing appears at the target.
        if self.thread.is_some() {
            self.request_close(Vec::new(), Close::Discard);
            let _ = join_writer(self.thread.take());
        }
    }
}

fn writer(mut file: File, pool: &Pool, paths: &Paths) -> Result<(), NetDemoError> {
    if let Some(e) = write_blocks(&mut file, pool) {
        drop(file);
        let _ = std::fs::remove_file(&paths.tmp);
        return Err(NetDemoError::Io(format!("{}: {e}", paths.tmp.display())));
    }
    let close = pool.lock().close.unwrap_or(Close::Discard);
    if close == Close::Discard {
        drop(file);
        let _ = std::fs::remove_file(&paths.tmp);
        return Ok(());
    }
    commit(file, paths)
}

/// Writes blocks until the close request has none left. The error when a write failed.
fn write_blocks(file: &mut File, pool: &Pool) -> Option<String> {
    loop {
        let block = {
            let mut state = pool.lock();
            loop {
                if let Some(b) = state.full.pop_front() {
                    break Some(b);
                }
                if state.close.is_some() {
                    break None;
                }
                state = pool
                    .filled
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        let mut block = block?;
        let failed = file.write_all(&block).err();
        block.clear();
        // A block grown by a large keyframe goes back to its working size: memory stays bounded.
        block.shrink_to(BLOCK_BYTES);
        let mut state = pool.lock();
        if let Some(e) = failed {
            let text = e.to_string();
            state.error = Some(text.clone());
            // The tick must not wait for blocks that will not come back.
            drop(state);
            pool.freed.notify_all();
            return Some(text);
        }
        state.free.push(block);
        drop(state);
        pool.freed.notify_one();
    }
}

fn commit(file: File, paths: &Paths) -> Result<(), NetDemoError> {
    let (tmp, target) = (&paths.tmp, &paths.target);
    if let Err(e) = file.sync_all() {
        drop(file);
        let _ = std::fs::remove_file(tmp);
        return Err(NetDemoError::Io(format!("{}: {e}", tmp.display())));
    }
    drop(file);
    // From here on the recording is whole and on disk: losing it is worse than a stray file.
    if !paths.overwrite && target.exists() {
        return Err(NetDemoError::Exists(format!(
            "{} appeared during the recording, which stays at {}",
            target.display(),
            tmp.display()
        )));
    }
    std::fs::rename(tmp, target).map_err(|e| {
        NetDemoError::Io(format!(
            "{}: {e}; the recording stays at {}",
            target.display(),
            tmp.display()
        ))
    })
}

/// A new tmp name next to the target (a rename must stay on one file system): `<target>.<pid>.<n>.tmp`.
fn next_tmp_path(target: &Path) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let mut name = target.as_os_str().to_os_string();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(name)
}

/// Creates a tmp file no other writer holds.
fn create_tmp(target: &Path) -> Result<(PathBuf, File), NetDemoError> {
    loop {
        let tmp = next_tmp_path(target);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            // Left by an earlier process with this pid, or another sink's name: the counter moves on.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(NetDemoError::Io(format!("{}: {e}", tmp.display()))),
        }
    }
}

/// Writes `bytes` to `target` through a tmp file and a rename (a ring export).
///
/// # Errors
/// [`NetDemoError::Exists`] or [`NetDemoError::Io`].
pub fn write_atomic(target: &Path, overwrite: bool, parts: &[&[u8]]) -> Result<u64, NetDemoError> {
    write_atomic_with(target, overwrite, parts, || {})
}

/// [`write_atomic`] with `before_rename` run after the bytes are on disk and before the rename: the
/// window a file can appear at the target in (tests only pass anything but a no-op).
fn write_atomic_with(
    target: &Path,
    overwrite: bool,
    parts: &[&[u8]],
    before_rename: impl FnOnce(),
) -> Result<u64, NetDemoError> {
    if !overwrite && target.exists() {
        return Err(NetDemoError::Exists(target.display().to_string()));
    }
    let (tmp, mut file) = create_tmp(target)?;
    let result = (|| -> std::io::Result<u64> {
        let mut written = 0u64;
        for part in parts {
            file.write_all(part)?;
            written += part.len() as u64;
        }
        file.sync_all()?;
        Ok(written)
    })();
    drop(file);
    before_rename();
    // Again after the bytes are down, as `commit` does: a file that appeared at the target while
    // the export was written is not replaced (rc1-rev-net-k 8). The export can be taken again.
    if result.is_ok() && !overwrite && target.exists() {
        let _ = std::fs::remove_file(&tmp);
        return Err(NetDemoError::Exists(target.display().to_string()));
    }
    let result = result.and_then(|written| std::fs::rename(&tmp, target).map(|()| written));
    result.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        NetDemoError::Io(format!("{}: {e}", target.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("netdemo-sink-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("a.netdemo")
    }

    /// What is in the target's directory, by name.
    fn names(target: &Path) -> Vec<String> {
        let dir = target.parent().expect("a parent");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn bytes_arrive_in_order_across_many_blocks() {
        let target = temp("order");
        let mut sink = FileSink::create(&target, false, b"head").expect("creates");
        let mut want = b"head".to_vec();
        for i in 0..2000u32 {
            let chunk = vec![(i % 251) as u8; 1 + (i as usize % 3000)];
            assert_eq!(sink.write(&chunk), Ok(true));
            want.extend_from_slice(&chunk);
        }
        assert!(!target.exists(), "not before the finish");
        assert_eq!(sink.finish(b"tail"), Ok(want.len() as u64 + 4));
        want.extend_from_slice(b"tail");
        assert_eq!(std::fs::read(&target).expect("read"), want);
        assert_eq!(names(&target), vec!["a.netdemo".to_string()], "no tmp file left");
    }

    #[test]
    fn abandoning_or_dropping_leaves_nothing() {
        let target = temp("abandon");
        let mut sink = FileSink::create(&target, false, b"head").expect("creates");
        sink.write(b"body").expect("writes");
        sink.abandon();
        assert!(names(&target).is_empty());
        let sink = FileSink::create(&target, false, b"head").expect("creates");
        drop(sink);
        assert!(names(&target).is_empty());
    }

    /// Two writers aimed at one path keep their own tmp files: neither truncates the other.
    #[test]
    fn two_writers_to_one_path_do_not_share_a_tmp_file() {
        let target = temp("two");
        let mut first = FileSink::create(&target, true, b"first").expect("creates");
        assert_eq!(names(&target).len(), 1, "the first tmp file");
        write_atomic(&target, true, &[b"export"]).expect("exports");
        assert_eq!(std::fs::read(&target).expect("read"), b"export");
        first.write(b"-body").expect("writes");
        first.finish(b"").expect("closes");
        assert_eq!(std::fs::read(&target).expect("read"), b"first-body");
    }

    /// rc1-rev-net-k 8: an export without `overwrite` does not replace a file that appeared at the
    /// target while it was being written, as a recording's `commit` refuses; no tmp file is left.
    #[test]
    fn an_export_does_not_replace_a_file_that_appeared_meanwhile() {
        let target = temp("export-appeared");
        let err = write_atomic_with(&target, false, &[b"export"], || {
            std::fs::write(&target, b"theirs").expect("write");
        })
        .expect_err("refused");
        assert_eq!(err.code(), "netdemo_exists");
        assert_eq!(std::fs::read(&target).expect("read"), b"theirs");
        assert_eq!(names(&target), vec!["a.netdemo".to_string()], "no tmp file left");
        // With `overwrite` the export replaces it, as before.
        let written = write_atomic_with(&target, true, &[b"ex", b"port"], || {}).expect("replaces");
        assert_eq!(written, 6);
        assert_eq!(std::fs::read(&target).expect("read"), b"export");
    }

    /// Spec 103 § 6: a file that appeared at the target during the recording is not replaced, and hours of recording
    /// are not thrown away for it: they stay at the tmp path, which the error names.
    #[test]
    fn a_file_that_appeared_meanwhile_is_kept_and_so_is_the_recording() {
        let target = temp("appeared");
        let mut sink = FileSink::create(&target, false, b"recording").expect("creates");
        sink.write(b"-body").expect("writes");
        std::fs::write(&target, b"theirs").expect("write");
        let err = sink.finish(b"").expect_err("refused");
        assert_eq!(err.code(), "netdemo_exists");
        assert_eq!(std::fs::read(&target).expect("read"), b"theirs");
        let tmp: Vec<String> = names(&target)
            .into_iter()
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert_eq!(tmp.len(), 1, "{tmp:?}");
        assert!(err.to_string().contains(&tmp[0]), "{err}");
        let dir = target.parent().expect("a parent");
        assert_eq!(std::fs::read(dir.join(&tmp[0])).expect("read"), b"recording-body");
    }

    /// Spec 103 § 6: a rename that fails keeps the whole recording too. A directory at the target makes it fail for real.
    #[test]
    fn a_failing_rename_keeps_the_recording() {
        let target = temp("rename");
        let sink = FileSink::create(&target, true, b"recording").expect("creates");
        std::fs::create_dir(&target).expect("a directory at the target");
        let err = sink.finish(b"").expect_err("refused");
        assert_eq!(err.code(), "netdemo_io");
        let tmp: Vec<String> = names(&target)
            .into_iter()
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert_eq!(tmp.len(), 1, "{tmp:?}");
        assert!(err.to_string().contains(&tmp[0]), "{err}");
    }

    /// Replaces the writer's file with `file`, on a new writer thread: the way to make the disk misbehave for real.
    #[cfg(target_os = "linux")]
    fn swap_writer_file(sink: &mut FileSink, file: File) {
        let pool = Arc::clone(&sink.pool);
        let old = sink.thread.take();
        pool.lock().close = Some(Close::Discard);
        pool.filled.notify_all();
        let _ = old.map(std::thread::JoinHandle::join);
        pool.lock().close = None;
        let (writer_pool, paths) = (Arc::clone(&pool), sink.paths.clone());
        sink.thread = Some(std::thread::spawn(move || writer(file, &writer_pool, &paths)));
    }

    /// Spec 103 § 6: a write that fails removes the tmp file and is reported; nothing appears at the target.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_failing_disk_leaves_no_file_and_reports_it() {
        let target = temp("full");
        let mut sink = FileSink::create(&target, false, b"").expect("creates");
        // A device that refuses every write.
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .expect("/dev/full is on every Linux host this test runs on");
        swap_writer_file(&mut sink, full);
        let mut failed = None;
        for _ in 0..64 {
            match sink.write(&vec![1u8; BLOCK_BYTES]) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        match failed {
            Some(e) => {
                assert_eq!(e.code(), "netdemo_io");
                sink.abandon();
            }
            None => {
                let e = sink.finish(b"").expect_err("the write failed");
                assert_eq!(e.code(), "netdemo_io");
            }
        }
        assert!(names(&target).is_empty(), "{:?}", names(&target));
    }

    /// Spec 103 § 6: a close never waits for the disk on the caller's thread. The writer here is stuck for as long as
    /// the test likes, on a pipe nobody reads, and `begin_finish` still returns; a timeless proof, since a wait would
    /// hang this test.
    #[cfg(target_os = "linux")]
    #[test]
    fn beginning_a_close_does_not_wait_for_a_stuck_disk() {
        use std::io::Read;
        use std::os::fd::OwnedFd;
        let target = temp("stuck");
        let mut sink = FileSink::create(&target, false, b"").expect("creates");
        let (mut reader, pipe) = std::io::pipe().expect("a pipe");
        swap_writer_file(&mut sink, File::from(OwnedFd::from(pipe)));
        // Far past a pipe's buffer: the writer blocks inside `write_all`.
        sink.write(&vec![1u8; BLOCK_BYTES]).expect("takes it");
        let closing = sink.begin_finish(b"tail");
        let mut seen = 0usize;
        let mut buf = vec![0u8; 64 * 1024];
        while seen < BLOCK_BYTES + 4 {
            let n = reader.read(&mut buf).expect("reads");
            assert!(n > 0, "the writer closed early, at {seen}");
            seen += n;
        }
        assert_eq!(seen, BLOCK_BYTES + 4, "every byte, then the close");
        // A pipe cannot be synced: the close reports that, and takes the tmp file with it.
        assert_eq!(
            closing.wait().expect_err("no sync on a pipe").code(),
            "netdemo_io"
        );
        assert!(names(&target).is_empty(), "{:?}", names(&target));
    }
}
