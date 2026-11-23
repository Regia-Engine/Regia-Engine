# Material safe policy

`regia assets lint-materials` and the editor methods `asset.gltf_material_lint`
and `asset.gltf_material_fix` share one material policy. The default profile is
`standard`. It repairs format problems and writes values the glTF specification
already implies. It leaves a valid authored appearance alone.

Use `compatibility` only when you want the older authoring conventions. That
profile changes appearance. The command, the JSON report and each finding name
which kind of action ran.

The agent page is [glTF material authoring lint](../agents/gltf-material-lint.md).
The decision record is ADR-8250.

## What the default keeps

A packed metallic-roughness texture is multiplied by `metallicFactor` and
`roughnessFactor`. A material with `metallicFactor` 0.35 and `roughnessFactor`
0.7 stays at those values under a default check and a default fix.

An omitted `metallicFactor` means 1. An omitted `roughnessFactor` means 1. The
default repair does not insert those keys.

Base color tints the surface. `KHR_materials_volume` `attenuationColor` tints
light traveling through the medium. The default repair keeps both colors. A
`thicknessFactor` of 0, and a volume extension that omits thickness, stays a
thin surface. The repair does not invent an attenuation color for that case.

These rules follow glTF 2.0 materials
(<https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#materials>) and
KHR_materials_volume, Base Color and Absorption
(<https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_materials_volume/README.md#base-color-and-absorption>).

## What still changes on the default profile

Format clamps move an out-of-range factor into [0, 1]. That finding is
`preserves_appearance`. HDR emission is scaled into `emissiveStrength` so each
nonnegative channel's color-times-strength product matches the authored product
within the documented tolerance. Transmission that omits an IOR receives the
specification value 1.5. Material extensions are added to `extensionsUsed`.

A name rule, and `classify_untextured`, are explicit artistic choices. They run
on `standard` as well as on `compatibility`. A name rule that sets untextured
metalness, or that raises emissive strength above the energy-equivalent value,
is labeled `changes_appearance`.

## How to keep the previous repair

Previous releases applied the compatibility conventions by default:

- both ORM factors were forced to 1
- a missing untextured metalness was written as 0, and roughness as 1
- any present `KHR_materials_volume`, including thickness 0, moved a non-white
  base color into attenuation and replaced the base RGB with white

Pass the profile on the command line. It overrides a settings file.

```sh
regia assets lint-materials assets/props --profile compatibility --fix --json
```

Or put it in the settings object used by the CLI and by the agent:

```json
{"profile": "compatibility"}
```

```json
{"method":"asset.gltf_material_fix","params":{"path":"assets/props/part.gltf","settings":{"profile":"compatibility"}}}
```

`--profile standard` forces the safe profile even when the settings file asks
for compatibility.

## Reading a report

Text mode prints the effective profile before the file list. JSON adds `profile`
and `policy` next to `ok` and `files`. Each finding's `effect` says whether that
repair keeps the shaded result (`preserves_appearance`) or changes it
(`changes_appearance`). Appearance-changing actions run only after an explicit
profile or an explicit name setting selects them.

Run the fix a second time to confirm it is done. A clean second pass reports
`changed: false` and an empty `remaining` list. The check reads the source file
and does not step the play clock, so confirm it from the method result rather
than from `play.wait_until`.
