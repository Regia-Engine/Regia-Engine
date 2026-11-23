// SPDX-License-Identifier: MPL-2.0
//! Utility reasoners on entities (ADR-2565, ADR-2336): [`UtilityReasoner`] runs a
//! [`crate::utility::Reasoner`] over the agent's [`Blackboard`] at a configurable rate, with the
//! definition coming from a `.utility.ron` asset ([`UtilityDefs`]) that hot reloads, or from code
//! ([`UtilityReasoner::from_def`]).
//!
//! The selected action's name goes to [`UtilityReasoner::current_action`] and to the blackboard key
//! [`UtilityReasoner::action_key`], so a behavior tree or a state tree can switch on it. The last
//! decision's full report is kept for `ai.utility_explain`.
//!
//! # Determinism
//!
//! The clock is [`UtilityClock`], which counts fixed steps. An agent decides on the steps its
//! spawn-order arrival picks out of its interval (never its entity id, ADR-1347), so a crowd does
//! not all score on one step; a new reasoner binds its definition on the first step after it
//! appears, and the generator is the reasoner's own seeded stream. A hot reload is a dev-time
//! event and lands on the next frame.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::SourceAssetChanged;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::table_source::{TableBytes, read_table};
use regia_scene::layers::LayerParked;

use crate::AiStep;
use crate::blackboard::{BbValue, Blackboard};
use crate::goap_ecs::GoapStep;
use crate::perception_system::PerceptionStep;
use crate::utility::{DecisionReport, Reasoner, UTILITY_AI_TYPE_PATH, UtilityAiDef, UtilityAiError};

/// The default [`UtilityReasoner::action_key`].
pub const DEFAULT_UTILITY_ACTION_KEY: &str = "utility_action";

/// The default [`UtilityReasoner::interval_ticks`]: 10 Hz on a 60 Hz step.
pub const DEFAULT_UTILITY_INTERVAL_TICKS: u32 = 6;

/// An entity that decides by utility. Needs a [`Blackboard`] on the same entity: that is what the
/// considerations read.
#[derive(Component, Reflect, Debug, Clone)]
#[reflect(Component, Default)]
pub struct UtilityReasoner {
    /// The `.utility.ron` path under `assets/`, for example `ai/guard.utility.ron`. Empty for a
    /// reasoner built in code.
    pub path: String,
    /// Fixed steps between decisions (0 reads as 1).
    pub interval_ticks: u32,
    /// The blackboard key that holds the selected action's name (`BbValue::String`). Empty writes
    /// nothing.
    pub action_key: String,
    /// The selected action's name (read-only; empty before the first selection).
    pub current_action: String,
    /// Decisions taken so far (read-only).
    pub decisions: u64,
    /// The [`UtilityDefs`] generation in use (read-only; 0 for a reasoner built in code).
    pub generation: u64,
    /// The path the running reasoner came from.
    #[reflect(ignore)]
    applied_path: String,
    /// The scoring state.
    #[reflect(ignore)]
    reasoner: Option<Reasoner>,
    /// The last decision's report.
    #[reflect(ignore)]
    last_report: Option<DecisionReport>,
}

impl Default for UtilityReasoner {
    fn default() -> Self {
        Self {
            path: String::new(),
            interval_ticks: DEFAULT_UTILITY_INTERVAL_TICKS,
            action_key: DEFAULT_UTILITY_ACTION_KEY.to_string(),
            current_action: String::new(),
            decisions: 0,
            generation: 0,
            applied_path: String::new(),
            reasoner: None,
            last_report: None,
        }
    }
}

impl UtilityReasoner {
    /// A reasoner that takes its definition from the asset at `path` (under `assets/`).
    #[must_use]
    pub fn from_path(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }

    /// A reasoner over a definition made in code.
    ///
    /// # Errors
    ///
    /// The errors of [`Reasoner::new`].
    pub fn from_def(def: &UtilityAiDef) -> Result<Self, UtilityAiError> {
        Ok(Self {
            reasoner: Some(Reasoner::new(def)?),
            ..Self::default()
        })
    }

    /// The running reasoner, once a definition is in use.
    #[must_use]
    pub fn reasoner(&self) -> Option<&Reasoner> {
        self.reasoner.as_ref()
    }

