# App owner plugin histories

This page records internal quality coverage for the `regia_app` plugin lifecycle. The behavior is
an internal app primitive; it has no agent-protocol method, so there are no agent error codes or
agent calls to exercise.

The regression test `cargo test -p regia_app --test owner_plugin_histories` records builds,
finishes and cleanups while one `App` passes through two game-owner epochs. It covers nested tuple
registration, duplicate names within each epoch, a child registered by its parent's `finish`, and
engine plugin survival. It tears down each owner's schedule systems outside plugin callbacks before
calling `forget_owner_plugins`, then confirms that the same plugin names can register for the next
owner. Repeated `finish_plugins` calls verify that callbacks do not replay after finalization.

The test uses counters and an ordered event trace. It does not use a dynamic library, sleeps,
timing assertions, or source-tree fixtures.
