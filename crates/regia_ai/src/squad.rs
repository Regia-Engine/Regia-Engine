// SPDX-License-Identifier: MPL-2.0
//! Squads (ADR-2709, `book/agents/ai-squads.md`): groups of agents with a shared [`Blackboard`], role
//! slots and a leader.
//!
//! A squad is an entity with [`AiSquad`] (and the `Blackboard` it requires). An agent joins by getting an
//! [`AiSquadMember`] naming that entity and leaves by losing it or by being despawned. An agent that dies
//! but stays in the world gets [`AiDead`]. All of this takes effect at once through component hooks, so a
//! member's death changes the leader, the roles and the attack tokens within the same step, whichever
//! system caused it.
//!
//! **Leader rule.** The member with the highest [`AiSquadMember::rank`] leads; on equal ranks the one that
//! joined first. An election runs when a squad with no leader gains a member and when the leader leaves
//! or dies, never otherwise: a new member with a higher rank does not depose a living leader, and writing
//! a member's `rank` elects nobody. Inserting `AiSquadMember` over an existing one is leaving and
//! rejoining: the member goes to the end of the join order, and if it led, a new leader is elected.
//!
//! **Role rule.** [`SquadRole::auto_fill`] slots are filled when a slot or a member becomes free: roles in
//! declared order, each taking roleless members by the leader rule's order.
//!
//! Each member's own `Blackboard`, when it has one, gets `squad_is_leader` (a bool) and `squad_role` (a
//! string, absent with no role); the squad's gets `squad_leader` (an entity, absent with no leader).

use regia_app::prelude::*;
use regia_ecs::ecs::lifecycle::HookContext;
use regia_ecs::ecs::world::DeferredWorld;

use crate::attack_tokens::release_all_on_death;
use crate::blackboard::{BbValue, Blackboard};
use crate::formation::AiFormation;
use crate::task::{BtContext, BtStatus, BtTask, BtTaskRegistration};
use crate::tree::BtTaskRegistry;

/// A member's blackboard key: whether it leads its squad.
pub const SQUAD_IS_LEADER_KEY: &str = "squad_is_leader";
/// A member's blackboard key: its role's name.
pub const SQUAD_ROLE_KEY: &str = "squad_role";
/// The squad's blackboard key: its leader.
pub const SQUAD_LEADER_KEY: &str = "squad_leader";

/// One named role of a squad, with room for `capacity` members.
#[derive(Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Default)]
pub struct SquadRole {
    /// The role's name.
    pub name: String,
    /// How many members may hold it.
    pub capacity: u32,
    /// Fill free slots from roleless members automatically (the module's role rule).
    pub auto_fill: bool,
    /// The members holding it, in the order they got it.
    pub holders: Vec<Entity>,
}

impl SquadRole {
    /// A role that fills itself.
    pub fn new(name: impl Into<String>, capacity: u32) -> Self {
        Self {
            name: name.into(),
            capacity,
            auto_fill: true,
            holders: Vec::new(),
        }
    }
}

/// A squad: the entity holding this component is the group. Its `Blackboard` is shared by the members.
/// `members`, `leader` and each role's `holders` are kept by the hooks: read them, and change them through
/// [`AiSquadMember`], [`set_squad_leader`], [`claim_squad_role`] and [`release_squad_role`].
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
#[require(Blackboard)]
#[component(on_remove = squad_removed)]
pub struct AiSquad {
    /// A name for tools.
    pub name: String,
    /// The members, in the order they joined.
    pub members: Vec<Entity>,
    /// The leader.
    pub leader: Option<Entity>,
    /// The role slots.
    pub roles: Vec<SquadRole>,
    /// How many elections have run.
    pub elections: u32,
}

impl AiSquad {
    /// An empty squad.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Adds a role slot. Roles should be declared before members join.
    pub fn with_role(mut self, role: SquadRole) -> Self {
        self.roles.push(role);
        self
    }
}

