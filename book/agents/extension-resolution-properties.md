# Extension resolution properties

The resolver is a pure operation over parsed manifests and project inputs. These checks exercise
that existing surface without loading an extension library.

## Run the checks

Run the focused integration targets and then the complete framework crate suite:

```sh
cargo test -p regia_framework --test extension_manifest --test extension_runtime
cargo test -p regia_framework
```

The manifest target enumerates 64 directed acyclic graphs with four extensions. Its independent
reference model computes dependency closure by repeated expansion, then checks that each dependency
precedes its dependent. It compares the resolved order, states, diagnostics and hash after reversing
discovery order, and supplies repeated roots in different orders. The fixtures also exercise optional
edges, unavailable optional dependencies, disabled dependencies, platform and profile filters,
conflicts, duplicate roots and cycles. A failed resolution followed by a valid resolution over the
same manifests checks that no call state leaks between runs.

The runtime target remains the integration check for filesystem discovery and registry behavior. No
dynamic extension is loaded by the resolution-property target.

The focused calls used for this page are `cargo test -p regia_framework --test extension_manifest
--test extension_runtime` and `cargo test -p regia_framework`. The first reports 15 manifest/resolver
checks and 43 registry/runtime checks. The injected indegree-count defect was caught by the new oracle
with `dependency dd must precede aa` at graph mask `0b001000`; the temporary change was reverted before
the clean run.

## Diagnostic identities covered

The property fixtures compare complete diagnostic records, including extension id, code, severity
and detail, across equivalent input permutations. Expected resolver codes include `conflict`,
`dependency_cycle`, `dependency_disabled`, `filtered`, `optional_dependency_missing`, and
`version_unsatisfied` where its existing fixture applies. These are resolver diagnostics, not new
agent-protocol error codes. The agent methods remain documented in [Extensions](plugins.md).

## Filter boundary

Platform filtering is exercised on roots and optional dependency edges. The current resolver input has
no audience field and does not filter `code.audience`; audience filtering cannot be exercised as a
resolver property without a separate runtime/API change. No audience behavior is claimed by this page.

## Agent protocol

No new agent method is introduced. To inspect a project's resolved extension set, use the existing
`plugin.check` method or `regia plugin check --project DIR`; the check is a dry run and reports
resolver diagnostics without loading a dynamic library. `plugin.list` reports the resulting active,
failed, filtered, disabled and available states. The established plugin method schemas and error codes
are in the [Extensions](plugins.md) guide.
