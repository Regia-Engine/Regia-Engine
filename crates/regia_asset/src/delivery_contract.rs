// SPDX-License-Identifier: MPL-2.0
//! Per-class delivery contract (`.contract.ron`, format `delivery-contract` v1, ADR-8401).
//!
//! The file stores the settings for one asset class: unit scale, up and forward axes, ground
//! origin, the LOD chain with triangle budgets, and the kit rules (joint influences, required
//! nodes, sockets and clips, material and texture caps, footprint and socket clearance).
//! Checking an observation against that file is [`check_delivery`]. Class names and numeric caps
//! are settings in the file.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The `.contract.ron` format version this build reads and writes.
pub const DELIVERY_CONTRACT_FORMAT_VERSION: u32 = 1;

/// File name suffix of a delivery contract (`prop.contract.ron`).
pub const DELIVERY_CONTRACT_SUFFIX: &str = ".contract.ron";

/// Default joint influences per vertex. A second influence set is five or more.
pub const DEFAULT_MAX_JOINT_INFLUENCES: u32 = 4;

/// Default material cap. Five materials fail. More than [`DEFAULT_MATERIAL_WARN_ABOVE`] warns.
pub const DEFAULT_MAX_MATERIALS: u32 = 4;

/// A material count above this, and still within the cap, is a warning. `0` disables it.
pub const DEFAULT_MATERIAL_WARN_ABOVE: u32 = 3;

/// Default longest texture edge, in pixels.
pub const DEFAULT_TEXTURE_MAX_EDGE: u32 = 2048;

/// Default relative band for a footprint radius (`abs(contract - measured) / measured`).
pub const DEFAULT_FOOTPRINT_TOLERANCE: f64 = 0.10;

fn default_max_joint_influences() -> u32 {
    DEFAULT_MAX_JOINT_INFLUENCES
}

fn default_max_materials() -> u32 {
    DEFAULT_MAX_MATERIALS
}

fn default_material_warn_above() -> u32 {
    DEFAULT_MATERIAL_WARN_ABOVE
}

fn default_texture_max_edge() -> u32 {
    DEFAULT_TEXTURE_MAX_EDGE
}

fn default_footprint_tolerance() -> f64 {
    DEFAULT_FOOTPRINT_TOLERANCE
}

fn default_name_count() -> u32 {
    1
}

/// A signed coordinate axis named by a delivery contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignedAxis {
    /// Positive X.
    PosX,
    /// Negative X.
    NegX,
    /// Positive Y.
    PosY,
    /// Negative Y.
    NegY,
    /// Positive Z.
    PosZ,
    /// Negative Z.
    NegZ,
}

impl SignedAxis {
    /// Every axis, in declaration order.
    pub const ALL: [SignedAxis; 6] = [
        SignedAxis::PosX,
        SignedAxis::NegX,
        SignedAxis::PosY,
        SignedAxis::NegY,
        SignedAxis::PosZ,
        SignedAxis::NegZ,
    ];

    /// The wire name (`pos_y`, `neg_z`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PosX => "pos_x",
            Self::NegX => "neg_x",
            Self::PosY => "pos_y",
            Self::NegY => "neg_y",
            Self::PosZ => "pos_z",
            Self::NegZ => "neg_z",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::PosX | Self::NegX => 0,
            Self::PosY | Self::NegY => 1,
            Self::PosZ | Self::NegZ => 2,
        }
    }

    fn positive(self) -> bool {
        matches!(self, Self::PosX | Self::PosY | Self::PosZ)
    }
}

/// How large one authored unit is, and how far a node scale may sit from 1.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScaleSettings {
    /// Metres represented by one unit of the source file. Must be finite and greater than 0.
    pub meters_per_unit: f64,
    /// A node scale component fails when it is farther than this from 1.
    pub identity_tolerance: f64,
}

/// The axes the asset must be authored in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AxisSettings {
    /// The up axis the observation must name.
    pub up: SignedAxis,
    /// The forward axis. It must not be parallel to [`Self::up`].
    pub forward: SignedAxis,
}

/// Where the origin sits relative to the mesh bounds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginSettings {
    /// Expected minimum along the up axis, in metres (a hovering asset sets its hover height).
    pub ground_offset_m: f64,
    /// How far below `ground_offset_m` the minimum may sit, and how far above it a fine mesh may sit.
    pub ground_tolerance_m: f64,
    /// How far above `ground_offset_m` a coarse LOD may sit. Simplified meshes lose their lowest points.
    pub coarse_floor_rise_m: f64,
    /// Each horizontal center must lie within this fraction of the largest span of the origin.
    pub center_fraction: f64,
}

/// A LOD whose triangle count is compared with another LOD.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LodRatio {
    /// The LOD being judged.
    pub name: String,
    /// The reference LOD (usually the budget LOD).
    pub of: String,
    /// Nominal `name / of` triangle ratio. A warning fires outside half to double this value.
    pub nominal: f64,
}

/// An exclusive triangle cap for one LOD.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LodCap {
    /// The LOD the cap applies to.
    pub name: String,
    /// The count must be strictly below this.
    pub max_triangles_exclusive: u64,
}

/// Which LODs exist and how their triangle counts must fall.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LodSettings {
    /// LODs that must be present.
    pub required: Vec<String>,
    /// LODs that may be present in addition to [`Self::required`].
    #[serde(default)]
    pub optional: Vec<String>,
    /// The LOD whose triangle count must lie in `lod0_min_triangles..=lod0_max_triangles`.
    pub budget_lod: String,
    /// Smallest accepted triangle count of [`Self::budget_lod`].
    pub lod0_min_triangles: u64,
    /// Largest accepted triangle count of [`Self::budget_lod`].
    pub lod0_max_triangles: u64,
    /// Order used for the strictly decreasing chain. Missing names are skipped.
    #[serde(default)]
    pub chain: Vec<String>,
    /// Ratio warnings. Empty means no ratio check.
    #[serde(default)]
    pub ratios: Vec<LodRatio>,
    /// Exclusive triangle caps.
    #[serde(default)]
    pub hard_caps: Vec<LodCap>,
    /// When true, each present chain LOD must have fewer triangles than the previous present one.
    pub require_decreasing: bool,
}

