// SPDX-License-Identifier: MPL-2.0
//! Final-artifact inspection (ADR-8348).
//!
//! Original Rust. ZIP structure follows the public PKWARE APPNOTE (local headers, central
//! directory, end-of-central-directory and zip64 records); tar follows the POSIX ustar/pax
//! interchange format with the GNU long-name extension; gzip is RFC 1952 through the existing
//! `flate2` decoder. Containers are read in place: nothing is extracted to disk, names are
//! never used as filesystem paths, and the artifact is opened read-only.
//!
//! Every entry is charged to the existing entry, byte, memory and work budgets. Nested
//! containers are opened up to `max_container_depth`; deeper ones, ambiguous names, duplicate
//! names, overlapping ZIP records, CRC or size mismatches fail closed. Encrypted entries and
//! container formats this tool cannot open are listed as uninspected, never silently passed.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flate2::Crc;
use flate2::read::{DeflateDecoder, MultiGzDecoder};
use regia_asset::pak::{FPAK_MAGIC, PakReader};

use super::{Location, Plan, Scan, View};
use crate::package_hygiene::receipt::{ArtifactOptions, ContainerCoverage, UninspectedEntry};
use crate::package_hygiene::{AuditError, Policy, Report};

/// Bytes sniffed at the start of an entry to recognize a container.
const HEAD: usize = 512;
/// Largest GNU long name or pax header read into memory.
const MAX_TAR_META: u64 = 1 << 20;
/// Zero padding accepted after the tar end marker (blocking factors up to 32 Ki blocks).
const MAX_TAR_TRAILER: u64 = 16 << 20;

const LOCAL_SIG: u32 = 0x0403_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const EOCD_LEN: usize = 22;

/// Final-artifact bookkeeping carried by a [`Scan`].
pub(in crate::package_hygiene) struct State {
    max_depth: u32,
    prefixes: Vec<String>,
    /// In-package paths an allowance may name for a location key.
    allow_paths: HashMap<String, Vec<String>>,
    open: u32,
    /// Containers opened, in traversal order.
    pub containers: Vec<ContainerCoverage>,
    /// Entries and containers this tool did not decode.
    pub uninspected: Vec<UninspectedEntry>,
    /// Archive (ZIP, tar, gzip) entries visited.
    pub archive_entries: u64,
    /// Deepest container level opened (the artifact is 0).
    pub deepest: u32,
    /// Directory for transient spool files of nested containers larger than memory.
    spool_dir: PathBuf,
    spools: u64,
}

impl State {
    fn new(policy: &Policy, options: &ArtifactOptions, artifact: &Path) -> Self {
        let spool_dir = options.spool_dir.clone().unwrap_or_else(|| {
            artifact
                .parent()
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
        });
        Self {
            spool_dir,
            spools: 0,
            max_depth: policy.max_container_depth,
            prefixes: options.allow_prefixes.clone(),
            allow_paths: HashMap::new(),
            open: 0,
            containers: Vec::new(),
            uninspected: Vec::new(),
            archive_entries: 0,
            deepest: 0,
        }
    }
}

/// What an audit produced: the report, the coverage state and the artifact's container kind.
pub(in crate::package_hygiene) struct Outcome {
    pub report: Report,
    pub state: State,
    pub kind: &'static str,
    pub display: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    None,
    Zip,
    Fpak,
    Gzip,
    Tar,
    Unsupported(&'static str),
}

fn sniff(head: &[u8]) -> Kind {
    let starts = |magic: &[u8]| head.starts_with(magic);
    if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
        Kind::Zip
    } else if starts(&FPAK_MAGIC) {
        Kind::Fpak
    } else if starts(&[0x1f, 0x8b]) {
        Kind::Gzip
    } else if head.get(257..262) == Some(b"ustar") {
        Kind::Tar
    } else if starts(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Kind::Unsupported("zstd")
    } else if starts(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        Kind::Unsupported("xz")
    } else if starts(b"BZh")
        && head.get(3).is_some_and(|level| (b'1'..=b'9').contains(level))
        && head.get(4..10) == Some(&[0x31, 0x41, 0x59, 0x26, 0x53, 0x59])
    {
        Kind::Unsupported("bzip2")
    } else if starts(&[b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c]) {
        Kind::Unsupported("7z")
    } else if starts(b"Rar!\x1a\x07") {
        Kind::Unsupported("rar")
    } else if starts(b"hsqs") {
        Kind::Unsupported("squashfs")
    } else if starts(&[0x04, 0x22, 0x4d, 0x18]) {
        Kind::Unsupported("lz4")
    } else if starts(b"\x7fELF") && head.get(8..11) == Some(b"AI\x02") {
        Kind::Unsupported("appimage")
    } else if starts(b"MSCF") {
        Kind::Unsupported("cab")
    } else if starts(b"xar!") {
        Kind::Unsupported("xar")
    } else if starts(b"!<arch>\n") {
        Kind::Unsupported("ar")
    } else {
        Kind::None
    }
}

/// Bytes of an archive that can be read at an offset: the artifact file or a decoded buffer.
#[derive(Clone, Copy)]
enum Source<'s> {
    File(&'s File, u64),
    Mem(&'s [u8]),
}

impl<'s> Source<'s> {
    fn len(self) -> u64 {
        match self {
            Self::File(_, len) => len,
            Self::Mem(bytes) => bytes.len() as u64,
        }
    }

    fn read_at(self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        match self {
            Self::File(file, _) => {
                let mut file = file;
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(buf)
            }
            Self::Mem(bytes) => {
                let start = usize::try_from(offset).map_err(|_| eof())?;
                let end = start.checked_add(buf.len()).ok_or_else(eof)?;
                buf.copy_from_slice(bytes.get(start..end).ok_or_else(eof)?);
                Ok(())
            }
        }
    }

    fn section(self, offset: u64, len: u64) -> Section<'s> {
        Section {
            source: self,
            pos: offset,
            end: offset.saturating_add(len),
        }
    }
}

fn eof() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::UnexpectedEof)
}

/// A bounded byte range of a [`Source`].
struct Section<'s> {
    source: Source<'s>,
    pos: u64,
    end: u64,
}

