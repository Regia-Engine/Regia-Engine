// SPDX-License-Identifier: MPL-2.0
//! The forest flythrough on the engine's ECS frame (ADR-1503): a [`ForestFlight`] resource the
//! agent methods `terrain.forest_*` read and write, and [`forest_flight_system`] (added by
//! [`ForestFlightPlugin`]) that applies it to the [`RuntimeScatter`] field and the extracted
//! frame once per frame:
//!
//! - installs a carved set (a corridor carve the agent applied) by rebuilding the field with the
//!   same terrain, meshes, impostor and detail settings;
//! - sets the requested [`MotionLod`] on the field (it takes effect at the field's next update);
//! - pushes the primary view's eye onto the camera wake and sways the scatter draws in it
//!   (fixed steps of `1 / fps`, so an offline render sways the same way every run);
//! - optionally meters pops on the extracted scatter draws.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_render::{ExtractedFrame, LodViewport};

use super::super::ScatterError;
use super::super::field::{ScatterField, ScatterLibrary};
use super::super::runtime::{RuntimeScatter, extract_runtime_scatter, scatter_view_of};
use super::super::set::ScatterSet;
use super::motion::MotionLod;
use super::pops::{PopMeter, PopStats};
use super::query::ScatterQuery;
use super::wake::{SwayField, SwaySettings, SwayStats, TrackWake, WakeModel, WakeSample};

/// What the last frames of the flythrough did.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ForestFlightStats {
    /// Frames processed.
    pub frames: u64,
    /// Motion LOD in effect on the field.
    pub motion: Option<MotionLod>,
    /// Last frame's sway step.
    pub sway: SwayStats,
    /// Draws leaned last frame.
    pub leaned: u32,
    /// Last frame's pops (when metering).
    pub pops_frame: PopStats,
    /// Pops since metering started.
    pub pops_total: PopStats,
    /// Field rebuilds for carved sets.
    pub rebuilds: u32,
    /// Why the last rebuild failed, if it did.
    pub last_error: Option<String>,
}

/// The forest flythrough state (see the module docs). The host inserts it with
/// [`ForestFlight::with_query`] next to its [`RuntimeScatter`].
#[derive(Resource)]
pub struct ForestFlight {
    query: Option<Arc<ScatterQuery>>,
    library: Option<Arc<ScatterLibrary>>,
    key_base: u64,
    pending_set: Option<ScatterSet>,
    /// Requested speed-aware LOD (`None`: off).
    pub motion: Option<MotionLod>,
    /// Sway the scatter in the camera's wake (`None`: off).
    pub sway: Option<SwaySettings>,
    /// The wake model.
    pub wake: WakeModel,
    /// Frame rate the wake and sway step at (the output rate of an offline render).
    pub fps: f32,
    /// Meter pops on the extracted scatter draws.
    pub meter_pops: bool,
    /// Smallest projected radius a pop counts at, pixels.
    pub pop_threshold_px: f32,
    /// What the last frames did.
    pub stats: ForestFlightStats,
    track: TrackWake,
    sway_field: SwayField,
    meter: Option<PopMeter>,
    applied_motion: Option<Option<MotionLod>>,
}

impl Default for ForestFlight {
    fn default() -> Self {
        Self {
            query: None,
            library: None,
            key_base: 0,
            pending_set: None,
            motion: None,
            sway: None,
            wake: WakeModel::default(),
            fps: 60.0,
            meter_pops: false,
            pop_threshold_px: 2.0,
            stats: ForestFlightStats::default(),
            track: TrackWake::new(WakeModel::default()),
            sway_field: SwayField::new(SwaySettings::default()),
            meter: None,
            applied_motion: None,
        }
    }
}

impl ForestFlight {
    /// A flythrough over `query` (the same set and terrain as the host's field). `library` and
    /// `key_base` (the field's) let a carved set be installed; without them carves are reported
    /// but not installed.
    #[must_use]
    pub fn with_query(query: ScatterQuery, library: Option<Arc<ScatterLibrary>>, key_base: u64) -> Self {
        Self {
            query: Some(Arc::new(query)),
            library,
            key_base,
            ..Self::default()
        }
    }

    /// The placement query, when the host gave one.
    #[must_use]
    pub fn query(&self) -> Option<&ScatterQuery> {
        self.query.as_deref()
    }

    /// The mesh library carves are installed with (also gives `flight_check` the LOD switch
    /// sizes).
    #[must_use]
    pub fn library(&self) -> Option<&ScatterLibrary> {
        self.library.as_deref()
    }

    /// Whether a carved set can be installed into the running field.
    #[must_use]
    pub fn can_install(&self) -> bool {
        self.query.is_some() && self.library.is_some()
    }

