// SPDX-License-Identifier: MIT-0
//! The enemy: a navmesh agent steered by a behavior tree (`regia_ai`), with a health bar.
//!
//! Three pieces work together:
//!
//! - **Navigation** (`regia_nav`): [`setup`] bakes the level's static box colliders into a
//!   [`NavMesh`] and adds a [`NavCrowd`]. The enemy is one crowd agent; the crowd walks it around
//!   the pillars. [`sync_from_crowd`] copies the agent's position onto the enemy's transform.
//! - **Brain** (`regia_ai`): [`TREE`] is a `.bt.ron` behavior tree. The `Sense` service writes
//!   `alert` and `in_range` into the blackboard from the distance to the player; the selector under
//!   it attacks when in range, chases when alert, and idles otherwise. The leaf tasks `Chase`,
//!   `Attack` and `Idle` are plain Rust types registered by name. The tree text is a string here
//!   so the template has no asset to load; move it to a `.bt.ron` file once you edit it a lot.
//! - **Health bar**: a flat box above the enemy, scaled by its [`Health`] fraction.
//!
//! Whether the enemy is defeated rides in [`CombatProgress`], a saved resource, and
//! [`reconcile`] makes the world match it after a load.

use regia::prelude::*;
use regia_ai::{AiStep, BehaviorTree, BehaviorTreeDef, Blackboard, BtContext, BtStatus, BtTask};
use regia_core::math::{Quat, Vec3};
use regia_framework::camera::CameraRig;
use regia_framework::character::Character;
use regia_framework::save_game::SaveableResource;
use regia_nav::{
    CrowdAgentId, CrowdAgentParams, NavCrowd, NavCrowdConfig, NavCrowdStep, NavMesh, NavMeshBuilder,
};
use regia_physics::{BodyKind, Collider, ColliderShape, RigidBody};
use regia_render::MeshRenderer;
use regia_scene::{GlobalTransform, Transform, components::Name};

use crate::combat::{DamageEvent, Died, Health};
use crate::level::{cube_mesh, solid};

/// Hit points of the enemy.
pub const ENEMY_HEALTH: f32 = 36.0;
/// Where the enemy stands at the start (on the navmesh).
pub const POST: Vec3 = Vec3::new(0.0, 0.0, -12.0);
/// How far the enemy notices the player (meters).
pub const SIGHT: f32 = 10.0;
/// How far the enemy chases before it gives up (meters).
pub const GIVE_UP: f32 = 16.0;
/// Distance at which it attacks (meters).
pub const STRIKE_RANGE: f32 = 1.6;
/// Chase speed (meters per second).
pub const CHASE_SPEED: f32 = 3.2;
/// Hit points one blow takes from the player.
pub const STRIKE_DAMAGE: f32 = 10.0;
/// Seconds from the start of an attack to the blow.
const WINDUP: f32 = 0.5;
/// Seconds from the start of an attack to the end of its recovery.
const ATTACK_TOTAL: f32 = 1.2;
/// Height of the enemy's center above the navmesh surface (meters).
const CENTER_HEIGHT: f32 = 0.8;

/// The behavior tree, `.bt.ron` v1. Node indices in `children` point into `nodes`.
pub const TREE: &str = r#"(
    root: 0,
    nodes: [
        (name: "sense", kind: service, children: [1], key: "", seconds: 0.0, times: None, task: "{{crate_name}}::enemy::Sense", policy: require_all),
        (name: "brain", kind: selector, children: [2, 8], key: "", seconds: 0.0, times: None, task: "", policy: require_all),
        (name: "engage_if_alert", kind: sequence, children: [3, 4], key: "", seconds: 0.0, times: None, task: "", policy: require_all),
        (name: "alert", kind: condition, children: [], key: "alert", seconds: 0.0, times: None, task: "", policy: require_all),
        (name: "fight", kind: selector, children: [5, 7], key: "", seconds: 0.0, times: None, task: "", policy: require_all),
        (name: "attack_if_in_range", kind: sequence, children: [6, 9], key: "", seconds: 0.0, times: None, task: "", policy: require_all),
        (name: "in_range", kind: condition, children: [], key: "in_range", seconds: 0.0, times: None, task: "", policy: require_all),
        (name: "chase", kind: leaf, children: [], key: "", seconds: 0.0, times: None, task: "{{crate_name}}::enemy::Chase", policy: require_all),
        (name: "idle", kind: leaf, children: [], key: "", seconds: 0.0, times: None, task: "{{crate_name}}::enemy::Idle", policy: require_all),
        (name: "attack", kind: leaf, children: [], key: "", seconds: 0.0, times: None, task: "{{crate_name}}::enemy::Attack", policy: require_all),
    ],
)"#;

