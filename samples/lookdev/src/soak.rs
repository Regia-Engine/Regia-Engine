// SPDX-License-Identifier: MIT-0
//! The hitch soak (`specs/27` § 2.4, `do27_chase --soak`): a long scripted drive over every
//! region of a DO27 map through the full runtime frame, with the events that make a game
//! hitch — camera cuts (the plate cameras and teleports across the map), time-of-day jumps,
//! scatter tiles streaming in and out, set pieces entering view, decals and VFX spawned.
//!
//! The script is deterministic: frame `i` of a map always has the same pose, sun and spawns,
//! so a before/after pair differs only by the engine. [`SoakScript::pose`] feeds
//! [`crate::chase::run`]; the per-frame samples become a CSV timeline, a PNG plot
//! ([`crate::soak_plot`]) and a [`regia_render::HitchReport`].

use std::sync::{Arc, Mutex};

use regia_core::glam::Vec3;
use regia_vfx::scene::VfxLibrary;
use regia_vfx::sim::LodInput;
use regia_vfx::{Decal, DecalPool, VfxGpu, VfxProfile, VfxScene};

use crate::LookdevError;
use crate::chase::{DT, SPEED};
use crate::do27::sun_toward;

/// Bit in [`SoakPose::events`]: the camera cut this frame.
pub const EVENT_CUT: u8 = 1;
/// Bit in [`SoakPose::events`]: the sun jumped to a new time of day.
pub const EVENT_TIME_OF_DAY: u8 = 2;
/// Bit in [`SoakPose::events`]: VFX and a decal were spawned.
pub const EVENT_SPAWN: u8 = 4;

/// What the soak script does over a map.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SoakOptions {
    /// Measured seconds of drive per map (60 Hz steps).
    pub seconds: f32,
    /// A camera cut every this many seconds (0 = never).
    pub cut_every_s: f32,
    /// How long the cut camera holds before cutting back to the chase.
    pub cut_hold_s: f32,
    /// A time-of-day jump every this many seconds (0 = never).
    pub time_of_day_every_s: f32,
    /// A VFX burst and a decal every this many seconds (0 = never).
    pub spawn_every_s: f32,
}

impl SoakOptions {
    /// The ten-minute soak: every region, a cut every 12 s, the sun every 20 s, effects every 4 s.
    pub const LONG: Self = Self {
        seconds: 600.0,
        cut_every_s: 12.0,
        cut_hold_s: 3.0,
        time_of_day_every_s: 20.0,
        spawn_every_s: 4.0,
    };
    /// The 60 s scripted drive of the soak test: the same event mix, compressed.
    pub const MINUTE: Self = Self {
        seconds: 60.0,
        cut_every_s: 8.0,
        cut_hold_s: 2.0,
        time_of_day_every_s: 10.0,
        spawn_every_s: 3.0,
    };
}

/// The time-of-day stops the sun cycles through: `(elevation°, azimuth°)`. The first is
/// replaced by the map's plate sun.
pub const SUN_STOPS: [(f32, f32); 6] = [
    (0.0, 0.0),
    (62.0, 140.0),
    (14.0, 255.0),
    (35.0, 20.0),
    (6.0, 95.0),
    (45.0, 300.0),
];

/// One frame of the script.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SoakPose {
    /// Vehicle XZ position.
    pub vehicle: [f32; 2],
    /// Vehicle XZ heading (unit).
    pub heading: [f32; 2],
    /// The cut camera, or `None` for the chase camera behind the vehicle.
    pub camera: Option<SoakCamera>,
    /// Unit vector toward the sun.
    pub sun_toward: [f32; 3],
    /// [`EVENT_CUT`] | [`EVENT_TIME_OF_DAY`] | [`EVENT_SPAWN`] bits.
    pub events: u8,
}

/// Where a cut camera looks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SoakCamera {
    /// A fixed eye and target with a horizontal field of view in degrees.
    Fixed([f32; 3], [f32; 3], f32),
    /// A chase camera behind this XZ point and heading (heights come from the heightfield).
    Chase([f32; 2], [f32; 2]),
}

/// A cut camera: a fixed `(eye, target, fov°)`, or a teleport this many metres further
/// along the tour (a chase camera in an unseen region).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SoakCut {
    /// A fixed camera (a UE plate camera).
    Fixed([f32; 3], [f32; 3], f32),
    /// The chase camera, `metres` further along the tour.
    Teleport(f32),
}

/// The deterministic soak drive of one map.
#[derive(Debug, Clone)]
pub struct SoakScript {
    /// The tour polyline (XZ).
    pub tour: Vec<[f32; 2]>,
    /// Tour length, metres.
    pub length_m: f32,
    /// The cut cameras, used in turn.
    pub cuts: Vec<SoakCut>,
    /// Sun stops (unit vectors toward the sun).
    pub suns: Vec<[f32; 3]>,
    /// The script's timing.
    pub options: SoakOptions,
}

