// SPDX-License-Identifier: MPL-2.0
//! The [`Runner`]: discovers placed tests, runs them one at a time and builds the report
//! (spec 41 § 2.2, § 2.7).
//!
//! The runner is a resource advanced once per frame by [`functional_tick`], after gameplay systems.
//! Every limit and wait is a frame count. Bodies run under `catch_unwind` (ADR-1673: the one
//! `AssertUnwindSafe` boundary, because a panicking body must end its run `Aborted` instead of
//! taking the process down; shipping builds use `panic = "abort"` and compile the plugin out).

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};

use regia_app::prelude::*;
use regia_scene::components::{EntityGuid, Name};
use serde_json::Value;

use super::context::{RunData, Shared, TestBody, TestContext, TestHooks};
use super::expected::ExpectedStore;
use super::report::{PassRecord, TestRecord, TestReport};
use super::script::{FuncTestScript, ScriptBody};
use super::types::{FunctionalTest, MAX_PASSES, RunOptions, RunnerStatus, Selection, TestBodyRef, Verdict};

/// Factory of a registered Rust body.
pub type BodyFactory = fn() -> Box<dyn TestBody>;

/// Registered Rust bodies by type path.
#[derive(Resource, Default)]
pub struct BodyRegistry {
    factories: BTreeMap<String, BodyFactory>,
}

impl BodyRegistry {
    /// Registers `T` under its type path.
    pub fn register<T: TestBody + Default>(&mut self) {
        self.factories
            .insert(std::any::type_name::<T>().to_owned(), || Box::new(T::default()));
    }

    /// Registered type paths, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.factories.keys().map(String::as_str).collect()
    }
}

/// A test found in the world.
#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveredTest {
    /// The placed entity.
    pub entity: Entity,
    /// Name (the entity's `Name`, else its guid, else its id).
    pub name: String,
    /// Level path (from the options).
    pub level: String,
    /// Sort key tail: guid text.
    pub guid: String,
    /// The component.
    pub test: FunctionalTest,
}

fn entity_name(world: &World, entity: Entity) -> (String, String) {
    let guid = world
        .get::<EntityGuid>(entity)
        .map_or_else(|| format!("{:016x}", entity.to_bits()), |g| g.to_string());
    let name = world
        .get::<Name>(entity)
        .map_or_else(|| format!("test_{guid}"), |n| n.as_str().to_owned());
    (name, guid)
}

/// Finds every `FunctionalTest` in the world, ordered by (level, first tag, name, guid).
pub fn discover(world: &mut World, level: &str) -> Vec<DiscoveredTest> {
    let mut query = world.query::<(Entity, &FunctionalTest)>();
    let found: Vec<(Entity, FunctionalTest)> = query.iter(world).map(|(e, t)| (e, t.clone())).collect();
    let mut tests: Vec<DiscoveredTest> = found
        .into_iter()
        .map(|(entity, test)| {
            let (name, guid) = entity_name(world, entity);
            DiscoveredTest {
                entity,
                name,
                level: level.to_owned(),
                guid,
                test,
            }
        })
        .collect();
    tests.sort_by(|a, b| {
        (&a.level, a.test.tags.first(), &a.name, &a.guid).cmp(&(
            &b.level,
            b.test.tags.first(),
            &b.name,
            &b.guid,
        ))
    });
    tests
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Ready,
    Active,
}

struct Current {
    entry: DiscoveredTest,
    body: Option<Box<dyn TestBody>>,
    data: RunData,
    phase: Phase,
    ready_polls: u32,
    ran_labels: BTreeSet<String>,
    passes: Vec<PassRecord>,
    verdict: Verdict,
    abort_reason: Option<String>,
    total_frames: u64,
    steps: Vec<super::report::StepRecord>,
    pass_index: usize,
    finished: bool,
    viewpoint_set: bool,
    /// Wall clock of the test, for the report's `wall_ms` only: nothing decides on it.
    began: std::time::Instant,
}

/// Executes functional tests of the loaded level.
#[derive(Resource)]
pub struct Runner {
    shared: Shared,
    pending_start: Option<(Selection, RunOptions)>,
    selection: Selection,
    queue: std::collections::VecDeque<DiscoveredTest>,
    current: Option<Current>,
    report: TestReport,
    settle: bool,
    done: bool,
    started: bool,
    total: usize,
    finished_count: usize,
    iterations: u64,
    build: String,
    factories: BTreeMap<String, BodyFactory>,
}

