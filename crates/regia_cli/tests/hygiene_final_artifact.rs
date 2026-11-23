// SPDX-License-Identifier: MPL-2.0
//! CT-16: the final generated package is inspected, not only the staging tree (ADR-8348).
//!
//! Every archive here is a miniature written by the test itself (ZIP, tar, tar.gz, `.fpak`);
//! no platform build runs. Nothing is extracted: the tests also check that no file appears.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use regia_cli::package_hygiene::{
    Allowance, ArtifactOptions, Policy, RECEIPT_SCHEMA, RECEIPT_VERSION, audit_artifact, check_artifact,
    check_tree, receipt_path, tool_version, verify_receipt, write_receipt,
};
use serde_json::Value;

const FORBIDDEN: &str = "--synthetic-developer-console";

struct Dir(PathBuf);

impl Dir {
    fn new(label: &str) -> anyhow::Result<Self> {
        let root = regia_test_utils::unique_temp_dir(label);
        std::fs::create_dir_all(&root)?;
        Ok(Self(root))
    }

    fn write(&self, name: &str, bytes: &[u8]) -> anyhow::Result<PathBuf> {
        let path = self.0.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        regia_test_utils::write_atomic(&path, bytes)?;
        Ok(path)
    }

    /// Every path under the root, sorted (to prove an audit created nothing).
    fn listing(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![self.0.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                out.push(path);
            }
        }
        out.sort();
        out
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn policy() -> Policy {
    Policy {
        forbidden_strings: vec![FORBIDDEN.into()],
        ..Policy::default()
    }
}

fn options() -> ArtifactOptions {
    ArtifactOptions::default()
}

// ---------------------------------------------------------------------------------------------
// Miniature archive writers (test fixtures, not production code).
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Entry {
    name: Vec<u8>,
    data: Vec<u8>,
    deflate: bool,
    flags: u16,
    method: Option<u16>,
    size: Option<u32>,
    crc: Option<u32>,
    local_name: Option<Vec<u8>>,
}

fn entry(name: &str, data: &[u8]) -> Entry {
    Entry {
        name: name.as_bytes().to_vec(),
        data: data.to_vec(),
        deflate: true,
        flags: 0,
        method: None,
        size: None,
        crc: None,
        local_name: None,
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    crc.sum()
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).expect("in-memory deflate");
    encoder.finish().expect("in-memory deflate")
}

fn zip(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for e in entries {
        let payload = if e.deflate {
            deflate(&e.data)
        } else {
            e.data.clone()
        };
        let method = e.method.unwrap_or(if e.deflate { 8 } else { 0 });
        let crc = e.crc.unwrap_or_else(|| crc32(&e.data));
        let size = e.size.unwrap_or(e.data.len() as u32);
        let offset = out.len() as u32;
        let local_name = e.local_name.clone().unwrap_or_else(|| e.name.clone());
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        for v in [20u16, e.flags, method, 0, 0] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [crc, payload.len() as u32, size] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&(local_name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&local_name);
        out.extend_from_slice(&payload);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        for v in [20u16, 20, e.flags, method, 0, 0] {
            central.extend_from_slice(&v.to_le_bytes());
        }
        for v in [crc, payload.len() as u32, size] {
            central.extend_from_slice(&v.to_le_bytes());
        }
        for v in [e.name.len() as u16, 0, 0, 0, 0] {
            central.extend_from_slice(&v.to_le_bytes());
        }
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(&e.name);
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    for v in [0u16, 0, entries.len() as u16, entries.len() as u16] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn tar_header(name: &str, size: usize, typeflag: u8) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..107].copy_from_slice(b"0000644");
    h[108..115].copy_from_slice(b"0001750");
    h[116..123].copy_from_slice(b"0001750");
    h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
    h[136..147].copy_from_slice(b"00000000000");
    h[156] = typeflag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[265..270].copy_from_slice(b"build");
    h[297..302].copy_from_slice(b"build");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    h
}

fn tar(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in files {
        let typeflag = if name.ends_with('/') { b'5' } else { b'0' };
        out.extend_from_slice(&tar_header(name, data.len(), typeflag));
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(512), 0);
    }
    out.resize(out.len() + 1024, 0);
    out.resize(out.len().next_multiple_of(10240), 0);
    out
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).expect("in-memory gzip");
    encoder.finish().expect("in-memory gzip")
}

/// A one-entry `.fpak` whose decoded payload is `content`, written with the real pak writer.
fn fpak(content: &[u8]) -> anyhow::Result<Vec<u8>> {
    use regia_asset::{AssetId, AssetManifest, ManifestEntry};
    use regia_asset_pipeline::pak_writer::{PakWriteOptions, write_pak};
    use regia_core::id::TypePathHash;
    let cooked = Dir::new("final-artifact-cooked")?;
    let id = AssetId::NIL;
    cooked.write(&regia_asset::cooked_file_name(id), content)?;
    let manifest = AssetManifest {
        entries: vec![ManifestEntry {
            path: "levels/arena".into(),
            id,
            type_path: "opaque".into(),
        }],
    };
    let out = cooked.0.join("out.fpak");
    write_pak(
        &out,
        &cooked.0,
        &[(id, TypePathHash(0))],
        &manifest,
        &PakWriteOptions::default(),
    )?;
    Ok(std::fs::read(out)?)
}

