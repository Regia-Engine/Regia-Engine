# Procedural macro compile checks

The `regia_macros` derives are compile-time APIs. Expansion tests cover token shape and parser
errors; this page records the compiler checks that also type-check the generated implementations.
The fixtures use small Rust stand-ins for the runtime traits so the compiler exercises the emitted
code without building another engine crate. The stand-ins mirror each real trait's supertraits,
required methods and provided defaults (see the doc comment in `tests/fixtures/support.rs`), so a
fixture that type-checks here exercises the same trait surface the real traits demand. The stand-ins
use `PhantomData` contexts and stub codecs rather than the real engine types, so this is surface
parity, not a claim of type-checking against the real traits themselves. A stand-in looser than its
trait would let a fixture prove a usage the real trait rejects.

## Run the checks

```sh
cargo test -p regia_macros
cargo test -p regia_macros --doc
```

The unit test `compiler_fixtures::derives_compile_and_run_in_isolated_rustc_crates` invokes the
host `rustc` once per positive fixture with `--test`, `--extern regia_macros=<built proc-macro>` and
an isolated output directory. It then runs the resulting test executable. The negative test
`compiler_fixtures::derives_reject_bad_inputs_with_stable_diagnostic_fragments` expects compilation
to fail and checks only a short diagnostic substring, never all of rustc's stderr. A separate test
passes an invalid script-order fixture to the harness with an expected-success result and asserts
that the harness rejects it.

## Compiler coverage

| Derive | Compiling fixture | Rejected fixtures and diagnostic anchors |
|---|---|---|
| `Script` | Renamed derive import, raw type identifier, generic `Send + Sync` bounds and `#[script(order = -7)]`; calls the generated constructor and checks the order. | Duplicate and field-level `#[script]`; out-of-range `order` includes `order must be an i32`. |
| `BtTask` | Renamed derive import, raw identifier and generic bounds; implements the required `tick`; calls the generated constructor. | A union is rejected by the derive itself: the diagnostic `` `#[derive(BtTask)]` cannot be derived for the union `<Name>` `` is spanned on the type, naming it. |
| `Replicate` | Struct fixture (renamed derive import, raw identifier, generic bound, skipped field) and an enum fixture (tagged variants, `quantize`/`angle`/`max_len` on variant fields, relying on the codec trait's default delta methods); both require the generated codec trait. A control fixture proves a valid `max_len` on a `String` field compiles. | Duplicate, type-level and unsupported field attributes; an invalid `max_len` value fails in the generated `MaxLen(n)` construction — the anchor is the rustc type error (``expected `usize` ``), which the control fixture shows is caused by the bad number alone. |
| `NetMessage` | Struct fixture (renamed derive import, raw identifier, generic bounds, direction, channel, rate and version, asserting the generated `CHANNEL`/`RATE_LIMIT`/`VERSION` constants) and an enum broadcast fixture requiring the marker trait. | Duplicate `version`, unsupported field attribute and version zero (`version` is a `u32` of at least 1). |

Diagnostic text outside the asserted anchor can vary by compiler version. The anchors name the bad
attribute, the expected type, or the rejected type; the tests do not depend on source paths, line
numbers, suggestions or complete stderr output.

## Runtime inspection remains on the existing guides

These derives do not add agent methods. Once the runtime plugins and registered values exist,
`scripts.list` and `scripts.get` inspect registered scripts and their slots ([Framework](framework.md));
`net.messages { "type": "game.fire" }` inspects a registered derived message
([Net assets and derived messages](net-assets.md)). An absent required plugin returns `F0003` on the
script calls. The macro fixtures validate compilation and generated trait implementations; they do
not claim to exercise those runtime calls or create new agent-protocol coverage.
