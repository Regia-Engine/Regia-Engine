// SPDX-License-Identifier: MPL-2.0
//! The WGSL code node ([`Node::Code`], `.matgraph.ron` v5, ADR-7852): checking, include
//! resolution and the inlined function.
//!
//! A Code node is the body of one function. Its parameters are the node's typed inputs (in
//! order), then each declared texture input as a `texture_2d<f32>` of that name and, when there
//! is one, the graph's sampler as [`CODE_SAMPLER`]; it returns the node's declared output type.
//! The compiler emits it as a uniquely named module-scope function (`mg_code_<n>`, or
//! `mgv_code_<n>` in the vertex stage) and calls it in the node's `let`.
//!
//! Before any pipeline is built, [`check_code`] parses and validates the node with naga as a
//! standalone module (the function first, then its include files) and enforces the
//! restrictions: no entry points, no module-scope variables (so no bindings and no textures
//! except the declared texture inputs), no `override` constants, and a body whose braces
//! balance (it cannot close its own function and declare something at module scope). Each
//! diagnostic carries the 1-based line and column in the node's own text, so an editor can mark
//! it.
//!
//! `#include "name.wgsl"` lines (anywhere in the text; each counts as an empty line of the
//! body) pull in project helper files from [`CODE_INCLUDE_DIR`]: plain names only, no paths.
//! Includes are resolved into a copy of the graph that is compiled, never into the document
//! ([`MaterialGraph::resolve_code_includes`]): each resolved file is appended to the code after
//! the body as a `#resolved "name.wgsl" <lines>` block, so the structural hash (and every cache
//! keyed by it) covers the file contents. The same restrictions apply to included text.
//!
//! [`compile_lenient_with`] is the editor's compile: a Code node that fails its check is
//! replaced by a magenta constant of its output type, so the graph still compiles and previews
//! while the node's diagnostics are listed. [`crate::compile()`] (and the cook) refuse the graph
//! with the same messages instead.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};

use crate::compile::{CompileError, CompileErrors, CompiledGraph, compile_with, fnv64};
use crate::format::{CodeInput, FunctionSource, MaterialGraph, Node, ValueType};

/// Where project include files live, relative to the project root.
pub const CODE_INCLUDE_DIR: &str = "shaders/include";

/// Longest code text a node may hold, in bytes (includes not counted).
pub const MAX_CODE_BYTES: usize = 16 * 1024;

/// Most typed inputs one Code node may declare.
pub const MAX_CODE_INPUTS: usize = 16;

/// Most `#include` lines one Code node may hold.
pub const MAX_CODE_INCLUDES: usize = 8;

/// Longest include file, in bytes.
pub const MAX_INCLUDE_BYTES: usize = 64 * 1024;

/// The sampler parameter a Code node with texture inputs receives.
pub const CODE_SAMPLER: &str = "material_sampler";

/// The include directive's keyword.
pub const INCLUDE_DIRECTIVE: &str = "#include";

/// The keyword of a resolved include block (written by the resolver into a compile copy).
pub const RESOLVED_DIRECTIVE: &str = "#resolved";

/// Every [`CodeDiagnostic::code`], for the agent protocol and the book page.
pub const CODE_ERROR_CODES: [&str; 6] = [
    "code_syntax",
    "code_invalid",
    "code_restricted",
    "code_include",
    "code_signature",
    "code_too_large",
];

/// One problem in a Code node's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeDiagnostic {
    /// Stable lowercase code (one of [`CODE_ERROR_CODES`]).
    pub code: &'static str,
    /// 1-based line in the node's code; 0 for the node's declaration (inputs, textures,
    /// output) rather than a line of its text.
    pub line: u32,
    /// 1-based column (characters) on that line; 0 with line 0.
    pub column: u32,
    /// The include file the problem is in, if it is not the node's own text (`line` and
    /// `column` are then those of the `#include` line).
    pub include: Option<String>,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for CodeDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            write!(f, "code: {}", self.message)
        } else {
            write!(
                f,
                "code line {}, column {}: {}",
                self.line, self.column, self.message
            )?;
            if let Some(include) = &self.include {
                write!(f, " (in {include})")?;
            }
            Ok(())
        }
    }
}

impl CodeDiagnostic {
    fn new(code: &'static str, line: u32, column: u32, message: impl Into<String>) -> Self {
        Self {
            code,
            line,
            column,
            include: None,
            message: message.into(),
        }
    }

    fn decl(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(code, 0, 0, message)
    }
}

/// An `#include` line of a Code node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncludeLine {
    /// 1-based line of the directive.
    pub line: u32,
    /// The file name (`name.wgsl`).
    pub name: String,
}

/// A Code node's text split into its parts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodeParts {
    /// The body: the authored lines with every directive line emptied (line numbers kept).
    pub body: String,
    /// The `#include` lines, in order.
    pub includes: Vec<IncludeLine>,
    /// Resolved include files by name (blocks the resolver appended).
    pub resolved: BTreeMap<String, String>,
}

