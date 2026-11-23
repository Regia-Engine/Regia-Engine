// SPDX-License-Identifier: MPL-2.0
//! The Vehicle Tuning panel's model and agent surface (ADR-9760, `book/agents/vehicles.md`).
//!
//! [`VehicleTuning`] holds a `.vehicle.ron` being tuned: the asset as opened (and its text, so a save
//! keeps the hand comments), the edited asset, its undo and redo stacks, the recorded proving runs
//! and the play car's telemetry strip. The panel (`vehicle_tuning_panel`) and the agent methods
//! (`editor.vehicle_tuning_open`, `_state`, `_set`, `_undo`, `_redo`, `_apply`, `_save`, `_run`)
//! change the same resource, so what an agent sets is what the panel shows.
//!
//! - **Fields** are named by a dotted path into the configuration's serde shape: `chassis_mass`,
//!   `wheels.0.radius`, `drivetrain.gearbox.final_drive`, `drivetrain.engine.torque_curve.2`,
//!   `wheels.1.tire_model.cornering_stiffness`, `aero.drag_coefficient`, or `name`. An enum's variant
//!   is passed through (`drivetrain.gearbox` reads inside `Geared`), and a whole section can be set
//!   (`drivetrain: "Simple"`, `aero: null`). A value the configuration refuses changes nothing and
//!   answers the `VehicleError` code `physics.vehicle_config_check` uses.
//! - **Undo**: one step per field edit, and one per drag however many frames it lasts.
//! - **Apply** sends the edited configuration to a play vehicle through `Vehicle::set_config` at the
//!   editor's next pump: no respawn.
//! - **Runs** drive the edited car on the proving ground (`regia_physics::vehicle_proving`).
//! - **Save** writes `.vehicle.ron` v4 atomically, merging into the opened text so comments stay
//!   (`regia_ui::ron_merge::merge_ron_text`); when the merge cannot reproduce the asset it writes the
//!   canonical text and says the comments were lost.
//! - **Trailers and aircraft** (ADR-7745, [`assets`]): a `.trailer.ron` and a `.aircraft.ron` open
//!   beside the car with the same forms, undo and save (`asset: "trailer"` / `"aircraft"` on the
//!   methods). A run can tow the open trailer and drive on a generated track (`track`: flat, ramp,
//!   slope, bump_strip); `editor.vehicle_tuning_fly` trims the open aircraft and flies the trim.
//! - **Bikes and hover hulls** (ADR-8630): a `.bike.ron` and a `.hoverhull.ron` open the same way
//!   (`asset: "bike"` / `"hoverhull"`); `editor.vehicle_tuning_ride` spawns one on the proving ground
//!   and steps it a counted number of fixed steps. The riding bike or hover craft reads the document
//!   at every step, so an edit reaches it at its next step without a respawn.

use std::path::{Component, Path, PathBuf};

use regia::physics::PhysicsWorld;
use regia::physics::vehicle::{VehicleConfig, VehicleController};
use regia::physics::vehicle_asset::{VehicleAsset, VehicleAssetError};
use regia::physics::vehicle_proving::{
    MAX_PROVING_SECONDS, ProvingCourse, ProvingRun, ProvingScript, ProvingTrack, ProvingTrailerRun,
    run_proving_course,
};
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

#[path = "vehicle_tuning_assets.rs"]
pub mod assets;

use assets::{BikeRide, FlightRun, HoverRide, MAX_RIDE_STEPS, OtherDocs, RideInput};

/// The most undo steps kept.
pub const MAX_UNDO: usize = 256;

/// The most recorded runs kept; the oldest goes first.
pub const MAX_RUNS: usize = 8;

/// Frames of play telemetry the strip keeps.
pub const TELEMETRY_FRAMES: usize = 240;

/// Why a tuning action failed: a stable code (`book/agents/vehicles.md`) and a message.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TuningError {
    /// Agent error code: `vehicle_tuning_*`, or a `vehicle_*` code of the configuration.
    pub code: String,
    /// The field the refusal is about, when it is about one.
    pub field: Option<String>,
    /// What went wrong.
    pub message: String,
}

impl TuningError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            field: None,
            message: message.into(),
        }
    }

    fn asset(error: &VehicleAssetError) -> Self {
        let field = match error {
            VehicleAssetError::Config(
                regia::physics::vehicle::VehicleError::InvalidConfig { field }
                | regia::physics::vehicle::VehicleError::InvalidWheel { field, .. },
            ) => Some((*field).to_string()),
            _ => None,
        };
        Self {
            code: error.code().to_string(),
            field,
            message: error.to_string(),
        }
    }

    /// The error as an agent answer: `invalid_params` with `data.reason` set to the code.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        let mut err = RpcError::invalid_params(format!("[{}] {}", self.code, self.message));
        if let Some(data) = err.data.as_mut() {
            data["reason"] = json!(self.code);
            if let Some(field) = &self.field {
                data["field"] = json!(field);
            }
        }
        err
    }
}

/// A finished proving run the panel keeps for the side-by-side table.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct RecordedRun {
    /// The label shown in the table (`run N` unless given).
    pub label: String,
    /// The digest of the configuration the run drove.
    pub config_digest: String,
    /// The run.
    pub run: ProvingRun,
    /// The proving-ground track it drove (ADR-7745).
    pub track: ProvingTrack,
    /// The fewest and the most wheels on the ground on one step, and how many steps changed it.
    pub contacts: (u32, u32, u32),
    /// The highest the chassis rose above its start, meters.
    pub max_rise: f32,
    /// The towed trailer's run and the digest of the trailer configuration, when it towed one.
    pub trailer: Option<(ProvingTrailerRun, String)>,
}

impl RecordedRun {
    /// The run's metrics without the trace.
    #[must_use]
    pub fn metrics(&self) -> Value {
        let run = &self.run;
        json!({
            "label": self.label,
            "script": run.script.name(),
            "seconds": run.seconds,
            "zero_to_100_s": run.zero_to_100_s,
            "top_speed_kmh": run.top_speed_kmh,
            "max_lateral_g": run.max_lateral_g,
            "distance_m": run.distance_m,
            "upshifts": run.upshifts,
            "digest": format!("{:016x}", run.digest),
            "config_digest": self.config_digest,
            "track": self.track.name(),
            "min_contacts": self.contacts.0,
            "max_contacts": self.contacts.1,
            "contact_changes": self.contacts.2,
            "max_rise_m": self.max_rise,
            "trailer": self.trailer.as_ref().map(|(t, digest)| json!({
                "coupled": t.coupled,
                "max_articulation_deg": t.max_articulation.to_degrees(),
                "max_coupling_gap_m": t.max_coupling_gap,
                "chassis_mass": t.chassis_mass,
                "config_digest": digest,
            })),
        })
    }
}

/// One frame of the play car's telemetry strip.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct TelemetryFrame {
    /// The vehicle's step count.
    pub steps: u64,
    /// Speed in its own plane, km/h.
    pub speed_kmh: f32,
    /// Engine speed, rpm.
    pub rpm: f32,
    /// Engaged gear.
    pub gear: i32,
    /// Per wheel: `(slip ratio, slip angle rad, suspension load N)`.
    pub wheels: Vec<(f32, f32, f32)>,
}

/// The Vehicle Tuning panel's state.
#[derive(Resource, Debug, Default)]
pub struct VehicleTuning {
    /// The open file, relative to the project; `None` before an open.
    pub path: Option<String>,
    /// The asset as it was opened or last saved, at the file's own format version.
    pub opened: Option<VehicleAsset>,
    /// The file's text as it was opened or last saved (`None` for a created asset not saved yet).
    pub source: Option<String>,
    /// The asset being edited.
    pub asset: Option<VehicleAsset>,
    /// Earlier assets, newest last.
    pub undo: Vec<VehicleAsset>,
    /// Undone assets, newest last.
    pub redo: Vec<VehicleAsset>,
    /// The field a drag is editing: further edits of it join the same undo step.
    pub drag: Option<String>,
    /// The last refused edit or failed action.
    pub error: Option<TuningError>,
    /// What the last save or apply did, for the status line.
    pub notice: Option<String>,
    /// The last save kept the file's comments (`None` before a save).
    pub comments_kept: Option<bool>,
    /// The play vehicle Apply targets (entity bits); `None` picks the car this file spawned.
    pub target: Option<u64>,
    /// An Apply waiting for the next pump, with the vehicle it names.
    pub pending_apply: Option<Option<u64>>,
    /// The last Apply's answer or failure.
    pub last_apply: Option<Result<Value, TuningError>>,
    /// Recorded proving runs, oldest first.
    pub runs: Vec<RecordedRun>,
    /// The play car's telemetry, oldest first.
    pub telemetry: Vec<TelemetryFrame>,
    /// The vehicle the strip reads.
    pub telemetry_entity: Option<u64>,
    /// Which wheel the tire and suspension plots show.
    pub plot_wheel: usize,
    /// Runs started so far (names `run N`).
    pub run_count: u32,
    /// The proving-ground track the next run drives (ADR-7745).
    pub track: ProvingTrack,
    /// The next run tows the open trailer.
    pub tow: bool,
    /// The trailer and aircraft documents and the recorded flights.
    pub others: OtherDocs,
    shared: bool,
    shared_depths: (usize, usize),
    shared_gesture: u64,
    pending: Vec<VehicleEdit>,
    history_request: Option<bool>,
}

