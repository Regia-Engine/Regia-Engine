// SPDX-License-Identifier: MPL-2.0
//! Spec 71 / 72 conformance audit (lane spec-29-71, ADR-5648): tests for behavior the specs require
//! that no earlier test held, over the in-memory `LoopbackHub` at fixed steps.

use glam::Vec3;
use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_net::codec::{CodecContext, decode_from_slice, encode_to_vec, layout_hash};
use regia_net::relevance::GrantTarget;
use regia_net::replication::{
    ClientReplication, NetEvents, NetOutOfScope, NetRelevance, ServerReplication, register_replicated,
};
use regia_net::runtime::{NetEntities, protocol_hash};
use regia_net::testing::LoopbackHub;
use regia_net::*;
use regia_scene::transform::Transform;

// ---- spec 71 ---------------------------------------------------------------------------------

/// A component with a field that never travels (spec 71 § 3 `#[replicate(skip)]`).
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Tagged {
    hp: u32,
    #[replicate(skip)]
    local: u32,
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Health {
    hp: u32,
}

#[derive(Component, Clone, Debug, PartialEq, Replicate)]
struct Target(Entity);

fn setup71(world: &mut World) {
    register_replicated::<Tagged>(world, ReplicateOptions::default());
    register_replicated::<Health>(world, ReplicateOptions::default());
    register_replicated::<Target>(world, ReplicateOptions::default());
}

fn setup71_state(world: &mut World) {
    setup71(world);
    world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Unreliable;
}

fn rep() -> Replicated {
    Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    }
}

fn copy(hub: &LoopbackHub, client: usize, e: Entity) -> Option<Entity> {
    let id = hub.server.world.get::<NetId>(e).copied()?;
    hub.clients[client].world.resource::<NetEntities>().entity(id)
}

fn get<C: Component + Clone>(hub: &LoopbackHub, client: usize, e: Entity) -> Option<C> {
    let c = copy(hub, client, e)?;
    hub.clients[client].world.get::<C>(c).cloned()
}

/// Spec 71 § 3: a `skip` field never travels, starts at `Default` on the first spawn, and the
/// client keeps its own value through later updates and through a full resend, on the reliable
/// tick stream and on the acknowledged state channel.
#[test]
fn a_skipped_field_keeps_the_clients_own_value() {
    for (wire, setup) in [
        ("reliable stream", setup71 as fn(&mut World)),
        ("state channel", setup71_state),
    ] {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 71, setup);
        hub.negotiate = true;
        let a = hub.add_client();
        let e = hub.server.world.spawn((rep(), Tagged { hp: 1, local: 99 })).id();
        hub.run(6);
        assert_eq!(
            get::<Tagged>(&hub, a, e),
            Some(Tagged { hp: 1, local: 0 }),
            "{wire}: the skipped field does not travel; Default on the first spawn"
        );
        let c = copy(&hub, a, e).expect("copy");
        hub.clients[a].world.get_mut::<Tagged>(c).expect("tagged").local = 7;
        hub.server.world.get_mut::<Tagged>(e).expect("tagged").hp = 2;
        hub.run(6);
        assert_eq!(
            get::<Tagged>(&hub, a, e),
            Some(Tagged { hp: 2, local: 7 }),
            "{wire}: a delta keeps the client's own value"
        );
        // A full state (spawn record for a copy the client has) keeps it too.
        assert!(!replication::net_resend(&mut hub.server.world, e).is_empty());
        hub.server.world.get_mut::<Tagged>(e).expect("tagged").hp = 3;
        hub.run(6);
        assert_eq!(
            get::<Tagged>(&hub, a, e),
            Some(Tagged { hp: 3, local: 7 }),
            "{wire}: a full resend keeps the client's own value"
        );
    }
}