impl Default for Runner {
    fn default() -> Self {
        Self {
            shared: Shared {
                opts: RunOptions::default(),
                store: ExpectedStore::in_memory(),
                hooks: TestHooks::default(),
                observed: BTreeMap::new(),
            },
            pending_start: None,
            selection: Selection::default(),
            queue: Default::default(),
            current: None,
            report: TestReport::new("", 0),
            settle: false,
            done: false,
            started: false,
            total: 0,
            finished_count: 0,
            iterations: 0,
            build: String::new(),
            factories: BTreeMap::new(),
        }
    }
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic with a non-text payload".to_owned()
    }
}

impl Runner {
    /// Installs the services the agent (or a test) provides.
    pub fn set_hooks(&mut self, hooks: TestHooks) {
        self.shared.hooks = hooks;
    }

    /// Sets the build text written into reports.
    pub fn set_build(&mut self, build: impl Into<String>) {
        self.build = build.into();
    }

    /// Queues a run; tests are discovered at the next [`functional_tick`]. Replaces a finished run;
    /// while a run is active use [`Runner::abort`] first.
    pub fn start(&mut self, selection: Selection, opts: RunOptions) {
        self.pending_start = Some((selection, opts));
        self.done = false;
        self.started = true;
    }

    /// Whether a run is queued or in progress (the run condition of the tick system).
    pub fn is_active(&self) -> bool {
        self.pending_start.is_some()
            || self.current.is_some()
            || !self.queue.is_empty()
            || (self.started && !self.done)
    }

    /// What the runner is doing.
    pub fn status(&self) -> RunnerStatus {
        if let Some(cur) = &self.current {
            RunnerStatus::Running {
                test: cur.entry.name.clone(),
                pass: cur.pass_index,
                frame: cur.data.frame,
            }
        } else if self.done {
            RunnerStatus::Done
        } else if self.started {
            RunnerStatus::Running {
                test: String::new(),
                pass: 0,
                frame: 0,
            }
        } else {
            RunnerStatus::Idle
        }
    }

    /// Tests finished and total selected so far.
    pub fn progress(&self) -> (usize, usize) {
        (self.finished_count, self.total)
    }

    /// The report so far.
    pub fn report(&self) -> &TestReport {
        &self.report
    }

    /// How many times a looped set has restarted.
    pub fn iterations(&self) -> u64 {
        self.iterations
    }

