// SPDX-License-Identifier: MPL-2.0
//! Cue points stored in a WAV file (ADR-4632).
//!
//! A [`WavCue`] is one row of a RIFF `cue ` chunk: the sample offset stored in the file, the
//! point's id, an optional name from a `LIST` / `adtl` / `labl` label, and the point's place in
//! that chunk. This is not a sound cue (`SoundCue`). A sound cue picks a variation when it
//! plays. These points are markers already written into the WAV container.
//!
//! Label bytes are Latin-1 (ISO-8859-1). Each byte becomes the Unicode scalar U+0000 through
//! U+00FF, and the text ends at the first NUL or at the end of the label chunk. The playback
//! path does not read these strings, so this decoding stays inside the importer. A missing
//! label is `None`. A label that is present and empty is `Some("")`.
//!
//! `sample_offset` is the chunk's `dwSampleOffset` as stored. This module does not decode PCM
//! and does not resample, and it does not reinterpret the offset for a compressed layout.
//! `order` is the zero-based index of the point in file order across every top-level `cue `
//! chunk. It is not `dwPosition`.
//!
//! The reader walks the bytes it was given. It does not trust the RIFF size field. A file with
//! no `cue ` chunk returns an empty list. A `cue ` chunk, or a `LIST` / `adtl` list, that is
//! cut short returns [`WavCueError::Truncated`] and no partial list.

use thiserror::Error;

/// One cue point from a WAV file, in `cue ` chunk order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavCue {
    /// `dwName` from the cue point. A `labl` sub-chunk with the same id supplies [`Self::name`].
    /// Two points may share an id; both rows are kept.
    pub id: u32,
    /// `dwSampleOffset` from the cue point, in samples of the file's own layout.
    ///
    /// Not resampled, and not interpreted for a compressed layout.
    pub sample_offset: u32,
    /// The first `labl` whose id matches [`Self::id`].
    ///
    /// The bytes are Latin-1, cut at the first NUL. `None` when no label matches this id.
    /// `Some("")` when a label is present and has no text before the NUL.
    pub name: Option<String>,
    /// Zero-based index of this point in file order across every top-level `cue ` chunk.
    ///
    /// This is not `dwPosition`.
    pub order: u32,
}

/// Why [`read_wav_cues`] refused a byte buffer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WavCueError {
    /// The buffer does not start with the four bytes `RIFF`.
    #[error("WAV cue reader: the file does not start with RIFF")]
    NotRiff,
    /// The RIFF form type is not `WAVE`.
    #[error("WAV cue reader: the RIFF form is not WAVE")]
    NotWave,
    /// A `cue ` chunk, or a `LIST` / `adtl` chunk, claims more bytes than the buffer holds.
    #[error("WAV cue reader: a cue or label chunk is truncated")]
    Truncated,
}

impl WavCueError {
    /// Stable code: `wav_cue_not_riff`, `wav_cue_not_wave`, or `wav_cue_truncated`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotRiff => "wav_cue_not_riff",
            Self::NotWave => "wav_cue_not_wave",
            Self::Truncated => "wav_cue_truncated",
        }
    }
}

/// A cue point before its label and order are attached.
struct RawCue {
    id: u32,
    sample_offset: u32,
}

