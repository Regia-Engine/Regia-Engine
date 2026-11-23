// SPDX-License-Identifier: MPL-2.0
// Original code (spec 55 § 2.3, ADR-2439). The numbers are authoring choices tuned to look right
// inside published textile ranges: typical fabric areal densities (mass per area) and bending
// rigidities as tabulated by the Kawabata Evaluation System in textile-science handbooks, e.g.
// S. Kawabata, "The Standardization and Analysis of Hand Evaluation", Textile Machinery Society
// of Japan, 1980, and the fabric-weight tables in W. E. Morton and J. W. S. Hearle, "Physical
// Properties of Textile Fibres", Woodhead 2008. No exact value is taken from those sources.

//! Named cloth starting points (`light_fabric`, `heavy_fabric`, `stiff_fabric`, `elastic`,
//! `chain`, `silk`, `denim`, `leather`, `rope_hemp`) and the pinning helpers for grid cloth
//! (spec 55 § 2.3, ADR-2439).
//!
//! A [`ClothPreset`] is one table row ([`PresetRow`]). [`ClothPreset::apply`] writes the
//! row's solver settings into a [`ClothConfig`] and a [`ClothTuning`] and is idempotent: it
//! assigns, it never accumulates. The link compliances and the areal density are not part of
//! either struct (they are fixed when a solver is built, ADR-2250): [`ClothPreset::params`]
//! gives a full [`ClothParams`] for a cooked asset, [`ClothPreset::particle_mass`] the mass of
//! a grid particle, and [`ClothSolver::apply_preset`] also rewrites the compliances of a live
//! solver.
//!
//! Every number below is "tuned to look right, within the textile ranges". The textile
//! literature gives ranges (areal density from about 0.05 kg/m² for silk chiffon to about
//! 1 kg/m² for thick leather; bending rigidity over about three orders of magnitude between
//! chiffon and canvas), not values for this solver's compliance units, so no number here claims
//! more precision than that.

use thiserror::Error;

use super::asset::{ClothParams, StrainCapParams, TearSettings, WindSetting};
use super::tuning::{ClothTuning, StrainCap, TearParams};
use super::wind::WindModel;
use super::{ClothConfig, ClothError, ClothGridHandle, ClothSolver};

/// A preset name no row matches. Agent reason `unknown_cloth_preset`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown_cloth_preset: `{name}` (known: {known})", known = ClothPreset::names_joined())]
pub struct UnknownPreset {
    /// The name that was asked for.
    pub name: String,
}

impl UnknownPreset {
    /// The stable agent reason code.
    pub const CODE: &'static str = "unknown_cloth_preset";
}

/// One preset's numbers (the table row). All values are authoring choices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PresetRow {
    /// Substeps per fixed step. Stiffer cloth gets more: stiff constraints converge better
    /// with substeps than with iterations (XPBD).
    pub substeps: u32,
    /// Gauss-Seidel iterations per substep.
    pub iterations: u32,
    /// Linear velocity damping in 1/s. Heavier or stiffer cloth is damped more so it does not
    /// ring.
    pub damping: f32,
    /// Compliance of the stretch links in m/N (0 = inextensible). Woven cloth barely stretches
    /// along the threads, knits and elastane do.
    pub stretch_compliance: f32,
    /// Compliance of the shear links in m/N. Woven cloth shears more easily than it stretches.
    pub shear_compliance: f32,
    /// Compliance of the dihedral bends in rad/N·m. This is the knob that separates chiffon
    /// (high, bends freely) from leather (low, holds a fold shape): the textile bending
    /// rigidity ranges span roughly three orders of magnitude and so does this column.
    pub bend_compliance: f32,
    /// Areal density in kg/m². Light silk is near 0.06, cotton shirting 0.12 to 0.2, denim
    /// 0.3 to 0.45, heavy wool or canvas near 0.5 and thin leather near 0.9; chain mail is far
    /// heavier than any cloth.
    pub density: f32,
    /// Hard cap on edge length ratios `(max, min)`, or `None`. `max` is slightly above 1 for
    /// woven cloth (real cloth does not stretch past a few percent) and far above for elastic.
    pub strain_cap: Option<(f32, f32)>,
    /// Tether scale (at least 1), or `None`.
    pub tether: Option<f32>,
    /// Aerodynamic drag coefficient of the wind model (dimensionless, spec 55 § 2.7).
    pub wind_drag: f32,
    /// Aerodynamic lift coefficient of the wind model (dimensionless).
    pub wind_lift: f32,
    /// Gravity multiplier (1 = normal). Presets leave it at 1; it is part of the row so a
    /// preset applied after a gravity tweak resets it, which keeps `apply` idempotent.
    pub gravity_scale: f32,
    /// Length ratio above which a link tears (greater than 1), or `None` for cloth that never
    /// tears in play. Tough cloth tears late (or never), light silk tears early.
    pub tear_strain: Option<f32>,
}

