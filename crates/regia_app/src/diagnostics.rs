// SPDX-License-Identifier: MPL-2.0
//! Frame diagnostics (`docs/specs/10-core.md` § 3.3).

use std::collections::VecDeque;
use std::time::Duration;

#[cfg(test)]
use std::cell::Cell;

use regia_ecs::prelude::*;

// Archetype headers the last `spawned_entity_count` walked. That is the live-set cost:
// one step per archetype, not one step per allocated entity index.
#[cfg(test)]
thread_local! {
    static ENTITY_COUNT_STEPS: Cell<u64> = const { Cell::new(0) };
}

use crate::schedules::Last;
use crate::time::RealTime;
use crate::{App, Plugin};

/// Frame statistics over a sliding window, updated in [`Last`] by [`DiagnosticsPlugin`].
#[derive(Resource, Debug, Clone, Default)]
pub struct Diagnostics {
    /// Average frames per second over the window.
    pub fps: f64,
    /// Average frame time over the window.
    pub frame_time_avg: Duration,
    /// Average of the slowest 1% of frame times in the window.
    pub frame_time_1pct_low: Duration,
    /// Frames per second at [`Diagnostics::frame_time_1pct_low`] ("1% low").
    pub fps_1pct_low: f64,
    /// Spawned entities in the main world.
    pub entity_count: u32,
    /// Draw calls submitted last frame; written by the renderer.
    pub draw_calls: u32,
    /// Live heap bytes, when the host installed `regia_core::memory::TaggingAllocator` as its
    /// global allocator (dev builds, tests, soaks); `None` otherwise. Per subsystem:
    /// `regia_core::memory::snapshot`.
    pub allocated_bytes: Option<u64>,
    /// Frames recorded since startup.
    pub frames: u64,
    window: VecDeque<Duration>,
    // Sum of `window`, in nanoseconds. Exact, so evicting a `Duration::MAX` sample keeps the
    // rest of the window. A saturating `Duration` accumulator would not.
    window_nanos: u128,
    // Reused by the 1% low selection. Capacity stays at the configured window.
    worst_scratch: Vec<Duration>,
    /// Wall time (sum of raw frame deltas) since the last summary was logged.
    since_log: Duration,
}

impl Diagnostics {
    /// Adds one frame time and recomputes the window statistics. Zero deltas (the first frame,
    /// the frame after a resume) are ignored.
    ///
    /// Once the window has reached `window_frames`, a further sample does not allocate. The
    /// average is a running sum of the samples. The 1% low is the mean of the slowest
    /// `len.div_ceil(100)` samples, selected into a buffer this value keeps.
    pub fn record_frame(&mut self, frame_time: Duration, window_frames: usize) {
        self.frames += 1;
        if frame_time.is_zero() {
            return;
        }
        let window_frames = window_frames.clamp(1, MAX_WINDOW_FRAMES);
        if self.window.capacity() < window_frames {
            self.window.reserve(window_frames - self.window.len());
        }
        while self.window.len() >= window_frames {
            if let Some(old) = self.window.pop_front() {
                let old_nanos = old.as_nanos();
                debug_assert!(self.window_nanos >= old_nanos);
                self.window_nanos = self.window_nanos.saturating_sub(old_nanos);
            }
        }
        self.window.push_back(frame_time);
        self.window_nanos += frame_time.as_nanos();
        let len = self.window.len();
        let total = nanos_to_duration_saturating(self.window_nanos);
        self.frame_time_avg = total / len as u32;
        self.fps = 1.0 / self.frame_time_avg.as_secs_f64();

        // Cleared first so `reserve` sizes the buffer, not `len + window`.
        self.worst_scratch.clear();
        if self.worst_scratch.capacity() < window_frames {
            self.worst_scratch.reserve(window_frames);
        }
        self.worst_scratch.extend(self.window.iter().copied());
        let worst = len.div_ceil(100);
        // Ascending order: index `len - worst` is the smallest of the slowest `worst` samples,
        // and everything after it is at least that large.
        let split_at = len - worst;
        self.worst_scratch.select_nth_unstable(split_at);
        let worst_nanos = self.worst_scratch[split_at..]
            .iter()
            .fold(0u128, |sum, sample| sum + sample.as_nanos());
        self.frame_time_1pct_low = nanos_to_duration_saturating(worst_nanos) / worst as u32;
        self.fps_1pct_low = 1.0 / self.frame_time_1pct_low.as_secs_f64();
    }