impl Read for Section<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.end.saturating_sub(self.pos);
        let want = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
        if want == 0 {
            return Ok(0);
        }
        let got = match self.source {
            Source::File(file, _) => {
                let mut file = file;
                file.seek(SeekFrom::Start(self.pos))?;
                file.read(&mut buf[..want])?
            }
            Source::Mem(bytes) => {
                let start = usize::try_from(self.pos).map_err(|_| eof())?;
                let slice = bytes.get(start..start + want).ok_or_else(eof)?;
                buf[..want].copy_from_slice(slice);
                want
            }
        };
        self.pos += got as u64;
        Ok(got)
    }
}

/// Counts decoded bytes and keeps their CRC-32.
struct Counted<R> {
    inner: R,
    total: u64,
    crc: Crc,
}

impl<R: Read> Counted<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            total: 0,
            crc: Crc::new(),
        }
    }
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.crc.update(&buf[..n]);
        self.total += n as u64;
        Ok(n)
    }
}

/// Tees a streamed entry into a private spool file once its first bytes show a container.
/// The spool name is generated, never taken from the archive.
struct Spool<R> {
    inner: R,
    path: PathBuf,
    head: Vec<u8>,
    kind: Option<Kind>,
    file: Option<File>,
    failed: bool,
}

impl<R: Read> Spool<R> {
    fn new(inner: R, path: PathBuf) -> Self {
        Self {
            inner,
            path,
            head: Vec::new(),
            kind: None,
            file: None,
            failed: false,
        }
    }

    fn decide(&mut self) {
        let kind = sniff(&self.head);
        self.kind = Some(kind);
        if matches!(kind, Kind::Zip | Kind::Fpak | Kind::Gzip | Kind::Tar) {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.path)
            {
                Ok(mut file) => {
                    self.failed |= file.write_all(&self.head).is_err();
                    self.file = Some(file);
                }
                Err(_) => self.failed = true,
            }
        }
        self.head = Vec::new();
    }
}

impl<R: Read> Read for Spool<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if self.kind.is_some() {
            if let Some(file) = self.file.as_mut() {
                self.failed |= file.write_all(&buf[..n]).is_err();
            }
            return Ok(n);
        }
        self.head.extend_from_slice(&buf[..n]);
        if self.head.len() >= 262 || n == 0 {
            self.decide();
        }
        Ok(n)
    }
}

/// Removes a spool file however the nested scan ends.
struct SpoolGuard(PathBuf);

impl Drop for SpoolGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct ZipEntry {
    name: String,
    is_dir: bool,
    flags: u16,
    method: u16,
    crc: u32,
    compressed: u64,
    size: u64,
    offset: u64,
    data_start: u64,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// A tar numeric field: octal text, or GNU base-256 when the high bit is set.
fn tar_number(field: &[u8]) -> Option<u64> {
    let first = *field.first()?;
    if first & 0x80 != 0 {
        let mut value = u64::from(first & 0x7f);
        for &byte in &field[1..] {
            value = value.checked_mul(256)?.checked_add(u64::from(byte))?;
        }
        return Some(value);
    }
    let text: Vec<u8> = field
        .iter()
        .copied()
        .skip_while(|b| *b == b' ')
        .take_while(|b| *b != 0 && *b != b' ')
        .collect();
    if text.is_empty() {
        return Some(0);
    }
    let mut value = 0u64;
    for byte in text {
        if !(b'0'..=b'7').contains(&byte) {
            return None;
        }
        value = value.checked_mul(8)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(value)
}

fn nul_terminated(field: &[u8]) -> &[u8] {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    &field[..end]
}

/// `path` and `size` of a pax extended header; other keys are ignored.
fn pax_records(data: &[u8]) -> Option<(Option<String>, Option<u64>)> {
    let mut path = None;
    let mut size = None;
    let mut at = 0usize;
    while at < data.len() {
        let rest = &data[at..];
        let space = rest.iter().position(|b| *b == b' ')?;
        let len: usize = std::str::from_utf8(&rest[..space]).ok()?.parse().ok()?;
        let record = rest.get(..len)?;
        let body = record.get(space + 1..len.checked_sub(1)?)?;
        if record.last() != Some(&b'\n') {
            return None;
        }
        let eq = body.iter().position(|b| *b == b'=')?;
        let (key, value) = (&body[..eq], &body[eq + 1..]);
        match key {
            b"path" => path = Some(std::str::from_utf8(value).ok()?.to_owned()),
            b"size" => size = Some(std::str::from_utf8(value).ok()?.parse().ok()?),
            _ => {}
        }
        at = at.checked_add(len)?;
    }
    Some((path, size))
}

/// Read until `buf` is full or the reader ends; answers the bytes read.
fn read_full(reader: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

fn remap(mut err: AuditError, code: &'static str, reason: &str) -> AuditError {
    err.code = code;
    if let Some(report) = err.report.as_mut() {
        report.incomplete_reason = reason.to_owned();
    }
    err
}

/// Audit a final artifact in place. `Outcome::report` may carry findings.
pub(in crate::package_hygiene) fn run(
    path: &Path,
    policy: &Policy,
    options: &ArtifactOptions,
) -> Result<Outcome, AuditError> {
    let mut no_review = |_: &str, _: &str| {};
    let mut scan = Scan::new(policy, &mut no_review)?;
    scan.artifact = Some(State::new(policy, options, path));
    let meta =
        std::fs::symlink_metadata(path).map_err(|_| AuditError::new("package_hygiene_io", "artifact"))?;
    if meta.file_type().is_symlink() || (!meta.is_dir() && !meta.is_file()) {
        return Err(AuditError::new("package_hygiene_path", "artifact"));
    }
    let kind;
    let display;
    if meta.is_dir() {
        let root = scan.publish(".")?;
        display = root.display.clone();
        scan.enter(".")?;
        scan.directory(path, path, 0)?;
        scan.leave(".", "directory", scan.report.files_scanned)?;
        kind = "directory";
    } else {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| AuditError::new("package_hygiene_path", "non_utf8_name"))?
            .to_owned();
        let root = scan.publish(&name)?;
        display = root.display.clone();
        if let Some(state) = scan.artifact.as_mut() {
            state.allow_paths.insert(name.clone(), Vec::new());
        }
        let size = meta.len();
        let before = scan.snap(path, &name)?;
        let file = File::open(path).map_err(|_| scan.fail("package_hygiene_io", &name, "read_error"))?;
        let mut head = vec![0u8; HEAD];
        let got = read_full(&mut (&file), &mut head)
            .map_err(|_| scan.fail("package_hygiene_io", &name, "read_error"))?;
        head.truncate(got);
        kind = match sniff(&head) {
            Kind::Zip => {
                scan.report.files_scanned += 1;
                scan.enter(&name)?;
                let entries = scan.zip(&name, Source::File(&file, size), true)?;
                scan.leave(&name, "zip", entries)?;
                "zip"
            }
            Kind::Gzip => {
                scan.report.files_scanned += 1;
                scan.enter(&name)?;
                let (container, entries) = scan.gzip(&name, section_of(&file, size), true)?;
                scan.leave(&name, container, entries)?;
                container
            }
            Kind::Tar => {
                scan.report.files_scanned += 1;
                scan.enter(&name)?;
                let entries = scan.tar(&name, &mut section_of(&file, size), true)?;
                scan.leave(&name, "tar", entries)?;
                "tar"
            }
            Kind::Fpak => {
                scan.file(path, &name, size)?;
                "fpak"
            }
            Kind::Unsupported(container) => {
                scan.file(path, &name, size)?;
                container
            }
            Kind::None => {
                scan.file(path, &name, size)?;
                "file"
            }
        };
        scan.ensure_stable(path, &name, &before)?;
    }
    let state = scan
        .artifact
        .take()
        .unwrap_or_else(|| State::new(policy, options, path));
    Ok(Outcome {
        report: scan.into_report(),
        state,
        kind,
        display,
    })
}

fn section_of(file: &File, len: u64) -> Section<'_> {
    Source::File(file, len).section(0, len)
}

impl Scan<'_> {
    /// Waivers match the location key itself (staging trees and directory artifacts) or one of
    /// the in-package paths registered for an archive entry.
    pub(super) fn allowed(&self, path: &str, rule: &str) -> bool {
        if self.rules.allowed(path, rule) {
            return true;
        }
        if self.artifact.is_none() {
            return false;
        }
        self.allow_candidates(path)
            .iter()
            .any(|candidate| self.rules.allowed(candidate, rule))
    }

