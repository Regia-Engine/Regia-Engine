// SPDX-License-Identifier: MPL-2.0
//! The `.rfdrec` recording format v1 (`specs/60` § 4.11, `docs/formats/rfdrec.md`).
//!
//! A recording is the exact frames a session delivered, each with the simulation tick of its
//! arrival, so a replay is deterministic. Layout, little-endian: `"RFDR"`, `u32 format_version`,
//! `u32 protocol_version`, string `client_name`, string `dcc`, `u64 record_count`, then per record
//! `u64 sim_tick`, `u32 frame_len` and the complete wire frame, header included.

use std::path::Path;

use super::wire::{FrameHeader, HEADER_BYTES, PROTOCOL_VERSION};

/// Magic at the start of a recording.
pub const MAGIC: [u8; 4] = *b"RFDR";
/// The recording format version this module writes and reads.
pub const FORMAT_VERSION: u32 = 1;
/// Largest single recorded frame the reader accepts.
const MAX_FRAME_BYTES: u32 = (64 << 20) + HEADER_BYTES as u32;

/// Why a recording could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordingError {
    /// The bytes are not a valid recording (`replay_corrupt`).
    #[error("replay_corrupt: {0}")]
    Corrupt(String),
    /// A newer `format_version` than this build reads (`replay_corrupt` with the version).
    #[error("replay_corrupt: format_version {0} is newer than this build reads ({FORMAT_VERSION})")]
    TooNew(u32),
    /// A file system error (`path_denied` or `replay_not_found` by the caller).
    #[error("io: {0}")]
    Io(String),
}

impl RecordingError {
    /// The agent error code for this failure.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Corrupt(_) | Self::TooNew(_) => "replay_corrupt",
            Self::Io(_) => "replay_not_found",
        }
    }
}

/// One recorded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Simulation tick at which the frame arrived.
    pub sim_tick: u64,
    /// The complete wire frame, header included.
    pub frame: Vec<u8>,
}

impl Record {
    /// The frame's header, or `None` for a frame shorter than a header.
    #[must_use]
    pub fn header(&self) -> Option<FrameHeader> {
        let head: &[u8; HEADER_BYTES] = self.frame.get(..HEADER_BYTES)?.try_into().ok()?;
        Some(FrameHeader::parse(head))
    }

    /// The payload bytes after the header.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        self.frame.get(HEADER_BYTES..).unwrap_or(&[])
    }
}

/// A recording in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    /// Protocol version of the recorded frames.
    pub protocol_version: u32,
    /// Client name of the recorded session.
    pub client_name: String,
    /// The recorded session's `dcc` text.
    pub dcc: String,
    /// Records in arrival order.
    pub records: Vec<Record>,
}

