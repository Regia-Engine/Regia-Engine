// SPDX-License-Identifier: MPL-2.0
//! Font subsetting at cook time (`docs/specs/12-assets.md` § 7, ADR-2697): a shipped `.ttf` keeps the outlines of the
//! characters the shipped locales use and loses the rest. No font-writing crate: fonts are read with `skrifa` (the
//! version `cosmic-text` already uses) and written here, from the table layouts of the public OpenType specification.
//!
//! **Retain-glyph-ids mode.** The output has the same glyph count and the same tables as the input. Only three things
//! change: `glyf` and `loca` (a glyph outside the closure gets a zero-length entry), `gvar` (the same for its variation
//! data) and `head` (`indexToLocFormat`, `checkSumAdjustment`). `cmap`, `GSUB`, `GPOS`, `GDEF`, `hmtx` and the rest are
//! copied byte for byte, so text made of kept characters shapes to the same glyph ids and advances. `DSIG` is the one
//! table dropped: its signature covers bytes that changed.
//!
//! **The closure.** Characters map through `cmap` (every Unicode subtable, with format 14 variants) to glyphs, and the
//! locale text's distinct grapheme clusters are shaped with the engine's shaper ([`shaped_glyph_ids`]): whatever a shaper
//! does to the text before it reaches the font (composing and partly decomposing accents, mirroring brackets in
//! right-to-left text, splitting Thai and Khmer vowels) shows up as the glyphs it picks, so no table of those rules is
//! written down here. `GSUB` lookups then
//! run to a fixpoint over the kept set, every lookup of the list, whether or not a feature names it: single, multiple,
//! alternate and reverse chaining substitutions add their outputs, a ligature adds its output when all its inputs are
//! kept. A contextual or chained lookup adds nothing itself: the lookups it calls are in the same list and already
//! apply without their context, a safe superset. Composite components come last (`glyf` flags); they never enter the
//! glyph stream, so they do not feed `GSUB`. Glyph 0 is always kept.
//!
//! **A character outside the set still maps through `cmap`, to an empty glyph:** it draws nothing, with its advance,
//! and no fallback font is asked. A font that draws text the locales do not hold (player names, chat) needs
//! `subset: off` or `extra_chars`.
//!
//! **Kept whole, with a warning:** CFF and CFF2 outlines ([`KeptWhole::Cff`]), `.ttc` collections
//! ([`KeptWhole::Collection`]), `COLR` color fonts, whose layers are glyphs reached by no rule above
//! ([`KeptWhole::Color`]), `morx` fonts, where the shaper ignores `GSUB` ([`KeptWhole::Aat`]), fonts whose character map
//! this reader cannot follow but the shaper can ([`KeptWhole::Cmap`]), fonts with no outlines to drop, such as bitmap
//! color fonts ([`KeptWhole::NoOutlines`]), and a project with no localization ([`NO_LOCALES_KEPT`]): nothing says which
//! characters it uses.
//!
//! **Settings** are the `settings` string (RON) of the font's `.meta`:
//!
//! ```ron
//! (subset: locales(["fr", "de"]), extra_chars: "©®", keep_ascii: true, keep_punctuation: true, keep_pseudo: false)
//! ```
//!
//! `subset` is `auto` (the default: the locales the package ships), `off`, or `locales([...])`. The shape of the
//! `.meta` does not change: `settings` was already free-form per importer.
//!
//! **How the bytes ship.** Fonts ship as raw files the game reads through `assets://`, and that stays. The cook row
//! ([`cook_font`]) runs the subset, reports it in [`FONT_SUBSETS_FILE`] for `assets.font_subset` and records a cache key
//! holding the character set and the shaped glyphs, so a translation edit that changes them recooks the font. Its cooked
//! file is the report row and no manifest row exists, so no pak holds a font. `regia package` puts the subset in place of
//! the raw copy ([`stage_fonts`]), derived again from the current source and locales: a package skips its cook when the
//! paks are fresh, and neither a font nor an `.ftl` makes a pak stale. The subset is deterministic, so what ships is what
//! the cook reported.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use cosmic_text as ct;
use regia_asset::{AssetId, MetaSidecar, meta_path_for};
use regia_ui::loc_bundle::locales_to_ship;
use regia_ui::loc_config::{LOCALIZATION_CONFIG_PATH, LocalizationConfig};
use regia_ui::loc_glyphs::{FontCoverage, is_invisible, text_runs};
use regia_ui::loc_pseudo::{PseudoOptions, pseudo_localize};
use serde::{Deserialize, Serialize};
use skrifa::MetadataProvider;
use skrifa::charmap::MapVariant;
use skrifa::raw::tables::cmap::PlatformId;
use skrifa::raw::tables::gsub::{SingleSubst, SubstitutionLookup, SubstitutionSubtables};
use skrifa::raw::types::{BigEndian, GlyphId16};
use skrifa::raw::{FontRef, ReadError, TableProvider};
use unicode_segmentation::UnicodeSegmentation;

use crate::cook::CookError;
use crate::db::AssetDatabase;

/// The importer name the cook records for font files.
pub const FONT_IMPORTER: &str = "font";
/// The type path of a cooked font row (it never reaches a manifest or a pak).
pub const COOKED_FONT_TYPE: &str = "regia_asset_pipeline::CookedFont";
/// The report of the last cook, in the cooked cache.
pub const FONT_SUBSETS_FILE: &str = "font_subsets.json";
/// `format_version` of [`FONT_SUBSETS_FILE`].
pub const FONT_SUBSETS_VERSION: u32 = 1;
/// The warning of a project with no localization to read a character set from.
pub const NO_LOCALES_KEPT: &str = "font_subset_no_locales_kept";
/// The warning of a font the subsetter refused: it ships as it is.
pub const UNREADABLE_KEPT: &str = "font_subset_unreadable_kept";
/// The cook error of a `.meta` whose `settings` do not parse.
pub const BAD_SETTINGS: &str = "font_subset_bad_settings";
/// The cook error of `subset: locales([...])` naming a locale the project does not have.
pub const UNKNOWN_LOCALE: &str = "font_subset_unknown_locale";

/// Part of the cook key: bump when the same input cooks to different bytes.
const SUBSET_REVISION: &str = "font-subset-1";
/// Where a project's locales live, as `regia loc` reads them.
const LOCALIZATION_DIR: &str = "assets/localization";
/// Closure steps before a font is refused. Real fonts take a few million; overlapping offsets and coverage ranges in a
/// hostile `GSUB` can otherwise claim far more work than its bytes.
const MAX_CLOSURE_WORK: u64 = 1 << 26;

const TAG_CFF: [u8; 4] = *b"CFF ";
const TAG_CFF2: [u8; 4] = *b"CFF2";
const TAG_COLR: [u8; 4] = *b"COLR";
const TAG_DSIG: [u8; 4] = *b"DSIG";
const TAG_MORX: [u8; 4] = *b"morx";
const TAG_GLYF: [u8; 4] = *b"glyf";
const TAG_GVAR: [u8; 4] = *b"gvar";
const TAG_HEAD: [u8; 4] = *b"head";
const TAG_LOCA: [u8; 4] = *b"loca";
const TAG_MAXP: [u8; 4] = *b"maxp";
const MAGIC_TRUETYPE: [u8; 4] = [0, 1, 0, 0];
const MAGIC_TRUE: [u8; 4] = *b"true";
const MAGIC_CFF: [u8; 4] = *b"OTTO";
const MAGIC_COLLECTION: [u8; 4] = *b"ttcf";

/// Why a font could not be read or written. Each has a stable code ([`Self::code`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FontSubsetError {
    /// The bytes do not start like an sfnt font.
    #[error("not a TrueType or OpenType font")]
    NotAFont,
    /// A table or field reaches past the end of its data.
    #[error("truncated font: {0}")]
    Truncated(String),
    /// A table breaks the OpenType specification in a way that changes what the subset would hold.
    #[error("malformed font: {0}")]
    Malformed(String),
    /// The glyph closure exceeded its work limit.
    #[error("glyph closure exceeded its work limit (overlapping GSUB offsets?)")]
    TooComplex,
    /// `.meta` settings that do not parse.
    #[error("{BAD_SETTINGS}: {0}")]
    BadSettings(String),
    /// A requested locale the project does not have.
    #[error("{UNKNOWN_LOCALE}: no locale `{0}` under {LOCALIZATION_DIR}")]
    UnknownLocale(String),
}

