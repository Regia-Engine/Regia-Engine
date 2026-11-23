// SPDX-License-Identifier: MPL-2.0
//! Plan, cut, pack, verify and audit a release tree.
//!
//! Cut and pack both refuse to leave a chunk larger than the cap. Pack adds release buckets,
//! tier checks and reuse of a previous chunk's compressed bytes. The plan counts how many
//! pieces a bucket needs; the cutter applies that split only when a single archive would pass
//! the cap and the bucket holds more than one unit.

use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::codec::{self, ChunkCodec};
use crate::hash::{bucket_index, content_hash, hex_encode, sha256_file};
use crate::manifest::{
    AuditSettings, CANONICAL_FILE, ChunkLink, ChunkManifest, ChunkRecord, DirPrint, write_manifest_files,
};
use crate::scan::{self, NodeKind, Unit};
use crate::tar;
use crate::{ChunkError, ChunkSettings, FORMAT_VERSION};

/// What [`plan_tree`] expects to emit for one group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedGroup {
    /// Group name.
    pub group: String,
    /// File and link paths in the group.
    pub files: u64,
    /// Pieces the splitter would emit. One oversize file still counts as one piece.
    pub estimated_chunks: u32,
    /// Payload bytes, with each hard-link set counted once.
    pub raw_bytes: u64,
    /// Compressed bytes `cut` wrote for the group. `Some` only from [`plan_tree_compressed`],
    /// which also sets `estimated_chunks` to the chunks `cut` wrote (ADR-8846).
    pub compressed_bytes: Option<u64>,
}

/// A plan across every group in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasePlan {
    /// Groups in name order.
    pub groups: Vec<PlannedGroup>,
}

/// A cut: one shipping chunk per piece, no previous manifest, symlinks allowed.
#[derive(Debug, Clone)]
pub struct CutRequest {
    /// Tree to walk.
    pub source: PathBuf,
    /// Directory that receives chunks and `release-chunks.json`.
    pub output: PathBuf,
    /// Caps and grouping.
    pub settings: ChunkSettings,
    /// Platform label. Empty skips the `manifest-<platform>` copy.
    pub platform: String,
    /// Release tag stored in the manifest.
    pub tag: String,
    /// Product version stored in the manifest.
    pub product_version: String,
    /// Source revision stored in the manifest.
    pub source_commit: String,
    /// Optional C library requirement stored in `libc_required`.
    pub libc_required: String,
    /// Tier label stored in the manifest.
    pub tier: String,
}

/// A pack: buckets, installer links, optional reuse of `previous_dir`.
#[derive(Debug, Clone)]
pub struct PackRequest {
    /// Tree to walk.
    pub source: PathBuf,
    /// Directory that receives chunks and `release-chunks.json`.
    pub output: PathBuf,
    /// Caps and grouping.
    pub settings: ChunkSettings,
    /// Platform label. Empty skips the `manifest-<platform>` copy.
    pub platform: String,
    /// Release tag stored in the manifest.
    pub tag: String,
    /// Product version stored in the manifest.
    pub product_version: String,
    /// Source revision stored in the manifest.
    pub source_commit: String,
    /// Optional C library requirement stored in `libc_required`.
    pub libc_required: String,
    /// Tier label stored in the manifest.
    pub tier: String,
    /// Previous manifest directory. `None` packs from scratch.
    pub previous_dir: Option<PathBuf>,
}

/// Counts from a successful [`verify_tree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Chunk rows that matched their files.
    pub chunks: usize,
    /// Member paths checked across those chunks.
    pub files: usize,
}

struct Labels<'a> {
    platform: &'a str,
    tag: &'a str,
    product_version: &'a str,
    source_commit: &'a str,
    libc_required: &'a str,
    tier: &'a str,
}

struct Job<'a> {
    output: &'a Path,
    codec: &'a dyn ChunkCodec,
    cap: u64,
    previous: Option<&'a ChunkManifest>,
    previous_dir: Option<&'a Path>,
    pack: bool,
    tier_groups: &'a [String],
}

