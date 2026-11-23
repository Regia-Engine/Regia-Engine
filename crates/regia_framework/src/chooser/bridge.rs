// SPDX-License-Identifier: MPL-2.0
//! Bridges (spec 51 § 2.7): thin `PostUpdate` systems in [`ChooserSet::Bridge`] that turn
//! [`ChooserPicked`] messages into a montage play on the entity's `MontagePlayer` and a sound-cue
//! play on the `Audio` resource. Neither `regia_animation` nor `regia_audio` knows choosers exist.
//!
//! - A pick whose result is an asset of kind `montage` (`anim/hit_front.montage.ron`) plays the
//!   montage named by the full path, else by the file stem (`hit_front`), through
//!   `MontagePlayer::play_named`, with the outputs `rate` (float), `section` (name or enum) and
//!   `start_time` (float) as its `PlayOptions`. An entity without a `MontagePlayer` is left alone.
//!   `clip` picks are not bridged (ADR-1585: no named-slot entry point exists in `regia_animation`).
//! - A pick of kind `cue` plays the registered sound cue named by the full path, else by the stem,
//!   through `Audio::play_cue`; the cue's own variation is unchanged. The spec's position,
//!   `volume_db` and `bus` overrides need entry points `Audio` does not have yet (ADR-1649).

use regia_animation::montage_player::{MontagePlayer, PlayOptions};
use regia_app::prelude::*;
#[cfg(feature = "client")]
use regia_audio::Audio;

use super::{ChooserPicked, ChooserSet, ChooserStats, ChooserValue, ResultEntry, asset_kind};

/// Adds the montage and cue bridges in [`ChooserSet::Bridge`]. Add it with [`ChooserPlugin`](super::ChooserPlugin).
pub struct ChooserBridgePlugin;

impl Plugin for ChooserBridgePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PostUpdate,
            (
                montage_bridge,
                #[cfg(feature = "client")]
                cue_bridge,
            )
                .in_set(ChooserSet::Bridge),
        );
    }
}

/// The file stem of an asset path: the file name up to its first `.` (`anim/hit.montage.ron` is
/// `hit`).
#[must_use]
pub fn asset_stem(path: &str) -> &str {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file.split('.').next().unwrap_or(file)
}

fn output<'a>(pick: &'a ChooserPicked, name: &str) -> Option<&'a ChooserValue> {
    pick.outputs.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v)
}

fn play_options(pick: &ChooserPicked) -> PlayOptions {
    let mut options = PlayOptions::default();
    if let Some(ChooserValue::Float(rate)) = output(pick, "rate")
        && rate.is_finite()
        && *rate >= 0.0
    {
        options.rate = *rate;
    }
    if let Some(ChooserValue::Name(section) | ChooserValue::Enum(section)) = output(pick, "section") {
        options.section = Some(section.clone());
    }
    if let Some(ChooserValue::Float(start)) = output(pick, "start_time")
        && start.is_finite()
    {
        options.start_time = start.max(0.0);
    }
    options
}

/// `PostUpdate` ([`ChooserSet::Bridge`]): plays picked montages on the picking entity's player.
pub fn montage_bridge(
    mut picks: MessageReader<ChooserPicked>,
    mut players: Query<&mut MontagePlayer>,
    stats: Option<ResMut<ChooserStats>>,
) {
    let _span = tracing::trace_span!(target: "regia_framework", "chooser_bridge").entered();
    let mut played = 0;
    for pick in picks.read() {
        let ResultEntry::Asset(path) = &pick.result else {
            continue;
        };
        if asset_kind(path) != "montage" {
            continue;
        }
        let Ok(mut player) = players.get_mut(pick.entity) else {
            continue;
        };
        let name = if player.montage(path).is_some() {
            path.as_str()
        } else {
            asset_stem(path)
        };
        if player.play_named(name, play_options(pick)).is_some() {
            played += 1;
        } else {
            tracing::warn!(target: "regia_framework", montage = %path, "chooser picked a montage the player's library does not have");
        }
    }
    if played > 0
        && let Some(mut stats) = stats
    {
        stats.montage_plays += played;
    }
}

/// `PostUpdate` ([`ChooserSet::Bridge`]): plays picked sound cues.
#[cfg(feature = "client")]
pub fn cue_bridge(
    mut picks: MessageReader<ChooserPicked>,
    audio: Option<ResMut<Audio>>,
    stats: Option<ResMut<ChooserStats>>,
) {
    let Some(mut audio) = audio else {
        picks.clear();
        return;
    };
    let mut played = 0;
    for pick in picks.read() {
        let ResultEntry::Asset(path) = &pick.result else {
            continue;
        };
        if asset_kind(path) != "cue" {
            continue;
        }
        let result = match audio.play_cue(path) {
            Err(regia_audio::AudioError::UnknownEntry(_)) => audio.play_cue(asset_stem(path)),
            other => other,
        };
        match result {
            Ok(_) => played += 1,
            Err(error) => {
                tracing::warn!(target: "regia_framework", cue = %path, "chooser cue did not play: {error}");
            }
        }
    }
    if played > 0
        && let Some(mut stats) = stats
    {
        stats.cue_plays += played;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(outputs: Vec<(&str, ChooserValue)>) -> ChooserPicked {
        ChooserPicked {
            entity: Entity::PLACEHOLDER,
            site: "s".into(),
            table: "t.chooser.ron".into(),
            row: Some("r".into()),
            result: ResultEntry::Asset("anim/hit.montage.ron".into()),
            used_catch_all: false,
            outputs: outputs.into_iter().map(|(n, v)| (n.to_owned(), v)).collect(),
        }
    }

    /// Spec 51 § 2.7 / test 17: the outputs `rate`, `section` (name or enum) and `start_time` become
    /// the montage's `PlayOptions`; bad numbers keep the defaults, a negative start clamps to 0.
    #[test]
    fn outputs_become_the_montage_play_options() {
        let o = play_options(&pick(vec![
            ("rate", ChooserValue::Float(1.5)),
            ("section", ChooserValue::Name("loop".into())),
            ("start_time", ChooserValue::Float(-1.0)),
        ]));
        assert_eq!(
            (o.rate, o.section.as_deref(), o.start_time),
            (1.5, Some("loop"), 0.0)
        );
        let o = play_options(&pick(vec![
            ("section", ChooserValue::Enum("end".into())),
            ("start_time", ChooserValue::Float(0.25)),
        ]));
        assert_eq!((o.section.as_deref(), o.start_time), (Some("end"), 0.25));
        let defaults = PlayOptions::default();
        let o = play_options(&pick(vec![
            ("rate", ChooserValue::Float(-2.0)),
            ("start_time", ChooserValue::Float(f32::INFINITY)),
            ("section", ChooserValue::Int(3)),
        ]));
        assert_eq!(
            (o.rate, o.section, o.start_time),
            (defaults.rate, None, defaults.start_time)
        );
        // The last write of a name wins.
        let o = play_options(&pick(vec![
            ("rate", ChooserValue::Float(2.0)),
            ("rate", ChooserValue::Float(0.5)),
        ]));
        assert_eq!(o.rate, 0.5);
        assert_eq!(asset_stem("anim/hit.montage.ron"), "hit");
        assert_eq!(asset_stem(r"anim\sub\x.montage.ron"), "x");
    }
}
