// SPDX-License-Identifier: MPL-2.0
//! `regia apk-size` (ADR-527): what a built APK or AAB is made of.
//!
//! Reads the archive's zip central directory (no decompression: every size is in the directory,
//! APPNOTE.TXT § 4.3.12) and sums the entries by what they are: native libraries per ABI, cooked
//! paks, other raw assets, dex, compiled resources, the manifest and the signature. `regia
//! package --platform android` prints the same report on stderr after the build. `--json` is the
//! machine form agents read; `--budget-mb` turns the report into a check (exit 1 when the archive
//! is larger).
//!
//! It also flags packaging mistakes that only show up in the archive: a `.fpak` stored deflated
//! (the generated Gradle project marks paks `noCompress` so the runtime can map them) and native
//! libraries for an ABI the manifest did not ask for.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Serialize;

/// `regia apk-size` arguments.
#[derive(Debug, Args)]
pub struct ApkSizeArgs {
    /// The `.apk` or `.aab` to read.
    pub path: PathBuf,
    /// Print the report as JSON on stdout (agents and CI).
    #[arg(long, default_value_t = false)]
    pub json: bool,
    /// Fail (exit 1) when the archive is larger than this many MiB.
    #[arg(long, value_name = "MIB")]
    pub budget_mb: Option<f64>,
    /// How many of the largest entries to list (default 10).
    #[arg(long, default_value_t = 10)]
    pub top: usize,
}

/// One entry of the zip central directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZipEntryInfo {
    /// Path inside the archive.
    pub name: String,
    /// Bytes stored in the archive.
    pub compressed: u64,
    /// Bytes once extracted.
    pub uncompressed: u64,
    /// Zip compression method (0 = stored, 8 = deflate).
    pub method: u16,
}

/// What an archive entry is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApkCategory {
    /// `lib/<abi>/*.so`.
    NativeLib(String),
    /// `assets/**/*.fpak`: cooked paks.
    Paks,
    /// Any other file under `assets/`.
    RawAssets,
    /// `classes*.dex` (the Java shim and libraries).
    Dex,
    /// `res/**` and `resources.arsc` / `resources.pb`.
    Resources,
    /// `AndroidManifest.xml`.
    Manifest,
    /// `META-INF/**`: signature and build metadata.
    Signature,
    /// Anything else (AAB `BundleConfig.pb`, kotlin metadata, …).
    Other,
}

impl ApkCategory {
    /// Classifies an entry name. AAB module prefixes (`base/`, `<feature>/`) are ignored for the
    /// known top-level folders.
    #[must_use]
    pub fn of(name: &str) -> Self {
        let path = strip_module(name);
        if let Some(rest) = path.strip_prefix("lib/") {
            let abi = rest.split('/').next().unwrap_or_default();
            if !abi.is_empty() && rest.len() > abi.len() {
                return Self::NativeLib(abi.to_owned());
            }
        }
        if path.starts_with("assets/") {
            return if path.ends_with(".fpak") {
                Self::Paks
            } else {
                Self::RawAssets
            };
        }
        let file = path.rsplit('/').next().unwrap_or(path);
        if file.starts_with("classes") && file.ends_with(".dex") {
            return Self::Dex;
        }
        if path.starts_with("res/") || matches!(file, "resources.arsc" | "resources.pb") {
            return Self::Resources;
        }
        if file == "AndroidManifest.xml" {
            return Self::Manifest;
        }
        if path.starts_with("META-INF/") || name.starts_with("META-INF/") {
            return Self::Signature;
        }
        Self::Other
    }

    /// Display label.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::NativeLib(abi) => format!("native lib ({abi})"),
            Self::Paks => "paks".into(),
            Self::RawAssets => "raw assets".into(),
            Self::Dex => "dex".into(),
            Self::Resources => "resources".into(),
            Self::Manifest => "manifest".into(),
            Self::Signature => "signature".into(),
            Self::Other => "other".into(),
        }
    }
}

/// `base/lib/…` → `lib/…` (AAB modules); APK names pass through.
fn strip_module(name: &str) -> &str {
    const TOP: [&str; 5] = ["lib/", "assets/", "res/", "dex/", "manifest/"];
    // Already a top-level folder (an APK, or `assets/res/…` inside one): not a module prefix.
    if TOP.iter().any(|t| name.starts_with(t)) {
        return name;
    }
    match name.split_once('/') {
        Some((_, rest)) if TOP.iter().any(|t| rest.starts_with(t)) || rest == "resources.pb" => rest,
        _ => name,
    }
}