/// True for an include file name: 1–64 characters of `[A-Za-z0-9_-]` plus `.wgsl`, no path.
#[must_use]
pub fn valid_include_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".wgsl") else {
        return false;
    };
    !stem.is_empty()
        && name.len() <= 64
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// True for a name a Code input may take: a WGSL identifier (`[A-Za-z_][A-Za-z0-9_]*`, 1–64
/// characters, not `_`, not starting with `__` or `mg_`) other than [`CODE_SAMPLER`].
#[must_use]
pub fn valid_input_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= 64
        && name != "_"
        && !name.starts_with("__")
        && !name.starts_with("mg_")
        && name != CODE_SAMPLER
}

/// The quoted name of a directive line (`#include "x.wgsl"` → `x.wgsl`), and what follows it.
fn quoted(rest: &str) -> Option<(&str, &str)> {
    let rest = rest.trim_start();
    let inner = rest.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some((&inner[..end], inner[end + 1..].trim()))
}

/// Splits a Code node's text into its body, include lines and resolved include blocks
/// (`#resolved "name" <n>` followed by `n` lines). Bad directives are diagnostics.
#[must_use]
pub fn parse_code(code: &str) -> (CodeParts, Vec<CodeDiagnostic>) {
    let mut parts = CodeParts::default();
    let mut diags = Vec::new();
    let lines: Vec<&str> = code.split('\n').collect();
    let mut body: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let line_no = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let trimmed = raw.trim_start();
        let column = u32::try_from(raw.len() - trimmed.len() + 1).unwrap_or(1);
        if let Some(rest) = trimmed.strip_prefix(RESOLVED_DIRECTIVE) {
            // Everything from the first resolved block on is resolver output.
            let parsed = quoted(rest).and_then(|(name, count)| Some((name, count.parse::<usize>().ok()?)));
            match parsed {
                Some((name, count)) if i + count < lines.len() => {
                    let text = lines[i + 1..=i + count].join("\n");
                    parts.resolved.insert(name.to_string(), text);
                    i += count + 1;
                }
                _ => {
                    diags.push(CodeDiagnostic::new(
                        "code_include",
                        line_no,
                        column,
                        format!("`{RESOLVED_DIRECTIVE}` is reserved for resolved include files"),
                    ));
                    i += 1;
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(INCLUDE_DIRECTIVE) {
            match quoted(rest) {
                Some((name, tail)) if tail.is_empty() || tail.starts_with("//") => {
                    if valid_include_name(name) {
                        parts.includes.push(IncludeLine {
                            line: line_no,
                            name: name.to_string(),
                        });
                    } else {
                        diags.push(CodeDiagnostic::new(
                            "code_include",
                            line_no,
                            column,
                            format!(
                                "include `{name}` must be a file name in {CODE_INCLUDE_DIR}/ (letters, digits, `_`, `-`, ending in .wgsl; no path)"
                            ),
                        ));
                    }
                }
                _ => diags.push(CodeDiagnostic::new(
                    "code_include",
                    line_no,
                    column,
                    "write an include as `#include \"name.wgsl\"` on its own line",
                )),
            }
            body.push("");
        } else {
            body.push(raw);
        }
        i += 1;
    }
    if parts.includes.len() > MAX_CODE_INCLUDES {
        let extra = &parts.includes[MAX_CODE_INCLUDES];
        diags.push(CodeDiagnostic::new(
            "code_include",
            extra.line,
            1,
            format!("at most {MAX_CODE_INCLUDES} includes per Code node"),
        ));
    }
    parts.body = body.join("\n");
    (parts, diags)
}

/// The WGSL parameter list of a Code node's function.
fn params(inputs: &[CodeInput], textures: &[String]) -> String {
    let mut out: Vec<String> = inputs
        .iter()
        .map(|i| format!("{}: {}", i.name, i.ty.wgsl()))
        .collect();
    out.extend(textures.iter().map(|t| format!("{t}: texture_2d<f32>")));
    if !textures.is_empty() {
        out.push(format!("{CODE_SAMPLER}: sampler"));
    }
    out.join(", ")
}

/// The module-scope function a Code node compiles to: `fn <name>(<params>) -> <output> {`, the
/// body (directive lines emptied), `}`.
#[must_use]
pub fn function_source(
    name: &str,
    inputs: &[CodeInput],
    textures: &[String],
    output: ValueType,
    body: &str,
) -> String {
    let mut src = String::new();
    let _ = writeln!(
        src,
        "fn {name}({}) -> {} {{",
        params(inputs, textures),
        output.wgsl()
    );
    src.push_str(body);
    if !body.ends_with('\n') {
        src.push('\n');
    }
    src.push_str("}\n");
    src
}

/// Byte offset → (1-based line, 1-based character column) in `text`.
fn line_col(text: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(text.len());
    let before = &text[..text.floor_char_boundary(offset)];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(col).unwrap_or(u32::MAX),
    )
}

/// The first brace in `body` that closes past the function, or an unclosed one: WGSL has no
/// string literals, so only `//` and (nested) `/* */` comments need skipping.
fn unbalanced_brace(body: &str) -> Option<(usize, &'static str)> {
    let bytes = body.as_bytes();
    let mut depth = 0i32;
    let mut first_open = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut nest = 1;
                i += 2;
                while i < bytes.len() && nest > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        nest += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        nest -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            b'{' => {
                depth += 1;
                first_open.push(i);
            }
            b'}' => {
                depth -= 1;
                first_open.pop();
                if depth < 0 {
                    return Some((
                        i,
                        "this `}` closes the Code node's function; braces in the body must balance",
                    ));
                }
            }
            _ => {}
        }
        i += 1;
    }
    first_open
        .first()
        .map(|&at| (at, "this `{` is never closed; braces in the body must balance"))
}

