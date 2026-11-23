// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask coverage --crate <name>... [--json <out>] [--lowest N]`: measured code coverage
//! (ADR-2705, `docs/06-testing-and-ci.md` § 15).
//!
//! The command wraps `cargo llvm-cov test` for the named crates, reads the export JSON and prints per-crate line
//! and function coverage plus the N lowest-covered public functions (the export's function table joined with the
//! `pub fn` items found by a source scan of each crate). `cargo-llvm-cov` is a tool, not a dependency: it is
//! installed lane-private (`CARGO_INSTALL_ROOT=<tools> cargo install cargo-llvm-cov --locked`) and needs the
//! `llvm-tools-preview` component (use a private `RUSTUP_HOME`, never a shared one). Instrumented builds use their
//! own target directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// Arguments of `cargo xtask coverage`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Crate to measure (repeatable); the tests of these crates run instrumented.
    #[arg(long = "crate", required = true, num_args = 1..)]
    pub crates: Vec<String>,
    /// Also write the summary as JSON to this file.
    #[arg(long)]
    pub json: Option<PathBuf>,
    /// How many of the lowest-covered public functions to list.
    #[arg(long, default_value_t = 20)]
    pub lowest: usize,
    /// Directory holding `bin/cargo-llvm-cov` (default `<repo>/tools`, falling back to `PATH`).
    #[arg(long)]
    pub tools: Option<PathBuf>,
    /// Instrumented target directory (default `<repo>/target-coverage`).
    #[arg(long)]
    pub target_dir: Option<PathBuf>,
    /// Read an existing llvm-cov export JSON instead of running the tests.
    #[arg(long)]
    pub from_export: Option<PathBuf>,
}

/// Line or function totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ratio {
    /// Items in the measured code.
    pub count: u64,
    /// Items executed at least once.
    pub covered: u64,
}

impl Ratio {
    /// Percent covered, 100 when nothing is countable.
    pub fn percent(&self) -> f64 {
        if self.count == 0 {
            100.0
        } else {
            self.covered as f64 * 100.0 / self.count as f64
        }
    }
}

/// Totals of one crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrateTotals {
    /// Line totals.
    pub lines: Ratio,
    /// Function totals.
    pub functions: Ratio,
}

/// One function instance of the export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnRecord {
    /// Crate the source file belongs to.
    pub krate: String,
    /// Source file, relative to the crate when possible.
    pub file: String,
    /// Last path segment of the (demangled) name.
    pub name: String,
    /// Largest execution count over the instances.
    pub count: u64,
    /// Code regions (a size proxy).
    pub regions: usize,
}

/// A parsed export.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Per-crate totals, by crate name.
    pub crates: BTreeMap<String, CrateTotals>,
    /// Function instances in the measured crates.
    pub functions: Vec<FnRecord>,
}

/// The crate named by a path containing `crates/<name>/src/`, if any.
pub fn crate_of(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    let rest = path.split("crates/").nth(1)?;
    let (name, tail) = rest.split_once('/')?;
    tail.starts_with("src/").then(|| name.to_string())
}

/// The last path segment of a function symbol: handles `a::b::name`, `a::b::name::h0123456789abcdef`,
/// `<T as Trait>::name`, legacy `_ZN..E` manglings and the v0 mangling (`_R..`, which is what current
/// `rustc` emits). A closure is `{closure}`; an unparsable symbol falls back to its text.
pub fn short_name(symbol: &str) -> String {
    if symbol.starts_with("_R") {
        if let Some(name) = V0::new(symbol).and_then(|mut p| p.outer_name()) {
            return name;
        }
        return symbol.to_string();
    }
    if let Some(rest) = symbol.strip_prefix("_ZN") {
        let mut segs = Vec::new();
        let mut s = rest;
        while let Some(pos) = s.find(|c: char| !c.is_ascii_digit()) {
            if pos == 0 {
                break;
            }
            let Ok(n) = s[..pos].parse::<usize>() else { break };
            let body = &s[pos..];
            if body.len() < n {
                break;
            }
            segs.push(&body[..n]);
            s = &body[n..];
        }
        if let Some(last) = segs.iter().rev().find(|seg| !is_hash(seg)) {
            return (*last).to_string();
        }
    }
    let mut parts: Vec<&str> = symbol.split("::").collect();
    while parts.len() > 1 && is_hash(parts[parts.len() - 1]) {
        parts.pop();
    }
    let last = parts.last().copied().unwrap_or(symbol);
    last.split('<').next().unwrap_or(last).trim().to_string()
}

