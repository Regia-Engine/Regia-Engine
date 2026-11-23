// SPDX-License-Identifier: MPL-2.0
//! Traversal moves (`docs/specs/traversal.md`, ADR-2971): swimming, ladders, mantles and vaults, and
//! the bridge that lets AI use them.
//!
//! A character gains the moves by having [`TraversalSettings`]. [`step_character`] asks for that
//! component and nothing else, so a character without it runs exactly what it ran before, and neither
//! [`CharacterPlugin`](crate::character::CharacterPlugin) nor any existing entity gains anything.
//!
//! | Piece | What |
//! |---|---|
//! | [`TraversalSettings`] | saved tuning; its presence opts a character in |
//! | [`CharacterTraversal`] | the running move, predicted and replicated with the `net` feature |
//! | [`Ladder`] | a ladder volume at an entity |
//! | [`TraversalJumpPoint`] | an authored jump the nav bridge publishes |
//! | [`TraversalEvent`] | start, finish and refusal, with the montage slot and warp targets for the host |
//! | [`TraversalPlugin`] | reflection, messages and the request system |
//! | [`nav::TraversalNavPlugin`] | ladders and jump points on the navmesh; crowd crossings as [`TraversalRequest`]s |
//!
//! Every move runs inside [`step_character`], so the movement system, the predicted schedule and a
//! re-run after a correction all run the same code. All durations count whole fixed steps.
//!
//! [`step_character`]: crate::character::step_character

use regia_animation::WarpTarget;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_physics::{
    CharacterBody, ColliderShape, PhysicsLayers, PhysicsWorld, QueryFilter, ShapeAxis, WorldQueries,
};
use regia_scene::Transform;
use regia_scene::spawn::ReflectFormatVersion;
use serde::{Deserialize, Serialize};

use crate::character::{
    CharacterInput, CharacterMovement, CharacterState, CollisionQuery, MoveResult, MovementMode,
    drain_root_motion, write_step,
};

mod curve;
mod ladder;
mod ledge;
pub mod nav;
#[cfg(feature = "net")]
pub mod net;
mod swim;

pub use curve::path_point;
pub use ladder::{LadderProbe, probe_ladder};
pub use ledge::{LedgeProbe, probe_ledge};
pub use swim::{FlatWater, TraversalWater, WaterSource, water_surface};

/// `format_version` of the saved [`TraversalSettings`] (`traversal-settings`).
pub const TRAVERSAL_SETTINGS_FORMAT_VERSION: u32 = 1;
/// `format_version` of the saved [`Ladder`] (`traversal-ladder`).
pub const TRAVERSAL_LADDER_FORMAT_VERSION: u32 = 1;
/// `format_version` of the saved [`TraversalJumpPoint`] (`traversal-jump-point`).
pub const TRAVERSAL_JUMP_POINT_FORMAT_VERSION: u32 = 1;

/// The skin kept between a moved capsule and the world when no `CharacterBody` says otherwise (m).
const DEFAULT_SKIN: f32 = 0.02;
/// Swept and fitted capsules are this much smaller than the real one, so a capsule resting at its
/// skin does not read as hit (m). Smaller than the skin.
const SWEEP_SHRINK: f32 = 0.01;
/// The shortest horizontal input that counts as intent to move.
const MIN_INTENT: f32 = 0.3;

// ---- settings -----------------------------------------------------------------------------------

/// Swim tuning (spec § 3). Distances are meters, speeds m/s, rates 1/s.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[reflect(Default)]
#[serde(default)]
pub struct SwimSettings {
    /// Allow swimming.
    pub enabled: bool,
    /// The chest's height above the capsule center: the line that decides entering the water.
    pub chest_offset: f32,
    /// Horizontal swim speed.
    pub speed: f32,
    /// Multiplier on `speed` while sprinting.
    pub sprint_multiplier: f32,
    /// Vertical speed of a held dive or 3D input.
    pub dive_speed: f32,
    /// Fastest rise back to the float line.
    pub rise_speed: f32,
    /// Vertical target speed per meter under the float line.
    pub float_rate: f32,
    /// How fast velocity follows its target.
    pub drag: f32,
    /// A dive stops this far under the float line.
    pub max_dive_depth: f32,
    /// Within this distance of the float line the phase reads `Surface`.
    pub surface_band: f32,
    /// A grounded swimmer whose chest is this far above the water walks.
    pub exit_margin: f32,
    /// An ungrounded swimmer whose chest is this far above the water falls.
    pub leave_margin: f32,
    /// Upward speed of a hop out of the water.
    pub jump_velocity: f32,
}

impl Default for SwimSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            chest_offset: 0.5,
            speed: 3.0,
            sprint_multiplier: 1.5,
            dive_speed: 2.0,
            rise_speed: 1.5,
            float_rate: 2.0,
            drag: 6.0,
            max_dive_depth: 6.0,
            surface_band: 0.25,
            exit_margin: 0.1,
            leave_margin: 0.6,
            jump_velocity: 4.0,
        }
    }
}

/// Ladder tuning of a character (spec § 4); the ladder's own numbers are on [`Ladder`].
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[reflect(Default)]
#[serde(default)]
pub struct LadderSettings {
    /// Allow ladders.
    pub enabled: bool,
    /// Extra lateral reach beyond the ladder's half width.
    pub attach_margin: f32,
    /// Extra reach in front of (and, at the top, behind) the climb line.
    pub attach_reach: f32,
    /// Smallest cosine between the input and the ladder's direction that attaches (0..=1).
    pub intent: f32,
    /// Seconds of the short curve onto the climb line.
    pub enter_time: f32,
    /// Speed of the walk off at the top.
    pub exit_speed: f32,
    /// Backward speed of a jump off.
    pub jump_off_speed: f32,
    /// Upward speed of a jump off.
    pub jump_off_up: f32,
    /// Seconds after a jump off before any ladder attaches again.
    pub cooldown: f32,
}

impl Default for LadderSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            attach_margin: 0.15,
            attach_reach: 0.5,
            intent: 0.5,
            enter_time: 0.2,
            exit_speed: 2.0,
            jump_off_speed: 3.0,
            jump_off_up: 3.0,
            cooldown: 0.5,
        }
    }
}

/// Mantle and vault tuning (spec § 5). Heights are meters above the feet.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[reflect(Default)]
#[serde(default)]
pub struct MantleSettings {
    /// Allow mantles and vaults.
    pub enabled: bool,
    /// While falling and moving, take a ledge without a key press.
    pub auto_in_air: bool,
    /// The lowest ledge.
    pub min_height: f32,
    /// The highest ledge.
    pub max_height: f32,
    /// How far ahead the wall may be.
    pub reach: f32,
    /// How far past the wall's face the top is read.
    pub edge_inset: f32,
    /// The steepest ledge top, radians.
    pub max_slope: f32,
    /// How deep the top must be to stand on.
    pub min_depth: f32,
    /// How far past the edge the capsule's rim ends up.
    pub stand_inset: f32,
    /// How far over the stand height the capsule passes the edge. At least 0.04.
    pub clearance: f32,
    /// Travel speed along the path.
    pub speed: f32,
    /// Allow vaults over thin walls.
    pub vault_enabled: bool,
    /// The highest wall a vault takes.
    pub vault_max_height: f32,
    /// The thickest wall a vault takes.
    pub vault_max_thickness: f32,
    /// How far below the feet a vault may land.
    pub vault_max_drop: f32,
    /// Travel speed of a vault.
    pub vault_speed: f32,
}

