// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests for release verify, diff, and the ordered upload plan.
//!
//! The assertions follow the release-verify, release-diff, and release-upload cases at pin
//! `c9513623f`. Temps use the process id and an atomic counter. No test sleeps or reads a clock.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use super::hash::{hex_encode, sha256_file};
use super::manifest::{ChunkManifest, ChunkRecord, write_manifest_files};
use super::scan::{Node, NodeKind, Unit};
use super::tar;
use super::{
    ChunkCodec, ChunkError, DEFAULT_MAX_CHUNK_BYTES, DiffOptions, GateGroup, ReleaseVerifyReport,
    UploadPlanOptions, VerifyOptions, ZstdCli, attempts_until_success, diff_release, directory_group,
    expand_patterns, format_markdown, format_size, format_text, gated_uploads, load_manifest_assets,
    load_plan_str, publish_gate_ok, repo_ok, sha256_hex, size_mismatch_message, skip_same_size, stable_size,
    tag_ok, upload_plan, verify_release,
};

fn check_set(report: &ReleaseVerifyReport) -> BTreeSet<&str> {
    report.problems.iter().map(|problem| problem.check).collect()
}

fn joined(report: &ReleaseVerifyReport) -> String {
    report
        .problems
        .iter()
        .map(|problem| problem.message.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "regia-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("temp");
        Self { path }
    }

    fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.path.join(rel)
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone)]
struct FileIn {
    path: String,
    sha256: Option<String>,
}

#[derive(Clone)]
struct FixChunk {
    name: String,
    group: String,
    asset: String,
    files: Vec<FileIn>,
    content_key: String,
}

#[derive(Clone)]
struct FixManifest {
    platform: String,
    tier: Option<String>,
    tag: String,
    version: String,
    git_sha: String,
    glibc: String,
    chunks: Vec<FixChunk>,
    links: Vec<(String, String)>,
}

fn file_in(path: &str) -> FileIn {
    FileIn {
        path: path.to_string(),
        sha256: None,
    }
}

fn fix_chunk(name: &str, group: &str, asset: &str, files: &[&str]) -> FixChunk {
    FixChunk {
        name: name.to_string(),
        group: group.to_string(),
        asset: asset.to_string(),
        files: files.iter().copied().map(file_in).collect(),
        content_key: String::new(),
    }
}

type PackedFile = (&'static str, &'static [u8]);
type PackedChunk = (&'static str, Vec<PackedFile>);

fn standard_contents() -> Vec<PackedChunk> {
    vec![
        (
            "linux-core.tar.zst",
            vec![("bin/app", b"app-binary"), ("config/app.toml", b"cfg")],
        ),
        (
            "linux-data-models-00of02.tar.zst",
            vec![("data/models/a.bin", b"a-bin"), ("data/models/a.gltf", b"a-gltf")],
        ),
        (
            "linux-data-models-01of02.tar.zst",
            vec![("data/models/b.bin", b"b-bin")],
        ),
        (
            "linux-tex-low-00of01.tar.zst",
            vec![("data/tex/low.bin", b"low-tex")],
        ),
        (
            "linux-tex-high-00of01.tar.zst",
            vec![("data/tex/high.bin", b"high-tex")],
        ),
    ]
}

fn default_manifests() -> Vec<FixManifest> {
    let shared = vec![
        fix_chunk(
            "core",
            "core",
            "linux-core.tar.zst",
            &["bin/app", "config/app.toml"],
        ),
        fix_chunk(
            "data-models-00of02",
            "data-models",
            "linux-data-models-00of02.tar.zst",
            &["data/models/a.bin", "data/models/a.gltf"],
        ),
        fix_chunk(
            "data-models-01of02",
            "data-models",
            "linux-data-models-01of02.tar.zst",
            &["data/models/b.bin"],
        ),
    ];
    let mut manifests = Vec::new();
    for tier in ["low", "high"] {
        let mut chunks = shared.clone();
        chunks.push(fix_chunk(
            &format!("gfx-tex-{tier}-00of01"),
            "gfx-tex",
            &format!("linux-tex-{tier}-00of01.tar.zst"),
            &[&format!("data/tex/{tier}.bin")],
        ));
        manifests.push(FixManifest {
            platform: "linux".to_string(),
            tier: Some(tier.to_string()),
            tag: "v1.2.3".to_string(),
            version: "1.2.3".to_string(),
            git_sha: "0123abcd".to_string(),
            glibc: "2.35".to_string(),
            chunks,
            links: vec![(
                "data/models/a_copy.bin".to_string(),
                "data/models/a.bin".to_string(),
            )],
        });
    }
    manifests
}

fn dummy_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut pairs: Vec<(&str, &[u8])> = files.to_vec();
    pairs.sort_by(|left, right| left.0.cmp(right.0));
    let mut out = Vec::new();
    for (index, (name, payload)) in pairs.iter().enumerate() {
        if index > 0 {
            out.push(b'\n');
        }
        out.extend(name.as_bytes());
        out.push(b':');
        out.extend_from_slice(payload);
    }
    out
}

fn write_dummy(dir: &Path, asset: &str, files: &[(&str, &[u8])]) {
    fs::write(dir.join(asset), dummy_bytes(files)).expect("dummy asset");
}

fn write_real(dir: &Path, asset: &str, files: &[(&str, &[u8])]) -> Result<String, ChunkError> {
    let stage = dir.join(format!(".stage-{}", asset.replace('/', "_")));
    fs::create_dir_all(&stage).map_err(|err| ChunkError::io(&stage, &err))?;
    let mut order: Vec<(&str, &[u8])> = files.to_vec();
    order.sort_by(|left, right| left.0.cmp(right.0));
    let mut units = Vec::new();
    for (index, (name, bytes)) in order.iter().enumerate() {
        let abs = stage.join(format!("f{index}"));
        fs::write(&abs, bytes).map_err(|err| ChunkError::io(&abs, &err))?;
        units.push(Unit {
            stored: Node {
                rel: (*name).to_string(),
                abs,
                size: u64::try_from(bytes.len()).unwrap_or(0),
                mode: 0o644,
                kind: NodeKind::File,
                link_target: None,
            },
            links: Vec::new(),
        });
    }
    let tar_path = stage.join("archive.tar");
    tar::write_units(&tar_path, &units)?;
    let key = hex_encode(&sha256_file(&tar_path)?);
    ZstdCli::standard().compress_file(&tar_path, &dir.join(asset))?;
    Ok(key)
}

