// SPDX-License-Identifier: MIT-0
//! `do27_chase --memory-json`: what one chase run leaves for the feature-cost bench
//! (`xtask gpu-bench --suite features`): the renderer's GPU memory after the last frame, in the
//! fields `seeing.memory_report` returns, and the heap allocations per measured frame.
//!
//! The allocations are counted only when the binary installs
//! [`regia_core::memory::TaggingAllocator`] (`do27_chase` built with `--features alloc-count`);
//! otherwise they are `null`. They are process-wide (every thread), between the end of the
//! warm-up and the end of the last measured frame; a run without warm-up frames has no frame to
//! open the window after (content load and setup would count), so its rate is `null` too.

use std::path::Path;

/// Counts heap allocations across the measured frames, fed from `chase::run_with`'s per-frame
/// callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocWindow {
    /// Frames rendered before measuring.
    warmup: u32,
    /// Measured frames.
    frames: u32,
    /// Allocation total when the window opened.
    start: Option<u64>,
    /// Allocation total when the window closed.
    end: Option<u64>,
}

impl AllocWindow {
    /// A window over frames `warmup..warmup + frames`. It opens after the last warm-up frame, so
    /// with no warm-up it never opens (setup would count as frame allocations).
    #[must_use]
    pub fn new(warmup: u32, frames: u32) -> Self {
        Self {
            warmup,
            frames,
            start: None,
            end: None,
        }
    }

    /// Called after frame `index` (warm-up included) with the allocation total then. Returns
    /// true on the last measured frame, when the window closes.
    pub fn after_frame(&mut self, index: u32, total: u64) -> bool {
        if self.warmup > 0 && index + 1 == self.warmup {
            self.start = Some(total);
        }
        let last = (self.warmup + self.frames).saturating_sub(1);
        if self.frames > 0 && index == last {
            self.end = Some(total);
            return true;
        }
        false
    }

    /// Allocations per measured frame, once the window has closed.
    #[must_use]
    pub fn per_frame(&self) -> Option<f64> {
        let (start, end) = (self.start?, self.end?);
        (self.frames > 0).then(|| end.saturating_sub(start) as f64 / f64::from(self.frames))
    }

    /// Allocations over the whole window, once it has closed.
    #[must_use]
    pub fn total(&self) -> Option<u64> {
        Some(self.end?.saturating_sub(self.start?))
    }
}

/// Allocations made since start-up, all subsystems; `None` without a tagging allocator.
#[must_use]
pub fn total_allocations() -> Option<u64> {
    let snapshot = regia_core::memory::snapshot();
    snapshot.tracking.then(|| {
        snapshot
            .subsystems
            .iter()
            .map(|(_, use_)| use_.total_allocations)
            .sum()
    })
}

/// The `--memory-json` document: `seeing.memory_report`'s fields (`available`, `backend`,
/// `allocated_bytes`, `reserved_bytes`, `rss_bytes`, `categories`, the `top` largest `labels`),
/// plus `allocs_per_frame`, `allocs_total`, `measured_frames` and `features` (the `--on` list).
#[must_use]
pub fn memory_json(
    report: &regia_render::MemoryReport,
    window: Option<&AllocWindow>,
    features: &[String],
    top: usize,
) -> serde_json::Value {
    let categories: serde_json::Map<String, serde_json::Value> = report
        .categories
        .iter()
        .map(|(category, total)| {
            (
                category.name().to_owned(),
                serde_json::json!({ "bytes": total.bytes, "count": total.count }),
            )
        })
        .collect();
    let labels: Vec<serde_json::Value> = report
        .labels
        .iter()
        .take(top)
        .map(|l| {
            serde_json::json!({
                "label": l.label,
                "category": l.category.name(),
                "bytes": l.bytes,
                "count": l.count,
            })
        })
        .collect();
    serde_json::json!({
        "schema": 1,
        "available": report.available,
        "backend": report.backend,
        "allocated_bytes": report.allocated_bytes,
        "reserved_bytes": report.reserved_bytes,
        "rss_bytes": report.rss_bytes,
        "categories": categories,
        "labels": labels,
        "features": features,
        "measured_frames": window.map(|w| w.frames),
        "allocs_total": window.and_then(AllocWindow::total),
        "allocs_per_frame": window.and_then(AllocWindow::per_frame),
    })
}

/// Writes `doc` to `path` (pretty, final newline) through a temporary file and a rename.
///
/// # Errors
///
/// The file could not be written.
pub fn write_json(path: &Path, doc: &serde_json::Value) -> Result<(), crate::LookdevError> {
    let mut text = serde_json::to_string_pretty(doc).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|source| crate::LookdevError::Io {
            path: path.display().to_string(),
            source,
        })
}

/// The surface `do27_chase --on wet` shades with: the full water film, full hollows and full
/// ripples (spec 99 § 6, the surface response at its most work), no snow. DO27's maps are dry;
/// this is a cost probe, not the shell's frame.
pub const WET_SURFACE: regia_render::SurfaceWeatherFrame = regia_render::SurfaceWeatherFrame {
    wetness: 1.0,
    puddles: 1.0,
    snow_cover: 0.0,
    ripple: 1.0,
    clock_s: 0.0,
};

