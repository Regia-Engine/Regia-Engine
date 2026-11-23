// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for the formats frozen after the registry was built (ADR-1917): auto-tile rules, chunk
//! reasons and the chunk host record, demos and match records, voices, dialogue and subtitles, the cooked
//! extension record, the loudness table, motion-matching sources, retarget setups, nav lanes and links, plus
//! two unfrozen readers (cooked nav cells and the dedicated server's key file), and the painted scatter store
//! (`<map>.scatterpaint.bin`, spec 82 § 13, ADR-1936), cloth and soft-body assets (spec 55 § 4) and the cooked
//! permutation list (ADR-2026), entity kind declarations (spec 101). Fixtures are checked-in
//! examples (`docs/formats/`, copied or extracted into `fixtures/`) or values the format's own code builds.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::sync::OnceLock;

use regia_animation::RetargetSetup;
use regia_animation::motion_matching::MotionDatabaseSource;
use regia_asset::AssetId;
use regia_asset::content::{
    ChunkWhy, ContentHost, ContentSignature, ContentTrust, SignaturePolicy, WhyReason, WhyRecord,
};
use regia_asset::dialogue::{
    self, Cue, DialogueAsset, DialogueLine, Gender, Number, SubtitleTrack, Variant, VoiceAsset,
};
use regia_audio::{LoudnessPolicy, LoudnessTable, SoundLoudness};
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_framework::demo::{DemoHeader, DemoKeyframe, DemoRecording};
use regia_framework::extension::cooked::{CookedExtension, CookedExtensions};
use regia_framework::input_recording::{decode_frec, encode_frec};
use regia_nav::{
    NavLaneSet, NavLinkSet, NavMeshBuildParams, NavTileCache, NavTileCacheParams, NavTileLayout,
};
use regia_net::ServerIdentity;
use regia_terrain::{
    Brush, DirtyRect, PaintedInstance, PaintedScatter, ScatterBrushMode, ScatterDelta, ScatterStroke,
};

use super::formats::{pretty, reflected_fixture, reflected_load, text};
use super::{FuzzInput, FuzzTarget};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

/// The reason table of a small cook: a rule, a follower, a duplicated asset, a default and a pinned one.
fn chunk_why_fixture() -> Vec<u8> {
    let id = |n: u128| AssetId(Guid::from_u128(n));
    let record = |n: u128, path: &str, chunk: u32, reason: WhyReason| WhyRecord {
        id: id(n),
        path: path.into(),
        chunk,
        reason,
        duplicated_in: Vec::new(),
    };
    ChunkWhy {
        chunks: vec!["arena".into(), "base".into()],
        selectors: BTreeMap::from([(
            "arena".to_owned(),
            vec!["levels/arena/**".to_owned(), "tag:arena".to_owned()],
        )]),
        records: vec![
            record(1, "assets/levels/arena/main.level.ron", 0, WhyReason::Rule(0)),
            record(2, "assets/arena/rock.mesh", 0, WhyReason::Follows(id(1))),
            WhyRecord {
                duplicated_in: vec![0, 1],
                ..record(3, "assets/ui/hud.png", 1, WhyReason::Shared)
            },
            record(4, "assets/ui/font.ttf", 1, WhyReason::Default),
            record(5, "assets/boot.level.ron", 1, WhyReason::Pinned),
        ],
    }
    .to_bytes()
    .unwrap_or_default()
}

/// A reason table goes past the decode: every record is explained (the `content.why` path) and every chunk's
/// direct assets are listed (the editor's Content panel).
fn chunk_why_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let table = ChunkWhy::from_bytes(b).map_err(e)?;
    for record in &table.records {
        let _ = table.explain(record.id);
    }
    for chunk in &table.chunks {
        let _ = table.direct_assets(chunk);
    }
    table.to_bytes().map_err(e)
}

/// A two-step demo with a snapshot keyframe, a hash-only keyframe and input timestamps.
fn demo_value() -> DemoRecording {
    let header = DemoHeader {
        step_count: 2,
        ..DemoHeader::new("arena", 42, 60, 1)
            .with_build("c0ffee1")
            .with_content("a1b2c3")
    };
    DemoRecording {
        header,
        actions: vec!["Fire".into(), "Move".into()],
        steps: vec![
            vec![Vec3::ZERO, Vec3::new(0.25, 0.0, 0.0)],
            vec![Vec3::X, Vec3::new(0.0, 0.0, -1.0)],
        ],
        keyframes: vec![
            DemoKeyframe {
                tick: 0,
                state_hash: 1_234_567_890,
                snapshot_hex: "0a01".into(),
            },
            DemoKeyframe {
                tick: 2,
                state_hash: 77,
                snapshot_hex: String::new(),
            },
        ],
        timestamps: vec![vec![16_667, 16_901], Vec::new()],
    }
}

fn voice_fixture() -> String {
    pretty(&VoiceAsset {
        format_version: dialogue::VOICE_VERSION,
        id: "guard_captain".into(),
        name_key: "voice-guard-captain".into(),
        gender: Gender::Feminine,
        number: Number::Singular,
    })
    .unwrap_or_default()
}

fn cue(at: f32, until: Option<f32>, text_key: Option<&str>, text: Option<&str>) -> Cue {
    Cue {
        at,
        until,
        text_key: text_key.map(str::to_owned),
        text: text.map(str::to_owned),
    }
}

