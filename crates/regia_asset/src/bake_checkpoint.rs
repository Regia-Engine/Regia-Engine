// SPDX-License-Identifier: MPL-2.0
//! Bake checkpoints (`.bakeck`, ADR-3095): what a long lightmap or probe bake has finished, so a
//! bake that dies (a crash, a driver reset, a killed job) continues from there instead of from
//! zero.
//!
//! The file is a fixed little-endian header and an opaque payload the baker (`regia_pt`) encodes:
//!
//! ```text
//! magic[8] = "RGBKCKPT"  format_version:u32  header_len:u32  kind:u32  stage:u32
//! input_hash:[u8;32]  units_total:u64  units_done:u64  batches_done:u64  work_seconds:f64
//! payload_len:u64  payload_hash:u64  header_hash:u64  | payload[payload_len]
//! ```
//!
//! `input_hash` names the bake (its inputs, settings, seed and baker kind): a checkpoint is only
//! resumed by the same bake ([`BakeCheckpoint::check_input`], `bake_checkpoint_mismatch`).
//! `payload_hash` and `header_hash` are FNV-1a 64 of the payload and of the header before it, so a
//! torn or edited file is refused (`bake_checkpoint_damaged`) and never resumed into a wrong light.
//! The file is written to a temporary name beside it and renamed over it ([`BakeCheckpoint::write_atomic`]),
//! so a crash mid-write leaves the previous checkpoint whole.
//!
//! This crate only knows the container: the editor and the agent read a checkpoint's header
//! ([`read_header`]) to say a resumable bake exists, without linking the path tracer.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Current version of the `.bakeck` layout. v1 is the first; a later version that changes the
/// header or a payload is refused by an older build (`bake_checkpoint_version`) and a checkpoint is
/// never migrated: it is a transient file, the bake starts again.
pub const BAKE_CHECKPOINT_FORMAT_VERSION: u32 = 1;
/// The first eight bytes of a `.bakeck` file.
pub const BAKE_CHECKPOINT_MAGIC: [u8; 8] = *b"RGBKCKPT";
/// The file extension of a checkpoint (without the dot).
pub const BAKE_CHECKPOINT_EXTENSION: &str = "bakeck";
/// Bytes of the v1 header.
pub const BAKE_CHECKPOINT_HEADER_LEN: usize = 112;
/// The largest payload a checkpoint may declare (16 GiB): a guard against a damaged length, far
/// above the largest bake (a 4096² atlas is about 600 MiB of texel sums).
pub const MAX_BAKE_CHECKPOINT_PAYLOAD: u64 = 16 << 30;

/// The agent code of a checkpoint whose bake inputs, settings, seed or baker differ from the bake
/// resuming it.
pub const BAKE_CHECKPOINT_MISMATCH: &str = "bake_checkpoint_mismatch";
/// The agent code of a truncated, edited or otherwise unreadable checkpoint.
pub const BAKE_CHECKPOINT_DAMAGED: &str = "bake_checkpoint_damaged";
/// The agent code of a checkpoint of another format version.
pub const BAKE_CHECKPOINT_VERSION: &str = "bake_checkpoint_version";
/// The agent code of a checkpoint that cannot be read or written (a missing `--resume` file, a
/// full disk).
pub const BAKE_CHECKPOINT_IO: &str = "bake_checkpoint_io";

/// Which baker wrote a checkpoint. Bakers do not share checkpoints: their random streams and sums
/// differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BakeCheckpointKind {
    /// `regia_pt::lightmap::bake_lightmap` (CPU lightmap texels).
    LightmapCpu,
    /// `regia_pt::lightmap::bake_lightmap_gpu_with` (GPU lightmap texels).
    LightmapGpu,
    /// `regia_pt::probe::bake_gi_volume` on the CPU probe tracer.
    GiCpu,
    /// `regia_pt::probe::bake_gi_volume` on the GPU path tracer.
    GiGpu,
}

impl BakeCheckpointKind {
    /// Every kind, in code order.
    pub const ALL: [Self; 4] = [Self::LightmapCpu, Self::LightmapGpu, Self::GiCpu, Self::GiGpu];

