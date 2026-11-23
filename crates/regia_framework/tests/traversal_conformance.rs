// SPDX-License-Identifier: MPL-2.0
//! Spec conformance evidence for `docs/specs/traversal.md` (ADR-2971, audit ADR-5664): the rules the
//! main suites (`traversal.rs`, `traversal_nav.rs`, `traversal_net.rs`) exercise only on the way to
//! something else, pinned one by one. Rapier backend, fixed steps, no wall clock.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_framework::character::{
    Character, CharacterInput, CharacterMovement, CharacterPlugin, CharacterState, MovementMode,
    use_rapier_character_controller,
};
use regia_framework::traversal::{
    CharacterTraversal, FlatWater, Ladder, TraversalEvent, TraversalJumpPoint, TraversalKind,
    TraversalPlugin, TraversalRefusal, TraversalSettings, TraversalWater, probe_traversal,
};
use regia_physics::{CharacterBody, Collider, ColliderShape, RigidBody};
use regia_scene::Transform;

const DT: f64 = 1.0 / 62.5;
/// Resting capsule-center height of the default capsule (radius 0.3, half height 0.9) on y = 0.
const REST_Y: f32 = 1.2;

fn base_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(DT)));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed)
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins(CharacterPlugin)
        .add_plugins(TraversalPlugin);
    use_rapier_character_controller(&mut app);
    app
}

fn static_box(app: &mut App, half: Vec3, translation: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(translation),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
        ))
        .id()
}

fn spawn_character(app: &mut App, start: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(start),
            Character,
            CharacterMovement::default(),
            CharacterInput::default(),
            CharacterState {
                grounded: true,
                ..CharacterState::default()
            },
            CharacterBody::default(),
            TraversalSettings::default(),
        ))
        .id()
}

fn set_input(app: &mut App, e: Entity, f: impl FnOnce(&mut CharacterInput)) {
    f(&mut app.world_mut().get_mut::<CharacterInput>(e).expect("input"));
}

fn position(app: &App, e: Entity) -> Vec3 {
    app.world().get::<Transform>(e).expect("transform").translation
}

fn traversal(app: &App, e: Entity) -> CharacterTraversal {
    *app.world().get::<CharacterTraversal>(e).expect("traversal")
}

fn run(app: &mut App, steps: usize) {
    for _ in 0..steps {
        app.update();
    }
}

fn run_until(app: &mut App, steps: usize, mut done: impl FnMut(&App) -> bool) -> bool {
    for _ in 0..steps {
        app.update();
        if done(app) {
            return true;
        }
    }
    false
}

fn events(app: &App) -> Vec<TraversalEvent> {
    let messages = app.world().resource::<Messages<TraversalEvent>>();
    let mut cursor = messages.get_cursor();
    cursor.read(messages).cloned().collect()
}

/// The repo root, from this crate's manifest directory.
fn repo_file(path: &str) -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|error| panic!("{path}: {error}"))
}

// ---- § 6: refusal codes are documented where the spec says ------------------------------------------

/// The parameter-error codes of `Ladder::validate` and `TraversalJumpPoint::validate` (§ 6).
const PARAM_CODES: [&str; 2] = ["ladder_bad_params", "jump_point_bad_params"];

/// Every backticked word in the first cell of the § 6 code table.
fn spec_table_codes(spec: &str) -> Vec<String> {
    let section = spec
        .split("## 6.")
        .nth(1)
        .and_then(|rest| rest.split("## 7.").next())
        .expect("spec § 6");
    let mut codes = Vec::new();
    for line in section.lines() {
        let Some(row) = line.strip_prefix("| `") else {
            continue;
        };
        let first_cell = row.split(" |").next().unwrap_or_default();
        for (i, part) in format!("`{first_cell}").split('`').enumerate() {
            if i % 2 == 1 && !part.is_empty() {
                codes.push(part.to_string());
            }
        }
    }
    codes
}

