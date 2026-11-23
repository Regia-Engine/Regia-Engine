// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for untrusted files that are not frozen content formats: the WebM demuxer, `.regiatrace`, the server's
//! ban list, content manifest and `server.toml`, simulated-client recordings, input bindings, the extension manifest
//! and the ABI stamp record a dynamic extension's file carries,
//! PNG input, the session messages, and the small text parsers that read what a LAN peer or a route table says.

use std::fmt::{Debug, Display};
use std::io::Cursor;

use regia_media::container::{Demuxer, MkvDemuxer};
use regia_media::mux::PatternClip;
use regia_net::session::proto::SessionMsg;

use super::formats::{pretty, text};
use super::{FuzzInput, FuzzTarget};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

fn e_dbg<E: Debug>(err: E) -> String {
    format!("{err:?}")
}

/// At most this many packets per demux: a mutated file may hold more than the fixture, but must still end.
const MAX_PACKETS: usize = 10_000;

/// A file goes past the open: every packet is read, and a seek follows, as playback does. A damaged cluster may end
/// the stream early (spec 50: the bitstream is untrusted after open); only the open has to fail cleanly.
fn webm_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let mut demuxer = MkvDemuxer::open(Cursor::new(b.to_vec())).map_err(e)?;
    let _ = demuxer.probe();
    for _ in 0..MAX_PACKETS {
        match demuxer.next_packet() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    let _ = demuxer.seek(0);
    let _ = demuxer.next_packet();
    Ok(Vec::new())
}

/// The default `server.toml` with a `motd` as long as the file, so the harness's cut at half the length
/// lands inside a quoted string. TOML has no closing delimiter: a cut at a line end is a valid shorter file,
/// and where that falls moved whenever the default gained lines.
fn server_toml_fixture() -> String {
    let default = regia_net::server_host::config::DEFAULT_SERVER_TOML;
    let motd = "m".repeat(default.len().min(4096));
    default.replacen("motd = \"\"", &format!("motd = \"{motd}\""), 1)
}

/// The default `hub.toml` with a `key_path` as long as the file, so the harness's cut at half the
/// length lands inside a quoted string. A cut on a line end is a valid shorter file.
fn hub_toml_fixture() -> String {
    let default = regia_net::lobby::hub::DEFAULT_HUB_TOML;
    let path = "k".repeat(default.len().min(4096));
    default.replacen("key_path = \"\"", &format!("key_path = \"{path}\""), 1)
}

fn trace_fixture() -> Vec<u8> {
    use regia_telemetry::trace::events::Encoder;
    use regia_telemetry::trace::file::{Header, chunk, encode_chunk, encode_header};
    use regia_telemetry::trace::{Event, EventKind, Lane};
    let mut out = encode_header(&Header {
        format_version: regia_telemetry::trace::FORMAT_VERSION,
        start_unix_ms: 1,
        engine_version: "fuzz".into(),
        ..Header::default()
    });
    let mut encoder = Encoder::new();
    for (t_ns, kind) in [
        (0, EventKind::FrameBegin { frame: 1 }),
        (
            10,
            EventKind::Mark {
                name: "door".into(),
                data: Some("{\"n\":1}".into()),
                frame: 1,
            },
        ),
    ] {
        encoder.push(
            &Event {
                t_ns,
                lane: Lane::Main,
                kind,
            },
            &mut out,
        );
    }
    encoder.cut(&mut out);
    out.extend(encode_chunk(chunk::INDEX, &[0, 0]));
    out
}

fn trace_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let trace = regia_telemetry::trace::read_trace_bytes(b).map_err(e_dbg)?;
    if trace.index.len() > b.len() {
        return Err(format!(
            "{}index of {} entries from {} bytes",
            super::BUG_PREFIX,
            trace.index.len(),
            b.len()
        ));
    }
    Ok(Vec::new())
}

/// A stamp record as it starts a section of a dynamic extension's file, with section bytes behind it. The record
/// leads so the harness's canonical rejections (a truncation to half, a hostile leading word) break it, as they
/// would break the only copy a damaged library holds; the loader itself scans any position.
fn stamp_fixture() -> Vec<u8> {
    use regia_framework::extension::stamp::{ExtensionStamp, manifest_hash};
    let mut out = ExtensionStamp::for_build(0x5EED_AB1E, 0xF1A6, manifest_hash(b"id = \"fixture_dyn\""))
        .to_bytes()
        .to_vec();
    out.extend_from_slice(&[0x22; 64]);
    out
}

