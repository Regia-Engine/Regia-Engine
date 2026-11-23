// SPDX-License-Identifier: MPL-2.0
//! M18 robustness: mutation fuzzing of the asset loaders that consume untrusted bytes.
//!
//! Every loader here is fed a valid fixture and thousands of deterministic corruptions (truncate,
//! bit flips, hostile length/version words, spliced junk, …). Each mutation must return `Ok` or an
//! `Err` with a useful message — never panic. The shared harness also asserts empty bytes, a
//! truncation and a hostile length/version word are rejected, and that each call is time-boxed.
//! See `docs/evidence/m18/robustness.md`.
//!
//! The ADR-538 targets go past the parse: the pak target reads every entry through each read
//! path, the `.fgiv` target queries every probe of a decoded volume, and the full material target
//! saves and reloads every material that parses.

use regia_asset::{
    AssetId, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CookedImage, CookedIndexWidth, CookedMesh,
    CookedStandardMaterial, CookedTexture, CpuMorphTarget, CpuSkin, Heightfield, MaterialAssetRon,
    MetaSidecar, PakReader, TextureCookUsage, decode_cooked_file, encode_cooked_file, load_heightfield_bytes,
    load_image_bytes, load_material_bytes, load_mesh_bytes, load_texture_bytes, pack_colors_unorm16,
    pack_importer_version,
};
use regia_test_utils::fuzz::{FuzzRng, assert_no_panic, fuzz_loader, fuzz_text_loader, mutate_text};

/// Iterations per loader: enough to cover every mutation strategy many times, fast enough to stay
/// a unit test.
const N: usize = 3_000;

/// Wrap a cooked payload in the FCKD envelope with the given type tag.
fn cooked_fixture(type_tag: AssetTypeTag, payload: &[u8]) -> Vec<u8> {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: type_tag.0,
        asset_id: AssetId::new_random(),
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, payload).expect("encode cooked file")
}

fn material_cooked_file() -> Vec<u8> {
    let authored = MaterialAssetRon::default_standard();
    let payload = CookedStandardMaterial::new(authored)
        .to_postcard()
        .expect("cook material");
    cooked_fixture(AssetTypeTag::standard_material(), &payload)
}

/// A minimal cooked file (an empty-ish image payload) — used where only the envelope decode and
/// type-tag check matter.
fn minimal_cooked_file() -> Vec<u8> {
    cooked_fixture(AssetTypeTag::image(), b"payload")
}

/// A valid single-triangle `CookedMesh`, so `load_mesh_bytes` runs the real packed-stream decode
/// and validation path (not an early type-tag mismatch).
fn mesh_cooked_file() -> Vec<u8> {
    let payload = triangle_mesh().to_postcard().expect("mesh postcard");
    cooked_fixture(AssetTypeTag::mesh(), &payload)
}

/// The single-triangle mesh with the cooked mesh v2 blocks filled: a skin of `joint_count` joints
/// with two influence sets (`u8` joints up to 256, `u16` above), `COLOR_0`, and a sparse morph
/// target with position, normal and tangent deltas. `load_mesh_bytes` then runs the skin, colour,
/// morph and joint-width decoders on every mutation.
fn skinned_mesh_cooked_file(joint_count: usize) -> Vec<u8> {
    let last = (joint_count - 1) as u16;
    let skin = CpuSkin {
        joint_names: (0..joint_count).map(|i| format!("joint_{i}")).collect(),
        joints: vec![[0, 1, 2, last], [last, 0, 1, 2], [1, 2, last, 0]],
        weights: vec![
            [30000, 20000, 10000, 5535],
            [65535, 0, 0, 0],
            [16384, 16384, 16384, 16383],
        ],
        joints1: vec![[2, 1, 0, last]; 3],
        weights1: vec![[0; 4]; 3],
        eight_influences: false,
    };
    let morph = CpuMorphTarget {
        name: "smile".into(),
        vertex_indices: vec![0, 2],
        position_deltas: vec![[0.0, 0.25, -0.5], [0.125, 0.0, 0.0]],
        normal_deltas: vec![[0.0, 0.0, 0.5], [0.25, 0.0, 0.0]],
        tangent_deltas: vec![[0.5, 0.0, 0.0], [0.0, -0.25, 0.0]],
    };
    let mut mesh = triangle_mesh();
    mesh.skin = Some(skin.pack(3).expect("pack skin"));
    mesh.stream_color0 =
        pack_colors_unorm16(&[[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 0.5], [0.0, 0.0, 1.0, 0.0]]);
    mesh.morph_targets = vec![morph.pack(3).expect("pack morph")];
    let payload = mesh.to_postcard().expect("mesh postcard");
    cooked_fixture(AssetTypeTag::mesh(), &payload)
}

/// A valid single-triangle `CookedMesh` without the v2 blocks.
fn triangle_mesh() -> CookedMesh {
    let positions: Vec<u8> = [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]
        .iter()
        .flat_map(|f| f.to_le_bytes())
        .collect();
    CookedMesh {
        aabb_min: [0.0; 3],
        aabb_max: [1.0; 3],
        bounding_sphere: [0.5, 0.5, 0.5, 1.0],
        stream_positions: positions,
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
    }
}

/// A minimal valid KTX2 blob (one 4×4 BC7 block) built with the same reader the runtime uses, so
/// `CookedTexture::validate` accepts the cooked-texture fixture.
fn minimal_ktx2() -> Vec<u8> {
    use ktx2::dfd::{Basic, Block};
    use ktx2::{Format, Header, Index, LevelIndex};
    let (basic, type_size) = Basic::from_format(Format::BC7_SRGB_BLOCK).expect("dfd");
    let dfd_body = Block::Basic(basic).to_vec();
    let dfd_total_len = 4u32 + dfd_body.len() as u32;
    let level_count = 1u32;
    let level_index_len = level_count as usize * LevelIndex::LENGTH;
    let dfd_offset = Header::LENGTH + level_index_len;
    let kvd_offset = dfd_offset + dfd_total_len as usize;
    let data_offset = kvd_offset;
    let payload: Vec<u8> = vec![0u8; 16]; // one 4x4 BC7 block
    let level_index = LevelIndex {
        byte_offset: data_offset as u64,
        byte_length: payload.len() as u64,
        uncompressed_byte_length: payload.len() as u64,
    };
    let header = Header {
        format: Some(Format::BC7_SRGB_BLOCK),
        type_size,
        pixel_width: 4,
        pixel_height: 4,
        pixel_depth: 0,
        layer_count: 0,
        face_count: 1,
        level_count,
        supercompression_scheme: None,
        index: Index {
            dfd_byte_offset: dfd_offset as u32,
            dfd_byte_length: dfd_total_len,
            kvd_byte_offset: kvd_offset as u32,
            kvd_byte_length: 0,
            sgd_byte_offset: 0,
            sgd_byte_length: 0,
        },
    };
    let mut out = Vec::new();
    out.extend_from_slice(&header.as_bytes());
    out.extend_from_slice(&level_index.as_bytes());
    out.extend_from_slice(&dfd_total_len.to_le_bytes());
    out.extend_from_slice(&dfd_body);
    out.extend_from_slice(&payload);
    out
}