fn stamp_real(manifests: &mut [FixManifest], asset: &str, files: &[(&str, &[u8])], key: &str) {
    for manifest in manifests {
        for chunk in &mut manifest.chunks {
            if chunk.asset != asset {
                continue;
            }
            chunk.content_key = key.to_string();
            for file in &mut chunk.files {
                if let Some((_, bytes)) = files.iter().find(|(name, _)| *name == file.path) {
                    file.sha256 = Some(sha256_hex(bytes));
                }
            }
        }
    }
}

fn unpacked_of(asset: &str, extra: &BTreeMap<String, u64>) -> u64 {
    if let Some(size) = extra.get(asset) {
        return *size;
    }
    standard_contents()
        .into_iter()
        .find(|(name, _)| *name == asset)
        .map(|(_, files)| files.iter().map(|(_, bytes)| bytes.len() as u64).sum())
        .unwrap_or(0)
}

fn file_sha(path: &Path) -> String {
    let len = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if len > DEFAULT_MAX_CHUNK_BYTES {
        return String::new();
    }
    hex_encode(&sha256_file(path).expect("hash asset"))
}

fn write_manifests(root: &Path, manifests: &[FixManifest], extra_unpacked: &BTreeMap<String, u64>) {
    for manifest in manifests {
        let stem = match &manifest.tier {
            Some(tier) => format!("manifest-{}-{tier}", manifest.platform),
            None => format!("manifest-{}", manifest.platform),
        };
        let mut chunks_json = Vec::new();
        let mut tsv_chunks: Vec<(String, String, String, i64, u64)> = Vec::new();
        for chunk in &manifest.chunks {
            let asset_path = root.join(&chunk.asset);
            let exists = asset_path.is_file();
            let size = if exists {
                i64::try_from(fs::metadata(&asset_path).expect("asset meta").len()).unwrap_or(0)
            } else {
                0
            };
            let sha = if exists {
                file_sha(&asset_path)
            } else {
                String::new()
            };
            let unpacked = unpacked_of(&chunk.asset, extra_unpacked);
            let files_json: Vec<Value> = chunk
                .files
                .iter()
                .map(|file| match &file.sha256 {
                    Some(sha) => json!({"path": file.path, "sha256": sha}),
                    None => json!(file.path),
                })
                .collect();
            let mut entry = json!({
                "name": chunk.name,
                "group": chunk.group,
                "asset": chunk.asset,
                "buckets": 1,
                "sha256": sha,
                "size": size,
                "unpacked": unpacked,
                "files": files_json,
            });
            if !chunk.content_key.is_empty() {
                entry["content_key"] = json!(chunk.content_key);
            }
            chunks_json.push(entry);
            tsv_chunks.push((chunk.name.clone(), chunk.asset.clone(), sha, size, unpacked));
        }
        let mut doc = json!({
            "format": 1,
            "tag": manifest.tag,
            "version": manifest.version,
            "git_sha": manifest.git_sha,
            "platform": manifest.platform,
            "glibc_required": manifest.glibc,
            "chunks": chunks_json,
            "links": manifest.links.iter().map(|(path, target)| json!([path, target])).collect::<Vec<_>>(),
        });
        if let Some(tier) = &manifest.tier {
            doc["tier"] = json!(tier);
        }
        let json_path = root.join(format!("{stem}.json"));
        let tsv_path = root.join(format!("{stem}.tsv"));
        let json_text = serde_json::to_string_pretty(&doc).expect("json");
        fs::write(&json_path, format!("{json_text}\n")).expect("write json");
        let mut rows = vec![
            "format\t1".to_string(),
            format!("tag\t{}", manifest.tag),
            format!("version\t{}", manifest.version),
            format!("glibc_required\t{}", manifest.glibc),
        ];
        if let Some(tier) = &manifest.tier {
            rows.push(format!("tier\t{tier}"));
        }
        for (name, asset, sha, size, unpacked) in &tsv_chunks {
            rows.push(format!("chunk\t{name}\t{asset}\t{sha}\t{size}\t{unpacked}"));
        }
        for chunk in &manifest.chunks {
            for file in &chunk.files {
                rows.push(format!("file\t{}\t{}", chunk.name, file.path));
            }
        }
        for (path, target) in &manifest.links {
            rows.push(format!("link\t{path}\t{target}"));
        }
        fs::write(&tsv_path, format!("{}\n", rows.join("\n"))).expect("write tsv");
        let sidecar = format!(
            "{}  {}\n{}  {}\n",
            file_sha(&json_path),
            json_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("manifest.json"),
            file_sha(&tsv_path),
            tsv_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("manifest.tsv"),
        );
        fs::write(root.join(format!("{stem}.json.sha256")), sidecar).expect("sidecar");
    }
}

fn build(root: &Path, real: bool) -> Vec<FixManifest> {
    let mut manifests = default_manifests();
    for (asset, files) in standard_contents() {
        if real {
            let key = write_real(root, asset, &files).expect("real archive");
            stamp_real(&mut manifests, asset, &files, &key);
        } else {
            write_dummy(root, asset, &files);
        }
    }
    write_manifests(root, &manifests, &BTreeMap::new());
    manifests
}

fn shallow() -> VerifyOptions {
    VerifyOptions::default()
}

fn deep() -> VerifyOptions {
    VerifyOptions {
        deep: true,
        ..VerifyOptions::default()
    }
}

#[test]
fn clean_release_exits_zero() {
    let scratch = Scratch::new("verify-clean");
    build(&scratch, false);
    let report = verify_release(&scratch, &shallow());
    assert!(report.ok(), "{}", joined(&report));
    assert_eq!(report.to_value()["problem_count"], 0);
    assert_eq!(report.to_value()["ok"], true);
}

#[test]
fn clean_release_deep_exits_zero() {
    let scratch = Scratch::new("verify-deep");
    build(&scratch, true);
    let report = verify_release(&scratch, &deep());
    assert_eq!(report.to_value()["problem_count"], 0, "{}", joined(&report));
}

