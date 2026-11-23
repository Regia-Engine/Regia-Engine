// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/utility.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (the body of `score`, via `regia_detsim::ai::utility::score`, plan row S-09). `score_idx` repeats it
// with one named change: the input is read from a slice by `InputIdx` instead of from an `Inputs` field. The
// catalogue, its file format, its checks and the index plumbing are original (gap AI-7, ADR-8609).

//! A game's own utility catalogue as data (audit gap AI-7).
//!
//! [`UtilityCatalogue`] holds the options a unit scores ([`OptionDef`]) and the named inputs their
//! considerations read ([`InputDef`]). Options and inputs are addressed by index ([`OptionIdx`],
//! [`InputIdx`]); a unit's inputs are a plain `&[f32]` slice indexed by [`InputIdx`] ([`InputValues`]).
//! [`score_idx`] is [`super::score`] with the struct-field lookup replaced by a slice index: the same
//! float operations in the same order, so a catalogue that holds the same curves scores the same bits.
//!
//! DO27's twelve options and fifteen inputs are [`UtilityCatalogue::do27_preset`]: option `i` of the
//! preset is the [`OptionId`] with `bit() == i`, input `i` is the [`ConsiderationId`] with
//! `index() == i`, so `OptionId`, `ConsiderationId` and `Inputs` stay as typed names over the preset.
//!
//! The file form is `.detutility.ron` (format `ai-det-utility`, version [`DET_UTILITY_FORMAT_VERSION`]):
//! [`UtilityCatalogue::from_ron_str`] parses and validates it with no asset system, so a headless sim
//! loads it directly. **Order is sim state**: an option's index is its position in the file and decides
//! ties ([`super::ArgMax`] keeps the last of equal best scores), so reordering options or inputs changes
//! the sim even when no curve changes.

use serde::{Deserialize, Serialize};

use super::difficulty::Thresholds;
use super::tactics;
use super::utility::{AiOption, ConsiderationId, CurveKind, Inputs, OptionId, ResponseCurve};
use crate::float::{det_clamp, det_max};

/// The format version of `.detutility.ron` this build writes and reads.
pub const DET_UTILITY_FORMAT_VERSION: u32 = 1;

/// The format's registry name (`docs/formats/ai-det-utility.md`, `format-hashes.ron`).
pub const DET_UTILITY_FORMAT: &str = "ai-det-utility";

/// The file suffix of a utility catalogue.
pub const DET_UTILITY_SUFFIX: &str = ".detutility.ron";

/// The type path of a cooked catalogue in a pak: the payload is the canonical text
/// ([`UtilityCatalogue::to_ron`]), so a game reads it back with [`UtilityCatalogue::from_ron_str`].
pub const DET_UTILITY_TYPE_PATH: &str = "regia_detsim::ai::UtilityCatalogue";

/// The most options a catalogue holds: an [`OptionMask`] is one `u64`.
pub const MAX_CATALOGUE_OPTIONS: usize = 64;

/// The most inputs a catalogue names.
pub const MAX_CATALOGUE_INPUTS: usize = 256;

/// The most considerations one option holds.
pub const MAX_OPTION_CONSIDERATIONS: usize = 32;

/// The longest option or input name, in bytes.
pub const MAX_CATALOGUE_NAME_BYTES: usize = 64;

/// The largest catalogue text [`UtilityCatalogue::from_ron_str`] reads, in bytes.
pub const MAX_CATALOGUE_TEXT_BYTES: usize = 1 << 20;

/// An option's index in a [`UtilityCatalogue`]: its position in the file, and its bit in an
/// [`OptionMask`]. For the DO27 preset it equals [`OptionId::bit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OptionIdx(pub u16);

impl OptionIdx {
    /// The index as a `usize`.
    pub fn index(self) -> usize {
        usize::from(self.0)
    }
}

/// An input's index in a [`UtilityCatalogue`]: its position in the file and in a unit's
/// [`InputValues`]. For the DO27 preset it equals [`ConsiderationId::index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InputIdx(pub u16);

