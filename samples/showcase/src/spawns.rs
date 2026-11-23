// SPDX-License-Identifier: MIT-0
//! The spawn schedule (`spawns.ron`): VFX and decal spawns at given frames.
//!
//! Every entry uses an existing engine API: effects are [`regia_vfx::SpawnEffect`] messages and
//! decals are [`regia_vfx::SpawnDecal`] messages (or a named prefab through
//! [`regia_vfx::place_decal`]). The schedule only decides *when* and *where*; it adds no effect.
//!
//! ```text
//! Spawns(
//!     version: 1,
//!     effects: [
//!         (frame: 200, effect: "explosion", at: UnderEntity(offset: (0.0, 1.0, 0.0)), forward: (0.0, 1.0, 0.0), seed: Some(7)),
//!     ],
//!     decals: [
//!         (frame: 201, at: UnderEntity(offset: (0.0, 0.0, 0.0)), half_size: (6.0, 6.0, 2.0),
//!          color: (0.05, 0.04, 0.04, 0.9), lifetime: None, prefab: None, yaw_deg: 0.0),
//!     ],
//! )
//! ```

use std::path::Path;

use regia_core::glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::{ErrorCode, ShowcaseError};

/// Format version of `spawns.ron`.
pub const SPAWNS_VERSION: u32 = 1;

/// Most entries (effects and decals together) a file may hold.
pub const MAX_ENTRIES: usize = 100_000;

/// Where a spawn happens.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Place {
    /// A world position in metres.
    World((f32, f32, f32)),
    /// The terrain surface at world `(x, z)`, plus `offset` metres along the surface normal's up.
    Ground {
        /// World X.
        x: f32,
        /// World Z.
        z: f32,
        /// Metres above the surface.
        offset: f32,
    },
    /// The tracked entity's position at the spawn frame, plus a world-space `offset`.
    Entity {
        /// World-space offset in metres.
        offset: (f32, f32, f32),
    },
    /// The terrain surface under the tracked entity, plus `offset` metres along world up.
    UnderEntity {
        /// Metres above the surface.
        offset: (f32, f32, f32),
    },
}

impl Place {
    /// True when the position depends on the tracked entity.
    #[must_use]
    pub fn needs_track(&self) -> bool {
        matches!(self, Place::Entity { .. } | Place::UnderEntity { .. })
    }

    fn finite(&self) -> bool {
        match *self {
            Place::World(p) => finite3(p),
            Place::Ground { x, z, offset } => x.is_finite() && z.is_finite() && offset.is_finite(),
            Place::Entity { offset } | Place::UnderEntity { offset } => finite3(offset),
        }
    }
}

fn finite3(v: (f32, f32, f32)) -> bool {
    v.0.is_finite() && v.1.is_finite() && v.2.is_finite()
}

/// One VFX spawn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectSpawn {
    /// Frame of the 60 Hz step the effect is requested on.
    pub frame: u32,
    /// Effect (system) name.
    pub effect: String,
    /// Where.
    pub at: Place,
    /// Local +Z of the instance (the velocity cone axis). Default: world up.
    #[serde(default = "up")]
    pub forward: (f32, f32, f32),
    /// Deterministic seed; `None` lets the engine derive one from a per-run counter.
    #[serde(default)]
    pub seed: Option<u64>,
}

fn up() -> (f32, f32, f32) {
    (0.0, 1.0, 0.0)
}

/// One decal spawn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecalSpawn {
    /// Frame of the 60 Hz step the decal is requested on.
    pub frame: u32,
    /// Where (the centre of the decal volume).
    pub at: Place,
    /// Surface normal; local +Z of the decal aligns to it. `None`: the terrain normal for a ground
    /// place, world up otherwise.
    #[serde(default)]
    pub normal: Option<(f32, f32, f32)>,
    /// Half extents `(u, v, depth)` in metres (ignored when `prefab` is set).
    #[serde(default = "default_half_size")]
    pub half_size: (f32, f32, f32),
    /// Linear RGBA tint (ignored when `prefab` is set).
    #[serde(default = "default_decal_color")]
    pub color: (f32, f32, f32, f32),
    /// Lifetime in seconds; `None` keeps it until the pool recycles it.
    #[serde(default)]
    pub lifetime: Option<f32>,
    /// A decal prefab name (`scorch`, `rut`, ... or one loaded from the scene's `vfx` directory).
    #[serde(default)]
    pub prefab: Option<String>,
    /// Extra yaw about the normal, degrees.
    #[serde(default)]
    pub yaw_deg: f32,
}

