// SPDX-License-Identifier: MPL-2.0
//! The in-process native crash record (ADR-328): what the Android signal handler writes into a
//! pre-opened file, and what the next launch reads back.
//!
//! Android cannot run the out-of-process monitor the desktop path uses (ADR-158), so the crashing
//! process writes a small fixed-layout record itself, from the signal handler, with `write(2)`
//! only. Everything here that the handler uses works on a caller-provided buffer and allocates
//! nothing: [`encode`], [`walk_frame_pointers`] and [`MapsFilter`]. The rest ([`decode_file`],
//! [`collect_pending`], [`report_text`]) runs on the next launch, where allocation is fine.
//! None of it is Android-specific, so it is built and tested on every target.
//!
//! The file (`<internal>/crash/pending.bin`) is a [`HEADER_SIZE`]-byte header, then up to
//! [`MAPS_MAX`] bytes of the executable lines of `/proc/self/maps` at the moment of the crash
//! (so a return address can be named as `libGLES_mali.so+0x…` on the next launch). All integers
//! are little-endian:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 8 | magic `FERCRASH` |
//! | 8 | 4 | format version ([`RECORD_VERSION`]) |
//! | 12 | 4 | header size ([`HEADER_SIZE`]) |
//! | 16 | 8 | crash time, ms since the Unix epoch |
//! | 24 | 4 | signal |
//! | 28 | 4 | `si_code` |
//! | 32 | 8 | fault address |
//! | 40 | 4 | process id |
//! | 44 | 4 | thread id |
//! | 48 | 16 | thread name (NUL-padded) |
//! | 64 | 32 | PC, LR, SP, FP |
//! | 96 | 4 | frames recorded |
//! | 100 | 4 | why the frame walk ended ([`WalkEnd`]) |
//! | 104 | 256 | [`MAX_FRAMES`] return addresses |
//! | 360 | 24 | engine version |
//! | 384 | 24 | game version |
//! | 408 | 48 | build id (git hash) |
//! | 456 | 16 | build profile |
//! | 472 | 4 | bytes of maps text after the header |
//! | 476 | 32 | reserved (zero) |
//! | 508 | 4 | FNV-1a 32 of bytes 0..508 |
//!
//! A file of zeros is an armed run that did not crash ([`RecordError::Empty`]). The handler writes
//! the maps text first and the header last, so a crash cut short leaves a header that fails the
//! checksum, never a half-written record that reads as valid.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The record's magic bytes.
pub const RECORD_MAGIC: [u8; 8] = *b"FERCRASH";
/// The record format version.
pub const RECORD_VERSION: u32 = 1;
/// Size of the fixed header, bytes.
pub const HEADER_SIZE: usize = 512;
/// Largest maps text kept after the header, bytes.
pub const MAPS_MAX: usize = 64 * 1024;
/// Size of the preallocated record file.
pub const FILE_SIZE: usize = HEADER_SIZE + MAPS_MAX;
/// Return addresses the frame walk records at most.
pub const MAX_FRAMES: usize = 32;
/// The frame walk stops at a frame pointer further than this above the stack pointer.
pub const MAX_STACK_SPAN: u64 = 8 * 1024 * 1024;
/// User-space addresses fit in 48 bits; the bits above hold pointer-authentication codes or
/// memory tags on aarch64 (Pixel 6 and later sign return addresses), which are stripped.
pub const ADDRESS_MASK: u64 = (1 << 48) - 1;
/// The live record's file name in the record folder.
pub const PENDING_FILE: &str = "pending.bin";
/// Suffix of a collected record (`<time>.crash.bin`).
pub const COLLECTED_SUFFIX: &str = ".crash.bin";
/// Collected records kept in the record folder (oldest removed first).
pub const KEEP_RECORDS: usize = 5;
/// A `SIGABRT` this soon after a panic report is the `panic = "abort"` abort, not a new crash.
pub const PANIC_ABORT_WINDOW_MS: u64 = 5_000;

const CHECKSUM_AT: usize = HEADER_SIZE - 4;

/// Why the frame-pointer walk stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkEnd {
    /// No walk ran (no frame pointer, or this architecture is not walked).
    NotWalked,
    /// [`MAX_FRAMES`] frames were recorded.
    Bound,
    /// The next frame pointer was misaligned, outside the stack span, not above the previous one,
    /// or unreadable.
    BadAddress,
    /// The chain ended (a null frame pointer or a zero return address).
    EndOfChain,
}

impl WalkEnd {
    fn to_u32(self) -> u32 {
        match self {
            Self::NotWalked => 0,
            Self::Bound => 1,
            Self::BadAddress => 2,
            Self::EndOfChain => 3,
        }
    }

    fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Bound,
            2 => Self::BadAddress,
            3 => Self::EndOfChain,
            _ => Self::NotWalked,
        }
    }

    /// Lower-case name for reports (`bound`, `bad_address`, `end_of_chain`, `not_walked`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotWalked => "not_walked",
            Self::Bound => "bound",
            Self::BadAddress => "bad_address",
            Self::EndOfChain => "end_of_chain",
        }
    }
}