/// One step into a JSON value along a field path; an externally tagged enum's variant is passed
/// through when the step does not name it.
pub(crate) fn step<'a>(value: &'a mut Value, segment: &str) -> Option<&'a mut Value> {
    let through = matches!(value, Value::Object(map)
        if !map.contains_key(segment)
            && map.len() == 1
            && map.keys().all(|k| k.starts_with(|c: char| c.is_ascii_uppercase())));
    let value = if through {
        value.as_object_mut()?.values_mut().next()?
    } else {
        value
    };
    match value {
        Value::Object(map) => map.get_mut(segment),
        Value::Array(items) => segment.parse::<usize>().ok().and_then(|i| items.get_mut(i)),
        _ => None,
    }
}

/// 64-bit FNV-1a of `bytes` (a configuration digest for the run table).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Which open document an action is about (ADR-7745).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub enum AssetKind {
    /// The `.vehicle.ron`.
    #[default]
    Vehicle,
    /// The `.trailer.ron`.
    Trailer,
    /// The `.aircraft.ron`.
    Aircraft,
    /// The `.bike.ron` (ADR-8630).
    Bike,
    /// The `.hoverhull.ron` (ADR-8630).
    HoverHull,
}

impl AssetKind {
    /// Every kind, in the panel's order.
    pub const ALL: [Self; 5] = [
        Self::Vehicle,
        Self::Trailer,
        Self::Aircraft,
        Self::Bike,
        Self::HoverHull,
    ];

    /// The kind's name in the agent protocol (`vehicle`, `trailer`, `aircraft`, `bike`,
    /// `hoverhull`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Vehicle => "vehicle",
            Self::Trailer => "trailer",
            Self::Aircraft => "aircraft",
            Self::Bike => "bike",
            Self::HoverHull => "hoverhull",
        }
    }

    /// The kind named `name`.
    ///
    /// # Errors
    /// `vehicle_tuning_bad_asset` for another name.
    pub fn parse(name: &str) -> Result<Self, TuningError> {
        Self::ALL.into_iter().find(|k| k.name() == name).ok_or_else(|| {
            TuningError::new(
                "vehicle_tuning_bad_asset",
                format!("unknown asset `{name}`: vehicle, trailer, aircraft, bike or hoverhull"),
            )
        })
    }

    /// The kind a file name's ending says, if any.
    #[must_use]
    pub fn of_path(rel: &str) -> Option<Self> {
        if rel.ends_with(".vehicle.ron") {
            Some(Self::Vehicle)
        } else if rel.ends_with(".trailer.ron") {
            Some(Self::Trailer)
        } else if rel.ends_with(".aircraft.ron") {
            Some(Self::Aircraft)
        } else if rel.ends_with(".bike.ron") {
            Some(Self::Bike)
        } else if rel.ends_with(".hoverhull.ron") {
            Some(Self::HoverHull)
        } else {
            None
        }
    }
}

/// Reads a field path of `config` (as serde JSON).
#[must_use]
pub fn read_field(config: &VehicleConfig, field: &str) -> Option<Value> {
    let mut root = serde_json::to_value(config).ok()?;
    let mut slot = &mut root;
    for segment in field.split('.') {
        slot = step(slot, segment)?;
    }
    Some(slot.clone())
}

/// `asset` with `field` set to `value`, validated.
///
/// # Errors
/// `vehicle_tuning_bad_field` for a path the configuration does not have,
/// `vehicle_tuning_bad_value` for a value of the wrong shape, or the asset's validation code.
pub fn with_field(asset: &VehicleAsset, field: &str, value: Value) -> Result<VehicleAsset, TuningError> {
    let mut next = asset.clone();
    if field == "name" {
        next.name = value
            .as_str()
            .ok_or_else(|| TuningError::new("vehicle_tuning_bad_value", "`name` is a string"))?
            .to_string();
    } else {
        let bad_field = || TuningError {
            code: "vehicle_tuning_bad_field".to_string(),
            field: Some(field.to_string()),
            message: format!("the vehicle configuration has no field `{field}`"),
        };
        if field.is_empty() {
            return Err(bad_field());
        }
        let mut root = serde_json::to_value(&asset.config)
            .map_err(|e| TuningError::new("vehicle_tuning_bad_value", e.to_string()))?;
        let mut slot = &mut root;
        for segment in field.split('.') {
            slot = step(slot, segment).ok_or_else(bad_field)?;
        }
        *slot = value;
        next.config = serde_json::from_value(root).map_err(|e| TuningError {
            code: "vehicle_tuning_bad_value".to_string(),
            field: Some(field.to_string()),
            message: format!("`{field}`: {e}"),
        })?;
    }
    next.validate().map_err(|e| TuningError::asset(&e))?;
    Ok(next)
}

/// `rel` under the project, refusing anything that climbs out of it.
fn project_path(project: Option<&Path>, rel: &str) -> Result<PathBuf, TuningError> {
    project_path_ext(project, rel, ".vehicle.ron")
}

/// `rel` under the project ending in `ext`, refusing anything that climbs out of it.
pub(crate) fn project_path_ext(project: Option<&Path>, rel: &str, ext: &str) -> Result<PathBuf, TuningError> {
    let project =
        project.ok_or_else(|| TuningError::new("vehicle_tuning_no_project", "no project is open"))?;
    let path = Path::new(rel);
    if rel.is_empty() || path.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(TuningError::new(
            "vehicle_tuning_bad_path",
            format!("`{rel}` must be a plain path relative to the project"),
        ));
    }
    if !rel.ends_with(ext) {
        return Err(TuningError::new(
            "vehicle_tuning_bad_path",
            format!("`{rel}` must end in {ext}"),
        ));
    }
    Ok(project.join(path))
}

