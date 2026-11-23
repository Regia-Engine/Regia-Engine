// SPDX-License-Identifier: MPL-2.0
//! Video in the UI (specs/50 § 2.7, ADR-2044): [`UiVideo`] shows a video player's current picture
//! in a node, and [`UiVideoControl`] buttons (play, pause, toggle, loop, skip) and a seek [`Slider`] bound
//! to that node drive the player. Pictures come from `regia_render::VideoFrames` (the media crate
//! installs the source), so `regia_ui` never names the media crate.
//!
//! Each frame [`sync_ui_video`] asks the source for the player's picture; when its serial changed
//! it queues the RGBA8 picture on the UI renderer under the node's image id and writes that id
//! into the node's [`UiImage`] (aspect kept by `preserve_aspect`). While the picture's size stays
//! the same, that id keeps one GPU texture: later pictures rewrite mip 0 and do not rebuild the
//! draw list. A seek slider writes its value without marking the slider changed, and extract moves
//! the thumb on the clean path. Transport input becomes a
//! `VideoControl` queued on the source; the media crate applies it before the next clock tick,
//! so a click never blocks and a player on the audio clock stays in sync with the sound.

use regia_app::prelude::*;
use regia_render::{VideoControl, VideoFrames, VideoSystems};

use crate::interaction::{UiEvent, UiEventKind, UiValue};
use crate::node::UiImage;
use crate::render::SharedUiRenderer;
use crate::widgets::Slider;

/// Image ids of video nodes: this base with the node entity's low 32 bits (reserved for video).
pub const VIDEO_IMAGE_BASE: u64 = 0x5649_4400_0000_0000;

/// Shows a video player's picture in this node.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct UiVideo {
    /// The player (its entity bits).
    pub player: u64,
    /// Keep the clip's aspect inside the node (letterbox) instead of stretching.
    pub keep_aspect: bool,
}

impl UiVideo {
    /// A node showing `player`, aspect kept.
    #[must_use]
    pub fn new(player: u64) -> Self {
        Self {
            player,
            keep_aspect: true,
        }
    }
}

/// What the node shows now (read by tests, agents and HUD code).
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct UiVideoShown {
    /// Image id registered with the UI renderer.
    pub image: u64,
    /// Picture serial last queued.
    pub serial: u64,
    /// Presentation time of the shown picture in nanoseconds (`None` for the fallback frame).
    pub pts_ns: Option<i64>,
    /// The fallback frame shows.
    pub fallback: bool,
    /// Pictures queued to the UI renderer so far.
    pub uploads: u64,
}

/// What a transport widget does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UiVideoAction {
    /// Play.
    Play,
    /// Pause.
    Pause,
    /// Play or pause.
    Toggle,
    /// A seek slider: its value (`0..=1`) is a fraction of the clip's duration.
    SeekSlider,
    /// Loop on or off (a `Toggle` widget's value, or flip on click).
    Loop,
    /// Skip the video (closes the player; a fullscreen player marked not skippable refuses).
    Skip,
}

/// Binds a widget (button, toggle or slider) to the video node `video`.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct UiVideoControl {
    /// The node holding the [`UiVideo`].
    pub video: Entity,
    /// What it does.
    pub action: UiVideoAction,
}

/// Counters of the transport widgets (refused controls are kept with their code).
#[derive(Resource, Debug, Default, Clone, PartialEq)]
pub struct UiVideoStats {
    /// Controls queued on the source.
    pub sent: u64,
    /// Controls the source refused, with the last code.
    pub refused: u64,
    /// Last refusal code (`player_not_found`, `control_queue_full`, `no_video_source`).
    pub last_error: Option<String>,
}

/// Queued seeks a seek slider is still waiting for: maps the slider's entity to the
/// fraction queued on the source. While an entry exists, [`ui_video_controls`] leaves the
/// slider at the value the widget set; the entry drops once the source reports a position
/// near the target (the seek was applied), a newer seek overwrites it, the slider despawns, or
/// the source has not reached the target within `SEEK_WAIT_FRAMES` frames (a failed seek, or a
/// source that lands elsewhere): the slider then follows playback again.
#[derive(Resource, Debug, Default)]
pub struct UiVideoPendingSeeks {
    /// Slider entity -> (queued seek fraction `0..=1`, frames waited).
    pending: std::collections::HashMap<Entity, (f32, u32)>,
}

/// How near the reported position fraction must be to a queued seek target before the
/// seek counts as applied and the slider resumes following the playback position.
const SEEK_APPLIED_EPS: f32 = 0.05;

/// How many frames a seek slider waits for its queued seek before it follows playback again.
const SEEK_WAIT_FRAMES: u32 = 120;

