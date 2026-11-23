// SPDX-License-Identifier: MPL-2.0
//! `regia engine chunk plan|cut|pack|verify` (ADR-8404).
//!
//! Plans, writes and checks release chunks under a byte cap. `--json` prints one document
//! (schema 1). Error codes are listed in `book/agents/release-chunks.md`.

use std::fs;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use regia_release_chunks::{
    ChunkError, ChunkSettings, CutRequest, DEFAULT_MAX_CHUNK_BYTES, PackRequest, ZstdCli, cut_tree,
    load_rules_str, pack_release, parse_byte_size, plan_tree, plan_tree_compressed, verify_tree,
};
use serde_json::{Value, json};

/// Version of the `--json` documents this command prints.
pub const JSON_SCHEMA: u32 = 1;

/// `regia engine chunk` subcommands.
#[derive(Debug, Subcommand)]
pub enum ChunkCommand {
    /// Count groups and how many pieces a later split would write.
    Plan(PlanArgs),
    /// Write chunks under the cap. A group that does not fit is split until each piece does.
    Cut(WriteArgs),
    /// Pack with release buckets. `--previous` may reuse a chunk's compressed bytes.
    Pack(PackArgs),
    /// Check hashes, the cap and archive member names.
    Verify(VerifyArgs),
}