/// Two lines: one with a default and a speaker/listener variant carrying inline cues, one with its own keys.
fn dialogue_fixture() -> String {
    let halt = DialogueLine {
        id: "halt".into(),
        speaker: "guard_captain".into(),
        listeners: vec!["player".into()],
        note: "Shouted from the wall.".into(),
        text_key: None,
        display_key: None,
        mature: false,
        variants: vec![
            Variant {
                speaker: None,
                listener: None,
                sound: "audio/vo/halt.ogg".into(),
                cues: None,
            },
            Variant {
                speaker: Some("guard_captain".into()),
                listener: Some("player".into()),
                sound: "audio/vo/halt_player.ogg".into(),
                cues: Some(vec![
                    cue(0.0, Some(1.25), Some("dlg-gate-halt-1"), None),
                    cue(1.25, None, Some("dlg-gate-halt-2"), None),
                ]),
            },
        ],
    };
    let pass = DialogueLine {
        id: "pass".into(),
        speaker: "guard_captain".into(),
        listeners: Vec::new(),
        note: String::new(),
        text_key: Some("dlg-gate-pass-custom".into()),
        display_key: Some("dlg-gate-pass-short".into()),
        mature: true,
        variants: Vec::new(),
    };
    pretty(&DialogueAsset {
        format_version: dialogue::DIALOGUE_VERSION,
        name: "gate".into(),
        lines: vec![halt, pass],
    })
    .unwrap_or_default()
}

fn subtitles_fixture() -> String {
    pretty(&SubtitleTrack {
        format_version: dialogue::SUBTITLES_VERSION,
        priority: 2,
        keep_breaks: true,
        one_at_a_time: false,
        cues: vec![
            cue(0.0, Some(1.5), Some("subs-door-1"), None),
            cue(1.5, None, None, Some("*creak*")),
        ],
    })
    .unwrap_or_default()
}

fn extensions_fixture() -> String {
    let extension = |id: &str, version: &str, manifest_hash: &str| CookedExtension {
        id: id.into(),
        version: version.into(),
        manifest_hash: manifest_hash.into(),
    };
    CookedExtensions {
        format_version: regia_framework::extension::cooked::COOKED_FORMAT_VERSION,
        resolved_hash: "5f0c9a".into(),
        extensions: vec![
            extension("dialogue", "1.4.2", "a31e"),
            extension("weather", "0.3.0", "77b0"),
        ],
    }
    .to_ron()
    .unwrap_or_default()
}

/// The table of `docs/formats/loudness-ron.md`'s example, plus a silent clip.
fn loudness_fixture() -> String {
    let door = SoundLoudness {
        integrated_lufs: Some(-17.42),
        loudness_range_lu: 0.0,
        true_peak_db: Some(-3.1),
        short_clip: true,
        dual_mono: true,
        sample_rate: 48_000,
        frames: 9_600,
        normalize: true,
        target_lufs: -23.0,
        gain_db: -5.58,
        peak_limited: false,
    };
    let silence = SoundLoudness {
        integrated_lufs: None,
        true_peak_db: None,
        short_clip: false,
        dual_mono: false,
        frames: 96_000,
        normalize: false,
        gain_db: 0.0,
        ..door
    };
    LoudnessTable {
        format_version: regia_audio::LOUDNESS_TABLE_VERSION,
        policy: LoudnessPolicy::default(),
        entries: BTreeMap::from([
            ("assets/sfx/door.wav".to_owned(), door),
            ("assets/sfx/silence.wav".to_owned(), silence),
        ]),
    }
    .to_ron_string()
    .unwrap_or_default()
}

fn nav_cell_params() -> NavTileCacheParams {
    NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size: 64,
    }
}

/// A 32 m floor built into a whole tile cache once, with one off-mesh link: its layout and the cooked
/// cell of tile (0, 0).
fn nav_cell_world() -> Option<&'static (NavTileLayout, Vec<u8>)> {
    static WORLD: OnceLock<Option<(NavTileLayout, Vec<u8>)>> = OnceLock::new();
    WORLD
        .get_or_init(|| {
            let c = 16.0;
            let floor = [
                [
                    Vec3::new(-c, 0.0, -c),
                    Vec3::new(c, 0.0, -c),
                    Vec3::new(c, 0.0, c),
                ],
                [
                    Vec3::new(-c, 0.0, -c),
                    Vec3::new(c, 0.0, c),
                    Vec3::new(-c, 0.0, c),
                ],
            ];
            let mut whole = NavTileCache::build(nav_cell_params(), &floor).ok()?;
            // A link on tile (0, 0), so the fixture reaches the v2 link records (ADR-2589).
            let origin = whole.layout().origin;
            let from = Vec3::new(origin.x + 2.0, 0.0, origin.z + 2.0);
            whole.set_off_mesh_links(vec![regia_nav::OffMeshLink::new(
                from,
                from + Vec3::new(3.0, 0.0, 0.0),
            )]);
            let blob = whole.cook_cell(&[(0, 0)]).ok()?;
            Some((whole.layout(), blob))
        })
        .as_ref()
}

/// A cooked cell goes past the decode: loaded into an empty streamed twin of the floor's cache, then the tile
/// is rebuilt from the loaded heights and areas. Nothing writes a cell back from a streamed cache.
/// The types a saved `NavLinkAnchor` needs.
fn nav_anchor_register(registry: &mut regia_ecs::prelude::TypeRegistry) {
    registry.register::<regia_nav::NavLinkAnchor>();
    registry.register::<regia_nav::NavLinkKind>();
}

fn traversal_settings_register(registry: &mut regia_ecs::prelude::TypeRegistry) {
    registry.register::<regia_framework::traversal::TraversalSettings>();
    registry.register::<regia_framework::traversal::SwimSettings>();
    registry.register::<regia_framework::traversal::LadderSettings>();
    registry.register::<regia_framework::traversal::MantleSettings>();
}

fn traversal_ladder_register(registry: &mut regia_ecs::prelude::TypeRegistry) {
    registry.register::<regia_framework::traversal::Ladder>();
}

fn traversal_jump_point_register(registry: &mut regia_ecs::prelude::TypeRegistry) {
    registry.register::<regia_framework::traversal::TraversalJumpPoint>();
}

