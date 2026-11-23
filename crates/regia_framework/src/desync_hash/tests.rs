// SPDX-License-Identifier: MPL-2.0
use super::*;

use serde::Serialize;

#[derive(Component, Serialize, Clone, Copy, Debug, PartialEq)]
struct Pos([i32; 2]);

#[derive(Component, Serialize, Clone, Copy, Debug, PartialEq)]
struct Hp(u32);

/// Not hashed: only moves entities between archetypes.
#[derive(Component)]
struct Marker;

#[derive(Resource, Serialize, Default)]
struct Gold(u32);

#[derive(Component, Serialize, Clone, Copy)]
struct NetId(u32);

impl DesyncKey for NetId {
    fn desync_key(&self) -> u64 {
        u64::from(self.0)
    }
}

const SYSTEMS: Systems<3> = Systems::new(["movement", "combat", "economy"]);

fn hash() -> EcsDesyncHash<3> {
    EcsDesyncHash::new(SYSTEMS)
        .component::<Pos>("movement")
        .component::<Hp>("combat")
        .resource::<Gold>("economy")
}

/// Three lockstep entities spawned in the same order (so the same ids) in every world.
fn world() -> (World, [Entity; 3]) {
    let mut world = World::new();
    world.insert_resource(Gold(7));
    let a = world.spawn(Pos([1, 1])).id();
    let b = world.spawn(Pos([2, 2])).id();
    let c = world.spawn((Pos([3, 3]), Hp(5))).id();
    (world, [a, b, c])
}

fn query_order(world: &mut World) -> Vec<Entity> {
    world.query_filtered::<Entity, With<Pos>>().iter(world).collect()
}

#[test]
fn ecs_set_hash_ignores_archetype_order() {
    // Two worlds with the same lockstep state. In `moved`, `a` visits an archetype with an unhashed
    // `Marker` and comes back, so it sits at the end of its table: queries visit `b` before `a`.
    let (mut plain, ids) = world();
    let (mut moved, moved_ids) = world();
    assert_eq!(ids, moved_ids, "both worlds hand out the same entity ids");
    let [a, _, _] = moved_ids;
    moved.entity_mut(a).insert(Marker);
    moved.entity_mut(a).remove::<Marker>();
    let plain_order = query_order(&mut plain);
    let moved_order = query_order(&mut moved);
    assert_ne!(
        plain_order, moved_order,
        "the test must visit the entities in another order"
    );

    let (mut h_plain, mut h_moved) = (hash(), hash());
    let w_plain = h_plain.hashes(&plain);
    let w_moved = h_moved.hashes(&moved);
    assert_eq!(
        w_plain, w_moved,
        "archetype order {plain_order:?} vs {moved_order:?} moved a word"
    );

    // The same entities spawned after archetypes were created in another order: `other` made the
    // `(Pos, Hp)` archetype before the `Pos` one, so its queries visit `b` first.
    let mut other = World::new();
    other.insert_resource(Gold(7));
    let x = other.spawn((Pos([0, 0]), Hp(0))).id();
    other.despawn(x);
    let mut spare = World::new();
    spare.insert_resource(Gold(7));
    let y = spare.spawn(Pos([0, 0])).id();
    spare.despawn(y);
    for w in [&mut other, &mut spare] {
        w.spawn(Pos([1, 1]));
        let b = w.spawn(Hp(5)).id();
        w.entity_mut(b).insert(Pos([2, 2]));
    }
    let (other_order, spare_order) = (query_order(&mut other), query_order(&mut spare));
    assert_ne!(other_order, spare_order, "archetype creation order must differ");
    assert_eq!(
        hash().hashes(&other),
        hash().hashes(&spare),
        "archetype order {other_order:?} vs {spare_order:?} moved a word"
    );
}

