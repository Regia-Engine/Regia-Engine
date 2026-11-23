// SPDX-License-Identifier: MPL-2.0
//! Deterministic ustar archives (IEEE Std 1003.1). Members are sorted by the caller.
//! mtime, uid and gid are 0. Modes are 0644 for files, 0755 when the caller sets the exec
//! bit, and 0777 for links. Names that do not fit the 100/155 ustar split use a POSIX `pax`
//! extended header. Two zero blocks end the archive.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use crate::ChunkError;
use crate::scan::{Node, NodeKind, Unit};

/// One member read back from an archive. Pax headers are applied, not returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub name: String,
    pub kind: MemberKind,
    pub size: u64,
    pub link: Option<String>,
    /// SHA-256 of a regular file's payload. Links have none.
    pub file_sha256: Option<String>,
}

/// What a [`Member`] stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberKind {
    /// Regular file bytes.
    File,
    /// A hard link at `link`.
    Hardlink,
    /// A symbolic link at `link`.
    Symlink,
}

/// Writes `units` to `path`. The stored node is a file or a symlink; extra hard links follow it.
///
/// # Errors
///
/// `io_error`, `path_too_long`, or `tar_invalid` when a path cannot be encoded.
pub(crate) fn write_units(path: &Path, units: &[Unit]) -> Result<(), ChunkError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| ChunkError::io(parent, &err))?;
    }
    let file = File::create(path).map_err(|err| ChunkError::io(path, &err))?;
    let mut out = std::io::BufWriter::new(file);
    for unit in units {
        write_node(&mut out, &unit.stored, None)?;
        for link in &unit.links {
            write_node(&mut out, link, Some(unit.stored.rel.as_str()))?;
        }
    }
    out.write_all(&[0u8; 1024])
        .map_err(|err| ChunkError::io(path, &err))?;
    out.flush().map_err(|err| ChunkError::io(path, &err))?;
    Ok(())
}

/// Reads members until the end blocks.
///
/// # Errors
///
/// `io_error` or `tar_invalid`.
pub(crate) fn read_members(path: &Path) -> Result<Vec<Member>, ChunkError> {
    let mut file = File::open(path).map_err(|err| ChunkError::io(path, &err))?;
    let mut members = Vec::new();
    let mut pending_path: Option<String> = None;
    let mut pending_link: Option<String> = None;
    let mut zero_blocks = 0u32;
    loop {
        let mut header = [0u8; 512];
        read_exact_or_eof(&mut file, &mut header, path)?;
        if header.iter().all(|byte| *byte == 0) {
            zero_blocks += 1;
            if zero_blocks >= 2 {
                break;
            }
            continue;
        }
        if zero_blocks > 0 {
            return Err(ChunkError::new(
                "tar_invalid",
                format!("{}: data after the end block", path.display()),
            ));
        }
        check_sum(&header)
            .map_err(|err| ChunkError::new("tar_invalid", format!("{}: {err}", path.display())))?;
        let typeflag = header[156];
        let size = parse_octal(&header[124..136])
            .map_err(|_| ChunkError::new("tar_invalid", format!("{}: bad size field", path.display())))?;
        let name = header_name(&header)?;
        let linkname = field_text(&header[157..257]);
        let padded = size.div_ceil(512) * 512;
        let mut body = vec![
            0u8;
            usize::try_from(padded).map_err(|_| {
                ChunkError::new("tar_invalid", "member is larger than this process can read")
            })?
        ];
        if padded > 0 {
            file.read_exact(&mut body)
                .map_err(|err| ChunkError::io(path, &err))?;
        }
        let payload = body
            .get(..usize::try_from(size).unwrap_or(body.len()))
            .unwrap_or(&body);
        if typeflag == b'x' || typeflag == b'g' {
            let text = std::str::from_utf8(payload).map_err(|_| {
                ChunkError::new(
                    "tar_invalid",
                    format!("{}: pax header is not UTF-8", path.display()),
                )
            })?;
            let records = parse_pax(text)?;
            if let Some(value) = records.get("path") {
                pending_path = Some(value.clone());
            }
            if let Some(value) = records.get("linkpath") {
                pending_link = Some(value.clone());
            }
            continue;
        }
        let member_name = pending_path.take().unwrap_or(name);
        let link = pending_link
            .take()
            .or(if linkname.is_empty() { None } else { Some(linkname) });
        let kind = match typeflag {
            b'0' | b'\0' => MemberKind::File,
            b'1' => MemberKind::Hardlink,
            b'2' => MemberKind::Symlink,
            other => {
                return Err(ChunkError::new(
                    "tar_invalid",
                    format!("{}: unsupported type flag {}", path.display(), other as char),
                ));
            }
        };
        let file_sha256 = (kind == MemberKind::File).then(|| crate::sha256_hex(payload));
        members.push(Member {
            name: member_name,
            kind,
            size: if kind == MemberKind::File { size } else { 0 },
            link,
            file_sha256,
        });
    }
    Ok(members)
}