fn reason(error: &regia_cli::package_hygiene::AuditError) -> String {
    error
        .report
        .as_ref()
        .map(|report| report.incomplete_reason.clone())
        .unwrap_or_default()
}

/// An uncompressed `.fpak` with `count` incompressible entries of `len` bytes; `marker` is
/// planted in the last one.
fn fpak_many(count: usize, len: usize, marker: &[u8]) -> anyhow::Result<Vec<u8>> {
    use regia_asset::{AssetId, AssetManifest, ManifestEntry};
    use regia_asset_pipeline::pak_writer::{PakWriteOptions, write_pak};
    use regia_core::id::TypePathHash;
    let cooked = Dir::new("final-artifact-cooked-many")?;
    let mut ids = Vec::new();
    let mut manifest = AssetManifest { entries: Vec::new() };
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for i in 0..count {
        let id = AssetId::sub_asset(AssetId::NIL, &format!("entry{i}"));
        let mut bytes: Vec<u8> = (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 32) as u8 | 0x80
            })
            .collect();
        if i + 1 == count {
            bytes[len / 2..len / 2 + marker.len()].copy_from_slice(marker);
        }
        cooked.write(&regia_asset::cooked_file_name(id), &bytes)?;
        ids.push((id, TypePathHash(0)));
        manifest.entries.push(ManifestEntry {
            path: format!("levels/part{i}"),
            id,
            type_path: "opaque".into(),
        });
    }
    let out = cooked.0.join("many.fpak");
    let options = PakWriteOptions {
        compress: false,
        ..PakWriteOptions::default()
    };
    write_pak(&out, &cooked.0, &ids, &manifest, &options)?;
    Ok(std::fs::read(out)?)
}

/// A clean desktop-style package: binary, notices and a pak, under one top directory.
fn clean_package() -> anyhow::Result<Vec<Entry>> {
    Ok(vec![
        entry("Game/", b""),
        entry("Game/game", b"\x7fELF-not-really-a-binary"),
        entry("Game/licenses/NOTICE.txt", b"Third-party notices"),
        entry("Game/content/base.fpak", &fpak(b"cooked level bytes")?),
    ])
}

// ---------------------------------------------------------------------------------------------
// Acceptance tests.
// ---------------------------------------------------------------------------------------------

#[test]
fn forbidden_string_added_after_staging_fails_the_final_hook() -> anyhow::Result<()> {
    let stage = Dir::new("final-artifact-stage")?;
    stage.write("game", b"\x7fELF-not-really-a-binary")?;
    stage.write("licenses/NOTICE.txt", b"Third-party notices")?;
    // The staging audit passes: the forbidden string is not staged.
    check_tree(&stage.0, &policy())?;

    let out = Dir::new("final-artifact-out")?;
    let mut entries = clean_package()?;
    // Introduced by a later packaging step, compressed so raw archive bytes do not show it.
    entries.push(entry(
        "Game/late/injected.cfg",
        format!("args={FORBIDDEN}").as_bytes(),
    ));
    let artifact = out.write("game-windows.zip", &zip(&entries))?;
    assert!(
        !std::fs::read(&artifact)?
            .windows(FORBIDDEN.len())
            .any(|w| w == FORBIDDEN.as_bytes()),
        "fixture must hide the string behind deflate"
    );
    let error = check_artifact(&artifact, &policy(), &options()).expect_err("final hook rejects");
    assert_eq!(error.code, "package_hygiene_failed");
    let report = error.report.as_ref().expect("completed report");
    assert!(report.complete);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].rule, "forbidden_string_1");
    assert_eq!(
        report.findings[0].path,
        "game-windows.zip::Game/late/injected.cfg"
    );
    assert_eq!(report.findings[0].offset, 5);
    assert!(
        !error.to_string().contains(FORBIDDEN),
        "no matched text in errors"
    );

    // The same injection in a tar.gz and inside a nested pak entry is found too.
    let tarball = out.write(
        "game-linux.tar.gz",
        &gzip(&tar(&[
            ("Game/", b""),
            ("Game/game", b"binary"),
            ("Game/late.txt", FORBIDDEN.as_bytes()),
        ])),
    )?;
    let error = check_artifact(&tarball, &policy(), &options()).expect_err("tar.gz rejects");
    assert_eq!(error.code, "package_hygiene_failed");
    let paked = out.write(
        "game-pak.zip",
        &zip(&[entry("Game/content/late.fpak", &fpak(FORBIDDEN.as_bytes())?)]),
    )?;
    let error = check_artifact(&paked, &policy(), &options()).expect_err("nested pak rejects");
    let report = error.report.as_ref().expect("report");
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.path.starts_with("game-pak.zip::Game/content/late.fpak::")),
        "decoded pak entry location: {:?}",
        report.findings
    );
    assert_eq!(report.pak_entries_scanned, 1);
    Ok(())
}

