// SPDX-License-Identifier: MPL-2.0
//! The Vehicle Tuning panel's other documents (ADR-7745, ADR-8630): a `.trailer.ron`, a
//! `.aircraft.ron`, a `.bike.ron` and a `.hoverhull.ron` open beside the `.vehicle.ron`, with the same
//! forms, undo, comment-keeping save and proving-ground runs as the car.
//!
//! - [`TuningDoc`] is one open file of a [`TunedAsset`] kind: the asset as opened (and its text), the
//!   edited asset, its undo and redo stacks. Fields are dotted serde paths into the configuration
//!   (`chassis.chassis_mass`, `coupling.point.2`, `surfaces.0.span`, `mass`) or `name`, set through
//!   [`TuningDoc::set`] exactly as the car's are.
//! - A trailer is tried on the proving ground by towing it behind the tuned car
//!   (`regia_physics::vehicle_proving::run_proving_course`).
//! - An aircraft is tried by [`fly`]: the level-flight trim at a speed and altitude
//!   (`regia_physics::aero::trim::solve`), then that trim flown for a number of fixed steps; the
//!   record says how far the flight drifted from it.
//!
//! - A bike or a hover hull is tried by a **ride** ([`BikeRide`], [`HoverRide`]): the proving ground
//!   spawns one from the open document and steps it a counted number of fixed steps with a held input.
//!   The ride keeps its state between calls and reads the document's configuration at every step, so
//!   an edit, an undo or a redo reaches the riding bike or hover craft at its next step, without a
//!   respawn. The hover form edits the hull's `params`; the file's `limits` and `flight` are kept and
//!   stepped as written.
//!
//! Neither a trailer nor an aircraft has a live Apply to a play session: `Trailer` and `Aircraft`
//! have no `set_config` (listed in the release fragment's known issues).

use std::collections::BTreeMap;
use std::path::Path;

use regia::framework::det_hover::{HOVERHULL_FORMAT_VERSION, HoverHullAsset, HoverHullError};
use regia::physics::PhysicsWorld;
use regia::physics::aero::asset::{AIRCRAFT_FORMAT_VERSION, AircraftAsset};
use regia::physics::aero::trim::{TrimTarget, solve};
use regia::physics::aero::{AeroEnvironment, AeroError, Aircraft, AircraftConfig};
use regia::physics::vehicle_bike::asset::{BIKE_FORMAT_VERSION, BikeAsset, BikeAssetError};
use regia::physics::vehicle_bike::{BikeConfig, BikeInput, BikeState, FIXED_STEP_S, bike_step};
use regia::physics::vehicle_proving::{PROVING_DT, step_proving};
use regia::physics::vehicle_trailer::TrailerConfig;
use regia::physics::vehicle_trailer_asset::{TrailerAsset, TrailerAssetError};
use regia_core::math::Vec3;
use regia_detsim::hover::{FlightMode, HoverLimits, HoverParams, HoverState, WorldPhysics, step_with};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use regia_ecs::prelude::World;

use crate::shared_panel_history::{SharedDoc, SharedHistory, StepKind};
use crate::vehicle_tuning::{MAX_UNDO, TuningError, VehicleTuning, project_path_ext, step, write_atomic};

/// A file kind the Vehicle Tuning panel edits beside the car.
pub trait TunedAsset: Clone + PartialEq + Send + Sync + 'static {
    /// The configuration the forms edit.
    type Config: Serialize + DeserializeOwned + Clone + PartialEq;
    /// The file name ending (`.trailer.ron`).
    const EXT: &'static str;
    /// The kind's name in the agent protocol (`trailer`, `aircraft`, `bike`, `hoverhull`).
    const KIND: &'static str;

    /// Parses and validates a file's text.
    ///
    /// # Errors
    /// The kind's load error, as a [`TuningError`] with its code.
    fn from_ron(text: &str) -> Result<Self, TuningError>;
    /// The canonical text.
    ///
    /// # Errors
    /// The kind's write error.
    fn to_ron(&self) -> Result<String, TuningError>;
    /// Checks the name and the configuration.
    ///
    /// # Errors
    /// The kind's validation error.
    fn validate(&self) -> Result<(), TuningError>;
    /// A new asset with the kind's default configuration.
    fn named(name: &str) -> Self;
    /// The display name.
    fn name(&self) -> &str;
    /// Sets the display name.
    fn set_name(&mut self, name: String);
    /// The configuration.
    fn config(&self) -> &Self::Config;
    /// The configuration, to replace.
    fn config_mut(&mut self) -> &mut Self::Config;
    /// Sets the format version (the file's own, for the comment-keeping merge).
    fn set_format_version(&mut self, version: u32);
}

