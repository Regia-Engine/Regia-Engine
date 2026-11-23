// SPDX-License-Identifier: MPL-2.0
//! Dev-run versus package-run access traces.
//!
//! Requirements: the parity classifier (mark `requirements`, commit c9513623f).
//! Rewritten in Rust. Cook-index preimage strings stay private data so the
//! acceptance hashes match; they are not API names. Skip prefixes that name
//! another tree are dropped (`target/` already covers build output).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

/// One comparison of a dev access trace with a package access trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParityReport {
    /// `missing`, `drift`, `dieted` and `cooked` lines, sorted by path.
    pub lines: Vec<String>,
    /// Paths the dev run opened that the package run did not account for.
    pub missing: Vec<String>,
    /// Paths newer in the dev tree than the package stamp.
    pub drift: Vec<String>,
    /// Paths the package diet replaced on purpose.
    pub dieted: Vec<String>,
    /// Paths the package served from a cooked entry or a touched sibling.
    pub cooked: Vec<String>,
    /// Package files neither run touched. Reported, and not a gate failure.
    pub never_touched: Vec<String>,
    /// `1` when the gate fails, otherwise `0`.
    pub exit_code: i32,
    /// Markdown form of the same report.
    pub markdown: String,
}

impl ParityReport {
    /// Whether the parity gate fails.
    #[must_use]
    pub fn gate_fails(&self) -> bool {
        self.exit_code != 0
    }

    /// JSON object of the report, including `ok`.
    #[must_use]
    pub fn json_value(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = value.as_object_mut() {
            obj.insert("ok".to_string(), serde_json::json!(self.exit_code == 0));
        }
        value
    }
}

/// Compare two access traces. Drift is reported and does not fail the gate.
///
/// # Errors
///
/// Returns an error when either trace cannot be read.
pub fn diff_paths(
    dev_trace: &Path,
    dev_root: &Path,
    package_trace: &Path,
    package_root: &Path,
) -> std::io::Result<ParityReport> {
    diff_paths_mode(dev_trace, dev_root, package_trace, package_root, false)
}

/// Compare two access traces. `strict_drift` also fails the gate on drift.
///
/// # Errors
///
/// Returns an error when either trace cannot be read.
pub fn diff_paths_mode(
    dev_trace: &Path,
    dev_root: &Path,
    package_trace: &Path,
    package_root: &Path,
    strict_drift: bool,
) -> std::io::Result<ParityReport> {
    let dev_text = fs::read_to_string(dev_trace)?;
    let pkg_text = fs::read_to_string(package_trace)?;
    Ok(diff_loaded(
        &dev_text,
        dev_root,
        &pkg_text,
        package_root,
        strict_drift,
    ))
}

struct TraceSets {
    opened: BTreeSet<String>,
    seen: BTreeSet<String>,
}

fn diff_loaded(
    dev_text: &str,
    dev_root: &Path,
    pkg_text: &str,
    pkg_root: &Path,
    strict_drift: bool,
) -> ParityReport {
    let dev = read_trace(dev_text, &path_text(dev_root));
    let pkg = read_trace(pkg_text, &path_text(pkg_root));
    let stamp = staged_at(pkg_root);
    let dropped = gltf_image_refs(pkg_root);
    let cook = load_cook(pkg_root, &pkg.seen);
    let mut lines = Vec::new();
    let mut missing = Vec::new();
    let mut drift = Vec::new();
    let mut dieted = Vec::new();
    let mut cooked = Vec::new();
    for rel in dev.opened.difference(&pkg.seen) {
        let Some(kind) = classify_rel(rel, dev_root, pkg_root, &pkg.seen, stamp, &dropped, &cook) else {
            continue;
        };
        let line = format!("{kind} {rel}");
        lines.push(line);
        match kind {
            "missing" => missing.push(rel.clone()),
            "drift" => drift.push(rel.clone()),
            "dieted" => dieted.push(rel.clone()),
            "cooked" => cooked.push(rel.clone()),
            _ => {}
        }
    }
    let never_touched = never_touched(pkg_root, &dev.seen, &pkg.seen);
    let mut exit_code = i32::from(!missing.is_empty());
    if strict_drift && !drift.is_empty() {
        exit_code = 1;
    }
    let mut report = ParityReport {
        lines,
        missing,
        drift,
        dieted,
        cooked,
        never_touched,
        exit_code,
        markdown: String::new(),
    };
    report.markdown = render_markdown(&report);
    report
}