/// A valid `CookedTexture`, so `load_texture_bytes` runs the real postcard + KTX2 validation path.
fn texture_cooked_file() -> Vec<u8> {
    let texture = CookedTexture {
        width: 4,
        height: 4,
        usage: TextureCookUsage::ColorSrgb,
        ktx2: minimal_ktx2().into(),
    };
    let payload = texture.to_postcard().expect("texture postcard");
    cooked_fixture(AssetTypeTag::texture(), &payload)
}

/// A valid `CookedImage`, so `load_image_bytes` runs the real decode + validation path.
fn image_cooked_file() -> Vec<u8> {
    let image = CookedImage {
        width: 2,
        height: 2,
        rgba8: vec![255; 2 * 2 * 4],
    };
    let payload = image.to_postcard().expect("image postcard");
    cooked_fixture(AssetTypeTag::image(), &payload)
}

/// A valid `Heightfield`, so `load_heightfield_bytes` runs the real decode + validation path.
fn heightfield_cooked_file() -> Vec<u8> {
    let heightfield = Heightfield {
        width: 2,
        height: 2,
        samples: vec![0, 65535, 32768, 16384],
    };
    let payload = heightfield.to_postcard().expect("heightfield postcard");
    cooked_fixture(AssetTypeTag::heightfield(), &payload)
}

#[test]
fn material_ron_never_panics() {
    let fixture = include_str!("fixtures/material.example.ron");
    fuzz_text_loader(fixture, 0xA1, N, |source| {
        MaterialAssetRon::from_ron(source.as_bytes())
    });
}

#[test]
fn meta_ron_never_panics() {
    let fixture = std::str::from_utf8(include_bytes!("fixtures/meta.example.ron")).expect("utf8");
    fuzz_text_loader(fixture, 0xA2, N, |source| {
        MetaSidecar::from_ron(source.as_bytes())
    });
}

#[test]
fn pak_reader_never_panics() {
    let fixture = include_bytes!("fixtures/empty.fpak").to_vec();
    let len = fixture.len() as u64;
    fuzz_loader(&fixture, 0xA3, N, move |bytes| {
        PakReader::from_bytes(bytes.to_vec(), "fuzz.fpak", len)
    });
}

#[test]
fn cooked_header_never_panics() {
    let fixture = minimal_cooked_file();
    fuzz_loader(&fixture, 0xA5, N, |bytes| decode_cooked_file(bytes).map(|_| ()));
}

#[test]
fn cooked_material_never_panics() {
    let fixture = {
        let authored = MaterialAssetRon::default_standard();
        CookedStandardMaterial::new(authored)
            .to_postcard()
            .expect("cook material")
    };
    fuzz_loader(&fixture, 0xA6, N, CookedStandardMaterial::from_postcard);
}

#[test]
fn load_material_bytes_never_panics() {
    let fixture = material_cooked_file();
    fuzz_loader(&fixture, 0xA7, N, load_material_bytes);
}

#[test]
fn load_mesh_bytes_never_panics() {
    let fixture = mesh_cooked_file();
    fuzz_loader(&fixture, 0xA8, N, load_mesh_bytes);
}

#[test]
fn load_skinned_morphed_mesh_bytes_never_panics() {
    // u8 joints (4 joints) and u16 joints (300 joints): both widths of the joint decoder.
    for (joint_count, seed) in [(4usize, 0xB8u64), (300, 0xB9)] {
        let fixture = skinned_mesh_cooked_file(joint_count);
        let mesh = load_mesh_bytes(&fixture)
            .expect("valid skinned fixture loads")
            .mesh;
        let skin = mesh.skin.as_ref().expect("fixture reaches the skin decoder");
        assert_eq!(skin.joint_names.len(), joint_count);
        assert_eq!(skin.joints1.len(), 3, "second influence set decoded");
        assert_eq!(mesh.colors.len(), 3, "COLOR_0 decoded");
        let morph = &mesh.morph_targets[0];
        assert_eq!(morph.vertex_indices, vec![0, 2]);
        assert_eq!(morph.normal_deltas.len(), 2);
        assert_eq!(morph.tangent_deltas.len(), 2);
        fuzz_loader(&fixture, seed, N, load_mesh_bytes);
    }
}

#[test]
fn load_texture_bytes_never_panics() {
    let fixture = texture_cooked_file();
    fuzz_loader(&fixture, 0xA9, N, load_texture_bytes);
}

#[test]
fn load_image_bytes_never_panics() {
    let fixture = image_cooked_file();
    fuzz_loader(&fixture, 0xAA, N, load_image_bytes);
}

#[test]
fn load_heightfield_bytes_never_panics() {
    let fixture = heightfield_cooked_file();
    fuzz_loader(&fixture, 0xAB, N, load_heightfield_bytes);
}

