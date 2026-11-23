// SPDX-License-Identifier: MIT-0
//! The player's melee combo (ADR-2596): three swings timed by a montage
//! (`assets/combat/combo.montage.ron`, `regia_animation::MontagePlayer`), gated by the `slash`
//! ability (spec 53: cooldown, no swing while staggered), with hit detection by a physics shape
//! query (a sphere overlap in front of the character while a `hit` window is open).
//!
//! Every player character gets a small procedural "sword arm" rig (root, shoulder, hand) whose
//! clips sweep the blade; the montage plays on it, so the same timing runs with or without the
//! Fox (the Fox has no attack clips). A visible blade box follows the hand joint.
//!
//! Combo: attack starts `slash_1`. Attack again while the swing's `combo` window is open links
//! its section to the next one (`MontagePlayer::set_next_section`), up to `slash_3`, the finisher.
//! A press outside the window is ignored; the chain ends when a section ends unlinked.

use std::collections::BTreeSet;
use std::sync::Arc;

use regia::prelude::*;
use regia_animation::clip::{JointTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::montage::SectionNext;
use regia_animation::pose::translated;
use regia_animation::{
    AnimationClip, Animator, ClipLibrary, JointGlobals, Montage, MontageEvent, MontageEventKind,
    MontagePlayer, PlayOptions, Skeleton, SlotDef,
};
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::camera::CameraRig;
use regia_framework::character::Character;
use regia_render::MeshRenderer;
use regia_scene::Transform;

use crate::combat::{self, Combatant, TEAM_ENEMY, TEAM_PLAYER};

/// The combo montage.
pub const COMBO_MONTAGE_TEXT: &str = include_str!("../assets/combat/combo.montage.ron");

/// Radius of the swing's hit sphere (m).
pub const HIT_RADIUS: f32 = 0.95;
/// Distance of the hit sphere's center in front of the character (m).
pub const HIT_REACH: f32 = 1.05;
/// Enemies within this distance pull the swing toward them (soft lock).
const LOCK_RANGE: f32 = 4.0;

/// The keys that attack (also the mouse's left button, recorded as `MouseLeft`).
pub const ATTACK_KEYS: [&str; 2] = ["KeyF", "MouseLeft"];

/// Combat state of a player character (reflected for agent checks).
#[derive(Component, Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Component, Default)]
pub struct MeleeState {
    /// Swing of the combo now playing (1..=3), 0 when idle.
    pub combo_step: u32,
    /// Swings started.
    pub swings: u32,
    /// Hits landed.
    pub hits: u32,
    /// A `hit` window is open.
    pub hit_window: bool,
    /// A `combo` window is open (an attack now links the next swing).
    pub combo_window: bool,
    /// Damage of the open hit window.
    pub damage: f32,
    /// The swing's direction on the ground plane.
    pub facing: Vec3,
    /// Presses refused (cooldown, stagger, down).
    pub refused: u32,
}

/// The character's sword-arm rig and blade.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwordArm {
    /// The rig entity (montage player).
    pub rig: Entity,
    /// The blade mesh entity.
    pub blade: Entity,
}

/// On the sword rig: its character.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmOf(pub Entity);

/// On the blade mesh: its character.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blade {
    /// The character.
    pub character: Entity,
}

/// The montage play of the current combo (events of older plays, still blending out, are ignored).
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CurrentSwing(pub Option<regia_animation::MontageId>);

/// The entities hit by the open window (each is hit once per window).
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct SwingHits(pub BTreeSet<Entity>);

/// Attack presses waiting for the next update, per character (agents and tests may press
/// [`press_attack`] directly; keys go through [`read_attack_keys`]).
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttackPress {
    /// A press is pending.
    pub pending: bool,
    /// The attack key was held last frame (edge detection).
    pub held: bool,
}

/// Installs the melee systems.
pub struct MeleePlugin;

impl Plugin for MeleePlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<MeleeState>().add_systems(
            Update,
            (
                (equip_players, read_attack_keys, start_attacks)
                    .chain()
                    .before(regia_animation::systems::evaluate_animators),
                (montage_windows, sweep_hits, place_blades)
                    .chain()
                    .after(regia_animation::systems::evaluate_animators),
            ),
        );
    }
}

/// The sword arm's skeleton: root at the capsule center, the right shoulder, the hand.
fn arm_skeleton() -> Option<Skeleton> {
    Skeleton::from_local_bind(
        vec!["root".into(), "shoulder".into(), "hand".into()],
        vec![None, Some(0), Some(1)],
        vec![
            translated(Vec3::ZERO),
            translated(Vec3::new(0.28, 0.3, 0.0)),
            translated(Vec3::new(0.0, 0.0, -0.5)),
        ],
    )
    .ok()
}

