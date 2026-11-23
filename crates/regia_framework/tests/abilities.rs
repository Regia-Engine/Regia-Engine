// SPDX-License-Identifier: MPL-2.0
//! Spec 53 acceptance tests (`docs/specs/53-abilities-attributes-effects.md` § 8): a headless app
//! with `MinimalPlugins`, a 60 Hz fixed step and `AbilityPlugin`. No sleeps, no timing asserts.

use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Rng;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::abilities::*;
use regia_framework::damage::{DamageEvent, DamagePlugin, Died, DiedEvent, Health};
use regia_framework::state_hash::{insert_resource_text, snapshot_hash, world_snapshot};
use regia_framework::tags::{Tag, TagContainer, TagQuery};
use regia_scene::components::{EntityGuid, Name};

fn t(name: &str) -> Tag {
    Tag::new(name).expect("valid tag")
}

fn q(text: &str) -> TagQuery {
    TagQuery::from_ron(text).expect("query")
}

fn app_with(plugin: AbilityPlugin) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(plugin);
    // Warm up so the fixed clock runs one step per update.
    step(&mut app);
    app
}

fn new_app() -> App {
    app_with(AbilityPlugin::default())
}

fn tick(app: &App) -> u64 {
    app.world().resource::<AbilityCounters>().tick
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

fn steps(app: &mut App, n: usize) {
    for _ in 0..n {
        step(app);
    }
}

fn add_effect(app: &mut App, path: &str, asset: EffectAsset) {
    app.world_mut()
        .resource_mut::<AbilityAssets>()
        .insert_effect(path, asset);
}

fn add_ability(app: &mut App, path: &str, asset: AbilityAsset) {
    app.world_mut()
        .resource_mut::<AbilityAssets>()
        .insert_ability(path, asset);
}

fn spawn_attrs(app: &mut App, list: Vec<Attribute>) -> Entity {
    let attrs = Attributes::from_list(list).expect("attributes");
    app.world_mut().spawn(attrs).id()
}

fn cur(app: &App, e: Entity, name: &str) -> f32 {
    app.world()
        .get::<Attributes>(e)
        .and_then(|a| a.get(AttrId::of(name)))
        .expect("attribute")
}

fn base(app: &App, e: Entity, name: &str) -> f32 {
    app.world()
        .get::<Attributes>(e)
        .and_then(|a| a.base(AttrId::of(name)))
        .expect("attribute")
}

fn apply(app: &mut App, req: ApplyEffect) -> ApplyOutcome {
    apply_effect(app.world_mut(), req).expect("apply")
}

fn live(app: &App, e: Entity) -> Vec<AppliedEffect> {
    app.world()
        .get::<AppliedEffects>(e)
        .map(|l| l.list.clone())
        .unwrap_or_default()
}

fn modifier(attr: &str, op: ModOp, amount: f32) -> Modifier {
    Modifier {
        attribute: attr.to_owned(),
        op,
        amount: Magnitude::Const(amount),
        per_stack: false,
        when: None,
    }
}

fn timed(seconds: f32) -> EffectAsset {
    EffectAsset::new(Lifetime::Timed(Magnitude::Const(seconds)))
}

fn forever() -> EffectAsset {
    EffectAsset::new(Lifetime::UntilRemoved)
}

fn instant() -> EffectAsset {
    EffectAsset::new(Lifetime::Instant)
}

struct Log<M: Message> {
    cursor: MessageCursor<M>,
    seen: Vec<M>,
}

impl<M: Message + Clone> Log<M> {
    fn new() -> Self {
        Self {
            cursor: MessageCursor::default(),
            seen: Vec::new(),
        }
    }

    fn read(&mut self, app: &App) -> &[M] {
        let start = self.seen.len();
        if let Some(messages) = app.world().get_resource::<Messages<M>>() {
            self.seen.extend(self.cursor.read(messages).cloned());
        }
        &self.seen[start..]
    }
}

// 1. Op order.
#[test]
fn t01_op_order_and_replace_ties() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 10.0, 0.0, 1000.0)]);
    let mut asset = forever();
    asset.live_mods = vec![
        modifier("x", ModOp::AddPre, 5.0),
        modifier("x", ModOp::Percent, 0.5),
        modifier("x", ModOp::Percent, 0.25),
        modifier("x", ModOp::Scale, 2.0),
        modifier("x", ModOp::Scale, 1.5),
        modifier("x", ModOp::AddPost, -3.0),
    ];
    add_effect(&mut app, "ops", asset);
    apply(&mut app, ApplyEffect::new("ops", e));
    step(&mut app);
    assert_eq!(cur(&app, e, "x"), 75.75);
    let breakdown = explain_attribute(app.world(), e, AttrId::of("x")).expect("breakdown");
    let after: Vec<f32> = breakdown.phases.iter().map(|p| p.value_after).collect();
    assert_eq!(after, vec![15.0, 26.25, 78.75, 75.75, 75.75]);
    assert_eq!(breakdown.phases[1].modifiers.len(), 2);

    for order in [[1i16, 2], [2, 1]] {
        let mut app = new_app();
        let e = spawn_attrs(&mut app, vec![Attribute::new("x", 10.0, 0.0, 1000.0)]);
        for p in order {
            let mut a = forever();
            a.live_mods = vec![modifier(
                "x",
                ModOp::Replace { priority: p },
                f32::from(p) * 100.0,
            )];
            add_effect(&mut app, &format!("r{p}"), a);
            apply(&mut app, ApplyEffect::new(&format!("r{p}"), e));
        }
        step(&mut app);
        assert_eq!(cur(&app, e, "x"), 200.0, "priority 2 wins in order {order:?}");
    }

    // Equal priority: the earlier applied tick wins, then the lower id.
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 10.0, 0.0, 1000.0)]);
    for (name, v) in [("a", 1.0), ("b", 2.0), ("c", 3.0)] {
        let mut a = forever();
        a.live_mods = vec![modifier("x", ModOp::Replace { priority: 1 }, v)];
        add_effect(&mut app, name, a);
    }
    apply(&mut app, ApplyEffect::new("b", e));
    step(&mut app);
    apply(&mut app, ApplyEffect::new("a", e));
    step(&mut app);
    assert_eq!(cur(&app, e, "x"), 2.0, "earlier tick wins");
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 10.0, 0.0, 1000.0)]);
    for (name, v) in [("a", 1.0), ("b", 2.0)] {
        let mut a = forever();
        a.live_mods = vec![modifier("x", ModOp::Replace { priority: 1 }, v)];
        add_effect(&mut app, name, a);
    }
    apply(&mut app, ApplyEffect::new("b", e));
    apply(&mut app, ApplyEffect::new("a", e));
    step(&mut app);
    assert_eq!(cur(&app, e, "x"), 2.0, "same tick: lower id wins");
}

// 2. Clamps and derived.
#[test]
fn t02_clamps_max_from_and_derived() {
    let mut app = new_app();
    let e = spawn_attrs(
        &mut app,
        vec![
            Attribute::new("health_max", 100.0, 1.0, 9999.0),
            Attribute::new("health", 100.0, 0.0, 9999.0).with_max_from("health_max"),
            Attribute::new("strength", 10.0, 0.0, 100.0),
            Attribute::new("armor", 0.0, 0.0, 1000.0).with_derived(&[("strength", 2.0)], 0.0),
        ],
    );
    step(&mut app);
    assert_eq!(cur(&app, e, "armor"), 20.0);
    let mut low = timed(1.0);
    low.live_mods = vec![modifier("health_max", ModOp::Scale, 0.5)];
    add_effect(&mut app, "low", low);
    let mut strong = timed(1.0);
    strong.live_mods = vec![modifier("strength", ModOp::Scale, 2.0)];
    add_effect(&mut app, "strong", strong);
    let mut huge = timed(1.0);
    huge.live_mods = vec![modifier("strength", ModOp::AddPost, 1000.0)];
    add_effect(&mut app, "huge", huge);
    apply(&mut app, ApplyEffect::new("low", e));
    apply(&mut app, ApplyEffect::new("strong", e));
    step(&mut app);
    assert_eq!(cur(&app, e, "health_max"), 50.0);
    assert_eq!(
        cur(&app, e, "health"),
        50.0,
        "health follows its ceiling the same step"
    );
    assert_eq!(cur(&app, e, "armor"), 40.0, "derived armor doubles the same step");
    apply(&mut app, ApplyEffect::new("huge", e));
    step(&mut app);
    assert_eq!(cur(&app, e, "strength"), 100.0, "current is clamped to max");
    let err = set_base(app.world_mut(), e, AttrId::of("armor"), 5.0).expect_err("derived");
    assert_eq!(err.reason(), "attribute_derived");
    let err = set_base(app.world_mut(), e, AttrId::of("strength"), f32::NAN).expect_err("nan");
    assert_eq!(err.reason(), "attribute_not_finite");
    assert_eq!(base(&app, e, "strength"), 10.0);
    let cyclic = AttributesAsset {
        format_version: 1,
        description: String::new(),
        attributes: vec![
            AttributeDecl {
                name: "a".into(),
                base: 1.0,
                min: 0.0,
                max: 10.0,
                max_from: Some("b".into()),
                derived: None,
            },
            AttributeDecl {
                name: "b".into(),
                base: 1.0,
                min: 0.0,
                max: 10.0,
                max_from: Some("a".into()),
                derived: None,
            },
        ],
    };
    assert_eq!(
        cyclic.build().map_err(|e| e.reason()).err(),
        Some("attribute_cycle")
    );
}

// 3. Base and current.
#[test]
fn t03_base_persists_and_live_mods_vanish() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("health", 100.0, 0.0, 200.0)]);
    let mut hit = instant();
    hit.base_edits = vec![modifier("health", ModOp::AddPre, -30.0)];
    add_effect(&mut app, "hit", hit);
    let mut buff = timed(0.1);
    buff.live_mods = vec![modifier("health", ModOp::AddPre, 20.0)];
    add_effect(&mut app, "buff", buff);
    apply(&mut app, ApplyEffect::new("buff", e));
    apply(&mut app, ApplyEffect::new("hit", e));
    step(&mut app);
    assert_eq!(base(&app, e, "health"), 70.0);
    assert_eq!(cur(&app, e, "health"), 90.0);
    steps(&mut app, 10);
    assert!(live(&app, e).is_empty());
    assert_eq!(base(&app, e, "health"), 70.0);
    assert_eq!(cur(&app, e, "health"), 70.0);
}

fn pulse_ticks(on_apply: bool) -> (u64, Vec<u64>, u64) {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("hits", 0.0, 0.0, 1000.0)]);
    let mut asset = timed(1.0);
    asset.pulse = Some(Pulse {
        every: 0.25,
        on_apply,
    });
    asset.base_edits = vec![modifier("hits", ModOp::AddPre, 1.0)];
    add_effect(&mut app, "p", asset);
    let applied = tick(&app);
    apply(&mut app, ApplyEffect::new("p", e));
    let mut pulses = Vec::new();
    let mut last = 0.0;
    if base(&app, e, "hits") > last {
        pulses.push(applied);
        last = base(&app, e, "hits");
    }
    let mut ended = 0;
    for _ in 0..80 {
        step(&mut app);
        if base(&app, e, "hits") > last {
            pulses.push(tick(&app));
            last = base(&app, e, "hits");
        }
        if ended == 0 && live(&app, e).is_empty() {
            ended = tick(&app);
        }
    }
    (applied, pulses, ended)
}

// 4. Ticks.
#[test]
fn t04_timed_lengths_and_pulses_are_integer_ticks() {
    let (a, pulses, ended) = pulse_ticks(true);
    assert_eq!(ended, a + 60, "a 1 s effect ends on the 60th step");
    assert_eq!(pulses, vec![a, a + 15, a + 30, a + 45, a + 60]);
    let (a, pulses, ended) = pulse_ticks(false);
    assert_eq!(ended, a + 60);
    assert_eq!(pulses, vec![a + 15, a + 30, a + 45, a + 60]);
    assert_eq!(ticks_from_seconds(0.1, 60.0), 6);
    assert_eq!(ticks_from_seconds(0.001, 60.0), 1);
    // Just outside the 1e-4 snap: ceil, not the neighbouring integer.
    assert_eq!(ticks_from_seconds(6.0002, 1.0), 7);
}

// 5. Pulse catch-up.
#[test]
fn t05_pulse_catch_up_is_bounded() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("hits", 0.0, 0.0, 1000.0)]);
    let mut asset = forever();
    asset.pulse = Some(Pulse {
        every: 1.0 / 60.0,
        on_apply: false,
    });
    asset.base_edits = vec![modifier("hits", ModOp::AddPre, 1.0)];
    add_effect(&mut app, "p", asset);
    steps(&mut app, 25);
    let out = apply(&mut app, ApplyEffect::new("p", e));
    step(&mut app);
    let before = base(&app, e, "hits");
    let now = tick(&app);
    {
        let world = app.world_mut();
        let mut list = world.get_mut::<AppliedEffects>(e).expect("effects");
        let entry = list.get_mut(out.id.expect("id")).expect("entry");
        entry.next_pulse_tick = now + 1 - 20;
    }
    step(&mut app);
    assert_eq!(base(&app, e, "hits") - before, 8.0);
    assert_eq!(
        app.world()
            .resource::<AbilityStats>()
            .warning_count("pulse_overrun"),
        1
    );
}

// 6. Stacking matrix.
struct Row {
    key: StackKey,
    when_full: WhenFull,
    time: TimeOnAdd,
    lapse: OnLapse,
    third: Outcome,
    after_third: (u16, u32),
    changes: &'static [(u16, u16)],
    ends_after: Option<u32>,
}

/// What one stacking row produced: third outcome, (stacks, remaining) after it, count changes, end step.
type RowResult = (Outcome, (u16, u32), Vec<(u16, u16)>, Option<u32>);

fn run_row(row: &Row) -> RowResult {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 10.0)]);
    let src = app.world_mut().spawn_empty().id();
    let mut asset = timed(10.0 / 60.0);
    asset.stack = Some(StackRule {
        key: row.key,
        cap: 2,
        on_add: OnAdd {
            time: row.time,
            pulse: PulseOnAdd::Keep,
        },
        when_full: row.when_full.clone(),
        on_lapse: row.lapse,
        cue_each_stack: false,
    });
    add_effect(&mut app, "s", asset);
    let mut log: Log<StacksChanged> = Log::new();
    let req = ApplyEffect::new("s", e).from_source(src);
    apply(&mut app, req.clone());
    steps(&mut app, 3);
    apply(&mut app, req.clone());
    log.read(&app);
    steps(&mut app, 3);
    let third = apply(&mut app, req);
    log.read(&app);
    let entries = live(&app, e);
    assert_eq!(entries.len(), 1);
    let after = (entries[0].stacks, entries[0].remaining_ticks);
    let mut ends = None;
    for n in 1..=40u32 {
        step(&mut app);
        log.read(&app);
        if live(&app, e).is_empty() {
            ends = Some(n);
            break;
        }
    }
    log.read(&app);
    let changes = log.seen.iter().map(|c| (c.old, c.new)).collect();
    (third.outcome, after, changes, ends)
}

#[test]
fn t06_stacking_matrix() {
    use OnLapse::*;
    use TimeOnAdd::*;
    use WhenFull::*;
    let pop: &[(u16, u16)] = &[(1, 2), (2, 1), (1, 0)];
    let one: &[(u16, u16)] = &[(1, 2)];
    let restart_pop: &[(u16, u16)] = &[(1, 2), (1, 0)];
    let mut rows = Vec::new();
    for key in [StackKey::BySource, StackKey::ByTarget] {
        let r = |when_full: WhenFull, time, lapse, third, after_third, changes, ends_after| Row {
            key,
            when_full,
            time,
            lapse,
            third,
            after_third,
            changes,
            ends_after,
        };
        rows.extend([
            r(Reject, Reset, EndAll, Outcome::RejectedFull, (2, 7), one, Some(7)),
            r(
                Reject,
                Reset,
                PopOne,
                Outcome::RejectedFull,
                (2, 7),
                pop,
                Some(17),
            ),
            r(Reject, Keep, EndAll, Outcome::RejectedFull, (2, 4), one, Some(4)),
            r(Reject, Keep, PopOne, Outcome::RejectedFull, (2, 4), pop, Some(14)),
            r(
                RejectRenew,
                Reset,
                EndAll,
                Outcome::RejectedFull,
                (2, 10),
                one,
                Some(10),
            ),
            r(
                RejectRenew,
                Reset,
                PopOne,
                Outcome::RejectedFull,
                (2, 10),
                pop,
                Some(20),
            ),
            r(
                RejectRenew,
                Keep,
                EndAll,
                Outcome::RejectedFull,
                (2, 4),
                one,
                Some(4),
            ),
            r(
                RejectRenew,
                Keep,
                PopOne,
                Outcome::RejectedFull,
                (2, 4),
                pop,
                Some(14),
            ),
            r(Restart, Reset, EndAll, Outcome::Applied, (1, 10), one, Some(10)),
            r(
                Restart,
                Reset,
                PopOne,
                Outcome::Applied,
                (1, 10),
                restart_pop,
                Some(10),
            ),
            r(Restart, Keep, EndAll, Outcome::Applied, (1, 10), one, Some(10)),
            r(
                Restart,
                Keep,
                PopOne,
                Outcome::Applied,
                (1, 10),
                restart_pop,
                Some(10),
            ),
            r(Reject, Add, EndAll, Outcome::RejectedFull, (2, 14), one, Some(14)),
            r(
                RejectRenew,
                Add,
                EndAll,
                Outcome::RejectedFull,
                (2, 24),
                one,
                Some(24),
            ),
            r(Reject, Reset, Renew, Outcome::RejectedFull, (2, 7), one, None),
        ]);
    }
    assert!(rows.len() >= 24);
    for row in &rows {
        let (third, after, changes, ends) = run_row(row);
        let label = format!("{:?} {:?} {:?} {:?}", row.key, row.when_full, row.time, row.lapse);
        assert_eq!(third, row.third, "{label}");
        assert_eq!(after, row.after_third, "{label}");
        assert_eq!(changes, row.changes, "{label}");
        assert_eq!(ends, row.ends_after, "{label}");
    }
}

