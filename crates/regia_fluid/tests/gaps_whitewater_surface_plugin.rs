// SPDX-License-Identifier: MPL-2.0
//! Test-gap audit (lane testgaps-fluid-2d-xr): whitewater settings and invariants, the surface
//! mesh's topology, frames, and the ECS world and plugin.

use std::collections::HashMap;

use glam::Vec3;
use regia_app::prelude::*;
use regia_fluid::plugin::{FluidEntry, MAX_SIMS, step_running_fluids};
use regia_fluid::sim::{Emitter, EmitterKind, FluidParams, FluidSim, Shape};
use regia_fluid::whitewater::{MAX_WHITEWATER_PER_KIND, WhitewaterKind, WhitewaterParams};
use regia_fluid::{FluidCache, FluidPlugin, FluidWorld, PointKind, presets};

fn dam(adjust: impl FnOnce(&mut FluidParams)) -> FluidSim {
    let (lo, hi, dx) = presets::default_bounds("dam_break").expect("bounds");
    let mut p = FluidParams {
        bounds_min: lo,
        bounds_max: hi,
        cell_size: dx * 2.0,
        ..FluidParams::default()
    };
    adjust(&mut p);
    presets::build("dam_break", p).expect("dam")
}

/// Every whitewater range check fires at its limit and not inside it.
#[test]
fn whitewater_validate_boundaries() {
    let ok = |f: &dyn Fn(&mut WhitewaterParams)| {
        let mut p = WhitewaterParams::default();
        f(&mut p);
        p.validate()
    };
    assert!(ok(&|_| {}).is_ok());
    assert!(ok(&|p| p.max_foam = MAX_WHITEWATER_PER_KIND).is_ok());
    assert!(ok(&|p| p.max_foam = MAX_WHITEWATER_PER_KIND + 1).is_err());
    assert!(ok(&|p| p.max_spray = MAX_WHITEWATER_PER_KIND + 1).is_err());
    assert!(ok(&|p| p.max_bubbles = MAX_WHITEWATER_PER_KIND + 1).is_err());
    assert!(
        ok(&|p| p.trapped_air = [1.0, 1.0]).is_err(),
        "min must be below max"
    );
    assert!(ok(&|p| p.wave_crest = [-0.1, 1.0]).is_err());
    assert!(ok(&|p| p.kinetic = [0.0, f32::NAN]).is_err());
    assert!(ok(&|p| p.kinetic = [0.0, 1.0]).is_ok());
    assert!(ok(&|p| p.rate_trapped_air = 10_000.0).is_ok());
    assert!(ok(&|p| p.rate_trapped_air = 10_000.5).is_err());
    assert!(ok(&|p| p.rate_wave_crest = -1.0).is_err());
    assert!(ok(&|p| p.foam_lifetime = 0.0).is_err());
    assert!(ok(&|p| p.foam_lifetime = 600.0).is_ok());
    assert!(ok(&|p| p.foam_lifetime = 600.1).is_err());
    assert!(ok(&|p| p.buoyancy = 100.1).is_err());
    assert!(ok(&|p| p.drag = 1.1).is_err());
    assert!(ok(&|p| p.drag = f32::NAN).is_err());
    assert!(
        ok(&|p| p.spray_neighbours = 21).is_err(),
        "spray above bubble neighbours"
    );
    assert!(ok(&|p| p.spray_neighbours = 20).is_ok());
    // The simulator refuses a bad block through its own params.
    let Err(err) = FluidSim::new(FluidParams {
        whitewater: WhitewaterParams {
            drag: 5.0,
            ..WhitewaterParams::default()
        },
        ..FluidParams::default()
    }) else {
        panic!("drag 5.0 must be refused");
    };
    assert_eq!(err.code(), "fluid_bad_param");
}

