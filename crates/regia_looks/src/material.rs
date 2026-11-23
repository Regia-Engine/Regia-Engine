// SPDX-License-Identifier: MPL-2.0
//! Look materials: what the cheap shaders need, mapped from material library entries
//! (`jewelry.material_library/1`). Every value is data; nothing here names a product or a maker.

use bytemuck::{Pod, Zeroable};
use serde_json::Value;

/// Which fragment shader a material uses. One pipeline per kind; a swap between materials of the
/// same kind only changes uniform data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LookKind {
    /// Polished or rough metal (also anodized metal, through the color table).
    Metal,
    /// Faceted transparent stone.
    Gem,
    /// Smooth domed transparent or translucent stone.
    Cabochon,
    /// Play-of-color stone.
    Opal,
    /// Faceted stone with a frosted (sandblasted) surface.
    Frosted,
}

impl LookKind {
    /// Every kind, in pipeline order.
    pub const ALL: [LookKind; 5] = [
        LookKind::Metal,
        LookKind::Gem,
        LookKind::Cabochon,
        LookKind::Opal,
        LookKind::Frosted,
    ];

    /// The WGSL fragment entry point of this kind.
    pub fn fragment_entry(self) -> &'static str {
        match self {
            LookKind::Metal => "fs_metal",
            LookKind::Gem => "fs_gem",
            LookKind::Cabochon => "fs_cabochon",
            LookKind::Opal => "fs_opal",
            LookKind::Frosted => "fs_frosted",
        }
    }

    /// Lowercase name used by the agent surface.
    pub fn as_str(self) -> &'static str {
        match self {
            LookKind::Metal => "metal",
            LookKind::Gem => "gem",
            LookKind::Cabochon => "cabochon",
            LookKind::Opal => "opal",
            LookKind::Frosted => "frosted",
        }
    }
}

/// A metal look: Schlick Fresnel from F0 with the optional F82 tint (Hoffman 2023), or an anodize
/// color table in place of the Fresnel term.
#[derive(Debug, Clone, PartialEq)]
pub struct MetalLook {
    /// Normal-incidence reflectance, linear Rec.709 (may exceed 1 in red for gold).
    pub f0: [f32; 3],
    /// F82 tint (1, 1, 1 is plain Schlick).
    pub f82_tint: [f32; 3],
    /// Perceptual roughness 0..1.
    pub roughness: f32,
    /// Anodize table (`cos theta -> linear RGB`, entry `i` at cos theta = i / (n - 1)), if anodized.
    pub anodize_lut: Option<Vec<[f32; 3]>>,
    /// Grain size of a blasted finish in millimeters (the finish's `feature_scale_mm`), 0 for none.
    pub grain_mm: f32,
}

/// A transparent stone look (faceted, cabochon or frosted).
#[derive(Debug, Clone, PartialEq)]
pub struct GemLook {
    /// Refractive index at the sodium d line.
    pub n_d: f32,
    /// Abbe number; the fake dispersion spreads the index by `(n_d - 1) / abbe` across RGB.
    pub abbe: f32,
    /// Absorption per millimeter, linear RGB.
    pub attenuation_per_mm: [f32; 3],
    /// Surface roughness (0 polished; frosted stones use 0.4 to 0.7).
    pub roughness: f32,
    /// Multiplier on the fake dispersion (1 = the Abbe number's spread).
    pub fire: f32,
}

/// A play-of-color stone look.
#[derive(Debug, Clone, PartialEq)]
pub struct OpalLook {
    /// Body color, linear Rec.709.
    pub body: [f32; 3],
    /// Patch size of the color play in millimeters.
    pub patch_mm: f32,
    /// Strength of the color play, 0..1.
    pub play: f32,
}

/// One material as the cheap shaders take it.
#[derive(Debug, Clone, PartialEq)]
pub enum LookMaterial {
    /// Metal or anodized metal.
    Metal(MetalLook),
    /// Faceted stone.
    Gem(GemLook),
    /// Smooth domed stone.
    Cabochon(GemLook),
    /// Faceted stone with a frosted surface.
    Frosted(GemLook),
    /// Play-of-color stone.
    Opal(OpalLook),
}

/// Default multiplier on the fake dispersion (a studio setting, not a material value).
pub const DEFAULT_FIRE: f32 = 1.5;

/// A gem whose surface roughness is at least this draws with the frosted shader.
pub const FROSTED_ROUGHNESS: f32 = 0.25;

/// Which virtual pavilion a faceted stone shows. The cut decides it, not the material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FacetFamily {
    /// The round cut's cone cut into mains and lower halves by azimuth.
    #[default]
    Brilliant,
    /// A four-sided pyramid in chevron tiers: princess and other square or step outlines.
    Step,
}

