# Lockstep math and the determinism scan

A lockstep match sends only inputs; every peer runs the same simulation and must compute the same
bits. A Windows PC, a Steam Deck, a Mac and an Android phone can only share one match if nothing on
the tick depends on the host: not the platform's math library, not a hash map's iteration order, not
a SIMD lane order, and not a floating-point mode some library left on the thread. `regia_detsim` is
the kit for that code, and `cargo xtask det-scan` checks it (ADR-8012 A-4, ADR-8013).

## The kit: `regia_detsim`

| Module | What it gives the tick |
|---|---|
| `regia_detsim::math` | `sin`, `cos`, `sqrt`, `sqrt_f64`, `powf`, `exp` through the pure-Rust `libm` 0.2.16, and the canonical, correctly rounded `atan2` (below). The workspace pins `libm` exactly (`=0.2.16`); a bump is a sim-version change. `regia_detsim::MATH_BITS_VERSION` names the bits (now `1`). |
| `regia_detsim::tick` | `TICK_RATE_HZ` (60), `tick_dt()`, `ticks_from_secs()`, `ticks_from_secs_u32()`, and `TickRate` for a host that runs another rate (the same bits at 60 Hz). Every peer runs the same rate. |
| `regia_detsim::fpu` | `ensure_default_fp_env()`: put the thread's floating-point environment back to the IEEE default (no flush-to-zero, no denormals-are-zero, round to nearest, no traps) and report what was wrong. `read_fp_env()` only reads. `FpEnvWatch`: the periodic check a sim loop runs before and after each step (ADR-8840). |
| `regia_detsim::float` | `det_max`, `det_min`, `det_clamp` (and `_f64` forms): float `max`/`min`/`clamp` with one answer on every host and optimization level. `canonical(x)`: any NaN to `0x7fc0_0000`, `-0` to `+0`, every other value unchanged. |

The crate depends on no ECS, GPU or platform crate, so a sim library or a headless server links it
alone. A host must call the guard first thing on the sim thread, and again after foreign code (an
audio or video library, a plugin) ran on it:

<!-- ignore-reason: a sketch of a host's sim thread; its inputs (dir_x, speed, ...) come from the game -->
```rust,ignore
use regia_detsim::{fpu, math, tick};

fn sim_thread_main() {
    let report = fpu::ensure_default_fp_env();
    if !report.was_default() {
        tracing::warn!(?report, "sim thread had a non-default float environment; reset");
    }
    let dt = tick::tick_dt();
    let heading = math::atan2(dir_z, dir_x);
    let step = [math::cos(heading) * speed * dt, math::sin(heading) * speed * dt];
    // ...
}
```

`FpEnvReport` fields: `supported` (false on targets without a guard, e.g. wasm32), `flush_to_zero`,
`denormals_are_zero` (x86-64 only), `rounding_not_nearest`, `traps_enabled`, `other_control`
(aarch64: default NaN, alternative half precision, the FEAT_AFP bits) and `raw_before` (MXCSR or
FPCR as read).

### The periodic check: `FpEnvWatch`

The guard at thread start does not cover foreign code that runs on the sim thread later: an audio or
video call between two steps, or a plugin called from inside one, can set flush-to-zero, and from then
on that peer's subnormal results are `0.0` while the other peers' are not. The match drifts with no
error. A sim loop owns one `FpEnvWatch` and calls it at two defined points (ADR-8840):

<!-- ignore-reason: a sketch of a host's own sim loop; `sim` and `tick` come from the game -->
```rust,ignore
let mut fp = regia_detsim::fpu::FpEnvWatch::default(); // every step
loop {
    fp.before_step(tick); // undo what ran on this thread since the last step
    sim.step(tick);
    fp.after_step(tick); // a change made inside the step: a desync risk
    tick += 1;
}
```