/// A boustrophedon over the square `[-half, half]²` that, after `route`, covers the map in
/// rows about `spacing` apart, sized so the whole tour is at least `length_m` long.
#[must_use]
pub fn tour(route: &[[f32; 2]], half: f32, length_m: f32) -> Vec<[f32; 2]> {
    let mut points: Vec<[f32; 2]> = route.to_vec();
    let route_len = polyline_length(&points);
    let side = 2.0 * half;
    let rows = (((length_m - route_len).max(0.0) / side.max(1.0)).ceil() as usize).clamp(3, 64);
    for r in 0..rows {
        let z = -half + side * r as f32 / (rows - 1) as f32;
        let (a, b) = if r % 2 == 0 { (-half, half) } else { (half, -half) };
        points.push([a, z]);
        points.push([b, z]);
    }
    points.dedup();
    points
}

/// Length of an XZ polyline, metres.
#[must_use]
pub fn polyline_length(points: &[[f32; 2]]) -> f32 {
    points
        .windows(2)
        .map(|p| ((p[1][0] - p[0][0]).powi(2) + (p[1][1] - p[0][1]).powi(2)).sqrt())
        .sum()
}

/// Position and unit heading `s` metres along a polyline (clamped to its ends).
#[must_use]
pub fn along(route: &[[f32; 2]], s: f32) -> ([f32; 2], [f32; 2]) {
    let mut left = s.max(0.0);
    for pair in route.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let d = [b[0] - a[0], b[1] - a[1]];
        let len = (d[0] * d[0] + d[1] * d[1]).sqrt();
        if len < 1e-3 {
            continue;
        }
        let dir = [d[0] / len, d[1] / len];
        if left <= len {
            return ([a[0] + dir[0] * left, a[1] + dir[1] * left], dir);
        }
        left -= len;
    }
    let n = route.len();
    if n < 2 {
        return (route.first().copied().unwrap_or([0.0; 2]), [1.0, 0.0]);
    }
    let (a, b) = (route[n - 2], route[n - 1]);
    let d = [b[0] - a[0], b[1] - a[1]];
    let len = (d[0] * d[0] + d[1] * d[1]).sqrt().max(1e-3);
    (b, [d[0] / len, d[1] / len])
}

impl SoakScript {
    /// The script for a map: `route` (the chase route) then the boustrophedon over 84 % of
    /// a `world_width_m` map; `cameras` are the map's plate cameras; `plate_sun` its
    /// `(elevation°, azimuth°)`.
    #[must_use]
    pub fn new(
        route: &[[f32; 2]],
        world_width_m: f32,
        cameras: &[([f32; 3], [f32; 3], f32)],
        plate_sun: (f32, f32),
        options: SoakOptions,
    ) -> Self {
        let half = world_width_m * 0.42;
        let tour = tour(route, half, options.seconds * SPEED);
        let length_m = polyline_length(&tour);
        let mut cuts: Vec<SoakCut> = Vec::new();
        for (i, &(eye, target, fov)) in cameras.iter().enumerate() {
            cuts.push(SoakCut::Fixed(eye, target, fov));
            // A teleport across the map between plate cameras.
            cuts.push(SoakCut::Teleport(
                length_m * (0.3 + 0.17 * i as f32) % length_m.max(1.0),
            ));
        }
        if cuts.is_empty() {
            cuts.push(SoakCut::Teleport(length_m * 0.5));
        }
        let mut suns: Vec<[f32; 3]> = SUN_STOPS.iter().map(|&(el, az)| sun_toward(el, az)).collect();
        suns[0] = sun_toward(plate_sun.0, plate_sun.1);
        Self {
            tour,
            length_m,
            cuts,
            suns,
            options,
        }
    }

    /// Measured frames in the script.
    #[must_use]
    pub fn frames(&self) -> u32 {
        (self.options.seconds / DT).round() as u32
    }

    /// Distance along the tour at `t` seconds: forward, then back (a ping-pong past the end).
    fn tour_s(&self, t: f32) -> f32 {
        let s = t * SPEED;
        let lap = self.length_m.max(1.0);
        let m = s % (2.0 * lap);
        if m <= lap { m } else { 2.0 * lap - m }
    }

    /// Which `period`-long slot `t` falls in (`None` when the period is off).
    fn slot(t: f32, period: f32) -> Option<u32> {
        (period > 0.0).then(|| (t / period).floor() as u32)
    }

    /// The pose of measured frame `frame` (warm-up frames use frame 0).
    #[must_use]
    pub fn pose(&self, frame: u32) -> SoakPose {
        let o = self.options;
        let t = frame as f32 * DT;
        let prev_t = frame.saturating_sub(1) as f32 * DT;
        let (vehicle, heading) = along(&self.tour, self.tour_s(t));
        let mut events = 0u8;
        // Cuts: hold the k-th cut camera from k·period for `cut_hold_s` (k ≥ 1).
        let cut_at = |t: f32| -> Option<usize> {
            let k = Self::slot(t, o.cut_every_s)?;
            (k >= 1 && t - k as f32 * o.cut_every_s < o.cut_hold_s)
                .then(|| (k as usize - 1) % self.cuts.len().max(1))
        };
        let cut = cut_at(t);
        if frame > 0 && cut != cut_at(prev_t) {
            events |= EVENT_CUT;
        }
        let camera = cut.and_then(|k| self.cuts.get(k)).map(|c| match *c {
            SoakCut::Fixed(eye, target, fov) => SoakCamera::Fixed(eye, target, fov),
            SoakCut::Teleport(ahead) => {
                let (p, dir) = along(&self.tour, (self.tour_s(t) + ahead) % self.length_m.max(1.0));
                SoakCamera::Chase(p, dir)
            }
        });
        let sun_slot = Self::slot(t, o.time_of_day_every_s).unwrap_or(0) as usize;
        if frame > 0 && Self::slot(prev_t, o.time_of_day_every_s).unwrap_or(0) as usize != sun_slot {
            events |= EVENT_TIME_OF_DAY;
        }
        if frame > 0
            && Self::slot(t, o.spawn_every_s).is_some()
            && Self::slot(prev_t, o.spawn_every_s) != Self::slot(t, o.spawn_every_s)
        {
            events |= EVENT_SPAWN;
        }
        SoakPose {
            vehicle,
            heading,
            camera,
            sun_toward: self.suns[sun_slot % self.suns.len()],
            events,
        }
    }
}