    fn allow_candidates(&self, key: &str) -> Vec<String> {
        let Some(state) = &self.artifact else {
            return Vec::new();
        };
        if let Some(paths) = state.allow_paths.get(key) {
            return paths.clone();
        }
        match key.rsplit_once("::") {
            Some((parent, child)) => self
                .allow_candidates(parent)
                .into_iter()
                .map(|path| format!("{path}::{child}"))
                .collect(),
            None => vec![key.to_owned()],
        }
    }

    /// Publish an archive child and register the paths its allowances may name.
    fn child(&mut self, parent: &str, name: &str) -> Result<String, AuditError> {
        let key = format!("{parent}::{name}");
        let parents = self.allow_candidates(parent);
        let paths = if parents.is_empty() {
            let mut paths = vec![name.to_owned()];
            if let Some(state) = &self.artifact {
                for prefix in &state.prefixes {
                    if let Some(rest) = name.strip_prefix(prefix.as_str())
                        && !rest.is_empty()
                    {
                        paths.push(rest.to_owned());
                    }
                }
            }
            paths
        } else {
            parents
                .into_iter()
                .map(|path| format!("{path}::{name}"))
                .collect()
        };
        if let Some(state) = self.artifact.as_mut() {
            state.allow_paths.insert(key.clone(), paths);
        }
        self.publish(&key)?;
        Ok(key)
    }

    fn enter(&mut self, key: &str) -> Result<(), AuditError> {
        let Some(state) = self.artifact.as_mut() else {
            return Ok(());
        };
        let level = state.open;
        if level > state.max_depth {
            return Err(self.fail("package_hygiene_limit", key, "depth"));
        }
        state.open += 1;
        state.deepest = state.deepest.max(level);
        Ok(())
    }

    fn leave(&mut self, key: &str, kind: &str, entries: u64) -> Result<(), AuditError> {
        let location = self.location(key);
        if let Some(state) = self.artifact.as_mut() {
            state.open = state.open.saturating_sub(1);
            state.containers.push(ContainerCoverage {
                location_id: location.id,
                path: location.display,
                kind: kind.to_owned(),
                depth: state.open,
                entries,
            });
        }
        Ok(())
    }

    fn location(&self, key: &str) -> Location {
        self.published.get(key).cloned().unwrap_or_else(|| Location {
            display: self.display_for(key),
            id: String::new(),
        })
    }

    fn uninspected(&mut self, key: &str, kind: &str, reason: &str) {
        let location = self.location(key);
        if let Some(state) = self.artifact.as_mut() {
            state.uninspected.push(UninspectedEntry {
                location_id: location.id,
                path: location.display,
                kind: kind.to_owned(),
                reason: reason.to_owned(),
            });
        }
    }

    fn archive_error(&mut self, key: &str) -> AuditError {
        self.fail("package_hygiene_archive", key, "archive")
    }

    /// Validate an archive entry name without touching the filesystem. A backslash is read as
    /// the separator Windows extractors treat it as (Windows PowerShell 5.1 writes them), a
    /// leading `./` and a trailing `/` are dropped; absolute, drive, empty, `.` and `..`
    /// components are refused as `traversal_name`, control characters as `path`.
    fn archive_name(&mut self, raw: &str) -> Result<String, AuditError> {
        let normalized = raw.replace('\\', "/");
        let name = normalized.strip_prefix("./").unwrap_or(&normalized);
        let name = name.strip_suffix('/').unwrap_or(name);
        let traversal = name.starts_with('/')
            || name.contains(':')
            || name
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..");
        let reason = if name.is_empty() {
            Some(("invalid_name", "path"))
        } else if traversal {
            Some(("traversal_name", "traversal_name"))
        } else if name.chars().any(char::is_control) {
            Some(("invalid_name", "path"))
        } else {
            None
        };
        if let Some((label, reason)) = reason {
            let id = self.next_location_id()?;
            let location = Location {
                display: label.to_owned(),
                id,
            };
            return Err(self.fail_loc("package_hygiene_path", &location, reason));
        }
        Ok(name.to_owned())
    }

