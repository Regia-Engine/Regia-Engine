// SPDX-License-Identifier: MPL-2.0
//! Attack tokens (ADR-2709, `book/agents/ai-squads.md`): a capped pool on a target, so that of everyone
//! who wants to attack it only so many do at once and the rest do something else.
//!
//! The target carries [`AiAttackTokens`]. An agent asks with [`acquire_token`] (or the
//! [`AcquireAttackToken`] leaf) and gives the token back with [`release_token`] (or
//! [`ReleaseAttackToken`]). A holder that dies ([`crate::squad::AiDead`]) or is despawned frees its tokens
//! at once through a component hook, and a target that dies or is despawned frees every holder.
//!
//! **Who gets a token.** An agent refused a token is remembered as a *waiter* until it stops asking
//! (`waiter_ttl_steps` steps with no request) or cancels. A free token goes to the waiters by priority,
//! highest first, then in the order they first asked; someone asking out of turn is refused and queues.
//! So an agent that just gave a token back and asks again waits behind everyone already waiting, and
//! equal-priority agents take turns instead of the first in tick order holding on forever.
//!
//! With [`AiAttackTokens::preempt`] a waiter first in line also takes a token from the lowest-priority
//! holder (the latest granted of those) when its own priority is strictly higher. The holder learns it
//! from [`HasAttackToken`] or [`holds_token`].
//!
//! Everything is a function of the order of the calls, with no clock but fixed steps
//! ([`AiSquadClock`], advanced by [`crate::squad::AiSquadPlugin`]; without the plugin waiters do not expire),
//! so two runs agree.
//!
//! A holder keeps its token while it lives, including while it is parked, and a tree removed from an agent
//! does not reset its leaves. Keep `waiter_ttl_steps` above the cadence of the agents that wait (with
//! [`crate::AiTickBudget`], `cadence + lateness`), or a slow agent loses its place at each expiry.

use regia_app::prelude::*;
use regia_ecs::ecs::lifecycle::HookContext;
use regia_ecs::ecs::world::DeferredWorld;

use crate::blackboard::BbValue;
use crate::squad::{AiDead, AiSquadClock};
use crate::task::{BtContext, BtStatus, BtTask, BtTaskRegistration};
use crate::tree::BtTaskRegistry;

/// One held token.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
#[reflect(Default)]
pub struct TokenHold {
    /// The agent holding it.
    pub holder: Entity,
    /// The priority it was granted at.
    pub priority: i32,
    /// When it was granted, as this pool's request counter.
    pub granted: u64,
}

impl Default for TokenHold {
    fn default() -> Self {
        Self {
            holder: Entity::PLACEHOLDER,
            priority: 0,
            granted: 0,
        }
    }
}

/// One agent waiting for a token.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq)]
#[reflect(Default)]
pub struct TokenWaiter {
    /// The agent.
    pub agent: Entity,
    /// The priority of its last request.
    pub priority: i32,
    /// When it first asked, as this pool's request counter.
    pub since: u64,
    /// The fixed step of its last request.
    pub last_request_step: u64,
}

impl Default for TokenWaiter {
    fn default() -> Self {
        Self {
            agent: Entity::PLACEHOLDER,
            priority: 0,
            since: 0,
            last_request_step: 0,
        }
    }
}

/// A target's pool of attack tokens. `holders` and `waiters` are kept by [`acquire_token`],
/// [`release_token`] and the hooks; lower the capacity with [`set_token_capacity`].
#[derive(Component, Reflect, Debug, Clone, PartialEq, Eq)]
#[reflect(Component, Default)]
#[component(on_remove = pool_removed)]
pub struct AiAttackTokens {
    /// How many agents may hold a token at once. Lower it with [`set_token_capacity`]: writing it directly
    /// leaves the extra holders until they release.
    pub capacity: u32,
    /// Whether a higher-priority request may take a lower-priority holder's token.
    pub preempt: bool,
    /// A waiter that has not asked for this many fixed steps stops waiting.
    pub waiter_ttl_steps: u32,
    /// The agents holding a token.
    pub holders: Vec<TokenHold>,
    /// The agents waiting for one.
    pub waiters: Vec<TokenWaiter>,
    /// Requests seen.
    pub requests: u64,
    /// Tokens granted.
    pub granted: u64,
    /// Requests refused.
    pub denied: u64,
    /// Tokens taken from a holder by a higher priority.
    pub preempted: u64,
}

