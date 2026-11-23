// SPDX-License-Identifier: MPL-2.0
//! `regia procgen graph check <file>`: compile a procedural graph or variant and print every error
//! (`docs/specs/62-procedural-content-graph.md` § 8, book `agents/procgen.md`).
//! `regia procgen import-scatter <file.scatter.ron> --out <dir>`: one `.biome.ron` per scatter set
//! (`docs/specs/65-biomes-and-vegetation-scatter.md` § 9, book `agents/biomes.md`).
//! `regia procgen plant <graph> --out <dir>`: grow seeded variants of a plant graph and write them as
//! glTF plus a `<name>.plants.ron` index (`docs/specs/68-procedural-plants.md` § 8).
//!
//! `regia procgen bake <level|generator>`: runs the `Baked` generators of a level headless and writes
//! their `.pgout` files, the same as the editor's `procgen.bake` (spec 62 § 8, spec 67).
//! `regia procgen verify <level|generator>`: re-bakes into a temporary directory and compares the
//! bytes with the level's bake directory; a mismatch exits non-zero and names the files. The levels
//! a level places are loaded too: their members bake and verify in their source level's directory,
//! and an entity that lost its anchored generator is named (ADR-7331).
//!
//! Variant parents and called graphs (`graph.call`, `graph.loop`) are project-relative paths,
//! resolved against `--project` (default: the current directory).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use regia_ecs::prelude::Entity;
use regia_procgen::ProcNodeRegistry;
use regia_procgen::graph::resolve_graph;

/// `regia procgen` subcommands.
#[derive(Debug, Subcommand)]
pub enum ProcgenCommand {
    /// Procedural graph files.
    #[command(subcommand)]
    Graph(GraphCommand),
    /// Convert a `.scatter.ron` into a `.biome.ron` (one layer per class, one entry per mesh).
    ImportScatter(ImportScatterArgs),
    /// Grow `--variants` seeded variants of a Plant graph and write `<name>_<k>.glb` plus
    /// `<name>.plants.ron` into `--out`; exits non-zero on any failure.
    Plant(PlantArgs),
    /// Run a level's `Baked` generators headless and write their `.pgout` files (the editor's bake).
    Bake(BakeArgs),
    /// Re-bake into a temporary directory and compare bytes with the existing bake; exits non-zero
    /// and prints the differing files on a mismatch.
    Verify(BakeArgs),
}

/// `regia procgen bake` and `regia procgen verify`.
#[derive(Debug, Args)]
pub struct BakeArgs {
    /// A level (`<name>.level.ron` or `<name>.level.d`), or with `--level` a generator's guid or name
    /// inside that level.
    pub target: String,
    /// The level when `target` names a generator.
    #[arg(long)]
    pub level: Option<PathBuf>,
    /// Project root: graph paths resolve here (default: the current directory).
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Bake directory (default: `<level>.procgen` next to the level). For `verify`, the directory
    /// compared against.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// `bake` only: after a successful bake of the whole level, delete the `.pgout` files in the
    /// bake directory whose guid no entity of the level holds (what `verify` reports as
    /// `orphan_bake`). A file whose entity is still in the level (an entity that lost its anchored
    /// generator, a generator in another mode) stays, and nothing is deleted while a placed level or
    /// a prefab template the level needs cannot be found. Refused with a single generator and by
    /// `verify`.
    #[arg(long)]
    pub prune: bool,
    /// `bake --prune` only: print what prune would delete and why each other file stays, then stop
    /// without baking or deleting anything.
    #[arg(long)]
    pub dry_run: bool,
}

/// `regia procgen import-scatter`.
#[derive(Debug, Args)]
pub struct ImportScatterArgs {
    /// The `.scatter.ron` file.
    pub file: PathBuf,
    /// Output directory for `<stem>.biome.ron`.
    #[arg(long)]
    pub out: PathBuf,
    /// The scatter set's project-relative path written into the biome's sources (default: the
    /// file path as given).
    #[arg(long)]
    pub set_path: Option<String>,
}

/// `regia procgen plant`.
#[derive(Debug, Args)]
pub struct PlantArgs {
    /// The Plant graph (a project file or a shipped `starter/plant_*.pgraph.ron`).
    pub graph: PathBuf,
    /// Output directory.
    #[arg(long)]
    pub out: PathBuf,
    /// Number of variants (seeds `seed`, `seed + 1`, ...).
    #[arg(long, default_value_t = 1)]
    pub variants: u32,
    /// First seed.
    #[arg(long, default_value_t = 0)]
    pub seed: u32,
    /// Graph parameter override `name=value` (RON value); repeatable.
    #[arg(long = "param")]
    pub params: Vec<String>,
    /// Project root for called graphs (default: the current directory).
    #[arg(long)]
    pub project: Option<PathBuf>,
}

/// `regia procgen graph` subcommands.
#[derive(Debug, Subcommand)]
pub enum GraphCommand {
    /// Compile a `.pgraph.ron` or `.pgvar.ron` and print every error; exits non-zero on errors.
    Check(CheckArgs),
}

/// `regia procgen graph check`.
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// The graph or variant file.
    pub file: PathBuf,
    /// Project root for variant parents and called graphs (default: the current directory).
    #[arg(long)]
    pub project: Option<PathBuf>,
    /// Print the result as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Runs a `regia procgen` subcommand.
///
/// # Errors
///
/// The file cannot be read or parsed, or the graph does not compile.
pub fn run(command: &ProcgenCommand) -> Result<()> {
    match command {
        ProcgenCommand::Graph(GraphCommand::Check(args)) => check(args),
        ProcgenCommand::ImportScatter(args) => import_scatter(args),
        ProcgenCommand::Plant(args) => plant(args),
        ProcgenCommand::Bake(args) => bake_cmd(args),
        ProcgenCommand::Verify(args) => verify_cmd(args),
    }
}

/// One baked file of a headless bake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BakedFile {
    /// File name inside its bake directory (`<guid>.pgout`).
    pub name: String,
    /// The generator's guid text (its guid in the opened level).
    pub generator: String,
    /// Bytes written.
    pub bytes: u64,
    /// The file written: in the bake directory, or for a placed or additive member without a file
    /// of its own there, in its source level's directory under its source guid (ADR-7331).
    pub path: PathBuf,
}

/// The level a `bake`/`verify` target names and the generator filter.
fn resolve_target(args: &BakeArgs) -> Result<(PathBuf, Option<String>)> {
    if let Some(level) = &args.level {
        return Ok((level.clone(), Some(args.target.clone())));
    }
    let p = PathBuf::from(&args.target);
    if p.exists() {
        Ok((p, None))
    } else {
        bail!(
            "`{}` is not a level on disk; to bake one generator pass its level with --level",
            args.target
        )
    }
}

/// A headless world with a level open for baking: the level and every level it places (loaded
/// through `regia_framework::level_assets::load_level_source`, opened, placements filled), the bake
/// directory `out` as the primary directory and each placed level's own directory in
/// `ProcLevelBakeDirs` (ADR-7331), as the game resolves them.
struct BakeSession {
    app: regia_app::prelude::App,
    /// The opened level's id.
    opened: regia_scene::level::LevelId,
    /// Level path by id, for every level the load brought in.
    paths: BTreeMap<regia_scene::level::LevelId, PathBuf>,
    /// What the load could not find (placed levels, layer definitions, prefab templates) and the
    /// prefab instances whose members are not in the session: while one is listed, `--prune`
    /// cannot tell an orphan from a file of the missing content.
    unresolved: Vec<String>,
}