#[test]
fn every_refusal_code_is_in_the_spec_table_and_the_agent_page() {
    let spec = repo_file("docs/specs/traversal.md");
    let page = repo_file("book/agents/character-traversal.md");
    let table = spec_table_codes(&spec);
    let known: Vec<&str> = TraversalRefusal::ALL
        .iter()
        .map(|r| r.code())
        .chain(PARAM_CODES)
        .collect();
    for code in &known {
        assert!(
            table.iter().any(|c| c == code),
            "`{code}` is missing from the spec § 6 table"
        );
        assert!(
            page.contains(&format!("`{code}`")),
            "`{code}` is missing from book/agents/character-traversal.md"
        );
    }
    for code in &table {
        assert!(
            known.contains(&code.as_str()),
            "the spec § 6 table lists `{code}`, which no refusal or validate returns"
        );
    }
    // The agent errors of § 9 are on the page too.
    for code in [
        "character_not_found",
        "traversal_not_enabled",
        "traversal_bad_param",
    ] {
        assert!(page.contains(&format!("`{code}`")), "{code}");
        assert!(spec.contains(&format!("`{code}`")), "{code}");
    }
}

// ---- § 2: the reserved modes ------------------------------------------------------------------------

#[test]
fn the_reserved_modes_are_engine_ids_without_gravity() {
    for mode in [MovementMode::CLIMBING, MovementMode::TRAVERSING] {
        let MovementMode::Custom(id) = mode else {
            panic!("{mode:?} is a Custom id");
        };
        assert!(id >= 0xFFFF_0000, "{id:#x} is in the engine's range");
        assert!(!mode.has_gravity(), "{mode:?} has no gravity");
    }
    assert_ne!(MovementMode::CLIMBING, MovementMode::TRAVERSING);
    // `Swimming` keeps its old meaning for a character without settings.
    assert!(MovementMode::Swimming.has_gravity());
}

// ---- § 6: parameter errors, field by field ----------------------------------------------------------

type Setter = fn(&mut TraversalSettings, f32);

/// Every number `TraversalSettings::validate` checks, with the field name it reports.
const SETTINGS_FIELDS: [(&str, Setter); 34] = [
    ("swim.chest_offset", |s, v| s.swim.chest_offset = v),
    ("swim.speed", |s, v| s.swim.speed = v),
    ("swim.sprint_multiplier", |s, v| s.swim.sprint_multiplier = v),
    ("swim.dive_speed", |s, v| s.swim.dive_speed = v),
    ("swim.rise_speed", |s, v| s.swim.rise_speed = v),
    ("swim.float_rate", |s, v| s.swim.float_rate = v),
    ("swim.drag", |s, v| s.swim.drag = v),
    ("swim.max_dive_depth", |s, v| s.swim.max_dive_depth = v),
    ("swim.surface_band", |s, v| s.swim.surface_band = v),
    ("swim.exit_margin", |s, v| s.swim.exit_margin = v),
    ("swim.leave_margin", |s, v| s.swim.leave_margin = v),
    ("swim.jump_velocity", |s, v| s.swim.jump_velocity = v),
    ("ladder.attach_margin", |s, v| s.ladder.attach_margin = v),
    ("ladder.attach_reach", |s, v| s.ladder.attach_reach = v),
    ("ladder.intent", |s, v| s.ladder.intent = v),
    ("ladder.enter_time", |s, v| s.ladder.enter_time = v),
    ("ladder.exit_speed", |s, v| s.ladder.exit_speed = v),
    ("ladder.jump_off_speed", |s, v| s.ladder.jump_off_speed = v),
    ("ladder.jump_off_up", |s, v| s.ladder.jump_off_up = v),
    ("ladder.cooldown", |s, v| s.ladder.cooldown = v),
    ("mantle.min_height", |s, v| s.mantle.min_height = v),
    ("mantle.max_height", |s, v| s.mantle.max_height = v),
    ("mantle.reach", |s, v| s.mantle.reach = v),
    ("mantle.edge_inset", |s, v| s.mantle.edge_inset = v),
    ("mantle.max_slope", |s, v| s.mantle.max_slope = v),
    ("mantle.min_depth", |s, v| s.mantle.min_depth = v),
    ("mantle.stand_inset", |s, v| s.mantle.stand_inset = v),
    ("mantle.clearance", |s, v| s.mantle.clearance = v),
    ("mantle.speed", |s, v| s.mantle.speed = v),
    ("mantle.vault_max_height", |s, v| s.mantle.vault_max_height = v),
    ("mantle.vault_max_thickness", |s, v| {
        s.mantle.vault_max_thickness = v
    }),
    ("mantle.vault_max_drop", |s, v| s.mantle.vault_max_drop = v),
    ("mantle.vault_speed", |s, v| s.mantle.vault_speed = v),
    // Not a number: an empty slot is the 34th check.
    ("montage_slot", |s, _| s.montage_slot.clear()),
];