/// What a block of the standalone check module holds.
enum Block {
    /// The function's signature line.
    Signature,
    /// The node's body.
    Body,
    /// An include file.
    Include(String),
}

/// Where a line of the check module came from.
enum Origin {
    /// The function's signature.
    Signature,
    /// Line `n` (1-based) of the node's text.
    Body(u32),
    /// Line `n` (1-based) of an include file.
    Include(String, u32),
}

/// The standalone module [`check_code`] validates and a map back to the node's text.
struct CheckModule {
    source: String,
    /// `(first module line, block)`, ascending.
    blocks: Vec<(u32, Block)>,
    body_lines: u32,
}

impl CheckModule {
    fn origin(&self, line: u32) -> Origin {
        let Some((start, block)) = self.blocks.iter().rev().find(|(start, _)| line >= *start) else {
            return Origin::Signature;
        };
        let n = line - start + 1;
        match block {
            Block::Signature => Origin::Signature,
            // The closing brace the compiler adds counts as the body's last line.
            Block::Body => Origin::Body(n.min(self.body_lines.max(1))),
            Block::Include(name) => Origin::Include(name.clone(), n),
        }
    }
}

fn build_check_module(
    inputs: &[CodeInput],
    textures: &[String],
    output: ValueType,
    parts: &CodeParts,
) -> CheckModule {
    let mut source = String::new();
    let mut blocks = vec![(1, Block::Signature), (2, Block::Body)];
    source.push_str(&function_source(
        "mg_code_check",
        inputs,
        textures,
        output,
        &parts.body,
    ));
    let body_lines = u32::try_from(parts.body.split('\n').count()).unwrap_or(u32::MAX);
    let mut seen = BTreeSet::new();
    for include in &parts.includes {
        if !seen.insert(include.name.clone()) {
            continue;
        }
        if let Some(text) = parts.resolved.get(&include.name) {
            let start = u32::try_from(source.matches('\n').count() + 1).unwrap_or(u32::MAX);
            blocks.push((start, Block::Include(include.name.clone())));
            source.push_str(text);
            if !text.ends_with('\n') {
                source.push('\n');
            }
        }
    }
    CheckModule {
        source,
        blocks,
        body_lines,
    }
}

/// A diagnostic at module line/column, mapped back to the node's text.
fn mapped(
    module: &CheckModule,
    parts: &CodeParts,
    code: &'static str,
    at: Option<(u32, u32)>,
    message: String,
) -> CodeDiagnostic {
    let Some((line, column)) = at else {
        return CodeDiagnostic::new(code, 1, 1, message);
    };
    match module.origin(line) {
        Origin::Signature => CodeDiagnostic::decl(
            "code_signature",
            format!("the node's inputs, textures or output do not form a valid signature: {message}"),
        ),
        Origin::Body(n) => CodeDiagnostic::new(code, n, column, message),
        Origin::Include(name, inner) => {
            let directive = parts
                .includes
                .iter()
                .find(|i| i.name == name)
                .map_or(1, |i| i.line);
            CodeDiagnostic {
                code,
                line: directive,
                column: 1,
                message: format!("{name} line {inner}, column {column}: {message}"),
                include: Some(name),
            }
        }
    }
}

/// Module line/column of the first `@vertex`, `@fragment` or `@compute` attribute.
fn stage_attribute(source: &str) -> Option<(u32, u32)> {
    ["@vertex", "@fragment", "@compute"]
        .iter()
        .filter_map(|a| source.find(a))
        .min()
        .map(|at| line_col(source, at))
}

/// The error and its sources, one line.
fn chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(s) = source {
        let _ = write!(text, ": {s}");
        source = s.source();
    }
    text
}

