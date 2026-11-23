// SPDX-License-Identifier: MPL-2.0
//! Painted material-layer weights: the "paint layer" brush target over [`crate::layers`].
//!
//! [`TerrainLayers`](crate::TerrainLayers) selects layers procedurally by slope and altitude
//! bands. A [`LayerWeightMap`] lets an artist override that per sample: one 8-bit weight per
//! layer per heightfield sample, 0 = not painted. Painted weight replaces procedural weight in
//! proportion to the painted coverage ([`TerrainLayers::blend_painted`](crate::TerrainLayers::blend_painted)),
//! so an unpainted map renders exactly the procedural look.
//!
//! Brushes edit one layer at a time through [`LayerWeightLayer`] (a [`StrokeTarget`] of `u8`).
//! The map persists as a versioned postcard blob ([`LayerWeightMap::to_bytes`],
//! `LAYER_WEIGHTS_FORMAT_VERSION`); none of the frozen terrain formats changes.

use glam::{IVec2, UVec2};
use serde::{Deserialize, Serialize};

use crate::brush::{StrokeSource, StrokeTarget};
use crate::layers::MAX_LAYERS;

/// Version of the painted layer-weight blob.
pub const LAYER_WEIGHTS_FORMAT_VERSION: u32 = 1;

/// The painted-weights file of a map, next to it and named as [`crate::holes_ron_path`] names the
/// holes file: `maps/switchbacks.level.ron` → `maps/switchbacks.weights.bin`. The file is the
/// [`LayerWeightMap::to_bytes`] blob.
#[must_use]
pub fn layer_weights_path(map: &std::path::Path) -> std::path::PathBuf {
    let holes = crate::holes_ron_path(map);
    let name = holes.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let stem = name.strip_suffix(".holes.ron").unwrap_or(name);
    holes.with_file_name(format!("{stem}.weights.bin"))
}

/// Largest `width * height * layers` a weight map allocates (1 GiB of weights).
const MAX_WEIGHT_SAMPLES: u64 = 1 << 30;

/// Errors for painted layer weights.
#[derive(Clone, Debug, thiserror::Error)]
pub enum LayerWeightError {
    /// Layer count outside `1..=MAX_LAYERS` or an empty grid.
    #[error(
        "layer weight map needs 1..={max} layers and a non-empty grid, got {layers} layers on {width}x{height}"
    )]
    BadShape {
        /// Layers requested.
        layers: u32,
        /// Columns.
        width: u32,
        /// Rows.
        height: u32,
        /// Maximum layers.
        max: usize,
    },
    /// `width * height * layers` exceeds `MAX_WEIGHT_SAMPLES`, or `width * height` exceeds
    /// [`MAX_LAYER_WEIGHT_SAMPLES`].
    #[error("layer weight map {width}x{height}x{layers} is too large")]
    TooLarge {
        /// Columns.
        width: u32,
        /// Rows.
        height: u32,
        /// Layers.
        layers: u32,
    },
    /// The blob's version is not this build's.
    #[error("unsupported layer weight version {found}, expected {expected}")]
    BadVersion {
        /// Version in the blob.
        found: u32,
        /// Version this build reads.
        expected: u32,
    },
    /// The blob could not be decoded or its sizes disagree.
    #[error("invalid layer weight blob: {0}")]
    Decode(String),
}

/// Most samples (columns × rows) a painted weight map may have: a 16 K × 16 K heightfield.
/// Larger shapes are refused before anything is allocated (a corrupt blob must not ask for
/// terabytes).
pub const MAX_LAYER_WEIGHT_SAMPLES: u64 = 16_385 * 16_385;

/// The weight count of a valid shape: `width * height * layers`, or `None` for 0 or more than
/// [`MAX_LAYERS`] layers, an empty grid or more than [`MAX_LAYER_WEIGHT_SAMPLES`] samples.
fn weight_count(width: u32, height: u32, layers: u32) -> Option<usize> {
    if layers == 0 || layers as usize > MAX_LAYERS || width == 0 || height == 0 {
        return None;
    }
    let samples = u64::from(width) * u64::from(height);
    if samples > MAX_LAYER_WEIGHT_SAMPLES {
        return None;
    }
    usize::try_from(samples * u64::from(layers)).ok()
}

