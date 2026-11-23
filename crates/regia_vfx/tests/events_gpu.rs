// SPDX-License-Identifier: MPL-2.0
//! `.vfx.ron` v6 on the GPU: the compute path against the CPU reference on a headless adapter
//! (lavapipe in the gate), the bounded event buffer and the wind (ADR-2642).

use regia_core::math::Vec3;
use regia_render::RenderDevice;
use regia_vfx::format::{VFX_FORMAT_VERSION, VfxSystemDef};
use regia_vfx::gpu::VfxPoolStats;
use regia_vfx::probe::{ProbeSettings, ProbeState, probe_steps};
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::sim::LodInput;
use regia_vfx::{PoolParticle, VfxCamera, VfxGpu, VfxProfile};

const DT: f32 = 1.0 / 64.0;

/// GPU against CPU, metres and m/s: the same arithmetic, except that a software rasterizer's
/// `pow` and `cos` are approximations.
const TOLERANCE: f32 = 5e-3;

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn profile() -> VfxProfile {
    VfxProfile {
        particle_capacity: 8192,
        event_capacity: 4096,
        child_capacity: 8192,
        ..VfxProfile::desktop_high()
    }
}

/// One system played on one context, a frame at a time.
struct Rig {
    device: RenderDevice,
    gpu: VfxGpu,
    scene: VfxScene,
    camera: VfxCamera,
    origin: Vec3,
    /// First emitter record of the instance.
    base: u32,
}

impl Rig {
    fn new(def: VfxSystemDef, profile: VfxProfile, origin: Vec3, forward: Vec3, wind: Vec3) -> Self {
        let device = device();
        let gpu = VfxGpu::new(&device, profile).expect("vfx gpu");
        // A fresh allocator hands out the top of the record table.
        let base = regia_vfx::gpu::layout::MAX_EMITTERS - def.emitters.len() as u32;
        let mut library = VfxLibrary::new();
        let name = def.name.clone();
        library.add(def).expect("valid def");
        let mut scene = VfxScene::new(library);
        scene.set_wind(wind);
        scene.spawn(&name, origin, forward, 7).expect("spawned");
        Self {
            device,
            gpu,
            scene,
            camera: VfxCamera::looking_at(Vec3::new(0.0, 40.0, -60.0), Vec3::ZERO),
            origin,
            base,
        }
    }

    fn frames(&mut self, n: u32, dt: f32) {
        for _ in 0..n {
            self.scene.update(
                dt,
                &LodInput {
                    camera_pos: self.origin,
                },
                self.gpu.capacity(),
            );
            self.scene.upload(&mut self.gpu);
            self.gpu.set_frame(&self.camera, dt, 64, 64);
            let mut encoder = self
                .device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            self.gpu.simulate(&mut encoder);
            self.gpu.record_stats_copy(&mut encoder);
            self.device.queue.submit([encoder.finish()]);
        }
    }

    fn stats(&self) -> VfxPoolStats {
        self.gpu.read_pool_stats().expect("stats")
    }

    fn particles(&self) -> Vec<PoolParticle> {
        self.gpu.read_particles()
    }
}

fn system(update: &str, count: u32, life: f32) -> VfxSystemDef {
    VfxSystemDef::from_ron(&format!(
        r#"(
    version: {VFX_FORMAT_VERSION},
    name: "t",
    system: (budget: 100000),
    emitters: [
        (
            name: "parent",
            spawn: (mode: Burst({count}), lifetime: (min: {life:?}, max: {life:?})),
            init: (
                shape: Sphere(radius: (min: 0.5, max: 1.0)),
                velocity: (mode: Radial(speed: (min: 0.0, max: 2.0))),
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
                velocity: (mode: Directional(dir: (0.0, 1.0, 0.0), spread: 30.0, speed: (min: 1.0, max: 3.0))),
                size: (min: 0.5, max: 1.5),
            ),
            update: (gravity: (0.0, -2.0, 0.0), drag: 0.3),
            render: (mode: Billboard),
        ),
    ],
)"#
    ))
    .expect("test system parses")
}

fn cpu_settings(wind: Vec3) -> ProbeSettings {
    ProbeSettings {
        dt: DT,
        seed: 7,
        ground: None,
        wind,
        event_capacity: profile().event_capacity,
        child_capacity: profile().child_capacity,
        ..ProbeSettings::default()
    }
}

