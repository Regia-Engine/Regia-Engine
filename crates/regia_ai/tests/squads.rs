// SPDX-License-Identifier: MPL-2.0
//! Squads, attack tokens and formation slots (ADR-2709): fixed steps and exact comparisons, no wall
//! clock.

use std::sync::{Arc, Mutex};

use regia_ai::attack_tokens::{
    AcquireAttackToken, AiAttackTokens, AiTokenHolder, HasAttackToken, ReleaseAttackToken, TokenOutcome,
    acquire_token, holds_token, release_all_tokens, release_token, set_token_capacity,
};
use regia_ai::blackboard::{BbValue, Blackboard};
use regia_ai::formation::{AiFormation, FormationAnchor, FormationShape, resolve_formation};
use regia_ai::squad::{
    AiDead, AiSquad, AiSquadClock, AiSquadMember, AiSquadPlugin, SQUAD_IS_LEADER_KEY, SQUAD_LEADER_KEY,
    SQUAD_ROLE_KEY, SquadError, SquadRead, SquadRole, SquadWrite, claim_squad_role, release_squad_role,
    set_squad_leader, set_squad_role, squad_of,
};
use regia_ai::state_tree::{StateTree, StateTreeDef};
use regia_ai::{
    AiPlugin, BehaviorTree, BehaviorTreeBuilder, BehaviorTreeDef, BtContext, BtStatus, BtTask,
    BtTaskRegistry, DefKind, DefNode,
};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::NavMeshBuilder;
use regia_scene::Transform;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((AiPlugin, AiSquadPlugin));
    app.update();
    app
}

fn step(app: &mut App, n: usize) {
    for _ in 0..n {
        app.world_mut().run_schedule(FixedUpdate);
    }
}

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

fn leaf(name: &str, task: &str) -> DefNode {
    DefNode {
        name: name.into(),
        kind: DefKind::Leaf,
        task: task.into(),
        ..Default::default()
    }
}

fn member(app: &mut App, squad: Entity, rank: i32) -> Entity {
    app.world_mut()
        .spawn((Blackboard::new(), AiSquadMember::new(squad).with_rank(rank)))
        .id()
}

fn board(app: &App, entity: Entity) -> &Blackboard {
    app.world().get::<Blackboard>(entity).expect("a blackboard")
}

// -- Squads --

#[test]
fn the_first_member_leads_and_a_stronger_joiner_does_not_depose_it() {
    let mut app = app();
    let squad = app.world_mut().spawn(AiSquad::new("alpha")).id();
    let a = member(&mut app, squad, 0);
    let b = member(&mut app, squad, 9);
    let data = app.world().get::<AiSquad>(squad).expect("squad");
    assert_eq!(data.members, vec![a, b]);
    assert_eq!(data.leader, Some(a));
    assert_eq!(data.elections, 1);
    assert_eq!(
        board(&app, a).get(SQUAD_IS_LEADER_KEY),
        Some(&BbValue::Bool(true))
    );
    assert_eq!(
        board(&app, b).get(SQUAD_IS_LEADER_KEY),
        Some(&BbValue::Bool(false))
    );
    assert_eq!(
        board(&app, squad).get(SQUAD_LEADER_KEY),
        Some(&BbValue::Entity(a.to_bits()))
    );
    assert_eq!(squad_of(app.world(), a), Some(squad));
}

// The brief's acceptance: killing a leader re-elects by the stated rule, in the same step.
#[test]
fn killing_the_leader_elects_the_highest_rank_then_the_earliest_joiner() {
    let mut app = app();
    let squad = app.world_mut().spawn(AiSquad::new("alpha")).id();
    let leader = member(&mut app, squad, 100);
    let low = member(&mut app, squad, 1);
    let high_early = member(&mut app, squad, 5);
    let high_late = member(&mut app, squad, 5);

    // Despawn: no step runs between the kill and the check.
    app.world_mut().despawn(leader);
    let data = app.world().get::<AiSquad>(squad).expect("squad");
    assert_eq!(
        data.leader,
        Some(high_early),
        "rank 5 beats 1; the earlier of the two 5s"
    );
    assert_eq!(data.elections, 2);
    assert_eq!(data.members, vec![low, high_early, high_late]);
    assert_eq!(
        board(&app, high_early).get(SQUAD_IS_LEADER_KEY),
        Some(&BbValue::Bool(true))
    );
    assert_eq!(
        board(&app, squad).get(SQUAD_LEADER_KEY),
        Some(&BbValue::Entity(high_early.to_bits()))
    );

    // Death in place: same rule, same immediacy.
    app.world_mut().entity_mut(high_early).insert(AiDead);
    let data = app.world().get::<AiSquad>(squad).expect("squad");
    assert_eq!(data.leader, Some(high_late));
    assert_eq!(data.members, vec![low, high_late]);
    assert_eq!(squad_of(app.world(), high_early), None);
    assert_eq!(
        board(&app, high_early).get(SQUAD_IS_LEADER_KEY),
        Some(&BbValue::Bool(false))
    );

    // Leaving (not dying) re-elects too; the last one out leaves no leader.
    app.world_mut().entity_mut(high_late).remove::<AiSquadMember>();
    assert_eq!(
        app.world().get::<AiSquad>(squad).expect("squad").leader,
        Some(low)
    );
    app.world_mut().despawn(low);
    let data = app.world().get::<AiSquad>(squad).expect("squad");
    assert_eq!(data.leader, None);
    assert!(data.members.is_empty());
    assert!(!board(&app, squad).contains(SQUAD_LEADER_KEY));
}

#[test]
fn a_leader_killed_mid_tick_is_replaced_before_the_next_agent_ticks() {
    /// (the agent ticking, the leader it sees)
    type Seen = Vec<(u64, Option<u64>)>;
    /// Kills the entity in the `victim` key, and logs what the squad's leader is on every tick.
    #[derive(Clone, Default)]
    struct Killer {
        seen: Arc<Mutex<Seen>>,
    }
    impl BtTask for Killer {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            if let Some(BbValue::Entity(bits)) = ctx.blackboard().and_then(|b| b.get("victim")).cloned()
                && let Some(victim) = Entity::try_from_bits(bits)
                && ctx.world.get_entity(victim).is_ok()
            {
                ctx.world.despawn(victim);
            }
            let squad = ctx.blackboard().and_then(|b| match b.get("squad") {
                Some(BbValue::Entity(bits)) => Entity::try_from_bits(*bits),
                _ => None,
            });
            let leader = squad
                .and_then(|s| ctx.world.get::<AiSquad>(s))
                .and_then(|s| s.leader)
                .map(Entity::to_bits);
            if let Ok(mut seen) = self.seen.lock() {
                seen.push((ctx.entity.to_bits(), leader));
            }
            BtStatus::Success
        }
    }
    let killer = Killer::default();
    let seen = killer.seen.clone();
    let mut app = app();
    app.world_mut()
        .resource_mut::<BtTaskRegistry>()
        .register_factory("test::Killer", move || Box::new(killer.clone()));
    let squad = app.world_mut().spawn(AiSquad::new("alpha")).id();
    let leader = member(&mut app, squad, 9);
    let heir = member(&mut app, squad, 1);
    let def = BehaviorTreeDef::leaf("kill", "test::Killer");
    let compile =
        |app: &App| BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles");
    // Tick order is arrival order: the first tree kills, the second reads the leader.
    let first = compile(&app);
    let mut killers_board = Blackboard::new();
    killers_board.insert("victim", leader);
    killers_board.insert("squad", squad);
    let killer_agent = app.world_mut().spawn((killers_board, first)).id();
    let second = compile(&app);
    let mut watcher_board = Blackboard::new();
    watcher_board.insert("squad", squad);
    let watcher = app.world_mut().spawn((watcher_board, second)).id();
    step(&mut app, 1);
    let log = seen.lock().expect("log").clone();
    assert_eq!(
        log,
        vec![
            (killer_agent.to_bits(), Some(heir.to_bits())),
            (watcher.to_bits(), Some(heir.to_bits())),
        ]
    );
}

