// SPDX-License-Identifier: MPL-2.0
//! The [`TestBody`] trait and the [`TestContext`] a body works through (spec 41 § 2.3, § 2.4).

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use serde_json::Value;

use super::assert::{self, Relation};
use super::expected::{DEFAULT_RELATIVE_TOLERANCE, ExpectedStore, diff_values};
use super::report::StepRecord;
use super::types::{RunOptions, Verdict};
use regia_core::math::{Mat4, Quat, Vec2, Vec3, Vec4};
use regia_scene::Transform;

/// Condition evaluator hook: `Ok((holds, detail))` or why it could not be evaluated.
pub type ConditionFn = fn(&mut World, &Value) -> Result<(bool, String), String>;
/// Setting reader hook.
pub type GetSettingFn = fn(&mut World, &str) -> Option<Value>;
/// Setting writer hook.
pub type SetSettingFn = fn(&mut World, &str, &Value) -> Result<(), String>;
/// Input injection hook: action, value, frames.
pub type InjectFn = fn(&mut World, &str, [f32; 2], u32) -> Result<(), String>;
/// Viewpoint hook.
pub type ViewpointFn = fn(&mut World, Option<Entity>) -> Result<(), String>;
/// Level reload hook.
pub type ReloadFn = fn(&mut World) -> Result<(), String>;

/// Engine services a test may need that live in higher layers; the agent crate installs them.
/// Every field is optional: a missing service makes the matching feature fail with a clear reason.
#[derive(Clone, Copy, Default)]
pub struct TestHooks {
    /// Evaluates one `play.expect` condition (JSON) in the current frame: `Ok((holds, detail))`.
    pub condition: Option<ConditionFn>,
    /// Reads an engine setting by path.
    pub get_setting: Option<GetSettingFn>,
    /// Writes an engine setting by path.
    pub set_setting: Option<SetSettingFn>,
    /// Injects an input action value for a number of frames.
    pub inject: Option<InjectFn>,
    /// Moves the local camera to an entity's pose (`Some`) or restores it (`None`).
    pub viewpoint: Option<ViewpointFn>,
    /// Reloads the loaded level (tests tagged `isolated`).
    pub reload_level: Option<ReloadFn>,
}

/// A small deterministic generator (SplitMix64) seeded from the run seed and the test name.
#[derive(Clone, Debug)]
pub struct TestRng(u64);

impl TestRng {
    pub(crate) fn seeded(seed: u64, name: &str) -> Self {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for b in name.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        Self(seed ^ h)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A float in `[0, 1)`.
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// Mutable state of the run in progress, owned by the runner and lent to the context.
pub(crate) struct RunData {
    pub name: String,
    pub frame: u64,
    pub pass_label: Option<String>,
    pub concluded: Option<(Verdict, String)>,
    pub failures: Vec<String>,
    pub notes: Vec<String>,
    pub steps: Vec<StepRecord>,
    pub open_step: Option<(String, u64, usize)>,
    pub scoped: Vec<Entity>,
    pub overrides: Vec<(String, Value)>,
    pub extra_passes: BTreeSet<String>,
    pub frame_limit: u32,
    pub on_limit: Verdict,
    pub rng: TestRng,
    pub blessed: bool,
}

impl RunData {
    pub(crate) fn new(name: &str, seed: u64, frame_limit: u32, on_limit: Verdict) -> Self {
        Self {
            name: name.to_owned(),
            frame: 0,
            pass_label: None,
            concluded: None,
            failures: Vec::new(),
            notes: Vec::new(),
            steps: Vec::new(),
            open_step: None,
            scoped: Vec::new(),
            overrides: Vec::new(),
            extra_passes: BTreeSet::new(),
            frame_limit,
            on_limit,
            rng: TestRng::seeded(seed, name),
            blessed: false,
        }
    }
}

/// Run-set state shared by every test: options, expected values, hooks.
pub(crate) struct Shared {
    pub opts: RunOptions,
    pub store: ExpectedStore,
    pub hooks: TestHooks,
    /// Latest value each `expect_value` call saw, by test and key, for `Runner::bless`.
    pub observed: BTreeMap<String, BTreeMap<String, Value>>,
}

/// What a body works through: the world, the run's state and the assertions.
pub struct TestContext<'w> {
    world: &'w mut World,
    run: &'w mut RunData,
    shared: &'w mut Shared,
}

impl<'w> TestContext<'w> {
    pub(crate) fn new(world: &'w mut World, run: &'w mut RunData, shared: &'w mut Shared) -> Self {
        Self { world, run, shared }
    }