fn trailer_error(e: &TrailerAssetError) -> TuningError {
    TuningError {
        code: e.code().to_string(),
        field: None,
        message: e.to_string(),
    }
}

fn aero_error(e: &AeroError) -> TuningError {
    let field = match e {
        AeroError::InvalidConfig { field } => Some(field.clone()),
        _ => None,
    };
    TuningError {
        code: e.code().to_string(),
        field,
        message: e.to_string(),
    }
}

fn bike_error(e: &BikeAssetError) -> TuningError {
    TuningError {
        code: e.code().to_string(),
        field: e.field_name().map(str::to_string),
        message: e.to_string(),
    }
}

fn hover_error(e: &HoverHullError) -> TuningError {
    TuningError {
        code: e.code().to_string(),
        field: e.field_name().map(str::to_string),
        message: e.to_string(),
    }
}

impl TunedAsset for TrailerAsset {
    type Config = TrailerConfig;
    const EXT: &'static str = ".trailer.ron";
    const KIND: &'static str = "trailer";

    fn from_ron(text: &str) -> Result<Self, TuningError> {
        Self::from_ron(text).map_err(|e| trailer_error(&e))
    }
    fn to_ron(&self) -> Result<String, TuningError> {
        Self::to_ron(self).map_err(|e| trailer_error(&e))
    }
    fn validate(&self) -> Result<(), TuningError> {
        Self::validate(self).map_err(|e| trailer_error(&e))
    }
    fn named(name: &str) -> Self {
        Self::new(name, TrailerConfig::default())
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn set_name(&mut self, name: String) {
        self.name = name;
    }
    fn config(&self) -> &TrailerConfig {
        &self.config
    }
    fn config_mut(&mut self) -> &mut TrailerConfig {
        &mut self.config
    }
    fn set_format_version(&mut self, version: u32) {
        self.format_version = version;
    }
}

impl TunedAsset for AircraftAsset {
    type Config = AircraftConfig;
    const EXT: &'static str = ".aircraft.ron";
    const KIND: &'static str = "aircraft";

    fn from_ron(text: &str) -> Result<Self, TuningError> {
        Self::from_ron(text).map_err(|e| aero_error(&e))
    }
    fn to_ron(&self) -> Result<String, TuningError> {
        Self::to_ron(self).map_err(|e| aero_error(&e))
    }
    fn validate(&self) -> Result<(), TuningError> {
        Self::validate(self).map_err(|e| aero_error(&e))
    }
    fn named(name: &str) -> Self {
        Self {
            format_version: AIRCRAFT_FORMAT_VERSION,
            name: name.to_string(),
            config: AircraftConfig::default(),
        }
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn set_name(&mut self, name: String) {
        self.name = name;
    }
    fn config(&self) -> &AircraftConfig {
        &self.config
    }
    fn config_mut(&mut self) -> &mut AircraftConfig {
        &mut self.config
    }
    fn set_format_version(&mut self, version: u32) {
        self.format_version = version;
    }
}

impl TunedAsset for BikeAsset {
    type Config = BikeConfig;
    const EXT: &'static str = ".bike.ron";
    const KIND: &'static str = "bike";

    fn from_ron(text: &str) -> Result<Self, TuningError> {
        Self::from_ron(text).map_err(|e| bike_error(&e))
    }
    fn to_ron(&self) -> Result<String, TuningError> {
        Self::to_ron(self).map_err(|e| bike_error(&e))
    }
    fn validate(&self) -> Result<(), TuningError> {
        Self::validate(self).map_err(|e| bike_error(&e))
    }
    fn named(name: &str) -> Self {
        Self {
            format_version: BIKE_FORMAT_VERSION,
            name: name.to_string(),
            config: BikeConfig::default(),
            notes: None,
        }
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn set_name(&mut self, name: String) {
        self.name = name;
    }
    fn config(&self) -> &BikeConfig {
        &self.config
    }
    fn config_mut(&mut self) -> &mut BikeConfig {
        &mut self.config
    }
    fn set_format_version(&mut self, version: u32) {
        self.format_version = version;
    }
}

/// The form edits the hull's `params` (`ride_height`, `thrust_accel`, ...); `limits`, `flight` and
/// `notes` are kept as the file wrote them.
impl TunedAsset for HoverHullAsset {
    type Config = HoverParams;
    const EXT: &'static str = ".hoverhull.ron";
    const KIND: &'static str = "hoverhull";