/// One native crash, as the handler records it. Plain fixed-size data (`Copy`, no heap), so the
/// handler can fill a copy on its stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrashRecord {
    /// Crash time, ms since the Unix epoch.
    pub unix_ms: u64,
    /// The signal number (11 = SIGSEGV…).
    pub signal: u32,
    /// `si_code` (`<= 0`: sent by a process).
    pub code: i32,
    /// The faulting address (0 when the signal carries none).
    pub address: u64,
    /// The crashed process.
    pub pid: u32,
    /// The crashing thread.
    pub tid: u32,
    /// The crashing thread's name, NUL-padded (the kernel's 15-byte `comm`).
    pub thread: [u8; 16],
    /// Program counter (0 where the architecture is not read).
    pub pc: u64,
    /// Link register (aarch64), pointer-authentication bits stripped.
    pub lr: u64,
    /// Stack pointer.
    pub sp: u64,
    /// Frame pointer.
    pub fp: u64,
    /// Return addresses from the frame-pointer walk, innermost first; `frame_count` are valid.
    pub frames: [u64; MAX_FRAMES],
    /// Valid entries of the `frames` array.
    pub frame_count: u32,
    /// Why the walk stopped.
    pub walk_end: WalkEnd,
    /// The engine version (NUL-padded).
    pub engine_version: [u8; 24],
    /// The game version (NUL-padded).
    pub game_version: [u8; 24],
    /// The build id: the git hash (NUL-padded).
    pub build_id: [u8; 48],
    /// The build profile (NUL-padded).
    pub profile: [u8; 16],
    /// Bytes of maps text written after the header.
    pub maps_len: u32,
}

impl CrashRecord {
    /// All zero: the template the handler fills.
    pub const EMPTY: Self = Self {
        unix_ms: 0,
        signal: 0,
        code: 0,
        address: 0,
        pid: 0,
        tid: 0,
        thread: [0; 16],
        pc: 0,
        lr: 0,
        sp: 0,
        fp: 0,
        frames: [0; MAX_FRAMES],
        frame_count: 0,
        walk_end: WalkEnd::NotWalked,
        engine_version: [0; 24],
        game_version: [0; 24],
        build_id: [0; 48],
        profile: [0; 16],
        maps_len: 0,
    };

    /// A template with the build facts filled in (at attach time, never in the handler).
    #[must_use]
    pub fn with_build(build: &crate::crash::BuildInfo) -> Self {
        Self {
            engine_version: fixed(&build.engine),
            game_version: fixed(&build.version),
            build_id: fixed(&build.git_hash),
            profile: fixed(&build.profile),
            ..Self::EMPTY
        }
    }

    /// The recorded return addresses.
    #[must_use]
    pub fn frames(&self) -> &[u64] {
        let n = (self.frame_count as usize).min(MAX_FRAMES);
        &self.frames[..n]
    }

    /// The thread name (empty when unknown).
    #[must_use]
    pub fn thread_name(&self) -> String {
        text(&self.thread)
    }

    /// The build identity the crashed run had (it may differ from this run's after an update).
    #[must_use]
    pub fn build(&self) -> crate::crash::BuildInfo {
        crate::crash::BuildInfo {
            version: text(&self.game_version),
            git_hash: text(&self.build_id),
            profile: text(&self.profile),
            engine: text(&self.engine_version),
        }
    }
}

/// `s` as a NUL-padded fixed field, cut at a character boundary when it is too long.
#[must_use]
pub fn fixed<const N: usize>(s: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let mut end = s.len().min(N);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    out[..end].copy_from_slice(&s.as_bytes()[..end]);
    out
}

/// A NUL-padded field as text (lossy).
fn text(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// FNV-1a, 32 bit (no allocation; the handler checksums the header with it).
#[must_use]
pub fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Little-endian writer over the header buffer at fixed offsets.
struct Put<'a>(&'a mut [u8; HEADER_SIZE]);

impl Put<'_> {
    fn bytes(&mut self, at: usize, bytes: &[u8]) {
        if let Some(dst) = self.0.get_mut(at..at + bytes.len()) {
            dst.copy_from_slice(bytes);
        }
    }

    fn u32(&mut self, at: usize, v: u32) {
        self.bytes(at, &v.to_le_bytes());
    }

    fn u64(&mut self, at: usize, v: u64) {
        self.bytes(at, &v.to_le_bytes());
    }
}

/// Encodes `record` into the header buffer `out`, checksum included. Allocation-free and
/// panic-free: safe in a signal handler.
pub fn encode(record: &CrashRecord, out: &mut [u8; HEADER_SIZE]) {
    out.fill(0);
    let mut put = Put(out);
    put.bytes(0, &RECORD_MAGIC);
    put.u32(8, RECORD_VERSION);
    put.u32(12, HEADER_SIZE as u32);
    put.u64(16, record.unix_ms);
    put.u32(24, record.signal);
    put.u32(28, record.code.cast_unsigned());
    put.u64(32, record.address);
    put.u32(40, record.pid);
    put.u32(44, record.tid);
    put.bytes(48, &record.thread);
    put.u64(64, record.pc);
    put.u64(72, record.lr);
    put.u64(80, record.sp);
    put.u64(88, record.fp);
    put.u32(96, record.frame_count.min(MAX_FRAMES as u32));
    put.u32(100, record.walk_end.to_u32());
    for (i, frame) in record.frames.iter().enumerate() {
        put.u64(104 + 8 * i, *frame);
    }
    put.bytes(360, &record.engine_version);
    put.bytes(384, &record.game_version);
    put.bytes(408, &record.build_id);
    put.bytes(456, &record.profile);
    put.u32(472, record.maps_len);
    let sum = fnv1a32(&put.0[..CHECKSUM_AT]);
    put.u32(CHECKSUM_AT, sum);
}