impl InputIdx {
    /// The index as a `usize`.
    pub fn index(self) -> usize {
        usize::from(self.0)
    }
}

/// A unit's normalized inputs, indexed by [`InputIdx`]. An index past the end reads 0 (the value an
/// unset field of [`Inputs`] has).
pub type InputValues = [f32];

impl OptionId {
    /// The option's name in the DO27 preset (`snake_case`).
    pub fn name(self) -> &'static str {
        match self {
            OptionId::Push => "push",
            OptionId::Hold => "hold",
            OptionId::Strafe => "strafe",
            OptionId::Dogfight => "dogfight",
            OptionId::Kite => "kite",
            OptionId::Retreat => "retreat",
            OptionId::Regroup => "regroup",
            OptionId::Defend => "defend",
            OptionId::CollectScrap => "collect_scrap",
            OptionId::Escort => "escort",
            OptionId::Flank => "flank",
            OptionId::FocusFire => "focus_fire",
        }
    }

    /// The option's index in the DO27 preset (its bit).
    pub fn idx(self) -> OptionIdx {
        OptionIdx(self as u16)
    }

    /// The preset option at `idx`, `None` past the twelfth.
    pub fn from_idx(idx: OptionIdx) -> Option<OptionId> {
        OptionId::ALL.get(idx.index()).copied()
    }
}

impl ConsiderationId {
    /// Every input, in index order.
    pub const ALL: [ConsiderationId; 15] = [
        ConsiderationId::HullPct,
        ConsiderationId::AmmoPct,
        ConsiderationId::RangeBandFrac,
        ConsiderationId::LocalForceRatio,
        ConsiderationId::CoverAvailability,
        ConsiderationId::DistanceToLeaderFrac,
        ConsiderationId::DistanceToThreatFrac,
        ConsiderationId::ThreatIncoming,
        ConsiderationId::CommanderPriority,
        ConsiderationId::ScrapProximity,
        ConsiderationId::PointDistanceFrac,
        ConsiderationId::LeaderTargetFocus,
        ConsiderationId::RelativeEnergyFrac,
        ConsiderationId::TurnRateFrac,
        ConsiderationId::HopperFrac,
    ];

    /// The input's index in the DO27 preset.
    pub fn index(self) -> InputIdx {
        InputIdx(self as u16)
    }

    /// The input's name in the DO27 preset (`snake_case`, the [`Inputs`] field name).
    pub fn name(self) -> &'static str {
        match self {
            ConsiderationId::HullPct => "hull_pct",
            ConsiderationId::AmmoPct => "ammo_pct",
            ConsiderationId::RangeBandFrac => "range_band_frac",
            ConsiderationId::LocalForceRatio => "local_force_ratio",
            ConsiderationId::CoverAvailability => "cover_availability",
            ConsiderationId::DistanceToLeaderFrac => "distance_to_leader_frac",
            ConsiderationId::DistanceToThreatFrac => "distance_to_threat_frac",
            ConsiderationId::ThreatIncoming => "threat_incoming",
            ConsiderationId::CommanderPriority => "commander_priority",
            ConsiderationId::ScrapProximity => "scrap_proximity",
            ConsiderationId::PointDistanceFrac => "point_distance_frac",
            ConsiderationId::LeaderTargetFocus => "leader_target_focus",
            ConsiderationId::RelativeEnergyFrac => "relative_energy_frac",
            ConsiderationId::TurnRateFrac => "turn_rate_frac",
            ConsiderationId::HopperFrac => "hopper_frac",
        }
    }
}

impl Inputs {
    /// The inputs as [`InputValues`] of the DO27 preset (index [`ConsiderationId::index`]).
    pub fn values(&self) -> [f32; 15] {
        ConsiderationId::ALL.map(|id| self.get(id))
    }
}

/// A set of options by [`OptionIdx`]: bit `i` holds option `i`. One `u64`, so a catalogue holds at most
/// [`MAX_CATALOGUE_OPTIONS`] options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OptionMask(pub u64);

impl OptionMask {
    /// No option.
    pub fn empty() -> Self {
        OptionMask(0)
    }