fn default_half_size() -> (f32, f32, f32) {
    (2.0, 2.0, 1.0)
}

fn default_decal_color() -> (f32, f32, f32, f32) {
    (0.05, 0.04, 0.04, 0.9)
}

/// The parsed `spawns.ron`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename = "Spawns", deny_unknown_fields)]
pub struct SpawnFile {
    /// Format version ([`SPAWNS_VERSION`]).
    pub version: u32,
    /// VFX spawns, any order.
    #[serde(default)]
    pub effects: Vec<EffectSpawn>,
    /// Decal spawns, any order.
    #[serde(default)]
    pub decals: Vec<DecalSpawn>,
}

impl SpawnFile {
    /// Parses and validates `text`.
    ///
    /// # Errors
    /// [`ErrorCode::SpawnsMalformed`] or [`ErrorCode::SpawnNeedsTrack`] naming `path`.
    pub fn from_ron(text: &str, path: &Path, has_track: bool) -> Result<Self, ShowcaseError> {
        let bad = |message: String| ShowcaseError::new(ErrorCode::SpawnsMalformed, path, message);
        let file: SpawnFile = ron::from_str(text).map_err(|e| bad(e.to_string()))?;
        file.validate().map_err(bad)?;
        if !has_track {
            let needs = file.effects.iter().any(|e| e.at.needs_track())
                || file.decals.iter().any(|d| d.at.needs_track());
            if needs {
                return Err(ShowcaseError::new(
                    ErrorCode::SpawnNeedsTrack,
                    path,
                    "a spawn uses Entity or UnderEntity but the scene has no track",
                ));
            }
        }
        Ok(file)
    }

    /// Checks every field.
    ///
    /// # Errors
    /// A message naming the entry.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != SPAWNS_VERSION {
            return Err(format!(
                "version {} (this build reads {SPAWNS_VERSION})",
                self.version
            ));
        }
        if self.effects.len() + self.decals.len() > MAX_ENTRIES {
            return Err(format!("more than {MAX_ENTRIES} entries"));
        }
        for (i, e) in self.effects.iter().enumerate() {
            if e.effect.is_empty() || e.effect.len() > 128 {
                return Err(format!("effects[{i}]: effect must be 1 to 128 characters"));
            }
            if !e.at.finite() || !finite3(e.forward) {
                return Err(format!("effects[{i}]: a number is not finite"));
            }
            let f = e.forward;
            if f.0 * f.0 + f.1 * f.1 + f.2 * f.2 < 1e-12 {
                return Err(format!("effects[{i}]: forward is the zero vector"));
            }
        }
        for (i, d) in self.decals.iter().enumerate() {
            let color_ok = d.color.0.is_finite()
                && d.color.1.is_finite()
                && d.color.2.is_finite()
                && d.color.3.is_finite();
            if !d.at.finite() || !finite3(d.half_size) || !color_ok || !d.yaw_deg.is_finite() {
                return Err(format!("decals[{i}]: a number is not finite"));
            }
            if d.half_size.0 <= 0.0 || d.half_size.1 <= 0.0 || d.half_size.2 <= 0.0 {
                return Err(format!("decals[{i}]: half_size must be positive"));
            }
            if let Some(n) = d.normal
                && (!finite3(n) || n.0 * n.0 + n.1 * n.1 + n.2 * n.2 < 1e-12)
            {
                return Err(format!("decals[{i}]: normal must be a non-zero finite vector"));
            }
            if d.lifetime.is_some_and(|l| !l.is_finite() || l <= 0.0) {
                return Err(format!("decals[{i}]: lifetime must be positive"));
            }
        }
        Ok(())
    }
}

/// One event of the schedule, in firing order.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Request a VFX effect.
    Effect(EffectSpawn),
    /// Request a decal.
    Decal(DecalSpawn),
}

impl Event {
    /// The frame the event fires on.
    #[must_use]
    pub fn frame(&self) -> u32 {
        match self {
            Event::Effect(e) => e.frame,
            Event::Decal(d) => d.frame,
        }
    }
}

/// The events of a [`SpawnFile`] sorted by frame (effects before decals within a frame, file order
/// within each), with a cursor so every event fires exactly once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Schedule {
    events: Vec<Event>,
    cursor: usize,
}