#[test]
fn a_leader_set_by_hand_leads_until_it_goes() {
    let mut app = app();
    let squad = app.world_mut().spawn(AiSquad::new("alpha")).id();
    let a = member(&mut app, squad, 9);
    let b = member(&mut app, squad, 0);
    let outsider = app.world_mut().spawn_empty().id();
    assert_eq!(
        set_squad_leader(app.world_mut(), squad, outsider),
        Err(SquadError::NotAMember)
    );
    assert_eq!(
        set_squad_leader(app.world_mut(), outsider, a),
        Err(SquadError::NotASquad)
    );
    set_squad_leader(app.world_mut(), squad, b).expect("a member");
    assert_eq!(app.world().get::<AiSquad>(squad).expect("squad").leader, Some(b));
    assert_eq!(
        board(&app, a).get(SQUAD_IS_LEADER_KEY),
        Some(&BbValue::Bool(false))
    );
    assert_eq!(
        board(&app, b).get(SQUAD_IS_LEADER_KEY),
        Some(&BbValue::Bool(true))
    );
    app.world_mut().despawn(b);
    assert_eq!(app.world().get::<AiSquad>(squad).expect("squad").leader, Some(a));
}

#[test]
fn role_slots_fill_by_the_rule_and_a_death_passes_the_slot_on() {
    let mut app = app();
    let squad = app
        .world_mut()
        .spawn(
            AiSquad::new("alpha")
                .with_role(SquadRole::new("flanker", 2))
                .with_role(SquadRole::new("support", 1)),
        )
        .id();
    let a = member(&mut app, squad, 0);
    let b = member(&mut app, squad, 0);
    let c = member(&mut app, squad, 0);
    let d = member(&mut app, squad, 0);
    let roles = |app: &App| {
        app.world()
            .get::<AiSquad>(squad)
            .expect("squad")
            .roles
            .iter()
            .map(|r| r.holders.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(roles(&app), vec![vec![a, b], vec![c]]);
    assert_eq!(
        board(&app, a).get(SQUAD_ROLE_KEY),
        Some(&BbValue::String("flanker".into()))
    );
    assert_eq!(
        board(&app, c).get(SQUAD_ROLE_KEY),
        Some(&BbValue::String("support".into()))
    );
    assert!(!board(&app, d).contains(SQUAD_ROLE_KEY));
    assert_eq!(
        app.world()
            .get::<AiSquadMember>(c)
            .expect("member")
            .role
            .as_deref(),
        Some("support")
    );

    // d takes the slot a leaves, the same step.
    app.world_mut().despawn(a);
    assert_eq!(roles(&app), vec![vec![b, d], vec![c]]);
    assert_eq!(
        board(&app, d).get(SQUAD_ROLE_KEY),
        Some(&BbValue::String("flanker".into()))
    );

    // Never more holders than slots, through churn.
    for rank in 0..6 {
        member(&mut app, squad, rank);
    }
    let holders = roles(&app);
    assert_eq!(holders[0].len(), 2);
    assert_eq!(holders[1].len(), 1);
}

#[test]
fn roles_can_be_claimed_by_hand() {
    let mut app = app();
    let manual = |name: &str, capacity| SquadRole {
        auto_fill: false,
        ..SquadRole::new(name, capacity)
    };
    let squad = app
        .world_mut()
        .spawn(
            AiSquad::new("alpha")
                .with_role(manual("flanker", 1))
                .with_role(manual("support", 2)),
        )
        .id();
    let a = member(&mut app, squad, 0);
    let b = member(&mut app, squad, 0);
    let outsider = app.world_mut().spawn_empty().id();
    assert!(!board(&app, a).contains(SQUAD_ROLE_KEY), "auto_fill is off");

    claim_squad_role(app.world_mut(), a, "flanker").expect("free");
    assert_eq!(
        claim_squad_role(app.world_mut(), b, "flanker"),
        Err(SquadError::RoleFull)
    );
    assert_eq!(
        claim_squad_role(app.world_mut(), b, "pilot"),
        Err(SquadError::UnknownRole)
    );
    assert_eq!(
        claim_squad_role(app.world_mut(), outsider, "support"),
        Err(SquadError::NotAMember)
    );
    // Claiming another role leaves the first.
    claim_squad_role(app.world_mut(), a, "support").expect("free");
    let data = app.world().get::<AiSquad>(squad).expect("squad");
    assert!(data.roles[0].holders.is_empty());
    assert_eq!(data.roles[1].holders, vec![a]);
    assert_eq!(
        board(&app, a).get(SQUAD_ROLE_KEY),
        Some(&BbValue::String("support".into()))
    );
    claim_squad_role(app.world_mut(), a, "support").expect("already held is fine");

    release_squad_role(app.world_mut(), a).expect("a member");
    assert!(!board(&app, a).contains(SQUAD_ROLE_KEY));
    assert_eq!(
        release_squad_role(app.world_mut(), outsider),
        Err(SquadError::NotAMember)
    );
}

#[test]
fn a_squad_that_goes_away_takes_its_memberships_with_it() {
    let mut app = app();
    let squad = app.world_mut().spawn(AiSquad::new("alpha")).id();
    let a = member(&mut app, squad, 0);
    app.world_mut().despawn(squad);
    app.world_mut().flush();
    assert!(app.world().get::<AiSquadMember>(a).is_none());
    assert_eq!(squad_of(app.world(), a), None);
    // A member of an entity that is not a squad is simply in no squad.
    let plain = app.world_mut().spawn_empty().id();
    let orphan = member(&mut app, plain, 0);
    assert_eq!(squad_of(app.world(), orphan), None);
}

#[test]
fn a_squad_that_goes_away_leaves_nothing_on_its_members_blackboards() {
    let mut app = app();
    let foe = body(&mut app, None, Vec3::ZERO);
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha").with_role(SquadRole::new("flanker", 1)),
            AiFormation::new(FormationAnchor::Entity(foe), FormationShape::default()),
        ))
        .id();
    let a = body(&mut app, Some(squad), Vec3::new(1.0, 0.0, 0.0));
    step(&mut app, 1);
    assert_eq!(
        board(&app, a).get(SQUAD_IS_LEADER_KEY),
        Some(&BbValue::Bool(true))
    );
    assert!(board(&app, a).contains(SQUAD_ROLE_KEY));
    assert!(board(&app, a).contains("formation_slot"));
    app.world_mut().despawn(squad);
    app.world_mut().flush();
    assert_eq!(
        board(&app, a).get(SQUAD_IS_LEADER_KEY),
        Some(&BbValue::Bool(false))
    );
    assert!(!board(&app, a).contains(SQUAD_ROLE_KEY));
    assert!(!board(&app, a).contains("formation_slot"));
    assert!(!board(&app, a).contains("formation_slot_index"));
}

#[test]
fn removing_or_replacing_a_formation_takes_the_old_keys_away() {
    let mut app = app();
    let foe = body(&mut app, None, Vec3::ZERO);
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation::new(FormationAnchor::Entity(foe), FormationShape::default()),
        ))
        .id();
    let a = body(&mut app, Some(squad), Vec3::new(1.0, 0.0, 0.0));
    step(&mut app, 1);
    assert!(board(&app, a).contains("formation_slot"));
    app.world_mut().entity_mut(squad).insert(AiFormation {
        slot_key: "post".to_string(),
        ..AiFormation::new(FormationAnchor::Entity(foe), FormationShape::default())
    });
    assert!(!board(&app, a).contains("formation_slot"));
    step(&mut app, 1);
    assert!(board(&app, a).contains("post"));
    assert!(board(&app, a).contains("post_index"));
    app.world_mut().entity_mut(squad).remove::<AiFormation>();
    assert!(!board(&app, a).contains("post"));
    assert!(!board(&app, a).contains("post_index"));
}

#[test]
fn a_formation_does_not_touch_the_slot_of_a_target_in_another_squad() {
    let mut app = app();
    let far = body(&mut app, None, Vec3::new(50.0, 0.0, 0.0));
    let squad_b = app
        .world_mut()
        .spawn((
            AiSquad::new("b"),
            AiFormation {
                interval_steps: 1,
                ..AiFormation::new(FormationAnchor::Entity(far), FormationShape::default())
            },
        ))
        .id();
    let target_in_b = body(&mut app, Some(squad_b), Vec3::new(40.0, 0.0, 0.0));
    let squad_a = app
        .world_mut()
        .spawn((
            AiSquad::new("a"),
            AiFormation {
                interval_steps: 1,
                ..AiFormation::new(FormationAnchor::Entity(target_in_b), FormationShape::default())
            },
        ))
        .id();
    body(&mut app, Some(squad_a), Vec3::new(1.0, 0.0, 0.0));
    step(&mut app, 3);
    assert!(board(&app, target_in_b).contains("formation_slot"));
}

