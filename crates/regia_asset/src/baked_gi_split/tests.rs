// SPDX-License-Identifier: MPL-2.0
use super::*;
use crate::baked_gi::{
    BAKED_GI_VERSION, BakeInfo, BakeLights, PORTABLE_VOLUME_BYTES, ProbeData, ProbeFlags, ProbeMeta,
};

/// A volume of `layout` whose every probe record carries its own index (meta offset words, the
/// first SH word, the first moment word), built straight from the sections: a 300 MiB volume
/// without 300 MiB of full-precision probes.
fn patterned(layout: &VolumeLayout) -> BakedGiVolume {
    let n = layout.probe_count();
    let texels = (layout.moment_resolution * layout.moment_resolution) as usize;
    let sh_rec = GI_COMPONENTS * SH_RECORD_BYTES;
    let mom_rec = texels * MOMENT_TEXEL_BYTES;
    let mut meta = vec![0u8; n * META_RECORD_BYTES];
    let mut sh = vec![0u8; n * sh_rec];
    let mut moments = vec![0u8; n * mom_rec];
    for i in 0..n {
        let tag = (i as u32).to_le_bytes();
        let m = &mut meta[i * META_RECORD_BYTES..];
        m[0..4].copy_from_slice(&tag);
        m[6..8].copy_from_slice(&ProbeFlags::VALID.to_le_bytes());
        sh[i * sh_rec..i * sh_rec + 4].copy_from_slice(&tag);
        moments[i * mom_rec..i * mom_rec + 4].copy_from_slice(&tag);
    }
    BakedGiVolume {
        terrain: layout.terrain.clone(),
        bricks: layout.bricks.clone(),
        lights: BakeLights::default(),
        info: BakeInfo {
            seed: 3,
            scene_hash: 0x5eed,
            ..BakeInfo::default()
        },
        moment_resolution: layout.moment_resolution,
        max_distance: layout.max_distance,
        emitters: Some(Vec::new()),
        split: None,
        exposure: [1.0; GI_COMPONENTS],
        meta,
        sh,
        moments,
        grid_cache: Default::default(),
    }
}

/// A flat terrain layer of `nx × nz` 2 m columns and four levels, with `bricks` 8 × 4 × 8
/// bricks spread over it (moment resolution `res`).
fn big_layout(nx: u32, nz: u32, bricks: u32, res: u32) -> VolumeLayout {
    let spacing = 2.0;
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-100.0, -50.0],
            spacing: [spacing, spacing],
            counts: [nx, nz],
            levels: vec![0.5, 1.5, 4.0, 10.0],
            ground: (0..nx * nz).map(|k| (k % 7) as f32 * 0.25).collect(),
        }),
        bricks: (0..bricks)
            .map(|k| {
                // Spread along the diagonal, some straddling the future cuts.
                let fx = (k as f32 + 0.5) / bricks as f32;
                DetailBrick {
                    origin: [
                        -100.0 + fx * (nx as f32 - 10.0) * spacing,
                        0.5,
                        -50.0 + ((k * 37) % nz.max(1)) as f32 * spacing * 0.8,
                    ],
                    spacing: [1.0, 1.0, 1.0],
                    counts: [8, 4, 8],
                    first_probe: 0,
                    flags: if k % 5 == 0 {
                        crate::baked_gi::BrickFlags::INTERIOR
                    } else {
                        0
                    },
                }
            })
            .collect(),
        moment_resolution: res,
        max_distance: 24.0,
    };
    layout.assign_brick_offsets();
    layout
}