fn write_node(out: &mut impl Write, node: &Node, hard_target: Option<&str>) -> Result<(), ChunkError> {
    let (typeflag, linkname, size) = if let Some(target) = hard_target {
        (b'1', Some(target.to_string()), 0)
    } else {
        match node.kind {
            NodeKind::File => (b'0', None, node.size),
            NodeKind::Symlink => (b'2', Some(node.link_target.clone().unwrap_or_default()), 0),
        }
    };
    let mut pax = String::new();
    let (name, prefix) = match ustar_split(&node.rel) {
        Some(pair) => pair,
        None => {
            pax.push_str(&pax_record("path", &node.rel));
            (short_fallback(&node.rel), String::new())
        }
    };
    if let Some(target) = &linkname
        && target.len() > 100
    {
        pax.push_str(&pax_record("linkpath", target));
    }
    if !pax.is_empty() {
        let mut header = blank_header();
        place(&mut header, 0, b"./PaxHeader");
        write_octal(&mut header[100..108], 0o644);
        let pax_len = u64::try_from(pax.len())
            .map_err(|_| ChunkError::new("tar_invalid", "pax header is too large to store"))?;
        write_octal(&mut header[124..136], pax_len);
        header[156] = b'x';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        write_checksum(&mut header);
        out.write_all(&header)
            .map_err(|err| ChunkError::new("io_error", err.to_string()))?;
        out.write_all(pax.as_bytes())
            .map_err(|err| ChunkError::new("io_error", err.to_string()))?;
        let pad = pax.len().div_ceil(512) * 512 - pax.len();
        out.write_all(&vec![0u8; pad])
            .map_err(|err| ChunkError::new("io_error", err.to_string()))?;
    }
    let mut header = blank_header();
    place(&mut header, 0, name.as_bytes());
    place(&mut header, 345, prefix.as_bytes());
    write_octal(&mut header[100..108], u64::from(node.mode));
    write_octal(&mut header[124..136], size);
    header[156] = typeflag;
    if let Some(target) = &linkname
        && target.len() <= 100
    {
        place(&mut header, 157, target.as_bytes());
    }
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    write_checksum(&mut header);
    out.write_all(&header)
        .map_err(|err| ChunkError::new("io_error", err.to_string()))?;
    if typeflag == b'0' {
        let mut input = File::open(&node.abs).map_err(|err| ChunkError::io(&node.abs, &err))?;
        let copied = std::io::copy(&mut input, out).map_err(|err| ChunkError::io(&node.abs, &err))?;
        if copied != size {
            return Err(ChunkError::new(
                "io_error",
                format!("{} changed size while it was packed", node.abs.display()),
            ));
        }
        let pad = usize::try_from(size.div_ceil(512) * 512 - size).unwrap_or(0);
        out.write_all(&vec![0u8; pad])
            .map_err(|err| ChunkError::new("io_error", err.to_string()))?;
    }
    Ok(())
}

fn blank_header() -> [u8; 512] {
    [0u8; 512]
}

fn place(header: &mut [u8; 512], at: usize, bytes: &[u8]) {
    let end = at + bytes.len();
    if end <= header.len() {
        header[at..end].copy_from_slice(bytes);
    }
}

fn write_octal(slot: &mut [u8], value: u64) {
    let width = slot.len() - 1;
    let text = format!("{value:0width$o}");
    let bytes = text.as_bytes();
    let n = bytes.len().min(width);
    slot[..n].copy_from_slice(&bytes[..n]);
}

fn write_checksum(header: &mut [u8; 512]) {
    header[148..156].fill(b' ');
    let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    let text = format!("{sum:06o}");
    header[148..154].copy_from_slice(text.as_bytes());
    header[154] = 0;
    header[155] = b' ';
}