#[test]
fn tiers_filter_skips_other_tier() {
    let scratch = Scratch::new("verify-tiers");
    build(&scratch, false);
    let report = verify_release(
        &scratch,
        &VerifyOptions {
            deep: false,
            tiers: vec!["low".to_string()],
            ..VerifyOptions::default()
        },
    );
    assert!(report.ok(), "{}", joined(&report));
    assert_eq!(report.skipped, vec!["manifest-linux-high.json".to_string()]);
}

#[test]
fn sidecar_mismatch_detected() {
    let scratch = Scratch::new("verify-sidecar");
    build(&scratch, false);
    let json_path = scratch.join("manifest-linux-low.json");
    let mut bytes = fs::read(&json_path).expect("json");
    bytes.push(b'\n');
    fs::write(&json_path, bytes).expect("rewrite");
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(check_set(&report).contains("sidecar_sha256"));
    assert!(joined(&report).contains("manifest-linux-low.json"));
}

#[test]
fn json_tsv_disagreement_detected() {
    let scratch = Scratch::new("verify-tsv");
    build(&scratch, false);
    let tsv_path = scratch.join("manifest-linux-low.tsv");
    let text = fs::read_to_string(&tsv_path).expect("tsv");
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.starts_with("chunk\tcore\t") {
            let mut parts: Vec<String> = line.split('\t').map(str::to_string).collect();
            let size: i64 = parts[4].parse().expect("size");
            parts[4] = (size + 1).to_string();
            lines.push(parts.join("\t"));
        } else {
            lines.push(line.to_string());
        }
    }
    fs::write(&tsv_path, format!("{}\n", lines.join("\n"))).expect("tsv");
    let stem = "manifest-linux-low";
    let json_path = scratch.join(format!("{stem}.json"));
    let sidecar = format!(
        "{}  {stem}.json\n{}  {stem}.tsv\n",
        file_sha(&json_path),
        file_sha(&tsv_path)
    );
    fs::write(scratch.join(format!("{stem}.json.sha256")), sidecar).expect("sidecar");
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(
        check_set(&report).contains("json_tsv_mismatch"),
        "{}",
        joined(&report)
    );
}

#[test]
fn shallow_verify_catches_a_missing_chunk() {
    let scratch = Scratch::new("verify-missing");
    build(&scratch, false);
    fs::remove_file(scratch.join("linux-core.tar.zst")).expect("unlink");
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(check_set(&report).contains("missing_asset"));
    assert!(joined(&report).contains("linux-core.tar.zst"));
}

#[test]
fn missing_asset_detected() {
    shallow_verify_catches_a_missing_chunk();
}

#[test]
fn size_mismatch_detected() {
    let scratch = Scratch::new("verify-size");
    build(&scratch, false);
    let asset = scratch.join("linux-core.tar.zst");
    let mut bytes = fs::read(&asset).expect("asset");
    bytes.push(b'x');
    fs::write(&asset, bytes).expect("rewrite");
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(check_set(&report).contains("size_mismatch"));
}

#[test]
fn flipped_byte_fails_deep_verify() {
    let scratch = Scratch::new("verify-flip");
    build(&scratch, false);
    let asset = scratch.join("linux-core.tar.zst");
    let mut bytes = fs::read(&asset).expect("asset");
    bytes[0] ^= 0xff;
    fs::write(&asset, bytes).expect("rewrite");
    let report = verify_release(&scratch, &deep());
    assert!(!report.ok());
    assert!(check_set(&report).contains("sha256_mismatch"));
    assert!(joined(&report).contains("linux-core.tar.zst"));
}

#[test]
fn deep_sha256_mismatch_detected() {
    flipped_byte_fails_deep_verify();
}

#[test]
fn deep_member_missing_detected() {
    let scratch = Scratch::new("verify-member-miss");
    let mut manifests = build(&scratch, true);
    manifests[0].chunks[0].files.push(file_in("bin/not-in-archive"));
    write_manifests(&scratch, &manifests, &BTreeMap::new());
    let report = verify_release(&scratch, &deep());
    assert!(!report.ok());
    assert!(check_set(&report).contains("tar_members"), "{}", joined(&report));
    assert!(joined(&report).contains("not-in-archive"));
}

#[test]
fn deep_member_extra_detected() {
    let scratch = Scratch::new("verify-member-extra");
    let mut manifests = build(&scratch, true);
    let mut files = standard_contents()
        .into_iter()
        .find(|(name, _)| *name == "linux-core.tar.zst")
        .expect("core")
        .1;
    files.push(("bin/extra", b"extra"));
    let key = write_real(&scratch, "linux-core.tar.zst", &files).expect("archive");
    manifests[0].chunks[0].content_key = key;
    // The high tier still lists the old content key; refresh both core rows.
    let low_key = manifests[0].chunks[0].content_key.clone();
    for manifest in &mut manifests {
        if manifest.chunks[0].asset == "linux-core.tar.zst" {
            manifest.chunks[0].content_key.clone_from(&low_key);
        }
    }
    write_manifests(&scratch, &manifests, &BTreeMap::new());
    let report = verify_release(&scratch, &deep());
    assert!(!report.ok());
    assert!(check_set(&report).contains("tar_members"), "{}", joined(&report));
    assert!(joined(&report).contains("extra"));
}

#[test]
fn member_sha256_mismatch_is_named() {
    let scratch = Scratch::new("verify-member-sha");
    let mut manifests = build(&scratch, true);
    for manifest in &mut manifests {
        for file in &mut manifest.chunks[0].files {
            if file.path == "bin/app" {
                file.sha256 = Some("0".repeat(64));
            }
        }
    }
    write_manifests(&scratch, &manifests, &BTreeMap::new());
    let report = verify_release(&scratch, &deep());
    assert!(
        check_set(&report).contains("member_sha256"),
        "{}",
        joined(&report)
    );
    assert!(joined(&report).contains("bin/app"));
}

#[test]
fn content_key_mismatch_is_named() {
    let scratch = Scratch::new("verify-content-key");
    let mut manifests = build(&scratch, true);
    for manifest in &mut manifests {
        if manifest.chunks[0].asset == "linux-core.tar.zst" {
            manifest.chunks[0].content_key = "ab".repeat(32);
        }
    }
    write_manifests(&scratch, &manifests, &BTreeMap::new());
    let report = verify_release(&scratch, &deep());
    assert!(
        check_set(&report).contains("content_key_mismatch"),
        "{}",
        joined(&report)
    );
    assert!(joined(&report).contains("linux-core.tar.zst"));
}

