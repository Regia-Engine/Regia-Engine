// SPDX-License-Identifier: MPL-2.0
//! Spec 53 § 10 storage rework (ADR-2426): the idle-memory rework must not change what the ability
//! system does. One long scene that exercises every pass and every path the reworked storage sits
//! on (the § 7 entity order as entities come, go, gain, lose and change their `EntityGuid`; the
//! `AttributeChanged` snapshot through base edits, recomputes, an attribute added mid-run and a write
//! that bypasses the API; effects, stacks, pulses, tags, abilities and triggers) is folded, step by
//! step, into one hash: every message of the step in order, the world's abilities state bit for
//! bit, and the pass counters. The pinned number was measured on the code before the rework
//! (`integ/v035` at 94d4d501e); the reworked code must give the same number.
//!
//! No wall clock: every step runs `FixedUpdate` directly, and inputs come from a seeded `Rng`.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Rng;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::abilities::*;
use regia_framework::tags::{Tag, TagContainer, TagQuery};
use regia_scene::components::{EntityGuid, Name};
use std::collections::HashMap;

/// The trace hash of [`scene`] measured on the code before the storage rework (ADR-2426).
/// Re-pinned by ADR-4756 (audit G finding 5): guid-less units now refuse `chance` effects
/// deterministically instead of rolling by entity id, which changes this scene's deterministic
/// trace (it deliberately spawns units without an `EntityGuid`). LEAD: this is a determinism pin
/// change, not a weakening — the determinism check (`a == b`) and the message-kind checks are
/// untouched. New value measured with the fix on the test machine/the build server harness, 2026-11-08.
/// Re-pinned once more by ADR-5960 (lead decision, round 16): the trace names units by their spawn
/// index instead of their entity bits ([`Units`]), so a resource stored as an entity no longer
/// moves it (`the_trace_does_not_depend_on_entity_ids`); measured 2026-11-11 on the workstation.
/// Re-pinned by integ-post05b (ADR-7310, rev-claude-32-k and rev-claude-41-k): ADR-7216 (7) ends a
/// despawned target's live effects with `EffectEnded { reason: TargetGone }`, which the scene's
/// despawns now fold in. Proved on the merged line (the build server, 2026-11-12): with only that despawn
/// hook switched off the trace is the old pin `0x8615_f655_a68e_b48e` exactly; with it on,
/// `0xa3c8_253b_21f3_f9cb`. The determinism check (`a == b`) and the message-kind checks are untouched.
///
/// Re-pinned by integ-v06g (ADR-7715), where the v0.5.1 line merges into v0.6: v0.5.1's ADR-7573 (the
/// recompute pass prunes the chance rows of `AbilityCounters` whose guid left the world,
/// `effects.rs::prune_rolls`; its pin was `0xc5f9_d191_f4a3_4827` on that line) and the despawn hook above
/// both move the trace. Proved on the merged code (the build server, 2026-11-14): with only `prune_rolls` switched
/// off (a temporary env switch, not committed) the trace is the v0.6 pin `0xa3c8_253b_21f3_f9cb` exactly;
/// with it on, `0x00df_632a_2959_41ac`.
const BASE_TRACE: u64 = 0x00df_632a_2959_41ac;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The hash, and the kind of every message folded.
struct Fold(u64, Vec<&'static str>);

/// Every unit the scene spawned, by its spawn index (ADR-5960). The trace names units by this
/// index, never by their entity bits: those move whenever something else (a resource stored as an
/// entity, say) spawns first. Despawned units stay listed: a message may still name one.
#[derive(Default)]
struct Units {
    by_entity: HashMap<Entity, u64>,
    by_text: HashMap<String, u64>,
}

impl Units {
    fn add(&mut self, e: Entity, index: usize) {
        self.by_entity.insert(e, index as u64);
        self.by_text.insert(format!("{e:?}"), index as u64);
    }

    /// The spawn index of `e`, which must be a unit.
    fn key(&self, e: Entity) -> u64 {
        *self
            .by_entity
            .get(&e)
            .unwrap_or_else(|| panic!("{e:?} is not a unit"))
    }

    /// `text` (a message's `Debug` text) with every entity (`{index}v{generation}`) written as
    /// `unit{spawn index}`. An entity that is not a unit fails the test: it would bring raw bits
    /// back into the trace.
    fn stable(&self, text: &str) -> String {
        let b = text.as_bytes();
        let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'.';
        let mut out = String::with_capacity(text.len());
        let (mut copied, mut i) = (0, 0);
        while i < b.len() {
            if !b[i].is_ascii_digit() || (i > 0 && word(b[i - 1])) {
                i += 1;
                continue;
            }
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let mut k = j + 1;
            while k < b.len() && b[k].is_ascii_digit() {
                k += 1;
            }
            if j < b.len() && b[j] == b'v' && k > j + 1 && (k == b.len() || !word(b[k])) {
                let token = &text[i..k];
                let unit = self
                    .by_text
                    .get(token)
                    .unwrap_or_else(|| panic!("entity {token} is not a unit: {text}"));
                out.push_str(&text[copied..i]);
                out.push_str(&format!("unit{unit}"));
                copied = k;
                i = k;
            } else {
                i = j;
            }
        }
        out.push_str(&text[copied..]);
        out
    }
}

impl Fold {
    /// Messages folded of `kind`.
    fn count(&self, kind: &str) -> usize {
        self.1.iter().filter(|&&k| k == kind).count()
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    fn text(&mut self, text: &str) {
        self.u64(text.len() as u64);
        self.bytes(text.as_bytes());
    }
}

fn t(name: &str) -> Tag {
    Tag::new(name).expect("valid tag")
}

fn q(text: &str) -> TagQuery {
    TagQuery::from_ron(text).expect("query")
}

fn modifier(attr: &str, op: ModOp, amount: Magnitude) -> Modifier {
    Modifier {
        attribute: attr.to_owned(),
        op,
        amount,
        per_stack: false,
        when: None,
    }
}

fn unit_attributes(i: usize) -> Attributes {
    let mut list = vec![
        Attribute::new("health_max", 100.0, 1.0, 9999.0),
        Attribute::new("health", 100.0, 0.0, 9999.0).with_max_from("health_max"),
        Attribute::new("mana", 40.0, 0.0, 100.0),
        Attribute::new("strength", 10.0, 0.0, 100.0),
        Attribute::new("speed", 4.0, 0.0, 12.0),
        Attribute::new("armor", 0.0, 0.0, 1000.0).with_derived(&[("strength", 2.0)], 1.0),
    ];
    // Some units differ from the template, so the reports are not all alike.
    if i % 3 == 1 {
        list[2] = Attribute::new("mana", 25.0 + i as f32, 0.0, 100.0);
    }
    if i % 5 == 2 {
        list.push(Attribute::new("stamina", 50.0, 0.0, 100.0));
    }
    Attributes::from_list(list).expect("attributes")
}

fn effects(app: &mut App) {
    let mut list: Vec<(String, EffectAsset)> = Vec::new();
    for k in 0..12u32 {
        let mut a = EffectAsset::new(Lifetime::Timed(Magnitude::Const(0.2 + (k % 5) as f32 * 0.35)));
        a.chance = if k.is_multiple_of(3) { 0.5 } else { 1.0 };
        a.live_mods = vec![modifier(
            "speed",
            ModOp::Percent,
            Magnitude::Const(0.05 * k as f32),
        )];
        if k.is_multiple_of(2) {
            a.live_mods.push(Modifier {
                when: Some(q(r#"Has("State.Armed")"#)),
                ..modifier("armor", ModOp::AddPost, Magnitude::Const(3.0))
            });
        }
        if k % 4 == 1 {
            a.live_mods.push(modifier(
                "mana",
                ModOp::AddPost,
                Magnitude::Stat {
                    side: StatSide::Target,
                    attribute: "strength".to_owned(),
                    read: StatRead::Live,
                    weight: 0.25,
                    offset: 0.0,
                },
            ));
        }
        a.base_edits = vec![modifier("health", ModOp::AddPre, Magnitude::Const(-0.5))];
        if k % 3 != 2 {
            a.pulse = Some(Pulse {
                every: 0.1,
                on_apply: k.is_multiple_of(2),
            });
        }
        a.status_tags = vec![t(&format!("Status.S{k}"))];
        if k.is_multiple_of(4) {
            a.stack = Some(StackRule {
                key: StackKey::ByTarget,
                cap: 3,
                on_add: OnAdd::default(),
                when_full: WhenFull::RejectRenew,
                on_lapse: OnLapse::PopOne,
                cue_each_stack: false,
            });
        }
        list.push((format!("e{k}"), a));
    }
    let mut fixed = EffectAsset::new(Lifetime::UntilRemoved);
    fixed.live_mods = vec![modifier("strength", ModOp::AddPre, Magnitude::Const(2.0))];
    fixed.status_tags = vec![t("Status.Blessed")];
    list.push(("blessing".to_owned(), fixed));
    let mut heal = EffectAsset::new(Lifetime::Instant);
    heal.base_edits = vec![modifier("health", ModOp::AddPre, Magnitude::Const(7.5))];
    list.push(("heal".to_owned(), heal));
    let mut cost = EffectAsset::new(Lifetime::Instant);
    cost.base_edits = vec![modifier("mana", ModOp::AddPre, Magnitude::Const(-10.0))];
    list.push(("cost".to_owned(), cost));
    let mut cd = EffectAsset::new(Lifetime::Timed(Magnitude::Const(10.0 / 60.0)));
    cd.status_tags = vec![t("Cooldown.Bolt")];
    list.push(("cd".to_owned(), cd));
    let mut assets = app.world_mut().resource_mut::<AbilityAssets>();
    for (path, asset) in list {
        assets.insert_effect(&path, asset);
    }
    let mut bolt = AbilityAsset::new("bolt");
    bolt.kind_tags = vec![t("Ability.Spell.Fire")];
    bolt.run_tags = vec![t("State.Casting")];
    bolt.cost = Some("cost".into());
    bolt.cooldown = Some("cd".into());
    bolt.max_run_ticks = Some(3);
    assets.insert_ability("bolt", bolt);
    let mut haste = AbilityAsset::new("haste");
    haste.triggers = vec![Trigger::WhileTagged(t("State.Sprinting"))];
    assets.insert_ability("haste", haste);
    let mut react = AbilityAsset::new("react");
    react.triggers = vec![Trigger::TagGained(t("Status.Blessed"))];
    react.max_run_ticks = Some(2);
    assets.insert_ability("react", react);
}

/// Every message kind the scene can write, read in order each step.
#[derive(Default)]
struct Cursors {
    attributes: MessageCursor<AttributeChanged>,
    tags: MessageCursor<EffectiveTagsChanged>,
    cues: MessageCursor<CueEvent>,
    lifecycle: MessageCursor<AbilityLifecycle>,
    applied: MessageCursor<EffectApplied>,
    ended: MessageCursor<EffectEnded>,
    refused: MessageCursor<EffectRefused>,
    stacks: MessageCursor<StacksChanged>,
}

fn fold_messages<M: Message + std::fmt::Debug>(
    world: &mut World,
    cursor: &mut MessageCursor<M>,
    fold: &mut Fold,
    units: &Units,
    kind: &'static str,
) {
    if let Some(messages) = world.get_resource::<Messages<M>>() {
        for m in cursor.read(messages) {
            fold.text(kind);
            fold.text(&units.stable(&format!("{m:?}")));
            fold.1.push(kind);
        }
    }
    if let Some(mut messages) = world.get_resource_mut::<Messages<M>>() {
        messages.update();
    }
}

fn fold_step(world: &mut World, cursors: &mut Cursors, fold: &mut Fold, units: &Units) {
    fold_messages(world, &mut cursors.attributes, fold, units, "attribute");
    fold_messages(world, &mut cursors.tags, fold, units, "tags");
    fold_messages(world, &mut cursors.cues, fold, units, "cue");
    fold_messages(world, &mut cursors.lifecycle, fold, units, "lifecycle");
    fold_messages(world, &mut cursors.applied, fold, units, "applied");
    fold_messages(world, &mut cursors.ended, fold, units, "ended");
    fold_messages(world, &mut cursors.refused, fold, units, "refused");
    fold_messages(world, &mut cursors.stacks, fold, units, "stacks");
    fold.u64(abilities_state_hash_keyed(world, |e| units.key(e)));
    let stats = world.resource::<AbilityStats>();
    fold.u64(stats.modifier_evaluations);
    fold.u64(stats.entities_skipped);
    fold.u64(stats.entities);
    fold.u64(stats.live_effects);
    // Bit for bit, by name (the state hash quantizes and keys by the § 7 order).
    let mut query = world.query::<(
        &Name,
        Option<&Attributes>,
        Option<&AppliedEffects>,
        Option<&AbilityBook>,
    )>();
    let mut rows: Vec<String> = query
        .iter(world)
        .map(|(name, attrs, effects, book)| {
            let mut row = name.as_str().to_owned();
            for a in attrs.map(Attributes::list).unwrap_or_default() {
                row.push_str(&format!(
                    " {}={:08x}/{:08x}",
                    a.name,
                    a.base.to_bits(),
                    a.current.to_bits()
                ));
            }
            for x in effects.map_or(&[][..], |l| &l.list[..]) {
                row.push_str(&format!(
                    " e{}:{}:{}:{}",
                    x.id.0, x.stacks, x.remaining_ticks, x.suspended
                ));
            }
            for g in book.map_or(&[][..], |b| &b.grants[..]) {
                row.push_str(&format!(" g{}:{:?}", g.id.0, g.state));
            }
            row
        })
        .collect();
    rows.sort();
    for row in rows {
        fold.text(&row);
    }
}

fn spawn_unit(world: &mut World, units: &mut Units, i: usize) -> Entity {
    let mut entity = world.spawn((unit_attributes(i), Name::new(format!("unit{i:03}"))));
    if i % 4 != 3 {
        entity.insert(EntityGuid(Guid(5_000 + (i as u128 * 7919) % 1_000)));
    }
    if i.is_multiple_of(2) {
        entity.insert(TagContainer::from_names(["State.Armed"]).expect("tags"));
    }
    let id = entity.id();
    units.add(id, i);
    id
}

/// Runs the scene and returns its trace hash and the folded messages. `pad` empty entities are
/// spawned before the units, which shifts every unit's entity id the way a resource stored as an
/// entity does when a plugin adds one.
fn scene(pad: usize) -> (u64, Fold) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AbilityPlugin::default());
    app.update();
    effects(&mut app);
    let mut cursors = Cursors::default();
    let mut fold = Fold(FNV_OFFSET, Vec::new());
    let mut rng = Rng::new(0x2426);
    let mut units: Vec<Entity> = Vec::new();
    let mut spawned = Units::default();
    let mut next = 0usize;
    let world = app.world_mut();
    for _ in 0..pad {
        world.spawn_empty();
    }
    for _ in 0..40 {
        units.push(spawn_unit(world, &mut spawned, next));
        next += 1;
    }
    let mut grants: Vec<(Entity, GrantId)> = Vec::new();
    for (n, &u) in units.iter().enumerate().filter(|(n, _)| n.is_multiple_of(3)) {
        for path in ["bolt", "haste", "react"] {
            let g = grant_ability(world, u, path, GrantOptions::default()).expect("grant");
            if path == "bolt" {
                grants.push((u, g));
            }
        }
        if n.is_multiple_of(2) {
            apply_effect(world, ApplyEffect::new("blessing", u)).expect("blessing");
        }
    }
    for s in 0..720usize {
        let world = app.world_mut();
        for _ in 0..3 {
            let target = units[rng.range(0..units.len())];
            let source = units[rng.range(0..units.len())];
            let k = rng.range(0..12u32);
            let _ = apply_effect(
                world,
                ApplyEffect::new(&format!("e{k}"), target).from_source(source),
            );
        }
        if s.is_multiple_of(11) {
            let target = units[rng.range(0..units.len())];
            let _ = apply_effect(world, ApplyEffect::new("heal", target));
        }
        if s.is_multiple_of(7) && !grants.is_empty() {
            let (owner, g) = grants[rng.range(0..grants.len())];
            if world.get_entity(owner).is_ok() {
                let _ = activate(world, owner, g, StartContext::default());
            }
        }
        if s % 37 == 5 {
            units.push(spawn_unit(world, &mut spawned, next));
            next += 1;
        }
        if s % 53 == 20 && units.len() > 20 {
            let gone = units.remove(rng.range(0..units.len()));
            world.despawn(gone);
            grants.retain(|(owner, _)| *owner != gone);
        }
        if s % 41 == 9 {
            let u = units[rng.range(0..units.len())];
            if world.get::<EntityGuid>(u).is_some() {
                world.entity_mut(u).remove::<EntityGuid>();
            } else {
                world.entity_mut(u).insert(EntityGuid(Guid(9_000 + s as u128)));
            }
        }
        if s % 59 == 13 {
            // A guid changed in place: the entity keeps its table row, its sort key moves.
            let u = units[rng.range(0..units.len())];
            if let Some(mut guid) = world.get_mut::<EntityGuid>(u) {
                guid.0 = Guid(3_000 + s as u128);
            }
        }
        if s % 29 == 3 {
            let u = units[rng.range(0..units.len())];
            let value = rng.range(0..90u32) as f32;
            let _ = set_base(world, u, AttrId::of("mana"), value);
        }
        if s % 31 == 17 {
            let u = units[rng.range(0..units.len())];
            let names: &[&str] = match rng.range(0..3u32) {
                0 => &["State.Armed"],
                1 => &["State.Sprinting"],
                _ => &[],
            };
            world
                .entity_mut(u)
                .insert(TagContainer::from_names(names.iter().copied()).expect("tags"));
        }
        if s % 43 == 30 {
            let u = units[rng.range(0..units.len())];
            remove_effects_matching(world, u, &q(r#"Has("Status")"#), Some(2));
        }
        if s % 47 == 8 {
            // A write that goes around `set_base`, as a reflection edit would.
            let u = units[rng.range(0..units.len())];
            if let Some(mut attrs) = world.get_mut::<Attributes>(u) {
                attrs.set_current_for_test(AttrId::of("speed"), 1.5 + s as f32 * 0.01);
            }
        }
        if s == 300 {
            for &u in units.iter().step_by(4) {
                if let Some(mut attrs) = world.get_mut::<Attributes>(u) {
                    let _ = attrs.push(Attribute::new("focus", 5.0, 0.0, 10.0));
                    let _ = attrs.set_base(AttrId::of("health"), 55.0);
                }
            }
        }
        if s % 97 == 50 {
            // A component replaced whole, as a load does: no snapshot, baseline only.
            let u = units[rng.range(0..units.len())];
            world.entity_mut(u).insert(unit_attributes(s));
        }
        world.run_schedule(FixedUpdate);
        fold.u64(s as u64);
        fold_step(world, &mut cursors, &mut fold, &spawned);
    }
    (fold.0, fold)
}

/// ADR-2426: the reworked storage behaves exactly as the old one did on this scene.
#[test]
fn the_reworked_storage_gives_the_trace_of_the_old_one() {
    eprintln!("size_of::<Attributes>() = {}", size_of::<Attributes>());
    let (a, fold) = scene(0);
    let (b, _) = scene(0);
    assert_eq!(a, b, "the scene is deterministic");
    // The scene reaches the message kinds the reworked storage orders or reports.
    for kind in ["attribute", "tags", "lifecycle", "applied", "ended", "stacks"] {
        assert!(fold.count(kind) > 0, "no {kind} message");
    }
    assert_eq!(a, BASE_TRACE, "trace {a:#018x}");
}

/// ADR-5960: the trace keys units by their spawn index, not by their entity bits, so a resource
/// or any other entity spawned before the units (which shifts every unit's id) leaves it alone.
#[test]
fn the_trace_does_not_depend_on_entity_ids() {
    let (a, _) = scene(0);
    for pad in [1, 7] {
        let (b, _) = scene(pad);
        assert_eq!(
            a, b,
            "{pad} entities spawned first moved the trace: {a:#018x} vs {b:#018x}"
        );
    }
}

/// The message text names a unit by its spawn index wherever its entity appears, and leaves
/// numbers that are not entities alone.
#[test]
fn message_text_names_units_by_spawn_index() {
    let mut world = World::new();
    world.spawn_empty();
    let mut units = Units::default();
    let a = world.spawn_empty().id();
    units.add(a, 4);
    let text = format!("X {{ entity: {a:?}, source: Some({a:?}), v: 1.5, n: 12, id: E7v2x }}");
    assert_eq!(
        units.stable(&text),
        "X { entity: unit4, source: Some(unit4), v: 1.5, n: 12, id: E7v2x }"
    );
}
