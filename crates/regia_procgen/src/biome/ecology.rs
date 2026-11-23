// SPDX-License-Identifier: MPL-2.0
//! Plausible ecology for biomes (ADR-2456, book `agents/biomes.md` "Ecology").
//!
//! A biome (`.biome.ron`) says what to place; this module says whether that mix could grow together.
//! Three parts:
//!
//! - **Species traits** ([`Species`], [`SpeciesCatalog::builtin`]): per species the soil moisture and soil
//!   reaction (acidity) windows it occurs in on the indicator-value scales of the public botany
//!   literature (moisture 1-10, reaction 1-9, light 1-9; `docs/provenance.md`), its indicator class, the
//!   light it needs, its shade tolerance (1-5) and the shade it casts, its altitude band, the steepest
//!   ground it holds, its forest stratum and succession stage.
//! - **Ecology profiles** (`<name>.ecology.ron`, [`EcologyProfile`], format `ecology` v1): the site a
//!   biome stands for (moisture, reaction, altitude, slope, soil label), which entry is which species in
//!   which role (dominant, associated, understory) with its share of the layer, and the opt-in
//!   competition rules the `biome.ecology` node applies. The `.biome.ron` format is unchanged; a biome
//!   without a profile is checked by species names alone.
//! - **The validator** ([`validate_ecology`]) and the **competition pass** ([`competition_pass`]).
//!
//! The validator flags implausible mixes with the reason (`ecology_*` codes, [`EcologyIssue`]): a species
//! whose moisture, reaction, altitude or slope window misses the site; two species that never share a
//! site; a light-demanding understory under a deep-shade canopy; a pioneer sharing dominance with a
//! deep-shade late-successional tree; a biome named after a tree that does not dominate it; shares that
//! disagree with the entry weights. Without a profile the site is inferred from the dominant's
//! indicator class (the tree the biome is named after, else the canopy entry with the largest weight).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use regia_core::version::{VersionStep, migrate};
use serde::{Deserialize, Serialize};

use super::def::{BiomeDef, EntryFilter};

/// Current `.ecology.ron` format version.
pub const ECOLOGY_FORMAT_VERSION: u32 = 1;

/// Migration steps of `.ecology.ron` (none yet: version 1 is the first).
pub const ECOLOGY_MIGRATIONS: &[VersionStep<ron::Value>] = &[];

/// The `format_version` values the migrator reads (none: version 1 is the first).
pub fn ecology_migration_sources() -> Vec<u32> {
    ECOLOGY_MIGRATIONS.iter().map(|s| s.from).collect()
}

/// A field-name fingerprint of the format for `check-formats`: every struct and enum with its fields or
/// variants, in declaration order.
pub fn ecology_schema_fingerprint() -> String {
    [
        "EcologyProfile{format_version,site,species,competition,custom_species}",
        "Site{moisture,reaction,altitude_m,slope_deg,soil}",
        "SpeciesUse{entry,species,role,share}",
        "Role{Dominant,Associated,Understory}",
        "Competition{shade_rules,self_thinning}",
        "SelfThinning{cell_m,max_stems_per_ha,exponent}",
        "Species{id,common_name,group,aliases,stratum,moisture,reaction,indicator,light,shade_tolerance,casts_shade,altitude_m,max_slope_deg,succession,vernal}",
        "Stratum{Canopy,Subcanopy,Shrub,Field,Ground}",
        "Succession{Pioneer,Mid,Late}",
    ]
    .join(";")
}

/// The companion profile path of a biome: `a/b.biome.ron` → `a/b.ecology.ron`.
pub fn ecology_path_for(biome_path: &str) -> String {
    match biome_path.strip_suffix(".biome.ron") {
        Some(stem) => format!("{stem}.ecology.ron"),
        None => format!("{biome_path}.ecology.ron"),
    }
}

/// The forest stratum a species grows in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Stratum {
    /// Tall trees forming the canopy.
    Canopy,
    /// Small trees under or between the canopy.
    Subcanopy,
    /// Shrubs.
    Shrub,
    /// Herbs, grasses, ferns and dwarf shrubs.
    Field,
    /// Mosses and lichens.
    Ground,
}

impl Stratum {
    /// Trees that cast a crown shadow (canopy and subcanopy).
    pub fn is_tree(self) -> bool {
        matches!(self, Self::Canopy | Self::Subcanopy)
    }
}

/// Where a species stands in forest succession.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Succession {
    /// Colonizes open ground; needs light; short-lived dominance.
    Pioneer,
    /// Intermediate.
    Mid,
    /// Late-successional (climax); regenerates in shade.
    Late,
}

/// One species' tolerances (ADR-2456; values in the public indicator-value literature's scales, see
/// `docs/provenance.md`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Species {
    /// Stable id (`betula_pendula`).
    pub id: String,
    /// Common name for messages (`silver birch`).
    pub common_name: String,
    /// The group a biome name uses (`birch`).
    pub group: String,
    /// Name tokens that identify it in entry names and mesh paths (`silver_birch`); the id and the group
    /// are matched too.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Stratum.
    pub stratum: Stratum,
    /// Soil moisture window it occurs in, 1 (very dry) to 10 (waterlogged).
    pub moisture: (f64, f64),
    /// Soil reaction window, 1 (strongly acid) to 9 (calcareous).
    pub reaction: (f64, f64),
    /// Indicator class (moisture, reaction): where it is most typical; an inferred site.
    pub indicator: (f64, f64),
    /// Light it needs, 1 (deep shade) to 9 (full light).
    pub light: f64,
    /// Shade tolerance, 1 (very intolerant) to 5 (very tolerant).
    pub shade_tolerance: f64,
    /// Shade its crown casts, 0 (none: not a tree) to 5 (deep).
    #[serde(default)]
    pub casts_shade: f64,
    /// Altitude band, meters.
    pub altitude_m: (f64, f64),
    /// Steepest ground it holds, degrees.
    pub max_slope_deg: f64,
    /// Succession stage.
    pub succession: Succession,
    /// A spring plant that grows before the canopy leafs out (exempt from the shade rules).
    #[serde(default)]
    pub vernal: bool,
}

/// A row of the built-in table: id, common name, group, aliases, stratum, moisture, reaction,
/// indicator (moisture, reaction), light, shade tolerance, casts shade, altitude, max slope, succession,
/// vernal.
type Row = (
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
    Stratum,
    (f64, f64),
    (f64, f64),
    (f64, f64),
    f64,
    f64,
    f64,
    (f64, f64),
    f64,
    Succession,
    bool,
);

use Stratum::{Canopy, Field, Ground, Shrub, Subcanopy};
use Succession::{Late, Mid, Pioneer};