/// Spec 71 § 4.1 / § 14 test 8: spawns made in one tick go out parents first, even when the
/// child's `NetId` is the lower one (the client's apply order shows the wire order).
#[test]
fn spawns_of_one_tick_arrive_parents_first_whatever_the_net_id_order() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 72, setup71);
    let a = hub.add_client();
    // The parent exists first but becomes replicated after its child, in the same tick: the
    // child is assigned first, so it has the lower `NetId`.
    let parent = hub.server.world.spawn(Health { hp: 1 }).id();
    let child = hub
        .server
        .world
        .spawn((rep(), Health { hp: 2 }, ChildOf(parent)))
        .id();
    let grandchild = hub
        .server
        .world
        .spawn((rep(), Health { hp: 3 }, ChildOf(child)))
        .id();
    hub.server.world.entity_mut(parent).insert(rep());
    let mut order: Vec<NetId> = Vec::new();
    for _ in 0..6 {
        hub.step();
        let w = &hub.clients[a].world;
        for s in &w.resource::<NetEvents>().spawned {
            order.push(s.net_id);
        }
    }
    let (pid, cid, gid) = (
        *hub.server.world.get::<NetId>(parent).expect("parent id"),
        *hub.server.world.get::<NetId>(child).expect("child id"),
        *hub.server.world.get::<NetId>(grandchild).expect("grandchild id"),
    );
    assert!(cid < pid, "the test needs the child to have the lower id");
    assert_eq!(order, vec![pid, cid, gid], "depth first, then NetId");
    let counters = hub.server.world.resource::<ServerReplication>().counters;
    assert_eq!(
        counters.parent_not_replicated, 0,
        "the parent replicates in the same tick"
    );
}

#[derive(Resource, Default)]
struct Hidden(Vec<Entity>);

fn not_hidden(world: &World, e: Entity, _peer: PeerId) -> bool {
    world.get_resource::<Hidden>().is_none_or(|h| !h.0.contains(&e))
}

/// Spec 71 § 5 and § 11: pending references are capped by `replication.max_pending_refs`; the
/// oldest is dropped and counted.
#[test]
fn pending_references_are_capped_oldest_first() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 73, setup71);
    let a = hub.add_client();
    hub.clients[a]
        .world
        .resource_mut::<NetSettings>()
        .replication
        .max_pending_refs = 2;
    hub.server.world.insert_resource(NetRelevance {
        filter: Some(not_hidden),
    });
    let targets: Vec<Entity> = (0..3)
        .map(|i| hub.server.world.spawn((rep(), Health { hp: i })).id())
        .collect();
    hub.server.world.insert_resource(Hidden(targets.clone()));
    let mut referrers = Vec::new();
    for t in &targets {
        referrers.push(hub.server.world.spawn((rep(), Target(*t))).id());
        // One referrer per tick, so the oldest is well defined.
        hub.run(2);
    }
    hub.run(2);
    let first_copy = copy(&hub, a, referrers[0]).expect("first referrer copy");
    let client = hub.clients[a].world.resource::<ClientReplication>();
    assert_eq!(client.pending_refs.len(), 2, "capped at max_pending_refs");
    assert_eq!(client.counters.pending_refs_dropped, 1);
    assert!(
        client.pending_refs.iter().all(|p| p.entity != first_copy),
        "the oldest pending reference is the one dropped"
    );
    let waiting: Vec<NetId> = client.pending_refs.iter().map(|p| p.waiting_for).collect();
    let ids: Vec<NetId> = targets[1..]
        .iter()
        .map(|t| *hub.server.world.get::<NetId>(*t).expect("id"))
        .collect();
    assert_eq!(waiting, ids);
}

/// Spec 71 § 3: an `AssetRef<T>` travels as its 128-bit `AssetId` and round-trips exactly.
#[test]
fn asset_refs_travel_as_their_asset_id() {
    #[derive(Clone)]
    struct Mesh;
    let cx = CodecContext::detached();
    let id = regia_asset::AssetId(Guid::from_u128(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210));
    let r = regia_asset::AssetRef::<Mesh>::new(id);
    let bytes = encode_to_vec(&r, &cx);
    assert_eq!(bytes.len(), 16, "128 bits");
    let back: regia_asset::AssetRef<Mesh> = decode_from_slice(&bytes, &cx).expect("decodes");
    assert_eq!(back.id, id);
    assert!(r.wire_eq(&back, &cx));
    assert_ne!(
        layout_hash::<regia_asset::AssetRef<Mesh>>(),
        layout_hash::<u64>(),
        "its own codec shape in the protocol hash"
    );
}

mod lo {
    use regia_ecs::prelude::*;
    use regia_net::Replicate;
    #[derive(Clone, Debug, Component, Replicate)]
    pub struct Aim {
        #[replicate(quantize(min = 0.0, max = 1.0, bits = 8))]
        pub x: f32,
    }
}
mod hi {
    use regia_ecs::prelude::*;
    use regia_net::Replicate;
    #[derive(Clone, Debug, Component, Replicate)]
    pub struct Aim {
        #[replicate(quantize(min = 0.0, max = 1.0, bits = 9))]
        pub x: f32,
    }
}

