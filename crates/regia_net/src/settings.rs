// SPDX-License-Identifier: MPL-2.0
//! `config/net.ron`: project settings for the whole replication layer (spec 70 § 7, spec 71 § 11).
//!
//! `format_version: 2` adds `replication.entropy` (ADR-4854). Version 1 files still load with
//! entropy off; [`migrate_net_settings`] upgrades them for `regia upgrade-project`.
//! Version 3 adds the shared timeout policy (ADR-7950); legacy `timeout_s` reads in seconds
//! and migrates to `timeout_ms`. It also carries `session.rate_control`, the transport's rate
//! controller (spec 120 § 4.2, ADR-8301), folded into the unreleased v3 as an additive section
//! (ADR-7724); files without it take its defaults. Version 4 adds
//! `session.rate_control.first_flight` (ADR-10156; opt-in, off by default, ADR-10231); a v1 to v3 file
//! reads with its default and migrates by a version step. See `docs/formats/net-ron.md`.

#[cfg(feature = "ecs")]
use regia_ecs::prelude::{Resource, bevy_ecs};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The `format_version` this build writes and the newest it reads.
pub const NET_SETTINGS_FORMAT_VERSION: u32 = 4;

/// Version of the replication wire layout (spawn records, deltas, despawns, message frames). Not a
/// content format. Version 2 adds the acknowledged state channel (spec 72 § 4, ADR-1950) and the
/// input and input acknowledgment frames of client prediction (spec 73). Version 3 adds the
/// entropy stage for state frames (spec 72 § 18, ADR-3315); protocol 2 peers connect with entropy
/// off. Version 4 is the quantized transform record with deltas against the acknowledged baseline
/// (spec 120 § 8.2, ADR-8467): a build of protocol 3 or older has another transform record, and its
/// protocol hash refuses it at the handshake (`protocol_mismatch`), so it is never misread.
pub const NET_PROTOCOL_VERSION: u32 = 4;

/// The oldest protocol this build still speaks: a peer between this and
/// [`NET_PROTOCOL_VERSION`] joins, and its connection falls back to what it speaks (ADR-1950).
/// Since protocol 4 a real build older than 4 never gets that far: its transform record differs,
/// and so does its protocol hash (ADR-8467).
pub const MIN_NET_PROTOCOL_VERSION: u32 = 1;

/// Default of [`ReplicationSettings::android_listen_max_clients`] (spec 72 § 14): an Android
/// listen server serves at most 8 clients.
pub const ANDROID_LISTEN_MAX_CLIENTS_DEFAULT: u32 = 8;

/// Smallest `replication.mtu_bytes` a `config/net.ron` may name.
///
/// 64 bytes of transport reserve, 16 of state-frame header and 128 of payload: the same sum as
/// `replication::MIN_STATE_MTU_BYTES`. It lives here so settings validation compiles without the
/// `ecs` feature (that module is ECS-only). The replication module asserts the two stay equal.
pub(crate) const MIN_STATE_MTU_BYTES: u32 = 64 + 16 + 128;

/// The effective Android listen cap: `0` in `config/net.ron` reads as
/// [`ANDROID_LISTEN_MAX_CLIENTS_DEFAULT`] (spec 72 § 14).
pub fn android_listen_cap_value(android_listen_max_clients: u32) -> u32 {
    if android_listen_max_clients == 0 {
        ANDROID_LISTEN_MAX_CLIENTS_DEFAULT
    } else {
        android_listen_max_clients
    }
}

/// The player capacity a server enforces (spec 72 § 14): on Android a listen (non-dedicated)
/// server accepts at most `replication.android_listen_max_clients` players even when
/// `max_players` is higher; dedicated servers and every other platform keep `max_players`. The
/// platform is a parameter (not `cfg!` inside) so any host can test the rule.
pub fn listen_capacity(
    is_android: bool,
    dedicated: bool,
    max_players: u32,
    android_listen_max_clients: u32,
) -> u32 {
    if is_android && !dedicated {
        max_players.min(android_listen_cap_value(android_listen_max_clients))
    } else {
        max_players
    }
}