/// Marks the enemy and remembers where it stands when idle.
#[derive(Component, Debug, Clone, Copy)]
pub struct Enemy {
    /// The enemy's post (ground position).
    pub home: Vec3,
}

/// The enemy's crowd agent.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrowdLink(pub CrowdAgentId);

/// The goal last sent to the crowd, so the leaves re-plan only when it moves.
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct NavGoal(pub Option<Vec3>);

/// The bar floating over `owner`.
#[derive(Component, Debug, Clone, Copy)]
pub struct HealthBar {
    /// The enemy it shows.
    pub owner: Entity,
}

/// Whether the enemy was defeated, how often the player died and the player's health. Saved with
/// the game; `lib.rs` keeps `health` in step with the player.
#[derive(Resource, Reflect, Debug, Clone, PartialEq)]
#[reflect(Resource, Default)]
pub struct CombatProgress {
    /// The enemy is dead and stays dead until a load says otherwise.
    pub enemy_defeated: bool,
    /// How many times the player died.
    pub deaths: u32,
    /// The player's hit points.
    pub health: f32,
}

impl Default for CombatProgress {
    fn default() -> Self {
        Self {
            enemy_defeated: false,
            deaths: 0,
            health: crate::combat::PLAYER_HEALTH,
        }
    }
}

impl SaveableResource for CombatProgress {}

/// Installs the AI and crowd plugins and registers the tree's leaf tasks.
pub struct EnemyPlugin;

impl Plugin for EnemyPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<regia_ai::AiPlugin>()) {
            app.add_plugins(regia_ai::AiPlugin);
        }
        if !app.has_plugin(std::any::type_name::<regia_nav::NavCrowdPlugin>()) {
            app.add_plugins(regia_nav::NavCrowdPlugin);
        }
        {
            let mut registry = app
                .world_mut()
                .get_resource_or_insert_with::<regia_ai::BtTaskRegistry>(Default::default);
            registry.register::<Sense>();
            registry.register::<Chase>();
            registry.register::<Idle>();
            registry.register::<Attack>();
        }
        // The brain thinks first, then the crowd moves, then the transform follows the agent.
        app.configure_sets(FixedUpdate, NavCrowdStep.after(AiStep))
            .add_systems(FixedUpdate, sync_from_crowd.after(NavCrowdStep));
    }
}

/// `Startup` (after the level): bakes the navmesh, creates the crowd and spawns the enemy.
pub fn setup(world: &mut World) {
    let triangles = triangles_from_colliders(world);
    let mesh = match NavMeshBuilder::default().build(&triangles) {
        Ok(mesh) => mesh,
        Err(err) => {
            error!("navmesh build failed: {err}; no enemy");
            return;
        }
    };
    let hz = world.get_resource::<FixedTime>().map_or(60.0, |fixed| fixed.hz);
    let config = NavCrowdConfig {
        step_seconds: (1.0 / hz) as f32,
        max_agents: 8,
        ..NavCrowdConfig::default()
    };
    match NavCrowd::new(config) {
        Ok(crowd) => {
            world.insert_resource(crowd);
        }
        Err(err) => {
            error!("nav crowd: {err}; no enemy");
            return;
        }
    }
    world.insert_resource(mesh);
    reconcile(world);
}