/// Adds [`sync_ui_video`] and [`ui_video_controls`]. Off unless added; does nothing without an
/// installed `VideoFrames` source.
#[derive(Debug, Default)]
pub struct UiVideoPlugin;

impl Plugin for UiVideoPlugin {
    fn build(&self, app: &mut App) {
        // `ui_video_controls` reads the pointer. `UiPlugin` also inits it; a host that adds only this
        // plugin (the media crate's tests do) must not panic on a missing resource.
        app.init_resource::<UiVideoStats>()
            .init_resource::<UiVideoPendingSeeks>()
            .init_resource::<crate::interaction::UiPointer>()
            .add_systems(PreUpdate, ui_video_controls)
            .add_systems(PostUpdate, sync_ui_video.after(VideoSystems::Publish));
    }
}

/// `PostUpdate`: copies each [`UiVideo`] node's current picture to the UI renderer when it changed.
pub fn sync_ui_video(
    frames: Option<Res<VideoFrames>>,
    renderer: Option<Res<SharedUiRenderer>>,
    mut nodes: Query<(Entity, &UiVideo, Option<&mut UiImage>, Option<&mut UiVideoShown>)>,
    mut commands: Commands,
) {
    let Some(source) = frames.as_deref().and_then(VideoFrames::source) else {
        return;
    };
    let _span = tracing::trace_span!("ui_video_sync").entered();
    for (entity, video, image, shown) in &mut nodes {
        let id = VIDEO_IMAGE_BASE | (entity.to_bits() & 0xFFFF_FFFF);
        let Some(info) = source.frame(video.player) else {
            continue;
        };
        let mut now = shown.as_deref().copied().unwrap_or_default();
        now.image = id;
        if now.serial != info.serial || now.uploads == 0 {
            if let (Some(renderer), Some((serial, w, h, rgba))) =
                (renderer.as_deref(), source.rgba(video.player))
            {
                renderer.lock().queue_dynamic_image(id, w, h, rgba);
                now.uploads += 1;
                now.serial = serial;
            } else {
                now.serial = info.serial;
            }
            now.pts_ns = info.pts_ns;
            now.fallback = info.fallback;
        }
        match image {
            Some(mut img) => {
                if img.id != id || img.preserve_aspect != video.keep_aspect {
                    img.id = id;
                    img.preserve_aspect = video.keep_aspect;
                }
            }
            None => {
                commands.entity(entity).insert(UiImage {
                    id,
                    preserve_aspect: video.keep_aspect,
                    ..UiImage::default()
                });
            }
        }
        match shown {
            Some(mut s) => {
                if *s != now {
                    *s = now;
                }
            }
            None => {
                commands.entity(entity).insert(now);
            }
        }
    }
}

/// The control a transport widget's event asks for, if any. `duration` is the clip length in
/// seconds (for a seek slider), `playing` and `looping` the player's current state.
#[must_use]
pub fn control_for(
    action: UiVideoAction,
    kind: &UiEventKind,
    duration: Option<f64>,
    looping: bool,
) -> Option<VideoControl> {
    match (action, kind) {
        (UiVideoAction::Play, UiEventKind::Click) => Some(VideoControl::Play),
        (UiVideoAction::Pause, UiEventKind::Click) => Some(VideoControl::Pause),
        (UiVideoAction::Toggle, UiEventKind::Click) => Some(VideoControl::Toggle),
        (UiVideoAction::Skip, UiEventKind::Click) => Some(VideoControl::Skip),
        (UiVideoAction::Loop, UiEventKind::ValueChanged(UiValue::Bool(on))) => {
            Some(VideoControl::SetLoop(*on))
        }
        (UiVideoAction::Loop, UiEventKind::Click) => Some(VideoControl::SetLoop(!looping)),
        (UiVideoAction::SeekSlider, UiEventKind::ValueChanged(UiValue::Number(f))) => {
            let d = duration?;
            let fraction = if f.is_finite() {
                f64::from(f.clamp(0.0, 1.0))
            } else {
                0.0
            };
            Some(VideoControl::Seek(fraction * d))
        }
        _ => None,
    }
}

