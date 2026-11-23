# `.functest.ron`: a data test body (v1)

Spec: `docs/specs/41-functional-tests-and-session-harness.md` § 2.3, § 4. Code: `regia_framework::functional::script` (`FuncTestScript`).
Decision: ADR-1673.

```ron
FuncTestScript(
    format_version: 1,
    steps: [
        (name: "walk to the door", do: [Inject(action: "Move", value: (0.0, 1.0), frames: 90)]),
        (name: "door opens", wait_until: Some((condition: (entity: (name: "Door"), component: "Door", path: "state", op: "eq", value: "Open"),
                                                 within_frames: 120))),
        (name: "player is through", expect: [(name: "x", condition: (resource: "R", path: "n", op: "gt", value: 1))]),
    ],
)
```

A step runs its `do` actions, waits for `wait_until` (failing the pass with the step name at `within_frames`), then evaluates every
`expect` in one frame. The `condition` value is the agent protocol's `play.expect` / `play.wait_until` grammar, carried unchanged.
**A change to that grammar needs this format's version considered** (ADR-1585: pin it, since a saved script is the point).
No loops or parallelism in scripts: use a Rust body.

Version **1**; a newer version is an error naming the file. Shape hashed by `cargo xtask check-formats` (name `functest-script`).
