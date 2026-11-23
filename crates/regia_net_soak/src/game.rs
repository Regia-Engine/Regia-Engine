// SPDX-License-Identifier: MPL-2.0
//! The soak's sample game: its wire vocabulary and the hostile peer's.
//!
//! Every player moves one avatar on a grid from its per-tick input; the server simulates the
//! inputs that arrived (a missing input moves nothing), and each tick's snapshot tells every
//! client where the server put its avatar after the newest simulated tick, plus as many other
//! entities as its congestion budget allows. The client predicts its own avatar from its inputs
//! and counts a correction whenever the server disagrees. Inputs carry an optional command tail
//! (0 to [`MAX_COMMAND_SAMPLE`] bytes); bigger commands travel as reliable [`SoakMsg::Command`]
//! messages of up to [`MAX_RELIABLE_COMMAND`] bytes.

use regia_net::{NetCodec, NetGame, WireError, WireReader, WireWriter};

/// Largest command tail of one input sample, bytes (a legal sample; DO27's large samples are
/// about half this).
pub const MAX_COMMAND_SAMPLE: usize = 1024;
/// Largest reliable command, bytes.
pub const MAX_RELIABLE_COMMAND: usize = 4096;
/// Largest world blob the server sends on join, bytes.
pub const MAX_WORLD_BLOB: usize = 32 * 1024;
/// Largest chunk of a bulk transfer, bytes.
pub const BULK_CHUNK: usize = 32 * 1024;
/// Most entities one snapshot part may list.
pub const MAX_PART_ENTITIES: usize = 512;
/// Grid units an avatar moves per tick per unit of input.
pub const STEP_UNITS: i32 = 16;

/// The sample game.
#[derive(Debug, Clone, Copy)]
pub struct SoakGame;

impl NetGame for SoakGame {
    type Message = SoakMsg;
    type Input = SoakInput;
    /// The client's index in the run.
    type JoinData = u32;
    const GAME_ID: &'static str = "regia.net_soak";
    const PROTOCOL_VERSION: u32 = 1;
}

/// One tick of player input.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SoakInput {
    /// Move along x: -1, 0 or 1.
    pub dx: i8,
    /// Move along y: -1, 0 or 1.
    pub dy: i8,
    /// Button bits.
    pub buttons: u8,
    /// The command sample of this tick (0..=[`MAX_COMMAND_SAMPLE`] bytes).
    pub command: Vec<u8>,
}

impl NetCodec for SoakInput {
    fn encode(&self, w: &mut WireWriter) {
        w.u8(self.dx as u8);
        w.u8(self.dy as u8);
        w.u8(self.buttons);
        if let Err(e) = w.bytes_u16(&self.command, MAX_COMMAND_SAMPLE) {
            w.fail(e);
        }
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        let dx = r.u8()? as i8;
        let dy = r.u8()? as i8;
        let buttons = r.u8()?;
        let command = r.bytes_u16(MAX_COMMAND_SAMPLE)?.to_vec();
        if !(-1..=1).contains(&dx) || !(-1..=1).contains(&dy) {
            return Err(WireError::Invalid("move out of range"));
        }
        Ok(Self {
            dx,
            dy,
            buttons,
            command,
        })
    }
}

/// One replicated entity in a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityState {
    /// Entity id.
    pub id: u16,
    /// Position x, grid units.
    pub x: i32,
    /// Position y, grid units.
    pub y: i32,
    /// Heading.
    pub yaw: u16,
}

/// Bytes one [`EntityState`] takes on the wire.
pub const ENTITY_BYTES: usize = 12;