// 7. By source.
#[test]
fn t07_by_source_keeps_entries_apart() {
    for (key, entries) in [(StackKey::BySource, 2usize), (StackKey::ByTarget, 1)] {
        let mut app = new_app();
        let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 10.0)]);
        let a = app.world_mut().spawn_empty().id();
        let b = app.world_mut().spawn_empty().id();
        let mut asset = timed(10.0 / 60.0);
        asset.stack = Some(StackRule {
            key,
            cap: 5,
            on_add: OnAdd::default(),
            when_full: WhenFull::Reject,
            on_lapse: OnLapse::EndAll,
            cue_each_stack: false,
        });
        add_effect(&mut app, "poison", asset);
        apply(&mut app, ApplyEffect::new("poison", e).from_source(a));
        steps(&mut app, 5);
        apply(&mut app, ApplyEffect::new("poison", e).from_source(b));
        assert_eq!(live(&app, e).len(), entries, "{key:?}");
        if key == StackKey::BySource {
            steps(&mut app, 5);
            assert_eq!(live(&app, e).len(), 1, "the first ends alone");
            assert_eq!(live(&app, e)[0].source, Some(b));
            steps(&mut app, 5);
            assert!(live(&app, e).is_empty());
        } else {
            assert_eq!(live(&app, e)[0].stacks, 2);
        }
    }
}

// 8. Counted tags.
#[test]
fn t08_counted_tags() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 10.0)]);
    let poisoned = t("Status.Poisoned");
    let mut short = timed(5.0 / 60.0);
    short.status_tags = vec![poisoned];
    let mut long = timed(10.0 / 60.0);
    long.status_tags = vec![poisoned];
    add_effect(&mut app, "short", short);
    add_effect(&mut app, "long", long);
    let mut log: Log<EffectiveTagsChanged> = Log::new();
    apply(&mut app, ApplyEffect::new("short", e));
    apply(&mut app, ApplyEffect::new("long", e));
    for _ in 0..5 {
        step(&mut app);
        log.read(&app);
    }
    assert!(live(&app, e).len() == 1);
    assert!(
        effective_has(app.world(), e, poisoned),
        "the second source keeps it"
    );
    assert!(effective_has(app.world(), e, t("Status")), "hierarchy match");
    for _ in 0..6 {
        step(&mut app);
        log.read(&app);
    }
    assert!(!effective_has(app.world(), e, poisoned));
    let adds: usize = log.seen.iter().filter(|c| c.added.contains(&poisoned)).count();
    let removes: usize = log.seen.iter().filter(|c| c.removed.contains(&poisoned)).count();
    assert_eq!((adds, removes), (1, 1));
}

