// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/mode_bots.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 47-61 `mode_kind`, 147-215 `Objectives::from_state`, `is_objective_kind` and `bots_from_state`,
// and the mode-mapping test 1086-1096; plan row S-13, ADR-8554). DO27's hooks read the game's runtime state and its
// mode enum; here they read the S-14 objective board, its contestant list and `MatchMode`:
// - `mode_kind(mode, free_for_all)` is `bot_mode`: DO27's team deathmatch is `TeamDeathmatch` (arena unless the
//   free-for-all flag is set), and DO27's commander mode, which kept its own bots, is a game's `Custom` mode;
// - `Objectives::from_state` is `BotHooks::objectives`: a flag's live position is the board's own `position` (DO27
//   looked up the flag entity's transform), the hill center is its `base_position`, flags sort by team;
// - `bots_from_state` is `BotHooks::bodies`: living contestants on a fighting team (team 1 and up), sorted by id;
//   DO27 read the hull fraction from the entity's health, here the host passes it (`hull_pct`), clamped with
//   `det_clamp` (ADR-8085);
// - new: `BotHooks::orders` (every bot's order for a snapshot, through `regia_detsim::ai::mode_orders`) and
//   `contacts_of`.

//! Mode-bot hooks: the match core's objective board and contestants as the bot layer's view, and every bot's goal
//! order for a capture-the-flag, king-of-the-hill or deathmatch match (plan row S-13, ADR-8554).
//!
//! The bot orders themselves live in `regia_detsim::ai` (re-exported here); this module only reads the rules. A host
//! stepping a lockstep match calls, per tick and in the same order on every peer:
//!
//! 1. [`BotHooks::bodies`]: the living bots from the contestant list the rules step;
//! 2. [`BotHooks::orders`]: each bot's [`ModeOrder`] (a goal point, a focus contact, the KOTH intent or CTF role);
//! 3. its own movement toward each goal, then the rules ([`ObjectiveBoard::ctf_tick`], [`ObjectiveBoard::koth_tick`]).
//!
//! Like the rest of the match core this is plain data with no ECS and no std float math.

use regia_detsim::float::det_clamp;

pub use regia_detsim::ai::{
    AiLevel, BotBody, CtfRole, EnemySight, FlagView, HillView, KothIntent, ModeKind, ModeOrder, Objectives,
    Pickup, mode_order, mode_orders,
};

use super::mode::MatchMode;
use super::objective::{Contestant, ObjectiveBoard};

/// The bot behavior set of a match mode, or `None` for a game's own mode ([`MatchMode::Custom`] keeps the game's own
/// bots) and for capture-zone domination (ADR-8649), which has no stock bot set yet. Team deathmatch is the arena set unless `free_for_all` asks every bot to hunt every other.
pub fn bot_mode(mode: MatchMode, free_for_all: bool) -> Option<ModeKind> {
    match mode {
        MatchMode::FreeForAll => Some(ModeKind::Ffa),
        MatchMode::TeamDeathmatch => Some(if free_for_all {
            ModeKind::Ffa
        } else {
            ModeKind::Arena
        }),
        MatchMode::KingOfTheHill => Some(ModeKind::Koth),
        MatchMode::CaptureTheFlag => Some(ModeKind::Ctf),
        MatchMode::Custom => None,
        // No zone bot set yet (ADR-8649): the game's own bots play domination.
        MatchMode::Domination => None,
    }
}

/// Every bot body as a contact, in the same order (the scripted contact list: every living bot sees every other).
pub fn contacts_of(bots: &[BotBody]) -> Vec<EnemySight> {
    bots.iter()
        .filter(|b| b.hull_pct > 0.0)
        .map(|b| EnemySight {
            id: b.id,
            team: b.team,
            pos: b.pos,
        })
        .collect()
}

/// The match settings the bot hooks read: the bots' difficulty, the match seed and the deathmatch flavor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BotHooks {
    /// Difficulty level of every bot body the hooks build.
    pub level: AiLevel,
    /// The match seed (carried on the objective view).
    pub world_seed: u64,
    /// Team deathmatch bots hunt every other bot ([`bot_mode`]).
    pub free_for_all: bool,
}

impl BotHooks {
    /// The bot behavior set of the board's mode ([`bot_mode`]).
    pub fn mode(&self, board: &ObjectiveBoard) -> Option<ModeKind> {
        bot_mode(board.mode, self.free_for_all)
    }

    /// The board's objectives as the bot layer's view at `tick`: the first hill (with a volume) and every flag,
    /// sorted by team.
    pub fn objectives(&self, board: &ObjectiveBoard, tick: u64) -> Objectives {
        let mut flags: Vec<FlagView> = Vec::new();
        let mut hill: Option<HillView> = None;
        for obj in &board.objectives {
            if obj.is_hill() {
                if hill.is_none()
                    && let Some(volume) = obj.volume
                {
                    hill = Some(HillView {
                        center: obj.base_position,
                        min: volume.min,
                        max: volume.max,
                    });
                }
            } else if obj.is_flag() {
                flags.push(FlagView {
                    team: obj.team_id,
                    base: obj.base_position,
                    pos: obj.position,
                    carrier: obj.carrier_id,
                    at_base: obj.at_base,
                });
            }
        }
        flags.sort_by_key(|f| f.team);
        Objectives {
            hill,
            flags,
            tick,
            world_seed: self.world_seed,
        }
    }

