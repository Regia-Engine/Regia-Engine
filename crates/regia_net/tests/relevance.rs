// SPDX-License-Identifier: MPL-2.0
//! Spec 72 § 13: relevance, priority and budgets over the in-memory `LoopbackHub` at fixed steps
//! (and the interest pipeline driven directly for the grid parity walk).

use std::collections::{BTreeMap, BTreeSet};

use glam::Vec3;
use regia_ecs::prelude::*;
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::CodecContext;
use regia_net::relevance::{InterestEntity, PeerInterest};
use regia_net::replication::{NetDespawning, NetEvents, ServerReplication, register_replicated};
use regia_net::runtime::{Datagram, NetEntities};
use regia_net::testing::{LinkConditions, LoopbackHub, SplitMix64, every_test_on_state_channel};
use regia_net::*;
use regia_scene::transform::Transform;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp {
    hp: u32,
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Stats {
    a: u32,
    b: u32,
    c: u32,
    d: u32,
    e: u32,
    f: u32,
}

/// Raw bytes (for the 16 KiB state limit).
#[derive(Component, Clone, Debug, PartialEq, Default)]
struct Blob(Vec<u8>);

impl ReplicateCodec for Blob {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_varint(self.0.len() as u64);
        for b in &self.0 {
            w.write_bits(u64::from(*b), 8);
        }
    }
    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let n = r.read_len(1 << 20)?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(r.read_bits(8)? as u8);
        }
        Ok(Self(v))
    }
}

fn profile(radius: f32, hysteresis: f32, linger: u32) -> NetProfile {
    NetProfile {
        relevance: RelevanceShape::Spatial { radius, hysteresis },
        linger_ticks: linger,
        ..NetProfile::default()
    }
}

fn setup(world: &mut World) {
    register_replicated::<Hp>(world, ReplicateOptions::default());
    register_replicated::<Stats>(world, ReplicateOptions::default());
    register_replicated::<Blob>(world, ReplicateOptions::default());
    if world.resource::<NetMode>().is_server() {
        install_net_interest(world);
        let mut ps = world.resource_mut::<NetProfiles>();
        let world_p = NetProfile {
            relevance: RelevanceShape::World,
            linger_ticks: 0,
            ..NetProfile::default()
        };
        let inserts = [
            ("near", profile(10.0, 0.2, 5)),
            ("near0", profile(10.0, 0.0, 0)),
            ("everywhere", world_p.clone()),
            ("p1", world_p.clone()),
            (
                "p3",
                NetProfile {
                    priority: 3.0,
                    ..world_p.clone()
                },
            ),
            (
                "excl",
                NetProfile {
                    owner_only: true,
                    sent_to_all: true,
                    ..world_p.clone()
                },
            ),
            (
                "all",
                NetProfile {
                    sent_to_all: true,
                    ..profile(10.0, 0.0, 0)
                },
            ),
            (
                "held0",
                NetProfile {
                    follow_parent: true,
                    linger_ticks: 0,
                    ..NetProfile::default()
                },
            ),
            (
                "grouped",
                NetProfile {
                    group_required: true,
                    ..world_p.clone()
                },
            ),
            (
                "custom_yes",
                NetProfile {
                    custom_filter: Some("always".into()),
                    ..profile(10.0, 0.0, 0)
                },
            ),
            (
                "custom_no",
                NetProfile {
                    custom_filter: Some("never".into()),
                    ..world_p.clone()
                },
            ),
            (
                "custom_unknown",
                NetProfile {
                    custom_filter: Some("nope".into()),
                    ..world_p.clone()
                },
            ),
        ];
        for (n, p) in inserts {
            ps.insert(n, p).expect("valid test profile");
        }
        ps.set_default("everywhere");
        add_net_relevance_filter::<Always>(world, "always");
        add_net_relevance_filter::<Never>(world, "never");
    }
}

struct Always;
impl NetRelevanceFilter for Always {
    fn relevant(_: &World, _: Entity, _: PeerId) -> bool {
        true
    }
}
struct Never;
impl NetRelevanceFilter for Never {
    fn relevant(_: &World, _: Entity, _: PeerId) -> bool {
        false
    }
}

fn hub(seed: u64) -> LoopbackHub {
    LoopbackHub::new(NetMode::DedicatedServer, seed, setup)
}

fn rep() -> Replicated {
    Replicated {
        transform: TransformReplication::None,
        ..Replicated::default()
    }
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
        .spawn((rep(), Hp { hp: 1 }, at(x), NetProfileRef(profile.into())))
        .id()
}

fn copy(hub: &LoopbackHub, client: usize, e: Entity) -> Option<Entity> {
    let id = hub.server.world.get::<NetId>(e).copied()?;
    hub.clients[client].world.resource::<NetEntities>().entity(id)
}

fn has_copy(hub: &LoopbackHub, client: usize, e: Entity) -> bool {
    copy(hub, client, e).is_some()
}

fn net_id(hub: &LoopbackHub, e: Entity) -> NetId {
    *hub.server
        .world
        .get::<NetId>(e)
        .expect("replicated entity has a NetId")
}

fn why(hub: &LoopbackHub, peer: PeerId, e: Entity) -> RelevanceVerdict {
    let id = net_id(hub, e);
    hub.server
        .world
        .resource::<NetInterest>()
        .why(&hub.server.world, peer, id)
        .expect("a verdict")
}

/// These tests count in ticks: one send tick per tick.
fn one_tick_settings() -> NetSettings {
    let mut settings = NetSettings::default();
    settings.replication.send_every_ticks = 1;
    settings
}

fn peer_state(hub: &LoopbackHub, peer: PeerId) -> &PeerInterest {
    hub.server
        .world
        .resource::<NetInterest>()
        .peers
        .get(&peer)
        .expect("peer interest")
}

