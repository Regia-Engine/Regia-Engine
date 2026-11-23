// SPDX-License-Identifier: MPL-2.0
//! The real voice chat paths without a device: a scripted microphone, the permission flow, device
//! loss, the codec provider seam, the output stage and the spatial emitter
//! (`docs/specs/59-voice-chat.md` §§ 2.2, 2.5, 2.12; acceptance 7, 10, 12, 15).

use regia_app::{App, PostUpdate};
use regia_audio::voice_chat::*;
use regia_audio::{
    Audio, AudioError, AudioListener, AudioOcclusion, AudioPlugin, AudioSettings, ListenerFrame, RolloffCurve,
};
use regia_core::math::{Affine3A, Vec3};
use regia_scene::GlobalTransform;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const FRAME: usize = 320;

fn settings() -> VoiceChatSettings {
    VoiceChatSettings {
        deterministic: true,
        quality: VoiceQuality::Narrow,
        ..VoiceChatSettings::default()
    }
}

fn consenting() -> VoiceChatUserSettings {
    VoiceChatUserSettings {
        consent: true,
        ..VoiceChatUserSettings::default()
    }
}

fn session_with(mic: &FakeMicrophone, user: VoiceChatUserSettings) -> VoiceChatSession {
    let mut session = VoiceChatSession::new(settings(), user).expect("session");
    session.set_microphone(Box::new(mic.clone()));
    session.set_mode(VoiceMode::Open);
    session
        .set_channel(1, Some(true), Some(true), None)
        .expect("channel");
    session
}

/// One voice frame of a sine at 16 kHz, mono.
fn sine_frame(index: &mut u64) -> Vec<f32> {
    (0..FRAME)
        .map(|_| {
            let phase = std::f64::consts::TAU * 440.0 * (*index as f64) / 16_000.0;
            *index += 1;
            phase.sin() as f32 * 0.25
        })
        .collect()
}

#[test]
fn without_consent_the_microphone_is_never_touched() {
    let mic = FakeMicrophone::new(16_000, 1);
    let mut session = session_with(&mic, VoiceChatUserSettings::default());
    assert_eq!(session.enable_microphone(), Err(VoiceChatError::ConsentRequired));
    for _ in 0..50 {
        session.tick(true);
    }
    assert_eq!((mic.opens(), mic.requests()), (0, 0));
    assert_eq!(session.status().state, "off");
    assert_eq!(session.status().microphone, "off");
}

#[test]
fn a_session_needs_an_explicit_enable_action() {
    let mic = FakeMicrophone::new(16_000, 1);
    let mut session = session_with(&mic, consenting());
    for _ in 0..50 {
        session.tick(true);
    }
    assert_eq!(mic.opens(), 0, "consent alone opens nothing");
}

#[test]
fn a_fake_microphone_reaches_the_wire_as_exact_frames() {
    let mic = FakeMicrophone::new(16_000, 1);
    let mut session = session_with(&mic, consenting());
    session.enable_microphone().expect("enable");
    assert_eq!(session.status().state, "idle");
    assert_eq!(session.status().microphone, "open");
    assert_eq!(session.status().device.as_deref(), Some("fake microphone"));
    let mut index = 0;
    for _ in 0..20 {
        mic.push(&sine_frame(&mut index));
        session.tick(false);
    }
    assert_eq!(session.stats().frames_encoded, 20);
    let packets = session.take_outgoing();
    assert_eq!(packets.len(), 20);
    let first = VoicePacket::parse(&packets[0]).expect("parses");
    assert!(
        first.flags.contains(VoiceFlags::RESET),
        "a new stream flushes the decoder"
    );
    assert_eq!(first.codec, VoiceCodecId::Pcm);
    assert!(
        !VoicePacket::parse(&packets[1])
            .expect("parses")
            .flags
            .contains(VoiceFlags::RESET)
    );
}

#[test]
fn stereo_44k_chunks_of_any_size_become_frames() {
    let mic = FakeMicrophone::new(44_100, 2);
    let mut session = session_with(&mic, consenting());
    session.enable_microphone().expect("enable");
    let mut total = 0usize;
    for chunk in [441usize, 17, 1000, 3, 882].iter().cycle().take(100) {
        mic.push(&vec![0.1; chunk * 2]);
        total += chunk;
        session.tick(false);
    }
    // No backlog overrun at this rate: every whole frame was encoded or is still waiting.
    assert_eq!(session.stats().capture_overruns, 0);
    let frames = (total as f64 * 16_000.0 / 44_100.0 / 320.0).floor() as u64;
    assert!(
        session.stats().frames_encoded + 1 >= frames.saturating_sub(5).min(100),
        "{} of {frames}",
        session.stats().frames_encoded
    );
}

#[test]
fn a_stalled_game_drops_old_capture_instead_of_sending_it_late() {
    let mic = FakeMicrophone::new(16_000, 1);
    let mut session = session_with(&mic, consenting());
    session.enable_microphone().expect("enable");
    mic.push(&vec![0.1; FRAME * 40]);
    session.tick(false);
    assert_eq!(session.stats().capture_overruns, 35);
}

#[test]
fn the_permission_prompt_appears_once_and_a_grant_opens_the_device() {
    let mic = FakeMicrophone::new(16_000, 1);
    mic.set_permission(MicPermission::NotAsked);
    mic.answer_prompt_with(MicPermission::Granted);
    let mut session = session_with(&mic, consenting());
    session.enable_microphone().expect("prompt shown");
    assert_eq!(mic.requests(), 1);
    assert_eq!(session.status().microphone, "awaiting_permission");
    for _ in 0..10 {
        session.tick(false);
    }
    assert_eq!(
        (mic.requests(), mic.opens()),
        (1, 0),
        "polling does not ask again"
    );
    mic.resolve_prompt();
    session.tick(false);
    assert_eq!(mic.opens(), 1);
    assert_eq!(session.status().state, "idle");
    assert!(
        session
            .take_events()
            .contains(&VoiceChatEvent::PermissionResult { granted: true })
    );
}

