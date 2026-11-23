// SPDX-License-Identifier: MPL-2.0
//! A YUV4MPEG2 reader for `regia media encode`: 8-bit 4:2:0 only (`C420`, `C420jpeg`, `C420paldv`,
//! `C420mpeg2` or no `C` tag), progressive. Original code from the public format description (the
//! stream header line `YUV4MPEG2 W.. H.. F..:.. ...`, then `FRAME` lines each followed by one raw
//! picture; `docs/provenance.md`). The file is untrusted: every number is bounded before anything is
//! allocated.

use std::io::{BufRead, Read};

use super::EncodeError;

/// Longest header or frame line accepted.
const MAX_LINE: usize = 4096;

/// The stream header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Y4mHeader {
    /// Picture width.
    pub width: u32,
    /// Picture height.
    pub height: u32,
    /// Frame rate numerator.
    pub fps_num: u32,
    /// Frame rate denominator.
    pub fps_den: u32,
}

/// Reads pictures from a YUV4MPEG2 stream.
pub struct Y4mReader<R> {
    input: R,
    header: Y4mHeader,
    frame_bytes: usize,
}

fn bad(message: impl Into<String>) -> EncodeError {
    EncodeError::new("encode_bad_input", format!("y4m: {}", message.into()))
}

fn read_line<R: BufRead>(input: &mut R) -> Result<Option<String>, EncodeError> {
    let mut line = Vec::new();
    let mut limited = input.take(MAX_LINE as u64 + 1);
    let n = limited
        .read_until(b'\n', &mut line)
        .map_err(|e| bad(format!("read failed: {e}")))?;
    if n == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(bad("line too long or cut short"));
    }
    line.pop();
    // Some capture tools write CRLF: a trailing `\r` would break the `F30000:1001` parse (a
    // silent 25 fps fallback) and the `FRAME` comparison. Accept both line endings.
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| bad("header is not ASCII"))
}

impl<R: BufRead> Y4mReader<R> {
    /// Parses the stream header.
    ///
    /// # Errors
    /// `encode_bad_input` for a missing or malformed header, an interlaced stream or a layout other
    /// than 8-bit 4:2:0; picture checks are the caller's ([`super::check_picture`]).
    pub fn new(mut input: R) -> Result<Self, EncodeError> {
        let line = read_line(&mut input)?.ok_or_else(|| bad("empty file"))?;
        let mut words = line.split(' ');
        if words.next() != Some("YUV4MPEG2") {
            return Err(bad("not a YUV4MPEG2 stream"));
        }
        let (mut width, mut height, mut fps) = (None, None, None);
        for word in words.filter(|w| !w.is_empty()) {
            let (tag, value) = word.split_at(1);
            match tag {
                "W" => width = value.parse::<u32>().ok(),
                "H" => height = value.parse::<u32>().ok(),
                "F" => {
                    fps = value
                        .split_once(':')
                        .and_then(|(n, d)| Some((n.parse::<u32>().ok()?, d.parse::<u32>().ok()?)));
                }
                "I" if value != "p" && value != "?" => {
                    return Err(bad("interlaced streams are not supported"));
                }
                "C" if !matches!(value, "420" | "420jpeg" | "420paldv" | "420mpeg2") => {
                    return Err(bad(format!(
                        "color space `{value}`: only 8-bit 4:2:0 is supported"
                    )));
                }
                _ => {}
            }
        }
        let (Some(width), Some(height)) = (width, height) else {
            return Err(bad("header has no W and H"));
        };
        let (fps_num, fps_den) = fps.unwrap_or((25, 1));
        if width == 0 || height == 0 || width > super::MAX_SIDE || height > super::MAX_SIDE {
            return Err(EncodeError::new(
                "encode_bad_size",
                format!("y4m: {width}x{height} is outside 1..={}", super::MAX_SIDE),
            ));
        }
        let (w, h) = (width as usize, height as usize);
        let frame_bytes = w * h + 2 * w.div_ceil(2) * h.div_ceil(2);
        Ok(Self {
            input,
            header: Y4mHeader {
                width,
                height,
                fps_num,
                fps_den,
            },
            frame_bytes,
        })
    }

