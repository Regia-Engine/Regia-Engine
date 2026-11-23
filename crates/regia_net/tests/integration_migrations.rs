// SPDX-License-Identifier: MPL-2.0
//! Frozen network configuration regressions for the K batch 3 integration (ADR-4854).

use regia_net::server_host::config::{SERVER_TOML_FORMAT_VERSION, ServerSettings, migrate_server_toml};

#[test]
fn prior_server_versions_migrate_without_changing_settings_or_comments() {
    assert_eq!(SERVER_TOML_FORMAT_VERSION, 4);
    assert_eq!(regia_net::formats::server_toml_migration_sources(), [1, 2, 3]);
    for version in [1, 2, 3] {
        let text = format!(
            "# retained\nformat_version = {version} # schema\n[levels]\nrotation = [\"arena\"]\n[persistence]\nenabled = true\nkeep = 7\n[bots]\nmin_players = 2\nmax_bots = 3\n"
        );
        let mut before = ServerSettings::from_toml(&text).expect("old version loads");
        let upgraded = migrate_server_toml(&text).expect("prior version migrates");
        assert!(upgraded.starts_with("# retained\nformat_version = 4 # schema\n"));
        let after = ServerSettings::from_toml(&upgraded).expect("new version loads");
        before.format_version = SERVER_TOML_FORMAT_VERSION;
        assert_eq!(after, before, "all settings survive v{version} migration");
        assert_eq!(migrate_server_toml(&upgraded).expect("idempotent"), upgraded);
    }
}

#[test]
fn entropy_configuration_has_a_registered_schema_migration() {
    // Version 4 (ADR-10200) adds `session.rate_control.first_flight`; v1 to v3 migrate.
    assert_eq!(regia_net::settings::NET_SETTINGS_FORMAT_VERSION, 4);
    assert_eq!(regia_net::formats::net_settings_migration_sources(), [1, 2, 3]);
    let old = regia_net::NetSettings::from_ron("(format_version: 1, replication: (send_every_ticks: 7))")
        .expect("v1 still loads");
    assert_eq!(old.replication.send_every_ticks, 7);
    assert_eq!(old.replication.entropy, regia_net::entropy::EntropyMode::Off);
}

#[test]
fn net_settings_migrator_preserves_nondefault_fields_and_rejects_unknown_versions() {
    let text = "(format_version: 1, replication: (send_every_ticks: 7, mtu_bytes: 900), prediction: (history_ticks: 99))";
    let mut before = regia_net::NetSettings::from_ron(text).expect("source");
    let upgraded = regia_net::settings::migrate_net_settings(text).expect("migrate");
    let after = regia_net::NetSettings::from_ron(&upgraded).expect("upgraded");
    before.format_version = regia_net::settings::NET_SETTINGS_FORMAT_VERSION;
    assert_eq!(after, before);
    assert!(!upgraded.contains('\r'));
    assert_eq!(
        regia_net::settings::migrate_net_settings(&upgraded).expect("idempotent"),
        upgraded
    );
    for text in ["(format_version: 0)", "(format_version: 5)", "broken"] {
        assert!(regia_net::settings::migrate_net_settings(text).is_err(), "{text}");
    }
}

/// `session.rate_control` (spec 120 § 4.2, ADR-8301) is folded into the unreleased v3 as an
/// additive section (ADR-7724): a v2 file migrates with every setting kept and the rate
/// controller's defaults (on, 16 KiB/s up and 32 KiB/s down floors, 10 s breaker, listen servers
/// share the uplink); a v3 file without the section takes them; a v3 file names its own values.
#[test]
fn net_settings_rate_control_is_additive_in_v3_with_its_defaults() {
    let text = "(format_version: 2, session: (timeout_s: 40.0), replication: (client_rate_bytes: 30000))";
    let mut before = regia_net::NetSettings::from_ron(text).expect("v2");
    let upgraded = regia_net::settings::migrate_net_settings(text).expect("migrate");
    let after = regia_net::NetSettings::from_ron(&upgraded).expect("current");
    // A v2 file migrates to the current version (v4 since ADR-10200), rate control kept.
    before.format_version = regia_net::settings::NET_SETTINGS_FORMAT_VERSION;
    assert_eq!(after.replication, before.replication);
    assert_eq!(after.session.rate_control, before.session.rate_control);
    assert!(upgraded.contains("format_version: 4"), "{upgraded}");
    assert!(upgraded.contains("rate_control"), "{upgraded}");
    let plain = regia_net::NetSettings::from_ron("(format_version: 3, session: (timeout_ms: 40000))")
        .expect("v3 without rate_control");
    assert_eq!(plain.session.rate_control, regia_net::cc::RateSettings::default());
    let r = &after.session.rate_control;
    assert!(r.enabled && r.listen_shared_uplink);
    assert_eq!(
        (r.floor_up_bytes_per_second, r.floor_down_bytes_per_second),
        (16_384, 32_768)
    );
    assert_eq!(r.circuit_breaker_ms, 10_000);
    let own = regia_net::NetSettings::from_ron(
        "(format_version: 3, session: (rate_control: (enabled: false, floor_up_bytes_per_second: 8192)))",
    )
    .expect("v3 values");
    assert!(!own.session.rate_control.enabled);
    assert_eq!(own.session.rate_control.client().floor_bytes_per_second, 8_192);
    assert!(!own.session.rate_control.server().enabled);
}