/// The precipitation state of `weather` at `tier`'s budget, stepped once: on, the occlusion view
/// asked for, the preset's mean wind carrying the drops.
#[must_use]
pub fn precip_state(
    weather: &regia_vfx::WeatherState,
    tier: regia_render::QualityTier,
) -> regia_vfx::precip::PrecipState {
    let config = regia_vfx::WeatherConfig::default();
    let settings = regia_vfx::PrecipitationSettings {
        enabled: true,
        ..regia_vfx::PrecipitationSettings::from_config(&config)
    };
    let rates = weather.rates(&config);
    let wind = weather.wind_m_per_s;
    let mut integrator = regia_vfx::PrecipIntegrator::default();
    integrator.step(
        &rates,
        &config,
        wind,
        &regia_vfx::PrecipBudget::for_quality(tier),
        1.0 / 60.0,
    );
    regia_vfx::precip::PrecipState {
        integrator,
        rates,
        config,
        settings,
        wind_m_per_s: [wind[0], 0.0, wind[1]],
        quality: tier,
        active: true,
        ..regia_vfx::precip::PrecipState::default()
    }
}

/// The weather `do27_chase --on rain` (spec 99's `storm`) or `--on snow` (`blizzard`) draws: the
/// runtime's precipitation layer at the tier's budget, its occlusion view, and the preset's wind,
/// stepped on the chase's fixed time step. DO27's maps have no rain; this is a cost probe.
pub struct OnPrecipitation {
    handle: regia_vfx::PrecipitationHandle,
    budget: regia_vfx::PrecipBudget,
    layer: std::sync::Arc<dyn regia_render::translucent::TranslucentLayer>,
}

impl OnPrecipitation {
    /// The layer for `weather` at the tier of `settings` (`regia_vfx::precip::precipitation_tier`).
    ///
    /// # Errors
    ///
    /// The VFX or precipitation shaders failed to build.
    pub fn new(
        device: &regia_render::RenderDevice,
        weather: &regia_vfx::WeatherState,
        settings: &regia_render::RenderSettings,
    ) -> Result<Self, crate::LookdevError> {
        let tier = regia_vfx::precip::precipitation_tier(Some(settings));
        let profile = regia_vfx::VfxProfile::desktop_high();
        let failed = |e: regia_vfx::VfxError| crate::LookdevError::Config(format!("--on rain/snow: {e:?}"));
        let gpu = regia_vfx::VfxGpu::new(device, profile).map_err(failed)?;
        let precip = regia_vfx::PrecipitationGpu::new(device, &profile).map_err(failed)?;
        let handle = regia_vfx::PrecipitationHandle::default();
        if let Ok(mut slot) = handle.state.lock() {
            *slot = precip_state(weather, tier);
        }
        if let Ok(mut slot) = handle.gpu.lock() {
            *slot = Some(precip);
        }
        let layer = regia_vfx::runtime::translucent_layer_with_precipitation(
            std::sync::Arc::new(std::sync::Mutex::new(gpu)),
            handle.clone(),
            device.queue.clone(),
        );
        Ok(Self {
            handle,
            budget: regia_vfx::PrecipBudget::for_quality(tier),
            layer,
        })
    }

    /// The translucent layer to add to the frame renderer's list.
    #[must_use]
    pub fn layer(&self) -> std::sync::Arc<dyn regia_render::translucent::TranslucentLayer> {
        std::sync::Arc::clone(&self.layer)
    }

    /// The tier's occlusion view `(texels, extent m)` for
    /// `FrameRenderer::set_precipitation_occlusion_budget`.
    #[must_use]
    pub fn occlusion_budget(&self) -> Option<(u32, f32)> {
        self.budget.occlusion
    }