    /// `lightmap_cpu`, `lightmap_gpu`, `gi_cpu` or `gi_gpu`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::LightmapCpu => "lightmap_cpu",
            Self::LightmapGpu => "lightmap_gpu",
            Self::GiCpu => "gi_cpu",
            Self::GiGpu => "gi_gpu",
        }
    }

    /// The kind's code in the file (1..=4).
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            Self::LightmapCpu => 1,
            Self::LightmapGpu => 2,
            Self::GiCpu => 3,
            Self::GiGpu => 4,
        }
    }

    /// The kind of a file code.
    #[must_use]
    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.code() == code)
    }

    /// The unit the bake counts (`texels` or `probes`).
    #[must_use]
    pub fn unit(self) -> &'static str {
        match self {
            Self::LightmapCpu | Self::LightmapGpu => "texels",
            Self::GiCpu | Self::GiGpu => "probes",
        }
    }
}

/// A checkpoint error with its agent code ([`BAKE_CHECKPOINT_MISMATCH`], [`BAKE_CHECKPOINT_DAMAGED`],
/// [`BAKE_CHECKPOINT_VERSION`] or [`BAKE_CHECKPOINT_IO`]).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct BakeCheckpointError {
    /// The agent code.
    pub code: &'static str,
    /// What is wrong, for a person.
    pub message: String,
}

impl BakeCheckpointError {
    /// An error with `code`.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn damaged(message: impl Into<String>) -> Self {
        Self::new(BAKE_CHECKPOINT_DAMAGED, message)
    }
}

/// What a checkpoint says about its bake (the fixed header).
#[derive(Clone, Debug, PartialEq)]
pub struct BakeCheckpointHeader {
    /// The baker.
    pub kind: BakeCheckpointKind,
    /// The baker's stage (a probe bake: 0 while relocating, 1 in the main trace; a lightmap bake: 0).
    pub stage: u32,
    /// Hash of the bake's inputs, settings, seed and baker (the baker computes it).
    pub input_hash: [u8; 32],
    /// Work units of the stage (texels or probes).
    pub units_total: u64,
    /// Units finished.
    pub units_done: u64,
    /// Batches finished over every run of the bake.
    pub batches_done: u64,
    /// Seconds spent baking over every run of the bake (not the time between runs): the rate the
    /// estimate of the remaining time divides by.
    pub work_seconds: f64,
}

impl BakeCheckpointHeader {
    /// Units still to bake in the stage.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.units_total.saturating_sub(self.units_done)
    }

    /// `units_done / units_total` in `0..=1` (1 for an empty stage).
    #[must_use]
    pub fn fraction(&self) -> f32 {
        if self.units_total == 0 {
            1.0
        } else {
            (self.units_done.min(self.units_total) as f64 / self.units_total as f64) as f32
        }
    }

    /// Seconds the rest of the stage takes at the rate so far ([`bake_eta_seconds`]).
    #[must_use]
    pub fn eta_seconds(&self) -> Option<f64> {
        bake_eta_seconds(self.units_done, self.units_total, self.work_seconds)
    }
}

/// The seconds `units_total - units_done` more units take at the rate `units_done / work_seconds`,
/// where both count every run of the bake (a resumed bake keeps the units and seconds of the runs
/// before it, so its estimate starts where they left it). `None` before a unit is done or with no
/// time measured.
#[must_use]
pub fn bake_eta_seconds(units_done: u64, units_total: u64, work_seconds: f64) -> Option<f64> {
    if units_done == 0 || !(work_seconds.is_finite() && work_seconds > 0.0) {
        return None;
    }
    let remaining = units_total.saturating_sub(units_done);
    Some(remaining as f64 * work_seconds / units_done as f64)
}