impl FontSubsetError {
    /// The stable code (agent and CLI output).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAFont => "font_subset_not_a_font",
            Self::Truncated(_) => "font_subset_truncated",
            Self::Malformed(_) => "font_subset_malformed",
            Self::TooComplex => "font_subset_too_complex",
            Self::BadSettings(_) => BAD_SETTINGS,
            Self::UnknownLocale(_) => UNKNOWN_LOCALE,
        }
    }
}

fn malformed(table: &str, why: impl std::fmt::Display) -> FontSubsetError {
    FontSubsetError::Malformed(format!("{table}: {why}"))
}

fn truncated(what: &str) -> FontSubsetError {
    FontSubsetError::Truncated(what.to_owned())
}

/// Why a valid font was not subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeptWhole {
    /// CFF or CFF2 outlines.
    Cff,
    /// A `.ttc` or `.otc` collection.
    Collection,
    /// A `COLR` color font.
    Color,
    /// A `morx` table: the shaper applies it instead of `GSUB`, and the closure does not read it.
    Aat,
    /// No character map this reader follows, though the shaper may have one (format 0, 6, 10 or 13, MacRoman).
    Cmap,
    /// No `glyf` table: bitmap outlines only.
    NoOutlines,
}

impl KeptWhole {
    /// The stable warning code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Cff => "font_subset_cff_kept",
            Self::Collection => "font_subset_collection_kept",
            Self::Color => "font_subset_color_kept",
            Self::Aat => "font_subset_aat_kept",
            Self::Cmap => "font_subset_cmap_kept",
            Self::NoOutlines => "font_subset_no_outlines_kept",
        }
    }
}

/// What [`subset_font`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubsetOutput {
    /// The font to ship: the subset, or the input when it was kept whole or nothing could be saved.
    pub bytes: Vec<u8>,
    /// Set when the font was kept whole.
    pub kept_whole: Option<KeptWhole>,
    /// Glyphs in the font.
    pub glyphs_total: u32,
    /// Glyphs that keep their outline (all of them when kept whole).
    pub glyphs_kept: u32,
}

/// Big-endian `u16` at `at`, or `None` past the end.
fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// Big-endian `u32` at `at`, or `None` past the end.
fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// The table directory of an sfnt file.
struct Directory<'a> {
    magic: [u8; 4],
    tables: Vec<([u8; 4], &'a [u8])>,
}

impl<'a> Directory<'a> {
    fn parse(data: &'a [u8]) -> Result<Self, FontSubsetError> {
        let magic: [u8; 4] = data
            .get(..4)
            .and_then(|m| m.try_into().ok())
            .ok_or(FontSubsetError::NotAFont)?;
        if ![MAGIC_TRUETYPE, MAGIC_TRUE, MAGIC_CFF].contains(&magic) {
            return Err(FontSubsetError::NotAFont);
        }
        let count = usize::from(u16_at(data, 4).ok_or_else(|| truncated("sfnt header"))?);
        let mut tables: Vec<([u8; 4], &'a [u8])> = Vec::with_capacity(count);
        for index in 0..count {
            let at = 12 + 16 * index;
            let record = data
                .get(at..at + 16)
                .ok_or_else(|| truncated("table directory"))?;
            let tag = [record[0], record[1], record[2], record[3]];
            // `skrifa` finds tables by binary search: an unsorted directory hides tables from it, and a closure that
            // cannot see `GSUB` would drop glyphs a shaper that scans the directory still reaches.
            if tables.last().is_some_and(|(last, _)| *last >= tag) {
                return Err(malformed("table directory", "tags are not in ascending order"));
            }
            let offset = u32::from_be_bytes([record[8], record[9], record[10], record[11]]) as usize;
            let length = u32::from_be_bytes([record[12], record[13], record[14], record[15]]) as usize;
            let table = offset
                .checked_add(length)
                .and_then(|end| data.get(offset..end))
                .ok_or_else(|| truncated(&format!("table {}", String::from_utf8_lossy(&tag))))?;
            tables.push((tag, table));
        }
        Ok(Self { magic, tables })
    }

    fn table(&self, tag: [u8; 4]) -> Option<&'a [u8]> {
        self.tables.iter().find(|(t, _)| *t == tag).map(|(_, data)| *data)
    }

    fn require(&self, tag: [u8; 4]) -> Result<&'a [u8], FontSubsetError> {
        self.table(tag)
            .ok_or_else(|| malformed(&String::from_utf8_lossy(&tag), "required table is missing"))
    }
}

/// `count + 1` offsets of a short or long offset array, each at most `limit` and none smaller than the one before.
fn read_offsets(
    table: &str,
    data: &[u8],
    count: usize,
    long: bool,
    limit: usize,
) -> Result<Vec<usize>, FontSubsetError> {
    let mut out = Vec::with_capacity(count + 1);
    for index in 0..=count {
        let offset = if long {
            u32_at(data, index * 4).map(|v| v as usize)
        } else {
            u16_at(data, index * 2).map(|v| usize::from(v) * 2)
        }
        .ok_or_else(|| truncated(table))?;
        if offset > limit || out.last().is_some_and(|last| offset < *last) {
            return Err(malformed(table, format!("bad offset for glyph {index}")));
        }
        out.push(offset);
    }
    Ok(out)
}

/// Calls `visit` with each component of a composite glyph; a simple or empty glyph has none.
fn visit_components(glyph: &[u8], mut visit: impl FnMut(u16)) -> Result<(), FontSubsetError> {
    const ARG_1_AND_2_ARE_WORDS: u16 = 0x0001;
    const WE_HAVE_A_SCALE: u16 = 0x0008;
    const MORE_COMPONENTS: u16 = 0x0020;
    const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 0x0040;
    const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;
    if glyph.is_empty() {
        return Ok(());
    }
    let contours = u16_at(glyph, 0).ok_or_else(|| truncated("glyph header"))? as i16;
    if contours >= 0 {
        return Ok(());
    }
    let mut at = 10usize;
    loop {
        let flags = u16_at(glyph, at).ok_or_else(|| truncated("composite glyph"))?;
        let component = u16_at(glyph, at + 2).ok_or_else(|| truncated("composite glyph"))?;
        visit(component);
        at += 4;
        at += if flags & ARG_1_AND_2_ARE_WORDS != 0 { 4 } else { 2 };
        if flags & WE_HAVE_A_SCALE != 0 {
            at += 2;
        } else if flags & WE_HAVE_AN_X_AND_Y_SCALE != 0 {
            at += 4;
        } else if flags & WE_HAVE_A_TWO_BY_TWO != 0 {
            at += 8;
        }
        if flags & MORE_COMPONENTS == 0 {
            return Ok(());
        }
    }
}

/// The growing kept set, with its work budget.
struct Closure {
    kept: Vec<bool>,
    changed: bool,
    work: u64,
}

impl Closure {
    fn kept(&self, glyph: u16) -> bool {
        self.kept.get(usize::from(glyph)).copied().unwrap_or(false)
    }

    /// A glyph id past the font's count is ignored: no shaper can draw it either.
    fn add(&mut self, glyph: u16) {
        if let Some(slot) = self.kept.get_mut(usize::from(glyph))
            && !*slot
        {
            *slot = true;
            self.changed = true;
        }
    }

    fn spend(&mut self, steps: u64) -> Result<(), FontSubsetError> {
        self.work = self.work.saturating_add(steps);
        if self.work > MAX_CLOSURE_WORK {
            return Err(FontSubsetError::TooComplex);
        }
        Ok(())
    }