/// A reader of the v0 symbol grammar (the Rust RFC 2603 manglings) that finds the name of the outermost
/// function and skips everything else. It never panics: every step is bounds checked.
struct V0<'a> {
    s: &'a [u8],
    pos: usize,
    depth: u32,
}

impl<'a> V0<'a> {
    fn new(symbol: &'a str) -> Option<Self> {
        let rest = symbol.strip_prefix("_R")?;
        let s = rest.as_bytes();
        // An optional decimal version comes first.
        let mut pos = 0;
        while s.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        Some(Self { s, pos, depth: 0 })
    }

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

    fn base62(&mut self) -> Option<()> {
        while let Some(c) = self.peek() {
            self.pos += 1;
            if c == b'_' {
                return Some(());
            }
            if !c.is_ascii_alphanumeric() {
                return None;
            }
        }
        None
    }

    fn decimal(&mut self) -> Option<usize> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        std::str::from_utf8(&self.s[start..self.pos]).ok()?.parse().ok()
    }

    fn ident(&mut self) -> Option<String> {
        if self.eat(b's') {
            self.base62()?;
        }
        self.eat(b'u');
        let len = self.decimal()?;
        self.eat(b'_');
        let end = self.pos.checked_add(len)?;
        let bytes = self.s.get(self.pos..end)?;
        self.pos = end;
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    fn enter(&mut self) -> Option<()> {
        self.depth += 1;
        (self.depth < 200).then_some(())
    }

    /// The name of the outermost nested path (`N`), looking through generic instantiation (`I`).
    fn outer_name(&mut self) -> Option<String> {
        match self.peek()? {
            b'N' => {
                self.pos += 1;
                let ns = self.peek()?;
                self.pos += 1;
                self.skip_path()?;
                let name = self.ident()?;
                Some(if ns == b'C' { "{closure}".to_string() } else { name })
            }
            b'I' => {
                self.pos += 1;
                self.outer_name()
            }
            _ => None,
        }
    }

    fn skip_path(&mut self) -> Option<()> {
        self.enter()?;
        let r = self.skip_path_inner();
        self.depth -= 1;
        r
    }

    fn skip_path_inner(&mut self) -> Option<()> {
        let c = self.peek()?;
        self.pos += 1;
        match c {
            b'C' => self.ident().map(drop),
            b'M' => {
                if self.eat(b's') {
                    self.base62()?;
                }
                self.skip_path()?;
                self.skip_type()
            }
            b'X' => {
                if self.eat(b's') {
                    self.base62()?;
                }
                self.skip_path()?;
                self.skip_type()?;
                self.skip_path()
            }
            b'Y' => {
                self.skip_type()?;
                self.skip_path()
            }
            b'N' => {
                self.pos += 1;
                self.skip_path()?;
                self.ident().map(drop)
            }
            b'I' => {
                self.skip_path()?;
                while self.peek()? != b'E' {
                    self.skip_generic_arg()?;
                }
                self.pos += 1;
                Some(())
            }
            b'B' => self.base62(),
            _ => None,
        }
    }

    fn skip_generic_arg(&mut self) -> Option<()> {
        match self.peek()? {
            b'L' => {
                self.pos += 1;
                self.base62()
            }
            b'K' => {
                self.pos += 1;
                self.skip_const()
            }
            _ => self.skip_type(),
        }
    }

    fn skip_const(&mut self) -> Option<()> {
        match self.peek()? {
            b'p' => {
                self.pos += 1;
                Some(())
            }
            b'B' => {
                self.pos += 1;
                self.base62()
            }
            _ => {
                self.skip_type()?;
                self.eat(b'n');
                while self.peek()?.is_ascii_hexdigit() {
                    self.pos += 1;
                }
                self.eat(b'_').then_some(())
            }
        }
    }

    fn skip_lifetime_opt(&mut self) -> Option<()> {
        if self.eat(b'L') { self.base62() } else { Some(()) }
    }

    fn skip_type(&mut self) -> Option<()> {
        self.enter()?;
        let r = self.skip_type_inner();
        self.depth -= 1;
        r
    }

    fn skip_type_inner(&mut self) -> Option<()> {
        let c = self.peek()?;
        if c.is_ascii_lowercase() {
            self.pos += 1;
            return Some(());
        }
        match c {
            b'C' | b'M' | b'X' | b'Y' | b'N' | b'I' | b'B' => self.skip_path(),
            b'A' => {
                self.pos += 1;
                self.skip_type()?;
                self.skip_const()
            }
            b'S' | b'P' | b'O' => {
                self.pos += 1;
                self.skip_type()
            }
            b'T' => {
                self.pos += 1;
                while self.peek()? != b'E' {
                    self.skip_type()?;
                }
                self.pos += 1;
                Some(())
            }
            b'R' | b'Q' => {
                self.pos += 1;
                self.skip_lifetime_opt()?;
                self.skip_type()
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
                while self.peek()? != b'E' {
                    self.skip_type()?;
                }
                self.pos += 1;
                self.skip_type()
            }
            b'D' => {
                self.pos += 1;
                if self.eat(b'G') {
                    self.base62()?;
                }
                while self.peek()? != b'E' {
                    self.skip_path()?;
                    while self.eat(b'p') {
                        self.ident()?;
                        self.skip_type()?;
                    }
                }
                self.pos += 1;
                if self.eat(b'L') { self.base62() } else { Some(()) }
            }
            _ => None,
        }
    }
}