/// What the host does with a library file before it opens it (`regia_editor::extension::load`): find every
/// complete record and compare them. The re-encoding is the first record, which scans back to itself.
fn stamp_load(b: &[u8]) -> Result<Vec<u8>, String> {
    use regia_framework::extension::stamp::ExtensionStamp;
    let records = ExtensionStamp::scan_file_records(b);
    let Some(first) = records.first() else {
        return Err("not_an_extension: no complete stamp record".into());
    };
    if ExtensionStamp::scan_file_bytes(b).as_ref() != Some(first) {
        return Err(format!(
            "{}the first-record scan and the all-records scan disagree",
            super::BUG_PREFIX
        ));
    }
    for other in &records[1..] {
        let _ = other.check_against(first);
    }
    let _ = first.describe();
    Ok(first.to_bytes().to_vec())
}

/// A synthetic library of one object format exporting the stamp record and needing the dynamic `std`.
fn object_fixture(format: regia_framework::extension::objfile::ObjFormat) -> Vec<u8> {
    use regia_framework::extension::objfile::{ObjFormat, synth};
    use regia_framework::extension::stamp::{ExtensionStamp, STAMP_DATA_SYMBOL, manifest_hash};
    let stamp = ExtensionStamp::for_build(0x5EED_AB1E, 0xF1A6, manifest_hash(b"id = \"fixture_dyn\""));
    let record = stamp.to_bytes();
    let exports: [(&str, &[u8]); 1] = [(STAMP_DATA_SYMBOL, &record)];
    match format {
        ObjFormat::Elf64 => synth::elf64(&exports, &["libregia_dylib.so", "libstd-0123abcd.so"]),
        ObjFormat::Pe64 => synth::pe64(&exports, &["regia_dylib.dll", "std-0123abcd.dll"]),
        ObjFormat::MachO64 => synth::macho64(
            &exports,
            &["@rpath/libregia_dylib.dylib", "@rpath/libstd-0123abcd.dylib"],
        ),
    }
}

/// What the host does with a library's bytes before the loader maps it (ADR-2844): read the exported
/// stamp and list the libraries it needs. Both answers must lie inside the input.
fn object_load(b: &[u8]) -> Result<Vec<u8>, String> {
    use regia_framework::extension::objfile::{ObjFile, links_dynamic_std};
    use regia_framework::extension::stamp::{ExtensionStamp, STAMP_DATA_SYMBOL, STAMP_SIZE};
    let file = ObjFile::parse(b).map_err(|e| e.to_string())?;
    let _ = file.needed_libraries();
    let _ = links_dynamic_std(b);
    let bytes = file
        .exported_data(STAMP_DATA_SYMBOL, STAMP_SIZE)
        .map_err(|e| e.to_string())?;
    let (start, end) = (b.as_ptr() as usize, b.as_ptr() as usize + b.len());
    let at = bytes.as_ptr() as usize;
    if bytes.len() != STAMP_SIZE || at < start || at + bytes.len() > end {
        return Err(format!(
            "{}the symbol's bytes are not inside the file",
            super::BUG_PREFIX
        ));
    }
    ExtensionStamp::from_object_bytes(b).map_err(|e| e.to_string())?;
    Ok(Vec::new())
}

fn session_fixture() -> Vec<u8> {
    SessionMsg::LevelReady { travel_id: 7 }
        .encode()
        .unwrap_or_default()
}

const PNG_SIDE: u32 = 4;

fn png_fixture() -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, PNG_SIDE, PNG_SIDE);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(&[200u8; (PNG_SIDE * PNG_SIDE * 4) as usize]);
    }
    out
}

fn input_fixture() -> String {
    use regia_input::{ActionKind, Binding, InputConfig, InputSource};
    InputConfig::new()
        .action("Jump", ActionKind::Button)
        .action("Fire", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("Space")))
        .binding(Binding::new("Fire", InputSource::key("KeyF")))
        .to_ron()
}