    /// The report of the last decision taken.
    #[must_use]
    pub fn last_report(&self) -> Option<&DecisionReport> {
        self.last_report.as_ref()
    }

    /// What the reasoner would pick from `blackboard` at `now`, changing nothing. Compare it with
    /// [`UtilityReasoner::last_report`] to see what moved since the last decision.
    #[must_use]
    pub fn explain(&self, blackboard: &Blackboard, now: f32) -> Option<DecisionReport> {
        self.reasoner.as_ref().map(|r| r.explain(blackboard, now))
    }

    /// Takes one decision and publishes it. A steady definition overwrites the last report in
    /// place; the first decision, and a definition whose shape changed, builds a new one.
    fn decide(&mut self, blackboard: &mut Mut<Blackboard>, now: f32) {
        let UtilityReasoner {
            reasoner,
            last_report,
            decisions,
            current_action,
            action_key,
            ..
        } = self;
        let Some(reasoner) = reasoner.as_mut() else {
            return;
        };
        *decisions += 1;
        if let Some(report) = last_report.as_mut() {
            reasoner.decide_into(blackboard, now, report);
        } else {
            *last_report = Some(reasoner.decide(blackboard, now));
        }
        // The reasoner's own current action, not the report's pick: a reload can remove the current
        // action, and a decision with nothing eligible then selects nothing and keeps no name.
        match reasoner.current_action() {
            Some(current) => {
                if *current_action != current {
                    current_action.clear();
                    current_action.push_str(current);
                }
                if !action_key.is_empty() && blackboard.get_string(action_key) != current {
                    blackboard
                        .values
                        .insert(action_key.clone(), BbValue::String(current.to_string()));
                }
            }
            None => {
                current_action.clear();
                if !action_key.is_empty() && blackboard.contains(action_key.as_str()) {
                    blackboard.remove(action_key.as_str());
                }
            }
        }
    }
}

/// Why a definition is not in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtilityDefError {
    /// `bad_path`, `definition_not_found`, or a [`UtilityAiError::reason`].
    pub reason: &'static str,
    /// Human-readable detail.
    pub detail: String,
}

/// One `.utility.ron` file.
#[derive(Debug, Clone, Default)]
pub struct UtilityDefSlot {
    /// The definition in use (kept when a reload fails).
    pub definition: Option<Arc<UtilityAiDef>>,
    /// Bumped on every applied load.
    pub generation: u64,
    /// The last failure, cleared by a load that works.
    pub error: Option<UtilityDefError>,
}

/// The loaded `.utility.ron` files, by path under `assets/`.
#[derive(Resource, Debug, Default)]
pub struct UtilityDefs {
    slots: BTreeMap<String, UtilityDefSlot>,
}

impl UtilityDefs {
    /// The slot of `path`.
    #[must_use]
    pub fn slot(&self, path: &str) -> Option<&UtilityDefSlot> {
        self.slots.get(path)
    }

    /// Every slot, sorted by path.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &UtilityDefSlot)> {
        self.slots.iter().map(|(path, slot)| (path.as_str(), slot))
    }

    /// Puts a parsed `definition` at `path` and returns its generation.
    ///
    /// # Errors
    ///
    /// The errors of [`Reasoner::new`]; the slot is unchanged.
    pub fn insert(
        &mut self,
        path: impl Into<String>,
        definition: UtilityAiDef,
    ) -> Result<u64, UtilityAiError> {
        Reasoner::new(&definition)?;
        let slot = self.slots.entry(path.into()).or_default();
        slot.definition = Some(Arc::new(definition));
        slot.generation += 1;
        slot.error = None;
        Ok(slot.generation)
    }

    fn fail(&mut self, path: &str, reason: &'static str, detail: String) {
        tracing::warn!(target: "regia_ai", path, reason, "utility definition not loaded: {detail}");
        self.slots.entry(path.to_string()).or_default().error = Some(UtilityDefError { reason, detail });
    }
}