#[test]
fn duplicate_path_detected() {
    let scratch = Scratch::new("verify-dup");
    let mut manifests = build(&scratch, false);
    manifests[0].chunks[1].files.push(file_in("bin/app"));
    write_manifests(&scratch, &manifests, &BTreeMap::new());
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(check_set(&report).contains("duplicate_path"));
}

#[test]
fn link_target_missing_detected() {
    let scratch = Scratch::new("verify-link");
    let mut manifests = build(&scratch, false);
    manifests[0].links.push((
        "data/models/c.bin".to_string(),
        "data/models/nope.bin".to_string(),
    ));
    write_manifests(&scratch, &manifests, &BTreeMap::new());
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(check_set(&report).contains("link_target"));
}

#[test]
fn link_path_is_file_detected() {
    let scratch = Scratch::new("verify-link-file");
    let mut manifests = build(&scratch, false);
    manifests[0]
        .links
        .push(("bin/app".to_string(), "config/app.toml".to_string()));
    write_manifests(&scratch, &manifests, &BTreeMap::new());
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(check_set(&report).contains("link_path_is_file"));
}

#[test]
fn unsafe_path_detected() {
    for bad_path in ["../evil.bin", "/abs/evil.bin", "data\\evil.bin"] {
        let scratch = Scratch::new("verify-unsafe");
        let mut manifests = build(&scratch, false);
        manifests[0].chunks[0].files.push(file_in(bad_path));
        write_manifests(&scratch, &manifests, &BTreeMap::new());
        let report = verify_release(&scratch, &shallow());
        assert!(!report.ok(), "{bad_path}");
        assert!(
            check_set(&report).contains("unsafe_path"),
            "{bad_path}: {}",
            joined(&report)
        );
    }
}

#[test]
fn shared_chunk_mismatch_detected() {
    let scratch = Scratch::new("verify-shared");
    let mut manifests = build(&scratch, false);
    fs::write(scratch.join("linux-core-alt.tar.zst"), b"different-core-bytes").expect("alt");
    manifests[1].chunks[0].asset = "linux-core-alt.tar.zst".to_string();
    let mut extra = BTreeMap::new();
    extra.insert(
        "linux-core-alt.tar.zst".to_string(),
        (b"x".len() + b"y".len()) as u64,
    );
    write_manifests(&scratch, &manifests, &extra);
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(
        check_set(&report).contains("shared_chunk_mismatch"),
        "{}",
        joined(&report)
    );
}

#[test]
fn tex_wrong_tier_detected() {
    let scratch = Scratch::new("verify-tex");
    let mut manifests = build(&scratch, false);
    manifests[0].chunks.push(fix_chunk(
        "gfx-tex-high-00of01",
        "gfx-tex",
        "linux-tex-high-00of01.tar.zst",
        &["data/tex/high.bin"],
    ));
    write_manifests(&scratch, &manifests, &BTreeMap::new());
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(
        check_set(&report).contains("tex_tier_mismatch"),
        "{}",
        joined(&report)
    );
}

#[test]
fn metadata_mismatch_detected() {
    let scratch = Scratch::new("verify-meta");
    let mut manifests = build(&scratch, false);
    manifests[1].tag = "v9.9.9".to_string();
    write_manifests(&scratch, &manifests, &BTreeMap::new());
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(check_set(&report).contains("metadata_mismatch"));
}

#[test]
fn archive_too_large_detected() {
    let scratch = Scratch::new("verify-big");
    let mut manifests = build(&scratch, false);
    let big = scratch.join("linux-big.tar.zst");
    let file = fs::File::create(&big).expect("big");
    file.set_len(DEFAULT_MAX_CHUNK_BYTES + 1).expect("sparse length");
    drop(file);
    manifests[0].chunks.push(FixChunk {
        name: "big".to_string(),
        group: "big".to_string(),
        asset: "linux-big.tar.zst".to_string(),
        files: Vec::new(),
        content_key: String::new(),
    });
    write_manifests(&scratch, &manifests, &BTreeMap::new());
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(
        check_set(&report).contains("archive_too_large"),
        "{}",
        joined(&report)
    );
    assert!(joined(&report).contains("linux-big.tar.zst"));
}

#[test]
fn orphan_archive_detected() {
    let scratch = Scratch::new("verify-orphan");
    build(&scratch, false);
    fs::write(scratch.join("orphan.tar.zst"), b"orphan-bytes").expect("orphan");
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(check_set(&report).contains("orphan_archive"));
    assert!(joined(&report).contains("orphan.tar.zst"));
    let problems = &report.to_value()["problems"];
    assert!(
        problems
            .as_array()
            .expect("problems")
            .iter()
            .any(|problem| { problem["check"] == "orphan_archive" && problem["manifest"].is_null() })
    );
}

#[test]
fn chunk_group_gap_detected() {
    let scratch = Scratch::new("verify-gap");
    let mut manifests = build(&scratch, false);
    for manifest in &mut manifests {
        manifest.chunks.retain(|chunk| chunk.name != "data-models-01of02");
    }
    fs::remove_file(scratch.join("linux-data-models-01of02.tar.zst")).expect("unlink");
    write_manifests(&scratch, &manifests, &BTreeMap::new());
    let report = verify_release(&scratch, &shallow());
    assert!(!report.ok());
    assert!(
        check_set(&report).contains("chunk_group_gap"),
        "{}",
        joined(&report)
    );
}

#[test]
fn corrupt_deleted_and_edited_are_all_named() {
    let scratch = Scratch::new("verify-combo");
    build(&scratch, false);
    let asset = scratch.join("linux-data-models-00of02.tar.zst");
    let mut bytes = fs::read(&asset).expect("asset");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&asset, bytes).expect("rewrite");
    fs::remove_file(scratch.join("linux-data-models-01of02.tar.zst")).expect("unlink");
    let tsv_path = scratch.join("manifest-linux-high.tsv");
    let text = fs::read_to_string(&tsv_path)
        .expect("tsv")
        .replace("v1.2.3", "v1.2.4");
    fs::write(&tsv_path, text).expect("tsv");
    let report = verify_release(&scratch, &deep());
    assert!(!report.ok());
    let messages = joined(&report);
    let checks = check_set(&report);
    assert!(checks.contains("sha256_mismatch"));
    assert!(messages.contains("linux-data-models-00of02.tar.zst"));
    assert!(checks.contains("missing_asset"));
    assert!(messages.contains("linux-data-models-01of02.tar.zst"));
    assert!(messages.contains("manifest-linux-high.tsv"));
    assert!(checks.contains("sidecar_sha256") || checks.contains("json_tsv_mismatch"));
}