impl Default for MantleSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_in_air: true,
            min_height: 0.5,
            max_height: 2.0,
            reach: 0.6,
            edge_inset: 0.03,
            max_slope: 0.6,
            min_depth: 0.6,
            stand_inset: 0.05,
            clearance: 0.05,
            speed: 3.0,
            vault_enabled: true,
            vault_max_height: 1.2,
            vault_max_thickness: 0.5,
            vault_max_drop: 1.5,
            vault_speed: 4.0,
        }
    }
}

/// Gives a character the traversal moves (spec § 2). Saved in scenes (`traversal-settings` v1). Both
/// sides of a networked character must hold the same values, like `CharacterMovement`.
#[derive(Component, Reflect, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[reflect(Component, Default, FormatVersion)]
#[serde(default)]
#[require(CharacterTraversal)]
pub struct TraversalSettings {
    /// Swim tuning.
    pub swim: SwimSettings,
    /// Ladder tuning.
    pub ladder: LadderSettings,
    /// Mantle and vault tuning.
    pub mantle: MantleSettings,
    /// The montage slot named in [`TraversalEvent::Started`].
    pub montage_slot: String,
    /// Always [`TRAVERSAL_SETTINGS_FORMAT_VERSION`].
    pub format_version: u32,
}

impl regia_scene::spawn::FormatVersion for TraversalSettings {
    const CURRENT: u32 = TRAVERSAL_SETTINGS_FORMAT_VERSION;
}

impl Default for TraversalSettings {
    fn default() -> Self {
        Self {
            swim: SwimSettings::default(),
            ladder: LadderSettings::default(),
            mantle: MantleSettings::default(),
            montage_slot: "traversal".to_string(),
            format_version: TRAVERSAL_SETTINGS_FORMAT_VERSION,
        }
    }
}

/// A parameter a traversal type refuses. [`TraversalParamError::code`] is the protocol code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: `{field}` is not finite or out of range")]
pub struct TraversalParamError {
    /// The protocol code: `traversal_bad_settings`, `ladder_bad_params` or `jump_point_bad_params`.
    pub code: &'static str,
    /// The field at fault.
    pub field: &'static str,
}

impl TraversalParamError {
    /// The protocol code.
    pub fn code(&self) -> &'static str {
        self.code
    }
}

/// `value` is finite and at least `min`.
fn at_least(value: f32, min: f32) -> bool {
    value.is_finite() && value >= min
}

/// `value` is finite and above zero.
fn positive(value: f32) -> bool {
    value.is_finite() && value > 0.0
}

impl TraversalSettings {
    /// Checks every number.
    ///
    /// # Errors
    ///
    /// `traversal_bad_settings`, naming the first bad field.
    pub fn validate(&self) -> Result<(), TraversalParamError> {
        let (swim, ladder, mantle) = (&self.swim, &self.ladder, &self.mantle);
        let checks: [(&'static str, bool); 34] = [
            ("swim.chest_offset", swim.chest_offset.is_finite()),
            ("swim.speed", at_least(swim.speed, 0.0)),
            ("swim.sprint_multiplier", at_least(swim.sprint_multiplier, 0.0)),
            ("swim.dive_speed", at_least(swim.dive_speed, 0.0)),
            ("swim.rise_speed", at_least(swim.rise_speed, 0.0)),
            ("swim.float_rate", at_least(swim.float_rate, 0.0)),
            ("swim.drag", positive(swim.drag)),
            ("swim.max_dive_depth", at_least(swim.max_dive_depth, 0.0)),
            ("swim.surface_band", at_least(swim.surface_band, 0.0)),
            ("swim.exit_margin", at_least(swim.exit_margin, 0.0)),
            ("swim.leave_margin", at_least(swim.leave_margin, 0.0)),
            ("swim.jump_velocity", at_least(swim.jump_velocity, 0.0)),
            ("ladder.attach_margin", at_least(ladder.attach_margin, 0.0)),
            ("ladder.attach_reach", at_least(ladder.attach_reach, 0.0)),
            (
                "ladder.intent",
                at_least(ladder.intent, 0.0) && ladder.intent <= 1.0,
            ),
            ("ladder.enter_time", at_least(ladder.enter_time, 0.0)),
            ("ladder.exit_speed", positive(ladder.exit_speed)),
            ("ladder.jump_off_speed", at_least(ladder.jump_off_speed, 0.0)),
            ("ladder.jump_off_up", at_least(ladder.jump_off_up, 0.0)),
            ("ladder.cooldown", at_least(ladder.cooldown, 0.0)),
            ("mantle.min_height", at_least(mantle.min_height, 0.0)),
            (
                "mantle.max_height",
                at_least(mantle.max_height, 0.0) && mantle.max_height >= mantle.min_height,
            ),
            ("mantle.reach", positive(mantle.reach)),
            ("mantle.edge_inset", positive(mantle.edge_inset)),
            ("mantle.max_slope", at_least(mantle.max_slope, 0.0)),
            ("mantle.min_depth", positive(mantle.min_depth)),
            ("mantle.stand_inset", at_least(mantle.stand_inset, 0.0)),
            (
                "mantle.clearance",
                at_least(mantle.clearance, curve::CORNER_ROUND),
            ),
            ("mantle.speed", positive(mantle.speed)),
            ("mantle.vault_max_height", at_least(mantle.vault_max_height, 0.0)),
            (
                "mantle.vault_max_thickness",
                at_least(mantle.vault_max_thickness, 0.0),
            ),
            ("mantle.vault_max_drop", at_least(mantle.vault_max_drop, 0.0)),
            ("mantle.vault_speed", positive(mantle.vault_speed)),
            ("montage_slot", !self.montage_slot.is_empty()),
        ];
        match checks.iter().find(|(_, ok)| !ok) {
            Some((field, _)) => Err(TraversalParamError {
                code: "traversal_bad_settings",
                field,
            }),
            None => Ok(()),
        }
    }

    /// Upgrades an older saved shape (none yet: version 1 is the first).
    ///
    /// # Errors
    ///
    /// A message for a version newer than this build reads.
    pub fn migrate(&mut self) -> Result<(), String> {
        migrate_version(
            &mut self.format_version,
            TRAVERSAL_SETTINGS_FORMAT_VERSION,
            "traversal-settings",
        )
    }
}

/// Sets `version` to `current`, or refuses a newer one.
fn migrate_version(version: &mut u32, current: u32, name: &str) -> Result<(), String> {
    if *version > current {
        return Err(format!("{name} format_version {version} is newer than {current}"));
    }
    *version = current;
    Ok(())
}

/// The shape fingerprint of `traversal-settings`.
pub fn traversal_settings_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<TraversalSettings>(),
        serde_field_names::<SwimSettings>(),
        serde_field_names::<LadderSettings>(),
        serde_field_names::<MantleSettings>(),
    ])
}

/// The `from` versions of `traversal-settings`'s migration steps (none: v1 is the first).
pub fn traversal_settings_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The shape fingerprint of `traversal-ladder`.
pub fn traversal_ladder_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<Ladder>()])
}

/// The `from` versions of `traversal-ladder`'s migration steps (none: v1 is the first).
pub fn traversal_ladder_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The shape fingerprint of `traversal-jump-point`.
pub fn traversal_jump_point_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<TraversalJumpPoint>()])
}