#[test]
fn changed_artifact_hash_invalidates_a_receipt() -> anyhow::Result<()> {
    let out = Dir::new("final-artifact-receipt")?;
    // Stored entries, so an edit of equal length keeps the archive size.
    let stored = |notice: &[u8]| -> anyhow::Result<Vec<u8>> {
        let mut entries = clean_package()?;
        entries[2] = entry("Game/licenses/NOTICE.txt", notice);
        for e in &mut entries {
            e.deflate = false;
        }
        Ok(zip(&entries))
    };
    let artifact = out.write("game.zip", &stored(b"Third-party notices")?)?;
    let receipt = check_artifact(&artifact, &policy(), &options())?;
    let sidecar = write_receipt(&artifact, &receipt)?;
    assert_eq!(sidecar, receipt_path(&artifact));
    let json = std::fs::read_to_string(&sidecar)?;
    verify_receipt(&artifact, &json, &policy())?;

    // Same size, different bytes: only the hash can tell.
    let original = std::fs::read(&artifact)?;
    let changed = stored(b"Third-party Notices")?;
    assert_eq!(changed.len(), original.len(), "fixture keeps the size");
    regia_test_utils::write_atomic(&artifact, &changed)?;
    let error = verify_receipt(&artifact, &json, &policy()).expect_err("hash changed");
    assert_eq!(
        (error.code, error.location.as_str()),
        ("package_hygiene_receipt", "artifact_hash")
    );

    regia_test_utils::write_atomic(&artifact, &original)?;
    verify_receipt(&artifact, &json, &policy())?;
    let mut longer = original.clone();
    longer.push(0);
    regia_test_utils::write_atomic(&artifact, &longer)?;
    let error = verify_receipt(&artifact, &json, &policy()).expect_err("size changed");
    assert_eq!(error.location, "artifact_bytes");
    regia_test_utils::write_atomic(&artifact, &original)?;

    // The policy, container settings, tool and schema are bound as well.
    let mut other = policy();
    other.forbidden_strings.push("another".into());
    assert_eq!(
        verify_receipt(&artifact, &json, &other)
            .expect_err("policy")
            .location,
        "policy_digest"
    );
    let strict = Policy {
        strict_containers: true,
        ..policy()
    };
    assert_eq!(
        verify_receipt(&artifact, &json, &strict)
            .expect_err("strict")
            .location,
        "strict_containers"
    );
    let shallow = Policy {
        max_container_depth: 1,
        ..policy()
    };
    assert_eq!(
        verify_receipt(&artifact, &json, &shallow)
            .expect_err("depth")
            .location,
        "max_container_depth"
    );
    for (field, value) in [
        ("tool", Value::from("regia_cli 0.0.0-other")),
        ("version", Value::from(RECEIPT_VERSION + 1)),
        ("schema", Value::from("other.schema")),
    ] {
        let mut edited: Value = serde_json::from_str(&json)?;
        edited[field] = value;
        let error = verify_receipt(&artifact, &edited.to_string(), &policy()).expect_err("bound field");
        assert_eq!(error.location, field);
    }
    let error = verify_receipt(&artifact, "{not json", &policy()).expect_err("malformed");
    assert_eq!(error.location, "receipt");
    Ok(())
}