/// Writes through a temporary file and a rename, so a reader never sees half a file.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<(), TuningError> {
    let io = |e: std::io::Error| TuningError::new("vehicle_tuning_io", format!("{}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// The text a save writes for `asset`: merged into `source` (whose asset was `opened`) when that
/// reproduces `asset` exactly, else the canonical text. The flag says the comments were kept.
///
/// # Errors
/// `vehicle_asset_parse` when RON cannot write the asset.
pub fn save_text(
    source: Option<&str>,
    opened: Option<&VehicleAsset>,
    asset: &VehicleAsset,
) -> Result<(String, bool), TuningError> {
    let canonical = asset.to_ron().map_err(|e| TuningError::asset(&e))?;
    let (Some(source), Some(opened)) = (source, opened) else {
        return Ok((canonical, false));
    };
    let old = opened.to_ron().map_err(|e| TuningError::asset(&e))?;
    let merged = regia_ui::ron_merge::merge_ron_text(source, &old, &canonical);
    match merged {
        Some(text) if VehicleAsset::from_ron(&text).as_ref() == Ok(asset) => Ok((text, true)),
        _ => Ok((canonical, false)),
    }
}

impl VehicleTuning {
    /// The asset being edited, or `vehicle_tuning_nothing_open`.
    ///
    /// # Errors
    /// `vehicle_tuning_nothing_open` before an open.
    pub fn current(&self) -> Result<&VehicleAsset, TuningError> {
        self.asset
            .as_ref()
            .ok_or_else(|| TuningError::new("vehicle_tuning_nothing_open", "open a .vehicle.ron first"))
    }

    /// Whether the edited asset differs from the file.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        match (&self.asset, &self.opened) {
            (Some(asset), Some(opened)) => asset.config != opened.config || asset.name != opened.name,
            (Some(_), None) => true,
            _ => false,
        }
    }

    /// Opens `rel` (a `.vehicle.ron` under the project); with `create`, a missing file starts as the
    /// default car named after the file, written at the first save. `create` refuses a path whose
    /// file already exists (it never opens or overwrites it). Opening drops the state bound to the
    /// previous file: the queued Apply, the explicit Apply target, the recorded runs and the live
    /// telemetry entity.
    ///
    /// # Errors
    /// The path codes, `vehicle_tuning_not_found` for a missing file without `create`,
    /// `vehicle_tuning_exists` for `create` on a path whose file exists, `vehicle_tuning_io` for a
    /// read failure, or the `vehicle_asset_*` / `vehicle_*` code of a file that does not load.
    pub fn open(&mut self, project: Option<&Path>, rel: &str, create: bool) -> Result<(), TuningError> {
        let file = project_path(project, rel)?;
        let (opened, source, asset) = if file.is_file() {
            if create {
                return Err(TuningError::new(
                    "vehicle_tuning_exists",
                    format!("{rel} already exists (create starts a new file only)"),
                ));
            }
            let text = std::fs::read_to_string(&file)
                .map_err(|e| TuningError::new("vehicle_tuning_io", format!("{}: {e}", file.display())))?;
            let asset = VehicleAsset::from_ron(&text).map_err(|e| TuningError::asset(&e))?;
            let mut opened = asset.clone();
            // The merge compares against the file at its own version, so an older file's version
            // is rewritten by the first save.
            if let Ok(found) = regia_core::version::peek_ron_version(&text) {
                opened.format_version = found;
            }
            (Some(opened), Some(text), asset)
        } else if create {
            let name = Path::new(rel)
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".vehicle.ron"))
                .filter(|n| !n.is_empty())
                .unwrap_or("vehicle");
            (None, None, VehicleAsset::new(name, VehicleConfig::default()))
        } else {
            return Err(TuningError::new(
                "vehicle_tuning_not_found",
                format!("{rel} does not exist (pass create to start a new one)"),
            ));
        };
        self.path = Some(rel.to_string());
        self.opened = opened;
        self.source = source;
        self.asset = Some(asset);
        self.undo.clear();
        self.redo.clear();
        self.drag = None;
        self.error = None;
        self.notice = None;
        self.comments_kept = None;
        self.telemetry.clear();
        self.pending_apply = None;
        self.target = None;
        self.runs.clear();
        self.telemetry_entity = None;
        self.plot_wheel = 0;
        Ok(())
    }

    /// Sets `field` to `value`: one undo step, or part of the open drag's step when `drag` is set and
    /// the drag is on the same field. A refused value changes nothing (and closes no drag).
    ///
    /// # Errors
    /// `vehicle_tuning_nothing_open` and the codes of [`with_field`].
    pub fn set(&mut self, field: &str, value: Value, drag: bool) -> Result<(), TuningError> {
        let current = self.current()?;
        let next = with_field(current, field, value).map_err(|mut e| {
            e.field = Some(field.to_string());
            e
        })?;
        if next == *current {
            return Ok(());
        }
        let joins = drag && self.drag.as_deref() == Some(field);
        if self.shared {
            if !joins {
                self.shared_gesture = self.shared_gesture.wrapping_add(1);
            }
            if let (Some(path), Some(before)) = (&self.path, &self.asset) {
                self.pending.push(VehicleEdit {
                    path: path.clone(),
                    field: field.to_string(),
                    before: before.clone(),
                    after: next.clone(),
                    gesture: drag.then_some(self.shared_gesture),
                });
            }
        } else if !joins {
            if let Some(previous) = self.asset.clone() {
                self.undo.push(previous);
            }
            if self.undo.len() > MAX_UNDO {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.drag = drag.then(|| field.to_string());
        self.asset = Some(next);
        self.error = None;
        Ok(())
    }

    /// Ends a drag: the next edit of the same field is a new undo step.
    pub fn end_drag(&mut self) {
        self.drag = None;
    }

    /// Steps back one edit. Returns whether there was one.
    pub fn undo(&mut self) -> bool {
        if self.shared {
            self.history_request = Some(false);
            return self.shared_depths.0 > 0;
        }
        self.drag = None;
        match (self.undo.pop(), self.asset.take()) {
            (Some(previous), Some(now)) => {
                self.redo.push(now);
                self.asset = Some(previous);
                true
            }
            (previous, now) => {
                self.asset = now.or(previous);
                false
            }
        }
    }

    /// Steps forward one undone edit. Returns whether there was one.
    pub fn redo(&mut self) -> bool {
        if self.shared {
            self.history_request = Some(true);
            return self.shared_depths.1 > 0;
        }
        self.drag = None;
        match (self.redo.pop(), self.asset.take()) {
            (Some(next), Some(now)) => {
                self.undo.push(now);
                self.asset = Some(next);
                true
            }
            (next, now) => {
                self.asset = now.or(next);
                false
            }
        }
    }

    /// Writes the edited asset to `rel` (default: the open file). Returns the path written.
    ///
    /// # Errors
    /// `vehicle_tuning_nothing_open`, `vehicle_tuning_no_path`, the path codes, `vehicle_tuning_io`.
    pub fn save(&mut self, project: Option<&Path>, rel: Option<&str>) -> Result<String, TuningError> {
        let asset = self.current()?.clone();
        let rel = rel
            .map(str::to_string)
            .or_else(|| self.path.clone())
            .ok_or_else(|| TuningError::new("vehicle_tuning_no_path", "give a path to save to"))?;
        let file = project_path(project, &rel)?;
        let same_file = self.path.as_deref() == Some(rel.as_str());
        let (text, kept) = if same_file {
            save_text(self.source.as_deref(), self.opened.as_ref(), &asset)?
        } else {
            save_text(None, None, &asset)?
        };
        write_atomic(&file, &text)?;
        let had_source = same_file && self.source.is_some();
        self.path = Some(rel.clone());
        self.source = Some(text);
        self.opened = Some(asset);
        self.comments_kept = Some(kept);
        self.notice = Some(if kept || !had_source {
            format!("saved {rel}")
        } else {
            format!("saved {rel} in the canonical form: the file's comments were lost")
        });
        self.error = None;
        Ok(rel)
    }

    /// Drives the edited car through `script` for `seconds` on the proving ground and records it.
    ///
    /// # Errors
    /// `vehicle_tuning_nothing_open`, `vehicle_tuning_bad_seconds`, or a `vehicle_*` code.
    pub fn run(
        &mut self,
        script: ProvingScript,
        seconds: f32,
        label: Option<&str>,
    ) -> Result<&RecordedRun, TuningError> {
        let config = self.current()?.config.clone();
        if !(seconds.is_finite() && seconds > 0.0 && seconds <= MAX_PROVING_SECONDS) {
            return Err(TuningError::new(
                "vehicle_tuning_bad_seconds",
                format!("`seconds` must be above 0 and at most {MAX_PROVING_SECONDS}"),
            ));
        }
        let trailer = if self.tow {
            let doc = &self.others.trailer;
            let asset = doc.asset.as_ref().ok_or_else(|| {
                TuningError::new(
                    "vehicle_tuning_no_trailer",
                    "tow needs an open .trailer.ron (open one, or clear tow)",
                )
            })?;
            Some(asset.config.clone())
        } else {
            None
        };
        let trailer_digest = trailer.as_ref().map(|t| {
            let text = ron::to_string(t).unwrap_or_default();
            format!("{:016x}", fnv1a(text.as_bytes()))
        });
        let course = ProvingCourse {
            track: self.track,
            trailer,
        };
        let out = {
            let _span = tracing::info_span!("editor.vehicle_tuning_run").entered();
            run_proving_course(&config, script, seconds, &course)
                .map_err(|e| TuningError::new(e.code(), e.to_string()))?
        };
        self.run_count += 1;
        let label = label.map_or_else(|| format!("run {}", self.run_count), str::to_string);
        self.runs.push(RecordedRun {
            label,
            config_digest: format!("{:016x}", config.digest()),
            run: out.run,
            track: out.track,
            contacts: (out.min_contacts, out.max_contacts, out.contact_changes),
            max_rise: out.max_rise,
            trailer: out.trailer.zip(trailer_digest),
        });
        if self.runs.len() > MAX_RUNS {
            self.runs.remove(0);
        }
        self.runs
            .last()
            .ok_or_else(|| TuningError::new("vehicle_tuning_io", "the run was not recorded"))
    }

    /// Opens `rel` in the document its ending names (`.vehicle.ron`, `.trailer.ron`, `.aircraft.ron`,
    /// `.bike.ron` or `.hoverhull.ron`). Opening another kind leaves the car as it is.
    ///
    /// # Errors
    /// `vehicle_tuning_bad_path` for another ending, and the codes of the kind's open.
    pub fn open_any(
        &mut self,
        project: Option<&Path>,
        rel: &str,
        create: bool,
    ) -> Result<AssetKind, TuningError> {
        let kind = AssetKind::of_path(rel).ok_or_else(|| {
            TuningError::new(
                "vehicle_tuning_bad_path",
                format!(
                    "`{rel}` must end in .vehicle.ron, .trailer.ron, .aircraft.ron, .bike.ron or .hoverhull.ron"
                ),
            )
        })?;
        match kind {
            AssetKind::Vehicle => self.open(project, rel, create)?,
            AssetKind::Trailer => self.others.trailer.open(project, rel, create)?,
            AssetKind::Aircraft => self.others.aircraft.open(project, rel, create)?,
            AssetKind::Bike => self.others.bike.open(project, rel, create)?,
            AssetKind::HoverHull => self.others.hoverhull.open(project, rel, create)?,
        }
        Ok(kind)
    }

    /// Sets a field of the `kind` document.
    ///
    /// # Errors
    /// As [`VehicleTuning::set`] for that document.
    pub fn set_in(
        &mut self,
        kind: AssetKind,
        field: &str,
        value: Value,
        drag: bool,
    ) -> Result<(), TuningError> {
        match kind {
            AssetKind::Vehicle => self.set(field, value, drag),
            AssetKind::Trailer => self.others.trailer.set(field, value, drag),
            AssetKind::Aircraft => self.others.aircraft.set(field, value, drag),
            AssetKind::Bike => self.others.bike.set(field, value, drag),
            AssetKind::HoverHull => self.others.hoverhull.set(field, value, drag),
        }
    }

    /// Undoes one edit of the `kind` document. Returns whether there was one.
    pub fn undo_in(&mut self, kind: AssetKind) -> bool {
        match kind {
            AssetKind::Vehicle => self.undo(),
            AssetKind::Trailer => self.others.trailer.undo(),
            AssetKind::Aircraft => self.others.aircraft.undo(),
            AssetKind::Bike => self.others.bike.undo(),
            AssetKind::HoverHull => self.others.hoverhull.undo(),
        }
    }

    /// Redoes one edit of the `kind` document. Returns whether there was one.
    pub fn redo_in(&mut self, kind: AssetKind) -> bool {
        match kind {
            AssetKind::Vehicle => self.redo(),
            AssetKind::Trailer => self.others.trailer.redo(),
            AssetKind::Aircraft => self.others.aircraft.redo(),
            AssetKind::Bike => self.others.bike.redo(),
            AssetKind::HoverHull => self.others.hoverhull.redo(),
        }
    }

    /// Saves the `kind` document.
    ///
    /// # Errors
    /// As [`VehicleTuning::save`] for that document.
    pub fn save_in(
        &mut self,
        kind: AssetKind,
        project: Option<&Path>,
        rel: Option<&str>,
    ) -> Result<String, TuningError> {
        match kind {
            AssetKind::Vehicle => self.save(project, rel),
            AssetKind::Trailer => self.others.trailer.save(project, rel),
            AssetKind::Aircraft => self.others.aircraft.save(project, rel),
            AssetKind::Bike => self.others.bike.save(project, rel),
            AssetKind::HoverHull => self.others.hoverhull.save(project, rel),
        }
    }

    /// The `kind` document's last save: `(comments_kept, notice)`.
    #[must_use]
    pub fn saved_in(&self, kind: AssetKind) -> (Option<bool>, Option<String>) {
        match kind {
            AssetKind::Vehicle => (self.comments_kept, self.notice.clone()),
            AssetKind::Trailer => (
                self.others.trailer.comments_kept,
                self.others.trailer.notice.clone(),
            ),
            AssetKind::Aircraft => (
                self.others.aircraft.comments_kept,
                self.others.aircraft.notice.clone(),
            ),
            AssetKind::Bike => (self.others.bike.comments_kept, self.others.bike.notice.clone()),
            AssetKind::HoverHull => (
                self.others.hoverhull.comments_kept,
                self.others.hoverhull.notice.clone(),
            ),
        }
    }

    /// Records `error` on the `kind` document (the panel shows it beside that form).
    pub fn refuse_in(&mut self, kind: AssetKind, error: TuningError) {
        match kind {
            AssetKind::Vehicle => self.error = Some(error),
            AssetKind::Trailer => self.others.trailer.error = Some(error),
            AssetKind::Aircraft => self.others.aircraft.error = Some(error),
            AssetKind::Bike => self.others.bike.error = Some(error),
            AssetKind::HoverHull => self.others.hoverhull.error = Some(error),
        }
    }

    /// Rides the open bike (`kind` [`AssetKind::Bike`]) or hover hull ([`AssetKind::HoverHull`]) on the
    /// proving ground for `steps` fixed steps with `input` held. The first ride, or one with
    /// `respawn`, spawns it at rest; later rides carry on from where the last one stopped, stepping the
    /// document's configuration as it is now (an edit since the last ride reaches it at once). Returns
    /// the ride's state with `asset`, `steps`, `spawned` and the `config` it stepped.
    ///
    /// # Errors
    /// `vehicle_tuning_bad_asset` for another kind, `vehicle_tuning_bad_steps` outside
    /// `1..=MAX_RIDE_STEPS`, `vehicle_tuning_nothing_open`, `vehicle_tuning_bad_value` for a non-finite
    /// input, or the bike's `bike_bad_input` (a refused step keeps the steps before it).
    pub fn ride(
        &mut self,
        kind: AssetKind,
        steps: u32,
        input: RideInput,
        respawn: bool,
    ) -> Result<Value, TuningError> {
        if !matches!(kind, AssetKind::Bike | AssetKind::HoverHull) {
            return Err(TuningError::new(
                "vehicle_tuning_bad_asset",
                format!("a {} does not ride: ride takes bike or hoverhull", kind.name()),
            ));
        }
        if !(1..=MAX_RIDE_STEPS).contains(&steps) {
            return Err(TuningError::new(
                "vehicle_tuning_bad_steps",
                format!("`steps` must be 1 to {MAX_RIDE_STEPS}"),
            ));
        }
        let axes = [
            input.steer,
            input.speed,
            input.drive,
            input.throttle,
            input.strafe,
        ];
        if axes.iter().any(|v| !v.is_finite()) {
            return Err(TuningError::new(
                "vehicle_tuning_bad_value",
                "steer, speed, drive, throttle and strafe must be finite",
            ));
        }
        let (spawned, mut value) = if kind == AssetKind::Bike {
            let config = self.others.bike.current()?.config;
            let spawned = respawn || self.others.bike_ride.is_none();
            let mut ride = match self.others.bike_ride {
                Some(ride) if !spawned => ride,
                _ => BikeRide::spawn(),
            };
            let result = ride.ride(&config, steps, input);
            // A refused first step spawns nothing; a later refusal keeps the steps it took.
            if ride.steps > 0 {
                self.others.bike_ride = Some(ride);
            }
            result?;
            let mut value = ride.json();
            value["config"] = serde_json::to_value(config).unwrap_or(Value::Null);
            (spawned, value)
        } else {
            let hull = self.others.hoverhull.current()?.clone();
            let spawned = respawn || self.others.hover_ride.is_none();
            let mut ride = match self.others.hover_ride {
                Some(ride) if !spawned => ride,
                _ => HoverRide::spawn(&hull.params),
            };
            ride.ride(&hull, steps, input);
            self.others.hover_ride = Some(ride);
            let mut value = ride.json();
            value["config"] = serde_json::to_value(hull.params).unwrap_or(Value::Null);
            (spawned, value)
        };
        value["asset"] = json!(kind.name());
        value["steps"] = json!(steps);
        value["spawned"] = json!(spawned);
        Ok(value)
    }

    /// Trims the open aircraft for level flight at `speed` m/s and `altitude` m and flies the trim
    /// for `seconds`; records the flight.
    ///
    /// # Errors
    /// `vehicle_tuning_nothing_open`, `vehicle_tuning_bad_seconds`, or an `aircraft_*` code.
    pub fn fly(
        &mut self,
        speed: f32,
        altitude: f32,
        seconds: f32,
        label: Option<&str>,
    ) -> Result<&FlightRun, TuningError> {
        let config = self.others.aircraft.current()?.config.clone();
        let label = label.map_or_else(
            || format!("flight {}", self.others.flight_count + 1),
            str::to_string,
        );
        let flight = assets::fly(&config, speed, altitude, seconds, label)?;
        self.others.flight_count += 1;
        self.others.flights.push(flight);
        if self.others.flights.len() > MAX_RUNS {
            self.others.flights.remove(0);
        }
        self.others
            .flights
            .last()
            .ok_or_else(|| TuningError::new("vehicle_tuning_io", "the flight was not recorded"))
    }

    /// The play vehicle Apply reaches in `world`: `wanted` (entity bits) when given, else the panel's
    /// target, else the vehicle running the opened file's configuration, else the only vehicle.
    ///
    /// # Errors
    /// `vehicle_tuning_no_target` when none of those finds one vehicle.
    pub fn pick_target(&self, world: &mut World, wanted: Option<u64>) -> Result<Entity, TuningError> {
        let mut query = world.query::<(Entity, &VehicleController)>();
        let mut rows: Vec<(Entity, u64)> = query
            .iter(world)
            .map(|(entity, controller)| (entity, controller.vehicle.config().digest()))
            .collect();
        rows.sort_by_key(|(entity, _)| (entity.index().index(), entity.to_bits()));
        if let Some(bits) = wanted.or(self.target) {
            return rows
                .iter()
                .find(|(entity, _)| entity.to_bits() == bits)
                .map(|(entity, _)| *entity)
                .ok_or_else(|| {
                    TuningError::new(
                        "vehicle_tuning_no_target",
                        format!("entity {bits} is not a vehicle in the play world"),
                    )
                });
        }
        let digests: Vec<u64> = [self.opened.as_ref(), self.asset.as_ref()]
            .into_iter()
            .flatten()
            .map(|asset| asset.config.digest())
            .collect();
        if let Some((entity, _)) = rows.iter().find(|(_, digest)| digests.contains(digest)) {
            return Ok(*entity);
        }
        match rows.as_slice() {
            [(only, _)] => Ok(*only),
            _ => Err(TuningError::new(
                "vehicle_tuning_no_target",
                format!(
                    "{} vehicles in the play world and none runs this file: name one with `entity`",
                    rows.len()
                ),
            )),
        }
    }

    /// Sends the edited configuration to a play vehicle in `world` now (`Vehicle::set_config`).
    ///
    /// # Errors
    /// `vehicle_tuning_nothing_open`, `vehicle_tuning_no_target`, `vehicle_tuning_no_physics`, or the
    /// `vehicle_*` code `set_config` refuses with.
    pub fn apply_now(&mut self, world: &mut World, wanted: Option<u64>) -> Result<Value, TuningError> {
        let config = self.current()?.config.clone();
        let entity = self.pick_target(world, wanted)?;
        if !world.contains_resource::<PhysicsWorld>() {
            return Err(TuningError::new(
                "vehicle_tuning_no_physics",
                "the play world has no PhysicsWorld",
            ));
        }
        let digest = config.digest();
        let change = world.resource_scope(|world, mut physics: Mut<PhysicsWorld>| {
            let mut controller = world
                .get_mut::<VehicleController>(entity)
                .ok_or_else(|| TuningError::new("vehicle_tuning_no_target", "the vehicle went away"))?;
            controller
                .vehicle
                .set_config(&mut physics, config)
                .map(|change| (change, controller.vehicle.config_revision()))
                .map_err(|e| TuningError::new(e.code(), e.to_string()))
        })?;
        self.telemetry_entity = Some(entity.to_bits());
        self.notice = Some(format!("applied to vehicle {}", entity.to_bits()));
        Ok(json!({
            "entity": entity.to_bits(),
            "config_digest": format!("{digest:016x}"),
            "config_revision": change.1,
            "change": change.0,
        }))
    }

    /// Reads one telemetry frame of the strip's vehicle from `world` (the play world).
    pub fn sample_telemetry(&mut self, world: &mut World) {
        let _span = tracing::trace_span!(target: "regia_editor", "vehicle_tuning_telemetry").entered();
        let entity = match self.telemetry_entity {
            Some(bits) => Entity::try_from_bits(bits),
            None => self.pick_target(world, None).ok(),
        };
        let Some(entity) = entity else {
            return;
        };
        let (Some(physics), Some(controller)) = (
            world.get_resource::<PhysicsWorld>(),
            world.get::<VehicleController>(entity),
        ) else {
            return;
        };
        let vehicle = &controller.vehicle;
        let Ok(state) = vehicle.state(physics) else {
            return;
        };
        let steps = vehicle.steps();
        if self.telemetry.last().is_some_and(|frame| frame.steps == steps) {
            return;
        }
        self.telemetry_entity = Some(entity.to_bits());
        self.telemetry.push(TelemetryFrame {
            steps,
            speed_kmh: state.speed * 3.6,
            rpm: state.rpm,
            gear: state.gear,
            wheels: state
                .wheels
                .iter()
                .map(|wheel| (wheel.slip_ratio, wheel.slip_angle, wheel.suspension_force))
                .collect(),
        });
        if self.telemetry.len() > TELEMETRY_FRAMES {
            let extra = self.telemetry.len() - TELEMETRY_FRAMES;
            self.telemetry.drain(..extra);
        }
    }

    /// The state `editor.vehicle_tuning_state` reports; `plots` adds the plot samples.
    #[must_use]
    pub fn json(&self, plots: bool) -> Value {
        let validation = self.asset.as_ref().map(|asset| match asset.validate() {
            Ok(()) => json!({ "ok": true }),
            Err(e) => json!({ "ok": false, "code": e.code(), "message": e.to_string() }),
        });
        let warnings: Vec<Value> = self
            .asset
            .as_ref()
            .map(|asset| {
                regia::physics::vehicle_tire::tire_warnings(&asset.config)
                    .into_iter()
                    .map(|w| json!({ "code": w.code, "wheel": w.wheel, "message": w.message }))
                    .collect()
            })
            .unwrap_or_default();
        let mut v = json!({
            "path": self.path,
            "open": self.asset.is_some(),
            "name": self.asset.as_ref().map(|a| a.name.clone()),
            "dirty": self.is_dirty(),
            "config": self.asset.as_ref().and_then(|a| serde_json::to_value(&a.config).ok()),
            "config_digest": self.asset.as_ref().map(|a| format!("{:016x}", a.config.digest())),
            "validation": validation,
            "warnings": warnings,
            "undo_depth": self.undo_depth(),
            "redo_depth": self.redo_depth(),
            "error": self.error,
            "notice": self.notice,
            "comments_kept": self.comments_kept,
            "target": self.target,
            "apply_pending": self.pending_apply.is_some(),
            "last_apply": self.last_apply.as_ref().map(|r| match r {
                Ok(v) => json!({ "ok": true, "answer": v }),
                Err(e) => json!({ "ok": false, "error": e }),
            }),
            "runs": self.runs.iter().map(RecordedRun::metrics).collect::<Vec<_>>(),
            "telemetry": self.telemetry.last(),
            "telemetry_frames": self.telemetry.len(),
            "track": self.track.name(),
            "tow": self.tow,
            "trailer": self.others.trailer.json(),
            "aircraft": self.others.aircraft.json(),
            "flights": self.others.flights,
            "bike": self.others.bike.json(),
            "hoverhull": self.others.hoverhull.json(),
            "rides": {
                "bike": self.others.bike_ride.as_ref().map(BikeRide::json),
                "hoverhull": self.others.hover_ride.as_ref().map(HoverRide::json),
            },
        });
        if plots && let Some(asset) = &self.asset {
            v["plots"] = plots_json(&asset.config, self.plot_wheel);
        }
        v
    }
}

/// The plot samples of `editor.vehicle_tuning_state { plots: true }`.
fn plots_json(config: &VehicleConfig, wheel: usize) -> Value {
    use crate::vehicle_tuning_plots as plots;
    use regia::physics::vehicle_drivetrain::Drivetrain;
    let torque = match &config.drivetrain {
        Drivetrain::Geared(d) => json!(plots::torque_power(&d.engine, 41)),
        Drivetrain::Simple => Value::Null,
    };
    let tire = config
        .wheels
        .get(wheel)
        .and_then(|w| w.tire_model.slip())
        .map(|tire| {
            let load = tire.nominal_load;
            json!({ "wheel": wheel, "load": load, "points": plots::tire_side_force(tire, load, 0.5, 41) })
        });
    json!({
        "torque_power": torque,
        "wheel_force": plots::wheel_force_per_gear(config, 41),
        "tire_side_force": tire,
        "suspension": plots::suspension_travel(config, wheel, 21),
    })
}

/// Whether a play session runs (the editor's `PieStatus`).
fn playing(world: &World) -> bool {
    world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|s| s.mode.is_some())
}

