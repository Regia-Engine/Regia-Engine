// SPDX-License-Identifier: MPL-2.0
//! Per-unit AI world views that borrow the per-team facts built once per tick (gap AI-4, ADR-8518).
//!
//! Original code. A unit's view of the world is mostly team-level: its allies, its team's known
//! enemies and their threat. Copying that into every unit's view every tick costs
//! `units x team rows` copies and an allocation per unit. [`AiTickWorld`] holds the per-tick facts once
//! (the [`TeamRosters`] and the [`SightingsBoard`]); [`AiTickWorld::view`] hands each unit an
//! [`AiWorldView`] that borrows its team's slices and keeps only the unit's own fields by value. Building
//! a view is a few word copies and never allocates.
//!
//! # Stage order of one AI tick
//!
//! 1. **Rosters**: `TeamRosters::build` (and `SlotIndex::build`) after spawns and removals.
//! 2. **Index build**: the host's `DetSpatial::build` for its own queries (target pick, drivers); the
//!    sighting refresh builds its own index inside [`PerceptionIndex`].
//! 3. **Perception** (the sight channel): `SightingsBoard::refresh_with`.
//! 4. **Targets**: per-unit target choice reads the views (plan row ai-nav-01).
//! 5. **Decide**: `AiCore::decide` scores the due units.
//! 6. **Act**: the host's driver moves and fires, updating its spatial rows as units move.
//!
//! The views of stages 4 to 6 borrow what stages 1 and 3 built, so the rosters and the board must not
//! change while views are alive (the borrow checker enforces it).
//!
//! [`PerceptionIndex`]: super::PerceptionIndex

use super::perception::{KnownEnemy, SightingsBoard};
use super::roster::TeamRosters;

/// The fields of one unit a view carries by value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AiUnitFacts {
    /// Its entity id.
    pub entity: u32,
    /// Its slot in the host's row array (the rosters' slot).
    pub slot: u32,
    /// Its team.
    pub team: u8,
    /// Its position.
    pub pos: [f32; 3],
}

/// The per-tick facts every unit's view shares: built once, borrowed by every [`AiWorldView`].
#[derive(Debug, Clone, Copy)]
pub struct AiTickWorld<'a> {
    tick: u64,
    rosters: &'a TeamRosters,
    board: &'a SightingsBoard,
}

impl<'a> AiTickWorld<'a> {
    /// The facts of `tick`: this tick's rosters and the board after this tick's refresh.
    pub fn new(tick: u64, rosters: &'a TeamRosters, board: &'a SightingsBoard) -> Self {
        AiTickWorld { tick, rosters, board }
    }

    /// The tick.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// The rosters.
    pub fn rosters(&self) -> &'a TeamRosters {
        self.rosters
    }

    /// The sighting board.
    pub fn board(&self) -> &'a SightingsBoard {
        self.board
    }

    /// One unit's view: its team's rows borrowed, its own fields copied. Never allocates.
    pub fn view(&self, unit: AiUnitFacts) -> AiWorldView<'a> {
        AiWorldView {
            tick: self.tick,
            unit,
            rosters: self.rosters,
            allies: self.rosters.team(unit.team),
            known: self.board.for_team(unit.team),
        }
    }
}

/// One unit's view of the world for one tick: borrowed team facts plus the unit's own fields.
#[derive(Debug, Clone, Copy)]
pub struct AiWorldView<'a> {
    tick: u64,
    unit: AiUnitFacts,
    rosters: &'a TeamRosters,
    allies: &'a [u32],
    known: &'a [KnownEnemy],
}

impl<'a> AiWorldView<'a> {
    /// The tick.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// The unit's own fields.
    pub fn unit(&self) -> AiUnitFacts {
        self.unit
    }

    /// The slots of the unit's team (itself included), ascending: `TeamRosters::team`.
    pub fn allies(&self) -> &'a [u32] {
        self.allies
    }

    /// The team's known enemies in board order (first sighting first): `SightingsBoard::for_team`.
    pub fn known_enemies(&self) -> &'a [KnownEnemy] {
        self.known
    }

    /// The team's first board row for `entity` (a scan of the team's row, as the board's own lookups).
    pub fn known_enemy(&self, entity: u32) -> Option<&'a KnownEnemy> {
        self.known.iter().find(|known| known.entity == entity)
    }

    /// Fill `out` with the slots of every listed row not on the unit's team, ascending
    /// (`TeamRosters::others_into`; allocates only when `out` must grow).
    pub fn enemy_slots_into(&self, out: &mut Vec<u32>) {
        self.rosters.others_into(self.unit.team, out);
    }

    /// Total dps of the known enemies that can reach the team: `SightingsBoard::visible_threat`, same
    /// order of addition.
    pub fn visible_threat(&self) -> f32 {
        self.known
            .iter()
            .filter(|k| k.threat.can_reach_me)
            .map(|k| k.threat.dps)
            .sum()
    }

    /// The rosters the view borrows (other teams' rows, team of a slot).
    pub fn rosters(&self) -> &'a TeamRosters {
        self.rosters
    }
}