#[test]
fn a_role_edit_evicts_the_latest_holders_and_fills_free_slots() {
    let mut app = app();
    let squad = app.world_mut().spawn(AiSquad::new("alpha")).id();
    let a = member(&mut app, squad, 0);
    let b = member(&mut app, squad, 0);
    let c = member(&mut app, squad, 0);
    set_squad_role(app.world_mut(), squad, "guard", 2, true).expect("a squad");
    let holders = |app: &App| {
        app.world().get::<AiSquad>(squad).expect("squad").roles[0]
            .holders
            .clone()
    };
    assert_eq!(holders(&app), vec![a, b], "new slots fill at once");
    set_squad_role(app.world_mut(), squad, "guard", 1, true).expect("a squad");
    assert_eq!(holders(&app), vec![a]);
    assert!(!board(&app, b).contains(SQUAD_ROLE_KEY));
    assert!(!board(&app, c).contains(SQUAD_ROLE_KEY));
    assert_eq!(
        set_squad_role(app.world_mut(), a, "guard", 1, true),
        Err(SquadError::NotASquad)
    );
}

#[test]
fn writing_the_squad_field_does_not_strand_the_old_squad() {
    let mut app = app();
    let old = app.world_mut().spawn(AiSquad::new("old")).id();
    let new = app.world_mut().spawn(AiSquad::new("new")).id();
    let a = member(&mut app, old, 0);
    // A reflected write changes the field in place, with no hook.
    app.world_mut().get_mut::<AiSquadMember>(a).expect("member").squad = new;
    assert_eq!(squad_of(app.world(), a), Some(old), "a write is not a move");
    app.world_mut().despawn(a);
    let data = app.world().get::<AiSquad>(old).expect("squad");
    assert!(data.members.is_empty());
    assert_eq!(data.leader, None);
}

#[test]
fn a_revived_member_comes_back_without_taking_over() {
    let mut app = app();
    let squad = app.world_mut().spawn(AiSquad::new("alpha")).id();
    let a = member(&mut app, squad, 9);
    let b = member(&mut app, squad, 0);
    app.world_mut().entity_mut(a).insert(AiDead);
    assert_eq!(app.world().get::<AiSquad>(squad).expect("squad").leader, Some(b));
    app.world_mut().entity_mut(a).remove::<AiDead>();
    app.world_mut().flush();
    let data = app.world().get::<AiSquad>(squad).expect("squad");
    assert_eq!(data.members, vec![b, a], "it rejoins at the end");
    assert_eq!(data.leader, Some(b), "a living leader stays");
    // Despawning a dead member changes nothing further.
    app.world_mut().entity_mut(a).insert(AiDead);
    app.world_mut().despawn(a);
    app.world_mut().flush();
    assert_eq!(app.world().get::<AiSquad>(squad).expect("squad").members, vec![b]);
}

#[test]
fn members_share_the_squad_blackboard_through_the_leaves() {
    let mut app = app();
    {
        let mut registry = app.world_mut().resource_mut::<BtTaskRegistry>();
        SquadWrite::new("my_target", "shared_target").register(&mut registry, "test::Tell");
        SquadRead::new("shared_target", "target").register(&mut registry, "test::Hear");
    }
    let squad = app.world_mut().spawn(AiSquad::new("alpha")).id();
    let spotter = member(&mut app, squad, 0);
    let listener = member(&mut app, squad, 0);
    let lone = app.world_mut().spawn(Blackboard::new()).id();
    let tree = |app: &App, task: &str| {
        BehaviorTree::compile(
            &BehaviorTreeDef::leaf("l", task),
            app.world().resource::<BtTaskRegistry>(),
        )
        .expect("compiles")
    };
    let tell = tree(&app, "test::Tell");
    let hear = tree(&app, "test::Hear");
    let lone_hear = tree(&app, "test::Hear");
    app.world_mut().entity_mut(listener).insert(hear);
    app.world_mut().entity_mut(spotter).insert(tell);
    app.world_mut().entity_mut(lone).insert(lone_hear);
    let last = |app: &App, e: Entity| {
        app.world()
            .get::<regia_ai::BehaviorTreeStatus>(e)
            .expect("status")
            .last_status
    };

    step(&mut app, 1);
    assert_eq!(last(&app, spotter), BtStatus::Failure, "nothing to tell yet");
    assert_eq!(last(&app, listener), BtStatus::Failure, "nothing to hear yet");
    assert_eq!(last(&app, lone), BtStatus::Failure, "not in a squad");

    app.world_mut()
        .get_mut::<Blackboard>(spotter)
        .expect("board")
        .insert("my_target", Vec3::new(1.0, 2.0, 3.0));
    step(&mut app, 2);
    assert_eq!(
        board(&app, squad).get("shared_target"),
        Some(&BbValue::Vec3(Vec3::new(1.0, 2.0, 3.0)))
    );
    assert_eq!(
        board(&app, listener).get("target"),
        Some(&BbValue::Vec3(Vec3::new(1.0, 2.0, 3.0)))
    );
    assert_eq!(last(&app, lone), BtStatus::Failure);
}

// -- Attack tokens --

fn target(app: &mut App, capacity: u32) -> Entity {
    app.world_mut().spawn(AiAttackTokens::new(capacity)).id()
}

fn holders(app: &App, target: Entity) -> Vec<Entity> {
    app.world()
        .get::<AiAttackTokens>(target)
        .expect("pool")
        .holders
        .iter()
        .map(|hold| hold.holder)
        .collect()
}

/// Holds its token for 20 ticks, then succeeds.
#[derive(Default)]
struct Swing {
    left: u32,
}

impl BtTask for Swing {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        if self.left == 0 {
            self.left = 20;
            return BtStatus::Running;
        }
        self.left -= 1;
        if let Some(mut board) = ctx.blackboard_mut() {
            let count = match board.get("swings") {
                Some(BbValue::Int(n)) => *n,
                _ => 0,
            };
            board.insert("swings", count + 1);
        }
        if self.left == 0 {
            BtStatus::Success
        } else {
            BtStatus::Running
        }
    }
}

/// `Sequence(Acquire, Swing, Release)`.
fn attacker_tree(app: &App) -> BehaviorTree {
    let mut b = BehaviorTreeBuilder::new();
    let acquire = b.add(leaf("acquire", "regia_ai::AcquireAttackToken"));
    let swing = b.add(leaf("swing", "test::Swing"));
    let release = b.add(leaf("release", "regia_ai::ReleaseAttackToken"));
    let root = b.sequence("attack", vec![acquire, swing, release]);
    let def = b.build(root);
    BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles")
}

/// 8 attackers, 2 tokens, 600 steps. Returns the per-step holder hash, the most holders seen and each
/// agent's swing count.
fn eight_attackers() -> (u64, usize, Vec<i64>) {
    let mut app = app();
    app.world_mut()
        .resource_mut::<BtTaskRegistry>()
        .register_factory("test::Swing", || Box::new(Swing::default()));
    let foe = target(&mut app, 2);
    let mut agents = Vec::new();
    for _ in 0..8 {
        let tree = attacker_tree(&app);
        let mut board = Blackboard::new();
        board.insert("target", foe);
        agents.push(app.world_mut().spawn((board, tree)).id());
    }
    let (mut hash, mut most) = (0xCBF2_9CE4_8422_2325u64, 0);
    for _ in 0..600 {
        step(&mut app, 1);
        let held = holders(&app, foe);
        most = most.max(held.len());
        for holder in held {
            let index = agents.iter().position(|a| *a == holder).unwrap_or(99);
            fnv(&mut hash, &index.to_le_bytes());
        }
        fnv(&mut hash, b"/");
    }
    let swings = agents
        .iter()
        .map(|agent| match board(&app, *agent).get("swings") {
            Some(BbValue::Int(n)) => *n,
            _ => 0,
        })
        .collect();
    (hash, most, swings)
}