/// Each editor frame: runs a queued Apply on the play world and reads the telemetry strip while a
/// session runs; without one a queued Apply fails with `vehicle_tuning_not_playing`.
pub fn pump(world: &mut World, session: Option<&mut crate::pie::PieSession>) {
    let Some(mut panel) = world.remove_resource::<VehicleTuning>() else {
        return;
    };
    pump_world(&mut panel, session.map(|s| s.app.world_mut()));
    world.insert_resource(panel);
}

/// [`pump`] against a play world (`None` when no session runs).
pub fn pump_world(panel: &mut VehicleTuning, play: Option<&mut World>) {
    let _span = tracing::trace_span!(target: "regia_editor", "vehicle_tuning_pump").entered();
    match play {
        Some(world) => {
            if let Some(wanted) = panel.pending_apply.take() {
                let result = panel.apply_now(world, wanted);
                panel.last_apply = Some(result);
            }
            panel.sample_telemetry(world);
        }
        None => {
            if panel.pending_apply.take().is_some() {
                panel.last_apply = Some(Err(TuningError::new(
                    "vehicle_tuning_not_playing",
                    "Apply needs a play session",
                )));
            }
            panel.telemetry_entity = None;
        }
    }
}

/// Runs a model operation and transfers hosted edits to the shared command stack.
pub(crate) fn with_panel<T>(world: &mut World, f: impl FnOnce(&mut VehicleTuning, Option<&Path>) -> T) -> T {
    with_panel_frame(world, None, f)
}