    fn from_ron(text: &str) -> Result<Self, TuningError> {
        Self::from_ron(text).map_err(|e| hover_error(&e))
    }
    fn to_ron(&self) -> Result<String, TuningError> {
        Self::to_ron(self).map_err(|e| hover_error(&e))
    }
    fn validate(&self) -> Result<(), TuningError> {
        Self::validate(self).map_err(|e| hover_error(&e))
    }
    fn named(name: &str) -> Self {
        Self {
            format_version: HOVERHULL_FORMAT_VERSION,
            name: name.to_string(),
            params: HoverParams::DEFAULT_HULL,
            limits: None,
            flight: None,
            notes: None,
        }
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn set_name(&mut self, name: String) {
        self.name = name;
    }
    fn config(&self) -> &HoverParams {
        &self.params
    }
    fn config_mut(&mut self) -> &mut HoverParams {
        &mut self.params
    }
    fn set_format_version(&mut self, version: u32) {
        self.format_version = version;
    }
}

/// `asset` with `field` set to `value`, validated.
///
/// # Errors
/// `vehicle_tuning_bad_field` for a path the configuration does not have,
/// `vehicle_tuning_bad_value` for a value of the wrong shape, or the kind's validation code.
pub fn with_field<A: TunedAsset>(asset: &A, field: &str, value: Value) -> Result<A, TuningError> {
    let mut next = asset.clone();
    let refuse = |code: &str, message: String| TuningError {
        code: code.to_string(),
        field: Some(field.to_string()),
        message,
    };
    if field == "name" {
        let name = value
            .as_str()
            .ok_or_else(|| refuse("vehicle_tuning_bad_value", "`name` is a string".to_string()))?;
        next.set_name(name.to_string());
    } else {
        let bad_field = || {
            refuse(
                "vehicle_tuning_bad_field",
                format!("the {} configuration has no field `{field}`", A::KIND),
            )
        };
        if field.is_empty() {
            return Err(bad_field());
        }
        let mut root = serde_json::to_value(asset.config())
            .map_err(|e| refuse("vehicle_tuning_bad_value", e.to_string()))?;
        let mut slot = &mut root;
        for segment in field.split('.') {
            slot = step(slot, segment).ok_or_else(bad_field)?;
        }
        *slot = value;
        *next.config_mut() = serde_json::from_value(root)
            .map_err(|e| refuse("vehicle_tuning_bad_value", format!("`{field}`: {e}")))?;
    }
    next.validate().map_err(|mut e| {
        e.field = e.field.or_else(|| Some(field.to_string()));
        e
    })?;
    Ok(next)
}

/// The text a save writes for `asset`: merged into `source` (whose asset was `opened`) when that
/// reproduces `asset` exactly, else the canonical text. The flag says the comments were kept.
///
/// # Errors
/// The kind's write error.
pub fn save_text<A: TunedAsset>(
    source: Option<&str>,
    opened: Option<&A>,
    asset: &A,
) -> Result<(String, bool), TuningError> {
    let canonical = asset.to_ron()?;
    let (Some(source), Some(opened)) = (source, opened) else {
        return Ok((canonical, false));
    };
    let old = opened.to_ron()?;
    match regia_ui::ron_merge::merge_ron_text(source, &old, &canonical) {
        Some(text) if A::from_ron(&text).as_ref() == Ok(asset) => Ok((text, true)),
        _ => Ok((canonical, false)),
    }
}

/// One open file of kind `A`: what the panel's section for it and the agent methods with
/// `asset: "trailer"` / `"aircraft"` change.
#[derive(Debug, Clone, PartialEq)]
pub struct TuningDoc<A> {
    /// The open file, relative to the project; `None` before an open.
    pub path: Option<String>,
    /// The asset as it was opened or last saved, at the file's own format version.
    pub opened: Option<A>,
    /// The file's text as it was opened or last saved (`None` for a created asset not saved yet).
    pub source: Option<String>,
    /// The asset being edited.
    pub asset: Option<A>,
    /// Earlier assets, newest last.
    pub undo: Vec<A>,
    /// Undone assets, newest last.
    pub redo: Vec<A>,
    /// The field a drag is editing: further edits of it join the same undo step.
    pub drag: Option<String>,
    /// The last refused edit or failed action.
    pub error: Option<TuningError>,
    /// What the last save did, for the status line.
    pub notice: Option<String>,
    /// The last save kept the file's comments (`None` before a save).
    pub comments_kept: Option<bool>,
    /// The edits on their way to the editor's shared history (P-322).
    shared_history: SharedHistory<A>,
    /// Files opened before the open one, by path, as they were left: a shared undo of an edit to
    /// one of them opens it again.
    retained: BTreeMap<String, RetainedDoc<A>>,
}

/// A file the document had open before, as it was left.
#[derive(Debug, Clone, PartialEq)]
struct RetainedDoc<A> {
    opened: Option<A>,
    source: Option<String>,
    asset: Option<A>,
}

impl<A> Default for TuningDoc<A> {
    fn default() -> Self {
        Self {
            path: None,
            opened: None,
            source: None,
            asset: None,
            undo: Vec::new(),
            redo: Vec::new(),
            drag: None,
            error: None,
            notice: None,
            comments_kept: None,
            shared_history: SharedHistory::default(),
            retained: BTreeMap::new(),
        }
    }
}

impl<A: TunedAsset> TuningDoc<A> {
    /// The asset being edited.
    ///
    /// # Errors
    /// `vehicle_tuning_nothing_open` before an open.
    pub fn current(&self) -> Result<&A, TuningError> {
        self.asset.as_ref().ok_or_else(|| TuningError {
            code: "vehicle_tuning_nothing_open".to_string(),
            field: None,
            message: format!("open a {} first", A::EXT),
        })
    }

