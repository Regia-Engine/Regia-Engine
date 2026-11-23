// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask minidump <file>`: reads the minidump of a native crash bundle (or a bare `.dmp`)
//! and prints the crash reason and the crashing thread's stack (ADR-158).
//!
//! `rust-minidump` (`minidump` + `minidump-processor`) parses the dump and walks the stack (frame
//! pointers, CFI where the dump has it, stack scanning otherwise). Frames are then resolved to
//! function, file and line with `addr2line` (binutils, the same tool `regia package` already
//! needs for `objcopy`) against the release's split debug symbols: `regia package` writes
//! `symbols/<binary>.debug` for every shipped binary, and that directory is what `--symbols`
//! points at. Without it, an unstripped binary at the path recorded in the dump (a local dev
//! build) works too. The tool only reads; nothing is uploaded.
//!
//! Dev tool only: none of these crates is a runtime dependency.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use minidump::{Minidump, Module};
use minidump_processor::ProcessState;
use minidump_unwind::{Symbolizer, simple_symbol_supplier};

/// One stack frame of the crashing thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The instruction address in the crashed process.
    pub address: u64,
    /// The module's file name (`do27`, `libvulkan_radeon.so`…), empty when outside any module.
    pub module: String,
    /// The address relative to the module's load address.
    pub offset: u64,
    /// `function at file:line` when symbols resolved it.
    pub symbol: Option<String>,
    /// How the stack walker found the frame (`context`, `cfi`, `frame pointer`, `scan`).
    pub trust: String,
}

/// What `cargo xtask minidump` prints.
#[derive(Debug, Clone)]
pub struct CrashReport {
    /// `SIGSEGV / SEGV_MAPERR`…
    pub reason: String,
    /// The faulting address.
    pub address: u64,
    /// The crashing thread's id.
    pub thread_id: u32,
    /// The crashing thread's name, when the dump has it.
    pub thread_name: Option<String>,
    /// The crashing thread's frames, innermost first.
    pub frames: Vec<Frame>,
    /// Main module: file and build id (to check the symbols belong to this build).
    pub main_module: Option<(String, String)>,
    /// Number of threads in the dump.
    pub threads: usize,
}

impl CrashReport {
    /// The human-readable report.
    #[must_use]
    pub fn text(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "crash: {} at {:#x}", self.reason, self.address);
        let name = self.thread_name.as_deref().unwrap_or("<unnamed>");
        let _ = writeln!(
            s,
            "thread: {name} (tid {}), {} threads in the dump",
            self.thread_id, self.threads
        );
        if let Some((file, id)) = &self.main_module {
            let _ = writeln!(s, "main module: {file} (build id {id})");
        }
        let _ = writeln!(s, "\ncrashing thread stack:");
        for (i, f) in self.frames.iter().enumerate() {
            let module = if f.module.is_empty() {
                "?".to_owned()
            } else {
                format!("{}+{:#x}", f.module, f.offset)
            };
            let symbol = f.symbol.as_deref().unwrap_or("");
            let _ = writeln!(
                s,
                "#{i:<3} {:#018x} {module:<40} {symbol}  [{}]",
                f.address, f.trust
            );
        }
        s
    }
}

/// The minidump bytes of `path`: a `.dmp`, or a feedback bundle `.zip` holding `minidump.dmp`.
///
/// # Errors
/// Unreadable file, or a bundle without a minidump.
pub fn load_dump_bytes(path: &Path) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.starts_with(b"PK") {
        let entries = regia_framework::feedback::read_bundle(path).map_err(anyhow::Error::msg)?;
        let Some(entry) = entries.into_iter().find(|e| e.name == "minidump.dmp") else {
            bail!(
                "{} has no minidump.dmp (a panic bundle? its crash.txt holds the backtrace)",
                path.display()
            );
        };
        return Ok(entry.data);
    }
    Ok(bytes)
}

/// Runs a future to completion on this thread (`minidump-processor` is async; nothing here
/// waits on IO, so no executor is needed).
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context as TaskContext, Poll, Wake, Waker};
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = TaskContext::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        std::thread::park();
    }
}

