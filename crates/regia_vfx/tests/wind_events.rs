// SPDX-License-Identifier: MPL-2.0
//! `.vfx.ron` v6 on the CPU reference path: the scene wind against its closed forms, and event
//! spawns' counts, draws and limits (ADR-2642).

use regia_core::math::Vec3;
use regia_vfx::format::{
    EventSpawnDef, EventTrigger, RangeDef, SpawnMode, UpdateModule, VFX_FORMAT_VERSION, VfxSystemDef,
};
use regia_vfx::probe::{ProbeEventStats, ProbeSettings, probe_effect, probe_steps};

const DT: f32 = 1.0 / 60.0;

/// A system of one burst parent; `update` is its update module, `life` its lifetime.
fn parent(update: &str, count: u32, life: f32) -> String {
    format!(
        r#"(
    version: {VFX_FORMAT_VERSION},
    name: "t",
    system: (budget: 100000),
    emitters: [
        (
            name: "parent",
            spawn: (mode: Burst({count}), lifetime: (min: {life:?}, max: {life:?})),
            init: (
                velocity: (mode: Directional(dir: (0.0, 0.0, 1.0), speed: (min: 0.0, max: 0.0))),
                size: (min: 1.0, max: 1.0),
            ),
            update: ({update}),
            render: (mode: Billboard),
        ),
        (
            name: "child",
            spawn: (mode: Event, lifetime: (min: 1.0, max: 2.0)),
            init: (
                shape: Sphere(radius: (min: 0.1, max: 0.4)),
                velocity: (mode: Radial(speed: (min: 1.0, max: 3.0))),
                size: (min: 0.5, max: 1.5),
            ),
            render: (mode: Billboard),
        ),
    ],
)"#
    )
}

fn def(update: &str, count: u32, life: f32) -> VfxSystemDef {
    VfxSystemDef::from_ron(&parent(update, count, life)).expect("test system parses")
}