fn overrides_fixture() -> String {
    use regia_input::{Binding, InputOverrides, InputSource};
    let mut overrides = InputOverrides::default();
    overrides
        .contexts
        .entry(regia_input::GAMEPLAY_CONTEXT.to_string())
        .or_default()
        .insert("Jump".into(), Binding::new("Jump", InputSource::key("KeyJ")));
    overrides.to_ron()
}

fn parse_opt<T>(value: Option<T>, what: &str) -> Result<(), String> {
    value.map(|_| ()).ok_or_else(|| format!("not {what}"))
}

/// A crash bundle's `gpu_breadcrumbs.json` (`gpu-breadcrumbs` v1, spec 54 § 4.1): a fenced loss
/// with two suspects and two frames.
fn breadcrumbs_fixture() -> String {
    use regia_render::breadcrumbs::{
        AdapterReport, BreadcrumbReport, EvidenceLevel, FrameReport, LostSummary, REPORT_FORMAT_VERSION,
        SubmissionCounters, SuspectPass,
    };
    let report = BreadcrumbReport {
        format_version: REPORT_FORMAT_VERSION,
        level: EvidenceLevel::Fenced,
        adapter: AdapterReport {
            name: "llvmpipe".into(),
            backend: "vulkan".into(),
            device_type: "cpu".into(),
            ..AdapterReport::default()
        },
        lost: Some(LostSummary {
            reason: "unknown".into(),
            message: "device lost".into(),
            frame: 41,
            unix_ms: 1,
        }),
        counters: SubmissionCounters {
            submitted: 9,
            completed: 7,
            acknowledged_frame: 40,
        },
        suspects: vec![
            SuspectPass {
                name: "shadow_cascades".into(),
                frame: 41,
                submission: 8,
            },
            SuspectPass {
                name: "opaque".into(),
                frame: 41,
                submission: 9,
            },
        ],
        frames: vec![
            FrameReport {
                frame: 41,
                passes: vec!["shadow_cascades".into(), "opaque".into()],
                submissions: vec![[8, 0, 2], [9, 2, 4]],
                truncated: false,
            },
            FrameReport {
                frame: 40,
                passes: vec!["opaque".into()],
                submissions: vec![[7, 0, 2]],
                truncated: false,
            },
        ],
        truncated: false,
    };
    serde_json::to_string_pretty(&report).unwrap_or_default()
}

/// What `render.breadcrumbs { source: "last_bundle" }` does with the entry: parse it (a newer
/// version is refused by name) and write it back.
fn breadcrumbs_load(text: &str) -> Result<String, String> {
    let report = regia_render::breadcrumbs::BreadcrumbReport::from_json(text)?;
    serde_json::to_string_pretty(&report).map_err(e)
}

/// A finished-match record (`match-result` v1): a host or the CLI may read records back from a
/// folder anyone could have written to.
fn match_result_fixture() -> String {
    regia_framework::match_results::MatchResult::example().to_json()
}

/// What `rules.match_result` and `regia server results` do with a file: read it (a newer version
/// or a list past its cap is refused by name) and write it back.
fn match_result_load(text: &str) -> Result<String, String> {
    let result = regia_framework::match_results::MatchResult::from_json(text.as_bytes()).map_err(e)?;
    Ok(result.to_json())
}