/// Flags shared by `plan`, `cut` and `pack`.
#[derive(Debug, Args)]
pub struct ChunkCommon {
    /// Compressed-size cap (`2GiB`, `512MiB`, `64KiB`, or a byte count). Default 1.9 GiB, always below 2 GiB.
    #[arg(long)]
    pub max_chunk_bytes: Option<String>,
    /// Raw-byte target for one pack bucket. Default 1.6 GiB. One file may be larger; the compressed cap still applies.
    #[arg(long)]
    pub bucket_raw: Option<String>,
    /// TOML grouping rules. When set, these replace the built-in directory layout.
    #[arg(long)]
    pub rules: Option<PathBuf>,
    /// Store a file that has any execute bit as mode 0755. Otherwise files are 0644.
    #[arg(long, default_value_t = false)]
    pub preserve_exec: bool,
    /// Platform label stored in the manifest. Empty skips `manifest-<platform>.json`.
    #[arg(long, default_value = "")]
    pub platform: String,
    /// Release tag stored in the manifest.
    #[arg(long, default_value = "")]
    pub tag: String,
    /// Product version stored in the manifest.
    #[arg(long, default_value = "")]
    pub product_version: String,
    /// Source revision stored in the manifest.
    #[arg(long, default_value = "")]
    pub source_commit: String,
    /// Optional C library requirement stored in `libc_required`.
    #[arg(long, default_value = "")]
    pub libc_required: String,
    /// Tier label stored in the manifest.
    #[arg(long, default_value = "")]
    pub tier: String,
    /// zstd level, 1..=19. Default 3.
    #[arg(long, default_value_t = 3)]
    pub zstd_level: u8,
    /// zstd worker threads. At least 1.
    #[arg(long, default_value_t = 1)]
    pub zstd_threads: u32,
    /// Pass `--long=27` to zstd.
    #[arg(long, default_value_t = false)]
    pub zstd_long: bool,
    /// Print the JSON document on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `regia engine chunk plan`.
#[derive(Debug, Args)]
pub struct PlanArgs {
    /// Tree to group.
    pub source: PathBuf,
    /// Count from compressed sizes: cut the tree with zstd into a scratch directory, report
    /// the chunks and compressed bytes `cut` writes, and remove the scratch (ADR-8846).
    #[arg(long, default_value_t = false)]
    pub compressed: bool,
    /// Directory for the `--compressed` scratch cut. Default: the system temp directory.
    #[arg(long)]
    pub scratch: Option<PathBuf>,
    /// Caps, rules and labels.
    #[command(flatten)]
    pub common: ChunkCommon,
}

/// `regia engine chunk cut`.
#[derive(Debug, Args)]
pub struct WriteArgs {
    /// Tree to cut.
    pub source: PathBuf,
    /// Directory that receives the chunks and `release-chunks.json`.
    #[arg(long)]
    pub out: PathBuf,
    /// Caps, rules and labels.
    #[command(flatten)]
    pub common: ChunkCommon,
}

/// `regia engine chunk pack`.
#[derive(Debug, Args)]
pub struct PackArgs {
    /// Tree to pack.
    pub source: PathBuf,
    /// Directory that receives the chunks and `release-chunks.json`.
    #[arg(long)]
    pub out: PathBuf,
    /// Previous manifest directory. Matching content keys copy that chunk's compressed bytes.
    #[arg(long)]
    pub previous: Option<PathBuf>,
    /// Caps, rules and labels.
    #[command(flatten)]
    pub common: ChunkCommon,
}

/// `regia engine chunk verify`.
#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// Directory that holds `release-chunks.json` and the chunk files.
    pub dir: PathBuf,
    /// Cap to enforce (`2GiB`, `512MiB`, `64KiB`, or a byte count). Default 1.9 GiB.
    #[arg(long)]
    pub cap: Option<String>,
    /// Print the JSON document on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Runs a `regia engine chunk` subcommand.
///
/// # Errors
///
/// A chunk error prints `code: message` and, with `--json`, a schema-1 document on stdout.
/// The process exits non-zero.
pub fn run(command: &ChunkCommand) -> Result<()> {
    match command {
        ChunkCommand::Plan(args) => plan(args),
        ChunkCommand::Cut(args) => cut(args),
        ChunkCommand::Pack(args) => pack(args),
        ChunkCommand::Verify(args) => verify(args),
    }
}

fn plan(args: &PlanArgs) -> Result<()> {
    let planned = if args.compressed {
        prepare(&args.common).and_then(|(settings, codec)| {
            let scratch = args.scratch.clone().unwrap_or_else(std::env::temp_dir);
            plan_tree_compressed(&args.source, &settings, &codec, &scratch)
        })
    } else {
        settings(&args.common).and_then(|settings| plan_tree(&args.source, &settings))
    };
    match planned {
        Ok(planned) => {
            let groups: Vec<Value> = planned
                .groups
                .iter()
                .map(|group| {
                    json!({
                        "group": group.group,
                        "files": group.files,
                        "estimated_chunks": group.estimated_chunks,
                        "raw_bytes": group.raw_bytes,
                        "compressed_bytes": group.compressed_bytes,
                    })
                })
                .collect();
            let human = planned
                .groups
                .iter()
                .map(|group| {
                    let compressed = group
                        .compressed_bytes
                        .map(|bytes| format!("\tcompressed={bytes}"))
                        .unwrap_or_default();
                    format!(
                        "{group}\tfiles={files}\tchunks={chunks}\traw={raw}{compressed}",
                        group = group.group,
                        files = group.files,
                        chunks = group.estimated_chunks,
                        raw = group.raw_bytes,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            emit(
                args.common.json,
                &json!({
                    "schema": JSON_SCHEMA,
                    "ok": true,
                    "command": "plan",
                    "groups": groups,
                }),
                &human,
            )
        }
        Err(err) => refuse(args.common.json, "plan", err),
    }
}

fn cut(args: &WriteArgs) -> Result<()> {
    let (settings, codec) = match prepare(&args.common) {
        Ok(pair) => pair,
        Err(err) => return refuse(args.common.json, "cut", err),
    };
    let request = CutRequest {
        source: args.source.clone(),
        output: args.out.clone(),
        settings,
        platform: args.common.platform.clone(),
        tag: args.common.tag.clone(),
        product_version: args.common.product_version.clone(),
        source_commit: args.common.source_commit.clone(),
        libc_required: args.common.libc_required.clone(),
        tier: args.common.tier.clone(),
    };
    match cut_tree(&request, &codec) {
        Ok(manifest) => emit(
            args.common.json,
            &manifest_doc("cut", &manifest),
            &manifest_lines(&manifest),
        ),
        Err(err) => refuse(args.common.json, "cut", err),
    }
}

fn pack(args: &PackArgs) -> Result<()> {
    let (settings, codec) = match prepare(&args.common) {
        Ok(pair) => pair,
        Err(err) => return refuse(args.common.json, "pack", err),
    };
    let request = PackRequest {
        source: args.source.clone(),
        output: args.out.clone(),
        settings,
        platform: args.common.platform.clone(),
        tag: args.common.tag.clone(),
        product_version: args.common.product_version.clone(),
        source_commit: args.common.source_commit.clone(),
        libc_required: args.common.libc_required.clone(),
        tier: args.common.tier.clone(),
        previous_dir: args.previous.clone(),
    };
    match pack_release(&request, &codec) {
        Ok(manifest) => emit(
            args.common.json,
            &manifest_doc("pack", &manifest),
            &manifest_lines(&manifest),
        ),
        Err(err) => refuse(args.common.json, "pack", err),
    }
}

fn verify(args: &VerifyArgs) -> Result<()> {
    let cap = match &args.cap {
        Some(text) => match parse_byte_size(text) {
            Ok(cap) => cap,
            Err(err) => return refuse(args.json, "verify", err),
        },
        None => DEFAULT_MAX_CHUNK_BYTES,
    };
    match verify_tree(&args.dir, cap) {
        Ok(report) => emit(
            args.json,
            &json!({
                "schema": JSON_SCHEMA,
                "ok": true,
                "command": "verify",
                "chunks": report.chunks,
                "files": report.files,
            }),
            &format!(
                "chunks={chunks}\tfiles={files}",
                chunks = report.chunks,
                files = report.files
            ),
        ),
        Err(err) => refuse(args.json, "verify", err),
    }
}

fn prepare(common: &ChunkCommon) -> Result<(ChunkSettings, ZstdCli), ChunkError> {
    let settings = settings(common)?;
    let codec = ZstdCli::new(common.zstd_level, common.zstd_threads, common.zstd_long)?;
    Ok((settings, codec))
}

fn settings(common: &ChunkCommon) -> Result<ChunkSettings, ChunkError> {
    let mut settings = ChunkSettings::default();
    if let Some(text) = &common.max_chunk_bytes {
        settings.max_chunk_bytes = parse_byte_size(text)?;
    }
    if let Some(text) = &common.bucket_raw {
        settings.bucket_raw = parse_byte_size(text)?;
    }
    settings.preserve_exec = common.preserve_exec;
    if let Some(path) = &common.rules {
        let text = fs::read_to_string(path).map_err(|err| ChunkError::io(path, &err))?;
        settings.rules = Some(load_rules_str(&text)?);
    }
    Ok(settings)
}

fn manifest_doc(command: &str, manifest: &regia_release_chunks::ChunkManifest) -> Value {
    json!({
        "schema": JSON_SCHEMA,
        "ok": true,
        "command": command,
        "format_version": manifest.format_version,
        "chunks": manifest.chunks.iter().map(|chunk| json!({
            "name": chunk.name,
            "asset": chunk.asset,
            "size": chunk.size,
            "sha256": chunk.sha256,
        })).collect::<Vec<_>>(),
    })
}

fn manifest_lines(manifest: &regia_release_chunks::ChunkManifest) -> String {
    manifest
        .chunks
        .iter()
        .map(|chunk| {
            format!(
                "{name}\t{size}\t{sha}",
                name = chunk.name,
                size = chunk.size,
                sha = chunk.sha256
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn emit(json_mode: bool, doc: &Value, human: &str) -> Result<()> {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(doc)?);
    } else if !human.is_empty() {
        println!("{human}");
    }
    Ok(())
}

fn refuse(json_mode: bool, command: &str, err: ChunkError) -> Result<()> {
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": JSON_SCHEMA,
                "ok": false,
                "command": command,
                "code": err.code,
                "message": &err.message,
            }))?
        );
    }
    let code = err.code;
    let message = err.message;
    bail!("{code}: {message}")
}
