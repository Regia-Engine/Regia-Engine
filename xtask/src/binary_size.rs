// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask binary-size`: what makes a shipped game binary large, and a budget that stops it growing by
//! accident (ADR-3188).
//!
//! The command generates the chosen templates with `regia new`, builds each with the shipping profile for a
//! target, reads the executable's (or Android `.so`'s) symbol table and attributes every symbol's size to a
//! crate by its demangled path. Bytes no symbol covers (string literals, unwind tables, relocations,
//! padding) are one line of their own, never spread over the crates. `docs/ci/size-budgets.toml` records
//! the total and per-crate sizes with a tolerance; `--check` fails above it, `--update` re-records.
//!
//! The analysis half (`analyze`, `attribute`, `check_budget`) is pure and tested on small ELF files built in
//! the tests. Only ELF (Linux, Android) carries per-symbol sizes. A Windows PE gives section sizes only: its
//! symbols live in a PDB, which this tool does not read (no PDB dependency), and the report says so.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use serde::{Deserialize, Serialize};

/// Arguments of `cargo xtask binary-size`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Target triple (default: the host). `*-android` builds the template's `cdylib` with `cargo ndk`;
    /// `*-windows-msvc` builds with `cargo xwin`.
    #[arg(long)]
    pub target: Option<String>,
    /// Templates to measure (repeatable; default: blank and third_person).
    #[arg(long = "template")]
    pub templates: Vec<String>,
    /// Print the reports as JSON.
    #[arg(long)]
    pub json: bool,
    /// Compare with `docs/ci/size-budgets.toml`; exit 1 above the tolerance.
    #[arg(long)]
    pub check: bool,
    /// Re-record the budgets for the measured templates and target.
    #[arg(long)]
    pub update: bool,
    /// Measure this already-built binary instead of building templates (row name: the first `--template`, else `file`).
    #[arg(long)]
    pub file: Option<PathBuf>,
    /// Cargo target directory shared by the generated projects (default: under the temp dir).
    #[arg(long)]
    pub target_dir: Option<PathBuf>,
    /// Rows in the top-symbol and top-crate lists.
    #[arg(long, default_value_t = 30)]
    pub top: usize,
}

/// Templates measured when none is named.
pub const DEFAULT_TEMPLATES: [&str; 2] = ["blank", "third_person"];
/// Where the budgets live, relative to the workspace root.
pub const BUDGET_FILE: &str = "docs/ci/size-budgets.toml";
/// Crates recorded per budget row.
pub const RECORDED_CRATES: usize = 40;
/// Default tolerance in percent.
pub const DEFAULT_TOLERANCE_PERCENT: u32 = 3;
/// A crate under this many bytes of slack never fails on its own (tiny crates flap with hash changes).
pub const CRATE_SLACK_BYTES: u64 = 16 * 1024;
/// A crate with no budget row fails only above this size.
pub const NEW_CRATE_FLOOR_BYTES: u64 = 64 * 1024;
/// The bucket for bytes no symbol covers.
pub const UNATTRIBUTED: &str = "(no symbol)";
/// The bucket for symbols whose name is not a Rust path (C libraries linked in).
pub const NATIVE: &str = "(native)";

/// Loaded-section totals, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sections {
    /// Executable code.
    pub text: u64,
    /// Read-only data (strings, vtables, tables, unwind info).
    pub rodata: u64,
    /// Initialized writable data (including relocation targets).
    pub data: u64,
    /// Zero-initialized data (no file bytes).
    pub bss: u64,
}

impl Sections {
    /// Everything the loader maps.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.text + self.rodata + self.data + self.bss
    }
}

/// Where the symbol sizes came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolSource {
    /// The static symbol table (`.symtab`): the shipping profile keeps it.
    Symtab,
    /// Only the dynamic symbols: the binary was stripped.
    Dynsym,
    /// No sizes at all (a PE without a readable PDB, or a fully stripped file).
    None,
}

/// One sized symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// Raw (mangled) name.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
}

/// What `analyze` reads from a binary.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// `elf` or `pe`.
    pub format: &'static str,
    /// Bytes on disk.
    pub file_bytes: u64,
    /// Loaded-section totals.
    pub sections: Sections,
    /// Where `symbols` came from.
    pub source: SymbolSource,
    /// Sized symbols, aliases (same address and size) removed.
    pub symbols: Vec<Symbol>,
    /// Caveats the report prints.
    pub notes: Vec<String>,
}

/// One crate's share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrateRow {
    /// Crate name (`core`, `regia_render`, …, [`UNATTRIBUTED`], [`NATIVE`]).
    pub name: String,
    /// Bytes.
    pub bytes: u64,
    /// Symbols.
    pub symbols: u64,
}

/// One big symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolRow {
    /// Demangled path (hash removed).
    pub path: String,
    /// Bytes.
    pub bytes: u64,
}

/// A path instantiated several times (one symbol per type-parameter set).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GenericRow {
    /// Demangled path without hash.
    pub path: String,
    /// Number of copies.
    pub instantiations: u64,
    /// Bytes over all copies.
    pub bytes: u64,
}

/// The report of one binary.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Template id (or `file`).
    pub template: String,
    /// Target triple.
    pub target: String,
    /// Binary format.
    pub format: &'static str,
    /// Bytes on disk.
    pub file_bytes: u64,
    /// Loaded-section totals.
    pub sections: Sections,
    /// Loaded total (`text + rodata + data + bss`): what the budget tracks.
    pub total_bytes: u64,
    /// Where the symbol sizes came from.
    pub symbol_source: SymbolSource,
    /// Bytes in sections the loader does not map (symbol table, debug info).
    pub unloaded_bytes: u64,
    /// Crates by size, biggest first; includes [`UNATTRIBUTED`] and [`NATIVE`] as rows of their own.
    pub crates: Vec<CrateRow>,
    /// Biggest symbols.
    pub top_symbols: Vec<SymbolRow>,
    /// Generic paths with the most copies.
    pub generics: Vec<GenericRow>,
    /// Caveats.
    pub notes: Vec<String>,
}

// ------------------------------------------------------------------------------------------ demangling

/// Decodes one legacy-mangled identifier segment (`$LT$`, `..`, `$u20$` …).
fn decode_segment(raw: &str) -> String {
    let raw = raw
        .strip_prefix('_')
        .filter(|r| r.starts_with('$'))
        .unwrap_or(raw);
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("..") {
            out.push_str("::");
            rest = r;
        } else if let Some(r) = rest.strip_prefix('$') {
            let Some(end) = r.find('$') else {
                out.push('$');
                rest = r;
                continue;
            };
            let code = &r[..end];
            let ch = match code {
                "SP" => Some('@'),
                "BP" => Some('*'),
                "RF" => Some('&'),
                "LT" => Some('<'),
                "GT" => Some('>'),
                "LP" => Some('('),
                "RP" => Some(')'),
                "C" => Some(','),
                _ => code
                    .strip_prefix('u')
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .and_then(char::from_u32),
            };
            match ch {
                Some(c) => {
                    out.push(c);
                    rest = &r[end + 1..];
                }
                None => {
                    out.push('$');
                    rest = r;
                }
            }
        } else {
            let next = rest.find(['$', '.']).unwrap_or(rest.len()).max(1);
            let (head, tail) = rest.split_at(next);
            out.push_str(head);
            rest = tail;
        }
    }
    out
}

