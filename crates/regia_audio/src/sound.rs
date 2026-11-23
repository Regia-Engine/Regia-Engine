// SPDX-License-Identifier: MPL-2.0
//! Sound assets and playback parameters (`docs/specs/16-audio.md` § 2).
//!
//! A [`SoundAsset`] is either [`Static`](SoundSource::Static) (decoded PCM in memory, for short
//! SFX) or [`Streaming`](SoundSource::Streaming) (decoded on the fly, for music/ambience/long
//! VO). Streaming sources are stored as factories because a kira streaming decoder is single-use:
//! the factory re-opens the underlying bytes for every instance.

use std::sync::Arc;
use std::time::Duration;

use kira::{
    Decibels, Panning, PlaySoundError, PlaybackRate,
    sound::{
        FromFileError,
        static_sound::{StaticSoundData, StaticSoundSettings},
        streaming::StreamingSoundData,
    },
};

use crate::clip::SoundClip;
use crate::decode::{SoundDecodeLimits, decode_static_bytes, decode_static_file};
use crate::error::AudioError;

/// Creates a fresh streaming decoder each call.
pub type StreamingFactory =
    Arc<dyn Fn() -> Result<StreamingSoundData<FromFileError>, AudioError> + Send + Sync>;

/// Decodes a whole streaming source into memory within the limits
/// (`SoundAsset::decode_to_memory_with_limits`).
type StaticFactory = Arc<dyn Fn(&SoundDecodeLimits) -> Result<StaticSoundData, AudioError> + Send + Sync>;

/// The underlying source of a sound.
#[derive(Clone)]
pub enum SoundSource {
    /// Decoded PCM held in memory (boxed: `StaticSoundData` dwarfs the factory pointer).
    Static(Box<StaticSoundData>),
    /// A factory that opens a streaming decoder (music, ambience, long VO).
    Streaming(StreamingFactory),
}

/// A playable sound: static or streaming, plus its duration.
#[derive(Clone)]
pub struct SoundAsset {
    source: SoundSource,
    duration_secs: f64,
    sample_rate: u32,
    label: Arc<str>,
    /// For streaming sources: decodes the same bytes fully into memory.
    decode: Option<StaticFactory>,
    /// Loudness normalization gain added to every play's volume, dB (ADR-1855; 0 = none).
    loudness_gain_db: f32,
    /// Bus a play routes to when its `PlaybackParams.bus` is `None` (spec 16 § 2 asset default);
    /// `None` routes to `Master` as before.
    default_bus: Option<String>,
    /// Volume added to every play's volume, dB (spec 16 § 2 asset default; 0 = no offset,
    /// composed like the loudness gain).
    default_volume_db: f32,
    /// Pitch ratio multiplied into every play's pitch (spec 16 § 2 asset default; 1 = no shift).
    default_pitch: f32,
    /// Loop region a play uses when its `PlaybackParams.loop_region` is `None` (spec 16 § 2).
    default_loop_region: Option<LoopRegion>,
}

/// Underrun counters of the streaming sounds ([`crate::Audio::stream_stats`]). A stream is
/// *starved* when it had been playing and stopped advancing for 0.1 s of rendered audio: its
/// decoder thread fell behind (slow storage, a loaded machine) and the mixer holds it silent until
/// the decoder catches up, so the sound arrives late from then on.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StreamStats {
    /// Streaming voices and music stems playing now.
    pub streaming_voices: usize,
    /// Of those, the ones starved right now.
    pub starved_voices: usize,
    /// Underruns since the audio system started (one per starved stretch of one stream).
    pub underruns: u64,
    /// Rendered time streams spent starved, seconds, summed over streams.
    pub starved_secs: f64,
}

/// A loop region in seconds: `start` to `end` (`None` = end of the sound).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LoopRegion {
    /// Where the loop starts, in seconds.
    pub start_secs: f64,
    /// Where the loop ends (exclusive), in seconds; `None` loops to the end of the sound.
    pub end_secs: Option<f64>,
}

/// How a sound starts: bus, volume, pitch, pan, looping, fades.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackParams {
    /// Bus to play on; `None` plays on `Master`.
    pub bus: Option<String>,
    /// Volume in dB (pre-bus).
    pub volume_db: f32,
    /// Playback rate ratio (1 = original speed and pitch).
    pub pitch: f32,
    /// Stereo panning −1..1; `None` leaves the sound centred.
    pub panning: Option<f32>,
    /// Loop region; `None` plays once through.
    pub loop_region: Option<LoopRegion>,
    /// Fade in from silence over this duration.
    pub fade_in: Option<Duration>,
    /// Start playback at this offset, in seconds.
    pub start_offset_secs: f64,
}

impl Default for PlaybackParams {
    fn default() -> Self {
        Self {
            bus: None,
            volume_db: 0.0,
            pitch: 1.0,
            panning: None,
            loop_region: None,
            fade_in: None,
            start_offset_secs: 0.0,
        }
    }
}

impl PlaybackParams {
    /// Parameters with only the bus set (everything else default).
    pub fn on_bus(bus: impl Into<String>) -> Self {
        Self {
            bus: Some(bus.into()),
            ..Self::default()
        }
    }
}

/// A sound ready to hand to kira: static or streaming data with settings applied.
pub(crate) enum PlayData {
    /// In-memory sound.
    Static(StaticSoundData),
    /// Streaming sound.
    Streaming(StreamingSoundData<FromFileError>),
}

