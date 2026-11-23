// SPDX-License-Identifier: MPL-2.0
//! The Fracture panel's tools past the cut (ADR-9750, `book/agents/fracture.md`): which chunks the
//! world holds up, a test break in the editor, the bond view and **Place**.
//!
//! - **Supports** ([`SupportPick`]): the cook's rule from the settings (the lowest centimetre, a
//!   `support_below_y` plane, or none for a free object), chunks picked by clicking them in the
//!   viewport (or listed by an agent), or the chunks touching a box. Picks and the box are written
//!   into the saved `.destructible.ron` as per-chunk support flags; only the plane rule lives in the
//!   mesh `.meta` and so survives a re-cook.
//! - **Test break** ([`TestBreak`]): the saved asset's break model, `DestructionWorld` with the same
//!   cook, run in the editor. A hit (a viewport click or `editor.fracture_test_hit`) is an explosion
//!   queued and stepped [`TEST_TICKS`] fixed ticks; the panel shows broken bonds, detached and
//!   collapsed chunks and the falling set with the `structure_hash`, which a game's world fed the
//!   same asset, pose, seed and hits matches bit for bit. **Reset** starts over.
//! - **Debris preview** ([`DebrisPreview`], ADR-7745): **Run debris** replays the hits in a preview
//!   world the way a game runs them (the destruction world, its `DestructionPhysics` bridge and a
//!   rapier world through `regia_physics`' public API, a floor under the structure's lowest point)
//!   and then runs the loose chunks [`DEBRIS_STEPS`] more fixed steps; the viewport draws each chunk's
//!   box where it came to rest. **Reset** drops it with the test.
//! - **Bonds**: each bond drawn across its face, colored by its break impulse (red is the weakest),
//!   a hovered bond named, and the weakest ten listed.
//! - **Place** ([`place`]): spawns an entity with a [`DestructibleSource`] naming the saved asset at
//!   the viewport drop point, one undo step, saved with the level.

use std::collections::BTreeSet;
use std::sync::Arc;

use regia::physics::PhysicsWorld;
use regia::physics::destruction::{
    Authority, BreakKind, ChunkState, Damage, DamageKind, DestructibleAsset, DestructibleId,
    DestructiblePose, DestructibleSource, DestructionError, DestructionPhysics, DestructionWorld,
    DetachCause, add_fixed_box, step_standalone,
};
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::destructible_cook::{SupportRule, cook_destructible};
use regia_asset_pipeline::fracture::FractureSet;
use regia_asset_pipeline::fracture_cook::destructible_cook_settings;
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

use crate::audio_gizmo::Segment;
use crate::command::{EditorCommand, SpawnEntities, UndoStack};
use crate::fracture_panel::{FracturePanel, PanelError};
use crate::viewport_agent::Handler;

/// Fixed ticks one test hit runs: enough for the per-tick break cap to drain on a set of the
/// panel's size (1024 chunks), so a hit's whole result shows at once.
pub const TEST_TICKS: u32 = 4;

/// The fixed tick of a test break, seconds (60 Hz, the engine's default fixed rate).
pub const TEST_DT: f32 = 1.0 / 60.0;

/// The instance seed of the test break and of a placed destructible.
pub const TEST_SEED: u64 = 0;

/// Most bonds the weakest list shows.
pub const WEAKEST_BONDS: usize = 10;

/// Color of support chunks in the viewport.
pub const SUPPORT_EDGES: egui::Color32 = egui::Color32::from_rgb(90, 160, 255);
/// Color of chunks a hit knocked loose.
pub const DETACHED_EDGES: egui::Color32 = egui::Color32::from_rgb(235, 70, 60);
/// Color of chunks that fell because they lost their support.
pub const COLLAPSED_EDGES: egui::Color32 = egui::Color32::from_rgb(200, 90, 200);
/// Color of a broken bond.
pub const BROKEN_BOND: egui::Color32 = egui::Color32::from_rgb(90, 90, 90);
/// Color of the hovered bond.
pub const HOVERED_BOND: egui::Color32 = egui::Color32::WHITE;
/// Color of a debris chunk's box in the debris preview.
pub const DEBRIS_EDGES: egui::Color32 = egui::Color32::from_rgb(250, 170, 60);

/// Fixed steps the debris preview runs after the hits (one second at [`TEST_DT`]).
pub const DEBRIS_STEPS: u32 = 60;

/// Most fixed steps one debris preview runs.
pub const MAX_DEBRIS_STEPS: u32 = 600;

/// Gravity of the debris preview, m/s² (straight down).
pub const DEBRIS_GRAVITY: [f32; 3] = [0.0, -9.81, 0.0];

/// Half the side of the debris preview's floor, meters.
const DEBRIS_FLOOR_HALF: f32 = 500.0;

/// How far a chunk may sink into the preview floor before it counts as below it, meters. Rapier's
/// soft contacts let a heavy chunk (a brick of the 3 m test wall) sink up to about 5 cm for a step when it lands
/// or is pushed against the floor at 60 Hz; a chunk that falls through goes meters.
pub const FLOOR_TOLERANCE: f32 = 0.1;

/// Which chunks the world holds up.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum SupportPick {
    /// The cook's rule from the settings: `free`, `support_below_y` or the lowest centimetre. The
    /// only kind the mesh `.meta` stores.
    #[default]
    Rule,
    /// These chunks (viewport clicks or an agent's list). A new run clears them: indices name the
    /// chunks of one result.
    Chunks(BTreeSet<u32>),
    /// Chunks whose bounds touch this box (asset space).
    Box {
        /// Lower corner.
        min: [f32; 3],
        /// Upper corner.
        max: [f32; 3],
    },
}

/// What a click in the viewport does while the panel has a result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClickMode {
    /// Nothing: the viewport selects and moves as usual.
    #[default]
    Off,
    /// A click toggles the support flag of the chunk under the pointer.
    Supports,
    /// A click hits the test break at the point under the pointer.
    TestHit,
}

impl ClickMode {
    fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Supports => "supports",
            Self::TestHit => "test_hit",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "supports" => Some(Self::Supports),
            "test_hit" => Some(Self::TestHit),
            _ => None,
        }
    }
}

/// One hit of the test break, as given.
#[derive(Clone, Debug, PartialEq)]
pub struct TestHit {
    /// World-space centre.
    pub point: [f32; 3],
    /// Impulse at the centre, newton-seconds.
    pub impulse: f32,
    /// Radius, metres.
    pub radius: f32,
}

impl TestHit {
    /// The explosion this hit queues.
    #[must_use]
    pub fn damage(&self, target: DestructibleId) -> Damage {
        Damage {
            target: Some(target),
            kind: DamageKind::Explosion {
                center: self.point,
                radius: self.radius,
                impulse: self.impulse,
            },
        }
    }
}

/// The test break: the break model of the panel's result, as it would be saved.
#[derive(Clone, Debug)]
pub struct TestBreak {
    /// The world (server authority, so it decides breaks).
    pub world: DestructionWorld,
    /// The one instance.
    pub id: DestructibleId,
    /// Its asset (the same cook **Save** writes).
    pub asset: Arc<DestructibleAsset>,
    /// The hits so far, in order.
    pub hits: Vec<TestHit>,
    /// Chunks knocked loose by a hit.
    pub detached: BTreeSet<u32>,
    /// Chunks that fell because they lost every path to a support.
    pub collapsed: BTreeSet<u32>,
    /// The last debris preview of these hits (ADR-7745); a new hit clears it.
    pub debris: Option<DebrisPreview>,
}

impl TestBreak {
    /// A fresh test of `asset` at `origin`.
    ///
    /// # Errors
    /// `destruction_*` when the world refuses the asset or the pose.
    pub fn new(asset: Arc<DestructibleAsset>, origin: [f32; 3]) -> Result<Self, PanelError> {
        let mut world = DestructionWorld::new(Authority::Server);
        let id = world
            .define(Arc::clone(&asset), DestructiblePose::at(origin), TEST_SEED)
            .map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        Ok(Self {
            world,
            id,
            asset,
            hits: Vec::new(),
            detached: BTreeSet::new(),
            collapsed: BTreeSet::new(),
            debris: None,
        })
    }

    /// Runs the debris preview of the hits so far: `steps` fixed steps after them.
    ///
    /// # Errors
    /// `fracture_test_bad_steps` for 0 or more than [`MAX_DEBRIS_STEPS`]; `fracture_test_no_hits`
    /// before a hit; `destruction_*` when the preview world refuses the asset or a hit.
    pub fn run_debris(
        &mut self,
        origin: [f32; 3],
        steps: u32,
        floor_y: Option<f32>,
    ) -> Result<&DebrisPreview, PanelError> {
        if self.hits.is_empty() {
            return Err(PanelError::new(
                "fracture_test_no_hits",
                "hit the test break first: the preview runs the chunks the hits knock loose",
            ));
        }
        let preview = DebrisPreview::run(&self.asset, origin, &self.hits, steps, floor_y)?;
        Ok(&*self.debris.insert(preview))
    }

    /// Queues `hit` and runs [`TEST_TICKS`] fixed ticks.
    ///
    /// # Errors
    /// `destruction_*` for a hit the world refuses (a radius of 0, a negative impulse).
    pub fn hit(&mut self, hit: TestHit) -> Result<(), PanelError> {
        let _span = tracing::info_span!("editor.fracture_test_hit").entered();
        self.world
            .damage(hit.damage(self.id))
            .map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        for _ in 0..TEST_TICKS {
            let report = self.world.step(TEST_DT);
            for event in &report.events {
                if let BreakKind::ChunkDetached { chunk, cause, .. } = &event.kind {
                    match cause {
                        DetachCause::Damage => self.detached.insert(*chunk),
                        DetachCause::Collapse => self.collapsed.insert(*chunk),
                    };
                }
            }
            // No rapier here: the debris bodies are a game's; drop their requests.
            self.world.take_spawns(0);
        }
        self.hits.push(hit);
        self.debris = None;
        Ok(())
    }

    /// Bond indices that broke, ascending.
    #[must_use]
    pub fn broken_bonds(&self) -> Vec<u32> {
        self.world.instance(self.id).map_or_else(Vec::new, |i| {
            i.bond_broken()
                .iter()
                .enumerate()
                .filter(|(_, b)| **b)
                .map(|(n, _)| n as u32)
                .collect()
        })
    }

    /// Every chunk that left the structure, ascending.
    #[must_use]
    pub fn falling(&self) -> Vec<u32> {
        self.world.instance(self.id).map_or_else(Vec::new, |i| {
            i.chunk_states()
                .iter()
                .enumerate()
                .filter(|(_, s)| **s == ChunkState::Detached)
                .map(|(n, _)| n as u32)
                .collect()
        })
    }

    /// What the panel and `editor.fracture_test_hit` report.
    #[must_use]
    pub fn report(&self) -> Value {
        let broken = self.broken_bonds();
        let falling = self.falling();
        json!({
            "hits": self.hits.len(),
            "ticks": self.world.tick(),
            "bonds": self.asset.bonds.len(),
            "bonds_broken": broken.len(),
            "broken_bonds": broken.iter().map(|&b| {
                let bond = &self.asset.bonds[b as usize];
                json!({ "bond": b, "a": bond.a, "b": bond.b })
            }).collect::<Vec<_>>(),
            "chunks": self.asset.chunks.len(),
            "detached": self.detached,
            "collapsed": self.collapsed,
            "falling": falling,
            "attached": self.asset.chunks.len() - falling.len(),
            "structure_hash": format!("{:016x}", self.world.structure_hash()),
        })
    }
}