/// The soak's effects: the showcase VFX library on a Desktop High pool and a decal pool,
/// drawn through the renderer's translucent layer like the runtime's `VfxRuntimePlugin`.
pub struct SoakVfx {
    gpu: Arc<Mutex<VfxGpu>>,
    scene: VfxScene,
    decals: DecalPool,
    names: Vec<String>,
    capacity: u32,
}

impl SoakVfx {
    /// The GPU pool and the showcase library.
    ///
    /// # Errors
    ///
    /// The VFX shaders failed to build or a showcase definition was rejected.
    pub fn new(device: &regia_render::RenderDevice) -> Result<Self, LookdevError> {
        let profile = VfxProfile::desktop_high();
        let gpu = VfxGpu::new(device, profile).map_err(|e| LookdevError::Config(e.to_string()))?;
        Self::with_gpu(gpu, profile)
    }

    /// [`Self::new`] around a context built elsewhere (on a warm worker:
    /// [`VfxGpu::new_warmed`]) with `profile`.
    ///
    /// # Errors
    ///
    /// A showcase definition was rejected.
    pub fn with_gpu(gpu: VfxGpu, profile: VfxProfile) -> Result<Self, LookdevError> {
        let mut library = VfxLibrary::new();
        let mut names = Vec::new();
        for def in regia_vfx::showcase_effects() {
            names.push(def.name.clone());
            library
                .add(def)
                .map_err(|e| LookdevError::Config(e.to_string()))?;
        }
        Ok(Self {
            gpu: Arc::new(Mutex::new(gpu)),
            scene: VfxScene::new(library),
            decals: DecalPool::new(profile.decal_capacity),
            names,
            capacity: profile.particle_capacity,
        })
    }

    /// The translucent layer to install on the renderer.
    #[must_use]
    pub fn layer(&self) -> Arc<dyn regia_render::translucent::TranslucentLayer> {
        regia_vfx::translucent_layer(Arc::clone(&self.gpu))
    }

    /// Spawns one showcase effect 14–30 m ahead of the vehicle and a 3 m scorch decal under it.
    pub fn spawn(&mut self, map: &regia_terrain::TerrainMap, p: [f32; 2], dir: [f32; 2], step: u32) {
        let ahead = 14.0 + (step % 5) as f32 * 4.0;
        let (x, z) = (p[0] + dir[0] * ahead, p[1] + dir[1] * ahead);
        if !self.names.is_empty() {
            let name = &self.names[(step as usize / 7) % self.names.len()];
            let at = Vec3::new(x, map.sample_bilinear(x, z) + 0.5, z);
            let _ = self.scene.spawn(name, at, Vec3::Y, u64::from(step));
        }
        let normal = Vec3::from_array(map.normal_at(p[0], p[1]));
        self.decals.spawn(Decal {
            position: Vec3::new(p[0], map.sample_bilinear(p[0], p[1]), p[1]),
            rotation: regia_vfx::align_to_normal(normal, step as f32 * 0.37),
            half_size: Vec3::new(1.5, 1.5, 1.0),
            color: [0.05, 0.04, 0.03, 0.85],
            age: 0.0,
            lifetime: Some(30.0),
        });
    }

    /// A permanent `n`×`n` grid of 4 m decals every `spacing` metres around `center` (the pool
    /// keeps the newest [`VfxProfile::decal_capacity`]), for a
    /// frame whose decal-receiver pass covers a whole view (`do27_chase --decals`). Every
    /// other decal accepts terrain only, so the receiver's layers decide what it covers;
    /// the rest fade with the receiver normal.
    pub fn spawn_decal_grid(
        &mut self,
        map: &regia_terrain::TerrainMap,
        center: [f32; 2],
        n: u32,
        spacing: f32,
    ) {
        let half = (n.saturating_sub(1)) as f32 * 0.5;
        for row in 0..n {
            for col in 0..n {
                let x = center[0] + (col as f32 - half) * spacing;
                let z = center[1] + (row as f32 - half) * spacing;
                let index = row * n + col;
                let normal = Vec3::from_array(map.normal_at(x, z));
                let decal = Decal {
                    position: Vec3::new(x, map.sample_bilinear(x, z), z),
                    rotation: regia_vfx::align_to_normal(normal, index as f32 * 0.61),
                    half_size: Vec3::new(2.0, 2.0, 3.0),
                    color: [0.06, 0.05, 0.04, 0.9],
                    age: 0.0,
                    lifetime: None,
                };
                let mut surface = regia_vfx::DecalSurface::from_decal(&decal);
                if index.is_multiple_of(2) {
                    surface.receiver_mask = 1;
                }
                self.decals.spawn_with(decal, surface);
            }
        }
    }