/// `from_sections` must refuse an exposure the decoder refuses (0, NaN, subnormal): a tool that
/// re-assembles a volume must not write one the runtime cannot load (corr-low A12).
#[test]
fn from_sections_refuses_an_exposure_the_decoder_rejects() {
    let layout = big_layout(1, 1, 1, 4);
    let volume = patterned(&layout);
    let with = |exposure: [f32; GI_COMPONENTS]| {
        BakedGiVolume::from_sections(
            &layout,
            volume.lights,
            volume.info,
            exposure,
            volume.meta_bytes().to_vec(),
            volume.sh_bytes().to_vec(),
            volume.moment_bytes().to_vec(),
        )
    };
    assert!(with([0.0; GI_COMPONENTS]).is_err(), "zero exposure");
    assert!(with([f32::NAN; GI_COMPONENTS]).is_err(), "NaN exposure");
    // The smallest subnormal: 1/e overflows to infinity, which the decoder refuses. (Not every
    // subnormal does: 1/(f32::MIN_POSITIVE / 2) is about 1.7e38, finite, and both accept it.)
    assert!(
        with([f32::from_bits(1); GI_COMPONENTS]).is_err(),
        "subnormal exposure (1/e overflows)"
    );
    assert!(with([-1.0; GI_COMPONENTS]).is_err(), "negative exposure");
    assert!(with([f32::INFINITY; GI_COMPONENTS]).is_err(), "infinite exposure");
    assert!(with([1.0; GI_COMPONENTS]).is_ok());
}

/// The record of probe `i` (meta, SH, moments) in `v`.
fn record(v: &BakedGiVolume, i: usize) -> (&[u8], &[u8], &[u8]) {
    let texels = (v.moment_resolution * v.moment_resolution) as usize;
    let sh_rec = GI_COMPONENTS * SH_RECORD_BYTES;
    let mom_rec = texels * MOMENT_TEXEL_BYTES;
    (
        &v.meta[i * META_RECORD_BYTES..(i + 1) * META_RECORD_BYTES],
        &v.sh[i * sh_rec..(i + 1) * sh_rec],
        &v.moments[i * mom_rec..(i + 1) * mom_rec],
    )
}

/// Checks the split invariants of `parts` against `whole`: every part fits `cap`, every unsplit
/// probe has exactly one owner, every part record is a byte copy of its source (so a probe held
/// by two parts matches on both sides). Returns how many probes more than one part holds.
fn check_parts(whole: &BakedGiVolume, parts: &[BakedGiVolume], cap: u64) -> usize {
    let n = whole.probe_count();
    let mut owners = vec![0u8; n];
    let mut holders = vec![0u8; n];
    for part in parts {
        let split = part.split.as_ref().expect("a part names its split");
        let layout = part.layout();
        assert!(
            layout.runtime_buffer_bytes() <= cap,
            "part {} is {} bytes",
            split.index,
            layout.runtime_buffer_bytes()
        );
        split.validate(&layout).expect("valid section");
        assert_eq!(split.source_probes as usize, n);
        let mut owned = 0;
        for local in 0..part.probe_count() {
            let src = split.source_probe(&layout, local).expect("source");
            assert!(src < n, "part {} probe {local} -> {src}", split.index);
            assert_eq!(
                record(part, local),
                record(whole, src),
                "part {} probe {local} copies {src}",
                split.index
            );
            // The record's tag is the source index (catches a self-consistent wrong mapping).
            assert_eq!(
                u32::from_le_bytes(
                    part.sh[local * GI_COMPONENTS * SH_RECORD_BYTES..][..4]
                        .try_into()
                        .expect("four bytes")
                ),
                src as u32
            );
            holders[src] = holders[src].saturating_add(1);
            if split.owns_probe(&layout, local) {
                owners[src] += 1;
                owned += 1;
            }
            let nominal = layout.nominal_position(local).expect("position");
            let source = whole.layout().nominal_position(src).expect("source position");
            for a in 0..3 {
                assert!(
                    (nominal[a] - source[a]).abs() < 1e-3,
                    "part {} probe {local}: {nominal:?} vs {source:?}",
                    split.index
                );
            }
        }
        assert_eq!(owned, split.owned_probe_count(&layout));
    }
    assert!(
        owners.iter().all(|o| *o == 1),
        "a probe without exactly one owner: {:?}",
        owners.iter().position(|o| *o != 1)
    );
    // The set joins back into the unsplit volume (layout and every record), so tools that read
    // a whole bake read a split one as it was baked.
    let joined = BakedGiVolume::join_split_set(parts).expect("the set joins");
    assert!(joined.layout() == whole.layout(), "joined layout differs");
    assert!(joined == *whole, "joined volume differs from the unsplit one");
    holders.iter().filter(|h| **h > 1).count()
}

