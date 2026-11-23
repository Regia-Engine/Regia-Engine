// SPDX-License-Identifier: MPL-2.0
//! Contact sounds from physics (ADR-3270, `book/agents/physics-audio.md`).
//!
//! [`PhysicsAudioPlugin`] turns physics contacts into sound through the [`ImpactBank`]:
//!
//! - **Impacts.** Each fixed step, every `ContactForce` message (a collider opts in with
//!   `Collider::active_events.contact_force` and a `contact_force_threshold`) is sampled through
//!   `regia_physics::contact_query`: the touching point, the normal, and both bodies' motion. The
//!   relative normal (approach) speed comes from the bodies' motion *before* the step (kept from the
//!   previous step); the impulse is the momentum that stops that approach (reduced mass times the
//!   speed, plus any bounce), at least the total force times the step. A contact above the bank's
//!   minimum impulse and approach speed is an impact candidate, unless a dynamic body in it played
//!   an impact within the cooldown. The loudest `max_new_impacts_per_step` candidates play at the
//!   contact; the rest are dropped and counted ([`PhysicsAudioStats::dropped_by_budget`]).
//! - **Scrapes and rolls.** The same contact, after the step, gives the slip speed at the contact
//!   point, the rolling speed (angular speed times the distance from the center of mass to the
//!   contact) and the normal force. [`ImpactBank::contact_loop`] decides rolling or sliding and
//!   the level. Each touching pair keeps at most one loop voice; when the pair stops sliding or
//!   rolling (or separates) its level fades to zero over `fade_steps` fixed steps and the voice
//!   stops. At most `max_loop_voices` loops play; the quietest pairs beyond that are counted.
//! - **Breaks.** `DestructionFxMessage`s become one break sound per destructible per step (the
//!   strongest), its `fx_tag` mapped to an impact material by the bank's `destruction_materials`.
//! - **Footstep surfaces.** [`PhysicsFootstepSurfaces`] plus [`install_physics_footstep_probe`]
//!   make the footstep system pick the surface from the physics material of the collider under the
//!   foot (a downward ray), mapped through [`PhysicsFootstepSurfaces::surfaces`].
//!
//! Materials are the colliders' `PhysicsMaterialRef` names (none: the other material, then the
//! bank's default). The faster body's material comes first. Everything is inert without an
//! [`ImpactBank`] resource, and the plugin is opt-in (not in `regia::DefaultPlugins`), so no
//! existing schedule changes. Nothing allocates per step in steady state: scratch vectors and maps
//! are reused, and new voices and plays allocate only when a sound starts. Be aware the previous
//! motions of every collider in the world are snapshotted each fixed step (an `O(colliders)`
//! walk, allocation-free once warm), whatever their contacts.
//!
//! Provenance: original design (ADR-3270); rigid-body point velocity and the friction-power level
//! are textbook mechanics (see `regia_audio::impacts`).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use regia_app::{App, FixedTime, FixedUpdate, Plugin, Update};
use regia_audio::{Audio, ContactLoop, ImpactBank, ImpactContact, SoundAssets, SoundInstanceId};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_physics::contact_query::{BodyMotion, collider_body_motion, contact_sample, normal_impulse};
use regia_physics::destruction::DestructionFxMessage;
use regia_physics::{
    Collider, ContactForce, PhysicsLayers, PhysicsMaterialRef, PhysicsStep, PhysicsWorld, QueryFilter,
    RigidBodyHandle, WorldQueries,
};
use regia_scene::GlobalTransform;

use crate::footsteps::{FootstepSurfaceProbe, FootstepWalker, play_footsteps};

/// Steps between start attempts of a loop that could not start (out of range, missing sound).
const LOOP_RETRY_STEPS: u64 = 30;

/// Loop level changes smaller than this (dB) are not sent to the mixer.
const LOOP_DB_EPSILON: f32 = 0.25;

/// Loop pitch changes smaller than this fraction are not sent to the mixer.
const LOOP_PITCH_EPSILON: f32 = 0.005;