/// Power of the easings that make a crossfade equal-power. Kira fades a sound's volume linearly
/// in decibels between silence (−60 dB) and full, so two plain ramps crossing each other sit at
/// about −30 dB each half way and the mix dips by about 8 dB. Bending the dB ramp by `1 − (1 − t)^p`
/// (fade in) and `t^p` (fade out) with this power keeps the amplitudes close to `sin`/`cos` of
/// `t · π/2`: the summed power of the two stays within 2 % of one throughout the fade.
pub(crate) const EQUAL_POWER_FADE_POW: f64 = 4.32;

/// The fade-in tween of an equal-power crossfade (see [`EQUAL_POWER_FADE_POW`]).
pub(crate) fn equal_power_fade_in(fade: Duration) -> kira::Tween {
    kira::Tween {
        duration: fade,
        easing: kira::Easing::OutPowf(EQUAL_POWER_FADE_POW),
        ..kira::Tween::default()
    }
}

/// The fade-out tween of an equal-power crossfade (see [`EQUAL_POWER_FADE_POW`]).
pub(crate) fn equal_power_fade_out(fade: Duration) -> kira::Tween {
    kira::Tween {
        duration: fade,
        easing: kira::Easing::InPowf(EQUAL_POWER_FADE_POW),
        ..kira::Tween::default()
    }
}

impl PlayData {
    /// The same sound with its fade-in replaced by `tween`.
    pub(crate) fn with_fade_in(self, tween: kira::Tween) -> Self {
        match self {
            Self::Static(data) => Self::Static(data.fade_in_tween(tween)),
            Self::Streaming(data) => Self::Streaming(data.fade_in_tween(tween)),
        }
    }
}

impl SoundAsset {
    /// Wraps an in-memory [`SoundClip`] (mono PCM) as a static sound.
    pub fn from_clip(clip: &SoundClip) -> Self {
        let data = clip_to_static_data(clip, &StaticSoundSettings::new());
        let duration = clip.len() as f64 / f64::from(clip.sample_rate());
        Self {
            source: SoundSource::Static(Box::new(data)),
            duration_secs: duration,
            sample_rate: clip.sample_rate(),
            label: "clip".into(),
            decode: None,
            loudness_gain_db: 0.0,
            default_bus: None,
            default_volume_db: 0.0,
            default_pitch: 1.0,
            default_loop_region: None,
        }
    }

    /// Wraps fully-decoded kira data as a static sound.
    pub fn from_static_data(data: StaticSoundData) -> Self {
        let duration = data.duration().as_secs_f64();
        let sample_rate = data.sample_rate;
        Self {
            source: SoundSource::Static(Box::new(data)),
            duration_secs: duration,
            sample_rate,
            label: "static".into(),
            decode: None,
            loudness_gain_db: 0.0,
            default_bus: None,
            default_volume_db: 0.0,
            default_pitch: 1.0,
            default_loop_region: None,
        }
    }

    /// Loads and fully decodes a file (short SFX) within the default [`SoundDecodeLimits`].
    ///
    /// # Errors
    /// [`AudioError::LoadFailed`] when it does not decode, [`AudioError::DecodeLimit`] past the
    /// limits.
    pub fn from_file_static(path: impl AsRef<std::path::Path>) -> Result<Self, AudioError> {
        Self::from_file_static_with_limits(path, &SoundDecodeLimits::default())
    }

    /// [`SoundAsset::from_file_static`] with explicit decode limits.
    ///
    /// # Errors
    /// As [`SoundAsset::from_file_static`].
    pub fn from_file_static_with_limits(
        path: impl AsRef<std::path::Path>,
        limits: &SoundDecodeLimits,
    ) -> Result<Self, AudioError> {
        let path = path.as_ref();
        let data = decode_static_file(path, limits)?;
        Ok(Self::static_labeled(data, path.display().to_string().into()))
    }

    /// Fully decodes a sound file's bytes (WAV, Ogg Vorbis, FLAC, MP3) within `limits`; `label`
    /// names it in errors.
    ///
    /// # Errors
    /// [`AudioError::LoadFailed`] when the bytes do not decode, [`AudioError::DecodeLimit`] past
    /// the limits.
    pub fn from_bytes_static_with_limits(
        bytes: Vec<u8>,
        label: &str,
        limits: &SoundDecodeLimits,
    ) -> Result<Self, AudioError> {
        let data = decode_static_bytes(bytes, label, limits)?;
        Ok(Self::static_labeled(data, label.into()))
    }

    fn static_labeled(data: StaticSoundData, label: Arc<str>) -> Self {
        let duration = data.duration().as_secs_f64();
        let sample_rate = data.sample_rate;
        Self {
            source: SoundSource::Static(Box::new(data)),
            duration_secs: duration,
            sample_rate,
            label,
            decode: None,
            loudness_gain_db: 0.0,
            default_bus: None,
            default_volume_db: 0.0,
            default_pitch: 1.0,
            default_loop_region: None,
        }
    }

