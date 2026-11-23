# Migration chain limit regression recipe

`regia_core::version::migrate` is an internal library primitive. It has no agent-protocol method,
so this recipe exercises the existing Rust API directly; no agent call or new protocol surface is
available for it.

The integration test `crates/regia_core/tests/migration_chain_guard.rs` uses transition records as
an independent list-walk oracle and callbacks that record an ID before changing the payload. Its
cases pin successful 31- and 32-step chains, refusal before a 33rd callback, sparse version jumps,
a target reached on step 32, callback failure after a nonempty prefix, and table-order independence
for unique `from` values. It also makes a successful call after each failure using the same payload
to show that a failed invocation does not retain loop state.

Run the recipe with:

```sh
cargo test -p regia_core --test migration_chain_guard
```

Failure leaves the payload mutations made by callbacks in place. The migration loop does not promise
rollback. Duplicate `from` entries retain the existing first-match lookup behavior; this regression
does not establish a new ambiguity policy.