/// The game's messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoakMsg {
    /// Unreliable, every tick: the newest simulated tick, where the server put the receiver's
    /// avatar after it, and one part of the entity list.
    Snapshot {
        /// Newest simulated tick.
        tick: u64,
        /// Receiver's avatar x after `tick`.
        own_x: i32,
        /// Receiver's avatar y after `tick`.
        own_y: i32,
        /// Part index.
        part: u8,
        /// Parts of this tick's snapshot.
        parts: u8,
        /// Entities of this part.
        entities: Vec<EntityState>,
    },
    /// Reliable, once on join: a world blob (fragmented by the transport).
    World {
        /// Tick it describes.
        tick: u64,
        /// The blob.
        blob: Vec<u8>,
    },
    /// Reliable, client to server: a large command.
    Command {
        /// Client-chosen id.
        id: u32,
        /// The command bytes.
        payload: Vec<u8>,
    },
    /// Reliable, server to client: the command with this id arrived.
    CommandAck {
        /// Id of the command.
        id: u32,
    },
    /// Reliable, both ways: a small chat line.
    Chat {
        /// Sender's sequence number.
        seq: u32,
        /// Text bytes.
        text: Vec<u8>,
    },
    /// Reliable, server to client: one chunk of a bulk transfer (a map or patch download).
    Bulk {
        /// Chunk index.
        seq: u32,
        /// Chunks in the whole transfer.
        of: u32,
        /// The bytes (up to [`BULK_CHUNK`]).
        data: Vec<u8>,
    },
}

const TAG_SNAPSHOT: u8 = 1;
const TAG_WORLD: u8 = 2;
const TAG_COMMAND: u8 = 3;
const TAG_COMMAND_ACK: u8 = 4;
const TAG_CHAT: u8 = 5;
const TAG_BULK: u8 = 6;

impl NetCodec for SoakMsg {
    fn encode(&self, w: &mut WireWriter) {
        let result = match self {
            Self::Snapshot {
                tick,
                own_x,
                own_y,
                part,
                parts,
                entities,
            } => {
                w.u8(TAG_SNAPSHOT);
                w.u64(*tick);
                w.i32(*own_x);
                w.i32(*own_y);
                w.u8(*part);
                w.u8(*parts);
                let n = entities.len().min(MAX_PART_ENTITIES);
                w.u16(n as u16);
                for e in &entities[..n] {
                    w.u16(e.id);
                    w.i32(e.x);
                    w.i32(e.y);
                    w.u16(e.yaw);
                }
                Ok(())
            }
            Self::World { tick, blob } => {
                w.u8(TAG_WORLD);
                w.u64(*tick);
                w.bytes_u16(blob, MAX_WORLD_BLOB)
            }
            Self::Command { id, payload } => {
                w.u8(TAG_COMMAND);
                w.u32(*id);
                w.bytes_u16(payload, MAX_RELIABLE_COMMAND)
            }
            Self::CommandAck { id } => {
                w.u8(TAG_COMMAND_ACK);
                w.u32(*id);
                Ok(())
            }
            Self::Chat { seq, text } => {
                w.u8(TAG_CHAT);
                w.u32(*seq);
                w.bytes_u16(text, 255)
            }
            Self::Bulk { seq, of, data } => {
                w.u8(TAG_BULK);
                w.u32(*seq);
                w.u32(*of);
                w.bytes_u16(data, BULK_CHUNK)
            }
        };
        if let Err(e) = result {
            w.fail(e);
        }
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(match r.u8()? {
            TAG_SNAPSHOT => {
                let tick = r.u64()?;
                let own_x = r.i32()?;
                let own_y = r.i32()?;
                let part = r.u8()?;
                let parts = r.u8()?;
                let n = usize::from(r.u16()?);
                if n > MAX_PART_ENTITIES {
                    return Err(WireError::TooLong {
                        len: n,
                        max: MAX_PART_ENTITIES,
                    });
                }
                let mut entities = Vec::with_capacity(n.min(r.remaining() / ENTITY_BYTES));
                for _ in 0..n {
                    entities.push(EntityState {
                        id: r.u16()?,
                        x: r.i32()?,
                        y: r.i32()?,
                        yaw: r.u16()?,
                    });
                }
                Self::Snapshot {
                    tick,
                    own_x,
                    own_y,
                    part,
                    parts,
                    entities,
                }
            }
            TAG_WORLD => Self::World {
                tick: r.u64()?,
                blob: r.bytes_u16(MAX_WORLD_BLOB)?.to_vec(),
            },
            TAG_COMMAND => Self::Command {
                id: r.u32()?,
                payload: r.bytes_u16(MAX_RELIABLE_COMMAND)?.to_vec(),
            },
            TAG_COMMAND_ACK => Self::CommandAck { id: r.u32()? },
            TAG_CHAT => Self::Chat {
                seq: r.u32()?,
                text: r.bytes_u16(255)?.to_vec(),
            },
            TAG_BULK => Self::Bulk {
                seq: r.u32()?,
                of: r.u32()?,
                data: r.bytes_u16(BULK_CHUNK)?.to_vec(),
            },
            _ => return Err(WireError::Invalid("unknown soak message")),
        })
    }
}

