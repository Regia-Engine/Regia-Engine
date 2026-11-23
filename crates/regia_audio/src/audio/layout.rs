// SPDX-License-Identifier: MPL-2.0
//! Replacing the bus layout of a live [`Audio`] and the parts of snapshots beyond bus volumes
//! (mutes, send levels, the authored blend time; `audio-buses` v1, ADR-8105 (g), ADR-8354).

use std::time::Duration;

use super::{Audio, BusBlend};
use crate::bus::BusLayout;
use crate::error::AudioError;

impl Audio {
    /// Replaces the bus layout (the editor mixer's chain, send and snapshot edits). The layout's
    /// snapshots are registered (replacing same-named ones). When `layout` builds the same
    /// tracks as the current one ([`BusLayout::same_tracks`]: only bus levels, mutes, send levels
    /// or snapshots differ), the new send levels apply live with no rebuild. Otherwise the bus
    /// tracks are rebuilt as on an output resume: playing voices are released, emitters restart
    /// on their next sync, and every bus keeps its host volume, mute and snapshot blend; a bus
    /// the new layout adds starts at its layout volume.
    ///
    /// # Errors
    ///
    /// [`AudioError::Invalid`] when `layout` fails [`BusLayout::validate`] or a render-out
    /// mixdown is running (its offline mixer holds the bus tree); the current layout stays.
    pub fn set_bus_layout(&mut self, mut layout: BusLayout) -> Result<(), AudioError> {
        let _span = tracing::debug_span!(target: "regia_audio", "set_bus_layout").entered();
        layout.validate().map_err(AudioError::Invalid)?;
        if self.mixdown.is_some() {
            return Err(AudioError::Invalid(
                "a render-out mixdown is running; change the bus layout after it ends".into(),
            ));
        }
        layout.version = crate::bus::AUDIO_BUSES_FORMAT_VERSION;
        for snapshot in &layout.snapshots {
            self.snapshots.register(snapshot.clone());
        }
        let rebuild = !self.bus_layout.same_tracks(&layout);
        let old = std::mem::replace(&mut self.bus_layout, layout);
        self.sync_bus_mix(&old);
        if self.suspended {
            // `resume_output` builds the new tree.
            return Ok(());
        }
        if rebuild {
            self.release_voices();
            let conv = self.convolution_context(self.output_sample_rate);
            match self.backend.build_buses(&self.bus_layout, &conv) {
                Ok(buses) => self.buses = buses,
                Err(error) => {
                    tracing::error!(target: "regia_audio", "bus rebuild failed: {error}");
                    // Keep the tracks that still exist; the old layout describes them.
                    self.bus_layout = old;
                    self.sync_bus_mix(&BusLayout::default());
                    return Err(error);
                }
            }
            self.reapply_bus_state();
            self.reset_emitters_after_rebuild();
        } else {
            self.buses.set_layout_levels(&self.bus_layout);
        }
        self.retarget_sends(Duration::ZERO);
        Ok(())
    }

    /// Keeps one blend per bus of the current layout after a layout change: buses that stay keep
    /// their blend and host volume (a changed layout volume moves the host volume with it when
    /// the host never set one of its own), new buses start at their layout volume and mute,
    /// removed buses are forgotten.
    fn sync_bus_mix(&mut self, old: &BusLayout) {
        let names: Vec<String> = self.bus_layout.buses.iter().map(|b| b.name.clone()).collect();
        self.bus_mix.retain(|name, _| names.contains(name));
        self.user_base_db.retain(|name, _| names.contains(name));
        for def in &self.bus_layout.buses {
            let old_db = old.buses.iter().find(|b| b.name == def.name).map(|b| b.volume_db);
            match self.bus_mix.get_mut(&def.name) {
                Some(blend) => {
                    let base = self.user_base_db.entry(def.name.clone()).or_insert(def.volume_db);
                    if old_db.is_some_and(|db| db == *base) && *base != def.volume_db {
                        *base = def.volume_db;
                        blend.target_db = self.snapshots.target_volume_db(&def.name, def.volume_db);
                        blend.rate_db_per_sec = f32::INFINITY;
                    }
                }
                None => {
                    self.user_base_db.insert(def.name.clone(), def.volume_db);
                    self.bus_mix.insert(
                        def.name.clone(),
                        BusBlend {
                            current_db: def.volume_db,
                            target_db: self.snapshots.target_volume_db(&def.name, def.volume_db),
                            rate_db_per_sec: f32::INFINITY,
                            last_applied_db: f32::NAN,
                            muted: def.mute,
                        },
                    );
                }
            }
        }
    }

    /// Pushes the registered snapshot `name` with its authored blend time
    /// ([`crate::AudioSnapshot::blend_secs`]): bus volumes blend over it, send levels and filter
    /// cutoffs tween over it, mutes apply at once.
    ///
    /// # Errors
    ///
    /// [`AudioError::UnknownSnapshot`] when no snapshot of that name is registered.
    pub fn apply_snapshot(&mut self, name: &str) -> Result<(), AudioError> {
        let secs = self
            .snapshots
            .get(name)
            .map(|s| s.blend_secs)
            .ok_or_else(|| AudioError::UnknownSnapshot(name.into()))?;
        self.push_snapshot(name, crate::bus::safe_duration(secs))
    }

    /// The registered snapshot `name`.
    pub fn snapshot(&self, name: &str) -> Option<&crate::AudioSnapshot> {
        self.snapshots.get(name)
    }

    /// Whether `bus` is silenced now: the active snapshot's mute override, else the host mute.
    pub(super) fn muted_now(&self, bus: &str) -> bool {
        self.snapshots
            .target_mute(bus)
            .unwrap_or_else(|| self.bus_mix.get(bus).is_some_and(|b| b.muted))
    }

    /// Points every send of the layout at the active snapshot's level, or at the layout's level
    /// when the snapshot does not set it, tweened over `fade`.
    pub(super) fn retarget_sends(&mut self, fade: Duration) {
        let routes: Vec<(String, String, f32)> = self
            .bus_layout
            .buses
            .iter()
            .flat_map(|b| {
                b.sends
                    .iter()
                    .map(|s| (b.name.clone(), s.bus.clone(), s.volume_db))
            })
            .collect();
        for (bus, send, layout_db) in routes {
            let db = self.snapshots.target_send(&bus, &send).unwrap_or(layout_db);
            self.buses.set_send_volume(&bus, &send, db, fade);
        }
    }
}
