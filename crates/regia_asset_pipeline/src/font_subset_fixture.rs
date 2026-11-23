// SPDX-License-Identifier: MPL-2.0
//! A tiny TrueType font generated in code, for the [`crate::font_subset`] tests and its fuzz target (ADR-2697): no
//! font file enters the repo and every OpenType structure the subsetter reads is present on purpose. Built from the
//! table layouts of the public OpenType specification.
//!
//! The font has simple outlines (a triangle that differs per glyph), composites (one nested), and one GSUB lookup of
//! each kind the closure handles: ligature, single (both formats), alternate, multiple, a contextual lookup with a
//! nested single that no feature names, an extension, a reverse chaining single, and a two-step chain whose lookups
//! are listed backwards so the closure needs a second pass. [`tiny_variable_font`] adds `fvar` and `gvar`.

/// Glyph ids of the fixture.
pub mod gid {
    /// `.notdef`.
    pub const NOTDEF: u16 = 0;
    /// U+0020: no outline.
    pub const SPACE: u16 = 1;
    /// `f`.
    pub const F: u16 = 2;
    /// `i`.
    pub const I: u16 = 3;
    /// The `f_i` ligature, reachable only through GSUB (feature `liga`).
    pub const F_I: u16 = 4;
    /// `e`.
    pub const E: u16 = 5;
    /// U+0301.
    pub const ACUTE: u16 = 6;
    /// U+00E9, a composite of [`E`] and [`ACUTE`].
    pub const E_ACUTE: u16 = 7;
    /// `x`.
    pub const X: u16 = 8;
    /// Output of a single substitution (format 2) of [`X`].
    pub const X_ALT: u16 = 9;
    /// `a`.
    pub const A: u16 = 10;
    /// Output of an alternate substitution of [`A`].
    pub const A_ALT: u16 = 11;
    /// `m`.
    pub const M: u16 = 12;
    /// First output of a multiple substitution of [`M`].
    pub const M_PART1: u16 = 13;
    /// Second output of a multiple substitution of [`M`].
    pub const M_PART2: u16 = 14;
    /// `n`.
    pub const N: u16 = 15;
    /// Output of the single substitution nested in the contextual lookup on [`N`].
    pub const N_CONTEXT: u16 = 16;
    /// `z`: nothing refers to it.
    pub const Z: u16 = 17;
    /// U+0113, a composite of the composite [`E_ACUTE`] and [`ACUTE`].
    pub const E_MACRON: u16 = 18;
    /// `q`.
    pub const Q: u16 = 19;
    /// [`Q`] through one single substitution.
    pub const Q_STEP1: u16 = 20;
    /// [`Q_STEP1`] through a second single substitution, listed earlier in the lookup list.
    pub const Q_STEP2: u16 = 21;
    /// `k`.
    pub const K: u16 = 22;
    /// Output of a single substitution wrapped in an extension lookup.
    pub const K_EXTENSION: u16 = 23;
    /// `r`.
    pub const R: u16 = 24;
    /// Output of a reverse chaining single substitution of [`R`].
    pub const R_REVERSE: u16 = 25;
    /// How many glyphs the font has.
    pub const COUNT: u16 = 26;
}

/// What the fixture's character map holds.
const CHARS: [(char, u16); 15] = [
    (' ', gid::SPACE),
    ('a', gid::A),
    ('e', gid::E),
    ('f', gid::F),
    ('i', gid::I),
    ('k', gid::K),
    ('m', gid::M),
    ('n', gid::N),
    ('q', gid::Q),
    ('r', gid::R),
    ('x', gid::X),
    ('z', gid::Z),
    ('\u{E9}', gid::E_ACUTE),
    ('\u{113}', gid::E_MACRON),
    ('\u{301}', gid::ACUTE),
];