/// A named set of cloth settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClothPreset {
    /// Thin cotton or voile: moves in light wind, bends freely.
    LightFabric,
    /// Thick wool or canvas: heavy, drapes in folds, little wind response.
    HeavyFabric,
    /// Starched or interfaced cloth: holds its shape, bends in few places.
    StiffFabric,
    /// Knit with elastane: stretches well past 5 percent and springs back.
    Elastic,
    /// Chain mail or a bead curtain: very heavy, no stretch, no bending stiffness, no lift.
    Chain,
    /// Silk charmeuse or chiffon: the lightest and softest.
    Silk,
    /// Denim twill: heavy for cloth, moderately stiff.
    Denim,
    /// Thin garment leather: heavy, stiff, tears late.
    Leather,
    /// Hemp rope or webbing as a narrow ribbon: heavy per area, no lift.
    RopeHemp,
}

impl ClothPreset {
    /// Every preset, in documentation order.
    pub const ALL: [ClothPreset; 9] = [
        Self::LightFabric,
        Self::HeavyFabric,
        Self::StiffFabric,
        Self::Elastic,
        Self::Chain,
        Self::Silk,
        Self::Denim,
        Self::Leather,
        Self::RopeHemp,
    ];

    /// The stable lowercase name (`light_fabric`, ...).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::LightFabric => "light_fabric",
            Self::HeavyFabric => "heavy_fabric",
            Self::StiffFabric => "stiff_fabric",
            Self::Elastic => "elastic",
            Self::Chain => "chain",
            Self::Silk => "silk",
            Self::Denim => "denim",
            Self::Leather => "leather",
            Self::RopeHemp => "rope_hemp",
        }
    }

    /// The preset called `name`.
    ///
    /// # Errors
    /// [`UnknownPreset`] for a name no preset has (names are lowercase and exact).
    pub fn from_name(name: &str) -> Result<Self, UnknownPreset> {
        Self::ALL
            .into_iter()
            .find(|p| p.name() == name)
            .ok_or_else(|| UnknownPreset {
                name: name.to_owned(),
            })
    }

    fn names_joined() -> String {
        Self::ALL.map(Self::name).join(", ")
    }

    /// The table row.
    #[must_use]
    pub fn row(self) -> PresetRow {
        let base = PresetRow {
            substeps: 8,
            iterations: 1,
            damping: 0.2,
            stretch_compliance: 0.0,
            shear_compliance: 1e-4,
            bend_compliance: 1e-2,
            density: 0.15,
            strain_cap: Some((1.08, 0.9)),
            tether: None,
            wind_drag: 1.0,
            wind_lift: 0.3,
            gravity_scale: 1.0,
            tear_strain: None,
        };
        match self {
            Self::LightFabric => PresetRow {
                damping: 0.3,
                shear_compliance: 1e-4,
                bend_compliance: 5e-2,
                density: 0.12,
                ..base
            },
            Self::HeavyFabric => PresetRow {
                iterations: 2,
                damping: 0.15,
                shear_compliance: 1e-5,
                bend_compliance: 5e-3,
                density: 0.45,
                strain_cap: Some((1.05, 0.92)),
                wind_drag: 0.8,
                wind_lift: 0.15,
                ..base
            },
            Self::StiffFabric => PresetRow {
                substeps: 10,
                iterations: 2,
                damping: 0.4,
                shear_compliance: 1e-6,
                bend_compliance: 1e-4,
                density: 0.35,
                strain_cap: Some((1.03, 0.95)),
                wind_lift: 0.4,
                ..base
            },
            Self::Elastic => PresetRow {
                iterations: 2,
                stretch_compliance: 5e-3,
                shear_compliance: 5e-3,
                bend_compliance: 2e-2,
                density: 0.25,
                strain_cap: Some((1.6, 0.8)),
                tear_strain: Some(2.5),
                ..base
            },
            Self::Chain => PresetRow {
                substeps: 10,
                iterations: 3,
                damping: 0.5,
                shear_compliance: 0.0,
                bend_compliance: 0.0,
                density: 1.5,
                strain_cap: Some((1.01, 0.98)),
                wind_drag: 0.3,
                wind_lift: 0.0,
                ..base
            },
            Self::Silk => PresetRow {
                damping: 0.25,
                shear_compliance: 5e-5,
                bend_compliance: 1e-1,
                density: 0.06,
                wind_drag: 1.2,
                wind_lift: 0.4,
                tear_strain: Some(2.0),
                ..base
            },
            Self::Denim => PresetRow {
                iterations: 2,
                shear_compliance: 1e-5,
                bend_compliance: 1e-3,
                density: 0.38,
                strain_cap: Some((1.04, 0.94)),
                wind_drag: 0.9,
                wind_lift: 0.2,
                tear_strain: Some(3.0),
                ..base
            },
            Self::Leather => PresetRow {
                substeps: 10,
                iterations: 2,
                damping: 0.35,
                shear_compliance: 1e-6,
                bend_compliance: 2e-4,
                density: 0.9,
                strain_cap: Some((1.02, 0.96)),
                wind_drag: 0.7,
                wind_lift: 0.1,
                tear_strain: Some(3.0),
                ..base
            },
            Self::RopeHemp => PresetRow {
                substeps: 10,
                iterations: 2,
                damping: 0.4,
                shear_compliance: 0.0,
                bend_compliance: 2e-3,
                density: 0.6,
                strain_cap: Some((1.03, 0.97)),
                wind_drag: 0.5,
                wind_lift: 0.0,
                tear_strain: Some(3.0),
                ..base
            },
        }
    }

    /// Writes this preset's solver settings into `tuning` and `config`: substeps, iterations,
    /// damping, gravity scale, strain cap, tether, wind model and tearing. Fields not in the
    /// row (collision, self-collision, pose pull, time step) keep their values. Applying a
    /// preset twice gives the same result as once.
    pub fn apply(self, tuning: &mut ClothTuning, config: &mut ClothConfig) {
        let r = self.row();
        config.substeps = r.substeps;
        config.iterations = r.iterations;
        config.damping = r.damping;
        tuning.gravity_scale = r.gravity_scale;
        tuning.strain_cap = r.strain_cap.map(|(max, min)| StrainCap { max, min });
        tuning.tether = r.tether;
        tuning.wind_model = WindModel::Aerodynamic {
            drag: r.wind_drag,
            lift: r.wind_lift,
        };
        tuning.tear = r.tear_strain.map(|strain| TearParams {
            strain,
            ..TearParams::default()
        });
    }

    /// A full parameter set for a cooked asset: the defaults with this row's numbers.
    #[must_use]
    pub fn params(self) -> ClothParams {
        let r = self.row();
        ClothParams {
            gravity_scale: r.gravity_scale,
            damping: r.damping,
            stretch_compliance: r.stretch_compliance,
            shear_compliance: r.shear_compliance,
            bend_compliance: r.bend_compliance,
            substeps: r.substeps,
            iterations: r.iterations,
            strain_cap: r.strain_cap.map(|(max, min)| StrainCapParams { max, min }),
            tether: r.tether,
            wind_model: WindSetting::Aerodynamic {
                drag: r.wind_drag,
                lift: r.wind_lift,
            },
            tear: r.tear_strain.map(|strain| TearSettings {
                strain,
                max_per_step: TearParams::default().max_per_step,
            }),
            density: r.density,
            ..ClothParams::default()
        }
    }

    /// Mass in kg of one particle of a square grid with `spacing` meters between neighbors:
    /// the areal density times the area one particle owns (`spacing²`).
    #[must_use]
    pub fn particle_mass(self, spacing: f32) -> f32 {
        self.row().density * spacing * spacing
    }

    /// `params` with this preset underneath: every field of `params` that still has its
    /// [`ClothParams::default`] value takes the preset's, every field the author changed is
    /// kept. This is how a `.cloth.ron` with both `preset` and `params` reads.
    #[must_use]
    pub fn under(self, params: &ClothParams) -> ClothParams {
        let d = ClothParams::default();
        let p = self.params();
        macro_rules! pick {
            ($($f:ident),* $(,)?) => {
                ClothParams { $($f: if params.$f == d.$f { p.$f } else { params.$f }),* }
            };
        }
        pick!(
            gravity_scale,
            gravity_override,
            damping,
            stretch_compliance,
            shear_compliance,
            bend_compliance,
            substeps,
            iterations,
            pull_compliance,
            inertia_scale,
            strain_cap,
            tether,
            collision_thickness,
            friction,
            self_collision,
            self_friction,
            wind_model,
            air_density,
            teleport_distance,
            tear,
            density,
            wind_scale,
        )
    }
}