/// Membership of a squad. The squad entity must exist first.
#[derive(Component, Reflect, Debug, Clone, PartialEq, Eq)]
#[reflect(Component, Default)]
#[component(on_insert = member_inserted, on_discard = member_discarded)]
pub struct AiSquadMember {
    /// The squad entity.
    pub squad: Entity,
    /// Higher ranks lead first (the module's leader rule).
    pub rank: i32,
    /// The role this member holds, kept by the squad.
    pub role: Option<String>,
    /// The squad it really joined. To change squads, insert a new value; a write to `squad` is not a move.
    #[reflect(ignore)]
    joined: Option<Entity>,
}

impl Default for AiSquadMember {
    fn default() -> Self {
        Self::new(Entity::PLACEHOLDER)
    }
}

impl AiSquadMember {
    /// A member of `squad` with rank 0.
    pub fn new(squad: Entity) -> Self {
        Self {
            squad,
            rank: 0,
            role: None,
            joined: None,
        }
    }

    /// Sets the rank.
    pub fn with_rank(mut self, rank: i32) -> Self {
        self.rank = rank;
        self
    }
}

/// Marks an agent dead while it stays in the world (a corpse, a ragdoll). Inserting it does at once what a
/// despawn would: the agent leaves its squad and frees its attack tokens, and a pool on it empties.
/// Removing it (a revive) puts an [`AiSquadMember`] back in its squad.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
#[component(on_add = agent_died, on_remove = agent_revived)]
pub struct AiDead;

fn set_member_bool(world: &mut DeferredWorld<'_>, member: Entity, key: &str, value: bool) {
    let unchanged = world
        .get::<Blackboard>(member)
        .is_none_or(|board| board.get(key) == Some(&BbValue::Bool(value)));
    if !unchanged && let Some(mut board) = world.get_mut::<Blackboard>(member) {
        board.insert(key, value);
    }
}

fn publish_role(world: &mut DeferredWorld<'_>, member: Entity, role: Option<&str>) {
    if let Some(mut data) = world.get_mut::<AiSquadMember>(member) {
        let current = data.role.as_deref();
        if current != role {
            data.role = role.map(str::to_string);
        }
    }
    let wanted = role.map(|r| BbValue::String(r.to_string()));
    let unchanged = world
        .get::<Blackboard>(member)
        .is_none_or(|board| board.get(SQUAD_ROLE_KEY) == wanted.as_ref());
    if !unchanged && let Some(mut board) = world.get_mut::<Blackboard>(member) {
        match wanted {
            Some(value) => {
                board.insert(SQUAD_ROLE_KEY, value);
            }
            None => {
                board.remove(SQUAD_ROLE_KEY);
            }
        }
    }
}

/// The members in the leader rule's order: rank descending, then join order.
fn by_rule(world: &DeferredWorld<'_>, squad: &AiSquad) -> Vec<Entity> {
    let mut ordered: Vec<(i32, usize, Entity)> = squad
        .members
        .iter()
        .enumerate()
        .map(|(joined, &member)| {
            (
                world.get::<AiSquadMember>(member).map_or(i32::MIN, |m| m.rank),
                joined,
                member,
            )
        })
        .collect();
    ordered.sort_by_key(|(rank, joined, _)| (std::cmp::Reverse(*rank), *joined));
    ordered.into_iter().map(|(_, _, member)| member).collect()
}

/// Sets the leader to `leader` and publishes it.
fn install_leader(world: &mut DeferredWorld<'_>, squad_entity: Entity, leader: Option<Entity>) {
    let previous = world.get::<AiSquad>(squad_entity).and_then(|s| s.leader);
    if let Some(mut squad) = world.get_mut::<AiSquad>(squad_entity) {
        squad.leader = leader;
    }
    if let Some(mut board) = world.get_mut::<Blackboard>(squad_entity) {
        match leader {
            Some(entity) => {
                board.insert(SQUAD_LEADER_KEY, entity);
            }
            None => {
                board.remove(SQUAD_LEADER_KEY);
            }
        }
    }
    if let Some(previous) = previous
        && previous != leader.unwrap_or(Entity::PLACEHOLDER)
    {
        set_member_bool(world, previous, SQUAD_IS_LEADER_KEY, false);
    }
    if let Some(leader) = leader {
        set_member_bool(world, leader, SQUAD_IS_LEADER_KEY, true);
    }
}