/// The `from` versions of `traversal-jump-point`'s migration steps (none: v1 is the first).
pub fn traversal_jump_point_migration_sources() -> Vec<u32> {
    Vec::new()
}

// ---- authored volumes ---------------------------------------------------------------------------

/// A ladder (spec § 4): rungs from the entity's origin up +Y for `height`, climbed from the entity's
/// local +Z side, facing its local -Z. The entity's yaw and position count; tilt and scale do not.
/// Static: a moved ladder applies at the next attach. Saved in scenes (`traversal-ladder` v1).
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[reflect(Component, Default, FormatVersion)]
#[serde(default)]
pub struct Ladder {
    /// Rung half width (m).
    pub half_width: f32,
    /// Height from the base to the top, where the upper floor is (m).
    pub height: f32,
    /// Climb speed (m/s).
    pub climb_speed: f32,
    /// The capsule center's distance from the ladder's plane (m); at least the capsule's radius
    /// plus a small gap in practice.
    pub standoff: f32,
    /// How far past the plane the top exit stands (m).
    pub exit_forward: f32,
    /// The game's id, also the nav link's `user_id`; 0 = unnamed.
    pub user_id: u64,
    /// Publish a nav link ([`nav::TraversalNavPlugin`]).
    pub nav_link: bool,
    /// The runtime switch: a disabled ladder attaches nobody.
    pub enabled: bool,
    /// Always [`TRAVERSAL_LADDER_FORMAT_VERSION`].
    pub format_version: u32,
}

impl regia_scene::spawn::FormatVersion for Ladder {
    const CURRENT: u32 = TRAVERSAL_LADDER_FORMAT_VERSION;
}

impl Default for Ladder {
    fn default() -> Self {
        Self {
            half_width: 0.35,
            height: 3.0,
            climb_speed: 1.5,
            standoff: 0.4,
            exit_forward: 0.6,
            user_id: 0,
            nav_link: true,
            enabled: true,
            format_version: TRAVERSAL_LADDER_FORMAT_VERSION,
        }
    }
}

impl Ladder {
    /// Checks every number.
    ///
    /// # Errors
    ///
    /// `ladder_bad_params`, naming the first bad field.
    pub fn validate(&self) -> Result<(), TraversalParamError> {
        let checks: [(&'static str, bool); 5] = [
            ("half_width", at_least(self.half_width, 0.0)),
            ("height", positive(self.height)),
            ("climb_speed", positive(self.climb_speed)),
            ("standoff", at_least(self.standoff, 0.0)),
            ("exit_forward", positive(self.exit_forward)),
        ];
        match checks.iter().find(|(_, ok)| !ok) {
            Some((field, _)) => Err(TraversalParamError {
                code: "ladder_bad_params",
                field,
            }),
            None => Ok(()),
        }
    }

    /// Upgrades an older saved shape (none yet: version 1 is the first).
    ///
    /// # Errors
    ///
    /// A message for a version newer than this build reads.
    pub fn migrate(&mut self) -> Result<(), String> {
        migrate_version(
            &mut self.format_version,
            TRAVERSAL_LADDER_FORMAT_VERSION,
            "traversal-ladder",
        )
    }
}

/// An authored jump (spec § 7): takes off at the entity and lands at `to`, a point in its local
/// space on the landing floor. AI reaches it through its nav link. Saved in scenes
/// (`traversal-jump-point` v1).
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[reflect(Component, Default, FormatVersion)]
#[serde(default)]
pub struct TraversalJumpPoint {
    /// The landing, in the entity's local space.
    pub to: Vec3,
    /// Usable both ways.
    pub bidirectional: bool,
    /// The game's id, also the nav link's `user_id`; 0 = unnamed.
    pub user_id: u64,
    /// How far above the higher end the capsule's path passes (m).
    pub arc_height: f32,
    /// The runtime switch: a disabled point publishes no link.
    pub enabled: bool,
    /// Always [`TRAVERSAL_JUMP_POINT_FORMAT_VERSION`].
    pub format_version: u32,
}

impl regia_scene::spawn::FormatVersion for TraversalJumpPoint {
    const CURRENT: u32 = TRAVERSAL_JUMP_POINT_FORMAT_VERSION;
}

impl Default for TraversalJumpPoint {
    fn default() -> Self {
        Self {
            to: Vec3::new(0.0, 0.0, -3.0),
            bidirectional: false,
            user_id: 0,
            arc_height: 1.0,
            enabled: true,
            format_version: TRAVERSAL_JUMP_POINT_FORMAT_VERSION,
        }
    }
}

impl TraversalJumpPoint {
    /// Checks every number.
    ///
    /// # Errors
    ///
    /// `jump_point_bad_params`, naming the first bad field.
    pub fn validate(&self) -> Result<(), TraversalParamError> {
        let checks: [(&'static str, bool); 2] = [
            ("to", self.to.is_finite()),
            ("arc_height", at_least(self.arc_height, 0.0)),
        ];
        match checks.iter().find(|(_, ok)| !ok) {
            Some((field, _)) => Err(TraversalParamError {
                code: "jump_point_bad_params",
                field,
            }),
            None => Ok(()),
        }
    }

    /// Upgrades an older saved shape (none yet: version 1 is the first).
    ///
    /// # Errors
    ///
    /// A message for a version newer than this build reads.
    pub fn migrate(&mut self) -> Result<(), String> {
        migrate_version(
            &mut self.format_version,
            TRAVERSAL_JUMP_POINT_FORMAT_VERSION,
            "traversal-jump-point",
        )
    }
}

// ---- state --------------------------------------------------------------------------------------

/// The move a character is in.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Default)]
pub enum TraversalKind {
    /// No move: `step_character`'s own movement applies.
    #[default]
    None,
    /// In the water.
    Swim,
    /// On a ladder's climb line.
    Ladder,
    /// The short curve onto a ladder.
    LadderEnter,
    /// The curve off the top of a ladder.
    LadderExit,
    /// Up onto a ledge.
    Mantle,
    /// Over a thin wall.
    Vault,
    /// An authored or requested jump.
    Jump,
}

impl TraversalKind {
    /// The name the agent protocol and montages use.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Swim => "swim",
            Self::Ladder => "ladder",
            Self::LadderEnter => "ladder_enter",
            Self::LadderExit => "ladder_exit",
            Self::Mantle => "mantle",
            Self::Vault => "vault",
            Self::Jump => "jump",
        }
    }

    /// Whether this kind moves along a curve.
    pub fn is_curve(self) -> bool {
        matches!(
            self,
            Self::LadderEnter | Self::LadderExit | Self::Mantle | Self::Vault | Self::Jump
        )
    }
}

/// Where a swimmer is relative to the float line.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Default)]
pub enum SwimPhase {
    /// At the float line.
    #[default]
    Surface,
    /// Under it.
    Underwater,
}