/// Demangles a legacy Rust symbol (`_ZN…E`) into its path without the trailing hash.
///
/// Returns `None` for anything else. Legacy mangling carries no type parameters, so two instantiations
/// of one generic function demangle to the same path and differ only in the dropped hash.
#[must_use]
pub fn demangle_legacy(name: &str) -> Option<String> {
    let name = name.strip_prefix("_ZN").or_else(|| name.strip_prefix("ZN"))?;
    let name = name.split(".llvm.").next().unwrap_or(name);
    let mut rest = name;
    let mut segments: Vec<String> = Vec::new();
    loop {
        if rest == "E" || rest.is_empty() {
            break;
        }
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let len: usize = rest[..digits].parse().ok()?;
        let body = rest.get(digits..digits + len)?;
        segments.push(body.to_owned());
        rest = &rest[digits + len..];
    }
    if segments.len() < 2 {
        return None;
    }
    // The last segment is the hash `h` + 16 hex digits.
    if let Some(last) = segments.last()
        && last.len() == 17
        && last.starts_with('h')
        && last[1..].chars().all(|c| c.is_ascii_hexdigit())
    {
        segments.pop();
    }
    let decoded: Vec<String> = segments.iter().map(|s| decode_segment(s)).collect();
    Some(decoded.join("::"))
}

/// A small parser for the v0 symbol grammar (RFC 2603). It writes the path with every generic argument
/// list elided to `<…>`, so instantiations of one generic function share one text.
struct V0<'a> {
    s: &'a [u8],
    pos: usize,
    steps: u32,
}

impl V0<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn tick(&mut self, depth: u32) -> Option<()> {
        self.steps += 1;
        (depth < 48 && self.steps < 20_000).then_some(())
    }

    fn base62(&mut self) -> Option<u64> {
        if self.eat(b'_') {
            return Some(0);
        }
        let mut v: u64 = 0;
        loop {
            let c = self.peek()?;
            self.pos += 1;
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'z' => c - b'a' + 10,
                b'A'..=b'Z' => c - b'A' + 36,
                b'_' => return v.checked_add(1),
                _ => return None,
            };
            v = v.checked_mul(62)?.checked_add(u64::from(d))?;
        }
    }

    fn ident(&mut self) -> Option<&str> {
        if self.eat(b's') {
            self.base62()?;
        }
        self.eat(b'u');
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        let len: usize = std::str::from_utf8(&self.s[start..self.pos]).ok()?.parse().ok()?;
        self.eat(b'_');
        let body = self.s.get(self.pos..self.pos.checked_add(len)?)?;
        self.pos += len;
        std::str::from_utf8(body).ok()
    }

    /// Runs `f` at the position a backref points to, then returns to the current position.
    fn backref<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        let here = self.pos - 1;
        let target = usize::try_from(self.base62()?).ok()?;
        if target >= here {
            return None;
        }
        let saved = self.pos;
        self.pos = target;
        let r = f(self);
        self.pos = saved;
        r
    }

    fn eat_disambiguator(&mut self) {
        if self.eat(b's') {
            let _ = self.base62();
        }
    }

    fn path(&mut self, out: &mut String, depth: u32) -> Option<()> {
        self.tick(depth)?;
        match self.peek()? {
            b'C' => {
                self.pos += 1;
                out.push_str(self.ident()?);
            }
            b'M' => {
                self.pos += 1;
                self.eat_disambiguator();
                self.path(out, depth + 1)?;
                out.push_str("::{impl ");
                self.ty(out, depth + 1)?;
                out.push('}');
            }
            b'X' => {
                self.pos += 1;
                self.eat_disambiguator();
                let mut skip = String::new();
                self.path(&mut skip, depth + 1)?;
                out.push('<');
                self.ty(out, depth + 1)?;
                out.push_str(" as ");
                self.path(out, depth + 1)?;
                out.push('>');
            }
            b'Y' => {
                self.pos += 1;
                out.push('<');
                self.ty(out, depth + 1)?;
                out.push_str(" as ");
                self.path(out, depth + 1)?;
                out.push('>');
            }
            b'N' => {
                self.pos += 1;
                let ns = self.peek()?;
                self.pos += 1;
                self.path(out, depth + 1)?;
                let name = self.ident()?.to_owned();
                out.push_str("::");
                if name.is_empty() {
                    out.push_str(if ns == b'C' { "{closure}" } else { "{unnamed}" });
                } else {
                    out.push_str(&name);
                }
            }
            b'I' => {
                self.pos += 1;
                self.path(out, depth + 1)?;
                while !self.eat(b'E') {
                    self.tick(depth)?;
                    self.generic_arg(depth + 1)?;
                }
                out.push_str("<…>");
            }
            b'B' => {
                self.pos += 1;
                self.backref(|p| p.path(out, depth + 1))?;
            }
            _ => return None,
        }
        Some(())
    }

    fn generic_arg(&mut self, depth: u32) -> Option<()> {
        if self.eat(b'L') {
            return self.base62().map(|_| ());
        }
        if self.eat(b'K') {
            return self.konst(depth);
        }
        let mut sink = String::new();
        self.ty(&mut sink, depth)
    }

    fn konst(&mut self, depth: u32) -> Option<()> {
        self.tick(depth)?;
        if self.eat(b'p') {
            return Some(());
        }
        if self.eat(b'B') {
            return self.backref(|p| p.konst(depth + 1));
        }
        let mut sink = String::new();
        self.ty(&mut sink, depth + 1)?;
        self.eat(b'n');
        while self.peek()? != b'_' {
            self.pos += 1;
        }
        self.pos += 1;
        Some(())
    }

    fn ty(&mut self, out: &mut String, depth: u32) -> Option<()> {
        self.tick(depth)?;
        let c = self.peek()?;
        let basic = match c {
            b'a' => "i8",
            b'b' => "bool",
            b'c' => "char",
            b'd' => "f64",
            b'e' => "str",
            b'f' => "f32",
            b'h' => "u8",
            b'i' => "isize",
            b'j' => "usize",
            b'l' => "i32",
            b'm' => "u32",
            b'n' => "i128",
            b'o' => "u128",
            b'p' => "_",
            b's' => "i16",
            b't' => "u16",
            b'u' => "()",
            b'v' => "...",
            b'x' => "i64",
            b'y' => "u64",
            b'z' => "!",
            _ => "",
        };
        if !basic.is_empty() {
            self.pos += 1;
            out.push_str(basic);
            return Some(());
        }
        match c {
            b'A' => {
                self.pos += 1;
                out.push('[');
                self.ty(out, depth + 1)?;
                out.push_str("; _]");
                self.konst(depth + 1)?;
            }
            b'S' => {
                self.pos += 1;
                out.push('[');
                self.ty(out, depth + 1)?;
                out.push(']');
            }
            b'T' => {
                self.pos += 1;
                out.push('(');
                while !self.eat(b'E') {
                    self.tick(depth)?;
                    self.ty(out, depth + 1)?;
                    out.push(',');
                }
                out.push(')');
            }
            b'R' | b'Q' => {
                self.pos += 1;
                if self.eat(b'L') {
                    self.base62()?;
                }
                out.push_str(if c == b'R' { "&" } else { "&mut " });
                self.ty(out, depth + 1)?;
            }
            b'P' | b'O' => {
                self.pos += 1;
                out.push_str(if c == b'P' { "*const " } else { "*mut " });
                self.ty(out, depth + 1)?;
            }
            b'F' => {
                self.pos += 1;
                if self.eat(b'G') {
                    self.base62()?;
                }
                self.eat(b'U');
                if self.eat(b'K') && !self.eat(b'C') {
                    self.ident()?;
                }
                out.push_str("fn(");
                while !self.eat(b'E') {
                    self.tick(depth)?;
                    self.ty(out, depth + 1)?;
                    out.push(',');
                }
                out.push_str(")->");
                self.ty(out, depth + 1)?;
            }
            b'D' => {
                self.pos += 1;
                if self.eat(b'G') {
                    self.base62()?;
                }
                out.push_str("dyn ");
                while !self.eat(b'E') {
                    self.tick(depth)?;
                    self.path(out, depth + 1)?;
                    out.push('+');
                    while self.eat(b'p') {
                        self.ident()?;
                        let mut sink = String::new();
                        self.ty(&mut sink, depth + 1)?;
                    }
                }
                if self.eat(b'L') {
                    self.base62()?;
                }
            }
            b'B' => {
                self.pos += 1;
                self.backref(|p| p.ty(out, depth + 1))?;
            }
            _ => self.path(out, depth + 1)?,
        }
        Some(())
    }
}