    /// The world.
    pub fn world(&mut self) -> &mut World {
        self.world
    }

    /// Frames since Begin of the current pass.
    pub fn frame(&self) -> u64 {
        self.run.frame
    }

    /// A parameter given with `--param key=value`.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.shared.opts.params.get(key).map(String::as_str)
    }

    /// The seeded generator of this test.
    pub fn rng(&mut self) -> &mut TestRng {
        &mut self.run.rng
    }

    /// Label of the active pass; `None` for the first pass.
    pub fn pass_label(&self) -> Option<&str> {
        self.run.pass_label.as_deref()
    }

    /// Queues an extra pass; labels run in sorted order, each once.
    pub fn add_pass(&mut self, label: &str) {
        self.run.extra_passes.insert(label.to_owned());
    }

    /// Concludes the pass. A second call in the same pass is ignored with a log line.
    pub fn conclude(&mut self, verdict: Verdict, message: &str) {
        if self.run.concluded.is_some() {
            tracing::warn!(target: "regia_framework", test = %self.run.name, "second conclude in one pass ignored");
            return;
        }
        self.run.concluded = Some((verdict, message.to_owned()));
    }

    /// Whether the pass has been concluded.
    pub fn is_concluded(&self) -> bool {
        self.run.concluded.is_some()
    }

    /// Changes the frame limit of the current and later passes.
    pub fn set_frame_limit(&mut self, frames: u32, on_limit: Verdict) {
        self.run.frame_limit = frames;
        self.run.on_limit = on_limit;
    }

    /// Opens a named step (closing an open one).
    pub fn begin_step(&mut self, name: &str) {
        self.end_step();
        self.run.open_step = Some((name.to_owned(), self.run.frame, self.run.failures.len()));
    }

    /// Closes the open step; it fails when an assertion failed inside it.
    pub fn end_step(&mut self) {
        if let Some((name, start, failures_at_open)) = self.run.open_step.take() {
            let verdict = if self.run.failures.len() > failures_at_open {
                Verdict::Failed
            } else {
                Verdict::Passed
            };
            self.run.steps.push(StepRecord {
                name,
                frames: self.run.frame.saturating_sub(start),
                verdict,
            });
        }
    }

    /// Name of the open step.
    pub fn current_step(&self) -> Option<&str> {
        self.run.open_step.as_ref().map(|(n, _, _)| n.as_str())
    }

    /// Adds a note to the log (and the tracing output).
    pub fn note(&mut self, text: &str) {
        tracing::info!(target: "regia_framework", test = %self.run.name, "{text}");
        self.run.notes.push(text.to_owned());
    }

    /// Logs a warning.
    pub fn warn(&mut self, text: &str) {
        tracing::warn!(target: "regia_framework", test = %self.run.name, "{text}");
    }

    /// Logs an error without failing the test (use an assertion to fail).
    pub fn error(&mut self, text: &str) {
        tracing::error!(target: "regia_framework", test = %self.run.name, "{text}");
    }

    /// Spawns a bundle that is despawned at teardown.
    pub fn spawn_scoped(&mut self, bundle: impl Bundle) -> Entity {
        let entity = self.world.spawn(bundle).id();
        self.run.scoped.push(entity);
        entity
    }

    /// Despawns `entity` at teardown.
    pub fn despawn_at_teardown(&mut self, entity: Entity) {
        self.run.scoped.push(entity);
    }