    /// Whether the edited asset differs from the file.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        match (&self.asset, &self.opened) {
            (Some(asset), Some(opened)) => asset.config() != opened.config() || asset.name() != opened.name(),
            (Some(_), None) => true,
            _ => false,
        }
    }

    /// Opens `rel` under the project; with `create`, a missing file starts as the kind's default
    /// named after the file (written at the first save), and an existing file is refused.
    ///
    /// # Errors
    /// The path codes, `vehicle_tuning_not_found`, `vehicle_tuning_exists`, `vehicle_tuning_io`, or
    /// the kind's load code (`trailer_*`, `aircraft_*`).
    pub fn open(&mut self, project: Option<&Path>, rel: &str, create: bool) -> Result<(), TuningError> {
        let file = project_path_ext(project, rel, A::EXT)?;
        let simple = |code: &str, message: String| TuningError {
            code: code.to_string(),
            field: None,
            message,
        };
        let (opened, source, asset) = if file.is_file() {
            if create {
                return Err(simple(
                    "vehicle_tuning_exists",
                    format!("{rel} already exists (create starts a new file only)"),
                ));
            }
            let text = std::fs::read_to_string(&file)
                .map_err(|e| simple("vehicle_tuning_io", format!("{}: {e}", file.display())))?;
            let asset = A::from_ron(&text)?;
            let mut opened = asset.clone();
            if let Ok(found) = regia_core::version::peek_ron_version(&text) {
                opened.set_format_version(found);
            }
            (Some(opened), Some(text), asset)
        } else if create {
            let name = Path::new(rel)
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(A::EXT))
                .filter(|n| !n.is_empty())
                .unwrap_or(A::KIND);
            (None, None, A::named(name))
        } else {
            return Err(simple(
                "vehicle_tuning_not_found",
                format!("{rel} does not exist (pass create to start a new one)"),
            ));
        };
        self.retain_open();
        *self = Self {
            path: Some(rel.to_string()),
            opened,
            source,
            asset: Some(asset),
            shared_history: std::mem::take(&mut self.shared_history),
            retained: std::mem::take(&mut self.retained),
            ..Self::default()
        };
        self.retained.remove(rel);
        Ok(())
    }

