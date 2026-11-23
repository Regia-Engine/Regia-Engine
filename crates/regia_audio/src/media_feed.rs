// SPDX-License-Identifier: MPL-2.0
//! The media audio seam (specs/50 § 3.3, ADR-1670): a single-producer single-consumer ring of `f32`
//! samples from a video player's decode thread to the mixer, and the [`AudioClock`] that counts frames
//! the output consumed. The ring stores sample bits in atomics, so neither side locks or allocates after
//! creation; a short read fills silence and counts an underrun. The mixer's stream reaches its consumer
//! through the stream's shared control with one uncontended `try_lock` per audio block, so a stream
//! rebuilt after an output suspend reads the same consumer whatever became of the old stream. `regia_media` creates one feed per
//! player with an audio track and registers its consumer in [`MediaAudioRegistry`]; the `AudioPlugin`
//! system `attach_media_feeds` starts a [`MediaStreamData`] kira sound on the named bus for each one
//! (`Audio::play_media_feed`). The sound resamples linearly to the mix rate, applies the feed volume,
//! and counts the frames the output consumed: that count is the player's audio clock (ADR-1670).

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use kira::Frame;
use kira::info::Info;
use kira::sound::{Sound, SoundData};
use regia_ecs::prelude::*;

#[derive(Debug)]
struct Ring {
    samples: Box<[AtomicU32]>,
    /// Total samples written.
    head: AtomicUsize,
    /// Total samples read.
    tail: AtomicUsize,
    channels: u8,
    underruns: AtomicU64,
    /// Bumped by the producer's `clear`; the consumer then skips to `flush_to`.
    flush_epoch: AtomicU64,
    /// `head` at the last producer `clear`.
    flush_to: AtomicUsize,
}

/// Constructor for a feed's two ends.
#[derive(Debug, Clone, Copy)]
pub struct PcmFeed;

impl PcmFeed {
    /// A ring holding `capacity_frames` frames of `channels` interleaved samples.
    ///
    /// Returns `None` for a channel count outside 1..=8: the stream downmixes at most an 8-wide
    /// frame, and a wider ring would be read at the wrong stride.
    pub fn channel(channels: u8, capacity_frames: usize) -> Option<(PcmProducer, PcmConsumer)> {
        if !(1..=8).contains(&channels) {
            return None;
        }
        let cap = capacity_frames.max(1) * usize::from(channels);
        let ring = Arc::new(Ring {
            samples: (0..cap).map(|_| AtomicU32::new(0)).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            channels,
            underruns: AtomicU64::new(0),
            flush_epoch: AtomicU64::new(0),
            flush_to: AtomicUsize::new(0),
        });
        Some((
            PcmProducer { ring: ring.clone() },
            PcmConsumer { ring, seen_epoch: 0 },
        ))
    }
}

/// The writing end (decode thread).
#[derive(Debug)]
pub struct PcmProducer {
    ring: Arc<Ring>,
}

impl PcmProducer {
    /// Appends samples; returns how many fit (the rest are dropped: the ring is full).
    pub fn push(&mut self, samples: &[f32]) -> usize {
        let cap = self.ring.samples.len();
        let head = self.ring.head.load(Ordering::Relaxed);
        let tail = self.ring.tail.load(Ordering::Acquire);
        let free = cap - head.wrapping_sub(tail).min(cap);
        let n = samples.len().min(free);
        for (i, s) in samples[..n].iter().enumerate() {
            self.ring.samples[(head.wrapping_add(i)) % cap].store(s.to_bits(), Ordering::Relaxed);
        }
        self.ring.head.store(head.wrapping_add(n), Ordering::Release);
        n
    }

    /// Drops everything queued so far (a seek): the reader skips to the current write position on its
    /// next read, so samples pushed after this call are kept.
    pub fn clear(&mut self) {
        let head = self.ring.head.load(Ordering::Relaxed);
        self.ring.flush_to.store(head, Ordering::Relaxed);
        self.ring.flush_epoch.fetch_add(1, Ordering::Release);
    }

    /// Samples waiting to be read.
    pub fn queued(&self) -> usize {
        self.ring
            .head
            .load(Ordering::Acquire)
            .wrapping_sub(self.ring.tail.load(Ordering::Acquire))
    }

    /// Interleaved channel count.
    pub fn channels(&self) -> u8 {
        self.ring.channels
    }
}

/// The reading end (audio thread).
#[derive(Debug)]
pub struct PcmConsumer {
    ring: Arc<Ring>,
    seen_epoch: u64,
}

impl PcmConsumer {
    /// Fills `out` from the ring; a short read fills the rest with silence and counts one underrun.
    /// Returns the samples that came from the ring.
    pub fn pop(&mut self, out: &mut [f32]) -> usize {
        let cap = self.ring.samples.len();
        let mut tail = self.ring.tail.load(Ordering::Relaxed);
        let epoch = self.ring.flush_epoch.load(Ordering::Acquire);
        if epoch != self.seen_epoch {
            self.seen_epoch = epoch;
            let to = self.ring.flush_to.load(Ordering::Relaxed);
            // Skip forward only: a flush point already read past is not rewound.
            let ahead = to.wrapping_sub(tail);
            if ahead != 0 && ahead <= cap {
                tail = to;
            }
        }
        let head = self.ring.head.load(Ordering::Acquire);
        let avail = head.wrapping_sub(tail).min(cap);
        let n = out.len().min(avail);
        for (i, o) in out[..n].iter_mut().enumerate() {
            *o = f32::from_bits(self.ring.samples[(tail.wrapping_add(i)) % cap].load(Ordering::Relaxed));
        }
        self.ring.tail.store(tail.wrapping_add(n), Ordering::Release);
        if n < out.len() {
            out[n..].fill(0.0);
            self.ring.underruns.fetch_add(1, Ordering::Relaxed);
        }
        n
    }

