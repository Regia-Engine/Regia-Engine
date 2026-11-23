// SPDX-License-Identifier: MPL-2.0
//! Spec 53 conformance tests (`docs/specs/53-abilities-attributes-effects.md`): behavior the
//! spec states that `abilities.rs` built but no test asserted (audit lane spec-20-52, ADR-5639).
//! A headless app with `MinimalPlugins`, a 60 Hz fixed step and `AbilityPlugin`; no sleeps, no
//! timing asserts.

use regia_app::prelude::*;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::abilities::*;
use regia_framework::tags::{Tag, TagContainer, TagQuery};
use regia_scene::components::Name;

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
    step(&mut app);
    app
}

fn new_app() -> App {
    app_with(AbilityPlugin::default())
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

fn instant() -> EffectAsset {
    EffectAsset::new(Lifetime::Instant)
}

fn cue(tag: &str, at: CueAt) -> CueBinding {
    CueBinding {
        tag: t(tag),
        at,
        params: CueParams::default(),
    }
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

fn codes(log: &[AbilityLifecycle], g: GrantId) -> Vec<String> {
    log.iter()
        .filter(|l| l.grant == g)
        .map(|l| match l.event {
            LifecycleEvent::Refused(r) => format!("refused:{}", r.cause.code()),
            LifecycleEvent::Ending { cancelled } => format!("ending:{cancelled}"),
            ref other => other.code().to_owned(),
        })
        .collect()
}

/// An owner with mana and health, a 10-mana instant cost and a 10-tick cooldown effect.
fn owner_setup() -> (App, Entity) {
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

// § 2.6 state table, row `any -> revoke`: a running grant is cancelled first, then `Revoked`.
#[test]
fn revoke_cancels_a_running_grant_first_then_writes_revoked() {
    let (mut app, owner) = owner_setup();
    let mut log: Log<AbilityLifecycle> = Log::new();
    let g = grant_ability(app.world_mut(), owner, "bolt", GrantOptions::default()).expect("grant");
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start");
    assert!(effective_has(app.world(), owner, t("State.Casting")));
    revoke_ability(app.world_mut(), owner, g, false).expect("revoke");
    log.read(&app);
    let seen = codes(&log.seen, g);
    let cancelled = seen
        .iter()
        .position(|c| c == "cancelled")
        .expect("cancelled first");
    let revoked = seen.iter().position(|c| c == "revoked").expect("revoked");
    assert!(cancelled < revoked, "{seen:?}");
    assert_eq!(seen.last().map(String::as_str), Some("revoked"), "{seen:?}");
    assert_eq!(ability_state(app.world(), owner, g), None, "the grant is gone");
    assert!(
        !effective_has(app.world(), owner, t("State.Casting")),
        "the run tags leave with the run"
    );
    assert_eq!(
        revoke_ability(app.world_mut(), owner, g, false).map_err(|e| e.reason()),
        Err("grant_not_found")
    );
}

// § 2.6: "A grant revoked with `defer: true` loses its input binding at once and is removed when
// its run ends."
#[test]
fn a_deferred_revoke_unbinds_at_once_and_removes_after_the_run() {
    let (mut app, owner) = owner_setup();
    let mut log: Log<AbilityLifecycle> = Log::new();
    let g = grant_ability(
        app.world_mut(),
        owner,
        "bolt",
        GrantOptions {
            input_code: Some(5),
            ..GrantOptions::default()
        },
    )
    .expect("grant");
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start");
    revoke_ability(app.world_mut(), owner, g, true).expect("deferred revoke");
    let book = app.world().get::<AbilityBook>(owner).expect("book");
    let grant = book.get(g).expect("still there while running");
    assert_eq!(grant.input_code, None, "the binding goes at once");
    assert!(grant.pending_removal);
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Running));
    assert_eq!(
        check_activation(app.world(), owner, g, &StartContext::default()).map_err(|r| r.cause),
        Err(RefusalCause::NotGranted),
        "a grant pending removal cannot start"
    );
    log.read(&app);
    assert!(!codes(&log.seen, g).contains(&"revoked".to_owned()));
    end(app.world_mut(), owner, g).expect("end");
    step(&mut app);
    log.read(&app);
    assert_eq!(
        ability_state(app.world(), owner, g),
        None,
        "removed when the run ended"
    );
    assert!(codes(&log.seen, g).contains(&"revoked".to_owned()));
}

// § 2.6 Granting: `try_now` keeps nothing when the check fails; `one_shot` removes the grant
// after its first run ends.
#[test]
fn try_now_keeps_nothing_on_refusal_and_one_shot_removes_after_its_run() {
    let (mut app, owner) = owner_setup();
    let mut armed = AbilityAsset::new("armed");
    armed.owner_needs = Some(q(r#"Has("State.Armed")"#));
    add_ability(&mut app, "armed", armed);
    let refused = grant_ability(
        app.world_mut(),
        owner,
        "armed",
        GrantOptions {
            try_now: true,
            ..GrantOptions::default()
        },
    );
    assert_eq!(refused.map_err(|e| e.reason()), Err("owner_missing_tag"));
    assert!(
        app.world()
            .get::<AbilityBook>(owner)
            .is_none_or(|b| b.grants.is_empty()),
        "a refused try_now keeps no grant"
    );
    app.world_mut()
        .entity_mut(owner)
        .insert(TagContainer::from_names(["State.Armed"]).expect("names"));
    let g = grant_ability(
        app.world_mut(),
        owner,
        "armed",
        GrantOptions {
            try_now: true,
            one_shot: true,
            ..GrantOptions::default()
        },
    )
    .expect("try_now starts");
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Running));
    end(app.world_mut(), owner, g).expect("end");
    step(&mut app);
    assert_eq!(
        ability_state(app.world(), owner, g),
        None,
        "one_shot removes the grant after its first run"
    );
}

// § 2.6 state table, row `Ready -> activate, check fails`: `AbilityLifecycle::Refused { cause, tag }`.
#[test]
fn a_refused_activate_writes_the_refused_lifecycle_message_with_its_tag() {
    let (mut app, owner) = owner_setup();
    let mut log: Log<AbilityLifecycle> = Log::new();
    let g = grant_ability(app.world_mut(), owner, "bolt", GrantOptions::default()).expect("grant");
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start");
    end(app.world_mut(), owner, g).expect("end");
    step(&mut app);
    let refusal = activate(app.world_mut(), owner, g, StartContext::default()).expect_err("cooling");
    assert_eq!(refusal.cause, RefusalCause::OnCooldown);
    log.read(&app);
    let last = log.seen.iter().rev().find(|l| l.grant == g).expect("a message");
    match last.event {
        LifecycleEvent::Refused(r) => {
            assert_eq!(r.cause, RefusalCause::OnCooldown);
            assert_eq!(r.tag, Some(t("Cooldown.Bolt")));
        }
        other => panic!("expected Refused, got {other:?}"),
    }
}

// § 2.6 Requirements: `target_forbids` against each picked entity, reported as `target_rejected`
// with the blocking tag.
#[test]
fn target_forbids_rejects_a_pick_that_holds_the_tag() {
    let (mut app, owner) = owner_setup();
    let mut heal = AbilityAsset::new("heal");
    heal.target_forbids = Some(q(r#"Has("Team.Enemy")"#));
    add_ability(&mut app, "heal", heal);
    let world = app.world_mut();
    let g = grant_ability(world, owner, "heal", GrantOptions::default()).expect("grant");
    let enemy = world
        .spawn(TagContainer::from_names(["Team.Enemy.Orc"]).expect("names"))
        .id();
    let friend = world
        .spawn(TagContainer::from_names(["Team.Friend"]).expect("names"))
        .id();
    let at = |e: Entity| StartContext {
        picks: Some(Picks::new(vec![Pick::Entity(e)])),
        ..StartContext::default()
    };
    let r = check_activation(world, owner, g, &at(enemy)).expect_err("enemy");
    assert_eq!(
        (r.cause, r.tag),
        (RefusalCause::TargetRejected, Some(t("Team.Enemy")))
    );
    assert_eq!(check_activation(world, owner, g, &at(friend)), Ok(()));
}

// § 2.6 Starting: `when_running: Restart` ends the run and starts again (no `already_running`).
#[test]
fn when_running_restart_starts_the_run_again() {
    let (mut app, owner) = owner_setup();
    let mut spin = AbilityAsset::new("spin");
    spin.when_running = WhenRunning::Restart;
    add_ability(&mut app, "spin", spin);
    let mut log: Log<AbilityLifecycle> = Log::new();
    let g = grant_ability(app.world_mut(), owner, "spin", GrantOptions::default()).expect("grant");
    activate(app.world_mut(), owner, g, StartContext::default()).expect("first");
    activate(app.world_mut(), owner, g, StartContext::default()).expect("restart is not a refusal");
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Running));
    log.read(&app);
    let seen = codes(&log.seen, g);
    assert_eq!(
        seen.iter().filter(|c| *c == "started").count(),
        2,
        "two starts: {seen:?}"
    );
    assert!(!seen.iter().any(|c| c.starts_with("refused")), "{seen:?}");
}

// § 2.6 Other: `max_level` caps the granted level.
#[test]
fn max_level_caps_the_granted_level() {
    let (mut app, owner) = owner_setup();
    let mut capped = AbilityAsset::new("capped");
    capped.max_level = 3;
    add_ability(&mut app, "capped", capped);
    let g = grant_ability(
        app.world_mut(),
        owner,
        "capped",
        GrantOptions {
            level: 7,
            ..GrantOptions::default()
        },
    )
    .expect("grant");
    let book = app.world().get::<AbilityBook>(owner).expect("book");
    assert_eq!(book.get(g).expect("grant").level, 3);
}

// § 2.6 check order: `AbilitySettings.cheats { free_costs, no_cooldowns }` skip those two checks.
#[test]
fn cheats_skip_the_cost_and_cooldown_checks() {
    let (mut app, owner) = owner_setup();
    let g = grant_ability(app.world_mut(), owner, "bolt", GrantOptions::default()).expect("grant");
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start");
    end(app.world_mut(), owner, g).expect("end");
    step(&mut app);
    let world = app.world_mut();
    assert_eq!(
        check_activation(world, owner, g, &StartContext::default()).map_err(|r| r.cause),
        Err(RefusalCause::OnCooldown)
    );
    world.resource_mut::<AbilitySettings>().cheats.no_cooldowns = true;
    assert_eq!(
        check_activation(world, owner, g, &StartContext::default()),
        Ok(())
    );
    world.resource_mut::<AbilitySettings>().cheats.no_cooldowns = false;
    // Spend the mana down so the cost cannot be paid.
    world
        .get_mut::<Attributes>(owner)
        .expect("attrs")
        .set_base(AttrId::of("mana"), 5.0)
        .expect("set");
    steps(&mut app, 12);
    let world = app.world_mut();
    assert_eq!(
        check_activation(world, owner, g, &StartContext::default()).map_err(|r| r.cause),
        Err(RefusalCause::CannotAfford)
    );
    world.resource_mut::<AbilitySettings>().cheats.free_costs = true;
    assert_eq!(
        check_activation(world, owner, g, &StartContext::default()),
        Ok(())
    );
}

fn no_healing(world: &World, req: &EditRequest) -> EditVerdict {
    if req.delta > 0.0
        && world
            .get::<Name>(req.target)
            .is_some_and(|n| n.as_str() == "diseased")
    {
        EditVerdict::Refuse
    } else {
        EditVerdict::Pass
    }
}

// § 2.2 Edit gates: `EditVerdict::Refuse` ("no healing while diseased") leaves the base unchanged.
#[test]
fn an_edit_gate_that_refuses_keeps_the_base() {
    let mut app = app_with(AbilityPlugin::default().gate(no_healing));
    let sick = spawn_attrs(&mut app, vec![Attribute::new("health", 50.0, 0.0, 100.0)]);
    app.world_mut().entity_mut(sick).insert(Name::new("diseased"));
    let well = spawn_attrs(&mut app, vec![Attribute::new("health", 50.0, 0.0, 100.0)]);
    let mut heal = instant();
    heal.base_edits = vec![modifier("health", ModOp::AddPre, 20.0)];
    add_effect(&mut app, "heal", heal);
    let mut hurt = instant();
    hurt.base_edits = vec![modifier("health", ModOp::AddPre, -20.0)];
    add_effect(&mut app, "hurt", hurt);
    for e in [sick, well] {
        apply(&mut app, ApplyEffect::new("heal", e));
    }
    step(&mut app);
    assert_eq!(base(&app, sick, "health"), 50.0, "the gate refused the heal");
    assert_eq!(base(&app, well, "health"), 70.0);
    apply(&mut app, ApplyEffect::new("hurt", sick));
    step(&mut app);
    assert_eq!(base(&app, sick, "health"), 30.0, "other edits pass the gate");
}

// § 4.5: "applying the effect skips that modifier and logs `unknown_attribute` once per
// application" (fail-before: a base edit logged one per modifier and a live modifier none).
#[test]
fn a_missing_attribute_is_skipped_and_logged_once_per_application() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("health", 50.0, 0.0, 100.0)]);
    let mut odd = instant();
    odd.base_edits = vec![
        modifier("stamina", ModOp::AddPre, -5.0),
        modifier("health", ModOp::AddPre, -5.0),
        modifier("armor", ModOp::AddPre, 3.0),
    ];
    add_effect(&mut app, "odd", odd);
    let mut slow = timed(1.0);
    slow.live_mods = vec![
        modifier("move_speed", ModOp::Scale, 0.5),
        modifier("jump", ModOp::Scale, 0.5),
    ];
    add_effect(&mut app, "slow", slow);
    let count = |app: &App| {
        app.world()
            .resource::<AbilityStats>()
            .warning_count("unknown_attribute")
    };
    let before = count(&app);
    apply(&mut app, ApplyEffect::new("odd", e));
    assert_eq!(count(&app) - before, 1, "one warning for one instant application");
    assert_eq!(base(&app, e, "health"), 45.0, "the known modifier still lands");
    assert!(
        app.world()
            .get::<Attributes>(e)
            .is_some_and(|a| a.get(AttrId::of("stamina")).is_none()),
        "no hidden attribute is created"
    );
    let before = count(&app);
    apply(&mut app, ApplyEffect::new("slow", e));
    steps(&mut app, 3);
    assert_eq!(count(&app) - before, 1, "one warning for one timed application");
    assert_eq!(live(&app, e).len(), 1, "the effect still applies");
}

