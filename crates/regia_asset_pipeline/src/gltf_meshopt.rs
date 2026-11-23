// SPDX-License-Identifier: MIT
// Portions ported from meshoptimizer (https://github.com/zeux/meshoptimizer, version 0.25 as vendored by the
// `meshopt` 0.6.2 crate: src/vertexfilter.cpp, src/indexcodec.cpp),
// Copyright (c) 2016-2025 Arseny Kapoulkine, MIT licence. See THIRD_PARTY_NOTICES.md.

//! `EXT_meshopt_compression`: one compressed `bufferView` to its plain bytes (ADR-3016).
//!
//! The extension stores a view's bytes in a compressed stream in another buffer. Three stream
//! kinds exist (`mode`): `ATTRIBUTES` and `TRIANGLES` go through the `meshopt` crate's safe decoders
//! (`decode_vertex_buffer`, `decode_index_buffer`), which are the codec's own code and are documented
//! as safe on untrusted input. That crate has no safe entry for `INDICES` or for the post-decode
//! `filter`s (only raw `unsafe` bindings), and this crate denies `unsafe`, so those two are ported
//! here from the same codec's scalar paths. `crate::fuzz_entry::meshopt_load` fuzzes the input.
//!
//! The C decoders `assert` on a stride or count their contract forbids, which would abort the process
//! on a hostile file. [`MeshoptView::from_json`] and [`decode_stream`] check all of that first.

use serde_json::Value;

/// Largest decoded view.
pub const MAX_DECODED_VIEW_BYTES: u64 = 1 << 32;

/// Largest ratio of decoded to encoded bytes accepted. The codec has no way to exceed it: the densest
/// stream it can write, a constant vertex stream in the newer vertex format, is about 1024 to 1, and
/// the index formats spend at least one byte per triangle or per index. The check runs before the
/// output is allocated, so a small hostile file cannot ask for gigabytes.
pub const MAX_DECODE_RATIO: u64 = 2048;

/// Largest vertex stride the vertex codec takes.
const MAX_VERTEX_STRIDE: u64 = 256;

/// Why a compressed view was refused. The message goes into the import error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct MeshoptError(pub String);

/// The `mode` of a compressed view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeshoptMode {
    /// A vertex stream.
    Attributes,
    /// A triangle list in the index codec.
    Triangles,
    /// Any index stream in the index-sequence codec.
    Indices,
}

/// The `filter` applied to decoded vertices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeshoptFilter {
    /// None.
    None,
    /// Octahedral unit vectors in 8 or 16 bits per component.
    Octahedral,
    /// Three-component quaternions in 16 bits per component.
    Quaternion,
    /// 32-bit floats as a 24-bit mantissa and an 8-bit exponent.
    Exponential,
}

/// One `bufferViews[i].extensions.EXT_meshopt_compression` block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeshoptView {
    /// Index of the buffer holding the compressed stream.
    pub buffer: usize,
    /// Start of the stream in that buffer.
    pub byte_offset: u64,
    /// Length of the stream.
    pub byte_length: u64,
    /// Bytes per decoded element.
    pub byte_stride: u64,
    /// Decoded element count.
    pub count: u64,
    /// The stream kind.
    pub mode: MeshoptMode,
    /// The filter.
    pub filter: MeshoptFilter,
}

impl MeshoptView {
    /// Reads and checks a block.
    ///
    /// # Errors
    ///
    /// A field is missing, has the wrong type, or breaks a rule of its `mode` or `filter`.
    pub fn from_json(block: &Value) -> Result<Self, MeshoptError> {
        let number = |key: &str| {
            block
                .get(key)
                .and_then(Value::as_u64)
                .ok_or_else(|| MeshoptError(format!("`{key}` is missing or not an unsigned integer")))
        };
        let mode = match block.get("mode").and_then(Value::as_str) {
            Some("ATTRIBUTES") => MeshoptMode::Attributes,
            Some("TRIANGLES") => MeshoptMode::Triangles,
            Some("INDICES") => MeshoptMode::Indices,
            other => return Err(MeshoptError(format!("unknown `mode` {other:?}"))),
        };
        let filter = match block.get("filter").and_then(Value::as_str) {
            None | Some("NONE") => MeshoptFilter::None,
            Some("OCTAHEDRAL") => MeshoptFilter::Octahedral,
            Some("QUATERNION") => MeshoptFilter::Quaternion,
            Some("EXPONENTIAL") => MeshoptFilter::Exponential,
            Some(other) => return Err(MeshoptError(format!("unknown `filter` {other:?}"))),
        };
        let view = Self {
            buffer: usize::try_from(number("buffer")?)
                .map_err(|_| MeshoptError("`buffer` is out of range".into()))?,
            byte_offset: block.get("byteOffset").map_or(Ok(0), |_| number("byteOffset"))?,
            byte_length: number("byteLength")?,
            byte_stride: number("byteStride")?,
            count: number("count")?,
            mode,
            filter,
        };
        check_stream(view.mode, view.filter, view.byte_stride, view.count)?;
        Ok(view)
    }