/// How a server sends entity state (`replication.state_channel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StateChannel {
    /// Deltas against acknowledged baselines on the unreliable channel; a client that does not
    /// speak protocol 2 keeps the reliable tick stream.
    #[default]
    Unreliable,
    /// The reliable tick stream for every connection (a diagnosis switch).
    Reliable,
}

/// Why `config/net.ron` could not be read.
#[derive(Debug, Error)]
pub enum NetSettingsError {
    /// Not valid RON for the settings shape.
    #[error("config/net.ron: {0}")]
    Parse(String),
    /// Written by a newer build.
    #[error("config/net.ron: format_version {found} is newer than {supported}")]
    TooNew {
        /// The file's version.
        found: u32,
        /// The newest this build reads.
        supported: u32,
    },
}

/// Upgrades v1 to v3 `config/net.ron` to v4 while retaining settings (ADR-7950, ADR-8301,
/// ADR-10200, ADR-10231).
///
/// Missing entropy defaults to off, and so does a missing `session.rate_control.first_flight`
/// (a v1 to v3 project keeps the start it had). The
/// output uses LF and includes the current settings; a file already at v4 is returned unchanged,
/// including comments.
pub fn migrate_net_settings(text: &str) -> Result<String, NetSettingsError> {
    let mut settings = NetSettings::from_ron(text)?;
    if settings.format_version == NET_SETTINGS_FORMAT_VERSION {
        return Ok(text.to_owned());
    }
    if !matches!(settings.format_version, 1..=3) {
        return Err(NetSettingsError::Parse(format!(
            "no migrator from config/net.ron v{}",
            settings.format_version
        )));
    }
    settings.format_version = NET_SETTINGS_FORMAT_VERSION;
    settings.to_ron()
}

/// A token-bucket rate: `count` tokens per `per_ticks` ticks (spec 70 § 6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    /// Bucket size and tokens gained per window.
    pub count: u32,
    /// Window length in ticks (at least 1).
    pub per_ticks: u32,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            count: 120,
            per_ticks: 60,
        }
    }
}

/// The `messages` section (spec 70 § 7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageSettings {
    /// Rate of a message type that names none.
    pub default_rate: RateLimit,
    /// Inbound messages per peer per tick before the peer counts a breach.
    pub peer_cap_per_tick: u32,
    /// Inbound bytes per peer per second before the peer counts a breach.
    pub peer_bytes_per_second: u32,
    /// Quiet ticks after which a `Slowed` peer is `Clear` again.
    pub slowed_recover_ticks: u32,
    /// Ticks of breaching while `Slowed` before the peer is disconnected with `Flood`.
    pub disconnect_after_slowed_ticks: u32,
    /// A frame whose simulation step reached this many milliseconds forgives the next frame's
    /// burst; `0` disables forgiveness. The comparison reads the simulation clock
    /// (`Time::sim_delta`): under `TimeUpdateStrategy::Automatic` the frame delta is clamped to
    /// 250 ms, and a frame at the clamp counts as reaching any threshold of 250 ms or more.
    /// The simulation step is scaled by `Time::time_scale`: in slow motion (scale 0.5) a
    /// stalled frame reaches at most 125 ms, so the default threshold never forgives, and at
    /// scale 2.0 an ordinary 125 ms frame does. A game that runs slow motion online lowers the
    /// threshold to the scaled clamp (spec 70 § 6.6).
    pub stall_forgive_ms: u32,
    /// Ticks a message waits for its target's spawn before it is dropped as unresolved.
    pub unresolved_wait_ticks: u32,
}

impl Default for MessageSettings {
    fn default() -> Self {
        Self {
            default_rate: RateLimit::default(),
            peer_cap_per_tick: 512,
            peer_bytes_per_second: 262_144,
            slowed_recover_ticks: 300,
            disconnect_after_slowed_ticks: 600,
            stall_forgive_ms: 250,
            unresolved_wait_ticks: 32,
        }
    }
}