    /// One pass of one lookup over the kept set.
    fn apply(&mut self, lookup: &SubstitutionLookup<'_>) -> Result<(), FontSubsetError> {
        match lookup.subtables().map_err(gsub_error)? {
            SubstitutionSubtables::Single(subtables) => {
                for subtable in subtables.iter() {
                    self.spend(1)?;
                    match subtable.map_err(gsub_error)? {
                        SingleSubst::Format1(t) => {
                            let delta = t.delta_glyph_id();
                            for glyph in t.coverage().map_err(gsub_error)?.iter() {
                                self.spend(1)?;
                                let glyph = glyph.to_u16();
                                if self.kept(glyph) {
                                    self.add(glyph.wrapping_add_signed(delta));
                                }
                            }
                        }
                        SingleSubst::Format2(t) => {
                            let outputs = t.substitute_glyph_ids();
                            for (index, glyph) in t.coverage().map_err(gsub_error)?.iter().enumerate() {
                                self.spend(1)?;
                                if self.kept(glyph.to_u16())
                                    && let Some(output) = outputs.get(index)
                                {
                                    self.add(output.get().to_u16());
                                }
                            }
                        }
                    }
                }
            }
            SubstitutionSubtables::Multiple(subtables) => {
                for subtable in subtables.iter() {
                    self.spend(1)?;
                    let t = subtable.map_err(gsub_error)?;
                    let sequences = t.sequences();
                    for (index, glyph) in t.coverage().map_err(gsub_error)?.iter().enumerate() {
                        self.spend(1)?;
                        if self.kept(glyph.to_u16()) {
                            let sequence = sequences.get(index).map_err(gsub_error)?;
                            self.add_all(sequence.substitute_glyph_ids())?;
                        }
                    }
                }
            }
            SubstitutionSubtables::Alternate(subtables) => {
                for subtable in subtables.iter() {
                    self.spend(1)?;
                    let t = subtable.map_err(gsub_error)?;
                    let sets = t.alternate_sets();
                    for (index, glyph) in t.coverage().map_err(gsub_error)?.iter().enumerate() {
                        self.spend(1)?;
                        if self.kept(glyph.to_u16()) {
                            let set = sets.get(index).map_err(gsub_error)?;
                            self.add_all(set.alternate_glyph_ids())?;
                        }
                    }
                }
            }
            SubstitutionSubtables::Ligature(subtables) => {
                for subtable in subtables.iter() {
                    self.spend(1)?;
                    let t = subtable.map_err(gsub_error)?;
                    let sets = t.ligature_sets();
                    for (index, glyph) in t.coverage().map_err(gsub_error)?.iter().enumerate() {
                        self.spend(1)?;
                        if !self.kept(glyph.to_u16()) {
                            continue;
                        }
                        let set = sets.get(index).map_err(gsub_error)?;
                        for ligature in set.ligatures().iter() {
                            let ligature = ligature.map_err(gsub_error)?;
                            let components = ligature.component_glyph_ids();
                            self.spend(components.len() as u64 + 1)?;
                            if components.iter().all(|c| self.kept(c.get().to_u16())) {
                                self.add(ligature.ligature_glyph().to_u16());
                            }
                        }
                    }
                }
            }
            SubstitutionSubtables::Reverse(subtables) => {
                for subtable in subtables.iter() {
                    self.spend(1)?;
                    let t = subtable.map_err(gsub_error)?;
                    let outputs = t.substitute_glyph_ids();
                    for (index, glyph) in t.coverage().map_err(gsub_error)?.iter().enumerate() {
                        self.spend(1)?;
                        if self.kept(glyph.to_u16())
                            && let Some(output) = outputs.get(index)
                        {
                            self.add(output.get().to_u16());
                        }
                    }
                }
            }
            // The lookups these call are in the same list and run on their own (see the module docs).
            SubstitutionSubtables::Contextual(_) | SubstitutionSubtables::ChainContextual(_) => {}
        }
        Ok(())
    }

    fn add_all(&mut self, glyphs: &[BigEndian<GlyphId16>]) -> Result<(), FontSubsetError> {
        self.spend(glyphs.len() as u64)?;
        for glyph in glyphs {
            self.add(glyph.get().to_u16());
        }
        Ok(())
    }
}

fn gsub_error(e: ReadError) -> FontSubsetError {
    malformed("GSUB", e)
}

/// The glyph closure of `chars` and `glyphs` in `font`, before composites (see the module docs).
fn stream_closure(
    font: &FontRef<'_>,
    glyph_count: usize,
    chars: &BTreeSet<char>,
    glyphs: &BTreeSet<u16>,
) -> Result<Vec<bool>, FontSubsetError> {
    let mut closure = Closure {
        kept: vec![false; glyph_count],
        changed: false,
        work: 0,
    };
    closure.add(0);
    for glyph in glyphs {
        closure.add(*glyph);
    }
    // Every Unicode subtable, not the one `Charmap` prefers: the shaper picks by its own order.
    let cmap = font.cmap().map_err(|e| malformed("cmap", e))?;
    for record in cmap.encoding_records() {
        let unicode = matches!(record.platform_id(), PlatformId::Unicode)
            || (matches!(record.platform_id(), PlatformId::Windows)
                && matches!(record.encoding_id(), 1 | 10));
        if !unicode {
            continue;
        }
        let Ok(subtable) = record.subtable(cmap.offset_data()) else {
            continue;
        };
        for c in chars {
            closure.spend(1)?;
            if let Some(glyph) = subtable.map_codepoint(*c)
                && let Ok(glyph) = u16::try_from(glyph.to_u32())
            {
                closure.add(glyph);
            }
        }
    }
    let charmap = font.charmap();
    // A symbol font's `cmap` is not Unicode: `Charmap` knows the mapping.
    for c in chars {
        if let Some(glyph) = charmap.map(*c)
            && let Ok(glyph) = u16::try_from(glyph.to_u32())
        {
            closure.add(glyph);
        }
    }
    // A base and a variation selector reach a glyph of their own: a text may use any of a kept character's.
    for (base, _, variant) in charmap.variant_mappings() {
        closure.spend(1)?;
        if let MapVariant::Variant(glyph) = variant
            && char::from_u32(base).is_some_and(|c| chars.contains(&c))
            && let Ok(glyph) = u16::try_from(glyph.to_u32())
        {
            closure.add(glyph);
        }
    }
    let gsub = match font.gsub() {
        Ok(gsub) => gsub,
        Err(ReadError::TableIsMissing(_)) => return Ok(closure.kept),
        Err(e) => return Err(gsub_error(e)),
    };
    let lookups = gsub.lookup_list().map_err(gsub_error)?.lookups();
    loop {
        closure.changed = false;
        for lookup in lookups.iter() {
            closure.apply(&lookup.map_err(gsub_error)?)?;
            closure.spend(1)?;
        }
        if !closure.changed {
            return Ok(closure.kept);
        }
    }
}

/// The outlines of a TrueType font: the glyph ranges of `glyf`.
struct GlyphTable<'a> {
    glyf: &'a [u8],
    offsets: Vec<usize>,
}

impl GlyphTable<'_> {
    fn glyph(&self, id: usize) -> &[u8] {
        match (self.offsets.get(id), self.offsets.get(id + 1)) {
            (Some(&start), Some(&end)) => self.glyf.get(start..end).unwrap_or(&[]),
            _ => &[],
        }
    }

    /// Adds every glyph a kept composite refers to, through nested composites.
    fn add_components(&self, kept: &mut [bool]) -> Result<(), FontSubsetError> {
        let mut pending: Vec<usize> = kept
            .iter()
            .enumerate()
            .filter_map(|(id, on)| on.then_some(id))
            .collect();
        while let Some(id) = pending.pop() {
            let mut bad = None;
            visit_components(self.glyph(id), |component| {
                match kept.get_mut(usize::from(component)) {
                    Some(slot) => {
                        if !*slot {
                            *slot = true;
                            pending.push(usize::from(component));
                        }
                    }
                    None => bad = Some(component),
                }
            })?;
            if let Some(component) = bad {
                return Err(malformed(
                    "glyf",
                    format!("glyph {id} names component {component}"),
                ));
            }
        }
        Ok(())
    }

    /// The new `glyf`, and each glyph's new offset: kept glyphs in order, long-aligned, others zero length.
    fn rebuild(&self, kept: &[bool]) -> (Vec<u8>, Vec<u32>) {
        let mut glyf = Vec::new();
        let mut offsets = Vec::with_capacity(kept.len() + 1);
        for (id, on) in kept.iter().enumerate() {
            offsets.push(glyf.len() as u32);
            if *on {
                glyf.extend_from_slice(self.glyph(id));
                glyf.resize(glyf.len().next_multiple_of(4), 0);
            }
        }
        offsets.push(glyf.len() as u32);
        (glyf, offsets)
    }
}

