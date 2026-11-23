// SPDX-License-Identifier: MPL-2.0
//! Logic that lives in a prefab or a level (v0.6.0 parity row P-165, ADR-8105 (N2), ADR-8566): the
//! reflected [`LogicGraphRef`] component names a project `.logic.ron` and optionally overrides the
//! starting values of the graph's locals. [`crate::AiPlugin`] compiles it into the entity's
//! [`LogicGraph`] (`PreUpdate`, before the next fixed step), so a graph authored on a prefab runs
//! in play like one attached by code or `logic.compile`.
//!
//! The component is saved in scenes as its own format, `logic-graph-ref` v1
//! ([`LOGIC_GRAPH_REF_FORMAT_VERSION`]). The scene format itself is unchanged: a scene stores
//! components by type path, and `scene_schema_fingerprint` hashes the envelope, not the component
//! types, so `scene` stays at its version (ADR-8566).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::AssetServer;
use regia_scene::spawn::ReflectFormatVersion;
use serde::{Deserialize, Serialize};

use super::def::{LogicGraphDef, LogicGraphError};
use super::runtime::LogicGraph;
use super::{LogicGraphStatus, LogicInbox};
use crate::blackboard::Blackboard;
use crate::state_tree::Literal;
use crate::tree::BtTaskRegistry;

/// `format_version` of the saved [`LogicGraphRef`] component (`logic-graph-ref`).
pub const LOGIC_GRAPH_REF_FORMAT_VERSION: u32 = 1;

/// The most records [`LogicGraphRefLoads`] keeps; the oldest are dropped first.
pub const MAX_LOGIC_GRAPH_REF_LOG: usize = 32;

/// A logic graph authored on an entity: the project `.logic.ron` it runs, whether it runs, and
/// the starting values of some of its locals. [`crate::AiPlugin`] compiles it into a
/// [`LogicGraph`] whenever the component is added or changed; a disabled reference takes the
/// graph off again. Saved in scenes and prefabs (`logic-graph-ref` v1); prefab instances override
/// its fields like any other component's.
#[derive(Component, Reflect, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[reflect(Component, Default, FormatVersion)]
#[serde(default)]
pub struct LogicGraphRef {
    /// The `.logic.ron`, project-relative with `/` separators (`assets/prefabs/door.logic.ron`).
    pub graph: String,
    /// Whether the graph runs (default `true`).
    pub enabled: bool,
    /// Starting values that replace the defaults of the named graph locals.
    pub overrides: Vec<LogicLocalOverride>,
    /// Always [`LOGIC_GRAPH_REF_FORMAT_VERSION`].
    pub format_version: u32,
}

impl regia_scene::spawn::FormatVersion for LogicGraphRef {
    const CURRENT: u32 = LOGIC_GRAPH_REF_FORMAT_VERSION;
}

impl Default for LogicGraphRef {
    fn default() -> Self {
        Self {
            graph: String::new(),
            enabled: true,
            overrides: Vec::new(),
            format_version: LOGIC_GRAPH_REF_FORMAT_VERSION,
        }
    }
}

/// One graph local whose starting value a [`LogicGraphRef`] replaces.
#[derive(Reflect, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[reflect(Default)]
#[serde(default)]
pub struct LogicLocalOverride {
    /// The local's name, as the graph declares it (without `local/`).
    pub name: String,
    /// The starting value; its kind must match the local's default (an `Int` may start a `Float`
    /// local).
    pub value: LogicOverrideValue,
}

/// A starting value for a graph local: the kinds a `.logic.ron` local can declare.
#[derive(Reflect, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[reflect(Default)]
pub enum LogicOverrideValue {
    /// A boolean.
    Bool(bool),
    /// A signed integer.
    Int(i64),
    /// A 32-bit float (must be finite).
    Float(f32),
    /// A string.
    String(String),
}

impl Default for LogicOverrideValue {
    fn default() -> Self {
        Self::Bool(false)
    }
}

