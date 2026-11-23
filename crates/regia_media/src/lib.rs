// SPDX-License-Identifier: MPL-2.0
//! Video playback (`docs/specs/50-video-playback.md`, ADR-1593, ADR-1670).
//!
//! This milestone ships the codec-independent half of the player:
//!
//! - [`container`]: the in-house WebM / Matroska demuxer and [`probe`], hardened for untrusted files.
//! - [`codec`]: the codec rule (royalty-free only), the decoder traits, and the reference raw video and
//!   float PCM codecs. With the `video` feature (or `av1`, `opus`, `vorbis` alone) the compressed
//!   decoders plug in behind the same traits: `av1` (rav1d), `opus` (libopus translated to Rust) and
//!   `vorbis` (lewton), ADR-1869. [`xiph`] holds their shared header parsers.
//! - [`isolate`]: the decoder isolation seam: caps before every call, corrupt-packet streaks, budgets.
//! - [`session`]: demux plus guarded decode on the calling thread or on a worker thread.
//! - [`player`]: the state machine, clocks (`Lockstep` integer rule), seek, loop and events.
//! - [`list`], [`source`], [`present`], [`mod@reference`], [`mux`], [`mod@bench`].
//! - [`boot`] (startup movies and `SkipVideo`), [`cues`] (text cues routed to the caption board) and
//!   [`mod@validate`] (whole-file checks for import and cook), ADR-2410.
//! - [`plugin`]: the ECS surface (`VideoPlayer`, `MediaPlugin`, `MediaFiles`).
//! - [`ring`] and [`frame_source`]: the renderer half (ADR-2044): decoding into a ring bounded by
//!   frames and bytes, publishing pictures as textures through `regia_render::VideoFrameSource`,
//!   the fallback frame, transport controls and the material `video` binding (`VideoFramePlugin`).
//!
//! Without the `video` feature no codec library is linked and AV1, Opus and Vorbis tracks are refused
//! with `codec_unsupported` and a build hint.
//! [`encode`] holds the offline WebM encoder of render-out and tools (feature `media-encode`: AV1 through
//! `rav1e`, Opus through `unsafe-libopus`, ADR-3369); without the feature it reports
//! `media_encode_unavailable`.
//! `voice_opus` (feature `voice-opus`) is the Opus codec of voice chat, behind
//! `regia_audio::voice_chat::VoiceCodecProvider`.
//!
//! # Getting started
//!
//! [`probe`] reads a file's tracks without opening a player. The [`mux`] module writes small
//! deterministic clips, so this example needs no asset on disk:
//!
//! ```
//! use std::io::Cursor;
//!
//! use regia_media::mux::PatternClip;
//! use regia_media::{TrackKind, probe};
//!
//! // 10 frames of raw 64 by 48 video at 30 fps, as WebM bytes.
//! let bytes = PatternClip::new(64, 48, 10, 30).to_webm();
//!
//! let info = probe(Cursor::new(bytes))?;
//! let video = info.video_track().expect("the clip has a video track");
//! assert_eq!(video.kind, TrackKind::Video);
//! let params = video.video.as_ref().expect("video parameters");
//! assert_eq!((params.width, params.height), (64, 48));
//! # Ok::<(), regia_media::MediaError>(())
//! ```
//!
//! Playing a clip needs a window or an audio device, so a game adds `MediaPlugin` and spawns a
//! `VideoPlayer` instead; see `book/agents/video-playback.md`.

#[cfg(feature = "av1")]
pub mod av1;
pub mod bench;
pub mod boot;
pub mod codec;
pub mod container;
pub mod cues;
pub mod ebml;
pub mod encode;
pub mod error;
pub mod frame_source;
pub mod fuzzing;
pub mod isolate;
pub mod list;
pub mod mux;
#[cfg(feature = "opus")]
#[allow(unsafe_code)] // FFI-shaped decoder API (docs/04 § 5, ADR-1593, ADR-1869)
pub mod opus;
pub mod overlay;
pub mod player;
pub mod plugin;
pub mod present;
pub mod reference;
pub mod ring;
pub mod session;
pub mod source;
pub mod validate;
#[cfg(feature = "voice-opus")]
#[allow(unsafe_code)] // FFI-shaped codec API (docs/04 § 5, ADR-1593, ADR-1911)
pub mod voice_opus;
#[cfg(feature = "vorbis")]
pub mod vorbis;
pub mod xiph;

pub use boot::{BootMovies, SkipVideo, boot_movies_done};
pub use container::{MediaInfo, TrackInfo, TrackKind, probe};
pub use cues::{CueStore, TextCue};
pub use error::MediaError;
pub use frame_source::{
    MediaFrameSource, VideoFramePlugin, VideoFrameSettings, VideoMaterial, publish_frames_once,
};
pub use player::{ClockMode, LoopMode, PlayState, PlayerCore, SeekMode, VideoEvent, VideoStats};
pub use plugin::VideoVisible;
pub use plugin::{MediaFiles, MediaPlugin, VideoPlayer, VideoPlayerState, VideoRegistry};
pub use present::{Fit, VideoFullscreen};
pub use ring::{RingBudget, RingFeed, RingStats};
pub use source::{VideoList, VideoSource};
pub use validate::{ClipReport, validate};
