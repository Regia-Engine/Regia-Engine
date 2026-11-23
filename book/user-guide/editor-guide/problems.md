# Problems

The **Problems** tab lists what the asset validators found: a missing reference, a bad name, a
texture over its size limit, an orphaned asset, and the rules of the project's
`config/validation.ron`. It is the one list a validation run, a level save and an agent all fill.
Clicking a row selects the asset in the Content tab. The panel never changes content. Depth: [Asset
validators and the dependency graph (agents)](../../agents/asset-validators.md).

## Open it

The tab is in the default bottom group (**View ▸ Reset layout** restores a closed one); an agent
opens it with `editor.layout_apply {"open": "problems"}`. Validation reads the editor's loaded
asset registry, so cook the project first (`registry_not_loaded: cook the project first` appears
in red otherwise). Before any run it says "Nothing validated yet: use Validate in the Content
Browser, or Re-run."

## Walk-through: clean up a folder

1. In the **Content** tab, right-click a folder or asset and choose **Validate** (just that
   path) or **Validate with dependencies** (it and everything it reaches), or press the **Validate
   project** button in the Content tab's header.
2. Open **Problems**. The line under the toolbar reads `N passed, N failed, N with warnings, N not
   validated`. Rows follow, worst first (error, warning, info) and each reads `<severity> <code>
   <asset>: <text>`.
3. Hover a row for the validator's hint, when it has one.
4. Narrow the list: untick **error**, **warning** or **info** to hide a severity, or pick one
   validator in the drop-down (it lists the validators that have findings).
5. Click a row. The Content tab opens that asset's folder and selects it.
6. Fix the asset, cook, and press **Re-run**. It repeats the last scope (the whole project the first
   time).

Saving a level also queues a validation of what changed; its report fills this tab too, unless
`validate_on_save` is false in `config/validation.ron` ([the agents page](../../agents/asset-validators.md)
has the rule file).

## What is on screen

| Action | Control / key | Agent method |
|---|---|---|
| Validate a path, a path with its dependencies, the whole project | Content tab right-click menu ▸ **Validate**, **Validate with dependencies**; header button **Validate project** | `editor.problems_panel` with `run: true` (and `assets`), or `editor.asset_validate` |
| Repeat the last run | **Re-run** | `editor.problems_panel` with `run: true` |
| Hide or show a severity | The **error**, **warning**, **info** checkboxes | `editor.problems_panel` with `hide` |
| One validator only | The validator drop-down (**all validators** shows all) | `editor.problems_panel` with `validator` |
| Select the asset of a row | Click the row | `editor.problems_panel` with `select` |
| Read the whole panel | the tab | `editor.asset_panels_state` |
| Control the validate-on-save trigger | none | `editor.save_validation` |

The list lays out only the rows in view, so a project-wide run with tens of thousands of findings
stays responsive.

## Keyboard

None of its own. The checkboxes, the drop-down and **Re-run** are focusable (Tab, then Space or
Enter); each row is a selectable label.

## Known limits

- Needs a cooked registry; findings are about the registry's assets and the project's `.ui.ron`
  layouts, not about uncooked edits.
- A `config/validation.ron` that does not load is reported as an error and the run does not start.
- The panel shows findings; it does not fix them.

## See also

[Reference Graph](reference-graph.md), [Size Map](size-map.md), [Content
browser](content-browser.md), [Asset validators and the dependency graph
(agents)](../../agents/asset-validators.md).