impl FacetFamily {
    /// Lowercase name used by the agent surface.
    pub fn as_str(self) -> &'static str {
        match self {
            FacetFamily::Brilliant => "brilliant",
            FacetFamily::Step => "step",
        }
    }

    /// The family a cut's name shows: square and rectangular step outlines are [`FacetFamily::Step`], and
    /// everything else, including a cut this crate has no stand-in for, is [`FacetFamily::Brilliant`].
    pub fn of_cut(cut_type: &str) -> FacetFamily {
        match cut_type {
            "princess" | "square" | "emerald" | "asscher" | "baguette" => FacetFamily::Step,
            _ => FacetFamily::Brilliant,
        }
    }
}

/// The size of a stone as the stone shaders need it (metals ignore it).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StoneSize {
    /// Table-to-culet depth (faceted) or dome height (cabochon), millimeters.
    pub depth_mm: f32,
    /// Half the outline length (along the cut's y), millimeters.
    pub half_length_mm: f32,
    /// Half the outline width (along the cut's x), millimeters.
    pub half_width_mm: f32,
    /// A turn of the virtual facets about the stone axis, radians (keeps stones of a pave field
    /// from sparkling in lockstep).
    pub turn: f32,
    /// The virtual pavilion the stone shows.
    pub facets: FacetFamily,
}

impl StoneSize {
    /// The size of a round stone or cabochon of diameter `d_mm` and depth `depth_mm`.
    pub fn round(d_mm: f32, depth_mm: f32) -> StoneSize {
        StoneSize {
            depth_mm,
            half_length_mm: d_mm * 0.5,
            half_width_mm: d_mm * 0.5,
            turn: 0.0,
            facets: FacetFamily::Brilliant,
        }
    }

    /// The size of a stone from its mesh's bounds, whichever way its outline lies: `half_length_mm` is the half
    /// extent along y and `half_width_mm` along x, which is how the shaders read them. A package's stand-in cuts
    /// run their length along x; a kit's may not, so a player reads the mesh and not the nominal size.
    pub fn from_bounds(lo: [f32; 3], hi: [f32; 3], facets: FacetFamily) -> StoneSize {
        StoneSize {
            depth_mm: (hi[2] - lo[2]).abs(),
            half_length_mm: (hi[1] - lo[1]).abs() * 0.5,
            half_width_mm: (hi[0] - lo[0]).abs() * 0.5,
            turn: 0.0,
            facets,
        }
    }
}

fn finite_f32(x: f32, fallback: f32) -> f32 {
    if x.is_finite() { x } else { fallback }
}

fn finite3(c: [f32; 3], fallback: f32) -> [f32; 3] {
    [
        finite_f32(c[0], fallback),
        finite_f32(c[1], fallback),
        finite_f32(c[2], fallback),
    ]
}

/// Per-draw material parameters, std140-compatible (four `vec4`s), shared by every kind.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct MaterialParams {
    /// Metal: f0.rgb, roughness. Stones: attenuation.rgb per mm, roughness. Opal: body.rgb, patch mm.
    pub p0: [f32; 4],
    /// Metal: f82 tint.rgb, anodize row (-1 none). Stones: n_d, abbe, fire, path mm. Opal: play, 0, 0, 0.
    pub p1: [f32; 4],
    /// Stones: half-extent xyz in mm (the fake pavilion scale), facet sparkle strength. Metal: grain mm, 0, 0, 0.
    pub p2: [f32; 4],
    /// Stones: turn of the virtual facets, facet family (0 brilliant, 1 step), 0, 0.
    pub p3: [f32; 4],
}

impl LookMaterial {
    /// The shader kind.
    pub fn kind(&self) -> LookKind {
        match self {
            LookMaterial::Metal(_) => LookKind::Metal,
            LookMaterial::Gem(_) => LookKind::Gem,
            LookMaterial::Cabochon(_) => LookKind::Cabochon,
            LookMaterial::Frosted(_) => LookKind::Frosted,
            LookMaterial::Opal(_) => LookKind::Opal,
        }
    }

