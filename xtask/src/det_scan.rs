// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/math.rs, commit f7e8f4740), ADR-8012.
// Mark: adapt (lines 52-256, the source-scan test): `forbidden_method`, `call_with_prefix` and the
// test's samples. Named issues fixed: the method list adds the f64-only and fused forms (`hypot`,
// `tanh`, `cbrt`, `mul_add`, `powi` and the other hyperbolic, log and exp variants) and drops `pow`
// (Rust has no float `pow`; integer `pow` is exact and the integer-mm geometry of ADR-8012 A-4 uses
// it); a trailing `//` comment no longer hides the live call before it, and string literals and
// block comments are blanked, so a test module is skipped by its braces instead of "to end of file".
// The rest (rules beyond transcendentals, the closure list, pins as data, clippy.toml checks) is new.

//! `det-scan`: the lockstep determinism scan (ADR-8012 A-4, ADR-8013).
//!
//! Scans the `src/` of every crate on the lockstep-closure list ([`CLOSURE`]) and every `[[module]]`
//! directory it lists in a crate outside the closure (ADR-8102), with comments, string contents and
//! `#[cfg(test)]` items blanked, and reports each line that breaks a lockstep rule:
//!
//! | Rule | Flags |
//! |---|---|
//! | `std_float_fn` | a std float method whose bits depend on the platform libm, on fusion or (for `min`, `max`, `clamp`) on the zero and NaN sign a target picks ([`FORBIDDEN_METHODS`]), as `x.m(`, `f32::m(`, `f64::m(`, `Float::m(` or `<T as Float>::m(`, and the path forms used as a value (`.map(f32::sin)`); `x.min(`/`x.max(`/`x.clamp(` only on a line with a float hint ([`ORDER_METHODS`]) |
//! | `direct_libm` | `libm::` outside the closure's `math_module` (`regia_detsim::math`) |
//! | `fast_math` | `fadd_fast`-style intrinsics and `algebraic_*` float methods (they allow reassociation) |
//! | `f64_parse` | text parsed as `f64` (narrowed to `f32` later it is rounded twice) |
//! | `hash_iteration` | a `HashMap`/`HashSet`-family type (its iteration order differs per process) |
//! | `rayon` | rayon or a parallel iterator in step code |
//! | `simd_type` | glam's SIMD types (`Vec3A`, `Vec4`, `Quat`, `Mat2`, `Mat3A`, `Mat4`, `Affine2`, `Affine3A`), `std::simd`, `__m128`-style and NEON vector types, `_mm*` intrinsics and the `core::arch` intrinsic modules |
//! | `glam_trig` | a glam scalar-type function that calls std trig or `exp` inside glam ([`GLAM_TRIG`]), as `Mat3::from_rotation_y(` or a value, or a distinctive method call such as `v.slerp(` or `v.to_angle(` |
//!
//! The host-dependence rules (`wall_clock`, `os_random`, `env_read`, `address_order`,
//! `thread_spawn`, `arch_cfg`, `float_sort`) also inspect multiline tokens. See [`RULE_DETAILS`]
//! for the complete catalog and rationale shared by both scan modes and agent guides.
//!
//! A finding is waived by `// det-scan: allow(<rule>[, <rule>]): <reason>` on its line or the line
//! above; an allow without a reason is itself a finding (`allow_without_reason`). An as-is copy whose
//! bytes must not change lists its known sites as `[[pin]]` rows instead (each must match exactly one
//! live line). The scan also checks that every workspace crate a closure crate depends on is listed
//! or exempted, and that each closure crate's `clippy.toml` denies [`clippy_paths`] plus every path
//! of the workspace root `clippy.toml` (clippy does not merge parent configs).
//!
//! A `[[module]]` row (ADR-8049) lists a module directory or file inside a crate that is not lockstep
//! code as a whole (for example `crates/regia_net/src/lockstep`): its sources get the same line rules, but
//! the crate's dependencies and `clippy.toml` are not checked. The owning crate keeps the module
//! from importing its non-lockstep code with its own test.
//!
//! `cargo xtask det-scan` prints the findings; `--json` prints one JSON object for agents.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use serde::Deserialize;

mod external;
mod host_rules;

#[cfg(test)]
mod rules_tests;

/// Arguments for the internal closure scan or a bounded external workspace scan.
#[derive(clap::Args, Debug, Default)]
pub struct Args {
    /// Scan this Cargo workspace instead of the Regia checkout.
    #[arg(long, value_name = "ROOT", requires = "crates")]
    pub external: Option<PathBuf>,
    /// Workspace package name to scan (repeatable; required with --external).
    #[arg(long = "crate", value_name = "NAME", requires = "external")]
    pub crates: Vec<String>,
    /// Root-relative source file allowed to call libm directly.
    #[arg(long, value_name = "PATH", requires = "external")]
    pub math_module: Option<PathBuf>,
    /// Also apply the external root's closure policy and clippy.toml checks.
    #[arg(long, requires = "external")]
    pub with_closure: bool,
    /// Print one JSON object for automation.
    #[arg(long)]
    pub json: bool,
    /// Print the rule catalog as a Markdown table (or JSON with --json).
    #[arg(long, conflicts_with_all = ["external", "crates", "math_module", "with_closure"])]
    pub list_rules: bool,
}

/// The lockstep-closure list, relative to the workspace root.
pub const CLOSURE: &str = "docs/ci/lockstep-closure.toml";

/// Std float methods whose result depends on the platform's libm, on fused rounding, or on how a
/// target lowers them. Longer names first so the reported method is the one that was called.
/// `clamp`, `max` and `min` are here because the zero sign (and NaN sign) of their result differs
/// between x86-64 optimization levels and aarch64 (ADR-8085); see [`ORDER_METHODS`].
pub const FORBIDDEN_METHODS: &[&str] = &[
    "sin_cos", "mul_add", "exp_m1", "ln_1p", "asinh", "acosh", "atanh", "atan2", "hypot", "log10", "log2",
    "exp2", "powf", "powi", "clamp", "sinh", "cosh", "tanh", "asin", "acos", "atan", "sqrt", "cbrt", "sin",
    "cos", "tan", "exp", "log", "max", "min", "ln",
];

/// The [`FORBIDDEN_METHODS`] that integers (and `Ord` types) also have. A text scan cannot see a
/// receiver's type, so their method form `x.m(..)` (and `<T as Trait>::m(`) is flagged only on a line
/// with a float hint: an `f32`/`f64` token or a float literal. `f32::m`, `f64::m` and `Float::m` are
/// always flagged, and `.m()` with no argument (`Iterator::max`) never is. The lockstep `clippy.toml`
/// denies `f32::m`/`f64::m` by type, so a float call the scan cannot see still fails clippy.
pub const ORDER_METHODS: &[&str] = &["clamp", "max", "min"];

/// glam's scalar float types (allowed in lockstep code, unlike the SIMD ones) and those of their
/// associated functions and methods that call std `sin_cos`, `sin`, `cos`, `tan`, `atan2`, `exp`,
/// `exp2`, `ln`, `log2`, `powf` or `mul_add` inside glam (the workspace glam uses std math, not libm),
/// so their bits depend on the platform's libm (ADR-8085, review row F2). Read from glam 0.33.12's
/// source, transitively (`Vec3::rotate_axis` goes through `Quat::from_axis_angle`, `Mat3::from_euler`
/// through the shared Euler code). Build rotations from `regia_detsim::math::{sin, cos}` instead:
/// `Mat3::from_cols` with the sine and cosine, or the matrix written out.
pub const GLAM_TRIG: &[(&str, &[&str])] = &[
    ("Vec2", VEC2_TRIG),
    ("DVec2", VEC2_TRIG),
    ("Vec3", VEC3_TRIG),
    ("DVec3", VEC3_TRIG),
    (
        "DVec4",
        &[
            "cos", "exp", "exp2", "ln", "log2", "mul_add", "powf", "sin", "sin_cos",
        ],
    ),
    ("Mat3", MAT3_TRIG),
    ("DMat3", MAT3_TRIG),
    ("DMat2", &["from_angle", "from_scale_angle"]),
    (
        "DMat4",
        &[
            "from_axis_angle",
            "from_euler",
            "from_rotation_x",
            "from_rotation_y",
            "from_rotation_z",
            "to_euler",
            "perspective_infinite_lh",
            "perspective_infinite_reverse_lh",
            "perspective_infinite_reverse_rh",
            "perspective_infinite_rh",
            "perspective_lh",
            "perspective_rh",
            "perspective_rh_gl",
        ],
    ),
    (
        "DQuat",
        &[
            "from_axis_angle",
            "from_euler",
            "from_rotation_arc",
            "from_rotation_arc_colinear",
            "from_rotation_x",
            "from_rotation_y",
            "from_rotation_z",
            "from_scaled_axis",
            "rotate_towards",
            "slerp",
            "slerp_long",
            "to_axis_angle",
            "to_euler",
            "to_scaled_axis",
        ],
    ),
    ("Affine3", AFFINE3_TRIG),
    ("DAffine3", AFFINE3_TRIG),
    (
        "DAffine2",
        &[
            "from_angle",
            "from_angle_translation",
            "from_scale_angle_translation",
            "to_scale_angle_translation",
        ],
    ),
];

const VEC2_TRIG: &[&str] = &[
    "cos",
    "exp",
    "exp2",
    "from_angle",
    "ln",
    "log2",
    "mul_add",
    "powf",
    "rotate_angle",
    "rotate_towards",
    "sin",
    "sin_cos",
    "to_angle",
];

const VEC3_TRIG: &[&str] = &[
    "angle_to",
    "cos",
    "exp",
    "exp2",
    "ln",
    "log2",
    "mul_add",
    "powf",
    "rotate_axis",
    "rotate_towards",
    "rotate_x",
    "rotate_y",
    "rotate_z",
    "sin",
    "sin_cos",
    "slerp",
];

const MAT3_TRIG: &[&str] = &[
    "from_angle",
    "from_axis_angle",
    "from_euler",
    "from_rotation_x",
    "from_rotation_y",
    "from_rotation_z",
    "from_scale_angle_translation",
    "to_euler",
];

