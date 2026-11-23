# The `FunctionalTest` component (v1)

Spec: `docs/specs/41-functional-tests-and-session-harness.md` § 2.2, § 4. Code: `regia_framework::functional::FunctionalTest`.
Decision: ADR-1673. Agent page: `book/agents/functional-tests.md`.

A test placed in a `.level.ron` as the reflected component `regia_framework::functional::FunctionalTest`:

```ron
"regia_framework::functional::FunctionalTest": (
    format_version: 1,
    enabled: true,
    description: "Door opens when the player is near",
    owner: "gameplay",
    tags: ["doors", "smoke"],
    frame_limit: 1800,                  // 0 = none; fixed-step frames from Begin, per pass
    ready_limit: 600,                   // frames the ready gate may take; 0 = none
    on_limit: Failed,                   // verdict applied when frame_limit runs out
    viewpoint: Some("<entity guid>"),   // optional: the local camera takes this entity's pose
    settings: [("render.quality", "low")],  // overridden for the run; values parse as bool, int, float, else text
    body: Script("tests/door_opens.functest.ron"),   // or Rust("my_game::DoorTest")
    reset_expected: false,              // clears this test's expected entries for this process, then clears the live flag; does not rewrite the level file
),
```

Version **1**. A level with the component loads in shipping builds but the component is ignored; the runner and
`functest.*` exist only in dev and test builds (the cook strips the entities from the `shipping` profile: follow-up).
The shape is hashed by `cargo xtask check-formats` (name `functional-test`).