/// § 13 test 1 (reliable tick stream, ADR-1662): a field change made while the link loses half
/// the frames reaches the client in the next delivered frame, and that frame carries only the
/// field.
#[test]
fn delta_reaches_client_under_loss_and_carries_only_the_field() {
    let mut hub = hub(1);
    let a = hub.add_client();
    let e = hub
        .server
        .world
        .spawn((
            rep(),
            Stats {
                a: 1,
                b: 2,
                c: 3,
                d: 4,
                e: 5,
                f: 6,
            },
        ))
        .id();
    hub.run(5);
    hub.down = LinkConditions {
        loss: 0.5,
        ..LinkConditions::default()
    };
    // Tick frames or state frames: this holds on either wire.
    let carries_state = |d: &Datagram| matches!(d.bytes.first(), Some(1 | 7));
    let empty = hub.client_logs[a]
        .iter()
        .rev()
        .find(|(_, d)| carries_state(d))
        .map(|(_, d)| d.bytes.len())
        .expect("a frame");
    let from = hub.client_logs[a].len();
    if let Some(mut s) = hub.server.world.get_mut::<Stats>(e) {
        s.c = 300;
    }
    hub.run(40);
    let c = copy(&hub, a, e).expect("copy");
    assert_eq!(hub.clients[a].world.get::<Stats>(c).map(|s| s.c), Some(300));
    let biggest = hub.client_logs[a][from..]
        .iter()
        .filter(|(_, d)| carries_state(d))
        .map(|(_, d)| d.bytes.len())
        .max()
        .expect("frames");
    // A state record also names its id, length and version offsets.
    let framing = if every_test_on_state_channel() { 3 } else { 0 };
    assert!(
        biggest <= empty + 10 + framing,
        "only the changed field travels: {biggest} vs empty {empty}"
    );
}

/// § 13 test 4: a spawn sent over a lossy link reaches the client once.
#[test]
fn lossy_spawn_spawns_once() {
    let mut hub = hub(2);
    let a = hub.add_client();
    hub.down = LinkConditions {
        loss: 0.6,
        ..LinkConditions::default()
    };
    let e = spawn_with(&mut hub, "everywhere", 0.0);
    let mut spawned = 0;
    for _ in 0..60 {
        hub.step();
        spawned += hub.clients[a].world.resource::<NetEvents>().spawned.len();
    }
    assert!(has_copy(&hub, a, e));
    assert_eq!(spawned, 1);
}

/// § 13 test 5: one case per stage of § 5.1, end to end and through `why`.
#[test]
fn pipeline_stages() {
    let mut hub = hub(3);
    let a = hub.add_client();
    let b = hub.add_client();
    let (pa, pb) = (hub.clients[a].peer, hub.clients[b].peer);
    viewpoint(&mut hub, pa, 0.0);
    viewpoint(&mut hub, pb, 0.0);
    // Owner always relevant, even when denied; the deny still applies to others.
    let owned = spawn_with(&mut hub, "near0", 1000.0);
    hub.server.world.entity_mut(owned).insert((
        NetOwner(pa),
        NetVisibility {
            allow: Vec::new(),
            deny: vec![pa, pb],
        },
    ));
    // Deny beats every grant.
    let denied_all = spawn_with(&mut hub, "all", 0.0);
    hub.server.world.entity_mut(denied_all).insert(NetVisibility {
        allow: Vec::new(),
        deny: vec![pb],
    });
    // owner_only beats sent_to_all.
    let excl = spawn_with(&mut hub, "excl", 0.0);
    hub.server.world.entity_mut(excl).insert(NetOwner(pa));
    // A custom filter cannot add an entity excluded by distance.
    let far_custom = spawn_with(&mut hub, "custom_yes", 50.0);
    let near_custom = spawn_with(&mut hub, "custom_yes", 5.0);
    let refused = spawn_with(&mut hub, "custom_no", 0.0);
    let unknown = spawn_with(&mut hub, "custom_unknown", 0.0);
    // Allow list.
    let only_b = spawn_with(&mut hub, "everywhere", 0.0);
    hub.server.world.entity_mut(only_b).insert(NetVisibility {
        allow: vec![pb],
        deny: Vec::new(),
    });
    // group_required, group deny.
    let player_a = hub
        .server
        .world
        .spawn((
            NetPlayer {
                id: NetPlayerId { peer: pa, local: 0 },
            },
            NetGroupVisibility {
                allow: vec!["red".into()],
                deny: vec!["spoiler".into()],
            },
        ))
        .id();
    let grouped_red = spawn_with(&mut hub, "grouped", 0.0);
    hub.server
        .world
        .entity_mut(grouped_red)
        .insert(NetGroups(vec!["red".into()]));
    let grouped_none = spawn_with(&mut hub, "grouped", 0.0);
    let spoiler = spawn_with(&mut hub, "everywhere", 0.0);
    hub.server
        .world
        .entity_mut(spoiler)
        .insert(NetGroups(vec!["spoiler".into()]));
    hub.run(4);

    let check = |hub: &LoopbackHub, client: usize, e: Entity, relevant: bool, stage: RelevanceStage| {
        let peer = hub.clients[client].peer;
        let v = why(hub, peer, e);
        assert_eq!(
            (v.relevant, v.stage),
            (relevant, stage),
            "{peer:?} {e:?}: {}",
            v.detail
        );
        assert_eq!(has_copy(hub, client, e), relevant, "{peer:?} {e:?} copy");
    };
    check(&hub, a, owned, true, RelevanceStage::Owner);
    check(&hub, b, owned, false, RelevanceStage::Visibility);
    check(&hub, a, denied_all, true, RelevanceStage::SentToAll);
    check(&hub, b, denied_all, false, RelevanceStage::Visibility);
    check(&hub, a, excl, true, RelevanceStage::Owner);
    check(&hub, b, excl, false, RelevanceStage::OwnerOnly);
    check(&hub, a, far_custom, false, RelevanceStage::Spatial);
    check(&hub, a, near_custom, true, RelevanceStage::Spatial);
    check(&hub, a, refused, false, RelevanceStage::Custom);
    check(&hub, a, unknown, true, RelevanceStage::Spatial);
    check(&hub, a, only_b, false, RelevanceStage::Visibility);
    check(&hub, b, only_b, true, RelevanceStage::Spatial);
    check(&hub, a, grouped_red, true, RelevanceStage::Spatial);
    check(&hub, a, grouped_none, false, RelevanceStage::Group);
    check(&hub, b, grouped_red, false, RelevanceStage::Group);
    check(&hub, a, spoiler, false, RelevanceStage::Group);
    check(&hub, b, spoiler, true, RelevanceStage::Spatial);
    let interest = hub.server.world.resource::<NetInterest>();
    assert_eq!(interest.counters.filter_unknown, 1, "net.filter_unknown once");
    // A second registration of a filter name is refused.
    assert!(!add_net_relevance_filter::<Always>(
        &mut hub.server.world,
        "always"
    ));
    let _ = player_a;
}