const AFFINE3_TRIG: &[&str] = &[
    "from_axis_angle",
    "from_rotation_x",
    "from_rotation_y",
    "from_rotation_z",
];

/// The [`GLAM_TRIG`] methods whose names are distinctive enough to flag in method form (`v.slerp(`)
/// without seeing the receiver's type. The vector `sin`/`cos`/`exp`/... methods are already caught
/// by `std_float_fn`; the constructors are flagged in path form (`Mat3::from_rotation_y`).
const GLAM_TRIG_METHOD_FORMS: &[&str] = &[
    "to_scale_angle_translation",
    "to_scaled_axis",
    "rotate_towards",
    "to_axis_angle",
    "rotate_angle",
    "rotate_axis",
    "slerp_long",
    "rotate_x",
    "rotate_y",
    "rotate_z",
    "to_angle",
    "angle_to",
    "to_euler",
    "slerp",
];

// Keep the accepted waiver names and the human/agent catalog in one declaration.
macro_rules! rule_catalog {
    ($(($name:literal, $why:literal)),+ $(,)?) => {
        /// The source rule names a finding or an allow comment may carry.
        pub const RULES: &[&str] = &[$($name),+];
        /// Source rules and their rationale, shared by the CLI and generated book table.
        pub const RULE_DETAILS: &[(&str, &str)] = &[$(($name, $why)),+];
    };
}

rule_catalog! {
    ("std_float_fn", "Platform float math, fusion, and min/max/clamp zero or NaN signs can change bits; includes function values. Use the deterministic math and float wrappers."),
    ("direct_libm", "Direct libm calls bypass the audited math boundary; only the selected math module is exempt."),
    ("fast_math", "Fast intrinsics and algebraic methods permit reassociation or contraction, changing rounding."),
    ("f64_parse", "Parsing f64 before narrowing to f32 can round twice; parse the simulation's precision directly."),
    ("hash_iteration", "Hash container order can differ per process; use ordered containers or justify lookup-only use."),
    ("rayon", "Parallel iteration can change simulation update and reduction order."),
    ("simd_type", "SIMD types and intrinsics can combine lanes differently across architectures."),
    ("glam_trig", "Scalar glam angle and rotation functions can call platform math internally; build from deterministic sine and cosine."),
    ("wall_clock", "Instant/SystemTime reads, UNIX_EPOCH and chrono/time clocks depend on the host; use recorded tick counts."),
    ("os_random", "OS-seeded RNGs and RandomState differ between peers; use an explicit recorded seed and deterministic streams."),
    ("env_read", "Runtime environment and process arguments vary by host; pass agreed configuration into the simulation. Compile-time option_env! is allowed."),
    ("address_order", "Pointer addresses vary by allocation and process; use stable entity IDs for comparisons and sort keys. Review and waive non-ordering casts."),
    ("thread_spawn", "Thread/task scheduling can change update order; create workers outside the tick and merge results in a fixed order."),
    ("arch_cfg", "Architecture and CPU-feature branches can select different arithmetic; only reviewed equivalence boundaries may waive them."),
    ("float_sort", "partial_cmp sort comparators can panic on NaNs or give inconsistent ordering; use total_cmp with a defined NaN policy or an integer key."),
}

/// Render the book's rules-and-rationale table directly from the source rule catalog.
pub fn rules_markdown() -> String {
    let mut text = String::from("| Rule | Why |\n|---|---|\n");
    for (rule, why) in RULE_DETAILS {
        text.push_str(&format!("| `{rule}` | {why} |\n"));
    }
    text.push_str(
        "| `allow_without_reason` | A waiver needs a nonempty reason so its safety can be reviewed. |\n",
    );
    text
}

/// Hash containers whose iteration order is not fixed (`ahash`/`foldhash`/`RandomState`).
const HASH_TYPES: &[&str] = &[
    "HashMap",
    "HashSet",
    "HashTable",
    "hash_map",
    "hash_set",
    "DashMap",
    "DashSet",
];

/// SIMD vector types: glam's SSE2/NEON-backed types, portable SIMD and the platform vector types.
const SIMD_TYPES: &[&str] = &[
    "Vec3A",
    "Vec4",
    "Quat",
    "Mat2",
    "Mat3A",
    "Mat4",
    "Affine2",
    "Affine3A",
    "BVec3A",
    "BVec4A",
    "Simd",
    "f32x2",
    "f32x4",
    "f32x8",
    "f32x16",
    "f64x2",
    "f64x4",
    "f64x8",
    "__m128",
    "__m128d",
    "__m128i",
    "__m256",
    "__m256d",
    "__m256i",
    "__m512",
    "__m512d",
    "float32x2_t",
    "float32x4_t",
    "float64x1_t",
    "float64x2_t",
    "v128",
];

/// Float intrinsics and methods that let the compiler reassociate or contract.
const FAST_MATH: &[&str] = &["fadd_fast", "fsub_fast", "fmul_fast", "fdiv_fast", "frem_fast"];

/// Hash-container methods that iterate, denied by the lockstep `clippy.toml`.
const HASH_ITER_METHODS: &[(&str, &[&str])] = &[
    (
        "std::collections::HashMap",
        &[
            "iter",
            "iter_mut",
            "keys",
            "values",
            "values_mut",
            "drain",
            "into_keys",
            "into_values",
            "retain",
        ],
    ),
    ("std::collections::HashSet", &["iter", "drain", "retain"]),
];

/// Host-dependent std methods that clippy can resolve even through imports or aliases.
const HOST_METHODS: &[&str] = &[
    "std::time::Instant::now",
    "std::time::SystemTime::now",
    "std::thread::spawn",
    "std::thread::Builder::spawn",
    "std::thread::Builder::spawn_scoped",
    "std::env::var",
    "std::env::var_os",
    "std::env::vars",
    "std::env::vars_os",
    "std::env::args",
    "std::env::args_os",
    "std::env::current_dir",
    "std::env::current_exe",
];

/// Host-dependent types whose use clippy can resolve through aliases.
const HOST_TYPES: &[&str] = &["std::collections::hash_map::RandomState", "std::thread::Builder"];

/// The `disallowed-methods` paths every lockstep crate's `clippy.toml` must carry: the `f32::` and
/// `f64::` forms of [`FORBIDDEN_METHODS`], the `glam::` paths of [`GLAM_TRIG`] and the hash-container
/// iteration methods, plus host clock, environment and thread methods.
pub fn clippy_paths() -> Vec<String> {
    let mut out: Vec<String> = HOST_METHODS.iter().map(|p| (*p).to_owned()).collect();
    for ty in ["f32", "f64"] {
        for m in FORBIDDEN_METHODS {
            out.push(format!("{ty}::{m}"));
        }
    }
    for (ty, methods) in GLAM_TRIG {
        for m in *methods {
            out.push(format!("glam::{ty}::{m}"));
        }
    }
    for (ty, methods) in HASH_ITER_METHODS {
        for m in *methods {
            out.push(format!("{ty}::{m}"));
        }
    }
    out
}

/// The parsed [`CLOSURE`] file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Closure {
    /// The one file allowed to call `libm` directly (repository-relative).
    pub math_module: String,
    /// The crates in the lockstep closure.
    #[serde(default, rename = "crate")]
    pub crates: Vec<ClosureCrate>,
    /// Modules of crates that are not lockstep as a whole (ADR-8049), e.g. the match rules core of
    /// `regia_framework` (ADR-8102) and the lockstep core of `regia_net` (ADR-8146): scanned with the
    /// same rules, without the crate's dependency and clippy checks.
    #[serde(default, rename = "module")]
    pub modules: Vec<ClosureModule>,
    /// Workspace crates a closure crate depends on that are not lockstep code, with the reason.
    #[serde(default)]
    pub exempt: Vec<Exempt>,
    /// Known sites of an as-is copy, each matched against exactly one live line.
    #[serde(default)]
    pub pin: Vec<Pin>,
}

/// One crate of the closure.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureCrate {
    /// Repository-relative crate directory, e.g. `crates/regia_detsim`.
    pub path: String,
    /// Why the crate runs on a lockstep tick.
    pub why: String,
}

/// One module of the closure: a directory or `.rs` file inside a crate that is not lockstep code as a
/// whole. Its sources are scanned with the line rules; its crate's dependencies and `clippy.toml` are
/// not checked (ADR-8049, ADR-8102, ADR-8146).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureModule {
    /// Repository-relative module directory or file, e.g. `crates/regia_net/src/lockstep`.
    pub path: String,
    /// Why the module runs on a lockstep tick.
    pub why: String,
}

/// A workspace dependency of a closure crate that is not lockstep code.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exempt {
    /// The crate name, e.g. `regia_core`.
    pub name: String,
    /// Why nothing of it runs on the tick (or why its use there is safe).
    pub why: String,
}

/// A known site in an as-is copy.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    /// Repository-relative file.
    pub path: String,
    /// The whole line, trimmed, exactly as in the file.
    pub text: String,
    /// The rule the line trips.
    pub rule: String,
    /// Why the site stays (e.g. "as-is copy: changing it moves the sim's bits").
    pub why: String,
}

/// One line that breaks a rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Repository-relative file.
    pub file: String,
    /// One-based line.
    pub line: usize,
    /// One of [`RULES`], or `allow_without_reason`.
    pub rule: &'static str,
    /// What matched, e.g. the method name or the type.
    pub what: String,
    /// The line, trimmed.
    pub text: String,
}

/// The result of a scan.
#[derive(Debug, Default)]
pub struct Report {
    /// The scanned crate directories.
    pub crates: Vec<String>,
    /// The scanned module paths (`[[module]]` rows).
    pub modules: Vec<String>,
    /// How many `.rs` files were scanned.
    pub files: usize,
    /// Lines that break a rule.
    pub findings: Vec<Finding>,
    /// Problems with the closure itself: an unlisted dependency, a missing clippy entry, a stale pin.
    pub problems: Vec<String>,
}

impl Report {
    /// `true` when nothing was found.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty() && self.problems.is_empty()
    }

    /// The report as one JSON value (the `--json` output).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ok": self.is_clean(),
            "closure": CLOSURE,
            "crates": self.crates,
            "modules": self.modules,
            "files": self.files,
            "findings": self.findings.iter().map(|f| serde_json::json!({
                "file": f.file,
                "line": f.line,
                "rule": f.rule,
                "what": f.what,
                "text": f.text,
            })).collect::<Vec<_>>(),
            "problems": self.problems,
        })
    }
}

