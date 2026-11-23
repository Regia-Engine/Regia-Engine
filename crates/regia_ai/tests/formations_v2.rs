// SPDX-License-Identifier: MPL-2.0
//! Formation assets and minimum-cost slot assignment (ADR-8358).
//!
//! Fixed steps and exact comparisons. No wall clock.

use regia_ai::AiPlugin;
use regia_ai::blackboard::Blackboard;
use regia_ai::formation::{AiFormation, FormationAnchor, FormationShape, resolve_formation};
use regia_ai::formation_asset::{
    AssignMember, AssignSlot, FormationAsset, FormationFile, SlotAssignment, assign_slots,
    horizontal_distance_um,
};
use regia_ai::squad::{AiSquad, AiSquadMember, AiSquadPlugin};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;
use regia_test_utils::unique_temp_dir;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((AiPlugin, AiSquadPlugin));
    app.update();
    app
}

fn step(app: &mut App, n: usize) {
    for _ in 0..n {
        app.world_mut().run_schedule(FixedUpdate);
    }
}

fn member(id: u64, x: f32, z: f32, role: Option<&str>) -> AssignMember {
    AssignMember {
        id,
        position: Vec3::new(x, 0.0, z),
        role: role.map(str::to_string),
    }
}

fn slot(index: u32, x: f32, z: f32, role: Option<&str>) -> AssignSlot {
    AssignSlot {
        index,
        fill: index,
        position: Vec3::new(x, 0.0, z),
        role: role.map(str::to_string),
    }
}

fn total_um(members: &[AssignMember], slots: &[AssignSlot], assigned: &[SlotAssignment]) -> i64 {
    assigned
        .iter()
        .map(|row| {
            let member = members.iter().find(|item| item.id == row.member).expect("member");
            let slot = slots.iter().find(|item| item.index == row.slot).expect("slot");
            horizontal_distance_um(member.position, slot.position)
        })
        .sum()
}

/// Every bijection of `n` equal members and slots. `n <= 8`.
fn brute_min_um(members: &[AssignMember], slots: &[AssignSlot]) -> i64 {
    fn walk(
        members: &[AssignMember],
        slots: &[AssignSlot],
        used: &mut [bool],
        index: usize,
        acc: i64,
        best: &mut i64,
    ) {
        if index == members.len() {
            *best = (*best).min(acc);
            return;
        }
        if acc >= *best {
            return;
        }
        for (slot_i, slot) in slots.iter().enumerate() {
            if used[slot_i] {
                continue;
            }
            used[slot_i] = true;
            let dist = horizontal_distance_um(members[index].position, slot.position);
            walk(members, slots, used, index + 1, acc + dist, best);
            used[slot_i] = false;
        }
    }
    let mut used = vec![false; slots.len()];
    let mut best = i64::MAX;
    let mut members = members.to_vec();
    members.sort_by_key(|item| item.id);
    walk(&members, slots, &mut used, 0, 0, &mut best);
    best
}

fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut state = seed;
    for i in (1..items.len()).rev() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let j = (state >> 33) as usize % (i + 1);
        items.swap(i, j);
    }
}

/// The sticky resolver in `squads.rs` still keeps a slot when a member walks across.
/// The full file stays the lock; this is the same rule on a two-member ring.
#[test]
fn existing_squad_tests_unchanged() {
    let mut app = app();
    let foe = app
        .world_mut()
        .spawn((Transform::from_translation(Vec3::ZERO), Blackboard::new()))
        .id();
    let squad = app
        .world_mut()
        .spawn((
            AiSquad::new("alpha"),
            AiFormation {
                interval_steps: 1,
                ..AiFormation::new(
                    FormationAnchor::Entity(foe),
                    FormationShape::Ring {
                        radius: 4.0,
                        spacing: 4.0,
                    },
                )
            },
        ))
        .id();
    let spawn = |app: &mut App, at: Vec3| {
        app.world_mut()
            .spawn((
                Transform::from_translation(at),
                Blackboard::new(),
                AiSquadMember::new(squad),
            ))
            .id()
    };
    let east = spawn(&mut app, Vec3::new(10.0, 0.0, 0.0));
    let west = spawn(&mut app, Vec3::new(-10.0, 0.0, 0.0));
    step(&mut app, 1);
    let index_of = |app: &App, member: Entity| {
        app.world()
            .get::<AiFormation>(squad)
            .expect("formation")
            .slots
            .iter()
            .find(|row| row.member == member)
            .map(|row| row.index)
    };
    let first_east = index_of(&app, east).expect("east placed");
    let first_west = index_of(&app, west).expect("west placed");
    app.world_mut()
        .get_mut::<Transform>(east)
        .expect("transform")
        .translation = Vec3::new(-20.0, 0.0, 0.0);
    step(&mut app, 3);
    assert_eq!(index_of(&app, east), Some(first_east));
    assert_eq!(index_of(&app, west), Some(first_west));
    assert!(
        app.world()
            .get::<AiFormation>(squad)
            .expect("formation")
            .custom
            .is_none()
    );
    let _ = resolve_formation;
}

