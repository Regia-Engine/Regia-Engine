// SPDX-License-Identifier: MPL-2.0
//! `#[derive(NetMessage)]` (spec 70 § 6.1, ADR-2415): a derived message carries the same
//! constants and travels as the same bytes as the hand-written impls the net tests use, its id is
//! stable, its version feeds the protocol hash, and it runs end to end over the loopback hub.

use glam::Vec3;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::{CodecContext, LayoutHasher, ReplicateCodec, decode_from_slice};
use regia_net::message::{
    Channel, NetMessageRegistry, ValidateContext, Verdict, message_stable_id, register_net_message,
};
use regia_net::replication::register_replicated;
use regia_net::runtime::NetEntities;
use regia_net::settings::RateLimit;
use regia_net::testing::LoopbackHub;
use regia_net::*;

/// The hand-written forms, as `tests/net_model.rs` writes them (codec by `#[derive(Replicate)]`,
/// the trait by hand), plus one with a fully hand-written codec.
mod hand {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Replicate)]
    pub struct Fire {
        pub power: u32,
    }
    impl NetMessage for Fire {
        const DIRECTION: Direction = Direction::ToServer;
        fn validate(&self, _cx: &ValidateContext) -> Verdict {
            super::check_power(self.power)
        }
    }
    impl ToServerMessage for Fire {}

    #[derive(Clone, Debug, PartialEq, Replicate)]
    pub struct Ping {
        pub n: u32,
    }
    impl NetMessage for Ping {
        const DIRECTION: Direction = Direction::ToOwner;
        const CHANNEL: Channel = Channel::UnreliableSequenced;
    }
    impl ToOwnerMessage for Ping {}

    #[derive(Clone, Debug, PartialEq, Replicate)]
    pub struct Blob {
        #[replicate(max_len = 200000)]
        pub bytes: Vec<u8>,
    }
    impl NetMessage for Blob {
        const DIRECTION: Direction = Direction::Broadcast;
    }
    impl BroadcastMessage for Blob {}

    #[derive(Clone, Debug, PartialEq, Replicate)]
    pub struct Beat;
    impl NetMessage for Beat {
        const DIRECTION: Direction = Direction::ToServer;
        const ALWAYS_ADMIT: bool = true;
    }
    impl ToServerMessage for Beat {}

    #[derive(Clone, Debug, PartialEq, Replicate)]
    pub struct Aligned {
        pub n: u32,
    }
    impl NetMessage for Aligned {
        const DIRECTION: Direction = Direction::Broadcast;
        const CHANNEL: Channel = Channel::Unreliable;
        const ALIGN_TO_VIEW: bool = true;
        const SKIP_PREDICTING_OWNER: bool = true;
        const RATE_LIMIT: Option<RateLimit> = Some(RateLimit {
            count: 10,
            per_ticks: 60,
        });
    }
    impl BroadcastMessage for Aligned {}

    /// Every field encoded by hand, in declaration order, with the field types' own codecs.
    #[derive(Clone, Debug, PartialEq)]
    pub struct Shot {
        pub target: Entity,
        pub at: Vec3,
        pub power: i32,
        pub tag: Option<String>,
        pub hits: Vec<u16>,
        pub crit: bool,
        pub kind: Kind,
    }
    impl ReplicateCodec for Shot {
        fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
            ReplicateCodec::encode(&self.target, w, cx);
            ReplicateCodec::encode(&self.at, w, cx);
            ReplicateCodec::encode(&self.power, w, cx);
            ReplicateCodec::encode(&self.tag, w, cx);
            ReplicateCodec::encode(&self.hits, w, cx);
            ReplicateCodec::encode(&self.crit, w, cx);
            ReplicateCodec::encode(&self.kind, w, cx);
        }
        fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
            Ok(Self {
                target: <Entity as ReplicateCodec>::decode(r, cx)?,
                at: <Vec3 as ReplicateCodec>::decode(r, cx)?,
                power: <i32 as ReplicateCodec>::decode(r, cx)?,
                tag: <Option<String> as ReplicateCodec>::decode(r, cx)?,
                hits: <Vec<u16> as ReplicateCodec>::decode(r, cx)?,
                crit: <bool as ReplicateCodec>::decode(r, cx)?,
                kind: <Kind as ReplicateCodec>::decode(r, cx)?,
            })
        }
    }
    impl NetMessage for Shot {
        const DIRECTION: Direction = Direction::ToServer;
        const CHANNEL: Channel = Channel::ReliableUnordered;
    }
    impl ToServerMessage for Shot {}

    /// An enum: a varint tag, then the variant's fields.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Kind {
        Miss,
        Hit(u8),
        Graze { by: f32 },
    }
    impl ReplicateCodec for Kind {
        fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
            match self {
                Kind::Miss => w.write_varint(0),
                Kind::Hit(n) => {
                    w.write_varint(1);
                    ReplicateCodec::encode(n, w, cx);
                }
                Kind::Graze { by } => {
                    w.write_varint(2);
                    ReplicateCodec::encode(by, w, cx);
                }
            }
        }
        fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
            Ok(match r.read_varint()? {
                0 => Kind::Miss,
                1 => Kind::Hit(<u8 as ReplicateCodec>::decode(r, cx)?),
                2 => Kind::Graze {
                    by: <f32 as ReplicateCodec>::decode(r, cx)?,
                },
                _ => return Err(DecodeError::Invalid("kind")),
            })
        }
    }
}

