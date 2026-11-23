// SPDX-License-Identifier: MPL-2.0
//! "Bytes in a later frame": [`BytesRequest`] reads a file through the [`Vfs`] on the IO task pool, so
//! one call works for desktop files, Android APK assets and (later) web fetches.
//!
//! Design after miniquad's `fs::load_file` / macroquad's `load_file` (survey catalogue #19, design
//! only): start the read, keep a handle, poll it each frame.

use std::sync::Arc;

use regia_core::tasks::{self, Task};
use regia_platform::{MountPoint, Vfs, VfsPath};

/// A file read in flight. Poll it once per frame with [`BytesRequest::poll`].
pub struct BytesRequest {
    path: String,
    state: State,
}

enum State {
    Running(Task<Result<Arc<[u8]>, String>>),
    Done(Option<Result<Arc<[u8]>, String>>),
}

impl BytesRequest {
    /// Starts reading `path` (`mount://relative`, or a bare relative path meaning `assets://`).
    pub fn start(vfs: &Vfs, path: &str) -> Self {
        let parsed = match parse_path(path) {
            Ok(parsed) => parsed,
            Err(err) => return Self::failed(path, err),
        };
        let vfs = vfs.clone();
        let task = tasks::spawn_io(async move { vfs.read_sync(&parsed).map_err(|err| err.to_string()) });
        Self {
            path: path.to_string(),
            state: State::Running(task),
        }
    }

    /// A request that already failed with `error`.
    pub fn failed(path: &str, error: String) -> Self {
        Self {
            path: path.to_string(),
            state: State::Done(Some(Err(error))),
        }
    }

    /// The path this request reads.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// `None` while the read runs; then the bytes or the error, exactly once (later polls return
    /// `None` again).
    pub fn poll(&mut self) -> Option<Result<Arc<[u8]>, String>> {
        match &mut self.state {
            State::Running(task) if !task.is_finished() => None,
            State::Running(_) => {
                let State::Running(task) = std::mem::replace(&mut self.state, State::Done(None)) else {
                    return None;
                };
                Some(tasks::block_on(task))
            }
            State::Done(result) => result.take(),
        }
    }

    /// Waits for the read (blocking this thread). For start-up code and tests, not per-frame code.
    pub fn wait(mut self) -> Result<Arc<[u8]>, String> {
        match std::mem::replace(&mut self.state, State::Done(None)) {
            State::Running(task) => tasks::block_on(task),
            State::Done(result) => result.unwrap_or_else(|| Err(format!("{}: already taken", self.path))),
        }
    }
}

impl Drop for BytesRequest {
    fn drop(&mut self) {
        let unread = match std::mem::replace(&mut self.state, State::Done(None)) {
            // Finished but never polled: collect it (does not block) so an error is not lost.
            State::Running(task) if task.is_finished() => tasks::block_on(task).err(),
            State::Running(_) => {
                tracing::debug!(target: "regia_quick", "load_bytes {}: dropped before it finished; no further polls, a read already running is not interrupted", self.path);
                None
            }
            State::Done(Some(Err(err))) => Some(err),
            State::Done(_) => None,
        };
        if let Some(err) = unread {
            tracing::warn!(target: "regia_quick", "load_bytes {}: dropped with an unread error: {err}", self.path);
        }
    }
}

/// Parses a `load_bytes` path: `mount://relative`, or a bare relative path in `assets://`.
pub(crate) fn parse_path(path: &str) -> Result<VfsPath, String> {
    let parsed = if path.contains("://") {
        VfsPath::parse(path)
    } else {
        VfsPath::new(MountPoint::Assets, path)
    };
    parsed.map_err(|err| format!("{path}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_platform::DirMount;

    #[test]
    fn dropping_requests_in_every_state_does_not_panic() {
        regia_core::tasks::init(regia_core::tasks::TaskPoolOptions::default());
        // An unread error (logged), a taken result, and a read dropped before or after it finished.
        drop(BytesRequest::failed("a.bin", "boom".to_string()));
        let mut taken = BytesRequest::failed("b.bin", "boom".to_string());
        assert!(taken.poll().is_some());
        drop(taken);
        let vfs = Vfs::new();
        drop(BytesRequest::start(&vfs, "assets://missing.bin"));
    }

    #[test]
    fn reads_a_file_and_reports_a_missing_one_by_path() {
        regia_core::tasks::init(regia_core::tasks::TaskPoolOptions::default());
        let dir = regia_test_utils::unique_temp_dir("quick-load");
        std::fs::write(dir.join("hello.txt"), b"hi").expect("write");
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::Assets, DirMount::read_only(&dir));
        let bytes = BytesRequest::start(&vfs, "hello.txt").wait().expect("read");
        assert_eq!(bytes.as_ref(), b"hi");
        let missing = BytesRequest::start(&vfs, "assets://nope.bin").wait();
        let err = missing.expect_err("a missing file fails");
        assert!(err.contains("nope.bin"), "the error names the file: {err}");
        assert!(parse_path("../escape").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_finished_result_is_handed_out_once() {
        let mut request = BytesRequest::failed("x.bin", "boom".to_string());
        assert_eq!(request.poll().expect("ready").expect_err("failed"), "boom");
        assert!(request.poll().is_none(), "the result is handed out once");
        let taken = request.wait().expect_err("already taken");
        assert!(taken.contains("x.bin"), "{taken}");
    }
}