#[test]
fn a_denial_reports_whether_asking_again_can_work() {
    let mic = FakeMicrophone::new(16_000, 1);
    mic.set_permission(MicPermission::NotAsked);
    mic.answer_prompt_with(MicPermission::Denied { can_prompt: true });
    let mut session = session_with(&mic, consenting());
    session.enable_microphone().expect("prompt shown");
    mic.resolve_prompt();
    session.tick(false);
    let status = session.status();
    assert_eq!((status.state, status.error), ("error", Some("permission_denied")));
    assert!(status.can_prompt);
    assert_eq!(mic.opens(), 0);

    // An explicit retry asks again; the user picks "don't ask again".
    mic.answer_prompt_with(MicPermission::Denied { can_prompt: false });
    session.enable_microphone().expect("prompt shown");
    assert_eq!(mic.requests(), 2);
    mic.resolve_prompt();
    session.tick(false);
    assert!(!session.status().can_prompt);

    // From here only the system settings help: no prompt, a plain refusal.
    assert_eq!(session.enable_microphone(), Err(VoiceChatError::PermissionDenied));
    assert_eq!(mic.requests(), 2);
    for _ in 0..300 {
        session.tick(false);
    }
    assert_eq!(mic.opens(), 0, "a denied state never retries on its own");
}

#[test]
fn device_loss_owes_end_of_talk_and_recovers_with_a_reset() {
    let mic = FakeMicrophone::new(16_000, 1);
    let mut session = session_with(&mic, consenting());
    session.enable_microphone().expect("enable");
    let mut index = 0;
    for _ in 0..5 {
        mic.push(&sine_frame(&mut index));
        session.tick(false);
    }
    assert_eq!(session.status().state, "transmitting");
    session.take_outgoing();
    mic.set_present(false);
    session.tick(false);
    let status = session.status();
    assert_eq!((status.state, status.error), ("error", Some("no_input_device")));
    assert_eq!(status.microphone, "lost");
    let out = session.take_outgoing();
    assert_eq!(out.len(), 1);
    assert!(
        VoicePacket::parse(&out[0])
            .expect("parses")
            .flags
            .contains(VoiceFlags::END_OF_TALK)
    );
    // Retries are counted in frames; the device stays away for one round.
    let opens = mic.opens();
    for _ in 0..RETRY_FRAMES {
        session.tick(false);
    }
    assert_eq!(mic.opens(), opens + 1);
    assert_eq!(session.status().state, "error");
    mic.set_present(true);
    for _ in 0..=RETRY_FRAMES {
        session.tick(false);
    }
    assert_eq!(session.status().state, "idle");
    assert_eq!(session.status().microphone, "open");
    mic.push(&sine_frame(&mut index));
    session.tick(false);
    session.tick(false);
    let back = session.take_outgoing();
    assert!(
        VoicePacket::parse(&back[0])
            .expect("parses")
            .flags
            .contains(VoiceFlags::RESET),
        "listeners flush their decoder after a device swap"
    );
}

#[test]
fn a_device_lost_while_muted_comes_back_muted() {
    let mic = FakeMicrophone::new(16_000, 1);
    let mut session = session_with(&mic, consenting());
    session.enable_microphone().expect("enable");
    session.set_self_muted(true);
    assert_eq!(session.status().state, "muted");
    mic.set_present(false);
    session.tick(false);
    mic.set_present(true);
    for _ in 0..=RETRY_FRAMES {
        session.tick(false);
    }
    assert_eq!(session.status().state, "muted");
}

#[test]
fn pause_closes_the_stream_and_resume_reopens_it() {
    let mic = FakeMicrophone::new(16_000, 1);
    let mut session = session_with(&mic, consenting());
    session.enable_microphone().expect("enable");
    session.suspend_microphone();
    assert!(!mic.is_open());
    assert_eq!(session.status().microphone, "suspended");
    assert_eq!(session.status().state, "off");
    session.resume_microphone();
    assert!(mic.is_open());
    assert_eq!(session.status().state, "idle");
    assert_eq!(mic.opens(), 2);
}

#[test]
fn withdrawing_consent_closes_the_stream_within_a_frame() {
    let mic = FakeMicrophone::new(16_000, 1);
    let mut session = session_with(&mic, consenting());
    session.enable_microphone().expect("enable");
    session.user_mut().consent = false;
    session.tick(false);
    assert!(!mic.is_open());
    assert_eq!(session.status().state, "off");
}

#[test]
fn turning_voice_chat_off_releases_the_device() {
    let mic = FakeMicrophone::new(16_000, 1);
    let mut session = session_with(&mic, consenting());
    session.enable_microphone().expect("enable");
    session.disable_microphone();
    assert_eq!((mic.is_open(), mic.closes()), (false, 1));
    assert_eq!(session.status().state, "off");
}

#[test]
fn a_busy_or_refused_open_surfaces_its_code() {
    for (error, code) in [
        (VoiceChatError::DeviceBusy, "device_busy"),
        (VoiceChatError::PermissionDenied, "permission_denied"),
        (VoiceChatError::NoInputDevice, "no_input_device"),
    ] {
        let mic = FakeMicrophone::new(16_000, 1);
        mic.set_open_error(Some(error.clone()));
        let mut session = session_with(&mic, consenting());
        assert_eq!(session.enable_microphone(), Err(error));
        assert_eq!(session.status().error, Some(code));
    }
}

#[test]
fn capture_errors_map_to_agent_codes() {
    for (text, code) in [
        (
            "input stream: Access denied by the operating system",
            "permission_denied",
        ),
        ("input stream: device is busy", "device_busy"),
        ("no default input device", "no_input_device"),
        ("unsupported input sample format I24", "unsupported_format"),
    ] {
        let error = AudioError::Capture(text.to_string());
        assert_eq!(classify_capture_error(&error).code(), code, "{text}");
    }
}

/// A codec with a fixed small payload, so the Opus id's packet cap holds.
struct StubCodec(usize);

impl VoiceCodec for StubCodec {
    fn encode(&mut self, _frame: &[f32], out: &mut [u8]) -> Result<usize, VoiceChatError> {
        out[..10].fill(7);
        Ok(10)
    }

    fn decode(
        &mut self,
        _packet: Option<&[u8]>,
        _fec: bool,
        out: &mut [f32],
    ) -> Result<usize, VoiceChatError> {
        out[..self.0].fill(0.25);
        Ok(self.0)
    }
}

