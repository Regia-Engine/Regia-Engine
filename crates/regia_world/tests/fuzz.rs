// SPDX-License-Identifier: MPL-2.0
//! Spec 78 § 18 test 2, the fuzz half: mutated world indexes and cell blobs fail without a panic. The
//! inputs come from a fixed seed, so a failure repeats.

use regia_asset::AssetId;
use regia_core::id::Guid;
use regia_scene::cooked::{COOKED_SCENE_VERSION, CookedScene};
use regia_scene::level::LevelId;
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::LevelStreaming;
use regia_world::format::{
    CELL_BLOB_FORMAT_VERSION, CellBlob, WORLD_INDEX_FORMAT_VERSION, WorldIndex, cell_blob_id,
};

fn level() -> LevelId {
    LevelId(Guid::from_u128(42))
}

/// Numerical Recipes' 64-bit LCG.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 16
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mutate(rng: &mut Lcg, base: &[u8]) -> Vec<u8> {
    let mut bytes = base.to_vec();
    match rng.below(4) {
        0 => {
            let cut = rng.below(bytes.len());
            bytes.truncate(cut);
        }
        1 => {
            for _ in 0..1 + rng.below(8) {
                let at = rng.below(bytes.len());
                bytes[at] = rng.next() as u8;
            }
        }
        2 => {
            let at = rng.below(bytes.len());
            let run = (rng.below(64)).min(bytes.len() - at);
            for byte in &mut bytes[at..at + run] {
                *byte = 0xff;
            }
        }
        _ => {
            for _ in 0..rng.below(32) {
                bytes.push(rng.next() as u8);
            }
        }
    }
    bytes
}

#[test]
fn mutated_blobs_never_panic_and_a_changed_file_never_passes_the_hash() {
    let id = cell_blob_id(level(), CellCoord::new(1, 2));
    let blob = CellBlob {
        format_version: CELL_BLOB_FORMAT_VERSION,
        coord: CellCoord::new(1, 2),
        scene: CookedScene {
            format_version: COOKED_SCENE_VERSION,
            strings: vec!["a".into(), "b".into()],
            entities: vec![],
            resources: vec![],
        },
        anchor: [1, 0, -2],
        nav: Some(vec![9; 32]),
    };
    let bytes = blob
        .to_bytes(id, vec![AssetId::sub_asset(id, "dep")])
        .expect("encode");
    let hash = *blake3::hash(&bytes).as_bytes();
    let mut rng = Lcg(0xfa11);
    for _ in 0..20_000 {
        let mutated = mutate(&mut rng, &bytes);
        let result = CellBlob::from_bytes(&mutated, id, &hash);
        if mutated != bytes {
            assert!(result.is_err());
        }
        // With a hash that matches, the parsers meet the damage themselves.
        let matching = *blake3::hash(&mutated).as_bytes();
        let _ = CellBlob::from_bytes(&mutated, id, &matching);
    }
}

#[test]
fn mutated_indexes_never_panic() {
    let index = WorldIndex {
        format_version: WORLD_INDEX_FORMAT_VERSION,
        level: level().0,
        content_hash: [1; 32],
        settings: LevelStreaming {
            enabled: true,
            ..LevelStreaming::default()
        },
        cells: vec![],
        bounds: [CellCoord::new(0, 0), CellCoord::new(0, 0)],
        hlod: None,
        replicated: vec![(Guid::from_u128(7), CellCoord::new(0, 0))],
        nav: None,
    };
    let bytes = index.to_bytes().expect("encode");
    let mut rng = Lcg(0xbead);
    for _ in 0..20_000 {
        let mutated = mutate(&mut rng, &bytes);
        if let Ok(parsed) = WorldIndex::from_bytes(&mutated, level()) {
            let _ = parsed.validate();
        }
    }
}
