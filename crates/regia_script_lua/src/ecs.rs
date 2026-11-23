// SPDX-License-Identifier: MPL-2.0
//! ECS wiring: [`EcsModWorld`] (the `ModWorld` over a `World`) and [`LuaModsPlugin`].

use std::collections::{BTreeMap, BTreeSet, HashMap};

use regia_app::prelude::*;
use regia_core::id::Name as InternedName;
use regia_core::math::Vec3;
use regia_ecs::ecs::change_detection::{MAX_CHANGE_AGE, Tick};
use regia_framework::save_game::{SaveError, SaveSections};
use regia_scene::Transform;
use regia_scene::components::Name;

use crate::authority::{HostCommandRules, ScriptAuthority};
use crate::components::{LuaComponent, LuaValue};
use crate::files::ModFiles;
use crate::host::LuaMods;
use crate::sandbox::LuaLimits;
use crate::value::ScriptValue;
use crate::world::ModWorld;

/// Marks an entity a mod spawned; only that mod may despawn it.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LuaSpawned {
    /// The mod's id.
    pub mod_id: String,
}

/// The entity index `bits` names. The low half of `Entity::to_bits` is not this index: the ECS
/// stores the row in a niche, and the lowest index is what `find` promises.
fn entity_index(bits: u64) -> Option<u32> {
    Entity::try_from_bits(bits).map(|entity| entity.index_u32())
}

/// How many entities mod `id` owns in `world` (`lua.entity_cap`).
pub(crate) fn owned_count(world: &mut World, id: &str) -> usize {
    let mut query = world.query::<&LuaSpawned>();
    query.iter(world).filter(|spawned| spawned.mod_id == id).count()
}

/// Name, component and spawn indexes for mod lookups.
///
/// Name buckets are ordered by entity index, so the first hit is the lowest index. Component
/// sets are ordered by entity bits, the order `entities_with` already promised. Updates are
/// idempotent: an observer and a direct write of the same change count once.
#[derive(Resource, Default)]
struct ScriptLookups {
    names: HashMap<InternedName, BTreeMap<u32, u64>>,
    name_of: HashMap<u64, InternedName>,
    components: HashMap<String, BTreeSet<u64>>,
    decl_of: HashMap<u64, String>,
    spawned_of: HashMap<u64, String>,
    owned: HashMap<String, usize>,
    /// World change tick of the last sweep for in-place edits (`sync_script_lookups`). `None`
    /// until a rebuild set it.
    synced: Option<Tick>,
}

impl ScriptLookups {
    fn lowest(&self, name: &str, except: &[u64]) -> Option<u64> {
        self.names
            .get(name)
            .and_then(|bucket| bucket.values().copied().find(|bits| !except.contains(bits)))
    }

    fn note_name(&mut self, bits: u64, name: &Name) {
        let Some(index) = entity_index(bits) else {
            return;
        };
        let interned = name.0.clone();
        if self.name_of.get(&bits).is_some_and(|old| old == &interned) {
            return;
        }
        self.forget_name(bits);
        self.names
            .entry(interned.clone())
            .or_default()
            .insert(index, bits);
        self.name_of.insert(bits, interned);
    }

    fn forget_name(&mut self, bits: u64) {
        let Some(old) = self.name_of.remove(&bits) else {
            return;
        };
        let empty = {
            let Some(bucket) = self.names.get_mut(&old) else {
                return;
            };
            if let Some(index) = entity_index(bits) {
                bucket.remove(&index);
            } else {
                bucket.retain(|_, stored| *stored != bits);
            }
            bucket.is_empty()
        };
        if empty {
            self.names.remove(&old);
        }
    }

    fn note_component(&mut self, bits: u64, decl: &str) {
        if self.decl_of.get(&bits).is_some_and(|old| old == decl) {
            return;
        }
        self.forget_component(bits);
        self.components.entry(decl.to_owned()).or_default().insert(bits);
        self.decl_of.insert(bits, decl.to_owned());
    }

    fn forget_component(&mut self, bits: u64) {
        let Some(decl) = self.decl_of.remove(&bits) else {
            return;
        };
        let empty = self.components.get_mut(&decl).is_some_and(|set| {
            set.remove(&bits);
            set.is_empty()
        });
        if empty {
            self.components.remove(&decl);
        }
    }