/// A provider that counts builds and answers for the Opus id, so the seam is tested without a codec
/// library.
struct CountingProvider(AtomicUsize);

impl VoiceCodecProvider for CountingProvider {
    fn preferred(&self) -> VoiceCodecId {
        VoiceCodecId::Opus
    }

    fn build(
        &self,
        id: VoiceCodecId,
        quality: VoiceQuality,
        _complexity: u8,
    ) -> Result<Box<dyn VoiceCodec + Send>, VoiceChatError> {
        assert_eq!(id, VoiceCodecId::Opus);
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(StubCodec(quality.frame_samples())))
    }
}

#[test]
fn the_provider_codec_carries_transmit_and_fake_speakers() {
    let provider = Arc::new(CountingProvider(AtomicUsize::new(0)));
    let mut session = VoiceChatSession::new(settings(), VoiceChatUserSettings::default()).expect("session");
    assert_eq!(session.status().codec, "pcm");
    session.set_codec_provider(provider.clone()).expect("provider");
    assert_eq!(session.status().codec, "opus");
    session
        .set_channel(2, Some(true), Some(true), None)
        .expect("join");
    session.set_fake_speaker(7, Some(TestSource::sine(440.0, -12.0)));
    for _ in 0..20 {
        session.tick(false);
    }
    assert_eq!(session.status().speakers[0].state, "speaking");
    assert!(
        provider.0.load(Ordering::Relaxed) >= 3,
        "encoder, fake speaker and receive decoder"
    );
}

#[test]
fn opus_without_a_provider_is_refused_and_counted() {
    let mut speaker = VoiceSpeaker::new(1, JitterSettings::default());
    let packet = VoicePacket {
        flags: VoiceFlags::default(),
        sender: 1,
        channel: 1,
        seq: 0,
        timestamp: 0,
        quality: VoiceQuality::Narrow,
        codec: VoiceCodecId::Opus,
        payload: &[0],
    };
    assert_eq!(speaker.receive(&packet, 1), None);
    assert_eq!(speaker.stats().unsupported, 1);
}

#[test]
fn control_messages_queue_for_the_server() {
    let mut session = VoiceChatSession::new(settings(), VoiceChatUserSettings::default()).expect("session");
    session.set_channel(1, Some(true), None, None).expect("join");
    session
        .set_channel(1, Some(true), None, None)
        .expect("join again");
    session.mute_sender(4, true);
    session.block_account("acct", "", true);
    session.set_channel(1, Some(false), None, None).expect("leave");
    assert_eq!(
        session.take_control(),
        vec![
            VoiceControl::Join(1),
            VoiceControl::MuteHint {
                sender: 4,
                muted: true
            },
            VoiceControl::Block {
                account: "acct".into(),
                blocked: true
            },
            VoiceControl::Leave(1),
        ]
    );
    assert!(session.take_control().is_empty());
}

/// Spec 59 § 2.13: a local mute is per account and also sends a `mute_hint` so the server stops
/// forwarding that speaker. Before ADR-5642 an account mute dropped packets locally but sent no hint.
#[test]
fn an_account_mute_hints_every_bound_slot_once_per_change() {
    let mut session = VoiceChatSession::new(settings(), VoiceChatUserSettings::default()).expect("session");
    session.bind_sender(4, "acct");
    session.bind_sender(5, "other");
    assert!(session.take_control().is_empty(), "binding alone sends nothing");
    session.mute_account("acct", "", true);
    assert!(session.is_sender_silenced(4));
    assert_eq!(
        session.take_control(),
        vec![VoiceControl::MuteHint {
            sender: 4,
            muted: true
        }]
    );
    // The account's next slot (a rejoin) is hinted when it is bound; a repeat bind is quiet.
    session.bind_sender(9, "acct");
    session.bind_sender(9, "acct");
    assert_eq!(
        session.take_control(),
        vec![VoiceControl::MuteHint {
            sender: 9,
            muted: true
        }]
    );
    // Unmuting the slot alone keeps the hint: the account is still muted.
    assert_eq!(session.mute_sender(4, false), (4, true));
    assert_eq!(
        session.take_control(),
        vec![VoiceControl::MuteHint {
            sender: 4,
            muted: true
        }]
    );
    // A slot muted on its own stays hinted when the account is unmuted.
    session.mute_sender(9, true);
    session.take_control();
    session.mute_account("acct", "", false);
    assert_eq!(
        session.take_control(),
        vec![VoiceControl::MuteHint {
            sender: 4,
            muted: false
        }]
    );
    assert!(!session.is_sender_silenced(4));
    assert!(session.is_sender_silenced(9));
    // A reused slot that now belongs to a muted account is hinted; leaving it un-hints nothing.
    session.mute_account("other", "", true);
    assert_eq!(
        session.take_control(),
        vec![VoiceControl::MuteHint {
            sender: 5,
            muted: true
        }]
    );
    session.bind_sender(5, "acct");
    assert_eq!(
        session.take_control(),
        vec![VoiceControl::MuteHint {
            sender: 5,
            muted: false
        }]
    );
}

#[test]
fn adaptive_bit_rate_stays_between_floor_and_target() {
    let mut session = VoiceChatSession::new(settings(), VoiceChatUserSettings::default()).expect("session");
    let target = VoiceQuality::Narrow.target_bps();
    session.set_link_quality(VoiceLinkQuality {
        loss_pct: 20.0,
        rtt_ms: 80.0,
    });
    for _ in 0..4000 {
        session.tick(false);
    }
    assert_eq!(session.status().bitrate_bps, target / 5 * 3, "the 60 % floor");
    session.set_link_quality(VoiceLinkQuality {
        loss_pct: 0.0,
        rtt_ms: 80.0,
    });
    for _ in 0..4000 {
        session.tick(false);
    }
    assert_eq!(session.status().bitrate_bps, target);
}

#[test]
fn a_profile_change_waits_for_the_end_of_the_talk_spurt() {
    let mic = FakeMicrophone::new(16_000, 1);
    let mut session = session_with(&mic, consenting());
    session.enable_microphone().expect("enable");
    let mut index = 0;
    mic.push(&sine_frame(&mut index));
    session.tick(false);
    session.set_quality(VoiceQuality::Standard);
    mic.push(&sine_frame(&mut index));
    session.tick(false);
    assert_eq!(session.status().quality, "narrow", "not mid-talk");
    session.set_mode(VoiceMode::PushToTalk);
    for _ in 0..20 {
        mic.push(&sine_frame(&mut index));
        session.tick(false);
    }
    assert_eq!(session.status().quality, "standard");
    assert_eq!(session.status().rate, 24_000);
}