impl Schedule {
    /// Builds the schedule of `file`.
    #[must_use]
    pub fn new(file: &SpawnFile) -> Self {
        let mut events: Vec<Event> = file
            .effects
            .iter()
            .cloned()
            .map(Event::Effect)
            .chain(file.decals.iter().cloned().map(Event::Decal))
            .collect();
        // A stable sort keeps effects before decals on one frame and the file order inside each.
        events.sort_by_key(Event::frame);
        Self { events, cursor: 0 }
    }

    /// Number of events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// True when there is nothing to fire.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Events not yet fired.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.events.len() - self.cursor
    }

    /// Takes the events due at `frame` or earlier that have not fired yet.
    pub fn take_due(&mut self, frame: i64) -> Vec<Event> {
        let start = self.cursor;
        while self.cursor < self.events.len() && i64::from(self.events[self.cursor].frame()) <= frame {
            self.cursor += 1;
        }
        self.events[start..self.cursor].to_vec()
    }

    /// Rewinds so a seek back replays the events.
    pub fn rewind(&mut self) {
        self.cursor = 0;
    }
}

/// What [`resolve`] needs to know about the world.
pub trait Ground {
    /// Terrain height at world `(x, z)`.
    fn height(&self, x: f32, z: f32) -> f32;
    /// Unit terrain normal at world `(x, z)`.
    fn normal(&self, x: f32, z: f32) -> Vec3;
}

/// The world position of `place`. `entity` is the tracked entity's position at the spawn frame
/// (with its height already resolved), when there is a track.
#[must_use]
pub fn resolve(place: &Place, entity: Option<Vec3>, ground: &dyn Ground) -> Vec3 {
    match *place {
        Place::World(p) => Vec3::new(p.0, p.1, p.2),
        Place::Ground { x, z, offset } => Vec3::new(x, ground.height(x, z) + offset, z),
        Place::Entity { offset } => entity.unwrap_or(Vec3::ZERO) + Vec3::new(offset.0, offset.1, offset.2),
        Place::UnderEntity { offset } => {
            let e = entity.unwrap_or(Vec3::ZERO);
            Vec3::new(e.x + offset.0, ground.height(e.x, e.z) + offset.1, e.z + offset.2)
        }
    }
}

