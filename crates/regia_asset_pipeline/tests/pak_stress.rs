// SPDX-License-Identifier: MPL-2.0
//! Pak write → mmap read stress (lane pak-corruption, 2026-10-08).
//!
//! The wave-E merge gate once read a terrain index back from a freshly cooked pak as
//! `Cooked(BadMagic)`: the bytes `PakReader::read_decompressed` returned did not start with a
//! cooked header. This test cooks real `FCKD` cooked files (random ids, so the stored lz4 lengths
//! vary run to run exactly like a real cook's fresh `.meta` ids), packs them with the shipping
//! writer and reads them back through the mapped reader, in many threads at once, over:
//!
//! - tiny, empty, exact-block-multiple and block ± 1 payloads, mixed compressibility (incompressible,
//!   partly zero, flat — the ratio-cap fallback);
//! - payloads tuned so the compressed entry stores *exactly* `size` bytes (block table + lz4
//!   blocks), the boundary the old reader mistook for "stored raw";
//! - a pak rewritten in place by one thread while others open and read it (atomic replace);
//! - truncated copies of a valid pak (must be an error, never a panic or wrong bytes).
//!
//! Each failure dumps the entry's TOC record, its stored bytes and the expected payload under the
//! scratch directory and names it in the panic. `REGIA_PAK_STRESS_ITERS` (per thread, default 40)
//! and `REGIA_PAK_STRESS_THREADS` (default: available parallelism) scale it up for a soak.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use regia_asset::{
    AssetId, AssetManifest, COOKED_FORMAT_VERSION, CookedHeader, FPAK_COMPRESS_BLOCK_SIZE, ManifestEntry,
    PakReader, cooked_file_name, decode_cooked_file, encode_cooked_file,
};
use regia_asset_pipeline::pak_writer::{PakWriteOptions, write_pak};
use regia_core::id::TypePathHash;

const TYPE_PATH: &str = "regia_terrain::stream::TerrainIndex";

fn env_usize(name: &str, default: usize) -> usize {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// xorshift64*: deterministic per seed, so a failing seed replays.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

/// The stored length of `bytes` as the writer lays a compressed entry out: one `u32` block offset
/// per block, then each block's size-prepended lz4 output.
fn stored_compressed_len(bytes: &[u8]) -> usize {
    bytes
        .chunks(FPAK_COMPRESS_BLOCK_SIZE)
        .map(|c| 4 + lz4_flex::compress_prepend_size(c).len())
        .sum()
}

/// A cooked file (`FCKD` header + payload) whose payload is `noise` random bytes then `zeros` zero
/// bytes.
fn cooked_file(id: AssetId, rng: &mut Rng, noise: usize, zeros: usize) -> Vec<u8> {
    tagged_cooked_file(id, rng, noise, zeros, 1_000_000)
}

/// [`cooked_file`] with `tag` stored in the header's importer version (the rewrite test's version).
fn tagged_cooked_file(id: AssetId, rng: &mut Rng, noise: usize, zeros: usize, tag: u32) -> Vec<u8> {
    let mut payload = vec![0u8; noise + zeros];
    rng.fill(&mut payload[..noise]);
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: TypePathHash::of(TYPE_PATH),
        asset_id: id,
        importer_version: tag,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, &payload).expect("encode cooked")
}

/// A cooked file whose compressed entry stores exactly as many bytes as it decompresses to (the
/// boundary the old reader treated as "stored raw"), or `None` if no zero-run length in range hits it.
fn boundary_cooked_file(id: AssetId, rng: &mut Rng, total: usize) -> Option<Vec<u8>> {
    let seed = rng.next();
    // Each extra zero byte (in place of a noise byte) saves about one stored byte, so sweeping the
    // zero-run length walks the stored length across `size` one byte at a time.
    for zeros in 16..total.min(4096) {
        let mut r = Rng::new(seed);
        let file = cooked_file(id, &mut r, total - zeros, zeros);
        if stored_compressed_len(&file) == file.len() {
            return Some(file);
        }
    }
    None
}