/// Whitewater invariants over a dam break: counts match the particles, caps hold for all three
/// kinds at every step, particles stay in the domain, foam lifetimes stay within the setting, and
/// `generated_last_step` equals the growth that was not culled.
#[test]
fn whitewater_invariants_hold_every_step() {
    let caps = (7usize, 5usize, 3usize);
    let mut sim = dam(|p| {
        p.whitewater.max_foam = caps.0;
        p.whitewater.max_spray = caps.1;
        p.whitewater.max_bubbles = caps.2;
    });
    let lo = sim.dims().origin;
    let hi = sim.dims().max();
    let life = sim.params().whitewater.foam_lifetime;
    let mut generated = 0;
    for step in 0..100 {
        sim.step();
        let ww = sim.whitewater();
        let counts = ww.counts();
        let mut tally = [0usize; 4];
        for p in ww.particles() {
            tally[p.kind as usize] += 1;
            assert!(
                p.position.cmpge(lo).all() && p.position.cmple(hi).all(),
                "step {step}: {:?}",
                p.position
            );
            assert!(p.position.is_finite() && p.velocity.is_finite());
            assert!(p.age >= 0.0);
            assert!(
                p.lifetime > 0.0 && p.lifetime <= life,
                "step {step}: lifetime {}",
                p.lifetime
            );
        }
        assert_eq!(counts, tally, "step {step}: cached counts match the particles");
        assert_eq!(counts[0], 0, "no liquid-tagged whitewater");
        assert!(
            counts[1] <= caps.0 && counts[2] <= caps.1 && counts[3] <= caps.2,
            "step {step}: {counts:?}"
        );
        let s = sim.stats();
        assert_eq!((s.foam, s.spray, s.bubbles), (counts[1], counts[2], counts[3]));
        generated += s.whitewater_generated;
    }
    assert!(
        generated > 0,
        "the scene generated whitewater (the caps were exercised)"
    );
}

/// With whitewater disabled nothing is generated or published, and the frame holds liquid only.
#[test]
fn disabled_whitewater_makes_nothing() {
    let mut sim = dam(|p| p.whitewater.enabled = false);
    for _ in 0..60 {
        sim.step();
        assert_eq!(sim.stats().whitewater_generated, 0);
    }
    assert!(sim.whitewater().particles().is_empty());
    assert_eq!(sim.whitewater().counts(), [0; 4]);
    let f = sim.frame();
    assert_eq!(f.len(), sim.positions().len());
    assert_eq!(f.count(PointKind::Liquid), f.len());
    assert!(!sim.whitewater().params().enabled);
}

/// Whitewater is part of the state digest, and zero-rate settings generate nothing.
#[test]
fn zero_rates_generate_nothing_and_whitewater_is_in_the_digest() {
    let mut quiet = dam(|p| {
        p.whitewater.rate_trapped_air = 0.0;
        p.whitewater.rate_wave_crest = 0.0;
    });
    let mut loud = dam(|_| {});
    for _ in 0..60 {
        quiet.step();
        loud.step();
    }
    assert!(quiet.whitewater().particles().is_empty());
    assert!(!loud.whitewater().particles().is_empty());
    // Same liquid, since whitewater never feeds back: equal positions, different digest.
    assert_eq!(quiet.positions(), loud.positions());
    assert_ne!(quiet.state_digest(), loud.state_digest());
}

/// Tags map both ways and names are stable.
#[test]
fn kind_tags_and_names() {
    for (tag, name) in [(0u8, "liquid"), (1, "foam"), (2, "spray"), (3, "bubble")] {
        assert_eq!(
            PointKind::from_tag(tag).map(|k| (k as u8, k.name())),
            Some((tag, name))
        );
    }
    assert_eq!(PointKind::from_tag(4), None);
    assert_eq!(PointKind::from_tag(255), None);
    for (tag, k) in [
        (1u8, WhitewaterKind::Foam),
        (2, WhitewaterKind::Spray),
        (3, WhitewaterKind::Bubble),
    ] {
        assert_eq!(WhitewaterKind::from_tag(tag), Some(k));
        assert_eq!(PointKind::from(k) as u8, tag);
        assert_eq!(PointKind::from(k).name(), k.name());
    }
    assert_eq!(WhitewaterKind::from_tag(0), None);
    assert_eq!(WhitewaterKind::from_tag(4), None);
}