// § 2.4 Application order step 2: `apply_if` against the target's effective tags.
#[test]
fn apply_if_refuses_with_blocked_condition() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 10.0)]);
    let mut wet = timed(1.0);
    wet.apply_if = Some(q(r#"Has("Status.Burning")"#));
    add_effect(&mut app, "douse", wet);
    let out = apply(&mut app, ApplyEffect::new("douse", e));
    assert_eq!(out.outcome, Outcome::BlockedCondition);
    assert_eq!(out.outcome.code(), "blocked_condition");
    assert!(live(&app, e).is_empty());
    app.world_mut()
        .entity_mut(e)
        .insert(TagContainer::from_names(["Status.Burning"]).expect("names"));
    assert_eq!(
        apply(&mut app, ApplyEffect::new("douse", e)).outcome,
        Outcome::Applied
    );
}

fn stacking(when_full: WhenFull) -> EffectAsset {
    let mut asset = timed(10.0 / 60.0);
    asset.stack = Some(StackRule {
        key: StackKey::ByTarget,
        cap: 2,
        on_add: OnAdd::default(),
        when_full,
        on_lapse: OnLapse::EndAll,
        cue_each_stack: false,
    });
    asset
}

// § 2.5 / § 5.2: a merge reports `stacked`; `when_full: Restart` ends the entry with `restarted`
// and applies fresh at 1 stack.
#[test]
fn a_merge_reports_stacked_and_a_full_restart_ends_with_restarted() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 10.0)]);
    add_effect(&mut app, "bleed", stacking(WhenFull::Restart));
    let mut ended: Log<EffectEnded> = Log::new();
    let first = apply(&mut app, ApplyEffect::new("bleed", e));
    assert_eq!((first.outcome, first.stacks), (Outcome::Applied, 1));
    let second = apply(&mut app, ApplyEffect::new("bleed", e));
    assert_eq!((second.outcome, second.stacks), (Outcome::Stacked, 2));
    assert_eq!(second.outcome.code(), "stacked");
    assert_eq!(second.id, first.id, "a merge keeps the entry");
    let third = apply(&mut app, ApplyEffect::new("bleed", e));
    assert_eq!(third.stacks, 1, "a restart applies fresh at 1 stack");
    assert_ne!(third.id, first.id, "a new entry");
    ended.read(&app);
    step(&mut app);
    ended.read(&app);
    assert!(
        ended
            .seen
            .iter()
            .any(|m| Some(m.id) == first.id && m.reason == EndReason::Restarted),
        "{:?}",
        ended.seen
    );
    assert_eq!(live(&app, e).len(), 1);
}