impl Recording {
    /// An empty recording for `client_name` / `dcc`.
    #[must_use]
    pub fn new(client_name: impl Into<String>, dcc: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            client_name: client_name.into(),
            dcc: dcc.into(),
            records: Vec::new(),
        }
    }

    /// Appends one frame received at `sim_tick`.
    pub fn push(&mut self, sim_tick: u64, frame: Vec<u8>) {
        self.records.push(Record { sim_tick, frame });
    }

    /// Serializes the recording.
    ///
    /// # Errors
    ///
    /// `Corrupt` when a string or frame does not fit its length field.
    pub fn to_bytes(&self) -> Result<Vec<u8>, RecordingError> {
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.protocol_version.to_le_bytes());
        for s in [&self.client_name, &self.dcc] {
            let n = u16::try_from(s.len()).map_err(|_| RecordingError::Corrupt("string too long".into()))?;
            out.extend_from_slice(&n.to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        out.extend_from_slice(&(self.records.len() as u64).to_le_bytes());
        for r in &self.records {
            let n =
                u32::try_from(r.frame.len()).map_err(|_| RecordingError::Corrupt("frame too long".into()))?;
            out.extend_from_slice(&r.sim_tick.to_le_bytes());
            out.extend_from_slice(&n.to_le_bytes());
            out.extend_from_slice(&r.frame);
        }
        Ok(out)
    }

    /// Parses a recording. Total: no input panics, and nothing is allocated beyond the bytes
    /// present.
    ///
    /// # Errors
    ///
    /// `Corrupt` for bad magic, truncation, a bad frame length or trailing bytes; `TooNew` for a
    /// newer `format_version`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RecordingError> {
        struct Cursor<'a> {
            bytes: &'a [u8],
            pos: usize,
        }
        impl<'a> Cursor<'a> {
            fn take(&mut self, n: usize) -> Result<&'a [u8], RecordingError> {
                let end = self.pos.checked_add(n).filter(|e| *e <= self.bytes.len());
                let end = end.ok_or_else(|| RecordingError::Corrupt("truncated".into()))?;
                let s = &self.bytes[self.pos..end];
                self.pos = end;
                Ok(s)
            }
        }
        let mut c = Cursor { bytes, pos: 0 };
        if c.take(4)? != MAGIC {
            return Err(RecordingError::Corrupt("bad magic".into()));
        }
        let u32_of = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let version = u32_of(c.take(4)?);
        if version > FORMAT_VERSION {
            return Err(RecordingError::TooNew(version));
        }
        if version == 0 {
            return Err(RecordingError::Corrupt("format_version 0".into()));
        }
        let protocol_version = u32_of(c.take(4)?);
        let mut strings = [String::new(), String::new()];
        for s in &mut strings {
            let b = c.take(2)?;
            let n = usize::from(u16::from_le_bytes([b[0], b[1]]));
            *s = std::str::from_utf8(c.take(n)?)
                .map_err(|_| RecordingError::Corrupt("invalid UTF-8".into()))?
                .to_owned();
        }
        let count = {
            let b = c.take(8)?;
            let mut a = [0u8; 8];
            a.copy_from_slice(b);
            u64::from_le_bytes(a)
        };
        // A record is at least 12 bytes + a 12-byte frame header.
        let remaining = bytes.len() - c.pos;
        if count > (remaining / 24) as u64 {
            return Err(RecordingError::Corrupt("record count exceeds file size".into()));
        }
        let mut records = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let b = c.take(8)?;
            let mut a = [0u8; 8];
            a.copy_from_slice(b);
            let sim_tick = u64::from_le_bytes(a);
            let frame_len = u32_of(c.take(4)?);
            if frame_len < HEADER_BYTES as u32 || frame_len > MAX_FRAME_BYTES {
                return Err(RecordingError::Corrupt("bad frame length".into()));
            }
            records.push(Record {
                sim_tick,
                frame: c.take(frame_len as usize)?.to_vec(),
            });
        }
        if c.pos != bytes.len() {
            return Err(RecordingError::Corrupt("trailing bytes".into()));
        }
        let [client_name, dcc] = strings;
        Ok(Self {
            protocol_version,
            client_name,
            dcc,
            records,
        })
    }

    /// Writes the recording to `path` atomically through an owner-only temporary file
    /// ([`crate::endpoint::write_owner_only`]: exclusively created, then renamed), so a
    /// symlink planted at a fixed temp name is replaced, not followed.
    ///
    /// # Errors
    ///
    /// `Io` on any file system failure, including a `path` with no file name.
    pub fn write_file(&self, path: &Path) -> Result<(), RecordingError> {
        let bytes = self.to_bytes()?;
        let Some(name) = path.file_name() else {
            return Err(RecordingError::Io(format!("not a file path: {}", path.display())));
        };
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        // Not the agent-folder writer: a recording goes where the user says, links included.
        crate::endpoint::write_replacing(dir, name, &bytes).map_err(|e| RecordingError::Io(e.to_string()))
    }

    /// Reads a recording from `path`.
    ///
    /// # Errors
    ///
    /// `Io` when the file cannot be read, otherwise as [`Recording::from_bytes`].
    pub fn read_file(path: &Path) -> Result<Self, RecordingError> {
        let bytes = std::fs::read(path).map_err(|e| RecordingError::Io(e.to_string()))?;
        Self::from_bytes(&bytes)
    }

    /// One text line per record for `regia dcc-feed dump`: tick, message type, handle (0 when
    /// none) and frame size.
    #[must_use]
    pub fn dump_lines(&self, limits: &super::wire::Limits) -> Vec<String> {
        self.records
            .iter()
            .map(|r| {
                let (ty, handle) = match r.header() {
                    Some(h) => {
                        let handle = super::wire::decode(h, r.payload(), limits)
                            .ok()
                            .and_then(|m| m.handle())
                            .unwrap_or(0);
                        (format!("{:#06x}", h.msg_type), handle)
                    }
                    None => ("short".to_owned(), 0),
                };
                format!(
                    "tick={} type={ty} handle={handle} size={}",
                    r.sim_tick,
                    r.frame.len()
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_planted_tmp_symlink_is_not_followed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_recording_link");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "precious").expect("victim");
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        // The pre-fix temp name was the fixed sibling `<name>.rfdrec.tmp`, opened with
        // create + truncate: a planted symlink there truncated the target.
        std::os::unix::fs::symlink(&victim, dir.join("take.rfdrec.tmp")).expect("plant link");
        let recording = Recording {
            protocol_version: PROTOCOL_VERSION,
            client_name: "addon".to_owned(),
            dcc: "test".to_owned(),
            records: Vec::new(),
        };
        let path = dir.join("take.rfdrec");
        recording.write_file(&path).expect("write");
        assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "precious");
        assert_eq!(
            std::fs::metadata(&victim)
                .expect("victim meta")
                .permissions()
                .mode()
                & 0o777,
            0o644,
            "the write replaced the planted link instead of chmodding its target"
        );
        assert!(
            !std::fs::symlink_metadata(&path)
                .expect("meta")
                .file_type()
                .is_symlink(),
            "the recording landed as a regular file, not a link"
        );
        assert_eq!(Recording::read_file(&path).expect("read back"), recording);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// corr-agent (ADR-5597): the recording writer took on the agent folder's link check, so a
    /// recording into a symlinked folder (a project opened through a link, a linked take folder)
    /// failed with "the agent folder must not be a symbolic link".
    #[cfg(unix)]
    #[test]
    fn a_recording_into_a_linked_folder_writes() {
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_recording_linked_dir");
        let real = dir.join("real");
        std::fs::create_dir(&real).expect("real dir");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("dir link");
        let recording = Recording {
            protocol_version: PROTOCOL_VERSION,
            client_name: "addon".to_owned(),
            dcc: "test".to_owned(),
            records: Vec::new(),
        };
        recording
            .write_file(&link.join("take.rfdrec"))
            .expect("write through the folder link");
        assert_eq!(
            Recording::read_file(&real.join("take.rfdrec")).expect("read back"),
            recording
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_file_name_still_writes() {
        use std::os::unix::ffi::OsStringExt;
        let dir = regia_test_utils::unique_temp_dir("dcc_feed_recording_non_utf8");
        let name = std::ffi::OsString::from_vec(b"take-\xff.rfdrec".to_vec());
        let path = dir.join(&name);
        let recording = Recording {
            protocol_version: PROTOCOL_VERSION,
            client_name: "addon".to_owned(),
            dcc: "test".to_owned(),
            records: Vec::new(),
        };
        if cfg!(target_os = "macos") {
            // APFS stores only UTF-8 names and refuses this one (EILSEQ, "Illegal byte sequence").
            // The writer answers that as an error: no panic, and no file under another name.
            let refused = recording.write_file(&path);
            assert!(matches!(refused, Err(RecordingError::Io(_))), "{refused:?}");
            let left = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
            assert_eq!(left, 0, "nothing written, no temp file left");
        } else {
            recording.write_file(&path).expect("write");
            assert_eq!(Recording::read_file(&path).expect("read back"), recording);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