/// `PreUpdate`: turns transport widget events into controls queued on the source, and keeps
/// seek sliders on the playback position — except while the pointer holds the slider or a
/// queued seek is still in flight, where the widget's value wins.
pub fn ui_video_controls(
    frames: Option<Res<VideoFrames>>,
    mut events: MessageReader<UiEvent>,
    controls: Query<&UiVideoControl>,
    videos: Query<&UiVideo>,
    mut sliders: Query<(Entity, &UiVideoControl, &mut Slider)>,
    mut stats: ResMut<UiVideoStats>,
    // Optional: the plugin is usable without the UI interaction plugin (no pointer, nothing held).
    pointer: Option<Res<crate::interaction::UiPointer>>,
    mut pending: ResMut<UiVideoPendingSeeks>,
) {
    let source = frames.as_deref().and_then(VideoFrames::source);
    for event in events.read() {
        let Ok(control) = controls.get(event.target) else {
            continue;
        };
        let Ok(video) = videos.get(control.video) else {
            continue;
        };
        let Some(source) = source else {
            stats.refused += 1;
            stats.last_error = Some("no_video_source".into());
            continue;
        };
        let info = source.frame(video.player);
        let duration = info.as_ref().and_then(|i| i.duration);
        let looping = info.as_ref().is_some_and(|i| i.looping);
        let Some(c) = control_for(control.action, &event.kind, duration, looping) else {
            continue;
        };
        match source.control(video.player, c) {
            Ok(()) => {
                stats.sent += 1;
                // The seek lands later; until the playback position reflects it, the
                // slider must not be reset to the stale position (a seek slider's
                // `ValueChanged` always queues a `Seek`).
                if let UiEventKind::ValueChanged(UiValue::Number(f)) = &event.kind {
                    pending.pending.insert(event.target, (f.clamp(0.0, 1.0), 0));
                }
            }
            Err(e) => {
                stats.refused += 1;
                stats.last_error = Some(e.code().to_string());
            }
        }
    }
    let Some(source) = source else {
        return;
    };
    if !pending.pending.is_empty() {
        // A despawned slider's seek is never applied to it.
        pending.pending.retain(|e, _| sliders.contains(*e));
    }
    for (entity, control, mut slider) in &mut sliders {
        if control.action != UiVideoAction::SeekSlider {
            continue;
        }
        // While the pointer holds this slider the widget owns the value: copying the
        // playback position back now would fight the in-progress drag (the next delta
        // would apply to the stale position).
        if pointer
            .as_deref()
            .is_some_and(|p| p.pressed == Some(entity) || p.dragging == Some(entity))
        {
            continue;
        }
        let Ok(video) = videos.get(control.video) else {
            continue;
        };
        let Some(info) = source.frame(video.player) else {
            continue;
        };
        let Some(d) = info.duration.filter(|d| *d > 0.0) else {
            continue;
        };
        let fraction = (info.position / d).clamp(0.0, 1.0) as f32;
        // A seek queued from this slider is still in flight: keep the widget's value until
        // the source reports a position near the target, then resume following playback.
        if let Some((want, waited)) = pending.pending.get_mut(&entity) {
            if (fraction - *want).abs() <= SEEK_APPLIED_EPS || *waited >= SEEK_WAIT_FRAMES {
                pending.pending.remove(&entity);
            } else {
                *waited += 1;
                continue;
            }
        }
        let value = slider.min + (slider.max - slider.min) * fraction;
        if (slider.value - value).abs() > f32::EPSILON {
            // The thumb still moves: extract patches seek chrome on a clean frame.
            slider.bypass_change_detection().value = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::UiColor;

    #[test]
    fn transport_events_map_to_controls() {
        let click = UiEventKind::Click;
        assert_eq!(
            control_for(UiVideoAction::Play, &click, None, false),
            Some(VideoControl::Play)
        );
        assert_eq!(
            control_for(UiVideoAction::Pause, &click, None, false),
            Some(VideoControl::Pause)
        );
        assert_eq!(
            control_for(UiVideoAction::Toggle, &click, None, false),
            Some(VideoControl::Toggle)
        );
        assert_eq!(
            control_for(UiVideoAction::Skip, &click, None, false),
            Some(VideoControl::Skip)
        );
        assert_eq!(
            control_for(UiVideoAction::Loop, &click, None, true),
            Some(VideoControl::SetLoop(false))
        );
        assert_eq!(
            control_for(
                UiVideoAction::Loop,
                &UiEventKind::ValueChanged(UiValue::Bool(true)),
                None,
                false
            ),
            Some(VideoControl::SetLoop(true))
        );
        let half = UiEventKind::ValueChanged(UiValue::Number(0.5));
        assert_eq!(
            control_for(UiVideoAction::SeekSlider, &half, Some(4.0), false),
            Some(VideoControl::Seek(2.0))
        );
        assert_eq!(control_for(UiVideoAction::SeekSlider, &half, None, false), None);
        let past = UiEventKind::ValueChanged(UiValue::Number(3.0));
        assert_eq!(
            control_for(UiVideoAction::SeekSlider, &past, Some(4.0), false),
            Some(VideoControl::Seek(4.0))
        );
        assert_eq!(
            control_for(UiVideoAction::Play, &UiEventKind::PointerEnter, None, false),
            None
        );
    }

    /// One texture for a same-size picture, and the seek thumb moves without walking the subtree.
    #[test]
    fn playing_picture_reuses_one_texture() {
        use std::sync::{Arc, Mutex};

        use regia_render::{
            VideoControl, VideoControlError, VideoFrameInfo, VideoFrameSource, VideoFrames, VideoPlayback,
        };

        use crate::prelude::*;
        use crate::render::SharedUiRenderer;
        use crate::style::FlexDirection;

        struct Picture {
            serial: u64,
            position: f64,
        }

        struct FakeVideo {
            picture: Mutex<Picture>,
            rgba: Arc<[u8]>,
        }

        impl FakeVideo {
            fn new() -> Self {
                Self {
                    picture: Mutex::new(Picture {
                        serial: 1,
                        position: 2.0,
                    }),
                    rgba: Arc::<[u8]>::from(vec![255u8; 64 * 64 * 4]),
                }
            }

            fn set(&self, serial: u64, position: f64) {
                let Ok(mut picture) = self.picture.lock() else {
                    return;
                };
                picture.serial = serial;
                picture.position = position;
            }
        }

        impl VideoFrameSource for FakeVideo {
            fn frame(&self, _player: u64) -> Option<VideoFrameInfo> {
                let Ok(picture) = self.picture.lock() else {
                    return None;
                };
                Some(VideoFrameInfo {
                    width: 64,
                    height: 64,
                    aspect: 1.0,
                    serial: picture.serial,
                    pts_ns: Some(0),
                    fallback: false,
                    playback: VideoPlayback::Playing,
                    position: picture.position,
                    duration: Some(10.0),
                    looping: false,
                    audio_clock: false,
                })
            }

            fn texture(&self, _player: u64) -> Option<regia_render::DynamicTexture> {
                None
            }

            fn rgba(&self, _player: u64) -> Option<(u64, u32, u32, Arc<[u8]>)> {
                let Ok(picture) = self.picture.lock() else {
                    return None;
                };
                Some((picture.serial, 64, 64, Arc::clone(&self.rgba)))
            }

            fn control(&self, _player: u64, _control: VideoControl) -> Result<(), VideoControlError> {
                Ok(())
            }

            fn players(&self) -> Vec<u64> {
                vec![1]
            }
        }

        let mut app = App::new();
        app.add_plugins(UiPlugin::code_only());
        app.add_plugins(UiVideoPlugin);
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        let fake = Arc::new(FakeVideo::new());
        app.insert_resource(VideoFrames(Some(fake.clone())));

        let world = app.world_mut();
        let root = world
            .spawn((
                UiRoot::default(),
                NodeStyle::new(Style {
                    size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                    direction: FlexDirection::Column,
                    ..Default::default()
                }),
            ))
            .id();
        let video = world
            .spawn((
                UiNode,
                UiVideo::new(1),
                NodeStyle::new(Style {
                    size: [Dimension::Points(320.0), Dimension::Points(180.0)],
                    ..Default::default()
                }),
            ))
            .id();
        let slider = world
            .spawn((
                UiNode,
                Slider {
                    value: 0.2,
                    min: 0.0,
                    max: 1.0,
                    step: 0.0,
                },
                UiVideoControl {
                    video,
                    action: UiVideoAction::SeekSlider,
                },
                NodeStyle::new(Style {
                    size: [Dimension::Points(320.0), Dimension::Points(24.0)],
                    ..Default::default()
                }),
            ))
            .id();
        world.entity_mut(root).add_child(video);
        world.entity_mut(video).add_child(slider);

        for _ in 0..8 {
            app.update();
        }
        let snap = |app: &App| {
            let renderer = app.world().resource::<SharedUiRenderer>().lock();
            (
                renderer.image_texture_creates(),
                renderer.image_level0_writes(),
                renderer.image_byte_copies(),
                renderer.images_generation(),
                renderer.subtree_walks(),
            )
        };
        let (creates, level0, bytes, generation, walks) = snap(&app);
        // The loader queues a 1×1 placeholder (4 bytes) and a 2×2 fallback (16 bytes) once.
        // The video adds one texture and copies no bytes.
        assert_eq!(creates, 3, "two built-in images plus one video texture");
        assert_eq!(bytes, 20, "the video picture is not copied into a Vec");
        let before_x = white_knob_x(&app);

        fake.set(2, 8.0);
        app.update();

        let (creates2, level0_2, bytes2, generation2, walks2) = snap(&app);
        assert_eq!(
            (creates2, bytes2, generation2, walks2),
            (creates, bytes, generation, walks),
            "same-size picture creates {creates}->{creates2}, bytes {bytes}->{bytes2}, \
             generation {generation}->{generation2}, walks {walks}->{walks2}, level0 {level0}->{level0_2}"
        );
        assert!(
            level0_2 > level0,
            "the second picture rewrites mip 0 ({level0} -> {level0_2})"
        );
        let value = app.world().get::<Slider>(slider).expect("slider").value;
        assert!((value - 0.8).abs() < 1e-3, "seek fraction 0.8, value {value}");
        let after_x = white_knob_x(&app);
        assert!(
            after_x > before_x,
            "seek thumb moves right ({before_x} -> {after_x})"
        );
    }

    fn white_knob_x(app: &App) -> f32 {
        app.world()
            .resource::<SharedUiRenderer>()
            .lock()
            .quads()
            .iter()
            .filter(|quad| {
                quad.color == UiColor::WHITE
                    && (quad.size[0] - quad.size[1]).abs() < 0.5
                    && quad.size[0] >= 8.0
            })
            .map(|quad| quad.pos[0])
            .fold(f32::MIN, f32::max)
    }

    /// Corr-ui-b: a queued seek that never reached its target (a failed seek, a source that lands
    /// elsewhere) held the seek slider forever, so it stopped following playback, and the entries
    /// of despawned sliders were never dropped.
    #[test]
    fn a_seek_that_never_lands_releases_the_slider() {
        use std::sync::Arc;

        use bevy_ecs::system::RunSystemOnce;
        use regia_render::{VideoControlError, VideoFrameInfo, VideoFrameSource, VideoPlayback};

        /// Always at 2 s of 10 s: the queued seek to 90 % never lands.
        struct Stuck;

        impl VideoFrameSource for Stuck {
            fn frame(&self, _player: u64) -> Option<VideoFrameInfo> {
                Some(VideoFrameInfo {
                    width: 4,
                    height: 4,
                    aspect: 1.0,
                    serial: 1,
                    pts_ns: Some(0),
                    fallback: false,
                    playback: VideoPlayback::Playing,
                    position: 2.0,
                    duration: Some(10.0),
                    looping: false,
                    audio_clock: false,
                })
            }

            fn texture(&self, _player: u64) -> Option<regia_render::DynamicTexture> {
                None
            }

            fn rgba(&self, _player: u64) -> Option<(u64, u32, u32, Arc<[u8]>)> {
                None
            }

            fn control(&self, _player: u64, _control: VideoControl) -> Result<(), VideoControlError> {
                Ok(())
            }

            fn players(&self) -> Vec<u64> {
                vec![1]
            }
        }

        let mut world = World::new();
        world.init_resource::<Messages<UiEvent>>();
        world.init_resource::<UiVideoStats>();
        world.init_resource::<crate::interaction::UiPointer>();
        world.init_resource::<UiVideoPendingSeeks>();
        world.insert_resource(VideoFrames(Some(Arc::new(Stuck))));
        let video = world.spawn(UiVideo::new(1)).id();
        let seek = |world: &mut World| {
            world
                .spawn((
                    Slider {
                        value: 0.9,
                        min: 0.0,
                        max: 1.0,
                        step: 0.0,
                    },
                    UiVideoControl {
                        video,
                        action: UiVideoAction::SeekSlider,
                    },
                ))
                .id()
        };
        let slider = seek(&mut world);
        world
            .resource_mut::<UiVideoPendingSeeks>()
            .pending
            .insert(slider, (0.9, 0));
        let value = |world: &World| world.get::<Slider>(slider).map_or(-1.0, |s| s.value);
        world.run_system_once(ui_video_controls).expect("runs");
        assert_eq!(value(&world), 0.9, "a seek in flight holds the slider");
        for _ in 0..SEEK_WAIT_FRAMES {
            world.run_system_once(ui_video_controls).expect("runs");
        }
        assert!(
            (value(&world) - 0.2).abs() < 1e-6,
            "the slider follows playback again: {}",
            value(&world)
        );
        assert!(world.resource::<UiVideoPendingSeeks>().pending.is_empty());

        // A despawned slider's entry is dropped.
        let gone = seek(&mut world);
        world
            .resource_mut::<UiVideoPendingSeeks>()
            .pending
            .insert(gone, (0.9, 0));
        world.despawn(gone);
        world.run_system_once(ui_video_controls).expect("runs");
        assert!(world.resource::<UiVideoPendingSeeks>().pending.is_empty());
    }
}
