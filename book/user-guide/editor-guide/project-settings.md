# Project settings

The **Project Settings** tab (**Edit ▸ Project settings…**) edits the project's `Regia.toml`
and keeps every key it does not show: name, id, version, build number, organization, the
default level, the platform targets and their quality presets, the content chunk rules, and the
input maps of `config/input.ron`. The **Preferences** tab (**Edit ▸ Preferences…**) holds your
own editor settings and the keymap; it saves as you change it. **Edit ▸ World settings…** opens
the level's world settings. Walk-through: [Package a build](../../tutorials/package-a-build.md).
Depth: [Editor preferences, keymap, layout and project settings](../../agents/editor-preferences.md),
[Content Chunks panel](../../agents/content-chunks-panel.md), [Project structure](../project-structure.md).

## Project Settings tab

| Action | Control | Agent method |
|---|---|---|
| Read the settings | the panel | `editor.project_settings_get` |
| Name, id, version, build number, organization, default level | The top rows, **Default level:** | `editor.project_settings_set` (`project`) |
| Platform targets and quality presets | **Platforms and quality presets** | `editor.project_settings_set` (`platforms`) |
| Content chunk rules | **Add chunk**, **Remove rule**, **Entry roots**, **Content: all / closure**, **Chunk host (base_url):** | `editor.content_chunks_set_rule`, `editor.content_chunks_state` |
| Preview what the chunk settings would ship | **Preview chunk assignment** ([Content Chunks](content-chunks.md#choosing-what-ships-closure-settings)) | `editor.content_chunks_preview` |
| Input maps | **Input maps (config/input.ron)**: **Add**, **Remove**, **Set modifiers and triggers** | `editor.project_input_edit` |
| Save, reload | **Save**, **Reload** | `editor.project_settings_set` (saves unless `save: false`) |

## Preferences tab

| Action | Control / key | Agent method |
|---|---|---|
| Find a setting | **Search** | `editor.prefs_panel`, `editor.prefs_get` |
| Change a setting | its row | `editor.prefs_set` |
| Reset | **Reset** on a row, **Reset all to defaults** | `editor.prefs_reset` |
| Keymap | **Keymap** tab: **Set…** records a chord, **Default** clears it | `editor.keymap_bind`, `editor.keymap_get` |
| Import, export, reset the keymap | **Import…**, **Export…**, **Reset all shortcuts** | `editor.keymap_import`, `editor.keymap_export`, `editor.keymap_reset` |
| Rebind from the palette | Ctrl+Shift+P, highlight a command, F2, press the new chord | `editor.keymap_bind` |
| Editor theme | the theme settings | `editor.set_theme`, `editor.get_theme` |

## World settings

| Action | Control | Agent method |
|---|---|---|
| Open | **Edit ▸ World settings…** | `editor.world_settings_panel` |
| Read, change | the panel | `editor.world_settings_get`, `editor.world_settings_set` |

## Keyboard and accessibility notes

Every row is a focusable field or button, so the whole tab works from the keyboard. Some
fields' labels are separate widgets rather than the field's own label; see
[Editor accessibility](accessibility.md).

## Changed project settings

In **Edit ▸ Preferences… ▸ Project Settings**, a dot and bold label mark settings that differ
from a documented default. **Revert** resets one key; **Reset project** or the corresponding
category button resets every supported key in that category, even when search hides some rows.
Each action saves immediately and takes one undo step. Tab and Enter operate both controls.

Hover a key to read its default. Project identity and keys without a documented default remain
visible and are preserved by Reset. The exact defaults and agent parameters are listed in
[Project defaults and revert](../../agents/editor-preferences.md#project-defaults-and-revert-adr-8196).
