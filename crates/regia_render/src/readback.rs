// SPDX-License-Identifier: MPL-2.0
//! Delayed GPU readback.
//!
//! Auto-exposure and timestamp queries used to map a staging buffer and then
//! block on `Device::poll(Wait)` in the same frame that produced the data.
//! That parks the CPU until the GPU is idle. A frame instead keeps three
//! staging slots: the GPU writes one, a later non-blocking `poll(Poll)` runs
//! the map callback, and the CPU reads the slot only after it is mapped and at
//! least [`READBACK_LATENCY`] frames have passed.
//!
//! The ring and the log-luminance average are covered by unit tests. The
//! histogram and timestamp paths in the frame renderer are the callers.

use std::sync::atomic::Ordering;

/// Staging buffers in the readback ring. Three covers a two-frame delay plus
/// the slot being filled this frame.
pub(crate) const READBACK_SLOTS: usize = 3;

/// Frames between the submit that fills a slot and the CPU read.
///
/// Two means the value applied on frame N was captured on frame N−2 (or
/// earlier, if the map callback was late). One slot stays in flight, one is
/// the delayed result, one is free to record.
pub(crate) const READBACK_LATENCY: u64 = 2;

/// Slot has not been submitted, or it was read and unmapped.
pub(crate) const ST_IDLE: u8 = 0;
/// `map_async` has been called; the callback has not run.
pub(crate) const ST_INFLIGHT: u8 = 1;
/// The callback succeeded and the staging buffer is mapped.
pub(crate) const ST_READY: u8 = 2;
/// The callback failed. The buffer is not mapped.
pub(crate) const ST_FAILED: u8 = 3;

/// Log-space blend toward a new luminance sample. Small enough that a one-frame
/// spike does not jump the exposure; large enough that a cut adapts in well
/// under a second at 60 Hz.
///
/// The post stack applies this to a histogram that is at least two frames old.
pub(crate) const LUMINANCE_SMOOTH_ALPHA: f32 = 0.15;

/// `true` when a mapped slot is old enough to read.
#[must_use]
pub(crate) fn slot_is_consumable(state: u8, submitted_frame: u64, frame: u64, latency: u64) -> bool {
    state == ST_READY && frame.saturating_sub(submitted_frame) >= latency
}

/// Lowest log2 luminance (cd/m²) the exposure histogram resolves. Must match
/// `exposure_hist.wgsl`.
pub(crate) const HIST_LOG2_MIN: f32 = -6.0;
/// Width of the exposure histogram in stops (log2 units). Must match `exposure_hist.wgsl`.
///
/// The HDR target holds photometric luminance: a sunlit snowfield is ~10⁴ cd/m², a
/// shadowed interior ~10 cd/m². The range is `[2⁻⁶, 2¹⁸)` cd/m² so daylight does not
/// pile up in the top bin (the old `[2⁻⁸, 2⁸)` range clipped every sunlit pixel to
/// 256 cd/m², and auto exposure then blew outdoor frames out to white).
pub(crate) const HIST_LOG2_RANGE: f32 = 24.0;
/// Fraction of the darkest pixels the default meter ignores (`ExposureMeter`).
pub(crate) const METER_LOW_PERCENTILE: f64 = 0.40;
/// Fraction of pixels (from the dark end) above which the meter ignores the brightest.
pub(crate) const METER_HIGH_PERCENTILE: f64 = 0.95;