/// Why a record file was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// All zeros (or no bytes): an armed run that did not crash.
    #[error("no crash recorded")]
    Empty,
    /// Shorter than the header, or than the maps text the header announces.
    #[error("record truncated: {len} bytes, {need} needed")]
    Truncated {
        /// Bytes present.
        len: usize,
        /// Bytes the record needs.
        need: usize,
    },
    /// The first 8 bytes are not [`RECORD_MAGIC`].
    #[error("not a crash record (bad magic)")]
    BadMagic,
    /// A format version this build does not read.
    #[error("unsupported record version {0}")]
    UnsupportedVersion(u32),
    /// The header size field is not [`HEADER_SIZE`].
    #[error("bad record header size {0}")]
    BadHeaderSize(u32),
    /// The header checksum does not match (a write cut short).
    #[error("record checksum mismatch (stored {stored:#010x}, computed {computed:#010x})")]
    Checksum {
        /// The checksum in the file.
        stored: u32,
        /// The checksum of the bytes read.
        computed: u32,
    },
    /// A field holds an impossible value.
    #[error("bad record field {0}")]
    BadField(&'static str),
}

fn le32(b: &[u8], at: usize) -> u32 {
    let mut v = [0u8; 4];
    v.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(v)
}

fn le64(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

fn field<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    let mut v = [0u8; N];
    v.copy_from_slice(&b[at..at + N]);
    v
}

/// Decodes a record header (the first [`HEADER_SIZE`] bytes of `bytes`).
///
/// # Errors
/// A [`RecordError`] naming what is wrong.
pub fn decode(bytes: &[u8]) -> Result<CrashRecord, RecordError> {
    let head = &bytes[..bytes.len().min(HEADER_SIZE)];
    if head.iter().all(|b| *b == 0) {
        return Err(RecordError::Empty);
    }
    if bytes.len() < HEADER_SIZE {
        return Err(RecordError::Truncated {
            len: bytes.len(),
            need: HEADER_SIZE,
        });
    }
    if head[..8] != RECORD_MAGIC {
        return Err(RecordError::BadMagic);
    }
    let version = le32(head, 8);
    if version != RECORD_VERSION {
        return Err(RecordError::UnsupportedVersion(version));
    }
    let size = le32(head, 12);
    if size as usize != HEADER_SIZE {
        return Err(RecordError::BadHeaderSize(size));
    }
    let stored = le32(head, CHECKSUM_AT);
    let computed = fnv1a32(&head[..CHECKSUM_AT]);
    if stored != computed {
        return Err(RecordError::Checksum { stored, computed });
    }
    let frame_count = le32(head, 96);
    if frame_count as usize > MAX_FRAMES {
        return Err(RecordError::BadField("frame_count"));
    }
    let maps_len = le32(head, 472);
    if maps_len as usize > MAPS_MAX {
        return Err(RecordError::BadField("maps_len"));
    }
    let mut frames = [0u64; MAX_FRAMES];
    for (i, frame) in frames.iter_mut().enumerate() {
        *frame = le64(head, 104 + 8 * i);
    }
    Ok(CrashRecord {
        unix_ms: le64(head, 16),
        signal: le32(head, 24),
        code: le32(head, 28).cast_signed(),
        address: le64(head, 32),
        pid: le32(head, 40),
        tid: le32(head, 44),
        thread: field(head, 48),
        pc: le64(head, 64),
        lr: le64(head, 72),
        sp: le64(head, 80),
        fp: le64(head, 88),
        frames,
        frame_count,
        walk_end: WalkEnd::from_u32(le32(head, 100)),
        engine_version: field(head, 360),
        game_version: field(head, 384),
        build_id: field(head, 408),
        profile: field(head, 456),
        maps_len,
    })
}

/// Decodes a whole record file: the header and the maps text after it.
///
/// # Errors
/// A [`RecordError`]; [`RecordError::Truncated`] when the maps text is cut short.
pub fn decode_file(bytes: &[u8]) -> Result<(CrashRecord, String), RecordError> {
    let record = decode(bytes)?;
    let end = HEADER_SIZE + record.maps_len as usize;
    let Some(maps) = bytes.get(HEADER_SIZE..end) else {
        return Err(RecordError::Truncated {
            len: bytes.len(),
            need: end,
        });
    };
    Ok((record, String::from_utf8_lossy(maps).into_owned()))
}

/// Walks the frame-pointer chain from `fp` (AAPCS64 frame records: `[fp]` is the caller's frame
/// pointer, `[fp + 8]` the return address) into `out`, innermost first.
///
/// `read_pair(addr)` returns the two words at `addr`, or `None` when they cannot be read (the
/// handler reads through a pipe, so a bad address is an `EFAULT`, not a second fault). Every
/// frame pointer must be 8-byte aligned, at or above `sp`, at most [`MAX_STACK_SPAN`] above it,
/// and strictly above the previous one (the stack grows down), so a corrupt chain cannot loop.
/// Return addresses are masked with [`ADDRESS_MASK`]. Allocation-free.
pub fn walk_frame_pointers(
    fp: u64,
    sp: u64,
    mut read_pair: impl FnMut(u64) -> Option<[u64; 2]>,
    out: &mut [u64],
) -> (usize, WalkEnd) {
    let mut n = 0;
    let mut fp = fp;
    let mut previous: Option<u64> = None;
    while n < out.len() {
        if fp == 0 {
            return (n, WalkEnd::EndOfChain);
        }
        let in_span = fp >= sp && fp - sp <= MAX_STACK_SPAN;
        let rising = previous.is_none_or(|p| fp > p);
        if !fp.is_multiple_of(8) || !in_span || !rising {
            return (n, WalkEnd::BadAddress);
        }
        let Some([next, ret]) = read_pair(fp) else {
            return (n, WalkEnd::BadAddress);
        };
        let ret = ret & ADDRESS_MASK;
        if ret == 0 {
            return (n, WalkEnd::EndOfChain);
        }
        out[n] = ret;
        n += 1;
        previous = Some(fp);
        fp = next;
    }
    (n, WalkEnd::Bound)
}

