// SPDX-License-Identifier: MPL-2.0
//! Bounded file and archive adapters. Archive members are decompressed in memory, never extracted.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use walkdir::WalkDir;

use super::{Result, fail, io_error};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct Budget {
    bytes: usize,
    files: usize,
    /// Every directory entry visited, shared by discovery and extracted-bundle walks.
    entries: usize,
}

impl Budget {
    fn visit(&mut self) -> Result<()> {
        self.entries = self
            .entries
            .checked_sub(1)
            .ok_or_else(|| fail("logs_triage_limit", "input discovery exceeds entry budget"))?;
        Ok(())
    }

    fn file(&mut self) -> Result<()> {
        self.files = self
            .files
            .checked_sub(1)
            .ok_or_else(|| fail("logs_triage_limit", "too many input files"))?;
        Ok(())
    }

    fn read(&mut self, mut reader: impl Read) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        reader
            .by_ref()
            .take((self.bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        self.bytes = self
            .bytes
            .checked_sub(bytes.len())
            .ok_or_else(|| fail("logs_triage_limit", "input exceeds read budget"))?;
        Ok(bytes)
    }

    fn path(&mut self, path: &Path) -> Result<Vec<u8>> {
        self.file()?;
        if !fs::symlink_metadata(path).map_err(io_error)?.is_file() {
            return Err(fail(
                "logs_triage_path",
                "input must be a regular file, not a symlink",
            ));
        }
        self.read(File::open(path).map_err(io_error)?)
    }
}

fn safe_member(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.contains(['\\', ':', '*', '?', '[', ']'])
        && !name.chars().any(char::is_control)
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn useful(path: &str) -> bool {
    let base = path.rsplit_once(".from-").map_or(path, |(base, _)| base);
    let name = base.rsplit(['/', '\\']).next().unwrap_or(base);
    name == "manifest.txt"
        || name == "summary.json"
        || name == "summary.partial.json"
        || base.ends_with(".jsonl")
        || base.ends_with(".log")
        || base.ends_with(".summary.json")
        || base.ends_with(".summary.partial.json")
        || base.ends_with("-summary.json")
        || base.ends_with("-system.json")
}

fn payload(bytes: Vec<u8>) -> Value {
    // Keep a chunk split inside a UTF-8 character intact until the analyzer stitches its bytes.
    match String::from_utf8(bytes) {
        Ok(text) => Value::String(text),
        Err(error) => json!({"bytes": error.into_bytes()}),
    }
}

// Names are parsed from the right, independent of any application-specific upload prefix.
fn part(path: &Path) -> Result<Option<(PathBuf, u64, u64)>> {
    let Some(name) = path
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_suffix(".zip"))
    else {
        return Ok(None);
    };
    let Some((stem, nums)) = name.rsplit_once(".part") else {
        return Ok(None);
    };
    let Some((n, m)) = nums.split_once("of") else {
        return Err(fail("logs_triage_parts", "invalid split upload name"));
    };
    let (n, m) = (n.parse::<u64>(), m.parse::<u64>());
    match (n, m) {
        (Ok(n), Ok(m)) if n > 0 && n <= m => Ok(Some((path.with_file_name(format!("{stem}.zip")), n, m))),
        _ => Err(fail("logs_triage_parts", "invalid split upload indices")),
    }
}

pub(super) fn load(paths: &[PathBuf], max_bytes: usize, max_files: usize) -> Result<Vec<Value>> {
    let mut budget = Budget {
        bytes: max_bytes,
        files: max_files,
        entries: max_files,
    };
    let mut candidates = BTreeSet::new();
    let mut folders = BTreeSet::new();
    for path in paths {
        let meta = fs::symlink_metadata(path).map_err(io_error)?;
        if meta.file_type().is_symlink() {
            return Err(fail("logs_triage_path", "symlink inputs are not followed"));
        }
        if meta.is_file() {
            candidates.insert(fs::canonicalize(path).map_err(io_error)?);
        } else if meta.is_dir() {
            let root = fs::canonicalize(path).map_err(io_error)?;
            if root.join("manifest.txt").is_file() {
                folders.insert(root);
            } else {
                let mut entries = WalkDir::new(&root).follow_links(false).into_iter();
                while let Some(entry) = entries.next() {
                    let entry =
                        entry.map_err(|_| fail("logs_triage_io", "cannot enumerate input directory"))?;
                    budget.visit()?;
                    if entry.file_type().is_symlink() {
                        return Err(fail("logs_triage_path", "input directory contains a symlink"));
                    }
                    if entry.file_type().is_dir() && entry.path().join("manifest.txt").is_file() {
                        folders.insert(entry.path().to_owned());
                        entries.skip_current_dir();
                    } else if entry.file_type().is_file()
                        && (entry.path().extension().is_some_and(|e| e == "zip")
                            || useful(&entry.file_name().to_string_lossy()))
                    {
                        candidates.insert(entry.path().to_owned());
                    }
                }
            }
        } else {
            return Err(fail("logs_triage_path", "input must be a file or directory"));
        }
        if folders.len().saturating_add(candidates.len()) > max_files {
            return Err(fail("logs_triage_limit", "input discovery exceeds file budget"));
        }
    }
    candidates.retain(|p| !folders.iter().any(|f| p.starts_with(f)));
    let mut bundles = Vec::new();
    for folder in folders {
        let mut files = BTreeMap::new();
        for entry in WalkDir::new(&folder).follow_links(false) {
            let entry = entry.map_err(|_| fail("logs_triage_io", "cannot enumerate bundle"))?;
            budget.visit()?;
            if entry.file_type().is_symlink() {
                return Err(fail("logs_triage_path", "bundle contains a symlink"));
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry
                .path()
                .strip_prefix(&folder)
                .map_err(|_| fail("logs_triage_path", "invalid relative path"))?
                .to_string_lossy()
                .replace('\\', "/");
            if !safe_member(&name) {
                return Err(fail("logs_triage_path", "unsafe input member"));
            }
            if useful(&name) {
                files.insert(name, payload(budget.path(entry.path())?));
            }
        }
        bundles.push(json!({"name":folder.file_name().unwrap_or_default().to_string_lossy(),"files":files}));
    }
    let mut parts: BTreeMap<PathBuf, (u64, BTreeMap<u64, PathBuf>)> = BTreeMap::new();
    // Group loose session files from one directory, so summaries accompany their JSONL.
    let mut loose: BTreeMap<PathBuf, BTreeMap<String, Value>> = BTreeMap::new();
    for path in candidates {
        if let Some((stem, n, m)) = part(&path)? {
            let entry = parts.entry(stem).or_insert_with(|| (m, BTreeMap::new()));
            if entry.0 != m || entry.1.insert(n, path).is_some() {
                return Err(fail("logs_triage_parts", "conflicting split upload parts"));
            }
        } else if path.extension().is_some_and(|e| e == "zip") {
            let bytes = budget.path(&path)?;
            bundles.push(archive(&path, bytes, &mut budget)?);
        } else {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| fail("logs_triage_path", "non-UTF-8 input name"))?;
            let parent = path.parent().unwrap_or(Path::new("."));
            loose
                .entry(parent.to_owned())
                .or_default()
                .insert(name.to_owned(), payload(budget.path(&path)?));
        }
    }
    for (folder, files) in loose {
        bundles.push(json!({"name":folder.file_name().unwrap_or_default().to_string_lossy(),"files":files}));
    }
    for (stem, (total, parts)) in parts {
        if parts.len() as u64 != total || parts.keys().copied().ne(1..=total) {
            return Err(fail(
                "logs_triage_parts",
                "split upload has missing parts; supply the whole set",
            ));
        }
        let mut bytes = Vec::new();
        for path in parts.values() {
            bytes.extend(budget.path(path)?);
        }
        bundles.push(archive(&stem, bytes, &mut budget)?);
    }
    Ok(bundles)
}

// ZIP layout from the public PKWARE APPNOTE (local header, central directory, end record).
const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const END: u32 = 0x0605_4b50;
const END_LEN: usize = 22;

struct Member {
    name: String,
    method: u16,
    flags: u16,
    crc: u32,
    compressed: u64,
    uncompressed: u64,
    local: u64,
}

fn le(bytes: &[u8], at: usize, len: usize) -> Result<u64> {
    let field = at
        .checked_add(len)
        .and_then(|end| bytes.get(at..end))
        .ok_or_else(|| fail("logs_triage_archive", "truncated ZIP record"))?;
    Ok(field
        .iter()
        .rev()
        .fold(0u64, |value, &b| (value << 8) | u64::from(b)))
}

fn index(at: u64) -> Result<usize> {
    usize::try_from(at).map_err(|_| fail("logs_triage_archive", "ZIP offset out of range"))
}

/// Reads the central directory. ZIP64, multi-disk and encrypted archives are refused: tester
/// uploads are small single-file archives, and a refusal is better than a partial read.
fn members(bytes: &[u8]) -> Result<Vec<Member>> {
    let corrupt = || fail("logs_triage_archive", "invalid ZIP central directory");
    let lowest = bytes.len().saturating_sub(END_LEN + usize::from(u16::MAX));
    let last = bytes.len().checked_sub(END_LEN).ok_or_else(corrupt)?;
    // The end record's comment must reach the end of the file exactly.
    let end = (lowest..=last)
        .rev()
        .find(|&at| {
            le(bytes, at, 4).is_ok_and(|sig| sig == u64::from(END))
                && le(bytes, at + 20, 2)
                    .is_ok_and(|n| index(n).is_ok_and(|n| n == bytes.len() - at - END_LEN))
        })
        .ok_or_else(corrupt)?;
    let count = le(bytes, end + 10, 2)?;
    let offset = le(bytes, end + 16, 4)?;
    if le(bytes, end + 4, 2)? != 0
        || le(bytes, end + 6, 2)? != 0
        || count != le(bytes, end + 8, 2)?
        || count == u64::from(u16::MAX)
        || offset == u64::from(u32::MAX)
    {
        return Err(fail(
            "logs_triage_archive",
            "multi-disk and ZIP64 archives are not supported; supply extracted folders",
        ));
    }
    let mut at = index(offset)?;
    let mut out = Vec::new();
    for _ in 0..count {
        if le(bytes, at, 4)? != u64::from(CENTRAL) {
            return Err(corrupt());
        }
        let name_len = index(le(bytes, at + 28, 2)?)?;
        let skip = index(le(bytes, at + 30, 2)? + le(bytes, at + 32, 2)?)?;
        let name = bytes.get(at + 46..at + 46 + name_len).ok_or_else(corrupt)?;
        let member = Member {
            name: std::str::from_utf8(name)
                .map_err(|_| fail("logs_triage_path", "non-UTF-8 archive member name"))?
                .to_owned(),
            flags: le(bytes, at + 8, 2)? as u16,
            method: le(bytes, at + 10, 2)? as u16,
            crc: le(bytes, at + 16, 4)? as u32,
            compressed: le(bytes, at + 20, 4)?,
            uncompressed: le(bytes, at + 24, 4)?,
            local: le(bytes, at + 42, 4)?,
        };
        if member.compressed == u64::from(u32::MAX) || member.uncompressed == u64::from(u32::MAX) {
            return Err(fail(
                "logs_triage_archive",
                "multi-disk and ZIP64 archives are not supported; supply extracted folders",
            ));
        }
        out.push(member);
        at = at + 46 + name_len + skip;
    }
    Ok(out)
}

/// Decompresses one member in memory, charging the budget, and checks its length and CRC-32.
fn member_bytes(bytes: &[u8], member: &Member, budget: &mut Budget) -> Result<Vec<u8>> {
    let corrupt = || {
        fail(
            "logs_triage_archive",
            "ZIP member failed decompression, integrity or length checks",
        )
    };
    if member.flags & 1 != 0 {
        return Err(fail(
            "logs_triage_archive",
            "encrypted ZIP members are not supported",
        ));
    }
    let local = index(member.local)?;
    if le(bytes, local, 4)? != u64::from(LOCAL) {
        return Err(corrupt());
    }
    let start = local + 30 + index(le(bytes, local + 26, 2)? + le(bytes, local + 28, 2)?)?;
    let data = start
        .checked_add(index(member.compressed)?)
        .and_then(|end| bytes.get(start..end))
        .ok_or_else(corrupt)?;
    // One byte past the declared size is read so an understated size is caught, not truncated.
    let limit = member.uncompressed.saturating_add(1);
    let content = match member.method {
        0 => budget.read(data.take(limit))?,
        8 => budget.read(flate2::read::DeflateDecoder::new(data).take(limit))?,
        _ => {
            return Err(fail(
                "logs_triage_archive",
                "unsupported ZIP compression method; use stored or deflate",
            ));
        }
    };
    let mut crc = flate2::Crc::new();
    crc.update(&content);
    if content.len() as u64 != member.uncompressed || crc.sum() != member.crc {
        return Err(corrupt());
    }
    Ok(content)
}

fn archive(path: &Path, bytes: Vec<u8>, budget: &mut Budget) -> Result<Value> {
    let entries = members(&bytes)?;
    let mut names = BTreeSet::new();
    let mut expanded = 0u64;
    for entry in &entries {
        budget.file()?;
        let name = entry.name.strip_suffix('/').unwrap_or(&entry.name);
        if !safe_member(name) || !names.insert(&entry.name) {
            return Err(fail("logs_triage_path", "unsafe or duplicate archive member"));
        }
        expanded = expanded.saturating_add(entry.uncompressed);
        if expanded > budget.bytes as u64 {
            return Err(fail("logs_triage_limit", "archive exceeds expanded byte budget"));
        }
    }
    let mut files = BTreeMap::new();
    for entry in entries
        .iter()
        .filter(|e| !e.name.ends_with('/') && useful(&e.name))
    {
        let content = member_bytes(&bytes, entry, budget)?;
        files.insert(entry.name.clone(), payload(content));
    }
    Ok(json!({"name":path.file_name().unwrap_or_default().to_string_lossy(),"files":files}))
}

pub(super) fn publish(out: &Path, report: &Value) -> Result<()> {
    // Reserving the destination with create_dir provides portable no-clobber semantics.
    fs::create_dir(out).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            fail(
                "logs_triage_output_exists",
                "output already exists; choose a new directory",
            )
        } else {
            io_error(e)
        }
    })?;
    let result = write_report(out, report);
    if result.is_err() {
        let _ = fs::remove_dir_all(out);
    }
    result
}

