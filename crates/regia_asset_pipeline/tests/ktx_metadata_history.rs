// SPDX-License-Identifier: MPL-2.0
//! KTX2 metadata rewrite history contracts (T2.14, `docs/specs/12-assets.md` § 7.1a).

use std::collections::BTreeMap;

use ktx2::{Header, LevelIndex};
use regia_asset::{MIP_STREAM_KEY, MipStreamMeta};
use regia_asset_pipeline::mip_stream::set_key_value;
use regia_asset_pipeline::{BcnFormat, write_ktx2_bcn};
use regia_test_utils::unique_temp_dir;

fn fixture() -> Vec<u8> {
    let bytes = write_ktx2_bcn(
        8,
        8,
        BcnFormat::Bc7Srgb,
        &[vec![0x13; 64], vec![0x27; 16], vec![0x39; 16], vec![0x4b; 16]],
    )
    .expect("valid multi-level fixture");
    let bytes = set_key_value(&bytes, "alpha", b"unrelated-alpha-value").expect("alpha key");
    let bytes = set_key_value(&bytes, "omega", b"unrelated-omega-value").expect("omega key");
    set_key_value(&bytes, MIP_STREAM_KEY, &MipStreamMeta::new(32).to_value()).expect("initial mip metadata")
}

fn store_atomically(path: &std::path::Path, bytes: &[u8]) {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes).expect("write fixture temp");
    std::fs::rename(temp, path).expect("publish fixture atomically");
}

fn index(bytes: &[u8]) -> (Header, Vec<LevelIndex>) {
    let reader = ktx2::Reader::new(bytes).expect("valid KTX2");
    let header = reader.header();
    let count = header.level_count.max(1) as usize;
    let end = Header::LENGTH + count * LevelIndex::LENGTH;
    let entries = bytes[Header::LENGTH..end]
        .as_chunks::<{ LevelIndex::LENGTH }>()
        .0
        .iter()
        .map(LevelIndex::from_bytes)
        .collect();
    (header, entries)
}

fn keys(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    ktx2::Reader::new(bytes)
        .expect("KTX2 reader")
        .key_value_data()
        .map(|(key, value)| (key.to_owned(), value.to_vec()))
        .collect()
}

