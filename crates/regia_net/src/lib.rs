// SPDX-License-Identifier: MPL-2.0
//! `regia_net`: client-server networking for games built on Regia Engine.
//!
//! Phase 1 of the networking line (`docs/master-plan.md` § 3.5b, ADR-1600) is the generic transport
//! and session layer in the table below. Phase 2, game state on top of it, is here too: the network
//! model ([`model`](mod@model), spec 70), entity replication (`replication`,
//! spec 71), relevance (`relevance`, spec 72), client prediction and
//! reconciliation (`predict`, spec 73), snapshot interpolation
//! (`interp`, spec 74) and lag compensation (`lagcomp`, spec 75).
//!
//! | Module | What |
//! |---|---|
//! | [`connection`] | Reliable-ordered and unreliable channels over one UDP flow (acks, resends, fragments, RTT) |
//! | [`handshake`] | Connect request, stateless cookie challenge, join token, accept/deny, session tokens |
//! | [`password`] | Optional host password kept as a salted hash |
//! | [`crypto`] | Transport encryption: Noise NX, sealed records, replay window, server keys (ADR-1901) |
//! | [`server`] / [`client`] | The endpoints a game drives, generic over its [`NetGame`] |
//! | [`session`] | Peer registry, health flags, silence timeout, IP bans; the session lifecycle of spec 77 (`NetSession`) |
//! | [`tick`] | Authoritative tick driver, per-peer input buffers, redundant input bundles |
//! | [`congestion`] / [`rate_limit`] / [`send_clock`] | Loss-driven send budget, reliable cap, send cadence |
//! | [`conditioner`] | Seeded loss / delay / jitter / reorder / burst for tests and `net.conditioner_set` |
//! | [`relay`] / [`join_code`] / [`port_map`] / [`upnp`] / [`discovery`] | Rendezvous and forwarding, join codes, PCP / NAT-PMP / UPnP IGD, LAN announce |
//! | [`status`] | `net.status` / `net.peers` data, the diagnostics overlay and the agent bridge resource |
//! | [`socket`] | Real UDP (buffer read-back and DSCP EF, ADR-7998) and an in-memory loopback network |
//! | `server_host` | The dedicated-server host of spec 76: `server.toml`, pacing, console, health, signals |
//! | [`lobby`] | Lobbies, matchmaking and the lobby-to-session launch of spec 89 (`Lobbies`, `LobbyPlugin`) |
//! | `recorder` | The network recorder of spec 103: a virtual connection, `.netdemo` files, a memory ring |
//! | `voice` | Feature `voice`: voice chat relay and client transport (specs/59, ADR-1911) |
//! | [`rollback`] | Game-owned snapshots, replay from a corrected tick, and a tick-counted visual offset (spec 120 § 9.5) |
//!
//! Everything game-specific enters through [`NetGame`]: the game's message set, its per-tick
//! input and its join data are the game's own types implementing [`NetCodec`]. The engine never
//! sees a game rule.
//!
//! ```
//! use regia_net::prelude::*;
//!
//! struct Pong;
//! impl NetGame for Pong {
//!     type Message = Vec<u8>;
//!     type Input = i32;
//!     type JoinData = ();
//!     const GAME_ID: &'static str = "example.pong";
//!     const PROTOCOL_VERSION: u32 = 1;
//! }
//!
//! let net = LoopbackNet::new();
//! let server_socket = net.bind("127.0.0.1:7777".parse().expect("addr")).expect("bind");
//! let client_socket = net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
//! let mut server: NetServer<Pong, _> = NetServer::new(server_socket, ServerConfig::default());
//! let mut client: NetClient<Pong, _> = NetClient::connect(
//!     client_socket,
//!     "127.0.0.1:7777".parse().expect("addr"),
//!     ClientConfig::default(),
//!     &(),
//! )
//! .expect("join data fits");
//! for step in 0..20u64 {
//!     client.update(step * 10);
//!     server.update(step * 10);
//! }
//! assert!(client.is_connected());
//! assert_eq!(server.session().len(), 1);
//! ```