/// The built-in temperate and boreal European species. Windows are coarse classes set from the public
/// indicator-value tables, shade-tolerance scores and tree-species atlas cited in `docs/provenance.md`
/// (rounded; tune per project with `custom_species`).
#[rustfmt::skip]
const BUILTIN: &[Row] = &[
    // Canopy trees.
    ("betula_pendula", "silver birch", "birch", &["silver_birch", "birch"], Canopy, (3.0, 7.0), (2.0, 7.0), (5.0, 4.0), 7.0, 2.0, 1.0, (0.0, 1900.0), 40.0, Pioneer, false),
    ("betula_pubescens", "downy birch", "birch", &["downy_birch", "bog_birch", "mountain_birch"], Canopy, (5.0, 9.0), (1.0, 5.0), (7.0, 3.0), 7.0, 1.9, 1.0, (0.0, 2000.0), 35.0, Pioneer, false),
    ("quercus_robur", "pedunculate oak", "oak", &["pedunculate_oak", "english_oak", "oak"], Canopy, (3.0, 7.0), (3.0, 8.0), (5.0, 5.0), 7.0, 2.5, 3.0, (0.0, 1000.0), 35.0, Mid, false),
    ("quercus_petraea", "sessile oak", "oak", &["sessile_oak"], Canopy, (2.0, 6.0), (2.0, 7.0), (5.0, 4.0), 6.0, 2.7, 3.0, (0.0, 1600.0), 45.0, Mid, false),
    ("acer_pseudoplatanus", "sycamore maple", "maple", &["sycamore_maple", "sycamore", "maple"], Canopy, (4.0, 7.0), (5.0, 8.0), (6.0, 6.0), 4.0, 3.7, 4.0, (0.0, 1700.0), 45.0, Mid, false),
    ("acer_platanoides", "Norway maple", "maple", &["norway_maple"], Canopy, (4.0, 7.0), (5.0, 8.0), (5.0, 7.0), 4.0, 4.2, 4.0, (0.0, 1000.0), 40.0, Mid, false),
    ("fagus_sylvatica", "European beech", "beech", &["beech", "fagus"], Canopy, (4.0, 7.0), (3.0, 8.0), (5.0, 6.0), 3.0, 4.6, 5.0, (0.0, 1600.0), 45.0, Late, false),
    ("carpinus_betulus", "hornbeam", "hornbeam", &["hornbeam", "carpinus"], Canopy, (4.0, 7.0), (4.0, 8.0), (6.0, 6.0), 4.0, 4.0, 4.0, (0.0, 900.0), 35.0, Late, false),
    ("tilia_cordata", "small-leaved lime", "lime", &["small_leaved_lime", "linden", "lime", "tilia"], Canopy, (3.0, 7.0), (4.0, 8.0), (5.0, 6.0), 5.0, 4.2, 4.0, (0.0, 1000.0), 45.0, Late, false),
    ("fraxinus_excelsior", "European ash", "ash", &["ash", "fraxinus"], Canopy, (4.0, 8.0), (6.0, 9.0), (6.0, 7.0), 4.0, 2.7, 2.0, (0.0, 1400.0), 40.0, Mid, false),
    ("alnus_glutinosa", "black alder", "alder", &["black_alder", "alder", "alnus"], Canopy, (7.0, 10.0), (4.0, 8.0), (9.0, 6.0), 5.0, 2.7, 3.0, (0.0, 1000.0), 15.0, Mid, false),
    ("picea_abies", "Norway spruce", "spruce", &["norway_spruce", "spruce", "picea"], Canopy, (4.0, 8.0), (1.0, 6.0), (6.0, 3.0), 5.0, 4.4, 4.0, (0.0, 2100.0), 45.0, Late, false),
    ("pinus_sylvestris", "Scots pine", "pine", &["scots_pine", "pine", "pinus"], Canopy, (2.0, 9.0), (1.0, 8.0), (5.0, 3.0), 7.0, 1.7, 2.0, (0.0, 2100.0), 45.0, Pioneer, false),
    ("abies_alba", "silver fir", "fir", &["silver_fir", "fir", "abies"], Canopy, (4.0, 8.0), (3.0, 8.0), (6.0, 5.0), 3.0, 4.6, 5.0, (300.0, 1800.0), 45.0, Late, false),
    ("larix_decidua", "European larch", "larch", &["larch", "larix"], Canopy, (3.0, 7.0), (2.0, 8.0), (5.0, 5.0), 8.0, 1.4, 1.0, (400.0, 2400.0), 50.0, Pioneer, false),
    ("populus_tremula", "aspen", "aspen", &["aspen", "populus_tremula"], Canopy, (3.0, 8.0), (2.0, 8.0), (5.0, 5.0), 6.0, 1.8, 2.0, (0.0, 1900.0), 40.0, Pioneer, false),
    // Small trees and shrubs.
    ("sorbus_aucuparia", "rowan", "rowan", &["rowan", "mountain_ash", "sorbus"], Subcanopy, (3.0, 8.0), (1.0, 6.0), (5.0, 4.0), 6.0, 2.7, 2.0, (0.0, 2300.0), 50.0, Pioneer, false),
    ("corylus_avellana", "hazel", "hazel", &["hazel", "corylus"], Shrub, (3.0, 7.0), (4.0, 8.0), (5.0, 6.0), 6.0, 3.2, 3.0, (0.0, 1700.0), 45.0, Mid, false),
    ("juniperus_communis", "common juniper", "juniper", &["juniper", "juniperus"], Shrub, (2.0, 6.0), (1.0, 8.0), (4.0, 4.0), 8.0, 1.0, 2.0, (0.0, 2500.0), 50.0, Pioneer, false),
    ("frangula_alnus", "alder buckthorn", "buckthorn", &["alder_buckthorn", "frangula"], Shrub, (5.0, 9.0), (2.0, 6.0), (7.0, 4.0), 6.0, 3.0, 2.0, (0.0, 1000.0), 25.0, Mid, false),
    ("crataegus_monogyna", "hawthorn", "hawthorn", &["hawthorn", "crataegus"], Shrub, (3.0, 7.0), (5.0, 9.0), (5.0, 7.0), 7.0, 2.3, 3.0, (0.0, 1000.0), 40.0, Pioneer, false),
    ("salix_aurita", "eared willow", "willow", &["eared_willow", "willow", "salix"], Shrub, (7.0, 10.0), (2.0, 6.0), (8.0, 4.0), 7.0, 1.8, 2.0, (0.0, 1500.0), 15.0, Pioneer, false),
    // Field layer.
    ("vaccinium_myrtillus", "bilberry", "bilberry", &["bilberry", "blueberry", "vaccinium"], Field, (4.0, 8.0), (1.0, 4.0), (6.0, 2.0), 5.0, 3.5, 0.0, (0.0, 2500.0), 45.0, Mid, false),
    ("vaccinium_vitis_idaea", "lingonberry", "lingonberry", &["lingonberry", "cowberry"], Field, (3.0, 7.0), (1.0, 3.0), (4.0, 2.0), 6.0, 3.0, 0.0, (0.0, 2800.0), 45.0, Mid, false),
    ("calluna_vulgaris", "heather", "heather", &["heather", "calluna", "ling"], Field, (2.0, 8.0), (1.0, 3.0), (5.0, 1.0), 8.0, 1.2, 0.0, (0.0, 2700.0), 45.0, Pioneer, false),
    ("pteridium_aquilinum", "bracken", "bracken", &["bracken", "pteridium"], Field, (3.0, 7.0), (2.0, 5.0), (5.0, 3.0), 6.0, 2.5, 0.0, (0.0, 1500.0), 40.0, Pioneer, false),
    ("dryopteris_filix_mas", "male fern", "fern", &["male_fern", "fern", "dryopteris"], Field, (4.0, 7.0), (3.0, 7.0), (5.0, 5.0), 3.0, 4.5, 0.0, (0.0, 1900.0), 45.0, Late, false),
    ("oxalis_acetosella", "wood sorrel", "sorrel", &["wood_sorrel", "oxalis"], Field, (4.0, 8.0), (2.0, 7.0), (6.0, 4.0), 2.0, 5.0, 0.0, (0.0, 2000.0), 45.0, Late, false),
    ("galium_odoratum", "woodruff", "woodruff", &["woodruff", "sweet_woodruff"], Field, (4.0, 6.0), (5.0, 8.0), (5.0, 6.0), 2.0, 5.0, 0.0, (0.0, 1500.0), 40.0, Late, false),
    ("mercurialis_perennis", "dog's mercury", "mercury", &["dogs_mercury", "mercurialis"], Field, (5.0, 7.0), (6.0, 9.0), (6.0, 7.0), 3.0, 4.5, 0.0, (0.0, 1800.0), 40.0, Late, false),
    ("anemone_nemorosa", "wood anemone", "anemone", &["wood_anemone", "anemone"], Field, (4.0, 7.0), (3.0, 8.0), (5.0, 5.0), 5.0, 4.0, 0.0, (0.0, 1900.0), 40.0, Late, true),
    ("molinia_caerulea", "purple moor-grass", "moor_grass", &["moor_grass", "molinia"], Field, (6.0, 10.0), (1.0, 5.0), (8.0, 3.0), 7.0, 2.0, 0.0, (0.0, 2200.0), 30.0, Mid, false),
    ("deschampsia_flexuosa", "wavy hair-grass", "hair_grass", &["hair_grass", "deschampsia"], Field, (3.0, 7.0), (1.0, 4.0), (5.0, 2.0), 6.0, 3.0, 0.0, (0.0, 2500.0), 45.0, Mid, false),
    // Ground layer.
    ("hylocomium_splendens", "glittering wood-moss", "moss", &["feather_moss", "wood_moss", "moss", "hylocomium"], Ground, (4.0, 7.0), (1.0, 5.0), (5.0, 3.0), 5.0, 4.0, 0.0, (0.0, 2500.0), 45.0, Late, false),
    ("sphagnum_palustre", "peat moss", "sphagnum", &["peat_moss", "sphagnum"], Ground, (8.0, 10.0), (1.0, 3.0), (9.0, 2.0), 6.0, 2.5, 0.0, (0.0, 2000.0), 15.0, Mid, false),
    ("cladonia_rangiferina", "reindeer lichen", "lichen", &["reindeer_lichen", "lichen", "cladonia"], Ground, (1.0, 4.0), (1.0, 3.0), (2.0, 2.0), 8.0, 1.0, 0.0, (0.0, 2800.0), 45.0, Pioneer, false),
];