/// A small valid `.fgiv`: a 2x2 terrain layer with two levels and one 2x2x2 brick, 4x4 moments.
fn baked_gi_file() -> Vec<u8> {
    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, BakedGiVolume, DetailBrick, GI_COMPONENTS, ProbeData, ProbeMeta, TerrainLayer,
        VolumeLayout,
    };
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [0.0, 0.0],
            spacing: [2.0, 2.0],
            counts: [2, 2],
            levels: vec![1.0, 3.0],
            ground: vec![0.0, 0.5, 1.0, 1.5],
        }),
        bricks: vec![DetailBrick {
            origin: [0.0, 1.0, 0.0],
            spacing: [1.0; 3],
            counts: [2, 2, 2],
            first_probe: 0,
            flags: 0,
        }],
        moment_resolution: 4,
        max_distance: 20.0,
    };
    layout.assign_brick_offsets();
    let probe = ProbeData {
        sh: [[[1.0, 0.1, -0.2, 0.05]; 3]; GI_COMPONENTS],
        moments: (0..16).map(|t| [t as f32, (t * t) as f32 + 1.0]).collect(),
        meta: ProbeMeta::default(),
    };
    let probes = vec![probe; layout.probe_count()];
    let info = BakeInfo {
        seed: 1,
        rays_per_probe: 16,
        samples: 1,
        max_bounces: 2,
        scene_hash: 7,
        filtered: false,
    };
    BakedGiVolume::from_probes(&layout, BakeLights::default(), info, &probes)
        .and_then(|v| v.to_bytes())
        .expect("encode the valid .fgiv fixture")
}

#[test]
fn baked_gi_volume_never_panics() {
    use regia_asset::baked_gi::BakedGiVolume;
    let fixture = baked_gi_file();
    BakedGiVolume::from_bytes(&fixture).expect("the fixture itself loads");
    fuzz_loader(&fixture, 0xAC, N, BakedGiVolume::from_bytes);
    // The split manifest (ADR-2618, ADR-3006): a part's split section reaches the same loader, then
    // the part goes through the tools that read a whole set.
    let parts = split_set_fixture();
    for (k, part) in parts.iter().enumerate() {
        let bytes = part.to_bytes().expect("encode a part");
        BakedGiVolume::from_bytes(&bytes).expect("a part loads");
        fuzz_loader(&bytes, 0xAD + k as u64, N, BakedGiVolume::from_bytes);
    }
}

/// A set of two parts, terrain plus a brick in each and one across the cut, every record distinct,
/// with the unsplit fixture's sections.
fn split_set_fixture() -> Vec<regia_asset::baked_gi::BakedGiVolume> {
    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, BakedGiVolume, DetailBrick, ProbeData, ProbeFlags, ProbeMeta, TerrainLayer,
        VolumeLayout,
    };
    let brick = |x: f32, cols: u32| DetailBrick {
        origin: [x, 0.5, 2.0],
        spacing: [1.0; 3],
        counts: [cols, 2, 2],
        first_probe: 0,
        flags: 0,
    };
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [0.0, 0.0],
            spacing: [2.0, 2.0],
            counts: [16, 3],
            levels: vec![1.0, 2.0],
            ground: (0..48).map(|k| k as f32 * 0.01).collect(),
        }),
        bricks: vec![brick(2.0, 2), brick(13.0, 6), brick(26.0, 2)],
        moment_resolution: 2,
        max_distance: 8.0,
    };
    layout.assign_brick_offsets();
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|i| ProbeData {
            sh: [[[0.3 + i as f32 * 0.01, 0.1, 0.0, -0.1]; 3]; 4],
            moments: vec![[1.0 + i as f32 * 0.01, 2.0 + i as f32 * 0.01]; 4],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags: ProbeFlags::VALID,
            },
        })
        .collect();
    let whole = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
        .expect("fixture volume");
    let parts = whole
        .split_parts(layout.runtime_buffer_bytes() * 7 / 10)
        .expect("the fixture splits");
    assert_eq!(parts.len(), 2);
    parts
}

/// Reads `bytes` as part `k` of `set`, runs the section's own queries over the part, joins the set
/// and queries what came back: everything a tool does with a split manifest. Anything hostile is an
/// error, never a panic or an allocation the file's size does not justify.
fn read_part_into_set(
    k: usize,
    bytes: &[u8],
    set: &[regia_asset::baked_gi::BakedGiVolume],
) -> Result<usize, regia_asset::baked_gi::BakedGiError> {
    use regia_asset::baked_gi::BakedGiVolume;
    let part = BakedGiVolume::from_bytes(bytes)?;
    let layout = part.layout();
    if let Some(split) = part.split.as_ref() {
        split.validate(&layout)?;
        for local in 0..part.probe_count() {
            let _ = (
                split.source_probe(&layout, local),
                split.owns_probe(&layout, local),
            );
        }
        let _ = split.owned_probe_count(&layout);
        for edge in 0..4 {
            let _ = split.neighbor(edge);
        }
    }
    let mut parts = set.to_vec();
    parts[k] = part;
    let whole = BakedGiVolume::join_split_set(&parts)?;
    Ok(whole.probe_count())
}

/// The fixture set, once: [`fuzz_loader`] runs its loader on worker threads with `'static` input.
fn shared_split_set() -> &'static [regia_asset::baked_gi::BakedGiVolume] {
    static SET: std::sync::OnceLock<Vec<regia_asset::baked_gi::BakedGiVolume>> = std::sync::OnceLock::new();
    SET.get_or_init(split_set_fixture)
}

/// The whole-set path of the split manifest: every part, mutated in turn, joined with the valid
/// others (ADR-3006).
#[test]
fn fgiv_split_manifest_set_never_panics() {
    use regia_asset::baked_gi::{BakedGiError, BakedGiVolume};
    fn part_0(bytes: &[u8]) -> Result<usize, BakedGiError> {
        read_part_into_set(0, bytes, shared_split_set())
    }
    fn part_1(bytes: &[u8]) -> Result<usize, BakedGiError> {
        read_part_into_set(1, bytes, shared_split_set())
    }
    let parts = shared_split_set();
    let joined = BakedGiVolume::join_split_set(parts).expect("the fixture joins");
    type PartReader = fn(&[u8]) -> Result<usize, BakedGiError>;
    let readers: [PartReader; 2] = [part_0, part_1];
    for (k, read) in readers.into_iter().enumerate() {
        let bytes = parts[k].to_bytes().expect("encode a part");
        assert_eq!(read(&bytes).expect("the fixture reads"), joined.probe_count());
        fuzz_loader(&bytes, 0x0005_3900 + k as u64, N, read);
    }
}