// The `#[derive(Replicate)]` expansion names `regia_net` by path; this lets it work inside the
// crate's own tests too.
extern crate self as regia_net;

pub mod bits;
#[cfg(feature = "ecs")]
pub mod bridge;
pub mod client;
pub mod codec;
pub mod conditioner;
pub mod congestion;
pub mod connection;
pub mod crypto;
#[cfg(feature = "ecs")]
mod dense;
pub mod discovery;
pub mod entropy;
// Replication for a sim outside the ECS (spec 120 § 8.6, ADR-8186).
pub mod external_replication;
mod fast_map;
pub mod flood;
// Schema fingerprints of the versioned files for `cargo xtask check-formats` (ADR-2316).
pub mod formats;
pub mod handshake;
// The interest grid, relevance shape and rate tiers both replication paths share (ADR-8186).
pub mod interest;
#[cfg(feature = "ecs")]
pub mod interp;
pub mod join_code;
#[cfg(feature = "ecs")]
pub mod lagcomp;
pub mod lobby;
pub mod message;
pub mod model;
#[cfg(feature = "ecs")]
pub mod origin;
pub mod password;
pub mod payload;
#[cfg(feature = "ecs")]
pub mod plugin;
pub mod port_map;
#[cfg(feature = "ecs")]
pub mod predict;
#[cfg(feature = "ecs")]
pub mod profile;
pub mod rate_limit;
#[cfg(feature = "ecs")]
pub mod recorder;
pub mod relay;
#[cfg(feature = "ecs")]
pub mod relevance;
#[cfg(feature = "ecs")]
pub mod replication;
#[cfg(feature = "ecs")]
pub mod resume;
pub mod rollback;
#[cfg(feature = "ecs")]
pub mod runtime;
mod scalar;
pub mod send;
pub mod send_clock;
pub mod server;
#[cfg(feature = "ecs")]
pub mod server_host;
pub mod session;
pub mod settings;
pub mod snapshot_delta;
pub mod socket;
#[cfg(feature = "ecs")]
mod stable_math;
pub mod status;
pub mod testing;
pub mod tick;
pub mod upnp;
#[cfg(feature = "voice")]
pub mod voice;
pub mod wire;

// App-level transport glue (ADR-2039).
#[cfg(feature = "ecs")]
pub mod link;
// Trust on first use for server keys (ADR-2416).
pub mod known_hosts;
// Outside tests only the dedicated-server host (feature `ecs`) calls it (ADR-8032).
#[cfg(all(windows, any(feature = "ecs", test)))]
#[allow(unsafe_code)]
// Every unsafe block below names its invariants in a SAFETY comment (docs/04-conventions.md § 5).
pub(crate) mod windows_acl;
// Owner-only files on every platform (server identity key, known-hosts store; ADR-4828). Public for
// `regia_agent`'s token, endpoint and audit log on Windows (ADR-5489) and the CLI's signing and
// content-key files (the engine's one owner-only helper, ADR-5616).
pub mod private_file;
// One IPv4 address or one IPv6 /64 for every per-address rule (ADR-3283).
pub mod addr_block;
// Conditioner specs from a command line or the environment (ADR-3075).
pub mod conditioner_spec;
// The DACL walk shared by the Windows secret-file checks; pure, so it is tested on every host
// (ADR-5880).
#[cfg(any(windows, test))]
mod acl_scan;
// UDP socket-buffer and DSCP sockopts (ADR-7998).
mod socket_opts;
// The frozen version probe and the handshake's version stamps (spec 120 § 12.2, ADR-7985).
pub mod version_probe;
// Server input feedback, client clock filter and input lead (spec 120 § 6.4, § 9.1, § 9.2; ADR-8077).
pub mod feedback;
// Lockstep mode: host frames with redundancy, agreed-tick input delay, quorum hash authority
// (spec 120 § 7.1-7.3, § 7.5; ADR-8146).
pub mod lockstep;
// The lockstep server game type's host side: the seat cap, the observer thread and its step clock,
// driving `lockstep::LockstepHost` through its tick API; outside the scanned lockstep module
// (ADR-8558, ADR-8656). Also at `lockstep::server`.
pub mod lockstep_server;
// Desync detection: the game's state hashes, subsystem rows, the probe, the injection hook
// (spec 120 § 7.6; ADR-8211).
pub mod desync;
// NAT traversal from the game socket: candidates, paced checks, nomination (spec 120 § 10.7,
// ADR-8340).
pub mod nat;
// Automatic client reconnect after a timeout: the backoff timeline (spec 120 § 10.3, ADR-8352).
pub mod reconnect;
// Per-peer inbound rate buckets, flood timer and malformed leak on the server (ADR-8181).
pub mod inbound;
// The host check before hosting: port-forward self-check and uplink capacity (spec 120 § 10.7,
// ADR-8370).
pub mod host_check;
// Client capture with timing and its replay (spec 120 § 15.5, ADR-8221).
pub mod capture;