/// Runs the leader rule.
fn elect(world: &mut DeferredWorld<'_>, squad_entity: Entity) {
    let Some(squad) = world.get::<AiSquad>(squad_entity) else {
        return;
    };
    let winner = by_rule(world, squad).first().copied();
    if let Some(mut squad) = world.get_mut::<AiSquad>(squad_entity) {
        squad.elections += 1;
    }
    install_leader(world, squad_entity, winner);
}

/// Applies the role rule: free `auto_fill` slots take roleless members.
fn fill_roles(world: &mut DeferredWorld<'_>, squad_entity: Entity) {
    let Some(squad) = world.get::<AiSquad>(squad_entity) else {
        return;
    };
    if !squad.roles.iter().any(|r| r.auto_fill) {
        return;
    }
    let candidates = by_rule(world, squad);
    let mut assigned: Vec<(usize, Entity)> = Vec::new();
    let mut free: Vec<Entity> = candidates
        .into_iter()
        .filter(|member| squad.roles.iter().all(|role| !role.holders.contains(member)))
        .collect();
    for (index, role) in squad.roles.iter().enumerate() {
        if !role.auto_fill {
            continue;
        }
        let room = (role.capacity as usize).saturating_sub(role.holders.len());
        let take = room.min(free.len());
        assigned.extend(free.drain(..take).map(|member| (index, member)));
    }
    if assigned.is_empty() {
        return;
    }
    for (index, member) in assigned {
        let mut name = None;
        if let Some(mut squad) = world.get_mut::<AiSquad>(squad_entity)
            && let Some(role) = squad.roles.get_mut(index)
        {
            role.holders.push(member);
            name = Some(role.name.clone());
        }
        publish_role(world, member, name.as_deref());
    }
}

fn join(world: &mut DeferredWorld<'_>, member: Entity) {
    let Some(squad_entity) = world.get::<AiSquadMember>(member).map(|m| m.squad) else {
        return;
    };
    if world.get::<AiDead>(member).is_some() {
        return;
    }
    let Some(mut squad) = world.get_mut::<AiSquad>(squad_entity) else {
        tracing::warn!(
            target: "regia_ai",
            "entity {member} joined a squad entity with no AiSquad ({squad_entity}); it has no squad"
        );
        return;
    };
    if !squad.members.contains(&member) {
        squad.members.push(member);
    }
    let needs_leader = squad.leader.is_none();
    if let Some(mut data) = world.get_mut::<AiSquadMember>(member) {
        data.joined = Some(squad_entity);
    }
    if needs_leader {
        elect(world, squad_entity);
    } else {
        set_member_bool(world, member, SQUAD_IS_LEADER_KEY, false);
    }
    fill_roles(world, squad_entity);
    crate::formation::mark_dirty(world, squad_entity);
}

fn leave(world: &mut DeferredWorld<'_>, member: Entity, squad_entity: Entity) {
    let Some(mut squad) = world.get_mut::<AiSquad>(squad_entity) else {
        return;
    };
    let Some(position) = squad.members.iter().position(|m| *m == member) else {
        return;
    };
    squad.members.remove(position);
    for role in &mut squad.roles {
        role.holders.retain(|holder| *holder != member);
    }
    let was_leader = squad.leader == Some(member);
    if was_leader {
        squad.leader = None;
    }
    if let Some(mut data) = world.get_mut::<AiSquadMember>(member) {
        data.joined = None;
    }
    publish_role(world, member, None);
    set_member_bool(world, member, SQUAD_IS_LEADER_KEY, false);
    if was_leader {
        elect(world, squad_entity);
    }
    fill_roles(world, squad_entity);
    crate::formation::member_left(world, squad_entity, member);
}

fn member_inserted(mut world: DeferredWorld<'_>, context: HookContext) {
    join(&mut world, context.entity);
}

fn member_discarded(mut world: DeferredWorld<'_>, context: HookContext) {
    if let Some(squad) = world.get::<AiSquadMember>(context.entity).and_then(|m| m.joined) {
        leave(&mut world, context.entity, squad);
    }
}