/// Counters of the contact-sound system since it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysicsAudioStats {
    /// Fixed steps processed with a bank present.
    pub steps: u64,
    /// `ContactForce` messages read.
    pub contacts: u64,
    /// Contacts with no touching point in the narrow phase (skipped).
    pub unsampled: u64,
    /// Impacts played.
    pub impacts: u64,
    /// Impact candidates dropped by the per-step cap (the quietest).
    pub dropped_by_budget: u64,
    /// Candidates dropped by the cap in the last step.
    pub dropped_last_step: u32,
    /// Most impacts played in one step.
    pub max_impacts_in_a_step: u32,
    /// Candidates skipped because a body in them was cooling down.
    pub cooled_down: u64,
    /// Scrape or roll voices started.
    pub loops_started: u64,
    /// Scrape or roll voices stopped (faded out, kind or clip changed, or over budget).
    pub loops_stopped: u64,
    /// Pair-steps a loop was wanted but over `max_loop_voices` (kept silent).
    pub loops_over_budget: u64,
    /// Break sounds played.
    pub breaks: u64,
}

/// One touching pair's scrape or roll state.
#[derive(Clone, Debug, PartialEq)]
pub struct PairVoice {
    /// The lower-bits collider entity.
    pub a: Entity,
    /// The other collider entity.
    pub b: Entity,
    /// Whether `a`'s body is the faster one (its material comes first).
    pub a_first: bool,
    /// Scrape or roll.
    pub kind: ContactLoop,
    /// Curve level while active, dB relative to the set.
    pub level_db: f32,
    /// Loop pitch relative to the set.
    pub pitch: f32,
    /// Linear gain heard now: `10^(level_db / 20)` while active, ramping to 0 over the fade.
    pub gain: f32,
    /// The fixed step the pair last slid or rolled.
    pub last_active_step: u64,
    /// The contact point when it last slid or rolled.
    pub position: Vec3,
    /// The loop voice, when one is playing.
    pub voice: Option<SoundInstanceId>,
    /// The sound name the voice is playing (re-resolved every step; a changed clip restarts it).
    pub sound: Option<String>,
    /// The level last sent to the voice, dB (absolute).
    pub sent_db: f32,
    /// The pitch last sent to the voice.
    pub sent_pitch: f32,
    /// The step of the last failed start (retried after `LOOP_RETRY_STEPS`).
    pub failed_step: Option<u64>,
    /// Inside the loop budget this step.
    pub voiced: bool,
}

/// An impact candidate of one step.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    a: Entity,
    b: Entity,
    a_first: bool,
    body_a: Option<Entity>,
    body_b: Option<Entity>,
    impulse_ns: f32,
    normal_speed_m_s: f32,
    position: Vec3,
    db: f32,
}

/// The strongest destruction moment of one destructible in one step.
#[derive(Clone, Debug)]
struct PendingBreak {
    destructible: u32,
    magnitude: f32,
    position: Vec3,
    seed: u64,
    fx_tag: String,
}

/// The contact-sound system's state: previous motions, cooldowns, pair voices, scratch buffers and
/// counters (a resource the plugin adds).
#[derive(Resource, Debug)]
pub struct PhysicsAudioState {
    step: u64,
    /// Collider entity → (its body's motion after the previous step, step stamp).
    previous: HashMap<Entity, (BodyMotion, u64)>,
    /// Dynamic body entity → step of its last impact.
    last_impact: HashMap<Entity, u64>,
    /// Destructible → step of its last break.
    last_break: HashMap<u32, u64>,
    candidates: Vec<Candidate>,
    order: Vec<(f32, usize)>,
    breaks: Vec<PendingBreak>,
    /// Pair voices, sorted by `(a, b)` bits.
    pairs: Vec<PairVoice>,
    /// Counters.
    pub stats: PhysicsAudioStats,
}

impl Default for PhysicsAudioState {
    fn default() -> Self {
        Self {
            step: 0,
            previous: HashMap::with_capacity(256),
            last_impact: HashMap::with_capacity(256),
            last_break: HashMap::with_capacity(16),
            candidates: Vec::with_capacity(256),
            order: Vec::with_capacity(256),
            breaks: Vec::with_capacity(16),
            pairs: Vec::with_capacity(64),
            stats: PhysicsAudioStats::default(),
        }
    }
}