fn logic_graph_ref_register(registry: &mut regia_ecs::prelude::TypeRegistry) {
    registry.register::<regia_ai::logic::LogicGraphRef>();
    registry.register::<regia_ai::logic::LogicLocalOverride>();
    registry.register::<regia_ai::logic::LogicOverrideValue>();
}

/// The `logic-graph-ref` fixture: a reference with one override of each kind (P-165, ADR-8566).
fn logic_graph_ref_fixture() -> regia_ai::logic::LogicGraphRef {
    use regia_ai::logic::{LogicGraphRef, LogicLocalOverride, LogicOverrideValue};
    let local = |name: &str, value| LogicLocalOverride {
        name: name.into(),
        value,
    };
    LogicGraphRef {
        overrides: vec![
            local("open", LogicOverrideValue::Bool(true)),
            local("count", LogicOverrideValue::Int(3)),
            local("speed", LogicOverrideValue::Float(1.5)),
            local("label", LogicOverrideValue::String("north".into())),
        ],
        ..LogicGraphRef::new("assets/prefabs/door.logic.ron")
    }
}

/// The deterministic navmesh fixture (`NVM2`, ADR-8174): two rooms joined by a personnel door, an
/// upper room over the second and a stair up to it, so every polygon kind but ground and tunnel and
/// both link kinds (shared edge, overlap) are in the bytes.
fn det_navmesh_fixture() -> Vec<u8> {
    use regia_nav::det::navmesh::{BakeParts, DoorQuad, NavMesh, Room, StairRun};
    let room = |min_x: f32, max_x: f32, floor_y: f32, level: i32| Room {
        min_x,
        min_z: -10.0,
        max_x,
        max_z: 10.0,
        floor_y,
        ceil_y: floor_y + 3.2,
        level,
    };
    let parts = BakeParts {
        rooms: vec![
            room(-20.0, -1.0, 0.0, 0),
            room(1.0, 20.0, 0.0, 0),
            room(1.0, 20.0, 3.0, 1),
        ],
        doors: vec![DoorQuad {
            center: [0.0, 0.0],
            dir: [1.0, 0.0],
            width_m: 1.10,
            depth_m: 2.0,
            floor_y: 0.0,
            ceil_y: 2.2,
            level: 0,
        }],
        stairs: vec![StairRun {
            from: [4.0, -8.0],
            to: [4.0, 8.0],
            width_m: 2.0,
            y0: 0.0,
            y1: 3.0,
            ceil_m: 3.0,
        }],
        extent_m: 40.0,
        arena: true,
        ..BakeParts::default()
    };
    NavMesh::bake(&parts).map(|m| m.to_bytes()).unwrap_or_default()
}

/// Load a cooked navmesh and write it again (the loader sorts links and canonicalizes floats, so a
/// loaded mesh's bytes are a fixed point).
fn det_navmesh_load(b: &[u8]) -> Result<Vec<u8>, String> {
    use regia_nav::det::navmesh::{NAVMESH_MAGIC, NAVMESH_VERSION, NavMesh};
    NavMesh::from_bytes(b)
        .map(|m| m.to_bytes())
        .ok_or_else(|| match (b.get(..4), b.get(4..8)) {
            (Some(magic), Some(v)) if magic == NAVMESH_MAGIC.as_slice() => {
                let version = u32::from_le_bytes([v[0], v[1], v[2], v[3]]);
                if version == NAVMESH_VERSION {
                    "refused: a length, value or index size is out of range".to_string()
                } else {
                    format!("refused: version {version}, this build reads version {NAVMESH_VERSION}")
                }
            }
            _ => "refused: not an NVM2 navmesh (bad magic or too short for a version)".to_string(),
        })
}

fn nav_cell_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let (layout, _) = nav_cell_world()
        .ok_or_else(|| format!("{}the fuzz floor did not build or cook", super::BUG_PREFIX))?;
    let mut cache = NavTileCache::streamed(nav_cell_params(), *layout).map_err(e)?;
    cache.load_cell(0, b).map_err(e)?;
    let _ = cache.update();
    Ok(Vec::new())
}

/// A painted scatter store with two classes over three storage tiles, put in through the stroke journal (the
/// editor's undo path), so the bytes are the ones `PaintedScatter::to_bytes` writes for a real paint.
fn scatter_paint_fixture() -> Vec<u8> {
    let instance = |id: u64, mesh: u16, position: [f32; 3]| PaintedInstance {
        id,
        mesh,
        flags: 0,
        position,
        normal: [0.0, 1.0, 0.0],
        yaw: 0.5,
        scale: 1.25,
        shade: 0.5,
        tilt: [0.25, 0.75],
    };
    let stroke = ScatterStroke {
        mode: ScatterBrushMode::Paint,
        added: vec![
            ScatterDelta {
                class: "rocks".into(),
                meshes: vec!["meshes/rock_a.mesh".into(), "meshes/rock_b.mesh".into()],
                instances: vec![
                    instance(1, 0, [10.0, 2.0, 20.0]),
                    instance(2, 1, [30.0, 1.5, 50.0]),
                    instance(3, 0, [-10.0, 0.5, 70.0]),
                ],
            },
            ScatterDelta {
                class: "shrubs".into(),
                meshes: vec!["meshes/shrub.mesh".into()],
                instances: vec![instance(7, 0, [100.0, 3.0, -20.0])],
            },
        ],
        removed: Vec::new(),
        brush: Brush::default(),
        dirty: DirtyRect {
            col_start: 0,
            col_end: 0,
            row_start: 0,
            row_end: 0,
        },
        tiles: BTreeSet::new(),
    };
    let mut store = PaintedScatter::new();
    stroke.restore(&mut store, false);
    store.to_bytes().unwrap_or_default()
}