    /// One 60 Hz step of the effects and their upload, seen from `eye`.
    pub fn update(&mut self, eye: [f32; 3]) {
        self.scene.update(
            DT,
            &LodInput {
                camera_pos: Vec3::from_array(eye),
            },
            self.capacity,
        );
        self.decals.update(DT);
        if let Ok(mut gpu) = self.gpu.lock() {
            self.scene.upload(&mut gpu);
            gpu.write_decals_with(self.decals.decals(), self.decals.surfaces());
        }
    }
}

/// The machine's CPU floor ([`cpu_floor`]): percentiles of a fixed workload, ms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuFloor {
    /// Iterations.
    pub runs: usize,
    /// Median, ms.
    pub p50_ms: f32,
    /// 99th percentile, ms.
    pub p99_ms: f32,
    /// 99.9th percentile, ms.
    pub p999_ms: f32,
    /// Runs over 2 × p50.
    pub over_2x: usize,
}

impl CpuFloor {
    /// The box can time a CPU-bound frame against `specs/27` § 2.4: the fixed work itself
    /// stays within p99/p50 ≤ 1.3, p99.9/p50 ≤ 2.0 and ≤ 0.1 % over 2 × p50.
    #[must_use]
    pub fn quiet(&self) -> bool {
        let p50 = self.p50_ms.max(1e-6);
        self.p99_ms / p50 <= 1.3
            && self.p999_ms / p50 <= 2.0
            && self.over_2x as f32 <= 0.001 * self.runs as f32
    }

    /// One line.
    #[must_use]
    pub fn summary(&self) -> String {
        let p50 = self.p50_ms.max(1e-6);
        format!(
            "cpu floor ({} runs of fixed work): p50 {:.3} ms, p99 {:.3} ({:.2}x), p99.9 {:.3} ({:.2}x), > 2 x p50: {} -> {}",
            self.runs,
            self.p50_ms,
            self.p99_ms,
            self.p99_ms / p50,
            self.p999_ms,
            self.p999_ms / p50,
            self.over_2x,
            if self.quiet() {
                "quiet: wall-time hitch rows are meaningful here"
            } else {
                "NOISY: wall-time hitch rows measure the box, not the engine"
            }
        )
    }
}

/// Times `runs` repetitions of the same ~3 ms of CPU work (a 4 MiB table walked with dependent
/// loads): on a dedicated core p99/p50 stays near 1; on a shared one the spread is the noise a
/// CPU-bound frame's wall time inherits (scheduler, SMT sibling, shared cache).
#[must_use]
pub fn cpu_floor(runs: u32) -> CpuFloor {
    let table: Vec<u64> = (0..524_288u64)
        .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .collect();
    let mut times = Vec::with_capacity(runs as usize);
    let mut acc = 1u64;
    for _ in 0..runs {
        let started = std::time::Instant::now();
        for _ in 0..300_000 {
            acc = acc.rotate_left(7) ^ table[(acc as usize) & (table.len() - 1)];
        }
        times.push(started.elapsed().as_secs_f32() * 1000.0);
    }
    std::hint::black_box(acc);
    times.sort_by(f32::total_cmp);
    let q = |p: f32| regia_render::hitch::percentile_sorted(&times, p);
    let p50 = q(0.5);
    CpuFloor {
        runs: times.len(),
        p50_ms: p50,
        p99_ms: q(0.99),
        p999_ms: q(0.999),
        over_2x: times.iter().filter(|t| **t > 2.0 * p50).count(),
    }
}

/// Event bits as a short tag list (`cut,tod,spawn`).
#[must_use]
pub fn event_tags(events: u8) -> String {
    let mut tags = Vec::new();
    if events & EVENT_CUT != 0 {
        tags.push("cut");
    }
    if events & EVENT_TIME_OF_DAY != 0 {
        tags.push("tod");
    }
    if events & EVENT_SPAWN != 0 {
        tags.push("spawn");
    }
    tags.join("+")
}

/// Why the frames over 2 × p50 were long ([`spike_causes`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpikeCauses {
    /// Frames over 2 × p50.
    pub spikes: usize,
    /// Of which under 2 × p50 once run-queue wait and memory stall are taken out: the
    /// scheduler gave the render thread's or an encode worker's core to another process, or
    /// the process waited on reclaim (the machine, not the engine).
    pub machine: usize,
    /// Of the rest, the largest overrun of a phase against its median: `render_into`
    /// recording (`encode`).
    pub encode: usize,
    /// … `submit` (finishing the command buffers on the workers and `queue.submit`).
    pub submit: usize,
    /// … the host's scatter update, contact map or collect.
    pub scatter: usize,
    /// … waiting for the GPU (frames in flight).
    pub gpu_wait: usize,
    /// … the soak's effect spawns and their CPU step ([`crate::chase::ChaseSample::spawn_ms`]).
    pub spawn: usize,
    /// … none of the above (time outside the measured phases).
    pub other: usize,
    /// Spikes not explained by the waits whose core canary ran at least 1.5 × its median
    /// around the frame: a contended core (SMT sibling, shared cache), counted before the
    /// phases.
    pub contended: usize,
    /// Engine spikes (neither machine nor contended) on or right after a scripted event.
    pub engine_at_events: usize,
    /// Spikes on or right after a scripted event (cut, time-of-day jump, spawn).
    pub at_events: usize,
    /// Frames still over 2 × p50 with the run-queue wait taken out of the wall time.
    pub without_runq: usize,
    /// Spikes in which the process took a major page fault (a page read back from disk or
    /// swap while a thread slept). Counted across the causes above, not instead of them.
    pub major_faults: usize,
}

