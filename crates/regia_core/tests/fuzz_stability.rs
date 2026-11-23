// SPDX-License-Identifier: MPL-2.0
//! The fuzz mutator is seed-stable (spec 97 § 2.1, § 4, § 5 item 4; ADR-5660 audit): `FuzzRng` is the public SplitMix64
//! stream, and a seed gives the same mutated inputs on every platform and after any refactor. A change to a constant or to
//! the strategy order changes the inputs every seeded fuzz test sees, which these tests catch.
//!
//! The digest values below were taken from this build; the lead runs the same test on Windows, macOS and Android aarch64
//! (`cargo test -p regia_core --test fuzz_stability`), where it must give the same numbers.

use regia_core::fuzz::{FuzzRng, mutate_bytes, mutate_text, mutate_text_values};

/// The published SplitMix64 reference outputs (S. Vigna, `splitmix64.c`, state starting at 0).
#[test]
fn fuzz_rng_is_the_public_splitmix64_stream() {
    let mut rng = FuzzRng::new(0);
    let words: Vec<u64> = (0..4).map(|_| rng.next_u64()).collect();
    assert_eq!(
        words,
        [
            0xE220_A839_7B1D_CDAF,
            0x6E78_9E6A_A1B9_65F4,
            0x06C4_5D18_8009_454F,
            0xF88B_B8A8_724C_81EC
        ]
    );
    let mut rng = FuzzRng::new(1_234_567);
    let words: Vec<u64> = (0..3).map(|_| rng.next_u64()).collect();
    assert_eq!(
        words,
        [
            0x599E_D017_FB08_FC85,
            0x2C73_F084_5854_0FA5,
            0x883E_BCE5_A3F2_7C77
        ]
    );
}

/// FNV-1a over a length-prefixed sequence of outputs (no dependency, the same on every platform).
fn digest(outputs: impl IntoIterator<Item = Vec<u8>>) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
    };
    for out in outputs {
        eat(&(out.len() as u64).to_le_bytes());
        eat(&out);
    }
    h
}

const BYTES_FIXTURE: &[u8] =
    b"RGPK\x02\x00\x00\x00\x10\x00\x00\x00header-and-a-payload\x00\x01\x02\x03\xFF\xFE";
const TEXT_FIXTURE: &str = "Scene(format_version: 3, name: \"yard\", scale: 1.5, visible: true, \
                            tags: [\"a\", \"b\"], items: [(id: 1, w: 0.25), (id: 2, w: -4.0)], extra: None)";

/// The mutated inputs of 256 seeds per mutator, pinned. If this fails after an intended mutator change, every seeded fuzz
/// test in the workspace now sees different inputs: say so in an ADR and update the digests in the same commit.
#[test]
fn seeded_mutations_are_pinned() {
    let bytes = digest((0..256u64).map(|seed| mutate_bytes(BYTES_FIXTURE, &mut FuzzRng::new(seed))));
    let text =
        digest((0..256u64).map(|seed| mutate_text(TEXT_FIXTURE, &mut FuzzRng::new(seed)).into_bytes()));
    let values = digest(
        (0..256u64).map(|seed| mutate_text_values(TEXT_FIXTURE, &mut FuzzRng::new(seed)).into_bytes()),
    );
    // One stream drives many mutations in a row, as a fuzz run does.
    let mut rng = FuzzRng::new(97);
    let chained = digest((0..256).map(|_| mutate_text(TEXT_FIXTURE, &mut rng).into_bytes()));
    assert_eq!(
        [bytes, text, values, chained],
        [
            0x7635_6e32_1c6e_1af4,
            0x389d_96cc_c78b_1f80,
            0x08d1_23f7_4284_befd,
            0xd37c_5e33_1643_676c
        ],
        "mutator output moved: [{bytes:#018x}, {text:#018x}, {values:#018x}, {chained:#018x}]"
    );
}

/// The same seed twice gives the same input (no clock, no OS entropy, no global state).
#[test]
fn a_seed_repeats_its_mutation() {
    for seed in [0u64, 1, 7, u64::MAX] {
        assert_eq!(
            mutate_bytes(BYTES_FIXTURE, &mut FuzzRng::new(seed)),
            mutate_bytes(BYTES_FIXTURE, &mut FuzzRng::new(seed))
        );
        assert_eq!(
            mutate_text(TEXT_FIXTURE, &mut FuzzRng::new(seed)),
            mutate_text(TEXT_FIXTURE, &mut FuzzRng::new(seed))
        );
        assert_eq!(
            mutate_text_values(TEXT_FIXTURE, &mut FuzzRng::new(seed)),
            mutate_text_values(TEXT_FIXTURE, &mut FuzzRng::new(seed))
        );
    }
}