/// Hostile manifest values the bit flips rarely reach: each is a clean error from the join.
#[test]
fn fgiv_split_manifest_hostile_values_are_errors() {
    use regia_asset::baked_gi::BakedGiVolume;
    let good = split_set_fixture();
    type Edit = fn(&mut regia_asset::baked_gi_split::VolumeSplit);
    let cases: [(&str, Edit); 7] = [
        ("source columns", |s| s.source_columns = [u32::MAX, u32::MAX]),
        ("source column x", |s| s.source_columns[0] = u32::MAX),
        ("owned columns", |s| s.owned_columns = [0, 0, u32::MAX, u32::MAX]),
        ("source probes", |s| s.source_probes = u32::MAX),
        ("terrain offset", |s| s.terrain_offset = [u32::MAX; 2]),
        ("group", |s| s.group ^= 1),
        ("brick source", |s| {
            if let Some(b) = s.bricks.first_mut() {
                b.source = u32::MAX;
            }
        }),
    ];
    for (name, edit) in cases {
        for k in 0..good.len() {
            let mut parts = good.clone();
            if let Some(split) = parts[k].split.as_mut() {
                edit(split);
            }
            assert!(
                BakedGiVolume::join_split_set(&parts).is_err(),
                "{name} on part {k} must not join"
            );
        }
    }
}

/// Table-driven hostile `.fgiv` shapes: each must be a clean error.
#[test]
fn baked_gi_hostile_inputs_are_errors() {
    use regia_asset::baked_gi::BakedGiVolume;
    let valid = baked_gi_file();
    let with_word = |at: usize, word: u32| {
        let mut b = valid.clone();
        b[at..at + 4].copy_from_slice(&word.to_le_bytes());
        b
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("magic only", valid[..4].to_vec()),
        ("header only 12 bytes", valid[..12].to_vec()),
        ("version 0", with_word(4, 0)),
        ("version u32::MAX", with_word(4, u32::MAX)),
        ("header length u32::MAX", with_word(8, u32::MAX)),
        ("header length 0", with_word(8, 0)),
        ("truncated mid-body", valid[..valid.len() - 40].to_vec()),
        ("truncated mid-header", valid[..20].to_vec()),
    ];
    for (what, bytes) in cases {
        let err = BakedGiVolume::from_bytes(&bytes).expect_err(what);
        assert!(!err.to_string().is_empty(), "{what}: empty error message");
    }
}

/// A forged layout whose brick counts overflow `usize` when multiplied must fail validation
/// instead of overflowing (the header is not covered by the payload hash).
#[test]
fn baked_gi_layout_with_overflowing_counts_is_an_error() {
    use regia_asset::baked_gi::{DetailBrick, VolumeLayout};
    for counts in [[u32::MAX; 3], [u32::MAX, u32::MAX, 2]] {
        let layout = VolumeLayout {
            terrain: None,
            bricks: vec![
                DetailBrick {
                    origin: [0.0; 3],
                    spacing: [1.0; 3],
                    counts,
                    first_probe: 0,
                    flags: 0,
                };
                2
            ],
            moment_resolution: 4,
            max_distance: 10.0,
        };
        assert!(layout.validate().is_err(), "{counts:?}");
    }
}

/// A valid uncompressed pak with two entries and a manifest row, plus the ids to read back.
fn pak_with_entries() -> (Vec<u8>, Vec<AssetId>) {
    use regia_asset::ManifestEntry;
    use regia_asset::{FPAK_VERSION, PakEntry, PakToc, encode_pak_header};
    use regia_core::id::TypePathHash;
    let ids: Vec<AssetId> = ["a", "b"]
        .iter()
        .map(|name| AssetId::sub_asset(AssetId::default(), name))
        .collect();
    let mut data = vec![0u8; 60];
    let mut entries = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let offset = data.len() as u64;
        let payload = vec![i as u8 + 1; 37];
        data.extend_from_slice(&payload);
        entries.push(PakEntry {
            id: *id,
            type_tag: TypePathHash(7),
            offset,
            size: payload.len() as u64,
            compressed_size: payload.len() as u64,
            block_table_offset: 0,
        });
    }
    let toc_offset = data.len() as u64;
    let toc = PakToc {
        entries,
        manifest: vec![ManifestEntry {
            path: "a.bin".into(),
            id: ids[0],
            type_path: "t".into(),
        }],
        dependencies: vec!["base.fpak".into()],
    };
    let toc_bytes = postcard::to_allocvec(&toc).expect("encode the pak table of contents");
    data.extend_from_slice(&toc_bytes);
    let header = encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_bytes.len() as u64, &[0; 32]);
    data[..60].copy_from_slice(&header);
    (data, ids)
}

/// Parse a pak and read every entry back; both steps must return, never panic.
fn parse_and_read_all(bytes: &[u8]) -> Result<usize, regia_asset::PakError> {
    let reader = PakReader::from_bytes(bytes.to_vec(), "fuzz.fpak", bytes.len() as u64)?;
    let mut total = 0;
    for entry in reader.manifest() {
        total += entry.path.len();
    }
    for id in reader_ids(&reader) {
        total += reader.read_decompressed(id)?.len();
    }
    Ok(total)
}

fn reader_ids(reader: &PakReader) -> Vec<AssetId> {
    ["a", "b"]
        .iter()
        .map(|name| AssetId::sub_asset(AssetId::default(), name))
        .filter(|id| reader.contains(*id))
        .collect()
}

#[test]
fn pak_with_entries_never_panics() {
    let (fixture, ids) = pak_with_entries();
    let reader = PakReader::from_bytes(fixture.clone(), "ok.fpak", fixture.len() as u64).expect("valid pak");
    for id in ids {
        assert_eq!(reader.read_decompressed(id).expect("entry reads").len(), 37);
    }
    // Truncation (half) and a hostile leading word are rejected by the harness; the mutations
    // include hostile offsets and sizes in the table of contents.
    fuzz_loader(&fixture, 0xAD, N, parse_and_read_all);
}