/// Run the existing Regia closure scan. Kept for callers that do not need external options.
pub fn run(root: &Path, json: bool) -> Result<ExitCode> {
    run_args(
        root,
        &Args {
            json,
            ..Args::default()
        },
    )
}

/// Run a scan or print its rule catalog. External input errors produce a failed report, including
/// under `--json`, so an agent always receives one JSON object for a syntactically valid command.
pub fn run_args(root: &Path, args: &Args) -> Result<ExitCode> {
    if args.list_rules {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "rules": RULE_DETAILS.iter().map(|(rule, why)| serde_json::json!({"rule": rule, "why": why})).collect::<Vec<_>>()
                }))?
            );
        } else {
            print!("{}", rules_markdown());
        }
        return Ok(ExitCode::SUCCESS);
    }
    let report = if args.external.is_some() {
        match external::scan(args) {
            Ok(report) => report,
            Err(error) => Report {
                problems: vec![format!("external_input: {error:#}")],
                ..Report::default()
            },
        }
    } else {
        scan(root)?
    };
    if args.json {
        let mut json = report.to_json();
        if args.external.is_some() && !args.with_closure {
            json["closure"] = serde_json::Value::Null;
        }
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        for f in &report.findings {
            println!("{}:{}: {} `{}`: {}", f.file, f.line, f.rule, f.what, f.text);
        }
        for p in &report.problems {
            let label = if args.external.is_some() {
                "det-scan"
            } else {
                CLOSURE
            };
            println!("{label}: {p}");
        }
        let problems = if args.external.is_some() {
            "problem(s)"
        } else {
            "closure problem(s)"
        };
        println!(
            "det-scan: {} crate(s), {} module(s), {} file(s), {} finding(s), {} {problems}",
            report.crates.len(),
            report.modules.len(),
            report.files,
            report.findings.len(),
            report.problems.len()
        );
    }
    Ok(if report.is_clean() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Scan the closure listed in [`CLOSURE`] under `root`.
pub fn scan(root: &Path) -> Result<Report> {
    let path = root.join(CLOSURE);
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let closure: Closure = toml::from_str(&text).with_context(|| format!("parsing {CLOSURE}"))?;
    scan_closure(root, &closure)
}

/// Scan `closure` under `root`.
pub fn scan_closure(root: &Path, closure: &Closure) -> Result<Report> {
    let mut report = Report::default();
    let mut pin_counts = vec![0usize; closure.pin.len()];
    for (i, pin) in closure.pin.iter().enumerate() {
        if !RULES.contains(&pin.rule.as_str()) {
            report
                .problems
                .push(format!("pin {i} (`{}`): unknown rule `{}`", pin.path, pin.rule));
        }
        if pin.why.trim().is_empty() {
            report
                .problems
                .push(format!("pin {i} (`{}`): `why` is empty", pin.path));
        }
    }
    let names = closure_names(root, closure, &mut report.problems);
    let internal = workspace_crates(root)?;
    let root_clippy = clippy_methods(&root.join("clippy.toml")).unwrap_or_default();

    for krate in &closure.crates {
        let dir = root.join(&krate.path);
        report.crates.push(krate.path.clone());
        if krate.why.trim().is_empty() {
            report
                .problems
                .push(format!("crate `{}`: `why` is empty", krate.path));
        }
        check_dependencies(
            &dir,
            &krate.path,
            &names,
            &internal,
            closure,
            &mut report.problems,
        );
        check_clippy(&dir, &krate.path, &root_clippy, &mut report.problems);

        let mut files = Vec::new();
        collect_rs_files(&dir.join("src"), &mut files);
        scan_files(root, closure, files, &mut pin_counts, &mut report)?;
    }
    for module in &closure.modules {
        report.modules.push(module.path.clone());
        if module.why.trim().is_empty() {
            report
                .problems
                .push(format!("module `{}`: `why` is empty", module.path));
        }
        let path = root.join(&module.path);
        let mut files = Vec::new();
        if path.is_dir() {
            collect_module_files(&path, &mut files);
        } else if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
            files.push(path);
        } else {
            report
                .problems
                .push(format!("module `{}`: not a directory or `.rs` file", module.path));
            continue;
        }
        if files.is_empty() {
            report
                .problems
                .push(format!("module `{}`: no `.rs` files", module.path));
        }
        scan_files(root, closure, files, &mut pin_counts, &mut report)?;
    }
    for (pin, count) in closure.pin.iter().zip(pin_counts) {
        if count != 1 {
            report.problems.push(format!(
                "pin `{}` `{}` ({}) matched {count} live line(s), expected 1",
                pin.path, pin.text, pin.rule
            ));
        }
    }
    Ok(report)
}

/// Scan `files` (sorted here) into `report`, counting pinned sites.
fn scan_files(
    root: &Path,
    closure: &Closure,
    mut files: Vec<PathBuf>,
    pin_counts: &mut [usize],
    report: &mut Report,
) -> Result<()> {
    files.sort();
    for file in files {
        let rel = rel_path(root, &file);
        let text = std::fs::read_to_string(&file).with_context(|| format!("reading {rel}"))?;
        report.files += 1;
        for finding in scan_source(&rel, &text, rel == closure.math_module) {
            let pinned = closure
                .pin
                .iter()
                .position(|p| p.path == finding.file && p.text == finding.text && p.rule == finding.rule);
            match pinned {
                Some(i) => pin_counts[i] += 1,
                None => report.findings.push(finding),
            }
        }
    }
    Ok(())
}

/// The `.rs` files of a module directory, without the files a `#[cfg(test)] mod name;` line of the
/// same directory declares (a test-only file is not step code).
fn collect_module_files(dir: &Path, out: &mut Vec<PathBuf>) {
    collect_module_files_in(dir, &BTreeSet::new(), out);
}

/// [`collect_module_files`] for `dir`, leaving out `inherited`: the test-only children that the
/// parent directory's `<dir>.rs` declares (ADR-8892; a non-`mod.rs` file's children live in the
/// directory of its name).
fn collect_module_files_in(dir: &Path, inherited: &BTreeSet<String>, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    let mut test_only = inherited.clone();
    // Per file stem: the test-only children it declares (for its own directory `<stem>/`).
    let mut children: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in entries.iter().filter(|p| p.is_file()) {
        let file_stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let mut gated = false;
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if line.starts_with("#[cfg(test)]") || line.starts_with("#[cfg(all(test") {
                gated = true;
                continue;
            }
            if gated
                && let Some(name) = line
                    .trim_start_matches("pub(crate) ")
                    .trim_start_matches("pub ")
                    .strip_prefix("mod ")
                    .and_then(|rest| rest.strip_suffix(';'))
            {
                test_only.insert(name.trim().to_owned());
                children
                    .entry(file_stem.clone())
                    .or_default()
                    .insert(name.trim().to_owned());
            }
            gated = false;
        }
    }
    let none = BTreeSet::new();
    for path in entries {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if test_only.contains(stem) {
            continue;
        }
        if path.is_dir() {
            collect_module_files_in(&path, children.get(stem).unwrap_or(&none), out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Package names of the closure crates (from their `Cargo.toml`).
fn closure_names(root: &Path, closure: &Closure, problems: &mut Vec<String>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for krate in &closure.crates {
        match package_name(&root.join(&krate.path)) {
            Some(name) => {
                names.insert(name);
            }
            None => problems.push(format!(
                "crate `{}`: no readable Cargo.toml with a package name",
                krate.path
            )),
        }
    }
    names
}

fn package_name(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value.get("package")?.get("name")?.as_str().map(str::to_owned)
}

/// Names of the workspace's own crates: the path entries of `[workspace.dependencies]`.
fn workspace_crates(root: &Path) -> Result<BTreeSet<String>> {
    let text =
        std::fs::read_to_string(root.join("Cargo.toml")).context("reading the workspace Cargo.toml")?;
    let value: toml::Value = toml::from_str(&text).context("parsing the workspace Cargo.toml")?;
    let mut out = BTreeSet::new();
    if let Some(deps) = value
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for (name, spec) in deps {
            if spec.get("path").is_some() {
                out.insert(name.clone());
            }
        }
    }
    Ok(out)
}

/// Every workspace crate a closure crate depends on (normal and target dependencies) is in the closure
/// or exempted.
fn check_dependencies(
    dir: &Path,
    rel: &str,
    names: &BTreeSet<String>,
    internal: &BTreeSet<String>,
    closure: &Closure,
    problems: &mut Vec<String>,
) {
    let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
        return; // reported by `closure_names`
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        problems.push(format!("crate `{rel}`: Cargo.toml does not parse"));
        return;
    };
    let mut tables: Vec<&toml::Table> = Vec::new();
    if let Some(t) = value.get("dependencies").and_then(|d| d.as_table()) {
        tables.push(t);
    }
    if let Some(targets) = value.get("target").and_then(|t| t.as_table()) {
        for target in targets.values() {
            if let Some(t) = target.get("dependencies").and_then(|d| d.as_table()) {
                tables.push(t);
            }
        }
    }
    for table in tables {
        for (key, spec) in table {
            let name = spec.get("package").and_then(|p| p.as_str()).unwrap_or(key);
            let is_internal = internal.contains(name) || spec.get("path").is_some();
            if !is_internal || names.contains(name) {
                continue;
            }
            if !closure
                .exempt
                .iter()
                .any(|e| e.name == name && !e.why.trim().is_empty())
            {
                problems.push(format!(
                    "crate `{rel}` depends on the workspace crate `{name}`, which is neither a [[crate]] of the \
                     closure nor an [[exempt]] row with a `why`"
                ));
            }
        }
    }
}

/// The `disallowed-methods` paths of a clippy config, or `None` when it is missing or unreadable.
fn clippy_methods(path: &Path) -> Option<BTreeSet<String>> {
    clippy_entries(path, "disallowed-methods")
}

fn clippy_entries(path: &Path, key: &str) -> Option<BTreeSet<String>> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    let list = value.get(key)?.as_array()?;
    Some(
        list.iter()
            .filter_map(|e| e.as_str().or_else(|| e.get("path").and_then(|p| p.as_str())))
            .map(str::to_owned)
            .collect(),
    )
}