#[test]
fn nested_fpak_and_zip_limits_fail_closed() -> anyhow::Result<()> {
    let out = Dir::new("final-artifact-limits")?;

    // Depth: zip in zip in zip in zip in zip; the default opens three nested levels.
    let mut nested = zip(&[entry("leaf.txt", b"leaf")]);
    for level in 0..4 {
        nested = zip(&[entry(&format!("level{level}.zip"), &nested)]);
    }
    let deep = out.write("deep.zip", &nested)?;
    let error = check_artifact(&deep, &policy(), &options()).expect_err("too deep");
    assert_eq!(
        (error.code, reason(&error).as_str()),
        ("package_hygiene_limit", "depth")
    );
    let deeper = Policy {
        max_container_depth: 4,
        ..policy()
    };
    let receipt = check_artifact(&deep, &deeper, &options())?;
    assert_eq!(receipt.coverage.max_depth, 4);
    assert_eq!(receipt.coverage.containers.len(), 5);
    let zero = Policy {
        max_container_depth: 0,
        ..policy()
    };
    let error = check_artifact(&deep, &zero, &options()).expect_err("no nesting at all");
    assert_eq!(reason(&error), "depth");
    let invalid = Policy {
        max_container_depth: 17,
        ..policy()
    };
    assert_eq!(
        check_artifact(&deep, &invalid, &options()).expect_err("cap").code,
        "package_hygiene_config"
    );

    // Entry count inside a nested ZIP.
    let many: Vec<Entry> = (0..40).map(|i| entry(&format!("f{i}.txt"), b"x")).collect();
    let wide = out.write("wide.zip", &zip(&[entry("inner.zip", &zip(&many))]))?;
    let capped = Policy {
        max_entries: 20,
        ..policy()
    };
    let error = check_artifact(&wide, &capped, &options()).expect_err("entry cap");
    assert_eq!(
        (error.code, reason(&error).as_str()),
        ("package_hygiene_limit", "entry_count")
    );

    // Expanded bytes: a deflate entry that inflates past its declared size.
    let mut bomb = entry("bomb.bin", &vec![0u8; 64 << 10]);
    bomb.size = Some(1024);
    bomb.crc = Some(crc32(&[0u8; 1024]));
    let bombed = out.write("bomb.zip", &zip(&[bomb]))?;
    let error = check_artifact(&bombed, &policy(), &options()).expect_err("bomb");
    assert_eq!(
        (error.code, reason(&error).as_str()),
        ("package_hygiene_limit", "decompression_bomb")
    );
    // A declared size over the per-entry ceiling is refused before decoding.
    let big = out.write("big.zip", &zip(&[entry("big.bin", &vec![7u8; 8 << 10])]))?;
    let small_files = Policy {
        max_file_bytes: 4 << 10,
        ..policy()
    };
    let error = check_artifact(&big, &small_files, &options()).expect_err("entry bytes");
    assert_eq!(error.code, "package_hygiene_limit");
    // Total expanded bytes across nested entries.
    let total = Policy {
        max_total_bytes: 12 << 10,
        ..policy()
    };
    let two = out.write(
        "two.zip",
        &zip(&[
            entry("a.bin", &vec![1u8; 8 << 10]),
            entry("b.bin", &vec![2u8; 8 << 10]),
        ]),
    )?;
    let error = check_artifact(&two, &total, &options()).expect_err("total bytes");
    assert_eq!(
        (error.code, reason(&error).as_str()),
        ("package_hygiene_limit", "total_bytes")
    );

    // A nested pak whose decoded entry does not fit the memory budget fails closed.
    let tight = Policy {
        scan_secrets: false,
        max_memory_bytes: 96 << 10,
        scan_window_bytes: 4 << 10,
        ..policy()
    };
    let pak_zip = out.write(
        "pak.zip",
        &zip(&[entry("content/huge.fpak", &fpak(&vec![b'p'; 160 << 10])?)]),
    )?;
    let error = check_artifact(&pak_zip, &tight, &options()).expect_err("nested pak budget");
    assert_eq!(error.code, "package_hygiene_limit");
    assert!(
        matches!(
            reason(&error).as_str(),
            "nested_limit" | "decompression_bomb" | "memory_budget"
        ),
        "unexpected reason {}",
        reason(&error)
    );
    let peak = error.report.as_ref().map_or(0, |r| r.peak_memory_bytes);
    assert!(peak <= tight.max_memory_bytes, "peak {peak} within the budget");

    // A malformed nested pak is a pak failure, never an opaque pass.
    let broken = out.write(
        "broken.zip",
        &zip(&[entry("content/x.bin", b"FPAKbroken-pak-bytes")]),
    )?;
    assert_eq!(
        check_artifact(&broken, &policy(), &options())
            .expect_err("bad pak")
            .code,
        "package_hygiene_pak"
    );
    Ok(())
}

#[test]
fn nested_pak_larger_than_memory_is_spooled_scanned_and_removed() -> anyhow::Result<()> {
    let out = Dir::new("final-artifact-spool")?;
    let tight = Policy {
        scan_secrets: false,
        max_memory_bytes: 48 << 10,
        scan_window_bytes: 4 << 10,
        ..policy()
    };
    let clean = fpak_many(4, 16 << 10, b"")?;
    assert!(
        clean.len() as u64 > tight.max_memory_bytes,
        "the pak cannot be held in memory"
    );
    let artifact = out.write("big.zip", &zip(&[entry("content/big.fpak", &clean)]))?;
    let receipt = check_artifact(&artifact, &tight, &options())?;
    assert_eq!(receipt.hygiene.pak_entries_scanned, 4);
    assert!(
        receipt
            .coverage
            .containers
            .iter()
            .any(|c| (c.kind.as_str(), c.depth, c.entries) == ("fpak", 1, 4))
    );
    assert!(receipt.hygiene.peak_memory_bytes <= tight.max_memory_bytes);
    let spools = |dir: &Dir| {
        dir.listing()
            .iter()
            .filter(|p| p.to_string_lossy().contains(".regia-hygiene-spool"))
            .count()
    };
    assert_eq!(spools(&out), 0, "spool removed after a pass");

    let planted = fpak_many(4, 16 << 10, FORBIDDEN.as_bytes())?;
    let artifact = out.write("planted.zip", &zip(&[entry("content/big.fpak", &planted)]))?;
    let error = check_artifact(&artifact, &tight, &options()).expect_err("found in a spooled pak");
    assert_eq!(error.code, "package_hygiene_failed");
    let report = error.report.as_ref().expect("report");
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.path.starts_with("planted.zip::content/big.fpak::")),
        "spooled pak entry location: {:?}",
        report.findings
    );
    assert_eq!(spools(&out), 0, "spool removed after a rejection");

    // A tar.gz can only be streamed; the same pak is spooled from the stream.
    let tarball = out.write(
        "planted.tar.gz",
        &gzip(&tar(&[("Game/content/big.fpak", &planted)])),
    )?;
    let error = check_artifact(&tarball, &tight, &options()).expect_err("found through tar.gz");
    assert_eq!(error.code, "package_hygiene_failed");
    assert_eq!(spools(&out), 0);

    // An unwritable spool directory fails closed instead of skipping the pak.
    let missing = ArtifactOptions {
        spool_dir: Some(out.0.join("missing-dir")),
        ..ArtifactOptions::default()
    };
    let error = check_artifact(&artifact, &tight, &missing).expect_err("no spool");
    assert_eq!(
        (error.code, reason(&error).as_str()),
        ("package_hygiene_limit", "nested_limit")
    );
    Ok(())
}

