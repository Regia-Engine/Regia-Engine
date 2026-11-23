// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/fog.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (plan row S-16, ADR-8154). Lines 1-215 of the source. What changed and why:
// - The sight table is data (`SightTable`), not a const table naming game content; the four default
//   radii are kept as the defaults (`DEFAULT_*_SIGHT_M`), and kind overrides are rows a game adds.
// - The game state type is a seam: `FogWorld` (entities as `FogEntity` rows, the alliance query)
//   replaces the game's own state struct; "dead or zero health" is the row's `alive` flag; the
//   on-foot rule (a pilot flag or the pilot kind) is the row's `SightClass::OnFoot`.
// - The four-team cap is a setting (`FogRules::team_count`); team 0 stays the neutral/observer team.
// - A line-of-sight hook (`crate::los::LineOfSight`) gates each watcher; `OpenGround` reproduces the
//   source's "open ground" behavior exactly.
// - `TeamVision` gathers a viewer's watchers once, so a whole snapshot filters in O(watchers x
//   entities) without re-reading the sight table per pair; it answers the same as the free functions.
// - The presentation grid's cell size and fallback half extent are parameters; `stamp` visits only
//   the cells inside the radius's bounding box (the same cells as the full scan, tested).
// The radius test (`dx * dx + dz * dz <= r * r` on XZ) and every visibility rule are the source's.
// Fog v2 (ADR-8473) is Regia's own code, not from the source: the compiled kind index, the indexed
// `TeamVision`, the visible layer and `CellState`, `FogMemory` ghosts, stealth and detectors, eye heights
// and the elevation rule. With their defaults off, every answer is the source's.

//! Fog of war: team visibility as a pure function of sim state (plan row S-16, ADR-8154).
//!
//! Each team sees what its own and allied watchers (units, buildings, structures with a long sight
//! radius) can see. Sight is a radius per entity on the ground plane (XZ), from a [`SightTable`]:
//! a default per [`SightClass`] plus per-kind overrides. A [`LineOfSight`] hook can block a watcher's
//! view; [`OpenGround`] blocks nothing.
//!
//! Everything here is a pure function of the state a [`FogWorld`] reports for one viewing team.
//! Visibility is meant for presentation and for per-viewer filtering (what a client is sent or
//! shown): gameplay that must stay in lockstep keeps reading the full state, so peers never diverge
//! because two players see different things. The answers are still deterministic (IEEE `+ - *` in
//! a fixed order, no transcendentals, no hash iteration), so a server may also use them to decide
//! what each client receives.
//!
//! The rules, for viewer team `v` and an entity of team `t`:
//!
//! - fog off ([`FogRules::enabled`] false) shows everything;
//! - an observer viewer (team 0 or above [`FogRules::team_count`]) sees everything;
//! - neutral entities (team 0 or above the count), own and allied entities, dead entities and map
//!   features ([`SightClass::Feature`]) always show;
//! - an enemy unit or building shows while it is inside the sight radius of a living own or allied
//!   watcher whose line of sight to it is clear.
//! - a stealthed enemy ([`KindSight::stealth`]) shows only while it is inside the detection radius of
//!   a living own or allied detector whose line of sight to it is clear.
//!
//! Optional rules (all off by default, which keeps the plain radius test): per-class eye heights for
//! the line-of-sight hook, the [`ElevationRule`], and remembered kinds whose last sighting a
//! [`FogMemory`] keeps as a ghost. A [`FogGrid`] holds a team's explored and visible-now cells
//! ([`CellState`]). [`TeamVision`] answers from a spatial index once a team has many watchers.
//!
//! ```
//! use regia_detsim::fog::{FogEntity, FogRules, FogWorld, SightClass, entity_visible_to_team};
//! use regia_detsim::los::OpenGround;
//!
//! /// Rows of a toy sim: (id, team, x).
//! struct Field(Vec<(u64, u32, f32)>);
//! impl FogWorld for Field {
//!     fn fog_entities(&self) -> impl Iterator<Item = FogEntity<'_>> {
//!         self.0.iter().map(|&(id, team, x)| FogEntity::new(id, "scout", SightClass::Unit, team, [x, 0.0, 0.0]))
//!     }
//! }
//!
//! // Team 1's scout at the origin; two team 2 scouts, one near and one far.
//! let world = Field(vec![(1, 1, 0.0), (2, 2, 100.0), (3, 2, 900.0)]);
//! let rules = FogRules::default();
//! let near = world.fog_entities().find(|e| e.id == 2).unwrap();
//! let far = world.fog_entities().find(|e| e.id == 3).unwrap();
//!
//! assert!(entity_visible_to_team(&world, &rules, 1, &near, &OpenGround));
//! assert!(!entity_visible_to_team(&world, &rules, 1, &far, &OpenGround));
//! ```

pub use crate::los::{LineOfSight, OpenGround};

use crate::broadphase::{BroadBody, BroadphaseConfig, FloorRule};
use crate::float::det_max;
use crate::spatial::{DetSpatial, SpatialRow};

/// The ground height `(x, z) -> y` the elevation rule reads (KI-GP-018).
type GroundHeight<'a> = &'a dyn Fn(f32, f32) -> f32;

/// A team index. Team 0 is neutral (map features, wildlife) and, as a viewer, an observer.
pub type TeamId = u32;

/// Default sight radius in metres (XZ) of an ordinary combat unit.
pub const DEFAULT_UNIT_SIGHT_M: f32 = 150.0;
/// Default sight radius in metres (XZ) of an ordinary building.
pub const DEFAULT_BUILDING_SIGHT_M: f32 = 100.0;
/// Default sight radius in metres (XZ) of a unit on foot (a crew member out of its vehicle).
pub const DEFAULT_ON_FOOT_SIGHT_M: f32 = 60.0;
/// Default number of player teams: teams `1..=4` play, team 0 and teams above it are neutral.
pub const DEFAULT_TEAM_COUNT: TeamId = 4;

/// What an entity is, for fog: it picks the default sight radius and whether it can be hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SightClass {
    /// A combat unit: watches with [`SightTable::unit_m`], hidden from enemies out of sight.
    Unit,
    /// A unit on foot: watches with [`SightTable::on_foot_m`], hidden like a unit.
    OnFoot,
    /// A building: watches with [`SightTable::building_m`], hidden like a unit.
    Building,
    /// A map feature (an objective, a geyser, an effect emitter): never watches, always shown.
    Feature,
}

/// One kind's sight override: entities whose kind key equals `kind` watch with `radius_m`, whatever
/// their class.
///
/// The row also carries the kind's detection and memory traits (all off by default, which keeps the
/// plain radius rules): a stealthed kind shows to enemies only inside a detector's radius, a
/// detector kind reveals stealthed enemies within `detector_m`, and a remembered kind (a structure)
/// leaves a last-seen ghost in [`FogMemory`] when it drops out of sight.
#[derive(Debug, Clone, PartialEq)]
pub struct KindSight {
    /// The kind key the sim reports (the same key as the sim bridge's snapshot kind).
    pub kind: String,
    /// Sight radius in metres on the ground plane. `0.0` makes the kind a non-watcher.
    pub radius_m: f32,
    /// Stealthed: an enemy of this kind shows only inside the radius of a living own or allied
    /// detector with a clear line of sight (and is otherwise hidden like any unseen enemy).
    pub stealth: bool,
    /// Detection radius in metres (XZ); `0.0` (the default) detects nothing.
    pub detector_m: f32,
    /// Remembered: when an enemy of this kind drops out of sight, [`FogMemory`] keeps a ghost of
    /// where it was last seen until a watcher sees that spot without it.
    pub remember: bool,
}

impl KindSight {
    /// A kind row with radius `radius_m` and no detection or memory traits.
    pub fn new(kind: impl Into<String>, radius_m: f32) -> Self {
        Self {
            kind: kind.into(),
            radius_m,
            stealth: false,
            detector_m: 0.0,
            remember: false,
        }
    }

    /// Marks the kind stealthed.
    pub fn stealthed(mut self) -> Self {
        self.stealth = true;
        self
    }

    /// Makes the kind a detector with radius `detector_m`.
    pub fn detector(mut self, detector_m: f32) -> Self {
        self.detector_m = detector_m;
        self
    }

    /// Marks the kind remembered (it leaves a last-seen ghost).
    pub fn remembered(mut self) -> Self {
        self.remember = true;
        self
    }
}