/// Why a [`LogicGraphRef`] could not become a running graph. [`LogicGraphRefError::reason`] is the
/// snake_case code the agent protocol and the load log report.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum LogicGraphRefError {
    /// The reference names no file.
    #[error("the logic graph reference names no file")]
    NoGraph,
    /// The path is absolute, climbs out of the project or is otherwise not a project path.
    #[error("{0}: not a project-relative path")]
    BadPath(String),
    /// The file could not be read.
    #[error("{path}: cannot be read: {reason}")]
    Unreadable {
        /// The project path.
        path: String,
        /// The IO error.
        reason: String,
    },
    /// The file did not parse or compile.
    #[error("{path}: {error}")]
    Graph {
        /// The project path.
        path: String,
        /// The parse or compile error.
        error: LogicGraphError,
    },
    /// An override names a local the graph does not declare.
    #[error("override `{0}`: the graph declares no such local")]
    UnknownLocal(String),
    /// An override's value is of another kind than the local's default, or a non-finite float.
    #[error("override `{0}`: the value's kind does not match the local's default")]
    OverrideKind(String),
}

impl LogicGraphRefError {
    /// The snake_case code of this error (a compile error reports the graph's own reason).
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NoGraph => "no_graph",
            Self::BadPath(_) => "bad_path",
            Self::Unreadable { .. } => "unreadable",
            Self::Graph { error, .. } => error.reason(),
            Self::UnknownLocal(_) => "unknown_local",
            Self::OverrideKind(_) => "override_kind",
        }
    }
}

impl LogicGraphRef {
    /// An enabled reference to `graph` (project-relative) with no overrides.
    pub fn new(graph: impl Into<String>) -> Self {
        Self {
            graph: graph.into(),
            ..Self::default()
        }
    }

    /// Upgrades an older saved shape (none yet: version 1 is the first).
    ///
    /// # Errors
    ///
    /// A message for a version newer than this build reads.
    pub fn migrate(&mut self) -> Result<(), String> {
        if self.format_version > LOGIC_GRAPH_REF_FORMAT_VERSION {
            return Err(format!(
                "logic-graph-ref format_version {} is newer than {LOGIC_GRAPH_REF_FORMAT_VERSION}",
                self.format_version
            ));
        }
        self.format_version = LOGIC_GRAPH_REF_FORMAT_VERSION;
        Ok(())
    }

    /// The referenced file under `root` (the project root).
    ///
    /// # Errors
    ///
    /// [`LogicGraphRefError::NoGraph`] or [`LogicGraphRefError::BadPath`].
    pub fn file(&self, root: &Path) -> Result<PathBuf, LogicGraphRefError> {
        let path = self.graph.trim().replace('\\', "/");
        if path.is_empty() {
            return Err(LogicGraphRefError::NoGraph);
        }
        let rel = regia_core::project_path::check_relative(&path)
            .map_err(|_| LogicGraphRefError::BadPath(path.clone()))?;
        Ok(root.join(rel))
    }

    /// Reads the referenced definition under `root` and applies the overrides to its locals'
    /// defaults.
    ///
    /// # Errors
    ///
    /// Any [`LogicGraphRefError`] but a compile error.
    pub fn definition(&self, root: &Path) -> Result<LogicGraphDef, LogicGraphRefError> {
        let file = self.file(root)?;
        let text = std::fs::read_to_string(&file).map_err(|e| LogicGraphRefError::Unreadable {
            path: self.graph.clone(),
            reason: e.to_string(),
        })?;
        let mut def = LogicGraphDef::from_ron(&text).map_err(|error| LogicGraphRefError::Graph {
            path: self.graph.clone(),
            error,
        })?;
        for o in &self.overrides {
            let local = def
                .locals
                .iter_mut()
                .find(|l| l.name == o.name)
                .ok_or_else(|| LogicGraphRefError::UnknownLocal(o.name.clone()))?;
            local.default = override_literal(&local.default, &o.value)
                .ok_or_else(|| LogicGraphRefError::OverrideKind(o.name.clone()))?;
        }
        Ok(def)
    }

    /// Reads, overrides and compiles the referenced graph.
    ///
    /// # Errors
    ///
    /// Any [`LogicGraphRefError`].
    pub fn compile(&self, root: &Path, registry: &BtTaskRegistry) -> Result<LogicGraph, LogicGraphRefError> {
        let def = self.definition(root)?;
        LogicGraph::compile(&def, registry).map_err(|error| LogicGraphRefError::Graph {
            path: self.graph.clone(),
            error,
        })
    }
}

/// The literal an override writes over `default`, or `None` when the kinds differ.
fn override_literal(default: &Literal, value: &LogicOverrideValue) -> Option<Literal> {
    match (default, value) {
        (Literal::Bool(_), LogicOverrideValue::Bool(v)) => Some(Literal::Bool(*v)),
        (Literal::Int(_), LogicOverrideValue::Int(v)) => Some(Literal::Int(*v)),
        (Literal::Float(_), LogicOverrideValue::Float(v)) if v.is_finite() => Some(Literal::Float(*v)),
        // An authored starting value: f32 is the local's kind.
        (Literal::Float(_), LogicOverrideValue::Int(v)) => Some(Literal::Float(*v as f32)),
        (Literal::String(_), LogicOverrideValue::String(v)) => Some(Literal::String(v.clone())),
        _ => None,
    }
}