// The brief's acceptance: never 3 holders, and two runs hash equal.
#[test]
fn eight_agents_on_two_tokens_never_have_three_holders_and_two_runs_agree() {
    let (hash_a, most_a, swings_a) = eight_attackers();
    let (hash_b, most_b, swings_b) = eight_attackers();
    assert_eq!(most_a, 2, "the pool fills and never overflows");
    assert_eq!(most_b, 2);
    assert_eq!(hash_a, hash_b);
    assert_eq!(swings_a, swings_b);
    // Taking turns: nobody starves, and the shares are even to within one swing cycle.
    let (low, high) = (
        swings_a.iter().copied().min().unwrap_or(0),
        swings_a.iter().copied().max().unwrap_or(0),
    );
    assert!(low > 0, "{swings_a:?}");
    assert!(high - low <= 20, "{swings_a:?}");
}

// The brief's acceptance: a holder's death frees the token the same step.
#[test]
fn a_holders_death_frees_the_token_at_once() {
    let mut app = app();
    let foe = target(&mut app, 1);
    let a = app.world_mut().spawn_empty().id();
    let b = app.world_mut().spawn_empty().id();
    assert_eq!(acquire_token(app.world_mut(), a, foe, 0), TokenOutcome::Granted);
    assert_eq!(acquire_token(app.world_mut(), b, foe, 0), TokenOutcome::Denied);

    app.world_mut().entity_mut(a).insert(AiDead);
    assert!(holders(&app, foe).is_empty());
    assert_eq!(acquire_token(app.world_mut(), a, foe, 0), TokenOutcome::AgentGone);
    assert!(
        app.world()
            .get::<AiTokenHolder>(a)
            .expect("holder")
            .held
            .is_empty()
    );
    assert_eq!(acquire_token(app.world_mut(), b, foe, 0), TokenOutcome::Granted);

    app.world_mut().despawn(b);
    assert!(holders(&app, foe).is_empty(), "a despawn frees it too");
}

#[test]
fn a_death_in_a_tick_lets_a_later_agent_take_the_token_that_same_step() {
    /// Despawns the entity in `victim`, once.
    #[derive(Default)]
    struct Kill;
    impl BtTask for Kill {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            if let Some(BbValue::Entity(bits)) = ctx.blackboard().and_then(|b| b.get("victim")).cloned()
                && let Some(victim) = Entity::try_from_bits(bits)
                && ctx.world.get_entity(victim).is_ok()
            {
                ctx.world.despawn(victim);
            }
            BtStatus::Success
        }
    }
    let mut app = app();
    app.world_mut()
        .resource_mut::<BtTaskRegistry>()
        .register_factory("test::Kill", || Box::new(Kill));
    let foe = target(&mut app, 1);
    let holder = app.world_mut().spawn_empty().id();
    assert_eq!(
        acquire_token(app.world_mut(), holder, foe, 0),
        TokenOutcome::Granted
    );
    let registry = app.world().resource::<BtTaskRegistry>();
    let kill = BehaviorTree::compile(&BehaviorTreeDef::leaf("k", "test::Kill"), registry).expect("compiles");
    let take = BehaviorTree::compile(
        &BehaviorTreeDef::leaf("a", "regia_ai::AcquireAttackToken"),
        registry,
    )
    .expect("compiles");
    let mut killers_board = Blackboard::new();
    killers_board.insert("victim", holder);
    app.world_mut().spawn((killers_board, kill));
    let mut takers_board = Blackboard::new();
    takers_board.insert("target", foe);
    let taker = app.world_mut().spawn((takers_board, take)).id();
    step(&mut app, 1);
    assert_eq!(holders(&app, foe), vec![taker]);
}

#[test]
fn a_targets_death_or_despawn_frees_every_holder() {
    let mut app = app();
    let foe = target(&mut app, 2);
    let a = app.world_mut().spawn_empty().id();
    let b = app.world_mut().spawn_empty().id();
    acquire_token(app.world_mut(), a, foe, 0);
    acquire_token(app.world_mut(), b, foe, 0);
    app.world_mut().entity_mut(foe).insert(AiDead);
    assert!(holders(&app, foe).is_empty());
    assert!(
        app.world()
            .get::<AiTokenHolder>(a)
            .expect("holder")
            .held
            .is_empty()
    );
    assert_eq!(acquire_token(app.world_mut(), a, foe, 0), TokenOutcome::NoPool);

    let other = target(&mut app, 2);
    acquire_token(app.world_mut(), a, other, 0);
    app.world_mut().despawn(other);
    assert!(
        app.world()
            .get::<AiTokenHolder>(a)
            .expect("holder")
            .held
            .is_empty()
    );
    assert_eq!(acquire_token(app.world_mut(), a, other, 0), TokenOutcome::NoPool);
    // A holder that goes away later finds nothing to clean up.
    app.world_mut().despawn(a);
}

#[test]
fn a_free_token_goes_to_the_highest_priority_then_the_longest_wait() {
    let mut app = app();
    let foe = target(&mut app, 1);
    let holder = app.world_mut().spawn_empty().id();
    let low_early = app.world_mut().spawn_empty().id();
    let high = app.world_mut().spawn_empty().id();
    let low_late = app.world_mut().spawn_empty().id();
    let world = app.world_mut();
    assert_eq!(acquire_token(world, holder, foe, 0), TokenOutcome::Granted);
    assert_eq!(acquire_token(world, low_early, foe, 1), TokenOutcome::Denied);
    assert_eq!(acquire_token(world, high, foe, 5), TokenOutcome::Denied);
    assert_eq!(acquire_token(world, low_late, foe, 1), TokenOutcome::Denied);

    assert!(release_token(world, holder, foe));
    // The token is free, but others are ahead of whoever asks out of turn.
    assert_eq!(acquire_token(world, low_late, foe, 1), TokenOutcome::Denied);
    assert_eq!(acquire_token(world, low_early, foe, 1), TokenOutcome::Denied);
    assert_eq!(
        acquire_token(world, holder, foe, 0),
        TokenOutcome::Denied,
        "it goes to the back"
    );
    assert_eq!(acquire_token(world, high, foe, 5), TokenOutcome::Granted);
    assert!(release_token(world, high, foe));
    assert_eq!(acquire_token(world, low_late, foe, 1), TokenOutcome::Denied);
    assert_eq!(acquire_token(world, low_early, foe, 1), TokenOutcome::Granted);
    // Asking again while holding is fine, and takes no second token.
    assert_eq!(acquire_token(world, low_early, foe, 1), TokenOutcome::AlreadyHeld);
    let pool = world.get::<AiAttackTokens>(foe).expect("pool");
    assert_eq!(pool.holders.len(), 1);
    assert!(pool.granted >= 3);
    assert!(pool.denied >= 6);
}

#[test]
fn preemption_takes_from_the_lowest_priority_and_only_for_strictly_higher() {
    let mut app = app();
    let foe = app
        .world_mut()
        .spawn(AiAttackTokens::new(2).with_preemption())
        .id();
    let weak = app.world_mut().spawn_empty().id();
    let strong = app.world_mut().spawn_empty().id();
    let equal = app.world_mut().spawn_empty().id();
    let boss = app.world_mut().spawn_empty().id();
    let world = app.world_mut();
    assert_eq!(acquire_token(world, weak, foe, 1), TokenOutcome::Granted);
    assert_eq!(acquire_token(world, strong, foe, 5), TokenOutcome::Granted);
    assert_eq!(
        acquire_token(world, equal, foe, 1),
        TokenOutcome::Denied,
        "equal does not preempt"
    );
    assert_eq!(acquire_token(world, boss, foe, 9), TokenOutcome::Preempted(weak));
    assert!(!holds_token(world, weak, foe));
    assert!(holds_token(world, boss, foe));
    let pool = world.get::<AiAttackTokens>(foe).expect("pool");
    assert_eq!(pool.holders.len(), 2);
    assert_eq!(pool.preempted, 1);
    assert!(world.get::<AiTokenHolder>(weak).expect("holder").held.is_empty());
    // Without preemption, priority only orders the line.
    let plain = world.spawn(AiAttackTokens::new(1)).id();
    assert_eq!(acquire_token(world, weak, plain, 1), TokenOutcome::Granted);
    assert_eq!(acquire_token(world, boss, plain, 9), TokenOutcome::Denied);
    assert!(holds_token(world, weak, plain));
}