// 9. Immunity, cleanse, conditions.
#[test]
fn t09_immunity_cleanse_and_suspension() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1000.0)]);
    let mut ward = forever();
    ward.immunity = vec![q(r#"Has("Effect.Fire")"#)];
    add_effect(&mut app, "ward", ward);
    let mut burn = forever();
    burn.kind_tags = vec![t("Effect.Fire.Burn")];
    add_effect(&mut app, "burn", burn);
    apply(&mut app, ApplyEffect::new("ward", e));
    let mut refused: Log<EffectRefused> = Log::new();
    assert_eq!(
        apply(&mut app, ApplyEffect::new("burn", e)).outcome,
        Outcome::BlockedImmunity
    );
    assert_eq!(refused.read(&app).len(), 1);

    let mut poison = forever();
    poison.kind_tags = vec![t("Effect.Damage.Poison")];
    poison.status_tags = vec![t("Status.Poisoned")];
    add_effect(&mut app, "poison", poison);
    let mut cure = forever();
    cure.cleanses = vec![q(r#"Has("Effect.Damage")"#)];
    cure.status_tags = vec![t("Status.Cured")];
    add_effect(&mut app, "cure", cure);
    apply(&mut app, ApplyEffect::new("poison", e));
    let mut ended: Log<EffectEnded> = Log::new();
    apply(&mut app, ApplyEffect::new("cure", e));
    let reasons: Vec<EndReason> = ended.read(&app).iter().map(|x| x.reason).collect();
    assert_eq!(reasons, vec![EndReason::Cleansed]);
    assert!(!effective_has(app.world(), e, t("Status.Poisoned")));
    assert!(effective_has(app.world(), e, t("Status.Cured")));

    // active_if under each on_resume, and freeze_clock.
    for (on_resume, freeze) in [
        (OnResume::Continue, false),
        (OnResume::Restart, false),
        (OnResume::FireAndRestart, false),
        (OnResume::Continue, true),
    ] {
        let mut app = new_app();
        let e = spawn_attrs(&mut app, vec![Attribute::new("hits", 0.0, 0.0, 1000.0)]);
        app.world_mut()
            .entity_mut(e)
            .insert(TagContainer::from_names(["State.Awake"]).expect("names"));
        let mut a = timed(1.0);
        a.pulse = Some(Pulse {
            every: 10.0 / 60.0,
            on_apply: false,
        });
        a.base_edits = vec![modifier("hits", ModOp::AddPre, 1.0)];
        a.active_if = Some(q(r#"Has("State.Awake")"#));
        a.status_tags = vec![t("Status.Hasted")];
        a.suspend = SuspendRule {
            freeze_clock: freeze,
            on_resume,
        };
        add_effect(&mut app, "a", a);
        let start = tick(&app);
        apply(&mut app, ApplyEffect::new("a", e));
        steps(&mut app, 3);
        let next_before = live(&app, e)[0].next_pulse_tick;
        assert_eq!(next_before, start + 10);
        let remaining_before = live(&app, e)[0].remaining_ticks;
        app.world_mut()
            .get_mut::<TagContainer>(e)
            .expect("tags")
            .remove(t("State.Awake"));
        step(&mut app);
        assert!(live(&app, e)[0].suspended);
        assert!(!effective_has(app.world(), e, t("Status.Hasted")));
        steps(&mut app, 19);
        let remaining_mid = live(&app, e)[0].remaining_ticks;
        if freeze {
            assert_eq!(remaining_mid, remaining_before, "the clock is frozen");
        } else {
            assert_eq!(remaining_mid, remaining_before - 20);
        }
        assert_eq!(base(&app, e, "hits"), 0.0, "no pulse while suspended");
        app.world_mut()
            .get_mut::<TagContainer>(e)
            .expect("tags")
            .add(t("State.Awake"));
        step(&mut app);
        let now = tick(&app);
        let entry = &live(&app, e)[0];
        assert!(!entry.suspended);
        match on_resume {
            OnResume::Continue => {
                // The stored tick is behind: the missed pulses run at once (catch-up), then it keeps the grid.
                assert_eq!(base(&app, e, "hits"), 2.0);
                assert_eq!(entry.next_pulse_tick, next_before + 20);
            }
            OnResume::Restart => {
                assert_eq!(base(&app, e, "hits"), 0.0);
                assert_eq!(entry.next_pulse_tick, now + 10);
            }
            OnResume::FireAndRestart => {
                assert_eq!(base(&app, e, "hits"), 1.0);
                assert_eq!(entry.next_pulse_tick, now + 10);
            }
        }
    }
}

fn magnitude_twice(input: &MagnitudeInput) -> f32 {
    2.0 * input.level as f32
}

fn magnitude_stack_seconds(input: &MagnitudeInput) -> f32 {
    f32::from(input.stacks)
}

// Edge: a merge renews the timer with the stack count the entry has after the merge.
#[test]
fn merge_renews_duration_with_the_new_stack_count() {
    let mut app = app_with(AbilityPlugin::default().magnitude_fn("stack_seconds", magnitude_stack_seconds));
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 10.0)]);
    let mut asset = EffectAsset::new(Lifetime::Timed(Magnitude::Function("stack_seconds".into())));
    asset.stack = Some(StackRule {
        key: StackKey::ByTarget,
        cap: 3,
        on_add: OnAdd {
            time: TimeOnAdd::Reset,
            pulse: PulseOnAdd::Keep,
        },
        when_full: WhenFull::Reject,
        on_lapse: OnLapse::EndAll,
        cue_each_stack: false,
    });
    add_effect(&mut app, "s", asset);
    let req = ApplyEffect::new("s", e);
    apply(&mut app, req.clone());
    apply(&mut app, req);
    let entries = live(&app, e);
    assert_eq!(entries[0].stacks, 2);
    assert!(
        entries[0].remaining_ticks > 100,
        "two stacks renew for ~120 ticks, got {}",
        entries[0].remaining_ticks
    );
}

// 10. Amount sources.
#[test]
fn t10_amount_sources() {
    let mut app = app_with(AbilityPlugin::default().magnitude_fn("twice", magnitude_twice));
    let e = spawn_attrs(
        &mut app,
        vec![
            Attribute::new("x", 0.0, -1000.0, 1000.0),
            Attribute::new("power", 10.0, 0.0, 100.0),
        ],
    );
    let src = spawn_attrs(&mut app, vec![Attribute::new("power", 5.0, 0.0, 100.0)]);
    let curve = InlineCurve {
        name: "poison".into(),
        points: vec![(0.0, 0.0), (8.0, -10.0)],
    };
    let mut c = instant();
    c.curves = vec![curve];
    c.base_edits = vec![Modifier {
        amount: Magnitude::Curve(LevelCurveRef {
            table: String::new(),
            name: "poison".into(),
        }),
        ..modifier("x", ModOp::AddPre, 0.0)
    }];
    add_effect(&mut app, "curve", c.clone());
    apply(&mut app, ApplyEffect::new("curve", e).at_level(4));
    assert_eq!(base(&app, e, "x"), -5.0);
    c.base_edits[0].amount = Magnitude::Curve(LevelCurveRef {
        table: String::new(),
        name: "nope".into(),
    });
    add_effect(&mut app, "missing", c);
    apply(&mut app, ApplyEffect::new("missing", e));
    assert_eq!(base(&app, e, "x"), -5.0);
    let stats = app.world().resource::<AbilityStats>();
    assert_eq!(stats.warning_count("curve_missing"), 1);

    let key = t("Param.Amount");
    let mut p = instant();
    p.base_edits = vec![Modifier {
        amount: Magnitude::Param { key, default: 1.0 },
        ..modifier("x", ModOp::AddPre, 0.0)
    }];
    add_effect(&mut app, "param", p);
    apply(&mut app, ApplyEffect::new("param", e).with_param(key, 7.0));
    assert_eq!(base(&app, e, "x"), 2.0);
    apply(&mut app, ApplyEffect::new("param", e));
    assert_eq!(base(&app, e, "x"), 3.0);
    assert_eq!(
        app.world()
            .resource::<AbilityStats>()
            .warning_count("param_missing"),
        1
    );

    for (read, expect_after) in [(StatRead::AtApply, 5.0), (StatRead::Live, 50.0)] {
        let mut s = forever();
        s.live_mods = vec![Modifier {
            amount: Magnitude::Stat {
                side: StatSide::Source,
                attribute: "power".into(),
                read,
                weight: 1.0,
                offset: 0.0,
            },
            ..modifier("power", ModOp::AddPost, 0.0)
        }];
        add_effect(&mut app, "stat", s);
        let id = apply(&mut app, ApplyEffect::new("stat", e).from_source(src))
            .id
            .expect("id");
        step(&mut app);
        assert_eq!(cur(&app, e, "power"), 15.0);
        set_base(app.world_mut(), src, AttrId::of("power"), 50.0).expect("set");
        steps(&mut app, 2);
        assert_eq!(cur(&app, e, "power"), 10.0 + expect_after, "{read:?}");
        remove_effect(app.world_mut(), e, id).expect("remove");
        set_base(app.world_mut(), src, AttrId::of("power"), 5.0).expect("set");
        step(&mut app);
    }

    let mut f = instant();
    f.base_edits = vec![Modifier {
        amount: Magnitude::Function("twice".into()),
        ..modifier("x", ModOp::AddPre, 0.0)
    }];
    add_effect(&mut app, "fn", f.clone());
    apply(&mut app, ApplyEffect::new("fn", e).at_level(3));
    assert_eq!(base(&app, e, "x"), 9.0);
    f.base_edits[0].amount = Magnitude::Function("unregistered".into());
    add_effect(&mut app, "nofn", f);
    apply(&mut app, ApplyEffect::new("nofn", e));
    assert_eq!(base(&app, e, "x"), 9.0);
    assert_eq!(
        app.world()
            .resource::<AbilityStats>()
            .warning_count("magnitude_fn_missing"),
        1
    );
}

fn ability_setup() -> (App, Entity) {
    let mut app = new_app();
    let owner = spawn_attrs(
        &mut app,
        vec![
            Attribute::new("mana", 30.0, 0.0, 100.0),
            Attribute::new("health", 100.0, 0.0, 100.0),
        ],
    );
    let mut cost = instant();
    cost.base_edits = vec![modifier("mana", ModOp::AddPre, -10.0)];
    add_effect(&mut app, "cost", cost);
    let mut cd = timed(10.0 / 60.0);
    cd.status_tags = vec![t("Cooldown.Bolt")];
    add_effect(&mut app, "cd", cd);
    let mut bolt = AbilityAsset::new("bolt");
    bolt.kind_tags = vec![t("Ability.Spell.Fire")];
    bolt.run_tags = vec![t("State.Casting")];
    bolt.cost = Some("cost".into());
    bolt.cooldown = Some("cd".into());
    add_ability(&mut app, "bolt", bolt);
    (app, owner)
}

fn lifecycle_codes(log: &[AbilityLifecycle]) -> Vec<String> {
    log.iter()
        .map(|l| match l.event {
            LifecycleEvent::Refused(r) => format!("refused:{}", r.cause.code()),
            LifecycleEvent::Ending { cancelled } => format!("ending:{cancelled}"),
            ref other => other.code().to_owned(),
        })
        .collect()
}

// 11. States.
#[test]
fn t11_state_table_and_refusals() {
    let (mut app, owner) = ability_setup();
    let mut log: Log<AbilityLifecycle> = Log::new();
    let g = grant_ability(app.world_mut(), owner, "bolt", GrantOptions::default()).expect("grant");
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start");
    assert_eq!(base(&app, owner, "mana"), 20.0, "OnStart charges");
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Running));
    assert!(effective_has(app.world(), owner, t("State.Casting")));
    let refusal = check_activation(app.world(), owner, g, &StartContext::default()).expect_err("running");
    assert_eq!(refusal.cause, RefusalCause::AlreadyRunning);
    end(app.world_mut(), owner, g).expect("end");
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Ending));
    step(&mut app);
    log.read(&app);
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Cooling));
    assert!(!effective_has(app.world(), owner, t("State.Casting")));
    let refusal = check_activation(app.world(), owner, g, &StartContext::default()).expect_err("cooling");
    assert_eq!(refusal.cause, RefusalCause::OnCooldown);
    assert_eq!(refusal.tag, Some(t("Cooldown.Bolt")));
    steps(&mut app, 10);
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Ready));
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start again");
    cancel(app.world_mut(), owner, g).expect("cancel");
    step(&mut app);
    log.read(&app);
    assert_eq!(
        lifecycle_codes(&log.seen),
        [
            "started",
            "charged",
            "ending:false",
            "ended",
            "started",
            "charged",
            "cancelled",
            "ending:true",
            "ended"
        ]
    );
    steps(&mut app, 12);
    // Cannot afford: mana 10 - 10 = 0 is fine once; then 0 - 10 is short.
    activate(app.world_mut(), owner, g, StartContext::default()).expect("third");
    end(app.world_mut(), owner, g).expect("end");
    steps(&mut app, 12);
    assert_eq!(base(&app, owner, "mana"), 0.0);
    let refusal = activate(app.world_mut(), owner, g, StartContext::default()).expect_err("broke");
    assert_eq!(refusal.cause, RefusalCause::CannotAfford);
    assert_eq!(refusal.attribute, Some(AttrId::of("mana")));
    assert_eq!(base(&app, owner, "mana"), 0.0, "a failed cost changes nothing");

    // Not cancelable.
    let (mut app, owner) = ability_setup();
    let mut stuck = AbilityAsset::new("stuck");
    stuck.cancelable = false;
    add_ability(&mut app, "stuck", stuck);
    let g = grant_ability(app.world_mut(), owner, "stuck", GrantOptions::default()).expect("grant");
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start");
    let err = cancel(app.world_mut(), owner, g).expect_err("not cancelable");
    assert_eq!(err.reason(), "not_cancelable");
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Running));

    // Explicit charge pays only when charged.
    let (mut app, owner) = ability_setup();
    let mut channel = AbilityAsset::new("channel");
    channel.cost = Some("cost".into());
    channel.charge = ChargeMode::Explicit;
    add_ability(&mut app, "channel", channel);
    let g = grant_ability(app.world_mut(), owner, "channel", GrantOptions::default()).expect("grant");
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start");
    assert_eq!(base(&app, owner, "mana"), 30.0);
    charge(app.world_mut(), owner, g).expect("charge");
    assert_eq!(base(&app, owner, "mana"), 20.0);

    // Every refusal cause from a crafted setup.
    let (mut app, owner) = ability_setup();
    let world = app.world_mut();
    let err = check_activation(world, owner, GrantId(99), &StartContext::default()).expect_err("x");
    assert_eq!(err.cause, RefusalCause::NotGranted);
    let g = grant_ability(world, owner, "bolt", GrantOptions::default()).expect("grant");
    world.entity_mut(owner).insert(AbilitiesLocked);
    assert_eq!(
        check_activation(world, owner, g, &StartContext::default()).map_err(|r| r.cause),
        Err(RefusalCause::OwnerLocked)
    );
    world.entity_mut(owner).remove::<AbilitiesLocked>();
    bind_input(world, owner, g, Some(3)).expect("bind");
    block_input(world, owner, 3, true);
    assert_eq!(
        check_activation(world, owner, g, &StartContext::default()).map_err(|r| r.cause),
        Err(RefusalCause::InputBlocked)
    );
    block_input(world, owner, 3, false);
    let zero = grant_ability(
        world,
        owner,
        "bolt",
        GrantOptions {
            level: 0,
            ..GrantOptions::default()
        },
    )
    .expect("grant");
    assert_eq!(
        check_activation(world, owner, zero, &StartContext::default()).map_err(|r| r.cause),
        Err(RefusalCause::LevelZero)
    );
    let mut picky = AbilityAsset::new("picky");
    picky.owner_needs = Some(q(r#"Has("State.Armed")"#));
    picky.owner_forbids = Some(q(r#"Has("Status.Silenced")"#));
    picky.target_needs = Some(q(r#"Has("Team.Enemy")"#));
    world
        .resource_mut::<AbilityAssets>()
        .insert_ability("picky", picky);
    let p = grant_ability(world, owner, "picky", GrantOptions::default()).expect("grant");
    let r = check_activation(world, owner, p, &StartContext::default()).expect_err("missing");
    assert_eq!(
        (r.cause, r.tag),
        (RefusalCause::OwnerMissingTag, Some(t("State.Armed")))
    );
    world
        .entity_mut(owner)
        .insert(TagContainer::from_names(["State.Armed", "Status.Silenced"]).expect("names"));
    let r = check_activation(world, owner, p, &StartContext::default()).expect_err("forbidden");
    assert_eq!(
        (r.cause, r.tag),
        (RefusalCause::OwnerForbiddenTag, Some(t("Status.Silenced")))
    );
    world
        .entity_mut(owner)
        .insert(TagContainer::from_names(["State.Armed"]).expect("names"));
    let friend = world
        .spawn(TagContainer::from_names(["Team.Friend"]).expect("names"))
        .id();
    let ctx = StartContext {
        picks: Some(Picks::new(vec![Pick::Entity(friend)])),
        ..StartContext::default()
    };
    let r = check_activation(world, owner, p, &ctx).expect_err("target");
    assert_eq!(
        (r.cause, r.tag),
        (RefusalCause::TargetRejected, Some(t("Team.Enemy")))
    );
    let mut wall = AbilityAsset::new("wall");
    wall.blocks = Some(q(r#"Has("Ability.Spell")"#));
    world.resource_mut::<AbilityAssets>().insert_ability("wall", wall);
    let w = grant_ability(world, owner, "wall", GrantOptions::default()).expect("grant");
    activate(world, owner, w, StartContext::default()).expect("wall");
    let r = check_activation(world, owner, g, &StartContext::default()).expect_err("blocked");
    assert_eq!(
        (r.cause, r.tag),
        (RefusalCause::BlockedByAbility, Some(t("Ability.Spell")))
    );
    world.resource_mut::<AbilitySettings>().cheats.free_costs = true;
    cancel(world, owner, w).expect("cancel");
}

// 12. Cancel and block.
#[test]
fn t12_cancel_and_block() {
    let (mut app, owner) = ability_setup();
    let mut a = AbilityAsset::new("a");
    a.kind_tags = vec![t("Ability.Move.Dash")];
    a.blocks = Some(q(r#"Has("Ability.Spell")"#));
    let mut b = AbilityAsset::new("b");
    b.kind_tags = vec![t("Ability.Spell.Shield")];
    b.cancels = Some(q(r#"Has("Ability.Move")"#));
    let mut c = AbilityAsset::new("c");
    c.kind_tags = vec![t("Ability.Spell.Fire")];
    add_ability(&mut app, "a", a);
    add_ability(&mut app, "b", b);
    add_ability(&mut app, "c", c);
    let world = app.world_mut();
    let ga = grant_ability(world, owner, "a", GrantOptions::default()).expect("a");
    let gc = grant_ability(world, owner, "c", GrantOptions::default()).expect("c");
    activate(world, owner, ga, StartContext::default()).expect("a starts");
    let r = activate(world, owner, gc, StartContext::default()).expect_err("blocked");
    assert_eq!(r.cause, RefusalCause::BlockedByAbility);
    end(world, owner, ga).expect("end");
    step(&mut app);
    activate(app.world_mut(), owner, gc, StartContext::default()).expect("c starts after a ended");

    let (mut app, owner) = ability_setup();
    let mut a = AbilityAsset::new("a");
    a.kind_tags = vec![t("Ability.Move.Dash")];
    let mut b = AbilityAsset::new("b");
    b.kind_tags = vec![t("Ability.Spell.Shield")];
    b.cancels = Some(q(r#"Has("Ability.Move")"#));
    add_ability(&mut app, "a", a);
    add_ability(&mut app, "b", b);
    let world = app.world_mut();
    let ga = grant_ability(world, owner, "a", GrantOptions::default()).expect("a");
    let gb = grant_ability(world, owner, "b", GrantOptions::default()).expect("b");
    activate(world, owner, ga, StartContext::default()).expect("a");
    activate(world, owner, gb, StartContext::default()).expect("b");
    assert_eq!(ability_state(world, owner, ga), Some(AbilityState::Ending));
    assert_eq!(ability_state(world, owner, gb), Some(AbilityState::Running));
}

#[derive(serde::Serialize, serde::Deserialize, Default, Debug, PartialEq)]
struct Combo {
    hits: u32,
}

// 13. Run state.
#[test]
fn t13_run_state() {
    let mut app = app_with(AbilityPlugin::default().run_state::<Combo>("combo"));
    let owner = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1.0)]);
    for (name, mode) in [
        ("keep", RunStateMode::KeepAcrossRuns),
        ("fresh", RunStateMode::FreshEachRun),
        ("scratch", RunStateMode::ScratchOnly),
    ] {
        let mut a = AbilityAsset::new(name);
        a.run_state = mode;
        a.run_type = "combo".into();
        add_ability(&mut app, name, a);
    }
    let world = app.world_mut();
    let keep = grant_ability(world, owner, "keep", GrantOptions::default()).expect("g");
    let fresh = grant_ability(world, owner, "fresh", GrantOptions::default()).expect("g");
    let scratch = grant_ability(world, owner, "scratch", GrantOptions::default()).expect("g");
    for g in [keep, fresh, scratch] {
        activate(world, owner, g, StartContext::default()).expect("start");
        if g != scratch {
            assert_eq!(run_value::<Combo>(world, owner, g), Some(Combo::default()));
            set_run_value(world, owner, g, &Combo { hits: 3 }).expect("set");
        }
        set_scratch(world, owner, g, [1.0, 2.0, 3.0, 4.0]).expect("scratch");
        end(world, owner, g).expect("end");
    }
    step(&mut app);
    let world = app.world_mut();
    for g in [keep, fresh, scratch] {
        activate(world, owner, g, StartContext::default()).expect("again");
    }
    assert_eq!(run_value::<Combo>(world, owner, keep), Some(Combo { hits: 3 }));
    assert_eq!(run_value::<Combo>(world, owner, fresh), Some(Combo::default()));
    assert_eq!(run_value::<Combo>(world, owner, scratch), None);
    let book = world.get::<AbilityBook>(owner).expect("book");
    assert_eq!(book.get(scratch).expect("g").scratch, [1.0, 2.0, 3.0, 4.0]);
}

// 14. Signals.
#[test]
fn t14_signals() {
    let (mut app, owner) = ability_setup();
    let mut a = AbilityAsset::new("a");
    a.triggers = vec![Trigger::Signal(t("Event.Hit"))];
    add_ability(&mut app, "a", a.clone());
    add_ability(&mut app, "b", a);
    add_ability(&mut app, "plain", AbilityAsset::new("plain"));
    let world = app.world_mut();
    let ga = grant_ability(world, owner, "a", GrantOptions::default()).expect("a");
    let gb = grant_ability(world, owner, "b", GrantOptions::default()).expect("b");
    let started = signal_now(world, AbilitySignal::new(owner, t("Event.Hit.Fire")));
    assert_eq!(
        started,
        vec![ga, gb],
        "GrantId order, child tag matches parent trigger"
    );
    let none = signal_now(world, AbilitySignal::new(owner, t("Event.Heal")));
    assert!(none.is_empty());
    assert_eq!(world.resource::<AbilityStats>().signals_unmatched, 1);
    let gp = grant_ability(
        world,
        owner,
        "plain",
        GrantOptions {
            extra_triggers: vec![Trigger::Signal(t("Event.Heal"))],
            ..GrantOptions::default()
        },
    )
    .expect("plain");
    // The message path matches in AbilitySet::Triggers.
    send_signal(world, AbilitySignal::new(owner, t("Event.Heal")));
    step(&mut app);
    assert_eq!(ability_state(app.world(), owner, gp), Some(AbilityState::Running));
}

// 15. Picks.
#[test]
fn t15_picks() {
    let (mut app, owner) = ability_setup();
    let mut a = AbilityAsset::new("aim");
    a.needs_picks = true;
    a.cost = Some("cost".into());
    a.target_needs = Some(q(r#"Has("Team.Enemy")"#));
    a.pick_timeout_ticks = Some(5);
    add_ability(&mut app, "aim", a);
    let world = app.world_mut();
    let enemy = world
        .spawn(TagContainer::from_names(["Team.Enemy"]).expect("n"))
        .id();
    let friend = world
        .spawn(TagContainer::from_names(["Team.Friend"]).expect("n"))
        .id();
    let g = grant_ability(world, owner, "aim", GrantOptions::default()).expect("g");
    activate(world, owner, g, StartContext::default()).expect("start");
    assert_eq!(base(&app, owner, "mana"), 30.0, "no charge before picks");
    let world = app.world_mut();
    let r = submit_picks(world, owner, g, Picks::new(vec![Pick::Entity(friend)])).expect_err("bad pick");
    assert_eq!(r.cause, RefusalCause::TargetRejected);
    submit_picks(world, owner, g, Picks::new(vec![Pick::Entity(enemy)])).expect("submit");
    assert_eq!(base(&app, owner, "mana"), 20.0, "charged on submit");
    let world = app.world_mut();
    end(world, owner, g).expect("end");
    step(&mut app);
    // Abort ends the run.
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start");
    abort_picks(app.world_mut(), owner, g).expect("abort");
    step(&mut app);
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Ready));
    // Timeout.
    let mut log: Log<AbilityLifecycle> = Log::new();
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start");
    for _ in 0..7 {
        step(&mut app);
        log.read(&app);
    }
    assert!(lifecycle_codes(&log.seen).contains(&"refused:pick_timeout".to_owned()));
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Ready));
}

fn cue_scenario(discard: bool) -> u64 {
    let mut app = new_app();
    if discard {
        app.add_systems(PostUpdate, |mut m: ResMut<Messages<CueEvent>>| m.clear());
    } else {
        app.add_systems(Update, |mut r: MessageReader<CueEvent>| for _ in r.read() {});
    }
    let e = spawn_attrs(&mut app, vec![Attribute::new("health", 100.0, 0.0, 100.0)]);
    app.world_mut().entity_mut(e).insert(Name::new("hero"));
    let mut burn = timed(2.0);
    burn.pulse = Some(Pulse {
        every: 0.5,
        on_apply: true,
    });
    burn.base_edits = vec![modifier("health", ModOp::AddPre, -1.0)];
    burn.cues = vec![
        CueBinding {
            tag: t("Cue.Burn"),
            at: CueAt::Sustained,
            params: CueParams::default(),
        },
        CueBinding {
            tag: t("Cue.Burn.Tick"),
            at: CueAt::Pulsed,
            params: CueParams::default(),
        },
    ];
    add_effect(&mut app, "burn", burn);
    for i in 0..600 {
        if i % 150 == 0 {
            apply(&mut app, ApplyEffect::new("burn", e));
        }
        step(&mut app);
    }
    assert!(!app.world().resource::<CueLog>().since(0, 10).is_empty());
    snapshot_hash(&world_snapshot(app.world()))
}

// 16. Cues are inert.
#[test]
fn t16_cues_are_inert() {
    assert_eq!(cue_scenario(false), cue_scenario(true));
}

fn determinism_run(reverse: bool) -> u64 {
    let mut app = new_app();
    let mut names: Vec<usize> = (0..50).collect();
    if reverse {
        names.reverse();
    }
    for i in names {
        let attrs = Attributes::from_list([
            Attribute::new("health", 100.0, 0.0, 200.0),
            Attribute::new("speed", 4.0, 0.0, 20.0),
        ])
        .expect("attrs");
        app.world_mut().spawn((
            attrs,
            Name::new(format!("unit{i:02}")),
            EntityGuid(Guid(1000 + i as u128)),
        ));
    }
    for k in 0..20u32 {
        let mut a = timed(0.2 + (k % 5) as f32 * 0.3);
        a.chance = if k % 3 == 0 { 0.5 } else { 1.0 };
        a.live_mods = vec![modifier("speed", ModOp::Percent, 0.05 * k as f32)];
        a.base_edits = vec![modifier("health", ModOp::AddPre, -0.5)];
        a.pulse = Some(Pulse {
            every: 0.1,
            on_apply: false,
        });
        a.status_tags = vec![t(&format!("Status.S{k}"))];
        if k % 4 == 0 {
            a.stack = Some(StackRule {
                key: StackKey::ByTarget,
                cap: 3,
                on_add: OnAdd::default(),
                when_full: WhenFull::RejectRenew,
                on_lapse: OnLapse::PopOne,
                cue_each_stack: false,
            });
        }
        add_effect(&mut app, &format!("e{k}"), a);
    }
    let mut by_name: Vec<(String, Entity)> = Vec::new();
    {
        let world = app.world_mut();
        let mut query = world.query::<(Entity, &Name)>();
        for (e, n) in query.iter(world) {
            by_name.push((n.as_str().to_owned(), e));
        }
    }
    by_name.sort();
    let mut rng = Rng::new(17);
    for _ in 0..1000 {
        for _ in 0..3 {
            let unit = rng.range(0..50usize);
            let k = rng.range(0..20u32);
            let target = by_name[unit].1;
            apply(&mut app, ApplyEffect::new(&format!("e{k}"), target));
        }
        step(&mut app);
    }
    let mut snapshot = world_snapshot(app.world());
    assert!(insert_resource_text(
        app.world(),
        &mut snapshot,
        "AbilityCounters"
    ));
    snapshot_hash(&snapshot)
}

// 17. Determinism.
#[test]
fn t17_determinism_with_reversed_spawn_order() {
    let a = determinism_run(false);
    let b = determinism_run(false);
    let c = determinism_run(true);
    assert_eq!(a, b);
    assert_eq!(a, c);
}

const POISON: &str = r#"(format_version: 1, name: "poison",
 lifetime: Timed(Const(6.0)), pulse: Some((every: 1.0, on_apply: false)),
 base_edits: [(attribute: "health", op: AddPre, amount: Curve((table: "", name: "poison")))],
 live_mods: [(attribute: "move_speed", op: Scale, amount: Const(0.8))],
 status_tags: ["Status.Poisoned"], kind_tags: ["Effect.Damage.Poison"],
 stack: Some((key: ByTarget, cap: 3, on_add: (time: Reset, pulse: Keep), when_full: RejectRenew,
              on_lapse: EndAll, cue_each_stack: false)),
 cues: [(tag: "Cue.Status.Poison", at: Sustained)],
 curves: [(name: "poison", points: [(1.0, -2.0), (10.0, -9.0)])])"#;

const HERO: &str = r#"(format_version: 1, description: "Hero stats", attributes: [
  (name: "health_max", base: 100.0, min: 1.0, max: 9999.0),
  (name: "health", base: 100.0, min: 0.0, max: 9999.0, max_from: Some("health_max")),
  (name: "stamina", base: 50.0, min: 0.0, max: 100.0),
  (name: "strength", base: 10.0, min: 0.0, max: 100.0),
  (name: "move_speed", base: 4.0, min: 0.0, max: 12.0),
  (name: "armor", base: 0.0, min: 0.0, max: 1000.0,
   derived: Some((inputs: [("strength", 2.0)], offset: 0.0))),
])"#;

const FIRE_BOLT: &str = r#"(format_version: 1, name: "fire_bolt", kind_tags: ["Ability.Spell.Fire"], run_tags: ["State.Casting"],
 blocks: Some(Has("Ability.Spell")), owner_forbids: Some(Has("Status.Silenced")),
 cost: Some("effects/mana_10.effect.ron"), cooldown: Some("effects/cd_fire_bolt.effect.ron"),
 charge: OnStart, run_state: ScratchOnly, when_running: Reject, triggers: [], max_run_ticks: Some(120),
 input_code: Some(1), run_site: HostOnly, cues: [(tag: "Cue.Cast.Fire", at: Applied)])"#;

fn resave<T: serde::Serialize + for<'de> serde::Deserialize<'de>>(text: &str, version: u32) -> String {
    let parsed: T = parse_ron(text, version).expect("parse");
    let first = to_ron(&parsed).expect("write");
    let again: T = parse_ron(&first, version).expect("reparse");
    let second = to_ron(&again).expect("write");
    assert_eq!(first, second, "resave is byte-identical");
    first
}

// 18. Formats.
#[test]
fn t18_formats() {
    resave::<EffectAsset>(POISON, EFFECT_FORMAT_VERSION);
    resave::<AttributesAsset>(HERO, ATTRIBUTES_FORMAT_VERSION);
    resave::<AbilityAsset>(FIRE_BOLT, ABILITY_FORMAT_VERSION);
    let hero: AttributesAsset = parse_ron(HERO, 1).expect("hero");
    assert_eq!(hero.build().expect("build").len(), 6);
    let newer = POISON.replacen("format_version: 1", "format_version: 2", 1);
    let err = parse_ron::<EffectAsset>(&newer, 1).expect_err("newer");
    assert_eq!(err.code, "format_too_new");
    let typo = POISON.replacen("status_tags", "statuss_tags", 1);
    let err = parse_ron::<EffectAsset>(&typo, 1).expect_err("typo");
    assert_eq!(err.code, "unknown_field");
    assert!(err.span.is_some_and(|(line, _)| line == 5), "{err:?}");

    // Validation through the registry.
    let mut assets = AbilityAssets::default();
    let d = assets.load_text("effects/poison.effect.ron", POISON, None, None);
    assert!(!has_errors(&d), "{d:?}");
    let d = assets.load_text("abilities/fire_bolt.ability.ron", FIRE_BOLT, None, None);
    let codes: Vec<&str> = d.iter().map(|x| x.code).collect();
    assert_eq!(codes, vec!["reference_missing", "reference_missing"]);
    let bad = r#"(format_version: 1, lifetime: Instant, live_mods: [(attribute: "x", op: Scale, amount: Const(-1.0))],
        pulse: Some((every: 1.0)), stack: Some((key: ByTarget, cap: 0, when_full: Reject, on_lapse: EndAll)),
        curves: [(name: "c", points: [(2.0, 1.0), (1.0, 0.0)])])"#;
    let d = assets.load_text("bad.effect.ron", bad, None, None);
    let mut codes: Vec<&str> = d.iter().map(|x| x.code).collect();
    codes.sort_unstable();
    assert_eq!(
        codes,
        vec![
            "curve_unsorted",
            "effect_instant_live_mods",
            "effect_pulse_on_instant",
            "modifier_negative_scale",
            "stack_cap_zero"
        ]
    );
    let mut registry = regia_framework::tags::TagRegistry::new();
    registry.declare("Status.Poisoned", "").expect("declare");
    let poison: EffectAsset = parse_ron(POISON, 1).expect("poison");
    let d = validate_effect(&poison, Some(&registry));
    assert!(d.iter().any(|x| x.code == "undeclared_tag"));

    // A file round trip through a temp dir.
    let dir = regia_test_utils::unique_temp_dir("abilities_formats");
    let path = dir.join("hero.attrs.ron");
    std::fs::write(&path, HERO).expect("write");
    let d = assets.load_file(&path, None, None).expect("load");
    assert!(d.is_empty());
    assert!(assets.attributes(&path.to_string_lossy()).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

fn shield(world: &World, req: &EditRequest) -> EditVerdict {
    if req.delta < 0.0
        && world
            .get::<Name>(req.target)
            .is_some_and(|n| n.as_str() == "shielded")
    {
        EditVerdict::Scale(0.5)
    } else {
        EditVerdict::Pass
    }
}

// 19. Health bridge.
#[test]
fn t19_health_bridge() {
    let mut app = app_with(AbilityPlugin::default().gate(shield));
    app.add_plugins((DamagePlugin, HealthBridge));
    let attrs = || {
        Attributes::from_list([
            Attribute::new("health_max", 100.0, 1.0, 1000.0),
            Attribute::new("health", 100.0, 0.0, 1000.0).with_max_from("health_max"),
        ])
        .expect("attrs")
    };
    let plain = app.world_mut().spawn((attrs(), Health::new(100.0))).id();
    let shielded = app
        .world_mut()
        .spawn((attrs(), Health::new(100.0), Name::new("shielded")))
        .id();
    step(&mut app);
    assert!(app.world().get::<HealthFromAttributes>(plain).is_some());
    let mut deaths: Log<DiedEvent> = Log::new();
    for target in [plain, shielded] {
        app.world_mut().write_message(DamageEvent {
            target,
            amount: 30.0,
            source: None,
            position: None,
        });
    }
    step(&mut app);
    assert_eq!(cur(&app, plain, "health"), 70.0, "applied once, not twice");
    assert_eq!(app.world().get::<Health>(plain).map(|h| h.current), Some(70.0));
    assert_eq!(cur(&app, shielded, "health"), 85.0, "the shield gate halves it");
    let killer = app.world_mut().spawn_empty().id();
    for _ in 0..2 {
        app.world_mut().write_message(DamageEvent {
            target: plain,
            amount: 100.0,
            source: Some(killer),
            position: None,
        });
        step(&mut app);
        deaths.read(&app);
    }
    assert_eq!(deaths.seen.len(), 1);
    assert_eq!(deaths.seen[0].killer, Some(killer));
}

/// A bridged heal after death raises `health` and leaves `Died`. `alive` follows `current > 0`.
#[test]
fn a_bridged_heal_after_death_leaves_died() {
    let mut app = app_with(AbilityPlugin::default());
    app.add_plugins((DamagePlugin, HealthBridge));
    let attrs = Attributes::from_list([
        Attribute::new("health_max", 100.0, 1.0, 1000.0),
        Attribute::new("health", 100.0, 0.0, 1000.0).with_max_from("health_max"),
    ])
    .expect("attrs");
    let target = app.world_mut().spawn((attrs, Health::new(100.0))).id();
    step(&mut app);
    assert!(app.world().get::<HealthFromAttributes>(target).is_some());
    let mut deaths: Log<DiedEvent> = Log::new();
    app.world_mut().write_message(DamageEvent {
        target,
        amount: 100.0,
        source: None,
        position: None,
    });
    step(&mut app);
    deaths.read(&app);
    assert_eq!(deaths.seen.len(), 1);
    assert!(app.world().get::<Died>(target).is_some());
    assert!(!app.world().get::<HealthBridgeState>(target).expect("state").alive);

    app.world_mut().write_message(DamageEvent {
        target,
        amount: -50.0,
        source: None,
        position: None,
    });
    step(&mut app);
    deaths.read(&app);
    assert_eq!(cur(&app, target, "health"), 50.0);
    assert_eq!(app.world().get::<Health>(target).map(|h| h.current), Some(50.0));
    assert!(!app.world().get::<Health>(target).expect("health").is_dead());
    assert!(app.world().get::<Died>(target).is_some(), "Died stays");
    assert!(app.world().get::<HealthBridgeState>(target).expect("state").alive);
    assert_eq!(deaths.seen.len(), 1, "the heal raises no DiedEvent");

    app.world_mut().write_message(DamageEvent {
        target,
        amount: 50.0,
        source: None,
        position: None,
    });
    step(&mut app);
    deaths.read(&app);
    assert!(app.world().get::<Health>(target).expect("health").is_dead());
    assert!(!app.world().get::<HealthBridgeState>(target).expect("state").alive);
    assert_eq!(
        deaths.seen.len(),
        2,
        "current above zero makes a later killing hit raise DiedEvent again"
    );
}

/// Audit G finding 5 (ADR-4756): the chance stream is keyed only by a stable id (the low bits of
/// the target's `EntityGuid`); a target without one refuses deterministically, so the spawn order
/// cannot change accept versus reject.
#[test]
fn chance_rolls_are_stable_across_spawn_order_or_refuse_deterministically() {
    let trial = |extra_first: bool, guid: Option<u128>| {
        let mut app = new_app();
        app.world_mut().resource_mut::<AbilitySettings>().seed = 1;
        if extra_first {
            app.world_mut().spawn_empty();
        }
        let mut coin = instant();
        coin.chance = 0.5;
        coin.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
        add_effect(&mut app, "coin", coin);
        let e = app
            .world_mut()
            .spawn(Attributes::from_list(vec![Attribute::new("x", 0.0, 0.0, 1000.0)]).expect("attributes"))
            .id();
        if let Some(g) = guid {
            app.world_mut().entity_mut(e).insert(EntityGuid(Guid(g)));
        }
        (0..32)
            .map(|_| apply(&mut app, ApplyEffect::new("coin", e)).outcome)
            .collect::<Vec<_>>()
    };
    let refused = vec![Outcome::FailedChance; 32];
    assert_eq!(trial(false, None), refused, "no guid: a deterministic refusal");
    assert_eq!(
        trial(true, None),
        refused,
        "the spawn order cannot change the refusal"
    );
    let with_guid = trial(false, Some(7));
    assert_eq!(
        with_guid,
        trial(true, Some(7)),
        "the guid-keyed stream ignores the spawn order"
    );
    assert!(
        with_guid.contains(&Outcome::Applied) && with_guid.contains(&Outcome::FailedChance),
        "the roll still draws from the stream"
    );
}

/// Audit G finding 6 (ADR-4756): simultaneous bridged deaths are emitted in the § 7 entity order
/// (ascending `EntityGuid`, then bits), not archetype-table order, across spawn order changes and
/// respawns (what a demo seek does to a despawned entity).
#[test]
fn simultaneous_bridged_deaths_emit_in_entity_order() {
    let mut app = app_with(AbilityPlugin::default());
    app.add_plugins((DamagePlugin, HealthBridge));
    let attrs = || {
        Attributes::from_list([
            Attribute::new("health_max", 100.0, 1.0, 1000.0),
            Attribute::new("health", 100.0, 0.0, 1000.0).with_max_from("health_max"),
        ])
        .expect("attrs")
    };
    // Spawn order puts the higher guid first, so archetype-table order (what the old notify loop
    // wrote) is the reverse of the § 7 order: the emitted sequence must be guid order anyway.
    let b = app
        .world_mut()
        .spawn((attrs(), Health::new(100.0), EntityGuid(Guid(20))))
        .id();
    let a = app
        .world_mut()
        .spawn((attrs(), Health::new(100.0), EntityGuid(Guid(10))))
        .id();
    step(&mut app);
    let guid_of = |app: &App, e: Entity| app.world().get::<EntityGuid>(e).map(|g| g.0.0).expect("guid");
    let mut deaths: Log<DiedEvent> = Log::new();
    let kill_both = |app: &mut App, log: &mut Log<DiedEvent>, pair: [Entity; 2]| {
        for e in pair {
            set_base(app.world_mut(), e, AttrId::of("health"), 0.0).expect("set");
        }
        step(app);
        let start = log.seen.len();
        log.read(app);
        log.seen[start..]
            .iter()
            .map(|d| guid_of(app, d.entity))
            .collect::<Vec<_>>()
    };
    let revive = |app: &mut App, pair: [Entity; 2]| {
        for e in pair {
            set_base(app.world_mut(), e, AttrId::of("health"), 100.0).expect("set");
        }
        step(app);
    };

    let first = kill_both(&mut app, &mut deaths, [a, b]);
    assert_eq!(first, vec![10, 20], "deaths are in guid order, not spawn order");

    revive(&mut app, [a, b]);
    // A seek-style respawn (what `restore_world_demo` does): `a` despawns and comes back under a
    // new entity id, keeping its guid. Archetype-table order may or may not change; the emitted
    // order must not.
    app.world_mut().despawn(a);
    let a2 = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(a2)
        .insert((attrs(), Health::new(100.0), EntityGuid(Guid(10))));
    assert_ne!(a2, a);
    step(&mut app);
    let second = kill_both(&mut app, &mut deaths, [a2, b]);
    assert_eq!(second, first, "the respawned entity keeps its place in the order");
}

fn fixed(_: &MagnitudeInput) -> f32 {
    1.0
}
fn pass_gate(_: &World, _: &EditRequest) -> EditVerdict {
    EditVerdict::Pass
}

// 20. Hot reload registration.
#[test]
fn t20_owner_registrations_drop_and_reregister() {
    let mut world = World::new();
    world.init_resource::<MagnitudeFns>();
    world.init_resource::<EditGates>();
    world.init_resource::<RunStateTypes>();
    let game = TypeOwner::Game(7);
    world.resource_mut::<MagnitudeFns>().register(game, "f", fixed);
    world
        .resource_mut::<MagnitudeFns>()
        .register(TypeOwner::Engine, "engine", fixed);
    world.resource_mut::<EditGates>().register(game, pass_gate);
    world
        .resource_mut::<RunStateTypes>()
        .register::<Combo>(game, "combo");
    drop_owner_registrations(&mut world, game);
    assert!(world.resource::<MagnitudeFns>().get("f").is_none());
    assert!(world.resource::<MagnitudeFns>().get("engine").is_some());
    assert!(world.resource::<EditGates>().is_empty());
    assert!(!world.resource::<RunStateTypes>().contains("combo"));
    world.resource_mut::<MagnitudeFns>().register(game, "f", fixed);
    assert_eq!(world.resource::<MagnitudeFns>().names(), vec!["engine", "f"]);

    // A missing function after reload reads as 0.
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, -10.0, 10.0)]);
    let mut f = instant();
    f.base_edits = vec![Modifier {
        amount: Magnitude::Function("f".into()),
        ..modifier("x", ModOp::AddPre, 0.0)
    }];
    add_effect(&mut app, "f", f);
    apply(&mut app, ApplyEffect::new("f", e));
    assert_eq!(base(&app, e, "x"), 0.0);
}

// 22. Budget counter.
#[test]
fn t22_budget_counters() {
    let mut app = new_app();
    for k in 0..8 {
        let mut a = forever();
        a.live_mods = vec![modifier("x", ModOp::AddPre, 1.0)];
        add_effect(&mut app, &format!("e{k}"), a);
    }
    let mut ids = Vec::new();
    for _ in 0..1000 {
        let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 100.0)]);
        for k in 0..8 {
            let id = apply(&mut app, ApplyEffect::new(&format!("e{k}"), e))
                .id
                .expect("id");
            ids.push((e, id));
        }
    }
    step(&mut app);
    let stats = app.world().resource::<AbilityStats>();
    assert!(stats.modifier_evaluations <= 8000);
    assert_eq!(stats.modifier_evaluations, 8000);
    for (e, id) in ids {
        remove_effect(app.world_mut(), e, id).expect("remove");
    }
    steps(&mut app, 2);
    let stats = app.world().resource::<AbilityStats>();
    assert_eq!(stats.modifier_evaluations, 0);
    assert_eq!(stats.entities_skipped, 1000, "idle entities are skipped");
}