fn is_hash(seg: &str) -> bool {
    seg.len() == 17 && seg.starts_with('h') && seg[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn ratio(v: &Value) -> Ratio {
    Ratio {
        count: v.get("count").and_then(Value::as_u64).unwrap_or(0),
        covered: v.get("covered").and_then(Value::as_u64).unwrap_or(0),
    }
}

/// Parses an llvm-cov export (`cargo llvm-cov --json`), keeping the crates in `wanted` (all when empty).
pub fn parse_export(text: &str, wanted: &[String]) -> Result<Report> {
    let doc: Value = serde_json::from_str(text).context("coverage export is not JSON")?;
    let data = doc
        .get("data")
        .and_then(Value::as_array)
        .context("export has no `data` array")?;
    let keep = |c: &str| wanted.is_empty() || wanted.iter().any(|w| w == c);
    let mut report = Report::default();
    for block in data {
        for file in block.get("files").and_then(Value::as_array).into_iter().flatten() {
            let name = file.get("filename").and_then(Value::as_str).unwrap_or("");
            let Some(krate) = crate_of(name) else { continue };
            if !keep(&krate) {
                continue;
            }
            let summary = file.get("summary");
            let entry = report.crates.entry(krate).or_default();
            if let Some(l) = summary.and_then(|s| s.get("lines")) {
                let r = ratio(l);
                entry.lines.count += r.count;
                entry.lines.covered += r.covered;
            }
            if let Some(f) = summary.and_then(|s| s.get("functions")) {
                let r = ratio(f);
                entry.functions.count += r.count;
                entry.functions.covered += r.covered;
            }
        }
        for func in block
            .get("functions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let file = func
                .get("filenames")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str);
            let Some(file) = file else { continue };
            let Some(krate) = crate_of(file) else { continue };
            if !keep(&krate) {
                continue;
            }
            let symbol = func.get("name").and_then(Value::as_str).unwrap_or("");
            let count = func.get("count").and_then(Value::as_u64).unwrap_or(0);
            let regions = func.get("regions").and_then(Value::as_array).map_or(0, Vec::len);
            let name = short_name(symbol);
            let rel = file.replace('\\', "/");
            let rel = rel
                .split_once(&format!("crates/{krate}/"))
                .map_or(rel.clone(), |(_, t)| t.to_string());
            // Merge generic instances of one function in one file.
            if let Some(prev) = report
                .functions
                .iter_mut()
                .find(|f| f.krate == krate && f.file == rel && f.name == name)
            {
                prev.count = prev.count.max(count);
                prev.regions = prev.regions.max(regions);
            } else {
                report.functions.push(FnRecord {
                    krate,
                    file: rel,
                    name,
                    count,
                    regions,
                });
            }
        }
    }
    Ok(report)
}

/// Names of the `pub fn` items in a source text (free functions and methods; not `pub(crate)`).
pub fn scan_public_fns(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in source.lines() {
        let t = line.trim_start();
        let Some(mut rest) = t.strip_prefix("pub ") else {
            continue;
        };
        loop {
            let before = rest;
            for q in ["const ", "async ", "unsafe ", "extern \"C\" "] {
                if let Some(r) = rest.strip_prefix(q) {
                    rest = r;
                }
            }
            if before == rest {
                break;
            }
        }
        let Some(rest) = rest.strip_prefix("fn ") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            out.push(name);
        }
    }
    out
}