// § 2.4 Chain: `chain.on_removed` follows any removal other than expiry, applied in a later pass.
#[test]
fn a_removed_effect_runs_its_on_removed_chain_and_not_on_lapse() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 10.0)]);
    let mut mark = instant();
    mark.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
    add_effect(&mut app, "mark", mark);
    let mut lapse = instant();
    lapse.base_edits = vec![modifier("x", ModOp::AddPre, 5.0)];
    add_effect(&mut app, "lapse", lapse);
    let mut aura = timed(1.0);
    aura.chain.on_removed = vec!["mark".into()];
    aura.chain.on_lapse = vec!["lapse".into()];
    add_effect(&mut app, "aura", aura);
    let id = apply(&mut app, ApplyEffect::new("aura", e)).id.expect("entry");
    step(&mut app);
    remove_effect(app.world_mut(), e, id).expect("remove");
    steps(&mut app, 2);
    assert_eq!(base(&app, e, "x"), 1.0, "on_removed ran once, on_lapse did not");
}

// § 2.7: `cues_enabled = false` silences every cue; `cue_filter` suppresses per entity.
#[test]
fn cues_enabled_and_cue_filter_silence_cues() {
    fn not_quiet(e: Entity, world: &World) -> bool {
        world.get::<Name>(e).is_none_or(|n| n.as_str() != "quiet")
    }
    let mut app = new_app();
    let loud = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 10.0)]);
    let quiet = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 10.0)]);
    app.world_mut().entity_mut(quiet).insert(Name::new("quiet"));
    let mut hit = instant();
    hit.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
    hit.cues = vec![cue("Cue.Hit", CueAt::Applied)];
    add_effect(&mut app, "hit", hit);
    let mut cues: Log<CueEvent> = Log::new();
    app.world_mut().resource_mut::<AbilitySettings>().cues_enabled = false;
    apply(&mut app, ApplyEffect::new("hit", loud));
    assert!(cues.read(&app).is_empty(), "cues disabled");
    assert!(app.world().resource::<CueLog>().since(0, 10).is_empty());
    assert_eq!(base(&app, loud, "x"), 1.0, "the edit itself still lands");
    let mut settings = app.world_mut().resource_mut::<AbilitySettings>();
    settings.cues_enabled = true;
    settings.cue_filter = Some(not_quiet);
    apply(&mut app, ApplyEffect::new("hit", quiet));
    apply(&mut app, ApplyEffect::new("hit", loud));
    let seen: Vec<Entity> = cues.read(&app).iter().map(|c| c.entity).collect();
    assert_eq!(seen, [loud], "only the unfiltered entity cues");
}