#[test]
fn every_settings_number_refuses_a_non_finite_value_by_name() {
    for (field, set) in SETTINGS_FIELDS {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut settings = TraversalSettings::default();
            set(&mut settings, bad);
            let error = settings
                .validate()
                .expect_err(&format!("{field} = {bad} is refused"));
            assert_eq!(
                (error.code(), error.field),
                ("traversal_bad_settings", field),
                "{field} = {bad}"
            );
        }
    }
}

#[test]
fn out_of_range_settings_name_their_field() {
    let cases: [(&str, Setter, f32); 7] = [
        ("swim.speed", |s, v| s.swim.speed = v, -1.0),
        ("swim.drag", |s, v| s.swim.drag = v, 0.0),
        ("ladder.intent", |s, v| s.ladder.intent = v, 1.5),
        ("ladder.exit_speed", |s, v| s.ladder.exit_speed = v, 0.0),
        ("mantle.max_height", |s, v| s.mantle.max_height = v, 0.4),
        // The corner rounding needs at least 4 cm of clearance (§ 5).
        ("mantle.clearance", |s, v| s.mantle.clearance = v, 0.03),
        ("mantle.speed", |s, v| s.mantle.speed = v, 0.0),
    ];
    for (field, set, value) in cases {
        let mut settings = TraversalSettings::default();
        set(&mut settings, value);
        let error = settings.validate().expect_err(field);
        assert_eq!((error.code(), error.field), ("traversal_bad_settings", field));
    }
    // The edges themselves are allowed.
    let mut settings = TraversalSettings::default();
    settings.ladder.intent = 1.0;
    settings.mantle.clearance = 0.04;
    settings.mantle.max_height = settings.mantle.min_height;
    assert_eq!(settings.validate(), Ok(()));
}

#[test]
fn every_ladder_and_jump_point_number_refuses_a_non_finite_value_by_name() {
    type LadderSetter = fn(&mut Ladder, f32);
    let ladder_fields: [(&str, LadderSetter); 5] = [
        ("half_width", |l, v| l.half_width = v),
        ("height", |l, v| l.height = v),
        ("climb_speed", |l, v| l.climb_speed = v),
        ("standoff", |l, v| l.standoff = v),
        ("exit_forward", |l, v| l.exit_forward = v),
    ];
    for (field, set) in ladder_fields {
        for bad in [f32::NAN, f32::INFINITY, -1.0] {
            let mut ladder = Ladder::default();
            set(&mut ladder, bad);
            let error = ladder.validate().expect_err(field);
            assert_eq!((error.code(), error.field), ("ladder_bad_params", field));
        }
    }
    type PointSetter = fn(&mut TraversalJumpPoint, f32);
    let point_fields: [(&str, PointSetter); 2] =
        [("to", |p, v| p.to.y = v), ("arc_height", |p, v| p.arc_height = v)];
    for (field, set) in point_fields {
        for bad in [f32::NAN, f32::INFINITY] {
            let mut point = TraversalJumpPoint::default();
            set(&mut point, bad);
            let error = point.validate().expect_err(field);
            assert_eq!((error.code(), error.field), ("jump_point_bad_params", field));
        }
    }
}

