// SPDX-License-Identifier: MPL-2.0
//! Counting tests for the per-step scratch of the CPU solver. No wall clock.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use glam::Vec3;
use regia_fluid::sim::{Emitter, EmitterKind, FluidParams, FluidSim, Shape};
use regia_fluid::solid::Collider;
use regia_fluid::whitewater::WhitewaterParams;

/// Process-global span interest. Sibling tests must not be inside `step` while this file's
/// subscriber rebuilds it, or a callsite gets cached as `never`.
fn lock_steps() -> std::sync::MutexGuard<'static, ()> {
    static STEPS: Mutex<()> = Mutex::new(());
    STEPS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

fn quiet_pool() -> FluidParams {
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: Vec3::new(0.4, 0.4, 0.2),
        cell_size: 0.1,
        gravity: Vec3::ZERO,
        reseed: false,
        separation: 0.0,
        max_substeps: 1,
        particle_cap: 65_536,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    params
}

fn rest_particles(n: usize) -> (Vec<Vec3>, Vec<Vec3>) {
    let mut pos = Vec::with_capacity(n);
    let mut vel = Vec::with_capacity(n);
    for i in 0..n {
        let t = (i as f32 + 0.5) / n as f32;
        pos.push(Vec3::new(0.1 + 0.2 * t, 0.15, 0.1));
        vel.push(Vec3::ZERO);
    }
    (pos, vel)
}

/// Two steps reuse the velocity scratch allocated at construction.
#[test]
fn step_reuses_velocity_scratch() {
    let _hold = lock_steps();
    let mut sim = FluidSim::new(quiet_pool()).expect("sim");
    let (pos, vel) = rest_particles(8);
    sim.set_particles(pos, vel);
    let before = sim.velocity_scratch_grows();
    sim.step();
    sim.step();
    let after = sim.velocity_scratch_grows();
    eprintln!("velocity scratch grows {before} -> {after}");
    assert_eq!(after, before, "a step allocated velocity scratch");
}

/// Two projections reuse the Poisson matrix and the CG workspace.
#[test]
fn project_reuses_poisson_workspace() {
    let _hold = lock_steps();
    let mut sim = FluidSim::new(quiet_pool()).expect("sim");
    let (pos, vel) = rest_particles(8);
    sim.set_particles(pos, vel);
    let before = sim.poisson_workspace_grows();
    sim.step();
    sim.step();
    let after = sim.poisson_workspace_grows();
    eprintln!("poisson workspace grows {before} -> {after}");
    assert_eq!(after, before, "a step allocated the pressure workspace");
}

/// A source, separation and whitewater all bin the particles. The second step does not grow
/// the bin scratch.
#[test]
fn bin_scratch_capacity_is_stable() {
    let mut params = quiet_pool();
    params.separation = 0.5;
    params.whitewater = WhitewaterParams {
        enabled: true,
        ..WhitewaterParams::default()
    };
    let _hold = lock_steps();
    let mut sim = FluidSim::new(params).expect("sim");
    sim.add_emitter(Emitter {
        shape: Shape::Box {
            center: Vec3::new(0.2, 0.1, 0.1),
            half_extents: Vec3::new(0.15, 0.08, 0.08),
        },
        velocity: Vec3::new(0.0, 0.2, 0.0),
        kind: EmitterKind::Source,
        done: false,
    })
    .expect("source");
    sim.step();
    let grows = sim.bin_scratch_grows();
    let cap = sim.bin_scratch_capacity();
    eprintln!("bin scratch after first step: grows {grows}, capacity {cap:?}");
    assert!(cap.0 > 0 && cap.1 > 0 && cap.2 > 0, "bin scratch was not stored");
    sim.step();
    eprintln!(
        "bin scratch after second step: grows {}, capacity {:?}",
        sim.bin_scratch_grows(),
        sim.bin_scratch_capacity()
    );
    assert_eq!(sim.bin_scratch_grows(), grows, "the second step allocated bins");
    assert_eq!(sim.bin_scratch_capacity(), cap);
}