/// The file and small-text targets.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        FuzzTarget {
            name: "gpu-breadcrumbs",
            format: Some("gpu-breadcrumbs"),
            format_version: regia_render::breadcrumbs::REPORT_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: breadcrumbs_fixture,
                load: breadcrumbs_load,
            },
        },
        FuzzTarget {
            name: "match_result_json",
            format: Some("match-result"),
            format_version: regia_framework::match_results::MATCH_RESULT_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: match_result_fixture,
                load: match_result_load,
            },
        },
        FuzzTarget {
            name: "media.webm",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: || PatternClip::new(16, 16, 4, 25).to_webm(),
                load: webm_load,
            },
        },
        FuzzTarget {
            name: "telemetry.regiatrace",
            format: Some("regiatrace"),
            format_version: regia_telemetry::trace::FORMAT_VERSION,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: trace_fixture,
                load: trace_load,
            },
        },
        FuzzTarget {
            name: "extension.stamp",
            format: None,
            format_version: regia_framework::extension::stamp::STAMP_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: stamp_fixture,
                load: stamp_load,
            },
        },
        FuzzTarget {
            name: "extension.objfile.elf",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: || object_fixture(regia_framework::extension::objfile::ObjFormat::Elf64),
                load: object_load,
            },
        },
        FuzzTarget {
            name: "extension.objfile.pe",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: || object_fixture(regia_framework::extension::objfile::ObjFormat::Pe64),
                load: object_load,
            },
        },
        FuzzTarget {
            name: "extension.objfile.macho",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: || object_fixture(regia_framework::extension::objfile::ObjFormat::MachO64),
                load: object_load,
            },
        },
        FuzzTarget {
            name: "agent.png_input",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: png_fixture,
                load: |b| {
                    crate::image::decode_png_rgba_within(b, crate::image::MAX_INPUT_SIDE).map(|_| Vec::new())
                },
            },
        },
        FuzzTarget {
            name: "net.session_msg",
            format: None,
            format_version: regia_net::REGIA_NET_WIRE_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: session_fixture,
                load: |b| SessionMsg::decode(b).and_then(|m| m.encode()).map_err(e_dbg),
            },
        },
        text(
            "net.bans",
            Some("bans"),
            regia_net::session::bans::BAN_LIST_FORMAT_VERSION,
            true,
            || include_str!("fixtures/bans.ron").into(),
            |s| {
                regia_net::session::bans::BanList::from_ron(s)
                    .and_then(|b| b.to_ron())
                    .map_err(e_dbg)
            },
        ),
        text(
            "net.manifest",
            Some("net-manifest"),
            regia_net::session::manifest::NET_MANIFEST_FORMAT_VERSION,
            false,
            || include_str!("fixtures/net-manifest.ron").into(),
            |s| {
                regia_net::session::manifest::ContentManifest::from_ron(s)
                    .map(|_| String::new())
                    .map_err(e_dbg)
            },
        ),
        text(
            "net.profile",
            Some("netprofile"),
            regia_net::profile::NET_PROFILE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/netprofile.ron").into(),
            |s| {
                regia_net::profile::NetProfile::from_ron(s)
                    .and_then(|p| p.to_ron())
                    .map_err(e_dbg)
            },
        ),
        text(
            "net.siminput",
            Some("siminput"),
            regia_net::session::hub::SIM_RECORDING_FORMAT_VERSION,
            true,
            || include_str!("fixtures/siminput.ron").into(),
            |s| {
                let recording = regia_net::session::hub::SimRecording::from_ron(s)?;
                pretty(&recording)
            },
        ),
        text(
            "net.server_toml",
            Some("server-toml"),
            regia_net::server_host::config::SERVER_TOML_FORMAT_VERSION,
            false,
            server_toml_fixture,
            |s| {
                regia_net::server_host::config::ServerSettings::from_toml(s)
                    .map(|_| String::new())
                    .map_err(e_dbg)
            },
        ),
        text(
            "net.playlist_ron",
            Some("playlist-ron"),
            regia_net::formats::PLAYLIST_RON_FORMAT_VERSION,
            false,
            || include_str!("../../tests/fixtures/format-corpus/playlist-ron/v2.ron").into(),
            |s| {
                regia_net::formats::PlaylistFile::from_ron(s)
                    .map(|_| String::new())
                    .map_err(e_dbg)
            },
        ),
        text(
            "net.hub_toml",
            Some("hub-toml"),
            regia_net::lobby::hub::HUB_TOML_FORMAT_VERSION,
            false,
            hub_toml_fixture,
            |s| {
                regia_net::lobby::hub::HubFile::from_toml(s)
                    .map(|_| String::new())
                    .map_err(e_dbg)
            },
        ),
        text(
            "net.ssdp_location",
            None,
            0,
            false,
            || {
                // 10.0.0.1, not 192.168.x.x: this fixture ships in every game binary, and the
                // packaging host-string gate scans for private-network addresses (ADR-10354).
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nLOCATION: http://10.0.0.1:5000/desc.xml\r\n\r\n".into()
            },
            |s| {
                parse_opt(regia_net::upnp::parse_ssdp_location(s), "an SSDP response")?;
                Ok(String::new())
            },
        ),
        text(
            "net.http_url",
            None,
            0,
            false,
            || "http://10.0.0.1:5000/desc.xml".into(),
            |s| {
                parse_opt(regia_net::upnp::parse_http_url(s), "an http url")?;
                Ok(String::new())
            },
        ),
        text(
            "net.proc_route",
            None,
            0,
            false,
            || {
                "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
                 eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n"
                    .into()
            },
            |s| {
                parse_opt(regia_net::port_map::parse_proc_route(s), "a route table")?;
                Ok(String::new())
            },
        ),
        text(
            "net.address_range",
            None,
            0,
            false,
            || "198.51.100.0/24".into(),
            |s| {
                parse_opt(
                    regia_net::session::bans::parse_address_range(s),
                    "an address range",
                )?;
                Ok(String::new())
            },
        ),
        text(
            "content.http_url",
            None,
            0,
            false,
            // Half a URL is usually a URL: this one's half-length cut ends at the colon, a bad port.
            || "http://cdn.example.com:8080/builds/manifest.ron".into(),
            |s| {
                regia_asset::content::HttpUrl::parse(s)?;
                Ok(String::new())
            },
        ),
        text(
            "asset.tag_rules",
            Some("asset-tags"),
            regia_asset::registry::TAG_RULES_FORMAT_VERSION,
            false,
            || {
                "(format_version: 1, rules: [(paths: [\"assets/props/forest/**\"], tags: {\"biome\": \"forest\"})])"
                    .into()
            },
            |s| {
                regia_asset::registry::TagRules::from_ron(s)
                    .map(|_| String::new())
                    .map_err(e)
            },
        ),
        // `config/asset_collections.ron` (ADR-7759).
        text(
            "asset.collections",
            Some("asset-collections"),
            regia_asset::asset_collections::ASSET_COLLECTIONS_FORMAT_VERSION,
            false,
            || {
                "(format_version: 1, collections: [(name: \"Props\", assets: [\"assets/props/crate.glb\"]), \
                 (name: \"Review\", query: Some(\"is:review\"))], favorites: [\"assets/props\"], \
                 folder_colors: {\"assets/props\": \"signal\"})"
                    .into()
            },
            |s| {
                regia_asset::asset_collections::AssetCollectionsFile::from_ron(s)
                    .map(|_| String::new())
                    .map_err(e)
            },
        ),
        // `config/import_presets.ron` (ADR-7904).
        text(
            "asset.import_presets",
            Some("import-presets"),
            regia_asset::import_presets::IMPORT_PRESETS_FORMAT_VERSION,
            false,
            || {
                "(format_version: 1, presets: [(name: \"UI sprite\", importer: \"texture\", \
                 settings: \"(usage: Some(ui))\"), (name: \"Character\", importer: \"gltf\", \
                 settings: \"(skin_influences: 8)\")])"
                    .into()
            },
            |s| {
                regia_asset::import_presets::ImportPresetsFile::from_ron(s)
                    .map(|_| String::new())
                    .map_err(e)
            },
        ),
        text("input.config", None, 0, true, input_fixture, |s| {
            regia_input::InputConfig::from_ron(s)?.try_to_ron().map_err(e)
        }),
        text(
            "input.overrides",
            Some("input-overrides"),
            regia_input::OVERRIDES_VERSION,
            true,
            overrides_fixture,
            |s| regia_input::InputOverrides::from_ron(s)?.try_to_ron().map_err(e),
        ),
        text(
            "extension.manifest",
            None,
            regia_framework::extension::manifest::MANIFEST_FORMAT_VERSION,
            false,
            || "format_version = 1\nid = \"studio_tools\"\nversion = \"1.2.0\"\n".into(),
            |s| {
                regia_framework::extension::manifest::ExtensionManifest::parse(s)
                    .map(|_| String::new())
                    .map_err(e_dbg)
            },
        ),
        // The client's state-frame apply path (ADR-8761): a recorded `STATE` frame applied after the
        // frames before it to a scratch client world with a small registry.
        FuzzTarget {
            name: "net.state_frame_apply",
            format: None,
            format_version: regia_net::NET_PROTOCOL_VERSION,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: regia_net::testing::state_frame_fuzz::fixture,
                load: regia_net::testing::state_frame_fuzz::load,
            },
        },
        // The standalone snapshot delta codec (sec-decode-z 2): a full packet then a delta against it, through one
        // decoder. Each input also goes through with its CRCs made right, so a mutation reaches the body decoder.
        FuzzTarget {
            name: "net.snapshot_delta",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: snapshot_delta_fixture,
                load: |b| {
                    let _ = snapshot_delta_load(&snapshot_delta_resealed(b));
                    snapshot_delta_load(b)
                },
            },
        },
        // An answer to the IGD M-SEARCH (sec-decode-z 5): the status line, the search target and the description URL.
        text(
            "net.ssdp_answer",
            None,
            0,
            false,
            || {
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nDATE: Thu, 12 Nov 2026 23:00:00 GMT\r\nEXT:\r\n\
                 SERVER: Linux/5.10 UPnP/1.0 MiniUPnPd/2.3\r\n\
                 USN: uuid:00000000-0000-0000-0000-000000000001::urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\
                 ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\
                 LOCATION: http://10.0.0.1:5000/desc.xml\r\n\r\n"
                    .into()
            },
            |s| {
                let location =
                    regia_net::upnp::parse_ssdp_answer(s).ok_or_else(|| "not an SSDP answer".to_owned())?;
                parse_opt(regia_net::upnp::parse_http_url(&location), "an http url")?;
                Ok(String::new())
            },
        ),
        // A gateway's device description (sec-decode-z 5): `xml_text` over the document, then the WAN service search
        // that reads it block by block.
        text(
            "net.upnp_description",
            None,
            0,
            false,
            upnp_description_fixture,
            |s| {
                for tag in [
                    "friendlyName",
                    "URLBase",
                    "serviceType",
                    "controlURL",
                    "errorCode",
                ] {
                    let _ = regia_net::upnp::xml_text(s, tag);
                }
                let service = regia_net::upnp::find_wan_service(s, UPNP_LOCATION)
                    .ok_or_else(|| "no WAN service in the description".to_owned())?;
                parse_opt(
                    regia_net::upnp::parse_http_url(&service.control_url),
                    "an http control url",
                )?;
                Ok(String::new())
            },
        ),
    ]
}