    fn with_decl(&self, decl: &str) -> Vec<u64> {
        self.components
            .get(decl)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    fn note_spawned(&mut self, bits: u64, mod_id: &str) {
        if self.spawned_of.get(&bits).is_some_and(|old| old == mod_id) {
            return;
        }
        self.forget_spawned(bits);
        *self.owned.entry(mod_id.to_owned()).or_default() += 1;
        self.spawned_of.insert(bits, mod_id.to_owned());
    }

    fn forget_spawned(&mut self, bits: u64) {
        let Some(id) = self.spawned_of.remove(&bits) else {
            return;
        };
        if let Some(count) = self.owned.get_mut(&id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.owned.remove(&id);
            }
        }
    }

    fn owned_of(&self, owner: &str) -> usize {
        self.owned.get(owner).copied().unwrap_or(0)
    }
}

/// [`ModWorld`] over an ECS world: names are the `Name` component, positions the local
/// `Transform` translation.
pub struct EcsModWorld<'w> {
    world: &'w mut World,
    scanned: u64,
    /// Script-component rows [`entities_with`](ModWorld::entities_with) examined since the last take.
    component_visits: u64,
    /// The lookup index has caught up with in-place edits for this wrapper.
    lookups_synced: bool,
}

impl<'w> EcsModWorld<'w> {
    /// Wraps `world` for a mod call.
    pub fn new(world: &'w mut World) -> Self {
        Self {
            world,
            scanned: 0,
            component_visits: 0,
            lookups_synced: false,
        }
    }

    /// Whether the plugin's lookup index exists. The first call on this wrapper also brings it
    /// up to date with edits no observer sees (a `Name`, `LuaComponent` or `LuaSpawned` changed
    /// in place through `&mut`, as the inspector and game systems do). Nothing a mod can do
    /// during the wrapper's life edits those in place, so one sweep per wrapper is enough.
    fn lookups_ready(&mut self) -> bool {
        if !self.world.contains_resource::<ScriptLookups>() {
            return false;
        }
        if !self.lookups_synced {
            sync_script_lookups(self.world);
            self.lookups_synced = true;
        }
        true
    }

    /// How many script-component rows the lookups since the last take examined.
    ///
    /// A world with the plugin's component index counts the entities that carry the
    /// declaration. A world without it counts every script component it walked.
    #[must_use]
    pub fn take_component_visits(&mut self) -> u64 {
        std::mem::take(&mut self.component_visits)
    }

    fn entity(&self, bits: u64) -> Option<Entity> {
        let entity = Entity::try_from_bits(bits)?;
        self.world.get_entity(entity).is_ok().then_some(entity)
    }

    /// The indexed entity still carries `name`. A stale hit rebuilds once.
    fn name_matches(&self, bits: u64, name: &str) -> bool {
        self.entity(bits)
            .and_then(|entity| self.world.get::<Name>(entity))
            .is_some_and(|found| found.as_str() == name)
    }
}

