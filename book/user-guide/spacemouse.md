# SpaceMouse

Regia Engine's desktop apps (the editor, Tincal, Red-Rouge and any app built on Regia Engine's UI) support
3Dconnexion SpaceMouse devices for 3D viewport navigation and for 2D panes (image views,
timelines, lists). They all use the same defaults and the same navigation function, so the device
feels the same in every app. Plug it in at any time; nothing else is needed on Windows. On Linux,
Regia Engine uses the `spacenavd` daemon when it is running (no setup, and the device stays shared
with other apps such as Blender); otherwise install the udev rule below once.

The SpaceMouse moves a viewport only while the pointer is over it (or it has keyboard focus) and
no text field or dialog has focus. Games do not read it unless they choose to.

## Linux: spacenavd or the udev rule

If the `spacenavd` daemon is running (its socket is `/run/spnav.sock` or `/var/run/spnav.sock`),
Regia Engine reads the device through it and needs no permissions; the device shows up the first time
you touch it. Otherwise Regia Engine reads the device through the kernel's `hidraw` interface. By default only root can read
it. Save this as `/etc/udev/rules.d/70-spacemouse.rules`:

```text
# /etc/udev/rules.d/70-spacemouse.rules: read access to SpaceMouse HID nodes for the logged-in user
KERNEL=="hidraw*", ATTRS{idVendor}=="256f", MODE="0660", TAG+="uaccess"
KERNEL=="hidraw*", ATTRS{idVendor}=="046d", ATTRS{idProduct}=="c6[0-2][0-9a-f]", MODE="0660", TAG+="uaccess"
```

Then reload the rules and replug the device:

```sh
sudo udevadm control --reload-rules && sudo udevadm trigger
```

The first line covers every 3Dconnexion device; the second covers older units sold under the
Logitech vendor id. Without the rule, the app logs one warning with this text and carries on
without the device.

Regia Engine never touches the device's LEDs or settings. If 3Dconnexion's own driver holds the
device so that it cannot be opened, the app shows nothing and keeps working.

## Defaults

| What | Default |
|---|---|
| Push / pull / lift the cap | Orbit viewports: pan with the cap (push right, the view moves right; lift, it moves up); push the cap away from you to zoom in, pull it to zoom out |
| Tilt / twist the cap | Orbit around the target: twist counter-clockwise turns the view counter-clockwise; tilt away to look down from higher |
| Pan speed | 1 orbit distance per second at full deflection |
| Zoom speed | the distance changes by a factor `e^1.5` (about 4.5) per second at full deflection |
| Orbit speed | 1.5 radians (86°) per second at full deflection |
| Fly cameras | 1 scene size per second, turning at 1.5 radians per second |
| Dead zone | 4 % of the travel, then a quadratic curve: fine control near the centre, full speed at the stops |
| Horizon lock | on: no roll, pitch stops at ±89° |
| Dominant axis | off: all axes move at once |
| Turntable views (review lightboxes) | The object spins instead of the camera: the same speeds, pitch limit and horizon lock |
| 2D views (images, video, crop windows) | Push / lift the cap pans 1 visible width per second; push away zooms in, pull zooms out |
| Timelines | Twist the cap: counter-clockwise moves the playhead forward, 1 visible span per second at full twist; push / pull zooms |
| Lists (scene cards, cuts) | Tilt the cap right for the next item, left for the previous one, past about 70 % of the travel; held, it repeats every 0.35 s |
| Button 1 (left) | Fit the selection (everything when nothing is selected) |
| Button 2 (right) | Home view |
| Button 3 | Toggle dominant-axis mode (only the strongest axis moves) |
| Button 4 | Toggle horizon lock |
| Buttons 5 and up | Unassigned |

## Your settings

Each app has three SpaceMouse settings, in the `spacemouse` section of its input settings
(`config/input.ron`):

```ron
(
    spacemouse: (
        enabled: true,      // false: the app ignores SpaceMouse devices
        sensitivity: 1.0,   // speed multiplier, 0.1 to 4.0
        invert: (false, false, false, false, false, false), // x, y, z, rx, ry, rz
    ),
)
```

Everything else (dead zone, curve, speeds, button actions) is fixed so all apps stay the same.

## Object tool

Some apps offer an **Object tool** (off by default, switched by the `spacemouse.object_tool_toggle`
action, no device button): while it is on, the cap moves or rotates the selected object, part or
pivot instead of the camera, and the camera stays put. Each gesture, from the first touch until the
cap has rested for 0.3 s, is one undo step.

## For developers

Read `regia_input::spacemouse::SpaceMouseState` and call `nav::step` with a `NavContext`
for your camera or pane (`NavMode` Orbit, Fly, Turntable, View2D, Timeline or Step); apply the
returned `NavDelta` (pan, orbit, roll, zoom, translate, scrub, step). Step mode keeps its repeat
timer in the delta: pass `NavDelta::step_held` and `NavDelta::step_dir` back (`NavContext::list_dir`) next frame; a tilt flipped to the other side then steps at once. For the
Object tool, `ObjectGesture` turns frames into one undo entry per gesture. Handle the
`SpaceMouseAction` messages `view.fit_selection` and `view.home` with your existing fit and home
code. The editor's viewport is the reference integration. Agents can drive and inspect the device
with `input.spacemouse_inject` and `input.spacemouse_status` ([agent guide](../agents/spacemouse.md)).