/// The `replication` section (spec 71 § 11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplicationSettings {
    /// `false` disables sleep everywhere (a diagnosis switch).
    pub sleep_enabled: bool,
    /// Pending entity references a client keeps (oldest dropped and counted).
    pub max_pending_refs: u32,
    /// Reserved. A `NetVec` holds `replication::NETVEC_MAX` elements: that bound is part of the wire
    /// layout, so both ends must agree on it and this setting changes nothing.
    pub netvec_max: u32,
    /// Ticks between send ticks (spec 72 § 3): frames are assembled on ticks that are a multiple
    /// of it, and priorities and budgets count send ticks. 0 reads as 1.
    pub send_every_ticks: u32,
    /// The project default `.netprofile.ron` (spec 72 § 8).
    pub default_profile: String,
    /// Per-connection rate in bytes per second (spec 72 § 7).
    pub client_rate_bytes: u32,
    /// Total outbound cap in bytes per second (0 = none).
    pub server_rate_bytes: u32,
    /// Largest state packet in bytes, transport headers included (spec 72 § 7). A record larger
    /// than a packet goes reliably; tick frames are fragmented by the transport.
    pub mtu_bytes: u32,
    /// How entity state travels (spec 72 § 4).
    pub state_channel: StateChannel,
    /// Spawn records per connection per send tick.
    pub spawns_per_send: u32,
    /// Interest grid cell size in metres.
    pub grid_cell_m: f32,
    /// Clients an Android listen server accepts (spec 72 § 14). Enforced: on Android a listen
    /// server refuses players past this cap even when `max_players` is higher; 0 reads as
    /// [`ANDROID_LISTEN_MAX_CLIENTS_DEFAULT`]. Dedicated servers and other platforms ignore it.
    pub android_listen_max_clients: u32,
    /// Entropy coding of state frames (spec 72 § 18, ADR-3315). Negotiated per connection: a
    /// protocol 2 peer receives byte-identical protocol 2 frames with the stage off.
    pub entropy: crate::entropy::EntropyMode,
}

impl Default for ReplicationSettings {
    fn default() -> Self {
        Self {
            sleep_enabled: true,
            max_pending_refs: 4096,
            netvec_max: 4096,
            send_every_ticks: 2,
            default_profile: "engine://net/profiles/default.netprofile.ron".into(),
            client_rate_bytes: 49_152,
            server_rate_bytes: 0,
            mtu_bytes: 1200,
            state_channel: StateChannel::default(),
            spawns_per_send: 24,
            grid_cell_m: 64.0,
            android_listen_max_clients: ANDROID_LISTEN_MAX_CLIENTS_DEFAULT,
            entropy: crate::entropy::EntropyMode::Off,
        }
    }
}

/// `config/net.ron`. A resource; the net layer reads it every tick. Nothing loads the file for you:
/// parse it with [`NetSettings::from_ron`] and insert the result before the net plugins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ecs", derive(Resource))]
#[serde(default)]
pub struct NetSettings {
    /// Always [`NET_SETTINGS_FORMAT_VERSION`] when written.
    pub format_version: u32,
    /// Message delivery and flood limits (spec 70).
    pub messages: MessageSettings,
    /// Entity replication (spec 71).
    pub replication: ReplicationSettings,
    /// Proxy interpolation (spec 74 § 10).
    pub interpolation: InterpolationSettings,
    /// Session lifecycle timeouts and limits (spec 77 § 12).
    pub session: crate::session::SessionSettings,
    /// Client prediction and server reconciliation (spec 73 § 10).
    pub prediction: PredictionSettings,
    /// Lag compensation for hit checks (spec 75 § 9, spec 90 § 3.1).
    pub lag_compensation: LagCompSettings,
    /// Lobbies and matchmaking (spec 89 § 6).
    pub lobby: crate::lobby::LobbySettings,
}

impl Default for NetSettings {
    fn default() -> Self {
        Self {
            format_version: NET_SETTINGS_FORMAT_VERSION,
            messages: MessageSettings::default(),
            replication: ReplicationSettings::default(),
            interpolation: InterpolationSettings::default(),
            session: crate::session::SessionSettings::default(),
            prediction: PredictionSettings::default(),
            lag_compensation: LagCompSettings::default(),
            lobby: crate::lobby::LobbySettings::default(),
        }
    }
}