/// A checkpoint: its header and the baker's payload.
#[derive(Clone, Debug, PartialEq)]
pub struct BakeCheckpoint {
    /// What it says about the bake.
    pub header: BakeCheckpointHeader,
    /// The baker's accumulators (opaque here).
    pub payload: Vec<u8>,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64 of `bytes` (the payload and header checks).
#[must_use]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(a)
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

/// The parsed header and the payload length and hash it declares.
fn parse_header(b: &[u8]) -> Result<(BakeCheckpointHeader, u64, u64), BakeCheckpointError> {
    if b.len() < 16 || b[..8] != BAKE_CHECKPOINT_MAGIC {
        return Err(BakeCheckpointError::damaged(
            "not a bake checkpoint (no RGBKCKPT header)",
        ));
    }
    let version = u32_at(b, 8);
    if version != BAKE_CHECKPOINT_FORMAT_VERSION {
        return Err(BakeCheckpointError::new(
            BAKE_CHECKPOINT_VERSION,
            format!(
                "the checkpoint is format version {version}; this build reads version {BAKE_CHECKPOINT_FORMAT_VERSION}: delete it and bake again"
            ),
        ));
    }
    let header_len = u32_at(b, 12) as usize;
    if header_len != BAKE_CHECKPOINT_HEADER_LEN {
        return Err(BakeCheckpointError::damaged(format!(
            "header length {header_len}, expected {BAKE_CHECKPOINT_HEADER_LEN}"
        )));
    }
    if b.len() < header_len {
        return Err(BakeCheckpointError::damaged(format!(
            "truncated: {} bytes, the header alone is {header_len}",
            b.len()
        )));
    }
    let header_hash = u64_at(b, header_len - 8);
    if fnv1a64(&b[..header_len - 8]) != header_hash {
        return Err(BakeCheckpointError::damaged("the header does not match its hash"));
    }
    let kind_code = u32_at(b, 16);
    let kind = BakeCheckpointKind::from_code(kind_code)
        .ok_or_else(|| BakeCheckpointError::damaged(format!("unknown baker kind {kind_code}")))?;
    let mut input_hash = [0u8; 32];
    input_hash.copy_from_slice(&b[24..56]);
    let header = BakeCheckpointHeader {
        kind,
        stage: u32_at(b, 20),
        input_hash,
        units_total: u64_at(b, 56),
        units_done: u64_at(b, 64),
        batches_done: u64_at(b, 72),
        work_seconds: f64::from_bits(u64_at(b, 80)),
    };
    let payload_len = u64_at(b, 88);
    let payload_hash = u64_at(b, 96);
    if header.units_done > header.units_total {
        return Err(BakeCheckpointError::damaged(format!(
            "{} units done of {}",
            header.units_done, header.units_total
        )));
    }
    if !(header.work_seconds.is_finite() && header.work_seconds >= 0.0) {
        return Err(BakeCheckpointError::damaged(
            "work seconds are not a non-negative number",
        ));
    }
    if payload_len > MAX_BAKE_CHECKPOINT_PAYLOAD {
        return Err(BakeCheckpointError::damaged(format!(
            "payload of {payload_len} bytes is above {MAX_BAKE_CHECKPOINT_PAYLOAD}"
        )));
    }
    Ok((header, payload_len, payload_hash))
}

impl BakeCheckpoint {
    /// The file bytes: the header, then the payload.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let h = &self.header;
        let mut b = Vec::with_capacity(BAKE_CHECKPOINT_HEADER_LEN + self.payload.len());
        b.extend_from_slice(&BAKE_CHECKPOINT_MAGIC);
        b.extend_from_slice(&BAKE_CHECKPOINT_FORMAT_VERSION.to_le_bytes());
        b.extend_from_slice(&(BAKE_CHECKPOINT_HEADER_LEN as u32).to_le_bytes());
        b.extend_from_slice(&h.kind.code().to_le_bytes());
        b.extend_from_slice(&h.stage.to_le_bytes());
        b.extend_from_slice(&h.input_hash);
        b.extend_from_slice(&h.units_total.to_le_bytes());
        b.extend_from_slice(&h.units_done.to_le_bytes());
        b.extend_from_slice(&h.batches_done.to_le_bytes());
        b.extend_from_slice(&h.work_seconds.to_bits().to_le_bytes());
        b.extend_from_slice(&(self.payload.len() as u64).to_le_bytes());
        b.extend_from_slice(&fnv1a64(&self.payload).to_le_bytes());
        let header_hash = fnv1a64(&b);
        b.extend_from_slice(&header_hash.to_le_bytes());
        debug_assert_eq!(b.len(), BAKE_CHECKPOINT_HEADER_LEN);
        b.extend_from_slice(&self.payload);
        b
    }