// § 10 / § 5.1: the cue ring for `abilities.cues` keeps the last 256 cues.
#[test]
fn the_cue_ring_keeps_the_last_256() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 1000.0)]);
    let mut hit = instant();
    hit.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
    hit.cues = vec![cue("Cue.Hit", CueAt::Applied)];
    add_effect(&mut app, "hit", hit);
    for _ in 0..300 {
        apply(&mut app, ApplyEffect::new("hit", e));
    }
    assert_eq!(CUE_RING, 256);
    let ring = app.world().resource::<CueLog>().since(0, usize::MAX);
    assert_eq!(ring.len(), CUE_RING);
}

// § 2.7: `Applied` and `Removed` bindings write a `Burst`; a `Sustained` binding writes `Begin`
// when the effect starts and `End` when it ends.
#[test]
fn cue_kinds_follow_the_binding_moment() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 10.0)]);
    let mut burn = timed(5.0 / 60.0);
    burn.cues = vec![
        cue("Cue.Burn.Start", CueAt::Applied),
        cue("Cue.Burn", CueAt::Sustained),
        cue("Cue.Burn.Stop", CueAt::Removed),
    ];
    add_effect(&mut app, "burn", burn);
    let mut cues: Log<CueEvent> = Log::new();
    apply(&mut app, ApplyEffect::new("burn", e));
    for _ in 0..8 {
        step(&mut app);
        cues.read(&app);
    }
    cues.read(&app);
    let seen: Vec<(String, CueKind)> = cues
        .seen
        .iter()
        .map(|c| (c.tag.name().to_owned(), c.kind))
        .collect();
    let at = |name: &str, kind: CueKind| {
        seen.iter()
            .position(|(n, k)| n == name && *k == kind)
            .unwrap_or_else(|| panic!("no {name} {kind:?} in {seen:?}"))
    };
    let start = at("Cue.Burn.Start", CueKind::Burst);
    let begin = at("Cue.Burn", CueKind::Begin);
    let end = at("Cue.Burn", CueKind::End);
    let stop = at("Cue.Burn.Stop", CueKind::Burst);
    assert!(begin < end && start < end && begin < stop, "{seen:?}");
    assert_eq!(seen.len(), 4, "one cue per binding moment: {seen:?}");
}