#[test]
fn duplicate_and_traversal_names_never_create_files() -> anyhow::Result<()> {
    let out = Dir::new("final-artifact-names")?;
    let parent = out.0.join("sub");
    std::fs::create_dir_all(&parent)?;
    let mut cases: Vec<(String, Vec<u8>, &str, &str)> = Vec::new();
    for name in [
        "../escape.txt",
        "/abs.txt",
        "a/../../escape.txt",
        "a\\..\\escape.txt",
        "C:/escape.txt",
        "a//escape.txt",
        "./../escape.txt",
        "a/./escape.txt",
    ] {
        cases.push((
            format!("zip {name}"),
            zip(&[entry("ok.txt", b"ok"), entry(name, b"payload")]),
            "package_hygiene_path",
            "traversal_name",
        ));
        cases.push((
            format!("tar {name}"),
            tar(&[("ok.txt", b"ok"), (name, b"payload")]),
            "package_hygiene_path",
            "traversal_name",
        ));
    }
    for (first, second) in [
        ("dir/A.txt", "dir/a.txt"),
        ("same.txt", "same.txt"),
        ("./x.txt", "x.txt"),
    ] {
        cases.push((
            format!("zip {first} {second}"),
            zip(&[entry(first, b"one"), entry(second, b"two")]),
            "package_hygiene_archive",
            "duplicate_name",
        ));
        cases.push((
            format!("tar {first} {second}"),
            tar(&[(first, b"one"), (second, b"two")]),
            "package_hygiene_archive",
            "duplicate_name",
        ));
    }
    let mut link = tar(&[("ok.txt", b"ok")]);
    let header = tar_header("link-to-outside", 0, b'2');
    link.splice(0..0, header);
    cases.push(("tar symlink".into(), link, "package_hygiene_path", "path"));
    let mut mismatch = entry("visible.txt", b"payload");
    mismatch.local_name = Some(b"hidden.exe!".to_vec());
    cases.push((
        "zip local name mismatch".into(),
        zip(&[mismatch]),
        "package_hygiene_archive",
        "archive",
    ));
    let mut bad_crc = entry("crc.txt", b"payload");
    bad_crc.crc = Some(1);
    cases.push((
        "zip crc".into(),
        zip(&[bad_crc]),
        "package_hygiene_archive",
        "archive",
    ));
    let mut truncated = zip(&[entry("cut.txt", b"payload payload payload")]);
    truncated.truncate(truncated.len() - 30);
    cases.push((
        "zip truncated".into(),
        truncated,
        "package_hygiene_archive",
        "archive",
    ));

    // A backslash separator (Windows PowerShell 5.1 archives) is a separator, not an error.
    let windows = out.0.join("windows.zip");
    regia_test_utils::write_atomic(&windows, &zip(&[entry("Game\\bin\\game.exe", b"exe")]))?;
    let receipt = check_artifact(&windows, &policy(), &options())?;
    assert_eq!(receipt.coverage.archive_entries, 1);

    for (index, (label, bytes, code, why)) in cases.into_iter().enumerate() {
        let ext = if label.starts_with("tar") { "tar" } else { "zip" };
        let artifact = parent.join(format!("case{index}.{ext}"));
        regia_test_utils::write_atomic(&artifact, &bytes)?;
        let before = out.listing();
        let error = check_artifact(&artifact, &policy(), &options()).expect_err(&label);
        assert_eq!((error.code, reason(&error).as_str()), (code, why), "{label}");
        assert!(
            !error.to_string().contains("escape"),
            "{label}: names are not echoed"
        );
        assert_eq!(out.listing(), before, "{label}: the audit created no file");
        assert!(!out.0.join("escape.txt").exists() && !parent.join("escape.txt").exists());
        assert!(!Path::new("/abs.txt").exists());
    }
    Ok(())
}