/// Spec 71 § 11 / § 14 test 13: two builds that register a component with a different
/// quantization have different protocol hashes (not only different layout hashes).
#[test]
fn the_protocol_hash_covers_each_fields_quantization() {
    let mut w_lo = World::new();
    register_replicated::<lo::Aim>(&mut w_lo, ReplicateOptions::default());
    let mut w_hi = World::new();
    register_replicated::<hi::Aim>(&mut w_hi, ReplicateOptions::default());
    let mut w_lo2 = World::new();
    register_replicated::<lo::Aim>(&mut w_lo2, ReplicateOptions::default());
    assert_ne!(protocol_hash(&w_lo), protocol_hash(&w_hi));
    assert_eq!(protocol_hash(&w_lo), protocol_hash(&w_lo2));
}

/// Spec 71 § 9: `TransformReplication::None` sends no transform: the copy has none.
#[test]
fn transform_none_sends_no_transform() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 74, setup71);
    let a = hub.add_client();
    let t = Transform {
        translation: Vec3::new(4.0, 5.0, 6.0),
        ..Transform::default()
    };
    let none = hub.server.world.spawn((rep(), Health { hp: 1 }, t)).id();
    let moving = hub
        .server
        .world
        .spawn((Replicated::default(), Health { hp: 2 }, t))
        .id();
    hub.run(4);
    assert!(copy(&hub, a, none).is_some());
    assert!(get::<Transform>(&hub, a, none).is_none(), "no transform travels");
    assert_eq!(
        get::<Transform>(&hub, a, moving).map(|t| t.translation),
        Some(t.translation)
    );
}

// ---- spec 72 ---------------------------------------------------------------------------------

fn world_profile() -> NetProfile {
    NetProfile {
        relevance: RelevanceShape::World,
        linger_ticks: 0,
        ..NetProfile::default()
    }
}

fn setup72(world: &mut World) {
    register_replicated::<Health>(world, ReplicateOptions::default());
    if world.resource::<NetMode>().is_server() {
        install_net_interest(world);
        let mut ps = world.resource_mut::<NetProfiles>();
        ps.insert("everywhere", world_profile()).expect("profile");
        ps.insert(
            "near0",
            NetProfile {
                relevance: RelevanceShape::Spatial {
                    radius: 10.0,
                    hysteresis: 0.0,
                },
                linger_ticks: 0,
                ..NetProfile::default()
            },
        )
        .expect("profile");
        ps.insert(
            "held0",
            NetProfile {
                follow_parent: true,
                linger_ticks: 0,
                ..NetProfile::default()
            },
        )
        .expect("profile");
        ps.set_default("everywhere");
    }
}

fn hub72(seed: u64) -> LoopbackHub {
    LoopbackHub::new(NetMode::DedicatedServer, seed, setup72)
}

fn at(x: f32) -> Transform {
    Transform {
        translation: Vec3::new(x, 0.0, 0.0),
        ..Transform::default()
    }
}

fn viewpoint(hub: &mut LoopbackHub, peer: PeerId, x: f32) -> Entity {
    hub.server
        .world
        .spawn((
            NetViewpoint {
                player: NetPlayerId { peer, local: 0 },
            },
            at(x),
        ))
        .id()
}

fn spawn_with(hub: &mut LoopbackHub, profile: &str, x: f32) -> Entity {
    hub.server
        .world
        .spawn((rep(), Health { hp: 1 }, at(x), NetProfileRef(profile.into())))
        .id()
}

fn why(hub: &LoopbackHub, peer: PeerId, e: Entity) -> RelevanceVerdict {
    let id = *hub.server.world.get::<NetId>(e).expect("net id");
    hub.server
        .world
        .resource::<NetInterest>()
        .why(&hub.server.world, peer, id)
        .expect("a verdict")
}

fn conn_counters(hub: &LoopbackHub, peer: PeerId) -> regia_net::send::ConnSendCounters {
    hub.server
        .world
        .resource::<NetInterest>()
        .peers
        .get(&peer)
        .expect("peer interest")
        .send
        .counters
}