/// Why a move was refused: part of the agent protocol ([`TraversalRefusal::code`], spec § 6).
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraversalRefusal {
    /// Nothing to climb in reach.
    LedgeNoWall,
    /// The top is under `min_height`.
    LedgeTooLow,
    /// The top is over `max_height`.
    LedgeTooHigh,
    /// The top is steeper than `max_slope`.
    LedgeTooSteep,
    /// The top is shallower than `min_depth` and no vault applies.
    LedgeTooThin,
    /// The capsule does not fit on the ledge or on the way there.
    LedgeNoHeadroom,
    /// A vaultable wall has no floor or room behind it.
    VaultNoLanding,
    /// A ladder request found no ladder.
    LadderNoneInReach,
    /// The capsule does not fit on the climb line.
    LadderNoRoom,
    /// The top exit has no room.
    LadderExitBlocked,
    /// The top exit has no floor.
    LadderExitNoFloor,
    /// Something entered the path of a running curve, or moved the character off it.
    TraversalInterrupted,
    /// No physics world: ledge and ladder moves cannot run.
    TraversalNoPhysics,
    /// The character's settings hold a bad value.
    TraversalBadSettings,
    /// A request holds a non-finite point.
    TraversalBadRequest,
    /// A request arrived during a ladder climb or a curve, and was dropped.
    TraversalBusy,
    /// A request asked for a move the character's settings turn off.
    TraversalDisabled,
}

impl TraversalRefusal {
    /// Every refusal, in wire order.
    pub const ALL: [Self; 17] = [
        Self::LedgeNoWall,
        Self::LedgeTooLow,
        Self::LedgeTooHigh,
        Self::LedgeTooSteep,
        Self::LedgeTooThin,
        Self::LedgeNoHeadroom,
        Self::VaultNoLanding,
        Self::LadderNoneInReach,
        Self::LadderNoRoom,
        Self::LadderExitBlocked,
        Self::LadderExitNoFloor,
        Self::TraversalInterrupted,
        Self::TraversalNoPhysics,
        Self::TraversalBadSettings,
        Self::TraversalBadRequest,
        Self::TraversalBusy,
        Self::TraversalDisabled,
    ];

    /// The protocol code.
    pub fn code(self) -> &'static str {
        match self {
            Self::LedgeNoWall => "ledge_no_wall",
            Self::LedgeTooLow => "ledge_too_low",
            Self::LedgeTooHigh => "ledge_too_high",
            Self::LedgeTooSteep => "ledge_too_steep",
            Self::LedgeTooThin => "ledge_too_thin",
            Self::LedgeNoHeadroom => "ledge_no_headroom",
            Self::VaultNoLanding => "vault_no_landing",
            Self::LadderNoneInReach => "ladder_none_in_reach",
            Self::LadderNoRoom => "ladder_no_room",
            Self::LadderExitBlocked => "ladder_exit_blocked",
            Self::LadderExitNoFloor => "ladder_exit_no_floor",
            Self::TraversalInterrupted => "traversal_interrupted",
            Self::TraversalNoPhysics => "traversal_no_physics",
            Self::TraversalBadSettings => "traversal_bad_settings",
            Self::TraversalBadRequest => "traversal_bad_request",
            Self::TraversalBusy => "traversal_busy",
            Self::TraversalDisabled => "traversal_disabled",
        }
    }
}

/// The ladder a character is on, copied into its state so a re-run needs no entity lookup and no
/// ladder crosses the wire.
#[derive(Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Default)]
pub struct LadderLine {
    /// The climb line: `x` and `z` of the capsule's center, `y` where its feet are at the base.
    pub line: Vec3,
    /// The horizontal direction into the ladder; the locked facing.
    pub into: Vec3,
    /// The center's height where its feet are at the top.
    pub top_y: f32,
    /// Climb speed (m/s).
    pub speed: f32,
    /// The capsule center at the top exit.
    pub exit: Vec3,
    /// The ladder's `user_id`.
    pub id: u64,
}

impl Default for LadderLine {
    fn default() -> Self {
        Self {
            line: Vec3::ZERO,
            into: Vec3::NEG_Z,
            top_y: 0.0,
            speed: 0.0,
            exit: Vec3::ZERO,
            id: 0,
        }
    }
}

/// A move a host or the nav bridge asks a character to make (spec § 7).
#[derive(Reflect, Clone, Copy, Debug, PartialEq)]
pub enum TraversalOrder {
    /// Try a ledge in this direction.
    Mantle {
        /// The horizontal direction to look in.
        forward: Vec3,
    },
    /// Get on a ladder and climb it by itself.
    Ladder {
        /// The ladder's `user_id`; 0 = the nearest to `near`.
        user_id: u64,
        /// Where the crossing starts.
        near: Vec3,
        /// Climb up (else down).
        up: bool,
    },
    /// Run the jump curve to a point on the landing floor.
    Jump {
        /// The point on the landing floor.
        to: Vec3,
        /// How far above the higher end the path passes (m).
        arc_height: f32,
    },
}

/// The state of a character's move (spec § 2, § 8). Predicted and replicated with the `net` feature;
/// it holds what a re-run needs (the curve's points, the ladder's line), so nothing here names an
/// entity.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct CharacterTraversal {
    /// The running move.
    pub kind: TraversalKind,
    /// Surface or underwater, while swimming.
    pub phase: SwimPhase,
    /// Metres under the float line, while swimming.
    pub depth: f32,
    /// Fixed steps of the curve already run.
    pub step: u32,
    /// Fixed steps the curve takes.
    pub steps: u32,
    /// The curve's four control points (see [`path_point`]).
    pub path: [Vec3; 4],
    /// The ladder, while on or entering one.
    pub ladder: LadderLine,
    /// A self-driven climb: +1 up, -1 down, 0 follows the input.
    pub auto_climb: i8,
    /// Fixed steps until a ladder attaches again.
    pub cooldown_steps: u32,
    /// The last refusal; stays until a later move starts.
    pub last_refusal: Option<TraversalRefusal>,
    /// A move to try at the next step.
    pub pending: Option<TraversalOrder>,
    /// The floating origin these points are written in (`regia_scene::streaming::OriginOffset`). A step
    /// moves them into its own frame first, which covers a local rebase and a peer whose origin
    /// differs: the state crosses the wire with its frame.
    pub frame: [f64; 3],
}

/// The largest coordinate a state may hold (m): far beyond any level, and its squares stay finite.
const MAX_COORD: f32 = 1.0e9;
/// The most fixed steps a state may say a curve takes.
const MAX_CURVE_STEPS: u32 = 1 << 24;

/// A point a state may hold.
fn sane_point(point: Vec3) -> bool {
    point.is_finite() && point.abs().max_element() <= MAX_COORD
}

impl CharacterTraversal {
    /// Whether the numbers of the running move can be stepped: finite, in range and consistent. A state
    /// written by anything but the step (a peer, a save, a tool) may not be.
    pub fn is_sane(&self) -> bool {
        let curve_ok = !self.kind.is_curve()
            || (self.path.iter().all(|p| sane_point(*p))
                && self.step <= self.steps
                && self.steps <= MAX_CURVE_STEPS);
        let line = &self.ladder;
        let ladder_ok = !matches!(self.kind, TraversalKind::Ladder | TraversalKind::LadderEnter)
            || (sane_point(line.line)
                && sane_point(line.exit)
                && sane_point(line.into)
                && line.into.length_squared() > 0.25
                && line.top_y.is_finite()
                && line.top_y >= line.line.y
                && line.top_y.abs() <= MAX_COORD
                && line.speed.is_finite()
                && line.speed >= 0.0);
        curve_ok && ladder_ok && self.frame.iter().all(|axis| axis.is_finite())
    }

