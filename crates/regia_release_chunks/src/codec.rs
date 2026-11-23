// SPDX-License-Identifier: MPL-2.0
//! Compression of a finished tar. Shipping chunks use the `zstd` program (RFC 8878).
//! [`Identity`] copies bytes and exists so tests can force a size without a compressor.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use crate::ChunkError;

/// Compresses and decompresses one chunk file.
pub trait ChunkCodec {
    /// Short name recorded in errors (`identity`, `zstd`).
    fn name(&self) -> &'static str;

    /// File suffix of a written chunk (`tar` or `tar.zst`).
    fn extension(&self) -> &'static str;

    /// Writes the compressed form of `input` to `output`.
    ///
    /// # Errors
    ///
    /// `io_error`, `zstd_missing` or `zstd_failed`.
    fn compress_file(&self, input: &Path, output: &Path) -> Result<(), ChunkError>;

    /// Writes the uncompressed form of `input` to `output`.
    ///
    /// # Errors
    ///
    /// `io_error`, `zstd_missing` or `zstd_failed`.
    fn decompress_file(&self, input: &Path, output: &Path) -> Result<(), ChunkError>;
}

/// Copies the tar unchanged. Not a shipping chunk format: tests use it so a cap can be
/// checked without the `zstd` program. The file suffix is `tar`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Identity;

impl ChunkCodec for Identity {
    fn name(&self) -> &'static str {
        "identity"
    }

    fn extension(&self) -> &'static str {
        "tar"
    }

    fn compress_file(&self, input: &Path, output: &Path) -> Result<(), ChunkError> {
        copy_file(input, output)
    }

    fn decompress_file(&self, input: &Path, output: &Path) -> Result<(), ChunkError> {
        copy_file(input, output)
    }
}

/// The `zstd` program on `PATH`. Level is 1..=19 (positive only). Thread count is at least 1.
/// `long` passes `--long=27` when compressing.
#[derive(Debug, Clone, Copy)]
pub struct ZstdCli {
    level: u8,
    threads: u32,
    long: bool,
}

impl ZstdCli {
    /// A compressor with `level` (1..=19), `threads` (at least 1) and optional `--long=27`.
    ///
    /// # Errors
    ///
    /// `zstd_level_invalid` when the level or the thread count is outside that range.
    pub fn new(level: u8, threads: u32, long: bool) -> Result<Self, ChunkError> {
        if !(1..=19).contains(&level) {
            return Err(ChunkError::new(
                "zstd_level_invalid",
                format!("zstd level {level} is outside 1..=19"),
            ));
        }
        if threads == 0 {
            return Err(ChunkError::new(
                "zstd_level_invalid",
                "zstd thread count 0 is rejected; pass at least 1",
            ));
        }
        Ok(Self { level, threads, long })
    }

    /// Level 3, one thread, no long-window match. The pack command raises the level when asked.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            level: 3,
            threads: 1,
            long: false,
        }
    }
}

impl ChunkCodec for ZstdCli {
    fn name(&self) -> &'static str {
        "zstd"
    }

    fn extension(&self) -> &'static str {
        "tar.zst"
    }

    fn compress_file(&self, input: &Path, output: &Path) -> Result<(), ChunkError> {
        let mut args = vec![
            "-q".to_string(),
            format!("-{}", self.level),
            format!("-T{}", self.threads),
            "-f".to_string(),
        ];
        if self.long {
            args.push("--long=27".to_string());
        }
        args.push("-o".to_string());
        args.push(output.display().to_string());
        args.push(input.display().to_string());
        run_zstd(&args)
    }

    fn decompress_file(&self, input: &Path, output: &Path) -> Result<(), ChunkError> {
        let args = [
            "-q".to_string(),
            "-d".to_string(),
            "-f".to_string(),
            "-o".to_string(),
            output.display().to_string(),
            input.display().to_string(),
        ];
        run_zstd(&args)
    }
}

fn run_zstd(args: &[impl AsRef<std::ffi::OsStr>]) -> Result<(), ChunkError> {
    match Command::new("zstd").args(args).output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.trim();
            Err(ChunkError::new(
                "zstd_failed",
                if detail.is_empty() {
                    format!("zstd exited {}", output.status)
                } else {
                    format!("zstd exited {}: {detail}", output.status)
                },
            ))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(ChunkError::new("zstd_missing", "the zstd program is not on PATH"))
        }
        Err(err) => Err(ChunkError::new(
            "zstd_failed",
            format!("could not run zstd: {err}"),
        )),
    }
}

pub(crate) fn copy_file(from: &Path, to: &Path) -> Result<(), ChunkError> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|err| ChunkError::io(parent, &err))?;
    }
    let mut input = File::open(from).map_err(|err| ChunkError::io(from, &err))?;
    let mut output = File::create(to).map_err(|err| ChunkError::io(to, &err))?;
    std::io::copy(&mut input, &mut output).map_err(|err| ChunkError::io(to, &err))?;
    output.flush().map_err(|err| ChunkError::io(to, &err))?;
    Ok(())
}

pub(crate) fn file_len(path: &Path) -> Result<u64, ChunkError> {
    let meta = std::fs::metadata(path).map_err(|err| ChunkError::io(path, &err))?;
    Ok(meta.len())
}