/// Shares document edits and guards GUI navigation across repeated layout passes.
pub(crate) fn with_panel_frame<T>(
    world: &mut World,
    frame: Option<u64>,
    f: impl FnOnce(&mut VehicleTuning, Option<&Path>) -> T,
) -> T {
    let _span = tracing::trace_span!(target: "regia_editor", "vehicle_shared_history").entered();
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<VehicleTuning>().unwrap_or_default();
    let shared = world.get_resource::<crate::command::UndoStack>();
    panel.shared = shared.is_some();
    panel.shared_depths = shared.map_or((0, 0), |s| (s.undo_depth(), s.redo_depth()));
    if panel.shared {
        remember_vehicle(world, &panel);
    }
    let out = f(&mut panel, project.as_deref());
    if panel.shared {
        remember_vehicle(world, &panel);
        let mut stack = world.resource_mut::<crate::command::UndoStack>();
        for command in panel.pending.drain(..) {
            if command.gesture.is_none() {
                stack.seal();
            }
            stack.push(Box::new(command));
            if panel.drag.is_none() {
                stack.seal();
            }
        }
    }
    let request = panel.history_request.take();
    world.insert_resource(panel);
    if let Some(redo) = request {
        if let Some(frame) = frame {
            crate::command::navigate_world_once(world, frame, redo);
        } else if redo {
            crate::command::redo_world(world);
        } else {
            crate::command::undo_world(world);
        }
    }
    out
}