/// Sight radii as data: a default per [`SightClass`] and per-kind overrides (a radar mast watches
/// far, a decoy not at all).
#[derive(Debug, Clone, PartialEq)]
pub struct SightTable {
    /// Per-kind overrides, checked first; the first row whose kind matches wins.
    pub kinds: Vec<KindSight>,
    /// Radius of a [`SightClass::Unit`] with no override.
    pub unit_m: f32,
    /// Radius of a [`SightClass::OnFoot`] with no override.
    pub on_foot_m: f32,
    /// Radius of a [`SightClass::Building`] with no override.
    pub building_m: f32,
    /// Eye height in metres of a [`SightClass::Unit`] watcher: added to the watcher's `y` before
    /// the line-of-sight hook is asked (a terrain hook such as the det grid's sees over low
    /// ground). `0.0`, the default, passes the position unchanged.
    pub unit_eye_m: f32,
    /// Eye height in metres of a [`SightClass::OnFoot`] watcher (see [`SightTable::unit_eye_m`]).
    pub on_foot_eye_m: f32,
    /// Eye height in metres of a [`SightClass::Building`] watcher (see [`SightTable::unit_eye_m`]).
    pub building_eye_m: f32,
}

impl Default for SightTable {
    fn default() -> Self {
        Self {
            kinds: Vec::new(),
            unit_m: DEFAULT_UNIT_SIGHT_M,
            on_foot_m: DEFAULT_ON_FOOT_SIGHT_M,
            building_m: DEFAULT_BUILDING_SIGHT_M,
            unit_eye_m: 0.0,
            on_foot_eye_m: 0.0,
            building_eye_m: 0.0,
        }
    }
}

impl SightTable {
    /// Adds (or replaces) the override for `kind`. Replacing changes the radius only; the row's
    /// detection and memory traits stay.
    pub fn with_kind(mut self, kind: impl Into<String>, radius_m: f32) -> Self {
        let kind = kind.into();
        match self.kinds.iter_mut().find(|row| row.kind == kind) {
            Some(row) => row.radius_m = radius_m,
            None => self.kinds.push(KindSight::new(kind, radius_m)),
        }
        self
    }

    /// Adds (or replaces, every field) the row for `row.kind`.
    pub fn with_row(mut self, row: KindSight) -> Self {
        match self.kinds.iter_mut().find(|old| old.kind == row.kind) {
            Some(old) => *old = row,
            None => self.kinds.push(row),
        }
        self
    }

    /// The first row whose kind equals `kind`.
    pub fn kind_row(&self, kind: &str) -> Option<&KindSight> {
        self.kinds.iter().find(|row| row.kind == kind)
    }

    /// The default radius of `class` (no kind row).
    pub fn class_radius_m(&self, class: SightClass) -> f32 {
        match class {
            SightClass::Unit => self.unit_m,
            SightClass::OnFoot => self.on_foot_m,
            SightClass::Building => self.building_m,
            SightClass::Feature => 0.0,
        }
    }

    /// The eye height of `class` (features have none).
    pub fn eye_m(&self, class: SightClass) -> f32 {
        match class {
            SightClass::Unit => self.unit_eye_m,
            SightClass::OnFoot => self.on_foot_eye_m,
            SightClass::Building => self.building_eye_m,
            SightClass::Feature => 0.0,
        }
    }

    /// Sight radius in metres of an entity of `kind` and `class`. Map features never watch (0.0)
    /// unless a kind override says otherwise.
    pub fn radius_m(&self, kind: &str, class: SightClass) -> f32 {
        if let Some(row) = self.kind_row(kind) {
            return row.radius_m;
        }
        self.class_radius_m(class)
    }

    /// The first radius or height that is negative, infinite or NaN, as `(what, value)`.
    pub fn first_invalid(&self) -> Option<(String, f32)> {
        let ok = |r: f32| r.is_finite() && r >= 0.0;
        for (what, r) in [
            ("unit_m", self.unit_m),
            ("on_foot_m", self.on_foot_m),
            ("building_m", self.building_m),
            ("unit_eye_m", self.unit_eye_m),
            ("on_foot_eye_m", self.on_foot_eye_m),
            ("building_eye_m", self.building_eye_m),
        ] {
            if !ok(r) {
                return Some((what.to_string(), r));
            }
        }
        for row in &self.kinds {
            if !ok(row.radius_m) {
                return Some((format!("kinds[{}]", row.kind), row.radius_m));
            }
            if !ok(row.detector_m) {
                return Some((format!("kinds[{}].detector_m", row.kind), row.detector_m));
            }
        }
        None
    }

    /// Resolves the kind strings once: a [`CompiledSight`] answers by [`KindId`] (an index) with no
    /// string compare. The first row of a repeated kind wins, as in [`SightTable::radius_m`].
    pub fn compile(&self) -> CompiledSight {
        let mut rows = Vec::with_capacity(self.kinds.len());
        let mut by_name: Vec<(String, usize)> = Vec::with_capacity(self.kinds.len());
        for row in &self.kinds {
            let index = rows.len();
            rows.push(row.clone());
            by_name.push((row.kind.clone(), index));
        }
        // Sort by name, then index, and keep each name's first row.
        by_name.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        by_name.dedup_by(|later, first| later.0 == first.0);
        CompiledSight {
            any_stealth: rows.iter().any(|row| row.stealth),
            any_detector: rows.iter().any(|row| row.detector_m > 0.0),
            any_remember: rows.iter().any(|row| row.remember),
            rows,
            by_name,
            class_radius: [self.unit_m, self.on_foot_m, self.building_m, 0.0],
            class_eye: [self.unit_eye_m, self.on_foot_eye_m, self.building_eye_m, 0.0],
        }
    }
}

/// A kind's index in a [`CompiledSight`] (the position of its first row in [`SightTable::kinds`]).
pub type KindId = u16;

/// A [`SightTable`] with its kind strings resolved to [`KindId`]s (see [`SightTable::compile`]).
///
/// An entity that carries its kind id ([`FogEntity::kind_id`]) is answered by index; one without
/// falls back to a binary search over the kind names (the same answer as the table's linear
/// lookup).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompiledSight {
    rows: Vec<KindSight>,
    /// `(name, row index)`, sorted by name, the first row of each name only.
    by_name: Vec<(String, usize)>,
    class_radius: [f32; 4],
    class_eye: [f32; 4],
    any_stealth: bool,
    any_detector: bool,
    any_remember: bool,
}

fn class_slot(class: SightClass) -> usize {
    match class {
        SightClass::Unit => 0,
        SightClass::OnFoot => 1,
        SightClass::Building => 2,
        SightClass::Feature => 3,
    }
}

impl CompiledSight {
    /// The id of `kind`, if the table has a row for it (and it fits a [`KindId`]).
    pub fn kind_id(&self, kind: &str) -> Option<KindId> {
        self.row_index(kind).and_then(|i| KindId::try_from(i).ok())
    }

    fn row_index(&self, kind: &str) -> Option<usize> {
        self.by_name
            .binary_search_by(|(name, _)| name.as_str().cmp(kind))
            .ok()
            .map(|at| self.by_name[at].1)
    }

    /// The row of `entity`: by its kind id when it carries a valid one, else by its kind string.
    pub fn row_of(&self, entity: &FogEntity<'_>) -> Option<&KindSight> {
        if let Some(row) = entity.kind_id.and_then(|id| self.rows.get(usize::from(id))) {
            return Some(row);
        }
        if self.rows.is_empty() {
            return None;
        }
        self.row_index(entity.kind).map(|i| &self.rows[i])
    }

    /// The row with id `id`.
    pub fn row(&self, id: KindId) -> Option<&KindSight> {
        self.rows.get(usize::from(id))
    }

    /// Number of kind rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when the table has no kind rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Sight radius of `entity` (the same value as [`SightTable::radius_m`] for its kind and class).
    pub fn radius_m(&self, entity: &FogEntity<'_>) -> f32 {
        match self.row_of(entity) {
            Some(row) => row.radius_m,
            None => self.class_radius[class_slot(entity.class)],
        }
    }

    /// Eye height of `class`.
    pub fn eye_m(&self, class: SightClass) -> f32 {
        self.class_eye[class_slot(class)]
    }

    /// True when some row is stealthed.
    pub fn any_stealth(&self) -> bool {
        self.any_stealth
    }