/// Table-driven hostile pak tables: header fields and entry fields set to extremes.
#[test]
fn pak_hostile_header_fields_are_errors() {
    use regia_asset::{FPAK_VERSION, encode_pak_header};
    let (valid, _) = pak_with_entries();
    let toc_len = valid.len() as u64;
    let header_with = |toc_offset: u64, toc_len: u64| {
        let mut bytes = valid.clone();
        bytes[..60].copy_from_slice(&encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_len, &[0; 32]));
        bytes
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("toc offset u64::MAX", header_with(u64::MAX, 10)),
        ("toc length u64::MAX", header_with(60, u64::MAX)),
        ("toc offset + length overflows", header_with(u64::MAX - 5, 10)),
        ("toc past the end", header_with(toc_len, 1 << 40)),
        ("toc length zero", header_with(60, 0)),
        ("only the header", valid[..60].to_vec()),
        ("short of a header", valid[..59].to_vec()),
    ];
    for (what, bytes) in cases {
        let err = PakReader::from_bytes(bytes.clone(), "hostile.fpak", bytes.len() as u64)
            .err()
            .unwrap_or_else(|| panic!("{what} was accepted"));
        assert!(!err.to_string().is_empty(), "{what}: empty error message");
    }
}

// ---- ADR-538: deeper targets ----------------------------------------------------------------

/// A two-entry pak: one entry stored raw and one lz4-compressed (a block-offset table at the
/// entry start, then one size-prepended block), plus a manifest row and a dependency. The blobs
/// are packed without the writer's 4 KiB alignment (the reader does not need it), so most
/// mutations land in a TOC, block table or lz4 stream instead of padding.
fn pak_fixture() -> (Vec<u8>, [(AssetId, Vec<u8>); 2]) {
    use regia_asset::pak::{FPAK_VERSION, PakEntry, PakToc, encode_pak_header, flags};
    use regia_core::id::TypePathHash;

    let raw_id = AssetId::sub_asset(AssetId::NIL, "fuzz/raw");
    let lz4_id = AssetId::sub_asset(AssetId::NIL, "fuzz/lz4");
    let raw: Vec<u8> = (0u8..48).collect();
    let packed: Vec<u8> = (0..300u32).map(|i| (i * 7 % 13) as u8).collect();

    let mut data = vec![0u8; 60];
    let raw_offset = data.len() as u64;
    data.extend_from_slice(&raw);
    let lz4_offset = data.len() as u64;
    let block = lz4_flex::compress_prepend_size(&packed);
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
        manifest: vec![regia_asset::ManifestEntry {
            path: "fuzz/raw.bin".into(),
            id: raw_id,
            type_path: "fuzz::Raw".into(),
        }],
        dependencies: vec!["base.fpak".into()],
    };
    let toc_offset = data.len() as u64;
    let toc_bytes = postcard::to_allocvec(&toc).expect("toc");
    data.extend_from_slice(&toc_bytes);
    let hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
    let header = encode_pak_header(
        FPAK_VERSION,
        flags::COMPRESSED,
        toc_offset,
        toc_bytes.len() as u64,
        &hash,
    );
    data[..60].copy_from_slice(&header);
    (data, [(raw_id, raw), (lz4_id, packed)])
}

/// Opens a pak and reads every TOC entry through each read path, so a corrupt TOC or entry
/// reaches the block-table and lz4 decoders, not only the header parse. A corrupt entry is an
/// `Err` from its read; the property is that no read panics. Returns the entries that decoded.
fn open_and_read_every_entry(bytes: &[u8]) -> Result<usize, regia_asset::PakError> {
    let reader = std::sync::Arc::new(PakReader::from_bytes(
        bytes.to_vec(),
        "fuzz.fpak",
        bytes.len() as u64,
    )?);
    let ids: Vec<AssetId> = reader.toc.entries.iter().map(|e| e.id).collect();
    let mut decoded = 0;
    for id in ids {
        if reader.read_decompressed(id).is_ok() {
            decoded += 1;
        }
        let _ = reader.read_shared(id).map(|payload| payload.len());
        let _ = reader.prefetch(id);
        let _ = reader.read_raw(id).map(|bytes| bytes.len());
        let _ = reader.contains(id);
    }
    let _ = reader.manifest().len();
    Ok(decoded)
}

#[test]
fn pak_with_entries_reads_never_panic() {
    let (fixture, payloads) = pak_fixture();
    // The fixture reaches both decoders: the raw view and the lz4 block path.
    let reader =
        PakReader::from_bytes(fixture.clone(), "fuzz.fpak", fixture.len() as u64).expect("fixture opens");
    for (id, payload) in &payloads {
        assert_eq!(
            &reader.read_decompressed(*id).expect("fixture entry reads"),
            payload
        );
    }
    assert_eq!(open_and_read_every_entry(&fixture).expect("fixture"), 2);
    fuzz_loader(&fixture, 0x0005_3801, N, open_and_read_every_entry);
}

/// A small valid volume: a 2×2-column terrain layer of two levels and one 2×1×2 detail brick,
/// 2×2 moment texels per probe, every probe valid and relocated a little.
fn fgiv_volume() -> regia_asset::baked_gi::BakedGiVolume {
    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, BakedGiVolume, DetailBrick, ProbeData, ProbeFlags, ProbeMeta, TerrainLayer,
        VolumeLayout,
    };
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-4.0, -4.0],
            spacing: [8.0, 8.0],
            counts: [2, 2],
            levels: vec![1.0, 3.0],
            ground: vec![0.0, 0.5, 1.0, 1.5],
        }),
        bricks: vec![DetailBrick {
            origin: [-1.0, 0.5, -1.0],
            spacing: [2.0, 1.0, 2.0],
            counts: [2, 1, 2],
            first_probe: 0,
            flags: 0,
        }],
        moment_resolution: 2,
        max_distance: 8.0,
    };
    layout.assign_brick_offsets();
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|i| {
            let k = i as f32 * 0.1;
            ProbeData {
                sh: [[[0.5 + k, 0.1, -0.1, 0.05]; 3]; 4],
                moments: vec![[1.0 + k, 2.0 + k]; 4],
                meta: ProbeMeta {
                    offset: [0.1, 0.0, -0.1],
                    flags: ProbeFlags::VALID,
                },
            }
        })
        .collect();
    BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
        .expect("fixture volume")
}