impl Default for AiAttackTokens {
    fn default() -> Self {
        Self::new(1)
    }
}

impl AiAttackTokens {
    /// A pool of `capacity` tokens with no preemption; a waiter that stops asking is forgotten after 30
    /// fixed steps.
    pub fn new(capacity: u32) -> Self {
        Self {
            capacity,
            preempt: false,
            waiter_ttl_steps: 30,
            holders: Vec::new(),
            waiters: Vec::new(),
            requests: 0,
            granted: 0,
            denied: 0,
            preempted: 0,
        }
    }

    /// Turns preemption on.
    pub fn with_preemption(mut self) -> Self {
        self.preempt = true;
        self
    }

    /// Whether `agent` holds a token here.
    pub fn is_held_by(&self, agent: Entity) -> bool {
        self.holders.iter().any(|hold| hold.holder == agent)
    }
}

/// The pools an agent holds a token in. Added by [`acquire_token`]; its hook frees the tokens when the
/// agent goes away.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
#[component(on_discard = holder_discarded)]
pub struct AiTokenHolder {
    /// The targets this agent holds a token on.
    pub held: Vec<Entity>,
}

/// What a request got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenOutcome {
    /// A free token was granted.
    Granted,
    /// The agent already holds one here.
    AlreadyHeld,
    /// A token was taken from the named lower-priority holder.
    Preempted(Entity),
    /// Refused for now: the agent is waiting.
    Denied,
    /// The target has no pool, or is dead or gone.
    NoPool,
    /// The agent is dead or gone.
    AgentGone,
}

impl TokenOutcome {
    /// Whether the agent holds a token after the request.
    pub fn is_held(self) -> bool {
        matches!(self, Self::Granted | Self::AlreadyHeld | Self::Preempted(_))
    }
}

fn pool_removed(mut world: DeferredWorld<'_>, context: HookContext) {
    let target = context.entity;
    let holders: Vec<Entity> = world
        .get::<AiAttackTokens>(target)
        .map(|pool| pool.holders.iter().map(|hold| hold.holder).collect())
        .unwrap_or_default();
    for holder in holders {
        if let Some(mut held) = world.get_mut::<AiTokenHolder>(holder) {
            held.held.retain(|t| *t != target);
        }
    }
}

fn drop_hold(world: &mut DeferredWorld<'_>, holder: Entity, target: Entity) {
    if let Some(mut pool) = world.get_mut::<AiAttackTokens>(target) {
        pool.holders.retain(|hold| hold.holder != holder);
    }
}

fn holder_discarded(mut world: DeferredWorld<'_>, context: HookContext) {
    let holder = context.entity;
    let targets = world
        .get::<AiTokenHolder>(holder)
        .map(|held| held.held.clone())
        .unwrap_or_default();
    for target in targets {
        drop_hold(&mut world, holder, target);
    }
}

/// An agent died: its tokens go back, it stops waiting, and a pool on it empties.
pub(crate) fn release_all_on_death(world: &mut DeferredWorld<'_>, agent: Entity) {
    let targets = world
        .get::<AiTokenHolder>(agent)
        .map(|held| held.held.clone())
        .unwrap_or_default();
    for target in targets {
        drop_hold(world, agent, target);
    }
    if let Some(mut held) = world.get_mut::<AiTokenHolder>(agent) {
        held.held.clear();
    }
    let holders: Vec<Entity> = world
        .get::<AiAttackTokens>(agent)
        .map(|pool| pool.holders.iter().map(|hold| hold.holder).collect())
        .unwrap_or_default();
    for holder in holders {
        if let Some(mut held) = world.get_mut::<AiTokenHolder>(holder) {
            held.held.retain(|t| *t != agent);
        }
    }
    if let Some(mut pool) = world.get_mut::<AiAttackTokens>(agent) {
        pool.holders.clear();
        pool.waiters.clear();
    }
}