/// A public function with its coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicFn {
    /// Crate name.
    pub krate: String,
    /// Source file, relative to the crate.
    pub file: String,
    /// Function name.
    pub name: String,
    /// Execution count (0 = never run).
    pub count: u64,
    /// Code regions.
    pub regions: usize,
}

/// Joins the export's functions with the public names per (crate, file) and returns the lowest `n`
/// (never-run first, then by count, larger functions first).
pub fn lowest_public(
    report: &Report,
    public: &BTreeMap<(String, String), Vec<String>>,
    n: usize,
) -> Vec<PublicFn> {
    let mut rows: Vec<PublicFn> = report
        .functions
        .iter()
        .filter(|f| {
            public
                .get(&(f.krate.clone(), f.file.clone()))
                .is_some_and(|names| names.contains(&f.name))
        })
        .map(|f| PublicFn {
            krate: f.krate.clone(),
            file: f.file.clone(),
            name: f.name.clone(),
            count: f.count,
            regions: f.regions,
        })
        .collect();
    rows.sort_by(|a, b| {
        a.count
            .cmp(&b.count)
            .then(b.regions.cmp(&a.regions))
            .then(a.krate.cmp(&b.krate))
            .then(a.file.cmp(&b.file))
            .then(a.name.cmp(&b.name))
    });
    rows.truncate(n);
    rows
}

/// The per-crate table as text.
pub fn render_table(report: &Report) -> String {
    let mut s = String::from("crate                  lines            functions\n");
    for (name, t) in &report.crates {
        s.push_str(&format!(
            "{name:<22} {:>6.2}% ({}/{})  {:>6.2}% ({}/{})\n",
            t.lines.percent(),
            t.lines.covered,
            t.lines.count,
            t.functions.percent(),
            t.functions.covered,
            t.functions.count
        ));
    }
    s
}

/// The summary as JSON.
pub fn to_json(report: &Report, lowest: &[PublicFn]) -> Value {
    let crates: Vec<Value> = report
        .crates
        .iter()
        .map(|(name, t)| {
            json!({
                "crate": name,
                "lines": {"count": t.lines.count, "covered": t.lines.covered, "percent": t.lines.percent()},
                "functions": {"count": t.functions.count, "covered": t.functions.covered, "percent": t.functions.percent()},
            })
        })
        .collect();
    let lowest: Vec<Value> = lowest
        .iter()
        .map(|f| json!({"crate": f.krate, "file": f.file, "name": f.name, "count": f.count, "regions": f.regions}))
        .collect();
    json!({"crates": crates, "lowest_public": lowest})
}

fn collect_public(root: &Path, crates: &[String]) -> BTreeMap<(String, String), Vec<String>> {
    let mut map = BTreeMap::new();
    for c in crates {
        let src = root.join("crates").join(c).join("src");
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs")
                    && let Ok(text) = std::fs::read_to_string(&p)
                    && let Ok(rel) = p.strip_prefix(root.join("crates").join(c))
                {
                    map.insert(
                        (c.clone(), rel.to_string_lossy().replace('\\', "/")),
                        scan_public_fns(&text),
                    );
                }
            }
        }
    }
    map
}