    /// Overrides an engine setting until teardown. An unknown key fails with `setting_unknown`.
    pub fn override_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        let hooks = self.shared.hooks;
        let (Some(get), Some(set)) = (hooks.get_setting, hooks.set_setting) else {
            return Err(format!("setting_unknown: no settings service for `{key}`"));
        };
        let Some(previous) = get(self.world, key) else {
            return Err(format!("setting_unknown: `{key}`"));
        };
        set(self.world, key, &value).map_err(|e| format!("setting_unknown: `{key}`: {e}"))?;
        if !self.run.overrides.iter().any(|(k, _)| k == key) {
            self.run.overrides.push((key.to_owned(), previous));
        }
        Ok(())
    }

    /// How many failures this pass has recorded so far.
    pub fn failure_count(&self) -> usize {
        self.run.failures.len()
    }

    /// Records a failure message (the assertion helpers call this).
    pub fn fail(&mut self, message: &str) {
        let text = match &self.run.open_step {
            Some((step, _, _)) => format!("[{step}] {message}"),
            None => message.to_owned(),
        };
        self.run.failures.push(text);
    }

    fn check(&mut self, result: Result<(), String>) -> bool {
        match result {
            Ok(()) => true,
            Err(message) => {
                self.fail(&message);
                false
            }
        }
    }

    /// `f32` equality within `tolerance`.
    pub fn near_f32(&mut self, actual: f32, expected: f32, tolerance: f32, label: &str) -> bool {
        self.check(assert::near_f32(actual, expected, tolerance, label))
    }
    /// `f64` equality within `tolerance`.
    pub fn near_f64(&mut self, actual: f64, expected: f64, tolerance: f64, label: &str) -> bool {
        self.check(assert::near_f64(actual, expected, tolerance, label))
    }
    /// `Vec2` equality within `tolerance`.
    pub fn near_vec2(&mut self, actual: Vec2, expected: Vec2, tolerance: f32, label: &str) -> bool {
        self.check(assert::near_vec2(actual, expected, tolerance, label))
    }
    /// `Vec3` equality within `tolerance`.
    pub fn near_vec3(&mut self, actual: Vec3, expected: Vec3, tolerance: f32, label: &str) -> bool {
        self.check(assert::near_vec3(actual, expected, tolerance, label))
    }
    /// `Vec4` equality within `tolerance`.
    pub fn near_vec4(&mut self, actual: Vec4, expected: Vec4, tolerance: f32, label: &str) -> bool {
        self.check(assert::near_vec4(actual, expected, tolerance, label))
    }
    /// `Quat` equality (angle in radians; `q` equals `-q`).
    pub fn near_quat(&mut self, actual: Quat, expected: Quat, tolerance: f32, label: &str) -> bool {
        self.check(assert::near_quat(actual, expected, tolerance, label))
    }
    /// Two rotations facing the same way.
    pub fn same_facing(&mut self, actual: Quat, expected: Quat, tolerance: f32, label: &str) -> bool {
        self.check(assert::same_facing(actual, expected, tolerance, label))
    }
    /// `Transform` equality.
    pub fn near_transform(
        &mut self,
        actual: &Transform,
        expected: &Transform,
        tolerance: f32,
        label: &str,
    ) -> bool {
        self.check(assert::near_transform(actual, expected, tolerance, label))
    }
    /// `Mat4` equality, element-wise.
    pub fn near_mat4(&mut self, actual: &Mat4, expected: &Mat4, tolerance: f32, label: &str) -> bool {
        self.check(assert::near_mat4(actual, expected, tolerance, label))
    }
    /// Exact equality.
    pub fn equal<T: PartialEq + std::fmt::Debug>(&mut self, actual: &T, expected: &T, label: &str) -> bool {
        self.check(assert::exact(actual, expected, label))
    }
    /// Relational check with the six-way selector.
    pub fn relate<T: PartialOrd + std::fmt::Display>(
        &mut self,
        actual: T,
        relation: Relation,
        expected: T,
        label: &str,
    ) -> bool {
        self.check(assert::relate(actual, relation, expected, label))
    }
    /// `value` must be true.
    pub fn is_true(&mut self, value: bool, label: &str) -> bool {
        self.check(if value {
            Ok(())
        } else {
            Err(format!("{label}: expected true"))
        })
    }
    /// `value` must be false.
    pub fn is_false(&mut self, value: bool, label: &str) -> bool {
        self.check(if value {
            Err(format!("{label}: expected false"))
        } else {
            Ok(())
        })
    }
    /// The entity must be alive.
    pub fn entity_alive(&mut self, entity: Entity) -> bool {
        let alive = self.world.get_entity(entity).is_ok();
        self.check(if alive {
            Ok(())
        } else {
            Err("entity expected alive".to_owned())
        })
    }
    /// The entity must carry component `C`.
    pub fn entity_has<C: Component>(&mut self, entity: Entity) -> bool {
        let has = self.world.get::<C>(entity).is_some();
        self.check(if has {
            Ok(())
        } else {
            Err(format!("entity expected to have {}", std::any::type_name::<C>()))
        })
    }

    /// Evaluates one `play.expect` condition now; returns whether it holds (and records a failure when not).
    pub fn condition(&mut self, condition: &Value) -> bool {
        match self.eval_condition(condition) {
            Ok((true, _)) => true,
            Ok((false, detail)) => {
                self.fail(&format!("condition failed: {detail}"));
                false
            }
            Err(e) => {
                self.fail(&format!("condition could not be evaluated: {e}"));
                false
            }
        }
    }

    /// Evaluates a condition without recording a failure: `Ok((holds, detail))`.
    pub fn eval_condition(&mut self, condition: &Value) -> Result<(bool, String), String> {
        let Some(eval) = self.shared.hooks.condition else {
            return Err("no condition evaluator installed (add the agent plugin)".to_owned());
        };
        eval(self.world, condition)
    }

    /// Injects an input action for a number of frames through the installed service.
    pub fn inject(&mut self, action: &str, value: [f32; 2], frames: u32) -> Result<(), String> {
        let Some(inject) = self.shared.hooks.inject else {
            return Err("no input injection service installed (add the agent plugin)".to_owned());
        };
        inject(self.world, action, value, frames)
    }

    /// Compares a reflected value with the level's expected-value file (spec 41 § 2.6).
    pub fn expect_value(&mut self, key: &str, value: &dyn PartialReflect) -> bool {
        use regia_ecs::reflect::serde::TypedReflectSerializer;
        let json = {
            let Some(registry) = self.world.get_resource::<AppTypeRegistry>() else {
                self.fail(&format!("{key}: no type registry"));
                return false;
            };
            let registry = registry.read();
            match serde_json::to_value(TypedReflectSerializer::new(value, &registry)) {
                Ok(v) => v,
                Err(e) => {
                    let message = format!("{key}: value cannot be serialized: {e}");
                    drop(registry);
                    self.fail(&message);
                    return false;
                }
            }
        };
        self.expect_json(key, json)
    }

    /// Like [`TestContext::expect_value`] for an already serialized value.
    pub fn expect_json(&mut self, key: &str, json: Value) -> bool {
        let name = self.run.name.clone();
        self.shared
            .observed
            .entry(name.clone())
            .or_default()
            .insert(key.to_owned(), json.clone());
        match self.shared.store.get(&name, key).cloned() {
            None if self.shared.opts.update_expected => {
                self.shared.store.set(&name, key, json);
                self.run.blessed = true;
                true
            }
            None => {
                self.fail(&format!("{key}: no expected value"));
                false
            }
            Some(expected) => {
                let diffs = diff_values(&expected, &json, DEFAULT_RELATIVE_TOLERANCE);
                if diffs.is_empty() {
                    true
                } else if self.shared.opts.update_expected {
                    self.shared.store.set(&name, key, json);
                    self.run.blessed = true;
                    true
                } else {
                    self.fail(&format!("{key}: differs from expected: {}", diffs.join("; ")));
                    false
                }
            }
        }
    }
}

/// A functional-test body in Rust (spec 41 § 2.3).
pub trait TestBody: Send + Sync + 'static {
    /// Once when the run opens; may spawn things and request streaming.
    fn setup(&mut self, _cx: &mut TestContext) {}
    /// Polled each frame until it returns true.
    fn ready(&mut self, _cx: &mut TestContext) -> bool {
        true
    }
    /// Once per pass.
    fn begin(&mut self, cx: &mut TestContext);
    /// Each frame while the pass is active.
    fn tick(&mut self, _cx: &mut TestContext) {}
    /// Once per pass at conclude; reset state for the next pass.
    fn conclude(&mut self, _cx: &mut TestContext, _verdict: Verdict) {}
    /// Once after the last pass, always.
    fn teardown(&mut self, _cx: &mut TestContext) {}
    /// Asked right after each conclude: start the next queued pass in the same frame.
    fn wants_pass_now(&mut self, _cx: &TestContext) -> bool {
        false
    }
    /// Extra report text.
    fn summary(&self, _verdict: Verdict) -> Option<String> {
        None
    }
}