    /// Opens a file for streaming (music, ambience, long VO). Each play re-opens the file.
    pub fn from_file_streaming(path: impl Into<String>) -> Result<Self, AudioError> {
        let path = path.into();
        let probe = StreamingSoundData::from_file(&path).map_err(|e| AudioError::load(&path, e))?;
        let duration = probe.duration().as_secs_f64();
        let label: Arc<str> = path.clone().into();
        let factory_path = path.clone();
        let decode_path = path.clone();
        Ok(Self {
            source: SoundSource::Streaming(Arc::new(move || {
                StreamingSoundData::from_file(&factory_path).map_err(|e| AudioError::load(&factory_path, e))
            })),
            duration_secs: duration,
            sample_rate: 48_000,
            label,
            decode: Some(Arc::new(move |limits| {
                decode_static_file(std::path::Path::new(&decode_path), limits)
            })),
            loudness_gain_db: 0.0,
            default_bus: None,
            default_volume_db: 0.0,
            default_pitch: 1.0,
            default_loop_region: None,
        })
    }

    /// A streaming sound over in-memory bytes (tests and procedurally-generated music: the
    /// decoder walks a clone of the cursor each play).
    pub fn streaming_from_bytes(bytes: Arc<Vec<u8>>, label: &str) -> Result<Self, AudioError> {
        let probe = StreamingSoundData::from_cursor(std::io::Cursor::new((*bytes).clone()))
            .map_err(|e| AudioError::load(label, e))?;
        let duration = probe.duration().as_secs_f64();
        let label: Arc<str> = label.into();
        let factory_label = Arc::clone(&label);
        let decode_label = Arc::clone(&label);
        let decode_bytes = Arc::clone(&bytes);
        Ok(Self {
            source: SoundSource::Streaming(Arc::new(move || {
                StreamingSoundData::from_cursor(std::io::Cursor::new((*bytes).clone()))
                    .map_err(|e| AudioError::load(factory_label.as_ref(), e))
            })),
            duration_secs: duration,
            sample_rate: 48_000,
            label,
            decode: Some(Arc::new(move |limits| {
                decode_static_bytes((*decode_bytes).clone(), decode_label.as_ref(), limits)
            })),
            loudness_gain_db: 0.0,
            default_bus: None,
            default_volume_db: 0.0,
            default_pitch: 1.0,
            default_loop_region: None,
        })
    }

    /// A streaming sound from a caller's decoder: `factory` builds a fresh kira
    /// `StreamingSoundData` for every play (a decoder is single-use), typically
    /// `StreamingSoundData::from_decoder` over a procedural or custom source. `duration_secs` is
    /// the stream's length (NaN when unknown; 0 means empty), used to check loop regions and by
    /// the underrun watch. Such a sound has no in-memory form:
    /// [`SoundAsset::decode_to_memory`] returns an error for it.
    pub fn from_streaming_factory(
        factory: impl Fn() -> Result<StreamingSoundData<FromFileError>, AudioError> + Send + Sync + 'static,
        duration_secs: f64,
        label: &str,
    ) -> Self {
        Self {
            source: SoundSource::Streaming(Arc::new(factory)),
            duration_secs,
            sample_rate: 48_000,
            label: label.into(),
            decode: None,
            loudness_gain_db: 0.0,
            default_bus: None,
            default_volume_db: 0.0,
            default_pitch: 1.0,
            default_loop_region: None,
        }
    }

    /// The sound's duration in seconds (best effort for streams: from the container metadata).
    pub fn duration_secs(&self) -> f64 {
        self.duration_secs
    }

    /// The sound's sample rate in Hz (48 kHz is assumed for streams, whose decoders re-rate).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// A short human-readable label (path or `"clip"`), used in error messages.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// True when the sound is decoded on the fly while it plays (a streaming source).
    pub fn is_streaming(&self) -> bool {
        matches!(self.source, SoundSource::Streaming(_))
    }

    /// The same sound fully decoded into memory (a static source with the same label); a sound
    /// already in memory is returned as is.
    ///
    /// Use this for offline renders that must be reproducible. A streaming sound is decoded by a
    /// background thread into a short ring buffer, and kira holds the sound silent (without
    /// advancing it) whenever that buffer runs dry; the offline renderer consumes as fast as it
    /// can, so how often that happens depends on the machine's load, not on the engine. A decoded
    /// copy renders the same samples on every run. It costs the whole PCM in memory
    /// (`duration × sample rate × 8` bytes, about 23 MB per minute at 48 kHz), so keep streaming
    /// for playback on a device. The default [`SoundDecodeLimits`] apply
    /// ([`AudioError::DecodeLimit`] past them).
    pub fn decode_to_memory(&self) -> Result<SoundAsset, AudioError> {
        self.decode_to_memory_with_limits(&SoundDecodeLimits::default())
    }

    /// [`SoundAsset::decode_to_memory`] with explicit decode limits (a sound already in memory is
    /// returned as is).
    ///
    /// # Errors
    /// [`AudioError::LoadFailed`] when the stream has no in-memory decoder or does not decode,
    /// [`AudioError::DecodeLimit`] past `limits`.
    pub fn decode_to_memory_with_limits(&self, limits: &SoundDecodeLimits) -> Result<SoundAsset, AudioError> {
        if !self.is_streaming() {
            return Ok(self.clone());
        }
        let decode = self
            .decode
            .as_ref()
            .ok_or_else(|| AudioError::load(self.label.as_ref(), "no in-memory decoder for this stream"))?;
        let data = decode(limits)?;
        Ok(Self {
            duration_secs: data.duration().as_secs_f64(),
            sample_rate: data.sample_rate,
            source: SoundSource::Static(Box::new(data)),
            label: Arc::clone(&self.label),
            decode: None,
            loudness_gain_db: self.loudness_gain_db,
            default_bus: self.default_bus.clone(),
            default_volume_db: self.default_volume_db,
            default_pitch: self.default_pitch,
            default_loop_region: self.default_loop_region,
        })
    }