#[test]
fn encrypted_and_unknown_payload_coverage_is_explicit() -> anyhow::Result<()> {
    let out = Dir::new("final-artifact-coverage")?;
    let mut encrypted = entry("secret/blob.bin", b"ciphertext-ish bytes");
    encrypted.flags = 1;
    let mut odd_method = entry("odd/method.bin", b"lzma-ish bytes");
    odd_method.deflate = false;
    odd_method.method = Some(14);
    let zstd = [&[0x28u8, 0xb5, 0x2f, 0xfd][..], b"opaque frame"].concat();
    let artifact = out.write(
        "game.apk",
        &zip(&[
            entry("assets/plain.txt", b"plain"),
            encrypted,
            odd_method,
            entry("assets/archive.zst", &zstd),
        ]),
    )?;
    let receipt = check_artifact(&artifact, &policy(), &options())?;
    assert!(receipt.hygiene.complete, "the scan itself finished");
    assert!(!receipt.coverage.complete, "coverage is explicitly partial");
    let listed: Vec<(&str, &str, &str)> = receipt
        .coverage
        .uninspected
        .iter()
        .map(|u| (u.path.as_str(), u.kind.as_str(), u.reason.as_str()))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("game.apk::secret/blob.bin", "zip_entry", "encrypted"),
            ("game.apk::odd/method.bin", "zip_entry", "compression_method"),
            ("game.apk::assets/archive.zst", "zstd", "unsupported_container"),
        ]
    );
    assert!(
        receipt
            .coverage
            .uninspected
            .iter()
            .all(|u| u.location_id.starts_with("location-"))
    );
    assert!(receipt.coverage.supported.iter().any(|kind| kind == "zip"));

    // Strict policy refuses the same artifact and names the first uninspected location.
    let strict = Policy {
        strict_containers: true,
        ..policy()
    };
    let error = check_artifact(&artifact, &strict, &options()).expect_err("strict refuses");
    assert_eq!(error.code, "package_hygiene_uninspected");
    assert_eq!(error.location, "game.apk::secret/blob.bin");
    assert!(error.location_id.is_some());

    // The raw bytes of an uninspected stored entry are still scanned as container content.
    let mut hidden = entry("x/enc.bin", FORBIDDEN.as_bytes());
    hidden.deflate = false;
    hidden.flags = 1;
    let leaky = out.write("leaky.zip", &zip(&[hidden]))?;
    let error = check_artifact(&leaky, &policy(), &options()).expect_err("raw bytes scanned");
    assert_eq!(error.code, "package_hygiene_failed");
    assert_eq!(
        error.report.as_ref().expect("report").findings[0].path,
        "leaky.zip"
    );

    // An AppImage-shaped artifact is opaque bytes plus an uninspected squashfs payload.
    let mut appimage = b"\x7fELF\x02\x01\x01\0AI\x02".to_vec();
    appimage.resize(600, 0);
    let image = out.write("game-x86_64.AppImage", &appimage)?;
    let receipt = check_artifact(&image, &policy(), &options())?;
    assert_eq!(receipt.artifact_kind, "appimage");
    assert_eq!(receipt.coverage.uninspected[0].reason, "unsupported_container");
    Ok(())
}

#[test]
fn a_valid_package_yields_one_deterministic_json_receipt() -> anyhow::Result<()> {
    let out = Dir::new("final-artifact-valid")?;
    let artifact = out.write("game-windows.zip", &zip(&clean_package()?))?;
    let first = check_artifact(&artifact, &policy(), &options())?;
    let second = check_artifact(&artifact, &policy(), &options())?;
    assert_eq!(first, second);
    assert_eq!(first.to_json()?, second.to_json()?);
    assert_eq!(first.schema, RECEIPT_SCHEMA);
    assert_eq!(first.version, RECEIPT_VERSION);
    assert_eq!(first.tool, tool_version());
    assert_eq!(first.artifact, "game-windows.zip");
    assert_eq!(first.artifact_kind, "zip");
    assert!(first.artifact_hash.starts_with("b3:") && first.artifact_hash.len() == 67);
    assert_eq!(first.artifact_bytes, std::fs::metadata(&artifact)?.len());
    assert_eq!(first.policy_digest, first.hygiene.policy_digest);
    assert!(first.coverage.complete && first.hygiene.complete);
    assert!(first.hygiene.findings.is_empty());
    let kinds: Vec<(&str, u32, u64)> = first
        .coverage
        .containers
        .iter()
        .map(|c| (c.kind.as_str(), c.depth, c.entries))
        .collect();
    assert_eq!(kinds, vec![("fpak", 1, 1), ("zip", 0, 4)]);
    assert_eq!(first.coverage.archive_entries, 4);
    assert_eq!(first.hygiene.pak_entries_scanned, 1);
    // Bounded work is a deterministic counter, not a timing.
    assert!(first.hygiene.work > 0 && first.hygiene.work <= first.hygiene.limits.max_work);
    assert!(first.hygiene.peak_memory_bytes <= first.hygiene.limits.max_memory_bytes);

    let before = out.listing();
    let sidecar = write_receipt(&artifact, &first)?;
    let after = out.listing();
    assert_eq!(after.len(), before.len() + 1, "exactly one receipt file");
    assert!(after.contains(&sidecar));
    assert!(
        sidecar
            .to_string_lossy()
            .ends_with("game-windows.zip.hygiene-receipt.json")
    );
    let text = std::fs::read_to_string(&sidecar)?;
    assert_eq!(text, first.to_json()?);
    assert!(!text.contains(&*out.0.to_string_lossy()), "no absolute paths");
    let value: Value = serde_json::from_str(&text)?;
    assert_eq!(value["coverage"]["complete"], true);
    // Rewriting gives byte-identical output.
    write_receipt(&artifact, &second)?;
    assert_eq!(std::fs::read_to_string(&sidecar)?, text);

    // tar.gz and directory artifacts get receipts too; a directory opens nested archives.
    let tarball = out.write(
        "game-linux.tar.gz",
        &gzip(&tar(&[
            ("Game/", b""),
            ("Game/game", b"binary"),
            ("Game/NOTICE.txt", b"notices"),
        ])),
    )?;
    let receipt = check_artifact(&tarball, &policy(), &options())?;
    assert_eq!(receipt.artifact_kind, "tar.gz");
    assert_eq!(receipt.coverage.archive_entries, 3);
    let tree = Dir::new("final-artifact-tree")?;
    tree.write("game", b"binary")?;
    tree.write("extras/inner.zip", &zip(&[entry("doc.txt", b"fine")]))?;
    let receipt = check_artifact(&tree.0, &policy(), &options())?;
    assert_eq!(receipt.artifact_kind, "directory");
    assert!(
        receipt
            .coverage
            .containers
            .iter()
            .any(|c| c.kind == "zip" && c.depth == 1)
    );
    tree.write("extras/late.zip", &zip(&[entry("doc.txt", FORBIDDEN.as_bytes())]))?;
    assert_eq!(
        check_artifact(&tree.0, &policy(), &options())
            .expect_err("nested zip in dir")
            .code,
        "package_hygiene_failed"
    );
    Ok(())
}

