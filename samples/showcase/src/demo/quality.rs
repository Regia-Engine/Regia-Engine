// SPDX-License-Identifier: MIT-0
//! The quality-tier picker (ADR-3013): the engine's automatic detection (`regia_render::quality::detect` over
//! the built-in GPU table, the same pick `regia_framework::auto_quality` makes on a first launch) with its
//! reason rewritten in plain words, and a live switch of the renderer's settings to a tier.

use regia::prelude::*;
use regia_render::quality::{DeviceClass, detect};
use regia_render::{AdapterProfile, Detection, GpuTierTable, QualityTier, RenderDevice, RenderSettings};
use serde_json::{Value, json};

use super::{DemoError, DemoErrorCode, DemoState, TierSource};
use crate::app::{Options, StartOptions, finish_settings};

/// The tiers the desktop picker offers, cheapest first (Very Low since v0.5.1, ADR-7882: Auto picks it on the
/// slowest built-in graphics, so the player can pick it back).
pub const DESKTOP_TIERS: [QualityTier; 6] = [
    QualityTier::VeryLow,
    QualityTier::Low,
    QualityTier::Medium,
    QualityTier::High,
    QualityTier::Ultra,
    QualityTier::Deck,
];

/// The tiers the picker offers on this platform.
#[must_use]
pub fn picker_tiers() -> &'static [QualityTier] {
    if cfg!(target_os = "android") {
        &[QualityTier::MobileLow, QualityTier::MobileHigh]
    } else {
        &DESKTOP_TIERS
    }
}

/// What a tier is, in a few words for the picker.
#[must_use]
pub const fn tier_summary(tier: QualityTier) -> &'static str {
    match tier {
        QualityTier::MobileLow => "phone, low settings",
        QualityTier::MobileHigh => "phone, high settings",
        QualityTier::Deck => "Steam Deck: 1280x800, 40 fps cap, upscaled from 70 %",
        QualityTier::Low => "desktop, low settings",
        QualityTier::VeryLow => {
            "desktop low settings drawn at 33 % and upscaled, for the slowest built-in graphics"
        }
        QualityTier::Medium => "desktop high settings drawn at 67 % and upscaled",
        QualityTier::High => "desktop, high settings",
        QualityTier::Ultra => "desktop, highest settings",
    }
}

/// A tier named by the picker or an agent (`auto` is not a tier: the caller detects).
///
/// # Errors
/// [`DemoErrorCode::UnknownTier`] for a name that is not one of [`QualityTier::ALL`].
pub fn parse_tier(name: &str) -> Result<QualityTier, DemoError> {
    QualityTier::from_name(name).ok_or_else(|| {
        let known: Vec<&str> = QualityTier::ALL.iter().map(|t| t.name()).collect();
        DemoError::new(
            DemoErrorCode::UnknownTier,
            format!("unknown tier `{name}` (auto, {})", known.join(", ")),
        )
    })
}

/// A detection and why, in words a player reads.
#[derive(Clone, Debug, PartialEq)]
pub struct TierWords {
    /// The tier picked.
    pub tier: QualityTier,
    /// The plain-words explanation shown under the picker.
    pub words: String,
    /// The engine's one-line reason (for logs and bug reports).
    pub reason: String,
    /// The GPU table row that matched, or `fallback`.
    pub rule: String,
    /// `measured`, `by_class`, `guess` or `fallback`.
    pub evidence: String,
    /// The graphics card as the driver names it.
    pub adapter: String,
}

impl TierWords {
    /// The words as JSON (`demo.state`'s `quality.detection`).
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "tier": self.tier.name(),
            "why": self.words,
            "reason": self.reason,
            "rule": self.rule,
            "evidence": self.evidence,
            "adapter": self.adapter,
        })
    }
}

/// The graphics card as a player knows it: its name, plus the memory or "built-in graphics".
fn gpu_label(adapter: &AdapterProfile) -> String {
    match adapter.class {
        DeviceClass::Integrated => format!("{} (built-in graphics)", adapter.name),
        DeviceClass::Cpu => format!("{} (software rendering on the processor)", adapter.name),
        DeviceClass::Virtual => format!("{} (a virtual graphics device)", adapter.name),
        _ => match adapter.vram_label() {
            Some(vram) => format!("{} with {vram} of video memory", adapter.name),
            None => adapter.name.clone(),
        },
    }
}

