# The merge gate and goldens across machines

Every merge to `main` passes one gate: `cargo fmt`, `clippy -D warnings`, every workspace test, rustdoc with
`-D warnings`, the packaging end-to-end test (`package_e2e -- --ignored`), the Khronos glTF samples, and the
`xtask` checks (`check-versions`, `notices --check`, `check-formats`, `book-check`, `check-shaders`,
`check-unwraps`, `check-layers`). `book-check` also runs `agent-coverage` (`cargo xtask agent-coverage`):
every method registered in `regia_agent`'s dispatcher has a schema entry, a bundled `agent.guide` topic
that names it, a `book/COVERAGE.md` row, a mention on that guide page (and of each of its parameters, ADR-5866),
and a test that calls it. A gated `main` commit is tagged `green-YYYYMMDD-HHMM` (and `green`); consumers such
as DO27 pin those tags.

The gate runs on two kinds of host:

- **the workstation** (`gate.sh`): lavapipe on Mesa 26.2 / LLVM 23. The goldens are blessed here. Since 2026-10-14 nothing
  builds on the workstation, so `gate.sh` is not the merge gate any more. It also never ran `check-shaders`,
  `check-unwraps` or the doc tests.
- **The build machines** (`the gate script`, one gate at a time under `<scratch>`): 48 cores and shared
  RTX 5090s. It gates a commit, not a working tree. Before anything builds, it refuses the commit when:
  - a merge is in progress;
  - a `.rs`, `.toml`, `.md` or `.wgsl` file has a line that starts with a conflict marker (seven `<` or `>` and a
    space);
  - the commit's manifests do not parse as a git dependency from a throwaway host crate (what DO27 does).

  The commit is then exported with `git archive` and built in its own tree and target dir. Two passes run at the
  same time:
  1. **lavapipe pass**: every check above, goldens compared, and the doc tests. clippy and rustdoc build in their
     own target dirs beside the test build. The test binaries run up to 12 at a time, longest first, each the way
     `cargo test` runs it (package directory as cwd, `CARGO_MANIFEST_DIR` set), with a 1200 s limit per binary.
     Wall-clock-bound binaries run alone after the pool.
  2. **real-GPU pass** on a 5090: every test binary of the rendering crates that creates a device, with
     `REGIA_SKIP_GOLDENS=1`, 4 at a time, with the per-binary limit of `the gate script` (600 s now). It must pass,
     apart from its quarantine list. With `REGIA_PLATE_DIR` set, the pass also records each golden's PSNR, and `cargo xtask plate-floors check
     --vendor nvidia` fails it when a plate is under the NVIDIA floor (ADR-2089).
     A plate under its floor is not yet a card defect: `cargo xtask plate-floors attribute --plates <that run> --reference
     <the same test binaries' plates on lavapipe> --vendor nvidia` says, per plate, whether the golden moved on every
     device (`GOLDEN`) or the card's frame differs from the lavapipe frame (`VENDOR`) (ADR-5798). It fails (exit 1) on
     every other verdict: `SHARED` when the frames agree but the lavapipe run is not under a lavapipe floor of its own,
     `INCONSISTENT` when the scores and frames cannot come from one build (ADR-5897). A `GOLDEN` plate is a failure of
     `plate-floors check --vendor lavapipe` on the lavapipe run, so wiring that excuses one runs that check too.

     With `GATE_GPU_HOST=the test machine` (ADR-3317; the default is `the build server`) this pass runs on the test machine's RTX 5080
     instead: the rig builds the test binaries itself from the same commit (`git archive`), runs the same binaries with
     the same limits and retries, card pinned by UUID under its lock, and prints the same result lines. The per-binary
     limit and the package list are those of `the gate script` (600 s now). Lanes run short card tests on the 5080 with
     `rig-slot.sh --card the test machine CMD...`; while a gate holds the card they wait. Recipe and details:
     `docs/06-testing-and-ci.md` § 21 (`tools/gaterig/`).

  In both passes a binary that fails is run once more alone. If it passes alone, the gate reports
  `FLAKY UNDER LOAD` and does not count the failure; if it fails alone, it fails the gate. A binary over its time
  limit is rerun alone and reported as `HANG`. A signal at process exit after every test passed counts as passed
  and is printed as `EXIT-CRASH`. The exact rules, the solo and quarantine lists, and how a flake is recorded:
  [Flaky tests, retries and nightly jobs](flaky-tests.md). Nothing else is retried.

## Before you submit: `cargo xtask preflight`

`cargo xtask preflight` runs the gate's cheap checks in the gate's order and stops at the first failure:
- the gate's conflict-marker refusal;
- `fmt --check`;
- a fast pre-check for bare `rust` fences in `book/` that cannot compile;
- clippy and rustdoc;
- the xtask checks in the build machines gate's order: `check-versions`, `notices --check`, `check-formats`, `book-check`,
  `check-shaders`, `check-unwraps`, `check-layers`, `check-packaging`, `api-snapshot` (a removed or changed public
  item of a stable crate; `docs/api-snapshots/README.md` says how to update the snapshots, the deprecation window and the `freeze` switch);
- `doc-examples` (the compiled doc examples of the stable crates may not drop; `docs/ci/doc-examples.toml` holds the floors and `docs/04-conventions.md` § 14 the rule);
- the feature matrix (`cargo xtask check-features`, below; `--no-features` skips it);
- a test build (`cargo test --workspace --no-run`; `--no-build` skips it).

The options:
- `--changed` limits the cargo steps to the crates that differ from `main` (a renamed file counts for both crates)
  and the crates that depend on them.
- `--keep-going` (`-k`) runs every step and lists every failure at the end. That is one fix round instead of one per
  failing step.