impl VehicleTuning {
    /// Undo depth of the shared editor history, or this standalone model's history.
    pub fn undo_depth(&self) -> usize {
        if self.shared {
            self.shared_depths.0 + self.pending.len()
        } else {
            self.undo.len()
        }
    }
    /// Redo depth of the shared editor history, or this standalone model's history.
    pub fn redo_depth(&self) -> usize {
        if self.shared {
            self.shared_depths.1
        } else {
            self.redo.len()
        }
    }
}

#[derive(Clone)]
struct VehicleDocument {
    opened: Option<VehicleAsset>,
    source: Option<String>,
    asset: Option<VehicleAsset>,
}

#[derive(Resource, Default)]
struct VehicleDocuments(std::collections::BTreeMap<String, VehicleDocument>);

/// A `.vehicle.ron` the panel opened before the one it shows, with unsaved edits (P-322 part b,
/// document autosave): its path, the file's text and asset as opened, and the edited asset.
pub(crate) struct RetainedVehicle<'a> {
    /// The file, relative to the project.
    pub path: &'a str,
    /// The file's text as it was opened or last saved.
    pub source: Option<&'a str>,
    /// The asset as it was opened or last saved.
    pub opened: Option<&'a VehicleAsset>,
    /// The edited asset.
    pub asset: &'a VehicleAsset,
}

/// The vehicles kept behind the open one that have unsaved edits (not the open file).
pub(crate) fn retained_dirty_vehicles(world: &World) -> Vec<RetainedVehicle<'_>> {
    let Some(docs) = world.get_resource::<VehicleDocuments>() else {
        return Vec::new();
    };
    let open = world
        .get_resource::<VehicleTuning>()
        .and_then(|panel| panel.path.as_deref());
    docs.0
        .iter()
        .filter(|(path, _)| Some(path.as_str()) != open)
        .filter_map(|(path, doc)| {
            let asset = doc.asset.as_ref()?;
            let dirty = doc
                .opened
                .as_ref()
                .is_none_or(|opened| opened.config != asset.config || opened.name != asset.name);
            dirty.then_some(RetainedVehicle {
                path,
                source: doc.source.as_deref(),
                opened: doc.opened.as_ref(),
                asset,
            })
        })
        .collect()
}

fn remember_vehicle(world: &mut World, panel: &VehicleTuning) {
    if let Some(path) = &panel.path {
        world
            .get_resource_or_insert_with::<VehicleDocuments>(Default::default)
            .0
            .insert(
                path.clone(),
                VehicleDocument {
                    opened: panel.opened.clone(),
                    source: panel.source.clone(),
                    asset: panel.asset.clone(),
                },
            );
    }
}

#[derive(Debug)]
struct VehicleEdit {
    path: String,
    field: String,
    before: VehicleAsset,
    after: VehicleAsset,
    gesture: Option<u64>,
}

impl VehicleEdit {
    fn restore(&self, world: &mut World, asset: &VehicleAsset) {
        let mut panel = world.remove_resource::<VehicleTuning>().unwrap_or_default();
        remember_vehicle(world, &panel);
        if panel.path.as_deref() != Some(&self.path) {
            let doc = world
                .get_resource::<VehicleDocuments>()
                .and_then(|docs| docs.0.get(&self.path))
                .cloned();
            panel = VehicleTuning::default();
            panel.path = Some(self.path.clone());
            if let Some(doc) = doc {
                panel.opened = doc.opened;
                panel.source = doc.source;
                panel.asset = doc.asset;
            }
        }
        panel.asset = Some(asset.clone());
        panel.drag = None;
        panel.error = None;
        remember_vehicle(world, &panel);
        world.insert_resource(panel);
    }
}

impl crate::command::EditorCommand for VehicleEdit {
    fn apply(&mut self, world: &mut World) {
        self.restore(world, &self.after);
    }
    fn revert(&mut self, world: &mut World) {
        self.restore(world, &self.before);
    }
    fn merge(&mut self, next: &dyn crate::command::EditorCommand) -> bool {
        let Some(next) = (next as &dyn std::any::Any).downcast_ref::<Self>() else {
            return false;
        };
        if self.gesture.is_some()
            && self.gesture == next.gesture
            && self.path == next.path
            && self.field == next.field
        {
            self.after = next.after.clone();
            true
        } else {
            false
        }
    }
    fn changes_level_state(&self) -> bool {
        false
    }
    fn touched(&self) -> Vec<crate::command::CommandTouch> {
        vec![crate::command::CommandTouch::asset(
            self.path.clone(),
            Some(self.field.clone()),
        )]
    }
    fn description(&self) -> String {
        format!("Set {} in {}", self.field, self.path)
    }
}

fn string_param<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a string"))),
    }
}

fn entity_param(params: &Value) -> Result<Option<u64>, RpcError> {
    match params.get("entity") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id (a u64)")),
    }
}

/// The `asset` parameter: the document an action is about (default: the vehicle).
fn kind_param(params: &Value) -> Result<AssetKind, RpcError> {
    match string_param(params, "asset")? {
        None => Ok(AssetKind::Vehicle),
        Some(name) => AssetKind::parse(name).map_err(|e| e.rpc()),
    }
}

/// `editor.vehicle_tuning_open { path, create? }`: the ending picks the document.
fn tuning_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = string_param(params, "path")?
        .ok_or_else(|| {
            RpcError::invalid_params(
                "`path` (a .vehicle.ron, .trailer.ron, .aircraft.ron, .bike.ron or .hoverhull.ron under the \
                 project) is required",
            )
        })?
        .to_string();
    let create = params.get("create").and_then(Value::as_bool) == Some(true);
    with_panel(world, |panel, project| {
        let kind = panel.open_any(project, &path, create).map_err(|e| e.rpc())?;
        let mut v = panel.json(false);
        v["opened"] = json!(kind.name());
        Ok(v)
    })
}

/// `editor.vehicle_tuning_state { plots? }`.
fn tuning_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let plots = params.get("plots").and_then(Value::as_bool) == Some(true);
    if let Some(wheel) = params.get("wheel").and_then(Value::as_u64) {
        with_panel(world, |panel, _| panel.plot_wheel = wheel as usize);
    }
    Ok(with_panel(world, |panel, _| panel.json(plots)))
}

