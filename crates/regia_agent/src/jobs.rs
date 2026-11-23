// SPDX-License-Identifier: MPL-2.0
//! Long-running jobs behind one front door (`docs/specs/23-agent-native.md` § 4, ADR-2346):
//! `agent.job_status` and `agent.job_cancel`.
//!
//! A long operation answers a job id at once and runs off the protocol thread; the agent polls
//! `agent.job_status { job, since }` for the job's state, progress and the events it streamed
//! since the cursor `since` (the answer's `next` is the cursor for the next poll). Two kinds of
//! job answer it:
//!
//! - **Registry jobs** live in [`JobRegistry`]: a handler starts one with [`JobRegistry::start`]
//!   and hands the [`JobHandle`] to its worker thread, which reports progress, streams events and
//!   finishes it. `agent.run_tests` is one (`tests:<n>`).
//! - **Provider jobs** keep their own state and status method; `agent.job_status` forwards to it
//!   by the id's kind prefix ([`PROVIDERS`]): `render_out:<n>` reads `render.out_progress`,
//!   `dcc_bake:<id>` reads `dcc_feed.bake_status`. Their own methods keep working as before.
//!
//! Nothing here blocks: the registry holds shared state behind a mutex that a worker thread
//! updates and the protocol thread reads.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most events a job keeps; older ones are dropped (counted in `dropped`).
pub const MAX_EVENTS: usize = 4096;

/// Most events one `agent.job_status` answer carries (poll again from `next` for the rest).
pub const MAX_EVENTS_PER_POLL: usize = 512;

/// Finished registry jobs kept for `agent.job_status` after they end (the oldest go first).
pub const KEEP_FINISHED: usize = 32;

/// A job's state as `agent.job_status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Still working.
    Running,
    /// Finished; the result is in `result`.
    Done,
    /// Ended with an error; the error is in `error`.
    Failed,
    /// Stopped by `agent.job_cancel` (or the provider's own cancel).
    Cancelled,
}

impl JobState {
    /// The name the protocol uses (`running`, `done`, `failed`, `cancelled`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether the job has ended.
    #[must_use]
    pub fn finished(self) -> bool {
        self != Self::Running
    }
}

/// The mutable part of a registry job, shared between the worker and the protocol thread.
#[derive(Debug)]
struct JobShared {
    state: JobState,
    /// 0..=1 when the worker knows how far it is.
    fraction: Option<f64>,
    /// What the worker is doing now.
    message: String,
    /// Streamed events, oldest first; `first_seq` is the sequence number of `events[0]`.
    events: std::collections::VecDeque<Value>,
    first_seq: u64,
    /// Events dropped from the front because of [`MAX_EVENTS`].
    dropped: u64,
    result: Option<Value>,
    error: Option<Value>,
}

/// The worker's side of a registry job: cheap to clone, usable from any thread.
#[derive(Debug, Clone)]
pub struct JobHandle {
    shared: Arc<Mutex<JobShared>>,
    cancel: Arc<AtomicBool>,
}

impl JobHandle {
    fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(JobShared {
                state: JobState::Running,
                fraction: None,
                message: String::new(),
                events: std::collections::VecDeque::new(),
                first_seq: 0,
                dropped: 0,
                result: None,
                error: None,
            })),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut JobShared) -> R) -> R {
        let mut guard = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }

    /// Sets the progress: `fraction` in 0..=1 when known, and what the job is doing.
    pub fn progress(&self, fraction: Option<f64>, message: impl Into<String>) {
        let message = message.into();
        self.with(|s| {
            if s.state == JobState::Running {
                s.fraction = fraction.filter(|f| f.is_finite()).map(|f| f.clamp(0.0, 1.0));
                s.message = message;
            }
        });
    }

    /// Streams one event (any JSON object; `seq` is added). Ignored once the job ended.
    pub fn event(&self, mut event: Value) {
        self.with(|s| {
            if s.state.finished() {
                return;
            }
            let seq = s.first_seq + s.events.len() as u64;
            if let Some(object) = event.as_object_mut() {
                object.insert("seq".into(), json!(seq));
            }
            s.events.push_back(event);
            while s.events.len() > MAX_EVENTS {
                s.events.pop_front();
                s.first_seq += 1;
                s.dropped += 1;
            }
        });
    }

    /// Ends the job with `result` (state `done`, or `cancelled` when a cancel was asked for).
    pub fn finish(&self, result: Value) {
        self.finish_as(result, self.cancel_requested());
    }

    /// Ends the job with `result`, state `cancelled` when `cancelled` is true and `done`
    /// otherwise, whatever the cancel flag reads now. For a worker whose own outcome says whether
    /// the cancel stopped it: a cancel that arrives after its last check cannot undo the work, so
    /// the job must not read `cancelled` over finished work.
    pub fn finish_as(&self, result: Value, cancelled: bool) {
        self.with(|s| {
            if s.state == JobState::Running {
                s.state = if cancelled {
                    JobState::Cancelled
                } else {
                    JobState::Done
                };
                s.fraction = Some(1.0);
                s.result = Some(result);
            }
        });
    }

    /// Ends the job with an error: a lowercase `reason` code and a message.
    pub fn fail(&self, reason: &str, message: impl Into<String>) {
        let message = message.into();
        self.with(|s| {
            if s.state == JobState::Running {
                s.state = JobState::Failed;
                s.error = Some(json!({ "reason": reason, "message": message }));
            }
        });
    }

    /// Whether `agent.job_cancel` asked this job to stop. Workers poll it between steps.
    #[must_use]
    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// Asks the job to stop, as `agent.job_cancel` does (a host ending while a worker runs).
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    /// The job's state now.
    #[must_use]
    pub fn state(&self) -> JobState {
        self.with(|s| s.state)
    }
}