fn write_report(out: &Path, report: &Value) -> Result<()> {
    let mut names = BTreeSet::new();
    for file in report["files"].as_array().into_iter().flatten() {
        let session = file["session"]
            .as_str()
            .ok_or_else(|| fail("logs_triage_path", "missing session name"))?;
        let name = file["path"]
            .as_str()
            .ok_or_else(|| fail("logs_triage_path", "missing file name"))?;
        if !safe_member(session)
            || session.contains('/')
            || !safe_member(name)
            || !names.insert((session, name))
        {
            return Err(fail(
                "logs_triage_path",
                "unsafe or colliding scrubbed output names",
            ));
        }
        let path = out.join(session).join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        atomic_write(&path, file["text"].as_str().unwrap_or("").as_bytes())?;
    }
    let mut summary = report.clone();
    if let Some(files) = summary["files"].as_array_mut() {
        for file in files {
            if let Some(file) = file.as_object_mut() {
                file.remove("text");
            }
        }
    }
    let text = serde_json::to_vec_pretty(&summary)
        .map_err(|_| fail("logs_triage_io", "cannot serialize triage report"))?;
    // Written last: a reader can use report.json as the completed-output marker.
    atomic_write(&out.join("report.json"), &text)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_file_name(format!(
        ".triage-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    drop(file);
    fs::rename(temp, path).map_err(io_error)
}