// Chains, spill, chance, effect grants and end_with_source (§ 2.4, § 2.5, § 7).
#[test]
fn chains_spill_chance_grants_and_sources() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1000.0)]);
    let mut bump = instant();
    bump.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
    let mut loop_a = bump.clone();
    loop_a.chain.on_apply = vec!["loop".into()];
    add_effect(&mut app, "bump", bump);
    add_effect(&mut app, "loop", loop_a);
    let mut parent = timed(2.0 / 60.0);
    parent.chain.on_apply = vec!["bump".into()];
    parent.chain.on_lapse = vec!["bump".into(), "bump".into()];
    add_effect(&mut app, "parent", parent);
    apply(&mut app, ApplyEffect::new("parent", e));
    assert_eq!(
        base(&app, e, "x"),
        0.0,
        "chained effects wait for the effect pass"
    );
    step(&mut app);
    assert_eq!(base(&app, e, "x"), 1.0);
    step(&mut app);
    assert_eq!(base(&app, e, "x"), 3.0, "on_lapse applies after expiry");
    // A self-chain stops after depth 4.
    let mut log: Log<EffectApplied> = Log::new();
    set_base(app.world_mut(), e, AttrId::of("x"), 0.0).expect("reset");
    apply(&mut app, ApplyEffect::new("loop", e));
    step(&mut app);
    log.read(&app);
    assert_eq!(base(&app, e, "x"), 5.0, "the first application plus 4 chained");
    assert!(log.seen.iter().any(|m| m.outcome == Outcome::ChainTooDeep));

    // Spill.
    let mut full = forever();
    full.stack = Some(StackRule {
        key: StackKey::ByTarget,
        cap: 1,
        on_add: OnAdd::default(),
        when_full: WhenFull::Spill("bump".into()),
        on_lapse: OnLapse::EndAll,
        cue_each_stack: false,
    });
    add_effect(&mut app, "full", full);
    apply(&mut app, ApplyEffect::new("full", e));
    let before = base(&app, e, "x");
    assert_eq!(
        apply(&mut app, ApplyEffect::new("full", e)).outcome,
        Outcome::Spilled
    );
    assert_eq!(base(&app, e, "x"), before + 1.0);

    // Chance: a fixed seed gives the same rolls; failures are outcomes. The target needs an
    // EntityGuid (the stream is guid-keyed, audit G finding 5, ADR-4756).
    let rolls = |seed: u64| {
        let mut app = new_app();
        app.world_mut().resource_mut::<AbilitySettings>().seed = seed;
        let e = app
            .world_mut()
            .spawn((
                Attributes::from_list(vec![Attribute::new("x", 0.0, 0.0, 1000.0)]).expect("attributes"),
                EntityGuid(Guid(42)),
            ))
            .id();
        let mut coin = instant();
        coin.chance = 0.5;
        coin.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
        add_effect(&mut app, "coin", coin);
        (0..64)
            .map(|_| apply(&mut app, ApplyEffect::new("coin", e)).outcome == Outcome::Applied)
            .collect::<Vec<bool>>()
    };
    let a = rolls(1);
    assert_eq!(a, rolls(1));
    assert!(a.iter().any(|x| *x) && a.iter().any(|x| !*x));

    // Effect grants end with the effect; end_with_source.
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 10.0)]);
    add_ability(&mut app, "dash", AbilityAsset::new("dash"));
    let mut boots = forever();
    boots.grants = vec![EffectGrant {
        ability: "dash".into(),
        level: 1,
        on_effect_end: OnEffectEnd::Cancel,
    }];
    boots.end_with_source = true;
    add_effect(&mut app, "boots", boots);
    let src = app.world_mut().spawn_empty().id();
    apply(&mut app, ApplyEffect::new("boots", e).from_source(src));
    assert_eq!(app.world().get::<AbilityBook>(e).map(|b| b.grants.len()), Some(1));
    app.world_mut().despawn(src);
    let mut ended: Log<EffectEnded> = Log::new();
    step(&mut app);
    assert_eq!(ended.read(&app)[0].reason, EndReason::SourceGone);
    assert_eq!(app.world().get::<AbilityBook>(e).map(|b| b.grants.len()), Some(0));
}