fn check_sum(header: &[u8; 512]) -> Result<(), String> {
    let stored = parse_octal(&header[148..156]).map_err(|_| "bad checksum field".to_string())?;
    let mut copy = *header;
    copy[148..156].fill(b' ');
    let sum: u32 = copy.iter().map(|byte| u32::from(*byte)).sum();
    if u64::from(sum) == stored {
        Ok(())
    } else {
        Err(format!("checksum {stored} does not match {sum}"))
    }
}

fn parse_octal(bytes: &[u8]) -> Result<u64, ()> {
    let mut value = 0u64;
    let mut saw = false;
    for byte in bytes {
        match byte {
            b' ' | 0 if !saw => {}
            b'0'..=b'7' => {
                saw = true;
                value = value
                    .checked_mul(8)
                    .and_then(|v| v.checked_add(u64::from(byte - b'0')))
                    .ok_or(())?;
            }
            b' ' | 0 => break,
            _ => return Err(()),
        }
    }
    Ok(value)
}

fn field_text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn header_name(header: &[u8; 512]) -> Result<String, ChunkError> {
    let name = field_text(&header[0..100]);
    let prefix = field_text(&header[345..500]);
    let joined = if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    };
    if joined.is_empty() {
        Err(ChunkError::new("tar_invalid", "member name is empty"))
    } else {
        Ok(joined)
    }
}

fn ustar_split(rel: &str) -> Option<(String, String)> {
    if rel.len() <= 100 {
        return Some((rel.to_string(), String::new()));
    }
    let bytes = rel.as_bytes();
    let mut found = None;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'/' {
            continue;
        }
        let prefix = &rel[..index];
        let name = &rel[index + 1..];
        if !prefix.is_empty() && prefix.len() <= 155 && !name.is_empty() && name.len() <= 100 {
            found = Some((name.to_string(), prefix.to_string()));
        }
    }
    found
}

fn short_fallback(rel: &str) -> String {
    let tail = rel.rsplit('/').next().unwrap_or(rel);
    let bytes = tail.as_bytes();
    if bytes.len() <= 100 {
        tail.to_string()
    } else {
        String::from_utf8_lossy(&bytes[bytes.len() - 100..]).into_owned()
    }
}

fn pax_record(key: &str, value: &str) -> String {
    let mut len = 1usize;
    let record = loop {
        let candidate = format!("{len} {key}={value}\n");
        if candidate.len() == len || len > candidate.len() + 8 {
            break candidate;
        }
        len = candidate.len();
    };
    // The length counts its own digits, so grow once more when the digit count changes.
    let again = format!("{} {key}={value}\n", record.len());
    if again.len() == record.len() {
        record
    } else {
        format!("{} {key}={value}\n", again.len())
    }
}

fn parse_pax(text: &str) -> Result<std::collections::BTreeMap<String, String>, ChunkError> {
    let bytes = text.as_bytes();
    let mut at = 0usize;
    let mut map = std::collections::BTreeMap::new();
    while at < bytes.len() {
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        if at == start || bytes.get(at) != Some(&b' ') {
            return Err(ChunkError::new("tar_invalid", "pax record length is missing"));
        }
        let len: usize = std::str::from_utf8(&bytes[start..at])
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| ChunkError::new("tar_invalid", "pax record length is not a number"))?;
        if len == 0 || start + len > bytes.len() {
            return Err(ChunkError::new(
                "tar_invalid",
                "pax record length runs past the header",
            ));
        }
        let record = std::str::from_utf8(&bytes[start..start + len])
            .map_err(|_| ChunkError::new("tar_invalid", "pax record is not UTF-8"))?;
        let body = record
            .get(at - start + 1..)
            .ok_or_else(|| ChunkError::new("tar_invalid", "pax record is truncated"))?;
        let body = body.strip_suffix('\n').unwrap_or(body);
        let (key, value) = body
            .split_once('=')
            .ok_or_else(|| ChunkError::new("tar_invalid", "pax record has no '='"))?;
        map.insert(key.to_string(), value.to_string());
        at = start + len;
    }
    Ok(map)
}

fn read_exact_or_eof(file: &mut File, header: &mut [u8; 512], path: &Path) -> Result<(), ChunkError> {
    let mut filled = 0usize;
    while filled < header.len() {
        match file.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => {
                header.fill(0);
                return Ok(());
            }
            Ok(0) => {
                return Err(ChunkError::new(
                    "tar_invalid",
                    format!("{}: truncated header", path.display()),
                ));
            }
            Ok(n) => filled += n,
            Err(err) => return Err(ChunkError::io(path, &err)),
        }
    }
    Ok(())
}