    /// True when some row is remembered.
    pub fn any_remember(&self) -> bool {
        self.any_remember
    }
}

/// The elevation-advantage rule: a watcher looking up at higher ground sees less far.
///
/// When the target stands more than `step_m` above the watcher (`target.y - watcher.y > step_m`,
/// both the positions the [`FogWorld`] reports, before any eye height), the watcher's radius for
/// that target is `radius * low_ground_scale` (the scale is clamped to `0..=1`, NaN reads as 0).
/// `low_ground_scale` 0 is the classic rule "low ground never sees high ground"; 1 turns the rule
/// off. Looking down or across is unchanged. The rule is a radius test only; terrain occlusion is
/// the [`LineOfSight`] hook's job.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ElevationRule {
    /// Height difference in metres above which the target counts as higher ground.
    pub step_m: f32,
    /// Radius scale toward higher ground, `0..=1`.
    pub low_ground_scale: f32,
}

impl ElevationRule {
    /// The rule with its scale clamped to `0..=1` (NaN to 0).
    fn sanitized(self) -> Self {
        let scale = self.low_ground_scale;
        let scale = if scale >= 0.0 {
            if scale > 1.0 { 1.0 } else { scale }
        } else {
            0.0
        };
        Self {
            step_m: self.step_m,
            low_ground_scale: scale,
        }
    }
}

/// The fog settings of one match: on or off, how many player teams there are, and who sees how far.
#[derive(Debug, Clone, PartialEq)]
pub struct FogRules {
    /// Fog on (the default). Off shows every entity to every viewer.
    pub enabled: bool,
    /// Player teams are `1..=team_count`; team 0 and higher teams are neutral (always shown) and,
    /// as viewers, observers (see everything).
    pub team_count: TeamId,
    /// Sight radii.
    pub sight: SightTable,
    /// The elevation-advantage rule; `None` (the default) is flat sight.
    pub elevation: Option<ElevationRule>,
}

impl Default for FogRules {
    fn default() -> Self {
        Self {
            enabled: true,
            team_count: DEFAULT_TEAM_COUNT,
            sight: SightTable::default(),
            elevation: None,
        }
    }
}

impl FogRules {
    /// True when `team` is a player team (`1..=team_count`).
    #[inline]
    pub fn is_player_team(&self, team: TeamId) -> bool {
        team != 0 && team <= self.team_count
    }
}

/// One entity as fog sees it. A [`FogWorld`] yields these; positions are in the same frame as the
/// rest of the caller's queries (fog measures distance on X and Z).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FogEntity<'a> {
    /// The sim's stable id (the same id as the sim bridge's snapshot rows).
    pub id: u64,
    /// The kind key, matched against [`SightTable::kinds`].
    pub kind: &'a str,
    /// The sight class.
    pub class: SightClass,
    /// The owning team.
    pub team: TeamId,
    /// World position in metres.
    pub position: [f32; 3],
    /// False for a destroyed entity (a wreck, zero health): it watches nothing and is always shown.
    pub alive: bool,
    /// The kind's id in the [`CompiledSight`] of the rules in force ([`CompiledSight::kind_id`]),
    /// when the host stores it: the kind is then looked up by index. `None` (the default) looks the
    /// kind string up. A host that sets it must recompute it whenever it changes the sight table.
    pub kind_id: Option<KindId>,
}

impl<'a> FogEntity<'a> {
    /// A living entity.
    pub fn new(id: u64, kind: &'a str, class: SightClass, team: TeamId, position: [f32; 3]) -> Self {
        Self {
            id,
            kind,
            class,
            team,
            position,
            alive: true,
            kind_id: None,
        }
    }

    /// The same entity carrying its compiled kind id.
    pub fn with_kind_id(mut self, kind_id: Option<KindId>) -> Self {
        self.kind_id = kind_id;
        self
    }
}

/// The state fog reads: the entities and the alliances. A game's sim state (or a snapshot of it)
/// implements this; the functions in this module never mutate it.
pub trait FogWorld {
    /// Every entity that can watch or be hidden, in a stable order.
    fn fog_entities(&self) -> impl Iterator<Item = FogEntity<'_>>;

    /// True when teams `a` and `b` are allied (share sight). Only asked for two different player
    /// teams. The default is no alliances.
    fn is_allied(&self, _a: TeamId, _b: TeamId) -> bool {
        false
    }
}

#[inline]
fn dist_sq_xz(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    dx * dx + dz * dz
}

/// True when `team` shares `viewer`'s sight: the same team, or an ally.
fn shares_sight<W: FogWorld + ?Sized>(world: &W, viewer: TeamId, team: TeamId) -> bool {
    team == viewer || world.is_allied(viewer, team)
}

/// A living watcher of `viewer`: (position, sight radius). The dead, neutral teams, other teams'
/// eyes and entities with no sight watch nothing.
fn watcher_of<W: FogWorld + ?Sized>(
    world: &W,
    rules: &FogRules,
    viewer: TeamId,
    entity: &FogEntity<'_>,
) -> Option<([f32; 3], f32)> {
    if !entity.alive {
        return None;
    }
    if !rules.is_player_team(entity.team) {
        return None;
    }
    if !shares_sight(world, viewer, entity.team) {
        return None;
    }
    let radius = rules.sight.radius_m(entity.kind, entity.class);
    if radius <= 0.0 {
        return None;
    }
    Some((entity.position, radius))
}

/// One gathered watcher: where it stands, where its eye is, and how far it sees.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Watcher {
    at: [f32; 3],
    eye: [f32; 3],
    radius: f32,
    radius_sq: f32,
}

/// One gathered detector: its eye and its detection radius squared.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Detector {
    eye: [f32; 3],
    radius_sq: f32,
}

/// The eye of a watcher at `at`: `at` itself when the eye height is zero (bit for bit, so a hook
/// sees the position the world reported), else raised by `eye_m`.
#[inline]
fn eye_of(at: [f32; 3], eye_m: f32) -> [f32; 3] {
    if eye_m == 0.0 {
        at
    } else {
        [at[0], at[1] + eye_m, at[2]]
    }
}

/// The exact sight test of one watcher: the radius (with the elevation rule), then the hook.
#[inline]
fn watcher_sees<L: LineOfSight + ?Sized>(
    w: &Watcher,
    pos: [f32; 3],
    elevation: Option<ElevationRule>,
    los: &L,
    counters: &mut FogCounters,
) -> bool {
    let d = dist_sq_xz(w.at, pos);
    let in_range = match elevation {
        Some(rule) if pos[1] - w.at[1] > rule.step_m => {
            let r = w.radius * rule.low_ground_scale;
            d <= r * r
        }
        _ => d <= w.radius_sq,
    };
    if !in_range {
        return false;
    }
    counters.los_calls += 1;
    los.clear(w.eye, pos)
}

/// The exact detection test of one detector.
#[inline]
fn detector_detects<L: LineOfSight + ?Sized>(
    d: &Detector,
    pos: [f32; 3],
    los: &L,
    counters: &mut FogCounters,
) -> bool {
    let in_range = dist_sq_xz(d.eye, pos) <= d.radius_sq;
    if !in_range {
        return false;
    }
    counters.los_calls += 1;
    los.clear(d.eye, pos)
}

/// `watcher_of` plus the eye height, for the linear (reference) path.
fn linear_watcher<W: FogWorld + ?Sized>(
    world: &W,
    rules: &FogRules,
    viewer: TeamId,
    entity: &FogEntity<'_>,
) -> Option<Watcher> {
    let (at, radius) = watcher_of(world, rules, viewer, entity)?;
    Some(Watcher {
        at,
        eye: eye_of(at, rules.sight.eye_m(entity.class)),
        radius,
        radius_sq: radius * radius,
    })
}

/// A living own or allied detector of `viewer`, for the linear (reference) path.
fn linear_detector<W: FogWorld + ?Sized>(
    world: &W,
    rules: &FogRules,
    viewer: TeamId,
    entity: &FogEntity<'_>,
) -> Option<Detector> {
    if !entity.alive || !rules.is_player_team(entity.team) || !shares_sight(world, viewer, entity.team) {
        return None;
    }
    let row = rules.sight.kind_row(entity.kind)?;
    let detects = row.detector_m > 0.0;
    if !detects {
        return None;
    }
    Some(Detector {
        eye: eye_of(entity.position, rules.sight.eye_m(entity.class)),
        radius_sq: row.detector_m * row.detector_m,
    })
}

