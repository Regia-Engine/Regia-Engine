// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-core/src/replay_library.rs, commit c9513623f), ADR-8012.
//! A folder of `.lsreplay` files (ADR-8513; adapted, see docs/ported-sources.toml):
//! list newest first, keep the newest N, import with a SHA-256 duplicate check, export with a
//! sidecar, damaged files listed apart.
//!
//! Listing reads each file's header and walks its record frames (kind and length, never a
//! payload), so a cut file is listed as damaged and an aborted match is known without decoding
//! the body. Import and export decode every record ([`super::LsReplayReader`] is the only parser).

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
// det-scan: allow(wall_clock): file dates on disk (list, rotate, import), never sim state
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::sha256::{Sha256, sha256_hex};
use super::{
    LSREPLAY_MAGIC, LsReplayError, LsReplayHeader, LsReplayReader, MAX_FULL_BYTES, MAX_HEADER_BYTES,
    MAX_LSREPLAY_BYTES, MAX_RECORD_BYTES, RECORD_ABORT, RECORD_FULL, read_head,
};

/// The file extension.
pub const LSREPLAY_EXTENSION: &str = "lsreplay";
/// Replays a library keeps by default.
pub const DEFAULT_KEEP_NEWEST: usize = 20;
/// The sidecar format an export writes beside each replay.
pub const META_FORMAT: &str = "regia-lsreplay-meta/1";

/// One replay of a library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayInfo {
    /// The file.
    pub path: PathBuf,
    /// Its file name.
    pub file_name: String,
    /// The modification time, unix seconds (import keeps the source's).
    pub date: u64,
    /// The file size, bytes.
    pub length: u64,
    /// The game id.
    pub game_id: String,
    /// The game's protocol version.
    pub protocol_version: u32,
    /// The engine version that wrote it.
    pub engine_version: String,
    /// The seats at the start.
    pub seats: Vec<u16>,
    /// Ticks per second.
    pub tick_rate: u32,
    /// The match seed.
    pub seed: u64,
    /// Records in the file.
    pub records: u64,
    /// The file holds an abort record (rotation never deletes it).
    pub aborted: bool,
    /// The SHA-256 of the bytes, hex (set by import and export; listing does not read the body).
    pub sha256: Option<String>,
}

/// A file with the extension that does not read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DamagedReplay {
    /// The file.
    pub path: PathBuf,
    /// Its file name.
    pub file_name: String,
    /// The error code (`replay_truncated`, `replay_version_newer`, ...).
    pub code: String,
    /// The reason for a person.
    pub reason: String,
}

/// A listing: the readable replays newest first, and the damaged ones by name.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReplayList {
    /// Readable replays, newest first (then by name).
    pub replays: Vec<ReplayInfo>,
    /// Damaged files, by name.
    pub damaged: Vec<DamagedReplay>,
}

/// What [`ReplayLibrary::rotate`] did.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RotateReport {
    /// Files deleted, oldest first.
    pub deleted: Vec<PathBuf>,
    /// Older files kept because they hold an abort record.
    pub kept_aborted: usize,
    /// Files that could not be deleted, with the reason.
    pub failed: Vec<(PathBuf, String)>,
}

/// An exported replay and its sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedReplay {
    /// The copy.
    pub replay_path: PathBuf,
    /// The `<stem>.json` sidecar beside it ([`META_FORMAT`]).
    pub sidecar_path: PathBuf,
    /// The copy's listing.
    pub info: ReplayInfo,
}

/// A folder of `.lsreplay` files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayLibrary {
    /// The folder.
    pub dir: PathBuf,
    /// Replays [`Self::rotate`] keeps.
    pub keep: usize,
}

