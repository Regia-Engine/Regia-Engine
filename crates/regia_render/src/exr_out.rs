// SPDX-License-Identifier: MPL-2.0
//! Minimal OpenEXR 2 scanline writer for the resource inspector: uncompressed, one part,
//! `HALF` or `FLOAT` channels written from raw sample bits (so a half-float texture
//! round-trips bit-exact).
//!
//! Written from the public OpenEXR file-layout document ("OpenEXR File Layout", Academy
//! Software Foundation): magic + version, a header of typed attributes ending in a null byte,
//! a line-offset table, then one chunk per scanline (y, byte count, channel planes in
//! alphabetical channel order). The reference tracer has its own copy for FLOAT-only layered
//! output (`regia_pt::exr`); this one lives here because `regia_pt` is not a runtime
//! dependency.

/// Samples of one channel plane, row-major, top row first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExrSamples {
    /// IEEE half-float bit patterns (`HALF`).
    Half(Vec<u16>),
    /// IEEE single-float bit patterns (`FLOAT`).
    Float(Vec<u32>),
}

impl ExrSamples {
    fn len(&self) -> usize {
        match self {
            Self::Half(v) => v.len(),
            Self::Float(v) => v.len(),
        }
    }

    fn bytes_per_sample(&self) -> usize {
        match self {
            Self::Half(_) => 2,
            Self::Float(_) => 4,
        }
    }

    fn pixel_type(&self) -> u32 {
        match self {
            Self::Half(_) => PIXEL_HALF,
            Self::Float(_) => PIXEL_FLOAT,
        }
    }
}

/// One named channel plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExrPlane {
    /// Channel name (`R`, `G`, `B`, `A`, `Y`, ...).
    pub name: String,
    /// `width × height` samples.
    pub samples: ExrSamples,
}

/// Why an image could not be encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExrError(pub String);

impl std::fmt::Display for ExrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exr: {}", self.0)
    }
}

impl std::error::Error for ExrError {}

const MAGIC: u32 = 20_000_630;
const PIXEL_HALF: u32 = 1;
const PIXEL_FLOAT: u32 = 2;

fn attr(out: &mut Vec<u8>, name: &str, kind: &str, value: &[u8]) {
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(kind.as_bytes());
    out.push(0);
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value);
}