fn block(res_cells: f32, steps: usize) -> FluidSim {
    let mut sim = FluidSim::new(FluidParams {
        bounds_max: Vec3::splat(1.0),
        cell_size: 1.0 / res_cells,
        gravity: Vec3::ZERO,
        ..FluidParams::default()
    })
    .expect("sim");
    sim.add_emitter(Emitter {
        shape: Shape::Box {
            center: Vec3::splat(0.5),
            half_extents: Vec3::splat(0.25),
        },
        velocity: Vec3::ZERO,
        kind: EmitterKind::Fill,
        done: false,
    })
    .expect("emitter");
    for _ in 0..steps {
        sim.step();
    }
    sim
}

/// An empty liquid has no surface; a block's mesh is closed (every edge shared by two triangles),
/// indexes inside its vertices, has unit outward normals and a positive enclosed volume.
#[test]
fn surface_mesh_is_a_closed_outward_manifold() {
    let empty = FluidSim::new(FluidParams {
        bounds_max: Vec3::splat(1.0),
        cell_size: 0.125,
        ..FluidParams::default()
    })
    .expect("sim");
    let m = empty.surface_mesh(2);
    assert_eq!((m.triangle_count(), m.positions.len()), (0, 0));
    assert_eq!(m.enclosed_volume(), 0.0);

    let sim = block(8.0, 1);
    let mesh = sim.surface_mesh(2);
    assert!(mesh.triangle_count() > 100);
    assert_eq!(mesh.normals.len(), mesh.positions.len());
    assert_eq!(mesh.indices.len() % 3, 0);
    assert!(mesh.indices.iter().all(|i| (*i as usize) < mesh.positions.len()));
    let mut edges: HashMap<(u32, u32), u32> = HashMap::new();
    for t in mesh.indices.as_chunks::<3>().0 {
        assert!(
            t[0] != t[1] && t[1] != t[2] && t[0] != t[2],
            "degenerate triangle {t:?}"
        );
        for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
            *edges.entry((a.min(b), a.max(b))).or_default() += 1;
        }
    }
    let open = edges.values().filter(|c| **c != 2).count();
    assert_eq!(
        open,
        0,
        "{open} of {} edges are not shared by exactly two triangles",
        edges.len()
    );
    let centre = Vec3::splat(0.5);
    for (p, n) in mesh.positions.iter().zip(&mesh.normals) {
        let n = Vec3::from(*n);
        assert!((n.length() - 1.0).abs() < 1e-3, "normal length {}", n.length());
        assert!(
            n.dot((Vec3::from(*p) - centre).normalize()) > 0.0,
            "normal points into the block at {p:?}"
        );
    }
    let vol = mesh.enclosed_volume();
    assert!(vol > 0.0, "outward winding gives positive volume ({vol})");
    // A 0.5 m cube: the mesh is rounded, so within 35 %.
    assert!((vol - 0.125).abs() < 0.125 * 0.35, "{vol}");
}

/// The resolution parameter clamps to 1..=4, and a finer lattice gives more triangles.
#[test]
fn surface_resolution_clamps_and_refines() {
    let sim = block(6.0, 1);
    let at = |r: u32| sim.surface_mesh(r).triangle_count();
    assert_eq!(at(0), at(1), "0 clamps up to 1");
    assert_eq!(at(9), at(4), "above the maximum clamps down to 4");
    assert!(at(1) < at(2) && at(2) < at(3), "{} {} {}", at(1), at(2), at(3));
    let (phi, n, h) = sim.level_set(2);
    assert_eq!(n, [13, 13, 13]);
    assert!((h - 1.0 / 12.0).abs() < 1e-6);
    assert_eq!(phi.len(), 13 * 13 * 13);
    // Inside the block is negative, far air positive, and the closed walls are never inside.
    assert!(phi[6 + 13 * (6 + 13 * 6)] < 0.0);
    assert!(phi[0] > 0.0 && phi[phi.len() - 1] > 0.0);
}