Each check is one control-register read, plus a write only when the environment changed. A change
found by `before_step` came from code between two steps and was undone before the step ran; one found
by `after_step` (`during_step()` counts them) happened inside the step, so that step may already
differ from a peer's: report it. `FpEnvWatch::new(n)` checks one step in `n` (the first step always;
`0` never), `checks()`, `corrections()`, `first_change()` and `last_change()` (an `FpEnvChange` with
the tick, the `point` (`before_step` or `after_step`) and the `FpEnvReport`) are the counters a host
puts in its status. `SimHarness` runs one around every step (`fp_env()`, `set_fp_check_every`); the
threaded sim host has its own cadence setting (`SimHostSettings::fp_check_every`, ADR-8526), and the
lockstep host and seat check before every frame they hand out.

### Zero sign and NaN sign

IEEE 754 lets `max` and `min` return either zero when the operands are `+0` and `-0`, and leaves the
sign of a NaN the hardware makes to the CPU. Measured with inputs the optimizer cannot see (rustc
1.99, `-O0` and release, `tests/float_order_bits.rs`):

| Expression | x86-64 (Linux) | aarch64 (macOS) |
|---|---|---|
| `0.0f32.max(-0.0)` | `-0` | `+0` |
| `(-0.0f32).max(0.0)` | `+0` (a standalone `-O3` build with rustc 1.96 gave `-0`) | `+0` |
| `0.0f32.min(-0.0)` | `-0` | `-0` |
| `(-0.0f32).min(0.0)` | `+0` | `-0` |
| `0.0 / 0.0`, `sqrt(-1.0)` | `0xffc0_0000` | `0x7fc0_0000` |

x86-64 returns the second operand when the two compare equal, aarch64 returns `+0` for `max` and `-0`
for `min`, and the optimizer may reorder the operands.

`+ - * /` do not care, but a sign-sensitive step after them does: `atan2(a.max(b), -1.0)` is `-π`
on one peer and `+π` on another. So in lockstep code:

- Use `det_max`, `det_min` and `det_clamp` instead of `f32::max`, `f32::min` and `f32::clamp`. A
  zero result is always `+0`, a NaN result is always `0x7fc0_0000`, and a NaN operand of
  `det_max`/`det_min` is ignored, as in std. Any other result is exactly std's bits, so the swap
  moves nothing else. `det_clamp` never panics (std panics on `lo > hi` or a NaN bound): a debug
  peer and a release peer take the same path.
- Never branch on, hash or `copysign` a value's zero sign or NaN sign unless it went through
  `canonical` first. That includes `atan2`, `1.0 / x`, `signum`, `is_sign_negative` and `to_bits`.
  `math::sqrt` of a negative number still returns the host's NaN.

<!-- ignore-reason: a sketch of a sim step; `dir`, `speed` and `limit` come from the game -->
```rust,ignore
use regia_detsim::float::{canonical, det_clamp, det_max};
use regia_detsim::math;

let speed = det_clamp(speed, 0.0, limit);
let lean = math::atan2(det_max(dir.y, 0.0), dir.x); // never -π from a -0
let hashed = canonical(speed).to_bits();
```

`regia_core::math::detmath` (ADR-4773) is a second deterministic math set for cook-side and
presentation code. Its bits differ from `regia_detsim::math`, but both are the same on every host; do
not mix them inside one sim value.

## The rules for lockstep code

Inside a crate on the lockstep closure:

- Float math is `f32` (or `f64`) `+ - * /` in a fixed order, plus `regia_detsim::math`. No std
  float methods that call the platform libm or may be fused: `sin`, `cos`, `tan`, `asin`, `acos`,
  `atan`, `atan2`, `sin_cos`, `sinh`, `cosh`, `tanh`, `asinh`, `acosh`, `atanh`, `sqrt`, `cbrt`,
  `hypot`, `exp`, `exp2`, `exp_m1`, `ln`, `ln_1p`, `log`, `log2`, `log10`, `powf`, `powi`, `mul_add`.
  Write `x * x` for `x.powi(2)` and `a * b + c` for `a.mul_add(b, c)`. Passing one as a value
  (`.map(f32::sin)`) is the same call.
- No std float `min`, `max` or `clamp` (`fold(0.0, f32::max)` included): use `det_max`, `det_min`,
  `det_clamp` (above). Integer `min`/`max`/`clamp` are fine.