    /// Every option.
    pub fn all() -> Self {
        OptionMask(u64::MAX)
    }

    /// This mask with `idx` added (an index of 64 or more is ignored).
    pub fn with(mut self, idx: OptionIdx) -> Self {
        if let Some(bit) = 1u64.checked_shl(u32::from(idx.0)) {
            self.0 |= bit;
        }
        self
    }

    /// Whether the mask holds `idx` (never for an index of 64 or more).
    pub fn contains(self, idx: OptionIdx) -> bool {
        1u64.checked_shl(u32::from(idx.0))
            .is_some_and(|bit| self.0 & bit != 0)
    }
}

impl From<super::RepertoireMask> for OptionMask {
    /// The preset's repertoire as a mask over its [`OptionIdx`] (the same bits).
    fn from(mask: super::RepertoireMask) -> Self {
        OptionMask(u64::from(mask.0))
    }
}

/// A named input a catalogue's considerations read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputDef {
    /// The input's name, unique in the catalogue.
    pub name: String,
}

/// One scored input of an [`OptionDef`]: [`super::Consideration`] by [`InputIdx`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdxConsideration {
    /// The input read.
    pub input: InputIdx,
    /// The curve the input goes through.
    pub curve: ResponseCurve,
    /// The weight of an additive consideration (1 for a multiplier).
    pub weight: f32,
    /// Multiply in (product rule) instead of adding.
    pub multiplier: bool,
}

/// One option of a [`UtilityCatalogue`].
#[derive(Debug, Clone, PartialEq)]
pub struct OptionDef {
    /// The option's name, unique in the catalogue.
    pub name: String,
    /// Its considerations, scored in order.
    pub considerations: Vec<IdxConsideration>,
    /// The option's floor, carried as data like [`AiOption::min_score`]: [`score_idx`] and
    /// [`super::score_options_idx`] do not apply it (nor do [`super::score`] and
    /// [`super::score_options`]); a [`super::DecisionPolicy`] may.
    pub min_score: f32,
}

/// [`super::score`] over a catalogue option: `Σ w_i · curve_i(input_i)` normalized by `Σ |w_i|`, times
/// the product of the multiplier considerations, clamped to 0..1. Same operations in the same order as
/// [`super::score`], so the same curves on the same input bits give the same score bits.
pub fn score_idx(option: &OptionDef, inputs: &InputValues) -> f32 {
    let mut add = 0.0;
    let mut add_w = 0.0;
    let mut mult = 1.0;
    for c in &option.considerations {
        let x = inputs.get(c.input.index()).copied().unwrap_or(0.0);
        let v = c.curve.evaluate(x);
        if c.multiplier {
            mult *= det_max(v, 0.0);
        } else {
            add += c.weight * v;
            add_w += c.weight.abs();
        }
    }
    let norm = if add_w > 0.0 { add / add_w } else { 0.0 };
    det_clamp(norm * mult, 0.0, 1.0)
}

/// Why a catalogue was refused. [`Self::code`] is the stable agent-protocol error code.
#[derive(Debug, Clone, PartialEq)]
pub enum DetUtilityError {
    /// The text is not a catalogue (RON syntax, a missing or unknown field, a wrong type).
    Parse {
        /// The parser's message.
        message: String,
    },
    /// The text is larger than [`MAX_CATALOGUE_TEXT_BYTES`].
    TextTooLarge {
        /// The text's size in bytes.
        bytes: usize,
    },
    /// The file is a newer format version than this build reads.
    NewerVersion {
        /// The file's `format_version`.
        found: u32,
        /// The version this build reads.
        supported: u32,
    },
    /// The file's `format_version` was never published (0).
    UnsupportedVersion {
        /// The file's `format_version`.
        found: u32,
    },
    /// A consideration names an input the catalogue does not list.
    UnknownInput {
        /// The option holding the consideration.
        option: String,
        /// The input name it reads.
        input: String,
    },
    /// Two options, or two inputs, share a name.
    DuplicateName {
        /// `"option"` or `"input"`.
        kind: &'static str,
        /// The repeated name.
        name: String,
    },
    /// An option or input name is empty, or longer than [`MAX_CATALOGUE_NAME_BYTES`].
    BadName {
        /// `"option"` or `"input"`.
        kind: &'static str,
        /// The name.
        name: String,
    },
    /// A curve parameter, weight or floor is NaN or infinite.
    NonFinite {
        /// The option.
        option: String,
        /// The field: `min_score`, or `considerations[i].weight` / `.curve.m` / `.k` / `.b` / `.c`.
        field: String,
    },
    /// More options, inputs or considerations than the caps allow.
    OverCap {
        /// `"options"`, `"inputs"` or `"considerations"` (of the named option).
        what: String,
        /// The count found.
        count: usize,
        /// The cap.
        max: usize,
    },
}