/// Spec 72 § 12: `why_not_relevant`'s stage names are exactly the nine of the spec.
#[test]
fn relevance_stage_names_match_the_spec() {
    let names: Vec<&str> = [
        RelevanceStage::Owner,
        RelevanceStage::SentToAll,
        RelevanceStage::OwnerOnly,
        RelevanceStage::Visibility,
        RelevanceStage::Group,
        RelevanceStage::Link,
        RelevanceStage::Spatial,
        RelevanceStage::Custom,
        RelevanceStage::Grant,
    ]
    .into_iter()
    .map(RelevanceStage::as_str)
    .collect();
    assert_eq!(
        names,
        [
            "owner",
            "sent_to_all",
            "owner_only",
            "visibility",
            "group",
            "link",
            "spatial",
            "custom",
            "grant"
        ]
    );
}

/// Spec 72 § 8: the built-in profiles carry the numbers the spec lists.
#[test]
fn built_in_profiles_carry_the_spec_numbers() {
    let ps: std::collections::BTreeMap<&str, NetProfile> =
        regia_net::profile::builtin_profiles().into_iter().collect();
    let radius = |p: &NetProfile| match p.relevance {
        RelevanceShape::Spatial { radius, .. } => Some(radius),
        RelevanceShape::World => None,
    };
    assert_eq!(ps["default"], NetProfile::default());
    assert_eq!(radius(&ps["default"]), Some(150.0));
    assert_eq!((ps["player"].priority, radius(&ps["player"])), (3.0, Some(400.0)));
    assert_eq!(
        (
            ps["projectile"].priority,
            radius(&ps["projectile"]),
            ps["projectile"].linger_ticks
        ),
        (2.0, Some(200.0), 2)
    );
    assert_eq!((ps["prop"].priority, radius(&ps["prop"])), (0.5, Some(100.0)));
    assert!(ps["held"].follow_parent);
    assert!(ps["game_state"].sent_to_all);
    assert_eq!(ps["game_state"].min_interval_ticks, 6);
    for (name, p) in &ps {
        p.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    // § 8 validation: hysteresis below 0 is refused.
    let bad = NetProfile {
        relevance: RelevanceShape::Spatial {
            radius: 10.0,
            hysteresis: -0.1,
        },
        ..NetProfile::default()
    };
    assert!(bad.validate().is_err());
}

/// Spec 72 § 5.1: a deny (peer or group) beats a temporary grant.
#[test]
fn denies_beat_temporary_grants() {
    let mut hub = hub72(80);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    viewpoint(&mut hub, pa, 0.0);
    hub.server.world.spawn((
        NetPlayer {
            id: NetPlayerId { peer: pa, local: 0 },
        },
        NetGroupVisibility {
            allow: Vec::new(),
            deny: vec!["spoiler".into()],
        },
    ));
    let peer_denied = spawn_with(&mut hub, "near0", 500.0);
    hub.server.world.entity_mut(peer_denied).insert(NetVisibility {
        allow: Vec::new(),
        deny: vec![pa],
    });
    let group_denied = spawn_with(&mut hub, "near0", 500.0);
    hub.server
        .world
        .entity_mut(group_denied)
        .insert(NetGroups(vec!["spoiler".into()]));
    let granted = spawn_with(&mut hub, "near0", 500.0);
    hub.run(3);
    let until = NetTick(hub.server_tick().0 + 30);
    for e in [peer_denied, group_denied, granted] {
        assert!(net_grant_relevance(
            &mut hub.server.world,
            e,
            GrantTarget::Peer(pa),
            until
        ));
    }
    hub.run(4);
    let v = why(&hub, pa, peer_denied);
    assert_eq!((v.relevant, v.stage), (false, RelevanceStage::Visibility));
    let v = why(&hub, pa, group_denied);
    assert_eq!((v.relevant, v.stage), (false, RelevanceStage::Group));
    let v = why(&hub, pa, granted);
    assert_eq!((v.relevant, v.stage), (true, RelevanceStage::Grant));
    assert!(copy(&hub, a, granted).is_some());
    assert!(copy(&hub, a, peer_denied).is_none());
    assert!(copy(&hub, a, group_denied).is_none());
}

/// Spec 72 § 5.1 stage 4: `follow_parent` follows a `NetOwnerLink` target as well as a `ChildOf`
/// parent, and a deny on the follower still applies.
#[test]
fn the_link_rule_follows_an_owner_link_target() {
    let mut hub = hub72(81);
    let a = hub.add_client();
    let b = hub.add_client();
    let (pa, pb) = (hub.clients[a].peer, hub.clients[b].peer);
    viewpoint(&mut hub, pa, 0.0);
    viewpoint(&mut hub, pb, 0.0);
    let holder = spawn_with(&mut hub, "near0", 5.0);
    let follower = spawn_with(&mut hub, "held0", 9999.0);
    hub.server.world.entity_mut(follower).insert((
        NetOwnerLink(holder),
        NetVisibility {
            allow: Vec::new(),
            deny: vec![pb],
        },
    ));
    hub.run(4);
    let v = why(&hub, pa, follower);
    assert_eq!(
        (v.relevant, v.stage),
        (true, RelevanceStage::Link),
        "{}",
        v.detail
    );
    assert!(copy(&hub, a, follower).is_some());
    let v = why(&hub, pb, follower);
    assert_eq!((v.relevant, v.stage), (false, RelevanceStage::Visibility));
    assert!(copy(&hub, b, follower).is_none());
    // The holder walks away: the follower goes with it.
    *hub.server.world.get_mut::<Transform>(holder).expect("t") = at(1000.0);
    hub.run(4);
    assert!(copy(&hub, a, holder).is_none());
    let v = why(&hub, pa, follower);
    assert_eq!(
        (v.relevant, v.stage),
        (false, RelevanceStage::Link),
        "{}",
        v.detail
    );
    assert!(copy(&hub, a, follower).is_none());
}

fn one_tick_settings() -> NetSettings {
    let mut settings = NetSettings::default();
    settings.replication.send_every_ticks = 1;
    settings
}

/// Spec 72 § 7: `server_rate_bytes` scales every connection's budget down by the same factor.
#[test]
fn the_server_cap_scales_every_budget_by_one_factor() {
    let budgets = |cap: u32| {
        let mut hub = hub72(82);
        let mut settings = one_tick_settings();
        settings.replication.server_rate_bytes = cap;
        hub.server.world.insert_resource(settings);
        let a = hub.add_client();
        let b = hub.add_client();
        for _ in 0..4 {
            spawn_with(&mut hub, "everywhere", 0.0);
        }
        hub.run(10);
        let (pa, pb) = (hub.clients[a].peer, hub.clients[b].peer);
        (conn_counters(&hub, pa).budget, conn_counters(&hub, pb).budget)
    };
    let (free_a, free_b) = budgets(0);
    let (cap_a, cap_b) = budgets(49_152);
    assert!(free_a > 0 && free_b > 0);
    assert_eq!(cap_a, cap_b, "one factor for every connection");
    let ratio = f64::from(cap_a) / f64::from(free_a);
    assert!(
        (0.45..=0.55).contains(&ratio),
        "two connections at 48 KiB/s under a 48 KiB/s cap get half each: {cap_a} vs {free_a}"
    );
}

/// Spec 72 § 10: entities whose accumulator exceeds 10× their priority are counted as starved
/// under a tiny budget, and none are with room to spare.
#[test]
fn starved_entities_are_counted() {
    let starved_after = |rate: u32| {
        let mut hub = hub72(83);
        let mut settings = one_tick_settings();
        settings.replication.client_rate_bytes = rate;
        hub.server.world.insert_resource(settings);
        let ents: Vec<Entity> = (0..200)
            .map(|_| spawn_with(&mut hub, "everywhere", 0.0))
            .collect();
        let a = hub.add_client();
        let pa = hub.clients[a].peer;
        let mut most = 0;
        for t in 0..200u32 {
            for e in &ents {
                if let Some(mut h) = hub.server.world.get_mut::<Health>(*e) {
                    h.hp = 2 + t % 2;
                }
            }
            hub.step();
            most = most.max(conn_counters(&hub, pa).starved);
        }
        most
    };
    // 1 KiB/s is about one record per tick for 200 changing entities: the share each sent record
    // gives up (gain / records sent) is far above 10x the priority, so accumulators pass it.
    assert!(starved_after(1024) > 0, "a tiny budget starves entities");
    assert_eq!(starved_after(10 * 1024 * 1024), 0, "a large budget starves none");
}

#[derive(Component, Clone, Debug, PartialEq, Default)]
struct Blob(Vec<u8>);

impl ReplicateCodec for Blob {
    fn encode(&self, w: &mut regia_net::bits::BitWriter, _cx: &CodecContext) {
        w.write_varint(self.0.len() as u64);
        for b in &self.0 {
            w.write_bits(u64::from(*b), 8);
        }
    }
    fn decode(
        r: &mut regia_net::bits::BitReader,
        _cx: &CodecContext,
    ) -> Result<Self, regia_net::bits::DecodeError> {
        let n = r.read_len(1 << 20)?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(r.read_bits(8)? as u8);
        }
        Ok(Self(v))
    }
}