    /// Parses a checkpoint file's bytes.
    ///
    /// # Errors
    ///
    /// [`BAKE_CHECKPOINT_VERSION`] for another format version; [`BAKE_CHECKPOINT_DAMAGED`] for
    /// anything else wrong (magic, lengths, counts, either hash).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BakeCheckpointError> {
        let (header, payload_len, payload_hash) = parse_header(bytes)?;
        let payload = &bytes[BAKE_CHECKPOINT_HEADER_LEN..];
        if payload.len() as u64 != payload_len {
            return Err(BakeCheckpointError::damaged(format!(
                "the payload is {} bytes, the header says {payload_len} (a torn write?)",
                payload.len()
            )));
        }
        if fnv1a64(payload) != payload_hash {
            return Err(BakeCheckpointError::damaged(
                "the payload does not match its hash",
            ));
        }
        Ok(Self {
            header,
            payload: payload.to_vec(),
        })
    }

    /// Reads and checks `path` (the length against the file's size before the payload is read).
    ///
    /// # Errors
    ///
    /// [`BAKE_CHECKPOINT_IO`] when it cannot be read; as [`Self::from_bytes`] otherwise.
    pub fn read(path: &Path) -> Result<Self, BakeCheckpointError> {
        let (_, payload_len, _) = read_header_parts(path)?;
        let mut bytes = Vec::with_capacity(BAKE_CHECKPOINT_HEADER_LEN + payload_len as usize);
        std::fs::File::open(path)
            .and_then(|mut f| f.read_to_end(&mut bytes))
            .map_err(|e| io_error(path, &e))?;
        Self::from_bytes(&bytes)
    }

    /// Writes the checkpoint to `path` atomically: to `<path>.tmp` beside it, flushed, then renamed
    /// over `path`. Returns the bytes written.
    ///
    /// # Errors
    ///
    /// [`BAKE_CHECKPOINT_IO`] when the directory, the temporary file or the rename fails.
    pub fn write_atomic(&self, path: &Path) -> Result<u64, BakeCheckpointError> {
        let bytes = self.to_bytes();
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).map_err(|e| io_error(dir, &e))?;
        }
        let tmp = temp_path(path);
        let written = (|| {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(io_error(path, &e));
        }
        Ok(bytes.len() as u64)
    }

    /// Checks that this checkpoint belongs to the bake of `kind` whose input hash is `input_hash`.
    ///
    /// # Errors
    ///
    /// [`BAKE_CHECKPOINT_MISMATCH`] naming what differs.
    pub fn check_input(
        &self,
        kind: BakeCheckpointKind,
        input_hash: &[u8; 32],
    ) -> Result<(), BakeCheckpointError> {
        if self.header.kind != kind {
            return Err(BakeCheckpointError::new(
                BAKE_CHECKPOINT_MISMATCH,
                format!(
                    "the checkpoint is a {} bake, this is a {} bake: delete it or bake with the same backend",
                    self.header.kind.name(),
                    kind.name()
                ),
            ));
        }
        if &self.header.input_hash != input_hash {
            return Err(BakeCheckpointError::new(
                BAKE_CHECKPOINT_MISMATCH,
                format!(
                    "the checkpoint was written by another bake (input hash {}, this bake {}): the scene, layout, settings or seed changed; delete it to bake from the start",
                    hex(&self.header.input_hash),
                    hex(input_hash)
                ),
            ));
        }
        Ok(())
    }
}

fn io_error(path: &Path, e: &std::io::Error) -> BakeCheckpointError {
    BakeCheckpointError::new(BAKE_CHECKPOINT_IO, format!("{}: {e}", path.display()))
}

/// Lowercase hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The temporary name a checkpoint is written to before the rename (`<path>.tmp`).
#[must_use]
pub fn temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

fn read_header_parts(path: &Path) -> Result<(BakeCheckpointHeader, u64, u64), BakeCheckpointError> {
    let mut file = std::fs::File::open(path).map_err(|e| io_error(path, &e))?;
    let len = file.metadata().map_err(|e| io_error(path, &e))?.len();
    let mut head = Vec::with_capacity(BAKE_CHECKPOINT_HEADER_LEN);
    (&mut file)
        .take(BAKE_CHECKPOINT_HEADER_LEN as u64)
        .read_to_end(&mut head)
        .map_err(|e| io_error(path, &e))?;
    let (header, payload_len, payload_hash) = parse_header(&head)?;
    if len != BAKE_CHECKPOINT_HEADER_LEN as u64 + payload_len {
        return Err(BakeCheckpointError::damaged(format!(
            "the file is {len} bytes, the header says {} (a torn write?)",
            BAKE_CHECKPOINT_HEADER_LEN as u64 + payload_len
        )));
    }
    Ok((header, payload_len, payload_hash))
}