#[test]
fn lowering_the_capacity_evicts_the_lowest_priority_and_never_overflows() {
    let mut app = app();
    let foe = target(&mut app, 3);
    let a = app.world_mut().spawn_empty().id();
    let b = app.world_mut().spawn_empty().id();
    let c = app.world_mut().spawn_empty().id();
    let world = app.world_mut();
    acquire_token(world, a, foe, 5);
    acquire_token(world, b, foe, 1);
    acquire_token(world, c, foe, 1);
    assert_eq!(set_token_capacity(world, foe, 1), Some(vec![c, b]));
    assert_eq!(world.get::<AiAttackTokens>(foe).expect("pool").holders.len(), 1);
    assert!(holds_token(world, a, foe));
    assert!(world.get::<AiTokenHolder>(b).expect("holder").held.is_empty());
    assert_eq!(set_token_capacity(world, a, 1), None);
    assert_eq!(acquire_token(world, b, foe, 9), TokenOutcome::Denied);
}

#[test]
fn a_waiter_that_stops_asking_is_forgotten() {
    let mut app = app();
    let foe = target(&mut app, 1);
    app.world_mut()
        .get_mut::<AiAttackTokens>(foe)
        .expect("pool")
        .waiter_ttl_steps = 5;
    let holder = app.world_mut().spawn_empty().id();
    let quitter = app.world_mut().spawn_empty().id();
    let keen = app.world_mut().spawn_empty().id();
    let world = app.world_mut();
    acquire_token(world, holder, foe, 0);
    assert_eq!(acquire_token(world, quitter, foe, 9), TokenOutcome::Denied);
    release_token(world, holder, foe);
    assert_eq!(
        acquire_token(world, keen, foe, 0),
        TokenOutcome::Denied,
        "the quitter is ahead"
    );
    world.resource_mut::<AiSquadClock>().step += 6;
    assert_eq!(
        acquire_token(world, keen, foe, 0),
        TokenOutcome::Granted,
        "and then it is not"
    );
    assert!(world.get::<AiAttackTokens>(foe).expect("pool").waiters.is_empty());

    // A waiter that dies or is despawned does not hold the line either.
    let pool = target(&mut app, 1);
    let dead = app.world_mut().spawn_empty().id();
    let gone = app.world_mut().spawn_empty().id();
    let asker = app.world_mut().spawn_empty().id();
    let blocker = app.world_mut().spawn_empty().id();
    let world = app.world_mut();
    acquire_token(world, blocker, pool, 0);
    acquire_token(world, dead, pool, 5);
    acquire_token(world, gone, pool, 5);
    release_token(world, blocker, pool);
    world.entity_mut(dead).insert(AiDead);
    world.despawn(gone);
    assert_eq!(acquire_token(world, asker, pool, 0), TokenOutcome::Granted);
}

#[test]
fn release_all_gives_back_every_pool_and_cancels_a_request() {
    let mut app = app();
    let a = target(&mut app, 1);
    let b = target(&mut app, 1);
    let agent = app.world_mut().spawn_empty().id();
    let other = app.world_mut().spawn_empty().id();
    let world = app.world_mut();
    acquire_token(world, agent, a, 0);
    acquire_token(world, agent, b, 0);
    assert_eq!(release_all_tokens(world, agent), 2);
    assert_eq!(release_all_tokens(world, agent), 0);
    assert!(!release_token(world, agent, a), "nothing held");
    acquire_token(world, other, a, 0);
    assert_eq!(acquire_token(world, agent, a, 3), TokenOutcome::Denied);
    assert_eq!(world.get::<AiAttackTokens>(a).expect("pool").waiters.len(), 1);
    release_token(world, agent, a);
    assert!(world.get::<AiAttackTokens>(a).expect("pool").waiters.is_empty());
}

#[test]
fn the_leaves_report_through_tree_statuses() {
    let mut app = app();
    {
        let mut registry = app.world_mut().resource_mut::<BtTaskRegistry>();
        AcquireAttackToken::default()
            .with_wait(false)
            .register(&mut registry, "test::TryAcquire");
        ReleaseAttackToken {
            target_key: Some("target".to_string()),
        }
        .register(&mut registry, "test::ReleaseThis");
        HasAttackToken::default().register(&mut registry, "test::Has");
    }
    let foe = target(&mut app, 1);
    let blocker = app.world_mut().spawn_empty().id();
    acquire_token(app.world_mut(), blocker, foe, 0);
    let run = |app: &mut App, task: &str, with_target: bool| -> BtStatus {
        let tree = BehaviorTree::compile(
            &BehaviorTreeDef::leaf("l", task),
            app.world().resource::<BtTaskRegistry>(),
        )
        .expect("compiles");
        let mut board = Blackboard::new();
        if with_target {
            board.insert("target", foe);
        }
        let agent = app.world_mut().spawn((board, tree)).id();
        step(app, 1);
        let status = app
            .world()
            .get::<regia_ai::BehaviorTreeStatus>(agent)
            .expect("status")
            .last_status;
        app.world_mut().despawn(agent);
        status
    };
    assert_eq!(
        run(&mut app, "regia_ai::AcquireAttackToken", true),
        BtStatus::Running
    );
    assert_eq!(run(&mut app, "test::TryAcquire", true), BtStatus::Failure);
    assert_eq!(
        run(&mut app, "test::TryAcquire", false),
        BtStatus::Failure,
        "no target"
    );
    assert_eq!(run(&mut app, "test::Has", true), BtStatus::Failure);
    assert_eq!(
        run(&mut app, "test::ReleaseThis", true),
        BtStatus::Success,
        "always"
    );
    assert_eq!(
        run(&mut app, "regia_ai::ReleaseAttackToken", false),
        BtStatus::Success
    );
    assert_eq!(holders(&app, foe), vec![blocker], "another agent's token stays");
}

#[test]
fn a_priority_can_come_from_the_blackboard() {
    let mut app = app();
    AcquireAttackToken::default()
        .with_priority_key("urgency")
        .register(
            &mut app.world_mut().resource_mut::<BtTaskRegistry>(),
            "test::Urgent",
        );
    let foe = target(&mut app, 0);
    let tree = BehaviorTree::compile(
        &BehaviorTreeDef::leaf("l", "test::Urgent"),
        app.world().resource::<BtTaskRegistry>(),
    )
    .expect("compiles");
    let mut agents_board = Blackboard::new();
    agents_board.insert("target", foe);
    agents_board.insert("urgency", 7);
    app.world_mut().spawn((agents_board, tree));
    step(&mut app, 1);
    let pool = app.world().get::<AiAttackTokens>(foe).expect("pool");
    assert_eq!(pool.waiters.len(), 1);
    assert_eq!(pool.waiters[0].priority, 7);
}

/// Never finishes.
#[derive(Default)]
struct Forever;

impl BtTask for Forever {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Running
    }
}

#[test]
fn a_scoped_acquire_gives_the_token_back_when_a_timeout_aborts_the_attack() {
    let mut app = app();
    app.world_mut()
        .resource_mut::<BtTaskRegistry>()
        .register_factory("test::Forever", || Box::new(Forever));
    let foe = target(&mut app, 1);
    let build = |app: &App, acquire: &str| {
        let mut b = BehaviorTreeBuilder::new();
        let a = b.add(leaf("acquire", acquire));
        let f = b.add(leaf("forever", "test::Forever"));
        let seq = b.sequence("attack", vec![a, f]);
        let root = b.timeout("limit", 0.05, seq);
        let def = b.build(root);
        BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles")
    };
    let scoped = build(&app, "regia_ai::AcquireAttackTokenScoped");
    let mut with_target = Blackboard::new();
    with_target.insert("target", foe);
    let agent = app.world_mut().spawn((with_target.clone(), scoped)).id();
    step(&mut app, 1);
    assert_eq!(holders(&app, foe), vec![agent]);
    step(&mut app, 8);
    // The timeout fired and the next run took it again; between those the leaf gave it back at least once.
    let granted = app.world().get::<AiAttackTokens>(foe).expect("pool").granted;
    assert!(granted >= 2, "granted {granted}");

    // The plain leaf keeps it across the abort.
    let other = target(&mut app, 1);
    let plain = build(&app, "regia_ai::AcquireAttackToken");
    with_target.insert("target", other);
    let keeper = app.world_mut().spawn((with_target, plain)).id();
    step(&mut app, 8);
    assert_eq!(holders(&app, other), vec![keeper]);
    assert_eq!(app.world().get::<AiAttackTokens>(other).expect("pool").granted, 1);
}