/// `editor.vehicle_tuning_set { field, value }`.
fn tuning_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let field = string_param(params, "field")?
        .ok_or_else(|| {
            RpcError::invalid_params("`field` (a dotted path such as wheels.0.radius) is required")
        })?
        .to_string();
    let Some(value) = params.get("value").cloned() else {
        return Err(RpcError::invalid_params("`value` is required"));
    };
    let kind = kind_param(params)?;
    with_panel(world, |panel, _| {
        if let Err(e) = panel.set_in(kind, &field, value, false) {
            panel.refuse_in(kind, e.clone());
            return Err(e.rpc());
        }
        Ok(())
    })?;
    tuning_state(world, &Value::Null)
}

/// `editor.vehicle_tuning_undo { asset? }`: the vehicle's edits go through the shared editor
/// history (its document reactivated when needed); a trailer's or aircraft's through its own.
fn tuning_undo(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let kind = kind_param(params)?;
    let changed = with_panel(world, |panel, _| panel.undo_in(kind));
    let mut value = with_panel(world, |panel, _| panel.json(false));
    value["changed"] = json!(changed);
    Ok(value)
}

/// `editor.vehicle_tuning_redo { asset? }`: the vehicle's edits go through the shared editor
/// history (its document reactivated when needed); a trailer's or aircraft's through its own.
fn tuning_redo(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let kind = kind_param(params)?;
    let changed = with_panel(world, |panel, _| panel.redo_in(kind));
    let mut value = with_panel(world, |panel, _| panel.json(false));
    value["changed"] = json!(changed);
    Ok(value)
}

/// `editor.vehicle_tuning_apply { entity? }`: queued for the next pump.
fn tuning_apply(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let wanted = entity_param(params)?;
    let playing = playing(world);
    with_panel(world, |panel, _| {
        panel.current().map_err(|e| e.rpc())?;
        if !playing {
            return Err(TuningError::new("vehicle_tuning_not_playing", "Apply needs a play session").rpc());
        }
        if wanted.is_some() {
            panel.target = wanted;
        }
        panel.pending_apply = Some(wanted);
        Ok(json!({ "queued": true, "entity": wanted }))
    })
}

/// `editor.vehicle_tuning_save { path?, asset? }`.
fn tuning_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = string_param(params, "path")?.map(str::to_string);
    let kind = kind_param(params)?;
    with_panel(world, |panel, project| {
        let written = panel
            .save_in(kind, project, path.as_deref())
            .map_err(|e| e.rpc())?;
        let (kept, notice) = panel.saved_in(kind);
        Ok(json!({ "written": written, "asset": kind.name(), "comments_kept": kept, "notice": notice }))
    })
}

/// `editor.vehicle_tuning_run { script, seconds, label? }`.
fn tuning_run(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let script = string_param(params, "script")?.unwrap_or("accel");
    let script = ProvingScript::from_name(script).ok_or_else(|| {
        TuningError::new(
            "vehicle_tuning_bad_script",
            format!("unknown script `{script}`: accel or skidpad"),
        )
        .rpc()
    })?;
    let seconds = match params.get("seconds") {
        None | Some(Value::Null) => 20.0,
        Some(v) => v
            .as_f64()
            .ok_or_else(|| RpcError::invalid_params("`seconds` must be a number"))? as f32,
    };
    let label = string_param(params, "label")?.map(str::to_string);
    let track = match string_param(params, "track")? {
        None => None,
        Some(name) => Some(ProvingTrack::from_name(name).ok_or_else(|| {
            TuningError::new(
                "vehicle_tuning_bad_track",
                format!("unknown track `{name}`: flat, ramp, slope or bump_strip"),
            )
            .rpc()
        })?),
    };
    let tow = match params.get("tow") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`tow` must be a boolean"))?,
        ),
    };
    with_panel(world, |panel, _| {
        // A refused run leaves the panel's track and tow as they were: the GUI draws the tow
        // checkbox disabled without a trailer, so a stuck `tow` could not be cleared there
        // (review sprint-w11-rev-c-k V1).
        let before = (panel.track, panel.tow);
        if let Some(track) = track {
            panel.track = track;
        }
        if let Some(tow) = tow {
            panel.tow = tow;
        }
        match panel.run(script, seconds, label.as_deref()) {
            Ok(run) => Ok(run.metrics()),
            Err(e) => {
                (panel.track, panel.tow) = before;
                Err(e.rpc())
            }
        }
    })
}

/// A number parameter with a default.
fn number_param(params: &Value, key: &str, default: f32) -> Result<f32, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_f64()
            .map(|n| n as f32)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a number"))),
    }
}

/// `editor.vehicle_tuning_fly { speed?, altitude?, seconds?, label? }`.
fn tuning_fly(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let speed = number_param(params, "speed", 50.0)?;
    let altitude = number_param(params, "altitude", 1000.0)?;
    let seconds = number_param(params, "seconds", 10.0)?;
    let label = string_param(params, "label")?.map(str::to_string);
    with_panel(world, |panel, _| {
        let flight = panel
            .fly(speed, altitude, seconds, label.as_deref())
            .map_err(|e| e.rpc())?;
        serde_json::to_value(flight).map_err(|e| RpcError::internal(e.to_string()))
    })
}

/// `editor.vehicle_tuning_ride { asset, steps?, steer?, speed?, drive?, throttle?, strafe?, respawn? }`.
fn tuning_ride(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let kind = match string_param(params, "asset")? {
        None => {
            return Err(RpcError::invalid_params(
                "`asset` (bike or hoverhull) is required",
            ));
        }
        Some(name) => AssetKind::parse(name).map_err(|e| e.rpc())?,
    };
    let steps = match params.get("steps") {
        None | Some(Value::Null) => 60,
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`steps` must be a whole number"))?;
            u32::try_from(n).unwrap_or(u32::MAX)
        }
    };
    let input = RideInput {
        steer: number_param(params, "steer", 0.0)?,
        speed: number_param(params, "speed", 8.0)?,
        drive: number_param(params, "drive", 1.0)?,
        throttle: number_param(params, "throttle", 0.0)?,
        strafe: number_param(params, "strafe", 0.0)?,
    };
    let respawn = params.get("respawn").and_then(Value::as_bool) == Some(true);
    with_panel(world, |panel, _| {
        panel.ride(kind, steps, input, respawn).map_err(|e| {
            panel.refuse_in(kind, e.clone());
            e.rpc()
        })
    })
}

const fn param(
    name: &'static str,
    kind: &'static str,
    required: bool,
    description: &'static str,
) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const ASSET_DOC: &str = "The document: vehicle (default), trailer, aircraft, bike or hoverhull.";

const ASSET_PARAMS: &[ParamSchema] = &[param("asset", "string", false, ASSET_DOC)];

const OPEN_PARAMS: &[ParamSchema] = &[
    param(
        "path",
        "string",
        true,
        "A .vehicle.ron, .trailer.ron, .aircraft.ron, .bike.ron or .hoverhull.ron relative to the project; the \
         ending picks the document.",
    ),
    param(
        "create",
        "boolean",
        false,
        "Start the kind's default when the file does not exist (written at the first save).",
    ),
];

const STATE_PARAMS: &[ParamSchema] = &[
    param(
        "plots",
        "boolean",
        false,
        "Add the plot samples: torque and power, wheel force per gear, tire side force, suspension.",
    ),
    param(
        "wheel",
        "integer",
        false,
        "The wheel the tire and suspension plots show.",
    ),
];

const SET_PARAMS: &[ParamSchema] = &[
    param(
        "field",
        "string",
        true,
        "A dotted path: chassis_mass, wheels.0.radius, drivetrain.gearbox.final_drive, \
         drivetrain.engine.torque_curve.2, wheels.1.tire_model.cornering_stiffness, aero, name.",
    ),
    param(
        "value",
        "any",
        true,
        "The new value, in the field's serde JSON shape.",
    ),
    param("asset", "string", false, ASSET_DOC),
];

const APPLY_PARAMS: &[ParamSchema] = &[param(
    "entity",
    "integer",
    false,
    "The play vehicle (bits from physics.vehicles); default: the car running this file, or the only one.",
)];

const SAVE_PARAMS: &[ParamSchema] = &[
    param(
        "path",
        "string",
        false,
        "A file of the document's kind relative to the project; default: the open file.",
    ),
    param("asset", "string", false, ASSET_DOC),
];