/// Totals for one [`ApkCategory`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CategoryTotal {
    /// The category.
    pub category: ApkCategory,
    /// Display label.
    pub label: String,
    /// Entries in it.
    pub entries: usize,
    /// Bytes stored.
    pub compressed: u64,
    /// Bytes extracted.
    pub uncompressed: u64,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApkSizeReport {
    /// The archive read.
    pub path: String,
    /// Archive size on disk.
    pub file_bytes: u64,
    /// Entries in the central directory.
    pub entries: usize,
    /// Sum of stored sizes.
    pub compressed: u64,
    /// Sum of extracted sizes.
    pub uncompressed: u64,
    /// Totals per category, largest stored size first.
    pub categories: Vec<CategoryTotal>,
    /// ABIs with native libraries.
    pub abis: Vec<String>,
    /// The largest entries by stored size.
    pub largest: Vec<ZipEntryInfo>,
    /// Packaging problems found in the archive (stable `code: text` strings).
    pub warnings: Vec<String>,
    /// The budget it was checked against, in MiB.
    pub budget_mb: Option<f64>,
    /// `file_bytes` is within the budget (`true` without one).
    pub within_budget: bool,
}

const EOCD_SIGNATURE: u32 = 0x0605_4b50;
const ZIP64_EOCD_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
const ZIP64_EOCD_SIGNATURE: u32 = 0x0606_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
/// End-of-central-directory record without its comment.
const EOCD_LEN: usize = 22;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    let bytes: [u8; 2] = b.get(at..at.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(bytes))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = b.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    let bytes: [u8; 8] = b.get(at..at.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

/// Reads every central-directory entry of a zip archive (APK and AAB are zips). Zip64 archives
/// and zip64 entry sizes are supported; multi-disk archives are not (no Android tool writes them).
pub fn read_zip_entries(bytes: &[u8]) -> Result<Vec<ZipEntryInfo>> {
    if bytes.len() < EOCD_LEN {
        bail!("not a zip archive: {} bytes", bytes.len());
    }
    // The EOCD sits at the end, after at most a 65535-byte comment. Its comment must reach the
    // end exactly, as Android's own zip readers require: that also tells the real record from a
    // signature inside the comment (a `<=` test accepts the later, fake one).
    let lowest = bytes.len().saturating_sub(EOCD_LEN + usize::from(u16::MAX));
    let eocd = (lowest..=bytes.len() - EOCD_LEN)
        .rev()
        .find(|&at| {
            u32_at(bytes, at) == Some(EOCD_SIGNATURE)
                && u16_at(bytes, at + 20)
                    .is_some_and(|comment_len| usize::from(comment_len) == bytes.len() - at - EOCD_LEN)
        })
        .context("not a zip archive: no end-of-central-directory record")?;
    let field = |at: usize| u32_at(bytes, eocd + at).context("truncated end-of-central-directory record");
    let mut count = u64::from(u16_at(bytes, eocd + 10).context("truncated end-of-central-directory record")?);
    let mut offset = u64::from(field(16)?);
    if count == u64::from(u16::MAX) || offset == u64::from(u32::MAX) {
        // Zip64: the locator sits right before the EOCD and points at the zip64 record.
        let locator = eocd
            .checked_sub(20)
            .context("zip64 archive without its locator")?;
        if u32_at(bytes, locator) != Some(ZIP64_EOCD_LOCATOR_SIGNATURE) {
            bail!("zip64 archive without its locator");
        }
        let record = usize::try_from(u64_at(bytes, locator + 8).context("truncated zip64 locator")?)?;
        if u32_at(bytes, record) != Some(ZIP64_EOCD_SIGNATURE) {
            bail!("zip64 end-of-central-directory record not found");
        }
        count = u64_at(bytes, record + 32).context("truncated zip64 record")?;
        offset = u64_at(bytes, record + 48).context("truncated zip64 record")?;
    }
    let mut at = usize::try_from(offset)?;
    let mut entries = Vec::new();
    for index in 0..count {
        let bad = || format!("central directory entry {index} is truncated or corrupt");
        if u32_at(bytes, at) != Some(CENTRAL_SIGNATURE) {
            bail!("{}", bad());
        }
        let method = u16_at(bytes, at + 10).with_context(bad)?;
        let mut compressed = u64::from(u32_at(bytes, at + 20).with_context(bad)?);
        let mut uncompressed = u64::from(u32_at(bytes, at + 24).with_context(bad)?);
        let name_len = usize::from(u16_at(bytes, at + 28).with_context(bad)?);
        let extra_len = usize::from(u16_at(bytes, at + 30).with_context(bad)?);
        let comment_len = usize::from(u16_at(bytes, at + 32).with_context(bad)?);
        let name_start = at + 46;
        let name = bytes.get(name_start..name_start + name_len).with_context(bad)?;
        let extra = bytes
            .get(name_start + name_len..name_start + name_len + extra_len)
            .with_context(bad)?;
        // Zip64 extended information (header 0x0001): the 64-bit sizes, in order, for each
        // 32-bit field that is saturated.
        if uncompressed == u64::from(u32::MAX) || compressed == u64::from(u32::MAX) {
            let mut x = 0;
            while x + 4 <= extra.len() {
                let id = u16_at(extra, x).unwrap_or_default();
                let len = usize::from(u16_at(extra, x + 2).unwrap_or_default());
                if id == 1 {
                    let mut field_at = x + 4;
                    if uncompressed == u64::from(u32::MAX) {
                        uncompressed = u64_at(extra, field_at).with_context(bad)?;
                        field_at += 8;
                    }
                    if compressed == u64::from(u32::MAX) {
                        compressed = u64_at(extra, field_at).with_context(bad)?;
                    }
                    break;
                }
                x += 4 + len;
            }
        }
        entries.push(ZipEntryInfo {
            name: String::from_utf8_lossy(name).into_owned(),
            compressed,
            uncompressed,
            method,
        });
        at = name_start + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

/// Builds the report from the entries. `expected_abis` (from the manifest) adds a warning for any
/// other ABI found; empty = no check.
#[must_use]
pub fn report(
    path: &Path,
    file_bytes: u64,
    entries: &[ZipEntryInfo],
    expected_abis: &[String],
    top: usize,
    budget_mb: Option<f64>,
) -> ApkSizeReport {
    let mut categories: Vec<CategoryTotal> = Vec::new();
    let mut warnings = Vec::new();
    for entry in entries {
        if entry.name.ends_with('/') {
            continue;
        }
        let category = ApkCategory::of(&entry.name);
        if category == ApkCategory::Paks && entry.method != 0 {
            warnings.push(format!(
                "pak_compressed: {} is stored deflated; paks must be stored (noCompress \"fpak\")",
                entry.name
            ));
        }
        match categories.iter_mut().find(|c| c.category == category) {
            Some(total) => {
                total.entries += 1;
                total.compressed += entry.compressed;
                total.uncompressed += entry.uncompressed;
            }
            None => categories.push(CategoryTotal {
                label: category.label(),
                category,
                entries: 1,
                compressed: entry.compressed,
                uncompressed: entry.uncompressed,
            }),
        }
    }
    categories.sort_by(|a, b| b.compressed.cmp(&a.compressed).then(a.category.cmp(&b.category)));
    let mut abis: Vec<String> = categories
        .iter()
        .filter_map(|c| match &c.category {
            ApkCategory::NativeLib(abi) => Some(abi.clone()),
            _ => None,
        })
        .collect();
    abis.sort();
    if !expected_abis.is_empty() {
        for abi in abis.iter().filter(|a| !expected_abis.contains(a)) {
            warnings.push(format!(
                "unexpected_abi: native libraries for {abi}, which the manifest does not list"
            ));
        }
    }
    let mut largest: Vec<ZipEntryInfo> = entries
        .iter()
        .filter(|e| !e.name.ends_with('/'))
        .cloned()
        .collect();
    largest.sort_by(|a, b| b.compressed.cmp(&a.compressed).then_with(|| a.name.cmp(&b.name)));
    largest.truncate(top);
    let budget_bytes = budget_mb.map(|mb| mb * 1024.0 * 1024.0);
    ApkSizeReport {
        path: path.display().to_string(),
        file_bytes,
        entries: entries.len(),
        compressed: entries.iter().map(|e| e.compressed).sum(),
        uncompressed: entries.iter().map(|e| e.uncompressed).sum(),
        categories,
        abis,
        largest,
        warnings,
        budget_mb,
        within_budget: budget_bytes.is_none_or(|b| file_bytes as f64 <= b),
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// The human-readable report.
#[must_use]
pub fn render_text(r: &ApkSizeReport) -> String {
    let mut out = format!(
        "{}: {:.2} MiB on disk, {} entries ({:.2} MiB stored, {:.2} MiB extracted)\n",
        r.path,
        mib(r.file_bytes),
        r.entries,
        mib(r.compressed),
        mib(r.uncompressed)
    );
    for c in &r.categories {
        let share = if r.compressed == 0 {
            0.0
        } else {
            c.compressed as f64 * 100.0 / r.compressed as f64
        };
        out.push_str(&format!(
            "  {:<22} {:>9.2} MiB stored {:>9.2} MiB extracted {:>5.1}% {:>6} entries\n",
            c.label,
            mib(c.compressed),
            mib(c.uncompressed),
            share,
            c.entries
        ));
    }
    if !r.largest.is_empty() {
        out.push_str("  largest:\n");
        for e in &r.largest {
            out.push_str(&format!(
                "    {:>9.2} MiB {}{}\n",
                mib(e.compressed),
                e.name,
                if e.method == 0 { " (stored)" } else { "" }
            ));
        }
    }
    for w in &r.warnings {
        out.push_str(&format!("  warning: {w}\n"));
    }
    if let Some(budget) = r.budget_mb {
        out.push_str(&format!(
            "  budget {budget:.2} MiB: {}\n",
            if r.within_budget { "ok" } else { "OVER" }
        ));
    }
    out
}

/// Reads `path` and builds its report.
pub fn report_for_file(
    path: &Path,
    expected_abis: &[String],
    top: usize,
    budget_mb: Option<f64>,
) -> Result<ApkSizeReport> {
    let _span = tracing::info_span!("apk_size_report").entered();
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let entries = read_zip_entries(&bytes).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(report(
        path,
        bytes.len() as u64,
        &entries,
        expected_abis,
        top,
        budget_mb,
    ))
}

/// Runs `regia apk-size`.
pub fn run(args: &ApkSizeArgs) -> Result<()> {
    if let Some(mb) = args.budget_mb
        && !(mb.is_finite() && mb > 0.0)
    {
        bail!("--budget-mb must be a positive number");
    }
    let r = report_for_file(&args.path, &[], args.top, args.budget_mb)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        print!("{}", render_text(&r));
    }
    if !r.within_budget {
        bail!(
            "{} is {:.2} MiB, over the {:.2} MiB budget",
            args.path.display(),
            mib(r.file_bytes),
            args.budget_mb.unwrap_or_default()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal zip with the given (name, stored size, extracted size, method) entries. Only the
    /// central directory and EOCD are meaningful; local headers are skipped by the reader.
    fn zip(entries: &[(&str, u32, u32, u16)], comment: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; 16]; // stand-in for the local headers and data
        let cd_start = out.len() as u32;
        for (name, compressed, uncompressed, method) in entries {
            out.extend_from_slice(&CENTRAL_SIGNATURE.to_le_bytes());
            out.extend_from_slice(&[20, 0, 20, 0, 0, 0]); // made by, needed, flags
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0; 8]); // time, date, crc
            out.extend_from_slice(&compressed.to_le_bytes());
            out.extend_from_slice(&uncompressed.to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&[0; 4]); // extra, comment lengths
            out.extend_from_slice(&[0; 8]); // disk, internal, external attributes
            out.extend_from_slice(&0u32.to_le_bytes()); // local header offset
            out.extend_from_slice(name.as_bytes());
        }
        let cd_len = out.len() as u32 - cd_start;
        out.extend_from_slice(&EOCD_SIGNATURE.to_le_bytes());
        out.extend_from_slice(&[0; 4]); // disk numbers
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_len.to_le_bytes());
        out.extend_from_slice(&cd_start.to_le_bytes());
        out.extend_from_slice(&(comment.len() as u16).to_le_bytes());
        out.extend_from_slice(comment);
        out
    }

    fn apk() -> Vec<u8> {
        zip(
            &[
                ("AndroidManifest.xml", 2_000, 6_000, 8),
                ("classes.dex", 30_000, 70_000, 8),
                ("lib/arm64-v8a/libgame.so", 9_000_000, 20_000_000, 8),
                ("lib/x86_64/libgame.so", 9_500_000, 21_000_000, 8),
                ("assets/paks/base.fpak", 40_000_000, 40_000_000, 0),
                ("assets/regia.toml", 300, 900, 8),
                ("res/mipmap-hdpi/ic_launcher.png", 4_000, 4_000, 0),
                ("resources.arsc", 1_000, 1_000, 0),
                ("META-INF/CERT.RSA", 1_200, 1_500, 8),
                ("assets/", 0, 0, 0),
            ],
            b"signed by apksigner",
        )
    }

    #[test]
    fn audit_eocd_signature_inside_a_comment_is_not_a_record() {
        let mut comment = EOCD_SIGNATURE.to_le_bytes().to_vec();
        comment.extend_from_slice(&[0; 18]);
        comment.extend_from_slice(b"tail");
        let bytes = zip(&[("assets/user.fpak", 11, 11, 0)], &comment);
        let entries = read_zip_entries(&bytes).expect("valid ZIP with a binary comment");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "assets/user.fpak");
    }

    /// w10-corr-low-landing: Android's zip readers require the EOCD comment to end at the end
    /// of the file, so an APK with bytes after it is not one Android installs: refuse it rather
    /// than size it.
    #[test]
    fn a_zip_with_bytes_after_the_eocd_comment_is_refused() {
        let mut bytes = zip(&[("assets/user.fpak", 11, 11, 0)], b"comment");
        bytes.extend_from_slice(&[0xAA; 7]);
        let err = read_zip_entries(&bytes).expect_err("trailing bytes are not a zip Android reads");
        assert!(err.to_string().contains("end-of-central-directory"), "{err:#}");
    }

    /// w10-corr-low-landing: a comment holding an EOCD signature (with a zero comment length)
    /// must not be taken for the record: the backward scan meets it first, and only the
    /// exact-length test skips it (the corr-low `<=` change read zero entries here).
    #[test]
    fn an_eocd_signature_inside_the_comment_is_not_the_record() {
        let mut comment = EOCD_SIGNATURE.to_le_bytes().to_vec();
        comment.extend_from_slice(&[0; EOCD_LEN - 4]);
        comment.extend_from_slice(b"xyz");
        let bytes = zip(&[("assets/user.fpak", 11, 11, 0)], &comment);
        let entries = read_zip_entries(&bytes).expect("zip");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "assets/user.fpak");
    }

    #[test]
    fn reads_the_central_directory_past_a_comment() {
        let entries = read_zip_entries(&apk()).expect("zip");
        assert_eq!(entries.len(), 10);
        assert_eq!(entries[2].name, "lib/arm64-v8a/libgame.so");
        assert_eq!(
            (entries[2].compressed, entries[2].uncompressed, entries[2].method),
            (9_000_000, 20_000_000, 8)
        );
    }

    #[test]
    fn rejects_what_is_not_a_zip() {
        assert!(read_zip_entries(b"short").is_err());
        assert!(read_zip_entries(&[0u8; 4096]).is_err());
        let mut truncated = apk();
        // Point the central directory past the end.
        let len = truncated.len();
        let eocd = len - 19 - EOCD_LEN;
        truncated[eocd + 16..eocd + 20].copy_from_slice(&u32::MAX.wrapping_sub(1).to_le_bytes());
        assert!(read_zip_entries(&truncated).is_err());
    }

    #[test]
    fn a_zip64_offset_near_the_address_limit_is_an_error_not_a_panic() {
        // The audit's 42-byte file: a zip64 locator whose record offset is `usize::MAX - 2`, then
        // an EOCD whose entry count sends the reader down the zip64 branch (ADR-1660).
        let eocd = |out: &mut Vec<u8>| {
            out.extend_from_slice(&EOCD_SIGNATURE.to_le_bytes());
            out.extend_from_slice(&[0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
            out.extend_from_slice(&[0; 8]);
            out.extend_from_slice(&[0, 0]);
        };
        let locator = |out: &mut Vec<u8>, record: u64| {
            out.extend_from_slice(&ZIP64_EOCD_LOCATOR_SIGNATURE.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&record.to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes());
        };
        let mut short = Vec::new();
        locator(&mut short, u64::MAX - 2);
        eocd(&mut short);
        assert_eq!(short.len(), 42);
        assert!(read_zip_entries(&short).is_err());

        // A valid zip64 record whose central-directory offset is `u64::MAX - 2`.
        let mut record = Vec::new();
        record.extend_from_slice(&ZIP64_EOCD_SIGNATURE.to_le_bytes());
        record.resize(32, 0);
        record.extend_from_slice(&1u64.to_le_bytes());
        record.resize(48, 0);
        record.extend_from_slice(&(u64::MAX - 2).to_le_bytes());
        locator(&mut record, 0);
        eocd(&mut record);
        assert!(read_zip_entries(&record).is_err());
    }

    #[test]
    fn categories_cover_apk_and_aab_layouts() {
        use ApkCategory::*;
        assert_eq!(
            ApkCategory::of("lib/arm64-v8a/libgame.so"),
            NativeLib("arm64-v8a".into())
        );
        assert_eq!(
            ApkCategory::of("base/lib/arm64-v8a/libgame.so"),
            NativeLib("arm64-v8a".into())
        );
        assert_eq!(ApkCategory::of("assets/paks/base.fpak"), Paks);
        assert_eq!(ApkCategory::of("base/assets/paks/base.fpak"), Paks);
        assert_eq!(ApkCategory::of("assets/regia.toml"), RawAssets);
        assert_eq!(
            ApkCategory::of("assets/res/icon.png"),
            RawAssets,
            "not an AAB module prefix"
        );
        assert_eq!(ApkCategory::of("classes2.dex"), Dex);
        assert_eq!(ApkCategory::of("base/dex/classes.dex"), Dex);
        assert_eq!(ApkCategory::of("res/layout/a.xml"), Resources);
        assert_eq!(ApkCategory::of("base/resources.pb"), Resources);
        assert_eq!(ApkCategory::of("AndroidManifest.xml"), Manifest);
        assert_eq!(ApkCategory::of("base/manifest/AndroidManifest.xml"), Manifest);
        assert_eq!(ApkCategory::of("META-INF/CERT.RSA"), Signature);
        assert_eq!(ApkCategory::of("BundleConfig.pb"), Other);
        assert_eq!(ApkCategory::of("lib/"), Other);
    }

    #[test]
    fn report_sums_by_category_and_flags_problems() {
        let entries = read_zip_entries(&apk()).expect("zip");
        let r = report(
            Path::new("game.apk"),
            60_000_000,
            &entries,
            &["arm64-v8a".to_owned()],
            3,
            Some(50.0),
        );
        assert_eq!(r.categories[0].category, ApkCategory::Paks, "largest first");
        assert_eq!(r.categories[0].compressed, 40_000_000);
        let raw = r
            .categories
            .iter()
            .find(|c| c.category == ApkCategory::RawAssets)
            .expect("raw");
        assert_eq!(raw.entries, 1, "directory entries are skipped");
        assert_eq!(r.abis, ["arm64-v8a", "x86_64"]);
        assert_eq!(r.warnings.len(), 1);
        assert!(
            r.warnings[0].starts_with("unexpected_abi: native libraries for x86_64"),
            "{:?}",
            r.warnings
        );
        assert_eq!(r.largest.len(), 3);
        assert_eq!(r.largest[0].name, "assets/paks/base.fpak");
        assert!(!r.within_budget, "60 MB > 50 MiB");
        let text = render_text(&r);
        assert!(
            text.contains("native lib (arm64-v8a)") && text.contains("budget 50.00 MiB: OVER"),
            "{text}"
        );
        let json = serde_json::to_value(&r).expect("json");
        assert_eq!(json["categories"][0]["category"], "paks");
        assert_eq!(json["categories"][1]["category"]["native_lib"], "x86_64");
    }

    #[test]
    fn a_deflated_pak_is_flagged() {
        let entries = read_zip_entries(&zip(&[("assets/a.fpak", 10, 20, 8)], b"")).expect("zip");
        let r = report(Path::new("a.apk"), 100, &entries, &[], 10, None);
        assert!(r.warnings[0].starts_with("pak_compressed: assets/a.fpak"));
        assert!(r.within_budget, "no budget");
    }
}
