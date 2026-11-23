# Material coverage

Material lint checks the textures and extensions a glTF material actually uses.
It does not decide the color space from the image file name, and it does not
remove an extension the importer cannot render.

## What is checked

- A texture `index` must fall inside `textures` when that array is present.
  A document that omits `textures` is treated as unfinished authoring, not as a
  bad index. `textures[i].source` is checked the same way against `images`.
- A material that selects UV channel 1 (or any `texCoord`) must be used by a
  primitive that has that `TEXCOORD_n` set as a `VEC2` accessor. The finding
  points at the material `texCoord` and names the mesh and primitive.
  `KHR_texture_transform.texCoord` overrides the texture's own channel.
- Base color and emissive slots are sRGB. Metallic-roughness, occlusion and
  other data maps are linear. Normal slots are normal maps. A finding is
  recorded only when one in-range texture index is asked to be two of those.
- `thicknessFactor` must be nonnegative. `attenuationDistance`, when present,
  must be greater than 0. `ior` must be at least 1. Thickness 0 and an omitted
  attenuation distance are valid. These values are reported and left as authored.
- `KHR_materials_volume` without `KHR_materials_transmission` is reported.
  Transmission is not added for you.
- Every extension object on a material, including a `textureInfo` extension such
  as `KHR_texture_transform`, must be listed in `extensionsUsed`. Fix mode
  appends the missing names. It does not delete the extension.
- Extension JSON is walked to a depth of 32 and 65536 key and string bytes.
  Past either bound the walk stops, the counters stop, and the unread JSON stays.

## Compatibility

`compatible` is false when `extensionsRequired` names an extension the importer
does not read. The bytes of that extension stay in the file. An unknown
extension that is only in `extensionsUsed` stays too, and `compatible` stays
true. The report lists each name as `cooked`, `side_table`, or `unsupported`.

The same capability list is the `importer` array from `regia assets lint-materials`
JSON and from `asset.content_lint_rules`. `KHR_texture_transform` is a side
table: declaring it does not fail compatibility, and the transform is not baked
or removed by this check.

## What fix mode writes

The default fix still only completes safe declarations and the repairs selected
by the material profile. Coverage findings stay in `remaining`. `ok` is true
only when `remaining` is empty and `compatible` is true.

This check reads a source file. It does not advance the play clock. Assert
`ok`, `compatible`, and the finding code on `asset.gltf_material_lint`. Use
`play.expect` or `play.wait_until` for a gameplay flag such as `Time.paused`.
See [glTF material authoring lint](../agents/gltf-material-lint.md).