/// § 13 test 5: a link-followed weapon appears and disappears with its holder.
#[test]
fn link_rule_follows_the_holder() {
    let mut hub = hub(4);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    viewpoint(&mut hub, pa, 0.0);
    let holder = spawn_with(&mut hub, "near0", 5.0);
    let weapon = hub
        .server
        .world
        .spawn((
            rep(),
            Hp { hp: 9 },
            at(0.0),
            NetProfileRef("held0".into()),
            ChildOf(holder),
        ))
        .id();
    hub.run(4);
    assert!(has_copy(&hub, a, holder) && has_copy(&hub, a, weapon));
    assert_eq!(why(&hub, pa, weapon).stage, RelevanceStage::Link);
    hub.server.world.entity_mut(holder).insert(at(500.0));
    hub.run(4);
    assert!(
        !has_copy(&hub, a, holder) && !has_copy(&hub, a, weapon),
        "both leave"
    );
    hub.server.world.entity_mut(holder).insert(at(3.0));
    hub.run(4);
    assert!(
        has_copy(&hub, a, holder) && has_copy(&hub, a, weapon),
        "both return"
    );
}

/// § 13 test 6: hysteresis band and linger.
#[test]
fn hysteresis_and_linger() {
    let mut hub = hub(5);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    viewpoint(&mut hub, pa, 0.0);
    // radius 10, hysteresis 0.2 (leaves beyond 12), linger 5.
    let e = spawn_with(&mut hub, "near", 9.0);
    hub.run(4);
    let first = copy(&hub, a, e).expect("relevant inside the radius");
    let mut despawns = 0;
    let count = |hub: &LoopbackHub| {
        hub.clients[a]
            .world
            .resource::<NetEvents>()
            .despawning
            .iter()
            .filter(|d: &&NetDespawning| d.entity == first)
            .count()
    };
    // Back and forth within the band: no spawn or despawn.
    for i in 0..40 {
        let x = if i % 2 == 0 { 11.5 } else { 9.0 };
        hub.server.world.entity_mut(e).insert(at(x));
        hub.step();
        despawns += count(&hub);
        assert_eq!(copy(&hub, a, e), Some(first), "step {i}");
    }
    assert_eq!(despawns, 0);
    // Out for 3 ticks (less than the linger), then back: nothing.
    hub.server.world.entity_mut(e).insert(at(30.0));
    hub.run(3);
    hub.server.world.entity_mut(e).insert(at(5.0));
    for _ in 0..6 {
        hub.step();
        despawns += count(&hub);
    }
    assert_eq!(copy(&hub, a, e), Some(first));
    assert_eq!(despawns, 0);
    // Out for longer than the linger: exactly one despawn.
    hub.server.world.entity_mut(e).insert(at(30.0));
    let mut lingered = 0;
    for _ in 0..20 {
        hub.step();
        despawns += count(&hub);
        if peer_state(&hub, pa).is_lingering(net_id(&hub, e)) {
            lingered += 1;
        }
    }
    assert_eq!(despawns, 1);
    assert_eq!(lingered, 5, "lingers exactly linger_ticks");
    assert!(!has_copy(&hub, a, e));
}

/// § 13 test 7: a temporary grant holds for exactly N ticks.
#[test]
fn temporary_grant_for_n_ticks() {
    let mut hub = hub(6);
    let a = hub.add_client();
    let b = hub.add_client();
    let (pa, pb) = (hub.clients[a].peer, hub.clients[b].peer);
    viewpoint(&mut hub, pa, 0.0);
    viewpoint(&mut hub, pb, 0.0);
    let enemy = spawn_with(&mut hub, "near0", 500.0);
    hub.run(3);
    assert!(!has_copy(&hub, a, enemy));
    let now = hub.server_tick();
    let n = 12u32;
    assert!(net_grant_relevance(
        &mut hub.server.world,
        enemy,
        GrantTarget::Peer(pa),
        NetTick(now.0 + n)
    ));
    let mut granted_ticks = 0;
    let mut b_ever = false;
    for _ in 0..30 {
        hub.step();
        let id = net_id(&hub, enemy);
        if peer_state(&hub, pa).holds(id) {
            granted_ticks += 1;
        }
        b_ever |= peer_state(&hub, pb).holds(id);
    }
    assert_eq!(granted_ticks, n, "relevant for exactly the granted ticks");
    assert!(!b_ever, "only the target");
    assert!(!has_copy(&hub, a, enemy), "normal rules resume");
    // A group grant reaches every peer whose players carry the group.
    hub.server.world.spawn((
        NetPlayer {
            id: NetPlayerId { peer: pb, local: 0 },
        },
        NetGroups(vec!["blue".into()]),
    ));
    let now = hub.server_tick();
    net_grant_relevance(
        &mut hub.server.world,
        enemy,
        GrantTarget::Group("blue".into()),
        NetTick(now.0 + 5),
    );
    hub.run(3);
    assert!(has_copy(&hub, b, enemy) && !has_copy(&hub, a, enemy));
    assert_eq!(why(&hub, pb, enemy).stage, RelevanceStage::Grant);
}

