# Fracture

The **Fracture** tab breaks a box or a project mesh into chunks for destruction: pick the
source, set the fracture and cook settings, preview the chunks (exploded in the viewport if you
like), then save a `.fracture.ron` plus a `.destructible.ron`, or write the settings into the
mesh's `.meta` so the cook fractures it. Depth: [Fracture (agents)](../../agents/fracture.md).

| Action | Control | Agent method |
|---|---|---|
| Read the panel | the tab | `editor.fracture_panel_get` |
| Source | **Box** or **Project mesh** | `editor.fracture_panel_set` |
| Settings | The fields (chunk count, seed, half extents, ...) | `editor.fracture_panel_set` |
| Preview the chunks | **Preview** | `editor.fracture_panel_run`; `editor.fracture_preview` fractures without the panel |
| Show in the viewport, explode | **Show in the viewport**, the **Explode** slider | `editor.fracture_panel_set` |
| Save | **Save** | `editor.fracture_panel_save` |
| Cook it from the mesh | **Apply to .meta** | `editor.fracture_panel_apply_meta` |

The tab has no keyboard shortcuts; every control takes focus with Tab. The three **Half
extents** fields share one label; see [Editor accessibility](accessibility.md).

More fracture tools (support picking, a test break in the editor, the bond view, placing in the
level) are on the branch `lane/sprint-v06-fracture-tool`;
this page gains them at the v0.6 integration.