/// Decodes a `.fgiv` and queries every probe the way the runtime and the agent do (positions,
/// SH, irradiance, moments, visibility, grid neighbors), then re-encodes it. A header field
/// the payload hash does not cover (counts, spacing, origins, resolution) reaches every query.
fn decode_and_query_fgiv(bytes: &[u8]) -> Result<usize, regia_asset::baked_gi::BakedGiError> {
    use regia_asset::baked_gi::{BakedGiVolume, GiComponent};
    let volume = BakedGiVolume::from_bytes(bytes)?;
    let layout = volume.layout();
    let probes = volume.probe_count();
    let _ = (layout.nominal_positions().len(), layout.half_cells().len());
    for i in 0..probes {
        let _ = (volume.probe_meta(i), volume.probe_position(i));
        for c in GiComponent::ALL {
            let _ = (volume.probe_sh(i, c), volume.irradiance(i, c, [0.0, 1.0, 0.0]));
        }
        let _ = volume.probe_moments(i).len();
        let _ = volume.visibility(i, [0.3, 0.9, -0.3], 2.0);
        let _ = layout.neighbors(i).len();
    }
    // The queries past the end answer defaults, never index out of bounds.
    let _ = (volume.probe_meta(probes), volume.probe_moments(probes + 1));
    let _ = (volume.interior_brick_count(), volume.exposure());
    let _ = volume.to_bytes();
    Ok(probes)
}

#[test]
fn fgiv_v2_never_panics() {
    let fixture = fgiv_volume().to_bytes().expect("v2 bytes");
    assert_eq!(decode_and_query_fgiv(&fixture).expect("fixture decodes"), 12);
    fuzz_loader(&fixture, 0x0005_3802, N, decode_and_query_fgiv);
}

/// A split part (format version 4, ADR-2618): the split section is in the header, which the
/// payload hash does not cover, so every mutated owner, offset and neighbor reaches the
/// queries and the section's own checks.
#[test]
fn fgiv_v4_split_part_never_panics() {
    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, BakedGiVolume, ProbeData, ProbeFlags, ProbeMeta, TerrainLayer, VolumeLayout,
    };
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [0.0, 0.0],
            spacing: [2.0, 2.0],
            counts: [12, 2],
            levels: vec![1.0],
            ground: vec![0.0; 24],
        }),
        bricks: Vec::new(),
        moment_resolution: 2,
        max_distance: 8.0,
    };
    layout.assign_brick_offsets();
    let probe = ProbeData {
        sh: [[[0.4, 0.1, 0.0, -0.1]; 3]; 4],
        moments: vec![[1.0, 2.0]; 4],
        meta: ProbeMeta {
            offset: [0.0; 3],
            flags: ProbeFlags::VALID,
        },
    };
    let whole = BakedGiVolume::from_probes(
        &layout,
        BakeLights::default(),
        BakeInfo::default(),
        &vec![probe; 24],
    )
    .expect("fixture volume");
    let parts = whole
        .split_parts(layout.runtime_buffer_bytes() * 7 / 10)
        .expect("the fixture splits");
    assert_eq!(parts.len(), 2);
    let fixture = parts[1].to_bytes().expect("v4 bytes");
    assert!(decode_and_query_fgiv(&fixture).expect("fixture decodes") > 0);
    fuzz_loader(&fixture, 0x0005_3805, N, decode_and_query_fgiv);
}

/// A set of parts that crop a brick at the cut (format version 5, ADR-3237): a terrain layer with
/// one long brick across it that is larger than the cap, so no whole-brick split fits, plus a small
/// brick inside it. Every record distinct.
fn cropped_split_set_fixture() -> Vec<regia_asset::baked_gi::BakedGiVolume> {
    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, BakedGiVolume, DetailBrick, ProbeData, ProbeFlags, ProbeMeta, TerrainLayer,
        VolumeLayout,
    };
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [0.0, 0.0],
            spacing: [2.0, 2.0],
            counts: [16, 3],
            levels: vec![1.0],
            ground: vec![0.0; 48],
        }),
        bricks: vec![
            DetailBrick {
                origin: [1.0, 0.0, 0.5],
                spacing: [0.5; 3],
                counts: [57, 2, 7],
                first_probe: 0,
                flags: 0,
            },
            DetailBrick {
                origin: [14.0, 0.0, 1.0],
                spacing: [1.0; 3],
                counts: [3, 2, 3],
                first_probe: 0,
                flags: 0,
            },
        ],
        moment_resolution: 2,
        max_distance: 8.0,
    };
    layout.assign_brick_offsets();
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|i| ProbeData {
            sh: [[[0.3 + i as f32 * 0.001, 0.1, 0.0, -0.1]; 3]; 4],
            moments: vec![[1.0 + i as f32 * 0.001, 2.0]; 4],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags: ProbeFlags::VALID,
            },
        })
        .collect();
    let whole = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
        .expect("fixture volume");
    let per_probe = 8 + 4 * 16 + 4 * 4;
    let cap = layout.bricks[0].probe_count() as u64 * per_probe * 7 / 10;
    let parts = whole.split_parts(cap).expect("the fixture splits with a crop");
    assert!(
        parts.iter().any(|p| p
            .split
            .as_ref()
            .is_some_and(|s| s.cropped_bricks(&p.layout()) > 0)),
        "the fixture crops a brick"
    );
    parts
}

fn shared_cropped_set() -> &'static [regia_asset::baked_gi::BakedGiVolume] {
    static SET: std::sync::OnceLock<Vec<regia_asset::baked_gi::BakedGiVolume>> = std::sync::OnceLock::new();
    SET.get_or_init(cropped_split_set_fixture)
}

/// A cropped split part (format version 5, ADR-3237) through the decoder and every query, then the
/// whole-set join with the valid other parts: the crop records are in the header, which the payload
/// hash does not cover, so every mutated offset, count and owned range reaches the checks.
#[test]
fn fgiv_v5_cropped_split_part_never_panics() {
    use regia_asset::baked_gi::{BakedGiError, BakedGiVolume};
    fn part_0(bytes: &[u8]) -> Result<usize, BakedGiError> {
        decode_and_query_fgiv(bytes)?;
        read_part_into_set(0, bytes, shared_cropped_set())
    }
    fn part_1(bytes: &[u8]) -> Result<usize, BakedGiError> {
        decode_and_query_fgiv(bytes)?;
        read_part_into_set(1, bytes, shared_cropped_set())
    }
    let parts = shared_cropped_set();
    let joined = BakedGiVolume::join_split_set(parts).expect("the cropped fixture joins");
    type PartReader = fn(&[u8]) -> Result<usize, BakedGiError>;
    let readers: [PartReader; 2] = [part_0, part_1];
    for (k, read) in readers.into_iter().enumerate() {
        let bytes = parts[k].to_bytes().expect("encode a part");
        assert_eq!(read(&bytes).expect("the fixture reads"), joined.probe_count());
        fuzz_loader(&bytes, 0x0005_3A00 + k as u64, N, read);
    }
}