/// Short offsets when they reach (offsets are all even: glyphs are long-aligned), else long.
fn encode_loca(offsets: &[u32]) -> (Vec<u8>, i16) {
    if offsets.last().is_some_and(|last| *last <= 0x1FFFE) {
        let bytes = offsets
            .iter()
            .flat_map(|o| ((*o / 2) as u16).to_be_bytes())
            .collect();
        (bytes, 0)
    } else {
        (offsets.iter().flat_map(|o| o.to_be_bytes()).collect(), 1)
    }
}

/// `gvar` with the variation data of glyphs outside `kept` gone. The shared tuples stay.
fn rebuild_gvar(gvar: &[u8], kept: &[bool]) -> Result<Vec<u8>, FontSubsetError> {
    const HEADER: usize = 20;
    let field = || truncated("gvar header");
    let axis_count = usize::from(u16_at(gvar, 4).ok_or_else(field)?);
    let tuple_count = usize::from(u16_at(gvar, 6).ok_or_else(field)?);
    let shared_at = u32_at(gvar, 8).ok_or_else(field)? as usize;
    let glyph_count = usize::from(u16_at(gvar, 12).ok_or_else(field)?);
    let flags = u16_at(gvar, 14).ok_or_else(field)?;
    let data_at = u32_at(gvar, 16).ok_or_else(field)? as usize;
    if glyph_count != kept.len() {
        return Err(malformed(
            "gvar",
            format!("{glyph_count} glyphs, the font has {}", kept.len()),
        ));
    }
    let long = flags & 1 != 0;
    let data = gvar.get(data_at..).ok_or_else(|| truncated("gvar data"))?;
    let offsets_bytes = gvar.get(HEADER..).ok_or_else(field)?;
    let offsets = read_offsets("gvar", offsets_bytes, glyph_count, long, data.len())?;
    let shared_len = tuple_count * axis_count * 2;
    let shared = shared_at
        .checked_add(shared_len)
        .and_then(|end| gvar.get(shared_at..end))
        .ok_or_else(|| truncated("gvar shared tuples"))?;

    let mut new_data = Vec::new();
    let mut new_offsets = Vec::with_capacity(glyph_count + 1);
    for (id, on) in kept.iter().enumerate() {
        new_offsets.push(new_data.len());
        if *on {
            new_data.extend_from_slice(&data[offsets[id]..offsets[id + 1]]);
            // Short offsets count words, so each entry keeps an even length.
            new_data.resize(new_data.len().next_multiple_of(2), 0);
        }
    }
    new_offsets.push(new_data.len());

    let width = if long { 4 } else { 2 };
    let new_shared_at = HEADER + (glyph_count + 1) * width;
    let new_data_at = new_shared_at + shared.len();
    let mut out = Vec::with_capacity(new_data_at + new_data.len());
    out.extend_from_slice(&gvar[..8]);
    out.extend_from_slice(&(new_shared_at as u32).to_be_bytes());
    out.extend_from_slice(&gvar[12..16]);
    out.extend_from_slice(&(new_data_at as u32).to_be_bytes());
    for offset in new_offsets {
        if long {
            out.extend_from_slice(&(offset as u32).to_be_bytes());
        } else {
            let words = u16::try_from(offset / 2)
                .map_err(|_| malformed("gvar", "short offsets overflow after rebuilding"))?;
            out.extend_from_slice(&words.to_be_bytes());
        }
    }
    out.extend_from_slice(shared);
    out.extend_from_slice(&new_data);
    Ok(out)
}

/// The OpenType table checksum: the big-endian `u32` sum, the last word padded with zeros.
#[must_use]
pub fn table_checksum(table: &[u8]) -> u32 {
    let mut sum = 0u32;
    for chunk in table.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum = sum.wrapping_add(u32::from_be_bytes(word));
    }
    sum
}