    /// Decoded size in bytes (`count * byteStride`).
    #[must_use]
    pub fn decoded_len(&self) -> Option<u64> {
        self.count.checked_mul(self.byte_stride)
    }
}

/// The rules of the extension for one stream, and the decoders' contracts.
fn check_stream(
    mode: MeshoptMode,
    filter: MeshoptFilter,
    stride: u64,
    count: u64,
) -> Result<(), MeshoptError> {
    if count == 0 {
        return Err(MeshoptError("`count` is 0".into()));
    }
    match mode {
        MeshoptMode::Attributes => {
            if stride == 0 || !stride.is_multiple_of(4) || stride > MAX_VERTEX_STRIDE {
                return Err(MeshoptError(format!(
                    "ATTRIBUTES needs a `byteStride` divisible by 4, at most {MAX_VERTEX_STRIDE}, not {stride}"
                )));
            }
        }
        MeshoptMode::Triangles | MeshoptMode::Indices => {
            if stride != 2 && stride != 4 {
                return Err(MeshoptError(format!(
                    "an index stream needs a `byteStride` of 2 or 4, not {stride}"
                )));
            }
            if mode == MeshoptMode::Triangles && !count.is_multiple_of(3) {
                return Err(MeshoptError(format!(
                    "TRIANGLES needs a `count` divisible by 3, not {count}"
                )));
            }
            if filter != MeshoptFilter::None {
                return Err(MeshoptError("a filter applies to ATTRIBUTES only".into()));
            }
        }
    }
    match filter {
        MeshoptFilter::Octahedral if stride != 4 && stride != 8 => Err(MeshoptError(format!(
            "OCTAHEDRAL needs a `byteStride` of 4 or 8, not {stride}"
        ))),
        MeshoptFilter::Quaternion if stride != 8 => Err(MeshoptError(format!(
            "QUATERNION needs a `byteStride` of 8, not {stride}"
        ))),
        _ => Ok(()),
    }
}

/// Decodes one stream into `count * stride` little-endian bytes.
///
/// # Errors
///
/// The arguments break a rule of the stream kind, the decoded size is out of bounds for the input,
/// or the codec refuses the bytes.
pub fn decode_stream(
    mode: MeshoptMode,
    filter: MeshoptFilter,
    stride: u64,
    count: u64,
    encoded: &[u8],
) -> Result<Vec<u8>, MeshoptError> {
    check_stream(mode, filter, stride, count)?;
    let decoded = count
        .checked_mul(stride)
        .filter(|n| *n <= MAX_DECODED_VIEW_BYTES)
        .ok_or_else(|| MeshoptError(format!("{count} elements of {stride} bytes is too large")))?;
    let allowed = (encoded.len() as u64).saturating_mul(MAX_DECODE_RATIO);
    if decoded > allowed {
        return Err(MeshoptError(format!(
            "{decoded} decoded bytes from {} compressed bytes is more than the codec can produce",
            encoded.len()
        )));
    }
    let count = usize::try_from(count).map_err(|_| MeshoptError("`count` is too large".into()))?;
    let mut out = match mode {
        MeshoptMode::Attributes => decode_attributes(stride, count, encoded)?,
        MeshoptMode::Triangles if stride == 2 => {
            le_u16(&meshopt::encoding::decode_index_buffer::<u16>(encoded, count).map_err(codec)?)
        }
        MeshoptMode::Triangles => {
            le_u32(&meshopt::encoding::decode_index_buffer::<u32>(encoded, count).map_err(codec)?)
        }
        MeshoptMode::Indices => decode_index_sequence(stride, count, encoded)?,
    };
    apply_filter(filter, stride, &mut out);
    Ok(out)
}

