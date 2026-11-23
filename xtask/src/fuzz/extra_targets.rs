// SPDX-License-Identifier: MPL-2.0
//! Targets the guided fuzzer runs beyond `regia_agent::fuzz_targets::targets()` (ADR-2723), shared by source (`#[path]`)
//! with `xtask`, which lists, seeds and replays them on stable. Two kinds:
//!
//! - The `FuzzTargetExtensions` a crate above `regia_agent` adds and `xtask` can link: `lua.luamod`, `lua.luastate`,
//!   `import.gltf`, `import.obj`, `import.usd` and `import.usdz` (ADR-3235). The editor's `import.*` wrap `regia_asset_pipeline::fuzz_entry`, called directly here;
//!   `editor.prefs`, `editor.keymap` and `editor.layout` stay out, because `regia_editor` is not in `xtask`'s graph.
//! - The frozen formats the registry leaves to per-crate tests because their fixtures have several parts (spec 97
//!   § 2.3), the loaders `docs/06` § 1 names: `pak.read`, `cooked.mesh`, `cooked.clip`, `save.payload`. They are
//!   `round_trip: false`: the loaders have no writer to compare with.

use regia_agent::fuzz_targets::{FuzzInput, FuzzTarget};
use regia_animation::AnimationClip;
use regia_animation::clip::{JointTrack, TrackCurve};
use regia_animation::cooked::{clip_type_tag, load_cooked_clip};
use regia_app::prelude::World;
use regia_app::prelude::bevy_ecs::reflect::AppTypeRegistry;
use regia_asset::pak::{FPAK_VERSION, PakEntry, PakToc, encode_pak_header, flags};
use regia_asset::{
    AssetId, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CookedIndexWidth, CookedMesh, ManifestEntry,
    PakReader, encode_cooked_file, load_mesh_bytes, pack_importer_version,
};
use regia_asset_pipeline::fuzz_entry;
use regia_core::id::TypePathHash;
use regia_core::math::Vec3;
use regia_framework::abilities::AbilityCounters;
use regia_framework::save_flow::{SaveBlobs, SaveInfo};
use regia_framework::save_game::{SaveFileRaw, SaveGame, SaveSpec, Saveable, SaveableResources};
use regia_scene::Transform;

/// The fixed asset id of every fixture (a run must repeat).
fn fixture_id() -> AssetId {
    AssetId::sub_asset(AssetId::NIL, "regia-fuzz")
}

/// A fixture that cannot be built is an engine bug the fixture check reports as an empty input, which every loader
/// rejects (`cargo xtask fuzz seed` and the unit tests read the fixtures back through their loaders).
fn or_empty<E>(built: Result<Vec<u8>, E>) -> Vec<u8> {
    built.unwrap_or_default()
}

/// An FPAK with a raw entry and a literal-only LZ4 block entry, so mutations reach both read paths.
fn pak_fixture() -> Vec<u8> {
    let raw_id = AssetId::sub_asset(AssetId::NIL, "fuzz/raw");
    let lz4_id = AssetId::sub_asset(AssetId::NIL, "fuzz/lz4");
    let raw: Vec<u8> = (0u8..48).collect();
    // The size prefix, then one LZ4 sequence: a token of 12 literals and no match.
    let packed: Vec<u8> = (0u8..12).collect();
    let mut block = 12u32.to_le_bytes().to_vec();
    block.push(12 << 4);
    block.extend_from_slice(&packed);

    let mut data = vec![0u8; 60];
    let raw_offset = data.len() as u64;
    data.extend_from_slice(&raw);
    let lz4_offset = data.len() as u64;
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&block);
    let toc = PakToc {
        entries: vec![
            PakEntry {
                id: raw_id,
                type_tag: TypePathHash(1),
                offset: raw_offset,
                size: raw.len() as u64,
                compressed_size: raw.len() as u64,
                block_table_offset: 0,
            },
            PakEntry {
                id: lz4_id,
                type_tag: TypePathHash(2),
                offset: lz4_offset,
                size: packed.len() as u64,
                compressed_size: (4 + block.len()) as u64,
                block_table_offset: lz4_offset,
            },
        ],
        manifest: vec![ManifestEntry {
            path: "fuzz/raw.bin".into(),
            id: raw_id,
            type_path: "fuzz::Raw".into(),
        }],
        dependencies: vec!["base.fpak".into()],
    };
    let toc_offset = data.len() as u64;
    let Ok(toc_bytes) = postcard::to_allocvec(&toc) else {
        return Vec::new();
    };
    data.extend_from_slice(&toc_bytes);
    let hash = *blake3::hash(&data[..data.len() - toc_bytes.len()]).as_bytes();
    let header = encode_pak_header(
        FPAK_VERSION,
        flags::COMPRESSED,
        toc_offset,
        toc_bytes.len() as u64,
        &hash,
    );
    data[..60].copy_from_slice(&header);
    data
}