    /// Advances the fall offsets, the drift and the clock by `dt` seconds, as
    /// `regia_vfx::precip::step_precipitation` does each `Update`.
    pub fn step(&self, dt: f32) {
        if let Ok(mut state) = self.handle.state.lock() {
            let state = &mut *state;
            let wind = [state.wind_m_per_s[0], state.wind_m_per_s[2]];
            state
                .integrator
                .step(&state.rates, &state.config, wind, &self.budget, dt);
            state.clock_s += f64::from(dt);
            state.since_draw_s += dt;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storm_and_blizzard_states_fall_and_ask_for_the_occlusion_view() {
        let storm = precip_state(&regia_vfx::WeatherState::STORM, regia_render::QualityTier::High);
        assert!(storm.active);
        assert!(storm.rates.rain_mm_per_h > 0.0, "{:?}", storm.rates);
        assert!(storm.occlusion_request().is_some());
        assert_eq!(storm.wind_m_per_s, [18.0, 0.0, 6.0]);
        let blizzard = precip_state(
            &regia_vfx::WeatherState::BLIZZARD,
            regia_render::QualityTier::Ultra,
        );
        assert!(blizzard.rates.snow_we_mm_per_h > 0.0, "{:?}", blizzard.rates);
        assert!(blizzard.occlusion_request().is_some());
        assert_eq!(blizzard.quality, regia_render::QualityTier::Ultra);
        // Deterministic: the same inputs give the same state.
        let again = precip_state(&regia_vfx::WeatherState::STORM, regia_render::QualityTier::High);
        assert_eq!(again.integrator, storm.integrator);
    }

    #[test]
    fn on_names_the_weather_probes() {
        let names = |list: &[&str]| list.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        let mut quality = regia_render::FrameQuality::LAB;
        let before = quality;
        let rain = crate::chase::apply_on(&names(&["rain", "wet"]), &mut quality).expect("rain");
        assert_eq!(rain.precipitation, Some(regia_vfx::WeatherState::STORM));
        assert!(rain.wet);
        // The weather probes are frame inputs, not quality switches.
        assert_eq!(quality, before);
        let snow = crate::chase::apply_on(&names(&["snow", "snow"]), &mut quality).expect("snow");
        assert_eq!(snow.precipitation, Some(regia_vfx::WeatherState::BLIZZARD));
        assert!(!snow.wet);
        assert!(crate::chase::apply_on(&names(&["rain", "snow"]), &mut quality).is_err());
        assert!(crate::chase::apply_on(&names(&["vfx-layer", "rain"]), &mut quality).is_err());
        let layer = crate::chase::apply_on(&names(&["vfx-layer"]), &mut quality).expect("layer");
        assert_eq!(layer.precipitation, Some(regia_vfx::WeatherState::CLEAR));
        assert!(crate::chase::apply_on(&names(&["hail"]), &mut quality).is_err());
        let plain = crate::chase::apply_on(&[], &mut quality).expect("plain");
        assert_eq!(plain, crate::chase::OnSwitches::default());
        for name in ["rain", "snow", "vfx-layer", "wet"] {
            assert!(crate::chase::ON_FEATURES.contains(&name), "{name}");
        }
    }

    #[test]
    fn the_clear_layer_draws_nothing_and_asks_for_no_view() {
        let clear = precip_state(&regia_vfx::WeatherState::CLEAR, regia_render::QualityTier::High);
        assert!(clear.rates.is_none(), "{:?}", clear.rates);
        assert!(clear.occlusion_request().is_none());
    }

    #[test]
    fn the_wet_probe_is_wet_and_snowless() {
        assert!(!WET_SURFACE.is_dry());
        assert_eq!(WET_SURFACE.snow_cover, 0.0);
    }

    #[test]
    fn window_opens_after_the_warmup_and_closes_on_the_last_frame() {
        let mut w = AllocWindow::new(2, 3);
        assert!(!w.after_frame(0, 100));
        assert!(!w.after_frame(1, 150)); // end of warm-up: opens at 150
        assert!(!w.after_frame(2, 160));
        assert!(!w.after_frame(3, 170));
        assert!(w.after_frame(4, 180));
        assert_eq!(w.total(), Some(30));
        assert_eq!(w.per_frame(), Some(10.0));
    }

    #[test]
    fn window_without_warmup_has_no_rate() {
        // Nothing marks the end of setup, so content load would count as frame allocations.
        let mut w = AllocWindow::new(0, 2);
        assert!(!w.after_frame(0, 44));
        assert!(w.after_frame(1, 48));
        assert_eq!(w.per_frame(), None);
        assert_eq!(w.total(), None);
    }

    #[test]
    fn an_unclosed_or_empty_window_has_no_rate() {
        let mut w = AllocWindow::new(1, 4);
        w.after_frame(0, 10);
        w.after_frame(1, 12);
        assert_eq!(w.per_frame(), None);
        let mut empty = AllocWindow::new(0, 0);
        assert!(!empty.after_frame(0, 9));
        assert_eq!(empty.per_frame(), None);
    }

    #[test]
    fn memory_json_carries_the_report_fields_and_the_rate() {
        let report = regia_render::MemoryReport::from_allocations(
            "Vulkan",
            [("shadow atlas", 4096u64), ("hdr colour", 1024)],
            8192,
        );
        let mut w = AllocWindow::new(1, 2);
        w.after_frame(0, 0);
        w.after_frame(1, 3);
        w.after_frame(2, 6);
        let doc = memory_json(&report, Some(&w), &["oit".to_owned()], 1);
        assert_eq!(doc["schema"], 1);
        assert_eq!(doc["backend"], "Vulkan");
        assert_eq!(doc["allocated_bytes"], 5120);
        assert_eq!(doc["reserved_bytes"], 8192);
        assert_eq!(doc["labels"].as_array().map(Vec::len), Some(1));
        assert_eq!(doc["allocs_per_frame"], 3.0);
        assert_eq!(doc["features"][0], "oit");
        let untracked = memory_json(&report, None, &[], 20);
        assert!(untracked["allocs_per_frame"].is_null());
    }
}