/// § 13 test 8: the grid-based relevant set equals a brute-force all-pairs test on every tick of a
/// 600-tick scripted walk (2,000 entities, 8 viewpoints).
#[test]
fn interest_grid_parity() {
    let mut world = World::new();
    world.insert_resource(NetMode::DedicatedServer);
    install_net_interest(&mut world);
    {
        let mut ps = world.resource_mut::<NetProfiles>();
        for (i, r) in [20.0f32, 45.0, 90.0, 150.0].iter().enumerate() {
            ps.insert(&format!("r{i}"), profile(*r, 0.0, 0)).expect("profile");
        }
        ps.insert(
            "w",
            NetProfile {
                relevance: RelevanceShape::World,
                linger_ticks: 0,
                ..NetProfile::default()
            },
        )
        .expect("profile");
    }
    let mut rng = SplitMix64::new(77);
    let mut rand = move |lo: f32, hi: f32| lo + (hi - lo) * rng.next_f64() as f32;
    let mut ents: Vec<(Entity, Vec3, Vec3)> = Vec::new();
    for i in 0..2000u32 {
        let p = Vec3::new(rand(-600.0, 600.0), 0.0, rand(-600.0, 600.0));
        let v = Vec3::new(rand(-2.0, 2.0), 0.0, rand(-2.0, 2.0));
        let name = if i % 50 == 0 {
            "w".to_string()
        } else {
            format!("r{}", i % 4)
        };
        let e = world
            .spawn((
                Transform {
                    translation: p,
                    ..Transform::default()
                },
                NetId(i + 1),
                NetProfileRef(name),
            ))
            .id();
        ents.push((e, p, v));
    }
    let peers = [PeerId(1), PeerId(2), PeerId(3), PeerId(4)];
    let mut vps: Vec<(Entity, Vec3, Vec3)> = Vec::new();
    for k in 0..8u32 {
        let peer = peers[(k % 4) as usize];
        let p = Vec3::new(rand(-400.0, 400.0), 0.0, rand(-400.0, 400.0));
        let v = Vec3::new(rand(-3.0, 3.0), 0.0, rand(-3.0, 3.0));
        let e = world
            .spawn((
                NetViewpoint {
                    player: NetPlayerId { peer, local: 0 },
                },
                Transform {
                    translation: p,
                    ..Transform::default()
                },
            ))
            .id();
        vps.push((e, p, v));
    }
    let mut interest = world.remove_resource::<NetInterest>().expect("interest");
    let mut checked = 0usize;
    for tick in 0..600u32 {
        for (e, p, v) in ents.iter_mut().chain(vps.iter_mut()) {
            *p += *v;
            if p.x.abs() > 600.0 {
                v.x = -v.x;
            }
            if p.z.abs() > 600.0 {
                v.z = -v.z;
            }
            if let Some(mut t) = world.get_mut::<Transform>(*e) {
                t.translation = *p;
            }
        }
        let list: Vec<InterestEntity> = ents
            .iter()
            .enumerate()
            .map(|(i, (e, _, _))| InterestEntity {
                id: NetId(i as u32 + 1),
                slot: i as u32,
                entity: *e,
                owner: PeerId::SERVER,
                depth: 0,
                dirty: true,
                structure: tick == 0,
                predicted: false,
                parent: None,
            })
            .collect();
        interest.update(&mut world, &list, &peers, NetTick(tick));
        for p in peers {
            let grid: BTreeSet<NetId> = interest.peers[&p].held_ids().collect();
            let brute = interest.brute_force(&world, p);
            assert_eq!(grid, brute, "tick {tick} peer {p:?}");
            checked += grid.len();
        }
    }
    assert!(
        checked > 10_000,
        "the walk exercises relevance ({checked} relevant pairs)"
    );
}

/// A tick where ownership and the entity set stand still hands relevance only the entities that
/// changed. What a connection holds must still be what the world says, through moves, a walking
/// viewpoint, spawns, despawns, ownership and a profile override.
#[test]
fn held_sets_follow_the_world_on_quiet_and_busy_ticks() {
    let mut hub = hub(21);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    let vp = viewpoint(&mut hub, pa, 0.0);
    let mut ents: Vec<Entity> = (0..120)
        .map(|i| spawn_with(&mut hub, "near0", i as f32 - 60.0))
        .collect();
    let (owned, wide) = (ents[3], ents[100]);
    let mut always: Vec<Entity> = Vec::new();
    hub.run(3);
    let mut rng = SplitMix64::new(21);
    let mut gains = 0;
    let mut last = BTreeSet::new();
    for t in 0..240u32 {
        for (i, e) in ents.iter().enumerate() {
            if (i as u32 + t).is_multiple_of(3) {
                let step = (rng.next_f64() as f32 - 0.5) * 2.0;
                if let Some(mut tr) = hub.server.world.get_mut::<Transform>(*e) {
                    tr.translation.x += step;
                }
            }
        }
        if let Some(mut tr) = hub.server.world.get_mut::<Transform>(vp) {
            tr.translation.x = (t as f32 % 80.0 - 40.0).abs() - 20.0;
        }
        let world = &mut hub.server.world;
        match t {
            60 => {
                if let Some(gone) = ents.pop() {
                    world.despawn(gone);
                }
            }
            120 => {
                world.entity_mut(owned).insert(NetOwner(pa));
                always.push(owned);
            }
            150 => {
                world.entity_mut(wide).insert(NetProfileRef("everywhere".into()));
                always.push(wide);
            }
            190 => {
                world.entity_mut(wide).insert(NetProfileRef("near0".into()));
                always.retain(|e| *e != wide);
            }
            _ => {}
        }
        if t == 90 {
            ents.push(spawn_with(&mut hub, "near0", 0.0));
        }
        hub.step();
        let world = &hub.server.world;
        let at = |e: Entity| world.get::<Transform>(e).map(|t| t.translation);
        let center = at(vp).expect("viewpoint");
        let expected: BTreeSet<NetId> = ents
            .iter()
            .filter(|e| always.contains(e) || at(**e).is_some_and(|p| p.distance(center) <= 10.0))
            .filter_map(|e| world.get::<NetId>(*e).copied())
            .collect();
        let held: BTreeSet<NetId> = peer_state(&hub, pa).held_ids().collect();
        assert_eq!(held, expected, "tick {t}");
        gains += held.difference(&last).count();
        last = held;
    }
    assert!(gains > 40, "the walk changes what is held ({gains} gains)");
}

