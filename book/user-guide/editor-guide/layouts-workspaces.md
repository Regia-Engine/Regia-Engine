# Layouts, workspaces and feature profiles

The editor's dock is yours to arrange: drag a tab to move it, out of the window to float it. This page covers keeping
arrangements, switching between task layouts and trimming the editor down to the features a project uses. Agents have
the same controls ([Editor layouts, workspaces and feature profiles](../../agents/editor-layouts.md)).

## Named layouts

**View ▸ Layouts** saves the current arrangement under a name (tick **Workspace** to also list it in the workspace
switcher), shows a saved one, renames it (type the new name, then **Rename**) and deletes it. Each layout is one file in
your user config folder (`layouts/<name>.editor_layout.ron`), so you can copy it to another machine. **View ▸ Save
layout as my default** makes the current arrangement the one new projects open with, and **View ▸ Reset layout** brings
back the built-in one.

## Workspaces

The **Workspace** list at the right end of the toolbar (also **View ▸ Workspace**, and `Workspace:` entries in the
command palette, Ctrl+Shift+P) switches between task layouts:

| Workspace | Opens |
|---|---|
| Level | the built-in layout |
| Animation | animation graph, clips, skeletons, montages, motion matching, ragdolls |
| Materials | material graph, material instances, look development |
| UI | the UI designer, its animation and inspector panels, theme, localization |
| World | terrain, procedural graphs, the 2D tile mode, layers, world settings |
| Cinematics | shots, sequence curves, cameras, video |
| Debug | log, problems, tests, network, size map, and the profiler window |
| Terrain | terrain tools and viewport, with terrain brushing active |
| 2D tiles | tile tools and viewport, with tile painting active |

Switching never closes a document, and switching back shows the arrangement you left, exactly. Your own workspaces are
the named layouts you saved as workspaces.

Each workspace also selects a **Mode**: World, Terrain, 2D tiles, Animation or UI design.
The mode brings forward its tool palette and shows a strip of related tool-panel buttons.
Change Mode beside the workspace switcher to remember a different mode for this workspace
for the rest of the session. Your documents and brush settings remain open and unchanged.
User mode overrides reset on restart; saved layout files still contain only the arrangement.
Extensions can contribute workspaces, marked **(extension)** in the list.

## Maximize and full screen

**Ctrl+Space** (**View ▸ Maximize panel**, or **Maximize** in a tab's right-click menu) shows the tab under the pointer,
or the focused one, alone in the whole dock area; press it again to put everything back as it was. **F11** (**View ▸
Full-screen viewport**) shows the viewport alone and hides the menus, toolbars and status bar; F11 or Escape brings
them back. While a play session runs, Escape stops play first; the next Escape leaves full screen. Both keys can be
rebound in **Edit ▸ Preferences… ▸ Keymap**.

## Changing a pane

Every tab group has a **+** button at the end of its tab bar. Its list starts with the tabs already in the group (handy
when a crowded tab bar hides some: pick one to bring it forward), then every panel you can add there. Right-click a tab
for **Replace with**, which puts another panel in its place. While you drag an asset or an entity, holding it over a tab
for 30 frames (about half a second) brings that tab forward, so you can drop into a panel that was behind another.

The **+** button is named **Add panel** for screen readers and is reachable with Tab;
Enter opens its menu. Its **Join with neighbor** submenu merges this area's tab stack into
the selected neighboring area. **Swap with** exchanges two areas' tab stacks while keeping
their sizes and positions. Both commands also appear in a tab's right-click menu. These
are view changes and do not enter document undo history.

## Feature profiles

**View ▸ Feature profile** hides whole feature areas the project does not use. **Full** shows everything; **UI only**
keeps the core panels (viewport, outliner, inspector, content, log, settings) and the UI designer's panels. Hidden
panels leave View ▸ Panels, the palette, the `+` list and Replace with, and their tabs close; Create loses the entity
kinds of hidden areas. Pick **Full** to bring everything back. The active profile belongs to the project
(`.regia/editor_profile.ron`); a profile you build with an agent can be saved to your `profiles` folder and shared as a
file.

## The window comes back

When you close the editor, it remembers for this project the window's monitor, size, position and full-screen state,
the open level and the documents open in the Material and animation graph editors. The next time you open the same
project they come back. If that monitor is no longer connected, the window opens centered on your primary display.

## Panels in their own windows

Right-click a tab and pick **Float in own window** to give that panel a window of its own: drag it to another monitor,
resize it, keep a graph editor (material graph, anim graph, sequence curves, montage) beside the level. **Dock back** on
the same menu, or the window's close button, puts its panels back into the main window; nothing closes. Tearing a tab
out of a group with the mouse floats it too. From the keyboard, the command palette (Ctrl+Shift+P) has **Float the
focused panel in its own window** and **Dock every floated panel back**.

The windows share everything with the main window: the theme, the UI scale, the open level and play. Drag an asset from
the Content Browser in one window and release it over a field in another: the drop lands there (the field does not
light up while you drag across, the window system keeps the pointer in the window the drag started in).

Saved layouts remember where each window was. If its monitor is gone the next time, the window docks back. On Wayland
the compositor decides where a window opens.

**Edit ▸ Preferences ▸ Appearance ▸ Floating windows** picks the mode: `os` (each floated group is its own window; the
default on Linux and Windows) or `in_window` (single-window mode: floated panels stay inside the editor's window).

## More than one Outliner, Content Browser or Inspector

**Window ▸ New Outliner**, **New Content Browser** and **New Inspector** open another copy of the panel beside the
first (up to eight of each; the command palette has the same three commands, so no mouse is needed). The copies are
titled "Outliner 2", "Content 3" and so on, and each keeps its own view: an Outliner its search and columns, a
Content Browser its folder and search, an Inspector its lock. Use two Outliners with different searches to compare
parts of a level, or lock one Inspector on an entity while another follows the selection. The layout remembers every
copy and what it showed, and brings them back when the project opens. Closing a copy forgets its view; the next new
copy starts fresh.