/// The acceptance case: a layout of about 300 MiB splits into parts of at most 128 MiB, every
/// probe has exactly one owner and the band probes match on both sides (byte for byte).
#[test]
fn a_300_mib_layout_splits_into_portable_parts_with_one_owner_per_probe() {
    // 760 × 760 columns × 4 levels at 4 × 4 moments (136 bytes a probe) plus 40 bricks.
    let layout = big_layout(760, 760, 40, 4);
    let bytes = layout.runtime_buffer_bytes();
    assert!((290 << 20..=320 << 20).contains(&bytes), "{} MiB", bytes >> 20);
    let whole = patterned(&layout);
    let parts = whole.split_parts(PORTABLE_VOLUME_BYTES).expect("splits");
    assert_eq!(parts.len(), 3, "300 MiB needs three 128 MiB parts");
    let shared = check_parts(&whole, &parts, PORTABLE_VOLUME_BYTES);
    // Two seams, each four shared columns (band two, guard one each side) of 760 × 4 probes,
    // plus the bricks that straddle a seam.
    assert!(shared >= 2 * 4 * 760 * 4, "{shared} shared probes");
    let groups: Vec<u64> = parts
        .iter()
        .map(|p| p.split.as_ref().expect("split part").group)
        .collect();
    assert!(groups.iter().all(|g| *g == groups[0] && *g != 0));
    // Neighbors name each other, and each band is one terrain cell in both rectangles.
    for p in &parts {
        let s = p.split.as_ref().expect("split part");
        for (e, n) in s.neighbors.iter().enumerate() {
            let Some(other) = s.neighbor(e) else {
                assert_eq!(s.band[e], 0.0, "outer edge {}", SPLIT_EDGES[e]);
                continue;
            };
            let o = parts[*n as usize].split.as_ref().expect("split part");
            assert_eq!(o.neighbor((e + 2) % 4), Some(s.index));
            assert_eq!(other, o.index);
            assert_eq!(s.band[e], 2.0);
            let a = e % 2;
            let overlap = s.rect[a + 2].min(o.rect[a + 2]) - s.rect[a].max(o.rect[a]);
            assert!((overlap - 2.0).abs() < 1e-3, "band overlap {overlap}");
        }
    }
}

#[test]
fn a_volume_that_fits_is_returned_unchanged() {
    let layout = big_layout(40, 30, 3, 4);
    let whole = patterned(&layout);
    let parts = whole.split_parts(PORTABLE_VOLUME_BYTES).expect("fits");
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0], whole);
    assert!(parts[0].split.is_none());
}

/// A lowered cap (the test-only parameter) splits a small layout, in both axes when needed,
/// and a brick-only layout on its virtual column grid.
#[test]
fn a_lowered_cap_splits_small_layouts_and_brick_only_layouts() {
    let layout = big_layout(48, 48, 6, 4);
    let whole = patterned(&layout);
    let full = layout.runtime_buffer_bytes();
    let parts = whole.split_parts(full * 35 / 100).expect("four parts");
    assert_eq!(parts.len(), 4);
    let shared = check_parts(&whole, &parts, full * 35 / 100);
    assert!(shared > 0);
    let tiles = parts[0].split.as_ref().expect("split part").tiles;
    assert_eq!(tiles[0] * tiles[1], 4);

    let mut bricks_only = VolumeLayout {
        terrain: None,
        bricks: (0..12)
            .map(|k| DetailBrick {
                origin: [k as f32 * 9.0, 0.0, (k % 3) as f32 * 3.0],
                spacing: [1.0; 3],
                counts: [8, 4, 6],
                first_probe: 0,
                flags: 0,
            })
            .collect(),
        moment_resolution: 8,
        max_distance: 8.0,
    };
    bricks_only.assign_brick_offsets();
    let whole = patterned(&bricks_only);
    let cap = bricks_only.runtime_buffer_bytes() * 6 / 10;
    let parts = whole.split_parts(cap).expect("brick-only split");
    assert!(parts.len() >= 2);
    check_parts(&whole, &parts, cap);
}