    /// The living contestants on a fighting team (team 1 and up) as bot bodies, sorted by entity id. `hull_pct`
    /// gives each one's hull fraction (clamped to 0..1); a body carries the flag when the board names it a carrier.
    pub fn bodies(
        &self,
        board: &ObjectiveBoard,
        units: &[Contestant],
        hull_pct: impl Fn(&Contestant) -> f32,
    ) -> Vec<BotBody> {
        let mut bots: Vec<BotBody> = units
            .iter()
            .filter(|u| u.alive && u.team >= 1)
            .map(|u| BotBody {
                id: u.id,
                team: u.team,
                pos: u.position,
                hull_pct: det_clamp(hull_pct(u), 0.0, 1.0),
                level: self.level,
                carrying_flag: board
                    .objectives
                    .iter()
                    .any(|o| o.is_flag() && o.carrier_id == Some(u.id)),
            })
            .collect();
        bots.sort_by_key(|b| b.id);
        bots
    }

    /// Every living bot's goal order at `tick`, in entity-id order: `bots` from [`BotHooks::bodies`], `contacts`
    /// the contacts they know ([`contacts_of`] for "every bot sees every other"), `pickups` the arena's pickups
    /// (read in deathmatch only). Empty for a mode with no bot set ([`bot_mode`]).
    pub fn orders(
        &self,
        board: &ObjectiveBoard,
        tick: u64,
        bots: &[BotBody],
        contacts: &[EnemySight],
        pickups: &[Pickup],
    ) -> Vec<(u32, ModeOrder)> {
        let _span = tracing::trace_span!("rules_bot_orders", bots = bots.len()).entered();
        let Some(mode) = self.mode(board) else {
            return Vec::new();
        };
        let objectives = self.objectives(board, tick);
        mode_orders(mode, bots, contacts, &objectives, pickups)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objective::{HillVolume, ObjectiveSettings};

    fn hooks() -> BotHooks {
        BotHooks {
            level: AiLevel::Hard,
            world_seed: 11,
            free_for_all: false,
        }
    }

    #[test]
    fn the_objective_view_mirrors_the_board() {
        let mut board = ObjectiveBoard::new(MatchMode::CaptureTheFlag, ObjectiveSettings::default());
        board.place_flags(&[2, 1], 50.0, 10).expect("two flags");
        let view = hooks().objectives(&board, 9);
        assert_eq!(view.tick, 9);
        assert_eq!(view.world_seed, 11);
        assert_eq!(view.flags.iter().map(|f| f.team).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(view.flags[0].base, [-50.0, 0.0, 0.0]);
        assert!(view.flags.iter().all(|f| f.at_base && f.carrier.is_none()));
        assert_eq!(view.hill, None);

        let mut koth = ObjectiveBoard::new(MatchMode::KingOfTheHill, ObjectiveSettings::default());
        koth.install_hill_volume(
            3,
            HillVolume {
                min: [-4.0, 0.0, -4.0],
                max: [4.0, 2.0, 4.0],
            },
        )
        .expect("the hill");
        let view = hooks().objectives(&koth, 0);
        let hill = view.hill.expect("the hill");
        assert_eq!(hill.center, [0.0, 1.0, 0.0]);
        assert!(hill.contains([3.0, 1.0, -3.0]) && !hill.contains([5.0, 1.0, 0.0]));
        assert!(view.flags.is_empty());
    }

    #[test]
    fn bodies_clamp_the_hull_and_carry_the_level() {
        let board = ObjectiveBoard::new(MatchMode::TeamDeathmatch, ObjectiveSettings::default());
        let units = [Contestant {
            id: 4,
            team: 2,
            position: [1.0, 2.0, 3.0],
            alive: true,
        }];
        let bots = hooks().bodies(&board, &units, |_| 1.5);
        assert_eq!(bots.len(), 1);
        assert_eq!(bots[0].hull_pct, 1.0);
        assert_eq!(bots[0].level, AiLevel::Hard);
        assert!(!bots[0].carrying_flag);
    }

    #[test]
    fn a_custom_mode_gets_no_bot_orders() {
        let board = ObjectiveBoard::new(MatchMode::Custom, ObjectiveSettings::default());
        let bots = [BotBody {
            id: 1,
            team: 1,
            pos: [0.0; 3],
            hull_pct: 1.0,
            level: AiLevel::Easy,
            carrying_flag: false,
        }];
        assert!(
            hooks()
                .orders(&board, 0, &bots, &contacts_of(&bots), &[])
                .is_empty()
        );
    }
}