/// Reads cue points from a WAV byte buffer.
///
/// A buffer with no `cue ` chunk returns an empty list. Only top-level `cue ` chunks and
/// top-level `LIST` / `adtl` lists are read. Other chunks (`fmt `, `data`, `note`, `ltxt`, an
/// `INFO` list) are skipped. PCM bytes are not decoded.
///
/// # Errors
///
/// [`WavCueError::NotRiff`] when the first four bytes are not `RIFF`, including a buffer
/// shorter than four bytes. [`WavCueError::NotWave`] when the form type is present and is not
/// `WAVE`. [`WavCueError::Truncated`] when a buffer that starts with `RIFF` is shorter than the
/// 12-byte header, when a `cue ` or `adtl` chunk declares more bytes than remain, when a list's
/// 4-byte type is cut off, or when `dwCuePoints` does not fit in the cue chunk (including a
/// count whose point bytes would overflow).
pub fn read_wav_cues(bytes: &[u8]) -> Result<Vec<WavCue>, WavCueError> {
    if bytes.len() < 12 {
        return Err(if bytes.starts_with(b"RIFF") {
            WavCueError::Truncated
        } else {
            WavCueError::NotRiff
        });
    }
    if &bytes[0..4] != b"RIFF" {
        return Err(WavCueError::NotRiff);
    }
    if &bytes[8..12] != b"WAVE" {
        return Err(WavCueError::NotWave);
    }

    let mut raw = Vec::new();
    // Keyed by cue id, first label wins (ADR-5597): a list with a linear dedupe and a linear
    // lookup per cue made a crafted file of ~10^6 labels cost ~10^12 comparisons.
    let mut labels = std::collections::BTreeMap::new();
    let mut offset = 12usize;
    while offset + 8 <= bytes.len() {
        let id = read_id(bytes, offset)?;
        let size = u32_as_usize(read_u32(bytes, offset + 4)?)?;
        let body_at = offset + 8;
        let available = bytes.len() - body_at;
        if id == *b"cue " {
            if size > available {
                return Err(WavCueError::Truncated);
            }
            parse_cue_points(&bytes[body_at..body_at + size], &mut raw)?;
        } else if id == *b"LIST" {
            if available < 4 || size < 4 {
                return Err(WavCueError::Truncated);
            }
            let list_type = read_id(bytes, body_at)?;
            if list_type == *b"adtl" {
                if size > available {
                    return Err(WavCueError::Truncated);
                }
                parse_adtl(&bytes[body_at + 4..body_at + size], &mut labels)?;
            } else if size > available {
                // The type is readable (INFO and the like) but the list is cut. Keep cues
                // already found and stop. A cue that sits inside the missing tail is not read.
                break;
            }
        } else if size > available {
            break;
        }

        let padded = if size.is_multiple_of(2) {
            size
        } else {
            size.checked_add(1).ok_or(WavCueError::Truncated)?
        };
        let next = body_at.checked_add(padded).ok_or(WavCueError::Truncated)?;
        if next > bytes.len() {
            // The body fit. A missing pad byte on the last chunk is allowed.
            break;
        }
        offset = next;
    }
    attach(raw, &labels)
}

fn attach(
    raw: Vec<RawCue>,
    labels: &std::collections::BTreeMap<u32, String>,
) -> Result<Vec<WavCue>, WavCueError> {
    let mut cues = Vec::with_capacity(raw.len());
    for (index, point) in raw.into_iter().enumerate() {
        let order = u32::try_from(index).map_err(|_| WavCueError::Truncated)?;
        let name = labels.get(&point.id).cloned();
        cues.push(WavCue {
            id: point.id,
            sample_offset: point.sample_offset,
            name,
            order,
        });
    }
    Ok(cues)
}

fn parse_cue_points(body: &[u8], out: &mut Vec<RawCue>) -> Result<(), WavCueError> {
    if body.len() < 4 {
        return Err(WavCueError::Truncated);
    }
    let count = u32_as_usize(read_u32(body, 0)?)?;
    // Checked before any use of the count as a length, so a huge count cannot allocate.
    let needed = count.checked_mul(24).ok_or(WavCueError::Truncated)?;
    let points = &body[4..];
    if points.len() < needed {
        return Err(WavCueError::Truncated);
    }
    let (rows, _) = points[..needed].as_chunks::<24>();
    for row in rows {
        out.push(RawCue {
            id: u32::from_le_bytes([row[0], row[1], row[2], row[3]]),
            sample_offset: u32::from_le_bytes([row[20], row[21], row[22], row[23]]),
        });
    }
    Ok(())
}

fn parse_adtl(body: &[u8], labels: &mut std::collections::BTreeMap<u32, String>) -> Result<(), WavCueError> {
    let mut offset = 0usize;
    while offset + 8 <= body.len() {
        let id = read_id(body, offset)?;
        let size = u32_as_usize(read_u32(body, offset + 4)?)?;
        let body_at = offset + 8;
        let available = body.len() - body_at;
        if size > available {
            return Err(WavCueError::Truncated);
        }
        if id == *b"labl" {
            if size < 4 {
                return Err(WavCueError::Truncated);
            }
            let name_id = read_u32(body, body_at)?;
            let text = latin1_zstr(&body[body_at + 4..body_at + size]);
            labels.entry(name_id).or_insert(text);
        }
        let padded = if size.is_multiple_of(2) {
            size
        } else {
            size.checked_add(1).ok_or(WavCueError::Truncated)?
        };
        let next = body_at.checked_add(padded).ok_or(WavCueError::Truncated)?;
        if next > body.len() {
            break;
        }
        offset = next;
    }
    Ok(())
}

