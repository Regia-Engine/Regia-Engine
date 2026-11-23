// SPDX-License-Identifier: MPL-2.0
//! Rust and WGSL struct agreement (ADR-3002).
//!
//! A uniform or storage binding reads host memory laid out by a Rust type. When the Rust
//! struct and the WGSL struct disagree on a size or a member offset, wgpu fails at run time
//! (a binding smaller than the shader expects) or, worse, the shader reads shifted fields. This
//! module checks the agreement on the composed source:
//!
//! 1. [`struct_layouts`] asks naga's `Layouter` for the size and member offsets of every named
//!    struct of a module.
//! 2. A [`ContractRow`] names a shader file (or any), a WGSL struct and the host type's size and
//!    member offsets (built with [`contract_row!`](crate::contract_row), so the offsets come
//!    from `offset_of!` and cannot drift from the type).
//! 3. [`bound_structs`] lists the struct of every uniform and storage binding of a module.
//! 4. [`run_contract_ratchet`] joins them: every bound struct needs a row or an
//!    [`AllowEntry`] with a reason; a row that matches nothing, an allowlist entry that is now
//!    covered (or no longer bound) and any mismatch fail. The allowlist can only shrink.

use std::collections::{BTreeMap, BTreeSet};

/// The name a struct has in the source: the composer renames an imported struct
/// `NameX_naga_oil_mod_X<module>`, which is cut here.
fn source_name(name: &str) -> &str {
    name.split("X_naga_oil_mod_X").next().unwrap_or(name)
}

/// Size and member offsets of one WGSL struct as naga lays it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructLayout {
    /// Size in bytes (the struct's span, a multiple of its alignment).
    pub size: u32,
    /// `(member name, byte offset)`, in declaration order.
    pub members: Vec<(String, u32)>,
}

/// The size and member offsets of every named struct of `module`.
///
/// # Errors
///
/// naga's layouter rejects a type of the module.
pub fn struct_layouts(module: &naga::Module) -> Result<BTreeMap<String, StructLayout>, String> {
    let mut layouter = naga::proc::Layouter::default();
    layouter.update(module.to_ctx()).map_err(|err| err.to_string())?;
    let mut out = BTreeMap::new();
    for (handle, ty) in module.types.iter() {
        let (Some(name), naga::TypeInner::Struct { members, .. }) = (ty.name.as_deref(), &ty.inner) else {
            continue;
        };
        let members = members
            .iter()
            .map(|m| (m.name.clone().unwrap_or_default(), m.offset))
            .collect();
        out.insert(
            source_name(name).to_string(),
            StructLayout {
                size: layouter[handle].size,
                members,
            },
        );
    }
    Ok(out)
}

/// How a binding reaches the shader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BindingSpace {
    /// `var<uniform>`.
    Uniform,
    /// `var<storage>`, read or read-write.
    Storage,
}

/// A struct read through a uniform or storage binding (arrays are peeled to their element).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BoundStruct {
    /// The WGSL struct name.
    pub name: String,
    /// Uniform or storage.
    pub space: BindingSpace,
}

/// Every named struct a uniform or storage binding of `module` reads, once each.
#[must_use]
pub fn bound_structs(module: &naga::Module) -> Vec<BoundStruct> {
    let mut seen = BTreeSet::new();
    for (_, global) in module.global_variables.iter() {
        if global.binding.is_none() {
            continue;
        }
        let space = match global.space {
            naga::AddressSpace::Uniform => BindingSpace::Uniform,
            naga::AddressSpace::Storage { .. } => BindingSpace::Storage,
            _ => continue,
        };
        let mut ty = global.ty;
        loop {
            match &module.types[ty].inner {
                naga::TypeInner::Array { base, .. } | naga::TypeInner::BindingArray { base, .. } => {
                    ty = *base
                }
                naga::TypeInner::Struct { .. } => {
                    if let Some(name) = module.types[ty].name.as_deref() {
                        seen.insert(BoundStruct {
                            name: source_name(name).to_string(),
                            space,
                        });
                    }
                    break;
                }
                _ => break,
            }
        }
    }
    seen.into_iter().collect()
}

