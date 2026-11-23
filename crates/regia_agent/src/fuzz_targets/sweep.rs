// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for the formats the format-registry sweep froze (ADR-2316): the engine's lobby keys, UI
//! animations (`.uianim.ron`), the asset registry file, cooked conversations (`.convc`), behavior trees
//! (`.bt.ron`) and painted terrain layer weights (`<map>.weights.bin`). The other formats of the sweep reuse
//! targets that existed before it (`net.*`, `input.overrides`, `asset.tag_rules`, `render.permutations`,
//! `nav.cell`, `data-table`) or are fuzzed in their own crate (XR, validation rules, the CLI's session and
//! release files; `COVERED_ELSEWHERE` in `tests/fuzz_targets.rs`).

use std::collections::BTreeMap;
use std::fmt::Display;

use regia_ai::{
    AbortMode, BehaviorTree, BehaviorTreeDef, BtCompareOp, BtLiteral, BtTaskRegistry, BtValueKind, DefKind,
    DefNode,
};
use regia_asset::AssetId;
use regia_asset::registry::{AssetRegistry, RegistryEntry};
use regia_core::id::Guid;
use regia_framework::conversation::asset as conversation;
use regia_framework::conversation::cooked::{CONVC_VERSION, CookedClip, CookedConversation};
use regia_net::lobby::{self, LOBBY_KEYS_VERSION, LobbyServer, keys};
use regia_terrain::LayerWeightMap;
use regia_ui::anim_timeline::UiAnimLibrary;

use super::formats::{pretty, text};
use super::{FuzzInput, FuzzTarget};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

/// A new lobby's engine keys, the rotation keys (layout v2, ADR-8897), a launched server and a ready
/// member.
fn lobby_keys_fixture() -> String {
    let mut data = lobby::engine_keys("arena", 0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210);
    data.extend(lobby::rotation_keys("ctf", "public-ctf"));
    data.insert(
        keys::SERVER.into(),
        LobbyServer::Steam(76_561_198_000_000_001).to_key(),
    );
    data.insert(keys::READY.into(), "1".into());
    pretty(&data).unwrap_or_default()
}

/// What a joining client does with a lobby's data: every key and value within the platform limits, then
/// each engine key read as the layout says (version, 16-digit hashes, state, server).
fn lobby_keys_load(s: &str) -> Result<String, String> {
    let data: BTreeMap<String, String> = ron::from_str(s).map_err(e)?;
    for (key, value) in &data {
        lobby::check_key(key).map_err(e)?;
        lobby::check_value(value).map_err(e)?;
        match key.as_str() {
            keys::VERSION => {
                let version: u32 = value.parse().map_err(e)?;
                if version == 0 || version > LOBBY_KEYS_VERSION {
                    return Err(format!(
                        "lobby key layout {version} (this build reads {LOBBY_KEYS_VERSION})"
                    ));
                }
            }
            keys::PROTOCOL | keys::CONTENT => {
                if value.len() != 16
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(format!("{key} is not 16 lowercase hex digits"));
                }
            }
            keys::STATE if !keys::STATES.contains(&value.as_str()) => {
                return Err(format!("unknown lobby state {value}"));
            }
            keys::SERVER if LobbyServer::from_key(value).is_none() => {
                return Err(format!("{key} is not udp:<addr> or steam:<id>"));
            }
            _ => {}
        }
    }
    pretty(&data)
}

/// The example of `book/agents/ui-timeline.md` plus a looping colour track.
fn uianim_fixture() -> String {
    r#"(
    format_version: 1,
    animations: {
        "pop": (
            tracks: [
                (property: "opacity", keys: [(time: 0.0, value: 0.0), (time: 0.2, value: 1.0, ease: "quad_out")]),
                (property: "scale", keys: [(time: 0.0, value: 0.9), (time: 0.2, value: 1.0, ease: "back_out")]),
            ],
        ),
        "blink": (
            tracks: [(property: "opacity", keys: [(time: 0.0, value: 1.0), (time: 0.5, value: 0.2)])],
            mode: ping_pong,
            loops: 3,
            speed: 2.0,
        ),
    },
)"#
    .into()
}

fn uianim_load(s: &str) -> Result<String, String> {
    UiAnimLibrary::from_ron(s).map_err(e)?.to_ron_string().map_err(e)
}