/// Still liquid makes no whitewater, and the normal pass does not walk neighbours.
#[test]
fn settled_pool_skips_whitewater_normals() {
    let _hold = lock_steps();
    let mut params = quiet_pool();
    params.whitewater.enabled = true;
    let mut sim = FluidSim::new(params).expect("sim");
    let (pos, vel) = rest_particles(16);
    sim.set_particles(pos, vel);
    let before = sim.whitewater().normal_neighbor_visits();
    sim.step();
    let visits = sim.whitewater().normal_neighbor_visits();
    eprintln!(
        "settled pool normal visits {before} -> {visits}, generated {}",
        sim.whitewater().generated_last_step()
    );
    assert_eq!(sim.whitewater().generated_last_step(), 0);
    assert_eq!(visits, before, "normals ran for liquid that cannot emit");
}

/// A finite step with no sink does not allocate a keep-mask.
#[test]
fn finite_step_does_not_allocate_a_drop_mask() {
    let _hold = lock_steps();
    let mut sim = FluidSim::new(quiet_pool()).expect("sim");
    let (pos, vel) = rest_particles(8);
    sim.set_particles(pos, vel);
    let before = sim.drop_mask_allocs();
    sim.step();
    let after = sim.drop_mask_allocs();
    eprintln!("drop-mask allocs {before} -> {after}");
    assert_eq!(after, before, "a finite step allocated a keep-mask");
}

struct SpanNames {
    next: AtomicU64,
    names: Mutex<Vec<&'static str>>,
}

impl Subscriber for SpanNames {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attrs: &Attributes<'_>) -> Id {
        self.names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(attrs.metadata().name());
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        Id::from_u64(id)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, _event: &Event<'_>) {}

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

/// The extrapolation and the boundary push have their own spans.
#[test]
fn step_emits_extrapolate_and_boundary_spans() {
    let names = std::sync::Arc::new(SpanNames {
        next: AtomicU64::new(0),
        names: Mutex::new(Vec::new()),
    });
    let _hold = lock_steps();
    let subscriber = SharedSubscriber(std::sync::Arc::clone(&names));
    tracing::subscriber::with_default(subscriber, || {
        // A sibling test may already have executed these callsites under the default
        // subscriber, which caches them as never for the whole process.
        tracing::callsite::rebuild_interest_cache();
        let mut sim = FluidSim::with_colliders(
            quiet_pool(),
            vec![Collider::Sphere {
                center: Vec3::new(0.2, 0.05, 0.1),
                radius: 0.04,
            }],
        )
        .expect("sim");
        let (pos, vel) = rest_particles(4);
        sim.set_particles(pos, vel);
        sim.step();
    });
    let seen = names.names.lock().unwrap_or_else(|e| e.into_inner()).clone();
    eprintln!("step spans: {seen:?}");
    assert!(
        seen.contains(&"fluid_extrapolate"),
        "missing fluid_extrapolate in {seen:?}"
    );
    assert!(
        seen.contains(&"fluid_boundaries"),
        "missing fluid_boundaries in {seen:?}"
    );
}

struct SharedSubscriber(std::sync::Arc<SpanNames>);

impl Subscriber for SharedSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.0.enabled(metadata)
    }

    fn new_span(&self, attrs: &Attributes<'_>) -> Id {
        self.0.new_span(attrs)
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        self.0.record(span, values);
    }

    fn record_follows_from(&self, span: &Id, follows: &Id) {
        self.0.record_follows_from(span, follows);
    }

    fn event(&self, event: &Event<'_>) {
        self.0.event(event);
    }

    fn enter(&self, span: &Id) {
        self.0.enter(span);
    }

    fn exit(&self, span: &Id) {
        self.0.exit(span);
    }
}