/// Attributes every frame over 2 × `p50_ms`: to the machine when it is under the limit
/// without its run-queue and memory waits, to a contended core when its canary ran slow,
/// otherwise to the phase whose time overran its own median the most. The report separates
/// what a quiet box would not show from what the engine did.
#[must_use]
pub fn spike_causes(samples: &[crate::chase::ChaseSample], p50_ms: f32) -> SpikeCauses {
    let median = |f: fn(&crate::chase::ChaseSample) -> f32| {
        let mut v: Vec<f32> = samples.iter().map(f).collect();
        v.sort_by(f32::total_cmp);
        regia_render::hitch::percentile_sorted(&v, 0.5)
    };
    let encode = median(|s| s.encode_ms);
    let submit = median(|s| s.submit_ms);
    let scatter = median(|s| s.scatter_ms);
    let wait = median(|s| s.wait_ms);
    let spawn = median(|s| s.spawn_ms);
    let canary = median(|s| s.canary_us);
    let limit = 2.0 * p50_ms;
    let mut out = SpikeCauses::default();
    for (i, s) in samples.iter().enumerate() {
        // The machine's share: run-queue wait of the render thread and its encode workers,
        // plus time the process stalled on memory (reclaim).
        let runq = s.runq_ms + s.runq_workers_ms + s.psi_mem_ms;
        if s.wall_ms - runq > limit {
            out.without_runq += 1;
        }
        if s.wall_ms <= limit {
            continue;
        }
        out.spikes += 1;
        if s.major_faults > 0 {
            out.major_faults += 1;
        }
        let prev_events = i
            .checked_sub(1)
            .and_then(|j| samples.get(j))
            .map_or(0, |p| p.events);
        if s.events | prev_events != 0 {
            out.at_events += 1;
        }
        // The machine: the frame is under the limit once the waits are taken out (a wait
        // sits inside whichever phase was preempted, so phases alone would blame the engine).
        if s.wall_ms - runq <= limit {
            out.machine += 1;
            continue;
        }
        if canary > 0.0 && s.canary_us >= 1.5 * canary {
            out.contended += 1;
            continue;
        }
        if s.events | prev_events != 0 {
            out.engine_at_events += 1;
        }
        let overruns = [
            (s.encode_ms - encode, 1),
            (s.submit_ms - submit, 2),
            (s.scatter_ms - scatter, 3),
            (s.wait_ms - wait, 4),
            (s.spawn_ms - spawn, 6),
        ];
        let (largest, which) = overruns
            .iter()
            .copied()
            .fold((0.0_f32, 5), |best, o| if o.0 > best.0 { o } else { best });
        let which = if largest < 0.25 * (s.wall_ms - p50_ms) {
            5
        } else {
            which
        };
        match which {
            1 => out.encode += 1,
            2 => out.submit += 1,
            3 => out.scatter += 1,
            4 => out.gpu_wait += 1,
            6 => out.spawn += 1,
            _ => out.other += 1,
        }
    }
    out
}

impl SpikeCauses {
    /// Spikes left to the engine (neither the machine nor a contended core).
    #[must_use]
    pub fn engine(&self) -> usize {
        self.spikes - self.machine - self.contended
    }

    /// One report line.
    #[must_use]
    pub fn line(&self, frames: usize) -> String {
        format!(
            "spike causes: machine (under 2 x p50 without run-queue wait and memory stall) {}, contended core (canary >= 1.5 x) {}; \
             engine {} ({:.3} %; largest overrun vs its median: encode {}, submit {}, scatter {}, gpu wait {}, spawn {}, other {}; at a cut/time-of-day/spawn {}); \
             all spikes at events {}; over 2 x p50 with the waits removed: {} ({:.3} %); with a major page fault {}",
            self.machine,
            self.contended,
            self.engine(),
            100.0 * self.engine() as f32 / frames.max(1) as f32,
            self.encode,
            self.submit,
            self.scatter,
            self.gpu_wait,
            self.spawn,
            self.other,
            self.engine_at_events,
            self.at_events,
            self.without_runq,
            100.0 * self.without_runq as f32 / frames.max(1) as f32,
            self.major_faults,
        )
    }
}

/// `text` with the § 2.4 verdict line replaced by a "not applicable" line when the run had no
/// warm-up frames: the first frames create every pipeline and bind group, so a `--warmup 0` run
/// (the sweep's cold and quit scenes) cannot meet the zero-creates target and `FAIL` would mislead.
#[must_use]
pub fn verdict_for_warmup(text: &str, warmup: u32) -> String {
    if warmup > 0 {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("verdict: ") {
            out.push_str("  verdict: n/a (no warm-up frames; specs/27 § 2.4 is judged after warm-up)\n");
        } else {
            out.push_str(line);
        }
    }
    out
}