impl BakeSession {
    /// Loads and opens `level`. `dirs` maps each placed level's own bake directory to the one the
    /// session uses for it (identity for a bake; a scratch copy for `verify`).
    fn open(level: &Path, project: &Path, out: &Path, dirs: &dyn Fn(&Path) -> PathBuf) -> Result<Self> {
        use regia_app::prelude::{App, MinimalPlugins};
        use regia_procgen::world::{ProcGenPlugin, ProcGenSettings, ProcGraphSource, ProcLevelBakeDirs};
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Dir(project.to_path_buf()),
            bake_dir: Some(out.to_path_buf()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.finish_plugins();
        let report = regia_framework::level_assets::load_level_source(app.world_mut(), level)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", level.display()))?;
        for missing in &report.missing {
            eprintln!("warning: {}: {missing} not found", level.display());
        }
        let opened = report
            .level
            .ok_or_else(|| anyhow::anyhow!("reading {}: no level id", level.display()))?;
        let paths: BTreeMap<_, _> = app
            .world()
            .get_resource::<regia_framework::level_assets::LoadedLevelSources>()
            .map(|l| l.levels.iter().map(|(p, id)| (*id, p.clone())).collect())
            .unwrap_or_default();
        {
            let mut known = app.world_mut().resource_mut::<ProcLevelBakeDirs>();
            for (id, path) in paths.iter().filter(|(id, _)| **id != opened) {
                known
                    .0
                    .insert(*id, dirs(&regia_procgen::world::level_bake_dir(path)));
            }
        }
        regia_scene::level::open_level(app.world_mut(), opened, regia_scene::level::LoadMode::Replace)
            .map_err(|e| anyhow::anyhow!("spawning {}: {e}", level.display()))?;
        app.update();
        // Prefab instances wait for their templates: load them from the project so the generators
        // inside a template bake and verify like the game spawns them (w11 item 4).
        let mut unresolved = report.missing.clone();
        for missing in fill_pending_prefabs(&mut app, project) {
            eprintln!("warning: {}: prefab {missing} not found", level.display());
            unresolved.push(format!("prefab {missing}"));
        }
        let mut session = Self {
            app,
            opened,
            paths,
            unresolved,
        };
        let unspawned = session.unspawned_prefab_instances();
        session.unresolved.extend(unspawned);
        Ok(session)
    }

    /// The prefab instances of the loaded levels whose members are not in the session, as
    /// `unresolved` entries: a record that did not spawn (its data layer starts `Absent`, its
    /// placement failed), an instance still waiting for its template, or one whose instantiation
    /// failed. Their members' bakes are named by guids the session cannot see, so while one is
    /// listed `--prune` keeps every guid-named file. Sorted, one entry per instance.
    fn unspawned_prefab_instances(&mut self) -> Vec<String> {
        use regia_scene::components::EntityGuid;
        use regia_scene::placed::{PlacedLevel, derive_guid};
        use regia_scene::prefab::{PendingPrefab, PrefabInstance};
        let world = self.app.world_mut();
        // Every spawned guid: whether it carries a resolved instance, and the prefab it waits for.
        let live: BTreeMap<regia_core::id::Guid, (bool, Option<u128>)> = world
            .query::<(&EntityGuid, Option<&PrefabInstance>, Option<&PendingPrefab>)>()
            .iter(world)
            .map(|(g, inst, pending)| (g.0, (inst.is_some(), pending.map(|p| p.prefab))))
            .collect();
        // A placed level's records spawn under `derive_guid(placement, record)`, at any nesting.
        let placements: Vec<regia_core::id::Guid> = world
            .query_filtered::<&EntityGuid, regia_ecs::prelude::With<PlacedLevel>>()
            .iter(world)
            .map(|g| g.0)
            .collect();
        let mut out = std::collections::BTreeSet::new();
        // A pending instance anywhere (also nested in a template) still waits for its members.
        for (guid, (_, pending)) in &live {
            if let Some(prefab) = pending {
                out.insert(format!(
                    "prefab instance {guid} waits for prefab {}",
                    regia_core::id::Guid::from_u128(*prefab)
                ));
            }
        }
        let Some(library) = world.get_resource::<regia_scene::level::LevelLibrary>() else {
            return out.into_iter().collect();
        };
        for id in self.paths.keys().copied().chain(std::iter::once(self.opened)) {
            let Some(level) = library.get(id) else {
                continue;
            };
            for record in level
                .scene
                .entities
                .iter()
                .filter(|r| r.prefab_instance.is_some())
            {
                let spawned = std::iter::once(record.guid)
                    .chain(placements.iter().map(|p| derive_guid(*p, record.guid)))
                    .filter_map(|g| live.get(&g).map(|state| (g, *state)))
                    .collect::<Vec<_>>();
                if spawned
                    .iter()
                    .any(|(_, (resolved, pending))| *resolved && pending.is_none())
                {
                    continue;
                }
                let guid = record.guid;
                if spawned.is_empty() {
                    out.insert(format!(
                        "prefab instance {guid} did not spawn (its data layer is not loaded, or its placement failed)"
                    ));
                } else if spawned.iter().all(|(_, (_, pending))| pending.is_none()) {
                    out.insert(format!("prefab instance {guid} failed to instantiate"));
                }
            }
        }
        out.into_iter().collect()
    }

    /// Every guid an entity of the session holds (the opened level, the levels it places, the
    /// entities prefab instances spawned) and every record guid of the loaded level files (layers
    /// that did not spawn too): a bake file named after one is not an orphan.
    fn held_guids(&mut self) -> std::collections::BTreeSet<regia_core::id::Guid> {
        use regia_scene::components::EntityGuid;
        let world = self.app.world_mut();
        let mut held: std::collections::BTreeSet<regia_core::id::Guid> =
            world.query::<&EntityGuid>().iter(world).map(|g| g.0).collect();
        if let Some(library) = world.get_resource::<regia_scene::level::LevelLibrary>() {
            for id in self.paths.keys().copied().chain(std::iter::once(self.opened)) {
                if let Some(level) = library.get(id) {
                    held.extend(level.scene.entities.iter().map(|r| r.guid));
                }
            }
        }
        held
    }

    /// The `Baked` generators (all, or the one named by `only`: a guid or a name), in guid order.
    fn generators(&mut self, level: &Path, only: Option<&str>) -> Result<Vec<(String, Entity)>> {
        use regia_procgen::world::{ProcGenMode, ProcGenerator};
        use regia_scene::components::{EntityGuid, Name};
        let world = self.app.world_mut();
        let mut gens: Vec<(u128, String, Entity)> = world
            .query::<(Entity, &ProcGenerator, Option<&EntityGuid>, Option<&Name>)>()
            .iter(world)
            .filter(|(_, g, _, _)| g.mode == ProcGenMode::Baked)
            .filter(|(_, _, guid, name)| {
                only.is_none_or(|o| {
                    guid.is_some_and(|g| g.0.to_string() == o) || name.is_some_and(|n| n.as_str() == o)
                })
            })
            .map(|(e, _, guid, _)| {
                let g = guid.map_or(regia_core::id::Guid::NIL, |g| g.0);
                (g.0, g.to_string(), e)
            })
            .collect();
        gens.sort();
        if let Some(o) = only
            && gens.is_empty()
        {
            bail!(
                "no Baked generator named or numbered `{o}` in {}",
                level.display()
            );
        }
        Ok(gens.into_iter().map(|(_, g, e)| (g, e)).collect())
    }

    /// Bakes `gens`; every failure is printed and the call fails after the others are written.
    fn bake(&mut self, gens: Vec<(String, Entity)>) -> Result<Vec<BakedFile>> {
        let world = self.app.world_mut();
        let mut written = Vec::new();
        let mut failures = Vec::new();
        for (guid, e) in gens {
            match regia_procgen::world::bake(world, e) {
                Ok(path) => {
                    let bytes = std::fs::metadata(&path).map_or(0, |m| m.len());
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    written.push(BakedFile {
                        name,
                        generator: guid,
                        bytes,
                        path,
                    });
                }
                Err((code, msg)) => failures.push(format!("{guid}: {code}: {msg}")),
            }
        }
        if !failures.is_empty() {
            for f in &failures {
                eprintln!("error: {f}");
            }
            bail!("{} generator(s) failed to bake", failures.len());
        }
        Ok(written)
    }

    /// The path of a level the session loaded (the opened level for `None`).
    fn level_path(&self, level: Option<regia_scene::level::LevelId>) -> String {
        self.paths
            .get(&level.unwrap_or(self.opened))
            .map_or_else(|| "?".to_string(), |p| p.display().to_string())
    }
}

/// Most rounds of nested prefab loading [`fill_pending_prefabs`] runs.
const MAX_PREFAB_ROUNDS: usize = 16;

/// The `.prefab.ron` files under the project (its `assets/` directory when there is one) by the id
/// in their `.meta` sidecar.
fn prefab_index(project: &Path) -> BTreeMap<regia_core::id::Guid, PathBuf> {
    let assets = project.join("assets");
    let root = if assets.is_dir() {
        assets
    } else {
        project.to_path_buf()
    };
    let mut out = BTreeMap::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
                continue;
            }
            let name = entry.file_name();
            let Some(source) = name.to_str().and_then(|n| n.strip_suffix(".meta")) else {
                continue;
            };
            if !source.ends_with(".prefab.ron") {
                continue;
            }
            let id = std::fs::read(&path)
                .ok()
                .and_then(|bytes| regia_asset::MetaSidecar::from_ron(&bytes).ok())
                .map(|meta| meta.id.0);
            if let Some(id) = id {
                out.entry(id).or_insert_with(|| path.with_file_name(source));
            }
        }
    }
    out
}

/// Reads and bakes one `.prefab.ron` into a template with id `asset`.
fn load_template(
    app: &regia_app::prelude::App,
    asset: regia_core::id::Guid,
    path: &Path,
) -> Result<regia_scene::prefab::PrefabTemplate> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file = regia_scene::prelude::SceneFile::from_ron_str(&text)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
    let registry = app
        .world()
        .resource::<regia_ecs::reflect::AppTypeRegistry>()
        .0
        .clone();
    let registry = registry.read();
    regia_scene::prefab::PrefabTemplate::from_scene_file(asset, &file, &registry)
        .map_err(|e| anyhow::anyhow!("baking {}: {e}", path.display()))
}

/// Loads every template (with its nested prefabs) a pending prefab instance of the session waits
/// for into the `PrefabLibrary`, from the project's `.prefab.ron` files, and updates until the
/// instances spawn. Returns the ids no project file provides (or that failed to load), sorted.
fn fill_pending_prefabs(app: &mut regia_app::prelude::App, project: &Path) -> Vec<regia_core::id::Guid> {
    use regia_scene::prefab::{PendingPrefab, PrefabLibrary};
    let mut index: Option<BTreeMap<regia_core::id::Guid, PathBuf>> = None;
    let mut unresolved = std::collections::BTreeSet::new();
    for _ in 0..MAX_PREFAB_ROUNDS {
        let world = app.world_mut();
        let pending: std::collections::BTreeSet<regia_core::id::Guid> = world
            .query::<&PendingPrefab>()
            .iter(world)
            .map(|p| regia_core::id::Guid::from_u128(p.prefab))
            .collect();
        let mut loaded = false;
        for asset in pending {
            loop {
                let missing = app
                    .world()
                    .get_resource::<PrefabLibrary>()
                    .map_or(Some(asset), |l| l.first_missing(asset));
                let Some(missing) = missing.filter(|m| !unresolved.contains(m)) else {
                    break;
                };
                let index = index.get_or_insert_with(|| prefab_index(project));
                let template = index.get(&missing).map(|path| load_template(app, missing, path));
                match template {
                    Some(Ok(t)) => {
                        app.world_mut()
                            .get_resource_or_insert_with(PrefabLibrary::default)
                            .insert(t);
                        loaded = true;
                    }
                    Some(Err(e)) => {
                        eprintln!("warning: {e:#}");
                        unresolved.insert(missing);
                    }
                    None => {
                        unresolved.insert(missing);
                    }
                }
            }
        }
        if !loaded {
            break;
        }
        app.update();
    }
    unresolved.into_iter().collect()
}