/// The host side of a registry row: a Rust type's size and the offset of the fields that
/// back WGSL members. Build it with [`contract_row!`](crate::contract_row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractRow {
    /// The shader files the row applies to, comma separated; empty = every module that declares
    /// the struct.
    pub shader: &'static str,
    /// The WGSL struct name.
    pub wgsl_struct: &'static str,
    /// `size_of` the Rust type.
    pub size: usize,
    /// `(WGSL member, offset of the Rust field)`. Every WGSL member must be listed unless
    /// `partial`.
    pub members: Vec<(&'static str, usize)>,
    /// `true`: WGSL members that are not listed are not an error (a trailing run of padding
    /// the host fills with one array).
    pub partial: bool,
    /// `true`: the host type may be larger than the WGSL struct (a shader that reads a prefix of
    /// a shared uniform; a binding bigger than the shader's minimum is valid). A smaller host
    /// type is always an error.
    pub host_may_extend: bool,
}

impl ContractRow {
    /// A row with every member listed.
    #[must_use]
    pub fn new(
        shader: &'static str,
        wgsl_struct: &'static str,
        size: usize,
        members: Vec<(&'static str, usize)>,
    ) -> Self {
        Self {
            shader,
            wgsl_struct,
            size,
            members,
            partial: false,
            host_may_extend: false,
        }
    }

    /// The same row, tolerating a host type that extends past the WGSL struct.
    #[must_use]
    pub fn host_may_extend(mut self) -> Self {
        self.host_may_extend = true;
        self
    }

    /// The same row, tolerating WGSL members that are not listed.
    #[must_use]
    pub fn partial(mut self) -> Self {
        self.partial = true;
        self
    }
}

/// Builds a [`ContractRow`] from a Rust type: `contract_row!("file.wgsl", "WgslStruct", Type {
/// member, other => rust_field, nested => outer.inner })`. A bare name is the WGSL member and
/// the Rust field at once; `a => b` maps WGSL member `a` to Rust field `b` (a dotted path is
/// allowed). An empty file name applies the row to every module that declares the struct.
#[macro_export]
macro_rules! contract_row {
    (@off $ty:ty, $m:ident) => { ::std::mem::offset_of!($ty, $m) };
    (@off $ty:ty, $m:ident => $($f:ident).+) => { ::std::mem::offset_of!($ty, $($f).+) };
    ($shader:expr, $wgsl:expr, $ty:ty { $($m:ident $(=> $($f:ident).+)?),* $(,)? }) => {
        $crate::ContractRow::new(
            $shader,
            $wgsl,
            ::std::mem::size_of::<$ty>(),
            vec![$((stringify!($m), $crate::contract_row!(@off $ty, $m $(=> $($f).+)?))),*],
        )
    };
}

/// One bound struct that has no registry row yet, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllowEntry {
    /// The shader file name, as the scan names it.
    pub file: &'static str,
    /// The WGSL struct.
    pub wgsl_struct: &'static str,
    /// Why there is no row.
    pub reason: &'static str,
}

/// A module the ratchet scans.
pub struct ContractInput<'a> {
    /// The file name rows and allowlist entries use.
    pub file: &'a str,
    /// The parsed (composed, for composed shaders) module.
    pub module: &'a naga::Module,
}

/// The ratchet's result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContractReport {
    /// Distinct `(file, struct)` pairs bound by a uniform or storage binding.
    pub bound: usize,
    /// Of those, pairs covered by at least one row.
    pub covered: usize,
    /// Pairs on the allowlist.
    pub allowed: usize,
    /// Row checks run (a row on `n` modules counts `n`).
    pub checks: usize,
    /// Size and offset mismatches, one line each.
    pub mismatches: Vec<String>,
    /// `file::Struct` bound with neither a row nor an allowlist entry.
    pub unlisted: Vec<String>,
    /// Rows that match no module or no struct.
    pub dead_rows: Vec<String>,
    /// Allowlist entries that are covered by a row or no longer bound.
    pub stale_allow: Vec<String>,
}