#[test]
fn allowances_match_staging_relative_paths_inside_the_archive() -> anyhow::Result<()> {
    let out = Dir::new("final-artifact-allow")?;
    let artifact = out.write(
        "game.zip",
        &zip(&[entry("Game/licenses/CREDITS.txt", FORBIDDEN.as_bytes())]),
    )?;
    let mut reviewed = policy();
    reviewed.allow.push(Allowance {
        rule: "forbidden_string_1".into(),
        path: "licenses/CREDITS.txt".into(),
        reason: "Synthetic reviewed attribution".into(),
        definition: None,
    });
    // Without the package prefix the staging-relative allowance does not apply.
    assert!(check_artifact(&artifact, &reviewed, &options()).is_err());
    let prefixed = ArtifactOptions {
        allow_prefixes: vec!["Game/".into()],
        ..ArtifactOptions::default()
    };
    let receipt = check_artifact(&artifact, &reviewed, &prefixed)?;
    assert_eq!(receipt.hygiene.allowed_findings, 1);
    Ok(())
}

#[test]
fn audit_artifact_reports_findings_without_rejecting() -> anyhow::Result<()> {
    let out = Dir::new("final-artifact-audit")?;
    let artifact = out.write("game.zip", &zip(&[entry("a.txt", FORBIDDEN.as_bytes())]))?;
    let receipt = audit_artifact(&artifact, &policy(), &options())?;
    assert_eq!(receipt.hygiene.findings.len(), 1);
    assert!(!receipt.to_json()?.contains(FORBIDDEN));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// CLI and agent workflow.
// ---------------------------------------------------------------------------------------------

fn project_with_artifact(label: &str, artifact_bytes: &[u8]) -> anyhow::Result<Dir> {
    let project = Dir::new(label)?;
    project.write(
        "Regia.toml",
        format!("[package_hygiene]\nforbidden_strings=['{FORBIDDEN}']\n").as_bytes(),
    )?;
    project.write("dist/game-windows.zip", artifact_bytes)?;
    Ok(project)
}

fn cli(project: &Path, extra: &[&str]) -> anyhow::Result<std::process::Output> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_regia"));
    cmd.args(["package", "--platform", "windows", "--project"])
        .arg(project)
        .arg("--audit-artifact")
        .arg(project.join("dist/game-windows.zip"))
        .args(extra)
        .arg("--json");
    Ok(cmd.output()?)
}