/// One asset's cooked bytes, picked from the shapes the gate and real cooks produce.
fn random_asset(id: AssetId, rng: &mut Rng) -> Vec<u8> {
    let block = FPAK_COMPRESS_BLOCK_SIZE;
    let (noise, zeros) = match rng.below(10) {
        0 => (rng_len(rng, 0, 48), 0),
        1 => return exact_len_cooked_file(id, rng, block),
        2 => return exact_len_cooked_file(id, rng, 2 * block),
        3 => return exact_len_cooked_file(id, rng, block + 1),
        4 => return exact_len_cooked_file(id, rng, block - 1),
        5 => {
            let total = rng_len(rng, 200, 20_000);
            if let Some(file) = boundary_cooked_file(id, rng, total) {
                return file;
            }
            (total, 0)
        }
        6 => (0, rng_len(rng, 1, 3 * block)),
        7 => {
            let noise = rng_len(rng, 1, 2 * block);
            (noise, rng_len(rng, 0, noise))
        }
        _ => (rng_len(rng, 1, 40_000), rng_len(rng, 0, 2_000)),
    };
    cooked_file(id, rng, noise, zeros)
}

/// A cooked file of exactly `total` bytes (header included): exact block multiples and block ± 1.
fn exact_len_cooked_file(id: AssetId, rng: &mut Rng, total: usize) -> Vec<u8> {
    // The header's varints depend on the payload length; find the payload length that fits.
    let overhead = (0..total)
        .find(|&h| {
            let header = CookedHeader {
                format_version: COOKED_FORMAT_VERSION,
                type_tag: TypePathHash::of(TYPE_PATH),
                asset_id: id,
                importer_version: 1_000_000,
                dependencies: Vec::new(),
                payload_len: (total - h) as u64,
            };
            postcard::to_allocvec(&header).is_ok_and(|b| b.len() + 4 == h)
        })
        .expect("a header overhead fits");
    let zeros = rng.below(64);
    let file = cooked_file(id, rng, total - overhead - zeros, zeros);
    assert_eq!(file.len(), total);
    file
}

fn rng_len(rng: &mut Rng, lo: usize, hi: usize) -> usize {
    lo + rng.below(hi - lo + 1)
}

/// Write `assets` into `dir/cooked` and pack them into `out`.
fn cook_and_pack(dir: &Path, out: &Path, assets: &[(AssetId, Vec<u8>)]) {
    let cooked = dir.join("cooked");
    std::fs::create_dir_all(&cooked).expect("cooked dir");
    let mut entries = Vec::new();
    let mut manifest = Vec::new();
    for (i, (id, bytes)) in assets.iter().enumerate() {
        std::fs::write(cooked.join(cooked_file_name(*id)), bytes).expect("write cooked");
        entries.push((*id, TypePathHash::of(TYPE_PATH)));
        manifest.push(ManifestEntry {
            path: format!("assets/stress_{i}.terrain.layers.ron"),
            id: *id,
            type_path: TYPE_PATH.to_string(),
        });
    }
    let manifest = AssetManifest { entries: manifest };
    write_pak(out, &cooked, &entries, &manifest, &PakWriteOptions::default()).expect("write pak");
}

/// Dump a failing entry (TOC record, stored bytes, expected payload) and return where it went.
fn dump_failure(dir: &Path, pak: &PakReader, id: AssetId, expected: &[u8], what: &str) -> PathBuf {
    let dump = dir.join(format!("failure-{id}"));
    let _ = std::fs::create_dir_all(&dump);
    let _ = std::fs::write(
        dump.join("entry.txt"),
        format!(
            "{what}\npak: {}\nentry: {:#?}\nheader: {:#?}\nall entries: {:#?}\n",
            pak.path,
            pak.entry(id),
            pak.header,
            pak.toc.entries
        ),
    );
    if let Ok(raw) = pak.read_raw(id) {
        let _ = std::fs::write(dump.join("stored.bin"), raw);
    }
    let _ = std::fs::write(dump.join("expected.bin"), expected);
    dump
}

