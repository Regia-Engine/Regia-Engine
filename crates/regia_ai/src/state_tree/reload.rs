// SPDX-License-Identifier: MPL-2.0
//! File-driven hot reload of `.statetree.ron` (ADR-2408): an agent entity carrying a
//! [`StateTreeSource`] has its tree recompiled whenever the asset watcher reports that file
//! changed, and the new tree is swapped in at the start of the next fixed step, never in the
//! middle of one.
//!
//! The watcher message is read in `Update`; the compiled tree waits in [`StateTreeReloads`] until
//! the state-tree tick takes it. A file that does not parse or compile is recorded in the log and
//! the running tree stays untouched. Nothing here adds a `FixedUpdate` system: with an empty queue
//! the tick does one resource lookup per pass.
//!
//! A file the tree embeds (an `includes` entry, a `bt:` behavior tree, a state tree such a
//! behavior tree runs) reloads the agents of every file that embeds it: each compile remembers
//! the files it read.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::{AssetServer, SourceAssetChanged};

use super::{ReloadOutcome, StateTree, StateTreeDef};
use crate::tree::BtTaskRegistry;

/// How many [`StateTreeReloadRecord`]s the log keeps.
pub const MAX_RELOAD_LOG: usize = 32;

/// Where an agent's state tree was authored: the `.statetree.ron` to watch (project-relative, or
/// absolute).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct StateTreeSource {
    /// The definition file.
    pub path: PathBuf,
}

/// What happened to one reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateTreeReloadRecord {
    /// The agent.
    pub entity: Entity,
    /// The file that changed.
    pub path: PathBuf,
    /// The swap outcome, or why the file was rejected (the tree kept running unchanged).
    pub result: Result<ReloadOutcome, String>,
}

/// Compiled trees waiting for the next fixed step, plus the log of what happened.
#[derive(Resource, Default)]
pub struct StateTreeReloads {
    pending: Vec<(Entity, PathBuf, StateTree)>,
    log: VecDeque<StateTreeReloadRecord>,
    /// Swaps applied so far.
    pub applied: u64,
    /// Files rejected so far (parse or compile errors).
    pub rejected: u64,
    /// Changed files read, parsed and compiled so far (with their includes): one per file per
    /// change, however many agents share it (ADR-3342).
    pub file_compiles: u64,
    /// Agent trees built so far from an already compiled file's merged definition (no file read,
    /// no include load; each agent gets its own task objects).
    pub instances: u64,
    /// Changes skipped because the file's text equals the text last compiled for it (an editor
    /// save and the asset watcher both reporting one write).
    pub unchanged: u64,
    /// Source files whose embedded files were learned by a compile run only for that (a changed
    /// file that no agent names directly may be one of them).
    pub embed_scans: u64,
    /// The text last compiled per file (with the texts of the files it embedded), so a repeated
    /// report of one write compiles once.
    last_text: std::collections::BTreeMap<PathBuf, u64>,
    /// Per source file, the files its last compile read: includes, `bt:` behavior trees and the
    /// state trees those run. A change to one of them reloads the agents of the source file.
    embeds: std::collections::BTreeMap<PathBuf, BTreeSet<PathBuf>>,
}

impl StateTreeReloads {
    /// Queues `tree` for `entity`; it is swapped in at the start of the next fixed step. A newer
    /// tree for the same entity replaces an older one that has not been applied yet.
    pub fn queue(&mut self, entity: Entity, path: PathBuf, tree: StateTree) {
        self.pending.retain(|(e, _, _)| *e != entity);
        self.pending.push((entity, path, tree));
    }

    /// How many trees wait for the next step.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The most recent records, oldest first.
    pub fn log(&self) -> impl Iterator<Item = &StateTreeReloadRecord> {
        self.log.iter()
    }

    pub(crate) fn push_record(&mut self, record: StateTreeReloadRecord) {
        match record.result {
            Ok(_) => self.applied += 1,
            Err(_) => self.rejected += 1,
        }
        if self.log.len() == MAX_RELOAD_LOG {
            self.log.pop_front();
        }
        self.log.push_back(record);
    }