#[test]
fn a_scoped_acquire_lets_go_of_the_old_target_when_the_target_changes() {
    let mut app = app();
    let first = target(&mut app, 1);
    let second = target(&mut app, 1);
    let build = |app: &App, task: &str| {
        BehaviorTree::compile(
            &BehaviorTreeDef::leaf("l", task),
            app.world().resource::<BtTaskRegistry>(),
        )
        .expect("compiles")
    };
    let scoped = build(&app, "regia_ai::AcquireAttackTokenScoped");
    let plain = build(&app, "regia_ai::AcquireAttackToken");
    let mut with_target = Blackboard::new();
    with_target.insert("target", first);
    let a = app.world_mut().spawn((with_target.clone(), scoped)).id();
    let b = app.world_mut().spawn((with_target, plain)).id();
    step(&mut app, 1);
    assert_eq!(holders(&app, first), vec![a]);
    let pool = app.world().get::<AiAttackTokens>(first).expect("pool");
    assert_eq!(pool.waiters.len(), 1);
    assert_eq!(pool.waiters[0].agent, b);

    // The waiter's request ends with the switch; the scoped holder lets go of its token.
    app.world_mut()
        .get_mut::<Blackboard>(b)
        .expect("board")
        .insert("target", second);
    step(&mut app, 1);
    assert!(
        app.world()
            .get::<AiAttackTokens>(first)
            .expect("pool")
            .waiters
            .is_empty()
    );
    assert_eq!(holders(&app, second), vec![b]);
    app.world_mut()
        .get_mut::<Blackboard>(a)
        .expect("board")
        .insert("target", second);
    step(&mut app, 1);
    assert!(holders(&app, first).is_empty());
    assert_eq!(holders(&app, second), vec![b], "the plain holder keeps its token");
}

#[test]
fn state_tree_tasks_acquire_and_release() {
    let mut app = app();
    let foe = target(&mut app, 1);
    let def = StateTreeDef::from_ron(
        r#"(
            format_version: 2,
            root: "Root",
            states: [
                (name: "Root", children: ["Wait", "Attack", "Rest"]),
                (name: "Wait", tasks: ["regia_ai::AcquireAttackToken"], transitions: [
                    (trigger: Succeeded, target: State("Attack")),
                ]),
                (name: "Attack", tasks: ["regia_ai::HasAttackToken"], transitions: [
                    (trigger: Succeeded, target: State("Rest")),
                ]),
                (name: "Rest", tasks: ["regia_ai::ReleaseAttackToken"], transitions: [
                    (trigger: Succeeded, target: State("Wait")),
                ]),
            ],
        )"#,
    )
    .expect("parses");
    let mut agents = Vec::new();
    for _ in 0..3 {
        let tree = StateTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles");
        let mut with_target = Blackboard::new();
        with_target.insert("target", foe);
        agents.push(app.world_mut().spawn((tree, with_target)).id());
    }
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..200 {
        step(&mut app, 1);
        let held = holders(&app, foe);
        assert!(held.len() <= 1, "{held:?}");
        seen.extend(held);
    }
    assert_eq!(seen.len(), 3, "the token went round all three");
}

#[test]
fn a_scoped_state_task_releases_when_the_state_is_left() {
    let mut app = app();
    app.world_mut()
        .resource_mut::<BtTaskRegistry>()
        .register_factory("test::Forever", || Box::new(Forever));
    let foe = target(&mut app, 1);
    let def = StateTreeDef::from_ron(
        r#"(
            format_version: 2,
            root: "Root",
            states: [
                (name: "Root", children: ["Attack", "Away"]),
                (name: "Attack", tasks: ["regia_ai::AcquireAttackTokenScoped", "test::Forever"], transitions: [
                    (trigger: Tick, condition: Key("leave"), target: State("Away")),
                ]),
                (name: "Away", tasks: ["test::Forever"]),
            ],
        )"#,
    )
    .expect("parses");
    let tree = StateTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles");
    let mut with_target = Blackboard::new();
    with_target.insert("target", foe);
    let agent = app.world_mut().spawn((tree, with_target)).id();
    step(&mut app, 2);
    assert_eq!(holders(&app, foe), vec![agent]);
    app.world_mut()
        .get_mut::<Blackboard>(agent)
        .expect("board")
        .insert("leave", true);
    step(&mut app, 2);
    assert!(holders(&app, foe).is_empty(), "leaving the state released it");
}

// -- Formations --

/// A 40 m by 40 m floor at y = 0 around the origin, with a hole in `hole` (min x, max x, min z, max z).
fn floor(app: &mut App, hole: Option<(f32, f32, f32, f32)>) {
    let cell = 2.0f32;
    let mut triangles = Vec::new();
    for ix in -10..10 {
        for iz in -10..10 {
            let (x, z) = (ix as f32 * cell, iz as f32 * cell);
            if let Some((x0, x1, z0, z1)) = hole
                && x + cell > x0
                && x < x1
                && z + cell > z0
                && z < z1
            {
                continue;
            }
            let a = Vec3::new(x, 0.0, z);
            let b = Vec3::new(x + cell, 0.0, z);
            let c = Vec3::new(x + cell, 0.0, z + cell);
            let d = Vec3::new(x, 0.0, z + cell);
            triangles.push([a, b, c]);
            triangles.push([a, c, d]);
        }
    }
    let mesh = NavMeshBuilder::default()
        .build(&triangles)
        .expect("the mesh builds");
    app.insert_resource(mesh);
}

fn body(app: &mut App, squad: Option<Entity>, position: Vec3) -> Entity {
    let entity = app
        .world_mut()
        .spawn((Transform::from_translation(position), Blackboard::new()))
        .id();
    if let Some(squad) = squad {
        app.world_mut()
            .entity_mut(entity)
            .insert(AiSquadMember::new(squad));
    }
    entity
}

/// Every slot of a formation is one member's, and two slots are never closer than the separation.
fn assert_unique(app: &App, squad: Entity) -> usize {
    let formation = app.world().get::<AiFormation>(squad).expect("formation");
    let mut indices: Vec<u32> = formation.slots.iter().map(|s| s.index).collect();
    indices.sort_unstable();
    let before = indices.len();
    indices.dedup();
    assert_eq!(
        indices.len(),
        before,
        "a slot index is shared: {:?}",
        formation.slots
    );
    let mut members: Vec<Entity> = formation.slots.iter().map(|s| s.member).collect();
    members.sort();
    members.dedup();
    assert_eq!(members.len(), before, "a member has two slots");
    for (i, a) in formation.slots.iter().enumerate() {
        for b in &formation.slots[i + 1..] {
            let gap = (a.position - b.position).length();
            assert!(
                gap >= formation.min_separation - 1e-4,
                "slots {} and {} are {gap} apart",
                a.index,
                b.index
            );
        }
    }
    before
}

// The brief's acceptance: slots stay unique over 600 steps, while the target moves and members come and go.
#[test]
fn formation_slots_stay_unique_over_600_steps() {
    let mut app = app();
    floor(&mut app, Some((-6.0, 2.0, 2.0, 8.0)));
    let foe = body(&mut app, None, Vec3::new(1.0, 0.0, 0.0));
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation {
                interval_steps: 3,
                ..AiFormation::new(
                    FormationAnchor::Entity(foe),
                    FormationShape::Ring {
                        radius: 3.0,
                        spacing: 1.5,
                    },
                )
            },
        ))
        .id();
    let mut members: Vec<Entity> = (0..12)
        .map(|i| body(&mut app, Some(squad), Vec3::new(i as f32 - 6.0, 0.0, -9.0)))
        .collect();
    let mut spawned = 0;
    for tick in 0..600 {
        let angle = tick as f32 * 0.05;
        app.world_mut().get_mut::<Transform>(foe).expect("t").translation =
            Vec3::new(angle.cos() * 8.0, 0.0, angle.sin() * 8.0);
        match tick % 97 {
            40 => {
                if let Some(victim) = members.pop() {
                    app.world_mut().despawn(victim);
                }
            }
            80 => {
                if let Some(victim) = members.first().copied() {
                    app.world_mut().entity_mut(victim).insert(AiDead);
                    members.remove(0);
                }
            }
            90 => {
                spawned += 1;
                members.push(body(&mut app, Some(squad), Vec3::new(spawned as f32, 0.0, 9.0)));
            }
            _ => {}
        }
        step(&mut app, 1);
        let placed = assert_unique(&app, squad);
        let formation = app.world().get::<AiFormation>(squad).expect("formation");
        assert_eq!(placed + formation.unplaced.len(), members.len(), "step {tick}");
        for slot in &formation.slots {
            // On the mesh: on the floor.
            assert!(slot.position.y.abs() < 0.5, "{slot:?}");
            assert_eq!(
                board(&app, slot.member).get("formation_slot"),
                Some(&BbValue::Vec3(slot.position))
            );
            assert_eq!(
                board(&app, slot.member).get("formation_slot_index"),
                Some(&BbValue::Int(i64::from(slot.index)))
            );
        }
    }
    assert!(!members.is_empty());
}