fn row_species(r: &Row) -> Species {
    Species {
        id: r.0.to_string(),
        common_name: r.1.to_string(),
        group: r.2.to_string(),
        aliases: r.3.iter().map(|a| (*a).to_string()).collect(),
        stratum: r.4,
        moisture: r.5,
        reaction: r.6,
        indicator: r.7,
        light: r.8,
        shade_tolerance: r.9,
        casts_shade: r.10,
        altitude_m: r.11,
        max_slope_deg: r.12,
        succession: r.13,
        vernal: r.14,
    }
}

/// The species a validation or a competition pass knows: the built-in table plus a profile's custom
/// species (a custom species with a built-in id replaces it).
#[derive(Clone, Debug, PartialEq)]
pub struct SpeciesCatalog {
    list: Vec<Species>,
}

impl SpeciesCatalog {
    /// The built-in table.
    pub fn builtin() -> &'static SpeciesCatalog {
        static CAT: OnceLock<SpeciesCatalog> = OnceLock::new();
        CAT.get_or_init(|| SpeciesCatalog {
            list: BUILTIN.iter().map(row_species).collect(),
        })
    }

    /// The built-in table with `custom` added (same id: replaced).
    pub fn with_custom(custom: &[Species]) -> SpeciesCatalog {
        let mut list = Self::builtin().list.clone();
        for c in custom {
            match list.iter_mut().find(|s| s.id == c.id) {
                Some(s) => *s = c.clone(),
                None => list.push(c.clone()),
            }
        }
        SpeciesCatalog { list }
    }

    /// Every species, built-in order first.
    pub fn species(&self) -> &[Species] {
        &self.list
    }

    /// A species by id.
    pub fn get(&self, id: &str) -> Option<&Species> {
        self.list.iter().find(|s| s.id == id)
    }

    /// Whether `group` names a species group (`birch`, `oak`, …).
    pub fn is_group(&self, group: &str) -> bool {
        self.list.iter().any(|s| s.group == group)
    }

    /// The species a name or asset path names: the alias (or id or group) with the most tokens that
    /// appears as a whole-token run in `text` (`trees/silver_birch_a.glb` → silver birch, `oak01` → oak);
    /// ties go to table order.
    pub fn match_name(&self, text: &str) -> Option<&Species> {
        let tokens = tokens(text);
        let mut best: Option<(usize, &Species)> = None;
        for s in &self.list {
            let names = std::iter::once(s.id.as_str())
                .chain(s.aliases.iter().map(String::as_str))
                .chain(std::iter::once(s.group.as_str()));
            for name in names {
                let alias: Vec<&str> = name.split('_').filter(|t| !t.is_empty()).collect();
                if alias.is_empty() || !contains_run(&tokens, &alias) {
                    continue;
                }
                if best.is_none_or(|(n, _)| alias.len() > n) {
                    best = Some((alias.len(), s));
                }
            }
        }
        best.map(|(_, s)| s)
    }
}

/// Lowercase alphabetic tokens of `text`.
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphabetic())
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn contains_run(tokens: &[String], run: &[&str]) -> bool {
    tokens
        .windows(run.len())
        .any(|w| w.iter().zip(run).all(|(a, b)| a == b))
}

fn d_version() -> u32 {
    ECOLOGY_FORMAT_VERSION
}
fn d_lo() -> f64 {
    -1.0e9
}
fn d_hi() -> f64 {
    1.0e9
}
fn d_alt() -> (f64, f64) {
    (d_lo(), d_hi())
}
fn d_slope() -> (f64, f64) {
    (0.0, 90.0)
}
fn d_true() -> bool {
    true
}
fn d_cell() -> f64 {
    25.0
}
fn d_stems() -> f64 {
    400.0
}
fn d_exponent() -> f64 {
    1.605
}

/// The site a biome stands for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Site {
    /// Soil moisture range, 1 (very dry) to 10 (waterlogged).
    pub moisture: (f64, f64),
    /// Soil reaction range, 1 (strongly acid) to 9 (calcareous).
    pub reaction: (f64, f64),
    /// Altitude range, meters (default unbounded).
    #[serde(default = "d_alt")]
    pub altitude_m: (f64, f64),
    /// Ground slope range, degrees (default 0-90).
    #[serde(default = "d_slope")]
    pub slope_deg: (f64, f64),
    /// A soil label for messages (`podzol`).
    #[serde(default)]
    pub soil: String,
}

/// A species' role in a biome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Role {
    /// Forms most of the canopy and defines the biome.
    Dominant,
    /// Grows with the dominant in the canopy or below it.
    Associated,
    /// Shrub, field or ground layer under the canopy.
    Understory,
}

impl Role {
    /// Lowercase name for the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dominant => "dominant",
            Self::Associated => "associated",
            Self::Understory => "understory",
        }
    }
}

/// Which species a biome entry places.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeciesUse {
    /// The entry, `layer/entry`.
    pub entry: String,
    /// Species id.
    pub species: String,
    /// Role.
    pub role: Role,
    /// Its share of the layer's placements (0-1); the layer's shares sum to 1 and match the entry
    /// weights. `None`: not checked (child-only entries).
    #[serde(default)]
    pub share: Option<f64>,
}

/// Size-dependent self-thinning of the canopy (`biome.ecology`): in each world-anchored cell the
/// stand-density sum `Σ scaleᵢ^exponent` may not exceed `max_stems_per_ha` per hectare; the smallest
/// stems go first.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SelfThinning {
    /// Cell size, meters.
    #[serde(default = "d_cell")]
    pub cell_m: f64,
    /// Most stems per hectare at scale 1.
    #[serde(default = "d_stems")]
    pub max_stems_per_ha: f64,
    /// Size exponent (stand density index exponent, 1.605 by default).
    #[serde(default = "d_exponent")]
    pub exponent: f64,
}

/// The opt-in placement rules the `biome.ecology` node applies to a biome.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Competition {
    /// Shade-needing understory (light ≤ 3) only under a tree crown; light-demanding understory
    /// (light ≥ 7) not under a deep-shade crown (casts ≥ 4).
    #[serde(default = "d_true")]
    pub shade_rules: bool,
    /// Canopy self-thinning (`None`: off).
    #[serde(default)]
    pub self_thinning: Option<SelfThinning>,
}