/// A JSON string literal of `s` (RFC 8259 § 7 escapes).
fn json_str(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The [`META_FORMAT`] sidecar of `info` (JSON; `regia_net` has no JSON dependency, and the
/// fields are flat).
fn sidecar_json(info: &ReplayInfo, digest: &str) -> String {
    let seats: Vec<String> = info.seats.iter().map(u16::to_string).collect();
    format!(
        "{{\n  \"format\": {},\n  \"replay\": {},\n  \"game_id\": {},\n  \"protocol_version\": {},\n  \"engine_version\": {},\n  \"date\": {},\n  \"length\": {},\n  \"seats\": [{}],\n  \"tick_rate\": {},\n  \"seed\": {},\n  \"aborted\": {},\n  \"sha256\": {}\n}}\n",
        json_str(META_FORMAT),
        json_str(&info.file_name),
        json_str(&info.game_id),
        info.protocol_version,
        json_str(&info.engine_version),
        info.date,
        info.length,
        seats.join(", "),
        info.tick_rate,
        info.seed,
        info.aborted,
        json_str(digest),
    )
}

fn is_replay(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(LSREPLAY_EXTENSION))
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("(no file name)")
        .to_owned()
}

fn unix_secs(time: SystemTime) -> u64 {
    // det-scan: allow(wall_clock): file dates on disk (list, rotate, import), never sim state
    time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn open_err(path: &Path, err: &std::io::Error) -> LsReplayError {
    if err.kind() == std::io::ErrorKind::NotFound {
        LsReplayError::NotFound(path.display().to_string())
    } else {
        LsReplayError::Io(format!("{}: {err}", path.display()))
    }
}

/// Reads `path` whole, refusing a file over [`MAX_LSREPLAY_BYTES`] before reading it.
pub(super) fn read_bounded(path: &Path) -> Result<Vec<u8>, LsReplayError> {
    let file = File::open(path).map_err(|e| open_err(path, &e))?;
    let len = file.metadata().map_err(|e| open_err(path, &e))?.len();
    if len > MAX_LSREPLAY_BYTES {
        return Err(LsReplayError::TooLarge {
            what: "the replay",
            len,
            max: MAX_LSREPLAY_BYTES,
        });
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(MAX_LSREPLAY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| open_err(path, &e))?;
    if bytes.len() as u64 > MAX_LSREPLAY_BYTES {
        return Err(LsReplayError::TooLarge {
            what: "the replay",
            len: bytes.len() as u64,
            max: MAX_LSREPLAY_BYTES,
        });
    }
    Ok(bytes)
}

/// The header, the record count and whether an abort record is present, without reading any
/// payload: the header's bytes, then each record's kind and length (seeking past the payload).
fn scan(path: &Path) -> Result<(LsReplayHeader, u64, bool, SystemTime, u64), LsReplayError> {
    let mut file = File::open(path).map_err(|e| open_err(path, &e))?;
    let meta = file.metadata().map_err(|e| open_err(path, &e))?;
    let len = meta.len();
    if len > MAX_LSREPLAY_BYTES {
        return Err(LsReplayError::TooLarge {
            what: "the replay",
            len,
            max: MAX_LSREPLAY_BYTES,
        });
    }
    let head_max = (LSREPLAY_MAGIC.len() + 8 + MAX_HEADER_BYTES) as u64;
    let mut head = Vec::with_capacity(len.min(head_max) as usize);
    (&mut file)
        .take(head_max)
        .read_to_end(&mut head)
        .map_err(|e| open_err(path, &e))?;
    let (header, mut pos) = read_head(&head)?;
    let mut records = 0u64;
    let mut aborted = false;
    let mut frame = [0u8; 5];
    while (pos as u64) < len {
        let left = len - pos as u64;
        if left < 5 {
            return Err(LsReplayError::Truncated {
                at: pos as u64,
                what: "a record frame",
                need: 5,
                have: left,
            });
        }
        file.seek(SeekFrom::Start(pos as u64))
            .map_err(|e| open_err(path, &e))?;
        file.read_exact(&mut frame).map_err(|e| open_err(path, &e))?;
        let kind = frame[0];
        let body = u64::from(u32::from_le_bytes([frame[1], frame[2], frame[3], frame[4]]));
        let max = if kind == RECORD_FULL {
            (MAX_FULL_BYTES + 17) as u64
        } else {
            MAX_RECORD_BYTES as u64
        };
        if body > max {
            return Err(LsReplayError::TooLarge {
                what: "a record",
                len: body,
                max,
            });
        }
        if body > left - 5 {
            return Err(LsReplayError::Truncated {
                at: pos as u64,
                what: "a record payload",
                need: body,
                have: left - 5,
            });
        }
        if !matches!(kind, 0x01 | 0x02 | 0x03 | 0x05 | 0x07) {
            return Err(LsReplayError::UnknownRecord { kind, at: pos as u64 });
        }
        aborted |= kind == RECORD_ABORT;
        records += 1;
        pos += 5 + body as usize;
    }
    // det-scan: allow(wall_clock): file dates on disk (list, rotate, import), never sim state
    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
    Ok((header, records, aborted, modified, len))
}

fn info_of(path: &Path) -> Result<ReplayInfo, LsReplayError> {
    let (h, records, aborted, modified, length) = scan(path)?;
    Ok(ReplayInfo {
        path: path.to_path_buf(),
        file_name: name_of(path),
        date: unix_secs(modified),
        length,
        game_id: h.game_id,
        protocol_version: h.protocol_version,
        engine_version: h.engine_version,
        seats: h.seats,
        tick_rate: h.tick_rate,
        seed: h.seed,
        records,
        aborted,
        sha256: None,
    })
}

/// Writes `bytes` to `dir/<a free name like file_name>` through a temporary file and a rename,
/// with modification time `modified`.
fn install(
    dir: &Path,
    file_name: &str,
    bytes: &[u8],
    modified: SystemTime,
) -> Result<PathBuf, LsReplayError> {
    fs::create_dir_all(dir).map_err(|e| LsReplayError::Io(format!("{}: {e}", dir.display())))?;
    let dest = unique_path(dir, file_name);
    let tmp = dir.join(format!(".{}.{}.partial", name_of(&dest), std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        file.set_modified(modified)?;
        Ok(())
    })()
    .and_then(|()| fs::rename(&tmp, &dest));
    if let Err(err) = written {
        let _ = fs::remove_file(&tmp);
        return Err(LsReplayError::Io(format!(
            "cannot write {}: {err}",
            dest.display()
        )));
    }
    Ok(dest)
}

/// `dir/file_name`, or `dir/<stem>-N.lsreplay` for the first free N from 2.
fn unique_path(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("replay");
    (2..10_000u32)
        .map(|n| dir.join(format!("{stem}-{n}.{LSREPLAY_EXTENSION}")))
        .find(|p| !p.exists())
        .unwrap_or_else(|| dir.join(format!("{stem}-{}.{LSREPLAY_EXTENSION}", std::process::id())))
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish_hex())
}

/// Reads and decodes every record of `path`: the bytes and their modification time.
fn load_validated(path: &Path) -> Result<(Vec<u8>, SystemTime), LsReplayError> {
    if !is_replay(path) {
        return Err(LsReplayError::NotLsReplay(format!(
            "{} does not end in .{LSREPLAY_EXTENSION}",
            name_of(path)
        )));
    }
    let bytes = read_bounded(path)?;
    let modified = fs::metadata(path)
        .and_then(|m| m.modified())
        // det-scan: allow(wall_clock): file dates on disk (list, rotate, import), never sim state
        .unwrap_or(UNIX_EPOCH);
    LsReplayReader::new(&bytes)?.read_all()?;
    Ok((bytes, modified))
}

impl ReplayLibrary {
    /// The library in `dir`, keeping [`DEFAULT_KEEP_NEWEST`].
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            keep: DEFAULT_KEEP_NEWEST,
        }
    }

    /// The library keeping the newest `keep` (at least 1).
    pub fn keep_newest(mut self, keep: usize) -> Self {
        self.keep = keep.max(1);
        self
    }

    /// A free path for a new recording named `stem` (the writer creates it).
    pub fn new_recording_path(&self, stem: &str) -> PathBuf {
        let stem: String = stem
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        unique_path(&self.dir, &format!("{stem}.{LSREPLAY_EXTENSION}"))
    }

    /// Lists the folder (a missing folder is empty): readable replays newest first, damaged
    /// files apart.
    pub fn list(&self) -> Result<ReplayList, LsReplayError> {
        let _span = tracing::info_span!("regia_net::lsreplay_list").entered();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(ReplayList::default()),
            Err(err) => return Err(LsReplayError::Io(format!("{}: {err}", self.dir.display()))),
        };
        let mut list = ReplayList::default();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || !is_replay(&path) {
                continue;
            }
            match info_of(&path) {
                Ok(info) => list.replays.push(info),
                Err(err) => list.damaged.push(DamagedReplay {
                    file_name: name_of(&path),
                    path,
                    code: err.code().to_owned(),
                    reason: err.to_string(),
                }),
            }
        }
        list.replays
            .sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.file_name.cmp(&a.file_name)));
        list.damaged.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        Ok(list)
    }

    /// Deletes the readable replays older than the newest [`Self::keep`], except those holding
    /// an abort record. Damaged files are left alone.
    pub fn rotate(&self) -> Result<RotateReport, LsReplayError> {
        let list = self.list()?;
        let mut report = RotateReport::default();
        let mut old: Vec<&ReplayInfo> = list.replays.iter().skip(self.keep).collect();
        old.reverse();
        for info in old {
            if info.aborted {
                report.kept_aborted += 1;
                continue;
            }
            match fs::remove_file(&info.path) {
                Ok(()) => report.deleted.push(info.path.clone()),
                Err(err) => report.failed.push((info.path.clone(), err.to_string())),
            }
        }
        Ok(report)
    }

    /// Copies `source` into the library after decoding every record. Bytes the library already
    /// holds (same SHA-256, under any name) are refused with `replay_duplicate`.
    pub fn import(&self, source: &Path) -> Result<ReplayInfo, LsReplayError> {
        let (bytes, modified) = load_validated(source)?;
        let digest = sha256_hex(&bytes);
        if let Some(existing) = self.find(&digest, bytes.len() as u64) {
            return Err(LsReplayError::Duplicate {
                existing: name_of(&existing),
            });
        }
        let dest = install(&self.dir, &name_of(source), &bytes, modified)?;
        let mut info = info_of(&dest)?;
        info.sha256 = Some(digest);
        Ok(info)
    }

    /// Writes the library's `file_name` into `dest_dir` with a [`META_FORMAT`] sidecar
    /// (`<stem>.json`). An identical copy already there is reused.
    pub fn export(&self, file_name: &str, dest_dir: &Path) -> Result<ExportedReplay, LsReplayError> {
        let source = self.dir.join(file_name);
        if Path::new(file_name).components().count() != 1 {
            return Err(LsReplayError::NotFound(format!(
                "{file_name} (a file name in the library)"
            )));
        }
        let (bytes, modified) = load_validated(&source)?;
        let digest = sha256_hex(&bytes);
        let other = ReplayLibrary::new(dest_dir);
        let dest = match other.find(&digest, bytes.len() as u64) {
            Some(existing) => existing,
            None => install(dest_dir, file_name, &bytes, modified)?,
        };
        let mut info = info_of(&dest)?;
        info.sha256 = Some(digest.clone());
        let stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("replay");
        let sidecar_path = dest.with_file_name(format!("{stem}.json"));
        let body = sidecar_json(&info, &digest);
        let tmp = sidecar_path.with_extension("json.partial");
        fs::write(&tmp, &body)
            .and_then(|()| fs::rename(&tmp, &sidecar_path))
            .map_err(|e| {
                let _ = fs::remove_file(&tmp);
                LsReplayError::Io(format!("cannot write {}: {e}", sidecar_path.display()))
            })?;
        Ok(ExportedReplay {
            replay_path: dest,
            sidecar_path,
            info,
        })
    }

    /// Exports every readable replay; damaged ones are returned apart.
    pub fn export_all(
        &self,
        dest_dir: &Path,
    ) -> Result<(Vec<ExportedReplay>, Vec<DamagedReplay>), LsReplayError> {
        let list = self.list()?;
        let mut refused = list.damaged;
        let mut exported = Vec::new();
        for info in &list.replays {
            match self.export(&info.file_name, dest_dir) {
                Ok(e) => exported.push(e),
                Err(err) => refused.push(DamagedReplay {
                    path: info.path.clone(),
                    file_name: info.file_name.clone(),
                    code: err.code().to_owned(),
                    reason: err.to_string(),
                }),
            }
        }
        Ok((exported, refused))
    }

    /// A replay in the folder with `digest` (only files of `len` bytes are hashed).
    fn find(&self, digest: &str, len: u64) -> Option<PathBuf> {
        let entries = fs::read_dir(&self.dir).ok()?;
        entries.flatten().map(|e| e.path()).find(|path| {
            path.is_file()
                && is_replay(path)
                && fs::metadata(path).is_ok_and(|m| m.len() == len)
                && sha256_file(path).is_ok_and(|d| d == digest)
        })
    }
}
