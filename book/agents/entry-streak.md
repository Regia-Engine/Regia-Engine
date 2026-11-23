# Entry streak

`regia_vfx::entry` draws a body entering the atmosphere along a timed track: a bright head, an
ionised trail, a long-lived dust and smoke train behind it, shed pieces, and, below 20 km, a dark
body with no glow at all. It is an engine feature for any game; nothing in it is specific to one
scene. Everything is a pure function of the track, the profile and the time (`frame / 60`), so two
runs draw the same frame.

## What it models

- **Heating** scales with air density times speed cubed (`heating_flux`), against an exponential
  atmosphere. The glow saturates in that flux.
- **Gates.** The glow is exactly 0 at or below `GLOW_CEILING_M` (20 km), whatever the speed, and
  also below the ablation speed (2 km/s, fully open at 4 km/s). A body falling at terminal speed
  (150 to 250 m/s) never glows, even high up.
- **Colour** follows gas temperature: `head_temperature_k` gives about 3000 K below 3 km/s up to
  about 9000 K at 11 km/s, and the gas cools exponentially toward 1300 K (`cooled_temperature_k`,
  0.45 s). `temperature_color` is the full ramp (deep red, orange, yellow-white, white, blue-white, with
  sodium, magnesium and iron line tints); what the streak shows is `glow_color`, the same ramp held to
  `VISIBLE_MIN_K..=VISIBLE_MAX_K` (3500 to 6000 K). A fireball's light is mostly a 4000 to 5000 K vapour
  and a day-adapted eye takes about 6000 K as white, so the head reads warm white, never blue; cooling
  gas goes yellow, then orange, and then fades out instead of turning deep red (which reads pink, or as
  a laser line, over a blue sky). `heat_color` is the older speed-only ramp.
- **Head**: a round, soft blob (Gaussian across, faded to nothing at both ends along the direction of
  travel as seen on screen, as long as it is wide plus the on-screen part of one exposure of travel)
  in three nested layers, so it has no outline at any range and stays round when the body comes
  straight at the camera.
- **Ionised wake**: about a second and a half of samples behind the head (dense next to it),
  brightness falling off in two exponentials (under half the head after 0.1 s, under a twentieth by a
  second) and gusting, the colour cooling. Its pixel floor narrows as it dims, so it tapers to a point
  instead of reading as a constant-width rod. No vertex emits more than `peak_radiance`.
- **Afterglow**: the young train keeps a faint, gusting glow (`ember` on the train points) that cools
  and fades within `EMBER_LIFE_S` (3 s): a few seconds after the head goes dark nothing glows.
