# Scan a game's lockstep crates

Run Regia's determinism scanner against a game's Cargo workspace without copying its source into
Regia. From the Regia checkout:

```text
cargo xtask det-scan --external /path/to/game --crate game-sim --crate game-physics --json
```

Use Cargo **package names**, not folder names. Repeat `--crate` for every tick crate. Duplicate names
are scanned once. Without `--external`, the existing Regia closure scan is unchanged.
You can also run the built `xtask` executable directly; the external scan does not need a Regia
checkout at runtime. A relative root is relative to the process working directory.

The scan reads manifests itself; it does not run Cargo metadata, resolve/download dependencies,
execute build scripts, compile the game, or change files. It discovers the root package, declared
workspace members (including globs and excludes), and in-root path dependencies. A standalone
package without `[workspace]` is also supported. It scans `.rs`
files under each selected package's `src/`, plus explicit `[lib].path` and `[[bin]].path` files.
An unknown package, missing source tree, unreadable file, or malformed manifest fails the scan.
Files and findings are ordered by root-relative path and line, independent of argument order.

## Choose the math boundary

If a game still has one audited wrapper around its pinned `libm`, name its root-relative file:

```text
cargo xtask det-scan --external /path/to/game --crate game-sim --math-module crates/game-sim/src/math.rs
```

Only `direct_libm` is exempt in that file. All other rules still apply. Without this option there
is no direct-libm exemption (unless `--with-closure` supplies `math_module`). A game calling
`regia_detsim::math` needs no local exemption. An explicit math module must exist in the selected
source set; a typo cannot silently grant an exemption.

## Rules and why

This table is generated from `xtask::det_scan::RULE_DETAILS` by
`cargo xtask det-scan --list-rules`. The integration test compares it byte for byte with that output;
regenerate the marked block when the catalog changes. `--list-rules --json` returns the source
rule names and reasons for tools.

<!-- det-scan-rules:start -->
| Rule | Why |
|---|---|
| `std_float_fn` | Platform float math, fusion, and min/max/clamp zero or NaN signs can change bits; includes function values. Use the deterministic math and float wrappers. |
| `direct_libm` | Direct libm calls bypass the audited math boundary; only the selected math module is exempt. |
| `fast_math` | Fast intrinsics and algebraic methods permit reassociation or contraction, changing rounding. |
| `f64_parse` | Parsing f64 before narrowing to f32 can round twice; parse the simulation's precision directly. |
| `hash_iteration` | Hash container order can differ per process; use ordered containers or justify lookup-only use. |
| `rayon` | Parallel iteration can change simulation update and reduction order. |
| `simd_type` | SIMD types and intrinsics can combine lanes differently across architectures. |
| `glam_trig` | Scalar glam angle and rotation functions can call platform math internally; build from deterministic sine and cosine. |
| `wall_clock` | Instant/SystemTime reads, UNIX_EPOCH and chrono/time clocks depend on the host; use recorded tick counts. |
| `os_random` | OS-seeded RNGs and RandomState differ between peers; use an explicit recorded seed and deterministic streams. |
| `env_read` | Runtime environment and process arguments vary by host; pass agreed configuration into the simulation. Compile-time option_env! is allowed. |
| `address_order` | Pointer addresses vary by allocation and process; use stable entity IDs for comparisons and sort keys. Review and waive non-ordering casts. |
| `thread_spawn` | Thread/task scheduling can change update order; create workers outside the tick and merge results in a fixed order. |
| `arch_cfg` | Architecture and CPU-feature branches can select different arithmetic; only reviewed equivalence boundaries may waive them. |
| `float_sort` | partial_cmp sort comparators can panic on NaNs or give inconsistent ordering; use total_cmp with a defined NaN policy or an integer key. |
| `allow_without_reason` | A waiver needs a nonempty reason so its safety can be reviewed. |
<!-- det-scan-rules:end -->

The source rules are shared with the Regia scan, including float min/max/clamp, function values
such as `map(f32::sin)`, and scalar glam's platform-math calls. Comments, literal contents and
`#[cfg(test)]` items are blanked by the same scanner.

