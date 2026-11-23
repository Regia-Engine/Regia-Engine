// SPDX-License-Identifier: MPL-2.0
//! The GPU radix sort (ported from FidelityFX Parallel Sort v1.1.4) against a CPU stable sort.
//!
//! Counts cover the edges of its work split: empty, one key, one block (512), one past a
//! block, a single partial group, many groups, and more blocks than the 800-group cap
//! (groups take extra blocks and the per-bin reduce spans two scan values). Keys beyond the
//! count and pairs before the range's base must not move.

use regia_render::RenderDevice;
use regia_vfx::gpu::radix_sort::{GpuRadixSort, RadixSortTarget, RadixTieBreak};
use wgpu::util::DeviceExt;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// xorshift32: deterministic keys without a dependency.
fn keys(n: usize, seed: u32, mask: u32) -> Vec<u32> {
    let mut s = seed.max(1);
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s & mask
        })
        .collect()
}

fn read(device: &RenderDevice, buffer: &wgpu::Buffer, bytes: u64) -> Vec<u32> {
    let d = &device.device;
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("radix readback"),
        size: bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_buffer_to_buffer(buffer, 0, &staging, 0, bytes);
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    rx.recv().expect("map").expect("map ok");
    let data = staging.slice(..).get_mapped_range().expect("range");
    let out = bytemuck::cast_slice(&data[..]).to_vec();
    drop(data);
    staging.unmap();
    out
}

/// Sorts `count` pairs at `base` in a range of `max_keys` and checks the result.
fn sort_and_check(device: &RenderDevice, count: u32, max_keys: u32, base: u32, mask: u32, seed: u32) {
    let d = &device.device;
    let total = (base + max_keys) as usize;
    let key_list = keys(total, seed, mask);
    // Payload = original position, so stability is checkable.
    let mut pairs: Vec<u32> = Vec::with_capacity(total * 2);
    for (i, k) in key_list.iter().enumerate() {
        pairs.push(*k);
        pairs.push(i as u32);
    }
    let pair_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("radix pairs"),
        contents: bytemuck::cast_slice(&pairs),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
    });
    let counters = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("radix counters"),
        contents: bytemuck::cast_slice(&[7u32, count, 9, 9]),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let sort = GpuRadixSort::new(
        d,
        &RadixSortTarget {
            pairs: &pair_buf,
            base,
            max_keys,
            counters: &counters,
            count_index: 1,
            key_bits: 32,
        },
    );
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        sort.record(&mut pass);
    }
    device.queue.submit([encoder.finish()]);
    let got = read(device, &pair_buf, (total * 8) as u64);

    // Expected: the range's first `count` pairs stably sorted by key; everything else as was.
    let mut want = pairs.clone();
    let start = base as usize;
    let mut range: Vec<(u32, u32)> = (0..count as usize)
        .map(|i| (pairs[(start + i) * 2], pairs[(start + i) * 2 + 1]))
        .collect();
    range.sort_by_key(|p| p.0);
    for (i, (k, v)) in range.iter().enumerate() {
        want[(start + i) * 2] = *k;
        want[(start + i) * 2 + 1] = *v;
    }
    if got != want {
        let first = got
            .chunks(2)
            .zip(want.chunks(2))
            .position(|(g, w)| g != w)
            .unwrap_or(0);
        panic!(
            "count {count} (max {max_keys}, base {base}, mask {mask:#x}): first mismatch at pair {first}: got {:?}, want {:?}",
            &got[first * 2..first * 2 + 2],
            &want[first * 2..first * 2 + 2]
        );
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The scan scratch is one `u32` per index in one workgroup array.
///
/// `scan_prefix` stores `i * TG + lid` from all 128 invocations with no barrier,
/// so packing four indices into a `vec4` and read-modify-writing a component races
/// (lanes 0, 32, 64 and 96 share one variable). One `var<workgroup>` per index is
/// the same layout and makes naga's output enormous. A lavapipe sort can still
/// pass with the race; this check is what makes the single-writer claim true.
#[test]
fn scan_scratch_is_one_scalar_per_index() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let src = include_str!("../src/gpu/radix_sort.wgsl");
    assert!(
        src.contains("var<workgroup> lds: array<u32, 512>"),
        "scan scratch must be one u32 per index"
    );
    assert!(!src.contains("fn wl_set"), "vec4 component setter is the race");
    assert!(!src.contains("wl_set("), "vec4 component setter is the race");
    assert!(
        !src.contains("var<workgroup> ld0:"),
        "per-index variables are the expensive form"
    );
    let body = src.split("fn scan_prefix").nth(1).expect("scan_prefix");
    let body = body.split("@compute").next().expect("scan_prefix body");
    assert!(
        body.contains("lds[row * TG + col] = v"),
        "store must assign one scalar"
    );
    assert!(!body.contains("vec4"), "scan scratch must not pack a vec4");
}

