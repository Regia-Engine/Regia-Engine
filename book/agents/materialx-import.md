# MaterialX import (`import.materialx_report`)

DCC and material-authoring tools exchange materials as MaterialX documents (`.mtlx`, an open standard with a public
specification). Regia imports one onto a [material graph](../systems/material-graph.md) (ADR-3341): drop
`wood.mtlx` and the images it names under `assets/`, and `regia cook` (or the editor's import) turns the document's
surface material into a `regia_matgraph::MaterialGraph`, compiles it and cooks it exactly like a `.matgraph.ron`. A
`MaterialGraphRef` names the `.mtlx` path the same way it names a graph. No Regia format is added: the cooked output
is the existing cooked material graph, so there is no new format version and no migrator.

The mapping follows the public MaterialX Specification (document structure, node graphs, custom nodes, the standard
node library) and the public Standard Surface and OpenPBR Surface specifications.

## What is read

- **One material per file.** The first `<surfacematerial>` (or the one the `.meta` names); its `surfaceshader`
  input must connect to a top-level `standard_surface` or `open_pbr_surface`. A document with no material imports
  its first surface shader. Other materials give `mtlx_extra_material` warnings.
- **Node graphs** (`<nodegraph>`) are flattened into the material graph. A node read by several inputs is emitted
  once.
- **Custom nodes**: a `<nodedef>` with a `<nodegraph nodedef="...">` implementation is expanded at each use, its
  interface bound to the instance's inputs.
- **Parameters.** Interface inputs of node graphs and the value inputs of custom nodes (the instance's value, else
  the nodedef default) become material parameters with those values as defaults, then the shader's unconnected
  inputs (`base_color`, `metalness` / `base_metalness`, `specular_roughness`, and the emission and opacity inputs
  when the document sets them). Integer and boolean inputs stay constants (they pick branches). At most 15
  parameters; the rest stay constants with an `mtlx_param_limit` warning. Names: the input name for interface and
  shader inputs, `<instance>_<input>` for custom nodes.
- **Images** resolve relative to the document plus every enclosing `fileprefix`, to a project path, and cook through
  the graph texture path (`<image>#rgba8`). Colour images with an `srgb_texture` (or `g22_rec709`) colour space
  decode as sRGB; everything else, and every float or vector image, is linear. At most 7 images per material
  (`mtlx_texture_limit` beyond). Absolute paths, URIs, UDIM sets and paths that leave the project are refused
  (`mtlx_texture_path`); the image's `default` stands in.
- **Texture coordinates.** MaterialX puts the texture origin at the bottom left, Regia at the top left. A plain
  `image` samples the mesh UV as is; a `tiledimage` with constant tiling and offset becomes one `UvTransform` with
  the offset flipped (`v' = v·s + (1 - s - o)`); a texture coordinate computed by other nodes is flipped into
  MaterialX space and back (`flip_v: false` in the `.meta` turns this off).

## Surface shaders

| Regia | `standard_surface` | `open_pbr_surface` |
|---|---|---|
| `base_color` | `base_color × base` | `base_color × base_weight` |
| `metallic` | `metalness` | `base_metalness` |
| `roughness` | `specular_roughness` | `specular_roughness` |
| `normal` | `normal` (a `normalmap` wired straight in → tangent space, anything else → world space; a normal map that passes through another node first is dropped with an `mtlx_lossy_input` warning, ADR-5571) | `geometry_normal` (same) |
| `emissive` (nits) | `emission × emission_color × emission_scale` | `emission_luminance × emission_color` |
| `alpha` | mean of `opacity` × (1 − `transmission`) | `geometry_opacity` × (1 − `transmission_weight`) |
| `shading.clearcoat`, `clearcoat_roughness` | `coat`, `coat_roughness` (constants) | `coat_weight`, `coat_roughness` (constants) |
| `shading.anisotropy`, `anisotropy_rotation` | `specular_anisotropy`, `specular_rotation` × π | `specular_roughness_anisotropy` |

Every lossy mapping gives an `mtlx_lossy_input` warning naming `shader.input`, only when the document sets the
input away from its default. A connected input that is dropped is reported without being translated, so its
textures and parameters are not declared (ADR-5571):

- **Specular weight, colour and IOR** (`specular`, `specular_color`, `specular_IOR`; `specular_weight`,
  `specular_color`, `specular_ior`): dropped. The lit model's specular is a fixed dielectric F0 of 0.04 (IOR 1.5)
  tinted by metalness.
- **Subsurface** (`subsurface`, `subsurface_weight`): approximated by blending the diffuse colour toward
  `subsurface_color` by the weight; radius, scale and anisotropy are dropped.
- **Transmission**: approximated by coverage (`1 − transmission`, alpha blend); no refraction, tint, depth,
  scattering or dispersion.
- **Sheen / fuzz**, **thin film**, **thin-walled**, **diffuse roughness**: dropped (no such lobe or mode).
- **Coat**: weight and roughness only, as constants of the shading block. A textured coat weight or roughness is
  replaced by its default; coat colour, IOR, anisotropy, darkening, `coat_affect_*` and the coat normal are dropped.
- **Anisotropy**: a constant only; OpenPBR's anisotropy direction comes from the mesh tangent (`geometry_tangent`
  connections are dropped).