    /// Moves every stored point into the frame `to`, and records it.
    fn reframe(&mut self, to: [f64; 3]) {
        if self.frame == to {
            return;
        }
        let from = self.frame;
        let shift = |p: Vec3| {
            Vec3::new(
                (f64::from(p.x) + from[0] - to[0]) as f32,
                (f64::from(p.y) + from[1] - to[1]) as f32,
                (f64::from(p.z) + from[2] - to[2]) as f32,
            )
        };
        for point in &mut self.path {
            *point = shift(*point);
        }
        self.ladder.line = shift(self.ladder.line);
        self.ladder.exit = shift(self.ladder.exit);
        self.ladder.top_y = shift(Vec3::new(0.0, self.ladder.top_y, 0.0)).y;
        match &mut self.pending {
            Some(TraversalOrder::Ladder { near, .. }) => *near = shift(*near),
            Some(TraversalOrder::Jump { to: point, .. }) => *point = shift(*point),
            _ => {}
        }
        self.frame = to;
    }

    /// Queues `order` for the character's next step. Its points are in the frame of
    /// [`frame`](Self::frame) (the origin of the character's last step): the step moves them to
    /// its own origin. A caller holding points in the current local frame after a floating-origin
    /// rebase, or before a new character's first step, uses [`request_in`](Self::request_in).
    pub fn request(&mut self, order: TraversalOrder) {
        self.pending = Some(order);
    }

    /// Queues `order`, whose points are in the local frame of `origin` (the
    /// `regia_scene::streaming::OriginOffset` value they were read under).
    pub fn request_in(&mut self, order: TraversalOrder, origin: [f64; 3]) {
        self.reframe(origin);
        self.pending = Some(order);
    }

    /// How far a curve has come, 0..=1 (0 when no curve runs).
    pub fn progress(&self) -> f32 {
        if self.kind.is_curve() && self.steps > 0 {
            self.step as f32 / self.steps as f32
        } else {
            0.0
        }
    }
}

// ---- messages -----------------------------------------------------------------------------------

/// A named warp target for a move's montage.
#[derive(Clone, Debug, PartialEq)]
pub struct TraversalWarpTarget {
    /// The target's name (`ledge_edge`, `ledge_stand`, `vault_over`, `vault_land`).
    pub name: &'static str,
    /// The target.
    pub target: WarpTarget,
}

/// What a character's moves report (spec § 5, § 6). Sent on the first run of a tick only.
#[derive(Message, Clone, Debug, PartialEq)]
pub enum TraversalEvent {
    /// A move began.
    Started {
        /// The character.
        entity: Entity,
        /// The move.
        kind: TraversalKind,
        /// The montage slot ([`TraversalSettings::montage_slot`]).
        slot: String,
        /// The montage name: the kind's name.
        montage: &'static str,
        /// Seconds a curve takes (0 for swim and ladder).
        duration: f32,
        /// Warp targets for the montage.
        warp_targets: Vec<TraversalWarpTarget>,
    },
    /// A move ended normally.
    Finished {
        /// The character.
        entity: Entity,
        /// The move.
        kind: TraversalKind,
    },
    /// An attempt was refused.
    Refused {
        /// The character.
        entity: Entity,
        /// Why.
        reason: TraversalRefusal,
    },
}

/// A crowd agent took off on a link, or a host wants a character to use one (spec § 7).
#[derive(Message, Clone, Copy, Debug, PartialEq)]
pub struct TraversalRequest {
    /// The agent's raw id (`regia_nav::CrowdAgentId`), `None` from a host.
    pub agent: Option<u64>,
    /// The character to move, when one is tied to the agent.
    pub entity: Option<Entity>,
    /// The link's kind.
    pub kind: regia_nav::NavLinkKind,
    /// The link's user id.
    pub user_id: u64,
    /// The crossing's start on the navmesh.
    pub start: Vec3,
    /// The crossing's end on the navmesh.
    pub end: Vec3,
    /// How far above the higher end a jump's path passes (m): an authored [`TraversalJumpPoint`]'s
    /// `arc_height`, else a low default.
    pub arc_height: f32,
}

/// Registers the traversal types and messages and applies [`TraversalRequest`]s that name a
/// character. The moves run without it, as long as a character has [`TraversalSettings`], but no
/// [`TraversalEvent`] is sent. Use with [`CharacterPlugin`](crate::character::CharacterPlugin).
pub struct TraversalPlugin;

impl Plugin for TraversalPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<TraversalSettings>()
            .register_type::<SwimSettings>()
            .register_type::<LadderSettings>()
            .register_type::<MantleSettings>()
            .register_type::<CharacterTraversal>()
            .register_type::<TraversalKind>()
            .register_type::<SwimPhase>()
            .register_type::<TraversalRefusal>()
            .register_type::<TraversalOrder>()
            .register_type::<LadderLine>()
            .register_type::<Ladder>()
            .register_type::<TraversalJumpPoint>()
            .add_message::<TraversalEvent>()
            .add_message::<TraversalRequest>()
            .add_systems(
                FixedUpdate,
                apply_traversal_requests.before(crate::character::movement_step),
            );
    }
}

/// `FixedUpdate`: queues each request that names a character on that character.
pub(crate) fn apply_traversal_requests(
    mut requests: MessageReader<TraversalRequest>,
    mut characters: Query<&mut CharacterTraversal>,
    origin: Option<Res<regia_scene::streaming::OriginOffset>>,
) {
    // Requests carry points in the current local frame.
    let origin = origin.map_or([0.0; 3], |offset| offset.0);
    for request in requests.read() {
        let Some(entity) = request.entity else {
            continue;
        };
        let Ok(mut traversal) = characters.get_mut(entity) else {
            continue;
        };
        if let Some(order) = nav::order_for(request) {
            traversal.request_in(order, origin);
        }
    }
}

// ---- world helpers ------------------------------------------------------------------------------

/// The physics queries, when the world has a physics world.
fn queries(world: &World) -> Option<WorldQueries<'_>> {
    Some(WorldQueries {
        world: world.get_resource::<PhysicsWorld>()?,
        layers: world.get_resource::<PhysicsLayers>(),
    })
}

/// The capsule shape the sweeps and fit tests use.
fn sweep_shape(radius: f32, half_height: f32) -> ColliderShape {
    ColliderShape::Capsule {
        half_height: half_height.max(0.0),
        radius: (radius - SWEEP_SHRINK).max(0.001),
        axis: ShapeAxis::Y,
    }
}

/// How many parents a collider's owner may be under and still count as the character's.
const MAX_OWN_DEPTH: u32 = 32;

/// Whether `other` is `entity` or under it: a character's own hitboxes are colliders of its own body, which
/// its sweep ignores (`regia_physics::move_character`), and these queries must too.
pub(crate) fn is_own(world: &World, entity: Entity, other: Entity) -> bool {
    let mut at = other;
    for _ in 0..MAX_OWN_DEPTH {
        if at == entity {
            return true;
        }
        match world.get::<ChildOf>(at) {
            Some(parent) => at = parent.parent(),
            None => return false,
        }
    }
    false
}

/// A capsule of one character, for clear-space questions.
#[derive(Clone, Copy)]
pub(crate) struct Body {
    pub entity: Entity,
    pub radius: f32,
    pub half_height: f32,
}

impl Body {
    /// How far from `from` the capsule can move toward `to` before it hits something; `None` when
    /// the whole way is clear (or the world has no physics).
    pub fn sweep(&self, world: &World, from: Vec3, to: Vec3) -> Option<f32> {
        let queries = queries(world)?;
        let delta = to - from;
        let length = delta.length();
        if length <= 1.0e-6 {
            return None;
        }
        let not_own = |other: Entity| !is_own(world, self.entity, other);
        let filter = QueryFilter::new().exclude_sensors().predicate(&not_own);
        queries
            .shapecast(
                &sweep_shape(self.radius, self.half_height),
                Transform::from_translation(from),
                delta / length,
                length,
                filter,
            )
            .map(|hit| hit.time_of_impact)
    }