impl Default for Competition {
    fn default() -> Self {
        Self {
            shade_rules: true,
            self_thinning: None,
        }
    }
}

/// An ecology profile `<name>.ecology.ron` next to `<name>.biome.ron` (format `ecology` v1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EcologyProfile {
    /// Format version (1).
    #[serde(default = "d_version")]
    pub format_version: u32,
    /// The site.
    pub site: Site,
    /// Entry → species.
    #[serde(default)]
    pub species: Vec<SpeciesUse>,
    /// Placement rules for `biome.ecology`.
    #[serde(default)]
    pub competition: Competition,
    /// Project species added to (or replacing) the built-in table.
    #[serde(default)]
    pub custom_species: Vec<Species>,
}

/// Why a profile could not be read (`ecology_invalid`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EcologyError {
    /// Not valid RON for the format, or a newer version.
    #[error("ecology_invalid: {0}")]
    Parse(String),
}

impl EcologyProfile {
    /// Parses and migrates an `.ecology.ron` text.
    ///
    /// # Errors
    ///
    /// [`EcologyError::Parse`] for bad RON, a newer `format_version` or a failed migration.
    pub fn from_ron(text: &str) -> Result<Self, EcologyError> {
        let version = regia_core::version::peek_ron_version(text).unwrap_or(ECOLOGY_FORMAT_VERSION);
        if version > ECOLOGY_FORMAT_VERSION {
            return Err(EcologyError::Parse(format!(
                "format_version {version} is newer than {ECOLOGY_FORMAT_VERSION}"
            )));
        }
        let mut p: Self = if version < ECOLOGY_FORMAT_VERSION {
            // Through the value tree only to migrate (it cannot carry unit enum variants back).
            let mut value: ron::Value =
                ron::from_str(text).map_err(|e| EcologyError::Parse(e.to_string()))?;
            migrate(&mut value, version, ECOLOGY_MIGRATIONS, ECOLOGY_FORMAT_VERSION)
                .map_err(|e| EcologyError::Parse(format!("{e:?}")))?;
            value
                .into_rust()
                .map_err(|e| EcologyError::Parse(e.to_string()))?
        } else {
            ron::from_str(text).map_err(|e| EcologyError::Parse(e.to_string()))?
        };
        p.format_version = ECOLOGY_FORMAT_VERSION;
        Ok(p)
    }

    /// The canonical text (every field written).
    pub fn to_ron(&self) -> String {
        let cfg = regia_core::ron_pretty()
            .struct_names(true)
            .indentor("    ".to_string());
        ron::ser::to_string_pretty(self, cfg).unwrap_or_default()
    }

    /// The catalog this profile validates against.
    pub fn catalog(&self) -> SpeciesCatalog {
        SpeciesCatalog::with_custom(&self.custom_species)
    }
}

/// How bad an issue is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    /// An implausible mix: fails `cargo xtask check-ecology`.
    Error,
    /// Worth a look.
    Warning,
}

/// One ecology problem.
#[derive(Clone, Debug, PartialEq)]
pub struct EcologyIssue {
    /// Stable code (`ecology_moisture`, …; book `agents/biomes.md`).
    pub code: String,
    /// Severity.
    pub severity: Severity,
    /// Where: the entry `layer/entry`, `site`, `name` or `species/<n>`.
    pub path: String,
    /// The species concerned (id), if one.
    pub species: Option<String>,
    /// The reason, in words.
    pub message: String,
}

/// How an entry's species was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapSource {
    /// Named in the profile.
    Profile,
    /// Matched from the entry name or mesh path.
    Name,
}

/// One biome entry with its species.
#[derive(Clone, Debug, PartialEq)]
pub struct MappedEntry {
    /// `layer/entry`.
    pub entry: String,
    /// Index of the layer in the biome.
    pub layer: usize,
    /// Index of the entry in the layer.
    pub index: usize,
    /// Species id.
    pub species: String,
    /// Role.
    pub role: Role,
    /// Declared share (profile), if any.
    pub share: Option<f64>,
    /// The entry's weight over the layer's placing entries (`None` for child-only entries).
    pub weight_share: Option<f64>,
    /// Where the species came from.
    pub source: MapSource,
}

/// The result of [`validate_ecology`].
#[derive(Clone, Debug, PartialEq)]
pub struct EcologyReport {
    /// The biome's name.
    pub biome: String,
    /// The site checked against (`None`: no profile and no dominant to infer it from).
    pub site: Option<Site>,
    /// Whether the site was inferred from the dominant.
    pub site_inferred: bool,
    /// Light reaching the floor under the canopy, 1-9 (`None`: no trees).
    pub floor_light: Option<f64>,
    /// Entries with a species.
    pub species: Vec<MappedEntry>,
    /// Enabled entries with no species (`layer/entry`: rocks, logs, spacers, unknown plants).
    pub unmapped: Vec<String>,
    /// Problems, in rule order.
    pub issues: Vec<EcologyIssue>,
}

impl EcologyReport {
    /// No error-level issue.
    pub fn plausible(&self) -> bool {
        !self.issues.iter().any(|i| i.severity == Severity::Error)
    }
}

/// Moisture class words.
fn moisture_words(lo: f64, hi: f64) -> String {
    let w = |v: f64| match v {
        v if v < 2.5 => "very dry",
        v if v < 4.5 => "dry",
        v if v < 6.5 => "mesic",
        v if v < 7.5 => "moist",
        v if v < 8.5 => "wet",
        _ => "waterlogged",
    };
    let (a, b) = (w(lo), w(hi));
    if a == b {
        a.to_string()
    } else {
        format!("{a} to {b}")
    }
}

/// Reaction class words.
fn reaction_words(lo: f64, hi: f64) -> String {
    let w = |v: f64| match v {
        v if v < 2.5 => "strongly acid",
        v if v < 4.5 => "acid",
        v if v < 6.5 => "weakly acid",
        v if v < 7.5 => "neutral",
        v if v < 8.5 => "base-rich",
        _ => "calcareous",
    };
    let (a, b) = (w(lo), w(hi));
    if a == b {
        a.to_string()
    } else {
        format!("{a} to {b}")
    }
}

fn span(w: (f64, f64)) -> String {
    let f = |v: f64| {
        if (v - v.round()).abs() < 1e-9 {
            format!("{}", v.round() as i64)
        } else {
            format!("{v:.1}")
        }
    };
    if (w.0 - w.1).abs() < 1e-9 {
        f(w.0)
    } else {
        format!("{}-{}", f(w.0), f(w.1))
    }
}