    /// Replaces the query's set with `set` (a carve: same classes and meshes, more exclusions)
    /// and asks [`forest_flight_system`] to rebuild the running field with it.
    ///
    /// # Errors
    ///
    /// [`ScatterError::Class`] when there is no query, or `set` changes the classes or meshes.
    pub fn install_set(&mut self, set: ScatterSet) -> Result<(), ScatterError> {
        let Some(query) = self.query.as_mut() else {
            return Err(ScatterError::Class {
                class: String::new(),
                reason: "no scatter query: the host inserted ForestFlight without one".into(),
            });
        };
        Arc::make_mut(query).replace_set(set.clone())?;
        self.pending_set = Some(set);
        Ok(())
    }

    /// Whether a carved set waits for the next frame.
    #[must_use]
    pub fn has_pending_set(&self) -> bool {
        self.pending_set.is_some()
    }

    /// Restarts pop metering (counters to zero).
    pub fn reset_pops(&mut self) {
        self.meter = None;
        self.stats.pops_total = PopStats::default();
        self.stats.pops_frame = PopStats::default();
    }

    fn rebuild(&mut self, field: &mut ScatterField) {
        let Some(set) = self.pending_set.take() else {
            return;
        };
        let (Some(query), Some(library)) = (self.query.as_ref(), self.library.as_ref()) else {
            self.stats.last_error = Some("no mesh library: the carve cannot be installed".into());
            return;
        };
        match ScatterField::new(&set, query.source().clone(), library, self.key_base) {
            Ok(mut next) => {
                next.set_impostors(field.impostors());
                next.set_detail(field.detail());
                next.set_enabled(field.enabled());
                next.set_motion_lod(field.motion_lod());
                *field = next;
                self.stats.rebuilds += 1;
                self.stats.last_error = None;
            }
            Err(e) => self.stats.last_error = Some(e.to_string()),
        }
    }
}

/// `Render`, after [`extract_runtime_scatter`]: applies the [`ForestFlight`] (see the module
/// docs). Does nothing without the resource.
pub fn forest_flight_system(
    flight: Option<ResMut<ForestFlight>>,
    scatter: Option<ResMut<RuntimeScatter>>,
    viewport: Option<Res<LodViewport>>,
    extracted: Option<ResMut<ExtractedFrame>>,
) {
    let Some(mut flight) = flight else { return };
    let flight = &mut *flight;
    let _span = tracing::info_span!(target: "regia_terrain", "forest_flight").entered();
    if let Some(mut scatter) = scatter {
        let field = scatter.field_mut();
        flight.rebuild(field);
        if flight.applied_motion != Some(flight.motion) {
            field.set_motion_lod(flight.motion);
            flight.applied_motion = Some(flight.motion);
        }
        flight.stats.motion = field.motion_lod();
    }
    flight.stats.frames += 1;
    let Some(mut extracted) = extracted else { return };
    let (width, height) = viewport.map_or((1920, 1080), |v| (v.width, v.height));
    let Some(view) = scatter_view_of(&extracted, width, height) else {
        return;
    };
    let fps = if flight.fps.is_finite() && flight.fps > 0.0 {
        flight.fps
    } else {
        60.0
    };
    if let Some(settings) = flight.sway {
        if flight.track.model() != flight.wake {
            flight.track = TrackWake::new(flight.wake);
        }
        if flight.sway_field.settings() != settings.sanitized() {
            flight.sway_field = SwayField::new(settings);
        }
        flight.track.push(WakeSample {
            time_s: flight.stats.frames as f64 / f64::from(fps),
            position: view.eye,
        });
        let meshes = &mut extracted.meshes;
        flight.stats.sway = flight.sway_field.step_extracted(&flight.track, meshes, 1.0 / fps);
        flight.stats.leaned = flight.sway_field.apply_extracted(meshes);
    } else {
        flight.stats.sway = SwayStats::default();
        flight.stats.leaned = 0;
    }
    if flight.meter_pops {
        let threshold = flight.pop_threshold_px;
        let meter = flight
            .meter
            .get_or_insert_with(|| PopMeter::new(threshold, height as f32));
        flight.stats.pops_frame = meter.observe_extracted(&view, &extracted.meshes);
        flight.stats.pops_total = meter.total();
    }
}

/// Adds [`forest_flight_system`] to `Render` after [`extract_runtime_scatter`] (and before the
/// glass extraction, like it). Insert a [`ForestFlight`] to use it.
pub struct ForestFlightPlugin;

impl Plugin for ForestFlightPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Render,
            forest_flight_system
                .after(extract_runtime_scatter)
                .before(regia_render::extract_glass),
        );
    }
}