    /// Frame times currently in the window, oldest first.
    pub fn window(&self) -> impl Iterator<Item = Duration> + '_ {
        self.window.iter().copied()
    }

    /// One-line summary, as logged.
    pub fn summary(&self) -> String {
        let mut line = format!(
            "{:.1} fps (1% low {:.1}), frame {:.2} ms, {} entities, {} draw calls",
            self.fps,
            self.fps_1pct_low,
            self.frame_time_avg.as_secs_f64() * 1000.0,
            self.entity_count,
            self.draw_calls
        );
        if let Some(bytes) = self.allocated_bytes {
            line.push_str(&format!(
                ", {:.1} MiB allocated",
                bytes as f64 / (1024.0 * 1024.0)
            ));
        }
        line
    }
}

/// `Duration` from a nanosecond total. Totals past [`Duration::MAX`] saturate, which is what a
/// saturating sum of the same samples produces before it is divided by the sample count.
fn nanos_to_duration_saturating(nanos: u128) -> Duration {
    if nanos >= Duration::MAX.as_nanos() {
        return Duration::MAX;
    }
    let secs = u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX);
    let subsec = u32::try_from(nanos % 1_000_000_000).unwrap_or(0);
    Duration::new(secs, subsec)
}

/// Largest sliding window honored by [`Diagnostics::record_frame`].
pub const MAX_WINDOW_FRAMES: usize = 100_000;

/// Diagnostics settings.
#[derive(Resource, Debug, Clone)]
pub struct DiagnosticsConfig {
    /// Frames in the sliding window (default 300).
    pub window_frames: usize,
    /// Log [`Diagnostics::summary`] at this interval of real time; `None` (default) never logs.
    pub log_interval: Option<Duration>,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            window_frames: 300,
            log_interval: None,
        }
    }
}

/// Adds [`Diagnostics`] and the `Last` system that updates it.
#[derive(Default)]
pub struct DiagnosticsPlugin {
    /// Initial settings.
    pub config: DiagnosticsConfig,
}

impl Plugin for DiagnosticsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Diagnostics>()
            .insert_resource(self.config.clone())
            .add_systems(Last, update_diagnostics);
    }
}

// Live spawned entities. Each one sits in exactly one archetype, so this matches
// `Entities::count_spawned` without walking despawned index slots.
fn spawned_entity_count(world: &World) -> u32 {
    let archetypes = world.archetypes();
    // Counted inside the loop, so a test can tell this walk from a scan of the entity index.
    #[cfg(test)]
    let mut steps = 0u64;
    let total = archetypes
        .iter()
        .map(|archetype| {
            #[cfg(test)]
            {
                steps += 1;
            }
            u64::from(archetype.len())
        })
        .sum::<u64>();
    #[cfg(test)]
    ENTITY_COUNT_STEPS.set(steps);
    u32::try_from(total).unwrap_or(u32::MAX)
}