/// Asks for a token on `target` at `priority`. Refused, the agent queues as a waiter (the module docs).
pub fn acquire_token(world: &mut World, agent: Entity, target: Entity, priority: i32) -> TokenOutcome {
    if world.get_entity(agent).is_err() || world.get::<AiDead>(agent).is_some() {
        return TokenOutcome::AgentGone;
    }
    if world.get::<AiDead>(target).is_some() || world.get::<AiAttackTokens>(target).is_none() {
        return TokenOutcome::NoPool;
    }
    let step = world.get_resource::<AiSquadClock>().map_or(0, |clock| clock.step);
    // Who is still waiting: alive, not dead, and asked recently enough.
    let mut waiters = world
        .get::<AiAttackTokens>(target)
        .map(|pool| pool.waiters.clone())
        .unwrap_or_default();
    let ttl = world
        .get::<AiAttackTokens>(target)
        .map_or(0, |pool| u64::from(pool.waiter_ttl_steps));
    waiters.retain(|w| {
        world.get_entity(w.agent).is_ok()
            && world.get::<AiDead>(w.agent).is_none()
            && step.saturating_sub(w.last_request_step) <= ttl
    });
    let Some(mut pool) = world.get_mut::<AiAttackTokens>(target) else {
        return TokenOutcome::NoPool;
    };
    pool.requests += 1;
    let request = pool.requests;
    if pool.is_held_by(agent) {
        pool.waiters = waiters;
        pool.waiters.retain(|w| w.agent != agent);
        return TokenOutcome::AlreadyHeld;
    }
    let since = waiters
        .iter()
        .find(|w| w.agent == agent)
        .map_or(request, |w| w.since);
    // The agent's place in line: how many others come first by priority, then by first request.
    let ahead = waiters
        .iter()
        .filter(|w| w.agent != agent)
        .filter(|w| (std::cmp::Reverse(w.priority), w.since) < (std::cmp::Reverse(priority), since))
        .count();
    let free = (pool.capacity as usize).saturating_sub(pool.holders.len());
    let mut victim = None;
    let granted = if ahead < free {
        true
    } else if pool.preempt && ahead == 0 && free == 0 {
        victim = pool
            .holders
            .iter()
            .filter(|hold| hold.priority < priority)
            .min_by_key(|hold| (hold.priority, std::cmp::Reverse(hold.granted)))
            .map(|hold| hold.holder);
        victim.is_some()
    } else {
        false
    };
    if !granted {
        match waiters.iter_mut().find(|w| w.agent == agent) {
            Some(waiter) => {
                waiter.priority = priority;
                waiter.last_request_step = step;
            }
            None => waiters.push(TokenWaiter {
                agent,
                priority,
                since,
                last_request_step: step,
            }),
        }
        pool.waiters = waiters;
        pool.denied += 1;
        return TokenOutcome::Denied;
    }
    waiters.retain(|w| w.agent != agent);
    pool.waiters = waiters;
    if let Some(victim) = victim {
        pool.holders.retain(|hold| hold.holder != victim);
        pool.preempted += 1;
    }
    pool.holders.push(TokenHold {
        holder: agent,
        priority,
        granted: request,
    });
    pool.granted += 1;
    if let Some(victim) = victim
        && let Some(mut held) = world.get_mut::<AiTokenHolder>(victim)
    {
        held.held.retain(|t| *t != target);
    }
    match world.get_mut::<AiTokenHolder>(agent) {
        Some(mut held) => {
            if !held.held.contains(&target) {
                held.held.push(target);
            }
        }
        None => {
            world
                .entity_mut(agent)
                .insert(AiTokenHolder { held: vec![target] });
        }
    }
    victim.map_or(TokenOutcome::Granted, TokenOutcome::Preempted)
}

/// Gives a token back, or stops waiting for one. Returns whether the agent held one.
pub fn release_token(world: &mut World, agent: Entity, target: Entity) -> bool {
    let mut held = false;
    if let Some(mut pool) = world.get_mut::<AiAttackTokens>(target) {
        held = pool.is_held_by(agent);
        if held {
            pool.holders.retain(|hold| hold.holder != agent);
        }
        if pool.waiters.iter().any(|w| w.agent == agent) {
            pool.waiters.retain(|w| w.agent != agent);
        }
    }
    if let Some(mut tokens) = world.get_mut::<AiTokenHolder>(agent) {
        tokens.held.retain(|t| *t != target);
    }
    held
}