/// The same messages with `#[derive(NetMessage)]`.
mod derived {
    use super::*;

    #[derive(Clone, Debug, PartialEq, NetMessage)]
    #[net(direction = "to_server", validate = Self::check)]
    pub struct Fire {
        pub power: u32,
    }
    impl Fire {
        fn check(&self, _cx: &ValidateContext) -> Verdict {
            super::check_power(self.power)
        }
    }

    #[derive(Clone, Debug, PartialEq, NetMessage)]
    #[net(direction = "to_owner", channel = "unreliable_sequenced")]
    pub struct Ping {
        pub n: u32,
    }

    #[derive(Clone, Debug, PartialEq, NetMessage)]
    #[net(direction = "broadcast")]
    pub struct Blob {
        #[replicate(max_len = 200000)]
        pub bytes: Vec<u8>,
    }

    #[derive(Clone, Debug, PartialEq, NetMessage)]
    #[net(direction = "to_server", always_admit)]
    pub struct Beat;

    #[derive(Clone, Debug, PartialEq, NetMessage)]
    #[net(direction = "broadcast", channel = "unreliable", rate = "10/60")]
    #[net(align_to_view, skip_predicting_owner = true)]
    pub struct Aligned {
        pub n: u32,
    }

    #[derive(Clone, Debug, PartialEq, NetMessage)]
    #[net(direction = "to_server", channel = "reliable_unordered")]
    pub struct Shot {
        pub target: Entity,
        pub at: Vec3,
        pub power: i32,
        pub tag: Option<String>,
        pub hits: Vec<u16>,
        pub crit: bool,
        pub kind: Kind,
    }

    #[derive(Clone, Debug, PartialEq, Replicate)]
    pub enum Kind {
        Miss,
        Hit(u8),
        Graze { by: f32 },
    }

    /// An explicit id and version, with the codec written by `#[derive(Replicate)]`.
    #[derive(Clone, Debug, PartialEq, Replicate, NetMessage)]
    #[net(direction = "broadcast", id = "game.explosion", version = 2, codec = "manual")]
    pub struct Explosion {
        #[replicate(quantize(min = -100.0, max = 100.0, bits = 12))]
        pub radius: f32,
    }

    /// The same type at version 1, for the protocol-hash check.
    pub mod v1 {
        use super::*;
        #[derive(Clone, Debug, PartialEq, NetMessage)]
        #[net(direction = "broadcast", id = "game.explosion")]
        pub struct Explosion {
            #[replicate(quantize(min = -100.0, max = 100.0, bits = 12))]
            pub radius: f32,
        }
    }
}