fn setup72_blob(world: &mut World) {
    setup72(world);
    register_replicated::<Blob>(world, ReplicateOptions::default());
}

/// Spec 72 § 7 / § 13 test 10: an entity over 16 KiB is not sent and `net.state_too_large` is
/// counted once for it, however many ticks it stays.
#[test]
fn an_oversized_entity_is_refused_and_counted_once() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 84, setup72_blob);
    let a = hub.add_client();
    let big = hub
        .server
        .world
        .spawn((
            rep(),
            Blob(vec![7; 17 * 1024]),
            NetProfileRef("everywhere".into()),
        ))
        .id();
    hub.run(30);
    assert!(copy(&hub, a, big).is_none());
    let counters = hub.server.world.resource::<ServerReplication>().counters;
    assert_eq!(counters.state_too_large, 1, "logged and counted once per entity");
}

/// Spec 72 § 7 (ADR-1950 (5)): a despawn on the reliable frame is remembered, so a state frame
/// for that id still on its way (delayed past the despawn) does not bring the copy back.
#[test]
fn a_state_frame_delayed_past_the_despawn_does_not_revive_the_copy() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    const TAG_STATE: u8 = 7;
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 85, setup71_state);
    hub.negotiate = true;
    let a = hub.add_client();
    let e = hub.server.world.spawn((rep(), Health { hp: 1 })).id();
    hub.run(8);
    let id = *hub.server.world.get::<NetId>(e).expect("net id");
    assert!(copy(&hub, a, e).is_some(), "spawned");
    let held = Arc::new(AtomicBool::new(false));
    let flag = held.clone();
    hub.down_filter = Some(Box::new(move |d, _| {
        let is_state = d.bytes.first() == Some(&TAG_STATE) && d.bytes.len() > 8;
        if is_state && !flag.swap(true, Ordering::Relaxed) {
            regia_net::testing::FrameFate::Delay(8)
        } else {
            regia_net::testing::FrameFate::Deliver
        }
    }));
    hub.server.world.get_mut::<Health>(e).expect("hp").hp = 2;
    hub.run(2);
    assert!(
        held.load(Ordering::Relaxed),
        "the delta's state frame is held back"
    );
    assert!(hub.server.world.despawn(e));
    hub.run(16);
    let client = hub.clients[a].world.resource::<ClientReplication>();
    assert!(
        client.state.counters.tombstoned >= 1,
        "the late state frame met the tombstone"
    );
    assert!(
        hub.clients[a]
            .world
            .resource::<NetEntities>()
            .entity(id)
            .is_none(),
        "the copy stays despawned"
    );
}