fn squad_removed(mut world: DeferredWorld<'_>, context: HookContext) {
    let Some(squad) = world.get::<AiSquad>(context.entity) else {
        return;
    };
    let members = squad.members.clone();
    // What the members read about the squad goes now: once it is gone, `leave` has nothing to work with.
    for &member in &members {
        publish_role(&mut world, member, None);
        set_member_bool(&mut world, member, SQUAD_IS_LEADER_KEY, false);
        crate::formation::member_left(&mut world, context.entity, member);
    }
    let mut commands = world.commands();
    for member in members {
        if let Ok(mut entity) = commands.get_entity(member) {
            entity.try_remove::<AiSquadMember>();
        }
    }
}

fn agent_died(mut world: DeferredWorld<'_>, context: HookContext) {
    let entity = context.entity;
    if let Some(squad) = world.get::<AiSquadMember>(entity).and_then(|m| m.joined) {
        leave(&mut world, entity, squad);
    }
    release_all_on_death(&mut world, entity);
}

fn agent_revived(mut world: DeferredWorld<'_>, context: HookContext) {
    // Removal hooks run with the component still there, so `join` must not read it as dead: a despawn
    // of a dead member removes the membership too, which `leave` then finds has nothing to do.
    let entity = context.entity;
    if world.get::<AiSquadMember>(entity).is_none() {
        return;
    }
    let squad_entity = world.get::<AiSquadMember>(entity).map(|m| m.squad);
    if let Some(squad_entity) = squad_entity
        && world.get_entity(squad_entity).is_ok()
    {
        world.commands().queue(move |world: &mut World| {
            if world.get::<AiDead>(entity).is_none() {
                let mut deferred = DeferredWorld::from(world);
                join(&mut deferred, entity);
            }
        });
    }
}

/// Why a squad call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SquadError {
    /// The squad entity has no [`AiSquad`].
    #[error("the entity is not a squad")]
    NotASquad,
    /// The agent is not a member of that squad.
    #[error("the entity is not a member of the squad")]
    NotAMember,
    /// The squad has no role with that name.
    #[error("the squad has no role with that name")]
    UnknownRole,
    /// The role's slots are all taken.
    #[error("the role has no free slot")]
    RoleFull,
}

impl SquadError {
    /// The `data.reason` of an agent error.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NotASquad => "not_a_squad",
            Self::NotAMember => "not_a_member",
            Self::UnknownRole => "unknown_role",
            Self::RoleFull => "role_full",
        }
    }
}

/// Makes `member` the leader by hand. It leads until it leaves or dies; then the leader rule runs.
///
/// # Errors
///
/// [`SquadError::NotASquad`], [`SquadError::NotAMember`].
pub fn set_squad_leader(world: &mut World, squad_entity: Entity, member: Entity) -> Result<(), SquadError> {
    let squad = world.get::<AiSquad>(squad_entity).ok_or(SquadError::NotASquad)?;
    if !squad.members.contains(&member) {
        return Err(SquadError::NotAMember);
    }
    let mut deferred = DeferredWorld::from(world);
    install_leader(&mut deferred, squad_entity, Some(member));
    crate::formation::mark_dirty(&mut deferred, squad_entity);
    Ok(())
}

/// Adds the named role or changes its capacity and `auto_fill`. Lowering the capacity below the holders
/// takes the role from the latest to get it; free `auto_fill` slots then fill by the module's rule.
///
/// # Errors
///
/// [`SquadError::NotASquad`].
pub fn set_squad_role(
    world: &mut World,
    squad_entity: Entity,
    name: &str,
    capacity: u32,
    auto_fill: bool,
) -> Result<(), SquadError> {
    let mut squad = world
        .get_mut::<AiSquad>(squad_entity)
        .ok_or(SquadError::NotASquad)?;
    let index = match squad.roles.iter().position(|role| role.name == name) {
        Some(index) => index,
        None => {
            squad.roles.push(SquadRole::new(name, capacity));
            squad.roles.len() - 1
        }
    };
    let role = &mut squad.roles[index];
    role.capacity = capacity;
    role.auto_fill = auto_fill;
    let evicted = role.holders.split_off(role.holders.len().min(capacity as usize));
    let mut deferred = DeferredWorld::from(world);
    for member in evicted {
        publish_role(&mut deferred, member, None);
    }
    fill_roles(&mut deferred, squad_entity);
    Ok(())
}