/// The report for one map: the § 2.4 summary, the frame-path creates by label and the
/// worst spikes with what happened in (and just before) each.
#[must_use]
pub fn report_text(run: &crate::chase::ChaseRun, report: &regia_render::HitchReport) -> String {
    use std::collections::BTreeMap;
    use std::fmt::Write as _;
    let mut out = format!("{} hitch report (specs/27 § 2.4)\n{}", run.map, report.summary());
    let _ = writeln!(
        out,
        "  {}",
        spike_causes(&run.samples, report.p50_ms).line(run.samples.len())
    );
    let mut by_label: BTreeMap<(regia_render::FramePathKind, &str), (u64, u64, u32, u32)> = BTreeMap::new();
    for (frame, event) in &run.events {
        let entry = by_label
            .entry((event.kind, event.label.as_str()))
            .or_insert((0, 0, u32::MAX, 0));
        entry.0 += 1;
        entry.1 += event.bytes;
        entry.2 = entry.2.min(*frame);
        entry.3 = entry.3.max(*frame);
    }
    let _ = writeln!(out, "  frame-path creates after warm-up, by label:");
    if by_label.is_empty() {
        let _ = writeln!(out, "    none");
    }
    for ((kind, label), (count, bytes, first, last)) in &by_label {
        let _ = writeln!(
            out,
            "    {kind:?} {label:?}: {count} ({} KiB), frames {first}..={last}",
            bytes / 1024
        );
    }
    let _ = writeln!(
        out,
        "  worst frames over 2 x p50 (frame: wall = cpu + wait; what happened):"
    );
    for spike in &report.spikes {
        let Some(s) = run.samples.get(spike.frame) else {
            continue;
        };
        let prev = spike.frame.checked_sub(1).and_then(|i| run.samples.get(i));
        let labels: Vec<&str> = run
            .events
            .iter()
            .filter(|(f, _)| *f as usize == spike.frame)
            .map(|(_, e)| e.label.as_str())
            .collect();
        let c = s.counters;
        let _ = writeln!(
            out,
            "    {:>6} ({:>7.2} s): wall {:>6.2} ms = cpu {:.2} (encode {:.2}, submit {:.2}) + scatter {:.2} (update {:.2}, contact {:.2}, collect {:.2}) + wait {:.2}; runq {:.2} (+ workers {:.2}); psi cpu {:.2} mem {:.2}; faults {} (major {}); spawn {:.2} ms (major faults {}); canary {:.0} us; gpu {:.2}; \
             events [{}] prev [{}]; creates p{} bg{} buf{} tex{} ({} KiB); sky bakes {}; upload {} KiB; terrain {} writes {} KiB; rocks {}{}",
            spike.frame,
            spike.frame as f32 * DT,
            s.wall_ms,
            s.cpu_ms,
            s.encode_ms,
            s.submit_ms,
            s.scatter_ms,
            s.scatter_update_ms,
            s.contact_ms,
            s.collect_ms,
            s.wait_ms,
            s.runq_ms,
            s.runq_workers_ms,
            s.psi_cpu_ms,
            s.psi_mem_ms,
            s.faults,
            s.major_faults,
            s.spawn_ms,
            s.spawn_major_faults,
            s.canary_us,
            s.gpu.frame_ms,
            event_tags(s.events),
            prev.map_or(String::new(), |p| event_tags(p.events)),
            c.pipelines,
            c.bind_groups,
            c.buffers,
            c.textures,
            (c.buffer_bytes + c.texture_bytes) / 1024,
            c.inline_bakes,
            c.upload_bytes / 1024,
            s.shared_upload.0,
            s.shared_upload.1 / 1024,
            s.rocks,
            if labels.is_empty() {
                String::new()
            } else {
                format!("; {}", labels.join(", "))
            }
        );
    }
    out
}

