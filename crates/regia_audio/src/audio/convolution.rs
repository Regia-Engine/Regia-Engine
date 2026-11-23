// SPDX-License-Identifier: MPL-2.0
//! Convolution on the audio system (ADR-2721): the registered impulse responses, the tier, the
//! bus convolvers and the zone-IR convolver.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::Audio;
use crate::bus::{ConvolutionContext, REVERB_BUS, SILENT_SEND_DB, ZONE_IR_BUS};
use crate::{
    AudioError, ConvolutionError, ConvolutionKernel, ConvolutionStats, ConvolutionTier,
    DEFAULT_IR_CROSSFADE_SECS, ImpulseResponse,
};

/// The convolution state of an [`Audio`].
#[derive(Default)]
pub(crate) struct ConvolutionState {
    tier: Option<ConvolutionTier>,
    irs: HashMap<String, ImpulseResponse>,
    kernels: HashMap<(String, u32), Arc<ConvolutionKernel>>,
    /// The IR the reverb zones ask for now (`None`: the preset reverb).
    zone_ir: Option<String>,
    /// The IR loaded into the zone convolver (it stays loaded after the listener leaves, so its
    /// tail rings out and re-entering costs nothing).
    zone_loaded: Option<String>,
    /// The zone IR changed since the reverb sends were last set: they crossfade.
    zone_flip: bool,
    zone_switches: u64,
    zone_fallbacks: u64,
    /// The IR the zones asked for last (the `AudioSync` system asks every frame; a fallback is
    /// counted once per change of request, not per frame).
    zone_requested: Option<String>,
}

/// One convolver's report (`audio.convolution_stats`).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ConvolutionReport {
    /// The bus (`ZoneIr` for the zone convolver).
    pub bus: String,
    /// Index among the bus's convolution effects.
    pub index: usize,
    /// The IR it was given (`None`: none).
    pub ir: Option<String>,
    /// The predelay it was given, seconds.
    pub predelay_secs: f32,
    /// The audio thread's counters.
    pub stats: ConvolutionStats,
}

/// A registered IR (`audio.bus_effects`, `audio.convolution_stats`).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ImpulseResponseInfo {
    /// The name it was registered under.
    pub name: String,
    /// Frames at its own rate.
    pub frames: usize,
    /// 1 or 2.
    pub channels: usize,
    /// Its own sample rate.
    pub sample_rate: u32,
    /// Length, seconds.
    pub duration_secs: f64,
}

impl Audio {
    /// The rate buses are built at now: a running mixdown's, else the output's.
    fn convolution_rate(&self) -> u32 {
        self.mixdown()
            .map_or(self.output_sample_rate, |job| job.sample_rate)
    }

    /// The kernel of IR `name` at `rate` (prepared once, then shared).
    fn kernel_at(&mut self, name: &str, rate: u32) -> Result<Arc<ConvolutionKernel>, ConvolutionError> {
        let key = (name.to_string(), rate);
        if let Some(kernel) = self.convolution.kernels.get(&key) {
            return Ok(Arc::clone(kernel));
        }
        let ir = self.convolution.irs.get(name).ok_or(ConvolutionError::Empty)?;
        let kernel = ConvolutionKernel::prepare(ir, rate, self.convolution_tier())?;
        self.convolution.kernels.insert(key, Arc::clone(&kernel));
        Ok(kernel)
    }

    /// The context a bus build at `rate` needs: every registered IR prepared at that rate.
    pub(crate) fn convolution_context(&mut self, rate: u32) -> ConvolutionContext {
        let names: Vec<String> = self.convolution.irs.keys().cloned().collect();
        let mut kernels = HashMap::new();
        for name in names {
            if let Ok(kernel) = self.kernel_at(&name, rate) {
                kernels.insert(name, kernel);
            }
        }
        ConvolutionContext {
            sample_rate: rate,
            tier: self.convolution_tier(),
            kernels,
        }
    }

    /// The bus layout the mixer was built from (`audio.bus_effects`).
    pub fn bus_layout(&self) -> &crate::BusLayout {
        &self.bus_layout
    }

    /// The convolution tier ([`ConvolutionTier::platform_default`] until set).
    pub fn convolution_tier(&self) -> ConvolutionTier {
        self.convolution
            .tier
            .unwrap_or_else(ConvolutionTier::platform_default)
    }

