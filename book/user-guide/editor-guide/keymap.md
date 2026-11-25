# Keymap

<!-- index: Keymap -->
<!-- index: Shortcut -->
<!-- index: Command palette -->

The keymap is the list of chords that run editor commands. It is not a dock panel id. You edit it on the **Preferences** tab (`preferences`), under **Keymap**, or from the command palette. The chords the editor ships, and the ones you replace them with, are the same list. Depth: [Editor preferences, keymap, layout](../../agents/editor-preferences.md), [Editor command palette](../../agents/editor-command.md).

Open **Edit ▸ Preferences…**, then **Keymap**. An agent opens that tab with `editor.layout_apply {"open": "preferences"}`.

## Change a chord

1. Find the command in the Keymap list, or press Ctrl+Shift+P, highlight the command and press F2.
2. Press the new chord (**Set…** in the list). The label spells modifiers as `Ctrl`, `Shift` and `Alt`. On macOS, Ctrl in a label is the Command key.
3. The binding is saved at once in the user config file `regia/editor_keymap.ron` (format version 1). A project does not carry this file. Copy it if you want the same chords on another machine.
4. **Default** on a row clears your chord and restores the built-in one. **Reset all shortcuts** clears every user binding. **Import…** and **Export…** read and write the same file shape. A file from a newer editor (`keymap_newer`) is left untouched.

A user chord replaces the built-in chord of that command. If two commands claim one chord, the Keymap tab lists the conflict. The user binding wins while it is set. Bindings for command ids this editor does not know stay in the file and are written back, so a newer editor's chords are not dropped.

## Chords you cannot move

| Chord | Stays |
|---|---|
| Ctrl+Shift+P | Opens the command palette. A binding that uses it is dropped (`reserved`). |
| Escape | Stops play. A safety key: it stays global. |
| Ctrl+S | Save. Safety key. |
| Ctrl+Z | Undo. Safety key. |
| Ctrl+Shift+Z and Ctrl+Y | Redo. Ctrl+Y is also a fixed editor key beside the palette command. Safety keys. |

Binding a safety key in a panel scope is refused (`shortcut_safety_key`).

## Where a chord applies

A binding has a scope. The focused panel's scope is tried first, then the global bindings. While a text field has the keyboard, only text-scope bindings run, so a letter you type into a name does not run a viewport command.

| Scope | When it wins |
|---|---|
| `global` | No more specific scope claimed the chord |
| `viewport` | The viewport has focus. Built-in camera, gizmo, frame and collider chords start here |
| `outliner` | The Outliner has focus. Built-in Outliner chords start here |
| `inspector` | The Inspector has focus |
| `content` | The Content browser has focus |
| `graph` | A graph canvas has focus (material, animation, procedural, behavior, state tree, conversation, VFX) |
| `timeline` | A timeline has focus (sequence curves, shots, clips, montages, UI animation) |
| `text` | A text field has the keyboard |

The same chord may be bound in two different non-global scopes. A clash is reported only inside one scope, or against a global binding.

In the file, a scoped binding's key is `<scope>:<command id>` (for example `viewport:view.toggle_grid`). A global binding's key is the command id alone.

## Presets

The Keymap tab lists presets. **Default** is an empty user file: every command keeps its built-in chord. **Laptop** is for a keyboard without a navigation block: Drop to floor moves from End to Alt+Down in the viewport, and the Outliner's Delete also answers Backspace. Applying a preset replaces the current user keymap at once. You can save your own preset under `regia/keymaps/<name>.editor_keymap.ron`. A built-in preset cannot be overwritten (`preset_builtin`).

## Defaults you will meet first

The built-in chords are listed on [Editor](../editor.md) (viewport and Outliner) and [Editor guide](index.md) (undo, redo, the palette, play, zoom). F1 runs `help.context` in every scope, including while a text field has focus, and opens the help for the focused panel. **Set…** can move a command off its built-in chord, including undo, save and stop-play. It cannot assign Ctrl+Shift+P, and it cannot give a safety chord to one panel: that chord stays global.

The command id in the Keymap row is the id you pass to `editor.command`. Search the palette for `open` to bind a key that brings one panel forward (`window.open_viewport`, and the same shape for the other tabs).

## Actions

| Action | Control | Agent method |
|---|---|---|
| Read bindings and conflicts | The Keymap tab | `editor.keymap_get` |
| Set or clear one chord | **Set…**, **Default**, or F2 in the palette | `editor.keymap_bind` |
| Find which command a chord runs | Press a key in the Keymap search | `editor.keymap_lookup` |
| Import, export, reset | **Import…**, **Export…**, **Reset all shortcuts** | `editor.keymap_import`, `editor.keymap_export`, `editor.keymap_reset` |
| List, apply or save a preset | The preset list | `editor.keymap_preset_list`, `editor.keymap_preset_apply`, `editor.keymap_preset_save` |

## Known limits

- Ctrl+Shift+P is not rebindable.
- Safety keys cannot be scoped to one panel.
- A chord that does not parse is dropped (`bad_chord` in the file report, code `keymap_parse` when the whole file is refused).
- The numpad and the main digit row report as the same key, so a preset cannot bind a numpad view key separately.
- Held fly keys (`camera.fly_*`) are commands. Binding one moves it. They are not dispatched as a tap.

## See also

[Project settings](project-settings.md), [Editor guide](index.md), [Editor](../editor.md).
