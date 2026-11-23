# Contact shadows (screen-space, sun only)

The cascaded sun shadows resolve a few centimeters per texel close to the camera and meters
far away, so small casters lose their shadow: a rock seems to float, a tire does not touch the
road, a ledge of relief has no dark underside. Contact shadows add it back from the depth
buffer. Every pixel marches toward the sun's point on screen: 48 pixels at 1440 lines and
above, proportionally fewer on smaller targets (at least 4), so the shadows cover the same
share of the screen at every resolution. A depth sample in front of the ray blocks the sun
when it is in front by more than a small bias and by less than an assumed thickness, and also
in front of the receiver's own surface continued along the march: a surface never shadows
itself, even when it faces away from the camera (inside-out meshes). The bias
grows where the depth buffer cannot be trusted to a pixel (silhouettes, ground seen at a
grazing angle), so edges do not grow dotted dark lines. The result multiplies the **sun term
only**: sky light, bounce and local lights are untouched.

It runs on Desktop Low/High/Ultra and Deck (`RenderProfile::contact_shadows`), and is off on
Mobile Low and Mobile High. It needs the depth prepass and a single-sample frame, like SSAO, and
runs only when the sun casts shadows. Lab and editor baselines (`FrameQuality::LAB`,
`FrameQuality::EDITOR`) keep it off. The shared terrain draw gets the same texture
(`SharedPassDraw::set_engine_contact_shadows`), so rocks shadow the ground and relief shadows
itself.

## Toggle and compare

`render.set_contact_shadows { "enabled": false }` returns
`{ "before": { "enabled", "active" }, "after": { … } }`. `active` is false when the profile has no
depth prepass or uses MSAA, even with `enabled: true`. A/B it like any look change:

1. `seeing.screenshot` (target `"offscreen"`) → `on.png`
2. `render.set_contact_shadows { "enabled": false }`, `play.step`, `seeing.screenshot` → `off.png`
3. `seeing.compare_images` on the two; set `enabled` back to `true`.

Look for: dark seams under rocks and props where they meet the ground, shadowed undersides of
ledges and crust plates, and the sun side of bumps lit while their lee side darkens. A shadow
that floats away from its caster, or flat ground darkening at a grazing sun, is a bug; report
the screenshot.

## Cost

One compute dispatch after SSAO, about two depth loads per pixel (each 64-pixel line strip is
loaded once into workgroup memory). `GpuTimings::contact_shadows_ms` reports it; the lookdev
chase prints it as `gpu contact shadows`. Budget: ≤ 0.3 ms at 2560×1440 on Desktop High.

From lookdev: `LOOK_R2_PROFILE=contact=0` turns it off for before/after shots of the DO27 plates.