/// True when any watcher of `viewer` (own or allied units, buildings and long-sight structures)
/// sees `pos` with a clear line of sight. Observer viewers (team 0 or above the team count) see
/// everything. Ignores [`FogRules::enabled`] (the question is "is this point in sight").
///
/// This walks every entity (the reference answer); [`TeamVision::sees_point`] gives the same
/// answer from a spatial index.
pub fn team_sees_point<W: FogWorld + ?Sized, L: LineOfSight + ?Sized>(
    world: &W,
    rules: &FogRules,
    viewer: TeamId,
    pos: [f32; 3],
    los: &L,
) -> bool {
    if !rules.is_player_team(viewer) {
        return true;
    }
    let elevation = rules.elevation.map(ElevationRule::sanitized);
    let mut counters = FogCounters::default();
    world
        .fog_entities()
        .filter_map(|entity| linear_watcher(world, rules, viewer, &entity))
        .any(|w| watcher_sees(&w, pos, elevation, los, &mut counters))
}

/// True when a living own or allied detector of `viewer` ([`KindSight::detector_m`]) has `pos`
/// inside its detection radius with a clear line of sight. Observers detect everything.
pub fn team_detects_point<W: FogWorld + ?Sized, L: LineOfSight + ?Sized>(
    world: &W,
    rules: &FogRules,
    viewer: TeamId,
    pos: [f32; 3],
    los: &L,
) -> bool {
    if !rules.is_player_team(viewer) {
        return true;
    }
    let mut counters = FogCounters::default();
    world
        .fog_entities()
        .filter_map(|entity| linear_detector(world, rules, viewer, &entity))
        .any(|d| detector_detects(&d, pos, los, &mut counters))
}

/// The always-shown cases of [`entity_visible_to_team`]; `None` when the answer depends on sight.
fn always_shown<W: FogWorld + ?Sized>(
    world: &W,
    rules: &FogRules,
    viewer: TeamId,
    entity: &FogEntity<'_>,
) -> Option<bool> {
    if !rules.enabled || !rules.is_player_team(viewer) {
        return Some(true);
    }
    if !rules.is_player_team(entity.team) || shares_sight(world, viewer, entity.team) {
        return Some(true);
    }
    if !entity.alive {
        return Some(true);
    }
    match entity.class {
        SightClass::Feature => Some(true),
        SightClass::Unit | SightClass::OnFoot | SightClass::Building => None,
    }
}

/// True when `entity` shows in `viewer`'s views. Fog off shows all. Neutral teams, the dead and map
/// features always show. Own and allied entities always show; enemies show while inside friendly
/// sight with a clear line of sight, and a stealthed enemy ([`KindSight::stealth`]) only while a
/// friendly detector detects it ([`team_detects_point`]).
pub fn entity_visible_to_team<W: FogWorld + ?Sized, L: LineOfSight + ?Sized>(
    world: &W,
    rules: &FogRules,
    viewer: TeamId,
    entity: &FogEntity<'_>,
    los: &L,
) -> bool {
    match always_shown(world, rules, viewer, entity) {
        Some(shown) => shown,
        None => {
            if rules.sight.kind_row(entity.kind).is_some_and(|row| row.stealth) {
                team_detects_point(world, rules, viewer, entity.position, los)
            } else {
                team_sees_point(world, rules, viewer, entity.position, los)
            }
        }
    }
}

/// Work counters of fog queries: how many watchers were gathered, how many index candidates and
/// line-of-sight calls the queries cost. No wall clock; a host sums them per frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FogCounters {
    /// Living own or allied watchers gathered.
    pub watchers: u64,
    /// Living own or allied detectors gathered.
    pub detectors: u64,
    /// Point and entity sight queries answered from sight (not the always-shown cases).
    pub queries: u64,
    /// Watchers or detectors a query tested exactly (index candidates, or every watcher on the
    /// linear path).
    pub index_candidates: u64,
    /// Line-of-sight hook calls.
    pub los_calls: u64,
}

impl FogCounters {
    /// Adds `other` to these counters (saturating).
    pub fn add(&mut self, other: &FogCounters) {
        self.watchers = self.watchers.saturating_add(other.watchers);
        self.detectors = self.detectors.saturating_add(other.detectors);
        self.queries = self.queries.saturating_add(other.queries);
        self.index_candidates = self.index_candidates.saturating_add(other.index_candidates);
        self.los_calls = self.los_calls.saturating_add(other.los_calls);
    }
}

/// Watchers below which [`TeamVision`] tests every watcher instead of building its spatial index
/// (the answers are the same; the index only pays off with many watchers).
pub const DEFAULT_INDEX_MIN_WATCHERS: usize = 16;

/// Most index cells (the index raises its cell size to stay under it).
const VISION_INDEX_MAX_CELLS: usize = 1 << 12;

/// One viewer's sight, gathered once: its watchers, detectors and allies. Answers the same as
/// [`team_sees_point`] and [`entity_visible_to_team`] for the world it was built from, without
/// re-reading the sight table or the alliances per query. Reuse one value across frames
/// ([`TeamVision::rebuild`]) to filter a snapshot with no allocation once it has warmed up.
///
/// With [`DEFAULT_INDEX_MIN_WATCHERS`] or more watchers, the watchers are filed once per rebuild
/// into a [`DetSpatial`] uniform grid (each row's disc is its sight radius) and a point query tests
/// only the watchers whose disc can hold it, stopping at the first that sees: the same accepted set
/// as the linear scan, in about O(entities x nearby watchers) per frame instead of O(entities x
/// watchers). The sight table is compiled ([`SightTable::compile`]) only when it changes.
#[derive(Debug, Clone)]
pub struct TeamVision {
    viewer: TeamId,
    observer: bool,
    enabled: bool,
    team_count: TeamId,
    /// `shares[t]`: team `t` (a player team) is the viewer or allied to it.
    shares: Vec<bool>,
    /// Every living own or allied watcher, in world order.
    watchers: Vec<Watcher>,
    /// Every living own or allied detector, in world order.
    detectors: Vec<Detector>,
    elevation: Option<ElevationRule>,
    sight: CompiledSight,
    sight_source: Option<SightTable>,
    index: DetSpatial,
    index_rows: Vec<SpatialRow>,
    /// Watchers too far-sighted for the index (more than twice the median radius, or not finite):
    /// tested on every query.
    far: Vec<usize>,
    /// Scratch for the median radius.
    radii: Vec<f32>,
    index_cell_m: f32,
    indexed: bool,
    index_min_watchers: usize,
    gathered: FogCounters,
}

impl Default for TeamVision {
    fn default() -> Self {
        Self {
            viewer: 0,
            observer: false,
            enabled: false,
            team_count: 0,
            shares: Vec::new(),
            watchers: Vec::new(),
            detectors: Vec::new(),
            elevation: None,
            sight: CompiledSight::default(),
            sight_source: None,
            index: DetSpatial::default(),
            index_rows: Vec::new(),
            far: Vec::new(),
            radii: Vec::new(),
            index_cell_m: 0.0,
            indexed: false,
            index_min_watchers: DEFAULT_INDEX_MIN_WATCHERS,
            gathered: FogCounters::default(),
        }
    }
}

impl TeamVision {
    /// Gathers `viewer`'s sight over `world`.
    pub fn new<W: FogWorld + ?Sized>(world: &W, rules: &FogRules, viewer: TeamId) -> Self {
        let mut vision = Self::default();
        vision.rebuild(world, rules, viewer);
        vision
    }

    /// Sets how many watchers it takes to build the spatial index (0 always indexes,
    /// `usize::MAX` never does); the next rebuild uses it. The answers do not change.
    pub fn set_index_min_watchers(&mut self, watchers: usize) {
        self.index_min_watchers = watchers;
    }

    /// [`TeamVision::set_index_min_watchers`], by value.
    pub fn with_index_min_watchers(mut self, watchers: usize) -> Self {
        self.index_min_watchers = watchers;
        self
    }