// ----- the spatial path ------------------------------------------------------------------------

fn listener_facing_forward() -> ListenerFrame {
    ListenerFrame::looking_at(Vec3::ZERO, Vec3::NEG_Z)
}

fn levels(stereo: &[f32]) -> (f32, f32) {
    let (mut left, mut right, mut n) = (0.0f64, 0.0f64, 0.0f64);
    for pair in stereo.as_chunks::<2>().0 {
        left += f64::from(pair[0]) * f64::from(pair[0]);
        right += f64::from(pair[1]) * f64::from(pair[1]);
        n += 1.0;
    }
    (
        (left / n.max(1.0)).sqrt() as f32,
        (right / n.max(1.0)).sqrt() as f32,
    )
}

fn render_at(position: Vec3, frames: usize) -> (f32, f32) {
    let params = voice_emitter_params(&VoiceChatEmitter::default(), 25.0);
    let frame = regia_audio::EmitterFrame {
        position,
        ..Default::default()
    };
    let placement = voice_placement(&listener_facing_forward(), &frame, &params, 1.0, None);
    let mut mixer = SpeakerMixer::new(16_000);
    let mut index = 0;
    let mut out = Vec::new();
    for _ in 0..frames {
        out.clear();
        mixer.render(&sine_frame(&mut index), placement, &mut out);
    }
    levels(&out)
}

#[test]
fn the_chat_emitter_uses_the_spec_defaults() {
    let params = voice_emitter_params(&VoiceChatEmitter::default(), 25.0);
    assert_eq!((params.min_distance, params.max_distance), (2.0, 25.0));
    assert_eq!(params.rolloff, RolloffCurve::Log { db_per_doubling: 6.0 });
    assert_eq!(params.doppler_factor, 0.0);
    assert_eq!(params.bus.as_deref(), Some("Voice"));
    assert!(params.occlusion);
}

/// Spec 59 § 2.12, the defaults the test above does not pin: the cone is off (360 degrees,
/// outer gain 1), air absorption is off by default, and the max distance is the channel radius
/// unless the emitter names its own.
#[test]
fn the_chat_emitter_has_no_cone_no_air_absorption_and_the_radius_as_max() {
    let params = voice_emitter_params(&VoiceChatEmitter::default(), 25.0);
    assert!(!params.air_absorption);
    assert_eq!(
        (
            params.cone_inner_degrees,
            params.cone_outer_degrees,
            params.cone_outer_gain
        ),
        (360.0, 360.0, 1.0)
    );
    assert_eq!(
        voice_emitter_params(&VoiceChatEmitter::default(), 40.0).max_distance,
        40.0
    );
    let own = VoiceChatEmitter {
        max_distance: Some(10.0),
        air_absorption: true,
        ..VoiceChatEmitter::default()
    };
    let params = voice_emitter_params(&own, 25.0);
    assert_eq!(params.max_distance, 10.0);
    assert!(params.air_absorption);
}

#[test]
fn near_left_is_louder_and_left_of_far_right_by_the_rolloff() {
    let (near_l, near_r) = render_at(Vec3::new(-2.0, 0.0, 0.0), 10);
    let (far_l, far_r) = render_at(Vec3::new(20.0, 0.0, 0.0), 10);
    assert!(near_l > near_r * 10.0, "left panned: {near_l} {near_r}");
    assert!(far_r > far_l * 10.0, "right panned: {far_l} {far_r}");
    // 2 m to 20 m is log2(10) doublings at 6 dB each.
    let expected = 6.0 * 10f32.log2();
    let measured = 20.0 * (near_l / far_r).log10();
    assert!((measured - expected).abs() < 1.0, "{measured} vs {expected}");
}

#[test]
fn out_of_the_radius_is_silent() {
    let (left, right) = render_at(Vec3::new(30.0, 0.0, 0.0), 10);
    assert!(left.max(right) < 1e-6, "{left} {right}");
}

#[test]
fn an_unresolved_speaker_plays_flat_at_minus_6_db_then_crossfades() {
    let mut mixer = SpeakerMixer::new(16_000);
    let mut index = 0;
    let mut out = Vec::new();
    mixer.render(&sine_frame(&mut index), VoicePlacement::Unresolved, &mut out);
    let flat = 10f32.powf(FLAT_UNRESOLVED_DB / 20.0);
    assert!((mixer.gains()[0] - flat).abs() < 1e-5);
    assert!((mixer.gains()[0] - mixer.gains()[1]).abs() < 1e-6);
    let spatial = VoicePlacement::Spatial {
        gain: 1.0,
        pan: -1.0,
        cutoff_hz: None,
    };
    let target = std::f32::consts::SQRT_2;
    let mut seen = Vec::new();
    for _ in 0..SPATIAL_CROSSFADE_FRAMES {
        out.clear();
        mixer.render(&sine_frame(&mut index), spatial, &mut out);
        seen.push(mixer.gains()[0]);
    }
    assert!(seen.windows(2).all(|w| w[1] > w[0]), "{seen:?}");
    assert!(
        seen[0] < target * 0.9,
        "the first frame is not there yet: {seen:?}"
    );
    assert!((seen[4] - target).abs() < 1e-3, "{seen:?}");
    assert!(mixer.gains()[1].abs() < 1e-3);
}

#[test]
fn gains_never_jump_inside_a_frame() {
    let mut mixer = SpeakerMixer::new(16_000);
    let mut out = Vec::new();
    let constant = vec![0.5f32; FRAME];
    mixer.render(&constant, VoicePlacement::Center, &mut out);
    out.clear();
    mixer.render(
        &constant,
        VoicePlacement::Spatial {
            gain: 0.0,
            pan: 0.0,
            cutoff_hz: None,
        },
        &mut out,
    );
    let left: Vec<f32> = out.iter().step_by(2).copied().collect();
    let biggest = left.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
    assert!(biggest < 0.01, "{biggest}");
}

