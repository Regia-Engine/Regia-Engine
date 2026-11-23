// SPDX-License-Identifier: MPL-2.0
//! The road profile asset (`<name>.road.ron`, format name `road`, `format_version` 1; spec 64 § 7.1).
//!
//! A profile describes a road's cross section (carriageway, shoulders, crown), its materials and
//! texture tiling, curb pieces, markings, terrain flattening limits and junction margins. Road
//! nodes read it from the host's capture (`WorldSnapshot::road_profile`) by the asset path in their
//! `profile` setting.

use regia_app::prelude::*;

use crate::value::SettingValue;
use regia_scene::spawn::ReflectFormatVersion;

/// The starter road network graph (`starter/road_network.pgraph.ron`, spec 64 § 7.1): splines tagged
/// `road` through `road.junctions` into `road.ribbon` and `road.markings`. Add `terrain.flatten_road`
/// between the splines and the ribbon to flatten at bake time.
pub const STARTER_ROAD_NETWORK_GRAPH: &str = include_str!("../starter/road_network.pgraph.ron");

/// `format_version` of the saved [`RoadOf`] component (`road-of`).
pub const ROAD_OF_FORMAT_VERSION: u32 = 1;

/// Marks a spline entity as a road with a profile (spec 64 § 7.1). `in.entities { as: splines }`
/// puts the profile path in the item scope as `road_profile`, which road nodes read before their
/// own `profile` setting. Saved in scenes (`road-of` v1).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default, FormatVersion)]
pub struct RoadOf {
    /// The `.road.ron` asset path.
    pub profile: String,
    /// Always [`ROAD_OF_FORMAT_VERSION`].
    pub format_version: u32,
}

impl regia_scene::spawn::FormatVersion for RoadOf {
    const CURRENT: u32 = ROAD_OF_FORMAT_VERSION;
}

impl Default for RoadOf {
    fn default() -> Self {
        Self {
            profile: String::new(),
            format_version: ROAD_OF_FORMAT_VERSION,
        }
    }
}

impl RoadOf {
    /// A road of a profile path.
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            ..Self::default()
        }
    }

    /// Upgrades an older saved shape (none yet: version 1 is the first).
    ///
    /// # Errors
    ///
    /// A message for a version newer than this build reads.
    pub fn migrate(&mut self) -> Result<(), String> {
        if self.format_version > ROAD_OF_FORMAT_VERSION {
            return Err(format!(
                "road-of format_version {} is newer than {ROAD_OF_FORMAT_VERSION}",
                self.format_version
            ));
        }
        self.format_version = ROAD_OF_FORMAT_VERSION;
        Ok(())
    }
}

/// The shape fingerprint of the saved `road-of` component (`cargo xtask check-formats`).
pub fn road_of_schema_fingerprint() -> String {
    use regia_ecs::reflect::{TypeInfo, Typed};
    match RoadOf::type_info() {
        TypeInfo::Struct(s) => format!(
            "RoadOf({})",
            s.iter().map(|f| f.name()).collect::<Vec<_>>().join(",")
        ),
        _ => String::new(),
    }
}

/// `road-of` migration sources (none: version 1 is the first).
pub fn road_of_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// `format_version` of `.road.ron` files.
pub const ROAD_FORMAT_VERSION: u32 = 1;

/// The shape fingerprint of the `road` format (`cargo xtask check-formats`): field names in order.
pub fn road_schema_fingerprint() -> String {
    "RoadProfile(format_version,width_m,shoulders_m,surface_lift_m,cross_slope_pct,material,shoulder_material,\
     uv_m_per_tile,samples_per_m,pieces,markings,terrain,junction);RoadPiece(mesh,side,offset_m);\
     RoadMarking(pattern,offset_m,width_m,color);RoadTerrain(flatten,flatten_extra_m,falloff_m,carve_max_m,fill_max_m);\
     RoadJunction(margin_m,material);RoadSide(Left|Right)"
        .to_string()
}

/// `road` migration sources (none: version 1 is the first).
pub fn road_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Which side of the road a piece sits on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RoadSide {
    /// Left of the direction of travel.
    #[default]
    Left,
    /// Right of the direction of travel.
    Right,
}

