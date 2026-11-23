# API snapshot export fixtures

`cargo xtask api-snapshot` checks the existing public API contract in spec 57 § 11.1 and
`docs/api-snapshots/README.md`. The source scanner lists conditional production APIs on every host,
with their `#[cfg(...)]` text, and excludes test-only APIs. Exported names are the names a downstream
crate uses, including aliases and globs through private implementation modules.

The dedicated fixtures live in `xtask/tests/api_snapshot/fixtures/`. They are small Rust library
crates compiled directly with rustc by the existing xtask integration-test runner; no engine,
window, GPU, installed application, or agent connection is required.

```sh
cargo test -p xtask --test api_snapshot api_snapshot_export_fixtures -- --nocapture
cargo test -p xtask --test api_snapshot
cargo xtask api-snapshot
```

The first call checks nested `all`/`any`/`not`, argument order, methods, trait members, enum variants,
macros, explicit `#[path]` modules, aliases, globs, reexport cycles, and multiple export conditions
for the same public name, and shared impls on every conditional definition of an exported type
or trait. An always-false operand beside `test` is excluded in either argument
order. Boolean `true`/`false` forms follow the same rules; non-test `cfg(any())` and `cfg(false)`
remain conservatively listed. Repeated owner/impl conditions appear once, and deprecation marker
text keeps its existing `since ..: ..` form even under a conditional owner. The second also checks the existing deprecation, freeze, corruption,
update-refusal, and rotation policy tests in temporary fixture workspaces. The final call checks
the repository's real snapshots without updating them.

The compiler checks are independent of the scanner's results: expected visibility is written
explicitly for selected configurations (default, `extra`, the feature named `test`, `test + extra`, and `test + other`). Each
library compiles to a temporary rlib; a downstream crate is compiled with `--extern` to check both
accessible public names and rejected names. Successful probes and expected compiler diagnostics
are both required. The tests use unique temporary directories and do not change process-wide
configuration.

Expected negative probes include `E0425` (missing value), `E0433` (missing module/type path),
`E0599` (missing associated function or variant), `E0046` (required trait members), `E0407`
(a method absent from the trait), and `E0603` (private implementation module). Deprecation
probes use `--deny=deprecated`, so an active conditional deprecation must fail while its inactive
configuration compiles. Doc-hidden items remain accessible to Rust callers; only unconditional
`#[doc(hidden)]` items are omitted from snapshots. Conditional `cfg_attr(doc(hidden))` items remain
in the cross-configuration API. Conditional `cfg_attr(deprecated)` does not produce an unconditional
deprecation marker or allow an unconditional retirement. The ordinary `#[deprecated]` policy
continues to apply.

This is a bounded source scan, not arbitrary cfg satisfiability analysis. Non-test configuration
atoms remain unknown when recognizing test-only predicates; conditions on retained APIs are
preserved as written. The compiler probes establish the selected fixture combinations, not every
possible target/feature assignment. Macro expansion and conditional derives remain outside the
scanner's documented scope. There are no new protocol methods, error codes, commands, or snapshot
formats. Existing `api-snapshot` failures retain their text diagnostics and nonzero exit status.