/// Average scene luminance from a log2 histogram.
///
/// Bin `i` of `n` bins covers log2-luminance `HIST_LOG2_MIN + [i, i+1) · range / n`.
/// The average is taken in log space over the pixels between
/// [`METER_LOW_PERCENTILE`] and [`METER_HIGH_PERCENTILE`]: deep shadow and the sun
/// disc or specular glints do not drag the exposure (percentile-trimmed metering,
/// after Narkowicz, "Automatic Exposure", 2016, public blog). An empty histogram
/// returns middle grey (`0.18`), matching the exposure the post stack uses before
/// the first sample arrives.
#[must_use]
pub(crate) fn average_log_luminance(counts: &[u32], low_percentile: f64, high_percentile: f64) -> f32 {
    let bins = counts.len().max(1) as f32;
    let total: f64 = counts.iter().map(|c| f64::from(*c)).sum();
    if total <= 0.0 {
        return 0.18;
    }
    let low = low_percentile.clamp(0.0, 1.0);
    let high = high_percentile.clamp(0.0, 1.0);
    // A degenerate window (low ≥ high) meters the median bin.
    let (low, high) = if high > low {
        (low, high)
    } else {
        (0.5, 0.5 + 1.0e-9)
    };
    let lo = total * low;
    let hi = total * high;
    let mut below = 0.0f64;
    let mut count = 0.0f64;
    let mut sum = 0.0f64;
    for (index, bin) in counts.iter().enumerate() {
        let n = f64::from(*bin);
        let start = below;
        let end = below + n;
        below = end;
        let kept = (end.min(hi) - start.max(lo)).max(0.0);
        if kept <= 0.0 {
            continue;
        }
        let log_y = (index as f32 + 0.5) / bins * HIST_LOG2_RANGE + HIST_LOG2_MIN;
        count += kept;
        sum += kept * f64::from(log_y);
    }
    if count <= 0.0 {
        return 0.18;
    }
    2.0f32.powf((sum / count) as f32)
}

/// Fraction of the histogram's pixels whose luminance lies in `[threshold, 16 · threshold)`
/// cd/m² (bins whose centre falls in the range): the bright-subject coverage of
/// [`crate::post::ExposureMeter::bright_threshold`]. `0` for an empty histogram or a
/// threshold ≤ 0.
#[must_use]
pub(crate) fn bright_fraction(counts: &[u32], threshold: f32) -> f32 {
    let total: f64 = counts.iter().map(|c| f64::from(*c)).sum();
    if total <= 0.0 || threshold <= 0.0 || !threshold.is_finite() {
        return 0.0;
    }
    let bins = counts.len().max(1) as f32;
    let lo = threshold.log2();
    let hi = lo + 4.0;
    let mut bright = 0.0f64;
    for (index, bin) in counts.iter().enumerate() {
        let log_y = (index as f32 + 0.5) / bins * HIST_LOG2_RANGE + HIST_LOG2_MIN;
        if log_y >= lo && log_y < hi {
            bright += f64::from(*bin);
        }
    }
    (bright / total) as f32
}

/// Exponential moving average of luminance, taken in log2 space so a bright
/// frame and a dark frame pull equally.
///
/// `previous == None` adopts `sample` unchanged (the first delayed read). The average is
/// carried from frame to frame, so a non-finite value never enters it (ADR-869): a NaN or
/// infinite `sample` keeps `previous` (0.18 cd/m² when there is none), and a non-finite
/// `previous` is dropped. An infinite average would otherwise pin the exposure at its
/// clamp for good, since `log2(∞) · (1 − α)` stays infinite.
#[must_use]
pub(crate) fn smooth_luminance(previous: Option<f32>, sample: f32, alpha: f32) -> f32 {
    let previous = previous.filter(|p| p.is_finite());
    if !sample.is_finite() {
        return previous.map_or(0.18, |p| p.max(1.0e-6));
    }
    let sample = sample.max(1.0e-6);
    let Some(previous) = previous else {
        return sample;
    };
    let previous = previous.max(1.0e-6);
    let alpha = alpha.clamp(0.0, 1.0);
    let log = previous.log2() * (1.0 - alpha) + sample.log2() * alpha;
    log.exp2()
}

/// Luminance that [`crate::exposure_from_luminance`] maps back to `exposure`.
///
/// Test helper: the post stack now starts auto exposure at the fixed EV100 scale
/// directly (`PostSettings::exposure_scale`) until the first histogram sample.
#[cfg(test)]
#[must_use]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn luminance_for_exposure(exposure: f32) -> f32 {
    0.18 / exposure.max(1.0e-4)
}