    /// Refuse a second entry whose name differs only in ASCII case (extractors on
    /// case-insensitive file systems would merge them).
    fn unique_name(&mut self, names: &mut HashSet<String>, key: &str, name: &str) -> Result<(), AuditError> {
        if names.insert(name.to_ascii_lowercase()) {
            return Ok(());
        }
        Err(self.fail("package_hygiene_archive", key, "duplicate_name"))
    }

    /// Run `body` with `bytes` charged to the memory budget, answering its value.
    fn charged<T>(
        &mut self,
        bytes: u64,
        key: &str,
        body: impl FnOnce(&mut Self) -> Result<T, AuditError>,
    ) -> Result<T, AuditError> {
        if !self.resident.charge(bytes, self.policy.max_memory_bytes) {
            return Err(self.fail("package_hygiene_limit", key, "memory_budget"));
        }
        let result = body(self);
        self.resident.release(bytes);
        result
    }

    fn read_charged(
        &mut self,
        source: Source<'_>,
        offset: u64,
        len: u64,
        key: &str,
    ) -> Result<Vec<u8>, AuditError> {
        let Ok(size) = usize::try_from(len) else {
            return Err(self.fail("package_hygiene_limit", key, "memory_budget"));
        };
        if self.resident.current.saturating_add(len) > self.policy.max_memory_bytes {
            return Err(self.fail("package_hygiene_limit", key, "memory_budget"));
        }
        let mut buf = Vec::new();
        buf.try_reserve_exact(size)
            .map_err(|_| self.fail("package_hygiene_limit", key, "memory_budget"))?;
        buf.resize(size, 0);
        source
            .read_at(offset, &mut buf)
            .map_err(|_| self.archive_error(key))?;
        Ok(buf)
    }

    /// Scan container metadata or residual bytes at their offset within the container.
    fn scan_at(&mut self, key: &str, bytes: &[u8], origin: u64) -> Result<(), AuditError> {
        self.bytes(key, bytes.len() as u64)?;
        self.charge_work(key, bytes.len() as u64)?;
        let text = !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok();
        let slots = self.whole_slots(text);
        self.seen.clear();
        self.scan_slots(
            key,
            View {
                bytes,
                origin,
                bos: true,
                eos: true,
                names: false,
            },
            &slots,
        )
    }

    /// Scan a raw byte range of a source (ZIP gaps, headers and uninspected payloads).
    fn scan_segment(
        &mut self,
        key: &str,
        source: Source<'_>,
        start: u64,
        len: u64,
    ) -> Result<(), AuditError> {
        if len == 0 {
            return Ok(());
        }
        let room = self.policy.max_memory_bytes.saturating_sub(self.resident.current);
        if len <= room {
            let bytes = self.read_charged(source, start, len, key)?;
            return self.charged(len, key, |scan| scan.scan_at(key, &bytes, start));
        }
        match self.plan(len) {
            Plan::Stream { window, overlap } => {
                self.bytes(key, len)?;
                self.stream_reader(source.section(start, len), key, len, window, overlap, start)
                    .map_err(|err| {
                        if err.code == "package_hygiene_io" {
                            remap(err, "package_hygiene_archive", "archive")
                        } else {
                            err
                        }
                    })
            }
            Plan::Refuse(reason) => Err(self.fail("package_hygiene_limit", key, reason)),
            Plan::Whole => Err(self.fail("package_hygiene_limit", key, "memory_budget")),
        }
    }

    /// Decode, verify and scan one entry payload of `size` bytes, then open it when it is a
    /// container. `crc` is checked when the archive records one.
    fn payload(
        &mut self,
        key: &str,
        reader: impl Read,
        size: u64,
        crc: Option<u32>,
    ) -> Result<(), AuditError> {
        let room = self.policy.max_memory_bytes.saturating_sub(self.resident.current);
        let mut counted = Counted::new(reader.take(size.saturating_add(1)));
        if size <= room {
            let Ok(len) = usize::try_from(size) else {
                return Err(self.fail("package_hygiene_limit", key, "memory_budget"));
            };
            let data = self.charged(size, key, |scan| {
                let mut data = Vec::new();
                data.try_reserve_exact(len)
                    .map_err(|_| scan.fail("package_hygiene_limit", key, "memory_budget"))?;
                data.resize(len, 0);
                counted
                    .read_exact(&mut data)
                    .map_err(|_| scan.archive_error(key))?;
                let mut extra = [0u8; 1];
                if read_full(&mut counted, &mut extra).map_err(|_| scan.archive_error(key))? != 0 {
                    return Err(scan.fail("package_hygiene_limit", key, "decompression_bomb"));
                }
                if crc.is_some_and(|expected| expected != counted.crc.sum()) {
                    return Err(scan.archive_error(key));
                }
                scan.charge_work(key, size)?;
                scan.scan_buffer(key, &data, false)?;
                Ok(data)
            })?;
            // The decoded buffer stays charged while a nested container is open.
            return self.charged(size, key, |scan| scan.nested_bytes(key, data));
        }
        match self.plan(size) {
            Plan::Stream { window, overlap } => {
                let spool_path = self.spool_path();
                let guard = SpoolGuard(spool_path.clone());
                let mut spool = Spool::new(&mut counted, spool_path);
                let result = self.stream_reader(&mut spool, key, size, window, overlap, 0);
                let (kind, spooled, failed) = (spool.kind, spool.file.take(), spool.failed);
                drop(spool);
                if let Err(err) = result {
                    return Err(if counted.total > size {
                        remap(err, "package_hygiene_limit", "decompression_bomb")
                    } else if err.code == "package_hygiene_io" {
                        remap(err, "package_hygiene_archive", "archive")
                    } else {
                        err
                    });
                }
                if crc.is_some_and(|expected| expected != counted.crc.sum()) {
                    return Err(self.archive_error(key));
                }
                match kind.unwrap_or(Kind::None) {
                    Kind::None => Ok(()),
                    Kind::Unsupported(kind) => {
                        self.uninspected(key, kind, "unsupported_container");
                        Ok(())
                    }
                    kind => {
                        let Some(file) = spooled.filter(|_| !failed) else {
                            return Err(self.fail("package_hygiene_limit", key, "nested_limit"));
                        };
                        drop(file);
                        self.spooled(key, &guard.0, size, kind)
                    }
                }
            }
            Plan::Refuse(reason) => Err(self.fail("package_hygiene_limit", key, reason)),
            Plan::Whole => Err(self.fail("package_hygiene_limit", key, "nested_limit")),
        }
    }

