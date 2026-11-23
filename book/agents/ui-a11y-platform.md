# Screen-reader OS bridge (AccessKit)

With the `accesskit` feature, the accessibility tree from [ui.a11y_tree](ui-a11y.md) reaches the platform's
assistive technology through AccessKit: AT-SPI on Linux (Orca), UI Automation on Windows (Narrator, NVDA),
NSAccessibility on macOS (VoiceOver) and TalkBack on Android (ADR-3454). The bridge is off by default
(ADR-1998).

```toml
regia = { workspace = true, features = ["accesskit"] }
```

The three methods below exist on every build. Without the feature they answer `platform_bridge_off`.

## How the bridge works

- **Idle until a screen reader connects.** No tree is built or sent while no assistive technology is connected
  (and exporting is off). When one connects, the next frame sends the whole tree.
- **Incremental after that.** Each frame sends only the nodes that changed. Structural changes always go: new
  nodes, nodes whose children changed, the dialog that is modal, announcements, including when that count
  is past the budget. Property-only changes (a label, a value, a state, bounds) fill the slots left after
  those structural nodes: at most `256 - structural_count` that frame, and the rest go in later frames.
- **Stable ids.** A platform node's id is the id `ui.a11y_tree` reports (the entity bits). The window root is
  `0`. The two announcers (polite, assertive) are `4294967296` and `8589934592`. No entity can have these ids
  (an entity id's low 32 bits are never 0).
- **Requests come back through the UI's request path.** A screen reader's click, focus, increment, set value or
  close runs like `ui.a11y_act`: it is refused for disabled, inert and unknown nodes, and the outcome shows
  under `requests` in `ui.a11y_tree`.

| Engine | AccessKit |
|---|---|
| role `window` (a `UiRoot`) | `Pane` under the synthetic `Window` root |
| other roles | the same role (`progress_bar` is `ProgressIndicator`) |
| name | label |
| description, hint | description (`description. hint`) |
| value | value; sliders and progress bars also have `numeric_value`, `min`, `max`, `step` |
| bounds (x, y, width, height) | bounds (x0, y0, x1, y1), physical px in the window |
| `disabled`, `selected`, `invalid`, `checked`, `expanded`, `read_only` | the same states (`checked` is `toggled`) |
| the open dialog of the top input layer | `modal`, with custom action `1` "Close" |
| live region politeness | `live` |
| announcements | the label of the polite or assertive announcer |

| AccessKit action | Engine request | Offered by |
|---|---|---|
| `click` | `press` | buttons, check boxes, combo boxes, other focusable nodes |
| `focus` | `focus` | focusable nodes |
| `increment`, `decrement` | `raise`, `lower` | sliders |
| `set_value` (text), `replace_selected_text` | `set_text` | editable text fields. A numeric `set_value` is accepted and queued, then left unmapped (`maps_to` null); the text stays unchanged. Pass a string to set it. |
| `custom_action` 1 | `close` | dialogs |
| `scroll_into_view` | `scroll_into_view` | every node under the root |
| `scroll_up`, `scroll_down`, `scroll_left`, `scroll_right` | `page_up` ... `page_right` | scroll views, lists |

A disabled or inert node offers no actions. Other AccessKit actions (tooltips, text selection, ...) are counted
and dropped.

## Methods

- `ui.a11y_platform_tree {}` (permission `read`) returns the tree the OS adapter holds, depth first from the
  window root. Each node has `id`, `parent`, `children`, `role` (the AccessKit role name), `label`,
  `description`, `value`, `numeric_value`, `min`, `max`, `step`, `bounds`, `actions`, `custom_actions`,
  `toggled`, `expanded`, `selected`, `disabled`, `read_only`, `modal`, `invalid` and `live`. The result also has
  `adapter` (`atspi`, `uia`, `ns_accessibility`, `android`, or `null` with no window), `adapter_error`,
  `android` (`os`, `registered`, `attached`, `status`, `detail`), `active`,
  `needs_full_tree`, `root`, `focus`, `last_error` and `stats` (`updates`, `full_updates`, `nodes_sent`,
  `delivered`, `superseded`, `invalid_updates`, `resyncs`, `actions`, `actions_dropped`, `activations`,
  `deactivations`). `resyncs` counts the times a screen reader connected while a frame's updates were on their
  way to the adapter, so the whole tree went instead.
- `ui.a11y_platform_connect { connected }` (permission `play`) simulates a screen reader connecting (`true`: the
  next frame sends the whole tree) or leaving (`false`). Use it to check the bridge with no screen reader.
  The result includes the same `android` object as the tree.
- `ui.a11y_platform_action { node, action, value? }` (permission `play`) queues an AccessKit action request as the
  OS adapter would. The node must offer the action (its `actions`, or for `custom_action` its `custom_actions`).
  `value` is text or a number for `set_value`, text for `replace_selected_text`, and the action id for
  `custom_action`. Text `set_value` and `replace_selected_text` map to `set_text`. A number for `set_value`
  is queued and left unmapped (`maps_to` is null, and the field's text does not change); pass a string to
  set the text. The request runs in the next frame. The result is `{ queued, node, action, maps_to, note }`,
  where `maps_to` is the engine request (`press`, `raise`, ...) or `null`.

```json
{"method": "ui.a11y_platform_connect", "params": {"connected": true}}
{"method": "ui.a11y_platform_tree", "params": {}}
{"method": "ui.a11y_platform_action", "params": {"node": 4294967300, "action": "increment"}}
{"method": "ui.a11y_platform_action", "params": {"node": 4294967310, "action": "set_value", "value": "Ada"}}
{"method": "ui.a11y_platform_action", "params": {"node": 4294967320, "action": "custom_action", "value": 1}}
```

A typical check: connect, wait one frame, read the platform tree and check that every control has a `label`
and the actions you expect. Then act, wait one frame, and read `ui.a11y_tree` `requests` and the platform tree
again.

## Error codes

| Code | When |
|---|---|
| `platform_bridge_off` | the build has no `accesskit` feature, or the app installed another screen-reader backend |
| `bad_connected` | `ui.a11y_platform_connect` without a boolean `connected` |
| `a11y_node_missing` | `ui.a11y_platform_action` without `node` |
| `a11y_action_missing` | `ui.a11y_platform_action` without `action` |
| `a11y_value_missing` | `set_value`, `replace_selected_text` or `custom_action` without a usable `value` |
| `bad_entity` | `node` is not a non-negative integer |
| `bad_action` | `action` is not a string, or not an AccessKit action name |
| `bad_value` | `value` is neither a string nor a finite number |
| `text_too_long` | a text `value` over 4096 characters |
| `unknown_node` | the node is not in the platform tree (the tree is empty until a screen reader connects) |
| `unsupported_action` | the node does not offer that action |

## Environment

- `REGIA_ACCESSKIT=0` keeps the OS adapter off. The bridge and its platform tree still work, and the agent
  reports `adapter_error`.
- `REGIA_SCREEN_READER=1` marks the bridge as connected at start-up (headless runs).

## Known limits

- Android (ADR-3454): TalkBack activates the bridge and the next update is a full tree. There is no
  callback when TalkBack turns off, so the bridge stays active until the process ends or
  `ui.a11y_platform_connect` is `false`. The adapter checks `AccessibilityManager.isEnabled()` before each
  update and holds updates while it is off, then sends the whole tree once it is back on (ADR-4932). The delegate class is loaded from the crate (`embedded-dex`);
  the activity shim is unchanged. `set_window_focused` does nothing on Android. One adapter per
  process, on the primary window's surface view. A JNI failure is logged and the bridge is disabled.
  A shipping build (`panic = "abort"`) still ends if AccessKit unwraps after that preflight.
- Steam Deck game mode has no assistive-technology path (no screen reader in game mode). Desktop mode
  on the Deck is Linux (AT-SPI).
- On X11 the window position is not sent to AT-SPI, so Orca's flat review may place nodes at the window origin.
- The Linux adapter's D-Bus thread panics if the accessibility bus fails in a way it does not expect. Shipping
  builds use `panic = "abort"`, so that panic ends the game. This is one reason the feature is off by default;
  `REGIA_ACCESSKIT=0` is the way out.
- With the feature on, the Windows and macOS window is created hidden and shown right after the adapter
  attaches (the Windows adapter must subclass the window before it is first shown).

## v0.5 C10/C12 acceptance checkpoint (2026-11-08)

Candidate `75e1a7794` is blocked at prerequisite inspection. The blur/axis work
(`cef03dc3e`) is integrated, but the UI accessibility fixes (`c114b1bc7`,
`lane/sprint-ui-a11y-lint`) are not. Source comparison confirms that the
candidate lacks that lane's explicit accessible labels on the invert toggles
and dead-zone slider and its reduced-motion scroll handling. The Android
TalkBack adapter (`d53c28aa6`, `lane/sprint-a11y-android-reader`) is also absent.
Finish and integrate the existing work before replaying acceptance; do not
implement another copy of those repairs.

No bridge tests or OS-reader sessions ran in this checkpoint. A recording
adapter, `ui.a11y_platform_connect`, or `ui.a11y_platform_action` proves the
bridge path only. None proves that a real reader announces or activates the
runtime controls.

On the resumed candidate, record the exact commit, OS and named reader/version
for Orca/Linux and Narrator or NVDA/Windows, plus VoiceOver/macOS if available.
Record announcements, focus order, action results, and modal entry/exit. Keep
each unavailable session explicitly deferred. Exercise keyboard and physical
pad navigation and persist an axis remap with inversion/dead-zone tuning
through restart. On TEST Pixel 9 Pro `54101FDAP0015H`, exercise actual touch,
text scale, localization and safe-area flow; restore settings and remove test
packages. Check that the blurred modal preserves focus and that warm target
counts stay stable. Keep deterministic protocol/test evidence separate from
these OS/device observations.

The lane handoff is
`<workspace>`.
It lists the exact checks still required. C10/C12 remain thin; this checkpoint
does not change either row to verified.

## v0.5 C10/C12 acceptance checkpoint (2026-11-09, lane v05-a11y-runtime-b)

The prerequisites above are absorbed on `lane/sprint-v05-a11y-runtime-b`. The controls screen (a `RebindPanel` with
every option, inside a frosted panel) is covered through the AccessKit bridge by the `controls_screen` module of
`crates/regia_ui/tests/accesskit_bridge.rs` (`--features accesskit`). It checks names and descriptions, Tab, D-pad and touch reach,
a blur-neutral tree and Tab order, restart persistence of invert and dead zone, a rebind capture on a pause dialog,
and a phone flow: safe area, 200 % text, reduced motion, right-to-left, and the touch audit. These tests use a
recording adapter. They are not screen-reader results.

Still deferred, with the reason:

- Orca on Linux: no host with Orca and a desktop session is open to lanes. the workstation is the owner's desktop.
- Narrator or NVDA on Windows: on the N150 laptop, programs started over ssh cannot see the desktop. A GUI run
  needs an interactive scheduled task, and nothing remote can hear what Narrator says. A reader session needs
  someone at the machine.
- VoiceOver on macOS: ssh cannot capture the screen and gets no AppleEvents permission there. VoiceOver scripting
  needs a root-owned flag.
- TalkBack, touch and scale on the TEST Pixel 9 Pro `54101FDAP0015H`: the phone was not connected during the lane.

To run a reader session, open the First Person template's controls screen (F10) in a build with
`--features regia/accesskit`. Tab through one axis row and record what the reader says. Toggle invert, step the
dead zone, restart the game, and confirm the values come back. Then start a capture, press Escape, and confirm the
dialog stays open.

## Android device pass (2026-11-09, lane v05-a11y-android-talkback, ADR-4932)

Run on the TEST Pixel 9 Pro (`54101FDAP0015H`, Android 17, landscape 2142x960 px at OS scale 2.25). The build was
a debug APK of a project made from the First Person template, with `regia` `features = ["accesskit"]`.

- Turning TalkBack on (`settings put secure enabled_accessibility_services ...`) activated the bridge:
  `android.status` `attached`, `activations` 1, one full update of 82 nodes, no invalid updates. Later focus
  changes went out as small updates.
- `uiautomator dump` lists the screen's nodes as Android sees them. Buttons are `android.widget.Button`, toggles
  are `CheckBox` with `checkable` set, and the dead zone is a `SeekBar` with its value. Bounds are in physical px.
  Clicks are an `ACTION_CLICK` in the node's action list (`clickable` reads false; TalkBack uses the action).
- `accesskit_android` 0.7.5 gives TalkBack the label only. The Android adapter therefore folds each node's
  description into its label (`regia_platform::accessibility::fold_descriptions_into_labels`): `Invert Y` with
  `Move, gamepad` reaches TalkBack as `Invert Y, Move, gamepad`. `ui.a11y_platform_tree` still shows both fields,
  because it is the tree the UI sent, before the Android fold.
- An update sent after an accessibility client went away aborted the game: Android refused the event, and
  `accesskit_android` 0.7.5 unwraps that refusal on the UI thread. The adapter now holds updates while
  accessibility is off (`android_update_gate`) and resyncs with the whole tree after
  (`AccessKitBridge::request_resync`). On the fixed build, a drag right after `uiautomator` disconnected left
  the game running.
- Back is named `BrowserBack` (it had no winit key code), and leaves a rebind capture instead of being bound.
- TalkBack's own gestures cannot be driven from adb: events from `adb shell input` bypass TalkBack's touch
  explorer (a tap on a launcher icon opened the app instead of focusing it). Explore by touch, swipe navigation
  and the double tap therefore need a person with the phone. The report of the lane lists the steps.

## Android display size and reader cost (2026-11-10, lane v05-a11y-density, ADR-5379)

Run on the TEST Pixel 9 Pro (`54101FDAP0015H`). The build was a First Person template project with
`regia` `features = ["accesskit"]`. Its native code used the release profile's codegen in a debuggable APK, so
the agent could attach.

- **Display size.** The runner re-reads the density (`Configuration.densityDpi / 160`) after every resume,
  when the window gains focus, and on every configuration change. It writes `WindowScaleFactorChanged` when the
  density moved. With the controls screen open, `adb shell wm density 480` moved `ui.state` `scale.scale_factor`
  from 2.25 to 3.0 in the same process, and the Look button grew from 360x84 to 480x113 px.
  `ui.a11y_audit {"profile":"touch"}` reported 0 non-info findings at both densities. `wm density reset` went
  back to 2.25. Before the change, winit's scale stayed at the density the app started with.
- **Reader cost.** `ui.a11y_tree` `sync_cost` gives the cost of each live frame's sync over its last 512 frames,
  split into `build`, `diff` and `backend`. TalkBack was on before the game started, the game had focus, and the
  controls screen was open (81 nodes, 168 UI entities). Over four windows the mean was 0.27 to 0.58 ms and the
  p99 0.54 to 0.86 ms. The tree build was about 80 % of it. The HUD alone (7 nodes) had a mean of 0.12 ms and a
  p99 of 0.29 ms.
- **Measuring.** Turn TalkBack on before you start the game. When TalkBack is turned on, it first shows a
  permission prompt over the game. That pauses the game: it runs at the hidden rate (about 10 frames a second, no
  presents) and the numbers are higher (mean 1.38 ms, p99 2.32 ms). Keep the screen awake
  (`svc power stayon usb`, then restore it). Agent calls do not count as user activity, and the app stops
  answering once the screen times out and suspends it.