/// Longest maps line kept (longer lines are cut, keeping their newline).
pub const MAPS_LINE_MAX: usize = 320;

/// Streams `/proc/self/maps` text and passes on the executable lines (`r-xp` and friends), the
/// only ones a return address can fall in. Fixed-size state, no allocation: the handler feeds it
/// the file in chunks read into a stack buffer.
#[derive(Debug, Clone)]
pub struct MapsFilter {
    line: [u8; MAPS_LINE_MAX],
    len: usize,
}

impl Default for MapsFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl MapsFilter {
    /// An empty filter.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            line: [0; MAPS_LINE_MAX],
            len: 0,
        }
    }

    /// Feeds the next chunk; `emit` gets each complete executable line, newline included.
    pub fn feed(&mut self, chunk: &[u8], mut emit: impl FnMut(&[u8])) {
        for &b in chunk {
            if b == b'\n' {
                self.end_line(&mut emit);
            } else if self.len < MAPS_LINE_MAX - 1 {
                self.line[self.len] = b;
                self.len += 1;
            }
        }
    }

    /// Ends the input: a last line without a newline is passed on too.
    pub fn finish(&mut self, mut emit: impl FnMut(&[u8])) {
        if self.len > 0 {
            self.end_line(&mut emit);
        }
    }

    fn end_line(&mut self, emit: &mut impl FnMut(&[u8])) {
        let len = self.len;
        self.len = 0;
        let line = &self.line[..len];
        // `start-end perms offset dev inode [path]`: the perms field follows the first space.
        let Some(space) = line.iter().position(|b| *b == b' ') else {
            return;
        };
        if line.get(space + 3) != Some(&b'x') {
            return;
        }
        self.line[len] = b'\n';
        emit(&self.line[..=len]);
    }
}

/// One executable mapping from the recorded maps text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    /// First address.
    pub start: u64,
    /// One past the last address.
    pub end: u64,
    /// File offset of `start`.
    pub offset: u64,
    /// The mapped file (empty for an anonymous mapping).
    pub path: String,
}

/// Parses the recorded maps lines (unreadable lines are skipped).
#[must_use]
pub fn parse_maps(text: &str) -> Vec<Mapping> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let (start, end) = parts.next()?.split_once('-')?;
            let _perms = parts.next()?;
            let offset = parts.next()?;
            let _dev = parts.next()?;
            let _inode = parts.next()?;
            let path = parts.collect::<Vec<_>>().join(" ");
            Some(Mapping {
                start: u64::from_str_radix(start, 16).ok()?,
                end: u64::from_str_radix(end, 16).ok()?,
                offset: u64::from_str_radix(offset, 16).ok()?,
                path,
            })
        })
        .collect()
}

/// `0x… path+0x…` (the file offset in the mapped library), or `0x…` when no mapping holds it.
#[must_use]
pub fn describe_address(address: u64, maps: &[Mapping]) -> String {
    match maps.iter().find(|m| (m.start..m.end).contains(&address)) {
        Some(m) if !m.path.is_empty() => {
            format!("{address:#018x} {}+{:#x}", m.path, address - m.start + m.offset)
        }
        Some(_) => format!("{address:#018x} [anonymous executable mapping]"),
        None => format!("{address:#018x}"),
    }
}

/// One `PT_LOAD` segment: `(file offset, file size, virtual address)`.
pub type LoadSegment = (u64, u64, u64);

/// The `PT_LOAD` segments of a little-endian ELF64 image (its first bytes: the program headers
/// sit near the start). `None` when it is not one, or the headers lie past `head`.
#[must_use]
pub fn elf_load_segments(head: &[u8]) -> Option<Vec<LoadSegment>> {
    if head.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let u16_at = |at: usize| -> Option<u64> {
        let b = head.get(at..at + 2)?;
        Some(u64::from(u16::from_le_bytes([b[0], b[1]])))
    };
    let u64_at = |at: usize| -> Option<u64> {
        let mut v = [0u8; 8];
        v.copy_from_slice(head.get(at..at + 8)?);
        Some(u64::from_le_bytes(v))
    };
    let phoff = usize::try_from(u64_at(0x20)?).ok()?;
    let entsize = usize::try_from(u16_at(0x36)?).ok()?;
    let count = usize::try_from(u16_at(0x38)?).ok()?;
    if entsize < 56 {
        return None;
    }
    let mut segments = Vec::new();
    for i in 0..count {
        let at = phoff.checked_add(i.checked_mul(entsize)?)?;
        let kind = head.get(at..at + 4)?;
        if kind == [1, 0, 0, 0] {
            segments.push((u64_at(at + 8)?, u64_at(at + 32)?, u64_at(at + 16)?));
        }
    }
    Some(segments)
}