/// Whether `path` stays under `assets/`: relative, no `..`, no root or prefix.
fn confined(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Reads the `.utility.ron` at `path` (under `assets/`) into [`UtilityDefs`], through the project's
/// VFS and asset server, and returns whether a definition is in use. A failed read keeps what was
/// there and records why in [`UtilityDefSlot::error`].
pub fn load_utility_definition(world: &mut World, path: &str) -> bool {
    let _span = tracing::debug_span!(target: "regia_ai", "utility_definition_load").entered();
    if !world.contains_resource::<UtilityDefs>() {
        world.init_resource::<UtilityDefs>();
    }
    if !confined(path) {
        let mut defs = world.resource_mut::<UtilityDefs>();
        defs.fail(path, "bad_path", "the path must stay under assets/".to_string());
        return false;
    }
    let read = read_table(world, path, UTILITY_AI_TYPE_PATH);
    let mut defs = world.resource_mut::<UtilityDefs>();
    let text = match read {
        Ok(TableBytes::Text(text)) => Ok(text),
        Ok(TableBytes::Cooked(bytes)) => String::from_utf8(bytes)
            .map_err(|_| ("definition_not_found", "the entry is not UTF-8".to_string())),
        Err(detail) => Err(("definition_not_found", detail)),
    };
    let parsed = text.and_then(|text| UtilityAiDef::from_ron(&text).map_err(|e| (e.reason(), e.to_string())));
    match parsed.and_then(|def| defs.insert(path, def).map_err(|e| (e.reason(), e.to_string()))) {
        Ok(_) => true,
        Err((reason, detail)) => {
            defs.fail(path, reason, detail);
            defs.slot(path).is_some_and(|slot| slot.definition.is_some())
        }
    }
}

/// The clock of the scoring system: fixed steps, never the wall clock.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct UtilityClock {
    /// Fixed steps run so far.
    pub tick: u64,
    /// Simulated seconds of those steps (the `now` of a decision).
    pub seconds: f64,
    /// Decisions taken in the last fixed step.
    pub decisions_last_tick: u32,
}

impl UtilityClock {
    /// The `now` of the last step, as a decision sees it.
    #[must_use]
    pub fn now(&self) -> f32 {
        self.seconds as f32
    }
}

/// `FixedUpdate` set of [`UtilityPlugin`]: after `PerceptionStep` and `GoapStep`, before `AiStep`, so
/// the scores see this step's perception and a tree reads this step's decision.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UtilityStep;

/// Installs utility reasoners: loads `.utility.ron` files, hot reloads them when the source changes,
/// and scores every [`UtilityReasoner`] at its rate. Opt-in (not part of [`crate::AiPlugin`]).
#[derive(Default)]
pub struct UtilityPlugin;

impl Plugin for UtilityPlugin {
    fn build(&self, app: &mut App) {
        // The decision phase comes from the reasoner's arrival order, not its entity id: ids are
        // record-time allocations (ADR-1347), so id-phased decisions land on different steps in
        // two runs of the same level.
        regia_ecs::spawn_order::track_spawn_order::<UtilityReasoner>(app.world_mut());
        app.register_type::<UtilityReasoner>()
            .register_type::<UtilityClock>()
            .init_resource::<UtilityDefs>()
            .init_resource::<UtilityClock>()
            .configure_sets(
                FixedUpdate,
                UtilityStep.after(PerceptionStep).after(GoapStep).before(AiStep),
            )
            .add_systems(PreUpdate, utility_sync)
            .add_systems(
                FixedUpdate,
                (utility_bind, utility_tick).chain().in_set(UtilityStep),
            );
    }
}

/// Rebuilds `component`'s reasoner from `def`. A hot reload keeps the current action, the cooldowns
/// and the generator (matched by name) so tuning a number does not restart the agent.
fn apply_definition(
    component: &mut UtilityReasoner,
    path: &str,
    generation: u64,
    def: &UtilityAiDef,
) -> Result<(), UtilityAiError> {
    let mut next = Reasoner::new(def)?;
    if component.applied_path == path
        && let Some(previous) = &component.reasoner
    {
        next.adopt_state_from(previous);
    }
    component.current_action.clear();
    if let Some(current) = next.current_action() {
        component.current_action.push_str(current);
    }
    component.reasoner = Some(next);
    component.applied_path.clear();
    component.applied_path.push_str(path);
    component.generation = generation;
    Ok(())
}