#[test]
fn a_layout_past_four_parts_is_refused_with_the_sizes() {
    let layout = big_layout(48, 48, 2, 4);
    let whole = patterned(&layout);
    let err = whole
        .split_parts(layout.runtime_buffer_bytes() / 8)
        .expect_err("too large");
    let text = err.to_string();
    assert!(text.contains("does not split into 4 parts"), "{text}");
}

/// Parts round-trip through format version 4 and the part files; `read_split_set` reads the set
/// from part 0, refuses a part from another bake, and a part refuses the older versions.
#[test]
fn parts_round_trip_through_files_and_old_versions_refuse_them() {
    let layout = big_layout(48, 20, 3, 4);
    let whole = patterned(&layout);
    let parts = whole
        .split_parts(layout.runtime_buffer_bytes() * 6 / 10)
        .expect("split");
    assert_eq!(parts.len(), 2);
    for p in &parts {
        let bytes = p.to_bytes().expect("v4");
        assert_eq!(baked_gi_version(&bytes).expect("version"), BAKED_GI_VERSION);
        assert_eq!(&BakedGiVolume::from_bytes(&bytes).expect("reads"), p);
        assert!(p.to_bytes_v3().is_err() && p.to_bytes_v2().is_err() && p.to_bytes_v1().is_err());
    }
    let dir = regia_test_utils::unique_temp_dir("baked-gi-split");
    let path = dir.join("bore.fgiv");
    let written = write_split_set(&parts, &path).expect("write");
    assert_eq!(written, vec![path.clone(), dir.join("bore.part1.fgiv")]);
    let back = BakedGiVolume::read_split_set(&path).expect("read the set");
    assert_eq!(back, parts);
    assert!(
        BakedGiVolume::read_split_set(&written[1]).is_err(),
        "part 1 is not the entry"
    );
    // A part 1 of another bake beside part 0 is refused.
    let mut other = parts[1].clone();
    other.split.as_mut().expect("split part").group ^= 1;
    std::fs::write(&written[1], other.to_bytes().expect("bytes")).expect("write");
    assert!(BakedGiVolume::read_split_set(&path).is_err());
    // Rewriting as one volume removes the stale part.
    write_split_set(std::slice::from_ref(&whole), &path).expect("rewrite");
    assert!(!written[1].exists());
    assert_eq!(BakedGiVolume::read_split_set(&path).expect("one").len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hostile split section is a clean error, not a panic or a bad index.
#[test]
fn hostile_split_sections_are_layout_errors() {
    let layout = big_layout(48, 20, 3, 4);
    let whole = patterned(&layout);
    let parts = whole
        .split_parts(layout.runtime_buffer_bytes() * 6 / 10)
        .expect("split");
    let base = parts[0].clone();
    type Edit = fn(&mut VolumeSplit);
    let cases: Vec<(&str, Edit)> = vec![
        ("count 1", |s| s.count = 1),
        ("count 9", |s| s.count = 9),
        ("index past count", |s| s.index = 7),
        ("tiles mismatch", |s| s.tiles = [3, 3]),
        ("NaN rect", |s| s.rect[0] = f32::NAN),
        ("negative band", |s| s.band[2] = -1.0),
        ("self neighbor", |s| s.neighbors[2] = s.index),
        ("no band toward a neighbor", |s| {
            let joined = s.neighbors.map(|n| n != NO_NEIGHBOR);
            for (band, joined) in s.band.iter_mut().zip(joined) {
                if joined {
                    *band = 0.0;
                }
            }
        }),
        ("brick owner past count", |s| s.bricks[0].owner = 5),
        ("missing brick record", |s| {
            s.bricks.pop();
        }),
        ("owned columns past the layer", |s| s.owned_columns[2] = 10_000),
        ("offset past the source", |s| s.terrain_offset[0] = u32::MAX),
    ];
    for (what, edit) in cases {
        let mut v = base.clone();
        edit(v.split.as_mut().expect("split part"));
        let bytes = v.to_bytes().expect("encodes");
        let err = BakedGiVolume::from_bytes(&bytes).expect_err(what);
        assert!(matches!(err, BakedGiError::Layout(_)), "{what}: {err}");
    }
}

/// Version 3 files (every bake before ADR-2618) read unsplit and migrate to version 4 with their
/// sections byte for byte.
#[test]
fn version_3_files_read_unsplit_and_migrate() {
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [0.0, 0.0],
            spacing: [4.0, 4.0],
            counts: [3, 2],
            levels: vec![1.0, 2.0],
            ground: vec![0.0; 6],
        }),
        bricks: Vec::new(),
        moment_resolution: 2,
        max_distance: 8.0,
    };
    layout.assign_brick_offsets();
    let probe = ProbeData {
        sh: [[[0.5, 0.1, 0.0, 0.0]; 3]; GI_COMPONENTS],
        moments: vec![[1.0, 2.0]; 4],
        meta: ProbeMeta {
            offset: [0.0; 3],
            flags: ProbeFlags::VALID,
        },
    };
    let mut v = BakedGiVolume::from_probes(
        &layout,
        BakeLights::default(),
        BakeInfo::default(),
        &vec![probe; 12],
    )
    .expect("volume");
    v.emitters = Some(Vec::new());
    let v3 = v.to_bytes_v3().expect("v3");
    assert_eq!(baked_gi_version(&v3).expect("version"), 3);
    let old = BakedGiVolume::from_bytes(&v3).expect("v3 reads");
    assert_eq!(old.split, None);
    assert_eq!(old, v);
    let migrated = migrate_baked_gi_file(&v3).expect("migrates");
    assert_eq!(baked_gi_version(&migrated).expect("version"), BAKED_GI_VERSION);
    let back = BakedGiVolume::from_bytes(&migrated).expect("v4 reads");
    assert_eq!(back, v);
    assert_eq!(back.meta_bytes(), old.meta_bytes());
    assert!(migrate_baked_gi_file(&migrated).is_err(), "already current");
    let mut plain = v.clone();
    plain.emitters = None;
    for old_version in [plain.to_bytes_v1(), plain.to_bytes_v2()] {
        let Ok(bytes) = old_version else {
            continue;
        };
        assert_eq!(
            BakedGiVolume::from_bytes(&migrate_baked_gi_file(&bytes).expect("migrates"))
                .expect("reads")
                .split,
            None
        );
    }
    // Version 4 joined the sources with ADR-3237 (`version_4_parts_read_uncropped_and_migrate`).
    assert_eq!(baked_gi_migration_sources(), vec![1, 2, 3, 4]);
    assert!(baked_gi_schema_fingerprint().contains("owned_columns"));
}