#[test]
fn c15_platform_copy_verifies_shallow() {
    let scratch = Scratch::new("verify-c15");
    let asset = scratch.join("core-000.tar.zst");
    fs::write(&asset, b"dummy-chunk").expect("asset");
    let manifest = ChunkManifest {
        format_version: 1,
        platform: "linux".to_string(),
        tag: "v1".to_string(),
        product_version: "1".to_string(),
        source_commit: "abc".to_string(),
        libc_required: String::new(),
        tier: "low".to_string(),
        chunks: vec![ChunkRecord {
            name: "core".to_string(),
            group: "core".to_string(),
            buckets: 1,
            asset: "core-000.tar.zst".to_string(),
            sha256: hex_encode(&sha256_file(&asset).expect("hash")),
            size: fs::metadata(&asset).expect("meta").len(),
            unpacked: 0,
            files: Vec::new(),
            content_key: "not-the-tar".to_string(),
        }],
        links: Vec::new(),
    };
    write_manifest_files(&scratch, &manifest).expect("manifest");
    assert!(scratch.join("manifest-linux.low.json").is_file());
    assert!(!scratch.join("manifest-linux.low.json.sha256").exists());
    let report = verify_release(&scratch, &shallow());
    assert!(report.ok(), "{}", joined(&report));
}

#[test]
fn missing_release_dir_is_a_problem() {
    let scratch = Scratch::new("verify-nodir");
    let missing = scratch.join("gone");
    let report = verify_release(&missing, &shallow());
    assert!(check_set(&report).contains("release_dir"));
}

fn diff_chunk(name: &str, sha: &str, size: i64, unpacked: i64, files: Value, asset: Option<&str>) -> Value {
    let asset = asset
        .map(str::to_string)
        .unwrap_or_else(|| format!("linux-{name}.tar.zst"));
    json!({
        "name": name,
        "group": name,
        "asset": asset,
        "sha256": sha,
        "size": size,
        "unpacked": unpacked,
        "files": files,
    })
}

fn old_chunks() -> Vec<Value> {
    vec![
        diff_chunk(
            "core",
            &"a".repeat(64),
            100,
            1000,
            json!(["bin/app", "data/maps/a.bin"]),
            None,
        ),
        diff_chunk(
            "audio",
            &"b".repeat(64),
            200,
            2000,
            json!(["data/audio/theme.ogg"]),
            None,
        ),
        diff_chunk("legacy", &"c".repeat(64), 50, 500, json!(["data/old.bin"]), None),
    ]
}

fn new_chunks() -> Vec<Value> {
    vec![
        diff_chunk(
            "core",
            &"d".repeat(64),
            120,
            1100,
            json!(["bin/app", "data/maps/b.bin"]),
            None,
        ),
        diff_chunk(
            "audio",
            &"b".repeat(64),
            200,
            2000,
            json!(["data/maps/a.bin"]),
            None,
        ),
        diff_chunk("extra", &"e".repeat(64), 30, 300, json!(["data/new.bin"]), None),
    ]
}

fn write_diff(
    root: &Path,
    platform: &str,
    tier: Option<&str>,
    tag: &str,
    chunks: &[Value],
    links: &[(&str, &str)],
) {
    fs::create_dir_all(root).expect("dir");
    let mut doc = json!({
        "format": 1,
        "tag": tag,
        "version": tag.trim_start_matches('v'),
        "platform": platform,
        "chunks": chunks,
        "links": links.iter().map(|(path, target)| json!([path, target])).collect::<Vec<_>>(),
    });
    let filename = if let Some(tier) = tier {
        doc["tier"] = json!(tier);
        format!("manifest-{platform}-{tier}.json")
    } else {
        format!("manifest-{platform}.json")
    };
    fs::write(root.join(filename), serde_json::to_string(&doc).expect("json")).expect("write");
}

fn standard_pair(root: &Path) -> super::PairDiff {
    let old = root.join("old");
    let new = root.join("new");
    write_diff(&old, "linux", Some("low"), "v1.0.0", &old_chunks(), &[]);
    write_diff(&new, "linux", Some("low"), "v1.1.0", &new_chunks(), &[]);
    let report = diff_release(&old, &new, &DiffOptions::default()).expect("diff");
    assert_eq!(report.pairs.len(), 1);
    report.pairs.into_iter().next().expect("pair")
}

#[test]
fn chunk_classification() {
    let scratch = Scratch::new("diff-class");
    let pair = standard_pair(&scratch);
    let names = |chunks: &[super::ChunkView]| -> Vec<String> {
        chunks.iter().map(|chunk| chunk.name.clone()).collect()
    };
    assert_eq!(names(&pair.chunks.unchanged), ["audio"]);
    assert_eq!(
        pair.chunks
            .changed
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        ["core"]
    );
    assert_eq!(names(&pair.chunks.added), ["extra"]);
    assert_eq!(names(&pair.chunks.removed), ["legacy"]);
}

#[test]
fn same_name_new_asset_is_changed() {
    let scratch = Scratch::new("diff-asset");
    let old = scratch.join("old");
    let new = scratch.join("new");
    write_diff(
        &old,
        "linux",
        Some("low"),
        "v1",
        &[diff_chunk(
            "core",
            &"a".repeat(64),
            10,
            10,
            json!([]),
            Some("a.tar.zst"),
        )],
        &[],
    );
    write_diff(
        &new,
        "linux",
        Some("low"),
        "v1",
        &[diff_chunk(
            "core",
            &"a".repeat(64),
            10,
            10,
            json!([]),
            Some("b.tar.zst"),
        )],
        &[],
    );
    let report = diff_release(&old, &new, &DiffOptions::default()).expect("diff");
    assert!(report.pairs[0].chunks.unchanged.is_empty());
    assert_eq!(report.pairs[0].chunks.changed[0].name, "core");
}