/// Ids are never reused, `get`/`get_mut` find by id, entries keep creation order, and
/// `FluidEntry::shown_frame` picks the live state, a cache frame, or the live state again when
/// the index is past the bake (G-fluid #12). An empty frame looked like an empty recording.
#[test]
fn world_ids_and_shown_frames() {
    let mut w = FluidWorld::default();
    let a = w.insert(block(4.0, 1), Some("block".into())).expect("a");
    let b = w.insert(block(4.0, 1), None).expect("b");
    assert_ne!(a, b);
    assert!(w.remove(a));
    let c = w.insert(block(4.0, 1), None).expect("c");
    assert!(
        c != a && c != b,
        "a removed id is not handed out again ({a} {b} {c})"
    );
    assert_eq!(w.entries().iter().map(|e| e.id).collect::<Vec<_>>(), vec![b, c]);
    assert!(w.get(a).is_none());
    assert_eq!(w.get(b).and_then(|e| e.preset.clone()), None);
    assert_eq!(
        w.get(b).map(|e| (e.running, e.surface_resolution, e.revision)),
        Some((false, 2, 0))
    );
    for _ in 0..MAX_SIMS {
        if w.insert(block(4.0, 0), None).is_none() {
            break;
        }
    }
    assert_eq!(w.entries().len(), MAX_SIMS);

    let entry: &mut FluidEntry = w.get_mut(b).expect("b");
    let live = entry.sim.frame();
    assert_eq!(entry.shown_frame(), live, "no cache: the live state");
    entry.playing = Some(0);
    assert_eq!(
        entry.shown_frame(),
        live,
        "playing without a cache falls back to live"
    );
    let mut cache = FluidCache::for_sim(&entry.sim, false);
    cache.record(&entry.sim).expect("record");
    entry.sim.step();
    entry.sim.step();
    entry.cache = Some(cache);
    let shown = entry.shown_frame();
    assert_eq!(
        shown.step,
        1,
        "the cache frame, not the live state (step {})",
        entry.sim.steps()
    );
    entry.playing = Some(5);
    // The index is past the one recorded frame. `FluidCache::play` returns `fluidcache_no_frame`;
    // the shown frame stays the live state (same contract as `playback_past_the_end_keeps_a_frame`).
    assert_eq!(
        entry.shown_frame(),
        entry.sim.frame(),
        "a frame past the end shows the live state"
    );
    entry.playing = None;
    assert_eq!(entry.shown_frame().step, entry.sim.steps());
}

/// `FluidPlugin` installs the world and steps running entries on `FixedUpdate`; a paused entry and a
/// playing entry do not move, and a plugin-free app has no `FluidWorld`.
#[test]
fn plugin_registers_the_world_and_steps_on_fixed_update() {
    let bare = App::new();
    assert!(
        bare.world().get_resource::<FluidWorld>().is_none(),
        "nothing is installed by default"
    );

    let mut app = App::new();
    app.add_plugins(FluidPlugin);
    let (run, pause) = {
        let mut w = app.world_mut().resource_mut::<FluidWorld>();
        let run = w.insert(block(4.0, 0), None).expect("run");
        let pause = w.insert(block(4.0, 0), None).expect("pause");
        w.get_mut(run).expect("entry").running = true;
        (run, pause)
    };
    for _ in 0..4 {
        app.world_mut().run_schedule(FixedUpdate);
    }
    let w = app.world().resource::<FluidWorld>();
    assert_eq!(w.get(run).map(|e| (e.sim.steps(), e.revision)), Some((4, 4)));
    assert_eq!(w.get(pause).map(|e| (e.sim.steps(), e.revision)), Some((0, 0)));
    // The system is also public: calling it with nothing running changes nothing.
    let mut world = World::new();
    world.insert_resource(FluidWorld::default());
    use regia_ecs::ecs::system::RunSystemOnce;
    world.run_system_once(step_running_fluids).expect("system");
    assert!(world.resource::<FluidWorld>().entries().is_empty());
}