    /// Uniform parameters. `stone` sizes the fake internal path of stones (metals ignore it);
    /// `lut_row` is this material's row in the anodize table texture, if any.
    pub fn params(&self, stone: StoneSize, lut_row: Option<u32>) -> MaterialParams {
        let z = [0.0; 4];
        let depth = finite_f32(stone.depth_mm, 0.0);
        let half_l = finite_f32(stone.half_length_mm, 0.0);
        let half_w = finite_f32(stone.half_width_mm, 0.0);
        match self {
            LookMaterial::Metal(m) => {
                let f0 = finite3(m.f0, 0.0);
                let f82 = finite3(m.f82_tint, 1.0);
                MaterialParams {
                    p0: [f0[0], f0[1], f0[2], finite_f32(m.roughness, 0.12)],
                    p1: [f82[0], f82[1], f82[2], lut_row.map_or(-1.0, |r| r as f32)],
                    p2: [finite_f32(m.grain_mm, 0.0), 0.0, 0.0, 0.0],
                    p3: z,
                }
            }
            LookMaterial::Gem(g) | LookMaterial::Cabochon(g) | LookMaterial::Frosted(g) => {
                // Absorption is never negative: the shaders tint by `exp(-att * path)`, so a negative value
                // brightens the stone without bound (and overflows to infinity for a large one).
                let att = finite3(g.attenuation_per_mm, 0.0).map(|a| a.max(0.0));
                let n_d = if g.n_d.is_finite() && g.n_d >= 1.0 {
                    g.n_d
                } else {
                    1.5
                };
                let abbe = if g.abbe.is_finite() { g.abbe.max(1.0) } else { 60.0 };
                MaterialParams {
                    p0: [att[0], att[1], att[2], finite_f32(g.roughness, 0.0)],
                    p1: [n_d, abbe, finite_f32(g.fire, DEFAULT_FIRE), 2.0 * depth],
                    p2: [depth, half_l, half_w, 1.0],
                    p3: [
                        finite_f32(stone.turn, 0.0),
                        match stone.facets {
                            FacetFamily::Brilliant => 0.0,
                            FacetFamily::Step => 1.0,
                        },
                        0.0,
                        0.0,
                    ],
                }
            }
            LookMaterial::Opal(o) => {
                let body = finite3(o.body, 0.0);
                MaterialParams {
                    p0: [body[0], body[1], body[2], finite_f32(o.patch_mm, 0.0)],
                    p1: [finite_f32(o.play, 0.0), 0.0, 0.0, 0.0],
                    p2: [depth, half_l, half_w, 0.0],
                    p3: z,
                }
            }
        }
    }

    /// The material as drawn on a cut: a clear stone on a cabochon (smooth dome) cut uses the
    /// cabochon shader; on a faceted cut it keeps its gem or frosted shader. The cut, not the
    /// library entry, decides the shape.
    pub fn for_cut(self, faceted: bool) -> LookMaterial {
        match (self, faceted) {
            (LookMaterial::Gem(g) | LookMaterial::Frosted(g), false) => LookMaterial::Cabochon(g),
            (other, _) => other,
        }
    }

    /// The anodize table, if this is an anodized metal.
    pub fn anodize_lut(&self) -> Option<&[[f32; 3]]> {
        match self {
            LookMaterial::Metal(MetalLook {
                anodize_lut: Some(lut),
                ..
            }) => Some(lut),
            _ => None,
        }
    }
}

/// Why a library entry could not become a look material.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LookError {
    /// The library JSON did not parse.
    #[error("library_parse: {0}")]
    Parse(String),
    /// The library is not `jewelry.material_library/1`.
    #[error("library_schema: expected jewelry.material_library/1, found {0}")]
    Schema(String),
    /// A field the look needs is missing or has the wrong type.
    #[error("material_missing_field: {material}: {field}")]
    MissingField {
        /// The entry's `material_id`.
        material: String,
        /// The missing field path.
        field: String,
    },
    /// No entry has that id.
    #[error("material_unknown: {0}")]
    Unknown(String),
    /// The entry's kind has no look (a `finish` alone is a modifier, not a material).
    #[error("material_not_drawable: {0} ({1})")]
    NotDrawable(String, String),
}

impl LookError {
    /// The stable lowercase error code (agent protocol).
    pub fn code(&self) -> &'static str {
        match self {
            LookError::Parse(_) => "library_parse",
            LookError::Schema(_) => "library_schema",
            LookError::MissingField { .. } => "material_missing_field",
            LookError::Unknown(_) => "material_unknown",
            LookError::NotDrawable(..) => "material_not_drawable",
        }
    }
}

fn rgb(v: Option<&Value>) -> Option<[f32; 3]> {
    let a = v?.as_array()?;
    if a.len() != 3 {
        return None;
    }
    Some([
        a[0].as_f64()? as f32,
        a[1].as_f64()? as f32,
        a[2].as_f64()? as f32,
    ])
}