fn check_power(power: u32) -> Verdict {
    match power {
        0 => Verdict::Reject("no power"),
        99 => Verdict::Kick("cheat"),
        _ => Verdict::Accept,
    }
}

/// The constants a registry stores, compared field by field (the name aside).
fn same_consts<A: NetMessage, B: NetMessage>() {
    let (a, b) = (std::any::type_name::<A>(), std::any::type_name::<B>());
    assert_eq!(A::DIRECTION, B::DIRECTION, "{a} vs {b}");
    assert_eq!(A::CHANNEL, B::CHANNEL, "{a} vs {b}");
    assert_eq!(A::RATE_LIMIT, B::RATE_LIMIT, "{a} vs {b}");
    assert_eq!(A::ALWAYS_ADMIT, B::ALWAYS_ADMIT, "{a} vs {b}");
    assert_eq!(A::ALIGN_TO_VIEW, B::ALIGN_TO_VIEW, "{a} vs {b}");
    assert_eq!(A::SKIP_PREDICTING_OWNER, B::SKIP_PREDICTING_OWNER, "{a} vs {b}");
    assert_eq!(A::VERSION, B::VERSION, "{a} vs {b}");
}

/// Encodes both, checks the bytes and the bit length are equal, and decodes each form from the
/// other's bytes.
fn same_bytes<A, B>(a: &A, b: &B, cx: &CodecContext)
where
    A: ReplicateCodec + PartialEq + std::fmt::Debug,
    B: ReplicateCodec + PartialEq + std::fmt::Debug,
{
    let mut wa = BitWriter::new();
    ReplicateCodec::encode(a, &mut wa, cx);
    let mut wb = BitWriter::new();
    ReplicateCodec::encode(b, &mut wb, cx);
    assert_eq!(wa.bit_len(), wb.bit_len(), "{a:?} vs {b:?}");
    let (ba, bb) = (wa.finish(), wb.finish());
    assert_eq!(ba, bb, "{a:?} vs {b:?}");
    let back_b: B = decode_from_slice(&ba, cx).expect("derived decodes hand bytes");
    assert_eq!(&back_b, b);
    let back_a: A = decode_from_slice(&bb, cx).expect("hand decodes derived bytes");
    assert_eq!(&back_a, a);
}

#[test]
fn derived_constants_match_the_hand_written_impls() {
    same_consts::<hand::Fire, derived::Fire>();
    same_consts::<hand::Ping, derived::Ping>();
    same_consts::<hand::Blob, derived::Blob>();
    same_consts::<hand::Beat, derived::Beat>();
    same_consts::<hand::Aligned, derived::Aligned>();
    same_consts::<hand::Shot, derived::Shot>();
    // The validate hook runs the named function.
    let world = World::new();
    let cx = ValidateContext {
        from: PeerId::SERVER,
        target: None,
        world: &world,
        tick: NetTick(0),
    };
    for power in [0, 1, 99] {
        assert_eq!(
            derived::Fire { power }.validate(&cx),
            hand::Fire { power }.validate(&cx)
        );
    }
}