struct Prepared {
    name: String,
    group: String,
    buckets: u32,
    tar_path: PathBuf,
    content_key: String,
    files: Vec<String>,
    links: Vec<ChunkLink>,
    unpacked: u64,
}

/// Groups the tree and counts the pieces a later split would write.
///
/// # Errors
///
/// `chunk_cap_invalid`, `path_rejected`, `path_too_long`, `io_error` or `rules_invalid`.
pub fn plan_tree(source: &Path, settings: &ChunkSettings) -> Result<ReleasePlan, ChunkError> {
    settings.enforce_cap()?;
    let groups = scan::collect(source, settings, false)?;
    let mut planned = Vec::new();
    for (group, units) in groups {
        let buckets = bucket_count(&group, &units, settings, None)?;
        let slices = assign_buckets(&units, buckets)?;
        let mut estimated = 0u32;
        let mut raw_bytes = 0u64;
        let mut files = 0u64;
        for slice in slices {
            if slice.is_empty() {
                continue;
            }
            estimated = estimated.saturating_add(estimate_pieces(&slice, settings.max_chunk_bytes));
            raw_bytes = raw_bytes.saturating_add(slice.iter().map(Unit::unpacked).sum());
            let listed = u64::try_from(file_list(&slice, false).len())
                .map_err(|_| ChunkError::new("rules_invalid", "file count does not fit"))?;
            files = files.saturating_add(listed);
        }
        planned.push(PlannedGroup {
            group,
            files,
            estimated_chunks: estimated,
            raw_bytes,
            compressed_bytes: None,
        });
    }
    Ok(ReleasePlan { groups: planned })
}