/// A one-particle system without events.
fn lone(update: UpdateModule, life: f32) -> VfxSystemDef {
    let mut d = def(r#"events: [(on: Death, child: "child", count: 1)]"#, 1, life);
    d.emitters.truncate(1);
    d.emitters[0].update = update;
    d
}

fn settings(wind: Vec3) -> ProbeSettings {
    ProbeSettings {
        dt: DT,
        ground: None,
        wind,
        ..ProbeSettings::default()
    }
}

fn run(d: &VfxSystemDef, wind: Vec3, steps: u32) -> (Vec<regia_vfx::probe::ProbeState>, ProbeEventStats) {
    probe_steps(d, Vec3::ZERO, Vec3::Z, settings(wind), steps)
}

/// With coupling `c` and still-air drag `k`, a constant wind's terminal velocity is
/// `(g + c w) / (k + c)`, so the drift over a window of the settled phase is that velocity times
/// its length, whatever the step.
#[test]
fn constant_wind_with_drag_reaches_the_analytic_terminal_drift() {
    let wind = Vec3::new(6.0, 0.0, -3.0);
    let gravity = Vec3::new(0.0, -2.0, 0.0);
    let (response, wind_drag, drag) = (0.5f32, 4.0f32, 1.0f32);
    let d = lone(
        UpdateModule {
            gravity: gravity.into(),
            drag,
            wind_response: response,
            wind_drag,
            ..UpdateModule::default()
        },
        1000.0,
    );
    let c = response * wind_drag;
    let terminal = (gravity + wind * c) / (drag + c);
    let settle = 1200; // 20 s against a time constant of a third of a second.
    let (first, _) = run(&d, wind, settle);
    let (second, _) = run(&d, wind, settle + 600);
    let v = first[0].velocity;
    assert!((v - terminal).length() < 1e-3, "velocity {v} terminal {terminal}");
    let drift = second[0].position - first[0].position;
    let expected = terminal * (600.0 * DT);
    assert!(
        (drift - expected).length() < 1e-1,
        "drift {drift} expected {expected}"
    );
}

/// The integrator is implicit Euler: `v_n - v_inf = (v_0 - v_inf) (1 + (k + c) dt)^-n` exactly, the
/// discrete closed form of the kernel's update, at any step along the way.
#[test]
fn the_wind_transient_follows_the_implicit_euler_closed_form() {
    let wind = Vec3::new(10.0, 0.0, 0.0);
    let d = lone(
        UpdateModule {
            wind_response: 1.0,
            wind_drag: 2.0,
            ..UpdateModule::default()
        },
        1000.0,
    );
    let denominator = 1.0 + 2.0 * DT;
    for n in [1u32, 5, 30, 120] {
        let (state, _) = run(&d, wind, n);
        let expected = wind * (1.0 - denominator.powi(-(n as i32)));
        let got = state[0].velocity;
        assert!((got - expected).length() < 2e-3, "step {n}: {got} vs {expected}");
    }
}

/// `wind_response` scales the coupling rate continuously: a low response picks the wind up slowly
/// and reaches the same speed in calm-air terms; 0 ignores the wind whatever `wind_drag` says.
#[test]
fn wind_response_scales_how_fast_the_wind_is_picked_up() {
    let speed_at = |response: f32, wind_drag: f32| {
        let d = lone(
            UpdateModule {
                wind_response: response,
                wind_drag,
                ..UpdateModule::default()
            },
            1000.0,
        );
        run(&d, Vec3::new(8.0, 0.0, 0.0), 60).0[0].velocity.x
    };
    let (quick, slow) = (speed_at(1.0, 2.0), speed_at(0.25, 2.0));
    assert!(quick > slow && slow > 0.0, "quick {quick}, slow {slow}");
    assert_eq!(speed_at(0.0, 2.0), 0.0);
    assert_eq!(speed_at(1.0, 0.0), 0.0, "no drag: no coupling to the air");
}

/// A v5 file (with the fields it could have) loads with no wind response and no events, and its
/// simulation does not move under any wind: bits equal to the same file in calm air, and to the
/// same body stamped v6.
#[test]
fn a_v5_file_migrates_and_simulates_bit_identically() {
    let v5 = r##"(
    version: 5,
    name: "dust",
    system: (budget: 2000),
    emitters: [(
        name: "e",
        spawn: (mode: Rate(120.0), lifetime: (min: 0.5, max: 1.5)),
        init: (
            shape: Sphere(radius: (min: 0.1, max: 0.6)),
            velocity: (mode: Planar(speed: (min: 1.0, max: 2.0), lift: (min: 0.1, max: 0.4))),
            size: (min: 0.2, max: 0.6),
            color: [(t: 0.0, color: "#c0a080")],
        ),
        update: (gravity: (0.0, -1.0, 0.0), drag: 0.5, kill_below: Some(-3.0),
                 depth_collision: Some((kill: false, offset: 0.1))),
        render: (mode: Billboard, sprite: Puff, blend: Alpha, softness: 1.0, lit: true),
    )],
)"##;
    let loaded = VfxSystemDef::from_ron(v5).expect("v5 loads");
    assert_eq!(loaded.version, VFX_FORMAT_VERSION);
    let stamped =
        VfxSystemDef::from_ron(&v5.replace("version: 5", &format!("version: {VFX_FORMAT_VERSION}")))
            .expect("the same body at the current version");
    assert_eq!(loaded, stamped, "the migrator changes nothing but the version");
    assert_eq!(loaded.emitters[0].update.wind_response, 0.0);
    assert!(!loaded.has_events());
    let bits = |states: &[regia_vfx::probe::ProbeState]| -> Vec<u32> {
        states
            .iter()
            .flat_map(|s| s.position.to_array().into_iter().chain(s.velocity.to_array()))
            .map(f32::to_bits)
            .collect()
    };
    let calm = probe_steps(&loaded, Vec3::ZERO, Vec3::Y, settings(Vec3::ZERO), 200).0;
    let windy = probe_steps(
        &loaded,
        Vec3::ZERO,
        Vec3::Y,
        settings(Vec3::new(30.0, 0.0, 9.0)),
        200,
    )
    .0;
    assert!(!calm.is_empty());
    assert_eq!(bits(&calm), bits(&windy), "response 0: the wind moves nothing");
    // Round trip: re-saved at v6 and reloaded, the same definition.
    let saved = loaded.to_ron().expect("save");
    assert!(
        saved.contains(&format!("version: {VFX_FORMAT_VERSION}")),
        "{saved}"
    );
    assert_eq!(VfxSystemDef::from_ron(&saved).expect("reload"), loaded);
}