#[test]
fn a_low_cutoff_muffles_high_speech_energy() {
    let params = |cutoff_hz| VoicePlacement::Spatial {
        gain: 1.0,
        pan: 0.0,
        cutoff_hz,
    };
    let tone: Vec<f32> = (0..FRAME * 10)
        .map(|i| (std::f32::consts::TAU * 6000.0 * i as f32 / 16_000.0).sin() * 0.25)
        .collect();
    let (mut open, mut muffled) = (Vec::new(), Vec::new());
    SpeakerMixer::new(16_000).render(&tone, params(None), &mut open);
    SpeakerMixer::new(16_000).render(&tone, params(Some(500.0)), &mut muffled);
    assert!(levels(&muffled).0 < levels(&open).0 * 0.3);
}

// ----- the plugin ------------------------------------------------------------------------------

fn voice_app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    app.insert_resource(VoiceChatSettings {
        quality: VoiceQuality::Narrow,
        deterministic: true,
        ..VoiceChatSettings::default()
    });
    app.add_plugins((AudioPlugin, VoiceChatPlugin { ticks_per_second: 50 }));
    {
        let session = app.world().resource::<VoiceChatResource>();
        session
            .0
            .lock()
            .expect("lock")
            .set_channel(2, Some(true), Some(true), None)
            .expect("join");
    }
    app
}

fn set_fake(app: &App, sender: u16) {
    let session = app.world().resource::<VoiceChatResource>();
    session
        .0
        .lock()
        .expect("lock")
        .set_fake_speaker(sender, Some(TestSource::sine(440.0, -12.0)));
}

#[test]
fn a_speaker_without_an_entity_plays_flat_and_finds_its_emitter_later() {
    let mut app = voice_app();
    set_fake(&app, 7);
    for _ in 0..12 {
        app.update();
    }
    {
        let output = app.world().resource::<VoiceChatOutput>();
        assert_eq!(output.senders(), vec![7]);
        assert!(!output.is_placed(7));
        let flat = 10f32.powf(FLAT_UNRESOLVED_DB / 20.0);
        let gains = output.gains(7).expect("gains");
        assert!((gains[0] - flat).abs() < 1e-3, "{gains:?}");
        assert!(output.stats().frames > 0);
        assert_eq!(output.stats().streams_opened, 1);
    }
    let entity = app
        .world_mut()
        .spawn((
            VoiceChatEmitter {
                sender: 7,
                ..VoiceChatEmitter::default()
            },
            GlobalTransform(Affine3A::from_translation(Vec3::new(-2.0, 0.0, 0.0))),
        ))
        .id();
    for _ in 0..10 {
        app.update();
    }
    let output = app.world().resource::<VoiceChatOutput>();
    assert!(output.is_placed(7));
    let gains = output.gains(7).expect("gains");
    assert!(gains[0] > gains[1] * 5.0, "panned left: {gains:?}");
    let marker = app.world().get::<VoiceChatSpeaking>(entity).expect("speaking");
    assert!(marker.level > 0.0);
}

#[test]
fn the_speaking_marker_follows_the_talk_spurt() {
    let mut app = voice_app();
    let entity = app
        .world_mut()
        .spawn((
            VoiceChatEmitter {
                sender: 3,
                ..VoiceChatEmitter::default()
            },
            GlobalTransform::IDENTITY,
        ))
        .id();
    set_fake(&app, 3);
    for _ in 0..12 {
        app.update();
    }
    assert!(app.world().get::<VoiceChatSpeaking>(entity).is_some());
    app.world()
        .resource::<VoiceChatResource>()
        .0
        .lock()
        .expect("lock")
        .set_fake_speaker(3, None);
    // One second to time the speaker out, one more to release its stream.
    for _ in 0..120 {
        app.update();
    }
    assert!(app.world().get::<VoiceChatSpeaking>(entity).is_none());
    assert!(
        app.world().resource::<VoiceChatOutput>().senders().is_empty(),
        "the stream is released after a second of silence"
    );
}

#[test]
fn a_flat_channel_stays_centered() {
    let mut app = voice_app();
    app.world()
        .resource::<VoiceChatResource>()
        .0
        .lock()
        .expect("lock")
        .set_channel(1, Some(true), Some(true), None)
        .expect("team");
    set_fake(&app, 4);
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<VoiceChatOutput>().gains(4),
        Some([1.0, 1.0])
    );
}

#[test]
fn the_voice_bus_carries_the_speaker_in_a_real_mix() {
    let mut app = voice_app();
    set_fake(&app, 7);
    let mut peak = 0.0f32;
    for _ in 0..30 {
        app.update();
        let mut audio = app.world_mut().resource_mut::<Audio>();
        for s in audio.render_offline(960).expect("offline render") {
            peak = peak.max(s.abs());
        }
    }
    assert!(peak > 0.01, "the tone reached the mix: {peak}");
}

#[test]
fn speaker_occlusion_rays_reach_the_provider_and_its_answer_quietens_the_voice() {
    let mut app = voice_app();
    app.init_resource::<AudioOcclusion>();
    let entity = app
        .world_mut()
        .spawn((
            VoiceChatEmitter {
                sender: 7,
                ..VoiceChatEmitter::default()
            },
            GlobalTransform(Affine3A::from_translation(Vec3::new(4.0, 0.0, 0.0))),
        ))
        .id();
    app.world_mut()
        .spawn((AudioListener::default(), GlobalTransform::IDENTITY));
    set_fake(&app, 7);
    for _ in 0..12 {
        app.update();
    }
    let open = app.world().resource::<VoiceChatOutput>().gains(7).expect("gains")[1];
    {
        let mut occlusion = app.world_mut().resource_mut::<AudioOcclusion>();
        assert!(
            occlusion.pending().iter().any(|r| r.emitter == entity),
            "the ray is queued"
        );
        occlusion.report(entity, 1.0);
    }
    for _ in 0..80 {
        app.update();
        app.world_mut()
            .resource_mut::<AudioOcclusion>()
            .report(entity, 1.0);
    }
    let shut = app.world().resource::<VoiceChatOutput>().gains(7).expect("gains")[1];
    assert!(shut < open * 0.8, "{shut} vs {open}");
}