/// An edge of a grid cloth for [`ClothSolver::pin_edge`]. "Top" is row 0, "Left" is column 0;
/// the names follow the order [`ClothSolver::add_grid`] stores particles in, not world
/// directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GridEdge {
    /// Row 0 (a flag's hoist side lies on `Left`; a curtain hangs from `Top`).
    Top,
    /// The last row.
    Bottom,
    /// Column 0.
    Left,
    /// The last column.
    Right,
}

impl ClothSolver {
    /// Applies `preset` to this solver: [`ClothPreset::apply`] on the current config and
    /// tuning, and the preset's stretch and bend compliance on every link. The shear
    /// compliance is not applied: a built solver does not remember which links are shear
    /// (build grids with the preset's compliances, or cook with [`ClothPreset::params`]).
    /// Nothing changes when a value is rejected.
    ///
    /// # Errors
    /// The [`ClothError`] of the first rejected setting.
    pub fn apply_preset(&mut self, preset: ClothPreset) -> Result<(), ClothError> {
        let mut config = *self.config();
        let mut tuning = *self.tuning();
        preset.apply(&mut tuning, &mut config);
        config.validate()?;
        tuning.validate()?;
        let row = preset.row();
        self.set_config(config)?;
        self.set_tuning(tuning)?;
        for link in &mut self.distance {
            link.compliance = row.stretch_compliance;
        }
        for bend in &mut self.bends {
            bend.compliance = row.bend_compliance;
        }
        Ok(())
    }