// § 4.5: the validation codes `effect_bad_lifetime`, `effect_unknown_attribute`,
// `ability_bad_cost`, `ability_bad_cooldown`, `unknown_run_state` and the warning
// `ability_no_max_run_ticks`.
#[test]
fn validation_reports_each_remaining_code() {
    let has = |diags: &[Diagnostic], code: &str| diags.iter().any(|d| d.code == code);

    let zero = timed(0.0);
    assert!(has(&validate_effect(&zero, None), "effect_bad_lifetime"));
    let mut chance = timed(1.0);
    chance.chance = 1.5;
    assert!(has(&validate_effect(&chance, None), "effect_bad_lifetime"));
    let mut bad_pulse = timed(1.0);
    bad_pulse.pulse = Some(Pulse {
        every: 0.0,
        on_apply: false,
    });
    assert!(has(&validate_effect(&bad_pulse, None), "effect_bad_lifetime"));
    let mut bad_name = instant();
    bad_name.base_edits = vec![modifier("Not A Name", ModOp::AddPre, 1.0)];
    assert!(has(&validate_effect(&bad_name, None), "effect_unknown_attribute"));
    assert!(validate_effect(&timed(1.0), None).is_empty(), "a clean effect");

    let mut assets = AbilityAssets::default();
    assets.insert_effect("timed", timed(1.0));
    assets.insert_effect("instant", instant());
    let mut cd = timed(1.0);
    cd.status_tags = vec![t("Cooldown.X")];
    assets.insert_effect("cd", cd);

    let mut bad_cost = AbilityAsset::new("a");
    bad_cost.cost = Some("timed".into());
    bad_cost.max_run_ticks = Some(10);
    let d = validate_ability(&bad_cost, &assets, None, None);
    assert!(has(&d, "ability_bad_cost") && has_errors(&d), "{d:?}");

    let mut bad_cd = AbilityAsset::new("b");
    bad_cd.cooldown = Some("instant".into());
    bad_cd.max_run_ticks = Some(10);
    assert!(has(
        &validate_ability(&bad_cd, &assets, None, None),
        "ability_bad_cooldown"
    ));
    let mut untagged = AbilityAsset::new("c");
    untagged.cooldown = Some("timed".into());
    untagged.max_run_ticks = Some(10);
    assert!(has(
        &validate_ability(&untagged, &assets, None, None),
        "ability_bad_cooldown"
    ));

    let mut run = AbilityAsset::new("d");
    run.run_state = RunStateMode::KeepAcrossRuns;
    run.run_type = "missing".into();
    run.max_run_ticks = Some(10);
    assert!(has(
        &validate_ability(&run, &assets, None, None),
        "unknown_run_state"
    ));

    let mut good = AbilityAsset::new("e");
    good.cost = Some("instant".into());
    good.cooldown = Some("cd".into());
    let d = validate_ability(&good, &assets, None, None);
    let warn = d
        .iter()
        .find(|x| x.code == "ability_no_max_run_ticks")
        .expect("warning");
    assert_eq!(warn.severity, regia_framework::abilities::Severity::Warning);
    assert!(!has_errors(&d), "only the warning: {d:?}");
    good.max_run_ticks = Some(120);
    assert!(validate_ability(&good, &assets, None, None).is_empty());
}

