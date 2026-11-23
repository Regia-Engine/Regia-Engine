# `assets/xr/actions.xr.ron`: the XR action manifest

Owner: `regia_xr::input` (spec 58, ADR-1676). Current `format_version`: **1**. Declares the action sets, the actions in
them and the suggested bindings per interaction profile. The manifest is frozen when the session attaches its action
sets (the attach-once rule): after that, declarations cannot change until the session is re-created. Unknown fields are
a load error (`deny_unknown_fields`); a newer `format_version` is refused (`FormatTooNew`). Registered in
`cargo xtask check-formats` (`xr-actions`) and `regia upgrade-project` since ADR-2316.

## Shape

```ron
(format_version: 1,
 sets: [(name: "gameplay", priority: 0, actions: [
   (name: "move", ty: Vec2, label: "xr-action-move", subs: ["left"]),
   (name: "grab", ty: Float, label: "xr-action-grab", subs: ["left", "right"]),
   (name: "grip_pose", ty: Pose, subs: ["left", "right"]),
   (name: "buzz", ty: Haptic, subs: ["left", "right"])])],
 bindings: [(profile: "simple_controller", map: [("grab", "left", "select/click")])])
```

| Field | Type | Meaning |
|---|---|---|
| `format_version` | u32 | Must be 1. |
| `sets` | list of sets | `name` (`[a-z][a-z0-9_]*`, unique), `priority` (u32), `actions`. |
| action `name` | string | `[a-z][a-z0-9_]*`, unique within its set. |
| action `ty` | `Bool`, `Float`, `Vec2`, `Pose`, `Haptic` | The value type. |
| action `label` | string, default empty | Localization key for the rebinding UI (empty: the name). |
| action `subs` | list, default empty | Sub-action paths, each `left` or `right`; empty means the action is not per hand. |
| `bindings` | list of profiles | `profile` (a standard interaction-profile path or a short name such as `simple_controller`), `optional` (bool, default false: vendor profiles are suggested only when the runtime advertises their extension), `map`. |
| binding | triple `(action, sub, path)` | The action name, its sub-path (empty for an action without subs) and the input path below `/user/hand/<sub>/input/`. |

## Validation

- Every name matches `[a-z][a-z0-9_]*`; duplicates (sets, or actions within a set, or sub-paths) are rejected naming the offender.
- A sub-path other than `left` or `right` is rejected.
- A binding naming an unknown action is `xr_action_unknown`; a binding naming a sub-path the action does not declare is rejected.

The default manifest (`XrActionManifest::default()`) is valid and is what a project without the file gets. The
`comfort.dominant_hand` setting derives a manifest with the dominant-hand actions swapped
(`with_dominant_hand`). Agent surface: `book/agents/xr.md`.