/// Every static box collider as triangles (what `NavMeshBuilder` consumes).
fn triangles_from_colliders(world: &mut World) -> Vec<[Vec3; 3]> {
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
            for [a, b, d, e] in [
                [0, 1, 2, 3],
                [4, 7, 6, 5],
                [0, 3, 7, 4],
                [1, 5, 6, 2],
                [0, 4, 5, 1],
                [3, 2, 6, 7],
            ] {
                triangles.push([c[a], c[b], c[d]]);
                triangles.push([c[a], c[d], c[e]]);
            }
        }
    }
    triangles
}

/// Spawns the enemy when it is not defeated and not alive; removes it when it is defeated and
/// alive (a load can do either).
pub fn reconcile(world: &mut World) {
    let defeated = world
        .get_resource::<CombatProgress>()
        .is_some_and(|p| p.enemy_defeated);
    let living: Option<Entity> = world.query_filtered::<Entity, With<Enemy>>().iter(world).next();
    match (defeated, living) {
        (true, Some(enemy)) => remove_enemy(world, enemy),
        (false, None) if spawn_enemy(world).is_none() => {
            warn!("the enemy could not be spawned (no navmesh?)");
        }
        _ => {}
    }
}

/// `Update`: runs [`reconcile`] when the progress changed (the enemy fell, or a load).
pub fn reconcile_on_change(world: &mut World, mut seen: Local<Option<CombatProgress>>) {
    let now = world.get_resource::<CombatProgress>().cloned();
    if *seen != now {
        *seen = now;
        reconcile(world);
    }
}

/// Spawns the enemy with its agent, tree, health and bar. `None` without a navmesh or a place on it.
fn spawn_enemy(world: &mut World) -> Option<Entity> {
    let def: BehaviorTreeDef = match ron::from_str(TREE) {
        Ok(def) => def,
        Err(err) => {
            error!("enemy tree: {err}");
            return None;
        }
    };
    let tree = match BehaviorTree::compile(&def, world.get_resource::<regia_ai::BtTaskRegistry>()?) {
        Ok(tree) => tree,
        Err(err) => {
            error!("enemy tree: {err}");
            return None;
        }
    };
    if !world.contains_resource::<NavCrowd>() || !world.contains_resource::<NavMesh>() {
        return None;
    }
    let params = CrowdAgentParams::new(0.35, 1.6, CHASE_SPEED, 8.0);
    let agent = world
        .resource_scope(|world, mut crowd: Mut<NavCrowd>| {
            crowd.add_agent(world.resource::<NavMesh>(), POST, params)
        })
        .ok()?;
    let position = world
        .get_resource::<NavCrowd>()
        .and_then(|crowd| crowd.agent(agent))
        .map_or(POST, |a| a.position())
        + Vec3::new(0.0, CENTER_HEIGHT, 0.0);
    let start = Transform::from_translation(position);
    let enemy = world
        .spawn((
            Name::new("Enemy"),
            start,
            GlobalTransform(start.compute_affine()),
            Enemy { home: POST },
            Health::new(ENEMY_HEALTH),
            Blackboard::new(),
            tree,
            CrowdLink(agent),
            NavGoal::default(),
        ))
        .id();
    for (name, at, half, color) in [
        (
            "Body",
            Vec3::ZERO,
            Vec3::new(0.32, 0.8, 0.25),
            [0.75, 0.2, 0.18, 1.0],
        ),
        (
            "Head",
            Vec3::new(0.0, 0.95, -0.05),
            Vec3::splat(0.2),
            [0.3, 0.1, 0.1, 1.0],
        ),
    ] {
        world.spawn((
            Name::new(name),
            Transform::from_translation(at),
            GlobalTransform(Transform::from_translation(at).compute_affine()),
            MeshRenderer::new(std::sync::Arc::new(cube_mesh(half)), solid(color)),
            ChildOf(enemy),
        ));
    }
    let bar_at = position + Vec3::new(0.0, 1.6, 0.0);
    world.spawn((
        Name::new("EnemyHealthBar"),
        Transform::from_translation(bar_at),
        GlobalTransform(Transform::from_translation(bar_at).compute_affine()),
        MeshRenderer::new(
            std::sync::Arc::new(cube_mesh(Vec3::new(0.5, 0.06, 0.04))),
            solid([0.9, 0.15, 0.15, 1.0]),
        ),
        HealthBar { owner: enemy },
    ));
    Some(enemy)
}