// § 3.2 / § 3.4: ordinary systems write request messages (`ApplyEffect`, `StartAbility`,
// `CancelAbility`, `RevokeAbility`), consumed in `AbilitySet::Requests` of the next step; § 2.8:
// the `IntentSeq` is carried onto the live effect, the run and the resulting message.
#[test]
fn request_messages_land_in_the_next_step_and_carry_the_intent() {
    let (mut app, owner) = owner_setup();
    let mut spin = AbilityAsset::new("spin");
    spin.kind_tags = vec![t("Ability.Spin")];
    add_ability(&mut app, "spin", spin);
    add_effect(&mut app, "buff", timed(1.0));
    let mut applied: Log<EffectApplied> = Log::new();
    let mut request = ApplyEffect::new("buff", owner);
    request.intent = IntentSeq(42);
    app.world_mut().write_message(request);
    assert!(live(&app, owner).is_empty(), "nothing lands before the step");
    step(&mut app);
    let entries = live(&app, owner);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].intent, IntentSeq(42));
    let seen = applied.read(&app).to_vec();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(
        (seen[0].outcome, seen[0].intent),
        (Outcome::Applied, IntentSeq(42))
    );

    let g = grant_ability(app.world_mut(), owner, "spin", GrantOptions::default()).expect("grant");
    app.world_mut().write_message(StartAbility {
        owner,
        grant: g,
        context: StartContext {
            intent: IntentSeq(7),
            ..StartContext::default()
        },
    });
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Ready));
    step(&mut app);
    assert_eq!(ability_state(app.world(), owner, g), Some(AbilityState::Running));
    let run = app
        .world()
        .get::<AbilityBook>(owner)
        .and_then(|b| b.get(g))
        .and_then(|x| x.run.clone())
        .expect("a run");
    assert_eq!(run.intent, IntentSeq(7));
    app.world_mut().write_message(CancelAbility { owner, grant: g });
    step(&mut app);
    assert_ne!(ability_state(app.world(), owner, g), Some(AbilityState::Running));
    app.world_mut().write_message(RevokeAbility {
        owner,
        grant: g,
        defer: false,
    });
    step(&mut app);
    assert_eq!(ability_state(app.world(), owner, g), None, "revoked by message");
}