    pub(crate) fn take_pending(&mut self) -> Vec<(Entity, PathBuf, StateTree)> {
        std::mem::take(&mut self.pending)
    }

    pub(crate) fn restore_pending(&mut self, mut left: Vec<(Entity, PathBuf, StateTree)>) {
        // Anything queued meanwhile is newer than what could not be applied.
        left.retain(|(e, _, _)| !self.pending.iter().any(|(p, _, _)| p == e));
        left.append(&mut self.pending);
        self.pending = left;
    }
}

/// Reads the `.statetree.ron` an include names, project-relative, through the shared path rule
/// (`regia_core::project_path::resolve_read`, ADR-4922): no absolute, drive, UNC or `..` path on
/// any host (`..\\x`, `C:\\x` and `NUL` are refused on Linux too), and no symlink that leads out of
/// `project_root`. `None` when it is refused or cannot be read (the compile then reports
/// `include_missing`).
#[must_use]
pub fn read_state_tree_include(project_root: &Path, include: &str) -> Option<String> {
    let path = regia_core::project_path::resolve_read(project_root, include).ok()?;
    std::fs::read_to_string(path).ok()
}

/// Reads and compiles one definition file.
#[cfg(test)]
fn compile_file(root: &Path, file: &Path, registry: &BtTaskRegistry) -> Result<StateTree, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    compile_text(&text, registry, &RecordingLoader::new(root))
}

/// Parses and compiles one definition's text, embedded files read through `loader`.
fn compile_text(
    text: &str,
    registry: &BtTaskRegistry,
    loader: &RecordingLoader<'_>,
) -> Result<StateTree, String> {
    let def = StateTreeDef::from_ron(text).map_err(|e| e.to_string())?;
    StateTree::compile_with(&def, registry, &|rel| loader.load(rel)).map_err(|e| e.to_string())
}

/// A stable 64-bit FNV-1a hash of a file's text (equality check only, never persisted).
fn text_hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The loader of one reload: reads project-relative files (includes, `bt:` trees and the state
/// trees those embed) through [`read_state_tree_include`], once each, and remembers what it read.
/// The agents sharing a file reuse its reads, and the watcher learns which files the tree embeds.
struct RecordingLoader<'a> {
    root: &'a Path,
    /// Full path (the project root joined with the clean relative path) to the text read.
    read: std::cell::RefCell<std::collections::BTreeMap<PathBuf, Option<String>>>,
}

impl<'a> RecordingLoader<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            read: std::cell::RefCell::new(std::collections::BTreeMap::new()),
        }
    }

    fn load(&self, rel: &str) -> Option<String> {
        let Ok(full) = regia_core::project_path::resolve_read(self.root, rel) else {
            // Refused or missing: nothing to watch either (a missing file is `include_missing`).
            return None;
        };
        if let Some(text) = self.read.borrow().get(&full) {
            return text.clone();
        }
        let text = std::fs::read_to_string(&full).ok();
        self.read.borrow_mut().insert(full, text.clone());
        text
    }

    /// Every file read so far.
    fn files(&self) -> BTreeSet<PathBuf> {
        self.read.borrow().keys().cloned().collect()
    }

    /// The hash of `source` and every embedded file's text, in path order.
    fn key(&self, source: &str) -> u64 {
        let read = self.read.borrow();
        embed_key(
            source,
            read.iter().map(|(path, text)| (path.as_path(), text.as_deref())),
        )
    }
}

/// The hash of a definition's text and the texts of the files it embeds (`None`: unreadable).
fn embed_key<'p>(source: &str, embedded: impl Iterator<Item = (&'p Path, Option<&'p str>)>) -> u64 {
    let mut key = text_hash(source);
    for (path, text) in embedded {
        key = mix(key, text_hash(&path.to_string_lossy()));
        key = mix(key, text.map_or(0, text_hash));
    }
    key
}

fn mix(a: u64, b: u64) -> u64 {
    (a ^ b.rotate_left(17)).wrapping_mul(0x0100_0000_01b3)
}