impl PhysicsAudioState {
    /// Fixed steps processed so far.
    #[must_use]
    pub fn step(&self) -> u64 {
        self.step
    }

    /// The live pair voices (sliding, rolling or fading), sorted by entity bits.
    #[must_use]
    pub fn pairs(&self) -> &[PairVoice] {
        &self.pairs
    }

    /// The pair voice of colliders `a` and `b` (either order).
    #[must_use]
    pub fn pair(&self, a: Entity, b: Entity) -> Option<&PairVoice> {
        let key = pair_key(a, b);
        self.pairs
            .binary_search_by(|p| (p.a.to_bits(), p.b.to_bits()).cmp(&key))
            .ok()
            .map(|i| &self.pairs[i])
    }

    /// Forgets every pair, cooldown and previous motion (voices are not stopped: use with a fresh
    /// mixer or after stopping them).
    pub fn clear(&mut self) {
        self.previous.clear();
        self.last_impact.clear();
        self.last_break.clear();
        self.pairs.clear();
    }
}

fn pair_key(a: Entity, b: Entity) -> (u64, u64) {
    let (x, y) = (a.to_bits(), b.to_bits());
    if x <= y { (x, y) } else { (y, x) }
}

/// The material name of a collider entity (`""` without a `PhysicsMaterialRef`, which falls through
/// to the other material and then the bank's default).
fn material<'a>(materials: &'a Query<&PhysicsMaterialRef>, entity: Entity) -> &'a str {
    materials.get(entity).map_or("", |m| m.0.as_str())
}

/// What [`physics_audio_step`] plays through: the bank, the mixer and the registered sounds.
type ContactOutput<'w> = (
    Option<ResMut<'w, ImpactBank>>,
    Option<ResMut<'w, Audio>>,
    Option<Res<'w, SoundAssets>>,
);

/// The messages [`physics_audio_step`] reads.
type ContactInput<'w, 's> = (
    MessageReader<'w, 's, ContactForce>,
    MessageReader<'w, 's, DestructionFxMessage>,
);

