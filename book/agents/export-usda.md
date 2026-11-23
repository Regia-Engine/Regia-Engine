# Exporting a glTF binary to USDA (`regia export usda`)

`regia export usda` reads a glTF binary (`.glb`) and writes one USDA stage. The stage is an
exchange file for a DCC tool. It is not a cooked Regia format: it has no `format_version`, and
`regia upgrade-project` does not migrate it.

This command is CLI only. It has no agent method.

```text
regia export usda --glb prop.glb --out prop.usda
```

## What is written

The file starts with `#usda 1.0`. The default prim is `World`, an `Xform`. `metersPerUnit` is
`1` (a glTF file is in meters; a USDA file that omits the value is treated as centimeters) and
`upAxis` is `Y`.

| glTF | USDA | Notes |
|---|---|---|
| Default scene, or the first scene | Children of `World` | With no scene, the nodes that are not children of another node. |
| Node | `Xform` | Translation, orientation and scale, or one matrix when the node stores a matrix. An identity transform is omitted. |
| Triangle primitive | `Mesh` | One prim per primitive. Points and indices stay in the glTF order. |
| `POSITION` | `point3f[] points` | |
| Indices, or `0, 1, 2, ...` when the primitive has none | `int[] faceVertexCounts` (all `3`) and `int[] faceVertexIndices` | |
| `NORMAL` | `normal3f[] normals` (`interpolation = "vertex"`) | Omitted when the primitive has no normals, or when the stream length is not the point count. Normals are not invented. |
| `TEXCOORD_0` | `texCoord2f[] primvars:st` (`interpolation = "vertex"`) | The first UV set only. The `t` coordinate is not flipped. |
| Triangle winding | `orientation = "rightHanded"` | The same counter-clockwise order as the glTF primitive. |
| | `subdivisionScheme = "none"` | Without this, a mesh is treated as a subdivision surface. |
| Point bounds | `float3[] extent` | The axis-aligned bounds of that primitive's points. |

A node matrix is written as `matrix4d xformOp:transform`. glTF stores a column-major matrix, and
the translation is in the last column. The USDA matrix is the row-vector form, so the translation
is in the last row. A decomposed transform is `xformOp:translate`, `xformOp:orient` (a `quatf` in
`w, x, y, z` order) and `xformOp:scale`, and `xformOpOrder` lists those ops in that order: the
order the schema example uses, which is the opposite of the order the ops are applied, so the
scale is the most local and the result matches glTF's translate-rotate-scale.

Sibling names are USDA identifiers (`A-Z`, `a-z`, `_`, then digits). A character outside that set
becomes `_`. Two siblings do not share a name.

Primitives that are not triangles are skipped. A file whose triangles all come out empty (no
mesh, only lines or points, or positions with no triangles) is `usda_empty`.

## UV origin

glTF `TEXCOORD_0` has its origin at the upper left. A USD preview-surface `st` set has its origin
at the lower left. This writer copies `t` as stored. A tool that samples the texture with the
preview-surface rule will see the set upside down until it flips `t`.

## What is not written

Materials, textures, skins, animation, cameras, lights and morph targets are not written. There
is no USDA reader in this command.

## Errors

The process prints `error: <code>: <detail>` and exits 1. A successful run prints the path, the
byte count, the node count and the mesh count, and exits 0. The stage is written to a temporary
file beside the destination and renamed into place. A failed write removes the temporary file.
`usda_empty` does not create the destination and does not replace a file that is already there.

| Code | Meaning |
|---|---|
| `usda_glb_missing` | The `.glb` path does not exist. |
| `usda_glb_invalid` | The file is not a glTF 2 binary, it does not parse, a number is not finite, an index is outside the primitive's points or does not fit in a USDA `int`, a triangle index count is not a multiple of 3, or the node hierarchy has a cycle. |
| `usda_empty` | The file has no triangle mesh to write. Nothing is written. |
| `usda_write` | The stage could not be written or renamed. |

## Checking the file

Open the `.usda` in a DCC tool, or compare its `points`, `faceVertexCounts` and
`faceVertexIndices` with the source primitive. The crate's tests do that with a small reader
that understands only the attribute text this command writes. That reader is not a USD parser.