/// Store used by map callbacks. Release so a later acquire load sees the map.
pub(crate) fn store_map_result(state: &std::sync::atomic::AtomicU8, ok: bool) {
    state.store(if ok { ST_READY } else { ST_FAILED }, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_is_middle_grey() {
        let y = average_log_luminance(&[0; 64], METER_LOW_PERCENTILE, METER_HIGH_PERCENTILE);
        assert!((y - 0.18).abs() < 1.0e-6, "{y}");
    }

    #[test]
    fn one_bin_uses_its_centre() {
        let mut counts = [0u32; 64];
        counts[32] = 10;
        // Centre of bin 32: (32.5 / 64) * 24 - 6 = 6.1875.
        let y = average_log_luminance(&counts, METER_LOW_PERCENTILE, METER_HIGH_PERCENTILE);
        let expect = 2.0f32.powf(6.1875);
        assert!((y - expect).abs() < 1.0e-2, "{y} vs {expect}");
    }

    #[test]
    fn daylight_does_not_saturate_the_meter() {
        // A sunlit snowfield (~12 000 cd/m²) must meter near its own luminance,
        // not at the top of the histogram.
        let bin = ((12_000.0f32.log2() - HIST_LOG2_MIN) / HIST_LOG2_RANGE * 64.0) as usize;
        assert!(bin < 63, "12 000 cd/m² lands in bin {bin}");
        let mut counts = [0u32; 64];
        counts[bin] = 100;
        let y = average_log_luminance(&counts, METER_LOW_PERCENTILE, METER_HIGH_PERCENTILE);
        assert!((y.log2() - 12_000.0f32.log2()).abs() < 0.4, "{y}");
    }

    #[test]
    fn bright_fraction_counts_the_band_above_the_threshold() {
        let bins = 64usize;
        let bin_of =
            |lum: f32| (((lum.log2() - HIST_LOG2_MIN) / HIST_LOG2_RANGE) * bins as f32).floor() as usize;
        let mut counts = vec![0u32; bins];
        counts[bin_of(30.0)] = 50; // shadowed rock
        counts[bin_of(900.0)] = 40; // sunlit snow
        counts[bin_of(60_000.0)] = 10; // fire core, above 16× the threshold
        let f = bright_fraction(&counts, 400.0);
        assert!((f - 0.40).abs() < 1e-6, "{f}");
        assert_eq!(bright_fraction(&counts, 0.0), 0.0);
        assert_eq!(bright_fraction(&[0; 64], 400.0), 0.0);
    }

    #[test]
    fn meter_ignores_dark_corners_and_glints() {
        // 30 % black shadow, 65 % mid scene, 5 % specular glint: the meter reads the scene.
        let mut counts = [0u32; 64];
        counts[2] = 30;
        counts[40] = 65;
        counts[63] = 5;
        let y = average_log_luminance(&counts, METER_LOW_PERCENTILE, METER_HIGH_PERCENTILE);
        let mid = 2.0f32.powf(40.5 / 64.0 * HIST_LOG2_RANGE + HIST_LOG2_MIN);
        assert!((y.log2() - mid.log2()).abs() < 0.05, "{y} vs {mid}");
    }

    #[test]
    fn smoothing_is_symmetric_in_log_space() {
        let first = smooth_luminance(None, 0.2, LUMINANCE_SMOOTH_ALPHA);
        assert!((first - 0.2).abs() < 1.0e-6);
        let next = smooth_luminance(Some(0.2), 0.8, 0.5);
        // log2 midpoint of 0.2 and 0.8 is log2(0.4).
        let expect = 0.4f32;
        assert!((next - expect).abs() < 1.0e-4, "{next}");
    }

    #[test]
    fn latency_holds_a_fresh_slot() {
        assert!(!slot_is_consumable(ST_READY, 5, 6, READBACK_LATENCY));
        assert!(slot_is_consumable(ST_READY, 5, 7, READBACK_LATENCY));
        assert!(!slot_is_consumable(ST_INFLIGHT, 5, 9, READBACK_LATENCY));
        assert!(!slot_is_consumable(ST_IDLE, 5, 9, READBACK_LATENCY));
    }

    #[test]
    fn non_finite_luminance_never_enters_the_average() {
        assert_eq!(smooth_luminance(Some(2.0), f32::NAN, LUMINANCE_SMOOTH_ALPHA), 2.0);
        assert_eq!(
            smooth_luminance(Some(2.0), f32::INFINITY, LUMINANCE_SMOOTH_ALPHA),
            2.0
        );
        assert_eq!(smooth_luminance(None, f32::NAN, LUMINANCE_SMOOTH_ALPHA), 0.18);
        // A poisoned average restarts from the next finite sample.
        assert_eq!(
            smooth_luminance(Some(f32::INFINITY), 4.0, LUMINANCE_SMOOTH_ALPHA),
            4.0
        );
        assert_eq!(smooth_luminance(Some(f32::NAN), 4.0, LUMINANCE_SMOOTH_ALPHA), 4.0);
        // Finite inputs are unchanged by the guard.
        let mixed = smooth_luminance(Some(1.0), 16.0, 0.25);
        assert!((mixed - 2.0).abs() < 1e-5, "{mixed}");
        let mut avg = None;
        for sample in [1.0, f32::NAN, 8.0, f32::INFINITY, f32::NEG_INFINITY, 2.0] {
            avg = Some(smooth_luminance(avg, sample, LUMINANCE_SMOOTH_ALPHA));
            assert!(avg.is_some_and(f32::is_finite), "{avg:?} after {sample}");
        }
    }

    #[test]
    fn exposure_round_trips_through_luminance() {
        let exposure = 1.0 / 1.2;
        let y = luminance_for_exposure(exposure);
        let back = crate::exposure_from_luminance(y);
        assert!((back - exposure).abs() < 1.0e-4, "{back}");
    }

    /// The meter exactly as it was before ADR-046 (fixed 0.40–0.95 window, no weights),
    /// kept here so a level with no `look` block is held to the old exposure.
    fn pre_adr046_average_log_luminance(counts: &[u32]) -> f32 {
        let bins = counts.len().max(1) as f32;
        let total: f64 = counts.iter().map(|c| f64::from(*c)).sum();
        if total <= 0.0 {
            return 0.18;
        }
        let lo = total * 0.40;
        let hi = total * 0.95;
        let (mut below, mut count, mut sum) = (0.0f64, 0.0f64, 0.0f64);
        for (index, bin) in counts.iter().enumerate() {
            let n = f64::from(*bin);
            let (start, end) = (below, below + n);
            below = end;
            let kept = (end.min(hi) - start.max(lo)).max(0.0);
            if kept <= 0.0 {
                continue;
            }
            let log_y = (index as f32 + 0.5) / bins * HIST_LOG2_RANGE + HIST_LOG2_MIN;
            count += kept;
            sum += kept * f64::from(log_y);
        }
        if count <= 0.0 {
            return 0.18;
        }
        2.0f32.powf((sum / count) as f32)
    }

    fn bin_of(luminance: f32) -> usize {
        let t = ((luminance.max(1.0e-9).log2() - HIST_LOG2_MIN) / HIST_LOG2_RANGE).clamp(0.0, 0.999);
        ((t * 64.0) as usize).min(63)
    }

    #[test]
    fn a_level_without_a_look_meters_an_airless_frame_like_the_pre_adr046_meter() {
        // The default meter a migrated level with no `look` block gets.
        let meter = crate::post::ExposureMeter::default();
        assert_eq!(f64::from(meter.low_percentile), 0.40_f32 as f64);
        assert_eq!(f64::from(meter.high_percentile), 0.95_f32 as f64);
        assert_eq!(
            meter.center_weight, 0.0,
            "the default meter weights every pixel the same"
        );
        let settings = crate::PostSettings::default();
        assert_eq!(settings.meter, meter);
        // Airless frames: a black sky (0 cd/m², the histogram floor) over sunlit ice, from
        // a mostly-sky cockpit view to a mostly-ice overhead view; plus a few stars.
        for sky_fraction in [0.3_f32, 0.5, 0.62, 0.8, 0.95] {
            let mut counts = [0u32; 64];
            let n = 100_000_u32;
            let sky = (n as f32 * sky_fraction) as u32;
            counts[bin_of(0.0)] += sky - 40;
            counts[bin_of(12.0)] += 40; // daylight stars (ADR-046 radiance)
            let ice = n - sky;
            counts[bin_of(450.0)] += ice / 2;
            counts[bin_of(1_200.0)] += ice - ice / 2;
            let old = pre_adr046_average_log_luminance(&counts);
            let new = average_log_luminance(
                &counts,
                f64::from(meter.low_percentile),
                f64::from(meter.high_percentile),
            );
            let (old_ev, new_ev) = (crate::ev100_from_luminance(old), crate::ev100_from_luminance(new));
            assert!(
                (old_ev - new_ev).abs() <= 0.1,
                "sky {sky_fraction}: EV100 {new_ev:.2} vs the pre-ADR-046 meter's {old_ev:.2}"
            );
        }
    }
}