// Warm successors for replication games: the successor journal and its shadow world (spec 120
// § 10.5, ADR-8853).
pub mod migration;
// The hand-off of a migrating replication game: rosters, tickets and the promoted server (spec 120
// § 10.5, ADR-8901).
pub mod handoff;

pub use client::{ClientConfig, ClientEnd, ClientEvent, ClientState, NetClient};
pub use conditioner::{CONDITIONER_PROFILES, ConditionerConfig, ConditionerStats, NetConditioner};
pub use conditioner_spec::{CONDITIONER_ENV, CONDITIONER_SEED_ENV};
pub use connection::{Channel, LinkConfig, LinkStats, SendError};
pub use crypto::{CryptoError, CryptoMode, ServerIdentity, ServerKey};
pub use fast_map::{FastHasher, FastMap};
pub use handshake::{DenyReason, DisconnectReason, PeerRole};
pub use inbound::InboundRateConfig;
#[cfg(feature = "ecs")]
pub use link::{LinkClock, LinkEvent, LinkInterpolationPlugin, LinkOptions, NetLink, NetLinkPlugin};
pub use payload::{NetCodec, NetGame, REGIA_NET_WIRE_VERSION, protocol_id};
pub use rollback::{RollbackCounters, RollbackError, RollbackLog, VisualOffset};
pub use server::{AdoptError, NetServer, RejectKind, ServerConfig, ServerEvent};
pub use session::{HealthPolicy, PeerId};
pub use snapshot_delta::{
    DecodedSnapshot, EncodeReport, EntitySnapshot, FieldDef, FieldKind, FieldValue, RecordSelection, Seq,
    SnapshotDecoder, SnapshotEncoder, SnapshotError, SnapshotSchema, WorldSnapshot,
};
pub use socket::{
    DEFAULT_RECV_BUFFER_BYTES, DEFAULT_SEND_BUFFER_BYTES, DSCP_EF_TOS, MAX_SOCKET_BUFFER_BYTES,
    SocketBufferReport, SocketBufferSettings,
};
pub use socket::{DatagramSocket, LoopbackNet, LoopbackSocket, UdpTransport};
pub use status::{
    DiagnosticsOverlay, EndpointReport, MAX_REJECTION_PEERS, NetAgentState, NetStatus, PeerRejections,
    PeerRow, RejectionsByPeer,
};
pub use tick::{BUNDLE_HEADER_BYTES, MAX_INPUT_BUNDLES_PER_SEND, MAX_RELIABLE_INPUT_BYTES};
pub use tick::{TickClock, TickDriver, TickInputs};
pub use wire::{WireError, WireReader, WireWriter};