    /// Whether the capsule fits at `at`. A tiny cast joins the overlap test because an overlap sees
    /// no exact shape for a heightfield.
    pub fn fits(&self, world: &World, at: Vec3) -> bool {
        let Some(queries) = queries(world) else {
            return true;
        };
        let not_own = |other: Entity| !is_own(world, self.entity, other);
        let shape = sweep_shape(self.radius, self.half_height);
        let pose = Transform::from_translation(at);
        let filter = QueryFilter::new().exclude_sensors().predicate(&not_own);
        if !queries.overlap(&shape, pose, filter).is_empty() {
            return false;
        }
        let filter = QueryFilter::new().exclude_sensors().predicate(&not_own);
        queries
            .shapecast(&shape, pose, Vec3::NEG_Y, 1.0e-3, filter)
            .is_none()
    }
}

/// The skin the character's sweeps keep.
pub(crate) fn skin_of(world: &World, entity: Entity) -> f32 {
    world
        .get::<CharacterBody>(entity)
        .map_or(DEFAULT_SKIN, |body| body.offset.max(0.0))
}

/// The input's horizontal direction when it is long enough to be intent.
fn intent_of(input: &CharacterInput) -> Option<Vec3> {
    let horizontal = Vec3::new(input.move_dir.x, 0.0, input.move_dir.z);
    (horizontal.is_finite() && horizontal.length() >= MIN_INTENT).then(|| horizontal.normalize())
}

/// Sends `event`, unless this is a re-run of a tick (the first run already told everyone).
fn send_event(world: &mut World, event: TraversalEvent) {
    #[cfg(feature = "net")]
    if world
        .get_resource::<regia_net::NetResimulating>()
        .is_some_and(|r| r.0)
    {
        return;
    }
    if let Some(mut messages) = world.get_resource_mut::<Messages<TraversalEvent>>() {
        messages.write(event);
    }
}

/// Records a refused attempt.
fn refuse(world: &mut World, entity: Entity, traversal: &mut CharacterTraversal, reason: TraversalRefusal) {
    traversal.last_refusal = Some(reason);
    send_event(world, TraversalEvent::Refused { entity, reason });
}

/// The facing that looks along `into` (characters face -Z). On the deterministic kernel, like the
/// character step's own facing: the traversal writes the rotation every ladder or curve step, so
/// platform libm `atan2`/`sin`/`cos` would let peers and replays drift by an ulp.
fn facing_along(into: Vec3) -> Quat {
    crate::character::yaw_rotation(regia_core::math::det_atan2(-into.x, -into.z))
}

/// The state every move that is not grounded walking leaves.
fn in_move_state(state: &mut CharacterState, mode: MovementMode, velocity: Vec3) {
    state.velocity = if velocity.is_finite() {
        velocity
    } else {
        Vec3::ZERO
    };
    state.grounded = false;
    state.mode = mode;
    state.time_in_air = 0.0;
    state.landed_this_frame = false;
    state.jumps_used = 0;
    state.coyote_time_left = 0.0;
    state.jump_buffer_time_left = 0.0;
    state.crouched = false;
    state.ground_velocity = Vec3::ZERO;
}

/// A sweep this step made itself reports no ground to gameplay.
fn clear_body_ground(world: &mut World, entity: Entity) {
    if let Some(mut body) = world.get_mut::<CharacterBody>(entity)
        && body.ground.grounded
    {
        body.ground.grounded = false;
        body.ground.ground_entity = Entity::PLACEHOLDER;
        body.ground.ground_velocity = Vec3::ZERO;
    }
}

/// The result of a step that moved the capsule by itself.
fn free_result(position: Vec3) -> MoveResult {
    MoveResult {
        position,
        grounded: false,
        ground_normal: Vec3::Y,
        ground_angle: std::f32::consts::FRAC_PI_2,
        ..MoveResult::default()
    }
}

/// What one traversal step works with.
pub(crate) struct StepCx<'a> {
    pub collision: &'a dyn CollisionQuery,
    pub entity: Entity,
    pub movement: &'a CharacterMovement,
    pub input: &'a CharacterInput,
    pub dt: f32,
    pub swim: SwimSettings,
    pub ladder: LadderSettings,
    pub mantle: MantleSettings,
    pub body: Body,
    pub skin: f32,
}

/// Starts a curve: sends the event and leaves the state on the curve's first step.
fn start_curve(
    world: &mut World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    kind: TraversalKind,
    path: [Vec3; 4],
    speed: f32,
    warp_targets: Vec<TraversalWarpTarget>,
) {
    let steps = curve::steps_for(&path, speed, cx.dt);
    // Whatever ran until now ends first, so a host sees every start closed before the next one.
    if traversal.kind != TraversalKind::None {
        send_event(
            world,
            TraversalEvent::Finished {
                entity: cx.entity,
                kind: traversal.kind,
            },
        );
    }
    traversal.kind = kind;
    traversal.path = path;
    traversal.step = 0;
    traversal.steps = steps;
    traversal.last_refusal = None;
    traversal.pending = None;
    traversal.phase = SwimPhase::Surface;
    traversal.depth = 0.0;
    let slot = world
        .get::<TraversalSettings>(cx.entity)
        .map(|s| s.montage_slot.clone())
        .unwrap_or_default();
    send_event(
        world,
        TraversalEvent::Started {
            entity: cx.entity,
            kind,
            slot,
            montage: kind.name(),
            duration: steps as f32 * cx.dt,
            warp_targets,
        },
    );
}

/// What a traversal step tells `step_character`.
pub(crate) enum TraversalStep {
    /// A move took the whole step.
    Done(MoveResult),
    /// No move: the plain movement runs as if nothing had looked.
    Plain,
    /// A move ended: the plain movement runs from the mode and state left, and this step's jump press
    /// is spent (a jump off a ladder or out of the water is not also a jump).
    PlainInputSpent,
}

