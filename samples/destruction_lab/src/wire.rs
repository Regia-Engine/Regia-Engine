// SPDX-License-Identifier: MIT-0
//! The game's own message for a blast: destruction over the network carries breaks, and a game
//! decides for itself what else about an explosion its clients hear.

use regia::framework::explosion::{Explosion, Falloff, Occlusion};
use regia::net::bits::{BitReader, BitWriter, DecodeError};
use regia::net::codec::{CodecContext, LayoutHasher, ReplicateCodec};
use regia::net::message::{BroadcastMessage, Direction, NetMessage};

/// An explosion the server set off. Reliable and ordered, like the stream of its breaks. The
/// receiver checks it with [`Explosion::validate`]; `ignore` names a server entity, so it stays
/// behind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LabBlast(pub Explosion);

impl NetMessage for LabBlast {
    const DIRECTION: Direction = Direction::Broadcast;
}
impl BroadcastMessage for LabBlast {}

impl ReplicateCodec for LabBlast {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        let e = &self.0;
        for c in e.center.to_array() {
            w.write_f32(c);
        }
        for v in [e.tnt_kg, e.radius, e.impulse, e.upward_bias, e.torque] {
            w.write_f32(v);
        }
        match e.falloff {
            Falloff::Linear => w.write_bool(false),
            Falloff::InverseSquare { floor } => {
                w.write_bool(true);
                w.write_f32(floor);
            }
        }
        w.write_bool(e.occlusion == Occlusion::RayCast);
        w.write_varint(e.seed);
        w.write_bool(e.destruction);
        w.write_bool(e.gust);
        w.write_bool(e.fx);
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let center = regia::core::math::Vec3::new(r.read_f32()?, r.read_f32()?, r.read_f32()?);
        let (tnt_kg, radius, impulse) = (r.read_f32()?, r.read_f32()?, r.read_f32()?);
        let (upward_bias, torque) = (r.read_f32()?, r.read_f32()?);
        let falloff = if r.read_bool()? {
            Falloff::InverseSquare { floor: r.read_f32()? }
        } else {
            Falloff::Linear
        };
        let occlusion = if r.read_bool()? {
            Occlusion::RayCast
        } else {
            Occlusion::None
        };
        Ok(Self(Explosion {
            center,
            tnt_kg,
            radius,
            impulse,
            falloff,
            upward_bias,
            occlusion,
            torque,
            seed: r.read_varint()?,
            ignore: None,
            destruction: r.read_bool()?,
            gust: r.read_bool()?,
            fx: r.read_bool()?,
        }))
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str("destruction_lab::LabBlast/1");
    }
}

#[cfg(test)]
mod tests {
    use regia::core::math::Vec3;

    use super::*;

    fn round_trip(blast: LabBlast) -> LabBlast {
        let cx = CodecContext::detached();
        let mut w = BitWriter::new();
        blast.encode(&mut w, &cx);
        let bytes = w.finish();
        LabBlast::decode(&mut BitReader::new(&bytes), &cx).expect("decodes")
    }

    #[test]
    fn a_blast_survives_the_wire() {
        for blast in [
            Explosion::grenade(Vec3::new(1.0, 2.0, 3.0)),
            Explosion::barrel(Vec3::new(-1.0, 0.5, 9.0)),
            Explosion {
                occlusion: Occlusion::None,
                destruction: false,
                gust: false,
                fx: false,
                seed: u64::MAX,
                ..Explosion::bomb(Vec3::ZERO)
            },
        ] {
            assert_eq!(round_trip(LabBlast(blast)).0, blast);
        }
    }

    #[test]
    fn a_cut_message_is_refused_and_ignore_stays_behind() {
        let cx = CodecContext::detached();
        let mut w = BitWriter::new();
        LabBlast(Explosion::grenade(Vec3::ZERO)).encode(&mut w, &cx);
        let bytes = w.finish();
        assert!(LabBlast::decode(&mut BitReader::new(&bytes[..4]), &cx).is_err());
        let blast = Explosion {
            ignore: Some(regia::ecs::prelude::Entity::PLACEHOLDER),
            ..Explosion::grenade(Vec3::ZERO)
        };
        assert_eq!(round_trip(LabBlast(blast)).0.ignore, None);
    }
}