/// Checks the declaration (names, counts, size) without parsing the code.
fn check_declaration(inputs: &[CodeInput], textures: &[String], code: &str) -> Vec<CodeDiagnostic> {
    let mut diags = Vec::new();
    if code.len() > MAX_CODE_BYTES {
        diags.push(CodeDiagnostic::decl(
            "code_too_large",
            format!("{} bytes of code; at most {MAX_CODE_BYTES}", code.len()),
        ));
    }
    if inputs.len() > MAX_CODE_INPUTS {
        diags.push(CodeDiagnostic::decl(
            "code_signature",
            format!("{} inputs; at most {MAX_CODE_INPUTS}", inputs.len()),
        ));
    }
    let mut names = BTreeSet::new();
    for name in inputs
        .iter()
        .map(|i| i.name.as_str())
        .chain(textures.iter().map(String::as_str))
    {
        if !valid_input_name(name) {
            diags.push(CodeDiagnostic::decl(
                "code_signature",
                format!(
                    "input `{name}` is not a valid name (a WGSL identifier, not starting with `mg_` or `__`, not `{CODE_SAMPLER}`)"
                ),
            ));
        }
        if !names.insert(name) {
            diags.push(CodeDiagnostic::decl(
                "code_signature",
                format!("input `{name}` is declared twice"),
            ));
        }
    }
    diags
}

/// Texture inputs the graph does not declare.
fn check_textures(textures: &[String], declared: &BTreeSet<&str>) -> Vec<CodeDiagnostic> {
    textures
        .iter()
        .filter(|t| !declared.contains(t.as_str()))
        .map(|t| {
            CodeDiagnostic::decl(
                "code_signature",
                format!("texture input `{t}` is not a texture the graph declares"),
            )
        })
        .collect()
}

/// Parses and validates one Code node as a standalone module (its function first, then its
/// resolved include files) and enforces the restrictions. Empty when the node is valid. An
/// `#include` that was not resolved ([`MaterialGraph::resolve_code_includes`]) is a
/// `code_include` diagnostic at its line. Results are memoized by content.
#[must_use]
pub fn check_code(
    inputs: &[CodeInput],
    textures: &[String],
    output: ValueType,
    code: &str,
) -> Arc<Vec<CodeDiagnostic>> {
    let key = fnv64(
        ron::to_string(&(inputs, textures, output, code))
            .unwrap_or_default()
            .as_bytes(),
    );
    let cache = CHECKS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        return hit.clone();
    }
    // naga is not expected to panic on any input, but a checker that brings the editor down
    // on one keystroke would break the contract: a panic is a diagnostic.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check_uncached(inputs, textures, output, code)
    }))
    .unwrap_or_else(|_| {
        vec![CodeDiagnostic::new(
            "code_invalid",
            1,
            1,
            "the WGSL checker failed on this code (please report it)",
        )]
    });
    let result = Arc::new(result);
    let mut map = cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.len() >= CHECK_CACHE_CAP {
        map.clear();
    }
    map.insert(key, result.clone());
    result
}

/// Memoized check results by content hash.
type CheckCache = Mutex<HashMap<u64, Arc<Vec<CodeDiagnostic>>>>;

/// Memoized [`check_code`] results (cleared when full).
static CHECKS: OnceLock<CheckCache> = OnceLock::new();
const CHECK_CACHE_CAP: usize = 512;

fn check_uncached(
    inputs: &[CodeInput],
    textures: &[String],
    output: ValueType,
    code: &str,
) -> Vec<CodeDiagnostic> {
    let _span = tracing::debug_span!(target: "regia_matgraph", "code_node_check").entered();
    let mut diags = check_declaration(inputs, textures, code);
    if diags.iter().any(|d| d.code == "code_too_large") {
        return diags;
    }
    let (parts, mut parse_diags) = parse_code(code);
    diags.append(&mut parse_diags);
    for include in &parts.includes {
        if !parts.resolved.contains_key(&include.name) {
            diags.push(CodeDiagnostic::new(
                "code_include",
                include.line,
                1,
                format!(
                    "include `{}` was not found in the project's {CODE_INCLUDE_DIR}/",
                    include.name
                ),
            ));
        }
    }
    if let Some((at, why)) = unbalanced_brace(&parts.body) {
        let (line, column) = line_col(&parts.body, at);
        diags.push(CodeDiagnostic::new("code_restricted", line, column, why));
    }
    if !diags.is_empty() {
        return diags;
    }
    for (name, text) in &parts.resolved {
        if text.len() > MAX_INCLUDE_BYTES {
            diags.push(CodeDiagnostic::decl(
                "code_too_large",
                format!(
                    "include {name} is {} bytes; at most {MAX_INCLUDE_BYTES}",
                    text.len()
                ),
            ));
            return diags;
        }
    }
    let module = build_check_module(inputs, textures, output, &parts);
    let parsed = match naga::front::wgsl::parse_str(&module.source) {
        Ok(parsed) => parsed,
        Err(err) => {
            let at = err
                .location(&module.source)
                .map(|l| (l.line_number, l.line_position));
            let mut message = err.message().to_string();
            if let Some((_, label)) = err.labels().next()
                && !label.is_empty()
                && label != message
            {
                let _ = write!(message, " ({label})");
            }
            diags.push(mapped(&module, &parts, "code_syntax", at, message));
            return diags;
        }
    };
    let restricted = |what: &str, span: naga::Span| {
        let loc = span.location(&module.source);
        mapped(
            &module,
            &parts,
            "code_restricted",
            Some((loc.line_number, loc.line_position)),
            what.to_string(),
        )
    };
    for ep in &parsed.entry_points {
        diags.push(mapped(
            &module,
            &parts,
            "code_restricted",
            stage_attribute(&module.source),
            format!("entry point `{}` is not allowed in a Code node", ep.name),
        ));
    }
    for (handle, global) in parsed.global_variables.iter() {
        diags.push(restricted(
            &format!(
                "module-scope variable `{}` is not allowed: a Code node takes no bindings or textures except its texture inputs",
                global.name.as_deref().unwrap_or("?")
            ),
            parsed.global_variables.get_span(handle),
        ));
    }
    for (handle, o) in parsed.overrides.iter() {
        diags.push(restricted(
            &format!(
                "`override` constant `{}` is not allowed in a Code node",
                o.name.as_deref().unwrap_or("?")
            ),
            parsed.overrides.get_span(handle),
        ));
    }
    if !diags.is_empty() {
        return diags;
    }
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::default(),
    );
    if let Err(err) = validator.validate(&parsed) {
        // The narrowest span is the most specific one (the statement or expression, not the
        // whole function the error was propagated through).
        let narrowest = err
            .spans()
            .filter(|(span, _)| span.is_defined())
            .min_by_key(|(span, _)| span.to_range().map_or(usize::MAX, |r| r.len()));
        let at = narrowest
            .map(|(span, _)| span.location(&module.source))
            .map(|l| (l.line_number, l.line_position));
        let mut message = chain(err.as_inner());
        if let Some((_, label)) = narrowest.filter(|(_, l)| !l.is_empty()) {
            let _ = write!(message, " ({label})");
        }
        diags.push(mapped(&module, &parts, "code_invalid", at, message));
    }
    diags
}