/// Rewrites a detection in plain words: which tier, what it means, and why the engine chose it (the GPU table's
/// evidence, then any step down for missing features, video memory or the platform).
#[must_use]
pub fn detect_words(detection: &Detection) -> TierWords {
    let tier = detection.tier;
    let gpu = gpu_label(&detection.adapter);
    let because = match detection.evidence.as_str() {
        "measured" => format!(
            "your graphics card, {gpu}, is one the engine's GPU table lists from measurements (row `{}`)",
            detection.rule
        ),
        "by_class" => format!(
            "your graphics card, {gpu}, is in a class of cards the engine's GPU table rates for this tier (row `{}`)",
            detection.rule
        ),
        "guess" => format!(
            "your graphics card, {gpu}, is in the engine's GPU table as an estimate, not a measurement (row `{}`)",
            detection.rule
        ),
        _ => format!(
            "your graphics card, {gpu}, is not in the engine's GPU table, so its general rule decided: {}",
            detection.note.trim_end_matches('.')
        ),
    };
    let mut words = format!(
        "Picked {} ({}) because {because}.",
        tier.name(),
        tier_summary(tier)
    );
    if !detection.caps.is_empty() {
        let steps: Vec<String> = detection.caps.iter().map(|c| c.replace(" → ", " to ")).collect();
        words.push_str(" It was then lowered: ");
        words.push_str(&steps.join("; "));
        words.push('.');
    }
    words.push_str(" You can pick another tier at any time.");
    TierWords {
        tier,
        words,
        reason: detection.reason.clone(),
        rule: detection.rule.clone(),
        evidence: detection.evidence.clone(),
        adapter: detection.adapter.name.clone(),
    }
}

/// Detects a tier for the running GPU, with the project's GPU table when the app has `AutoQuality`.
///
/// # Errors
/// [`DemoErrorCode::NoRenderer`] before the app has a GPU device.
pub fn detect_now(world: &World) -> Result<TierWords, DemoError> {
    let device = world.get_resource::<RenderDevice>().ok_or_else(|| {
        DemoError::new(
            DemoErrorCode::NoRenderer,
            "no GPU device yet: detection reads the graphics card",
        )
    })?;
    let adapter = AdapterProfile::from_device(device);
    let detection = match world.get_resource::<regia_framework::auto_quality::AutoQuality>() {
        Some(auto) => auto.detect(&adapter),
        None => detect(&adapter, &GpuTierTable::builtin()),
    };
    Ok(detect_words(&detection))
}

/// Switches the renderer's settings to `tier` live (the tier's preset, Medium's render scale, the GPU-dependent
/// levers), then the showcase's own changes ([`finish_settings`]).
pub fn apply_tier(world: &mut World, tier: QualityTier) {
    let options = world
        .get_resource::<StartOptions>()
        .map_or_else(Options::default, |o| o.0);
    let adapter = world
        .get_resource::<RenderDevice>()
        .map(AdapterProfile::from_device);
    let mut settings = world
        .get_resource::<RenderSettings>()
        .cloned()
        .unwrap_or_default();
    tier.apply_for_gpu(&mut settings, adapter.as_ref());
    finish_settings(&mut settings, &options);
    world.insert_resource(settings);
}

/// `Update`: detects and applies a tier on the first frame that has a GPU device, unless one was picked already.
pub fn detect_on_first_device(world: &mut World) {
    let pending = world
        .get_resource::<DemoState>()
        .is_some_and(|s| s.tier_source == TierSource::Pending);
    if !pending || !world.contains_resource::<RenderDevice>() {
        return;
    }
    if let Err(error) = super::apply(world, super::DemoCommand::DetectTier) {
        tracing::warn!(target: "showcase", "demo: tier detection: {error}");
    } else if let Some(words) = world.resource::<DemoState>().detection.as_ref() {
        tracing::info!(target: "showcase", "demo: {}", words.words);
    }
}

/// What the demo answers as `host` in the engine's `quality.get_detected` (ADR-5131): its tier, who picked it and
/// the detection's words, the same as `demo.state`'s `quality`.
#[must_use]
pub fn host_detection(state: &DemoState) -> Value {
    json!({
        "tier": state.tier.name(),
        "source": state.tier_source.name(),
        "detection": state.detection.as_ref().map(TierWords::to_json),
    })
}