/// One chunk of a debris preview.
#[derive(Clone, Debug, PartialEq)]
pub struct DebrisChunk {
    /// Chunk index.
    pub chunk: u32,
    /// Its centre of mass where it sat in the structure (editor space).
    pub start: Vec3,
    /// Its centre of mass at the end (or where its body was last, if the budget expired it).
    pub translation: Vec3,
    /// Its rotation at the end.
    pub rotation: Quat,
    /// The lowest its collision hulls reached over the whole preview, meters above the floor.
    pub min_clearance: f32,
    /// It still had a body at the end (the bridge expires sleeping or old debris).
    pub live: bool,
}

/// The debris of a test break run under the physics sim (ADR-7745).
#[derive(Clone, Debug, PartialEq)]
pub struct DebrisPreview {
    /// Fixed steps run after the hits.
    pub steps: u32,
    /// The floor's height (the structure's lowest point, editor space).
    pub floor_y: f32,
    /// Every chunk that got a debris body, by index.
    pub chunks: Vec<DebrisChunk>,
    /// The preview's rapier state digest at the end: equal previews have equal digests.
    pub digest: u64,
}

impl DebrisPreview {
    /// Replays `hits` on `asset` at `origin` the way a game's world runs them: per fixed tick the
    /// destruction world steps, its [`DestructionPhysics`] bridge gives loose chunks their bodies,
    /// and the rapier world steps ([`TEST_DT`], [`DEBRIS_GRAVITY`]) on a fixed floor whose top is
    /// at `floor_y` (default: the structure's lowest point; lower for a structure that stands on
    /// something, a wall on a plinth or a bridge's deck); then `steps` more ticks.
    ///
    /// # Errors
    /// `fracture_test_bad_steps`, `fracture_test_bad_floor` for a floor that is not finite or is
    /// above the structure's lowest point, or the `destruction_*` code the world gives.
    pub fn run(
        asset: &Arc<DestructibleAsset>,
        origin: [f32; 3],
        hits: &[TestHit],
        steps: u32,
        floor_y: Option<f32>,
    ) -> Result<Self, PanelError> {
        let _span = tracing::info_span!("editor.fracture_test_debris", steps).entered();
        if steps == 0 || steps > MAX_DEBRIS_STEPS {
            return Err(PanelError::new(
                "fracture_test_bad_steps",
                format!("`steps` must be 1 to {MAX_DEBRIS_STEPS}"),
            ));
        }
        let destruction = |e: DestructionError| PanelError::new(e.code(), e.to_string());
        let lowest = asset
            .chunks
            .iter()
            .map(|c| c.bounds_min[1])
            .fold(f32::INFINITY, f32::min);
        let bottom = origin[1] + if lowest.is_finite() { lowest } else { 0.0 };
        let floor_y = match floor_y {
            None => bottom,
            // At most the lowest point, exactly as the message and the book say (review
            // sprint-w11-rev-c-k V2): `FLOOR_TOLERANCE` is only the clearance check's sink allowance.
            Some(y) if y.is_finite() && y <= bottom => y,
            Some(y) => {
                return Err(PanelError::new(
                    "fracture_test_bad_floor",
                    format!("`floor_y` {y} must be finite and at most the structure's lowest point {bottom}"),
                ));
            }
        };
        let mut physics = PhysicsWorld::default();
        add_fixed_box(
            &mut physics,
            [origin[0], floor_y - 0.5, origin[2]],
            [DEBRIS_FLOOR_HALF, 0.5, DEBRIS_FLOOR_HALF],
        );
        let mut world = DestructionWorld::new(Authority::Server);
        let id = world
            .define(Arc::clone(asset), DestructiblePose::at(origin), TEST_SEED)
            .map_err(destruction)?;
        let mut bridge = DestructionPhysics::default();
        bridge.attach(&mut physics, &world, id).map_err(destruction)?;
        let mut chunks: std::collections::BTreeMap<u32, DebrisChunk> = std::collections::BTreeMap::new();
        let tick = |world: &mut DestructionWorld,
                    bridge: &mut DestructionPhysics,
                    physics: &mut PhysicsWorld,
                    chunks: &mut std::collections::BTreeMap<u32, DebrisChunk>| {
            world.step(TEST_DT);
            let report = bridge.sync(physics, world, TEST_DT);
            step_standalone(physics, TEST_DT, DEBRIS_GRAVITY);
            for (_, chunk) in report.spawned.iter().filter(|(d, _)| *d == id) {
                let start = asset.chunks.get(*chunk as usize).map_or(Vec3::ZERO, |c| {
                    Vec3::from_array(c.centroid) + Vec3::from_array(origin)
                });
                chunks.entry(*chunk).or_insert(DebrisChunk {
                    chunk: *chunk,
                    start,
                    translation: start,
                    rotation: Quat::IDENTITY,
                    min_clearance: f32::INFINITY,
                    live: true,
                });
            }
            for (index, out) in chunks.iter_mut() {
                let Some((t, q)) = bridge.debris_pose(physics, id, *index) else {
                    out.live = false;
                    continue;
                };
                out.live = true;
                out.translation = t;
                out.rotation = q;
                let hull_low = asset.chunks.get(*index as usize).map_or(t.y, |c| {
                    c.hulls
                        .iter()
                        .flatten()
                        .map(|p| (t + q * Vec3::from_array(*p)).y)
                        .fold(f32::INFINITY, f32::min)
                });
                out.min_clearance = out.min_clearance.min(hull_low - floor_y);
            }
        };
        for hit in hits {
            world.damage(hit.damage(id)).map_err(destruction)?;
            for _ in 0..TEST_TICKS {
                tick(&mut world, &mut bridge, &mut physics, &mut chunks);
            }
        }
        for _ in 0..steps {
            tick(&mut world, &mut bridge, &mut physics, &mut chunks);
        }
        Ok(Self {
            steps,
            floor_y,
            chunks: chunks.into_values().collect(),
            digest: physics.state_digest(),
        })
    }

    /// Chunks whose centre of mass ended more than `distance` meters from where it sat.
    #[must_use]
    pub fn moved(&self, distance: f32) -> usize {
        self.chunks
            .iter()
            .filter(|c| c.translation.distance(c.start) > distance)
            .count()
    }

    /// Every chunk stayed above the floor (within [`FLOOR_TOLERANCE`]) on every step.
    #[must_use]
    pub fn above_floor(&self) -> bool {
        self.chunks.iter().all(|c| c.min_clearance >= -FLOOR_TOLERANCE)
    }