/// A layout two columns deep still splits along its long axis (an uncut axis needs no minimum
/// column count).
#[test]
fn a_narrow_layout_splits_along_its_long_axis() {
    let layout = big_layout(40, 2, 0, 4);
    let whole = patterned(&layout);
    let cap = layout.runtime_buffer_bytes() * 7 / 10;
    let parts = whole.split_parts(cap).expect("splits");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].split.as_ref().expect("split part").tiles, [2, 1]);
    check_parts(&whole, &parts, cap);
}

/// `join_split_set` refuses anything but one complete set in index order, and returns a whole
/// volume unchanged.
#[test]
fn only_a_complete_set_joins() {
    let layout = big_layout(48, 20, 3, 4);
    let whole = patterned(&layout);
    let cap = layout.runtime_buffer_bytes() * 7 / 10;
    let parts = whole.split_parts(cap).expect("splits");
    assert!(parts.len() >= 2);
    assert!(BakedGiVolume::join_split_set(std::slice::from_ref(&whole)).expect("whole") == whole);
    assert!(BakedGiVolume::join_split_set(&[]).is_err(), "no parts");
    assert!(
        BakedGiVolume::join_split_set(&parts[..1]).is_err(),
        "a missing part"
    );
    let mut reversed = parts.clone();
    reversed.reverse();
    assert!(
        BakedGiVolume::join_split_set(&reversed).is_err(),
        "parts out of order"
    );
    let mut mixed = parts.clone();
    mixed[1] = whole.clone();
    assert!(
        BakedGiVolume::join_split_set(&mixed).is_err(),
        "a whole volume among parts"
    );
    let other = patterned(&big_layout(48, 21, 3, 4))
        .split_parts(cap)
        .expect("other splits");
    let mut foreign = parts.clone();
    foreign[1] = other[1].clone();
    assert!(
        BakedGiVolume::join_split_set(&foreign).is_err(),
        "a part of another bake"
    );
}