/// Gives `member` the named role, leaving any role it holds.
///
/// # Errors
///
/// See [`SquadError`]. Nothing changes on an error.
pub fn claim_squad_role(world: &mut World, member: Entity, role: &str) -> Result<(), SquadError> {
    let squad_entity = world
        .get::<AiSquadMember>(member)
        .and_then(|m| m.joined)
        .ok_or(SquadError::NotAMember)?;
    let mut squad = world
        .get_mut::<AiSquad>(squad_entity)
        .ok_or(SquadError::NotASquad)?;
    if !squad.members.contains(&member) {
        return Err(SquadError::NotAMember);
    }
    let index = squad
        .roles
        .iter()
        .position(|r| r.name == role)
        .ok_or(SquadError::UnknownRole)?;
    if squad.roles[index].holders.contains(&member) {
        return Ok(());
    }
    if squad.roles[index].holders.len() >= squad.roles[index].capacity as usize {
        return Err(SquadError::RoleFull);
    }
    for other in &mut squad.roles {
        other.holders.retain(|holder| *holder != member);
    }
    squad.roles[index].holders.push(member);
    let mut deferred = DeferredWorld::from(world);
    publish_role(&mut deferred, member, Some(role));
    fill_roles(&mut deferred, squad_entity);
    Ok(())
}

/// Takes `member`'s role away. An `auto_fill` slot it frees goes straight to the first roleless member by
/// the leader rule, which may be `member` itself, so turn `auto_fill` off for a role claimed by hand.
///
/// # Errors
///
/// [`SquadError::NotAMember`], [`SquadError::NotASquad`].
pub fn release_squad_role(world: &mut World, member: Entity) -> Result<(), SquadError> {
    let squad_entity = world
        .get::<AiSquadMember>(member)
        .and_then(|m| m.joined)
        .ok_or(SquadError::NotAMember)?;
    let mut squad = world
        .get_mut::<AiSquad>(squad_entity)
        .ok_or(SquadError::NotASquad)?;
    for role in &mut squad.roles {
        role.holders.retain(|holder| *holder != member);
    }
    let mut deferred = DeferredWorld::from(world);
    publish_role(&mut deferred, member, None);
    fill_roles(&mut deferred, squad_entity);
    Ok(())
}

/// The squad `agent` is in now, if any.
pub fn squad_of(world: &World, agent: Entity) -> Option<Entity> {
    let squad = world.get::<AiSquadMember>(agent)?.joined?;
    world
        .get::<AiSquad>(squad)
        .is_some_and(|s| s.members.contains(&agent))
        .then_some(squad)
}

/// A leaf that copies a key from the squad's blackboard to the agent's. Fails when the agent has no
/// squad or the key is absent. `regia_ai::SquadRead` reads `squad_shared` into `squad_shared`;
/// [`SquadRead::register`] configures others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquadRead {
    /// The squad's key.
    pub key: String,
    /// The agent's key.
    pub into: String,
}

impl Default for SquadRead {
    fn default() -> Self {
        Self::new("squad_shared", "squad_shared")
    }
}

impl SquadRead {
    /// Copies `key` on the squad's blackboard to `into` on the agent's.
    pub fn new(key: impl Into<String>, into: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            into: into.into(),
        }
    }

    /// Registers this configured leaf in `registry` under `task_path`, the name a tree's leaf uses.
    pub fn register(self, registry: &mut BtTaskRegistry, task_path: impl Into<String>) {
        registry.register_factory(task_path, move || Box::new(self.clone()));
    }
}

impl BtTask for SquadRead {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let value = squad_of(ctx.world, ctx.entity)
            .and_then(|squad| ctx.world.get::<Blackboard>(squad))
            .and_then(|board| board.get(&self.key))
            .cloned();
        let Some(value) = value else {
            return BtStatus::Failure;
        };
        let Some(mut board) = ctx.blackboard_mut() else {
            return BtStatus::Failure;
        };
        board.insert(self.into.clone(), value);
        BtStatus::Success
    }
}