/// `(emitter, x, y, z, vx, vy, vz)` rows of a GPU pool (`base` is the instance's first record).
fn gpu_rows(particles: &[PoolParticle], base: u32) -> Vec<[f32; 7]> {
    particles
        .iter()
        .map(|p| {
            [
                (p.emitter_slot - base) as f32,
                p.position[0],
                p.position[1],
                p.position[2],
                p.velocity[0],
                p.velocity[1],
                p.velocity[2],
            ]
        })
        .collect()
}

fn cpu_rows(states: &[ProbeState]) -> Vec<[f32; 7]> {
    states
        .iter()
        .map(|p| {
            [
                p.emitter as f32,
                p.position.x,
                p.position.y,
                p.position.z,
                p.velocity.x,
                p.velocity.y,
                p.velocity.z,
            ]
        })
        .collect()
}

/// Every GPU particle has its own CPU particle of the same emitter within `tolerance` on every
/// field. Nearest-match, because thread order picks a child's slot (and rounding can reorder
/// any sort of two near-equal positions).
fn assert_pools_match(gpu: &[[f32; 7]], cpu: &[[f32; 7]], tolerance: f32) {
    assert_eq!(gpu.len(), cpu.len(), "live particle counts differ");
    let distance = |a: &[f32; 7], b: &[f32; 7]| {
        if a[0] != b[0] {
            return f32::INFINITY;
        }
        a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
    };
    let mut unmatched: Vec<[f32; 7]> = cpu.to_vec();
    for row in gpu {
        let nearest = unmatched
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| distance(row, a).total_cmp(&distance(row, b)));
        let Some((index, other)) = nearest else {
            panic!("no CPU particle left for {row:?}");
        };
        let d = distance(row, other);
        assert!(d <= tolerance, "gpu {row:?} nearest cpu {other:?} is {d} away");
        unmatched.swap_remove(index);
    }
}

/// The GPU pool after N frames holds what the CPU reference holds after N steps: the same
/// counts, positions and velocities, children and wind included.
#[test]
fn the_gpu_path_matches_the_cpu_reference() {
    let wind = Vec3::new(4.0, 0.0, -2.0);
    let def = system(
        r#"gravity: (0.0, 1.0, 0.0), drag: 0.2, wind_response: 0.8, wind_drag: 1.5,
           events: [(on: Death, child: "child", count: 5, inherit_velocity: 0.5)]"#,
        300,
        0.5,
    );
    let origin = Vec3::new(2.0, 3.0, 1.0);
    let forward = Vec3::Y;
    let mut rig = Rig::new(def.clone(), profile(), origin, forward, wind);
    let mut done = 0;
    for steps in [16u32, 34, 50] {
        rig.frames(steps - done, DT);
        done = steps;
        let (cpu, cpu_stats) = probe_steps(&def, origin, forward, cpu_settings(wind), steps);
        assert_pools_match(&gpu_rows(&rig.particles(), rig.base), &cpu_rows(&cpu), TOLERANCE);
        let stats = rig.stats();
        assert_eq!(stats.events_fired, cpu_stats.fired, "events at step {steps}");
        assert_eq!(stats.children_spawned, cpu_stats.children_spawned);
        assert_eq!(stats.events_overflow, 0);
        assert_eq!(stats.children_dropped, 0);
    }
    assert_eq!(rig.stats().children_spawned, 300 * 5);
}