- **Train**: deposited where the body ablates, pale grey-brown (`DUST_COLOR`). It emerges from behind
  the wake over 0.6 s, is thrown out by the hot wake to 0.3 of the cylindrical blast radius (Mach number
  times the body's diameter, about 100 to 180 m half-width at 11 km/s) within about a second, then
  widens by turbulent diffusion (40 m per square-root second, about a kilometre across after two
  minutes on the width formula), billows and meanders smoothly, thins as it widens and drifts with
  `EntryProfile::wind`. Glowing dust fades on `dust_life_s` (24 s by default), so the default profile
  does not show that two-minute width. `train_life_s` (240 s) fades the dark-flight damage smoke and
  is the sample window; `legacy_dust` sets `dust_life_s` to 240 s. Below the ceiling a faint dark
  train (`damage_smoke`, 0 turns it off) stands for on-board damage. The train is lit smoke; it never
  emits.
- **Shading**: every glowing ribbon is soft (Gaussian across, no flat core) and additive (it adds
  light and never dims the sky behind it); the dust is soft and hides the sky behind it. Both use the
  flag slots of `TrailVertex` (`_pad`: additive share, `_pad2[0]`: soft); other trails leave them at 0
  and draw exactly as before.
- **Shed pieces** are ablation debris (2 to 20 cm). They break off from `shed_altitude_m` on, spread
  over the glowing flight, and fall back behind the head along the flight path: a fragment has more
  area per mass than the body, so drag (`Cd A rho v^2 / 2m`, with `A / m` going as one over its radius)
  slows it faster. The smaller the piece, the faster it falls back. A small sideways push at break-up
  (Passey and Melosh 1980, damped by the same drag) keeps them within a few pixels of the train's line
  at 4K. Each glows from its own heating (`rho v^3` at its own speed), never more than `PIECE_MAX_SHARE`
  (0.6) of the head, bright while it ablates, then cools (yellow, then orange) and fades over the end of a short life (`PIECE_LIFE_MIN_S` to
  `PIECE_LIFE_MAX_S`, 2 to 4 s, the smallest first), flickering as it tumbles; every piece is gone before
  the head goes dark. It is drawn as a short streak along its own velocity: one exposure of motion
  (`speed * shutter`, as the head) plus its vapour cooling behind it over `PIECE_WAKE_S` (0.12 s), one
  soft core with no halo. Seen exactly head-on it is a round blob, as the head is.
- **Dark body**: a thin dark streak of `speed * shutter` at least `min_px` wide, so it reads at range.
- **Range fit**: a point beyond 90 % of the camera's far plane is pulled along its view ray to that
  distance and its width shrinks by the same factor, so a bolide 190 km away looks the same but is not
  clipped by a 20 km far plane. The train keeps at least half of `min_px`.

## Using it

Add `EntryStreakPlugin` next to `VfxRuntimePlugin` and put an `EntryStreak` component on an entity:

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::Vec3;
# let app: &mut App = todo!();
# let world: &mut World = todo!();
use regia_vfx::entry::{EntryTrack, TrackPoint};
use regia_vfx::{EntryStreak, EntryStreakPlugin};

app.add_plugins(EntryStreakPlugin);
let track = EntryTrack::new(vec![
    TrackPoint { time: 0.0, position: Vec3::new(0.0, 60_000.0, -40_000.0) },
    TrackPoint { time: 8.0, position: Vec3::new(0.0, 30_000.0, 0.0) },
])?;
world.spawn(EntryStreak::new(track));
```

`frame` advances once per update while `playing`; set it to seek. Set `tier` to `EntryTier::Cheap` on
Low, Deck and Mobile: it keeps the head, a six-sample ion trail and a train at a quarter of the density,
and drops the pieces. `peak_radiance` is display-referred like every other VFX colour (above 1
blooms), so a host sets it against its exposure.

## Agent protocol

`vfx.entry_state` evaluates a track at a time and returns the head (position, altitude, speed, glow,
radiance, colour, `temperature_k`, `dark`), the counts of ion points, train points, glowing (`ember_points`)
train points and pieces, `ion_tail_temperature_k` (cooler than the head), `train_max_half_width_m` (the
widest train point), `glow_range_k` (the colour temperatures the glow shows, `[3500, 6000]`) and
`piece_streaks`: per glowing piece, `behind_m` (along the head's heading), `aside_m` (off the line
through the head along its heading), `speed`, `age_s`, `life_s`, `glow`, `glow_share` (its glow over
the head's) and `temperature_k`. It is read-only.

```json
{"jsonrpc":"2.0","id":1,"method":"vfx.entry_state","params":{
  "track":[{"time":0,"position":[90000,80000,107000]},{"time":120,"position":[700,105.7,5]}],
  "time":60,"tier":"cheap"}}
```

Check it: at a `time` where the track is at or below 20 km, `head.dark` is `true`, `head.glow` and
`head.radiance` are 0, and `ion_points` and `pieces` are 0 (`piece_streaks` is empty); 3 s later `ember_points` is 0 too, while
`train_points` and `train_max_half_width_m` (hundreds of metres) show the dust train. A few seconds
after the shed altitude, every entry of `piece_streaks` has `behind_m` > 0, `aside_m` smaller than
`behind_m` once the piece is a second old, and `glow_share` at most 0.6. Parameters: `track` (required, at most 64
points, times strictly increasing), `time`, `tier` (`full` or `cheap`), `ground_y`, `wind`,
`shed_count`, `seed`, and the dust knobs `dust_opacity_scale` (default 0.5), `dust_life_s` (default 24)
and `legacy_dust` (true starts from the pre-look-5 dust: opacity 1, pale grey-brown, 240 s).

Errors: `F0002` (invalid params, JSON-RPC `-32602`) for a missing or malformed track, a
non-increasing time, a non-finite value or an unknown tier.

## Cost

Evaluation is a few hundred CPU samples (about 1000 train points at the full tier, 240 at cheap) and
at most a few thousand ribbon vertices through the existing trail pipeline. Measured numbers at 1440p
and 4K are in the lane report; the effect adds no GPU pass of its own.