impl BtTaskRegistration for SquadRead {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Self::default())
    }
    fn task_type_path() -> &'static str {
        "regia_ai::SquadRead"
    }
}

/// A leaf that copies a key from the agent's blackboard to the squad's. Fails when the agent has no
/// squad or the key is absent. `regia_ai::SquadWrite` writes `squad_shared` to `squad_shared`;
/// [`SquadWrite::register`] configures others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquadWrite {
    /// The agent's key.
    pub from: String,
    /// The squad's key.
    pub key: String,
}

impl Default for SquadWrite {
    fn default() -> Self {
        Self::new("squad_shared", "squad_shared")
    }
}

impl SquadWrite {
    /// Copies `from` on the agent's blackboard to `key` on the squad's.
    pub fn new(from: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            key: key.into(),
        }
    }

    /// Registers this configured leaf in `registry` under `task_path`, the name a tree's leaf uses.
    pub fn register(self, registry: &mut BtTaskRegistry, task_path: impl Into<String>) {
        registry.register_factory(task_path, move || Box::new(self.clone()));
    }
}

impl BtTask for SquadWrite {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let Some(squad) = squad_of(ctx.world, ctx.entity) else {
            return BtStatus::Failure;
        };
        let Some(value) = ctx.blackboard().and_then(|board| board.get(&self.from)).cloned() else {
            return BtStatus::Failure;
        };
        let Some(mut board) = ctx.world.get_mut::<Blackboard>(squad) else {
            return BtStatus::Failure;
        };
        board.insert(self.key.clone(), value);
        BtStatus::Success
    }
}

impl BtTaskRegistration for SquadWrite {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Self::default())
    }
    fn task_type_path() -> &'static str {
        "regia_ai::SquadWrite"
    }
}

/// The squad step: after physics, before [`crate::AiStep`], so trees read this step's formation slots.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AiSquadStep;

/// The fixed steps [`AiSquadPlugin`] has run. Attack-token waiters age by it.
#[derive(Resource, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Resource, Default)]
pub struct AiSquadClock {
    /// Fixed steps so far.
    pub step: u64,
}

/// Installs squads, attack tokens and formations: the clock, the leaves
/// (`regia_ai::AcquireAttackToken`, `regia_ai::AcquireAttackTokenScoped`,
/// `regia_ai::ReleaseAttackToken`, `regia_ai::HasAttackToken`, `regia_ai::SquadRead`,
/// `regia_ai::SquadWrite`) and the formation step. Not part of [`crate::AiPlugin`], so a game that does
/// not ask for it keeps the schedule it had.
#[derive(Default)]
pub struct AiSquadPlugin;

impl Plugin for AiSquadPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AiSquadClock>()
            .init_resource::<BtTaskRegistry>()
            .register_type::<AiSquad>()
            .register_type::<SquadRole>()
            .register_type::<AiSquadMember>()
            .register_type::<AiDead>()
            .register_type::<AiSquadClock>()
            .register_type::<crate::attack_tokens::AiAttackTokens>()
            .register_type::<crate::attack_tokens::AiTokenHolder>()
            .register_type::<crate::attack_tokens::TokenHold>()
            .register_type::<crate::attack_tokens::TokenWaiter>()
            .register_type::<AiFormation>()
            .register_type::<crate::formation::FormationAnchor>()
            .register_type::<crate::formation::FormationShape>()
            .register_type::<crate::formation::FormationSlot>()
            .configure_sets(
                FixedUpdate,
                AiSquadStep
                    .after(regia_physics::PhysicsStep)
                    .before(crate::AiStep),
            )
            .add_systems(FixedUpdate, crate::formation::step_squads.in_set(AiSquadStep));
        let mut registry = app.world_mut().resource_mut::<BtTaskRegistry>();
        registry.register::<crate::attack_tokens::AcquireAttackToken>();
        crate::attack_tokens::AcquireAttackToken::scoped()
            .register(&mut registry, "regia_ai::AcquireAttackTokenScoped");
        registry.register::<crate::attack_tokens::ReleaseAttackToken>();
        registry.register::<crate::attack_tokens::HasAttackToken>();
        registry.register::<SquadRead>();
        registry.register::<SquadWrite>();
    }
}
