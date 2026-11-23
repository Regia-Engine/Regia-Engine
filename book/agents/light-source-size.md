# Light source size

A point or spot light is a point unless its entity also has a `LightSourceSize { radius_m }`
component (ADR-1367). The radius is that of the emitting sphere, in meters. It changes three
things:

- **Shadow softness.** The light's local shadow (PCSS) widens its penumbra in proportion to the
  radius, up to the filter's 10-texel cap. Without the component the shadow source is 45 cm, the
  engine-wide value used before this component existed.
- **Highlight size.** On glossy surfaces the specular highlight takes the light's size. It is lit
  from the point of the sphere nearest the reflected view ray, and the lobe is renormalized so a
  bigger light gives a broader, dimmer highlight with the same energy. Diffuse light does not
  change: a sphere above the horizon lights a surface like a point at its center.
- **Reference traces.** `seeing.reference_trace` traces a point light with a positive source
  size as a sphere light of that radius and the same intensity. A spot with a positive radius
  is traced as a sphere limited by the spot cone (`PtLight::SphereSpot`). A spot with no size,
  or a radius of 0, stays a point.

`radius_m: 0.0` is a pinpoint highlight. On a PCSS profile (Desktop High and Ultra) the local
shadow filter still has its 2.5-texel contact floor: a zero radius does not become a 1-tap
hard edge. A negative radius counts as 0. A non-finite one counts as absent. The component's
default (0.45) matches the absent shadow, so adding it with its default only changes the highlight.

## Add and edit it

```text
scene.add_component  { "entity": 17, "component": "LightSourceSize", "value": { "radius_m": 0.05 } }
editor.set_property  { "entity": 17, "component": "LightSourceSize", "path": "radius_m", "value": 0.2 }
editor.set_property  { "entity": 17, "component": "LightSourceSize", "path": "radius_m", "value": 0.3, "dry_run": true }
editor.undo          {}
```

`editor.set_property` returns `{ "before", "after", "applied" }` and is one undo step. A value
that is not a number fails with `F0002` (invalid params). A missing component or field fails with
`F0003` (not found), the same as for any component. This page adds no error codes. Remove the component (`scene.remove_component`) to go
back to the defaults.

## Check it

1. Put a shadowed point light 1–2 m above a crate on a floor, with a glossy sphere in view.
   Take a `seeing.screenshot` and save it as `point.png`.
2. Add `LightSourceSize { radius_m: 0.02 }`, run `play.step`, and take `small.png`. The crate's
   shadow edge is sharper than in `point.png` (45 cm before), and the sphere's highlight is a
   small, bright dot.
3. Set `radius_m` to 0.3 and take `large.png`. The shadow edge is wider and the highlight is a
   broader, dimmer disc.
4. Run `seeing.compare_images` on the pairs. On a matte surface away from shadow edges the three
   images match: the size changes only shadows and highlights.

Typical sizes: an LED or a cockpit indicator 0.005–0.02 m, a household bulb 0.03–0.06 m, a tunnel
fixture 0.3–0.5 m, a floodlight lens 0.15–0.4 m.
