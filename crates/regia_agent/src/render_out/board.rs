// SPDX-License-Identifier: MPL-2.0
//! The progress board (ADR-2582): what `render.out_progress` would answer about the running job,
//! published by the app thread before each step of the job, so that a connection thread can answer
//! a progress poll while the app thread is inside a long step.
//!
//! A job's first render on a host compiles pipelines and uploads the scene on the app thread
//! (30 s on Red-Rouge's scene on an RTX 5090, ADR-1965). Requests are answered by the app thread
//! between frames, so a poll sent meanwhile waited out its 30 s and got `F0006`. Now the connection
//! that queued a poll waits [`BOARD_ANSWER_AFTER`] for the app; if the app has not answered by then
//! and the board says the app thread is inside a render-out step of the job asked about, the
//! connection answers from the board (`app_busy: true`, `busy_ms`, `warming` while the job has not
//! finished a render yet) and marks the queued request as answered, so the app does not run it
//! later. Any other case (no step running, another job, a refused parameter, a permission profile
//! without `read`) waits for the app exactly as before.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use serde_json::{Value, json};

/// How long a connection waits for the app thread before it answers a progress poll from the
/// board. Far above an app frame of a job (milliseconds to a few hundred), far below the poll's
/// own timeout (30 s by default).
pub const BOARD_ANSWER_AFTER: Duration = Duration::from_millis(250);

/// The progress board of one agent server (see the module docs). Cloning shares the board: the
/// server's connection threads hold one clone, the world holds another as a resource.
#[derive(Resource, Clone, Default)]
pub struct RenderOutBoard(Arc<Mutex<BoardState>>);

#[derive(Default)]
struct BoardState {
    /// When the app thread entered the render-out step it is inside; `None` between steps.
    busy_since: Option<Instant>,
    /// Whether the master token may call `render.out_progress` (its permission is `read`).
    read_allowed: bool,
    /// The job of the last step.
    job: u64,
    /// The running-job answer of `render.out_progress` at the last step, without `hashes`.
    answer: Value,
    /// The job's frame hashes so far, for the `hashes` window (kept between steps, so each step
    /// copies only the new ones).
    hashes: Vec<u64>,
    /// How many times [`RenderOutBoard::answer`] looked (tests).
    #[cfg(test)]
    reads: u64,
}

impl RenderOutBoard {
    fn lock(&self) -> std::sync::MutexGuard<'_, BoardState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The app thread enters a step of `job`: `answer` is what a poll would get now.
    pub(crate) fn enter(&self, job: u64, answer: Value, hashes: &[u64], read_allowed: bool) {
        let mut state = self.lock();
        if state.job != job || state.hashes.len() > hashes.len() {
            state.hashes.clear();
        }
        let have = state.hashes.len();
        state.hashes.extend_from_slice(&hashes[have..]);
        state.job = job;
        state.answer = answer;
        state.read_allowed = read_allowed;
        state.busy_since = Some(Instant::now());
    }

    /// The app thread left the step.
    pub(crate) fn leave(&self) {
        self.lock().busy_since = None;
    }

    /// Whether the app thread is inside a render-out step now.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.lock().busy_since.is_some()
    }

    /// The answer to a `render.out_progress` with `params` from the board, when the app thread is
    /// inside a step of the job asked about (no `job`, or the running job's id) and the call is
    /// allowed; `None` otherwise (the caller waits for the app).
    pub(crate) fn answer(&self, params: &Value) -> Option<Value> {
        if crate::contract::check_params(crate::contract::contract("render.out_progress"), params).is_err() {
            return None;
        }
        let state = self.lock();
        #[cfg(test)]
        let state = {
            let mut state = state;
            state.reads += 1;
            state
        };
        let since = state.busy_since.filter(|_| state.read_allowed)?;
        // A wrong-typed `job` or `hashes` is left to the app thread, whose handler refuses it
        // with `invalid_params` (ADR-2622).
        let wanted = crate::params::opt_u64(params, "job").ok()?;
        if wanted.is_some_and(|id| id != state.job) {
            return None;
        }
        let mut answer = state.answer.clone();
        answer["hashes"] = super::hash_window(&state.hashes, params).ok()?;
        answer["app_busy"] = json!(true);
        answer["busy_ms"] = json!(since.elapsed().as_millis() as u64);
        Some(answer)
    }
}

#[cfg(test)]
impl RenderOutBoard {
    /// How many times [`Self::answer`] looked.
    pub(crate) fn reads(&self) -> u64 {
        self.lock().reads
    }
}

/// Clears the board when dropped: a step that returns early or panics leaves no busy mark.
pub(crate) struct BusyGuard(pub(crate) Option<RenderOutBoard>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Some(board) = &self.0 {
            board.leave();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(job: u64) -> Value {
        json!({ "job": job, "status": "running", "phase": "warmup", "frame": 0, "warming": true })
    }

    #[test]
    fn an_idle_board_answers_nothing() {
        let board = RenderOutBoard::default();
        assert!(!board.busy());
        assert_eq!(board.answer(&json!({})), None);
    }

    #[test]
    fn a_busy_board_answers_the_running_job_only() {
        let board = RenderOutBoard::default();
        board.enter(3, running(3), &[], true);
        let answer = board.answer(&json!({})).expect("busy");
        assert_eq!(answer["job"], json!(3));
        assert_eq!(answer["app_busy"], json!(true));
        assert_eq!(answer["warming"], json!(true));
        assert!(answer["busy_ms"].is_u64());
        assert_eq!(answer["hashes"], Value::Null);
        assert!(board.answer(&json!({ "job": 3 })).is_some());
        assert_eq!(
            board.answer(&json!({ "job": 2 })),
            None,
            "a finished job is the app's to answer"
        );
        board.leave();
        assert_eq!(board.answer(&json!({})), None);
    }

    #[test]
    fn refused_parameters_and_a_profile_without_read_wait_for_the_app() {
        let board = RenderOutBoard::default();
        board.enter(1, running(1), &[], true);
        assert_eq!(board.answer(&json!([1, 2])), None, "params must be an object");
        board.enter(1, running(1), &[], false);
        assert_eq!(board.answer(&json!({})), None);
    }

    #[test]
    fn the_hash_window_follows_the_jobs_hashes() {
        let board = RenderOutBoard::default();
        board.enter(1, running(1), &[1, 2], true);
        board.enter(1, running(1), &[1, 2, 3], true);
        let answer = board
            .answer(&json!({ "hashes": true, "hashes_from": 1 }))
            .expect("busy");
        assert_eq!(answer["hashes"]["from"], json!(1));
        assert_eq!(answer["hashes"]["hashes"].as_array().map(Vec::len), Some(2));
        // A new job starts its own list.
        board.enter(2, running(2), &[9], true);
        let answer = board.answer(&json!({ "hashes": true })).expect("busy");
        assert_eq!(answer["hashes"]["hashes"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn the_guard_clears_the_mark() {
        let board = RenderOutBoard::default();
        board.enter(1, running(1), &[], true);
        drop(BusyGuard(Some(board.clone())));
        assert!(!board.busy());
    }
}