This is a source heuristic, not Rust type or dataflow analysis. Method-form min/max/clamp needs a
float hint on the same line; integer calls and argumentless iterator min/max are allowed. Macro
expansion, generated code, and modules/includes outside the scanned source set are not followed.
There is no separate NaN-dataflow rule: canonicalize NaNs and signed zeros before hashing or
sign-sensitive operations as described in [Lockstep math](lockstep-math.md). Pair scanning with
typed clippy checks and cross-platform simulation pins; a clean report alone is not a determinism
proof.

## Waive a reviewed site

A reasoned waiver on the same line or immediately above it works exactly as in Regia:

<!-- ignore-reason: demonstrates a scanner waiver; the game's map type and values are illustrative -->
```rust,ignore
// det-scan: allow(hash_iteration): lookup by stable entity id only; never iterated
let by_id: HashMap<u32, usize> = HashMap::new();
```

A waiver without a nonempty reason emits `allow_without_reason` and retains the original finding.
Use comma-separated rule names to waive multiple findings on one line.

## Optional closure and clippy policy

By default the external scan does **not** read a closure file or any `clippy.toml`. Add
`--with-closure` to read the game's own `docs/ci/lockstep-closure.toml`, using the
[same schema as Regia](lockstep-math.md#adding-a-crate-to-the-closure). It checks that selected crates
are listed, that their workspace dependencies are listed or exempted, and that each selected
crate's `clippy.toml` contains the lockstep disallowed methods and types plus the root config's methods.
Only selected crates' source is scanned; closure pins under their directories must match exactly
one live finding. The closure's math module is the default; `--math-module` overrides it.

The supplied root is resolved once; every external read stays inside that resolved directory.
Manifest member paths are root-relative;
explicit target paths are package-relative. These paths and `--math-module` reject `..` components.
Paths that escape the root, symlinked entries beneath it (even to another
file inside the root), and special files are refused before opening. Outside-root dependencies
are not opened or scanned. Keep the workspace stable during scanning; this is a source tool,
not a sandbox against another process concurrently replacing directories. Use a dedicated checkout
or snapshot if source generation is running.

## Automation contract

`--json` prints one object with `ok`, `closure`, `crates`, `files`, `findings`, and `problems`, using
the [existing scan shape](lockstep-math.md#for-agents---json). `closure` is `null` when policy checks
are off, otherwise `"docs/ci/lockstep-closure.toml"`. `crates` contains sorted root-relative
directories (`"."` for the root package). Finding fields are `file`, one-based `line`, `rule`,
`what`, and `text`. Source-rule codes are in the generated table above.

The machine-readable CLI is the agent surface for this offline tool; no running engine or
JSON-RPC endpoint is needed. Agents should check the exit status and `ok`, then attach findings to
source review. Exit status **0** means clean; **1** means findings, policy failures or external
input errors. External input errors are in `problems` with the lowercase code `external_input`
and contextual detail. Invalid CLI usage exits **2** with clap's diagnostic on stderr.

## Game adoption checks

Pin the v0.6 integration commit containing this command and replace the game's duplicate
source-scan invocation with the external command for all its simulation crates. Review findings;
keep the game-side clippy policy and numeric regression pins. This tool changes no simulation
math. For the DO27 handoff, its owner runs the steering golden, all 3000-tick world pins and the
fight pin after switching the invocation, and again after any math-call edits made to resolve
findings. The scan does not read or run that game's repository as part of Regia's acceptance tests.

## Host-dependence rules

The same seven [host-dependence checks](lockstep-math.md#host-dependent-operations-adr-8367)
run for a game: `wall_clock`, `os_random`, `env_read`, `address_order`, `thread_spawn`,
`arch_cfg`, and `float_sort`. Multiline calls and closures retain source line numbers. The
allow-with-reason form and `[[pin]]` policy apply to each rule; a math-module exemption still
exempts only `direct_libm`. Compile-time `option_env!` and `env!` are permitted.