    /// Gathers `viewer`'s sight again, reusing this value's buffers.
    pub fn rebuild<W: FogWorld + ?Sized>(&mut self, world: &W, rules: &FogRules, viewer: TeamId) {
        self.viewer = viewer;
        self.observer = !rules.is_player_team(viewer);
        self.enabled = rules.enabled;
        self.team_count = rules.team_count;
        self.elevation = rules.elevation.map(ElevationRule::sanitized);
        self.shares.clear();
        self.watchers.clear();
        self.detectors.clear();
        self.indexed = false;
        self.gathered = FogCounters::default();
        if self.sight_source.as_ref() != Some(&rules.sight) {
            self.sight = rules.sight.compile();
            self.sight_source = Some(rules.sight.clone());
        }
        if self.observer {
            return;
        }
        let teams = usize::try_from(rules.team_count).unwrap_or(usize::MAX);
        // One flag per player team; a huge team count is answered by asking the world each time.
        if teams <= 4096 {
            self.shares.reserve(teams + 1);
            self.shares.push(false);
            for team in 1..=rules.team_count {
                self.shares.push(shares_sight(world, viewer, team));
            }
        }
        for entity in world.fog_entities() {
            if !entity.alive || !rules.is_player_team(entity.team) || !self.shares_sight(world, entity.team) {
                continue;
            }
            let row = self.sight.row_of(&entity);
            let radius = match row {
                Some(row) => row.radius_m,
                None => self.sight.class_radius[class_slot(entity.class)],
            };
            let eye = eye_of(entity.position, self.sight.eye_m(entity.class));
            if radius > 0.0 {
                self.watchers.push(Watcher {
                    at: entity.position,
                    eye,
                    radius,
                    radius_sq: radius * radius,
                });
            }
            if let Some(row) = row
                && row.detector_m > 0.0
            {
                self.detectors.push(Detector {
                    eye,
                    radius_sq: row.detector_m * row.detector_m,
                });
            }
        }
        self.gathered.watchers = self.watchers.len() as u64;
        self.gathered.detectors = self.detectors.len() as u64;
        if self.watchers.len() >= self.index_min_watchers {
            self.build_index();
        }
    }

    /// Files the watchers into the spatial index (row disc = sight radius). A few long-sight
    /// watchers (radar masts) would widen every query to their radius, so watchers whose radius is
    /// more than twice the median (or not finite) stay on a short list that every query tests.
    fn build_index(&mut self) {
        self.radii.clear();
        self.radii
            .extend(self.watchers.iter().map(|w| w.radius).filter(|r| r.is_finite()));
        let cutoff = if self.radii.is_empty() {
            0.0
        } else {
            let mid = self.radii.len() / 2;
            let (_, median, _) = self.radii.select_nth_unstable_by(mid, f32::total_cmp);
            *median * 2.0
        };
        self.far.clear();
        let mut max_r = 0.0f32;
        for (slot, w) in self.watchers.iter().enumerate() {
            if w.radius.is_finite() && w.radius <= cutoff {
                max_r = det_max(max_r, w.radius);
            } else {
                self.far.push(slot);
            }
        }
        let cell = if max_r >= 1.0 { max_r } else { 1.0 };
        if self.index_cell_m != cell {
            self.index = DetSpatial::new(BroadphaseConfig {
                cell_size: cell,
                margin: 0.0,
                max_cells: VISION_INDEX_MAX_CELLS,
                floor_rule: FloorRule::Never,
                ..BroadphaseConfig::default()
            });
            self.index_cell_m = cell;
        }
        self.index_rows.clear();
        for (slot, w) in self.watchers.iter().enumerate() {
            // Far watchers keep their slot (the slots must be a permutation) but match no query.
            // Their disc is zero so they do not widen the index's query reach.
            let far = self.far.binary_search(&slot).is_ok();
            let (group_mask, radius) = if far { (0, 0.0) } else { (1, w.radius) };
            self.index_rows.push(SpatialRow {
                slot,
                body: BroadBody::new([w.at[0], 0.0, w.at[2]], radius, [0.0, 0.0]),
                group_mask,
                flags: 0,
            });
        }
        self.indexed = self.index.build(&self.index_rows).is_ok();
    }

    /// The viewing team.
    pub fn viewer(&self) -> TeamId {
        self.viewer
    }

    /// True when the viewer is an observer (sees everything).
    pub fn is_observer(&self) -> bool {
        self.observer
    }

    /// True when fog was on in the rules this vision was built from.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Number of living own or allied watchers.
    pub fn watcher_count(&self) -> usize {
        self.watchers.len()
    }

    /// Number of living own or allied detectors.
    pub fn detector_count(&self) -> usize {
        self.detectors.len()
    }

    /// True when the last rebuild filed the watchers into the spatial index.
    pub fn is_indexed(&self) -> bool {
        self.indexed
    }

    /// The compiled sight table of the rules this vision was built from.
    pub fn compiled_sight(&self) -> &CompiledSight {
        &self.sight
    }

    /// Counters of the last rebuild (watchers and detectors gathered).
    pub fn gathered(&self) -> FogCounters {
        self.gathered
    }

    fn shares_sight<W: FogWorld + ?Sized>(&self, world: &W, team: TeamId) -> bool {
        match usize::try_from(team).ok().and_then(|t| self.shares.get(t)) {
            Some(&shares) => shares,
            None => shares_sight(world, self.viewer, team),
        }
    }

    /// True when `team` is an enemy player team of the viewer (not neutral, not own, not allied).
    pub fn is_enemy<W: FogWorld + ?Sized>(&self, world: &W, team: TeamId) -> bool {
        team != 0 && team <= self.team_count && !self.shares_sight(world, team)
    }

    /// Same answer as [`team_sees_point`] for the world this vision was built from.
    pub fn sees_point<L: LineOfSight + ?Sized>(&self, pos: [f32; 3], los: &L) -> bool {
        self.sees_point_counted(pos, los, &mut FogCounters::default())
    }

    /// [`TeamVision::sees_point`], adding its work to `counters`.
    pub fn sees_point_counted<L: LineOfSight + ?Sized>(
        &self,
        pos: [f32; 3],
        los: &L,
        counters: &mut FogCounters,
    ) -> bool {
        if self.observer {
            return true;
        }
        counters.queries += 1;
        let elevation = self.elevation;
        let watchers = &self.watchers;
        if self.indexed {
            let center = BroadBody::new([pos[0], 0.0, pos[2]], 0.0, [0.0, 0.0]);
            let near = self.index.any_within(center, 0.0, 1, |slot| {
                counters.index_candidates += 1;
                watchers
                    .get(slot)
                    .is_some_and(|w| watcher_sees(w, pos, elevation, los, counters))
            });
            near || self.far.iter().any(|&slot| {
                counters.index_candidates += 1;
                watchers
                    .get(slot)
                    .is_some_and(|w| watcher_sees(w, pos, elevation, los, counters))
            })
        } else {
            watchers.iter().any(|w| {
                counters.index_candidates += 1;
                watcher_sees(w, pos, elevation, los, counters)
            })
        }
    }

    /// Same answer as [`team_detects_point`] for the world this vision was built from.
    pub fn detects_point_counted<L: LineOfSight + ?Sized>(
        &self,
        pos: [f32; 3],
        los: &L,
        counters: &mut FogCounters,
    ) -> bool {
        if self.observer {
            return true;
        }
        counters.queries += 1;
        self.detectors.iter().any(|d| {
            counters.index_candidates += 1;
            detector_detects(d, pos, los, counters)
        })
    }

    /// Same answer as [`entity_visible_to_team`] for the world this vision was built from. `world`
    /// is only asked for alliances of teams outside the gathered range.
    pub fn entity_visible<W: FogWorld + ?Sized, L: LineOfSight + ?Sized>(
        &self,
        world: &W,
        entity: &FogEntity<'_>,
        los: &L,
    ) -> bool {
        self.entity_visible_counted(world, entity, los, &mut FogCounters::default())
    }

    /// [`TeamVision::entity_visible`], adding its work to `counters`.
    pub fn entity_visible_counted<W: FogWorld + ?Sized, L: LineOfSight + ?Sized>(
        &self,
        world: &W,
        entity: &FogEntity<'_>,
        los: &L,
        counters: &mut FogCounters,
    ) -> bool {
        if !self.enabled || self.observer {
            return true;
        }
        let player = entity.team != 0 && entity.team <= self.team_count;
        if !player || self.shares_sight(world, entity.team) || !entity.alive {
            return true;
        }
        match entity.class {
            SightClass::Feature => true,
            SightClass::Unit | SightClass::OnFoot | SightClass::Building => {
                if self.sight.any_stealth() && self.sight.row_of(entity).is_some_and(|row| row.stealth) {
                    self.detects_point_counted(entity.position, los, counters)
                } else {
                    self.sees_point_counted(entity.position, los, counters)
                }
            }
        }
    }