fn be16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn be32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn words(values: &[u16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// A one-contour triangle, 16-bit coordinates, on-curve, different for every glyph.
fn simple_glyph(id: u16) -> Vec<u8> {
    let left = 40 + id * 10;
    let right = left + 300;
    let top = 500 + id * 4;
    let mut out = Vec::new();
    be16(&mut out, 1); // numberOfContours
    be16(&mut out, left);
    be16(&mut out, 0);
    be16(&mut out, right);
    be16(&mut out, top);
    be16(&mut out, 2); // endPtsOfContours[0]
    be16(&mut out, 0); // instructionLength
    out.extend_from_slice(&[1, 1, 1]); // flags: on curve, long x and y
    for delta in [left as i16, (right - left) as i16, -((right - left) as i16 / 2)] {
        out.extend_from_slice(&delta.to_be_bytes());
    }
    for delta in [0i16, 0, top as i16] {
        out.extend_from_slice(&delta.to_be_bytes());
    }
    out
}

/// A composite of `parts`, each placed with word offsets.
fn composite_glyph(parts: &[u16]) -> Vec<u8> {
    const ARG_1_AND_2_ARE_WORDS: u16 = 0x0001;
    const ARGS_ARE_XY_VALUES: u16 = 0x0002;
    const MORE_COMPONENTS: u16 = 0x0020;
    let mut out = Vec::new();
    out.extend_from_slice(&(-1i16).to_be_bytes());
    for _ in 0..4 {
        be16(&mut out, 0); // bounding box
    }
    for (i, part) in parts.iter().enumerate() {
        let more = if i + 1 < parts.len() { MORE_COMPONENTS } else { 0 };
        be16(&mut out, ARG_1_AND_2_ARE_WORDS | ARGS_ARE_XY_VALUES | more);
        be16(&mut out, *part);
        out.extend_from_slice(&((i as i16) * 20).to_be_bytes());
        out.extend_from_slice(&((i as i16) * 40).to_be_bytes());
    }
    out
}

fn glyph(id: u16) -> Vec<u8> {
    match id {
        gid::SPACE => Vec::new(),
        gid::E_ACUTE => composite_glyph(&[gid::E, gid::ACUTE]),
        gid::E_MACRON => composite_glyph(&[gid::E_ACUTE, gid::ACUTE]),
        _ => simple_glyph(id),
    }
}

/// `glyf` and long `loca`, padded to four bytes per glyph.
fn glyf_and_loca() -> (Vec<u8>, Vec<u8>) {
    let mut glyf = Vec::new();
    let mut loca = Vec::new();
    for id in 0..gid::COUNT {
        be32(&mut loca, glyf.len() as u32);
        glyf.extend(glyph(id));
        while !glyf.len().is_multiple_of(4) {
            glyf.push(0);
        }
    }
    be32(&mut loca, glyf.len() as u32);
    (glyf, loca)
}

fn head_table() -> Vec<u8> {
    let mut out = Vec::new();
    be32(&mut out, 0x0001_0000); // version
    be32(&mut out, 0x0001_0000); // fontRevision
    be32(&mut out, 0); // checksumAdjustment, set when assembled
    be32(&mut out, 0x5F0F_3CF5); // magicNumber
    be16(&mut out, 0); // flags
    be16(&mut out, 1000); // unitsPerEm
    out.extend_from_slice(&[0; 16]); // created, modified
    for value in [0u16, 0, 1000, 1000] {
        be16(&mut out, value); // bounding box
    }
    be16(&mut out, 0); // macStyle
    be16(&mut out, 8); // lowestRecPPEM
    out.extend_from_slice(&2i16.to_be_bytes()); // fontDirectionHint
    be16(&mut out, 1); // indexToLocFormat: long
    be16(&mut out, 0); // glyphDataFormat
    out
}

fn hhea_table() -> Vec<u8> {
    let mut out = Vec::new();
    be32(&mut out, 0x0001_0000);
    out.extend_from_slice(&800i16.to_be_bytes());
    out.extend_from_slice(&(-200i16).to_be_bytes());
    out.extend_from_slice(&0i16.to_be_bytes()); // lineGap
    be16(&mut out, 1000); // advanceWidthMax
    out.extend_from_slice(&[0; 22]); // bearings, extent, caret, reserved, metricDataFormat
    be16(&mut out, gid::COUNT); // numberOfHMetrics
    out
}

fn hmtx_table() -> Vec<u8> {
    let mut out = Vec::new();
    for id in 0..gid::COUNT {
        be16(&mut out, 500 + id * 10);
        be16(&mut out, 0);
    }
    out
}

fn maxp_table() -> Vec<u8> {
    let mut out = Vec::new();
    be32(&mut out, 0x0001_0000);
    be16(&mut out, gid::COUNT);
    be16(&mut out, 16); // maxPoints
    be16(&mut out, 4); // maxContours
    be16(&mut out, 16); // maxCompositePoints
    be16(&mut out, 4); // maxCompositeContours
    be16(&mut out, 1); // maxZones
    out.extend_from_slice(&[0; 12]); // twilight, storage, functions, defs, stack, instructions
    be16(&mut out, 2); // maxComponentElements
    be16(&mut out, 2); // maxComponentDepth
    out
}

/// A format 4 subtable with one segment per character.
fn cmap_table() -> Vec<u8> {
    let mut chars: Vec<(u16, u16)> = CHARS.iter().map(|(c, g)| (u32::from(*c) as u16, *g)).collect();
    chars.sort_unstable();
    let segments = chars.len() as u16 + 1;
    let floor = 1u16 << (15 - segments.leading_zeros());
    let mut sub = Vec::new();
    be16(&mut sub, 4); // format
    be16(&mut sub, 16 + segments * 8); // length
    be16(&mut sub, 0); // language
    be16(&mut sub, segments * 2);
    be16(&mut sub, floor * 2); // searchRange
    be16(&mut sub, floor.trailing_zeros() as u16); // entrySelector
    be16(&mut sub, segments * 2 - floor * 2); // rangeShift
    for (code, _) in &chars {
        be16(&mut sub, *code); // endCode
    }
    be16(&mut sub, 0xFFFF);
    be16(&mut sub, 0); // reservedPad
    for (code, _) in &chars {
        be16(&mut sub, *code); // startCode
    }
    be16(&mut sub, 0xFFFF);
    for (code, glyph) in &chars {
        be16(&mut sub, glyph.wrapping_sub(*code)); // idDelta
    }
    be16(&mut sub, 1);
    for _ in 0..segments {
        be16(&mut sub, 0); // idRangeOffset
    }
    let mut out = Vec::new();
    be16(&mut out, 0); // version
    be16(&mut out, 1); // numTables
    be16(&mut out, 3); // Windows
    be16(&mut out, 1); // Unicode BMP
    be32(&mut out, 12);
    out.extend(sub);
    out
}

fn coverage(glyphs: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    be16(&mut out, 1); // format
    be16(&mut out, glyphs.len() as u16);
    out.extend(words(glyphs));
    out
}

fn lookup(kind: u16, subtable: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    be16(&mut out, kind);
    be16(&mut out, 0); // lookupFlag
    be16(&mut out, 1); // subTableCount
    be16(&mut out, 8); // subtableOffsets[0]
    out.extend(subtable);
    out
}

fn single_delta(from: u16, to: u16) -> Vec<u8> {
    let mut out = Vec::new();
    be16(&mut out, 1); // format
    be16(&mut out, 6); // coverageOffset
    be16(&mut out, to.wrapping_sub(from));
    out.extend(coverage(&[from]));
    out
}

fn single_list(from: u16, to: u16) -> Vec<u8> {
    let mut out = Vec::new();
    be16(&mut out, 2); // format
    be16(&mut out, 8); // coverageOffset
    be16(&mut out, 1); // glyphCount
    be16(&mut out, to);
    out.extend(coverage(&[from]));
    out
}

/// Multiple and alternate substitutions share this shape: one covered glyph, one list of outputs.
fn one_to_many(from: u16, to: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    be16(&mut out, 1); // format
    be16(&mut out, 10 + 2 * to.len() as u16); // coverageOffset
    be16(&mut out, 1); // sequence or alternate set count
    be16(&mut out, 8); // its offset
    be16(&mut out, to.len() as u16);
    out.extend(words(to));
    out.extend(coverage(&[from]));
    out
}

fn ligature(first: u16, second: u16, output: u16) -> Vec<u8> {
    let mut out = Vec::new();
    be16(&mut out, 1); // format
    be16(&mut out, 18); // coverageOffset
    be16(&mut out, 1); // ligSetCount
    be16(&mut out, 8); // ligatureSetOffsets[0]
    be16(&mut out, 1); // ligatureCount
    be16(&mut out, 4); // ligatureOffsets[0]
    be16(&mut out, output);
    be16(&mut out, 2); // componentCount
    be16(&mut out, second);
    out.extend(coverage(&[first]));
    out
}

/// Sequence context format 3: on `input`, apply lookup `nested` at position 0.
fn context(input: u16, nested: u16) -> Vec<u8> {
    let mut out = Vec::new();
    be16(&mut out, 3); // format
    be16(&mut out, 1); // glyphCount
    be16(&mut out, 1); // seqLookupCount
    be16(&mut out, 12); // coverageOffsets[0]
    be16(&mut out, 0); // sequenceIndex
    be16(&mut out, nested); // lookupListIndex
    out.extend(coverage(&[input]));
    out
}

fn extension(kind: u16, subtable: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    be16(&mut out, 1); // format
    be16(&mut out, kind);
    be32(&mut out, 8); // extensionOffset
    out.extend(subtable);
    out
}

fn reverse_single(from: u16, to: u16) -> Vec<u8> {
    let mut out = Vec::new();
    be16(&mut out, 1); // format
    be16(&mut out, 12); // coverageOffset
    be16(&mut out, 0); // backtrackGlyphCount
    be16(&mut out, 0); // lookaheadGlyphCount
    be16(&mut out, 1); // glyphCount
    be16(&mut out, to);
    out.extend(coverage(&[from]));
    out
}

fn gsub_table() -> Vec<u8> {
    let lookups = [
        lookup(4, ligature(gid::F, gid::I, gid::F_I)),
        lookup(1, single_list(gid::X, gid::X_ALT)),
        lookup(3, one_to_many(gid::A, &[gid::A_ALT])),
        lookup(2, one_to_many(gid::M, &[gid::M_PART1, gid::M_PART2])),
        lookup(5, context(gid::N, 5)),
        lookup(1, single_delta(gid::N, gid::N_CONTEXT)),
        // The second step before the first: only a second pass over the list reaches it.
        lookup(1, single_delta(gid::Q_STEP1, gid::Q_STEP2)),
        lookup(1, single_delta(gid::Q, gid::Q_STEP1)),
        lookup(7, extension(1, single_list(gid::K, gid::K_EXTENSION))),
        lookup(8, reverse_single(gid::R, gid::R_REVERSE)),
    ];
    let mut lookup_list = Vec::new();
    be16(&mut lookup_list, lookups.len() as u16);
    let mut at = 2 + 2 * lookups.len() as u16;
    for table in &lookups {
        be16(&mut lookup_list, at);
        at += table.len() as u16;
    }
    for table in lookups {
        lookup_list.extend(table);
    }

    // One script (DFLT) with one feature (`liga` → lookup 0).
    let mut script_list = Vec::new();
    be16(&mut script_list, 1); // scriptCount
    script_list.extend_from_slice(b"DFLT");
    be16(&mut script_list, 8); // scriptOffset
    be16(&mut script_list, 4); // defaultLangSysOffset
    be16(&mut script_list, 0); // langSysCount
    be16(&mut script_list, 0); // lookupOrderOffset
    be16(&mut script_list, 0xFFFF); // requiredFeatureIndex
    be16(&mut script_list, 1); // featureIndexCount
    be16(&mut script_list, 0); // featureIndices[0]

    let mut feature_list = Vec::new();
    be16(&mut feature_list, 1); // featureCount
    feature_list.extend_from_slice(b"liga");
    be16(&mut feature_list, 8); // featureOffset
    be16(&mut feature_list, 0); // featureParamsOffset
    be16(&mut feature_list, 1); // lookupIndexCount
    be16(&mut feature_list, 0); // lookupListIndices[0]

    let mut out = Vec::new();
    be32(&mut out, 0x0001_0000);
    let script_at = 10u16;
    let feature_at = script_at + script_list.len() as u16;
    let lookup_at = feature_at + feature_list.len() as u16;
    be16(&mut out, script_at);
    be16(&mut out, feature_at);
    be16(&mut out, lookup_at);
    out.extend(script_list);
    out.extend(feature_list);
    out.extend(lookup_list);
    out
}

/// A `name` table with the family (ID 1) and PostScript (ID 6) names, which font databases require.
fn name_table() -> Vec<u8> {
    let family: Vec<u8> = "RegiaFixture".encode_utf16().flat_map(u16::to_be_bytes).collect();
    let mut out = Vec::new();
    be16(&mut out, 0); // version
    be16(&mut out, 2); // count
    be16(&mut out, 30); // storageOffset
    for name_id in [1u16, 6] {
        be16(&mut out, 3); // platformID: Windows
        be16(&mut out, 1); // encodingID: Unicode BMP
        be16(&mut out, 0x0409); // languageID: en-US
        be16(&mut out, name_id);
        be16(&mut out, family.len() as u16);
        be16(&mut out, 0); // both records share one string
    }
    out.extend(family);
    out
}

fn post_table() -> Vec<u8> {
    let mut out = Vec::new();
    be32(&mut out, 0x0003_0000); // version 3: no glyph names
    out.extend_from_slice(&[0; 28]);
    out
}

/// A one-axis `fvar` (`wght` 100 to 900, default 400).
fn fvar_table() -> Vec<u8> {
    let mut out = Vec::new();
    be16(&mut out, 1); // majorVersion
    be16(&mut out, 0); // minorVersion
    be16(&mut out, 16); // axesArrayOffset
    be16(&mut out, 2); // reserved
    be16(&mut out, 1); // axisCount
    be16(&mut out, 20); // axisSize
    be16(&mut out, 0); // instanceCount
    be16(&mut out, 8); // instanceSize
    out.extend_from_slice(b"wght");
    be32(&mut out, 100 << 16);
    be32(&mut out, 400 << 16);
    be32(&mut out, 900 << 16);
    be16(&mut out, 0); // flags
    be16(&mut out, 256); // axisNameID
    out
}

/// One embedded-peak tuple moving all 7 points (3 outline, 4 phantom) of a simple glyph: private point numbers with a
/// zero count mean "all points". Padded to an even length, which short `gvar` offsets need.
fn glyph_variation(id: u16) -> Vec<u8> {
    const EMBEDDED_PEAK_TUPLE: u16 = 0x8000;
    const PRIVATE_POINT_NUMBERS: u16 = 0x2000;
    let step = (id % 7 + 1) as u8;
    let mut serialized = vec![0u8]; // point number count: all points
    for _ in 0..2 {
        serialized.push(6); // run of 7 signed byte deltas
        serialized.extend_from_slice(&[step; 7]);
    }
    let mut out = Vec::new();
    be16(&mut out, 1); // tupleVariationCount
    be16(&mut out, 10); // dataOffset
    be16(&mut out, serialized.len() as u16); // variationDataSize
    be16(&mut out, EMBEDDED_PEAK_TUPLE | PRIVATE_POINT_NUMBERS); // tupleIndex
    be16(&mut out, 0x4000); // peak 1.0
    out.extend(serialized);
    out.resize(out.len().next_multiple_of(2), 0);
    out
}

/// `gvar` with short offsets: every simple glyph varies; the shared tuple array holds one unused tuple so the
/// subsetter has to carry it over.
fn gvar_table() -> Vec<u8> {
    let mut data = Vec::new();
    let mut offsets = Vec::new();
    for id in 0..gid::COUNT {
        be16(&mut offsets, (data.len() / 2) as u16);
        if !matches!(id, gid::SPACE | gid::E_ACUTE | gid::E_MACRON) {
            data.extend(glyph_variation(id));
        }
    }
    be16(&mut offsets, (data.len() / 2) as u16);
    let shared = [0x40u8, 0x00];
    let shared_at = 20 + offsets.len() as u32;
    let data_at = shared_at + shared.len() as u32;
    let mut out = Vec::new();
    be16(&mut out, 1); // majorVersion
    be16(&mut out, 0); // minorVersion
    be16(&mut out, 1); // axisCount
    be16(&mut out, 1); // sharedTupleCount
    be32(&mut out, shared_at);
    be16(&mut out, gid::COUNT);
    be16(&mut out, 0); // flags: short offsets
    be32(&mut out, data_at);
    out.extend(offsets);
    out.extend_from_slice(&shared);
    out.extend(data);
    out
}

fn checksum(table: &[u8]) -> u32 {
    let mut sum = 0u32;
    for chunk in table.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum = sum.wrapping_add(u32::from_be_bytes(word));
    }
    sum
}

/// An sfnt file from `tables`, sorted by tag, with its checksums.
fn assemble(magic: [u8; 4], mut tables: Vec<([u8; 4], Vec<u8>)>) -> Vec<u8> {
    tables.sort_by_key(|(tag, _)| *tag);
    let count = tables.len() as u16;
    let floor = 1u16 << (15 - count.leading_zeros());
    let mut out = magic.to_vec();
    be16(&mut out, count);
    be16(&mut out, floor * 16);
    be16(&mut out, floor.trailing_zeros() as u16);
    be16(&mut out, count * 16 - floor * 16);
    let mut at = 12 + 16 * tables.len();
    for (tag, table) in &tables {
        out.extend_from_slice(tag);
        be32(&mut out, checksum(table));
        be32(&mut out, at as u32);
        be32(&mut out, table.len() as u32);
        at += table.len().next_multiple_of(4);
    }
    let mut head_at = None;
    for (tag, table) in &tables {
        if tag == b"head" {
            head_at = Some(out.len());
        }
        out.extend_from_slice(table);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }
    if let Some(head_at) = head_at {
        let adjust = 0xB1B0_AFBAu32.wrapping_sub(checksum(&out));
        out[head_at + 8..head_at + 12].copy_from_slice(&adjust.to_be_bytes());
    }
    out
}

fn base_tables() -> Vec<([u8; 4], Vec<u8>)> {
    let (glyf, loca) = glyf_and_loca();
    vec![
        (*b"GSUB", gsub_table()),
        (*b"cmap", cmap_table()),
        (*b"glyf", glyf),
        (*b"head", head_table()),
        (*b"hhea", hhea_table()),
        (*b"hmtx", hmtx_table()),
        (*b"loca", loca),
        (*b"maxp", maxp_table()),
        (*b"name", name_table()),
        (*b"post", post_table()),
    ]
}

/// The static fixture font (see the module docs).
#[must_use]
pub fn tiny_font() -> Vec<u8> {
    assemble([0, 1, 0, 0], base_tables())
}

/// The fixture font with a `wght` axis: `fvar` and `gvar` added.
#[must_use]
pub fn tiny_variable_font() -> Vec<u8> {
    let mut tables = base_tables();
    tables.push((*b"fvar", fvar_table()));
    tables.push((*b"gvar", gvar_table()));
    assemble([0, 1, 0, 0], tables)
}

/// The fixture with `extra` tables and without the tag `drop`: for the kept-whole cases (`CFF `, `COLR`, no `glyf`).
#[must_use]
pub fn tiny_font_with(magic: [u8; 4], extra: &[([u8; 4], &[u8])], drop: Option<[u8; 4]>) -> Vec<u8> {
    let mut tables = base_tables();
    tables.retain(|(tag, _)| Some(*tag) != drop);
    tables.extend(extra.iter().map(|(tag, data)| (*tag, data.to_vec())));
    assemble(magic, tables)
}

/// A `.ttc` of one face: the fixture with its table offsets moved past the collection header.
#[must_use]
pub fn tiny_collection() -> Vec<u8> {
    const HEADER: usize = 16;
    let font = tiny_font();
    let count = usize::from(u16::from_be_bytes([font[4], font[5]]));
    let mut face = font;
    for record in 0..count {
        let at = 12 + 16 * record + 8;
        let offset = u32::from_be_bytes([face[at], face[at + 1], face[at + 2], face[at + 3]]);
        face[at..at + 4].copy_from_slice(&(offset + HEADER as u32).to_be_bytes());
    }
    let mut out = b"ttcf".to_vec();
    be32(&mut out, 0x0001_0000);
    be32(&mut out, 1); // numFonts
    be32(&mut out, HEADER as u32); // tableDirectoryOffsets[0]
    out.extend(face);
    out
}