- **Opacity colour** (Standard Surface): averaged to one coverage value.
- **Standard Surface emission** is a unitless weight: `emission_scale` (default 1, the glTF factor convention)
  turns it into nits. OpenPBR's `emission_luminance` is already nits.

## Standard library nodes

| MaterialX | Material graph |
|---|---|
| `image`, `tiledimage` | `Sample` (+ `UvTransform` for constant tiling); `uaddressmode`/`vaddressmode` other than `periodic` and `filtertype` other than `linear` are lossy (the graph sampler repeats and filters linearly); `realworld*` sizes are ignored |
| `constant`, `dot` | a constant node, a pass-through |
| `add`, `subtract`, `multiply`, `divide`, `min`, `max` | `Add`, `Sub`, `Mul`, `Div`, `Min`, `Max` (folded when both sides are constant) |
| `power`, `mix`, `clamp`, `smoothstep` | `Pow`, `Lerp(bg, fg, mix)`, `Saturate` or `Min(Max(..))`, `Smoothstep` |
| `dotproduct`, `normalize`, `magnitude`, `absval`, `sin`, `cos` | `Dot`, `Normalize`, `Length`, `Abs`, `Sin`, `Cos` |
| `invert`, `modulo`, `floor` | `Sub(amount, in)`, `Mul(Frac(Div(a, b)), b)`, `Sub(in, Frac(in))` |
| `normalmap` | `UnpackNormal` + `NormalStrength(scale)` + `Normalize`, the tangent-space normal output (an object-space map reads as world space; `normal`/`tangent`/`bitangent` inputs are dropped) |
| `texcoord` | `TexCoord` (index 0) or `TexCoord1` (index 1; higher indices read set 1) |
| `position`, `normal` | `WorldPosition`, `WorldNormal` with `space="world"`; object space is `WorldPosition − ObjectPosition` and the world normal (lossy: rotation and scale are not undone) |
| `geomcolor`, `time` | `VertexColor`, `Time` |
| `extract`, `separate2/3/4`, `swizzle`, `convert` | `Swizzle` (and `Append` for `0`/`1` channels and widening) |
| `combine2/3/4` | `Append` |
| `ifgreater`, `ifgreatereq`, `ifequal` | constant operands: a **static** choice (only the chosen input is emitted). Varying operands: `Lerp(in2, in1, step)` with a steep `Saturate` standing in for the step (exact outside a 1e-5 band, lossy) |
| `noise2d`, `noise3d`, `fractal3d` | `Noise` (fractal: `octaves` clamped to 1-8, lacunarity 2 and diminish 0.5 fixed), rescaled to `(n·2 − 1)·amplitude + pivot`; a vector noise repeats one scalar noise in every channel (lossy) |

Every other node (`place2d`, `cellnoise2d`, `worleynoise*`, `hsvtocolor`, `luminance`, `tangent`, ...) gives **one
`mtlx_unsupported_node` warning per node** and a fallback constant of its type (mid grey `0.5` for colours, zero
otherwise), so the material still compiles. Nodes mapped to the nearest existing node because the material graph
lacks the exact one are listed in the report's `matgraph_gaps` (a step/select node, `place2d`, object transforms,
vector noise, custom fractal parameters, more than 7 images).

