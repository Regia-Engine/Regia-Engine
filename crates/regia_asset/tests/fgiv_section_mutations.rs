// SPDX-License-Identifier: MPL-2.0
//! Bounded storage regressions, separate from the seeded no-panic corpus in `fuzz.rs`.
//! All starting files use the public writers; no independent legacy header schema lives here.

use regia_asset::baked_gi::{
    BAKED_GI_MIN_VERSION, BAKED_GI_VERSION, BakeInfo, BakeLights, BakedEmitter, BakedGiError, BakedGiVolume,
    DetailBrick, GiComponent, ProbeData, ProbeFlags, ProbeMeta, TerrainLayer, VolumeLayout,
};
use regia_asset::baked_gi_split::migrate_baked_gi_file;

const MAX_FIXTURE_BYTES: usize = 4096;

fn tiny_volume() -> BakedGiVolume {
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-2.0, 3.0],
            spacing: [2.0; 2],
            counts: [1, 1],
            levels: vec![1.0],
            ground: vec![0.5],
        }),
        bricks: vec![DetailBrick {
            origin: [0.0, 1.0, 0.0],
            spacing: [1.0; 3],
            counts: [1; 3],
            first_probe: 0,
            flags: 0,
        }],
        moment_resolution: 2,
        max_distance: 8.0,
    };
    layout.assign_brick_offsets();
    let probes: Vec<_> = (0..2)
        .map(|i| ProbeData {
            sh: std::array::from_fn(|c| {
                std::array::from_fn(|ch| {
                    let l0 = (1 + i * 12 + c * 3 + ch) as f32;
                    [l0, l0 * 0.1, -l0 * 0.2, l0 * 0.05]
                })
            }),
            moments: vec![[1.0 + i as f32, 2.0 + i as f32]; 4],
            meta: ProbeMeta {
                offset: [0.25, 0.0, -0.125],
                flags: ProbeFlags::VALID | ProbeFlags::RELOCATED | ProbeFlags::wall(2, 0.3),
            },
        })
        .collect();
    BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
        .expect("tiny public-writer fixture")
}

fn versioned_bytes(volume: &BakedGiVolume, version: u32) -> Vec<u8> {
    let bytes = match version {
        1 => volume.to_bytes_v1(),
        2 => volume.to_bytes_v2(),
        3 => volume.to_bytes_v3(),
        4 => volume.to_bytes_v4(),
        5 => volume.to_bytes(),
        _ => panic!("add the new supported writer to this corpus"),
    }
    .expect("supported writer");
    assert!(bytes.len() <= MAX_FIXTURE_BYTES);
    assert_eq!(word(&bytes, 4), version);
    bytes
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("fixture word"))
}

fn assert_preserved(expected: &BakedGiVolume, actual: &BakedGiVolume) {
    assert_eq!(actual.probe_count(), 2);
    assert_eq!(actual.layout(), expected.layout());
    assert_eq!(actual.exposure(), expected.exposure());
    assert_eq!(actual.meta_bytes(), expected.meta_bytes());
    assert_eq!(actual.sh_bytes(), expected.sh_bytes());
    assert_eq!(actual.moment_bytes(), expected.moment_bytes());
    for i in 0..2 {
        assert_eq!(actual.probe_meta(i), expected.probe_meta(i));
        assert_eq!(actual.probe_moments(i), expected.probe_moments(i));
        for c in GiComponent::ALL {
            assert_eq!(actual.probe_sh(i, c), expected.probe_sh(i, c));
            assert_eq!(
                actual.irradiance(i, c, [0.0, 1.0, 0.0]),
                expected.irradiance(i, c, [0.0, 1.0, 0.0])
            );
            assert!(actual.probe_sh(i, c)[0][0] > 0.0);
        }
    }
    assert_eq!(actual, expected);
}

#[test]
fn every_supported_writer_preserves_all_components_and_migration() {
    assert_eq!((BAKED_GI_MIN_VERSION, BAKED_GI_VERSION), (1, 5));
    let volume = tiny_volume();
    for version in BAKED_GI_MIN_VERSION..=BAKED_GI_VERSION {
        let bytes = versioned_bytes(&volume, version);
        let loaded = BakedGiVolume::from_bytes(&bytes).expect("valid version");
        assert_preserved(&volume, &loaded);
        let migrated = if version < BAKED_GI_VERSION {
            migrate_baked_gi_file(&bytes).expect("existing legacy migrator")
        } else {
            assert!(
                migrate_baked_gi_file(&bytes).is_err(),
                "current files do not migrate"
            );
            loaded.to_bytes().expect("current rewrite")
        };
        assert_eq!(word(&migrated, 4), BAKED_GI_VERSION);
        assert_preserved(
            &volume,
            &BakedGiVolume::from_bytes(&migrated).expect("migrated volume"),
        );
        assert!(migrate_baked_gi_file(&migrated).is_err());
        assert_eq!(
            BakedGiVolume::from_bytes(&migrated)
                .expect("current")
                .to_bytes()
                .expect("stable rewrite"),
            migrated
        );
    }
}