// ---- § 3: swim speed and blocked motion ---------------------------------------------------------------

/// Open water: a surface at y = 0 over a deep floor, the character at its float line.
fn open_water(app: &mut App) -> Entity {
    static_box(app, Vec3::new(60.0, 0.5, 60.0), Vec3::new(0.0, -20.5, 0.0));
    app.insert_resource(TraversalWater(Box::new(FlatWater {
        height: 0.0,
        min: [-60.0, -60.0],
        max: [60.0, 60.0],
    })));
    // Chest offset 0.5: the float line is y = -0.5.
    let c = spawn_character(app, Vec3::new(0.0, -0.5, 0.0));
    if let Some(mut state) = app.world_mut().get_mut::<CharacterState>(c) {
        state.grounded = false;
    }
    run(app, 5);
    assert_eq!(traversal(app, c).kind, TraversalKind::Swim, "in the water");
    c
}

/// The steady horizontal speed after the drag has settled (§ 3: `v += (target - v)(1 - exp(-drag dt))`).
fn steady_swim_speed(sprint: bool) -> f32 {
    let mut app = base_app();
    let c = open_water(&mut app);
    set_input(&mut app, c, |i| {
        i.move_dir = Vec3::X;
        i.sprint = sprint;
    });
    // exp(-6 x 2 s) is about 6e-6: the velocity is at its target.
    run(&mut app, 125);
    let x0 = position(&app, c).x;
    run(&mut app, 50);
    assert_eq!(traversal(&app, c).kind, TraversalKind::Swim);
    (position(&app, c).x - x0) / (50.0 * DT as f32)
}

#[test]
fn surface_swimming_runs_at_the_swim_speed_and_sprint_multiplies_it() {
    let swim = TraversalSettings::default().swim;
    let walk = steady_swim_speed(false);
    assert!((walk - swim.speed).abs() < 0.02, "swim speed {walk}");
    let sprint = steady_swim_speed(true);
    assert!(
        (sprint - swim.speed * swim.sprint_multiplier).abs() < 0.03,
        "sprint speed {sprint}"
    );
}

#[test]
fn a_swimmer_held_by_a_wall_reads_the_motion_it_achieved() {
    let mut app = base_app();
    // A wall at x = 3 from the deep floor to above the water.
    static_box(&mut app, Vec3::new(1.0, 12.0, 20.0), Vec3::new(4.0, -8.0, 0.0));
    let c = open_water(&mut app);
    set_input(&mut app, c, |i| i.move_dir = Vec3::X);
    run(&mut app, 200);
    let at = position(&app, c);
    assert!(at.x < 3.0 - 0.25, "held in front of the wall: {at}");
    assert_eq!(traversal(&app, c).kind, TraversalKind::Swim);
    // Pushing into the wall, the velocity is the motion achieved, not the 3 m/s target.
    let velocity = app.world().get::<CharacterState>(c).expect("state").velocity;
    assert!(velocity.x.abs() < 0.3, "blocked velocity {velocity}");
}

// ---- § 4 / § 10: ladder ties do not depend on entity order ---------------------------------------------

fn spawn_ladder(app: &mut App, x: f32, user_id: u64) -> Entity {
    // Identity rotation: climbed from +Z, facing -Z.
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
            Ladder {
                height: 3.0,
                user_id,
                ..Ladder::default()
            },
        ))
        .id()
}

