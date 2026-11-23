// SPDX-License-Identifier: MIT-0
//! Enemies (ADR-2596): a melee grunt and a ranged caster, each driven by a `.bt.ron` behavior
//! tree (`regia_ai`, `assets/ai/*.bt.ron`) and moved by a navmesh crowd (`regia_nav::NavCrowd`).
//!
//! The level's static colliders are baked into a `NavMesh` at startup; each enemy is a crowd
//! agent, and [`sync_from_crowd`] copies the agent's position onto its kinematic capsule after
//! every crowd step. The trees tick in `FixedUpdate` (`regia_ai::AiStep`), before the crowd step
//! (`regia_nav::NavCrowdStep`), so a target set by a leaf moves the agent in the same step.
//!
//! The `Sense` service implements notice and give-up with the engine's perception predicates:
//! `regia_ai::in_sight_cone` plus `regia_ai::line_of_sight` for sight, `regia_ai::can_hear` for
//! the player's footsteps and swings (`HearingEvent`s addressed to [`TEAM_ENEMY`]), and being hit.
//! An alert enemy that has not perceived the player for [`GIVE_UP_SECONDS`], or that is pulled
//! more than [`LEASH`] meters from its post, gives up and walks its patrol again.

use regia::prelude::*;
use regia_ai::{
    AiStep, BehaviorTree, BehaviorTreeDef, Blackboard, BtContext, BtStatus, BtTask, HearingEvent, Perception,
};
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_nav::{
    CrowdAgentId, CrowdAgentParams, NavCrowd, NavCrowdConfig, NavCrowdStep, NavMesh, NavMeshBuilder,
};
use regia_physics::queries::{QueryFilter, WorldQueries};
use regia_physics::{BodyKind, Collider, ColliderShape, RigidBody};
use regia_render::MeshRenderer;
use regia_scene::Transform;

use crate::combat::{self, Combatant, TEAM_ENEMY};

/// The grunt's tree.
pub const GRUNT_TREE_TEXT: &str = include_str!("../assets/ai/grunt.bt.ron");
/// The caster's tree.
pub const RANGED_TREE_TEXT: &str = include_str!("../assets/ai/ranged.bt.ron");

/// Seconds an alert enemy keeps hunting without perceiving the player.
pub const GIVE_UP_SECONDS: f32 = 4.0;
/// Farthest an enemy follows the player from its post (meters).
pub const LEASH: f32 = 20.0;
/// Height of the capsule center above the navmesh surface.
pub const CENTER_HEIGHT: f32 = 0.8;
/// Height of the eyes above the capsule center.
const EYE_HEIGHT: f32 = 0.6;

/// The two enemy kinds.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnemyKind {
    /// Melee grunt: walks up and swipes.
    #[default]
    Grunt,
    /// Ranged caster: keeps its distance and fires slow bolts.
    Ranged,
}

/// Per-kind tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    /// Walking speed on patrol (m/s).
    pub patrol_speed: f32,
    /// Running speed in a chase (m/s).
    pub chase_speed: f32,
    /// The attack starts within this distance (m).
    pub attack_range: f32,
    /// A chase stops this far from the player (m).
    pub keep_distance: f32,
    /// Sight radius (m).
    pub sight: f32,
    /// Sight half-angle (radians).
    pub half_angle: f32,
    /// Hearing radius (m).
    pub hearing: f32,
    /// Seconds from the start of an attack to the strike.
    pub windup: f32,
    /// Seconds after the strike before the next decision.
    pub recover: f32,
    /// Damage of one attack.
    pub damage: f32,
    /// Body color.
    pub color: [f32; 4],
    /// Attribute file.
    pub attrs: &'static str,
    /// Attack ability.
    pub ability: &'static str,
}

/// The tuning of `kind`.
#[must_use]
pub fn tuning(kind: EnemyKind) -> Tuning {
    match kind {
        EnemyKind::Grunt => Tuning {
            patrol_speed: 1.6,
            chase_speed: 3.4,
            attack_range: 1.8,
            keep_distance: 1.2,
            sight: 12.0,
            half_angle: 70f32.to_radians(),
            hearing: 7.0,
            windup: 0.4,
            recover: 0.35,
            damage: 14.0,
            color: [0.72, 0.2, 0.18, 1.0],
            attrs: combat::GRUNT_ATTRS,
            ability: combat::SWIPE_ABILITY,
        },
        EnemyKind::Ranged => Tuning {
            patrol_speed: 1.3,
            chase_speed: 2.8,
            attack_range: 11.0,
            keep_distance: 7.0,
            sight: 16.0,
            half_angle: 65f32.to_radians(),
            hearing: 9.0,
            windup: 0.55,
            recover: 0.4,
            damage: 10.0,
            color: [0.45, 0.25, 0.7, 1.0],
            attrs: combat::RANGED_ATTRS,
            ability: combat::BOLT_ABILITY,
        },
    }
}

/// One enemy of the level: id, kind, post and patrol route.
#[derive(Debug, Clone, PartialEq)]
pub struct EnemySpawn {
    /// Stable id (saved in `CombatProgress::defeated`).
    pub id: u32,
    /// Kind.
    pub kind: EnemyKind,
    /// Patrol waypoints on the ground (the first is the post).
    pub route: Vec<Vec3>,
}