// Input codes, tag triggers, when_running and AttributeChanged (§ 2.2, § 2.6).
#[test]
fn input_triggers_when_running_and_change_messages() {
    let (mut app, owner) = ability_setup();
    let mut a = AbilityAsset::new("a");
    a.input_code = Some(1);
    a.when_running = WhenRunning::QueueOne;
    a.max_run_ticks = Some(2);
    add_ability(&mut app, "a", a);
    let mut b = AbilityAsset::new("b");
    b.input_code = Some(1);
    add_ability(&mut app, "b", b);
    let world = app.world_mut();
    let ga = grant_ability(world, owner, "a", GrantOptions::default()).expect("a");
    let gb = grant_ability(world, owner, "b", GrantOptions::default()).expect("b");
    world.get_mut::<AbilityBook>(owner).expect("book").one_per_press = true;
    let mut log: Log<AbilityLifecycle> = Log::new();
    world.write_message(AbilityInput {
        entity: owner,
        code: 1,
        down: true,
    });
    step(&mut app);
    assert_eq!(ability_state(app.world(), owner, ga), Some(AbilityState::Running));
    assert_eq!(
        ability_state(app.world(), owner, gb),
        Some(AbilityState::Ready),
        "one per press"
    );
    log.read(&app);
    app.world_mut().write_message(AbilityInput {
        entity: owner,
        code: 1,
        down: false,
    });
    // QueueOne: a start while running runs again after the end.
    activate(app.world_mut(), owner, ga, StartContext::default()).expect("queued");
    for _ in 0..4 {
        step(&mut app);
        log.read(&app);
    }
    let codes = lifecycle_codes(&log.seen);
    assert!(codes.contains(&"input_released".to_owned()), "{codes:?}");
    assert_eq!(codes.iter().filter(|c| *c == "started").count(), 2, "{codes:?}");

    // Tag triggers.
    let mut haste = AbilityAsset::new("haste");
    haste.triggers = vec![Trigger::WhileTagged(t("State.Sprinting"))];
    add_ability(&mut app, "haste", haste);
    let mut react = AbilityAsset::new("react");
    react.triggers = vec![Trigger::TagGained(t("Status"))];
    add_ability(&mut app, "react", react);
    let world = app.world_mut();
    let gh = grant_ability(world, owner, "haste", GrantOptions::default()).expect("h");
    let gr = grant_ability(world, owner, "react", GrantOptions::default()).expect("r");
    step(&mut app);
    app.world_mut()
        .entity_mut(owner)
        .insert(TagContainer::from_names(["State.Sprinting", "Status.Wet"]).expect("names"));
    step(&mut app);
    assert_eq!(ability_state(app.world(), owner, gh), Some(AbilityState::Running));
    assert_eq!(ability_state(app.world(), owner, gr), Some(AbilityState::Running));
    app.world_mut().entity_mut(owner).insert(TagContainer::default());
    steps(&mut app, 2);
    assert_eq!(ability_state(app.world(), owner, gh), Some(AbilityState::Ready));

    // AttributeChanged: one base and one current message for an instant edit.
    let mut changes: Log<AttributeChanged> = Log::new();
    step(&mut app);
    changes.read(&app);
    changes.seen.clear();
    set_base(app.world_mut(), owner, AttrId::of("mana"), 5.0).expect("set");
    step(&mut app);
    let got: Vec<(AttrLane, f32)> = changes
        .read(&app)
        .iter()
        .filter(|c| c.attribute == AttrId::of("mana"))
        .map(|c| (c.lane, c.new))
        .collect();
    assert_eq!(got, vec![(AttrLane::Base, 5.0), (AttrLane::Current, 5.0)]);
}

/// Reflect-serializes `value` to RON, reads it back and writes it again.
fn reflect_resave<T: Reflect + GetTypeRegistration>(registry: &TypeRegistry, value: &T) -> (String, String) {
    use regia_ecs::reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
    use serde::de::DeserializeSeed;
    let first =
        ron::to_string(&TypedReflectSerializer::new(value.as_partial_reflect(), registry)).expect("write");
    let registration = registry.get(std::any::TypeId::of::<T>()).expect("registered");
    let mut de = ron::Deserializer::from_str(&first).expect("reader");
    let back = TypedReflectDeserializer::new(registration, registry)
        .deserialize(&mut de)
        .expect("read");
    let second = ron::to_string(&TypedReflectSerializer::new(back.as_ref(), registry)).expect("write");
    (first, second)
}

// 18 (runtime state). The saved components and resource survive a RON resave unchanged.
#[test]
fn t18_runtime_state_resave_is_byte_identical() {
    let (mut app, owner) = ability_setup();
    let mut poison = timed(1.0);
    poison.status_tags = vec![t("Status.Poisoned")];
    poison.pulse = Some(Pulse {
        every: 0.25,
        on_apply: false,
    });
    poison.live_mods = vec![modifier("health", ModOp::Percent, -0.1)];
    add_effect(&mut app, "poison", poison);
    let src = app.world_mut().spawn_empty().id();
    apply(&mut app, ApplyEffect::new("poison", owner).from_source(src));
    let g = grant_ability(app.world_mut(), owner, "bolt", GrantOptions::default()).expect("grant");
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start");
    steps(&mut app, 3);
    let registry = app.world().resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let world = app.world();
    let (a, b) = reflect_resave(&registry, world.get::<Attributes>(owner).expect("attrs"));
    assert_eq!(a, b);
    let (a, b) = reflect_resave(&registry, world.get::<AppliedEffects>(owner).expect("effects"));
    assert_eq!(a, b);
    assert!(!a.contains("source"), "the source entity is not saved: {a}");
    let (a, b) = reflect_resave(&registry, world.get::<AbilityBook>(owner).expect("book"));
    assert_eq!(a, b);
    let (a, b) = reflect_resave(&registry, world.get::<TagCounts>(owner).expect("counts"));
    assert_eq!(a, b);
    assert!(a.contains("Status.Poisoned"), "{a}");
    let (a, b) = reflect_resave(&registry, world.resource::<AbilityCounters>());
    assert_eq!(a, b);
}

// 10 (named tables). A `Curve` with a table name reads the boxed `LevelCurves` resource.
#[test]
fn named_curve_tables_go_through_level_curves() {
    let mut app = new_app();
    let mut tables = InlineCurveTables::default();
    tables.insert(
        "damage",
        vec![InlineCurve {
            name: "fire".into(),
            points: vec![(1.0, 10.0), (3.0, 30.0)],
        }],
    );
    app.world_mut().insert_resource(LevelCurvesRes(Box::new(tables)));
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1000.0)]);
    let mut hit = instant();
    hit.base_edits = vec![Modifier {
        amount: Magnitude::Curve(LevelCurveRef {
            table: "damage".into(),
            name: "fire".into(),
        }),
        ..modifier("x", ModOp::AddPre, 0.0)
    }];
    add_effect(&mut app, "hit", hit.clone());
    apply(&mut app, ApplyEffect::new("hit", e).at_level(2));
    assert_eq!(base(&app, e, "x"), 20.0);
    hit.base_edits[0].amount = Magnitude::Curve(LevelCurveRef {
        table: "other".into(),
        name: "fire".into(),
    });
    add_effect(&mut app, "miss", hit);
    apply(&mut app, ApplyEffect::new("miss", e).at_level(2));
    assert_eq!(base(&app, e, "x"), 20.0);
    assert_eq!(
        app.world()
            .resource::<AbilityStats>()
            .warning_count("curve_missing"),
        1
    );
}

// § 2.3: `when` counts only while the target matches; `per_stack` multiplies by the stack count;
// the per-entity limit refuses with `limit_reached`.
#[test]
fn conditional_and_per_stack_modifiers_and_the_entity_limit() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 10.0, 0.0, 1000.0)]);
    let mut wet = forever();
    wet.live_mods = vec![Modifier {
        when: Some(q(r#"Has("State.Wet")"#)),
        ..modifier("x", ModOp::AddPre, 5.0)
    }];
    add_effect(&mut app, "wet", wet);
    let mut rage = forever();
    rage.live_mods = vec![Modifier {
        per_stack: true,
        ..modifier("x", ModOp::Scale, 2.0)
    }];
    rage.stack = Some(StackRule {
        key: StackKey::ByTarget,
        cap: 3,
        on_add: OnAdd::default(),
        when_full: WhenFull::Reject,
        on_lapse: OnLapse::EndAll,
        cue_each_stack: false,
    });
    add_effect(&mut app, "rage", rage);
    apply(&mut app, ApplyEffect::new("wet", e));
    step(&mut app);
    assert_eq!(cur(&app, e, "x"), 10.0, "the condition does not match yet");
    app.world_mut()
        .entity_mut(e)
        .insert(TagContainer::from_names(["State.Wet"]).expect("names"));
    step(&mut app);
    assert_eq!(cur(&app, e, "x"), 15.0);
    for _ in 0..3 {
        apply(&mut app, ApplyEffect::new("rage", e));
    }
    step(&mut app);
    assert_eq!(cur(&app, e, "x"), 120.0, "(10 + 5) * 2^3");

    let mut many = forever();
    many.status_tags = vec![t("Status.Many")];
    add_effect(&mut app, "many", many);
    let mut last = Outcome::Applied;
    for _ in 0..200 {
        last = apply(&mut app, ApplyEffect::new("many", e)).outcome;
    }
    assert_eq!(last, Outcome::LimitReached);
    assert_eq!(live(&app, e).len(), MAX_EFFECTS_PER_ENTITY);
}

// 10 (spec 51 adapter, ADR-1649): a `Curve` amount whose table is a loaded `.curves.ron` path reads
// spec 51's `CurveTables` (its interpolation and extrapolation), before the boxed resource.
#[test]
fn named_curve_tables_read_spec_51_curve_tables() {
    use regia_framework::curve_table::{CurveTable, CurveTables};
    let mut app = new_app();
    let mut tables = CurveTables::default();
    tables.insert(
        "tables/damage.curves.ron",
        CurveTable::from_ron_str(
            r#"(format_version: 1, interp: Linear, curves: { "fire": (post: Linear, keys: [(1.0, 10.0), (3.0, 30.0)]) })"#,
        )
        .expect("curve table"),
    );
    app.world_mut().insert_resource(tables);
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1000.0)]);
    let mut hit = instant();
    hit.base_edits = vec![Modifier {
        amount: Magnitude::Curve(LevelCurveRef {
            table: "tables/damage.curves.ron".into(),
            name: "fire".into(),
        }),
        ..modifier("x", ModOp::AddPre, 0.0)
    }];
    add_effect(&mut app, "hit", hit);
    apply(&mut app, ApplyEffect::new("hit", e).at_level(2));
    assert_eq!(base(&app, e, "x"), 20.0);
    // Level 5 is past the last key: the curve's linear post extrapolation gives 50.
    let mut hit5 = instant();
    hit5.base_edits = vec![Modifier {
        amount: Magnitude::Curve(LevelCurveRef {
            table: "tables/damage.curves.ron".into(),
            name: "fire".into(),
        }),
        ..modifier("x", ModOp::AddPre, 0.0)
    }];
    add_effect(&mut app, "hit5", hit5);
    apply(&mut app, ApplyEffect::new("hit5", e).at_level(5));
    assert_eq!(base(&app, e, "x"), 70.0);
}