    /// Sets the compliance of every distance link (`stretch`; built shear links too, as
    /// [`Self::apply_preset`] does) and every bend link (`bend`), at runtime
    /// (`physics.cloth_set_params`, ADR-9070). `None` leaves that kind alone. Nothing changes when
    /// a value is rejected.
    ///
    /// # Errors
    /// [`ClothError::NonFinite`] or [`ClothError::Negative`] for a NaN, infinite or negative
    /// compliance.
    pub fn set_link_compliances(
        &mut self,
        stretch: Option<f32>,
        bend: Option<f32>,
    ) -> Result<(), ClothError> {
        let stretch = stretch
            .map(|c| super::non_negative(c, "stretch_compliance"))
            .transpose()?;
        let bend = bend
            .map(|c| super::non_negative(c, "bend_compliance"))
            .transpose()?;
        if let Some(c) = stretch {
            for link in &mut self.distance {
                link.compliance = c;
            }
        }
        if let Some(c) = bend {
            for link in &mut self.bends {
                link.compliance = c;
            }
        }
        Ok(())
    }

    /// The compliance of the first distance link and of the first bend link (`None` when there
    /// is no such link). After [`Self::set_link_compliances`] or [`Self::apply_preset`] every link
    /// of that kind has it.
    #[must_use]
    pub fn link_compliances(&self) -> (Option<f32>, Option<f32>) {
        (
            self.distance.first().map(|l| l.compliance),
            self.bends.first().map(|l| l.compliance),
        )
    }

