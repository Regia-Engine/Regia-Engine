# `<level>.expected.ron`: expected values (v1)

Spec: `docs/specs/41-functional-tests-and-session-harness.md` § 2.6. Code: `regia_framework::functional::expected`. Decision: ADR-1673.

```ron
(
    format_version: 1,
    tests: {
        "door_opens": { "final_pose": (translation: [1.0, 0.0, 2.0]) },
    },
)
```

Test name, then key, to the value `TestContext::expect_value` serialized with the reflection serializer (JSON data model, RON text).
Keys are sorted; the file is written atomically (temp file, then rename). Floats compare at a relative tolerance of 1e-5
times the absolute expected value (an expected 0 matches only an actual 0) and mismatches print a field-path diff. A missing
entry fails unless the run used `--update-expected` (the entry is written and the test is marked `blessed` in the report) or the
owner confirmed `functest.bless`. A file this build cannot read, including a newer `format_version`, aborts the run and is left
unchanged. Version **1**; shape hashed by `cargo xtask check-formats`
(name `expected-values`).