/// The level's enemies.
#[must_use]
pub fn roster() -> Vec<EnemySpawn> {
    let g = 0.25;
    vec![
        EnemySpawn {
            id: 0,
            kind: EnemyKind::Grunt,
            route: vec![
                Vec3::new(-6.0, g, -16.0),
                Vec3::new(0.0, g, -18.0),
                Vec3::new(-3.0, g, -22.0),
            ],
        },
        EnemySpawn {
            id: 1,
            kind: EnemyKind::Ranged,
            route: vec![Vec3::new(-18.0, g, 14.0), Vec3::new(-22.0, g, 8.0)],
        },
        EnemySpawn {
            id: 2,
            kind: EnemyKind::Grunt,
            route: vec![
                Vec3::new(14.0, g, 16.0),
                Vec3::new(20.0, g, 14.0),
                Vec3::new(16.0, g, 22.0),
            ],
        },
    ]
}

/// The enemy component.
#[derive(Component, Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Component, Default)]
pub struct Enemy {
    /// Stable id.
    pub id: u32,
    /// Kind.
    pub kind: EnemyKind,
    /// Patrol route (ground points).
    pub route: Vec<Vec3>,
    /// Facing yaw (radians; 0 looks down −Z).
    pub yaw: f32,
}

impl Enemy {
    /// Unit facing vector on the ground plane.
    #[must_use]
    pub fn forward(&self) -> Vec3 {
        Vec3::new(-self.yaw.sin(), 0.0, -self.yaw.cos())
    }
}

/// What the brain is doing (reflected for agent checks and the debug HUD).
#[derive(Component, Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Component, Default)]
pub struct EnemyBrain {
    /// `patrol`, `chase`, `attack`, `hold` or `give_up`.
    pub state: String,
    /// Hunting the player.
    pub alert: bool,
    /// Times the enemy noticed a player.
    pub noticed: u32,
    /// Times it gave up.
    pub gave_up: u32,
    /// Strikes made.
    pub strikes: u32,
    /// Distance to the nearest player when last perceived (m).
    pub target_distance: f32,
    /// How it last noticed: `sight`, `hearing` or `hurt`.
    pub noticed_by: String,
}

/// The enemy's crowd agent.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrowdLink(pub CrowdAgentId);

/// The goal last sent to the crowd (so leaves re-plan only when it moves).
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct NavGoal(pub Option<Vec3>);

/// A body part of an enemy's visual (follows the enemy).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnemyVisual(pub Entity);

/// A ranged bolt in flight.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct Bolt {
    /// Who fired it.
    pub owner: Entity,
    /// Velocity (m/s).
    pub velocity: Vec3,
    /// Seconds left.
    pub ttl: f32,
    /// Damage on hit.
    pub damage: f32,
}

/// The compiled tree definitions.
#[derive(Resource, Clone, Default)]
pub struct EnemyTrees {
    /// The grunt's tree.
    pub grunt: Option<BehaviorTreeDef>,
    /// The caster's tree.
    pub ranged: Option<BehaviorTreeDef>,
    /// Parse or compile problems.
    pub errors: Vec<String>,
}

/// Installs the AI and crowd plugins, the trees' leaf tasks and the enemy systems.
pub struct EnemyPlugin;

impl Plugin for EnemyPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<regia_ai::AiPlugin>()) {
            app.add_plugins(regia_ai::AiPlugin);
        }
        if !app.has_plugin(std::any::type_name::<regia_nav::NavCrowdPlugin>()) {
            app.add_plugins(regia_nav::NavCrowdPlugin);
        }
        app.register_type::<Enemy>()
            .register_type::<EnemyKind>()
            .register_type::<EnemyBrain>()
            .register_type::<Bolt>()
            .configure_sets(FixedUpdate, NavCrowdStep.after(AiStep))
            .add_systems(
                FixedUpdate,
                (
                    sync_from_crowd.after(NavCrowdStep),
                    move_bolts.in_set(combat::CombatSet::Projectiles),
                ),
            )
            .add_systems(Update, follow_visuals);
        {
            let mut registry = app
                .world_mut()
                .get_resource_or_insert_with::<regia_ai::BtTaskRegistry>(Default::default);
            registry.register::<Sense>();
            registry.register::<Hold>();
            registry.register::<Patrol>();
            registry.register::<Chase>();
            registry.register::<MeleeAttack>();
            registry.register::<RangedAttack>();
        }
        let mut trees = EnemyTrees::default();
        for (name, text, slot) in [("grunt", GRUNT_TREE_TEXT, 0), ("ranged", RANGED_TREE_TEXT, 1)] {
            match ron::from_str::<BehaviorTreeDef>(text) {
                Ok(def) => {
                    if slot == 0 {
                        trees.grunt = Some(def);
                    } else {
                        trees.ranged = Some(def);
                    }
                }
                Err(err) => trees.errors.push(format!("ai/{name}.bt.ron: {err}")),
            }
        }
        for line in &trees.errors {
            tracing::error!(target: "third_person", "{line}");
        }
        app.insert_resource(trees);
    }
}