fn overlaps(a: (f64, f64), b: (f64, f64)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

fn window_ok(w: (f64, f64)) -> bool {
    w.0.is_finite() && w.1.is_finite() && w.0 <= w.1
}

/// The species of every enabled entry: the profile's list first, then name matching; roles for
/// name-matched entries come from the stratum (trees: the named group or the largest canopy weight is
/// dominant).
pub fn map_species(
    def: &BiomeDef,
    profile: Option<&EcologyProfile>,
    catalog: &SpeciesCatalog,
) -> (Vec<MappedEntry>, Vec<String>, Vec<EcologyIssue>) {
    let mut issues = Vec::new();
    let mut mapped: Vec<MappedEntry> = Vec::new();
    let mut unmapped = Vec::new();
    let declared: BTreeMap<&str, (usize, &SpeciesUse)> = profile
        .map(|p| {
            p.species
                .iter()
                .enumerate()
                .map(|(k, u)| (u.entry.as_str(), (k, u)))
                .collect()
        })
        .unwrap_or_default();
    let mut seen = BTreeSet::new();
    for (li, layer) in def.layers.iter().enumerate() {
        let total: f64 = layer
            .entries
            .iter()
            .filter(|e| e.enabled && !e.child_only())
            .map(|e| e.weight)
            .sum();
        for (ei, e) in layer.entries.iter().enumerate() {
            if !e.enabled {
                continue;
            }
            let path = format!("{}/{}", layer.name, e.name);
            let weight_share = (!e.child_only() && total > 0.0).then(|| e.weight / total);
            if let Some((k, u)) = declared.get(path.as_str()) {
                seen.insert(path.clone());
                if catalog.get(&u.species).is_none() {
                    issues.push(EcologyIssue {
                        code: "ecology_species_unknown".into(),
                        severity: Severity::Error,
                        path: format!("species/{k}"),
                        species: Some(u.species.clone()),
                        message: format!("`{path}` names the unknown species `{}`", u.species),
                    });
                    continue;
                }
                mapped.push(MappedEntry {
                    entry: path,
                    layer: li,
                    index: ei,
                    species: u.species.clone(),
                    role: u.role,
                    share: u.share,
                    weight_share,
                    source: MapSource::Profile,
                });
                continue;
            }
            let found = catalog
                .match_name(&e.name)
                .or_else(|| e.spawn.asset().and_then(|a| catalog.match_name(a)));
            match found {
                Some(s) => mapped.push(MappedEntry {
                    entry: path,
                    layer: li,
                    index: ei,
                    species: s.id.clone(),
                    role: if s.stratum.is_tree() {
                        Role::Associated
                    } else {
                        Role::Understory
                    },
                    share: None,
                    weight_share,
                    source: MapSource::Name,
                }),
                None => unmapped.push(path),
            }
        }
    }
    if let Some(p) = profile {
        for (k, u) in p.species.iter().enumerate() {
            if !seen.contains(u.entry.as_str()) {
                issues.push(EcologyIssue {
                    code: "ecology_entry_unknown".into(),
                    severity: Severity::Error,
                    path: format!("species/{k}"),
                    species: Some(u.species.clone()),
                    message: format!("the biome has no enabled entry `{}`", u.entry),
                });
            }
        }
    }
    // Name-matched canopy dominants when the profile names none.
    let has_dominant = mapped.iter().any(|m| m.role == Role::Dominant);
    if !has_dominant {
        let named = name_groups(&def.name, catalog);
        let canopy = |m: &MappedEntry| {
            catalog
                .get(&m.species)
                .is_some_and(|s| s.stratum == Stratum::Canopy)
        };
        let mut any = false;
        for m in mapped.iter_mut().filter(|m| m.source == MapSource::Name) {
            if canopy(m) && catalog.get(&m.species).is_some_and(|s| named.contains(&s.group)) {
                m.role = Role::Dominant;
                any = true;
            }
        }
        if !any && named.is_empty() {
            // The canopy entry with the largest weight (ties: first).
            let mut best: Option<(f64, usize)> = None;
            for (k, m) in mapped.iter().enumerate() {
                if m.source == MapSource::Name && canopy(m) {
                    let w = m.weight_share.unwrap_or(0.0);
                    if best.is_none_or(|(bw, _)| w > bw) {
                        best = Some((w, k));
                    }
                }
            }
            if let Some((_, k)) = best {
                mapped[k].role = Role::Dominant;
            }
        }
    }
    (mapped, unmapped, issues)
}

/// The species groups a biome name names (`Birch woodland` → `birch`; plurals too).
fn name_groups(name: &str, catalog: &SpeciesCatalog) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for t in tokens(name) {
        let stems = [Some(t.as_str()), t.strip_suffix("es"), t.strip_suffix('s')];
        if let Some(g) = stems.into_iter().flatten().find(|g| catalog.is_group(g)) {
            out.insert(g.to_string());
        }
    }
    out
}

/// The share weights of the trees, for the canopy shade: the declared share, else the weight share,
/// else 1.
fn tree_weight(m: &MappedEntry) -> f64 {
    m.share.or(m.weight_share).unwrap_or(1.0).max(0.0)
}