/// Version 4 (ADR-10200, ADR-10231): `session.rate_control.first_flight`, off by default (the
/// lead's revised ruling on ADR-10156's first flight: opt-in for v0.7). A v3 file has no such
/// key: it loads with the first flight off, as before, and migrates by a version step with every
/// other setting kept; a v4 file can switch it on, and the switch reaches both ends' controller
/// settings.
#[test]
fn net_settings_v4_adds_the_first_flight_key_off_by_default() {
    let v3 =
        "(format_version: 3, session: (timeout_ms: 40000, rate_control: (start_bytes_per_second: 65536)))";
    let mut before = regia_net::NetSettings::from_ron(v3).expect("v3 loads");
    assert!(
        !before.session.rate_control.first_flight,
        "a v3 file reads the default"
    );
    assert!(!regia_net::cc::RateSettings::default().first_flight);
    assert!(!regia_net::cc::RateConfig::default().first_flight);
    let upgraded = regia_net::settings::migrate_net_settings(v3).expect("v3 migrates");
    assert!(upgraded.contains("format_version: 4"), "{upgraded}");
    assert!(upgraded.contains("first_flight: false"), "{upgraded}");
    let after = regia_net::NetSettings::from_ron(&upgraded).expect("v4 loads");
    before.format_version = 4;
    assert_eq!(after, before, "every v3 setting survives");
    assert_eq!(
        regia_net::settings::migrate_net_settings(&upgraded).expect("idempotent"),
        upgraded
    );
    let on = regia_net::NetSettings::from_ron(
        "(format_version: 4, session: (rate_control: (first_flight: true)))",
    )
    .expect("v4 names its own value");
    let r = &on.session.rate_control;
    assert!(r.first_flight);
    assert!(r.server().first_flight && r.client().first_flight);
    let off = regia_net::cc::RateSettings::default();
    assert!(!off.server().first_flight && !off.client().first_flight);
}

#[test]
fn writing_legacy_net_settings_stamps_the_current_schema() {
    let mut old = regia_net::NetSettings::from_ron("(format_version: 1, replication: (mtu_bytes: 900))")
        .expect("legacy load");
    let text = old.to_ron().expect("write settings");
    let current = regia_net::NetSettings::from_ron(&text).expect("read settings");
    assert_eq!(
        current.format_version,
        regia_net::settings::NET_SETTINGS_FORMAT_VERSION
    );
    old.format_version = regia_net::settings::NET_SETTINGS_FORMAT_VERSION;
    assert_eq!(current, old);
}

#[test]
fn timeout_policy_migrates_legacy_seconds_and_writes_only_milliseconds() {
    for version in [1, 2] {
        let text = format!("(format_version: {version}, session: (timeout_s: 30.0))");
        let migrated = regia_net::settings::migrate_net_settings(&text).expect("migrate");
        let settings = regia_net::NetSettings::from_ron(&migrated).expect("load");
        assert_eq!(settings.session.timeout_policy().timeout_ms, 30_000);
        assert_eq!(settings.session.interrupt_ms, 1_500);
        assert_eq!(settings.session.start_grace_ms, 30_000);
        assert!(migrated.contains("timeout_ms: 30000"));
        assert!(!migrated.contains("    timeout_s:"));
        assert_eq!(
            regia_net::settings::migrate_net_settings(&migrated).expect("idempotent"),
            migrated
        );
    }
    let settings = regia_net::NetSettings::from_ron(
        "(format_version: 3, session: (timeout_ms: 40000, interrupt_ms: 2500, start_grace_ms: 60000))",
    )
    .expect("v3");
    assert_eq!(
        settings.session.timeout_policy(),
        regia_net::session::TimeoutPolicy {
            timeout_ms: 40_000,
            interrupt_ms: 2_500,
            start_grace_ms: 60_000
        }
    );
}

#[test]
fn server_timeout_policy_is_configurable_and_migrates_without_resetting_it() {
    for version in [1, 2, 3, 4] {
        let text = format!(
            "format_version = {version}\n[levels]\nrotation = [\"arena\"]\n[net]\ntimeout_ms = 40000\ninterrupt_ms = 2500\nstart_grace_ms = 60000\n"
        );
        let migrated = migrate_server_toml(&text).expect("migrate");
        let settings = ServerSettings::from_toml(&migrated).expect("load");
        assert_eq!(
            settings.timeout,
            regia_net::session::TimeoutPolicy {
                timeout_ms: 40_000,
                interrupt_ms: 2_500,
                start_grace_ms: 60_000
            }
        );
    }
}

#[test]
fn timeout_policy_reload_reports_every_restart_only_key() {
    let before = ServerSettings::default();
    let mut after = before.clone();
    after.timeout.interrupt_ms += 1;
    after.timeout.timeout_ms += 1;
    after.timeout.start_grace_ms += 1;
    assert_eq!(
        before.restart_only_changes(&after),
        ["net.interrupt_ms", "net.timeout_ms", "net.start_grace_ms"]
    );
    assert!(before.hot_changes(&after).is_empty());
}
