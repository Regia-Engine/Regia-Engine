# Attack-token queue regression recipe

The queue model in `regia_ai::attack_tokens` is an **internal test primitive**. It is not available
through the agent protocol and must not be called from game code. Run its bounded history suite with:

```sh
cargo test -p regia_ai --test attack_token_queue_model
```

The test compares a small independent model with the supported token functions after every operation.
It covers all 24 orderings of four agents, two target pools, repeat requests, release/cancel, capacity
changes, fixed-step expiry, death, target removal, preemption, holder uniqueness and counters. A separate
sequence verifies that a waiter which returns after its TTL rejoins behind a peer that kept asking. A
slot-reuse case confirms the next entity generation inherits no token or waiter state.

To exercise the existing agent surface that configures a pool, use `ai.squad_set` with `token_pool` and
read the result with `ai.squads`. The existing happy-path test is
`ai_squads::tests::a_squad_can_be_built_read_and_edited`; malformed pool parameters return `F0002` with
`bad_tokens`, and a missing target returns `F0003` with `entity_not_found`. The queue operations themselves
remain Rust-only; they have no agent error codes.
