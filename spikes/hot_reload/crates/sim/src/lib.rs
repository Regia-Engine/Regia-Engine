//! A deterministic mock simulation running on its own OS thread at its own fixed rate, like DO27's
//! sim (specs/25 § 3). The presentation side only reads published values; nothing feeds back.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Simulation state: discs on springs. Serializable so it can be restarted from a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HoverSim {
    /// Ticks simulated so far.
    pub tick: u64,
    /// Disc positions (x, z).
    pub pos: Vec<[f32; 2]>,
    /// Disc velocities.
    pub vel: Vec<[f32; 2]>,
}

impl HoverSim {
    /// `n` discs in a deterministic start layout.
    pub fn new(n: usize) -> Self {
        Self {
            tick: 0,
            pos: (0..n).map(|i| [i as f32 * 0.5, (i % 7) as f32]).collect(),
            vel: vec![[0.0, 0.0]; n],
        }
    }

    /// One fixed step.
    pub fn step(&mut self, dt: f32) {
        for (p, v) in self.pos.iter_mut().zip(&mut self.vel) {
            v[0] -= p[0] * 4.0 * dt;
            v[1] -= p[1] * 4.0 * dt;
            p[0] += v[0] * dt;
            p[1] += v[1] * dt;
        }
        self.tick += 1;
    }

    /// Order-dependent checksum of the state (for determinism checks).
    pub fn checksum(&self) -> u64 {
        self.pos.iter().chain(&self.vel).fold(self.tick, |h, v| {
            h.rotate_left(5) ^ u64::from(v[0].to_bits()) ^ (u64::from(v[1].to_bits()) << 32)
        })
    }

    /// Serialized state.
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).unwrap_or_default()
    }

    /// State from [`HoverSim::to_bytes`].
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }
}

/// Values the sim thread publishes for presentation.
#[derive(Debug, Default)]
pub struct Published {
    /// Latest tick.
    pub tick: AtomicU64,
    /// Checksum at that tick.
    pub checksum: AtomicU64,
}

/// A running simulation thread. Dropping it stops and joins the thread.
pub struct SimHandle {
    stop: Arc<AtomicBool>,
    published: Arc<Published>,
    thread: Option<JoinHandle<HoverSim>>,
    /// When this handle started the thread.
    pub started: Instant,
}

impl SimHandle {
    /// Starts `sim` on a new thread stepping at `hz` on its own clock.
    pub fn start(sim: HoverSim, hz: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let published = Arc::new(Published::default());
        published.tick.store(sim.tick, Ordering::Release);
        let (s, p) = (stop.clone(), published.clone());
        let thread = std::thread::Builder::new()
            .name("hover-sim".into())
            .spawn(move || {
                let mut sim = sim;
                let period = Duration::from_secs_f64(1.0 / f64::from(hz.max(1)));
                let dt = period.as_secs_f32();
                let mut next = Instant::now();
                while !s.load(Ordering::Acquire) {
                    sim.step(dt);
                    p.checksum.store(sim.checksum(), Ordering::Relaxed);
                    p.tick.store(sim.tick, Ordering::Release);
                    next += period;
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else {
                        next = now; // fell behind (debugger, suspend): do not spiral
                    }
                }
                sim
            })
            .ok();
        Self {
            stop,
            published,
            thread,
            started: Instant::now(),
        }
    }

    /// Latest published tick.
    pub fn tick(&self) -> u64 {
        self.published.tick.load(Ordering::Acquire)
    }

    /// Latest published checksum.
    pub fn checksum(&self) -> u64 {
        self.published.checksum.load(Ordering::Relaxed)
    }

    /// Stops the thread and returns the final state (specs/20 § 4: join in `on_unload`).
    pub fn stop_join(mut self) -> Option<HoverSim> {
        self.stop.store(true, Ordering::Release);
        self.thread.take().and_then(|t| t.join().ok())
    }
}

impl Drop for SimHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