/// The serde shape of `<map>.weights.bin` for `cargo xtask check-formats`
/// (`docs/formats/terrain-weights.md`, ADR-2316): the [`LayerWeightMap`] fields in their postcard
/// order (`version` first).
#[must_use]
pub fn layer_weights_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<LayerWeightMap>()])
}

/// `<map>.weights.bin` is at its first version: no migration steps.
#[must_use]
pub fn layer_weights_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Per-sample painted weights of every material layer (layer-major `u8` planes).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerWeightMap {
    /// Blob version.
    version: u32,
    /// Columns (the heightfield's).
    width: u32,
    /// Rows.
    height: u32,
    /// Layer planes.
    layers: u32,
    /// `layers` planes of `width * height` weights, 0..=255.
    weights: Vec<u8>,
}

impl LayerWeightMap {
    /// An unpainted map for a `width × height` heightfield with `layers` layers.
    ///
    /// # Errors
    ///
    /// [`LayerWeightError::BadShape`] for 0 or more than [`MAX_LAYERS`] layers or an empty grid;
    /// [`LayerWeightError::TooLarge`] for a grid larger than [`MAX_LAYER_WEIGHT_SAMPLES`] samples
    /// or a map larger than `MAX_WEIGHT_SAMPLES` weights.
    pub fn new(width: u32, height: u32, layers: u32) -> Result<Self, LayerWeightError> {
        if layers == 0 || layers as usize > MAX_LAYERS || width == 0 || height == 0 {
            return Err(LayerWeightError::BadShape {
                layers,
                width,
                height,
                max: MAX_LAYERS,
            });
        }
        // Hostile shapes must be errors, not a wrapped product or an allocation abort. `u128`: two
        // `u32::MAX` sides times the layer count overflow `u64`.
        // Two caps: the whole map (`MAX_WEIGHT_SAMPLES`) and one layer's grid
        // (`MAX_LAYER_WEIGHT_SAMPLES`).
        let total = u128::from(width) * u128::from(height) * u128::from(layers);
        let samples = u64::from(width) * u64::from(height);
        if total > u128::from(MAX_WEIGHT_SAMPLES) || samples > MAX_LAYER_WEIGHT_SAMPLES {
            return Err(LayerWeightError::TooLarge {
                width,
                height,
                layers,
            });
        }
        let Ok(total) = usize::try_from(total) else {
            return Err(LayerWeightError::TooLarge {
                width,
                height,
                layers,
            });
        };
        Ok(Self {
            version: LAYER_WEIGHTS_FORMAT_VERSION,
            width,
            height,
            layers,
            weights: vec![0; total],
        })
    }

    /// Columns.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Rows.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Layer planes.
    pub fn layers(&self) -> u32 {
        self.layers
    }

    fn index(&self, layer: u32, col: u32, row: u32) -> Option<usize> {
        (layer < self.layers && col < self.width && row < self.height).then(|| {
            (layer as usize * self.height as usize + row as usize) * self.width as usize + col as usize
        })
    }

    /// The painted weight of `layer` at a sample (0 outside the map).
    pub fn get(&self, layer: u32, col: u32, row: u32) -> u8 {
        self.index(layer, col, row).map_or(0, |i| self.weights[i])
    }

    /// Sets the painted weight of `layer` at a sample (ignored outside the map).
    pub fn set(&mut self, layer: u32, col: u32, row: u32, weight: u8) {
        if let Some(i) = self.index(layer, col, row) {
            self.weights[i] = weight;
        }
    }

    /// One layer's plane, row-major.
    pub fn plane(&self, layer: u32) -> Option<&[u8]> {
        let n = self.width as usize * self.height as usize;
        (layer < self.layers).then(|| &self.weights[layer as usize * n..(layer as usize + 1) * n])
    }

    /// `true` when nothing is painted.
    pub fn is_blank(&self) -> bool {
        self.weights.iter().all(|w| *w == 0)
    }