/// The crate's `clippy.toml` denies every lockstep path and keeps the root config's entries.
fn check_clippy(dir: &Path, rel: &str, root_clippy: &BTreeSet<String>, problems: &mut Vec<String>) {
    let Some(have) = clippy_methods(&dir.join("clippy.toml")) else {
        problems.push(format!(
            "crate `{rel}` has no clippy.toml with `disallowed-methods` (copy crates/regia_detsim/clippy.toml)"
        ));
        return;
    };
    let mut missing: Vec<String> = clippy_paths()
        .into_iter()
        .chain(root_clippy.iter().cloned())
        .filter(|p| !have.contains(p))
        .collect();
    let types = clippy_entries(&dir.join("clippy.toml"), "disallowed-types").unwrap_or_default();
    missing.extend(
        HOST_TYPES
            .iter()
            .filter(|p| !types.contains(**p))
            .map(|p| (*p).to_owned()),
    );
    if !missing.is_empty() {
        problems.push(format!(
            "crate `{rel}`: clippy.toml does not deny {}",
            missing.join(", ")
        ));
    }
}

fn rel_path(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// The findings of one source file. `math_module` exempts it from `direct_libm`.
pub fn scan_source(file: &str, text: &str, math_module: bool) -> Vec<Finding> {
    let original: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let (blanked, comments) = blank_source(text);
    let live_source = without_test_items(&blanked);
    let mut host_findings = host_rules::findings(&live_source);
    for (index, live) in live_source.lines().enumerate() {
        let line_no = index + 1;
        let raw = original.get(line_no - 1).copied().unwrap_or("");
        let comment = comments.get(&line_no).map_or("", String::as_str);
        let above = comments.get(&(line_no - 1)).map(String::as_str);
        for (rule, what) in line_findings(live, math_module)
            .into_iter()
            .chain(host_findings.remove(&line_no).unwrap_or_default())
        {
            match allow_for(comment, above, rule) {
                Allow::Waived => continue,
                Allow::NoReason => out.push(Finding {
                    file: file.to_owned(),
                    line: line_no,
                    rule: "allow_without_reason",
                    what: rule.to_owned(),
                    text: raw.trim().to_owned(),
                }),
                Allow::None => {}
            }
            out.push(Finding {
                file: file.to_owned(),
                line: line_no,
                rule,
                what,
                text: raw.trim().to_owned(),
            });
        }
    }
    out
}

/// The rule findings of one live (blanked) line: at most one per rule.
fn line_findings(line: &str, math_module: bool) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if let Some(method) = forbidden_method(line) {
        out.push(("std_float_fn", method.to_owned()));
    }
    if let Some(what) = glam_trig(line) {
        out.push(("glam_trig", what));
    }
    let tokens = idents(line);
    let token = |pred: &dyn Fn(&str) -> bool| tokens.iter().copied().find(|t| pred(t));
    if !math_module && path_segment(line, "libm::") {
        out.push(("direct_libm", "libm::".to_owned()));
    }
    if let Some(t) = token(&|t| FAST_MATH.contains(&t) || t.starts_with("algebraic_")) {
        out.push(("fast_math", t.to_owned()));
    }
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.contains("parse::<f64>")
        || compact.contains("f64::from_str")
        || (compact.contains(".parse(") && tokens.contains(&"f64"))
    {
        out.push(("f64_parse", "f64 parse".to_owned()));
    }
    if let Some(t) = token(&|t| HASH_TYPES.iter().any(|h| t.contains(h))) {
        out.push(("hash_iteration", t.to_owned()));
    }
    if let Some(t) = token(&|t| {
        t == "rayon" || t.starts_with("par_") || t.starts_with("into_par_") || t.contains("Parallel")
    }) {
        out.push(("rayon", t.to_owned()));
    }
    let simd_path = [
        "simd::",
        "arch::x86_64::",
        "arch::x86::",
        "arch::aarch64::",
        "arch::arm::",
        "arch::wasm32::",
    ]
    .into_iter()
    .find(|p| compact.contains(p));
    if let Some(t) = token(&|t| SIMD_TYPES.contains(&t) || t.starts_with("_mm")) {
        out.push(("simd_type", t.to_owned()));
    } else if let Some(p) = simd_path {
        out.push(("simd_type", p.to_owned()));
    }
    out
}

/// `segment` appears as a path start (not as the tail of a longer identifier).
fn path_segment(line: &str, segment: &str) -> bool {
    line.match_indices(segment).any(|(at, _)| {
        line[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
    })
}

/// The identifiers of a line, in order.
fn idents(line: &str) -> Vec<&str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .collect()
}

enum Allow {
    None,
    Waived,
    NoReason,
}

/// A `// det-scan: allow(<rules>): <reason>` comment for `rule` on the line or the line above.
fn allow_for(line: &str, above: Option<&str>, rule: &str) -> Allow {
    for text in [Some(line), above].into_iter().flatten() {
        let Some(at) = text.find("det-scan: allow(") else {
            continue;
        };
        let rest = &text[at + "det-scan: allow(".len()..];
        let Some(close) = rest.find(')') else {
            continue;
        };
        if !rest[..close].split(',').any(|r| r.trim() == rule) {
            continue;
        }
        let reason = rest[close + 1..]
            .trim_start()
            .strip_prefix(':')
            .unwrap_or("")
            .trim();
        return if reason.is_empty() {
            Allow::NoReason
        } else {
            Allow::Waived
        };
    }
    Allow::None
}

/// DO27's matcher: a forbidden method called as `x.m(`, `f32::m(`, `f64::m(` (and `Float::m(`,
/// `<f32 as Float>::m(`). The path forms also match a function used as a value (`.map(f32::sin)`,
/// `fold(0.0, f32::max)`; review row F3). An [`ORDER_METHODS`] name in method or `>::` form needs a
/// float hint on the line, and its argument-less `.m()` form (`Iterator::max`) never matches.
fn forbidden_method(line: &str) -> Option<&'static str> {
    let hint = float_hint(line);
    FORBIDDEN_METHODS.iter().copied().find(|method| {
        let order = ORDER_METHODS.contains(method);
        let gated = !order || hint;
        (gated && call_with_prefix(line, ".", method, Form::Call { empty: !order }))
            || call_with_prefix(line, "f32::", method, Form::CallOrValue)
            || call_with_prefix(line, "f64::", method, Form::CallOrValue)
            || call_with_prefix(line, "Float::", method, Form::CallOrValue)
            || (gated && call_with_prefix(line, ">::", method, Form::CallOrValue))
    })
}

/// A glam scalar-type function from [`GLAM_TRIG`] in path form (`Mat3::from_rotation_y(`, or as a
/// value), reported as `Type::method`, or a [`GLAM_TRIG_METHOD_FORMS`] method call (`v.slerp(`),
/// reported as the method name.
fn glam_trig(line: &str) -> Option<String> {
    for (ty, methods) in GLAM_TRIG {
        for m in *methods {
            let path = format!("{ty}::");
            if call_with_prefix(line, &path, m, Form::CallOrValue) {
                // `DMat3::` must not count as `Mat3::`: the type starts at a path boundary.
                let needle = format!("{ty}::{m}");
                let at_boundary = line.match_indices(&needle).any(|(at, _)| {
                    line[..at]
                        .chars()
                        .next_back()
                        .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
                });
                if at_boundary {
                    return Some(needle);
                }
            }
        }
    }
    GLAM_TRIG_METHOD_FORMS
        .iter()
        .find(|m| call_with_prefix(line, ".", m, Form::Call { empty: true }))
        .map(|m| (*m).to_owned())
}

/// The line carries an `f32`/`f64` token or a float literal (`0.5`, `1e-3`, `2f32`, `1.0_f64`).
fn float_hint(line: &str) -> bool {
    let b = line.as_bytes();
    let digit_at = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    let ident_at = |i: usize| b.get(i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
    for (i, &c) in b.iter().enumerate() {
        if !c.is_ascii_digit() || (i > 0 && (ident_at(i - 1) || b[i - 1] == b'.')) {
            continue;
        }
        // A numeric literal starts here; it is a float if it has a fraction, an exponent or a float
        // suffix.
        let mut j = i;
        while digit_at(j) || b.get(j) == Some(&b'_') {
            j += 1;
        }
        if b.get(j) == Some(&b'.') && digit_at(j + 1) {
            return true;
        }
        if matches!(b.get(j), Some(b'e' | b'E'))
            && (digit_at(j + 1) || matches!(b.get(j + 1), Some(b'-' | b'+')))
        {
            return true;
        }
        if line[j..].starts_with("f32") || line[j..].starts_with("f64") {
            return true;
        }
    }
    idents(line).iter().any(|t| *t == "f32" || *t == "f64")
}

/// How the name after a prefix must end for [`call_with_prefix`] to match.
#[derive(Clone, Copy)]
enum Form {
    /// Followed by `(`; with `empty: false`, not by `()` (an argument-less call).
    Call { empty: bool },
    /// Followed by `(` or by any non-identifier character (a path used as a value).
    CallOrValue,
}

fn call_with_prefix(line: &str, prefix: &str, method: &str, form: Form) -> bool {
    let mut from = 0;
    while let Some(rel) = line[from..].find(method) {
        let at = from + rel;
        let prefix_ok = at >= prefix.len() && &line[at - prefix.len()..at] == prefix;
        let end = at + method.len();
        let next = line.as_bytes().get(end).copied();
        let ends_ok = match form {
            Form::Call { empty } => {
                next == Some(b'(') && (empty || line.as_bytes().get(end + 1).copied() != Some(b')'))
            }
            Form::CallOrValue => next.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == b'_')),
        };
        if prefix_ok && ends_ok {
            return true;
        }
        from = end;
    }
    false
}