fn setup72_linger(world: &mut World) {
    setup72(world);
    if world.resource::<NetMode>().is_server() {
        world
            .resource_mut::<NetProfiles>()
            .insert(
                "near_l5",
                NetProfile {
                    relevance: RelevanceShape::Spatial {
                        radius: 10.0,
                        hysteresis: 0.0,
                    },
                    linger_ticks: 5,
                    ..NetProfile::default()
                },
            )
            .expect("profile");
    }
}

/// Spec 72 § 5.2 with spec 71 § 4.5: a level-bound entity that leaves a spatial radius lingers,
/// then gets an out-of-scope notice instead of a despawn; the copy keeps its last state and
/// receives the full current state when it is relevant again.
#[test]
fn a_level_entity_out_of_range_lingers_then_goes_out_of_scope_not_away() {
    use regia_scene::components::EntityGuid;
    use regia_scene::level::LevelMember;
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 86, setup72_linger);
    let guid = EntityGuid(Guid::from_u128(0x5648));
    let door = hub
        .server
        .world
        .spawn((
            rep(),
            LevelMember(7),
            guid,
            Health { hp: 1 },
            at(5.0),
            NetProfileRef("near_l5".into()),
        ))
        .id();
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    viewpoint(&mut hub, pa, 0.0);
    let local = hub.clients[a]
        .world
        .spawn((rep(), LevelMember(7), guid, Health { hp: 0 }))
        .id();
    hub.run(4);
    assert_eq!(copy(&hub, a, door), Some(local), "bound to the level copy");
    assert_eq!(hub.clients[a].world.get::<Health>(local), Some(&Health { hp: 1 }));
    *hub.server.world.get_mut::<Transform>(door).expect("t") = at(100.0);
    hub.run(3);
    assert!(
        hub.clients[a].world.get::<NetOutOfScope>(local).is_none(),
        "still lingering"
    );
    hub.run(8);
    let w = &hub.clients[a].world;
    assert!(
        w.get_entity(local).is_ok(),
        "level content is never despawned by relevance"
    );
    assert!(
        w.get::<NetOutOfScope>(local).is_some(),
        "out of scope after the linger"
    );
    hub.server.world.get_mut::<Health>(door).expect("hp").hp = 9;
    hub.run(3);
    assert_eq!(
        hub.clients[a].world.get::<Health>(local),
        Some(&Health { hp: 1 }),
        "no updates while out of scope"
    );
    *hub.server.world.get_mut::<Transform>(door).expect("t") = at(5.0);
    hub.run(4);
    let w = &hub.clients[a].world;
    assert!(w.get::<NetOutOfScope>(local).is_none());
    assert_eq!(
        w.get::<Health>(local),
        Some(&Health { hp: 9 }),
        "full current state"
    );
}