/// Removes the enemy: its crowd agent, its bar and the entity with its parts.
fn remove_enemy(world: &mut World, enemy: Entity) {
    if let Some(CrowdLink(id)) = world.get::<CrowdLink>(enemy).copied()
        && let Some(mut crowd) = world.get_resource_mut::<NavCrowd>()
    {
        let _ = crowd.remove_agent(id);
    }
    let bars: Vec<Entity> = world
        .query::<(Entity, &HealthBar)>()
        .iter(world)
        .filter(|(_, bar)| bar.owner == enemy)
        .map(|(entity, _)| entity)
        .collect();
    for bar in bars {
        let _ = world.try_despawn(bar);
    }
    let _ = world.try_despawn(enemy);
}

/// `Update`: when the enemy dies it is recorded in [`CombatProgress`] (and [`reconcile_on_change`]
/// removes it).
pub fn handle_deaths(
    mut died: MessageReader<Died>,
    enemies: Query<(), With<Enemy>>,
    mut progress: ResMut<CombatProgress>,
) {
    for event in died.read() {
        if enemies.get(event.entity).is_ok() {
            progress.enemy_defeated = true;
            info!("enemy defeated");
        }
    }
}

/// What a bar reads about its owner: where it is and how hurt.
type OwnerRow = (&'static Transform, &'static Health);

/// `Update`: keeps each bar over its owner, scaled by the owner's health, facing the camera.
pub fn update_bars(
    mut commands: Commands,
    rigs: Query<&CameraRig>,
    owners: Query<OwnerRow, (With<Enemy>, Without<HealthBar>)>,
    mut bars: Query<(Entity, &HealthBar, &mut Transform), Without<Enemy>>,
) {
    let yaw = rigs.iter().next().map_or(0.0, |rig| rig.yaw);
    for (bar, link, mut transform) in &mut bars {
        let Ok((owner, health)) = owners.get(link.owner) else {
            commands.entity(bar).despawn();
            continue;
        };
        transform.translation = owner.translation + Vec3::new(0.0, 1.6, 0.0);
        transform.rotation = Quat::from_rotation_y(yaw);
        transform.scale = Vec3::new(health.fraction().max(0.001), 1.0, 1.0);
    }
}

/// Yaw that looks along `dir` (0 looks down -Z).
fn yaw_of(dir: Vec3) -> f32 {
    (-dir.x).atan2(-dir.z)
}

/// `FixedUpdate` (after the crowd step): the enemy's transform follows its agent and faces where
/// it walks.
pub fn sync_from_crowd(
    crowd: Option<Res<NavCrowd>>,
    mut enemies: Query<(&CrowdLink, &mut Transform), With<Enemy>>,
) {
    let Some(crowd) = crowd else {
        return;
    };
    for (link, mut transform) in &mut enemies {
        let Some(agent) = crowd.agent(link.0) else {
            continue;
        };
        transform.translation = agent.position() + Vec3::new(0.0, CENTER_HEIGHT, 0.0);
        let velocity = agent.velocity().with_y(0.0);
        if velocity.length() > 0.2 {
            transform.rotation = Quat::from_rotation_y(yaw_of(velocity));
        }
    }
}

/// The player's position, if any player is alive.
fn player_position(world: &mut World) -> Option<Vec3> {
    world
        .query_filtered::<(&Transform, &Health), With<Character>>()
        .iter(world)
        .find(|(_, health)| !health.is_dead())
        .map(|(transform, _)| transform.translation)
}