/// Hostile crop values the bit flips rarely reach: each is a clean error from the reader or the join.
#[test]
fn fgiv_cropped_split_hostile_values_are_errors() {
    use regia_asset::baked_gi::BakedGiVolume;
    use regia_asset::baked_gi_split::SplitBrick;
    let good = cropped_split_set_fixture();
    type Edit = fn(&mut SplitBrick);
    let cases: [(&str, Edit); 6] = [
        ("crop offset", |b| b.crop_offset = [u32::MAX; 3]),
        ("source counts", |b| b.source_counts = [u32::MAX; 3]),
        ("source counts smaller", |b| {
            b.source_counts[0] = b.source_counts[0].saturating_sub(1)
        }),
        ("owned cells", |b| b.owned = [0, 0, u32::MAX, u32::MAX]),
        ("owned none", |b| b.owned = [0; 4]),
        ("shifted crop", |b| {
            b.crop_offset[0] = b.crop_offset[0].wrapping_add(1)
        }),
    ];
    for (name, edit) in cases {
        for k in 0..good.len() {
            let mut parts = good.clone();
            let Some(split) = parts[k].split.as_mut() else {
                continue;
            };
            let Some(record) = split.bricks.first_mut() else {
                continue;
            };
            edit(record);
            let read_back = parts[k]
                .to_bytes()
                .map_err(|e| e.to_string())
                .and_then(|b| BakedGiVolume::from_bytes(&b).map_err(|e| e.to_string()));
            let joined = BakedGiVolume::join_split_set(&parts);
            assert!(
                read_back.is_err()
                    || joined.is_err()
                    || joined.is_ok_and(|j| j == BakedGiVolume::join_split_set(&good).expect("good joins")),
                "{name} on part {k} must not join into another volume"
            );
        }
    }
}

#[test]
fn fgiv_v1_never_panics() {
    let fixture = fgiv_volume().to_bytes_v1().expect("v1 bytes");
    assert_eq!(decode_and_query_fgiv(&fixture).expect("fixture decodes"), 12);
    fuzz_loader(&fixture, 0x0005_3803, N, decode_and_query_fgiv);
}