/// Checks a biome's species mix against its site and each other (ADR-2456). `profile` is the biome's
/// `.ecology.ron`, if it has one; `catalog` is `profile.catalog()` or [`SpeciesCatalog::builtin`].
pub fn validate_ecology(
    def: &BiomeDef,
    profile: Option<&EcologyProfile>,
    catalog: &SpeciesCatalog,
) -> EcologyReport {
    let (mapped, unmapped, mut issues) = map_species(def, profile, catalog);
    let mut err = |code: &str, path: &str, species: Option<&str>, message: String| {
        issues.push(EcologyIssue {
            code: code.to_string(),
            severity: Severity::Error,
            path: path.to_string(),
            species: species.map(str::to_string),
            message,
        });
    };
    let sp = |m: &MappedEntry| catalog.get(&m.species);
    let dominants: Vec<&MappedEntry> = mapped.iter().filter(|m| m.role == Role::Dominant).collect();

    // The site: the profile's, else the dominants' indicator classes.
    let (site, inferred) = match profile {
        Some(p) => {
            let s = &p.site;
            if !(window_ok(s.moisture)
                && window_ok(s.reaction)
                && window_ok(s.altitude_m)
                && window_ok(s.slope_deg))
            {
                err(
                    "ecology_invalid",
                    "site",
                    None,
                    "site windows need finite min <= max".into(),
                );
            }
            (Some(s.clone()), false)
        }
        None => {
            let ds: Vec<&Species> = dominants.iter().filter_map(|m| sp(m)).collect();
            if ds.is_empty() {
                (None, false)
            } else {
                let mean = |f: fn(&Species) -> f64| ds.iter().map(|s| f(s)).sum::<f64>() / ds.len() as f64;
                let (fm, rm) = (mean(|s| s.indicator.0), mean(|s| s.indicator.1));
                let alt = ds.iter().fold((f64::MIN, f64::MAX), |a, s| {
                    (a.0.max(s.altitude_m.0), a.1.min(s.altitude_m.1))
                });
                let slope = ds.iter().map(|s| s.max_slope_deg).fold(90.0f64, f64::min);
                (
                    Some(Site {
                        moisture: (fm - 0.5, fm + 0.5),
                        reaction: (rm - 0.5, rm + 0.5),
                        altitude_m: alt,
                        slope_deg: (0.0, slope),
                        soil: String::new(),
                    }),
                    true,
                )
            }
        }
    };

    // The name: every group the name names must dominate.
    let named = name_groups(&def.name, catalog);
    for g in &named {
        let dominant = dominants.iter().any(|m| sp(m).is_some_and(|s| &s.group == g));
        if !dominant {
            let present = mapped.iter().any(|m| sp(m).is_some_and(|s| &s.group == g));
            err(
                "ecology_name_mismatch",
                "name",
                None,
                if present {
                    format!("`{}` is named after {g}, but {g} is not a dominant", def.name)
                } else {
                    format!("`{}` is named after {g}, but no {g} grows in it", def.name)
                },
            );
        }
    }

    // Each species against the site.
    if let Some(site) = &site {
        let soil = if site.soil.is_empty() {
            String::new()
        } else {
            format!(", {}", site.soil)
        };
        let site_m = format!(
            "F {} ({}{soil})",
            span(site.moisture),
            moisture_words(site.moisture.0, site.moisture.1)
        );
        let site_r = format!(
            "R {} ({})",
            span(site.reaction),
            reaction_words(site.reaction.0, site.reaction.1)
        );
        let basis = if inferred {
            " (inferred from the dominant)"
        } else {
            ""
        };
        for m in &mapped {
            let Some(s) = sp(m) else { continue };
            if !overlaps(s.moisture, site.moisture) {
                err(
                    "ecology_moisture",
                    &m.entry,
                    Some(&s.id),
                    format!(
                        "{} needs soil moisture F {} ({}); the site{basis} is {site_m}",
                        s.common_name,
                        span(s.moisture),
                        moisture_words(s.moisture.0, s.moisture.1)
                    ),
                );
            }
            if !overlaps(s.reaction, site.reaction) {
                err(
                    "ecology_reaction",
                    &m.entry,
                    Some(&s.id),
                    format!(
                        "{} needs soil reaction R {} ({}); the site{basis} is {site_r}",
                        s.common_name,
                        span(s.reaction),
                        reaction_words(s.reaction.0, s.reaction.1)
                    ),
                );
            }
            let e = &def.layers[m.layer].entries[m.index];
            // The entry's own filter windows first, then the site's.
            let mut alt = (d_lo(), d_hi());
            let mut slope_min = 0.0f64;
            if !e.filters_off {
                for f in &e.filters {
                    match f {
                        EntryFilter::Altitude { min, max, .. } => alt = (alt.0.max(*min), alt.1.min(*max)),
                        EntryFilter::Slope { min_deg, .. } => slope_min = slope_min.max(*min_deg),
                        _ => {}
                    }
                }
            }
            let shown = |w: (f64, f64)| span((w.0.max(-1.0e6), w.1.min(1.0e6)));
            if alt.0 <= alt.1 && !overlaps(s.altitude_m, alt) {
                err(
                    "ecology_altitude",
                    &m.entry,
                    Some(&s.id),
                    format!(
                        "{} grows at {} m; this entry places it at {} m",
                        s.common_name,
                        span(s.altitude_m),
                        shown(alt)
                    ),
                );
            } else if !overlaps(s.altitude_m, site.altitude_m) {
                err(
                    "ecology_altitude",
                    &m.entry,
                    Some(&s.id),
                    format!(
                        "{} grows at {} m; the site{basis} lies at {} m",
                        s.common_name,
                        span(s.altitude_m),
                        shown(site.altitude_m)
                    ),
                );
            }
            if slope_min > s.max_slope_deg {
                err(
                    "ecology_slope",
                    &m.entry,
                    Some(&s.id),
                    format!(
                        "{} holds slopes up to {} degrees; this entry starts at {} degrees",
                        s.common_name,
                        span((s.max_slope_deg, s.max_slope_deg)),
                        span((slope_min, slope_min))
                    ),
                );
            } else if site.slope_deg.0 > s.max_slope_deg {
                err(
                    "ecology_slope",
                    &m.entry,
                    Some(&s.id),
                    format!(
                        "{} holds slopes up to {} degrees; the site{basis} starts at {} degrees",
                        s.common_name,
                        span((s.max_slope_deg, s.max_slope_deg)),
                        span((site.slope_deg.0, site.slope_deg.0))
                    ),
                );
            }
        }
    }

    // Pairs that never share a site.
    let mut ids: Vec<(&MappedEntry, &Species)> = Vec::new();
    for m in &mapped {
        if let Some(s) = sp(m)
            && !ids.iter().any(|(_, t)| t.id == s.id)
        {
            ids.push((m, s));
        }
    }
    for (i, (_, a)) in ids.iter().enumerate() {
        for (mb, b) in &ids[i + 1..] {
            let why = if !overlaps(a.moisture, b.moisture) {
                Some(format!(
                    "{} (moisture F {}) and {} (F {}) never share a site",
                    a.common_name,
                    span(a.moisture),
                    b.common_name,
                    span(b.moisture)
                ))
            } else if !overlaps(a.reaction, b.reaction) {
                Some(format!(
                    "{} (reaction R {}) and {} (R {}) never share a site",
                    a.common_name,
                    span(a.reaction),
                    b.common_name,
                    span(b.reaction)
                ))
            } else {
                None
            };
            if let Some(message) = why {
                err("ecology_pair_exclusive", &mb.entry, Some(&b.id), message);
            }
        }
    }

    // Canopy shade and the understory's light.
    let trees: Vec<(&MappedEntry, &Species)> = mapped
        .iter()
        .filter(|m| m.role != Role::Understory)
        .filter_map(|m| sp(m).map(|s| (m, s)))
        .filter(|(_, s)| s.stratum == Stratum::Canopy)
        .collect();
    let floor_light = {
        let total: f64 = trees.iter().map(|(m, _)| tree_weight(m)).sum();
        (total > 0.0).then(|| {
            let shade = trees
                .iter()
                .map(|(m, s)| tree_weight(m) * s.casts_shade)
                .sum::<f64>()
                / total;
            (10.0 - 1.4 * shade).clamp(1.0, 9.0)
        })
    };
    if let Some(floor) = floor_light {
        for m in mapped.iter().filter(|m| m.role == Role::Understory) {
            let Some(s) = sp(m) else { continue };
            if s.stratum.is_tree() || s.vernal {
                continue;
            }
            if s.light > floor + 1.5 {
                err(
                    "ecology_understory_light",
                    &m.entry,
                    Some(&s.id),
                    format!(
                        "{} needs light {} but the canopy leaves about {} on the floor",
                        s.common_name,
                        span((s.light, s.light)),
                        span(((floor * 2.0).round() / 2.0, (floor * 2.0).round() / 2.0))
                    ),
                );
            }
        }
    }

    // A deep-shade late tree and a shade-intolerant pioneer cannot share dominance.
    for (i, a) in dominants.iter().enumerate() {
        for b in &dominants[i + 1..] {
            let (Some(sa), Some(sb)) = (sp(a), sp(b)) else {
                continue;
            };
            for (shader, pioneer, at) in [(sa, sb, &b.entry), (sb, sa, &a.entry)] {
                if shader.casts_shade >= 4.0
                    && pioneer.shade_tolerance < 2.5
                    && pioneer.succession == Succession::Pioneer
                {
                    err(
                        "ecology_competition",
                        at,
                        Some(&pioneer.id),
                        format!(
                            "{} (pioneer, shade tolerance {}) cannot share dominance with {}, whose shade ({}) suppresses it",
                            pioneer.common_name,
                            span((pioneer.shade_tolerance, pioneer.shade_tolerance)),
                            shader.common_name,
                            span((shader.casts_shade, shader.casts_shade))
                        ),
                    );
                }
            }
        }
    }

    // Proportions (profile only).
    if profile.is_some() {
        let canopy_species = mapped
            .iter()
            .any(|m| m.source == MapSource::Profile && sp(m).is_some_and(|s| s.stratum == Stratum::Canopy));
        if canopy_species && dominants.is_empty() {
            err(
                "ecology_dominant_missing",
                "species",
                None,
                "the profile names canopy trees but no dominant".into(),
            );
        }
        let mut by_layer: BTreeMap<usize, Vec<&MappedEntry>> = BTreeMap::new();
        for m in mapped.iter().filter(|m| m.source == MapSource::Profile) {
            by_layer.entry(m.layer).or_default().push(m);
        }
        for (li, list) in by_layer {
            let layer = &def.layers[li].name;
            let shared: Vec<&&MappedEntry> = list
                .iter()
                .filter(|m| m.share.is_some() && m.weight_share.is_some())
                .collect();
            if shared.is_empty() {
                continue;
            }
            let sum: f64 = shared.iter().filter_map(|m| m.share).sum();
            let placing = def.layers[li]
                .entries
                .iter()
                .filter(|e| e.enabled && !e.child_only())
                .count();
            if shared.len() == placing && (sum - 1.0).abs() > 0.02 {
                err(
                    "ecology_share_sum",
                    layer,
                    None,
                    format!("the shares of layer `{layer}` sum to {sum:.2}, not 1"),
                );
            }
            for m in &shared {
                let (Some(share), Some(ws)) = (m.share, m.weight_share) else {
                    continue;
                };
                if (share - ws).abs() > 0.05 {
                    err(
                        "ecology_share_mismatch",
                        &m.entry,
                        Some(&m.species),
                        format!("declared share {share:.2}, but the entry weights give {ws:.2}"),
                    );
                }
            }
            let top_dominant = list
                .iter()
                .filter(|m| m.role == Role::Dominant)
                .filter_map(|m| m.share)
                .fold(f64::NAN, f64::min);
            for m in list.iter().filter(|m| m.role == Role::Associated) {
                if let Some(s) = m.share
                    && s > top_dominant
                {
                    err(
                        "ecology_dominance_order",
                        &m.entry,
                        Some(&m.species),
                        format!(
                            "an associated species has a larger share ({s:.2}) than a dominant ({top_dominant:.2})"
                        ),
                    );
                }
            }
        }
    }

    EcologyReport {
        biome: def.name.clone(),
        site,
        site_inferred: inferred,
        floor_light,
        species: mapped,
        unmapped,
        issues,
    }
}

