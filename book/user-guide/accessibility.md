# Accessibility

Regia Engine's runtime UI builds an accessibility tree from your widgets, and with the `accesskit` feature it
sends that tree to the player's screen reader. You get a tree without extra work: buttons, check boxes, sliders,
text fields, dialogs and lists get their role, name, value and states from the widget components. You add an
`Accessible` component where the inferred result is wrong or missing.

## What you get for free

- **Roles and names.** A `Button` with a `Text` child is a button named by that text. A `Slider` is a slider
  with its number, range and step. A `TextInput` with a placeholder and no label is named by the placeholder.
  Unnamed plain containers are flattened, so the tree reads like the screen.
- **States.** Focus, disabled, selected, invalid, checked, expanded (combo boxes, dialogs) and read-only text
  fields.
- **Dialogs.** An open `Modal` is the top input layer. Everything under it is inert: a screen reader is told the
  dialog is modal and the controls behind it offer no actions.
- **Requests.** A screen reader can press, focus, step a slider, set a text field's text, close a dialog and
  scroll. These go through the same path as a key press or a click, so a disabled control refuses them.

## Turning the screen-reader bridge on

```toml
[dependencies]
regia = { workspace = true, features = ["accesskit"] }
```

With the feature, the bridge talks to AT-SPI on Linux (Orca), UI Automation on Windows (Narrator, NVDA),
NSAccessibility on macOS (VoiceOver) and TalkBack on Android. It costs nothing until a screen reader connects.
After that it sends only what changed each frame, and spreads a very large change over several frames (256
changed nodes per frame), so a frame never stalls. On Android the adapter injects into the game's surface
view; the Java class travels inside the crate (`embedded-dex`), so the activity shim is unchanged. TalkBack
connecting asks for a full tree on the next update.

`REGIA_ACCESSKIT=0` turns the OS side off without a rebuild.

## Fixing names and roles: `Accessible`

```rust
# use regia::prelude::*;
use regia::ui::a11y::{A11yRole, Accessible, Politeness};

# fn spawn_menu(mut commands: Commands) {
// An icon button: no text, so give it a name (a localization key wins when defined).
commands.spawn((UiNode, Button, Focusable::default(), Accessible::default().with_label_key("menu-settings")));

// A score readout the screen reader announces when it changes.
commands.spawn((
    UiNode,
    Text::new("0"),
    Accessible::default().with_role(A11yRole::Status).with_label("Score").with_live(Politeness::Polite),
));

// Decoration: leave it out of the tree.
commands.spawn((UiNode, UiImage::default(), Accessible::decorative()));
# }
```

To say something once (a save finished, a match starts), call
`ScreenReader::announce("Game saved", Politeness::Polite)`. Use `Politeness::Assertive` only for messages that
must interrupt.

## Checklist

1. Every control has a name. Read `ui.a11y_tree` and look for empty `name` on buttons, check boxes, sliders and
   text fields.
2. Icon-only buttons have a label or label key.
3. Decorative images are `Accessible::decorative()`.
4. Status text that changes during play (score, timers, objectives) is a live region, `Polite` unless urgent.
5. Dialogs are `Modal`s, so what is behind them is inert.
6. With the bridge on, connect a screen reader (or `ui.a11y_platform_connect`) and walk the menus with the
   keyboard and the reader's own navigation.

## Auditing a screen

A theme can pass its contrast lint and still ship a screen with white text on a light picture, a 12 px close box,
a button with no name, or a label cut off at double text size. The audit checks the screen you actually built, from
its computed layout, its accessibility tree and its resolved theme. It adds nothing to the frame, so it costs
nothing until you call it, and it leaves the UI as it was.

From an agent:

```json
{"method": "ui.a11y_audit", "params": {}}
```

From a test or a debug key:

```rust
# use regia::prelude::*;
use regia::ui::a11y_audit::{AuditOptions, AuditProfile, audit_ui};

# fn print_audit(world: &mut World) {
let report = audit_ui(world, &AuditOptions {
    profile: AuditProfile::Touch, // on phones and tablets
    ..Default::default()
});
for finding in &report.findings {
    println!("{} {}: {} (needs {})", finding.rule.code(), finding.path, finding.measured, finding.required);
}
# }
```