    /// Writes the latest values `expect_value` observed for `test` (one `key`, or all) into the expected
    /// file. Returns how many entries were written.
    pub fn bless(&mut self, test: &str, key: Option<&str>) -> Result<usize, String> {
        let Some(values) = self.shared.observed.get(test) else {
            return Err(format!(
                "test `{test}` has not observed any expected value in this session"
            ));
        };
        let picked: Vec<(String, Value)> = values
            .iter()
            .filter(|(k, _)| key.is_none_or(|want| want == k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if picked.is_empty() {
            return Err(format!(
                "no observed value `{}` for test `{test}`",
                key.unwrap_or("")
            ));
        }
        for (k, v) in &picked {
            self.shared.store.set(test, k, v.clone());
        }
        self.shared.store.save().map_err(|e| e.to_string())?;
        Ok(picked.len())
    }

    /// Stops the run: the current test is torn down at the next tick and ends `Aborted`.
    pub fn abort(&mut self, reason: &str) {
        self.queue.clear();
        self.pending_start = None;
        self.shared.opts.looped = false;
        if let Some(cur) = &mut self.current {
            cur.abort_reason = Some(reason.to_owned());
        } else {
            self.done = true;
        }
    }

    fn begin_set(&mut self, world: &mut World, selection: Selection, mut opts: RunOptions) {
        self.report = TestReport::new(self.build.clone(), opts.seed);
        let loaded = match &opts.project_root {
            Some(root) if !opts.level.is_empty() => {
                match ExpectedStore::load(&ExpectedStore::path_for_level(root, &opts.level)) {
                    Ok(store) => store,
                    Err(e) => {
                        // Spec 41: a newer (or otherwise unreadable) expected file is an error
                        // that names the file. Do not fall through to an in-memory bless that
                        // would leave the bad file in place and still report success.
                        let message = e.to_string();
                        tracing::error!(target: "regia_framework", "{message}");
                        self.report.tests.push(TestRecord {
                            name: opts.level.clone(),
                            level: opts.level.clone(),
                            tags: Vec::new(),
                            verdict: Verdict::Aborted,
                            message,
                            skipped: false,
                            passes: Vec::new(),
                            steps: Vec::new(),
                            frames: 0,
                            blessed: false,
                            notes: Vec::new(),
                            wall_ms: None,
                        });
                        self.queue.clear();
                        opts.looped = false;
                        opts.update_expected = false;
                        self.shared.opts = opts;
                        self.shared.store = ExpectedStore::in_memory();
                        self.selection = selection;
                        self.finished_count = 0;
                        self.total = 0;
                        return;
                    }
                }
            }
            _ => ExpectedStore::in_memory(),
        };
        self.shared.store = loaded;
        self.shared.opts = opts;
        self.selection = selection;
        self.finished_count = 0;
        self.fill_queue(world);
    }

    fn fill_queue(&mut self, world: &mut World) {
        let all = discover(world, &self.shared.opts.level);
        self.queue.clear();
        self.total = 0;
        for t in all {
            if !self.selection.matches(&t.name, &t.test.tags) {
                continue;
            }
            if !t.test.enabled {
                self.report.tests.push(TestRecord {
                    name: t.name.clone(),
                    level: t.level.clone(),
                    tags: t.test.tags.clone(),
                    verdict: Verdict::Pending,
                    message: "disabled".to_owned(),
                    skipped: true,
                    passes: Vec::new(),
                    steps: Vec::new(),
                    frames: 0,
                    blessed: false,
                    notes: Vec::new(),
                    wall_ms: None,
                });
                continue;
            }
            self.total += 1;
            self.queue.push_back(t);
        }
    }

    /// Advances the runner by one frame.
    #[tracing::instrument(skip_all, target = "regia_framework", name = "functional_tick")]
    pub fn advance(&mut self, world: &mut World) {
        if let Some((selection, opts)) = self.pending_start.take() {
            self.begin_set(world, selection, opts);
        }
        if self.current.is_none() {
            if self.settle {
                self.settle = false;
                return;
            }
            let Some(entry) = self.queue.pop_front() else {
                self.finish_set(world);
                return;
            };
            self.open(world, entry);
        }
        self.step_current(world);
        if self.current.as_ref().is_some_and(|c| c.finished) {
            self.close(world);
        }
    }

    fn finish_set(&mut self, world: &mut World) {
        if let Err(e) = self.shared.store.save() {
            tracing::error!(target: "regia_framework", "{e}");
        }
        if self.shared.opts.looped && self.total > 0 {
            self.iterations += 1;
            self.report.tests.clear();
            self.fill_queue(world);
        } else {
            self.done = true;
            self.started = false;
        }
    }

    fn open(&mut self, world: &mut World, mut entry: DiscoveredTest) {
        if entry.test.tags.iter().any(|t| t == "isolated")
            && let Some(reload) = self.shared.hooks.reload_level
        {
            match reload(world) {
                Ok(()) => {
                    if let Some(fresh) = discover(world, &self.shared.opts.level)
                        .into_iter()
                        .find(|t| t.name == entry.name)
                    {
                        entry = fresh;
                    }
                }
                Err(e) => tracing::error!(target: "regia_framework", "level reload failed: {e}"),
            }
        }
        if entry.test.reset_expected {
            self.shared.store.clear_test(&entry.name);
            if let Some(mut c) = world.get_mut::<FunctionalTest>(entry.entity) {
                c.reset_expected = false;
            }
        }
        let data = RunData::new(
            &entry.name,
            self.shared.opts.seed,
            entry.test.frame_limit,
            entry.test.on_limit,
        );
        let body = self.make_body(&entry);
        let mut cur = Current {
            entry,
            body: None,
            data,
            phase: Phase::Ready,
            ready_polls: 0,
            ran_labels: BTreeSet::new(),
            passes: Vec::new(),
            verdict: Verdict::Pending,
            abort_reason: None,
            total_frames: 0,
            steps: Vec::new(),
            pass_index: 0,
            finished: false,
            viewpoint_set: false,
            began: std::time::Instant::now(),
        };
        match body {
            Ok(body) => cur.body = Some(body),
            Err(reason) => cur.abort_reason = Some(reason),
        }
        self.current = Some(cur);
        // Setup runs in the first step_current call, so it shares the frame with the first ready poll.
    }

    fn make_body(&self, entry: &DiscoveredTest) -> Result<Box<dyn TestBody>, String> {
        match &entry.test.body {
            TestBodyRef::Rust(path) => self
                .factory(path)
                .map(|f| f())
                .ok_or_else(|| format!("body_not_found: no Rust body `{path}` is registered")),
            TestBodyRef::Script(path) => {
                let Some(root) = &self.shared.opts.project_root else {
                    return Err(format!("body_not_found: script `{path}` needs a project root"));
                };
                // Script paths come from levels and agents: project-relative only (ADR-1816).
                let file = regia_core::project_path::resolve_read(root, path)
                    .map_err(|e| format!("body_not_found: script `{path}`: {e}"))?;
                let text = std::fs::read_to_string(file)
                    .map_err(|e| format!("body_not_found: script `{path}`: {e}"))?;
                let script =
                    FuncTestScript::parse(&text).map_err(|e| format!("script_invalid: `{path}`: {e}"))?;
                Ok(Box::new(ScriptBody::new(script)))
            }
        }
    }

    fn factory(&self, path: &str) -> Option<BodyFactory> {
        self.factories.get(path).copied()
    }
}

/// Guarded body call: a panic becomes an abort reason.
fn guarded<R>(f: impl FnOnce() -> R) -> Result<R, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(panic_text)
}

impl Runner {
    fn step_current(&mut self, world: &mut World) {
        let Some(mut cur) = self.current.take() else {
            return;
        };
        self.step_inner(world, &mut cur);
        self.current = Some(cur);
    }