#[test]
fn download_and_unpacked_sizes() {
    let scratch = Scratch::new("diff-bytes");
    let pair = standard_pair(&scratch);
    assert_eq!(pair.chunks.update_download_bytes, 150);
    assert_eq!(pair.chunks.fresh_download_bytes, 350);
    assert_eq!(pair.chunks.unpacked_before_bytes, 3500);
    assert_eq!(pair.chunks.unpacked_after_bytes, 3400);
    assert_eq!(pair.chunks.unpacked_delta_bytes(), -100);
}

#[test]
fn update_skips_sha_already_present_under_another_chunk() {
    let scratch = Scratch::new("diff-skip");
    let old = scratch.join("old");
    let new = scratch.join("new");
    write_diff(
        &old,
        "linux",
        Some("low"),
        "v1",
        &[diff_chunk("core", &"a".repeat(64), 100, 1000, json!([]), None)],
        &[],
    );
    write_diff(
        &new,
        "linux",
        Some("low"),
        "v1",
        &[
            diff_chunk("core", &"a".repeat(64), 100, 1000, json!([]), None),
            diff_chunk("copy", &"a".repeat(64), 100, 1000, json!([]), None),
        ],
        &[],
    );
    let report = diff_release(&old, &new, &DiffOptions::default()).expect("diff");
    assert_eq!(report.pairs[0].chunks.update_download_bytes, 0);
    assert_eq!(report.pairs[0].chunks.fresh_download_bytes, 100);
}

#[test]
fn diff_reports_a_moved_file_once() {
    let scratch = Scratch::new("diff-move");
    let pair = standard_pair(&scratch);
    assert_eq!(
        pair.files.added,
        ["data/maps/b.bin".to_string(), "data/new.bin".to_string()]
    );
    assert_eq!(
        pair.files.removed,
        ["data/audio/theme.ogg".to_string(), "data/old.bin".to_string()]
    );
    assert!(!pair.files.added.iter().any(|path| path == "data/maps/a.bin"));
    assert!(!pair.files.removed.iter().any(|path| path == "data/maps/a.bin"));
    assert_eq!(pair.files.moved.len(), 1);
    assert_eq!(pair.files.moved[0].path, "data/maps/a.bin");
    assert_eq!(pair.files.moved[0].from_chunk, "core");
    assert_eq!(pair.files.moved[0].to_chunk, "audio");
}

#[test]
fn files_added_removed_and_moved() {
    diff_reports_a_moved_file_once();
}

#[test]
fn files_grouped_by_first_two_segments_counts_only() {
    let scratch = Scratch::new("diff-dir");
    let pair = standard_pair(&scratch);
    let value = super::DiffReport { pairs: vec![pair] }.to_value();
    let maps = &value["pairs"][0]["files"]["by_directory"]["data/maps"];
    assert_eq!(maps["added"], 1);
    assert_eq!(maps["moved"], 1);
    assert!(maps["byte_delta"].is_null());
    assert!(maps["added_bytes"].is_null());
}

#[test]
fn file_byte_deltas_when_sizes_known() {
    let scratch = Scratch::new("diff-file-bytes");
    let old = scratch.join("old");
    let new = scratch.join("new");
    write_diff(
        &old,
        "linux",
        Some("low"),
        "v1",
        &[diff_chunk(
            "core",
            &"a".repeat(64),
            10,
            10,
            json!([{"path": "data/x/old.bin", "size": 40}]),
            None,
        )],
        &[],
    );
    write_diff(
        &new,
        "linux",
        Some("low"),
        "v1",
        &[diff_chunk(
            "core",
            &"b".repeat(64),
            10,
            10,
            json!([{"path": "data/x/new.bin", "size": 70}]),
            None,
        )],
        &[],
    );
    let report = diff_release(&old, &new, &DiffOptions::default()).expect("diff");
    let group = &report.to_value()["pairs"][0]["files"]["by_directory"]["data/x"];
    assert_eq!(group["added_bytes"], 70);
    assert_eq!(group["removed_bytes"], 40);
    assert_eq!(group["byte_delta"], 30);
}

#[test]
fn links_added_and_removed() {
    let scratch = Scratch::new("diff-links");
    let old = scratch.join("old");
    let new = scratch.join("new");
    write_diff(
        &old,
        "linux",
        Some("low"),
        "v1",
        &old_chunks(),
        &[("data/a_copy.bin", "data/a.bin")],
    );
    write_diff(
        &new,
        "linux",
        Some("low"),
        "v1",
        &new_chunks(),
        &[("data/b_copy.bin", "data/b.bin")],
    );
    let report = diff_release(&old, &new, &DiffOptions::default()).expect("diff");
    assert_eq!(
        report.pairs[0].links_added,
        vec![("data/b_copy.bin".to_string(), "data/b.bin".to_string())]
    );
    assert_eq!(
        report.pairs[0].links_removed,
        vec![("data/a_copy.bin".to_string(), "data/a.bin".to_string())]
    );
}

#[test]
fn untiered_manifest_filename() {
    let scratch = Scratch::new("diff-untiered");
    let release = scratch.join("rel");
    write_diff(&release, "linux", None, "v1", &old_chunks(), &[]);
    let report = diff_release(&release, &release, &DiffOptions::default()).expect("diff");
    assert_eq!(report.pairs.len(), 1);
    assert_eq!(report.pairs[0].platform, "linux");
    assert_eq!(report.pairs[0].tier, "");
}

#[test]
fn directory_with_multiple_tiers_and_filters() {
    let scratch = Scratch::new("diff-multi");
    let old = scratch.join("old");
    let new = scratch.join("new");
    for (platform, tier) in [("linux", "low"), ("linux", "high"), ("windows", "low")] {
        write_diff(&old, platform, Some(tier), "v1", &old_chunks(), &[]);
        write_diff(&new, platform, Some(tier), "v1.1", &new_chunks(), &[]);
    }
    let report = diff_release(&old, &new, &DiffOptions::default()).expect("diff");
    let keys: Vec<(&str, &str)> = report
        .pairs
        .iter()
        .map(|pair| (pair.platform.as_str(), pair.tier.as_str()))
        .collect();
    assert_eq!(keys, [("linux", "high"), ("linux", "low"), ("windows", "low")]);
    let filtered = diff_release(
        &old,
        &old,
        &DiffOptions {
            platform: Some("linux".to_string()),
            tier: Some("high".to_string()),
        },
    )
    .expect("filter");
    assert_eq!(filtered.pairs.len(), 1);
    assert_eq!(filtered.pairs[0].platform, "linux");
    assert_eq!(filtered.pairs[0].tier, "high");
}