/// The schema of the `net.snapshot_delta` fixture: one field of every kind.
fn snapshot_delta_schema() -> Result<regia_net::SnapshotSchema, String> {
    use regia_core::math::Vec3;
    use regia_net::{FieldDef, FieldKind};
    regia_net::SnapshotSchema::new(vec![
        FieldDef::new("alive", FieldKind::Bool),
        FieldDef::new("team", FieldKind::U8),
        FieldDef::new("ammo", FieldKind::U16),
        FieldDef::new("score", FieldKind::U32),
        FieldDef::new("delta", FieldKind::I32),
        FieldDef::new(
            "health",
            FieldKind::Float {
                min: 0.0,
                max: 100.0,
                bits: 10,
            },
        ),
        FieldDef::new(
            "pos",
            FieldKind::Vec3Quantised {
                min: Vec3::splat(-512.0),
                max: Vec3::splat(512.0),
                bits: 18,
            },
        ),
        FieldDef::new("rot", FieldKind::QuatSmallestThree { bits: 10 }),
        FieldDef::new("stance", FieldKind::Enum { variants: 5 }),
    ])
    .map_err(e)
}

fn snapshot_delta_entity(id: u32, step: u32) -> regia_net::EntitySnapshot {
    use regia_core::math::{Quat, Vec3};
    use regia_net::FieldValue;
    let f = (id * 7 + step) as f32;
    regia_net::EntitySnapshot::new(
        regia_net::NetId(id),
        vec![
            FieldValue::Bool(id.is_multiple_of(2)),
            FieldValue::U8(id as u8),
            FieldValue::U16(300 + id as u16),
            FieldValue::U32(1000 * id + step),
            FieldValue::I32(step as i32 - 40),
            FieldValue::F32(f % 100.0),
            FieldValue::Vec3(Vec3::new(f, -f * 0.5, 3.25)),
            FieldValue::Quat(Quat::from_rotation_y(f * 0.1)),
            FieldValue::Enum(id % 5),
        ],
    )
}

