# Inspector

Panel id: `inspector`.

The **Inspector** tab (docked right) shows the components of the selection through reflection:
one collapsing section per component, one row per field. Every edit is an undoable property
change. With several entities selected, fields that agree show their value and fields that
differ are marked mixed; an edit sets it on all of them as one undo step. Depth: [Editor
surface](../../agents/editor.md), [Editor § several entities](../editor.md).

| Action | Control / key | Agent method |
|---|---|---|
| Read the selection's components | the sections | `editor.inspect_selection`, `scene.get_entity` |
| Edit a field | Type or drag in its row; Enter confirms a text field | `editor.set_property` (one entity or `entities`) |
| Move, rotate, scale exactly | The `Transform` section | `editor.set_transform` |
| Add a component | **Add component…**, then search and pick (Up/Down highlight a row; Enter adds it, or the first match when none is highlighted) | `editor.add_component` |
| Remove a component | **✕ Remove** in its section or **Remove …** in its header's right-click menu (one entity), **Remove component** menu (several) | `editor.remove_component` |
| Set an asset field (mesh, material, sound, ...) | Drop an asset from the Content Browser on the field, or pick one with its 🔍 button (search, thumbnails) | `editor.asset_field_choices`, then `editor.set_property` |
| Use the Content Browser's selected asset, clear, show in the Content Browser | **Use selected**, **Clear**, **Browse** beside the field | `editor.asset_field_set` (`from_selection`, or `path`) |
| See whether an asset is shared; give this field its own copy | the link icon and count beside the field; **Make unique** | `editor.asset_referencer_count`, `editor.make_unique` |
| Edit the asset a field uses (data asset, material, physics material) in place | the ▶ arrow before the field: the asset's fields under it; each edit is one undo step; **Save** there or File ▸ **Save all** writes the file | `editor.inspect_asset_inline`, `editor.asset_inline_set`, `editor.save_all` |
| Point an entity field at another entity | **Pick**, then click the entity in the viewport or Outliner (Esc cancels) | `editor.set_property` |
| Edit a curve or gradient field | click its mini graph (or Enter): canvas, key table, **Add key**, presets | `editor.curve_field_edit` |
| Pick a color | click the swatch: RGB, 0-255, HSV, OKLCH, hex, intensity (HDR), swatches, recent | `editor.set_property`, `editor.color_swatches` |
| Pick a color from the screen | the drop button beside the swatch, then click any pixel of the editor window, viewport included (or arrow keys and Enter); Esc or a right click cancels; one undo step for every selected entity | `editor.color_eyedropper` |
| List and map elements | the **+**, **−**, **↑**, **↓** buttons of the field | `editor.set_property` with the whole value |
| Prefab override | **Revert**, **Apply** on an overridden field | `editor.set_variant_override` |
| Undo, redo | Ctrl+Z, Ctrl+Shift+Z / Ctrl+Y (outside text fields) | `editor.undo`, `editor.redo` |

An asset field shows the asset's thumbnail beside its path. While you drag an asset over it, the field is
outlined in the selection color when it takes that asset and in red when it does not; a refused drop changes nothing
and says why under the field (`assets/props/crate.gltf is not a material (material)`). A material field takes
materials, material graphs and material instances; a mesh field takes meshes and the built-in cube, plane and
sphere. Depth: [Inspector asset fields (agents)](../../agents/inspector.md).

Ctrl+F (with the pointer over the Inspector) focuses its search box; Esc clears it. Tab and Shift+Tab move
between its fields; Enter commits a numeric text field, and in the **Add component…** picker adds the highlighted row (Up/Down), or the first match. Findings about its icon buttons and unlabeled checkboxes:
[Editor accessibility](accessibility.md).

## Finding and keeping fields

| Task | Do | Agent method |
|---|---|---|
| Find a field | Type in the search box (Ctrl+F): letters in order, not necessarily together (`lgtclr` finds a light's color); matches are highlighted and their components open | `editor.inspect_selection { search }` |
| See only what you changed | **Modified only**: hides fields equal to their default, or to the prefab's value on a prefab instance | `editor.inspect_selection { modified_only }` |
| Keep a field at the top | Right-click its name ▸ **Pin to top** (keyboard: Tab to the name, Shift+F10, Tab to **Pin to top**, Enter); it shows in **Pinned** on every entity with that component (saved for you, not the project) | `editor.inspector_pin` |
| Keep the Inspector on one entity | 🔒 **Lock**; select other entities freely | `editor.inspector_lock` |
| Watch several entities | ⧉ **Open pinned inspector**: a floating Inspector for this entity; open more for others | `editor.inspector_open_pinned` |
| Add a component | **Add component…**, type to search (grouped by category, recent first; grayed entries say why) | `editor.component_types`, `editor.add_component` |

With several entities selected, the search box and **Modified only** filter the rows too (a field counts as modified when
it differs on any selected entity), the reset arrow resets the field on every entity, a linked vector gives every entity
the typed value with its other lanes following that entity's own ratio, and a `@Button` runs on every entity; each of
these is one undo step.

Fields a component marks with `@Category("…")` sit under their own collapsible headers, after the fields without a
category. An entity with many components gets a **Filter components** box above the list.

## Field help

Hover a field label, or reach it with Tab, and press F1 to open the component's manual section.
When no component section exists, F1 opens the Inspector guide. The rendered book navigates to
the section anchor; a source-only installation opens the Markdown page. Agents use
`editor.help_context { field: { component, path }, open: true }`.
