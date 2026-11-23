// SPDX-License-Identifier: MPL-2.0
//! Bounded GLB chunk scanning and stable-file reads, independently derived from
//! Khronos glTF 2.0 §4.4. Chunk payloads are skipped with seeks in metadata mode.

use std::fs::{self, File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::{ContentBudget, ContentError};

pub(crate) const BLOCK: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl Stamp {
    fn new(meta: &Metadata) -> Self {
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            created: meta.created().ok(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
            },
        }
    }
}

pub(crate) struct StableFile {
    pub(crate) file: File,
    pub(crate) path: PathBuf,
    stamp: Stamp,
}

impl StableFile {
    pub(crate) fn open(path: &Path, budget: &ContentBudget<'_>) -> Result<Self, ContentError> {
        budget.checkpoint()?;
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(ContentError::Invalid("expected a regular file".into()));
        }
        let file = File::open(path)?;
        let value = Self {
            file,
            path: path.to_owned(),
            stamp: Stamp::new(&metadata),
        };
        value.verify()?;
        Ok(value)
    }
    pub(crate) fn len(&self) -> u64 {
        self.stamp.len
    }
    pub(crate) fn verify(&self) -> Result<(), ContentError> {
        if Stamp::new(&self.file.metadata()?) != self.stamp
            || fs::metadata(&self.path).map(|m| Stamp::new(&m)).ok().as_ref() != Some(&self.stamp)
        {
            return Err(ContentError::Changed(self.path.display().to_string()));
        }
        Ok(())
    }
    pub(crate) fn read(
        &mut self,
        offset: u64,
        length: u64,
        budget: &mut ContentBudget<'_>,
        payload: bool,
    ) -> Result<Vec<u8>, ContentError> {
        self.verify()?;
        let result = read_range(&mut self.file, offset, length, budget, payload);
        // Prefer an explicit changed-file error if a concurrent truncate caused EOF.
        self.verify()?;
        result
    }
}

pub(crate) fn read_range(
    reader: &mut (impl Read + Seek),
    offset: u64,
    length: u64,
    budget: &mut ContentBudget<'_>,
    payload: bool,
) -> Result<Vec<u8>, ContentError> {
    budget.encoded(length)?;
    let allocation = if payload { super::padded(length)? } else { length };
    if payload {
        budget.allocated(allocation)?;
    }
    let allocation = usize::try_from(allocation).map_err(|_| ContentError::Limit("address space"))?;
    let length = usize::try_from(length).map_err(|_| ContentError::Limit("address space"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(allocation)
        .map_err(|_| ContentError::Limit("allocation"))?;
    reader.seek(SeekFrom::Start(offset))?;
    let mut block = [0; BLOCK];
    while bytes.len() < length {
        budget.checkpoint()?;
        let n = BLOCK.min(length - bytes.len());
        reader.read_exact(&mut block[..n])?;
        bytes.extend_from_slice(&block[..n]);
    }
    bytes.resize(allocation, 0);
    Ok(bytes)
}

/// JSON bytes and ranges into an open, identity-checked source.
pub(crate) struct MetadataInput {
    pub(crate) source: StableFile,
    pub(crate) json: Vec<u8>,
    /// Beginning of all non-JSON chunks, including their headers.
    pub(crate) tail: Option<u64>,
    pub(crate) bin: Option<(u64, u64)>,
}

pub(crate) fn metadata(path: &Path, budget: &mut ContentBudget<'_>) -> Result<MetadataInput, ContentError> {
    let mut source = StableFile::open(path, budget)?;
    let length = source.len();
    let scanned = scan(&mut source.file, length, budget);
    source.verify()?;
    let scanned = scanned?;
    Ok(MetadataInput {
        source,
        json: scanned.json,
        tail: scanned.tail,
        bin: scanned.bin,
    })
}

pub(crate) struct Scanned {
    pub(crate) json: Vec<u8>,
    pub(crate) tail: Option<u64>,
    pub(crate) bin: Option<(u64, u64)>,
}

pub(crate) fn scan(
    reader: &mut (impl Read + Seek),
    length: u64,
    budget: &mut ContentBudget<'_>,
) -> Result<Scanned, ContentError> {
    budget.checkpoint()?;
    let mut prefix = [0; 4];
    let n = length.min(4) as usize;
    budget.encoded(n as u64)?;
    reader.read_exact(&mut prefix[..n])?;
    let (json, tail, bin) = if &prefix == b"glTF" {
        let remaining = read_range(reader, 4, 8, budget, false)?;
        let version = word(&remaining[..4]);
        let declared = u64::from(word(&remaining[4..]));
        if version != 2 || declared != length || length < 20 {
            return Err(ContentError::Invalid(
                "invalid GLB version or declared length".into(),
            ));
        }
        let header = read_range(reader, 12, 8, budget, false)?;
        if &header[4..] != b"JSON" {
            return Err(ContentError::Invalid("GLB must start with JSON".into()));
        }
        let json_length = u64::from(word(&header[..4]));
        let end = chunk_end(12, json_length, length)?;
        if json_length > budget.limits().max_json_bytes {
            return Err(ContentError::Limit("max_json_bytes"));
        }
        let json = read_range(reader, 20, json_length, budget, false)?;
        let mut offset = end;
        let mut bin = None;
        while offset < length {
            budget.work(1)?;
            if length - offset < 8 {
                return Err(ContentError::Invalid("truncated GLB chunk header".into()));
            }
            let header = read_range(reader, offset, 8, budget, false)?;
            let chunk_length = u64::from(word(&header[..4]));
            let next = chunk_end(offset, chunk_length, length)?;
            if &header[4..] == b"JSON" {
                return Err(ContentError::Invalid("duplicate GLB JSON chunk".into()));
            }
            if &header[4..] == b"BIN\0" {
                if bin.is_some() || offset != end {
                    return Err(ContentError::Invalid(
                        "GLB BIN must be the second and only BIN chunk".into(),
                    ));
                }
                bin = Some((offset + 8, chunk_length));
            }
            offset = next;
        }
        (json, Some(end), bin)
    } else {
        if length > budget.limits().max_json_bytes {
            return Err(ContentError::Limit("max_json_bytes"));
        }
        (read_range(reader, 0, length, budget, false)?, None, None)
    };
    Ok(Scanned { json, tail, bin })
}

fn word(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}
fn chunk_end(offset: u64, length: u64, total: u64) -> Result<u64, ContentError> {
    if !length.is_multiple_of(4) {
        return Err(ContentError::Invalid("unaligned GLB chunk".into()));
    }
    offset
        .checked_add(8)
        .and_then(|n| n.checked_add(length))
        .filter(|n| *n <= total)
        .ok_or_else(|| ContentError::Invalid("GLB chunk outside file".into()))
}