/// How many names must match a glob.
///
/// `pattern` is a glob: `*` matches any run of characters, including empty. A pattern with no
/// `*` matches one name exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NameCount {
    /// Glob or exact name (`socket_hatch*`, `hatch_open`).
    pub pattern: String,
    /// Minimum number of names that must match. Default 1.
    #[serde(default = "default_name_count")]
    pub count: u32,
}

/// The measured footprint must sit near this radius.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FootprintRule {
    /// Contract radius in metres.
    pub radius_m: f64,
    /// `abs(radius_m - measured) / measured` must be at most this. Default 0.10.
    /// A measured radius of 0 passes only when `radius_m` is 0.
    #[serde(default = "default_footprint_tolerance")]
    pub tolerance: f64,
}

/// A matching socket's opening must be at least this wide and this tall.
///
/// An opening with no measured size fails closed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SocketClearance {
    /// Glob or exact socket name.
    pub pattern: String,
    /// Minimum opening width, in metres.
    pub min_width_m: f64,
    /// Minimum opening height, in metres.
    pub min_height_m: f64,
}

/// One `.contract.ron` document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryContract {
    /// Format version. Newer than [`DELIVERY_CONTRACT_FORMAT_VERSION`] is refused.
    pub format_version: u32,
    /// Asset-class label. A setting, not a built-in class table.
    pub class_name: String,
    /// Unit scale.
    pub scale: ScaleSettings,
    /// Authored axes.
    pub axes: AxisSettings,
    /// Ground origin.
    pub origin: OriginSettings,
    /// LOD chain and budgets.
    pub lod: LodSettings,
    /// Most joint influences on one vertex. Default 4. A second influence set counts as at least 5.
    #[serde(default = "default_max_joint_influences")]
    pub max_joint_influences: u32,
    /// Node names that must be present.
    #[serde(default)]
    pub required_nodes: Vec<NameCount>,
    /// Socket names that must be present.
    #[serde(default)]
    pub required_sockets: Vec<NameCount>,
    /// Clip names that must be present.
    #[serde(default)]
    pub required_clips: Vec<NameCount>,
    /// Most materials on the asset. Default 4.
    #[serde(default = "default_max_materials")]
    pub max_materials: u32,
    /// A count above this, and still within [`Self::max_materials`], is a warning. `0` disables it.
    #[serde(default = "default_material_warn_above")]
    pub material_warn_above: u32,
    /// Longest texture edge, in pixels. Default 2048.
    #[serde(default = "default_texture_max_edge")]
    pub texture_max_edge: u32,
    /// Footprint radius. Absent skips the check.
    #[serde(default)]
    pub footprint: Option<FootprintRule>,
    /// Opening size each matching socket must clear.
    #[serde(default)]
    pub socket_clearance: Vec<SocketClearance>,
}

/// Errors reading a `.contract.ron`.
#[derive(Debug, thiserror::Error)]
pub enum ContractFileError {
    /// The text is not valid RON for the format.
    #[error("RON: {0}")]
    Ron(#[from] ron::error::SpannedError),
    /// Serializing failed.
    #[error("RON: {0}")]
    RonWrite(#[from] ron::Error),
    /// Written by a newer engine.
    #[error(
        "delivery contract format_version {found} is newer than this engine's {expected}; upgrade the engine"
    )]
    Newer {
        /// The version in the file.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },
    /// A setting is missing or inconsistent.
    #[error("invalid delivery contract: {0}")]
    Invalid(String),
}

impl ContractFileError {
    /// The agent error code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Ron(_) | Self::RonWrite(_) => "contract_parse",
            Self::Newer { .. } => "contract_version_newer",
            Self::Invalid(_) => "contract_invalid",
        }
    }
}

impl DeliveryContract {
    /// Parses a `.contract.ron`, refusing a newer version before the typed parse.
    ///
    /// # Errors
    ///
    /// See [`ContractFileError`].
    pub fn from_ron_str(text: &str) -> Result<Self, ContractFileError> {
        if let Ok(found) = regia_core::version::peek_ron_version(text)
            && found > DELIVERY_CONTRACT_FORMAT_VERSION
        {
            return Err(ContractFileError::Newer {
                found,
                expected: DELIVERY_CONTRACT_FORMAT_VERSION,
            });
        }
        let file: Self = ron::from_str(text)?;
        if file.format_version == 0 {
            return Err(ContractFileError::Invalid(
                "format_version 0 does not exist; use 1".into(),
            ));
        }
        if file.format_version != DELIVERY_CONTRACT_FORMAT_VERSION {
            return Err(ContractFileError::Newer {
                found: file.format_version,
                expected: DELIVERY_CONTRACT_FORMAT_VERSION,
            });
        }
        file.validate()?;
        Ok(file)
    }

    /// Checks class, axes, scale, origin and LOD settings.
    ///
    /// # Errors
    ///
    /// [`ContractFileError::Invalid`] with the first problem.
    pub fn validate(&self) -> Result<(), ContractFileError> {
        validate_class_name(&self.class_name).map_err(ContractFileError::Invalid)?;
        if self.axes.up.index() == self.axes.forward.index() {
            return Err(ContractFileError::Invalid(
                "up and forward must name different axes (they are parallel)".into(),
            ));
        }
        finite_positive(self.scale.meters_per_unit, "scale.meters_per_unit")
            .map_err(ContractFileError::Invalid)?;
        finite_non_negative(self.scale.identity_tolerance, "scale.identity_tolerance")
            .map_err(ContractFileError::Invalid)?;
        if !self.origin.ground_offset_m.is_finite() {
            return Err(ContractFileError::Invalid(
                "origin.ground_offset_m must be finite".into(),
            ));
        }
        finite_non_negative(self.origin.ground_tolerance_m, "origin.ground_tolerance_m")
            .map_err(ContractFileError::Invalid)?;
        finite_non_negative(self.origin.coarse_floor_rise_m, "origin.coarse_floor_rise_m")
            .map_err(ContractFileError::Invalid)?;
        finite_positive(self.origin.center_fraction, "origin.center_fraction")
            .map_err(ContractFileError::Invalid)?;
        validate_lod(&self.lod).map_err(ContractFileError::Invalid)?;
        validate_kit(self).map_err(ContractFileError::Invalid)?;
        Ok(())
    }