pub use bits::{BitReader, BitWriter, DecodeError};
pub use codec::{CodecContext, ReplicateCodec};
#[cfg(feature = "ecs")]
pub use interp::{
    InterpProfile, InterpolateAppExt, NetInterpProfile, NetInterpolation, NetInterpolationPlugin,
    NetPendingDespawn, NetStarved, NetViewTick, net_interpolate,
};
#[cfg(feature = "ecs")]
pub use lagcomp::{
    ClaimInvalid, ClaimVerdict, HitAttach, HitClaim, HitClaimPolicy, HitFilter, HitShape, HitShapeKind,
    HitShapes, LagCompEcho, LagCompEchoLog, LagCompEchoPlugin, LagCompError, LagCompHistory,
    LagCompJointSource, LagCompOcclusion, LagCompPlugin, LagCompProjectile, LagCompWindows, LagCompensated,
    LagCompensation, LagHit, NoLagCompensation, PoseSource, RewoundView, ShooterWindow, set_shooter_rtt,
};
#[cfg(feature = "ecs")]
pub use lobby::{Lobbies, LobbyPlugin};
pub use lobby::{LobbyBackend, LocalLobbyHub, Matchmaker};
// The message wire trait is core (ADR-8501); the App extension, the sender and the delivery
// resource are `ecs`.
pub use message::{
    BroadcastMessage, Direction, NetMessage, NetSendError, ToOwnerMessage, ToServerMessage, ValidateContext,
    Verdict,
};
#[cfg(feature = "ecs")]
pub use message::{NetMessageAppExt, NetMessages, NetSender};
#[cfg(feature = "ecs")]
pub use model::NetInterrupted;
pub use model::{
    LocalPeer, NetId, NetMode, NetOwnedLocally, NetOwner, NetPlayer, NetPlayerId, NetRole, NetTick,
    NetTickFrac, has_authority, is_locally_controlled, is_locally_controlled_owned,
};
#[cfg(feature = "ecs")]
pub use model::{NetOwnerLink, offline, on_client, on_server, with_local_players};
#[cfg(feature = "ecs")]
pub use plugin::{NetPlugin, NetSet};
#[cfg(feature = "ecs")]
pub use predict::{
    EntityTolerance, InputCommand, MotionMode, NetClock, NetInput, NetInputAppExt, NetPredictedSpawnRejected,
    NetPredictionKey, NetResimulating, NetVisualChild, NetVisualOffset, PredictAppExt, Predicted,
    PredictedScheduleExt, PredictedUpdate, PredictedValues, PredictionCx, PredictionKey, TickDrivenMotion,
    Tolerance, add_net_input, add_predicted_systems, net_predict, predict, predict_entity_tolerance,
    predict_local_writes,
};
#[cfg(feature = "ecs")]
pub use profile::{
    InterpolationMode, NetProfile, NetProfileRef, NetProfiles, ProfileError, RateTier, RateTiers,
    RelevanceShape,
};
#[cfg(feature = "ecs")]
pub use regia_macros::Replicate;
#[cfg(feature = "ecs")]
pub use relevance::{
    GrantTarget, NetGroupVisibility, NetInterest, NetRelevanceAppExt, NetRelevanceFilter, NetViewpoint,
    NetVisibility, RelevanceStage, RelevanceVerdict, add_net_relevance_filter, install_net_interest,
    net_grant_relevance,
};
#[cfg(feature = "ecs")]
pub use replication::{
    Audience, NetAsleep, NetChanged, NetChangedEvents, NetCombatHeat, NetEvents, NetGroups, NetSleep, NetVec,
    NotifyWhen, ReplicateAppExt, ReplicateOptions, Replicated, TransformReplication, net_note_combat,
    net_resend, net_teleport, net_wake,
};
#[cfg(feature = "ecs")]
pub use resume::{GraceHook, GraceVerdict, NetGraceHook, NetResumeState};
pub use session::TimeoutPolicy;
#[cfg(feature = "ecs")]
pub use session::net_session::{NetSessionEvents, NetSessionPlugin, SessionAppExt};
#[cfg(feature = "ecs")]
pub use session::{NetSession, SessionRules};
pub use settings::{
    LagCompSettings, MIN_NET_PROTOCOL_VERSION, NET_PROTOCOL_VERSION, NetSettings, PredictionSettings,
    StateChannel,
};