    /// Discards up to `frames` whole frames without reading them; frames past the queued data
    /// count one underrun each, exactly as a popped frame does. Returns the frames actually
    /// skipped. The media stream's catch-up drain uses this to skip whole frames in one step.
    pub fn skip_frames(&mut self, frames: u64) -> u64 {
        if frames == 0 {
            return 0;
        }
        let cap = self.ring.samples.len();
        let channels = usize::from(self.ring.channels).max(1);
        let mut tail = self.ring.tail.load(Ordering::Relaxed);
        let epoch = self.ring.flush_epoch.load(Ordering::Acquire);
        if epoch != self.seen_epoch {
            self.seen_epoch = epoch;
            let to = self.ring.flush_to.load(Ordering::Relaxed);
            // Skip forward only: a flush point already read past is not rewound.
            let ahead = to.wrapping_sub(tail);
            if ahead != 0 && ahead <= cap {
                tail = to;
            }
        }
        let head = self.ring.head.load(Ordering::Acquire);
        let avail_frames = head.wrapping_sub(tail).min(cap) / channels;
        let skipped = frames.min(avail_frames as u64);
        self.ring
            .tail
            .store(tail.wrapping_add(skipped as usize * channels), Ordering::Release);
        let missed = frames - skipped;
        if missed > 0 {
            // Saturate rather than wrap: an extreme `frames` argument (the stream caps its own
            // catch-up skips, but the method is public) must not corrupt the stat.
            let _ = self
                .ring
                .underruns
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    Some(n.saturating_add(missed))
                });
        }
        skipped
    }

    /// Ring capacity in whole frames (the most a single skip can find queued). The ring holds
    /// `capacity_frames * channels` samples of 4 bytes each, allocated when the feed was created.
    pub fn capacity_frames(&self) -> u64 {
        (self.ring.samples.len() / usize::from(self.ring.channels).max(1)) as u64
    }

    /// Drops everything queued (seek, close).
    pub fn clear(&mut self) {
        self.seen_epoch = self.ring.flush_epoch.load(Ordering::Acquire);
        let head = self.ring.head.load(Ordering::Acquire);
        self.ring.tail.store(head, Ordering::Release);
    }

    /// Underruns so far.
    pub fn underruns(&self) -> u64 {
        self.ring.underruns.load(Ordering::Relaxed)
    }

    /// Interleaved channel count.
    pub fn channels(&self) -> u8 {
        self.ring.channels
    }
}

/// Frames the audio output consumed, shared between the backend (writer) and players (readers).
#[derive(Debug, Clone, Resource)]
pub struct AudioClock {
    frames: Arc<AtomicU64>,
    sample_rate: u32,
}

impl AudioClock {
    /// A clock at `sample_rate`, starting at zero.
    pub fn new(sample_rate: u32) -> Self {
        Self {
            frames: Arc::new(AtomicU64::new(0)),
            sample_rate: sample_rate.max(1),
        }
    }

    /// Frames consumed so far.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Acquire)
    }

    /// Mix rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Called by the backend after it consumed `n` frames (device callback or offline render).
    pub fn advance(&self, n: u64) {
        self.frames.fetch_add(n, Ordering::AcqRel);
    }
}

/// One registered media feed, waiting to be attached to the mixer.
#[derive(Debug)]
pub struct MediaFeedEntry {
    /// Owner id (the player entity's bits).
    pub owner: u64,
    /// Target bus name (matched without case; falls back to master when missing).
    pub bus: String,
    /// The reading end.
    pub consumer: PcmConsumer,
    /// Linear volume.
    pub volume: f32,
    /// The feed's sample rate.
    pub sample_rate: u32,
}

/// Shared state of a playing media stream.
#[derive(Debug, Default)]
struct StreamControl {
    volume_bits: AtomicU32,
    stopped: AtomicBool,
    frames: AtomicU64,
    underruns: AtomicU64,
    /// The rate the output really mixes the stream at (learned from the mixer's `dt`); 0 until
    /// the first mix.
    mixed_rate: AtomicU32,
    /// The player is not playing (paused, seeking, buffering): the stream plays silence and takes
    /// nothing from the feed, so the audio decoded before a pause is heard after it.
    paused: AtomicBool,
    /// The player's rate as `f32` bits; 0 bits mean 1.0 (a stream nobody set a rate on).
    rate_bits: AtomicU32,
    /// The feed's interleaved channel count (1..=8).
    channels: usize,
    /// The feed's reading end. It lives here, not in the kira sound, so a stream rebuilt after an
    /// output suspend reads on even while the old sound still sits in a torn-down track that kira
    /// has not dropped yet (an offline mixer frees removed tracks only at its next track insert;
    /// concurrency audit 11-03 H1). The audio thread only `try_lock`s it, once per block.
    reader: Mutex<Option<PcmConsumer>>,
}

/// Playback rates at which a media feed is heard (resampled); outside them it is muted
/// (specs/50 § 2.5).
pub const MEDIA_FEED_RATES: std::ops::RangeInclusive<f32> = 0.5..=2.0;