// Audit-gameplay H5 (ADR-1809): a cost that does not take hold leaves the run uncharged.
#[test]
fn a_cost_that_does_not_take_hold_does_not_charge() {
    // Explicit: the charge is refused, nothing is paid, no cooldown, and a later charge retries.
    let (mut app, owner) = ability_setup();
    let mut never = instant();
    never.base_edits = vec![modifier("mana", ModOp::AddPre, -10.0)];
    never.chance = 0.0;
    add_effect(&mut app, "never", never);
    let mut channel = AbilityAsset::new("channel");
    channel.cost = Some("never".into());
    channel.cooldown = Some("cd".into());
    channel.charge = ChargeMode::Explicit;
    add_ability(&mut app, "channel", channel);
    let mut log: Log<AbilityLifecycle> = Log::new();
    let g = grant_ability(app.world_mut(), owner, "channel", GrantOptions::default()).expect("grant");
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start");
    let refusal = charge(app.world_mut(), owner, g).expect_err("the cost never takes hold");
    assert_eq!(refusal.cause, RefusalCause::CannotAfford);
    assert_eq!(base(&app, owner, "mana"), 30.0);
    assert!(
        !effective_has(app.world(), owner, t("Cooldown.Bolt")),
        "no cooldown without a paid cost"
    );
    let run = app
        .world()
        .get::<AbilityBook>(owner)
        .and_then(|b| b.get(g))
        .and_then(|x| x.run.clone())
        .expect("still running");
    assert!(!run.charged);
    // The cost can take hold now: the retry pays once.
    app.world_mut()
        .resource_mut::<AbilityAssets>()
        .insert_effect("never", {
            let mut cost = instant();
            cost.base_edits = vec![modifier("mana", ModOp::AddPre, -10.0)];
            cost
        });
    charge(app.world_mut(), owner, g).expect("charge");
    assert_eq!(base(&app, owner, "mana"), 20.0);
    assert!(effective_has(app.world(), owner, t("Cooldown.Bolt")));
    log.read(&app);
    assert_eq!(
        lifecycle_codes(&log.seen),
        ["started", "refused:cannot_afford", "charged"]
    );

    // OnStart: the start is refused and the unpaid run ends; the grant is ready again.
    let (mut app, owner) = ability_setup();
    let mut never = instant();
    never.base_edits = vec![modifier("mana", ModOp::AddPre, -10.0)];
    never.chance = 0.0;
    add_effect(&mut app, "never", never);
    let mut bolt = AbilityAsset::new("free_bolt");
    bolt.cost = Some("never".into());
    bolt.cooldown = Some("cd".into());
    add_ability(&mut app, "free_bolt", bolt);
    let g = grant_ability(app.world_mut(), owner, "free_bolt", GrantOptions::default()).expect("grant");
    let refusal = activate(app.world_mut(), owner, g, StartContext::default()).expect_err("unpaid");
    assert_eq!(refusal.cause, RefusalCause::CannotAfford);
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Ending));
    step(&mut app);
    assert_eq!(
        ability_state(app.world(), owner, g),
        Some(AbilityState::Ready),
        "no cooldown: the next start is not `on_cooldown`"
    );
    assert_eq!(base(&app, owner, "mana"), 30.0);
}

// Audit-gameplay H4 (ADR-1809): a loaded save keeps the reflected marker but not the
// bookkeeping; the bridge recreates it, so a loaded bridged actor still mirrors health and dies.
#[test]
fn a_loaded_bridged_actor_without_bridge_state_still_dies() {
    let mut app = new_app();
    app.add_plugins((DamagePlugin, HealthBridge));
    let attrs = Attributes::from_list([
        Attribute::new("health_max", 100.0, 1.0, 1000.0),
        Attribute::new("health", 100.0, 0.0, 1000.0).with_max_from("health_max"),
    ])
    .expect("attrs");
    // What a load restores: the marker and `Health`, no `HealthBridgeState`.
    let actor = app
        .world_mut()
        .spawn((attrs, Health::new(100.0), HealthFromAttributes))
        .id();
    step(&mut app);
    assert!(
        app.world().get::<HealthBridgeState>(actor).is_some(),
        "state recreated"
    );
    let mut deaths: Log<DiedEvent> = Log::new();
    let killer = app.world_mut().spawn_empty().id();
    app.world_mut().write_message(DamageEvent {
        target: actor,
        amount: 100.0,
        source: Some(killer),
        position: None,
    });
    step(&mut app);
    deaths.read(&app);
    assert_eq!(cur(&app, actor, "health"), 0.0);
    assert_eq!(app.world().get::<Health>(actor).map(|h| h.current), Some(0.0));
    assert_eq!(deaths.seen.len(), 1);
    assert_eq!(deaths.seen[0].killer, Some(killer));
}

// ADR-1809: a failed earlier effect grant must not shift a later grant onto its end policy.
#[test]
fn effect_grants_keep_their_own_end_policy_when_an_earlier_grant_failed() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 10.0)]);
    add_ability(&mut app, "real", AbilityAsset::new("real"));
    let mut aura = forever();
    aura.grants = vec![
        EffectGrant {
            ability: "missing".into(),
            level: 1,
            on_effect_end: OnEffectEnd::Cancel,
        },
        EffectGrant {
            ability: "real".into(),
            level: 1,
            on_effect_end: OnEffectEnd::Keep,
        },
    ];
    add_effect(&mut app, "aura", aura);
    let out = apply(&mut app, ApplyEffect::new("aura", e));
    assert_eq!(app.world().get::<AbilityBook>(e).map(|b| b.grants.len()), Some(1));
    remove_effect(app.world_mut(), e, out.id.expect("entry")).expect("remove");
    step(&mut app);
    assert_eq!(
        app.world().get::<AbilityBook>(e).map(|b| b.grants.len()),
        Some(1),
        "`real` is Keep: it outlives the effect"
    );
}

// ADR-1809: a press that only queues a running `QueueOne` grant does not consume `one_per_press`.
#[test]
fn a_queue_only_press_passes_to_the_next_bound_grant() {
    let (mut app, owner) = ability_setup();
    let mut a = AbilityAsset::new("a");
    a.input_code = Some(1);
    a.when_running = WhenRunning::QueueOne;
    a.max_run_ticks = Some(30);
    add_ability(&mut app, "a", a);
    let mut b = AbilityAsset::new("b");
    b.input_code = Some(1);
    add_ability(&mut app, "b", b);
    let world = app.world_mut();
    let ga = grant_ability(world, owner, "a", GrantOptions::default()).expect("a");
    let gb = grant_ability(world, owner, "b", GrantOptions::default()).expect("b");
    world.get_mut::<AbilityBook>(owner).expect("book").one_per_press = true;
    activate(world, owner, ga, StartContext::default()).expect("a runs");
    world.write_message(AbilityInput {
        entity: owner,
        code: 1,
        down: true,
    });
    step(&mut app);
    assert_eq!(ability_state(app.world(), owner, ga), Some(AbilityState::Running));
    assert!(
        app.world()
            .get::<AbilityBook>(owner)
            .and_then(|b| b.get(ga))
            .is_some_and(|x| x.queued),
        "the press queued `a`"
    );
    assert_eq!(
        ability_state(app.world(), owner, gb),
        Some(AbilityState::Running),
        "the press still started `b`"
    );
}

// ADR-1809: an attribute added in the same step as a change does not swallow the change.
#[test]
fn an_attribute_added_in_the_same_step_keeps_the_other_changes() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("health", 100.0, 0.0, 100.0)]);
    step(&mut app);
    let mut changed: Log<AttributeChanged> = Log::new();
    changed.read(&app);
    {
        let mut attrs = app.world_mut().get_mut::<Attributes>(e).expect("attrs");
        attrs.set_base(AttrId::of("health"), 40.0).expect("set");
        attrs
            .push(Attribute::new("mana", 10.0, 0.0, 100.0))
            .expect("push");
    }
    step(&mut app);
    let seen: Vec<(AttrId, AttrLane, f32, f32)> = changed
        .read(&app)
        .iter()
        .map(|c| (c.attribute, c.lane, c.old, c.new))
        .collect();
    assert!(
        seen.contains(&(AttrId::of("health"), AttrLane::Base, 100.0, 40.0)),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|(id, ..)| *id == AttrId::of("mana")),
        "a new attribute only takes its baseline: {seen:?}"
    );
}

// ADR-1809: the world effect cap counts this step's applies and ends, not the last recompute's.
#[test]
fn the_world_effect_cap_is_current_within_a_step() {
    let mut app = new_app();
    add_effect(&mut app, "mark", forever());
    let per = MAX_EFFECTS_PER_ENTITY;
    let total = usize::try_from(MAX_EFFECTS_TOTAL).expect("fits");
    let mut applied = Vec::new();
    while applied.len() < total {
        let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1.0)]);
        for _ in 0..per.min(total - applied.len()) {
            let out = apply(&mut app, ApplyEffect::new("mark", e));
            assert_eq!(out.outcome, Outcome::Applied);
            applied.push((e, out.id.expect("id")));
        }
    }
    // No recompute ran since: the next apply already sees the full world.
    let extra = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1.0)]);
    assert_eq!(
        apply(&mut app, ApplyEffect::new("mark", extra)).outcome,
        Outcome::LimitReached
    );
    step(&mut app);
    // Ending them all in a step frees the cap in that same step.
    for (e, id) in applied {
        remove_effect(app.world_mut(), e, id).expect("remove");
    }
    assert_eq!(
        apply(&mut app, ApplyEffect::new("mark", extra)).outcome,
        Outcome::Applied
    );
}

// ADR-1809: a loaded `TagCounts` (its report snapshot is not saved) does not re-report its tags.
#[test]
fn a_loaded_tag_count_does_not_re_report_its_tags() {
    use regia_ecs::reflect::FromReflect;
    let (mut app, owner) = ability_setup();
    let mut poison = forever();
    poison.status_tags = vec![t("Status.Poisoned")];
    add_effect(&mut app, "poison", poison);
    let mut changes: Log<EffectiveTagsChanged> = Log::new();
    apply(&mut app, ApplyEffect::new("poison", owner));
    step(&mut app);
    assert_eq!(
        changes.read(&app).len(),
        1,
        "a fresh entity reports its first tags"
    );
    // What a load restores: the counts, without the report snapshot.
    let saved = app.world().get::<TagCounts>(owner).expect("counts").clone();
    let loaded = TagCounts::from_reflect(saved.as_partial_reflect()).expect("from reflect");
    app.world_mut().entity_mut(owner).insert(loaded);
    step(&mut app);
    assert!(changes.read(&app).is_empty(), "no phantom `added` after a load");
}

// § 10: "a skipped entity holds exactly the value a full recompute would give".

#[test]
fn a_skipped_entity_holds_what_a_full_recompute_gives() {
    let mut app = new_app();
    let mut buff = timed(0.1);
    buff.live_mods = vec![modifier("x", ModOp::Scale, 1.5)];
    add_effect(&mut app, "buff", buff);
    let attrs = Attributes::from_list([
        Attribute::new("cap", 80.0, 1.0, 1000.0),
        Attribute::new("x", 12.3, 0.0, 1000.0).with_max_from("cap"),
        Attribute::new("y", 0.0, 0.0, 1000.0).with_derived(&[("x", 2.5)], 1.0),
    ])
    .expect("attributes");
    let e = app.world_mut().spawn(attrs).id();
    apply(&mut app, ApplyEffect::new("buff", e));
    let bits = |app: &App| -> Vec<(u32, u32)> {
        app.world()
            .get::<Attributes>(e)
            .expect("attributes")
            .list()
            .iter()
            .map(|a| (a.base.to_bits(), a.current.to_bits()))
            .collect()
    };
    // The effect runs out, and the entity turns idle.
    steps(&mut app, 12);
    assert_eq!(cur(&app, e, "x"), 12.3, "the buff ended");
    let skipped = app.world().resource::<AbilityStats>().entities_skipped;
    assert_eq!(skipped, 1, "the dirty flag is skipping it");
    let idle = bits(&app);
    // A forced full recompute lands on the same bits.
    app.world_mut()
        .get_mut::<Attributes>(e)
        .expect("attributes")
        .mark_dirty();
    step(&mut app);
    assert_eq!(
        app.world().resource::<AbilityStats>().entities_skipped,
        0,
        "recomputed this time"
    );
    assert_eq!(bits(&app), idle);
}

// § 10 budgets (ADR-2012).

#[test]
fn a_book_refuses_a_grant_past_its_budget() {
    let mut app = new_app();
    add_ability(&mut app, "a", AbilityAsset::new("a"));
    let owner = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1.0)]);
    for _ in 0..MAX_GRANTS_PER_BOOK {
        grant_ability(app.world_mut(), owner, "a", GrantOptions::default()).expect("grant");
    }
    let error = grant_ability(app.world_mut(), owner, "a", GrantOptions::default()).expect_err("full");
    assert_eq!(error.reason(), "grant_limit");
    let book = app.world().get::<AbilityBook>(owner).expect("book");
    assert_eq!(book.grants.len(), MAX_GRANTS_PER_BOOK);
    // Room again after a revoke, and ids are not reused.
    let last = book.grants[MAX_GRANTS_PER_BOOK - 1].id;
    revoke_ability(app.world_mut(), owner, last, false).expect("revoke");
    let id = grant_ability(app.world_mut(), owner, "a", GrantOptions::default()).expect("grant");
    assert!(id.0 > last.0);
}

#[test]
fn an_effect_past_the_soft_budget_applies_and_warns() {
    let mut app = new_app();
    add_effect(&mut app, "e", forever());
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1.0)]);
    for _ in 0..SOFT_EFFECTS_PER_ENTITY {
        assert_eq!(
            apply(&mut app, ApplyEffect::new("e", e)).outcome,
            Outcome::Applied
        );
    }
    let warnings = |app: &App| {
        app.world()
            .resource::<AbilityStats>()
            .warning_count("effects_over_soft_limit")
    };
    assert_eq!(warnings(&app), 0, "at the budget is inside it");
    assert_eq!(
        apply(&mut app, ApplyEffect::new("e", e)).outcome,
        Outcome::Applied
    );
    assert_eq!(warnings(&app), 1);
    assert_eq!(live(&app, e).len(), SOFT_EFFECTS_PER_ENTITY + 1);
}

#[test]
fn a_burst_past_the_message_budget_drops_its_tail_deterministically() {
    let _serial = MESSAGE_BUDGET_WARN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = new_app();
    let mut bump = instant();
    bump.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
    add_effect(&mut app, "bump", bump);
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1_000_000.0)]);
    for _ in 0..MAX_MESSAGES_PER_STEP + 3 {
        app.world_mut().write_message(ApplyEffect::new("bump", e));
    }
    step(&mut app);
    assert_eq!(base(&app, e, "x"), MAX_MESSAGES_PER_STEP as f32);
    let warnings = |app: &App| {
        app.world()
            .resource::<AbilityStats>()
            .warning_count("message_budget")
    };
    assert_eq!(
        warnings(&app),
        1,
        "one warning for the burst, not one per message"
    );
    steps(&mut app, 3);
    assert_eq!(
        base(&app, e, "x"),
        MAX_MESSAGES_PER_STEP as f32,
        "the tail does not come back"
    );
    assert_eq!(warnings(&app), 1);
}

#[test]
fn a_burst_at_the_message_budget_is_taken_whole() {
    let mut app = new_app();
    let mut bump = instant();
    bump.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
    add_effect(&mut app, "bump", bump);
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1_000_000.0)]);
    for _ in 0..MAX_MESSAGES_PER_STEP {
        app.world_mut().write_message(ApplyEffect::new("bump", e));
    }
    step(&mut app);
    assert_eq!(base(&app, e, "x"), MAX_MESSAGES_PER_STEP as f32);
    assert_eq!(
        app.world()
            .resource::<AbilityStats>()
            .warning_count("message_budget"),
        0
    );
}