/// `Startup` (after the level): bakes the navmesh from the static colliders, creates the crowd
/// and spawns every enemy that is not defeated.
pub fn setup(world: &mut World) {
    let triangles = triangles_from_colliders(world);
    let mesh = match NavMeshBuilder::default().build(&triangles) {
        Ok(mesh) => mesh,
        Err(err) => {
            tracing::error!(target: "third_person", "navmesh build failed: {err}; no enemies");
            return;
        }
    };
    let hz = world.get_resource::<FixedTime>().map_or(60.0, |f| f.hz);
    let config = NavCrowdConfig {
        step_seconds: (1.0 / hz) as f32,
        max_agents: 32,
        ..NavCrowdConfig::default()
    };
    match NavCrowd::new(config) {
        Ok(crowd) => {
            world.insert_resource(crowd);
        }
        Err(err) => {
            tracing::error!(target: "third_person", "nav crowd: {err}");
            return;
        }
    }
    world.insert_resource(mesh);
    reconcile(world);
}

/// Spawns the roster's enemies that are neither defeated nor alive (after a load too).
pub fn reconcile(world: &mut World) {
    let defeated: Vec<u32> = world
        .get_resource::<crate::checkpoint::CombatProgress>()
        .map(|p| p.defeated.clone())
        .unwrap_or_default();
    let living: Vec<(Entity, u32)> = world
        .query::<(Entity, &Enemy)>()
        .iter(world)
        .map(|(e, enemy)| (e, enemy.id))
        .collect();
    for (entity, id) in &living {
        if defeated.contains(id) {
            remove_enemy(world, *entity);
        }
    }
    for spawn in roster() {
        if defeated.contains(&spawn.id) || living.iter().any(|(_, id)| *id == spawn.id) {
            continue;
        }
        if spawn_enemy(world, &spawn).is_none() {
            tracing::warn!(target: "third_person", "enemy {} did not spawn", spawn.id);
        }
    }
}

/// Spawns one enemy: capsule, attributes, tree, blackboard, perception and crowd agent.
pub fn spawn_enemy(world: &mut World, spawn: &EnemySpawn) -> Option<Entity> {
    let tune = tuning(spawn.kind);
    let post = *spawn.route.first()?;
    let def = {
        let trees = world.get_resource::<EnemyTrees>()?;
        match spawn.kind {
            EnemyKind::Grunt => trees.grunt.clone()?,
            EnemyKind::Ranged => trees.ranged.clone()?,
        }
    };
    let tree = match BehaviorTree::compile(&def, world.resource::<regia_ai::BtTaskRegistry>()) {
        Ok(tree) => tree,
        Err(err) => {
            tracing::error!(target: "third_person", "enemy tree: {err}");
            return None;
        }
    };
    if !world.contains_resource::<NavCrowd>() || !world.contains_resource::<NavMesh>() {
        return None;
    }
    let params = CrowdAgentParams::new(0.35, 1.6, tune.patrol_speed, 8.0);
    let agent = world.resource_scope(|world, mut crowd: Mut<NavCrowd>| {
        let mesh = world.resource::<NavMesh>();
        crowd.add_agent(mesh, post, params)
    });
    let agent = match agent {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(target: "third_person", "enemy {} off the navmesh: {err}", spawn.id);
            return None;
        }
    };
    let position = world
        .get_resource::<NavCrowd>()
        .and_then(|c| c.agent(agent))
        .map_or(post, |a| a.position());
    let enemy = world
        .spawn((
            Transform {
                translation: position + Vec3::new(0.0, CENTER_HEIGHT, 0.0),
                ..Default::default()
            },
            Enemy {
                id: spawn.id,
                kind: spawn.kind,
                route: spawn.route.clone(),
                yaw: 0.0,
            },
            EnemyBrain {
                state: "patrol".into(),
                ..Default::default()
            },
            Blackboard::new(),
            Perception {
                sight_radius: tune.sight,
                sight_half_angle: tune.half_angle,
                hearing_radius: tune.hearing,
                team: TEAM_ENEMY,
                ..Perception::default()
            },
            tree,
            CrowdLink(agent),
            NavGoal::default(),
            RigidBody {
                kind: BodyKind::Kinematic,
                ..Default::default()
            },
            Collider {
                shape: ColliderShape::Capsule {
                    half_height: 0.5,
                    radius: 0.32,
                    axis: regia_physics::ShapeAxis::Y,
                },
                ..Default::default()
            },
            regia_physics::NoInterpolation,
            Name::new(format!("enemy-{}", spawn.id)),
        ))
        .id();
    if combat::equip(world, enemy, tune.attrs, TEAM_ENEMY, tune.ability).is_none() {
        tracing::warn!(target: "third_person", "enemy {} has no combat data", spawn.id);
    }
    combat::add_tag(
        world,
        enemy,
        match spawn.kind {
            EnemyKind::Grunt => "Enemy.Grunt",
            EnemyKind::Ranged => "Enemy.Ranged",
        },
    );
    // Visual: a body box and a head box (the head marks the facing).
    for (offset, half, color) in [
        (Vec3::ZERO, Vec3::new(0.3, 0.75, 0.22), tune.color),
        (
            Vec3::new(0.0, 0.55, -0.2),
            Vec3::new(0.12, 0.08, 0.08),
            [0.95, 0.9, 0.3, 1.0],
        ),
    ] {
        world.spawn((
            Transform::from_translation(position + Vec3::new(0.0, CENTER_HEIGHT, 0.0) + offset),
            MeshRenderer::new(crate::level::box_mesh(half), crate::level::solid_material(color)),
            EnemyVisual(enemy),
            VisualOffset(offset),
        ));
    }
    Some(enemy)
}