/// `FixedUpdate` after `PhysicsStep`: impacts, pair loops and breaks for this step (inert without
/// an [`ImpactBank`]).
pub fn physics_audio_step(
    (mut contacts, mut fx): ContactInput<'_, '_>,
    physics: Option<Res<PhysicsWorld>>,
    fixed: Option<Res<FixedTime>>,
    (bank, audio, sounds): ContactOutput<'_>,
    mut state: ResMut<PhysicsAudioState>,
    materials: Query<&PhysicsMaterialRef>,
    colliders: Query<Entity, With<Collider>>,
) {
    if bank.is_none() || audio.is_none() || physics.is_none() {
        contacts.clear();
        fx.clear();
        // The bank (or the mixer, or physics) went away: no loop may keep playing, and motions kept
        // from before the gap must not read as the previous step when it comes back.
        let state = &mut *state;
        if let Some(mut audio) = audio {
            for pair in &mut state.pairs {
                if let Some(voice) = pair.voice.take() {
                    audio.stop_voice(voice, std::time::Duration::from_millis(30));
                    state.stats.loops_stopped += 1;
                }
            }
        }
        state.pairs.clear();
        state.previous.clear();
        return;
    }
    let (Some(mut bank), Some(mut audio), Some(physics)) = (bank, audio, physics) else {
        return;
    };
    let _span = tracing::trace_span!("physics_audio_step").entered();
    let state = &mut *state;
    state.step += 1;
    state.stats.steps += 1;
    let step = state.step;
    let dt = fixed
        .map(|f| f.step().as_secs_f32())
        .filter(|s| s.is_finite() && *s > 0.0)
        .unwrap_or(1.0 / 60.0);
    let budget = bank.budget().clone();
    let cooldown = u64::from(budget.cooldown_steps);

    // 1. Contacts: impact candidates and the pairs' loop levels.
    state.candidates.clear();
    for contact in contacts.read() {
        state.stats.contacts += 1;
        let (a, b) = (contact.entity_a, contact.entity_b);
        if a == b {
            continue;
        }
        let Some(sample) = contact_sample(&physics, a, b) else {
            state.stats.unsampled += 1;
            continue;
        };
        let previous = |entity: Entity, now: BodyMotion| {
            state
                .previous
                .get(&entity)
                .filter(|(_, stamp)| *stamp + 1 == step)
                .map_or(now, |m| m.0)
        };
        let before_a = previous(a, sample.motion_a);
        let before_b = previous(b, sample.motion_b);
        let a_first = before_a.linear.length_squared() >= before_b.linear.length_squared();
        let n = sample.normal;
        let approach = (before_a.velocity_at(sample.point) - before_b.velocity_at(sample.point)).dot(n);
        let (ma, mb) = (sample.motion_a, sample.motion_b);
        let after = (ma.velocity_at(sample.point) - mb.velocity_at(sample.point)).dot(n);
        // The impact impulse is the momentum that stops the approach (plus a bounce, if the pair
        // separates within the step): rapier may stop a fast body over two steps (a speculative
        // contact, then the touching one), and it reports only its last substep's impulse in the
        // force, which stays the floor for a pressing contact.
        let impulse = normal_impulse(approach, after.min(0.0), ma.inverse_mass, mb.inverse_mass)
            .max(contact.total_force * dt);
        if let Some(db) = bank.impact_db(impulse, approach) {
            let dynamic = |m: &BodyMotion| m.body.filter(|_| m.dynamic);
            let cooling = [dynamic(&sample.motion_a), dynamic(&sample.motion_b)]
                .into_iter()
                .flatten()
                .any(|body| {
                    state
                        .last_impact
                        .get(&body)
                        .is_some_and(|&s| step <= s + cooldown)
                });
            if cooling {
                state.stats.cooled_down += 1;
            } else {
                state.candidates.push(Candidate {
                    a,
                    b,
                    a_first,
                    body_a: dynamic(&sample.motion_a),
                    body_b: dynamic(&sample.motion_b),
                    impulse_ns: impulse,
                    normal_speed_m_s: approach,
                    position: sample.point,
                    db,
                });
            }
        }
        // After the step: slip at the contact, rolling speed and normal force.
        let tangential = |v: Vec3| v - n * v.dot(n);
        let slip = tangential(ma.velocity_at(sample.point) - mb.velocity_at(sample.point)).length();
        let spin = |m: &BodyMotion| tangential(m.angular.cross(sample.point - m.center_of_mass)).length();
        let rolling = spin(&ma).max(spin(&mb));
        let level = bank.contact_loop(slip, rolling, contact.total_force);
        let key = pair_key(a, b);
        let found = state
            .pairs
            .binary_search_by(|p| (p.a.to_bits(), p.b.to_bits()).cmp(&key));
        let a_low = a.to_bits() == key.0;
        match (found, level) {
            (Ok(i), Some(level)) => {
                let pair = &mut state.pairs[i];
                if pair.kind != level.kind {
                    if let Some(voice) = pair.voice.take() {
                        audio.stop_voice(voice, std::time::Duration::from_millis(30));
                        state.stats.loops_stopped += 1;
                    }
                    pair.failed_step = None;
                }
                pair.kind = level.kind;
                pair.level_db = level.db;
                pair.pitch = level.pitch;
                pair.last_active_step = step;
                pair.position = sample.point;
                pair.a_first = a_first == a_low;
            }
            (Err(i), Some(level)) => {
                let (lo, hi) = if a_low { (a, b) } else { (b, a) };
                state.pairs.insert(
                    i,
                    PairVoice {
                        a: lo,
                        b: hi,
                        a_first: a_first == a_low,
                        kind: level.kind,
                        level_db: level.db,
                        pitch: level.pitch,
                        gain: 0.0,
                        last_active_step: step,
                        position: sample.point,
                        voice: None,
                        sound: None,
                        sent_db: f32::NAN,
                        sent_pitch: f32::NAN,
                        failed_step: None,
                        voiced: false,
                    },
                );
            }
            _ => {}
        }
    }

    // 2. Impacts: the loudest within the cap (ties by entity bits, so the pick is deterministic).
    state.order.clear();
    for (i, c) in state.candidates.iter().enumerate() {
        state.order.push((c.db, i));
    }
    let candidates = &state.candidates;
    state.order.sort_unstable_by(|x, y| {
        y.0.total_cmp(&x.0).then_with(|| {
            let (cx, cy) = (&candidates[x.1], &candidates[y.1]);
            pair_key(cx.a, cx.b).cmp(&pair_key(cy.a, cy.b))
        })
    });
    let cap = budget.max_new_impacts_per_step as usize;
    let mut played_now = 0u32;
    let mut dropped = 0u32;
    let order = std::mem::take(&mut state.order);
    for &(_, index) in &order {
        let c = state.candidates[index];
        let cooling = [c.body_a, c.body_b].into_iter().flatten().any(|body| {
            state
                .last_impact
                .get(&body)
                .is_some_and(|&s| step <= s + cooldown)
        });
        if cooling {
            state.stats.cooled_down += 1;
            continue;
        }
        if played_now as usize >= cap {
            dropped += 1;
            continue;
        }
        let (ma, mb) = (material(&materials, c.a), material(&materials, c.b));
        let (first, second) = if c.a_first { (ma, mb) } else { (mb, ma) };
        let mover = if c.a_first {
            c.body_a.or(c.body_b)
        } else {
            c.body_b.or(c.body_a)
        };
        let contact = ImpactContact {
            body: mover.map_or(0, Entity::to_bits),
            material_a: first,
            material_b: second,
            impulse_ns: c.impulse_ns,
            normal_speed_m_s: c.normal_speed_m_s,
            position: c.position,
        };
        match bank.play_impact(&mut audio, sounds.as_deref(), &contact) {
            Ok(Some(play)) => {
                // A play that started no voice (out of emitter range, an unregistered sound)
                // does not charge the per-step cap, so it never pushes an audible impact out of
                // the step. It still starts the bodies' cooldown, like an audible one: a rattle
                // out of range would otherwise be planned and logged on every step, evicting
                // the audible plays from the log.
                if play.started {
                    played_now += 1;
                }
                for body in [c.body_a, c.body_b].into_iter().flatten() {
                    state.last_impact.insert(body, step);
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(target: "regia_framework", "impact not played: {error}"),
        }
    }
    state.order = order;
    state.stats.impacts += u64::from(played_now);
    state.stats.dropped_by_budget += u64::from(dropped);
    state.stats.dropped_last_step = dropped;
    state.stats.max_impacts_in_a_step = state.stats.max_impacts_in_a_step.max(played_now);

    // 3. Loops: fade, budget, start, retune, stop.
    let fade_steps = u64::from(budget.fade_steps.max(1));
    for pair in &mut state.pairs {
        let since = step.saturating_sub(pair.last_active_step);
        let fade = 1.0 - since as f32 / fade_steps as f32;
        pair.gain = if fade > 0.0 {
            10f32.powf(pair.level_db / 20.0) * fade
        } else {
            0.0
        };
    }
    state.order.clear();
    for (i, pair) in state.pairs.iter().enumerate() {
        if pair.gain > 0.0 {
            state.order.push((pair.gain, i));
        }
    }
    let pairs = &state.pairs;
    state.order.sort_unstable_by(|x, y| {
        y.0.total_cmp(&x.0).then_with(|| {
            (pairs[x.1].a.to_bits(), pairs[x.1].b.to_bits())
                .cmp(&(pairs[y.1].a.to_bits(), pairs[y.1].b.to_bits()))
        })
    });
    for pair in &mut state.pairs {
        pair.voiced = false;
    }
    let max_loops = budget.max_loop_voices as usize;
    for (k, &(_, i)) in state.order.iter().enumerate() {
        if k < max_loops {
            state.pairs[i].voiced = true;
        } else {
            state.stats.loops_over_budget += 1;
        }
    }
    let mut stopped = 0u64;
    let mut started = 0u64;
    for pair in &mut state.pairs {
        let wants = pair.voiced && pair.gain > 0.0;
        if !wants {
            if let Some(voice) = pair.voice.take() {
                audio.stop_voice(voice, std::time::Duration::from_millis(30));
                stopped += 1;
            }
            continue;
        }
        let (ma, mb) = (material(&materials, pair.a), material(&materials, pair.b));
        let (first, second) = if pair.a_first { (ma, mb) } else { (mb, ma) };
        let Some((sound, set_db, set_pitch)) = bank.loop_sound(first, second, pair.kind) else {
            // Nothing to play for this pair now (a material changed or went away): stop, not freeze.
            if let Some(voice) = pair.voice.take() {
                audio.stop_voice(voice, std::time::Duration::from_millis(30));
                stopped += 1;
            }
            continue;
        };
        let db = set_db + 20.0 * pair.gain.max(1e-6).log10();
        let pitch = set_pitch * pair.pitch;
        // The resolved clip changed under a playing voice (a bank edit, a material change):
        // stop it so it restarts on the new clip below, like the kind-change path above.
        if pair.sound.as_deref() != Some(sound)
            && let Some(voice) = pair.voice.take()
        {
            audio.stop_voice(voice, std::time::Duration::from_millis(30));
            stopped += 1;
            pair.failed_step = None;
            pair.sent_db = f32::NAN;
            pair.sent_pitch = f32::NAN;
        }
        match pair.voice {
            Some(voice) => {
                if pair.sent_db.is_nan() || (pair.sent_db - db).abs() > LOOP_DB_EPSILON {
                    audio.set_voice_volume_db(voice, db, 1.0 / 60.0);
                    pair.sent_db = db;
                }
                if pair.sent_pitch.is_nan() || (pair.sent_pitch - pitch).abs() > LOOP_PITCH_EPSILON * pitch {
                    audio.set_voice_pitch(voice, pitch, 1.0 / 60.0);
                    pair.sent_pitch = pitch;
                }
            }
            None => {
                if pair.failed_step.is_some_and(|s| step < s + LOOP_RETRY_STEPS) {
                    continue;
                }
                match bank.start_loop(&mut audio, sounds.as_deref(), sound, db, pitch, pair.position) {
                    Ok(Some(voice)) => {
                        pair.voice = Some(voice);
                        pair.sound = Some(sound.to_owned());
                        pair.sent_db = db;
                        pair.sent_pitch = pitch;
                        pair.failed_step = None;
                        started += 1;
                    }
                    Ok(None) => pair.failed_step = Some(step),
                    Err(error) => {
                        pair.failed_step = Some(step);
                        tracing::warn!(target: "regia_framework", "contact loop not started: {error}");
                    }
                }
            }
        }
    }
    state.stats.loops_started += started;
    state.stats.loops_stopped += stopped;
    state.pairs.retain(|p| p.gain > 0.0 || p.voice.is_some());

    // 4. Breaks: one per destructible per step (the strongest), within the cooldown.
    state.breaks.clear();
    for message in fx.read() {
        let event = &message.0;
        let id = event.destructible.0;
        match state.breaks.iter_mut().find(|entry| entry.destructible == id) {
            Some(entry) if event.magnitude > entry.magnitude => {
                entry.magnitude = event.magnitude;
                entry.position = Vec3::from_array(event.position);
                entry.seed = event.seed;
                event.fx_tag.clone_into(&mut entry.fx_tag);
            }
            Some(_) => {}
            None => state.breaks.push(PendingBreak {
                destructible: id,
                magnitude: event.magnitude,
                position: Vec3::from_array(event.position),
                seed: event.seed,
                fx_tag: event.fx_tag.clone(),
            }),
        }
    }
    state.breaks.sort_unstable_by_key(|entry| entry.destructible);
    let breaks = std::mem::take(&mut state.breaks);
    for entry in &breaks {
        let id = entry.destructible;
        if state.last_break.get(&id).is_some_and(|&s| step <= s + cooldown) {
            continue;
        }
        state.last_break.insert(id, step);
        match bank.play_break(
            &mut audio,
            sounds.as_deref(),
            &entry.fx_tag,
            entry.magnitude,
            entry.position,
            entry.seed,
        ) {
            Ok(Some(_)) => state.stats.breaks += 1,
            Ok(None) => {}
            Err(error) => tracing::warn!(target: "regia_framework", "break not played: {error}"),
        }
    }
    state.breaks = breaks;

    // 5. Previous motions for the next step's approach speeds: every collider, since a pair reports
    // a force when either of its colliders opts in.
    for entity in &colliders {
        if let Some(motion) = collider_body_motion(&physics, entity) {
            state.previous.insert(entity, (motion, step));
        }
    }
    state.previous.retain(|_, (_, s)| *s == step);
    if state.last_impact.len() > 4096 {
        state.last_impact.retain(|_, s| step <= *s + cooldown);
    }
    if state.last_break.len() > 1024 {
        state.last_break.retain(|_, s| step <= *s + cooldown);
    }
}

/// Physics material → footstep surface for [`install_physics_footstep_probe`]; a material not
/// listed is its own surface name, and a collider without a material gives no surface (the
/// footstep system falls through to the terrain layer).
#[derive(Resource, Clone, Debug, PartialEq)]
pub struct PhysicsFootstepSurfaces {
    /// Physics material name → footstep surface name.
    pub surfaces: BTreeMap<String, String>,
    /// How far below the foot the ground ray reaches, metres (it starts 0.25 m above the foot).
    /// Zero or less reads as 1 m.
    pub reach_m: f32,
}

impl Default for PhysicsFootstepSurfaces {
    /// No mapping (every material is its own surface), a 1 m reach.
    fn default() -> Self {
        Self {
            surfaces: BTreeMap::new(),
            reach_m: 1.0,
        }
    }
}

impl PhysicsFootstepSurfaces {
    /// The surface of a physics material: its entry in [`Self::surfaces`], else the material name.
    #[must_use]
    pub fn surface_for<'a>(&'a self, material: &'a str) -> &'a str {
        self.surfaces.get(material).map_or(material, String::as_str)
    }
}

/// Foot position → surface found under it, one entry per walker.
type FootSurfaces = Vec<(Vec3, Option<String>)>;

/// The surfaces the last refresh found under each walker's foot, shared with the
/// [`FootstepSurfaceProbe`] that [`install_physics_footstep_probe`] made (a resource of its own,
/// so replacing [`PhysicsFootstepSurfaces`] keeps the probe working).
#[derive(Resource, Clone, Debug, Default)]
pub struct PhysicsFootstepCache(Arc<RwLock<FootSurfaces>>);

impl PhysicsFootstepCache {
    /// The probe the footstep system calls: the surface the last refresh found at that foot
    /// position (within 1 mm), `None` elsewhere.
    #[must_use]
    pub fn probe(&self) -> FootstepSurfaceProbe {
        let cache = Arc::clone(&self.0);
        FootstepSurfaceProbe(Box::new(move |position: Vec3| {
            let entries = cache.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            entries
                .iter()
                .find(|(at, _)| at.distance_squared(position) <= 1.0e-6)
                .and_then(|(_, surface)| surface.clone())
        }))
    }
}

/// Inserts [`PhysicsFootstepSurfaces`] (if missing), a [`PhysicsFootstepCache`] and its
/// [`FootstepSurfaceProbe`], replacing a probe the host set. The refresh system
/// ([`refresh_physics_footstep_surfaces`]) is added by [`PhysicsAudioPlugin`].
pub fn install_physics_footstep_probe(app: &mut App) {
    if !app.world().contains_resource::<PhysicsFootstepSurfaces>() {
        app.insert_resource(PhysicsFootstepSurfaces::default());
    }
    let cache = PhysicsFootstepCache::default();
    app.insert_resource(cache.probe());
    app.insert_resource(cache);
}

/// `Update`, before the footstep system: casts a ray down from every walker's foot and records the
/// surface of the physics material it hits (inert without [`PhysicsFootstepSurfaces`] and
/// [`PhysicsFootstepCache`]). Entries are rewritten in place: a surface name is copied only when it
/// changes.
pub fn refresh_physics_footstep_surfaces(
    (surfaces, cache): (
        Option<Res<PhysicsFootstepSurfaces>>,
        Option<Res<PhysicsFootstepCache>>,
    ),
    physics: Option<Res<PhysicsWorld>>,
    layers: Option<Res<PhysicsLayers>>,
    walkers: Query<(Entity, &FootstepWalker, &GlobalTransform)>,
    materials: Query<&PhysicsMaterialRef>,
    (parents, handles): (Query<&ChildOf>, Query<&RigidBodyHandle>),
) {
    let (Some(surfaces), Some(cache), Some(physics)) = (surfaces, cache, physics) else {
        return;
    };
    let _span = tracing::trace_span!("physics_footstep_surfaces").entered();
    let physics = WorldQueries {
        world: &physics,
        layers: layers.as_deref(),
    };
    let reach = if surfaces.reach_m.is_finite() && surfaces.reach_m > 0.0 {
        surfaces.reach_m
    } else {
        1.0
    };
    let mut entries = cache.0.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut used = 0;
    let parent_of = |e: Entity| parents.get(e).ok().map(ChildOf::parent);
    for (entity, walker, global) in &walkers {
        let foot = global.translation() - Vec3::new(0.0, walker.foot_offset_m, 0.0);
        // Skip the walker's whole body: a character's capsule `Collider` on the walker entity,
        // on a child of it, or on the body a "feet" walker hangs under would otherwise swallow
        // the ray at distance 0 (it starts inside the capsule).
        let owner = walker_body(entity, &parent_of, &|e| {
            handles
                .get(e)
                .ok()
                .and_then(|h| physics.world.bodies.get(h.to_handle()))
                .map(|rb| !rb.is_fixed())
        });
        let not_the_walker = |collider: Entity| !owned_by_walker(collider, entity, owner, &parent_of);
        let hit = physics.raycast(
            foot + Vec3::new(0.0, 0.25, 0.0),
            Vec3::NEG_Y,
            reach + 0.25,
            QueryFilter::new()
                .exclude_sensors()
                .exclude_entities(std::slice::from_ref(&entity))
                .predicate(&not_the_walker),
        );
        let surface = hit
            .and_then(|h| materials.get(h.entity).ok())
            .filter(|m| !m.0.is_empty())
            .map(|m| surfaces.surface_for(&m.0));
        if used == entries.len() {
            entries.push((foot, None));
        }
        let entry = &mut entries[used];
        entry.0 = foot;
        if entry.1.as_deref() != surface {
            entry.1 = surface.map(str::to_string);
        }
        used += 1;
    }
    entries.truncate(used);
}

/// The moving body a footstep walker belongs to: the nearest of the walker and its `ChildOf`
/// ancestors that has a rigid body, if that body is not static (`is_moving` gives `None` for an
/// entity without a body, `Some(false)` for a static one). A walker parented under a static body
/// (a level root) belongs to none, so the floor is never skipped.
fn walker_body(
    walker: Entity,
    parent_of: &dyn Fn(Entity) -> Option<Entity>,
    is_moving: &dyn Fn(Entity) -> Option<bool>,
) -> Option<Entity> {
    let mut current = Some(walker);
    for _ in 0..64 {
        let node = current?;
        if let Some(moving) = is_moving(node) {
            return moving.then_some(node);
        }
        current = parent_of(node);
    }
    None // hierarchy loop guard
}

/// Whether `collider`'s entity is part of the walker: the walker itself, the walker's body
/// (`owner`), or a descendant of either.
fn owned_by_walker(
    collider: Entity,
    walker: Entity,
    owner: Option<Entity>,
    parent_of: &dyn Fn(Entity) -> Option<Entity>,
) -> bool {
    let mut current = Some(collider);
    for _ in 0..64 {
        let Some(node) = current else {
            return false;
        };
        if node == walker || Some(node) == owner {
            return true;
        }
        current = parent_of(node);
    }
    false // hierarchy loop guard
}

/// The contact-sound plugin: [`physics_audio_step`] after `PhysicsStep` in `FixedUpdate`, and
/// [`refresh_physics_footstep_surfaces`] before the footstep system. Opt-in; inert until an [`ImpactBank`] resource exists. Add it after `PhysicsPlugin`
/// and `AudioPlugin`.
pub struct PhysicsAudioPlugin;

impl Plugin for PhysicsAudioPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PhysicsAudioState>()
            .add_message::<ContactForce>()
            .add_message::<DestructionFxMessage>()
            .add_systems(FixedUpdate, physics_audio_step.after(PhysicsStep))
            .add_systems(Update, refresh_physics_footstep_surfaces.before(play_footsteps));
    }
}