/// Probability 1: every parent that dies spawns exactly `count` children, one frame later.
#[test]
fn on_death_children_equal_parents_killed_times_count() {
    let d = def(r#"events: [(on: Death, child: "child", count: 8)]"#, 250, 0.5);
    let steps = (0.5 / DT) as u32;
    let (before, stats) = run(&d, Vec3::ZERO, steps - 2);
    assert_eq!(before.len(), 250, "parents only");
    assert_eq!(stats, ProbeEventStats::default());
    let (after, stats) = run(&d, Vec3::ZERO, steps + 4);
    assert_eq!(stats.fired, 250);
    assert_eq!(stats.children_spawned, 250 * 8);
    assert_eq!((stats.overflow, stats.children_dropped), (0, 0));
    let children = after.iter().filter(|p| p.emitter == 1).count();
    assert_eq!(children, 2000);
    assert_eq!(after.len(), 2000, "the parents died");
}

/// Kills count as deaths: the `kill_below` plane fires on_death on the frame it kills.
#[test]
fn the_kill_plane_fires_on_death() {
    let d = def(
        r#"gravity: (0.0, -40.0, 0.0), kill_below: Some(-0.5), events: [(on: Death, child: "child", count: 2)]"#,
        40,
        30.0,
    );
    let (states, stats) = run(&d, Vec3::ZERO, 60);
    assert_eq!(stats.fired, 40);
    assert_eq!(stats.children_spawned, 80);
    let parents = states.iter().filter(|p| p.emitter == 0).count();
    assert_eq!(parents, 0, "all killed");
}

/// on_spawn fires once per particle on its first step, and a chain runs to its grandchildren.
#[test]
fn on_spawn_fires_once_and_chains_run_to_grandchildren() {
    let mut d = def(r#"events: [(on: Spawn, child: "child", count: 3)]"#, 10, 5.0);
    let mut grandchild = d.emitters[1].clone();
    grandchild.name = "grandchild".into();
    d.emitters[1].update.events = vec![EventSpawnDef {
        on: EventTrigger::Spawn,
        child: "grandchild".into(),
        count: 2,
        probability: 1.0,
        inherit_velocity: 0.0,
    }];
    d.emitters.push(grandchild);
    d.validate().expect("valid chain");
    let (states, stats) = run(&d, Vec3::ZERO, 30);
    let count = |e: usize| states.iter().filter(|p| p.emitter == e).count();
    assert_eq!((count(0), count(1), count(2)), (10, 30, 60));
    assert_eq!(stats.fired, 10 + 30);
    assert_eq!(stats.children_spawned, 30 + 60);
}

/// The draw for a firing is a hash of the particle and the step: a replay fires the same events,
/// a half probability fires about half of them, and each child's draws are identical again.
#[test]
fn seeded_probability_makes_counts_deterministic() {
    let d = def(
        r#"events: [(on: Death, child: "child", count: 2, probability: 0.5)]"#,
        2000,
        0.25,
    );
    let (a, stats_a) = run(&d, Vec3::ZERO, 30);
    let (b, stats_b) = run(&d, Vec3::ZERO, 30);
    assert_eq!(stats_a, stats_b);
    assert_eq!(a, b, "same seed, same children");
    assert_eq!(stats_a.fired, 2000);
    let spawned = stats_a.children_spawned;
    assert_eq!(spawned % 2, 0, "all of a link's count or none");
    assert!((900..=1100).contains(&(spawned / 2)), "{spawned} children");
    // Probability 0 never fires a child, though the events are counted.
    let never = def(
        r#"events: [(on: Death, child: "child", count: 2, probability: 0.0)]"#,
        100,
        0.25,
    );
    let (_, stats) = run(&never, Vec3::ZERO, 30);
    assert_eq!((stats.fired, stats.children_spawned), (100, 0));
}

/// Children start at the parent, inherit the asked fraction of its velocity on top of their own
/// sampled one, and keep to the child's ranges.
#[test]
fn children_start_at_the_parent_and_inherit_velocity() {
    let mut d = def(
        r#"events: [(on: Death, child: "child", count: 4, inherit_velocity: 0.5)]"#,
        5,
        0.5,
    );
    d.emitters[0].init.velocity.mode = regia_vfx::format::VelocityMode::Directional {
        dir: (0.0, 0.0, 1.0),
        spread: 0.0,
        speed: RangeDef { min: 10.0, max: 10.0 },
    };
    d.emitters[1].init.shape = regia_vfx::format::ShapeDef::Point;
    d.emitters[1].init.velocity.mode = regia_vfx::format::VelocityMode::Directional {
        dir: (0.0, 1.0, 0.0),
        spread: 0.0,
        speed: RangeDef { min: 1.0, max: 1.0 },
    };
    let steps = (0.5 / DT).ceil() as u32 + 3;
    let (states, stats) = run(&d, Vec3::ZERO, steps);
    assert_eq!(stats.children_spawned, 20);
    for child in states.iter().filter(|p| p.emitter == 1) {
        let v = child.velocity;
        assert!((v.z - 5.0).abs() < 1e-3, "half of the parent's +Z speed: {v}");
        assert!((v.length() - 26f32.sqrt()).abs() < 1e-3, "{v}");
        assert!((1.0..=2.01).contains(&child.life));
        assert!((child.position.z - 5.0).abs() < 0.5, "{}", child.position);
    }
}

/// A full event queue counts what it loses, and a wrapped child ring counts what it overwrites;
/// the counts are exact either way.
#[test]
fn overflow_is_counted_never_silently_lost() {
    let d = def(r#"events: [(on: Death, child: "child", count: 1)]"#, 500, 0.25);
    let small = ProbeSettings {
        event_capacity: 100,
        ..settings(Vec3::ZERO)
    };
    let (_, stats) = probe_steps(&d, Vec3::ZERO, Vec3::Z, small, 30);
    assert_eq!(stats.fired, 500);
    // Count-based keep-set (fixrev-22-k, Muse review of fix-vfx1-determinism finding 1): the
    // budget counts events, not ranks — the first 100 fired survive in slot order wherever
    // their slots sit, and the 400 past the budget are counted lost, never a function of
    // window position.
    assert_eq!(stats.overflow, 500 - 100);
    assert_eq!(stats.children_spawned, 100, "one child per kept event");

    let ring = ProbeSettings {
        child_capacity: 64,
        ..settings(Vec3::ZERO)
    };
    // 500 events of one child each in one frame: 64 fit the ring, the rest are refused.
    let (states, stats) = probe_steps(&d, Vec3::ZERO, Vec3::Z, ring, 30);
    assert_eq!(stats.overflow, 0);
    assert_eq!(stats.children_spawned, 64);
    assert_eq!(stats.children_dropped, 500 - 64);
    assert_eq!(states.iter().filter(|p| p.emitter == 1).count(), 64);
}

/// The probe's snapshots see event children under their own emitter.
#[test]
fn snapshots_report_event_children() {
    let d = def(r#"events: [(on: Death, child: "child", count: 3)]"#, 6, 0.25);
    let snaps = probe_effect(&d, Vec3::ZERO, Vec3::Z, settings(Vec3::ZERO), &[0.5]);
    assert_eq!(snaps[0].emitters[0].live, 0);
    assert_eq!(snaps[0].emitters[1].live, 18);
    assert_eq!(d.emitters[1].spawn.mode, SpawnMode::Event);
}

/// The worked example of `docs/formats/vfx-ron.md` loads and does what the page says: every shell
/// lands, fires its sparks on the collision and rolls for dust on the death.
#[test]
fn the_format_page_example_plays() {
    let page = include_str!("../../../docs/formats/vfx-ron.md");
    let start = page.find("```ron,vfx").expect("the example's fence") + "```ron,vfx".len();
    let end = start + page[start..].find("```").expect("the fence closes");
    let def = VfxSystemDef::from_ron(&page[start..end]).expect("the example parses");
    assert_eq!(def.name, "shell_impact");
    let run = ProbeSettings {
        ground: Some(0.0),
        ..settings(Vec3::ZERO)
    };
    let (_, stats) = probe_steps(&def, Vec3::new(0.0, 0.5, 0.0), Vec3::Z, run, 240);
    assert_eq!(stats.fired, 24, "12 shells: one collision and one death each");
    assert_eq!(
        stats.children_spawned % 3,
        0,
        "sparks come in twelves and dust in threes"
    );
    assert!(stats.children_spawned >= 12 * 12, "{stats:?}");
}