/// One traversal step, from `step_character`, for a character with [`TraversalSettings`].
pub(crate) fn step_traversal(
    world: &mut World,
    collision: &dyn CollisionQuery,
    entity: Entity,
    movement: &CharacterMovement,
    input: &CharacterInput,
    dt: f32,
) -> TraversalStep {
    let _span = tracing::trace_span!(target: "regia_framework", "character_traversal").entered();
    let Some(settings) = world.get::<TraversalSettings>(entity) else {
        return TraversalStep::Plain;
    };
    let validity = settings.validate();
    let (swim, ladder, mantle) = (settings.swim, settings.ladder, settings.mantle);
    let mut traversal = world
        .get::<CharacterTraversal>(entity)
        .copied()
        .unwrap_or_default();
    let before = traversal;
    let origin = world
        .get_resource::<regia_scene::streaming::OriginOffset>()
        .map_or([0.0; 3], |offset| offset.0);
    traversal.reframe(origin);

    if let Err(error) = validity {
        // Bad numbers: no moves run, and any running move ends. Default movement carries on.
        if traversal.last_refusal != Some(TraversalRefusal::TraversalBadSettings) {
            tracing::warn!(target: "regia_framework", %error, "traversal.bad_settings");
        }
        let was_moving = traversal.kind != TraversalKind::None;
        if was_moving {
            send_event(
                world,
                TraversalEvent::Finished {
                    entity,
                    kind: traversal.kind,
                },
            );
        }
        traversal = CharacterTraversal {
            last_refusal: Some(TraversalRefusal::TraversalBadSettings),
            frame: origin,
            ..CharacterTraversal::default()
        };
        store(world, entity, &before, &traversal);
        if was_moving || matches!(movement.mode, MovementMode::CLIMBING | MovementMode::TRAVERSING) {
            leave_to_falling(world, entity);
        }
        return TraversalStep::Plain;
    }
    if !traversal.is_sane() {
        // Not a state the step could have written. Nothing of it can be trusted, so the move ends.
        tracing::warn!(target: "regia_framework", ?entity, "traversal.bad_state");
        send_event(
            world,
            TraversalEvent::Refused {
                entity,
                reason: TraversalRefusal::TraversalInterrupted,
            },
        );
        traversal = CharacterTraversal {
            last_refusal: Some(TraversalRefusal::TraversalInterrupted),
            frame: origin,
            ..CharacterTraversal::default()
        };
        store(world, entity, &before, &traversal);
        leave_to_falling(world, entity);
        return TraversalStep::PlainInputSpent;
    }

    let mut state = world.get::<CharacterState>(entity).cloned().unwrap_or_default();
    let Some(transform) = world.get::<Transform>(entity).copied() else {
        return TraversalStep::Plain;
    };
    if !transform.translation.is_finite() {
        // The plain step keeps its own guard; a move has nothing to start from.
        return TraversalStep::Plain;
    }
    // A non-finite stick counts as no stick, so it can never reach a position.
    let input = &CharacterInput {
        move_dir: if input.move_dir.is_finite() {
            input.move_dir
        } else {
            Vec3::ZERO
        },
        ..input.clone()
    };
    let cx = StepCx {
        collision,
        entity,
        movement,
        input,
        dt,
        swim,
        ladder,
        mantle,
        body: Body {
            entity,
            radius: movement.radius,
            half_height: movement.half_height,
        },
        skin: skin_of(world, entity),
    };
    traversal.cooldown_steps = traversal.cooldown_steps.saturating_sub(1);
    // A request cannot interrupt a climb or a curve. Dropped and reported, it never fires late.
    if (traversal.kind == TraversalKind::Ladder || traversal.kind.is_curve())
        && traversal.pending.take().is_some()
    {
        refuse(world, entity, &mut traversal, TraversalRefusal::TraversalBusy);
    }

    let outcome = match traversal.kind {
        TraversalKind::None => enter_move(world, &cx, &mut traversal, &mut state, &transform),
        TraversalKind::Swim => swim::step(world, &cx, &mut traversal, &mut state, &transform),
        TraversalKind::Ladder => ladder::step(world, &cx, &mut traversal, &mut state, &transform),
        _ => curve::step(world, &cx, &mut traversal, &mut state, &transform),
    };
    store(world, entity, &before, &traversal);
    match outcome {
        Outcome::Moved(result, position, rotation, mode) => {
            drain_root_motion(world, entity);
            let next = Transform {
                translation: position,
                rotation,
                ..transform
            };
            state.mode = mode;
            write_step(world, entity, next, state, mode);
            TraversalStep::Done(result)
        }
        Outcome::Plain => TraversalStep::Plain,
        Outcome::PlainWith(mode) => {
            state.mode = mode;
            if let Some(mut stored) = world.get_mut::<CharacterState>(entity) {
                *stored = state;
            }
            if let Some(mut stored) = world.get_mut::<CharacterMovement>(entity) {
                stored.mode = mode;
            }
            TraversalStep::PlainInputSpent
        }
    }
}

/// How a traversal step ended.
pub(crate) enum Outcome {
    /// A move took the step: the result, the position, the facing and the mode.
    Moved(MoveResult, Vec3, Quat, MovementMode),
    /// No move: plain movement runs.
    Plain,
    /// A move ended: plain movement runs from this mode, with the step's state.
    PlainWith(MovementMode),
}

/// Writes the component back only when it changed, so an idle character has no change ticks.
fn store(world: &mut World, entity: Entity, before: &CharacterTraversal, after: &CharacterTraversal) {
    if before != after
        && let Some(mut stored) = world.get_mut::<CharacterTraversal>(entity)
    {
        *stored = *after;
    }
}

/// A running move ended outside its own step.
fn leave_to_falling(world: &mut World, entity: Entity) {
    if let Some(mut state) = world.get_mut::<CharacterState>(entity) {
        state.mode = MovementMode::Falling;
    }
    if let Some(mut movement) = world.get_mut::<CharacterMovement>(entity) {
        movement.mode = MovementMode::Falling;
    }
}

/// With no move running: tries the requested move, then a ledge, a ladder and the water.
fn enter_move(
    world: &mut World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
) -> Outcome {
    let mode = cx.movement.mode;
    // One of the moves' modes with no move running (a host wrote it, a save restored it): the character
    // is simply in the air, and the moves below get their say.
    let stale = matches!(
        mode,
        MovementMode::Swimming | MovementMode::CLIMBING | MovementMode::TRAVERSING
    );
    if !stale && !matches!(mode, MovementMode::Walking | MovementMode::Falling) {
        return Outcome::Plain;
    }
    if let Some(order) = traversal.pending.take()
        && let Some(outcome) = run_order(world, cx, traversal, state, transform, order)
    {
        return outcome;
    }
    if let Some(outcome) = try_ledge(world, cx, traversal, state, transform, !state.grounded) {
        return outcome;
    }
    if let Some(outcome) = ladder::try_attach(world, cx, traversal, state, transform) {
        return outcome;
    }
    swim::try_enter(world, cx, traversal, state, transform).unwrap_or(if stale {
        Outcome::PlainWith(MovementMode::Falling)
    } else {
        Outcome::Plain
    })
}

/// Runs a requested move. `None`: refused, and plain movement goes on.
fn run_order(
    world: &mut World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
    order: TraversalOrder,
) -> Option<Outcome> {
    match order {
        TraversalOrder::Mantle { forward } => {
            if !cx.mantle.enabled {
                refuse(world, cx.entity, traversal, TraversalRefusal::TraversalDisabled);
                return None;
            }
            let forward = Vec3::new(forward.x, 0.0, forward.z);
            if !forward.is_finite() || forward.length_squared() < 1.0e-6 {
                refuse(world, cx.entity, traversal, TraversalRefusal::TraversalBadRequest);
                return None;
            }
            match ledge::probe_for_step(world, cx, transform.translation, forward) {
                Ok(probe) => Some(ledge::start(world, cx, traversal, state, transform, &probe)),
                Err(reason) => {
                    refuse(world, cx.entity, traversal, reason);
                    None
                }
            }
        }
        TraversalOrder::Ladder { user_id, near, up } => {
            if !cx.ladder.enabled {
                refuse(world, cx.entity, traversal, TraversalRefusal::TraversalDisabled);
                return None;
            }
            if !near.is_finite() {
                refuse(world, cx.entity, traversal, TraversalRefusal::TraversalBadRequest);
                return None;
            }
            match ladder::attach_by_order(world, cx, traversal, state, transform, (user_id, near, up)) {
                Ok(outcome) => Some(outcome),
                Err(reason) => {
                    refuse(world, cx.entity, traversal, reason);
                    None
                }
            }
        }
        TraversalOrder::Jump { to, arc_height } => {
            if !to.is_finite() || !at_least(arc_height, 0.0) {
                refuse(world, cx.entity, traversal, TraversalRefusal::TraversalBadRequest);
                return None;
            }
            Some(curve::start_jump(
                world, cx, traversal, state, transform, to, arc_height,
            ))
        }
    }
}

