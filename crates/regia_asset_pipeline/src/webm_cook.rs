// SPDX-License-Identifier: MPL-2.0
//! The video importer (`docs/specs/50-video-playback.md` § 4, ADR-2410): `.webm` and `.mkv` files are
//! copied, validated and described, never transcoded.
//!
//! Cook: [`regia_media::validate()`] checks the whole file (every top-level element fits, every packet
//! demuxes, the codec rule holds); a failure fails the cook with the protocol code at the start of
//! the message (`container_invalid` for a truncated or damaged file, `codec_patented`,
//! `codec_unsupported`). The cooked payload (type [`COOKED_VIDEO_TYPE`]) is the clip report followed
//! by the file byte for byte at a 4 KiB offset ([`regia_media::validate::encode_cooked_video`]).
//! Other video containers (`.mp4`, `.mov`, `.avi`, …) are refused with `container_unsupported` and
//! the re-encode hint.
//!
//! Import side effect: a `.webm` without a `.video.ron` next to it gets one with the defaults
//! (`<stem>.video.ron`), so a dropped file is playable by path at once. `.video.ron` and
//! `.videolist.ron` sources cook by parsing (format version checked) and are stored as written.

use std::path::Path;

use regia_asset::AssetId;
use regia_media::validate::{ClipReport, encode_cooked_video};
use regia_media::{MediaError, VideoList, VideoSource};

use crate::cook::CookError;
use crate::db::AssetDatabase;

/// The importer name the cook records for video files.
pub const WEBM_IMPORTER: &str = "webm";
/// The importer name of `.video.ron` / `.videolist.ron`.
pub const VIDEO_RON_IMPORTER: &str = "video_ron";
/// The cooked type path of a video file.
pub const COOKED_VIDEO_TYPE: &str = "regia_media::CookedVideo";
/// The cooked type path of a `.video.ron`.
pub const VIDEO_SOURCE_TYPE: &str = "regia_media::VideoSource";
/// The cooked type path of a `.videolist.ron`.
pub const VIDEO_LIST_TYPE: &str = "regia_media::VideoList";

/// True for the media file extensions the video importer reads (lowercase, no dot).
#[must_use]
pub fn is_video_extension(ext: &str) -> bool {
    matches!(ext, "webm" | "mkv")
}

/// True for video container extensions the importer refuses (`container_unsupported`).
#[must_use]
pub fn is_refused_video_extension(ext: &str) -> bool {
    matches!(
        ext,
        "mp4" | "m4v" | "mov" | "avi" | "ts" | "m2ts" | "flv" | "wmv" | "3gp" | "mpg" | "mpeg"
    )
}

/// True for `.video.ron` and `.videolist.ron` file names.
#[must_use]
pub fn is_video_ron(source: &Path) -> bool {
    source.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        crate::cook::name_ends_with(n, ".video.ron") || crate::cook::name_ends_with(n, ".videolist.ron")
    })
}

fn video_error(path: &Path, e: &MediaError) -> CookError {
    CookError::Video(format!("{}: {}: {e} ({})", e.code(), path.display(), e.hint()))
}

/// What the video cook returns.
#[derive(Debug)]
pub struct CookedVideo {
    /// The asset id from the `.meta`.
    pub id: AssetId,
    /// Hash of the source bytes (the importer has no settings).
    pub settings_hash: [u8; 32],
    /// The cooked payload.
    pub payload: Vec<u8>,
    /// What validation found.
    pub report: ClipReport,
}

/// Validates and cooks one `.webm` / `.mkv` (see the module docs).
pub fn cook_video(source: &Path, bytes: &[u8]) -> Result<CookedVideo, CookError> {
    let _span = tracing::info_span!("cook_video", bytes = bytes.len()).entered();
    let report = regia_media::validate(std::io::Cursor::new(bytes)).map_err(|e| video_error(source, &e))?;
    let meta = AssetDatabase::load_or_create_meta(source, WEBM_IMPORTER).map_err(CookError::Db)?;
    let payload = encode_cooked_video(&report, bytes).map_err(|e| video_error(source, &e))?;
    ensure_video_ron(source)?;
    Ok(CookedVideo {
        id: meta.id,
        settings_hash: *blake3::hash(bytes).as_bytes(),
        payload,
        report,
    })
}

/// Writes `<stem>.video.ron` next to a media file that has none.
fn ensure_video_ron(source: &Path) -> Result<(), CookError> {
    let Some(name) = source.file_name().and_then(|n| n.to_str()) else {
        return Ok(());
    };
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    let ron_path = source.with_file_name(format!("{stem}.video.ron"));
    if ron_path.exists() {
        return Ok(());
    }
    let text = VideoSource::new(name.to_string())
        .to_ron()
        .map_err(|e| video_error(source, &e))?;
    crate::cook_ledger::write_atomic(&ron_path, text.as_bytes())?;
    tracing::info!(target: "regia_asset_pipeline", path = %ron_path.display(), "created a .video.ron for the clip");
    Ok(())
}