- No glam function that calls std trig inside glam. On the scalar types (`Vec2`, `Vec3`, `Mat3`,
  `Affine3` and the `D*` forms) that is: the angle constructors (`from_angle`, `from_rotation_x/y/z`,
  `from_axis_angle`, `from_euler`, `from_scaled_axis`, the `perspective_*` ones), the angle readers
  (`to_angle`, `to_euler`, `to_axis_angle`, `angle_to` on `Vec3`), the rotations (`rotate_angle`,
  `rotate_axis`, `rotate_x/y/z`, `rotate_towards`), `slerp`, and the per-component `sin`, `cos`,
  `exp`, `exp2`, `ln`, `log2`, `powf`, `mul_add`. Build the rotation from `regia_detsim::math`
  instead, for example `Mat3::from_cols(Vec3::new(c, 0.0, -s), Vec3::Y, Vec3::new(s, 0.0, c))` with
  `s = math::sin(yaw)` and `c = math::cos(yaw)`, or `Vec2::new(math::cos(a), math::sin(a))` for
  `Vec2::from_angle(a)`. The full list is `GLAM_TRIG` in `xtask/src/det_scan.rs`.
- No fast-math (`fadd_fast` and friends, `algebraic_*`).
- No text parsed as `f64` and narrowed to `f32` later: that rounds twice. Parse `f32` directly, or
  better, store integers (millimeters).
- No `HashMap`/`HashSet` iteration: the order differs per process. Use `BTreeMap`, `BTreeSet` or a
  sorted `Vec`. A map used only for lookups may stay, with an allow comment (below).
- No rayon or parallel iterators in step code.
- No SIMD vector types: glam's `Vec3A`, `Vec4`, `Quat`, `Mat2`, `Mat3A`, `Mat4`, `Affine2` and
  `Affine3A` use SSE2 or NEON and may combine lanes in another order per architecture. Use `Vec3`,
  `DVec*` or plain arrays. Do not turn on glam's `scalar-math` feature instead: features unify, so it
  would change the renderer too.
- Fold NaNs and zeros before hashing state for a desync check (`canonical`): x86-64 and aarch64
  produce different NaN payloads.

## `cargo xtask det-scan`

The scan reads `docs/ci/lockstep-closure.toml` and checks the `src/` of every listed crate (comments,
string contents and `#[cfg(test)]` items are skipped). Rules and what each flags:

The following table is generated by `cargo xtask det-scan --list-rules` from the source
catalog, with the same checked output as the external scanner guide.

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

It also fails when a closure crate depends on a workspace crate that is neither in the closure nor in
an `[[exempt]]` row, when a closure crate's `clippy.toml` lacks a lockstep path or a path of the
root `clippy.toml`, and when a `[[pin]]` does not match exactly one line.

```text
$ cargo xtask det-scan
crates/my_sim/src/drag.rs:12: std_float_fn `powi`: let d = v.powi(2) * k;
det-scan: 1 crate(s), 0 module(s), 3 file(s), 1 finding(s), 0 closure problem(s)
```

The exit code is 0 when clean, 1 otherwise. `cargo xtask preflight` runs it after `check-unwraps`.

### For agents: `--json`