/// One attach of a [`LogicGraphRef`]: the entity, the file and what happened.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicGraphRefRecord {
    /// The entity that carries the reference.
    pub entity: Entity,
    /// The referenced file, as authored.
    pub graph: String,
    /// `Ok` when the graph was attached, else the error's reason code and message.
    pub result: Result<(), (&'static str, String)>,
}

/// What [`crate::AiPlugin`] did with the [`LogicGraphRef`]s it saw, plus the project root it reads
/// them under.
#[derive(Resource, Debug, Clone, Default)]
pub struct LogicGraphRefLoads {
    /// The project root the references resolve under. `None`: the [`AssetServer`]'s project
    /// root, else the working directory.
    pub root: Option<PathBuf>,
    /// Graphs attached so far.
    pub attached: u64,
    /// References that failed so far (the entity keeps no graph from it).
    pub failed: u64,
    log: VecDeque<LogicGraphRefRecord>,
}

impl LogicGraphRefLoads {
    /// The most recent records, oldest first (at most [`MAX_LOGIC_GRAPH_REF_LOG`]).
    pub fn log(&self) -> impl Iterator<Item = &LogicGraphRefRecord> {
        self.log.iter()
    }

    fn push(&mut self, record: LogicGraphRefRecord) {
        if record.result.is_ok() {
            self.attached += 1;
        } else {
            self.failed += 1;
        }
        if self.log.len() >= MAX_LOGIC_GRAPH_REF_LOG {
            self.log.pop_front();
        }
        self.log.push_back(record);
    }
}

/// `PreUpdate`: compiles every added or changed [`LogicGraphRef`] into its entity's
/// [`LogicGraph`] (with an empty [`LogicInbox`] and [`Blackboard`] when the entity has none), so
/// the graph ticks from the next fixed step. A disabled reference removes the graph and its
/// status; a failed one removes them too and is logged in [`LogicGraphRefLoads`] with a warning.
#[tracing::instrument(skip_all, target = "regia_ai", name = "logic_graph_ref_attach")]
pub(crate) fn attach_logic_graph_refs(
    mut commands: Commands,
    refs: Query<(Entity, &LogicGraphRef), Changed<LogicGraphRef>>,
    registry: Option<Res<BtTaskRegistry>>,
    server: Option<Res<AssetServer>>,
    mut loads: ResMut<LogicGraphRefLoads>,
) {
    if refs.is_empty() {
        return;
    }
    let root = loads.root.clone().unwrap_or_else(|| {
        server
            .as_ref()
            .map_or_else(|| PathBuf::from("."), |s| s.project_root().to_path_buf())
    });
    let empty = BtTaskRegistry::default();
    let registry = registry.as_deref().unwrap_or(&empty);
    for (entity, graph_ref) in &refs {
        if !graph_ref.enabled {
            commands.entity(entity).remove::<(LogicGraph, LogicGraphStatus)>();
            continue;
        }
        let result = match graph_ref.compile(&root, registry) {
            Ok(graph) => {
                commands
                    .entity(entity)
                    .remove::<LogicGraphStatus>()
                    .insert(graph)
                    .insert_if_new((LogicInbox::default(), Blackboard::new()));
                Ok(())
            }
            Err(e) => {
                tracing::warn!(target: "regia_ai", "logic graph reference on {entity}: {e}");
                commands.entity(entity).remove::<(LogicGraph, LogicGraphStatus)>();
                Err((e.reason(), e.to_string()))
            }
        };
        loads.push(LogicGraphRefRecord {
            entity,
            graph: graph_ref.graph.clone(),
            result,
        });
    }
}

/// The shape fingerprint of the saved `logic-graph-ref` component (`cargo xtask check-formats`).
pub fn logic_graph_ref_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<LogicGraphRef>(),
        serde_field_names::<LogicLocalOverride>(),
        serde_variant_names::<LogicOverrideValue>(),
    ])
}