/// The refusal of a non-WebM video container.
pub fn refuse_container(source: &Path, ext: &str) -> CookError {
    video_error(source, &MediaError::ContainerUnsupported(format!(".{ext}")))
}

/// A cooked `.video.ron` / `.videolist.ron`: (asset id, settings hash, type path, payload).
pub type CookedVideoRon = (AssetId, [u8; 32], &'static str, Vec<u8>);

/// Cooks a `.video.ron` / `.videolist.ron`: parses it (refusing a bad or future version) and stores
/// it as written.
pub fn cook_video_ron(source: &Path, bytes: &[u8]) -> Result<CookedVideoRon, CookError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| video_error(source, &MediaError::invalid("not UTF-8 text")))?;
    let list = source
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| crate::cook::name_ends_with(n, ".videolist.ron"));
    let type_path = if list {
        VideoList::from_ron(text).map_err(|e| video_error(source, &e))?;
        VIDEO_LIST_TYPE
    } else {
        VideoSource::from_ron(text).map_err(|e| video_error(source, &e))?;
        VIDEO_SOURCE_TYPE
    };
    let meta = AssetDatabase::load_or_create_meta(source, VIDEO_RON_IMPORTER).map_err(CookError::Db)?;
    Ok((
        meta.id,
        *blake3::hash(bytes).as_bytes(),
        type_path,
        bytes.to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_media::mux::PatternClip;
    use regia_media::validate::decode_cooked_video;

    fn temp(name: &str, bytes: &[u8]) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = regia_test_utils::unique_temp_dir("webm_cook");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write");
        (dir, path)
    }

    /// A good clip cooks to a payload holding its report and its bytes unchanged, and gets a
    /// `.video.ron` next to it.
    #[test]
    fn cooks_a_clip_byte_for_byte() {
        let bytes = PatternClip::new(16, 8, 10, 30).to_webm();
        let (dir, path) = temp("intro.webm", &bytes);
        let cooked = cook_video(&path, &bytes).expect("cooks");
        let (report, data) = decode_cooked_video(&cooked.payload).expect("decodes");
        assert_eq!(data, bytes.as_slice());
        assert_eq!(report, cooked.report);
        assert_eq!(
            (report.width, report.height, report.tracks[0].packets),
            (16, 8, 10)
        );
        let ron = std::fs::read_to_string(dir.join("intro.video.ron")).expect("video.ron");
        let source = VideoSource::from_ron(&ron).expect("parses");
        assert_eq!(source.file, "intro.webm");
        assert!(dir.join("intro.webm.meta").is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// specs/50 § 4: a truncated file is refused with `container_invalid` at the start of the error.
    #[test]
    fn refuses_a_truncated_file_with_its_code() {
        let bytes = PatternClip::new(16, 8, 10, 30).to_webm();
        let cut = &bytes[..bytes.len() - 100];
        let (dir, path) = temp("cut.webm", cut);
        let err = cook_video(&path, cut).expect_err("truncated");
        let text = err.to_string();
        assert!(text.starts_with("container_invalid"), "{text}");
        assert!(text.contains("truncated"), "{text}");
        assert!(
            !dir.join("cut.video.ron").exists(),
            "nothing written for a refused file"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refuses_patented_codecs_and_foreign_containers() {
        let mut clip = PatternClip::new(16, 8, 4, 30);
        clip.video_codec = "V_MPEGH/ISO/HEVC".to_string();
        let bytes = clip.to_webm();
        let (dir, path) = temp("hevc.webm", &bytes);
        let err = cook_video(&path, &bytes).expect_err("patented").to_string();
        assert!(err.contains("codec_patented"), "{err}");
        assert!(
            refuse_container(&path, "mp4")
                .to_string()
                .contains("container_unsupported")
        );
        assert!(is_refused_video_extension("mov") && !is_refused_video_extension("webm"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn video_ron_cooks_by_parsing() {
        let good = br#"(format_version: 1, file: "intro.webm")"#;
        let (dir, path) = temp("intro.video.ron", good);
        let (_, _, ty, payload) = cook_video_ron(&path, good).expect("cooks");
        assert_eq!(ty, VIDEO_SOURCE_TYPE);
        assert_eq!(payload, good);
        let future = br#"(format_version: 9, file: "intro.webm")"#;
        assert!(cook_video_ron(&path, future).is_err());
        let list = br#"(format_version: 1, entries: ["intro.video.ron"])"#;
        let lpath = dir.join("all.videolist.ron");
        std::fs::write(&lpath, list).expect("write");
        assert_eq!(cook_video_ron(&lpath, list).expect("list").2, VIDEO_LIST_TYPE);
        let _ = std::fs::remove_dir_all(dir);
    }
}