/// Reads only the header of the checkpoint at `path`, and checks the file's length against it (the
/// payload's hash is checked when a bake resumes). What the editor and the agent show.
///
/// # Errors
///
/// As [`BakeCheckpoint::read`], without the payload hash.
pub fn read_header(path: &Path) -> Result<BakeCheckpointHeader, BakeCheckpointError> {
    read_header_parts(path).map(|(h, _, _)| h)
}

/// Where a bake writing `out` keeps its checkpoint by default: `.<file name>.bakeck` beside it (a
/// dot file, so content scans pass it by).
#[must_use]
pub fn default_checkpoint_path(out: &Path) -> PathBuf {
    let name = out
        .file_name()
        .map_or_else(|| "bake".to_string(), |n| n.to_string_lossy().into_owned());
    out.with_file_name(format!(".{name}.{BAKE_CHECKPOINT_EXTENSION}"))
}

/// What a bake writing `out` would find at its checkpoint ([`default_checkpoint_path`]): what
/// `level.export_bake`, `regia bake-* --dry-run` and the editor's Lightmaps panel report.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct BakeCheckpointStatus {
    /// A file is there.
    pub exists: bool,
    /// It reads and could be resumed (its inputs are checked when the bake starts).
    pub resumable: bool,
    /// Where the checkpoint is.
    pub path: String,
    /// The baker (`lightmap_cpu`, `lightmap_gpu`, `gi_cpu`, `gi_gpu`).
    pub kind: Option<&'static str>,
    /// `texels` or `probes`.
    pub unit: Option<&'static str>,
    /// The baker's stage.
    pub stage: Option<u32>,
    /// Units of the stage.
    pub units_total: Option<u64>,
    /// Units finished.
    pub units_done: Option<u64>,
    /// Units still to bake.
    pub remaining: Option<u64>,
    /// `units_done / units_total`.
    pub fraction: Option<f32>,
    /// Batches finished.
    pub batches_done: Option<u64>,
    /// Seconds of baking so far, over every run.
    pub work_seconds: Option<f64>,
    /// Seconds the rest takes at the rate so far.
    pub eta_seconds: Option<f64>,
    /// The error code of a file that cannot be resumed.
    pub code: Option<&'static str>,
    /// What is wrong with it.
    pub error: Option<String>,
}

impl BakeCheckpointStatus {
    /// The status of the checkpoint at `path` itself.
    #[must_use]
    pub fn of_file(path: &Path) -> Self {
        let mut s = Self {
            exists: path.is_file(),
            resumable: false,
            path: path.display().to_string(),
            kind: None,
            unit: None,
            stage: None,
            units_total: None,
            units_done: None,
            remaining: None,
            fraction: None,
            batches_done: None,
            work_seconds: None,
            eta_seconds: None,
            code: None,
            error: None,
        };
        if !s.exists {
            return s;
        }
        match read_header(path) {
            Ok(h) => {
                s.resumable = true;
                s.kind = Some(h.kind.name());
                s.unit = Some(h.kind.unit());
                s.stage = Some(h.stage);
                s.units_total = Some(h.units_total);
                s.units_done = Some(h.units_done);
                s.remaining = Some(h.remaining());
                s.fraction = Some(h.fraction());
                s.batches_done = Some(h.batches_done);
                s.work_seconds = Some(h.work_seconds);
                s.eta_seconds = h.eta_seconds();
            }
            Err(e) => {
                s.code = Some(e.code);
                s.error = Some(e.message);
            }
        }
        s
    }

    /// One line for a panel: `resumable checkpoint: 40 % (60 probes left)`, or why it cannot be
    /// resumed; `None` without a file.
    #[must_use]
    pub fn line(&self) -> Option<String> {
        if !self.exists {
            return None;
        }
        Some(match (self.fraction, self.remaining, self.unit, self.code) {
            (Some(f), Some(r), Some(u), _) => format!(
                "resumable checkpoint: {:.0} % ({r} {u} left); the next bake continues it",
                f * 100.0
            ),
            (_, _, _, Some(code)) => format!("checkpoint not resumable ({code}): the next bake starts over"),
            _ => "checkpoint found".to_string(),
        })
    }
}