#[test]
fn the_talk_action_and_the_resource_both_drive_push_to_talk() {
    assert_eq!(VOICE_TALK_ACTION, "voice_chat_talk");
    let mut app = voice_app();
    app.add_systems(PostUpdate, || {});
    {
        let session = app.world().resource::<VoiceChatResource>();
        session
            .0
            .lock()
            .expect("lock")
            .set_test_source(Some(TestSource::sine(440.0, -12.0)));
    }
    for _ in 0..5 {
        app.update();
    }
    let sent = |app: &App| {
        let session = app.world().resource::<VoiceChatResource>();
        let stats = session.0.lock().expect("lock").stats();
        stats.frames_encoded
    };
    assert_eq!(sent(&app), 0, "push to talk, nothing pressed");
    app.world_mut().resource_mut::<VoiceChatTalk>().0 = true;
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(sent(&app), 5);
}

#[test]
fn user_settings_ride_in_the_audio_user_settings_file() {
    let mut settings = regia_audio::AudioUserSettings::default();
    settings.voice_chat.consent = true;
    settings.voice_chat.mode = VoiceMode::Vad;
    settings.voice_chat.input_gain_db = -3.0;
    settings.voice_chat.muted_accounts.add("a", "note");
    let text = ron::ser::to_string(&settings).expect("serializes");
    let back: regia_audio::AudioUserSettings = ron::de::from_str(&text).expect("parses");
    assert_eq!(back, settings);
    let old: regia_audio::AudioUserSettings =
        ron::de::from_str("(bus_volumes: {}, muted: [])").expect("an older file still loads");
    assert!(!old.voice_chat.consent);
    assert_eq!(old.voice_chat.mode, VoiceMode::PushToTalk);
}

// ----- time scaling ----------------------------------------------------------------------------

fn tone(rate: u32, hz: f32, n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin() * 0.3)
        .collect()
}

fn biggest_step(x: &[f32]) -> f32 {
    x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
}

#[test]
fn shortening_removes_a_pitch_period_without_a_click() {
    for hz in [120.0f32, 200.0, 310.0] {
        let original = tone(16_000, hz, 320);
        let mut x = original.clone();
        let removed = shorten(&mut x, 16_000);
        assert_eq!(x.len(), 320 - removed);
        let period = 16_000.0 / hz;
        let cycles = removed as f32 / period;
        assert!(
            (cycles - cycles.round()).abs() < 0.1,
            "{hz} Hz: removed {removed}"
        );
        assert!(biggest_step(&x) <= biggest_step(&original) * 1.3, "{hz} Hz");
    }
}

#[test]
fn lengthening_adds_a_pitch_period_without_a_click() {
    for hz in [120.0f32, 200.0, 310.0] {
        let original = tone(16_000, hz, 320);
        let mut x = original.clone();
        let added = lengthen(&mut x, 16_000);
        assert_eq!(x.len(), 320 + added);
        let cycles = added as f32 * hz / 16_000.0;
        assert!((cycles - cycles.round()).abs() < 0.1, "{hz} Hz: added {added}");
        assert!(biggest_step(&x) <= biggest_step(&original) * 1.3, "{hz} Hz");
    }
}

#[test]
fn a_frame_too_short_for_two_periods_is_left_alone() {
    let mut x = vec![0.1f32; 100];
    assert_eq!(shorten(&mut x, 16_000), 0);
    assert_eq!(lengthen(&mut x, 16_000), 0);
    assert_eq!(x.len(), 100);
}

fn pcm_packet(codec: &mut PcmCodec, seq: u16, samples: &[f32]) -> Vec<u8> {
    let mut payload = vec![0u8; 1 + 2 * samples.len()];
    let n = codec.encode(samples, &mut payload).expect("encodes");
    let packet = VoicePacket {
        flags: VoiceFlags::default(),
        sender: 1,
        channel: 1,
        seq,
        timestamp: u32::from(seq).wrapping_mul(FRAME_TIMESTAMP_STEP),
        quality: VoiceQuality::Narrow,
        codec: VoiceCodecId::Pcm,
        payload: &payload[..n],
    };
    let mut wire = Vec::new();
    packet.write(&mut wire);
    wire
}

/// A sender whose clock runs `ppt` parts per thousand off the listener's: one packet per tick plus
/// or minus an occasional one.
fn run_drift(
    scaling: bool,
    extra_every: Option<u64>,
    skip_every: Option<u64>,
) -> (SpeakerStats, usize, usize) {
    let mut speaker = VoiceSpeaker::new(1, JitterSettings::default());
    speaker.set_time_scaling(scaling);
    let mut codec = PcmCodec::new(VoiceQuality::Narrow);
    let source = tone(16_000, 180.0, 320 * 3000);
    let mut seq = 0u16;
    let mut next = 0usize;
    let mut deepest = 0;
    let mut send = |speaker: &mut VoiceSpeaker, seq: &mut u16, frame: u64| {
        let samples = &source[next * 320..(next + 1) * 320];
        next += 1;
        let wire = pcm_packet(&mut codec, *seq, samples);
        speaker.receive(&VoicePacket::parse(&wire).expect("parses"), frame);
        *seq = seq.wrapping_add(1);
    };
    let mut emitted = 0;
    for frame in 1..=2000u64 {
        if !skip_every.is_some_and(|n| frame.is_multiple_of(n)) {
            send(&mut speaker, &mut seq, frame);
        }
        if extra_every.is_some_and(|n| frame.is_multiple_of(n)) {
            send(&mut speaker, &mut seq, frame);
        }
        emitted += speaker.pull_frame(frame).len();
        deepest = deepest.max(speaker.depth_frames());
    }
    (speaker.stats(), deepest, emitted)
}

#[test]
fn a_fast_sender_is_absorbed_by_shortening_not_by_dropping_frames() {
    let (stats, deepest, emitted) = run_drift(true, Some(20), None);
    assert!(stats.shortened > 0, "{stats:?}");
    assert_eq!(stats.jitter.late + stats.jitter.resyncs, 0, "{stats:?}");
    assert!(deepest <= 6, "the depth stays near target: {deepest}");
    assert_eq!(emitted, 2000 * 320, "output keeps its clock");
    assert!(stats.drift_ppm > 0.0, "{stats:?}");
}