    fn step_inner(&mut self, world: &mut World, cur: &mut Current) {
        // 1. Abort requested or body missing: straight to teardown.
        if cur.abort_reason.is_some() {
            self.teardown(world, cur);
            return;
        }
        // 2. First frame of the run: setup.
        let first_frame =
            cur.pass_index == 0 && cur.ready_polls == 0 && cur.phase == Phase::Ready && cur.passes.is_empty();
        if first_frame && !self.setup(world, cur) {
            self.teardown(world, cur);
            return;
        }
        match cur.phase {
            Phase::Ready => {
                cur.ready_polls += 1;
                let Some(mut body) = cur.body.take() else { return };
                let ready = {
                    let mut cx = TestContext::new(world, &mut cur.data, &mut self.shared);
                    guarded(|| body.ready(&mut cx))
                };
                cur.body = Some(body);
                match ready {
                    Err(msg) => cur.abort_reason = Some(format!("panic: {msg}")),
                    Ok(true) => self.begin_pass(world, cur),
                    Ok(false) => {
                        let limit = cur.entry.test.ready_limit;
                        if limit > 0 && cur.ready_polls >= limit {
                            cur.abort_reason = Some("ready_timeout".to_owned());
                        }
                    }
                }
            }
            Phase::Active => {
                cur.data.frame += 1;
                let Some(mut body) = cur.body.take() else { return };
                let result = {
                    let mut cx = TestContext::new(world, &mut cur.data, &mut self.shared);
                    guarded(|| body.tick(&mut cx))
                };
                cur.body = Some(body);
                if let Err(msg) = result {
                    cur.abort_reason = Some(format!("panic: {msg}"));
                } else if cur.data.concluded.is_none()
                    && cur.data.frame_limit > 0
                    && cur.data.frame >= u64::from(cur.data.frame_limit)
                {
                    cur.data.concluded = Some((cur.data.on_limit, "frame limit".to_owned()));
                }
                if cur.abort_reason.is_none() && cur.data.concluded.is_some() {
                    self.finish_pass(world, cur);
                }
            }
        }
        if cur.abort_reason.is_some() {
            self.teardown(world, cur);
        }
    }

    fn setup(&mut self, world: &mut World, cur: &mut Current) -> bool {
        // Settings first, so a bad key aborts before the body does anything.
        let overrides: Vec<(String, String)> = cur.entry.test.settings.clone();
        for (key, text) in overrides {
            let value = parse_setting_value(&text);
            let mut cx = TestContext::new(world, &mut cur.data, &mut self.shared);
            if let Err(e) = cx.override_setting(&key, value) {
                cur.abort_reason = Some(e);
                return false;
            }
        }
        let Some(mut body) = cur.body.take() else {
            return false;
        };
        let result = {
            let mut cx = TestContext::new(world, &mut cur.data, &mut self.shared);
            guarded(|| body.setup(&mut cx))
        };
        cur.body = Some(body);
        if let Err(msg) = result {
            cur.abort_reason = Some(format!("panic: {msg}"));
            return false;
        }
        true
    }

