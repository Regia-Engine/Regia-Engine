// SPDX-License-Identifier: MPL-2.0
//! A serializable texture recipe: a source (noise, a constant, or a mask baked from a mesh
//! recipe) and mask operations, written out as R32F, R16 or RGBA8. This is the form the agent
//! method `gen.texture` takes (`book/agents/gen-textures.md`).

use serde::{Deserialize, Serialize};

use crate::{
    BlendMode, CurvatureOptions, FloatImage, GenError, Levels, NoiseParams, Recipe, RecipeLimits,
    WearOptions, blend, blur, build_recipe, curvature_masks, invert, levels, noise_image, threshold,
    wear_masks,
};

/// A texture: its size, a source layer, operations on it, and the output format.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextureRecipe {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Where the values come from.
    pub source: Source,
    /// Operations, applied in order.
    #[serde(default)]
    pub ops: Vec<MaskOp>,
    /// Output format (default `rgba8`).
    #[serde(default)]
    pub format: TextureFormat,
}

/// A layer of values: a source and operations on it (the `layer` of [`MaskOp::Blend`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    /// Where the values come from.
    pub source: Source,
    /// Operations, applied in order.
    #[serde(default)]
    pub ops: Vec<MaskOp>,
}

/// Where a layer's values come from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    /// [`noise_image`].
    Noise(NoiseParams),
    /// Every pixel `value`.
    Constant {
        /// The value.
        value: f32,
    },
    /// The edge (convexity) mask of a mesh recipe ([`curvature_masks`]); the mesh needs UVs.
    Edge {
        /// The mesh (`gen.mesh`'s recipe form).
        mesh: Recipe,
        /// Mask options.
        #[serde(default)]
        curvature: CurvatureOptions,
    },
    /// The cavity (concavity) mask of a mesh recipe.
    Cavity {
        /// The mesh.
        mesh: Recipe,
        /// Mask options.
        #[serde(default)]
        curvature: CurvatureOptions,
    },
    /// The wear mask of a mesh recipe ([`wear_masks`]).
    Wear {
        /// The mesh.
        mesh: Recipe,
        /// Wear options.
        #[serde(default)]
        wear: WearOptions,
    },
    /// The grime mask of a mesh recipe ([`wear_masks`]).
    Grime {
        /// The mesh.
        mesh: Recipe,
        /// Wear options.
        #[serde(default)]
        wear: WearOptions,
    },
}

/// An operation on a layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum MaskOp {
    /// [`threshold`].
    Threshold {
        /// The step position.
        at: f32,
        /// Half-width of the smooth band (default 0: a hard step).
        #[serde(default)]
        softness: f32,
    },
    /// [`levels`].
    Levels(Levels),
    /// [`invert`].
    Invert {},
    /// [`blur`].
    Blur {
        /// Box radius in pixels.
        radius: u32,
        /// Passes (default 1).
        #[serde(default = "one_pass")]
        passes: u32,
        /// Wrap at the edges, for tileable images (default false).
        #[serde(default)]
        wrap: bool,
    },
    /// [`blend`] another layer over this one.
    Blend {
        /// The blend mode.
        mode: BlendMode,
        /// Opacity in `[0, 1]` (default 1).
        #[serde(default = "full")]
        opacity: f32,
        /// The layer blended on top (same size as the texture).
        layer: Box<Layer>,
    },
}

fn one_pass() -> u32 {
    1
}

fn full() -> f32 {
    1.0
}

/// How a texture is written out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextureFormat {
    /// Gray RGBA8 ([`FloatImage::to_rgba8`]).
    #[default]
    Rgba8,
    /// 16-bit samples ([`FloatImage::to_r16`]).
    R16,
    /// 32-bit floats ([`FloatImage::r32f_bytes`]).
    R32f,
}

impl TextureFormat {
    /// The format's name as the recipe spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Rgba8 => "rgba8",
            Self::R16 => "r16",
            Self::R32f => "r32f",
        }
    }
}