/// `PreUpdate` (exclusive), once per frame before the fixed steps: reads a changed source file again and
/// gives agents the definition they name. Allocates only when something changed. A file that does not
/// load keeps the definition in use and reports the error.
#[tracing::instrument(skip_all, target = "regia_ai", name = "utility_sync")]
fn utility_sync(
    world: &mut World,
    query: &mut QueryState<(Entity, &UtilityReasoner)>,
    mut changes: Local<MessageCursor<SourceAssetChanged>>,
    mut todo: Local<Vec<(Entity, String)>>,
) {
    let mut reload: Vec<String> = Vec::new();
    if let Some(messages) = world.get_resource::<Messages<SourceAssetChanged>>() {
        // Built on the first definition change only, so a quiet frame allocates nothing.
        let mut assets: Option<Option<std::path::PathBuf>> = None;
        for change in changes.read(messages) {
            let is_definition = change
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.to_ascii_lowercase().ends_with(".utility.ron"));
            if !is_definition {
                continue;
            }
            let assets = assets.get_or_insert_with(|| {
                world
                    .get_resource::<regia_asset::AssetServer>()
                    .map(|server| server.project_root().join("assets"))
            });
            let defs = world.resource::<UtilityDefs>();
            if let Some(path) = changed_slot(defs, assets.as_deref(), &change.path)
                && !reload.iter().any(|p| p == path)
            {
                reload.push(path.to_string());
            }
        }
    }
    for path in reload {
        load_utility_definition(world, &path);
    }
    // What needs a definition: a new or renamed path, and every reasoner after a reload or a
    // generation change. A reasoner built in code has an empty path and is never touched.
    todo.clear();
    let defs = world.resource::<UtilityDefs>();
    for (entity, reasoner) in query.iter(world) {
        if reasoner.path.is_empty() {
            continue;
        }
        let slot = defs.slot(&reasoner.path);
        // A path that failed to load has nothing to apply until a change reads it again.
        let usable = slot.is_none_or(|slot| slot.definition.is_some());
        let stale = reasoner.reasoner.is_none() || reasoner.applied_path != reasoner.path;
        let newer = slot.is_some_and(|slot| slot.generation != reasoner.generation);
        if usable && (stale || newer) {
            todo.push((entity, reasoner.path.clone()));
        }
    }
    bind_definitions(world, &mut todo);
}

/// The slot whose file `changed` is, or `None` (ADR-7559). A change under the project's `assets/`
/// folder matches the slot with exactly that relative path, whether or not the slot holds a
/// definition. A change elsewhere (no asset server, or an `assets://` mount outside the project)
/// matches the slot with the longest path `changed` ends with, so of `guard.utility.ron` and
/// `bosses/guard.utility.ron` only the second matches `.../bosses/guard.utility.ron`.
fn changed_slot<'a>(defs: &'a UtilityDefs, assets: Option<&Path>, changed: &Path) -> Option<&'a str> {
    if let Some(relative) = assets.and_then(|assets| changed.strip_prefix(assets).ok()) {
        return defs
            .iter()
            .map(|(path, _)| path)
            .find(|path| Path::new(path) == relative);
    }
    defs.iter()
        .map(|(path, _)| path)
        .filter(|path| confined(path) && changed.ends_with(Path::new(path)))
        .max_by_key(|path| Path::new(path).components().count())
}

/// Gives each listed reasoner the definition at its path, reading the file if nobody has yet.
fn bind_definitions(world: &mut World, todo: &mut Vec<(Entity, String)>) {
    for (entity, path) in todo.drain(..) {
        let known = world.resource::<UtilityDefs>().slot(&path).is_some();
        if !known {
            load_utility_definition(world, &path);
        }
        let Some((definition, generation)) = world
            .resource::<UtilityDefs>()
            .slot(&path)
            .and_then(|slot| slot.definition.clone().map(|def| (def, slot.generation)))
        else {
            continue;
        };
        if let Some(mut reasoner) = world.get_mut::<UtilityReasoner>(entity)
            && (reasoner.generation != generation
                || reasoner.applied_path != path
                || reasoner.reasoner.is_none())
            && let Err(error) = apply_definition(&mut reasoner, &path, generation, &definition)
        {
            // Unreachable for a slot's definition, which was validated on the way in.
            tracing::warn!(target: "regia_ai", %path, "utility definition refused: {error}");
        }
    }
}