/// Budgets over file contents warn: a file that loaded before they existed keeps loading.
#[test]
fn files_over_a_budget_warn_and_still_load() {
    let mut asset = forever();
    asset.live_mods = vec![modifier("x", ModOp::AddPre, 1.0); MAX_MODIFIERS_PER_EFFECT];
    let codes = |asset: &EffectAsset| -> Vec<&'static str> {
        validate_effect(asset, None).iter().map(|d| d.code).collect()
    };
    assert!(
        !codes(&asset).contains(&"effect_over_budget"),
        "at the budget is inside it"
    );
    asset.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
    assert_eq!(codes(&asset), ["effect_over_budget"]);
    let diagnostics = validate_effect(&asset, None);
    assert!(!has_errors(&diagnostics));

    let names: Vec<String> = (0..=MAX_ATTRIBUTES_PER_ENTITY)
        .map(|i| format!(r#"(name: "a{i}", base: 0.0, min: 0.0, max: 1.0)"#))
        .collect();
    let text = format!("(format_version: 1, attributes: [{}])", names.join(", "));
    let mut assets = AbilityAssets::default();
    let diagnostics = assets.load_text("big.attrs.ron", &text, None, None);
    assert!(!has_errors(&diagnostics), "{diagnostics:?}");
    assert!(diagnostics.iter().any(|d| d.code == "attributes_over_budget"));
    assert!(assets.attributes("big.attrs.ron").is_some(), "it loaded");
}

// § 7 cross-platform hash (ADR-2012).

fn hash_world(health: f32, stacks: bool) -> (App, Entity) {
    let mut app = new_app();
    let mut poison = forever();
    poison.live_mods = vec![modifier("x", ModOp::AddPre, 1.0)];
    add_effect(&mut app, "e", poison);
    let attrs = Attributes::from_list([
        Attribute::new("health", health, -100.0, 100.0),
        Attribute::new("x", 0.0, 0.0, 100.0),
    ])
    .expect("attributes");
    let e = app.world_mut().spawn((attrs, EntityGuid(Guid(7)))).id();
    apply(&mut app, ApplyEffect::new("e", e));
    if stacks {
        apply(&mut app, ApplyEffect::new("e", e));
    }
    steps(&mut app, 3);
    (app, e)
}

#[test]
fn the_state_hash_folds_attributes_on_the_quantum() {
    let hash = |health: f32, stacks: bool| {
        let (mut app, _) = hash_world(health, stacks);
        abilities_state_hash(app.world_mut())
    };
    let base_hash = hash(0.12341, false);
    assert_eq!(base_hash, hash(0.12341, false), "repeatable");
    // Well inside § 7's cross-platform tolerance, and a different `snapshot_hash`.
    let near = 0.12341_f32 * (1.0 + 2e-5);
    assert_ne!(near.to_bits(), 0.12341_f32.to_bits());
    assert_eq!(base_hash, hash(near, false), "inside one cell");
    assert_ne!(base_hash, hash(0.1236, false), "the next cell over");
    assert_ne!(base_hash, hash(0.12341, true), "counts fold exactly");
}

#[test]
fn the_state_hash_changes_when_a_clock_or_a_tick_does() {
    let (mut app, _) = hash_world(1.0, false);
    let before = abilities_state_hash(app.world_mut());
    step(&mut app);
    assert_ne!(before, abilities_state_hash(app.world_mut()));
}

/// Spawn order is not in the hash: entities fold in `EntityGuid` order.
#[test]
fn the_state_hash_does_not_see_spawn_order() {
    let run = |reverse: bool| {
        let mut app = new_app();
        let mut order: Vec<u128> = (1..=20).collect();
        if reverse {
            order.reverse();
        }
        for n in order {
            let attrs =
                Attributes::from_list([Attribute::new("x", n as f32, 0.0, 100.0)]).expect("attributes");
            app.world_mut().spawn((attrs, EntityGuid(Guid(n))));
        }
        steps(&mut app, 2);
        abilities_state_hash(app.world_mut())
    };
    assert_eq!(run(false), run(true));
}

/// The expected value is computed outside the engine (FNV-1a over little-endian words, by hand in
/// Python): this is the number another platform has to reach.
#[test]
fn the_state_hash_of_a_fixed_state_is_a_known_number() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(AbilityPlugin::default());
    let mut attrs = Attributes::from_list([Attribute::new("health", 12.5, -100.0, 100.0)]).expect("attrs");
    attrs.set_current_for_test(AttrId::of("health"), -0.25);
    app.world_mut().spawn((
        attrs,
        EntityGuid(Guid((0x0123_4567_89ab_cdef_u128 << 64) | 0xfedc_ba98_7654_3210)),
    ));
    {
        let mut counters = app.world_mut().resource_mut::<AbilityCounters>();
        counters.tick = 7;
        counters.last_effect_id = 3;
    }
    assert_eq!(abilities_state_hash(app.world_mut()), 0xc575_c85b_47f3_5d17);
}

// § 10 dirty flag for entities with fixed live effects (ADR-2012): skipped while nothing they read
// changed, recomputed after any change, and always equal to a recompute from scratch.

/// Each attribute's `(base, current)` as bits.
type AttributeBits = Vec<(u32, u32)>;

/// Every attribute's `(base, current)` bits as held, and as a recompute from scratch gives them.
fn held_and_fresh(app: &App, e: Entity) -> (AttributeBits, AttributeBits) {
    let attrs = app.world().get::<Attributes>(e).expect("attributes");
    let held = attrs
        .list()
        .iter()
        .map(|a| (a.base.to_bits(), a.current.to_bits()))
        .collect();
    let fresh = attrs
        .list()
        .iter()
        .map(|a| {
            let b = explain_attribute(app.world(), e, a.id).expect("breakdown");
            (b.base.to_bits(), b.current.to_bits())
        })
        .collect();
    (held, fresh)
}

fn skipped(app: &App) -> u64 {
    app.world().resource::<AbilityStats>().entities_skipped
}

fn fixed_attrs() -> Vec<Attribute> {
    vec![
        Attribute::new("cap", 40.0, 1.0, 1000.0),
        Attribute::new("x", 10.0, 0.0, 1000.0).with_max_from("cap"),
        Attribute::new("y", 0.0, 0.0, 1000.0).with_derived(&[("x", 2.0)], 1.0),
    ]
}

#[test]
fn an_entity_with_fixed_effects_is_skipped_until_something_it_reads_changes() {
    let mut app = new_app();
    let mut add = forever();
    add.live_mods = vec![modifier("x", ModOp::AddPre, 2.0)];
    add_effect(&mut app, "add", add);
    let e = spawn_attrs(&mut app, fixed_attrs());
    let id = apply(&mut app, ApplyEffect::new("add", e)).id.expect("id");
    let settle = |app: &mut App| {
        // The step after a change recomputes; the notify pass's report writes the attributes, so
        // the next one does too; from then on the entity is skipped.
        steps(app, 3);
        assert_eq!(skipped(app), 1, "settled");
        assert_eq!(app.world().resource::<AbilityStats>().modifier_evaluations, 0);
        let (held, fresh) = held_and_fresh(app, e);
        assert_eq!(held, fresh);
    };
    settle(&mut app);
    assert_eq!(cur(&app, e, "x"), 12.0);
    assert_eq!(cur(&app, e, "y"), 25.0);

    // A base edit.
    set_base(app.world_mut(), e, AttrId::of("x"), 20.0).expect("set");
    step(&mut app);
    assert_eq!(skipped(&app), 0);
    assert_eq!(cur(&app, e, "x"), 22.0);
    settle(&mut app);

    // A suspension.
    set_suspended(app.world_mut(), e, id, true).expect("suspend");
    step(&mut app);
    assert_eq!(cur(&app, e, "x"), 20.0);
    settle(&mut app);
    set_suspended(app.world_mut(), e, id, false).expect("resume");
    step(&mut app);
    assert_eq!(cur(&app, e, "x"), 22.0);
    settle(&mut app);

    // The asset replaced (a reload): the captured amount stays, the op is the new one.
    let mut scale = forever();
    scale.live_mods = vec![modifier("x", ModOp::Scale, 2.0)];
    add_effect(&mut app, "add", scale);
    step(&mut app);
    assert_eq!(skipped(&app), 0);
    assert_eq!(cur(&app, e, "x"), 40.0);
    settle(&mut app);

    // A host's write to the effect list.
    app.world_mut()
        .get_mut::<AppliedEffects>(e)
        .expect("effects")
        .list
        .clear();
    step(&mut app);
    assert_eq!(cur(&app, e, "x"), 20.0);

    // Another effect, then a removal.
    let id = apply(&mut app, ApplyEffect::new("add", e)).id.expect("id");
    settle(&mut app);
    assert_eq!(cur(&app, e, "x"), 40.0);
    remove_effect(app.world_mut(), e, id).expect("remove");
    step(&mut app);
    assert_eq!(cur(&app, e, "x"), 20.0);

    // Attributes copied in from an entity that holds other values (clean, but not this entity's).
    let id = apply(&mut app, ApplyEffect::new("add", e)).id.expect("id");
    settle(&mut app);
    let other = spawn_attrs(&mut app, fixed_attrs());
    steps(&mut app, 2);
    let copy = app.world().get::<Attributes>(other).expect("attributes").clone();
    app.world_mut().entity_mut(e).insert(copy);
    step(&mut app);
    assert_eq!(cur(&app, e, "x"), 20.0, "10 scaled by 2");
    let (held, fresh) = held_and_fresh(&app, e);
    assert_eq!(held, fresh);
    remove_effect(app.world_mut(), e, id).expect("remove");
}