    /// The canonical RON text, with a trailing newline.
    ///
    /// # Errors
    ///
    /// Serialization failed.
    pub fn to_ron_string(&self) -> Result<String, ContractFileError> {
        let mut text = ron::ser::to_string_pretty(self, regia_ecs::serialize::canonical_pretty_config())?;
        text.push('\n');
        Ok(text)
    }
}

/// The serde field-list fingerprint of `.contract.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn delivery_contract_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<DeliveryContract>(),
        regia_core::schema::serde_field_names::<ScaleSettings>(),
        regia_core::schema::serde_field_names::<AxisSettings>(),
        regia_core::schema::serde_field_names::<OriginSettings>(),
        regia_core::schema::serde_field_names::<LodSettings>(),
        regia_core::schema::serde_field_names::<LodRatio>(),
        regia_core::schema::serde_field_names::<LodCap>(),
        regia_core::schema::serde_variant_names::<SignedAxis>(),
        regia_core::schema::serde_field_names::<NameCount>(),
        regia_core::schema::serde_field_names::<FootprintRule>(),
        regia_core::schema::serde_field_names::<SocketClearance>(),
    ])
}

/// The `from` versions of the registered migration steps (none at v1).
#[must_use]
pub fn delivery_contract_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// What was measured on one asset, separate from the contract file.
///
/// `authored_up` is explicit. Bounds along the forward axis are not evidence of the up axis.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeliveryObservation {
    /// Scale of each node that has one, in source order. Empty skips the scale check.
    pub node_scales: Vec<[f64; 3]>,
    /// The up axis the asset was authored with.
    pub authored_up: SignedAxis,
    /// The forward axis, when the asset names one.
    #[serde(default)]
    pub authored_forward: Option<SignedAxis>,
    /// Axis-aligned bounds minimum in source X, Y, Z. Ignored for the ground check when
    /// [`Self::bounds_present`] is false, and still refused when a component is non-finite.
    pub bounds_min: [f64; 3],
    /// Axis-aligned bounds maximum in source X, Y, Z.
    pub bounds_max: [f64; 3],
    /// Whether `bounds_min` and `bounds_max` are real mesh bounds.
    pub bounds_present: bool,
    /// A coarse LOD may sit higher above the ground offset.
    pub coarse_lod: bool,
    /// Triangle counts keyed by the contract's LOD names.
    pub lod_triangles: BTreeMap<String, u64>,
    /// File name the vendor report uses when a finding does not name one.
    #[serde(default)]
    pub asset_name: String,
    /// Node names, in file order.
    #[serde(default)]
    pub nodes: Vec<String>,
    /// Sockets, in file order.
    #[serde(default)]
    pub sockets: Vec<MeasuredSocket>,
    /// Clip names, in file order.
    #[serde(default)]
    pub clips: Vec<String>,
    /// Materials on the asset. Zero skips nothing: a cap still applies, and zero is within it.
    #[serde(default)]
    pub material_count: u32,
    /// Material names, for the report. The count is [`Self::material_count`].
    #[serde(default)]
    pub material_names: Vec<String>,
    /// Textures and their edges. Empty skips the texture check.
    #[serde(default)]
    pub textures: Vec<MeasuredTexture>,
    /// Skinned primitives. Empty skips the joint check.
    #[serde(default)]
    pub skins: Vec<MeasuredSkin>,
    /// LOD0 horizontal radius from the origin, in metres. Absent skips the footprint check.
    #[serde(default)]
    pub footprint_radius_m: Option<f64>,
}

impl Default for DeliveryObservation {
    fn default() -> Self {
        Self {
            node_scales: Vec::new(),
            authored_up: SignedAxis::PosY,
            authored_forward: None,
            bounds_min: [0.0; 3],
            bounds_max: [0.0; 3],
            bounds_present: false,
            coarse_lod: false,
            lod_triangles: BTreeMap::new(),
            asset_name: String::new(),
            nodes: Vec::new(),
            sockets: Vec::new(),
            clips: Vec::new(),
            material_count: 0,
            material_names: Vec::new(),
            textures: Vec::new(),
            skins: Vec::new(),
            footprint_radius_m: None,
        }
    }
}

/// One socket and the opening that was measured on it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeasuredSocket {
    /// Socket name.
    pub name: String,
    /// Opening width in metres, after the node's scale. Absent when the size is unknown.
    #[serde(default)]
    pub width_m: Option<f64>,
    /// Opening height in metres, after the node's scale.
    #[serde(default)]
    pub height_m: Option<f64>,
    /// Where the size came from (`extras.size`, `extras.wh`, `extras.opening`, `mesh-bounds`, `unknown-size`).
    #[serde(default)]
    pub source: String,
}

/// One texture and its pixel edges. A missing edge is unreadable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasuredTexture {
    /// Texture name.
    pub name: String,
    /// Width in pixels, when the header could be read.
    #[serde(default)]
    pub width: Option<u32>,
    /// Height in pixels, when the header could be read.
    #[serde(default)]
    pub height: Option<u32>,
}

/// Joint influences measured on one primitive.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeasuredSkin {
    /// Mesh name.
    pub mesh: String,
    /// Primitive index inside the mesh.
    pub primitive: u32,
    /// Most non-zero influences on one vertex.
    pub influences: u32,
    /// The primitive carries a second influence set (`JOINTS_1` / `WEIGHTS_1`).
    #[serde(default)]
    pub second_set: bool,
    /// Weight sum of the first vertex that does not sum to 1. Absent when every vertex sums to 1.
    #[serde(default)]
    pub weight_sum: Option<f64>,
    /// A weight sat outside 0 to 1.
    #[serde(default)]
    pub weight_out_of_range: bool,
    /// A joint index sat past the skin's joint list.
    #[serde(default)]
    pub joint_out_of_range: bool,
    /// The primitive has joints without weights, or weights without joints.
    #[serde(default)]
    pub unpaired: bool,
}

/// Whether a finding fails the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindingSeverity {
    /// The asset fails the contract.
    Error,
    /// The asset is usable and the finding is reported.
    Warning,
}

