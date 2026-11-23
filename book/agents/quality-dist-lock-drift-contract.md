# Distribution lock drift contract (internal test recipe)

`regia_cli::distribution::lock::lock_drift` is an internal library primitive with no separate agent
method. The existing `upgrade-project --json` path exposes the result as `pin.lock_drift` and emits
`engine_lock_differs` when the count is nonzero; the distribution-upgrade guide documents that
command surface and its error code.

Run the focused regression with:

```console
cargo test -p regia_cli --lib distribution::lock
```

The tests write synthetic lockfiles into `regia_test_utils::unique_temp_dir` projects. The set oracle
is assembled from fixture tuples independently of the TOML parser under test. It counts each
project-side external `(name, version)` once, only for names also present in the engine lock. Entries
without `source` represent workspace packages and are excluded. Duplicate entries and package order
do not change the result. `None` means a lock is missing or unreadable; `Some(0)` means both locks
were readable and there is no drift. Inspection does not modify either input.

The seeding checks also keep this primitive's established behavior pinned: a missing engine lock is
a no-op, a project lock is never overwritten, and a valid absent project lock receives the engine
file bytes exactly. This is internal test coverage; it adds no agent method or error code.
