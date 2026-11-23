// SPDX-License-Identifier: MPL-2.0
//! Encode and decode cost of the entropy stage per state frame (spec 72 § 18 task 2; recorded,
//! not asserted). Run: `cargo bench -p regia_net --bench entropy`.

use criterion::{Criterion, criterion_group, criterion_main};
use regia_net::entropy::{code_or_raw, decode_coded};

/// A body shaped like a packed state frame: record count, small ids, mostly-stable quantized
/// fields.
fn frame_body(seed: u64, records: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(records * 11 + 1);
    out.push(records as u8);
    let mut s = seed;
    for _ in 0..records {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((s >> 33) as u8); // id
        out.push(10); // length byte
        out.extend_from_slice(&[2, 1, 0b0011_0000 | (s >> 40) as u8]);
        out.extend_from_slice(&[0x40 | (s >> 8) as u8, 0x40, 0x40, (s >> 20) as u8, 0, 0, 0, 0]);
    }
    out
}

fn bench_entropy(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_entropy");
    for (name, records) in [("small", 12usize), ("mtu", 100)] {
        let body = frame_body(42, records);
        let (coded, true) = code_or_raw(&body) else {
            panic!("the bench body must code");
        };
        group.bench_function(format!("encode/{name}"), |b| {
            b.iter(|| code_or_raw(std::hint::black_box(&body)));
        });
        group.bench_function(format!("decode/{name}"), |b| {
            b.iter(|| decode_coded(std::hint::black_box(&coded)).expect("decodes"));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_entropy);
criterion_main!(benches);