/// Demangles a v0 symbol (`_R…`) into its path with generic arguments elided to `<…>`.
#[must_use]
pub fn demangle_v0(name: &str) -> Option<String> {
    let body = name.strip_prefix("_R")?;
    let body = body.split(".llvm.").next().unwrap_or(body);
    let mut p = V0 {
        s: body.as_bytes(),
        pos: 0,
        steps: 0,
    };
    while p.peek().is_some_and(|c| c.is_ascii_digit()) {
        p.pos += 1;
    }
    let mut out = String::new();
    p.path(&mut out, 0)?;
    Some(out)
}

/// The demangled path of a symbol name, or `None` when it is not a Rust symbol.
#[must_use]
pub fn demangle(name: &str) -> Option<String> {
    demangle_legacy(name).or_else(|| demangle_v0(name))
}

/// The crate a demangled path belongs to.
///
/// `<A as B>::f` belongs to `A`'s crate; when `A` is a primitive or a type parameter, to `B`'s.
#[must_use]
pub fn crate_of(path: &str) -> Option<String> {
    fn leading_crate(s: &str) -> Option<&str> {
        let s = s.trim_start_matches(['<', '&', '*']);
        let s = s.strip_prefix("mut ").unwrap_or(s);
        let s = s.strip_prefix("const ").unwrap_or(s);
        let s = s.strip_prefix("dyn ").unwrap_or(s);
        let s = s.trim_start_matches(['<', '&', '*']);
        let end = s.find("::")?;
        let ident = &s[..end];
        let ok = !ident.is_empty() && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        (ok && ident != "_").then_some(ident)
    }
    if let Some(inner) = path.strip_prefix('<') {
        if let Some(c) = leading_crate(inner) {
            return Some(c.to_owned());
        }
        if let Some(pos) = inner.find(" as ")
            && let Some(c) = leading_crate(&inner[pos + 4..])
        {
            return Some(c.to_owned());
        }
        return None;
    }
    leading_crate(path).map(str::to_owned)
}

// ------------------------------------------------------------------------------------------- reading

fn push_symbol(list: &mut Vec<(u64, Symbol)>, addr: u64, name: &str, size: u64) {
    if size > 0 && !name.is_empty() {
        list.push((
            addr,
            Symbol {
                name: name.to_owned(),
                size,
            },
        ));
    }
}

fn dedupe_aliases(mut list: Vec<(u64, Symbol)>) -> Vec<Symbol> {
    list.sort_by(|a, b| (a.0, a.1.size, &a.1.name).cmp(&(b.0, b.1.size, &b.1.name)));
    list.dedup_by(|b, a| a.0 == b.0 && a.1.size == b.1.size);
    list.into_iter().map(|(_, s)| s).collect()
}

fn analyze_elf(bytes: &[u8]) -> Result<Analysis> {
    use goblin::elf::section_header::{SHF_ALLOC, SHF_EXECINSTR, SHF_WRITE, SHT_NOBITS};
    use goblin::elf::sym::{STT_FUNC, STT_OBJECT, STT_TLS};
    let elf = goblin::elf::Elf::parse(bytes).context("parsing the ELF file")?;
    let mut sections = Sections::default();
    for sh in &elf.section_headers {
        if sh.sh_flags & u64::from(SHF_ALLOC) == 0 {
            continue;
        }
        let size = sh.sh_size;
        if sh.sh_type == SHT_NOBITS {
            sections.bss += size;
        } else if sh.sh_flags & u64::from(SHF_EXECINSTR) != 0 {
            sections.text += size;
        } else if sh.sh_flags & u64::from(SHF_WRITE) != 0 {
            sections.data += size;
        } else {
            sections.rodata += size;
        }
    }
    let mut list = Vec::new();
    let mut source = SymbolSource::None;
    for sym in elf.syms.iter() {
        if !matches!(sym.st_type(), STT_FUNC | STT_OBJECT | STT_TLS) || sym.st_shndx == 0 {
            continue;
        }
        let name = elf.strtab.get_at(sym.st_name).unwrap_or("");
        push_symbol(&mut list, sym.st_value, name, sym.st_size);
        source = SymbolSource::Symtab;
    }
    let mut notes = Vec::new();
    if list.is_empty() {
        for sym in elf.dynsyms.iter() {
            if !matches!(sym.st_type(), STT_FUNC | STT_OBJECT | STT_TLS) || sym.st_shndx == 0 {
                continue;
            }
            let name = elf.dynstrtab.get_at(sym.st_name).unwrap_or("");
            push_symbol(&mut list, sym.st_value, name, sym.st_size);
            source = SymbolSource::Dynsym;
        }
        notes.push(
            "stripped binary: only the dynamic symbols are named, nearly everything lands in the (no symbol) line. \
             Measure the unstripped shipping build (`strip = \"none\"`)."
                .to_owned(),
        );
    }
    Ok(Analysis {
        format: "elf",
        file_bytes: bytes.len() as u64,
        sections,
        source,
        symbols: dedupe_aliases(list),
        notes,
    })
}