    fn spool_path(&mut self) -> PathBuf {
        let Some(state) = self.artifact.as_mut() else {
            return PathBuf::new();
        };
        state.spools += 1;
        state.spool_dir.join(format!(
            ".regia-hygiene-spool-{}-{}",
            std::process::id(),
            state.spools
        ))
    }

    /// Open a nested container that was spooled because it does not fit the memory budget.
    /// The spool is read like a staged file; `.fpak` spools are mapped, not charged.
    fn spooled(&mut self, key: &str, path: &Path, size: u64, kind: Kind) -> Result<(), AuditError> {
        let file = File::open(path).map_err(|_| self.fail("package_hygiene_limit", key, "nested_limit"))?;
        if file.metadata().map(|meta| meta.len()).ok() != Some(size) {
            return Err(self.fail("package_hygiene_limit", key, "nested_limit"));
        }
        self.enter(key)?;
        let (name, entries) = match kind {
            Kind::Fpak => {
                drop(file);
                let before = self.report.pak_entries_scanned;
                self.pak(path, key)?;
                ("fpak", self.report.pak_entries_scanned - before)
            }
            Kind::Zip => ("zip", self.zip(key, Source::File(&file, size), false)?),
            Kind::Tar => ("tar", self.tar(key, &mut section_of(&file, size), false)?),
            _ => self.gzip(key, section_of(&file, size), false)?,
        };
        self.leave(key, name, entries)
    }

    /// Open a decoded entry that is itself a container.
    pub(super) fn nested_bytes(&mut self, key: &str, data: Vec<u8>) -> Result<(), AuditError> {
        match sniff(&data) {
            Kind::None => Ok(()),
            Kind::Unsupported(kind) => {
                self.uninspected(key, kind, "unsupported_container");
                Ok(())
            }
            Kind::Zip => {
                self.enter(key)?;
                let entries = self.zip(key, Source::Mem(&data), false)?;
                self.leave(key, "zip", entries)
            }
            Kind::Tar => {
                self.enter(key)?;
                let entries = self.tar(key, &mut &data[..], false)?;
                self.leave(key, "tar", entries)
            }
            Kind::Gzip => {
                self.enter(key)?;
                let (kind, entries) = self.gzip(key, &data[..], false)?;
                self.leave(key, kind, entries)
            }
            Kind::Fpak => {
                self.enter(key)?;
                let len = data.len() as u64;
                let mut pak = PakReader::from_bytes(data, key, len).map_err(|err| self.fail_pak(key, err))?;
                pak.set_max_entry_bytes(self.policy.max_file_bytes);
                let before = self.report.pak_entries_scanned;
                self.scan_pak(key, &pak)?;
                let entries = self.report.pak_entries_scanned - before;
                self.leave(key, "fpak", entries)
            }
        }
    }

    /// A regular file of a directory artifact that is a container (staged `.fpak` files take
    /// [`Self::staged_pak`]). Containers are read from the file, not copied into memory.
    pub(super) fn nested_file(&mut self, path: &Path, rel: &str, size: u64) -> Result<(), AuditError> {
        let file = File::open(path).map_err(|_| self.fail("package_hygiene_io", rel, "read_error"))?;
        let mut head = vec![0u8; HEAD];
        let got = read_full(&mut (&file), &mut head)
            .map_err(|_| self.fail("package_hygiene_io", rel, "read_error"))?;
        head.truncate(got);
        match sniff(&head) {
            Kind::None => Ok(()),
            Kind::Unsupported(kind) => {
                self.uninspected(rel, kind, "unsupported_container");
                Ok(())
            }
            Kind::Fpak => self.staged_pak(path, rel),
            Kind::Zip => {
                self.enter(rel)?;
                let entries = self.zip(rel, Source::File(&file, size), false)?;
                self.leave(rel, "zip", entries)
            }
            Kind::Gzip => {
                self.enter(rel)?;
                let (kind, entries) = self.gzip(rel, section_of(&file, size), false)?;
                self.leave(rel, kind, entries)
            }
            Kind::Tar => {
                self.enter(rel)?;
                let entries = self.tar(rel, &mut section_of(&file, size), false)?;
                self.leave(rel, "tar", entries)
            }
        }
    }

    /// A `.fpak` file of a directory artifact, through the existing mapped pak scan.
    pub(super) fn staged_pak(&mut self, path: &Path, rel: &str) -> Result<(), AuditError> {
        self.enter(rel)?;
        let before = self.report.pak_entries_scanned;
        self.pak(path, rel)?;
        let entries = self.report.pak_entries_scanned - before;
        self.leave(rel, "fpak", entries)
    }