/// Offset of a visual part from the enemy's capsule center (enemy-local).
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct VisualOffset(pub Vec3);

/// Despawns an enemy: its crowd agent, its visuals and the entity.
pub fn remove_enemy(world: &mut World, enemy: Entity) {
    if let Some(CrowdLink(id)) = world.get::<CrowdLink>(enemy).copied()
        && let Some(mut crowd) = world.get_resource_mut::<NavCrowd>()
    {
        let _ = crowd.remove_agent(id);
    }
    let visuals: Vec<Entity> = world
        .query::<(Entity, &EnemyVisual)>()
        .iter(world)
        .filter(|(_, v)| v.0 == enemy)
        .map(|(e, _)| e)
        .collect();
    for visual in visuals {
        let _ = world.try_despawn(visual);
    }
    let _ = world.try_despawn(enemy);
}

/// Extracts a triangle soup from every static box collider (sensors and moving bodies skipped).
pub fn triangles_from_colliders(world: &mut World) -> Vec<[Vec3; 3]> {
    let mut triangles = Vec::new();
    let mut query = world.query::<(&Collider, &RigidBody, &Transform)>();
    for (collider, body, transform) in query.iter(world) {
        if collider.sensor || body.kind != BodyKind::Static {
            continue;
        }
        let affine = transform.compute_affine() * collider.offset.compute_affine();
        if let ColliderShape::Box { half_extents } = &collider.shape {
            let h = *half_extents;
            let c = [
                Vec3::new(-h.x, -h.y, -h.z),
                Vec3::new(h.x, -h.y, -h.z),
                Vec3::new(h.x, -h.y, h.z),
                Vec3::new(-h.x, -h.y, h.z),
                Vec3::new(-h.x, h.y, -h.z),
                Vec3::new(h.x, h.y, -h.z),
                Vec3::new(h.x, h.y, h.z),
                Vec3::new(-h.x, h.y, h.z),
            ]
            .map(|p| affine.transform_point3(p));
            for [a, b, cc, d] in [
                [0, 1, 2, 3],
                [4, 7, 6, 5],
                [0, 3, 7, 4],
                [1, 5, 6, 2],
                [0, 4, 5, 1],
                [3, 2, 6, 7],
            ] {
                triangles.push([c[a], c[b], c[cc]]);
                triangles.push([c[a], c[cc], c[d]]);
            }
        }
    }
    triangles
}

/// Yaw that looks along `dir` (0 looks down −Z).
fn yaw_of(dir: Vec3) -> f32 {
    (-dir.x).atan2(-dir.z)
}

/// `FixedUpdate` (after the crowd step): each enemy's capsule follows its agent, and faces where
/// it walks (or its target while attacking).
#[tracing::instrument(skip_all, target = "third_person", name = "enemy_sync")]
pub fn sync_from_crowd(world: &mut World) {
    let links: Vec<(Entity, CrowdAgentId)> = world
        .query::<(Entity, &CrowdLink)>()
        .iter(world)
        .map(|(e, l)| (e, l.0))
        .collect();
    let Some(crowd) = world.get_resource::<NavCrowd>() else {
        return;
    };
    let moves: Vec<(Entity, Vec3, Vec3)> = links
        .into_iter()
        .filter_map(|(e, id)| crowd.agent(id).map(|a| (e, a.position(), a.velocity())))
        .collect();
    for (entity, position, velocity) in moves {
        let attacking = world
            .get::<EnemyBrain>(entity)
            .is_some_and(|b| b.state == "attack");
        let Some(mut enemy) = world.get_mut::<Enemy>(entity) else {
            continue;
        };
        let flat = Vec3::new(velocity.x, 0.0, velocity.z);
        if !attacking && flat.length() > 0.2 {
            enemy.yaw = yaw_of(flat);
        }
        let yaw = enemy.yaw;
        if let Some(mut transform) = world.get_mut::<Transform>(entity) {
            transform.translation = position + Vec3::new(0.0, CENTER_HEIGHT, 0.0);
            transform.rotation = Quat::from_rotation_y(yaw);
        }
    }
}

/// `Update`: visual parts follow their enemy; a staggered enemy squashes.
#[tracing::instrument(skip_all, target = "third_person", name = "enemy_visuals")]
pub fn follow_visuals(world: &mut World) {
    let parts: Vec<(Entity, Entity, Vec3)> = world
        .query::<(Entity, &EnemyVisual, &VisualOffset)>()
        .iter(world)
        .map(|(e, v, o)| (e, v.0, o.0))
        .collect();
    for (part, enemy, offset) in parts {
        let Some(t) = world.get::<Transform>(enemy).copied() else {
            continue;
        };
        // The hit reaction shows as a squash while `Status.Staggered` lasts.
        let scale = if combat::is_staggered(world, enemy) {
            Vec3::new(1.12, 0.86, 1.12)
        } else {
            Vec3::ONE
        };
        if let Some(mut transform) = world.get_mut::<Transform>(part) {
            transform.translation = t.translation + t.rotation * (offset * scale);
            transform.rotation = t.rotation;
            transform.scale = scale;
        }
    }
}