impl ModWorld for EcsModWorld<'_> {
    fn find(&mut self, name: &str) -> Option<u64> {
        self.find_except(name, &[])
    }

    fn find_except(&mut self, name: &str, except: &[u64]) -> Option<u64> {
        if self.lookups_ready() {
            return self.find_indexed(name, except);
        }
        let mut query = self.world.query::<(Entity, &Name)>();
        let mut found: Option<Entity> = None;
        for (entity, n) in query.iter(self.world) {
            self.scanned += 1;
            if n.as_str() == name
                && !except.contains(&entity.to_bits())
                && found.is_none_or(|f| entity.index_u32() < f.index_u32())
            {
                found = Some(entity);
            }
        }
        found.map(Entity::to_bits)
    }

    fn take_scanned(&mut self) -> u64 {
        std::mem::take(&mut self.scanned)
    }

    fn find_cost(&mut self) -> u64 {
        if self.world.get_resource::<ScriptLookups>().is_some() {
            1
        } else {
            self.world.entities().len() as u64
        }
    }

    fn alive(&mut self, entity: u64) -> bool {
        self.entity(entity).is_some()
    }

    fn name(&mut self, entity: u64) -> Option<String> {
        let entity = self.entity(entity)?;
        self.world.get::<Name>(entity).map(|n| n.as_str().to_owned())
    }

    fn position(&mut self, entity: u64) -> Option<Vec3> {
        let entity = self.entity(entity)?;
        self.world.get::<Transform>(entity).map(|t| t.translation)
    }

    fn set_position(&mut self, entity: u64, position: Vec3) -> bool {
        let Some(entity) = self.entity(entity) else {
            return false;
        };
        match self.world.get_mut::<Transform>(entity) {
            Some(mut t) => {
                t.translation = position;
                true
            }
            None => false,
        }
    }

    fn spawn(&mut self, owner: &str, name: &str, position: Vec3) -> Option<u64> {
        let entity = self
            .world
            .spawn((
                Name::new(name),
                Transform::from_translation(position),
                LuaSpawned {
                    mod_id: owner.to_owned(),
                },
            ))
            .id();
        let bits = entity.to_bits();
        if let Some(mut lookups) = self.world.get_resource_mut::<ScriptLookups>() {
            lookups.note_name(bits, &Name::new(name));
            lookups.note_spawned(bits, owner);
        }
        Some(bits)
    }

    fn despawn(&mut self, owner: &str, entity: u64) -> bool {
        let Some(entity) = self.entity(entity) else {
            return false;
        };
        let owned = self
            .world
            .get::<LuaSpawned>(entity)
            .is_some_and(|s| s.mod_id == owner);
        if !owned {
            return false;
        }
        let bits = entity.to_bits();
        if let Some(mut lookups) = self.world.get_resource_mut::<ScriptLookups>() {
            lookups.forget_name(bits);
            lookups.forget_component(bits);
            lookups.forget_spawned(bits);
        }
        self.world.despawn(entity)
    }

    fn owned(&mut self, owner: &str) -> Vec<u64> {
        let mut query = self.world.query::<(Entity, &LuaSpawned)>();
        let mut owned: Vec<u64> = query
            .iter(self.world)
            .filter(|(_, spawned)| spawned.mod_id == owner)
            .map(|(entity, _)| entity.to_bits())
            .collect();
        owned.sort_unstable();
        owned
    }

    fn owned_count(&mut self, owner: &str) -> usize {
        if self.lookups_ready()
            && let Some(lookups) = self.world.get_resource::<ScriptLookups>()
        {
            lookups.owned_of(owner)
        } else {
            owned_count(self.world, owner)
        }
    }

    fn has_component(&mut self, entity: u64, decl: &str) -> bool {
        let Some(entity) = self.entity(entity) else {
            return false;
        };
        self.world
            .get::<LuaComponent>(entity)
            .is_some_and(|component| component.decl == decl)
    }

    fn attach_component(&mut self, entity: u64, decl: &str, initial: Vec<(String, ScriptValue)>) -> bool {
        let Some(entity) = self.entity(entity) else {
            return false;
        };
        if self.world.get::<LuaComponent>(entity).is_some() {
            // One script component per entity (ADR-3356): the name is the identity.
            return false;
        }
        let mut component = LuaComponent {
            decl: decl.to_owned(),
            props: BTreeMap::new(),
        };
        component.set_values(initial);
        let bits = entity.to_bits();
        self.world.entity_mut(entity).insert(component);
        if let Some(mut lookups) = self.world.get_resource_mut::<ScriptLookups>() {
            lookups.note_component(bits, decl);
        }
        true
    }

    fn remove_component(&mut self, entity: u64, decl: &str) -> bool {
        let Some(entity) = self.entity(entity) else {
            return false;
        };
        if !self.has_component(entity.to_bits(), decl) {
            return false;
        }
        let bits = entity.to_bits();
        if let Some(mut lookups) = self.world.get_resource_mut::<ScriptLookups>() {
            lookups.forget_component(bits);
        }
        self.world.entity_mut(entity).remove::<LuaComponent>();
        true
    }

    fn component_props(&mut self, entity: u64, decl: &str) -> Option<Vec<(String, ScriptValue)>> {
        let entity = self.entity(entity)?;
        let component = self.world.get::<LuaComponent>(entity)?;
        (component.decl == decl).then(|| component.values())
    }

    fn set_component_props(&mut self, entity: u64, decl: &str, props: Vec<(String, ScriptValue)>) -> bool {
        let Some(entity) = self.entity(entity) else {
            return false;
        };
        let Some(mut component) = self.world.get_mut::<LuaComponent>(entity) else {
            return false;
        };
        if component.decl != decl {
            return false;
        }
        component.set_values(props);
        true
    }

    fn set_component_prop(&mut self, entity: u64, decl: &str, key: &str, value: ScriptValue) -> bool {
        let Some(entity) = self.entity(entity) else {
            return false;
        };
        let Some(mut component) = self.world.get_mut::<LuaComponent>(entity) else {
            return false;
        };
        if component.decl != decl {
            return false;
        }
        component
            .props
            .insert(key.to_owned(), LuaValue::from_script(&value));
        true
    }

    fn entities_with(&mut self, decl: &str) -> Vec<u64> {
        let indexed = if self.lookups_ready() {
            self.world
                .get_resource::<ScriptLookups>()
                .map(|lookups| lookups.with_decl(decl))
        } else {
            None
        };
        if let Some(found) = indexed {
            self.component_visits += found.len() as u64;
            return found;
        }
        let mut query = self.world.query::<(Entity, &LuaComponent)>();
        let mut found: Vec<u64> = Vec::new();
        for (entity, component) in query.iter(self.world) {
            // No index: count every row, including declarations this pass ignores.
            self.component_visits += 1;
            if component.decl == decl {
                found.push(entity.to_bits());
            }
        }
        found.sort_unstable();
        found
    }

    fn component_cost(&mut self) -> u64 {
        if self.world.get_resource::<ScriptLookups>().is_some() {
            1
        } else {
            self.world.entities().len() as u64
        }
    }
}