/// Every declared texture name of `graph`.
fn graph_texture_names(graph: &MaterialGraph) -> BTreeSet<&str> {
    graph.textures.iter().map(|t| t.name.as_str()).collect()
}

/// [`check_code`] of one node plus the graph-level checks (texture inputs the graph declares).
/// Empty for a node that is not a Code node.
#[must_use]
pub fn check_node(graph: &MaterialGraph, node: &Node) -> Vec<CodeDiagnostic> {
    let Node::Code {
        inputs,
        textures,
        output,
        code,
    } = node
    else {
        return Vec::new();
    };
    let mut diags = check_textures(textures, &graph_texture_names(graph));
    diags.extend(check_code(inputs, textures, *output, code).iter().cloned());
    diags
}

/// Every Code node of `graph` (its own nodes, then those inside inline functions as
/// `function/node`) with its diagnostics; nodes that pass are listed with none.
#[must_use]
pub fn check_graph(graph: &MaterialGraph) -> Vec<(String, Vec<CodeDiagnostic>)> {
    let mut out = Vec::new();
    for (name, node) in &graph.nodes {
        if matches!(node, Node::Code { .. }) {
            out.push((name.clone(), check_node(graph, node)));
        }
    }
    for (fname, source) in &graph.functions {
        if let FunctionSource::Inline(function) = source {
            for (name, node) in &function.nodes {
                if matches!(node, Node::Code { .. }) {
                    out.push((format!("function:{fname}/{name}"), check_node(graph, node)));
                }
            }
        }
    }
    out
}

/// The include names a code text uses, in order, without duplicates.
#[must_use]
pub fn include_names(code: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    parse_code(code)
        .0
        .includes
        .into_iter()
        .filter(|i| seen.insert(i.name.clone()))
        .map(|i| i.name)
        .collect()
}

/// The code with its resolved blocks removed (what an author wrote).
#[must_use]
pub fn strip_resolved(code: &str) -> String {
    match code.find(&format!("\n{RESOLVED_DIRECTIVE} ")) {
        Some(at) => code[..at].to_string(),
        None if code.starts_with(&format!("{RESOLVED_DIRECTIVE} ")) => String::new(),
        None => code.to_string(),
    }
}

/// `code` (resolved blocks stripped) with `includes` (name → text) appended as resolved
/// blocks, for the names its `#include` lines use.
#[must_use]
pub fn with_resolved(code: &str, includes: &BTreeMap<String, String>) -> String {
    let mut out = strip_resolved(code);
    for name in include_names(&strip_resolved(code)) {
        if let Some(text) = includes.get(&name) {
            let text = text.trim_end_matches('\n');
            let count = text.split('\n').count();
            let _ = write!(out, "\n{RESOLVED_DIRECTIVE} \"{name}\" {count}\n{text}");
        }
    }
    out
}