    /// The same sound with a loudness normalization gain of `gain_db` added to every play's
    /// volume (`docs/specs/95-loudness-importers.md` § 2.5). A non-finite gain is refused (logged)
    /// and leaves 0 dB; the gain is clamped to ±[`crate::MAX_NORMALIZE_GAIN_DB`].
    #[must_use]
    pub fn with_loudness_gain_db(mut self, gain_db: f32) -> Self {
        self.loudness_gain_db = if gain_db.is_finite() {
            gain_db.clamp(-crate::MAX_NORMALIZE_GAIN_DB, crate::MAX_NORMALIZE_GAIN_DB)
        } else {
            note_rejected("loudness gain");
            tracing::warn!(target: "regia_audio", "`{}`: non-finite loudness gain {gain_db}; using 0 dB", self.label);
            0.0
        };
        self
    }

    /// The loudness normalization gain added to every play's volume, dB (0 = none).
    pub fn loudness_gain_db(&self) -> f32 {
        self.loudness_gain_db
    }

    /// The bus a play routes to when its [`PlaybackParams::bus`] is `None` (spec 16 § 2 asset
    /// default). `None` (the default) routes a bus-less play to `Master` as before.
    pub fn default_bus(&self) -> Option<&str> {
        self.default_bus.as_deref()
    }

    /// The volume added to every play's volume, dB (spec 16 § 2 asset default; 0 = no offset).
    /// Composed additively with the play's own volume, like [`Self::loudness_gain_db`]: a play at
    /// −3 dB on an asset with a −6 dB default sounds at −9 dB.
    pub fn default_volume_db(&self) -> f32 {
        self.default_volume_db
    }

    /// The pitch ratio multiplied into every play's pitch (spec 16 § 2 asset default; 1 = no
    /// shift). A play at pitch 2 on an asset with a 0.5 default plays at pitch 1.
    pub fn default_pitch(&self) -> f32 {
        self.default_pitch
    }

    /// The loop region a play uses when its [`PlaybackParams::loop_region`] is `None`
    /// (spec 16 § 2 asset default; `None` = the play decides, so a one-shot stays a one-shot).
    pub fn default_loop_region(&self) -> Option<LoopRegion> {
        self.default_loop_region
    }

    /// Sets the bus a bus-less play routes to (spec 16 § 2 asset default); see
    /// [`Self::default_bus`].
    #[must_use]
    pub fn with_default_bus(mut self, bus: impl Into<String>) -> Self {
        self.default_bus = Some(bus.into());
        self
    }

    /// Sets the volume added to every play (spec 16 § 2 asset default); see
    /// [`Self::default_volume_db`]. A non-finite value is refused (counted) and leaves 0 dB.
    #[must_use]
    pub fn with_default_volume_db(mut self, db: f32) -> Self {
        self.default_volume_db = if db.is_finite() {
            db
        } else {
            note_rejected("default volume");
            0.0
        };
        self
    }

    /// Sets the pitch ratio multiplied into every play (spec 16 § 2 asset default); see
    /// [`Self::default_pitch`]. A non-finite value is refused (counted) and leaves 1.0.
    #[must_use]
    pub fn with_default_pitch(mut self, ratio: f32) -> Self {
        self.default_pitch = if ratio.is_finite() {
            ratio
        } else {
            note_rejected("default pitch");
            1.0
        };
        self
    }

    /// Sets the loop region a play without one uses (spec 16 § 2 asset default); see
    /// [`Self::default_loop_region`].
    #[must_use]
    pub fn with_default_loop_region(mut self, region: LoopRegion) -> Self {
        self.default_loop_region = Some(region);
        self
    }

    /// The volume every play of this sound is offset by, dB: the asset default (spec 16 § 2) plus
    /// the loudness gain (ADR-1855), exactly what `play_data` adds to a play's start volume.
    /// A voice's per-frame volume (`VoiceEntry::effective_volume_db`) adds the same sum, so the
    /// offset survives every spatial update (ADR-5926).
    pub(crate) fn play_volume_offset_db(&self) -> f32 {
        self.default_volume_db + self.loudness_gain_db
    }

    /// The decoded frames and their sample rate, for an in-memory sound.
    pub(crate) fn static_frames(&self) -> Option<(&[kira::Frame], u32)> {
        match &self.source {
            SoundSource::Static(data) => Some((&data.frames, data.sample_rate)),
            SoundSource::Streaming(_) => None,
        }
    }