/// A mesh repeated along one road edge (curbs, barriers).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoadPiece {
    /// Mesh asset.
    pub mesh: String,
    /// Side.
    pub side: RoadSide,
    /// Distance from the center line, meters.
    pub offset_m: f32,
}

/// A painted marking strip.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoadMarking {
    /// `solid` or `dash(on_m, off_m)`.
    pub pattern: String,
    /// Offset from the center line, meters (positive = right).
    pub offset_m: f32,
    /// Strip width, meters.
    pub width_m: f32,
    /// Linear RGB color.
    pub color: [f32; 3],
}

/// Terrain flattening limits (spec 64 § 7.3).
#[derive(Clone, Debug, PartialEq)]
pub struct RoadTerrain {
    /// Flatten the terrain under the road at bake time.
    pub flatten: bool,
    /// Extra flattened width beyond the shoulders, meters.
    pub flatten_extra_m: f32,
    /// Blend width back to the original terrain, meters.
    pub falloff_m: f32,
    /// Largest cut below the original terrain, meters.
    pub carve_max_m: f32,
    /// Largest fill above it, meters.
    pub fill_max_m: f32,
}

impl Default for RoadTerrain {
    fn default() -> Self {
        Self {
            flatten: false,
            flatten_extra_m: 1.0,
            falloff_m: 6.0,
            carve_max_m: 3.0,
            fill_max_m: 3.0,
        }
    }
}

/// Junction settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoadJunction {
    /// Cut-back margin beyond half the other road's width, meters.
    pub margin_m: f32,
    /// Patch material (`None` = the road's material).
    pub material: Option<String>,
}

/// A road profile (spec 64 § 7.1).
#[derive(Clone, Debug, PartialEq)]
pub struct RoadProfile {
    /// Always [`ROAD_FORMAT_VERSION`] after loading.
    pub format_version: u32,
    /// Carriageway width, meters.
    pub width_m: f32,
    /// Shoulder widths `(left, right)`, meters.
    pub shoulders_m: (f32, f32),
    /// Lift above the terrain, meters (0..=0.5).
    pub surface_lift_m: f32,
    /// Crown fall from the center line, percent.
    pub cross_slope_pct: f32,
    /// Carriageway material.
    pub material: String,
    /// Shoulder material (`None` = the carriageway's).
    pub shoulder_material: Option<String>,
    /// Meters along the road per texture tile (V).
    pub uv_m_per_tile: f32,
    /// Ribbon rows per meter (at least one row per 4 m).
    pub samples_per_m: f32,
    /// Edge pieces.
    pub pieces: Vec<RoadPiece>,
    /// Markings.
    pub markings: Vec<RoadMarking>,
    /// Terrain flattening.
    pub terrain: RoadTerrain,
    /// Junctions.
    pub junction: RoadJunction,
}

impl Default for RoadProfile {
    fn default() -> Self {
        Self {
            format_version: ROAD_FORMAT_VERSION,
            width_m: 7.0,
            shoulders_m: (0.5, 0.5),
            surface_lift_m: 0.02,
            cross_slope_pct: 2.0,
            material: String::new(),
            shoulder_material: None,
            uv_m_per_tile: 8.0,
            samples_per_m: 0.5,
            pieces: Vec::new(),
            markings: Vec::new(),
            terrain: RoadTerrain::default(),
            junction: RoadJunction {
                margin_m: 1.0,
                material: None,
            },
        }
    }
}

fn num(v: &SettingValue, name: &str, lo: f64, hi: f64) -> Result<f32, String> {
    v.as_f64()
        .filter(|x| x.is_finite() && (lo..=hi).contains(x))
        .map(|x| x as f32)
        .ok_or_else(|| format!("`{name}` must be a number in {lo}..={hi}"))
}

fn text(v: &SettingValue, name: &str) -> Result<String, String> {
    v.as_text()
        .map(str::to_string)
        .ok_or_else(|| format!("`{name}` must be a string"))
}

fn opt_text(v: &SettingValue, name: &str) -> Result<Option<String>, String> {
    match v {
        SettingValue::Ident(i) if i == "None" => Ok(None),
        SettingValue::Call(c, args) if c == "Some" && args.len() == 1 => text(&args[0], name).map(Some),
        other => text(other, name).map(Some),
    }
}