/// Plans from compressed sizes: cuts the tree with `codec` into a fresh directory under
/// `scratch`, takes the chunk count and compressed bytes of each group from that cut, and
/// removes the directory again, also on error (ADR-8846, plan row C-09).
///
/// [`plan_tree`] estimates from the uncompressed tar size, so with a compressing codec it can
/// count more chunks than `cut` writes. This plan counts what `cut` writes with the same
/// settings and codec, at the cost of compressing the tree once. Groups `cut` writes no chunk
/// for keep `estimated_chunks` 0 and `compressed_bytes` `Some(0)`.
///
/// # Errors
///
/// The [`plan_tree`] and [`cut_tree`] errors, and `io_error` when `scratch` cannot be used.
pub fn plan_tree_compressed(
    source: &Path,
    settings: &ChunkSettings,
    codec: &dyn ChunkCodec,
    scratch: &Path,
) -> Result<ReleasePlan, ChunkError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut plan = plan_tree(source, settings)?;
    let output = scratch.join(format!(
        ".regia-chunk-plan-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&output).map_err(|err| ChunkError::io(&output, &err))?;
    let request = CutRequest {
        source: source.to_path_buf(),
        output: output.clone(),
        settings: settings.clone(),
        platform: String::new(),
        tag: String::new(),
        product_version: String::new(),
        source_commit: String::new(),
        libc_required: String::new(),
        tier: String::new(),
    };
    let cut = cut_tree(&request, codec);
    let _ = std::fs::remove_dir_all(&output);
    let manifest = cut?;
    for group in &mut plan.groups {
        let (count, bytes) = manifest
            .chunks
            .iter()
            .filter(|chunk| chunk.group == group.group)
            .fold((0u32, 0u64), |(count, bytes), chunk| {
                (count.saturating_add(1), bytes.saturating_add(chunk.size))
            });
        group.estimated_chunks = count;
        group.compressed_bytes = Some(bytes);
    }
    Ok(plan)
}

/// Cuts `request.source` into chunks under the cap.
///
/// # Errors
///
/// `chunk_cap_invalid`, `chunk_over_cap`, `path_rejected`, `path_too_long`, `io_error`,
/// `zstd_missing`, `zstd_failed`, `tar_invalid` or `rules_invalid`.
pub fn cut_tree(request: &CutRequest, codec: &dyn ChunkCodec) -> Result<ChunkManifest, ChunkError> {
    run(
        &request.source,
        &request.output,
        &request.settings,
        Labels {
            platform: &request.platform,
            tag: &request.tag,
            product_version: &request.product_version,
            source_commit: &request.source_commit,
            libc_required: &request.libc_required,
            tier: &request.tier,
        },
        None,
        false,
        codec,
    )
}

/// Packs a release. Symlinks and special files are `not_regular`. A previous manifest may
/// reuse compressed bytes and must keep non-tier groups identical.
///
/// # Errors
///
/// The cut errors, plus `not_regular`, `tier_chunk_differs`, `reuse_mismatch` and `manifest_invalid`.
pub fn pack_release(request: &PackRequest, codec: &dyn ChunkCodec) -> Result<ChunkManifest, ChunkError> {
    run(
        &request.source,
        &request.output,
        &request.settings,
        Labels {
            platform: &request.platform,
            tag: &request.tag,
            product_version: &request.product_version,
            source_commit: &request.source_commit,
            libc_required: &request.libc_required,
            tier: &request.tier,
        },
        request.previous_dir.as_deref(),
        true,
        codec,
    )
}

/// Checks the manifest, the chunk hashes, the cap and the tar member names.
///
/// # Errors
///
/// `chunk_cap_invalid`, `sha256_mismatch`, `size_mismatch`, `content_key_mismatch`,
/// `member_mismatch`, `link_mismatch`, `chunk_over_cap`, `verify_failed`, `manifest_invalid`,
/// `io_error`, `zstd_missing` or `zstd_failed`.
pub fn verify_tree(dir: &Path, cap: u64) -> Result<VerifyReport, ChunkError> {
    if cap == 0 || cap >= crate::TWO_GIB {
        return Err(ChunkError::new(
            "chunk_cap_invalid",
            format!("verify cap {cap} must be greater than 0 and below 2 GiB"),
        ));
    }
    let manifest = crate::manifest::read_manifest(&dir.join(CANONICAL_FILE))?;
    let mut files = 0usize;
    for chunk in &manifest.chunks {
        if chunk.size > cap {
            return Err(ChunkError::new(
                "chunk_over_cap",
                format!(
                    "chunk {} is {} bytes, above the cap of {cap}",
                    chunk.name, chunk.size
                ),
            ));
        }
        let asset = dir.join(&chunk.asset);
        let size = codec::file_len(&asset)?;
        if size != chunk.size {
            return Err(ChunkError::new(
                "size_mismatch",
                format!(
                    "chunk {} is {size} bytes on disk and {} in the manifest",
                    chunk.name, chunk.size
                ),
            ));
        }
        let digest = hex_encode(&sha256_file(&asset)?);
        if digest != chunk.sha256 {
            return Err(ChunkError::new(
                "sha256_mismatch",
                format!(
                    "chunk {} hash {digest} does not match {}",
                    chunk.name, chunk.sha256
                ),
            ));
        }
        let tar_path = dir.join(format!(".verify-{}.tar", chunk.name));
        let remove = Remove(&tar_path);
        codec_for(&chunk.asset)?.decompress_file(&asset, &tar_path)?;
        let key = hex_encode(&sha256_file(&tar_path)?);
        if key != chunk.content_key {
            return Err(ChunkError::new(
                "content_key_mismatch",
                format!(
                    "chunk {} content key {key} does not match {}",
                    chunk.name, chunk.content_key
                ),
            ));
        }
        let members = tar::read_members(&tar_path)?;
        let mut names: Vec<String> = members.into_iter().map(|member| member.name).collect();
        names.sort();
        let mut listed = chunk.files.clone();
        listed.sort();
        if names != listed {
            return Err(ChunkError::new(
                "member_mismatch",
                format!("chunk {} members do not match its file list", chunk.name),
            ));
        }
        files = files.saturating_add(listed.len());
        drop(remove);
    }
    for link in &manifest.links {
        let found = manifest
            .chunks
            .iter()
            .any(|chunk| chunk.files.iter().any(|file| file == &link.target));
        if !found {
            return Err(ChunkError::new(
                "link_mismatch",
                format!(
                    "link {} points at {}, which no chunk stores",
                    link.path, link.target
                ),
            ));
        }
    }
    Ok(VerifyReport {
        chunks: manifest.chunks.len(),
        files,
    })
}

/// Directory fingerprints for the audit file.
///
/// # Errors
///
/// `io_error` or `path_rejected`.
pub fn audit_tree(root: &Path, settings: &AuditSettings) -> Result<Vec<DirPrint>, ChunkError> {
    use std::collections::BTreeMap;
    let mut by_dir: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry =
            entry.map_err(|err| ChunkError::new("io_error", format!("walking {}: {err}", root.display())))?;
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.depth() > usize::try_from(settings.depth).unwrap_or(usize::MAX) {
            continue;
        }
        let rel = slash_rel(root, entry.path())?;
        let (dir, name) = match rel.rsplit_once('/') {
            Some((dir, name)) => (dir.to_string(), name.to_string()),
            None => (String::new(), rel.clone()),
        };
        let ext = name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();
        if !settings
            .extensions
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&ext))
        {
            continue;
        }
        if ext == "bin"
            && let Some(magic) = &settings.magic
            && !starts_with_magic(entry.path(), magic)?
        {
            continue;
        }
        let len = fs::metadata(entry.path())
            .map_err(|err| ChunkError::io(entry.path(), &err))?
            .len();
        by_dir.entry(dir).or_default().push((name, len));
    }
    let mut prints = Vec::new();
    for (path, mut files) in by_dir {
        files.sort();
        let mut buf = Vec::new();
        for (name, len) in &files {
            buf.extend(name.as_bytes());
            buf.push(0);
            buf.extend(len.to_le_bytes());
        }
        prints.push(DirPrint {
            path,
            hash: content_hash(&buf),
        });
    }
    Ok(prints)
}

