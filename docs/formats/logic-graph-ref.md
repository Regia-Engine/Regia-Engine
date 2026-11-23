# `logic-graph-ref` component, format version 1

A logic graph authored on a prefab or level entity (`regia_ai::logic::LogicGraphRef`, P-165, ADR-8105 (N2),
ADR-8566), saved inside `.level.ron` and `.prefab.ron` under the type path
`regia_ai::logic::graph_ref::LogicGraphRef`. `regia_ai::AiPlugin` compiles the named `.logic.ron` into the entity's
`LogicGraph` whenever the component is added or changed (`PreUpdate`), so the graph ticks from the next fixed step
like one attached by code. A prefab instance overrides the fields with the ordinary prefab overrides.

| Field | Type | Default | Notes |
|---|---|---|---|
| `graph` | `String` | `""` | The `.logic.ron`, project-relative with `/` separators. Absolute paths and `..` are refused. |
| `enabled` | `bool` | `true` | `false` takes the compiled graph (and its status) off the entity. |
| `overrides` | list of `(name: String, value: Bool(b) \| Int(i) \| Float(f) \| String(s))` | `[]` | Starting values for graph locals (the definition's `locals[].default`). The kind must match the local's default; an `Int` may start a `Float` local. |
| `format_version` | `u32` | `1` | A newer version is refused by the scene loader. |

A reference that cannot become a graph (no file named, a bad path, an unreadable file, a definition that does not
parse or compile, an override naming no local or of the wrong kind) leaves the entity without a graph, logs a
warning and is recorded in `regia_ai::logic::LogicGraphRefLoads` with its reason code (`no_graph`, `bad_path`,
`unreadable`, the compile reason, `unknown_local`, `override_kind`).

The scene format itself does not change for this component: a scene stores components by type path and
`scene_schema_fingerprint` hashes the envelope, not the component types, so `scene` keeps its version.

Migration note: version 1 is the first; there are no migrators (`logic_graph_ref_migration_sources()` is empty). The
shape fingerprint is `logic_graph_ref_schema_fingerprint()`, registered with `cargo xtask check-formats` as
`logic-graph-ref`.