#[test]
fn repeated_metadata_rewrites_preserve_fixture_layout_and_unrelated_values() {
    let temp_dir = unique_temp_dir("ktx-metadata-history");
    let original_path = temp_dir.join("original.ktx2");
    store_atomically(&original_path, &fixture());
    let baseline = std::fs::read(&original_path).expect("read saved baseline");
    let (base_header, base_indices) = index(&baseline);
    let base_reader = ktx2::Reader::new(&baseline[..]).expect("baseline reader");
    let expected_levels: Vec<Vec<u8>> = base_reader.levels().map(|level| level.data.to_vec()).collect();
    let dfd_start = base_header.index.dfd_byte_offset as usize;
    let dfd_end = dfd_start + base_header.index.dfd_byte_length as usize;
    let expected_dfd = baseline[dfd_start..dfd_end].to_vec();
    let expected_dimensions = (
        base_header.pixel_width,
        base_header.pixel_height,
        base_header.pixel_depth,
        base_header.layer_count,
        base_header.face_count,
        base_header.level_count,
    );
    let expected_format = base_header.format;
    let mut expected_keys = keys(&baseline);
    let base_payload_start = base_indices
        .iter()
        .map(|entry| entry.byte_offset)
        .min()
        .expect("fixture has level indices");
    let mut current = baseline;
    let steps: Vec<(&str, Vec<u8>)> = vec![
        ("zeta", b"123".to_vec()),
        ("zeta", b"1234".to_vec()),
        ("zeta", b"12345".to_vec()),
        ("zeta", b"1234567890".to_vec()),
        ("zeta", b"12345678901".to_vec()),
        ("zeta", b"123456789012".to_vec()),
        ("zeta", b"1234567890123".to_vec()),
        (
            "zeta",
            b"123456789012345678901234567890123456789012345678".to_vec(),
        ),
        ("zeta", Vec::new()),
        ("zeta", b"x".to_vec()),
        ("zeta", b"1234567890123456789012345678901".to_vec()),
        (MIP_STREAM_KEY, MipStreamMeta::new(64).to_value()),
        ("zeta", b"short".to_vec()),
        (
            MIP_STREAM_KEY,
            MipStreamMeta {
                min_resident_px: 512,
                ..MipStreamMeta::new(32)
            }
            .to_value(),
        ),
    ];
    for (step, (key, value)) in steps.iter().enumerate() {
        current = set_key_value(&current, key, value).expect("rewrite metadata");
        expected_keys.insert((*key).to_owned(), value.clone());

        let (header, entries) = index(&current);
        let reader = ktx2::Reader::new(&current[..]).expect("rewritten file remains readable");
        assert_eq!(
            (
                header.pixel_width,
                header.pixel_height,
                header.pixel_depth,
                header.layer_count,
                header.face_count,
                header.level_count,
            ),
            expected_dimensions,
            "dimensions at step {step}"
        );
        assert_eq!(header.format, expected_format, "format at step {step}");
        assert_eq!(
            reader
                .levels()
                .map(|level| level.data.to_vec())
                .collect::<Vec<_>>(),
            expected_levels,
            "level payloads at step {step}"
        );
        let dfd_start = header.index.dfd_byte_offset as usize;
        let dfd_end = dfd_start + header.index.dfd_byte_length as usize;
        assert_eq!(&current[dfd_start..dfd_end], expected_dfd, "DFD at step {step}");
        assert_eq!(keys(&current), expected_keys, "KVD values at step {step}");
        assert_eq!(
            header.index.kvd_byte_offset,
            header.index.dfd_byte_offset + header.index.dfd_byte_length,
            "KVD follows DFD at step {step}"
        );
        assert_eq!(entries.len(), base_indices.len(), "index count at step {step}");
        let payload_start = entries
            .iter()
            .map(|entry| entry.byte_offset)
            .min()
            .expect("rewritten file has level indices");
        let offset_adjustment = payload_start - base_payload_start;
        for (level_number, entry) in entries.iter().enumerate() {
            assert_eq!(
                entry.byte_offset,
                base_indices[level_number].byte_offset + offset_adjustment,
                "index range offset at level {level_number}, step {step}"
            );
            assert_eq!(entry.byte_length, base_indices[level_number].byte_length);
            assert_eq!(
                entry.uncompressed_byte_length,
                base_indices[level_number].uncompressed_byte_length
            );
            assert_eq!(entry.byte_offset % 16, 0, "level alignment at step {step}");
            let end = entry.byte_offset + entry.byte_length;
            assert!(end as usize <= current.len(), "index range at step {step}");
            assert_eq!(
                &current[entry.byte_offset as usize..end as usize],
                expected_levels[level_number],
                "index payload at level {level_number}, step {step}"
            );
            assert!(
                entry.byte_offset >= u64::from(header.index.kvd_byte_offset + header.index.kvd_byte_length),
                "level range begins after metadata at step {step}"
            );
        }
        if key == &MIP_STREAM_KEY {
            assert_eq!(
                MipStreamMeta::from_reader(&reader),
                MipStreamMeta::from_value(value),
                "mipstream metadata decodes at step {step}"
            );
        } else {
            let expected_mipstream = MipStreamMeta::from_value(
                expected_keys
                    .get(MIP_STREAM_KEY)
                    .expect("fixture retains the mipstream key"),
            );
            assert_eq!(MipStreamMeta::from_reader(&reader), expected_mipstream);
        }

        let again = set_key_value(&current, key, value).expect("repeat same replacement");
        assert_eq!(again, current, "identical update is byte stable at step {step}");
        let path = temp_dir.join(format!("history-{step}.ktx2"));
        store_atomically(&path, &current);
        current = std::fs::read(path).expect("read persisted history step");
    }
}

#[test]
fn metadata_rewrite_refuses_supercompression_and_noncanonical_sections() {
    let plain = fixture();
    let mut supercompressed = plain.clone();
    // KTX2 header supercompressionScheme is the u32 at byte offset 44.
    supercompressed[44..48].copy_from_slice(&2_u32.to_le_bytes());
    assert!(set_key_value(&supercompressed, "key", b"value").is_err());

    // A file the reader accepts whose DFD does not start at the end of the level index: 16 zero
    // bytes between the index and the DFD, with every later section moved by the same amount (so
    // the level alignment is unchanged). Only the layout check refuses it.
    const GAP: usize = 16;
    let (mut header, mut levels) = index(&plain);
    let index_end = Header::LENGTH + levels.len() * LevelIndex::LENGTH;
    header.index.dfd_byte_offset += GAP as u32;
    if header.index.kvd_byte_length != 0 {
        header.index.kvd_byte_offset += GAP as u32;
    }
    for level in &mut levels {
        level.byte_offset += GAP as u64;
    }
    let mut unsupported = Vec::with_capacity(plain.len() + GAP);
    unsupported.extend_from_slice(&header.as_bytes());
    for level in &levels {
        unsupported.extend_from_slice(&level.as_bytes());
    }
    unsupported.extend_from_slice(&[0; GAP]);
    unsupported.extend_from_slice(&plain[index_end..]);
    let gapped = ktx2::Reader::new(&unsupported[..]).expect("the gapped layout still parses");
    assert_eq!(
        gapped
            .levels()
            .map(|level| level.data.to_vec())
            .collect::<Vec<_>>(),
        ktx2::Reader::new(&plain[..])
            .expect("fixture reader")
            .levels()
            .map(|level| level.data.to_vec())
            .collect::<Vec<_>>(),
        "the gap moved every level with its index entry"
    );
    let refused = set_key_value(&unsupported, "key", b"value").expect_err("a DFD after a gap is refused");
    assert!(
        refused
            .to_string()
            .contains("the DFD does not follow the level index"),
        "{refused}"
    );
}
