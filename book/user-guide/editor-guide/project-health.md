# Project Health

Panel id: `project_health`.

The **Project Health** tab gathers the checks the project already has into one ranked list: validator findings,
budgets, missing references, large assets nothing uses, cook warnings, toolchain problems from `regia doctor` and
autosaves a crash left behind. Each row says what was found, recommends an action and links to where you fix it.
The pre-commit hook that validates staged assets is set up in **Project Settings ▸ Validation**. Depth: [Project
health, staged validation and the Problems and Output Log lists (agents)](../../agents/project-health.md), the
[Problems](problems.md) tab.

## Open it

Choose **Tools ▸ Project Health**. From the keyboard press Ctrl+Shift+P and pick **Open the Project Health tab**
(`window.open_project_health`). An agent brings it forward with `editor.layout_apply {"open": "project_health"}`
and runs the checks with `editor.project_health {"action": "run"}`.

## Run the checks

1. Press **Re-run**. The checks run as an editor job: a spinner shows "running (project_health:N)" and the button
   is disabled until it ends. The job can be cancelled like any editor job; the rows of the checks that ran stay.
2. The header then counts the errors, warnings and notes. "No checks ran yet: use Re-run." means the tab has not run
   in this session; "No problems found." means every check passed.
3. The run also fills the [Problems](problems.md) tab with the same validator report.

## Read the list

Rows rank errors first, then warnings, then notes; within a severity by category, in this order:

| Category | Comes from |
|---|---|
| Missing reference | the missing-reference and missing-UI-image validators |
| Budget | the texture size, triangle and size budgets of `config/validation.ron` |
| Validation | every other validator: one row per validator and finding, with a count and example assets |
| Toolchain | `regia doctor` checks that warn or fail (a note when the CLI did not run) |
| Cook warning | the cook's import warnings, per source |
| Unreferenced | cooked assets of 1 MiB or more that nothing references and that are not a root |
| Recovery | autosaves left by a crash |

Each row shows its severity, category and title (hover the title for the details) and, under it, **Recommended:**
with the action to take. Click the title to select the row. The button at the end of the row follows the link:

- **Open** opens the asset the way a double-click in the Content Browser does.
- **Show in Problems** filters the Problems tab to the validator and brings it forward.
- **Copy command** copies the fix command (for example, a toolchain install step); it is never run for you.

**Checks**, folded under the list, says which sources ran and which were skipped, and why.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Run every check | **Re-run** | `editor.project_health {"action": "run", "wait"?, "doctor"?}` |
| Cancel a run | the editor job list | `editor.cancel_job` |
| Read the rows and the sources | The list, **Checks** | `editor.project_health {}` |
| Follow a row's link | **Open**, **Show in Problems**, **Copy command** | `editor.project_health {"action": "open", "row": N}` |
| Install or remove the pre-commit hook | **Project Settings ▸ Validation** | `editor.precommit_hook {"action": "install" \| "uninstall"}`, CLI `regia hooks` |

Tab and Shift+Tab reach **Re-run** and every row's link button; Space or Enter presses the focused one.