/// Entities that only move update apart from the rest, on the pool when there are many: crossing
/// grid cells in both directions, they stay where the query finds them.
#[test]
fn moving_entities_stay_in_the_grid_across_cells() {
    let mut hub = hub(22);
    hub.server
        .world
        .resource_mut::<NetSettings>()
        .replication
        .grid_cell_m = 4.0;
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    viewpoint(&mut hub, pa, 0.0);
    let ents: Vec<Entity> = (0..700)
        .map(|i| spawn_with(&mut hub, "near0", -175.0 + i as f32 * 0.5))
        .collect();
    hub.run(3);
    let mut gains = 0;
    let mut last = BTreeSet::new();
    for _ in 0..120 {
        for (i, e) in ents.iter().enumerate() {
            if let Some(mut tr) = hub.server.world.get_mut::<Transform>(*e) {
                tr.translation.x += if i.is_multiple_of(2) { 1.5 } else { -1.5 };
            }
        }
        hub.step();
        let world = &hub.server.world;
        let expected: BTreeSet<NetId> = ents
            .iter()
            .filter(|e| {
                world
                    .get::<Transform>(**e)
                    .is_some_and(|t| t.translation.length() <= 10.0)
            })
            .filter_map(|e| world.get::<NetId>(*e).copied())
            .collect();
        let held: BTreeSet<NetId> = peer_state(&hub, pa).held_ids().collect();
        assert_eq!(held, expected);
        gains += held.difference(&last).count();
        last = held;
    }
    assert!(gains > 100, "entities walk through the viewpoint ({gains} gains)");
}

/// § 13 test 9: with a budget that fits about 10 of 100 changing entities, every entity is sent
/// within the starvation bound, priority 3 is sent 3x as often as priority 1 (±10 %), and an
/// unchanged entity is never sent after its spawn.
#[test]
fn priority_fairness() {
    // Both id orders: ties sort by NetId, so either group may win them.
    fairness(true);
    fairness(false);
}

fn fairness(p3_first: bool) {
    let mut hub = hub(9);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    let mut p3 = Vec::new();
    let mut p1 = Vec::new();
    for i in 0..100u32 {
        let name = if (i < 50) == p3_first { "p3" } else { "p1" };
        let e = hub
            .server
            .world
            .spawn((rep(), Hp { hp: 1000 }, NetProfileRef(name.into())))
            .id();
        if name == "p3" { p3.push(e) } else { p1.push(e) }
    }
    let still = hub
        .server
        .world
        .spawn((rep(), Hp { hp: 5 }, NetProfileRef("p1".into())))
        .id();
    hub.run(8);
    assert_eq!(
        hub.clients[a].world.resource::<NetEntities>().len(),
        101,
        "all spawned (spawn budget 24)"
    );
    // Measure one delta's size, then set the budget to about 10 of them.
    let mut settings = one_tick_settings();
    settings.replication.client_rate_bytes = 60 * 10 * 6;
    hub.server.world.insert_resource(settings);
    let ids3: Vec<NetId> = p3.iter().map(|e| net_id(&hub, *e)).collect();
    let ids1: Vec<NetId> = p1.iter().map(|e| net_id(&hub, *e)).collect();
    let still_id = net_id(&hub, still);
    let mut sends: BTreeMap<NetId, Vec<u32>> = BTreeMap::new();
    let ticks = 6000u32;
    let mut per_tick = Vec::new();
    for t in 0..ticks {
        for e in p3.iter().chain(p1.iter()) {
            if let Some(mut hp) = hub.server.world.get_mut::<Hp>(*e) {
                hp.hp = 1000 + t;
            }
        }
        hub.step();
        let now = hub.server_tick().0.saturating_sub(1);
        let st = peer_state(&hub, pa);
        let mut n = 0;
        for (id, s) in st.send_states() {
            if s.last_sent == Some(NetTick(now)) {
                sends.entry(id).or_default().push(now);
                n += 1;
            }
        }
        per_tick.push(n);
    }
    let avg = per_tick.iter().sum::<u32>() as f64 / f64::from(ticks);
    assert!(
        (4.0..=25.0).contains(&avg),
        "the budget limits the sends: {avg} per tick"
    );
    assert!(
        !sends.contains_key(&still_id),
        "an unchanged entity is never sent"
    );
    let count = |ids: &[NetId]| {
        ids.iter()
            .map(|id| sends.get(id).map_or(0, Vec::len))
            .sum::<usize>() as f64
    };
    let ratio = count(&ids3) / count(&ids1);
    assert!((2.7..=3.3).contains(&ratio), "priority 3 vs 1: {ratio}");
    // Starvation bound with n competing entities of effective priority at most 3.
    let bound = regia_net::send::starvation_bound(100, 3.0, 1.0);
    for id in ids1.iter().chain(ids3.iter()) {
        let list = sends.get(id).expect("every changing entity is sent");
        for w in list.windows(2) {
            assert!(w[1] - w[0] <= bound, "{id:?} waited {} > {bound}", w[1] - w[0]);
        }
    }
}