/// The nearest player target within `range` of `from`.
fn nearest_player(world: &mut World, from: Vec3, range: f32) -> Option<(Entity, Vec3, f32)> {
    combat::player_targets(world)
        .into_iter()
        .map(|(e, p)| (e, p, (p - from).length()))
        .filter(|(_, _, d)| *d <= range)
        .min_by(|a, b| a.2.total_cmp(&b.2))
}

/// Whether nothing blocks the line from `eye` to `target` (the target itself does not block).
fn clear_line(world: &World, eye: Vec3, target_pos: Vec3, me: Entity, target: Entity) -> bool {
    regia_ai::line_of_sight(world, eye, target_pos, &[me]).is_none_or(|hit| hit.entity == target)
}

/// Whether the game is in play (the brains idle on the title screen).
fn in_play(world: &World) -> bool {
    world
        .get_resource::<State<regia_framework::flow::GameFlow>>()
        .is_none_or(|s| *s.get() == regia_framework::flow::GameFlow::Playing)
}

/// Service: perception. Writes `disabled`, `alert`, `in_range` and `target_pos` every tick.
#[derive(Default, regia_ai::BtTask)]
pub struct Sense {
    /// Hearing events read so far.
    heard: MessageCursor<HearingEvent>,
    /// Hits read so far.
    hits: MessageCursor<combat::HitLanded>,
    /// Seconds since the player was last perceived.
    unseen: f32,
}

impl BtTask for Sense {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let me = ctx.entity;
        let Some(enemy) = ctx.world.get::<Enemy>(me).cloned() else {
            return BtStatus::Failure;
        };
        let Some(perception) = ctx.world.get::<Perception>(me).cloned() else {
            return BtStatus::Failure;
        };
        let tune = tuning(enemy.kind);
        let center = ctx.position().unwrap_or(Vec3::ZERO);
        let eye = center + Vec3::new(0.0, EYE_HEIGHT, 0.0);

        // Messages since the last tick (read every tick so none pile up).
        let heard: Vec<HearingEvent> = ctx
            .world
            .get_resource::<Messages<HearingEvent>>()
            .map(|m| self.heard.read(m).cloned().collect())
            .unwrap_or_default();
        let hurt = ctx
            .world
            .get_resource::<Messages<combat::HitLanded>>()
            .map(|m| self.hits.read(m).any(|h| h.target == me))
            .unwrap_or(false);

        let disabled =
            !in_play(ctx.world) || combat::is_down(ctx.world, me) || combat::is_staggered(ctx.world, me);
        let target = nearest_player(ctx.world, center, LEASH * 2.0);
        let mut noticed_by = None;
        if let Some((entity, pos, _)) = target {
            if regia_ai::in_sight_cone(
                eye,
                enemy.forward(),
                pos,
                perception.sight_half_angle,
                perception.sight_radius,
            ) && clear_line(ctx.world, eye, pos, me, entity)
            {
                noticed_by = Some("sight");
            } else if heard.iter().any(|h| regia_ai::can_hear(&perception, center, h)) {
                noticed_by = Some("hearing");
            } else if hurt {
                noticed_by = Some("hurt");
            }
        }
        let was_alert = ctx.blackboard().is_some_and(|bb| bb.get_bool("alert"));
        let home = enemy.route.first().copied().unwrap_or(center);
        let mut alert = was_alert;
        let mut gave_up = false;
        if let (Some(how), Some((_, pos, dist))) = (noticed_by, target) {
            self.unseen = 0.0;
            alert = true;
            if let Some(mut bb) = ctx.blackboard_mut() {
                bb.insert("target_pos", pos);
            }
            if let Some(mut brain) = ctx.world.get_mut::<EnemyBrain>(me) {
                brain.target_distance = dist;
                if !was_alert {
                    brain.noticed += 1;
                    brain.noticed_by = how.to_owned();
                }
            }
        } else if was_alert {
            self.unseen += ctx.dt;
            // Track the target's position for a short while after losing it (the chase goes to
            // the last place it was perceived, then the enemy gives up).
            let far_from_post =
                (Vec3::new(center.x, 0.0, center.z) - Vec3::new(home.x, 0.0, home.z)).length() > LEASH;
            if self.unseen > GIVE_UP_SECONDS || far_from_post || target.is_none() {
                alert = false;
                gave_up = true;
            }
        }
        if alert && let Some((_, pos, dist)) = target {
            // While alert the enemy keeps tracking the nearest player (it knows where the
            // fight is); losing perception only starts the give-up clock.
            if let Some(mut bb) = ctx.blackboard_mut() {
                bb.insert("target_pos", pos);
            }
            if let Some(mut brain) = ctx.world.get_mut::<EnemyBrain>(me) {
                brain.target_distance = dist;
            }
        }
        let in_range = alert
            && target.is_some_and(|(entity, pos, dist)| {
                dist <= tune.attack_range
                    && (enemy.kind == EnemyKind::Grunt || clear_line(ctx.world, eye, pos, me, entity))
            });
        if let Some(mut bb) = ctx.blackboard_mut() {
            bb.insert("disabled", disabled);
            bb.insert("alert", alert);
            bb.insert("in_range", in_range);
        }
        if gave_up && let Some(mut brain) = ctx.world.get_mut::<EnemyBrain>(me) {
            brain.gave_up += 1;
            brain.state = "give_up".into();
            brain.alert = false;
        }
        if let Some(mut brain) = ctx.world.get_mut::<EnemyBrain>(me) {
            brain.alert = alert;
        }
        BtStatus::Success
    }
}