## Settings

The `.meta` `settings` string is RON; every field is optional.

| Field | Default | Meaning |
| --- | --- | --- |
| `shading_model` | `Lit` | `Lit` (Regia's lit model) or `Unlit` (the base colour shown as is; clearcoat and anisotropy off). |
| `material` | `None` | The `surfacematerial` to import by name; `None` imports the first. |
| `alpha` | `Auto` | `Auto` (blend when opacity or transmission is set), `Opaque`, `Mask(cutoff)`, `Blend`. |
| `emission_scale` | `1.0` | Nits per unit of Standard Surface `emission × emission_color`. |
| `flip_v` | `true` | Flip `v` between MaterialX's and Regia's texture origins. |
| `expose_surface_params` | `true` | Expose the shader's unconnected inputs as parameters. |

## The report

`import.materialx_report {path}` imports the project file in memory (with its `.meta` settings, when the sidecar
exists) and writes nothing. It needs the editor, which installs the importer
(`regia_agent::materialx_report::MaterialXReporter`).

```json
{ "method": "import.materialx_report", "params": { "path": "assets/materials/wood.mtlx" } }
```

Result: `{ path, version, material, shader, mapped: [{ node, category, mapped_to }], unsupported: [{ code, node,
category, message }], warnings: [...], textures: [{ name, path, color_space }], params: [{ name, source, value,
width }], matgraph_gaps: [...], emitted_nodes, compiles, compile_errors? }`. Node paths are `nodegraph/node` (a
custom node adds its instance: `instance/node`); shader inputs are `shader.input`.

The cook prints the same warnings into its report (`regia cook`), one line per warning:
`assets/materials/wood.mtlx: mtlx_unsupported_node NG_cells/cells: ...`.

## Error codes

| Code | Meaning |
| --- | --- |
| `materialx_importer_unavailable` | The app has no importer (a game build); call the editor. |
| `mtlx_wrong_extension` | `path` is not a `.mtlx` file. |
| `mtlx_io` | The file cannot be read or leaves the project. |
| `mtlx_too_large` | Over 8 MiB. |
| `mtlx_not_utf8` | Not UTF-8 text. |
| `mtlx_parse` | The XML does not parse, or declares a DTD (refused). |
| `mtlx_not_materialx` | The root element is not `<materialx>`. |
| `mtlx_version` | The `version` is missing or not 1.x. |
| `mtlx_structure` | An element without a name, or a name declared twice in one scope. |
| `mtlx_no_material` | No surface material (or not the one named in the `.meta`). |
| `mtlx_unsupported_shader` | The surface shader is neither `standard_surface` nor `open_pbr_surface`. |
| `mtlx_cycle` | Node references form a cycle. |
| `mtlx_limit` | Over 4096 graph nodes, references deeper than 192, or over 512 custom-node expansions. |
| `mtlx_settings` | The `.meta` settings do not parse. |

Warning codes (in the report, never errors): `mtlx_unsupported_node`, `mtlx_lossy_input`, `mtlx_texture_limit`,
`mtlx_param_limit`, `mtlx_texture_path`, `mtlx_bad_value`, `mtlx_missing_node`, `mtlx_extra_material`.

## Fuzzing

`import.materialx` is in `fuzz.targets` in the editor: it imports the text it is given and compiles an accepted
graph without validation. `crates/regia_asset_pipeline/tests/materialx_import.rs` mutates every fixture too.

## Limits and known gaps

- The editor's import copies the `.mtlx` alone; copy the images it names next to it (same relative paths).
- One material per file (`material` picks another); a shader inside a node graph is not imported.
- Graph textures sample with one repeating linear sampler, at most 7 images.
- Tests: `crates/regia_asset_pipeline/src/materialx_import/tests.rs` (mapping rules, error codes, limits) and
  `crates/regia_asset_pipeline/tests/materialx_import.rs` (the hand-written fixtures in
  `tests/fixtures/materialx/` compile to WGSL that validates with naga in every host shader; a project cooks).
