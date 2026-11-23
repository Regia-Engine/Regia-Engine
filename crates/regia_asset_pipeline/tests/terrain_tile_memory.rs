// SPDX-License-Identifier: MPL-2.0
//! The tiled terrain texture cook holds one copy of the mip chain, not two (fixrev-terrain-tiles-g,
//! review finding 1 on ADR-5294). The threaded tile encoder used to copy every tile of every tiled
//! mip into its job list before encoding: about +358 MB per 8192² slot on top of the 358 MB chain.
//!
//! A counting global allocator measures the live heap during one `cook_tiled_texture` call. The
//! transient part of the peak (what is live beyond the mip chain and the cook's own output) must
//! stay well under the size of the tiled mip levels, which a full up-front copy of the tiles
//! exceeds. Desktop BCn and Android ASTC 4×4 share the job list, so both are measured. The
//! counter sees the Rust heap (the tile copies and mip chains are `Vec`s), not the ASTC encoder's
//! C++ allocations. Counts, not time; this file holds one test so no other test shares the
//! counter. CPU only; no GPU.

// A global allocator needs `unsafe impl GlobalAlloc` (as `tiled_inflate_alloc.rs`); its own binary.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use regia_asset_pipeline::{TextureCookQuality, TileFormat, cook_tiled_texture};

/// Live heap bytes and their high-water mark since the last [`reset_peak`].
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every call forwards to `System` with the caller's layout; the counters only observe.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grow(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grow(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
            grow(new_size);
        }
        new
    }
}

fn grow(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::SeqCst) + bytes;
    PEAK.fetch_max(live, Ordering::SeqCst);
}

fn reset_peak() -> usize {
    let live = LIVE.load(Ordering::SeqCst);
    PEAK.store(live, Ordering::SeqCst);
    live
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Deterministic noise (xorshift), so the encoders see real content.
fn noise_rgba(size: u32) -> Vec<u8> {
    let mut state = 0x9e37_79b9_u32;
    (0..size * size * 4)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        })
        .collect()
}

/// One tiled cook of a `size`² noise map at `tile`: the heap it held beyond its mip chain and its
/// output at the high-water mark, and the size of the levels that are cut into tiles.
fn transient_beyond_chain(platform: &str, size: u32, tile: u32) -> (u64, u64) {
    let source = noise_rgba(size);
    // The full RGBA8 chain the cook builds (mip 0 is a copy of the source) and the part of it at
    // or above the tile size (the levels that are cut into tiles).
    let level = |mip: u32| (u64::from((size >> mip).max(1))).pow(2) * 4;
    let chain_bytes: u64 = (0..=size.ilog2()).map(level).sum();
    let tiled_bytes: u64 = (0..=(size / tile).ilog2()).map(level).sum();
    let tiles: u32 = (0..=(size / tile).ilog2())
        .map(|mip| ((size / tile) >> mip).pow(2))
        .sum();

    let before = reset_peak();
    let cooked = cook_tiled_texture(
        "memory",
        size,
        size,
        &source,
        tile,
        TileFormat::Bc4,
        platform,
        TextureCookQuality::High,
    )
    .unwrap_or_else(|err| panic!("{platform} tiled cook: {err}"));
    let peak = PEAK.load(Ordering::SeqCst);
    let after = LIVE.load(Ordering::SeqCst);
    assert_eq!(cooked.tiles.len(), tiles as usize, "{platform}");

    // What stays live after the call is the cook's output; the chain is freed on return.
    let output = after.saturating_sub(before) as u64;
    let transient = (peak - before) as u64 - output.min((peak - before) as u64);
    let beyond_chain = transient.saturating_sub(chain_bytes);
    eprintln!(
        "{platform} {size}² at {tile}: chain {chain_bytes} B, tiled levels {tiled_bytes} B, \
         output {output} B, peak over start {} B, transient beyond the chain {beyond_chain} B",
        peak - before
    );
    (beyond_chain, tiled_bytes)
}

#[test]
fn tiled_cook_does_not_copy_every_tile_up_front() {
    // 1365 desktop BC4 tiles and 341 Android ASTC 4×4 tiles (one channel, the cheapest encode).
    // Both are measured before either is judged, so a failure reports both platforms.
    let measured = [("linux", 2048u32, 64u32), ("android", 1024, 64)]
        .map(|(platform, size, tile)| (platform, transient_beyond_chain(platform, size, tile)));
    for (platform, (beyond_chain, tiled_bytes)) in measured {
        assert!(
            beyond_chain < tiled_bytes / 2,
            "{platform}: the tile cook held {beyond_chain} B beyond its mip chain and output; a \
             second copy of the tiled levels is {tiled_bytes} B (bound {} B)",
            tiled_bytes / 2
        );
    }
}