/// Writes `dir_prints.txt` lines of `path`, a tab and the 16-hex fingerprint.
///
/// # Errors
///
/// `io_error` or `path_rejected`.
pub fn write_dir_prints(root: &Path, settings: &AuditSettings, dest: &Path) -> Result<(), ChunkError> {
    let prints = audit_tree(root, settings)?;
    let mut text = String::new();
    for print in &prints {
        let _ = writeln!(text, "{}\t{}", print.path, print.hash);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|err| ChunkError::io(parent, &err))?;
    }
    let tmp = dest.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp).map_err(|err| ChunkError::io(&tmp, &err))?;
        file.write_all(text.as_bytes())
            .map_err(|err| ChunkError::io(&tmp, &err))?;
        file.flush().map_err(|err| ChunkError::io(&tmp, &err))?;
    }
    fs::rename(&tmp, dest).map_err(|err| ChunkError::io(dest, &err))?;
    Ok(())
}

fn run(
    source: &Path,
    output: &Path,
    settings: &ChunkSettings,
    labels: Labels<'_>,
    previous_dir: Option<&Path>,
    pack: bool,
    codec: &dyn ChunkCodec,
) -> Result<ChunkManifest, ChunkError> {
    settings.enforce_cap()?;
    check_label("platform", labels.platform)?;
    check_label("tier", labels.tier)?;
    let groups = scan::collect(source, settings, pack)?;
    if pack {
        refuse_symlinks(&groups)?;
    }
    let previous = match previous_dir {
        Some(dir) => Some(crate::manifest::read_manifest(&dir.join(CANONICAL_FILE))?),
        None => None,
    };
    let mut tars = Vec::new();
    let mut written = Vec::new();
    let ctx = Assemble {
        output,
        settings,
        platform: labels.platform,
        tag: labels.tag,
        product_version: labels.product_version,
        source_commit: labels.source_commit,
        libc_required: labels.libc_required,
        tier: labels.tier,
        groups: &groups,
        previous: previous.as_ref(),
        previous_dir,
        pack,
        codec,
    };
    let result = (|| {
        let manifest = assemble(&ctx, &mut tars, &mut written)?;
        write_manifest_files(output, &manifest)?;
        Ok(manifest)
    })();
    discard(&tars);
    if result.is_err() {
        discard(&written);
    }
    result
}

