# Character rigs: attach, observe, verify

Twist chains, helper joints, drivers and pose-space correctives (`.drivers.ron`,
`docs/formats/drivers-ron.md`, [user guide](../user-guide/character-rig.md)) and skeleton profiles
(`.skelprofile.ron`).

## Attach a rig document

`anim.rig_attach { entity, ron, morphs?, dry_run? }` compiles the document against the rig's skeleton and attaches it.
Use `dry_run: true` first: it reports every unresolved joint or morph name in one error and attaches nothing.

```json
{"method": "anim.rig_attach", "params": {"entity": 4294967310, "dry_run": true, "ron": "(format_version: 1, drivers: [...])"}}
→ { "entity": 4294967310, "attached": false, "twists": ["forearm_twist"], "helpers": [], "drivers": ["elbow_bulge"],
    "correctives": [], "materials": [], "morphs": ["elbow_bulge"] }
```

`morphs` is the morph bank's names in bank order (so the weights line up with `MorphWeights`); by default the document's own
names in first-appearance order.

Parameter details: `entity` is a required integer entity id. `ron` is required string content for
`anim.rig_attach` and may be at most 16 MiB; `morphs` is an optional array of at most 1024 strings,
defaulting to the document's morph names in first-appearance order; `dry_run` is an optional boolean
(default `false`). `anim.rig_state` takes only the required `entity`. `anim.profile_check` takes the
required `entity` and requires at least one of optional string `profile` (`generic-humanoid` or
`vrm`) or optional string `ron` (at most 16 MiB); when both are present, `ron` takes precedence.

## Observe

`anim.rig_state { entity }` reports the last evaluated frame:

```json
→ { "evaluated": true,
    "twists": [{ "name": "forearm_twist", "value": 90.0 }],
    "helpers": [{ "name": "elbow_pad", "value": 100.0 }],
    "drivers": [{ "name": "elbow_bulge", "input": 100.0, "output": 1.0 }],
    "correctives": [{ "name": "elbow_crease", "activations": [1.0] }],
    "materials": { "wrinkle_elbow": 1.0 } }
```

Twist and helper values are degrees. Driver `input` is in the input's unit (degrees for joint inputs), `output` is the value
written (after the driver's `weight`). Corrective `activations` are the kernel values per example pose (1 at the example,
falling to 0 away from it).

## Verify a rig

1. `anim.rig_attach` with `dry_run: true`; fix any listed names; attach.
2. Pose the rig with `anim.set_param` (a parameter that drives a clip or a driver's `Param` input) and step (`play.step`).
3. `anim.rig_state`: a driver's `input` is the measured joint angle; its `output` should equal the curve's value there.
   A corrective's `activations` entry for a pose should read ~1 when the joints sit at that pose.
4. Capture the frame (`seeing.screenshot`) and compare the joint with and without the document (detach by respawning).

## Skeleton profiles

`anim.profile_check { entity, profile | ron }` checks the rig's skeleton against a built-in (`generic-humanoid`, `vrm`) or a
`.skelprofile.ron` text.

```json
{"method": "anim.profile_check", "params": {"entity": 4294967310, "profile": "vrm"}}
→ { "ok": false, "matched": 18, "missing_required": ["leftHand"], "missing_optional": ["leftThumbDistal", ...],
    "missing_roles": ["leftHand"], "extra_joints": ["paw"], "guessed": [], "hierarchy": [],
    "rest_pose": { "ok": false, "expected_deg": 0.0, "tolerance_deg": 10.0,
                   "arms": [{ "side": "left", "elevation_deg": 44.9 }, { "side": "right", "elevation_deg": 45.1 }] } }
```

`guessed` lists profile joints matched by the name guess rather than the name or an alias: review them. `rest_pose` is
`null` when the profile has no rule.

## Errors

| Code | When |
|---|---|
| `F0002` | Missing or invalid parameters, malformed `.drivers.ron` or `.skelprofile.ron`, an unknown profile, or a profile that fails validation. |
| `F0003` | `entity` is not a skinned rig, or `anim.rig_state` is called before drivers are attached. |

These are the handler's shared protocol codes. A skeleton that simply fails to match a valid profile
is a successful `anim.profile_check` result with `ok: false`, not an error.

## Failing call

This headless lavapipe call returns the captured invalid-entity error:

```text
$ regia agent call anim.profile_check '{"entity":0,"profile":"vrm"}' --project samples/boot_probe
error: method call failed: 0 is not an entity id (-32602): {"code":"F0002","hint":"check the method's parameters in the agent guide"}
```

The numeric proof lives in `crates/regia_animation/tests/character_rig.rs`: the largest roll step along a forearm drops from
90 to 30 degrees with twist joints, and pose-space correctives reproduce their authored weights at the example pose.