/// A full packet (entities 1, 2, 3) followed by a delta against it that updates 1, removes 2 and adds 4.
fn snapshot_delta_fixture() -> Vec<u8> {
    let Ok(schema) = snapshot_delta_schema() else {
        return Vec::new();
    };
    let mut encoder = regia_net::SnapshotEncoder::new(schema);
    let first = regia_net::WorldSnapshot::new((1..=3).map(|id| snapshot_delta_entity(id, 0)).collect());
    let second = regia_net::WorldSnapshot::new(vec![
        snapshot_delta_entity(1, 5),
        snapshot_delta_entity(3, 0),
        snapshot_delta_entity(4, 0),
    ]);
    let mut out = encoder
        .encode(regia_net::NetTick(10), &first, None, 4096)
        .unwrap_or_default();
    let ack = Some(regia_net::Seq(1));
    out.extend(
        encoder
            .encode(regia_net::NetTick(11), &second, ack, 4096)
            .unwrap_or_default(),
    );
    out
}

/// Splits concatenated packets by their `total_len` words. A length that does not fit leaves the rest as one packet,
/// for the decoder to reject.
fn snapshot_delta_packets(b: &[u8]) -> Vec<&[u8]> {
    use regia_net::snapshot_delta::OUTER_HEADER_BYTES;
    let mut out = Vec::new();
    let mut rest = b;
    while !rest.is_empty() {
        let declared = rest
            .get(2..6)
            .and_then(|w| <[u8; 4]>::try_from(w).ok())
            .map(|w| u32::from_le_bytes(w) as usize);
        let len = match declared {
            Some(n) if (OUTER_HEADER_BYTES..=rest.len()).contains(&n) => n,
            _ => rest.len(),
        };
        let (packet, tail) = rest.split_at(len);
        out.push(packet);
        rest = tail;
    }
    out
}

