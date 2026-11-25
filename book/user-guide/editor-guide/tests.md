# Tests

<!-- index: Functional test -->

Panel id: `tests`. The **Tests** tab lists the functional tests placed in the open level, runs them inside
play-in-editor, and shows each test's verdict and step-by-step report. A functional test is an
entity with a `FunctionalTest` component whose body (a script or Rust code) plays the game and
checks the outcome, so a level carries its own gameplay checks. The tab is the human view of
the same run an agent starts with `editor.tests_panel_run`. Spec: `docs/specs/41-functional-tests-and-session-harness.md`. Depth: [Functional tests (agents)](../../agents/functional-tests.md).

## Open it

The tab is in the default bottom group (**View ▸ Reset layout** restores a closed one). An agent
opens it with `editor.layout_apply {"open": "tests"}`. It lists the tests of the open level; with
no test placed the table is empty.

## Walk-through: run the level's tests

1. Open the level and press **Refresh** to read its tests. Each row shows a checkbox, the test's
   name (hover it for its description), its tags, its owner, and its last verdict.
2. Tick the tests you want, or leave every box clear to run all. The run button reads **Run all**
   when none is ticked and **Run selected** otherwise. A disabled test cannot be ticked and shows
   `disabled` as its verdict; disabled tests are skipped.
3. Press **Run all** or **Run selected**. When play is stopped, the editor starts play first and the status text
   reads `N test(s) queued`, then `running`. The button is grayed out while a run is in progress.
4. When the run ends, the status text shows the counts (for example `2 passed, 1 failed`) and each row's
   **last** column shows its verdict.
5. Under the table, one collapsible section per test shows its verdict, its message, each step with its verdict and
   frame count, and the message of each pass.
6. Click a test's name to select its placed entity (open the Inspector or Outliner to edit it). With the viewport's **Bounds and ranges** show
   option on, a placed test is drawn as a green box with its name and a line to its viewpoint.

## What is on screen

| Action | Control / key | Agent method |
|---|---|---|
| Re-read the level's tests | **Refresh** | `editor.tests_panel_state` (reads them fresh) |
| Choose tests to run | The checkboxes | `editor.tests_panel_run` with `tests` |
| Run (starting play if needed) | **Run all** / **Run selected** | `editor.tests_panel_run` |
| Read the list, verdicts, status and report | the tab | `editor.tests_panel_state` |
| Select a test's placed entity | Click the test's name | `editor.tests_panel_open` with `test` |
| Place a test or run one without the panel | none | `functest.create`, `functest.run`, `functest.status`, `functest.results` |

An agent polls `editor.tests_panel_state` for the report after `editor.tests_panel_run`.

## Keyboard

None of its own. The buttons and checkboxes are focusable (Tab, then Space or Enter).

## Known limits

- A run needs a play session whose app has the functional-test runner; otherwise the status reads
  `no_runner: this play session has no functional-test runner`.
- If you stop play while a run is going, the run is gone with its world and the status reads `play stopped before
  the run ended`.
- Running nothing is not a pass: with no enabled test selected the run is refused (`no_tests`), and a second run is
  refused while one is in progress (`already_running`).
- Errors from an agent call: `no_tests`, `already_running`, `test_not_found` (an unknown name in `tests` or `test`),
  `bad_params`.
- Command-line runs (`regia test`) are described on the agents page.

## See also

[Play-in-editor](play-in-editor.md), [Functional tests (agents)](../../agents/functional-tests.md), [Layers](layers.md).