fn tick(app: &App) -> u64 {
    app.world().resource::<AbilityCounters>().tick
}

fn pulsing_stack(pulse: PulseOnAdd) -> EffectAsset {
    let mut asset = timed(1.0);
    asset.pulse = Some(Pulse {
        every: 10.0 / 60.0,
        on_apply: false,
    });
    asset.base_edits = vec![modifier("x", ModOp::AddPre, 1.0)];
    asset.stack = Some(StackRule {
        key: StackKey::ByTarget,
        cap: 3,
        on_add: OnAdd {
            time: TimeOnAdd::Keep,
            pulse,
        },
        when_full: WhenFull::Reject,
        on_lapse: OnLapse::EndAll,
        cue_each_stack: false,
    });
    asset
}

// § 2.5 `on_add.pulse`: `Restart` puts the next pulse one full period after the merge; `Keep`
// leaves it where it was.
#[test]
fn a_merge_restarts_or_keeps_the_pulse_timer() {
    for (pulse, restarted) in [(PulseOnAdd::Keep, false), (PulseOnAdd::Restart, true)] {
        let mut app = new_app();
        let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 100.0)]);
        add_effect(&mut app, "tick", pulsing_stack(pulse));
        let t0 = tick(&app);
        apply(&mut app, ApplyEffect::new("tick", e));
        assert_eq!(live(&app, e)[0].next_pulse_tick, t0 + 10);
        steps(&mut app, 4);
        let t1 = tick(&app);
        let out = apply(&mut app, ApplyEffect::new("tick", e));
        assert_eq!(out.outcome, Outcome::Stacked);
        let next = live(&app, e)[0].next_pulse_tick;
        let want = if restarted { t1 + 10 } else { t0 + 10 };
        assert_eq!(next, want, "{pulse:?}");
    }
}

// § 2.5: "The entry keeps its level unless the new application has a higher level; then the
// entry takes the higher level and re-evaluates its amounts."
#[test]
fn a_merge_at_a_higher_level_takes_that_level_and_reevaluates() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("x", 0.0, 0.0, 100.0)]);
    let mut aura = forever_with_stack();
    aura.curves = vec![InlineCurve {
        name: "power".into(),
        points: vec![(1.0, 1.0), (10.0, 10.0)],
    }];
    aura.live_mods = vec![Modifier {
        amount: Magnitude::Curve(LevelCurveRef {
            table: String::new(),
            name: "power".into(),
        }),
        ..modifier("x", ModOp::AddPost, 0.0)
    }];
    add_effect(&mut app, "aura", aura);
    let current = |app: &App| {
        app.world()
            .get::<Attributes>(e)
            .and_then(|a| a.get(AttrId::of("x")))
            .expect("x")
    };
    apply(&mut app, ApplyEffect::new("aura", e).at_level(2));
    step(&mut app);
    assert_eq!(current(&app), 2.0);
    apply(&mut app, ApplyEffect::new("aura", e).at_level(5));
    step(&mut app);
    assert_eq!(live(&app, e)[0].level, 5);
    assert_eq!(current(&app), 5.0, "amounts re-evaluated at the higher level");
    apply(&mut app, ApplyEffect::new("aura", e).at_level(3));
    step(&mut app);
    assert_eq!(live(&app, e)[0].level, 5, "a lower level keeps the entry's");
    assert_eq!(current(&app), 5.0);
}