/// One placed point for [`competition_pass`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompPoint {
    /// World X.
    pub x: f32,
    /// World Z.
    pub z: f32,
    /// Crown radius (`spread_m`, already scaled).
    pub spread_m: f32,
    /// Uniform scale (the size in the thinning sum).
    pub size: f32,
    /// The point's seed (a tie-break).
    pub seed: u32,
    /// The species' stratum (`None`: not a species: a rock, a log; untouched).
    pub stratum: Option<Stratum>,
    /// Light the species needs.
    pub light: f64,
    /// Shade its crown casts.
    pub casts_shade: f64,
    /// A spring plant (exempt from the shade rules).
    pub vernal: bool,
    /// Apply the shade rules to this understory point.
    pub shade_rules: bool,
    /// Self-thinning of this canopy point: the rule and a group key (one per biome).
    pub thinning: Option<(u32, SelfThinning)>,
}

/// Why [`competition_pass`] removed a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompOut {
    /// Self-thinning: a smaller stem in a crowded cell.
    Thinned,
    /// A shade-needing plant outside every crown.
    Unshaded,
    /// A light-demanding plant under a deep-shade crown.
    Shaded,
}

impl CompOut {
    /// The counter suffix (`ecology_thinned`, `ecology_unshaded_out`, `ecology_shaded_out`).
    pub fn counter(self) -> &'static str {
        match self {
            Self::Thinned => "ecology_thinned",
            Self::Unshaded => "ecology_unshaded_out",
            Self::Shaded => "ecology_shaded_out",
        }
    }
}

/// Competition-aware thinning (ADR-2456), a pure function of its points (the result does not depend on
/// their order):
///
/// 1. Self-thinning: canopy points with a rule, per group and world-anchored cell of `cell_m`: while
///    `Σ sizeᵢ^exponent` exceeds `max_stems_per_ha × cell area / 1 ha`, the smallest stem goes (ties by
///    seed, then position).
/// 2. Shade: the remaining trees' crowns (`spread_m`). An understory point with `shade_rules` that needs
///    light ≤ 3 is removed when no crown covers it; one that needs light ≥ 7 is removed under a crown
///    that casts shade ≥ 4. Spring plants are exempt.
pub fn competition_pass(points: &[CompPoint]) -> Vec<Option<CompOut>> {
    let mut out = vec![None; points.len()];
    // 1. Self-thinning.
    let mut cells: BTreeMap<(u32, i64, i64, u64, u64), Vec<usize>> = BTreeMap::new();
    for (i, p) in points.iter().enumerate() {
        if let (Some(s), Some((group, rule))) = (p.stratum, p.thinning)
            && s.is_tree()
            && rule.cell_m > 0.0
            && rule.max_stems_per_ha.is_finite()
        {
            let c = rule.cell_m;
            let key = (
                group,
                (f64::from(p.x) / c).floor() as i64,
                (f64::from(p.z) / c).floor() as i64,
                c.to_bits(),
                rule.max_stems_per_ha.to_bits(),
            );
            cells.entry(key).or_default().push(i);
        }
    }
    for ((_, _, _, cbits, nbits), mut list) in cells {
        let c = f64::from_bits(cbits);
        let budget = f64::from_bits(nbits).max(0.0) * c * c / 10_000.0;
        let exp = list
            .first()
            .and_then(|&i| points[i].thinning)
            .map_or(d_exponent(), |(_, r)| r.exponent);
        // detmath, not the platform libm: one f64 ulp never crosses the slack, but the platform
        // powf gap at large sizes does, and the stems a platform keeps are its instances
        // (audit: procgen determinism).
        let load = |i: usize| crate::detmath::pow(f64::from(points[i].size.max(0.0)), exp);
        let mut total: f64 = list.iter().map(|&i| load(i)).sum();
        list.sort_by(|&a, &b| {
            let (pa, pb) = (&points[a], &points[b]);
            pa.size
                .total_cmp(&pb.size)
                .then(pa.seed.cmp(&pb.seed))
                .then(pa.x.total_cmp(&pb.x))
                .then(pa.z.total_cmp(&pb.z))
        });
        for i in list {
            if total <= budget + 1e-9 {
                break;
            }
            total -= load(i);
            out[i] = Some(CompOut::Thinned);
        }
    }
    // 2. Shade.
    let crowns: Vec<usize> = (0..points.len())
        .filter(|&i| {
            out[i].is_none() && points[i].stratum.is_some_and(Stratum::is_tree) && points[i].spread_m > 0.0
        })
        .collect();
    let max_r = crowns.iter().map(|&i| points[i].spread_m).fold(0.0f32, f32::max);
    if max_r <= 0.0 && !points.iter().any(|p| p.shade_rules) {
        return out;
    }
    let cell = f64::from(max_r.max(0.5)) * 2.0;
    let key = |x: f32, z: f32| {
        (
            (f64::from(x) / cell).floor() as i64,
            (f64::from(z) / cell).floor() as i64,
        )
    };
    let mut grid: BTreeMap<(i64, i64), Vec<usize>> = BTreeMap::new();
    for &i in &crowns {
        grid.entry(key(points[i].x, points[i].z)).or_default().push(i);
    }
    for (i, p) in points.iter().enumerate() {
        let Some(s) = p.stratum else { continue };
        if s.is_tree() || !p.shade_rules || p.vernal || out[i].is_some() {
            continue;
        }
        let (cx, cz) = key(p.x, p.z);
        let mut cover: Option<f64> = None;
        for dx in -1..=1 {
            for dz in -1..=1 {
                for &j in grid.get(&(cx + dx, cz + dz)).map_or(&[][..], Vec::as_slice) {
                    let q = &points[j];
                    let (ex, ez) = (q.x - p.x, q.z - p.z);
                    if ex * ex + ez * ez < q.spread_m * q.spread_m {
                        cover = Some(cover.map_or(q.casts_shade, |c| c.max(q.casts_shade)));
                    }
                }
            }
        }
        if p.light <= 3.0 && cover.is_none() {
            out[i] = Some(CompOut::Unshaded);
        } else if p.light >= 7.0 && cover.is_some_and(|c| c >= 4.0) {
            out[i] = Some(CompOut::Shaded);
        }
    }
    out
}

/// A shipped biome preset: its path, the biome text and the ecology profile text.
#[derive(Clone, Copy, Debug)]
pub struct Preset {
    /// `starter/biomes/<name>.biome.ron`.
    pub path: &'static str,
    /// The `.biome.ron` text.
    pub biome: &'static str,
    /// The `.ecology.ron` text.
    pub ecology: &'static str,
}

macro_rules! preset {
    ($name:literal) => {
        Preset {
            path: concat!("starter/biomes/", $name, ".biome.ron"),
            biome: include_str!(concat!("../../starter/biomes/", $name, ".biome.ron")),
            ecology: include_str!(concat!("../../starter/biomes/", $name, ".ecology.ron")),
        }
    };
}