impl StreamControl {
    fn rate(&self) -> f32 {
        match self.rate_bits.load(Ordering::Relaxed) {
            0 => 1.0,
            bits => f32::from_bits(bits),
        }
    }
}

/// Control of a playing media stream: volume, stop, and the frames the output consumed.
#[derive(Debug, Clone)]
pub struct MediaStreamHandle {
    control: Arc<StreamControl>,
    mix_rate: u32,
}

impl MediaStreamHandle {
    /// Frames the output consumed from this stream (its audio clock).
    pub fn frames(&self) -> u64 {
        self.control.frames.load(Ordering::Acquire)
    }

    /// The mix rate the frame count is in: the rate the output actually mixes at once it has
    /// mixed (a device may open at another rate than the settings ask for), else the configured
    /// rate.
    pub fn mix_rate(&self) -> u32 {
        match self.control.mixed_rate.load(Ordering::Acquire) {
            0 => self.mix_rate,
            rate => rate,
        }
    }

    /// Holds (`true`) or releases the stream: held, it plays silence, takes nothing from the feed
    /// and counts no frames, so its audio clock stands still with the picture (specs/50 § 2.2:
    /// a pause silences the feed within one audio block).
    pub fn set_paused(&self, paused: bool) {
        self.control.paused.store(paused, Ordering::Release);
    }

    /// Whether the stream is held by [`Self::set_paused`].
    pub fn is_paused(&self) -> bool {
        self.control.paused.load(Ordering::Acquire)
    }

    /// Sets the playback rate the feed is resampled by (specs/50 § 2.5): inside
    /// [`MEDIA_FEED_RATES`] the feed plays faster or slower (pitch follows); outside it the stream
    /// is muted but still drains the feed at the rate, so it stays in step with the picture. A
    /// rate that is not finite or not positive is ignored.
    pub fn set_rate(&self, rate: f32) {
        if rate.is_finite() && rate > 0.0 {
            self.control.rate_bits.store(rate.to_bits(), Ordering::Relaxed);
        }
    }

    /// The playback rate (1.0 until [`Self::set_rate`]).
    pub fn rate(&self) -> f32 {
        self.control.rate()
    }

    /// True once [`Self::stop`] was called.
    pub(crate) fn is_stopped(&self) -> bool {
        self.control.stopped.load(Ordering::Acquire)
    }

    /// Output frames that found the feed empty (silence was played).
    pub fn underruns(&self) -> u64 {
        self.control.underruns.load(Ordering::Relaxed)
    }

    /// Sets the linear volume.
    pub fn set_volume(&self, volume: f32) {
        let v = if volume.is_finite() {
            volume.clamp(0.0, 4.0)
        } else {
            0.0
        };
        self.control.volume_bits.store(v.to_bits(), Ordering::Relaxed);
    }

    /// Stops the stream; the mixer unloads it.
    pub fn stop(&self) {
        self.control.stopped.store(true, Ordering::Release);
    }
}

/// A kira sound that plays a [`PcmConsumer`] (no allocation on the audio thread).
#[derive(Debug)]
pub struct MediaStreamData {
    feed_rate: f64,
    control: Arc<StreamControl>,
    mix_rate: u32,
}

impl MediaStreamData {
    /// A stream of `consumer` (at `feed_rate` Hz) for a mixer at `mix_rate` Hz.
    pub fn new(consumer: PcmConsumer, feed_rate: u32, mix_rate: u32, volume: f32) -> Self {
        let control = Arc::new(StreamControl {
            channels: usize::from(consumer.channels()).clamp(1, 8),
            reader: Mutex::new(Some(consumer)),
            ..StreamControl::default()
        });
        control
            .volume_bits
            .store(volume.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
        Self {
            feed_rate: f64::from(feed_rate.max(1)),
            control,
            mix_rate: mix_rate.max(1),
        }
    }

    /// A stream that continues `handle`'s stream (same consumer, volume, frame count and stop
    /// flag): the rebuild of a stream whose mixer an output suspend dropped.
    pub(crate) fn resumed(feed_rate: u32, mix_rate: u32, handle: &MediaStreamHandle) -> Self {
        Self {
            feed_rate: f64::from(feed_rate.max(1)),
            control: Arc::clone(&handle.control),
            mix_rate: mix_rate.max(1),
        }
    }

    /// The handle (also returned by `into_sound`).
    pub fn handle(&self) -> MediaStreamHandle {
        MediaStreamHandle {
            control: Arc::clone(&self.control),
            mix_rate: self.mix_rate,
        }
    }
}

impl SoundData for MediaStreamData {
    type Error = Infallible;
    type Handle = MediaStreamHandle;