    /// Sets `field` to `value`: one undo step, or part of the open drag's step.
    ///
    /// # Errors
    /// `vehicle_tuning_nothing_open` and the codes of [`with_field`].
    pub fn set(&mut self, field: &str, value: Value, drag: bool) -> Result<(), TuningError> {
        let current = self.current()?;
        let next = with_field(current, field, value)?;
        if next == *current {
            return Ok(());
        }
        let joins = drag && self.drag.as_deref() == Some(field);
        let kind = match (joins, drag) {
            (true, _) => StepKind::Continue,
            (false, true) => StepKind::Start,
            (false, false) => StepKind::Discrete,
        };
        if let (Some(path), Some(before)) = (self.path.clone(), self.asset.clone()) {
            self.shared_history.record(0, &path, &before, &next, kind, field);
        }
        if !joins
            && !self.shared_history.is_shared()
            && let Some(previous) = self.asset.clone()
        {
            self.undo.push(previous);
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

    /// Ends a drag.
    pub fn end_drag(&mut self) {
        self.drag = None;
    }

    /// Steps back one edit. Returns whether there was one.
    pub fn undo(&mut self) -> bool {
        self.drag = None;
        if self.shared_history.is_shared() {
            return self.shared_history.navigate(false);
        }
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
        self.drag = None;
        if self.shared_history.is_shared() {
            return self.shared_history.navigate(true);
        }
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

    /// Writes the edited asset to `rel` (default: the open file), atomically. Returns the path.
    ///
    /// # Errors
    /// `vehicle_tuning_nothing_open`, `vehicle_tuning_no_path`, the path codes, `vehicle_tuning_io`.
    pub fn save(&mut self, project: Option<&Path>, rel: Option<&str>) -> Result<String, TuningError> {
        let asset = self.current()?.clone();
        let rel = rel
            .map(str::to_string)
            .or_else(|| self.path.clone())
            .ok_or_else(|| TuningError {
                code: "vehicle_tuning_no_path".to_string(),
                field: None,
                message: "give a path to save to".to_string(),
            })?;
        let file = project_path_ext(project, &rel, A::EXT)?;
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

    /// The document's state for `editor.vehicle_tuning_state`.
    #[must_use]
    pub fn json(&self) -> Value {
        let validation = self.asset.as_ref().map(|asset| match asset.validate() {
            Ok(()) => json!({ "ok": true }),
            Err(e) => json!({ "ok": false, "code": e.code, "message": e.message }),
        });
        json!({
            "path": self.path,
            "open": self.asset.is_some(),
            "name": self.asset.as_ref().map(|a| a.name().to_string()),
            "dirty": self.is_dirty(),
            "config": self.asset.as_ref().and_then(|a| serde_json::to_value(a.config()).ok()),
            "validation": validation,
            "undo_depth": self.undo_depth(),
            "redo_depth": self.redo_depth(),
            "error": self.error,
            "notice": self.notice,
            "comments_kept": self.comments_kept,
        })
    }
}

impl<A: TunedAsset> TuningDoc<A> {
    /// Undo depth: the editor's shared history once it holds this document's edits, else its own.
    #[must_use]
    pub fn undo_depth(&self) -> usize {
        if self.shared_history.is_shared() {
            self.shared_history.undo_depth()
        } else {
            self.undo.len()
        }
    }

    /// Redo depth, as [`TuningDoc::undo_depth`].
    #[must_use]
    pub fn redo_depth(&self) -> usize {
        if self.shared_history.is_shared() {
            self.shared_history.redo_depth()
        } else {
            self.redo.len()
        }
    }

    /// Keeps the open file's state for a later shared undo of one of its edits.
    fn retain_open(&mut self) {
        if let Some(path) = self.path.clone() {
            self.retained.insert(
                path,
                RetainedDoc {
                    opened: self.opened.clone(),
                    source: self.source.clone(),
                    asset: self.asset.clone(),
                },
            );
        }
    }

    /// A shared history step: `asset` becomes the edited asset of `path`, opening that file
    /// again (as it was left) when another one is open now.
    fn restore_step(&mut self, path: &str, asset: &A) {
        if self.path.as_deref() != Some(path) {
            self.retain_open();
            let doc = self.retained.remove(path);
            self.path = Some(path.to_owned());
            self.opened = doc.as_ref().and_then(|d| d.opened.clone());
            self.source = doc.and_then(|d| d.source);
            self.notice = None;
            self.comments_kept = None;
        }
        self.asset = Some(asset.clone());
        self.drag = None;
        self.error = None;
    }
}

/// The trailer document on the editor's shared history (P-322, `crate::shared_panel_history`).
pub(crate) struct TrailerHistory;

impl SharedDoc for TrailerHistory {
    type State = TrailerAsset;
    const PANEL: &'static str = "Vehicle Tuning";

    fn histories(world: &mut World) -> Vec<&mut SharedHistory<TrailerAsset>> {
        world
            .get_resource_mut::<VehicleTuning>()
            .map(|tuning| vec![&mut tuning.into_inner().others.trailer.shared_history])
            .unwrap_or_default()
    }

    fn restore(world: &mut World, _doc: u64, path: &str, state: &TrailerAsset) {
        if let Some(tuning) = world.get_resource_mut::<VehicleTuning>() {
            tuning.into_inner().others.trailer.restore_step(path, state);
        }
    }
    fn open_doc(world: &World) -> Option<(u64, String)> {
        let path = world
            .get_resource::<VehicleTuning>()?
            .others
            .trailer
            .path
            .clone()?;
        Some((0, path))
    }
}

/// The aircraft document on the editor's shared history (P-322, `crate::shared_panel_history`).
pub(crate) struct AircraftHistory;

impl SharedDoc for AircraftHistory {
    type State = AircraftAsset;
    const PANEL: &'static str = "Vehicle Tuning";

    fn histories(world: &mut World) -> Vec<&mut SharedHistory<AircraftAsset>> {
        world
            .get_resource_mut::<VehicleTuning>()
            .map(|tuning| vec![&mut tuning.into_inner().others.aircraft.shared_history])
            .unwrap_or_default()
    }

    fn restore(world: &mut World, _doc: u64, path: &str, state: &AircraftAsset) {
        if let Some(tuning) = world.get_resource_mut::<VehicleTuning>() {
            tuning.into_inner().others.aircraft.restore_step(path, state);
        }
    }
    fn open_doc(world: &World) -> Option<(u64, String)> {
        let path = world
            .get_resource::<VehicleTuning>()?
            .others
            .aircraft
            .path
            .clone()?;
        Some((0, path))
    }
}

/// The longest flight, seconds.
pub const MAX_FLIGHT_SECONDS: f32 = 120.0;

/// A level-flight test of an aircraft configuration: the trim, then the trim flown.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FlightRun {
    /// The run's name in the table.
    pub label: String,
    /// Target true airspeed, m/s.
    pub speed: f32,
    /// Altitude above sea level, meters.
    pub altitude: f32,
    /// Seconds flown.
    pub seconds: f32,
    /// The trim's angle of attack, degrees.
    pub alpha_deg: f32,
    /// The trim's pitch attitude, degrees.
    pub pitch_deg: f32,
    /// The trim's throttle, 0 to 1.
    pub throttle: f32,
    /// The trim's pitch input.
    pub pitch_input: f32,
    /// Some wing is stalled at the trim.
    pub stalled: bool,
    /// The largest altitude change from the start over the flight, meters.
    pub max_altitude_change_m: f32,
    /// The largest airspeed change from the target, percent.
    pub max_speed_change_pct: f32,
    /// The largest bank, degrees (absolute).
    pub max_bank_deg: f32,
    /// The world's state digest at the end: equal flights have equal digests.
    pub digest: String,
    /// The aircraft's mass, kilograms (the value the flight read).
    pub mass: f32,
}

/// Trims `config` for level flight at `speed` m/s and `altitude` m, then flies the trim for
/// `seconds` in a world with nothing else, at the proving ground's fixed step and gravity.
///
/// # Errors
/// `vehicle_tuning_bad_seconds`, or the `aircraft_*` code of the trim, the spawn or a step.
pub fn fly(
    config: &AircraftConfig,
    speed: f32,
    altitude: f32,
    seconds: f32,
    label: String,
) -> Result<FlightRun, TuningError> {
    let _span = tracing::info_span!("editor.vehicle_tuning_fly").entered();
    if !(seconds.is_finite() && seconds > 0.0 && seconds <= MAX_FLIGHT_SECONDS) {
        return Err(TuningError {
            code: "vehicle_tuning_bad_seconds".to_string(),
            field: None,
            message: format!("`seconds` must be above 0 and at most {MAX_FLIGHT_SECONDS}"),
        });
    }
    let trim = solve(config, &TrimTarget::level(speed, altitude)).map_err(|e| aero_error(&e))?;
    let mut physics = PhysicsWorld::default();
    let mut plane = Aircraft::spawn(
        &mut physics,
        config.clone(),
        Vec3::new(0.0, altitude, 0.0),
        trim.rotation(),
        trim.velocity(),
    )
    .map_err(|e| aero_error(&e))?;
    let env = AeroEnvironment::default();
    let steps = (seconds / PROVING_DT).round().max(1.0) as u32;
    let (mut dy, mut dv, mut bank) = (0.0_f32, 0.0_f32, 0.0_f32);
    for _ in 0..steps {
        plane
            .update_with_layers(&mut physics, trim.input(), &env, PROVING_DT, None)
            .map_err(|e| aero_error(&e))?;
        step_proving(&mut physics);
        let state = plane.state(&physics).map_err(|e| aero_error(&e))?;
        dy = dy.max((state.translation.y - altitude).abs());
        dv = dv.max((state.linear_velocity.length() - speed).abs() / speed);
        bank = bank.max(state.bank.abs());
    }
    Ok(FlightRun {
        label,
        speed,
        altitude,
        seconds,
        alpha_deg: trim.alpha.to_degrees(),
        pitch_deg: trim.pitch_attitude.to_degrees(),
        throttle: trim.throttle,
        pitch_input: trim.pitch_input,
        stalled: trim.stalled,
        max_altitude_change_m: dy,
        max_speed_change_pct: dv * 100.0,
        max_bank_deg: bank.to_degrees(),
        digest: format!("{:016x}", physics.state_digest()),
        mass: config.mass,
    })
}

/// The trailer, aircraft, bike and hover-hull documents, the flights and the rides: what the panel
/// holds beside the car.
#[derive(Debug, Default)]
pub struct OtherDocs {
    /// The open `.trailer.ron`.
    pub trailer: TuningDoc<TrailerAsset>,
    /// The open `.aircraft.ron`.
    pub aircraft: TuningDoc<AircraftAsset>,
    /// Recorded flights, oldest first (at most `MAX_RUNS`).
    pub flights: Vec<FlightRun>,
    /// Flights started so far (names `flight N`).
    pub flight_count: u32,
    /// The open `.bike.ron` (ADR-8630).
    pub bike: TuningDoc<BikeAsset>,
    /// The open `.hoverhull.ron` (ADR-8630).
    pub hoverhull: TuningDoc<HoverHullAsset>,
    /// The bike riding on the proving ground, once a ride spawned it.
    pub bike_ride: Option<BikeRide>,
    /// The hover craft riding on the proving ground, once a ride spawned it.
    pub hover_ride: Option<HoverRide>,
}

/// The most fixed steps one ride call steps (two minutes at 60 Hz).
pub const MAX_RIDE_STEPS: u32 = 7200;

/// What a ride holds for its steps: the bike's steer (rad), speed (m/s) and drive, or the hover
/// craft's `[throttle, strafe, steer]` input.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RideInput {
    /// Steer: the bike's front steer in radians, the hover craft's steer axis (-1 to 1).
    pub steer: f32,
    /// The bike's travel speed, m/s.
    pub speed: f32,
    /// The bike's drive command.
    pub drive: f32,
    /// The hover craft's throttle axis (-1 to 1).
    pub throttle: f32,
    /// The hover craft's strafe axis (-1 to 1).
    pub strafe: f32,
}

/// The wheel-ray length on the proving ground's flat floor, meters: both wheels in contact.
pub const RIDE_RAY_M: f32 = 0.3;

/// The bike on the proving ground: its state and the steps it has taken since it spawned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BikeRide {
    /// The bike's state after the last step.
    pub state: BikeState,
    /// Fixed steps since the spawn.
    pub steps: u64,
}

impl BikeRide {
    /// An upright bike at rest.
    #[must_use]
    pub fn spawn() -> Self {
        Self {
            state: BikeState::default(),
            steps: 0,
        }
    }

