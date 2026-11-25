# Vehicle tuning

Panel id: `vehicle_tuning`.

The **Vehicle Tuning** tab tunes a car (`.vehicle.ron`), a trailer (`.trailer.ron`), a fixed-wing
aircraft (`.aircraft.ron`), a two-wheel bike (`.bike.ron`) and a hover hull (`.hoverhull.ron`). It shows each configuration as editable sections, plots the car with the
runtime's own vehicle math, sends the edited car to a running play session without a respawn, and tries
every edit on a proving ground: scripted car runs on a flat plane, a ramp, a slope or a bump strip (towing
the trailer if you like), level flights for the aircraft, and rides for the bike and the hover hull. Depth for agents:
[Vehicles: Vehicle Tuning panel](../../agents/vehicles.md#vehicle-tuning-panel-editor) and
[Vehicle tuning: trailers, aircraft and tracks](../../agents/vehicle-tuning.md).

Open it from the editor's tab list (**Vehicle Tuning**); it docks with the other tool tabs.

## The car

| Action | Control | Agent method |
|---|---|---|
| Open a car | the path box, **Open** | `editor.vehicle_tuning_open { path }` |
| Start a new car (the default car, named after the file) | **New** | `editor.vehicle_tuning_open { path, create: true }` |
| Edit a field | the number, checkbox and picker fields of each section | `editor.vehicle_tuning_set { field, value }` |
| Undo, redo | **Undo**, **Redo** | `editor.vehicle_tuning_undo`, `editor.vehicle_tuning_redo` |
| Save | **Save** | `editor.vehicle_tuning_save` |
| Send the car to the play session | **Apply** (while playing) | `editor.vehicle_tuning_apply` |
| Run a script | **Run accel**, **Run skidpad**, the run length | `editor.vehicle_tuning_run { script, seconds }` |
| Pick the proving-ground track | the **track** picker in Runs | `editor.vehicle_tuning_run { track }` |
| Tow the open trailer | **tow the open trailer** in Runs | `editor.vehicle_tuning_run { tow: true }` |
| Read everything | the tab | `editor.vehicle_tuning_state` |

The sections are chassis, wheels (with the tire model), engine and its torque curve, gearbox, clutch,
differentials, anti-roll bars, aero and driver aids. A value the configuration refuses is not applied: the
field shows **refused** and the status line names the reason and its code. Dragging a number is one undo
step however long the drag lasts. **Save** writes the file atomically and keeps the file's comments
whenever it can; if it cannot, the status line says the comments were lost.

The plots show torque and power against engine speed, the force at the wheels in every gear, the selected
wheel's tire side force against slip angle, and its suspension force against travel.

## Proving ground

Every run drives the edited car from rest on a private world with a fixed 60 Hz step, so the same car and
script always give the same numbers. The runs table lists each run side by side: 0 to 100 km/h, top speed,
skid-pad lateral g, upshifts, the track, the wheel contacts and the trailer.

| Track | What is in the car's lane, 20 m ahead |
|---|---|
| flat | Nothing. |
| ramp | A wedge 1.5 m high and 6 m long: the car jumps off its top edge. |
| slope | A 10 % grade rising 6 m, then a long plateau. |
| bump strip | Low ridges every 2 m, alternating left and right: the wheels lose and regain the ground. |

**contacts** shows the fewest and most wheels on the ground during the run and how many steps that number
changed: on the flat plane it never changes; on the bump strip it does, and a car that stays planted over it
has softer, better-damped suspension.

With **tow the open trailer** ticked, the run spawns the open trailer coupled to a tow ball just behind the
car and brakes it with the car; the trailer column shows its mass and its largest articulation angle.

## The trailer

The **Trailer (.trailer.ron)** section has its own path box, **Open**, **New**, **Save**, **Undo** and
**Redo**, and shows the trailer's fields: the chassis (a vehicle configuration whose wheels neither drive
nor steer), the coupling (its kind, point and capture radius), the brakes, the parking brake, the stand and
the rear hitch. Agents use the same methods as for the car with `asset: "trailer"`. To try an edit, tick
**tow the open trailer** in Runs and run the car.

## The aircraft

The **Aircraft (.aircraft.ron)** section works the same way (`asset: "aircraft"`): mass, center of mass,
inertia, fuselage, parasite drag, each lifting surface (span, chord, incidence, airfoil coefficients, stall
and control links), engines and landing gear.

**Fly** takes an airspeed, an altitude and a length: it trims the aircraft for level flight there and flies
that trim on its own. The flights table shows the trim's angle of attack and throttle and how far the flight
drifted in altitude, speed and bank; a well-trimmed aircraft holds its altitude within a meter or two. Agent
method: `editor.vehicle_tuning_fly`.

## The bike and the hover hull

The **Bike (.bike.ron)** and **Hover hull (.hoverhull.ron)** sections work the same way (`asset: "bike"`,
`asset: "hoverhull"`). The bike's fields are its wheelbase, gravity, the lean limit and lean rate, the
upright speed and spring, the lean response and the wheel-contact reach. The hover hull's fields are its
`params`: ride height, thrust and braking, drag, speed limits, yaw and roll rates and the cushion; the
file's limits and flight mode are kept as written.

**Ride 1 s** rides the open bike or hull on the flat proving ground for 60 fixed steps with the held input
beside it (the bike's steer and speed; the hull's throttle, strafe and steer). The first ride spawns it at
rest; each later ride carries on from where the last stopped, so an edit you make between rides reaches the
bike or hull that is already riding, without a respawn. **Respawn** starts it at rest again. The line under
the buttons shows the step count and the lean and speed (bike) or speed, heading and altitude (hull). Agent
method: `editor.vehicle_tuning_ride`.

## Limits

- Trailers and aircraft have no **Apply** to a play session: try them on the proving ground or with **Fly**.
- Bikes and hover hulls have no **Apply** either: try them with **Ride**. The hover form does not edit a
  hull's limits or flight mode.

The tab has no keyboard shortcuts of its own; every control takes focus with Tab.