    /// Pins every particle whose value in `map` is at least `threshold` (the same rule a
    /// `reach`-style vertex map uses: 1 = fixed). Returns how many were pinned.
    ///
    /// # Errors
    /// [`ClothError::MapSizeMismatch`] when `map` is not one value per particle,
    /// [`ClothError::NonFinite`] for NaN or infinity.
    pub fn pin_by_map(&mut self, map: &[f32], threshold: f32) -> Result<usize, ClothError> {
        if !threshold.is_finite() {
            return Err(ClothError::NonFinite { what: "threshold" });
        }
        if map.len() != self.particles.len() {
            return Err(ClothError::MapSizeMismatch {
                map: "pin map",
                len: map.len(),
                expected: self.particles.len(),
            });
        }
        if map.iter().any(|v| !v.is_finite()) {
            return Err(ClothError::NonFinite { what: "pin map" });
        }
        let mut pinned = 0;
        for (i, &v) in map.iter().enumerate() {
            if v >= threshold {
                self.pin(i)?;
                pinned += 1;
            }
        }
        Ok(pinned)
    }

    /// Pins one edge of the grid `grid` (a flag pole side, a curtain rail, a cape's shoulder
    /// line). Returns how many particles were pinned.
    ///
    /// # Errors
    /// [`ClothError::ParticleOutOfRange`] when the handle does not belong to this solver.
    pub fn pin_edge(&mut self, grid: &ClothGridHandle, edge: GridEdge) -> Result<usize, ClothError> {
        let mut pinned = 0;
        for row in 0..grid.rows {
            for column in 0..grid.columns {
                let on = match edge {
                    GridEdge::Top => row == 0,
                    GridEdge::Bottom => row + 1 == grid.rows,
                    GridEdge::Left => column == 0,
                    GridEdge::Right => column + 1 == grid.columns,
                };
                if on && let Some(i) = grid.index(column, row) {
                    self.pin(i)?;
                    pinned += 1;
                }
            }
        }
        Ok(pinned)
    }
}

#[cfg(test)]
mod tests {
    use regia_core::math::Vec3;

    use super::*;
    use crate::cloth::ClothGrid;

    const N: usize = 20;
    const SPACING: f32 = 0.05;

    /// A 20 x 20 grid hanging from its top edge in the XY plane (rows go down).
    fn hanging(preset: ClothPreset) -> (ClothSolver, ClothGridHandle) {
        let row = preset.row();
        let mut solver = ClothSolver::new(ClothConfig::default()).expect("solver");
        let grid = ClothGrid {
            origin: Vec3::new(0.0, 0.0, 0.0),
            u_axis: Vec3::X,
            v_axis: -Vec3::Y,
            columns: N,
            rows: N,
            spacing: SPACING,
            particle_mass: preset.particle_mass(SPACING),
            stretch_compliance: row.stretch_compliance,
            shear_compliance: row.shear_compliance,
            bend_compliance: row.bend_compliance,
        };
        let handle = solver.add_grid(&grid).expect("grid");
        solver.pin_edge(&handle, GridEdge::Top).expect("pin");
        solver.apply_preset(preset).expect("preset");
        (solver, handle)
    }