/// The padded size of the sfnt file holding `tables`.
fn assembled_len(tables: &[([u8; 4], Cow<'_, [u8]>)]) -> usize {
    tables.iter().fold(12 + 16 * tables.len(), |total, (_, table)| {
        total.saturating_add(table.len().next_multiple_of(4))
    })
}

/// An sfnt file from `tables` (ascending by tag), with every checksum and `head.checkSumAdjustment` computed.
fn assemble(magic: [u8; 4], tables: &[([u8; 4], Cow<'_, [u8]>)]) -> Vec<u8> {
    let count = tables.len();
    // The largest power of two at most `count`, as the sfnt header's binary-search fields want it.
    let floor = if count == 0 { 1 } else { 1usize << count.ilog2() };
    let mut out = Vec::with_capacity(assembled_len(tables));
    out.extend_from_slice(&magic);
    out.extend_from_slice(&(count as u16).to_be_bytes());
    out.extend_from_slice(&((floor * 16) as u16).to_be_bytes());
    out.extend_from_slice(&(floor.trailing_zeros() as u16).to_be_bytes());
    out.extend_from_slice(&(((count - floor.min(count)) * 16) as u16).to_be_bytes());
    let mut at = 12 + 16 * count;
    for (tag, table) in tables {
        out.extend_from_slice(tag);
        out.extend_from_slice(&table_checksum(table).to_be_bytes());
        out.extend_from_slice(&(at as u32).to_be_bytes());
        out.extend_from_slice(&(table.len() as u32).to_be_bytes());
        at += table.len().next_multiple_of(4);
    }
    let mut head_at = None;
    for (tag, table) in tables {
        if *tag == TAG_HEAD {
            head_at = Some(out.len());
        }
        out.extend_from_slice(table);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    if let Some(head_at) = head_at {
        let adjustment = 0xB1B0_AFBAu32.wrapping_sub(table_checksum(&out));
        out[head_at + 8..head_at + 12].copy_from_slice(&adjustment.to_be_bytes());
    }
    out
}

/// The kept glyph ids of a TrueType `data` for `chars`: the whole closure of the module docs.
///
/// # Errors
///
/// The bytes are not a TrueType font with outlines, or its tables are damaged.
pub fn glyph_closure(data: &[u8], chars: &BTreeSet<char>) -> Result<BTreeSet<u16>, FontSubsetError> {
    let directory = Directory::parse(data)?;
    let (kept, _) = closure_of(data, &directory, chars, &BTreeSet::new())?;
    Ok(kept
        .iter()
        .enumerate()
        .filter_map(|(id, on)| on.then_some(id as u16))
        .collect())
}

/// The kept flags and the outlines they index.
fn closure_of<'a>(
    data: &'a [u8],
    directory: &Directory<'a>,
    chars: &BTreeSet<char>,
    glyphs: &BTreeSet<u16>,
) -> Result<(Vec<bool>, GlyphTable<'a>), FontSubsetError> {
    let glyph_count = usize::from(u16_at(directory.require(TAG_MAXP)?, 4).ok_or_else(|| truncated("maxp"))?);
    let head = directory.require(TAG_HEAD)?;
    if head.len() < 54 {
        return Err(truncated("head"));
    }
    if u32_at(head, 12) != Some(0x5F0F_3CF5) {
        return Err(malformed("head", "bad magic number"));
    }
    let long = match u16_at(head, 50) {
        Some(0) => false,
        Some(1) => true,
        other => return Err(malformed("head", format!("indexToLocFormat {other:?}"))),
    };
    let glyf = directory.require(TAG_GLYF)?;
    let offsets = read_offsets(
        "loca",
        directory.require(TAG_LOCA)?,
        glyph_count,
        long,
        glyf.len(),
    )?;
    let table = GlyphTable { glyf, offsets };
    let font = FontRef::new(data).map_err(|e| malformed("sfnt", e))?;
    let mut kept = stream_closure(&font, glyph_count, chars, glyphs)?;
    table.add_components(&mut kept)?;
    Ok((kept, table))
}

/// Why this sfnt is kept whole, judged from its directory.
fn whole_kind(directory: &Directory<'_>) -> Option<KeptWhole> {
    if directory.magic == MAGIC_CFF
        || directory.table(TAG_CFF).is_some()
        || directory.table(TAG_CFF2).is_some()
    {
        Some(KeptWhole::Cff)
    } else if directory.table(TAG_COLR).is_some() {
        Some(KeptWhole::Color)
    } else if directory.table(TAG_MORX).is_some() {
        Some(KeptWhole::Aat)
    } else if directory.table(TAG_GLYF).is_none() {
        Some(KeptWhole::NoOutlines)
    } else {
        None
    }
}

/// Subsets one font to `chars` (see the module docs). A font of a kind the subsetter keeps whole returns its input with
/// [`SubsetOutput::kept_whole`] set; a subset that would not be smaller returns the input too.
///
/// # Errors
///
/// A typed [`FontSubsetError`] for anything that is not a readable font; it never panics on hostile bytes.
pub fn subset_font(data: &[u8], chars: &BTreeSet<char>) -> Result<SubsetOutput, FontSubsetError> {
    subset_font_with(data, chars, &BTreeSet::new())
}

/// [`subset_font`] that also keeps `glyphs`, the output of [`shaped_glyph_ids`].
///
/// # Errors
///
/// As [`subset_font`].
pub fn subset_font_with(
    data: &[u8],
    chars: &BTreeSet<char>,
    glyphs: &BTreeSet<u16>,
) -> Result<SubsetOutput, FontSubsetError> {
    let _span = tracing::debug_span!("font_subset", bytes = data.len(), chars = chars.len()).entered();
    let whole = |why: KeptWhole, glyphs: u16| SubsetOutput {
        bytes: data.to_vec(),
        kept_whole: Some(why),
        glyphs_total: u32::from(glyphs),
        glyphs_kept: u32::from(glyphs),
    };
    let glyphs_of = |font: &FontRef<'_>| {
        font.maxp()
            .map(|maxp| maxp.num_glyphs())
            .map_err(|e| malformed("maxp", e))
    };
    if data.get(..4) == Some(MAGIC_COLLECTION.as_slice()) {
        let face = FontRef::from_index(data, 0).map_err(|e| malformed("ttcf", e))?;
        return Ok(whole(KeptWhole::Collection, glyphs_of(&face)?));
    }
    let directory = Directory::parse(data)?;
    if let Some(why) = whole_kind(&directory) {
        let font = FontRef::new(data).map_err(|e| malformed("sfnt", e))?;
        return Ok(whole(why, glyphs_of(&font)?));
    }
    let font = FontRef::new(data).map_err(|e| malformed("sfnt", e))?;
    if !font.charmap().has_map() {
        return Ok(whole(KeptWhole::Cmap, glyphs_of(&font)?));
    }

    let (kept, table) = closure_of(data, &directory, chars, glyphs)?;
    let glyphs_total = kept.len() as u32;
    let glyphs_kept = kept.iter().filter(|on| **on).count() as u32;
    let unchanged = || SubsetOutput {
        bytes: data.to_vec(),
        kept_whole: None,
        glyphs_total,
        glyphs_kept: glyphs_total,
    };
    let (glyf, offsets) = table.rebuild(&kept);
    let (loca, loc_format) = encode_loca(&offsets);
    let mut tables: Vec<([u8; 4], Cow<'_, [u8]>)> = Vec::with_capacity(directory.tables.len());
    for (tag, bytes) in &directory.tables {
        let rebuilt = match *tag {
            TAG_DSIG => continue,
            TAG_GLYF => Cow::Borrowed(glyf.as_slice()),
            TAG_LOCA => Cow::Borrowed(loca.as_slice()),
            TAG_GVAR => Cow::Owned(rebuild_gvar(bytes, &kept)?),
            TAG_HEAD => {
                let mut head = bytes.to_vec();
                head[8..12].fill(0);
                head[50..52].copy_from_slice(&loc_format.to_be_bytes());
                Cow::Owned(head)
            }
            _ => Cow::Borrowed(*bytes),
        };
        tables.push((*tag, rebuilt));
    }
    // Before any copy: overlapping table ranges can claim far more than the file holds.
    if assembled_len(&tables) >= data.len() {
        return Ok(unchanged());
    }
    Ok(SubsetOutput {
        bytes: assemble(directory.magic, &tables),
        kept_whole: None,
        glyphs_total,
        glyphs_kept,
    })
}

/// The glyphs the engine's shaper uses for `runs` in the font `data`, the font alone in its database. Runs with no
/// character this font maps are skipped, so a fallback chain's fonts only pay for their own scripts. Empty for bytes
/// the font database refuses: the game could not load them either.
#[must_use]
pub fn shaped_glyph_ids(data: &[u8], runs: &BTreeSet<String>) -> BTreeSet<u16> {
    let _span = tracing::debug_span!("font_subset_shape", runs = runs.len()).entered();
    let mut out = BTreeSet::new();
    let Ok(font) = FontRef::new(data) else {
        return out;
    };
    let charmap = font.charmap();
    let mut db = ct::fontdb::Database::new();
    db.load_font_data(data.to_vec());
    let Some(family) = db
        .faces()
        .next()
        .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
    else {
        return out;
    };
    let mut system = ct::FontSystem::new_with_locale_and_db("en-US".to_owned(), db);
    let mut buffer = ct::Buffer::new_empty(ct::Metrics::new(16.0, 20.0));
    buffer.set_wrap(ct::Wrap::None);
    let attrs = ct::Attrs::new().family(ct::Family::Name(&family));
    for run in runs {
        if !run.chars().any(|c| charmap.map(c).is_some()) {
            continue;
        }
        buffer.set_text(run, &attrs, ct::Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut system, false);
        for line in buffer.layout_runs() {
            out.extend(line.glyphs.iter().map(|glyph| glyph.glyph_id));
        }
    }
    out
}

/// How a font is subset.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FontSubsetMode {
    /// The locales the package ships.
    #[default]
    Auto,
    /// The font ships whole.
    Off,
    /// These locales.
    Locales(Vec<String>),
}

/// The `settings` of a font's `.meta` (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontImporterSettings {
    /// How the font is subset.
    pub subset: FontSubsetMode,
    /// More characters to keep: what the game draws that no locale file holds.
    pub extra_chars: String,
    /// Keep printable ASCII, which covers digits and numbers formatted at run time (default on).
    pub keep_ascii: bool,
    /// Keep the punctuation and marks the engine and shapers add themselves (default on; [`engine_punctuation`]).
    pub keep_punctuation: bool,
    /// In `auto`, keep the accented Latin the pseudo-locale draws, so a pseudo-localized test build shows no holes
    /// (default on).
    pub keep_pseudo: bool,
}

impl Default for FontImporterSettings {
    fn default() -> Self {
        Self {
            subset: FontSubsetMode::Auto,
            extra_chars: String::new(),
            keep_ascii: true,
            keep_punctuation: true,
            keep_pseudo: true,
        }
    }
}

impl FontImporterSettings {
    /// Reads a `.meta`'s `settings`. Absent means the defaults.
    ///
    /// # Errors
    ///
    /// [`FontSubsetError::BadSettings`]: a typo must not silently subset a font the author meant to keep.
    pub fn from_meta_string(settings: Option<&str>) -> Result<Self, FontSubsetError> {
        match settings {
            None => Ok(Self::default()),
            Some(text) => ron::from_str(text).map_err(|e| FontSubsetError::BadSettings(e.to_string())),
        }
    }
}

/// Characters the engine or a shaper can put on screen by itself, whatever the locale files hold: the ellipsis of an
/// overflowing label, the dotted circle of a broken cluster, the hyphen a shaper swaps in for U+2011, the replacement
/// character, common quotes and dashes.
#[must_use]
pub fn engine_punctuation() -> BTreeSet<char> {
    "\u{A0}\u{AD}\u{200C}\u{200D}\u{2010}\u{2011}\u{2012}\u{2013}\u{2014}\u{2015}\u{2018}\u{2019}\u{201A}\u{201C}\u{201D}\u{201E}\u{2022}\u{2026}\u{2039}\u{203A}\u{25CC}\u{FFFD}"
        .chars()
        .collect()
}

/// A locale's text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocaleChars {
    /// The visible characters its messages use (`regia_ui::loc_glyphs`).
    pub visible: BTreeSet<char>,
    /// Every character of its messages, invisible ones too.
    pub all: BTreeSet<char>,
    /// The distinct grapheme clusters of its messages, to shape.
    pub runs: BTreeSet<String>,
}

impl LocaleChars {
    fn add(&mut self, text: &str) {
        self.visible.extend(text.chars().filter(|c| !is_invisible(*c)));
        self.all.extend(text.chars());
        self.runs.extend(text.graphemes(true).map(str::to_owned));
    }

    /// The runs to shape: every distinct cluster, and in a locale with right-to-left text each one in a right-to-left
    /// paragraph too, since a sign with no direction of its own still sits in one at run time, where the shaper mirrors
    /// it. Clusters are enough: what a shaper does before it reaches the font happens within one (composing, mirroring,
    /// vowel splits), and what reaches across them (ligatures, joining, conjuncts) is `GSUB`, which the closure covers
    /// whole. So the cost follows the character set, not the amount of text.
    fn shaped_runs(&self) -> impl Iterator<Item = String> + '_ {
        let rtl = self.all.iter().any(|c| is_right_to_left(*c));
        self.runs.iter().flat_map(move |run| {
            let forced = rtl.then(|| format!("\u{200F}{run}"));
            std::iter::once(run.clone()).chain(forced)
        })
    }
}