    /// Builds the kira data for one instance of this sound with `params` applied.
    pub(crate) fn play_data(&self, params: &PlaybackParams) -> Result<PlayData, AudioError> {
        // Spec 16 § 2 asset defaults: a play that does not set a field uses the asset's default.
        // Volume adds and pitch multiplies (composed like the loudness gain); a play without a
        // loop region falls back to the asset's. The bus default is routing, applied by the
        // caller where the bus is chosen (`Audio`), not here.
        let mut composed = params.clone();
        if composed.loop_region.is_none() {
            composed.loop_region = self.default_loop_region;
        }
        if self.default_volume_db != 0.0 && composed.volume_db.is_finite() {
            composed.volume_db += self.default_volume_db;
        }
        if self.default_pitch != 1.0 {
            composed.pitch *= self.default_pitch;
        }
        let params = &composed;
        let params: std::borrow::Cow<'_, PlaybackParams> = match params.loop_region {
            Some(region) => {
                // In-memory sounds know their real rate; streams carry a 48 kHz placeholder.
                let rate = match &self.source {
                    SoundSource::Static(data) => Some(data.sample_rate),
                    SoundSource::Streaming(_) => None,
                };
                let safe = checked_loop_region(region, self.duration_secs, rate);
                if safe != Some(region) {
                    note_rejected("loop region");
                    tracing::warn!(
                        target: "regia_audio",
                        "`{}`: loop region {region:?} does not fit the sound ({:.3} s); using {safe:?}",
                        self.label,
                        self.duration_secs
                    );
                }
                std::borrow::Cow::Owned(PlaybackParams {
                    loop_region: safe,
                    ..params.clone()
                })
            }
            None => std::borrow::Cow::Borrowed(params),
        };
        // The loudness gain shifts the requested volume; a silenced play (−∞) stays silent.
        let params = if self.loudness_gain_db != 0.0 && params.volume_db.is_finite() {
            let gained = params.volume_db + self.loudness_gain_db;
            std::borrow::Cow::Owned(PlaybackParams {
                volume_db: gained,
                ..params.into_owned()
            })
        } else {
            params
        };
        match &self.source {
            SoundSource::Static(data) => {
                let settings = static_settings(&params);
                Ok(PlayData::Static(StaticSoundData {
                    sample_rate: data.sample_rate,
                    frames: data.frames.clone(),
                    slice: data.slice,
                    settings,
                }))
            }
            SoundSource::Streaming(factory) => {
                let mut data = factory()?;
                data.settings = streaming_settings(&params);
                Ok(PlayData::Streaming(data))
            }
        }
    }
}

/// A duration from seconds supplied by a host, a tool or an authored file, capped at
/// `max_secs`. `Duration::from_secs_f32` panics on a negative, NaN or huge value (and shipping
/// builds abort on a panic): NaN and negative values become zero.
pub(crate) fn duration_from_secs(secs: f32, max_secs: f32) -> Duration {
    if secs.is_nan() {
        return Duration::ZERO;
    }
    Duration::try_from_secs_f32(secs.min(max_secs)).unwrap_or(Duration::ZERO)
}

/// A tween length from seconds ([`duration_from_secs`], capped at an hour).
pub(crate) fn tween_from_secs(secs: f32) -> Duration {
    duration_from_secs(secs, 3_600.0)
}

/// Shortest explicit loop handed to kira when the sound's real sample rate is unknown (a
/// stream), seconds: two frames at 8 kHz, so the rounded start and end frames differ at every
/// rate of 8 kHz or more. kira's transport wraps a loop by subtracting its length until the
/// position is inside it: a zero-length loop never gets there (the mixer thread spins forever)
/// and an inverted one underflows. The whole sound always loops safely unless it is empty.
pub(crate) const MIN_LOOP_SECS: f64 = 2.0 / 8_000.0;

/// A loop region kira can play for a sound of `duration_secs` (0 = empty; NaN or negative =
/// unknown) at `sample_rate` (`None` = unknown). Non-finite or negative bounds are dropped, the
/// end is clamped to the sound, and a region that would round to zero frames — at the known
/// rate the way kira rounds (`(secs × rate).round()`), otherwise anything shorter than
/// [`MIN_LOOP_SECS`] — loops the whole sound instead. An empty sound does not loop at all (its
/// whole-sound loop has zero length too).
pub(crate) fn checked_loop_region(
    region: LoopRegion,
    duration_secs: f64,
    sample_rate: Option<u32>,
) -> Option<LoopRegion> {
    if duration_secs == 0.0 {
        return None;
    }
    let known = duration_secs.is_finite() && duration_secs > 0.0;
    let whole = LoopRegion {
        start_secs: 0.0,
        end_secs: None,
    };
    let start = if region.start_secs.is_finite() {
        region.start_secs.max(0.0)
    } else {
        0.0
    };
    let end = region
        .end_secs
        .filter(|end| end.is_finite())
        // An end at the sound's length is kept as written (kira reads it as the end of the audio).
        .filter(|end| !known || *end <= duration_secs);
    let effective_end = end.or(known.then_some(duration_secs));
    let too_short = |end: f64| match sample_rate.filter(|rate| *rate > 0) {
        Some(rate) => {
            let rate = f64::from(rate);
            (end * rate).round() <= (start * rate).round()
        }
        None => end - start < MIN_LOOP_SECS,
    };
    match effective_end {
        Some(end) if too_short(end) => Some(whole),
        _ => Some(LoopRegion {
            start_secs: start,
            end_secs: end,
        }),
    }
}

/// Mono clip → kira static data (each sample duplicated to both channels).
pub(crate) fn clip_to_static_data(clip: &SoundClip, settings: &StaticSoundSettings) -> StaticSoundData {
    StaticSoundData {
        sample_rate: clip.sample_rate(),
        frames: clip
            .samples()
            .iter()
            .copied()
            .map(kira::Frame::from_mono)
            .collect(),
        slice: None,
        settings: *settings,
    }
}