/// Linear RGB attenuation from a three-or-more-sample absorption spectrum, read at 610, 545 and
/// 465 nm (rough centers of the Rec.709 primaries' responses).
fn spectrum_rgb(spec: &Value) -> Option<[f32; 3]> {
    // Every sample must be a number: skipping a non-number in one list shifts every later pair.
    let w: Vec<f64> = spec
        .get("wavelength_nm")?
        .as_array()?
        .iter()
        .map(Value::as_f64)
        .collect::<Option<_>>()?;
    let k: Vec<f64> = spec
        .get("per_mm")?
        .as_array()?
        .iter()
        .map(Value::as_f64)
        .collect::<Option<_>>()?;
    if w.is_empty() || w.len() != k.len() {
        return None;
    }
    // The format requires strictly increasing wavelengths. A decrease or a repeat picks the wrong
    // sample (the search assumes order) and is refused so the caller can report a missing field.
    // `partial_cmp` treats NaN as unordered, so a non-finite sample fails this check too.
    if w.windows(2)
        .any(|p| p[0].partial_cmp(&p[1]) != Some(std::cmp::Ordering::Less))
        || w.iter().chain(k.iter()).any(|x| !x.is_finite())
    {
        return None;
    }
    let at = |l: f64| {
        let i = w.partition_point(|x| *x < l);
        if i == 0 {
            return k[0];
        }
        if i >= w.len() {
            return k[w.len() - 1];
        }
        let t = (l - w[i - 1]) / (w[i] - w[i - 1]);
        k[i - 1] + (k[i] - k[i - 1]) * t
    };
    Some([at(610.0) as f32, at(545.0) as f32, at(465.0) as f32])
}

/// A parsed material library: every entry's JSON, by id, in file order.
#[derive(Debug, Clone, PartialEq)]
pub struct Library {
    entries: Vec<(String, Value)>,
}

/// The built-in stand-in library (generic metals, anodize colors and stones), in the
/// `jewelry.material_library/1` shape. Values and sources: `docs/provenance.md` (ADR-2538).
pub const BUILTIN_LIBRARY: &str = include_str!("../data/library-looks.json");

impl Library {
    /// Parses a library file.
    pub fn parse(text: &str) -> Result<Library, LookError> {
        let v: Value = serde_json::from_str(text).map_err(|e| LookError::Parse(e.to_string()))?;
        let schema = v.get("schema").and_then(Value::as_str).unwrap_or("");
        if schema != "jewelry.material_library/1" {
            return Err(LookError::Schema(schema.to_string()));
        }
        let mats = v
            .get("materials")
            .and_then(Value::as_array)
            .ok_or_else(|| LookError::MissingField {
                material: String::new(),
                field: "materials".into(),
            })?;
        let mut entries = Vec::with_capacity(mats.len());
        for m in mats {
            let id = m
                .get("material_id")
                .and_then(Value::as_str)
                .ok_or_else(|| LookError::MissingField {
                    material: String::new(),
                    field: "material_id".into(),
                })?;
            entries.push((id.to_string(), m.clone()));
        }
        Ok(Library { entries })
    }

    /// The built-in stand-in library.
    pub fn builtin() -> Result<Library, LookError> {
        Library::parse(BUILTIN_LIBRARY)
    }