#[test]
fn a_change_moves_only_its_system() {
    let (mut world, [a, _, c]) = world();
    let mut h = hash();
    let base = h.hashes(&world);
    // The same call again gives the same words (the cached query state changes nothing).
    assert_eq!(h.hashes(&world), base);

    world.get_mut::<Pos>(a).expect("pos").0[0] += 1;
    let moved = h.hashes(&world);
    assert_ne!(moved.words[0], base.words[0]);
    assert_eq!(moved.words[1..], base.words[1..]);
    world.get_mut::<Pos>(a).expect("pos").0[0] -= 1;
    assert_eq!(h.hashes(&world), base);

    world.get_mut::<Hp>(c).expect("hp").0 = 4;
    let hit = h.hashes(&world);
    assert_eq!(hit.words[0], base.words[0]);
    assert_ne!(hit.words[1], base.words[1]);
    assert_eq!(hit.words[2], base.words[2]);
    world.get_mut::<Hp>(c).expect("hp").0 = 5;

    world.resource_mut::<Gold>().0 += 1;
    let paid = h.hashes(&world);
    assert_eq!(paid.words[..2], base.words[..2]);
    assert_ne!(paid.words[2], base.words[2]);
    world.resource_mut::<Gold>().0 -= 1;

    // A missing resource folds as absent, not as a skipped column.
    world.remove_resource::<Gold>();
    assert_ne!(h.hashes(&world).words[2], base.words[2]);
    world.insert_resource(Gold(7));

    // Removing a hashed component from one entity moves its system.
    world.entity_mut(c).remove::<Hp>();
    assert_ne!(h.hashes(&world).words[1], base.words[1]);
}

#[test]
fn swapped_values_and_new_entities_are_seen() {
    let (mut world, [a, b, _]) = world();
    let mut h = hash();
    let base = h.hashes(&world);
    // Swap a's and b's positions: the multiset of values is the same, the owners are not.
    world.entity_mut(a).insert(Pos([2, 2]));
    world.entity_mut(b).insert(Pos([1, 1]));
    assert_ne!(h.hashes(&world).words[0], base.words[0]);
    world.entity_mut(a).insert(Pos([1, 1]));
    world.entity_mut(b).insert(Pos([2, 2]));
    assert_eq!(h.hashes(&world), base);

    // A new entity with a default-looking value still counts.
    let d = world.spawn(Pos([0, 0])).id();
    assert_ne!(h.hashes(&world).words[0], base.words[0]);
    world.despawn(d);
    assert_eq!(h.hashes(&world), base);
}

#[test]
fn order_by_key_ignores_presentation_entities() {
    // `cosmetic` spawns a camera-like entity first, so every lockstep entity gets another id.
    let mut lean = World::new();
    let mut cosmetic = World::new();
    cosmetic.spawn(Marker);
    for w in [&mut lean, &mut cosmetic] {
        w.spawn((NetId(20), Pos([2, 2])));
        w.spawn((NetId(10), Pos([1, 1])));
        // Not a lockstep entity: no id, so it is left out under `order_by`.
        w.spawn(Pos([9, 9]));
    }
    // Only `cosmetic` has this one.
    cosmetic.spawn(Pos([5, 5]));

    let keyed = || EcsDesyncHash::new(Systems::new(["movement"])).component::<Pos>("movement");
    assert_eq!(
        keyed().order_by::<NetId>().hashes(&lean),
        keyed().order_by::<NetId>().hashes(&cosmetic)
    );
    // By entity id the two worlds differ (the ids and the extra entity are part of the fold).
    assert_ne!(keyed().hashes(&lean), keyed().hashes(&cosmetic));
    // The key is folded too: swapping ids between the values is a split.
    let mut swapped = World::new();
    swapped.spawn((NetId(10), Pos([2, 2])));
    swapped.spawn((NetId(20), Pos([1, 1])));
    assert_ne!(
        keyed().order_by::<NetId>().hashes(&lean),
        keyed().order_by::<NetId>().hashes(&swapped)
    );
}