    /// Puts the local camera at the test's viewpoint, at the first Begin (spec 41 § 2.5). An
    /// override a project installed as a hook wins over the default service.
    fn apply_viewpoint(&mut self, world: &mut World, cur: &mut Current) -> bool {
        let Some(guid) = cur.entry.test.viewpoint.clone().filter(|_| !cur.viewpoint_set) else {
            return true;
        };
        let Some(entity) = find_by_guid(world, &guid) else {
            cur.abort_reason = Some(format!("bad_params: viewpoint `{guid}` cannot be resolved"));
            return false;
        };
        let set = self
            .shared
            .hooks
            .viewpoint
            .unwrap_or(super::viewpoint::set_viewpoint);
        if let Err(e) = set(world, Some(entity)) {
            cur.abort_reason = Some(format!("bad_params: viewpoint: {e}"));
            return false;
        }
        cur.viewpoint_set = true;
        true
    }

    fn begin_pass(&mut self, world: &mut World, cur: &mut Current) {
        if !self.apply_viewpoint(world, cur) {
            return;
        }
        cur.phase = Phase::Active;
        cur.data.frame = 0;
        cur.data.concluded = None;
        cur.data.failures.clear();
        let Some(mut body) = cur.body.take() else { return };
        let result = {
            let mut cx = TestContext::new(world, &mut cur.data, &mut self.shared);
            guarded(|| body.begin(&mut cx))
        };
        cur.body = Some(body);
        match result {
            Err(msg) => cur.abort_reason = Some(format!("panic: {msg}")),
            Ok(()) if cur.data.concluded.is_some() => self.finish_pass(world, cur),
            Ok(()) => {}
        }
    }

    fn finish_pass(&mut self, world: &mut World, cur: &mut Current) {
        let (mut verdict, mut message) = cur
            .data
            .concluded
            .take()
            .unwrap_or((Verdict::Undecided, String::new()));
        if !verdict.is_decided() {
            verdict = Verdict::Undecided;
        }
        if !cur.data.failures.is_empty() {
            if verdict == Verdict::Passed {
                verdict = Verdict::Failed;
            }
            let joined = cur.data.failures.join("; ");
            message = if message.is_empty() {
                joined
            } else {
                format!("{message}; {joined}")
            };
        }
        let Some(mut body) = cur.body.take() else { return };
        let result = {
            let mut cx = TestContext::new(world, &mut cur.data, &mut self.shared);
            guarded(|| {
                body.conclude(&mut cx, verdict);
                cx.end_step();
            })
        };
        let wants_now = if result.is_ok() {
            let cx = TestContext::new(world, &mut cur.data, &mut self.shared);
            guarded(|| {
                let mut b = AssertUnwindSafe(&mut body);
                b.wants_pass_now(&cx)
            })
            .unwrap_or(false)
        } else {
            false
        };
        cur.body = Some(body);
        cur.passes.push(PassRecord {
            label: cur.data.pass_label.clone(),
            verdict,
            message,
            frames: cur.data.frame,
        });
        cur.total_frames += cur.data.frame;
        cur.steps.append(&mut cur.data.steps);
        cur.verdict = if cur.passes.len() == 1 {
            verdict
        } else {
            cur.verdict.worst(verdict)
        };
        if let Err(msg) = result {
            cur.abort_reason = Some(format!("panic: {msg}"));
            return;
        }
        if let Some(label) = &cur.data.pass_label {
            cur.ran_labels.insert(label.clone());
        }
        let next = cur
            .data
            .extra_passes
            .iter()
            .find(|l| !cur.ran_labels.contains(*l))
            .cloned();
        match next {
            None => self.teardown(world, cur),
            Some(_) if cur.passes.len() >= MAX_PASSES => {
                cur.abort_reason = Some("too_many_passes".to_owned())
            }
            Some(label) => {
                cur.pass_index += 1;
                cur.data.pass_label = Some(label);
                cur.phase = Phase::Ready;
                cur.ready_polls = 0;
                if wants_now {
                    // Start the next pass in this frame: poll ready once now.
                    self.step_inner(world, cur);
                }
            }
        }
    }