fn analyze_pe(bytes: &[u8], pdb_beside: bool) -> Result<Analysis> {
    use goblin::pe::section_table::{
        IMAGE_SCN_CNT_CODE, IMAGE_SCN_CNT_UNINITIALIZED_DATA, IMAGE_SCN_MEM_EXECUTE, IMAGE_SCN_MEM_WRITE,
    };
    let pe = goblin::pe::PE::parse(bytes).context("parsing the PE file")?;
    let mut sections = Sections::default();
    for s in &pe.sections {
        let size = u64::from(s.virtual_size.max(s.size_of_raw_data));
        let c = s.characteristics;
        if c & (IMAGE_SCN_CNT_CODE | IMAGE_SCN_MEM_EXECUTE) != 0 {
            sections.text += size;
        } else if c & IMAGE_SCN_CNT_UNINITIALIZED_DATA != 0 {
            sections.bss += size;
        } else if c & IMAGE_SCN_MEM_WRITE != 0 {
            sections.data += size;
        } else {
            sections.rodata += size;
        }
    }
    let note = if pdb_beside {
        "Windows: a PDB sits next to the binary, but this tool does not read PDB files (no PDB dependency, \
         ADR-3188): section sizes only. Use the Linux or Android report for the crate split, the engine code is the same."
    } else {
        "Windows: no PDB next to the binary, section sizes only. Use the Linux or Android report for the crate split."
    };
    Ok(Analysis {
        format: "pe",
        file_bytes: bytes.len() as u64,
        sections,
        source: SymbolSource::None,
        symbols: Vec::new(),
        notes: vec![note.to_owned()],
    })
}

/// Reads a binary: section totals and sized symbols.
///
/// # Errors
///
/// The file is neither ELF nor PE, or is malformed. Mach-O is not read.
pub fn analyze(bytes: &[u8], pdb_beside: bool) -> Result<Analysis> {
    if bytes.starts_with(b"\x7fELF") {
        analyze_elf(bytes)
    } else if bytes.starts_with(b"MZ") {
        analyze_pe(bytes, pdb_beside)
    } else {
        bail!("unsupported binary format (ELF for Linux/Android and PE for Windows are read; Mach-O is not)")
    }
}

// --------------------------------------------------------------------------------------- attribution

/// Attributes an analysis to crates and builds the report.
#[must_use]
pub fn attribute(a: &Analysis, template: &str, target: &str, top: usize) -> Report {
    let mut by_crate: HashMap<String, (u64, u64)> = HashMap::new();
    let mut symbols: Vec<SymbolRow> = Vec::with_capacity(a.symbols.len());
    let mut generics: HashMap<String, (u64, u64)> = HashMap::new();
    let mut named = 0_u64;
    for s in &a.symbols {
        named += s.size;
        let (krate, path) = match demangle(&s.name) {
            Some(path) => (crate_of(&path).unwrap_or_else(|| NATIVE.to_owned()), path),
            None => (NATIVE.to_owned(), s.name.clone()),
        };
        let e = by_crate.entry(krate).or_default();
        e.0 += s.size;
        e.1 += 1;
        if demangle(&s.name).is_some() {
            let g = generics.entry(path.clone()).or_default();
            g.0 += 1;
            g.1 += s.size;
        }
        symbols.push(SymbolRow { path, bytes: s.size });
    }
    let total = a.sections.total();
    let unattributed = total.saturating_sub(named);
    let mut crates: Vec<CrateRow> = by_crate
        .into_iter()
        .map(|(name, (bytes, symbols))| CrateRow { name, bytes, symbols })
        .collect();
    crates.push(CrateRow {
        name: UNATTRIBUTED.to_owned(),
        bytes: unattributed,
        symbols: 0,
    });
    crates.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    symbols.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    symbols.truncate(top);
    let mut generic_rows: Vec<GenericRow> = generics
        .into_iter()
        .filter(|(_, (n, _))| *n >= 2)
        .map(|(path, (instantiations, bytes))| GenericRow {
            path,
            instantiations,
            bytes,
        })
        .collect();
    generic_rows.sort_by(|a, b| {
        b.instantiations
            .cmp(&a.instantiations)
            .then_with(|| b.bytes.cmp(&a.bytes))
            .then_with(|| a.path.cmp(&b.path))
    });
    generic_rows.truncate(top);
    let loaded_file_bytes = a.sections.text + a.sections.rodata + a.sections.data;
    Report {
        template: template.to_owned(),
        target: target.to_owned(),
        format: a.format,
        file_bytes: a.file_bytes,
        sections: a.sections,
        total_bytes: total,
        symbol_source: a.source,
        unloaded_bytes: a.file_bytes.saturating_sub(loaded_file_bytes),
        crates,
        top_symbols: symbols,
        generics: generic_rows,
        notes: a.notes.clone(),
    }
}

// ------------------------------------------------------------------------------------------- budgets

/// One budget row: a template on a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Template id.
    pub template: String,
    /// Target triple.
    pub target: String,
    /// Loaded total in bytes.
    pub total_bytes: u64,
    /// Allowed growth in percent, for the total and each crate.
    pub tolerance_percent: u32,
    /// Per-crate bytes (the biggest ones; [`UNATTRIBUTED`] included).
    pub crates: BTreeMap<String, u64>,
}

/// `docs/ci/size-budgets.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetFile {
    /// Always 1.
    pub format_version: u32,
    /// The rows.
    #[serde(default)]
    pub budget: Vec<Budget>,
}

impl Default for BudgetFile {
    fn default() -> Self {
        Self {
            format_version: 1,
            budget: Vec::new(),
        }
    }
}

/// The budget row a report records.
#[must_use]
pub fn record(report: &Report, tolerance_percent: u32) -> Budget {
    Budget {
        template: report.template.clone(),
        target: report.target.clone(),
        total_bytes: report.total_bytes,
        tolerance_percent,
        crates: report
            .crates
            .iter()
            .filter(|c| c.symbols > 0 || c.name == UNATTRIBUTED)
            .take(RECORDED_CRATES)
            .map(|c| (c.name.clone(), c.bytes))
            .collect(),
    }
}

fn limit(bytes: u64, percent: u32) -> u64 {
    bytes + bytes * u64::from(percent) / 100
}