/// A painted store goes past the decode: every class reports, every instance resolves its mesh, then the bytes
/// again.
fn scatter_paint_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let store = PaintedScatter::from_bytes(b).map_err(e)?;
    for class in store.report() {
        for instance in store.instances(&class.name) {
            if store.mesh_path(&class.name, instance).is_none() {
                return Err(format!(
                    "{}instance {} of `{}` has no mesh after a valid load",
                    super::BUG_PREFIX,
                    instance.id,
                    class.name
                ));
            }
        }
    }
    store.to_bytes().map_err(e)
}

/// RFC 7748 § 6.1's key pair (Alice), so the public key belongs to the private one.
fn server_key_fixture() -> String {
    concat!(
        "(\n",
        "    format_version: 1,\n",
        "    private_key: \"77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a\",\n",
        "    public_key: \"8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a\",\n",
        ")\n",
    )
    .into()
}

/// The late-frozen format targets.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        // regia_2d (specs/86).
        text(
            "autotile",
            Some("autotile"),
            regia_2d::AUTOTILE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/autotile.example.ron").into(),
            |s| {
                regia_2d::AutoTileSet::from_ron(s)
                    .and_then(|set| set.to_ron())
                    .map_err(e)
            },
        ),
        // Saved tile patterns (specs/86 § 6, ADR-7853).
        text(
            "tilepatterns",
            Some("tilepatterns"),
            regia_2d::tilepatterns::TILEPATTERNS_FORMAT_VERSION,
            true,
            || include_str!("fixtures/tilepatterns.example.ron").into(),
            |s| {
                regia_2d::tilepatterns::TilePatternLibrary::from_ron(s)
                    .and_then(|lib| lib.to_ron())
                    .map_err(e)
            },
        ),
        // Content chunks (spec 40 § 2.2, § 2.6).
        FuzzTarget {
            name: "chunk-why",
            format: Some("chunk-why"),
            format_version: regia_asset::content::CHUNK_WHY_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: chunk_why_fixture,
                load: chunk_why_load,
            },
        },
        text(
            "content-host",
            Some("content-host"),
            regia_asset::content::CONTENT_HOST_FORMAT_VERSION,
            true,
            || {
                ContentHost::new("https://cdn.example.com/game")
                    .to_ron_string()
                    .unwrap_or_default()
            },
            |s| {
                ContentHost::from_ron_str(s)
                    .and_then(|host| host.to_ron_string())
                    .map_err(e)
            },
        ),
        // The per-device install record (`content-state.ron`, ADR-7236): a cache the game reads
        // from the user data directory, so damaged bytes must parse or refuse, never panic.
        text(
            "content-state",
            Some("content-state"),
            regia_asset::content::CONTENT_STATE_FORMAT_VERSION,
            true,
            || {
                let mut state = regia_asset::content::ContentState::default();
                state.chunks.insert(
                    "arena".into(),
                    regia_asset::content::InstalledChunk {
                        build_id: "1.0-1-0123456789ab".into(),
                        hash: "0".repeat(64),
                        size: 4096,
                        modified: 1_700_000_000,
                    },
                );
                state.to_ron_string().unwrap_or_default()
            },
            |s| {
                regia_asset::content::ContentState::from_ron_str(s)
                    .and_then(|state| state.to_ron_string())
                    .map_err(e)
            },
        ),
        // Signed content manifests (ADR-2710): the trust record and the detached signature
        // envelope a host serves (untrusted bytes, parsed before any key is used).
        text(
            "content-trust",
            Some("content-trust"),
            regia_asset::content::CONTENT_TRUST_FORMAT_VERSION,
            true,
            || {
                ContentTrust::new(
                    SignaturePolicy::Required,
                    vec![
                        regia_asset::content::format_public_key(&[7; 32]),
                        regia_asset::content::format_public_key(&[9; 32]),
                    ],
                )
                .and_then(|t| t.to_ron_string())
                .unwrap_or_default()
            },
            |s| {
                ContentTrust::from_ron_str(s)
                    .and_then(|t| t.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "content-signature",
            Some("content-signature"),
            regia_asset::content::CONTENT_SIGNATURE_FORMAT_VERSION,
            true,
            || {
                ContentSignature {
                    format_version: regia_asset::content::CONTENT_SIGNATURE_FORMAT_VERSION,
                    algorithm: regia_asset::content::SIGNATURE_ALGORITHM.into(),
                    key_id: "0123456789abcdef".into(),
                    signature: "5a".repeat(64),
                }
                .to_ron_string()
                .unwrap_or_default()
            },
            |s| {
                ContentSignature::from_bytes(s.as_bytes())
                    .and_then(|sig| sig.to_ron_string())
                    .map_err(e)
            },
        ),
        // Demos and match records (ADR-1569, ADR-1878).
        text(
            "demo",
            Some("demo"),
            regia_framework::demo::DEMO_FORMAT_VERSION,
            true,
            || demo_value().to_ron().unwrap_or_default(),
            |s| DemoRecording::from_ron(s).and_then(|d| d.to_ron()).map_err(e),
        ),
        FuzzTarget {
            name: "frec",
            format: Some("frec"),
            format_version: regia_framework::input_recording::FREC_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: || encode_frec(&demo_value()).unwrap_or_default(),
                load: |b| decode_frec(b).and_then(|d| encode_frec(&d)).map_err(e),
            },
        },
        // A network recording (spec 103): the container, then every frame through the client path's walk. The
        // re-encoding is the file itself, with the header written again from what was read. Each input also goes through
        // with its checksums made right: a mutation would otherwise stop at the first one and never reach a decoder.
        FuzzTarget {
            name: "netdemo",
            format: Some("netdemo"),
            format_version: regia_net::recorder::format::NETDEMO_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: regia_net::recorder::format::fixture_netdemo,
                load: |b| {
                    use regia_net::recorder::format::{read_whole, resealed};
                    let _ = read_whole(&resealed(b));
                    read_whole(b).map_err(e)
                },
            },
        },
        // Voices, dialogue and subtitles (spec 36 §§ 7-8): parsers only, so the canonical text is serde's.
        text(
            "voice",
            Some("voice"),
            dialogue::VOICE_VERSION,
            true,
            voice_fixture,
            |s| {
                let voice = dialogue::parse_voice("fuzz.voice.ron", s).map_err(e)?;
                pretty(&voice)
            },
        ),
        text(
            "dialogue",
            Some("dialogue"),
            dialogue::DIALOGUE_VERSION,
            true,
            dialogue_fixture,
            |s| {
                let asset = dialogue::parse_dialogue("fuzz.dialogue.ron", s).map_err(e)?;
                for line in &asset.lines {
                    let _ = line.select(&line.speaker, &[]);
                }
                pretty(&asset)
            },
        ),
        text(
            "subtitles",
            Some("subtitles"),
            dialogue::SUBTITLES_VERSION,
            true,
            subtitles_fixture,
            |s| {
                let track = dialogue::parse_subtitles("fuzz.subs.ron", s).map_err(e)?;
                let _ = dialogue::expand_cues(&track.cues, 10_000_000);
                pretty(&track)
            },
        ),
        // The cooked extension record (spec 57 § 4.4).
        text(
            "extension.cooked",
            Some("extensions-ron"),
            regia_framework::extension::cooked::COOKED_FORMAT_VERSION,
            true,
            extensions_fixture,
            |s| CookedExtensions::parse(s).and_then(|c| c.to_ron()).map_err(e),
        ),
        // The loudness table (specs/95 § 2.4).
        text(
            "loudness-table",
            Some("loudness-table"),
            regia_audio::LOUDNESS_TABLE_VERSION,
            true,
            loudness_fixture,
            |s| {
                LoudnessTable::from_ron(s.as_bytes())
                    .and_then(|t| t.to_ron_string())
                    .map_err(e)
            },
        ),
        // Animation sources (specs 52 and 85).
        text(
            "motiondb",
            Some("motiondb"),
            regia_animation::motion_matching::MOTIONDB_FORMAT_VERSION,
            true,
            || include_str!("fixtures/motiondb.ron").into(),
            |s| {
                MotionDatabaseSource::from_ron_str(s)
                    .and_then(|m| m.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "retarget",
            Some("retarget"),
            regia_animation::RETARGET_FORMAT_VERSION,
            true,
            || include_str!("fixtures/retarget.ron").into(),
            |s| {
                RetargetSetup::from_ron_str(s)
                    .and_then(|r| r.to_ron_string())
                    .map_err(e)
            },
        ),
        // Navigation (spec 92, ADR-1934).
        text(
            "navlinks",
            Some("navlinks"),
            regia_nav::NAV_LINKS_FORMAT_VERSION,
            true,
            || include_str!("fixtures/navlinks.ron").into(),
            |s| {
                let links = NavLinkSet::from_ron_str(s).map_err(e)?;
                NavLinkSet::to_ron_string(&links).map_err(e)
            },
        ),
        text(
            "navlanes",
            Some("navlanes"),
            regia_nav::NAV_LANES_FORMAT_VERSION,
            true,
            || include_str!("fixtures/navlanes.ron").into(),
            // Parse, stamp the current version and serialize: a loader's output is this build's
            // canonical form of the file (parse alone keeps the file's own version number).
            |s| {
                {
                    NavLaneSet::parse(s).and_then(|mut set| {
                        set.format_version = regia_nav::NAV_LANES_FORMAT_VERSION;
                        set.to_ron()
                    })
                }
                .map_err(e)
            },
        ),
        // Runtime nav file loaders and scene link anchors (ADR-2420).
        FuzzTarget {
            name: "navlinks.file",
            format: None,
            format_version: regia_nav::NAV_LINKS_FORMAT_VERSION,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: || include_str!("fixtures/navlinks.ron").as_bytes().to_vec(),
                load: |b| {
                    let links = NavLinkSet::from_bytes(b).map_err(e)?;
                    NavLinkSet::to_ron_string(&links)
                        .map(String::into_bytes)
                        .map_err(e)
                },
            },
        },
        FuzzTarget {
            name: "navlanes.file",
            format: None,
            format_version: regia_nav::NAV_LANES_FORMAT_VERSION,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: || include_str!("fixtures/navlanes.ron").as_bytes().to_vec(),
                load: |b| {
                    NavLaneSet::from_bytes(b)
                        .and_then(|set| set.to_ron())
                        .map(String::into_bytes)
                        .map_err(e)
                },
            },
        },
        text(
            "nav-link-anchor",
            Some("nav-link-anchor"),
            regia_nav::NAV_LINK_ANCHOR_FORMAT_VERSION,
            true,
            || {
                reflected_fixture(
                    nav_anchor_register,
                    &regia_nav::NavLinkAnchor::new(Vec3::new(0.0, 2.0, -3.0)),
                )
            },
            |s| {
                reflected_load::<regia_nav::NavLinkAnchor>(
                    s,
                    nav_anchor_register,
                    regia_nav::NavLinkAnchor::migrate,
                )
            },
        ),
        text(
            "traversal-settings",
            Some("traversal-settings"),
            regia_framework::traversal::TRAVERSAL_SETTINGS_FORMAT_VERSION,
            true,
            || {
                reflected_fixture(
                    traversal_settings_register,
                    &regia_framework::traversal::TraversalSettings::default(),
                )
            },
            |s| {
                reflected_load::<regia_framework::traversal::TraversalSettings>(
                    s,
                    traversal_settings_register,
                    regia_framework::traversal::TraversalSettings::migrate,
                )
            },
        ),
        text(
            "traversal-ladder",
            Some("traversal-ladder"),
            regia_framework::traversal::TRAVERSAL_LADDER_FORMAT_VERSION,
            true,
            || {
                reflected_fixture(
                    traversal_ladder_register,
                    &regia_framework::traversal::Ladder::default(),
                )
            },
            |s| {
                reflected_load::<regia_framework::traversal::Ladder>(
                    s,
                    traversal_ladder_register,
                    regia_framework::traversal::Ladder::migrate,
                )
            },
        ),
        text(
            "traversal-jump-point",
            Some("traversal-jump-point"),
            regia_framework::traversal::TRAVERSAL_JUMP_POINT_FORMAT_VERSION,
            true,
            || {
                reflected_fixture(
                    traversal_jump_point_register,
                    &regia_framework::traversal::TraversalJumpPoint::default(),
                )
            },
            |s| {
                reflected_load::<regia_framework::traversal::TraversalJumpPoint>(
                    s,
                    traversal_jump_point_register,
                    regia_framework::traversal::TraversalJumpPoint::migrate,
                )
            },
        ),
        // Logic authored on prefab and level entities (P-165, ADR-8566).
        text(
            "logic-graph-ref",
            Some("logic-graph-ref"),
            regia_ai::logic::LOGIC_GRAPH_REF_FORMAT_VERSION,
            true,
            || reflected_fixture(logic_graph_ref_register, &logic_graph_ref_fixture()),
            |s| {
                reflected_load::<regia_ai::logic::LogicGraphRef>(
                    s,
                    logic_graph_ref_register,
                    regia_ai::logic::LogicGraphRef::migrate,
                )
            },
        ),
        FuzzTarget {
            name: "nav.cell",
            format: Some("nav-cell"),
            format_version: regia_nav::NAV_CELL_FORMAT_VERSION,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: || nav_cell_world().map(|(_, blob)| blob.clone()).unwrap_or_default(),
                load: nav_cell_load,
            },
        },
        // The deterministic navmesh (`<map>.detnav`, `NVM2`, ADR-8174): magic, version word, polygons
        // with their links; `from_bytes` must never panic.
        FuzzTarget {
            name: "nav.det_navmesh",
            format: Some(regia_nav::det::navmesh::NAVMESH_FORMAT),
            format_version: regia_nav::det::navmesh::NAVMESH_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: det_navmesh_fixture,
                load: det_navmesh_load,
            },
        },
        // Painted scatter (`<map>.scatterpaint.bin`, spec 82 § 13): magic, version word, postcard body.
        FuzzTarget {
            name: "terrain.scatter_paint",
            format: Some("scatter-paint"),
            format_version: regia_terrain::SCATTER_PAINT_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: scatter_paint_fixture,
                load: scatter_paint_load,
            },
        },
        // The dedicated server's key file (ADR-1901, frozen by ADR-2316): no public writer, so no round trip.
        text(
            "net.server_key",
            Some("server-key"),
            regia_net::crypto::SERVER_KEY_FORMAT_VERSION,
            false,
            server_key_fixture,
            |s| ServerIdentity::from_file_text(s).map(|_| String::new()),
        ),
        // The project's weather (`config/weather.ron`, spec 99 § 7.1): load, validate, write back.
        text(
            "weather",
            Some("weather"),
            regia_vfx::weather::WEATHER_CONFIG_FORMAT_VERSION,
            true,
            || include_str!("fixtures/weather.ron").into(),
            |s| {
                regia_vfx::weather::WeatherConfig::from_ron(s)
                    .and_then(|config| config.to_ron())
                    .map_err(e)
            },
        ),
        // Cloth and soft-body sources (`docs/formats/cloth-ron.md`, spec 55 § 4).
        text(
            "cloth",
            Some("cloth"),
            regia_physics::cloth::asset::CLOTH_FORMAT_VERSION,
            true,
            || include_str!("fixtures/cloth.ron").into(),
            |s| {
                regia_physics::cloth::asset::ClothSource::from_ron_str(s)
                    .and_then(|source| source.to_ron_string())
                    .map_err(e)
            },
        ),
        // Entity kind declarations (`.kind.ron`, `docs/formats/kind-ron.md`, spec 101 § 2.3): parse
        // and validate, write back.
        text(
            "kind",
            Some("kind"),
            regia_scene::kind::file::KIND_FORMAT_VERSION,
            true,
            || include_str!("fixtures/kind.ron").into(),
            |s| {
                regia_scene::kind::file::KindFile::from_ron_str(s)
                    .and_then(|file| file.to_ron_string())
                    .map_err(e)
            },
        ),
        // Per-class delivery contract (`.contract.ron`, docs/formats/contract-ron.md, ADR-8401).
        text(
            "delivery-contract",
            Some("delivery-contract"),
            regia_asset::delivery_contract::DELIVERY_CONTRACT_FORMAT_VERSION,
            true,
            || include_str!("fixtures/contract.ron").into(),
            |s| {
                regia_asset::delivery_contract::DeliveryContract::from_ron_str(s)
                    .and_then(|file| file.to_ron_string())
                    .map_err(e)
            },
        ),
        text(
            "soft",
            Some("soft"),
            regia_physics::cloth::asset::SOFT_FORMAT_VERSION,
            true,
            || include_str!("fixtures/soft.ron").into(),
            |s| {
                let source = regia_physics::cloth::asset::SoftSource::from_ron_str(s).map_err(e)?;
                pretty(&source)
            },
        ),
        // The cooked payloads, without the `FCKD` wrapper the `cooked` target fuzzes: the fixtures of
        // `regia_physics/tests/cloth_formats.rs` (copies: a crate may not read outside its folder).
        FuzzTarget {
            name: "cooked-cloth",
            format: Some("cooked-cloth"),
            format_version: regia_physics::cloth::asset::COOKED_CLOTH_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: || cooked_payload(include_bytes!("fixtures/flag-v1.cloth.bin")),
                load: |b| {
                    regia_physics::cloth::ClothAsset::from_bytes(b)
                        .and_then(|asset| asset.to_bytes())
                        .map_err(e)
                },
            },
        },
        FuzzTarget {
            name: "cooked-soft",
            format: Some("cooked-soft"),
            format_version: regia_physics::cloth::asset::COOKED_SOFT_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: || cooked_payload(include_bytes!("fixtures/ball-v1.soft.bin")),
                load: |b| {
                    regia_physics::cloth::SoftAsset::from_bytes(b)
                        .and_then(|asset| asset.to_bytes())
                        .map_err(e)
                },
            },
        },
        // The cooked permutation list (`content/permutations.json`, ADR-2026): derived data, not frozen, but a
        // package's copy is read at startup.
        text(
            "render.permutations",
            Some("permutations"),
            regia_render::permutation_policy::PERMUTATION_MANIFEST_FORMAT_VERSION,
            true,
            || include_str!("fixtures/permutations.json").into(),
            |s| {
                regia_render::PermutationManifest::from_json(s)
                    .map_err(e)?
                    .to_json()
                    .map_err(e)
            },
        ),
        // Authored collision proxies of a glTF (the `collision` sub-asset payload, ADR-2341).
        FuzzTarget {
            name: "collision-set",
            format: Some("collision-set"),
            format_version: regia_asset::COLLISION_SET_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: collision_set_fixture,
                load: |b| {
                    regia_asset::CookedCollisionSet::from_postcard(b)
                        .map_err(e)?
                        .to_postcard()
                        .map_err(e)
                },
            },
        },
        // The instanced nodes of a glTF (the `prefab/instances` sub-asset payload, ADR-3016).
        FuzzTarget {
            name: "prefab-instances",
            format: Some("prefab-instances"),
            format_version: regia_asset::PREFAB_INSTANCES_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: prefab_instances_fixture,
                load: |b| {
                    regia_asset::PrefabInstanceSet::from_postcard(b)
                        .map_err(e)?
                        .to_postcard()
                        .map_err(e)
                },
            },
        },
        // Vehicle configurations (`docs/formats/vehicle.md`, ADR-2301): load, validate, write back.
        text(
            "vehicle",
            Some("vehicle"),
            regia_physics::vehicle_asset::VEHICLE_FORMAT_VERSION,
            true,
            || include_str!("fixtures/vehicle.ron").into(),
            |s| {
                let asset = regia_physics::vehicle_asset::VehicleAsset::from_ron(s).map_err(e)?;
                asset.validate().map_err(e)?;
                asset.to_ron().map_err(e)
            },
        ),
        // A saved vehicle (`docs/formats/vehicle-snapshot.md`, ADR-3080): load, write back. The fixture is a car
        // the format's own code saves, so it always matches the current layout.
        text(
            "vehicle-snapshot",
            Some("vehicle-snapshot"),
            regia_physics::vehicle_snapshot::VEHICLE_SNAPSHOT_FORMAT_VERSION,
            true,
            vehicle_snapshot_fixture,
            |s| {
                regia_physics::vehicle_snapshot::VehicleSnapshot::from_ron(s)
                    .and_then(|snapshot| snapshot.to_ron())
                    .map_err(e)
            },
        ),
        // Physics materials (`docs/formats/physmat.md`, ADR-2424): load, validate, write back.
        text(
            "physmat",
            Some("physmat"),
            regia_physics::material::PHYSMAT_FORMAT_VERSION,
            true,
            || include_str!("fixtures/physmat.ron").into(),
            |s| {
                let material = regia_physics::material::PhysicsMaterial::from_ron(s).map_err(e)?;
                material.validate().map_err(e)?;
                material.to_ron().map_err(e)
            },
        ),
        // The cooked convex hull set payload (`FCHS`, `docs/formats/hull-set.md`, ADR-2301), without the
        // `FCKD` wrapper the `cooked` target fuzzes; the fixture is two hulls the format's own code writes.
        FuzzTarget {
            name: "hull-set",
            format: Some("hull-set"),
            format_version: regia_physics::hull_set::HULL_SET_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: hull_set_fixture,
                load: |b| {
                    let set = regia_physics::hull_set::HullSet::from_bytes(b).map_err(e)?;
                    let _ = set.to_collider_shape();
                    Ok(set.to_bytes())
                },
            },
        },
        // The cooked motion database payload (`motiondb-cooked`, spec 52 § 4, ADR-2409), without the `FCKD`
        // wrapper; the fixture is the procedural locomotion database as the cook writes it.
        FuzzTarget {
            name: "motiondb-cooked",
            format: Some("motiondb-cooked"),
            format_version: regia_animation::motion_matching::COOKED_MOTIONDB_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: motiondb_cooked_fixture,
                load: |b| {
                    regia_animation::motion_matching::MotionDatabase::from_cooked_payload(b)
                        .and_then(|d| d.to_cooked_payload())
                        .map_err(e)
                },
            },
        },
        // Fixed-wing aircraft (`docs/formats/aircraft-ron.md`, ADR-3374): load, validate, write back.
        text(
            "aircraft",
            Some("aircraft"),
            regia_physics::aero::asset::AIRCRAFT_FORMAT_VERSION,
            true,
            || include_str!("fixtures/aircraft.ron").into(),
            |s| {
                let asset = regia_physics::aero::asset::AircraftAsset::from_ron(s).map_err(e)?;
                asset.validate().map_err(e)?;
                asset.to_ron().map_err(e)
            },
        ),
        // Vehicle sound definitions (`docs/formats/vehicle-audio.md`, ADR-3271): load, validate, write back.
        text(
            "vehicle-audio",
            Some("vehicle-audio"),
            regia_audio::VEHICLE_AUDIO_VERSION,
            true,
            || include_str!("fixtures/vehicle-audio.ron").into(),
            |s| {
                let def = regia_audio::VehicleAudioDef::from_ron(s).map_err(e)?;
                def.validate().map_err(e)?;
                def.to_ron().map_err(e)
            },
        ),
        // The project bus tree `config/audio.ron` (`docs/formats/audio-buses-ron.md`, ADR-8354): load
        // (v0 migrates), validate, write back.
        text(
            "audio-buses",
            Some("audio-buses"),
            regia_audio::AUDIO_BUSES_FORMAT_VERSION,
            true,
            || include_str!("fixtures/audio-buses.ron").into(),
            |s| regia_audio::BusLayout::parse(s)?.to_ron(),
        ),
        // Look-review shot lists (ADR-2463): the validated loader and canonical writer.
        text(
            "shot",
            Some("shot"),
            crate::shot_list::SHOT_LIST_FORMAT_VERSION,
            true,
            || include_str!("fixtures/shot.example.ron").into(),
            |s| {
                crate::shot_list::ShotList::from_ron(s)
                    .and_then(|list| list.to_ron())
                    .map_err(e)
            },
        ),
    ]
}