/// The `from` versions of `logic-graph-ref`'s migration steps (none: v1 is the first).
pub fn logic_graph_ref_migration_sources() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AiPlugin;
    use crate::blackboard::BbValue;
    use regia_core::id::Guid;
    use regia_scene::components::EntityGuid;
    use regia_scene::format::{PrefabInstanceData, SceneAssetRef, SceneFile};
    use regia_scene::prefab::{PrefabLibrary, PrefabTemplate, instantiate_prefab};
    use regia_scene::spawn::SpawnOptions;

    /// A door graph: on start it copies its `speed` local into the entity's blackboard; every
    /// fixed step it counts `ticks` there.
    const DOOR: &str = r#"(format_version: 1, locals: [(name: "speed", default: Int(1)), (name: "label", default: String("door"))], rules: [
        (name: "start", event: Start, actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("speed")), "value": BbRef("local/speed") }),
        ]),
        (name: "count", event: Timer(every_ticks: 1), actions: [
            (task: "logic::SetBlackboard", set: { "key": Literal(String("ticks")), "op": Literal(String("increment")) }),
        ]),
    ])"#;

    fn project() -> PathBuf {
        let root = regia_test_utils::unique_temp_dir("logic-graph-ref");
        let prefabs = root.join("assets/prefabs");
        std::fs::create_dir_all(&prefabs).expect("project dir");
        std::fs::write(prefabs.join("door.logic.ron"), DOOR).expect("graph file");
        root
    }

    fn app(root: &Path) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        enable_fixed_step(&mut app, 60.0);
        app.add_plugins(AiPlugin);
        app.add_plugins(regia_scene::ScenePlugin);
        app.init_resource::<PrefabLibrary>();
        app.world_mut().resource_mut::<LogicGraphRefLoads>().root = Some(root.to_path_buf());
        app
    }

    fn prefab_text(root: Guid, component: &str) -> String {
        format!(
            r#"(format_version: 1, entities: [
                (guid: "{root}", name: "Door", components: {{
                    "regia_ai::logic::graph_ref::LogicGraphRef": {component},
                }}),
            ])"#
        )
    }

    /// Bakes the prefab text, registers it and instantiates it under a fresh root: what a level
    /// load does for an instance.
    fn spawn_prefab(app: &mut App, text: &str) -> Entity {
        let file = SceneFile::from_ron_str(text).expect("prefab parses");
        let asset = Guid::new_random();
        let registry = app
            .world()
            .resource::<regia_ecs::reflect::AppTypeRegistry>()
            .0
            .clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        let local_root = template.entities[0].local_id;
        app.world_mut().resource_mut::<PrefabLibrary>().insert(template);
        let wrapper = app.world_mut().spawn(EntityGuid(Guid::new_random())).id();
        let instance = instantiate_prefab(
            app.world_mut(),
            &PrefabInstanceData::plain(SceneAssetRef(asset)),
            wrapper,
            &SpawnOptions::default(),
            0,
        )
        .expect("instantiate");
        instance.locals[&local_root]
    }

    fn board_int(app: &App, e: Entity, key: &str) -> Option<i64> {
        match app.world().get::<Blackboard>(e)?.get(key)? {
            BbValue::Int(v) => Some(*v),
            _ => None,
        }
    }

    #[test]
    fn a_prefab_with_a_logic_ref_runs_its_graph_in_play() {
        let root = project();
        let mut app = app(&root);
        let text = prefab_text(
            Guid::new_random(),
            r#"(graph: "assets/prefabs/door.logic.ron", enabled: true, overrides: [(name: "speed", value: Int(7))], format_version: 1)"#,
        );
        let door = spawn_prefab(&mut app, &text);
        assert!(
            app.world().get::<LogicGraph>(door).is_none(),
            "nothing runs before an update"
        );
        for _ in 0..4 {
            app.update();
        }
        let graph = app
            .world()
            .get::<LogicGraph>(door)
            .expect("the reference became a graph");
        assert!(graph.is_started());
        assert_eq!(
            graph.locals().get("speed"),
            Some(&BbValue::Int(7)),
            "the override set the start value"
        );
        assert_eq!(
            board_int(&app, door, "speed"),
            Some(7),
            "the Start rule ran with the override"
        );
        let ticks = board_int(&app, door, "ticks").expect("the timer rule ran");
        assert!(ticks >= 1, "the timer rule counted: {ticks}");
        assert!(app.world().get::<LogicGraphStatus>(door).is_some());
        let loads = app.world().resource::<LogicGraphRefLoads>();
        assert_eq!((loads.attached, loads.failed), (1, 0));
        // More fixed steps keep counting: the graph runs like a placed one.
        for _ in 0..3 {
            app.update();
        }
        let later = board_int(&app, door, "ticks").expect("ticks");
        assert!(later > ticks, "{later} after {ticks}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn disabling_the_reference_takes_the_graph_off_and_enabling_brings_it_back() {
        let root = project();
        let mut app = app(&root);
        let door = app
            .world_mut()
            .spawn(LogicGraphRef::new("assets/prefabs/door.logic.ron"))
            .id();
        app.update();
        assert!(app.world().get::<LogicGraph>(door).is_some());
        app.world_mut()
            .get_mut::<LogicGraphRef>(door)
            .expect("ref")
            .enabled = false;
        app.update();
        assert!(app.world().get::<LogicGraph>(door).is_none());
        assert!(app.world().get::<LogicGraphStatus>(door).is_none());
        app.world_mut()
            .get_mut::<LogicGraphRef>(door)
            .expect("ref")
            .enabled = true;
        app.update();
        assert!(app.world().get::<LogicGraph>(door).is_some());
        assert_eq!(app.world().resource::<LogicGraphRefLoads>().attached, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_bad_reference_attaches_nothing_and_is_logged_with_its_reason() {
        let root = project();
        let mut app = app(&root);
        let cases = [
            (LogicGraphRef::new(""), "no_graph"),
            (LogicGraphRef::new("../outside.logic.ron"), "bad_path"),
            (
                LogicGraphRef::new("assets/prefabs/missing.logic.ron"),
                "unreadable",
            ),
            (
                LogicGraphRef {
                    overrides: vec![LogicLocalOverride {
                        name: "nope".into(),
                        value: LogicOverrideValue::Int(1),
                    }],
                    ..LogicGraphRef::new("assets/prefabs/door.logic.ron")
                },
                "unknown_local",
            ),
            (
                LogicGraphRef {
                    overrides: vec![LogicLocalOverride {
                        name: "label".into(),
                        value: LogicOverrideValue::Bool(true),
                    }],
                    ..LogicGraphRef::new("assets/prefabs/door.logic.ron")
                },
                "override_kind",
            ),
        ];
        let entities: Vec<Entity> = cases
            .iter()
            .map(|(r, _)| app.world_mut().spawn(r.clone()).id())
            .collect();
        app.update();
        for e in &entities {
            assert!(app.world().get::<LogicGraph>(*e).is_none());
        }
        let loads = app.world().resource::<LogicGraphRefLoads>();
        assert_eq!((loads.attached, loads.failed), (0, cases.len() as u64));
        let mut reasons: Vec<(Entity, &str)> = loads
            .log()
            .map(|r| (r.entity, r.result.as_ref().expect_err("failed").0))
            .collect();
        reasons.sort();
        let mut expected: Vec<(Entity, &str)> = entities
            .iter()
            .zip(&cases)
            .map(|(e, (_, reason))| (*e, *reason))
            .collect();
        expected.sort();
        assert_eq!(reasons, expected);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_int_override_starts_a_float_local_and_a_non_finite_float_is_refused() {
        let float = Literal::Float(0.5);
        assert_eq!(
            override_literal(&float, &LogicOverrideValue::Int(3)),
            Some(Literal::Float(3.0))
        );
        assert_eq!(
            override_literal(&float, &LogicOverrideValue::Float(f32::NAN)),
            None
        );
        assert_eq!(
            override_literal(&Literal::Int(0), &LogicOverrideValue::Float(1.0)),
            None
        );
    }

    #[test]
    fn a_newer_format_version_is_refused_and_the_fingerprint_names_every_field() {
        let mut newer = LogicGraphRef {
            format_version: LOGIC_GRAPH_REF_FORMAT_VERSION + 1,
            ..LogicGraphRef::default()
        };
        assert!(newer.migrate().is_err());
        let mut old = LogicGraphRef {
            format_version: 0,
            ..LogicGraphRef::default()
        };
        assert!(old.migrate().is_ok());
        assert_eq!(old.format_version, LOGIC_GRAPH_REF_FORMAT_VERSION);
        let fingerprint = logic_graph_ref_schema_fingerprint();
        for field in [
            "graph",
            "enabled",
            "overrides",
            "format_version",
            "name",
            "value",
            "Float",
        ] {
            assert!(fingerprint.contains(field), "{field} in {fingerprint}");
        }
        assert!(logic_graph_ref_migration_sources().is_empty());
    }
}
