// SPDX-License-Identifier: MPL-2.0
//! Spec 53 §§ 9 and 10: no per-step heap allocation after warm-up, asserted with a counting
//! allocator over fixed steps (no wall clock). Scenes use the § 10 size, 1,000 entities with 8
//! effects each, and every step runs `FixedUpdate` on this thread, so the thread-local count sees
//! the whole `AbilitySet`.
//!
//! Its own test binary: it installs a counting global allocator.

use regia_app::prelude::*;
use regia_ecs::ecs::message::Messages;
use regia_ecs::ecs::schedule::{Schedule, ScheduleLabel, Schedules, SingleThreadedExecutor};
use regia_ecs::ecs::world::World;
use regia_framework::abilities::*;
use regia_framework::tags::{Tag, TagContainer, TagQuery};

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests and live bytes.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

fn allocations() -> usize {
    alloc_counter::thread_allocs() as usize
}

fn live_bytes() -> isize {
    alloc_counter::thread_live_bytes() as isize
}

const ENTITIES: usize = 1_000;
const EFFECTS: usize = 8;
const WARM_UP_STEPS: usize = 40;
const MEASURED_STEPS: usize = 120;

fn tag(name: &str) -> Tag {
    Tag::new(name).expect("valid tag")
}

fn query(text: &str) -> TagQuery {
    TagQuery::from_ron(text).expect("query")
}

fn modifier(attribute: &str, op: ModOp, amount: Magnitude) -> Modifier {
    Modifier {
        attribute: attribute.to_owned(),
        op,
        amount,
        per_stack: false,
        when: None,
    }
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AbilityPlugin::default());
    app.update();
    // On this thread, so the thread-local count sees every system of the step.
    let label = FixedUpdate.intern();
    if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    app
}

fn attributes() -> Attributes {
    Attributes::from_list([
        Attribute::new("health_max", 100.0, 1.0, 9999.0),
        Attribute::new("health", 100.0, 0.0, 9999.0).with_max_from("health_max"),
        Attribute::new("stamina", 50.0, 0.0, 100.0),
        Attribute::new("strength", 10.0, 0.0, 100.0),
        Attribute::new("speed", 4.0, 0.0, 12.0),
        Attribute::new("armor", 0.0, 0.0, 1000.0).with_derived(&[("strength", 2.0)], 1.0),
    ])
    .expect("attributes")
}

