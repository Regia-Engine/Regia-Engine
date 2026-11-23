// SPDX-License-Identifier: MPL-2.0
//! Spec 51 § 8 test 16: chooser bindings. A provider fills the context from components; an
//! `OnChange` binding evaluates once while the context stays the same, a changed input raises
//! exactly one `ChooserPicked`, and swapping the table switches the whole set.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Rng;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::chooser::{
    ChooserAppExt, ChooserBinding, ChooserContext, ChooserOutcome, ChooserPicked, ChooserPlugin,
    ChooserStats, ChooserTable, ChooserTraceLog, ChooserValue, Choosers, Reeval, ResultEntry,
};
use regia_scene::components::EntityGuid;

#[derive(Component)]
struct Speed(f32);

fn speed_provider(world: &World, entity: Entity, ctx: &mut ChooserContext) {
    if let Some(speed) = world.get::<Speed>(entity) {
        ctx.set("speed", ChooserValue::Float(speed.0));
    }
}

fn locomotion(set: &str) -> ChooserTable {
    ChooserTable::from_ron_str(&format!(
        r#"(format_version: 1, result: Asset("clip"), inputs: {{ "speed": Float }},
            columns: [(id: "speed", input: "speed", kind: FloatRange)],
            rows: [
                (id: "idle", cells: {{ "speed": (0.0, 0.5) }}, result: Asset("anim/{set}_idle.clip.ron")),
                (id: "walk", cells: {{ "speed": (0.5, 3.0) }}, result: Asset("anim/{set}_walk.clip.ron")),
                (id: "run", result: Asset("anim/{set}_run.clip.ron")),
            ])"#
    ))
    .expect("locomotion table")
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ChooserPlugin));
    enable_fixed_step(&mut app, 60.0);
    app.register_chooser_provider("speed", speed_provider);
    {
        let mut choosers = app.world_mut().resource_mut::<Choosers>();
        choosers.insert("loco/unarmed.chooser.ron", locomotion("unarmed"));
        choosers.insert("loco/rifle.chooser.ron", locomotion("rifle"));
    }
    app
}

fn drain(app: &App, cursor: &mut MessageCursor<ChooserPicked>) -> Vec<ChooserPicked> {
    cursor
        .read(app.world().resource::<Messages<ChooserPicked>>())
        .cloned()
        .collect()
}

#[test]
fn on_change_evaluates_once_for_an_unchanged_context_and_once_per_change() {
    let mut app = app();
    let mut cursor = MessageCursor::default();
    let entity = app
        .world_mut()
        .spawn((
            Speed(0.0),
            ChooserBinding::new("loco/unarmed.chooser.ron", "speed", "locomotion"),
        ))
        .id();
    let mut picks = Vec::new();
    for _ in 0..100 {
        app.update();
        picks.extend(drain(&app, &mut cursor));
    }
    let binding = app.world().get::<ChooserBinding>(entity).expect("binding");
    assert_eq!(binding.evaluations, 1, "an unchanged context is not re-evaluated");
    assert_eq!(picks.len(), 1, "the first pick is reported once");
    assert_eq!(picks[0].row.as_deref(), Some("idle"));
    assert_eq!(picks[0].site, "locomotion");
    assert_eq!(app.world().resource::<ChooserStats>().skipped, 99);

    app.world_mut().get_mut::<Speed>(entity).expect("speed").0 = 2.0;
    app.update();
    app.update();
    let changed = drain(&app, &mut cursor);
    assert_eq!(changed.len(), 1, "one change, one message");
    assert_eq!(changed[0].row.as_deref(), Some("walk"));
    assert_eq!(
        changed[0].result,
        ResultEntry::Asset("anim/unarmed_walk.clip.ron".into())
    );

    // An input change that keeps the same pick evaluates but raises nothing.
    app.world_mut().get_mut::<Speed>(entity).expect("speed").0 = 2.5;
    app.update();
    assert!(drain(&app, &mut cursor).is_empty());
    assert_eq!(
        app.world()
            .get::<ChooserBinding>(entity)
            .expect("binding")
            .evaluations,
        3
    );
}