/// Spec 72 § 5.2: a player without a `NetViewpoint` sees spatial entities around its controlled
/// (owned, predicted) character; a player with neither sees only relevant-everywhere, owned and
/// granted entities.
#[test]
fn a_player_without_a_viewpoint_sees_around_its_character() {
    let mut hub = hub72(87);
    let a = hub.add_client();
    let b = hub.add_client();
    let (pa, pb) = (hub.clients[a].peer, hub.clients[b].peer);
    let pawn = hub
        .server
        .world
        .spawn((
            rep(),
            Health { hp: 1 },
            at(500.0),
            NetOwner(pa),
            replication::NetPredicted,
            NetProfileRef("near0".into()),
        ))
        .id();
    let near_pawn = spawn_with(&mut hub, "near0", 505.0);
    let at_origin = spawn_with(&mut hub, "near0", 0.0);
    let everywhere = spawn_with(&mut hub, "everywhere", 0.0);
    let granted = spawn_with(&mut hub, "near0", 2000.0);
    hub.run(3);
    let until = NetTick(hub.server_tick().0 + 60);
    assert!(net_grant_relevance(
        &mut hub.server.world,
        granted,
        GrantTarget::Peer(pb),
        until
    ));
    hub.run(4);
    assert!(copy(&hub, a, pawn).is_some(), "owned");
    assert!(copy(&hub, a, near_pawn).is_some(), "near the character");
    assert!(copy(&hub, a, at_origin).is_none(), "far from the character");
    assert!(copy(&hub, a, everywhere).is_some());
    // b has no viewpoint and no character.
    assert!(copy(&hub, b, pawn).is_none());
    assert!(copy(&hub, b, near_pawn).is_none());
    assert!(copy(&hub, b, at_origin).is_none());
    assert!(copy(&hub, b, everywhere).is_some(), "relevant everywhere");
    assert!(copy(&hub, b, granted).is_some(), "granted");
    let v = why(&hub, pb, at_origin);
    assert_eq!(
        (v.relevant, v.stage),
        (false, RelevanceStage::Spatial),
        "{}",
        v.detail
    );
}