struct Assemble<'a> {
    output: &'a Path,
    settings: &'a ChunkSettings,
    platform: &'a str,
    tag: &'a str,
    product_version: &'a str,
    source_commit: &'a str,
    libc_required: &'a str,
    tier: &'a str,
    groups: &'a std::collections::BTreeMap<String, Vec<Unit>>,
    previous: Option<&'a ChunkManifest>,
    previous_dir: Option<&'a Path>,
    pack: bool,
    codec: &'a dyn ChunkCodec,
}

fn assemble(
    ctx: &Assemble<'_>,
    tars: &mut Vec<PathBuf>,
    written: &mut Vec<PathBuf>,
) -> Result<ChunkManifest, ChunkError> {
    let job = Job {
        output: ctx.output,
        codec: ctx.codec,
        cap: ctx.settings.max_chunk_bytes,
        previous: ctx.previous,
        previous_dir: ctx.previous_dir,
        pack: ctx.pack,
        tier_groups: &ctx.settings.tier_groups,
    };
    let mut chunks = Vec::new();
    let mut links = Vec::new();
    for (group, units) in ctx.groups {
        check_group(group)?;
        let buckets = if ctx.pack {
            bucket_count(group, units, ctx.settings, ctx.previous)?
        } else {
            1
        };
        let slices = assign_buckets(units, buckets)?;
        let mut prepared = Vec::new();
        for (bucket, slice) in slices.into_iter().enumerate() {
            if slice.is_empty() {
                continue;
            }
            let bucket_u32 = u32::try_from(bucket)
                .map_err(|_| ChunkError::new("rules_invalid", "bucket index does not fit"))?;
            emit_bucket(&job, group, bucket_u32, buckets, &slice, &mut prepared)?;
        }
        if !tier_allows(&job, group, &prepared)? {
            discard(
                &prepared
                    .iter()
                    .map(|piece| piece.tar_path.clone())
                    .collect::<Vec<_>>(),
            );
            return Err(ChunkError::new(
                "tier_chunk_differs",
                format!("group {group} changed outside the tier groups"),
            ));
        }
        for piece in prepared {
            tars.push(piece.tar_path.clone());
            links.extend(piece.links.clone());
            chunks.push(finish_piece(&job, &piece, written)?);
        }
    }
    chunks.sort_by(|left, right| left.group.cmp(&right.group).then(left.name.cmp(&right.name)));
    links.sort_by(|left, right| left.path.cmp(&right.path).then(left.target.cmp(&right.target)));
    Ok(ChunkManifest {
        format_version: FORMAT_VERSION,
        platform: ctx.platform.to_string(),
        tag: ctx.tag.to_string(),
        product_version: ctx.product_version.to_string(),
        source_commit: ctx.source_commit.to_string(),
        libc_required: ctx.libc_required.to_string(),
        tier: ctx.tier.to_string(),
        chunks,
        links,
    })
}

fn emit_bucket(
    job: &Job<'_>,
    group: &str,
    bucket: u32,
    bucket_count: u32,
    units: &[Unit],
    prepared: &mut Vec<Prepared>,
) -> Result<(), ChunkError> {
    let slices = split_to_fit(job, units)?;
    for (offset, slice) in slices.iter().enumerate() {
        let index = u32::try_from(offset)
            .map_err(|_| ChunkError::new("rules_invalid", "chunk index does not fit"))?;
        let name = chunk_name(group, bucket, bucket_count, index);
        prepared.push(prepare_piece(job, &name, group, bucket_count, slice)?);
    }
    Ok(())
}