/// The common imports for a game using `regia_net`.
pub mod prelude {
    pub use crate::{
        Channel, ClientConfig, ClientEnd, ClientEvent, ClientState, ConditionerConfig, DatagramSocket,
        DenyReason, DisconnectReason, LoopbackNet, NetAgentState, NetClient, NetCodec, NetGame, NetServer,
        PeerId, PeerRole, RejectKind, ServerConfig, ServerEvent, TickClock, TickInputs, UdpTransport,
        WireError, WireReader, WireWriter,
    };
}

// `#[derive(NetMessage)]` (spec 70 § 6.1, ADR-2415). The derive lives in the macro namespace, the
// trait of the same name in the type namespace, so both import as `regia_net::NetMessage`.
pub use regia_macros::NetMessage;
// Replicated asset handles: the asset-server context of the codec (spec 71 § 3, ADR-2415).
#[cfg(feature = "ecs")]
pub mod assets;
// Net profiles through the asset server: `engine://net/profiles/` and cooked `.netprofile.ron` (ADR-7232).
#[cfg(feature = "ecs")]
pub mod profile_assets;
pub mod quic;
// Path validation of address changes, both endpoints (ADR-8299; RFC 9000 §§ 8-9).
pub mod path;

pub use feedback::{InputTiming, LinkFeedback};
#[cfg(feature = "net-quic")]
pub use quic::{QuicDatagramSocket, QuicListener, QuicSocketControl};
pub use quic::{QuicStatus, quic_enabled, quic_error_code, quic_status};
// Spec 77 § 6.2 (ADR-7212): the travel marker games add to team or score entities.
#[cfg(feature = "ecs")]
pub use session::travel::NetPersistAcrossTravel;

// Spec 120 § 14 (ADR-7954): address join codes, the LAN announcer and the store-ticket hook.
pub use address_join::{
    ADDRESS_JOIN_IPV4_CHARS, ADDRESS_JOIN_IPV6_CHARS, ADDRESS_JOIN_TTL_DEFAULT_S, AddressJoin,
    AddressJoinError, decode_address_join, encode_address_join, mint_address_join,
};
pub use discovery::{DiscoveryConfig, DiscoveryHub, LanAnnouncer, LanSendError, LanSocket};
pub use ticket::{JoinTicketCheck, TicketRefused, TicketVerdict};

// Address join codes and the store-ticket hook (spec 120 § 14, ADR-7954).
// Declared after the re-exports: rustfmt sorts a module into the `quic` group
// and would reorder that group.
pub mod address_join;
pub mod ticket;

// Transport v2 framing: varints, truncated packet numbers, acknowledgment frames with ranges
// (spec 120 § 4.1, ADR-8184).
pub mod ack;
pub use connection::ReceiveTimestamp;
// Path MTU discovery: DPLPMTUD probes, search and black-hole detection (spec 120 § 4.3, ADR-8300).
pub mod pmtud;
pub use pmtud::MtuSearch;
// Transport v2 rate controller, pacer and shared uplink (spec 120 § 4.2, ADR-8301).
pub mod cc;
pub use cc::{RateConfig, RateController, RateState, RoundSample, SharedUplink};
/// Whether this build has the ECS half (feature `ecs`, spec 120 § 3.2): a test built on the core
/// (`--no-default-features`) asserts `false` to prove what it exercised links no ECS (ADR-8558).
pub const ECS_FEATURE: bool = cfg!(feature = "ecs");
// Unconfirmed resume tickets and reconnect tokens, kept for a retry until the client confirms (ADR-8623).
mod retry_tokens;
// Pre-proof answers shared fairly between source prefixes, refunded on proof (spec 120 § 11.4, ADR-8718).
mod answer_share;
// Per-address slots held for players that left holding a reconnect ticket (ADR-8733).
mod address_hold;
// Pending one-round-trip resumes, ordered by flight number (ADR-8858 A1, A3-A5; ADR-8902).
mod resume_register;
// Published external replication stats for agents: `net.external_stats` (KI-NT-012, ADR-8892).
pub use external_replication::NetExternalStats;
// Per-peer connection quality, grade and operator stats rows (spec 120 § 13; ADR-8182).
pub mod quality;
pub mod stats;
pub use quality::{GradeLimits, LinkGrade, PeerQuality};
