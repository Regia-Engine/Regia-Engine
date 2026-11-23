# Radial menu (weapon wheel, unit wheel)

`regia_ui::RadialMenu` is a reusable wheel for gamepad stick, mouse and touch: two to twelve sectors, an
optional centre action, an icon and a label per sector, hold-to-open or toggle, release-to-select or
press-to-select, and an open/close animation. The `ui_gallery` sample has a "Wheel" page (key `4`).

```rust,wrap
use regia_ui::prelude::*;
# use regia_ecs::prelude::{Commands, Entity, MessageReader, Single, With};
use regia_ui::{RadialConfirm, RadialEvent, RadialEventKind, RadialOpen, spawn_radial_menu};

fn spawn_weapon_wheel(mut commands: Commands, hud: Single<Entity, With<UiRoot>>) {
    let menu = RadialMenu::new(vec![
        RadialSector::new("rifle", "Rifle"),
        RadialSector::new("pistol", "Pistol"),
        RadialSector::new("grenade", "Grenade").enabled(false),
        RadialSector::new("knife", "Knife"),
    ])
    .with_center(RadialSector::new("holster", "Holster"))
    .with_open(RadialOpen::Hold)
    .with_confirm(RadialConfirm::Release)
    .with_open_action("WeaponWheel"); // a VirtualButtons action; or call menu.set_held(bool) yourself
    spawn_radial_menu(&mut commands, *hud, menu); // centred in the parent, starts closed
}

fn on_wheel(mut events: MessageReader<RadialEvent>) {
    for e in events.read() {
        if let RadialEventKind::Select { id, .. } = &e.kind {
            let _selected_id = id;
        }
    }
}
```

## Input

| Source | Selects with | Confirms with |
|---|---|---|
| Stick (right by default, `stick`) | the stick angle; magnitude below `dead_zone` (0.35) selects nothing, or the centre action | releasing the open control (hold), `A` (`UiNavActions::submit`) |
| Mouse | the cursor angle around the wheel's centre once the cursor moves; inside the hub (`inner_radius`) selects the centre action | button release (`Release`) or press (`Press`) |
| Touch | the finger's drag from where it landed (`touch_range` outer radii is full deflection) | lifting the finger (`Release`) or touching down (`Press`) |
| D-pad, shoulders, Tab | steps to the next enabled sector | `A` |

`B` / `Escape` cancels. While a wheel is open it takes the navigation actions (stick, d-pad, `A`, `B`), so the
focus ring does not move behind it, and it marks the pointer as consumed.

Hold mode opens while the open control is held and, under `Release`, selects the highlight when it is
released (releasing with nothing highlighted cancels). Under `Press` releasing cancels and a click or tap on a
sector selects. Toggle mode opens on the control's press and closes on the next press (a cancel).

## Selection feel

The math is `RadialGeometry`, pure and tested for 2 to 12 sectors: sector 0 is centred on `start_angle_deg`
(up by default), sectors run clockwise, and the angle wraps. A highlight only moves once the pointer is
`hysteresis` (0.12) of a sector past the boundary, so a stick resting on a boundary does not flicker; a
highlighted sector also survives the stick dipping to 80 % of the dead zone.

## Look

Each sector is a rounded chip placed on the ring (no arc-shaped quad, so no renderer change). Colours come from
the active theme (`surface_hi`, `primary`, `on_primary`, `text`, `text_dim`, `overlay`); `RadialStyle`
overrides any of them. Open and close ease over `animation_secs` (0.12; 0 is instant) and are driven by frame
time, so they run the same at 30 and 60 fps. Disabled sectors dim and cannot be selected.

## Events

`RadialEvent { target, kind }` messages: `Opened`, `Hover(selection)` (also `None` when the highlight leaves),
`Select { selection, id }`, `Cancel` and `Closed` (after `Select` and `Cancel`). `RadialMenu::selection`,
`open`, `progress` and `phase()` expose the live state.

Agents: see [Radial menu (agent guide)](../agents/radial-menu.md).
