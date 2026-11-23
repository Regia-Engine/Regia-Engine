# Low sun: haze colour and sky

With the sun 9–12° above the horizon, the light that reaches the ground has crossed four to six
air masses and is orange. The level's `aerosol_turbidity` sets the haze's vertical optical depth,
and its Mie coefficients are spectrally flat (grey). A grey haze lit by that orange light
scatters it straight back. The hills, and the sky within 40° of the sun, all take one
sepia veil.

Real continental aerosol scatters more blue than red: `τ(λ) = τ₅₅₀ (λ / 550 nm)^−α` with Ångström
exponent α ≈ 1.3 (Ångström 1929; Eck et al. 1999 measure 1.0–1.5 over land, near 0 for desert
dust and sea salt). `AerosolSpectrum { angstrom }` (ADR-1408, `regia_render::aerosol`) makes the
Earth Mie coefficients follow that law about the green channel. The turbidity stays as set, the
haze in front of distant hills goes grey-blue, and the sun and its aureole still redden.

- Without the resource the Mie coefficients stay flat, as before.
- Earth air only; Mars and airless skies ignore it.
- `AerosolSpectrum::default()` is the continental 1.3.

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
app.insert_resource(regia_render::AerosolSpectrum::default());
```

## Agent

- `render.set_aerosol { angstrom: 1.3 }` inserts the resource (clamped to 0..3).
  `{ angstrom: null }` removes it.
- `render.get_aerosol` returns `{ active, angstrom, factors, wavelengths_nm, continental }`.
  `factors` are the per-channel multipliers for red (680 nm), green (550 nm) and blue (440 nm).
- Errors: a missing or non-number `angstrom` answers `F0002`.

The plain look (`--plain` in the showcase) has no bloom, so the sun disc shows as a small,
hard-edged dot with no glare. The film look adds the bloom around it.