#[test]
fn assignment_is_minimal_and_deterministic() {
    for n in 1..=8 {
        let members: Vec<AssignMember> = (0..n)
            .map(|i| member(i as u64 + 1, (i * 3) as f32, ((i * 5) % 7) as f32, None))
            .collect();
        let slots: Vec<AssignSlot> = (0..n)
            .map(|i| slot(i as u32, ((i * 2) % 5) as f32, (i * 4) as f32, None))
            .collect();
        let assigned = assign_slots(&members, &slots);
        assert_eq!(assigned.len(), n, "n={n}");
        let got = total_um(&members, &slots, &assigned);
        let best = brute_min_um(&members, &slots);
        assert_eq!(got, best, "n={n} assignment is not a minimum");
        let mut shuffled = members.clone();
        shuffle(&mut shuffled, 0xA11C_E000 + n as u64);
        let again = assign_slots(&shuffled, &slots);
        assert_eq!(assigned, again, "n={n} order changed the assignment");
    }
}

#[test]
fn roles_take_only_their_slots() {
    let members = vec![
        member(1, 0.0, 0.0, Some("tank")),
        member(2, 0.1, 0.0, Some("flank")),
    ];
    let slots = vec![slot(0, 0.0, 0.0, Some("flank")), slot(1, 8.0, 0.0, Some("tank"))];
    let assigned = assign_slots(&members, &slots);
    assert_eq!(
        assigned,
        vec![
            SlotAssignment { member: 1, slot: 1 },
            SlotAssignment { member: 2, slot: 0 },
        ]
    );
    let blocked = vec![slot(0, 0.0, 0.0, Some("scout"))];
    assert!(assign_slots(&members, &blocked).is_empty());
}

#[test]
fn formation_ron_round_trips_and_bad_assets_are_refused() {
    let text = r#"(
        format_version: 1,
        name: "wedge-custom",
        spacing_scale: 1.0,
        fill_order: ["left", "right"],
        slots: [
            (name: "left", offset: (x: -1.0, y: 0.0, z: 2.0), role: Some("flanker")),
            (name: "right", offset: (x: 1.0, y: 0.0, z: 2.0), role: None),
        ],
    )"#;
    let file = FormationFile::from_ron(text).expect("parses");
    let canonical = file.to_ron();
    let dir = unique_temp_dir("formation-ron");
    let path = dir.join("shape.formation.ron");
    let tmp = dir.join("shape.formation.ron.tmp");
    std::fs::write(&tmp, &canonical).expect("temp write");
    std::fs::rename(&tmp, &path).expect("rename");
    let loaded = std::fs::read_to_string(&path).expect("read");
    let again = FormationFile::from_ron(&loaded).expect("reloaded");
    assert_eq!(again.to_ron(), canonical);
    let asset = again.to_asset().expect("asset");
    assert_eq!(asset.slots.len(), 2);
    assert_eq!(asset.slots[0].role.as_deref(), Some("flanker"));

    let nan = text.replace("x: -1.0", "x: NaN");
    assert_eq!(
        FormationFile::from_ron(&nan).expect_err("refused").reason(),
        "non_finite"
    );
    let dup = text.replace("name: \"right\"", "name: \"left\"");
    assert_eq!(
        FormationFile::from_ron(&dup).expect_err("refused").reason(),
        "duplicate_slot"
    );
    let future = text.replace("format_version: 1", "format_version: 9");
    let err = FormationFile::from_ron(&future).expect_err("refused");
    assert_eq!(err.reason(), "unsupported_version");
    assert!(err.to_string().to_ascii_lowercase().contains("version"));
    assert!(FormationAsset::preset("ring").is_some());
    assert!(FormationAsset::preset("no-such-shape").is_none());
}