- Every run ends with a table of step times. `--timings FILE` also writes the times as JSON (`scope`,
  `total_secs`, and per step `name`, `command`, `secs`, `outcome` = `passed` / `failed` / `not_run`).

Preflight prints the build-wrapper command to use on the workstation and never edits a file. It does not run the
tests themselves, the doc tests or the git-dependency parse.

## Optional features: `cargo xtask check-features`

The workspace clippy builds each crate once, with the features the workspace turns on. Code behind a feature no crate
turns on (DLSS, the Steamworks backend, the media codecs, `lua`, `xr`, the `regia` re-exports) and the build a shipped
game compiles (`regia_render` and `regia` with their defaults off) would otherwise never be compiled before a game
does. `cargo xtask check-features` runs `cargo clippy -p <crate> --features <set> -- -D warnings` once per row of the
feature matrix (`xtask/src/feature_matrix.rs`; the table is in `docs/04-conventions.md` § 11), keeps going after a
failure and ends with every failing row, its command and its error lines:

```text
check-features: rows
  render-shipping          ok    41.2s
  steamworks               FAILED 18.9s
  media-av1-nasm           skipped (needs `nasm` on PATH)
  ...
check-features: 1 row(s) FAILED
  steamworks: cargo clippy --locked -p regia_steam --features steamworks --all-targets --message-format short -- -D warnings
      crates/regia_steam/src/backend.rs:88:9: error[E0599]: no method named `...`
```

- `--list` prints each row's command and what it compiles; `--set NAME` runs one row; `--packages a,b` runs the rows
  of those crates (what preflight `--changed` does); `--check` uses `cargo check` (faster, warnings do not fail);
  `-j N` caps rustc jobs per row; `--fail-fast` stops at the first failure.
- A row that needs something the host lacks (`nasm` for the x86 AV1 assembly, Windows for `static-dxc`) is skipped
  and says why; it does not fail.
- Before it compiles anything it checks the table: a `[features]` key of a `crates/*` manifest that no row names
  fails the command (`--coverage-only` runs only this check). Add the row in the same commit as the feature.
- The DLSS rows compile the linked NGX path over a check stub of the binding (`REGIA_DLSS_CHECK_STUB=1`): no NVIDIA
  SDK is needed, nothing is linked, and only `check` and `clippy` work that way.

It compiles; it does not run a feature's tests. CI runs it in the `linux` job after clippy.

Formatting: lanes run `cargo fmt --all` before each commit. `gate.sh` (main checkout) and `the gate script` format and commit
`style: cargo fmt` themselves; the build machines's lavapipe pass (`the gate script`) only checks and fails on a diff, as does
preflight.

## Goldens on another lavapipe build

Goldens are single-reference (ADR-025). SSIM goldens (≥ 0.98) pass on every lavapipe build we have seen.
Pixel-exact goldens (a per-channel bound, e.g. ±2) can flip a handful of edge pixels when the LLVM that compiles
lavapipe's shaders changes. Compare them through `regia_test_utils::golden::check_pixel_golden`:

```rust,ignore
// In a test (regia_test_utils is a dev-dependency):
let reference = PixelTolerance { max_channel_diff: 2, outlier_ppm: 0 };
match check_pixel_golden(&device.adapter.get_info(), "my_shot", &pixels, &golden, w, h, reference) {
    Ok(verdict) => eprintln!("{verdict}"),
    Err(verdict) => panic!("my_shot drifted: {verdict}"),
}
```

`pixel_golden_for` decides the compare from the adapter:

| adapter | compare |
|---|---|
| lavapipe, LLVM 23 (the reference), WARP, any unlisted LLVM | the test's own tolerance, no outliers |
| lavapipe, LLVM listed in `LAVAPIPE_ICD_TOLERANCES` (19: 30 ppm) | same per-channel bound, up to that many ppm of pixels beyond it |
| hardware GPU | SSIM against the lavapipe golden (`ssim_threshold_for`, ADR-025) |

To add a lavapipe build, render the pixel goldens on it, count the pixels beyond each test's bound, and record
the numbers in `docs/06-testing-and-ci.md` § 2 next to the new table entry. Never re-bless a golden on a
non-reference build.

## Variables

| variable | effect |
|---|---|
| `REGIA_SKIP_GOLDENS=1` | `compare_rgba8_to_png` and, on hardware adapters, `check_pixel_golden` skip the lavapipe compare and print a note. Only set by the real-GPU pass. |
| `REGIA_PLATE_DIR=<dir>` | every golden compare records a plate there, also under `REGIA_SKIP_GOLDENS=1`: the frame as a PNG and its PSNR against the golden as a `.plate` line. `cargo xtask plate-floors check` holds them to the per-vendor floors in `docs/ci/plate-floors.toml` (`docs/06-testing-and-ci.md` section 13). |
| `REGIA_SPIRV_VAL_FALLBACK=1` | when `spirv-val` is **not installed**, `check-shaders` and the shader tests run naga validation and the explicit-layout scan alone. Without the variable a missing `spirv-val` is an error. Only a gate host without SPIRV-Tools sets it, and a warning is printed. The scan (`regia_render::spirv_explicit_layout_violations`, ADR-026's rule) runs on every host anyway, because older `spirv-val` releases (the build server's 2025.1) do not check that rule. An explicit layout on Function or Private storage is an error; `ArrayStride` on a Workgroup array is naga 30's known output and is reported as a warning (ADR-135), as is `spirv-val`'s VUID-10684 for it. |
| `CARGO_TARGET_DIR` | followed by the terrain cook test (finds `regia` beside its own executable) and by `regia package` (looks there first for the binary it built). |