    /// Painted weights (0..=1) of every layer at a sample, `MAX_LAYERS` long (zeros past
    /// [`LayerWeightMap::layers`]).
    pub fn sample(&self, col: u32, row: u32) -> [f32; MAX_LAYERS] {
        let mut out = [0.0; MAX_LAYERS];
        for (layer, w) in out.iter_mut().enumerate().take(self.layers as usize) {
            *w = f32::from(self.get(layer as u32, col, row)) / 255.0;
        }
        out
    }

    /// A brush target over one layer (`None` for a layer the map does not have).
    pub fn layer_mut(&mut self, layer: usize) -> Option<LayerWeightLayer<'_>> {
        (layer < self.layers as usize).then_some(LayerWeightLayer {
            map: self,
            layer: layer as u32,
        })
    }

    /// Serialises the map (postcard, versioned).
    ///
    /// # Errors
    ///
    /// [`LayerWeightError::Decode`] when postcard fails (implausibly large maps).
    pub fn to_bytes(&self) -> Result<Vec<u8>, LayerWeightError> {
        postcard::to_allocvec(self).map_err(|e| LayerWeightError::Decode(e.to_string()))
    }

    /// Reads a map written by [`LayerWeightMap::to_bytes`].
    ///
    /// # Errors
    ///
    /// [`LayerWeightError`] for a bad blob, version or shape.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, LayerWeightError> {
        let map: Self = postcard::from_bytes(bytes).map_err(|e| LayerWeightError::Decode(e.to_string()))?;
        if map.version != LAYER_WEIGHTS_FORMAT_VERSION {
            return Err(LayerWeightError::BadVersion {
                found: map.version,
                expected: LAYER_WEIGHTS_FORMAT_VERSION,
            });
        }
        let expected = weight_count(map.width, map.height, map.layers);
        if expected != Some(map.weights.len()) {
            return Err(LayerWeightError::Decode(format!(
                "{} weights for {}x{}x{}",
                map.weights.len(),
                map.width,
                map.height,
                map.layers
            )));
        }
        Ok(map)
    }
}

/// One layer of a [`LayerWeightMap`] as a brush target.
#[derive(Debug)]
pub struct LayerWeightLayer<'a> {
    map: &'a mut LayerWeightMap,
    layer: u32,
}

impl StrokeSource<u8> for LayerWeightLayer<'_> {
    fn size(&self) -> UVec2 {
        UVec2::new(self.map.width, self.map.height)
    }

    fn read(&self, position: IVec2) -> Option<u8> {
        if position.x < 0 || position.y < 0 {
            return None;
        }
        self.map
            .index(self.layer, position.x as u32, position.y as u32)
            .map(|i| self.map.weights[i])
    }
}