    /// Watchers as `(eye, radius)`, for the presentation grid.
    /// The living own and allied watchers of the last rebuild: `(position, sight radius)`, in
    /// the world's order. For presentation and debug views (the editor's fog view draws them).
    pub fn watcher_circles(&self) -> impl Iterator<Item = ([f32; 3], f32)> + '_ {
        self.watchers.iter().map(|w| (w.at, w.radius))
    }

    fn watcher_discs(&self) -> impl Iterator<Item = ([f32; 3], [f32; 3], f32)> + '_ {
        self.watchers.iter().map(|w| (w.at, w.eye, w.radius))
    }
}

/// Appends the ids of every entity of `world` that `viewer` sees to `out` (cleared first), sorted
/// and deduplicated, and returns how many entities were hidden.
pub fn visible_ids<W: FogWorld + ?Sized, L: LineOfSight + ?Sized>(
    world: &W,
    rules: &FogRules,
    viewer: TeamId,
    los: &L,
    vision: &mut TeamVision,
    out: &mut Vec<u64>,
) -> usize {
    vision.rebuild(world, rules, viewer);
    out.clear();
    let mut hidden = 0;
    for entity in world.fog_entities() {
        if vision.entity_visible(world, &entity, los) {
            out.push(entity.id);
        } else {
            hidden += 1;
        }
    }
    out.sort_unstable();
    out.dedup();
    hidden
}

/// Default presentation fog grid cell, in metres.
pub const FOG_GRID_CELL_M: f32 = 32.0;
/// Default half extent, in metres, a grid falls back to when it is given a non-finite or tiny one.
pub const FOG_GRID_FALLBACK_HALF_M: f32 = 640.0;
/// Most cells a grid may have (a 4096 x 4096 grid); [`FogGrid::try_covering`] refuses more.
pub const FOG_GRID_MAX_CELLS: usize = 4096 * 4096;

/// Why [`FogGrid::try_covering`] refused a grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FogGridError {
    /// The cell size is not a finite number of at least 1 cm.
    BadCell(f32),
    /// The grid would need more than [`FOG_GRID_MAX_CELLS`] cells.
    TooManyCells {
        /// Columns the grid would need.
        cols: usize,
        /// Rows the grid would need.
        rows: usize,
    },
}

impl std::fmt::Display for FogGridError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadCell(cell) => write!(
                f,
                "fog grid cell size {cell} is not a finite size of at least 0.01 m"
            ),
            Self::TooManyCells { cols, rows } => write!(
                f,
                "fog grid of {cols} x {rows} cells exceeds the limit of {FOG_GRID_MAX_CELLS} cells"
            ),
        }
    }
}

impl std::error::Error for FogGridError {}

/// Seen and visible cells over a playfield, for presentation (a minimap's explored area and the
/// lit area of a fog overlay). `seen` is the explored layer (0 never seen, 1 seen at some point),
/// `visible` the visible-now layer (rebuilt by every [`FogGrid::update`]); together they give each
/// cell a [`CellState`]. The origin is the south-west corner of cell (0, 0), centered on the
/// playfield when the cell count does not divide the size evenly. Nothing here feeds the sim. One
/// grid holds one viewing team.
///
/// The grid also keeps a **dirty-row bitset** for presentation uploads: every row whose
/// [`CellState`]s changed since the last [`FogGrid::clear_dirty_rows`] (a new grid starts with every
/// row dirty). [`FogGrid::update`], [`FogGrid::stamp`] and [`FogGrid::clear`] keep it; code that
/// writes `seen` or `visible` directly calls [`FogGrid::mark_all_dirty`]. Equality ignores it.
#[derive(Debug, Clone)]
pub struct FogGrid {
    /// World X and Z of the south-west corner of cell (0, 0).
    pub origin: [f32; 2],
    /// Cell edge in metres.
    pub cell_m: f32,
    /// Columns (along X).
    pub cols: usize,
    /// Rows (along Z).
    pub rows: usize,
    /// One byte per cell, row-major from the origin: 0 unseen, 1 seen (explored).
    pub seen: Vec<u8>,
    /// One byte per cell, row-major from the origin: 1 when a watcher sees the cell's center now
    /// (as of the last [`FogGrid::update`]), else 0.
    pub visible: Vec<u8>,
    /// One bit per row: the row's cell states changed since the last `clear_dirty_rows`.
    dirty_rows: Vec<u64>,
    /// The next visible layer, built by `update` before it is compared with the current one.
    scratch: Vec<u8>,
}

impl PartialEq for FogGrid {
    fn eq(&self, other: &Self) -> bool {
        self.origin == other.origin
            && self.cell_m == other.cell_m
            && self.cols == other.cols
            && self.rows == other.rows
            && self.seen == other.seen
            && self.visible == other.visible
    }
}

/// What a viewing team knows of one fog cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellState {
    /// Never seen: black on a minimap.
    Unexplored,
    /// Seen before, not now: terrain and remembered structures (ghosts) show, units do not.
    Explored,
    /// In sight now.
    Visible,
}

/// The `y` a [`FogGrid::update`] asks the line-of-sight hook for at a cell center: the cell has no
/// height of its own, so a terrain hook takes its ground there (the det grid's hook raises any
/// point below the ground to the ground).
pub const FOG_CELL_POINT_Y: f32 = f32::MIN;

impl FogGrid {
    /// Covers `±half_x` by `±half_z` with [`FOG_GRID_CELL_M`] cells. A non-finite or tiny half falls
    /// back to [`FOG_GRID_FALLBACK_HALF_M`]. 1280 m is 40 columns with origin -640; 2050 m is 65
    /// columns with origin -1040.
    ///
    /// A half so large that the grid would pass [`FOG_GRID_MAX_CELLS`] also falls back.
    pub fn covering(half_x: f32, half_z: f32) -> Self {
        Self::try_covering(half_x, half_z, FOG_GRID_CELL_M, FOG_GRID_FALLBACK_HALF_M).unwrap_or_else(|_| {
            Self::build(
                FOG_GRID_FALLBACK_HALF_M,
                FOG_GRID_FALLBACK_HALF_M,
                FOG_GRID_CELL_M,
            )
        })
    }

    /// Like [`FogGrid::covering`] with a chosen cell size and fallback half extent. Refuses a cell
    /// that is not a finite size of at least 1 cm, and a grid of more than [`FOG_GRID_MAX_CELLS`].
    pub fn try_covering(
        half_x: f32,
        half_z: f32,
        cell_m: f32,
        fallback_half_m: f32,
    ) -> Result<Self, FogGridError> {
        if !(cell_m.is_finite() && cell_m >= 0.01) {
            return Err(FogGridError::BadCell(cell_m));
        }
        let fallback = Self::half_or_fallback(fallback_half_m, FOG_GRID_FALLBACK_HALF_M);
        let hx = Self::half_or_fallback(half_x, fallback);
        let hz = Self::half_or_fallback(half_z, fallback);
        let cols = Self::count(hx * 2.0, cell_m);
        let rows = Self::count(hz * 2.0, cell_m);
        if cols
            .checked_mul(rows)
            .is_none_or(|cells| cells > FOG_GRID_MAX_CELLS)
        {
            return Err(FogGridError::TooManyCells { cols, rows });
        }
        Ok(Self::build(hx, hz, cell_m))
    }

    fn half_or_fallback(half: f32, fallback: f32) -> f32 {
        if half.is_finite() && half > 1.0 {
            half
        } else {
            fallback
        }
    }

    fn count(span: f32, cell_m: f32) -> usize {
        ((span / cell_m).ceil() as usize).max(1)
    }

    fn build(hx: f32, hz: f32, cell_m: f32) -> Self {
        let width = hx * 2.0;
        let depth = hz * 2.0;
        let cols = Self::count(width, cell_m);
        let rows = Self::count(depth, cell_m);
        let span_x = cols as f32 * cell_m;
        let span_z = rows as f32 * cell_m;
        let origin = [-hx - (span_x - width) * 0.5, -hz - (span_z - depth) * 0.5];
        Self {
            origin,
            cell_m,
            cols,
            rows,
            seen: vec![0; cols * rows],
            visible: vec![0; cols * rows],
            dirty_rows: vec![u64::MAX; rows.div_ceil(64)],
            scratch: Vec::new(),
        }
    }