#[test]
fn derived_messages_round_trip_byte_identically() {
    let cx = CodecContext::detached();
    for power in [0u32, 1, 127, 128, 300, u32::MAX] {
        same_bytes(&hand::Fire { power }, &derived::Fire { power }, &cx);
        same_bytes(&hand::Ping { n: power }, &derived::Ping { n: power }, &cx);
        same_bytes(&hand::Aligned { n: power }, &derived::Aligned { n: power }, &cx);
    }
    for len in [0usize, 1, 1025, 4096] {
        let bytes: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        same_bytes(
            &hand::Blob { bytes: bytes.clone() },
            &derived::Blob { bytes },
            &cx,
        );
    }
    same_bytes(&hand::Beat, &derived::Beat, &cx);
    let kinds = [
        (hand::Kind::Miss, derived::Kind::Miss),
        (hand::Kind::Hit(200), derived::Kind::Hit(200)),
        (
            hand::Kind::Graze { by: -0.25 },
            derived::Kind::Graze { by: -0.25 },
        ),
    ];
    for (i, (hk, dk)) in kinds.into_iter().enumerate() {
        let tag = (i % 2 == 0).then(|| format!("shot-{i}"));
        let hits: Vec<u16> = (0..i as u16 * 3).collect();
        same_bytes(
            &hand::Shot {
                target: Entity::PLACEHOLDER,
                at: Vec3::new(1.5, -2.0, i as f32),
                power: -7 * i as i32,
                tag: tag.clone(),
                hits: hits.clone(),
                crit: i == 1,
                kind: hk,
            },
            &derived::Shot {
                target: Entity::PLACEHOLDER,
                at: Vec3::new(1.5, -2.0, i as f32),
                power: -7 * i as i32,
                tag,
                hits,
                crit: i == 1,
                kind: dk,
            },
            &cx,
        );
    }
}

/// Entity fields map through the codec context the same way in both forms.
#[test]
fn derived_entity_fields_travel_as_net_ids() {
    struct Map;
    impl regia_net::codec::EntityMap for Map {
        fn to_net(&self, e: Entity) -> Option<NetId> {
            (e.index_u32() == 4).then_some(NetId(11))
        }
        fn to_entity(&self, id: NetId) -> Option<Entity> {
            (id == NetId(11)).then(|| Entity::from_raw_u32(4).unwrap_or(Entity::PLACEHOLDER))
        }
    }
    let map = Map;
    let cx = CodecContext::new(&map);
    let target = Entity::from_raw_u32(4).unwrap_or(Entity::PLACEHOLDER);
    same_bytes(
        &hand::Shot {
            target,
            at: Vec3::ZERO,
            power: 3,
            tag: None,
            hits: vec![],
            crit: false,
            kind: hand::Kind::Miss,
        },
        &derived::Shot {
            target,
            at: Vec3::ZERO,
            power: 3,
            tag: None,
            hits: vec![],
            crit: false,
            kind: derived::Kind::Miss,
        },
        &cx,
    );
}

#[test]
fn message_ids_are_stable_and_attributes_win() {
    // Without an id the derive names the type by module path, which is what the type path says.
    assert_eq!(
        <derived::Fire as NetMessage>::message_id(),
        std::any::type_name::<derived::Fire>()
    );
    assert_eq!(
        <derived::Fire as NetMessage>::message_id(),
        "derive_net_message::derived::Fire"
    );
    assert_eq!(
        <hand::Fire as NetMessage>::message_id(),
        "derive_net_message::hand::Fire"
    );
    assert_eq!(<derived::Explosion as NetMessage>::message_id(), "game.explosion");
    assert_eq!(<derived::Explosion as NetMessage>::VERSION, 2);
    assert_eq!(<derived::v1::Explosion as NetMessage>::VERSION, 1);

    let mut reg = NetMessageRegistry::default();
    let i = reg.register::<derived::Explosion>();
    let t = reg.get(i).expect("registered");
    assert_eq!(t.name, "game.explosion");
    assert_eq!(t.version, 2);
    assert_eq!(t.stable_id, message_stable_id("game.explosion"));
    assert_eq!(reg.find("game.explosion"), Some(i));
    assert_eq!(reg.find_stable(t.stable_id), Some(i));
    assert_eq!(reg.find_stable(message_stable_id("game.other")), None);
    // The stable id is a pinned function of the string: the same on every build and platform.
    assert_eq!(
        message_stable_id("game.explosion"),
        message_stable_id("game.explosion")
    );
    assert_ne!(
        message_stable_id("game.explosion"),
        message_stable_id("game.explosion2")
    );
}

