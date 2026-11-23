// SPDX-License-Identifier: MPL-2.0
//! Texture header evidence for the shared source-lint stage (ADR-8536).
//!
//! These fixtures are container headers. The probe must not decode pixels, so the
//! block payloads are zeros. A warm scan and a settings edit are ordered by the
//! evidence key and `cache_hits`, not by the clock.

use std::path::{Path, PathBuf};

use regia_asset::TextureCookUsage;
use regia_asset_pipeline::asset_graph::{AssetEntry, AssetKind};
use regia_asset_pipeline::content_lint::ContentLimits;
use regia_asset_pipeline::registry_bridge::apply_texture_size_evidence;
use regia_asset_pipeline::source_lint::{
    self, ProduceLimits, SourceDocument, SourceLintPolicy, SourceLintReport,
};
use regia_asset_pipeline::source_lint_cache::{self, PublishRequest, ScanRequest};
use regia_asset_pipeline::texture_lint::{
    self, RULE_BUDGET_UNKNOWN, RULE_IMPOSSIBLE_DIMENSIONS, RULE_MISSING_MIP, RULE_USAGE_MISMATCH,
    RULE_WRONG_COMPRESSION, TextureLintOutcome, TextureLintRequest,
};
use regia_asset_pipeline::{TextureImporterSettings, TexturePolicy};
use regia_test_utils::unique_temp_dir;

const DECODE_BUDGET: u64 = 512 * 1024 * 1024;
const KTX2_ID: [u8; 12] = [
    0xAB, 0x4B, 0x54, 0x58, 0x20, 0x32, 0x30, 0xBB, 0x0D, 0x0A, 0x1A, 0x0A,
];

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent directory");
    }
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("part");
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).expect("temporary file");
    std::fs::rename(&tmp, path).expect("atomic rename");
}

fn write_max_size(source: &Path, importer: &str, max_size: u32) {
    let mut meta = regia_asset::MetaSidecar::new(importer, regia_asset::AssetId::new_random());
    meta.settings = Some(format!("(max_size: Some({max_size}))"));
    let text = meta.to_ron_string().expect("meta");
    write_atomic(&regia_asset::meta_path_for(source), text.as_bytes());
}

/// BC7 payload for `levels` stored mips. 16 bytes a 4×4 block.
fn bc7_payload(width: u32, height: u32, levels: u32) -> usize {
    let mut total = 0_usize;
    let mut x = width;
    let mut y = height;
    for _ in 0..levels {
        let blocks_x = usize::try_from(x.div_ceil(4)).expect("blocks");
        let blocks_y = usize::try_from(y.div_ceil(4)).expect("blocks");
        total = total.saturating_add(blocks_x.saturating_mul(blocks_y).saturating_mul(16));
        x = (x / 2).max(1);
        y = (y / 2).max(1);
    }
    total
}

fn ktx2(vk_format: u32, width: u32, height: u32, levels: u32) -> Vec<u8> {
    let header = 80 + usize::try_from(levels).expect("levels") * 24;
    let mut bytes = vec![0_u8; header + bc7_payload(width, height, levels)];
    bytes[..12].copy_from_slice(&KTX2_ID);
    put_u32(&mut bytes, 12, vk_format);
    put_u32(&mut bytes, 16, 1);
    put_u32(&mut bytes, 20, width);
    put_u32(&mut bytes, 24, height);
    put_u32(&mut bytes, 36, 1);
    put_u32(&mut bytes, 40, levels);
    bytes
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0_u8; 45];
    bytes[..8].copy_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);
    bytes[12..16].copy_from_slice(b"IHDR");
    bytes[16..20].copy_from_slice(&width.to_be_bytes());
    bytes[20..24].copy_from_slice(&height.to_be_bytes());
    bytes
}

/// 64×64 BC1 with mip 0 and mip 1 (32×32). Payload bytes are zeros.
fn dds_bc1_two_levels() -> Vec<u8> {
    let mut bytes = vec![0_u8; 128 + 2048 + 512];
    bytes[..4].copy_from_slice(b"DDS ");
    put_u32(&mut bytes, 4, 124);
    put_u32(&mut bytes, 8, 0x1 | 0x2 | 0x4 | 0x1000 | 0x2_0000);
    put_u32(&mut bytes, 12, 64);
    put_u32(&mut bytes, 16, 64);
    put_u32(&mut bytes, 28, 2);
    put_u32(&mut bytes, 76, 32);
    put_u32(&mut bytes, 80, 0x4);
    bytes[84..88].copy_from_slice(b"DXT1");
    bytes
}