/// The surface normal a decal at `place` uses when the entry names none.
#[must_use]
pub fn default_normal(place: &Place, entity: Option<Vec3>, ground: &dyn Ground) -> Vec3 {
    match *place {
        Place::Ground { x, z, .. } => ground.normal(x, z),
        Place::UnderEntity { .. } => {
            let e = entity.unwrap_or(Vec3::ZERO);
            ground.normal(e.x, e.z)
        }
        Place::World(_) | Place::Entity { .. } => Vec3::Y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Slope;
    impl Ground for Slope {
        fn height(&self, x: f32, _z: f32) -> f32 {
            x * 0.5
        }
        fn normal(&self, _x: f32, _z: f32) -> Vec3 {
            Vec3::new(-0.5, 1.0, 0.0).normalize()
        }
    }

    fn file(text: &str) -> Result<SpawnFile, ShowcaseError> {
        SpawnFile::from_ron(text, Path::new("spawns.ron"), true)
    }

    const TEXT: &str = r#"Spawns(
        version: 1,
        effects: [
            (frame: 30, effect: "smoke", at: World((1.0, 2.0, 3.0))),
            (frame: 10, effect: "explosion", at: UnderEntity(offset: (0.0, 1.0, 0.0)), seed: Some(7)),
            (frame: 30, effect: "sparks", at: Ground(x: 4.0, z: 0.0, offset: 0.5), forward: (0.0, 0.0, 1.0)),
        ],
        decals: [
            (frame: 10, at: UnderEntity(offset: (0.0, 0.0, 0.0)), prefab: Some("scorch")),
            (frame: 5, at: World((0.0, 0.0, 0.0))),
        ],
    )"#;

    #[test]
    fn the_schedule_is_sorted_and_stable() {
        let s = Schedule::new(&file(TEXT).expect("parses"));
        assert_eq!(s.len(), 5);
        let frames: Vec<u32> = s.events.iter().map(Event::frame).collect();
        assert_eq!(frames, [5, 10, 10, 30, 30]);
        // On frame 10 the effect comes before the decal; on frame 30 the file order is kept.
        assert!(matches!(&s.events[1], Event::Effect(e) if e.effect == "explosion"));
        assert!(matches!(&s.events[2], Event::Decal(_)));
        assert!(matches!(&s.events[3], Event::Effect(e) if e.effect == "smoke"));
        assert!(matches!(&s.events[4], Event::Effect(e) if e.effect == "sparks"));
    }

    #[test]
    fn every_event_fires_exactly_once() {
        let mut s = Schedule::new(&file(TEXT).expect("parses"));
        let mut fired = Vec::new();
        for frame in -3..=40 {
            for e in s.take_due(frame) {
                fired.push((frame, e.frame()));
            }
        }
        assert_eq!(fired.len(), 5, "no event is lost or repeated");
        assert!(
            fired.iter().all(|(at, due)| at == &i64::from(*due)),
            "each fires on its frame"
        );
        assert_eq!(s.remaining(), 0);
        assert!(s.take_due(1000).is_empty());
        s.rewind();
        assert_eq!(s.take_due(5).len(), 1, "a rewind replays from the start");
    }

    #[test]
    fn a_late_start_fires_the_missed_events_once() {
        let mut s = Schedule::new(&file(TEXT).expect("parses"));
        assert_eq!(s.take_due(10).len(), 3, "frames 5, 10, 10");
        assert_eq!(s.take_due(10).len(), 0);
        assert_eq!(s.take_due(30).len(), 2);
    }

    #[test]
    fn places_resolve_against_the_ground_and_the_entity() {
        let g = Slope;
        let entity = Some(Vec3::new(10.0, 99.0, 4.0));
        let p = |place| resolve(&place, entity, &g);
        assert_eq!(p(Place::World((1.0, 2.0, 3.0))), Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(
            p(Place::Ground {
                x: 4.0,
                z: 7.0,
                offset: 0.5
            }),
            Vec3::new(4.0, 2.5, 7.0)
        );
        assert_eq!(
            p(Place::Entity {
                offset: (0.0, 1.0, 0.0)
            }),
            Vec3::new(10.0, 100.0, 4.0)
        );
        assert_eq!(
            p(Place::UnderEntity {
                offset: (1.0, 1.0, 0.0)
            }),
            Vec3::new(11.0, 6.0, 4.0)
        );
        let n = default_normal(
            &Place::UnderEntity {
                offset: (0.0, 0.0, 0.0),
            },
            entity,
            &g,
        );
        assert!((n - Vec3::new(-0.5, 1.0, 0.0).normalize()).length() < 1e-6);
        assert_eq!(
            default_normal(&Place::World((0.0, 0.0, 0.0)), entity, &g),
            Vec3::Y
        );
    }

    #[test]
    fn bad_files_are_refused_with_a_code() {
        let p = Path::new("spawns.ron");
        let bad = [
            "garbage",
            r#"Spawns(version: 9)"#,
            r#"Spawns(version: 1, effects: [(frame: 0, effect: "", at: World((0.0, 0.0, 0.0)))])"#,
            r#"Spawns(version: 1, effects: [(frame: 0, effect: "a", at: World((0.0, 0.0, 0.0)), forward: (0.0, 0.0, 0.0))])"#,
            r#"Spawns(version: 1, decals: [(frame: 0, at: World((0.0, 0.0, 0.0)), half_size: (0.0, 1.0, 1.0))])"#,
            r#"Spawns(version: 1, decals: [(frame: 0, at: World((0.0, 0.0, 0.0)), lifetime: Some(-1.0))])"#,
            r#"Spawns(version: 1, mystery: [])"#,
        ];
        for text in bad {
            let err = SpawnFile::from_ron(text, p, true).expect_err(text);
            assert_eq!(err.code, ErrorCode::SpawnsMalformed, "{text}: {err}");
        }
    }

    #[test]
    fn entity_places_need_a_track() {
        let err = SpawnFile::from_ron(TEXT, Path::new("spawns.ron"), false).expect_err("no track");
        assert_eq!(err.code, ErrorCode::SpawnNeedsTrack);
        let ok = r#"Spawns(version: 1, effects: [(frame: 0, effect: "a", at: World((0.0, 0.0, 0.0)))])"#;
        assert!(SpawnFile::from_ron(ok, Path::new("spawns.ron"), false).is_ok());
    }

    #[test]
    fn an_empty_file_is_a_valid_empty_schedule() {
        let s = Schedule::new(&file("Spawns(version: 1)").expect("parses"));
        assert!(s.is_empty());
    }
}