/// § 13 test 10: measured bytes stay within the rate; the spawn budget spreads 200 spawns over
/// ⌈200/24⌉ send ticks; a 17 KiB entity is not sent and is counted once.
#[test]
fn budgets() {
    let mut hub = hub(10);
    for _ in 0..200 {
        spawn_with(&mut hub, "everywhere", 0.0);
    }
    let a = hub.add_client();
    let mut spawn_ticks = Vec::new();
    for _ in 0..20 {
        hub.step();
        let n = hub.clients[a].world.resource::<NetEvents>().spawned.len();
        if n > 0 {
            assert!(n <= 24, "spawn budget per send tick: {n}");
            spawn_ticks.push(n);
        }
    }
    assert_eq!(spawn_ticks.len(), 200usize.div_ceil(24));
    assert_eq!(spawn_ticks.iter().sum::<usize>(), 200);

    // Rate: 100 entities changing every tick against a 6 KiB/s rate for 10 s.
    let mut settings = one_tick_settings();
    settings.replication.client_rate_bytes = 6144;
    hub.server.world.insert_resource(settings);
    let mut q = hub.server.world.query_filtered::<Entity, With<Hp>>();
    let ents: Vec<Entity> = q.iter(&hub.server.world).collect();
    let pa = hub.clients[a].peer;
    let mut used = 0u64;
    for t in 0..600u32 {
        for e in &ents {
            if let Some(mut hp) = hub.server.world.get_mut::<Hp>(*e) {
                hp.hp = 2 + t % 2;
            }
        }
        hub.step();
        used += u64::from(peer_state(&hub, pa).send.counters.budget_used);
    }
    let rate = used as f64 / 10.0;
    assert!(
        rate <= 6144.0 + 1200.0 / 10.0,
        "within the rate (±1 packet): {rate}"
    );
    assert!(rate >= 6144.0 * 0.8, "and the budget is used: {rate}");

    // 17 KiB entity.
    let big = hub
        .server
        .world
        .spawn((
            rep(),
            Blob(vec![7; 17 * 1024]),
            NetProfileRef("everywhere".into()),
        ))
        .id();
    hub.run(5);
    assert!(!has_copy(&hub, a, big));
    let rep_state = hub.server.world.resource::<ServerReplication>();
    assert!(rep_state.counters.state_too_large >= 1);
}

/// A parent whose spawn record is over `MAX_ENTITY_STATE_BYTES` is offered again every tick and
/// never enters the set of spawns picked this tick. That must not keep its child pending forever:
/// the child is still a spawn, and the client can wait for a parent that may arrive later.
#[test]
fn an_oversized_parent_spawn_does_not_block_its_child() {
    let mut hub = hub(20);
    let a = hub.add_client();
    // The same size the budget test uses: the encoded spawn is past the 16 KiB cap.
    let parent = hub
        .server
        .world
        .spawn((
            rep(),
            Blob(vec![7; 17 * 1024]),
            NetProfileRef("everywhere".into()),
        ))
        .id();
    let child = hub
        .server
        .world
        .spawn((
            rep(),
            Hp { hp: 2 },
            NetProfileRef("everywhere".into()),
            ChildOf(parent),
        ))
        .id();
    hub.run(8);
    assert!(
        !has_copy(&hub, a, parent),
        "the oversized parent stays off the wire"
    );
    assert!(
        has_copy(&hub, a, child),
        "the child is spawned even though its parent record cannot be"
    );
    assert!(
        hub.server
            .world
            .resource::<ServerReplication>()
            .counters
            .state_too_large
            >= 1
    );
}

/// § 13 test 11 (serial assembly, ADR-1662): two identical sessions with 16 connections produce
/// identical frames byte for byte.
#[test]
fn assembly_is_deterministic() {
    let run = || {
        let mut hub = hub(11);
        let clients: Vec<usize> = (0..16).map(|_| hub.add_client()).collect();
        for (i, c) in clients.iter().enumerate() {
            let peer = hub.clients[*c].peer;
            viewpoint(&mut hub, peer, i as f32 * 3.0);
        }
        let ents: Vec<Entity> = (0..60).map(|i| spawn_with(&mut hub, "near", i as f32)).collect();
        for t in 0..60u32 {
            for (i, e) in ents.iter().enumerate() {
                hub.server
                    .world
                    .entity_mut(*e)
                    .insert(at((i as f32 + t as f32 * 0.5) % 60.0));
            }
            hub.step();
        }
        hub.client_logs.clone()
    };
    let x = run();
    let y = run();
    assert_eq!(x.len(), 16);
    for (a, b) in x.iter().zip(y.iter()) {
        assert_eq!(a.len(), b.len());
        for ((sa, da), (sb, db)) in a.iter().zip(b.iter()) {
            assert_eq!((sa, &da.bytes), (sb, &db.bytes));
        }
    }
}

/// § 13 test 12: three resyncs of one entity within 5 s are a loop; more than 10 per second are
/// refused.
#[test]
fn resync_loop_and_rate() {
    let mut hub = hub(12);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    let ents: Vec<Entity> = (0..20).map(|_| spawn_with(&mut hub, "everywhere", 0.0)).collect();
    hub.run(4);
    let id = net_id(&hub, ents[0]);
    for _ in 0..3 {
        hub.server
            .world
            .resource_mut::<ServerReplication>()
            .request_resync(pa, [id]);
        hub.run(20);
    }
    assert_eq!(
        hub.server
            .world
            .resource::<ServerReplication>()
            .counters
            .resync_loops,
        1
    );
    assert!(has_copy(&hub, a, ents[0]), "resyncs keep the copy");
    let ids: Vec<NetId> = ents.iter().map(|e| net_id(&hub, *e)).collect();
    hub.run(60);
    hub.server
        .world
        .resource_mut::<ServerReplication>()
        .request_resync(pa, ids);
    hub.step();
    let st = peer_state(&hub, pa);
    assert!(st.send.counters.resyncs_limited >= 10, "{:?}", st.send.counters);
}

/// A change deferred by the budget is never lost to sleep: entities that go quiet right after one
/// change (and would fall asleep) still deliver it under a tiny budget.
#[test]
fn deferred_changes_survive_sleep() {
    let mut hub = hub(13);
    let a = hub.add_client();
    let ents: Vec<Entity> = (0..50)
        .map(|i| {
            hub.server
                .world
                .spawn((
                    Replicated {
                        transform: TransformReplication::None,
                        sleep: NetSleep::AfterQuiet { quiet_ticks: 3 },
                        ..Replicated::default()
                    },
                    Hp { hp: i },
                    NetProfileRef("p1".into()),
                ))
                .id()
        })
        .collect();
    hub.run(8);
    let mut settings = one_tick_settings();
    settings.replication.client_rate_bytes = 600; // 10 bytes per tick: about two deltas
    hub.server.world.insert_resource(settings);
    hub.run(20);
    for (i, e) in ents.iter().enumerate() {
        if let Some(mut hp) = hub.server.world.get_mut::<Hp>(*e) {
            hp.hp = 5000 + i as u32;
        }
    }
    hub.run(400);
    for (i, e) in ents.iter().enumerate() {
        let c = copy(&hub, a, *e).expect("copy");
        assert_eq!(
            hub.clients[a].world.get::<Hp>(c).map(|h| h.hp),
            Some(5000 + i as u32),
            "entity {i}"
        );
    }
    let rep = hub.server.world.resource::<ServerReplication>();
    assert!(
        ents.iter().all(|e| rep.is_asleep(net_id(&hub, *e)) == Some(true)),
        "they fall asleep once every change went out"
    );
}