fn budget() -> u64 {
    let limits = ContentLimits::default().max_decoded_bytes;
    assert_eq!(limits, DECODE_BUDGET);
    limits
}

fn assess_at(
    path: &str,
    bytes: &[u8],
    platform: &str,
    settings: TextureImporterSettings,
    usages: &[TextureCookUsage],
) -> TextureLintOutcome {
    texture_lint::assess(&TextureLintRequest {
        path,
        bytes,
        file_len: None,
        platform,
        policy: TexturePolicy::default(),
        settings,
        usages,
        max_decoded_bytes: budget(),
    })
}

fn rule_ids(outcome: &TextureLintOutcome) -> Vec<&str> {
    let mut ids: Vec<&str> = outcome
        .findings
        .iter()
        .map(|finding| finding.rule_id.as_str())
        .collect();
    ids.sort_unstable();
    ids
}

fn report_rules(report: &SourceLintReport) -> Vec<String> {
    let mut ids: Vec<String> = report
        .findings
        .iter()
        .map(|finding| finding.rule_id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

fn project(label: &str) -> PathBuf {
    let root = unique_temp_dir(label);
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    root
}

fn scan_project(root: &Path, target: &str) -> source_lint_cache::ScanOutcome {
    source_lint_cache::scan(&ScanRequest {
        project: root.to_path_buf(),
        target: target.to_owned(),
        paths: Vec::new(),
        changed: None,
        strict: false,
        fix: false,
        cancel_file: None,
        report_path: None,
        material_profile: None,
        skip_staging: false,
    })
    .expect("scan")
}

fn produce_file(root: Option<&Path>, rel: &str, bytes: &[u8], target: &str) -> SourceLintReport {
    let policy = root.map_or_else(SourceLintPolicy::engine_defaults, |root| {
        SourceLintPolicy::load_project(root).expect("policy")
    });
    source_lint::produce_in(
        root,
        &[SourceDocument { path: rel, bytes }],
        &policy,
        target,
        &ProduceLimits::default(),
        &|| false,
    )
    .expect("produce")
}

#[test]
fn distinct_findings_do_not_share_a_rule() {
    let wrong = assess_at(
        "assets/pack_orm.ktx2",
        &ktx2(145, 8, 8, 4),
        "android",
        TextureImporterSettings::default(),
        &[],
    );
    assert_eq!(rule_ids(&wrong), vec![RULE_WRONG_COMPRESSION]);
    assert_eq!(wrong.buffers_allocated, 0);
    assert!(wrong.decoded_bytes_reserved > 0);

    let mips = assess_at(
        "assets/pack_orm.ktx2",
        &ktx2(145, 16, 16, 2),
        "linux",
        TextureImporterSettings::default(),
        &[],
    );
    assert_eq!(rule_ids(&mips), vec![RULE_MISSING_MIP]);

    let impossible = assess_at(
        "assets/bad.png",
        &png(0, 8),
        "linux",
        TextureImporterSettings::default(),
        &[],
    );
    assert_eq!(rule_ids(&impossible), vec![RULE_IMPOSSIBLE_DIMENSIONS]);
    assert_eq!(impossible.width, None);
    assert_eq!(impossible.decoded_bytes, None);
    assert_eq!(impossible.decoded_bytes_reserved, 0);
    assert_eq!(impossible.buffers_allocated, 0);
    assert!(!impossible.incomplete);

    let unknown_header = assess_at(
        "assets/short.dds",
        b"DDS ",
        "linux",
        TextureImporterSettings::default(),
        &[],
    );
    assert_eq!(rule_ids(&unknown_header), vec![RULE_BUDGET_UNKNOWN]);
    assert_ne!(rule_ids(&unknown_header), rule_ids(&impossible));

    let mismatch = assess_at(
        "assets/hero_normal.ktx2",
        &ktx2(146, 8, 8, 4),
        "linux",
        TextureImporterSettings::default(),
        &[],
    );
    assert_eq!(rule_ids(&mismatch), vec![RULE_USAGE_MISMATCH]);
    assert_eq!(
        mismatch
            .findings
            .iter()
            .find(|finding| finding.rule_id == RULE_USAGE_MISMATCH)
            .map(|finding| finding.location.as_str()),
        Some("/usage")
    );

    let mut ids = vec![
        rule_ids(&wrong)[0],
        rule_ids(&mips)[0],
        rule_ids(&impossible)[0],
        rule_ids(&mismatch)[0],
    ];
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 4);
}

#[test]
fn dds_passthrough_reports_the_surviving_edge() {
    let bytes = dds_bc1_two_levels();
    let before = bytes.clone();
    let open = assess_at(
        "assets/albedo.dds",
        &bytes,
        "linux",
        TextureImporterSettings::default(),
        &[],
    );
    assert_eq!(open.width, Some(64));
    assert_eq!(open.height, Some(64));
    assert_eq!(open.decoded_bytes, Some(64 * 64 * 4));

    let capped = TextureImporterSettings {
        max_size: Some(8),
        ..TextureImporterSettings::default()
    };
    let kept = assess_at("assets/albedo.dds", &bytes, "linux", capped, &[]);
    assert_eq!(kept.width, Some(32));
    assert_eq!(kept.height, Some(32));
    assert_eq!(kept.decoded_bytes, Some(32 * 32 * 4));
    assert_ne!(kept.width, Some(8));
    assert_ne!(kept.width, Some(0));
    assert_eq!(kept.buffers_allocated, 0);
    assert_eq!(kept.decoded_bytes_reserved, 32 * 32 * 4);
    assert!(
        kept.findings
            .iter()
            .any(|finding| finding.rule_id == RULE_MISSING_MIP)
    );
    assert!(kept.findings.iter().all(|finding| {
        finding.rule_id != RULE_IMPOSSIBLE_DIMENSIONS
            && finding.rule_id != RULE_WRONG_COMPRESSION
            && finding.rule_id != RULE_USAGE_MISMATCH
    }));
    assert!(!kept.rewrites_source);
    assert_eq!(bytes, before);

    let root = project("texture-lint-dds-edge");
    let path = root.join("assets/albedo.dds");
    write_atomic(&path, &bytes);
    let first = scan_project(&root, "default");
    assert_eq!(
        first.report.metric("assets/albedo.dds", "texture_width").value,
        Some(64)
    );
    write_max_size(&path, "dds", 8);
    let second = scan_project(&root, "default");
    assert_eq!(
        second.report.metric("assets/albedo.dds", "texture_width").value,
        Some(32)
    );
    assert_eq!(
        second.report.metric("assets/albedo.dds", "texture_height").value,
        Some(32)
    );
    assert_eq!(
        second.report.metric("assets/albedo.dds", "decoded_bytes").value,
        Some(32 * 32 * 4)
    );
    assert_ne!(first.report.evidence_key, second.report.evidence_key);
    assert_eq!(std::fs::read(&path).expect("reread"), before);
}

#[test]
fn shared_usages_are_not_rewritten() {
    let bytes = ktx2(146, 8, 8, 4);
    let before = bytes.clone();
    let settings = TextureImporterSettings::default();
    let shared = assess_at(
        "assets/shared_color.ktx2",
        &bytes,
        "linux",
        settings.clone(),
        &[TextureCookUsage::ColorSrgb, TextureCookUsage::Ui],
    );
    assert!(
        shared
            .findings
            .iter()
            .all(|finding| finding.rule_id != RULE_USAGE_MISMATCH)
    );
    assert!(!shared.rewrites_source);
    assert_eq!(
        shared.reimport_preview.as_deref(),
        Some(settings.fingerprint().as_str())
    );
    assert_eq!(bytes, before);

    let mixed = assess_at(
        "assets/shared_color.ktx2",
        &bytes,
        "linux",
        settings,
        &[TextureCookUsage::ColorSrgb, TextureCookUsage::NormalMap],
    );
    assert_eq!(rule_ids(&mixed), vec![RULE_USAGE_MISMATCH]);
    assert!(!mixed.rewrites_source);
    assert_eq!(bytes, before);

    let root = project("texture-lint-share");
    let png_bytes = png(8, 8);
    let png_path = root.join("assets/shared_color.png");
    write_atomic(&png_path, &png_bytes);
    let agree = gltf_two_roles("shared_color.png", "emissiveTexture");
    write_atomic(root.join("assets/share.gltf").as_path(), agree.as_bytes());
    let agree_findings =
        texture_lint::shared_usage_findings(&root, "assets/share.gltf", agree.as_bytes(), "default")
            .expect("share");
    assert!(agree_findings.is_empty());
    let produced = produce_file(Some(&root), "assets/share.gltf", agree.as_bytes(), "default");
    assert!(
        produced
            .findings
            .iter()
            .all(|finding| finding.rule_id != RULE_USAGE_MISMATCH)
    );
    assert_eq!(std::fs::read(&png_path).expect("png"), png_bytes);

    let disagree = gltf_two_roles("shared_color.png", "normalTexture");
    let disagree_findings =
        texture_lint::shared_usage_findings(&root, "assets/share.gltf", disagree.as_bytes(), "default")
            .expect("roles");
    assert_eq!(disagree_findings.len(), 1);
    assert_eq!(disagree_findings[0].rule_id, RULE_USAGE_MISMATCH);
    assert_eq!(std::fs::read(&png_path).expect("png"), png_bytes);

    let missing = gltf_two_roles("missing.png", "normalTexture");
    let missing_findings =
        texture_lint::shared_usage_findings(&root, "assets/missing.gltf", missing.as_bytes(), "default")
            .expect("missing uri");
    assert!(missing_findings.is_empty());
    let offline = produce_file(None, "assets/missing.gltf", missing.as_bytes(), "default");
    assert!(
        offline
            .findings
            .iter()
            .all(|finding| finding.rule_id != RULE_USAGE_MISMATCH)
    );
}

#[test]
fn unknown_texture_is_not_stored_as_zero() {
    let garbage = b"not an image";
    let outcome = assess_at(
        "assets/broken.png",
        garbage,
        "linux",
        TextureImporterSettings::default(),
        &[],
    );
    assert!(outcome.incomplete);
    assert_eq!(outcome.width, None);
    assert_eq!(outcome.height, None);
    assert_eq!(outcome.decoded_bytes, None);
    assert_eq!(outcome.decoded_bytes_reserved, 0);
    assert_eq!(outcome.buffers_allocated, 0);
    assert_eq!(rule_ids(&outcome), vec![RULE_BUDGET_UNKNOWN]);

    let root = project("texture-lint-unknown");
    write_atomic(&root.join("assets/broken.png"), garbage);
    let report = produce_file(Some(&root), "assets/broken.png", garbage, "default");
    assert!(!report.complete);
    for name in ["texture_width", "texture_height", "decoded_bytes"] {
        let reading = report.metric("assets/broken.png", name);
        assert!(!reading.known, "{name}");
        assert_eq!(reading.value, None, "{name}");
        assert_ne!(reading.value, Some(0), "{name}");
    }
    let scanned = scan_project(&root, "default");
    assert!(!scanned.report.complete);
    assert_eq!(
        scanned.report.metric("assets/broken.png", "texture_width").value,
        None
    );

    let huge = png(20_000, 20_000);
    let over = assess_at(
        "assets/huge.png",
        &huge,
        "linux",
        TextureImporterSettings::default(),
        &[],
    );
    assert_eq!(rule_ids(&over), vec![RULE_IMPOSSIBLE_DIMENSIONS]);
    assert_eq!(over.width, Some(20_000));
    assert_eq!(over.decoded_bytes, Some(1_600_000_000));
    assert_eq!(over.decoded_bytes_reserved, 0);
    assert_eq!(over.buffers_allocated, 0);
    assert!(!over.incomplete);
}

#[test]
fn cli_cook_and_agent_produce_agree() {
    let root = project("texture-lint-agree");
    write_atomic(
        root.join("config/source_lint.ron").as_path(),
        b"(format_version: 1, profiles: { \"android\": (), },)\n",
    );
    let bytes = ktx2(145, 8, 8, 4);
    let rel = "assets/pack_orm.ktx2";
    write_atomic(&root.join(rel), &bytes);

    let direct = assess_at(rel, &bytes, "android", TextureImporterSettings::default(), &[]);
    let agent = produce_file(Some(&root), rel, &bytes, "android");
    let cli = scan_project(&root, "android");
    assert_eq!(rule_ids(&direct), vec![RULE_WRONG_COMPRESSION]);
    assert_eq!(report_rules(&agent), vec![RULE_WRONG_COMPRESSION.to_owned()]);
    assert_eq!(report_rules(&cli.report), vec![RULE_WRONG_COMPRESSION.to_owned()]);
    assert_eq!(
        agent.metric(rel, "texture_width").value,
        Some(u64::from(direct.width.expect("width")))
    );
    assert_eq!(
        cli.report.metric(rel, "texture_height").value,
        agent.metric(rel, "texture_height").value
    );
    assert_eq!(
        cli.report.metric(rel, "decoded_bytes").value,
        direct.decoded_bytes
    );
    assert_eq!(
        cli.report.findings[0].effect.as_deref(),
        Some("preserves_appearance")
    );
    assert_eq!(agent.findings[0].effect.as_deref(), Some("preserves_appearance"));

    let warn = source_lint_cache::diagnose_before_publish(&PublishRequest {
        project: root.clone(),
        target: "android".to_owned(),
        package: false,
    })
    .expect("warn gate");
    assert_eq!(warn.status, "warnings");
    assert!(!warn.blocking);

    write_atomic(root.join("config/validation.ron").as_path(), b"(cook: Fail)\n");
    let mut published = 0_u32;
    let blocked = source_lint_cache::run_publish_gate(
        &PublishRequest {
            project: root.clone(),
            target: "android".to_owned(),
            package: false,
        },
        || {
            published += 1;
            Ok::<(), String>(())
        },
    );
    assert!(blocked.is_err());
    assert_eq!(published, 0);
    let failed = source_lint_cache::diagnose_before_publish(&PublishRequest {
        project: root,
        target: "android".to_owned(),
        package: false,
    })
    .expect("fail gate");
    assert_eq!(failed.status, "failed");
    assert!(failed.blocking);
}

#[test]
fn settings_change_invalidates_the_evidence_cache() {
    let root = project("texture-lint-cache");
    let path = root.join("assets/icon.png");
    let bytes = png(8, 8);
    write_atomic(&path, &bytes);
    let first = scan_project(&root, "default");
    assert_eq!(first.counters.cache_hits, 0);
    assert!(first.report.complete);
    assert!(first.report.findings.is_empty());
    let second = scan_project(&root, "default");
    assert_eq!(second.report.evidence_key, first.report.evidence_key);
    assert!(second.counters.cache_hits > 0);
    write_max_size(&path, "png", 4);
    let third = scan_project(&root, "default");
    assert_ne!(third.report.evidence_key, first.report.evidence_key);
    assert_eq!(third.counters.cache_hits, 0);
    assert_eq!(
        third.report.metric("assets/icon.png", "texture_width").value,
        Some(8)
    );
    assert_eq!(std::fs::read(&path).expect("png"), bytes);
}

#[test]
fn known_size_is_applied_and_unknown_is_not_zero() {
    let mut asset = AssetEntry::new("assets/icon.png", AssetKind::Texture);
    apply_texture_size_evidence(&mut asset, None, None);
    assert_eq!(asset.texture_size, None);
    apply_texture_size_evidence(&mut asset, Some(0), Some(8));
    assert_eq!(asset.texture_size, None);
    apply_texture_size_evidence(&mut asset, Some(40_000), Some(8));
    assert_eq!(asset.texture_size, None);
    apply_texture_size_evidence(&mut asset, Some(32), Some(16));
    assert_eq!(asset.texture_size, Some([32, 16]));
    apply_texture_size_evidence(&mut asset, None, Some(4));
    assert_eq!(asset.texture_size, Some([32, 16]));
}

fn gltf_two_roles(uri: &str, second_slot: &str) -> String {
    format!(
        r#"{{"asset":{{"version":"2.0"}},"images":[{{"uri":"{uri}"}}],"textures":[{{"source":0}}],"materials":[{{"pbrMetallicRoughness":{{"baseColorTexture":{{"index":0}}}},"{second_slot}":{{"index":0}}}}]}}"#
    )
}