/// A built texture: the float values and the format they are written in.
#[derive(Clone, Debug, PartialEq)]
pub struct TextureOutput {
    /// The values before quantization.
    pub image: FloatImage,
    /// The output format.
    pub format: TextureFormat,
}

impl TextureOutput {
    /// The encoded bytes: RGBA8 rows, R16 samples little endian, or R32F bits little endian.
    pub fn bytes(&self) -> Vec<u8> {
        match self.format {
            TextureFormat::Rgba8 => self.image.to_rgba8().rgba8,
            TextureFormat::R16 => self
                .image
                .to_r16()
                .samples
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect(),
            TextureFormat::R32f => self.image.r32f_bytes(),
        }
    }

    /// A BLAKE3 hash (hex) of the format, size and encoded bytes.
    pub fn hash(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(b"regia_gen.texture.v1");
        h.update(self.format.name().as_bytes());
        h.update(&self.image.width.to_le_bytes());
        h.update(&self.image.height.to_le_bytes());
        h.update(&self.bytes());
        h.finalize().to_hex().to_string()
    }
}

/// Size limits of [`build_texture`] (an agent call is bounded by them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureLimits {
    /// Largest texture side.
    pub side: u32,
    /// Largest side of a texture with a mesh source (baking walks every triangle's texels).
    pub mesh_side: u32,
    /// Most operations, nested layers included.
    pub ops: usize,
    /// Deepest layer nesting.
    pub depth: usize,
    /// Largest blur radius.
    pub blur_radius: u32,
    /// Most blur passes.
    pub blur_passes: u32,
    /// Most estimated work, in units of one noise octave at one pixel (about 40 ns on a desktop
    /// core): [`texture_work`] of the recipe, checked before anything is built.
    pub work: u64,
    /// Limits of the mesh recipes.
    pub mesh: RecipeLimits,
}

impl Default for TextureLimits {
    /// The agent's limits: side 1024 (512 with a mesh source), 32 operations, nesting 4, blur
    /// radius 32 and 4 passes, 2^25 work units (about a second and a half), the default mesh
    /// recipe limits.
    fn default() -> Self {
        Self {
            side: 1024,
            mesh_side: 512,
            ops: 32,
            depth: 4,
            blur_radius: 32,
            blur_passes: 4,
            work: 1 << 25,
            mesh: RecipeLimits::default(),
        }
    }
}

fn too_large(what: &'static str, value: usize, limit: usize) -> Result<(), GenError> {
    if value > limit {
        Err(GenError::TooLarge { what, value, limit })
    } else {
        Ok(())
    }
}

/// Builds `recipe` within `limits`.
pub fn build_texture(recipe: &TextureRecipe, limits: &TextureLimits) -> Result<TextureOutput, GenError> {
    let _span = tracing::trace_span!("regia_gen::build_texture").entered();
    too_large(
        "texture side",
        recipe.width.max(recipe.height) as usize,
        limits.side as usize,
    )?;
    crate::image::check_size(recipe.width, recipe.height)?;
    let work = texture_work(recipe);
    if work > limits.work {
        return Err(GenError::TooLarge {
            what: "estimated texture work",
            value: usize::try_from(work).unwrap_or(usize::MAX),
            limit: usize::try_from(limits.work).unwrap_or(usize::MAX),
        });
    }
    let mut ops = 0usize;
    let image = layer(
        &recipe.source,
        &recipe.ops,
        (recipe.width, recipe.height),
        limits,
        0,
        &mut ops,
    )?;
    Ok(TextureOutput {
        image,
        format: recipe.format,
    })
}

/// The estimated cost of building `recipe`, in units of one noise octave at one pixel: per pixel,
/// noise costs its octaves (three times that for cellular kinds), a blur `(2·radius + 1) · 2 ·
/// passes / 32`, an edge or cavity mask 4, a wear or grime mask 16 plus its blur, and every other
/// source or operation 1; nested layers add up.
pub fn texture_work(recipe: &TextureRecipe) -> u64 {
    let px = u64::from(recipe.width) * u64::from(recipe.height);
    layer_work(&recipe.source, &recipe.ops, px, 0)
}

