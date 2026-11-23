// SPDX-License-Identifier: MPL-2.0
//! Boot movies (specs/50 § 2.7, ADR-2410): the project's startup movies (`[boot] movies` in
//! `Regia.toml`, read by `regia_framework::boot_movies`) played in order, fullscreen above the UI,
//! before the first level is shown.
//!
//! [`BootMovies`] is the sequence: insert it (the framework does, from the project file) and
//! [`MediaPlugin`](crate::MediaPlugin) plays it. Each movie gets its own entity with a
//! [`VideoPlayer`] and a [`VideoFullscreen`] at [`BOOT_MOVIE_LAYER`]; when it ends, fails or is
//! skipped the entity is despawned and the next one opens. A [`SkipVideo`] message (the `skip_video`
//! input action, `video.skip`, `media.boot_skip`) skips the current movie once it has played
//! [`BootMovies::skippable_after`] seconds of media time; an earlier request is refused with
//! `boot_skip_too_early` and forgotten. The same message also skips every other skippable
//! fullscreen player. [`boot_movies_done`] is the run condition a level reveal waits for.
//!
//! Everything is counted in media time and ticks, never wall clock, so a `Lockstep` sequence is
//! repeatable.

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::player::{ClockMode, LoopMode, PlayState};
use crate::plugin::{VideoPlayer, VideoPlayerState};
use crate::present::{Fit, VideoFullscreen};

/// Fullscreen layer of boot movies: above the UI and above game fullscreen videos.
pub const BOOT_MOVIE_LAYER: i32 = 1000;

/// Default seconds of a movie before it may be skipped.
pub const DEFAULT_SKIPPABLE_AFTER: f64 = 1.0;

/// Asks to skip the current boot movie and every skippable fullscreen video.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SkipVideo;

/// How a boot movie finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BootOutcome {
    /// Played to its end.
    Played,
    /// Skipped by the player.
    Skipped,
    /// Did not open or failed while playing (protocol code); the sequence moved on.
    Failed(String),
}

impl BootOutcome {
    /// Lowercase name (agent surface).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Played => "played",
            Self::Skipped => "skipped",
            Self::Failed(_) => "failed",
        }
    }
}

/// One finished movie.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootRecord {
    /// Position in the list.
    pub index: usize,
    /// Source path.
    pub source: String,
    /// How it finished.
    pub outcome: BootOutcome,
}

/// The boot movie sequence (a resource).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct BootMovies {
    /// Sources in play order (`.video.ron` or media file paths).
    pub movies: Vec<String>,
    /// Seconds of media time before a movie may be skipped (0 = at once).
    pub skippable_after: f64,
    /// Clock of the boot players (`Game` by default; tests use `Lockstep`).
    pub clock: ClockMode,
    /// Fit of the fullscreen picture.
    pub fit: Fit,
    next: usize,
    current: Option<(usize, Entity)>,
    records: Vec<BootRecord>,
    skip_wanted: bool,
    refused_skips: u32,
    done: bool,
}

impl BootMovies {
    /// A sequence of `movies`, skippable after [`DEFAULT_SKIPPABLE_AFTER`] seconds.
    pub fn new(movies: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let movies: Vec<String> = movies.into_iter().map(Into::into).collect();
        Self {
            done: movies.is_empty(),
            movies,
            skippable_after: DEFAULT_SKIPPABLE_AFTER,
            clock: ClockMode::Game,
            fit: Fit::Letterbox,
            next: 0,
            current: None,
            records: Vec::new(),
            skip_wanted: false,
            refused_skips: 0,
        }
    }

    /// Builder: the skip delay in seconds (negative or non-finite is 0).
    #[must_use]
    pub fn skippable_after(mut self, seconds: f64) -> Self {
        self.skippable_after = if seconds.is_finite() {
            seconds.max(0.0)
        } else {
            0.0
        };
        self
    }

    /// Builder: the clock of the boot players.
    #[must_use]
    pub fn with_clock(mut self, clock: ClockMode) -> Self {
        self.clock = clock;
        self
    }

    /// Every movie has finished (or the list is empty).
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// The playing movie as (index, player entity).
    pub fn current(&self) -> Option<(usize, Entity)> {
        self.current
    }