What it checks:

- Every control has a name.
- Text contrast against what is really behind the text: 4.5:1, or 3:1 for large text.
- The focus ring and control boundaries: 3:1.
- Targets are at least 24 UI px per side, or the theme's `size.touch-min` with the touch profile.
- Navigation reaches every control, and each control shows a focus indicator.
- No text is lost at 200 % text size.
- Nothing keeps animating with reduced motion on.

Read the results this way:

- **`error`** breaks the rule. Fix it.
- **`warning`** is probably wrong, or wrong in a way that loses nothing.
- **`info`** means the audit cannot tell. Text over an image or over the running game gets it, because the audit
  does not read pixels. Look at those by eye, on the real background. They are not passes.

Common fixes:

- A control whose label sits beside it needs its own name: `Accessible::default().with_label("Volume")`. Only a
  label inside the control names it.
- Give text fields and combo boxes the `Panel.field` or `Button.field` class. The default `surface` and `button` edges
  are under 3:1, which is fine for a button because its text identifies it.
- Keep a page that may not fit inside a scroll view that can shrink: `flex_shrink: 1.0`, a `min_size` height of 0, and
  clipped overflow. Flex items do not shrink by default, so a tall page otherwise runs off the window.
- Theme text color reaches labels through plain containers. A node with its own `Background` stops it, so choose
  that text's color yourself.

A target that is small on purpose can say why with the `TargetExemption` component (`Equivalent`, `Inline`,
`UserAgentControl`, `Essential`). The audit lists every exception it used, so a reviewer can see what was excused.

To keep a screen clean, compare `finding.key()` against a checked-in list in a test, as
`samples/ui_gallery/tests/a11y_audit.rs` does. Keys hold no entity ids, so the list stays valid between runs.

A clean audit is not a screen-reader test: still run your menus with a real screen reader, and read
[the rule list](../agents/ui-a11y-audit.md) for what each rule measures.

## Checking it with an agent

- [UI accessibility tree and screen readers](../agents/ui-a11y.md): `ui.a11y_tree`, `ui.a11y_announce`,
  `ui.a11y_act`.
- [Screen-reader OS bridge](../agents/ui-a11y-platform.md): `ui.a11y_platform_tree`, `ui.a11y_platform_connect`,
  `ui.a11y_platform_action` (the tree as the OS sees it).
- [UI accessibility audit](../agents/ui-a11y-audit.md): `ui.a11y_audit` (defects on the screen shown now).

## Limits

- Steam Deck game mode has no screen-reader path (desktop mode on the Deck is Linux, so AT-SPI). Android
  TalkBack is ADR-3454: there is no callback when TalkBack turns off, so the bridge stays active until the
  process ends or `ui.a11y_platform_connect` is sent `false`. A shipping build (`panic = "abort"`) still
  ends if AccessKit's own JNI unwrap fires after the preflight.
- The bridge is off by default. The Linux adapter's D-Bus thread can panic if the accessibility bus fails in an
  unexpected way, which ends a shipping (`panic = "abort"`) build. Test with your target desktops before you
  ship with it.

## Player assists: input, audio and color vision

Four more settings in `GameSettings.accessibility` (ADR-2646). They persist with the other settings and apply
live; your options menu only has to write them.

```rust,wrap
use regia::framework::settings::GameSettings;
use regia_input::{ActionAssist, PressMode, RepeatRate};
use regia::render::color_vision::{ColorVisionDeficiency, ColorVisionFilter, ColorVisionMode};
# use regia_ecs::prelude::ResMut;

fn apply_menu_choices(mut settings: ResMut<GameSettings>) {
    let a = &mut settings.accessibility;
    // Crouch latches on one press and off on the next; Fire repeats while held.
    a.input_assists.insert("Crouch".into(), ActionAssist { mode: PressMode::Toggle, ..Default::default() });
    a.input_assists.insert("Fire".into(), ActionAssist { repeat: Some(RepeatRate::DEFAULT), ..Default::default() });
    // The whole output in both ears.
    a.mono_audio = true;
    // Shift red/green confusions into colors a deuteranope can tell apart.
    a.color_vision = ColorVisionFilter::new(ColorVisionMode::Compensate, ColorVisionDeficiency::Deuteranopia);
}
```