impl StrokeTarget<u8> for LayerWeightLayer<'_> {
    fn write(&mut self, position: IVec2, value: u8) {
        if position.x >= 0 && position.y >= 0 {
            self.map
                .set(self.layer, position.x as u32, position.y as u32, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_file_sits_next_to_the_map_under_its_name() {
        let path = |p: &str| layer_weights_path(std::path::Path::new(p));
        assert_eq!(
            path("maps/switchbacks.level.ron"),
            std::path::Path::new("maps/switchbacks.weights.bin")
        );
        assert_eq!(
            path("maps/v1.2.level.ron"),
            std::path::Path::new("maps/v1.2.weights.bin")
        );
        assert_eq!(path("maps/bare"), std::path::Path::new("maps/bare.weights.bin"));
    }
    use crate::brush::{Brush, BrushMode, BrushShape, BrushStroke, BrushTarget};
    use glam::Vec2;

    #[test]
    fn hostile_shape_is_a_too_large_error() {
        assert!(matches!(
            LayerWeightMap::new(1 << 31, 1 << 31, 4),
            Err(LayerWeightError::TooLarge { .. })
        ));
        assert!(matches!(
            LayerWeightMap::new(65536, 65536, 1),
            Err(LayerWeightError::TooLarge { .. })
        ));
        assert!(LayerWeightMap::new(u32::MAX, u32::MAX, 8).is_err());
    }

    #[test]
    fn shape_is_validated() {
        assert!(LayerWeightMap::new(4, 4, 0).is_err());
        assert!(LayerWeightMap::new(4, 4, MAX_LAYERS as u32 + 1).is_err());
        assert!(LayerWeightMap::new(0, 4, 2).is_err());
        let map = LayerWeightMap::new(4, 3, 2).expect("map");
        assert!(map.is_blank());
        assert_eq!(map.plane(1).map(<[u8]>::len), Some(12));
        assert!(map.plane(2).is_none());
    }

    #[test]
    fn painting_one_layer_leaves_the_others() {
        let mut map = LayerWeightMap::new(33, 33, 3).expect("map");
        let brush = Brush {
            shape: BrushShape::Circle { radius: 4.0 },
            mode: BrushMode::Assign { value: 1.0 },
            target: BrushTarget::LayerWeight { layer: 1 },
            hardness: 1.0,
            ..Brush::default()
        };
        let mut layer = map.layer_mut(1).expect("layer 1");
        let mut stroke = BrushStroke::<u8>::new(brush, 16);
        stroke.stamp(&layer, Vec2::new(16.0, 16.0), Vec2::ONE, 0.0);
        let record = stroke.finish_into(&mut layer);
        assert!(!record.is_empty());
        assert_eq!(map.get(1, 16, 16), 255);
        assert_eq!(map.get(1, 30, 30), 0);
        assert!(map.plane(0).is_some_and(|p| p.iter().all(|w| *w == 0)));
        assert!(map.plane(2).is_some_and(|p| p.iter().all(|w| *w == 0)));
        let mut layer = map.layer_mut(1).expect("layer 1");
        record.undo(&mut layer);
        assert!(map.is_blank(), "undo restores the unpainted map");
    }

    #[test]
    fn hostile_shape_that_wraps_the_product_is_rejected() {
        let hostile = LayerWeightMap {
            version: LAYER_WEIGHTS_FORMAT_VERSION,
            width: 1 << 31,
            height: 1 << 31,
            layers: 4,
            weights: Vec::new(),
        };
        let bytes = hostile.to_bytes().expect("encode");
        assert!(matches!(
            LayerWeightMap::from_bytes(&bytes),
            Err(LayerWeightError::Decode(_))
        ));
        assert!(LayerWeightMap::new(1 << 31, 1 << 31, 4).is_err());
    }

    #[test]
    fn empty_and_oversized_shapes_are_refused_without_allocating() {
        assert!(LayerWeightMap::new(u32::MAX, u32::MAX, MAX_LAYERS as u32).is_err());
        assert!(LayerWeightMap::new(16_386, 16_385, 1).is_err());
        // A hand-made blob claiming an empty grid or a vast one is refused, not trusted.
        for (width, height, layers) in [(0u32, 4u32, 2u32), (4, 0, 2), (u32::MAX, u32::MAX, 16), (5, 4, 0)] {
            let blob = LayerWeightMap {
                version: LAYER_WEIGHTS_FORMAT_VERSION,
                width,
                height,
                layers,
                weights: Vec::new(),
            };
            let bytes = blob.to_bytes().expect("encode");
            assert!(
                LayerWeightMap::from_bytes(&bytes).is_err(),
                "{width}x{height}x{layers} accepted"
            );
        }
    }

    #[test]
    fn bytes_round_trip_and_reject_garbage() {
        let mut map = LayerWeightMap::new(5, 4, 2).expect("map");
        map.set(1, 2, 3, 77);
        let bytes = map.to_bytes().expect("encode");
        assert_eq!(LayerWeightMap::from_bytes(&bytes).expect("decode"), map);
        assert!(LayerWeightMap::from_bytes(&bytes[..bytes.len() / 2]).is_err());
        let mut wrong = map.clone();
        wrong.version = 99;
        let bytes = wrong.to_bytes().expect("encode");
        assert!(matches!(
            LayerWeightMap::from_bytes(&bytes),
            Err(LayerWeightError::BadVersion { found: 99, .. })
        ));
    }
}