/// Lines compiled into the library, with comments and the contents of string and character literals
/// blanked and `#[cfg(test)]` items (a `mod x;` declaration or a braced item) removed. One-based
/// line numbers; lines left empty are skipped.
#[cfg(test)]
fn live_lines(src: &str) -> Vec<(usize, String)> {
    let live = without_test_items(&blank_comments_and_literals(src));
    live.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| (i + 1, l.to_owned()))
        .collect()
}

/// `text` with comments and the contents of string and character literals replaced by spaces; line
/// numbers and columns stay the same (the same lexer as `xtask/tests/unsafe_policy.rs`).
#[cfg(test)]
fn blank_comments_and_literals(text: &str) -> String {
    blank_source(text).0
}

/// Retain only actual line comments for waiver parsing; literal text cannot grant a waiver.
fn blank_source(text: &str) -> (String, BTreeMap<usize, String>) {
    let c: Vec<char> = text.chars().collect();
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(
            c.iter()
                .enumerate()
                .filter(|(_, ch)| **ch == '\n')
                .map(|(i, _)| i + 1),
        )
        .collect();
    let mut comments = BTreeMap::new();
    let mut out: Vec<char> = c.clone();
    let blank = |out: &mut Vec<char>, from: usize, to: usize| {
        for slot in &mut out[from..to.min(c.len())] {
            if *slot != '\n' {
                *slot = ' ';
            }
        }
    };
    let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
    // `r` starts a raw literal alone or after a `b` or `c` prefix, never inside a longer name.
    let raw_prefix = |c: &[char], i: usize| {
        i == 0 || !ident(c[i - 1]) || (matches!(c[i - 1], 'b' | 'c') && (i < 2 || !ident(c[i - 2])))
    };
    let mut i = 0;
    while i < c.len() {
        let next = c.get(i + 1).copied();
        match c[i] {
            '/' if next == Some('/') => {
                let mut end = i;
                while end < c.len() && c[end] != '\n' {
                    end += 1;
                }
                let line = line_starts.partition_point(|start| *start <= i);
                comments.insert(line, c[i..end].iter().collect());
                blank(&mut out, i, end);
                i = end;
            }
            '/' if next == Some('*') => {
                let mut depth = 1;
                let mut end = i + 2;
                while end < c.len() && depth > 0 {
                    if c[end] == '/' && c.get(end + 1) == Some(&'*') {
                        depth += 1;
                        end += 2;
                    } else if c[end] == '*' && c.get(end + 1) == Some(&'/') {
                        depth -= 1;
                        end += 2;
                    } else {
                        end += 1;
                    }
                }
                blank(&mut out, i, end);
                i = end;
            }
            'r' if raw_prefix(&c, i) && (next == Some('"') || next == Some('#')) => {
                let mut hashes = 0;
                while c.get(i + 1 + hashes) == Some(&'#') {
                    hashes += 1;
                }
                if c.get(i + 1 + hashes) != Some(&'"') {
                    // A raw identifier (`r#type`), not a string.
                    i += 1;
                    continue;
                }
                let start = i + 2 + hashes;
                let mut end = start;
                while end < c.len() {
                    if c[end] == '"' && (0..hashes).all(|h| c.get(end + 1 + h) == Some(&'#')) {
                        break;
                    }
                    end += 1;
                }
                blank(&mut out, start, end);
                i = end + 1 + hashes;
            }
            '"' => {
                let mut end = i + 1;
                while end < c.len() && c[end] != '"' {
                    end += if c[end] == '\\' { 2 } else { 1 };
                }
                blank(&mut out, i + 1, end);
                i = end + 1;
            }
            '\'' => {
                if next == Some('\\') {
                    // Past the escaped character, which may itself be a quote.
                    let mut end = i + 3;
                    while end < c.len() && c[end] != '\'' {
                        end += 1;
                    }
                    blank(&mut out, i + 1, end);
                    i = end + 1;
                } else if c.get(i + 2) == Some(&'\'') {
                    blank(&mut out, i + 1, i + 2);
                    i += 3;
                } else {
                    // A lifetime.
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    (out.into_iter().collect(), comments)
}

/// `blanked` without the items under `#[cfg(test)]` or `#[cfg(all(test, ..))]`, line numbers kept.
fn without_test_items(blanked: &str) -> String {
    let mut c: Vec<char> = blanked.chars().collect();
    let text: String = c.iter().collect();
    for marker in ["#[cfg(test)]", "#[cfg(all(test"] {
        for (byte_at, _) in text.match_indices(marker) {
            let mut at = text[..byte_at].chars().count();
            if c[at] != '#' {
                // Inside an item an earlier marker already removed.
                continue;
            }
            let mut depth = 0usize;
            while at < c.len() {
                match c[at] {
                    ';' if depth == 0 => {
                        c[at] = ' ';
                        break;
                    }
                    '{' => depth += 1,
                    '}' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            c[at] = ' ';
                            break;
                        }
                    }
                    _ => {}
                }
                if c[at] != '\n' {
                    c[at] = ' ';
                }
                at += 1;
            }
        }
    }
    c.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask lives in the workspace")
            .to_path_buf()
    }

    #[test]
    fn regia_rules_depends_on_no_ecs_crate() {
        assert_no_ecs_in_closure("regia_rules", false);
    }

    /// ADR-8877 (review sprint-v06-absorb-rev-b R3, ADR-8174's open question): the deterministic
    /// navigation is its own crate, so a headless lockstep sim links it without bevy_ecs, the ECS
    /// wrapper, the scene or the app. Only normal dependencies count (dev-dependencies never reach a
    /// sim build).
    #[test]
    fn regia_nav_det_depends_on_no_ecs_crate() {
        assert_no_ecs_in_closure("regia_nav_det", true);
    }

    /// ADR-8877: `regia_nav_det` is a closure crate (dependency and clippy.toml checks), and the old
    /// `[[module]]` row for `regia_nav`'s `det` directory is gone.
    #[test]
    fn det_scan_lists_regia_nav_det_as_a_closure_crate() {
        let json = scan(&workspace_root())
            .expect("scan the repository's closure")
            .to_json();
        assert!(
            json["crates"]
                .as_array()
                .expect("crate rows")
                .iter()
                .any(|path| path == "crates/regia_nav_det"),
            "{json}"
        );
        assert!(
            json["modules"]
                .as_array()
                .expect("module rows")
                .iter()
                .all(|path| !path
                    .as_str()
                    .expect("module path")
                    .starts_with("crates/regia_nav/src/det")),
            "the deterministic navigation must receive crate-level dependency and clippy checks: {json}"
        );
        assert_eq!(json["problems"], serde_json::json!([]), "{json}");
    }

    /// Walks the resolved dependency closure of workspace crate `krate` (every kind, or with
    /// `normal_only` the normal dependencies) and fails on any ECS, app, physics, platform or GPU crate.
    fn assert_no_ecs_in_closure(krate: &str, normal_only: bool) {
        let output = std::process::Command::new(env!("CARGO"))
            .args(["metadata", "--format-version", "1", "--locked", "--offline"])
            .current_dir(workspace_root())
            .output()
            .expect("read the resolved Cargo dependency graph");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).expect("Cargo JSON");
        let packages = metadata["packages"].as_array().expect("packages");
        let root = packages
            .iter()
            .find(|p| p["name"] == krate)
            .unwrap_or_else(|| panic!("{krate} must be a standalone workspace crate"));
        let nodes = metadata["resolve"]["nodes"].as_array().expect("resolved nodes");
        let mut pending = vec![root["id"].as_str().expect("package id")];
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            let package = packages.iter().find(|p| p["id"] == id).expect("resolved package");
            let name = package["name"].as_str().expect("package name");
            assert!(
                !name.starts_with("bevy_")
                    && !name.starts_with("rapier")
                    && !matches!(
                        name,
                        "regia_app"
                            | "regia_ecs"
                            | "regia_framework"
                            | "regia_physics"
                            | "regia_platform"
                            | "regia_render"
                            | "wgpu"
                            | "winit"
                    ),
                "{krate} dependency closure contains {name}"
            );
            let node = nodes.iter().find(|n| n["id"] == id).expect("resolved node");
            if normal_only {
                // `deps[].dep_kinds[].kind` is null for a normal dependency, "dev" or "build" otherwise.
                pending.extend(
                    node["deps"]
                        .as_array()
                        .expect("dependency rows")
                        .iter()
                        .filter(|dep| {
                            dep["dep_kinds"]
                                .as_array()
                                .expect("dependency kinds")
                                .iter()
                                .any(|kind| kind["kind"].is_null())
                        })
                        .map(|dep| dep["pkg"].as_str().expect("dependency id")),
                );
            } else {
                pending.extend(
                    node["dependencies"]
                        .as_array()
                        .expect("dependency ids")
                        .iter()
                        .map(|dependency| dependency.as_str().expect("dependency id")),
                );
            }
        }
    }

    #[test]
    fn det_scan_lists_regia_rules_as_a_closure_crate() {
        let json = scan(&workspace_root())
            .expect("scan the repository's closure")
            .to_json();
        assert!(
            json["crates"]
                .as_array()
                .expect("crate rows")
                .iter()
                .any(|path| path == "crates/regia_rules"),
            "{json}"
        );
        assert!(
            json["modules"]
                .as_array()
                .expect("module rows")
                .iter()
                .all(|path| {
                    let path = path.as_str().expect("module path");
                    !path.starts_with("crates/regia_framework/src/rules")
                        && !path.starts_with("crates/regia_rules")
                }),
            "rules must receive crate-level dependency and clippy checks: {json}"
        );
        assert_eq!(json["findings"], serde_json::json!([]), "{json}");
        assert_eq!(json["problems"], serde_json::json!([]), "{json}");
        assert_eq!(json["ok"], true, "{json}");
    }

    fn rules(findings: &[Finding]) -> Vec<&'static str> {
        findings.iter().map(|f| f.rule).collect()
    }

    /// The acceptance test of plan row F-01: a closure crate with a planted `powi` and a planted
    /// hash-map iteration fails the scan, and the scan names both lines.
    #[test]
    fn det_scan_flags_a_planted_powi_and_hash_iteration() {
        let dir = regia_test_utils::TempDir::new("det-scan-plant");
        let root: &Path = &dir;
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/*\"]\n").expect("write");
        std::fs::write(root.join("clippy.toml"), "disallowed-methods = []\n").expect("write");
        let krate = root.join("crates/sim");
        std::fs::create_dir_all(krate.join("src")).expect("mkdir");
        std::fs::write(krate.join("Cargo.toml"), "[package]\nname = \"sim\"\n").expect("write");
        let clippy: Vec<String> = clippy_paths().iter().map(|p| format!("\"{p}\",")).collect();
        std::fs::write(
            krate.join("clippy.toml"),
            format!(
                "disallowed-methods = [{}]\ndisallowed-types = {:?}\n",
                clippy.join(" "),
                HOST_TYPES
            ),
        )
        .expect("write");
        std::fs::write(
            krate.join("src/lib.rs"),
            "use std::collections::BTreeMap;\n\
             pub fn drag(v: f32) -> f32 {\n    v.powi(2) * 0.5\n}\n\
             pub fn total(m: &std::collections::HashMap<u32, f32>) -> f32 {\n    \
             let mut sum = 0.0;\n    for (_, v) in m.iter() {\n        sum += v;\n    }\n    sum\n}\n\
             pub fn ordered(m: &BTreeMap<u32, f32>) -> f32 {\n    m.values().sum()\n}\n",
        )
        .expect("write");
        let closure: Closure = toml::from_str(
            "math_module = \"crates/sim/src/math.rs\"\n[[crate]]\npath = \"crates/sim\"\nwhy = \"planted\"\n",
        )
        .expect("closure parses");

        let report = scan_closure(root, &closure).expect("scan");
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert!(!report.is_clean());
        let found: Vec<(usize, &str, &str)> = report
            .findings
            .iter()
            .map(|f| (f.line, f.rule, f.what.as_str()))
            .collect();
        assert!(found.contains(&(3, "std_float_fn", "powi")), "{found:?}");
        assert!(found.contains(&(5, "hash_iteration", "HashMap")), "{found:?}");
        assert_eq!(
            found.len(),
            2,
            "the BTreeMap sum and the multiplication are clean: {found:?}"
        );
        assert_eq!(report.files, 1);
        let json = report.to_json();
        assert_eq!(json["ok"], false);
        assert_eq!(json["findings"][0]["rule"], "std_float_fn");
    }

    /// ADR-8049: a `[[module]]` row scans one module of a non-lockstep crate with the line rules,
    /// without the crate-level dependency and clippy checks, and a stale module path is a problem.
    #[test]
    fn det_scan_module_rows_scan_only_the_module() {
        let dir = regia_test_utils::TempDir::new("det-scan-module");
        let root: &Path = &dir;
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/*\"]\n").expect("write");
        let krate = root.join("crates/mixed");
        std::fs::create_dir_all(krate.join("src/det")).expect("mkdir");
        // The crate as a whole uses std transcendentals and a hash map; only `det` is lockstep.
        std::fs::write(
            krate.join("Cargo.toml"),
            "[package]\nname = \"mixed\"\n[dependencies]\nother = { path = \"../other\" }\n",
        )
        .expect("write");
        std::fs::write(
            krate.join("src/lib.rs"),
            "pub mod det;\npub fn look(v: f32) -> f32 { v.sin() }\n",
        )
        .expect("write");
        std::fs::write(
            krate.join("src/det/mod.rs"),
            "pub fn clean(v: f32) -> f32 { v * v + 1.0 }\n",
        )
        .expect("write");
        std::fs::write(
            krate.join("src/det/grid.rs"),
            "pub fn bad(v: f32) -> f32 {\n    v.sqrt()\n}\n",
        )
        .expect("write");
        let closure: Closure = toml::from_str(
            "math_module = \"crates/k/src/math.rs\"\n\
             [[module]]\npath = \"crates/mixed/src/det\"\nwhy = \"planted\"\n\
             [[module]]\npath = \"crates/mixed/src/gone\"\nwhy = \"stale\"\n",
        )
        .expect("closure parses");
        let report = scan_closure(root, &closure).expect("scan");
        assert_eq!(report.files, 2, "only the module's two files");
        let found: Vec<(&str, usize, &str)> = report
            .findings
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.what.as_str()))
            .collect();
        assert_eq!(
            found,
            vec![("crates/mixed/src/det/grid.rs", 2, "sqrt")],
            "lib.rs `sin` is outside"
        );
        assert_eq!(
            report.problems,
            vec!["module `crates/mixed/src/gone`: not a directory or `.rs` file".to_string()],
            "no dependency or clippy.toml problem for a module row"
        );
        assert_eq!(report.to_json()["modules"][0], "crates/mixed/src/det");
    }

    #[test]
    fn det_scan_is_clean_on_the_lockstep_closure() {
        let report = scan(&workspace_root()).expect("scan the repository's closure");
        assert!(
            report.crates.iter().any(|c| c == "crates/regia_detsim"),
            "{:?}",
            report.crates
        );
        assert!(report.files >= 3, "{}", report.files);
        // ADR-8877: the deterministic navigation moved from the `crates/regia_nav/src/det` module row
        // to its own closure crate.
        assert!(
            report.crates.iter().any(|c| c == "crates/regia_nav_det"),
            "{:?}",
            report.crates
        );
        assert!(
            report.is_clean(),
            "det-scan findings:\n{:#?}\nproblems:\n{:#?}",
            report.findings,
            report.problems
        );
    }

    /// ADR-8892: a `#[cfg(test)] mod name;` line of a non-`mod.rs` file `foo.rs` declares
    /// `foo/name.rs` (or `foo/name/`): that file is left out too.
    #[test]
    fn a_test_only_child_of_a_non_mod_file_is_not_scanned() {
        // `replay.rs` declares `#[cfg(test)] mod tests;`, whose file is `replay/tests.rs`;
        // a planted wall clock there is a test helper, not lockstep code. `replay/audit.rs` (a live
        // child) is still scanned.
        let dir = regia_test_utils::TempDir::new("det-scan-child-tests");
        let root: &Path = &dir;
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/*\"]\n").expect("write");
        std::fs::write(root.join("clippy.toml"), "disallowed-methods = []\n").expect("write");
        let module = root.join("crates/net/src/lockstep");
        std::fs::create_dir_all(module.join("replay")).expect("mkdir");
        std::fs::write(module.join("mod.rs"), "pub mod replay;\n").expect("write");
        std::fs::write(
            module.join("replay.rs"),
            "mod audit;\n#[cfg(test)]\nmod tests;\npub fn n() -> u32 {\n    1\n}\n",
        )
        .expect("write");
        std::fs::write(
            module.join("replay/audit.rs"),
            "pub fn late(us: f32) -> f32 {\n    us.sqrt()\n}\n",
        )
        .expect("write");
        std::fs::write(
            module.join("replay/tests.rs"),
            "fn now() -> std::time::SystemTime {\n    std::time::SystemTime::now()\n}\n",
        )
        .expect("write");
        let closure: Closure = toml::from_str(
            "math_module = \"crates/sim/src/math.rs\"\n\
             [[module]]\npath = \"crates/net/src/lockstep\"\nwhy = \"planted\"\n",
        )
        .expect("closure parses");
        let report = scan_closure(root, &closure).expect("scan");
        let found: Vec<(&str, usize, &str)> = report
            .findings
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.what.as_str()))
            .collect();
        assert_eq!(
            found,
            [("crates/net/src/lockstep/replay/audit.rs", 2, "sqrt")],
            "the test-only child file of replay.rs is left out"
        );
        assert_eq!(report.files, 3, "mod.rs, replay.rs and replay/audit.rs");
    }

    /// ADR-8146: a `[[module]]` row puts a directory of a crate that is not lockstep code as a
    /// whole on the closure. Its files are scanned with the same rules, a file a
    /// `#[cfg(test)] mod name;` line declares is left out, and a missing path is a problem.
    #[test]
    fn a_listed_module_is_scanned_and_its_test_only_file_is_not() {
        let dir = regia_test_utils::TempDir::new("det-scan-module");
        let root: &Path = &dir;
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/*\"]\n").expect("write");
        std::fs::write(root.join("clippy.toml"), "disallowed-methods = []\n").expect("write");
        let module = root.join("crates/net/src/lockstep");
        std::fs::create_dir_all(&module).expect("mkdir");
        std::fs::write(
            module.join("mod.rs"),
            "mod host;\n#[cfg(test)]\nmod tests;\npub fn ticks(a: u64, b: u64) -> u64 {\n    a.max(b)\n}\n",
        )
        .expect("write");
        std::fs::write(
            module.join("host.rs"),
            "pub fn late(us: f32) -> f32 {\n    us.sqrt()\n}\n",
        )
        .expect("write");
        std::fs::write(
            module.join("tests.rs"),
            "fn helper(m: &std::collections::HashMap<u32, f32>) -> f32 {\n    m.values().sum::<f32>().sqrt()\n}\n",
        )
        .expect("write");
        // A file elsewhere in the crate is not on the closure.
        std::fs::write(
            root.join("crates/net/src/other.rs"),
            "pub fn f(x: f32) -> f32 { x.sin() }\n",
        )
        .expect("write");
        let closure: Closure = toml::from_str(
            "math_module = \"crates/sim/src/math.rs\"\n\
             [[module]]\npath = \"crates/net/src/lockstep\"\nwhy = \"planted\"\n\
             [[module]]\npath = \"crates/net/src/missing.rs\"\nwhy = \"gone\"\n",
        )
        .expect("closure parses");
        let report = scan_closure(root, &closure).expect("scan");
        let found: Vec<(&str, usize, &str)> = report
            .findings
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.what.as_str()))
            .collect();
        assert_eq!(
            found,
            [("crates/net/src/lockstep/host.rs", 2, "sqrt")],
            "the integer max, the test-only file and the file outside the module are not findings"
        );
        assert_eq!(report.files, 2, "mod.rs and host.rs");
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(report.problems[0].contains("missing.rs"), "{:?}", report.problems);
    }

    /// ADR-8146: the engine's lockstep core and the soak kit's sample simulation are on the closure.
    #[test]
    fn the_lockstep_core_is_on_the_closure() {
        let report = scan(&workspace_root()).expect("scan the repository's closure");
        for path in [
            "crates/regia_net/src/lockstep",
            "crates/regia_net_soak/src/lockstep.rs",
        ] {
            assert!(
                report.modules.iter().any(|c| c == path),
                "{path} is not listed: {:?}",
                report.modules
            );
        }
        assert!(report.files >= 12, "{}", report.files);
        assert!(
            report.is_clean(),
            "{:#?}\n{:#?}",
            report.findings,
            report.problems
        );
    }

    /// DO27's source-scan test, ported (adapt): every forbidden method in every call form is caught;
    /// module functions, `libm` calls, declarations and test modules are not. Two of DO27's
    /// assertions changed with the named issues in the header: `powi` is now forbidden (ADR-8012 A-4),
    /// and a trailing comment no longer hides the live call before it.
    #[test]
    fn non_test_sources_use_math_module_for_transcendentals() {
        for method in FORBIDDEN_METHODS {
            // An order method's method form needs a float hint on the line (ORDER_METHODS).
            let hint = if ORDER_METHODS.contains(method) { "0.5" } else { "" };
            for sample in [
                format!("x.{method}({hint}"),
                format!("f32::{method}("),
                format!("f64::{method}("),
                format!("std::f32::{method}("),
                format!("core::f64::{method}("),
                format!("<f32 as Float>::{method}("),
            ] {
                assert!(forbidden_method(&sample).is_some(), "scanner missed {sample}");
            }
        }
        for added in ["hypot", "tanh", "cbrt", "mul_add", "powi"] {
            assert_eq!(forbidden_method(&format!("a.{added}(b)")), Some(added));
            assert_eq!(forbidden_method(&format!("f64::{added}(a, b)")), Some(added));
        }
        assert!(forbidden_method("math::sin(yaw)").is_none());
        assert!(forbidden_method("crate::math::sqrt(x * x)").is_none());
        assert!(forbidden_method("regia_detsim::math::atan2(y, x)").is_none());
        assert!(forbidden_method("regia_core::math::detmath::det_sin(a)").is_none());
        assert!(forbidden_method("libm::powf(x, y)").is_none());
        assert!(forbidden_method("pub fn sin(x: f32) -> f32 {").is_none());
        assert!(
            forbidden_method("let n = 10_i64.pow(3);").is_none(),
            "integer pow is exact"
        );
        assert_eq!(
            forbidden_method("(dx).powi(2)"),
            Some("powi"),
            "adapted: powi is banned (A-4)"
        );
        assert_eq!(forbidden_method("x.tanh()"), Some("tanh"), "not read as `tan`");

        // Adapted: DO27 skipped a line with any `//`; now only the comment is blanked.
        let lines = live_lines("x.cos(); // stay on std\nlet y = x.tan();\n");
        assert_eq!(lines.len(), 2);
        assert!(forbidden_method(&lines[0].1).is_some());
        assert!(!lines[0].1.contains("stay"));

        // `mod name;` is the whole item; a brace body ends at its closing brace.
        let after_decl = live_lines("fn a() {}\n#[cfg(test)]\nmod rate_gate;\nlet y = x.tan();\n");
        assert!(after_decl.iter().any(|(_, line)| line.contains("x.tan()")));
        assert!(after_decl.iter().all(|(_, line)| !line.contains("rate_gate")));
        let stopped = live_lines("let y = x.tan();\n#[cfg(test)]\nmod tests {\n    x.sin();\n}\n");
        assert!(stopped.iter().any(|(_, line)| line.contains("x.tan()")));
        assert!(stopped.iter().all(|(_, line)| !line.contains("x.sin()")));
        // New: code after a test module is scanned, and braces in strings do not end it early.
        let after_tests = live_lines(
            "#[cfg(test)]\nmod tests {\n    fn a() { let s = \"}\"; x.sin(); }\n}\nfn b() { x.cos(); }\n",
        );
        assert_eq!(after_tests.len(), 1, "{after_tests:?}");
        assert_eq!(after_tests[0].0, 5);
        assert!(after_tests[0].1.contains("x.cos()"));
        let nested = live_lines(
            "#[cfg(test)]\nmod tests {\n    #[cfg(test)]\n    fn a() {}\n}\nfn b() { x.cos(); }\n",
        );
        assert_eq!(
            nested.len(),
            1,
            "a marker inside a removed item removes nothing more: {nested:?}"
        );

        // The repository's own lockstep kit passes, with no pinned direct calls.
        let src = workspace_root().join("crates/regia_detsim/src");
        let mut files = Vec::new();
        collect_rs_files(&src, &mut files);
        files.sort();
        assert!(files.len() >= 3);
        let mut violations = Vec::new();
        for path in &files {
            let text =
                std::fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
            let math = path.ends_with("math.rs");
            violations.extend(scan_source(&path.display().to_string(), &text, math));
        }
        assert!(
            violations.is_empty(),
            "direct std transcendental outside regia_detsim::math:\n{violations:#?}"
        );
    }

    #[test]
    fn every_rule_has_a_planted_line_and_comments_and_strings_do_not_count() {
        let src = "\
let a = libm::sinf(x);
let b = unsafe { core::intrinsics::fmul_fast(x, y) };
let c = x.algebraic_add(y);
let d: f64 = text.parse()?;
let e = s.parse::<f64>()? as f32;
let f: HashSet<u32> = HashSet::new();
let g = items.par_iter().map(step).collect();
let h = Vec3A::new(1.0, 2.0, 3.0);
let i = unsafe { core::arch::x86_64::_mm_add_ps(p, q) };
let j: glam::Quat = q;
let k = a.mul_add(b, c);
let l = speeds.iter().copied().fold(0.0, f32::max);
let m = glam::Mat3::from_rotation_y(yaw);
let n = (dx * 0.5).clamp(-1.0, 1.0);
let o = Instant::now();
let p = RandomState::new();
let q = env::args();
let r = values.as_ptr() as usize;
thread::spawn(step);
let enabled = cfg!(target_arch = \"x86_64\");
values.sort_by(|a,b| a.partial_cmp(b).unwrap());
let s = \"x.sin() HashMap Vec3A\"; // x.cos() rayon
/* x.tan() */ let ok = math::sin(t) + det_sin(t) + v.x * v.y;
let ok2: DVec4 = d; let ok3: BTreeMap<u32, f32> = m; let ok4 = Vec3::new(0.0, 1.0, 0.0);
let ok5 = det_max(a, b) + det_clamp(x, lo, hi); let ok6 = seats.min(max_seats).max(1);
let ok7 = f32::MAX; let ok8 = Mat3::from_cols(a, b, c); let ok9 = ticks.iter().max();
";
        let found: Vec<(usize, &str)> = scan_source("f.rs", src, false)
            .iter()
            .map(|f| (f.line, f.rule))
            .collect();
        let expect = [
            (1, "direct_libm"),
            (2, "fast_math"),
            (3, "fast_math"),
            (4, "f64_parse"),
            (5, "f64_parse"),
            (6, "hash_iteration"),
            (7, "rayon"),
            (8, "simd_type"),
            (9, "simd_type"),
            (10, "simd_type"),
            (11, "std_float_fn"),
            (12, "std_float_fn"),
            (13, "glam_trig"),
            (14, "std_float_fn"),
            (15, "wall_clock"),
            (16, "os_random"),
            (17, "env_read"),
            (18, "address_order"),
            (19, "thread_spawn"),
            (20, "arch_cfg"),
            (21, "float_sort"),
        ];
        assert_eq!(found, expect);
        assert!(
            scan_source("math.rs", "let a = libm::sinf(x);\n", true).is_empty(),
            "the math module may call libm"
        );
    }

    /// Review row F1: std `min`/`max`/`clamp` on floats give a zero sign (and NaN sign) that differs
    /// between x86-64 opt levels and aarch64. The text scan flags the path forms always and the method
    /// form when the line carries a float hint (an `f32`/`f64` token or a float literal); integer
    /// `min`/`max`/`clamp` and `Iterator::max` stay clean. Clippy, which sees types, denies the rest.
    #[test]
    fn float_min_max_clamp_are_flagged_and_integer_ones_are_not() {
        for (line, method) in [
            ("let m = x.max(0.0);", "max"),
            ("let c = speed.clamp(-1.0_f32, limit);", "clamp"),
            ("let lo = (t as f32).min(limit);", "min"),
            ("let n = f32::min(a, b);", "min"),
            ("let n = f64::max(a, b);", "max"),
            ("let w = <f32 as Float>::max(a, b);", "max"),
            ("let q = Float::clamp(a, lo, hi);", "clamp"),
            ("let e = 1e-3f32.max(eps);", "max"),
            ("let h = height.max(2.5e3);", "max"),
        ] {
            assert_eq!(forbidden_method(line), Some(method), "{line}");
        }
        for line in [
            "let n = count.max(1);",
            "let i = idx.min(len - 1);",
            "let best = ticks.iter().max();",
            "let worst = xs.iter().copied().min();",
            "let c = std::cmp::max(a, b);",
            "let z = a.max_by(b, cmp);",
            "let s = seats.clamp(2, 40);",
            "let t = <u32 as Ord>::max(a, b);",
            "let v = pair.0.max(pair.1);",
            "let r = regia_detsim::float::det_max(a, b);",
        ] {
            assert_eq!(forbidden_method(line), None, "{line}");
        }
        // The clippy half: the f32/f64 paths are on the required list.
        let paths = clippy_paths();
        for p in [
            "f32::max",
            "f32::min",
            "f32::clamp",
            "f64::max",
            "f64::min",
            "f64::clamp",
        ] {
            assert!(paths.iter().any(|q| q == p), "{p}");
        }
    }

    /// Review row F3: a std float function used as a value (`.map(f32::sin)`, `fold(0.0, f32::max)`)
    /// is the same platform call as a direct call.
    #[test]
    fn a_std_float_fn_used_as_a_value_is_flagged() {
        for (line, method) in [
            (
                "let ys: Vec<f32> = xs.iter().copied().map(f32::sin).collect();",
                "sin",
            ),
            ("let f = f64::cbrt;", "cbrt"),
            ("let top = xs.iter().copied().fold(f32::MIN, f32::max);", "max"),
            ("let g = Float::sqrt;", "sqrt"),
            ("let h = <f32 as Float>::tanh;", "tanh"),
            ("apply(f32::sin_cos, x)", "sin_cos"),
            ("let k = f32::exp", "exp"),
        ] {
            assert_eq!(forbidden_method(line), Some(method), "{line}");
        }
        for line in [
            "let m = f32::MAX;",
            "let p = core::f32::consts::PI;",
            "let e = f32::EPSILON;",
            "let n = f32::from_bits(b);",
            "let s = x.sin;",
        ] {
            assert_eq!(forbidden_method(line), None, "{line}");
        }
    }

    /// Review row F2: glam's scalar types are allowed, but these of their constructors and methods
    /// call std `sin_cos`, `atan2`, `tan`, `exp`, ... inside glam (the workspace glam uses std math,
    /// not libm), so a lockstep crate calling them reaches the platform libm.
    #[test]
    fn glam_scalar_methods_that_call_platform_math_are_flagged() {
        for (line, what) in [
            ("let r = Mat3::from_rotation_y(yaw);", "Mat3::from_rotation_y"),
            (
                "let r = glam::Mat3::from_axis_angle(axis, a);",
                "Mat3::from_axis_angle",
            ),
            (
                "let r = Mat3::from_euler(EulerRot::YXZ, y, p, 0.0);",
                "Mat3::from_euler",
            ),
            ("let d = glam::Vec2::from_angle(heading);", "Vec2::from_angle"),
            (
                "let dirs: Vec<Vec2> = hs.iter().copied().map(Vec2::from_angle).collect();",
                "Vec2::from_angle",
            ),
            ("let q = DQuat::from_rotation_y(a);", "DQuat::from_rotation_y"),
            (
                "let t = DAffine3::from_rotation_z(a);",
                "DAffine3::from_rotation_z",
            ),
            ("let a = Affine3::from_rotation_x(a);", "Affine3::from_rotation_x"),
            ("let p = a.slerp(b, 0.5);", "slerp"),
            ("let p = v.rotate_towards(w, step);", "rotate_towards"),
            ("let h = dir.to_angle();", "to_angle"),
            ("let e = basis.to_euler(EulerRot::YXZ);", "to_euler"),
            ("let v = v.rotate_y(yaw);", "rotate_y"),
            ("let v = v.rotate_axis(up, yaw);", "rotate_axis"),
        ] {
            assert_eq!(glam_trig(line).as_deref(), Some(what), "{line}");
        }
        for line in [
            "let m = Mat3::from_cols(a, b, c);",
            "let v = Vec3::new(1.0, 0.0, 0.0).normalize();",
            "let l = v.length();",
            "let r = regia_detsim::math::sin(yaw);",
            "let r = my_rotate_y(v, yaw);",
            "let r = Mat3::from_rotation_y_table(i);",
        ] {
            assert_eq!(glam_trig(line), None, "{line}");
        }
        let paths = clippy_paths();
        for p in [
            "glam::Mat3::from_rotation_y",
            "glam::Vec2::from_angle",
            "glam::Vec3::slerp",
            "glam::DQuat::from_rotation_y",
        ] {
            assert!(paths.iter().any(|q| q == p), "{p}");
        }
    }

    #[test]
    fn an_allow_comment_needs_the_rule_and_a_reason() {
        let waived = "// det-scan: allow(hash_iteration): lookup by id only, never iterated\nlet m: HashMap<u32, u8>;\n";
        assert!(scan_source("f.rs", waived, false).is_empty());
        let same_line = "let m: HashMap<u32, u8>; // det-scan: allow(hash_iteration, rayon): lookup only\n";
        assert!(scan_source("f.rs", same_line, false).is_empty());
        let other_rule = "// det-scan: allow(rayon): not this one\nlet m: HashMap<u32, u8>;\n";
        assert_eq!(rules(&scan_source("f.rs", other_rule, false)), ["hash_iteration"]);
        let no_reason = "let m: HashMap<u32, u8>; // det-scan: allow(hash_iteration)\n";
        assert_eq!(
            rules(&scan_source("f.rs", no_reason, false)),
            ["allow_without_reason", "hash_iteration"]
        );
    }

    #[test]
    fn pins_match_once_and_unlisted_workspace_dependencies_fail() {
        let dir = regia_test_utils::TempDir::new("det-scan-pins");
        let root: &Path = &dir;
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace.dependencies]\nhelper = { path = \"crates/helper\" }\nrun_lib = { path = \"crates/run_lib\" }\n",
        )
        .expect("write");
        std::fs::write(root.join("clippy.toml"), "disallowed-methods = [\"a::b\"]\n").expect("write");
        let krate = root.join("crates/sim");
        std::fs::create_dir_all(krate.join("src")).expect("mkdir");
        std::fs::write(
            krate.join("Cargo.toml"),
            "[package]\nname = \"sim\"\n[dependencies]\nhelper = { workspace = true }\nrun_lib = { workspace = true }\nlibm = \"=0.2.16\"\n",
        )
        .expect("write");
        // The clippy config lacks the root's `a::b` and `f64::mul_add`.
        let partial: Vec<String> = clippy_paths()
            .iter()
            .filter(|p| *p != "f64::mul_add")
            .map(|p| format!("\"{p}\","))
            .collect();
        std::fs::write(
            krate.join("clippy.toml"),
            format!(
                "disallowed-methods = [{}]\ndisallowed-types = {:?}\n",
                partial.join(" "),
                HOST_TYPES
            ),
        )
        .expect("write");
        std::fs::write(
            krate.join("src/lib.rs"),
            "pub fn s(v: f32) -> f32 {\n    v.sqrt()\n}\n",
        )
        .expect("write");
        let closure: Closure = toml::from_str(
            "math_module = \"crates/sim/src/math.rs\"\n\
             [[crate]]\npath = \"crates/sim\"\nwhy = \"test\"\n\
             [[exempt]]\nname = \"helper\"\nwhy = \"cook-side only\"\n\
             [[pin]]\npath = \"crates/sim/src/lib.rs\"\ntext = \"v.sqrt()\"\nrule = \"std_float_fn\"\nwhy = \"as-is copy\"\n\
             [[pin]]\npath = \"crates/sim/src/lib.rs\"\ntext = \"v.cos()\"\nrule = \"std_float_fn\"\nwhy = \"stale\"\n",
        )
        .expect("closure parses");
        let report = scan_closure(root, &closure).expect("scan");
        assert!(
            report.findings.is_empty(),
            "the pinned sqrt is not a finding: {:?}",
            report.findings
        );
        let problems = report.problems.join("\n");
        assert!(problems.contains("`run_lib`"), "{problems}");
        assert!(!problems.contains("`helper`"), "{problems}");
        assert!(
            !problems.contains("`libm`"),
            "an external crate is not a workspace crate: {problems}"
        );
        assert!(
            problems.contains("f64::mul_add") && problems.contains("a::b"),
            "{problems}"
        );
        assert!(
            problems.contains("`v.cos()`") && problems.contains("matched 0"),
            "{problems}"
        );
        assert_eq!(report.problems.len(), 3, "{problems}");
    }

    /// A `[[module]]` row scans one directory of a crate outside the closure with the same rules,
    /// and nothing else of that crate; a row whose path holds no source is a closure problem
    /// (ADR-8102).
    #[test]
    fn det_scan_scans_a_listed_module_of_a_crate_outside_the_closure() {
        let dir = regia_test_utils::TempDir::new("det-scan-module");
        let root: &Path = &dir;
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/*\"]\n").expect("write");
        let krate = root.join("crates/game");
        std::fs::create_dir_all(krate.join("src/rules")).expect("mkdir");
        std::fs::write(krate.join("Cargo.toml"), "[package]\nname = \"game\"\n").expect("write");
        // Outside the module: not scanned, and the crate needs no lockstep clippy.toml.
        std::fs::write(
            krate.join("src/lib.rs"),
            "pub fn wave(t: f32) -> f32 {\n    t.sin()\n}\n",
        )
        .expect("write");
        std::fs::write(
            krate.join("src/rules/hill.rs"),
            "pub fn d2(a: f32, b: f32) -> f32 {\n    a * a + b * b\n}\npub fn d(a: f32) -> f32 {\n    a.hypot(1.0)\n}\n",
        )
        .expect("write");
        let closure: Closure = toml::from_str(
            "math_module = \"crates/sim/src/math.rs\"\n\
             [[module]]\npath = \"crates/game/src/rules\"\nwhy = \"stepped on the lockstep tick\"\n\
             [[module]]\npath = \"crates/game/src/gone\"\nwhy = \"moved away\"\n",
        )
        .expect("closure parses");
        let report = scan_closure(root, &closure).expect("scan");
        let found: Vec<(&str, usize, &str)> = report
            .findings
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.what.as_str()))
            .collect();
        assert_eq!(found, [("crates/game/src/rules/hill.rs", 5, "hypot")]);
        assert_eq!(report.files, 1, "lib.rs outside the module is not scanned");
        assert_eq!(report.modules, ["crates/game/src/rules", "crates/game/src/gone"]);
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(
            report.problems[0].contains("crates/game/src/gone"),
            "{:?}",
            report.problems
        );
        assert_eq!(report.to_json()["modules"][0], "crates/game/src/rules");
    }

    /// The lockstep `clippy.toml` of the kit carries every path the scan requires, so a crate that
    /// copies it passes the clippy half of the check.
    #[test]
    fn lockstep_clippy_toml_bans_every_scanned_method() {
        let root = workspace_root();
        let have =
            clippy_methods(&root.join("crates/regia_detsim/clippy.toml")).expect("the kit's clippy.toml");
        for path in clippy_paths() {
            assert!(
                have.contains(&path),
                "crates/regia_detsim/clippy.toml lacks {path}"
            );
        }
        let types = clippy_entries(&root.join("crates/regia_detsim/clippy.toml"), "disallowed-types")
            .expect("type policy");
        for path in HOST_TYPES {
            assert!(types.contains(*path), "clippy type policy lacks {path}");
        }
        for path in clippy_methods(&root.join("clippy.toml")).unwrap_or_default() {
            assert!(
                have.contains(&path),
                "crates/regia_detsim/clippy.toml lacks the root entry {path}"
            );
        }
    }
}
