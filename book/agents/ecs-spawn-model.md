# ECS spawn-order model

`SpawnOrder<C>` records the order in which a tracked component arrives in a `World`. Its sequence
number follows component insertion/application order, so it gives repeatable sorting when reserved
entity ids were assigned in a different order. Removing the component or despawning its entity
forgets the live entry; inserting it again receives a later sequence. Replacing an existing
component value keeps its arrival. A recycled entity slot has a new generation and cannot inherit
the removed generation's entry.

This guarantee is per component type and applies only when `track_spawn_order::<C>` succeeds before
entities use `C` and before another hook owns the necessary component hook slots. Existing entities
or incompatible hooks leave tracking disabled; keys then fall back to entity-id ordering. Tracking
two types in one world keeps two independent live sequences. No total ordering is promised for
independent command queues whose application order is itself unspecified.

## Exercising the model

The fixed-seed test `fixed_seed_component_arrival_model_matches_every_prefix` maintains its own
ordered live-arrival lists and checks each operation prefix in `crates/regia_ecs/tests/spawn_model.rs`.
It covers direct creation, reserved ids with deferred component insertion, remove/reinsert,
replacement, despawn, unrelated mutation, repeated registration, two component types, and entity
generation reuse. The separate late-tracking case checks that failed registration leaves the remove
hook slot available.

Use the existing `agent.run_tests` tool; no new agent method is added. For example:

```sh
regia agent call agent.run_tests '{"target":"spawn_model","package":"regia_ecs","filter":"fixed_seed_component_arrival_model_matches_every_prefix"}'
```

To run the full crate suite through the same runner, use `target: "all"` with `package: "regia_ecs"`.
The runner may return its documented `run_tests_bad_name`, `run_tests_busy`,
`run_tests_no_manifest`, or `run_tests_spawn_failed` errors. During this lane, the test-runner call
was not yet executed; the direct cargo test result is recorded in the lane report.