    fn teardown(&mut self, world: &mut World, cur: &mut Current) {
        if cur.finished {
            return;
        }
        if let Some(mut body) = cur.body.take() {
            let result = {
                let mut cx = TestContext::new(world, &mut cur.data, &mut self.shared);
                guarded(|| body.teardown(&mut cx))
            };
            if let Err(msg) = result {
                cur.abort_reason.get_or_insert_with(|| format!("panic: {msg}"));
            }
            cur.body = Some(body);
        }
        // Scoped entities, then settings, then the viewpoint.
        for entity in std::mem::take(&mut cur.data.scoped) {
            if world.get_entity(entity).is_ok() {
                world.despawn(entity);
            }
        }
        let hooks = self.shared.hooks;
        if let Some(set) = hooks.set_setting {
            for (key, previous) in std::mem::take(&mut cur.data.overrides).into_iter().rev() {
                if let Err(e) = set(world, &key, &previous) {
                    tracing::error!(target: "regia_framework", "restoring setting `{key}` failed: {e}");
                }
            }
        }
        if cur.viewpoint_set {
            let set = hooks.viewpoint.unwrap_or(super::viewpoint::set_viewpoint);
            if let Err(e) = set(world, None) {
                tracing::error!(target: "regia_framework", "restoring viewpoint failed: {e}");
            }
            cur.viewpoint_set = false;
        }
        cur.finished = true;
    }

    fn close(&mut self, _world: &mut World) {
        let Some(mut cur) = self.current.take() else {
            return;
        };
        let mut verdict = cur.verdict;
        let mut message = cur
            .passes
            .iter()
            .find(|p| p.verdict != Verdict::Passed)
            .or(cur.passes.last())
            .map(|p| p.message.clone())
            .unwrap_or_default();
        if let Some(reason) = &cur.abort_reason {
            verdict = Verdict::Aborted;
            message = reason.clone();
            if reason != "too_many_passes" {
                cur.passes.push(PassRecord {
                    label: cur.data.pass_label.clone(),
                    verdict: Verdict::Aborted,
                    message: reason.clone(),
                    frames: cur.data.frame,
                });
                cur.total_frames += cur.data.frame;
                cur.steps.append(&mut cur.data.steps);
            }
        } else if cur.passes.is_empty() {
            verdict = Verdict::Undecided;
        }
        if let Some(body) = &cur.body
            && let Some(extra) = body.summary(verdict)
        {
            message = if message.is_empty() {
                extra
            } else {
                format!("{message}; {extra}")
            };
        }
        self.report.tests.push(TestRecord {
            name: cur.entry.name.clone(),
            level: cur.entry.level.clone(),
            tags: cur.entry.test.tags.clone(),
            verdict,
            message,
            skipped: false,
            passes: cur.passes,
            steps: cur.steps,
            frames: cur.total_frames,
            blessed: cur.data.blessed,
            notes: cur.data.notes,
            wall_ms: Some(u64::try_from(cur.began.elapsed().as_millis()).unwrap_or(u64::MAX)),
        });
        self.finished_count += 1;
        self.settle = true;
    }
}

fn find_by_guid(world: &mut World, guid: &str) -> Option<Entity> {
    let mut query = world.query::<(Entity, &EntityGuid)>();
    query
        .iter(world)
        .find(|(_, g)| g.to_string() == guid)
        .map(|(e, _)| e)
}

/// Parses a settings override text as bool, integer, finite float, else string.
pub fn parse_setting_value(text: &str) -> Value {
    if let Ok(b) = text.parse::<bool>() {
        Value::Bool(b)
    } else if let Ok(i) = text.parse::<i64>() {
        Value::from(i)
    } else if let Some(f) = text.parse::<f64>().ok().filter(|f| f.is_finite()) {
        // Non-finite spellings (`nan`, `inf`) stay text: JSON has no such number and
        // `Value::from` would turn them into `null`.
        Value::from(f)
    } else {
        Value::String(text.to_owned())
    }
}

/// `true` while a run is queued or active (the run condition of [`functional_tick`]).
pub fn runner_is_active(runner: Option<Res<Runner>>) -> bool {
    runner.is_some_and(|r| r.is_active())
}

/// Advances the runner by one frame. Runs in `FunctionalTestSet` (after gameplay, before render extract).
pub fn functional_tick(world: &mut World) {
    let Some(mut runner) = world.remove_resource::<Runner>() else {
        return;
    };
    if let Some(registry) = world.get_resource::<BodyRegistry>() {
        runner.factories.clone_from(&registry.factories);
    }
    runner.advance(world);
    world.insert_resource(runner);
}
