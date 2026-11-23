# Material graph diagnostic replay

Material graph compile errors carry a node or path identity and a message. Repeating a compile of
the same graph returns those diagnostics in the same order. Rebuilding node and function maps in a
different insertion order preserves that sequence. Parameter and texture declarations are ordered
inputs: their vector order determines their shader slots and must be preserved by callers.

## Replaying a failure

Use the existing `material.graph_set` surface to store an invalid graph. For example, set a graph
whose `bad_dot` node dots a `Vec3` and a `Vec2`, and whose `bad_add` node references two missing
inputs:

```json
{
  "name": "diagnostic_replay",
  "nodes": {
    "bad_add": { "Add": ["missing_left", "missing_right"] },
    "bad_dot": { "Dot": ["position", "uv"] }
  },
  "output": { "normal": "bad_dot", "emissive": "bad_add" }
}
```

The `material.graph_set` response includes `ok: false` and the ordered `errors` array. Each entry
has `node` and `message`; missing references name `bad_add`, and the dot type error names
`bad_dot`. Do not sort that array when recording or comparing it. The graph stays in the library,
so send another `material.graph_set` with corrected nodes and outputs, then call
`material.compile { "name": "diagnostic_replay", "include_wgsl": true }` to confirm compilation.

Function expansion errors identify the originating call path. A recursive pair of inline functions
called from `nested` reports the nested call path and a cycle message alongside unrelated graph
errors. The `regia_matgraph` validation suite covers multiple missing references, a type error,
that nested cycle, repeated diagnostics, and reversing graph/function map insertion order. It then
compiles a valid graph through the already-used library and shader cache and compares its WGSL and
metadata with a fresh compile.

## Error handling

Compile errors are graph diagnostics, not a failed graph-set operation. A caller can inspect
`errors`, correct the graph, and compile again through the same library. Parameter and texture
slot numbers continue to follow their declaration arrays; the diagnostic replay tests retain that
order and do not reorder the declarations.