    fn bottom_y(solver: &ClothSolver, grid: &ClothGridHandle) -> f32 {
        let mut sum = 0.0;
        for c in 0..grid.columns {
            let i = grid.index(c, grid.rows - 1).expect("index");
            sum += solver.particles()[i].position.y;
        }
        sum / grid.columns as f32
    }

    fn bottom_z(solver: &ClothSolver, grid: &ClothGridHandle) -> f32 {
        let i = grid.index(N / 2, grid.rows - 1).expect("index");
        solver.particles()[i].position.z
    }

    #[test]
    fn every_preset_is_valid_and_named_uniquely() {
        let mut names = std::collections::BTreeSet::new();
        for preset in ClothPreset::ALL {
            let mut tuning = ClothTuning::default();
            let mut config = ClothConfig::default();
            preset.apply(&mut tuning, &mut config);
            tuning.validate().expect("tuning valid");
            config.validate().expect("config valid");
            preset.params().validate().expect("params valid");
            let r = preset.row();
            for v in [
                r.damping,
                r.stretch_compliance,
                r.shear_compliance,
                r.bend_compliance,
                r.density,
                r.wind_drag,
                r.wind_lift,
                r.gravity_scale,
            ] {
                assert!(v.is_finite() && v >= 0.0, "{}: {v}", preset.name());
            }
            assert!((0.01..=3.0).contains(&r.density), "{} density", preset.name());
            assert!(names.insert(preset.name()));
            assert_eq!(ClothPreset::from_name(preset.name()), Ok(preset));
        }
        assert_eq!(names.len(), 9);
    }

    #[test]
    fn unknown_names_are_an_error() {
        let err = ClothPreset::from_name("Silk").expect_err("case matters");
        assert_eq!(err.name, "Silk");
        assert!(err.to_string().starts_with("unknown_cloth_preset"));
        assert!(ClothPreset::from_name("").is_err());
        assert!(ClothPreset::from_name("light_fabric ").is_err());
    }

    #[test]
    fn apply_twice_equals_once() {
        for preset in ClothPreset::ALL {
            let mut t1 = ClothTuning {
                gravity_scale: 2.0,
                tether: Some(1.5),
                ..ClothTuning::default()
            };
            let mut c1 = ClothConfig::default();
            preset.apply(&mut t1, &mut c1);
            let (mut t2, mut c2) = (t1, c1);
            preset.apply(&mut t2, &mut c2);
            assert_eq!(t1, t2);
            assert_eq!(c1, c2);
            // The same on a solver: digest and settings agree.
            let (mut a, _) = hanging(preset);
            let (mut b, _) = hanging(preset);
            b.apply_preset(preset).expect("again");
            a.step_fixed(30);
            b.step_fixed(30);
            assert_eq!(a.state_digest(), b.state_digest(), "{}", preset.name());
        }
    }

