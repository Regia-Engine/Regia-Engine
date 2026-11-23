# DLSS lifecycle concurrency tests (agents)

These CPU-only unit tests pin the per-device registry behavior used by the DLSS binding. They do
not require the NVIDIA SDK, a native runtime library, a graphics device, or a GPU.

## Run and inspect the tests

Use the existing `agent.run_tests` method documented in [Jobs](jobs.md), with package
`regia_dlss`, target `lib`, and filter `lifecycle`. For example, from the project root:

```sh
regia agent call agent.run_tests '{"package":"regia_dlss","target":"lib","filter":"lifecycle"}'
```

The returned `tests:<n>` job streams `running`, per-test `test`, and `summary` events. Poll it
with the existing job tools described in [Jobs](jobs.md); a failed assertion is reported with the
test name and captured output. The same tests can be run with Cargo directly:

```sh
env -u DLSS_SDK cargo test -p regia_dlss --lib lifecycle
```

The coverage checks that concurrent waiters are woken after an opening error or a caught panic,
only one waiter becomes the next opener, unrelated keys remain available, consecutive failed
generations leave no stale slot, and close/reopen handoff shares one value and closes each value
once. It exercises the existing internal registry without a native SDK or public mock API.

This page describes tests only; it adds no agent method, error code, or host-visible DLSS behavior.