/// Two ladders side by side, equally far from a character between them; which one it attaches to, by
/// probe and by a real attach.
fn tie(low_on_left: bool, low_first: bool) -> (u64, u64) {
    let mut app = base_app();
    static_box(&mut app, Vec3::new(40.0, 0.5, 40.0), Vec3::new(0.0, -0.5, 0.0));
    let (low_x, high_x) = if low_on_left { (-0.3, 0.3) } else { (0.3, -0.3) };
    if low_first {
        spawn_ladder(&mut app, low_x, 5);
        spawn_ladder(&mut app, high_x, 9);
    } else {
        spawn_ladder(&mut app, high_x, 9);
        spawn_ladder(&mut app, low_x, 5);
    }
    // In front of both climb lines (z = 0.4), 0.5 m from each.
    let c = spawn_character(&mut app, Vec3::new(0.0, REST_Y, 0.8));
    run(&mut app, 10);
    let probe = probe_traversal(app.world_mut(), c, None, None).expect("probe");
    let probed = probe.ladder.expect("both ladders are in reach").user_id;
    set_input(&mut app, c, |i| i.move_dir = Vec3::NEG_Z);
    assert!(
        run_until(&mut app, 120, |app| traversal(app, c).kind
            == TraversalKind::Ladder),
        "attached: {}",
        position(&app, c)
    );
    (probed, traversal(&app, c).ladder.id)
}

#[test]
fn equally_near_ladders_attach_by_user_id_whatever_the_entity_order() {
    for low_on_left in [true, false] {
        for low_first in [true, false] {
            assert_eq!(
                tie(low_on_left, low_first),
                (5, 5),
                "low id on the left: {low_on_left}, spawned first: {low_first}"
            );
        }
    }
}

// ---- § 5: a curve takes ceil(length / speed / dt) whole steps ------------------------------------------

#[test]
fn a_mantle_takes_the_whole_steps_its_path_length_asks_for() {
    let mut app = base_app();
    static_box(&mut app, Vec3::new(40.0, 0.5, 40.0), Vec3::new(0.0, -0.5, 0.0));
    // A block 1.5 m high, its face at x = 2.
    static_box(&mut app, Vec3::new(5.0, 0.75, 20.0), Vec3::new(7.0, 0.75, 0.0));
    let c = spawn_character(&mut app, Vec3::new(1.4, REST_Y, 0.0));
    if let Some(mut s) = app.world_mut().get_mut::<TraversalSettings>(c) {
        s.mantle.auto_in_air = false;
    }
    run(&mut app, 30);
    let probe = probe_traversal(app.world_mut(), c, None, Some(Vec3::X)).expect("probe");
    let ledge = probe.ledge.expect("a ledge ahead");
    assert_eq!(ledge.kind, TraversalKind::Mantle);
    let path = ledge.path;
    let length: f32 = (0..3).map(|i| path[i].distance(path[i + 1])).sum();
    let speed = TraversalSettings::default().mantle.speed;
    let dt = DT as f32;
    let expected = (length / speed / dt).ceil() as u32;
    assert!(expected > 1, "{expected}");

    set_input(&mut app, c, |i| {
        i.move_dir = Vec3::X;
        i.jump = true;
    });
    app.update();
    let t = traversal(&app, c);
    assert_eq!(t.kind, TraversalKind::Mantle);
    assert_eq!(t.path, path, "the step runs the path the probe found");
    assert_eq!(t.steps, expected, "ceil({length} / {speed} / {dt})");
    let duration = events(&app)
        .into_iter()
        .find_map(|event| match event {
            TraversalEvent::Started { duration, .. } => Some(duration),
            _ => None,
        })
        .expect("started");
    assert!(
        (duration - expected as f32 * dt).abs() < 1.0e-5,
        "duration {duration} = steps x dt"
    );
    // The first step ran with the press; the curve ends on its last whole step, not before or after.
    set_input(&mut app, c, |i| {
        i.move_dir = Vec3::ZERO;
        i.jump = false;
    });
    let mut more = 0;
    while traversal(&app, c).kind == TraversalKind::Mantle && more < 1000 {
        app.update();
        more += 1;
    }
    assert_eq!(more, expected - 1, "steps after the first");
    assert_eq!(
        app.world().get::<CharacterState>(c).expect("state").mode,
        MovementMode::Walking
    );
}