impl DetUtilityError {
    /// The stable error code (lowercase ascii, digits and `_`).
    pub fn code(&self) -> &'static str {
        match self {
            DetUtilityError::Parse { .. } => "det_utility_parse",
            DetUtilityError::TextTooLarge { .. } => "det_utility_over_cap",
            DetUtilityError::NewerVersion { .. } => "det_utility_newer_version",
            DetUtilityError::UnsupportedVersion { .. } => "det_utility_unsupported_version",
            DetUtilityError::UnknownInput { .. } => "det_utility_unknown_input",
            DetUtilityError::DuplicateName { .. } => "det_utility_duplicate_name",
            DetUtilityError::BadName { .. } => "det_utility_bad_name",
            DetUtilityError::NonFinite { .. } => "det_utility_non_finite",
            DetUtilityError::OverCap { .. } => "det_utility_over_cap",
        }
    }
}

impl std::fmt::Display for DetUtilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetUtilityError::Parse { message } => write!(f, "not a utility catalogue: {message}"),
            DetUtilityError::TextTooLarge { bytes } => {
                write!(
                    f,
                    "catalogue text is {bytes} bytes; the cap is {MAX_CATALOGUE_TEXT_BYTES}"
                )
            }
            DetUtilityError::NewerVersion { found, supported } => write!(
                f,
                "catalogue format_version {found} is newer than this build reads ({supported}); update the engine"
            ),
            DetUtilityError::UnsupportedVersion { found } => {
                write!(f, "catalogue format_version {found} was never published")
            }
            DetUtilityError::UnknownInput { option, input } => {
                write!(
                    f,
                    "option `{option}` reads input `{input}`, which the catalogue does not list"
                )
            }
            DetUtilityError::DuplicateName { kind, name } => write!(f, "two {kind}s are named `{name}`"),
            DetUtilityError::BadName { kind, name } => write!(
                f,
                "{kind} name `{name}` is empty or longer than {MAX_CATALOGUE_NAME_BYTES} bytes"
            ),
            DetUtilityError::NonFinite { option, field } => {
                write!(f, "option `{option}`: {field} is NaN or infinite")
            }
            DetUtilityError::OverCap { what, count, max } => {
                write!(f, "{count} {what}; the cap is {max}")
            }
        }
    }
}

impl std::error::Error for DetUtilityError {}

/// A response curve in a file: [`ResponseCurve`] with the parameters a shape does not read optional
/// (default 0).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurveFile {
    /// The curve shape.
    pub kind: CurveKind,
    /// Slope (linear) or sharpness (logistic).
    #[serde(default)]
    pub m: f32,
    /// Knee: the logistic center or the plateau start.
    #[serde(default)]
    pub k: f32,
    /// Band center.
    #[serde(default)]
    pub b: f32,
    /// Band half-width.
    #[serde(default)]
    pub c: f32,
}

impl From<CurveFile> for ResponseCurve {
    fn from(c: CurveFile) -> Self {
        ResponseCurve {
            kind: c.kind,
            m: c.m,
            k: c.k,
            b: c.b,
            c: c.c,
        }
    }
}

impl From<ResponseCurve> for CurveFile {
    fn from(c: ResponseCurve) -> Self {
        CurveFile {
            kind: c.kind,
            m: c.m,
            k: c.k,
            b: c.b,
            c: c.c,
        }
    }
}