/// Compresses `units`. A piece over the cap is split in half when it holds more than one
/// unit. One unit that does not fit is `chunk_over_cap` and leaves no chunk file.
fn split_to_fit(job: &Job<'_>, units: &[Unit]) -> Result<Vec<Vec<Unit>>, ChunkError> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let size = probe_compressed_len(job, units)?;
    if size <= job.cap {
        return Ok(vec![units.to_vec()]);
    }
    if units.len() == 1 {
        return Err(ChunkError::new(
            "chunk_over_cap",
            format!("a file is {size} bytes, above the cap of {}", job.cap),
        ));
    }
    let mid = units.len() / 2;
    let mut parts = split_to_fit(job, &units[..mid])?;
    parts.extend(split_to_fit(job, &units[mid..])?);
    Ok(parts)
}

fn probe_compressed_len(job: &Job<'_>, units: &[Unit]) -> Result<u64, ChunkError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let tar_path = job.output.join(format!(".probe-{id}.tar"));
    let part_path = job.output.join(format!(".probe-{id}.part"));
    let measured = (|| -> Result<u64, ChunkError> {
        tar::write_units(&tar_path, &units_for_tar(units, job.pack))?;
        job.codec.compress_file(&tar_path, &part_path)?;
        codec::file_len(&part_path)
    })();
    let _ = fs::remove_file(&tar_path);
    let _ = fs::remove_file(&part_path);
    measured
}

fn prepare_piece(
    job: &Job<'_>,
    name: &str,
    group: &str,
    buckets: u32,
    units: &[Unit],
) -> Result<Prepared, ChunkError> {
    let tar_path = job.output.join(format!(".work-{name}.tar"));
    let stored = units_for_tar(units, job.pack);
    tar::write_units(&tar_path, &stored)?;
    let content_key = hex_encode(&sha256_file(&tar_path)?);
    let files = file_list(units, job.pack);
    let links = link_list(units, name);
    let unpacked = units.iter().map(Unit::unpacked).sum();
    Ok(Prepared {
        name: name.to_string(),
        group: group.to_string(),
        buckets,
        tar_path,
        content_key,
        files,
        links,
        unpacked,
    })
}

fn finish_piece(
    job: &Job<'_>,
    piece: &Prepared,
    written: &mut Vec<PathBuf>,
) -> Result<ChunkRecord, ChunkError> {
    let asset_name = format!("{}.{}", piece.name, job.codec.extension());
    let final_path = job.output.join(&asset_name);
    let partial = job.output.join(format!(".{asset_name}.partial"));
    if let Some(previous) = find_reuse(job.previous, &piece.group, &piece.files, &piece.content_key) {
        let Some(dir) = job.previous_dir else {
            return Err(ChunkError::new(
                "reuse_mismatch",
                "a reused chunk has no previous directory",
            ));
        };
        let src = dir.join(&previous.asset);
        if !src.is_file() {
            let _ = fs::remove_file(&partial);
            return Err(ChunkError::new(
                "reuse_mismatch",
                format!("previous chunk {} is missing at {}", previous.name, src.display()),
            ));
        }
        codec::copy_file(&src, &partial)?;
    } else {
        job.codec.compress_file(&piece.tar_path, &partial)?;
    }
    let size = codec::file_len(&partial)?;
    if size > job.cap {
        let _ = fs::remove_file(&partial);
        return Err(ChunkError::new(
            "chunk_over_cap",
            format!(
                "chunk {} is {size} bytes, above the cap of {}",
                piece.name, job.cap
            ),
        ));
    }
    let sha256 = hex_encode(&sha256_file(&partial)?);
    fs::rename(&partial, &final_path).map_err(|err| ChunkError::io(&final_path, &err))?;
    written.push(final_path);
    Ok(ChunkRecord {
        name: piece.name.clone(),
        group: piece.group.clone(),
        buckets: piece.buckets,
        asset: asset_name,
        sha256,
        size,
        unpacked: piece.unpacked,
        files: piece.files.clone(),
        content_key: piece.content_key.clone(),
    })
}