    fn mark_row(&mut self, row: usize) {
        if let Some(word) = self.dirty_rows.get_mut(row / 64) {
            *word |= 1u64 << (row % 64);
        }
    }

    /// True when row `row`'s cell states changed since the last [`FogGrid::clear_dirty_rows`].
    pub fn is_row_dirty(&self, row: usize) -> bool {
        row < self.rows
            && self
                .dirty_rows
                .get(row / 64)
                .is_some_and(|word| word & (1u64 << (row % 64)) != 0)
    }

    /// The dirty rows, ascending (see the type docs).
    pub fn dirty_rows(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.rows).filter(|&row| self.is_row_dirty(row))
    }

    /// Number of dirty rows.
    pub fn dirty_row_count(&self) -> usize {
        self.dirty_rows().count()
    }

    /// Forgets the dirty rows (a presentation layer calls it after it has read them).
    pub fn clear_dirty_rows(&mut self) {
        self.dirty_rows.fill(0);
    }

    /// Marks every row dirty: call it after writing `seen` or `visible` directly.
    pub fn mark_all_dirty(&mut self) {
        self.dirty_rows.fill(u64::MAX);
    }

    fn cell_at(&self, x: f32, z: f32) -> Option<usize> {
        let cx = ((x - self.origin[0]) / self.cell_m).floor();
        let cz = ((z - self.origin[1]) / self.cell_m).floor();
        if cx < 0.0 || cz < 0.0 {
            return None;
        }
        let (cx, cz) = (cx as usize, cz as usize);
        if cx >= self.cols || cz >= self.rows {
            return None;
        }
        Some(cz * self.cols + cx)
    }

    /// True when `(x, z)` lies on the grid.
    pub fn contains(&self, x: f32, z: f32) -> bool {
        self.cell_at(x, z).is_some()
    }

    /// The center of cell `(col, row)`.
    fn center(&self, col: usize, row: usize) -> (f32, f32) {
        (
            self.origin[0] + (col as f32 + 0.5) * self.cell_m,
            self.origin[1] + (row as f32 + 0.5) * self.cell_m,
        )
    }

    /// Cell index range `[lo, hi)` along one axis that can hold a center within `radius` of `at`,
    /// with one cell of margin on each side so float rounding never drops a cell.
    fn span(&self, at: f32, radius: f32, origin: f32, cells: usize) -> (usize, usize) {
        let lo = ((at - radius - origin) / self.cell_m).floor() - 1.0;
        let hi = ((at + radius - origin) / self.cell_m).ceil() + 1.0;
        let clamp = |v: f32| -> usize {
            if v <= 0.0 {
                0
            } else if v >= cells as f32 {
                cells
            } else {
                v as usize
            }
        };
        (clamp(lo), clamp(hi))
    }

    /// Marks every cell whose center lies inside `radius` of `(x, z)` as seen.
    pub fn stamp(&mut self, x: f32, z: f32, radius: f32) {
        let r2 = radius * radius;
        let (cols, rows) = if x.is_finite() && z.is_finite() && radius.is_finite() {
            (
                self.span(x, radius.abs(), self.origin[0], self.cols),
                self.span(z, radius.abs(), self.origin[1], self.rows),
            )
        } else {
            // Non-finite input: the full scan, so the answer is exactly the per-cell test's.
            ((0, self.cols), (0, self.rows))
        };
        for row in rows.0..rows.1 {
            for col in cols.0..cols.1 {
                let (cx, cz) = self.center(col, row);
                let dx = cx - x;
                let dz = cz - z;
                if dx * dx + dz * dz <= r2 && self.seen[row * self.cols + col] == 0 {
                    self.seen[row * self.cols + col] = 1;
                    self.mark_row(row);
                }
            }
        }
    }

    /// Stamps every living own or allied watcher of `viewer` with its sight radius (line of sight
    /// is not applied: the grid marks explored ground).
    pub fn stamp_team<W: FogWorld + ?Sized>(&mut self, world: &W, rules: &FogRules, viewer: TeamId) {
        for entity in world.fog_entities() {
            if let Some((at, radius)) = watcher_of(world, rules, viewer, &entity) {
                self.stamp(at[0], at[2], radius);
            }
        }
    }

    /// True when the cell under `(x, z)` has been seen.
    pub fn seen_at(&self, x: f32, z: f32) -> bool {
        self.cell_at(x, z).is_some_and(|i| self.seen[i] != 0)
    }

    /// Number of seen cells.
    pub fn seen_count(&self) -> usize {
        self.seen.iter().filter(|&&s| s != 0).count()
    }

    /// Marks every cell unseen and not visible.
    pub fn clear(&mut self) {
        self.seen.fill(0);
        self.visible.fill(0);
        self.mark_all_dirty();
    }

    /// Rebuilds the visible-now layer from `vision` and adds it to the explored layer: a cell is
    /// visible when its center lies inside a watcher's radius and the hook gives that watcher's eye
    /// a clear line to the center (at [`FOG_CELL_POINT_Y`]). An observer sees every cell. The
    /// elevation rule does not apply here (a cell has no height of its own); give the grid the ground
    /// with [`Self::update_on_ground`] for that. Returns the hook calls made.
    ///
    /// With [`OpenGround`] the visible layer is exactly the radius stamp of every watcher
    /// ([`FogGrid::stamp_team`] for the same viewer).
    ///
    /// Rows whose cell states changed are marked dirty (see the type docs); an unchanged view marks
    /// none.
    pub fn update<L: LineOfSight + ?Sized>(&mut self, vision: &TeamVision, los: &L) -> u64 {
        self.update_inner(vision, los, None)
    }

    /// [`Self::update`] with the ground under each cell, so the elevation rule
    /// ([`FogRules::elevation`]) applies to cells as it does to entities (KI-GP-018, ADR-10070): a cell
    /// whose ground (`ground(x, z)` at its center, metres) stands more than `step_m` above a
    /// watcher's position is seen only inside that watcher's scaled radius. Without an elevation
    /// rule, or for an observer, the answer is exactly [`Self::update`]'s. The hook is asked at the
    /// same point ([`FOG_CELL_POINT_Y`]) either way. Every peer must pass the same heights.
    pub fn update_on_ground<L: LineOfSight + ?Sized, G: Fn(f32, f32) -> f32>(
        &mut self,
        vision: &TeamVision,
        los: &L,
        ground: G,
    ) -> u64 {
        let ground: &dyn Fn(f32, f32) -> f32 = &ground;
        self.update_inner(vision, los, Some(ground))
    }

    fn update_inner<L: LineOfSight + ?Sized>(
        &mut self,
        vision: &TeamVision,
        los: &L,
        ground: Option<&dyn Fn(f32, f32) -> f32>,
    ) -> u64 {
        let mut next = std::mem::take(&mut self.scratch);
        next.clear();
        next.resize(self.visible.len(), 0);
        let calls = if vision.is_observer() {
            next.fill(1);
            for (row, seen) in self.seen.chunks_mut(self.cols.max(1)).enumerate() {
                if seen.contains(&0) {
                    seen.fill(1);
                    if let Some(word) = self.dirty_rows.get_mut(row / 64) {
                        *word |= 1u64 << (row % 64);
                    }
                }
            }
            0
        } else {
            let lift = vision.elevation.zip(ground);
            self.trace_visible(vision, los, lift, &mut next)
        };
        for row in 0..self.rows {
            let span = row * self.cols..(row + 1) * self.cols;
            if self.visible[span.clone()] != next[span] {
                self.mark_row(row);
            }
        }
        std::mem::swap(&mut self.visible, &mut next);
        self.scratch = next;
        calls
    }

    /// The visible-now layer of `vision` into `next` (zeroed, one byte per cell); newly seen cells
    /// join `seen` and dirty their row.
    fn trace_visible<L: LineOfSight + ?Sized>(
        &mut self,
        vision: &TeamVision,
        los: &L,
        lift: Option<(ElevationRule, GroundHeight<'_>)>,
        next: &mut [u8],
    ) -> u64 {
        let mut calls = 0u64;
        for (at, eye, radius) in vision.watcher_discs() {
            let (x, z) = (at[0], at[2]);
            let r2 = radius * radius;
            // The elevation rule on cells (KI-GP-018): the radius toward higher ground.
            let low = lift.map(|(rule, ground)| {
                let r = radius * rule.low_ground_scale;
                (rule.step_m, r * r, ground)
            });
            let (cols, rows) = if x.is_finite() && z.is_finite() && radius.is_finite() {
                (
                    self.span(x, radius, self.origin[0], self.cols),
                    self.span(z, radius, self.origin[1], self.rows),
                )
            } else {
                ((0, self.cols), (0, self.rows))
            };
            for row in rows.0..rows.1 {
                for col in cols.0..cols.1 {
                    let i = row * self.cols + col;
                    if next[i] != 0 {
                        continue;
                    }
                    let (cx, cz) = self.center(col, row);
                    let dx = cx - x;
                    let dz = cz - z;
                    let d2 = dx * dx + dz * dz;
                    let in_range = match low {
                        Some((step_m, low_r2, ground)) if ground(cx, cz) - at[1] > step_m => d2 <= low_r2,
                        _ => d2 <= r2,
                    };
                    if in_range {
                        calls += 1;
                        if los.clear(eye, [cx, FOG_CELL_POINT_Y, cz]) {
                            next[i] = 1;
                            if self.seen[i] == 0 {
                                self.seen[i] = 1;
                                self.mark_row(row);
                            }
                        }
                    }
                }
            }
        }
        calls
    }

    /// The state of the cell under `(x, z)`; `None` off the grid.
    pub fn state_at(&self, x: f32, z: f32) -> Option<CellState> {
        self.cell_at(x, z).map(|i| self.state_of(i))
    }

    /// The state of cell `(col, row)`; `None` off the grid.
    pub fn state(&self, col: usize, row: usize) -> Option<CellState> {
        (col < self.cols && row < self.rows).then(|| self.state_of(row * self.cols + col))
    }

    fn state_of(&self, i: usize) -> CellState {
        if self.visible[i] != 0 {
            CellState::Visible
        } else if self.seen[i] != 0 {
            CellState::Explored
        } else {
            CellState::Unexplored
        }
    }

    /// True when the cell under `(x, z)` is visible now.
    pub fn visible_at(&self, x: f32, z: f32) -> bool {
        self.cell_at(x, z).is_some_and(|i| self.visible[i] != 0)
    }

    /// Number of visible cells.
    pub fn visible_count(&self) -> usize {
        self.visible.iter().filter(|&&s| s != 0).count()
    }
}