#[test]
fn swapping_the_table_switches_the_set() {
    let mut app = app();
    let mut cursor = MessageCursor::default();
    let entity = app
        .world_mut()
        .spawn((
            Speed(5.0),
            ChooserBinding::new("loco/unarmed.chooser.ron", "speed", "loco"),
        ))
        .id();
    app.update();
    assert_eq!(
        drain(&app, &mut cursor)[0].result,
        ResultEntry::Asset("anim/unarmed_run.clip.ron".into())
    );
    app.world_mut()
        .get_mut::<ChooserBinding>(entity)
        .expect("binding")
        .table = "loco/rifle.chooser.ron".into();
    app.update();
    let swapped = drain(&app, &mut cursor);
    assert_eq!(swapped.len(), 1);
    assert_eq!(swapped[0].table, "loco/rifle.chooser.ron");
    assert_eq!(
        swapped[0].result,
        ResultEntry::Asset("anim/rifle_run.clip.ron".into())
    );
}

#[test]
fn manual_bindings_wait_for_a_request_and_the_trace_log_keeps_their_traces() {
    let mut app = app();
    app.world_mut().resource_mut::<ChooserTraceLog>().enabled = true;
    let entity = app
        .world_mut()
        .spawn((
            Speed(1.0),
            ChooserBinding::new("loco/unarmed.chooser.ron", "speed", "manual").with_reeval(Reeval::Manual),
        ))
        .id();
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(
        app.world()
            .get::<ChooserBinding>(entity)
            .expect("binding")
            .evaluations,
        0
    );
    app.world_mut()
        .get_mut::<ChooserBinding>(entity)
        .expect("binding")
        .request();
    app.update();
    let binding = app.world().get::<ChooserBinding>(entity).expect("binding");
    assert_eq!(binding.evaluations, 1);
    assert_eq!(
        binding.last_pick.as_ref().and_then(|p| p.row.as_deref()),
        Some("walk")
    );
    let log = app.world().resource::<ChooserTraceLog>();
    let traces = log.traces(Some("manual"), 10);
    assert_eq!(traces.len(), 1);
    assert_eq!(traces[0].trace.selected.as_deref(), Some("walk"));
    assert_eq!(traces[0].trace.table, "loco/unarmed.chooser.ron");
}

#[test]
fn a_binding_without_a_table_or_provider_is_counted_and_skipped() {
    let mut app = app();
    app.world_mut().spawn((
        Speed(1.0),
        ChooserBinding::new("loco/missing.chooser.ron", "speed", "x"),
    ));
    app.world_mut().spawn((
        Speed(1.0),
        ChooserBinding::new("loco/unarmed.chooser.ron", "nobody", "y"),
    ));
    app.update();
    let stats = *app.world().resource::<ChooserStats>();
    assert_eq!(stats.unresolved, 2);
    assert_eq!(stats.evaluations, 0);
}

fn rated(rate: f32) -> String {
    format!(
        r#"(format_version: 1, result: Asset("montage"), inputs: {{ "speed": Float }}, outputs: {{ "rate": Float }},
            columns: [(id: "speed", input: "speed", kind: FloatRange), (id: "rate", output: "rate", kind: SetFloat)],
            rows: [
                (id: "front", cells: {{ "speed": (0.0, 10.0), "rate": {rate:?} }}, result: Asset("anim/front.montage.ron")),
            ])"#
    )
}