fn one() -> f32 {
    1.0
}

/// One consideration in a file: the input by name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsiderationFile {
    /// The input's name (an entry of the file's `inputs`).
    pub input: String,
    /// The curve.
    pub curve: CurveFile,
    /// The weight (default 1).
    #[serde(default = "one")]
    pub weight: f32,
    /// Multiply in instead of adding (default false).
    #[serde(default)]
    pub multiplier: bool,
}

/// One option in a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionFile {
    /// The option's name.
    pub name: String,
    /// The floor (default 0).
    #[serde(default)]
    pub min_score: f32,
    /// The considerations, scored in order.
    pub considerations: Vec<ConsiderationFile>,
}

/// The `.detutility.ron` file (format `ai-det-utility`, version 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetUtilityFile {
    /// [`DET_UTILITY_FORMAT_VERSION`].
    pub format_version: u32,
    /// The catalogue's name (free text, optional).
    #[serde(default)]
    pub name: String,
    /// The inputs, in index order.
    pub inputs: Vec<InputDef>,
    /// The options, in index order (the tie order).
    pub options: Vec<OptionFile>,
}

/// Reads `format_version` alone, so a newer file is refused as newer before its fields are checked.
#[derive(Deserialize)]
struct VersionHeader {
    #[serde(default)]
    format_version: u32,
}

/// A validated utility catalogue: options and inputs by index, with sorted name tables for lookup.
#[derive(Debug, Clone, PartialEq)]
pub struct UtilityCatalogue {
    name: String,
    options: Vec<OptionDef>,
    inputs: Vec<InputDef>,
    /// `(name, index)` sorted by name.
    option_names: Vec<(String, u16)>,
    /// `(name, index)` sorted by name.
    input_names: Vec<(String, u16)>,
}

fn check_name(kind: &'static str, name: &str) -> Result<(), DetUtilityError> {
    if name.is_empty() || name.len() > MAX_CATALOGUE_NAME_BYTES {
        return Err(DetUtilityError::BadName {
            kind,
            name: name.chars().take(MAX_CATALOGUE_NAME_BYTES).collect(),
        });
    }
    Ok(())
}

/// `(name, index)` sorted by name; a repeated name is an error.
fn name_table(
    kind: &'static str,
    names: impl Iterator<Item = String>,
) -> Result<Vec<(String, u16)>, DetUtilityError> {
    let mut table: Vec<(String, u16)> = Vec::new();
    for (i, name) in names.enumerate() {
        check_name(kind, &name)?;
        // The caps (checked first) keep `i` well under u16::MAX.
        table.push((name, i as u16));
    }
    table.sort();
    if let Some(pair) = table.windows(2).find(|w| w[0].0 == w[1].0) {
        return Err(DetUtilityError::DuplicateName {
            kind,
            name: pair[0].0.clone(),
        });
    }
    Ok(table)
}

fn lookup(table: &[(String, u16)], name: &str) -> Option<u16> {
    table
        .binary_search_by(|(n, _)| n.as_str().cmp(name))
        .ok()
        .map(|i| table[i].1)
}

fn finite(option: &str, field: impl FnOnce() -> String, v: f32) -> Result<(), DetUtilityError> {
    if v.is_finite() {
        Ok(())
    } else {
        Err(DetUtilityError::NonFinite {
            option: option.to_string(),
            field: field(),
        })
    }
}