/// Whether `c` is in a block of a right-to-left script.
fn is_right_to_left(c: char) -> bool {
    matches!(
        c,
        '\u{0590}'..='\u{08FF}' | '\u{FB1D}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFF}' | '\u{10800}'..='\u{10FFF}'
            | '\u{1E800}'..='\u{1EFFF}'
    )
}

/// The character sets of a project's locales.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocaleCharsets {
    /// The source locale.
    pub source: String,
    /// The locales a package ships.
    pub shipped: Vec<String>,
    /// Every locale found, by tag.
    pub locales: BTreeMap<String, LocaleChars>,
}

fn locale_error(path: &Path, e: impl std::fmt::Display) -> String {
    format!("{}: {e}", path.display())
}

/// Reads the locales of `project`: every `assets/localization/<tag>/*.ftl`, which holds what `regia loc gather` wrote
/// too. `Ok(None)` for a project with no localization.
///
/// # Errors
///
/// The config or a locale file cannot be read, or the source locale has no files, as a message: a cook
/// keeps the font whole on it.
pub fn read_project_charsets(project: &Path) -> Result<Option<LocaleCharsets>, String> {
    let dir = project.join(LOCALIZATION_DIR);
    if !dir.is_dir() {
        return Ok(None);
    }
    let config_path = project.join(LOCALIZATION_CONFIG_PATH);
    let config = if config_path.is_file() {
        let text = std::fs::read_to_string(&config_path).map_err(|e| locale_error(&config_path, e))?;
        LocalizationConfig::from_ron(&text).map_err(|e| locale_error(&config_path, e))?
    } else {
        LocalizationConfig::default()
    };
    let source = if config.source.is_empty() {
        "en".to_owned()
    } else {
        config.source.clone()
    };
    let mut out = LocaleCharsets {
        source: source.clone(),
        ..LocaleCharsets::default()
    };
    let entries = std::fs::read_dir(&dir).map_err(|e| locale_error(&dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| locale_error(&dir, e))?;
        let tag = entry.file_name().to_string_lossy().into_owned();
        if tag.starts_with('.') || !entry.path().is_dir() {
            continue;
        }
        let mut chars = LocaleChars::default();
        let mut found = false;
        for file in std::fs::read_dir(entry.path()).map_err(|e| locale_error(&entry.path(), e))? {
            let path = file.map_err(|e| locale_error(&entry.path(), e))?.path();
            if path.extension().is_some_and(|e| e == "ftl") {
                let text = std::fs::read_to_string(&path).map_err(|e| locale_error(&path, e))?;
                found = true;
                for run in text_runs(&text) {
                    chars.add(&run.text);
                }
            }
        }
        if found {
            out.locales.insert(tag, chars);
        }
    }
    if out.locales.is_empty() {
        return Ok(None);
    }
    let available: BTreeSet<String> = out.locales.keys().cloned().collect();
    let requested = if config.locales.is_empty() {
        available.iter().cloned().collect()
    } else {
        config.locales.clone()
    };
    out.shipped =
        locales_to_ship(&requested, &available, &config.fallbacks, &source).map_err(|e| e.to_string())?;
    Ok(Some(out))
}

/// What a font's settings and its project's locales ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontPlan {
    /// `auto`, `off` or `locales`.
    pub mode: &'static str,
    /// The characters to keep; `None` keeps the font whole.
    pub chars: Option<BTreeSet<char>>,
    /// The glyphs the shaper uses for the planned text in this font ([`shaped_glyph_ids`]).
    pub glyphs: BTreeSet<u16>,
    /// The locales the character set comes from.
    pub locales: Vec<String>,
    /// Why there is no character set, when a warning is due ([`NO_LOCALES_KEPT`]).
    pub note: Option<String>,
    /// Every project locale's visible characters: what [`FontPlan::missing`] checks.
    visible: BTreeMap<String, BTreeSet<char>>,
    /// The cook key's part from this plan.
    pub fingerprint: [u8; 32],
}

/// Plans one font, `data` (see the module docs). `charsets` is [`read_project_charsets`]'s result.
///
/// # Errors
///
/// [`FontSubsetError::UnknownLocale`] for a requested locale the project does not have.
pub fn plan_font(
    settings: &FontImporterSettings,
    charsets: &Result<Option<LocaleCharsets>, String>,
    data: &[u8],
) -> Result<FontPlan, FontSubsetError> {
    let mode = match settings.subset {
        FontSubsetMode::Auto => "auto",
        FontSubsetMode::Off => "off",
        FontSubsetMode::Locales(_) => "locales",
    };
    let read = charsets.as_ref().ok().and_then(Option::as_ref);
    let mut plan = FontPlan {
        mode,
        chars: None,
        glyphs: BTreeSet::new(),
        locales: Vec::new(),
        note: None,
        visible: BTreeMap::new(),
        fingerprint: [0; 32],
    };
    if let Some(found) = read {
        plan.visible = found
            .locales
            .iter()
            .map(|(tag, chars)| (tag.clone(), chars.visible.clone()))
            .collect();
    }
    match &settings.subset {
        FontSubsetMode::Off => {}
        FontSubsetMode::Auto | FontSubsetMode::Locales(_) => {
            let wanted: Option<Vec<String>> = match &settings.subset {
                FontSubsetMode::Locales(tags) => Some(tags.clone()),
                _ => read.map(|found| found.shipped.clone()),
            };
            match (wanted, read) {
                (Some(tags), Some(found)) => {
                    let mut chars = BTreeSet::new();
                    let mut runs = BTreeSet::new();
                    for tag in &tags {
                        let locale = found
                            .locales
                            .get(tag)
                            .ok_or_else(|| FontSubsetError::UnknownLocale(tag.clone()))?;
                        chars.extend(locale.all.iter().copied());
                        runs.extend(locale.shaped_runs());
                    }
                    add_always_kept(&mut chars, &mut runs, settings);
                    // A font kept whole has no use for the shaping.
                    if Directory::parse(data).is_ok_and(|d| whole_kind(&d).is_none()) {
                        plan.glyphs = shaped_glyph_ids(data, &runs);
                    }
                    plan.chars = Some(chars);
                    plan.locales = tags;
                }
                (Some(tags), None) if !tags.is_empty() => {
                    return Err(FontSubsetError::UnknownLocale(tags[0].clone()));
                }
                _ => {
                    plan.note = Some(match charsets {
                        Err(why) => format!("{NO_LOCALES_KEPT}: the locales do not read ({why})"),
                        Ok(_) => format!("{NO_LOCALES_KEPT}: the project has no localization"),
                    });
                }
            }
        }
    }
    let mut key = blake3::Hasher::new();
    key.update(SUBSET_REVISION.as_bytes());
    key.update(mode.as_bytes());
    if let Some(chars) = &plan.chars {
        for c in chars {
            key.update(&u32::from(*c).to_le_bytes());
        }
        key.update(b"glyphs");
        for glyph in &plan.glyphs {
            key.update(&glyph.to_le_bytes());
        }
    }
    plan.fingerprint = *key.finalize().as_bytes();
    Ok(plan)
}