    /// Inspect a ZIP (also APK, AAB, JAR) in place. With `residual`, every byte that is not an
    /// inspected payload (headers, central directory, gaps, signing blocks, comment, and the raw
    /// bytes of uninspected entries) is scanned as content of the container.
    fn zip(&mut self, key: &str, source: Source<'_>, residual: bool) -> Result<u64, AuditError> {
        let len = source.len();
        if len < EOCD_LEN as u64 {
            return Err(self.archive_error(key));
        }
        // Most archives carry no comment: try the last EOCD_LEN bytes before reading the whole
        // comment window, so a tight memory budget does not refuse an ordinary archive.
        let short = self.read_charged(source, len - EOCD_LEN as u64, EOCD_LEN as u64, key)?;
        let plain = u32_at(&short, 0) == Some(EOCD_SIG) && u16_at(&short, 20) == Some(0);
        drop(short);
        let tail_len = if plain {
            EOCD_LEN as u64
        } else {
            len.min((EOCD_LEN + usize::from(u16::MAX)) as u64)
        };
        let tail_start = len - tail_len;
        let tail = self.read_charged(source, tail_start, tail_len, key)?;
        let found = self.charged(tail_len, key, |_| {
            Ok((0..=tail.len() - EOCD_LEN).rev().find(|&at| {
                u32_at(&tail, at) == Some(EOCD_SIG)
                    && u16_at(&tail, at + 20)
                        .is_some_and(|comment| usize::from(comment) == tail.len() - at - EOCD_LEN)
            }))
        })?;
        let Some(at) = found else {
            return Err(self.archive_error(key));
        };
        let eocd = tail_start + at as u64;
        let field16 = |offset: usize| u16_at(&tail, at + offset).unwrap_or(u16::MAX);
        let field32 = |offset: usize| u32_at(&tail, at + offset).unwrap_or(u32::MAX);
        if field16(4) != 0 || field16(6) != 0 || field16(8) != field16(10) {
            return Err(self.archive_error(key));
        }
        let mut count = u64::from(field16(10));
        let mut cd_size = u64::from(field32(12));
        let mut cd_offset = u64::from(field32(16));
        drop(tail);
        let mut structural_end = eocd;
        if count == u64::from(u16::MAX) || cd_size == u64::from(u32::MAX) || cd_offset == u64::from(u32::MAX)
        {
            let Some(locator_at) = eocd.checked_sub(20) else {
                return Err(self.archive_error(key));
            };
            let locator = self.read_charged(source, locator_at, 20, key)?;
            if u32_at(&locator, 0) != Some(ZIP64_LOCATOR_SIG) || u32_at(&locator, 4) != Some(0) {
                return Err(self.archive_error(key));
            }
            let record_at = u64_at(&locator, 8).unwrap_or(u64::MAX);
            if record_at.checked_add(56).is_none_or(|end| end > locator_at) {
                return Err(self.archive_error(key));
            }
            let record = self.read_charged(source, record_at, 56, key)?;
            let Some((n, size, offset)) = zip64_record(&record) else {
                return Err(self.archive_error(key));
            };
            (count, cd_size, cd_offset) = (n, size, offset);
            structural_end = record_at;
        }
        if cd_offset.checked_add(cd_size) != Some(structural_end) || count.saturating_mul(46) > cd_size {
            return Err(self.archive_error(key));
        }
        let remaining = self.policy.max_entries.saturating_sub(self.entries) as u64;
        if count > remaining {
            return Err(self.fail("package_hygiene_limit", key, "entry_count"));
        }
        let central = self.read_charged(source, cd_offset, cd_size, key)?;
        let parsed = self.charged(cd_size, key, |scan| {
            if residual {
                scan.scan_at(key, &central, cd_offset)?;
            }
            Ok(parse_central(&central, count))
        })?;
        let Some(raw_entries) = parsed else {
            return Err(self.archive_error(key));
        };
        drop(central);

        // Structure first: names, duplicates, local headers and overlap, before any payload.
        let mut names = HashSet::new();
        let mut entries = Vec::with_capacity(raw_entries.len());
        for (raw_name, mut entry) in raw_entries {
            let Ok(text) = String::from_utf8(raw_name.clone()) else {
                let id = self.next_location_id()?;
                let location = Location {
                    display: "non_utf8_name".to_owned(),
                    id,
                };
                return Err(self.fail_loc("package_hygiene_path", &location, "path"));
            };
            let name = self.archive_name(&text)?;
            self.unique_name(&mut names, &format!("{key}::{name}"), &name)?;
            if entry.offset.checked_add(30).is_none_or(|end| end > cd_offset) {
                return Err(self.archive_error(key));
            }
            let local = self.read_charged(source, entry.offset, 30, key)?;
            let name_len = u64::from(u16_at(&local, 26).unwrap_or(u16::MAX));
            let extra_len = u64::from(u16_at(&local, 28).unwrap_or(u16::MAX));
            if u32_at(&local, 0) != Some(LOCAL_SIG) || u16_at(&local, 8) != Some(entry.method) {
                return Err(self.archive_error(key));
            }
            let local_name = self.read_charged(source, entry.offset + 30, name_len, key)?;
            if local_name != raw_name {
                return Err(self.archive_error(key));
            }
            entry.data_start = entry.offset + 30 + name_len + extra_len;
            if entry
                .data_start
                .checked_add(entry.compressed)
                .is_none_or(|end| end > cd_offset)
            {
                return Err(self.archive_error(key));
            }
            entry.name = name;
            entries.push(entry);
        }
        let mut spans: Vec<(u64, u64)> = entries
            .iter()
            .map(|entry| (entry.offset, entry.data_start + entry.compressed))
            .collect();
        spans.sort_unstable();
        if spans.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(self.archive_error(key));
        }