/// `min_interval_ticks` holds an entity back between sends; `max_interval_ticks` forces a send of a
/// low-priority entity against a saturated budget; the newest value always arrives.
#[test]
fn send_intervals() {
    let mut hub = hub(14);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    {
        let mut ps = hub.server.world.resource_mut::<NetProfiles>();
        let w = NetProfile {
            relevance: RelevanceShape::World,
            ..NetProfile::default()
        };
        ps.insert(
            "slow",
            NetProfile {
                min_interval_ticks: 6,
                ..w.clone()
            },
        )
        .expect("profile");
        ps.insert(
            "forced",
            NetProfile {
                priority: 0.01,
                max_interval_ticks: Some(10),
                ..w.clone()
            },
        )
        .expect("profile");
        ps.insert("busy", NetProfile { priority: 50.0, ..w })
            .expect("profile");
    }
    let slow = spawn_with(&mut hub, "slow", 0.0);
    let forced = spawn_with(&mut hub, "forced", 0.0);
    let busy: Vec<Entity> = (0..40).map(|_| spawn_with(&mut hub, "busy", 0.0)).collect();
    hub.run(6);
    let mut settings = one_tick_settings();
    settings.replication.client_rate_bytes = 600;
    hub.server.world.insert_resource(settings);
    let (sid, fid) = (net_id(&hub, slow), net_id(&hub, forced));
    let mut slow_sends = Vec::new();
    let mut forced_sends = Vec::new();
    for t in 0..120u32 {
        for e in busy.iter().chain([slow, forced].iter()) {
            if let Some(mut hp) = hub.server.world.get_mut::<Hp>(*e) {
                hp.hp = 10 + t;
            }
        }
        hub.step();
        let now = NetTick(hub.server_tick().0.saturating_sub(1));
        let st = peer_state(&hub, pa);
        if st.send_state(sid).and_then(|s| s.last_sent) == Some(now) {
            slow_sends.push(now.0);
        }
        if st.send_state(fid).and_then(|s| s.last_sent) == Some(now) {
            forced_sends.push(now.0);
        }
    }
    assert!(slow_sends.windows(2).all(|w| w[1] - w[0] >= 6), "{slow_sends:?}");
    assert!(!forced_sends.is_empty());
    assert!(
        forced_sends.windows(2).all(|w| w[1] - w[0] <= 10),
        "{forced_sends:?}"
    );
    // Stop changing: the newest values arrive.
    let mut settings = one_tick_settings();
    settings.replication.client_rate_bytes = 49_152;
    hub.server.world.insert_resource(settings);
    hub.run(20);
    for e in [slow, forced] {
        let c = copy(&hub, a, e).expect("copy");
        assert_eq!(hub.clients[a].world.get::<Hp>(c).map(|h| h.hp), Some(129));
    }
}

/// Review regressions: resyncs over the rate are kept for later ticks, not dropped.
#[test]
fn over_rate_resyncs_are_honored_later() {
    let mut hub = hub(15);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    let ents: Vec<Entity> = (0..25).map(|_| spawn_with(&mut hub, "everywhere", 0.0)).collect();
    hub.run(6);
    let ids: Vec<NetId> = ents.iter().map(|e| net_id(&hub, *e)).collect();
    hub.server
        .world
        .resource_mut::<ServerReplication>()
        .request_resync(pa, ids);
    hub.run(4 * 60);
    let c = peer_state(&hub, pa).send.counters;
    assert_eq!(c.resyncs, 25, "every request honored within 3 seconds: {c:?}");
}

/// Review regressions: a temporary grant also reveals a `follow_parent` entity.
#[test]
fn grant_reveals_a_link_follower() {
    let mut hub = hub(16);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    viewpoint(&mut hub, pa, 0.0);
    let holder = spawn_with(&mut hub, "near0", 500.0);
    let item = hub
        .server
        .world
        .spawn((
            rep(),
            Hp { hp: 3 },
            at(0.0),
            NetProfileRef("held0".into()),
            ChildOf(holder),
        ))
        .id();
    hub.run(4);
    assert!(!has_copy(&hub, a, item));
    let until = NetTick(hub.server_tick().0 + 20);
    net_grant_relevance(&mut hub.server.world, item, GrantTarget::Peer(pa), until);
    hub.run(4);
    assert_eq!(why(&hub, pa, item).stage, RelevanceStage::Grant);
    assert!(has_copy(&hub, a, item));
}

/// Review regressions: a teleport made while the entity's update is held back by the budget
/// still reaches the client with the teleport bit.
#[test]
fn teleport_survives_a_deferral() {
    let mut hub = hub(17);
    let a = hub.add_client();
    {
        let mut ps = hub.server.world.resource_mut::<NetProfiles>();
        let w = NetProfile {
            relevance: RelevanceShape::World,
            ..NetProfile::default()
        };
        ps.insert(
            "busy",
            NetProfile {
                priority: 50.0,
                ..w.clone()
            },
        )
        .expect("profile");
        ps.insert("low", NetProfile { priority: 0.01, ..w })
            .expect("profile");
    }
    let moving = |hub: &mut LoopbackHub, p: &str| {
        hub.server
            .world
            .spawn((
                Replicated::default(),
                Hp { hp: 1 },
                at(0.0),
                NetProfileRef(p.into()),
            ))
            .id()
    };
    let busy: Vec<Entity> = (0..30).map(|_| moving(&mut hub, "busy")).collect();
    let low = moving(&mut hub, "low");
    hub.run(6);
    let mut settings = one_tick_settings();
    settings.replication.client_rate_bytes = 600;
    hub.server.world.insert_resource(settings);
    hub.server.world.entity_mut(low).insert(at(1.0));
    net_teleport(&mut hub.server.world, low);
    let mut teleported = false;
    for t in 0..400u32 {
        if t < 300 {
            for e in &busy {
                hub.server.world.entity_mut(*e).insert(at(t as f32 * 0.01));
            }
        }
        hub.step();
        if let Some(c) = copy(&hub, a, low) {
            teleported |= hub.clients[a]
                .world
                .get::<regia_net::replication::NetTeleported>(c)
                .is_some();
        }
    }
    assert!(teleported, "the teleport bit arrives with the deferred update");
}