    #[test]
    fn same_preset_same_digest() {
        let run = || {
            let (mut s, _) = hanging(ClothPreset::Silk);
            s.set_wind(Vec3::new(0.0, 0.0, 4.0)).expect("wind");
            s.step_fixed(90);
            s.state_digest()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn stiff_hangs_with_less_wind_displacement_than_light() {
        let wind = Vec3::new(0.0, 0.0, 6.0);
        let mut out = Vec::new();
        for preset in [ClothPreset::LightFabric, ClothPreset::StiffFabric] {
            let (mut s, g) = hanging(preset);
            s.set_wind(wind).expect("wind");
            s.step_fixed(240);
            let cap = preset.row().strain_cap.expect("cap").0;
            let stretch = s.stats().max_stretch;
            assert!(
                stretch <= cap + 1e-3,
                "{} stretch {stretch} over cap {cap}",
                preset.name()
            );
            assert!(bottom_y(&s, &g) < 0.0, "hangs below the rail");
            out.push((bottom_y(&s, &g), bottom_z(&s, &g)));
        }
        let (light, stiff) = (out[0], out[1]);
        // The light cloth lifts and blows out more: its bottom edge is higher (it swings up)
        // and further downwind than the stiff cloth's.
        assert!(
            light.0 > stiff.0,
            "light bottom {} stiff bottom {}",
            light.0,
            stiff.0
        );
        assert!(light.1 > stiff.1, "light z {} stiff z {}", light.1, stiff.1);
    }

    #[test]
    fn heavy_accelerates_less_than_light_in_the_same_wind() {
        // A free sheet facing the wind with gravity off: the mean downwind speed after a short
        // time measures the acceleration the areal density allows.
        let mut speed = Vec::new();
        for preset in [ClothPreset::LightFabric, ClothPreset::HeavyFabric] {
            let (mut s, _) = hanging(preset);
            for i in 0..s.particles().len() {
                s.set_inv_mass(i, 1.0 / preset.particle_mass(SPACING))
                    .expect("free");
            }
            let mut tuning = *s.tuning();
            tuning.gravity_scale = 0.0;
            s.set_tuning(tuning).expect("tuning");
            s.set_wind(Vec3::new(0.0, 0.0, 6.0)).expect("wind");
            s.step_fixed(10);
            let n = s.particles().len() as f32;
            speed.push(s.particles().iter().map(|p| p.velocity.z).sum::<f32>() / n);
        }
        assert!(speed[0] > 0.0, "{speed:?}");
        assert!(speed[0] > speed[1] * 1.5, "light {} heavy {}", speed[0], speed[1]);
    }

    #[test]
    fn elastic_stretches_under_load_and_recovers() {
        let (mut s, g) = hanging(ClothPreset::Elastic);
        s.step_fixed(120);
        let relaxed = s.stats().max_stretch;
        // Load: the bottom row becomes 40 times heavier (a hem weight).
        let free = s.particles()[g.index(N / 2, N - 1).expect("index")].inv_mass;
        for c in 0..N {
            s.set_inv_mass(g.index(c, N - 1).expect("index"), free / 40.0)
                .expect("load");
        }
        s.step_fixed(240);
        let loaded = s.stats().max_stretch;
        assert!(loaded > 1.05, "loaded stretch {loaded} (relaxed {relaxed})");
        let cap = ClothPreset::Elastic.row().strain_cap.expect("cap").0;
        assert!(loaded <= cap + 1e-3);
        for c in 0..N {
            s.set_inv_mass(g.index(c, N - 1).expect("index"), free)
                .expect("unload");
        }
        s.step_fixed(600);
        let after = s.stats().max_stretch;
        assert!(after < loaded, "after {after} loaded {loaded}");
        assert!(after < relaxed + 0.03, "after {after} relaxed {relaxed}");
    }

    #[test]
    fn pin_helpers() {
        let (mut s, g) = hanging(ClothPreset::Silk);
        assert_eq!(s.stats().pinned, N);
        s.pin_edge(&g, GridEdge::Left).expect("left");
        assert_eq!(s.stats().pinned, 2 * N - 1);
        // A map pins by threshold; the wrong length is an error.
        let mut map = vec![0.0; s.particles().len()];
        map[N * N - 1] = 1.0;
        assert_eq!(s.pin_by_map(&map, 0.5).expect("map"), 1);
        assert_eq!(s.stats().pinned, 2 * N);
        assert!(s.pin_by_map(&[1.0], 0.5).is_err());
        map[0] = f32::NAN;
        assert!(s.pin_by_map(&map, 0.5).is_err());
        let foreign = ClothGridHandle {
            first: 10_000,
            columns: 2,
            rows: 2,
        };
        assert!(s.pin_edge(&foreign, GridEdge::Top).is_err());
    }

    #[test]
    fn under_keeps_author_changes() {
        let params = ClothParams {
            damping: 3.0,
            ..ClothParams::default()
        };
        let merged = ClothPreset::Leather.under(&params);
        assert_eq!(merged.damping, 3.0);
        assert_eq!(merged.density, ClothPreset::Leather.row().density);
        merged.validate().expect("valid");
    }
}