/// ASCII, punctuation, pseudo-locale and `extra_chars`, by the settings. The last two are text a shaper sees whole.
fn add_always_kept(chars: &mut BTreeSet<char>, runs: &mut BTreeSet<String>, settings: &FontImporterSettings) {
    if settings.keep_ascii {
        chars.extend((0x20u8..=0x7E).map(char::from));
    }
    if settings.keep_punctuation {
        chars.extend(engine_punctuation());
    }
    let mut add_text = |text: String| {
        chars.extend(text.chars());
        if !text.is_empty() {
            runs.insert(text);
        }
    };
    if settings.keep_pseudo && settings.subset == FontSubsetMode::Auto {
        const LETTERS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        add_text(pseudo_localize(LETTERS, &PseudoOptions::default()));
    }
    add_text(settings.extra_chars.clone());
}

/// The per-locale characters a font does not map, as one string each; locales with none are left out.
#[must_use]
pub fn missing_by_locale(plan: &FontPlan, data: &[u8]) -> BTreeMap<String, String> {
    let Some(coverage) = FontCoverage::from_font_data("", data) else {
        return BTreeMap::new();
    };
    plan.visible
        .iter()
        .filter_map(|(tag, chars)| {
            let missing: String = chars.iter().filter(|c| !coverage.covers(**c)).collect();
            (!missing.is_empty()).then(|| (tag.clone(), missing))
        })
        .collect()
}

/// One font of the cook report ([`FONT_SUBSETS_FILE`], `assets.font_subset`, the cook summary).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontSubsetSummary {
    /// The source's project-relative path.
    pub path: String,
    /// `auto`, `off` or `locales`.
    pub mode: String,
    /// Whether the shipped bytes are smaller than the source.
    pub subset: bool,
    /// The warning code when the font was kept whole on a rule, else `None`.
    pub code: Option<String>,
    /// Glyphs in the font (0 when it does not read).
    pub glyphs_total: u32,
    /// Glyphs that keep their outline.
    pub glyphs_kept: u32,
    /// Source size.
    pub bytes_before: u64,
    /// Shipped size.
    pub bytes_after: u64,
    /// The locales the character set comes from.
    pub locales: Vec<String>,
    /// Per locale, the characters its text uses that this font does not map.
    pub missing: BTreeMap<String, String>,
}

/// [`FONT_SUBSETS_FILE`]: the fonts of the last cook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontSubsetReport {
    /// [`FONT_SUBSETS_VERSION`].
    pub format_version: u32,
    /// One row per font, by path.
    pub fonts: Vec<FontSubsetSummary>,
}

/// What [`run_plan`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFont {
    /// The font to ship.
    pub bytes: Vec<u8>,
    /// Its report row.
    pub summary: FontSubsetSummary,
    /// Warning lines, each with its code.
    pub warnings: Vec<String>,
}

/// Subsets `data` as `plan` asks. A file the subsetter refuses ships as it is, with a warning and [`UNREADABLE_KEPT`]
/// as the row's code.
#[must_use]
pub fn run_plan(path: &str, data: &[u8], plan: &FontPlan) -> PlannedFont {
    let mut warnings = Vec::new();
    if let Some(note) = &plan.note {
        warnings.push(format!("{path}: {note}"));
    }
    let keep_whole = || {
        let glyphs = FontRef::from_index(data, 0)
            .ok()
            .and_then(|font| font.maxp().ok())
            .map_or(0, |maxp| u32::from(maxp.num_glyphs()));
        SubsetOutput {
            bytes: data.to_vec(),
            kept_whole: None,
            glyphs_total: glyphs,
            glyphs_kept: glyphs,
        }
    };
    let (output, code) = match &plan.chars {
        None => (
            keep_whole(),
            plan.note.as_ref().map(|_| NO_LOCALES_KEPT.to_owned()),
        ),
        Some(chars) => match subset_font_with(data, chars, &plan.glyphs) {
            Ok(output) => {
                let code = output.kept_whole.map(|why| why.code().to_owned());
                if let Some(code) = &code {
                    warnings.push(format!("{path}: {code}: this font kind ships whole"));
                }
                (output, code)
            }
            Err(e) => {
                warnings.push(format!(
                    "{path}: {UNREADABLE_KEPT}: {} ({e}); it ships as it is",
                    e.code()
                ));
                (keep_whole(), Some(UNREADABLE_KEPT.to_owned()))
            }
        },
    };
    let summary = FontSubsetSummary {
        path: path.to_owned(),
        mode: plan.mode.to_owned(),
        subset: output.bytes.len() < data.len(),
        code,
        glyphs_total: output.glyphs_total,
        glyphs_kept: output.glyphs_kept,
        bytes_before: data.len() as u64,
        bytes_after: output.bytes.len() as u64,
        locales: plan.locales.clone(),
        missing: missing_by_locale(plan, data),
    };
    PlannedFont {
        bytes: output.bytes,
        summary,
        warnings,
    }
}

/// True for the font file extensions the cook row handles (lowercase, no dot).
#[must_use]
pub fn is_font_extension(ext: &str) -> bool {
    matches!(ext, "ttf" | "otf" | "ttc" | "otc")
}

thread_local! {
    /// The summaries of the fonts cooked on this thread since [`take_summaries`], latest per path.
    static PENDING: RefCell<BTreeMap<String, FontSubsetSummary>> = const { RefCell::new(BTreeMap::new()) };
}

/// Takes the summaries of the fonts cooked on this thread.
#[must_use]
pub fn take_summaries() -> Vec<FontSubsetSummary> {
    PENDING
        .with(|p| std::mem::take(&mut *p.borrow_mut()))
        .into_values()
        .collect()
}

/// What the cook row of a font returns.
pub struct CookedFont {
    /// The asset id from the `.meta`.
    pub id: AssetId,
    /// The settings, the character set and the shaped glyphs, so a translation edit that changes them recooks the font.
    pub settings_hash: [u8; 32],
    /// The cooked file's payload: the report row as JSON. The font's bytes come from [`stage_fonts`].
    pub payload: Vec<u8>,
}

/// Cooks one font (see the module docs). A file that does not read ships as it is with a warning, as it did before this
/// cook row: only an author's own settings fail the cook.
///
/// # Errors
///
/// [`CookError::Font`] for bad `.meta` settings or an unknown locale, [`CookError::Db`] for a `.meta` that cannot be
/// read or written.
pub fn cook_font(
    source: &Path,
    rel_path: &str,
    bytes: &[u8],
    project_root: &Path,
) -> Result<CookedFont, CookError> {
    let _span = tracing::info_span!("cook_font", path = rel_path).entered();
    let meta = AssetDatabase::load_or_create_meta(source, FONT_IMPORTER).map_err(CookError::Db)?;
    let settings = FontImporterSettings::from_meta_string(meta.settings.as_deref())
        .map_err(|e| CookError::Font(format!("{rel_path}: {e}")))?;
    let plan = plan_font(&settings, &read_project_charsets(project_root), bytes)
        .map_err(|e| CookError::Font(format!("{rel_path}: {e}")))?;
    let cooked = run_plan(rel_path, bytes, &plan);
    let payload =
        serde_json::to_vec(&cooked.summary).map_err(|e| CookError::Font(format!("{rel_path}: {e}")))?;
    crate::subasset_remap::push_warnings(cooked.warnings);
    PENDING.with(|p| {
        p.borrow_mut().insert(rel_path.to_owned(), cooked.summary);
    });
    Ok(CookedFont {
        id: meta.id,
        settings_hash: plan.fingerprint,
        payload,
    })
}