    /// What `editor.fracture_test_debris` and the panel report.
    #[must_use]
    pub fn report(&self) -> Value {
        let lowest = self
            .chunks
            .iter()
            .map(|c| c.min_clearance)
            .fold(f32::INFINITY, f32::min);
        json!({
            "steps": self.steps,
            "floor_y": self.floor_y,
            "chunks": self.chunks.len(),
            "moved": self.moved(0.05),
            "above_floor": self.above_floor(),
            "min_clearance": if lowest.is_finite() { Some(lowest) } else { None },
            "digest": format!("{:016x}", self.digest),
            "debris": self.chunks.iter().map(|c| json!({
                "chunk": c.chunk,
                "start": c.start.to_array(),
                "end": c.translation.to_array(),
                "rotation": c.rotation.to_array(),
                "min_clearance": c.min_clearance,
                "live": c.live,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Edges of each debris chunk's box at its preview pose (empty without a preview).
#[must_use]
pub fn debris_segments(panel: &FracturePanel) -> Vec<Segment> {
    let (Some(test), true) = (panel.test.as_ref(), panel.show_gizmo) else {
        return Vec::new();
    };
    let Some(preview) = test.debris.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(preview.chunks.len() * 12);
    for c in &preview.chunks {
        let Some(chunk) = test.asset.chunks.get(c.chunk as usize) else {
            continue;
        };
        let centroid = Vec3::from_array(chunk.centroid);
        let lo = Vec3::from_array(chunk.bounds_min) - centroid;
        let hi = Vec3::from_array(chunk.bounds_max) - centroid;
        let corner = |i: usize| {
            let p = Vec3::new(
                if i & 1 == 0 { lo.x } else { hi.x },
                if i & 2 == 0 { lo.y } else { hi.y },
                if i & 4 == 0 { lo.z } else { hi.z },
            );
            c.translation + c.rotation * p
        };
        for i in 0..8usize {
            for bit in [1usize, 2, 4] {
                if i & bit == 0 {
                    out.push((corner(i), corner(i | bit), DEBRIS_EDGES));
                }
            }
        }
    }
    out
}

/// `editor.fracture_test_debris` / **Run debris**: runs the preview of the test's hits on a floor
/// at `floor_y` (default: the structure's lowest point).
///
/// # Errors
/// `fracture_test_no_hits` before a hit, and the codes of [`TestBreak::run_debris`].
pub fn test_debris(panel: &mut FracturePanel, steps: u32, floor_y: Option<f32>) -> Result<Value, PanelError> {
    let origin = panel.origin;
    let test = panel.test.as_mut().ok_or_else(|| {
        PanelError::new(
            "fracture_test_no_hits",
            "hit the test break first: the preview runs the chunks the hits knock loose",
        )
    })?;
    Ok(test.run_debris(origin, steps, floor_y)?.report())
}

/// The support rule [`SupportPick`] gives for `set`.
#[must_use]
pub fn support_rule(panel: &FracturePanel, set: &FractureSet) -> SupportRule {
    match &panel.supports {
        SupportPick::Rule => destructible_cook_settings("rule", &panel.settings).support,
        SupportPick::Chunks(list) => SupportRule::Explicit(list.iter().copied().collect()),
        SupportPick::Box { min, max } => SupportRule::Explicit(
            set.chunks
                .iter()
                .enumerate()
                .filter(|(_, c)| (0..3).all(|k| c.bounds_min[k] <= max[k] && c.bounds_max[k] >= min[k]))
                .map(|(i, _)| i as u32)
                .collect(),
        ),
    }
}

/// Cooks the panel's result as **Save** writes it, named `name`.
///
/// # Errors
/// `fracture_panel_no_result` before a run, `destruction_*` when the cook refuses the settings.
pub fn cook_asset(panel: &FracturePanel, name: &str) -> Result<DestructibleAsset, PanelError> {
    let Some(set) = &panel.result else {
        return Err(PanelError::new(
            "fracture_panel_no_result",
            "run the fracture first",
        ));
    };
    let mut cook = destructible_cook_settings(name, &panel.settings);
    cook.support = support_rule(panel, set);
    cook_destructible(set, &cook).map_err(|e| PanelError::new(e.code(), e.to_string()))
}

/// The support flag of each chunk of the panel's result under its current pick.
#[must_use]
pub fn support_flags(panel: &FracturePanel) -> Vec<bool> {
    let Some(set) = &panel.result else {
        return Vec::new();
    };
    let floor = set
        .chunks
        .iter()
        .map(|c| c.bounds_min[1])
        .fold(f32::INFINITY, f32::min);
    match support_rule(panel, set) {
        SupportRule::None => vec![false; set.chunks.len()],
        SupportRule::BelowY(y) => {
            let y = if y == f32::NEG_INFINITY { floor + 0.01 } else { y };
            set.chunks.iter().map(|c| c.bounds_min[1] <= y).collect()
        }
        SupportRule::Explicit(list) => (0..set.chunks.len() as u32).map(|i| list.contains(&i)).collect(),
    }
}

/// Toggles `chunk`'s support flag: the first toggle turns the current rule into a pick.
///
/// # Errors
/// `fracture_panel_no_result` before a run, `fracture_panel_bad_chunk` for an index past the set.
pub fn toggle_support(panel: &mut FracturePanel, chunk: u32) -> Result<(), PanelError> {
    let flags = support_flags(panel);
    if panel.result.is_none() {
        return Err(PanelError::new(
            "fracture_panel_no_result",
            "run the fracture first",
        ));
    }
    if chunk as usize >= flags.len() {
        return Err(PanelError::new(
            "fracture_panel_bad_chunk",
            format!("chunk {chunk} is outside the {} chunks", flags.len()),
        ));
    }
    let mut list: BTreeSet<u32> = flags
        .iter()
        .enumerate()
        .filter(|(_, f)| **f)
        .map(|(i, _)| i as u32)
        .collect();
    if !list.remove(&chunk) {
        list.insert(chunk);
    }
    panel.supports = SupportPick::Chunks(list);
    panel.test = None;
    Ok(())
}

/// Runs one test hit, building the test break first when there is none.
///
/// # Errors
/// `fracture_panel_no_result` before a run, `destruction_*` from the cook or the hit.
pub fn test_hit(panel: &mut FracturePanel, hit: TestHit) -> Result<Value, PanelError> {
    if panel.test.is_none() {
        let asset = cook_asset(panel, "fracture_test")?;
        panel.test = Some(TestBreak::new(Arc::new(asset), panel.origin)?);
    }
    let Some(test) = panel.test.as_mut() else {
        return Err(PanelError::new(
            "fracture_panel_no_result",
            "run the fracture first",
        ));
    };
    test.hit(hit)?;
    Ok(test.report())
}

/// One bond as the bond view shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct BondInfo {
    /// Bond index.
    pub bond: u32,
    /// Lower chunk.
    pub a: u32,
    /// Higher chunk.
    pub b: u32,
    /// Break impulse, newton-seconds.
    pub break_impulse: f32,
    /// Contact area, square metres.
    pub area: f32,
    /// Anchor, asset space.
    pub anchor: [f32; 3],
    /// Unit normal from `a` to `b`.
    pub normal: [f32; 3],
}

impl BondInfo {
    /// The name the viewport hover and the list give it.
    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "bond {}: chunk {} - chunk {}, {:.0} N s",
            self.bond, self.a, self.b, self.break_impulse
        )
    }

    fn json(&self, broken: bool) -> Value {
        json!({
            "bond": self.bond, "a": self.a, "b": self.b,
            "break_impulse": self.break_impulse, "area": self.area,
            "anchor": self.anchor, "normal": self.normal, "broken": broken,
        })
    }
}

/// What one bond cook was made from: the run (by its revision and a cheap identity of the set,
/// so a `result` written outside a run is seen too), the settings and the support pick.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BondKey {
    result_rev: u64,
    chunks: (usize, usize, usize),
    settings: regia_asset_pipeline::fracture_cook::FractureCookSettings,
    supports: SupportPick,
}

impl BondKey {
    fn of(panel: &FracturePanel) -> Option<Self> {
        let set = panel.result.as_ref()?;
        Some(Self {
            result_rev: panel.result_rev,
            chunks: (set.chunks.len(), set.joints.len(), set.chunks.as_ptr() as usize),
            settings: panel.settings.clone(),
            supports: panel.supports.clone(),
        })
    }
}

/// The bond view's last cook (ADR-7451 (4)): the bond list, the viewport heat view, hover picks
/// and `editor.fracture_bonds` call [`bonds`] several times a frame; the cook runs only when its
/// [`BondKey`] changed.
#[derive(Debug, Default)]
pub(crate) struct BondCache {
    /// Cooks so far ([`FracturePanel::bond_cooks`]).
    pub(crate) cooks: u64,
    /// The key of the cached bonds.
    key: Option<BondKey>,
    /// The bonds that key cooked to (empty when the cook refused the settings).
    bonds: Vec<BondInfo>,
}

fn bond_infos(asset: &DestructibleAsset) -> Vec<BondInfo> {
    asset
        .bonds
        .iter()
        .enumerate()
        .map(|(i, b)| BondInfo {
            bond: i as u32,
            a: b.a,
            b: b.b,
            break_impulse: b.break_impulse,
            area: b.area,
            anchor: b.anchor,
            normal: b.normal,
        })
        .collect()
}

/// The bonds of the panel's result as **Save** would cook them (empty before a run or when the
/// cook refuses the settings). The cook is cached by the run, the settings and the support pick
/// ([`FracturePanel::bond_cooks`] counts the cooks); with a test break the test's own asset is
/// read.
#[must_use]
pub fn bonds(panel: &FracturePanel) -> Vec<BondInfo> {
    if let Some(test) = &panel.test {
        return bond_infos(&test.asset);
    }
    let Some(key) = BondKey::of(panel) else {
        return Vec::new();
    };
    let mut cache = panel
        .bond_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.key.as_ref() != Some(&key) {
        let _span = tracing::info_span!(target: "regia_editor", "fracture_bond_cook").entered();
        cache.cooks += 1;
        cache.bonds = cook_asset(panel, "fracture_bonds")
            .map(|asset| bond_infos(&asset))
            .unwrap_or_default();
        cache.key = Some(key);
    }
    cache.bonds.clone()
}

/// The `n` weakest bonds: ascending break impulse, then index.
#[must_use]
pub fn weakest(bonds: &[BondInfo], n: usize) -> Vec<BondInfo> {
    let mut sorted = bonds.to_vec();
    sorted.sort_by(|x, y| {
        x.break_impulse
            .total_cmp(&y.break_impulse)
            .then(x.bond.cmp(&y.bond))
    });
    sorted.truncate(n);
    sorted
}

/// Heat color of `t` in 0..=1: red (weakest) through yellow to cyan (strongest).
#[must_use]
pub fn heat_color(t: f32) -> egui::Color32 {
    let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.0 };
    let lerp = |a: u8, b: u8, s: f32| (f32::from(a) + (f32::from(b) - f32::from(a)) * s).round() as u8;
    if t < 0.5 {
        let s = t * 2.0;
        egui::Color32::from_rgb(lerp(235, 240, s), lerp(50, 220, s), lerp(40, 60, s))
    } else {
        let s = (t - 0.5) * 2.0;
        egui::Color32::from_rgb(lerp(240, 70, s), lerp(220, 220, s), lerp(60, 235, s))
    }
}

/// How far the preview moves `chunk` (the explode shift plus the origin).
fn chunk_shift(set: &FractureSet, explode: f32, origin: Vec3, chunk: usize) -> Vec3 {
    let middle = set
        .chunks
        .iter()
        .map(|c| Vec3::from_array(c.centroid))
        .fold(Vec3::ZERO, |a, b| a + b)
        / set.chunks.len().max(1) as f32;
    set.chunks.get(chunk).map_or(origin, |c| {
        origin + (Vec3::from_array(c.centroid) - middle) * explode
    })
}

/// The color each chunk's edges take: test state first, then the support flag.
#[must_use]
pub fn chunk_colors(panel: &FracturePanel) -> Vec<egui::Color32> {
    let flags = support_flags(panel);
    flags
        .iter()
        .enumerate()
        .map(|(i, support)| {
            let i = i as u32;
            match &panel.test {
                Some(t) if t.detached.contains(&i) => DETACHED_EDGES,
                Some(t) if t.collapsed.contains(&i) => COLLAPSED_EDGES,
                _ if *support => SUPPORT_EDGES,
                _ => crate::fracture_panel::FRACTURE_EDGES,
            }
        })
        .collect()
}

/// The bond view's segments: one per bond across its face, along the bond's normal, colored by
/// heat (broken bonds grey, the hovered bond white). Empty with the view off.
#[must_use]
pub fn bond_segments(panel: &FracturePanel) -> Vec<Segment> {
    let (Some(set), true) = (&panel.result, panel.show_bonds) else {
        return Vec::new();
    };
    let list = bonds(panel);
    let (lo, hi) = list.iter().fold((f32::INFINITY, 0.0f32), |(lo, hi), b| {
        (lo.min(b.break_impulse), hi.max(b.break_impulse))
    });
    let broken: BTreeSet<u32> = panel
        .test
        .as_ref()
        .map(|t| t.broken_bonds().into_iter().collect())
        .unwrap_or_default();
    let origin = Vec3::from_array(panel.origin);
    let mut out = Vec::with_capacity(list.len());
    for bond in &list {
        let (sa, sb) = (
            chunk_shift(set, panel.explode, origin, bond.a as usize),
            chunk_shift(set, panel.explode, origin, bond.b as usize),
        );
        let anchor = Vec3::from_array(bond.anchor) + (sa + sb) * 0.5;
        let half = Vec3::from_array(bond.normal) * (0.25 * bond.area.max(1e-6).sqrt()).clamp(0.02, 0.5);
        let color = if panel.hover_bond == Some(bond.bond) {
            HOVERED_BOND
        } else if broken.contains(&bond.bond) {
            BROKEN_BOND
        } else if hi > lo {
            heat_color((bond.break_impulse - lo) / (hi - lo))
        } else {
            heat_color(0.0)
        };
        out.push((anchor - half, anchor + half, color));
    }
    out
}

/// Where a world ray first enters a chunk's (shifted) bounds: the chunk and the point in the
/// solid's unshifted space, which is where a test hit lands.
#[must_use]
pub fn pick_chunk(panel: &FracturePanel, origin: Vec3, direction: Vec3) -> Option<(u32, Vec3)> {
    let set = panel.result.as_ref()?;
    let base = Vec3::from_array(panel.origin);
    let mut best: Option<(f32, u32, Vec3)> = None;
    for (i, chunk) in set.chunks.iter().enumerate() {
        if panel
            .test
            .as_ref()
            .is_some_and(|t| t.falling().contains(&(i as u32)))
        {
            continue;
        }
        let shift = chunk_shift(set, panel.explode, base, i);
        let lo = Vec3::from_array(chunk.bounds_min) + shift;
        let hi = Vec3::from_array(chunk.bounds_max) + shift;
        let Some(t) = ray_box(origin, direction, lo, hi) else {
            continue;
        };
        if best.is_none_or(|(bt, ..)| t < bt) {
            // The point in the unexploded solid: the test break sits at `origin` unshifted.
            let point = origin + direction * t - shift + base;
            best = Some((t, i as u32, point));
        }
    }
    best.map(|(_, i, p)| (i, p))
}

/// The bond whose anchor is nearest the ray within `reach` metres.
#[must_use]
pub fn pick_bond(panel: &FracturePanel, origin: Vec3, direction: Vec3, reach: f32) -> Option<u32> {
    let set = panel.result.as_ref()?;
    let base = Vec3::from_array(panel.origin);
    let dir = direction.normalize_or_zero();
    bonds(panel)
        .iter()
        .filter_map(|b| {
            let (sa, sb) = (
                chunk_shift(set, panel.explode, base, b.a as usize),
                chunk_shift(set, panel.explode, base, b.b as usize),
            );
            let p = Vec3::from_array(b.anchor) + (sa + sb) * 0.5;
            let along = (p - origin).dot(dir).max(0.0);
            let d = (origin + dir * along - p).length();
            (d <= reach).then_some((d, b.bond))
        })
        .min_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)))
        .map(|(_, b)| b)
}

