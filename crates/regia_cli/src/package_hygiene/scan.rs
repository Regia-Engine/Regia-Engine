// SPDX-License-Identifier: MPL-2.0
//! Bounded, deterministic traversal and decoded-payload inspection.
//!
//! Literals and bounded patterns slide a window whose overlap is one byte shorter than the
//! widest pattern, which is enough to contain any match of that width (the standard exact-match
//! window; see the ADR-8244 provenance row). Unbounded patterns run on the whole input when it
//! fits in the memory budget and otherwise fail closed. A mapped pak is not charged; an owned
//! pak and each decoded entry are, and they are not retained together past the budget.

use std::cell::Cell;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::time::SystemTime;

use regia_asset::AssetId;
use regia_asset::pak::{PakError, PakReader};

use super::policy::Compiled;
use super::{AuditError, Finding, Policy, Report};

pub(super) mod artifact;

struct Resident {
    current: u64,
    peak: u64,
}

impl Resident {
    fn charge(&mut self, bytes: u64, cap: u64) -> bool {
        let next = self.current.saturating_add(bytes);
        if next < self.current || next > cap {
            return false;
        }
        self.current = next;
        if self.current > self.peak {
            self.peak = self.current;
        }
        true
    }

    fn release(&mut self, bytes: u64) {
        self.current = self.current.saturating_sub(bytes);
    }
}

struct Snap {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Copy)]
enum Slot {
    Raw(usize),
    Term(usize),
    Email,
    Key,
}

enum Plan {
    Whole,
    Stream { window: usize, overlap: usize },
    Refuse(&'static str),
}

struct OpenedPak {
    pak: PakReader,
    charged: u64,
}

struct View<'a> {
    bytes: &'a [u8],
    origin: u64,
    bos: bool,
    eos: bool,
    names: bool,
}

/// Public display data only. Exact paths stay in traversal and allowance arguments.
#[derive(Clone)]
struct Location {
    display: String,
    id: String,
}

impl Location {
    fn asset(&self, id: AssetId) -> Self {
        Self {
            display: format!("{}::{id}", self.display),
            id: format!("{}::{id}", self.id),
        }
    }
}

struct Scan<'a> {
    policy: &'a Policy,
    rules: Compiled,
    /// Default-policy matchers. Path redaction does not depend on which rules the owner enabled.
    privacy: Compiled,
    report: Report,
    entries: usize,
    locations: usize,
    resident: Resident,
    work: u64,
    seen: Vec<String>,
    local_review: &'a mut dyn FnMut(&str, &str),
    published: HashMap<String, Location>,
    /// Final-artifact state (ADR-8348). `None` for a staging-tree audit.
    artifact: Option<artifact::State>,
}

impl<'a> Scan<'a> {
    fn new(policy: &'a Policy, local_review: &'a mut dyn FnMut(&str, &str)) -> Result<Self, AuditError> {
        let rules = Compiled::new(policy)?;
        let policy_digest = rules.digest.clone();
        let mut scan = Scan {
            policy,
            rules,
            privacy: Compiled::new(&Policy::default())?,
            report: Report {
                policy_digest,
                ..Report::default()
            },
            entries: 0,
            locations: 0,
            resident: Resident { current: 0, peak: 0 },
            work: 0,
            seen: Vec::new(),
            local_review,
            published: HashMap::new(),
            artifact: None,
        };
        for finding in std::mem::take(&mut scan.rules.obsolete) {
            if scan.report.findings.len() >= scan.policy.max_findings {
                let location = Location {
                    display: "policy".to_owned(),
                    id: "policy".to_owned(),
                };
                return Err(scan.fail_loc("package_hygiene_limit", &location, "finding_cap"));
            }
            scan.report.findings.push(finding);
        }
        Ok(scan)
    }
}

pub(super) fn tree(
    root: &Path,
    policy: &Policy,
    local_review: &mut dyn FnMut(&str, &str),
) -> Result<Report, AuditError> {
    let mut scan = Scan::new(policy, local_review)?;
    let root_loc = scan.publish(".")?;
    let meta = std::fs::symlink_metadata(root)
        .map_err(|_| scan.fail_loc("package_hygiene_io", &root_loc, "read_error"))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(scan.fail_loc("package_hygiene_path", &root_loc, "path"));
    }
    scan.directory(root, root, 0)?;
    Ok(scan.into_report())
}