/// Gives back every token `agent` holds. Returns how many.
pub fn release_all_tokens(world: &mut World, agent: Entity) -> usize {
    let targets = world
        .get::<AiTokenHolder>(agent)
        .map(|held| held.held.clone())
        .unwrap_or_default();
    targets
        .into_iter()
        .filter(|target| release_token(world, agent, *target))
        .count()
}

/// Whether `agent` holds a token on `target`.
pub fn holds_token(world: &World, agent: Entity, target: Entity) -> bool {
    world
        .get::<AiAttackTokens>(target)
        .is_some_and(|pool| pool.is_held_by(agent))
}

/// Sets a pool's capacity. Lowering it below the holders takes tokens from the lowest priorities, the
/// latest granted first. Returns the agents that lost one, or `None` when `target` has no pool.
pub fn set_token_capacity(world: &mut World, target: Entity, capacity: u32) -> Option<Vec<Entity>> {
    let mut pool = world.get_mut::<AiAttackTokens>(target)?;
    pool.capacity = capacity;
    let mut evicted = Vec::new();
    while pool.holders.len() > capacity as usize {
        let Some((index, _)) = pool
            .holders
            .iter()
            .enumerate()
            .min_by_key(|(_, hold)| (hold.priority, std::cmp::Reverse(hold.granted)))
        else {
            break;
        };
        evicted.push(pool.holders.remove(index).holder);
    }
    for agent in &evicted {
        if let Some(mut held) = world.get_mut::<AiTokenHolder>(*agent) {
            held.held.retain(|t| *t != target);
        }
    }
    Some(evicted)
}

/// An entity named by a blackboard key on the agent's board.
fn target_of(ctx: &BtContext<'_>, key: &str) -> Option<Entity> {
    match ctx.blackboard()?.get(key)? {
        BbValue::Entity(bits) => Entity::try_from_bits(*bits),
        _ => None,
    }
}

/// A leaf that asks for a token on the entity in blackboard key `target_key`. `Success` once held. Refused,
/// it returns `Running` with `wait` and `Failure` without, and stays in line either way while the tree
/// keeps asking. `Failure` also for no target, no pool, or a dead agent.
///
/// `regia_ai::AcquireAttackToken` reads `target`, waits, and keeps the token until something releases it,
/// so a state may hand it on to the next state. `regia_ai::AcquireAttackTokenScoped` also gives it back
/// when the node is reset: a `Timeout` that fires, the end of a `Parallel`, a new `Repeat` iteration, or
/// leaving the state it runs in. A plain `Sequence` that fails does not reset its children, so neither
/// leaf lets go then: give a tree a [`ReleaseAttackToken`] on its way out.
/// [`AcquireAttackToken::register`] configures others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireAttackToken {
    /// The blackboard key holding the target entity.
    pub target_key: String,
    /// The request's priority.
    pub priority: i32,
    /// A blackboard key whose number overrides `priority`, when present.
    pub priority_key: Option<String>,
    /// Return `Running` while refused, instead of `Failure`.
    pub wait: bool,
    /// Give the token back when the node resets.
    pub release_on_reset: bool,
    asked: Option<Entity>,
}

impl Default for AcquireAttackToken {
    fn default() -> Self {
        Self {
            target_key: "target".to_string(),
            priority: 0,
            priority_key: None,
            wait: true,
            release_on_reset: false,
            asked: None,
        }
    }
}

impl AcquireAttackToken {
    /// The default leaf, also giving the token back when the node resets.
    pub fn scoped() -> Self {
        Self {
            release_on_reset: true,
            ..Self::default()
        }
    }

    /// Reads the target from this blackboard key.
    pub fn with_target_key(mut self, key: impl Into<String>) -> Self {
        self.target_key = key.into();
        self
    }

    /// Asks at this priority.
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// Reads the priority from this blackboard key when it holds a number.
    pub fn with_priority_key(mut self, key: impl Into<String>) -> Self {
        self.priority_key = Some(key.into());
        self
    }

    /// Whether a refused request returns `Running` (`true`, the default) or `Failure`.
    pub fn with_wait(mut self, wait: bool) -> Self {
        self.wait = wait;
        self
    }

    /// Registers this configured leaf in `registry` under `task_path`, the name a tree's leaf uses.
    pub fn register(self, registry: &mut BtTaskRegistry, task_path: impl Into<String>) {
        registry.register_factory(task_path, move || Box::new(self.clone()));
    }
}