/// Audit-gameplay H7 (ADR-1809): a reload that keeps the row and result but edits an output
/// re-raises the pick with the new output, and the stored pick always holds the latest outcome.
#[test]
fn a_reload_that_only_edits_an_output_re_raises_the_pick() {
    let mut app = app();
    let mut cursor = MessageCursor::default();
    let path = "hit/front.chooser.ron";
    app.world_mut()
        .resource_mut::<Choosers>()
        .load_text(path, &rated(1.0))
        .expect("first load");
    let entity = app
        .world_mut()
        .spawn((Speed(1.0), ChooserBinding::new(path, "speed", "hit")))
        .id();
    app.update();
    let first = drain(&app, &mut cursor);
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0].outputs,
        vec![("rate".to_owned(), ChooserValue::Float(1.0))]
    );

    app.world_mut()
        .resource_mut::<Choosers>()
        .load_text(path, &rated(1.5))
        .expect("reload");
    app.update();
    let after = drain(&app, &mut cursor);
    assert_eq!(after.len(), 1, "the edited output is a new pick");
    assert_eq!(after[0].row.as_deref(), Some("front"));
    assert_eq!(
        after[0].outputs,
        vec![("rate".to_owned(), ChooserValue::Float(1.5))]
    );
    let binding = app.world().get::<ChooserBinding>(entity).expect("binding");
    assert_eq!(
        binding.last_pick.as_ref().map(|p| p.outputs.clone()),
        Some(vec![("rate".to_owned(), ChooserValue::Float(1.5))])
    );

    // A reload that changes nothing the binding sees raises nothing.
    app.world_mut()
        .resource_mut::<Choosers>()
        .load_text(path, &rated(1.5))
        .expect("same reload");
    app.update();
    assert!(drain(&app, &mut cursor).is_empty());
}

// ---------------------------------------------------------------------------
// Audit G findings 3 and 4 (ADR-4756): fixed-step evaluation, guid-keyed streams.
// ---------------------------------------------------------------------------

fn weighted() -> ChooserTable {
    ChooserTable::from_ron_str(
        r#"(format_version: 1, mode: Weighted, result: Asset("cue"),
            columns: [(id: "w", kind: Weight)],
            rows: [
                (id: "a", cells: { "w": 1.0 }, result: Asset("s/a.cue.ron")),
                (id: "b", cells: { "w": 1.0 }, result: Asset("s/b.cue.ron")),
            ])"#,
    )
    .expect("weighted table")
}

/// Spec 51 § 2.7: `OnChange` compares the filled context by value with float bits compared
/// exactly, so `0.0` to `-0.0` is a change and the same NaN twice is not.
#[test]
fn on_change_compares_float_bits_exactly() {
    let mut app = app();
    let entity = app
        .world_mut()
        .spawn((
            Speed(0.0),
            ChooserBinding::new("loco/unarmed.chooser.ron", "speed", "locomotion"),
        ))
        .id();
    let evaluations = |app: &App| {
        app.world()
            .get::<ChooserBinding>(entity)
            .expect("binding")
            .evaluations
    };
    let set_and_step = |app: &mut App, speed: f32| {
        app.world_mut().get_mut::<Speed>(entity).expect("speed").0 = speed;
        for _ in 0..4 {
            app.update();
        }
    };
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(evaluations(&app), 1);
    set_and_step(&mut app, -0.0);
    assert_eq!(evaluations(&app), 2, "-0.0 has other bits than 0.0");
    set_and_step(&mut app, -0.0);
    assert_eq!(evaluations(&app), 2, "the same bits are no change");
    set_and_step(&mut app, f32::NAN);
    assert_eq!(evaluations(&app), 3, "NaN is a change");
    set_and_step(&mut app, f32::NAN);
    assert_eq!(evaluations(&app), 3, "the same NaN bits are no change");
}

/// Audit G finding 3 (ADR-4756): `Reeval::EveryTick` draws once per fixed step, so the same steps
/// packed into fewer rendered frames give the same pick sequence.
#[test]
fn every_tick_draws_once_per_fixed_step_not_per_rendered_frame() {
    let run = |updates: usize, steps_per_update: u32| {
        // 50 Hz: 20 ms steps divide exactly in nanoseconds, so `steps_per_update * step`
        // never starves the accumulator on rounding.
        let mut app = app();
        app.world_mut()
            .resource_mut::<Choosers>()
            .insert("loot/weighted.chooser.ron", weighted());
        if steps_per_update == 2 {
            app.insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_secs_f64(2.0 / 50.0),
            ));
            let mut fixed = FixedTime::default();
            fixed.hz = 50.0;
            fixed.max_substeps = 4;
            app.insert_resource(fixed);
        }
        app.world_mut().spawn((
            Speed(0.0),
            ChooserBinding::new("loot/weighted.chooser.ron", "speed", "loot")
                .with_seed(7)
                .with_reeval(Reeval::EveryTick),
        ));
        let mut cursor = MessageCursor::default();
        let mut picks = Vec::new();
        for _ in 0..updates {
            app.update();
            picks.extend(drain(&app, &mut cursor));
        }
        let mut query = app.world_mut().query::<&ChooserBinding>();
        let binding = query.single(app.world()).expect("one binding").clone();
        (picks, binding.evaluations, binding.rng)
    };
    let (picks_a, evals_a, rng_a) = run(60, 1);
    let (picks_b, evals_b, rng_b) = run(30, 2);
    assert_eq!(evals_a, 60);
    assert_eq!(
        evals_b, 60,
        "two steps per update still evaluate once per fixed step"
    );
    assert_eq!(
        picks_a, picks_b,
        "the same draws in the same order give the same picks"
    );
    assert_eq!(rng_a, rng_b, "the random stream state matches");
}