fn formation_hash() -> u64 {
    let mut app = app();
    floor(&mut app, None);
    let foe = body(&mut app, None, Vec3::ZERO);
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation::new(
                FormationAnchor::Entity(foe),
                FormationShape::Ring {
                    radius: 2.0,
                    spacing: 2.0,
                },
            ),
        ))
        .id();
    for i in 0..9 {
        body(&mut app, Some(squad), Vec3::new(i as f32, 0.0, 5.0));
    }
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for tick in 0..120 {
        app.world_mut().get_mut::<Transform>(foe).expect("t").translation =
            Vec3::new((tick as f32 * 0.1).sin() * 4.0, 0.0, 0.0);
        step(&mut app, 1);
        for slot in &app.world().get::<AiFormation>(squad).expect("formation").slots {
            fnv(&mut hash, &slot.index.to_le_bytes());
            fnv(&mut hash, &slot.position.x.to_bits().to_le_bytes());
            fnv(&mut hash, &slot.position.z.to_bits().to_le_bytes());
        }
    }
    hash
}

#[test]
fn two_formation_runs_agree() {
    assert_eq!(formation_hash(), formation_hash());
}

#[test]
fn slots_near_a_wall_do_not_collapse_onto_one_place() {
    // A floor only for z >= 0: the half of a ring on z < 0 projects onto the edge, where several places
    // land on the same spot. They must not become one slot.
    let mut app = app();
    let cell = 2.0f32;
    let mut triangles = Vec::new();
    for ix in -8..8 {
        for iz in 0..8 {
            let (x, z) = (ix as f32 * cell, iz as f32 * cell);
            let a = Vec3::new(x, 0.0, z);
            let b = Vec3::new(x + cell, 0.0, z);
            let c = Vec3::new(x + cell, 0.0, z + cell);
            let d = Vec3::new(x, 0.0, z + cell);
            triangles.push([a, b, c]);
            triangles.push([a, c, d]);
        }
    }
    app.insert_resource(NavMeshBuilder::default().build(&triangles).expect("builds"));
    let foe = body(&mut app, None, Vec3::new(0.0, 0.0, 0.5));
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation {
                extents: Vec3::new(6.0, 2.0, 6.0),
                min_separation: 1.0,
                ..AiFormation::new(
                    FormationAnchor::Entity(foe),
                    FormationShape::Ring {
                        radius: 3.0,
                        spacing: 1.5,
                    },
                )
            },
        ))
        .id();
    for i in 0..10 {
        body(&mut app, Some(squad), Vec3::new(i as f32 - 5.0, 0.0, 8.0));
    }
    step(&mut app, 1);
    assert_eq!(
        assert_unique(&app, squad),
        10,
        "outer rings make up for the blocked half"
    );
    assert!(
        app.world()
            .get::<AiFormation>(squad)
            .expect("f")
            .unplaced
            .is_empty()
    );
}

#[test]
fn a_member_keeps_its_slot_and_a_free_one_goes_to_the_nearest() {
    let mut app = app();
    let foe = body(&mut app, None, Vec3::ZERO);
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation {
                interval_steps: 1,
                ..AiFormation::new(
                    FormationAnchor::Entity(foe),
                    FormationShape::Ring {
                        radius: 4.0,
                        spacing: 4.0,
                    },
                )
            },
        ))
        .id();
    let east = body(&mut app, Some(squad), Vec3::new(10.0, 0.0, 0.0));
    let west = body(&mut app, Some(squad), Vec3::new(-10.0, 0.0, 0.0));
    step(&mut app, 1);
    let slot_of = |app: &App, member: Entity| {
        app.world()
            .get::<AiFormation>(squad)
            .expect("formation")
            .slots
            .iter()
            .find(|s| s.member == member)
            .copied()
    };
    let first_east = slot_of(&app, east).expect("placed");
    let first_west = slot_of(&app, west).expect("placed");
    assert!(first_east.position.x > 0.0, "{first_east:?}");
    assert!(first_west.position.x < 0.0, "{first_west:?}");
    // Walking across does not make them swap: the slot is theirs.
    app.world_mut().get_mut::<Transform>(east).expect("t").translation = Vec3::new(-20.0, 0.0, 0.0);
    step(&mut app, 3);
    assert_eq!(slot_of(&app, east).map(|s| s.index), Some(first_east.index));
    assert_eq!(slot_of(&app, west).map(|s| s.index), Some(first_west.index));
}

#[test]
fn the_innermost_ring_fills_first_however_far_away_the_members_come_from() {
    let mut app = app();
    let foe = body(&mut app, None, Vec3::ZERO);
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation::new(
                FormationAnchor::Entity(foe),
                FormationShape::Ring {
                    radius: 4.0,
                    spacing: 4.0,
                },
            ),
        ))
        .id();
    // The first ring has 6 places. All 7 members come from far off, nearer the outer ring.
    for i in 0..7 {
        body(&mut app, Some(squad), Vec3::new(100.0, 0.0, i as f32));
    }
    step(&mut app, 1);
    assert_eq!(assert_unique(&app, squad), 7);
    let formation = app.world().get::<AiFormation>(squad).expect("f");
    let inner = formation
        .slots
        .iter()
        .filter(|s| s.position.length() < 4.5)
        .count();
    assert_eq!(inner, 6, "{:?}", formation.slots);
}

#[test]
fn a_leader_anchored_formation_gives_the_leader_no_slot_and_follows_a_new_leader() {
    let mut app = app();
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation {
                interval_steps: 1,
                ..AiFormation::new(
                    FormationAnchor::Leader,
                    FormationShape::Line {
                        spacing: 2.0,
                        row_length: 3,
                    },
                )
            },
        ))
        .id();
    let leader = body(&mut app, Some(squad), Vec3::ZERO);
    let a = body(&mut app, Some(squad), Vec3::new(1.0, 0.0, 0.0));
    let b = body(&mut app, Some(squad), Vec3::new(2.0, 0.0, 0.0));
    let c = body(&mut app, Some(squad), Vec3::new(3.0, 0.0, 0.0));
    step(&mut app, 1);
    let slots = |app: &App| app.world().get::<AiFormation>(squad).expect("f").slots.clone();
    assert_eq!(slots(&app).len(), 3);
    assert!(slots(&app).iter().all(|s| s.member != leader));
    assert!(!board(&app, leader).contains("formation_slot"));
    // The default forward is -Z, so the rows are behind it, on +Z.
    assert!(
        slots(&app).iter().all(|s| s.position.z > 0.0),
        "{:?}",
        slots(&app)
    );

    // Kill the leader: the heir stops having a slot, the rest stand around it, the same step's resolve.
    app.world_mut().despawn(leader);
    let heir = app
        .world()
        .get::<AiSquad>(squad)
        .expect("squad")
        .leader
        .expect("elected");
    assert_eq!(heir, a);
    step(&mut app, 1);
    let after = slots(&app);
    assert_eq!(after.len(), 2);
    assert!(after.iter().all(|s| s.member == b || s.member == c));
    assert!(!board(&app, heir).contains("formation_slot"));
}

#[test]
fn a_member_that_goes_away_frees_its_slot_at_once() {
    let mut app = app();
    let foe = body(&mut app, None, Vec3::ZERO);
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation {
                interval_steps: 1000,
                ..AiFormation::new(
                    FormationAnchor::Entity(foe),
                    FormationShape::Wedge { spacing: 2.0 },
                )
            },
        ))
        .id();
    let a = body(&mut app, Some(squad), Vec3::new(1.0, 0.0, 0.0));
    let b = body(&mut app, Some(squad), Vec3::new(2.0, 0.0, 0.0));
    step(&mut app, 1);
    assert_eq!(assert_unique(&app, squad), 2);
    app.world_mut().entity_mut(a).insert(AiDead);
    let formation = app.world().get::<AiFormation>(squad).expect("f");
    assert_eq!(formation.slots.len(), 1);
    assert!(
        formation.dirty,
        "the next step resolves again, whatever the interval"
    );
    assert!(!board(&app, a).contains("formation_slot"));
    assert_eq!(formation.slots[0].member, b);
}