    /// Sets the convolution tier. IRs already registered stay registered; a kernel that no
    /// longer fits is refused when next prepared, and `Low` falls zones back to their preset.
    pub fn set_convolution_tier(&mut self, tier: ConvolutionTier) {
        self.convolution.tier = Some(tier);
        self.convolution.kernels.clear();
        if tier == ConvolutionTier::Low && self.convolution.zone_ir.is_some() {
            self.set_reverb_zone_ir(None);
        }
    }

    /// Registers `ir` under `name` (replacing one of the same name) after checking that it fits
    /// the tier at the output rate. Bus convolutions that name it and zones that name it pick it
    /// up: a playing convolver crossfades to the new IR.
    ///
    /// # Errors
    /// [`ConvolutionError::TooLong`] past the tier cap, [`ConvolutionError::TierDisabled`] on the
    /// low tier.
    pub fn register_impulse_response(
        &mut self,
        name: &str,
        ir: ImpulseResponse,
    ) -> Result<(), ConvolutionError> {
        let rate = self.convolution_rate();
        let kernel = ConvolutionKernel::prepare(&ir, rate, self.convolution_tier())?;
        self.convolution.kernels.retain(|(n, _), _| n != name);
        self.convolution
            .kernels
            .insert((name.to_string(), rate), Arc::clone(&kernel));
        self.convolution.irs.insert(name.to_string(), ir);
        let targets: Vec<(String, usize, f32)> = self
            .buses
            .convolutions()
            .filter(|(_, _, h)| h.ir_name() == Some(name))
            .map(|(bus, index, h)| (bus.to_string(), index, h.predelay_secs()))
            .collect();
        for (bus, index, predelay) in targets {
            if let Some(handle) = self.buses.convolution_mut(&bus, index)
                && let Err(error) = handle.set_kernel(
                    Some(name),
                    Some(Arc::clone(&kernel)),
                    predelay,
                    DEFAULT_IR_CROSSFADE_SECS,
                )
            {
                tracing::warn!(target: "regia_audio", "convolution on `{bus}`: {error}");
            }
        }
        if self.convolution.zone_loaded.as_deref() == Some(name) {
            self.convolution.zone_loaded = None;
        }
        if self.convolution.zone_ir.as_deref() == Some(name) {
            self.load_zone_ir();
        }
        Ok(())
    }

    /// Removes a registered IR; convolvers using it keep playing it until they are given another.
    pub fn unregister_impulse_response(&mut self, name: &str) -> bool {
        self.convolution.kernels.retain(|(n, _), _| n != name);
        self.convolution.irs.remove(name).is_some()
    }

