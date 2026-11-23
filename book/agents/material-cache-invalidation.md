# Material cache ownership and invalidation

`ShaderCache` deduplicates by the existing structural permutation hash. Graph names, parameter
values and texture paths do not add a shader permutation. Each successful tagged lookup registers
its library name as an owner, including a cache hit. Repeated lookups under the same name register
that owner once. An untagged lookup neither adds nor removes an owner.

Forgetting any registered owner invalidates the entire shared entry, including its ownership
metadata and FIFO position. `MaterialGraphLibrary::set` and `remove` use this rule for every
permutation associated with that name. Another name sharing the entry therefore misses on its
next compile. Existing `Arc<CompiledGraph>` holders remain valid; recompilation preserves the
same shader code and key. The new entry registers only owners that look it up after reinsertion.

The cache holds at most 256 structural entries. Hits do not refresh FIFO order. Eviction,
`forget`, `forget_tag` and `clear` remove ownership along with the affected entries; failed
compiles register no owner. `clear` and invalidation preserve hit/miss counters. The owner sets
contain only distinct names that accessed a live entry; the number of aliases is not a new
permutation limit.

## Inspect through existing agent methods

`material.graph_set` creates or replaces a named graph and compiles it immediately.
`material.compile` reports `cache_hit`, `key` and `cache`; `material.graph_get` without a name
reports the library and cache counters. Use the same graph under two names, compile both,
then replace the second with a structurally different graph. Compiling the first must miss
once and then hit, retaining its original key. Replacing the second graph invalidates the
shared entry even if that name originally obtained it through a hit.

```sh
regia agent call material.graph_set '{"name":"cache_a","nodes":{"v":{"Float":0.5}},"output":{"roughness":"v"}}'
regia agent call material.graph_set '{"name":"cache_b","nodes":{"v":{"Float":0.5}},"output":{"roughness":"v"}}'
regia agent call material.compile '{"name":"cache_a"}'
regia agent call material.compile '{"name":"cache_b"}'
regia agent call material.graph_set '{"name":"cache_b","nodes":{"v":{"Float":0.75}}}'
regia agent call material.compile '{"name":"cache_a"}'
regia agent call material.compile '{"name":"cache_a"}'
regia agent call material.graph_get '{}'
```

Graph validation failures retain the existing `ok: false` and node-mapped `errors` result.
Malformed method arguments retain JSON-RPC invalid-params errors. This cache correction adds
no method or error reason.

## Run the regression tests

With the repository as the host's project root and `build` permission granted, the existing
`agent.run_tests` and `agent.job_status` methods run and inspect the cache suite:

```sh
regia agent call agent.run_tests '{"package":"regia_matgraph","target":"cache_tag_alias"}'
regia agent call agent.job_status '{"job":"tests:1","since":0}'
```

Use the returned job ID and event cursor on subsequent polls. Require a finished job with
`result.ok: true` and zero failed tests. A dry run (`"dry_run": true`) only resolves the command.
The existing reasons `run_tests_bad_name`, `run_tests_busy`, `run_tests_no_manifest`,
`run_tests_spawn_failed`, `job_id_invalid` and `job_not_found` are documented in
[Jobs and test runs](jobs.md). Follow the shared-host build limits when running tests through
an agent; its child cargo processes inherit the host environment.

Equivalent direct checks:

```sh
cargo test -p regia_matgraph --lib cache
cargo test -p regia_matgraph --test cache_tag_alias
cargo test -p regia_matgraph --test default_graphs_unchanged
```

`cache_tag_alias` checks both A/B lookup orders, untagged/tagged sharing, library replacement
and removal, switch permutations, repeated forgetting, clear, failed compiles, FIFO eviction,
surviving holders and a deterministic reference model for mixed operations. Cache unit tests
also inspect private entry/order/tag invariants, deduplicated owners and metadata cleanup.
`default_graphs_unchanged` pins the existing generated code and keys. No reference images are
created or updated.