fn tier_allows(job: &Job<'_>, group: &str, prepared: &[Prepared]) -> Result<bool, ChunkError> {
    let Some(previous) = job.previous else {
        return Ok(true);
    };
    if job.tier_groups.iter().any(|allowed| allowed == group) {
        return Ok(true);
    }
    let mut fresh: Vec<(Vec<String>, String)> = prepared
        .iter()
        .map(|piece| (piece.files.clone(), piece.content_key.clone()))
        .collect();
    let mut old: Vec<(Vec<String>, String)> = previous
        .chunks
        .iter()
        .filter(|chunk| chunk.group == group)
        .map(|chunk| (chunk.files.clone(), chunk.content_key.clone()))
        .collect();
    fresh.sort();
    old.sort();
    Ok(fresh == old)
}

fn find_reuse<'a>(
    previous: Option<&'a ChunkManifest>,
    group: &str,
    files: &[String],
    content_key: &str,
) -> Option<&'a ChunkRecord> {
    previous?
        .chunks
        .iter()
        .find(|chunk| chunk.group == group && chunk.files == files && chunk.content_key == content_key)
}

fn bucket_count(
    group: &str,
    units: &[Unit],
    settings: &ChunkSettings,
    previous: Option<&ChunkManifest>,
) -> Result<u32, ChunkError> {
    if group == "core" {
        return Ok(1);
    }
    let floor = previous
        .and_then(|manifest| manifest.chunks.iter().find(|chunk| chunk.group == group))
        .map_or(1, |chunk| chunk.buckets.max(1));
    let mut count = 1u32;
    loop {
        if bucket_layout_ok(units, count, settings.bucket_raw)? || count >= 512 {
            break;
        }
        count = count.saturating_mul(2);
    }
    Ok(count.max(floor))
}

fn bucket_layout_ok(units: &[Unit], count: u32, raw_limit: u64) -> Result<bool, ChunkError> {
    let slots =
        usize::try_from(count).map_err(|_| ChunkError::new("rules_invalid", "bucket count does not fit"))?;
    let mut sums = vec![0u64; slots];
    let mut counts = vec![0u32; slots];
    for unit in units {
        let index = usize::try_from(bucket_index(&unit.stored.rel, count)?)
            .map_err(|_| ChunkError::new("rules_invalid", "bucket index does not fit"))?;
        sums[index] = sums[index].saturating_add(unit.unpacked());
        counts[index] = counts[index].saturating_add(1);
    }
    Ok(sums
        .iter()
        .zip(counts)
        .all(|(sum, n)| n <= 1 || *sum <= raw_limit))
}

fn assign_buckets(units: &[Unit], count: u32) -> Result<Vec<Vec<Unit>>, ChunkError> {
    let slots =
        usize::try_from(count).map_err(|_| ChunkError::new("rules_invalid", "bucket count does not fit"))?;
    let mut buckets = vec![Vec::new(); slots];
    for unit in units {
        let index = usize::try_from(bucket_index(&unit.stored.rel, count)?)
            .map_err(|_| ChunkError::new("rules_invalid", "bucket index does not fit"))?;
        buckets[index].push(unit.clone());
    }
    Ok(buckets)
}

fn estimate_pieces(units: &[Unit], cap: u64) -> u32 {
    if units.is_empty() {
        return 0;
    }
    if archive_bytes(units) <= cap || units.len() == 1 {
        return 1;
    }
    let mid = units.len() / 2;
    estimate_pieces(&units[..mid], cap).saturating_add(estimate_pieces(&units[mid..], cap))
}

fn archive_bytes(units: &[Unit]) -> u64 {
    let mut total = 1024u64;
    for unit in units {
        total = total.saturating_add(member_bytes(
            &unit.stored.rel,
            unit.stored.kind == NodeKind::File,
            unit.stored.size,
        ));
        for link in &unit.links {
            total = total.saturating_add(member_bytes(&link.rel, false, 0));
        }
    }
    total
}

fn member_bytes(rel: &str, is_file: bool, size: u64) -> u64 {
    let mut total = 512u64;
    if rel.len() > 100 {
        total = total.saturating_add(1024);
    }
    if is_file {
        total = total.saturating_add(size.div_ceil(512) * 512);
    }
    total
}

fn chunk_name(group: &str, bucket: u32, bucket_count: u32, index: u32) -> String {
    if bucket_count > 1 {
        format!("{group}-b{bucket:02}-{index:03}")
    } else {
        format!("{group}-{index:03}")
    }
}