impl NetSettings {
    /// Reads `config/net.ron` text. Missing fields take their defaults; a newer `format_version`
    /// is refused. A `session.timeout_ms` the file names wins over legacy `session.timeout_s`,
    /// whatever its value; the seconds count only when the file has no `timeout_ms` (ADR-7950).
    pub fn from_ron(text: &str) -> Result<Self, NetSettingsError> {
        let mut settings: NetSettings =
            ron::from_str(text).map_err(|e| NetSettingsError::Parse(e.to_string()))?;
        if settings.format_version > NET_SETTINGS_FORMAT_VERSION {
            return Err(NetSettingsError::TooNew {
                found: settings.format_version,
                supported: NET_SETTINGS_FORMAT_VERSION,
            });
        }
        if settings.replication.mtu_bytes < MIN_STATE_MTU_BYTES {
            return Err(NetSettingsError::Parse(format!(
                "replication_mtu_too_small: mtu_bytes must be at least {}",
                MIN_STATE_MTU_BYTES
            )));
        }
        if let Some(named) = session_names_timeout_ms(text) {
            settings.session.resolve_loaded_timeout(named);
        }
        Ok(settings)
    }

    /// Writes the settings as RON. `\n` on every platform: ron's default is `\r\n` on Windows,
    /// and `config/net.ron` is a hashed gameplay kind, so its bytes must not depend on the host
    /// (ADR-4772, same fix as `KnownHosts::to_ron`, ADR-3150).
    /// Saving a legacy file stamps the current schema version (ADR-4854).
    pub fn to_ron(&self) -> Result<String, NetSettingsError> {
        let mut current = self.clone();
        current.format_version = NET_SETTINGS_FORMAT_VERSION;
        current.session.timeout_ms = current.session.timeout_policy().timeout_ms;
        ron::ser::to_string_pretty(&current, regia_core::ron_pretty())
            .map_err(|e| NetSettingsError::Parse(e.to_string()))
    }
}

/// Whether the `session` section of `config/net.ron` text names `timeout_ms`, whatever its value
/// (`None` when the keys cannot be read; the settings are then left as parsed). Only the key is
/// looked at: every other field and value is skipped.
fn session_names_timeout_ms(text: &str) -> Option<bool> {
    /// A key that is present, whatever it holds.
    #[derive(Default)]
    struct Named(bool);
    impl<'de> Deserialize<'de> for Named {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            serde::de::IgnoredAny::deserialize(d).map(|_| Self(true))
        }
    }
    #[derive(Deserialize, Default)]
    #[serde(default, rename = "SessionSettings")]
    struct SessionKeys {
        timeout_ms: Named,
    }
    #[derive(Deserialize, Default)]
    #[serde(default, rename = "NetSettings")]
    struct Keys {
        session: SessionKeys,
    }
    ron::from_str::<Keys>(text)
        .ok()
        .map(|keys| keys.session.timeout_ms.0)
}

/// The `interpolation` section (spec 74 § 10): the client's proxy timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InterpolationSettings {
    /// Target delay in send intervals before jitter is added.
    pub delay_send_intervals: f64,
    /// Lower clamp of the target delay, in milliseconds.
    pub min_delay_ms: f64,
    /// Upper clamp of the target delay, in milliseconds.
    pub max_delay_ms: f64,
    /// Largest clock slow-down while the delay grows (0.10 = 10 % slower than real time).
    pub raise_rate: f64,
    /// Largest clock speed-up while the delay shrinks (0.02 = 2 % faster than real time).
    pub lower_rate: f64,
    /// A delay error above this resyncs the clock, in milliseconds.
    pub resync_error_ms: f64,
    /// Half-life of the visual offset left after extrapolation, in milliseconds.
    pub recover_half_life_ms: f64,
    /// States buffered per proxy.
    pub buffer_states: u32,
}

impl Default for InterpolationSettings {
    fn default() -> Self {
        Self {
            delay_send_intervals: 2.0,
            min_delay_ms: 33.0,
            max_delay_ms: 250.0,
            raise_rate: 0.10,
            lower_rate: 0.02,
            resync_error_ms: 500.0,
            recover_half_life_ms: 60.0,
            buffer_states: 32,
        }
    }
}