/// One result of [`check_delivery`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Stable lowercase code (`wrong_up_axis`, `lod_ratio`).
    pub code: &'static str,
    /// Error or warning.
    pub severity: FindingSeverity,
    /// One line naming the measured value and the setting it missed.
    pub message: String,
    /// Asset the row names. Empty when the report writer supplies the file name.
    pub asset: String,
    /// `node`, `socket`, `clip`, `material`, `texture`, `skin`, `footprint`, or empty.
    pub subject_kind: &'static str,
    /// The node, socket, clip or other name the row is about.
    pub subject: String,
    /// What the contract asked for.
    pub expected: String,
    /// What the asset had.
    pub found: String,
}

/// Every finding for one observation, in a fixed order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryReport {
    /// Findings. Warnings do not fail [`Self::passed`].
    pub findings: Vec<Finding>,
}

impl DeliveryReport {
    /// Findings that fail the contract.
    pub fn errors(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == FindingSeverity::Error)
    }

    /// Findings that do not fail the contract.
    pub fn warnings(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == FindingSeverity::Warning)
    }

    /// True when there is no error finding. Warnings still pass.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.errors().next().is_none()
    }
}

/// Why [`check_delivery`] refused to judge the observation.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// A measured number is NaN or infinite.
    #[error("non-finite value in {field}")]
    NonFinite {
        /// Dotted field name.
        field: String,
    },
    /// The contract settings are inconsistent.
    #[error("{0}")]
    InvalidContract(String),
    /// The observation is shaped wrong (inverted bounds).
    #[error("{0}")]
    InvalidObservation(String),
}

impl CheckError {
    /// The agent error code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NonFinite { .. } | Self::InvalidObservation(_) => "observation_invalid",
            Self::InvalidContract(_) => "contract_invalid",
        }
    }
}

/// Judges `observation` against `contract`.
///
/// An authored up axis other than the contract's up axis is the error `wrong_up_axis`. A missing
/// forward axis is not that error. The nose warning only says whether geometry extends along the
/// contract forward axis.
///
/// # Errors
///
/// The contract settings are invalid, a measured number is non-finite, or the bounds are inverted.
pub fn check_delivery(
    contract: &DeliveryContract,
    observation: &DeliveryObservation,
) -> Result<DeliveryReport, CheckError> {
    contract
        .validate()
        .map_err(|e| CheckError::InvalidContract(e.to_string()))?;
    validate_observation(observation)?;
    let mut findings = Vec::new();
    push_axis_findings(contract, observation, &mut findings);
    push_scale_findings(contract, observation, &mut findings);
    push_origin_findings(contract, observation, &mut findings);
    push_lod_findings(
        &contract.lod,
        &contract.class_name,
        &observation.lod_triangles,
        &mut findings,
    );
    push_kit_findings(contract, observation, &mut findings);
    if !observation.asset_name.is_empty() {
        for finding in &mut findings {
            if finding.asset.is_empty() {
                finding.asset.clone_from(&observation.asset_name);
            }
        }
    }
    Ok(DeliveryReport { findings })
}

fn push_axis_findings(
    contract: &DeliveryContract,
    observation: &DeliveryObservation,
    findings: &mut Vec<Finding>,
) {
    if observation.authored_up != contract.axes.up {
        findings.push(error(
            "wrong_up_axis",
            format!(
                "authored up axis is {}; the contract requires {}",
                observation.authored_up.as_str(),
                contract.axes.up.as_str()
            ),
        ));
    }
    if let Some(forward) = observation.authored_forward
        && forward != contract.axes.forward
    {
        findings.push(error(
            "wrong_forward_axis",
            format!(
                "authored forward axis is {}; the contract requires {}",
                forward.as_str(),
                contract.axes.forward.as_str()
            ),
        ));
    }
}

fn push_scale_findings(
    contract: &DeliveryContract,
    observation: &DeliveryObservation,
    findings: &mut Vec<Finding>,
) {
    for (index, scale) in observation.node_scales.iter().enumerate() {
        let exaggerated = scale
            .iter()
            .any(|component| (component - 1.0).abs() > contract.scale.identity_tolerance);
        if exaggerated {
            findings.push(error(
                "unit_scale",
                format!(
                    "node {index} has scale [{}, {}, {}]; 1 unit = {} m and node scale stays 1",
                    scale[0], scale[1], scale[2], contract.scale.meters_per_unit
                ),
            ));
        }
    }
}

fn push_origin_findings(
    contract: &DeliveryContract,
    observation: &DeliveryObservation,
    findings: &mut Vec<Finding>,
) {
    if !observation.bounds_present {
        findings.push(warn(
            "origin_ground",
            "no mesh bounds, cannot check the ground origin",
        ));
        return;
    }
    let (min_up, max_up) = project_min_max(observation.bounds_min, observation.bounds_max, contract.axes.up);
    let rise = if observation.coarse_lod {
        contract.origin.coarse_floor_rise_m
    } else {
        contract.origin.ground_tolerance_m
    };
    let low = contract.origin.ground_offset_m - contract.origin.ground_tolerance_m;
    let high = contract.origin.ground_offset_m + rise;
    if min_up < low || min_up > high {
        findings.push(error(
            "origin_ground",
            format!(
                "bounds along {} run {min_up:.3} to {max_up:.3} m; the minimum must sit in {low:.3}..={high:.3} (ground offset {} m)",
                contract.axes.up.as_str(),
                contract.origin.ground_offset_m
            ),
        ));
    }
    if let Some(message) = horizontal_center_message(contract, observation) {
        findings.push(error("origin_center", message));
    }
    let (_min_forward, max_forward) = project_min_max(
        observation.bounds_min,
        observation.bounds_max,
        contract.axes.forward,
    );
    if max_forward <= 0.0 {
        findings.push(warn(
            "nose_axis",
            format!(
                "no geometry extends past the origin along {}; this does not establish the up axis",
                contract.axes.forward.as_str()
            ),
        ));
    }
}