/// Failures of `report` against `budget` (empty = pass), and hints when it is far below.
#[must_use]
pub fn check_budget(report: &Report, budget: &Budget) -> (Vec<String>, Vec<String>) {
    let mut fails = Vec::new();
    let mut hints = Vec::new();
    let max_total = limit(budget.total_bytes, budget.tolerance_percent);
    if report.total_bytes > max_total {
        fails.push(format!(
            "total {} > budget {} + {}% = {}",
            report.total_bytes, budget.total_bytes, budget.tolerance_percent, max_total
        ));
    } else if report.total_bytes
        < budget.total_bytes - budget.total_bytes * u64::from(budget.tolerance_percent) / 100
    {
        hints.push(format!(
            "total {} is more than {}% under the budget {}: run `cargo xtask binary-size --update` to lock the gain in",
            report.total_bytes, budget.tolerance_percent, budget.total_bytes
        ));
    }
    let measured: HashMap<&str, u64> = report.crates.iter().map(|c| (c.name.as_str(), c.bytes)).collect();
    for (name, recorded) in &budget.crates {
        let now = measured.get(name.as_str()).copied().unwrap_or(0);
        let max = limit(*recorded, budget.tolerance_percent) + CRATE_SLACK_BYTES;
        if now > max {
            fails.push(format!(
                "crate `{name}` {now} > budget {recorded} + {}% + 16 KiB slack = {max}",
                budget.tolerance_percent
            ));
        }
    }
    // When the row is full (the top-N cut dropped crates), a crate below the smallest recorded one was
    // left out by that cut: it may grow to that size (plus tolerance) before it counts as new. A shorter
    // row recorded every crate, so anything new above the floor is new.
    let cutoff = if budget.crates.len() >= RECORDED_CRATES {
        budget
            .crates
            .values()
            .copied()
            .min()
            .map_or(0, |m| limit(m, budget.tolerance_percent) + CRATE_SLACK_BYTES)
    } else {
        0
    };
    let floor = NEW_CRATE_FLOOR_BYTES.max(cutoff);
    for c in &report.crates {
        if !budget.crates.contains_key(&c.name) && c.bytes > floor && c.symbols > 0 {
            fails.push(format!(
                "crate `{}` is {} bytes and has no budget row (new in the binary, or newly big): run --update if intended",
                c.name, c.bytes
            ));
        }
    }
    (fails, hints)
}

/// Loads the budget file (an empty one when it does not exist).
///
/// # Errors
///
/// The file exists but is not valid, or has another `format_version`.
pub fn load_budgets(path: &Path) -> Result<BudgetFile> {
    if !path.exists() {
        return Ok(BudgetFile::default());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: BudgetFile = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if file.format_version != 1 {
        bail!(
            "{}: format_version {} is not supported (1)",
            path.display(),
            file.format_version
        );
    }
    Ok(file)
}

/// Writes the budget file atomically.
///
/// # Errors
///
/// Serialization or IO failure.
pub fn save_budgets(path: &Path, file: &BudgetFile) -> Result<()> {
    let mut body = String::from(
        "# Binary size budgets (ADR-3188). Written by `cargo xtask binary-size --update`; checked by `--check`.\n\
         # total_bytes is the loaded size (text + rodata + data + bss) of the shipping build; crates are the biggest\n\
         # crates by attributed symbol bytes. Growth above tolerance_percent fails; re-record deliberately.\n\n",
    );
    body.push_str(&toml::to_string_pretty(file).context("serializing the budgets")?);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Merges `row` into `file`, replacing the row for the same template and target; keeps rows sorted.
pub fn upsert(file: &mut BudgetFile, row: Budget) {
    file.budget
        .retain(|b| !(b.template == row.template && b.target == row.target));
    file.budget.push(row);
    file.budget
        .sort_by(|a, b| (&a.template, &a.target).cmp(&(&b.template, &b.target)));
}

// -------------------------------------------------------------------------------------------- output

fn mib(bytes: u64) -> String {
    format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// The text form of a report.
#[must_use]
pub fn render_text(r: &Report, top: usize) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "== {} on {} ({}) ==", r.template, r.target, r.format);
    let _ = writeln!(
        s,
        "file {} | loaded {} | not loaded (symbols, debug info) {}",
        mib(r.file_bytes),
        mib(r.total_bytes),
        mib(r.unloaded_bytes)
    );
    let _ = writeln!(
        s,
        "text {} | rodata {} | data {} | bss {}",
        mib(r.sections.text),
        mib(r.sections.rodata),
        mib(r.sections.data),
        mib(r.sections.bss)
    );
    for note in &r.notes {
        let _ = writeln!(s, "note: {note}");
    }
    if r.symbol_source != SymbolSource::None {
        let _ = writeln!(s, "\ntop crates (of {} loaded bytes):", r.total_bytes);
        for c in r.crates.iter().take(top.min(25)) {
            let pct = if r.total_bytes == 0 {
                0.0
            } else {
                c.bytes as f64 * 100.0 / r.total_bytes as f64
            };
            let _ = writeln!(
                s,
                "  {:>10}  {:>5.1}%  {:>7} sym  {}",
                c.bytes, pct, c.symbols, c.name
            );
        }
        let _ = writeln!(s, "\ntop {} symbols:", r.top_symbols.len());
        for t in &r.top_symbols {
            let _ = writeln!(s, "  {:>10}  {}", t.bytes, t.path);
        }
        let _ = writeln!(s, "\ngeneric paths with the most instantiations:");
        for g in &r.generics {
            let _ = writeln!(
                s,
                "  {:>5} copies  {:>10} bytes  {}",
                g.instantiations, g.bytes, g.path
            );
        }
    }
    s
}

// ---------------------------------------------------------------------------------------------- build

/// The shipping profile, as `--config` values: generated projects define no `[profile.shipping]` of their
/// own, `regia package` passes the engine's settings the same way.
pub const PROFILE_CONFIG: [&str; 6] = [
    "profile.shipping.inherits=\"release\"",
    "profile.shipping.lto=\"fat\"",
    "profile.shipping.codegen-units=1",
    "profile.shipping.panic=\"abort\"",
    "profile.shipping.debug=\"line-tables-only\"",
    "profile.shipping.strip=\"none\"",
];

/// The cargo invocation and output path for one template and target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPlan {
    /// Arguments after `cargo`.
    pub cargo_args: Vec<String>,
    /// The binary, relative to the target directory.
    pub output: PathBuf,
}