impl MaterialGraph {
    /// Resolves the `#include` lines of every Code node (the graph's and those of inline
    /// functions; resolve function files first) by reading each name once through `read`
    /// (the host maps a name to `<project>/shaders/include/<name>`). Call it on the copy that
    /// is compiled, never on the document. An include `read` cannot supply stays unresolved
    /// and is reported by the compile on its line. Returns the names read, sorted.
    pub fn resolve_code_includes(
        &mut self,
        read: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Vec<String> {
        let mut wanted = BTreeSet::new();
        let codes = self.nodes.values().chain(
            self.functions
                .values()
                .filter_map(|f| match f {
                    FunctionSource::Inline(function) => Some(function.nodes.values()),
                    FunctionSource::File(_) => None,
                })
                .flatten(),
        );
        for node in codes {
            if let Node::Code { code, .. } = node {
                wanted.extend(include_names(code));
            }
        }
        let mut texts = BTreeMap::new();
        for name in &wanted {
            if valid_include_name(name)
                && let Ok(text) = read(name)
            {
                texts.insert(name.clone(), text);
            }
        }
        let resolve = |node: &mut Node| {
            if let Node::Code { code, .. } = node {
                *code = with_resolved(code, &texts);
            }
        };
        self.nodes.values_mut().for_each(resolve);
        for source in self.functions.values_mut() {
            if let FunctionSource::Inline(function) = source {
                function.nodes.values_mut().for_each(resolve);
            }
        }
        texts.into_keys().collect()
    }
}

/// The project path of include `name`, or `None` for a name that is not a plain include file
/// name ([`valid_include_name`]).
#[must_use]
pub fn include_path(project_root: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    valid_include_name(name).then(|| project_root.join(CODE_INCLUDE_DIR).join(name))
}

/// A reader for [`MaterialGraph::resolve_code_includes`] over a project's include directory.
pub fn project_include_reader(
    project_root: &std::path::Path,
) -> impl FnMut(&str) -> Result<String, String> + '_ {
    move |name| {
        let path = include_path(project_root, name).ok_or_else(|| format!("bad include name `{name}`"))?;
        std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// The function names an include text declares (for the editor's completion list); empty when
/// it does not parse.
#[must_use]
pub fn declared_functions(text: &str) -> Vec<String> {
    let Ok(Ok(module)) = std::panic::catch_unwind(|| naga::front::wgsl::parse_str(text)) else {
        return Vec::new();
    };
    module
        .functions
        .iter()
        .filter_map(|(_, f)| f.name.clone())
        .collect()
}

/// The magenta constant a failing Code node shades as.
#[must_use]
pub fn error_value(output: ValueType) -> Node {
    match output {
        ValueType::Float => Node::Float(1.0),
        ValueType::Vec2 => Node::Vec2([1.0, 0.0]),
        ValueType::Vec3 => Node::Vec3([1.0, 0.0, 1.0]),
        ValueType::Vec4 => Node::Vec4([1.0, 0.0, 1.0, 1.0]),
    }
}

/// A [`CompileError`] for a Code node diagnostic (`node` is the node's name in the graph).
#[must_use]
pub fn compile_error(node: &str, diag: &CodeDiagnostic) -> CompileError {
    CompileError {
        node: Some(node.to_string()),
        message: diag.to_string(),
    }
}

/// The editor's compile: like [`compile_with`], but every Code node that fails its check is
/// compiled as its magenta [`error_value`] and its diagnostics are returned beside the result,
/// so the graph still previews. If the composed code still fails in a host shader while the
/// graph has Code nodes, every Code node falls back and the host error is reported on them.
///
/// # Errors
///
/// The graph does not compile for a reason other than a Code node's text.
pub fn compile_lenient_with(
    graph: &MaterialGraph,
    switches: &BTreeMap<String, bool>,
) -> Result<(CompiledGraph, Vec<CompileError>), CompileErrors> {
    let mut errors = Vec::new();
    let mut copy = graph.clone();
    let mut replace = |nodes: &mut BTreeMap<String, Node>, prefix: &str| {
        for (name, node) in nodes.iter_mut() {
            let diags = check_node(graph, node);
            if diags.is_empty() {
                continue;
            }
            let Node::Code { output, .. } = node else {
                continue;
            };
            let full = format!("{prefix}{name}");
            errors.extend(diags.iter().map(|d| compile_error(&full, d)));
            *node = error_value(*output);
        }
    };
    replace(&mut copy.nodes, "");
    for (fname, source) in &mut copy.functions {
        if let FunctionSource::Inline(function) = source {
            replace(&mut function.nodes, &format!("function:{fname}/"));
        }
    }
    let err = match compile_with(&copy, switches) {
        Ok(compiled) => return Ok((compiled, errors)),
        Err(err) => err,
    };
    let codes = code_nodes(&copy);
    if codes.is_empty()
        || !err
            .0
            .iter()
            .any(|e| e.node.as_deref().is_some_and(|n| codes.iter().any(|c| c == n)))
    {
        return Err(err);
    }
    // The composed code failed in a host shader on a Code node: shade every one magenta.
    let fallback = |nodes: &mut BTreeMap<String, Node>| {
        for node in nodes.values_mut() {
            if let Node::Code { output, .. } = node {
                *node = error_value(*output);
            }
        }
    };
    fallback(&mut copy.nodes);
    for source in copy.functions.values_mut() {
        if let FunctionSource::Inline(function) = source {
            fallback(&mut function.nodes);
        }
    }
    let compiled = compile_with(&copy, switches)?;
    errors.extend(err.0);
    Ok((compiled, errors))
}

/// The names of the graph's own Code nodes.
fn code_nodes(graph: &MaterialGraph) -> Vec<String> {
    graph
        .nodes
        .iter()
        .filter(|(_, n)| matches!(n, Node::Code { .. }))
        .map(|(k, _)| k.clone())
        .collect()
}

/// [`compile_lenient_with`] with the declared switch defaults.
///
/// # Errors
///
/// As [`compile_lenient_with`].
pub fn compile_lenient(graph: &MaterialGraph) -> Result<(CompiledGraph, Vec<CompileError>), CompileErrors> {
    compile_lenient_with(graph, &BTreeMap::new())
}

/// WGSL words the code view highlights as keywords.
pub const WGSL_KEYWORDS: &[&str] = &[
    "alias",
    "break",
    "case",
    "const",
    "const_assert",
    "continue",
    "continuing",
    "default",
    "diagnostic",
    "discard",
    "else",
    "enable",
    "false",
    "fn",
    "for",
    "if",
    "let",
    "loop",
    "override",
    "requires",
    "return",
    "struct",
    "switch",
    "true",
    "var",
    "while",
];

/// WGSL type names the code view highlights as types.
pub const WGSL_TYPES: &[&str] = &[
    "bool",
    "f16",
    "f32",
    "i32",
    "u32",
    "vec2",
    "vec3",
    "vec4",
    "vec2f",
    "vec3f",
    "vec4f",
    "vec2i",
    "vec3i",
    "vec4i",
    "vec2u",
    "vec3u",
    "vec4u",
    "mat2x2",
    "mat3x3",
    "mat4x4",
    "mat2x2f",
    "mat3x3f",
    "mat4x4f",
    "array",
    "texture_2d",
    "sampler",
];

/// WGSL built-in functions offered by the completion list (the common math set).
pub const WGSL_BUILTINS: &[&str] = &[
    "abs",
    "acos",
    "asin",
    "atan",
    "atan2",
    "ceil",
    "clamp",
    "cos",
    "cross",
    "degrees",
    "distance",
    "dot",
    "dpdx",
    "dpdy",
    "exp",
    "exp2",
    "floor",
    "fract",
    "fwidth",
    "inverseSqrt",
    "length",
    "log",
    "log2",
    "max",
    "min",
    "mix",
    "normalize",
    "pow",
    "radians",
    "reflect",
    "refract",
    "round",
    "saturate",
    "select",
    "sign",
    "sin",
    "smoothstep",
    "sqrt",
    "step",
    "tan",
    "textureSample",
    "textureSampleLevel",
    "trunc",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn inp(name: &str, ty: ValueType) -> CodeInput {
        CodeInput {
            name: name.into(),
            ty,
            node: String::new(),
        }
    }

    fn abc() -> Vec<CodeInput> {
        vec![
            inp("a", ValueType::Float),
            inp("b", ValueType::Float),
            inp("c", ValueType::Float),
        ]
    }

    #[test]
    fn a_valid_body_has_no_diagnostics() {
        let d = check_code(&abc(), &[], ValueType::Float, "return a * b + c;");
        assert!(d.is_empty(), "{d:?}");
        let multi = "let t = a * b;\n// a comment with a } brace\nreturn t + c;";
        assert!(check_code(&abc(), &[], ValueType::Float, multi).is_empty());
    }

    #[test]
    fn a_syntax_error_is_reported_at_its_line_and_column() {
        let code = "let t = a * b;\nlet u = t +;\nreturn u + c;";
        let d = check_code(&abc(), &[], ValueType::Float, code);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].code, "code_syntax");
        assert_eq!(d[0].line, 2, "{d:?}");
        assert!(d[0].column >= 9, "{d:?}");
        assert!(d[0].to_string().starts_with("code line 2, column"), "{}", d[0]);
    }