fn horizontal_center_message(
    contract: &DeliveryContract,
    observation: &DeliveryObservation,
) -> Option<String> {
    let spans = [
        observation.bounds_max[0] - observation.bounds_min[0],
        observation.bounds_max[1] - observation.bounds_min[1],
        observation.bounds_max[2] - observation.bounds_min[2],
    ];
    let extent = spans.into_iter().fold(0.0_f64, f64::max);
    if extent <= 0.0 {
        return None;
    }
    let limit = contract.origin.center_fraction * extent;
    let up = contract.axes.up.index();
    let names = ["x", "y", "z"];
    let mut off = Vec::new();
    for (index, name) in names.iter().enumerate() {
        if index == up {
            continue;
        }
        let center = (observation.bounds_min[index] + observation.bounds_max[index]) * 0.5;
        if center.abs() > limit {
            off.push(format!("{name}={center:.3}"));
        }
    }
    if off.is_empty() {
        None
    } else {
        Some(format!(
            "horizontal center is {} ; each must lie within {limit:.3} of the origin",
            off.join(", ")
        ))
    }
}

fn push_lod_findings(
    lod: &LodSettings,
    class_name: &str,
    counts: &BTreeMap<String, u64>,
    findings: &mut Vec<Finding>,
) {
    for name in &lod.required {
        if !counts.contains_key(name) {
            findings.push(error(
                "lod_presence",
                format!("class {class_name} requires LOD {name}"),
            ));
        }
    }
    let required: BTreeSet<&str> = lod.required.iter().map(String::as_str).collect();
    let mut allowed = required.clone();
    allowed.extend(lod.optional.iter().map(String::as_str));
    let present: BTreeSet<&str> = counts.keys().map(String::as_str).collect();
    if !required.is_subset(&present) || !present.is_subset(&allowed) {
        let have = present.iter().copied().collect::<Vec<_>>().join(", ");
        let have = if have.is_empty() { "none".to_string() } else { have };
        findings.push(error(
            "lod_count",
            format!("class {class_name} has LODs [{have}] outside the required and optional sets"),
        ));
    }
    let Some(budget) = counts.get(&lod.budget_lod).copied() else {
        return;
    };
    if budget < lod.lod0_min_triangles || budget > lod.lod0_max_triangles {
        findings.push(error(
            "lod0_budget",
            format!(
                "{} has {budget} triangles; allowed {}-{}",
                lod.budget_lod, lod.lod0_min_triangles, lod.lod0_max_triangles
            ),
        ));
    }
    if lod.require_decreasing {
        let mut previous: Option<(&str, u64)> = None;
        for name in &lod.chain {
            let Some(count) = counts.get(name).copied() else {
                continue;
            };
            if let Some((prev_name, prev_count)) = previous
                && count >= prev_count
            {
                findings.push(error(
                    "lod_chain",
                    format!("{name} has {count} triangles, not fewer than {prev_name} ({prev_count})"),
                ));
            }
            previous = Some((name.as_str(), count));
        }
    }
    for ratio in &lod.ratios {
        let (Some(count), Some(denom)) = (counts.get(&ratio.name).copied(), counts.get(&ratio.of).copied())
        else {
            continue;
        };
        if denom == 0 {
            continue;
        }
        let (Some(count_f), Some(denom_f)) = (exact_unit_f64(count), exact_unit_f64(denom)) else {
            findings.push(warn(
                "lod_ratio",
                format!(
                    "{} or {} is too large for an exact ratio; the ratio check does not run",
                    ratio.name, ratio.of
                ),
            ));
            continue;
        };
        let actual = count_f / denom_f;
        let low = ratio.nominal / 2.0;
        let high = ratio.nominal * 2.0;
        if actual > high || actual < low {
            findings.push(warn(
                "lod_ratio",
                format!(
                    "{} is {:.1}% of {} (nominal {:.1}%)",
                    ratio.name,
                    actual * 100.0,
                    ratio.of,
                    ratio.nominal * 100.0
                ),
            ));
        }
    }
    for cap in &lod.hard_caps {
        let Some(count) = counts.get(&cap.name).copied() else {
            continue;
        };
        if count >= cap.max_triangles_exclusive {
            findings.push(error(
                "lod_cap",
                format!(
                    "{} has {count} triangles; the cap is below {}",
                    cap.name, cap.max_triangles_exclusive
                ),
            ));
        }
    }
}

fn project_min_max(bounds_min: [f64; 3], bounds_max: [f64; 3], axis: SignedAxis) -> (f64, f64) {
    let index = axis.index();
    let low = bounds_min[index];
    let high = bounds_max[index];
    if axis.positive() {
        (low, high)
    } else {
        (-high, -low)
    }
}

fn validate_observation(observation: &DeliveryObservation) -> Result<(), CheckError> {
    for (index, scale) in observation.node_scales.iter().enumerate() {
        for (axis, value) in scale.iter().enumerate() {
            require_finite(*value, &format!("node_scales[{index}][{axis}]"))?;
        }
    }
    for (axis, value) in observation.bounds_min.iter().enumerate() {
        require_finite(*value, &format!("bounds_min[{axis}]"))?;
    }
    for (axis, value) in observation.bounds_max.iter().enumerate() {
        require_finite(*value, &format!("bounds_max[{axis}]"))?;
    }
    if observation.bounds_present {
        for (index, (low, high)) in observation
            .bounds_min
            .iter()
            .zip(observation.bounds_max)
            .enumerate()
        {
            if high < *low {
                return Err(CheckError::InvalidObservation(format!(
                    "bounds_max[{index}] is below bounds_min[{index}]"
                )));
            }
        }
    }
    if let Some(radius) = observation.footprint_radius_m {
        require_finite(radius, "footprint_radius_m")?;
    }
    for (index, socket) in observation.sockets.iter().enumerate() {
        if let Some(width) = socket.width_m {
            require_finite(width, &format!("sockets[{index}].width_m"))?;
        }
        if let Some(height) = socket.height_m {
            require_finite(height, &format!("sockets[{index}].height_m"))?;
        }
    }
    for (index, skin) in observation.skins.iter().enumerate() {
        if let Some(sum) = skin.weight_sum {
            require_finite(sum, &format!("skins[{index}].weight_sum"))?;
        }
    }
    Ok(())
}