/// Latin-1, cut at the first NUL. A missing NUL keeps every byte in `bytes`.
fn latin1_zstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    bytes[..end].iter().copied().map(char::from).collect()
}

fn read_id(bytes: &[u8], at: usize) -> Result<[u8; 4], WavCueError> {
    let end = at.checked_add(4).ok_or(WavCueError::Truncated)?;
    if end > bytes.len() {
        return Err(WavCueError::Truncated);
    }
    Ok([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, WavCueError> {
    let end = at.checked_add(4).ok_or(WavCueError::Truncated)?;
    if end > bytes.len() {
        return Err(WavCueError::Truncated);
    }
    Ok(u32::from_le_bytes([
        bytes[at],
        bytes[at + 1],
        bytes[at + 2],
        bytes[at + 3],
    ]))
}

fn u32_as_usize(value: u32) -> Result<usize, WavCueError> {
    usize::try_from(value).map_err(|_| WavCueError::Truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(id);
        push_u32(&mut out, u32::try_from(body.len()).expect("chunk size"));
        out.extend_from_slice(body);
        if !body.len().is_multiple_of(2) {
            out.push(0);
        }
        out
    }

    fn wave(parts: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        for part in parts {
            body.extend_from_slice(part);
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        let size = u32::try_from(4 + body.len()).expect("riff size");
        push_u32(&mut out, size);
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&body);
        out
    }

    fn cue_point(id: u32, position: u32, sample_offset: u32) -> [u8; 24] {
        let mut point = [0u8; 24];
        point[0..4].copy_from_slice(&id.to_le_bytes());
        point[4..8].copy_from_slice(&position.to_le_bytes());
        point[8..12].copy_from_slice(b"data");
        point[20..24].copy_from_slice(&sample_offset.to_le_bytes());
        point
    }

    fn cue_chunk(points: &[[u8; 24]]) -> Vec<u8> {
        let mut body = Vec::new();
        push_u32(&mut body, u32::try_from(points.len()).expect("count"));
        for point in points {
            body.extend_from_slice(point);
        }
        chunk(b"cue ", &body)
    }

    fn labl(id: u32, text: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        push_u32(&mut body, id);
        body.extend_from_slice(text);
        body.push(0);
        chunk(b"labl", &body)
    }

    fn adtl(parts: &[Vec<u8>]) -> Vec<u8> {
        let mut body = b"adtl".to_vec();
        for part in parts {
            body.extend_from_slice(part);
        }
        chunk(b"LIST", &body)
    }

    fn fmt_pcm() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&44_100u32.to_le_bytes());
        body.extend_from_slice(&(44_100u32 * 2).to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(&16u16.to_le_bytes());
        chunk(b"fmt ", &body)
    }

    fn data_chunk(samples: &[u8]) -> Vec<u8> {
        chunk(b"data", samples)
    }

    fn refused(bytes: &[u8]) -> WavCueError {
        read_wav_cues(bytes).expect_err("refused")
    }

    #[test]
    fn two_labeled_points_round_trip_offset_and_name() {
        let bytes = wave(&[
            fmt_pcm(),
            data_chunk(&[0, 1, 2, 3, 4, 5, 6, 7]),
            cue_chunk(&[cue_point(10, 7, 100), cue_point(11, 8, 200)]),
            adtl(&[labl(10, b"intro"), labl(11, b"loop")]),
        ]);
        let cues = read_wav_cues(&bytes).expect("cues");
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].id, 10);
        assert_eq!(cues[0].sample_offset, 100);
        assert_eq!(cues[0].name.as_deref(), Some("intro"));
        assert_eq!(cues[0].order, 0);
        assert_eq!(cues[1].id, 11);
        assert_eq!(cues[1].sample_offset, 200);
        assert_eq!(cues[1].name.as_deref(), Some("loop"));
        assert_eq!(cues[1].order, 1);
        assert_ne!(cues[0].sample_offset, 7, "dwPosition is not the sample offset");
    }

    #[test]
    fn wav_without_a_cue_chunk_is_empty() {
        let bytes = wave(&[fmt_pcm(), data_chunk(&[9, 9, 9, 9])]);
        assert_eq!(read_wav_cues(&bytes).expect("no cue"), Vec::<WavCue>::new());
    }

    #[test]
    fn truncated_cue_chunk_returns_no_partial_list() {
        let mut body = Vec::new();
        push_u32(&mut body, 2);
        body.extend_from_slice(&cue_point(1, 0, 10));
        let bytes = wave(&[cue_chunk(&[cue_point(1, 0, 10)]), chunk(b"cue ", &body)]);
        let err = refused(&bytes);
        assert_eq!(err, WavCueError::Truncated);
        assert_eq!(err.code(), "wav_cue_truncated");
    }

    #[test]
    fn bytes_that_are_not_riff_are_refused() {
        for bytes in [b"hello".as_slice(), b"", b"RIF", b"RIFXrest", b"RF64rest"] {
            let err = refused(bytes);
            assert_eq!(err, WavCueError::NotRiff);
            assert_eq!(err.code(), "wav_cue_not_riff");
        }
    }

    #[test]
    fn a_riff_form_that_is_not_wave_is_refused() {
        let bytes = b"RIFF\x04\x00\x00\x00AVI ";
        let err = refused(bytes);
        assert_eq!(err, WavCueError::NotWave);
        assert_eq!(err.code(), "wav_cue_not_wave");
    }

    #[test]
    fn a_short_riff_header_is_truncated() {
        assert_eq!(refused(b"RIFF"), WavCueError::Truncated);
        assert_eq!(refused(b"RIFF\x00\x00\x00\x00WAV"), WavCueError::Truncated);
    }

    #[test]
    fn duplicate_ids_keep_both_rows_and_the_first_label() {
        let bytes = wave(&[
            adtl(&[labl(3, b"first"), labl(3, b"second"), labl(4, b"")]),
            cue_chunk(&[
                cue_point(3, 0, 1),
                cue_point(3, 9, 2),
                cue_point(4, 0, 3),
                cue_point(5, 0, 4),
            ]),
        ]);
        let cues = read_wav_cues(&bytes).expect("cues");
        assert_eq!(cues.len(), 4);
        assert_eq!(cues[0].name.as_deref(), Some("first"));
        assert_eq!(cues[1].name.as_deref(), Some("first"));
        assert_eq!(cues[0].order, 0);
        assert_eq!(cues[1].order, 1);
        assert_eq!(cues[1].sample_offset, 2);
        assert_eq!(cues[2].name.as_deref(), Some(""));
        assert_eq!(cues[3].name, None);
    }

    /// corr-agent (ADR-5597): 200 000 labels and points took ~4 * 10^10 comparisons with the
    /// list dedupe and per-cue scan (minutes on the `audio.cues` caller's thread); keyed labels
    /// make it linear. First label still wins.
    #[test]
    fn many_labels_and_points_are_matched_by_key() {
        const N: u32 = 200_000;
        let points: Vec<[u8; 24]> = (0..N).map(|id| cue_point(id, 0, id)).collect();
        let mut labels: Vec<Vec<u8>> = (0..N).map(|id| labl(id, b"x")).collect();
        labels.push(labl(0, b"late"));
        let bytes = wave(&[cue_chunk(&points), adtl(&labels)]);
        let cues = read_wav_cues(&bytes).expect("cues");
        assert_eq!(cues.len(), N as usize);
        assert!(cues.iter().all(|cue| cue.name.as_deref() == Some("x")));
    }

    #[test]
    fn names_are_latin1_not_utf8() {
        let bytes = wave(&[
            cue_chunk(&[cue_point(1, 0, 1), cue_point(2, 0, 2)]),
            adtl(&[labl(1, &[0xE9]), labl(2, &[0xC3, 0xA9])]),
        ]);
        let cues = read_wav_cues(&bytes).expect("cues");
        let single = cues[0].name.as_deref().expect("latin-1 e9");
        assert_eq!(single.chars().collect::<Vec<_>>(), vec!['\u{00E9}']);
        // UTF-8 for U+00E9 is the two bytes C3 A9. Latin-1 keeps both bytes.
        let pair: Vec<char> = cues[1]
            .name
            .as_deref()
            .expect("two latin-1 bytes")
            .chars()
            .collect();
        assert_eq!(pair, ['\u{00C3}', '\u{00A9}']);
        assert_ne!(pair, ['\u{00E9}']);
    }

    #[test]
    fn a_label_ends_at_the_first_nul() {
        let bytes = wave(&[cue_chunk(&[cue_point(1, 0, 8)]), adtl(&[labl(1, b"ab\0cd")])]);
        let cues = read_wav_cues(&bytes).expect("cues");
        assert_eq!(cues[0].name.as_deref(), Some("ab"));
    }

    #[test]
    fn a_huge_cue_count_is_truncated_without_a_partial_list() {
        let mut body = Vec::new();
        push_u32(&mut body, u32::MAX);
        let bytes = wave(&[chunk(b"cue ", &body)]);
        assert_eq!(refused(&bytes), WavCueError::Truncated);
    }

    #[test]
    fn an_odd_sized_label_does_not_hide_the_next_label() {
        let bytes = wave(&[
            cue_chunk(&[cue_point(1, 0, 1), cue_point(2, 0, 2)]),
            adtl(&[labl(1, b"Ab"), labl(2, b"Z")]),
        ]);
        let cues = read_wav_cues(&bytes).expect("cues");
        assert_eq!(cues[0].name.as_deref(), Some("Ab"));
        assert_eq!(cues[1].name.as_deref(), Some("Z"));
    }

    #[test]
    fn two_cue_chunks_continue_the_order() {
        let bytes = wave(&[
            cue_chunk(&[cue_point(1, 5, 10)]),
            fmt_pcm(),
            cue_chunk(&[cue_point(2, 1, 20), cue_point(3, 1, 30)]),
        ]);
        let cues = read_wav_cues(&bytes).expect("cues");
        assert_eq!(
            cues.iter()
                .map(|cue| (cue.order, cue.sample_offset))
                .collect::<Vec<_>>(),
            vec![(0, 10), (1, 20), (2, 30)]
        );
    }

    #[test]
    fn note_and_ltxt_are_not_names() {
        let mut note = Vec::new();
        push_u32(&mut note, 1);
        note.extend_from_slice(b"from-note\0");
        let mut ltxt = vec![0u8; 20];
        ltxt[..4].copy_from_slice(&1u32.to_le_bytes());
        let bytes = wave(&[
            cue_chunk(&[cue_point(1, 0, 50)]),
            adtl(&[
                chunk(b"note", &note),
                chunk(b"ltxt", &ltxt),
                labl(1, b"from-labl"),
            ]),
        ]);
        let cues = read_wav_cues(&bytes).expect("cues");
        assert_eq!(cues[0].name.as_deref(), Some("from-labl"));
        assert_eq!(cues[0].sample_offset, 50);
    }

    #[test]
    fn a_cue_after_a_truncated_data_chunk_is_not_read() {
        let mut data = Vec::new();
        data.extend_from_slice(b"data");
        push_u32(&mut data, 1_000);
        data.extend_from_slice(&[0, 0, 0, 0]);
        let kept = wave(&[cue_chunk(&[cue_point(7, 1, 15)]), data.clone()]);
        let cues = read_wav_cues(&kept).expect("keeps the earlier cue");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].sample_offset, 15);

        let hidden = wave(&[data, cue_chunk(&[cue_point(7, 1, 15)])]);
        assert_eq!(read_wav_cues(&hidden).expect("stops"), Vec::<WavCue>::new());
    }

    #[test]
    fn a_cut_list_type_refuses_the_file_and_a_cut_info_list_does_not() {
        let cue = cue_chunk(&[cue_point(1, 0, 4)]);
        let mut cut_type = Vec::new();
        cut_type.extend_from_slice(b"LIST");
        push_u32(&mut cut_type, 100);
        cut_type.extend_from_slice(b"IN");
        assert_eq!(refused(&wave(&[cue.clone(), cut_type])), WavCueError::Truncated);

        let mut info = Vec::new();
        info.extend_from_slice(b"LIST");
        push_u32(&mut info, 100);
        info.extend_from_slice(b"INFO");
        let cues = read_wav_cues(&wave(&[cue, info])).expect("info tail is not a cue");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].sample_offset, 4);
    }

    #[test]
    fn the_riff_size_field_is_not_trusted() {
        let mut bytes = wave(&[cue_chunk(&[cue_point(1, 9, 42)])]);
        bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
        let cues = read_wav_cues(&bytes).expect("walks the buffer");
        assert_eq!(cues[0].sample_offset, 42);
        assert_eq!(cues[0].order, 0);
    }

    #[test]
    fn a_zero_count_and_bytes_past_the_points_are_empty_of_extra_rows() {
        let mut body = Vec::new();
        push_u32(&mut body, 1);
        body.extend_from_slice(&cue_point(1, 0, 6));
        body.extend_from_slice(&[0xFF; 8]);
        let one = read_wav_cues(&wave(&[chunk(b"cue ", &body)])).expect("one");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].sample_offset, 6);

        let zeros = read_wav_cues(&wave(&[chunk(b"cue ", &0u32.to_le_bytes())])).expect("zero");
        assert!(zeros.is_empty());
    }
}