/// The virtual address (what `llvm-addr2line` takes) of `file_offset` in a library with
/// `segments`.
#[must_use]
pub fn offset_to_vaddr(segments: &[LoadSegment], file_offset: u64) -> Option<u64> {
    segments
        .iter()
        .find(|(off, size, _)| (*off..off.saturating_add(*size)).contains(&file_offset))
        .map(|(off, _, vaddr)| file_offset - off + vaddr)
}

/// The stack lines of a record: `pc`, `lr`, then the walked frames, each described against the
/// recorded maps. Where the mapped library is still readable on this device, the line ends with
/// its virtual address (`vaddr 0x…`, for `llvm-addr2line`); an app update since the crash can
/// change the library, so trust it only for the same build.
#[must_use]
pub fn stack_lines(record: &CrashRecord, maps_text: &str) -> Vec<String> {
    let maps = parse_maps(maps_text);
    let mut elf: Vec<(String, Option<Vec<LoadSegment>>)> = Vec::new();
    let mut describe = |address: u64| {
        let mut text = describe_address(address, &maps);
        let Some(m) = maps.iter().find(|m| (m.start..m.end).contains(&address)) else {
            return text;
        };
        if m.path.is_empty() || m.path.starts_with('[') {
            return text;
        }
        let at = match elf.iter().position(|(p, _)| *p == m.path) {
            Some(at) => at,
            None => {
                elf.push((
                    m.path.clone(),
                    read_head(Path::new(&m.path))
                        .as_deref()
                        .and_then(elf_load_segments),
                ));
                elf.len() - 1
            }
        };
        if let Some(vaddr) = elf[at]
            .1
            .as_deref()
            .and_then(|s| offset_to_vaddr(s, address - m.start + m.offset))
        {
            let _ = write!(text, " (vaddr {vaddr:#x})");
        }
        text
    };
    let mut lines = Vec::new();
    if record.pc != 0 {
        lines.push(format!("pc {}", describe(record.pc)));
    }
    if record.lr != 0 {
        lines.push(format!("lr {}", describe(record.lr)));
    }
    for (i, frame) in record.frames().iter().enumerate() {
        lines.push(format!("#{i:02} {}", describe(*frame)));
    }
    lines
}

/// The first 64 KiB of a file (where ELF program headers are), if readable.
fn read_head(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(64 * 1024)
        .read_to_end(&mut head)
        .ok()?;
    Some(head)
}

/// The signal's name for the common crash signals.
#[must_use]
pub fn signal_name(signal: u32) -> String {
    match signal {
        4 => "SIGILL".into(),
        5 => "SIGTRAP".into(),
        6 => "SIGABRT".into(),
        7 => "SIGBUS".into(),
        8 => "SIGFPE".into(),
        11 => "SIGSEGV".into(),
        n => format!("signal {n}"),
    }
}

/// The text crash report (`logs://crash-<time>-native.txt`, read by `diagnostics.crash_reports`).
#[must_use]
pub fn report_text(record: &CrashRecord, maps_text: &str, platform: &str) -> String {
    let mut s = String::new();
    let build = record.build();
    let thread = record.thread_name();
    let _ = writeln!(s, "Regia Engine native crash record");
    let _ = writeln!(s, "===========================");
    let _ = writeln!(s, "unix_ms: {}", record.unix_ms);
    let _ = writeln!(s, "platform: {platform}");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "native crash: {} (code {})",
        signal_name(record.signal),
        record.code
    );
    let _ = writeln!(s, "fault address: {:#018x}", record.address);
    let _ = writeln!(
        s,
        "thread: {} (tid {})",
        if thread.is_empty() {
            "<unknown>"
        } else {
            thread.as_str()
        },
        record.tid
    );
    let _ = writeln!(s, "process: {}", record.pid);
    let _ = writeln!(s, "minidump: none (an in-process record; ADR-328)");
    let _ = writeln!(s);
    let _ = writeln!(s, "build:");
    let _ = writeln!(s, "  version: {}", build.version);
    let _ = writeln!(s, "  git: {}", build.git_hash);
    let _ = writeln!(s, "  profile: {}", build.profile);
    let _ = writeln!(s, "  engine: {}", build.engine);
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "stack (pc, lr, then {} frame record(s); walk ended: {}):",
        record.frames().len(),
        record.walk_end.as_str()
    );
    for line in stack_lines(record, maps_text) {
        let _ = writeln!(s, "  {line}");
    }
    s
}

/// Whether a record is the abort that follows a panic report: `SIGABRT` within
/// [`PANIC_ABORT_WINDOW_MS`] after one of `panic_reports_ms` (or up to 1 s before, for clock
/// granularity). Such a record gets no second report.
#[must_use]
pub fn follows_panic(record: &CrashRecord, panic_reports_ms: &[u64]) -> bool {
    record.signal == 6
        && panic_reports_ms.iter().any(|&t| {
            t <= record.unix_ms.saturating_add(1_000)
                && record.unix_ms.saturating_sub(t) <= PANIC_ABORT_WINDOW_MS
        })
}