/// Tries a ledge when a jump asks for one, or by itself in the air. Refusals of a jump are reported;
/// the jump still jumps. `None`: no ledge move.
pub(crate) fn try_ledge(
    world: &mut World,
    cx: &StepCx<'_>,
    traversal: &mut CharacterTraversal,
    state: &mut CharacterState,
    transform: &Transform,
    in_air: bool,
) -> Option<Outcome> {
    if !cx.mantle.enabled {
        return None;
    }
    let forward = intent_of(cx.input)?;
    let asked = cx.input.jump;
    // A cooldown (a jump off a ladder) holds off the automatic grab as well: pushing into the wall
    // while leaving it must not pull the character straight back.
    if !asked && !(in_air && cx.mantle.auto_in_air && traversal.cooldown_steps == 0) {
        return None;
    }
    match ledge::probe_for_step(world, cx, transform.translation, forward) {
        Ok(probe) => Some(ledge::start(world, cx, traversal, state, transform, &probe)),
        Err(TraversalRefusal::LedgeNoWall) => None,
        Err(reason) => {
            if asked {
                refuse(world, cx.entity, traversal, reason);
            }
            None
        }
    }
}

/// What a capsule could use now (`physics.character_traversal_probe`).
#[derive(Debug, Clone, PartialEq)]
pub struct TraversalProbe {
    /// A ledge ahead, or why not.
    pub ledge: Result<LedgeProbe, TraversalRefusal>,
    /// A ladder in reach, or why not.
    pub ladder: Result<LadderProbe, TraversalRefusal>,
    /// The water surface over the position, if any.
    pub water_surface: Option<f32>,
    /// Whether the chest is at or under that surface.
    pub chest_under: bool,
}

/// Answers what `entity`'s capsule could use at `position` looking along `forward` (both default to
/// the character's own), without changing anything. `None`: not a character with
/// [`TraversalSettings`].
pub fn probe_traversal(
    world: &mut World,
    entity: Entity,
    position: Option<Vec3>,
    forward: Option<Vec3>,
) -> Option<TraversalProbe> {
    let settings = world.get::<TraversalSettings>(entity)?.clone();
    let movement = world.get::<CharacterMovement>(entity)?.sanitized();
    let transform = world.get::<Transform>(entity).copied()?;
    let at = position.unwrap_or(transform.translation);
    let look = forward.unwrap_or(transform.rotation * Vec3::NEG_Z);
    let skin = skin_of(world, entity);
    let ledge = probe_ledge(world, entity, &movement, &settings.mantle, at, look, skin);
    let ladder = probe_ladder(world, entity, &movement, &settings.ladder, at, skin);
    let water_surface = water_surface(world, at.x, at.z);
    let chest_under = water_surface.is_some_and(|s| at.y + settings.swim.chest_offset <= s);
    Some(TraversalProbe {
        ledge,
        ladder,
        water_surface,
        chest_under,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facing_uses_the_character_steps_deterministic_yaw() {
        for i in 0..64 {
            let angle = i as f32 * 0.1 - 3.2;
            let into = Vec3::new(
                regia_core::math::det_sin(angle),
                0.0,
                regia_core::math::det_cos(angle),
            );
            let expected = crate::character::yaw_rotation(regia_core::math::det_atan2(-into.x, -into.z));
            let got = facing_along(into);
            assert_eq!(
                [got.x, got.y, got.z, got.w].map(f32::to_bits),
                [expected.x, expected.y, expected.z, expected.w].map(f32::to_bits),
                "{into:?}"
            );
        }
    }

    #[test]
    fn an_order_in_the_current_frame_is_not_shifted_by_the_origin() {
        // A new character (frame [0; 3]) under a rebased origin: the order's point is already in
        // the current local frame and must reach the step unchanged.
        let origin = [1024.0, 0.0, -512.0];
        let to = Vec3::new(2.0, 0.5, 1.0);
        let order = TraversalOrder::Jump { to, arc_height: 1.0 };
        let mut t = CharacterTraversal::default();
        t.request_in(order, origin);
        t.reframe(origin);
        assert_eq!(t.pending, Some(order));
        // The message path (a nav crossing) does the same.
        let mut world = World::new();
        world.insert_resource(regia_scene::streaming::OriginOffset(origin));
        world.init_resource::<Messages<TraversalRequest>>();
        let c = world.spawn(CharacterTraversal::default()).id();
        world.write_message(TraversalRequest {
            agent: None,
            entity: Some(c),
            kind: regia_nav::NavLinkKind::Jump,
            user_id: 0,
            start: Vec3::ZERO,
            end: to,
            arc_height: 1.0,
        });
        let mut schedule = Schedule::default();
        schedule.add_systems(apply_traversal_requests);
        schedule.run(&mut world);
        let mut t = *world.get::<CharacterTraversal>(c).expect("traversal");
        t.reframe(origin);
        assert_eq!(t.pending, Some(order));
    }

    #[test]
    fn defaults_validate_and_bad_numbers_name_their_field() {
        assert_eq!(TraversalSettings::default().validate(), Ok(()));
        assert_eq!(Ladder::default().validate(), Ok(()));
        assert_eq!(TraversalJumpPoint::default().validate(), Ok(()));
        let mut settings = TraversalSettings::default();
        settings.swim.speed = f32::NAN;
        let error = settings.validate().expect_err("NaN");
        assert_eq!(
            (error.code(), error.field),
            ("traversal_bad_settings", "swim.speed")
        );
        let mut settings = TraversalSettings::default();
        settings.mantle.max_height = 0.1;
        let error = settings.validate().expect_err("range");
        assert_eq!(error.field, "mantle.max_height");
        let error = Ladder {
            height: f32::INFINITY,
            ..Ladder::default()
        }
        .validate()
        .expect_err("inf");
        assert_eq!((error.code(), error.field), ("ladder_bad_params", "height"));
        let error = TraversalJumpPoint {
            to: Vec3::new(f32::NAN, 0.0, 0.0),
            ..TraversalJumpPoint::default()
        }
        .validate()
        .expect_err("nan");
        assert_eq!((error.code(), error.field), ("jump_point_bad_params", "to"));
    }

    #[test]
    fn refusal_codes_are_protocol_text_and_distinct() {
        let mut seen = std::collections::BTreeSet::new();
        for refusal in TraversalRefusal::ALL {
            let code = refusal.code();
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
            assert!(seen.insert(code), "{code} twice");
        }
    }

    #[test]
    fn a_newer_saved_version_is_refused() {
        let mut settings = TraversalSettings {
            format_version: TRAVERSAL_SETTINGS_FORMAT_VERSION + 1,
            ..TraversalSettings::default()
        };
        assert!(settings.migrate().is_err());
        let mut ladder = Ladder {
            format_version: TRAVERSAL_LADDER_FORMAT_VERSION + 1,
            ..Ladder::default()
        };
        assert!(ladder.migrate().is_err());
        let mut point = TraversalJumpPoint {
            format_version: TRAVERSAL_JUMP_POINT_FORMAT_VERSION + 1,
            ..TraversalJumpPoint::default()
        };
        assert!(point.migrate().is_err());
    }
}