/// Opens a pak and reads every entry through each read path.
fn pak_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let reader =
        PakReader::from_bytes(bytes.to_vec(), "fuzz.fpak", bytes.len() as u64).map_err(|e| e.to_string())?;
    let ids: Vec<AssetId> = reader.toc.entries.iter().map(|e| e.id).collect();
    for id in ids {
        let _ = reader.read_decompressed(id);
        let _ = reader.prefetch(id);
        let _ = reader.read_raw(id).map(|bytes| bytes.len());
        let _ = reader.contains(id);
    }
    let _ = reader.manifest().len();
    Ok(Vec::new())
}

/// `payload` in the cooked envelope.
fn cooked(type_tag: TypePathHash, payload: &[u8]) -> Vec<u8> {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag,
        asset_id: fixture_id(),
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    or_empty(encode_cooked_file(&header, payload))
}

/// A cooked one-triangle mesh.
fn mesh_fixture() -> Vec<u8> {
    let mesh = CookedMesh {
        aabb_min: [0.0; 3],
        aabb_max: [1.0; 3],
        bounding_sphere: [0.5, 0.5, 0.5, 1.0],
        stream_positions: [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect(),
        stream_normals_tangents: vec![0u8; 3 * 8],
        stream_uv0: vec![0u8; 3 * 4],
        indices: [0u16, 1, 2].iter().flat_map(|i| i.to_le_bytes()).collect(),
        index_width: CookedIndexWidth::U16,
        vertex_count: 3,
        lods: vec![],
        submeshes: vec![],
        lod_metric: regia_asset::LOD_METRIC_ERROR,
        format_version: regia_asset::COOKED_MESH_FORMAT_VERSION,
        skin: None,
        stream_color0: Vec::new(),
        morph_targets: Vec::new(),
        skin_influences: regia_asset::DEFAULT_SKIN_INFLUENCES,
    };
    match mesh.to_postcard() {
        Ok(payload) => cooked(AssetTypeTag::mesh().0, &payload),
        Err(_) => Vec::new(),
    }
}

fn mesh_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    load_mesh_bytes(bytes)
        .map(|_| Vec::new())
        .map_err(|e| e.to_string())
}

/// A cooked clip with one animated track.
fn clip_fixture() -> Vec<u8> {
    let mut clip = AnimationClip::empty("fuzz", 1.0);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0)],
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Default::default()),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    match clip.to_cooked_payload() {
        Ok(payload) => cooked(clip_type_tag(), &payload),
        Err(_) => Vec::new(),
    }
}

fn clip_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    load_cooked_clip(bytes)
        .map(|_| Vec::new())
        .map_err(|e| e.to_string())
}

/// CRC-32 (IEEE) of `data`: the save footer (the engine's is private).
fn save_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A world that can restore a save: the types are registered, so a payload reaches the reflect decoder of its components and
/// resources. An empty registry would skip them all and fuzz only the postcard framing.
fn save_world() -> World {
    let mut world = World::new();
    let registry = AppTypeRegistry::default();
    {
        let mut types = registry.write();
        types.register::<Saveable>();
        types.register::<Transform>();
        types.register::<SaveInfo>();
        types.register::<SaveBlobs>();
        types.register::<AbilityCounters>();
    }
    world.insert_resource(registry);
    let mut saveable = SaveableResources::default();
    saveable.register::<SaveInfo>();
    saveable.register::<SaveBlobs>();
    saveable.register::<AbilityCounters>();
    world.insert_resource(saveable);
    world
}

/// The input is the save payload; `save_load` frames it, so mutations pass the CRC and reach the payload decoder. The
/// fixture is what `SaveGame::encode` writes for one entity and three resources.
fn save_fixture() -> Vec<u8> {
    const MAGIC_AND_VERSION: usize = 8;
    const CRC: usize = 4;
    let mut world = save_world();
    world.insert_resource(SaveInfo {
        display_name: "fuzz".to_string(),
        save_count: 1,
        ..SaveInfo::default()
    });
    let mut blobs = SaveBlobs::default();
    blobs.set("fuzz", vec![1, 2, 3]);
    world.insert_resource(blobs);
    world.insert_resource(AbilityCounters {
        tick: 1,
        last_effect_id: 2,
        rolls: vec![(1, 2, 3)],
    });
    world.spawn((
        Saveable,
        Transform {
            translation: Vec3::new(1.0, 2.0, 3.0),
            ..Transform::default()
        },
    ));
    let Ok(framed) = SaveGame::encode(&mut world, &SaveSpec::default()) else {
        return Vec::new();
    };
    let payload = framed
        .get(MAGIC_AND_VERSION..framed.len().saturating_sub(CRC))
        .unwrap_or_default();
    let Ok(mut raw) = postcard::from_bytes::<SaveFileRaw>(payload) else {
        return Vec::new();
    };
    // A run must repeat: the clock is the one thing in the file that would differ.
    raw.timestamp_secs = 1.0;
    or_empty(postcard::to_allocvec(&raw))
}