    /// Finished movies in order.
    pub fn records(&self) -> &[BootRecord] {
        &self.records
    }

    /// Skip requests refused because the movie had not played long enough.
    pub fn refused_skips(&self) -> u32 {
        self.refused_skips
    }

    fn finish_current(&mut self, outcome: BootOutcome) {
        if let Some((index, _)) = self.current.take() {
            self.records.push(BootRecord {
                index,
                source: self.movies.get(index).cloned().unwrap_or_default(),
                outcome,
            });
        }
    }
}

/// Run condition: no boot sequence, or it has finished.
pub fn boot_movies_done(boot: Option<Res<BootMovies>>) -> bool {
    boot.is_none_or(|b| b.is_done())
}

/// Whether the player's media position allows a skip.
fn may_skip(state: &VideoPlayerState, after: f64) -> bool {
    let core = state.core();
    match core.state() {
        PlayState::Opening => after <= 0.0,
        _ => core.position() + 1e-9 >= after,
    }
}

/// `PreUpdate`, before the players open: applies skips, retires the finished movie and starts the
/// next one.
pub(crate) fn run_boot_movies(
    mut commands: Commands,
    boot: Option<ResMut<BootMovies>>,
    mut skips: MessageReader<SkipVideo>,
    mut players: Query<(Entity, &mut VideoPlayerState, Option<&VideoFullscreen>)>,
) {
    let skip = skips.read().count() > 0;
    let Some(mut boot) = boot else {
        if skip {
            skip_fullscreen(&mut players, None);
        }
        return;
    };
    let current = boot.current.map(|(_, e)| e);
    if skip {
        skip_fullscreen(&mut players, current);
        boot.skip_wanted = true;
    }
    if let Some((_, entity)) = boot.current {
        match players.get_mut(entity) {
            Ok((_, mut state, _)) => {
                if std::mem::take(&mut boot.skip_wanted) {
                    if may_skip(&state, boot.skippable_after) {
                        state.core_mut().skip();
                    } else {
                        boot.refused_skips += 1;
                        tracing::debug!(target: "regia_media", "boot movie skip refused: boot_skip_too_early");
                    }
                }
                let core = state.core();
                let outcome = match core.state() {
                    PlayState::Ended => Some(BootOutcome::Played),
                    PlayState::Closed if core.last_error().is_none() => Some(BootOutcome::Skipped),
                    PlayState::Failed | PlayState::Closed => Some(BootOutcome::Failed(
                        core.last_error()
                            .map_or("decode_failed", crate::error::MediaError::code)
                            .to_string(),
                    )),
                    _ => None,
                };
                drop(core);
                if let Some(outcome) = outcome {
                    boot.finish_current(outcome);
                    commands.entity(entity).despawn();
                }
            }
            Err(_) => boot.finish_current(BootOutcome::Failed("player_not_found".to_string())),
        }
    }
    boot.skip_wanted = false;
    if boot.current.is_none() && !boot.done {
        if boot.next >= boot.movies.len() {
            boot.done = true;
            tracing::info!(target: "regia_media", movies = boot.movies.len(), "boot movies finished");
            return;
        }
        let index = boot.next;
        boot.next += 1;
        let source = boot.movies[index].clone();
        let entity = commands
            .spawn((
                Name::new(format!("boot_movie_{index}")),
                VideoPlayer {
                    source,
                    autoplay: true,
                    loop_mode: LoopMode::Off,
                    clock: boot.clock,
                    visibility: crate::plugin::VisibilityPolicy::Always,
                    ..VideoPlayer::default()
                },
                VideoFullscreen {
                    fit: boot.fit,
                    layer: BOOT_MOVIE_LAYER,
                    skippable: true,
                },
            ))
            .id();
        boot.current = Some((index, entity));
    }
}

fn skip_fullscreen(
    players: &mut Query<(Entity, &mut VideoPlayerState, Option<&VideoFullscreen>)>,
    except: Option<Entity>,
) {
    for (entity, mut state, full) in players.iter_mut() {
        if Some(entity) == except || !full.is_some_and(|f| f.skippable) {
            continue;
        }
        state.core_mut().skip();
    }
}