/// A data asset type for the `.data.ron` target (ADR-7232).
mod data_asset_type {
    use regia_app::prelude::*;

    #[derive(Reflect, Default, Debug, PartialEq)]
    #[reflect(Default)]
    pub(super) struct FuzzDataAsset {
        name: String,
        count: u32,
        weights: Vec<f32>,
        tag: Option<String>,
    }

    impl regia_asset::data_asset::Asset for FuzzDataAsset {}
}
use data_asset_type::FuzzDataAsset;

fn data_asset_fixture() -> String {
    format!(
        "DataAsset(\n    format_version: 1,\n    type: \"{}\",\n    value: (\n        name: \"rifle\",\n        count: 3,\n        weights: [0.5, 1.0, 2.25],\n        tag: Some(\"long\"),\n    ),\n)\n",
        <FuzzDataAsset as regia_ecs::reflect::TypePath>::type_path()
    )
}

fn data_asset_load(s: &str) -> Result<String, String> {
    use regia_asset::data_asset::{DataAssetFile, ReflectAsset};
    let mut registry = regia_ecs::reflect::TypeRegistry::default();
    registry.register::<FuzzDataAsset>();
    registry.register_type_data::<FuzzDataAsset, ReflectAsset>();
    let file = DataAssetFile::parse(s, &registry).map_err(e)?;
    DataAssetFile::to_ron(file.value.as_partial_reflect(), &registry).map_err(e)
}

/// A cooked cache's registry: a level with a dependency, tagged.
fn registry_fixture() -> Vec<u8> {
    let id = |n: u128| AssetId(Guid::from_u128(n));
    let entry = |n: u128, path: &str, deps: Vec<AssetId>| RegistryEntry {
        id: id(n),
        path: path.into(),
        type_path: "regia_scene::Level".into(),
        dependencies: deps,
        tags: BTreeMap::from([("biome".to_owned(), "desert".to_owned())]),
        size: 4096,
    };
    AssetRegistry::from_entries([
        entry(1, "levels/arena.level.ron", vec![id(2)]),
        entry(2, "meshes/rock.mesh", Vec::new()),
    ])
    .to_bytes()
    .unwrap_or_default()
}

/// A registry goes past the decode: every record's dependencies and referencers are walked (the
/// `assets.registry_*` path).
fn registry_load(b: &[u8]) -> Result<Vec<u8>, String> {
    let registry = AssetRegistry::from_bytes(b).map_err(e)?;
    for entry in registry.entries() {
        let _ = registry.dependencies(entry.id, true).entries.len();
        let _ = registry.referencers(entry.id, true).entries.len();
    }
    registry.to_bytes().map_err(e)
}

/// The fixture conversation, cooked with one clip in one locale.
fn convc_fixture() -> Vec<u8> {
    let Ok(asset) =
        conversation::parse_conversation("fuzz.conv.ron", include_str!("fixtures/conversation.ron"))
    else {
        return Vec::new();
    };
    let clip = CookedClip {
        clip: Some("audio/vo/en/l1.ogg".into()),
        duration_us: Some(1_250_000),
        fallback: false,
        clip_locale: Some("en".into()),
    };
    CookedConversation {
        format_version: CONVC_VERSION,
        asset,
        clips: BTreeMap::from([("en".to_owned(), BTreeMap::from([("l1".to_owned(), clip)]))]),
    }
    .to_bytes()
    .unwrap_or_default()
}

fn convc_load(b: &[u8]) -> Result<Vec<u8>, String> {
    CookedConversation::from_bytes(b)
        .and_then(|c| c.to_bytes())
        .map_err(e)
}

/// A selector over a condition, an observed value comparison and a cooldown-decorated leaf.
fn bt_fixture() -> String {
    let node = |name: &str, kind: DefKind| DefNode {
        name: name.into(),
        kind,
        ..DefNode::default()
    };
    let def = BehaviorTreeDef {
        format_version: regia_ai::tree::BT_FORMAT_VERSION,
        root: 0,
        nodes: vec![
            DefNode {
                children: vec![1, 4, 2],
                ..node("root", DefKind::Selector)
            },
            DefNode {
                key: "enemy_visible".into(),
                ..node("sees", DefKind::Condition)
            },
            DefNode {
                children: vec![3],
                seconds: 2.0,
                ..node("rest", DefKind::Cooldown)
            },
            DefNode {
                task: "game::Patrol".into(),
                ..node("patrol", DefKind::Leaf)
            },
            DefNode {
                key: "threat".into(),
                abort: AbortMode::Both,
                op: BtCompareOp::InRange,
                value_kind: BtValueKind::Float,
                literal: Some(BtLiteral::Float(0.2)),
                literal_hi: Some(BtLiteral::Float(0.8)),
                ..node("threat_band", DefKind::Compare)
            },
        ],
    };
    pretty(&def).unwrap_or_default()
}