/// A shoulder rotation: `yaw` about Y, then `pitch` about X (radians).
fn arm(yaw: f32, pitch: f32) -> Quat {
    Quat::from_rotation_y(yaw) * Quat::from_rotation_x(pitch)
}

/// A clip that rotates the shoulder through `keys` (time, rotation).
fn swing_clip(name: &str, duration: f32, keys: &[(f32, Quat)]) -> AnimationClip {
    let mut clip = AnimationClip::empty(name, duration);
    clip.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Static(Vec3::new(0.28, 0.3, 0.0)),
        rotations: TrackCurve::Animated {
            times: keys.iter().map(|k| k.0).collect(),
            values: keys.iter().map(|k| k.1).collect(),
            exact_rotation: true,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip
}

/// The rest pose: the blade held low and forward.
fn rest() -> Quat {
    arm(0.3, -0.9)
}

/// The three swing clips (lengths match the montage's sections).
fn swing_clips() -> ClipLibrary {
    let mut clips = ClipLibrary::new();
    clips.insert(swing_clip(
        "slash_1",
        0.45,
        &[
            (0.0, arm(1.3, -0.1)),
            (0.1, arm(1.4, 0.0)),
            (0.28, arm(-1.3, -0.15)),
            (0.45, rest()),
        ],
    ));
    clips.insert(swing_clip(
        "slash_2",
        0.45,
        &[
            (0.0, arm(-1.3, -0.1)),
            (0.1, arm(-1.4, 0.05)),
            (0.28, arm(1.2, -0.2)),
            (0.45, rest()),
        ],
    ));
    clips.insert(swing_clip(
        "slash_3",
        0.6,
        &[
            (0.0, arm(0.0, 1.5)),
            (0.15, arm(0.0, 1.7)),
            (0.32, arm(0.0, -1.2)),
            (0.6, rest()),
        ],
    ));
    clips
}

/// The arm's idle clip: the rest pose.
fn idle_clip() -> AnimationClip {
    let mut idle = AnimationClip::empty("arm_idle", 1.0);
    idle.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Static(Vec3::new(0.28, 0.3, 0.0)),
        rotations: TrackCurve::Static(rest()),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    idle
}

/// `Update`: gives each player character without one its combat kit (attributes, the slash
/// grant, the sword-arm rig and blade).
#[tracing::instrument(skip_all, target = "third_person", name = "melee_equip")]
pub fn equip_players(world: &mut World) {
    let fresh: Vec<Entity> = world
        .query_filtered::<Entity, (With<Character>, Without<Combatant>)>()
        .iter(world)
        .collect();
    for character in fresh {
        if combat::equip(
            world,
            character,
            combat::PLAYER_ATTRS,
            TEAM_PLAYER,
            combat::SLASH_ABILITY,
        )
        .is_none()
        {
            // Mark it anyway so it is not retried every frame; it cannot attack.
            world.entity_mut(character).insert(Combatant {
                team: TEAM_PLAYER,
                attack_grant: u32::MAX,
            });
        }
        attach_arm(world, character);
    }
}

/// Spawns the sword-arm rig and blade for `character`.
fn attach_arm(world: &mut World, character: Entity) {
    let Some(skeleton) = arm_skeleton() else {
        return;
    };
    let montage = match Montage::from_ron_str(COMBO_MONTAGE_TEXT) {
        Ok(m) => m,
        Err(err) => {
            tracing::error!(target: "third_person", "combo.montage.ron: {err}");
            return;
        }
    };
    let clips = swing_clips();
    let idle = idle_clip();
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], clips);
    player.add_montage(montage);
    let binding = spawn_skeleton(world, Arc::new(skeleton));
    let rig = world
        .spawn((
            Transform::default(),
            binding,
            Animator::single(idle, true),
            player,
            ArmOf(character),
        ))
        .id();
    let blade = world
        .spawn((
            Transform::default(),
            MeshRenderer::new(
                crate::level::box_mesh(Vec3::new(0.035, 0.035, 0.42)),
                crate::level::solid_material([0.85, 0.88, 0.92, 1.0]),
            ),
            Blade { character },
            Name::new("blade"),
        ))
        .id();
    world.entity_mut(character).insert((
        SwordArm { rig, blade },
        MeleeState {
            facing: Vec3::NEG_Z,
            ..Default::default()
        },
        SwingHits::default(),
        AttackPress::default(),
        CurrentSwing::default(),
    ));
}

/// Queues an attack press for `character` (the agent/test entry point; keys call it too).
pub fn press_attack(world: &mut World, character: Entity) {
    if let Some(mut press) = world.get_mut::<AttackPress>(character) {
        press.pending = true;
    }
}