impl UtilityCatalogue {
    /// Validate a file and build the catalogue. Checks, in order: the version, the caps, the names
    /// (non-empty, at most [`MAX_CATALOGUE_NAME_BYTES`], unique per kind), every consideration's input
    /// name, and every float (finite).
    pub fn from_file(file: &DetUtilityFile) -> Result<UtilityCatalogue, DetUtilityError> {
        check_version(file.format_version)?;
        if file.options.len() > MAX_CATALOGUE_OPTIONS {
            return Err(DetUtilityError::OverCap {
                what: "options".into(),
                count: file.options.len(),
                max: MAX_CATALOGUE_OPTIONS,
            });
        }
        if file.inputs.len() > MAX_CATALOGUE_INPUTS {
            return Err(DetUtilityError::OverCap {
                what: "inputs".into(),
                count: file.inputs.len(),
                max: MAX_CATALOGUE_INPUTS,
            });
        }
        for o in &file.options {
            if o.considerations.len() > MAX_OPTION_CONSIDERATIONS {
                return Err(DetUtilityError::OverCap {
                    what: format!("considerations of option `{}`", o.name),
                    count: o.considerations.len(),
                    max: MAX_OPTION_CONSIDERATIONS,
                });
            }
        }
        let input_names = name_table("input", file.inputs.iter().map(|i| i.name.clone()))?;
        let option_names = name_table("option", file.options.iter().map(|o| o.name.clone()))?;
        let mut options = Vec::with_capacity(file.options.len());
        for o in &file.options {
            finite(&o.name, || "min_score".into(), o.min_score)?;
            let mut considerations = Vec::with_capacity(o.considerations.len());
            for (i, c) in o.considerations.iter().enumerate() {
                let input = lookup(&input_names, &c.input).ok_or_else(|| DetUtilityError::UnknownInput {
                    option: o.name.clone(),
                    input: c.input.clone(),
                })?;
                finite(&o.name, || format!("considerations[{i}].weight"), c.weight)?;
                for (field, v) in [
                    ("m", c.curve.m),
                    ("k", c.curve.k),
                    ("b", c.curve.b),
                    ("c", c.curve.c),
                ] {
                    finite(&o.name, || format!("considerations[{i}].curve.{field}"), v)?;
                }
                considerations.push(IdxConsideration {
                    input: InputIdx(input),
                    curve: c.curve.into(),
                    weight: c.weight,
                    multiplier: c.multiplier,
                });
            }
            options.push(OptionDef {
                name: o.name.clone(),
                considerations,
                min_score: o.min_score,
            });
        }
        Ok(UtilityCatalogue {
            name: file.name.clone(),
            options,
            inputs: file.inputs.clone(),
            option_names,
            input_names,
        })
    }

    /// Parse and validate `.detutility.ron` text. No asset system, no file IO: a headless sim calls this
    /// on text it read itself. A newer `format_version` is refused as
    /// [`DetUtilityError::NewerVersion`] before any other field is read.
    pub fn from_ron_str(text: &str) -> Result<UtilityCatalogue, DetUtilityError> {
        if text.len() > MAX_CATALOGUE_TEXT_BYTES {
            return Err(DetUtilityError::TextTooLarge { bytes: text.len() });
        }
        let header: VersionHeader = ron::from_str(text).map_err(|e| DetUtilityError::Parse {
            message: e.to_string(),
        })?;
        check_version(header.format_version)?;
        let file: DetUtilityFile = ron::from_str(text).map_err(|e| DetUtilityError::Parse {
            message: e.to_string(),
        })?;
        UtilityCatalogue::from_file(&file)
    }