/// Sends the enemy's crowd agent to `goal` at `speed`; `None` stops it. Re-plans only when the
/// goal moved.
fn go_to(ctx: &mut BtContext<'_>, goal: Option<Vec3>, speed: f32) {
    let me = ctx.entity;
    let Some(CrowdLink(id)) = ctx.world.get::<CrowdLink>(me).copied() else {
        return;
    };
    let previous = ctx.world.get::<NavGoal>(me).and_then(|g| g.0);
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

/// Service: perception. Writes `alert`, `in_range` and `target_pos` every tick.
#[derive(Default, regia_ai::BtTask)]
pub struct Sense;

impl BtTask for Sense {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let center = ctx.position().unwrap_or(Vec3::ZERO);
        let was_alert = ctx.blackboard().is_some_and(|bb| bb.get_bool("alert"));
        let target = player_position(ctx.world);
        let distance = target.map_or(f32::MAX, |at| (at - center).with_y(0.0).length());
        // Notice inside SIGHT; once alert, keep hunting until the player is GIVE_UP away.
        let alert = distance < SIGHT || (was_alert && distance < GIVE_UP);
        let in_range = alert && distance <= STRIKE_RANGE;
        if let Some(mut bb) = ctx.blackboard_mut() {
            bb.insert("alert", alert);
            bb.insert("in_range", in_range);
            if let Some(at) = target {
                bb.insert("target_pos", at);
            }
        }
        BtStatus::Success
    }
}

/// Leaf: run at the player.
#[derive(Default, regia_ai::BtTask)]
pub struct Chase;

impl BtTask for Chase {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let Some(target) = ctx
            .blackboard()
            .and_then(|bb| bb.contains("target_pos").then(|| bb.get_vec3("target_pos")))
        else {
            return BtStatus::Failure;
        };
        go_to(ctx, Some(Vec3::new(target.x, 0.0, target.z)), CHASE_SPEED);
        BtStatus::Success
    }
}

/// Leaf: stand at the post.
#[derive(Default, regia_ai::BtTask)]
pub struct Idle;

impl BtTask for Idle {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let home = ctx.world.get::<Enemy>(ctx.entity).map(|enemy| enemy.home);
        go_to(ctx, home, 1.8);
        BtStatus::Success
    }
}

/// Leaf: stop, wind up, strike once, recover. Running until the recovery ends.
#[derive(Default, regia_ai::BtTask)]
pub struct Attack {
    /// Seconds into the attack.
    t: f32,
    /// The blow already landed.
    struck: bool,
}

impl BtTask for Attack {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        go_to(ctx, None, 1.0);
        self.t += ctx.dt;
        if self.t >= WINDUP && !self.struck {
            self.struck = true;
            let center = ctx.position().unwrap_or(Vec3::ZERO);
            let mut hits = Vec::new();
            let mut query = ctx
                .world
                .query_filtered::<(Entity, &Transform), With<Character>>();
            for (player, transform) in query.iter(ctx.world) {
                if (transform.translation - center).with_y(0.0).length() <= STRIKE_RANGE + 0.5 {
                    hits.push(player);
                }
            }
            for target in hits {
                ctx.world.write_message(DamageEvent {
                    target,
                    amount: STRIKE_DAMAGE,
                });
            }
        }
        if self.t >= ATTACK_TOTAL {
            self.t = 0.0;
            self.struck = false;
            BtStatus::Success
        } else {
            BtStatus::Running
        }
    }

    fn reset(&mut self, _ctx: &mut BtContext<'_>) {
        self.t = 0.0;
        self.struck = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tree_parses_and_its_leaves_are_registered() {
        let def: BehaviorTreeDef = ron::from_str(TREE).expect("the tree parses");
        let mut registry = regia_ai::BtTaskRegistry::default();
        registry.register::<Sense>();
        registry.register::<Chase>();
        registry.register::<Idle>();
        registry.register::<Attack>();
        BehaviorTree::compile(&def, &registry).expect("every leaf is registered");
    }

    #[test]
    fn facing_matches_the_move() {
        let yaw = yaw_of(-Vec3::Z);
        let forward = Quat::from_rotation_y(yaw) * -Vec3::Z;
        assert!((forward - -Vec3::Z).length() < 1e-4);
    }
}