/// `Update`: an attack key going down queues a press for player 0's character.
#[tracing::instrument(skip_all, target = "third_person", name = "melee_keys")]
pub fn read_attack_keys(world: &mut World) {
    let held = world
        .get_resource::<crate::input::ActionKeys>()
        .is_some_and(|k| ATTACK_KEYS.iter().any(|key| k.held.contains(*key)));
    let Some(character) = crate::input::possessed(world, 0) else {
        return;
    };
    if let Some(mut press) = world.get_mut::<AttackPress>(character) {
        if held && !press.held {
            press.pending = true;
        }
        press.held = held;
    }
}

/// The swing direction: toward the nearest enemy in reach, else the camera's forward.
fn swing_facing(world: &mut World, character: Entity) -> Vec3 {
    let Some(at) = world.get::<Transform>(character).map(|t| t.translation) else {
        return Vec3::NEG_Z;
    };
    let mut best: Option<(f32, Vec3)> = None;
    let mut query = world.query::<(&Combatant, &Transform)>();
    for (c, t) in query.iter(world) {
        if c.team != TEAM_ENEMY {
            continue;
        }
        let to = Vec3::new(t.translation.x - at.x, 0.0, t.translation.z - at.z);
        let d = to.length();
        if d > 1e-3 && d <= LOCK_RANGE && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, to / d));
        }
    }
    if let Some((_, dir)) = best {
        return dir;
    }
    let player = crate::input::player_of(world, character);
    let mut query = world.query::<(&CameraRig, &crate::camera::PlayerCamera)>();
    for (rig, cam) in query.iter(world) {
        if player.is_none_or(|p| p == cam.player) {
            return Vec3::new(-rig.yaw.sin(), 0.0, -rig.yaw.cos());
        }
    }
    world
        .get::<MeleeState>(character)
        .map_or(Vec3::NEG_Z, |m| m.facing)
}

/// `Update`: turns pending presses into swings: the first starts the combo, a press inside a
/// `combo` window links the next swing.
#[tracing::instrument(skip_all, target = "third_person", name = "melee_start")]
pub fn start_attacks(world: &mut World) {
    let in_play = world
        .get_resource::<State<regia_framework::flow::GameFlow>>()
        .is_none_or(|s| *s.get() == regia_framework::flow::GameFlow::Playing);
    let pressed: Vec<(Entity, SwordArm)> = world
        .query::<(Entity, &AttackPress, &SwordArm)>()
        .iter(world)
        .filter(|(_, p, _)| p.pending)
        .map(|(e, _, a)| (e, *a))
        .collect();
    for (character, arm) in pressed {
        if let Some(mut press) = world.get_mut::<AttackPress>(character) {
            press.pending = false;
        }
        let Some(state) = world.get::<MeleeState>(character).cloned() else {
            continue;
        };
        if !in_play
            || combat::is_down(world, character)
            || world.get::<crate::car::Driving>(character).is_some()
        {
            refuse(world, character);
            continue;
        }
        let starting = state.combo_step == 0;
        let linking = !starting && state.combo_window && state.combo_step < 3;
        if !starting && !linking {
            continue;
        }
        if !combat::try_attack(world, character) {
            refuse(world, character);
            continue;
        }
        let facing = swing_facing(world, character);
        let Some(mut player) = world.get_mut::<MontagePlayer>(arm.rig) else {
            continue;
        };
        if starting {
            let Some(montage) = player.montage("combo").cloned() else {
                continue;
            };
            let id = player.play(montage, PlayOptions::default());
            world.entity_mut(character).insert(CurrentSwing(Some(id)));
        } else {
            let from = format!("slash_{}", state.combo_step);
            let to = format!("slash_{}", state.combo_step + 1);
            player.set_next_section(None, &from, SectionNext::Jump(to));
        }
        if let Some(mut melee) = world.get_mut::<MeleeState>(character) {
            melee.combo_step += 1;
            melee.swings += 1;
            melee.combo_window = false;
            melee.facing = facing;
        }
        // A swing is loud: enemies within earshot hear it.
        if let Some(at) = world.get::<Transform>(character).map(|t| t.translation) {
            world.write_message(regia_ai::HearingEvent {
                position: at,
                radius: 6.0,
                team: TEAM_ENEMY,
                source: character,
            });
        }
        let at = world
            .get::<Transform>(character)
            .map_or(Vec3::ZERO, |t| t.translation);
        world.write_message(crate::sound::SoundCue::Swing(at));
    }
}

fn refuse(world: &mut World, character: Entity) {
    if let Some(mut melee) = world.get_mut::<MeleeState>(character) {
        melee.refused += 1;
    }
}