#[test]
fn an_unregistered_component_hashes_as_empty() {
    // `Hp` was never spawned or registered: its column folds zero rows until it appears.
    let mut world = World::new();
    world.spawn(Pos([1, 1]));
    let mut h = hash();
    let empty = h.hashes(&world);
    let e = world.spawn(Hp(1)).id();
    let one = h.hashes(&world);
    assert_ne!(one.words[1], empty.words[1]);
    world.despawn(e);
    assert_eq!(h.hashes(&world), empty);
}

#[test]
fn ecs_words_are_pinned() {
    // Integer-only state, so the canonical float rule cannot move these. They change only with the
    // fold layout (tags, the key, the row count) or `Mix`'s words, which a lockstep build must not.
    let (world, _) = world();
    let words = hash().hashes(&world).words;
    let printed: Vec<String> = words.iter().map(|w| format!("{w:#018x}")).collect();
    assert_eq!(printed, PINNED, "the ECS fold layout moved");
}

const PINNED: [&str; 3] = ["0xcfe097780c1c9d3c", "0x058664269586f817", "0x93579ebd1c69f87f"];

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Step;

fn step(mut q: Query<&mut Pos>) {
    for mut pos in &mut q {
        pos.0[0] += 1;
    }
}

fn plugin_app(plugin: DesyncHashPlugin<3>) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins).init_resource::<Gold>();
    app.add_systems(FixedUpdate, step.in_set(Step));
    app.add_plugins(plugin.after(Step));
    app.world_mut().spawn(Pos([0, 0]));
    app
}

fn plugin() -> DesyncHashPlugin<3> {
    DesyncHashPlugin::new(SYSTEMS)
        .component::<Pos>("movement")
        .component::<Hp>("combat")
        .resource::<Gold>("economy")
}

#[test]
fn the_plugin_records_after_the_step_in_fixed_update() {
    let mut app = plugin_app(plugin());
    for _ in 0..3 {
        app.world_mut().run_schedule(FixedUpdate);
    }
    let world = app.world_mut();
    let rows = desync_rows(world, None, 10).expect("installed");
    assert_eq!(rows.systems, ["movement", "combat", "economy"]);
    assert_eq!(rows.recorded, 3);
    assert_eq!(rows.newest, Some(3));
    assert_eq!(rows.rows.iter().map(|r| r.tick).collect::<Vec<_>>(), [1, 2, 3]);
    // The newest row is the state after the third step (the record ran after `Step`).
    assert_eq!(world.query::<&Pos>().single(world).expect("one").0, [3, 0]);
    let now = hash().hashes(world);
    assert_eq!(rows.rows[2].words, now.words.to_vec());
    // Each step moved only `movement`.
    assert_ne!(rows.rows[0].words[0], rows.rows[1].words[0]);
    assert_eq!(rows.rows[0].words[1..], rows.rows[1].words[1..]);

    // Windows: `to` and `count`.
    let two = desync_rows(world, Some(2), 2).expect("installed");
    assert_eq!(two.rows.iter().map(|r| r.tick).collect::<Vec<_>>(), [1, 2]);
    assert!(desync_rows(world, Some(9), 3).expect("installed").rows.is_empty());
    let typed = world.resource::<EcsDesyncHash<3>>();
    assert_eq!(typed.ring().len(), 3);
    assert_eq!(typed.last().map(|r| r.tick), Some(3));
    assert_eq!(typed.columns()[0], ("movement", std::any::type_name::<Pos>()));
}

#[derive(Resource)]
struct Label(u64);

#[test]
fn tick_from_labels_rows_and_a_rewind_replaces_them() {
    let mut app = plugin_app(plugin().tick_from(|w| w.resource::<Label>().0));
    for label in [10, 11, 12, 11, 12] {
        app.insert_resource(Label(label));
        app.world_mut().run_schedule(FixedUpdate);
    }
    let rows = desync_rows(app.world(), None, 96).expect("installed");
    assert_eq!(rows.recorded, 5);
    assert_eq!(rows.rows.iter().map(|r| r.tick).collect::<Vec<_>>(), [10, 11, 12]);
}