    /// The catalogue as a file (names, not indices).
    pub fn to_file(&self) -> DetUtilityFile {
        DetUtilityFile {
            format_version: DET_UTILITY_FORMAT_VERSION,
            name: self.name.clone(),
            inputs: self.inputs.clone(),
            options: self
                .options
                .iter()
                .map(|o| OptionFile {
                    name: o.name.clone(),
                    min_score: o.min_score,
                    considerations: o
                        .considerations
                        .iter()
                        .map(|c| ConsiderationFile {
                            input: self.inputs[c.input.index()].name.clone(),
                            curve: c.curve.into(),
                            weight: c.weight,
                            multiplier: c.multiplier,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// The catalogue as canonical `.detutility.ron` text (every field written; floats round-trip
    /// bit for bit).
    pub fn to_ron(&self) -> String {
        // Plain structs, strings, floats and bools always serialize. `regia_core::ron_pretty()`'s
        // layout with `\n` line ends on every host (the default writes `\r\n` on Windows, which would
        // make the canonical text differ by host): this crate is on the lockstep closure and depends
        // on no foundation crate, so it builds the same config here (as regia_world_geom does,
        // ADR-8421; the `ron_pretty_lint` test).
        let pretty: ron::ser::PrettyConfig = Default::default();
        ron::ser::to_string_pretty(&self.to_file(), pretty.new_line("\n")).unwrap_or_default()
    }

    /// A catalogue from enum options in slice order, over the fifteen preset inputs. Option `i` is the
    /// slice's option `i`, named [`OptionId::name`]; a repeated id is
    /// [`DetUtilityError::DuplicateName`].
    pub fn from_enum_options(name: &str, options: &[AiOption]) -> Result<UtilityCatalogue, DetUtilityError> {
        let file = DetUtilityFile {
            format_version: DET_UTILITY_FORMAT_VERSION,
            name: name.to_string(),
            inputs: ConsiderationId::ALL
                .iter()
                .map(|id| InputDef {
                    name: id.name().to_string(),
                })
                .collect(),
            options: options
                .iter()
                .map(|o| OptionFile {
                    name: o.id.name().to_string(),
                    min_score: o.min_score,
                    considerations: o
                        .considerations
                        .iter()
                        .map(|c| ConsiderationFile {
                            input: c.input.name().to_string(),
                            curve: c.curve.into(),
                            weight: c.weight,
                            multiplier: c.multiplier,
                        })
                        .collect(),
                })
                .collect(),
        };
        UtilityCatalogue::from_file(&file)
    }

    /// DO27's catalogue ([`super::catalogue()`]) for the thresholds `t`, as data: its twelve options in
    /// [`OptionId::ALL`] order (option index == [`OptionId::bit`]) over its fifteen inputs
    /// ([`ConsiderationId::ALL`] order). Scores equal [`super::score`] bit for bit. The curves depend on
    /// `t`, so each difficulty level is its own preset (`DifficultyProfile::thresholds`).
    pub fn do27_preset(t: &Thresholds) -> UtilityCatalogue {
        // DO27's option and input names are unique and within the caps, so this cannot fail; the
        // fallback is an empty catalogue (scores nothing), never a panic.
        UtilityCatalogue::from_enum_options("do27", &tactics::catalogue(t)).unwrap_or_else(|_| {
            UtilityCatalogue {
                name: "do27".into(),
                options: Vec::new(),
                inputs: Vec::new(),
                option_names: Vec::new(),
                input_names: Vec::new(),
            }
        })
    }

    /// The catalogue's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The options, in index order.
    pub fn options(&self) -> &[OptionDef] {
        &self.options
    }

    /// The inputs, in index order.
    pub fn inputs(&self) -> &[InputDef] {
        &self.inputs
    }

    /// The option at `idx`.
    pub fn option(&self, idx: OptionIdx) -> Option<&OptionDef> {
        self.options.get(idx.index())
    }

    /// The name of the option at `idx`.
    pub fn option_name(&self, idx: OptionIdx) -> Option<&str> {
        self.options.get(idx.index()).map(|o| o.name.as_str())
    }

    /// The name of the input at `idx`.
    pub fn input_name(&self, idx: InputIdx) -> Option<&str> {
        self.inputs.get(idx.index()).map(|i| i.name.as_str())
    }

    /// The option named `name` (binary search of the sorted name table).
    pub fn option_by_name(&self, name: &str) -> Option<OptionIdx> {
        lookup(&self.option_names, name).map(OptionIdx)
    }

    /// The input named `name` (binary search of the sorted name table).
    pub fn input_by_name(&self, name: &str) -> Option<InputIdx> {
        lookup(&self.input_names, name).map(InputIdx)
    }

    /// Every option's bit.
    pub fn all_options(&self) -> OptionMask {
        let n = self.options.len();
        if n >= 64 {
            OptionMask::all()
        } else {
            OptionMask((1u64 << n) - 1)
        }
    }

    /// Score option `idx` on `inputs` ([`score_idx`]); `None` for an unknown index.
    pub fn score(&self, idx: OptionIdx, inputs: &InputValues) -> Option<f32> {
        self.option(idx).map(|o| score_idx(o, inputs))
    }

    /// One option's score with each consideration's input name, raw input and curve value: the rows an
    /// explain view shows. `None` for an unknown index.
    pub fn explain(&self, idx: OptionIdx, inputs: &InputValues) -> Option<OptionExplain> {
        let option = self.option(idx)?;
        let considerations = option
            .considerations
            .iter()
            .map(|c| {
                let raw = inputs.get(c.input.index()).copied().unwrap_or(0.0);
                ConsiderationExplain {
                    input: self.input_name(c.input).unwrap_or_default().to_string(),
                    raw,
                    value: c.curve.evaluate(raw),
                    weight: c.weight,
                    multiplier: c.multiplier,
                }
            })
            .collect();
        Some(OptionExplain {
            option: option.name.clone(),
            score: score_idx(option, inputs),
            considerations,
        })
    }
}

fn check_version(found: u32) -> Result<(), DetUtilityError> {
    match found {
        DET_UTILITY_FORMAT_VERSION => Ok(()),
        0 => Err(DetUtilityError::UnsupportedVersion { found }),
        _ => Err(DetUtilityError::NewerVersion {
            found,
            supported: DET_UTILITY_FORMAT_VERSION,
        }),
    }
}

/// `regia upgrade-project`'s migrator for `.detutility.ron`: version 1 is the first published version,
/// so a v1 file is returned unchanged; a newer one is [`DetUtilityError::NewerVersion`].
pub fn migrate_det_utility_ron(text: &str) -> Result<String, DetUtilityError> {
    let header: VersionHeader = ron::from_str(text).map_err(|e| DetUtilityError::Parse {
        message: e.to_string(),
    })?;
    check_version(header.format_version)?;
    Ok(text.to_string())
}

/// The versions [`migrate_det_utility_ron`] upgrades from: none yet (v1 is the first).
pub fn det_utility_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// One consideration's row of an [`OptionExplain`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsiderationExplain {
    /// The input's name.
    pub input: String,
    /// The raw input value.
    pub raw: f32,
    /// The curve's value for it (0..1).
    pub value: f32,
    /// The weight.
    pub weight: f32,
    /// Whether it multiplies in.
    pub multiplier: bool,
}

/// One option's score with its considerations, by name ([`UtilityCatalogue::explain`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionExplain {
    /// The option's name.
    pub option: String,
    /// Its score ([`score_idx`]).
    pub score: f32,
    /// Its considerations, in order.
    pub considerations: Vec<ConsiderationExplain>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_index_is_the_option_bit_and_the_input_index() {
        let preset = UtilityCatalogue::do27_preset(&super::super::DifficultyProfile::medium().thresholds);
        assert_eq!(preset.options().len(), 12);
        assert_eq!(preset.inputs().len(), 15);
        for id in OptionId::ALL {
            assert_eq!(preset.option_by_name(id.name()), Some(id.idx()));
            assert_eq!(id.idx().0 as u32, id.bit());
            assert_eq!(OptionId::from_idx(id.idx()), Some(id));
        }
        for id in ConsiderationId::ALL {
            assert_eq!(preset.input_by_name(id.name()), Some(id.index()));
        }
    }

    #[test]
    fn option_mask_ignores_indices_past_the_cap() {
        let m = OptionMask::empty()
            .with(OptionIdx(3))
            .with(OptionIdx(64))
            .with(OptionIdx(900));
        assert_eq!(m.0, 1 << 3);
        assert!(m.contains(OptionIdx(3)));
        assert!(!m.contains(OptionIdx(64)));
        assert!(!OptionMask::all().contains(OptionIdx(64)));
    }

    #[test]
    fn a_missing_input_reads_zero() {
        let preset = UtilityCatalogue::do27_preset(&super::super::DifficultyProfile::medium().thresholds);
        let short: [f32; 0] = [];
        let zeros = Inputs::default().values();
        for i in 0..12u16 {
            assert_eq!(
                preset.score(OptionIdx(i), &short).map(f32::to_bits),
                preset.score(OptionIdx(i), &zeros).map(f32::to_bits)
            );
        }
    }
}
