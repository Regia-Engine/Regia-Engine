// SPDX-License-Identifier: MPL-2.0
//! Cross-architecture acceptance for the Stage A integer avoidance port.
use regia_detsim::avoid::{OrcaBody, orca_velocity};

#[test]
fn orca_bits_match_on_aarch64() {
    let a = OrcaBody {
        id: 1,
        x_mm: -10_000,
        z_mm: 0,
        vx_mm_s: 5_000,
        vz_mm_s: 0,
        r_mm: 2_000,
    };
    let b = OrcaBody {
        id: 2,
        x_mm: 10_000,
        z_mm: 0,
        vx_mm_s: -5_000,
        vz_mm_s: 0,
        r_mm: 2_000,
    };
    assert_eq!(orca_velocity(&a, [8_000, 0], &[b]), [4_000, -600]);
    assert_eq!(orca_velocity(&b, [-8_000, 0], &[a]), [-4_000, 600]);
    assert_eq!(
        corpus_hash(),
        0xf1a1_97b3_f232_f005,
        "DO27 d6ed37677 corpus bits moved"
    );
    println!("2048-case corpus: {:#018x}", corpus_hash());
    println!(
        "orca_bits_match_on_aarch64: arch={} os={}",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
}

// Pin generated from the unmodified d6ed37677 source, not from this port.
fn corpus_hash() -> u64 {
    let mut state = 0x2545_f491_u32;
    let mut next = |span: i64| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        i64::from(state) % (span * 2 + 1) - span
    };
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for case in 0..2048 {
        let me = OrcaBody {
            id: 1,
            x_mm: next(1_000_000),
            z_mm: next(1_000_000),
            vx_mm_s: next(20_000),
            vz_mm_s: next(20_000),
            r_mm: 1 + next(2500).abs(),
        };
        let preferred = [next(20_000), next(20_000)];
        let others: Vec<_> = (0..case % 33)
            .map(|index| OrcaBody {
                id: index + 2,
                x_mm: me.x_mm + next(25_000),
                z_mm: me.z_mm + next(25_000),
                vx_mm_s: next(20_000),
                vz_mm_s: next(20_000),
                r_mm: 1 + next(2500).abs(),
            })
            .collect();
        let result = orca_velocity(&me, preferred, &others);
        let mut reversed = others.clone();
        reversed.reverse();
        assert_eq!(
            result,
            orca_velocity(&me, preferred, &reversed),
            "candidate order at case {case}"
        );
        assert_eq!(
            result,
            regia_detsim::avoid::orca_velocity_with_settings(&me, preferred, &others, &Default::default()),
            "default settings at case {case}"
        );
        for v in result {
            for byte in v.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    hash
}