/// A terrain layer of 2 m columns with one long, tall brick across the middle that is larger than
/// the cap (ADR-3237): no split that keeps it whole fits, so it is cropped at every cut.
fn tower_layout(res: u32) -> VolumeLayout {
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-40.0, -20.0],
            spacing: [2.0, 2.0],
            counts: [41, 21],
            levels: vec![0.5, 1.5, 4.0],
            ground: vec![0.0; 41 * 21],
        }),
        bricks: vec![
            DetailBrick {
                origin: [-30.0, 0.0, -6.0],
                spacing: [0.5, 0.5, 0.5],
                counts: [121, 40, 25],
                first_probe: 0,
                flags: 0,
            },
            // A small brick inside the tower's footprint (it competes for the pick near the cut)
            // and an interior room away from it.
            DetailBrick {
                origin: [-2.0, 1.0, -2.0],
                spacing: [1.0, 1.0, 1.0],
                counts: [5, 4, 5],
                first_probe: 0,
                flags: 0,
            },
            DetailBrick {
                origin: [20.0, 0.5, 10.0],
                spacing: [1.0, 1.0, 1.0],
                counts: [8, 4, 6],
                first_probe: 0,
                flags: crate::baked_gi::BrickFlags::INTERIOR,
            },
        ],
        moment_resolution: res,
        max_distance: 24.0,
    };
    layout.assign_brick_offsets();
    layout
}

/// ADR-3237: a brick larger than the cap is cropped at the cut instead of refusing the split. Every
/// unsplit probe has exactly one owner, every part record is a byte copy of its source, the set
/// joins back into the unsplit volume, and only the bricks that cross a cut are cropped.
#[test]
fn a_brick_past_the_cap_is_cropped_and_every_cell_has_one_owner() {
    let layout = tower_layout(4);
    let whole = patterned(&layout);
    let tower = layout.bricks[0].probe_count() as u64 * 136;
    let cap = tower * 7 / 10;
    assert!(layout.runtime_buffer_bytes() > cap);
    let parts = whole.split_parts(cap).expect("the tower is cropped at the cut");
    assert!(parts.len() >= 2, "{} parts", parts.len());
    check_parts(&whole, &parts, cap);
    let mut cropped = 0;
    for p in &parts {
        let s = p.split.as_ref().expect("split part");
        let layout = p.layout();
        cropped += s.cropped_bricks(&layout);
        for (b, r) in layout.bricks.iter().zip(&s.bricks) {
            assert_eq!(r.source_counts, whole.bricks[r.source as usize].counts);
            if r.is_cropped(b) {
                assert_eq!(r.source, 0, "only the tower crosses a cut");
                assert!(b.counts[0] < r.source_counts[0] || b.counts[2] < r.source_counts[2]);
                assert_eq!(b.counts[1], r.source_counts[1], "never cropped in y");
            }
        }
        assert!(p.to_bytes_v4().is_err(), "version 4 cannot hold a crop");
        let bytes = p.to_bytes().expect("v5");
        assert_eq!(baked_gi_version(&bytes).expect("version"), BAKED_GI_VERSION);
        assert_eq!(&BakedGiVolume::from_bytes(&bytes).expect("reads"), p);
    }
    assert!(cropped >= 2, "the tower is cropped on both sides of the cut");
    // The parts hold far less than the tower twice: the crop keeps a margin, not the brick.
    let stored: u64 = parts.iter().map(|p| p.layout().runtime_buffer_bytes()).sum();
    assert!(
        stored < layout.runtime_buffer_bytes() * 3 / 2,
        "{stored} bytes stored for {}",
        layout.runtime_buffer_bytes()
    );
}

