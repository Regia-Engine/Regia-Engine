# Cooked material graph parity

`CookedMaterialGraph::cook` compiles graph code and captures its instance parameter values,
switches and source texture slots. `to_postcard` / `from_postcard` then rebuild the runtime
material without compiling the graph. `to_material` maps cooked texture slot numbers directly to
the supplied image table; an absent image stays unbound and uses the renderer's white fallback.
UV1 and time-read flags and vertex displacement bounds are observable on the runtime surface graph.
The bounds are zero when the permutation has no vertex function.

## Coverage

The `cooked_postcard_material_matches_compiled_graph_feature_matrix` unit test in
`regia_matgraph::cooked` covers UV1 and time reads, vertex offsets with a nonzero culling bound,
texture slots 0–2 with an unresolved slot 0, sRGB and linear sampling, switch-selected code and
two different parameter blocks. It compares WGSL, graph key, flags, parameters, offset, texture
pixels and colour-space flags, plus the material shading fields used by the graph bridge. The
legacy shader-library rename regression remains in the same module.

Run the focused test from the engine workspace:

```sh
cargo test -p regia_matgraph --lib cooked
```

For the existing agent test runner, grant the calling agent the `build` permission and call:

```sh
regia agent call agent.run_tests '{"target":"lib","package":"regia_matgraph","filter":"cooked"}' --project <project-dir>
```

The result is a `tests:<n>` job; poll it with `agent.job_status`. A dry run reports the exact
Cargo command. A project without build permission receives stable code `F0012`; malformed
arguments receive `F0002`, and an unavailable method receives `F0008`.

`play.expect` is an existing running-app assertion surface, rather than a way to invoke Rust unit
tests. For a graph loaded in an app, use `play.expect` only on an observable registered property;
the parity matrix itself is deterministic CPU unit coverage and needs no app, renderer or GPU.
Malformed expectation predicates return `F0014`.