/// One registry job.
#[derive(Debug, Clone)]
struct Entry {
    kind: &'static str,
    label: String,
    started_ms: u64,
    cancellable: bool,
    handle: JobHandle,
}

/// The registry of in-process jobs (`agent.job_status`). Inserted by [`crate::AgentPlugin`];
/// handlers that start jobs insert it on demand.
#[derive(Resource, Debug, Default)]
pub struct JobRegistry {
    next: u64,
    jobs: BTreeMap<u64, Entry>,
}

impl JobRegistry {
    /// Starts a job of `kind` (lowercase, e.g. `tests`) and returns its id (`<kind>:<n>`) and
    /// the handle its worker reports through. `cancellable` jobs honour `agent.job_cancel`.
    pub fn start(
        &mut self,
        kind: &'static str,
        label: impl Into<String>,
        cancellable: bool,
    ) -> (String, JobHandle) {
        self.next += 1;
        let handle = JobHandle::new();
        self.jobs.insert(
            self.next,
            Entry {
                kind,
                label: label.into(),
                started_ms: crate::safety::unix_ms_now(),
                cancellable,
                handle: handle.clone(),
            },
        );
        self.prune();
        (format!("{kind}:{}", self.next), handle)
    }

    /// Forgets the oldest finished jobs beyond [`KEEP_FINISHED`].
    fn prune(&mut self) {
        let finished: Vec<u64> = self
            .jobs
            .iter()
            .filter(|(_, e)| e.handle.state().finished())
            .map(|(id, _)| *id)
            .collect();
        let excess = finished.len().saturating_sub(KEEP_FINISHED);
        for id in finished.into_iter().take(excess) {
            self.jobs.remove(&id);
        }
    }

    /// Whether a job of `kind` is still running.
    #[must_use]
    pub fn running(&self, kind: &str) -> Option<String> {
        self.jobs
            .iter()
            .find(|(_, e)| e.kind == kind && e.handle.state() == JobState::Running)
            .map(|(id, e)| format!("{}:{id}", e.kind))
    }

    /// Asks every running job that can be cancelled to stop and waits for them, up to `within`: a
    /// host calls this as it ends, so a job's processes do not outlive it. Answers how many are
    /// still running.
    pub fn stop_running(&self, within: std::time::Duration) -> usize {
        let running = || {
            self.jobs
                .values()
                .filter(|e| e.cancellable && e.handle.state() == JobState::Running)
        };
        for entry in running() {
            entry.handle.request_cancel();
        }
        let deadline = std::time::Instant::now() + within;
        while running().next().is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        running().count()
    }

    /// A registry job's kind, state and result, for handlers that build on a finished job.
    pub(crate) fn outcome(&self, id: &str) -> Option<(&'static str, JobState, Option<Value>)> {
        let (_, entry) = self.get(id)?;
        Some(entry.handle.with(|s| (entry.kind, s.state, s.result.clone())))
    }

    fn get(&self, id: &str) -> Option<(u64, &Entry)> {
        let (kind, n) = id.split_once(':')?;
        let n: u64 = n.parse().ok()?;
        self.jobs.get(&n).filter(|e| e.kind == kind).map(|e| (n, e))
    }

    fn summary(n: u64, entry: &Entry) -> Value {
        entry.handle.with(|s| {
            json!({
                "job": format!("{}:{n}", entry.kind),
                "kind": entry.kind,
                "label": entry.label,
                "state": s.state.name(),
                "fraction": s.fraction,
                "message": s.message,
                "started_ms": entry.started_ms,
                "cancellable": entry.cancellable,
            })
        })
    }
}

/// A job kind whose state lives with its own status method.
#[derive(Debug, Clone, Copy)]
pub struct JobProvider {
    /// The id prefix, e.g. `render_out` in `render_out:3`.
    pub kind: &'static str,
    /// The status method `agent.job_status` forwards to.
    pub method: &'static str,
    /// The parameter of that method the id's suffix fills.
    pub param: &'static str,
    /// The cancel method `agent.job_cancel` forwards to (empty: not cancellable this way).
    pub cancel: &'static str,
}

/// The provider kinds `agent.job_status` and `agent.job_cancel` forward to.
pub const PROVIDERS: &[JobProvider] = &[
    JobProvider {
        kind: "render_out",
        method: "render.out_progress",
        param: "job",
        cancel: "render.out_cancel",
    },
    JobProvider {
        kind: "dcc_bake",
        method: "dcc_feed.bake_status",
        param: "job",
        cancel: "",
    },
];

