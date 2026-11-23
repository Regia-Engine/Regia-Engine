# Independent viewports

The editor's main viewport keeps its camera, layout and display settings in three world
resources: `EditorCamera`, `ViewportLayout` and `ViewportSettings`. A tool built on the Regia Engine
editor crates often needs more views of the same world: a workspace view, a review lightbox, a
before/after toggle, a concept side-by-side. Each needs its own camera, show flags and quality
preset. `ViewportInstance` holds its own copy of all three.

```rust,ignore
use regia_editor::viewport::{viewport_panel_with, ViewportPanelState};
use regia_editor::viewport_instance::{CameraLinkGroup, ViewportInstance};
use regia_editor::views::ViewportQuality;

struct Compare {
    left: ViewportInstance,
    right: ViewportInstance,
    states: [ViewportPanelState; 2],
}

impl Compare {
    fn new() -> Self {
        let group = CameraLinkGroup::new(1);
        let mut left = ViewportInstance::new(10);
        let mut right = ViewportInstance::new(11);
        right.layout.quality = ViewportQuality::Game;
        left.link_to(&group);
        right.link_to(&group); // adopts the left camera
        Self { left, right, states: Default::default() }
    }

    fn ui(&mut self, ui: &mut egui::Ui, world: &mut World, device: Option<&RenderDevice>) {
        let [a, b] = &mut self.states;
        ui.columns(2, |cols| {
            viewport_panel_with(&mut cols[0], world, device, a, &mut self.left);
            viewport_panel_with(&mut cols[1], world, device, b, &mut self.right);
        });
    }
}
```

`viewport_panel_with` is the full viewport panel: the panes, overlays, GPU picking, box select,
gizmos, hotkeys and toolbar. It reads and writes only the instance it is given. The world still
holds what every view shares: the selection, the gizmo mode, camera bookmarks and the undo
stack. `viewport_panel` (the main viewport) is the same panel over the world resources, and it
behaves as before.

- **One `ViewportPanelState` per instance.** It holds the gizmo drag state and the box-select
  drag. Set `state.toolbar = false` to hide the toolbar (a lightbox tile, say) and drive
  `instance.layout` from your own UI.
- **Ids.** Keep `ViewportInstance::id` unique per world. It keys the instance's egui widgets, its
  renderer and its entry in the `ViewportInstances` registry.
- **Linking.** `CameraLinkGroup` is a shared handle. Every member's perspective camera follows the
  member that moved last. Your own camera code (a SpaceMouse through `nav::step`, a "match
  concept view" button) just writes `instance.camera`, and the group follows on the next draw.
  `unlink()` leaves the group. Groups work across worlds too, so a before/after over two worlds
  can still orbit together.
- **Headless.** `regia_editor::render::capture_instance(device, world, &instance, kind, [w, h])`
  renders one instance with its overlays, with no egui. `render_instance_pane` and
  `capture_instance_pane` take a full `PaneRequest`.
- **Closing a view.** Call `ViewportInstances::remove(world, id)` to free its registry entry and
  its renderers. An instance you just stop drawing keeps its renderer cached until then.
- **Agent.** Every drawn instance shows up in `editor.viewport_instances`. Agents can edit, link
  and capture instances; the panel picks up their edits on its next draw. See
  [Independent viewports (agents)](../agents/viewports.md).

Cost: in a world without its own `RuntimeRenderer`, each instance pane owns a frame renderer
(render targets at the pane size, pipelines, IBL maps), the same as one main-viewport pane. In
a world with a `RuntimeRenderer`, all views share it, and TAAU is off while more than one view
draws in a frame.