#[test]
fn radix_sort_matches_a_stable_cpu_sort() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    for (count, max_keys) in [
        (0u32, 64u32),
        (1, 64),
        (7, 64),
        (511, 1024),
        (512, 1024),
        (513, 1024),
        (5_000, 8_192),
        (70_000, 70_000),
        (600_000, 600_000),
    ] {
        sort_and_check(&device, count, max_keys, 0, u32::MAX, 0x9e37_79b9 ^ count);
    }
    // Few distinct keys (heavy ties): stability decides the order.
    sort_and_check(&device, 20_000, 32_768, 0, 0x7, 77);
    // A range that starts past the buffer's front (the alpha list sits at `sort_n`), with a
    // count below the range size: the front and the tail stay put.
    sort_and_check(&device, 3_000, 4_096, 4_096, u32::MAX, 5);
}

/// Sorts `pairs` (key, payload) in place on the GPU with a tie-break and reads them back.
fn sort_with_tie_break(device: &RenderDevice, pairs: &[[u32; 2]], tie: RadixTieBreak) -> Vec<[u32; 2]> {
    let d = &device.device;
    let count = pairs.len() as u32;
    let pair_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("radix tie pairs"),
        contents: bytemuck::cast_slice(pairs),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
    });
    let counters = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("radix tie counters"),
        contents: bytemuck::cast_slice(&[0u32, count, 0, 0]),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let sort = GpuRadixSort::with_tie_break(
        d,
        &RadixSortTarget {
            pairs: &pair_buf,
            base: 0,
            max_keys: count,
            counters: &counters,
            count_index: 1,
            key_bits: 32,
        },
        tie,
    );
    assert!(sort.passes().is_multiple_of(2), "odd pass count");
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        sort.record(&mut pass);
    }
    device.queue.submit([encoder.finish()]);
    read(device, &pair_buf, u64::from(count) * 8)
        .as_chunks::<2>()
        .0
        .to_vec()
}

/// With a tie-break, equal keys come out in payload order whatever order they went in:
/// a full 32-bit payload tie-break, and the VFX alpha-list form (the key holds the low 12
/// slot bits, the tie-break sorts the slot bits above them).
#[test]
fn tie_break_orders_equal_keys_by_payload() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let count = 20_000u32;
    // Few distinct keys, unique payloads in a scrambled order.
    let key_list = keys(count as usize, 0x7135, 0x7);
    let payloads = keys(count as usize, 0x2468, u32::MAX);
    let mut pairs: Vec<[u32; 2]> = key_list.iter().zip(&payloads).map(|(&k, &p)| [k, p]).collect();
    pairs.sort_unstable_by_key(|p| p[1]);
    pairs.dedup_by_key(|p| p[1]);
    pairs.reverse();
    let mut want = pairs.clone();
    want.sort_unstable();
    let full = RadixTieBreak { shift: 0, bits: 32 };
    assert_eq!(
        sort_with_tie_break(&device, &pairs, full),
        want,
        "full payload tie-break"
    );
    let third = pairs.len() / 3;
    pairs.rotate_left(third);
    assert_eq!(
        sort_with_tie_break(&device, &pairs, full),
        want,
        "input order leaked"
    );

    // Alpha-list keys: depth bin above the low 12 slot bits; slots below 2^17.
    let slots = 1u32 << 17;
    let mut alpha: Vec<[u32; 2]> = (0..slots)
        .step_by(5)
        .zip(keys(slots as usize, 0x51ab, 0x3))
        .map(|(slot, bin)| [(bin << 12) | (slot & 0xfff), slot])
        .collect();
    alpha.reverse();
    let mut want = alpha.clone();
    want.sort_unstable();
    let slot_tie = RadixTieBreak { shift: 12, bits: 5 };
    assert_eq!(
        sort_with_tie_break(&device, &alpha, slot_tie),
        want,
        "slot-bit tie-break"
    );
}