fn units_for_tar(units: &[Unit], pack: bool) -> Vec<Unit> {
    if !pack {
        return units.to_vec();
    }
    units
        .iter()
        .cloned()
        .map(|mut unit| {
            unit.links.clear();
            unit
        })
        .collect()
}

fn file_list(units: &[Unit], pack: bool) -> Vec<String> {
    let mut names = Vec::new();
    for unit in units {
        if pack {
            names.push(unit.stored.rel.clone());
        } else {
            names.extend(unit.file_names());
        }
    }
    names.sort();
    names
}

fn link_list(units: &[Unit], chunk: &str) -> Vec<ChunkLink> {
    let mut links = Vec::new();
    for unit in units {
        for link in &unit.links {
            links.push(ChunkLink {
                path: link.rel.clone(),
                target: unit.stored.rel.clone(),
                chunk: chunk.to_string(),
            });
        }
    }
    links.sort_by(|left, right| left.path.cmp(&right.path));
    links
}

fn refuse_symlinks(groups: &std::collections::BTreeMap<String, Vec<Unit>>) -> Result<(), ChunkError> {
    for units in groups.values() {
        for unit in units {
            if unit.stored.kind != NodeKind::File {
                return Err(ChunkError::new(
                    "not_regular",
                    format!("{} is not a regular file", unit.stored.rel),
                ));
            }
        }
    }
    Ok(())
}

fn check_group(group: &str) -> Result<(), ChunkError> {
    let ok = !group.is_empty()
        && group
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if ok {
        Ok(())
    } else {
        Err(ChunkError::new(
            "rules_invalid",
            format!("group `{group}` must be [A-Za-z0-9_-]+"),
        ))
    }
}

fn check_label(field: &str, value: &str) -> Result<(), ChunkError> {
    if value.is_empty() {
        return Ok(());
    }
    let ok = value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-');
    if ok {
        Ok(())
    } else {
        Err(ChunkError::new(
            "path_rejected",
            format!("{field} `{value}` must be [a-z0-9_-]+ when it is used in a file name"),
        ))
    }
}

fn discard(paths: &[PathBuf]) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

fn slash_rel(root: &Path, path: &Path) -> Result<String, ChunkError> {
    let rel = path.strip_prefix(root).map_err(|_| {
        ChunkError::new(
            "path_rejected",
            format!("{} is outside {}", path.display(), root.display()),
        )
    })?;
    let mut text = String::new();
    for (index, component) in rel.components().enumerate() {
        match component {
            std::path::Component::Normal(part) => {
                let part = part.to_str().ok_or_else(|| {
                    ChunkError::new("path_rejected", format!("{} is not UTF-8", path.display()))
                })?;
                if index > 0 {
                    text.push('/');
                }
                text.push_str(part);
            }
            _ => {
                return Err(ChunkError::new(
                    "path_rejected",
                    format!("{} is not a relative path of normal components", path.display()),
                ));
            }
        }
    }
    Ok(text)
}

fn starts_with_magic(path: &Path, magic: &[u8]) -> Result<bool, ChunkError> {
    use std::io::Read;
    let mut file = fs::File::open(path).map_err(|err| ChunkError::io(path, &err))?;
    let mut buf = vec![0u8; magic.len()];
    let n = file.read(&mut buf).map_err(|err| ChunkError::io(path, &err))?;
    Ok(n == magic.len() && buf == magic)
}

fn codec_for(asset: &str) -> Result<Box<dyn ChunkCodec>, ChunkError> {
    if asset.ends_with(".tar.zst") {
        Ok(Box::new(crate::ZstdCli::standard()))
    } else if asset.ends_with(".tar") {
        Ok(Box::new(crate::Identity))
    } else {
        Err(ChunkError::new(
            "verify_failed",
            format!("chunk asset `{asset}` has no tar or tar.zst suffix"),
        ))
    }
}

struct Remove<'a>(&'a Path);

impl Drop for Remove<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}