/// Slab test: the entry distance of a ray into an axis-aligned box (0 when the origin is inside).
fn ray_box(origin: Vec3, direction: Vec3, lo: Vec3, hi: Vec3) -> Option<f32> {
    let (mut t0, mut t1) = (0.0f32, f32::INFINITY);
    for k in 0..3 {
        let (o, d) = (origin[k], direction[k]);
        if d.abs() < 1e-12 {
            if o < lo[k] || o > hi[k] {
                return None;
            }
            continue;
        }
        let (a, b) = ((lo[k] - o) / d, (hi[k] - o) / d);
        t0 = t0.max(a.min(b));
        t1 = t1.min(a.max(b));
        if t0 > t1 {
            return None;
        }
    }
    Some(t0)
}

/// Spawns an entity with a [`DestructibleSource`] for the last saved `.destructible.ron` at `at`
/// (the viewport drop point when `None`): one undo step, saved with the level.
///
/// # Errors
/// `fracture_panel_not_saved` before a **Save**, `fracture_panel_place` when the entity cannot be
/// built (the component type is not registered).
pub fn place(world: &mut World, at: Option<Vec3>) -> Result<(Entity, Guid, String), PanelError> {
    let saved = world
        .get_resource::<FracturePanel>()
        .and_then(|p| p.saved.clone())
        .ok_or_else(|| PanelError::new("fracture_panel_not_saved", "save the fracture before placing it"))?;
    let position = at.unwrap_or_else(|| crate::content::drop_position(world).0);
    let name = std::path::Path::new(&saved)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".destructible.ron"))
        .filter(|n| !n.is_empty())
        .unwrap_or("Destructible")
        .to_string();
    let source = DestructibleSource {
        asset: saved,
        seed: TEST_SEED,
    };
    let guid = Guid::new_random();
    let mut command = SpawnEntities::new_entity(world, guid, &name, Transform::from_translation(position))
        .and_then(|c| c.with_component(world, source.as_partial_reflect()))
        .map_err(|e| PanelError::new("fracture_panel_place", e.to_string()))?;
    command.apply(world);
    let entity = world
        .query::<(Entity, &regia_scene::prelude::EntityGuid)>()
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(e, _)| e)
        .ok_or_else(|| PanelError::new("fracture_panel_place", "the placed entity did not spawn"))?;
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    Ok((entity, guid, name))
}

/// Cell-centre sites of an `nx x ny x nz` grid filling a box of `half` extents: the Voronoi cells
/// of these sites are the grid's cells, so a 3x3x1 grid cuts a wall into nine bricks, numbered
/// x fastest, then y, then z (the bottom row first).
///
/// # Errors
/// `fracture_panel_bad_grid` for a zero count or more than `MAX_CHUNKS` cells.
pub fn grid_sites(half: [f32; 3], counts: [u32; 3]) -> Result<Vec<[f32; 3]>, PanelError> {
    let total = counts.iter().map(|&c| c as usize).product::<usize>();
    if counts.contains(&0) || total > regia_asset_pipeline::fracture::MAX_CHUNKS {
        return Err(PanelError::new(
            "fracture_panel_bad_grid",
            format!(
                "a grid needs 1 to {} cells, at least one per axis",
                regia_asset_pipeline::fracture::MAX_CHUNKS
            ),
        ));
    }
    let mut out = Vec::with_capacity(total);
    let centre = |k: usize, i: u32| -half[k] + (2.0 * half[k]) * (i as f32 + 0.5) / counts[k] as f32;
    for z in 0..counts[2] {
        for y in 0..counts[1] {
            for x in 0..counts[0] {
                out.push([centre(0, x), centre(1, y), centre(2, z)]);
            }
        }
    }
    Ok(out)
}

/// A `[x, y, z]` parameter: three finite numbers.
fn vec3(v: &Value, key: &str) -> Result<[f32; 3], RpcError> {
    v.as_array()
        .filter(|a| a.len() == 3)
        .and_then(|a| {
            let f = |i: usize| a[i].as_f64().filter(|x| x.is_finite()).map(|x| x as f32);
            Some([f(0)?, f(1)?, f(2)?])
        })
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be [x, y, z]")))
}

/// The `impact` parameter of `editor.fracture_panel_set` (ADR-9870): `{ at?: [x, y, z],
/// bias?: 0|1|2 }`, both optional (a fresh choice starts at the origin — the box source's
/// centre — with bias 1; a field left out keeps what the panel holds).
fn parse_impact(
    v: &Value,
    current: Option<&crate::fracture_panel::ImpactSites>,
) -> Result<crate::fracture_panel::ImpactSites, RpcError> {
    let bad = |what: &str| PanelError::new("fracture_panel_bad_impact", what.to_string()).rpc();
    let Some(o) = v.as_object() else {
        return Err(RpcError::invalid_params("`impact` must be { at, bias }"));
    };
    let mut impact = current.cloned().unwrap_or_default();
    if let Some(at) = o.get("at").filter(|a| !a.is_null()) {
        let at = vec3(at, "impact.at")?;
        if !at.iter().all(|c| c.is_finite()) {
            return Err(bad("`impact.at` must be finite"));
        }
        impact.at = at;
    }
    if let Some(bias) = o.get("bias").filter(|b| !b.is_null()) {
        let bias = bias
            .as_f64()
            .filter(|b| b.is_finite())
            .map(|b| b as f32)
            .ok_or_else(|| RpcError::invalid_params("`impact.bias` must be a number"))?;
        if ![0.0, 1.0, 2.0].contains(&bias) {
            return Err(bad("`impact.bias` must be 0, 1 or 2"));
        }
        impact.bias = bias;
    }
    Ok(impact)
}

/// The `clustered` parameter of `editor.fracture_panel_set` (ADR-9870): `{ clusters?: n,
/// spread?: s }`, both optional (a fresh choice starts at 4 clumps, spread 0.25; a field left out
/// keeps what the panel holds).
fn parse_clustered(
    v: &Value,
    chunks: usize,
    current: Option<&crate::fracture_panel::ClusterSites>,
) -> Result<crate::fracture_panel::ClusterSites, RpcError> {
    let bad = |what: &str| PanelError::new("fracture_panel_bad_clusters", what.to_string()).rpc();
    let Some(o) = v.as_object() else {
        return Err(RpcError::invalid_params(
            "`clustered` must be { clusters, spread }",
        ));
    };
    let mut clustered = current.cloned().unwrap_or_default();
    if let Some(n) = o.get("clusters").filter(|n| !n.is_null()) {
        let n = n
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| RpcError::invalid_params("`clustered.clusters` must be an integer"))?;
        if n == 0 || n > chunks {
            return Err(bad("`clustered.clusters` must be 1 to the chunk count"));
        }
        clustered.clusters = n;
    }
    if let Some(s) = o.get("spread").filter(|s| !s.is_null()) {
        let s = s
            .as_f64()
            .filter(|s| s.is_finite())
            .map(|s| s as f32)
            .ok_or_else(|| RpcError::invalid_params("`clustered.spread` must be a number"))?;
        if !((regia_asset_pipeline::fracture::MIN_CLUSTER_SPREAD..=1.0).contains(&s)) {
            return Err(bad("`clustered.spread` must be in [0.001, 1]"));
        }
        clustered.spread = s;
    }
    Ok(clustered)
}

/// Draws the supports, test break, bond and place sections under the panel's result. Returns
/// whether **Place** was clicked (the caller places with the world in hand).
pub fn draw_tools(ui: &mut egui::Ui, panel: &mut FracturePanel) -> bool {
    let mut place_clicked = false;
    let flags = support_flags(panel);
    ui.separator();
    ui.horizontal(|ui| {
        ui.strong("Supports");
        ui.weak(format!(
            "{} of {} chunks held by the world",
            flags.iter().filter(|f| **f).count(),
            flags.len()
        ));
    });
    ui.horizontal(|ui| {
        let kind = match panel.supports {
            SupportPick::Rule => 0,
            SupportPick::Chunks(_) => 1,
            SupportPick::Box { .. } => 2,
        };
        if ui
            .radio(kind == 0, "Rule")
            .on_hover_text("free / support_below_y / lowest centimetre")
            .clicked()
        {
            panel.supports = SupportPick::Rule;
            panel.test = None;
        }
        if ui.radio(kind == 1, "Picked").clicked() && kind != 1 {
            panel.supports = SupportPick::Chunks(
                flags
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| **f)
                    .map(|(i, _)| i as u32)
                    .collect(),
            );
            panel.test = None;
        }
        if ui.radio(kind == 2, "Box").clicked() && kind != 2 {
            panel.supports = SupportPick::Box {
                min: [-1.0e3, -1.0e3, -1.0e3],
                max: [1.0e3, 0.0, 1.0e3],
            };
            panel.test = None;
        }
        let mut picking = panel.click == ClickMode::Supports;
        if ui
            .toggle_value(&mut picking, "Click chunks")
            .on_hover_text("click a chunk in the viewport to toggle its support flag")
            .changed()
        {
            panel.click = if picking {
                ClickMode::Supports
            } else {
                ClickMode::Off
            };
        }
    });
    if let SupportPick::Box { min, max } = &mut panel.supports {
        let mut changed = false;
        for (label, corner) in [("Box min", min), ("Box max", max)] {
            ui.horizontal(|ui| {
                ui.label(label);
                for v in corner.iter_mut() {
                    changed |= ui.add(egui::DragValue::new(v).speed(0.01)).changed();
                }
            });
        }
        if changed {
            panel.test = None;
        }
    }
    if matches!(panel.supports, SupportPick::Rule) {
        ui.horizontal(|ui| {
            let mut plane = panel.settings.support_below_y.is_some();
            if ui
                .checkbox(&mut plane, "Plane: chunks below y")
                .on_hover_text("stored in the mesh .meta, so it survives a re-cook")
                .changed()
            {
                panel.settings.support_below_y = plane.then_some(0.0);
                panel.test = None;
            }
            if let Some(y) = panel.settings.support_below_y.as_mut()
                && ui.add(egui::DragValue::new(y).speed(0.01)).changed()
            {
                panel.test = None;
            }
        });
    }

    ui.separator();
    ui.horizontal(|ui| {
        ui.strong("Test break");
        let mut hitting = panel.click == ClickMode::TestHit;
        if ui
            .toggle_value(&mut hitting, "Click to hit")
            .on_hover_text("click the preview to hit it with an explosion")
            .changed()
        {
            panel.click = if hitting {
                ClickMode::TestHit
            } else {
                ClickMode::Off
            };
        }
        if ui
            .add_enabled(panel.test.is_some(), egui::Button::new("Reset"))
            .clicked()
        {
            panel.test = None;
        }
        // editor.fracture_test_debris (ADR-7745).
        let hit = panel.test.as_ref().is_some_and(|t| !t.hits.is_empty());
        if ui
            .add_enabled(hit, egui::Button::new("Run debris"))
            .on_hover_text("run the loose chunks under the physics sim for one second")
            .clicked()
            && let Err(e) = test_debris(panel, DEBRIS_STEPS, None)
        {
            panel.error = Some(e);
        }
    });
    ui.horizontal(|ui| {
        ui.label("Impulse (N s)");
        ui.add(
            egui::DragValue::new(&mut panel.hit_impulse)
                .speed(100.0)
                .range(0.0..=1.0e9),
        );
        ui.label("Radius (m)");
        ui.add(
            egui::DragValue::new(&mut panel.hit_radius)
                .speed(0.01)
                .range(0.001..=1000.0),
        );
    });
    if let Some(test) = &panel.test {
        let r = test.report();
        ui.label(format!(
            "{} hits: {} of {} bonds broken, {} knocked loose, {} collapsed, {} falling",
            r["hits"],
            r["bonds_broken"],
            r["bonds"],
            test.detached.len(),
            test.collapsed.len(),
            r["falling"].as_array().map_or(0, Vec::len)
        ));
        ui.monospace(format!(
            "structure_hash {}",
            r["structure_hash"].as_str().unwrap_or("")
        ));
        if let Some(debris) = &test.debris {
            ui.label(format!(
                "debris: {} chunks over {} steps, {} moved, {}",
                debris.chunks.len(),
                debris.steps,
                debris.moved(0.05),
                if debris.above_floor() {
                    "all above the floor"
                } else {
                    "some went through the floor"
                }
            ));
        }
    }

    ui.separator();
    ui.horizontal(|ui| {
        ui.strong("Bonds");
        ui.checkbox(&mut panel.show_bonds, "Heat view")
            .on_hover_text("bonds colored by break impulse: red is the weakest");
    });
    let list = bonds(panel);
    if let Some(b) = panel.hover_bond.and_then(|h| list.iter().find(|b| b.bond == h)) {
        ui.weak(b.label());
    } else if panel.show_bonds {
        // The line stays when nothing is hovered, so the list below does not jump.
        ui.weak("hover a bond to name it");
    }
    let mut hovered = None;
    if panel.show_bonds {
        ui.label("Weakest bonds");
        for b in weakest(&list, WEAKEST_BONDS) {
            let row = ui.selectable_label(panel.hover_bond == Some(b.bond), b.label());
            if row.hovered() {
                hovered = Some(b.bond);
            }
        }
    }
    // Nothing hovered in the list (or no list) clears the list's hover (ADR-7451 (3)).
    panel.set_hover(true, hovered);

    ui.separator();
    ui.horizontal(|ui| {
        let can = panel.saved.is_some();
        place_clicked = ui
            .add_enabled(can, egui::Button::new("Place"))
            .on_hover_text("spawn the saved destructible at the viewport drop point")
            .on_disabled_hover_text("save first")
            .clicked();
        if let Some(saved) = &panel.saved {
            ui.weak(saved.as_str());
        }
    });
    place_clicked
}