fn forever_with_stack() -> EffectAsset {
    let mut asset = EffectAsset::new(Lifetime::UntilRemoved);
    asset.stack = Some(StackRule {
        key: StackKey::ByTarget,
        cap: 5,
        on_add: OnAdd {
            time: TimeOnAdd::Keep,
            pulse: PulseOnAdd::Keep,
        },
        when_full: WhenFull::Reject,
        on_lapse: OnLapse::EndAll,
        cue_each_stack: false,
    });
    asset
}

// § 2.4 `grants` with `on_effect_end: AfterRun`: a granted ability that is running when the
// effect ends keeps its run and is removed when the run ends.
#[test]
fn an_after_run_grant_outlives_its_effect_until_the_run_ends() {
    let (mut app, owner) = owner_setup();
    add_ability(&mut app, "dash", AbilityAsset::new("dash"));
    let mut haste = EffectAsset::new(Lifetime::UntilRemoved);
    haste.grants = vec![EffectGrant {
        ability: "dash".into(),
        level: 1,
        on_effect_end: OnEffectEnd::AfterRun,
    }];
    add_effect(&mut app, "haste", haste);
    let id = apply(&mut app, ApplyEffect::new("haste", owner))
        .id
        .expect("entry");
    let g = app
        .world()
        .get::<AbilityBook>(owner)
        .and_then(|b| b.grants.first().map(|x| x.id))
        .expect("granted");
    activate(app.world_mut(), owner, g, StartContext::default()).expect("start");
    remove_effect(app.world_mut(), owner, id).expect("remove");
    step(&mut app);
    assert_eq!(
        ability_state(app.world(), owner, g),
        Some(AbilityState::Running),
        "the run continues after the effect ended"
    );
    end(app.world_mut(), owner, g).expect("end");
    step(&mut app);
    assert_eq!(
        ability_state(app.world(), owner, g),
        None,
        "removed after the run"
    );
}

// § 4.5: a live modifier that both targets a missing attribute and takes its amount from that
// same missing attribute (`Magnitude::Stat`) logs `unknown_attribute` once per application, not
// once for the skipped modifier and once for the failed amount read.
#[test]
fn a_stat_amount_on_the_same_missing_attribute_warns_once_per_application() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("move_speed", 10.0, 0.0, 100.0)]);
    let mut drain = timed(1.0);
    drain.live_mods = vec![Modifier {
        attribute: "health".to_owned(),
        op: ModOp::Scale,
        amount: Magnitude::Stat {
            side: StatSide::Target,
            attribute: "health".to_owned(),
            read: StatRead::AtApply,
            weight: 1.0,
            offset: 0.0,
        },
        per_stack: false,
        when: None,
    }];
    add_effect(&mut app, "drain", drain);
    let count = |app: &App| {
        app.world()
            .resource::<AbilityStats>()
            .warning_count("unknown_attribute")
    };
    let before = count(&app);
    apply(&mut app, ApplyEffect::new("drain", e));
    steps(&mut app, 3);
    assert_eq!(
        count(&app) - before,
        1,
        "one warning for one application, not one per check"
    );
    assert_eq!(live(&app, e).len(), 1, "the effect still applies");
    assert!(
        app.world()
            .get::<Attributes>(e)
            .is_some_and(|a| a.get(AttrId::of("health")).is_none()),
        "no hidden attribute is created"
    );
}

// § 4.5: a suspended effect (its `active_if` does not match the target's effective tags) never
// applies its live modifiers, so a missing attribute on one logs no `unknown_attribute` warning.
#[test]
fn a_suspended_effect_warns_nothing_about_its_live_modifiers() {
    let mut app = new_app();
    let e = spawn_attrs(&mut app, vec![Attribute::new("move_speed", 10.0, 0.0, 100.0)]);
    let mut sleeper = timed(1.0);
    sleeper.active_if = Some(q(r#"Has("Status.Asleep")"#));
    sleeper.live_mods = vec![modifier("health", ModOp::Scale, 0.5)];
    add_effect(&mut app, "sleeper", sleeper);
    let count = |app: &App| {
        app.world()
            .resource::<AbilityStats>()
            .warning_count("unknown_attribute")
    };
    let before = count(&app);
    let out = apply(&mut app, ApplyEffect::new("sleeper", e));
    assert_eq!(out.outcome, Outcome::Applied);
    steps(&mut app, 3);
    assert_eq!(
        count(&app) - before,
        0,
        "a suspended effect applies no modifiers, so nothing is skipped"
    );
    assert!(
        live(&app, e).iter().all(|entry| entry.suspended),
        "the entry is suspended"
    );
}