/// Runs the measurement and prints the report.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let text = if let Some(path) = &args.from_export {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        let tools = args.tools.clone().unwrap_or_else(|| root.join("tools"));
        let bin = tools.join("bin");
        let mut path_var = std::ffi::OsString::new();
        if bin.is_dir() {
            path_var.push(&bin);
            path_var.push(":");
        }
        path_var.push(std::env::var_os("PATH").unwrap_or_default());
        let target = args
            .target_dir
            .clone()
            .unwrap_or_else(|| root.join("target-coverage"));
        let out = target.join("coverage-export.json");
        std::fs::create_dir_all(&target)?;
        let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        // cargo-llvm-cov puts its instrumented artifacts under `$CARGO_TARGET_DIR/llvm-cov-target`.
        cmd.current_dir(root)
            .env("PATH", &path_var)
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_TARGET_DIR", &target);
        cmd.args(["llvm-cov", "test", "--json", "--output-path"])
            .arg(&out);
        for c in &args.crates {
            cmd.args(["-p", c]);
        }
        let status = cmd
            .status()
            .context("running cargo llvm-cov (is cargo-llvm-cov installed? see docs/06 § 15)")?;
        if !status.success() {
            bail!("cargo llvm-cov failed: {status}");
        }
        std::fs::read_to_string(&out).with_context(|| format!("reading {}", out.display()))?
    };
    let report = parse_export(&text, &args.crates)?;
    let public = collect_public(root, &args.crates);
    let lowest = lowest_public(&report, &public, args.lowest);
    print!("{}", render_table(&report));
    println!("\nlowest-covered public functions:");
    for f in &lowest {
        println!(
            "  {:>6} runs {:>4} regions  {}/{}  {}",
            f.count, f.regions, f.krate, f.file, f.name
        );
    }
    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_string_pretty(&to_json(&report, &lowest))?)?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"type":"llvm.coverage.json.export","version":"2.0.1","data":[{
      "files":[
        {"filename":"/w/crates/regia_net/src/seq.rs","summary":{"lines":{"count":10,"covered":5,"percent":50},"functions":{"count":2,"covered":1,"percent":50}}},
        {"filename":"/w/crates/regia_net/src/lib.rs","summary":{"lines":{"count":10,"covered":10,"percent":100},"functions":{"count":1,"covered":1,"percent":100}}},
        {"filename":"/w/crates/regia_core/src/x.rs","summary":{"lines":{"count":4,"covered":0,"percent":0},"functions":{"count":1,"covered":0,"percent":0}}}],
      "functions":[
        {"name":"regia_net::seq::wrap_diff","count":0,"regions":[[1,1,2,2,0,0,0,0],[3,1,4,2,0,0,0,0]],"filenames":["/w/crates/regia_net/src/seq.rs"]},
        {"name":"_ZN9regia_net3seq4next17h0123456789abcdefE","count":7,"regions":[[1,1,2,2,7,0,0,0]],"filenames":["/w/crates/regia_net/src/seq.rs"]},
        {"name":"regia_net::seq::helper","count":0,"regions":[[1,1,2,2,0,0,0,0]],"filenames":["/w/crates/regia_net/src/seq.rs"]},
        {"name":"regia_core::x::other","count":0,"regions":[],"filenames":["/w/crates/regia_core/src/x.rs"]}],
      "totals":{}}]}"#;

    #[test]
    fn crate_of_reads_src_paths_only() {
        assert_eq!(
            crate_of("/w/crates/regia_net/src/a/b.rs").as_deref(),
            Some("regia_net")
        );
        assert_eq!(
            crate_of("C:\\w\\crates\\regia_world\\src\\a.rs").as_deref(),
            Some("regia_world")
        );
        assert_eq!(crate_of("/w/crates/regia_net/tests/a.rs"), None);
        assert_eq!(crate_of("/home/.cargo/registry/src/x/lib.rs"), None);
    }

    #[test]
    fn short_name_handles_manglings() {
        assert_eq!(short_name("regia_net::seq::wrap_diff"), "wrap_diff");
        assert_eq!(short_name("regia_net::seq::next::h0123456789abcdef"), "next");
        assert_eq!(short_name("_ZN9regia_net3seq4next17h0123456789abcdefE"), "next");
        assert_eq!(short_name("regia_net::seq::run<u32>"), "run");
        assert_eq!(short_name("plain"), "plain");
        // v0 manglings as rustc emits them (taken from a real coverage export).
        assert_eq!(
            short_name("_RNvMNtCscCh4ujl5KQn_9regia_net7messageNtB2_9Direction6as_str"),
            "as_str"
        );
        assert_eq!(
            short_name("_RNvMs_NtCscCh4ujl5KQn_9regia_net7messageNtB4_7Channel6as_str"),
            "as_str"
        );
        assert_eq!(
            short_name(
                "_RNCNvXsi_NtCscCh4ujl5KQn_9regia_net5codecINtNtCs1C1lLj4PWSY_5alloc3vec3VectENtB7_14ReplicateCodec7wire_eq0Cs7jVWHlzJnDk_11regia_agent"
            ),
            "{closure}"
        );
        assert_eq!(short_name("_RNvCscCh4ujl5KQn_9regia_net4free"), "free");
        // A generic instantiation reports the function, not its arguments.
        assert_eq!(short_name("_RINvCscCh4ujl5KQn_9regia_net4pickjEB2_"), "pick");
        // Garbage falls back to its own text instead of panicking.
        assert_eq!(short_name("_RNv"), "_RNv");
        assert_eq!(short_name("_RNvNvNvNv"), "_RNvNvNvNv");
    }

    #[test]
    fn parse_totals_filter_by_crate() {
        let r = parse_export(FIXTURE, &["regia_net".to_string()]).expect("fixture parses");
        assert_eq!(r.crates.len(), 1);
        let t = &r.crates["regia_net"];
        assert_eq!(
            t.lines,
            Ratio {
                count: 20,
                covered: 15
            }
        );
        assert_eq!(t.functions, Ratio { count: 3, covered: 2 });
        assert!((t.lines.percent() - 75.0).abs() < 1e-9);
        assert_eq!(r.functions.len(), 3);
        let all = parse_export(FIXTURE, &[]).expect("fixture parses");
        assert_eq!(all.crates.len(), 2);
    }

    #[test]
    fn parse_rejects_non_export() {
        assert!(parse_export("not json", &[]).is_err());
        assert!(parse_export("{}", &[]).is_err());
    }

    #[test]
    fn scan_finds_public_fns_only() {
        let src = "pub fn a() {}\n    pub const fn b<T>(x: T) {}\npub(crate) fn c() {}\nfn d() {}\n  pub async unsafe fn e() {}\npub struct F;\n";
        assert_eq!(scan_public_fns(src), vec!["a", "b", "e"]);
    }

    #[test]
    fn lowest_public_sorts_never_run_first_and_skips_private() {
        let r = parse_export(FIXTURE, &["regia_net".to_string()]).expect("fixture parses");
        let mut public = BTreeMap::new();
        public.insert(
            ("regia_net".to_string(), "src/seq.rs".to_string()),
            vec!["wrap_diff".to_string(), "next".to_string()],
        );
        let rows = lowest_public(&r, &public, 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "wrap_diff");
        assert_eq!(rows[0].count, 0);
        assert_eq!(rows[1].name, "next");
        assert_eq!(lowest_public(&r, &public, 1).len(), 1);
    }

    #[test]
    fn table_and_json_render() {
        let r = parse_export(FIXTURE, &["regia_net".to_string()]).expect("fixture parses");
        let t = render_table(&r);
        assert!(t.contains("regia_net") && t.contains("75.00%") && t.contains("(15/20)"));
        let j = to_json(&r, &[]);
        assert_eq!(j["crates"][0]["lines"]["covered"], 15);
    }

    #[test]
    fn empty_ratio_is_full() {
        assert_eq!(Ratio::default().percent(), 100.0);
    }
}