#[test]
fn an_anchor_that_is_gone_or_dead_empties_the_slots() {
    let mut app = app();
    let foe = body(&mut app, None, Vec3::ZERO);
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation {
                interval_steps: 1,
                ..AiFormation::new(FormationAnchor::Entity(foe), FormationShape::default())
            },
        ))
        .id();
    let a = body(&mut app, Some(squad), Vec3::new(1.0, 0.0, 0.0));
    step(&mut app, 1);
    assert!(board(&app, a).contains("formation_slot"));
    app.world_mut().entity_mut(foe).insert(AiDead);
    step(&mut app, 1);
    let formation = app.world().get::<AiFormation>(squad).expect("f");
    assert!(formation.slots.is_empty());
    assert_eq!(formation.unplaced, vec![a]);
    assert!(!board(&app, a).contains("formation_slot"));
    assert!(!board(&app, a).contains("formation_slot_index"));
    app.world_mut().despawn(foe);
    step(&mut app, 1);
    assert!(app.world().get::<AiFormation>(squad).expect("f").slots.is_empty());
}

#[test]
fn require_path_skips_places_on_an_island() {
    // Two floors with a gap: a ring around the anchor on the left reaches the right floor too.
    let mut app = app();
    let quad = |x0: f32, x1: f32| {
        let a = Vec3::new(x0, 0.0, -10.0);
        let b = Vec3::new(x1, 0.0, -10.0);
        let c = Vec3::new(x1, 0.0, 10.0);
        let d = Vec3::new(x0, 0.0, 10.0);
        [[a, b, c], [a, c, d]]
    };
    let mut triangles = Vec::new();
    triangles.extend(quad(-10.0, 1.0));
    triangles.extend(quad(3.0, 10.0));
    app.insert_resource(NavMeshBuilder::default().build(&triangles).expect("builds"));
    let foe = body(&mut app, None, Vec3::ZERO);
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation {
                require_path: true,
                interval_steps: 1,
                ..AiFormation::new(
                    FormationAnchor::Entity(foe),
                    FormationShape::Ring {
                        radius: 4.0,
                        spacing: 1.0,
                    },
                )
            },
        ))
        .id();
    for i in 0..8 {
        body(&mut app, Some(squad), Vec3::new(-5.0, 0.0, i as f32));
    }
    step(&mut app, 1);
    assert_eq!(assert_unique(&app, squad), 8);
    let formation = app.world().get::<AiFormation>(squad).expect("f");
    assert!(
        formation.slots.iter().all(|s| s.position.x <= 1.0 + 1e-3),
        "{:?}",
        formation.slots
    );
}

#[test]
fn resolving_by_hand_and_a_shape_with_bad_numbers_never_panic() {
    let mut app = app();
    let foe = body(&mut app, None, Vec3::ZERO);
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation::new(
                FormationAnchor::Entity(foe),
                FormationShape::Ring {
                    radius: f32::NAN,
                    spacing: 0.0,
                },
            ),
        ))
        .id();
    let a = body(&mut app, Some(squad), Vec3::new(1.0, 0.0, 0.0));
    resolve_formation(app.world_mut(), squad);
    // A NaN radius counts as no radius: the first place is the anchor itself.
    assert_eq!(assert_unique(&app, squad), 1);
    assert_eq!(
        app.world().get::<AiFormation>(squad).expect("f").slots[0].member,
        a
    );
    // No formation, no squad: nothing to do.
    resolve_formation(app.world_mut(), foe);
}

/// Deep-zai lower note (`formation.rs` numerics): a non-finite `spacing` or `radius` never
/// panics and never puts a non-finite position in a slot or on a member's blackboard. The
/// shape sanitizes on its own: a non-finite or non-positive `spacing` plays as 1.0
/// (`candidates`), a non-finite `radius` yields no finite place and the member goes unplaced.
#[test]
fn non_finite_formation_spacing_and_radius_stay_finite() {
    for (radius, spacing) in [
        (f32::NAN, f32::NAN),
        (f32::INFINITY, 1.0),
        (f32::NEG_INFINITY, f32::INFINITY),
        (3.0, f32::NAN),
        (3.0, f32::INFINITY),
    ] {
        let mut app = app();
        let foe = body(&mut app, None, Vec3::ZERO);
        let squad = app
            .world_mut()
            .spawn((
                AiSquad::new("alpha"),
                AiFormation::new(
                    FormationAnchor::Entity(foe),
                    FormationShape::Ring { radius, spacing },
                ),
            ))
            .id();
        let member = body(&mut app, Some(squad), Vec3::new(1.0, 0.0, 0.0));
        resolve_formation(app.world_mut(), squad);
        let formation = app.world().get::<AiFormation>(squad).expect("formation");
        for slot in &formation.slots {
            assert!(
                slot.position.is_finite(),
                "radius {radius} spacing {spacing} made {:?}",
                slot.position
            );
        }
        // A NaN spacing reads as 1.0 and a NaN or negative-infinite radius as 0.0, so places
        // still exist; only a positive-infinite radius has no finite place at all.
        if radius == f32::INFINITY {
            assert!(formation.slots.is_empty(), "radius {radius} placed somebody");
        } else {
            assert!(
                !formation.slots.is_empty(),
                "radius {radius} spacing {spacing} placed nobody"
            );
        }
        let board = app.world().get::<Blackboard>(member).expect("blackboard");
        if let Some(value) = board.get("formation_slot") {
            let BbValue::Vec3(at) = value else {
                panic!("the slot key is a vec3, got {value:?}");
            };
            assert!(at.is_finite(), "radius {radius} spacing {spacing}");
        }
    }
}

/// Finding 8 (audit G): ring candidates came from the platform libm `sin`/`cos`, whose last bit
/// differs between platforms, and the offsets are written to members' blackboards and projected
/// onto the navmesh. These are the deterministic kernel's bits for the nine-place ring.
#[test]
fn ring_slots_match_pinned_det_sin_cos_bits() {
    // [x bits, z bits] of the ring places at radius 3, angle i * TAU / 9
    // (`regia_core::math::det_sin_cos`), from `FormationShape::Ring { radius: 3.0, spacing: 2.0 }`.
    const PINNED: [[u32; 2]; 9] = [
        [0x4040_0000, 0x0000_0000],
        [0x4013_149e, 0x3ff6_d498],
        [0x3f05_5c9b, 0x403d_1546],
        [0xbfc0_0002, 0x4026_46e1],
        [0xc034_6bc6, 0x3f83_55ef],
        [0xc034_6bc5, 0xbf83_55f9],
        [0xbfbf_fffe, 0xc026_46e2],
        [0x3f05_5cb6, 0xc03d_1544],
        [0x4013_14a0, 0xbff6_d492],
    ];
    let mut app = app();
    let foe = body(&mut app, None, Vec3::ZERO);
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation::new(
                FormationAnchor::Entity(foe),
                FormationShape::Ring {
                    radius: 3.0,
                    spacing: 2.0,
                },
            ),
        ))
        .id();
    for i in 0..9 {
        body(&mut app, Some(squad), Vec3::new(i as f32, 0.0, 5.0));
    }
    resolve_formation(app.world_mut(), squad);
    let formation = app.world().get::<AiFormation>(squad).expect("formation");
    assert_eq!(
        formation.slots.len(),
        9,
        "{:?}",
        formation.slots.iter().map(|s| s.index).collect::<Vec<_>>()
    );
    let mut slots: Vec<(u32, Vec3)> = formation
        .slots
        .iter()
        .map(|slot| (slot.index, slot.position))
        .collect();
    slots.sort_by_key(|(index, _)| *index);
    for ((index, position), [x_bits, z_bits]) in slots.iter().zip(PINNED) {
        assert_eq!(position.x.to_bits(), x_bits, "slot {index}");
        assert_eq!(position.z.to_bits(), z_bits, "slot {index}");
        assert_eq!(
            position.y.to_bits(),
            0u32,
            "slot {index} stays on the ground plane"
        );
    }
}