#[test]
fn version_feeds_the_protocol_hash() {
    fn hash_of(f: impl FnOnce(&mut NetMessageRegistry)) -> u64 {
        let mut reg = NetMessageRegistry::default();
        f(&mut reg);
        let mut h = LayoutHasher::new();
        reg.hash_into(&mut h);
        h.finish()
    }
    let v2 = hash_of(|r| {
        r.register::<derived::Explosion>();
    });
    let v1 = hash_of(|r| {
        r.register::<derived::v1::Explosion>();
    });
    let v2_again = hash_of(|r| {
        r.register::<derived::Explosion>();
    });
    assert_ne!(v1, v2, "a version bump changes the protocol hash");
    assert_eq!(v2, v2_again);
}

// ---- end to end over the loopback hub ------------------------------------------------------------

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Health {
    hp: u32,
}

#[derive(Resource, Default)]
struct Seen(Vec<(u32, i32)>);

fn setup(world: &mut World) {
    register_net_message::<derived::Fire>(world);
    register_net_message::<derived::Shot>(world);
    register_net_message::<derived::Explosion>(world);
    register_replicated::<Health>(world, ReplicateOptions::default());
}

fn send<M: Send + 'static>(
    world: &mut World,
    f: impl FnOnce(&mut NetSender) -> M + Send + Sync + 'static,
) -> M {
    let mut f = Some(f);
    world
        .run_system_once(move |mut net: NetSender| (f.take().expect("runs once"))(&mut net))
        .expect("system runs")
}

#[test]
fn derived_messages_run_end_to_end() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    hub.server.world.init_resource::<Seen>();
    hub.server.gameplay.add_systems(
        |shots: Res<NetMessages<derived::Shot>>,
         fire: Res<NetMessages<derived::Fire>>,
         mut seen: ResMut<Seen>| {
            for m in shots.iter() {
                seen.0.push((m.from.0, m.msg.power));
            }
            for m in fire.iter() {
                seen.0.push((m.from.0, m.msg.power as i32 * 1000));
            }
        },
    );
    let pawn = hub
        .server
        .world
        .spawn((Replicated::default(), Health { hp: 9 }, NetOwner(pa)))
        .id();
    hub.run(3);
    let id = *hub.server.world.get::<NetId>(pawn).expect("id");
    let copy = hub.clients[a]
        .world
        .resource::<NetEntities>()
        .entity(id)
        .expect("copy");
    send(&mut hub.clients[a].world, move |n| {
        n.send_to_server(
            copy,
            derived::Shot {
                target: copy,
                at: Vec3::X,
                power: -42,
                tag: Some("t".into()),
                hits: vec![1, 2],
                crit: true,
                kind: derived::Kind::Graze { by: 0.5 },
            },
        )
    })
    .expect("shot sent");
    // validate rejects power 0; power 4 runs.
    send(&mut hub.clients[a].world, move |n| {
        n.send_to_server(copy, derived::Fire { power: 0 })
    })
    .expect("sent");
    send(&mut hub.clients[a].world, move |n| {
        n.send_to_server(copy, derived::Fire { power: 4 })
    })
    .expect("sent");
    hub.run(4);
    let seen = &hub.server.world.resource::<Seen>().0;
    assert!(seen.contains(&(pa.0, -42)), "{seen:?}");
    assert!(seen.contains(&(pa.0, 4000)), "{seen:?}");
    assert!(!seen.contains(&(pa.0, 0)), "validate rejected power 0: {seen:?}");
    let reg = hub.server.world.resource::<NetMessageRegistry>();
    let fire = reg.index_of::<derived::Fire>().expect("registered");
    assert_eq!(reg.counters[usize::from(fire)].rejected, 1);
    // A to-server Explosion is a compile error (it is a broadcast); a session broadcast from the
    // server reaches the client by its explicit id.
    send(&mut hub.server.world, |n| {
        n.broadcast_session(derived::Explosion { radius: 12.5 })
    })
    .expect("broadcast");
    hub.run(3);
    let reg = hub.clients[a].world.resource::<NetMessageRegistry>();
    let i = reg.find("game.explosion").expect("found by id");
    assert_eq!(reg.counters[usize::from(i)].delivered, 1);
}