#[test]
fn without_time_scaling_the_same_sender_runs_the_buffer_up() {
    let (stats, deepest, _) = run_drift(false, Some(20), None);
    assert_eq!(stats.shortened, 0);
    assert!(deepest > 50, "{deepest}");
}

#[test]
fn a_slow_sender_is_absorbed_by_lengthening() {
    let (stats, _, emitted) = run_drift(true, None, Some(20));
    assert!(stats.lengthened > 0, "{stats:?}");
    assert_eq!(emitted, 2000 * 320);
    assert!(stats.drift_ppm < 0.0, "{stats:?}");
}

/// A sender on the listener's clock (one packet per tick) keeps the buffer where it is: no frame
/// is stretched or shortened, and the output keeps its clock. The scaling decision reads the
/// buffer depth before the tick's frame is taken; reading it after made an on-target buffer look
/// one frame short.
#[test]
fn an_on_clock_sender_is_never_scaled() {
    let (stats, _, emitted) = run_drift(true, None, None);
    assert_eq!((stats.shortened, stats.lengthened), (0, 0), "{stats:?}");
    assert_eq!(emitted, 2000 * 320);
}

#[test]
fn the_deterministic_session_never_scales() {
    let mut session = VoiceChatSession::new(settings(), VoiceChatUserSettings::default()).expect("session");
    session
        .set_channel(1, Some(true), Some(true), None)
        .expect("join");
    session.set_fake_speaker(5, Some(TestSource::sine(200.0, -12.0)));
    for _ in 0..200 {
        session.tick(false);
    }
    let (_, stats) = session.speaker_stats()[0];
    assert_eq!((stats.shortened, stats.lengthened), (0, 0));
}

// ----- two-frame packets, spare frames, comfort --------------------------------------------------

fn sender(settings: VoiceChatSettings) -> (VoiceChatSession, FakeMicrophone) {
    let mic = FakeMicrophone::new(16_000, 1);
    let mut session = VoiceChatSession::new(settings, consenting()).expect("session");
    session.set_microphone(Box::new(mic.clone()));
    session.set_mode(VoiceMode::Open);
    session
        .set_channel(1, Some(true), Some(true), None)
        .expect("join");
    session.enable_microphone().expect("enable");
    (session, mic)
}

fn listener() -> VoiceChatSession {
    let mut session = VoiceChatSession::new(settings(), VoiceChatUserSettings::default()).expect("session");
    session.set_channel(1, Some(true), None, None).expect("join");
    session
}

#[test]
fn multi_payloads_round_trip_and_bad_ones_are_refused() {
    let mut out = Vec::new();
    write_multi(&[1, 2], &[3, 4, 5], false, &mut out);
    let parsed = parse_multi(&out).expect("parses");
    assert_eq!(
        (parsed.first, parsed.second, parsed.redundant),
        (&[1u8, 2][..], &[3u8, 4, 5][..], false)
    );
    out.clear();
    write_multi(&[], &[9], true, &mut out);
    assert!(parse_multi(&out).expect("parses").redundant);
    assert!(parse_multi(&[2, 0]).is_err(), "short");
    assert!(parse_multi(&[3, 0, 0]).is_err(), "only two frames exist");
    assert!(parse_multi(&[2, 9, 0, 1]).is_err(), "length past the end");
}

#[test]
fn two_frame_packets_halve_the_packet_rate_and_play_every_frame() {
    let mut tx_settings = settings();
    tx_settings.frames_per_packet = 2;
    let (mut tx, mic) = sender(tx_settings);
    tx.set_local_sender(3);
    let mut rx = listener();
    let mut index = 0;
    let mut packets = 0;
    for _ in 0..100 {
        mic.push(&sine_frame(&mut index));
        tx.tick(false);
        for p in tx.take_outgoing() {
            packets += 1;
            let parsed = VoicePacket::parse(&p).expect("parses");
            assert!(parsed.flags.contains(VoiceFlags::MULTI));
            rx.receive_packet(&p);
        }
        rx.tick(false);
    }
    assert_eq!(packets, 50);
    assert_eq!(tx.stats().frames_encoded, 100);
    let (_, stats) = rx.speaker_stats()[0];
    assert_eq!(stats.unsupported, 0);
    assert!(stats.jitter.played >= 90, "{stats:?}");
    assert_eq!(stats.jitter.concealed, 0, "{stats:?}");
}

#[test]
fn mute_flushes_a_held_frame_before_the_end_of_talk() {
    let mut tx_settings = settings();
    tx_settings.frames_per_packet = 2;
    let (mut tx, mic) = sender(tx_settings);
    let mut index = 0;
    mic.push(&sine_frame(&mut index));
    tx.tick(false);
    assert!(tx.take_outgoing().is_empty(), "held");
    tx.set_self_muted(true);
    let out = tx.take_outgoing();
    assert_eq!(out.len(), 2);
    let (first, last) = (
        VoicePacket::parse(&out[0]).expect("parses"),
        VoicePacket::parse(&out[1]).expect("parses"),
    );
    assert_eq!(last.seq, first.seq.wrapping_add(1));
    assert!(last.flags.contains(VoiceFlags::END_OF_TALK));
}

#[test]
fn a_spare_frame_fills_a_lost_packets_hole() {
    let mut tx_settings = settings();
    tx_settings.redundancy = true;
    let (mut tx, mic) = sender(tx_settings);
    tx.set_link_quality(VoiceLinkQuality {
        loss_pct: 20.0,
        rtt_ms: 40.0,
    });
    tx.set_local_sender(3);
    let mut rx = listener();
    let mut index = 0;
    for frame in 0..60 {
        mic.push(&sine_frame(&mut index));
        tx.tick(false);
        for p in tx.take_outgoing() {
            // Every tenth packet is lost.
            if frame % 10 != 9 {
                rx.receive_packet(&p);
            }
        }
        rx.tick(false);
    }
    let (_, stats) = rx.speaker_stats()[0];
    assert!(stats.jitter.spare_filled >= 3, "{stats:?}");
    assert_eq!(stats.jitter.concealed + stats.jitter.recovered, 0, "{stats:?}");
    assert_eq!(
        stats.jitter.duplicates, 0,
        "a spare copy is not a duplicate: {stats:?}"
    );
}