/// Sends the enemy's crowd agent to `goal` at `speed` (re-plans only when the goal moved).
fn go_to(ctx: &mut BtContext<'_>, goal: Option<Vec3>, speed: f32) {
    let me = ctx.entity;
    let Some(CrowdLink(id)) = ctx.world.get::<CrowdLink>(me).copied() else {
        return;
    };
    let previous = ctx.world.get::<NavGoal>(me).and_then(|g| g.0);
    if !ctx.world.contains_resource::<NavCrowd>() {
        return;
    }
    let changed = match (previous, goal) {
        (Some(a), Some(b)) => (a - b).length() > 0.6,
        (None, None) => false,
        _ => true,
    };
    ctx.world.resource_scope(|world, mut crowd: Mut<NavCrowd>| {
        if let Some(agent) = crowd.agent(id)
            && (agent.params().max_speed - speed).abs() > 1e-3
        {
            let mut params = *agent.params();
            params.max_speed = speed;
            let _ = crowd.set_agent_params(id, params);
        }
        if !changed {
            return;
        }
        match goal {
            Some(goal) => {
                if let Some(mesh) = world.get_resource::<NavMesh>() {
                    let _ = crowd.set_target(mesh, id, goal);
                }
            }
            None => {
                let _ = crowd.clear_target(id);
            }
        }
    });
    if changed && let Some(mut g) = ctx.world.get_mut::<NavGoal>(me) {
        g.0 = goal;
    }
}

fn set_state(ctx: &mut BtContext<'_>, state: &str) {
    if let Some(mut brain) = ctx.world.get_mut::<EnemyBrain>(ctx.entity)
        && brain.state != state
    {
        state.clone_into(&mut brain.state);
    }
}

fn disabled(ctx: &BtContext<'_>) -> bool {
    ctx.blackboard().is_some_and(|bb| bb.get_bool("disabled"))
}

/// Leaf: stand still (staggered, dead or on the title screen).
#[derive(Default, regia_ai::BtTask)]
pub struct Hold;

impl BtTask for Hold {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        go_to(ctx, None, 0.5);
        set_state(ctx, "hold");
        BtStatus::Success
    }
}

/// Leaf: walk the patrol route.
#[derive(Default, regia_ai::BtTask)]
pub struct Patrol {
    /// The waypoint walked to.
    next: usize,
}

impl BtTask for Patrol {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        if disabled(ctx) {
            return BtStatus::Failure;
        }
        let Some(enemy) = ctx.world.get::<Enemy>(ctx.entity).cloned() else {
            return BtStatus::Failure;
        };
        if enemy.route.is_empty() {
            return BtStatus::Failure;
        }
        let ground = ctx.position().unwrap_or(Vec3::ZERO) - Vec3::new(0.0, CENTER_HEIGHT, 0.0);
        let mut goal = enemy.route[self.next % enemy.route.len()];
        let flat = |v: Vec3| Vec3::new(v.x, 0.0, v.z);
        if (flat(goal) - flat(ground)).length() < 0.7 {
            self.next = (self.next + 1) % enemy.route.len();
            goal = enemy.route[self.next];
        }
        go_to(ctx, Some(goal), tuning(enemy.kind).patrol_speed);
        set_state(ctx, "patrol");
        BtStatus::Success
    }
}

/// Leaf: run toward the player, stopping `keep_distance` short (the caster also stops once it
/// has a clear shot inside its range).
#[derive(Default, regia_ai::BtTask)]
pub struct Chase;

impl BtTask for Chase {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        if disabled(ctx) {
            return BtStatus::Failure;
        }
        let Some(kind) = ctx.world.get::<Enemy>(ctx.entity).map(|e| e.kind) else {
            return BtStatus::Failure;
        };
        let tune = tuning(kind);
        let Some(target) = ctx
            .blackboard()
            .and_then(|bb| bb.contains("target_pos").then(|| bb.get_vec3("target_pos")))
        else {
            return BtStatus::Failure;
        };
        let center = ctx.position().unwrap_or(Vec3::ZERO);
        let to = Vec3::new(target.x - center.x, 0.0, target.z - center.z);
        let dist = to.length();
        let goal = if dist > tune.keep_distance {
            let stop = target - to.normalize_or_zero() * tune.keep_distance;
            Some(Vec3::new(stop.x, center.y - CENTER_HEIGHT, stop.z))
        } else {
            None
        };
        go_to(ctx, goal, tune.chase_speed);
        if goal.is_none()
            && dist > 1e-3
            && let Some(mut enemy) = ctx.world.get_mut::<Enemy>(ctx.entity)
        {
            enemy.yaw = yaw_of(to);
        }
        set_state(ctx, "chase");
        BtStatus::Success
    }
}

