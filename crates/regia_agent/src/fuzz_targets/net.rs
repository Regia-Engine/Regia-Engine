// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for `regia_net`: the handshake datagrams, LAN discovery, port mapping, input bundles, the
//! relay server's datagram handler, join codes and `config/net.ron`. Every fixture is encoded by the
//! crate's own encoder, so a fixture always matches the current wire layout.

use std::fmt::Display;
use std::net::{Ipv4Addr, SocketAddr};

use regia_framework::net_vehicle::{
    DriverNetInput, NetVehicleState, NetWheelLook, VehicleNetInput, VehicleSeatRequest, VehicleShift,
};
use regia_net::handshake::{self, Accept, Challenge, ConnectRequest, Deny, PacketKind, PasswordChallenge};
use regia_net::wire::{WireReader, WireWriter};

use super::{FuzzInput, FuzzTarget};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

fn e_dbg<E: std::fmt::Debug>(err: E) -> String {
    format!("{err:?}")
}

/// Decodes a value and writes it back: a value that loads is a fixed point of the wire.
fn codec_round_trip<T: regia_net::codec::ReplicateCodec>(d: &[u8]) -> Result<Vec<u8>, String> {
    let cx = regia_net::codec::CodecContext::detached();
    let value = T::decode(&mut regia_net::bits::BitReader::new(d), &cx).map_err(e)?;
    Ok(regia_net::codec::encode_to_vec(&value, &cx))
}

const fn bytes(
    name: &'static str,
    round_trip: bool,
    fixture: fn() -> Vec<u8>,
    load: fn(&[u8]) -> Result<Vec<u8>, String>,
) -> FuzzTarget {
    FuzzTarget {
        name,
        format: None,
        format_version: regia_net::REGIA_NET_WIRE_VERSION,
        round_trip,
        input: FuzzInput::Bytes { fixture, load },
    }
}

/// The body of a handshake datagram of `kind` (after the magic and the kind byte).
fn body(datagram: &[u8], kind: PacketKind) -> Result<&[u8], String> {
    match handshake::split_datagram(datagram) {
        Some((k, body)) if k == kind => Ok(body),
        Some((k, _)) => Err(format!("expected a {kind:?} datagram, found {k:?}")),
        None => Err("not a handshake datagram".into()),
    }
}