fn codec(error: meshopt::Error) -> MeshoptError {
    MeshoptError(format!("the compressed stream does not decode: {error}"))
}

fn le_u16(values: &[u16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn le_u32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// One vertex of `N` bytes: the crate's decoder is generic on the vertex type and takes its stride
/// from the type's size, so a stride read from a file needs a type per stride.
#[derive(Clone, Copy)]
struct Vertex<const N: usize>([u8; N]);

impl<const N: usize> Default for Vertex<N> {
    fn default() -> Self {
        Self([0; N])
    }
}

fn decode_vertices<const N: usize>(encoded: &[u8], count: usize) -> Result<Vec<u8>, MeshoptError> {
    let vertices = meshopt::encoding::decode_vertex_buffer::<Vertex<N>>(encoded, count).map_err(codec)?;
    let mut out = Vec::with_capacity(vertices.len().saturating_mul(N));
    for vertex in &vertices {
        out.extend_from_slice(&vertex.0);
    }
    Ok(out)
}

/// `stride` is divisible by 4 and at most 256 ([`check_stream`]).
fn decode_attributes(stride: u64, count: usize, encoded: &[u8]) -> Result<Vec<u8>, MeshoptError> {
    macro_rules! dispatch {
        ($($n:literal),* $(,)?) => {
            match stride {
                $($n => decode_vertices::<$n>(encoded, count),)*
                other => Err(MeshoptError(format!("unsupported `byteStride` {other}"))),
            }
        };
    }
    dispatch!(
        4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 60, 64, 68, 72, 76, 80, 84, 88, 92, 96, 100,
        104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144, 148, 152, 156, 160, 164, 168, 172, 176, 180,
        184, 188, 192, 196, 200, 204, 208, 212, 216, 220, 224, 228, 232, 236, 240, 244, 248, 252, 256,
    )
}

/// `meshopt_decodeIndexSequence`: a header byte `0xD?`, then one variable-length code per index, then
/// a four-byte tail. Bit 0 of a code picks one of two running baselines, and the rest is a zigzag
/// delta from it.
fn decode_index_sequence(stride: u64, count: usize, encoded: &[u8]) -> Result<Vec<u8>, MeshoptError> {
    const HEADER: u8 = 0xd0;
    const MAX_VERSION: u8 = 1;
    const TAIL: usize = 4;
    let short = || MeshoptError("the index sequence is truncated".into());
    if encoded.len() < 1 + count.saturating_add(TAIL) {
        return Err(short());
    }
    let lead = encoded[0];
    if lead & 0xf0 != HEADER || lead & 0x0f > MAX_VERSION {
        return Err(MeshoptError(format!(
            "not an index sequence: header byte {lead:#04x}"
        )));
    }
    let data_end = encoded.len() - TAIL;
    let mut at = 1;
    let mut last = [0u32; 2];
    let mut out = Vec::with_capacity(count.saturating_mul(stride as usize));
    for _ in 0..count {
        if at >= data_end {
            return Err(short());
        }
        let (code, read) = read_vbyte(encoded, at).ok_or_else(short)?;
        at += read;
        let baseline = (code & 1) as usize;
        let delta = code >> 1;
        let delta = (delta >> 1) ^ (delta & 1).wrapping_neg();
        let index = last[baseline].wrapping_add(delta);
        last[baseline] = index;
        if stride == 2 {
            out.extend_from_slice(&(index as u16).to_le_bytes());
        } else {
            out.extend_from_slice(&index.to_le_bytes());
        }
    }
    if at != data_end {
        return Err(MeshoptError(
            "the index sequence has bytes past its last index".into(),
        ));
    }
    Ok(out)
}

/// One little-endian base-128 value of at most five bytes: the value and how many bytes it took.
fn read_vbyte(bytes: &[u8], start: usize) -> Option<(u32, usize)> {
    let mut value = 0u32;
    for i in 0..5 {
        let byte = *bytes.get(start + i)?;
        value |= u32::from(byte & 0x7f).wrapping_shl(7 * i as u32);
        if byte < 0x80 {
            return Some((value, i + 1));
        }
    }
    Some((value, 5))
}

/// Applies `filter` in place. The stride fits the filter ([`check_stream`]).
fn apply_filter(filter: MeshoptFilter, stride: u64, data: &mut [u8]) {
    match (filter, stride) {
        (MeshoptFilter::None, _) => {}
        (MeshoptFilter::Octahedral, 4) => octahedral_8(data),
        (MeshoptFilter::Octahedral, _) => octahedral_16(data),
        (MeshoptFilter::Quaternion, _) => quaternion(data),
        (MeshoptFilter::Exponential, _) => exponential(data),
    }
}

/// The filters round half away from zero (the C code adds or subtracts 0.5, then truncates).
fn round_away(value: f32) -> i32 {
    (value + if value >= 0.0 { 0.5 } else { -0.5 }) as i32
}

/// Rebuilds a unit vector scaled to `max` from `x`, `y` and a `z` that holds one at the same bit count.
fn octahedral(x: f32, y: f32, one: f32, max: f32) -> [i32; 3] {
    let z = one - x.abs() - y.abs();
    let t = if z >= 0.0 { 0.0 } else { z };
    let x = x + if x >= 0.0 { t } else { -t };
    let y = y + if y >= 0.0 { t } else { -t };
    let scale = max / (x * x + y * y + z * z).sqrt();
    [
        round_away(x * scale),
        round_away(y * scale),
        round_away(z * scale),
    ]
}

fn octahedral_8(data: &mut [u8]) {
    for v in data.as_chunks_mut::<4>().0 {
        let [x, y, one, _] = v.map(|b| f32::from(b as i8));
        let [x, y, z] = octahedral(x, y, one, 127.0);
        v[0] = x as u8;
        v[1] = y as u8;
        v[2] = z as u8;
    }
}

fn octahedral_16(data: &mut [u8]) {
    for v in data.as_chunks_mut::<8>().0 {
        let read = |i: usize| f32::from(i16::from_le_bytes([v[i * 2], v[i * 2 + 1]]));
        let [x, y, z] = octahedral(read(0), read(1), read(2), 32767.0);
        for (i, value) in [x, y, z].into_iter().enumerate() {
            v[i * 2..i * 2 + 2].copy_from_slice(&(value as i16).to_le_bytes());
        }
    }
}

fn quaternion(data: &mut [u8]) {
    let scale = 1.0 / 2.0f32.sqrt();
    for v in data.as_chunks_mut::<8>().0 {
        let read = |i: usize| i16::from_le_bytes([v[i * 2], v[i * 2 + 1]]);
        let w = i32::from(read(3));
        // The scale lives in the high bits, the index of the dropped component in the low two.
        let ss = scale / (w | 3) as f32;
        let x = f32::from(read(0)) * ss;
        let y = f32::from(read(1)) * ss;
        let z = f32::from(read(2)) * ss;
        let ww = 1.0 - x * x - y * y - z * z;
        let rebuilt = if ww >= 0.0 { ww } else { 0.0 }.sqrt();
        let which = w & 3;
        for (slot, value) in [
            (which + 1, round_away(x * 32767.0)),
            (which + 2, round_away(y * 32767.0)),
            (which + 3, round_away(z * 32767.0)),
            (which, (rebuilt * 32767.0 + 0.5) as i32),
        ] {
            let at = (slot & 3) as usize * 2;
            v[at..at + 2].copy_from_slice(&(value as i16).to_le_bytes());
        }
    }
}

fn exponential(data: &mut [u8]) {
    for bytes in data.as_chunks_mut::<4>().0 {
        let v = u32::from_le_bytes(*bytes);
        // A signed 24-bit mantissa under a signed 8-bit exponent.
        let mantissa = ((v << 8) as i32) >> 8;
        let exponent = (v as i32) >> 24;
        let power = f32::from_bits(((exponent + 127) as u32) << 23);
        *bytes = (power * mantissa as f32).to_bits().to_le_bytes();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn block() -> Value {
        json!({"buffer":0,"byteLength":100,"byteStride":12,"mode":"ATTRIBUTES","count":4})
    }

    #[test]
    fn a_block_reads_with_its_defaults() {
        let view = MeshoptView::from_json(&block()).expect("view");
        assert_eq!(view.byte_offset, 0);
        assert_eq!(view.filter, MeshoptFilter::None);
        assert_eq!(view.decoded_len(), Some(48));
    }

    #[test]
    fn broken_rules_are_refused_before_any_decoder_runs() {
        let broken = |patch: Value| {
            let mut value = block();
            for (key, field) in patch.as_object().expect("object") {
                value[key.as_str()] = field.clone();
            }
            MeshoptView::from_json(&value).expect_err("refused");
        };
        broken(json!({"byteStride": 6}));
        broken(json!({"byteStride": 0}));
        broken(json!({"byteStride": 260}));
        broken(json!({"count": 0}));
        broken(json!({"mode": "BOGUS"}));
        broken(json!({"filter": "BOGUS"}));
        broken(json!({"mode": "TRIANGLES", "byteStride": 4, "count": 4}));
        broken(json!({"mode": "INDICES", "byteStride": 3}));
        broken(json!({"mode": "INDICES", "byteStride": 4, "filter": "OCTAHEDRAL"}));
        broken(json!({"filter": "OCTAHEDRAL"}));
        broken(json!({"filter": "QUATERNION"}));
        broken(json!({"byteLength": "many"}));
    }

    #[test]
    fn a_huge_count_is_refused_before_it_allocates() {
        let error = decode_stream(
            MeshoptMode::Attributes,
            MeshoptFilter::None,
            256,
            u64::MAX / 512,
            &[0xa0; 64],
        )
        .expect_err("too large");
        assert!(error.0.contains("too large"), "{error}");
        let error = decode_stream(
            MeshoptMode::Attributes,
            MeshoptFilter::None,
            4,
            1 << 24,
            &[0xa0; 64],
        )
        .expect_err("past the ratio");
        assert!(error.0.contains("more than the codec"), "{error}");
    }

    #[test]
    fn vertex_stream_round_trips_for_every_stride_with_the_crate() {
        // Each stride goes through the crate's encoder for the matching vertex type.
        macro_rules! check {
            ($($n:literal),*) => {$(
                let vertices: Vec<[u8; $n]> =
                    (0..300u32).map(|i| std::array::from_fn(|b| (i as usize * 3 + b) as u8)).collect();
                let encoded = meshopt::encoding::encode_vertex_buffer(&vertices).expect("encode");
                let decoded = decode_stream(
                    MeshoptMode::Attributes, MeshoptFilter::None, $n, 300, &encoded,
                ).expect("decode");
                assert_eq!(decoded, vertices.concat(), "stride {}", $n);
            )*};
        }
        check!(4, 8, 12, 16, 32);
    }

    #[test]
    fn triangle_streams_round_trip_in_both_widths() {
        // The codec may rotate a triangle's corners (the winding stays), so compare them rotated to
        // their smallest corner.
        fn canonical(indices: &[u32]) -> Vec<[u32; 3]> {
            indices
                .as_chunks::<3>()
                .0
                .iter()
                .map(|t| {
                    let first = (0..3).min_by_key(|i| t[*i]).unwrap_or(0);
                    [t[first], t[(first + 1) % 3], t[(first + 2) % 3]]
                })
                .collect()
        }
        let triangles: Vec<u32> = vec![0, 1, 2, 2, 1, 3, 3, 1, 4, 4, 5, 6];
        let encoded = meshopt::encoding::encode_index_buffer(&triangles, 7).expect("encode");
        let wide = decode_stream(MeshoptMode::Triangles, MeshoptFilter::None, 4, 12, &encoded).expect("u32");
        let wide: Vec<u32> = wide
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes(*b))
            .collect();
        assert_eq!(canonical(&wide), canonical(&triangles));
        let short = decode_stream(MeshoptMode::Triangles, MeshoptFilter::None, 2, 12, &encoded).expect("u16");
        let short: Vec<u32> = short
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u32::from(u16::from_le_bytes(*b)))
            .collect();
        assert_eq!(canonical(&short), canonical(&triangles));
    }

    /// Calls into the codec's own C code, as the reference for the two ported decoders. The crate
    /// denies `unsafe`, and test code needs it only here (ADR-3016).
    #[allow(unsafe_code)]
    mod reference {
        use meshopt::ffi;

        pub fn encode_sequence(indices: &[u32], vertex_count: usize) -> Vec<u8> {
            // SAFETY: the bound sizes the output, and both pointers are valid for those lengths.
            unsafe {
                let bound = ffi::meshopt_encodeIndexSequenceBound(indices.len(), vertex_count);
                let mut out = vec![0u8; bound];
                let size = ffi::meshopt_encodeIndexSequence(
                    out.as_mut_ptr(),
                    out.len(),
                    indices.as_ptr(),
                    indices.len(),
                );
                out.truncate(size);
                out
            }
        }

        pub fn oct(data: &mut [u8], stride: usize) {
            // SAFETY: `data` holds `len / stride` elements of `stride` bytes.
            unsafe { ffi::meshopt_decodeFilterOct(data.as_mut_ptr().cast(), data.len() / stride, stride) }
        }

        pub fn quat(data: &mut [u8]) {
            // SAFETY: `data` holds `len / 8` elements of 8 bytes.
            unsafe { ffi::meshopt_decodeFilterQuat(data.as_mut_ptr().cast(), data.len() / 8, 8) }
        }

        pub fn exp(data: &mut [u8], stride: usize) {
            // SAFETY: `data` holds `len / stride` elements of `stride` bytes.
            unsafe { ffi::meshopt_decodeFilterExp(data.as_mut_ptr().cast(), data.len() / stride, stride) }
        }
    }

    /// A small deterministic generator (the C code and the port must see the same bytes).
    fn noise(len: usize, mut state: u32) -> Vec<u8> {
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(0x9e37_79b1).wrapping_add(0x7f4a_7c15);
                (state >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn the_index_sequence_decoder_matches_the_codec_encoder() {
        let indices: Vec<u32> = vec![0, 1, 2, 1000, 3, 999, 70_000, 4, 70_001, 5, 5, 5, 0];
        let encoded = reference::encode_sequence(&indices, 70_002);
        let wide = decode_stream(MeshoptMode::Indices, MeshoptFilter::None, 4, 13, &encoded).expect("u32");
        assert_eq!(wide, le_u32(&indices));
        let narrow: Vec<u16> = indices.iter().map(|i| *i as u16).collect();
        let short = decode_stream(MeshoptMode::Indices, MeshoptFilter::None, 2, 13, &encoded).expect("u16");
        assert_eq!(short, le_u16(&narrow));
    }

    #[test]
    fn a_damaged_index_sequence_is_an_error() {
        let encoded = reference::encode_sequence(&[0, 1, 2, 3], 4);
        let decode =
            |bytes: &[u8], count| decode_stream(MeshoptMode::Indices, MeshoptFilter::None, 4, count, bytes);
        assert!(decode(&encoded, 4).is_ok());
        assert!(decode(&encoded[..encoded.len() - 1], 4).is_err(), "cut short");
        assert!(decode(&encoded, 3).is_err(), "bytes left over");
        assert!(decode(&encoded, 5).is_err(), "too few codes");
        let mut bad = encoded.clone();
        bad[0] = 0xa0;
        assert!(decode(&bad, 4).is_err(), "another header");
        bad[0] = 0xdf;
        assert!(decode(&bad, 4).is_err(), "a version from the future");
        assert!(decode(&[], 4).is_err(), "empty");
        // Five continuation bytes per code do not read past the end.
        let mut long = vec![0xd0u8];
        long.extend([0xff; 20]);
        assert!(decode(&long, 4).is_err());
    }

    /// The SIMD paths of the C code round exact halves differently, so the port may differ from it by
    /// one unit in the last place; it never differs by more.
    fn assert_within_one(ours: &[u8], theirs: &[u8], element: usize) {
        for (a, b) in ours.chunks_exact(element).zip(theirs.chunks_exact(element)) {
            let (a, b) = if element == 1 {
                (i32::from(a[0] as i8), i32::from(b[0] as i8))
            } else {
                (
                    i32::from(i16::from_le_bytes([a[0], a[1]])),
                    i32::from(i16::from_le_bytes([b[0], b[1]])),
                )
            };
            assert!((a - b).abs() <= 1, "{a} against {b}");
        }
    }

    /// Valid encoded input: `|x| + |y| <= one`, `z = one`.
    fn oct_16_input(seed: u32) -> Vec<u8> {
        let one = 16383i32;
        let noise = noise(400 * 4, seed);
        let mut data = Vec::new();
        for pair in noise.as_chunks::<4>().0 {
            let x = i32::from(i16::from_le_bytes([pair[0], pair[1]])) % (one / 2);
            let y = i32::from(i16::from_le_bytes([pair[2], pair[3]])) % (one / 2);
            for value in [x, y, one, 777] {
                data.extend_from_slice(&(value as i16).to_le_bytes());
            }
        }
        data
    }

    #[test]
    fn the_octahedral_filter_matches_the_codec() {
        let mut ours = oct_16_input(11);
        let mut theirs = ours.clone();
        apply_filter(MeshoptFilter::Octahedral, 8, &mut ours);
        reference::oct(&mut theirs, 8);
        assert_within_one(&ours, &theirs, 2);
        for v in ours.as_chunks::<8>().0 {
            let read = |i: usize| f32::from(i16::from_le_bytes([v[i * 2], v[i * 2 + 1]])) / 32767.0;
            let length = (read(0).powi(2) + read(1).powi(2) + read(2).powi(2)).sqrt();
            assert!((length - 1.0).abs() < 1e-3, "{length}");
            assert_eq!(i16::from_le_bytes([v[6], v[7]]), 777, "W is kept");
        }

        let one = 63i32;
        let mut ours = Vec::new();
        for pair in noise(400 * 2, 12).as_chunks::<2>().0 {
            let x = i32::from(pair[0] as i8) % (one / 2);
            let y = i32::from(pair[1] as i8) % (one / 2);
            ours.extend([x as u8, y as u8, one as u8, 9]);
        }
        let mut theirs = ours.clone();
        apply_filter(MeshoptFilter::Octahedral, 4, &mut ours);
        reference::oct(&mut theirs, 4);
        assert_within_one(&ours, &theirs, 1);
    }

    #[test]
    fn the_quaternion_filter_matches_the_codec() {
        let mut ours = Vec::new();
        for (i, pair) in noise(400 * 6, 21).as_chunks::<6>().0.iter().enumerate() {
            // 12-bit components, with the scale and the index in the fourth value.
            let component = |a: u8, b: u8| i32::from(i16::from_le_bytes([a, b])) % 1400;
            for value in [
                component(pair[0], pair[1]),
                component(pair[2], pair[3]),
                component(pair[4], pair[5]),
                ((1 << 11) - 1) & !3 | (i as i32 & 3),
            ] {
                ours.extend_from_slice(&(value as i16).to_le_bytes());
            }
        }
        let mut theirs = ours.clone();
        apply_filter(MeshoptFilter::Quaternion, 8, &mut ours);
        reference::quat(&mut theirs);
        assert_within_one(&ours, &theirs, 2);
    }

    #[test]
    fn the_exponential_filter_matches_the_codec_exactly() {
        let mut ours = Vec::new();
        // 133 elements of 12 bytes.
        for raw in noise(133 * 12, 31).as_chunks::<4>().0 {
            // Exponents inside the range the encoder writes.
            let exponent = (raw[3] as i8) / 4;
            ours.extend([raw[0], raw[1], raw[2], exponent as u8]);
        }
        let mut theirs = ours.clone();
        apply_filter(MeshoptFilter::Exponential, 12, &mut ours);
        reference::exp(&mut theirs, 12);
        assert_eq!(ours, theirs);
    }

    #[test]
    fn filters_never_panic_on_arbitrary_bytes() {
        for seed in 0..32 {
            let bytes = noise(256, seed);
            let (mut a, mut b, mut c, mut d) = (bytes.clone(), bytes.clone(), bytes.clone(), bytes);
            octahedral_8(&mut a);
            octahedral_16(&mut b);
            quaternion(&mut c);
            exponential(&mut d);
        }
    }
}