/// Parses `bytes` and walks the crashing thread's stack, resolving frames against `symbols`
/// (a directory of `<module>.debug` files) or the module files themselves when they exist.
///
/// # Errors
/// Not a minidump, or no crashing thread in it.
pub fn analyze(bytes: Vec<u8>, symbols: Option<&Path>) -> Result<CrashReport> {
    let dump = Minidump::read(bytes).map_err(|e| anyhow::anyhow!("not a minidump: {e}"))?;
    let provider = Symbolizer::new(simple_symbol_supplier(Vec::new()));
    let state: ProcessState = block_on(minidump_processor::process_minidump(&dump, &provider))
        .map_err(|e| anyhow::anyhow!("could not process the minidump: {e}"))?;
    let exception = state.exception_info.as_ref();
    let index = state
        .requesting_thread
        .context("the minidump names no crashing thread")?;
    let stack = state.threads.get(index).context("crashing thread missing")?;
    let mut frames: Vec<Frame> = stack
        .frames
        .iter()
        .map(|f| {
            let (module, offset) = f.module.as_ref().map_or((String::new(), 0), |m| {
                (
                    file_name(&m.code_file()),
                    f.instruction.saturating_sub(m.base_address()),
                )
            });
            Frame {
                address: f.instruction,
                module,
                offset,
                symbol: f.function_name.clone(),
                trust: f.trust.as_str().to_owned(),
            }
        })
        .collect();
    let module_files: BTreeMap<String, PathBuf> = stack
        .frames
        .iter()
        .filter_map(|f| f.module.as_ref())
        .map(|m| (file_name(&m.code_file()), PathBuf::from(m.code_file().as_ref())))
        .collect();
    symbolize(&mut frames, symbols, &module_files);
    let main_module = state.modules.main_module().map(|m| {
        (
            m.code_file().into_owned(),
            m.code_identifier()
                .map_or_else(|| "unknown".to_owned(), |id| id.to_string()),
        )
    });
    Ok(CrashReport {
        reason: exception.map_or_else(|| "no exception record".to_owned(), |e| e.reason.to_string()),
        address: exception.map_or(0, |e| e.address.0),
        thread_id: stack.thread_id,
        thread_name: stack.thread_name.clone(),
        frames,
        main_module,
        threads: state.threads.len(),
    })
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned())
}

/// Resolves frames with `addr2line`, one call per module. Module offsets are used as addresses,
/// which is right for position-independent binaries (Rust's default on Linux) and shared
/// libraries, whose first load segment starts at 0.
fn symbolize(frames: &mut [Frame], symbols: Option<&Path>, module_files: &BTreeMap<String, PathBuf>) {
    let mut by_module: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, f) in frames.iter().enumerate() {
        if !f.module.is_empty() {
            by_module.entry(f.module.clone()).or_default().push(i);
        }
    }
    for (module, indices) in by_module {
        let candidates = [
            symbols.map(|d| d.join(format!("{module}.debug"))),
            symbols.map(|d| d.join(&module)),
            module_files.get(&module).cloned(),
        ];
        let Some(file) = candidates.into_iter().flatten().find(|p| p.is_file()) else {
            continue;
        };
        let mut cmd = Command::new("addr2line");
        cmd.arg("-C").arg("-f").arg("-i").arg("-a").arg("-e").arg(&file);
        for &i in &indices {
            cmd.arg(format!("{:#x}", frames[i].offset));
        }
        let Ok(out) = cmd.output() else {
            eprintln!("note: `addr2line` (binutils) is not installed; frames stay unresolved");
            return;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        // `-a` prints the address, then (function, file:line) pairs: more than one when inlined.
        let mut groups: Vec<Vec<String>> = Vec::new();
        for line in text.lines() {
            if line.starts_with("0x") && line.len() > 2 && line[2..].chars().all(|c| c.is_ascii_hexdigit()) {
                groups.push(Vec::new());
            } else if let Some(g) = groups.last_mut() {
                g.push(line.to_owned());
            }
        }
        for (&i, group) in indices.iter().zip(groups) {
            let pairs: Vec<String> = group
                .chunks(2)
                .filter(|p| p.first().is_some_and(|f| f != "??"))
                .map(|p| format!("{} at {}", p[0], p.get(1).map_or("??", String::as_str)))
                .collect();
            if !pairs.is_empty() {
                frames[i].symbol = Some(pairs.join(" <- inlined into "));
            }
        }
    }
}

/// `cargo xtask minidump`.
///
/// # Errors
/// Unreadable or invalid dump.
pub fn run(path: &Path, symbols: Option<&Path>) -> Result<std::process::ExitCode> {
    let report = analyze(load_dump_bytes(path)?, symbols)?;
    print!("{}", report.text());
    if report
        .main_module
        .as_ref()
        .is_some_and(|(file, _)| file.to_lowercase().ends_with(".exe"))
    {
        println!(
            "\nnote: this is a Windows dump. `addr2line` cannot read PDBs, so frames stay module+offset. For \
             function names convert the release's `symbols/<name>.pdb` with `dump_syms` and walk the dump \
             with `minidump-stackwalk --symbols-path` (see the Feedback chapter, \"Reading a Windows minidump\")"
        );
    } else if symbols.is_none() {
        println!(
            "\nnote: pass --symbols <package>/symbols (the release's `symbols/<binary>.debug`) to resolve \
             shipped, stripped builds"
        );
    }
    Ok(std::process::ExitCode::SUCCESS)
}