/// The report rows of every font in the cooked cache, sorted by path: `cooked` (the fonts this cook ran, from
/// [`take_summaries`]) and, for every other font row of `db`, the row its last cook wrote into its cooked file (a font
/// the incremental cook skipped). Without the second part a cook of an unchanged project wrote a report with no fonts
/// in it. A skipped font whose cooked file does not read has no row (the next cook that runs it writes one).
#[must_use]
pub fn report_rows(
    cache_dir: &Path,
    db: &AssetDatabase,
    cooked: Vec<FontSubsetSummary>,
) -> Vec<FontSubsetSummary> {
    let mut rows: BTreeMap<String, FontSubsetSummary> =
        cooked.into_iter().map(|row| (row.path.clone(), row)).collect();
    for (rel_path, entry) in &db.entries {
        if entry.importer != FONT_IMPORTER || entry.cooked_rel.is_empty() || rows.contains_key(rel_path) {
            continue;
        }
        let Ok(file) = std::fs::read(cache_dir.join(&entry.cooked_rel)) else {
            continue;
        };
        let Ok((_, payload)) = regia_asset::decode_cooked_file(&file) else {
            continue;
        };
        if let Ok(row) = serde_json::from_slice::<FontSubsetSummary>(payload) {
            rows.insert(rel_path.clone(), row);
        }
    }
    rows.into_values().collect()
}

/// Writes [`FONT_SUBSETS_FILE`] into the cooked cache. A report with the same bytes leaves the file alone, so a cook
/// of an unchanged project rewrites no cooked file (ADR-530).
///
/// # Errors
///
/// The file cannot be written.
pub fn write_report(cache_dir: &Path, fonts: &[FontSubsetSummary]) -> std::io::Result<()> {
    let report = FontSubsetReport {
        format_version: FONT_SUBSETS_VERSION,
        fonts: fonts.to_vec(),
    };
    let text = serde_json::to_vec_pretty(&report).map_err(std::io::Error::other)?;
    let path = cache_dir.join(FONT_SUBSETS_FILE);
    if std::fs::read(&path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    crate::cook_ledger::write_atomic(&path, &text)
}

/// One line for the cook summary.
#[must_use]
pub fn summary_line(font: &FontSubsetSummary) -> String {
    let saved = font.bytes_before.saturating_sub(font.bytes_after);
    match &font.code {
        Some(code) => format!("{}: {} bytes kept whole ({code})", font.path, font.bytes_before),
        None if font.mode == "off" => format!(
            "{}: {} bytes kept whole (subset: off)",
            font.path, font.bytes_before
        ),
        None => format!(
            "{}: {} -> {} bytes, {saved} saved, {} of {} glyphs keep their outline ({})",
            font.path,
            font.bytes_before,
            font.bytes_after,
            font.glyphs_kept,
            font.glyphs_total,
            font.locales.join(", ")
        ),
    }
}

fn staged_font_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            staged_font_files(&path, out)?;
        } else if kind.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| is_font_extension(&e.to_ascii_lowercase()))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// The settings a font's `.meta` holds, without creating one.
///
/// # Errors
///
/// The `.meta` or its settings do not read.
pub fn read_font_settings(source: &Path) -> Result<FontImporterSettings, FontSubsetError> {
    let path = meta_path_for(source);
    if !path.is_file() {
        return Ok(FontImporterSettings::default());
    }
    let bytes = std::fs::read(&path).map_err(|e| FontSubsetError::BadSettings(e.to_string()))?;
    let meta = MetaSidecar::from_ron(&bytes).map_err(|e| FontSubsetError::BadSettings(e.to_string()))?;
    FontImporterSettings::from_meta_string(meta.settings.as_deref())
}

/// What [`stage_fonts`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagedFonts {
    /// Staged font files.
    pub fonts: usize,
    /// Of those, the ones now smaller than their source.
    pub subset: usize,
    /// Their source size.
    pub bytes_before: u64,
    /// Their shipped size.
    pub bytes_after: u64,
    /// Warning lines, each with its code.
    pub warnings: Vec<String>,
}

/// Puts the subset in place of each raw font copy under `assets_dir`, the staged `assets/` of a package, derived from the
/// current source and locales (see the module docs).
///
/// # Errors
///
/// The staged tree cannot be walked or written, or a font's settings are bad.
pub fn stage_fonts(project_root: &Path, assets_dir: &Path) -> Result<StagedFonts, CookError> {
    let _span = tracing::info_span!("stage_fonts").entered();
    let mut staged = StagedFonts::default();
    if !assets_dir.is_dir() {
        return Ok(staged);
    }
    let mut files = Vec::new();
    staged_font_files(assets_dir, &mut files)?;
    files.sort();
    if files.is_empty() {
        return Ok(staged);
    }
    let charsets = read_project_charsets(project_root);
    for file in files {
        let Ok(rel) = file.strip_prefix(assets_dir) else {
            continue;
        };
        let source = project_root.join("assets").join(rel);
        if !source.is_file() {
            continue;
        }
        let rel_path = format!("assets/{}", regia_asset::normalise_asset_path(rel));
        let bytes = std::fs::read(&source)?;
        let settings =
            read_font_settings(&source).map_err(|e| CookError::Font(format!("{rel_path}: {e}")))?;
        let plan = plan_font(&settings, &charsets, &bytes)
            .map_err(|e| CookError::Font(format!("{rel_path}: {e}")))?;
        let made = run_plan(&rel_path, &bytes, &plan);
        if made.bytes.len() < bytes.len() {
            crate::cook_ledger::write_atomic(&file, &made.bytes)?;
            staged.subset += 1;
        }
        staged.fonts += 1;
        staged.bytes_before += bytes.len() as u64;
        staged.bytes_after += made.bytes.len() as u64;
        staged.warnings.extend(made.warnings);
    }
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::encode_loca;

    /// Short offsets store half the value and reach 0x1FFFE; one aligned step past it needs long ones.
    #[test]
    fn loca_stays_short_up_to_its_limit() {
        let (short, format) = encode_loca(&[0, 4, 0x1FFFC]);
        assert_eq!((format, short), (0, vec![0, 0, 0, 2, 0xFF, 0xFE]));
        let (long, format) = encode_loca(&[0, 4, 0x2_0000]);
        assert_eq!(format, 1);
        assert_eq!(long, vec![0, 0, 0, 0, 0, 0, 0, 4, 0, 2, 0, 0]);
    }

    /// The same report is not written again (its file keeps its time stamp, ADR-3287); a changed
    /// one is.
    #[test]
    fn an_unchanged_report_keeps_its_file() {
        use super::{FONT_SUBSETS_FILE, FontSubsetSummary, write_report};
        use std::time::{Duration, SystemTime};
        let dir = regia_test_utils::unique_temp_dir("font-subset-report");
        let path = dir.join(FONT_SUBSETS_FILE);
        let font = FontSubsetSummary {
            path: "fonts/ui.ttf".into(),
            mode: "auto".into(),
            subset: true,
            code: None,
            glyphs_total: 10,
            glyphs_kept: 4,
            bytes_before: 1000,
            bytes_after: 400,
            locales: vec!["en".into()],
            missing: std::collections::BTreeMap::new(),
        };
        write_report(&dir, std::slice::from_ref(&font)).expect("first write");
        // A fixed time far from now, so a rewrite always moves it.
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let file = std::fs::OpenOptions::new().write(true).open(&path).expect("open");
        file.set_modified(then).expect("set mtime");
        drop(file);
        let mtime = |p: &std::path::Path| std::fs::metadata(p).expect("meta").modified().expect("mtime");
        write_report(&dir, std::slice::from_ref(&font)).expect("same report");
        assert_eq!(mtime(&path), then, "an unchanged report was rewritten");
        write_report(&dir, &[]).expect("changed report");
        assert_ne!(mtime(&path), then, "a changed report was not written");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