/// What `regia procgen bake --prune` does with the `.pgout` files of a bake directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrunePlan {
    /// Orphans to delete, in name order: a name that is not `<guid>.pgout` /
    /// `<guid>.<Tier>.pgout`, or whose guid no entity or record of the level holds.
    pub delete: Vec<String>,
    /// Files no `Baked` generator writes that stay, with the reason, in name order.
    pub keep: Vec<(String, String)>,
}

/// Sorts the `.pgout` files of `dir` that no generator in `owners` writes (spec 62 § 8,
/// `orphan_bake`): a file whose guid is in `held` (an entity or record of the level that is not a
/// `Baked` generator: one that lost its anchored generator, or a generator in another mode) stays;
/// while `unresolved` names missing content, every guid-named file stays (it may belong to that
/// content); the rest are orphans. Other files and subdirectories (`detached/`) are not listed.
///
/// # Errors
///
/// The directory cannot be listed.
pub fn plan_prune(
    dir: &Path,
    owners: &std::collections::BTreeSet<regia_core::id::Guid>,
    held: &std::collections::BTreeSet<regia_core::id::Guid>,
    unresolved: &[String],
) -> Result<PrunePlan> {
    let mut plan = PrunePlan::default();
    for name in read_pgouts(dir)?.into_keys() {
        match regia_procgen::world::bake_file_guid(&name) {
            Some(g) if owners.contains(&g) => {}
            Some(g) if held.contains(&g) => plan.keep.push((
                name,
                format!(
                    "entity {g} is still in the level (it lost its anchored generator, or its generator is not \
                     Baked): remove the entity first"
                ),
            )),
            Some(_) if !unresolved.is_empty() => plan.keep.push((
                name,
                format!(
                    "the level has unresolved references ({}): the file may belong to them",
                    unresolved.join(", ")
                ),
            )),
            _ => plan.delete.push(name),
        }
    }
    Ok(plan)
}

/// Bakes every `Baked` generator of `level` (or the one named by `only`: a guid or a name) into
/// `out`, in guid order. Files other generators wrote earlier are left alone. The levels `level`
/// places are loaded too: a placed or additive member without a file of its own in `out` is baked
/// into its source level's directory under its source guid (ADR-7331), as `regia procgen bake`
/// of that level writes it.
///
/// # Errors
///
/// The level cannot be read or spawned, `only` matches nothing, or any generator fails to bake
/// (the message lists each failure; the files of the others are still written).
pub fn bake_level(level: &Path, project: &Path, only: Option<&str>, out: &Path) -> Result<Vec<BakedFile>> {
    let _span = tracing::info_span!(target: "regia_cli", "procgen_bake").entered();
    let mut session = BakeSession::open(level, project, out, &Path::to_path_buf)?;
    let gens = session.generators(level, only)?;
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    session.bake(gens)
}

fn bake_dir_of(args: &BakeArgs, level: &Path) -> PathBuf {
    args.out
        .clone()
        .unwrap_or_else(|| regia_procgen::world::level_bake_dir(level))
}

fn bake_cmd(args: &BakeArgs) -> Result<()> {
    let (level, only) = resolve_target(args)?;
    if args.prune && only.is_some() {
        bail!("--prune needs a whole level: it cannot tell orphans from the other generators' files");
    }
    if args.dry_run && !args.prune {
        bail!("--dry-run lists what --prune would delete: pass both");
    }
    let project = args.project.clone().unwrap_or_else(|| PathBuf::from("."));
    let out = bake_dir_of(args, &level);
    if !args.prune {
        let files = bake_level(&level, &project, only.as_deref(), &out)?;
        print_baked(&files);
        return Ok(());
    }
    let _span = tracing::info_span!(target: "regia_cli", "procgen_bake").entered();
    let mut session = BakeSession::open(&level, &project, &out, &Path::to_path_buf)?;
    let gens = session.generators(&level, only.as_deref())?;
    let guid_of = |text: &str| text.parse::<regia_core::id::Guid>().ok();
    if args.dry_run {
        let owners = gens.iter().filter_map(|(g, _)| guid_of(g)).collect();
        let held = session.held_guids();
        let plan = plan_prune(&out, &owners, &held, &session.unresolved)?;
        print_prune(&out, &plan, true);
        return Ok(());
    }
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let files = session.bake(gens)?;
    print_baked(&files);
    let owners = files.iter().filter_map(|f| guid_of(&f.generator)).collect();
    let held = session.held_guids();
    let plan = plan_prune(&out, &owners, &held, &session.unresolved)?;
    for name in &plan.delete {
        let path = out.join(name);
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    print_prune(&out, &plan, false);
    Ok(())
}

/// Prints one `baked` line per file and the count.
fn print_baked(files: &[BakedFile]) {
    for f in files {
        println!(
            "baked {} ({} bytes, generator {})",
            f.path.display(),
            f.bytes,
            f.generator
        );
    }
    println!("{} file(s) baked", files.len());
}

/// Prints a prune plan: one `pruned` (or, for a dry run, `would prune`) line per orphan, one `kept`
/// line with the reason per other file no generator writes, and the count.
fn print_prune(out: &Path, plan: &PrunePlan, dry_run: bool) {
    let verb = if dry_run { "would prune" } else { "pruned" };
    for name in &plan.delete {
        println!("{verb} {} (orphan_bake)", out.join(name).display());
    }
    for (name, why) in &plan.keep {
        println!("kept {}: {why}", out.join(name).display());
    }
    println!(
        "{} orphan file(s) {}{}",
        plan.delete.len(),
        if dry_run { "would be pruned" } else { "pruned" },
        if plan.keep.is_empty() {
            String::new()
        } else {
            format!(", {} kept", plan.keep.len())
        }
    );
}

/// The `.pgout` files of a directory as `name -> bytes`.
fn read_pgouts(dir: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(out);
    };
    for entry in rd {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".pgout") && entry.path().is_file() {
            let bytes = std::fs::read(entry.path()).with_context(|| format!("reading {name}"))?;
            out.insert(name, bytes);
        }
    }
    Ok(out)
}

/// Copies the `.pgout` files of `from` into `to` (created).
fn copy_pgouts(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for (name, bytes) in read_pgouts(from)? {
        std::fs::write(to.join(&name), bytes).with_context(|| format!("writing {name}"))?;
    }
    Ok(())
}