/// `Last` system: records the frame and counts live entities; logs a summary when configured.
pub fn update_diagnostics(world: &mut World) {
    let Some(delta) = world.get_resource::<RealTime>().map(RealTime::raw_delta) else {
        return;
    };
    let entity_count = spawned_entity_count(world);
    let config = world
        .get_resource::<DiagnosticsConfig>()
        .cloned()
        .unwrap_or_default();
    let Some(mut diagnostics) = world.get_resource_mut::<Diagnostics>() else {
        return;
    };
    diagnostics.record_frame(delta, config.window_frames);
    diagnostics.entity_count = entity_count;
    diagnostics.allocated_bytes = regia_core::memory::live_heap_bytes();
    // Wall time, not `RealTime::elapsed` (a sum of clamped deltas): a multi-second stall counts
    // in full, so the stall frame itself is logged.
    diagnostics.since_log = diagnostics.since_log.saturating_add(delta);
    if let Some(interval) = config.log_interval
        && diagnostics.since_log >= interval
    {
        diagnostics.since_log = Duration::ZERO;
        tracing::info!(target: "regia_app::diagnostics", "{}", diagnostics.summary());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_window_is_capped() {
        let mut d = Diagnostics::default();
        for _ in 0..10 {
            d.record_frame(Duration::from_millis(10), usize::MAX);
        }
        assert_eq!(d.window().count(), 10);
        assert!(d.window().count() <= MAX_WINDOW_FRAMES);
        // A full window stays at the cap. Filled directly: stepping 100k frames is not what
        // this test is about.
        let sample = Duration::from_millis(10);
        d.window = std::iter::repeat_n(sample, MAX_WINDOW_FRAMES).collect();
        d.window_nanos = (MAX_WINDOW_FRAMES as u128) * sample.as_nanos();
        d.record_frame(Duration::from_millis(20), usize::MAX);
        assert_eq!(d.window().count(), MAX_WINDOW_FRAMES);
        assert_eq!(d.window.back().copied(), Some(Duration::from_millis(20)));
    }

    #[test]
    fn window_average_and_one_percent_low() {
        let mut d = Diagnostics::default();
        for _ in 0..99 {
            d.record_frame(Duration::from_millis(10), 100);
        }
        d.record_frame(Duration::from_millis(50), 100);
        d.record_frame(Duration::ZERO, 100);
        assert_eq!(d.frames, 101);
        assert_eq!(d.window().count(), 100);
        assert_eq!(d.frame_time_avg, Duration::from_micros(10_400));
        assert_eq!(d.frame_time_1pct_low, Duration::from_millis(50));
        assert!((d.fps_1pct_low - 20.0).abs() < 1e-9);
        // The window slides.
        for _ in 0..100 {
            d.record_frame(Duration::from_millis(20), 100);
        }
        assert_eq!(d.frame_time_avg, Duration::from_millis(20));
        assert!((d.fps - 50.0).abs() < 1e-9);
        assert!(d.summary().starts_with("50.0 fps"));
    }

    #[test]
    fn huge_frame_times_saturate_instead_of_overflowing() {
        let mut d = Diagnostics::default();
        d.record_frame(Duration::MAX, 300);
        d.record_frame(Duration::MAX, 300);
        d.record_frame(Duration::from_nanos(1), 300);
        assert_eq!(d.window().count(), 3);
        assert!(d.frame_time_avg > Duration::from_secs(1));
        assert_eq!(d.frame_time_1pct_low, Duration::MAX);
    }

    #[test]
    fn sliding_off_a_max_frame_keeps_the_rest_of_the_window() {
        let mut d = Diagnostics::default();
        d.record_frame(Duration::MAX, 2);
        d.record_frame(Duration::from_nanos(1), 2);
        d.record_frame(Duration::from_millis(10), 2);
        assert_eq!(
            d.window().collect::<Vec<_>>(),
            [Duration::from_nanos(1), Duration::from_millis(10)]
        );
        let expected = (Duration::from_nanos(1) + Duration::from_millis(10)) / 2;
        assert_eq!(d.frame_time_avg, expected);
        // Two samples: the slowest hundredth is the single slower frame.
        assert_eq!(d.frame_time_1pct_low, Duration::from_millis(10));
    }

    #[test]
    fn entity_count_walks_archetypes_not_the_index_high_water_mark() {
        let mut world = World::new();
        #[derive(Component)]
        struct Marker;
        let spawned: Vec<_> = (0..4_000).map(|_| world.spawn(Marker).id()).collect();
        for entity in spawned.into_iter().skip(1) {
            world.despawn(entity);
        }
        let mut real = RealTime::default();
        real.advance_by(Duration::from_millis(16));
        world.insert_resource(real);
        world.init_resource::<Diagnostics>();
        world.insert_resource(DiagnosticsConfig::default());
        update_diagnostics(&mut world);

        let published = world.resource::<Diagnostics>().entity_count;
        let archetype_sum = world
            .archetypes()
            .iter()
            .fold(0u32, |sum, archetype| sum.saturating_add(archetype.len()));
        let high_water = world.entities().len();
        let archetypes = world.archetypes().len();
        assert_eq!(published, world.entities().count_spawned());
        assert_eq!(published, archetype_sum);
        let archetype_count = u64::try_from(archetypes).unwrap_or(u64::MAX);
        assert!(
            u64::from(high_water) > archetype_count + 1_000,
            "high water {high_water}, archetypes {archetypes}"
        );
        let steps = ENTITY_COUNT_STEPS.get();
        assert_eq!(
            steps, archetype_count,
            "entity count walked {steps} slots; the index high water is {high_water} and only \
             {archetypes} archetypes hold the live set"
        );
    }

    #[test]
    fn update_diagnostics_records_a_stall_unclamped() {
        let mut world = World::new();
        let mut real = RealTime::default();
        let start = std::time::Instant::now();
        real.tick(start);
        real.tick(start + Duration::from_millis(4700));
        world.insert_resource(real);
        world.init_resource::<Diagnostics>();
        update_diagnostics(&mut world);
        let d = world.resource::<Diagnostics>();
        assert_eq!(d.frame_time_1pct_low, Duration::from_millis(4700));
    }

    #[test]
    fn the_log_interval_counts_wall_time_across_a_stall() {
        let mut world = World::new();
        let mut real = RealTime::default();
        let start = std::time::Instant::now();
        real.tick(start);
        real.tick(start + Duration::from_millis(4700));
        world.insert_resource(real);
        world.init_resource::<Diagnostics>();
        world.insert_resource(DiagnosticsConfig {
            window_frames: 10,
            log_interval: Some(Duration::from_secs(1)),
        });
        // The clamped `elapsed` is 250 ms here; the wall time (4.7 s) passes the 1 s interval.
        update_diagnostics(&mut world);
        assert_eq!(world.resource::<Diagnostics>().since_log, Duration::ZERO);
    }
}