/// `Update` (after the animation evaluates): montage windows open and close the hit and combo
/// windows; the end of the montage ends the combo.
#[tracing::instrument(skip_all, target = "third_person", name = "melee_windows")]
pub fn montage_windows(world: &mut World, mut cursor: Local<MessageCursor<MontageEvent>>) {
    let events: Vec<MontageEvent> = world
        .get_resource::<Messages<MontageEvent>>()
        .map(|m| cursor.read(m).cloned().collect())
        .unwrap_or_default();
    for event in events {
        let Some(ArmOf(character)) = world.get::<ArmOf>(event.entity).copied() else {
            continue;
        };
        if world.get::<CurrentSwing>(character).and_then(|c| c.0) != Some(event.id) {
            continue;
        }
        let mut clear_hits = false;
        let Some(mut melee) = world.get_mut::<MeleeState>(character) else {
            continue;
        };
        match event.kind {
            MontageEventKind::WindowBegin { name, payload } if name == "hit" => {
                melee.hit_window = true;
                melee.damage = payload.and_then(|p| p.trim().parse::<f32>().ok()).unwrap_or(10.0);
                clear_hits = true;
            }
            MontageEventKind::WindowEnd { name } if name == "hit" => melee.hit_window = false,
            MontageEventKind::WindowBegin { name, .. } if name == "combo" => melee.combo_window = true,
            MontageEventKind::WindowEnd { name } if name == "combo" => melee.combo_window = false,
            MontageEventKind::Section { to, .. } => {
                // Keep the step in line with the section actually playing.
                if let Some(n) = to.strip_prefix("slash_").and_then(|n| n.parse::<u32>().ok()) {
                    melee.combo_step = n;
                }
            }
            MontageEventKind::BlendingOut { .. } | MontageEventKind::Ended { .. } => {
                melee.combo_step = 0;
                melee.combo_window = false;
                melee.hit_window = false;
            }
            _ => {}
        }
        if clear_hits && let Some(mut hits) = world.get_mut::<SwingHits>(character) {
            hits.0.clear();
        }
    }
}

/// `Update`: while a hit window is open, a sphere overlap in front of the character damages every
/// enemy inside it once per window.
#[tracing::instrument(skip_all, target = "third_person", name = "melee_sweep")]
pub fn sweep_hits(world: &mut World) {
    let open: Vec<(Entity, Vec3, f32)> = world
        .query::<(Entity, &MeleeState)>()
        .iter(world)
        .filter(|(_, m)| m.hit_window)
        .map(|(e, m)| (e, m.facing, m.damage))
        .collect();
    for (character, facing, damage) in open {
        if combat::is_down(world, character) {
            continue;
        }
        let Some(at) = world.get::<Transform>(character).map(|t| t.translation) else {
            continue;
        };
        let hits = crate::enemy::sphere_hits(world, at + facing * HIT_REACH, HIT_RADIUS, character);
        for target in hits {
            let already = world
                .get::<SwingHits>(character)
                .is_some_and(|h| h.0.contains(&target));
            if already
                || world
                    .get::<Combatant>(target)
                    .is_none_or(|c| c.team != TEAM_ENEMY)
            {
                continue;
            }
            if let Some(mut h) = world.get_mut::<SwingHits>(character) {
                h.0.insert(target);
            }
            if combat::deal_damage(world, character, target, damage)
                && let Some(mut melee) = world.get_mut::<MeleeState>(character)
            {
                melee.hits += 1;
            }
        }
    }
}

/// `Update`: each blade sits on its rig's hand joint, placed at the character facing the swing.
#[tracing::instrument(skip_all, target = "third_person", name = "melee_blades")]
pub fn place_blades(world: &mut World) {
    let arms: Vec<(Entity, SwordArm, Vec3)> = world
        .query::<(Entity, &SwordArm, &MeleeState)>()
        .iter(world)
        .map(|(e, a, m)| (e, *a, m.facing))
        .collect();
    for (character, arm, facing) in arms {
        let Some(at) = world.get::<Transform>(character).map(|t| t.translation) else {
            continue;
        };
        let Some(hand) = world
            .get::<JointGlobals>(arm.rig)
            .and_then(|g| g.globals.get(2).copied())
        else {
            continue;
        };
        let yaw = (-facing.x).atan2(-facing.z);
        let body = Transform {
            translation: at,
            rotation: Quat::from_rotation_y(yaw),
            ..Default::default()
        };
        let (_, hand_rot, hand_pos) = hand.to_scale_rotation_translation();
        let rotation = body.rotation * hand_rot;
        let blade_center = body.transform_point(hand_pos) + rotation * Vec3::new(0.0, 0.0, -0.42);
        if let Some(mut t) = world.get_mut::<Transform>(arm.blade) {
            t.translation = blade_center;
            t.rotation = rotation;
        }
    }
}