/// Re-bakes `level` into scratch copies of its bake directory and of the directories of the levels
/// it places, and compares every file the bake wrote byte for byte with the real one. Returns one
/// line per difference: `changed`, `missing`, `extra` (a file in `existing` no generator writes;
/// whole-level runs only), and `anchored_generator_missing` (an entity with a cell bake but no
/// generator, ADR-7331); empty means identical. A placed or additive member is compared with its
/// file in its source level's directory (ADR-7331); nothing outside the scratch directory is
/// written.
///
/// # Errors
///
/// As [`bake_level`], or a file cannot be read.
pub fn verify_level(
    level: &Path,
    project: &Path,
    only: Option<&str>,
    existing: &Path,
) -> Result<Vec<String>> {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "regia-procgen-verify-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = (|| {
        let primary = tmp.join("primary");
        copy_pgouts(existing, &primary)?;
        // Each placed level's directory is verified through a scratch copy (real -> scratch).
        let mirrors = std::cell::RefCell::new(BTreeMap::<PathBuf, PathBuf>::new());
        let mirror = |real: &Path| -> PathBuf {
            let mut m = mirrors.borrow_mut();
            let next = m.len();
            m.entry(real.to_path_buf())
                .or_insert_with(|| tmp.join(format!("level-{next}")))
                .clone()
        };
        let mut session = BakeSession::open(level, project, &primary, &mirror)?;
        let mirrors = mirrors.into_inner();
        for (real, copy) in &mirrors {
            copy_pgouts(real, copy)?;
        }
        // Scratch path -> the real path it stands for.
        let real_of = |path: &Path| -> PathBuf {
            if let Ok(rest) = path.strip_prefix(&primary) {
                return existing.join(rest);
            }
            mirrors
                .iter()
                .find_map(|(real, copy)| path.strip_prefix(copy).ok().map(|rest| real.join(rest)))
                .unwrap_or_else(|| path.to_path_buf())
        };
        // Named before the re-bake, which may load or rewrite cell bakes.
        let mut diffs = Vec::new();
        let mut named = std::collections::BTreeSet::new();
        if only.is_none() {
            for lost in regia_procgen::world::lost_anchored_generators(session.app.world_mut()) {
                let real = real_of(&lost.path);
                diffs.push(format!(
                    "anchored_generator_missing: {} in {}: {} is a cell bake but the entity has no ProcGenerator (saved while ADR-7287 (5) was open?); add the anchored generator again",
                    lost.guid,
                    session.level_path(lost.level),
                    real.display()
                ));
                named.insert(real);
            }
        }
        let held = session.held_guids();
        let gens = session.generators(level, only)?;
        let written = session.bake(gens)?;
        // A `PerTier` generator answers one path but writes one file per tier: compare them all.
        let mut fresh_files = std::collections::BTreeSet::new();
        for f in &written {
            let dir = f.path.parent().unwrap_or(&primary);
            match regia_procgen::world::bake_file_guid(&f.name) {
                Some(guid) => fresh_files.extend(regia_procgen::world::bake_files(dir, guid)),
                None => {
                    fresh_files.insert(f.path.clone());
                }
            }
        }
        let mut wrote = std::collections::BTreeSet::new();
        for path in &fresh_files {
            let real = real_of(path);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let label = if real.parent() == Some(existing) {
                name
            } else {
                real.display().to_string()
            };
            let fresh = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            match std::fs::read(&real) {
                Err(_) => diffs.push(format!(
                    "missing: {label} is not in {}",
                    real.parent().unwrap_or(existing).display()
                )),
                Ok(old) if old != fresh => diffs.push(format!(
                    "changed: {label} ({} bytes on disk, {} bytes re-baked)",
                    old.len(),
                    fresh.len()
                )),
                Ok(_) => {}
            }
            wrote.insert(real);
        }
        // With a single generator, only its own file is compared.
        if only.is_none() {
            for name in read_pgouts(existing)?.keys() {
                let real = existing.join(name);
                if !wrote.contains(&real) && !named.contains(&real) {
                    // The same rule as `plan_prune`.
                    let hint = match regia_procgen::world::bake_file_guid(name) {
                        Some(g) if held.contains(&g) => format!(
                            "its entity {g} is still in the level, so `regia procgen bake --prune` keeps it"
                        ),
                        Some(_) if !session.unresolved.is_empty() => {
                            "`regia procgen bake --prune` keeps it while the level has unresolved references"
                                .to_string()
                        }
                        _ => "`regia procgen bake --prune` removes it".to_string(),
                    };
                    diffs.push(format!(
                        "extra: {name} has no Baked generator that writes it (orphan_bake; {hint})"
                    ));
                }
            }
        }
        Ok(diffs)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn verify_cmd(args: &BakeArgs) -> Result<()> {
    if args.prune {
        bail!("--prune is a `regia procgen bake` option; verify never deletes files");
    }
    let (level, only) = resolve_target(args)?;
    let project = args.project.clone().unwrap_or_else(|| PathBuf::from("."));
    let existing = bake_dir_of(args, &level);
    let diffs = verify_level(&level, &project, only.as_deref(), &existing)?;
    if diffs.is_empty() {
        println!("ok: {} matches a fresh bake", existing.display());
        return Ok(());
    }
    for d in &diffs {
        println!("{d}");
    }
    bail!(
        "{}: {} file(s) differ from a fresh bake",
        existing.display(),
        diffs.len()
    )
}

fn import_scatter(args: &ImportScatterArgs) -> Result<()> {
    let text =
        std::fs::read_to_string(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let name = args
        .file
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".scatter.ron").or_else(|| n.split('.').next()))
        .unwrap_or("biome")
        .to_string();
    let set_path = args
        .set_path
        .clone()
        .unwrap_or_else(|| args.file.to_string_lossy().replace('\\', "/"));
    let def = regia_procgen::biome::import::biome_from_scatter_text(&text, &set_path, &name)
        .map_err(|e| anyhow::anyhow!("{}: {e}", args.file.display()))?;
    std::fs::create_dir_all(&args.out).with_context(|| format!("creating {}", args.out.display()))?;
    let out = args.out.join(format!("{name}.biome.ron"));
    regia_procgen::pgout::write_atomic(&out, def.to_ron().as_bytes())
        .with_context(|| format!("writing {}", out.display()))?;
    let entries: usize = def.layers.iter().map(|l| l.entries.len()).sum();
    println!(
        "wrote {} ({} layer(s), {entries} entr{})",
        out.display(),
        def.layers.len(),
        if entries == 1 { "y" } else { "ies" }
    );
    Ok(())
}

fn read_text(root: &Path, start: &Path, path: &str) -> Option<String> {
    let direct = Path::new(path);
    if direct == start {
        return std::fs::read_to_string(direct).ok();
    }
    // Subgraph paths come from graph files: project-relative only, no symlink out (ADR-1816).
    let file = regia_core::project_path::resolve_read(root, path).ok()?;
    std::fs::read_to_string(file).ok()
}