/// The `regia_net` targets.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        bytes(
            "net.connect_request",
            true,
            || {
                ConnectRequest {
                    protocol_id: 0x0123_4567_89AB_CDEF,
                    client_nonce: 42,
                    crypto_version: 1,
                    crypto_required: true,
                }
                .to_datagram()
            },
            |d| {
                ConnectRequest::decode(body(d, PacketKind::ConnectRequest)?)
                    .map(|m| m.to_datagram())
                    .map_err(e)
            },
        ),
        bytes(
            "net.challenge",
            true,
            || {
                Challenge {
                    client_nonce: 7,
                    cookie: [3; 16],
                    password: Some(PasswordChallenge {
                        salt: [1; 16],
                        nonce: [2; 16],
                    }),
                    crypto_version: 1,
                }
                .to_datagram()
            },
            |d| {
                Challenge::decode(body(d, PacketKind::Challenge)?)
                    .map(|m| m.to_datagram())
                    .map_err(e)
            },
        ),
        bytes(
            "net.challenge_response",
            true,
            || {
                handshake::ChallengeResponse {
                    client_nonce: 7,
                    cookie: [3; 16],
                    token: handshake::JoinToken {
                        name: "player".into(),
                        role: regia_net::PeerRole::Player,
                        build: "0.2.5".into(),
                        password_proof: [9; 32],
                        join_data: vec![1, 2, 3],
                        resume: None,
                    },
                }
                .to_datagram()
                .unwrap_or_default()
            },
            |d| {
                handshake::ChallengeResponse::decode(body(d, PacketKind::ChallengeResponse)?)
                    .map_err(e)?
                    .to_datagram()
                    .map_err(e)
            },
        ),
        bytes(
            "net.accept",
            true,
            || {
                Accept {
                    client_nonce: 7,
                    session_token: 0xDEAD_BEEF,
                    peer_id: 3,
                    tick_rate_hz: 60,
                    server_tick: 1234,
                    resume_ticket: [0; 16],
                    resumption: None,
                }
                .to_datagram()
            },
            |d| {
                Accept::decode(body(d, PacketKind::Accept)?)
                    .map(|m| m.to_datagram())
                    .map_err(e)
            },
        ),
        bytes(
            "net.deny",
            true,
            || {
                Deny {
                    client_nonce: 7,
                    reason: regia_net::DenyReason::Full,
                    detail: "server full".into(),
                }
                .to_datagram()
            },
            |d| {
                Deny::decode(body(d, PacketKind::Deny)?)
                    .map(|m| m.to_datagram())
                    .map_err(e)
            },
        ),
        bytes(
            "net.server_announce",
            true,
            || {
                regia_net::discovery::ServerAnnounce {
                    protocol_id: 99,
                    game_port: 7777,
                    players: 3,
                    max_players: 8,
                    password: true,
                    name: "Fuzz host".into(),
                    card: vec![4, 5, 6],
                }
                .encode()
                .unwrap_or_default()
            },
            |d| match regia_net::discovery::ServerAnnounce::decode(d).map_err(e)? {
                Some(announce) => announce.encode().map_err(e),
                None => Err("not an announce datagram".into()),
            },
        ),
        bytes(
            "net.pcp_map_response",
            true,
            || {
                regia_net::port_map::PcpMapResponse {
                    result: 0,
                    lifetime_secs: 7200,
                    nonce: [7; 12],
                    internal_port: 7777,
                    external_port: 40000,
                    external_ip: Ipv4Addr::new(203, 0, 113, 9),
                }
                .encode()
                .to_vec()
            },
            |d| {
                regia_net::port_map::PcpMapResponse::decode(d)
                    .map(|m| m.encode().to_vec())
                    .map_err(e)
            },
        ),
        bytes(
            "net.input_bundle",
            true,
            || {
                let mut w = WireWriter::new();
                regia_net::tick::InputBundle::<Vec<u8>> {
                    first_tick: 100,
                    inputs: vec![vec![1, 2], vec![3], Vec::new()],
                }
                .encode(&mut w);
                w.finish()
            },
            |d| {
                let mut r = WireReader::new(d);
                let bundle = regia_net::tick::InputBundle::<Vec<u8>>::decode(&mut r).map_err(e)?;
                r.finish().map_err(e)?;
                let mut w = WireWriter::new();
                bundle.encode(&mut w);
                Ok(w.finish())
            },
        ),
        // Networked vehicles (`docs/formats/net-vehicle-wire.md`, ADR-3080): each decoder, on bits a client or a
        // server could send. A value that loads writes back to a fixed point.
        bytes(
            "net.vehicle_input",
            true,
            || {
                regia_net::codec::encode_to_vec(
                    &VehicleNetInput {
                        throttle: 0.75,
                        brake: 0.25,
                        steer: -0.5,
                        shift: VehicleShift::Up,
                    },
                    &regia_net::codec::CodecContext::detached(),
                )
            },
            codec_round_trip::<VehicleNetInput>,
        ),
        bytes(
            "net.vehicle_driver_input",
            true,
            || {
                regia_net::codec::encode_to_vec(
                    &DriverNetInput {
                        vehicle: VehicleNetInput {
                            throttle: 1.0,
                            ..VehicleNetInput::default()
                        },
                        ..DriverNetInput::default()
                    },
                    &regia_net::codec::CodecContext::detached(),
                )
            },
            codec_round_trip::<DriverNetInput>,
        ),
        bytes(
            "net.vehicle_state",
            true,
            || {
                let wheel = NetWheelLook {
                    spin_angle: 1.0,
                    suspension_length: 0.3,
                };
                regia_net::codec::encode_to_vec(
                    &NetVehicleState {
                        linear_velocity: regia_core::math::Vec3::new(0.0, -0.06, -20.0),
                        angular_velocity: regia_core::math::Vec3::new(0.0, 0.1, 0.0),
                        steer: 0.25,
                        wheels: vec![wheel; 4],
                        drivetrain: Some(regia_physics::vehicle_drivetrain::DrivetrainCore {
                            engine_speed: 300.0,
                            gear: 2,
                            target_gear: 3,
                            shift_timer: 0.1,
                            automatic: true,
                            tc_factor: 1.0,
                            wheels: vec![
                                regia_physics::vehicle_drivetrain::WheelCore {
                                    angular_speed: 50.0,
                                    spin_angle: 1.0,
                                    load: 3000.0,
                                    abs_factor: 1.0,
                                    ..Default::default()
                                };
                                4
                            ],
                            ..Default::default()
                        }),
                    },
                    &regia_net::codec::CodecContext::detached(),
                )
            },
            codec_round_trip::<NetVehicleState>,
        ),
        bytes(
            "net.vehicle_seat_request",
            true,
            || {
                regia_net::codec::encode_to_vec(
                    &VehicleSeatRequest {
                        vehicle: Some(regia_net::NetId(7)),
                        seat: 1,
                        enter: true,
                    },
                    &regia_net::codec::CodecContext::detached(),
                )
            },
            codec_round_trip::<VehicleSeatRequest>,
        ),
        bytes(
            "net.relay_server",
            false,
            || regia_net::relay::encode_register(60, b"host-token").unwrap_or_default(),
            |d| {
                // The relay answers or drops; it never fails. Its replies are the "encoding".
                let mut relay = regia_net::relay::RelayCore::new(5);
                let from = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 7), 50000));
                let replies = relay.handle(from, d, 1000);
                let again = relay.handle(from, d, 1001);
                if replies.is_empty() && again.is_empty() {
                    return Err("dropped".into());
                }
                Ok(Vec::new())
            },
        ),
        FuzzTarget {
            name: "net.join_code",
            format: None,
            format_version: 0,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: || regia_net::join_code::mint_join_code(77),
                load: |s| {
                    regia_net::join_code::normalize_join_code(s).ok_or_else(|| "not a join code".into())
                },
            },
        },
        FuzzTarget {
            name: "net.settings",
            format: Some("net-settings"),
            format_version: regia_net::settings::NET_SETTINGS_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: || {
                    regia_net::settings::NetSettings::default()
                        .to_ron()
                        .unwrap_or_default()
                },
                load: |s| {
                    regia_net::settings::NetSettings::from_ron(s)
                        .and_then(|n| n.to_ron())
                        .map_err(e_dbg)
                },
            },
        },
        // The lockstep desync bundle `*.desync.ron` (spec 120 § 7.6, ADR-8455): a sample match with
        // a planted divergence; a bundle that decodes also replays through the built-in hook.
        FuzzTarget {
            name: "net.desync",
            format: Some("desync"),
            format_version: regia_net::desync::DESYNC_BUNDLE_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: || {
                    regia_net::desync::sample::sample_bundle(
                        3,
                        12,
                        1,
                        Some(regia_net::desync::DesyncInject::new("economy", 7)),
                    )
                    .to_ron()
                    .unwrap_or_default()
                },
                load: |s| {
                    let bundle = regia_net::desync::DesyncBundle::from_ron(s).map_err(e)?;
                    let _ = regia_net::desync::ReplayRegistry::with_builtin().replay(&bundle);
                    bundle.to_ron().map_err(e)
                },
            },
        },
        // The lockstep match replay `*.lsreplay` (ADR-8513): a sample match; bytes that decode
        // also run through the bit-exact audit with the built-in hook.
        FuzzTarget {
            name: "net.lsreplay",
            format: Some("lsreplay"),
            format_version: regia_net::lockstep::replay::LSREPLAY_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: || regia_net::lockstep::replay::sample_replay(3, 24, 8, 4, None),
                load: |b| {
                    let out = regia_net::lockstep::replay::read_whole(b).map_err(e)?;
                    let _ = regia_net::lockstep::replay::audit_with(
                        b,
                        &regia_net::desync::ReplayRegistry::with_builtin(),
                    );
                    Ok(out)
                },
            },
        },
    ]
}