    /// The registered IRs, sorted by name.
    pub fn impulse_responses(&self) -> Vec<ImpulseResponseInfo> {
        let mut out: Vec<ImpulseResponseInfo> = self
            .convolution
            .irs
            .iter()
            .map(|(name, ir)| ImpulseResponseInfo {
                name: name.clone(),
                frames: ir.frames(),
                channels: ir.channel_count(),
                sample_rate: ir.sample_rate(),
                duration_secs: ir.duration_secs(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Sends convolution `index` on `bus` a new IR (`None`: fade the wet path out) with
    /// `predelay_secs`, crossfading over `fade_secs`.
    ///
    /// # Errors
    /// [`AudioError::UnknownBus`] for a bus without that convolution, [`AudioError::UnknownEntry`]
    /// for an IR that is not registered, [`AudioError::Invalid`] (prefixed by the
    /// [`ConvolutionError::code`]) when the IR cannot be prepared or the convolver is busy.
    pub fn set_bus_convolution(
        &mut self,
        bus: &str,
        index: usize,
        ir: Option<&str>,
        predelay_secs: f32,
        fade_secs: f32,
    ) -> Result<(), AudioError> {
        if self.buses.convolution_mut(bus, index).is_none() {
            return Err(AudioError::UnknownBus(format!("{bus} (convolution {index})")));
        }
        let kernel = match ir {
            None => None,
            Some(name) => {
                if !self.convolution.irs.contains_key(name) {
                    return Err(AudioError::UnknownEntry(name.to_string()));
                }
                let fallback = self.convolution_rate();
                let rate = self
                    .buses
                    .convolution_mut(bus, index)
                    .map_or(fallback, |h| h.mixer_rate());
                Some(self.kernel_at(name, rate).map_err(convolution_error)?)
            }
        };
        let handle = self
            .buses
            .convolution_mut(bus, index)
            .ok_or_else(|| AudioError::UnknownBus(bus.to_string()))?;
        handle
            .set_kernel(ir, kernel, predelay_secs, fade_secs)
            .map_err(convolution_error)
    }

    /// Sets the wet/dry mix (0..1) of convolution `index` on `bus`.
    ///
    /// # Errors
    /// [`AudioError::UnknownBus`] for a bus without that convolution; [`AudioError::Invalid`] for
    /// a NaN or infinite mix.
    pub fn set_bus_convolution_mix(&mut self, bus: &str, index: usize, mix: f32) -> Result<(), AudioError> {
        if !mix.is_finite() {
            return Err(AudioError::Invalid(format!(
                "convolution mix must be finite, got {mix}"
            )));
        }
        let handle = self
            .buses
            .convolution_mut(bus, index)
            .ok_or_else(|| AudioError::UnknownBus(format!("{bus} (convolution {index})")))?;
        handle.set_mix(mix);
        Ok(())
    }

    /// Every convolver's report: the bus convolutions in layout order, then the zone convolver
    /// (bus [`ZONE_IR_BUS`]).
    pub fn convolution_stats(&mut self) -> Vec<ConvolutionReport> {
        let mut out: Vec<ConvolutionReport> = self
            .buses
            .convolutions()
            .map(|(bus, index, handle)| ConvolutionReport {
                bus: bus.to_string(),
                index,
                ir: handle.ir_name().map(str::to_string),
                predelay_secs: handle.predelay_secs(),
                stats: handle.stats(),
            })
            .collect();
        if let Some(handle) = self.buses.zone_convolver() {
            out.push(ConvolutionReport {
                bus: ZONE_IR_BUS.to_string(),
                index: 0,
                ir: handle.ir_name().map(str::to_string),
                predelay_secs: handle.predelay_secs(),
                stats: handle.stats(),
            });
        }
        out
    }

    /// The zone IR the listener's reverb zone asks for now (`None`: the preset reverb).
    pub fn reverb_zone_ir(&self) -> Option<&str> {
        self.convolution.zone_ir.as_deref()
    }

    /// How many times the zone IR changed, and how many zone IRs fell back to the preset (low
    /// tier, unregistered or refused IR).
    pub fn reverb_zone_ir_counts(&self) -> (u64, u64) {
        (self.convolution.zone_switches, self.convolution.zone_fallbacks)
    }

    /// Selects the impulse response the reverb zones play through (`None`: the zones' preset
    /// reverb). Called by the `AudioSync` system with the most specific zone around player 0
    /// before [`Audio::set_reverb_mix`]; a change crossfades the reverb sends (preset and IR) and,
    /// between two IRs, the convolver itself, over [`DEFAULT_IR_CROSSFADE_SECS`]. On the low tier,
    /// or for an IR that is not registered, the zone falls back to its preset.
    pub fn set_reverb_zone_ir(&mut self, ir: Option<&str>) {
        let changed_request = self.convolution.zone_requested.as_deref() != ir;
        if changed_request {
            self.convolution.zone_requested = ir.map(str::to_string);
        }
        let usable = ir.filter(|name| {
            let ok =
                self.convolution_tier() != ConvolutionTier::Low && self.convolution.irs.contains_key(*name);
            if !ok && changed_request {
                self.convolution.zone_fallbacks += 1;
            }
            ok
        });
        if usable == self.convolution.zone_ir.as_deref() {
            return;
        }
        self.convolution.zone_ir = usable.map(str::to_string);
        self.convolution.zone_switches += 1;
        self.convolution.zone_flip = true;
        if usable.is_some() {
            self.load_zone_ir();
        }
    }

    /// Loads the wanted zone IR into the zone convolver unless it is already there.
    fn load_zone_ir(&mut self) {
        let Some(name) = self.convolution.zone_ir.clone() else {
            return;
        };
        if self.convolution.zone_loaded.as_deref() == Some(name.as_str()) {
            return;
        }
        let fallback = self.convolution_rate();
        let rate = self.buses.zone_convolver().map_or(fallback, |h| h.mixer_rate());
        let kernel = match self.kernel_at(&name, rate) {
            Ok(kernel) => kernel,
            Err(error) => {
                tracing::warn!(target: "regia_audio", "zone IR `{name}`: {error}; using the preset");
                self.convolution.zone_fallbacks += 1;
                self.convolution.zone_ir = None;
                return;
            }
        };
        if let Some(handle) = self.buses.zone_convolver() {
            match handle.set_kernel(Some(&name), Some(kernel), 0.0, DEFAULT_IR_CROSSFADE_SECS) {
                Ok(()) => self.convolution.zone_loaded = Some(name),
                // Busy (a crossfade still running): the next frame tries again.
                Err(error) => tracing::debug!(target: "regia_audio", "zone IR `{name}`: {error}"),
            }
        }
    }

    /// The reverb send levels for `db`: `(into the preset reverb, into the zone IR)`, and the
    /// fade to use (longer when the zone IR just changed).
    pub(crate) fn zone_send_split(&mut self, db: f32) -> (f32, f32, Duration, Duration) {
        let fade = Duration::from_millis(120);
        let flip = std::mem::take(&mut self.convolution.zone_flip);
        let send_fade = if flip {
            Duration::from_secs_f32(DEFAULT_IR_CROSSFADE_SECS)
        } else {
            fade
        };
        if self.convolution.zone_ir.is_some() {
            (SILENT_SEND_DB, db, send_fade, send_fade)
        } else {
            (db, SILENT_SEND_DB, send_fade, send_fade)
        }
    }

    /// Whether the zone IR changed since the sends were last set.
    pub(crate) fn zone_flip_pending(&self) -> bool {
        self.convolution.zone_flip
    }

    /// Sets every reverb-feeding bus's send into the zone-IR bus.
    pub(crate) fn set_zone_ir_sends(&mut self, db: f32, fade: Duration) {
        for (bus, _) in self.buses.sends_into(REVERB_BUS) {
            self.buses.set_zone_ir_send(&bus, db, fade);
        }
    }

    /// Once a frame: frees retired slots, re-prepares kernels for a mixer that runs at another
    /// rate than the one they were built for, and retries a zone IR load that found the
    /// convolver busy.
    pub(crate) fn tick_convolution(&mut self) {
        self.buses.collect_convolution_garbage();
        let stale: Vec<(String, usize, String, f32, u32)> = self
            .buses
            .convolutions()
            .filter_map(|(bus, index, h)| {
                let name = h.ir_name()?;
                let rate = h.mixer_rate();
                (h.kernel_rate() != 0 && h.kernel_rate() != rate)
                    .then(|| (bus.to_string(), index, name.to_string(), h.predelay_secs(), rate))
            })
            .collect();
        for (bus, index, name, predelay, rate) in stale {
            let kernel = self.kernel_at(&name, rate);
            if let Some(handle) = self.buses.convolution_mut(&bus, index) {
                let result = kernel.and_then(|k| handle.set_kernel(Some(&name), Some(k), predelay, 0.0));
                if let Err(error) = result {
                    tracing::warn!(target: "regia_audio", "convolution on `{bus}` at {rate} Hz: {error}");
                }
            }
        }
        if let Some(handle) = self.buses.zone_convolver()
            && handle.kernel_rate() != 0
            && handle.kernel_rate() != handle.mixer_rate()
        {
            self.convolution.zone_loaded = None;
        }
        if self.convolution.zone_ir.is_some() && self.convolution.zone_loaded != self.convolution.zone_ir {
            self.load_zone_ir();
        }
    }

    /// After a bus rebuild the zone convolver is new and empty.
    pub(crate) fn reset_convolution_after_rebuild(&mut self) {
        self.convolution.zone_loaded = None;
        self.convolution.zone_flip = true;
    }
}

/// A [`ConvolutionError`] as an [`AudioError::Invalid`] that starts with its code.
fn convolution_error(error: ConvolutionError) -> AudioError {
    AudioError::Invalid(format!("{}: {error}", error.code()))
}