fn blur_work(px: u64, radius: u32, passes: u32) -> u64 {
    // `radius`/`passes` are raw serde u32s checked only later in `layer`, so every step here
    // must saturate: at u32::MAX for both, `(2r+1)·2·p` alone exceeds u64::MAX.
    let per_pass = 2u64
        .saturating_mul(u64::from(radius))
        .saturating_add(1)
        .saturating_mul(2)
        .saturating_mul(u64::from(passes));
    px.saturating_mul(per_pass).div_ceil(32)
}

fn layer_work(source: &Source, list: &[MaskOp], px: u64, depth: usize) -> u64 {
    // Nesting is limited when the recipe is built; stop counting at a depth no valid recipe has.
    if depth > 64 {
        return u64::MAX;
    }
    let mut work = match source {
        Source::Noise(p) => {
            let per = match p.kind {
                crate::NoiseKind::Cellular | crate::NoiseKind::CellularEdge => 3,
                _ => 1,
            };
            px.saturating_mul(u64::from(p.octaves) * per)
        }
        Source::Constant { .. } => px,
        Source::Edge { .. } | Source::Cavity { .. } => px.saturating_mul(4),
        Source::Wear { wear, .. } | Source::Grime { wear, .. } => px
            .saturating_mul(16)
            .saturating_add(blur_work(px, wear.grime_blur, 2)),
    };
    for op in list {
        let w = match op {
            MaskOp::Blur { radius, passes, .. } => blur_work(px, *radius, *passes),
            MaskOp::Blend { layer: l, .. } => px.saturating_add(layer_work(&l.source, &l.ops, px, depth + 1)),
            _ => px,
        };
        work = work.saturating_add(w);
    }
    work
}

fn layer(
    source: &Source,
    list: &[MaskOp],
    (w, h): (u32, u32),
    limits: &TextureLimits,
    depth: usize,
    ops: &mut usize,
) -> Result<FloatImage, GenError> {
    too_large("layer nesting", depth, limits.depth)?;
    *ops += list.len();
    too_large("operation count", *ops, limits.ops)?;
    let mesh = |recipe: &Recipe| {
        too_large(
            "texture side with a mesh source",
            w.max(h) as usize,
            limits.mesh_side as usize,
        )?;
        build_recipe(recipe, &limits.mesh)
    };
    let mut image = match source {
        Source::Noise(params) => noise_image(params, w, h)?,
        Source::Constant { value } => {
            crate::finite("constant", &[*value])?;
            FloatImage::new(w, h, *value)?
        }
        Source::Edge { mesh: m, curvature } => curvature_masks(&mesh(m)?, w, h, curvature)?.edge,
        Source::Cavity { mesh: m, curvature } => curvature_masks(&mesh(m)?, w, h, curvature)?.cavity,
        Source::Wear { mesh: m, wear } | Source::Grime { mesh: m, wear } => {
            too_large(
                "wear grime_blur",
                wear.grime_blur as usize,
                limits.blur_radius as usize,
            )?;
            let masks = wear_masks(&mesh(m)?, w, h, wear)?;
            if matches!(source, Source::Wear { .. }) {
                masks.wear
            } else {
                masks.grime
            }
        }
    };
    for op in list {
        image = match op {
            MaskOp::Threshold { at, softness } => threshold(&image, *at, *softness)?,
            MaskOp::Levels(l) => levels(&image, l)?,
            MaskOp::Invert {} => invert(&image),
            MaskOp::Blur { radius, passes, wrap } => {
                too_large("blur radius", *radius as usize, limits.blur_radius as usize)?;
                too_large("blur passes", *passes as usize, limits.blur_passes as usize)?;
                blur(&image, *radius, *passes, *wrap)?
            }
            MaskOp::Blend {
                mode,
                opacity,
                layer: l,
            } => {
                let top = layer(&l.source, &l.ops, (w, h), limits, depth + 1, ops)?;
                blend(&image, &top, *mode, *opacity)?
            }
        };
    }
    Ok(image)
}