#[test]
fn two_runs_name_the_split_system_and_tick() {
    let mut left = plugin_app(plugin());
    let mut right = plugin_app(plugin());
    for tick in 1..=8u64 {
        if tick == 6 {
            right.world_mut().resource_mut::<Gold>().0 += 1;
        }
        left.world_mut().run_schedule(FixedUpdate);
        right.world_mut().run_schedule(FixedUpdate);
    }
    let ours = desync_rows(left.world(), None, 96).expect("installed").rows;
    let theirs = desync_rows(right.world(), None, 96).expect("installed").rows;
    let report = compare_desync_rows(left.world(), &ours, &theirs)
        .expect("installed")
        .expect("same width");
    assert_eq!(report.list(), "economy@6");
    assert_eq!(report.last_agreed, Some(5));
    assert_eq!(report.compared, 8);
    let lines = divergence_lines(8, "left", "right", &report);
    assert!(lines[1].contains("root economy first 6"), "{lines:?}");

    // Identical runs never split.
    let same = compare_desync_rows(left.world(), &ours, &ours)
        .expect("installed")
        .expect("same width");
    assert!(same.systems.is_empty());
    assert_eq!(same.last_agreed, Some(8));

    // A row of the wrong width is refused, not compared.
    let short = vec![DesyncRow::new(3, vec![1, 2])];
    assert_eq!(
        compare_desync_rows(left.world(), &ours, &short).expect("installed"),
        Err(WordCountMismatch {
            tick: 3,
            words: 2,
            systems: 3
        })
    );
}

#[test]
fn without_the_plugin_there_are_no_rows() {
    let world = World::new();
    assert_eq!(desync_rows(&world, None, 8), None);
    assert_eq!(compare_desync_rows(&world, &[], &[]), None);
    // The record system is a no-op without the resource.
    let mut world = World::new();
    record_desync_hashes::<3>(&mut world);
    assert!(!world.contains_resource::<EcsDesyncHash<3>>());
}

#[test]
#[should_panic(expected = "`physics` is not one of the systems")]
fn an_unknown_system_name_panics_at_setup() {
    let _ = EcsDesyncHash::new(SYSTEMS).component::<Pos>("physics");
}

// ADR-8547: the diagnose ring, partial rows and external sims.

#[derive(Component, Serialize, Clone, Copy)]
struct Heading(f32);

/// Fails to serialize while `0` is set (a user impl that cannot serialize some state).
#[derive(Resource, Default)]
struct Flaky(bool);

impl Serialize for Flaky {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.0 {
            return Err(serde::ser::Error::custom("not now"));
        }
        serializer.serialize_u8(1)
    }
}

const RAW_SYSTEMS: Systems<2> = Systems::new(["heading", "flaky"]);

fn raw_world(heading: f32) -> World {
    let mut world = World::new();
    world.init_resource::<Flaky>();
    world.spawn(Heading(heading));
    world
}