/// Viewport input while the panel has a click mode on: a primary click picks a support chunk or
/// hits the test break; with the bond view on, hovering names the nearest bond. Returns whether the
/// panel owns the pointer (the viewport then skips box select and picking).
pub fn viewport_input(
    ui: &egui::Ui,
    world: &mut World,
    response: &egui::Response,
    camera: &crate::viewport::EditorCamera,
    size: regia_core::math::Vec2,
) -> bool {
    let Some(panel) = world.get_resource::<FracturePanel>() else {
        return false;
    };
    if panel.result.is_none() || !panel.show_gizmo {
        // The gizmo off (or no result) leaves no bond hovered in the viewport (rev-claude-57
        // editor-panels finding 1).
        if panel.hover_view.is_some() {
            world.resource_mut::<FracturePanel>().set_hover(false, None);
        }
        return false;
    }
    let (click, bonds_on) = (panel.click, panel.show_bonds);
    let ray = response.hover_pos().map(|pointer| {
        let local = pointer - response.rect.left_top();
        crate::viewport::pick_ray(camera, regia_core::math::Vec2::new(local.x, local.y), size)
    });
    if bonds_on {
        let hovered = ray.and_then(|(o, d)| pick_bond(panel, o, d, 0.15));
        let label = hovered.and_then(|h| bonds(panel).into_iter().find(|b| b.bond == h).map(|b| b.label()));
        if let (Some(label), Some(pos)) = (label, response.hover_pos()) {
            ui.painter().text(
                pos + egui::vec2(12.0, -12.0),
                egui::Align2::LEFT_BOTTOM,
                label,
                egui::FontId::proportional(12.0),
                HOVERED_BOND,
            );
        }
        // The pointer off every bond (or off the viewport) clears the viewport's hover
        // (ADR-7451 (3)); the resource is written only when the hover changes.
        if panel.hover_view != hovered {
            world.resource_mut::<FracturePanel>().set_hover(false, hovered);
        }
    } else if panel.hover_view.is_some() {
        world.resource_mut::<FracturePanel>().set_hover(false, None);
    }
    if click == ClickMode::Off {
        return false;
    }
    let pressed = ui.input(|i| i.pointer.primary_pressed());
    if pressed
        && response.hovered()
        && let Some((origin, direction)) = ray
    {
        let mut panel = world.resource_mut::<FracturePanel>();
        if let Some((chunk, point)) = pick_chunk(&panel, origin, direction) {
            let result = match click {
                ClickMode::Supports => toggle_support(&mut panel, chunk),
                ClickMode::TestHit => {
                    let hit = TestHit {
                        point: point.to_array(),
                        impulse: panel.hit_impulse,
                        radius: panel.hit_radius,
                    };
                    test_hit(&mut panel, hit).map(|_| ())
                }
                ClickMode::Off => Ok(()),
            };
            panel.error = result.err();
        }
    }
    true
}

/// Parses `supports`: `"rule"`, `"none"`, a chunk list, `{ below_y }` (`null` clears the plane),
/// or `{ box: [[min], [max]] }`.
pub(crate) fn parse_supports(panel: &mut FracturePanel, v: &Value) -> Result<(), RpcError> {
    let vec3 = |v: &Value| -> Option<[f32; 3]> {
        let a = v.as_array().filter(|a| a.len() == 3)?;
        let f = |i: usize| a[i].as_f64().filter(|x| x.is_finite()).map(|x| x as f32);
        Some([f(0)?, f(1)?, f(2)?])
    };
    let bad = || {
        RpcError::invalid_params(
            "`supports` must be \"rule\", \"none\", [chunk, ...], { below_y } (null clears the plane) \
             or { box: [[min], [max]] }",
        )
    };
    match v {
        Value::String(s) if s == "rule" => panel.supports = SupportPick::Rule,
        Value::String(s) if s == "none" => {
            panel.supports = SupportPick::Rule;
            panel.settings.free = true;
        }
        Value::Array(list) => {
            let chunks: Option<BTreeSet<u32>> = list
                .iter()
                .map(|x| x.as_u64().and_then(|n| u32::try_from(n).ok()))
                .collect();
            let chunks = chunks.ok_or_else(bad)?;
            let n = panel.result.as_ref().map_or(0, |s| s.chunks.len());
            if let Some(out) = chunks.iter().find(|&&c| c as usize >= n) {
                return Err(PanelError::new(
                    "fracture_panel_bad_chunk",
                    format!("chunk {out} is outside the {n} chunks of the last run"),
                )
                .rpc());
            }
            panel.supports = SupportPick::Chunks(chunks);
        }
        Value::Object(o) if o.len() == 1 && o.contains_key("below_y") => {
            panel.supports = SupportPick::Rule;
            panel.settings.free = false;
            // `null` clears the plane — the agent path of the *Plane: chunks below y* checkbox
            // switched off (w7-agent-parity-gaps-z follow-up 1, the last KNOWN_GAPS row).
            panel.settings.support_below_y = match &o["below_y"] {
                Value::Null => None,
                v => Some(v.as_f64().filter(|x| x.is_finite()).ok_or_else(bad)? as f32),
            };
        }
        Value::Object(o) if o.len() == 1 && o.contains_key("box") => {
            let corners = o["box"].as_array().filter(|a| a.len() == 2).ok_or_else(bad)?;
            let (min, max) = (
                vec3(&corners[0]).ok_or_else(bad)?,
                vec3(&corners[1]).ok_or_else(bad)?,
            );
            if (0..3).any(|k| min[k] > max[k]) {
                return Err(bad());
            }
            panel.supports = SupportPick::Box { min, max };
        }
        _ => return Err(bad()),
    }
    panel.test = None;
    Ok(())
}

/// What `state` reports about the supports.
pub(crate) fn supports_state(panel: &FracturePanel) -> Value {
    let flags = support_flags(panel);
    let chunks: Vec<u32> = flags
        .iter()
        .enumerate()
        .filter(|(_, f)| **f)
        .map(|(i, _)| i as u32)
        .collect();
    let kind = match &panel.supports {
        SupportPick::Rule if panel.settings.free => json!("none"),
        SupportPick::Rule => match panel.settings.support_below_y {
            Some(y) => json!({ "below_y": y }),
            None => json!("rule"),
        },
        SupportPick::Chunks(_) => json!("picked"),
        SupportPick::Box { min, max } => json!({ "box": [min, max] }),
    };
    json!({ "pick": kind, "chunks": chunks })
}

/// What `state` reports about the click mode and the test.
pub(crate) fn tools_state(panel: &FracturePanel) -> Value {
    json!({
        "click": panel.click.name(),
        "hit_impulse": panel.hit_impulse,
        "hit_radius": panel.hit_radius,
        "show_bonds": panel.show_bonds,
        "hover_bond": panel.hover_bond,
        "test": panel.test.as_ref().map(TestBreak::report),
        "saved": panel.saved,
    })
}

/// Parses the tool fields of `editor.fracture_panel_set`.
pub(crate) fn set_tools(panel: &mut FracturePanel, params: &Value) -> Result<(), RpcError> {
    // One site choice per call (ADR-7550): with two, which one the run takes would depend on the
    // order the keys are applied in.
    let choices = ["sites", "grid", "impact", "clustered"]
        .into_iter()
        .filter(|k| params.get(*k).is_some_and(|v| !v.is_null()))
        .collect::<Vec<_>>();
    if choices.len() > 1 {
        return Err(RpcError::invalid_params(format!(
            "give one site choice per call, not {}",
            choices.join(" and ")
        )));
    }
    if let Some(v) = params.get("click").filter(|v| !v.is_null()) {
        panel.click = v
            .as_str()
            .and_then(ClickMode::parse)
            .ok_or_else(|| RpcError::invalid_params("`click` must be off, supports or test_hit"))?;
    }
    if let Some(v) = params.get("show_bonds").filter(|v| !v.is_null()) {
        panel.show_bonds = v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`show_bonds` must be a boolean"))?;
    }
    for (key, slot, min) in [
        ("hit_impulse", &mut panel.hit_impulse, 0.0),
        ("hit_radius", &mut panel.hit_radius, 1e-3),
    ] {
        if let Some(v) = params.get(key).filter(|v| !v.is_null()) {
            *slot = v
                .as_f64()
                .filter(|x| x.is_finite() && *x >= min)
                .map(|x| x as f32)
                .ok_or_else(|| {
                    RpcError::invalid_params(format!("`{key}` must be a finite number >= {min}"))
                })?;
        }
    }
    if let Some(v) = params.get("sites").filter(|v| !v.is_null()) {
        let sites: Option<Vec<[f32; 3]>> = v.as_array().and_then(|a| {
            a.iter()
                .map(|p| {
                    let p = p.as_array().filter(|p| p.len() == 3)?;
                    let f = |i: usize| p[i].as_f64().filter(|x| x.is_finite()).map(|x| x as f32);
                    Some([f(0)?, f(1)?, f(2)?])
                })
                .collect()
        });
        panel.sites =
            Some(sites.ok_or_else(|| RpcError::invalid_params("`sites` must be [[x, y, z], ...]"))?);
        panel.impact = None;
        panel.clustered = None;
    }
    if params.get("sites").is_some_and(Value::is_null) {
        panel.sites = None;
    }
    if let Some(v) = params.get("impact").filter(|v| !v.is_null()) {
        panel.impact = Some(parse_impact(v, panel.impact.as_ref())?);
        panel.sites = None;
        panel.clustered = None;
    }
    if params.get("impact").is_some_and(Value::is_null) {
        panel.impact = None;
    }
    if let Some(v) = params.get("clustered").filter(|v| !v.is_null()) {
        panel.clustered = Some(parse_clustered(
            v,
            panel.settings.chunks,
            panel.clustered.as_ref(),
        )?);
        panel.sites = None;
        panel.impact = None;
    }
    if params.get("clustered").is_some_and(Value::is_null) {
        panel.clustered = None;
    }
    if let Some(v) = params.get("grid").filter(|v| !v.is_null()) {
        let counts: Option<[u32; 3]> = v.as_array().filter(|a| a.len() == 3).and_then(|a| {
            let c = |i: usize| a[i].as_u64().and_then(|n| u32::try_from(n).ok());
            Some([c(0)?, c(1)?, c(2)?])
        });
        let counts =
            counts.ok_or_else(|| RpcError::invalid_params("`grid` must be [x, y, z] cell counts"))?;
        let crate::fracture_panel::PanelSource::Box { half } = panel.source else {
            return Err(PanelError::new(
                "fracture_panel_grid_needs_box",
                "grid sites fill a box source; give mesh sites as `sites`",
            )
            .rpc());
        };
        panel.sites = Some(grid_sites(half, counts).map_err(|e| e.rpc())?);
        panel.settings.chunks = counts.iter().map(|&c| c as usize).product();
        panel.impact = None;
        panel.clustered = None;
    }
    if let Some(v) = params.get("supports").filter(|v| !v.is_null()) {
        parse_supports(panel, v)?;
    }
    Ok(())
}