/// What a team remembers of an enemy it no longer sees: the fields of its last sighting, never a
/// live one.
#[derive(Debug, Clone, PartialEq)]
pub struct Ghost {
    /// The sim's stable id of the remembered entity.
    pub id: u64,
    /// Its kind key when last seen.
    pub kind: String,
    /// Its sight class when last seen.
    pub class: SightClass,
    /// Its team when last seen.
    pub team: TeamId,
    /// Where it was last seen.
    pub position: [f32; 3],
    /// The tick it was last seen on (the tick the host passed to [`FogMemory::update`]).
    pub tick: u64,
}

/// Counters of the last [`FogMemory::update`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryCounters {
    /// Ghosts the viewer holds after the update.
    pub ghosts: u64,
    /// Ghosts refreshed from a live sighting (new or moved).
    pub refreshed: u64,
    /// Ghosts dropped (their spot seen empty, or the entity seen dead).
    pub dropped: u64,
    /// Sight queries the update made (one per ghost not refreshed).
    pub queries: u64,
}

/// Last-seen ghosts of remembered enemies ([`KindSight::remember`]), per viewing team.
///
/// Each [`FogMemory::update`] for a viewer: every living enemy of a remembered kind that the
/// viewer sees now refreshes its ghost (kind, class, team, position, tick); an enemy seen dead
/// drops its ghost; and every other ghost whose position a watcher now sees is dropped (the spot
/// is seen empty: the entity moved, died unseen, or is a stealthed entity no detector shows).
/// Ghosts are kept sorted by id; nothing iterates a hash container, so the memory is a pure
/// function of the sequence of states it was shown.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FogMemory {
    /// `(viewer, ghosts sorted by id)`, sorted by viewer.
    teams: Vec<(TeamId, Vec<Ghost>)>,
    /// Ids refreshed by the current update (scratch, sorted).
    refreshed: Vec<u64>,
    last: MemoryCounters,
}

impl FogMemory {
    /// An empty memory.
    pub fn new() -> Self {
        Self::default()
    }

    /// The ghosts `viewer` holds, sorted by id (live and hidden alike: a host shows a ghost only
    /// while its entity is not visible).
    pub fn ghosts(&self, viewer: TeamId) -> &[Ghost] {
        match self.teams.binary_search_by_key(&viewer, |(team, _)| *team) {
            Ok(at) => &self.teams[at].1,
            Err(_) => &[],
        }
    }

    /// Ghosts held over every viewer.
    pub fn ghost_count(&self) -> usize {
        self.teams.iter().map(|(_, ghosts)| ghosts.len()).sum()
    }

    /// Counters of the last update.
    pub fn last_counters(&self) -> MemoryCounters {
        self.last
    }

    /// Forgets every ghost of every viewer.
    pub fn clear(&mut self) {
        self.teams.clear();
    }

    /// Updates the viewer of `vision` (see the type docs) from `world` at `tick`. An observer
    /// viewer, or fog off, holds no ghosts (it sees everything).
    pub fn update<W: FogWorld + ?Sized, L: LineOfSight + ?Sized>(
        &mut self,
        world: &W,
        vision: &TeamVision,
        los: &L,
        tick: u64,
    ) -> MemoryCounters {
        let viewer = vision.viewer();
        let mut counters = MemoryCounters::default();
        let at = match self.teams.binary_search_by_key(&viewer, |(team, _)| *team) {
            Ok(at) => at,
            Err(at) => {
                if vision.is_observer() || !vision.is_enabled() || !vision.compiled_sight().any_remember() {
                    self.last = counters;
                    return counters;
                }
                self.teams.insert(at, (viewer, Vec::new()));
                at
            }
        };
        if vision.is_observer() || !vision.is_enabled() {
            counters.dropped = self.teams[at].1.len() as u64;
            self.teams.remove(at);
            self.last = counters;
            return counters;
        }
        self.refreshed.clear();
        let sight = vision.compiled_sight();
        let mut fog = FogCounters::default();
        {
            let ghosts = &mut self.teams[at].1;
            for entity in world.fog_entities() {
                if !vision.is_enemy(world, entity.team) || entity.class == SightClass::Feature {
                    continue;
                }
                if !sight.row_of(&entity).is_some_and(|row| row.remember) {
                    continue;
                }
                if !vision.entity_visible_counted(world, &entity, los, &mut fog) {
                    continue;
                }
                let found = ghosts.binary_search_by_key(&entity.id, |g| g.id);
                if !entity.alive {
                    if let Ok(i) = found {
                        ghosts.remove(i);
                        counters.dropped += 1;
                    }
                    continue;
                }
                match found {
                    Ok(i) => {
                        let ghost = &mut ghosts[i];
                        if ghost.kind != entity.kind {
                            ghost.kind.clear();
                            ghost.kind.push_str(entity.kind);
                        }
                        ghost.class = entity.class;
                        ghost.team = entity.team;
                        ghost.position = entity.position;
                        ghost.tick = tick;
                    }
                    Err(i) => ghosts.insert(
                        i,
                        Ghost {
                            id: entity.id,
                            kind: entity.kind.to_string(),
                            class: entity.class,
                            team: entity.team,
                            position: entity.position,
                            tick,
                        },
                    ),
                }
                counters.refreshed += 1;
                self.refreshed.push(entity.id);
            }
        }
        self.refreshed.sort_unstable();
        let refreshed = &self.refreshed;
        let ghosts = &mut self.teams[at].1;
        let before = ghosts.len();
        ghosts.retain(|ghost| {
            if refreshed.binary_search(&ghost.id).is_ok() {
                return true;
            }
            counters.queries += 1;
            !vision.sees_point_counted(ghost.position, los, &mut fog)
        });
        counters.dropped += (before - ghosts.len()) as u64;
        counters.ghosts = ghosts.len() as u64;
        self.last = counters;
        counters
    }
}