/// A collision set with one shape of every kind (postcard payload).
fn prefab_instances_fixture() -> Vec<u8> {
    regia_asset::PrefabInstanceSet::new(vec![regia_asset::PrefabInstancedNode {
        node: "trees".to_string(),
        parts: vec![regia_asset::AssetId::default()],
        transforms: vec![
            [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 2.0, 0.0, 0.0, 1.0,
            ];
            2
        ],
    }])
    .to_postcard()
    .unwrap_or_default()
}

fn collision_set_fixture() -> Vec<u8> {
    use regia_asset::collision_set::{
        CollisionAxis, CollisionBox, CollisionCapsule, CollisionConvex, CollisionSphere, CollisionTriMesh,
    };
    use regia_asset::{CookedCollisionGeometry as G, CookedCollisionShape};
    let shape = |node: &str, geometry: G| CookedCollisionShape {
        node: node.into(),
        requested: geometry.kind(),
        translation: [0.0, 0.5, 0.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        geometry,
    };
    regia_asset::CookedCollisionSet::new(vec![
        shape(
            "collision_box_a",
            G::Box(CollisionBox {
                half_extents: [0.5, 0.5, 0.5],
            }),
        ),
        shape("collision_sphere_b", G::Sphere(CollisionSphere { radius: 0.25 })),
        shape(
            "collision_capsule_c",
            G::Capsule(CollisionCapsule {
                half_height: 0.5,
                radius: 0.1,
                axis: CollisionAxis::Z,
            }),
        ),
        shape(
            "collision_convex_d",
            G::Convex(CollisionConvex {
                points: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            }),
        ),
        shape(
            "collision_mesh_e",
            G::TriMesh(CollisionTriMesh {
                vertices: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
                indices: vec![[0, 2, 1]],
            }),
        ),
    ])
    .to_postcard()
    .unwrap_or_default()
}

/// Two hulls (a tetrahedron and a unit cube's corners), as the importer's hull cook writes them.
fn hull_set_fixture() -> Vec<u8> {
    let tetra = vec![
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(1.0, 0.0, 0.0),
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.0, 0.0, 1.0),
    ];
    let cube = (0..8u8)
        .map(|i| {
            Vec3::new(
                2.0 + f32::from(i & 1),
                f32::from((i >> 1) & 1),
                f32::from((i >> 2) & 1),
            )
        })
        .collect();
    regia_physics::hull_set::HullSet {
        hulls: vec![tetra, cube],
    }
    .to_bytes()
}