fn with_panel<T>(world: &mut World, f: impl FnOnce(&mut FracturePanel) -> T) -> T {
    let mut panel = world.remove_resource::<FracturePanel>().unwrap_or_default();
    panel.poll();
    let out = f(&mut panel);
    world.insert_resource(panel);
    out
}

fn vec3_param(params: &Value, key: &str) -> Result<Option<[f32; 3]>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_array()
            .filter(|a| a.len() == 3)
            .and_then(|a| {
                let f = |i: usize| a[i].as_f64().filter(|x| x.is_finite()).map(|x| x as f32);
                Some([f(0)?, f(1)?, f(2)?])
            })
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be [x, y, z]"))),
    }
}

/// `editor.fracture_test_hit { point, impulse?, radius? }`.
fn method_test_hit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let point =
        vec3_param(params, "point")?.ok_or_else(|| RpcError::invalid_params("`point` is required"))?;
    let number = |key: &str| -> Result<Option<f32>, RpcError> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_f64()
                .filter(|x| x.is_finite())
                .map(|x| Some(x as f32))
                .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a finite number"))),
        }
    };
    let (impulse, radius) = (number("impulse")?, number("radius")?);
    with_panel(world, |panel| {
        let hit = TestHit {
            point,
            impulse: impulse.unwrap_or(panel.hit_impulse),
            radius: radius.unwrap_or(panel.hit_radius),
        };
        test_hit(panel, hit).map_err(|e| e.rpc())
    })
}

/// `editor.fracture_test_debris { steps? }`.
fn method_test_debris(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let steps = match params.get("steps") {
        None | Some(Value::Null) => DEBRIS_STEPS,
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| RpcError::invalid_params("`steps` must be a whole number"))?,
    };
    let floor_y = match params.get("floor_y") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .ok_or_else(|| RpcError::invalid_params("`floor_y` must be a number"))? as f32,
        ),
    };
    with_panel(world, |panel| {
        test_debris(panel, steps, floor_y).map_err(|e| e.rpc())
    })
}

const DEBRIS_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "steps",
        kind: "integer",
        required: false,
        description: "Fixed steps (60 Hz) to run after the hits, 1 to 600 (default 60).",
    },
    ParamSchema {
        name: "floor_y",
        kind: "number",
        required: false,
        description: "Height of the floor's top, at most the structure's lowest point (default: that point).",
    },
];

/// `editor.fracture_test_reset`.
fn method_test_reset(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(with_panel(world, |panel| {
        let had = panel.test.take().is_some();
        json!({ "reset": had })
    }))
}

/// `editor.fracture_bonds { top? }`.
fn method_bonds(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let top = match params.get("top") {
        None | Some(Value::Null) => WEAKEST_BONDS,
        Some(v) => v
            .as_u64()
            .map(|n| n.min(1024) as usize)
            .ok_or_else(|| RpcError::invalid_params("`top` must be a non-negative integer"))?,
    };
    with_panel(world, |panel| {
        if panel.result.is_none() {
            return Err(PanelError::new("fracture_panel_no_result", "run the fracture first").rpc());
        }
        let list = bonds(panel);
        let broken: BTreeSet<u32> = panel
            .test
            .as_ref()
            .map(|t| t.broken_bonds().into_iter().collect())
            .unwrap_or_default();
        let weak: Vec<Value> = weakest(&list, top)
            .iter()
            .map(|b| b.json(broken.contains(&b.bond)))
            .collect();
        let hovered = panel
            .hover_bond
            .and_then(|h| list.iter().find(|b| b.bond == h))
            .map(|b| json!({ "bond": b.bond, "label": b.label() }));
        Ok(json!({ "bonds": list.len(), "weakest": weak, "hovered": hovered }))
    })
}

/// `editor.fracture_place { at? }`.
fn method_place(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let at = vec3_param(params, "at")?;
    let (entity, guid, name) = place(world, at.map(Vec3::from_array)).map_err(|e| e.rpc())?;
    let source = world
        .get::<DestructibleSource>(entity)
        .cloned()
        .unwrap_or_default();
    let translation = world
        .get::<Transform>(entity)
        .map_or([0.0; 3], |t| t.translation.to_array());
    Ok(json!({
        "entity": entity.to_bits(),
        "guid": guid.to_string(),
        "name": name,
        "asset": source.asset,
        "seed": source.seed,
        "at": translation,
    }))
}

const TEST_HIT_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "point",
        kind: "array",
        required: true,
        description: "[x, y, z] world-space centre of the explosion (the preview sits at the panel's origin).",
    },
    ParamSchema {
        name: "impulse",
        kind: "number",
        required: false,
        description: "Impulse at the centre, newton-seconds (default the panel's hit_impulse).",
    },
    ParamSchema {
        name: "radius",
        kind: "number",
        required: false,
        description: "Radius, metres, > 0 (default the panel's hit_radius).",
    },
];

const BONDS_PARAMS: &[ParamSchema] = &[ParamSchema {
    name: "top",
    kind: "integer",
    required: false,
    description: "How many of the weakest bonds to list (default 10).",
}];

const PLACE_PARAMS: &[ParamSchema] = &[ParamSchema {
    name: "at",
    kind: "array",
    required: false,
    description: "[x, y, z] world position (default the viewport drop point).",
}];

const TEST_RESULT: &str = "{ hits, ticks, bonds, bonds_broken, broken_bonds: [{ bond, a, b }], chunks, detached: [chunk], \
    collapsed: [chunk], falling: [chunk], attached, structure_hash }";

/// The tools' agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.fracture_test_hit",
                tool: "fracture_test_hit",
                description: "Hit the Fracture panel's test break with an explosion and run 4 fixed ticks of the \
                    break model (DestructionWorld, the saved cook): broken bonds, chunks knocked loose, collapsed \
                    chunks, the falling set and structure_hash, which a game world fed the same asset, pose, \
                    seed 0 and hits matches.",
                params: TEST_HIT_PARAMS,
                result: TEST_RESULT,
                mutating: true,
                permission: Permission::EditScene,
            },
            method_test_hit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.fracture_test_reset",
                tool: "fracture_test_reset",
                description: "Drop the Fracture panel's test break (the Reset button); the next hit starts whole.",
                params: &[],
                result: "{ reset }",
                mutating: true,
                permission: Permission::EditScene,
            },
            method_test_reset as Handler,
        ),
        (
            MethodSchema {
                name: "editor.fracture_test_debris",
                tool: "fracture_test_debris",
                description: "Run the test break's loose chunks under the physics sim (the Run debris button): the \
                    hits replayed in a preview world with the destruction bridge and a floor under the structure, \
                    then steps more fixed ticks; reports where each chunk came to rest and that all stayed above \
                    the floor. Reset drops it.",
                params: DEBRIS_PARAMS,
                result: "{ steps, floor_y, chunks, moved, above_floor, min_clearance, digest, debris: [{ chunk, \
                    start, end, rotation, min_clearance, live }] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            method_test_debris as Handler,
        ),
        (
            MethodSchema {
                name: "editor.fracture_bonds",
                tool: "fracture_bonds",
                description: "The bonds of the Fracture panel's result as Save cooks them: the weakest first, \
                    whether the test broke each, and the bond under the pointer.",
                params: BONDS_PARAMS,
                result: "{ bonds, weakest: [{ bond, a, b, break_impulse, area, anchor, normal, broken }], hovered: \
                    { bond, label } }",
                mutating: false,
                permission: Permission::Read,
            },
            method_bonds as Handler,
        ),
        (
            MethodSchema {
                name: "editor.fracture_place",
                tool: "fracture_place",
                description: "Spawn an entity with a DestructibleSource naming the last saved .destructible.ron (the \
                    Place button): one undo step, saved with the level; a play with DestructionPlugin defines it.",
                params: PLACE_PARAMS,
                result: "{ entity, guid, name, asset, seed, at }",
                mutating: true,
                permission: Permission::EditScene,
            },
            method_place as Handler,
        ),
    ]
}

/// What a placed destructible's asset holds, for the Inspector: chunks, support chunks, bonds, the
/// weakest bond, total mass and the material tag. Reads a project `.destructible.ron`; a cooked
/// sub-asset (`...#fracture/destructible`) is summarized as such without a read.
///
/// # Errors
/// `fracture_panel_no_project` with no project, `fracture_panel_bad_path` for a path outside it,
/// `fracture_panel_io` for an unreadable file, `destruction_*` for one that does not parse.
pub fn destructible_summary(project: Option<&std::path::Path>, asset: &str) -> Result<Value, PanelError> {
    if asset.contains('#') {
        return Ok(json!({ "asset": asset, "cooked": true }));
    }
    let project =
        project.ok_or_else(|| PanelError::new("fracture_panel_no_project", "no project is open"))?;
    let rel = std::path::Path::new(asset);
    if asset.is_empty()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(PanelError::new(
            "fracture_panel_bad_path",
            format!("`{asset}` must be a plain path relative to the project"),
        ));
    }
    let file = project.join(rel);
    let text = std::fs::read_to_string(&file)
        .map_err(|e| PanelError::new("fracture_panel_io", format!("{}: {e}", file.display())))?;
    let parsed = DestructibleAsset::from_ron(&text).map_err(|e| PanelError::new(e.code(), e.to_string()))?;
    let weakest = parsed
        .bonds
        .iter()
        .map(|b| b.break_impulse)
        .fold(f32::INFINITY, f32::min);
    Ok(json!({
        "asset": asset,
        "cooked": false,
        "name": parsed.name,
        "chunks": parsed.chunks.len(),
        "supports": parsed.chunks.iter().filter(|c| c.support).count(),
        "bonds": parsed.bonds.len(),
        "weakest_bond": weakest.is_finite().then_some(weakest),
        "mass": parsed.chunks.iter().map(|c| f64::from(c.mass)).sum::<f64>(),
        "fx_tag": parsed.material.fx_tag,
        "meshes": parsed.meshes,
    }))
}