/// Every packet through one decoder, as a client receives them.
fn snapshot_delta_load(b: &[u8]) -> Result<Vec<u8>, String> {
    if b.is_empty() {
        return Err("no snapshot packet".to_owned());
    }
    let mut decoder = regia_net::SnapshotDecoder::new(snapshot_delta_schema()?);
    for packet in snapshot_delta_packets(b) {
        decoder.decode(packet).map_err(e)?;
    }
    Ok(Vec::new())
}

/// The input with each packet's CRC-32 made right (like `recorder::format::resealed`). Malformed input stays malformed.
fn snapshot_delta_resealed(b: &[u8]) -> Vec<u8> {
    use regia_net::snapshot_delta::{OUTER_HEADER_BYTES, crc32_ieee};
    let mut out = Vec::with_capacity(b.len());
    for packet in snapshot_delta_packets(b) {
        let mut packet = packet.to_vec();
        if packet.len() >= OUTER_HEADER_BYTES {
            let crc = crc32_ieee(packet.get(OUTER_HEADER_BYTES..).unwrap_or_default());
            if let Some(field) = packet.get_mut(6..10) {
                field.copy_from_slice(&crc.to_le_bytes());
            }
        }
        out.extend_from_slice(&packet);
    }
    out
}

/// Where the `net.upnp_description` fixture was fetched from.
const UPNP_LOCATION: &str = "http://10.0.0.1:5000/desc.xml";

/// A gateway description with the WAN IP connection service last, so a cut at half the length loses it.
fn upnp_description_fixture() -> String {
    "<?xml version=\"1.0\"?>\r\n<root xmlns=\"urn:schemas-upnp-org:device-1-0\">\
     <specVersion><major>1</major><minor>0</minor></specVersion>\
     <device><deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>\
     <friendlyName>Home Router</friendlyName><manufacturer>Example</manufacturer>\
     <modelName>Gateway 1</modelName><UDN>uuid:00000000-0000-0000-0000-000000000001</UDN>\
     <serviceList><service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>\
     <serviceId>urn:upnp-org:serviceId:L3Forwarding1</serviceId><controlURL>/ctl/L3F</controlURL>\
     <eventSubURL>/evt/L3F</eventSubURL><SCPDURL>/L3F.xml</SCPDURL></service></serviceList>\
     <deviceList><device><deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>\
     <deviceList><device><deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType>\
     <serviceList><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>\
     <serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId><controlURL>/ctl/IPConn</controlURL>\
     <eventSubURL>/evt/IPConn</eventSubURL><SCPDURL>/WANIPCn.xml</SCPDURL></service></serviceList>\
     </device></deviceList></device></deviceList></device></root>\r\n"
        .into()
}