#[test]
fn every_strict_prefix_has_a_typed_outcome_including_optional_padding() {
    let volume = tiny_volume();
    let mut checked = 0;
    let mut accepted_padding = 0;
    for version in BAKED_GI_MIN_VERSION..=BAKED_GI_VERSION {
        let bytes = versioned_bytes(&volume, version);
        let header_end = 12 + word(&bytes, 8) as usize;
        // The writer's last section is exactly one f32 ground height. It pads that section
        // from 4 to 16 bytes; the last 12 bytes carry no records and are not hashed.
        let payload_end = bytes.len() - 12;
        for end in 0..bytes.len() {
            let result = BakedGiVolume::from_bytes(&bytes[..end]);
            if end < header_end {
                assert!(
                    matches!(result, Err(BakedGiError::BadMagic)),
                    "v{version} prefix {end}: {result:?}"
                );
            } else if end < payload_end {
                assert!(
                    matches!(result, Err(BakedGiError::Payload(_))),
                    "v{version} prefix {end}: {result:?}"
                );
            } else {
                assert_preserved(&volume, &result.expect("only unused tail padding was removed"));
                accepted_padding += 1;
            }
            checked += 1;
        }
    }
    assert!(checked > 1000 && checked <= 4 * MAX_FIXTURE_BYTES);
    assert_eq!(accepted_padding, 5 * 12);
    println!("{checked} strict prefixes checked, including {accepted_padding} valid padding prefixes");
}

#[test]
fn declared_header_size_boundaries_have_typed_rejections() {
    let volume = tiny_volume();
    for version in BAKED_GI_MIN_VERSION..=BAKED_GI_VERSION {
        let valid = versioned_bytes(&volume, version);
        let header_len = word(&valid, 8);
        for len in [0, 1, header_len - 1] {
            let mut bytes = valid.clone();
            bytes[8..12].copy_from_slice(&len.to_le_bytes());
            assert!(
                matches!(BakedGiVolume::from_bytes(&bytes), Err(BakedGiError::Header(_))),
                "v{version}, header length {len}"
            );
        }
        for len in [(valid.len() - 11) as u32, u32::MAX] {
            let mut bytes = valid.clone();
            bytes[8..12].copy_from_slice(&len.to_le_bytes());
            assert!(matches!(
                BakedGiVolume::from_bytes(&bytes),
                Err(BakedGiError::BadMagic)
            ));
        }
        // The public writer puts section 0 at the next 16-byte boundary.
        let first_section = (12 + header_len as usize).next_multiple_of(16);
        for end in [first_section + 1, valid.len()] {
            let mut header_over_payload = valid.clone();
            header_over_payload[8..12].copy_from_slice(&((end - 12) as u32).to_le_bytes());
            assert!(
                matches!(
                    BakedGiVolume::from_bytes(&header_over_payload),
                    Err(BakedGiError::Payload(_))
                ),
                "v{version}: header end {end} aliases a section"
            );
        }
        // postcard accepts extension bytes up to the first section, including an exact
        // shared boundary. They are outside the serialized header and payload hash.
        for len in [header_len + 1, (first_section - 12) as u32] {
            let mut extended = valid.clone();
            extended[8..12].copy_from_slice(&len.to_le_bytes());
            extended[12 + header_len as usize..12 + len as usize].fill(0xa5);
            assert_preserved(
                &volume,
                &BakedGiVolume::from_bytes(&extended).expect("header padding extension"),
            );
        }
    }
}

#[test]
fn recorded_emitters_and_interior_flags_survive_supported_versions() {
    let mut volume = tiny_volume();
    volume.bricks[0].flags = 1;
    for version in 2..=BAKED_GI_VERSION {
        let bytes = versioned_bytes(&volume, version);
        assert_preserved(&volume, &BakedGiVolume::from_bytes(&bytes).expect("interior"));
    }
    volume.emitters = Some(vec![BakedEmitter {
        min: [-1.0; 3],
        max: [1.0; 3],
    }]);
    for version in 3..=BAKED_GI_VERSION {
        let bytes = versioned_bytes(&volume, version);
        let migrated = if version < BAKED_GI_VERSION {
            migrate_baked_gi_file(&bytes).expect("recorded emitter migration")
        } else {
            bytes
        };
        assert_preserved(&volume, &BakedGiVolume::from_bytes(&migrated).expect("emitter"));
    }
}

#[test]
fn ground_partial_records_with_matching_hashes_are_payload_errors() {
    use regia_asset::baked_gi::VolumeHeader;

    let valid = tiny_volume().to_bytes().expect("public current writer");
    let header_end = 12 + word(&valid, 8) as usize;
    let header: VolumeHeader = postcard::from_bytes(&valid[12..header_end]).expect("writer header");
    for extra in 1..=3 {
        let mut h = header.clone();
        h.sections[3].len += extra;
        let mut bytes = valid.clone();
        let mut hasher = blake3::Hasher::new();
        for s in h.sections {
            hasher.update(&bytes[s.offset as usize..(s.offset + s.len) as usize]);
        }
        h.payload_hash = *hasher.finalize().as_bytes();
        let encoded = postcard::to_allocvec(&h).expect("existing header serialization");
        assert_eq!(encoded.len(), header_end - 12, "only a small stride changes");
        bytes[12..header_end].copy_from_slice(&encoded);
        let result = BakedGiVolume::from_bytes(&bytes);
        assert!(
            matches!(result, Err(BakedGiError::Payload(_))),
            "{extra} trailing ground bytes must not be silently ignored: {result:?}"
        );
    }
}