#[test]
fn main_json_output() {
    let scratch = Scratch::new("diff-json");
    let pair = standard_pair(&scratch);
    let value = super::DiffReport { pairs: vec![pair] }.to_value();
    let pair = &value["pairs"][0];
    assert_eq!(pair["platform"], "linux");
    assert_eq!(pair["tier"], "low");
    assert_eq!(pair["update_download_bytes"], 150);
    assert_eq!(pair["fresh_download_bytes"], 350);
    assert_eq!(
        pair["chunks"]["counts"],
        json!({"unchanged": 1, "changed": 1, "added": 1, "removed": 1})
    );
    assert!(value["tiers"].is_array());
}

#[test]
fn main_text_and_markdown_output() {
    let scratch = Scratch::new("diff-text");
    let old = scratch.join("old");
    let new = scratch.join("new");
    write_diff(&old, "linux", Some("low"), "v1.0.0", &old_chunks(), &[]);
    write_diff(&new, "linux", Some("low"), "v1.1.0", &new_chunks(), &[]);
    let report = diff_release(&old, &new, &DiffOptions::default()).expect("diff");
    let text = format_text(&report);
    assert!(text.contains("Update download:"));
    assert!(text.contains("150 bytes"));
    let markdown = format_markdown(&report);
    assert!(markdown.contains("| Platform | Tier | Update download | Fresh download |"));
    assert!(markdown.contains("| linux | low |"));
    assert!(markdown.contains("150 bytes"));
    assert!(markdown.contains("3400 bytes"));
}

#[test]
fn main_accepts_single_manifest_files() {
    let scratch = Scratch::new("diff-files");
    let old = scratch.join("old");
    let new = scratch.join("new");
    write_diff(&old, "linux", Some("low"), "v1", &old_chunks(), &[]);
    write_diff(&new, "linux", Some("low"), "v1", &new_chunks(), &[]);
    let report = diff_release(
        &old.join("manifest-linux-low.json"),
        &new.join("manifest-linux-low.json"),
        &DiffOptions::default(),
    )
    .expect("diff");
    assert_eq!(report.pairs[0].chunks.update_download_bytes, 150);
}

#[test]
fn identical_releases_exit_zero() {
    let scratch = Scratch::new("diff-same");
    let old = scratch.join("old");
    let new = scratch.join("new");
    write_diff(&old, "linux", Some("low"), "v1", &old_chunks(), &[]);
    write_diff(&new, "linux", Some("low"), "v1", &old_chunks(), &[]);
    let report = diff_release(&old, &new, &DiffOptions::default()).expect("diff");
    assert_eq!(report.pairs[0].chunks.update_download_bytes, 0);
    assert!(report.pairs[0].chunks.changed.is_empty());
    assert!(report.pairs[0].chunks.added.is_empty());
}