/// Builds the plan for `template` on `target`.
#[must_use]
pub fn build_plan(template: &str, target: &str, crate_name: &str) -> BuildPlan {
    let android = target.contains("android");
    let package = if android {
        format!("{crate_name}_android")
    } else if template == "quick" || template == "net_game" {
        crate_name.to_owned()
    } else {
        format!("{crate_name}_launcher")
    };
    let mut cargo_args: Vec<String> = Vec::new();
    if android {
        let abi = if target.starts_with("aarch64") {
            "arm64-v8a"
        } else {
            "x86_64"
        };
        cargo_args.extend(["ndk", "-t", abi, "-P", "26", "build"].map(str::to_owned));
    } else if target.ends_with("windows-msvc") {
        cargo_args.extend(["xwin", "build", "--target"].map(str::to_owned));
        cargo_args.push(target.to_owned());
    } else {
        cargo_args.extend(["build", "--target"].map(str::to_owned));
        cargo_args.push(target.to_owned());
    }
    cargo_args.extend(["--profile", "shipping", "-p"].map(str::to_owned));
    cargo_args.push(package);
    for c in PROFILE_CONFIG {
        cargo_args.push("--config".to_owned());
        cargo_args.push(c.to_owned());
    }
    let file = if android {
        "libmain.so".to_owned()
    } else if target.contains("windows") {
        format!("{crate_name}.exe")
    } else {
        crate_name.to_owned()
    };
    BuildPlan {
        cargo_args,
        output: PathBuf::from(target).join("shipping").join(file),
    }
}

fn host_triple() -> Result<String> {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("running rustc -vV")?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(str::to_owned)
        .context("rustc -vV printed no host line")
}

fn step(label: &str, command: &mut Command) -> Result<()> {
    println!("binary-size: {label}");
    let status = command
        .status()
        .with_context(|| format!("{label}: cannot start the process"))?;
    if !status.success() {
        bail!("{label}: {status}");
    }
    Ok(())
}

fn build_template(
    root: &Path,
    template: &str,
    target: &str,
    base: &Path,
    target_dir: &Path,
) -> Result<PathBuf> {
    let work = base.join(template);
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    step(
        &format!("{template}: regia new"),
        Command::new("cargo")
            .current_dir(root)
            .args([
                "run",
                "--locked",
                "-q",
                "-p",
                "regia_cli",
                "--bin",
                "regia",
                "--",
                "new",
            ])
            .arg(crate::templates_check::PROJECT_NAME)
            .args(["--template", template, "--path"])
            .arg(&work),
    )?;
    let project = work.join(crate::templates_check::PROJECT_NAME);
    let plan = build_plan(template, target, crate::templates_check::PROJECT_NAME);
    step(
        &format!("{template}: cargo {}", plan.cargo_args.join(" ")),
        Command::new("cargo")
            .current_dir(&project)
            .args(&plan.cargo_args)
            .env("CARGO_TARGET_DIR", target_dir),
    )?;
    Ok(target_dir.join(plan.output))
}

/// Measures one built binary.
fn measure(path: &Path, template: &str, target: &str, top: usize) -> Result<Report> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let pdb = path.with_extension("pdb").exists();
    let analysis = analyze(&bytes, pdb).with_context(|| format!("analyzing {}", path.display()))?;
    Ok(attribute(&analysis, template, target, top))
}