/// The ratchet's result in the `check-shaders --json` report.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContractSummary {
    /// Distinct `(file, struct)` pairs bound by a uniform or storage binding.
    pub bound: usize,
    /// Pairs covered by a registry row (checked for size and member offsets).
    pub covered: usize,
    /// Pairs on the allowlist (no row yet, each with a reason).
    pub allowed: usize,
    /// Row checks run.
    pub checks: usize,
    /// Every failure, one line each.
    pub problems: Vec<String>,
}

impl ContractReport {
    /// The report in its JSON shape.
    #[must_use]
    pub fn summary(&self) -> ContractSummary {
        ContractSummary {
            bound: self.bound,
            covered: self.covered,
            allowed: self.allowed,
            checks: self.checks,
            problems: self.problems(),
        }
    }

    /// `true` when nothing failed.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty()
            && self.unlisted.is_empty()
            && self.dead_rows.is_empty()
            && self.stale_allow.is_empty()
    }

    /// Every failure as a line.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.extend(self.mismatches.iter().map(|m| format!("mismatch: {m}")));
        out.extend(self.unlisted.iter().map(|m| {
            format!("unlisted: {m} is read through a uniform or storage binding with no contract row and no allowlist entry")
        }));
        out.extend(
            self.dead_rows
                .iter()
                .map(|m| format!("dead row: {m} matches no struct of the scanned shaders")),
        );
        out.extend(
            self.stale_allow
                .iter()
                .map(|m| format!("stale allowlist entry: {m} is covered by a row or no longer bound")),
        );
        out
    }
}

fn compare(row: &ContractRow, file: &str, wgsl: &StructLayout, out: &mut Vec<String>) {
    let tag = format!("{file}::{}", row.wgsl_struct);
    let size_ok = if row.host_may_extend {
        row.size >= wgsl.size as usize
    } else {
        wgsl.size as usize == row.size
    };
    if !size_ok {
        out.push(format!(
            "{tag}: WGSL size {} but the Rust type is {} bytes",
            wgsl.size, row.size
        ));
    }
    for (name, offset) in &row.members {
        match wgsl.members.iter().find(|(m, _)| m == name) {
            None => out.push(format!(
                "{tag}: the Rust row names member `{name}` the WGSL struct lacks"
            )),
            Some((_, wgsl_offset)) if *wgsl_offset as usize != *offset => out.push(format!(
                "{tag}: member `{name}` is at WGSL offset {wgsl_offset} but the Rust field is at {offset}"
            )),
            Some(_) => {}
        }
    }
    if !row.partial {
        for (name, _) in &wgsl.members {
            if !row.members.iter().any(|(m, _)| m == name) {
                out.push(format!(
                    "{tag}: WGSL member `{name}` has no Rust field in the row"
                ));
            }
        }
    }
}