/// Spec 72 § 5.1 stage 3: a split-screen peer's group visibility is the union over its players.
#[test]
fn a_split_screen_peers_group_visibility_is_the_union_of_its_players() {
    let mut hub = hub72(88);
    hub.server
        .world
        .resource_mut::<NetProfiles>()
        .insert(
            "grouped",
            NetProfile {
                group_required: true,
                ..world_profile()
            },
        )
        .expect("profile");
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    let p0 = hub
        .server
        .world
        .spawn((
            NetPlayer {
                id: NetPlayerId { peer: pa, local: 0 },
            },
            NetGroupVisibility {
                allow: vec!["red".into()],
                deny: Vec::new(),
            },
        ))
        .id();
    let p1 = hub
        .server
        .world
        .spawn((
            NetPlayer {
                id: NetPlayerId { peer: pa, local: 1 },
            },
            NetGroupVisibility {
                allow: vec!["blue".into()],
                deny: vec!["spoiler".into()],
            },
        ))
        .id();
    let red = spawn_with(&mut hub, "grouped", 0.0);
    hub.server
        .world
        .entity_mut(red)
        .insert(NetGroups(vec!["red".into()]));
    let blue = spawn_with(&mut hub, "grouped", 0.0);
    hub.server
        .world
        .entity_mut(blue)
        .insert(NetGroups(vec!["blue".into()]));
    let spoiler = spawn_with(&mut hub, "everywhere", 0.0);
    hub.server
        .world
        .entity_mut(spoiler)
        .insert(NetGroups(vec!["spoiler".into()]));
    hub.run(4);
    assert!(copy(&hub, a, red).is_some(), "allowed by player 0");
    assert!(copy(&hub, a, blue).is_some(), "allowed by player 1");
    assert!(copy(&hub, a, spoiler).is_none(), "denied by player 1");
    // Quiet ticks reuse last tick's lists; the answer must not change.
    hub.run(4);
    assert!(copy(&hub, a, red).is_some() && copy(&hub, a, blue).is_some());
    // Player 1 drops `blue`: the union no longer allows it.
    hub.server
        .world
        .get_mut::<NetGroupVisibility>(p1)
        .expect("vis")
        .allow
        .clear();
    hub.run(4);
    let v = why(&hub, pa, blue);
    assert_eq!(
        (v.relevant, v.stage),
        (false, RelevanceStage::Group),
        "{}",
        v.detail
    );
    assert!(copy(&hub, a, blue).is_none());
    assert!(copy(&hub, a, red).is_some());
    let _ = p0;
}

macro_rules! audience_components {
    ($($name:ident),*) => {$(
        #[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
        struct $name(u32);
    )*};
}
audience_components!(AllA, OwnerOnlyA, OthersA, SpawnOnlyA, SpawnThenOwnerA, ProxyA);

fn setup_audiences(world: &mut World) {
    let with = |audience| ReplicateOptions {
        audience,
        ..ReplicateOptions::default()
    };
    register_replicated::<AllA>(world, with(Audience::All));
    register_replicated::<OwnerOnlyA>(world, with(Audience::OwnerOnly));
    register_replicated::<OthersA>(world, with(Audience::Others));
    register_replicated::<SpawnOnlyA>(world, with(Audience::SpawnOnly));
    register_replicated::<SpawnThenOwnerA>(world, with(Audience::SpawnThenOwner));
    register_replicated::<ProxyA>(world, with(Audience::ProxyCopies));
}

/// Spec 71 § 6 / § 14 test 5: after the spawn record, a change of each audience's component
/// reaches exactly its audience (a predicting owner and two other clients).
#[test]
fn later_changes_reach_exactly_each_audience() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 89, setup_audiences);
    let owner = hub.add_client();
    let b = hub.add_client();
    let c = hub.add_client();
    let po = hub.clients[owner].peer;
    let e = hub
        .server
        .world
        .spawn((
            rep(),
            NetOwner(po),
            replication::NetPredicted,
            AllA(1),
            OwnerOnlyA(1),
            OthersA(1),
            SpawnOnlyA(1),
            SpawnThenOwnerA(1),
            ProxyA(1),
        ))
        .id();
    hub.run(4);
    {
        let mut ent = hub.server.world.entity_mut(e);
        ent.insert((
            AllA(2),
            OwnerOnlyA(2),
            OthersA(2),
            SpawnOnlyA(2),
            SpawnThenOwnerA(2),
            ProxyA(2),
        ));
    }
    hub.run(4);
    let row = |client: usize| {
        (
            get::<AllA>(&hub, client, e).map(|v| v.0),
            get::<OwnerOnlyA>(&hub, client, e).map(|v| v.0),
            get::<OthersA>(&hub, client, e).map(|v| v.0),
            get::<SpawnOnlyA>(&hub, client, e).map(|v| v.0),
            get::<SpawnThenOwnerA>(&hub, client, e).map(|v| v.0),
            get::<ProxyA>(&hub, client, e).map(|v| v.0),
        )
    };
    // (All, OwnerOnly, Others, SpawnOnly, SpawnThenOwner, ProxyCopies)
    assert_eq!(
        row(owner),
        (Some(2), Some(2), None, Some(1), Some(2), None),
        "the predicting owner"
    );
    for other in [b, c] {
        assert_eq!(
            row(other),
            (Some(2), None, Some(2), Some(1), Some(1), Some(2)),
            "a proxy copy"
        );
    }
}