/// The `prediction` section (spec 73 § 10): the owning client's input stream, lead, history,
/// rollback and correction smoothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PredictionSettings {
    /// Unacknowledged commands resent with every input packet (newest first).
    pub input_redundancy: u32,
    /// Ticks of prediction history kept per predicted entity (clamped to 1..=128,
    /// `predict::MAX_HISTORY_TICKS`; 64 = 1,067 ms at 60 Hz). A rollback deeper than this cannot
    /// re-run: it snaps to the server's state (spec 120 § 9.4).
    pub history_ticks: u32,
    /// The rollback depth at low round trips. The depth used grows with the measured round trip
    /// (round trip + margin target + 2 ticks + one send interval) up to `history_ticks`; a deeper
    /// rollback still re-runs to the client tick and counts as a reset (spec 120 § 9.4, ADR-8219).
    pub max_resim_ticks: u32,
    /// Target input buffer margin on the server, in ticks, before jitter is added.
    pub margin_target_ticks: u32,
    /// Upper clamp of the margin target, in ticks.
    pub margin_max_ticks: u32,
    /// A margin error up to this many ticks is corrected by dilation; a larger one resyncs.
    pub dilation_window_ticks: u32,
    /// Largest change of the client's tick rate while dilating (0.05 = ±5 %).
    pub max_dilation: f64,
    /// Half-life of the visual correction offset, in milliseconds.
    pub smooth_half_life_ms: f64,
    /// A correction larger than this snaps instead of smoothing, in meters.
    pub snap_distance_m: f32,
    /// Acknowledged input ticks a predicted spawn waits for its server spawn before it is rejected.
    pub spawn_grace_ticks: u32,
    /// Input packets accepted per player per server tick; more are dropped.
    pub max_input_packets_per_tick: u32,
    /// Opt-in: the server adopts the client's reported position while the distance stays inside
    /// this many metres per second (`None` = off). The budget refills with the tick length and
    /// never exceeds this rate (spec 73 § 6.8).
    pub trust_client_within_m: Option<f32>,
    /// Tests only: re-run every predicted tick twice from the same start and compare.
    pub verify_resim: bool,
}

impl Default for PredictionSettings {
    fn default() -> Self {
        Self {
            input_redundancy: 16,
            history_ticks: 64,
            max_resim_ticks: 32,
            margin_target_ticks: 2,
            margin_max_ticks: 8,
            dilation_window_ticks: 8,
            max_dilation: 0.05,
            smooth_half_life_ms: 40.0,
            snap_distance_m: 2.0,
            spawn_grace_ticks: 8,
            max_input_packets_per_tick: 4,
            trust_client_within_m: None,
            verify_resim: false,
        }
    }
}

/// The `lag_compensation` section (spec 75 § 9, spec 90 § 3.1): the server's hit-shape history
/// and claim tolerances.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LagCompSettings {
    /// Runtime switch: `false` judges every shot at the present tick (no rewind). The history is
    /// still recorded, so switching back takes effect at once.
    pub enabled: bool,
    /// The project maximum of every shooter's rewind window, in milliseconds (spec 120 § 9.6):
    /// each shooter's window is its measured round trip + input lead + interpolation delay + one
    /// tick + an allowance, at most this; a shooter the server has not measured gets this. At most
    /// `lagcomp::MAX_REWIND_CEILING_MS` (1,000; a larger value acts as 1,000). A larger maximum
    /// lets players on longer round trips hit where they aimed, at the cost of their targets being
    /// hit later after reaching cover (ADR-8315).
    pub max_rewind_ms: f64,
    /// Ticks of flight during which a projectile is judged at the shooter's view (0 = never).
    pub projectile_rewind_ticks: u32,
    /// How much a claimed shape grows when a claim is checked, in metres.
    pub claim_tolerance_m: f32,
    /// Largest distance between a claim's ray origin and the server's muzzle point, in metres.
    pub claim_origin_tolerance_m: f32,
    /// Largest gap between a claim's view tick and the shooter's reported view tick, in ticks.
    pub claim_view_tick_slack: f64,
    /// Hit shapes recorded per entity; more are ignored (logged once).
    pub max_shapes_per_entity: u32,
    /// Dev builds: send a shooter the rewound shapes of its hits (spec 75 § 11, `LagCompEchoPlugin`).
    pub debug_echo: bool,
}