fn validate_kit(contract: &DeliveryContract) -> Result<(), String> {
    if contract.max_joint_influences == 0 || contract.max_joint_influences > 8 {
        return Err("max_joint_influences must be 1 to 8".into());
    }
    if contract.texture_max_edge == 0 {
        return Err("texture_max_edge must be at least 1".into());
    }
    for requirement in contract
        .required_nodes
        .iter()
        .chain(&contract.required_sockets)
        .chain(&contract.required_clips)
    {
        validate_pattern(&requirement.pattern)?;
        if requirement.count == 0 {
            return Err(format!("`{}` count must be at least 1", requirement.pattern));
        }
    }
    if let Some(footprint) = &contract.footprint {
        finite_non_negative(footprint.radius_m, "footprint.radius_m")?;
        finite_non_negative(footprint.tolerance, "footprint.tolerance")?;
    }
    for rule in &contract.socket_clearance {
        validate_pattern(&rule.pattern)?;
        finite_non_negative(rule.min_width_m, "socket_clearance.min_width_m")?;
        finite_non_negative(rule.min_height_m, "socket_clearance.min_height_m")?;
    }
    Ok(())
}

fn validate_pattern(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() || pattern.len() > 64 {
        return Err("a name pattern must be 1 to 64 bytes".into());
    }
    if !pattern
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'*')
    {
        return Err(format!(
            "name pattern `{pattern}` must be ASCII letters, digits, '_' , '-' or '*'"
        ));
    }
    Ok(())
}

/// Integers up to 2^53 convert to `f64` without rounding.
fn exact_unit_f64(count: u64) -> Option<f64> {
    const EXACT: u64 = 1_u64 << 53;
    if count > EXACT { None } else { Some(count as f64) }
}

fn require_finite(value: f64, field: &str) -> Result<(), CheckError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(CheckError::NonFinite {
            field: field.to_string(),
        })
    }
}

fn error(code: &'static str, message: impl Into<String>) -> Finding {
    row(code, FindingSeverity::Error, message, "", "", "", "")
}

fn warn(code: &'static str, message: impl Into<String>) -> Finding {
    row(code, FindingSeverity::Warning, message, "", "", "", "")
}

fn row(
    code: &'static str,
    severity: FindingSeverity,
    message: impl Into<String>,
    subject_kind: &'static str,
    subject: impl Into<String>,
    expected: impl Into<String>,
    found: impl Into<String>,
) -> Finding {
    Finding {
        code,
        severity,
        message: message.into(),
        asset: String::new(),
        subject_kind,
        subject: subject.into(),
        expected: expected.into(),
        found: found.into(),
    }
}

/// The clear opening, in metres, that passes a body of `radius_m` with `clearance_m` of spare room.
///
/// The side is the diameter plus the clearance. Callers store that side on a [`SocketClearance`].
#[must_use]
pub fn socket_opening_for_radius(radius_m: f64, clearance_m: f64) -> f64 {
    2.0 * radius_m + clearance_m
}

/// Suggested footprint radius: the measured value rounded up to the next tenth of a metre.
#[must_use]
pub fn suggested_footprint_radius(measured_m: f64) -> f64 {
    (measured_m * 10.0).ceil() / 10.0
}

/// `pattern` matches `name`. `*` matches any run, including empty. No other metacharacter.
#[must_use]
pub fn name_matches(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    if !parts[0].is_empty() {
        let Some(stripped) = rest.strip_prefix(parts[0]) else {
            return false;
        };
        rest = stripped;
    }
    for (index, part) in parts.iter().enumerate().skip(1) {
        if part.is_empty() {
            if index + 1 == parts.len() {
                return true;
            }
            continue;
        }
        if index + 1 == parts.len() {
            return rest.ends_with(part);
        }
        let Some(found) = rest.find(part) else {
            return false;
        };
        rest = &rest[found + part.len()..];
    }
    rest.is_empty()
}

fn count_matching(pattern: &str, names: &[String]) -> usize {
    names.iter().filter(|name| name_matches(pattern, name)).count()
}

fn push_kit_findings(
    contract: &DeliveryContract,
    observation: &DeliveryObservation,
    findings: &mut Vec<Finding>,
) {
    push_skin_findings(contract, observation, findings);
    push_name_findings(
        "missing_node",
        "node",
        &contract.required_nodes,
        |pattern| count_matching(pattern, &observation.nodes),
        findings,
    );
    push_name_findings(
        "missing_socket",
        "socket",
        &contract.required_sockets,
        |pattern| {
            observation
                .sockets
                .iter()
                .filter(|socket| name_matches(pattern, &socket.name))
                .count()
        },
        findings,
    );
    push_name_findings(
        "missing_clip",
        "clip",
        &contract.required_clips,
        |pattern| count_matching(pattern, &observation.clips),
        findings,
    );
    push_material_findings(contract, observation, findings);
    push_texture_findings(contract, observation, findings);
    push_footprint_finding(contract, observation, findings);
    push_clearance_findings(contract, observation, findings);
}

fn push_name_findings(
    code: &'static str,
    kind: &'static str,
    required: &[NameCount],
    mut count: impl FnMut(&str) -> usize,
    findings: &mut Vec<Finding>,
) {
    for requirement in required {
        let found = count(&requirement.pattern);
        if found < requirement.count as usize {
            findings.push(row(
                code,
                FindingSeverity::Error,
                format!(
                    "missing {kind} `{}`: expected {}, found {found}",
                    requirement.pattern, requirement.count
                ),
                kind,
                requirement.pattern.clone(),
                requirement.count.to_string(),
                found.to_string(),
            ));
        }
    }
}

fn push_skin_findings(
    contract: &DeliveryContract,
    observation: &DeliveryObservation,
    findings: &mut Vec<Finding>,
) {
    let cap = contract.max_joint_influences;
    for skin in &observation.skins {
        let where_ = format!("mesh {} primitive {}", skin.mesh, skin.primitive);
        if skin.unpaired {
            findings.push(row(
                "joint_influences",
                FindingSeverity::Error,
                format!("{where_} has only one of joints and weights"),
                "skin",
                skin.mesh.clone(),
                "joints and weights",
                "one of the two",
            ));
            continue;
        }
        let slots = if skin.second_set {
            skin.influences.max(5)
        } else {
            skin.influences
        };
        if slots > cap {
            findings.push(row(
                "joint_influences",
                FindingSeverity::Error,
                format!("{where_} has {slots} influences, the contract allows {cap}"),
                "skin",
                skin.mesh.clone(),
                cap.to_string(),
                slots.to_string(),
            ));
        }
        if skin.weight_out_of_range {
            findings.push(row(
                "joint_weights",
                FindingSeverity::Error,
                format!("{where_}: a weight is outside 0 to 1"),
                "skin",
                skin.mesh.clone(),
                "0 to 1",
                "outside",
            ));
        }
        if let Some(sum) = skin.weight_sum {
            findings.push(row(
                "joint_weights",
                FindingSeverity::Error,
                format!("{where_}: weights sum to {sum:.3}, must be renormalized to 1"),
                "skin",
                skin.mesh.clone(),
                "1",
                format!("{sum:.3}"),
            ));
        }
        if skin.joint_out_of_range {
            findings.push(row(
                "joint_index",
                FindingSeverity::Error,
                format!("{where_}: a joint index is past the skin's joint list"),
                "skin",
                skin.mesh.clone(),
                "inside the joint list",
                "out of range",
            ));
        }
    }
}