/// A record the next launch picked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedRecord {
    /// Where it was moved: `<record folder>/<time>.crash.bin`.
    pub path: PathBuf,
    /// The decoded header.
    pub record: CrashRecord,
    /// The executable maps lines at the crash.
    pub maps: String,
}

/// What [`collect_pending`] found.
#[derive(Debug)]
pub enum Collected {
    /// No `pending.bin`, or one of zeros (the last run did not crash).
    Nothing,
    /// A valid record, now moved to [`CollectedRecord::path`] (boxed: the record is ~0.5 KiB).
    Record(Box<CollectedRecord>),
    /// A file that is not a valid record; it was removed.
    Rejected(RecordError),
}

/// `YYYY-MM-DD_HH-MM-SS` (UTC) of `unix_ms`.
#[must_use]
pub fn stamp(unix_ms: u64) -> String {
    let (y, mo, d, h, mi, s) = regia_core::log::civil_date_time(unix_ms / 1000);
    format!("{y:04}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}")
}

/// Reads `<dir>/pending.bin` left by the last run. A valid record is renamed to
/// `<time>.crash.bin` (the newest [`KEEP_RECORDS`] are kept); an invalid one is removed. Call
/// before [`prepare_pending`] recreates the file.
///
/// # Errors
/// IO errors other than a missing file.
pub fn collect_pending(dir: &Path) -> std::io::Result<Collected> {
    let pending = dir.join(PENDING_FILE);
    let bytes = match std::fs::read(&pending) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Collected::Nothing),
        Err(e) => return Err(e),
    };
    match decode_file(&bytes) {
        Err(RecordError::Empty) => Ok(Collected::Nothing),
        Err(e) => {
            std::fs::remove_file(&pending)?;
            Ok(Collected::Rejected(e))
        }
        Ok((record, maps)) => {
            let stem = stamp(record.unix_ms);
            let mut path = dir.join(format!("{stem}{COLLECTED_SUFFIX}"));
            let mut n = 1u32;
            while path.exists() {
                path = dir.join(format!("{stem}_{n}{COLLECTED_SUFFIX}"));
                n += 1;
            }
            std::fs::rename(&pending, &path)?;
            rotate(dir, KEEP_RECORDS);
            Ok(Collected::Record(Box::new(CollectedRecord {
                path,
                record,
                maps,
            })))
        }
    }
}

/// The collected records in `dir`, oldest first (names sort by time).
#[must_use]
pub fn collected_records(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(COLLECTED_SUFFIX))
        .collect();
    names.sort();
    names.into_iter().map(|n| dir.join(n)).collect()
}

/// Removes the oldest collected records beyond `keep`.
pub fn rotate(dir: &Path, keep: usize) {
    let all = collected_records(dir);
    let excess = all.len().saturating_sub(keep);
    for old in all.iter().take(excess) {
        let _ = std::fs::remove_file(old);
    }
}

/// Suffix of the text report a record produces under `logs://` (`crash-<time>-native.txt`).
pub const REPORT_SUFFIX: &str = "-native.txt";

/// Modification times (ms since the epoch) of the panic reports in `logs_dir`: `crash-*.txt`
/// files that are not native reports.
#[must_use]
pub fn panic_report_times(logs_dir: &Path) -> Vec<u64> {
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| crate::crash::is_crash_file_name(n) && !n.ends_with(REPORT_SUFFIX))
        })
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .collect()
}

/// Writes [`report_text`] to `logs_dir/crash-<time>-native.txt` (atomically: a temp file, then
/// a rename), where `diagnostics.crash_reports` lists it. Returns `None`, writing nothing, when
/// the record is the abort that follows a panic report ([`follows_panic`]).
///
/// # Errors
/// IO errors creating the folder or writing the file.
pub fn write_report(
    logs_dir: &Path,
    collected: &CollectedRecord,
    platform: &str,
) -> std::io::Result<Option<PathBuf>> {
    if follows_panic(&collected.record, &panic_report_times(logs_dir)) {
        return Ok(None);
    }
    std::fs::create_dir_all(logs_dir)?;
    let stem = format!("crash-{}", stamp(collected.record.unix_ms));
    let mut path = logs_dir.join(format!("{stem}{REPORT_SUFFIX}"));
    let mut n = 1u32;
    while path.exists() {
        path = logs_dir.join(format!("{stem}_{n}{REPORT_SUFFIX}"));
        n += 1;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, report_text(&collected.record, &collected.maps, platform))?;
    std::fs::rename(&tmp, &path)?;
    Ok(Some(path))
}