/// The shipped European temperate and boreal presets (ADR-2456), each with its ecology profile.
pub const PRESETS: &[Preset] = &[
    preset!("birch_woodland"),
    preset!("oak_birch_woodland"),
    preset!("beech_forest"),
    preset!("oak_hornbeam_forest"),
    preset!("boreal_spruce_forest"),
    preset!("boreal_pine_forest"),
];

/// The shipped preset at `path` (`starter/biomes/<name>.biome.ron` or `.ecology.ron`), as text.
pub fn starter_text(path: &str) -> Option<&'static str> {
    PRESETS.iter().find_map(|p| {
        if p.path == path {
            Some(p.biome)
        } else if ecology_path_for(p.path) == path {
            Some(p.ecology)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_by_whole_tokens_and_prefer_longer_aliases() {
        let c = SpeciesCatalog::builtin();
        assert_eq!(
            c.match_name("trees/silver_birch_a.glb").map(|s| s.id.as_str()),
            Some("betula_pendula")
        );
        assert_eq!(
            c.match_name("downy_birch").map(|s| s.id.as_str()),
            Some("betula_pubescens")
        );
        assert_eq!(
            c.match_name("oak01").map(|s| s.id.as_str()),
            Some("quercus_robur")
        );
        assert_eq!(
            c.match_name("Norway maple").map(|s| s.id.as_str()),
            Some("acer_platanoides")
        );
        assert_eq!(
            c.match_name("maple").map(|s| s.id.as_str()),
            Some("acer_pseudoplatanus")
        );
        assert_eq!(c.match_name("limestone_rock"), None);
        assert_eq!(c.match_name("pineapple"), None);
        assert_eq!(c.match_name("boulder"), None);
    }

    #[test]
    fn every_builtin_species_has_sane_windows() {
        for s in SpeciesCatalog::builtin().species() {
            assert!(
                window_ok(s.moisture) && (1.0..=10.0).contains(&s.moisture.0) && s.moisture.1 <= 10.0,
                "{}",
                s.id
            );
            assert!(
                window_ok(s.reaction) && s.reaction.0 >= 1.0 && s.reaction.1 <= 9.0,
                "{}",
                s.id
            );
            assert!(
                (s.moisture.0..=s.moisture.1).contains(&s.indicator.0),
                "{} moisture indicator",
                s.id
            );
            assert!(
                (s.reaction.0..=s.reaction.1).contains(&s.indicator.1),
                "{} reaction indicator",
                s.id
            );
            assert!(
                (1.0..=9.0).contains(&s.light) && (1.0..=5.0).contains(&s.shade_tolerance),
                "{}",
                s.id
            );
            assert!(window_ok(s.altitude_m) && s.max_slope_deg > 0.0, "{}", s.id);
            let woody = s.stratum.is_tree() || s.stratum == Stratum::Shrub;
            assert_eq!(
                woody,
                s.casts_shade > 0.0,
                "{}: only woody plants cast shade",
                s.id
            );
        }
    }

    #[test]
    fn profile_round_trips_and_refuses_newer() {
        let p = EcologyProfile {
            format_version: 1,
            site: Site {
                moisture: (5.0, 6.0),
                reaction: (3.0, 4.0),
                altitude_m: (0.0, 800.0),
                slope_deg: (0.0, 30.0),
                soil: "brown earth".into(),
            },
            species: vec![SpeciesUse {
                entry: "canopy/oak".into(),
                species: "quercus_robur".into(),
                role: Role::Dominant,
                share: Some(1.0),
            }],
            competition: Competition {
                shade_rules: true,
                self_thinning: Some(SelfThinning {
                    cell_m: 20.0,
                    max_stems_per_ha: 300.0,
                    exponent: 1.605,
                }),
            },
            custom_species: Vec::new(),
        };
        let text = p.to_ron();
        assert_eq!(EcologyProfile::from_ron(&text), Ok(p));
        assert!(EcologyProfile::from_ron(&text.replace("format_version: 1", "format_version: 2")).is_err());
        assert!(EcologyProfile::from_ron("EcologyProfile(site: 3)").is_err());
    }

    #[test]
    fn ecology_paths_sit_next_to_the_biome() {
        assert_eq!(
            ecology_path_for("biomes/birch.biome.ron"),
            "biomes/birch.ecology.ron"
        );
        assert_eq!(ecology_path_for("x.ron"), "x.ron.ecology.ron");
    }

    fn pt(x: f32, z: f32, stratum: Stratum, light: f64) -> CompPoint {
        CompPoint {
            x,
            z,
            spread_m: 0.0,
            size: 1.0,
            seed: 0,
            stratum: Some(stratum),
            light,
            casts_shade: 0.0,
            vernal: false,
            shade_rules: true,
            thinning: None,
        }
    }

    #[test]
    fn shade_rules_keep_shade_plants_under_crowns_and_light_plants_out_of_deep_shade() {
        let mut beech = pt(0.0, 0.0, Stratum::Canopy, 3.0);
        beech.spread_m = 5.0;
        beech.casts_shade = 5.0;
        let pts = [
            beech,
            pt(1.0, 1.0, Stratum::Field, 2.0), // sorrel under the beech: kept
            pt(20.0, 0.0, Stratum::Field, 2.0), // sorrel in the open: removed
            pt(2.0, 0.0, Stratum::Field, 8.0), // heather under the beech: removed
            pt(30.0, 0.0, Stratum::Field, 8.0), // heather in the open: kept
            CompPoint {
                vernal: true,
                ..pt(25.0, 0.0, Stratum::Field, 2.0)
            }, // spring plant: exempt
            CompPoint {
                stratum: None,
                ..pt(40.0, 0.0, Stratum::Field, 2.0)
            }, // a rock: untouched
        ];
        let out = competition_pass(&pts);
        assert_eq!(
            out,
            vec![
                None,
                None,
                Some(CompOut::Unshaded),
                Some(CompOut::Shaded),
                None,
                None,
                None
            ]
        );
        // Order does not matter.
        let mut rev = pts.to_vec();
        rev.reverse();
        let mut back = competition_pass(&rev);
        back.reverse();
        assert_eq!(back, out);
    }

    #[test]
    fn self_thinning_removes_the_smallest_stems_until_the_cell_fits() {
        let rule = SelfThinning {
            cell_m: 100.0, // 1 ha: budget = max_stems_per_ha
            max_stems_per_ha: 3.0,
            exponent: 1.0,
        };
        let tree = |x: f32, size: f32, seed: u32| CompPoint {
            size,
            seed,
            thinning: Some((0, rule)),
            ..pt(x, 10.0, Stratum::Canopy, 7.0)
        };
        let pts = [
            tree(10.0, 1.5, 1),
            tree(20.0, 0.5, 2),
            tree(30.0, 1.0, 3),
            tree(40.0, 0.5, 4),
            tree(50.0, 1.2, 5),
        ];
        let out = competition_pass(&pts);
        // Sizes 1.5 + 0.5 + 1.0 + 0.5 + 1.2 = 4.7 > 3: drop 0.5 (seed 2), 0.5 (seed 4) → 3.7, then 1.0 → 2.7.
        assert_eq!(
            out,
            vec![
                None,
                Some(CompOut::Thinned),
                Some(CompOut::Thinned),
                Some(CompOut::Thinned),
                None
            ]
        );
        // Another group in the same cell has its own budget.
        let mut other = pts.to_vec();
        for p in &mut other {
            p.thinning = Some((1, rule));
        }
        let mut both = pts.to_vec();
        both.extend(other);
        let out2 = competition_pass(&both);
        assert_eq!(out2.iter().filter(|o| o.is_some()).count(), 6);
    }
}