/// What [`publish_detection`] last published: the parts of [`DemoState`] that [`host_detection`] reads.
#[cfg(feature = "agent")]
#[derive(Default)]
pub struct PublishedDetection(Option<(QualityTier, TierSource, Option<TierWords>)>);

#[cfg(feature = "agent")]
impl PublishedDetection {
    fn matches(&self, state: &DemoState) -> bool {
        self.0.as_ref().is_some_and(|(tier, source, words)| {
            *tier == state.tier && *source == state.tier_source && words.as_ref() == state.detection.as_ref()
        })
    }
}

/// `Last` (the `agent` feature only): keeps [`regia_agent::HostQualityDetection`] on the demo's pick, so an agent that
/// reads the generic `quality.get_detected` gets it (the demo has no `GameSettings`). It builds the value only when the
/// pick changed (or something else changed the published value): `DemoState` itself changes on every frame (its frame
/// counter), and building the JSON allocates.
#[cfg(feature = "agent")]
pub fn publish_detection(
    state: Res<DemoState>,
    mut host: ResMut<regia_agent::HostQualityDetection>,
    mut published: Local<PublishedDetection>,
) {
    if published.matches(&state) && !host.is_changed() {
        return;
    }
    let value = host_detection(&state);
    if host.0.as_ref() != Some(&value) {
        host.0 = Some(value);
    }
    published.0 = Some((state.tier, state.tier_source, state.detection.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(name: &str, class: DeviceClass, vram_mb: Option<u64>) -> AdapterProfile {
        AdapterProfile {
            class,
            ..AdapterProfile::named(name, 0x10de, vram_mb)
        }
    }

    #[test]
    fn every_detection_on_the_builtin_table_reads_as_a_sentence_naming_the_tier_and_the_card() {
        let table = GpuTierTable::builtin();
        for profile in [
            adapter(
                "NVIDIA GeForce RTX 3080 Ti",
                DeviceClass::Discrete,
                Some(12 * 1024),
            ),
            adapter(
                "NVIDIA GeForce GTX 1060 3GB",
                DeviceClass::Discrete,
                Some(3 * 1024),
            ),
            adapter("Intel(R) UHD Graphics 620", DeviceClass::Integrated, None),
            adapter("llvmpipe (LLVM 18.1.3, 256 bits)", DeviceClass::Cpu, None),
            adapter("Some Future GPU 9000", DeviceClass::Discrete, Some(32 * 1024)),
        ] {
            let detection = detect(&profile, &table);
            let words = detect_words(&detection);
            assert_eq!(words.tier, detection.tier);
            assert!(
                words
                    .words
                    .starts_with(&format!("Picked {} (", detection.tier.name())),
                "{}",
                words.words
            );
            assert!(words.words.contains(&profile.name), "{}", words.words);
            assert!(words.words.contains("because"), "{}", words.words);
            assert!(
                !words.words.contains('→'),
                "arrows are rewritten: {}",
                words.words
            );
            if !detection.caps.is_empty() {
                assert!(words.words.contains("It was then lowered"), "{}", words.words);
            }
            assert_eq!(words.reason, detection.reason);
        }
    }

    #[test]
    fn tier_names_parse_and_unknown_names_are_refused_with_a_code() {
        for tier in QualityTier::EVERY {
            assert_eq!(parse_tier(tier.name()), Ok(tier));
        }
        assert_eq!(parse_tier("high"), Ok(QualityTier::High));
        let Err(error) = parse_tier("Ultra9000") else {
            panic!("refused");
        };
        assert_eq!(error.code, DemoErrorCode::UnknownTier);
        for tier in picker_tiers() {
            assert!(!tier_summary(*tier).is_empty());
        }
    }

    #[test]
    fn switching_tiers_keeps_the_showcase_exposure_rule() {
        let mut world = World::new();
        world.insert_resource(StartOptions(Options::default()));
        apply_tier(&mut world, QualityTier::MobileLow);
        let low = world.resource::<RenderSettings>().clone();
        assert!(!low.profile.auto_exposure);
        assert!((low.post.exposure_ev - crate::app::PLAIN_EXPOSURE_EV).abs() < 1e-6);
        apply_tier(&mut world, QualityTier::Medium);
        let medium = world.resource::<RenderSettings>();
        assert!((medium.profile.render_scale - QualityTier::MEDIUM_RENDER_SCALE).abs() < 1e-6);
        assert!(medium.profile.gpu_driven);
    }
}