/// Writes `<map>.csv` (one row per measured frame), `<map>.png` ([`crate::soak_plot`]) and
/// `<map>-report.txt` (`text`) into `dir`.
///
/// # Errors
///
/// The directory or a file could not be written.
pub fn write_timeline_files(
    dir: &std::path::Path,
    run: &crate::chase::ChaseRun,
    report: &regia_render::HitchReport,
    text: &str,
) -> Result<(), LookdevError> {
    use std::fmt::Write as _;
    let io = |path: &std::path::Path| {
        let path = path.display().to_string();
        move |source| LookdevError::Io { path, source }
    };
    std::fs::create_dir_all(dir).map_err(io(dir))?;
    let mut csv = String::from(
        "frame,t_s,wall_ms,cpu_ms,encode_ms,submit_ms,scatter_ms,scatter_update_ms,contact_ms,collect_ms,wait_ms,\
         gpu_ms,rocks,pipelines,bind_groups,buffers,buffer_kib,textures,texture_kib,upload_kib,blocking_polls,map_asyncs,\
         terrain_writes,terrain_kib,runq_ms,tiles_placed,inline_bakes,sky_worker,runq_workers_ms,psi_cpu_ms,psi_mem_ms,faults,canary_us,events,\
         major_faults,spawn_ms,spawn_major_faults,\
         gpu_shadows_ms,gpu_gi_ms,gpu_prepass_ms,gpu_cull_ms,gpu_ssao_ms,gpu_contact_ms,gpu_forward_ms,gpu_terrain_forward_ms,\
         gpu_ssgi_ms,gpu_ssr_ms,gpu_sssr_trace_ms,gpu_sssr_denoise_ms,gpu_atmosphere_ms,gpu_clouds_ms,gpu_fog_ms,gpu_scene_ms,\
         gpu_taau_ms,gpu_post_ms\n",
    );
    for (i, s) in run.samples.iter().enumerate() {
        let c = s.counters;
        let _ = writeln!(
            csv,
            "{i},{:.4},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{},{},{},{},{},{},{},{},{},{},{},{},{:.3},{},{},{},{:.3},{:.3},{:.3},{},{:.1},{},{},{:.3},{}",
            i as f32 * DT,
            s.wall_ms,
            s.cpu_ms,
            s.encode_ms,
            s.submit_ms,
            s.scatter_ms,
            s.scatter_update_ms,
            s.contact_ms,
            s.collect_ms,
            s.wait_ms,
            s.gpu.frame_ms,
            s.rocks,
            c.pipelines,
            c.bind_groups,
            c.buffers,
            c.buffer_bytes / 1024,
            c.textures,
            c.texture_bytes / 1024,
            c.upload_bytes / 1024,
            c.blocking_polls,
            c.map_asyncs,
            s.shared_upload.0,
            s.shared_upload.1 / 1024,
            s.runq_ms,
            s.tiles_placed,
            c.inline_bakes,
            u8::from(s.sky_worker),
            s.runq_workers_ms,
            s.psi_cpu_ms,
            s.psi_mem_ms,
            s.faults,
            s.canary_us,
            event_tags(s.events),
            s.major_faults,
            s.spawn_ms,
            s.spawn_major_faults,
        );
        // Per-pass GPU time (the timestamp ranges `GpuTimings` resolves), so a slow frame can be
        // attributed to a pass; appended after the columns `samples_from_csv` reads.
        let g = &s.gpu;
        csv.pop();
        let passes = [
            g.shadows_ms,
            g.gi_ms,
            g.prepass_ms,
            g.cull_ms,
            g.ssao_ms,
            g.contact_shadows_ms,
            g.forward_ms,
            g.shared_forward_ms,
            g.ssgi_ms,
            g.ssr_ms,
            g.sssr_trace_ms,
            g.sssr_denoise_ms,
            g.atmosphere_ms,
            g.clouds_ms,
            g.fog_ms,
            g.scene_ms,
            g.taau_ms,
            g.post_ms,
        ];
        for ms in passes {
            let _ = write!(csv, ",{ms:.3}");
        }
        csv.push('\n');
    }
    let csv_path = dir.join(format!("{}.csv", run.map));
    std::fs::write(&csv_path, csv).map_err(io(&csv_path))?;
    let txt_path = dir.join(format!("{}-report.txt", run.map));
    std::fs::write(&txt_path, text).map_err(io(&txt_path))?;
    crate::soak_plot::write_timeline(
        &dir.join(format!("{}.png", run.map)),
        &run.samples,
        report.p50_ms,
        2400,
    )
}

