// SPDX-License-Identifier: MPL-2.0
//! A small, deliberately separate queue model for composed attack-token operations.

use regia_ai::attack_tokens::{
    AiAttackTokens, TokenOutcome, acquire_token, holds_token, release_token, set_token_capacity,
};
use regia_ai::squad::{AiDead, AiSquadClock};
use regia_app::prelude::*;

#[derive(Clone, Copy, Debug)]
enum Op {
    Ask(usize, usize, i32),
    Release(usize, usize),
    Step(u64),
    Capacity(usize, u32),
    Death(usize),
    RemoveTarget(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Hold {
    agent: usize,
    priority: i32,
    granted: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Wait {
    agent: usize,
    priority: i32,
    since: u64,
    last_step: u64,
}

#[derive(Debug)]
struct PoolModel {
    capacity: u32,
    preempt: bool,
    ttl: u64,
    holders: Vec<Hold>,
    waiters: Vec<Wait>,
    requests: u64,
    granted: u64,
    denied: u64,
    preempted: u64,
}

impl PoolModel {
    fn new(capacity: u32, preempt: bool, ttl: u64) -> Self {
        Self {
            capacity,
            preempt,
            ttl,
            holders: Vec::new(),
            waiters: Vec::new(),
            requests: 0,
            granted: 0,
            denied: 0,
            preempted: 0,
        }
    }

    fn ask(&mut self, agent: usize, priority: i32, step: u64, entities: &[Entity]) -> TokenOutcome {
        self.waiters
            .retain(|wait| step.saturating_sub(wait.last_step) <= self.ttl);
        self.requests += 1;
        if self.holders.iter().any(|hold| hold.agent == agent) {
            self.waiters.retain(|wait| wait.agent != agent);
            return TokenOutcome::AlreadyHeld;
        }

        let since = self
            .waiters
            .iter()
            .find(|wait| wait.agent == agent)
            .map_or(self.requests, |wait| wait.since);
        let ahead = self
            .waiters
            .iter()
            .filter(|wait| wait.agent != agent)
            .filter(|wait| wait.priority > priority || (wait.priority == priority && wait.since < since))
            .count();
        let free = (self.capacity as usize).saturating_sub(self.holders.len());
        let victim = if ahead == 0 && free == 0 && self.preempt {
            self.holders
                .iter()
                .filter(|hold| hold.priority < priority)
                .min_by_key(|hold| (hold.priority, std::cmp::Reverse(hold.granted)))
                .copied()
        } else {
            None
        };
        if ahead >= free && victim.is_none() {
            if let Some(wait) = self.waiters.iter_mut().find(|wait| wait.agent == agent) {
                wait.priority = priority;
                wait.last_step = step;
            } else {
                self.waiters.push(Wait {
                    agent,
                    priority,
                    since,
                    last_step: step,
                });
            }
            self.denied += 1;
            return TokenOutcome::Denied;
        }

        self.waiters.retain(|wait| wait.agent != agent);
        let mut removed = None;
        if let Some(victim) = victim {
            self.holders.retain(|hold| {
                let keep = hold.agent != victim.agent;
                if !keep {
                    removed = Some(victim.agent);
                }
                keep
            });
            self.preempted += 1;
        }
        self.holders.push(Hold {
            agent,
            priority,
            granted: self.requests,
        });
        self.granted += 1;
        removed.map_or(TokenOutcome::Granted, |old| {
            TokenOutcome::Preempted(entities[old])
        })
    }

    fn remove_agent(&mut self, agent: usize) {
        self.holders.retain(|hold| hold.agent != agent);
        self.waiters.retain(|wait| wait.agent != agent);
    }

    fn resize(&mut self, capacity: u32) -> Vec<usize> {
        self.capacity = capacity;
        let mut evicted = Vec::new();
        while self.holders.len() > capacity as usize {
            let victim = self
                .holders
                .iter()
                .enumerate()
                .min_by_key(|(_, hold)| (hold.priority, std::cmp::Reverse(hold.granted)))
                .map(|(index, _)| index);
            if let Some(index) = victim {
                evicted.push(self.holders.remove(index).agent);
            }
        }
        evicted
    }
}

fn collect_orders(values: &mut [usize; 4], start: usize, orders: &mut Vec<[usize; 4]>) {
    if start == values.len() {
        orders.push(*values);
        return;
    }
    for index in start..values.len() {
        values.swap(start, index);
        collect_orders(values, start + 1, orders);
        values.swap(start, index);
    }
}

#[test]
fn bounded_composed_histories_match_the_queue_model() {
    // Enumerate every ordering of four equal-priority requests, then compose each with
    // repeats, release/cancel, fixed-step expiry, capacity changes, death and removal.
    let mut orders = Vec::new();
    collect_orders(&mut [0, 1, 2, 3], 0, &mut orders);
    for (history_index, order) in orders.into_iter().enumerate() {
        let mut app = App::new();
        app.insert_resource(AiSquadClock::default());
        let targets: Vec<_> = (0..2)
            .map(|_| {
                app.world_mut()
                    .spawn(AiAttackTokens::new(2).with_preemption())
                    .id()
            })
            .collect();
        for target in &targets {
            let mut pool = app.world_mut().get_mut::<AiAttackTokens>(*target).expect("pool");
            pool.waiter_ttl_steps = 2;
        }
        let agents: Vec<_> = (0..4).map(|_| app.world_mut().spawn_empty().id()).collect();
        let mut models = [PoolModel::new(2, true, 2), PoolModel::new(2, true, 2)];
        let mut target_alive = [true, true];
        let history = [
            Op::Ask(order[0], 0, 3),
            Op::Ask(order[1], 0, 3),
            Op::Ask(order[2], 0, 3),
            Op::Ask(order[3], 0, 3),
            Op::Ask(order[0], 1, 2),
            Op::Ask(order[0], 0, 3),
            Op::Release(order[1], 0),
            Op::Ask(order[3], 0, 3),
            Op::Ask(order[2], 0, 3),
            Op::Step(1),
            Op::Ask(order[3], 0, 4),
            Op::Capacity(0, 1),
            Op::Release(order[3], 0),
            Op::Step(3),
            Op::Ask(order[1], 0, 0),
            Op::Capacity(0, 0),
            Op::Capacity(0, 2),
            Op::Ask(order[3], 0, 1),
            Op::Death(order[2]),
            Op::RemoveTarget(1),
        ];
        for (index, op) in history.into_iter().enumerate() {
            match op {
                Op::Ask(agent, target, priority) => {
                    let expected = models[target].ask(
                        agent,
                        priority,
                        app.world().resource::<AiSquadClock>().step,
                        &agents,
                    );
                    let actual = acquire_token(app.world_mut(), agents[agent], targets[target], priority);
                    assert_eq!(
                        actual, expected,
                        "history {history_index}, operation {index}: {op:?}"
                    );
                }
                Op::Release(agent, target) => {
                    let expected = models[target].holders.iter().any(|hold| hold.agent == agent);
                    models[target].remove_agent(agent);
                    assert_eq!(
                        release_token(app.world_mut(), agents[agent], targets[target]),
                        expected,
                        "history {history_index}, operation {index}: {op:?}"
                    );
                }
                Op::Step(by) => {
                    app.world_mut().resource_mut::<AiSquadClock>().step += by;
                }
                Op::Capacity(target, capacity) => {
                    let expected = models[target].resize(capacity);
                    let actual =
                        set_token_capacity(app.world_mut(), targets[target], capacity).expect("target pool");
                    assert_eq!(
                        actual,
                        expected.iter().map(|agent| agents[*agent]).collect::<Vec<_>>(),
                        "history {history_index}, operation {index}: {op:?}"
                    );
                }
                Op::Death(agent) => {
                    app.world_mut().entity_mut(agents[agent]).insert(AiDead);
                    for model in &mut models {
                        model.remove_agent(agent);
                    }
                }
                Op::RemoveTarget(target) => {
                    app.world_mut().despawn(targets[target]);
                    target_alive[target] = false;
                }
            }
            for (target_index, (target, model)) in targets.iter().zip(&models).enumerate() {
                if !target_alive[target_index] {
                    for entity in &agents {
                        assert!(!holds_token(app.world(), *entity, *target));
                    }
                    continue;
                }
                let actual = app.world().get::<AiAttackTokens>(*target).expect("pool");
                let holders: Vec<_> = actual.holders.iter().map(|hold| hold.holder).collect();
                let expected_holders: Vec<_> = model.holders.iter().map(|hold| agents[hold.agent]).collect();
                assert_eq!(
                    holders.len(),
                    holders.iter().collect::<std::collections::HashSet<_>>().len()
                );
                assert!(holders.len() <= actual.capacity as usize);
                assert_eq!(
                    holders, expected_holders,
                    "history {history_index}, operation {index}"
                );
                assert_eq!(actual.waiters.len(), model.waiters.len());
                for (actual, expected) in actual.waiters.iter().zip(&model.waiters) {
                    assert_eq!(actual.agent, agents[expected.agent]);
                    assert_eq!(actual.priority, expected.priority);
                    assert_eq!(actual.since, expected.since);
                    assert_eq!(actual.last_request_step, expected.last_step);
                }
                assert_eq!(actual.requests, model.requests);
                assert_eq!(actual.granted, model.granted);
                assert_eq!(actual.denied, model.denied);
                assert_eq!(actual.preempted, model.preempted);
                for (agent, entity) in agents.iter().enumerate() {
                    assert_eq!(
                        holds_token(app.world(), *entity, *target),
                        model.holders.iter().any(|h| h.agent == agent)
                    );
                }
                for (agent, entity) in agents.iter().enumerate() {
                    let expected = model.holders.iter().any(|hold| hold.agent == agent);
                    let lists_target = app
                        .world()
                        .get::<regia_ai::attack_tokens::AiTokenHolder>(*entity)
                        .is_some_and(|held| held.held.contains(target));
                    assert_eq!(lists_target, expected, "reverse holder link at {index}");
                }
            }
        }
    }
}

#[test]
fn dead_and_removed_entities_do_not_transfer_tokens_to_reused_slots() {
    let mut app = App::new();
    app.insert_resource(AiSquadClock::default());
    let target = app.world_mut().spawn(AiAttackTokens::new(1)).id();
    let old = app.world_mut().spawn_empty().id();
    assert_eq!(
        acquire_token(app.world_mut(), old, target, 0),
        TokenOutcome::Granted
    );
    app.world_mut().entity_mut(old).insert(AiDead);
    assert!(!holds_token(app.world(), old, target));

    let waiter = app.world_mut().spawn_empty().id();
    assert_eq!(
        acquire_token(app.world_mut(), waiter, target, 0),
        TokenOutcome::Granted
    );
    let stale = app.world_mut().spawn_empty().id();
    assert_eq!(
        acquire_token(app.world_mut(), stale, target, 7),
        TokenOutcome::Denied
    );
    let other_target = app.world_mut().spawn(AiAttackTokens::new(1)).id();
    assert_eq!(
        acquire_token(app.world_mut(), stale, other_target, 3),
        TokenOutcome::Granted
    );
    app.world_mut().despawn(stale);
    app.world_mut().flush();
    // Fill the ECS allocator's local free batch so freed indices become reusable.
    let fillers: Vec<_> = (0..1024).map(|_| app.world_mut().spawn_empty().id()).collect();
    for filler in fillers {
        app.world_mut().despawn(filler);
    }
    app.world_mut().flush();
    let reused = (0..1025)
        .map(|_| app.world_mut().spawn_empty().id())
        .find(|entity| entity.index() == stale.index())
        .expect("the allocator reuses a despawned index");
    assert_eq!(reused.index(), stale.index());
    assert_ne!(reused, stale);
    assert!(!holds_token(app.world(), reused, other_target));
    assert_eq!(
        acquire_token(app.world_mut(), reused, target, 0),
        TokenOutcome::Denied
    );
    let pool = app.world().get::<AiAttackTokens>(target).expect("pool");
    assert_eq!(pool.waiters.len(), 1);
    assert_eq!(pool.waiters[0].agent, reused);
}

#[test]
fn an_expired_waiter_rejoins_after_peers_who_kept_asking() {
    let mut app = App::new();
    app.insert_resource(AiSquadClock::default());
    let target = app.world_mut().spawn(AiAttackTokens::new(1)).id();
    app.world_mut()
        .get_mut::<AiAttackTokens>(target)
        .expect("pool")
        .waiter_ttl_steps = 2;
    let agents: Vec<_> = (0..3).map(|_| app.world_mut().spawn_empty().id()).collect();
    let mut model = PoolModel::new(1, false, 2);
    let request = |app: &mut App, model: &mut PoolModel, agent: usize| {
        let expected = model.ask(agent, 1, app.world().resource::<AiSquadClock>().step, &agents);
        let actual = acquire_token(app.world_mut(), agents[agent], target, 1);
        assert_eq!(
            actual,
            expected,
            "agent {agent} at step {}",
            app.world().resource::<AiSquadClock>().step
        );
    };

    request(&mut app, &mut model, 0);
    request(&mut app, &mut model, 1);
    app.world_mut().resource_mut::<AiSquadClock>().step += 1;
    request(&mut app, &mut model, 2);
    request(&mut app, &mut model, 2); // refresh the surviving peer's TTL
    app.world_mut().resource_mut::<AiSquadClock>().step += 2;
    request(&mut app, &mut model, 1); // expired, so it joins behind agent 2

    let pool = app.world().get::<AiAttackTokens>(target).expect("pool");
    assert_eq!(
        pool.waiters.iter().map(|wait| wait.agent).collect::<Vec<_>>(),
        vec![agents[2], agents[1]]
    );
    assert!(release_token(app.world_mut(), agents[0], target));
    model.remove_agent(0);
    request(&mut app, &mut model, 2);
    assert_eq!(
        app.world().get::<AiAttackTokens>(target).expect("pool").holders[0].holder,
        agents[2]
    );
}