impl EcsModWorld<'_> {
    /// One index probe. A hit whose `Name` no longer matches rebuilds the index once.
    fn find_indexed(&mut self, name: &str, except: &[u64]) -> Option<u64> {
        self.scanned += 1;
        let found = self
            .world
            .get_resource::<ScriptLookups>()
            .and_then(|lookups| lookups.lowest(name, except));
        let bits = found?;
        if self.name_matches(bits, name) {
            return Some(bits);
        }
        rebuild_script_lookups(self.world);
        self.scanned += 1;
        let again = self
            .world
            .get_resource::<ScriptLookups>()
            .and_then(|lookups| lookups.lowest(name, except));
        again.filter(|bits| self.name_matches(*bits, name))
    }
}

/// Mods waiting for the plugin's startup system.
#[derive(Resource, Default)]
struct PendingLuaMods(Vec<ModFiles>);

/// Adds [`LuaMods`], loads the given mods at `Startup` (in id order), calls `on_update` in
/// `Update` and `on_tick` plus the queued events in `FixedUpdate`, registers the `lua.*` agent
/// methods, and adds the mods' state to save games as a save section (every path through
/// `regia_framework`'s `SaveGame`).
#[derive(Clone)]
pub struct LuaModsPlugin {
    seed: u64,
    limits: LuaLimits,
    mods: Vec<ModFiles>,
    watch_every: u32,
    authority: ScriptAuthority,
    host_commands: HostCommandRules,
}

impl LuaModsPlugin {
    /// A plugin loading `mods` with seed 0 and the default limits.
    #[must_use]
    pub fn new(mods: Vec<ModFiles>) -> Self {
        Self {
            seed: 0,
            limits: LuaLimits::default(),
            mods,
            watch_every: 0,
            authority: ScriptAuthority::Host,
            host_commands: HostCommandRules::default(),
        }
    }

    /// Runs the mods on `authority`'s side of a networked game (ADR-8466): a client uses
    /// [`ScriptAuthority::Presentation`], which has no `regia.command`. The default is the host.
    #[must_use]
    pub fn with_authority(mut self, authority: ScriptAuthority) -> Self {
        self.authority = authority;
        self
    }

    /// The command names `regia.command` accepts on the host, and the queue caps (ADR-8466). The
    /// game drains the queue with `LuaMods::drain_host_commands`; the plugin does not.
    #[must_use]
    pub fn with_host_commands(mut self, rules: HostCommandRules) -> Self {
        self.host_commands = rules;
        self
    }