/// A `.fgiv` with an empty payload (all four sections empty, so the payload hash matches) and
/// the given header fields: only the header decides whether the file is rejected.
fn fgiv_with_header(
    terrain: Option<regia_asset::baked_gi::TerrainLayer>,
    bricks: Vec<regia_asset::baked_gi::DetailBrick>,
) -> Vec<u8> {
    use regia_asset::baked_gi::{
        BAKED_GI_MAGIC, BAKED_GI_VERSION, BakeInfo, BakeLights, MOMENT_ENCODING_UNORM16,
        SH_ENCODING_RGB9E5_SNORM8, SectionRef, VolumeHeader,
    };
    let header = VolumeHeader {
        probe_count: 0,
        terrain,
        bricks,
        lights: BakeLights::default(),
        info: BakeInfo::default(),
        moment_resolution: 2,
        max_distance: 8.0,
        exposure: [1.0; 4],
        sh_encoding: SH_ENCODING_RGB9E5_SNORM8,
        moment_encoding: MOMENT_ENCODING_UNORM16,
        sections: [SectionRef::default(); 4],
        payload_hash: *blake3::hash(b"").as_bytes(),
        emitters: None,
        split: None,
    };
    let body = postcard::to_allocvec(&header).expect("header");
    let mut out = Vec::new();
    out.extend_from_slice(&BAKED_GI_MAGIC);
    out.extend_from_slice(&BAKED_GI_VERSION.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Brick counts whose product overflows `usize` (and sums past `u32`) are a layout error, not
/// an arithmetic-overflow panic; non-finite origins are rejected before any query uses them.
#[test]
fn fgiv_hostile_header_layouts_are_errors() {
    use regia_asset::baked_gi::{BakedGiVolume, DetailBrick, TerrainLayer};
    let brick = |origin: [f32; 3], counts: [u32; 3]| DetailBrick {
        origin,
        spacing: [1.0; 3],
        counts,
        first_probe: 0,
        flags: 0,
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "u32::MAX³ brick",
            fgiv_with_header(None, vec![brick([0.0; 3], [u32::MAX; 3])]),
        ),
        (
            "brick past u32 probes",
            fgiv_with_header(None, vec![brick([0.0; 3], [65_536, 65_536, 65_536])]),
        ),
        (
            "NaN brick origin",
            fgiv_with_header(None, vec![brick([f32::NAN, 0.0, 0.0], [1, 1, 1])]),
        ),
        (
            "infinite terrain origin",
            fgiv_with_header(
                Some(TerrainLayer {
                    origin: [f32::INFINITY, 0.0],
                    spacing: [1.0, 1.0],
                    counts: [0, 0],
                    levels: vec![1.0],
                    ground: Vec::new(),
                }),
                Vec::new(),
            ),
        ),
        (
            "u32::MAX² terrain columns",
            fgiv_with_header(
                Some(TerrainLayer {
                    origin: [0.0, 0.0],
                    spacing: [1.0, 1.0],
                    counts: [u32::MAX, u32::MAX],
                    levels: vec![1.0],
                    ground: Vec::new(),
                }),
                Vec::new(),
            ),
        ),
    ];
    for (what, bytes) in cases {
        let result = assert_no_panic(what, || BakedGiVolume::from_bytes(&bytes));
        let error = result.err().unwrap_or_else(|| panic!("{what} was accepted"));
        assert!(!error.to_string().is_empty(), "{what}: empty error message");
    }
}

#[test]
fn material_ron_every_block_never_panics() {
    let fixture = include_str!("fixtures/material-full.example.ron");
    let parsed = MaterialAssetRon::from_ron(fixture.as_bytes()).expect("full fixture parses");
    let hard = parsed.hard_surface.as_ref().expect("hard surface block");
    assert!(hard.detail.is_some() && hard.parallax.is_some() && hard.edge_wear.is_some());
    assert!(parsed.transmission.is_some() && parsed.anisotropy.is_some() && parsed.layers.len() == 4);
    fuzz_text_loader(fixture, 0x0005_3804, N, |source| {
        MaterialAssetRon::from_ron(source.as_bytes())
    });
}

/// Every mutation of the full material that loads also saves and loads again
/// (`regia upgrade-project` rewrites materials through exactly this path), and cooks.
#[test]
fn material_ron_that_loads_also_saves_and_reloads() {
    let fixture = include_str!("fixtures/material-full.example.ron");
    let mut rng = FuzzRng::new(0x0005_3805);
    let mut failures = Vec::new();
    let mut loaded = 0usize;
    for _ in 0..N {
        let text = mutate_text(fixture, &mut rng);
        let outcome = assert_no_panic("material save/reload", || -> Result<bool, String> {
            let Ok(material) = MaterialAssetRon::from_ron(text.as_bytes()) else {
                return Ok(false);
            };
            let saved = material.to_ron_string().map_err(|e| format!("save: {e}"))?;
            MaterialAssetRon::from_ron(saved.as_bytes()).map_err(|e| format!("reload: {e}\n{saved}"))?;
            let cooked = CookedStandardMaterial::new(material)
                .to_postcard()
                .map_err(|e| format!("cook: {e}"))?;
            CookedStandardMaterial::from_postcard(&cooked).map_err(|e| format!("cooked reload: {e}"))?;
            Ok(true)
        });
        match outcome {
            Ok(true) => loaded += 1,
            Ok(false) => {}
            Err(why) if failures.len() < 3 => failures.push(why),
            Err(_) => {}
        }
    }
    assert!(
        failures.is_empty(),
        "materials that load but do not round-trip: {failures:#?}"
    );
    assert!(
        loaded > 0,
        "no mutation loaded: the round trip was never exercised"
    );
}

/// A cooked collision set (ADR-2341) with one shape of every kind, in its `FCKD` wrapper.
fn collision_set_cooked_file() -> Vec<u8> {
    use regia_asset::collision_set::{
        CollisionAxis, CollisionBox, CollisionCapsule, CollisionConvex, CollisionSphere, CollisionTriMesh,
    };
    use regia_asset::{CookedCollisionGeometry as G, CookedCollisionSet, CookedCollisionShape};
    let shape = |node: &str, geometry: G| CookedCollisionShape {
        node: node.into(),
        requested: geometry.kind(),
        translation: [0.0, 1.0, 0.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        geometry,
    };
    let set = CookedCollisionSet::new(vec![
        shape(
            "collision_box_a",
            G::Box(CollisionBox {
                half_extents: [0.5, 0.25, 1.0],
            }),
        ),
        shape("collision_sphere_b", G::Sphere(CollisionSphere { radius: 0.3 })),
        shape(
            "collision_capsule_c",
            G::Capsule(CollisionCapsule {
                half_height: 0.4,
                radius: 0.2,
                axis: CollisionAxis::X,
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
    ]);
    cooked_fixture(
        AssetTypeTag::collision_set(),
        &set.to_postcard().expect("encode set"),
    )
}

#[test]
fn load_collision_set_bytes_never_panics() {
    let fixture = collision_set_cooked_file();
    regia_asset::load_collision_set_bytes(&fixture).expect("the fixture loads");
    fuzz_loader(&fixture, 0xC5, N, regia_asset::load_collision_set_bytes);
}

/// The v0.5.0 input-path review's soak (ADR-5869): `REGIA_FUZZ_SOAK_ITERATIONS` mutations (default
/// 20 000) of each of the formats the review changed, from `REGIA_FUZZ_SOAK_FIRST_SEED` (default
/// 1); `REGIA_FUZZ_SOAK_PREFIX` keeps the targets whose name starts with it (`pak.read`,
/// `pak.summary`, `mods.pak`, `cooked.texture`). Ignored in the gate (minutes);
/// `cargo test -p regia_asset --test fuzz -- --ignored sec_review_soak --nocapture`.
#[test]
#[ignore = "long soak; run by hand"]
fn sec_review_soak() {
    use regia_asset::mods_vfs::{ModSource, ModsVfs};
    let var = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default)
    };
    let iterations = usize::try_from(var("REGIA_FUZZ_SOAK_ITERATIONS", 20_000)).unwrap_or(20_000);
    let seed = var("REGIA_FUZZ_SOAK_FIRST_SEED", 1);
    let prefix = std::env::var("REGIA_FUZZ_SOAK_PREFIX").unwrap_or_default();
    let (pak, _) = pak_fixture();
    let summary = |bytes: &[u8]| {
        PakReader::from_bytes(bytes.to_vec(), "fuzz.fpak", bytes.len() as u64).map(|reader| {
            let reader = std::sync::Arc::new(reader);
            let s = regia_asset::cooked_pack::summarize_pak(&reader, "fuzz", "linux", true);
            s.entries
        })
    };
    let mount = |bytes: &[u8]| {
        let dir = regia_test_utils::unique_temp_dir("sec-review-soak-mods");
        let path = dir.join("m.fpak");
        let result = std::fs::write(&path, bytes)
            .map_err(|e| e.to_string())
            .and_then(|()| {
                ModsVfs::default()
                    .mount("m", ModSource::Pak(path))
                    .map(|info| info.files)
                    .map_err(|e| e.to_string())
            });
        let _ = std::fs::remove_dir_all(&dir);
        result
    };
    let texture = texture_cooked_file();
    let run = |name: &str, f: &dyn Fn() -> usize| {
        if name.starts_with(&prefix) {
            let errors = f();
            eprintln!("soak {name:<16} iterations {iterations} seed {seed} errors {errors}");
        }
    };
    run("pak.read", &|| {
        fuzz_loader(&pak, seed, iterations, open_and_read_every_entry)
    });
    run("pak.summary", &|| fuzz_loader(&pak, seed, iterations, summary));
    run("mods.pak", &|| fuzz_loader(&pak, seed, iterations, mount));
    run("cooked.texture", &|| {
        fuzz_loader(&texture, seed, iterations, load_texture_bytes)
    });
}
