// SPDX-License-Identifier: MPL-2.0
//! `VoiceChatPlugin`: owns the session resource and pumps it from the frame loop
//! (`docs/specs/59-voice-chat.md` § 3).
//!
//! In the deterministic mode the pump runs a whole number of 20 ms voice frames per tick from a
//! rational accumulator (50 voice frames per second over `ticks_per_second` ticks), independent of
//! render rate and wall clock. A live game has no fixed update rate, so the pump follows the real
//! frame delta instead, capped so a long stall does not run a burst of frames.

use super::codec::SharedCodecProvider;
use super::emitter::{VoiceChatEmitter, VoiceChatOutput, voice_chat_output};
use super::session::{VoiceChatResource, VoiceChatSession};
use super::settings::{VoiceChatSettings, VoiceChatUserSettings};
use crate::plugin::audio_sync;
use regia_app::{App, Lifecycle, Plugin, PostUpdate, PostUpdateSet, PreUpdate, RealTime};
use regia_ecs::prelude::*;
use regia_input::Actions;
use regia_platform::{AppLifecycle, LifecycleEvent};

/// Voice frames per second (20 ms each).
const VOICE_FRAMES_PER_SECOND: u32 = 50;
/// Name of the push-to-talk input action (spec § 2.6). A project binds it in its input contexts.
pub const VOICE_TALK_ACTION: &str = "voice_chat_talk";
/// Most voice frames one update runs in live mode (100 ms); a longer stall drops the rest.
const MAX_FRAMES_PER_UPDATE: u32 = 5;

/// Adds [`VoiceChatResource`] and the system that pumps it.
///
/// Insert [`VoiceChatSettings`] and [`VoiceChatUserSettings`] before adding the plugin to change
/// the defaults. Push-to-talk comes from the [`VOICE_TALK_ACTION`] input action, or from a game
/// that writes [`VoiceChatTalk`] itself; the transport is `regia_net::voice` or the game's own (see
/// [`pump_transport`](super::pump_transport)). Played frames go to the `Voice` bus when an
/// [`Audio`](crate::Audio) resource exists; put a [`VoiceChatEmitter`] on a speaker's entity to
/// place it. The microphone stays closed until the game calls
/// [`set_microphone`](VoiceChatSession::set_microphone) and
/// [`enable_microphone`](VoiceChatSession::enable_microphone). A game that brings its own voice core
/// simply never adds this plugin.
pub struct VoiceChatPlugin {
    /// Simulation ticks per second the pump is called at (60 by default).
    pub ticks_per_second: u32,
}

impl Default for VoiceChatPlugin {
    fn default() -> Self {
        Self { ticks_per_second: 60 }
    }
}

/// The codec provider for sessions the plugin builds. `regia_media::voice_opus::VoiceOpusPlugin`
/// inserts it; a game may insert its own before adding [`VoiceChatPlugin`].
#[derive(Resource, Clone)]
pub struct VoiceChatCodecs(pub SharedCodecProvider);

/// The state of the `voice_chat_talk` action, written by the game's input code.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoiceChatTalk(pub bool);

#[derive(Resource)]
struct VoiceChatPump {
    ticks_per_second: u32,
    accumulator: u32,
    seconds: f64,
}

impl Plugin for VoiceChatPlugin {
    fn build(&self, app: &mut App) {
        let settings = app
            .world()
            .get_resource::<VoiceChatSettings>()
            .cloned()
            .unwrap_or_default();
        let user = app
            .world()
            .get_resource::<VoiceChatUserSettings>()
            .cloned()
            .unwrap_or_default();
        let session = VoiceChatSession::new(settings, user).or_else(|e| {
            tracing::warn!(target: "regia_audio", "voice chat settings are invalid ({e}); using the defaults");
            VoiceChatSession::new(VoiceChatSettings::default(), VoiceChatUserSettings::default())
        });
        match session {
            Ok(mut session) => {
                if let Some(codecs) = app.world().get_resource::<VoiceChatCodecs>()
                    && let Err(e) = session.set_codec_provider(codecs.0.clone())
                {
                    tracing::error!(target: "regia_audio", "voice chat stays on PCM: {e}");
                }
                app.insert_resource(VoiceChatResource(std::sync::Mutex::new(session)))
                    .init_resource::<VoiceChatTalk>()
                    .insert_resource(VoiceChatPump {
                        ticks_per_second: self.ticks_per_second.max(1),
                        accumulator: 0,
                        seconds: 0.0,
                    })
                    .init_resource::<VoiceChatOutput>()
                    .register_type::<VoiceChatEmitter>()
                    .add_systems(PreUpdate, voice_chat_pump)
                    .add_systems(Lifecycle, voice_chat_lifecycle)
                    .add_systems(
                        PostUpdate,
                        voice_chat_output
                            .in_set(PostUpdateSet::AudioSync)
                            .after(audio_sync),
                    );
            }
            Err(e) => tracing::error!(target: "regia_audio", "voice chat is disabled: {e}"),
        }
    }
}

fn voice_chat_pump(
    session: Res<VoiceChatResource>,
    talk: Res<VoiceChatTalk>,
    mut pump: ResMut<VoiceChatPump>,
    real: Option<Res<RealTime>>,
    actions: Option<Res<Actions>>,
) {
    let _span = tracing::trace_span!("voice_chat_pump").entered();
    let mut session = session.lock_session();
    let pressed = talk.0 || actions.is_some_and(|a| a.pressed(VOICE_TALK_ACTION));
    let delta = real.map_or(0.0, |r| r.raw_delta().as_secs_f64());
    if session.settings().deterministic || delta <= 0.0 {
        pump.accumulator += VOICE_FRAMES_PER_SECOND;
        while pump.accumulator >= pump.ticks_per_second {
            pump.accumulator -= pump.ticks_per_second;
            session.tick(pressed);
        }
        return;
    }
    pump.seconds += delta;
    let frame = 1.0 / f64::from(VOICE_FRAMES_PER_SECOND);
    let due = (pump.seconds / frame).floor();
    pump.seconds -= due * frame;
    for _ in 0..(due as u32).min(MAX_FRAMES_PER_UPDATE) {
        session.tick(pressed);
    }
}

/// `Lifecycle`: the microphone closes with the app (Android drops streams on pause, spec § 9) and
/// reopens with it; the event order matches the output stream's.
fn voice_chat_lifecycle(lifecycle: Option<Res<AppLifecycle>>, session: Res<VoiceChatResource>) {
    let Some(lifecycle) = lifecycle else {
        return;
    };
    let mut session = session.lock_session();
    match lifecycle.last_event {
        Some(LifecycleEvent::Suspended) => session.suspend_microphone(),
        Some(LifecycleEvent::Resumed) => session.resume_microphone(),
        _ => {}
    }
}
