// SPDX-License-Identifier: MPL-2.0
//! ECS-free, lockstep-safe match rules (ADR-8239).
//!
//! The host applies ordered inputs and steps [`session::MatchSession`] on each agreed simulation
//! tick. Modes, objectives, capture the flag, king of the hill, pause, ready-up and playlists use
//! plain data with no ECS, GPU or platform dependency. Settings and computations retain S-14's
//! behavior (ADR-8102); `regia_framework::rules` re-exports these same types for existing callers.
//!
//! The host owns the entities, supplies contestants in deterministic order, and consumes the
//! returned events. See `book/agents/match-rules.md` and `book/user-guide/game-modes.md`.

pub mod ctf;
pub mod end_stats;
mod error;
pub mod koth;
pub mod map_vote;
pub mod match_flow;
pub mod mode;
pub mod objective;
pub mod phases;
pub mod playlist;
pub mod proof;
pub mod series;
pub mod session;
pub mod spawn;
pub mod teams;
pub mod vision;
pub mod win_condition;
// Deterministic mode bots hooked to the board (plan row S-13, ADR-8554; moved here from
// `regia_framework::rules::bots` with the crate split, ADR-8637).
pub mod bots;
pub mod zones;
// Rule sets: a mode as data (`.rules.ron` v1) and the canonical bytes its content hash covers
// (ADR-8694).
mod canon;
pub mod ruleset;
// Co-op PvE waves and races (audit gaps GR-11.1 and GR-11.2).
pub mod race;
pub mod waves;

// Shrinking zones, team elimination with placements, and strategy victory conditions (GR-11.3,
// GR-11.4).
pub mod elimination;
pub mod victory;
pub mod zone;

pub use error::RulesError;