/// Checks every row against the modules that declare its struct, and joins the bound structs of
/// `inputs` with the rows and the allowlist (the ratchet).
#[must_use]
pub fn run_contract_ratchet(
    inputs: &[ContractInput<'_>],
    rows: &[ContractRow],
    allowlist: &[AllowEntry],
) -> ContractReport {
    let mut report = ContractReport::default();
    let mut layouts = Vec::new();
    for input in inputs {
        match struct_layouts(input.module) {
            Ok(l) => layouts.push(l),
            Err(err) => {
                report
                    .mismatches
                    .push(format!("{}: the module could not be laid out: {err}", input.file));
                layouts.push(BTreeMap::new());
            }
        }
    }
    let mut row_used = vec![false; rows.len()];
    let mut covered: BTreeSet<(String, String)> = BTreeSet::new();
    let mut bound: BTreeSet<(String, String)> = BTreeSet::new();
    for (input, layout) in inputs.iter().zip(&layouts) {
        let in_scope = bound_structs(input.module);
        for b in &in_scope {
            bound.insert((input.file.to_string(), b.name.clone()));
        }
        for (i, row) in rows.iter().enumerate() {
            if !(row.shader.is_empty() || row.shader.split(',').any(|f| f == input.file)) {
                continue;
            }
            let Some(wgsl) = layout.get(row.wgsl_struct) else {
                continue;
            };
            row_used[i] = true;
            report.checks += 1;
            compare(row, input.file, wgsl, &mut report.mismatches);
            if in_scope.iter().any(|b| b.name == row.wgsl_struct) {
                covered.insert((input.file.to_string(), row.wgsl_struct.to_string()));
            }
        }
    }
    for (i, row) in rows.iter().enumerate() {
        if !row_used[i] {
            report.dead_rows.push(format!(
                "{}::{}",
                if row.shader.is_empty() { "*" } else { row.shader },
                row.wgsl_struct
            ));
        }
    }
    let mut allowed = BTreeSet::new();
    for entry in allowlist {
        let key = (entry.file.to_string(), entry.wgsl_struct.to_string());
        if covered.contains(&key) || !bound.contains(&key) || !allowed.insert(key) {
            report
                .stale_allow
                .push(format!("{}::{}", entry.file, entry.wgsl_struct));
        }
    }
    for key in &bound {
        if !covered.contains(key) && !allowed.contains(key) {
            report.unlisted.push(format!("{}::{}", key.0, key.1));
        }
    }
    report.bound = bound.len();
    report.covered = covered.len();
    report.allowed = allowed.len();
    report
}

/// Parses plain WGSL the way the renderer hands it to wgpu.
///
/// # Errors
///
/// The source does not parse.
pub fn parse_plain(source: &str) -> Result<naga::Module, String> {
    let mut module = naga::front::wgsl::parse_str(source).map_err(|err| err.emit_to_string(source))?;
    crate::specialize_handle_arguments(&mut module);
    Ok(module)
}

/// The file name rows and allowlist entries use for a source named like the plain-source list
/// does: `terrain.wgsl (shared, hw height true)` and `water_body.wgsl + PROBE_ENTRY` are both
/// variants of one file.
#[must_use]
pub fn contract_file_name(name: &str) -> &str {
    let end = [name.find(" ("), name.find(" +")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(name.len());
    &name[..end]
}

/// The ratchet over the modules of every engine crate: `regia_render`'s own
/// ([`render_contract_modules`], when `render` is set), the plain sources of the other crates
/// (`plain`, `(name, source)`, as the renderer hands them to wgpu) and the composed ones
/// (`composed`, composed with no defines). Variants of one file merge under
/// [`contract_file_name`].
///
/// # Errors
///
/// A module fails to compose or parse.
pub fn ratchet_sources(
    render: bool,
    plain: &[(&str, &str)],
    composed: &[(&str, &str)],
    rows: &[ContractRow],
    allowlist: &[AllowEntry],
) -> Result<ContractReport, String> {
    let mut modules: Vec<(String, naga::Module)> = if render {
        render_contract_modules(true)?
    } else {
        Vec::new()
    };
    for (name, source) in plain {
        modules.push((
            contract_file_name(name).to_string(),
            parse_plain(source).map_err(|e| format!("{name}: {e}"))?,
        ));
    }
    for (name, source) in composed {
        let module = crate::shader_lib::compose_source(source, name, std::collections::HashMap::new())
            .map_err(|e| format!("{name}: {e}"))?;
        modules.push(((*name).to_string(), module));
    }
    let inputs: Vec<ContractInput<'_>> = modules
        .iter()
        .map(|(file, module)| ContractInput { file, module })
        .collect();
    Ok(run_contract_ratchet(&inputs, rows, allowlist))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct Good {
        a: [f32; 4],
        b: [u32; 2],
        c: f32,
        d: f32,
    }

    const SRC: &str = "
struct S { a: vec4<f32>, b: vec2<u32>, c: f32, d: f32 }
struct T { x: f32 }
@group(0) @binding(0) var<uniform> s: S;
@group(0) @binding(1) var<storage, read> t: array<T>;
@fragment fn fs() -> @location(0) vec4<f32> { return vec4<f32>(s.a.x + t[0].x); }
";

    fn module() -> naga::Module {
        parse_plain(SRC).expect("parses")
    }

    #[test]
    fn the_layouter_reports_size_and_offsets() {
        let l = struct_layouts(&module()).expect("layout");
        assert_eq!(l["S"].size, 32);
        assert_eq!(l["S"].members[1], ("b".to_string(), 16));
        assert_eq!(l["T"].size, 4);
    }

    #[test]
    fn bound_structs_peel_arrays_and_name_the_space() {
        let b = bound_structs(&module());
        assert_eq!(
            b,
            vec![
                BoundStruct {
                    name: "S".into(),
                    space: BindingSpace::Uniform
                },
                BoundStruct {
                    name: "T".into(),
                    space: BindingSpace::Storage
                },
            ]
        );
    }

    fn rows() -> Vec<ContractRow> {
        vec![
            crate::contract_row!("t.wgsl", "S", Good { a, b, c, d }),
            ContractRow::new("t.wgsl", "T", 4, vec![("x", 0)]),
        ]
    }

    #[test]
    fn a_matching_registry_passes_the_ratchet() {
        let m = module();
        let report = run_contract_ratchet(
            &[ContractInput {
                file: "t.wgsl",
                module: &m,
            }],
            &rows(),
            &[],
        );
        assert!(report.ok(), "{:?}", report.problems());
        assert_eq!((report.bound, report.covered), (2, 2));
    }

    #[test]
    fn a_size_or_offset_mismatch_fails() {
        let m = module();
        let mut bad = rows();
        bad[0].size = 28;
        bad[0].members[3].1 = 24;
        let report = run_contract_ratchet(
            &[ContractInput {
                file: "t.wgsl",
                module: &m,
            }],
            &bad,
            &[],
        );
        assert_eq!(report.mismatches.len(), 2, "{:?}", report.mismatches);
        assert!(report.mismatches[0].contains("WGSL size 32 but the Rust type is 28"));
        assert!(report.mismatches[1].contains("member `d` is at WGSL offset 28"));
    }

    #[test]
    fn an_unlisted_stale_or_dead_entry_fails() {
        let m = module();
        let inputs = [ContractInput {
            file: "t.wgsl",
            module: &m,
        }];
        // T has neither a row nor an allowlist line.
        let report = run_contract_ratchet(&inputs, &rows()[..1], &[]);
        assert_eq!(report.unlisted, vec!["t.wgsl::T".to_string()]);
        // An allowlist line for a covered struct, and for a struct that is not bound, is stale.
        let allow = [
            AllowEntry {
                file: "t.wgsl",
                wgsl_struct: "T",
                reason: "r",
            },
            AllowEntry {
                file: "t.wgsl",
                wgsl_struct: "S",
                reason: "r",
            },
            AllowEntry {
                file: "t.wgsl",
                wgsl_struct: "Gone",
                reason: "r",
            },
        ];
        let report = run_contract_ratchet(&inputs, &rows()[..1], &allow);
        assert!(report.unlisted.is_empty());
        assert_eq!(report.stale_allow.len(), 2, "{:?}", report.stale_allow);
        // A row for a struct that no module declares is dead.
        let dead = [ContractRow::new("t.wgsl", "Nope", 4, vec![])];
        let report = run_contract_ratchet(&inputs, &dead, &[]);
        assert_eq!(report.dead_rows, vec!["t.wgsl::Nope".to_string()]);
    }

    #[test]
    fn a_missing_wgsl_member_and_an_unlisted_one_fail_unless_partial() {
        let m = module();
        let inputs = [ContractInput {
            file: "t.wgsl",
            module: &m,
        }];
        let short = ContractRow::new("t.wgsl", "S", 32, vec![("a", 0), ("b", 16), ("c", 24)]);
        let report = run_contract_ratchet(&inputs, std::slice::from_ref(&short), &[]);
        assert!(
            report
                .mismatches
                .iter()
                .any(|m| m.contains("WGSL member `d` has no Rust field"))
        );
        let report = run_contract_ratchet(&inputs, &[short.partial()], &[]);
        assert!(report.mismatches.is_empty());
        let ghost = ContractRow::new("t.wgsl", "S", 32, vec![("zz", 0)]).partial();
        let report = run_contract_ratchet(&inputs, &[ghost], &[]);
        assert!(report.mismatches[0].contains("lacks"));
    }
}

/// Every `regia_render` module the ratchet scans, composed as the renderer composes it: the
/// frame's entry shaders, the modules the renderer builds beyond them, and every forward and
/// graph permutation of every shipped preset (`full`) or the matrix's fixed subset. One module
/// per file and define set. Named by file.
///
/// # Errors
///
/// A module fails to compose or parse (the line names it).
pub fn render_contract_modules(full: bool) -> Result<Vec<(String, naga::Module)>, String> {
    use crate::shader_check::{MatrixScope, shader_matrix_inputs};
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for input in shader_matrix_inputs(if full {
        MatrixScope::Full
    } else {
        MatrixScope::Subset
    }) {
        let key = (input.file.clone(), crate::shader_check::define_key(&input.defs));
        if !seen.insert(key) {
            continue;
        }
        let module = if input.plain {
            parse_plain(&input.source).map_err(|e| format!("{}: {e}", input.file))?
        } else {
            crate::shader_lib::compose_source(&input.source, &input.file, input.defs.clone())
                .map_err(|e| format!("{}: {e}", input.file))?
        };
        out.push((input.file.clone(), module));
    }
    Ok(out)
}

#[cfg(test)]
mod dump {
    /// Prints every bound struct of the engine (`cargo test -p regia_render --lib shader_contract::dump -- --ignored --nocapture`).
    #[test]
    #[ignore = "a listing for the registry author"]
    fn list_bound_structs() {
        let modules = super::render_contract_modules(true).expect("compose");
        let mut all = std::collections::BTreeSet::new();
        for (file, module) in &modules {
            let layouts = super::struct_layouts(module).expect("layout");
            for b in super::bound_structs(module) {
                let l = &layouts[&b.name];
                let members: Vec<String> = l.members.iter().map(|(n, o)| format!("{n}@{o}")).collect();
                all.insert(format!(
                    "{file}\t{}\t{:?}\t{}\t{}",
                    b.name,
                    b.space,
                    l.size,
                    members.join(" ")
                ));
            }
        }
        for line in &all {
            println!("BOUND {line}");
        }
        println!("BOUND-COUNT {} modules {}", all.len(), modules.len());
    }
}

#[cfg(test)]
mod render_ratchet {
    /// Every uniform and storage struct of the engine's shaders has a row that agrees with the Rust
    /// type, or an allowlist line with a reason.
    #[test]
    fn every_bound_struct_of_regia_render_is_checked_or_allowed() {
        let rows = crate::render_contract_rows();
        let allow = crate::render_contract_allowlist();
        let report = crate::ratchet_sources(true, &[], &[], &rows, &allow).expect("compose");
        assert!(
            report.ok(),
            "{} problems:\n{}",
            report.problems().len(),
            report.problems().join("\n")
        );
    }

    /// Prints the allowlist for the structs that have no row yet
    /// (`cargo test -p regia_render --lib print_allowlist -- --ignored --nocapture`).
    #[test]
    #[ignore = "a generator for the allowlist"]
    fn print_allowlist() {
        let rows = crate::render_contract_rows();
        let report = crate::ratchet_sources(true, &[], &[], &rows, &[]).expect("compose");
        for line in &report.unlisted {
            let (file, name) = line.split_once("::").expect("pair");
            println!("ALLOW    AllowEntry {{ file: \"{file}\", wgsl_struct: \"{name}\", reason: PENDING }},");
        }
        for line in report.mismatches.iter().chain(&report.dead_rows) {
            println!("PROBLEM {line}");
        }
    }
}