/// Reads a timeline written by [`write_timeline_files`] back into samples (the columns the
/// plot and the report use), to re-plot an old run.
///
/// # Errors
///
/// The file is unreadable or a row is malformed.
pub fn samples_from_csv(path: &std::path::Path) -> Result<Vec<crate::chase::ChaseSample>, LookdevError> {
    let text = std::fs::read_to_string(path).map_err(|source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().unwrap_or_default().split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name);
    let bad = |what: &str| LookdevError::Config(format!("{}: {what}", path.display()));
    let wall = col("wall_ms").ok_or_else(|| bad("no wall_ms column"))?;
    let mut out = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let cells: Vec<&str> = line.split(',').collect();
        let f = |i: Option<usize>| {
            i.and_then(|i| cells.get(i))
                .and_then(|c| c.parse::<f32>().ok())
                .unwrap_or(0.0)
        };
        let u = |name: &str| {
            col(name)
                .and_then(|i| cells.get(i))
                .and_then(|c| c.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let mut s = crate::chase::ChaseSample {
            wall_ms: cells
                .get(wall)
                .and_then(|c| c.parse().ok())
                .ok_or_else(|| bad("malformed wall_ms"))?,
            cpu_ms: f(col("cpu_ms")),
            encode_ms: f(col("encode_ms")),
            submit_ms: f(col("submit_ms")),
            scatter_ms: f(col("scatter_ms")),
            scatter_update_ms: f(col("scatter_update_ms")),
            contact_ms: f(col("contact_ms")),
            collect_ms: f(col("collect_ms")),
            wait_ms: f(col("wait_ms")),
            runq_ms: f(col("runq_ms")),
            runq_workers_ms: f(col("runq_workers_ms")),
            psi_cpu_ms: f(col("psi_cpu_ms")),
            psi_mem_ms: f(col("psi_mem_ms")),
            faults: u("faults") as u32,
            major_faults: u("major_faults") as u32,
            spawn_ms: f(col("spawn_ms")),
            spawn_major_faults: u("spawn_major_faults") as u32,
            canary_us: f(col("canary_us")),
            rocks: u("rocks") as u32,
            ..crate::chase::ChaseSample::default()
        };
        s.gpu.frame_ms = f(col("gpu_ms"));
        s.counters.pipelines = u("pipelines");
        s.counters.bind_groups = u("bind_groups");
        s.counters.buffers = u("buffers");
        s.counters.textures = u("textures");
        s.shared_upload.1 = u("terrain_kib") * 1024;
        let events = col("events").and_then(|i| cells.get(i)).copied().unwrap_or("");
        for (tag, bit) in [
            ("cut", EVENT_CUT),
            ("tod", EVENT_TIME_OF_DAY),
            ("spawn", EVENT_SPAWN),
        ] {
            if events.split('+').any(|t| t == tag) {
                s.events |= bit;
            }
        }
        out.push(s);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_warmup_zero_run_reports_the_verdict_as_not_applicable() {
        let text =
            "map hitch report\n  verdict: FAIL (specs/27 § 2.4): 4 bind-group creations\n  next line\n";
        let cold = verdict_for_warmup(text, 0);
        assert!(cold.contains("verdict: n/a (no warm-up frames"));
        assert!(!cold.contains("FAIL"));
        assert!(cold.contains("map hitch report\n") && cold.ends_with("  next line\n"));
        assert_eq!(verdict_for_warmup(text, 30), text);
    }

    fn script(options: SoakOptions) -> SoakScript {
        let cams = [([0.0, 50.0, 0.0], [100.0, 0.0, 0.0], 60.0)];
        SoakScript::new(
            &[[-500.0, 0.0], [500.0, 0.0]],
            1400.0,
            &cams,
            (20.0, 100.0),
            options,
        )
    }

    #[test]
    fn spikes_go_to_the_phase_that_overran() {
        let base = crate::chase::ChaseSample {
            wall_ms: 4.0,
            encode_ms: 1.5,
            submit_ms: 1.5,
            scatter_ms: 0.2,
            wait_ms: 0.1,
            ..crate::chase::ChaseSample::default()
        };
        let mut samples = vec![base; 100];
        // The render thread waited 6 ms for a core.
        samples[10].wall_ms = 10.0;
        samples[10].runq_ms = 6.0;
        // An encode worker was preempted: a long submit, no wait on the render thread.
        samples[20].wall_ms = 9.5;
        samples[20].submit_ms = 6.0;
        samples[20].runq_workers_ms = 5.0;
        // The engine: a long submit with no run-queue wait anywhere.
        samples[30].wall_ms = 9.0;
        samples[30].submit_ms = 6.5;
        // Scatter on a cut.
        samples[40].wall_ms = 9.0;
        samples[40].scatter_ms = 5.2;
        samples[40].events = EVENT_CUT;
        for sample in &mut samples {
            sample.canary_us = 20.0;
        }
        // An encode spike on a core running at half speed.
        samples[50].wall_ms = 9.0;
        samples[50].encode_ms = 6.5;
        samples[50].canary_us = 41.0;
        let causes = spike_causes(&samples, 4.0);
        assert_eq!(causes.spikes, 5);
        assert_eq!(causes.contended, 1, "{causes:?}");
        assert_eq!(
            (causes.machine, causes.submit, causes.scatter),
            (2, 1, 1),
            "{causes:?}"
        );
        assert_eq!(causes.at_events, 1);
        assert_eq!((causes.engine(), causes.engine_at_events), (2, 1));
        // Frames 30, 40 and 50 stay over 8 ms with the waits taken out; 10 and 20 do not.
        assert_eq!(causes.without_runq, 3);
    }

    #[test]
    fn the_long_tour_covers_the_map_and_the_drive() {
        let s = script(SoakOptions::LONG);
        assert!(s.length_m >= 600.0 * SPEED * 0.95, "{}", s.length_m);
        let (min, max) = s.tour.iter().fold(([f32::MAX; 2], [f32::MIN; 2]), |(lo, hi), p| {
            (
                [lo[0].min(p[0]), lo[1].min(p[1])],
                [hi[0].max(p[0]), hi[1].max(p[1])],
            )
        });
        // Every region: the tour spans ≥ 80 % of the map on both axes.
        assert!(max[0] - min[0] >= 1400.0 * 0.8 && max[1] - min[1] >= 1400.0 * 0.8);
        assert_eq!(s.frames(), 36_000);
    }

    #[test]
    fn events_fire_on_schedule() {
        let s = script(SoakOptions::MINUTE);
        let frames: Vec<SoakPose> = (0..s.frames()).map(|i| s.pose(i)).collect();
        let count = |bit: u8| frames.iter().filter(|p| p.events & bit != 0).count();
        // Cuts in and out at 8, 10, 16, 18 … 56, 58 s: 14 transitions.
        assert_eq!(count(EVENT_CUT), 14);
        assert_eq!(count(EVENT_TIME_OF_DAY), 5);
        assert_eq!(count(EVENT_SPAWN), 19);
        assert!(frames[0].camera.is_none());
        assert!(frames[(8.5 / DT) as usize].camera.is_some());
        assert_ne!(frames[0].sun_toward, frames[(10.5 / DT) as usize].sun_toward);
    }

    #[test]
    fn the_script_is_deterministic_and_ping_pongs() {
        let s = script(SoakOptions::MINUTE);
        assert_eq!(s.pose(1234), s.pose(1234));
        let end = s.length_m / SPEED;
        let a = s.tour_s(end - 1.0);
        let b = s.tour_s(end + 1.0);
        assert!((a - b).abs() < 1e-2, "{a} {b}");
    }
}