#[test]
fn cli_json_carries_the_receipt_and_verifies_it() -> anyhow::Result<()> {
    let project = project_with_artifact("final-artifact-cli", &zip(&clean_package()?))?;
    let before = project.listing();
    let output = cli(&project.0, &[])?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["ok"], true);
    assert_eq!(value["audit_only"], true);
    assert_eq!(value["final_audit"]["schema"], RECEIPT_SCHEMA);
    assert_eq!(value["final_audit"]["coverage"]["complete"], true);
    assert_eq!(project.listing(), before, "audit-only mode writes nothing");

    let receipt = project.write(
        "dist/game-windows.zip.hygiene-receipt.json",
        serde_json::to_string_pretty(&value["final_audit"])?.as_bytes(),
    )?;
    let receipt_arg = receipt.to_string_lossy().into_owned();
    let output = cli(&project.0, &["--verify-receipt", &receipt_arg])?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["receipt_verified"], true);

    project.write("dist/game-windows.zip", &zip(&[entry("x.txt", b"changed")]))?;
    let output = cli(&project.0, &["--verify-receipt", &receipt_arg])?;
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["error"]["code"], "package_hygiene_receipt");
    // The size differs too, and the cheaper size check answers first (the same-size case,
    // where only the hash can tell, is `changed_artifact_hash_invalidates_a_receipt`).
    assert_eq!(value["error"]["message"], "artifact_bytes");

    project.write(
        "dist/game-windows.zip",
        &zip(&[entry("late.txt", FORBIDDEN.as_bytes())]),
    )?;
    let output = cli(&project.0, &[])?;
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["error"]["code"], "package_hygiene_failed");
    assert!(value["hygiene"]["findings"][0]["location_id"].is_string());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(FORBIDDEN));
    Ok(())
}

#[test]
fn package_agent_audit_artifact_round_trip_supports_wait_until_and_expect() -> anyhow::Result<()> {
    use regia_agent::cook_package::{CliTally, RegiaCliProgram};
    use regia_agent::{AgentConfig, AgentPermissions, LocalReply, call_local};
    use regia_app::prelude::*;
    use serde_json::json;
    let project = project_with_artifact("final-artifact-agent", &zip(&clean_package()?))?;
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_agent::AgentPlugin));
    let mut config = AgentConfig::default();
    config.permissions.insert("build".into());
    app.world_mut()
        .insert_resource(AgentPermissions { config, source: None });
    app.world_mut().insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.0.clone()),
    });
    app.world_mut()
        .insert_resource(RegiaCliProgram(PathBuf::from(env!("CARGO_BIN_EXE_regia"))));
    let LocalReply::Done(escape) = call_local(
        app.world_mut(),
        "agent.package",
        json!({"audit_artifact":"../outside.zip", "platform":"windows", "dry_run":true}),
        None,
    ) else {
        panic!("preview must be immediate")
    };
    assert!(escape.is_err(), "paths outside the project are refused");
    let LocalReply::Done(reply) = call_local(
        app.world_mut(),
        "agent.package",
        json!({"audit_artifact":"dist/game-windows.zip", "platform":"windows", "dry_run":true}),
        None,
    ) else {
        panic!("preview must be immediate")
    };
    let reply = reply.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let args: Vec<String> = reply["command"]
        .as_array()
        .expect("command")
        .iter()
        .map(|s| s.as_str().expect("argument").to_owned())
        .collect();
    assert!(args.iter().any(|a| a == "--audit-artifact=dist/game-windows.zip"));
    let output = std::process::Command::new(&args[0])
        .args(&args[1..])
        .current_dir(&project.0)
        .output()?;
    let mut tally = CliTally::default();
    for line in String::from_utf8(output.stdout)?
        .lines()
        .chain(String::from_utf8(output.stderr)?.lines())
    {
        tally.feed(line);
    }
    let result = tally.result(output.status.code(), false, &args);
    assert_eq!(result["ok"], true, "{result}");
    let hash = result["report"]["final_audit"]["artifact_hash"]
        .as_str()
        .expect("receipt hash")
        .to_owned();
    assert_eq!(result["report"]["final_audit"]["coverage"]["complete"], true);
    // A local verification host publishes the job report to its event tap, as in the
    // hygiene_report_privacy workflow; the package job itself is unchanged.
    app.world_mut()
        .resource_mut::<regia_agent::AgentEventLog>()
        .push_host(0, "final_artifact_receipt", result);
    let condition = json!({"events_contain":{"kind":"final_artifact_receipt", "contains":hash}});
    let LocalReply::Pending(wait) = call_local(
        app.world_mut(),
        "play.wait_until",
        json!({"condition": condition, "timeout_ms":120000}),
        None,
    ) else {
        panic!("wait must be deferred")
    };
    app.world_mut().run_schedule(Last);
    let response = wait.try_recv()?;
    assert_eq!(response.result.expect("wait result")["satisfied"], true);
    let LocalReply::Done(reply) = call_local(
        app.world_mut(),
        "play.expect",
        json!({"assertions":[
            {"name":"receipt names its artifact hash", "condition":condition},
            {"name":"receipt has its schema", "condition":{"events_contain":{"kind":"final_artifact_receipt", "contains":RECEIPT_SCHEMA}}},
            {"name":"receipt has no absolute project path", "condition":{"not":{"events_contain":{"kind":"final_artifact_receipt", "contains":project.0.to_string_lossy()}}}}
        ]}),
        None,
    ) else {
        panic!("expect must be immediate")
    };
    assert_eq!(reply.map_err(|e| anyhow::anyhow!("{e:?}"))?["pass"], true);
    Ok(())
}