/// Shared attack timing: wind up facing the target, strike once, recover.
#[derive(Default)]
struct AttackClock {
    /// Seconds into the current attack (`None` = not attacking).
    t: Option<f32>,
    /// Whether this attack struck already.
    struck: bool,
}

/// What an attack tick should do.
enum AttackStep {
    /// Keep going.
    Running,
    /// Strike now.
    Strike(Vec3),
    /// Finished (or refused).
    Done(BtStatus),
}

impl AttackClock {
    fn step(&mut self, ctx: &mut BtContext<'_>) -> AttackStep {
        if disabled(ctx) {
            self.t = None;
            return AttackStep::Done(BtStatus::Failure);
        }
        let Some(kind) = ctx.world.get::<Enemy>(ctx.entity).map(|e| e.kind) else {
            return AttackStep::Done(BtStatus::Failure);
        };
        let tune = tuning(kind);
        let target = ctx
            .blackboard()
            .map(|bb| bb.get_vec3("target_pos"))
            .unwrap_or(Vec3::ZERO);
        let center = ctx.position().unwrap_or(Vec3::ZERO);
        let to = Vec3::new(target.x - center.x, 0.0, target.z - center.z);
        if self.t.is_none() {
            // Start only when the attack ability could start (cooldown, stagger).
            if !combat::attack_ready(ctx.world, ctx.entity) {
                go_to(ctx, None, tune.chase_speed);
                if to.length() > 1e-3
                    && let Some(mut enemy) = ctx.world.get_mut::<Enemy>(ctx.entity)
                {
                    enemy.yaw = yaw_of(to);
                }
                set_state(ctx, "recover");
                return AttackStep::Done(BtStatus::Success);
            }
            self.t = Some(0.0);
            self.struck = false;
            go_to(ctx, None, tune.chase_speed);
        }
        set_state(ctx, "attack");
        if to.length() > 1e-3
            && let Some(mut enemy) = ctx.world.get_mut::<Enemy>(ctx.entity)
        {
            enemy.yaw = yaw_of(to);
        }
        let t = self.t.unwrap_or(0.0) + ctx.dt;
        self.t = Some(t);
        if !self.struck && t >= tune.windup {
            self.struck = true;
            if !combat::try_attack(ctx.world, ctx.entity) {
                self.t = None;
                return AttackStep::Done(BtStatus::Success);
            }
            if let Some(mut brain) = ctx.world.get_mut::<EnemyBrain>(ctx.entity) {
                brain.strikes += 1;
            }
            return AttackStep::Strike(target);
        }
        if t >= tune.windup + tune.recover {
            self.t = None;
            return AttackStep::Done(BtStatus::Success);
        }
        AttackStep::Running
    }
}

/// Leaf: the grunt's swipe — a sphere overlap in front of it at the strike.
#[derive(Default, regia_ai::BtTask)]
pub struct MeleeAttack {
    clock: AttackClock,
}

impl BtTask for MeleeAttack {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        match self.clock.step(ctx) {
            AttackStep::Running => BtStatus::Running,
            AttackStep::Done(status) => status,
            AttackStep::Strike(_) => {
                let me = ctx.entity;
                let Some(enemy) = ctx.world.get::<Enemy>(me).cloned() else {
                    return BtStatus::Failure;
                };
                let center = ctx.position().unwrap_or(Vec3::ZERO);
                let hits = sphere_hits(ctx.world, center + enemy.forward() * 1.0, 0.9, me);
                let damage = tuning(enemy.kind).damage;
                for target in hits {
                    if ctx
                        .world
                        .get::<Combatant>(target)
                        .is_some_and(|c| c.team == combat::TEAM_PLAYER)
                    {
                        combat::deal_damage(ctx.world, me, target, damage);
                    }
                }
                BtStatus::Running
            }
        }
    }

    fn reset(&mut self, _ctx: &mut BtContext<'_>) {
        self.clock = AttackClock::default();
    }
}

/// Leaf: the caster's bolt — a slow projectile toward the target at the strike.
#[derive(Default, regia_ai::BtTask)]
pub struct RangedAttack {
    clock: AttackClock,
}

impl BtTask for RangedAttack {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        match self.clock.step(ctx) {
            AttackStep::Running => BtStatus::Running,
            AttackStep::Done(status) => status,
            AttackStep::Strike(target) => {
                let me = ctx.entity;
                let damage = ctx.world.get::<Enemy>(me).map_or(10.0, |e| tuning(e.kind).damage);
                let eye = ctx.position().unwrap_or(Vec3::ZERO) + Vec3::new(0.0, 0.3, 0.0);
                let dir = (target - eye).normalize_or_zero();
                ctx.world.spawn((
                    Transform::from_translation(eye + dir * 0.6),
                    Bolt {
                        owner: me,
                        velocity: dir * 13.0,
                        ttl: 2.0,
                        damage,
                    },
                    MeshRenderer::new(
                        crate::level::box_mesh(Vec3::splat(0.12)),
                        crate::level::solid_material([1.0, 0.55, 0.95, 1.0]),
                    ),
                    Name::new("bolt"),
                ));
                BtStatus::Running
            }
        }
    }

    fn reset(&mut self, _ctx: &mut BtContext<'_>) {
        self.clock = AttackClock::default();
    }
}