/// on_death at probability 1: children on the GPU equal parents killed times count.
#[test]
fn on_death_children_equal_parents_killed_times_count() {
    let def = system(r#"events: [(on: Death, child: "child", count: 8)]"#, 200, 0.25);
    let mut rig = Rig::new(def, profile(), Vec3::ZERO, Vec3::Y, Vec3::ZERO);
    rig.frames(30, DT);
    let stats = rig.stats();
    assert_eq!(stats.events_fired, 200);
    assert_eq!(stats.children_spawned, 1600);
    assert_eq!(stats.alive, 1600);
    assert_eq!(stats.events_overflow + stats.children_dropped, 0);
    // A second context plays the same counts.
    let again = system(r#"events: [(on: Death, child: "child", count: 8)]"#, 200, 0.25);
    let mut second = Rig::new(again, profile(), Vec3::ZERO, Vec3::Y, Vec3::ZERO);
    second.frames(30, DT);
    assert_eq!(second.stats(), stats);
}

/// Seeded probability: the same counts on every run and on the CPU, grandchildren included (a
/// child's slot depends on thread order, its identity does not).
#[test]
fn seeded_counts_are_deterministic_through_a_chain() {
    let mut def = system(
        r#"events: [(on: Death, child: "child", count: 2, probability: 0.5)]"#,
        600,
        0.25,
    );
    let mut grandchild = def.emitters[1].clone();
    grandchild.name = "grandchild".into();
    grandchild.spawn.lifetime = regia_vfx::format::RangeDef { min: 3.0, max: 3.0 };
    def.emitters[1].update.events = vec![regia_vfx::format::EventSpawnDef {
        on: regia_vfx::format::EventTrigger::Spawn,
        child: "grandchild".into(),
        count: 3,
        probability: 0.5,
        inherit_velocity: 0.0,
    }];
    def.emitters.push(grandchild);
    def.validate().expect("valid chain");
    let (cpu, cpu_stats) = probe_steps(&def, Vec3::ZERO, Vec3::Y, cpu_settings(Vec3::ZERO), 40);
    let mut counts = Vec::new();
    for _ in 0..3 {
        let mut rig = Rig::new(def.clone(), profile(), Vec3::ZERO, Vec3::Y, Vec3::ZERO);
        rig.frames(40, DT);
        let stats = rig.stats();
        assert_eq!(stats.events_fired, cpu_stats.fired, "fired vs the CPU");
        assert_eq!(
            stats.children_spawned, cpu_stats.children_spawned,
            "children vs the CPU"
        );
        assert_pools_match(&gpu_rows(&rig.particles(), rig.base), &cpu_rows(&cpu), TOLERANCE);
        counts.push(stats.children_spawned);
    }
    assert!(counts.windows(2).all(|w| w[0] == w[1]), "{counts:?}");
    assert!(cpu_stats.children_spawned > 0);
}

/// More events than the buffer holds: the excess is counted, nothing past the end is written, the
/// children of what fit still spawn, and the simulation carries on.
#[test]
fn event_buffer_overflow_is_counted() {
    let def = system(r#"events: [(on: Death, child: "child", count: 1)]"#, 1000, 0.25);
    let small = VfxProfile {
        event_capacity: 64,
        ..profile()
    };
    let mut rig = Rig::new(def.clone(), small, Vec3::ZERO, Vec3::Y, Vec3::ZERO);
    rig.frames(30, DT);
    let stats = rig.stats();
    assert_eq!(rig.gpu.event_capacity(), 64);
    assert_eq!(stats.events_fired, 1000);
    // Count-based keep-set (fixrev-22-k, Muse review of fix-vfx1-determinism finding 1): the
    // budget counts events, not ranks — the first 64 fired survive in slot order wherever
    // their slots sit, and the 936 past the budget are counted lost, never a function of
    // window position.
    assert_eq!(stats.events_overflow, 1000 - 64);
    assert_eq!(stats.children_spawned, 64, "one child per kept event");
    assert_eq!(stats.alive, 64);
    let (_, cpu) = probe_steps(
        &def,
        Vec3::ZERO,
        Vec3::Y,
        ProbeSettings {
            event_capacity: 64,
            ..cpu_settings(Vec3::ZERO)
        },
        30,
    );
    assert_eq!(cpu.overflow, stats.events_overflow);
    // Past one ring in a frame: refused and counted.
    let tiny_ring = VfxProfile {
        child_capacity: 32,
        ..profile()
    };
    let mut rig = Rig::new(def, tiny_ring, Vec3::ZERO, Vec3::Y, Vec3::ZERO);
    rig.frames(30, DT);
    let stats = rig.stats();
    assert_eq!(stats.children_spawned, 32);
    assert_eq!(stats.children_dropped, 1000 - 32);
    assert_eq!(stats.alive, 32);
}

/// `read_pool_stats_now` (what `vfx.systems` with `gpu: true` reads) records its own copy and
/// returns the same counters as the copy the last frame recorded, overflow included.
#[test]
fn the_on_demand_stats_read_matches_the_frame_copy() {
    let def = system(r#"events: [(on: Death, child: "child", count: 1)]"#, 1000, 0.25);
    let small = VfxProfile {
        event_capacity: 64,
        ..profile()
    };
    let mut rig = Rig::new(def, small, Vec3::ZERO, Vec3::Y, Vec3::ZERO);
    rig.frames(30, DT);
    let recorded = rig.stats();
    let now = rig.gpu.read_pool_stats_now().expect("on-demand stats");
    assert_eq!(now, recorded);
    assert_eq!(now.events_overflow, 1000 - 64);
    assert_eq!(now.alive, 64);
}

/// on_collision uses the depth-collision path: a top-down camera over a depth buffer holding the
/// plane y = 0 sees what the CPU's ground plane sees, sliding and killing.
#[test]
fn on_collision_fires_from_the_depth_buffer() {
    for (kill, parents_left) in [(true, 0usize), (false, 50)] {
        let update = format!(
            r#"gravity: (0.0, -20.0, 0.0), depth_collision: Some((kill: {kill}, offset: 0.1)),
               events: [(on: Collision, child: "child", count: 2)]"#
        );
        let mut def = system(&update, 50, 20.0);
        def.emitters[0].init.shape = regia_vfx::format::ShapeDef::Box {
            half_extents: (3.0, 3.0, 0.0),
            surface: false,
        };
        def.emitters[0].init.velocity.mode = regia_vfx::format::VelocityMode::Directional {
            dir: (0.0, 0.0, 1.0),
            spread: 0.0,
            speed: regia_vfx::format::RangeDef { min: 0.0, max: 0.0 },
        };
        let origin = Vec3::new(0.0, 3.0, 0.0);
        let mut rig = Rig::new(def.clone(), profile(), origin, Vec3::Y, Vec3::ZERO);
        // Straight down from 20 m: the plane y = 0 is one depth value everywhere.
        rig.camera = VfxCamera::looking_at(Vec3::new(0.0, 20.0, 0.001), Vec3::ZERO);
        let aspect = 1.0;
        let (_, _, view_proj) = rig.camera.matrices(aspect);
        let clip = view_proj * Vec3::ZERO.extend(1.0);
        let depth_value = clip.z / clip.w;
        let depth = plane_depth(&rig.device, f64::from(depth_value));
        rig.gpu.set_depth_view(Some(&depth));
        let steps = 90;
        rig.frames(steps, DT);
        let stats = rig.stats();
        assert!(
            stats.events_fired >= 50,
            "every parent reached the ground: {stats:?}"
        );
        let parents = rig
            .particles()
            .iter()
            .filter(|p| p.emitter_slot == rig.base)
            .count();
        assert_eq!(parents, parents_left, "kill {kill}");
        if kill {
            // Each kill is one collision event; only on_collision is linked.
            assert_eq!(stats.children_spawned, 100);
        } else {
            // A sliding particle fires on every step in contact, so at least one per parent.
            assert!(stats.children_spawned >= 100, "{stats:?}");
        }
        let (_, cpu) = probe_steps(
            &def,
            origin,
            Vec3::Y,
            ProbeSettings {
                ground: Some(0.0),
                ..cpu_settings(Vec3::ZERO)
            },
            steps,
        );
        if kill {
            assert_eq!(stats.children_spawned, cpu.children_spawned);
            assert_eq!(stats.events_fired, cpu.fired);
        }
    }
}

/// A `Depth32Float` texture cleared to `value`.
fn plane_depth(device: &RenderDevice, value: f64) -> wgpu::TextureView {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("plane depth"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("clear plane depth"),
        color_attachments: &[],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(value as f32),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    device.queue.submit([encoder.finish()]);
    view
}

/// Scenes without events cost nothing new and change nothing: no child ring update, and a scene
/// with `wind_response` 0 under a gale is bit-identical to calm air.
#[test]
fn response_zero_is_bit_identical_under_a_gale() {
    let mut def = system(
        r#"gravity: (0.0, -2.0, 0.0), drag: 0.5, events: [(on: Death, child: "child", count: 1)]"#,
        400,
        2.0,
    );
    def.emitters.truncate(1);
    def.emitters[0].update.events.clear();
    def.validate().expect("one plain emitter");
    let run = |wind: Vec3| {
        let mut rig = Rig::new(def.clone(), profile(), Vec3::ZERO, Vec3::Y, wind);
        rig.frames(40, DT);
        assert!(!rig.gpu.events_active(), "no events in this system");
        rig.particles()
    };
    let calm = run(Vec3::ZERO);
    let gale = run(Vec3::new(50.0, 0.0, 30.0));
    assert_eq!(calm.len(), 400);
    assert_eq!(calm, gale);
}

/// A frame that does not advance (`dt` 0, a second render of a game frame) fires nothing twice.
#[test]
fn a_zero_step_frame_fires_nothing() {
    let def = system(r#"events: [(on: Spawn, child: "child", count: 1)]"#, 20, 5.0);
    let mut rig = Rig::new(def, profile(), Vec3::ZERO, Vec3::Y, Vec3::ZERO);
    rig.frames(1, DT);
    assert_eq!(rig.stats().events_fired, 20);
    for _ in 0..3 {
        rig.gpu.set_frame(&rig.camera, 0.0, 64, 64);
        let mut encoder = rig
            .device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        rig.gpu.simulate(&mut encoder);
        rig.gpu.record_stats_copy(&mut encoder);
        rig.device.queue.submit([encoder.finish()]);
    }
    let stats = rig.stats();
    assert_eq!(stats.events_fired, 20, "not fired again");
    assert_eq!(stats.children_spawned, 20, "last frame's events did spawn");
}

/// A rebase moves the child ring with the main ring, and children of events fired before the
/// shift land in the new space.
#[test]
fn a_rebase_moves_children_and_pending_events() {
    let mut def = system(r#"events: [(on: Death, child: "child", count: 4)]"#, 10, 0.25);
    def.emitters[1].init.shape = regia_vfx::format::ShapeDef::Point;
    def.emitters[1].init.velocity.mode = regia_vfx::format::VelocityMode::Directional {
        dir: (0.0, 1.0, 0.0),
        spread: 0.0,
        speed: regia_vfx::format::RangeDef { min: 0.0, max: 0.0 },
    };
    def.emitters[1].update = Default::default();
    def.emitters[0].init.shape = regia_vfx::format::ShapeDef::Point;
    def.emitters[0].init.velocity.mode = regia_vfx::format::VelocityMode::Directional {
        dir: (0.0, 1.0, 0.0),
        spread: 0.0,
        speed: regia_vfx::format::RangeDef { min: 0.0, max: 0.0 },
    };
    let origin = Vec3::new(100.0, 0.0, 0.0);
    let mut rig = Rig::new(def, profile(), origin, Vec3::Y, Vec3::ZERO);
    let shift = Vec3::new(64.0, 0.0, 0.0);
    // The parents die on the 16th step at 1/64 s; their events are consumed on the 17th.
    rig.frames(16, DT);
    rig.gpu.shift_origin(shift);
    rig.frames(1, DT);
    let children: Vec<PoolParticle> = rig.particles();
    assert_eq!(children.len(), 40);
    for child in &children {
        assert!((child.position[0] - 36.0).abs() < 1e-3, "{child:?}");
    }
    rig.gpu.shift_origin(shift);
    rig.frames(1, DT);
    for child in &rig.particles() {
        assert!((child.position[0] + 28.0).abs() < 1e-3, "{child:?}");
    }
}

/// The instance's emitter records outlive its parents until the children are dead.
#[test]
fn children_keep_drawing_after_their_parents_die() {
    let def = system(r#"events: [(on: Death, child: "child", count: 2)]"#, 10, 0.25);
    let mut rig = Rig::new(def, profile(), Vec3::ZERO, Vec3::Y, Vec3::ZERO);
    rig.frames(40, DT);
    assert_eq!(
        rig.scene.live_instances(),
        1,
        "children alive: the instance holds"
    );
    assert_eq!(rig.stats().alive, 20);
    // Lifetime 1 to 2 s: all dead by 2.5 s, then the instance retires and the ring stops.
    rig.frames(180, DT);
    assert_eq!(rig.stats().alive, 0);
    assert_eq!(rig.scene.live_instances(), 0);
    rig.frames(1, DT);
    assert!(!rig.gpu.events_active());
}

/// rev-kimi-19 finding 4: under overflow the CPU probe keeps the same events as the GPU. A
/// two-level OnSpawn chain fires 64 parent events per frame plus one event per child; with a
/// budget of 96 the second frame fires 128 (64 new parents in the main window, 64 children in
/// the child ring). `event_prepare` keeps the main window first (oldest first), then the child
/// ring by ring index; the probe used to keep its `Vec` order instead (this frame's children
/// before this frame's parents, scrambled further by `swap_remove`), so it grew grandchildren
/// where the GPU grew children. The pools, the fired and spawned counts, and the overflow
/// (counted one frame later on the GPU, in `event_prepare`) must all agree.
#[test]
fn an_overflowing_chain_keeps_the_same_events_on_the_cpu_and_the_gpu() {
    let mut def = system(r#"events: [(on: Spawn, child: "child", count: 1)]"#, 1, 1.0);
    def.emitters[0].spawn.mode = regia_vfx::format::SpawnMode::Rate(64.0 / DT);
    let mut grandchild = def.emitters[1].clone();
    grandchild.name = "grandchild".into();
    grandchild.spawn.lifetime = regia_vfx::format::RangeDef { min: 3.0, max: 3.0 };
    def.emitters[1].update.events = vec![regia_vfx::format::EventSpawnDef {
        on: regia_vfx::format::EventTrigger::Spawn,
        child: "grandchild".into(),
        count: 1,
        probability: 1.0,
        inherit_velocity: 0.0,
    }];
    def.emitters.push(grandchild);
    def.validate().expect("valid chain");
    let budget = VfxProfile {
        event_capacity: 96,
        ..profile()
    };
    let settings = ProbeSettings {
        event_capacity: 96,
        ..cpu_settings(Vec3::ZERO)
    };
    let mut rig = Rig::new(def.clone(), budget, Vec3::ZERO, Vec3::Y, Vec3::ZERO);
    let mut done = 0;
    for steps in [2u32, 3, 6] {
        rig.frames(steps - done, DT);
        done = steps;
        let (cpu, cpu_stats) = probe_steps(&def, Vec3::ZERO, Vec3::Y, settings, steps);
        let stats = rig.stats();
        assert_eq!(
            stats.events_fired, cpu_stats.fired,
            "events fired by step {steps}"
        );
        assert_eq!(
            stats.children_spawned, cpu_stats.children_spawned,
            "children by step {steps}"
        );
        // The GPU counts a lost event when the next frame's prepare drops it.
        let (_, lagged) = probe_steps(&def, Vec3::ZERO, Vec3::Y, settings, steps - 1);
        assert_eq!(stats.events_overflow, lagged.overflow, "overflow by step {steps}");
        assert_pools_match(&gpu_rows(&rig.particles(), rig.base), &cpu_rows(&cpu), TOLERANCE);
    }
    let stats = rig.stats();
    assert!(
        stats.events_overflow > 0,
        "the chain overflowed the budget: {stats:?}"
    );
    let grandchildren = rig
        .particles()
        .iter()
        .filter(|p| p.emitter_slot - rig.base == 2)
        .count();
    assert!(grandchildren > 0, "the second level spawned");
}

/// ADR-7431 (4): a frame whose update window covers the whole pool after the ring wrapped. A
/// 64-slot pool takes 24 short-lived parents a frame, each firing OnSpawn into one child, with an
/// event budget of 16. The third frame's parents land in slots 48..64 and 0..8 (spawns 48..72):
/// the window (every spawn of the children's 2.25 s) fills the pool, so `pool_slot` scans slot
/// order from slot 0 and `event_prepare` keeps slots 0..8 then 48..56. The probe without the pool
/// kept spawns 48..64 (its spawn order) and grew different children. With `particle_capacity` it
/// keeps the same set: the pools, the fired and spawned counts and the overflow agree.
#[test]
fn a_whole_pool_window_keeps_the_same_events_on_the_cpu_and_the_gpu() {
    let mut def = system(r#"events: [(on: Spawn, child: "child", count: 1)]"#, 1, 0.02);
    def.emitters[0].spawn.mode = regia_vfx::format::SpawnMode::Rate(24.0 / DT);
    def.validate().expect("valid");
    let budget = VfxProfile {
        particle_capacity: 64,
        event_capacity: 16,
        child_capacity: 64,
        ..profile()
    };
    let settings = ProbeSettings {
        event_capacity: 16,
        child_capacity: 64,
        particle_capacity: 64,
        ..cpu_settings(Vec3::ZERO)
    };
    let mut rig = Rig::new(def.clone(), budget, Vec3::ZERO, Vec3::Y, Vec3::ZERO);
    let mut done = 0;
    for steps in [2u32, 3, 4, 6] {
        rig.frames(steps - done, DT);
        done = steps;
        let (cpu, cpu_stats) = probe_steps(&def, Vec3::ZERO, Vec3::Y, settings, steps);
        let stats = rig.stats();
        assert_eq!(
            stats.events_fired, cpu_stats.fired,
            "events fired by step {steps}"
        );
        assert_eq!(
            stats.children_spawned, cpu_stats.children_spawned,
            "children by step {steps}"
        );
        let (_, lagged) = probe_steps(&def, Vec3::ZERO, Vec3::Y, settings, steps - 1);
        assert_eq!(stats.events_overflow, lagged.overflow, "overflow by step {steps}");
        assert_pools_match(&gpu_rows(&rig.particles(), rig.base), &cpu_rows(&cpu), TOLERANCE);
    }
    let window = rig.gpu.update_window();
    assert_eq!(window, (0, 64), "the last frame's window is the whole pool");
    assert!(rig.stats().events_overflow > 0, "the budget overflowed");
}