#[test]
fn without_measured_loss_redundancy_stays_off() {
    let mut tx_settings = settings();
    tx_settings.redundancy = true;
    let (mut tx, mic) = sender(tx_settings);
    let mut index = 0;
    for _ in 0..10 {
        mic.push(&sine_frame(&mut index));
        tx.tick(false);
    }
    for p in tx.take_outgoing() {
        assert!(
            !VoicePacket::parse(&p)
                .expect("parses")
                .flags
                .contains(VoiceFlags::MULTI)
        );
    }
}

#[test]
fn a_silent_open_microphone_sends_only_comfort_packets() {
    let (mut tx, mic) = sender(settings());
    for _ in 0..100 {
        mic.push(&[0.0; FRAME]);
        tx.tick(false);
    }
    let out = tx.take_outgoing();
    assert_eq!(out.len(), 5, "one per 400 ms");
    for p in &out {
        assert_eq!(
            VoicePacket::parse(p).expect("parses").payload.len(),
            1,
            "PCM's comfort byte"
        );
    }
    assert_eq!(tx.stats().frames_encoded, 0);
    // Speech goes straight through again.
    let mut index = 0;
    mic.push(&sine_frame(&mut index));
    tx.tick(false);
    assert_eq!(tx.stats().frames_encoded, 1);
}

#[test]
fn comfort_packets_keep_a_silent_open_speaker_alive_and_speech_resumes() {
    let (mut tx, mic) = sender(settings());
    tx.set_local_sender(3);
    let mut rx = listener();
    let mut index = 0;
    let step = |tx: &mut VoiceChatSession, rx: &mut VoiceChatSession, samples: &[f32]| {
        mic.push(samples);
        tx.tick(false);
        for p in tx.take_outgoing() {
            rx.receive_packet(&p);
        }
        rx.tick(false);
    };
    for _ in 0..20 {
        let frame = sine_frame(&mut index);
        step(&mut tx, &mut rx, &frame);
    }
    for _ in 0..200 {
        step(&mut tx, &mut rx, &[0.0; FRAME]);
    }
    let (_, stats) = rx.speaker_stats()[0];
    assert_eq!(stats.timeouts, 0, "{stats:?}");
    for _ in 0..20 {
        let frame = sine_frame(&mut index);
        step(&mut tx, &mut rx, &frame);
    }
    assert!(rx.speaker(3).expect("speaker").level_db() > -30.0);
}

/// A codec that claims more bytes than the payload buffer holds, for every call.
struct OversizeCodec;

impl VoiceCodec for OversizeCodec {
    fn encode(&mut self, _frame: &[f32], _out: &mut [u8]) -> Result<usize, VoiceChatError> {
        Ok(1 << 20)
    }

    fn decode(
        &mut self,
        _packet: Option<&[u8]>,
        _fec: bool,
        out: &mut [f32],
    ) -> Result<usize, VoiceChatError> {
        out.fill(0.0);
        Ok(out.len())
    }

    fn encode_comfort(&mut self, _out: &mut [u8]) -> Result<usize, VoiceChatError> {
        Ok(1 << 20)
    }
}

struct OversizeProvider;

impl VoiceCodecProvider for OversizeProvider {
    fn preferred(&self) -> VoiceCodecId {
        VoiceCodecId::Opus
    }

    fn build(
        &self,
        _id: VoiceCodecId,
        _quality: VoiceQuality,
        _complexity: u8,
    ) -> Result<Box<dyn VoiceCodec + Send>, VoiceChatError> {
        Ok(Box::new(OversizeCodec))
    }
}

#[test]
fn a_codec_claiming_more_bytes_than_the_buffer_does_not_panic() {
    let mut session = VoiceChatSession::new(settings(), VoiceChatUserSettings::default()).expect("session");
    session
        .set_codec_provider(Arc::new(OversizeProvider))
        .expect("provider");
    session
        .set_channel(2, Some(true), Some(true), None)
        .expect("join");
    session.set_fake_speaker(7, Some(TestSource::sine(440.0, -12.0)));
    for _ in 0..20 {
        session.tick(false);
    }
}

/// A Voice-bus `play_media_feed` raises the `"Voice"` duck (Music −6 dB, attack 0.2 s, release
/// 1.0 s). An Sfx-bus feed does not. A paused Voice feed still counts; stopping it releases the
/// duck (specs/16 § 1, specs/59 voice playback).
#[test]
fn remote_voice_chat_ducks_music() {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    let music_offset = |audio: &Audio| {
        audio
            .bus_status()
            .into_iter()
            .find(|bus| bus.name == "Music")
            .and_then(|bus| bus.applied_offset_db())
            .expect("Music has an applied level")
    };
    let rate = 48_000u32;
    let (mut sfx_in, sfx) =
        regia_audio::media_feed::PcmFeed::channel(1, 256).expect("mono is a valid channel count");
    assert_eq!(sfx_in.push(&[0.1; 64]), 64);
    let (_sfx_handle, sfx_bus) = audio.play_media_feed("Sfx", sfx, rate, 1.0).expect("sfx feed");
    assert_eq!(sfx_bus, "Sfx");
    for _ in 0..12 {
        audio.tick(0.02);
    }
    assert!(
        music_offset(&audio).abs() < 1e-3,
        "an Sfx feed does not duck Music: {}",
        music_offset(&audio)
    );

    let (mut voice_in, voice) =
        regia_audio::media_feed::PcmFeed::channel(1, 256).expect("mono is a valid channel count");
    assert_eq!(voice_in.push(&[0.1; 64]), 64);
    let (voice_handle, voice_bus) = audio
        .play_media_feed("Voice", voice, rate, 1.0)
        .expect("voice feed");
    assert_eq!(voice_bus, "Voice");
    voice_handle.set_paused(true);
    for _ in 0..12 {
        audio.tick(0.02);
    }
    let ducked = music_offset(&audio);
    assert!(
        (ducked + 6.0).abs() < 1e-3,
        "a paused Voice-bus feed ducks Music by 6 dB, got {ducked}"
    );

    voice_handle.stop();
    for _ in 0..60 {
        audio.tick(0.02);
    }
    let released = music_offset(&audio);
    assert!(
        released.abs() < 1e-3,
        "stopping the Voice feed releases the duck, got {released}"
    );
}