/// A tree is compiled against an empty task registry: the graph checks (bounds, cycles, arity) run on
/// every input; unknown leaf tasks are then a rejection, not an error of the fuzzer.
fn bt_load(s: &str) -> Result<String, String> {
    // Through the v1 -> v2 migrator first, as `regia upgrade-project` does: the loaded file is
    // at the current version.
    let migrated = regia_ai::bt_edit::migrate_bt_ron(s)?;
    let def = regia_ai::bt_edit::parse_bt_ron(&migrated)?;
    let _ = BehaviorTree::compile(&def, &BtTaskRegistry::default());
    pretty(&def)
}

/// A 4×4 map with two layers, partly painted.
fn weights_fixture() -> Vec<u8> {
    let Ok(mut map) = LayerWeightMap::new(4, 4, 2) else {
        return Vec::new();
    };
    map.set(0, 1, 1, 200);
    map.set(1, 2, 3, 64);
    map.to_bytes().unwrap_or_default()
}

fn weights_load(b: &[u8]) -> Result<Vec<u8>, String> {
    LayerWeightMap::from_bytes(b)
        .and_then(|m| m.to_bytes())
        .map_err(e)
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        text(
            "net.lobby_keys",
            Some("lobby-keys"),
            LOBBY_KEYS_VERSION,
            true,
            lobby_keys_fixture,
            lobby_keys_load,
        ),
        text(
            "uianim",
            Some("uianim"),
            regia_ui::anim_timeline::UIANIM_FORMAT_VERSION,
            true,
            uianim_fixture,
            uianim_load,
        ),
        // `.uianim.ron` v2 frame events (ADR-8900), so mutations reach the `events` list (the
        // `uianim` fixture stays the v1 example; this one is the corpus `uianim/v2.ron`).
        text(
            "uianim-events",
            Some("uianim"),
            regia_ui::anim_timeline::UIANIM_FORMAT_VERSION,
            true,
            || include_str!("../../tests/fixtures/format-corpus/uianim/v2.ron").into(),
            uianim_load,
        ),
        text(
            "data-asset",
            Some("data-asset"),
            regia_asset::data_asset::DATA_ASSET_FORMAT_VERSION,
            true,
            data_asset_fixture,
            data_asset_load,
        ),
        FuzzTarget {
            name: "asset-registry",
            format: Some("asset-registry"),
            format_version: regia_asset::registry::REGISTRY_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: registry_fixture,
                load: registry_load,
            },
        },
        FuzzTarget {
            name: "convc",
            format: Some("convc"),
            format_version: CONVC_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: convc_fixture,
                load: convc_load,
            },
        },
        text(
            "bt",
            Some("bt"),
            regia_ai::tree::BT_FORMAT_VERSION,
            true,
            bt_fixture,
            bt_load,
        ),
        FuzzTarget {
            name: "terrain.weights",
            format: Some("terrain-weights"),
            format_version: regia_terrain::LAYER_WEIGHTS_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: weights_fixture,
                load: weights_load,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt_target_checks_the_format_version_and_exercises_v2_fields() {
        let fixture = bt_fixture();
        let mut def: BehaviorTreeDef = ron::from_str(&fixture).expect("fixture");
        assert_eq!(def.format_version, regia_ai::tree::BT_FORMAT_VERSION);
        assert!(def.nodes.iter().any(|node| {
            node.kind == DefKind::Compare && node.abort == AbortMode::Both && node.literal_hi.is_some()
        }));
        assert!(bt_load(&fixture).is_ok());
        def.format_version = u32::MAX;
        let future = pretty(&def).expect("future fixture");
        assert!(bt_load(&future).is_err(), "future versions must be rejected");
    }
}