/// `FixedUpdate` (exclusive), before the decisions: binds a reasoner spawned since the last step. Waiting
/// for the next frame's [`utility_sync`] would let the number of fixed steps in a frame decide when
/// its first decision comes.
fn utility_bind(
    world: &mut World,
    query: &mut QueryState<(Entity, &UtilityReasoner), Added<UtilityReasoner>>,
    mut todo: Local<Vec<(Entity, String)>>,
) {
    todo.clear();
    for (entity, reasoner) in query.iter(world) {
        if !reasoner.path.is_empty() && reasoner.reasoner.is_none() {
            todo.push((entity, reasoner.path.clone()));
        }
    }
    bind_definitions(world, &mut todo);
}

/// `FixedUpdate`: the decisions due this step (see the module docs).
#[tracing::instrument(skip_all, target = "regia_ai", name = "utility_tick")]
fn utility_tick(
    fixed: Option<Res<FixedTime>>,
    mut clock: ResMut<UtilityClock>,
    mut agents: Query<(Entity, &mut UtilityReasoner, &mut Blackboard), Without<LayerParked>>,
    spawn_order: Option<Res<regia_ecs::spawn_order::SpawnOrder<UtilityReasoner>>>,
) {
    clock.tick += 1;
    clock.seconds += fixed.map_or(1.0 / 60.0, |fixed| fixed.step().as_secs_f64());
    let (tick, now) = (clock.tick, clock.now());
    // The phase that staggers a crowd: the reasoner's spawn-order arrival. An untracked reasoner
    // (its component predates the tracking hooks) keeps the entity index, the historical key.
    let phase = |entity: Entity| {
        spawn_order
            .as_deref()
            .map_or(u64::from(entity.index_u32()), |order| {
                order.get(entity).unwrap_or_else(|| u64::from(entity.index_u32()))
            })
    };
    let mut decisions = 0;
    for (entity, mut reasoner, mut blackboard) in &mut agents {
        let interval = u64::from(reasoner.interval_ticks.max(1));
        if (tick + phase(entity)).is_multiple_of(interval) && reasoner.reasoner.is_some() {
            reasoner.decide(&mut blackboard, now);
            decisions += 1;
        }
    }
    clock.decisions_last_tick = decisions;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{UtilityDefs, changed_slot};

    fn slots(paths: &[&str]) -> UtilityDefs {
        let mut defs = UtilityDefs::default();
        for path in paths {
            defs.fail(path, "definition_not_found", String::new());
        }
        defs
    }

    #[test]
    fn under_the_project_assets_only_the_exact_path_matches() {
        let defs = slots(&["guard.utility.ron", "bosses/guard.utility.ron"]);
        let assets = Path::new("/project/assets");
        let pick = |changed: &str| changed_slot(&defs, Some(assets), Path::new(changed));
        assert_eq!(
            pick("/project/assets/guard.utility.ron"),
            Some("guard.utility.ron")
        );
        assert_eq!(
            pick("/project/assets/bosses/guard.utility.ron"),
            Some("bosses/guard.utility.ron")
        );
        assert_eq!(pick("/project/assets/minions/guard.utility.ron"), None);
        let only_top = slots(&["guard.utility.ron"]);
        assert_eq!(
            changed_slot(
                &only_top,
                Some(assets),
                Path::new("/project/assets/bosses/guard.utility.ron")
            ),
            None,
            "a same-named file in another folder is another file"
        );
    }

    #[test]
    fn outside_the_project_assets_the_longest_suffix_matches() {
        let defs = slots(&[
            "guard.utility.ron",
            "bosses/guard.utility.ron",
            "../escape.utility.ron",
        ]);
        let pick = |changed: &str| changed_slot(&defs, None, Path::new(changed));
        assert_eq!(
            pick("/mount/content/bosses/guard.utility.ron"),
            Some("bosses/guard.utility.ron")
        );
        assert_eq!(
            pick("/mount/content/guard.utility.ron"),
            Some("guard.utility.ron")
        );
        assert_eq!(pick("/mount/content/other.utility.ron"), None);
        assert_eq!(
            pick("/mount/escape.utility.ron"),
            None,
            "an unconfined path never matches"
        );
        // A mount elsewhere with no asset server: the project assets folder is not this file's.
        let assets = Path::new("/project/assets");
        assert_eq!(
            changed_slot(&defs, Some(assets), Path::new("/mount/content/guard.utility.ron")),
            Some("guard.utility.ron")
        );
    }
}