- **Input assists** (per action): hold or toggle, a minimum hold time that drops short accidental presses,
  auto-repeat while held, and a cool-down (`cooldown_steps`, a minimum number of evaluations between accepted
  presses). The default set is `input_assists`. `input_assists_by_player` and `input_assists_by_gamepad` overlay
  it for one local player or one pad. Your triggers still run as authored, on the assisted input. Call
  `Actions::release_toggle` when the game must drop a latched action (a cutscene, a death).
  [Input assists](../agents/input-assists.md).
- **Rumble and phone vibration**: `a.rumble_scale` and `a.vibration_scale` (`0` off to `1`) scale gamepad rumble
  and `PhoneVibration::vibrate`. Reduced motion does not change either.
  [Input assists](../agents/input-assists.md).
- **Scene motion**: `a.camera_shake_scale` (`0` to `1`) scales authored camera shake without rewriting the
  camera. `a.motion_blur` is `None` (leave the render profile), `Some(false)` (force off) or `Some(true)` (allow
  it; the velocity target is still required). `a.fov_override_deg` replaces the gameplay camera's field of view
  in degrees (`None` keeps the camera's value; shots and the sequencer keep theirs). `a.reduced_motion` also
  sets the effective shake scale and the head-bob scale to `0` and forces motion blur off. Framework effects
  that animate on their own: camera shake, handheld sway and breathing, and the walk bob all honor it. Field-of-view
  kicks stay as authored unless the override replaces the gameplay camera's fov.
  [Settings](../agents/framework.md).
- **Look**: `GameSettings.input.sensitivity` and `invert_y` scale the actions named in `InputConfig.look_actions`.
  With that list empty the game reads the two fields itself.
  [Input assists](../agents/input-assists.md).
- **Mono audio**: the output is downmixed to mono before the master limiter. [Mono audio](../agents/audio-mono.md).
- **Color vision**: `Simulate` shows the game as a player with protanopia, deuteranopia or tritanopia sees it (use
  it while designing); `Compensate` is the player option. It works through the view's color LUT, after your grade,
  and does not filter the UI. [Color-vision filters](../agents/color-vision.md).

What the engine covers, and what is still the game's job, is listed against public accessibility guidelines in
`docs/accessibility-review.md`.

## Photosensitivity check

Before a release, run the flash check on any sequence that strobes on purpose: a trailer, a
render-out, a muzzle flash, an explosion, lightning. It counts general flashes and saturated-red
flashes the way WCAG 2.2 success criterion 2.3.1 describes them, and it fails a sequence with more
than three flashes in any one second when that flash covers more than a quarter of a 10 degree
field.

On a folder of PNG frames (file-name order, one directory, no subfolders):

```text
cargo xtask flash-check captures/trailer --fps 30
cargo xtask flash-check captures/trailer --fps 30 --json
```

`--fps` is the rate the frames were captured at. Exit 0 means the sequence passed, exit 2 means it
failed the thresholds, and exit 1 means the folder or a frame could not be read. `--json` prints
the same report the agent returns: `passed`, `max_flashes`, `window_start_s`, `region`,
`area_fraction` and `kind` (`general` or `red`).

From a running game, `seeing.flash_check` analyzes a folder the same way, or captures the window
now (`frames` from 2 to 600, `fps` the analysis rate). The call and its error codes are on the
[seeing](../agents/seeing.md#photosensitivity-check) page.

The 10 degree field is 341×256 pixels on a 1024×768 screen at a typical viewing distance. Pass
`reference_width`, `reference_height`, `ten_degree_width_px` and `ten_degree_height_px` when the
player sits closer or the frame is a different size, and record that geometry with the result.
`cell_px` (default 8) trades exactness for speed. Use `1` when a small region matters.

This check is an aid. It is not a certification and it is not a guarantee that a sequence is safe
to watch. It does not apply the exception for a balanced pattern of squares smaller than 0.1
degree, and it does not limit flashes while the game is running. A failure is a reason to change
the effect or to warn the player, not a medical verdict.