#[test]
fn conditional_and_live_effects_are_recomputed_every_step_and_clocks_alone_force_none() {
    let mut app = new_app();
    // 15 steps.
    let mut buff = timed(0.25);
    buff.live_mods = vec![modifier("x", ModOp::AddPre, 1.0)];
    add_effect(&mut app, "timed", buff);
    let mut wet = forever();
    wet.live_mods = vec![Modifier {
        when: Some(q(r#"Has("State.Wet")"#)),
        ..modifier("x", ModOp::AddPre, 1.0)
    }];
    add_effect(&mut app, "wet", wet);
    let mut stat = forever();
    stat.live_mods = vec![Modifier {
        amount: Magnitude::Stat {
            side: StatSide::Target,
            attribute: "cap".into(),
            read: StatRead::Live,
            weight: 0.0,
            offset: 1.0,
        },
        ..modifier("x", ModOp::AddPre, 0.0)
    }];
    add_effect(&mut app, "stat", stat);
    let entities: Vec<Entity> = ["timed", "wet", "stat"]
        .iter()
        .map(|path| {
            let e = spawn_attrs(&mut app, fixed_attrs());
            apply(&mut app, ApplyEffect::new(path, e));
            e
        })
        .collect();
    let mut timed_skips = 0;
    for _ in 0..20 {
        step(&mut app);
        // The conditional and the live one never are; the timed one is, once settled, while its
        // clock alone moves.
        assert!(skipped(&app) <= 1);
        timed_skips += skipped(&app);
        for &e in &entities {
            let (held, fresh) = held_and_fresh(&app, e);
            assert_eq!(held, fresh);
        }
    }
    assert!(timed_skips >= 10, "{timed_skips}");
    assert!(live(&app, entities[0]).is_empty(), "the buff ran out");
    assert_eq!(cur(&app, entities[0], "x"), 10.0);
    // The condition turns on with no write to the entity's effects or attributes.
    app.world_mut()
        .entity_mut(entities[1])
        .insert(TagContainer::from_names(["State.Wet"]).expect("names"));
    step(&mut app);
    assert_eq!(cur(&app, entities[1], "x"), 11.0);
}

/// A fixed walk over the changes a fixed effect's entity can see, most steps quiet: after every
/// step each attribute equals a recompute from scratch, and the skip did happen.
#[test]
fn a_fixed_walk_of_changes_always_matches_a_full_recompute() {
    let mut app = new_app();
    let library: [(&str, ModOp, f32); 4] = [
        ("add", ModOp::AddPre, 2.0),
        ("pct", ModOp::Percent, 0.25),
        ("scale", ModOp::Scale, 1.5),
        ("post", ModOp::AddPost, -3.0),
    ];
    for (path, op, amount) in library {
        let mut asset = forever();
        asset.live_mods = vec![modifier("x", op, amount), modifier("cap", ModOp::AddPost, 1.0)];
        add_effect(&mut app, path, asset);
    }
    let mut short = timed(0.05);
    short.live_mods = vec![modifier("x", ModOp::AddPost, 5.0)];
    add_effect(&mut app, "short", short);
    let mut long = timed(5.0);
    long.live_mods = vec![modifier("x", ModOp::Percent, 0.5)];
    add_effect(&mut app, "long", long);
    let entities: Vec<Entity> = (0..6).map(|_| spawn_attrs(&mut app, fixed_attrs())).collect();
    let mut rng = Rng::new(2012);
    let mut skips = 0;
    for _ in 0..2_000 {
        let e = entities[rng.range(0..entities.len())];
        let ids: Vec<EffectId> = live(&app, e).iter().map(|x| x.id).collect();
        match rng.range(0..40u32) {
            0..=3 => {
                let path = ["add", "pct", "scale", "post", "short", "long"][rng.range(0..6usize)];
                apply(&mut app, ApplyEffect::new(path, e));
            }
            4 | 5 if !ids.is_empty() => {
                let id = ids[rng.range(0..ids.len())];
                remove_effect(app.world_mut(), e, id).expect("remove");
            }
            6 if !ids.is_empty() => {
                let id = ids[rng.range(0..ids.len())];
                let on = rng.chance(0.5);
                set_suspended(app.world_mut(), e, id, on).expect("suspend");
            }
            7 => {
                let value = rng.range(0.0..30.0f32);
                let name = ["cap", "x"][rng.range(0..2usize)];
                set_base(app.world_mut(), e, AttrId::of(name), value).expect("set");
            }
            8 => {
                let (path, _, _) = library[rng.range(0..library.len())];
                let op = library[rng.range(0..library.len())].1;
                let mut asset = forever();
                asset.live_mods = vec![modifier("x", op, 1.0), modifier("cap", ModOp::AddPost, 1.0)];
                add_effect(&mut app, path, asset);
            }
            9 if !ids.is_empty() => {
                let stacks = rng.range(1..4u16);
                if let Some(mut list) = app.world_mut().get_mut::<AppliedEffects>(e)
                    && let Some(entry) = list.list.first_mut()
                {
                    entry.stacks = stacks;
                }
            }
            _ => {}
        }
        step(&mut app);
        skips += skipped(&app);
        for &e in &entities {
            let (held, fresh) = held_and_fresh(&app, e);
            assert_eq!(held, fresh, "{e:?}");
        }
    }
    let with_effects = entities.iter().filter(|&&e| !live(&app, e).is_empty()).count();
    assert!(with_effects > 0, "the walk left effects in place");
    assert!(skips > 2_000, "quiet steps were skipped ({skips})");
}

/// Idle fixed steps must not clone an ability handle again after the grant was made.
#[test]
fn framework2_perf_ability_clone() {
    let mut app = new_app();
    let mut plain = AbilityAsset::new("plain");
    plain.triggers = vec![Trigger::Signal(t("Signal.Unused"))];
    add_ability(&mut app, "plain", plain);
    for _ in 0..4 {
        let owner = app.world_mut().spawn_empty().id();
        grant_ability(app.world_mut(), owner, "plain", GrantOptions::default()).expect("grant");
    }
    step(&mut app);
    let clones = ability_clone_count();
    steps(&mut app, 2);
    assert_eq!(
        ability_clone_count(),
        clones,
        "two idle fixed steps cloned ability handles again (was {clones})"
    );
}

/// A `WhileTagged` grant already refused because its cooldown tag is held does not call
/// `check_activation` again until that tag is gone, and the next step then starts the run.
#[test]
fn framework2_perf_activation_hold() {
    let (mut app, owner) = ability_setup();
    let mut haste = AbilityAsset::new("haste");
    haste.triggers = vec![Trigger::WhileTagged(t("State.Sprinting"))];
    haste.cooldown = Some("cd".into());
    add_ability(&mut app, "haste", haste);
    let applied = apply(&mut app, ApplyEffect::new("cd", owner));
    let effect = applied.id.expect("cooldown effect");
    app.world_mut()
        .entity_mut(owner)
        .insert(TagContainer::from_names(["State.Sprinting"]).expect("tags"));
    let grant = grant_ability(app.world_mut(), owner, "haste", GrantOptions::default()).expect("grant");
    step(&mut app);
    assert_eq!(
        ability_state(app.world(), owner, grant),
        Some(AbilityState::Cooling)
    );
    let checks = activation_check_count();
    steps(&mut app, 4);
    assert_eq!(
        activation_check_count(),
        checks,
        "the held cooldown rechecked activation (was {checks})"
    );
    assert_eq!(
        ability_state(app.world(), owner, grant),
        Some(AbilityState::Cooling)
    );
    remove_effect(app.world_mut(), owner, effect).expect("remove cooldown");
    step(&mut app);
    assert_eq!(
        ability_state(app.world(), owner, grant),
        Some(AbilityState::Running)
    );
    assert!(
        activation_check_count() > checks,
        "dropping the cooldown must recheck"
    );
}

/// corr-framework-a F1: a held `WhileTagged` refusal must not survive an ability asset reload.
/// The grant is refused for a missing owner tag; the reloaded asset drops that requirement, so the
/// next step starts the run (before the fix the cached refusal kept it `Ready` forever).
#[test]
fn corr_fa_while_hold_drops_on_asset_reload() {
    let mut app = new_app();
    let mut haste = AbilityAsset::new("haste");
    haste.triggers = vec![Trigger::WhileTagged(t("State.Sprinting"))];
    haste.owner_needs = Some(q(r#"Has("State.Rested")"#));
    add_ability(&mut app, "haste", haste.clone());
    let owner = app
        .world_mut()
        .spawn(TagContainer::from_names(["State.Sprinting"]).expect("tags"))
        .id();
    let grant = grant_ability(app.world_mut(), owner, "haste", GrantOptions::default()).expect("grant");
    steps(&mut app, 2);
    assert_eq!(
        ability_state(app.world(), owner, grant),
        Some(AbilityState::Ready)
    );
    haste.owner_needs = None;
    add_ability(&mut app, "haste", haste);
    step(&mut app);
    assert_eq!(
        ability_state(app.world(), owner, grant),
        Some(AbilityState::Running),
        "the reloaded asset no longer needs State.Rested"
    );
}

/// corr-low-framework (corr-framework-a L5): a held refusal also drops when the whole
/// `AbilityAssets` resource is replaced by a fresh one that reached the same number of inserts.
/// The generation was a per-resource counter starting at 0, so both read 1 and the hold stayed.
#[test]
fn a_replaced_asset_resource_drops_a_held_refusal() {
    let mut app = new_app();
    let mut haste = AbilityAsset::new("haste");
    haste.triggers = vec![Trigger::WhileTagged(t("State.Sprinting"))];
    haste.owner_needs = Some(q(r#"Has("State.Rested")"#));
    app.world_mut()
        .resource_mut::<AbilityAssets>()
        .insert_ability("haste", haste.clone());
    let owner = app
        .world_mut()
        .spawn(TagContainer::from_names(["State.Sprinting"]).expect("tags"))
        .id();
    let grant = grant_ability(app.world_mut(), owner, "haste", GrantOptions::default()).expect("grant");
    steps(&mut app, 2);
    assert_eq!(
        ability_state(app.world(), owner, grant),
        Some(AbilityState::Ready)
    );
    haste.owner_needs = None;
    let mut fresh = AbilityAssets::default();
    fresh.insert_ability("haste", haste);
    app.world_mut().insert_resource(fresh);
    step(&mut app);
    assert_eq!(
        ability_state(app.world(), owner, grant),
        Some(AbilityState::Running),
        "the replacement asset no longer needs State.Rested"
    );
}

/// corr-framework-a F1: a held `CannotAfford` refusal must not survive turning on the
/// `free_costs` cheat (nothing else changes, so the attribute tick does not move).
#[test]
fn corr_fa_while_hold_drops_on_cheat_toggle() {
    let mut app = new_app();
    let owner = spawn_attrs(&mut app, vec![Attribute::new("mana", 0.0, 0.0, 100.0)]);
    app.world_mut()
        .entity_mut(owner)
        .insert(TagContainer::from_names(["State.Sprinting"]).expect("tags"));
    let mut cost = instant();
    cost.base_edits = vec![modifier("mana", ModOp::AddPre, -10.0)];
    add_effect(&mut app, "cost", cost);
    let mut haste = AbilityAsset::new("haste");
    haste.triggers = vec![Trigger::WhileTagged(t("State.Sprinting"))];
    haste.cost = Some("cost".into());
    add_ability(&mut app, "haste", haste);
    let grant = grant_ability(app.world_mut(), owner, "haste", GrantOptions::default()).expect("grant");
    steps(&mut app, 2);
    assert_eq!(
        ability_state(app.world(), owner, grant),
        Some(AbilityState::Ready)
    );
    app.world_mut()
        .resource_mut::<AbilitySettings>()
        .cheats
        .free_costs = true;
    step(&mut app);
    assert_eq!(
        ability_state(app.world(), owner, grant),
        Some(AbilityState::Running),
        "free_costs skips the cost check"
    );
}

/// corr-framework-a F1: the hold fingerprints the tags at the start of the owner's turn, but the
/// refusal is computed on the tags at check time. Here an earlier grant's cooldown adds
/// `Cooldown.A` mid-turn, the second grant forbids it, and game code strips the cooldown before the
/// next step: the tags are then exactly the turn-start tags again and the second grant must start.
#[test]
fn corr_fa_while_hold_ignores_mid_turn_tag_changes() {
    let mut app = new_app();
    let mut cd = timed(100.0);
    cd.status_tags = vec![t("Cooldown.A")];
    add_effect(&mut app, "cd_a", cd);
    let mut first = AbilityAsset::new("first");
    first.triggers = vec![Trigger::WhileTagged(t("State.Sprinting"))];
    first.cooldown = Some("cd_a".into());
    add_ability(&mut app, "first", first);
    let mut second = AbilityAsset::new("second");
    second.triggers = vec![Trigger::WhileTagged(t("State.Sprinting"))];
    second.owner_forbids = Some(q(r#"Has("Cooldown.A")"#));
    add_ability(&mut app, "second", second);
    let owner = app
        .world_mut()
        .spawn(TagContainer::from_names(["State.Sprinting"]).expect("tags"))
        .id();
    let a = grant_ability(app.world_mut(), owner, "first", GrantOptions::default()).expect("grant a");
    let b = grant_ability(app.world_mut(), owner, "second", GrantOptions::default()).expect("grant b");
    step(&mut app);
    assert_eq!(ability_state(app.world(), owner, a), Some(AbilityState::Running));
    assert_eq!(ability_state(app.world(), owner, b), Some(AbilityState::Ready));
    let cooldown = live(&app, owner)
        .into_iter()
        .find(|x| x.asset == "cd_a")
        .expect("cooldown applied")
        .id;
    remove_effect(app.world_mut(), owner, cooldown).expect("remove cooldown");
    step(&mut app);
    assert_eq!(
        ability_state(app.world(), owner, b),
        Some(AbilityState::Running),
        "Cooldown.A is gone, so nothing forbids the second grant"
    );
}

// ---------------------------------------------------------------------------------------------
// v0.5.1 gameplay fixes (deep dive `sprint-deep-zai-gameplay`, findings 1-6).

/// Captures warn-and-above `tracing` output of `f` on this thread (the pattern of
/// `tests/player_data.rs`).
#[derive(Clone, Default)]
struct WarnSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for WarnSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut bytes) = self.0.lock() {
            bytes.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serializes the tests that drive the `message_budget` warning. Its `tracing::warn!` callsite
/// caches its interest process-wide: when one of these tests registers the callsite on a thread
/// without a subscriber while `a_burst_past_the_budget_reports_the_exact_number_dropped` has its
/// scoped capture subscriber live, the registration can store "never" over the capture's rebuild,
/// and the capture sees no line (14 of 30 parallel runs of this binary on the build server, 2026-11-13; 0
/// alone or with `--test-threads=1`). Run one at a time, the capture subscriber's registration
/// rebuilds the cached interest and the warning always reaches it.
static MESSAGE_BUDGET_WARN: Mutex<()> = Mutex::new(());

fn capture_warn<T>(f: impl FnOnce() -> T) -> (T, String) {
    let sink = WarnSink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer({
            let sink = sink.clone();
            move || sink.clone()
        })
        .finish();
    let out = tracing::subscriber::with_default(subscriber, f);
    let text = sink
        .0
        .lock()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    (out, text)
}

/// Finding 1 (ADR-7573): `AbilitySet::Requests` processes the four request kinds in the fixed
/// order apply -> start -> cancel -> revoke, each kind in message order (the kinds are separate
/// message queues, so a cross-kind write order cannot exist). A start written before the apply
/// that grants its `owner_needs` tag therefore still starts: the apply kind lands first whatever
/// the write order.
#[test]
fn requests_resolve_in_the_documented_kind_order() {
    let trial = |start_first: bool| {
        let mut app = new_app();
        let mut buff = forever();
        buff.status_tags = vec![t("Buff.Ready")];
        add_effect(&mut app, "buff", buff);
        let mut bolt = AbilityAsset::new("bolt");
        bolt.owner_needs = Some(q(r#"Has("Buff.Ready")"#));
        add_ability(&mut app, "bolt", bolt);
        let owner = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1000.0)]);
        let g = grant_ability(app.world_mut(), owner, "bolt", GrantOptions::default()).expect("grant");
        let world = app.world_mut();
        if start_first {
            world.write_message(StartAbility {
                owner,
                grant: g,
                context: StartContext::default(),
            });
            world.write_message(ApplyEffect::new("buff", owner));
        } else {
            world.write_message(ApplyEffect::new("buff", owner));
            world.write_message(StartAbility {
                owner,
                grant: g,
                context: StartContext::default(),
            });
        }
        step(&mut app);
        ability_state(app.world(), owner, g)
    };
    assert_eq!(
        trial(true),
        Some(AbilityState::Running),
        "the apply kind lands before the start kind, whatever the write order (ADR-7573)"
    );
    assert_eq!(
        trial(false),
        Some(AbilityState::Running),
        "apply written first: the tag lands in time"
    );
}

/// Finding 2 (rows): a chance-stream row whose guid no longer matches a live entity is dropped at
/// the recompute pass, so a streaming world with short-lived guid-bearing targets does not keep
/// one row per target forever, in memory and in the save. A live target's row (and stream
/// position) is untouched.
#[test]
fn chance_rows_for_despawned_guids_are_pruned() {
    let mut app = new_app();
    app.world_mut().resource_mut::<AbilitySettings>().seed = 1;
    let mut coin = instant();
    coin.chance = 0.5;
    coin.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
    add_effect(&mut app, "coin", coin);
    let a = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 100_000.0)]);
    app.world_mut().entity_mut(a).insert(EntityGuid(Guid(11)));
    let b = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 100_000.0)]);
    app.world_mut().entity_mut(b).insert(EntityGuid(Guid(22)));
    for _ in 0..4 {
        let _ = apply(&mut app, ApplyEffect::new("coin", a));
    }
    for _ in 0..6 {
        let _ = apply(&mut app, ApplyEffect::new("coin", b));
    }
    let rolls = |app: &App| app.world().resource::<AbilityCounters>().rolls.clone();
    assert_eq!(rolls(&app).len(), 2, "one row per (guid, asset) that rolled");
    let b_position = rolls(&app).iter().find(|r| r.0 == 22).expect("b rolled").2;

    app.world_mut().despawn(a);
    step(&mut app);
    let rolls = rolls(&app);
    assert!(
        rolls.iter().all(|r| r.0 != 11),
        "the despawned guid's row is gone: {rolls:?}"
    );
    assert_eq!(
        rolls.iter().find(|r| r.0 == 22).expect("b survives").2,
        b_position,
        "the live target's stream position is untouched"
    );
}

/// Finding 2 (replay): once a stream has been advanced, a roll costs O(1) — no replay of the
/// saved position. The drawn values are exactly the manual replay of the documented § 7 stream,
/// before and after this fix.
#[test]
fn chance_rolls_replay_nothing_once_the_stream_is_warm() {
    let mut app = new_app();
    app.world_mut().resource_mut::<AbilitySettings>().seed = 1;
    let mut coin = instant();
    coin.chance = 0.5;
    coin.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
    add_effect(&mut app, "coin", coin);
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 100_000.0)]);
    app.world_mut().entity_mut(e).insert(EntityGuid(Guid(7)));
    let before = chance_replay_steps();
    let outcomes: Vec<Outcome> = (0..64)
        .map(|_| apply(&mut app, ApplyEffect::new("coin", e)).outcome)
        .collect();
    assert_eq!(
        chance_replay_steps() - before,
        0,
        "a warm stream rolls in O(1), not by replaying its position"
    );
    let asset_id = regia_framework::tags::fnv1a64(b"coin");
    let mut rng = Rng::with_stream(1 ^ 7, asset_id);
    let expect: Vec<Outcome> = (0..64)
        .map(|_| {
            if rng.f32() < 0.5 {
                Outcome::Applied
            } else {
                Outcome::FailedChance
            }
        })
        .collect();
    assert_eq!(outcomes, expect, "the drawn values are the § 7 stream");
}

/// Finding 3: a `WhileTagged` start that fails its check writes
/// `AbilityLifecycle::Refused { cause, tag }` like every other start path (spec 53 § 2.6), and
/// the `while_hold` cache suppresses the repeats while the cause is unchanged.
#[test]
fn a_while_tagged_refusal_writes_the_refused_lifecycle_message() {
    let mut app = new_app();
    let mut haste = AbilityAsset::new("haste");
    haste.triggers = vec![Trigger::WhileTagged(t("State.Sprinting"))];
    haste.owner_needs = Some(q(r#"Has("State.Rested")"#));
    add_ability(&mut app, "haste", haste);
    let owner = app
        .world_mut()
        .spawn(TagContainer::from_names(["State.Sprinting"]).expect("tags"))
        .id();
    let g = grant_ability(app.world_mut(), owner, "haste", GrantOptions::default()).expect("grant");
    let mut log: Log<AbilityLifecycle> = Log::new();
    steps(&mut app, 2);
    log.read(&app);
    let codes = lifecycle_codes(&log.seen);
    assert!(
        codes.contains(&"refused:owner_missing_tag".to_owned()),
        "the failed WhileTagged start reports its refusal: {codes:?}"
    );
    assert_eq!(
        codes.iter().filter(|c| *c == "refused:owner_missing_tag").count(),
        1,
        "the held refusal is not reported again: {codes:?}"
    );
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Ready));
}

/// Finding 4: the `message_budget` warning reports the exact number of dropped messages. A burst
/// of one message past the budget drops exactly one.
#[test]
fn a_burst_past_the_budget_reports_the_exact_number_dropped() {
    let _serial = MESSAGE_BUDGET_WARN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = new_app();
    let mut bump = instant();
    bump.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
    add_effect(&mut app, "bump", bump);
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 100_000.0)]);
    let ((), text) = capture_warn(|| {
        for _ in 0..MAX_MESSAGES_PER_STEP + 1 {
            app.world_mut().write_message(ApplyEffect::new("bump", e));
        }
        step(&mut app);
    });
    assert_eq!(
        base(&app, e, "x"),
        MAX_MESSAGES_PER_STEP as f32,
        "only the budget's worth applied"
    );
    let line = text
        .lines()
        .find(|l| l.contains("budget exceeded"))
        .unwrap_or_else(|| panic!("no budget warning in the captured output: {text}"));
    let dropped: usize = line
        .split("dropped=")
        .nth(1)
        .and_then(|rest| rest.split([',', ' ']).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no dropped count in the warning: {line}"));
    assert_eq!(dropped, 1, "one message past the budget is one dropped: {line}");
}

/// Finding 6: a book that takes its tag baseline on the first `Triggers` pass (a fresh grant, and
/// a loaded book: `seen_tags` is not saved) fires no `TagGained` for tags already held on that
/// pass, while `WhileTagged` triggers evaluate the tags directly and start. Losing and regaining
/// the tag fires `TagGained` on the same grant (spec 53 § 4.6).
#[test]
fn a_loaded_book_takes_its_tag_baseline_without_tag_gained_events() {
    let mut app = new_app();
    let mut whilerun = AbilityAsset::new("whilerun");
    whilerun.triggers = vec![Trigger::WhileTagged(t("State.Sprinting"))];
    add_ability(&mut app, "whilerun", whilerun);
    let mut react = AbilityAsset::new("react");
    react.triggers = vec![Trigger::TagGained(t("State.Sprinting"))];
    add_ability(&mut app, "react", react);
    let owner = app
        .world_mut()
        .spawn(TagContainer::from_names(["State.Sprinting"]).expect("tags"))
        .id();
    let gw = grant_ability(app.world_mut(), owner, "whilerun", GrantOptions::default()).expect("w");
    let gr = grant_ability(app.world_mut(), owner, "react", GrantOptions::default()).expect("r");
    step(&mut app);
    assert_eq!(
        ability_state(app.world(), owner, gw),
        Some(AbilityState::Running),
        "WhileTagged evaluates the held tags directly"
    );
    assert_eq!(
        ability_state(app.world(), owner, gr),
        Some(AbilityState::Ready),
        "the first pass is the baseline: no TagGained for a tag already held"
    );
    // A loaded book starts at the same baseline: seen_tags is reflect-ignored, so a save/load
    // leaves it None.
    let registry = app.world().resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let book = app.world().get::<AbilityBook>(owner).expect("book");
    let (ron, _) = reflect_resave(&registry, book);
    assert!(
        !ron.contains("seen_tags"),
        "the tag baseline is not saved; a loaded book takes it again: {ron}"
    );

    app.world_mut().entity_mut(owner).insert(TagContainer::default());
    steps(&mut app, 2);
    assert_eq!(ability_state(app.world(), owner, gw), Some(AbilityState::Ready));
    app.world_mut()
        .entity_mut(owner)
        .insert(TagContainer::from_names(["State.Sprinting"]).expect("tags"));
    step(&mut app);
    assert_eq!(
        ability_state(app.world(), owner, gr),
        Some(AbilityState::Running),
        "regaining the tag fires TagGained"
    );
    assert_eq!(ability_state(app.world(), owner, gw), Some(AbilityState::Running));
}