/// Audit G finding 4 (ADR-4756): the binding's stream is keyed by the entity's `EntityGuid`, not
/// its archetype-table id, so the spawn order (and a seek that respawns the entity) cannot change
/// the draws.
#[test]
fn binding_stream_follows_the_entity_guid_not_the_entity_id() {
    let pick_sequence = |extra_first: bool| {
        let mut app = app();
        app.world_mut()
            .resource_mut::<Choosers>()
            .insert("loot/weighted.chooser.ron", weighted());
        if extra_first {
            app.world_mut().spawn_empty();
        }
        app.world_mut().spawn((
            EntityGuid(regia_core::id::Guid::from_u128(
                0x0123_4567_89ab_cdef_fedc_ba98_7654_3210,
            )),
            Speed(0.0),
            ChooserBinding::new("loot/weighted.chooser.ron", "speed", "loot")
                .with_seed(1)
                .with_reeval(Reeval::EveryTick),
        ));
        let mut cursor = MessageCursor::default();
        let mut picks = Vec::new();
        for _ in 0..20 {
            app.update();
            picks.extend(drain(&app, &mut cursor).into_iter().map(|p| p.row));
        }
        let mut query = app.world_mut().query::<&ChooserBinding>();
        let binding = query.single(app.world()).expect("one binding");
        (picks, binding.rng.clone())
    };
    let a = pick_sequence(false);
    let b = pick_sequence(true);
    assert_eq!(
        a.0, b.0,
        "an unrelated entity spawned first must not change the picks"
    );
    assert_eq!(a.1, b.1, "the random stream state matches");
}

/// Audit G finding 4 (ADR-4756): the snapshot carries the binding with its random stream, so a
/// seek continues the picks where the recording left off.
#[test]
fn a_seek_continues_the_binding_stream_from_the_snapshot() {
    use regia_framework::demo_world::{DemoTracked, restore_world_demo, world_demo_snapshot};

    let mut app = app();
    app.world_mut()
        .resource_mut::<Choosers>()
        .insert("loot/weighted.chooser.ron", weighted());
    app.world_mut().spawn((
        DemoTracked,
        EntityGuid(regia_core::id::Guid::from_u128(
            0xaaaa_bbbb_cccc_dddd_eeee_ffff_0000_1111,
        )),
        Speed(0.0),
        ChooserBinding::new("loot/weighted.chooser.ron", "speed", "loot")
            .with_seed(1)
            .with_reeval(Reeval::EveryTick),
    ));
    let mut cursor = MessageCursor::default();
    let collect = |app: &App, cursor: &mut MessageCursor<ChooserPicked>| {
        cursor
            .read(app.world().resource::<Messages<ChooserPicked>>())
            .map(|p| p.row.clone())
            .collect::<Vec<_>>()
    };
    for _ in 0..10 {
        app.update();
    }
    let _first: Vec<_> = collect(&app, &mut cursor);
    let snapshot = world_demo_snapshot(app.world()).expect("snapshot");
    let mut recorded = Vec::new();
    for _ in 0..10 {
        app.update();
        recorded.extend(collect(&app, &mut cursor));
    }
    restore_world_demo(app.world_mut(), &snapshot).expect("restore");
    let mut replayed = Vec::new();
    for _ in 0..10 {
        app.update();
        replayed.extend(collect(&app, &mut cursor));
    }
    assert_eq!(
        recorded, replayed,
        "the restored stream continues the recorded picks"
    );
}