fn push_material_findings(
    contract: &DeliveryContract,
    observation: &DeliveryObservation,
    findings: &mut Vec<Finding>,
) {
    let count = observation.material_count;
    let names = if observation.material_names.is_empty() {
        String::new()
    } else {
        format!(" ({})", observation.material_names.join(", "))
    };
    if count > contract.max_materials {
        findings.push(row(
            "material_count",
            FindingSeverity::Error,
            format!(
                "has {count} materials{names}, the cap is {}",
                contract.max_materials
            ),
            "material",
            String::new(),
            contract.max_materials.to_string(),
            count.to_string(),
        ));
        return;
    }
    if contract.material_warn_above > 0 && count > contract.material_warn_above {
        findings.push(row(
            "material_count",
            FindingSeverity::Warning,
            format!(
                "has {count} materials{names}, the nominal count is {}",
                contract.material_warn_above
            ),
            "material",
            String::new(),
            contract.material_warn_above.to_string(),
            count.to_string(),
        ));
    }
}

fn push_texture_findings(
    contract: &DeliveryContract,
    observation: &DeliveryObservation,
    findings: &mut Vec<Finding>,
) {
    let cap = contract.texture_max_edge;
    for texture in &observation.textures {
        let Some((width, height)) = texture.width.zip(texture.height) else {
            findings.push(row(
                "texture_unreadable",
                FindingSeverity::Warning,
                format!(
                    "{}: size unreadable (need a PNG or JPEG to enforce the {cap}px cap)",
                    texture.name
                ),
                "texture",
                texture.name.clone(),
                format!("{cap}"),
                "unreadable",
            ));
            continue;
        };
        let edge = width.max(height);
        if edge > cap {
            findings.push(row(
                "texture_edge",
                FindingSeverity::Error,
                format!("{} is {width}x{height}, the cap is {cap}px", texture.name),
                "texture",
                texture.name.clone(),
                cap.to_string(),
                edge.to_string(),
            ));
        }
    }
}

fn push_footprint_finding(
    contract: &DeliveryContract,
    observation: &DeliveryObservation,
    findings: &mut Vec<Finding>,
) {
    let Some(rule) = &contract.footprint else {
        return;
    };
    let Some(measured) = observation.footprint_radius_m else {
        return;
    };
    if measured <= 0.0 {
        if rule.radius_m != 0.0 {
            findings.push(row(
                "footprint_radius",
                FindingSeverity::Error,
                format!(
                    "empty footprint; the contract radius is {:.3} m and does not match",
                    rule.radius_m
                ),
                "footprint",
                String::new(),
                format!("{:.3}", rule.radius_m),
                "0",
            ));
        }
        return;
    }
    let drift = (rule.radius_m - measured).abs() / measured;
    if drift > rule.tolerance {
        let suggested = suggested_footprint_radius(measured);
        findings.push(row(
            "footprint_radius",
            FindingSeverity::Error,
            format!(
                "contract radius {:.3} m vs measured {measured:.3} m (drift {:.1}%); suggested radius {suggested:.1} m",
                rule.radius_m,
                drift * 100.0
            ),
            "footprint",
            String::new(),
            format!("{:.3}", rule.radius_m),
            format!("{measured:.3}"),
        ));
    }
}

fn push_clearance_findings(
    contract: &DeliveryContract,
    observation: &DeliveryObservation,
    findings: &mut Vec<Finding>,
) {
    for rule in &contract.socket_clearance {
        let mut matched = observation
            .sockets
            .iter()
            .filter(|socket| name_matches(&rule.pattern, &socket.name))
            .collect::<Vec<_>>();
        matched.sort_by(|a, b| a.name.cmp(&b.name));
        for socket in matched {
            let (Some(width), Some(height)) = (socket.width_m, socket.height_m) else {
                let source = if socket.source.is_empty() {
                    "unknown-size"
                } else {
                    socket.source.as_str()
                };
                findings.push(row(
                    "socket_opening",
                    FindingSeverity::Error,
                    format!("{}: unknown opening size ({source}); fails closed", socket.name),
                    "socket",
                    socket.name.clone(),
                    format!("{:.1} x {:.1} m", rule.min_width_m, rule.min_height_m),
                    "unknown",
                ));
                continue;
            };
            if width < rule.min_width_m || height < rule.min_height_m {
                let source = if socket.source.is_empty() {
                    "measured"
                } else {
                    socket.source.as_str()
                };
                findings.push(row(
                    "socket_opening",
                    FindingSeverity::Error,
                    format!(
                        "{}: {width:.2} x {height:.2} m ({source}) vs min {:.1} x {:.1} m",
                        socket.name, rule.min_width_m, rule.min_height_m
                    ),
                    "socket",
                    socket.name.clone(),
                    format!("{:.1} x {:.1}", rule.min_width_m, rule.min_height_m),
                    format!("{width:.2} x {height:.2}"),
                ));
            }
        }
    }
}

/// One failure row of a vendor report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VendorRow {
    /// Asset file or label.
    pub asset: String,
    /// Finding code.
    pub code: String,
    /// `node`, `socket`, `clip`, `material`, `texture`, `skin`, `footprint`, or empty.
    pub subject_kind: String,
    /// The name the contract or the asset used.
    pub subject: String,
    /// What the contract asked for.
    pub expected: String,
    /// What was found.
    pub found: String,
    /// The finding's sentence.
    pub message: String,
}