fn render_markdown(report: &ParityReport) -> String {
    let mut out = String::new();
    out.push_str("# package parity\n\n");
    out.push_str(&format!("- exit_code: {}\n", report.exit_code));
    out.push_str(&format!("- missing: {}\n", report.missing.len()));
    out.push_str(&format!("- drift: {}\n", report.drift.len()));
    out.push_str(&format!("- dieted: {}\n", report.dieted.len()));
    out.push_str(&format!("- cooked: {}\n", report.cooked.len()));
    out.push_str(&format!("- never_touched: {}\n", report.never_touched.len()));
    push_section(&mut out, "missing", &report.missing);
    push_section(&mut out, "drift", &report.drift);
    push_section(&mut out, "dieted", &report.dieted);
    push_section(&mut out, "cooked", &report.cooked);
    push_section(&mut out, "never_touched", &report.never_touched);
    out.push_str("\n## lines\n\n");
    for line in &report.lines {
        out.push_str(&format!("- `{line}`\n"));
    }
    out
}

fn push_section(out: &mut String, title: &str, rows: &[String]) {
    out.push_str(&format!("\n## {title}\n\n"));
    if rows.is_empty() {
        out.push_str("(none)\n");
        return;
    }
    for row in rows {
        out.push_str(&format!("- `{row}`\n"));
    }
}

fn never_touched(pkg_root: &Path, dev_seen: &BTreeSet<String>, pkg_seen: &BTreeSet<String>) -> Vec<String> {
    let mut files = Vec::new();
    walk_files(pkg_root, &mut files);
    let mut out = Vec::new();
    for path in files {
        let Ok(rel) = path.strip_prefix(pkg_root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() || dev_seen.contains(&rel) || pkg_seen.contains(&rel) {
            continue;
        }
        out.push(rel);
    }
    out.sort();
    out
}

fn walk_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut kids: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    kids.sort_by_key(|e| e.file_name());
    for entry in kids {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk_files(&path, out);
        } else if kind.is_file() || kind.is_symlink() {
            out.push(path);
        }
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().trim_end_matches(['/', '\\']).to_string()
}

fn read_trace(text: &str, root: &str) -> TraceSets {
    let prefix = format!("{root}/");
    let mut opened = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for line in text.lines() {
        let Some((status, op, name)) = parse_access_line(line) else {
            continue;
        };
        if status != "ok" || !name.starts_with(&prefix) {
            continue;
        }
        let rel = norm_rel(&name[prefix.len()..]);
        if op == "open" {
            opened.insert(rel.clone());
        }
        seen.insert(rel);
    }
    TraceSets { opened, seen }
}

fn parse_access_line(line: &str) -> Option<(&str, &str, String)> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut parts = line.splitn(3, ' ');
    let status = parts.next()?;
    let op = parts.next()?;
    let rest = parts.next()?.trim();
    if rest.is_empty() {
        return None;
    }
    let name = if let Some(path) = unescape_debug(rest) {
        path
    } else {
        rest.to_string()
    };
    Some((status, op, name))
}

fn unescape_debug(text: &str) -> Option<String> {
    let mut chars = text.chars();
    if chars.next() != Some('"') {
        return None;
    }
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        if ch == '"' {
            return Some(out);
        }
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '0' => out.push('\0'),
            '\'' => out.push('\''),
            'u' => {
                if chars.next() != Some('{') {
                    return None;
                }
                let mut hex = String::new();
                for h in chars.by_ref() {
                    if h == '}' {
                        break;
                    }
                    hex.push(h);
                }
                let cp = u32::from_str_radix(&hex, 16).ok()?;
                out.push(char::from_u32(cp)?);
            }
            other => out.push(other),
        }
    }
    None
}

fn norm_rel(path: &str) -> String {
    let slash = path.replace('\\', "/");
    norm_posix(&slash)
}