    /// The stream header.
    pub fn header(&self) -> Y4mHeader {
        self.header
    }

    /// The next picture (tightly packed Y, U, V planes), `None` at the end.
    ///
    /// # Errors
    /// `encode_bad_input` for a malformed frame line or a cut-short picture.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, EncodeError> {
        let Some(line) = read_line(&mut self.input)? else {
            return Ok(None);
        };
        if line != "FRAME" && !line.starts_with("FRAME ") {
            return Err(bad("expected a FRAME line"));
        }
        let mut frame = vec![0u8; self.frame_bytes];
        self.input
            .read_exact(&mut frame)
            .map_err(|_| bad("the last picture is cut short"))?;
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(header: &str, frames: usize, bytes: usize) -> Vec<u8> {
        let mut out = format!("{header}\n").into_bytes();
        for k in 0..frames {
            out.extend_from_slice(b"FRAME\n");
            out.extend(std::iter::repeat_n(k as u8, bytes));
        }
        out
    }

    #[test]
    fn reads_header_and_frames() {
        let data = stream("YUV4MPEG2 W4 H2 F30000:1001 Ip A1:1 C420jpeg", 3, 12);
        let mut r = Y4mReader::new(&data[..]).expect("header");
        assert_eq!(
            r.header(),
            Y4mHeader {
                width: 4,
                height: 2,
                fps_num: 30_000,
                fps_den: 1001
            }
        );
        for k in 0..3u8 {
            assert_eq!(r.next_frame().expect("frame"), Some(vec![k; 12]));
        }
        assert_eq!(r.next_frame().expect("end"), None);
    }

    #[test]
    fn crlf_stream_parses_with_the_declared_fps() {
        // Media audit G 3: a Windows-captured stream with CRLF line endings keeps the declared
        // frame rate and every frame decodes (the `\r` used to fail the `F` parse into a silent
        // 25 fps fallback, then abort at the first `FRAME\r`).
        let data = stream("YUV4MPEG2 W4 H2 F30000:1001 Ip A1:1 C420jpeg", 3, 12);
        let crlf = String::from_utf8(data)
            .expect("ascii")
            .replace('\n', "\r\n")
            .into_bytes();
        let mut r = Y4mReader::new(&crlf[..]).expect("header");
        assert_eq!(
            r.header(),
            Y4mHeader {
                width: 4,
                height: 2,
                fps_num: 30_000,
                fps_den: 1001
            }
        );
        for k in 0..3u8 {
            assert_eq!(r.next_frame().expect("frame"), Some(vec![k; 12]));
        }
        assert_eq!(r.next_frame().expect("end"), None);
    }

    #[test]
    fn refuses_hostile_streams() {
        let code = |data: &[u8]| Y4mReader::new(data).err().map(|e| e.code);
        assert_eq!(code(b"RIFF"), Some("encode_bad_input"));
        assert_eq!(code(b"YUV4MPEG2 W4 H2 C444\n"), Some("encode_bad_input"));
        assert_eq!(code(b"YUV4MPEG2 W4 H2 It\n"), Some("encode_bad_input"));
        assert_eq!(code(b"YUV4MPEG2 W4\n"), Some("encode_bad_input"));
        assert_eq!(code(b"YUV4MPEG2 W99999999 H2\n"), Some("encode_bad_size"));
        assert_eq!(code(b"YUV4MPEG2 W0 H2\n"), Some("encode_bad_size"));
        let long = format!("YUV4MPEG2 {}\n", "X".repeat(MAX_LINE * 2));
        assert_eq!(code(long.as_bytes()), Some("encode_bad_input"));
        // A cut-short last picture is an error, not a silent end.
        let mut data = stream("YUV4MPEG2 W4 H2", 1, 12);
        data.truncate(data.len() - 1);
        let mut r = Y4mReader::new(&data[..]).expect("header");
        assert_eq!(r.next_frame().err().map(|e| e.code), Some("encode_bad_input"));
    }
}