fn relative(root: &Path, path: &Path) -> Result<String, &'static str> {
    let relative = path.strip_prefix(root).map_err(|_| ".")?;
    let mut components = Vec::new();
    for component in relative.components() {
        let value = component.as_os_str().to_str().ok_or("non_utf8_name")?;
        if value.contains('\\') || value.contains(':') || value.chars().any(char::is_control) {
            return Err("invalid_name");
        }
        components.push(value);
    }
    Ok(components.join("/"))
}

fn format_error(err: &PakError) -> bool {
    matches!(
        err,
        PakError::BadMagic
            | PakError::UnsupportedVersion { .. }
            | PakError::UnsupportedFlags { .. }
            | PakError::TocDecode(_)
            | PakError::HashMismatch { .. }
            | PakError::Corrupt { .. }
    )
}

impl Scan<'_> {
    fn into_report(mut self) -> Report {
        self.stamp(true, "");
        self.report
    }

    fn stamp(&mut self, complete: bool, reason: &str) {
        self.report.complete = complete;
        self.report.incomplete_reason = if complete {
            String::new()
        } else {
            reason.to_owned()
        };
        self.report.peak_memory_bytes = self.resident.peak;
        self.report.work = self.work;
        self.report.limits = self.policy.limits();
        self.report.matcher_bytes = self.rules.matcher_bytes;
        self.report.matchers_compiled = self.rules.matchers_compiled;
        self.report.findings.sort();
    }

    fn next_location_id(&mut self) -> Result<String, AuditError> {
        let id = format!("location-{}", self.locations);
        self.locations = self
            .locations
            .checked_add(1)
            .ok_or_else(|| AuditError::new("package_hygiene_limit", "location_count"))?;
        Ok(id)
    }

    /// A policy match hides the entire path. Overlapping rules cannot expose a fragment, and a
    /// byte regex cannot split UTF-8. The shared scrubber covers homes, keyed secrets and emails.
    fn display_for(&self, original: &str) -> String {
        let matched = self
            .rules
            .raw
            .iter()
            .chain(&self.rules.terms)
            .chain(&self.privacy.raw)
            .any(|rule| rule.regex.is_match(original.as_bytes()));
        if matched {
            "[redacted]".to_owned()
        } else {
            regia_core::privacy::scrub_report(original)
        }
    }

    /// Publish one filesystem location. A repeat of the same original path keeps its ordinal.
    fn publish(&mut self, original: &str) -> Result<Location, AuditError> {
        if let Some(existing) = self.published.get(original) {
            return Ok(existing.clone());
        }
        let id = self.next_location_id()?;
        let display = self.display_for(original);
        (self.local_review)(&id, original);
        let location = Location { display, id };
        self.published.insert(original.to_owned(), location.clone());
        Ok(location)
    }

    /// Pak child identity is the parent ordinal plus the asset id. It does not take a new ordinal,
    /// and the public display keeps the asset id even when the pak path is redacted.
    fn publish_asset(&mut self, parent: &str, asset: AssetId) -> Result<Location, AuditError> {
        let key = format!("{parent}::{asset}");
        if let Some(existing) = self.published.get(&key) {
            return Ok(existing.clone());
        }
        let parent_loc = self.publish(parent)?;
        let location = parent_loc.asset(asset);
        (self.local_review)(&location.id, &key);
        self.published.insert(key, location.clone());
        Ok(location)
    }

    fn fail(&mut self, code: &'static str, path: &str, reason: &str) -> AuditError {
        let location = if let Some(existing) = self.published.get(path) {
            existing.clone()
        } else {
            Location {
                display: self.display_for(path),
                id: String::new(),
            }
        };
        self.fail_loc(code, &location, reason)
    }

    fn fail_loc(&mut self, code: &'static str, location: &Location, reason: &str) -> AuditError {
        self.stamp(false, reason);
        // A file-size admission fires before any scan buffer exists. That refusal keeps
        // `hygiene` null. A scan that has started (memory, work, pak, mutation) keeps the
        // boxed partial report.
        let report = if reason == "file_bytes" {
            None
        } else {
            Some(Box::new(std::mem::take(&mut self.report)))
        };
        AuditError {
            code,
            location: location.display.clone(),
            location_id: if location.id.is_empty() {
                None
            } else {
                Some(location.id.clone())
            },
            report,
            matcher_bytes: self.rules.matcher_bytes,
            matchers_compiled: self.rules.matchers_compiled,
        }
    }

    fn fail_pak(&mut self, path: &str, err: PakError) -> AuditError {
        let text = err.to_string();
        if text.contains("truncated or changed") {
            self.fail("package_hygiene_io", path, "changed_file")
        } else if text.contains("exceeds") || text.contains("exceeding") || text.contains("declares") {
            self.fail("package_hygiene_limit", path, "decompression_bomb")
        } else {
            self.fail("package_hygiene_pak", path, "pak")
        }
    }

    fn rel(&mut self, root: &Path, path: &Path) -> Result<String, AuditError> {
        match relative(root, path) {
            Ok(value) => Ok(value),
            Err(location) => Err(self.fail("package_hygiene_path", location, "path")),
        }
    }

    fn entry(&mut self, path: &str) -> Result<(), AuditError> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| self.fail("package_hygiene_limit", path, "entry_count"))?;
        if self.entries > self.policy.max_entries {
            return Err(self.fail("package_hygiene_limit", path, "entry_count"));
        }
        Ok(())
    }

    fn bytes(&mut self, path: &str, size: u64) -> Result<(), AuditError> {
        if size > self.policy.max_file_bytes {
            return Err(self.fail("package_hygiene_limit", path, "file_bytes"));
        }
        let next = self.report.bytes_scanned.saturating_add(size);
        if next < self.report.bytes_scanned || next > self.policy.max_total_bytes {
            return Err(self.fail("package_hygiene_limit", path, "total_bytes"));
        }
        self.report.bytes_scanned = next;
        Ok(())
    }

    fn charge_work(&mut self, path: &str, bytes: u64) -> Result<(), AuditError> {
        if bytes == 0 {
            return Ok(());
        }
        let next = self.work.saturating_add(bytes);
        if next < self.work || next > self.policy.max_work {
            return Err(self.fail("package_hygiene_limit", path, "work_budget"));
        }
        self.work = next;
        Ok(())
    }

    fn with_buffer(
        &mut self,
        bytes: u64,
        path: &str,
        body: impl FnOnce(&mut Self) -> Result<(), AuditError>,
    ) -> Result<(), AuditError> {
        if !self.resident.charge(bytes, self.policy.max_memory_bytes) {
            return Err(self.fail("package_hygiene_limit", path, "memory_budget"));
        }
        let result = body(self);
        self.resident.release(bytes);
        result
    }

    fn directory(&mut self, root: &Path, dir: &Path, depth: usize) -> Result<(), AuditError> {
        let rel_full = self.rel(root, dir)?;
        let rel_dir = if rel_full.is_empty() {
            ".".to_owned()
        } else {
            rel_full
        };
        if depth > 64 {
            return Err(self.fail("package_hygiene_limit", &rel_dir, "depth"));
        }
        let read =
            std::fs::read_dir(dir).map_err(|_| self.fail("package_hygiene_io", &rel_dir, "read_error"))?;
        let mut entries = Vec::new();
        for entry in read {
            self.entry(&rel_dir)?;
            entries.push(
                entry
                    .map_err(|_| self.fail("package_hygiene_io", &rel_dir, "read_error"))?
                    .path(),
            );
        }
        entries.sort();
        for path in entries {
            let rel = match relative(root, &path) {
                Ok(value) => value,
                Err(label) => {
                    // Invalid names get an identity but never a reveal callback or an echo.
                    let id = self.next_location_id()?;
                    let location = Location {
                        display: label.to_owned(),
                        id,
                    };
                    return Err(self.fail_loc("package_hygiene_path", &location, "path"));
                }
            };
            self.publish(&rel)?;
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|_| self.fail("package_hygiene_io", &rel, "read_error"))?;
            if meta.file_type().is_symlink() || (!meta.is_dir() && !meta.is_file()) {
                return Err(self.fail("package_hygiene_path", &rel, "path"));
            }
            if self.policy.scan_names || self.policy.scan_secrets {
                self.scan_name(&rel, rel.as_bytes())?;
            }
            if meta.is_dir() {
                self.directory(root, &path, depth + 1)?;
            } else {
                self.file(&path, &rel, meta.len())?;
            }
        }
        Ok(())
    }

    fn file(&mut self, path: &Path, rel: &str, size: u64) -> Result<(), AuditError> {
        self.bytes(rel, size)?;
        let before = self.snap(path, rel)?;
        invoke_probe(path);
        self.ensure_stable(path, rel, &before)?;
        self.read_content(path, rel, size)?;
        self.report.files_scanned = self.report.files_scanned.saturating_add(1);
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("fpak"))
        {
            if self.artifact.is_some() {
                self.staged_pak(path, rel)?;
            } else {
                self.pak(path, rel)?;
            }
        } else if self.artifact.is_some() {
            self.nested_file(path, rel, size)?;
        }
        self.ensure_stable(path, rel, &before)
    }

    fn snap(&mut self, path: &Path, rel: &str) -> Result<Snap, AuditError> {
        let meta = std::fs::symlink_metadata(path)
            .map_err(|_| self.fail("package_hygiene_io", rel, "read_error"))?;
        Ok(Snap {
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }

    fn ensure_stable(&mut self, path: &Path, rel: &str, before: &Snap) -> Result<(), AuditError> {
        let now = self.snap(path, rel)?;
        if now.len != before.len || now.modified != before.modified {
            return Err(self.fail("package_hygiene_io", rel, "changed_file"));
        }
        Ok(())
    }

    fn read_content(&mut self, path: &Path, rel: &str, size: u64) -> Result<(), AuditError> {
        let file =
            std::fs::File::open(path).map_err(|_| self.fail("package_hygiene_io", rel, "read_error"))?;
        match self.plan(size) {
            Plan::Refuse(reason) => Err(self.fail("package_hygiene_limit", rel, reason)),
            Plan::Whole => self.read_whole(file, rel, size),
            Plan::Stream { window, overlap } => self.stream_reader(file, rel, size, window, overlap, 0),
        }
    }

    fn plan(&self, size: u64) -> Plan {
        let Some(width) = self.max_content_width() else {
            if size > self.policy.max_memory_bytes {
                return Plan::Refuse("unbounded_regex");
            }
            return Plan::Whole;
        };
        if width as u64 > self.policy.max_memory_bytes {
            if size <= self.policy.max_memory_bytes {
                return Plan::Whole;
            }
            return Plan::Refuse("pattern_wider_than_memory");
        }
        let window_u = self
            .policy
            .scan_window_bytes
            .max(width as u64)
            .min(self.policy.max_memory_bytes);
        let Some(window) = usize::try_from(window_u).ok() else {
            return Plan::Refuse("memory_budget");
        };
        let overlap = width.saturating_sub(1);
        if window <= overlap {
            return Plan::Refuse("pattern_wider_than_memory");
        }
        Plan::Stream { window, overlap }
    }

    fn max_content_width(&self) -> Option<usize> {
        let mut width = 0usize;
        for slot in self.content_slots() {
            let rule = self.matcher(slot).and_then(|matcher| matcher.stream_width())?;
            width = width.max(rule);
        }
        Some(width)
    }

    fn read_whole(&mut self, file: std::fs::File, rel: &str, size: u64) -> Result<(), AuditError> {
        let Some(len) = usize::try_from(size).ok() else {
            return Err(self.fail("package_hygiene_limit", rel, "memory_budget"));
        };
        self.with_buffer(size, rel, |scan| {
            let mut data = Vec::new();
            data.try_reserve_exact(len)
                .map_err(|_| scan.fail("package_hygiene_limit", rel, "memory_budget"))?;
            file.take(size.saturating_add(1))
                .read_to_end(&mut data)
                .map_err(|_| scan.fail("package_hygiene_io", rel, "read_error"))?;
            if data.len() as u64 != size {
                return Err(scan.fail("package_hygiene_io", rel, "changed_file"));
            }
            scan.charge_work(rel, size)?;
            scan.scan_buffer(rel, &data, false)?;
            Ok(())
        })
    }

    fn stream_reader(
        &mut self,
        mut reader: impl Read,
        rel: &str,
        size: u64,
        window: usize,
        overlap: usize,
        base: u64,
    ) -> Result<(), AuditError> {
        if size == 0 {
            let mut extra = [0u8; 1];
            let n = reader
                .read(&mut extra)
                .map_err(|_| self.fail("package_hygiene_io", rel, "read_error"))?;
            if n != 0 {
                return Err(self.fail("package_hygiene_io", rel, "changed_file"));
            }
            return Ok(());
        }
        let Some(size_usize) = usize::try_from(size).ok() else {
            return Err(self.fail("package_hygiene_limit", rel, "memory_budget"));
        };
        let window = window.clamp(1, size_usize);
        let overlap = overlap.min(window.saturating_sub(1));
        self.seen.clear();
        let slots = self.stream_slots();
        self.with_buffer(window as u64, rel, |scan| {
            let mut buf = Vec::new();
            buf.try_reserve_exact(window)
                .map_err(|_| scan.fail("package_hygiene_limit", rel, "memory_budget"))?;
            buf.resize(window, 0);
            let mut filled = 0usize;
            let mut origin = base;
            let mut read_total = 0u64;
            loop {
                while filled < window && read_total < size {
                    let want = (window - filled).min(usize::try_from(size - read_total).unwrap_or(0));
                    if want == 0 {
                        break;
                    }
                    let n = reader
                        .read(&mut buf[filled..filled + want])
                        .map_err(|_| scan.fail("package_hygiene_io", rel, "read_error"))?;
                    if n == 0 {
                        return Err(scan.fail("package_hygiene_io", rel, "changed_file"));
                    }
                    filled += n;
                    read_total += n as u64;
                    scan.charge_work(rel, n as u64)?;
                }
                let eos = read_total == size;
                scan.scan_slots(
                    rel,
                    View {
                        bytes: &buf[..filled],
                        origin,
                        bos: origin == base,
                        eos,
                        names: false,
                    },
                    &slots,
                )?;
                if eos {
                    break;
                }
                let keep = overlap.min(filled);
                if keep >= filled {
                    return Err(scan.fail("package_hygiene_limit", rel, "pattern_wider_than_memory"));
                }
                let stride = filled - keep;
                buf.copy_within(stride..filled, 0);
                origin += stride as u64;
                filled = keep;
            }
            let mut extra = [0u8; 1];
            let n = reader
                .read(&mut extra)
                .map_err(|_| scan.fail("package_hygiene_io", rel, "read_error"))?;
            if n != 0 {
                return Err(scan.fail("package_hygiene_io", rel, "changed_file"));
            }
            Ok(())
        })
    }

    fn pak(&mut self, path: &Path, rel: &str) -> Result<(), AuditError> {
        let opened = self.open_pak(path, rel)?;
        let charged = opened.charged;
        let result = self.scan_pak(rel, &opened.pak);
        if charged > 0 {
            self.resident.release(charged);
        }
        result
    }

    fn open_pak(&mut self, path: &Path, rel: &str) -> Result<OpenedPak, AuditError> {
        if pak_owned_reads() {
            return self.read_owned_pak(path, rel);
        }
        match PakReader::open(path) {
            Ok(mut pak) => {
                pak.set_max_entry_bytes(self.policy.max_file_bytes);
                Ok(OpenedPak { pak, charged: 0 })
            }
            Err(err) if format_error(&err) => Err(self.fail_pak(rel, err)),
            Err(_) => {
                let len = std::fs::symlink_metadata(path)
                    .map(|meta| meta.len())
                    .unwrap_or(u64::MAX);
                if len > self.policy.max_memory_bytes
                    || self.resident.current.saturating_add(len) > self.policy.max_memory_bytes
                {
                    Err(self.fail("package_hygiene_limit", rel, "pak_resident"))
                } else {
                    self.read_owned_pak(path, rel)
                }
            }
        }
    }

    fn read_owned_pak(&mut self, path: &Path, rel: &str) -> Result<OpenedPak, AuditError> {
        let len = std::fs::symlink_metadata(path)
            .map_err(|_| self.fail("package_hygiene_io", rel, "read_error"))?
            .len();
        if len > self.policy.max_memory_bytes
            || self.resident.current.saturating_add(len) > self.policy.max_memory_bytes
        {
            return Err(self.fail("package_hygiene_limit", rel, "pak_resident"));
        }
        if !self.resident.charge(len, self.policy.max_memory_bytes) {
            return Err(self.fail("package_hygiene_limit", rel, "memory_budget"));
        }
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(_) => {
                self.resident.release(len);
                return Err(self.fail("package_hygiene_io", rel, "read_error"));
            }
        };
        if data.len() as u64 != len {
            self.resident.release(len);
            return Err(self.fail("package_hygiene_io", rel, "changed_file"));
        }
        match PakReader::from_bytes(data, rel, len) {
            Ok(mut pak) => {
                pak.set_max_entry_bytes(self.policy.max_file_bytes);
                Ok(OpenedPak { pak, charged: len })
            }
            Err(err) => {
                self.resident.release(len);
                Err(self.fail_pak(rel, err))
            }
        }
    }

    fn scan_pak(&mut self, rel: &str, pak: &PakReader) -> Result<(), AuditError> {
        if pak.toc.entries.len() > self.policy.max_entries.saturating_sub(self.entries)
            || pak.toc.manifest.len() > self.policy.max_entries
        {
            return Err(self.fail("package_hygiene_limit", rel, "entry_count"));
        }
        if self.policy.scan_names || self.policy.scan_secrets {
            let names: Vec<(AssetId, String, String)> = pak
                .toc
                .manifest
                .iter()
                .map(|manifest| {
                    (
                        manifest.id,
                        format!("{rel}::{}", manifest.id),
                        manifest.path.clone(),
                    )
                })
                .collect();
            for (id, entry, manifest_path) in names {
                self.publish_asset(rel, id)?;
                self.scan_name(&entry, manifest_path.as_bytes())?;
            }
        }
        let mut listed: Vec<(AssetId, u64)> = pak
            .toc
            .entries
            .iter()
            .map(|entry| (entry.id, entry.size))
            .collect();
        listed.sort_by_key(|(id, _)| *id);
        if listed.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(self.fail("package_hygiene_pak", rel, "pak"));
        }
        for (id, size) in listed {
            let path = format!("{rel}::{id}");
            self.publish_asset(rel, id)?;
            self.entry(&path)?;
            self.bytes(&path, size)?;
            if size > self.policy.max_memory_bytes {
                return Err(self.fail("package_hygiene_limit", &path, "decompression_bomb"));
            }
            if self.resident.current.saturating_add(size) > self.policy.max_memory_bytes {
                return Err(self.fail("package_hygiene_limit", &path, "nested_limit"));
            }
            self.with_buffer(size, &path, |scan| {
                let bytes = pak
                    .read_decompressed(id)
                    .map_err(|err| scan.fail_pak(&path, err))?;
                if bytes.len() as u64 != size {
                    return Err(scan.fail("package_hygiene_pak", &path, "pak"));
                }
                scan.charge_work(&path, size)?;
                scan.scan_buffer(&path, &bytes, false)?;
                if scan.artifact.is_some() {
                    scan.nested_bytes(&path, bytes)?;
                }
                Ok(())
            })?;
            self.report.pak_entries_scanned = self.report.pak_entries_scanned.saturating_add(1);
        }
        Ok(())
    }

    fn scan_name(&mut self, path: &str, bytes: &[u8]) -> Result<(), AuditError> {
        self.charge_work(path, bytes.len() as u64)?;
        let slots = self.name_slots();
        self.seen.clear();
        self.scan_slots(
            path,
            View {
                bytes,
                origin: 0,
                bos: true,
                eos: true,
                names: true,
            },
            &slots,
        )
    }

    fn scan_buffer(&mut self, path: &str, bytes: &[u8], names: bool) -> Result<(), AuditError> {
        let text = !names && !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok();
        let slots = if names {
            self.name_slots()
        } else {
            self.whole_slots(text)
        };
        self.seen.clear();
        self.scan_slots(
            path,
            View {
                bytes,
                origin: 0,
                bos: true,
                eos: true,
                names,
            },
            &slots,
        )
    }

    fn content_slots(&self) -> Vec<Slot> {
        let mut slots = Vec::with_capacity(self.rules.raw.len() + self.rules.terms.len() + 1);
        slots.extend((0..self.rules.raw.len()).map(Slot::Raw));
        slots.extend((0..self.rules.terms.len()).map(Slot::Term));
        if self.rules.email.is_some() {
            slots.push(Slot::Email);
        }
        slots
    }

    fn whole_slots(&self, text: bool) -> Vec<Slot> {
        let mut slots = self.content_slots();
        if !text {
            slots.retain(|slot| !matches!(slot, Slot::Email));
        }
        slots
    }

    fn stream_slots(&self) -> Vec<Slot> {
        self.content_slots()
            .into_iter()
            .filter(|slot| {
                self.matcher(*slot)
                    .and_then(|matcher| matcher.stream_width())
                    .is_some()
            })
            .collect()
    }

    fn name_slots(&self) -> Vec<Slot> {
        let mut slots = Vec::new();
        if self.policy.scan_names {
            slots.extend((0..self.rules.terms.len()).map(Slot::Term));
        }
        if self.rules.key_file.is_some() {
            slots.push(Slot::Key);
        }
        slots
    }

    fn matcher(&self, slot: Slot) -> Option<&super::policy::Matcher> {
        match slot {
            Slot::Raw(index) => self.rules.raw.get(index),
            Slot::Term(index) => self.rules.terms.get(index),
            Slot::Email => self.rules.email.as_ref(),
            Slot::Key => self.rules.key_file.as_ref(),
        }
    }

    fn scan_slots(&mut self, path: &str, view: View<'_>, slots: &[Slot]) -> Result<(), AuditError> {
        for slot in slots {
            let found = match self.matcher(*slot) {
                Some(matcher) if !self.seen.iter().any(|seen| seen == &matcher.id) => {
                    let local = if view.bos && view.eos {
                        matcher.find_in(view.bytes)
                    } else {
                        matcher.find_in_window(view.bytes, view.bos, view.eos)
                    };
                    local.map(|local| (matcher.id.clone(), local))
                }
                _ => None,
            };
            let Some((id, local)) = found else {
                continue;
            };
            self.seen.push(id.clone());
            let Some(offset) = usize::try_from(view.origin.saturating_add(local as u64)).ok() else {
                return Err(self.fail("package_hygiene_limit", path, "memory_budget"));
            };
            self.record(path, &id, offset, view.names)?;
        }
        Ok(())
    }

    fn record(&mut self, path: &str, id: &str, offset: usize, names: bool) -> Result<(), AuditError> {
        if self.allowed(path, id) {
            self.report.allowed_findings = self.report.allowed_findings.saturating_add(1);
            return Ok(());
        }
        let location = match self.published.get(path) {
            Some(location) => location.clone(),
            None => {
                return Err(self.fail("package_hygiene_path", path, "path"));
            }
        };
        let source = if names { "name" } else { "content" };
        // A final-artifact location can be scanned in several residual segments; keep the first.
        if self.artifact.is_some()
            && self
                .report
                .findings
                .iter()
                .any(|hit| hit.location_id == location.id && hit.rule == id && hit.source == source)
        {
            return Ok(());
        }
        if self.report.findings.len() >= self.policy.max_findings {
            return Err(self.fail("package_hygiene_limit", path, "finding_cap"));
        }
        self.report.findings.push(Finding {
            path: location.display,
            location_id: location.id,
            rule: id.to_owned(),
            source: source.to_owned(),
            offset,
        });
        Ok(())
    }
}

thread_local! {
    static MUTATION_PROBE: Cell<Option<fn(&Path)>> = const { Cell::new(None) };
    static PAK_OWNED_READS: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn set_mutation_probe(probe: Option<fn(&Path)>) {
    MUTATION_PROBE.with(|cell| cell.set(probe));
}

pub(super) fn set_pak_owned_reads(owned: bool) {
    PAK_OWNED_READS.with(|cell| cell.set(owned));
}

fn invoke_probe(path: &Path) {
    MUTATION_PROBE.with(|cell| {
        if let Some(probe) = cell.get() {
            probe(path);
        }
    });
}

fn pak_owned_reads() -> bool {
    PAK_OWNED_READS.with(Cell::get)
}