/// `Update`: for each changed file that an agent's [`StateTreeSource`] names or that such a file
/// embeds (an include, a `bt:` behavior tree, a state tree that behavior tree runs), reads and
/// compiles the agent's file once and queues a tree for every agent that uses it (or logs why it
/// was rejected). The first agent gets the compiled tree; the others get trees built from its
/// merged definition with the same reads, so 100 agents sharing one file cost one file read and
/// one read of each embedded file per change ([`StateTreeReloads::file_compiles`]). A change after
/// which the file and everything it embeds read the same as at the last compile is skipped
/// ([`StateTreeReloads::unchanged`]).
#[tracing::instrument(skip_all, target = "regia_ai", name = "state_tree_watch")]
pub(crate) fn watch_state_tree_sources(
    mut changes: MessageReader<SourceAssetChanged>,
    sources: Query<(Entity, &StateTreeSource)>,
    registry: Option<Res<BtTaskRegistry>>,
    server: Option<Res<AssetServer>>,
    mut reloads: ResMut<StateTreeReloads>,
) {
    let mut changed: Vec<PathBuf> = changes.read().map(|c| c.path.clone()).collect();
    if changed.is_empty() {
        return;
    }
    changed.sort();
    changed.dedup();
    let root = server
        .as_ref()
        .map_or_else(|| PathBuf::from("."), |s| s.project_root().to_path_buf());
    let empty = BtTaskRegistry::default();
    let registry = registry.as_deref().unwrap_or(&empty);
    let all: Vec<(Entity, PathBuf)> = sources
        .iter()
        .map(|(e, s)| {
            let full = if s.path.is_absolute() {
                s.path.clone()
            } else {
                root.join(&s.path)
            };
            (e, full)
        })
        .collect();
    // A changed file no agent names directly may be embedded: learn what each source embeds
    // (one compile per source file, kept until its next reload) before matching.
    if changed
        .iter()
        .any(|path| !all.iter().any(|(_, full)| full == path))
    {
        let unknown: BTreeSet<PathBuf> = all
            .iter()
            .map(|(_, full)| full.clone())
            .filter(|full| !reloads.embeds.contains_key(full))
            .collect();
        for file in unknown {
            let loader = RecordingLoader::new(&root);
            if let Ok(text) = std::fs::read_to_string(&file) {
                let _ = compile_text(&text, registry, &loader);
            }
            reloads.embed_scans += 1;
            reloads.embeds.insert(file, loader.files());
        }
    }
    let mut agents: Vec<(Entity, PathBuf)> = all
        .into_iter()
        .filter(|(_, full)| {
            changed.contains(full)
                || reloads
                    .embeds
                    .get(full)
                    .is_some_and(|embedded| changed.iter().any(|path| embedded.contains(path)))
        })
        .collect();
    agents.sort_by(|(a, pa), (b, pb)| {
        pa.cmp(pb)
            .then((a.index().index(), a.to_bits()).cmp(&(b.index().index(), b.to_bits())))
    });
    let mut start = 0;
    while start < agents.len() {
        let file = agents[start].1.clone();
        let end = start + agents[start..].iter().take_while(|(_, f)| *f == file).count();
        let users = &agents[start..end];
        start = end;
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) => {
                let reason = format!("cannot read {}: {e}", file.display());
                for (entity, _) in users {
                    reloads.push_record(StateTreeReloadRecord {
                        entity: *entity,
                        path: file.clone(),
                        result: Err(reason.clone()),
                    });
                }
                continue;
            }
        };
        // The file and what it embedded last time, as they read now.
        let current = reloads.embeds.get(&file).map(|embedded| {
            let texts: Vec<(PathBuf, Option<String>)> = embedded
                .iter()
                .map(|path| (path.clone(), std::fs::read_to_string(path).ok()))
                .collect();
            embed_key(&text, texts.iter().map(|(p, t)| (p.as_path(), t.as_deref())))
        });
        if current.is_some() && reloads.last_text.get(&file) == current.as_ref() {
            reloads.unchanged += 1;
            continue;
        }
        reloads.file_compiles += 1;
        let loader = RecordingLoader::new(&root);
        let compiled = compile_text(&text, registry, &loader);
        reloads.last_text.insert(file.clone(), loader.key(&text));
        reloads.embeds.insert(file.clone(), loader.files());
        match compiled {
            Ok(first) => {
                // The merged definition has its includes inlined; `bt:` tasks read through the
                // same loader, so the other agents read no file again.
                let merged = first.def().clone();
                for (entity, _) in &users[1..] {
                    match StateTree::compile_with(&merged, registry, &|rel| loader.load(rel)) {
                        Ok(tree) => {
                            reloads.instances += 1;
                            reloads.queue(*entity, file.clone(), tree);
                        }
                        Err(e) => reloads.push_record(StateTreeReloadRecord {
                            entity: *entity,
                            path: file.clone(),
                            result: Err(e.to_string()),
                        }),
                    }
                }
                reloads.instances += 1;
                reloads.queue(users[0].0, file, first);
            }
            Err(reason) => {
                for (entity, _) in users {
                    reloads.push_record(StateTreeReloadRecord {
                        entity: *entity,
                        path: file.clone(),
                        result: Err(reason.clone()),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AiPlugin, Blackboard, StateTreeStatus};

    const V1: &str = r#"(
        format_version: 2,
        root: "Root",
        states: [
            (name: "Root", children: ["Idle", "Alert"]),
            (name: "Idle", transitions: [(trigger: Event("heard"), target: State("Alert"))]),
            (name: "Alert"),
        ],
    )"#;

    const V2: &str = r#"(
        format_version: 2,
        root: "Root",
        states: [
            (name: "Root", children: ["Idle", "Alert", "Extra"]),
            (name: "Idle", transitions: [(trigger: Event("heard"), target: State("Extra"))]),
            (name: "Alert"),
            (name: "Extra"),
        ],
    )"#;

    /// A state "Idle" renamed away: the active state no longer exists.
    const V3: &str = r#"(
        format_version: 2,
        root: "Root",
        states: [
            (name: "Root", children: ["Calm"]),
            (name: "Calm"),
        ],
    )"#;

    struct Rig {
        app: App,
        entity: Entity,
        file: PathBuf,
        dir: PathBuf,
    }

    impl Rig {
        fn new() -> Self {
            let dir = regia_test_utils::unique_temp_dir("st-reload");
            let file = dir.join("guard.statetree.ron");
            std::fs::write(&file, V1).expect("writes");
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, AiPlugin));
            regia_app::enable_fixed_step(&mut app, 60.0);
            let registry = BtTaskRegistry::default();
            let tree = compile_file(&dir, &file, &registry).expect("compiles");
            let entity = app
                .world_mut()
                .spawn((
                    Blackboard::default(),
                    tree,
                    StateTreeSource { path: file.clone() },
                ))
                .id();
            let mut rig = Self {
                app,
                entity,
                file,
                dir,
            };
            rig.step(3);
            rig
        }

        fn step(&mut self, n: usize) {
            for _ in 0..n {
                self.app.update();
            }
        }

        fn states(&self) -> usize {
            self.app
                .world()
                .get::<StateTree>(self.entity)
                .map_or(0, |t| t.def().states.len())
        }

        fn status(&self) -> StateTreeStatus {
            self.app
                .world()
                .get::<StateTreeStatus>(self.entity)
                .cloned()
                .expect("status")
        }

        fn change(&mut self, text: &str) {
            std::fs::write(&self.file, text).expect("writes");
            self.app
                .world_mut()
                .write_message(SourceAssetChanged::new(self.file.clone()));
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_changed_file_swaps_in_at_the_next_fixed_step_and_keeps_the_path() {
        let mut rig = Rig::new();
        assert_eq!(rig.states(), 3);
        let before = rig.status();
        assert_eq!(before.active_path, vec!["Root", "Idle"]);
        rig.change(V2);
        // The frame that sees the message only queues the tree: the running one is untouched.
        rig.step(1);
        assert_eq!(rig.app.world().resource::<StateTreeReloads>().pending(), 1);
        assert_eq!(rig.states(), 3, "no swap outside a tick");
        // The next fixed step takes it first thing.
        rig.step(1);
        assert_eq!(rig.states(), 4);
        let after = rig.status();
        assert_eq!(
            after.active_path,
            vec!["Root", "Idle"],
            "every active state survived"
        );
        assert!(
            after.ticks > before.ticks,
            "the tick count carried over and kept counting"
        );
        let reloads = rig.app.world().resource::<StateTreeReloads>();
        assert_eq!((reloads.applied, reloads.rejected, reloads.pending()), (1, 0, 0));
        assert_eq!(
            reloads.log().map(|r| r.result.clone()).collect::<Vec<_>>(),
            vec![Ok(ReloadOutcome::Kept)]
        );
        // The new transition is live: the event now leads to Extra.
        rig.app
            .world_mut()
            .entity_mut(rig.entity)
            .insert(crate::StateTreeInbox {
                events: vec!["heard".into()],
                dropped: 0,
            });
        rig.step(2);
        assert_eq!(rig.status().active_path, vec!["Root", "Extra"]);
    }

    #[test]
    fn a_definition_that_loses_the_active_state_restarts_the_tree() {
        let mut rig = Rig::new();
        rig.change(V3);
        rig.step(2);
        let reloads = rig.app.world().resource::<StateTreeReloads>();
        assert_eq!(
            reloads.log().map(|r| r.result.clone()).collect::<Vec<_>>(),
            vec![Ok(ReloadOutcome::Restarted)]
        );
        assert_eq!(rig.status().active_path, vec!["Root", "Calm"]);
    }

    #[test]
    fn a_broken_file_is_logged_and_the_running_tree_is_untouched() {
        let mut rig = Rig::new();
        let before = rig.status();
        rig.change("(this is not a state tree");
        rig.step(2);
        let reloads = rig.app.world().resource::<StateTreeReloads>();
        assert_eq!((reloads.applied, reloads.rejected, reloads.pending()), (0, 1, 0));
        assert!(reloads.log().all(|r| r.result.is_err()));
        assert_eq!(rig.states(), 3);
        let after = rig.status();
        assert_eq!(after.active_path, before.active_path);
        assert!(after.ticks > before.ticks, "the tree kept ticking");
    }

    #[test]
    fn other_files_and_agents_without_a_source_are_left_alone() {
        let mut rig = Rig::new();
        let plain = rig
            .app
            .world_mut()
            .spawn((
                Blackboard::default(),
                compile_file(&rig.dir, &rig.file, &BtTaskRegistry::default()).expect("compiles"),
            ))
            .id();
        rig.app
            .world_mut()
            .write_message(SourceAssetChanged::new(rig.dir.join("other.statetree.ron")));
        rig.step(2);
        assert_eq!(rig.app.world().resource::<StateTreeReloads>().log().count(), 0);
        rig.change(V2);
        rig.step(2);
        assert_eq!(rig.states(), 4);
        assert_eq!(
            rig.app
                .world()
                .get::<StateTree>(plain)
                .map(|t| t.def().states.len()),
            Some(3)
        );
    }

    #[test]
    fn a_hundred_agents_sharing_one_file_compile_it_once_per_save() {
        let mut rig = Rig::new();
        let registry = BtTaskRegistry::default();
        let mut agents = vec![rig.entity];
        for _ in 0..99 {
            let tree = compile_file(&rig.dir, &rig.file, &registry).expect("compiles");
            let e = rig
                .app
                .world_mut()
                .spawn((
                    Blackboard::default(),
                    tree,
                    StateTreeSource {
                        path: rig.file.clone(),
                    },
                ))
                .id();
            agents.push(e);
        }
        rig.step(2);
        rig.change(V2);
        rig.step(2);
        let reloads = rig.app.world().resource::<StateTreeReloads>();
        assert_eq!((reloads.file_compiles, reloads.instances), (1, 100));
        assert_eq!(
            (reloads.applied, reloads.rejected, reloads.pending()),
            (100, 0, 0)
        );
        for e in &agents {
            assert_eq!(
                rig.app.world().get::<StateTree>(*e).map(|t| t.def().states.len()),
                Some(4),
                "every agent runs the new definition"
            );
        }
        // A second save: one more compile, not a hundred.
        rig.change(V1);
        rig.step(2);
        let reloads = rig.app.world().resource::<StateTreeReloads>();
        assert_eq!((reloads.file_compiles, reloads.instances), (2, 200));
    }

    #[test]
    fn a_repeated_report_of_one_write_compiles_once() {
        let mut rig = Rig::new();
        rig.change(V2);
        // The editor's own report and the watcher's, in the same frame and in the next one.
        rig.app
            .world_mut()
            .write_message(SourceAssetChanged::new(rig.file.clone()));
        rig.step(1);
        rig.app
            .world_mut()
            .write_message(SourceAssetChanged::new(rig.file.clone()));
        rig.step(2);
        let reloads = rig.app.world().resource::<StateTreeReloads>();
        assert_eq!(
            (reloads.file_compiles, reloads.unchanged, reloads.applied),
            (1, 1, 1)
        );
        // New text compiles again.
        rig.change(V3);
        rig.step(2);
        assert_eq!(rig.app.world().resource::<StateTreeReloads>().file_compiles, 2);
    }

    /// A project in a temp dir with `files`, an asset server rooted there, and `agents` agents
    /// running `guard.statetree.ron` (compiled through the same loader the watcher uses).
    struct EmbedRig {
        app: App,
        agents: Vec<Entity>,
        dir: PathBuf,
    }

    impl EmbedRig {
        fn new(files: &[(&str, &str)], agents: usize) -> Self {
            let dir = regia_test_utils::unique_temp_dir("st-reload-embed");
            for (name, text) in files {
                std::fs::write(dir.join(name), text).expect("writes");
            }
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, AiPlugin));
            regia_app::enable_fixed_step(&mut app, 60.0);
            let settings = regia_asset::AssetServerSettings {
                project_root: dir.clone(),
                platform: "linux".into(),
                content_dir: None,
            };
            app.world_mut()
                .insert_resource(AssetServer::open(settings).expect("server"));
            let registry = BtTaskRegistry::default();
            let file = dir.join("guard.statetree.ron");
            let agents = (0..agents)
                .map(|_| {
                    let tree = compile_file(&dir, &file, &registry).expect("compiles");
                    app.world_mut()
                        .spawn((
                            Blackboard::default(),
                            tree,
                            StateTreeSource {
                                path: PathBuf::from("guard.statetree.ron"),
                            },
                        ))
                        .id()
                })
                .collect();
            let mut rig = Self { app, agents, dir };
            for _ in 0..3 {
                rig.app.update();
            }
            rig
        }

        /// Rewrites `name` and reports it the way the asset watcher does.
        fn change(&mut self, name: &str, text: &str) {
            let path = self.dir.join(name);
            std::fs::write(&path, text).expect("writes");
            self.app.world_mut().write_message(SourceAssetChanged::new(path));
            self.app.update();
            self.app.update();
        }

        fn states(&self, agent: usize) -> Vec<String> {
            self.app
                .world()
                .get::<StateTree>(self.agents[agent])
                .map(|t| t.def().states.iter().map(|s| s.name.clone()).collect())
                .unwrap_or_default()
        }

        fn results(&self) -> Vec<Result<ReloadOutcome, String>> {
            let reloads = self.app.world().resource::<StateTreeReloads>();
            reloads.log().map(|r| r.result.clone()).collect()
        }
    }

    impl Drop for EmbedRig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const INCLUDER: &str = r#"(
        format_version: 2,
        root: "Root",
        includes: [(prefix: "part", path: "part.statetree.ron")],
        states: [
            (name: "Root", children: ["Idle", "part/A"]),
            (name: "Idle"),
        ],
    )"#;

    const PART_1: &str = r#"(
        format_version: 2,
        root: "A",
        states: [(name: "A")],
    )"#;

    const PART_2: &str = r#"(
        format_version: 2,
        root: "A",
        states: [(name: "A", children: ["B"]), (name: "B")],
    )"#;

    /// Lane v06-editor-usability-a item 4 (w11 "state-tree, BT and include edits do not
    /// hot-reload their embedders"): an edit of an included `.statetree.ron` reloads the agents
    /// of the file that includes it, once per write.
    #[test]
    fn an_include_edit_reloads_the_agents_of_the_including_file() {
        let mut rig = EmbedRig::new(
            &[("guard.statetree.ron", INCLUDER), ("part.statetree.ron", PART_1)],
            1,
        );
        assert!(rig.states(0).contains(&"part/A".to_string()));
        assert!(!rig.states(0).contains(&"part/B".to_string()));
        rig.change("part.statetree.ron", PART_2);
        assert!(
            rig.states(0).contains(&"part/B".to_string()),
            "the include's new state is live: {:?}",
            rig.states(0)
        );
        assert_eq!(rig.results(), vec![Ok(ReloadOutcome::Kept)]);
        // The same write reported again (an editor save and the watcher) compiles nothing.
        rig.change("part.statetree.ron", PART_2);
        let reloads = rig.app.world().resource::<StateTreeReloads>();
        assert_eq!((reloads.file_compiles, reloads.unchanged), (1, 1));
        // An unrelated file reloads nothing.
        rig.change("other.statetree.ron", PART_1);
        assert_eq!(rig.app.world().resource::<StateTreeReloads>().file_compiles, 1);
    }

    const RUNS_BT: &str = r#"(
        format_version: 2,
        root: "Root",
        states: [(name: "Root", tasks: ["bt:act.bt.ron"])],
    )"#;

    const BT_RUNS_INNER: &str = "(\n    format_version: 2,\n    root: 0,\n    nodes: [\n        (name: \"sub\", kind: state_tree, children: [], key: \"\", seconds: 0.0, times: None, task: \"inner.statetree.ron\", policy: require_all),\n    ],\n)\n";

    const INNER_1: &str = r#"(format_version: 2, root: "Root", states: [(name: "Root")])"#;
    const INNER_2: &str =
        r#"(format_version: 2, root: "Root", states: [(name: "Root", children: ["Next"]), (name: "Next")])"#;
    const INNER_BROKEN: &str = r#"(format_version: 2, root: "Missing", states: [(name: "Root")])"#;

    /// Item 4: a state tree runs `bt:act.bt.ron`, which runs `inner.statetree.ron`. An edit of the
    /// innermost file reloads the agents of the outer file; a broken inner file is logged against
    /// the outer file and leaves the running tree alone. With three agents on the outer file every
    /// one reloads (the agents after the first used to compile the `bt:` task without a loader and
    /// were rejected with "could not load").
    #[test]
    fn an_edit_two_embeds_deep_reloads_every_agent_of_the_outer_file() {
        let mut rig = EmbedRig::new(
            &[
                ("guard.statetree.ron", RUNS_BT),
                ("act.bt.ron", BT_RUNS_INNER),
                ("inner.statetree.ron", INNER_1),
            ],
            3,
        );
        rig.change("inner.statetree.ron", INNER_2);
        assert_eq!(
            rig.results(),
            vec![Ok(ReloadOutcome::Kept); 3],
            "the three agents of guard.statetree.ron reloaded"
        );
        let reloads = rig.app.world().resource::<StateTreeReloads>();
        assert_eq!(
            reloads.file_compiles, 1,
            "one compile of the outer file for three agents"
        );
        assert!(
            reloads
                .log()
                .all(|r| r.path == rig.dir.join("guard.statetree.ron")),
            "logged against the agents' own file"
        );
        rig.change("act.bt.ron", BT_RUNS_INNER);
        assert_eq!(rig.results().len(), 3, "the same bytes again reload nothing");
        rig.change("inner.statetree.ron", INNER_BROKEN);
        let results = rig.results();
        assert_eq!(results.len(), 6);
        assert!(results[3..].iter().all(Result::is_err), "{results:?}");
        assert_eq!(rig.states(0), vec!["Root".to_string()], "the running tree stays");
    }
}