/// Values the mixer refused since the process started (see [`rejected_value_count`]).
static REJECTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many values the mixer refused since the process started: NaN or infinite volumes,
/// playback rates, pans and filter cutoffs (replaced by safe ones before they reach kira), and
/// loop regions kira cannot wrap (replaced by the whole sound). A count that grows while a
/// scene runs means something upstream (a transform, an emitter field, a tuning value, a tool)
/// produces bad numbers; the first one is also logged as a warning. Process-wide, monotonic.
pub fn rejected_value_count() -> u64 {
    REJECTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Counts one refused value (warning on the first).
pub(crate) fn note_rejected(what: &str) {
    if REJECTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
        tracing::warn!(
            target: "regia_audio",
            "audio refused a bad {what} (NaN, infinite or unplayable); later ones are only counted \
             (rejected_value_count, audio.list_emitters `rejected_values`)"
        );
    }
}

/// The volume a NaN becomes, dB (the floor [`crate::gain_to_db`] reports for silence).
const SILENT_DB: f32 = -100.0;

/// Loudest voice volume handed to kira, dB. Far above any authored level; it only stops an
/// infinite value from reaching the mixer.
const MAX_VOICE_DB: f32 = 48.0;

/// Fastest playback rate handed to kira, either direction (six octaves). Bounds the work of
/// kira's streaming resampler, which steps once per source frame it skips.
const MAX_PLAYBACK_RATE: f64 = 64.0;

/// A voice volume kira can use: NaN is silence, ±∞ is clamped. A NaN that reached kira would
/// turn every sample of the voice, and the bus it sums into, into NaN.
pub(crate) fn safe_volume_db(db: f32) -> f32 {
    if !db.is_finite() {
        note_rejected("volume");
    }
    if db.is_nan() {
        SILENT_DB
    } else {
        db.clamp(SILENT_DB, MAX_VOICE_DB)
    }
}

/// A playback rate kira can use: NaN and ±∞ play at the original speed, finite rates are
/// bounded to ±[`MAX_PLAYBACK_RATE`].
pub(crate) fn safe_playback_rate(ratio: f32) -> f64 {
    let ratio = f64::from(ratio);
    if !ratio.is_finite() {
        note_rejected("playback rate");
    }
    if ratio.is_finite() {
        ratio.clamp(-MAX_PLAYBACK_RATE, MAX_PLAYBACK_RATE)
    } else {
        1.0
    }
}

/// A pan kira can use: NaN is centred, everything else is clamped to −1..1.
pub(crate) fn safe_pan(pan: f32) -> f32 {
    if pan.is_nan() {
        note_rejected("pan");
        0.0
    } else {
        pan.clamp(-1.0, 1.0)
    }
}

pub(crate) fn static_settings(params: &PlaybackParams) -> StaticSoundSettings {
    let mut settings = StaticSoundSettings::new()
        .volume(Decibels(safe_volume_db(params.volume_db)))
        .playback_rate(PlaybackRate(safe_playback_rate(params.pitch)));
    if let Some(pan) = params.panning {
        settings = settings.panning(Panning(safe_pan(pan)));
    }
    if let Some(region) = &params.loop_region {
        let end = region.end_secs;
        let start = region.start_secs;
        let region: kira::sound::Region = match end {
            Some(end) => (start..end).into(),
            None => (start..).into(),
        };
        settings = settings.loop_region(region);
    }
    if params.start_offset_secs > 0.0 {
        settings = settings.start_position(params.start_offset_secs);
    }
    if let Some(fade) = params.fade_in {
        settings = settings.fade_in_tween(kira::Tween {
            duration: fade,
            easing: kira::Easing::Linear,
            ..kira::Tween::default()
        });
    }
    settings
}

pub(crate) fn streaming_settings(params: &PlaybackParams) -> kira::sound::streaming::StreamingSoundSettings {
    use kira::sound::streaming::StreamingSoundSettings;
    let mut settings = StreamingSoundSettings::new()
        .volume(Decibels(safe_volume_db(params.volume_db)))
        .playback_rate(PlaybackRate(safe_playback_rate(params.pitch)));
    if let Some(pan) = params.panning {
        settings = settings.panning(Panning(safe_pan(pan)));
    }
    if let Some(region) = &params.loop_region {
        let end = region.end_secs;
        let start = region.start_secs;
        let region: kira::sound::Region = match end {
            Some(end) => (start..end).into(),
            None => (start..).into(),
        };
        settings = settings.loop_region(region);
    }
    if params.start_offset_secs > 0.0 {
        settings = settings.start_position(params.start_offset_secs);
    }
    if let Some(fade) = params.fade_in {
        settings = settings.fade_in_tween(kira::Tween {
            duration: fade,
            easing: kira::Easing::Linear,
            ..kira::Tween::default()
        });
    }
    settings
}

/// Maps kira's play errors to [`AudioError::PlayFailed`].
pub(crate) fn play_error<E: std::fmt::Debug>(error: PlaySoundError<E>) -> AudioError {
    AudioError::PlayFailed(error.to_string())
}

impl std::fmt::Debug for SoundAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoundAsset")
            .field(
                "kind",
                &match &self.source {
                    SoundSource::Static(_) => "static",
                    SoundSource::Streaming(_) => "streaming",
                },
            )
            .field("duration_secs", &self.duration_secs)
            .field("sample_rate", &self.sample_rate)
            .field("label", &self.label)
            .finish()
    }
}