#[test]
fn the_ecs_diagnose_ring_names_a_zero_sign_split() {
    let make = || {
        EcsDesyncHash::new(RAW_SYSTEMS)
            .component::<Heading>("heading")
            .resource::<Flaky>("flaky")
            .diagnose_raw()
    };
    let (mut a, mut b) = (make(), make());
    let (wa, mut wb) = (raw_world(0.0), raw_world(0.0));
    for tick in 1..=6u64 {
        if tick == 4 {
            let mut q = wb.query::<&mut Heading>();
            for mut h in q.iter_mut(&mut wb) {
                h.0 = -0.0;
            }
        }
        a.record(&wa, tick);
        b.record(&wb, tick);
    }
    // The canonical words agree (the wire rule), the raw words split at 4.
    assert_eq!(a.ring().window(6, 6), b.ring().window(6, 6));
    let ours: Vec<DesyncRow> = a
        .ring()
        .window_rows(6, 6)
        .iter()
        .map(DesyncRow::from_ring)
        .collect();
    let theirs: Vec<DesyncRow> = b
        .ring()
        .window_rows(6, 6)
        .iter()
        .map(DesyncRow::from_ring)
        .collect();
    assert!(
        ours.iter()
            .all(|row| row.raw.as_ref().is_some_and(|raw| raw.len() == 2))
    );
    let d = compare_typed(&RAW_SYSTEMS, &ours, &theirs).expect("same width");
    assert!(d.canonical.systems.is_empty());
    assert_eq!(
        d.float_bits,
        vec![FloatBitsSplit {
            system: "heading",
            raw_tick: 4,
            canonical_tick: None
        }]
    );
    // A plain hash keeps the same canonical words and no raw ones.
    let mut plain = EcsDesyncHash::new(RAW_SYSTEMS)
        .component::<Heading>("heading")
        .resource::<Flaky>("flaky");
    let row = plain.record(&wb, 6);
    assert_eq!(Some(row.hashes), b.last().map(|r| r.hashes));
    assert!(plain.ring().window_rows(6, 1)[0].raw.is_none());
}

#[test]
fn a_failing_resource_serialize_marks_the_row_partial() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .init_resource::<Flaky>()
        .add_plugins(
            DesyncHashPlugin::new(RAW_SYSTEMS)
                .component::<Heading>("heading")
                .resource::<Flaky>("flaky")
                .in_schedule(Update),
        );
    app.world_mut().spawn(Heading(1.0));
    app.update();
    app.world_mut().resource_mut::<Flaky>().0 = true;
    app.update();
    let rows = desync_rows(app.world(), None, 8).expect("installed");
    assert_eq!(rows.source, DesyncSource::Ecs);
    assert_eq!(rows.partial_rows, 1);
    assert_eq!(
        rows.rows.iter().map(|r| r.partial).collect::<Vec<_>>(),
        [false, true]
    );
    let d = diagnose_desync_rows(app.world(), &rows.rows, &rows.rows)
        .expect("installed")
        .expect("same width");
    assert_eq!(d.partial_from, Some(2));
}

struct Ext {
    gold: u32,
    tick: u64,
}

impl regia_detsim::hash::SystemFold<1> for Ext {
    const SYSTEMS: Systems<1> = Systems::new(["economy"]);
    fn fold_systems(&self, mix: &mut [Mix; 1]) {
        mix[0].u64(u64::from(self.gold));
    }
}

#[test]
fn a_desync_feed_reads_back_through_its_type_erased_reader() {
    let feed = DesyncFeed::<1>::new(Ext::SYSTEMS, false);
    let reader = feed.reader();
    assert!(reader.rows(None, 8).rows.is_empty());
    let mut ext = Ext { gold: 0, tick: 0 };
    for tick in 1..=5u64 {
        ext.tick = tick;
        ext.gold += 2;
        feed.record(ext.tick, &ext);
    }
    let rows = reader.rows(None, 3);
    assert_eq!(rows.source, DesyncSource::SimHost);
    assert_eq!((rows.newest, rows.recorded, rows.raw), (Some(5), 5, false));
    assert_eq!(rows.rows.iter().map(|r| r.tick).collect::<Vec<_>>(), [3, 4, 5]);
    assert_eq!(
        feed.window_rows(5, 1)[0].hashes.words.to_vec(),
        rows.rows[2].words
    );
    let mut theirs = rows.rows.clone();
    theirs[1].words[0] ^= 1;
    let d = reader.diagnose(&rows.rows, &theirs).expect("same width");
    assert_eq!(d.canonical.list(), "economy@4");
    assert_eq!(
        reader.diagnose(&rows.rows, &[DesyncRow::new(4, vec![1, 2])]),
        Err(WordCountMismatch {
            tick: 4,
            words: 2,
            systems: 1
        })
    );
    // A world without an ECS hash and without a host handle has no rows.
    assert!(desync_rows(&World::new(), None, 8).is_none());
}