const RUN_PARAMS: &[ParamSchema] = &[
    param("script", "string", false, "accel (default) or skidpad."),
    param(
        "seconds",
        "number",
        false,
        "Script length, above 0 and at most 120 (default 20).",
    ),
    param(
        "label",
        "string",
        false,
        "The run's name in the comparison table.",
    ),
    param(
        "track",
        "string",
        false,
        "The proving-ground track: flat, ramp, slope or bump_strip (kept for later runs).",
    ),
    param(
        "tow",
        "boolean",
        false,
        "Tow the open trailer on its hitch behind the car (kept for later runs).",
    ),
];

const FLY_PARAMS: &[ParamSchema] = &[
    param(
        "speed",
        "number",
        false,
        "Level-flight true airspeed, m/s (default 50).",
    ),
    param(
        "altitude",
        "number",
        false,
        "Altitude above sea level, m (default 1000).",
    ),
    param(
        "seconds",
        "number",
        false,
        "Seconds to fly the trim, above 0 and at most 120 (default 10).",
    ),
    param("label", "string", false, "The flight's name in the table."),
];

const RIDE_PARAMS: &[ParamSchema] = &[
    param(
        "asset",
        "string",
        true,
        "bike or hoverhull: the open document to ride.",
    ),
    param(
        "steps",
        "integer",
        false,
        "Fixed steps (1/60 s each) to ride, 1 to 7200 (default 60).",
    ),
    param(
        "steer",
        "number",
        false,
        "Held steer: the bike's front steer in radians (within ±1.2), the hover craft's steer axis (default 0).",
    ),
    param(
        "speed",
        "number",
        false,
        "The bike's held travel speed, m/s (within ±150, default 8).",
    ),
    param(
        "drive",
        "number",
        false,
        "The bike's held drive command (default 1).",
    ),
    param(
        "throttle",
        "number",
        false,
        "The hover craft's held throttle axis, -1 to 1 (default 0).",
    ),
    param(
        "strafe",
        "number",
        false,
        "The hover craft's held strafe axis, -1 to 1 (default 0).",
    ),
    param(
        "respawn",
        "boolean",
        false,
        "Spawn a new one at rest first instead of riding on.",
    ),
];

const RIDE: &str = "{ asset, steps, spawned, total_steps, config, state: { bike: lean, steer, speed, drive, \
    front_contact, rear_contact, fallen | hoverhull: velocity, yaw, yaw_rate, altitude, airborne, roll, pitch }, \
    digest (bike), recoveries (hoverhull) }";

const FLIGHT: &str = "{ label, speed, altitude, seconds, alpha_deg, pitch_deg, throttle, pitch_input, stalled, \
    max_altitude_change_m, max_speed_change_pct, max_bank_deg, digest, mass }";

const STATE: &str = "{ path, open, name, dirty, config, config_digest, validation: { ok, code, message }, \
    warnings: [{ code, wheel, message }], undo_depth, redo_depth, error: { code, field, message }, notice, \
    comments_kept, target, apply_pending, last_apply: { ok, answer, error }, runs: [metrics], telemetry: { steps, \
    speed_kmh, rpm, gear, wheels: [[slip_ratio, slip_angle, load]] }, telemetry_frames, track, tow, trailer: { path, \
    open, name, dirty, config, validation, undo_depth, redo_depth, error, notice, comments_kept }, aircraft: { same }, \
    flights: [flight], bike: { same }, hoverhull: { same }, rides: { bike: ride?, hoverhull: ride? }, plots? }";

const METRICS: &str = "{ label, script, seconds, zero_to_100_s, top_speed_kmh, max_lateral_g, distance_m, \
    upshifts, digest, config_digest, track, min_contacts, max_contacts, contact_changes, max_rise_m, trailer: \
    { coupled, max_articulation_deg, max_coupling_gap_m, chassis_mass, config_digest } }";

/// The Vehicle Tuning panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    // A named constructor (not a closure), so `cargo xtask book-coverage` reads each row's method
    // name from its first argument (ADR-7381).
    fn row(
        name: &'static str,
        tool: &'static str,
        description: &'static str,
        params: &'static [ParamSchema],
        result: &'static str,
        mutating: bool,
    ) -> MethodSchema {
        MethodSchema {
            name,
            tool,
            description,
            params,
            result,
            mutating,
            permission: if mutating {
                Permission::EditScene
            } else {
                Permission::Read
            },
        }
    }
    vec![
        (
            row(
                "editor.vehicle_tuning_open",
                "vehicle_tuning_open",
                "Open (or with create, start) a .vehicle.ron, .trailer.ron, .aircraft.ron, .bike.ron or \
                 .hoverhull.ron in the Vehicle Tuning panel; the file's ending picks the document.",
                OPEN_PARAMS,
                STATE,
                true,
            ),
            tuning_open as Handler,
        ),
        (
            row(
                "editor.vehicle_tuning_state",
                "vehicle_tuning_state",
                "The Vehicle Tuning panel: the edited config, validation and warnings, undo depth, the last Apply, \
                 recorded runs and the play car's telemetry; plots adds the plot samples.",
                STATE_PARAMS,
                STATE,
                false,
            ),
            tuning_state as Handler,
        ),
        (
            row(
                "editor.vehicle_tuning_set",
                "vehicle_tuning_set",
                "Set one field of the edited vehicle, trailer, aircraft, bike or hover hull (one undo step). A \
                 value the config refuses changes nothing and answers data.reason with the config's vehicle_*, \
                 trailer_*, aircraft_* or bike_* code, or the hover hull's field code.",
                SET_PARAMS,
                STATE,
                true,
            ),
            tuning_set as Handler,
        ),
        (
            row(
                "editor.vehicle_tuning_undo",
                "vehicle_tuning_undo",
                "Undo the last field edit or drag of the vehicle (a shared editor edit, reactivating its document when needed), trailer, aircraft, bike or hover hull.",
                ASSET_PARAMS,
                STATE,
                true,
            ),
            tuning_undo as Handler,
        ),
        (
            row(
                "editor.vehicle_tuning_redo",
                "vehicle_tuning_redo",
                "Redo the last undone edit of the vehicle (a shared editor edit), trailer, aircraft, bike or hover hull.",
                ASSET_PARAMS,
                STATE,
                true,
            ),
            tuning_redo as Handler,
        ),
        (
            row(
                "editor.vehicle_tuning_apply",
                "vehicle_tuning_apply",
                "Send the edited config to a play vehicle without a respawn (Vehicle::set_config) at the next \
                 editor frame; read the answer in vehicle_tuning_state.last_apply.",
                APPLY_PARAMS,
                "{ queued, entity }",
                true,
            ),
            tuning_apply as Handler,
        ),
        (
            row(
                "editor.vehicle_tuning_save",
                "vehicle_tuning_save",
                "Write the edited vehicle (.vehicle.ron v4), trailer (.trailer.ron), aircraft (.aircraft.ron), \
                 bike (.bike.ron) or hover hull (.hoverhull.ron), atomically, keeping the file's comments when \
                 the merge reproduces the asset.",
                SAVE_PARAMS,
                "{ written, comments_kept, notice }",
                true,
            ),
            tuning_save as Handler,
        ),
        (
            row(
                "editor.vehicle_tuning_run",
                "vehicle_tuning_run",
                "Drive the edited car on the proving ground (flat, ramp, slope or bump strip; towing the open \
                 trailer with tow) with a scripted input (accel or skidpad) and record the run: 0-100 km/h \
                 time, top speed, skid-pad lateral g, wheel contacts, the trailer's articulation.",
                RUN_PARAMS,
                METRICS,
                true,
            ),
            tuning_run as Handler,
        ),
        (
            row(
                "editor.vehicle_tuning_fly",
                "vehicle_tuning_fly",
                "Trim the open aircraft for level flight at a speed and altitude, fly the trim for fixed steps \
                 and record the flight: the trim's angle of attack and throttle, and how far it drifted.",
                FLY_PARAMS,
                FLIGHT,
                true,
            ),
            tuning_fly as Handler,
        ),
        (
            row(
                "editor.vehicle_tuning_ride",
                "vehicle_tuning_ride",
                "Ride the open bike or hover hull on the proving ground for a counted number of fixed steps \
                 with a held input. The first ride (or respawn) spawns it at rest; later rides carry on and \
                 step the document as it is now, so an edit reaches the riding bike or hover craft at its \
                 next step.",
                RIDE_PARAMS,
                RIDE,
                true,
            ),
            tuning_ride as Handler,
        ),
    ]
}

#[cfg(test)]
#[path = "vehicle_tuning/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "vehicle_tuning/tests_assets.rs"]
mod tests_assets;

#[cfg(test)]
#[path = "vehicle_tuning/tests_rides.rs"]
mod tests_rides;