/// Error findings as vendor rows. Warnings are not failures.
#[must_use]
pub fn vendor_rows(asset: &str, report: &DeliveryReport) -> Vec<VendorRow> {
    report
        .errors()
        .map(|finding| VendorRow {
            asset: if finding.asset.is_empty() {
                asset.to_string()
            } else {
                finding.asset.clone()
            },
            code: finding.code.to_string(),
            subject_kind: finding.subject_kind.to_string(),
            subject: finding.subject.clone(),
            expected: finding.expected.clone(),
            found: finding.found.clone(),
            message: finding.message.clone(),
        })
        .collect()
}

/// Markdown table, one row per failure.
#[must_use]
pub fn vendor_report_markdown(rows: &[VendorRow]) -> String {
    let mut lines = vec![
        "# Delivery contract".to_string(),
        String::new(),
        "| Asset | Code | Subject | Expected | Found |".into(),
        "|---|---|---|---|---|".into(),
    ];
    for row in rows {
        let subject = if row.subject.is_empty() {
            row.subject_kind.clone()
        } else if row.subject_kind.is_empty() {
            row.subject.clone()
        } else {
            format!("{} `{}`", row.subject_kind, row.subject)
        };
        lines.push(format!(
            "| {} | {} | {} | {} | {} |",
            md_cell(&row.asset),
            md_cell(&row.code),
            md_cell(&subject),
            md_cell(&row.expected),
            md_cell(&row.found)
        ));
    }
    if rows.is_empty() {
        lines.push("| | | | | |".into());
    }
    lines.push(String::new());
    lines.join("\n")
}

fn md_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// JSON array of failure rows. Keys are in a fixed order.
#[must_use]
pub fn vendor_report_json(rows: &[VendorRow]) -> String {
    let mut out = String::from("[\n");
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        out.push_str("  {\n");
        push_json_field(&mut out, "asset", &row.asset, true);
        push_json_field(&mut out, "code", &row.code, true);
        push_json_field(&mut out, "subject_kind", &row.subject_kind, true);
        push_json_field(&mut out, "subject", &row.subject, true);
        push_json_field(&mut out, "expected", &row.expected, true);
        push_json_field(&mut out, "found", &row.found, true);
        push_json_field(&mut out, "message", &row.message, false);
        out.push_str("\n  }");
    }
    if !rows.is_empty() {
        out.push('\n');
    }
    out.push_str("]\n");
    out
}

fn push_json_field(out: &mut String, key: &str, value: &str, comma: bool) {
    out.push_str("    \"");
    out.push_str(key);
    out.push_str("\": ");
    out.push_str(&json_string(value));
    if comma {
        out.push(',');
    }
}

fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(ch))),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Writes `report.md` and `report.json` into `dir`, one row per failure.
///
/// # Errors
///
/// The directory cannot be created or a file cannot be replaced.
pub fn write_vendor_report(
    dir: &std::path::Path,
    asset: &str,
    report: &DeliveryReport,
) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(dir)?;
    let rows = vendor_rows(asset, report);
    write_atomic(&dir.join("report.md"), vendor_report_markdown(&rows).as_bytes())?;
    write_atomic(&dir.join("report.json"), vendor_report_json(&rows).as_bytes())?;
    Ok(())
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn finite_positive(value: f64, name: &str) -> Result<(), String> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(format!("{name} must be finite and greater than 0"))
    }
}

fn finite_non_negative(value: f64, name: &str) -> Result<(), String> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(format!("{name} must be finite and at least 0"))
    }
}

fn validate_class_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err("class_name must be 1 to 64 bytes".into());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("class_name must be ASCII letters, digits, '_' or '-'".into());
    }
    Ok(())
}

fn valid_lod_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn validate_lod(lod: &LodSettings) -> Result<(), String> {
    if lod.lod0_min_triangles > lod.lod0_max_triangles {
        return Err("lod0_min_triangles is greater than lod0_max_triangles".into());
    }
    let mut known = BTreeSet::new();
    for name in lod.required.iter().chain(lod.optional.iter()) {
        if !valid_lod_name(name) {
            return Err(format!("LOD name `{name}` is empty or not a short token"));
        }
        if !known.insert(name.as_str()) {
            return Err(format!("LOD name `{name}` is listed twice"));
        }
    }
    if known.is_empty() {
        return Err("the contract names no LOD".into());
    }
    if !known.contains(lod.budget_lod.as_str()) {
        return Err(format!(
            "budget_lod `{}` is not a required or optional LOD",
            lod.budget_lod
        ));
    }
    for name in &lod.chain {
        if !known.contains(name.as_str()) {
            return Err(format!("chain LOD `{name}` is not a required or optional LOD"));
        }
    }
    for ratio in &lod.ratios {
        if !ratio.nominal.is_finite() || ratio.nominal <= 0.0 {
            return Err(format!(
                "ratio for `{}` must be finite and greater than 0",
                ratio.name
            ));
        }
        if !known.contains(ratio.name.as_str()) || !known.contains(ratio.of.as_str()) {
            return Err(format!(
                "ratio `{}` of `{}` names a LOD the contract does not list",
                ratio.name, ratio.of
            ));
        }
    }
    for cap in &lod.hard_caps {
        if !known.contains(cap.name.as_str()) {
            return Err(format!(
                "cap `{}` names a LOD the contract does not list",
                cap.name
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_fingerprint_hash() {
        let fp = delivery_contract_schema_fingerprint();
        let expected = "format_version,class_name,scale,axes,origin,lod,max_joint_influences,required_nodes,required_sockets,required_clips,max_materials,material_warn_above,texture_max_edge,footprint,socket_clearance;meters_per_unit,identity_tolerance;up,forward;ground_offset_m,ground_tolerance_m,coarse_floor_rise_m,center_fraction;required,optional,budget_lod,lod0_min_triangles,lod0_max_triangles,chain,ratios,hard_caps,require_decreasing;name,of,nominal;name,max_triangles_exclusive;pos_x,neg_x,pos_y,neg_y,pos_z,neg_z;pattern,count;radius_m,tolerance;pattern,min_width_m,min_height_m";
        assert_eq!(fp, expected);
        let hash = blake3::hash(fp.as_bytes()).to_hex().to_string();
        assert_eq!(hash.len(), 64, "{fp}");
        eprintln!("delivery-contract hash: {hash}");
    }
}