    /// Sets the host seed (deterministic per-mod random numbers).
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Sets the host limits.
    #[must_use]
    pub fn with_limits(mut self, limits: LuaLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Hot-reloads mods read from a directory when their files change, looking every `frames`
    /// frames (0, the default, is off). For development: it reads files on the game thread.
    #[must_use]
    pub fn with_watch(mut self, frames: u32) -> Self {
        self.watch_every = frames;
        self
    }
}

impl Plugin for LuaModsPlugin {
    fn build(&self, app: &mut App) {
        let mut mods = LuaMods::new(self.seed, self.limits)
            .with_authority(self.authority)
            .with_host_commands(self.host_commands.clone());
        mods.set_watch(self.watch_every);
        app.insert_resource(mods);
        app.insert_resource(PendingLuaMods(self.mods.clone()));
        // Script components persist in scenes through the reflection codec.
        app.register_type::<LuaComponent>()
            .register_type::<crate::components::LuaValue>();
        app.add_systems(Startup, load_pending_mods);
        app.add_systems(Update, lua_mods_update);
        app.add_systems(FixedUpdate, lua_mods_tick);
        app.add_observer(index_name_inserted)
            .add_observer(index_name_removed)
            .add_observer(index_component_inserted)
            .add_observer(index_component_removed)
            .add_observer(index_spawned_inserted)
            .add_observer(index_spawned_removed);
        rebuild_script_lookups(app.world_mut());
        crate::agent::install(app.world_mut());
        crate::fuzz::install(app.world_mut());
        app.world_mut()
            .get_resource_or_init::<SaveSections>()
            .register_required(crate::LUA_STATE_SECTION, write_state_section, read_state_section);
    }
}

fn index_name_inserted(on: On<Insert<Name>>, names: Query<&Name>, mut lookups: ResMut<ScriptLookups>) {
    let entity = on.event().entity;
    let Ok(name) = names.get(entity) else {
        return;
    };
    lookups.note_name(entity.to_bits(), name);
}

fn index_name_removed(on: On<Remove<Name>>, mut lookups: ResMut<ScriptLookups>) {
    lookups.forget_name(on.event().entity.to_bits());
}

fn index_component_inserted(
    on: On<Insert<LuaComponent>>,
    components: Query<&LuaComponent>,
    mut lookups: ResMut<ScriptLookups>,
) {
    let entity = on.event().entity;
    let Ok(component) = components.get(entity) else {
        return;
    };
    lookups.note_component(entity.to_bits(), &component.decl);
}

fn index_component_removed(on: On<Remove<LuaComponent>>, mut lookups: ResMut<ScriptLookups>) {
    lookups.forget_component(on.event().entity.to_bits());
}

fn index_spawned_inserted(
    on: On<Insert<LuaSpawned>>,
    spawned: Query<&LuaSpawned>,
    mut lookups: ResMut<ScriptLookups>,
) {
    let entity = on.event().entity;
    let Ok(mark) = spawned.get(entity) else {
        return;
    };
    lookups.note_spawned(entity.to_bits(), &mark.mod_id);
}

fn index_spawned_removed(on: On<Remove<LuaSpawned>>, mut lookups: ResMut<ScriptLookups>) {
    lookups.forget_spawned(on.event().entity.to_bits());
}

/// Replaces the lookup resource from the entities that exist now. Does not touch a call's
/// scan counters.
fn rebuild_script_lookups(world: &mut World) {
    let mut lookups = ScriptLookups::default();
    {
        let mut names = world.query::<(Entity, &Name)>();
        for (entity, name) in names.iter(world) {
            lookups.note_name(entity.to_bits(), name);
        }
    }
    {
        let mut components = world.query::<(Entity, &LuaComponent)>();
        for (entity, component) in components.iter(world) {
            lookups.note_component(entity.to_bits(), &component.decl);
        }
    }
    {
        let mut spawned = world.query::<(Entity, &LuaSpawned)>();
        for (entity, mark) in spawned.iter(world) {
            lookups.note_spawned(entity.to_bits(), &mark.mod_id);
        }
    }
    lookups.synced = Some(world.change_tick());
    world.insert_resource(lookups);
}

/// Re-notes every name, script component and spawn mark changed since the last sweep. Insert
/// and remove observers keep the index current for structural changes. An edit through `&mut`
/// (an inspector rename, a reflected apply, a game system writing `Name`) fires neither, so
/// without this `regia.find` of the new name answered nil and `entities_with` kept the old
/// declaration. Costs one change-tick check per named entity, only on a wrapper that looks
/// something up.
fn sync_script_lookups(world: &mut World) {
    let Some(synced) = world
        .get_resource::<ScriptLookups>()
        .map(|lookups| lookups.synced)
    else {
        return;
    };
    let _span = tracing::debug_span!(target: "regia_script_lua", "lua_lookups_sync").entered();
    let this_run = world.change_tick();
    let Some(synced) = synced else {
        rebuild_script_lookups(world);
        return;
    };
    if this_run.get().wrapping_sub(synced.get()) > MAX_CHANGE_AGE / 2 {
        // Change ticks older than the ECS's horizon stop comparing: start over.
        rebuild_script_lookups(world);
        return;
    }
    // One tick back: a direct write in the same exclusive system as the last sweep carries the
    // sweep's own tick. Re-noting an unchanged entry is a no-op.
    let since = Tick::new(synced.get().wrapping_sub(1));
    let mut names: Vec<(u64, Name)> = Vec::new();
    {
        let mut query = world.query::<(Entity, Ref<Name>)>();
        for (entity, name) in query.iter(world) {
            if name.last_changed().is_newer_than(since, this_run) {
                names.push((entity.to_bits(), Name::clone(&name)));
            }
        }
    }
    let mut decls: Vec<(u64, String)> = Vec::new();
    {
        let mut query = world.query::<(Entity, Ref<LuaComponent>)>();
        for (entity, component) in query.iter(world) {
            if component.last_changed().is_newer_than(since, this_run) {
                decls.push((entity.to_bits(), component.decl.clone()));
            }
        }
    }
    let mut marks: Vec<(u64, String)> = Vec::new();
    {
        let mut query = world.query::<(Entity, Ref<LuaSpawned>)>();
        for (entity, mark) in query.iter(world) {
            if mark.last_changed().is_newer_than(since, this_run) {
                marks.push((entity.to_bits(), mark.mod_id.clone()));
            }
        }
    }
    let mut lookups = world.resource_mut::<ScriptLookups>();
    for (bits, name) in &names {
        lookups.note_name(*bits, name);
    }
    for (bits, decl) in &decls {
        lookups.note_component(*bits, decl);
    }
    for (bits, mod_id) in &marks {
        lookups.note_spawned(*bits, mod_id);
    }
    lookups.synced = Some(this_run);
}

/// The save section's write: `None` when a game removed the resource.
fn write_state_section(world: &mut World) -> Result<Option<Vec<u8>>, SaveError> {
    if !world.contains_resource::<LuaMods>() {
        return Ok(None);
    }
    world.resource_scope::<LuaMods, _>(|world, mut mods| {
        let (bytes, report) = mods.save_state(&mut EcsModWorld::new(world));
        if bytes.is_empty() {
            let message = report
                .mods
                .iter()
                .find_map(|line| line.error.as_ref())
                .map_or_else(
                    || "lua_state: the Lua state could not be encoded".to_owned(),
                    |error| format!("{}: {}", error.code.code(), error.message),
                );
            return Err(SaveError::Serialize(message));
        }
        Ok(Some(bytes))
    })
}

/// The save section's read. A mod that fails is reported on `lua.list` and `lua.errors`; the load
/// of the game goes on.
fn read_state_section(world: &mut World, bytes: Option<&[u8]>) -> Result<(), SaveError> {
    world.try_resource_scope::<LuaMods, _>(|world, mut mods| {
        mods.restore_state(bytes, &mut EcsModWorld::new(world));
    });
    Ok(())
}

fn load_pending_mods(world: &mut World) {
    let Some(PendingLuaMods(mut pending)) = world.remove_resource::<PendingLuaMods>() else {
        return;
    };
    // Load in id order whatever order the game listed them in; unreadable manifests sort first
    // and fail on their own. Mounts claiming one id load in mount order (no mount, then `dir:`,
    // then `mods:`, so a local mod keeps its id against a Workshop item); the later claims are
    // refused with `lua_id_taken` (ADR-7881).
    pending.sort_by_cached_key(|files| {
        let id = files
            .get(crate::LUAMOD_FILE)
            .and_then(|t| crate::LuaModManifest::parse(t).ok())
            .map(|m| m.id)
            .unwrap_or_default();
        (id, files.mount().map(str::to_owned))
    });
    // `try_`: a game may have removed the resource; then there is nothing to load into.
    world.try_resource_scope::<LuaMods, _>(|world, mut mods| {
        for files in pending {
            if let Err(error) = mods.load(files, &mut EcsModWorld::new(world)) {
                tracing::warn!(target: "regia_script_lua", code = error.code.code(), "mod failed to load: {}", error.message);
            }
        }
    });
}

fn frame_delta(world: &World) -> f64 {
    world
        .get_resource::<Time>()
        .map_or(0.0, |t| f64::from(t.delta_secs()))
}

fn lua_mods_update(world: &mut World) {
    if !world.contains_resource::<LuaMods>() {
        return;
    }
    let dt = frame_delta(world);
    world.resource_scope::<LuaMods, _>(|world, mut mods| {
        let mut ecs = EcsModWorld::new(world);
        mods.call_update(&mut ecs, dt);
        mods.tick_watch(&mut ecs);
    });
}

fn lua_mods_tick(world: &mut World) {
    if !world.contains_resource::<LuaMods>() {
        return;
    }
    let dt = frame_delta(world);
    world.resource_scope::<LuaMods, _>(|world, mut mods| {
        mods.call_tick(&mut EcsModWorld::new(world), dt);
    });
}