impl BtTask for AcquireAttackToken {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let Some(target) = target_of(ctx, &self.target_key) else {
            return BtStatus::Failure;
        };
        let priority = self
            .priority_key
            .as_ref()
            .and_then(|key| match ctx.blackboard()?.get(key)? {
                BbValue::Int(v) => i32::try_from(*v).ok(),
                BbValue::Float(v) => Some(*v as i32),
                _ => None,
            })
            .unwrap_or(self.priority);
        // Asking about another target ends the request on the one before, and a scoped leaf's token.
        if let Some(previous) = self.asked.filter(|previous| *previous != target)
            && (self.release_on_reset || !holds_token(ctx.world, ctx.entity, previous))
        {
            release_token(ctx.world, ctx.entity, previous);
        }
        self.asked = Some(target);
        match acquire_token(ctx.world, ctx.entity, target, priority) {
            outcome if outcome.is_held() => BtStatus::Success,
            TokenOutcome::Denied if self.wait => BtStatus::Running,
            _ => BtStatus::Failure,
        }
    }

    fn reset(&mut self, ctx: &mut BtContext<'_>) {
        let Some(target) = self.asked.take() else {
            return;
        };
        // A token already held stays unless this leaf is scoped; a request still in line always ends.
        if self.release_on_reset || !holds_token(ctx.world, ctx.entity, target) {
            release_token(ctx.world, ctx.entity, target);
        }
    }
}

impl BtTaskRegistration for AcquireAttackToken {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Self::default())
    }
    fn task_type_path() -> &'static str {
        "regia_ai::AcquireAttackToken"
    }
}

/// A leaf that gives a token back: on the entity in `target_key`, or with `None` every token the agent
/// holds. Always `Success`, so a tree can run it on any path out of an attack.
/// `regia_ai::ReleaseAttackToken` gives back every token; [`ReleaseAttackToken::register`] configures
/// others.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseAttackToken {
    /// The blackboard key holding the target entity; `None` releases everything.
    pub target_key: Option<String>,
}

impl ReleaseAttackToken {
    /// Registers this configured leaf in `registry` under `task_path`, the name a tree's leaf uses.
    pub fn register(self, registry: &mut BtTaskRegistry, task_path: impl Into<String>) {
        registry.register_factory(task_path, move || Box::new(self.clone()));
    }
}

impl BtTask for ReleaseAttackToken {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        match &self.target_key {
            Some(key) => {
                if let Some(target) = target_of(ctx, key) {
                    release_token(ctx.world, ctx.entity, target);
                }
            }
            None => {
                release_all_tokens(ctx.world, ctx.entity);
            }
        }
        BtStatus::Success
    }
}

impl BtTaskRegistration for ReleaseAttackToken {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Self::default())
    }
    fn task_type_path() -> &'static str {
        "regia_ai::ReleaseAttackToken"
    }
}

/// A condition leaf: `Success` while the agent holds a token on the entity in `target_key`, so a behavior
/// tree can notice a preemption mid-attack. A state tree does not tick a finished task again, so there it
/// answers once, on entering the state. `regia_ai::HasAttackToken` reads `target`;
/// [`HasAttackToken::register`] configures others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HasAttackToken {
    /// The blackboard key holding the target entity.
    pub target_key: String,
}

impl Default for HasAttackToken {
    fn default() -> Self {
        Self {
            target_key: "target".to_string(),
        }
    }
}

impl HasAttackToken {
    /// Registers this configured leaf in `registry` under `task_path`, the name a tree's leaf uses.
    pub fn register(self, registry: &mut BtTaskRegistry, task_path: impl Into<String>) {
        registry.register_factory(task_path, move || Box::new(self.clone()));
    }
}

impl BtTask for HasAttackToken {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        match target_of(ctx, &self.target_key) {
            Some(target) if holds_token(ctx.world, ctx.entity, target) => BtStatus::Success,
            _ => BtStatus::Failure,
        }
    }
}

impl BtTaskRegistration for HasAttackToken {
    fn construct() -> Box<dyn BtTask> {
        Box::new(Self::default())
    }
    fn task_type_path() -> &'static str {
        "regia_ai::HasAttackToken"
    }
}