    /// Every material id, in file order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(id, _)| id.as_str())
    }

    /// The raw JSON entry.
    pub fn entry(&self, id: &str) -> Option<&Value> {
        self.entries.iter().find(|(i, _)| i == id).map(|(_, v)| v)
    }

    /// The entry whose `catalogue_facets` contains `facet` (`<group>=<label>`).
    pub fn by_facet(&self, facet: &str) -> Option<&str> {
        self.entries.iter().find_map(|(id, v)| {
            let facets = v.get("catalogue_facets")?.as_array()?;
            facets
                .iter()
                .any(|f| f.as_str() == Some(facet))
                .then_some(id.as_str())
        })
    }

    /// Maps an entry to a look. `finish` overrides a metal's roughness when given (a finish entry id).
    /// An anodized entry follows its substrate. A cycle, or a chain of more than eight substrates,
    /// is [`LookError::NotDrawable`] (code `material_not_drawable`) instead of recursing without a bound.
    pub fn look(&self, id: &str, finish: Option<&str>) -> Result<LookMaterial, LookError> {
        self.look_at(id, finish, 0)
    }

    fn look_at(&self, id: &str, finish: Option<&str>, depth: u32) -> Result<LookMaterial, LookError> {
        const ANODIZE_SUBSTRATE_LIMIT: u32 = 8;
        let missing = |field: &str| LookError::MissingField {
            material: id.to_string(),
            field: field.to_string(),
        };
        let kind = {
            let v = self.entry(id).ok_or_else(|| LookError::Unknown(id.to_string()))?;
            v.get("kind").and_then(Value::as_str).unwrap_or("").to_string()
        };
        let (finish_roughness, grain_mm) = match finish {
            Some(f) => {
                let e = self.entry(f).ok_or_else(|| LookError::Unknown(f.to_string()))?;
                let roughness = e
                    .pointer("/finish/roughness")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| LookError::MissingField {
                        material: f.to_string(),
                        field: "finish.roughness".into(),
                    })? as f32;
                // `feature_scale_mm` is null on smooth finishes; only a blasted finish draws a grain.
                let grain = if e.pointer("/finish/finish_kind").and_then(Value::as_str) == Some("sandblast") {
                    e.pointer("/finish/feature_scale_mm")
                        .and_then(Value::as_f64)
                        .map_or(0.0, |g| g as f32)
                } else {
                    0.0
                };
                (Some(roughness), grain)
            }
            None => (None, 0.0),
        };
        let v = self.entry(id).ok_or_else(|| LookError::Unknown(id.to_string()))?;
        match kind.as_str() {
            "conductor" => {
                let c = v.get("conductor").ok_or_else(|| missing("conductor"))?;
                let f0 = finite3(
                    rgb(c.get("f0_rgb_linear")).ok_or_else(|| missing("conductor.f0_rgb_linear"))?,
                    0.0,
                );
                let f82 = finite3(rgb(c.get("f82_tint_rgb")).unwrap_or([1.0; 3]), 1.0);
                Ok(LookMaterial::Metal(MetalLook {
                    f0,
                    f82_tint: f82,
                    roughness: finite_f32(finish_roughness.unwrap_or(0.12), 0.12),
                    anodize_lut: None,
                    grain_mm: finite_f32(grain_mm, 0.0),
                }))
            }
            "anodized" => {
                if depth >= ANODIZE_SUBSTRATE_LIMIT {
                    return Err(LookError::NotDrawable(
                        id.to_string(),
                        "anodize substrate cycle".into(),
                    ));
                }
                let a = v.get("anodized").ok_or_else(|| missing("anodized"))?.clone();
                let sub_id = a
                    .get("substrate_material_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| missing("anodized.substrate_material_id"))?
                    .to_string();
                let base = match self.look_at(&sub_id, None, depth + 1)? {
                    LookMaterial::Metal(m) => m,
                    other => {
                        return Err(LookError::NotDrawable(sub_id, other.kind().as_str().to_string()));
                    }
                };
                let entries = a
                    .pointer("/fresnel_lut/entries")
                    .and_then(Value::as_array)
                    .ok_or_else(|| missing("anodized.fresnel_lut.entries"))?;
                // A number too large for `f32` (`1e39`) reads as infinity; the standalone renderer's half-float
                // table keeps infinity, and the tone map turns it into NaN. Same policy as every other channel.
                let lut: Option<Vec<[f32; 3]>> = entries
                    .iter()
                    .map(|e| rgb(Some(e)).map(|c| finite3(c, 0.0)))
                    .collect();
                let lut = lut
                    .filter(|l| l.len() >= 2)
                    .ok_or_else(|| missing("anodized.fresnel_lut.entries"))?;
                Ok(LookMaterial::Metal(MetalLook {
                    roughness: finite_f32(finish_roughness.unwrap_or(base.roughness), base.roughness),
                    anodize_lut: Some(lut),
                    grain_mm: finite_f32(grain_mm, 0.0),
                    ..base
                }))
            }
            "gem_dielectric" => {
                let g = v.get("gem_dielectric").ok_or_else(|| missing("gem_dielectric"))?;
                let n_d = g
                    .get("n_d")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| missing("gem_dielectric.n_d"))? as f32;
                let n_d = if n_d.is_finite() { n_d } else { 1.5 };
                let abbe = g
                    .get("abbe_vd")
                    .and_then(Value::as_f64)
                    .map(|a| a as f32)
                    .unwrap_or(60.0);
                let abbe = if abbe.is_finite() { abbe } else { 60.0 };
                let att = rgb(g.get("attenuation_rgb_per_mm"))
                    .or_else(|| g.get("absorption_spectrum").and_then(spectrum_rgb))
                    .ok_or_else(|| missing("gem_dielectric.attenuation_rgb_per_mm"))?;
                let att = finite3(att, 0.0);
                let roughness = g.get("surface_roughness").and_then(Value::as_f64).unwrap_or(0.0) as f32;
                let roughness = finite_f32(roughness, 0.0);
                let look = GemLook {
                    n_d,
                    abbe,
                    attenuation_per_mm: att,
                    roughness,
                    fire: DEFAULT_FIRE,
                };
                Ok(if roughness >= FROSTED_ROUGHNESS {
                    LookMaterial::Frosted(look)
                } else {
                    LookMaterial::Gem(look)
                })
            }
            "gem_special" => {
                let s = v.get("gem_special").ok_or_else(|| missing("gem_special"))?;
                let body = finite3(
                    rgb(s.get("body_rgb_linear")).ok_or_else(|| missing("gem_special.body_rgb_linear"))?,
                    0.0,
                );
                let model = s.get("model").and_then(Value::as_str).unwrap_or("");
                match model {
                    "opal" => Ok(LookMaterial::Opal(OpalLook {
                        body,
                        patch_mm: finite_f32(
                            s.get("patch_scale_mm").and_then(Value::as_f64).unwrap_or(0.5) as f32,
                            0.5,
                        ),
                        play: finite_f32(
                            s.get("orient_strength")
                                .and_then(Value::as_f64)
                                .map_or(0.8, |x| x as f32),
                            0.8,
                        ),
                    })),
                    // Pearls and phenomenal stones draw as an opaque-ish cabochon of their body color.
                    _ => Ok(LookMaterial::Cabochon(GemLook {
                        n_d: 1.53,
                        abbe: 60.0,
                        attenuation_per_mm: body.map(|c| (1.0 - c.clamp(0.0, 1.0)) * 4.0 + 0.6),
                        roughness: 0.1,
                        fire: 0.0,
                    })),
                }
            }
            other => Err(LookError::NotDrawable(id.to_string(), other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_library_maps_every_drawable_entry() {
        let lib = Library::builtin().expect("builtin library parses");
        let mut kinds = std::collections::BTreeSet::new();
        for id in lib.ids() {
            match lib.look(id, None) {
                Ok(m) => {
                    kinds.insert(m.kind());
                }
                Err(LookError::NotDrawable(_, k)) => assert_eq!(k, "finish", "{id}"),
                Err(e) => panic!("{id}: {e}"),
            }
        }
        kinds.insert(
            lib.look("sapphire-blue", None)
                .expect("sapphire")
                .for_cut(false)
                .kind(),
        );
        assert_eq!(
            kinds.len(),
            LookKind::ALL.len(),
            "every kind has a stand-in: {kinds:?}"
        );
    }

    #[test]
    fn finish_overrides_roughness() {
        let lib = Library::builtin().expect("parses");
        let polished = lib.look("gold-yellow-18k", None).expect("gold");
        let brushed = lib
            .look("gold-yellow-18k", Some("finish-brushed"))
            .expect("gold brushed");
        match (polished, brushed) {
            (LookMaterial::Metal(a), LookMaterial::Metal(b)) => assert!(b.roughness > a.roughness),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn only_a_blasted_finish_has_a_grain() {
        let lib = Library::builtin().expect("parses");
        let grain = |finish: &str, id: &str| match lib.look(id, Some(finish)).expect("metal") {
            LookMaterial::Metal(m) => m.grain_mm,
            other => panic!("{other:?}"),
        };
        assert_eq!(grain("finish-polish", "steel"), 0.0);
        assert_eq!(grain("finish-brushed", "steel"), 0.0);
        assert!(grain("finish-sandblast", "steel") > 0.0);
        assert!(grain("finish-sandblast", "titanium-anodized-blue") > 0.0);
        let block = lib
            .look("steel", Some("finish-sandblast"))
            .expect("steel")
            .params(StoneSize::default(), None);
        assert!(block.p2[0] > 0.0, "the grain reaches the shader block: {block:?}");
    }

    #[test]
    fn gold_gets_more_saturated_with_karat() {
        let lib = Library::builtin().expect("parses");
        let saturation = |id: &str| match lib.look(id, None).expect("gold") {
            LookMaterial::Metal(m) => 1.0 - m.f0[2] / m.f0[0],
            other => panic!("{other:?}"),
        };
        let (k24, k18, k14) = (
            saturation("gold-yellow-24k"),
            saturation("gold-yellow-18k"),
            saturation("gold-yellow-14k"),
        );
        assert!(k24 > k18 && k18 > k14, "24K {k24} 18K {k18} 14K {k14}");
    }

    #[test]
    fn the_facet_family_reaches_the_shader_block() {
        let lib = Library::builtin().expect("parses");
        let gem = lib.look("emerald-green", None).expect("gem");
        let step = StoneSize {
            facets: FacetFamily::Step,
            ..StoneSize::round(3.0, 1.8)
        };
        assert_eq!(gem.params(step, None).p3[1], 1.0);
        assert_eq!(gem.params(StoneSize::round(3.0, 1.8), None).p3[1], 0.0);
    }

    #[test]
    fn errors_have_stable_codes() {
        assert_eq!(
            Library::parse("{").err().map_or("ok", |e| e.code()),
            "library_parse"
        );
        assert_eq!(
            Library::parse(r#"{"schema":"x"}"#)
                .err()
                .map_or("ok", |e| e.code()),
            "library_schema"
        );
        let lib = Library::builtin().expect("parses");
        assert_eq!(
            lib.look("nope", None).err().map_or("ok", |e| e.code()),
            "material_unknown"
        );
        assert_eq!(
            lib.look("finish-brushed", None).err().map_or("ok", |e| e.code()),
            "material_not_drawable"
        );
        let bad = r#"{"schema":"jewelry.material_library/1","materials":[{"material_id":"m","kind":"conductor","conductor":{}}]}"#;
        assert_eq!(
            Library::parse(bad)
                .expect("parses")
                .look("m", None)
                .err()
                .map_or("ok", |e| e.code()),
            "material_missing_field"
        );
    }

    #[test]
    fn absorption_spectrum_maps_to_rgb() {
        let spec: Value =
            serde_json::json!({"wavelength_nm": [450.0, 550.0, 650.0], "per_mm": [0.6, 0.2, 0.05]});
        let rgb = spectrum_rgb(&spec).expect("spectrum");
        assert!(rgb[2] > rgb[1] && rgb[1] > rgb[0], "{rgb:?}");
    }

    #[test]
    fn a_stone_size_reads_the_mesh_either_way_round() {
        let along_x = StoneSize::from_bounds([-2.0, -1.0, -1.2], [2.0, 1.0, 0.6], FacetFamily::Brilliant);
        assert_eq!(along_x.half_width_mm, 2.0, "x extent");
        assert_eq!(along_x.half_length_mm, 1.0, "y extent");
        assert!((along_x.depth_mm - 1.8).abs() < 1e-6);
    }

    #[test]
    fn step_outlines_show_the_step_pavilion() {
        for cut in ["princess", "square", "emerald", "asscher", "baguette"] {
            assert_eq!(FacetFamily::of_cut(cut), FacetFamily::Step, "{cut}");
        }
        for cut in ["round_brilliant", "marquise", "pear", "heart", "oval", "unknown"] {
            assert_eq!(FacetFamily::of_cut(cut), FacetFamily::Brilliant, "{cut}");
        }
    }

    #[test]
    fn params_fit_the_uniform_budget() {
        assert_eq!(std::mem::size_of::<MaterialParams>(), 64);
    }

    #[test]
    fn swapped_bounds_keep_positive_stone_size() {
        let swapped = StoneSize::from_bounds([2.0, 1.0, 0.6], [-2.0, -1.0, -1.2], FacetFamily::Brilliant);
        assert!(swapped.depth_mm > 0.0, "depth {}", swapped.depth_mm);
        assert_eq!(swapped.half_width_mm, 2.0);
        assert_eq!(swapped.half_length_mm, 1.0);
    }

    #[test]
    fn decreasing_absorption_spectrum_is_refused() {
        let spec = serde_json::json!({"wavelength_nm": [700.0, 400.0], "per_mm": [0.0, 10.0]});
        assert!(spectrum_rgb(&spec).is_none());
    }

    #[test]
    fn non_finite_look_inputs_do_not_reach_the_shader_block() {
        let gem = LookMaterial::Gem(GemLook {
            n_d: f32::NAN,
            abbe: f32::NAN,
            attenuation_per_mm: [f32::INFINITY, 0.0, 0.0],
            roughness: f32::NAN,
            fire: f32::NAN,
        });
        let stone = StoneSize::round(f32::NAN, 1.0);
        let p = gem.params(stone, None);
        for (v, name) in p.p0.iter().chain(&p.p1).chain(&p.p2).chain(&p.p3).zip([
            "p0.0", "p0.1", "p0.2", "p0.3", "p1.0", "p1.1", "p1.2", "p1.3", "p2.0", "p2.1", "p2.2", "p2.3",
            "p3.0", "p3.1", "p3.2", "p3.3",
        ]) {
            assert!(v.is_finite(), "{name} = {v}");
        }
        assert_eq!(p.p1[0], 1.5, "n_d");
        assert_eq!(p.p1[1], 60.0, "abbe");
        assert_eq!(p.p1[2], DEFAULT_FIRE, "fire");
        assert_eq!(p.p0[3], 0.0, "gem roughness");
        assert_eq!(p.p0[0], 0.0, "non-finite attenuation");
        assert_eq!(p.p2[1], 0.0, "half length");
        assert_eq!(p.p2[2], 0.0, "half width");
        let metal = LookMaterial::Metal(MetalLook {
            f0: [1.2, f32::NAN, 0.4],
            f82_tint: [f32::NAN, 1.0, 1.0],
            roughness: f32::NAN,
            anodize_lut: None,
            grain_mm: 0.04,
        });
        let m = metal.params(StoneSize::default(), None);
        assert_eq!(m.p0[0], 1.2, "finite f0 above 1 stays");
        assert_eq!(m.p0[1], 0.0);
        assert_eq!(m.p1[0], 1.0, "non-finite f82");
        assert_eq!(m.p0[3], 0.12, "metal roughness");
        assert_eq!(m.p2[0], 0.04);
        let diamond = Library::builtin()
            .expect("library")
            .look("diamond-white", None)
            .expect("diamond")
            .params(StoneSize::round(3.0, 1.8), None);
        assert_eq!(diamond.p1[0], 2.417);
    }

    #[test]
    fn anodize_substrate_cycle_is_not_drawable() {
        let text = r#"{"schema":"jewelry.material_library/1","materials":[
            {"material_id":"loop","kind":"anodized","anodized":{
                "substrate_material_id":"loop",
                "fresnel_lut":{"entries":[[1.0,1.0,1.0],[1.0,1.0,1.0]]}
            }}
        ]}"#;
        let err = Library::parse(text)
            .expect("parses")
            .look("loop", None)
            .expect_err("cycle");
        assert_eq!(err.code(), "material_not_drawable");
    }

    #[test]
    fn an_anodize_table_entry_too_large_for_f32_stays_finite() {
        // 1e39 is a finite JSON number that casts to f32 infinity.
        let text = r#"{"schema":"jewelry.material_library/1","materials":[
            {"material_id":"base","kind":"conductor","conductor":{"f0_rgb_linear":[0.5,0.5,0.5]}},
            {"material_id":"ano","kind":"anodized","anodized":{
                "substrate_material_id":"base",
                "fresnel_lut":{"entries":[[1e39,0.2,0.3],[0.4,-1e39,0.6]]}
            }}
        ]}"#;
        let look = Library::parse(text)
            .expect("parses")
            .look("ano", None)
            .expect("anodized");
        let lut = look.anodize_lut().expect("lut");
        assert!(
            lut.iter().flatten().all(|c| c.is_finite()),
            "every table value is finite: {lut:?}"
        );
        assert_eq!(lut[0], [0.0, 0.2, 0.3]);
        assert_eq!(lut[1], [0.4, 0.0, 0.6]);
    }

    #[test]
    fn negative_attenuation_never_brightens_a_stone() {
        let gem = LookMaterial::Gem(GemLook {
            n_d: 1.54,
            abbe: 60.0,
            attenuation_per_mm: [-3.0, 0.2, -1e30],
            roughness: 0.0,
            fire: DEFAULT_FIRE,
        });
        let p = gem.params(StoneSize::round(3.0, 1.8), None);
        assert_eq!(&p.p0[..3], &[0.0, 0.2, 0.0], "{p:?}");
        let cab = gem.for_cut(false).params(StoneSize::round(3.0, 1.8), None);
        assert!(cab.p0[..3].iter().all(|a| *a >= 0.0), "{cab:?}");
    }

    #[test]
    fn a_non_number_in_an_absorption_spectrum_is_refused_not_skipped() {
        // Skipping "x" would pair 650 nm with 0.2 (the 550 nm sample).
        let spec = serde_json::json!({"wavelength_nm": [450.0, 550.0, 650.0], "per_mm": [0.1, 0.2, "x"]});
        assert!(spectrum_rgb(&spec).is_none());
        let shifted = serde_json::json!({"wavelength_nm": [450.0, "x", 650.0], "per_mm": [0.1, 0.2, "y"]});
        assert!(spectrum_rgb(&shifted).is_none());
    }

    #[test]
    fn a_short_anodize_chain_still_resolves() {
        let text = r#"{"schema":"jewelry.material_library/1","materials":[
            {"material_id":"base","kind":"conductor","conductor":{"f0_rgb_linear":[0.5,0.5,0.5]}},
            {"material_id":"mid","kind":"anodized","anodized":{
                "substrate_material_id":"base",
                "fresnel_lut":{"entries":[[1.0,0.0,0.0],[0.0,1.0,0.0]]}
            }},
            {"material_id":"top","kind":"anodized","anodized":{
                "substrate_material_id":"mid",
                "fresnel_lut":{"entries":[[0.0,0.0,1.0],[1.0,1.0,0.0]]}
            }}
        ]}"#;
        let lib = Library::parse(text).expect("parses");
        match lib.look("top", None).expect("chain") {
            LookMaterial::Metal(m) => {
                let lut = m.anodize_lut.expect("lut");
                assert_eq!(lut.len(), 2);
                assert_eq!(lut[0], [0.0, 0.0, 1.0]);
                assert_eq!(m.f0, [0.5, 0.5, 0.5]);
            }
            other => panic!("{other:?}"),
        }
    }
}