/// Encodes an uncompressed scanline EXR. Planes are sorted by name (the format requires it);
/// `attributes` become `string` header attributes.
///
/// # Errors
///
/// The image is empty or a plane's sample count is not `width × height`.
pub fn encode(
    width: u32,
    height: u32,
    planes: &[ExrPlane],
    attributes: &[(String, String)],
) -> Result<Vec<u8>, ExrError> {
    if width == 0 || height == 0 {
        return Err(ExrError("zero-sized image".into()));
    }
    let n = (width as usize) * (height as usize);
    let mut sorted: Vec<&ExrPlane> = planes.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    if sorted.is_empty() {
        return Err(ExrError("no channels".into()));
    }
    for p in &sorted {
        if p.samples.len() != n {
            return Err(ExrError(format!(
                "channel {} has {} samples, expected {n}",
                p.name,
                p.samples.len()
            )));
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    let mut chlist = Vec::new();
    for p in &sorted {
        chlist.extend_from_slice(p.name.as_bytes());
        chlist.push(0);
        chlist.extend_from_slice(&p.samples.pixel_type().to_le_bytes());
        chlist.extend_from_slice(&[0, 0, 0, 0]); // pLinear + reserved
        chlist.extend_from_slice(&1i32.to_le_bytes());
        chlist.extend_from_slice(&1i32.to_le_bytes());
    }
    chlist.push(0);
    attr(&mut out, "channels", "chlist", &chlist);
    attr(&mut out, "compression", "compression", &[0]);
    let mut window = Vec::new();
    for v in [0i32, 0, width as i32 - 1, height as i32 - 1] {
        window.extend_from_slice(&v.to_le_bytes());
    }
    attr(&mut out, "dataWindow", "box2i", &window);
    attr(&mut out, "displayWindow", "box2i", &window);
    attr(&mut out, "lineOrder", "lineOrder", &[0]);
    attr(&mut out, "pixelAspectRatio", "float", &1.0f32.to_le_bytes());
    attr(&mut out, "screenWindowCenter", "v2f", &[0; 8]);
    attr(&mut out, "screenWindowWidth", "float", &1.0f32.to_le_bytes());
    for (k, v) in attributes {
        attr(&mut out, k, "string", v.as_bytes());
    }
    out.push(0);
    let w = width as usize;
    let line_bytes: usize = sorted.iter().map(|p| p.samples.bytes_per_sample() * w).sum();
    let first_chunk = out.len() + 8 * height as usize;
    for y in 0..height as usize {
        let offset = (first_chunk + y * (8 + line_bytes)) as u64;
        out.extend_from_slice(&offset.to_le_bytes());
    }
    for y in 0..height as usize {
        out.extend_from_slice(&(y as i32).to_le_bytes());
        out.extend_from_slice(&(line_bytes as u32).to_le_bytes());
        for p in &sorted {
            match &p.samples {
                ExrSamples::Half(v) => {
                    for s in &v[y * w..(y + 1) * w] {
                        out.extend_from_slice(&s.to_le_bytes());
                    }
                }
                ExrSamples::Float(v) => {
                    for s in &v[y * w..(y + 1) * w] {
                        out.extend_from_slice(&s.to_le_bytes());
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample bytes of scanline `y` (all planes, name order) of a file `encode` produced.
    fn scanline(file: &[u8], height: usize, line_bytes: usize, y: usize) -> &[u8] {
        let start = file.len() - (height - y) * (8 + line_bytes);
        let chunk = &file[start..start + 8 + line_bytes];
        assert_eq!(
            i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            y as i32
        );
        &chunk[8..]
    }

    #[test]
    fn half_planes_round_trip_bit_exact_in_name_order() {
        // 3 x 2, planes given out of order; sorted output is A, B, R.
        let r: Vec<u16> = vec![0x3c00, 0x0001, 0x7bff, 0x8000, 0xfc00, 0x7e01];
        let b: Vec<u16> = vec![1, 2, 3, 4, 5, 6];
        let a: Vec<u16> = vec![0x3c00; 6];
        let planes = [
            ExrPlane {
                name: "R".into(),
                samples: ExrSamples::Half(r.clone()),
            },
            ExrPlane {
                name: "A".into(),
                samples: ExrSamples::Half(a.clone()),
            },
            ExrPlane {
                name: "B".into(),
                samples: ExrSamples::Half(b.clone()),
            },
        ];
        let file = encode(3, 2, &planes, &[("regia.label".into(), "x".into())]).expect("encode");
        assert_eq!(u32::from_le_bytes([file[0], file[1], file[2], file[3]]), MAGIC);
        let line = 3 * 2 * 3;
        for y in 0..2 {
            let bytes = scanline(&file, 2, line, y);
            let (pairs, rest) = bytes.as_chunks::<2>();
            assert!(rest.is_empty());
            let got: Vec<u16> = pairs.iter().map(|p| u16::from_le_bytes(*p)).collect();
            let want: Vec<u16> = [&a, &b, &r]
                .iter()
                .flat_map(|p| p[y * 3..y * 3 + 3].to_vec())
                .collect();
            assert_eq!(got, want, "row {y}");
        }
    }

    #[test]
    fn float_plane_and_bad_sizes() {
        let plane = ExrPlane {
            name: "Y".into(),
            samples: ExrSamples::Float(vec![1.5f32.to_bits(), (-0.25f32).to_bits()]),
        };
        let file = encode(2, 1, std::slice::from_ref(&plane), &[]).expect("encode");
        let bytes = scanline(&file, 1, 8, 0);
        assert_eq!(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]), 1.5);
        assert_eq!(
            f32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            -0.25
        );
        assert!(encode(3, 1, &[plane], &[]).is_err(), "wrong sample count");
        assert!(encode(0, 1, &[], &[]).is_err(), "zero width");
    }
}
