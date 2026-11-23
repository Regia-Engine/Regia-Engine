# `TestReport` JSON (v2)

Spec: `docs/specs/41-functional-tests-and-session-harness.md` § 2.7, § 4. Code: `regia_framework::functional::report`. Decision: ADR-1673; v2: ADR-7980.

```json
{ "format_version": 2, "build": "0.2.4", "seed": 0,
  "tests": [ { "name": "door_opens", "level": "levels/arena.level.ron", "tags": ["smoke"],
               "verdict": "Failed", "message": "[player is through] x: expected 1, got 0",
               "skipped": false,
               "passes": [ { "label": null, "verdict": "Failed", "message": "...", "frames": 120 } ],
               "steps": [ { "name": "player is through", "frames": 0, "verdict": "Failed" } ],
               "frames": 120, "blessed": false,
               "notes": ["door angle at begin: 0.12 rad"] } ] }
```

`notes` carries the text a body logged with `TestContext::note` (spec 41 § 2.4), in order; the field is
absent when the test logged no notes. `notes` is the v2 change (ADR-7980): a v1 report (no `notes`)
loads as v2 with empty notes and saves as v2.
A report is a run output, not project content (`regia test --report <path>` writes it where the caller points; no
project loader reads one back), so `regia upgrade-project` does not rewrite reports: `TestReport::from_json` migrates
a v1 report whenever it is read (ADR-8580).

`verdict` is one of `Pending`, `Active`, `Passed`, `Failed`, `Aborted`, `Undecided`. A skipped (disabled) test keeps `Pending` and is not a pass.
`wall_ms` is set for tests that ran and absent for skipped ones; it is informational, and equality checks between runs clear it. A newer `format_version` is an error.
JUnit XML is derived (one `testsuite`, one `testcase` per test, `&`, `<`, `>`, `"` and `'` escaped). Exit codes: 0 all passed,
1 a failure, 2 an aborted or undecided test, 3 nothing ran, 4 harness or launch failure. Shape hashed by `cargo xtask check-formats`
(name `test-report`).
The harness files `session-progress.json`, `session-result.json` and `session-failure.json` (v1) are described on the agent page.