/// Whole-brick splits are tried first (ADR-3237): a layout that split before ADR-3237 splits into
/// the same parts, no brick cropped, each owned whole by its centre's part, and its parts still
/// write as version 4 (the same split section).
#[test]
fn a_split_that_fits_with_whole_bricks_crops_nothing() {
    let layout = big_layout(48, 48, 6, 4);
    let whole = patterned(&layout);
    let cap = layout.runtime_buffer_bytes() * 35 / 100;
    let parts = whole.split_parts(cap).expect("four parts");
    for p in &parts {
        let s = p.split.as_ref().expect("split part");
        let layout = p.layout();
        assert_eq!(s.cropped_bricks(&layout), 0);
        for (b, r) in layout.bricks.iter().zip(&s.bricks) {
            let whole_cells = if r.owner == s.index {
                [0, 0, b.counts[0], b.counts[2]]
            } else {
                [0; 4]
            };
            assert_eq!(r.owned, whole_cells);
        }
        let v4 = p.to_bytes_v4().expect("an uncropped part writes as v4");
        assert_eq!(baked_gi_version(&v4).expect("version"), 4);
        assert_eq!(&BakedGiVolume::from_bytes(&v4).expect("v4 reads"), p);
    }
}

/// Version 4 part files (every split bake before ADR-3237) read with no brick cropped and migrate
/// to version 5 with every section byte for byte; an unsplit volume's header is the same bytes in
/// both versions (only the version word moves), so a bake that needs no split writes what it did.
#[test]
fn version_4_parts_read_uncropped_and_migrate() {
    let layout = big_layout(48, 20, 3, 4);
    let whole = patterned(&layout);
    let parts = whole
        .split_parts(layout.runtime_buffer_bytes() * 6 / 10)
        .expect("split");
    for p in &parts {
        let v4 = p.to_bytes_v4().expect("v4");
        assert_eq!(baked_gi_version(&v4).expect("version"), 4);
        let old = BakedGiVolume::from_bytes(&v4).expect("v4 reads");
        assert_eq!(&old, p);
        let migrated = migrate_baked_gi_file(&v4).expect("migrates");
        assert_eq!(baked_gi_version(&migrated).expect("version"), BAKED_GI_VERSION);
        assert_eq!(migrated, p.to_bytes().expect("v5"));
        assert_eq!(
            BakedGiVolume::from_bytes(&migrated).expect("v5 reads").meta,
            p.meta
        );
        assert!(migrate_baked_gi_file(&migrated).is_err(), "already current");
    }
    let (v4, v5) = (whole.to_bytes_v4().expect("v4"), whole.to_bytes().expect("v5"));
    assert_eq!(v4.len(), v5.len());
    assert_eq!(v4[..4], v5[..4]);
    assert_eq!(
        v4[8..],
        v5[8..],
        "an unsplit volume differs only in the version word"
    );
    assert_eq!(baked_gi_migration_sources(), vec![1, 2, 3, 4]);
    assert!(baked_gi_schema_fingerprint().contains("crop_offset"));
}