/// Creates (or truncates) `<dir>/pending.bin` as [`FILE_SIZE`] zero bytes, so the handler only
/// overwrites blocks that already exist, and returns it opened for writing at offset 0.
///
/// # Errors
/// IO errors creating the folder or the file.
pub fn prepare_pending(dir: &Path) -> std::io::Result<std::fs::File> {
    use std::io::{Seek as _, Write as _};
    std::fs::create_dir_all(dir)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dir.join(PENDING_FILE))?;
    file.write_all(&vec![0u8; FILE_SIZE])?;
    file.sync_all()?;
    file.rewind()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CrashRecord {
        let mut r = CrashRecord::with_build(&crate::crash::BuildInfo {
            version: "1.2.3".into(),
            git_hash: "abcdef0123456789".into(),
            profile: "shipping".into(),
            engine: "0.1.28".into(),
        });
        r.unix_ms = 1_792_600_000_123;
        r.signal = 11;
        r.code = 1;
        r.address = 0x10;
        r.pid = 4321;
        r.tid = 4400;
        r.thread = fixed("regia-crash-t");
        r.pc = 0x7a00_1000;
        r.lr = 0x7a00_2000;
        r.sp = 0x7f00_0000;
        r.fp = 0x7f00_0040;
        r.frames[0] = 0x7a00_3000;
        r.frames[1] = 0x7b00_0010;
        r.frame_count = 2;
        r.walk_end = WalkEnd::EndOfChain;
        r
    }

    fn encoded(r: &CrashRecord, maps: &[u8]) -> Vec<u8> {
        let mut r = *r;
        r.maps_len = maps.len() as u32;
        let mut head = [0u8; HEADER_SIZE];
        encode(&r, &mut head);
        let mut bytes = head.to_vec();
        bytes.extend_from_slice(maps);
        bytes
    }

    #[test]
    fn encode_then_decode_round_trips() {
        let r = sample();
        let maps = b"7a000000-7a010000 r-xp 00000000 fd:01 12 /data/app/lib/arm64/libmain.so\n";
        let bytes = encoded(&r, maps);
        let (back, text) = decode_file(&bytes).expect("valid");
        assert_eq!(
            back,
            CrashRecord {
                maps_len: maps.len() as u32,
                ..r
            }
        );
        assert_eq!(text.as_bytes(), maps);
        assert_eq!(back.thread_name(), "regia-crash-t");
        assert_eq!(back.frames(), &[0x7a00_3000, 0x7b00_0010]);
        assert_eq!(back.build().git_hash, "abcdef0123456789");
        assert_eq!(back.build().engine, "0.1.28");
    }

    #[test]
    fn bad_files_are_rejected_with_a_typed_error() {
        let bytes = encoded(&sample(), b"");
        assert_eq!(decode(&[]), Err(RecordError::Empty));
        assert_eq!(decode(&[0u8; FILE_SIZE]), Err(RecordError::Empty));
        assert_eq!(
            decode(&bytes[..100]),
            Err(RecordError::Truncated {
                len: 100,
                need: HEADER_SIZE
            })
        );
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert_eq!(decode(&bad), Err(RecordError::BadMagic));
        let mut torn = bytes.clone();
        torn[30] ^= 0xff;
        assert!(matches!(decode(&torn), Err(RecordError::Checksum { .. })));
        let mut newer = bytes.clone();
        newer[8] = 9;
        assert_eq!(decode(&newer), Err(RecordError::UnsupportedVersion(9)));
        // The header announces maps text the file does not have.
        let mut r = sample();
        r.maps_len = 100;
        let mut head = [0u8; HEADER_SIZE];
        encode(&r, &mut head);
        assert!(matches!(
            decode_file(&head),
            Err(RecordError::Truncated { len: HEADER_SIZE, .. })
        ));
    }

    #[test]
    fn long_text_fields_are_cut_at_a_char_boundary() {
        let f: [u8; 4] = fixed("abé");
        assert_eq!(&f, b"ab\xc3\xa9");
        let f: [u8; 3] = fixed("abé");
        assert_eq!(&f, b"ab\0");
    }

    /// A synthetic stack image: 64 words from `BASE`, readable only inside.
    const BASE: u64 = 0x7000_0000;

    fn reader(image: &[u64]) -> impl FnMut(u64) -> Option<[u64; 2]> + '_ {
        move |addr| {
            let i = usize::try_from(addr.checked_sub(BASE)? / 8).ok()?;
            Some([*image.get(i)?, *image.get(i + 1)?])
        }
    }

    /// A chain of `frames` records 32 bytes apart from `BASE`, the last one pointing at `last`.
    fn chain(frames: usize, last: u64) -> Vec<u64> {
        let mut image = vec![0u64; frames * 4 + 2];
        for f in 0..frames {
            let at = f * 4;
            image[at] = if f + 1 == frames {
                last
            } else {
                BASE + 32 * (f as u64 + 1)
            };
            image[at + 1] = 0x5000 + f as u64;
        }
        image
    }

    #[test]
    fn frame_walk_stops_at_the_bound() {
        let image = chain(40, 0);
        let mut out = [0u64; MAX_FRAMES];
        let (n, end) = walk_frame_pointers(BASE, BASE - 16, reader(&image), &mut out);
        assert_eq!((n, end), (MAX_FRAMES, WalkEnd::Bound));
        assert_eq!(out[0], 0x5000);
        assert_eq!(out[MAX_FRAMES - 1], 0x5000 + MAX_FRAMES as u64 - 1);
    }

    #[test]
    fn frame_walk_stops_at_a_bad_address() {
        let mut out = [0u64; MAX_FRAMES];
        // Points outside the stack span.
        let image = chain(3, BASE + MAX_STACK_SPAN + 64);
        assert_eq!(
            walk_frame_pointers(BASE, BASE, reader(&image), &mut out),
            (3, WalkEnd::BadAddress)
        );
        // Points back down the stack (a loop).
        let image = chain(3, BASE);
        assert_eq!(
            walk_frame_pointers(BASE, BASE, reader(&image), &mut out),
            (3, WalkEnd::BadAddress)
        );
        // Misaligned.
        let image = chain(2, BASE + 4 * 32 + 3);
        assert_eq!(
            walk_frame_pointers(BASE, BASE, reader(&image), &mut out),
            (2, WalkEnd::BadAddress)
        );
        // In the span but unreadable (past the image).
        let image = chain(2, BASE + 4096);
        assert_eq!(
            walk_frame_pointers(BASE, BASE, reader(&image), &mut out),
            (2, WalkEnd::BadAddress)
        );
        // Below the stack pointer.
        assert_eq!(
            walk_frame_pointers(BASE, BASE + 8, reader(&image), &mut out),
            (0, WalkEnd::BadAddress)
        );
    }

    #[test]
    fn frame_walk_ends_at_a_null_frame_and_strips_pointer_auth_bits() {
        let mut image = chain(2, 0);
        image[1] = 0x0042_0000_7a00_1234;
        let mut out = [0u64; MAX_FRAMES];
        assert_eq!(
            walk_frame_pointers(BASE, BASE, reader(&image), &mut out),
            (2, WalkEnd::EndOfChain)
        );
        assert_eq!(out[0], 0x7a00_1234);
    }

    #[test]
    fn maps_filter_keeps_executable_lines_across_chunks() {
        let text = b"7a000000-7a010000 r--p 00000000 fd:01 12 /system/lib64/libc.so\n\
7a010000-7a020000 r-xp 00010000 fd:01 12 /system/lib64/libc.so\n\
7b000000-7b100000 rw-p 00000000 00:00 0 [anon:scudo]\n\
7c000000-7c200000 r-xp 00040000 fd:02 99 /vendor/lib64/egl/libGLES_mali.so";
        for chunk in [1usize, 7, 64, 4096] {
            let mut filter = MapsFilter::new();
            let mut out = Vec::new();
            for part in text.chunks(chunk) {
                filter.feed(part, |line| out.extend_from_slice(line));
            }
            filter.finish(|line| out.extend_from_slice(line));
            let out = String::from_utf8(out).expect("utf8");
            assert_eq!(out.lines().count(), 2, "chunk {chunk}: {out}");
            assert!(out.contains("r-xp 00010000"));
            assert!(out.ends_with("libGLES_mali.so\n"));
        }
    }

    #[test]
    fn addresses_are_named_by_their_mapping() {
        let maps = parse_maps(
            "7c000000-7c200000 r-xp 00040000 fd:02 99 /vendor/lib64/egl/libGLES_mali.so\n\
             7d000000-7d001000 r-xp 00000000 00:00 0\n",
        );
        assert_eq!(
            describe_address(0x7c00_1234, &maps),
            "0x000000007c001234 /vendor/lib64/egl/libGLES_mali.so+0x41234"
        );
        assert!(describe_address(0x7d00_0010, &maps).ends_with("[anonymous executable mapping]"));
        assert_eq!(describe_address(0x10, &maps), "0x0000000000000010");
    }

    #[test]
    fn file_offsets_map_to_virtual_addresses() {
        // An ELF64 header with two PT_LOAD program headers at 0x40.
        let mut elf = vec![0u8; 0x40 + 2 * 56];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[0x20..0x28].copy_from_slice(&0x40u64.to_le_bytes());
        elf[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        elf[0x38..0x3a].copy_from_slice(&2u16.to_le_bytes());
        for (i, (off, vaddr, size)) in [(0u64, 0u64, 0x1000u64), (0x1000, 0x11000, 0x8000)]
            .into_iter()
            .enumerate()
        {
            let at = 0x40 + i * 56;
            elf[at] = 1;
            elf[at + 8..at + 16].copy_from_slice(&off.to_le_bytes());
            elf[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
            elf[at + 32..at + 40].copy_from_slice(&size.to_le_bytes());
        }
        let segments = elf_load_segments(&elf).expect("elf");
        assert_eq!(segments, [(0, 0x1000, 0), (0x1000, 0x8000, 0x11000)]);
        assert_eq!(offset_to_vaddr(&segments, 0x1234), Some(0x11234));
        assert_eq!(offset_to_vaddr(&segments, 0x20000), None);
        assert_eq!(elf_load_segments(b"not an elf"), None);
        assert_eq!(elf_load_segments(&elf[..0x50]), None, "headers cut off");
    }

    #[test]
    fn an_abort_right_after_a_panic_is_recognized() {
        let mut r = sample();
        r.signal = 6;
        assert!(follows_panic(&r, &[r.unix_ms - 200]));
        assert!(!follows_panic(&r, &[r.unix_ms - PANIC_ABORT_WINDOW_MS - 1]));
        assert!(!follows_panic(&r, &[r.unix_ms + 5_000]));
        r.signal = 11;
        assert!(!follows_panic(&r, &[r.unix_ms - 200]));
    }

    #[test]
    fn the_report_names_signal_thread_build_and_stack() {
        let r = sample();
        let text = report_text(
            &r,
            "7a000000-7a010000 r-xp 00000000 fd:01 12 /data/app/lib/arm64/libmain.so\n",
            "android",
        );
        assert!(text.contains("native crash: SIGSEGV (code 1)"), "{text}");
        assert!(text.contains("thread: regia-crash-t (tid 4400)"), "{text}");
        assert!(text.contains("platform: android"), "{text}");
        assert!(text.contains("git: abcdef0123456789"), "{text}");
        assert!(
            text.contains("pc 0x000000007a001000 /data/app/lib/arm64/libmain.so+0x1000"),
            "{text}"
        );
        assert!(text.contains("#01 0x000000007b000010"), "{text}");
    }
}