fn fields(v: &SettingValue) -> Option<&[(String, SettingValue)]> {
    match v {
        SettingValue::Record(_, f) => Some(f),
        _ => None,
    }
}

fn pair(v: &SettingValue, name: &str) -> Result<(f32, f32), String> {
    match v.as_items() {
        Some([a, b]) => Ok((num(a, name, 0.0, 1.0e3)?, num(b, name, 0.0, 1.0e3)?)),
        _ => Err(format!("`{name}` must be (left, right)")),
    }
}

impl RoadProfile {
    /// Parses `.road.ron` text (missing fields take the defaults of [`RoadProfile::default`]).
    ///
    /// # Errors
    ///
    /// A message for malformed RON, an unknown field, a value out of range, or a version newer
    /// than [`ROAD_FORMAT_VERSION`].
    pub fn from_ron(text_in: &str) -> Result<Self, String> {
        let v = SettingValue::parse(text_in).map_err(|e| e.to_string())?;
        Self::from_value(&v)
    }

    /// The profile from a parsed RON record (also an inline `profile` setting).
    ///
    /// # Errors
    ///
    /// As [`RoadProfile::from_ron`].
    pub fn from_value(v: &SettingValue) -> Result<Self, String> {
        let f = fields(v).ok_or("a road profile is a `RoadProfile(...)` record")?;
        let mut p = Self::default();
        for (k, x) in f {
            match k.as_str() {
                "format_version" => {
                    let ver = x.as_i64().ok_or("`format_version` must be an integer")?;
                    if ver > i64::from(ROAD_FORMAT_VERSION) || ver < 1 {
                        return Err(format!(
                            "road format_version {ver} is not supported (1..={ROAD_FORMAT_VERSION})"
                        ));
                    }
                }
                "width_m" => p.width_m = num(x, k, 0.1, 1.0e3)?,
                "shoulders_m" => p.shoulders_m = pair(x, k)?,
                "surface_lift_m" => p.surface_lift_m = num(x, k, 0.0, 0.5)?,
                "cross_slope_pct" => p.cross_slope_pct = num(x, k, -50.0, 50.0)?,
                "material" => p.material = text(x, k)?,
                "shoulder_material" => p.shoulder_material = opt_text(x, k)?,
                "uv_m_per_tile" => p.uv_m_per_tile = num(x, k, 0.01, 1.0e4)?,
                "samples_per_m" => p.samples_per_m = num(x, k, 0.0, 100.0)?,
                "pieces" => {
                    p.pieces = x
                        .as_items()
                        .ok_or("`pieces` must be a list")?
                        .iter()
                        .map(|e| {
                            Ok(RoadPiece {
                                mesh: e.field("mesh").map_or(Ok(String::new()), |m| text(m, "mesh"))?,
                                side: match e.field("side").map(SettingValue::as_text) {
                                    None | Some(Some("Left")) => RoadSide::Left,
                                    Some(Some("Right")) => RoadSide::Right,
                                    Some(other) => {
                                        return Err(format!(
                                            "piece `side` must be Left or Right, got {}",
                                            other.unwrap_or("a non-name value")
                                        ));
                                    }
                                },
                                offset_m: e
                                    .field("offset_m")
                                    .map_or(Ok(0.0), |o| num(o, "offset_m", -1.0e3, 1.0e3))?,
                            })
                        })
                        .collect::<Result<_, String>>()?;
                }
                "markings" => {
                    p.markings = x
                        .as_items()
                        .ok_or("`markings` must be a list")?
                        .iter()
                        .map(|e| {
                            let color = e
                                .field("color")
                                .and_then(SettingValue::as_vec3)
                                .unwrap_or([1.0; 3]);
                            Ok(RoadMarking {
                                pattern: e
                                    .field("pattern")
                                    .map_or(Ok("solid".to_string()), |m| text(m, "pattern"))?,
                                offset_m: e
                                    .field("offset_m")
                                    .map_or(Ok(0.0), |o| num(o, "offset_m", -1.0e3, 1.0e3))?,
                                width_m: e
                                    .field("width_m")
                                    .map_or(Ok(0.15), |o| num(o, "width_m", 0.0, 1.0e2))?,
                                color: color.map(|c| c as f32),
                            })
                        })
                        .collect::<Result<_, String>>()?;
                }
                "terrain" => {
                    let t = fields(x).ok_or("`terrain` must be a record")?;
                    for (tk, tv) in t {
                        match tk.as_str() {
                            "flatten" => {
                                p.terrain.flatten = tv.as_bool().ok_or("`flatten` must be a bool")?
                            }
                            "flatten_extra_m" => p.terrain.flatten_extra_m = num(tv, tk, 0.0, 1.0e3)?,
                            "falloff_m" => p.terrain.falloff_m = num(tv, tk, 0.0, 1.0e3)?,
                            "carve_max_m" => p.terrain.carve_max_m = num(tv, tk, 0.0, 1.0e3)?,
                            "fill_max_m" => p.terrain.fill_max_m = num(tv, tk, 0.0, 1.0e3)?,
                            other => return Err(format!("unknown terrain field `{other}`")),
                        }
                    }
                }
                "junction" => {
                    let j = fields(x).ok_or("`junction` must be a record")?;
                    for (jk, jv) in j {
                        match jk.as_str() {
                            "margin_m" => p.junction.margin_m = num(jv, jk, 0.0, 1.0e3)?,
                            "material" => p.junction.material = opt_text(jv, jk)?,
                            other => return Err(format!("unknown junction field `{other}`")),
                        }
                    }
                }
                other => return Err(format!("unknown road profile field `{other}`")),
            }
        }
        Ok(p)
    }