/// The payload of a cooked fixture, or nothing (which fails the fixture test) if its wrapper broke.
fn cooked_payload(file: &[u8]) -> Vec<u8> {
    regia_asset::decode_cooked_file(file)
        .map(|(_, payload)| payload.to_vec())
        .unwrap_or_default()
}

/// The procedural locomotion database's cooked payload (empty, which fails the fixture test, if it
/// stopped building).
fn motiondb_cooked_fixture() -> Vec<u8> {
    regia_animation::motion_matching::procedural::procedural_locomotion()
        .build()
        .and_then(|d| d.to_cooked_payload())
        .unwrap_or_default()
}

/// A geared car's snapshot on flat ground, after a moment of throttle.
fn vehicle_snapshot_fixture() -> String {
    use regia_core::math::{Quat, Vec3};
    use regia_physics::vehicle::{VehicleConfig, VehicleInput};
    use regia_physics::vehicle_drivetrain::{Drivetrain, DrivetrainConfig};
    use regia_physics::vehicle_snapshot::{VehicleSimSettings, VehicleSimWorld, add_static_box};
    const DT: f32 = 1.0 / 60.0;
    let mut shared = regia_physics::PhysicsWorld::default();
    add_static_box(
        &mut shared,
        Vec3::new(0.0, -1.0, 0.0),
        Vec3::new(100.0, 1.0, 100.0),
    );
    let config = VehicleConfig {
        drivetrain: Drivetrain::Geared(DrivetrainConfig::default()),
        ..VehicleConfig::default()
    };
    let pose = (Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY);
    let Ok(mut sim) = VehicleSimWorld::new(config, &shared, VehicleSimSettings::default(), pose, DT) else {
        return String::new();
    };
    for _ in 0..30 {
        let input = VehicleInput {
            throttle: 1.0,
            ..VehicleInput::default()
        };
        if sim.step(input, DT, None).is_err() {
            return String::new();
        }
    }
    sim.snapshot()
        .ok()
        .and_then(|snapshot| snapshot.to_ron().ok())
        .unwrap_or_default()
}
