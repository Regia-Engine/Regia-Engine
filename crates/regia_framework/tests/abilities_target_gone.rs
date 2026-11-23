// SPDX-License-Identifier: MPL-2.0
//! Spec 53 § 2.4 `target_gone`: a target that despawns ends its live effects with
//! `EffectEnded { reason: TargetGone }` in the next effect pass (one step after the despawn), once
//! per effect, in § 7 order. A headless app with `MinimalPlugins` and a 60 Hz fixed step.

use regia_app::prelude::*;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::abilities::*;
use regia_framework::tags::Tag;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AbilityPlugin::default());
    step(&mut app);
    app
}

/// Runs updates until exactly one more fixed step ran.
fn step(app: &mut App) {
    let before = app
        .world()
        .get_resource::<AbilityCounters>()
        .map_or(0, |c| c.tick);
    for _ in 0..4 {
        app.update();
        if app
            .world()
            .get_resource::<AbilityCounters>()
            .is_some_and(|c| c.tick > before)
        {
            return;
        }
    }
    panic!("the fixed step did not run");
}

struct Log<M: Message> {
    cursor: MessageCursor<M>,
}

impl<M: Message + Clone> Log<M> {
    fn new() -> Self {
        Self {
            cursor: MessageCursor::default(),
        }
    }

    fn read(&mut self, app: &App) -> Vec<M> {
        app.world()
            .get_resource::<Messages<M>>()
            .map(|m| self.cursor.read(m).cloned().collect())
            .unwrap_or_default()
    }
}

fn target(app: &mut App) -> Entity {
    let attrs = Attributes::from_list(vec![Attribute::new("x", 0.0, 0.0, 100.0)]).expect("attributes");
    app.world_mut().spawn(attrs).id()
}

fn add(app: &mut App, path: &str, asset: EffectAsset) {
    app.world_mut()
        .resource_mut::<AbilityAssets>()
        .insert_effect(path, asset);
}

fn timed(seconds: f32) -> EffectAsset {
    EffectAsset::new(Lifetime::Timed(Magnitude::Const(seconds)))
}

fn apply(app: &mut App, path: &str, e: Entity) -> EffectId {
    apply_effect(app.world_mut(), ApplyEffect::new(path, e))
        .expect("apply")
        .id
        .expect("a live effect")
}

fn gone(ended: &[EffectEnded]) -> Vec<(Entity, EffectId)> {
    ended
        .iter()
        .filter(|m| m.reason == EndReason::TargetGone)
        .map(|m| (m.target, m.id))
        .collect()
}

#[test]
fn a_despawned_target_ends_its_effects_with_target_gone_the_step_after() {
    let mut app = app();
    let e = target(&mut app);
    add(&mut app, "burn", timed(10.0));
    add(&mut app, "aura", EffectAsset::new(Lifetime::UntilRemoved));
    let burn = apply(&mut app, "burn", e);
    let aura = apply(&mut app, "aura", e);
    step(&mut app);
    let mut ended: Log<EffectEnded> = Log::new();
    let live_before = app.world().resource::<AbilityStats>().live_effects;
    let removals_before = app.world().resource::<AbilityStats>().removals;
    app.world_mut().despawn(e);
    assert!(
        gone(&ended.read(&app)).is_empty(),
        "nothing ends before the next effect pass"
    );
    step(&mut app);
    let all = ended.read(&app);
    assert_eq!(
        gone(&all),
        vec![(e, burn), (e, aura)],
        "one target_gone per live effect, ascending id: {all:?}"
    );
    assert_eq!(all.len(), 2, "no other ending: {all:?}");
    assert_eq!(EndReason::TargetGone.code(), "target_gone");
    let stats = app.world().resource::<AbilityStats>();
    assert_eq!(stats.removals, removals_before + 2);
    assert_eq!(stats.ended_count(EndReason::TargetGone), 2);
    assert!(stats.live_effects <= live_before.saturating_sub(2));
    step(&mut app);
    assert!(ended.read(&app).is_empty(), "written once");
}

#[test]
fn two_targets_end_in_entity_order_then_id_order() {
    let mut app = app();
    let a = target(&mut app);
    let b = target(&mut app);
    add(&mut app, "burn", timed(10.0));
    let b1 = apply(&mut app, "burn", b);
    let a1 = apply(&mut app, "burn", a);
    let b2 = apply(&mut app, "burn", b);
    let mut ended: Log<EffectEnded> = Log::new();
    // Despawn in the opposite order of the § 7 order (entity bits): the messages still follow it.
    app.world_mut().despawn(b);
    app.world_mut().despawn(a);
    step(&mut app);
    // Neither has an `EntityGuid`, so the § 7 order is by entity bits.
    let mut want = vec![(b, b2), (a, a1), (b, b1)];
    want.sort_by_key(|(e, id)| (e.to_bits(), *id));
    assert_eq!(gone(&ended.read(&app)), want);
}

#[test]
fn nothing_is_written_without_live_effects_or_without_a_despawn() {
    let mut app = app();
    // An instant effect leaves no entry, so its target's despawn ends nothing.
    let i = target(&mut app);
    let mut hit = EffectAsset::new(Lifetime::Instant);
    hit.base_edits = vec![Modifier {
        attribute: "x".to_owned(),
        op: ModOp::AddPre,
        amount: Magnitude::Const(1.0),
        per_stack: false,
        when: None,
    }];
    add(&mut app, "hit", hit);
    apply_effect(app.world_mut(), ApplyEffect::new("hit", i)).expect("apply");
    // A plain entity with no `AppliedEffects`.
    let plain = app.world_mut().spawn_empty().id();
    // A target whose list is removed while it lives: not a despawn.
    let kept = target(&mut app);
    add(&mut app, "burn", timed(10.0));
    apply(&mut app, "burn", kept);
    step(&mut app);
    let mut ended: Log<EffectEnded> = Log::new();
    app.world_mut().despawn(i);
    app.world_mut().despawn(plain);
    app.world_mut().entity_mut(kept).remove::<AppliedEffects>();
    step(&mut app);
    step(&mut app);
    assert!(gone(&ended.read(&app)).is_empty());
}

#[test]
fn an_end_cue_follows_a_target_gone_ending() {
    let mut app = app();
    let e = target(&mut app);
    let mut glow = timed(10.0);
    let cue = Tag::new("Cue.Glow").expect("tag");
    glow.cues = vec![CueBinding {
        tag: cue,
        at: CueAt::Sustained,
        params: CueParams::default(),
    }];
    add(&mut app, "glow", glow);
    let id = apply(&mut app, "glow", e);
    step(&mut app);
    let mut cues: Log<CueEvent> = Log::new();
    app.world_mut().despawn(e);
    step(&mut app);
    let ends: Vec<CueEvent> = cues
        .read(&app)
        .into_iter()
        .filter(|c| c.kind == CueKind::End)
        .collect();
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!((ends[0].entity, ends[0].tag, ends[0].effect), (e, cue, Some(id)));
}