/// Entities whose colliders overlap a sphere of `radius` at `center` (a physics shape query),
/// `exclude` left out. Empty without a physics world.
pub fn sphere_hits(world: &World, center: Vec3, radius: f32, exclude: Entity) -> Vec<Entity> {
    let Some(physics) = world.get_resource::<regia_physics::PhysicsWorld>() else {
        return Vec::new();
    };
    let queries = WorldQueries {
        world: physics,
        layers: world.get_resource::<regia_physics::PhysicsLayers>(),
    };
    let mut hits = queries.overlap(
        &ColliderShape::Sphere { radius },
        Transform::from_translation(center),
        QueryFilter::new().exclude_entities(&[exclude]),
    );
    hits.sort_unstable();
    hits.dedup();
    hits
}

/// `FixedUpdate`: bolts fly; a ray along each step's motion (a physics query) finds what they
/// hit. A player is damaged; anything else stops the bolt.
#[tracing::instrument(skip_all, target = "third_person", name = "enemy_bolts")]
pub fn move_bolts(world: &mut World) {
    let dt = world.resource::<Time>().delta_secs();
    let bolts: Vec<(Entity, Bolt, Vec3)> = world
        .query::<(Entity, &Bolt, &Transform)>()
        .iter(world)
        .map(|(e, b, t)| (e, *b, t.translation))
        .collect();
    for (entity, bolt, position) in bolts {
        let step = bolt.velocity * dt;
        let hit = world
            .get_resource::<regia_physics::PhysicsWorld>()
            .and_then(|physics| {
                WorldQueries {
                    world: physics,
                    layers: world.get_resource::<regia_physics::PhysicsLayers>(),
                }
                .raycast(
                    position,
                    step.normalize_or_zero(),
                    step.length(),
                    QueryFilter::new().exclude_entities(&[bolt.owner]),
                )
            });
        if let Some(hit) = hit {
            if world
                .get::<Combatant>(hit.entity)
                .is_some_and(|c| c.team == combat::TEAM_PLAYER)
            {
                combat::deal_damage(world, bolt.owner, hit.entity, bolt.damage);
            }
            let _ = world.try_despawn(entity);
            continue;
        }
        let ttl = bolt.ttl - dt;
        if ttl <= 0.0 {
            let _ = world.try_despawn(entity);
            continue;
        }
        if let Some(mut t) = world.get_mut::<Transform>(entity) {
            t.translation = position + step;
        }
        if let Some(mut b) = world.get_mut::<Bolt>(entity) {
            b.ttl = ttl;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> regia_ai::BtTaskRegistry {
        let mut registry = regia_ai::BtTaskRegistry::default();
        registry.register::<Sense>();
        registry.register::<Hold>();
        registry.register::<Patrol>();
        registry.register::<Chase>();
        registry.register::<MeleeAttack>();
        registry.register::<RangedAttack>();
        registry
    }

    #[test]
    fn both_trees_parse_and_compile_against_the_sample_leaves() {
        for text in [GRUNT_TREE_TEXT, RANGED_TREE_TEXT] {
            let def: BehaviorTreeDef = ron::from_str(text).expect("tree parses");
            assert_eq!(def.nodes.len(), 13);
            assert_eq!(def.nodes[def.root].kind, regia_ai::DefKind::Service);
            BehaviorTree::compile(&def, &registry()).expect("every leaf is registered");
        }
        // The two trees differ only in the attack leaf.
        let grunt: BehaviorTreeDef = ron::from_str(GRUNT_TREE_TEXT).expect("grunt");
        let ranged: BehaviorTreeDef = ron::from_str(RANGED_TREE_TEXT).expect("ranged");
        let attack = |d: &BehaviorTreeDef| {
            d.nodes
                .iter()
                .find(|n| n.name == "attack")
                .map(|n| n.task.clone())
        };
        assert!(attack(&grunt).is_some_and(|t| t.ends_with("MeleeAttack")));
        assert!(attack(&ranged).is_some_and(|t| t.ends_with("RangedAttack")));
    }

    #[test]
    fn yaw_and_forward_agree() {
        for dir in [Vec3::NEG_Z, Vec3::X, Vec3::new(-1.0, 0.0, 1.0).normalize()] {
            let enemy = Enemy {
                yaw: yaw_of(dir),
                ..Default::default()
            };
            assert!((enemy.forward() - dir).length() < 1e-5, "{dir:?}");
        }
    }

    #[test]
    fn the_roster_has_two_kinds_and_unique_ids() {
        let roster = roster();
        let mut ids: Vec<u32> = roster.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), roster.len());
        assert!(roster.iter().any(|s| s.kind == EnemyKind::Grunt));
        assert!(roster.iter().any(|s| s.kind == EnemyKind::Ranged));
        assert!(roster.iter().all(|s| !s.route.is_empty()));
    }
}