/// A generic message: the derive bounds `Self` instead of guessing parameter bounds, and the id
/// stays the type path (which names the parameters).
#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(direction = "to_owner", channel = "unreliable_sequenced")]
struct Wrapped<T: Clone + std::fmt::Debug + PartialEq> {
    n: T,
}

#[test]
fn generic_derived_messages_work() {
    same_consts::<hand::Ping, Wrapped<u32>>();
    assert_eq!(
        <Wrapped<u32> as NetMessage>::message_id(),
        std::any::type_name::<Wrapped<u32>>()
    );
    assert_ne!(
        <Wrapped<u32> as NetMessage>::message_id(),
        <Wrapped<u16> as NetMessage>::message_id()
    );
    let cx = CodecContext::detached();
    same_bytes(&hand::Ping { n: 77 }, &Wrapped { n: 77u32 }, &cx);
    let mut reg = NetMessageRegistry::default();
    assert_eq!(reg.register::<Wrapped<u32>>(), 0);
    assert_eq!(reg.register::<Wrapped<u16>>(), 1);
}

/// Spec 70 § 6.1 (lane spec-28-68): the protocol hash covers every registered message type's
/// direction, channel and field layout, so builds that disagree on any of them cannot join. The
/// four types below share one message id and differ in exactly one of those.
#[test]
fn direction_channel_and_layout_feed_the_protocol_hash() {
    mod same_id {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Replicate)]
        pub struct Base {
            pub a: u32,
        }
        impl NetMessage for Base {
            const DIRECTION: Direction = Direction::ToServer;
            fn message_id() -> &'static str {
                "proto.same"
            }
        }

        #[derive(Clone, Debug, PartialEq, Replicate)]
        pub struct OtherChannel {
            pub a: u32,
        }
        impl NetMessage for OtherChannel {
            const DIRECTION: Direction = Direction::ToServer;
            const CHANNEL: Channel = Channel::Unreliable;
            fn message_id() -> &'static str {
                "proto.same"
            }
        }

        #[derive(Clone, Debug, PartialEq, Replicate)]
        pub struct OtherDirection {
            pub a: u32,
        }
        impl NetMessage for OtherDirection {
            const DIRECTION: Direction = Direction::Broadcast;
            fn message_id() -> &'static str {
                "proto.same"
            }
        }

        #[derive(Clone, Debug, PartialEq, Replicate)]
        pub struct OtherLayout {
            pub a: u32,
            pub b: u32,
        }
        impl NetMessage for OtherLayout {
            const DIRECTION: Direction = Direction::ToServer;
            fn message_id() -> &'static str {
                "proto.same"
            }
        }
    }
    fn hash_of(f: impl FnOnce(&mut NetMessageRegistry)) -> u64 {
        let mut reg = NetMessageRegistry::default();
        f(&mut reg);
        let mut h = LayoutHasher::new();
        reg.hash_into(&mut h);
        h.finish()
    }
    let base = hash_of(|r| {
        r.register::<same_id::Base>();
    });
    let base_again = hash_of(|r| {
        r.register::<same_id::Base>();
    });
    let channel = hash_of(|r| {
        r.register::<same_id::OtherChannel>();
    });
    let direction = hash_of(|r| {
        r.register::<same_id::OtherDirection>();
    });
    let layout = hash_of(|r| {
        r.register::<same_id::OtherLayout>();
    });
    assert_eq!(base, base_again, "the hash is a function of the registry");
    assert_ne!(base, channel, "a channel change moves the hash");
    assert_ne!(base, direction, "a direction change moves the hash");
    assert_ne!(base, layout, "a field layout change moves the hash");
    assert_ne!(channel, direction);
    assert_ne!(channel, layout);
    assert_ne!(direction, layout);
}