    #[test]
    fn a_type_error_is_reported_at_its_line() {
        let d = check_code(&abc(), &[], ValueType::Vec3, "\n\nreturn a * b + c;");
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].code, "code_invalid");
        assert_eq!(d[0].line, 3, "{d:?}");
    }

    #[test]
    fn bindings_entry_points_and_escapes_are_refused() {
        // Closing the function's brace to declare a binding at module scope.
        let escape = "return a;\n}\n@group(0) @binding(0) var<uniform> secret: vec4<f32>;\nfn more() -> f32 {\nreturn 1.0;";
        let d = check_code(&abc(), &[], ValueType::Float, escape);
        assert!(
            d.iter().any(|d| d.code == "code_restricted" && d.line == 2),
            "{d:?}"
        );
        let unclosed = "if (a > 0.0) {\nreturn a;";
        let d = check_code(&abc(), &[], ValueType::Float, unclosed);
        assert!(
            d.iter().any(|d| d.code == "code_restricted" && d.line == 1),
            "{d:?}"
        );
        // An include may not declare a binding or an entry point either.
        let mut includes = BTreeMap::new();
        includes.insert(
            "bad.wgsl".to_string(),
            "@group(1) @binding(0) var t: texture_2d<f32>;\nfn h() -> f32 { return 1.0; }".to_string(),
        );
        let code = with_resolved("#include \"bad.wgsl\"\nreturn h();", &includes);
        let d = check_code(&abc(), &[], ValueType::Float, &code);
        assert!(
            d.iter().any(|d| d.code == "code_restricted"
                && d.include.as_deref() == Some("bad.wgsl")
                && d.line == 1),
            "{d:?}"
        );
        includes.insert(
            "ep.wgsl".to_string(),
            "@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }".to_string(),
        );
        let code = with_resolved("#include \"ep.wgsl\"\nreturn a;", &includes);
        let d = check_code(&abc(), &[], ValueType::Float, &code);
        assert!(
            d.iter()
                .any(|d| d.code == "code_restricted" && d.message.contains("entry point")),
            "{d:?}"
        );
    }

    #[test]
    fn includes_resolve_and_missing_ones_are_reported_at_their_line() {
        let code = "// helpers\n#include \"mad.wgsl\"\nreturn mg_helper_free(a, b, c);";
        let d = check_code(&abc(), &[], ValueType::Float, code);
        assert!(d.iter().any(|d| d.code == "code_include" && d.line == 2), "{d:?}");
        let mut includes = BTreeMap::new();
        includes.insert(
            "mad.wgsl".to_string(),
            "fn mg_helper_free(x: f32, y: f32, z: f32) -> f32 {\n    return x * y + z;\n}\n".to_string(),
        );
        let resolved = with_resolved(code, &includes);
        assert!(check_code(&abc(), &[], ValueType::Float, &resolved).is_empty());
        assert_eq!(strip_resolved(&resolved), code, "resolution is reversible");
        assert_eq!(with_resolved(&resolved, &includes), resolved, "and idempotent");
        // A broken include is reported at its directive, naming the include's own line.
        includes.insert(
            "mad.wgsl".to_string(),
            "fn mg_helper_free(x: f32) -> f32 {\n    return x +;\n}\n".to_string(),
        );
        let d = check_code(&abc(), &[], ValueType::Float, &with_resolved(code, &includes));
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(
            (d[0].line, d[0].include.as_deref()),
            (2, Some("mad.wgsl")),
            "{d:?}"
        );
        assert!(d[0].message.starts_with("mad.wgsl line 2"), "{d:?}");
    }

    #[test]
    fn include_names_are_plain_files() {
        assert!(valid_include_name("noise-2d_v1.wgsl"));
        for bad in ["../x.wgsl", "a/b.wgsl", ".wgsl", "x.txt", "x", "C:x.wgsl"] {
            assert!(!valid_include_name(bad), "{bad}");
            let d = parse_code(&format!("#include \"{bad}\"\nreturn 1.0;")).1;
            assert!(
                d.iter().any(|d| d.code == "code_include" && d.line == 1),
                "{bad}: {d:?}"
            );
        }
        let d = parse_code("#include x.wgsl\nreturn 1.0;").1;
        assert_eq!(d.len(), 1);
        assert!(include_path(std::path::Path::new("/p"), "../x.wgsl").is_none());
    }

    #[test]
    fn the_declaration_is_checked() {
        let d = check_code(
            &[inp("mg_x", ValueType::Float)],
            &[],
            ValueType::Float,
            "return 1.0;",
        );
        assert!(
            d.iter().any(|d| d.code == "code_signature" && d.line == 0),
            "{d:?}"
        );
        let d = check_code(
            &[inp("a", ValueType::Float), inp("a", ValueType::Vec2)],
            &[],
            ValueType::Float,
            "return 1.0;",
        );
        assert!(d.iter().any(|d| d.message.contains("twice")), "{d:?}");
        let big = "x".repeat(MAX_CODE_BYTES + 1);
        let d = check_code(&[], &[], ValueType::Float, &big);
        assert_eq!(d[0].code, "code_too_large");
    }

    #[test]
    fn texture_inputs_arrive_as_parameters() {
        let code = "return textureSample(albedo, material_sampler, uv);";
        let d = check_code(
            &[inp("uv", ValueType::Vec2)],
            &["albedo".into()],
            ValueType::Vec4,
            code,
        );
        assert!(d.is_empty(), "{d:?}");
        let src = function_source(
            "mg_code_3",
            &[inp("uv", ValueType::Vec2)],
            &["albedo".into()],
            ValueType::Vec4,
            code,
        );
        assert!(src.starts_with("fn mg_code_3(uv: vec2<f32>, albedo: texture_2d<f32>, material_sampler: sampler) -> vec4<f32> {"), "{src}");
    }

    #[test]
    fn declared_functions_list_an_includes_helpers() {
        let names = declared_functions("fn a1() -> f32 { return 1.0; }\nfn b2(x: f32) -> f32 { return x; }");
        assert_eq!(names, vec!["a1".to_string(), "b2".to_string()]);
        assert!(declared_functions("fn (").is_empty());
    }
}