fn norm_posix(path: &str) -> String {
    let abs = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for part in path.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            out.pop();
            continue;
        }
        out.push(part);
    }
    let joined = out.join("/");
    if abs {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// Prefixes that are not package content. Another tree's install directories
/// are omitted; `target/` covers build output.
const SKIP_PREFIX: &[&str] = &[
    "config/upload/",
    "saved/",
    ".git/",
    "target/",
    "content/quarantine/_orch",
    "art/",
];

fn classify_rel(
    rel: &str,
    dev_root: &Path,
    pkg_root: &Path,
    have: &BTreeSet<String>,
    stamp: Option<f64>,
    dropped: &BTreeSet<String>,
    cook: &Cook,
) -> Option<&'static str> {
    if rel.is_empty() || rel == "." || SKIP_PREFIX.iter().any(|p| rel.starts_with(p)) {
        return None;
    }
    if dev_root.join(rel).is_dir() {
        return None;
    }
    if let Some(rest) = rel.strip_prefix("content/quarantine/audio/") {
        let moved = format!("assets/audio/{rest}");
        if have.contains(&moved) {
            return Some("dieted");
        }
    }
    let (stem, ext) = split_ext(rel);
    let ext_l = ext.to_ascii_lowercase();
    if ext_l == ".png" {
        let jpg = format!("{stem}.jpg");
        if have.contains(&jpg) {
            return Some("dieted");
        }
    }
    if is_dropped_lod(rel) {
        return Some("dieted");
    }
    if terrain_dieted(pkg_root, rel) {
        return Some("dieted");
    }
    if ext_l == ".bin" {
        let sibling = format!("{stem}.gltf");
        if have.contains(&sibling) && pkg_root.join(rel).is_file() {
            return Some("cooked");
        }
    }
    if let Some(stamp) = stamp
        && let Some(mtime) = mtime_secs(&dev_root.join(rel))
        && mtime > stamp
    {
        return Some("drift");
    }
    if image_dieted(rel, dropped) {
        return Some("dieted");
    }
    if served_from_cooked(rel, &ext_l, pkg_root, cook) {
        Some("cooked")
    } else {
        Some("missing")
    }
}

fn is_dropped_lod(rel: &str) -> bool {
    rel.ends_with("_LOD0.gltf")
        || rel.ends_with("_LOD1.gltf")
        || rel.ends_with("_LOD0.bin")
        || rel.ends_with("_LOD1.bin")
}

fn terrain_dieted(pkg_root: &Path, rel: &str) -> bool {
    let Some((prefix, map)) = terrain_map(rel) else {
        return false;
    };
    pkg_root
        .join(format!("{prefix}maps/{map}/cooked/terrain.tpak"))
        .is_file()
}

fn terrain_map(rel: &str) -> Option<(&'static str, &str)> {
    for prefix in ["assets_regia/", "assets/"] {
        let Some(rest) = rel.strip_prefix(prefix) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix("maps/") else {
            continue;
        };
        let Some((map, after)) = rest.split_once('/') else {
            continue;
        };
        let Some(tex) = after.strip_prefix("textures/") else {
            continue;
        };
        let hit = tex.starts_with("detail/")
            || tex.starts_with("macro/")
            || tex == "macro_albedo.png"
            || tex == "macro_mask.png";
        if hit {
            return Some((prefix, map));
        }
    }
    None
}

fn split_ext(rel: &str) -> (&str, &str) {
    let Some(dot) = rel.rfind('.') else {
        return (rel, "");
    };
    let name_start = rel.rfind('/').map_or(0, |s| s + 1);
    if dot <= name_start {
        return (rel, "");
    }
    (&rel[..dot], &rel[dot..])
}

fn mtime_secs(path: &Path) -> Option<f64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let dur = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(dur.as_secs_f64())
}

fn staged_at(pkg_root: &Path) -> Option<f64> {
    let text = fs::read_to_string(pkg_root.join("build-info.toml")).ok()?;
    for line in text.lines() {
        let (key, value) = line.split_once('=')?;
        if key.trim() == "staged_at" {
            let value = value.trim().trim_matches('"');
            return value.parse().ok();
        }
    }
    None
}

fn image_dieted(rel: &str, dropped: &BTreeSet<String>) -> bool {
    for prefix in ["assets_regia/", "assets/"] {
        if let Some(rest) = rel.strip_prefix(prefix)
            && dropped.contains(rest)
        {
            return true;
        }
    }
    false
}

fn gltf_image_refs(pkg_root: &Path) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    for name in ["assets_regia", "assets"] {
        let assets = pkg_root.join(name);
        if assets.is_dir() {
            collect_image_refs(&assets, &assets, &mut refs);
        }
    }
    refs
}

fn collect_image_refs(dir: &Path, assets: &Path, refs: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect_image_refs(&path, assets, refs);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".gltf") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(images) = doc.get("images").and_then(|v| v.as_array()) else {
            continue;
        };
        for image in images {
            let Some(uri) = image.get("uri").and_then(|v| v.as_str()) else {
                continue;
            };
            push_image_ref(refs, assets, &path, uri);
        }
    }
}