impl PartialEq for SoundAsset {
    fn eq(&self, other: &Self) -> bool {
        // Assets compare by identity of their payload: static data by frame slice, streaming
        // factories by pointer.
        match (&self.source, &other.source) {
            (SoundSource::Static(a), SoundSource::Static(b)) => {
                a.sample_rate == b.sample_rate && a.frames == b.frames
            }
            (SoundSource::Streaming(a), SoundSource::Streaming(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(samples: usize) -> SoundClip {
        SoundClip::new(vec![0.1; samples], 48_000).expect("valid clip")
    }

    fn region(start_secs: f64, end_secs: Option<f64>) -> LoopRegion {
        LoopRegion { start_secs, end_secs }
    }

    /// The loop check for an in-memory sound at 48 kHz.
    fn check(region: LoopRegion, duration_secs: f64) -> Option<LoopRegion> {
        checked_loop_region(region, duration_secs, Some(48_000))
    }

    #[test]
    fn loop_regions_are_checked_in_whole_frames_at_the_real_rate() {
        let whole = Some(region(0.0, None));
        // One frame wide at 4 kHz, starting on a half frame: both ends round to frame 1.
        let half = 0.5 / 4_000.0;
        let one = 1.0 / 4_000.0 - 1e-12;
        assert_eq!(
            checked_loop_region(region(half, Some(half + one)), 1.0, Some(4_000)),
            whole
        );
        // Two frames wide at 4 kHz is a real loop.
        assert_eq!(
            checked_loop_region(region(0.0, Some(2.0 / 4_000.0)), 1.0, Some(4_000)),
            Some(region(0.0, Some(2.0 / 4_000.0)))
        );
        // A stream (rate unknown): anything under two frames at 8 kHz loops whole.
        assert_eq!(
            checked_loop_region(region(0.0, Some(1.5 / 8_000.0)), 1.0, None),
            whole
        );
        assert_eq!(
            checked_loop_region(region(0.0, Some(0.01)), 1.0, None),
            Some(region(0.0, Some(0.01)))
        );
    }

    #[test]
    fn loop_regions_kira_cannot_wrap_are_replaced() {
        let whole = Some(region(0.0, None));
        // Valid regions pass through untouched.
        assert_eq!(
            check(region(0.25, Some(0.75)), 1.0),
            Some(region(0.25, Some(0.75)))
        );
        assert_eq!(check(region(0.5, None), 1.0), Some(region(0.5, None)));
        assert_eq!(
            check(region(0.25, Some(1.0)), 1.0),
            Some(region(0.25, Some(1.0))),
            "an end at the length is valid and left alone (no refusal logged)"
        );
        // Zero-length, inverted and past-the-end regions loop the whole sound.
        assert_eq!(check(region(0.5, Some(0.5)), 1.0), whole);
        assert_eq!(check(region(0.8, Some(0.2)), 1.0), whole);
        assert_eq!(check(region(1.0, None), 1.0), whole);
        assert_eq!(check(region(3.0, None), 1.0), whole);
        // An end past the sound is the end of the sound.
        assert_eq!(check(region(0.5, Some(9.0)), 1.0), Some(region(0.5, None)));
        // Non-finite bounds are dropped.
        assert_eq!(
            check(region(f64::NAN, Some(f64::INFINITY)), 1.0),
            Some(region(0.0, None))
        );
        assert_eq!(check(region(-2.0, Some(0.5)), 1.0), Some(region(0.0, Some(0.5))));
        // A very short sound still loops whole (8 samples at 48 kHz); an empty one plays once.
        assert_eq!(check(region(0.0, None), 8.0 / 48_000.0), whole);
        assert_eq!(check(region(0.0, Some(1e-5)), 8.0 / 48_000.0), whole);
        assert_eq!(check(region(0.0, None), 0.0), None);
        // Unknown length: only the explicit bounds are checked.
        assert_eq!(check(region(2.0, None), f64::NAN), Some(region(2.0, None)));
        assert_eq!(check(region(2.0, Some(2.0)), -1.0), whole);
    }

    #[test]
    fn tween_lengths_from_tools_never_panic() {
        assert_eq!(tween_from_secs(0.25), Duration::from_millis(250));
        assert_eq!(tween_from_secs(-1.0), Duration::ZERO);
        assert_eq!(tween_from_secs(f32::NAN), Duration::ZERO);
        assert_eq!(tween_from_secs(f32::INFINITY), Duration::from_secs(3_600));
        assert_eq!(tween_from_secs(1e30), Duration::from_secs(3_600));
        assert_eq!(duration_from_secs(f32::NAN, 10.0), Duration::ZERO);
        assert_eq!(duration_from_secs(25.0, 10.0), Duration::from_secs(10));
    }

    #[test]
    fn playback_values_are_made_safe_for_kira() {
        let before = rejected_value_count();
        assert_eq!(safe_volume_db(f32::NAN), SILENT_DB);
        assert!(rejected_value_count() > before, "a NaN is counted");
        assert_eq!(safe_volume_db(f32::INFINITY), MAX_VOICE_DB);
        assert_eq!(safe_volume_db(f32::NEG_INFINITY), SILENT_DB);
        assert_eq!(safe_volume_db(-6.0), -6.0);
        assert_eq!(safe_playback_rate(f32::NAN), 1.0);
        assert_eq!(safe_playback_rate(f32::INFINITY), 1.0);
        assert_eq!(safe_playback_rate(1e9), MAX_PLAYBACK_RATE);
        assert_eq!(safe_playback_rate(-0.5), -0.5);
        assert_eq!(safe_pan(f32::NAN), 0.0);
        assert_eq!(safe_pan(3.0), 1.0);
        let params = PlaybackParams {
            volume_db: f32::NAN,
            pitch: f32::NAN,
            panning: Some(f32::NAN),
            loop_region: Some(region(0.05, Some(0.05))),
            ..PlaybackParams::default()
        };
        let PlayData::Static(data) = SoundAsset::from_clip(&clip(4_800))
            .play_data(&params)
            .expect("static data")
        else {
            panic!("expected static data");
        };
        assert_eq!(data.settings.volume, kira::Value::Fixed(Decibels(SILENT_DB)));
        assert_eq!(data.settings.playback_rate, kira::Value::Fixed(PlaybackRate(1.0)));
        assert_eq!(data.settings.panning, kira::Value::Fixed(Panning(0.0)));
        assert_eq!(
            data.settings.loop_region,
            Some(kira::sound::Region {
                start: kira::sound::PlaybackPosition::Seconds(0.0),
                end: kira::sound::EndPosition::EndOfAudio,
            })
        );
    }

    #[test]
    fn clip_asset_reports_duration_and_plays_with_settings() {
        let asset = SoundAsset::from_clip(&clip(4_800));
        assert!((asset.duration_secs() - 0.1).abs() < 1e-9);
        let params = PlaybackParams {
            volume_db: -6.0,
            pitch: 2.0,
            panning: Some(-0.5),
            ..PlaybackParams::default()
        };
        let data = asset.play_data(&params).expect("static data");
        let PlayData::Static(data) = data else {
            panic!("expected static data");
        };
        assert_eq!(data.settings.volume, kira::Value::Fixed(Decibels(-6.0)));
        assert_eq!(data.frames.len(), 4_800);
    }

    #[test]
    fn spec16_section2_sound_asset_default_volume_pitch_and_loop_fill_an_unspecified_play() {
        // Spec 16 § 2: the sound asset stores loop region / default volume / pitch / bus;
        // a play that does not set them uses the asset's defaults. One second of audio so the
        // 0.1..0.2 s default loop region fits.
        let clip = clip(48_000);
        let default_region = region(0.1, Some(0.2));
        let asset = SoundAsset::from_clip(&clip)
            .with_default_volume_db(-6.0)
            .with_default_pitch(0.5)
            .with_default_loop_region(default_region);
        assert_eq!(asset.default_volume_db(), -6.0);
        assert_eq!(asset.default_pitch(), 0.5);
        assert_eq!(asset.default_loop_region(), Some(default_region));

        // An all-default play picks up the asset's loop / volume / pitch.
        let PlayData::Static(data) = asset.play_data(&PlaybackParams::default()).expect("data") else {
            panic!("expected static data");
        };
        assert_eq!(data.settings.volume, kira::Value::Fixed(Decibels(-6.0)));
        assert_eq!(data.settings.playback_rate, kira::Value::Fixed(PlaybackRate(0.5)));
        assert_eq!(
            data.settings.loop_region,
            Some(kira::sound::Region {
                start: kira::sound::PlaybackPosition::Seconds(0.1),
                end: kira::sound::EndPosition::Custom(kira::sound::PlaybackPosition::Seconds(0.2)),
            })
        );

        // Explicit play values compose with the asset defaults: volume adds, pitch multiplies,
        // an explicit loop region wins over the asset's.
        let params = PlaybackParams {
            volume_db: 3.0,
            pitch: 2.0,
            loop_region: Some(region(0.0, None)),
            ..PlaybackParams::default()
        };
        let PlayData::Static(data) = asset.play_data(&params).expect("data") else {
            panic!("expected static data");
        };
        assert_eq!(data.settings.volume, kira::Value::Fixed(Decibels(-3.0)));
        assert_eq!(data.settings.playback_rate, kira::Value::Fixed(PlaybackRate(1.0)));
        assert_eq!(
            data.settings.loop_region,
            Some(kira::sound::Region {
                start: kira::sound::PlaybackPosition::Seconds(0.0),
                end: kira::sound::EndPosition::EndOfAudio,
            })
        );
    }

    #[test]
    fn spec16_section2_sound_asset_default_volume_pitch_ignore_non_finite() {
        let before = rejected_value_count();
        let asset = SoundAsset::from_clip(&clip(4)).with_default_volume_db(f32::NAN);
        assert_eq!(asset.default_volume_db(), 0.0, "a NaN default is refused");
        let asset = asset.with_default_pitch(f32::INFINITY);
        assert_eq!(asset.default_pitch(), 1.0, "an infinite default is refused");
        assert!(rejected_value_count() > before);
    }

    #[test]
    fn the_crossfade_easings_hold_the_summed_power() {
        // Kira ramps the fade in dB from −60 to 0 through the easing; replay that for both halves.
        let p = EQUAL_POWER_FADE_POW;
        let amp = |db: f64| 10f64.powf(db / 20.0);
        for i in 1..100 {
            let t = f64::from(i) / 100.0;
            let fading_in = amp(-60.0 + 60.0 * (1.0 - (1.0 - t).powf(p)));
            let fading_out = amp(-60.0 * t.powf(p));
            let power = fading_in * fading_in + fading_out * fading_out;
            assert!((0.97..1.02).contains(&power), "t = {t}: summed power {power}");
        }
        assert!(
            matches!(equal_power_fade_in(Duration::from_secs(1)).easing, kira::Easing::OutPowf(q) if (q - p).abs() < 1e-12)
        );
        assert!(
            matches!(equal_power_fade_out(Duration::from_secs(1)).easing, kira::Easing::InPowf(q) if (q - p).abs() < 1e-12)
        );
    }
}