impl Default for LagCompSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rewind_ms: 1_000.0,
            projectile_rewind_ticks: 6,
            claim_tolerance_m: 0.1,
            claim_origin_tolerance_m: 0.5,
            claim_view_tick_slack: 2.0,
            max_shapes_per_entity: 32,
            debug_echo: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit fix-net3-determinism M3 (ADR-4772): `config/net.ron` is a hashed gameplay kind, so
    /// its bytes must not depend on the host. ron's default newline is `\r\n` on Windows, so this
    /// only fails there; the assertion guards the contract on every platform.
    #[test]
    fn to_ron_uses_lf_on_every_platform() {
        let text = NetSettings::default().to_ron().expect("ron");
        assert!(text.contains('\n'), "the text has newlines");
        assert!(!text.contains('\r'), "no CR bytes on any platform");
        assert_eq!(
            NetSettings::from_ron(&text).expect("re-parse"),
            NetSettings::default()
        );
    }

    #[test]
    fn spec_example_loads_and_round_trips() {
        let text = r#"NetSettings(
            format_version: 1,
            messages: (
                default_rate: (count: 120, per_ticks: 60),
                peer_cap_per_tick: 512,
                peer_bytes_per_second: 262144,
                slowed_recover_ticks: 300,
                disconnect_after_slowed_ticks: 600,
                stall_forgive_ms: 250,
                unresolved_wait_ticks: 32,
            ),
        )"#;
        let s = NetSettings::from_ron(text).expect("spec example");
        assert_eq!(
            s,
            NetSettings {
                format_version: 1,
                ..NetSettings::default()
            }
        );
        let back = NetSettings::from_ron(&s.to_ron().expect("write")).expect("read back");
        assert_eq!(back, NetSettings::default());
    }

    #[test]
    fn net_settings_ron_uses_lf_on_every_host() {
        // The asserts are host-independent; on Windows they fail until the newline is pinned
        // (ron's PrettyConfig default is `\r\n` there).
        let text = NetSettings::default().to_ron().expect("write");
        assert!(text.contains('\n'), "expected LF newlines");
        assert!(
            !text.contains('\r'),
            "the newline must be pinned to LF on every host"
        );
        assert_eq!(
            NetSettings::from_ron(&text).expect("read back"),
            NetSettings::default()
        );
    }

    #[test]
    fn listen_capacity_applies_the_android_listen_cap_only() {
        // Android listen: capped at the setting (0 reads as the default 8), never above
        // max_players.
        assert_eq!(listen_capacity(true, false, 16, 8), 8);
        assert_eq!(listen_capacity(true, false, 256, 8), 8);
        assert_eq!(listen_capacity(true, false, 16, 0), 8);
        assert_eq!(listen_capacity(true, false, 16, 2), 2);
        // The same inputs elsewhere keep max_players: Linux listen, Android dedicated.
        assert_eq!(listen_capacity(false, false, 16, 8), 16);
        assert_eq!(listen_capacity(true, true, 16, 8), 16);
        assert_eq!(listen_capacity(false, true, 256, 8), 256);
    }

    #[test]
    fn spec_72_replication_section_loads() {
        let text = r#"(replication: (
            send_every_ticks: 2,
            default_profile: "engine://net/profiles/default.netprofile.ron",
            client_rate_bytes: 49152,
            server_rate_bytes: 0,
            mtu_bytes: 1200,
            spawns_per_send: 24,
            grid_cell_m: 64.0,
            android_listen_max_clients: 8,
            sleep_enabled: true,
            max_pending_refs: 4096,
            netvec_max: 4096,
        ))"#;
        let s = NetSettings::from_ron(text).expect("spec 72 example");
        assert_eq!(s.replication, ReplicationSettings::default());
    }

    #[test]
    fn spec_72_entropy_key_loads_with_additive_default() {
        let s = NetSettings::from_ron("(replication: (entropy: Range))").expect("entropy key");
        assert_eq!(s.replication.entropy, crate::entropy::EntropyMode::Range);
        let default = NetSettings::default();
        assert_eq!(default.replication.entropy, crate::entropy::EntropyMode::Off);
        // Older files without the key read as off.
        let old = NetSettings::from_ron("(replication: (grid_cell_m: 32.0))").expect("old file");
        assert_eq!(old.replication.entropy, crate::entropy::EntropyMode::Off);
    }

    #[test]
    fn missing_fields_default_and_newer_versions_refuse() {
        let s = NetSettings::from_ron("(replication: (sleep_enabled: false))").expect("partial");
        assert!(!s.replication.sleep_enabled);
        assert_eq!(s.messages.peer_cap_per_tick, 512);
        assert!(NetSettings::from_ron("(format_version: 2)").is_ok());
        // Version 4 is current since ADR-10200 (`session.rate_control.first_flight`).
        assert!(NetSettings::from_ron("(format_version: 4)").is_ok());
        assert!(matches!(
            NetSettings::from_ron("(format_version: 5)"),
            Err(NetSettingsError::TooNew {
                found: 5,
                supported: 4
            })
        ));
        assert!(NetSettings::from_ron("(messages: 5)").is_err());
    }

    #[test]
    fn interpolation_section_spec_example_loads() {
        let text = r#"(interpolation: (
            delay_send_intervals: 2.0,
            min_delay_ms: 33.0,
            max_delay_ms: 250.0,
            raise_rate: 0.10,
            lower_rate: 0.02,
            resync_error_ms: 500.0,
            recover_half_life_ms: 60.0,
            buffer_states: 32,
        ))"#;
        let s = NetSettings::from_ron(text).expect("spec 74 example");
        assert_eq!(s.interpolation, InterpolationSettings::default());
        let partial = NetSettings::from_ron("(interpolation: (max_delay_ms: 400.0))").expect("partial");
        assert_eq!(partial.interpolation.max_delay_ms, 400.0);
        assert_eq!(partial.interpolation.buffer_states, 32);
    }

    #[test]
    fn prediction_spec_example_loads() {
        let text = r#"(
            prediction: (
                input_redundancy: 16,
                history_ticks: 64,
                max_resim_ticks: 32,
                margin_target_ticks: 2,
                margin_max_ticks: 8,
                dilation_window_ticks: 8,
                max_dilation: 0.05,
                smooth_half_life_ms: 40.0,
                snap_distance_m: 2.0,
                spawn_grace_ticks: 8,
                max_input_packets_per_tick: 4,
                trust_client_within_m: None,
            ),
        )"#;
        let s = NetSettings::from_ron(text).expect("spec 73 example");
        assert_eq!(s.prediction, PredictionSettings::default());
        let partial =
            NetSettings::from_ron("(prediction: (trust_client_within_m: Some(0.05)))").expect("partial");
        assert_eq!(partial.prediction.trust_client_within_m, Some(0.05));
        assert_eq!(partial.prediction.history_ticks, 64);
    }

    #[test]
    fn spec_75_lag_compensation_section_loads() {
        let text = r#"(lag_compensation: (
            max_rewind_ms: 250.0,
            projectile_rewind_ticks: 6,
            claim_tolerance_m: 0.1,
            claim_origin_tolerance_m: 0.5,
            max_shapes_per_entity: 32,
        ))"#;
        let s = NetSettings::from_ron(text).expect("spec 75 example");
        // The example's 250 ms was the default before spec 120 § 9.6 (ADR-8315); every other
        // value is still the default.
        assert_eq!(
            s.lag_compensation,
            LagCompSettings {
                max_rewind_ms: 250.0,
                ..LagCompSettings::default()
            }
        );
        let off = NetSettings::from_ron("(lag_compensation: (enabled: false))").expect("switch");
        assert!(!off.lag_compensation.enabled);
        assert_eq!(
            off.lag_compensation.max_rewind_ms, 1_000.0,
            "spec 120 § 9.6 default"
        );
    }
}