/// A hostile crop record is a clean error, not a panic, a bad index or an allocation.
#[test]
fn hostile_crop_records_are_layout_errors() {
    let layout = tower_layout(4);
    let whole = patterned(&layout);
    let cap = layout.bricks[0].probe_count() as u64 * 136 * 7 / 10;
    let parts = whole.split_parts(cap).expect("cropped split");
    let (k, base) = parts
        .iter()
        .find_map(|p| {
            let s = p.split.as_ref()?;
            let k = s
                .bricks
                .iter()
                .zip(&p.bricks)
                .position(|(r, b)| r.is_cropped(b))?;
            Some((k, p.clone()))
        })
        .expect("a part with a cropped brick");
    type Edit = fn(&mut SplitBrick);
    let cases: Vec<(&str, Edit)> = vec![
        ("offset past the source", |r| r.crop_offset[0] = u32::MAX),
        ("source smaller than the crop", |r| r.source_counts[2] = 1),
        ("owned past the crop", |r| r.owned[2] = 100_000),
        ("owned range reversed", |r| r.owned = [3, 0, 1, 1]),
        ("source past the layout", |r| {
            r.source_counts = [u32::MAX, u32::MAX, u32::MAX]
        }),
        ("first probe past the layout", |r| r.source_first = u32::MAX),
    ];
    for (what, edit) in cases {
        let mut v = base.clone();
        edit(&mut v.split.as_mut().expect("split part").bricks[k]);
        let bytes = v.to_bytes().expect("encodes");
        let err = BakedGiVolume::from_bytes(&bytes).expect_err(what);
        assert!(matches!(err, BakedGiError::Layout(_)), "{what}: {err}");
    }
}

/// The crop margin covers the deepest point of an overlapping brick of the same class, so the brick
/// pick cannot change inside a part's rectangle (ADR-3237), and is never under the minimum.
#[test]
fn crop_margins_cover_overlapping_bricks() {
    let layout = tower_layout(4);
    let m = crop_margins(&layout);
    // The small brick (5 × 4 × 5 cells: 1.5 cells deep at most) lies inside the tower.
    assert_eq!(m[0], 3, "{m:?}");
    // The tower is 40 cells tall and 25 deep (12 deep at most): the small brick keeps 13.
    assert_eq!(m[1], 13, "{m:?}");
    // The interior room overlaps no interior brick.
    assert_eq!(m[2], CROP_MIN_MARGIN_CELLS, "{m:?}");
}

/// A brick spacing the layout check accepts (finite and positive) but far finer than any bake
/// makes (1e-6 m) refined the crop grid to near `u32::MAX` columns, and the crop weights then
/// allocated a `u64` per column (an allocation abort). The split refuses such a grid with a layout
/// error before allocating, on both the crop path and the brick-only column grid.
#[test]
fn a_tiny_brick_spacing_is_refused_before_the_crop_grid_allocates() {
    for spacing in [1.0e-9f32, f32::MIN_POSITIVE, 1.0e-6] {
        let mut layout = tower_layout(4);
        layout.bricks.push(DetailBrick {
            origin: [5.0, 1.0, 5.0],
            spacing: [spacing, 1.0, spacing],
            counts: [2, 2, 2],
            first_probe: 0,
            flags: 0,
        });
        layout.assign_brick_offsets();
        layout
            .validate()
            .expect("the layout check accepts any positive spacing");
        let cap = layout.bricks[0].probe_count() as u64 * 136 * 7 / 10;
        let text = split_layout(&layout, cap)
            .expect_err("a tiny spacing")
            .to_string();
        assert!(text.contains("split grid"), "{spacing}: {text}");
    }
    // A brick-only layout: two far bricks of a tiny spacing put the virtual column grid itself
    // past the bound.
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: [0.0f32, 100.0]
            .map(|x| DetailBrick {
                origin: [x, 0.0, x],
                spacing: [1.0e-6; 3],
                counts: [2, 2, 2],
                first_probe: 0,
                flags: 0,
            })
            .to_vec(),
        moment_resolution: 4,
        max_distance: 24.0,
    };
    layout.assign_brick_offsets();
    let text = split_layout(&layout, 64).expect_err("a tiny spacing").to_string();
    assert!(text.contains("split grid"), "{text}");
}