/// Splits a job id into its kind and suffix: `render_out:3` → `("render_out", "3")`.
///
/// # Errors
/// `job_id_invalid` for an id without a `<kind>:<suffix>` shape.
pub fn split_job_id(id: &str) -> Result<(&str, &str), RpcError> {
    match id.split_once(':') {
        Some((kind, rest))
            if !kind.is_empty()
                && !rest.is_empty()
                && kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            Ok((kind, rest))
        }
        _ => Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "job_id_invalid",
            format!("`job` must look like <kind>:<id>, e.g. tests:1 (got {id:?})"),
        )),
    }
}

/// A protocol error with a lowercase `data.reason`.
pub(crate) fn reason_error(
    code: i64,
    regia_code: &str,
    reason: &str,
    message: impl Into<String>,
) -> RpcError {
    let mut error = RpcError::new(
        code,
        regia_code,
        message,
        "see book/agents/jobs.md for the job kinds and reasons",
    );
    if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
        data.insert("reason".into(), json!(reason));
    }
    error
}

/// Maps a provider's own status word onto the four job states.
fn provider_state(detail: &Value) -> JobState {
    let word = detail
        .get("status")
        .or_else(|| detail.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("running");
    match word {
        "done" | "finished" | "complete" | "completed" => JobState::Done,
        "failed" | "error" => JobState::Failed,
        "cancelled" | "canceled" => JobState::Cancelled,
        _ => JobState::Running,
    }
}

/// The suffix as a JSON value: a number when it is one, else the string.
fn suffix_value(suffix: &str) -> Value {
    suffix.parse::<u64>().map_or_else(|_| json!(suffix), |n| json!(n))
}

/// The job id of a request: `job`, or `id` (the name spec 23 § 4 uses, `job_status(id)`).
fn job_param(params: &Value) -> Option<&Value> {
    params.get("job").or_else(|| params.get("id"))
}

/// `agent.job_status`: one job's state, progress and events since `since`, or (without `job`)
/// every registry job. `call` runs a provider's status method. Public for editor hosts that
/// drive the same jobs (the `agent.hot_reload` bridge, ADR-5707).
pub fn job_status(
    world: &mut World,
    params: &Value,
    call: &mut dyn FnMut(&mut World, &str, &Value) -> Result<Value, RpcError>,
) -> Result<Value, RpcError> {
    let since = crate::params::opt_u64(params, "since")?.unwrap_or(0);
    let Some(id) = job_param(params) else {
        let registry = world.get_resource_or_insert_with::<JobRegistry>(Default::default);
        let jobs: Vec<Value> = registry
            .jobs
            .iter()
            .map(|(n, e)| JobRegistry::summary(*n, e))
            .collect();
        let providers: Vec<&str> = PROVIDERS.iter().map(|p| p.kind).collect();
        return Ok(json!({ "jobs": jobs, "provider_kinds": providers }));
    };
    let Some(id) = id.as_str() else {
        return Err(RpcError::invalid_params(
            "`job` must be a string id such as \"tests:1\"",
        ));
    };
    let (kind, suffix) = split_job_id(id)?;
    if let Some(provider) = PROVIDERS.iter().find(|p| p.kind == kind) {
        let mut forwarded = json!({});
        forwarded[provider.param] = suffix_value(suffix);
        let detail = call(world, provider.method, &forwarded)?;
        let state = provider_state(&detail);
        let fraction = crate::params::opt_f64(&detail, "fraction")?;
        return Ok(json!({
            "job": id,
            "kind": kind,
            "state": state.name(),
            "finished": state.finished(),
            "fraction": fraction,
            "message": "",
            "events": [],
            "next": since,
            "dropped": 0,
            "provider": provider.method,
            "detail": detail,
        }));
    }
    let registry = world.get_resource_or_insert_with::<JobRegistry>(Default::default);
    let Some((_, entry)) = registry.get(id) else {
        return Err(reason_error(
            NOT_FOUND,
            "F0003",
            "job_not_found",
            format!(
                "no job {id} (agent.job_status without `job` lists them; finished jobs are kept for a while)"
            ),
        ));
    };
    Ok(entry.handle.with(|s| {
        let skip = since.saturating_sub(s.first_seq) as usize;
        let events: Vec<Value> = s
            .events
            .iter()
            .skip(skip)
            .take(MAX_EVENTS_PER_POLL)
            .cloned()
            .collect();
        let next = s.first_seq.max(since) + events.len() as u64;
        let more = (next as usize) < (s.first_seq as usize + s.events.len());
        json!({
            "job": id,
            "kind": entry.kind,
            "label": entry.label,
            "state": s.state.name(),
            "finished": s.state.finished(),
            "fraction": s.fraction,
            "message": s.message,
            "events": events,
            "next": next,
            "more": more,
            "dropped": s.dropped,
            "result": s.result,
            "error": s.error,
        })
    }))
}

/// `agent.job_cancel`: asks a running job to stop. Idempotent: a finished job answers its state.
pub(crate) fn job_cancel(
    world: &mut World,
    params: &Value,
    call: &mut dyn FnMut(&mut World, &str, &Value) -> Result<Value, RpcError>,
) -> Result<Value, RpcError> {
    let Some(id) = job_param(params).and_then(Value::as_str) else {
        return Err(RpcError::invalid_params("`job` (a job id string) is required"));
    };
    let (kind, suffix) = split_job_id(id)?;
    if let Some(provider) = PROVIDERS.iter().find(|p| p.kind == kind) {
        if provider.cancel.is_empty() {
            return Err(reason_error(
                INVALID_PARAMS,
                "F0002",
                "job_not_cancellable",
                format!("{kind} jobs cannot be cancelled"),
            ));
        }
        let mut forwarded = json!({});
        forwarded[provider.param] = suffix_value(suffix);
        let detail = call(world, provider.cancel, &forwarded)?;
        return Ok(json!({ "job": id, "requested": true, "provider": provider.cancel, "detail": detail }));
    }
    let registry = world.get_resource_or_insert_with::<JobRegistry>(Default::default);
    let Some((_, entry)) = registry.get(id) else {
        return Err(reason_error(
            NOT_FOUND,
            "F0003",
            "job_not_found",
            format!("no job {id}"),
        ));
    };
    if !entry.cancellable {
        return Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "job_not_cancellable",
            format!("{id} cannot be cancelled"),
        ));
    }
    let state = entry.handle.state();
    let requested = state == JobState::Running;
    if requested {
        entry.handle.cancel.store(true, Ordering::Release);
    }
    Ok(json!({ "job": id, "requested": requested, "state": state.name() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_call(_: &mut World, method: &str, _: &Value) -> Result<Value, RpcError> {
        Err(RpcError::internal(format!("unexpected call to {method}")))
    }

    #[test]
    fn a_registry_job_streams_events_by_cursor_and_finishes() {
        let mut world = World::new();
        let (id, handle) = world
            .get_resource_or_insert_with::<JobRegistry>(Default::default)
            .start("tests", "unit", true);
        assert_eq!(id, "tests:1");
        handle.progress(Some(0.25), "compiling");
        for i in 0..3 {
            handle.event(json!({ "kind": "test", "i": i }));
        }
        let first = job_status(&mut world, &json!({ "job": id }), &mut no_call).expect("status");
        assert_eq!(first["state"], "running");
        assert_eq!(first["fraction"], 0.25);
        assert_eq!(first["events"].as_array().map(Vec::len), Some(3));
        assert_eq!(first["events"][2]["seq"], 2);
        assert_eq!(first["next"], 3);
        handle.event(json!({ "kind": "test", "i": 3 }));
        let second = job_status(&mut world, &json!({ "job": id, "since": 3 }), &mut no_call).expect("status");
        assert_eq!(second["events"].as_array().map(Vec::len), Some(1));
        assert_eq!(second["events"][0]["i"], 3);
        handle.finish(json!({ "ok": true }));
        handle.event(json!({ "late": true }));
        let done = job_status(&mut world, &json!({ "job": id, "since": 4 }), &mut no_call).expect("status");
        assert_eq!(done["state"], "done");
        assert_eq!(done["finished"], true);
        assert_eq!(done["result"]["ok"], true);
        assert_eq!(
            done["events"].as_array().map(Vec::len),
            Some(0),
            "no events after the end"
        );
    }

    #[test]
    fn events_beyond_the_cap_are_dropped_from_the_front() {
        let handle = JobHandle::new();
        for i in 0..(MAX_EVENTS + 10) {
            handle.event(json!({ "i": i }));
        }
        handle.with(|s| {
            assert_eq!(s.events.len(), MAX_EVENTS);
            assert_eq!(s.dropped, 10);
            assert_eq!(s.first_seq, 10);
            assert_eq!(s.events[0]["seq"], 10);
        });
    }

    #[test]
    fn a_poll_is_bounded_and_says_there_is_more() {
        let mut world = World::new();
        let (id, handle) = world
            .get_resource_or_insert_with::<JobRegistry>(Default::default)
            .start("tests", "many", false);
        for i in 0..(MAX_EVENTS_PER_POLL + 5) {
            handle.event(json!({ "i": i }));
        }
        let page = job_status(&mut world, &json!({ "job": id }), &mut no_call).expect("status");
        assert_eq!(page["events"].as_array().map(Vec::len), Some(MAX_EVENTS_PER_POLL));
        assert_eq!(page["more"], true);
        let rest = job_status(
            &mut world,
            &json!({ "job": id, "since": page["next"] }),
            &mut no_call,
        )
        .expect("status");
        assert_eq!(rest["events"].as_array().map(Vec::len), Some(5));
        assert_eq!(rest["more"], false);
    }

    #[test]
    fn cancel_marks_the_job_and_finish_reports_cancelled() {
        let mut world = World::new();
        let (id, handle) = world
            .get_resource_or_insert_with::<JobRegistry>(Default::default)
            .start("tests", "cancel me", true);
        let answer = job_cancel(&mut world, &json!({ "job": id }), &mut no_call).expect("cancel");
        assert_eq!(answer["requested"], true);
        assert!(handle.cancel_requested());
        handle.finish(json!({}));
        assert_eq!(handle.state(), JobState::Cancelled);
        let again = job_cancel(&mut world, &json!({ "job": id }), &mut no_call).expect("idempotent");
        assert_eq!(again["requested"], false);
        assert_eq!(again["state"], "cancelled");
    }

    #[test]
    fn unknown_and_malformed_ids_answer_reasons() {
        let mut world = World::new();
        let reason = |e: RpcError| e.data.map(|d| d["reason"].clone()).unwrap_or_default();
        let missing =
            job_status(&mut world, &json!({ "job": "tests:9" }), &mut no_call).expect_err("missing");
        assert_eq!(reason(missing), "job_not_found");
        for bad in ["tests", ":1", "Tests:1", "tests:", "a-b:1"] {
            let error = job_status(&mut world, &json!({ "job": bad }), &mut no_call).expect_err("bad id");
            assert_eq!(reason(error), "job_id_invalid", "{bad}");
        }
        let (id, _) = world
            .get_resource_or_insert_with::<JobRegistry>(Default::default)
            .start("tests", "fixed", false);
        let refused =
            job_cancel(&mut world, &json!({ "job": id }), &mut no_call).expect_err("not cancellable");
        assert_eq!(reason(refused), "job_not_cancellable");
    }

    #[test]
    fn provider_ids_forward_to_their_status_method() {
        let mut world = World::new();
        let mut seen = Vec::new();
        let mut call = |_: &mut World, method: &str, params: &Value| -> Result<Value, RpcError> {
            seen.push((method.to_string(), params.clone()));
            Ok(json!({ "status": "done", "fraction": 1.0, "frame": 24 }))
        };
        let status = job_status(&mut world, &json!({ "job": "render_out:3" }), &mut call).expect("forwarded");
        assert_eq!(status["state"], "done");
        assert_eq!(status["detail"]["frame"], 24);
        assert_eq!(status["provider"], "render.out_progress");
        let refused =
            job_cancel(&mut world, &json!({ "job": "dcc_bake:abc" }), &mut call).expect_err("no cancel");
        assert_eq!(
            refused.data.map(|d| d["reason"].clone()).unwrap_or_default(),
            "job_not_cancellable"
        );
        assert_eq!(seen.len(), 1, "the refused cancel called nothing");
        assert_eq!(seen[0].0, "render.out_progress");
        assert_eq!(seen[0].1["job"], 3);
    }

    #[test]
    fn finished_jobs_beyond_the_keep_count_are_forgotten() {
        let mut registry = JobRegistry::default();
        for _ in 0..(KEEP_FINISHED + 4) {
            let (_, handle) = registry.start("tests", "x", false);
            handle.finish(json!({}));
        }
        let (_, live) = registry.start("tests", "live", false);
        assert!(registry.jobs.len() <= KEEP_FINISHED + 1);
        assert_eq!(live.state(), JobState::Running);
        assert!(registry.running("tests").is_some());
    }

    // Original tests derived from spec 23 § 4 and book/agents/jobs.md. Workers own only
    // JobHandles; this test thread owns the World, just like the protocol thread.
    mod threaded {
        use super::*;
        use std::collections::BTreeSet;
        use std::sync::{Barrier, mpsc};
        use std::thread;

        fn start(world: &mut World, label: &str, cancellable: bool) -> (String, JobHandle) {
            world
                .get_resource_or_insert_with::<JobRegistry>(Default::default)
                .start("tests", label, cancellable)
        }

        fn poll(world: &mut World, id: &str, since: u64) -> Value {
            job_status(world, &json!({ "job": id, "since": since }), &mut no_call).expect("registry status")
        }

        fn cancel(world: &mut World, id: &str) -> Value {
            job_cancel(world, &json!({ "job": id }), &mut no_call).expect("registry cancel")
        }

        fn number(value: &Value) -> u64 {
            value.as_u64().expect("unsigned protocol field")
        }

        fn check_page(page: &Value, since: u64, job: usize) {
            let events = page["events"].as_array().expect("events");
            let first = number(&page["dropped"]).max(since);
            assert!(events.len() <= MAX_EVENTS_PER_POLL);
            assert_eq!(number(&page["next"]), first + events.len() as u64);
            for (offset, event) in events.iter().enumerate() {
                assert_eq!(number(&event["seq"]), first + offset as u64);
                assert_eq!(event["job"], job, "another job's event leaked");
                assert_eq!(number(&event["worker"]) as usize % 2, job);
            }
            if page["more"] == true {
                assert_eq!(events.len(), MAX_EVENTS_PER_POLL);
            }
            assert_eq!(page["finished"], page["state"] != "running");
        }

        #[test]
        fn workers_stream_while_protocol_polls_and_old_cursors_cross_eviction() {
            const WORKERS: usize = 16;
            const ROUNDS: usize = 12;
            const BATCH: usize = 64;
            let mut world = World::new();
            let jobs = [start(&mut world, "left", true), start(&mut world, "right", true)];
            let phase = Barrier::new(WORKERS + 1);
            let mut concurrent = Vec::new();
            let mut settled = Vec::new();
            thread::scope(|scope| {
                for worker in 0..WORKERS {
                    let handle = jobs[worker % 2].1.clone();
                    let phase = &phase;
                    scope.spawn(move || {
                        for round in 0..ROUNDS {
                            phase.wait();
                            for step in 0..BATCH {
                                handle.progress(
                                    Some(worker as f64 / WORKERS as f64),
                                    format!("worker {worker}"),
                                );
                                handle.event(json!({ "job": worker % 2, "worker": worker,
                                    "round": round, "step": step, "seq": u64::MAX }));
                            }
                            phase.wait();
                        }
                    });
                }
                for round in 0..ROUNDS {
                    phase.wait();
                    // Fixed work, independent of which worker wins the mutex. Save assertions
                    // until all barriers are past, so an assertion failure cannot strand workers.
                    for _ in 0..8 {
                        for (id, _) in &jobs {
                            concurrent.push(poll(&mut world, id, 0));
                        }
                    }
                    phase.wait();
                    for (id, _) in &jobs {
                        settled.push((round, poll(&mut world, id, 0)));
                    }
                }
            });
            for (index, page) in concurrent.iter().enumerate() {
                let job = index % 2;
                check_page(page, 0, job);
                assert_eq!(page["state"], "running");
                if let Some(fraction) = page["fraction"].as_f64() {
                    let worker = (fraction * WORKERS as f64) as usize;
                    assert_eq!(worker % 2, job);
                    assert_eq!(page["message"], format!("worker {worker}"));
                }
            }
            for (index, (round, page)) in settled.iter().enumerate() {
                let total = (round + 1) * (WORKERS / 2) * BATCH;
                let dropped = total.saturating_sub(MAX_EVENTS) as u64;
                check_page(page, 0, index % 2);
                assert_eq!(page["dropped"], dropped);
                assert_eq!(page["next"], dropped + MAX_EVENTS_PER_POLL as u64);
                assert_eq!(page["more"], total > MAX_EVENTS_PER_POLL);
            }
            let total = (ROUNDS * (WORKERS / 2) * BATCH) as u64;
            let dropped = total - MAX_EVENTS as u64;
            for (job, (id, handle)) in jobs.iter().enumerate() {
                // A stale client resumes at the oldest retained sequence, including when its
                // cursor is nonzero. `dropped` is cumulative, not the gap since that client.
                let stale = poll(&mut world, id, 1);
                check_page(&stale, 1, job);
                assert_eq!(stale["dropped"], dropped);
                let mut since = dropped;
                let mut identities = BTreeSet::new();
                for page_index in 0..MAX_EVENTS.div_ceil(MAX_EVENTS_PER_POLL) {
                    let page = poll(&mut world, id, since);
                    check_page(&page, since, job);
                    assert_eq!(
                        page["more"],
                        page_index + 1 < MAX_EVENTS.div_ceil(MAX_EVENTS_PER_POLL)
                    );
                    for event in page["events"].as_array().expect("events") {
                        let identity = (
                            number(&event["worker"]),
                            number(&event["round"]),
                            number(&event["step"]),
                        );
                        assert!(identity.0 < WORKERS as u64);
                        assert!(identity.1 < ROUNDS as u64);
                        assert!(identity.2 < BATCH as u64);
                        assert!(identities.insert(identity), "duplicate event");
                    }
                    since = number(&page["next"]);
                }
                assert_eq!(identities.len(), MAX_EVENTS);
                // The last round fits wholly in the ring, regardless of mutex order.
                for worker in (job..WORKERS).step_by(2) {
                    for step in 0..BATCH {
                        assert!(identities.contains(&(worker as u64, (ROUNDS - 1) as u64, step as u64)));
                    }
                    let mut previous = None;
                    for event in identities.iter().filter(|event| event.0 == worker as u64) {
                        let ordinal = event.1 * BATCH as u64 + event.2;
                        if let Some(previous) = previous {
                            assert_eq!(ordinal, previous + 1);
                        }
                        previous = Some(ordinal);
                    }
                }
                assert_eq!(since, total);
                for cursor in [total, total + 17, u64::MAX] {
                    let empty = poll(&mut world, id, cursor);
                    check_page(&empty, cursor, job);
                    assert_eq!(empty["events"], json!([]));
                    assert_eq!(empty["more"], false);
                }
                assert_eq!(poll(&mut world, id, total)["state"], "running");
                assert!(!handle.cancel_requested(), "another job's cancel leaked");
                if job == 0 {
                    assert_eq!(cancel(&mut world, id)["requested"], true);
                }
                handle.finish(json!({ "job": job }));
                let terminal = poll(&mut world, id, total);
                assert_eq!(terminal["state"], if job == 0 { "cancelled" } else { "done" });
                assert_eq!(terminal["result"], json!({ "job": job }));
            }
        }

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum End {
            Finish,
            Fail,
            Cancel,
        }

        fn end(handle: &JobHandle, action: End) {
            match action {
                End::Finish => handle.finish(json!({ "winner": "finish" })),
                End::Fail => handle.fail("threaded_failure", "worker failed"),
                End::Cancel => unreachable!("cancel belongs to the protocol thread"),
            }
        }

        fn check_terminal(page: &Value) {
            assert_eq!(page["finished"], true);
            match page["state"].as_str().expect("state") {
                "done" | "cancelled" => {
                    assert_eq!(page["result"], json!({ "winner": "finish" }));
                    assert_eq!(page["fraction"], 1.0);
                    assert_eq!(page["error"], Value::Null);
                }
                "failed" => {
                    assert_eq!(page["result"], Value::Null);
                    assert_eq!(
                        page["error"],
                        json!({ "reason": "threaded_failure", "message": "worker failed" })
                    );
                }
                other => panic!("unexpected terminal state {other}"),
            }
        }

        #[test]
        fn finish_fail_cancel_all_six_orders_preserve_the_first_terminal_state() {
            use End::{Cancel, Fail, Finish};
            for order in [
                [Finish, Fail, Cancel],
                [Finish, Cancel, Fail],
                [Fail, Finish, Cancel],
                [Fail, Cancel, Finish],
                [Cancel, Finish, Fail],
                [Cancel, Fail, Finish],
            ] {
                let mut world = World::new();
                let (id, handle) = start(&mut world, "ordered", true);
                handle.progress(Some(0.25), "before");
                handle.event(json!({ "before": true }));
                let mut expected = "running";
                let mut requested = false;
                let mut terminal = None;
                // A channel acknowledgement fixes the order without sleeps or scheduler bets.
                thread::scope(|scope| {
                    // Keep the sender inside the scope: a failed assertion drops it before
                    // scope joins the receiver, rather than leaving the worker waiting.
                    let (tx, rx) = mpsc::channel();
                    let (ack_tx, ack_rx) = mpsc::channel();
                    let worker = handle.clone();
                    scope.spawn(move || {
                        for action in rx {
                            end(&worker, action);
                            ack_tx.send(()).expect("protocol still listening");
                        }
                    });
                    for action in order {
                        if action == Cancel {
                            let answer = cancel(&mut world, &id);
                            assert_eq!(answer["requested"], expected == "running");
                            assert_eq!(answer["state"], expected);
                            requested |= expected == "running";
                        } else {
                            tx.send(action).expect("worker alive");
                            ack_rx.recv().expect("worker completed action");
                            if expected == "running" {
                                expected = if action == Fail {
                                    "failed"
                                } else if requested {
                                    "cancelled"
                                } else {
                                    "done"
                                };
                            }
                        }
                        let page = poll(&mut world, &id, 0);
                        assert_eq!(page["state"], expected, "{order:?}");
                        assert_eq!(handle.cancel_requested(), requested);
                        if expected != "running" {
                            check_terminal(&page);
                            if let Some(first) = &terminal {
                                assert_eq!(&page, first);
                            }
                            terminal = Some(page);
                        }
                    }
                    drop(tx);
                });
                let before = poll(&mut world, &id, 0);
                thread::scope(|scope| {
                    scope.spawn(|| {
                        handle.progress(Some(0.9), "late");
                        handle.event(json!({ "late": true }));
                        handle.finish(json!({ "late": true }));
                        handle.fail("late_failure", "late");
                    });
                });
                assert_eq!(
                    poll(&mut world, &id, 0),
                    before,
                    "late writes resurrected or changed a terminal job"
                );
                assert_eq!(cancel(&mut world, &id)["requested"], false);
            }
        }

        #[test]
        fn finite_terminal_races_allow_any_winner_and_never_resurrect() {
            for _ in 0..32 {
                let mut world = World::new();
                let (id, handle) = start(&mut world, "race", true);
                let barrier = Barrier::new(4);
                let mut pages = Vec::new();
                let cancellation = thread::scope(|scope| {
                    for action in [End::Finish, End::Fail] {
                        let worker = handle.clone();
                        let barrier = &barrier;
                        scope.spawn(move || {
                            barrier.wait();
                            end(&worker, action);
                        });
                    }
                    let worker = handle.clone();
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        for n in 0..32 {
                            worker.progress(Some(0.5), "racing");
                            worker.event(json!({ "n": n }));
                        }
                    });
                    barrier.wait();
                    let cancellation = cancel(&mut world, &id);
                    for _ in 0..8 {
                        pages.push(poll(&mut world, &id, 0));
                    }
                    cancellation
                });
                assert_eq!(cancellation["requested"], cancellation["state"] == "running");
                let terminal = poll(&mut world, &id, 0);
                check_terminal(&terminal);
                if cancellation["requested"] == false {
                    assert_eq!(cancellation["state"], terminal["state"]);
                }
                let mut seen_terminal = false;
                for page in pages {
                    if page["finished"] == true {
                        seen_terminal = true;
                        assert_eq!(page, terminal);
                    } else {
                        assert!(!seen_terminal, "terminal state returned to running");
                        assert_eq!(page["state"], "running");
                    }
                }
                thread::scope(|scope| {
                    scope.spawn(|| {
                        handle.finish(json!({ "late": true }));
                        handle.fail("late_failure", "late");
                        handle.progress(None, "late");
                        handle.event(json!({ "late": true }));
                    });
                });
                assert_eq!(poll(&mut world, &id, 0), terminal);
                assert_eq!(cancel(&mut world, &id)["requested"], false);
            }
        }

        #[test]
        fn pruning_keeps_running_jobs_and_evicted_handles_cannot_reinsert_them() {
            let mut world = World::new();
            let (live_id, live) = start(&mut world, "oldest still running", false);
            let jobs: Vec<_> = (0..KEEP_FINISHED + 4)
                .map(|_| start(&mut world, "finished", true))
                .collect();
            // Complete in reverse creation order: pruning is by registry id, not completion time.
            for (_, handle) in jobs.iter().rev() {
                thread::scope(|scope| {
                    scope.spawn(|| handle.finish(json!({ "ok": true })));
                });
            }
            let barrier = Barrier::new(2);
            let ((new_id, new), listing, live_page) = thread::scope(|scope| {
                scope.spawn(|| {
                    barrier.wait();
                    for n in 0..MAX_EVENTS_PER_POLL + 7 {
                        live.progress(Some(0.5), "surviving pruning");
                        live.event(json!({ "n": n }));
                    }
                });
                barrier.wait();
                let new_job = start(&mut world, "new running", true);
                let listing = job_status(&mut world, &json!({}), &mut no_call).expect("list");
                let page = poll(&mut world, &live_id, 0);
                (new_job, listing, page)
            });
            assert_eq!(live_page["state"], "running");
            assert!(live_page["events"].as_array().expect("events").len() <= MAX_EVENTS_PER_POLL);
            let surviving = poll(&mut world, &live_id, 0);
            assert_eq!(surviving["next"], MAX_EVENTS_PER_POLL);
            assert_eq!(surviving["more"], true);
            let tail = poll(&mut world, &live_id, MAX_EVENTS_PER_POLL as u64);
            assert_eq!(tail["events"].as_array().expect("events").len(), 7);
            assert_eq!(tail["next"], MAX_EVENTS_PER_POLL + 7);
            let listed: BTreeSet<_> = listing["jobs"]
                .as_array()
                .expect("jobs")
                .iter()
                .map(|job| job["job"].as_str().expect("job id").to_owned())
                .collect();
            let expected: BTreeSet<_> = jobs
                .iter()
                .skip(4)
                .map(|(id, _)| id.clone())
                .chain([live_id.clone(), new_id.clone()])
                .collect();
            assert_eq!(listed, expected);
            for (id, handle) in jobs.iter().take(4) {
                thread::scope(|scope| {
                    scope.spawn(|| {
                        handle.event(json!({ "late": true }));
                        handle.progress(None, "late");
                        handle.finish(json!({ "late": true }));
                        handle.fail("late_failure", "late");
                    });
                });
                for result in [
                    job_status(&mut world, &json!({ "job": id, "since": 0 }), &mut no_call),
                    job_cancel(&mut world, &json!({ "job": id }), &mut no_call),
                ] {
                    let error = result.expect_err("pruned job stays forgotten");
                    assert_eq!(error.code, NOT_FOUND);
                    let data = error.data.expect("error data");
                    assert_eq!(data["code"], "F0003");
                    assert_eq!(data["reason"], "job_not_found");
                }
                assert_eq!(handle.state(), JobState::Done);
            }
            let refused = job_cancel(&mut world, &json!({ "job": live_id }), &mut no_call)
                .expect_err("noncancellable live job");
            assert_eq!(refused.data.expect("error data")["reason"], "job_not_cancellable");
            assert!(!live.cancel_requested());
            assert_eq!(poll(&mut world, &live_id, 0)["state"], "running");
            assert_eq!(poll(&mut world, &new_id, 0)["state"], "running");
            live.finish(json!({}));
            new.finish(json!({}));
        }
    }

    /// `finish_as` ends the job as its worker says, whatever the cancel flag reads: a late cancel
    /// does not turn finished work `cancelled`; `finish` still follows the flag.
    #[test]
    fn finish_as_follows_the_worker_not_a_late_cancel() {
        let mut registry = JobRegistry::default();
        let (_, done) = registry.start("import", "late cancel", true);
        done.request_cancel();
        done.finish_as(json!({ "cancelled": false }), false);
        assert_eq!(done.state(), JobState::Done);

        let (_, cancelled) = registry.start("import", "cancelled", true);
        cancelled.finish_as(json!({ "cancelled": true }), true);
        assert_eq!(cancelled.state(), JobState::Cancelled);

        let (_, flagged) = registry.start("import", "finish", true);
        flagged.request_cancel();
        flagged.finish(json!({}));
        assert_eq!(
            flagged.state(),
            JobState::Cancelled,
            "finish still follows the flag"
        );

        // An ended job stays as it ended.
        flagged.finish_as(json!({}), false);
        assert_eq!(flagged.state(), JobState::Cancelled);
    }
}