/// A live effect with a long clock: every modifier source a step evaluates, with status tags, an
/// `active_if` and a `when`.
fn live_effect(index: usize) -> EffectAsset {
    let mut asset = EffectAsset::new(Lifetime::Timed(Magnitude::Const(1_000_000.0)));
    asset.live_mods = vec![
        modifier("strength", ModOp::AddPre, Magnitude::Const(1.0)),
        modifier("speed", ModOp::Scale, Magnitude::Const(0.9)),
        Modifier {
            when: Some(query(r#"Has("State.Armed")"#)),
            ..modifier("armor", ModOp::AddPost, Magnitude::Const(2.0))
        },
        modifier(
            "stamina",
            ModOp::AddPost,
            Magnitude::Stat {
                side: StatSide::Target,
                attribute: "strength".to_owned(),
                read: StatRead::Live,
                weight: 0.25,
                offset: 0.0,
            },
        ),
    ];
    asset.status_tags = vec![tag(&format!("Status.Effect{index}"))];
    asset.active_if = Some(query(r#"NoneOf([Has("State.Immune")])"#));
    asset
}

fn step(app: &mut App) {
    app.world_mut().run_schedule(FixedUpdate);
}

/// Message buffers turn over as a frame's `message_update` would do it, outside the counted part.
fn turn_over_messages(app: &mut App) {
    let world = app.world_mut();
    world.resource_mut::<Messages<AttributeChanged>>().update();
    world.resource_mut::<Messages<EffectiveTagsChanged>>().update();
    world.resource_mut::<Messages<CueEvent>>().update();
    world.resource_mut::<Messages<AbilityLifecycle>>().update();
}

/// What the executor allocates by itself in one run of `label`, whatever the systems do, measured
/// on a schedule of as many no-op systems. In a whole-workspace build `tracing` has its `log`
/// feature (feature unification), and then the ECS formats each system's name for a span even with
/// no subscriber: one `String` per system per run, none under `cargo test -p` (ADR-2012). The
/// counts in this file are what the engine's code does above that, so they stay exact in both
/// builds, and the allowance cannot grow past one per system unnoticed.
fn executor_allocations(app: &App, label: impl ScheduleLabel) -> usize {
    fn no_op() {}
    let systems = app
        .world()
        .resource::<Schedules>()
        .get(label.intern())
        .map_or(0, Schedule::systems_len);
    let mut schedule = Schedule::default();
    schedule.set_executor(SingleThreadedExecutor::new());
    for _ in 0..systems {
        schedule.add_systems(no_op);
    }
    let mut world = World::new();
    schedule.run(&mut world);
    // The least of three runs: one run that caught a lazy one-off allocation would inflate the
    // allowance subtracted from every measured step (audit-v035-flaky L5).
    let spent = (0..3)
        .map(|_| {
            let before = allocations();
            schedule.run(&mut world);
            allocations() - before
        })
        .min()
        .unwrap_or(0);
    assert!(spent <= systems, "{spent} allocations by {systems} no-op systems");
    spent
}

/// Heap allocations in [`MEASURED_STEPS`] steps after [`WARM_UP_STEPS`], above the executor's own.
fn allocations_per_steady_run(app: &mut App) -> usize {
    for _ in 0..WARM_UP_STEPS {
        step(app);
        turn_over_messages(app);
    }
    let executor = executor_allocations(app, FixedUpdate);
    let mut total = 0;
    for _ in 0..MEASURED_STEPS {
        let before = allocations();
        step(app);
        total += (allocations() - before).abs_diff(executor);
        turn_over_messages(app);
    }
    total
}

/// The control: a step that does allocate is seen, so a zero below means none happened.
#[test]
fn the_count_sees_an_allocation_inside_a_step() {
    fn allocates() {
        std::hint::black_box(Box::new(1_u64));
    }
    let mut app = app();
    app.add_systems(FixedUpdate, allocates);
    let label = FixedUpdate.intern();
    if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    assert!(allocations_per_steady_run(&mut app) >= MEASURED_STEPS);
}

#[test]
fn idle_entities_allocate_nothing_per_step() {
    let mut app = app();
    for i in 0..ENTITIES {
        let tags = TagContainer::from_names(["State.Armed"]).expect("tags");
        let mut entity = app.world_mut().spawn(attributes());
        if i % 2 == 0 {
            entity.insert(tags);
        }
    }
    assert_eq!(allocations_per_steady_run(&mut app), 0);
    assert_eq!(app.world().resource::<AbilityStats>().modifier_evaluations, 0);
}

#[test]
fn live_effects_allocate_nothing_per_step() {
    let mut app = app();
    for index in 0..EFFECTS {
        app.world_mut()
            .resource_mut::<AbilityAssets>()
            .insert_effect(&format!("e{index}.effect.ron"), live_effect(index));
    }
    for i in 0..ENTITIES {
        let tags = TagContainer::from_names(["State.Armed"]).expect("tags");
        let mut entity = app.world_mut().spawn(attributes());
        if i % 2 == 0 {
            entity.insert(tags);
        }
        let target = entity.id();
        for index in 0..EFFECTS {
            let outcome = apply_effect(
                app.world_mut(),
                ApplyEffect::new(&format!("e{index}.effect.ron"), target),
            )
            .expect("apply");
            assert_eq!(outcome.outcome, Outcome::Applied);
        }
    }
    assert_eq!(allocations_per_steady_run(&mut app), 0);
    let stats = app.world().resource::<AbilityStats>();
    assert_eq!(stats.live_effects, (ENTITIES * EFFECTS) as u64);
    assert!(stats.modifier_evaluations > 0, "the effects were evaluated");
}

/// Pulses change bases, write cues and report the change, every other step.
#[test]
fn pulses_allocate_nothing_after_warm_up() {
    let mut app = app();
    let mut poison = EffectAsset::new(Lifetime::UntilRemoved);
    poison.pulse = Some(Pulse {
        every: 2.0 / 60.0,
        on_apply: false,
    });
    poison.base_edits = vec![modifier("stamina", ModOp::AddPre, Magnitude::Const(-0.0001))];
    poison.cues = vec![CueBinding {
        tag: tag("Cue.Poison"),
        at: CueAt::Pulsed,
        params: CueParams::default(),
    }];
    app.world_mut()
        .resource_mut::<AbilityAssets>()
        .insert_effect("poison.effect.ron", poison);
    let mut targets = Vec::new();
    for _ in 0..ENTITIES {
        let target = app.world_mut().spawn(attributes()).id();
        apply_effect(app.world_mut(), ApplyEffect::new("poison.effect.ron", target)).expect("apply");
        targets.push(target);
    }
    assert_eq!(allocations_per_steady_run(&mut app), 0);
    // The zero is about steps that did the work: every target pulsed through the measured run.
    let pulses = (WARM_UP_STEPS + MEASURED_STEPS) / 2;
    for target in targets {
        let stamina = app
            .world()
            .get::<Attributes>(target)
            .and_then(|a| a.base(AttrId::of("stamina")))
            .expect("stamina");
        let expected = 50.0 - pulses as f32 * 0.0001;
        assert!(
            (stamina - expected).abs() < 0.01,
            "{stamina} after {pulses} pulses"
        );
    }
    let cues = app.world().resource::<Messages<CueEvent>>();
    assert!(!cues.is_empty(), "the pulses wrote cues");
}

/// Every grant state a step scans: a running `WhileTagged` grant, `Ready` ones refused each step
/// by a cooldown and by a cost (a dry run of the edit), a `TagGained` one, and one with no triggers.
#[test]
fn grants_allocate_nothing_per_step() {
    let mut app = app();
    let mut cooldown = EffectAsset::new(Lifetime::Timed(Magnitude::Const(1_000_000.0)));
    cooldown.status_tags = vec![tag("Cooldown.Held")];
    app.world_mut()
        .resource_mut::<AbilityAssets>()
        .insert_effect("cooldown.effect.ron", cooldown);
    let mut too_dear = EffectAsset::new(Lifetime::Instant);
    too_dear.base_edits = vec![modifier("stamina", ModOp::AddPre, Magnitude::Const(-1_000.0))];
    app.world_mut()
        .resource_mut::<AbilityAssets>()
        .insert_effect("too_dear.effect.ron", too_dear);
    let held = || vec![Trigger::WhileTagged(tag("State.Armed"))];
    let cooldown = || Some("cooldown.effect.ron".to_owned());
    let cost = || Some("too_dear.effect.ron".to_owned());
    let abilities = [
        ("while.ability.ron", held(), None, None),
        ("refused.ability.ron", held(), cooldown(), None),
        ("poor.ability.ron", held(), None, cost()),
        (
            "gained.ability.ron",
            vec![Trigger::TagGained(tag("State.Never"))],
            None,
            None,
        ),
        ("plain.ability.ron", Vec::new(), None, None),
    ];
    for (path, triggers, cooldown, cost) in &abilities {
        let mut asset = AbilityAsset::new(path);
        asset.triggers.clone_from(triggers);
        asset.cooldown.clone_from(cooldown);
        asset.cost.clone_from(cost);
        app.world_mut()
            .resource_mut::<AbilityAssets>()
            .insert_ability(path, asset);
    }
    let mut owners = Vec::new();
    for _ in 0..ENTITIES {
        let tags = TagContainer::from_names(["State.Armed"]).expect("tags");
        let owner = app.world_mut().spawn((attributes(), tags)).id();
        for (path, _, _, _) in &abilities {
            grant_ability(app.world_mut(), owner, path, GrantOptions::default()).expect("grant");
        }
        apply_effect(app.world_mut(), ApplyEffect::new("cooldown.effect.ron", owner)).expect("apply");
        owners.push(owner);
    }
    assert_eq!(allocations_per_steady_run(&mut app), 0);
    // The zero is about the states it names, in order of `abilities`.
    for owner in owners {
        let grants: Vec<AbilityState> = app
            .world()
            .get::<AbilityBook>(owner)
            .expect("book")
            .grants
            .iter()
            .map(|g| g.state)
            .collect();
        assert_eq!(
            grants,
            [
                AbilityState::Running,
                AbilityState::Ready,
                AbilityState::Ready,
                AbilityState::Ready,
                AbilityState::Ready
            ]
        );
        let book = app.world().get::<AbilityBook>(owner).expect("book");
        let (refused, poor) = (book.grants[1].id, book.grants[2].id);
        assert_eq!(
            ability_state(app.world(), owner, refused),
            Some(AbilityState::Cooling)
        );
        let check = check_activation(app.world(), owner, poor, &StartContext::default());
        assert_eq!(check.map_err(|r| r.cause), Err(RefusalCause::CannotAfford));
    }
}

/// § 10: at most 256 bytes per live effect, for its inline part.
#[test]
fn an_applied_effect_stays_inside_its_memory_budget() {
    assert!(
        size_of::<AppliedEffect>() <= 256,
        "{} bytes",
        size_of::<AppliedEffect>()
    );
}

/// § 10: "<= 64 KiB for 1,000 idle entities". What the ability system itself holds for 1,000 entities that
/// carry attributes and nothing live: the bytes still allocated after their first steps (bookkeeping the
/// plugin adds, its per-step scratch, the sorted entity order) minus what was allocated before the first
/// step. The attributes are the game's data and were spawned before the count starts. Bytes, counted on
/// the thread that runs `FixedUpdate`; no wall clock.
///
/// Met by ADR-2426 (it was about 317 bytes per entity before): each pass keeps its § 7 order as the
/// sorted entities alone, and entities that report the same values share one `AttributeChanged`
/// snapshot (one row of their own may differ). `Attributes` itself did not grow to hold it: see
/// [`the_attributes_component_did_not_grow_to_hold_the_bookkeeping`].
#[test]
fn a_thousand_idle_entities_cost_the_ability_system_at_most_64_kib() {
    let held = idle_bytes(|_| attributes());
    assert!(
        held <= 64 * 1024,
        "the ability system holds {held} bytes for {ENTITIES} idle entities"
    );
    eprintln!("abilities idle memory: {held} bytes for {ENTITIES} entities");
}

/// § 10 with idle entities that are not all alike: each has a health of its own (a wave after a
/// fight). They share one snapshot, each with its own health row (ADR-2426).
#[test]
fn a_thousand_idle_entities_with_their_own_health_stay_inside_the_budget() {
    let held = idle_bytes(|i| {
        let mut set = attributes();
        set.set_base(AttrId::of("health"), 1.0 + i as f32 * 0.05)
            .expect("health");
        set
    });
    assert!(
        held <= 64 * 1024,
        "the ability system holds {held} bytes for {ENTITIES} idle entities"
    );
    eprintln!("abilities idle memory, own health: {held} bytes for {ENTITIES} entities");
}

/// The bytes the ability system still holds after [`WARM_UP_STEPS`] steps of [`ENTITIES`] idle
/// entities made by `make`, half of them tagged; checks every one was skipped in the last step.
fn idle_bytes(make: impl Fn(usize) -> Attributes) -> isize {
    let mut app = app();
    for i in 0..ENTITIES {
        let mut entity = app.world_mut().spawn(make(i));
        if i % 2 == 0 {
            entity.insert(TagContainer::from_names(["State.Armed"]).expect("tags"));
        }
    }
    let before = live_bytes();
    for _ in 0..WARM_UP_STEPS {
        step(&mut app);
        turn_over_messages(&mut app);
    }
    // The first step reports every attribute once; those messages are the game's to read and their
    // buffers are a high-water mark of that burst, not a cost per idle entity: drop them.
    let world = app.world_mut();
    world.insert_resource(Messages::<AttributeChanged>::default());
    world.insert_resource(Messages::<EffectiveTagsChanged>::default());
    world.insert_resource(Messages::<CueEvent>::default());
    world.insert_resource(Messages::<AbilityLifecycle>::default());
    let held = live_bytes() - before;
    assert_eq!(
        app.world().resource::<AbilityStats>().entities_skipped,
        ENTITIES as u64,
        "the steps ran and every idle entity was skipped"
    );
    held
}

/// ADR-2426: the idle-memory budget is not met by moving bookkeeping into the component, whose
/// bytes the count above does not see (they live in the ECS table the game spawned).
#[test]
fn the_attributes_component_did_not_grow_to_hold_the_bookkeeping() {
    assert!(size_of::<Attributes>() <= 96, "{} bytes", size_of::<Attributes>());
}