`cargo xtask det-scan --json` prints one object (a tool's output, not a protocol request):

```json,ignore
{
  "ok": false,
  "closure": "docs/ci/lockstep-closure.toml",
  "crates": ["crates/regia_detsim", "crates/my_sim"],
  "files": 7,
  "findings": [
    { "file": "crates/my_sim/src/drag.rs", "line": 12, "rule": "std_float_fn", "what": "powi",
      "text": "let d = v.powi(2) * k;" }
  ],
  "problems": []
}
```

`rule` is one of `std_float_fn`, `direct_libm`, `fast_math`, `f64_parse`, `hash_iteration`, `rayon`,
`simd_type`, `glam_trig`, `wall_clock`, `os_random`, `env_read`, `address_order`,
`thread_spawn`, `arch_cfg`, `float_sort`, `allow_without_reason`; `what` names the method, type or path that matched;
`problems` lists closure problems as sentences.

### Waivers and pins

A line that is safe for a reason the scanner cannot see carries an allow comment on the line or the
line above, with the rule and a reason:

<!-- ignore-reason: shows the comment form the scanner reads, not a compilable item -->
```rust,ignore
// det-scan: allow(hash_iteration): lookup by entity id only, never iterated
let by_id: HashMap<u32, usize> = HashMap::new();
```

An as-is copy whose bytes must not change (so it cannot gain a comment) lists its known sites in the
closure file instead; each pin must match exactly one live line:

```toml
[[pin]]
path = "crates/my_sim/src/state.rs"
text = "(velocity[0] * velocity[0] + velocity[2] * velocity[2]).sqrt()"
rule = "std_float_fn"
why = "as-is copy: changing it moves the sim's bits (Stage B re-pins)"
```

### Adding a crate to the closure

1. Add a `[[crate]]` row (`path`, `why`) to `docs/ci/lockstep-closure.toml`.
2. Copy `crates/regia_detsim/clippy.toml` into the crate (clippy reads only the nearest file, so it
   repeats the root entries too). `cargo clippy` then denies the std float methods (including
   `min`, `max` and `clamp`), the glam trig functions and the hash-container iteration methods by
   type.
3. List or exempt every workspace crate it depends on (`[[exempt]]` with `name` and `why`).
4. Run `cargo xtask det-scan` until it is clean.

### Adding a module of a larger crate

Lockstep code can live inside a crate that is not lockstep code as a whole (`regia_net::lockstep` inside
`regia_net`, ADR-8146). A `[[module]]` row (`path`, `why`) names a directory or one `.rs` file; its files are scanned
with the same rules. A file that a `#[cfg(test)] mod name;` line of the directory declares is left out. The
dependency and `clippy.toml` checks are per crate and do not run for a module, so the scan is the module's only guard:
keep float code out of it or route it through `regia_detsim`.

## The canonical `atan2`

`math::atan2` is defined by the mathematics, not by a library build (ADR-8876, `MATH_BITS_VERSION` 1):

| Input | Result |
|---|---|
| finite or infinite, no zero, no NaN | the correctly rounded binary32 `atan2(y, x)`; infinities take the IEEE 754 values (`atan2(±inf, +inf) = ±π/4`, `atan2(±inf, -inf) = ±3π/4`, `atan2(±inf, x) = ±π/2`, `atan2(y, -inf) = ±π`) |
| `y` is `+0` or `-0` | `+0` when `x > 0` or `x` is a zero, `+π` when `x < 0` |
| `x` is `+0` or `-0`, `y` nonzero | `±π/2` by the sign of `y` |
| any NaN | `0x7fc0_0000` |
| a zero result (`atan2(-1, +inf)`, an underflowed ratio) | `+0` |

So no `-0`, no NaN payload and no library rounding choice reaches a hash, and any correctly rounded
`atan2f` plus those three steps reproduces it in another tool. The algorithm is a port of CORE-MATH's
`cr_atan2f` (MIT): a binary64 fast path, a rounding test and a double-double fallback. Inputs within
about 2^-24 of a rounding boundary take the slow path; everything else costs one binary64 division and
a small rational.

`math::atan2_libm` keeps the version-0 bits (the bare libm `atan2f`: signed zeros kept, the hardware's
NaN, up to one ulp off). Use it only where bytes must match a tool built on that libm, as the world
geometry Stage A compat path and the tunnel bake's portal yaw do. Never use it in new tick code.

Check a host with the vector files (compiled into the test binary, so a device or the Deck runs it on
its own):

```text
cargo test -p regia_detsim --test atan2_canon -- --nocapture
```

Each set prints one `regia-atan2-vectors {"set":...,"target":...,"rows":...,"hash":...,"expected":...}`
line; `hash` equals `expected` on every host. A host sim that moves onto `regia_detsim` from libm
`atan2f` sees new bits only on signed zeros, NaNs and the inputs where libm is not correctly rounded
(one ulp); it should carry `MATH_BITS_VERSION` in its lockstep handshake.

## Checking the bits on another machine

`crates/regia_detsim/tests/math_bits.rs` hashes the wrappers' results over 6161 inputs and compares
with the hashes recorded on x86-64 Linux. Run it on any host to confirm that host computes the same
bits:

```text
cargo test -p regia_detsim --test math_bits -- --nocapture
```

It prints one hash per function and the architecture; a mismatch names the function that moved.
`tests/float_order_bits.rs` asserts the bits of `det_max`, `det_min`, `det_clamp` and `canonical` on
the zero and NaN cases above; run it at `-O0` and in release on each host:

```text
CARGO_PROFILE_DEV_OPT_LEVEL=0 cargo test -p regia_detsim --test float_order_bits -- --nocapture
cargo test -p regia_detsim --release --test float_order_bits -- --nocapture
```

With `--nocapture` it also prints std's own answers on that host, for the record.

## Lockstep modules inside other crates

A crate whose `src/` is not all lockstep code can still put one directory on the tick: a `[[module]]` row in
`docs/ci/lockstep-closure.toml` names the directory (or one `.rs` file) and why it runs on the tick. The scan applies
the same rules to it and reports the scanned modules beside the crates (`modules` in `--json`); it does not check that
crate's dependencies or `clippy.toml`, so keep the module's imports to plain-data crates and review them by hand. A
row whose path holds no source is a closure problem, so a moved module must move its row (ADR-8102).
The match core now lives in `crates/regia_rules` and uses a `[[crate]]` row
(ADR-8239), so its dependency closure and clippy restrictions are checked too
([Match modes and match flow](match-rules.md)). Module-row support remains available for other uses.

## Scanning a game workspace

Use `cargo xtask det-scan --external /path/to/game --crate game-sim --json` to apply the same
source rules to a game. See [Scan a game's lockstep crates](external-det-scan.md) for repeated
crate selection, math-module waivers, optional closure checks and the generated rules catalog.

## Host-dependent operations (ADR-8367)

The seven host-dependence rules apply to both internal and external scans. Their token matcher
follows paths across whitespace and line breaks, and inspects balanced cfg predicates and sort
arguments. A sort with `partial_cmp` reports the `sort_by`/`sort_unstable_by` line, so a reasoned
allow belongs there or immediately above it. A visible pointer-to-`usize` conversion reports the
`as usize` line. Other findings report the matched path or token's first line.

`wall_clock` covers `Instant::now`, `SystemTime::now`, `UNIX_EPOCH`, `chrono::` and
`OffsetDateTime::now_utc`/`now_local`. `os_random` covers `RandomState`, `thread_rng`,
`rand::random`, `rand::rng`, `getrandom`, `OsRng` and `fastrand::`. `env_read` covers runtime
`env::var`/`var_os`, `vars`/`vars_os`, `args`/`args_os`, `current_dir` and `current_exe` reads.
Compile-time `option_env!` and `env!` are allowed; builds still need agreed settings.

`address_order` covers visible raw-pointer/`as_ptr()` casts to `usize`, `ptr::addr`, `.addr()`
and `Arc::as_ptr`. It flags extraction conservatively because a later comparison is not visible
to a lexical scan. Use stable IDs for ordering; waive a reviewed non-ordering use with a concrete
reason. `thread_spawn` covers `thread::spawn`, `thread::Builder` and `tokio::spawn`.
`arch_cfg` covers `cfg`, `cfg_attr` and `cfg!` predicates containing `target_arch` or
`target_feature`, plus `is_x86_feature_detected!` and `is_aarch64_feature_detected!`.
The floating-point guard's architecture-specific register access has three local reasoned waivers.

`float_sort` flags `partial_cmp` within `sort_by`/`sort_unstable_by` arguments, including multiline
closures. Use `total_cmp` with a defined NaN policy or an integer key. This check does not infer
float types: a non-float partial comparator needs a reasoned waiver. It also does not follow
comparators defined elsewhere. Clippy cannot express that context, so it does not ban all
`partial_cmp` calls.

Every new rule accepts the same `// det-scan: allow(<rule>): <reason>` form and exact-line pins
as the existing rules. A reasonless allow produces `allow_without_reason` plus the source finding.
The JSON shape and exit statuses are unchanged; the rule-name vocabulary grows by seven.
Clippy additionally resolves std clock/environment/spawn methods and `RandomState`/`Builder`
types through imports. Copy the updated config into every closure crate. A source allow does not
waive clippy; use a narrowly scoped reasoned `#[expect(...)]` only where justified.

A clean scan does not resolve aliases, expand macros, infer pointer provenance or prove
cross-platform determinism. Pair it with typed linting and numerical pins. This feature's agent
surface remains the offline xtask CLI; no runtime JSON-RPC method is added.

## More functions

`regia_detsim::math` also wraps the rest of the libm surface that `det-scan` bans in std form, with the
same pinned `libm` 0.2.16. Each is one libm call:

| f32 | libm | f64 | libm |
|---|---|---|---|
| `tan` | `tanf` | `tan_f64` | `tan` |
| `asin`, `acos`, `atan` | `asinf`, `acosf`, `atanf` | `asin_f64`, `acos_f64`, `atan_f64`, `atan2_f64` | `asin`, `acos`, `atan`, `atan2` |
| `ln`, `log2`, `log10` | `logf`, `log2f`, `log10f` | `ln_f64`, `log2_f64`, `log10_f64` | `log`, `log2`, `log10` |
| `exp2` | `exp2f` | `exp_f64`, `exp2_f64`, `powf_f64` | `exp`, `exp2`, `pow` |
| `hypot`, `cbrt` | `hypotf`, `cbrtf` | `hypot_f64`, `cbrt_f64` | `hypot`, `cbrt` |
| `sinh`, `cosh`, `tanh` | `sinhf`, `coshf`, `tanhf` | `sinh_f64`, `cosh_f64`, `tanh_f64` | `sinh`, `cosh`, `tanh` |
| `sin_cos` | `sinf` then `cosf` | `sin_f64`, `cos_f64`, `sin_cos_f64` | `sin`, `cos` |

`sin_cos` is deliberately two calls, not libm's fused `sincosf`, so it returns exactly the bits of
`sin` and `cos` and code that mixes them never sees two answers for one angle. The seven original
functions did not change; `math_bits_match_the_pinned_hashes_on_every_arch` still pins them. The new
ones are checked bit for bit against libm over a seeded sweep of a million inputs per function
(`new_math_wrappers_match_libm_bits`) and across targets by the `math_rng_fixed_vectors` golden
(below). There is no wrapper for `asinh`, `acosh`, `atanh`, `exp_m1`, `ln_1p`, `mul_add` or `powi`:
write `x * x` for `powi(2)`, and ask for a wrapper if a sim needs another.

## Random numbers

`regia_detsim::rng::DetRng` is the generator for lockstep code. It is xoshiro256** (Blackman and
Vigna, 2021) with its state filled by SplitMix64 from a `u64` seed. It is integer only, so every target
draws the same numbers.

| Call | Gives |
|---|---|
| `DetRng::seed_from(seed)` | a generator whose *key* is `seed` |
| `rng.stream(system_id)` | an independent generator for one system (loot, AI, weather). It depends on the key and the id only, never on how many numbers were drawn, so adding a draw in one system never moves another's numbers. Streams of streams work the same way |
| `next_u64()`, `next_u32()` | raw bits (`next_u32` is the high half of `next_u64`) |
| `range_u32(n)`, `range_u64(n)` | uniform in `[0, n)`, exactly unbiased (Lemire's method); `n == 0` means the full range |
| `range_i32(lo, hi)` | uniform in `[lo, hi)`; `lo` when the range is empty |
| `unit_f32()`, `unit_f64()` | `[0, 1)` from 24 (53) bits: exact multiples of `2^-24` (`2^-53`) |
| `shuffle(&mut slice)` | Fisher-Yates |
| `key()`, `state()`, `from_state(key, s)` | for a state hash or a save; `from_state` refuses the all-zero state |

Never use `next_u32() % n`: it favors small values. `DetRng` is `Clone` but not `Copy` (a silent copy
would replay numbers) and serializes with serde as `{ "key": .., "s": [..] }`. Deserializing an all-zero
state fails with `det_rng_zero_state`. Hash `key()` and `state()` into your sim's desync hash like any
other state. `DetRng` is also a `DetState` (tag `detrng`, version 1: the key, then the four state
words), so a snapshot nests it with `w.state(&rng)` / `r.state()?`. Save the key with the words:
`stream(id)` derives from the key, so a generator rebuilt from the four words alone draws the same
`next_u64` but hands out different streams.

<!-- ignore-reason: a sketch of a host's systems; Loot and Monster come from the game -->
```rust,ignore
use regia_detsim::rng::DetRng;

const LOOT: u64 = 1;
const AI: u64 = 2;

let match_rng = DetRng::seed_from(match_seed); // from the lobby, the same on every peer
let mut loot = match_rng.stream(LOOT);
let mut ai = match_rng.stream(AI);
let drop = loot.range_u32(100) < 15; // 15 % drop chance, unbiased
let wander = ai.unit_f32() * core::f32::consts::TAU;
```

## Integer millimeters

`regia_detsim::fixed` holds integer geometry. Integer arithmetic is the same on every target by
construction, so it is the first choice for state that must never drift: grid positions, portal edges,
avoidance math and edits.

- `Mm(i32)`: millimeters (±2147 km). `Mm2(i64)`: square millimeters, or any product of two `Mm`.
- No `+` or `-` operators. Rust's integer operators panic on overflow in a debug build and wrap in a
  release build, so a debug peer and a release peer would part at the first overflow. Pick `checked_*`
  (`Option`), `saturating_*` or `wrapping_*`; each gives the same answer in every build. `mul_wide`
  multiplies two `Mm` into an exact `Mm2`.
- `Mm::from_m_f32(m)` rounds the exact value of `m * 1000` half away from zero, with integer arithmetic
  on the float's bits (no float operation). It returns `None` for NaN, the infinities and values
  outside `i32`; `from_m_f32_saturating` clamps instead (NaN gives 0). It rounds the float you have,
  not the decimal you typed: `0.0625` is exactly 62.5 mm and gives 63, while an `f32` written `0.0025` is
  2.49999994 mm and gives 2 (and `0.0035` is 3.50000011 mm and gives 4).
- `Mm::to_m_f32()` is `mm as f32 / 1000.0`: one exact conversion and one correctly rounded
  division. `from_m_f32(to_m_f32(mm)) == mm` holds for every `|mm| <= 2^23` (8.39 km), checked
  exhaustively by `mm_round_trip_and_rounding_rule`.
- `fixed::dot(a, b)` and `fixed::cross(a, b)` on `[i32; 2]` return `i64`. `cross` is always exact;
  `dot` is exact except for `[i32::MIN, i32::MIN] · [i32::MIN, i32::MIN]`, which saturates.
  `len2(a)` is `|a|²` as an exact `u64`, `isqrt_u64(n)` is `floor(sqrt(n))`, and `Mm2::isqrt()`
  returns the side in `Mm`.

### The `math_rng_fixed_vectors` golden

The engine scenario `math_rng_fixed_vectors` (registered with the other engine scenarios,
`regia_detsim::golden::vectors`) runs a fixed table through every function above: 64 ticks of 64
inputs. Tick 0 holds the special values (`±0`, subnormals, `±MAX`, `±inf`, NaN, the overflow edges,
large trig arguments, the i32 millimeter edges). Later ticks hold a sweep, uniform values in `[-1, 1)`
and raw bit patterns from the seed. Every tick also draws from two `DetRng` streams and runs the
integer helpers on the draws. The state is one hash per function family, named after the function. A
target whose bits differ is reported with its first diverging tick and, in a live `compare`, the
family that moved. Its golden is `crates/regia_detsim/goldens/math_rng_fixed_vectors.detgold`; the
`cargo xtask det-golden` legs run it with the other engine scenarios.

`match_rules_ctf_koth_3600` (ADR-8503) registers beside the engine scenarios. It hashes a 3600-step
capture-the-flag and king-of-the-hill session every 60 steps. The golden lives in
`crates/regia_detsim/goldens/` and the runners in [Determinism goldens on every target](det-golden-runners.md)
check it on host, qemu, Android, Mac, Windows and Deck.