/// Runs the command; exit code 1 when `--check` finds a budget exceeded.
///
/// # Errors
///
/// A build or IO failure.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let target = match &args.target {
        Some(t) => t.clone(),
        None => host_triple()?,
    };
    let budget_path = root.join(BUDGET_FILE);
    let mut reports = Vec::new();
    if let Some(file) = &args.file {
        let name = args.templates.first().map_or("file", String::as_str);
        reports.push(measure(file, name, &target, args.top)?);
    } else {
        let templates: Vec<String> = if args.templates.is_empty() {
            DEFAULT_TEMPLATES.iter().map(|s| (*s).to_owned()).collect()
        } else {
            crate::templates_check::selected(&args.templates)?
                .into_iter()
                .map(str::to_owned)
                .collect()
        };
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "regia-binary-size-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let target_dir = args.target_dir.clone().unwrap_or_else(|| base.join("target"));
        for template in &templates {
            let built = build_template(root, template, &target, &base, &target_dir)?;
            reports.push(measure(&built, template, &target, args.top)?);
            let _ = std::fs::remove_dir_all(base.join(template));
        }
        if args.target_dir.is_none() {
            let _ = std::fs::remove_dir_all(&base);
        }
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&reports).context("serializing the reports")?
        );
    } else {
        for r in &reports {
            print!("{}", render_text(r, args.top));
        }
    }
    let mut file = load_budgets(&budget_path)?;
    if args.update {
        for r in &reports {
            let tolerance = file
                .budget
                .iter()
                .find(|b| b.template == r.template && b.target == r.target)
                .map_or(DEFAULT_TOLERANCE_PERCENT, |b| b.tolerance_percent);
            upsert(&mut file, record(r, tolerance));
        }
        save_budgets(&budget_path, &file)?;
        println!("binary-size: recorded {} in {}", reports.len(), BUDGET_FILE);
    }
    if args.check {
        let mut failed = false;
        for r in &reports {
            let Some(b) = file
                .budget
                .iter()
                .find(|b| b.template == r.template && b.target == r.target)
            else {
                eprintln!(
                    "binary-size: no budget row for {} on {} (run --update)",
                    r.template, r.target
                );
                failed = true;
                continue;
            };
            let (fails, hints) = check_budget(r, b);
            for h in &hints {
                println!("binary-size: {} on {}: {h}", r.template, r.target);
            }
            for f in &fails {
                eprintln!("binary-size: {} on {}: {f}", r.template, r.target);
            }
            failed |= !fails.is_empty();
        }
        if failed {
            return Ok(ExitCode::FAILURE);
        }
        println!("binary-size: within budget");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- a tiny ELF64 little-endian x86_64 builder: .text, .rodata, .data, .bss, .symtab, .strtab, .shstrtab

    struct Sym<'a> {
        name: &'a str,
        shndx: u16,
        value: u64,
        size: u64,
        info: u8,
    }

    fn le16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn le32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn le64(v: &mut Vec<u8>, x: u64) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    /// One section header: `[name, type, link, info]` and `[flags, addr, offset, size, entsize]`.
    fn shdr(v: &mut Vec<u8>, small: [u32; 4], big: [u64; 5]) {
        le32(v, small[0]);
        le32(v, small[1]);
        le64(v, big[0]);
        le64(v, big[1]);
        le64(v, big[2]);
        le64(v, big[3]);
        le32(v, small[2]);
        le32(v, small[3]);
        le64(v, 8);
        le64(v, big[4]);
    }

    /// Sections: 1 `.text` (exec, 0x100), 2 `.rodata` (0x40), 3 `.data` (write, 0x20), 4 `.bss` (nobits, 0x80),
    /// 5 `.symtab`, 6 `.strtab`, 7 `.shstrtab`.
    fn build_elf(syms: &[Sym<'_>]) -> Vec<u8> {
        let mut strtab = vec![0_u8];
        let mut symtab = vec![0_u8; 24];
        for s in syms {
            let off = strtab.len() as u32;
            strtab.extend_from_slice(s.name.as_bytes());
            strtab.push(0);
            le32(&mut symtab, off);
            symtab.push(s.info);
            symtab.push(0);
            le16(&mut symtab, s.shndx);
            le64(&mut symtab, s.value);
            le64(&mut symtab, s.size);
        }
        let shstr_names = [
            "",
            ".text",
            ".rodata",
            ".data",
            ".bss",
            ".symtab",
            ".strtab",
            ".shstrtab",
        ];
        let mut shstrtab = Vec::new();
        let mut name_off = Vec::new();
        for n in shstr_names {
            name_off.push(shstrtab.len() as u32);
            shstrtab.extend_from_slice(n.as_bytes());
            shstrtab.push(0);
        }
        let mut file = vec![0_u8; 64];
        let text_off = file.len() as u64;
        file.extend(std::iter::repeat_n(0x90, 0x100));
        let ro_off = file.len() as u64;
        file.extend(std::iter::repeat_n(1, 0x40));
        let data_off = file.len() as u64;
        file.extend(std::iter::repeat_n(2, 0x20));
        let sym_off = file.len() as u64;
        file.extend_from_slice(&symtab);
        let str_off = file.len() as u64;
        file.extend_from_slice(&strtab);
        let shstr_off = file.len() as u64;
        file.extend_from_slice(&shstrtab);
        while !file.len().is_multiple_of(8) {
            file.push(0);
        }
        let sh_off = file.len() as u64;
        shdr(&mut file, [0, 0, 0, 0], [0, 0, 0, 0, 0]);
        shdr(
            &mut file,
            [name_off[1], 1, 0, 0],
            [0x6, 0x1000, text_off, 0x100, 0],
        );
        shdr(&mut file, [name_off[2], 1, 0, 0], [0x2, 0x2000, ro_off, 0x40, 0]);
        shdr(
            &mut file,
            [name_off[3], 1, 0, 0],
            [0x3, 0x3000, data_off, 0x20, 0],
        );
        shdr(
            &mut file,
            [name_off[4], 8, 0, 0],
            [0x3, 0x4000, data_off + 0x20, 0x80, 0],
        );
        shdr(
            &mut file,
            [name_off[5], 2, 6, 1],
            [0, 0, sym_off, symtab.len() as u64, 24],
        );
        shdr(
            &mut file,
            [name_off[6], 3, 0, 0],
            [0, 0, str_off, strtab.len() as u64, 0],
        );
        shdr(
            &mut file,
            [name_off[7], 3, 0, 0],
            [0, 0, shstr_off, shstrtab.len() as u64, 0],
        );
        // ELF header.
        let mut h = Vec::new();
        h.extend_from_slice(b"\x7fELF\x02\x01\x01\x00");
        h.extend_from_slice(&[0; 8]);
        le16(&mut h, 3); // ET_DYN
        le16(&mut h, 62); // x86_64
        le32(&mut h, 1);
        le64(&mut h, 0); // entry
        le64(&mut h, 0); // phoff
        le64(&mut h, sh_off);
        le32(&mut h, 0);
        le16(&mut h, 64);
        le16(&mut h, 56);
        le16(&mut h, 0);
        le16(&mut h, 64);
        le16(&mut h, 8);
        le16(&mut h, 7);
        assert_eq!(h.len(), 64);
        file[..64].copy_from_slice(&h);
        file
    }

    const FUNC: u8 = 0x12; // global FUNC
    const OBJ: u8 = 0x11; // global OBJECT

    fn sym(name: &str, shndx: u16, value: u64, size: u64, info: u8) -> Sym<'_> {
        Sym {
            name,
            shndx,
            value,
            size,
            info,
        }
    }

    #[test]
    fn legacy_names_demangle_without_the_hash() {
        assert_eq!(
            demangle("_ZN4core3ptr13drop_in_place17h0123456789abcdefE").as_deref(),
            Some("core::ptr::drop_in_place")
        );
        assert_eq!(
            demangle("_ZN56_$LT$regia_render..Frame$u20$as$u20$core..fmt..Debug$GT$3fmt17h0123456789abcdefE")
                .as_deref(),
            Some("<regia_render::Frame as core::fmt::Debug>::fmt")
        );
        assert_eq!(demangle("memcpy"), None);
        assert_eq!(demangle("_ZN4core"), None);
    }

    #[test]
    fn crates_come_from_the_path_and_impl_headers() {
        assert_eq!(
            crate_of("regia_render::pass::draw").as_deref(),
            Some("regia_render")
        );
        assert_eq!(
            crate_of("<regia_render::Frame as core::fmt::Debug>::fmt").as_deref(),
            Some("regia_render")
        );
        assert_eq!(
            crate_of("<&alloc::vec::Vec<u8> as core::fmt::Debug>::fmt").as_deref(),
            Some("alloc")
        );
        assert_eq!(crate_of("<u8 as core::fmt::Debug>::fmt").as_deref(), Some("core"));
        assert_eq!(crate_of("main"), None);
    }

    #[test]
    fn v0_symbols_give_the_crate() {
        assert_eq!(
            demangle_v0("_RNvCs1234_7regia_x4draw").as_deref(),
            Some("regia_x::draw")
        );
        assert_eq!(demangle_v0("_RNvC4core3foo").as_deref(), Some("core::foo"));
    }

    #[test]
    fn v0_generics_are_elided_and_backrefs_resolve() {
        assert_eq!(
            demangle_v0("_RINvNtC7mycrate3foo3barjmE").as_deref(),
            Some("mycrate::foo::bar<…>")
        );
        assert_eq!(
            demangle_v0("_RINvNtC7mycrate3foo3barINtC4core3VecmEE").as_deref(),
            Some("mycrate::foo::bar<…>")
        );
        assert_eq!(
            demangle_v0("_RNvXCs1234_7mycratejNtB2_5Trait3foo").as_deref(),
            Some("<usize as mycrate::Trait>::foo")
        );
        assert_eq!(demangle_v0("_RNvB9_3foo"), None);
    }

    #[test]
    fn section_totals_and_symbol_attribution_match_the_fixture() {
        let elf = build_elf(&[
            sym(
                "_ZN4core3ptr13drop_in_place17haaaaaaaaaaaaaaaaE",
                1,
                0x1000,
                0x30,
                FUNC,
            ),
            sym(
                "_ZN4core3ptr13drop_in_place17hbbbbbbbbbbbbbbbbE",
                1,
                0x1030,
                0x20,
                FUNC,
            ),
            sym(
                "_ZN4core3ptr13drop_in_place17hccccccccccccccccE",
                1,
                0x1050,
                0x10,
                FUNC,
            ),
            sym("_ZN11regia_audio3mix17hddddddddddddddddE", 1, 0x1060, 0x80, FUNC),
            // An alias of the same address and size: counted once.
            sym(
                "_ZN11regia_audio9mix_alias17heeeeeeeeeeeeeeeeE",
                1,
                0x1060,
                0x80,
                FUNC,
            ),
            sym("TABLE", 2, 0x2000, 0x10, OBJ),
            // Section symbol, undefined symbol, zero size: ignored.
            sym("_ZN9regia_gpu4none17hffffffffffffffffE", 0, 0, 0x99, FUNC),
            sym("_ZN9regia_gpu4zero17h1111111111111111E", 1, 0x10f0, 0, FUNC),
        ]);
        let a = analyze(&elf, false).expect("analyze");
        assert_eq!(
            a.sections,
            Sections {
                text: 0x100,
                rodata: 0x40,
                data: 0x20,
                bss: 0x80
            }
        );
        assert_eq!(a.source, SymbolSource::Symtab);
        let r = attribute(&a, "t", "x86_64-unknown-linux-gnu", 30);
        assert_eq!(r.total_bytes, 0x100 + 0x40 + 0x20 + 0x80);
        let by: HashMap<&str, (u64, u64)> = r
            .crates
            .iter()
            .map(|c| (c.name.as_str(), (c.bytes, c.symbols)))
            .collect();
        assert_eq!(by["core"], (0x60, 3));
        assert_eq!(by["regia_audio"].0, 0x80);
        assert_eq!(by[NATIVE], (0x10, 1));
        // Unattributed = loaded - named, on a line of its own.
        let named: u64 = 0x60 + 0x80 + 0x10;
        assert_eq!(by[UNATTRIBUTED].0, r.total_bytes - named);
        // Biggest crate row after the no-symbol line.
        assert_eq!(r.top_symbols[0].bytes, 0x80);
        // Three copies of drop_in_place are one generic row; the single-copy mix is not listed.
        assert_eq!(r.generics.len(), 1);
        assert_eq!(r.generics[0].path, "core::ptr::drop_in_place");
        assert_eq!(r.generics[0].instantiations, 3);
        assert_eq!(r.generics[0].bytes, 0x60);
    }

    #[test]
    fn a_binary_without_symbols_is_all_unattributed() {
        let elf = build_elf(&[]);
        let a = analyze(&elf, false).expect("analyze");
        assert_eq!(a.source, SymbolSource::None);
        let r = attribute(&a, "t", "x", 30);
        assert_eq!(r.crates.len(), 1);
        assert_eq!(r.crates[0].name, UNATTRIBUTED);
        assert_eq!(r.crates[0].bytes, r.total_bytes);
    }

    #[test]
    fn unknown_formats_are_refused_and_pe_is_sections_only() {
        assert!(analyze(b"not a binary at all", false).is_err());
        assert!(analyze(b"MZ", false).is_err());
    }

    fn report_with(total: u64, crates: &[(&str, u64)]) -> Report {
        Report {
            template: "t".into(),
            target: "x".into(),
            format: "elf",
            file_bytes: total,
            sections: Sections {
                text: total,
                ..Sections::default()
            },
            total_bytes: total,
            symbol_source: SymbolSource::Symtab,
            unloaded_bytes: 0,
            crates: crates
                .iter()
                .map(|(n, b)| CrateRow {
                    name: (*n).to_owned(),
                    bytes: *b,
                    symbols: 1,
                })
                .collect(),
            top_symbols: Vec::new(),
            generics: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn the_budget_passes_inside_the_tolerance_and_fails_above_it() {
        let base = report_with(10_000_000, &[("a", 4_000_000), ("b", 1_000_000)]);
        let budget = record(&base, 5);
        assert!(check_budget(&base, &budget).0.is_empty());
        // +4 % total, +4 % crate: inside.
        let ok = report_with(10_400_000, &[("a", 4_160_000), ("b", 1_000_000)]);
        assert!(check_budget(&ok, &budget).0.is_empty());
        // +6 % total fails, and so does a crate +20 %.
        let big = report_with(10_600_000, &[("a", 4_800_000), ("b", 1_000_000)]);
        let (fails, _) = check_budget(&big, &budget);
        assert_eq!(fails.len(), 2, "{fails:?}");
        assert!(fails.iter().any(|f| f.starts_with("total")));
        assert!(fails.iter().any(|f| f.contains("crate `a`")));
    }

    #[test]
    fn a_new_big_crate_fails_and_a_big_gain_hints_at_update() {
        let base = report_with(10_000_000, &[("a", 4_000_000)]);
        let budget = record(&base, 5);
        let with_new = report_with(10_000_000, &[("a", 4_000_000), ("newcomer", 200_000)]);
        let (fails, _) = check_budget(&with_new, &budget);
        assert_eq!(fails.len(), 1);
        assert!(fails[0].contains("newcomer"));
        let tiny_new = report_with(10_000_000, &[("a", 4_000_000), ("tiny", 1_000)]);
        assert!(check_budget(&tiny_new, &budget).0.is_empty());
        let smaller = report_with(8_000_000, &[("a", 3_000_000)]);
        let (fails, hints) = check_budget(&smaller, &budget);
        assert!(fails.is_empty());
        assert_eq!(hints.len(), 1);
    }

    #[test]
    fn budget_files_round_trip_and_upsert_replaces_a_row() {
        let dir = regia_test_utils::unique_temp_dir("binary-size");
        let path = dir.join("size-budgets.toml");
        assert_eq!(
            load_budgets(&path).expect("missing is empty"),
            BudgetFile::default()
        );
        let mut file = BudgetFile::default();
        upsert(&mut file, record(&report_with(100, &[("a", 60)]), 3));
        upsert(&mut file, record(&report_with(200, &[("a", 90)]), 3));
        assert_eq!(file.budget.len(), 1);
        save_budgets(&path, &file).expect("save");
        let back = load_budgets(&path).expect("load");
        assert_eq!(back, file);
        assert_eq!(back.budget[0].total_bytes, 200);
        std::fs::write(&path, "format_version = 2\n").expect("write");
        assert!(load_budgets(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_build_plan_covers_host_android_and_windows() {
        let host = build_plan("blank", "x86_64-unknown-linux-gnu", "tpl_check");
        assert_eq!(
            &host.cargo_args[..6],
            [
                "build",
                "--target",
                "x86_64-unknown-linux-gnu",
                "--profile",
                "shipping",
                "-p"
            ]
        );
        assert_eq!(host.cargo_args[6], "tpl_check_launcher");
        assert_eq!(
            host.output,
            Path::new("x86_64-unknown-linux-gnu/shipping/tpl_check")
        );
        let android = build_plan("blank", "aarch64-linux-android", "tpl_check");
        assert_eq!(
            &android.cargo_args[..6],
            ["ndk", "-t", "arm64-v8a", "-P", "26", "build"]
        );
        assert!(android.cargo_args.contains(&"tpl_check_android".to_owned()));
        assert_eq!(
            android.output,
            Path::new("aarch64-linux-android/shipping/libmain.so")
        );
        let win = build_plan("quick", "x86_64-pc-windows-msvc", "tpl_check");
        assert_eq!(&win.cargo_args[..2], ["xwin", "build"]);
        assert_eq!(
            win.output,
            Path::new("x86_64-pc-windows-msvc/shipping/tpl_check.exe")
        );
    }
}