/// The Inspector's section under a `DestructibleSource`: the asset's summary
/// ([`destructible_summary`]) and whether a play defined it.
pub fn inspector_section(ui: &mut egui::Ui, world: &mut World, entity: Entity) {
    let Some(source) = world.get::<DestructibleSource>(entity).cloned() else {
        return;
    };
    let project = crate::mesh_source::project_root(world);
    match destructible_summary(project.as_deref(), &source.asset) {
        Ok(v) if v["cooked"] == json!(true) => {
            ui.weak("cooked sub-asset: chunks and meshes come from the cook");
        }
        Ok(v) => {
            ui.label(format!(
                "{} chunks ({} held by the world), {} bonds",
                v["chunks"], v["supports"], v["bonds"]
            ));
            ui.label(format!(
                "mass {:.0} kg, weakest bond {:.0} N s, effect tag {}",
                v["mass"].as_f64().unwrap_or(0.0),
                v["weakest_bond"].as_f64().unwrap_or(0.0),
                v["fx_tag"].as_str().unwrap_or("")
            ));
        }
        Err(e) => {
            ui.colored_label(
                egui::Color32::from_rgb(230, 90, 90),
                crate::data_panels::sentence(&e.message),
            )
            .on_hover_text(format!("Agent error code: {}", e.code));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fracture_panel::PanelSource;

    /// A finished 3x3x1 wall (half extents 1.5, 1.5, 0.25) on grid sites.
    fn wall() -> FracturePanel {
        let mut panel = FracturePanel::default();
        panel.source = PanelSource::Box {
            half: [1.5, 1.5, 0.25],
        };
        panel.sites = Some(grid_sites([1.5, 1.5, 0.25], [3, 3, 1]).expect("grid"));
        panel.settings.chunks = 9;
        let dir = regia_test_utils::unique_temp_dir("fracture-tools-unit");
        panel.start(Some(&dir)).expect("start");
        panel.wait();
        assert!(panel.error.is_none(), "{:?}", panel.error);
        panel.show_gizmo = true;
        panel
    }

    #[test]
    fn grid_sites_number_the_bottom_row_first() {
        let sites = grid_sites([1.5, 1.5, 0.25], [3, 3, 1]).expect("grid");
        assert_eq!(sites.len(), 9);
        assert_eq!(sites[0], [-1.0, -1.0, 0.0]);
        assert_eq!(sites[1], [0.0, -1.0, 0.0]);
        assert_eq!(sites[7], [0.0, 1.0, 0.0]);
        assert_eq!(
            grid_sites([1.0; 3], [0, 1, 1]).map_err(|e| e.code),
            Err("fracture_panel_bad_grid".to_string())
        );
    }

    #[test]
    fn the_default_rule_holds_the_bottom_row_and_a_click_toggles_one_chunk() {
        let mut panel = wall();
        let set = panel.result.as_ref().expect("result");
        assert_eq!(set.chunks.len(), 9);
        // Chunks are in site order: the bottom row is 0, 1, 2.
        let flags = support_flags(&panel);
        assert_eq!(
            flags,
            vec![true, true, true, false, false, false, false, false, false]
        );

        // A ray down the -z axis through the top centre brick picks it, at its front face.
        let (chunk, point) =
            pick_chunk(&panel, Vec3::new(0.0, 1.0, 5.0), Vec3::new(0.0, 0.0, -1.0)).expect("hit");
        assert_eq!(chunk, 7);
        assert!((point - Vec3::new(0.0, 1.0, 0.25)).length() < 1e-4, "{point:?}");
        assert!(pick_chunk(&panel, Vec3::new(9.0, 9.0, 5.0), Vec3::new(0.0, 0.0, -1.0)).is_none());

        toggle_support(&mut panel, 7).expect("toggle on");
        assert_eq!(
            panel.supports,
            SupportPick::Chunks([0, 1, 2, 7].into_iter().collect())
        );
        toggle_support(&mut panel, 0).expect("toggle off");
        assert_eq!(support_flags(&panel)[..3], [false, true, true]);
        assert_eq!(
            toggle_support(&mut panel, 9).map_err(|e| e.code),
            Err("fracture_panel_bad_chunk".to_string())
        );
        // Colors: support blue, the rest the edge color.
        let colors = chunk_colors(&panel);
        assert_eq!(colors[1], SUPPORT_EDGES);
        assert_eq!(colors[0], crate::fracture_panel::FRACTURE_EDGES);

        // A new run clears the picks: indices name one result.
        panel
            .start(Some(&regia_test_utils::unique_temp_dir("fracture-tools-rerun")))
            .expect("start");
        panel.wait();
        assert_eq!(panel.supports, SupportPick::Rule);
    }

    #[test]
    fn a_test_hit_colors_the_loose_chunk_and_greys_its_bonds() {
        let mut panel = wall();
        panel.show_bonds = true;
        let before = bond_segments(&panel);
        assert_eq!(before.len(), 12, "one segment per bond");
        let report = test_hit(
            &mut panel,
            TestHit {
                point: [0.0, 1.0, 0.0],
                impulse: 9000.0,
                radius: 0.5,
            },
        )
        .expect("hit");
        assert_eq!(report["detached"], json!([7]), "{report}");
        assert_eq!(chunk_colors(&panel)[7], DETACHED_EDGES);
        let after = bond_segments(&panel);
        assert_eq!(after.iter().filter(|s| s.2 == BROKEN_BOND).count(), 3);
        // A falling chunk is not pickable any more.
        let hit = pick_chunk(&panel, Vec3::new(0.0, 1.0, 5.0), Vec3::new(0.0, 0.0, -1.0));
        assert!(hit.is_none(), "{hit:?}");
        let e = test_hit(
            &mut panel,
            TestHit {
                point: [0.0; 3],
                impulse: 1.0,
                radius: 0.0,
            },
        )
        .expect_err("radius 0");
        assert!(e.code.starts_with("destruction_"), "{}", e.code);
    }

    /// Acceptance (ADR-7745, item 4): after N fixed steps at least one chunk moved and every chunk
    /// stayed above the floor on every step; the same preview twice is the same; a new hit and
    /// Reset drop it.
    #[test]
    fn the_debris_preview_moves_the_loose_chunk_and_keeps_it_above_the_floor() {
        // The wall stands on its two bottom corners over a floor 1 m below its base: the bottom
        // centre brick, knocked out, has nothing under it but that floor.
        let mut panel = wall();
        panel.supports = SupportPick::Chunks([0, 2].into_iter().collect());
        assert_eq!(
            test_debris(&mut panel, DEBRIS_STEPS, None).map_err(|e| e.code),
            Err("fracture_test_no_hits".to_string())
        );
        let hit = TestHit {
            point: [0.0, -1.0, 0.0],
            impulse: 9000.0,
            radius: 0.5,
        };
        let report = test_hit(&mut panel, hit.clone()).expect("hit");
        assert_eq!(report["detached"], json!([1]), "{report}");
        assert!(debris_segments(&panel).is_empty(), "nothing drawn before a run");
        let report = test_debris(&mut panel, 120, Some(-2.5)).expect("debris");
        println!("{report}");
        assert!(report["chunks"].as_u64().is_some_and(|n| n >= 1), "{report}");
        assert!(report["moved"].as_u64().is_some_and(|n| n >= 1), "{report}");
        assert_eq!(report["above_floor"], true, "{report}");
        let preview = panel
            .test
            .as_ref()
            .and_then(|t| t.debris.clone())
            .expect("kept on the test");
        assert!((preview.floor_y + 2.5).abs() < 1e-6, "{}", preview.floor_y);
        let loose = preview.chunks.iter().find(|c| c.chunk == 1).expect("chunk 1");
        assert!(loose.translation.y < loose.start.y - 0.5, "it fell: {loose:?}");
        assert_eq!(debris_segments(&panel).len(), preview.chunks.len() * 12);
        // Fixed steps, no clock: the same preview twice.
        let again = test_debris(&mut panel, 120, Some(-2.5)).expect("again");
        assert_eq!(again, report);
        // The default floor is the structure's lowest point.
        let default = test_debris(&mut panel, 30, None).expect("default floor");
        assert_eq!(default["floor_y"], json!(-1.5));
        assert_eq!(default["above_floor"], true, "{default}");
        for steps in [0, MAX_DEBRIS_STEPS + 1] {
            assert_eq!(
                test_debris(&mut panel, steps, None).map_err(|e| e.code),
                Err("fracture_test_bad_steps".to_string()),
                "{steps}"
            );
        }
        assert_eq!(
            test_debris(&mut panel, 30, Some(0.0)).map_err(|e| e.code),
            Err("fracture_test_bad_floor".to_string()),
            "a floor above the base"
        );
        // Another hit makes the preview stale: it goes.
        test_hit(
            &mut panel,
            TestHit {
                point: [0.0, 1.0, 0.0],
                ..hit
            },
        )
        .expect("second hit");
        assert!(panel.test.as_ref().is_some_and(|t| t.debris.is_none()));
    }

    /// The agent method answers by its registered name and Reset drops the preview with the test.
    #[test]
    fn fracture_test_debris_answers_by_its_name_and_reset_drops_it() {
        let mut world = World::new();
        world.insert_resource(wall());
        let call = |world: &mut World, name: &str, params: Value| {
            let list = methods();
            let handler = list
                .iter()
                .find(|(m, _)| m.name == name)
                .map(|(_, h)| *h)
                .expect("registered");
            handler(world, &params)
        };
        let e = call(&mut world, "editor.fracture_test_debris", json!({})).expect_err("no hits");
        assert_eq!(
            e.data.as_ref().and_then(|d| d["reason"].as_str()),
            Some("fracture_test_no_hits")
        );
        call(
            &mut world,
            "editor.fracture_test_hit",
            json!({ "point": [0.0, 1.0, 0.0], "impulse": 9000.0, "radius": 0.5 }),
        )
        .expect("hit");
        let v = call(&mut world, "editor.fracture_test_debris", json!({ "steps": 30 })).expect("debris");
        assert_eq!(v["floor_y"], json!(-1.5));
        assert_eq!(v["steps"], 30);
        assert_eq!(v["above_floor"], true);
        let e = call(
            &mut world,
            "editor.fracture_test_debris",
            json!({ "steps": "many" }),
        )
        .expect_err("bad");
        assert!(e.message.contains("steps"), "{}", e.message);
        // A floor above the structure's lowest point (-1.5) is refused, however little above it.
        let e = call(
            &mut world,
            "editor.fracture_test_debris",
            json!({ "floor_y": -1.45 }),
        )
        .expect_err("floor above the lowest point");
        assert_eq!(
            e.data.as_ref().and_then(|d| d["reason"].as_str()),
            Some("fracture_test_bad_floor")
        );
        call(
            &mut world,
            "editor.fracture_test_debris",
            json!({ "steps": 1, "floor_y": -1.5 }),
        )
        .expect("the lowest point itself");
        call(&mut world, "editor.fracture_test_reset", json!({})).expect("reset");
        assert!(world.resource::<FracturePanel>().test.is_none());
        assert!(debris_segments(world.resource::<FracturePanel>()).is_empty());
    }

    #[test]
    fn the_bond_view_names_and_ranks_bonds_by_break_impulse() {
        let mut panel = FracturePanel::default();
        panel.settings.chunks = 10;
        panel.settings.seed = 3;
        panel
            .start(Some(&regia_test_utils::unique_temp_dir("fracture-tools-bonds")))
            .expect("start");
        panel.wait();
        let list = bonds(&panel);
        assert!(list.len() > 10, "{}", list.len());
        let weak = weakest(&list, WEAKEST_BONDS);
        assert_eq!(weak.len(), WEAKEST_BONDS);
        assert!(weak.windows(2).all(|w| w[0].break_impulse <= w[1].break_impulse));
        let floor = list.iter().map(|b| b.break_impulse).fold(f32::INFINITY, f32::min);
        assert_eq!(weak[0].break_impulse, floor);
        assert!(
            weak[0]
                .label()
                .starts_with(&format!("bond {}: chunk", weak[0].bond))
        );
        // The weakest bond is red, the strongest cyan; the hovered one white.
        panel.show_bonds = true;
        panel.show_gizmo = true;
        let segments = bond_segments(&panel);
        assert_eq!(segments[weak[0].bond as usize].2, heat_color(0.0));
        assert_eq!(heat_color(0.0), egui::Color32::from_rgb(235, 50, 40));
        assert_eq!(heat_color(1.0), egui::Color32::from_rgb(70, 220, 235));
        assert_eq!(heat_color(f32::NAN), heat_color(0.0));
        // A ray through the weakest bond's anchor names it.
        let a = Vec3::from_array(weak[0].anchor);
        let picked = pick_bond(
            &panel,
            a + Vec3::new(0.0, 0.0, 10.0),
            Vec3::new(0.0, 0.0, -1.0),
            0.01,
        );
        assert_eq!(picked, Some(weak[0].bond));
        panel.hover_bond = picked;
        assert_eq!(bond_segments(&panel)[weak[0].bond as usize].2, HOVERED_BOND);
    }

    #[test]
    fn the_plane_rule_goes_to_the_meta_and_picks_do_not() {
        let dir = regia_test_utils::unique_temp_dir("fracture-tools-meta");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let glb = dir.join("assets/wall.glb");
        std::fs::write(&glb, b"not read here").expect("file");
        let mut panel = FracturePanel::default();
        panel.source = PanelSource::Mesh {
            path: "assets/wall.glb".to_string(),
        };
        parse_supports(&mut panel, &json!({ "below_y": -0.2 })).expect("plane");
        panel.apply_meta(Some(&dir)).expect("apply");
        let meta = regia_asset::MetaSidecar::from_ron(
            &std::fs::read(regia_asset::meta_path_for(&glb)).expect("read"),
        )
        .expect("meta");
        let settings = regia_asset_pipeline::GltfImporterSettings::from_meta_string(meta.settings.as_deref());
        assert_eq!(settings.fracture.expect("fracture").support_below_y, Some(-0.2));

        panel.supports = SupportPick::Box {
            min: [0.0; 3],
            max: [1.0; 3],
        };
        assert_eq!(
            panel.apply_meta(Some(&dir)).map_err(|e| e.code),
            Err("fracture_panel_meta_supports".to_string())
        );
        panel.supports = SupportPick::Rule;
        panel.sites = Some(vec![[0.0; 3]]);
        assert_eq!(
            panel.apply_meta(Some(&dir)).map_err(|e| e.code),
            Err("fracture_panel_meta_sites".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_inspector_summary_reads_the_saved_asset() {
        let dir = regia_test_utils::unique_temp_dir("fracture-tools-summary");
        let mut panel = wall();
        panel.supports = SupportPick::Chunks([0, 1].into_iter().collect());
        panel.save(Some(&dir), "assets/wall").expect("save");
        let v = destructible_summary(Some(&dir), "assets/wall.destructible.ron").expect("summary");
        assert_eq!(v["chunks"], 9);
        assert_eq!(v["supports"], 2);
        assert_eq!(v["bonds"], 12);
        assert_eq!(v["meshes"], "wall.fracture.ron");
        let cooked = destructible_summary(None, "assets/wall.glb#fracture/destructible").expect("cooked");
        assert_eq!(cooked["cooked"], true);
        for (project, asset, code) in [
            (None, "assets/wall.destructible.ron", "fracture_panel_no_project"),
            (
                Some(dir.as_path()),
                "../x.destructible.ron",
                "fracture_panel_bad_path",
            ),
            (
                Some(dir.as_path()),
                "assets/none.destructible.ron",
                "fracture_panel_io",
            ),
        ] {
            assert_eq!(
                destructible_summary(project, asset).map_err(|e| e.code),
                Err(code.to_string()),
                "{asset}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    const SCREEN: egui::Rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 1600.0));

    /// One headless frame of the panel's tool sections (`draw_tools`).
    fn tools_frame(ctx: &egui::Context, panel: &mut FracturePanel, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(SCREEN),
            events,
            ..egui::RawInput::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            draw_tools(ui, panel);
        });
        output.textures_delta.clear();
    }

    /// `editor.fracture_bonds`' `hovered`, through the method.
    fn hovered_answer(world: &mut World) -> Value {
        method_bonds(world, &json!({})).expect("bonds")["hovered"].clone()
    }

    /// ADR-7451 (3): the bond list's hover follows the pointer off the list: hover a row, then
    /// nothing, and `editor.fracture_bonds` answers `hovered: null`.
    #[test]
    fn the_bond_list_hover_clears_when_the_pointer_leaves() {
        let mut panel = wall();
        panel.show_bonds = true;
        let ctx = egui::Context::default();
        tools_frame(&ctx, &mut panel, vec![]);
        // Walk the pointer down the panel until a weakest-list row is under it (two frames per
        // spot: egui hit-tests against the previous frame's widgets).
        let mut found = None;
        for step in 0..400 {
            let at = egui::pos2(60.0, step as f32 * 4.0);
            tools_frame(&ctx, &mut panel, vec![egui::Event::PointerMoved(at)]);
            tools_frame(&ctx, &mut panel, vec![egui::Event::PointerMoved(at)]);
            if panel.hover_bond.is_some() {
                found = Some(at);
                break;
            }
        }
        assert!(found.is_some(), "no list row took the hover");
        let mut world = World::new();
        world.insert_resource(panel);
        assert!(
            hovered_answer(&mut world).is_object(),
            "a hovered row is reported"
        );
        let mut panel = world.remove_resource::<FracturePanel>().expect("panel");
        // Off the list (still in the window), then out of the window.
        for event in [
            egui::Event::PointerMoved(egui::pos2(790.0, 1590.0)),
            egui::Event::PointerGone,
        ] {
            tools_frame(&ctx, &mut panel, vec![event.clone()]);
            tools_frame(&ctx, &mut panel, vec![]);
            assert_eq!(panel.hover_bond, None, "after {event:?}");
        }
        world.insert_resource(panel);
        assert_eq!(hovered_answer(&mut world), Value::Null);
    }

    /// ADR-7451 (3): the viewport's bond hover clears when the pointer leaves the bond, and the
    /// list's hover-free frames do not take a viewport hover away while it lasts.
    #[test]
    fn the_viewport_bond_hover_clears_when_the_pointer_leaves() {
        let mut panel = wall();
        panel.show_bonds = true;
        let camera = crate::viewport::EditorCamera::default();
        let size = regia_core::math::Vec2::new(400.0, 300.0);
        // A pixel whose ray passes a bond anchor, found by a 2 px scan of the view (about 6 cm
        // apart at the wall, inside the 10 cm asked here and the viewport's 15 cm reach).
        let pixel = (0..150 * 200)
            .map(|i| regia_core::math::Vec2::new((i % 200) as f32 * 2.0, (i / 200) as f32 * 2.0))
            .find(|p| {
                let (o, d) = crate::viewport::pick_ray(&camera, *p, size);
                pick_bond(&panel, o, d, 0.1).is_some()
            })
            .expect("a pixel over a bond");
        let mut world = World::new();
        world.insert_resource(panel);
        let ctx = egui::Context::default();
        let mut origin = egui::Pos2::ZERO;
        let mut frame = |world: &mut World, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(SCREEN),
                events,
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(size.x, size.y), egui::Sense::click_and_drag());
                origin = rect.min;
                viewport_input(ui, world, &response, &camera, size);
                // The panel draws in the same frame with no list row hovered.
                let mut panel = world.remove_resource::<FracturePanel>().expect("panel");
                draw_tools(ui, &mut panel);
                world.insert_resource(panel);
            });
            output.textures_delta.clear();
            origin
        };
        let origin = frame(&mut world, vec![]);
        let at = origin + egui::vec2(pixel.x, pixel.y);
        frame(&mut world, vec![egui::Event::PointerMoved(at)]);
        frame(&mut world, vec![egui::Event::PointerMoved(at)]);
        assert!(
            world.resource::<FracturePanel>().hover_bond.is_some(),
            "the viewport names the bond under the pointer"
        );
        assert!(hovered_answer(&mut world).is_object());
        frame(&mut world, vec![egui::Event::PointerGone]);
        frame(&mut world, vec![]);
        assert_eq!(world.resource::<FracturePanel>().hover_bond, None);
        assert_eq!(hovered_answer(&mut world), Value::Null);
    }

    /// ADR-7451 (4): the bond view cooks once per result, settings and support pick, not per
    /// call: ten repaints cook once; a settings change, a support toggle and a new run cook once
    /// more each.
    #[test]
    fn the_bond_view_cooks_once_per_change_not_per_repaint() {
        let mut panel = wall();
        panel.show_bonds = true;
        let ctx = egui::Context::default();
        assert_eq!(panel.bond_cooks(), 0);
        for _ in 0..10 {
            tools_frame(&ctx, &mut panel, vec![]);
            let _ = bond_segments(&panel);
        }
        assert_eq!(panel.bond_cooks(), 1, "ten repaints cook once");
        let before = bonds(&panel);

        // `bond_seconds` is a cook setting: it scales every bond's break impulse.
        panel.settings.bond_seconds *= 2.0;
        for _ in 0..10 {
            tools_frame(&ctx, &mut panel, vec![]);
        }
        assert_eq!(panel.bond_cooks(), 2, "a settings change cooks once");
        let stronger = bonds(&panel);
        assert_eq!(stronger.len(), before.len());
        assert!(
            stronger
                .iter()
                .zip(&before)
                .all(|(s, b)| s.break_impulse > b.break_impulse),
            "the new cook's bonds are the ones shown"
        );

        toggle_support(&mut panel, 4).expect("toggle");
        for _ in 0..10 {
            tools_frame(&ctx, &mut panel, vec![]);
        }
        assert_eq!(panel.bond_cooks(), 3, "a support edit cooks once");

        panel
            .start(Some(&regia_test_utils::unique_temp_dir("fracture-tools-recook")))
            .expect("start");
        panel.wait();
        for _ in 0..10 {
            tools_frame(&ctx, &mut panel, vec![]);
        }
        assert_eq!(panel.bond_cooks(), 4, "a new run cooks once");
    }

    /// The agent path of the *Plane: chunks below y* checkbox switched off (the last agent-parity
    /// gap, w7-agent-parity-gaps-z follow-up 1): `{ below_y: null }` clears the plane exactly like
    /// the UI, and the slot takes nothing but a finite number or null.
    #[test]
    fn a_null_below_y_clears_the_plane_like_the_ui() {
        let mut panel = wall();
        parse_supports(&mut panel, &json!({ "below_y": -0.2 })).expect("set the plane");
        assert_eq!(panel.settings.support_below_y, Some(-0.2));
        parse_supports(&mut panel, &json!({ "below_y": null })).expect("clear the plane");
        assert_eq!(panel.supports, SupportPick::Rule);
        assert!(!panel.settings.free);
        assert_eq!(panel.settings.support_below_y, None, "the plane is cleared");
        assert!(panel.test.is_none(), "clearing drops the test break");
        assert_eq!(supports_state(&panel)["pick"], json!("rule"));
        // The slot still refuses everything that is not a finite number or null.
        assert!(parse_supports(&mut panel, &json!({ "below_y": "sea level" })).is_err());
        assert!(parse_supports(&mut panel, &json!({ "below_y": [0.0] })).is_err());
        assert!(parse_supports(&mut panel, &json!({ "below_y2": null })).is_err());
    }
}