        let mut inspected = Vec::new();
        for entry in &entries {
            let child = self.child(key, &entry.name)?;
            self.entry(&child)?;
            if let Some(state) = self.artifact.as_mut() {
                state.archive_entries += 1;
            }
            if self.policy.scan_names || self.policy.scan_secrets {
                self.scan_name(&child, entry.name.as_bytes())?;
            }
            if entry.is_dir {
                continue;
            }
            if entry.flags & 0x0041 != 0 {
                self.uninspected(&child, "zip_entry", "encrypted");
                continue;
            }
            if entry.method != 0 && entry.method != 8 {
                self.uninspected(&child, "zip_entry", "compression_method");
                continue;
            }
            if entry.method == 0 && entry.compressed != entry.size {
                return Err(self.archive_error(&child));
            }
            self.bytes(&child, entry.size)?;
            let section = source.section(entry.data_start, entry.compressed);
            if entry.method == 0 {
                self.payload(&child, section, entry.size, Some(entry.crc))?;
            } else {
                self.payload(&child, DeflateDecoder::new(section), entry.size, Some(entry.crc))?;
            }
            inspected.push((entry.data_start, entry.data_start + entry.compressed));
        }
        if residual {
            inspected.sort_unstable();
            let mut cursor = 0u64;
            for (start, end) in inspected {
                self.scan_segment(key, source, cursor, start - cursor)?;
                cursor = end;
            }
            self.scan_segment(key, source, cursor, cd_offset - cursor)?;
            self.scan_segment(key, source, structural_end, len - structural_end)?;
        }
        Ok(entries.len() as u64)
    }

    /// Inspect a gzip stream (all members). A tar inside is read as one `tar.gz` container;
    /// any other payload is one decoded entry named `[gunzip]`, held in memory.
    fn gzip(
        &mut self,
        key: &str,
        reader: impl Read,
        residual: bool,
    ) -> Result<(&'static str, u64), AuditError> {
        let mut decoder = MultiGzDecoder::new(reader);
        let mut head = vec![0u8; HEAD];
        let got = read_full(&mut decoder, &mut head).map_err(|_| self.archive_error(key))?;
        head.truncate(got);
        if sniff(&head) == Kind::Tar {
            let mut chained = (&head[..]).chain(decoder);
            let entries = self.tar(key, &mut chained, residual)?;
            return Ok(("tar.gz", entries));
        }
        let child = self.child(key, "[gunzip]")?;
        self.entry(&child)?;
        if let Some(state) = self.artifact.as_mut() {
            state.archive_entries += 1;
        }
        let room = self.policy.max_memory_bytes.saturating_sub(self.resident.current);
        let limit = room.min(self.policy.max_file_bytes);
        let mut data = head;
        let mut rest = (&mut decoder).take(limit.saturating_add(1).saturating_sub(data.len() as u64));
        rest.read_to_end(&mut data)
            .map_err(|_| self.archive_error(&child))?;
        if data.len() as u64 > limit {
            return Err(self.fail("package_hygiene_limit", &child, "nested_limit"));
        }
        let size = data.len() as u64;
        self.bytes(&child, size)?;
        self.charged(size, &child, |scan| {
            scan.charge_work(&child, size)?;
            scan.scan_buffer(&child, &data, false)?;
            Ok(())
        })?;
        self.charged(size, &child, |scan| scan.nested_bytes(&child, data))?;
        Ok(("gzip", 1))
    }

    /// Inspect a tar stream sequentially. With `residual`, header blocks, long names and pax
    /// records are scanned as content of the container (they carry owner and group names).
    fn tar(&mut self, key: &str, reader: &mut dyn Read, residual: bool) -> Result<u64, AuditError> {
        let mut names = HashSet::new();
        let mut long_name: Option<String> = None;
        let mut pax_path: Option<String> = None;
        let mut pax_size: Option<u64> = None;
        let mut position = 0u64;
        let mut count = 0u64;
        let mut header = [0u8; 512];
        loop {
            let got = read_full(reader, &mut header).map_err(|_| self.archive_error(key))?;
            if got != 512 {
                return Err(self.archive_error(key));
            }
            let at = position;
            position += 512;
            if header.iter().all(|b| *b == 0) {
                self.tar_trailer(key, reader)?;
                return Ok(count);
            }
            if residual {
                self.scan_at(key, &header, at)?;
            }
            let stored = tar_number(&header[148..156]);
            let computed: u64 = header
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    if (148..156).contains(&i) {
                        32
                    } else {
                        u64::from(*b)
                    }
                })
                .sum();
            let posix = &header[257..263] == b"ustar\0";
            let gnu = &header[257..265] == b"ustar  \0";
            if stored != Some(computed) || !(posix || gnu) {
                return Err(self.archive_error(key));
            }
            let Some(mut size) = tar_number(&header[124..136]) else {
                return Err(self.archive_error(key));
            };
            if let Some(override_size) = pax_size.take() {
                size = override_size;
            }
            let typeflag = header[156];
            if matches!(typeflag, b'L' | b'x' | b'g') {
                self.entry(key)?;
                if size > MAX_TAR_META {
                    return Err(self.archive_error(key));
                }
                let data = self.tar_data(key, reader, size, at + 512, residual)?;
                position += size.next_multiple_of(512);
                match typeflag {
                    b'L' => {
                        let name = std::str::from_utf8(nul_terminated(&data))
                            .map_err(|_| self.archive_error(key))?;
                        long_name = Some(name.to_owned());
                    }
                    b'x' => {
                        let Some((path, size)) = pax_records(&data) else {
                            return Err(self.archive_error(key));
                        };
                        pax_path = path.or(pax_path);
                        pax_size = size.or(pax_size);
                    }
                    _ => {}
                }
                continue;
            }
            let raw_name = if let Some(name) = long_name.take().or(pax_path.take()) {
                name
            } else {
                let name = nul_terminated(&header[0..100]);
                let prefix = if posix {
                    nul_terminated(&header[345..500])
                } else {
                    &[]
                };
                let joined = if prefix.is_empty() {
                    name.to_vec()
                } else {
                    [prefix, b"/", name].concat()
                };
                match String::from_utf8(joined) {
                    Ok(text) => text,
                    Err(_) => {
                        let id = self.next_location_id()?;
                        let location = Location {
                            display: "non_utf8_name".to_owned(),
                            id,
                        };
                        return Err(self.fail_loc("package_hygiene_path", &location, "path"));
                    }
                }
            };
            let is_dir = typeflag == b'5';
            let regular = matches!(typeflag, b'0' | 0 | b'7');
            if !regular && !matches!(typeflag, b'5' | b'1' | b'2' | b'3' | b'4' | b'6') {
                return Err(self.archive_error(key));
            }
            if !regular && size != 0 {
                return Err(self.archive_error(key));
            }
            if is_dir && matches!(raw_name.as_str(), "." | "./") {
                continue;
            }
            let name = self.archive_name(&raw_name)?;
            self.unique_name(&mut names, &format!("{key}::{name}"), &name)?;
            let child = self.child(key, &name)?;
            self.entry(&child)?;
            if let Some(state) = self.artifact.as_mut() {
                state.archive_entries += 1;
            }
            count += 1;
            if self.policy.scan_names || self.policy.scan_secrets {
                self.scan_name(&child, name.as_bytes())?;
            }
            if matches!(typeflag, b'1' | b'2' | b'3' | b'4' | b'6') {
                // Links and special files are refused as in a staging tree.
                return Err(self.fail("package_hygiene_path", &child, "path"));
            }
            if is_dir {
                continue;
            }
            self.bytes(&child, size)?;
            self.payload(&child, (&mut *reader).take(size), size, None)?;
            self.tar_skip(key, reader, size.next_multiple_of(512) - size)?;
            position += size.next_multiple_of(512);
        }
    }

    fn tar_data(
        &mut self,
        key: &str,
        reader: &mut dyn Read,
        size: u64,
        origin: u64,
        residual: bool,
    ) -> Result<Vec<u8>, AuditError> {
        let padded = size.next_multiple_of(512);
        let Ok(len) = usize::try_from(padded) else {
            return Err(self.archive_error(key));
        };
        self.charged(padded, key, |scan| {
            let mut data = vec![0u8; len];
            if read_full(reader, &mut data).map_err(|_| scan.archive_error(key))? != len {
                return Err(scan.archive_error(key));
            }
            data.truncate(usize::try_from(size).unwrap_or(len));
            if residual {
                scan.scan_at(key, &data, origin)?;
            }
            Ok(data)
        })
    }

    fn tar_skip(&mut self, key: &str, reader: &mut dyn Read, pad: u64) -> Result<(), AuditError> {
        let mut buf = [0u8; 512];
        let Ok(pad) = usize::try_from(pad) else {
            return Err(self.archive_error(key));
        };
        if read_full(reader, &mut buf[..pad]).map_err(|_| self.archive_error(key))? != pad {
            return Err(self.archive_error(key));
        }
        Ok(())
    }

    /// After the first zero block: the rest of the stream must be zero padding.
    fn tar_trailer(&mut self, key: &str, reader: &mut dyn Read) -> Result<(), AuditError> {
        let mut buf = [0u8; 8192];
        let mut total = 0u64;
        loop {
            let got = read_full(reader, &mut buf).map_err(|_| self.archive_error(key))?;
            if buf[..got].iter().any(|b| *b != 0) {
                return Err(self.archive_error(key));
            }
            total += got as u64;
            if total > MAX_TAR_TRAILER {
                return Err(self.archive_error(key));
            }
            if got < buf.len() {
                return Ok(());
            }
        }
    }
}

