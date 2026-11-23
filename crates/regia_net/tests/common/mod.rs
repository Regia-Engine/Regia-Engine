// SPDX-License-Identifier: MPL-2.0
//! A tiny test game ("duel") shared by the integration tests: two players move along a track;
//! the server owns positions and sends them back. Everything game-specific lives here, in the
//! test, as the payload traits intend.

#![allow(dead_code)]

use regia_net::prelude::*;

/// The test game.
pub struct Duel;

/// Duel messages.
#[derive(Debug, Clone, PartialEq)]
pub enum DuelMsg {
    /// Chat line.
    Chat(String),
    /// Positions at a tick.
    State { tick: u64, positions: Vec<(u16, i32)> },
    /// The winner's peer id.
    Winner(u16),
}

impl NetCodec for DuelMsg {
    fn encode(&self, w: &mut WireWriter) {
        match self {
            DuelMsg::Chat(text) => {
                w.u8(0);
                let _ = w.bytes_u16(text.as_bytes(), 4096);
            }
            DuelMsg::State { tick, positions } => {
                w.u8(1);
                w.u64(*tick);
                w.u8(positions.len() as u8);
                for (peer, pos) in positions {
                    w.u16(*peer);
                    w.i32(*pos);
                }
            }
            DuelMsg::Winner(peer) => {
                w.u8(2);
                w.u16(*peer);
            }
        }
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(DuelMsg::Chat(
                String::from_utf8(r.bytes_u16(4096)?.to_vec()).map_err(|_| WireError::Utf8)?,
            )),
            1 => {
                let tick = r.u64()?;
                let n = r.u8()?;
                let positions = (0..n)
                    .map(|_| Ok((r.u16()?, r.i32()?)))
                    .collect::<Result<_, WireError>>()?;
                Ok(DuelMsg::State { tick, positions })
            }
            2 => Ok(DuelMsg::Winner(r.u16()?)),
            _ => Err(WireError::Invalid("duel message tag")),
        }
    }
}

/// One step left (-1), none (0) or right (+1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Move(pub i8);

impl NetCodec for Move {
    fn encode(&self, w: &mut WireWriter) {
        w.u8(self.0 as u8);
    }
    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        let v = r.u8()? as i8;
        if !(-1..=1).contains(&v) {
            return Err(WireError::Invalid("move"));
        }
        Ok(Move(v))
    }
}

/// The team a player asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Team(pub u8);

impl NetCodec for Team {
    fn encode(&self, w: &mut WireWriter) {
        w.u8(self.0);
    }
    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(Team(r.u8()?))
    }
}

impl NetGame for Duel {
    type Message = DuelMsg;
    type Input = Move;
    type JoinData = Team;
    const GAME_ID: &'static str = "regia.test.duel";
    const PROTOCOL_VERSION: u32 = 1;

    fn validate_join(data: &Team) -> Result<(), String> {
        if data.0 <= 1 {
            Ok(())
        } else {
            Err(format!("no team {}", data.0))
        }
    }
}

/// Same messages, different protocol version: must be refused by a `Duel` server.
pub struct DuelV2;

impl NetGame for DuelV2 {
    type Message = DuelMsg;
    type Input = Move;
    type JoinData = Team;
    const GAME_ID: &'static str = "regia.test.duel";
    const PROTOCOL_VERSION: u32 = 2;
}

/// Applies one tick of duel rules to `positions`.
pub fn simulate(positions: &mut std::collections::BTreeMap<PeerId, i32>, inputs: &TickInputs<Move>) {
    for (peer, mv) in inputs {
        *positions.entry(*peer).or_insert(0) += i32::from(mv.0);
    }
}