/// The hostile peer's view of the same game: the same id and protocol version (so the handshake
/// passes), but messages and inputs are raw bytes it shapes itself.
#[derive(Debug, Clone, Copy)]
pub struct HostileGame;

/// Raw bytes written without a length prefix.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawBytes(pub Vec<u8>);

impl NetCodec for RawBytes {
    fn encode(&self, w: &mut WireWriter) {
        w.raw(&self.0);
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(Self(r.take(r.remaining())?.to_vec()))
    }
}

impl NetGame for HostileGame {
    type Message = RawBytes;
    type Input = RawBytes;
    type JoinData = u32;
    const GAME_ID: &'static str = SoakGame::GAME_ID;
    const PROTOCOL_VERSION: u32 = SoakGame::PROTOCOL_VERSION;
}

/// A hostile input sample: a valid header whose command tail claims `claimed` bytes (over the cap
/// when `claimed > MAX_COMMAND_SAMPLE`) and carries `carried`.
pub fn over_cap_input(claimed: u16, carried: usize) -> RawBytes {
    let mut bytes = vec![0u8, 0, 0];
    bytes.extend_from_slice(&claimed.to_le_bytes());
    bytes.resize(bytes.len() + carried, 0xA5);
    RawBytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let msgs = [
            SoakMsg::Snapshot {
                tick: 9,
                own_x: -3,
                own_y: 4,
                part: 1,
                parts: 2,
                entities: vec![EntityState {
                    id: 7,
                    x: 1,
                    y: -1,
                    yaw: 90,
                }],
            },
            SoakMsg::World {
                tick: 3,
                blob: vec![1; 20_000],
            },
            SoakMsg::Command {
                id: 5,
                payload: vec![2; 4096],
            },
            SoakMsg::CommandAck { id: 5 },
            SoakMsg::Chat {
                seq: 1,
                text: b"hi".to_vec(),
            },
            SoakMsg::Bulk {
                seq: 2,
                of: 8,
                data: vec![3; BULK_CHUNK],
            },
        ];
        for m in msgs {
            assert_eq!(SoakMsg::from_bytes(&m.to_bytes_for_test()).expect("decode"), m);
        }
        let input = SoakInput {
            dx: -1,
            dy: 1,
            buttons: 3,
            command: vec![9; MAX_COMMAND_SAMPLE],
        };
        let mut w = WireWriter::new();
        input.encode(&mut w);
        assert_eq!(SoakInput::from_bytes(&w.finish()).expect("decode"), input);
    }

    #[test]
    fn over_cap_input_is_refused_as_too_long() {
        let raw = over_cap_input(60_000, 8);
        let err = SoakInput::from_bytes(&raw.0).expect_err("over the cap");
        assert!(matches!(err, WireError::TooLong { len: 60_000, .. }), "{err:?}");
    }

    trait ToBytes {
        fn to_bytes_for_test(&self) -> Vec<u8>;
    }

    impl ToBytes for SoakMsg {
        fn to_bytes_for_test(&self) -> Vec<u8> {
            let mut w = WireWriter::new();
            self.encode(&mut w);
            w.finish()
        }
    }
}