#[test]
fn unreadable_inputs_are_release_input() {
    let scratch = Scratch::new("diff-bad");
    let good = scratch.join("good");
    write_diff(&good, "linux", Some("low"), "v1", &old_chunks(), &[]);
    let missing = scratch.join("missing");
    assert_eq!(
        diff_release(&missing, &good, &DiffOptions::default())
            .expect_err("unreadable")
            .code,
        "release_input"
    );
    assert_eq!(
        diff_release(&good, &missing, &DiffOptions::default())
            .expect_err("unreadable")
            .code,
        "release_input"
    );
    let empty = scratch.join("empty");
    fs::create_dir(&empty).expect("empty");
    assert_eq!(
        diff_release(&empty, &good, &DiffOptions::default())
            .expect_err("unreadable")
            .code,
        "release_input"
    );
    let bad = scratch.join("bad.json");
    fs::write(&bad, "{not json").expect("bad");
    assert_eq!(
        diff_release(&bad, &good, &DiffOptions::default())
            .expect_err("unreadable")
            .code,
        "release_input"
    );
    let malformed = scratch.join("malformed.json");
    fs::write(&malformed, r#"{"chunks": "nope"}"#).expect("malformed");
    assert_eq!(
        diff_release(&malformed, &good, &DiffOptions::default())
            .expect_err("unreadable")
            .code,
        "release_input"
    );
}

#[test]
fn format_size_and_directory_group() {
    assert_eq!(format_size(0), "0 B");
    assert_eq!(format_size(512), "512 B");
    assert_eq!(format_size(2048), "2.00 KiB");
    assert_eq!(directory_group("data/maps/level1/a.bin"), "data/maps");
    assert_eq!(directory_group("bin/app"), "bin/app");
    assert_eq!(directory_group("README"), "README");
    assert!(super::format_size_detailed(150).contains("150 bytes"));
}

#[test]
fn upload_plan_puts_manifests_last() {
    let scratch = Scratch::new("upload-plan");
    for name in [
        "b.tar.zst",
        "a.tar.zst",
        "notes.json",
        "manifest-linux.json",
        "manifest-linux.tsv",
        "manifest-linux.json.sha256",
        "release-chunks.json",
        "release-chunks.tsv",
        "release-chunks.json.sha256",
    ] {
        fs::write(scratch.join(name), b"x").expect("file");
    }
    let plan = upload_plan(&scratch).expect("plan");
    let names: Vec<&str> = plan.groups.iter().map(|group| group.name.as_str()).collect();
    assert_eq!(names, ["chunks", "payload", "manifests"]);
    assert_eq!(
        plan.groups[0].files,
        ["a.tar.zst".to_string(), "b.tar.zst".to_string()]
    );
    assert_eq!(plan.groups[1].files, ["notes.json".to_string()]);
    assert!(
        plan.groups[2]
            .files
            .iter()
            .any(|name| name == "manifest-linux.json")
    );
    assert!(
        plan.groups[2]
            .files
            .iter()
            .all(|name| name != "notes.json" && !name.ends_with(".tar.zst"))
    );
    assert!(plan.retry_safe);
    assert_eq!(plan.schema, 1);
    assert_eq!(plan.retries, 3);
    assert_eq!(plan.to_value()["groups"][2]["name"], "manifests");
}

#[test]
fn upload_plan_omits_empty_groups_and_rejects_a_missing_dir() {
    let scratch = Scratch::new("upload-empty");
    fs::write(scratch.join("only.tar.zst"), b"x").expect("chunk");
    let plan = upload_plan(&scratch).expect("plan");
    assert_eq!(plan.groups.len(), 1);
    assert_eq!(plan.groups[0].name, "chunks");
    let err = upload_plan(&scratch.join("gone")).expect_err("missing");
    assert_eq!(err.code, "release_dir");
}

#[test]
fn upload_dry_run_is_recorded_and_does_not_reorder() {
    let scratch = Scratch::new("upload-dry");
    fs::write(scratch.join("a.tar.zst"), b"x").expect("chunk");
    fs::write(scratch.join("manifest-linux.json"), b"{}").expect("manifest");
    let plan = super::upload_plan_with(
        &scratch,
        &UploadPlanOptions {
            dry_run: true,
            ..UploadPlanOptions::default()
        },
    )
    .expect("plan");
    assert!(plan.dry_run);
    assert_eq!(plan.groups[0].name, "chunks");
    assert_eq!(plan.groups.last().expect("last").name, "manifests");
}

#[test]
fn manifest_asset_parsing_keeps_order_and_drops_duplicates() {
    let scratch = Scratch::new("upload-assets");
    let path = scratch.join("m.json");
    fs::write(
        &path,
        r#"{"chunks":[{"asset":"a.zip"},"b.zip",{"name":"a.zip"},{"file":"c.zip"}]}"#,
    )
    .expect("manifest");
    assert_eq!(
        load_manifest_assets(&path).expect("assets"),
        vec!["a.zip".to_string(), "b.zip".to_string(), "c.zip".to_string()]
    );
}

#[test]
fn after_gate_blocks_and_passes() {
    let blocked = gated_uploads(
        &[
            GateGroup {
                files: vec!["a.zip".to_string()],
                after: Vec::new(),
            },
            GateGroup {
                files: vec!["b.zip".to_string()],
                after: vec!["a.zip".to_string(), "zzz.bin".to_string()],
            },
        ],
        &[],
    );
    assert_eq!(blocked.uploaded, vec!["a.zip".to_string()]);
    assert!(blocked.blocked);
    let passed = gated_uploads(
        &[
            GateGroup {
                files: vec!["a.zip".to_string()],
                after: Vec::new(),
            },
            GateGroup {
                files: vec!["b.zip".to_string()],
                after: vec!["a.zip".to_string()],
            },
        ],
        &[],
    );
    assert_eq!(passed.uploaded, vec!["a.zip".to_string(), "b.zip".to_string()]);
    assert!(!passed.blocked);
}

#[test]
fn publish_gate_accepts_only_an_exact_ok_line() {
    let scratch = Scratch::new("upload-gate");
    let gate = scratch.join("g.txt");
    assert!(!publish_gate_ok(Some(&gate)));
    assert!(!publish_gate_ok(None));
    fs::write(&gate, "x\nOK\ny\n").expect("gate");
    assert!(publish_gate_ok(Some(&gate)));
    fs::write(&gate, "BOOKED\n").expect("gate");
    assert!(!publish_gate_ok(Some(&gate)));
    fs::write(&gate, "OKAY\nnot yet\n").expect("gate");
    assert!(!publish_gate_ok(Some(&gate)));
}

#[test]
fn repo_and_tag_rules() {
    assert!(!repo_ok("noslash"));
    assert!(!repo_ok("a/b/c"));
    assert!(!repo_ok("a/b c"));
    assert!(repo_ok("o/n"));
    assert!(tag_ok("v1"));
    assert!(!tag_ok(""));
    assert!(!tag_ok("v 1"));
}

#[test]
fn retry_budget_is_a_counter() {
    let succeeded = attempts_until_success(2, 3);
    assert_eq!(succeeded.attempts, 3);
    assert!(succeeded.succeeded);
    let failed = attempts_until_success(10, 2);
    assert_eq!(failed.attempts, 2);
    assert!(!failed.succeeded);
}

#[test]
fn size_mismatch_text_and_skip_same_size() {
    assert!(size_mismatch_message("a.zip", 5, 12).contains("size mismatch"));
    assert!(skip_same_size(5, Some(5)));
    assert!(!skip_same_size(5, Some(4)));
    assert!(!skip_same_size(5, None));
}

#[test]
fn stable_size_restarts_after_a_change() {
    assert_eq!(stable_size(&[3, 3, 3], 3), Some(3));
    assert_eq!(stable_size(&[3, 3, 6, 6], 3), None);
    assert_eq!(stable_size(&[3, 3, 6, 6, 6], 3), Some(6));
}

#[test]
fn bad_plan_toml_and_unmatched_pattern() {
    let err = load_plan_str("this is = = not toml [[[").expect_err("toml");
    assert_eq!(err.code, "plan_invalid");
    let loaded = load_plan_str("[[groups]]\nname=\"g\"\npatterns=[\"missing-*.zip\"]\n").expect("plan");
    assert_eq!(loaded.groups[0].name, "g");
    assert_eq!(loaded.retries, 3);
    let scratch = Scratch::new("upload-glob");
    fs::write(scratch.join("a.zip"), b"a").expect("a");
    fs::write(scratch.join("b.zip"), b"b").expect("b");
    fs::create_dir(scratch.join("data")).expect("data");
    fs::write(scratch.join("data/x.bin"), b"x").expect("bin");
    fs::create_dir(scratch.join("extra")).expect("extra");
    fs::write(scratch.join("extra/y.zip"), b"y").expect("nested");
    let one = expand_patterns(&scratch, "g", &["a.zip".to_string()]).expect("a.zip");
    assert_eq!(one.len(), 1);
    let zips = expand_patterns(&scratch, "g", &["*.zip".to_string()]).expect("zips");
    let names: Vec<String> = zips
        .iter()
        .map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert_eq!(names, ["a.zip".to_string(), "b.zip".to_string()]);
    let bins = expand_patterns(&scratch, "g", &["data/*.bin".to_string()]).expect("bins");
    assert_eq!(bins.len(), 1);
    assert!(bins[0].ends_with("data/x.bin"));
    let missed = expand_patterns(&scratch, "g", &["missing-*.zip".to_string()]).expect_err("pattern");
    assert_eq!(missed.code, "pattern_unmatched");
    let question = expand_patterns(&scratch, "g", &["?.zip".to_string()]).expect("question");
    assert_eq!(question.len(), 2);
}