fn check(args: &CheckArgs) -> Result<()> {
    let root = args.project.clone().unwrap_or_else(|| PathBuf::from("."));
    let start = args.file.as_path();
    let path = start.to_string_lossy().to_string();
    let load = |p: &str| read_text(&root, start, p);
    let resolved = resolve_graph(&path, &load).with_context(|| format!("reading {}", args.file.display()))?;
    let registry = ProcNodeRegistry::with_builtins();
    let overrides: BTreeMap<String, regia_procgen::SettingValue> = resolved.merged_with(&BTreeMap::new());
    let result = resolved.graph.compile_full(&registry, &overrides, &load);
    let hash: String = resolved
        .graph
        .graph_hash(&registry)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    match result {
        Ok(compiled) => {
            if args.json {
                let warnings: Vec<_> = compiled
                    .warnings
                    .iter()
                    .map(|w| serde_json::json!({ "node": w.node, "code": w.code, "message": w.message }))
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({ "ok": true, "graph_hash": hash, "nodes": compiled.nodes.len(), "warnings": warnings })
                );
            } else {
                for w in &compiled.warnings {
                    println!("warning: {w}");
                }
                println!("ok: {} nodes, graph hash {hash}", compiled.nodes.len());
            }
            Ok(())
        }
        Err(errors) => {
            if args.json {
                let list: Vec<_> = errors
                    .0
                    .iter()
                    .map(|e| serde_json::json!({ "node": e.node, "code": e.code, "message": e.message }))
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({ "ok": false, "graph_hash": hash, "errors": list })
                );
            } else {
                for e in &errors.0 {
                    println!("error: {e}");
                }
            }
            bail!("{}: {} compile error(s)", args.file.display(), errors.0.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_a_good_and_a_bad_graph() {
        let dir = regia_test_utils::unique_temp_dir("procgen-cli");
        let good = dir.join("good.pgraph.ron");
        let mut g = regia_procgen::ProcGraph::default();
        g.nodes.insert(1, regia_procgen::NodeEntry::new("gen.grid"));
        std::fs::write(&good, g.to_ron()).expect("write");
        let bad = dir.join("bad.pgraph.ron");
        g.nodes.insert(2, regia_procgen::NodeEntry::new("no.such_node"));
        std::fs::write(&bad, g.to_ron()).expect("write");
        let args = |f: &Path| CheckArgs {
            file: f.to_path_buf(),
            project: Some(dir.clone()),
            json: true,
        };
        assert!(check(&args(&good)).is_ok());
        assert!(check(&args(&bad)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn plant(args: &PlantArgs) -> Result<()> {
    use std::sync::Arc;

    use regia_procgen::exec::StaticSnapshot;
    use regia_procgen::nodes::Domain;
    use regia_procgen::plant::{BuiltPlants, build_plant_variants};
    use regia_procgen::{CancelToken, ProcGraph, RunInputs, SettingValue};

    let root = args.project.clone().unwrap_or_else(|| PathBuf::from("."));
    let start = args.graph.as_path();
    let path = start.to_string_lossy().to_string();
    let load =
        |p: &str| read_text(&root, start, p).or_else(|| regia_procgen::plant::starter(p).map(str::to_string));
    let text = load(&path).with_context(|| format!("reading {path}"))?;
    let graph = ProcGraph::from_ron(&text).with_context(|| format!("parsing {path}"))?;
    if graph.flags.domain != Domain::Plant {
        bail!(
            "procgen_wrong_domain: {path} is a {} graph, not a Plant graph",
            graph.flags.domain.name()
        );
    }
    let mut overrides = BTreeMap::new();
    for p in &args.params {
        let (name, value) = p
            .split_once('=')
            .with_context(|| format!("--param `{p}` is not name=value"))?;
        let v = SettingValue::parse(value).with_context(|| format!("--param {name}: not a RON value"))?;
        overrides.insert(name.to_string(), v);
    }
    let registry = ProcNodeRegistry::with_builtins();
    let compiled = match graph.compile_full(&registry, &overrides, &load) {
        Ok(c) => c,
        Err(errors) => {
            for e in &errors.0 {
                eprintln!("error: {e}");
            }
            bail!("{path}: {} compile error(s)", errors.0.len());
        }
    };
    let subgraphs = regia_procgen::graph::collect_subgraphs_with(&graph, &overrides, &load);
    let mut snapshot = StaticSnapshot::default();
    for asset in regia_procgen::plant::envelope_assets(&compiled) {
        let file = regia_core::project_path::resolve_read(&root, &asset)
            .with_context(|| format!("envelope mesh {asset}"))?;
        let mesh = regia_asset_pipeline::plant_export::host_load_mesh(&file, &asset)
            .map_err(|e| anyhow::anyhow!("envelope mesh {asset}: {e}"))?;
        snapshot.meshes.insert(asset, Arc::new(mesh));
    }
    let snapshot = Arc::new(snapshot);
    std::fs::create_dir_all(&args.out).with_context(|| format!("creating {}", args.out.display()))?;
    let BuiltPlants { stem, index } = build_plant_variants(
        &compiled,
        &path,
        &|seed| RunInputs::new(snapshot.clone(), seed).with_graphs(subgraphs.clone()),
        (args.seed, args.variants),
        &CancelToken::new(),
        &mut |job, file| {
            let written = regia_asset_pipeline::plant_export::host_write(job, &args.out.join(file))?;
            println!(
                "wrote {} (triangles per LOD {:?})",
                args.out.join(file).display(),
                written.triangles_per_lod
            );
            Ok(written)
        },
    )
    .map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    let index_path = args.out.join(format!("{stem}.plants.ron"));
    let tmp = args
        .out
        .join(format!(".{stem}.plants.ron.tmp{}", std::process::id()));
    std::fs::write(&tmp, index.to_ron()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &index_path).with_context(|| format!("writing {}", index_path.display()))?;
    println!("wrote {}", index_path.display());
    Ok(())
}

#[cfg(test)]
mod plant_tests {
    use super::*;

    #[test]
    fn plant_writes_variants_and_an_index() {
        let out = regia_test_utils::unique_temp_dir("cli_procgen_plant");
        let args = PlantArgs {
            graph: PathBuf::from("starter/plant_bush.pgraph.ron"),
            out: out.clone(),
            variants: 2,
            seed: 5,
            params: vec!["growth_steps=4".into()],
            project: None,
        };
        plant(&args).expect("grows and writes");
        for k in 0..2 {
            assert!(out.join(format!("plant_bush_{k}.glb")).is_file());
        }
        let text = std::fs::read_to_string(out.join("plant_bush.plants.ron")).expect("index");
        let index = regia_procgen::plant::PlantsIndex::from_ron(&text).expect("parses");
        assert_eq!(index.variants.len(), 2);
        assert_eq!(index.variants[1].seed, 6);
        assert_eq!(index.variants[0].triangles_per_lod.len(), 4);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn plant_rejects_world_graphs() {
        let dir = regia_test_utils::unique_temp_dir("cli_procgen_plant_world");
        let g = dir.join("w.pgraph.ron");
        std::fs::write(&g, regia_procgen::ProcGraph::default().to_ron()).expect("write");
        let args = PlantArgs {
            graph: g,
            out: dir.join("out"),
            variants: 1,
            seed: 0,
            params: Vec::new(),
            project: Some(dir.clone()),
        };
        let err = plant(&args).expect_err("World graph");
        assert!(err.to_string().contains("procgen_wrong_domain"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod bake_tests {
    use super::*;

    /// A project with `g.pgraph.ron` and `maps/main.level.ron` holding one Baked generator.
    fn project() -> (PathBuf, PathBuf) {
        use regia_app::prelude::{App, MinimalPlugins};
        use regia_procgen::world::{
            ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource,
        };
        use regia_procgen::{Edge, NodeEntry};
        use regia_scene::components::{EntityGuid, Name};
        use regia_scene::prelude::{Transform, snapshot_scene};
        use regia_scene::spawn::SnapshotFilter;

        let dir = regia_test_utils::unique_temp_dir("cli_procgen_bake");
        let mut g = regia_procgen::ProcGraph::default();
        g.nodes.insert(1, NodeEntry::new("in.self"));
        g.nodes
            .insert(2, NodeEntry::new("sample.region").with_ron("spacing_m", "2.0"));
        g.nodes.insert(3, NodeEntry::new("out.points"));
        g.edges = vec![Edge::new(1, "out", 2, "region"), Edge::new(2, "out", 3, "in")];
        std::fs::write(dir.join("g.pgraph.ron"), g.to_ron()).expect("graph");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Dir(dir.clone()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.finish_plugins();
        let mut generator = ProcGenerator::new("g.pgraph.ron");
        generator.bounds = ProcBounds::Box {
            half_extents: regia_core::math::Vec3::new(6.0, 2.0, 6.0),
        };
        for (n, name) in [(5_u128, "alpha"), (3, "beta")] {
            app.world_mut().spawn((
                EntityGuid(regia_core::id::Guid(n)),
                Name::new(name),
                Transform::IDENTITY,
                generator.clone(),
            ));
        }
        let filter = SnapshotFilter {
            level: None,
            resources: Vec::new(),
        };
        let file = snapshot_scene(app.world_mut(), &filter).expect("snapshot");
        std::fs::create_dir_all(dir.join("maps")).expect("maps");
        let level = dir.join("maps/main.level.ron");
        std::fs::write(&level, file.to_ron_string().expect("ron")).expect("level");
        (dir, level)
    }

    fn bake_args(dir: &Path, level: &Path) -> BakeArgs {
        BakeArgs {
            target: level.to_string_lossy().into_owned(),
            level: None,
            project: Some(dir.to_path_buf()),
            out: None,
            prune: false,
            dry_run: false,
        }
    }

    #[test]
    fn bake_twice_is_byte_identical_and_verify_catches_an_edit() {
        let (dir, level) = project();
        let args = bake_args(&dir, &level);
        bake_cmd(&args).expect("first bake");
        let bake_dir = regia_procgen::world::level_bake_dir(&level);
        let first = read_pgouts(&bake_dir).expect("read");
        assert_eq!(first.len(), 2, "one file per generator: {:?}", first.keys());
        assert!(first.values().all(|b| !b.is_empty()));
        // A second bake writes the same bytes.
        bake_cmd(&args).expect("second bake");
        assert_eq!(read_pgouts(&bake_dir).expect("read"), first);
        // Verify is clean on an untouched bake, and does not write into the bake dir.
        verify_cmd(&args).expect("verify clean");
        assert_eq!(read_pgouts(&bake_dir).expect("read"), first);
        // Flip one byte: verify names the file and fails.
        let name = first.keys().next().expect("a file").clone();
        let path = bake_dir.join(&name);
        let mut bytes = std::fs::read(&path).expect("read");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, &bytes).expect("edit");
        let diffs = verify_level(&level, &dir, None, &bake_dir).expect("verify runs");
        assert_eq!(diffs.len(), 1, "{diffs:?}");
        assert!(
            diffs[0].starts_with("changed:") && diffs[0].contains(&name),
            "{diffs:?}"
        );
        assert!(verify_cmd(&args).is_err(), "a mismatch exits non-zero");
        // A missing file and an extra file are reported too.
        std::fs::write(&path, &first[&name]).expect("restore");
        std::fs::remove_file(&path).expect("remove");
        std::fs::write(bake_dir.join("stray.pgout"), b"x").expect("stray");
        let diffs = verify_level(&level, &dir, None, &bake_dir).expect("verify runs");
        assert!(diffs.iter().any(|d| d.starts_with("missing:")), "{diffs:?}");
        assert!(
            diffs.iter().any(|d| d.starts_with("extra: stray.pgout")),
            "{diffs:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_generator_name_bakes_only_that_generator() {
        let (dir, level) = project();
        let args = BakeArgs {
            target: "alpha".into(),
            level: Some(level.clone()),
            project: Some(dir.clone()),
            out: None,
            prune: false,
            dry_run: false,
        };
        bake_cmd(&args).expect("bake alpha");
        let files = read_pgouts(&regia_procgen::world::level_bake_dir(&level)).expect("read");
        assert_eq!(files.len(), 1);
        let missing = BakeArgs {
            target: "nope".into(),
            ..args
        };
        assert!(bake_cmd(&missing).is_err());
        let not_a_level = BakeArgs {
            target: "alpha".into(),
            level: None,
            project: Some(dir.clone()),
            out: None,
            prune: false,
            dry_run: false,
        };
        assert!(bake_cmd(&not_a_level).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec 62 § 8: verify names an entry with no generator `orphan_bake`; `bake --prune` deletes
    /// it (and only it: the generators' files, other files and `detached/` stay); `--prune` is
    /// refused with a single generator and by verify.
    #[test]
    fn prune_removes_orphan_bakes_and_verify_names_them() {
        let (dir, level) = project();
        let args = bake_args(&dir, &level);
        bake_cmd(&args).expect("bake");
        let bake_dir = regia_procgen::world::level_bake_dir(&level);
        let baked = read_pgouts(&bake_dir).expect("read");
        assert_eq!(baked.len(), 2);
        let ghost = format!("{}.pgout", regia_core::id::Guid(0xdead_beef));
        let ghost_tier = format!("{}.High.pgout", regia_core::id::Guid(0xdead_beef));
        for name in ["stray.pgout", ghost.as_str(), ghost_tier.as_str()] {
            std::fs::write(bake_dir.join(name), b"x").expect("orphan");
        }
        std::fs::write(bake_dir.join("notes.txt"), b"keep").expect("other file");
        std::fs::create_dir_all(bake_dir.join("detached")).expect("detached");
        std::fs::write(bake_dir.join("detached/m.glb"), b"keep").expect("detached file");

        let diffs = verify_level(&level, &dir, None, &bake_dir).expect("verify runs");
        assert_eq!(diffs.len(), 3, "{diffs:?}");
        assert!(
            diffs
                .iter()
                .all(|d| d.starts_with("extra: ") && d.contains("orphan_bake")),
            "{diffs:?}"
        );

        let refused = BakeArgs {
            prune: true,
            ..bake_args(&dir, &level)
        };
        assert!(verify_cmd(&refused).is_err(), "verify never prunes");
        let single = BakeArgs {
            target: "alpha".into(),
            level: Some(level.clone()),
            project: Some(dir.clone()),
            out: None,
            prune: true,
            dry_run: false,
        };
        assert!(bake_cmd(&single).is_err(), "--prune needs the whole level");
        assert_eq!(
            read_pgouts(&bake_dir).expect("read").len(),
            5,
            "nothing pruned yet"
        );

        bake_cmd(&refused).expect("bake --prune");
        assert_eq!(
            read_pgouts(&bake_dir).expect("read"),
            baked,
            "only the generators' files stay"
        );
        assert!(bake_dir.join("notes.txt").is_file());
        assert!(bake_dir.join("detached/m.glb").is_file());
        assert!(
            verify_level(&level, &dir, None, &bake_dir)
                .expect("verify")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --------------------------------------------------------------------------------------------
    // ADR-7331: placed members bake and verify in their source level's directory; levels that lost
    // an anchored generator are named
    // --------------------------------------------------------------------------------------------

    const SUB_ID: u128 = 0x5b2_0331;
    const SUB_GEN: regia_core::id::Guid = regia_core::id::Guid(0x6e6);
    const PLACEMENT: regia_core::id::Guid = regia_core::id::Guid(0x91ac);
    const LOST_GEN: regia_core::id::Guid = regia_core::id::Guid(0x7c7);

    /// A scratch world with the scene and generator plugins, for writing level records.
    fn scratch() -> regia_app::prelude::App {
        use regia_app::prelude::{App, MinimalPlugins};
        use regia_procgen::world::{ProcGenPlugin, ProcGenSettings};
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(ProcGenSettings {
            inline: true,
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.finish_plugins();
        app
    }

    fn write_level_of(app: &mut regia_app::prelude::App, path: &Path) {
        use regia_scene::prelude::snapshot_scene;
        use regia_scene::spawn::SnapshotFilter;
        let filter = SnapshotFilter {
            level: None,
            resources: Vec::new(),
        };
        let file = snapshot_scene(app.world_mut(), &filter).expect("snapshot");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("level dir");
        std::fs::write(path, file.to_ron_string().expect("ron")).expect("level");
    }

    /// A project whose `assets/levels/main.level.ron` places `assets/levels/sub.level.ron` inline
    /// (by its `.meta` id); the sublevel holds one `Baked` generator `SUB_GEN` with no pinned seed.
    /// Returns `(project, main, sub)`.
    fn placed_project(label: &str) -> (PathBuf, PathBuf, PathBuf) {
        use regia_procgen::world::{ProcBounds, ProcGenerator};
        use regia_procgen::{Edge, NodeEntry};
        use regia_scene::components::EntityGuid;
        use regia_scene::prelude::Transform;

        let dir = regia_test_utils::unique_temp_dir(label);
        let mut g = regia_procgen::ProcGraph::default();
        g.nodes.insert(1, NodeEntry::new("in.self"));
        g.nodes
            .insert(2, NodeEntry::new("sample.region").with_ron("spacing_m", "2.0"));
        g.nodes.insert(3, NodeEntry::new("out.points"));
        g.edges = vec![Edge::new(1, "out", 2, "region"), Edge::new(2, "out", 3, "in")];
        std::fs::write(dir.join("g.pgraph.ron"), g.to_ron()).expect("graph");
        let levels = dir.join("assets/levels");
        let sub = levels.join("sub.level.ron");
        let main = levels.join("main.level.ron");

        let mut app = scratch();
        let mut generator = ProcGenerator::new("g.pgraph.ron");
        generator.bounds = ProcBounds::Box {
            half_extents: regia_core::math::Vec3::new(6.0, 2.0, 6.0),
        };
        app.world_mut()
            .spawn((EntityGuid(SUB_GEN), Transform::IDENTITY, generator));
        write_level_of(&mut app, &sub);
        let sidecar =
            regia_asset::MetaSidecar::new("level", regia_asset::AssetId(regia_core::id::Guid(SUB_ID)));
        std::fs::write(
            regia_asset::meta_path_for(&sub),
            sidecar.to_ron_string().expect("meta"),
        )
        .expect("sub meta");

        let mut app = scratch();
        app.world_mut().spawn((
            regia_scene::placed::PlacedLevel::inline(regia_scene::level::LevelId(regia_core::id::Guid(
                SUB_ID,
            ))),
            EntityGuid(PLACEMENT),
            Transform::IDENTITY,
        ));
        write_level_of(&mut app, &main);
        (dir, main, sub)
    }

    /// Acceptance (ADR-7331 (2)): baking the host level re-bakes the placed member into its source
    /// level's directory under the source guid, with the bytes `regia procgen bake <sub>` writes;
    /// the host's directory gets no file for it.
    #[test]
    fn a_placed_members_rebake_writes_into_its_source_levels_directory() {
        let (dir, main, sub) = placed_project("cli_procgen_placed_rebake");
        let sub_dir = regia_procgen::world::level_bake_dir(&sub);
        let main_dir = regia_procgen::world::level_bake_dir(&main);
        let sub_file = sub_dir.join(format!("{SUB_GEN}.pgout"));
        bake_level(&main, &dir, None, &main_dir).expect("bake the host level");
        assert!(
            sub_file.is_file(),
            "the member's bake is written beside the sublevel: {}",
            sub_file.display()
        );
        let member = regia_scene::placed::derive_guid(PLACEMENT, SUB_GEN);
        assert!(
            !main_dir.join(format!("{member}.pgout")).exists(),
            "the host level's directory gets no file for the member"
        );
        // The same bytes the sublevel's own bake writes (seeded from the source guid).
        let alone = dir.join("alone.procgen");
        bake_level(&sub, &dir, None, &alone).expect("bake the sublevel alone");
        assert_eq!(
            std::fs::read(&sub_file).expect("member bake"),
            std::fs::read(alone.join(format!("{SUB_GEN}.pgout"))).expect("sublevel bake"),
            "the member re-bake equals the sublevel's own bake"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Acceptance (ADR-7331 (2)): `regia procgen verify <host>` compares a placed member with its
    /// bake in the source level's directory: clean after the sublevel's own bake, stale (named)
    /// after the file changed, and it never writes into that directory.
    #[test]
    fn verify_of_a_placed_level_reports_a_stale_member_bake() {
        let (dir, main, sub) = placed_project("cli_procgen_placed_verify");
        let sub_dir = regia_procgen::world::level_bake_dir(&sub);
        let main_dir = regia_procgen::world::level_bake_dir(&main);
        bake_level(&sub, &dir, None, &sub_dir).expect("bake the sublevel");
        let diffs = verify_level(&main, &dir, None, &main_dir).expect("verify runs");
        assert!(
            diffs.is_empty(),
            "a fresh sublevel bake verifies clean: {diffs:?}"
        );
        let sub_file = sub_dir.join(format!("{SUB_GEN}.pgout"));
        let mut bytes = std::fs::read(&sub_file).expect("read");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&sub_file, &bytes).expect("edit");
        let diffs = verify_level(&main, &dir, None, &main_dir).expect("verify runs");
        assert_eq!(diffs.len(), 1, "{diffs:?}");
        assert!(
            diffs[0].starts_with("changed:")
                && diffs[0].contains("sub.procgen")
                && diffs[0].contains(&SUB_GEN.to_string()),
            "the stale member bake is named with its directory: {diffs:?}"
        );
        assert_eq!(
            std::fs::read(&sub_file).expect("read"),
            bytes,
            "verify leaves the source level's directory alone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cell bake (two cells) for `guid`, as an anchored generator's bake looks on disk.
    fn cell_bake() -> Vec<u8> {
        use regia_procgen::exec::RunOutput;
        use regia_procgen::pgout::{PgoutHeader, encode_cells};
        let empty = RunOutput::default();
        let header = PgoutHeader {
            graph_hash: [1; 32],
            param_hash: [2; 32],
            seed: 3,
            input_hash: [4; 32],
            output_hash: empty.hash(),
            quality: 5,
        };
        encode_cells(&header, &[(0, 0, 0, &empty), (0, 1, 0, &empty)])
    }

    /// Acceptance (ADR-7331 (3)): a level saved while ADR-7287 (5) was open kept the record of an
    /// anchored generator without its `ProcGenerator`; its cell bake is still beside the level.
    /// `regia procgen verify` names it (`anchored_generator_missing`, guid and level) instead of
    /// calling the file an extra, and rewrites nothing.
    #[test]
    fn verify_names_a_level_that_lost_an_anchored_generator() {
        use regia_scene::components::EntityGuid;
        use regia_scene::prelude::Transform;
        let dir = regia_test_utils::unique_temp_dir("cli_procgen_anchored_missing");
        let level = dir.join("assets/levels/lost.level.ron");
        let mut app = scratch();
        app.world_mut().spawn((EntityGuid(LOST_GEN), Transform::IDENTITY));
        write_level_of(&mut app, &level);
        let level_text = std::fs::read(&level).expect("level");
        let bake_dir = regia_procgen::world::level_bake_dir(&level);
        std::fs::create_dir_all(&bake_dir).expect("bake dir");
        let file = bake_dir.join(format!("{LOST_GEN}.pgout"));
        std::fs::write(&file, cell_bake()).expect("cell bake");
        let diffs = verify_level(&level, &dir, None, &bake_dir).expect("verify runs");
        assert_eq!(diffs.len(), 1, "{diffs:?}");
        assert!(
            diffs[0].starts_with("anchored_generator_missing:")
                && diffs[0].contains(&LOST_GEN.to_string())
                && diffs[0].contains("lost.level.ron"),
            "the lost anchored generator is named with its guid and level: {diffs:?}"
        );
        assert_eq!(std::fs::read(&level).expect("level"), level_text, "no rewrite");
        assert_eq!(std::fs::read(&file).expect("bake"), cell_bake(), "no rewrite");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-7331 (3) through a placement: the placed sublevel lost an anchored generator whose cell
    /// bake is beside the sublevel; verifying the host level names it with the sublevel and the
    /// real file (not the scratch copy).
    #[test]
    fn verify_of_a_host_names_a_lost_anchored_generator_of_its_placed_level() {
        use regia_scene::components::EntityGuid;
        use regia_scene::prelude::Transform;
        let (dir, main, sub) = placed_project("cli_procgen_placed_anchored_missing");
        let sub_dir = regia_procgen::world::level_bake_dir(&sub);
        let main_dir = regia_procgen::world::level_bake_dir(&main);
        bake_level(&sub, &dir, None, &sub_dir).expect("bake the sublevel");
        // Rewrite the sublevel with an extra record that lost its generator, keeping the meta id.
        let mut scene = regia_scene::level_dir::read_level_source(&sub).expect("read sub");
        let mut app = scratch();
        app.world_mut().spawn((EntityGuid(LOST_GEN), Transform::IDENTITY));
        let filter = regia_scene::spawn::SnapshotFilter {
            level: None,
            resources: Vec::new(),
        };
        let extra = regia_scene::prelude::snapshot_scene(app.world_mut(), &filter).expect("snapshot");
        scene.entities.extend(extra.entities);
        std::fs::write(&sub, scene.to_ron_string().expect("ron")).expect("sub");
        let file = sub_dir.join(format!("{LOST_GEN}.pgout"));
        std::fs::write(&file, cell_bake()).expect("cell bake");
        let diffs = verify_level(&main, &dir, None, &main_dir).expect("verify runs");
        assert_eq!(diffs.len(), 1, "{diffs:?}");
        assert!(
            diffs[0].starts_with("anchored_generator_missing:")
                && diffs[0].contains(&LOST_GEN.to_string())
                && diffs[0].contains("sub.level.ron")
                && diffs[0].contains(&file.display().to_string()),
            "named with the sublevel and the real file: {diffs:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --------------------------------------------------------------------------------------------
    // w11-biome-procgen-tooling items 3 and 4: prune never deletes the bake of an entity the level
    // still holds; generators inside prefab templates bake and verify from the CLI
    // --------------------------------------------------------------------------------------------

    /// The graph `g.pgraph.ron` of the bake tests, written into `dir`.
    fn write_graph(dir: &Path) {
        use regia_procgen::{Edge, NodeEntry};
        let mut g = regia_procgen::ProcGraph::default();
        g.nodes.insert(1, NodeEntry::new("in.self"));
        g.nodes
            .insert(2, NodeEntry::new("sample.region").with_ron("spacing_m", "2.0"));
        g.nodes.insert(3, NodeEntry::new("out.points"));
        g.edges = vec![Edge::new(1, "out", 2, "region"), Edge::new(2, "out", 3, "in")];
        std::fs::write(dir.join("g.pgraph.ron"), g.to_ron()).expect("graph");
    }

    /// A `Baked` generator of `g.pgraph.ron` with a 12 m box.
    fn baked_generator() -> regia_procgen::world::ProcGenerator {
        use regia_procgen::world::{ProcBounds, ProcGenerator};
        let mut generator = ProcGenerator::new("g.pgraph.ron");
        generator.bounds = ProcBounds::Box {
            half_extents: regia_core::math::Vec3::new(6.0, 2.0, 6.0),
        };
        generator
    }

    /// Item 3 (fail-first): the level holds one `Baked` generator and an entity that lost its anchored
    /// generator (ADR-7331 (3): its record is there, its `ProcGenerator` is not) whose cell bake is
    /// beside the level. `bake --prune` deletes a true orphan (a guid no entity of the level has) but
    /// keeps the lost anchor's cell bake, byte for byte: the guid is still in the level, so the file
    /// is not an orphan until the entity is gone.
    #[test]
    fn prune_keeps_the_bake_of_a_lost_anchored_generator() {
        use regia_scene::components::EntityGuid;
        use regia_scene::prelude::Transform;
        let dir = regia_test_utils::unique_temp_dir("cli_procgen_prune_lost_anchor");
        write_graph(&dir);
        let level = dir.join("assets/levels/lost.level.ron");
        let mut app = scratch();
        app.world_mut().spawn((
            EntityGuid(regia_core::id::Guid(5)),
            Transform::IDENTITY,
            baked_generator(),
        ));
        app.world_mut().spawn((EntityGuid(LOST_GEN), Transform::IDENTITY));
        write_level_of(&mut app, &level);
        let bake_dir = regia_procgen::world::level_bake_dir(&level);
        std::fs::create_dir_all(&bake_dir).expect("bake dir");
        let lost = bake_dir.join(format!("{LOST_GEN}.pgout"));
        std::fs::write(&lost, cell_bake()).expect("cell bake");
        let ghost = bake_dir.join(format!("{}.pgout", regia_core::id::Guid(0xdead_beef)));
        std::fs::write(&ghost, b"x").expect("orphan");
        let args = BakeArgs {
            prune: true,
            ..bake_args(&dir, &level)
        };
        bake_cmd(&args).expect("bake --prune");
        assert!(!ghost.exists(), "a guid no entity has is an orphan and goes");
        assert_eq!(
            std::fs::read(&lost).ok(),
            Some(cell_bake()),
            "the lost anchored generator's cell bake stays: its entity is still in the level"
        );
        assert!(
            bake_dir
                .join(format!("{}.pgout", regia_core::id::Guid(5)))
                .is_file(),
            "the generator's own bake is written"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    const TPL_ASSET: regia_core::id::Guid = regia_core::id::Guid(0x7e_a5e7);
    const TPL_LOCAL: regia_core::id::Guid = regia_core::id::Guid(0x10ca1);
    const TPL_INST: regia_core::id::Guid = regia_core::id::Guid(0x1257);

    /// A project with `assets/prefabs/gen.prefab.ron` (asset `TPL_ASSET`; one `Baked` generator
    /// entity `TPL_LOCAL`) and `assets/levels/main.level.ron` holding one plain instance `TPL_INST`
    /// of it. Returns `(project, level, the guid the instance gives the template's generator)`.
    fn prefab_project(label: &str) -> (PathBuf, PathBuf, regia_core::id::Guid) {
        use regia_ecs::prelude::With;
        use regia_scene::components::EntityGuid;
        use regia_scene::prelude::SceneFile;
        let (dir, level) = prefab_project_with(label, PrefabCase::Plain);
        let prefab = dir.join("assets/prefabs/gen.prefab.ron");
        let level_file =
            SceneFile::from_ron_str(&std::fs::read_to_string(&level).expect("level text")).expect("level");
        // The guid the instance gives the template's generator, as the game spawns it.
        let text = std::fs::read_to_string(&prefab).expect("prefab text");
        let scene = SceneFile::from_ron_str(&text).expect("prefab parses");
        let mut app = scratch();
        let template = {
            let registry = app
                .world()
                .resource::<regia_ecs::reflect::AppTypeRegistry>()
                .0
                .clone();
            let registry = registry.read();
            regia_scene::prefab::PrefabTemplate::from_scene_file(TPL_ASSET, &scene, &registry)
                .expect("template")
        };
        app.world_mut()
            .get_resource_or_insert_with(regia_scene::prefab::PrefabLibrary::default)
            .insert(template);
        regia_scene::spawn::spawn_scene(app.world_mut(), &level_file, &Default::default())
            .expect("spawn level");
        let world = app.world_mut();
        let found: Vec<regia_core::id::Guid> = world
            .query_filtered::<&EntityGuid, With<regia_scene::prefab::PrefabLink>>()
            .iter(world)
            .map(|g| g.0)
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0],
            regia_scene::prefab::derived_guid(TPL_INST, TPL_LOCAL.as_u128()),
            "the member guid derives from the instance root"
        );
        (dir, level, found[0])
    }

    /// How [`prefab_project_with`] builds the prefab and its instance.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PrefabCase {
        /// A plain instance of a template holding one `Baked` generator.
        Plain,
        /// The instance belongs to data layer `night`, which the level starts `Absent`.
        AbsentLayer,
        /// The template also nests an instance of itself, so instantiating it fails (too deep).
        SelfNesting,
    }

    /// A project with `assets/prefabs/gen.prefab.ron` (asset `TPL_ASSET`; one `Baked` generator
    /// entity `TPL_LOCAL`) and `assets/levels/main.level.ron` holding one plain instance `TPL_INST`
    /// of it, shaped by `case`. Returns `(project, level)`.
    fn prefab_project_with(label: &str, case: PrefabCase) -> (PathBuf, PathBuf) {
        use regia_scene::components::EntityGuid;
        use regia_scene::layers::{
            LAYER_MEMBERSHIP_PATH, LEVEL_LAYERS_PATH, LayerDef, LayerDefs, LayerEntry, LayerMembership,
            LayerState, LevelLayers,
        };
        use regia_scene::prelude::{SceneEntity, SceneFile, Transform};
        let raw =
            |text: String| ron::value::RawValue::from_boxed_ron(text.into_boxed_str()).expect("raw ron");
        let plain =
            || regia_scene::format::PrefabInstanceData::plain(regia_scene::format::SceneAssetRef(TPL_ASSET));
        let dir = regia_test_utils::unique_temp_dir(label);
        write_graph(&dir);
        let prefab = dir.join("assets/prefabs/gen.prefab.ron");
        let mut app = scratch();
        app.world_mut()
            .spawn((EntityGuid(TPL_LOCAL), Transform::IDENTITY, baked_generator()));
        write_level_of(&mut app, &prefab);
        if case == PrefabCase::SelfNesting {
            let text = std::fs::read_to_string(&prefab).expect("prefab text");
            let mut file = SceneFile::from_ron_str(&text).expect("prefab parses");
            file.entities.push(SceneEntity {
                guid: regia_core::id::Guid(0x5e1f),
                parent: Some(TPL_LOCAL),
                prefab_instance: Some(plain()),
                ..SceneEntity::default()
            });
            std::fs::write(&prefab, file.to_ron_string().expect("ron")).expect("prefab");
        }
        let sidecar = regia_asset::MetaSidecar::new("prefab", regia_asset::AssetId(TPL_ASSET));
        std::fs::write(
            regia_asset::meta_path_for(&prefab),
            sidecar.to_ron_string().expect("meta"),
        )
        .expect("prefab meta");
        let mut level_file = SceneFile::new();
        let mut record = SceneEntity {
            guid: TPL_INST,
            prefab_instance: Some(plain()),
            ..SceneEntity::default()
        };
        if case == PrefabCase::AbsentLayer {
            let layers = LevelLayers {
                entries: vec![LayerEntry {
                    layer: "night".into(),
                    initial: LayerState::Absent,
                    ..LayerEntry::default()
                }],
                embedded: vec![LayerDefs {
                    format_version: regia_scene::layers::LAYERS_FORMAT_VERSION,
                    layers: vec![LayerDef {
                        name: "night".into(),
                        ..LayerDef::default()
                    }],
                }],
                ..LevelLayers::default()
            };
            level_file.resources.insert(
                LEVEL_LAYERS_PATH.to_string(),
                raw(ron::to_string(&layers).expect("layers ron")),
            );
            let membership = LayerMembership {
                layers: vec!["night".into()],
            };
            record.components.insert(
                LAYER_MEMBERSHIP_PATH.to_string(),
                raw(ron::to_string(&membership).expect("membership ron")),
            );
        }
        level_file.entities.push(record);
        let level = dir.join("assets/levels/main.level.ron");
        std::fs::create_dir_all(level.parent().expect("parent")).expect("levels");
        std::fs::write(&level, level_file.to_ron_string().expect("ron")).expect("level");
        (dir, level)
    }

    /// Writes the bake the template's generator gets in instance `TPL_INST` and runs
    /// `bake --prune` (dry run first); returns whether the file survived, and the dry-run plan.
    fn prune_member_bake(dir: &Path, level: &Path) -> (bool, PrunePlan) {
        let member = regia_scene::prefab::derived_guid(TPL_INST, TPL_LOCAL.as_u128());
        let bake_dir = regia_procgen::world::level_bake_dir(level);
        std::fs::create_dir_all(&bake_dir).expect("bake dir");
        let file = bake_dir.join(format!("{member}.pgout"));
        std::fs::write(&file, cell_bake()).expect("member bake");
        let mut session = BakeSession::open(level, dir, &bake_dir, &Path::to_path_buf).expect("session");
        assert!(
            !session.held_guids().contains(&member),
            "the member is not in the session (the case under test)"
        );
        let owners = std::collections::BTreeSet::new();
        let held = session.held_guids();
        let plan = plan_prune(&bake_dir, &owners, &held, &session.unresolved).expect("plan");
        let args = BakeArgs {
            prune: true,
            ..bake_args(dir, level)
        };
        bake_cmd(&args).expect("bake --prune runs");
        (file.is_file(), plan)
    }

    /// Review B1 (fail-first): a prefab instance in a data layer the level starts `Absent` never
    /// spawns, so its template's generator is not in the session; `bake --prune` must keep the
    /// member's bake (the game reads it when the layer goes live).
    #[test]
    fn prune_keeps_the_bake_of_a_prefab_instance_in_an_absent_layer() {
        let (dir, level) = prefab_project_with("cli_procgen_prune_absent_prefab", PrefabCase::AbsentLayer);
        let (kept, plan) = prune_member_bake(&dir, &level);
        assert!(plan.delete.is_empty(), "nothing is an orphan: {plan:?}");
        assert!(
            plan.keep
                .iter()
                .any(|(_, why)| why.contains(&format!("prefab instance {TPL_INST} did not spawn"))),
            "{plan:?}"
        );
        assert!(
            kept,
            "bake --prune deleted the member bake of an Absent-layer prefab instance"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review B2 (fail-first): an instance whose template is loaded but cannot instantiate (it
    /// nests itself) has no members in the session; `bake --prune` must keep their bakes.
    #[test]
    fn prune_keeps_the_bake_of_a_prefab_instance_that_failed_to_resolve() {
        let (dir, level) = prefab_project_with("cli_procgen_prune_failed_prefab", PrefabCase::SelfNesting);
        let (kept, plan) = prune_member_bake(&dir, &level);
        assert!(plan.delete.is_empty(), "nothing is an orphan: {plan:?}");
        assert!(
            plan.keep
                .iter()
                .any(|(_, why)| why.contains(&format!("prefab instance {TPL_INST} failed to instantiate"))),
            "{plan:?}"
        );
        assert!(
            kept,
            "bake --prune deleted the member bake of an unresolved prefab instance"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 4 (fail-first): a level with a prefab instance whose template holds a `Baked` generator.
    /// The CLI session loads the template from the project, so `regia procgen verify` reports the
    /// generator's missing bake and `regia procgen bake` writes it under the guid the instance
    /// gives the template's generator (the file the game reads); a second verify is clean.
    #[test]
    fn a_prefab_template_generator_bakes_and_verifies_from_the_cli() {
        let (dir, level, guid) = prefab_project("cli_procgen_prefab_template");
        let bake_dir = regia_procgen::world::level_bake_dir(&level);
        let diffs = verify_level(&level, &dir, None, &bake_dir).expect("verify runs");
        assert!(
            diffs
                .iter()
                .any(|d| d.starts_with("missing:") && d.contains(&format!("{guid}.pgout"))),
            "verify reports the template generator's missing bake: {diffs:?}"
        );
        let files = bake_level(&level, &dir, None, &bake_dir).expect("bake the level");
        assert_eq!(
            files.iter().map(|f| f.generator.clone()).collect::<Vec<_>>(),
            vec![guid.to_string()],
            "the template's generator is baked under its instance guid"
        );
        assert!(bake_dir.join(format!("{guid}.pgout")).is_file());
        let diffs = verify_level(&level, &dir, None, &bake_dir).expect("verify runs");
        assert!(diffs.is_empty(), "a fresh bake verifies clean: {diffs:?}");
        // And prune keeps it: the generator lives in the template.
        let args = BakeArgs {
            prune: true,
            ..bake_args(&dir, &level)
        };
        bake_cmd(&args).expect("bake --prune");
        assert!(bake_dir.join(format!("{guid}.pgout")).is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 3: `--dry-run` lists (orphan to delete, lost anchor to keep with its reason) and changes
    /// nothing: no bake is written and no file is deleted; it needs `--prune`. A level whose prefab
    /// template cannot be found prunes no guid-named file.
    #[test]
    fn prune_dry_run_lists_without_writing_and_unresolved_content_blocks_deletion() {
        use regia_scene::components::EntityGuid;
        use regia_scene::prelude::Transform;
        let dir = regia_test_utils::unique_temp_dir("cli_procgen_prune_dry_run");
        write_graph(&dir);
        let level = dir.join("assets/levels/lost.level.ron");
        let mut app = scratch();
        app.world_mut().spawn((
            EntityGuid(regia_core::id::Guid(5)),
            Transform::IDENTITY,
            baked_generator(),
        ));
        app.world_mut().spawn((EntityGuid(LOST_GEN), Transform::IDENTITY));
        write_level_of(&mut app, &level);
        let bake_dir = regia_procgen::world::level_bake_dir(&level);
        std::fs::create_dir_all(&bake_dir).expect("bake dir");
        std::fs::write(bake_dir.join(format!("{LOST_GEN}.pgout")), cell_bake()).expect("cell bake");
        let ghost = format!("{}.pgout", regia_core::id::Guid(0xdead_beef));
        std::fs::write(bake_dir.join(&ghost), b"x").expect("orphan");
        let before = read_pgouts(&bake_dir).expect("read");

        let no_prune = BakeArgs {
            dry_run: true,
            ..bake_args(&dir, &level)
        };
        assert!(bake_cmd(&no_prune).is_err(), "--dry-run needs --prune");
        let dry = BakeArgs {
            prune: true,
            dry_run: true,
            ..bake_args(&dir, &level)
        };
        bake_cmd(&dry).expect("dry run");
        assert_eq!(
            read_pgouts(&bake_dir).expect("read"),
            before,
            "nothing baked or deleted"
        );

        let mut session = BakeSession::open(&level, &dir, &bake_dir, &Path::to_path_buf).expect("session");
        let owners = std::collections::BTreeSet::from([regia_core::id::Guid(5)]);
        let held = session.held_guids();
        let plan = plan_prune(&bake_dir, &owners, &held, &session.unresolved).expect("plan");
        assert_eq!(plan.delete, vec![ghost.clone()]);
        assert_eq!(plan.keep.len(), 1, "{plan:?}");
        assert!(
            plan.keep[0].0 == format!("{LOST_GEN}.pgout") && plan.keep[0].1.contains("still in the level"),
            "{plan:?}"
        );
        // Unresolved content: guid-named files the level does not hold stay; other names still go.
        let plan = plan_prune(&bake_dir, &owners, &held, &["prefab 0x1".to_string()]).expect("plan");
        assert!(plan.delete.is_empty(), "{plan:?}");
        assert_eq!(plan.keep.len(), 2, "{plan:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