/// Review regressions: a child's spawn never reaches a client before its parent's, even when the
/// child has the higher priority and the spawn budget is full.
#[test]
fn parents_spawn_before_children_under_the_spawn_budget() {
    let mut hub = hub(18);
    {
        let mut ps = hub.server.world.resource_mut::<NetProfiles>();
        let w = NetProfile {
            relevance: RelevanceShape::World,
            ..NetProfile::default()
        };
        ps.insert(
            "hi",
            NetProfile {
                priority: 9.0,
                ..w.clone()
            },
        )
        .expect("profile");
        ps.insert("lo", NetProfile { priority: 0.1, ..w })
            .expect("profile");
    }
    let mut pairs = Vec::new();
    for _ in 0..40 {
        let parent = spawn_with(&mut hub, "lo", 0.0);
        let child = hub
            .server
            .world
            .spawn((
                rep(),
                Hp { hp: 2 },
                at(0.0),
                NetProfileRef("hi".into()),
                ChildOf(parent),
            ))
            .id();
        pairs.push((parent, child));
    }
    let a = hub.add_client();
    for _ in 0..12 {
        hub.step();
        for (p, c) in &pairs {
            if has_copy(&hub, a, *c) {
                assert!(has_copy(&hub, a, *p), "child before parent");
            }
        }
    }
    assert!(
        pairs
            .iter()
            .all(|(p, c)| has_copy(&hub, a, *p) && has_copy(&hub, a, *c))
    );
}

/// Review regressions (spec 74 client): when a parent and its replicated child leave relevance
/// together on an interpolating client, the child stays attached while the parent's despawn waits
/// for the interpolation tick, and both go when it is shown.
#[test]
fn deferred_despawn_keeps_children_attached() {
    let mut hub = hub(19);
    hub.down.delay_ticks = 2;
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    regia_net::interp::install_interpolation(&mut hub.clients[a].world, 60.0, 1.0);
    let parent = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Hp { hp: 1 },
            at(0.0),
            NetProfileRef("everywhere".into()),
        ))
        .id();
    let child = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Hp { hp: 2 },
            at(1.0),
            NetProfileRef("everywhere".into()),
            ChildOf(parent),
        ))
        .id();
    let mut step = 0u32;
    let frame = |hub: &mut LoopbackHub, step: &mut u32| {
        *step += 1;
        let now = f64::from(*step) * 1000.0 / 60.0;
        hub.clients[a].world.resource_mut::<NetInterpolation>().now_ms = now;
        hub.step();
        net_interpolate(&mut hub.clients[a].world, now);
    };
    for t in 0..40u32 {
        hub.server.world.entity_mut(parent).insert(at(t as f32 * 0.1));
        frame(&mut hub, &mut step);
    }
    let (cp, cc) = (
        copy(&hub, a, parent).expect("parent copy"),
        copy(&hub, a, child).expect("child copy"),
    );
    for e in [parent, child] {
        hub.server.world.entity_mut(e).insert(NetVisibility {
            allow: Vec::new(),
            deny: vec![pa],
        });
    }
    let mut pending_frames = 0;
    for _ in 0..40 {
        frame(&mut hub, &mut step);
        let cw = &hub.clients[a].world;
        if cw.get_entity(cp).is_ok() && cw.get::<NetPendingDespawn>(cp).is_some() {
            pending_frames += 1;
            if cw.get_entity(cc).is_ok() {
                assert_eq!(
                    cw.get::<ChildOf>(cc).map(|c| c.0),
                    Some(cp),
                    "child stays attached while pending"
                );
            }
        }
    }
    assert!(
        pending_frames > 0,
        "the parent's despawn waited for the interpolation tick"
    );
    let cw = &hub.clients[a].world;
    assert!(
        cw.get_entity(cp).is_err() && cw.get_entity(cc).is_err(),
        "both gone once shown"
    );
}

/// A second update with the same groups and the same unknown filter copies no names.
#[test]
fn an_unchanged_update_copies_no_group_or_filter_names() {
    let mut hub = hub(4);
    let a = hub.add_client();
    let peer = hub.clients[a].peer;
    hub.server.world.spawn((
        NetPlayer {
            id: NetPlayerId { peer, local: 0 },
        },
        NetGroups(vec!["red".into()]),
        NetViewpoint {
            player: NetPlayerId { peer, local: 0 },
        },
    ));
    hub.server
        .world
        .resource_mut::<NetProfiles>()
        .insert(
            "filtered",
            NetProfile {
                relevance: RelevanceShape::World,
                custom_filter: Some("missing".into()),
                ..NetProfile::default()
            },
        )
        .expect("profile");
    hub.server.world.spawn((
        Replicated::default(),
        Hp { hp: 1 },
        Transform::from_translation(Vec3::ZERO),
        NetProfileRef("filtered".into()),
    ));
    hub.run(6);
    let (groups, filters) = {
        let interest = hub.server.world.resource::<NetInterest>();
        (interest.group_name_copies(), interest.filter_name_copies())
    };
    hub.run(4);
    let interest = hub.server.world.resource::<NetInterest>();
    let group_delta = interest.group_name_copies() - groups;
    let filter_delta = interest.filter_name_copies() - filters;
    assert_eq!(
        group_delta, 0,
        "group names copied on an unchanged update: {group_delta}"
    );
    assert_eq!(
        filter_delta, 0,
        "unknown filter names copied on an unchanged update: {filter_delta}"
    );
}
