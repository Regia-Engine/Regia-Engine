# Audio zones

Reverb volumes with presets, a per-zone distance scale, rock-thickness occlusion and mouth routing
(`docs/specs/16-audio.md` § 3d). Three methods; all need `regia_audio::AudioPlugin`.

## `audio.list_zones`

No params. Returns `{ zones, reverb, rock_field, count }`. Each zone: `entity`, `name`, `shape`
(`{kind: box|capsule|sphere|global, ...}`), `preset`, `wet_db`, `decay`, `damping`, `stereo_width`,
`distance_scale`, `blend_distance`, `cavity`, `priority`, `centre`, `portals[]` (world space).
`reverb` is the mix applied to player 0 (`weight` 0..1 is how far into the blend the listener is);
`rock_field` is true once the host installed an `AudioRockField`.

## `audio.set_zone`

Creates or edits an entity's `AudioReverbZone`. A `preset` (`tube`, `mine_gallery`, `open`, `custom`)
is applied first and keeps the entity's shape and priority; explicit fields then override it.

```json
{ "method": "audio.set_zone",
  "params": { "entity": 4294967310, "preset": "tube",
              "shape": { "kind": "capsule", "radius": 3.0, "half_length": 100.0 },
              "portals": [{ "position": [0.0, 100.0, 0.0], "radius": 5.0 }] } }
```

Fields: `shape`, `preset`, `wet_db` (-80..12), `decay` (0..1), `damping`, `stereo_width`,
`distance_scale` (0.1..10), `blend_distance` (m), `priority`, `cavity`, `portals`. Out-of-range values and
unknown presets are `invalid_params`; a failed call changes nothing.

## `audio.probe_zones`

Moves nothing. `listener` and `emitter` are `[x, y, z]`; `min_distance` (default 1) and `max_distance`
(default 50) describe the emitter's range.

```json
{ "method": "audio.probe_zones",
  "params": { "listener": [100, 1.7, 0], "emitter": [70, -10, 0], "max_distance": 400 } }
```

Returns `distance`, `distance_scale`, `effective_max_distance`, `in_cavity { listener, emitter }`,
`route` (`mouth`, `path_length`, `virtual_position`, or null), `rock_thickness_m` (null without a rock
field), `rock_gain`, `cutoff_hz`, `gain`, `gain_db`, `audible` (gain above 0.003) and `reverb`.

How to check a tunnel: probe with the listener on the surface above the emitter (`rock_thickness_m` is
the cover, `audible` false), at the mouth (`route` set, `rock_thickness_m` near 0), and inside the tube
(`distance_scale` 1.5, `effective_max_distance` 1.5 times the emitter's). `audio.list_emitters` reports
the live values per emitter: `rock_m`, `zone_scale`, `routed`.

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
audio.list_zones {}             # { zones, reverb, rock_field, count }
```

## A tunnel

A box zone with the `tube` preset over a tunnel's covered section is the whole setup; the tunnel
acceptance scene places one over its tube and checks that every corner of the tube is inside the zone,
that the listener is fully wet anywhere in the tube and dry once further out than the zone's fade
(`reverb_zone_covers_the_tube`, [Tunnel acceptance scene](tunnel-scene.md)).

## Portal sound

Over cooked world geometry (a loaded `<map>.meshlayers`, plan row W-25, ADR-8416) install
`regia_audio::GeomSoundField::new(Arc<WorldGeom>)` on `AudioRockField`. It reads only the cooked
sections, portals, zones and blocker plugs, plus the core's segment test, so every answer is a pure
function of the file and the two points (no wall clock):

- **Path through the portals.** A source the listener cannot reach in a straight line is heard from the
  last portal before the listener (the mouth, for a listener outside), at the length of the shortest
  path through the portal centres. The emitter update moves the voice to that apparent source, so the
  distance curve attenuates by the path length and the pan points at the mouth.
- **Rock occlusion.** With no portal path (a source in the rock beside a passage, a buried source, a
  ridge) the sound stays on the straight line and `RockTuning` attenuates it by the rock on it: the span
  between the first roof or covered wall (`WorldGeom::rock_between`) or uncut ground seen from each end;
  an end inside the rock counts from itself.
- **Section reverb.** The section player 0 stands in arms its cooked preset (`tube`, `gallery`, `open`)
  as a global zone with priority 1 (an authored finite zone still wins); leaving every section disarms it.
- **Hearing range.** A listener in a covered section hears twice as far (the underground rule,
  `PortalSoundSettings::underground_hear_mult`).

Cells: a point inside a section zone is in that section (the smallest zone wins), a point under uncut
ground is in the rock (no portal reaches it), anything else is outdoors.

Agent reads (both null or false without a geometry field):

- `audio.list_zones` adds `geom_field` (true when the rock field is a `GeomSoundField`) and
  `armed_reverb` (`section`, `preset`, `decay`, `wet_db`) at player 0.
- `audio.probe_zones` adds `portal_path` (`kind`: `direct`, `portal` or `occluded`; `source_cell` and
  `listener_cell` as `{kind: outdoor|rock|section, section}`; `portals` crossed, source side first;
  `points`; `length_m`; `direct_m`; `arrival` unit vector; `apparent` source; `occlusion_m`;
  `range_scale`) and `armed_reverb` at the probe's listener. `distance` is the path length when routed.

```text
audio.probe_zones {"listener": [x, y, z], "emitter": [x, y, z], "max_distance": 600}
# portal_path.kind "portal", portal_path.portals [..], route.virtual_position toward the mouth
```

No new error codes: the methods keep their `F0013` (no audio) and `invalid_params` errors.