/// [`BakeCheckpointStatus::of_file`] of [`default_checkpoint_path`]`(out)`.
#[must_use]
pub fn checkpoint_status(out: &Path) -> BakeCheckpointStatus {
    BakeCheckpointStatus::of_file(&default_checkpoint_path(out))
}

/// The format version of a `.bakeck` file's bytes (`regia upgrade-project`'s probe): `None` for
/// other bytes.
///
/// # Errors
///
/// A file too short to hold the version.
pub fn bake_checkpoint_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    if bytes.len() < 12 {
        return Err("file is too short to hold a bake checkpoint header".into());
    }
    if bytes[..8] != BAKE_CHECKPOINT_MAGIC {
        return Ok(None);
    }
    Ok(Some(u32_at(bytes, 8)))
}

/// The layout fingerprint `cargo xtask check-formats` hashes (a binary format has no serde shape:
/// this text names every field in order and changes with the layout, payloads included).
#[must_use]
pub fn bake_checkpoint_schema_fingerprint() -> String {
    "bake-checkpoint: magic[8]=RGBKCKPT format_version:u32 header_len:u32=112 kind:u32(1 lightmap_cpu 2 \
     lightmap_gpu 3 gi_cpu 4 gi_gpu) stage:u32 input_hash:[u8;32] units_total:u64 units_done:u64 \
     batches_done:u64 work_seconds:f64 payload_len:u64 payload_hash:u64(fnv1a64) header_hash:u64(fnv1a64) | \
     lightmap payload: rays:u64 count:u64 texel*{e:[f32;3] dir:[f32;3] rel_err:f32 back:u32 taken:u32} | \
     gi payload: probes:u64 live:u64 offsets*[f32;3] flags*u32 last_trace*{tag:u8 back_fraction:f32 rays:u32} \
     rounds_done:u32 relocation_done:u8 pending:u64+u32* relocation_traces:u64 relocation_seconds:f64 \
     traced:u64+u32* done:u64 trace*{sh:[f32;48] rays:u32 back_faces:u32 hits:u32 rejected:u32 \
     closest_back:{tag:u8 dir:[f32;3] dist:f32} closest_front:{tag:u8 dir:[f32;3] dist:f32} moments:u32+[f32;2]* \
     clamped:u32 clamp_loss:f32 suppressed_groups:u32 firefly_loss:f32 noise:{tag:u8 value:f32}}"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BakeCheckpoint {
        BakeCheckpoint {
            header: BakeCheckpointHeader {
                kind: BakeCheckpointKind::GiCpu,
                stage: 1,
                input_hash: [7; 32],
                units_total: 100,
                units_done: 40,
                batches_done: 3,
                work_seconds: 8.0,
            },
            payload: vec![1, 2, 3, 4, 5],
        }
    }

    #[test]
    fn a_checkpoint_round_trips_through_bytes_and_a_file() {
        let c = sample();
        let bytes = c.to_bytes();
        assert_eq!(bytes.len(), BAKE_CHECKPOINT_HEADER_LEN + 5);
        assert_eq!(BakeCheckpoint::from_bytes(&bytes).expect("parses"), c);
        assert_eq!(bake_checkpoint_file_version(&bytes), Ok(Some(1)));
        assert_eq!(bake_checkpoint_file_version(b"something else"), Ok(None));
        let dir = regia_test_utils::unique_temp_dir("bake_checkpoint_round_trip");
        let path = dir.join("a").join(".x.flmap.bakeck");
        assert_eq!(c.write_atomic(&path).expect("writes"), bytes.len() as u64);
        assert!(!temp_path(&path).exists(), "the temporary file is renamed away");
        assert_eq!(BakeCheckpoint::read(&path).expect("reads"), c);
        assert_eq!(read_header(&path).expect("header"), c.header);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn damaged_files_are_refused_with_their_codes() {
        let good = sample().to_bytes();
        // Truncated anywhere: damaged, never a panic.
        for cut in [0, 7, 15, 50, BAKE_CHECKPOINT_HEADER_LEN - 1, good.len() - 1] {
            let e = BakeCheckpoint::from_bytes(&good[..cut]).expect_err("truncated");
            assert_eq!(e.code, BAKE_CHECKPOINT_DAMAGED, "cut {cut}: {e}");
        }
        // Another version.
        let mut v = good.clone();
        v[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            BakeCheckpoint::from_bytes(&v).expect_err("version").code,
            BAKE_CHECKPOINT_VERSION
        );
        // A flipped payload byte, a flipped header byte.
        let mut p = good.clone();
        *p.last_mut().expect("payload") ^= 1;
        assert_eq!(
            BakeCheckpoint::from_bytes(&p).expect_err("payload").code,
            BAKE_CHECKPOINT_DAMAGED
        );
        let mut h = good.clone();
        h[60] ^= 1;
        assert_eq!(
            BakeCheckpoint::from_bytes(&h).expect_err("header").code,
            BAKE_CHECKPOINT_DAMAGED
        );
        // Huge counts with a valid header hash: a payload length past the cap and past the file,
        // and more units done than total.
        let mut huge = sample();
        huge.header.units_done = u64::MAX;
        let e = BakeCheckpoint::from_bytes(&huge.to_bytes()).expect_err("units");
        assert_eq!(e.code, BAKE_CHECKPOINT_DAMAGED);
        let mut long = good[..BAKE_CHECKPOINT_HEADER_LEN].to_vec();
        long[88..96].copy_from_slice(&u64::MAX.to_le_bytes());
        let rehash = fnv1a64(&long[..BAKE_CHECKPOINT_HEADER_LEN - 8]);
        long[BAKE_CHECKPOINT_HEADER_LEN - 8..].copy_from_slice(&rehash.to_le_bytes());
        assert_eq!(
            BakeCheckpoint::from_bytes(&long).expect_err("len").code,
            BAKE_CHECKPOINT_DAMAGED
        );
        // A missing file is io.
        let missing = Path::new("/nonexistent/regia/.x.bakeck");
        assert_eq!(
            BakeCheckpoint::read(missing).expect_err("missing").code,
            BAKE_CHECKPOINT_IO
        );
    }

    #[test]
    fn another_bake_is_a_mismatch() {
        let c = sample();
        assert!(c.check_input(BakeCheckpointKind::GiCpu, &[7; 32]).is_ok());
        let e = c
            .check_input(BakeCheckpointKind::GiCpu, &[8; 32])
            .expect_err("hash");
        assert_eq!(e.code, BAKE_CHECKPOINT_MISMATCH);
        let e = c
            .check_input(BakeCheckpointKind::GiGpu, &[7; 32])
            .expect_err("kind");
        assert_eq!(e.code, BAKE_CHECKPOINT_MISMATCH);
    }

    /// The estimate divides by the work of every run: a resumed bake that has done 500 of 1000
    /// units in 50 s of earlier work says 50 s more, however long it was stopped.
    #[test]
    fn the_estimate_counts_every_run() {
        assert_eq!(bake_eta_seconds(500, 1000, 50.0), Some(50.0));
        assert_eq!(bake_eta_seconds(0, 1000, 50.0), None);
        assert_eq!(bake_eta_seconds(10, 1000, 0.0), None);
        let h = sample().header;
        assert_eq!(h.remaining(), 60);
        assert_eq!(h.eta_seconds(), Some(12.0));
    }

    #[test]
    fn the_default_path_is_a_dot_file_beside_the_output() {
        assert_eq!(
            default_checkpoint_path(Path::new("assets/maps/yard.fgiv")),
            Path::new("assets/maps/.yard.fgiv.bakeck")
        );
        let dir = regia_test_utils::unique_temp_dir("bake_checkpoint_status");
        let out = dir.join("yard.fgiv");
        assert!(!checkpoint_status(&out).exists);
        assert_eq!(checkpoint_status(&out).line(), None);
        sample()
            .write_atomic(&default_checkpoint_path(&out))
            .expect("writes");
        let s = checkpoint_status(&out);
        assert!(s.exists && s.resumable);
        assert_eq!((s.units_done, s.remaining), (Some(40), Some(60)));
        assert_eq!(
            s.line().as_deref(),
            Some("resumable checkpoint: 40 % (60 probes left); the next bake continues it")
        );
        std::fs::write(default_checkpoint_path(&out), b"RGBKCKPT").expect("damage");
        let s = checkpoint_status(&out);
        assert!(s.exists && !s.resumable);
        assert_eq!(s.code, Some(BAKE_CHECKPOINT_DAMAGED));
        let _ = std::fs::remove_dir_all(dir);
    }
}