/// Read every asset back through the mapped reader and require the exact cooked bytes.
fn verify(dir: &Path, pak_path: &Path, assets: &[(AssetId, Vec<u8>)], seed: u64) {
    let pak =
        PakReader::open(pak_path).unwrap_or_else(|e| panic!("seed {seed}: open {}: {e}", pak_path.display()));
    assert!(pak.is_mapped());
    for (id, expected) in assets {
        let what = match pak.read_decompressed(*id) {
            Ok(bytes) if &bytes == expected => match decode_cooked_file(&bytes) {
                Ok(_) => continue,
                Err(e) => format!("cooked decode: {e}"),
            },
            Ok(bytes) => format!(
                "wrong bytes: got {} (head {:02x?}), expected {} (head {:02x?})",
                bytes.len(),
                &bytes[..bytes.len().min(16)],
                expected.len(),
                &expected[..expected.len().min(16)]
            ),
            Err(e) => format!("read error: {e}"),
        };
        let dump = dump_failure(dir, &pak, *id, expected, &what);
        panic!("seed {seed}: asset {id}: {what} (dump: {})", dump.display());
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("regia-pak-stress-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn threads() -> usize {
    env_usize(
        "REGIA_PAK_STRESS_THREADS",
        std::thread::available_parallelism().map_or(4, |n| n.get()),
    )
}

/// The writer's stored layout lands exactly on `size` for some payloads; the reader must still
/// decompress them (the gate's `Cooked(BadMagic)`). Deterministic: no threads, fixed seeds.
#[test]
fn compressed_entry_storing_exactly_its_size_round_trips() {
    let dir = scratch("boundary");
    let mut hits = 0;
    for seed in 0..64u64 {
        let mut rng = Rng::new(seed);
        let id = AssetId::new_random();
        let total = rng_len(&mut rng, 300, 12_000);
        let Some(file) = boundary_cooked_file(id, &mut rng, total) else {
            continue;
        };
        hits += 1;
        let case = dir.join(format!("seed-{seed}"));
        let assets = vec![(id, file)];
        cook_and_pack(&case, &case.join("base.fpak"), &assets);
        verify(&case, &case.join("base.fpak"), &assets, seed);
    }
    assert!(hits >= 16, "the boundary generator found only {hits} payloads");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Many threads cook, pack and read paks at once over every payload shape.
#[test]
fn parallel_cook_pack_read_soak() {
    let iters = env_usize("REGIA_PAK_STRESS_ITERS", 40);
    let threads = threads();
    let base = std::process::id() as u64 ^ 0xFE44_17E0;
    let dir = scratch("soak");
    let done = AtomicU64::new(0);
    std::thread::scope(|s| {
        for t in 0..threads {
            let dir = dir.join(format!("t{t}"));
            let done = &done;
            s.spawn(move || {
                for i in 0..iters {
                    let seed = base.wrapping_mul(1_000_003) ^ ((t as u64) << 32) ^ i as u64;
                    let mut rng = Rng::new(seed);
                    let assets: Vec<_> = (0..1 + rng.below(8))
                        .map(|_| {
                            let id = AssetId::new_random();
                            (id, random_asset(id, &mut rng))
                        })
                        .collect();
                    let case = dir.join(format!("i{i}"));
                    let pak = case.join("base.fpak");
                    cook_and_pack(&case, &pak, &assets);
                    verify(&case, &pak, &assets, seed);
                    let _ = std::fs::remove_dir_all(&case);
                    done.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    eprintln!(
        "pak stress: {} paks cooked, packed and read back on {threads} threads",
        done.load(Ordering::Relaxed)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// One thread re-packs the same pak path over and over while readers open and read it: every open
/// must see one complete pak (the writer replaces the file atomically; a live mapping keeps the old
/// inode), never a half-written one or one truncated under the mapping.
#[test]
fn readers_never_see_a_pak_being_rewritten() {
    let rounds = env_usize("REGIA_PAK_STRESS_ITERS", 40).max(8) as u32;
    let dir = scratch("rewrite");
    let pak_path = dir.join("live.fpak");
    // Every version holds the same ids; each cooked header carries its version (importer version
    // field), so a reader regenerates the expected bytes and checks one open never mixes versions.
    let ids: Vec<AssetId> = (0..4).map(|_| AssetId::new_random()).collect();
    let version_assets = |v: u32| -> Vec<(AssetId, Vec<u8>)> {
        let mut rng = Rng::new(u64::from(v));
        ids.iter()
            .map(|id| {
                let noise = rng_len(&mut rng, 1_000, 2 * FPAK_COMPRESS_BLOCK_SIZE);
                let zeros = rng_len(&mut rng, 0, 4096);
                (*id, tagged_cooked_file(*id, &mut rng, noise, zeros, v))
            })
            .collect()
    };
    cook_and_pack(&dir.join("w"), &pak_path, &version_assets(0));
    let writing = AtomicBool::new(true);
    let reads = AtomicU64::new(0);
    std::thread::scope(|s| {
        let (writing, pak_path, ids, reads) = (&writing, &pak_path, &ids, &reads);
        let version_assets = &version_assets;
        let writer_dir = dir.join("w");
        s.spawn(move || {
            for v in 1..=rounds {
                cook_and_pack(&writer_dir, pak_path, &version_assets(v));
            }
            writing.store(false, Ordering::Release);
        });
        for _ in 0..threads().clamp(2, 16) {
            s.spawn(move || {
                let mut expected = std::collections::HashMap::new();
                while writing.load(Ordering::Acquire) {
                    let pak =
                        PakReader::open(pak_path).unwrap_or_else(|e| panic!("open during rewrite: {e}"));
                    let mut version = None;
                    for id in ids {
                        let bytes = pak
                            .read_decompressed(*id)
                            .unwrap_or_else(|e| panic!("read {id} during rewrite: {e}"));
                        let (header, _) = decode_cooked_file(&bytes)
                            .unwrap_or_else(|e| panic!("cooked decode of {id} during rewrite: {e}"));
                        let v = header.importer_version;
                        assert!(
                            version.is_none_or(|seen| seen == v),
                            "one open mixed versions {version:?} and {v}"
                        );
                        version = Some(v);
                        let assets = expected.entry(v).or_insert_with(|| version_assets(v));
                        assert!(
                            assets.iter().any(|(i, b)| i == id && *b == bytes),
                            "{id} does not match version {v} as written"
                        );
                    }
                    if expected.len() > 4 {
                        expected.clear();
                    }
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    assert!(
        reads.load(Ordering::Relaxed) > 0,
        "no reads overlapped the rewrites"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every truncation of a valid pak opens as an error or reads entries as errors — never a panic,
/// never different bytes.
#[test]
fn truncated_paks_are_errors_not_wrong_bytes() {
    let dir = scratch("truncate");
    let mut rng = Rng::new(0x7A11);
    let assets: Vec<_> = (0..3)
        .map(|_| {
            let id = AssetId::new_random();
            (id, cooked_file(id, &mut rng, 70_000, 3_000))
        })
        .collect();
    let pak_path = dir.join("full.fpak");
    cook_and_pack(&dir, &pak_path, &assets);
    let full = std::fs::read(&pak_path).expect("read pak");
    let cut_path = dir.join("cut.fpak");
    let cuts = (0..full.len()).step_by(997).chain([full.len() - 1, 59, 60, 61]);
    for cut in cuts {
        std::fs::write(&cut_path, &full[..cut]).expect("write cut");
        let Ok(pak) = PakReader::open(&cut_path) else {
            continue;
        };
        for (id, expected) in &assets {
            if let Ok(bytes) = pak.read_decompressed(*id) {
                assert_eq!(&bytes, expected, "cut at {cut}: {id} read different bytes");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