    /// Half the carriageway plus the shoulder on `side`, meters.
    pub fn half_extent(&self, side: RoadSide) -> f32 {
        self.width_m * 0.5
            + match side {
                RoadSide::Left => self.shoulders_m.0,
                RoadSide::Right => self.shoulders_m.1,
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_spec_example() {
        let p = RoadProfile::from_ron(
            r#"RoadProfile(
                format_version: 1,
                width_m: 7.0,
                shoulders_m: (0.5, 0.75),
                surface_lift_m: 0.02,
                cross_slope_pct: 2.0,
                material: "materials/asphalt.material.ron",
                shoulder_material: Some("materials/gravel.material.ron"),
                uv_m_per_tile: 8.0,
                samples_per_m: 0.5,
                pieces: [ (mesh: "road/curb.glb", side: Left, offset_m: 3.5), (mesh: "road/curb.glb", side: Right, offset_m: 3.5) ],
                markings: [ (pattern: "dash(3, 9)", offset_m: 0.0, width_m: 0.15, color: (1, 1, 1)) ],
                terrain: (flatten: true, flatten_extra_m: 1.0, falloff_m: 6.0, carve_max_m: 3.0, fill_max_m: 3.0),
                junction: (margin_m: 1.0, material: None),
            )"#,
        )
        .expect("parses");
        assert_eq!(p.shoulders_m, (0.5, 0.75));
        assert_eq!(
            p.shoulder_material.as_deref(),
            Some("materials/gravel.material.ron")
        );
        assert_eq!(p.pieces[1].side, RoadSide::Right);
        assert!(p.terrain.flatten);
        assert_eq!(p.half_extent(RoadSide::Right), 4.25);
        assert!(RoadProfile::from_ron("RoadProfile(format_version: 2)").is_err());
        assert!(RoadProfile::from_ron("RoadProfile(lanes: 4)").is_err());
        assert!(RoadProfile::from_ron("RoadProfile(surface_lift_m: 9.0)").is_err());
    }

    /// A piece `side` other than `Left` or `Right` is an error, like an unknown field, instead
    /// of silently becoming `Left` (spec 64 § 7.1; conformance lane 2026-11-10).
    #[test]
    fn an_unknown_piece_side_is_an_error() {
        let err =
            RoadProfile::from_ron(r#"RoadProfile(pieces: [(mesh: "c.glb", side: Rihgt, offset_m: 3.5)])"#)
                .expect_err("a misspelled side must not load");
        assert!(err.contains("side"), "{err}");
        let p = RoadProfile::from_ron(r#"RoadProfile(pieces: [(mesh: "c.glb", offset_m: 3.5)])"#)
            .expect("default side");
        assert_eq!(p.pieces[0].side, RoadSide::Left, "an omitted side is Left");
    }
}