    /// Steps the bike `steps` times on `config` with `input` held, at [`FIXED_STEP_S`], both rays at
    /// [`RIDE_RAY_M`]. A refused step stops the ride where it was.
    ///
    /// # Errors
    /// `bike_bad_input` when the configuration or the input is outside the model's range.
    pub fn ride(&mut self, config: &BikeConfig, steps: u32, input: RideInput) -> Result<(), TuningError> {
        let _span = tracing::debug_span!("editor.vehicle_tuning_ride_bike", steps).entered();
        let command = BikeInput {
            dt: FIXED_STEP_S,
            steer: input.steer,
            speed: input.speed,
            drive: input.drive,
            front_ray: RIDE_RAY_M,
            rear_ray: RIDE_RAY_M,
        };
        for _ in 0..steps {
            bike_step(&mut self.state, config, &command).map_err(|e| TuningError {
                code: e.code().to_string(),
                field: None,
                message: format!(
                    "{e} (steer within ±{} rad, speed within ±{} m/s, a finite drive)",
                    regia::physics::vehicle_bike::MAX_STEER_RAD,
                    regia::physics::vehicle_bike::MAX_SPEED_MPS
                ),
            })?;
            self.steps += 1;
        }
        Ok(())
    }

    /// The ride for `editor.vehicle_tuning_state` and `editor.vehicle_tuning_ride`.
    #[must_use]
    pub fn json(&self) -> Value {
        let s = self.state;
        json!({
            "total_steps": self.steps,
            "state": {
                "lean": s.lean,
                "steer": s.steer,
                "speed": s.speed,
                "drive": s.drive,
                "front_contact": s.front_contact,
                "rear_contact": s.rear_contact,
                "fallen": s.fallen,
            },
            "digest": format!("{:016x}", s.digest()),
        })
    }
}

/// The hover craft on the proving ground: its state, the steps it has taken since it spawned and the
/// steps whose state the step had to reset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HoverRide {
    /// The hull's state after the last step.
    pub state: HoverState,
    /// Fixed ticks since the spawn.
    pub steps: u64,
    /// Ticks the step reset a poisoned state (`StepReport::recovered`); 0 for a valid hull.
    pub recoveries: u64,
}

impl HoverRide {
    /// A hull at rest at its ride height over the flat floor (height 0).
    #[must_use]
    pub fn spawn(params: &HoverParams) -> Self {
        Self {
            state: HoverState {
                altitude: params.ride_height,
                prev_terrain: Some(0.0),
                ..HoverState::default()
            },
            steps: 0,
            recoveries: 0,
        }
    }