/// Entry count, central-directory size and offset of a single-disk zip64 end record.
fn zip64_record(record: &[u8]) -> Option<(u64, u64, u64)> {
    if u32_at(record, 0)? != ZIP64_EOCD_SIG
        || u32_at(record, 16)? != 0
        || u32_at(record, 20)? != 0
        || u64_at(record, 24)? != u64_at(record, 32)?
    {
        return None;
    }
    Some((u64_at(record, 32)?, u64_at(record, 40)?, u64_at(record, 48)?))
}

/// Parse `count` central-directory records that must fill `central` exactly.
fn parse_central(central: &[u8], count: u64) -> Option<Vec<(Vec<u8>, ZipEntry)>> {
    let mut at = 0usize;
    let mut entries = Vec::new();
    for _ in 0..count {
        if u32_at(central, at)? != CENTRAL_SIG {
            return None;
        }
        let flags = u16_at(central, at + 8)?;
        let method = u16_at(central, at + 10)?;
        let crc = u32_at(central, at + 16)?;
        let mut compressed = u64::from(u32_at(central, at + 20)?);
        let mut size = u64::from(u32_at(central, at + 24)?);
        let name_len = usize::from(u16_at(central, at + 28)?);
        let extra_len = usize::from(u16_at(central, at + 30)?);
        let comment_len = usize::from(u16_at(central, at + 32)?);
        let disk = u16_at(central, at + 34)?;
        let mut offset = u64::from(u32_at(central, at + 42)?);
        let name_start = at + 46;
        let name = central.get(name_start..name_start + name_len)?.to_vec();
        let extra = central.get(name_start + name_len..name_start + name_len + extra_len)?;
        let max = u64::from(u32::MAX);
        if size == max || compressed == max || offset == max {
            let mut x = 0usize;
            let mut found = false;
            while x + 4 <= extra.len() {
                let id = u16_at(extra, x)?;
                let len = usize::from(u16_at(extra, x + 2)?);
                let body = extra.get(x + 4..x + 4 + len)?;
                if id == 1 {
                    let mut field = 0usize;
                    if size == max {
                        size = u64_at(body, field)?;
                        field += 8;
                    }
                    if compressed == max {
                        compressed = u64_at(body, field)?;
                        field += 8;
                    }
                    if offset == max {
                        offset = u64_at(body, field)?;
                    }
                    found = true;
                    break;
                }
                x += 4 + len;
            }
            if !found {
                return None;
            }
        }
        if disk != 0 && disk != u16::MAX {
            return None;
        }
        let is_dir = name.last() == Some(&b'/');
        // A directory holds no data. Some writers deflate the empty payload (two bytes), so
        // only a nonzero decoded size is refused; the stored bytes stay residual content.
        if is_dir && size != 0 {
            return None;
        }
        entries.push((
            name,
            ZipEntry {
                name: String::new(),
                is_dir,
                flags,
                method,
                crc,
                compressed,
                size,
                offset,
                data_start: 0,
            },
        ));
        at = name_start + name_len + extra_len + comment_len;
    }
    (at == central.len()).then_some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_recognizes_supported_and_unsupported_containers() {
        assert_eq!(sniff(b"PK\x03\x04rest"), Kind::Zip);
        assert_eq!(sniff(&FPAK_MAGIC), Kind::Fpak);
        assert_eq!(sniff(&[0x1f, 0x8b, 8]), Kind::Gzip);
        assert_eq!(sniff(&[0x28, 0xb5, 0x2f, 0xfd]), Kind::Unsupported("zstd"));
        assert_eq!(sniff(b"BZh9 is plain text"), Kind::None);
        assert_eq!(sniff(b"plain"), Kind::None);
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(sniff(&tar), Kind::Tar);
    }

    #[test]
    fn tar_numbers_parse_octal_and_base_256() {
        assert_eq!(tar_number(b"0000644\0"), Some(0o644));
        assert_eq!(tar_number(b"  17 \0"), Some(0o17));
        assert_eq!(tar_number(&[0x80, 0, 0, 1, 0]), Some(256));
        assert_eq!(tar_number(b"9\0"), None);
    }

    #[test]
    fn pax_records_take_path_and_size_only() {
        let data = "16 path=a/b.txt\n11 size=12\n11 mtime=1\n";
        assert_eq!(
            pax_records(data.as_bytes()),
            Some((Some("a/b.txt".to_owned()), Some(12)))
        );
        assert_eq!(pax_records(b"9 path=x"), None);
    }
}