fn push_image_ref(refs: &mut BTreeSet<String>, assets: &Path, gltf: &Path, uri: &str) {
    if uri.starts_with("data:") {
        return;
    }
    let parent = gltf.parent().unwrap_or_else(|| Path::new(""));
    let joined = if Path::new(uri).is_absolute() {
        PathBuf::from(uri)
    } else {
        parent.join(uri)
    };
    let norm = PathBuf::from(norm_posix(&joined.to_string_lossy().replace('\\', "/")));
    let assets_norm = PathBuf::from(norm_posix(&assets.to_string_lossy().replace('\\', "/")));
    let Ok(rel) = norm.strip_prefix(&assets_norm) else {
        return;
    };
    let rel = norm_rel(&rel.to_string_lossy());
    if rel.is_empty() || rel == "." {
        return;
    }
    let lower = rel.to_ascii_lowercase();
    refs.insert(rel.clone());
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        let (stem, _) = split_ext(&rel);
        refs.insert(format!("{stem}.png"));
    }
}

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01B3;

/// Hash preimage for generated levels. The bytes are the acceptance contract
/// of the cook memo, not control flow.
const OVERRIDES_TEXT: &str = "\
# Per-asset overrides for Regia-generated LODs (src/world/gen_lod.rs).\n\
#   <path substring> skip            keep the LOD0 mesh at every level (a part the simplifier mangles)\n\
#   <path substring> r1,r2,r3        triangle ratios for LOD1, LOD2, LOD3 instead of 0.35,0.12,0.05\n\
# The substring is matched against the full path with '/' separators. An authored\n\
# _LOD<n> node or sibling file always wins over a generated level, with or without a line here.\n\
# One line per asset, with the reason; entries are added only from a judged frame.\n";

fn fnv64(data: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn lod_ratio_text(lod: u32) -> &'static str {
    match lod.min(3) {
        0 => "1.0000",
        1 => "0.3500",
        2 => "0.1200",
        _ => "0.0500",
    }
}

fn key_tag(lod: u32) -> String {
    if lod == 0 {
        return String::new();
    }
    let table = fnv64(OVERRIDES_TEXT.as_bytes());
    format!("genlod2:{}:96:48:{table:x}", lod_ratio_text(lod))
}

/// Cook memo key. `rel` starts at the asset-root component. `variant` is the
/// cook-index preimage the loader already resolved.
#[must_use]
pub fn gltf_memo_key(
    rel: &str,
    print_hex: &str,
    variant: &str,
    lod: u32,
    tex_edge: u32,
    colored: bool,
) -> String {
    let mut buf = Vec::new();
    for text in ["gltf", rel, print_hex, variant] {
        buf.extend(text.as_bytes());
        buf.push(0);
    }
    buf.extend(lod.to_le_bytes());
    buf.extend(tex_edge.to_le_bytes());
    buf.push(u8::from(colored));
    buf.extend(key_tag(lod).as_bytes());
    format!("{:016x}", fnv64(&buf))
}

struct Cook {
    index: BTreeMap<String, String>,
    prints: BTreeMap<String, String>,
    opened: BTreeSet<String>,
    mesh: bool,
    disk: BTreeSet<String>,
}

fn load_cook(pkg_root: &Path, have: &BTreeSet<String>) -> Cook {
    let mut index = BTreeMap::new();
    for line in read_lines(&pkg_root.join("cooked/bake_index.txt")) {
        let Some((key, value)) = line.split_once('\t') else {
            continue;
        };
        if !key.is_empty() {
            index.insert(key.to_string(), value.to_string());
        }
    }
    let mut prints = BTreeMap::new();
    for line in read_lines(&pkg_root.join("cooked/dir_prints.txt")) {
        let Some((key, value)) = line.split_once('\t') else {
            continue;
        };
        if !key.is_empty() && is_print_hex(value) {
            prints.insert(key.to_string(), value.to_string());
        }
    }
    let mut opened = BTreeSet::new();
    let mut mesh = false;
    for rel in have {
        let norm = rel.replace('\\', "/");
        if norm.starts_with("cooked/mesh/") && norm.ends_with(".bms") {
            mesh = true;
        } else if let Some(stem) = cooked_gltf_stem(&norm) {
            opened.insert(stem);
        }
    }
    let mut disk = BTreeSet::new();
    if mesh && let Ok(entries) = fs::read_dir(pkg_root.join("cooked/gltf")) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = cooked_name_stem(&name) {
                disk.insert(stem);
            }
        }
    }
    Cook {
        index,
        prints,
        opened,
        mesh,
        disk,
    }
}

fn is_print_hex(value: &str) -> bool {
    value.len() == 16 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn cooked_gltf_stem(norm: &str) -> Option<String> {
    let rest = norm.strip_prefix("cooked/gltf/")?;
    cooked_name_stem(rest)
}

fn cooked_name_stem(name: &str) -> Option<String> {
    if name.ends_with(".gltc") || name.ends_with(".bltc") {
        Some(
            name.rsplit_once('.')
                .map(|(s, _)| s.to_string())
                .unwrap_or_default(),
        )
    } else {
        None
    }
}

fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .map(|t| t.lines().map(ToString::to_string).collect())
        .unwrap_or_default()
}