fn frame_save_payload(payload: &[u8]) -> Vec<u8> {
    // The seed payload is the current layout (format 2 carries the entity id table, ADR-3239).
    let mut framed = b"FRTS".to_vec();
    framed.extend_from_slice(&regia_framework::save_game::SAVE_FORMAT_VERSION.to_le_bytes());
    framed.extend_from_slice(payload);
    framed.extend_from_slice(&save_crc32(payload).to_le_bytes());
    framed
}

fn save_load(payload: &[u8]) -> Result<Vec<u8>, String> {
    let framed = frame_save_payload(payload);
    let mut world = save_world();
    SaveGame::decode(&mut world, &framed, &SaveSpec::default())
        .map(|_| Vec::new())
        .map_err(|e| e.to_string())
}

/// A byte target whose loader has no writer.
fn bytes_target(
    name: &'static str,
    format: Option<&'static str>,
    fixture: fn() -> Vec<u8>,
    load: fn(&[u8]) -> Result<Vec<u8>, String>,
) -> FuzzTarget {
    FuzzTarget {
        name,
        format,
        format_version: 0,
        round_trip: false,
        input: FuzzInput::Bytes { fixture, load },
    }
}

/// Every target the guided fuzzer runs beyond the built-in registry.
pub fn targets() -> Vec<FuzzTarget> {
    let mut all = regia_script_lua::fuzz::fuzz_targets();
    all.extend([
        bytes_target("import.gltf", None, fuzz_entry::glb_fixture, fuzz_entry::glb_load),
        FuzzTarget {
            name: "import.obj",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Text {
                fixture: fuzz_entry::obj_fixture,
                load: fuzz_entry::obj_load,
            },
        },
        // The USD importer (ADR-3235): text layers, and packages holding a binary crate layer.
        FuzzTarget {
            name: "import.usd",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Text {
                fixture: fuzz_entry::usd_fixture,
                load: fuzz_entry::usd_load,
            },
        },
        bytes_target(
            "import.usdz",
            None,
            fuzz_entry::usdz_fixture,
            fuzz_entry::usdz_load,
        ),
        bytes_target("pak.read", Some("pak"), pak_fixture, pak_load),
        bytes_target("cooked.mesh", Some("cooked-mesh"), mesh_fixture, mesh_load),
        bytes_target("cooked.clip", Some("cooked-clip"), clip_fixture, clip_load),
        bytes_target("save.payload", Some("save"), save_fixture, save_load),
    ]);
    // ADR-3262: Radiance HDR, EXR, Tiled, IES, audio, WebM, heightmaps, atlas, FBX.
    all.extend(fuzz_entry::more_entries().into_iter().map(|entry| FuzzTarget {
        name: entry.name,
        format: None,
        format_version: 0,
        round_trip: false,
        input: match entry.kind {
            fuzz_entry::ImportEntryKind::Bytes { fixture, load } => FuzzInput::Bytes { fixture, load },
            fuzz_entry::ImportEntryKind::Text { fixture, load } => FuzzInput::Text { fixture, load },
        },
    }));
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the save target: a mutated payload reaches the component and resource decoders. This fixture restores
    /// each of them, so they are on the path.
    #[test]
    fn the_save_fixture_restores_through_the_reflect_decoders() {
        let framed = frame_save_payload(&save_fixture());
        let mut world = save_world();
        let result =
            SaveGame::decode(&mut world, &framed, &SaveSpec::default()).expect("the fixture decodes");
        assert_eq!((result.entity_count, result.resource_count), (1, 3));
        let mut transforms = world.query::<&Transform>();
        let restored: Vec<Vec3> = transforms.iter(&world).map(|t| t.translation).collect();
        assert_eq!(restored, vec![Vec3::new(1.0, 2.0, 3.0)]);
        assert_eq!(world.resource::<SaveInfo>().display_name, "fuzz");
        assert_eq!(world.resource::<AbilityCounters>().rolls, vec![(1, 2, 3)]);
        assert_eq!(world.resource::<SaveBlobs>().entries["fuzz"], vec![1, 2, 3]);
    }

    #[test]
    fn the_save_fixture_repeats() {
        assert_eq!(save_fixture(), save_fixture());
    }
}