    fn into_sound(self) -> Result<(Box<dyn Sound>, Self::Handle), Self::Error> {
        let handle = self.handle();
        let channels = self.control.channels.clamp(1, 8);
        Ok((
            Box::new(MediaStreamSound {
                channels,
                feed_rate: self.feed_rate,
                control: self.control,
                a: [0.0; 8],
                b: [0.0; 8],
                pos: 1.0,
            }),
            handle,
        ))
    }
}

/// Fixed-weight stereo downmix of a feed frame (common SMPTE order: L R C LFE Ls Rs [Lb Rb]).
/// Two channels pass through; the LFE channel is dropped; the sum is normalized by the summed
/// weights so the downmix cannot exceed full scale on its own.
fn downmix_to_stereo(v: &[f32; 8], channels: usize) -> (f32, f32) {
    const SIDE: f32 = std::f32::consts::FRAC_1_SQRT_2;
    if channels <= 2 {
        return (v[0], v[1]);
    }
    let (mut l, mut r) = (v[0], v[1]);
    let mut weight = 1.0;
    let centre = v[2] * SIDE;
    l += centre;
    r += centre;
    weight += SIDE;
    // (left, right) surround index pairs per layout.
    let pairs: &[(usize, usize)] = match channels {
        5 => &[(3, 4)],
        6 | 7 => &[(4, 5)],
        8 => &[(4, 5), (6, 7)],
        _ => &[],
    };
    for &(ls, rs) in pairs {
        l += v[ls] * SIDE;
        r += v[rs] * SIDE;
        weight += SIDE;
    }
    if channels == 4 || channels == 7 {
        // A single back-centre channel feeds both sides.
        let back = v[channels - 1] * SIDE;
        l += back;
        r += back;
        weight += SIDE;
    }
    (l / weight, r / weight)
}

struct MediaStreamSound {
    channels: usize,
    feed_rate: f64,
    control: Arc<StreamControl>,
    /// The two feed frames the output position lies between.
    a: [f32; 8],
    b: [f32; 8],
    /// Position between `a` (0) and `b` (1).
    pos: f64,
}

impl MediaStreamSound {
    /// Steps one feed frame: `b` becomes `a` and the next frame is read into `b` (silence and an
    /// underrun without a reader).
    fn advance(&mut self, reader: &mut Option<PcmConsumer>) {
        self.a = self.b;
        let n = match reader.as_mut() {
            Some(consumer) => consumer.pop(&mut self.b[..self.channels]),
            None => {
                self.b[..self.channels].fill(0.0);
                0
            }
        };
        if n < self.channels {
            self.control.underruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Discards `frames` whole feed frames (the catch-up drain); frames past the feed's end
    /// count one underrun each, exactly as a stepped frame does.
    fn skip_frames(&mut self, reader: &mut Option<PcmConsumer>, frames: u64) {
        match reader.as_mut() {
            Some(consumer) => {
                consumer.skip_frames(frames);
            }
            None => {
                // The reader is locked elsewhere (a rebuilt stream's first block): every
                // skipped frame found silence, as `advance` would have.
                if frames > 0 {
                    self.control.underruns.fetch_add(frames, Ordering::Relaxed);
                }
            }
        }
    }
}

impl Sound for MediaStreamSound {
    fn process(&mut self, out: &mut [Frame], dt: f64, _info: &Info) {
        let volume = f32::from_bits(self.control.volume_bits.load(Ordering::Relaxed));
        // The output's real rate, for the clock (`MediaStreamHandle::mix_rate`); published by the
        // frame count's release below.
        if dt > 0.0 && dt.is_finite() {
            let rate = (1.0 / dt).round();
            if (1.0..=f64::from(u32::MAX)).contains(&rate) {
                self.control.mixed_rate.store(rate as u32, Ordering::Relaxed);
            }
        }
        if self.control.paused.load(Ordering::Acquire) {
            // Held (the player is not playing): silence, nothing taken, no frames counted.
            out.fill(Frame::ZERO);
            return;
        }
        // The player's rate resamples the feed; outside the audible range the stream is muted
        // but keeps draining at the requested rate (the feed must stay in step with the picture
        // at any positive rate, so the drain is not clamped).
        let rate = self.control.rate();
        let audible = MEDIA_FEED_RATES.contains(&rate);
        let step = dt * self.feed_rate * f64::from(rate);
        // The reader for this block. Never blocks the audio thread: a lock held elsewhere (only a
        // rebuilt stream's first block can meet the old one's) plays the block as an underrun.
        let control = Arc::clone(&self.control);
        let mut taken = match control.reader.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        let mut none = None;
        let reader = match taken.as_deref_mut() {
            Some(reader) => reader,
            None => &mut none,
        };
        for frame in out.iter_mut() {
            self.pos += step;
            // Drain every whole feed frame the step crosses: a 768 kHz feed (the container
            // maximum) at rate 4 crosses ~64 frames per output frame at 48 kHz and more at
            // other mix rates, so the loop is not capped. Whole frames beyond the last pair
            // are discarded in one bulk skip (at most the ring capacity: frames past the
            // queued data are silence, and an extreme rate would otherwise skip `u64::MAX`
            // frames per output frame) so an extreme rate cannot spin the audio thread.
            while self.pos >= 1.0 {
                let whole = self.pos.floor();
                if whole > 1.0 {
                    let bulk = whole - 1.0;
                    let cap = match reader.as_ref() {
                        Some(consumer) => consumer.capacity_frames() as f64,
                        // No reader this block (a rebuilt stream's first): `skip_frames` counts
                        // one underrun per frame, as `advance` would for a single frame.
                        None => 1.0,
                    };
                    self.skip_frames(reader, bulk.min(cap + 1.0) as u64);
                    self.pos -= bulk;
                }
                self.pos -= 1.0;
                self.advance(reader);
            }
            let t = self.pos.clamp(0.0, 1.0) as f32;
            let lerp = |c: usize| {
                let v = self.a[c] + (self.b[c] - self.a[c]) * t;
                if v.is_finite() {
                    v.clamp(-1.0, 1.0) * volume
                } else {
                    0.0
                }
            };
            *frame = if !audible {
                Frame::ZERO
            } else if self.channels >= 2 {
                let mut v = [0.0f32; 8];
                for (c, slot) in v.iter_mut().enumerate().take(self.channels) {
                    *slot = lerp(c);
                }
                let (left, right) = downmix_to_stereo(&v, self.channels);
                Frame { left, right }
            } else {
                Frame::from_mono(lerp(0))
            };
        }
        self.control.frames.fetch_add(out.len() as u64, Ordering::AcqRel);
    }

    fn finished(&self) -> bool {
        self.control.stopped.load(Ordering::Acquire)
    }
}

/// Feeds registered by `regia_media`: pending ones wait for `attach_media_feeds`, playing ones keep
/// their stream handle (volume, stop, audio clock).
#[derive(Debug, Default, Resource)]
pub struct MediaAudioRegistry {
    feeds: Vec<MediaFeedEntry>,
    playing: Vec<PlayingFeed>,
}

/// One attached media stream.
#[derive(Debug)]
struct PlayingFeed {
    owner: u64,
    bus: String,
    handle: MediaStreamHandle,
    /// The feed's sample rate, to rebuild the stream after an output suspend (`None` for a
    /// stream recorded through [`MediaAudioRegistry::attached`], which is not rebuilt).
    feed_rate: Option<u32>,
    /// `Audio::suspend_count` when the stream was (re)started on the current mixer.
    suspends: u64,
}

impl MediaAudioRegistry {
    /// Registers (or replaces) the feed of `owner`; a replaced stream stops.
    pub fn register(&mut self, entry: MediaFeedEntry) {
        self.unregister(entry.owner);
        self.feeds.push(entry);
    }

    /// Removes the feed of `owner` and stops its stream.
    pub fn unregister(&mut self, owner: u64) {
        self.feeds.retain(|f| f.owner != owner);
        for feed in self.playing.iter().filter(|f| f.owner == owner) {
            feed.handle.stop();
        }
        self.playing.retain(|f| f.owner != owner);
    }

    /// Feeds waiting to be attached.
    pub fn feeds(&self) -> &[MediaFeedEntry] {
        &self.feeds
    }

    /// Feeds waiting to be attached, mutable.
    pub fn feeds_mut(&mut self) -> &mut [MediaFeedEntry] {
        &mut self.feeds
    }

    /// Takes the pending feeds (the attach system).
    pub fn take_pending(&mut self) -> Vec<MediaFeedEntry> {
        std::mem::take(&mut self.feeds)
    }

    /// Records a started stream and the bus it plays on. A stream recorded here is not rebuilt
    /// after an output suspend (the registry does not know its feed rate); `attach_media_feeds`
    /// records the streams it starts with their rate, and those are.
    pub fn attached(&mut self, owner: u64, bus: String, handle: MediaStreamHandle) {
        self.playing.push(PlayingFeed {
            owner,
            bus,
            handle,
            feed_rate: None,
            suspends: 0,
        });
    }

    /// The playing stream of `owner`.
    pub fn stream(&self, owner: u64) -> Option<&MediaStreamHandle> {
        self.playing.iter().find(|f| f.owner == owner).map(|f| &f.handle)
    }

    /// The bus `owner`'s stream plays on.
    pub fn bus_of(&self, owner: u64) -> Option<&str> {
        self.playing
            .iter()
            .find(|f| f.owner == owner)
            .map(|f| f.bus.as_str())
    }

    /// Sets the volume of `owner`'s feed (pending or playing).
    pub fn set_volume(&mut self, owner: u64, volume: f32) {
        for f in self.feeds.iter_mut().filter(|f| f.owner == owner) {
            f.volume = volume;
        }
        if let Some(h) = self.stream(owner) {
            h.set_volume(volume);
        }
    }
}

/// `AudioPlugin` system: starts a stream on the named bus for every pending media feed, and
/// rebuilds the playing ones after an output suspend dropped the mixer they played on (their
/// volume, frame count and stop flag carry over; concurrency audit 11-03 H1).
pub fn attach_media_feeds(audio: Option<ResMut<crate::Audio>>, registry: Option<ResMut<MediaAudioRegistry>>) {
    let (Some(mut audio), Some(mut registry)) = (audio, registry) else {
        return;
    };
    let suspends = audio.suspend_count();
    for entry in registry.take_pending() {
        let owner = entry.owner;
        let rate = entry.sample_rate;
        match audio.play_media_feed(&entry.bus, entry.consumer, rate, entry.volume) {
            Ok((handle, bus)) => registry.playing.push(PlayingFeed {
                owner,
                bus,
                handle,
                feed_rate: Some(rate),
                suspends,
            }),
            Err(e) => tracing::warn!(target: "regia_audio", owner, "media feed not attached: {e}"),
        }
    }
    if audio.output_suspended() {
        return;
    }
    for feed in registry.playing.iter_mut().filter(|f| f.suspends != suspends) {
        feed.suspends = suspends;
        let Some(rate) = feed.feed_rate else { continue };
        if feed.handle.is_stopped() {
            continue;
        }
        match audio.resume_media_feed(&feed.bus, rate, &feed.handle) {
            Ok(bus) => feed.bus = bus,
            Err(e) => tracing::warn!(
                target: "regia_audio",
                owner = feed.owner,
                "media feed not restored after the output resumed: {e}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_keeps_the_centre_and_surrounds_and_drops_the_lfe() {
        // 5.1: only the centre carries dialogue.
        let mut v = [0.0f32; 8];
        v[2] = 1.0;
        let (l, r) = downmix_to_stereo(&v, 6);
        assert!(l > 0.1 && (l - r).abs() < 1e-6, "{l} {r}");
        // LFE only: silent.
        let mut lfe = [0.0f32; 8];
        lfe[3] = 1.0;
        assert_eq!(downmix_to_stereo(&lfe, 6), (0.0, 0.0));
        // Left surround reaches the left side only.
        let mut ls = [0.0f32; 8];
        ls[4] = 1.0;
        let (l, r) = downmix_to_stereo(&ls, 6);
        assert!(l > 0.1 && r == 0.0);
        // Full-scale everywhere never exceeds 1.
        for ch in 3..=8 {
            let (l, r) = downmix_to_stereo(&[1.0; 8], ch);
            assert!(l <= 1.0 + 1e-6 && r <= 1.0 + 1e-6, "{ch}: {l} {r}");
        }
        // Stereo passes through unchanged.
        assert_eq!(
            downmix_to_stereo(&[0.25, -0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 2),
            (0.25, -0.5)
        );
    }

    #[test]
    fn producer_clear_drops_queued_samples_but_keeps_later_ones() {
        let (mut p, mut c) = PcmFeed::channel(1, 16).expect("the channel count is in 1..=8");
        p.push(&[1.0, 2.0, 3.0]);
        p.clear();
        p.push(&[4.0, 5.0]);
        let mut out = [0.0; 4];
        assert_eq!(c.pop(&mut out), 2);
        assert_eq!(out, [4.0, 5.0, 0.0, 0.0]);
        // A clear after the reader already consumed everything changes nothing.
        p.push(&[6.0]);
        assert_eq!(c.pop(&mut out[..1]), 1);
        p.clear();
        assert_eq!(c.pop(&mut out[..1]), 0);
    }

    #[test]
    fn ring_round_trips_and_wraps() {
        let (mut p, mut c) = PcmFeed::channel(2, 4).expect("the channel count is in 1..=8");
        assert_eq!(p.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 6);
        assert_eq!(p.push(&[7.0, 8.0, 9.0, 10.0]), 2, "capacity is 8 samples");
        let mut out = [0.0; 4];
        assert_eq!(c.pop(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(p.push(&[9.0, 10.0]), 2);
        let mut out = [0.0; 6];
        assert_eq!(c.pop(&mut out), 6);
        assert_eq!(out, [5.0, 6.0, 7.0, 8.0, 9.0, 10.0]);
        assert_eq!(c.underruns(), 0);
    }

    #[test]
    fn underrun_fills_silence_and_counts() {
        let (mut p, mut c) = PcmFeed::channel(1, 8).expect("the channel count is in 1..=8");
        p.push(&[0.5, 0.5]);
        let mut out = [9.0; 4];
        assert_eq!(c.pop(&mut out), 2);
        assert_eq!(out, [0.5, 0.5, 0.0, 0.0]);
        assert_eq!(c.underruns(), 1);
    }

    #[test]
    fn producer_on_another_thread() {
        let (mut p, mut c) = PcmFeed::channel(1, 64).expect("the channel count is in 1..=8");
        let t = std::thread::spawn(move || {
            let mut sent = 0usize;
            while sent < 1000 {
                let chunk: Vec<f32> = (sent..(sent + 10).min(1000)).map(|v| v as f32).collect();
                let n = p.push(&chunk);
                sent += n;
                if n == 0 {
                    std::thread::yield_now();
                }
            }
        });
        let mut got = Vec::new();
        let mut buf = [0.0f32; 7];
        while got.len() < 1000 {
            let n = c.pop(&mut buf);
            got.extend_from_slice(&buf[..n]);
            if n == 0 {
                std::thread::yield_now();
            }
        }
        assert!(t.join().is_ok());
        assert!(got.iter().enumerate().all(|(i, v)| *v == i as f32));
    }

    #[test]
    fn clock_and_registry() {
        let clock = AudioClock::new(48_000);
        clock.clone().advance(480);
        assert_eq!(clock.frames(), 480);
        let mut reg = MediaAudioRegistry::default();
        let (_, consumer) = PcmFeed::channel(2, 16).expect("the channel count is in 1..=8");
        reg.register(MediaFeedEntry {
            owner: 7,
            bus: "sfx".into(),
            consumer,
            volume: 1.0,
            sample_rate: 48_000,
        });
        let (_, consumer) = PcmFeed::channel(2, 16).expect("the channel count is in 1..=8");
        reg.register(MediaFeedEntry {
            owner: 7,
            bus: "music".into(),
            consumer,
            volume: 1.0,
            sample_rate: 48_000,
        });
        assert_eq!(reg.feeds().len(), 1);
        assert_eq!(reg.feeds()[0].bus, "music");
        reg.unregister(7);
        assert!(reg.feeds().is_empty());
        assert!(reg.stream(7).is_none());
    }

    fn headless() -> crate::Audio {
        crate::Audio::new(&crate::AudioSettings {
            headless: true,
            buses: crate::BusLayout::default_tree(),
            ..crate::AudioSettings::default()
        })
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
    }

    /// specs/50 § 8 test 8: a known tone arrives through the named bus; a missing bus falls back to
    /// master; an underrun inserts silence and counts.
    #[test]
    fn feed_plays_on_the_named_bus_offline() {
        let mut audio = headless();
        let rate = 48_000u32;
        let (mut p, c) = PcmFeed::channel(1, rate as usize).expect("the channel count is in 1..=8");
        let tone: Vec<f32> = (0..rate / 10)
            .map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / rate as f32).sin() * 0.5)
            .collect();
        assert_eq!(p.push(&tone), tone.len());
        let (handle, bus) = audio.play_media_feed("sfx", c, rate, 1.0).expect("plays");
        assert_eq!(bus, "Sfx");
        let mut out = Vec::new();
        for _ in 0..4 {
            audio.tick(1.0 / 60.0);
            out.extend(audio.render_offline(800).expect("offline"));
        }
        assert!(
            rms(&out[out.len() / 2..]) > 0.1,
            "the tone is audible: rms {}",
            rms(&out)
        );
        assert!(
            handle.frames() >= 2400,
            "the stream counted the frames it was mixed for"
        );
        let before = handle.underruns();
        for _ in 0..8 {
            audio.tick(1.0 / 60.0);
            let _ = audio.render_offline(800).expect("offline");
        }
        assert!(handle.underruns() > before, "the 0.1 s feed ran dry");
        let (_, c2) = PcmFeed::channel(1, 16).expect("the channel count is in 1..=8");
        let (_, bus2) = audio
            .play_media_feed("no_such_bus", c2, rate, 1.0)
            .expect("falls back");
        assert_eq!(bus2, "Master");
        handle.stop();
    }

    /// An output suspend (minimise, Android pause) drops the mixer and every sound on it,
    /// including the stream holding the feed's only reader. After the resume the registry still
    /// reports the feed playing, so it must be playing again: audible, and its clock counting on
    /// (concurrency audit 11-03 H1).
    #[test]
    fn a_playing_feed_survives_an_output_suspend() {
        use regia_ecs::prelude::bevy_ecs::system::RunSystemOnce;
        let rate = 48_000u32;
        let mut world = World::new();
        world.insert_resource(headless());
        let (mut producer, consumer) =
            PcmFeed::channel(1, rate as usize).expect("the channel count is in 1..=8");
        let mut reg = MediaAudioRegistry::default();
        reg.register(MediaFeedEntry {
            owner: 9,
            bus: "sfx".into(),
            consumer,
            volume: 1.0,
            sample_rate: rate,
        });
        world.insert_resource(reg);
        let _ = world.run_system_once(attach_media_feeds);
        let tone = |n: u32| -> Vec<f32> {
            (0..n)
                .map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / rate as f32).sin() * 0.5)
                .collect::<Vec<f32>>()
        };
        producer.push(&tone(rate / 20));
        for _ in 0..2 {
            let mut audio = world.resource_mut::<crate::Audio>();
            audio.tick(1.0 / 60.0);
            let _ = audio.render_offline(800).expect("offline");
        }
        let before = world
            .resource::<MediaAudioRegistry>()
            .stream(9)
            .map(MediaStreamHandle::frames)
            .unwrap_or(0);
        assert!(before > 0, "the feed played before the suspend");

        {
            let mut audio = world.resource_mut::<crate::Audio>();
            audio.suspend_output();
            assert!(audio.resume_output(), "offline output resumes");
        }
        producer.push(&tone(rate / 10));
        let mut out = Vec::new();
        for _ in 0..4 {
            // `AudioPlugin` runs the system every frame: the old stream's reader may only go
            // when the old tracks are torn down, and the rebuild waits for it.
            let _ = world.run_system_once(attach_media_feeds);
            let mut audio = world.resource_mut::<crate::Audio>();
            audio.tick(1.0 / 60.0);
            out.extend(audio.render_offline(800).expect("offline"));
        }
        let after = world
            .resource::<MediaAudioRegistry>()
            .stream(9)
            .map(MediaStreamHandle::frames)
            .unwrap_or(0);
        assert!(
            after > before,
            "the clock froze at {before} frames after the resume"
        );
        assert!(
            rms(&out[out.len() / 2..]) > 0.1,
            "the feed is silent after the resume: rms {}",
            rms(&out)
        );
    }

    /// The stream's clock is in frames of the output it plays on; a device that opened at
    /// another rate than the settings' must report the rate it really mixes at, or the player's
    /// audio clock drifts (48 kHz settings on a 44.1 kHz device: 5 s per minute; audit 11-03 M2).
    #[test]
    fn the_stream_reports_the_rate_its_output_mixes_at() {
        let (mut producer, consumer) = PcmFeed::channel(1, 4096).expect("the channel count is in 1..=8");
        producer.push(&[0.25; 2048]);
        let data = MediaStreamData::new(consumer, 48_000, 48_000, 1.0);
        let (mut sound, handle) = data.into_sound().expect("infallible");
        let info = kira::info::MockInfoBuilder::new().build();
        let mut out = vec![Frame::ZERO; 441];
        sound.process(&mut out, 1.0 / 44_100.0, &info);
        assert_eq!(handle.frames(), 441);
        assert_eq!(
            handle.mix_rate(),
            44_100,
            "the clock divides by the configured rate"
        );
    }

    /// Outside MEDIA_FEED_RATES the stream is muted but must still drain the feed at the
    /// requested rate, so the audio clock stays in step with the picture (correctness audit
    /// finding 6). Rate 8 at a 48 kHz feed and mix crosses 8 feed frames per output frame.
    #[test]
    fn a_muted_high_rate_still_drains_the_feed_at_the_requested_rate() {
        let (mut producer, consumer) = PcmFeed::channel(1, 1 << 20).expect("valid channel count");
        producer.push(&vec![0.5; 100_000]);
        let data = MediaStreamData::new(consumer, 48_000, 48_000, 1.0);
        let (mut sound, handle) = data.into_sound().expect("infallible");
        handle.set_rate(8.0);
        let info = kira::info::MockInfoBuilder::new().build();
        let mut out = vec![Frame::ZERO; 480];
        sound.process(&mut out, 1.0 / 48_000.0, &info);
        assert!(
            out.iter().all(|f| f.left == 0.0 && f.right == 0.0),
            "rate 8 is muted"
        );
        // The initial position is 1.0 (one frame pending), so 480 output frames at 8x drain
        // exactly 3840 + 1 feed frames — not the 1920 a 4.0 clamp would leave queued.
        assert_eq!(
            producer.queued(),
            100_000 - 3_841,
            "the feed drains at the requested rate"
        );
    }

    /// The catch-up drain covers the whole step at any feed rate: a 768 kHz feed (the container
    /// maximum) at rate 4 and a 44.1 kHz mix crosses ~69.7 feed frames per output frame, which a
    /// per-frame cap of 64 would silently under-drain (correctness audit finding 6).
    #[test]
    fn the_catch_up_drain_covers_the_whole_step() {
        let (mut producer, consumer) = PcmFeed::channel(1, 1 << 20).expect("valid channel count");
        producer.push(&vec![0.25; 200_000]);
        let data = MediaStreamData::new(consumer, 768_000, 44_100, 1.0);
        let (mut sound, handle) = data.into_sound().expect("infallible");
        handle.set_rate(4.0);
        let info = kira::info::MockInfoBuilder::new().build();
        let mut out = vec![Frame::ZERO; 441];
        sound.process(&mut out, 1.0 / 44_100.0, &info);
        // 441 output frames at 768/44.1*4 feed frames each = 30720, plus the one pending frame
        // the stream starts with. A 64-per-output-frame cap would drain at most ~28225.
        let consumed = 200_000 - producer.queued();
        assert!(consumed >= 30_720, "the whole step drained, consumed {consumed}");
        assert!(
            consumed <= 30_722,
            "no over-drain beyond the step, consumed {consumed}"
        );
    }

    /// `PcmConsumer::skip_frames` is public, so a hostile or extreme caller may pass a huge
    /// frame count: frames past the queued data count one underrun each, and the counter must
    /// saturate at `u64::MAX` instead of wrapping (fixrev2 review, media feed finding 4).
    #[test]
    fn skip_frames_saturates_the_underrun_counter() {
        let (_producer, mut consumer) = PcmFeed::channel(1, 16).expect("the channel count is in 1..=8");
        assert_eq!(consumer.skip_frames(u64::MAX), 0);
        assert_eq!(consumer.underruns(), u64::MAX, "the first extreme skip saturates");
        let _ = consumer.skip_frames(u64::MAX);
        assert_eq!(
            consumer.underruns(),
            u64::MAX,
            "the counter stays saturated instead of wrapping"
        );
    }

    /// Feeds are at most 8 interleaved channels wide: the stream downmixes an 8-wide frame and a
    /// wider ring would be read at the wrong stride (correctness audit finding 8).
    #[test]
    fn a_feed_rejects_channel_counts_outside_1_to_8() {
        assert!(PcmFeed::channel(0, 16).is_none());
        assert!(PcmFeed::channel(9, 16).is_none());
        assert!(PcmFeed::channel(10, 16).is_none());
        for channels in 1..=8 {
            assert!(
                PcmFeed::channel(channels, 16).is_some(),
                "{channels} channels is a valid feed"
            );
        }
    }

    #[test]
    fn attach_system_starts_pending_feeds() {
        let mut world = World::new();
        world.insert_resource(headless());
        let mut reg = MediaAudioRegistry::default();
        let (_, consumer) = PcmFeed::channel(2, 64).expect("the channel count is in 1..=8");
        reg.register(MediaFeedEntry {
            owner: 3,
            bus: "music".into(),
            consumer,
            volume: 0.5,
            sample_rate: 44_100,
        });
        world.insert_resource(reg);
        use regia_ecs::prelude::bevy_ecs::system::RunSystemOnce;
        let _ = world.run_system_once(attach_media_feeds);
        let reg = world.resource::<MediaAudioRegistry>();
        assert!(reg.feeds().is_empty());
        assert_eq!(reg.bus_of(3), Some("Music"));
        assert!(reg.stream(3).is_some());
    }

    /// `capacity_frames` reports the frames the ring allocated: exactly as many interleaved
    /// samples fit before `push` starts dropping (ADR-7635 measures a player's ring through it).
    #[test]
    fn capacity_frames_is_what_the_ring_holds() {
        for (channels, frames) in [(1u8, 1usize), (2, 480), (6, 1_000), (8, 33)] {
            let (mut producer, consumer) = PcmFeed::channel(channels, frames).expect("1..=8 channels");
            assert_eq!(consumer.capacity_frames(), frames as u64);
            let samples = vec![0.25f32; (frames + 3) * usize::from(channels)];
            assert_eq!(producer.push(&samples), frames * usize::from(channels));
        }
        // A zero request still gets one frame.
        let (_, consumer) = PcmFeed::channel(2, 0).expect("stereo");
        assert_eq!(consumer.capacity_frames(), 1);
    }
}