#[test]
fn a_bindings_first_roll_follows_its_guid_not_its_entity_index() {
    fn evaluate(spare_entities: usize) -> (Option<ChooserOutcome>, Option<Rng>) {
        let mut app = app();
        app.world_mut()
            .resource_mut::<Choosers>()
            .load_text(
                "weighted.chooser.ron",
                r#"(format_version: 1, mode: Weighted, result: Asset("cue"),
                columns: [(id: "w", kind: Weight)],
                rows: [
                    (id: "a", cells: { "w": 1.0 }, result: Asset("a.cue.ron")),
                    (id: "b", cells: { "w": 2.0 }, result: Asset("b.cue.ron")),
                    (id: "c", cells: { "w": 1.0 }, result: Asset("c.cue.ron")),
                ])"#,
            )
            .expect("weighted table");
        for _ in 0..spare_entities {
            app.world_mut().spawn_empty();
        }
        let entity = app
            .world_mut()
            .spawn((
                Speed(0.0),
                EntityGuid(Guid(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210)),
                ChooserBinding::new("weighted.chooser.ron", "speed", "weighted").with_seed(7),
            ))
            .id();
        app.update();
        let binding = app.world().get::<ChooserBinding>(entity).expect("binding");
        (binding.last_pick.clone(), binding.rng.clone())
    }

    let first = evaluate(1);
    let second = evaluate(0);
    assert_eq!(first.0, second.0, "same guid, table and seed pick the same row");
    assert_eq!(first.1, second.1, "the stored random stream matches too");
}

/// Muse read-only review of fix-framework2-determinism (high finding 2): a restored
/// `Reeval::OnChange` binding (the default) with an unchanged context must not draw and must not
/// re-raise. The straight run skips an unchanged context entirely; the skipped cache fields used
/// to make the first replayed evaluation draw a fresh pick — consuming a random number the
/// recording never took (every later pick diverges) and re-raising a pick the straight run never
/// raised.
#[test]
fn a_seek_does_not_redraw_an_unchanged_on_change_binding() {
    use regia_framework::demo_world::{DemoTracked, restore_world_demo, world_demo_snapshot};

    let mut app = app();
    app.world_mut()
        .resource_mut::<Choosers>()
        .insert("loot/weighted.chooser.ron", weighted());
    app.world_mut().spawn((
        DemoTracked,
        EntityGuid(regia_core::id::Guid::from_u128(
            0xaaaa_bbbb_cccc_dddd_eeee_ffff_0000_1111,
        )),
        Speed(0.0),
        // The default reeval is OnChange; the context (Speed 0.0) never changes.
        ChooserBinding::new("loot/weighted.chooser.ron", "speed", "loot").with_seed(1),
    ));
    let mut cursor = MessageCursor::default();
    let collect = |app: &App, cursor: &mut MessageCursor<ChooserPicked>| {
        cursor
            .read(app.world().resource::<Messages<ChooserPicked>>())
            .map(|p| p.row.clone())
            .collect::<Vec<_>>()
    };
    // Drain after every update: message buffers expire within two frames.
    let mut initial = Vec::new();
    for _ in 0..10 {
        app.update();
        initial.extend(collect(&app, &mut cursor));
    }
    assert_eq!(
        initial.len(),
        1,
        "one evaluation, one pick, then the context never changes"
    );
    let snapshot = world_demo_snapshot(app.world()).expect("snapshot");
    let mut recorded = Vec::new();
    for _ in 0..10 {
        app.update();
        recorded.extend(collect(&app, &mut cursor));
    }
    restore_world_demo(app.world_mut(), &snapshot).expect("restore");
    let mut replayed = Vec::new();
    for _ in 0..10 {
        app.update();
        replayed.extend(collect(&app, &mut cursor));
    }
    assert_eq!(
        recorded, replayed,
        "the replay of an unchanged OnChange context must match the recording"
    );
    let mut query = app.world_mut().query::<&ChooserBinding>();
    let binding = query.single(app.world()).expect("one binding");
    assert_eq!(
        binding.evaluations, 1,
        "a restore must not re-evaluate an unchanged OnChange binding (no extra draw)"
    );
}