#[test]
fn compare_typed_reads_every_row_at_a_tick() {
    // The second row at tick 5 on their side differs in `combat`; the first agrees (ADR-8724).
    let ours = vec![DesyncRow::new(4, vec![1, 1, 1]), DesyncRow::new(5, vec![1, 1, 1])];
    let theirs = vec![
        DesyncRow::new(4, vec![1, 1, 1]),
        DesyncRow::new(5, vec![1, 1, 1]),
        DesyncRow::new(5, vec![1, 2, 1]),
    ];
    let d = compare_typed(&SYSTEMS, &ours, &theirs).expect("same width");
    assert_eq!(d.canonical.list(), "combat@5");
    assert_eq!(d.canonical.last_agreed, Some(4));
    assert_eq!(
        d.canonical.unmatched,
        vec![UnmatchedRow {
            side: RowSide::Right,
            tick: 5,
            index: 2,
            tick_shared: true,
        }]
    );
    // Mirrored: the extra row on our side is read the same way.
    let d = compare_typed(&SYSTEMS, &theirs, &ours).expect("same width");
    assert_eq!(d.canonical.list(), "combat@5");
    assert_eq!(d.canonical.unmatched[0].side, RowSide::Left);
}

/// KI-GP-003 (ADR-10062): under `order_by_content` a column folds its rows in the order of their
/// value hashes, so two peers whose worlds hold the same lockstep values under other entity ids
/// (a presentation entity spawned first, or the same entities spawned in another order) agree,
/// while the default entity-id order tells them apart. A changed, added or duplicated value is
/// still a split (the rows are a multiset).
#[test]
fn content_order_ignores_entity_ids() {
    let mut lean = World::new();
    let mut shifted = World::new();
    for w in [&mut lean, &mut shifted] {
        w.insert_resource(Gold(7));
    }
    // A presentation entity first: every lockstep entity of `shifted` gets another id.
    shifted.spawn(Marker);
    lean.spawn(Pos([1, 1]));
    lean.spawn(Pos([2, 2]));
    lean.spawn((Pos([3, 3]), Hp(5)));
    // The same values spawned in the reverse order.
    shifted.spawn((Pos([3, 3]), Hp(5)));
    let moved = shifted.spawn(Pos([2, 2])).id();
    shifted.spawn(Pos([1, 1]));

    let content = || hash().order_by_content();
    assert_eq!(content().hashes(&lean), content().hashes(&shifted));
    assert_ne!(hash().hashes(&lean).words[0], hash().hashes(&shifted).words[0]);

    let base = content().hashes(&shifted);
    shifted.entity_mut(moved).insert(Pos([2, 3]));
    assert_ne!(content().hashes(&shifted).words[0], base.words[0]);
    shifted.entity_mut(moved).insert(Pos([2, 2]));
    assert_eq!(content().hashes(&shifted), base);
    let twin = shifted.spawn(Pos([2, 2])).id();
    assert_ne!(
        content().hashes(&shifted).words[0],
        base.words[0],
        "a duplicate row counts"
    );
    shifted.despawn(twin);
    assert_eq!(content().hashes(&shifted), base);
    // The plugin takes the same option.
    let _ = plugin().order_by_content();
}

/// KI-GP-003 (ADR-10062): a failing `Serialize` under content order still marks the row partial.
#[test]
fn content_order_keeps_the_partial_flag() {
    let mut world = World::new();
    world.spawn(Failing);
    let mut h = EcsDesyncHash::new(Systems::new(["broken"]))
        .component::<Failing>("broken")
        .order_by_content();
    assert!(h.fold(&world, FloatRule::Canonical).partial);
}

#[derive(Component)]
struct Failing;

impl Serialize for Failing {
    fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("planted"))
    }
}
