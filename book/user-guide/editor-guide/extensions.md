# Extensions

Panel id: `extensions`.

The **Extensions** tab lists the extensions your project can use (versioned bundles of code,
content or tools, described by a `regia-plugin.toml` manifest), shows what each one asks for, and
enables, disables or reloads it. It also hosts the tabs extensions add to the editor. Source
extensions are built (spec `docs/specs/57-plugin-extension-api.md`): Rust and content the project
compiles in. A dynamic extension is a library the editor can reload in a development build. A
shipping player does not load one, and this version has no extension registry or marketplace. The
manifest can carry publisher and licence fields. Depth:
[Extensions (agents)](../../agents/plugins.md), [Hot reload](../hot-reload.md).

**In-process extension code is trusted native code with the editor's full privileges.** The tab
shows a banner saying so; capabilities are a review aid, not a sandbox. Enable an extension only
from a source you trust.

## Open it

The tab is part of the default layout (the group under the viewport); **View ▸ Reset layout**
brings it back when closed. An agent uses `editor.layout_apply {"open": "extensions"}`. The command
palette has no `open` command for this tab.

## Review and enable an extension

1. Open the tab. The left column lists every extension as an id followed by its version, each with a colored dot:
   green is active, red is failed, gray is any other state (hover the dot for the state name).
   With no project open the tab says to open one; with none found it says "(no extensions found)".
2. Click an extension. The right column shows its name, version, state, kind, publisher, license,
   description, **Capabilities** (each one with its grant, or "not granted"), **Dependencies** (and
   what was found for each), what it **Registered**, and its **Diagnostics**.
3. Read the capabilities. For an extension that is available or disabled the button reads
   **Enable**, or **Grant (the missing capabilities) and enable** when it asks for capabilities that are not yet
   granted (`systems` and `settings` are granted implicitly). Press it. For an enabled extension the
   button reads **Disable**.
4. Read the message above the details. Code takes effect at the next start: the message then says
   "Done. Restart the editor to apply it.", and the bottom of the list shows "Restart the editor: N
   code change(s) pending".
5. For an extension the editor loaded as a dynamic library (built with `regia plugin build`),
   **Reload** swaps in the newly built library without a restart and reports "Reloaded (generation
   N)."

| Action | Control / key | Agent method |
|---|---|---|
| List extensions | the left column | `plugin.list` |
| Read one extension | click it | `plugin.info` |
| Enable, with grants | **Enable** / **Grant ... and enable** | `plugin.enable` |
| Disable | **Disable** | `plugin.disable` |
| Reload a dynamic extension | **Reload** | `plugin.reload` |
| Dry-run the resolver | none (not in the panel) | `plugin.check` |

`plugin.enable`, `plugin.disable` and `plugin.reload` need the `build` permission when an agent
calls them; the panel runs the same functions for a person.

## Tabs that extensions add

An extension can register an editor panel. While the extension is active, the editor adds a tab
with the title the extension chose, in the group that has focus, and removes it when the extension
is disabled or reloaded away; a tab whose extension is not loaded reads "(this extension is not
loaded)". Such a tab is not stored in a saved layout and has no id for `editor.layout_apply`: it
appears and disappears with the extension. What the tab shows and which keys it handles is up to
the extension.

## Keyboard

The Extensions tab has no shortcuts. List rows, **Enable**, **Disable** and **Reload** take focus
with Tab and press with Enter or Space.

## Known limits

- The tab never runs the dry-run resolver; use `plugin.check` or `regia plugin` on the command
  line.
- **Reload** appears only for a dynamic extension. A source extension reloads with the game's hot
  reload, and a content-only extension remounts by disabling and enabling it.
- Dynamic libraries are for development only: they never load in a shipping player and do not
  exist on Android.

See also: [Editor guide](index.md).