fn parts_of(rel: &str) -> Vec<&str> {
    rel.split(['/', '\\'])
        .filter(|p| !p.is_empty() && *p != ".")
        .collect()
}

fn asset_at(parts: &[&str]) -> Option<usize> {
    parts
        .iter()
        .position(|p| *p == "assets_regia")
        .or_else(|| parts.iter().position(|p| *p == "assets"))
}

fn asset_dir_key(rel: &str) -> Option<String> {
    let parts = parts_of(rel);
    let at = asset_at(&parts)?;
    let parent = &parts[at + 1..parts.len().saturating_sub(1)];
    if parent.is_empty() {
        Some(".".to_string())
    } else {
        Some(parent.join("/"))
    }
}

fn memo_rel(rel: &str) -> String {
    let parts = parts_of(rel);
    match asset_at(&parts) {
        Some(at) => parts[at..].join("/"),
        None => parts.join("/"),
    }
}

fn gltf_of(rel: &str) -> String {
    if rel.to_ascii_lowercase().ends_with(".bin") {
        let (stem, _) = split_ext(rel);
        format!("{stem}.gltf")
    } else {
        rel.to_string()
    }
}

fn covered(rel: &str, cook: &Cook) -> bool {
    if cook.index.is_empty() {
        return false;
    }
    let Some(folder) = asset_dir_key(rel) else {
        return false;
    };
    cook.prints.contains_key(&folder)
}

/// Private cook-index preimages. Not a public name.
const PREIMAGE_A: &str = "remainder";
const PREIMAGE_B: &str = "area52";
const PREIMAGE_C: &str = "shared";

fn memo_combos(rel: &str) -> Vec<(u32, u32, &'static str, bool)> {
    let text = rel.replace('\\', "/");
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    let mut add = |lod: u32, edge: u32, variant: &'static str, colored: bool| {
        let key = format!("{lod}|{edge}|{variant}|{colored}");
        if seen.insert(key) {
            rows.push((lod, edge, variant, colored));
        }
    };
    if text.contains("/scatter/") {
        for lod in 0..4 {
            add(lod, 2048, "", false);
            add(lod, 256, "", false);
        }
    } else if text.contains("/setpieces/hero/") {
        for lod in 0..4 {
            add(lod, 2048, PREIMAGE_A, true);
        }
    } else if text.contains("/setpieces/") {
        for lod in 0..4 {
            add(lod, 512, PREIMAGE_A, false);
        }
    } else if text.contains("/pilots/") {
        for edge in [128, 512, 1024, 2048, 4096] {
            add(0, edge, PREIMAGE_A, false);
            add(0, edge, PREIMAGE_B, false);
        }
    } else {
        for (lod, edge) in [(0, 2048), (1, 512), (2, 256), (1, 1024)] {
            add(lod, edge, PREIMAGE_A, false);
            add(lod, edge, PREIMAGE_B, false);
        }
    }
    for edge in [64, 128, 256, 512, 1024, 2048, 4096, 8192] {
        for variant in [PREIMAGE_A, PREIMAGE_B, "", PREIMAGE_C] {
            add(0, edge, variant, false);
            if edge == 2048 || edge == 4096 {
                add(0, edge, variant, true);
            }
        }
    }
    rows
}

fn index_value(rel: &str, cook: &Cook) -> Option<String> {
    let target = gltf_of(rel);
    if !target.to_ascii_lowercase().ends_with(".gltf") {
        return None;
    }
    let folder = asset_dir_key(&target)?;
    let print_hex = cook.prints.get(&folder)?;
    if cook.index.is_empty() {
        return None;
    }
    let memo = memo_rel(&target);
    for (lod, edge, variant, colored) in memo_combos(&target) {
        let key = gltf_memo_key(&memo, print_hex, variant, lod, edge, colored);
        if let Some(value) = cook.index.get(&key)
            && !value.is_empty()
        {
            return Some(value.clone());
        }
    }
    None
}

fn entry_read(rel: &str, cook: &Cook) -> bool {
    let Some(value) = index_value(rel, cook) else {
        return false;
    };
    if cook.opened.contains(&value) {
        return true;
    }
    cook.mesh && cook.disk.contains(&value)
}

fn served_from_cooked(rel: &str, ext_l: &str, pkg_root: &Path, cook: &Cook) -> bool {
    if ext_l != ".gltf" && ext_l != ".bin" {
        return false;
    }
    if pkg_root.join(rel).is_file() && covered(rel, cook) {
        return true;
    }
    entry_read(rel, cook)
}