    /// Steps the hull `steps` ticks of `regia_detsim::tick::tick_dt` over a flat floor at height 0 on
    /// Earth ([`WorldPhysics::EARTH`]), with `[throttle, strafe, steer]` held. The numbers are the
    /// sanitized hull, the file's limits (or [`HoverLimits::DEFAULT`]) and the file's flight mode (or
    /// the default one), as a host steps a loaded hull.
    pub fn ride(&mut self, hull: &HoverHullAsset, steps: u32, input: RideInput) {
        let _span = tracing::debug_span!("editor.vehicle_tuning_ride_hover", steps).entered();
        let params = hull.params.sanitized();
        let limits = hull.limits.unwrap_or(HoverLimits::DEFAULT).sanitized();
        let climb = hull
            .flight
            .as_deref()
            .and_then(FlightMode::from_name)
            .unwrap_or_default()
            .climb();
        let axes = [input.throttle, input.strafe, input.steer];
        for _ in 0..steps {
            let report = step_with(
                &params,
                &limits,
                climb,
                &WorldPhysics::EARTH,
                &mut self.state,
                &axes,
                0.0,
                regia_detsim::tick::tick_dt(),
            );
            self.steps += 1;
            self.recoveries += u64::from(report.recovered);
        }
    }

    /// The ride for `editor.vehicle_tuning_state` and `editor.vehicle_tuning_ride`.
    #[must_use]
    pub fn json(&self) -> Value {
        let s = &self.state;
        json!({
            "total_steps": self.steps,
            "recoveries": self.recoveries,
            "state": {
                "velocity": s.velocity,
                "yaw": s.yaw,
                "yaw_rate": s.yaw_rate,
                "altitude": s.altitude,
                "airborne": s.airborne,
                "roll": s.roll,
                "pitch": s.pitch,
            },
        })
    }
}
