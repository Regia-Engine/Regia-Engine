// SPDX-License-Identifier: MPL-2.0
//! Lightmaps at runtime (ADR-860): the GPU side of a `.flmap` (`regia_asset::lightmap`, baked by
//! `regia bake-lightmaps`), behind a flag that is off by default.
//!
//! - [`LightmapSettings`] (resource): `enabled` (default `false`) and an intensity multiplier.
//! - [`InstalledLightmap`] (resource): the lightmap a host or an agent installed, its source and
//!   the last load error with its agent code; [`InstalledLightmap::status`] is what
//!   `render.get_lightmaps` reports.
//! - [`LightmapGpu`]: the atlas uploaded as the file's blocks when [`lightmap_native_blocks`]
//!   accepts them (BC6H or ASTC HDR, edges multiple of 4, device feature on), otherwise the
//!   decoded texels as `Rgb9e5Ufloat`. Directions upload as BC7 or ASTC LDR when
//!   [`lightmap_native_direction_blocks`] accepts them, otherwise `Rgba8Unorm` (a 1×1 stand-in
//!   without directions). A linear clamp sampler and the parameter uniform travel with the bind
//!   group layout a pass declares to sample it.
//! - `shaders/lightmap.wgsl` ([`LIGHTMAP_WGSL`]): `lightmap_irradiance(uv2, scale_offset)`
//!   (bicubic B-spline or bilinear, [`LightmapSettings::bicubic`]),
//!   `lightmap_irradiance_for_normal(uv2, scale_offset, n)` (the directional rebalance,
//!   [`LightmapSettings::directional`], ADR-1556) and `lightmap_diffuse(albedo, irradiance)`;
//!   [`lightmap_diffuse`], `Lightmap::sample`, `Lightmap::sample_bicubic`,
//!   `Lightmap::sample_direction` and `regia_asset::lightmap::shade_directional` are their CPU
//!   mirrors.
//!
//! The forward pass shades with it (ADR-860 step 2, ADR-1623; [`LIGHTMAP_FORWARD_PASS`]): a
//! receiver is a drawn object whose key the host maps to an entry
//! ([`crate::FrameRenderer::set_lightmap_receivers`], or the [`LightmapReceiver`] component in an
//! app, applied by [`apply_lightmap_receivers`]). It is drawn from its entry's unwrap
//! ([`receiver_mesh`]: the source mesh's attributes through `xref`, the unwrap's indices and its
//! UV2 as the mesh's second UV stream) through the `LIGHTMAP` variant of the forward shader, whose
//! one `lightmap_receiver_irradiance` call replaces the probe / sky diffuse term. Every other
//! draw keeps its pipelines and bind groups.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use regia_app::prelude::*;
use regia_asset::lightmap::{Lightmap, LightmapContents, LightmapError, encode_direction};

/// The lookup functions (`shaders/lightmap.wgsl`); the includer declares the bindings.
pub const LIGHTMAP_WGSL: &str = include_str!("../shaders/lightmap.wgsl");

/// The forward pass samples installed lightmaps on their receivers (ADR-860 step 2, ADR-1623).
pub const LIGHTMAP_FORWARD_PASS: bool = true;

/// The line of `lab_forward.wgsl` (inside its `#ifdef LIGHTMAP` block) that
/// [`lightmap_forward_source`] replaces with [`LIGHTMAP_WGSL`].
pub const LIGHTMAP_LOOKUP_MARKER: &str = "// @lightmap_lookup_source";

/// The atlas and direction bindings are texture arrays, one layer a page (ADR-3300). `false`
/// keeps the 2-D bindings of a one-page atlas: the frame's group 3 layout must agree
/// ([`lightmap_texture_entry`]), and the frame patch of ADR-3300 switches both together. With 2-D
/// bindings a paged lightmap draws every receiver from page 0.
pub const LIGHTMAP_PAGED_BINDINGS: bool = true;

/// The page budget the bake tools default to (`regia bake-lightmaps --max-pages`, the Lightmaps
/// panel; ADR-3300): 4 when the frame binds texture arrays ([`LIGHTMAP_PAGED_BINDINGS`]), else 1,
/// so a build whose frame draws one page bakes one page (the density scales down as before pages)
/// unless the caller asks for more. Four 4096² pages are the ULTRA tier's whole lightmap budget
/// (ADR-3001).
pub const DEFAULT_LIGHTMAP_MAX_PAGES: u32 = if LIGHTMAP_PAGED_BINDINGS { 4 } else { 1 };

/// The `scale_offset` a draw hands the lookup for `entry`: the packed one (page in `z`) when the
/// frame binds texture arrays ([`LIGHTMAP_PAGED_BINDINGS`]), else the plain one, which the 2-D taps
/// read exactly as before pages.
pub fn lightmap_draw_scale_offset(entry: &regia_asset::lightmap::LightmapEntry) -> [f32; 4] {
    if LIGHTMAP_PAGED_BINDINGS {
        entry.packed_scale_offset()
    } else {
        entry.scale_offset
    }
}

/// The page reads of a 2-D atlas (`shaders/lightmap.wgsl`): exactly the reads before pages.
const LIGHTMAP_TAPS_2D: &str = "
fn lightmap_page_scale_offset(scale_offset: vec4<f32>) -> vec4<f32> {
    return scale_offset;
}
fn lightmap_page(scale_offset: vec4<f32>) -> i32 {
    return 0;
}
fn lightmap_atlas_sample(uv: vec2<f32>, page: i32) -> vec4<f32> {
    return textureSampleLevel(lightmap_atlas, lightmap_sampler, uv, 0.0);
}
fn lightmap_direction_sample(uv: vec2<f32>, page: i32) -> vec4<f32> {
    return textureSampleLevel(lightmap_direction, lightmap_sampler, uv, 0.0);
}
";

/// The page reads of a texture-array atlas (ADR-3300): the page rides in a packed `scale_offset`'s
/// `z` (`LightmapEntry::packed_scale_offset`, stride 2).
const LIGHTMAP_TAPS_PAGED: &str = "
fn lightmap_page(scale_offset: vec4<f32>) -> i32 {
    return i32(floor(scale_offset.z * 0.5));
}
fn lightmap_page_scale_offset(scale_offset: vec4<f32>) -> vec4<f32> {
    let page = floor(scale_offset.z * 0.5);
    return vec4<f32>(scale_offset.xy, scale_offset.z - 2.0 * page, scale_offset.w);
}
fn lightmap_atlas_sample(uv: vec2<f32>, page: i32) -> vec4<f32> {
    return textureSampleLevel(lightmap_atlas, lightmap_sampler, uv, page, 0.0);
}
fn lightmap_direction_sample(uv: vec2<f32>, page: i32) -> vec4<f32> {
    return textureSampleLevel(lightmap_direction, lightmap_sampler, uv, page, 0.0);
}
";

/// The lookup functions ([`LIGHTMAP_WGSL`]) with the page reads for 2-D (`paged` false) or
/// texture-array (`paged` true) bindings. The includer declares the bindings in that form.
#[must_use]
pub fn lightmap_lookup_wgsl(paged: bool) -> String {
    let taps = if paged {
        LIGHTMAP_TAPS_PAGED
    } else {
        LIGHTMAP_TAPS_2D
    };
    format!("{LIGHTMAP_WGSL}{taps}")
}

/// The declaration of the atlas binding in `lab_forward.wgsl` and `forward_mobile.wgsl`.
const ATLAS_DECLARATION_2D: &str = "var lightmap_atlas: texture_2d<f32>;";
/// The declaration of the direction binding in the same files.
const DIRECTION_DECLARATION_2D: &str = "var lightmap_direction: texture_2d<f32>;";

/// The forward shader source of the `LIGHTMAP` variant: `forward` (the text of
/// `lab_forward.wgsl`) with [`LIGHTMAP_LOOKUP_MARKER`] replaced by the lookup functions, in the
/// binding form of [`LIGHTMAP_PAGED_BINDINGS`]. The marker sits inside `#ifdef LIGHTMAP`, so every
/// other variant composes as before. A source without the marker comes back unchanged.
#[must_use]
pub fn lightmap_forward_source(forward: &str) -> String {
    lightmap_forward_source_with(forward, LIGHTMAP_PAGED_BINDINGS)
}

/// [`lightmap_forward_source`] in the binding form `paged` (ADR-3300): texture arrays also turn
/// the atlas and direction declarations into `texture_2d_array<f32>`.
#[must_use]
pub fn lightmap_forward_source_with(forward: &str, paged: bool) -> String {
    if !forward.contains(LIGHTMAP_LOOKUP_MARKER) {
        return forward.to_string();
    }
    // The declarations first: the spliced lookup file names them in its header comment.
    let declared = if paged {
        forward
            .replacen(
                ATLAS_DECLARATION_2D,
                "var lightmap_atlas: texture_2d_array<f32>;",
                1,
            )
            .replacen(
                DIRECTION_DECLARATION_2D,
                "var lightmap_direction: texture_2d_array<f32>;",
                1,
            )
    } else {
        forward.to_string()
    };
    declared.replacen(LIGHTMAP_LOOKUP_MARKER, &lightmap_lookup_wgsl(paged), 1)
}

/// The view dimension of the atlas and direction bindings ([`LIGHTMAP_PAGED_BINDINGS`]).
#[must_use]
pub const fn lightmap_view_dimension() -> wgpu::TextureViewDimension {
    if LIGHTMAP_PAGED_BINDINGS {
        wgpu::TextureViewDimension::D2Array
    } else {
        wgpu::TextureViewDimension::D2
    }
}

/// A fragment-stage layout entry for the atlas or the direction binding in the form the shaders
/// declare ([`LIGHTMAP_PAGED_BINDINGS`]): what the frame's lightmap group uses for those bindings.
#[must_use]
pub fn lightmap_texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: lightmap_view_dimension(),
            multisampled: false,
        },
        count: None,
    }
}

/// A receiver's geometry for the forward pass, built from its lightmap entry's unwrap by
/// [`receiver_mesh`].
#[derive(Clone, Debug)]
pub struct LightmapReceiverMesh {
    /// The source vertices copied through the entry's `xref` (a chart seam splits one source
    /// vertex into several).
    pub vertices: Vec<crate::LabVertex>,
    /// The unwrap's triangle list over [`Self::vertices`] (the source triangles, same order).
    pub indices: Vec<u32>,
    /// Entry-local UV2 (`0..1` over the entry's rectangle), one per vertex: the mesh's second UV
    /// stream (`FrameMesh::uv1`, [`crate::MeshUv1`]).
    pub uv2: Vec<[f32; 2]>,
    /// The entry's `scale_offset` as a draw hands it to the lookup ([`lightmap_draw_scale_offset`]:
    /// entry-local UV2 → atlas UV, the page in `z` when the frame binds texture arrays, ADR-3300).
    pub scale_offset: [f32; 4],
}

/// The receiver geometry of entry `entry` of `map` for the source mesh `vertices` / `indices`
/// (ADR-1623): [`Lightmap::check_receiver`] first (a mesh that changed since the bake is refused),
/// then every vertex attribute through [`Lightmap::remap_attribute`], the unwrap's own indices
/// and UV2.
///
/// # Errors
///
/// [`LightmapError::Layout`] (`lightmap_layout`): no such entry, or the mesh is not the one the
/// entry was baked for (re-bake it).
pub fn receiver_mesh(
    map: &Lightmap,
    entry: usize,
    vertices: &[crate::LabVertex],
    indices: &[u32],
) -> Result<LightmapReceiverMesh, LightmapError> {
    map.check_receiver(entry, vertices.len(), indices)?;
    let (Some(e), Some(unwrap)) = (map.entries.get(entry), map.unwraps.get(entry)) else {
        return Err(LightmapError::Layout(format!("no entry {entry}")));
    };
    let vertices = map
        .remap_attribute(entry, vertices)
        .ok_or_else(|| LightmapError::Layout(format!("entry `{}`: an xref is past the mesh", e.name)))?;
    if unwrap.uv.len() != vertices.len() {
        return Err(LightmapError::Layout(format!(
            "entry `{}`: {} UV2 for {} unwrapped vertices",
            e.name,
            unwrap.uv.len(),
            vertices.len()
        )));
    }
    Ok(LightmapReceiverMesh {
        vertices,
        indices: unwrap.indices.clone(),
        uv2: unwrap.uv.clone(),
        scale_offset: lightmap_draw_scale_offset(e),
    })
}

/// What the forward pass did with the installed lightmap in the last frame
/// ([`crate::FrameRenderer::lightmap_forward_stats`], `render.get_lightmaps` `forward`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct LightmapForwardStats {
    /// Receivers set ([`crate::FrameRenderer::set_lightmap_receivers`]).
    pub receivers: usize,
    /// The lightmap is installed and enabled, so receivers shade with it.
    pub active: bool,
    /// Visible draws the forward pass shaded with the lightmap.
    pub drawn: usize,
    /// Visible receiver draws that kept the probe light because the `LIGHTMAP` variant was not
    /// built yet (mid-play; it builds in the background).
    pub fallback: usize,
    /// Receiver draws the lightmap cannot shade (a material graph, blended or transmissive,
    /// skinned, morphed, a LOD cross-fade, a mask-decal instance, no UV2 stream, or an entry the
    /// lightmap does not have); they keep the probe light.
    pub unsupported: usize,
    /// Of [`Self::drawn`], the draws in the GPU-driven instanced batches (ADR-2107).
    pub instanced: usize,
    /// Of [`Self::drawn`], the cluster instances the visibility resolve shaded (ADR-2107).
    pub visbuffer: usize,
    /// Of [`Self::drawn`], the draws of the Mobile lite shader (ADR-2107).
    pub lite: usize,
    /// The lookup the frame allowed ([`LightmapTier::name`]; empty before the first frame).
    pub tier: &'static str,
    /// Bytes the forward pass's copy of the lightmap holds on the GPU now (ADR-3145): the atlas as
    /// uploaded (1 a texel for BC6H or ASTC HDR blocks the device samples natively, else 4) plus the
    /// direction texture when the frame's tier uploads it (4 a texel). 0 until a receiver draw
    /// uploads it, and after a clear. `LightmapStatus::gpu_bytes` is the file's RGB9E5 size instead.
    pub uploaded_bytes: u64,
    /// The forward pass's copy samples the file's blocks natively (ADR-2314).
    pub native_blocks: bool,
    /// The forward pass's copy holds the direction section (a [`LightmapTier::Bilinear`] copy does not).
    pub uploaded_directions: bool,
}

/// The lightmap lookup a render profile allows (ADR-2107, [`crate::RenderProfile::lightmap_tier`]).
/// It caps [`LightmapSettings`]: it never turns on what they turn off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LightmapTier {
    /// Bicubic filtering and directions, as the settings ask.
    #[default]
    Full,
    /// One bilinear fetch for the light and one for the direction.
    Directional,
    /// One bilinear fetch, and no direction section on the GPU.
    Bilinear,
}

impl LightmapTier {
    /// Stable lower-case name (`"full"`, `"directional"`, `"bilinear"`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Directional => "directional",
            Self::Bilinear => "bilinear",
        }
    }

    /// `settings` with this tier's cap applied.
    #[must_use]
    pub const fn apply(self, settings: LightmapSettings) -> LightmapSettings {
        LightmapSettings {
            bicubic: settings.bicubic && matches!(self, Self::Full),
            directional: settings.directional && !matches!(self, Self::Bilinear),
            ..settings
        }
    }

    /// This tier, lowered to what the frame's lite shader reads: the lite shader has no bicubic
    /// lookup, and at Low no direction lookup, whichever profile asked for it.
    #[must_use]
    pub const fn for_frame(self, lite: crate::forward_lite::ForwardLite) -> Self {
        match (self, lite) {
            (_, crate::forward_lite::ForwardLite::Low) | (Self::Bilinear, _) => Self::Bilinear,
            (Self::Full | Self::Directional, crate::forward_lite::ForwardLite::High)
            | (Self::Directional, crate::forward_lite::ForwardLite::Off) => Self::Directional,
            (Self::Full, crate::forward_lite::ForwardLite::Off) => Self::Full,
        }
    }
}

/// What a receiver whose mesh moved since the bake does ([`ReceiverFreshness::Stale`] with triangles
/// that still fit its entry; ADR-3006). Triangles that no longer fit and a missing entry already fall
/// back (`lightmap_layout`, `lightmap_entry`) whatever this says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StaleReceiverPolicy {
    /// Keep drawing the baked light and report it (`stale_instances`): a moved mesh keeps a light
    /// that is a little off until the re-bake. An exact hash also flags a re-export that changed
    /// nothing visible, which should not cost the receiver its lightmap.
    #[default]
    KeepDrawing,
    /// Draw the entity's own mesh with the frame's other light (the baked volume where one covers
    /// it) and report it (`lightmap_receiver_stale`, [`LightmapReceiverReport::stale_fallback`]).
    FallBack,
}

impl StaleReceiverPolicy {
    /// Every policy.
    pub const ALL: [Self; 2] = [Self::KeepDrawing, Self::FallBack];

    /// Stable snake-case name (`render.set_lightmaps` `stale_receivers`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::KeepDrawing => "keep_drawing",
            Self::FallBack => "fall_back",
        }
    }

    /// The policy named `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// The lightmap flag and its tunables.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct LightmapSettings {
    /// Lightmaps light the static meshes they cover (default `false`).
    pub enabled: bool,
    /// Multiplier on the baked irradiance.
    pub intensity: f32,
    /// Bicubic B-spline filtering (default `true`; `false` = bilinear, one tap instead of four).
    pub bicubic: bool,
    /// Use the lightmap's directions so normal maps bend the baked light (default `true`; no
    /// effect on a lightmap without directions).
    pub directional: bool,
    /// Receivers take their indirect specular from the baked light (default `true`, ADR-4762):
    /// the lightmap's radiance field read in the GGX lobe ([`lightmap_lobe_radiance`]), in place
    /// of the sky, probe and interior specular, as the lightmap already replaces their diffuse
    /// term. Without directions (or on a tier that drops them) the field is uniform. `false`
    /// keeps the receiver's other specular sources as before. Maps containing direct sun also
    /// keep those sources: their atlas cannot separate direct and indirect irradiance.
    pub specular: bool,
    /// What a receiver whose mesh moved since the bake does (default: keeps drawing, ADR-3006).
    pub stale_receivers: StaleReceiverPolicy,
}

impl Default for LightmapSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 1.0,
            bicubic: true,
            directional: true,
            specular: true,
            stale_receivers: StaleReceiverPolicy::KeepDrawing,
        }
    }
}

/// What `render.get_lightmaps` reports.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct LightmapStatus {
    /// The flag ([`LightmapSettings::enabled`]).
    pub enabled: bool,
    /// The intensity multiplier.
    pub intensity: f32,
    /// A lightmap is installed.
    pub installed: bool,
    /// Where it came from (a project-relative path or a host label).
    pub source: Option<String>,
    /// Atlas page width, texels (0 when none).
    pub width: u32,
    /// Atlas page height, texels.
    pub height: u32,
    /// Entries (placed meshes).
    pub entries: usize,
    /// Bytes the atlas takes on the GPU (4 per texel, 8 with directions), over every page.
    pub gpu_bytes: u64,
    /// The installed lightmap has a direction section (`.flmap` v2).
    pub has_directions: bool,
    /// The directional rebalance is on ([`LightmapSettings::directional`]).
    pub directional: bool,
    /// Bicubic filtering is on ([`LightmapSettings::bicubic`]).
    pub bicubic: bool,
    /// Receivers take their indirect specular from the lightmap ([`LightmapSettings::specular`],
    /// ADR-4762).
    pub specular: bool,
    /// The light components it holds (`sky`, `sun_bounce`, ...).
    pub contents: Vec<&'static str>,
    /// Share of the atlas holding baked or dilated texels.
    pub utilization: f32,
    /// The last load error.
    pub error: Option<String>,
    /// Its agent code (`lightmap_io`, `lightmap_bad_magic`, `lightmap_version`, ...).
    pub error_code: Option<String>,
    /// How the installed file stores its texels: `rgb9e5`, `bc6h` or `astc_hdr` (ADR-2314; empty
    /// when none is installed). A GPU that cannot sample the blocks gets their RGB9E5 decode.
    pub texel_encoding: &'static str,
    /// The denoiser its bake ran, when it did.
    pub denoise: Option<regia_asset::lightmap::LightmapDenoiseInfo>,
    /// The forward pass shades receivers with the lightmap ([`LIGHTMAP_FORWARD_PASS`]).
    pub forward_pass: bool,
    /// Entries of the installed lightmap with clusters baked for their unwrap (ADR-2107).
    pub meshlet_entries: usize,
    /// Atlas pages of the installed lightmap (ADR-3300; texture array layers on the GPU), 0 when
    /// none is installed.
    pub pages: u32,
    /// [`LightmapSettings::stale_receivers`], by its name.
    pub stale_receivers: &'static str,
}

/// The installed lightmap.
#[derive(Resource, Clone, Debug, Default)]
pub struct InstalledLightmap {
    /// The lightmap, when one is installed.
    pub lightmap: Option<Arc<Lightmap>>,
    /// Where it came from.
    pub source: Option<String>,
    /// The last load error and its agent code.
    pub error: Option<(String, &'static str)>,
}

impl InstalledLightmap {
    /// Installs `lightmap` (replacing any other) and clears the last error.
    pub fn install(&mut self, source: impl Into<String>, lightmap: Lightmap) {
        self.lightmap = Some(Arc::new(lightmap));
        self.source = Some(source.into());
        self.error = None;
    }

    /// Reads and installs the `.flmap` at `path`, labelled `source`. On failure the installed
    /// lightmap is kept and the error recorded.
    ///
    /// # Errors
    ///
    /// The file does not read or decode (`LightmapError`, see its `code`).
    pub fn load(&mut self, path: &std::path::Path, source: impl Into<String>) -> Result<(), LightmapError> {
        self.install_result(source, Lightmap::read(path))
    }

    /// Installs a lightmap that was read elsewhere (a cooked asset, [`read_cooked_lightmap`]),
    /// or records its error and keeps the installed one, as [`Self::load`] does.
    ///
    /// # Errors
    ///
    /// The `Err` of `read`, after recording it.
    pub fn install_result(
        &mut self,
        source: impl Into<String>,
        read: Result<Lightmap, LightmapError>,
    ) -> Result<(), LightmapError> {
        match read {
            Ok(map) => {
                self.install(source, map);
                Ok(())
            }
            Err(e) => {
                self.error = Some((e.to_string(), e.code()));
                Err(e)
            }
        }
    }

    /// Removes the installed lightmap.
    pub fn clear(&mut self) {
        self.lightmap = None;
        self.source = None;
        self.error = None;
    }

    /// The status under `settings`.
    #[must_use]
    pub fn status(&self, settings: &LightmapSettings) -> LightmapStatus {
        let map = self.lightmap.as_deref();
        LightmapStatus {
            enabled: settings.enabled,
            intensity: settings.intensity,
            installed: map.is_some(),
            source: self.source.clone(),
            width: map.map_or(0, |m| m.width),
            height: map.map_or(0, |m| m.height),
            entries: map.map_or(0, |m| m.entries.len()),
            gpu_bytes: map.map_or(0, |m| {
                let planes = if m.is_directional() { 2 } else { 1 };
                u64::from(m.width) * u64::from(m.height) * 4 * planes * u64::from(m.pages())
            }),
            has_directions: map.is_some_and(Lightmap::is_directional),
            directional: settings.directional,
            bicubic: settings.bicubic,
            specular: settings.specular,
            contents: map.map_or_else(Vec::new, |m| LightmapContents::names(m.bake.contents)),
            utilization: map.map_or(0.0, Lightmap::utilization),
            error: self.error.as_ref().map(|(m, _)| m.clone()),
            error_code: self.error.as_ref().map(|(_, c)| (*c).to_string()),
            forward_pass: LIGHTMAP_FORWARD_PASS,
            meshlet_entries: map.map_or(0, Lightmap::meshlet_entries),
            texel_encoding: map.map_or("", |m| m.texel_encoding().name()),
            denoise: map.and_then(|m| m.denoise),
            pages: map.map_or(0, Lightmap::pages),
            stale_receivers: settings.stale_receivers.name(),
        }
    }
}

/// Reads the lightmap cooked from `path` (a manifest path such as `assets/maps/bore.flmap`, or the
/// same path without `assets/`) out of the asset server's paks or cooked cache. `None` when the
/// manifest has no such entry (the caller then reads the raw file).
///
/// # Errors
///
/// Inside the `Some`: the cooked file does not read (`lightmap_io`) or its payload is not a
/// lightmap this build reads (the reader's code).
pub fn read_cooked_lightmap(
    server: &mut regia_asset::AssetServer,
    path: &str,
) -> Option<Result<Lightmap, LightmapError>> {
    let _span = tracing::info_span!("lightmap_read_cooked").entered();
    let rel = path.trim_start_matches("./");
    let id = [rel.to_string(), format!("assets/{rel}")]
        .iter()
        .find_map(|key| server.id_for_path(key))?;
    let io = |e: String| LightmapError::Io(std::io::Error::other(e));
    Some(
        server
            .read_cooked(id)
            .map_err(|e| io(e.to_string()))
            .and_then(|(bytes, _)| {
                let (_, payload) = regia_asset::decode_cooked_file(&bytes).map_err(|e| io(e.to_string()))?;
                Lightmap::from_bytes(payload)
            }),
    )
}

/// Marks a [`crate::MeshRenderer`] entity as a receiver of the installed lightmap's entry named
/// `entry` (ADR-1623; `regia bake-lightmaps` names entries `instance_<k>` after the bake
/// description's `k`-th instance, and `entity_<guid>_<part>` for a level, see [`receivers_by_key`]). While a lightmap is installed ([`InstalledLightmap`]),
/// [`apply_lightmap_receivers`] draws the entity from that entry's unwrap ([`receiver_mesh`]) and
/// the forward pass shades it with the lightmap while [`LightmapSettings::enabled`] is set.
/// Without a lightmap, or when the entity's mesh is not the one the entry was baked for, it is
/// drawn as without the component (the reason is in [`LightmapReceiverReport`]).
///
/// Reflected and registered by [`crate::register_render_types`] (ADR-2313): a level or prefab saves
/// it on the entity and loads it back (scene format v11).
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq, Eq)]
#[reflect(Component, Default, Clone)]
pub struct LightmapReceiver {
    /// The entry's name in the installed lightmap.
    pub entry: String,
}

/// Marks a drawn entity as part `part` of `owner`'s mesh (ADR-2313): a model with several parts
/// (one per material) draws part 0 on the entity itself or on a part entity, and every other part
/// on an entity of its own. [`receivers_by_key`] keys such an entity by its owner's
/// [`regia_scene::prelude::EntityGuid`] and `part`, so every part of a level's multi-part mesh
/// finds its entry (`regia bake-lightmaps --scene` keys entries `(entity, part)`). The editor's
/// mesh resolver inserts it on its part entities; a host that splits models inserts it itself.
/// Never saved (the part entities are rebuilt from the level's `MeshSource`).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeshPartOf {
    /// The entity carrying the level's guid (and the `MeshSource`).
    pub owner: Entity,
    /// The part's index in the model, the bake's part order.
    pub part: u32,
}

/// The receivers a level's own `.flmap` names (format version 3, `regia bake-lightmaps --scene`,
/// ADR-2024): every [`crate::MeshRenderer`] entity whose [`regia_scene::prelude::EntityGuid`] an
/// entry was baked for (its mesh's part 0, which is what a `MeshRenderer` on the guid entity
/// draws), and every `MeshRenderer` entity with a [`MeshPartOf`] whose owner's guid and part an
/// entry was baked for (ADR-2313: every part of a multi-part mesh), with that entry's name, sorted
/// by entity. A host inserts a [`LightmapReceiver`] for each; a lightmap with no keys names none.
#[must_use]
pub fn receivers_by_key(world: &mut World, map: &Lightmap) -> Vec<(Entity, String)> {
    receivers_by_key_with_parts(world, map)
        .into_iter()
        .map(|r| (r.entity, r.entry))
        .collect()
}

/// One receiver [`receivers_by_key_with_parts`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyedReceiver {
    /// The drawn entity.
    pub entity: Entity,
    /// The entity whose guid keys the entry (the drawn entity itself, or its [`MeshPartOf::owner`]).
    pub owner: Entity,
    /// The mesh part the entry was baked for.
    pub part: u32,
    /// The entry's name.
    pub entry: String,
}

/// [`receivers_by_key`] with the owner and part of each receiver, sorted by entity.
#[must_use]
pub fn receivers_by_key_with_parts(world: &mut World, map: &Lightmap) -> Vec<KeyedReceiver> {
    use regia_scene::prelude::EntityGuid;
    if !has_keys(map) {
        return Vec::new();
    }
    let mut candidates: Vec<ReceiverCandidate> = Vec::new();
    let mut own =
        world.query_filtered::<(Entity, &EntityGuid), (With<crate::MeshRenderer>, Without<MeshPartOf>)>();
    candidates.extend(own.iter(world).map(|(entity, guid)| ReceiverCandidate {
        entity,
        owner: entity,
        part: 0,
        guid: guid.0,
    }));
    let mut parts = world.query_filtered::<(Entity, &MeshPartOf), With<crate::MeshRenderer>>();
    let parts: Vec<(Entity, MeshPartOf)> = parts.iter(world).map(|(e, p)| (e, *p)).collect();
    for (entity, of) in parts {
        if let Some(guid) = world.get::<EntityGuid>(of.owner) {
            candidates.push(ReceiverCandidate {
                entity,
                owner: of.owner,
                part: of.part,
                guid: guid.0,
            });
        }
    }
    match_keyed_receivers(map, candidates)
}

/// Whether any entry of `map` is keyed to an entity (`regia bake-lightmaps --scene`).
#[must_use]
pub fn has_keys(map: &Lightmap) -> bool {
    map.entries.iter().any(|e| e.key.is_some())
}

/// A drawn entity that may receive a keyed entry: it draws part `part` of the mesh of `owner`,
/// whose level guid is `guid`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverCandidate {
    /// The drawn entity (a [`crate::MeshRenderer`]).
    pub entity: Entity,
    /// The entity carrying the guid (the drawn entity itself, or its [`MeshPartOf::owner`]).
    pub owner: Entity,
    /// The mesh part it draws.
    pub part: u32,
    /// The owner's [`regia_scene::prelude::EntityGuid`].
    pub guid: regia_core::id::Guid,
}

/// The candidates `map` has an entry for (by `(guid, part)` key, ADR-2024), with the entry's
/// name, sorted by entity. A pure function of its inputs, so a system with queries and the
/// world-wide [`receivers_by_key_with_parts`] attach the same receivers.
#[must_use]
pub fn match_keyed_receivers(
    map: &Lightmap,
    candidates: impl IntoIterator<Item = ReceiverCandidate>,
) -> Vec<KeyedReceiver> {
    let mut found: Vec<KeyedReceiver> = candidates
        .into_iter()
        .filter_map(|c| {
            let key = regia_asset::lightmap::LightmapEntryKey {
                entity: c.guid,
                part: c.part,
            };
            let entry = map.entry_for_key(key)?;
            Some(KeyedReceiver {
                entity: c.entity,
                owner: c.owner,
                part: c.part,
                entry: map.entries.get(entry)?.name.clone(),
            })
        })
        .collect();
    found.sort_by_key(|r| r.entity);
    found
}

/// Bookkeeping of an applied [`LightmapReceiver`], inserted and removed by
/// [`apply_lightmap_receivers`]: the entity's own mesh and second UV set (put back when the
/// receiver, its entry or the lightmap goes away) and the receiver mesh drawn instead.
#[derive(Component, Clone, Debug)]
pub struct AppliedLightmapReceiver {
    source: Arc<crate::CpuMesh>,
    source_uv1: Option<crate::MeshUv1>,
    receiver: Arc<crate::CpuMesh>,
    map: std::sync::Weak<Lightmap>,
    entry: u32,
    name: String,
    freshness: ReceiverFreshness,
}

impl AppliedLightmapReceiver {
    /// The entry index the entity draws with.
    #[must_use]
    pub fn entry(&self) -> u32 {
        self.entry
    }

    /// Its mesh against the hash the bake recorded, taken when the receiver was applied.
    #[must_use]
    pub fn freshness(&self) -> ReceiverFreshness {
        self.freshness
    }
}

/// A receiver's mesh against the `.flmap` entry it names (ADR-2656,
/// [`receiver_freshness`]). The UV2 a receiver draws comes from the entry itself, guarded by the
/// file's payload hash, so only the mesh can go stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum ReceiverFreshness {
    /// The mesh is the one the bake hashed.
    Fresh,
    /// No such entry, the triangles do not fit it, or the mesh's hash differs: it changed since the
    /// bake, so re-bake.
    Stale,
    /// The triangles fit and the file records no hash (a bake older than `.flmap` version 6, or one
    /// with no source mesh at hand): moved vertices cannot be told from this file.
    Unverified,
}

/// `vertices` and `indices` against entry `entry` of `map` (ADR-2656). Hashes the mesh when the file
/// has a hash for the entry, so it is not for every frame: [`apply_lightmap_receivers`] takes it
/// once, when it builds the receiver mesh.
#[must_use]
pub fn receiver_freshness(
    map: &Lightmap,
    entry: usize,
    vertices: &[crate::LabVertex],
    indices: &[u32],
) -> ReceiverFreshness {
    if map.check_receiver(entry, vertices.len(), indices).is_err() {
        return ReceiverFreshness::Stale;
    }
    match map.source_hash(entry) {
        None => ReceiverFreshness::Unverified,
        Some(baked) => {
            let live =
                regia_asset::lightmap::lightmap_source_hash(vertices.iter().map(|v| v.position), indices);
            if live == baked {
                ReceiverFreshness::Fresh
            } else {
                ReceiverFreshness::Stale
            }
        }
    }
}

/// The world's [`LightmapReceiver`] entities against the installed lightmap (`lightmaps` of
/// `render.get_gi_mode` and `level.get_baked_gi`, ADR-2656).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct LightmapInstanceCounts {
    /// Entities with a [`LightmapReceiver`].
    pub instances: usize,
    /// Of those, the ones whose mesh no longer matches its entry ([`ReceiverFreshness::Stale`]).
    pub stale_instances: usize,
    /// Of those, the ones the file has no hash to check ([`ReceiverFreshness::Unverified`]).
    pub unverified_instances: usize,
}

/// Counts the world's receivers against `map` (no lightmap: only `instances`). A receiver applied
/// from `map` reads what it recorded; any other hashes its mesh now. A receiver a
/// [`crate::MeshLodChain`] refuses is not stale: nothing about it is compared.
#[must_use]
pub fn lightmap_instance_counts(world: &World, map: Option<&Arc<Lightmap>>) -> LightmapInstanceCounts {
    let mut counts = LightmapInstanceCounts::default();
    let Some(mut query) = world.try_query::<(Entity, &LightmapReceiver)>() else {
        return counts;
    };
    for (entity, receiver) in query.iter(world) {
        counts.instances += 1;
        let Some(map) = map else {
            continue;
        };
        if world.get::<crate::MeshLodChain>(entity).is_some() {
            continue;
        }
        let applied = world.get::<AppliedLightmapReceiver>(entity);
        let freshness = match applied {
            Some(a) if a.map.ptr_eq(&Arc::downgrade(map)) && a.name == receiver.entry => a.freshness,
            _ => {
                let source = applied
                    .map(|a| a.source.clone())
                    .or_else(|| world.get::<crate::MeshRenderer>(entity).map(|m| m.mesh.clone()));
                match (map.entry(&receiver.entry), source) {
                    (Some(entry), Some(mesh)) => {
                        receiver_freshness(map, entry, &mesh.vertices, &mesh.indices)
                    }
                    (None, _) => ReceiverFreshness::Stale,
                    (Some(_), None) => continue,
                }
            }
        };
        match freshness {
            ReceiverFreshness::Fresh => {}
            ReceiverFreshness::Stale => counts.stale_instances += 1,
            ReceiverFreshness::Unverified => counts.unverified_instances += 1,
        }
    }
    counts
}

/// Most receiver errors [`LightmapReceiverReport::errors`] keeps.
pub const MAX_LIGHTMAP_RECEIVER_ERRORS: usize = 32;

/// One [`LightmapReceiver`] [`apply_lightmap_receivers`] could not apply.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct LightmapReceiverError {
    /// The entity (`Entity::to_bits`).
    pub entity: u64,
    /// The entry it names.
    pub entry: String,
    /// Agent code: `lightmap_entry` (the lightmap has no such entry), `lightmap_layout` (the mesh
    /// is not the one the entry was baked for: re-bake), `lightmap_receiver_lod` (a
    /// [`crate::MeshLodChain`] draws other index lists; a receiver draws LOD 0 only),
    /// `lightmap_receiver_stale` (the mesh moved since the bake and
    /// [`StaleReceiverPolicy::FallBack`] is set: re-bake).
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

/// What [`apply_lightmap_receivers`] did in the last frame (`render.get_lightmaps` `receivers`).
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct LightmapReceiverReport {
    /// Entities with a [`LightmapReceiver`].
    pub receivers: usize,
    /// Of those, drawn from their entry's unwrap.
    pub applied: usize,
    /// The ones it could not apply (at most [`MAX_LIGHTMAP_RECEIVER_ERRORS`]).
    pub errors: Vec<LightmapReceiverError>,
    /// With the profile's visibility buffer on: applied receivers whose clusters from the bake the
    /// renderer holds (ADR-2107). [`LightmapForwardStats::visbuffer`] says how many it drew there.
    pub clusters: usize,
    /// With the profile's visibility buffer on: applied receivers without: the file has none or
    /// they do not fit (bake with `--meshlets`), or the material cannot draw there. They draw on
    /// the forward paths.
    pub no_clusters: usize,
    /// Receivers whose mesh moved since the bake, drawn with the frame's other light under
    /// [`StaleReceiverPolicy::FallBack`] (each also in [`Self::errors`], `lightmap_receiver_stale`).
    pub stale_fallback: usize,
}

/// Agent code of a receiver [`StaleReceiverPolicy::FallBack`] took off its lightmap.
pub const LIGHTMAP_RECEIVER_STALE: &str = "lightmap_receiver_stale";

/// A receiver falling back under [`StaleReceiverPolicy::FallBack`]: the mesh it was found stale on,
/// so the hash is taken once and not per frame. Dropped when the mesh, the entry, the lightmap or
/// the policy changes, and the receiver is then checked again.
#[derive(Component, Clone, Debug)]
pub struct StaleLightmapFallback {
    mesh: Arc<crate::CpuMesh>,
    map: std::sync::Weak<Lightmap>,
    name: String,
}

/// The resources [`apply_lightmap_receivers`] reads.
type ReceiverInputs<'w> = (
    Option<Res<'w, InstalledLightmap>>,
    Option<Res<'w, LightmapSettings>>,
    Option<Res<'w, crate::RenderSettings>>,
);

/// A receiver entity as [`apply_lightmap_receivers`] reads it.
type ReceiverItem = (
    Entity,
    &'static LightmapReceiver,
    &'static mut crate::MeshRenderer,
    Option<&'static AppliedLightmapReceiver>,
    Option<&'static crate::MeshUv1>,
    Has<crate::MeshLodChain>,
    Option<&'static StaleLightmapFallback>,
);

/// An entity whose [`LightmapReceiver`] was removed after it was applied.
type ReleasedItem = (
    Entity,
    &'static mut crate::MeshRenderer,
    &'static AppliedLightmapReceiver,
);

/// Puts the entity's own mesh and second UV set back and drops the bookkeeping.
fn restore_receiver(
    commands: &mut Commands,
    entity: Entity,
    mesh: &mut crate::MeshRenderer,
    applied: &AppliedLightmapReceiver,
) {
    if Arc::ptr_eq(&mesh.mesh, &applied.receiver) {
        mesh.mesh = applied.source.clone();
        if let Some(uv1) = &applied.source_uv1 {
            commands.entity(entity).insert(uv1.clone());
        } else {
            commands.entity(entity).remove::<crate::MeshUv1>();
        }
    }
    commands.entity(entity).remove::<AppliedLightmapReceiver>();
}

/// The runtime mapping from drawn entities to lightmap entries (ADR-1623; `Render` schedule,
/// before [`crate::extract_frame`]). For each [`LightmapReceiver`] it resolves the entry by name
/// in the installed lightmap, swaps the entity's mesh for the entry's receiver mesh (built once
/// per lightmap: [`receiver_mesh`]) with its UV2 as [`crate::MeshUv1`], and hands the lightmap,
/// its settings and the `(object key, entry)` list to the [`crate::RuntimeRenderer`]. An entity
/// whose component, entry or lightmap goes away gets its own mesh and UV set back. A frame with no
/// receiver (now or in the previous frame) leaves the renderer's lightmap alone, so a host that
/// calls [`crate::FrameRenderer::set_lightmap_receivers`] itself is not overridden.
pub fn apply_lightmap_receivers(
    mut commands: Commands,
    (installed, settings, profile): ReceiverInputs<'_>,
    renderer: Option<ResMut<crate::RuntimeRenderer>>,
    mut report: ResMut<LightmapReceiverReport>,
    mut receivers: Query<ReceiverItem>,
    mut released: Query<ReleasedItem, Without<LightmapReceiver>>,
    mut held_clusters: Local<ReceiverClusterMap>,
) {
    let _span = tracing::info_span!("apply_lightmap_receivers").entered();
    for (entity, mut mesh, applied) in &mut released {
        restore_receiver(&mut commands, entity, &mut mesh, applied);
    }
    let had_receivers = report.receivers > 0;
    let map = installed.as_deref().and_then(|i| i.lightmap.clone());
    let mut list: Vec<(u64, u32)> = Vec::new();
    let mut errors: Vec<LightmapReceiverError> = Vec::new();
    let mut count = 0usize;
    // Clusters are kept only while the profile can draw them, so a frame without the visibility
    // buffer does none of this.
    let want_clusters = profile.is_some_and(|p| p.profile.visibility_buffer);
    let fall_back = settings
        .as_deref()
        .is_some_and(|s| s.stale_receivers == StaleReceiverPolicy::FallBack);
    let mut stale_fallback = 0usize;
    let mut wanted: Vec<WantedClusters> = Vec::new();
    let mut on_visibility = 0usize;
    let mut clusters = 0usize;
    for (entity, receiver, mut mesh, applied, uv1, has_lod, fallen) in &mut receivers {
        count += 1;
        let current = applied.filter(|a| Arc::ptr_eq(&mesh.mesh, &a.receiver));
        // Already applied from this lightmap under the same entry name: the stored index is the
        // one `entry` returned when the receiver was built. Skip the name walk. A lod chain still
        // falls through so it is reported and restored.
        // A receiver falling back (or one the stale policy now sends back) takes the slow path.
        if !has_lod
            && fallen.is_none()
            && let Some(installed) = map.as_ref()
            && let Some(a) = current
            && !(fall_back && a.freshness == ReceiverFreshness::Stale)
            && a.map.ptr_eq(&Arc::downgrade(installed))
            && a.name == receiver.entry
        {
            list.push((entity.to_bits(), a.entry));
            if want_clusters {
                on_visibility += 1;
                if clusters_can_draw(&mesh.material) {
                    wanted.push(WantedClusters {
                        entity: entity.to_bits(),
                        mesh: a.receiver.clone(),
                        entry: a.entry,
                    });
                }
            }
            continue;
        }
        let mut fail = |code: &'static str, message: String| {
            if errors.len() < MAX_LIGHTMAP_RECEIVER_ERRORS {
                errors.push(LightmapReceiverError {
                    entity: entity.to_bits(),
                    entry: receiver.entry.clone(),
                    code,
                    message,
                });
            }
        };
        let resolved = match map.as_ref() {
            None => None,
            Some(_) if has_lod => {
                fail(
                    "lightmap_receiver_lod",
                    "a MeshLodChain draws other index lists; a lightmap receiver draws LOD 0 only".into(),
                );
                None
            }
            Some(map) => match map.entry(&receiver.entry) {
                Some(entry) => Some((map, entry)),
                None => {
                    fail(
                        "lightmap_entry",
                        format!("the installed lightmap has no entry `{}`", receiver.entry),
                    );
                    None
                }
            },
        };
        let Some((map, entry)) = resolved else {
            if let Some(a) = applied {
                restore_receiver(&mut commands, entity, &mut mesh, a);
            }
            continue;
        };
        let Ok(index) = u32::try_from(entry) else {
            continue;
        };
        let stale_message = || {
            format!(
                "the mesh moved since the bake of entry `{}`; drawn with the frame's other light until a re-bake",
                receiver.entry
            )
        };
        // Already falling back: nothing changed, so no hash and no mesh are redone.
        if let Some(f) = fallen {
            if fall_back
                && f.map.ptr_eq(&Arc::downgrade(map))
                && f.name == receiver.entry
                && Arc::ptr_eq(&mesh.mesh, &f.mesh)
            {
                stale_fallback += 1;
                fail(LIGHTMAP_RECEIVER_STALE, stale_message());
                continue;
            }
            commands.entity(entity).remove::<StaleLightmapFallback>();
        }
        if let Some(a) = current
            && a.map.ptr_eq(&Arc::downgrade(map))
            && a.name == receiver.entry
        {
            // The policy changed under an applied receiver.
            if fall_back && a.freshness == ReceiverFreshness::Stale {
                restore_receiver(&mut commands, entity, &mut mesh, a);
                commands.entity(entity).insert(StaleLightmapFallback {
                    mesh: a.source.clone(),
                    map: Arc::downgrade(map),
                    name: receiver.entry.clone(),
                });
                stale_fallback += 1;
                fail(LIGHTMAP_RECEIVER_STALE, stale_message());
                continue;
            }
            list.push((entity.to_bits(), a.entry));
            if want_clusters {
                on_visibility += 1;
                if clusters_can_draw(&mesh.material) {
                    wanted.push(WantedClusters {
                        entity: entity.to_bits(),
                        mesh: a.receiver.clone(),
                        entry: a.entry,
                    });
                }
            }
            continue;
        }
        // The entity's own mesh: the one a receiver mesh replaced, unless the host has put a new
        // mesh on it since.
        let (source, source_uv1) = match current {
            Some(a) => (a.source.clone(), a.source_uv1.clone()),
            None => (mesh.mesh.clone(), uv1.cloned()),
        };
        // A counted mesh drawn from its upload has no CPU vertices to remap (ADR-7330): it keeps
        // its stand-in and takes the entry's UV2 as its second UV set when the unwrap is 1:1 with
        // the upload. No clusters: the visibility buffer needs CPU vertices.
        if let Some(id) = crate::runtime::counted_mesh_id(&source) {
            let layout = renderer
                .as_deref()
                .and_then(|r| r.0.asset_meshes())
                .and_then(|uploads| uploads.layout(id));
            match counted_receiver_uv2(map, entry, layout) {
                Ok((uv2, freshness)) => {
                    commands.entity(entity).insert((
                        crate::MeshUv1(Arc::new(uv2)),
                        AppliedLightmapReceiver {
                            source: source.clone(),
                            source_uv1,
                            receiver: source,
                            map: Arc::downgrade(map),
                            entry: index,
                            name: receiver.entry.clone(),
                            freshness,
                        },
                    ));
                    list.push((entity.to_bits(), index));
                }
                Err((code, message)) => {
                    fail(code, message);
                    if let Some(a) = applied {
                        restore_receiver(&mut commands, entity, &mut mesh, a);
                    }
                }
            }
            continue;
        }
        match receiver_mesh(map, entry, &source.vertices, &source.indices) {
            Ok(built) => {
                let freshness = receiver_freshness(map, entry, &source.vertices, &source.indices);
                if fall_back && freshness == ReceiverFreshness::Stale {
                    if let Some(a) = applied {
                        restore_receiver(&mut commands, entity, &mut mesh, a);
                    }
                    commands.entity(entity).insert(StaleLightmapFallback {
                        mesh: source,
                        map: Arc::downgrade(map),
                        name: receiver.entry.clone(),
                    });
                    stale_fallback += 1;
                    fail(LIGHTMAP_RECEIVER_STALE, stale_message());
                    continue;
                }
                let drawn = Arc::new(crate::CpuMesh {
                    vertices: built.vertices,
                    indices: built.indices,
                });
                mesh.mesh = drawn.clone();
                if want_clusters {
                    on_visibility += 1;
                    if clusters_can_draw(&mesh.material) {
                        wanted.push(WantedClusters {
                            entity: entity.to_bits(),
                            mesh: drawn.clone(),
                            entry: index,
                        });
                    }
                }
                commands.entity(entity).insert((
                    crate::MeshUv1(Arc::new(built.uv2)),
                    AppliedLightmapReceiver {
                        source,
                        source_uv1,
                        receiver: drawn,
                        map: Arc::downgrade(map),
                        entry: index,
                        name: receiver.entry.clone(),
                        freshness,
                    },
                ));
                list.push((entity.to_bits(), index));
            }
            Err(e) => {
                fail(e.code(), e.to_string());
                if let Some(a) = applied {
                    restore_receiver(&mut commands, entity, &mut mesh, a);
                }
            }
        }
    }
    let applied = list.len();
    if (count > 0 || had_receivers)
        && let Some(mut renderer) = renderer
    {
        let settings = settings.as_deref().copied().unwrap_or_default();
        clusters = sync_receiver_clusters(&mut renderer.0, map.as_ref(), &wanted, &mut held_clusters);
        renderer.0.set_lightmap(map, settings);
        renderer.0.set_lightmap_receivers(&list);
    }
    *report = LightmapReceiverReport {
        receivers: count,
        applied,
        errors,
        stale_fallback,
        clusters,
        no_clusters: on_visibility.saturating_sub(clusters),
    };
}

/// [`counted_receiver_uv2`]'s result: the UV2 and freshness, or an agent code and a message.
type CountedReceiverUv2 = Result<(Vec<[f32; 2]>, ReceiverFreshness), (&'static str, String)>;

/// The UV2 of a counted receiver (ADR-7330): the entry's unwrap when it is 1:1 with the uploaded
/// mesh (the identity `xref`, the same vertex count and the same triangle list, compared by
/// hash), and the receiver's freshness from the upload's source hash. Errors carry an agent code:
/// `lightmap_receiver_counted_pending` (no upload yet) or `lightmap_receiver_counted` (the unwrap
/// splits or reorders the mesh: a counted mesh has no CPU vertices to remap).
fn counted_receiver_uv2(
    map: &Lightmap,
    entry: usize,
    layout: Option<crate::asset_upload::AssetMeshLayout>,
) -> CountedReceiverUv2 {
    let Some(layout) = layout else {
        return Err((
            "lightmap_receiver_counted_pending",
            "the counted mesh has no GPU upload yet (still loading, or no uploader: add CountedMeshPlugin)"
                .into(),
        ));
    };
    let (Some(e), Some(unwrap)) = (map.entries.get(entry), map.unwraps.get(entry)) else {
        return Err(("lightmap_layout", format!("no entry {entry}")));
    };
    let one_to_one = e.source_vertices as usize == layout.vertex_count
        && unwrap.xref.len() == layout.vertex_count
        && unwrap.uv.len() == layout.vertex_count
        && unwrap.xref.iter().enumerate().all(|(i, &x)| x as usize == i)
        && crate::asset_upload::index_list_hash(&unwrap.indices) == layout.index_hash;
    if !one_to_one {
        return Err((
            "lightmap_receiver_counted",
            format!(
                "entry `{}`: its unwrap splits or reorders the counted mesh's vertices, which have no CPU copy to \
                 remap; load the mesh with keep_cpu_data",
                e.name
            ),
        ));
    }
    let freshness = match map.source_hash(entry) {
        None => ReceiverFreshness::Unverified,
        Some(baked) if baked == layout.source_hash => ReceiverFreshness::Fresh,
        Some(_) => ReceiverFreshness::Stale,
    };
    Ok((unwrap.uv.clone(), freshness))
}

/// The parameter uniform (`LightmapParams` in `shaders/lightmap.wgsl`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct LightmapParamsGpu {
    /// x: intensity / exposure, y: 0 when off, 1 when enabled, 2 when enabled and the lightmap holds
    /// the direct sun (`sun_direct`, ADR-2024: a receiver then drops the live sun's diffuse term),
    /// z: 1 when the directions are used, w: 1 for bicubic filtering.
    pub scale: [f32; 4],
    /// x: 1 while lightmaps are enabled and receivers take their indirect specular from them
    /// ([`LightmapSettings::specular`], ADR-4762); y, z, w: 0.
    pub spec: [f32; 4],
}

impl LightmapParamsGpu {
    /// The parameters for `map` under `settings`. A decode scale that is not finite (a huge
    /// intensity, an exposure that skipped [`Lightmap::validate`]) uploads as 0, so the shader
    /// never multiplies a texel by infinity.
    #[must_use]
    pub fn new(map: &Lightmap, settings: &LightmapSettings) -> Self {
        let decode = settings.intensity / map.exposure;
        Self {
            scale: [
                if decode.is_finite() { decode } else { 0.0 },
                match (
                    settings.enabled,
                    map.bake.contents & LightmapContents::SUN_DIRECT != 0,
                ) {
                    (false, _) => 0.0,
                    (true, false) => 1.0,
                    (true, true) => 2.0,
                },
                if settings.directional && map.is_directional() {
                    1.0
                } else {
                    0.0
                },
                if settings.bicubic { 1.0 } else { 0.0 },
            ],
            spec: [
                if settings.enabled
                    && settings.specular
                    && map.bake.contents & LightmapContents::SUN_DIRECT == 0
                {
                    1.0
                } else {
                    0.0
                },
                0.0,
                0.0,
                0.0,
            ],
        }
    }
}

/// The gradient of the lightmap's radiance field over its mean (`LIGHTMAP_SPEC_GRADIENT` in
/// `shaders/lightmap.wgsl`): a cosine lobe's first moment is 2/3 of its zeroth, so a field
/// `a · (1 + 3/2 · ω · d)` has the irradiance `π · a · (1 + n · d)` the directional rebalance uses.
pub const LIGHTMAP_SPEC_GRADIENT: f32 = 1.5;

/// The radiance (cd/m², linear RGB) a GGX lobe of mean vector `m` (`|m| <= 1`) sees in a
/// receiver's baked light (`lightmap_lobe_radiance` in WGSL, ADR-4762): irradiance `e` (lux, at the
/// baked normal) and the decoded direction `direction` (`[x, y, z, w]` as
/// `regia_asset::lightmap::shade_directional` reads it; zero for a lightmap without directions).
///
/// The directional rebalance `E(n) = e · (1 + n · d) / max(1 + w, 1)` is the irradiance of one
/// radiance field linear in the direction, `L(ω) = e / (π · max(1 + w, 1)) · (1 + 3/2 · ω · d)`.
/// For radiance linear in `ω` a normalized lobe's integral is the radiance at its mean vector, so
/// the unclamped field has this lobe integral exactly. Clamping negative radiance makes it an
/// approximation for strongly directional fields (`|d| > 2/3`). With `d = 0` it
/// is `e / π` for every lobe.
#[must_use]
pub fn lightmap_lobe_radiance(e: [f32; 3], direction: [f32; 4], m: [f32; 3]) -> [f32; 3] {
    let md = m[0] * direction[0] + m[1] * direction[1] + m[2] * direction[2];
    let level = (1.0 + LIGHTMAP_SPEC_GRADIENT * md).max(0.0) / (1.0 + direction[3]).max(1.0);
    e.map(|c| c * level * std::f32::consts::FRAC_1_PI)
}

/// The GGX lobe's mean vector the lite shader uses (`lightmap_spec_lobe` in WGSL, ADR-4762): for
/// unit normal `n`, unit mirror direction `r` and GGX `alpha`, the direction leans from `n` to `r`
/// by `(1 − α)(√(1 − α) + α)` (Lagarde & de Rousiers 2014, § 4.9.3) and the length is the ADR-1103
/// fit of the lobe's mean-vector length at `n · v = 1`. The full shader uses the view-dependent
/// `probe_spec_lobe` instead.
#[must_use]
pub fn lightmap_spec_lobe(n: [f32; 3], r: [f32; 3], alpha: f32) -> [f32; 3] {
    let a = alpha.clamp(0.0, 1.0);
    let x = 1.0 - a;
    let t = x * (x.sqrt() + a);
    let d: [f32; 3] = std::array::from_fn(|k| n[k] + (r[k] - n[k]) * t);
    let len = (1.0 + a * (3.673_155 + 7.571_242 * a)) / (1.0 + a * (3.719_715 + 15.010_073 * a));
    let norm = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).max(1e-12).sqrt();
    d.map(|c| c / norm * len)
}

/// The baked diffuse share left after single-scatter split-sum specular reflects its share of a uniform
/// field. `ab` is the BRDF table's scale and bias at the receiver's view angle and roughness.
/// Mirrors `lightmap_diffuse_weight` in WGSL; each channel stays in `0..=1`. The full shader and
/// the lite shader (ADR-5945) additionally reserve the material's optional multi-scatter share
/// ([`crate::hard_surface::multi_scatter_lobe`], at most this remainder) from it and add that
/// share lit by the same baked light.
#[must_use]
pub fn lightmap_diffuse_weight(f0: [f32; 3], ab: [f32; 2]) -> [f32; 3] {
    f0.map(|f| 1.0 - (f * ab[0] + ab[1]).clamp(0.0, 1.0))
}

/// Lambertian outgoing radiance of `albedo` lit by `irradiance` (`lightmap_diffuse` in WGSL).
#[must_use]
pub fn lightmap_diffuse(albedo: [f32; 3], irradiance: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|k| albedo[k] * irradiance[k] * std::f32::consts::FRAC_1_PI)
}

/// How far a receiver mesh's bounding sphere may be from the one its clusters were built with, as a
/// share of its radius ([`receiver_meshlets`]). Rounding only: both come from the same vertices.
const MESHLET_SPHERE_SLACK: f32 = 0.001;

/// What [`apply_lightmap_receivers`] remembers of one receiver's clusters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverClusters {
    /// The receiver mesh's key.
    mesh_key: u64,
    /// The blob decoded, fits the mesh and was handed to the renderer.
    usable: bool,
}

/// The renderer's cluster state of every receiver, by entity ([`apply_lightmap_receivers`]).
pub type ReceiverClusterMap = std::collections::HashMap<u64, ReceiverClusters>;

/// A receiver's clusters as `map` holds them for `entry` (ADR-2107, `regia bake-lightmaps
/// --meshlets`), for [`crate::FrameRenderer::insert_meshlets`] under the key of the receiver mesh
/// `vertices` belong to ([`receiver_mesh`]). `None` when the bake stored none.
///
/// # Errors
///
/// The blob is not a cluster file this build reads, or does not fit this receiver: other counts, or
/// another object-space sphere (a mesh edited since the bake keeps its counts, and bounds in another
/// space would cull the wrong clusters). A re-bake with `--meshlets` makes it fit.
pub fn receiver_meshlets(
    map: &Lightmap,
    entry: usize,
    vertices: &[crate::LabVertex],
) -> Option<Result<crate::visbuffer::MeshletMesh, String>> {
    let blob = map.entry_meshlets(entry)?;
    let unwrap = map.unwraps.get(entry)?;
    let decoded = crate::visbuffer::MeshletMesh::from_bytes(blob).map_err(|e| e.to_string());
    Some(decoded.and_then(|mesh| {
        let fits = mesh.source_vertex_count as usize == unwrap.xref.len()
            && mesh.source_vertex_count as usize == vertices.len()
            && mesh.source_index_count as usize == unwrap.indices.len();
        if !fits {
            return Err(format!(
                "clusters for {} vertices / {} indices, the unwrap has {} / {}",
                mesh.source_vertex_count,
                mesh.source_index_count,
                unwrap.xref.len(),
                unwrap.indices.len()
            ));
        }
        let sphere = crate::visbuffer::sphere_of_points(vertices.iter().map(|v| v.position));
        let slack = sphere[3].max(1e-3) * MESHLET_SPHERE_SLACK;
        let moved = (0..3)
            .map(|k| (sphere[k] - mesh.bounds[k]).powi(2))
            .sum::<f32>()
            .sqrt();
        // Written so a NaN in a stored sphere is refused too.
        if !(moved <= slack && (sphere[3] - mesh.bounds[3]).abs() <= slack) {
            return Err(format!(
                "clusters bound {:?}, the mesh {sphere:?}: it changed since the bake",
                mesh.bounds
            ));
        }
        mesh.validate().map_err(|e| e.to_string())?;
        if mesh.meshlets.is_empty() {
            return Err("the blob holds no clusters".to_string());
        }
        Ok(mesh)
    }))
}

/// The renderer can draw `material` as a visibility-buffer receiver: what `FrameRenderer`'s own gate
/// asks of a material. Clusters for any other would sit unused, and turn the path on for nothing.
fn clusters_can_draw(material: &crate::LabMaterial) -> bool {
    crate::hair::effective_alpha_mode(material) == crate::ALPHA_OPAQUE
        && material.transmission <= 0.0
        && material.surface_graph.is_none()
        && !material.thin_film.is_on()
}

/// One receiver that should hold its clusters this frame ([`apply_lightmap_receivers`]).
struct WantedClusters {
    /// The entity (`Entity::to_bits`).
    entity: u64,
    /// The receiver mesh it draws.
    mesh: Arc<crate::CpuMesh>,
    /// Its entry index.
    entry: u32,
}

/// Keeps the renderer's clusters in step with `wanted`: a new or changed receiver mesh gets its
/// bake's clusters, a receiver that left gives them back. A frame in which nothing changed costs
/// one lookup per receiver. Returns the receivers whose clusters the renderer holds now.
fn sync_receiver_clusters(
    renderer: &mut crate::FrameRenderer,
    map: Option<&Arc<Lightmap>>,
    wanted: &[WantedClusters],
    held: &mut ReceiverClusterMap,
) -> usize {
    let mut holding = 0;
    for w in wanted {
        let mesh_key = crate::mesh_key(&w.mesh);
        if let Some(h) = held.get(&w.entity)
            && h.mesh_key == mesh_key
        {
            holding += usize::from(h.usable && renderer.has_meshlets(mesh_key));
            continue;
        }
        if let Some(old) = held.remove(&w.entity) {
            renderer.remove_meshlets(old.mesh_key);
        }
        let decoded = map.and_then(|m| receiver_meshlets(m, w.entry as usize, &w.mesh.vertices));
        let usable = match decoded {
            Some(Ok(mesh)) => {
                renderer.insert_meshlets(mesh_key, Arc::new(mesh));
                true
            }
            Some(Err(message)) => {
                tracing::warn!(
                    target: "regia_render",
                    entry = w.entry,
                    code = "lightmap_meshlets",
                    "lightmap clusters not used ({message}); the receiver draws in the batches"
                );
                false
            }
            None => false,
        };
        held.insert(w.entity, ReceiverClusters { mesh_key, usable });
        holding += usize::from(usable);
    }
    if held.len() != wanted.len() {
        let keep: std::collections::HashSet<u64> = wanted.iter().map(|w| w.entity).collect();
        held.retain(|entity, h| {
            let stays = keep.contains(entity);
            if !stays {
                renderer.remove_meshlets(h.mesh_key);
            }
            stays
        });
    }
    holding
}

/// A lightmap resident on the GPU.
#[derive(Debug)]
pub struct LightmapGpu {
    /// The atlas (`Rgb9e5Ufloat`, or the file's BC6H / ASTC HDR blocks where the device samples
    /// them; one mip), one array layer per page (ADR-3300).
    pub texture: wgpu::Texture,
    /// Its view in the binding form ([`lightmap_view_dimension`]): every layer as an array, or
    /// with 2-D bindings layer 0.
    pub view: wgpu::TextureView,
    /// Every layer as a 2-D array view (ADR-3300), whatever the binding form.
    pub array_view: wgpu::TextureView,
    /// The direction texture's layers as a 2-D array view.
    pub direction_array_view: wgpu::TextureView,
    /// Pages uploaded (array layers of both textures, 1 for the neutral direction texel).
    pub pages: u32,
    /// Linear, clamp-to-edge sampler.
    pub sampler: wgpu::Sampler,
    /// The [`LightmapParamsGpu`] uniform.
    pub params: wgpu::Buffer,
    /// [`Self::bind_group_layout`] for `visibility`.
    pub layout: wgpu::BindGroupLayout,
    /// Atlas, sampler, parameters and directions, bindings 0..=3.
    pub bind_group: wgpu::BindGroup,
    /// The directions (`Rgba8Unorm`, the atlas size and pages; 1×1 for a lightmap without
    /// directions).
    pub direction_texture: wgpu::Texture,
    /// Its view in the binding form, as [`Self::view`].
    pub direction_view: wgpu::TextureView,
    /// The uploaded lightmap has directions.
    pub directional: bool,
    /// The atlas went up as the file's blocks (ADR-2314), not as the decoded RGB9E5 texels.
    pub native_blocks: bool,
}

/// The texture format and blocks of `map`'s block-compressed texels when `features` let the
/// device sample them and the atlas is whole blocks (wgpu's rule for a compressed level 0);
/// `None` uploads the decoded RGB9E5 texels instead.
#[must_use]
pub fn lightmap_native_blocks(
    map: &Lightmap,
    features: wgpu::Features,
) -> Option<(wgpu::TextureFormat, &[u8])> {
    use regia_asset::lightmap::LightmapTexelEncoding;
    let blocks = map.blocks.as_ref()?;
    let format = match blocks.encoding {
        LightmapTexelEncoding::Bc6h => wgpu::TextureFormat::Bc6hRgbUfloat,
        LightmapTexelEncoding::AstcHdr => wgpu::TextureFormat::Astc {
            block: wgpu::AstcBlock::B4x4,
            channel: wgpu::AstcChannel::Hdr,
        },
        LightmapTexelEncoding::Rgb9e5 => return None,
    };
    let whole = map.width.is_multiple_of(4) && map.height.is_multiple_of(4);
    (whole && features.contains(format.required_features())).then_some((format, blocks.data.as_slice()))
}

/// The texture format and blocks of `map`'s block-compressed directions (ADR-2635) when
/// `features` let the device sample them and the atlas is whole blocks; `None` uploads their RGBA8
/// decode ([`Lightmap::directions`]) instead.
#[must_use]
pub fn lightmap_native_direction_blocks(
    map: &Lightmap,
    features: wgpu::Features,
) -> Option<(wgpu::TextureFormat, &[u8])> {
    use regia_asset::lightmap::LightmapDirectionEncoding;
    if map.directions.is_empty() {
        return None;
    }
    let blocks = map.direction_blocks.as_ref()?;
    let format = match blocks.encoding {
        LightmapDirectionEncoding::Bc7 => wgpu::TextureFormat::Bc7RgbaUnorm,
        LightmapDirectionEncoding::AstcLdr => wgpu::TextureFormat::Astc {
            block: wgpu::AstcBlock::B4x4,
            channel: wgpu::AstcChannel::Unorm,
        },
        LightmapDirectionEncoding::Rgba8 => return None,
    };
    let whole = map.width.is_multiple_of(4) && map.height.is_multiple_of(4);
    (whole && features.contains(format.required_features())).then_some((format, blocks.data.as_slice()))
}

impl LightmapGpu {
    /// The layout of the four bindings `shaders/lightmap.wgsl` reads: the atlas (filterable
    /// float texture in the binding form, [`lightmap_view_dimension`]), the sampler, the parameter
    /// uniform and the directions (the same form).
    #[must_use]
    pub fn bind_group_layout(d: &wgpu::Device, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayout {
        Self::bind_group_layout_for(d, visibility, LIGHTMAP_PAGED_BINDINGS)
    }

    /// [`Self::bind_group_layout`] with 2-D (`paged` false) or texture-array bindings.
    #[must_use]
    pub fn bind_group_layout_for(
        d: &wgpu::Device,
        visibility: wgpu::ShaderStages,
        paged: bool,
    ) -> wgpu::BindGroupLayout {
        let dimension = if paged {
            wgpu::TextureViewDimension::D2Array
        } else {
            wgpu::TextureViewDimension::D2
        };
        d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lightmap bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: dimension,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<LightmapParamsGpu>() as u64
                        ),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: dimension,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        })
    }

    /// Uploads `map`: its texel section goes to the texture as stored (RGB9E5 is a sampleable,
    /// filterable format on every backend; BC6H and ASTC HDR blocks where the device samples them,
    /// [`lightmap_native_blocks`], else their CPU decode), and the parameters follow `settings`.
    #[must_use]
    pub fn upload(
        device: &crate::RenderDevice,
        map: &Lightmap,
        settings: &LightmapSettings,
        visibility: wgpu::ShaderStages,
    ) -> Self {
        Self::upload_with(device, map, settings, visibility, true)
    }

    /// [`Self::upload`]; `directions` false leaves the direction section off the GPU (the neutral
    /// texel stands in, as for a lightmap without directions). `settings` must then have
    /// [`LightmapSettings::directional`] off.
    pub(crate) fn upload_with(
        device: &crate::RenderDevice,
        map: &Lightmap,
        settings: &LightmapSettings,
        visibility: wgpu::ShaderStages,
        directions: bool,
    ) -> Self {
        let pages = map.pages();
        let _span =
            tracing::info_span!("lightmap_upload", width = map.width, height = map.height, pages).entered();
        if pages > 1 && !LIGHTMAP_PAGED_BINDINGS {
            tracing::warn!(
                target: "regia_render",
                pages,
                "a lightmap of {pages} pages is installed, but this build's frame binds one 2-D atlas \
                 (ADR-3300 frame patch not applied): receivers on pages 1 and later draw page 0's texels"
            );
        }
        let d = &device.device;
        let size = wgpu::Extent3d {
            width: map.width,
            height: map.height,
            depth_or_array_layers: pages,
        };
        let native = lightmap_native_blocks(map, d.features());
        let (format, data, bytes_per_row, rows) = match native {
            Some((format, blocks)) => (format, blocks, 16 * (map.width / 4), map.height / 4),
            None => (
                wgpu::TextureFormat::Rgb9e5Ufloat,
                bytemuck::cast_slice::<u32, u8>(&map.texels),
                4 * map.width,
                map.height,
            ),
        };
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("lightmap atlas"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        device.queue.write_texture(
            texture.as_image_copy(),
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(rows),
            },
            size,
        );
        let (view, array_view) = binding_views(&texture);
        let directional = directions && map.is_directional();
        // Without directions a neutral 1×1 texel (no direction, w = 0) keeps the layout whole;
        // the shader does not read it (`scale.z` is 0).
        let neutral = [encode_direction([0.0; 4])];
        // The file's BC7 / ASTC LDR direction blocks where the device samples them (ADR-2635).
        let native_dirs = if directional {
            lightmap_native_direction_blocks(map, d.features())
        } else {
            None
        };
        let (dir_size, dir_format, dir_data, dir_row, dir_rows) = match native_dirs {
            Some((format, blocks)) => (size, format, blocks, 16 * (map.width / 4), map.height / 4),
            None if directional => (
                size,
                wgpu::TextureFormat::Rgba8Unorm,
                bytemuck::cast_slice::<u32, u8>(&map.directions),
                4 * map.width,
                map.height,
            ),
            None => (
                wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                wgpu::TextureFormat::Rgba8Unorm,
                bytemuck::cast_slice::<u32, u8>(&neutral[..]),
                4,
                1,
            ),
        };
        let direction_texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("lightmap directions"),
            size: dir_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: dir_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        device.queue.write_texture(
            direction_texture.as_image_copy(),
            dir_data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(dir_row),
                rows_per_image: Some(dir_rows),
            },
            dir_size,
        );
        let (direction_view, direction_array_view) = binding_views(&direction_texture);
        let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("lightmap sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lightmap params"),
            size: std::mem::size_of::<LightmapParamsGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        device.queue.write_buffer(
            &params,
            0,
            bytemuck::bytes_of(&LightmapParamsGpu::new(map, settings)),
        );
        let layout = Self::bind_group_layout(d, visibility);
        let bind_group =
            crate::device::TrackedDevice::observe(d).create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("lightmap"),
                layout: &layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&direction_view),
                    },
                ],
            });
        Self {
            texture,
            view,
            array_view,
            direction_array_view,
            pages,
            sampler,
            params,
            layout,
            bind_group,
            direction_texture,
            direction_view,
            directional,
            native_blocks: native.is_some(),
        }
    }

    /// The directions went up as the file's BC7 / ASTC LDR blocks (ADR-2635), not as their RGBA8
    /// decode.
    #[must_use]
    pub fn native_direction_blocks(&self) -> bool {
        self.direction_texture.format() != wgpu::TextureFormat::Rgba8Unorm
    }

    /// Rewrites the parameters after a settings change.
    pub fn update_params(&self, device: &crate::RenderDevice, map: &Lightmap, settings: &LightmapSettings) {
        device.queue.write_buffer(
            &self.params,
            0,
            bytemuck::bytes_of(&LightmapParamsGpu::new(map, settings)),
        );
    }

    /// Bytes of the atlas texture (4 a texel, 1 for native blocks), and of the direction texture
    /// (4 a texel, 1 for native blocks) when the lightmap has directions, over every page.
    #[must_use]
    pub fn gpu_bytes(&self) -> u64 {
        let s = self.texture.size();
        let texels = u64::from(s.width) * u64::from(s.height) * u64::from(s.depth_or_array_layers);
        let atlas = if self.native_blocks { texels } else { texels * 4 };
        let directions = if self.native_direction_blocks() {
            texels
        } else {
            texels * 4
        };
        atlas + if self.directional { directions } else { 0 }
    }
}

/// The binding-form view ([`lightmap_view_dimension`]; layer 0 of a 2-D binding) and the full
/// array view of a lightmap texture.
fn binding_views(texture: &wgpu::Texture) -> (wgpu::TextureView, wgpu::TextureView) {
    let array = texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some("lightmap pages"),
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    let bound = if LIGHTMAP_PAGED_BINDINGS {
        array.clone()
    } else {
        texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("lightmap page 0"),
            dimension: Some(wgpu::TextureViewDimension::D2),
            base_array_layer: 0,
            array_layer_count: Some(1),
            ..Default::default()
        })
    };
    (bound, array)
}

/// Largest atlas edge [`combine_lightmaps`] makes, texels (the 2D texture limit every target
/// meets).
pub const MAX_COMBINED_LIGHTMAP_EDGE: u32 = 8192;

/// Where one source lightmap sits in a [`combine_lightmaps`] result.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct CombinedLightmapRegion {
    /// First atlas row of the source's atlas in the combined one.
    pub row: u32,
    /// Its rows (the source's height).
    pub rows: u32,
    /// Its first entry in the combined entry list.
    pub first_entry: usize,
    /// Its entries.
    pub entries: usize,
    /// Its entries renamed because an earlier source has an entry of that name: `name@<k>`, `k`
    /// the source's index (keyed entries match by key, so a rename only matters to a receiver that
    /// names the entry itself).
    pub renamed: usize,
    /// The combined page of the source's first page (ADR-3300): 0 when the sources stack in one
    /// page, else the pages before it.
    pub page: u32,
    /// The source's pages in the combined lightmap.
    pub pages: u32,
}

/// Several levels' lightmaps as one atlas (ADR-2635): each source's atlas is stacked below the
/// previous one (left-aligned; the combined width is the widest), every entry keeps its texels and
/// gets the rectangle and `scale_offset` of its new place, and the receivers of every source
/// resolve in the result by name or by key. The renderer draws one lightmap, so this is how an
/// additive level's lightmap joins the primary's without a second atlas binding. When the stack
/// would pass [`MAX_COMBINED_LIGHTMAP_EDGE`], or a source has several pages, the sources' pages are
/// appended instead (ADR-3300): every page is the widest source page by the tallest, each source
/// page sits at its top left, and an entry keeps its rectangle on its new page.
///
/// The result is RGB9E5 at the exposure of its brightest texel (block-compressed sources are
/// decoded: the result takes 4 bytes a texel); directional when any source is (a source without
/// directions gets the neutral direction, which leaves its light unchanged); its clusters are the
/// sources' when any has them. Bake settings and density come from the first source. One source
/// returns a copy of it. When every source is one page of the same size and exposure with uniform
/// directionality, the texel, coverage and direction words stack verbatim (no float rebase, so
/// the sources' bits are preserved exactly); otherwise every texel is decoded to float and
/// re-encoded at the combined exposure.
///
/// # Errors
///
/// [`LightmapError::Layout`] when `maps` is empty, the appended pages pass
/// `MAX_LIGHTMAP_PAGES`, or the sources disagree on the direct sun
/// ([`LightmapContents::SUN_DIRECT`]: the shader drops the live sun's diffuse term for every
/// receiver of a lightmap that holds it, so one atlas cannot mix the two).
pub fn combine_lightmaps(
    maps: &[&Lightmap],
) -> Result<(Lightmap, Vec<CombinedLightmapRegion>), LightmapError> {
    let _span = tracing::info_span!("combine_lightmaps", sources = maps.len()).entered();
    let Some(first) = maps.first() else {
        return Err(LightmapError::Layout("no lightmap to combine".into()));
    };
    let sun = first.bake.contents & LightmapContents::SUN_DIRECT;
    if maps
        .iter()
        .any(|m| m.bake.contents & LightmapContents::SUN_DIRECT != sun)
    {
        return Err(LightmapError::Layout(
            "the lightmaps disagree on the direct sun (bake every level with or every level without `--direct-sun`)"
                .into(),
        ));
    }
    let width = maps.iter().map(|m| m.width).max().unwrap_or(0);
    let height: u64 = maps.iter().map(|m| u64::from(m.height)).sum();
    if width > MAX_COMBINED_LIGHTMAP_EDGE
        || height > u64::from(MAX_COMBINED_LIGHTMAP_EDGE)
        || maps.iter().any(|m| m.pages() > 1)
    {
        return combine_as_pages(maps);
    }
    let height = height as u32;
    let texels = width as usize * height as usize;
    // A verbatim stack needs no decode or re-encode: every source is one page of the same size
    // and exposure, so its RGB9E5 words are already `irradiance × the combined exposure`
    // (each source's exposure is `exposure_for` its own peak; equal exposures mean the brightest
    // combined texel picks the same one). Directionality must be uniform (a mixed stack gets the
    // neutral direction, which only the float path fills). Anything else takes the float rebase
    // below, which stays correct for every input.
    let verbatim = maps.iter().all(|m| m.width == width && m.height == height)
        && maps.iter().all(|m| m.exposure == first.exposure)
        && maps.iter().all(|m| m.is_directional() == first.is_directional());
    let mut irradiance = if verbatim {
        Vec::new()
    } else {
        vec![[0.0f32; 3]; texels]
    };
    let mut coverage = if verbatim {
        Vec::new()
    } else {
        vec![regia_asset::lightmap::Coverage::EMPTY; texels]
    };
    let directional = maps.iter().any(|m| m.is_directional());
    let neutral = encode_direction([0.0; 4]);
    let mut directions = if directional && !verbatim {
        vec![neutral; texels]
    } else {
        Vec::new()
    };
    let mut verbatim_texels: Vec<u32> = Vec::new();
    let mut verbatim_coverage: Vec<u8> = Vec::new();
    let mut verbatim_directions: Vec<u32> = Vec::new();
    if verbatim {
        verbatim_texels = Vec::with_capacity(texels);
        verbatim_coverage = Vec::with_capacity(texels);
        verbatim_directions = Vec::with_capacity(if directional { texels } else { 0 });
    }
    let with_clusters = maps.iter().any(|m| !m.meshlets.is_empty());
    let mut entries = Vec::new();
    let mut unwraps = Vec::new();
    let mut meshlets = Vec::new();
    let mut regions = Vec::with_capacity(maps.len());
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Every source's own names, so a rename never takes a name a later source holds literally.
    let literal: std::collections::HashSet<&str> = maps
        .iter()
        .flat_map(|m| m.entries.iter().map(|e| e.name.as_str()))
        .collect();
    let mut row = 0u32;
    for (k, map) in maps.iter().enumerate() {
        if verbatim {
            // Same size and exposure: the words, coverage and directions move as they are.
            verbatim_texels.extend_from_slice(&map.texels);
            verbatim_coverage.extend_from_slice(&map.coverage);
            if directional {
                verbatim_directions.extend_from_slice(&map.directions);
            }
        } else {
            for y in 0..map.height {
                for x in 0..map.width {
                    let at = ((row + y) * width + x) as usize;
                    let src = (y * map.width + x) as usize;
                    irradiance[at] = map.irradiance_at(x, y);
                    coverage[at] = map.coverage.get(src).copied().unwrap_or_default();
                    if directional && map.is_directional() {
                        directions[at] = map.directions.get(src).copied().unwrap_or(neutral);
                    }
                }
            }
        }
        let (sx, sy) = (map.width as f32 / width as f32, map.height as f32 / height as f32);
        let oy = row as f32 / height as f32;
        let mut renamed = 0;
        let first_entry = entries.len();
        for (i, entry) in map.entries.iter().enumerate() {
            let mut e = entry.clone();
            e.rect[1] += row;
            let so = entry.scale_offset;
            e.scale_offset = [so[0] * sx, so[1] * sy, so[2] * sx, so[3] * sy + oy];
            if !names.insert(e.name.clone()) {
                e.name = unique_name(&names, &literal, &e.name, k);
                names.insert(e.name.clone());
                renamed += 1;
            }
            entries.push(e);
            unwraps.push(map.unwraps.get(i).cloned().unwrap_or_default());
            if with_clusters {
                meshlets.push(map.meshlets.get(i).cloned().unwrap_or_default());
            }
        }
        regions.push(CombinedLightmapRegion {
            row,
            rows: map.height,
            first_entry,
            entries: map.entries.len(),
            renamed,
            page: 0,
            pages: 1,
        });
        row += map.height;
    }
    let mut combined = if verbatim {
        finish_combined(
            first,
            maps,
            width,
            height,
            first.exposure,
            VerbatimSections {
                texels: verbatim_texels,
                coverage: verbatim_coverage,
                directions: verbatim_directions,
                entries,
                unwraps,
                meshlets,
            },
        )?
    } else {
        let mut bake = first.bake;
        bake.contents = maps.iter().fold(0, |bits, m| bits | m.bake.contents);
        let mut combined = Lightmap::from_irradiance(
            regia_asset::lightmap::LightmapLayout {
                width,
                height,
                density: first.density,
                padding: first.padding,
                entries,
                unwraps,
            },
            &irradiance,
            coverage,
            bake,
        )?;
        combined.directions = directions;
        combined.meshlets = meshlets;
        combined.denoise = first.denoise;
        combined
    };
    // One list over the stacked entries when any source has hashes (their order is the regions').
    if maps.iter().any(|m| !m.source_hashes.is_empty()) {
        combined.source_hashes = maps
            .iter()
            .flat_map(|m| (0..m.entries.len()).map(|k| m.source_hash(k).unwrap_or(0)))
            .collect();
    }
    combined.validate()?;
    Ok((combined, regions))
}

/// The encoded sections of a verbatim stack ([`combine_lightmaps`]): moved, never rebased to
/// float.
struct VerbatimSections {
    texels: Vec<u32>,
    coverage: Vec<u8>,
    directions: Vec<u32>,
    entries: Vec<regia_asset::lightmap::LightmapEntry>,
    unwraps: Vec<regia_asset::lightmap::MeshUnwrap>,
    meshlets: Vec<Vec<u8>>,
}

/// Builds the combined [`Lightmap`] from already-encoded sections: the common tail of
/// [`combine_lightmaps`] for a path that never rebased to float (the verbatim stack).
fn finish_combined(
    first: &Lightmap,
    maps: &[&Lightmap],
    width: u32,
    height: u32,
    exposure: f32,
    sections: VerbatimSections,
) -> Result<Lightmap, LightmapError> {
    let mut bake = first.bake;
    bake.contents = maps.iter().fold(0, |bits, m| bits | m.bake.contents);
    // `Lightmap` keeps a private entry-lookup cache (regia_asset), so it cannot be built with a
    // struct literal here: start from the first source and replace every public field. The
    // lookup sees the replaced entry list on its next query.
    let mut combined = first.clone();
    combined.width = width;
    combined.height = height;
    combined.exposure = exposure;
    combined.density = first.density;
    combined.padding = first.padding;
    combined.entries = sections.entries;
    combined.unwraps = sections.unwraps;
    combined.bake = bake;
    combined.texels = sections.texels;
    combined.coverage = sections.coverage;
    combined.directions = sections.directions;
    combined.meshlets = sections.meshlets;
    combined.blocks = None;
    combined.denoise = first.denoise;
    combined.direction_blocks = None;
    combined.source_hashes = Vec::new();
    combined.assign_geometry_offsets();
    Ok(combined)
}

/// `name@<k>` for source `k`, or `name@<j>` for the next `j` after `k` no entry holds yet (a source
/// that is itself a combination already has `@<k>` names). Always one `@<digits>` suffix, which a
/// level strips to find the name it wrote (`regia_framework` `own_entry_name`). `literal` holds
/// every source's own entry names: a rename that took one a later source holds literally (`e`
/// renamed `e@1` beside that source's own `e@1`) pushed the literal to `e@1@1`, and the level's
/// exact-match lookup then found the wrong entry.
fn unique_name(
    taken: &std::collections::HashSet<String>,
    literal: &std::collections::HashSet<&str>,
    name: &str,
    k: usize,
) -> String {
    let mut j = k;
    loop {
        let out = format!("{name}@{j}");
        if !taken.contains(&out) && !literal.contains(out.as_str()) {
            return out;
        }
        j += 1;
    }
}

/// [`combine_lightmaps`] by pages (ADR-3300): every source page becomes a page of the result, in
/// source order, at the top left of a page the widest source page by the tallest.
fn combine_as_pages(maps: &[&Lightmap]) -> Result<(Lightmap, Vec<CombinedLightmapRegion>), LightmapError> {
    let _span = tracing::info_span!("combine_lightmap_pages", sources = maps.len()).entered();
    let Some(first) = maps.first() else {
        return Err(LightmapError::Layout("no lightmap to combine".into()));
    };
    let width = maps.iter().map(|m| m.width).max().unwrap_or(1);
    let height = maps.iter().map(|m| m.height).max().unwrap_or(1);
    let pages: u32 = maps.iter().map(|m| m.pages()).sum();
    let limit = regia_asset::lightmap::MAX_LIGHTMAP_PAGES;
    if pages > limit {
        return Err(LightmapError::Layout(format!(
            "the combined lightmap would have {pages} pages, over {limit}"
        )));
    }
    let page_texels = width as usize * height as usize;
    let texels = page_texels * pages as usize;
    let mut irradiance = vec![[0.0f32; 3]; texels];
    let mut coverage = vec![regia_asset::lightmap::Coverage::EMPTY; texels];
    let directional = maps.iter().any(|m| m.is_directional());
    let neutral = encode_direction([0.0; 4]);
    let mut directions = if directional {
        vec![neutral; texels]
    } else {
        Vec::new()
    };
    let with_clusters = maps.iter().any(|m| !m.meshlets.is_empty());
    let (mut entries, mut unwraps, mut meshlets) = (Vec::new(), Vec::new(), Vec::new());
    let mut regions = Vec::with_capacity(maps.len());
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Every source's own names, so a rename never takes a name a later source holds literally.
    let literal: std::collections::HashSet<&str> = maps
        .iter()
        .flat_map(|m| m.entries.iter().map(|e| e.name.as_str()))
        .collect();
    let mut base = 0u32;
    for (k, map) in maps.iter().enumerate() {
        for p in 0..map.pages() {
            for y in 0..map.height {
                for x in 0..map.width {
                    let src = y + p * map.height;
                    let at =
                        ((base + p) as usize * height as usize + y as usize) * width as usize + x as usize;
                    irradiance[at] = map.irradiance_at(x, src);
                    coverage[at] = map.coverage_at(x, src);
                    if directional && map.is_directional() {
                        directions[at] = map
                            .directions
                            .get(src as usize * map.width as usize + x as usize)
                            .copied()
                            .unwrap_or(neutral);
                    }
                }
            }
        }
        let (sx, sy) = (map.width as f32 / width as f32, map.height as f32 / height as f32);
        let (mut renamed, first_entry) = (0, entries.len());
        for (i, entry) in map.entries.iter().enumerate() {
            let mut e = entry.clone();
            e.page += base;
            let so = entry.scale_offset;
            e.scale_offset = [so[0] * sx, so[1] * sy, so[2] * sx, so[3] * sy];
            if !names.insert(e.name.clone()) {
                e.name = unique_name(&names, &literal, &e.name, k);
                names.insert(e.name.clone());
                renamed += 1;
            }
            entries.push(e);
            unwraps.push(map.unwraps.get(i).cloned().unwrap_or_default());
            if with_clusters {
                meshlets.push(map.meshlets.get(i).cloned().unwrap_or_default());
            }
        }
        regions.push(CombinedLightmapRegion {
            row: 0,
            rows: map.height,
            first_entry,
            entries: map.entries.len(),
            renamed,
            page: base,
            pages: map.pages(),
        });
        base += map.pages();
    }
    // The layout's page count comes from its entries (`pages_of`), but a source with no entries
    // still takes one page above: trailing entry-less sources would leave texels for pages the
    // layout does not have, and `from_irradiance` refused the whole combination.
    let used = regia_asset::lightmap::pages_of(&entries) as usize * page_texels;
    irradiance.truncate(used);
    coverage.truncate(used);
    if directional {
        directions.truncate(used);
    }
    let mut bake = first.bake;
    bake.contents = maps.iter().fold(0, |bits, m| bits | m.bake.contents);
    let mut combined = Lightmap::from_irradiance(
        regia_asset::lightmap::LightmapLayout {
            width,
            height,
            density: first.density,
            padding: first.padding,
            entries,
            unwraps,
        },
        &irradiance,
        coverage,
        bake,
    )?;
    combined.directions = directions;
    combined.meshlets = meshlets;
    combined.denoise = first.denoise;
    if maps.iter().any(|m| !m.source_hashes.is_empty()) {
        combined.source_hashes = maps
            .iter()
            .flat_map(|m| (0..m.entries.len()).map(|k| m.source_hash(k).unwrap_or(0)))
            .collect();
    }
    combined.validate()?;
    Ok((combined, regions))
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "forward_mobile.wgsl,lab_forward.wgsl,vis_resolve.wgsl",
        "LightmapParams",
        LightmapParamsGpu { scale, spec }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame_work::counted::*;
    use regia_asset::lightmap::{Coverage, LightmapBakeInfo, LightmapEntry, LightmapLayout, MeshUnwrap};

    /// The cap never turns on what the settings turn off, and lowers what they turn on.
    #[test]
    fn a_tier_caps_the_settings() {
        let on = LightmapSettings {
            enabled: true,
            intensity: 2.0,
            bicubic: true,
            directional: true,
            specular: true,
            ..LightmapSettings::default()
        };
        let off = LightmapSettings {
            bicubic: false,
            directional: false,
            ..on
        };
        assert_eq!(LightmapTier::Full.apply(on), on);
        assert_eq!(LightmapTier::Full.apply(off), off);
        assert_eq!(LightmapTier::Directional.apply(off), off);
        assert_eq!(
            LightmapTier::Directional.apply(on),
            LightmapSettings { bicubic: false, ..on }
        );
        assert_eq!(LightmapTier::Bilinear.apply(on), off);
        assert_eq!(
            LightmapTier::Bilinear.apply(on).intensity,
            2.0,
            "only the lookup changes"
        );
    }

    /// A frame's lite shader lowers the tier to what it reads, and never raises it.
    #[test]
    fn the_lite_shader_lowers_the_tier() {
        use crate::forward_lite::ForwardLite::{High, Low, Off};
        use LightmapTier::{Bilinear, Directional, Full};
        for (asked, lite, expected) in [
            (Full, Off, Full),
            (Directional, Off, Directional),
            (Bilinear, Off, Bilinear),
            (Full, High, Directional),
            (Directional, High, Directional),
            (Bilinear, High, Bilinear),
            (Full, Low, Bilinear),
            (Directional, Low, Bilinear),
            (Bilinear, Low, Bilinear),
        ] {
            assert_eq!(asked.for_frame(lite), expected, "{asked:?} at {lite:?}");
        }
    }

    /// The preset rows: a Mobile tier asks for less, everything else takes what the settings ask.
    #[test]
    fn every_preset_names_its_lookup() {
        use crate::{RenderProfile, RenderProfilePreset};
        for preset in RenderProfilePreset::ALL {
            let expected = match preset {
                RenderProfilePreset::MobileLow => LightmapTier::Bilinear,
                RenderProfilePreset::MobileHigh => LightmapTier::Directional,
                _ => LightmapTier::Full,
            };
            let profile = RenderProfile::from_preset(preset);
            assert_eq!(profile.lightmap_tier, expected, "{preset:?}");
            assert_eq!(
                crate::FrameQuality::from_profile(&profile).lightmap_tier,
                expected,
                "{preset:?}: the frame quality carries it"
            );
        }
    }

    /// A 4×2 atlas with distinct texels, one entry covering it.
    fn small_map() -> Lightmap {
        let irr: Vec<[f32; 3]> = (0..8)
            .map(|i| [10.0 * (i + 1) as f32, 5.0 * i as f32, 1000.0 / (i + 1) as f32])
            .collect();
        Lightmap::from_irradiance(
            LightmapLayout {
                width: 4,
                height: 2,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "e".into(),
                    source_vertices: 3,
                    rect: [0, 0, 4, 2],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2],
                    uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                    indices: vec![0, 1, 2],
                }],
            },
            &irr,
            vec![Coverage::BAKED; 8],
            LightmapBakeInfo {
                contents: LightmapContents::SKY,
                ..LightmapBakeInfo::default()
            },
        )
        .expect("valid")
    }

    /// A source that holds both `e` and a literal `e@1`, combined after another `e`: its `e` is not
    /// renamed onto its own literal `e@1`, and the literal keeps its name (corr-low-render,
    /// corr-render-rest finding 22).
    #[test]
    fn a_rename_never_takes_a_name_a_source_holds_literally() {
        let a = small_map();
        let mut b = small_map();
        let mut literal = b.entries[0].clone();
        b.entries[0].rect = [0, 0, 2, 2];
        literal.rect = [2, 0, 2, 2];
        literal.name = "e@1".into();
        b.entries.push(literal);
        b.unwraps.push(b.unwraps[0].clone());
        b.validate().expect("valid two-entry map");
        let (c, regions) = combine_lightmaps(&[&a, &b]).expect("combine");
        let names: Vec<&str> = c.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names[0], "e");
        assert_eq!(names[2], "e@1", "the literal name is kept: {names:?}");
        assert_ne!(names[1], "e@1", "b's `e` took b's own literal name: {names:?}");
        assert!(
            names[1]
                .strip_prefix("e@")
                .is_some_and(|d| d.bytes().all(|b| b.is_ascii_digit())),
            "one suffix: {names:?}"
        );
        assert_eq!(regions[1].renamed, 1, "{names:?}");
    }

    /// Two lightmaps as one atlas (ADR-2635): the second stacks below the first, every texel and
    /// every entry's lookup land where they were, a clashing name is renamed, directions and
    /// clusters follow, and the sun rule and the size limit refuse.
    #[test]
    fn combine_lightmaps_stacks_atlases_and_keeps_every_lookup() {
        let a = small_map();
        let mut b = small_map();
        // A 2×2 second atlas, brighter (another exposure), directional.
        b.width = 2;
        b.height = 2;
        b.entries[0].rect = [0, 0, 2, 2];
        b.texels = (0..4u32)
            .map(|i| regia_asset::baked_gi::rgb9e5([400.0 * (i + 1) as f32 * b.exposure; 3]))
            .collect();
        b.coverage = vec![Coverage::BAKED; 4];
        b.set_directions(&[[0.0, 1.0, 0.0, 1.0]; 4])
            .expect("one per texel");
        b.validate().expect("valid second map");

        let (c, regions) = combine_lightmaps(&[&a, &b]).expect("combine");
        assert_eq!((c.width, c.height, c.entries.len()), (4, 4, 2));
        assert_eq!(regions[1].row, 2);
        assert_eq!(
            (regions[1].first_entry, regions[1].entries, regions[1].renamed),
            (1, 1, 1)
        );
        assert_eq!(c.entries[0].name, "e");
        assert_eq!(c.entries[1].name, "e@1");
        assert_eq!(c.entry("e@1"), Some(1));
        assert_eq!(c.entries[1].rect, [0, 2, 2, 2]);
        // A combined source already holds `e@1`: the next clash takes `e@2`, still one suffix.
        let (again, _) = combine_lightmaps(&[&c, &b]).expect("combine again");
        let names: Vec<&str> = again.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["e", "e@1", "e@2"]);
        let close =
            |x: [f32; 3], y: [f32; 3]| (0..3).all(|k| (x[k] - y[k]).abs() <= 1e-3 * y[k].abs().max(1.0));
        for (map, row) in [(&a, 0u32), (&b, 2u32)] {
            for y in 0..map.height {
                for x in 0..map.width {
                    assert!(
                        close(c.irradiance_at(x, row + y), map.irradiance_at(x, y)),
                        "texel ({x}, {y}) of the map at row {row}"
                    );
                }
            }
        }
        // Each entry's local UV reaches the same texel centre it did in its own atlas.
        for (entry, map, row) in [(0usize, &a, 0u32), (1, &b, 2)] {
            let local = [0.3, 0.7];
            let own = map.entries[0].scale_offset;
            let new = c.entries[entry].scale_offset;
            let at_own = [
                (local[0] * own[0] + own[2]) * map.width as f32,
                (local[1] * own[1] + own[3]) * map.height as f32 + row as f32,
            ];
            let at_new = [
                (local[0] * new[0] + new[2]) * c.width as f32,
                (local[1] * new[1] + new[3]) * c.height as f32,
            ];
            assert!(
                (at_own[0] - at_new[0]).abs() < 1e-4 && (at_own[1] - at_new[1]).abs() < 1e-4,
                "entry {entry}: {at_own:?} vs {at_new:?}"
            );
        }
        assert!(c.is_directional());
        assert_eq!(
            c.direction_at(0, 0),
            Some(regia_asset::lightmap::decode_direction(encode_direction(
                [0.0; 4]
            ))),
            "the first map had no directions: neutral"
        );
        assert_eq!(c.direction_at(0, 2), b.direction_at(0, 0));
        assert_eq!(
            c.to_bytes()
                .ok()
                .and_then(|bytes| Lightmap::from_bytes(&bytes).ok()),
            Some(c.clone())
        );

        // One source: the same lookups.
        let (one, _) = combine_lightmaps(&[&a]).expect("one");
        assert_eq!(one.texels, a.texels);
        // The direct sun must agree.
        let mut sunny = small_map();
        sunny.bake.contents |= LightmapContents::SUN_DIRECT;
        assert!(combine_lightmaps(&[&a, &sunny]).is_err());
        assert!(combine_lightmaps(&[]).is_err());
        assert_eq!((regions[1].page, regions[1].pages), (0, 1), "stacked in one page");
    }

    /// Same-size, same-exposure sources stack verbatim (no float rebase): the combined words are
    /// the sources' words bit for bit, coverage and directions follow, and every lookup stays
    /// where it was. Uniform directionality also stacks verbatim.
    #[test]
    fn combine_lightmaps_stacks_same_sized_sources_verbatim() {
        let a = small_map();
        let mut b = small_map();
        // Same size and exposure as `a` (its words and encode multiplier are untouched).
        b.texels.swap(0, 5);
        b.texels.swap(1, 6);
        let (c, regions) = combine_lightmaps(&[&a, &b]).expect("combine");
        assert_eq!((c.width, c.height), (4, 4));
        assert_eq!(c.exposure, a.exposure);
        let mut words = a.texels.clone();
        words.extend_from_slice(&b.texels);
        assert_eq!(c.texels, words, "the sources' RGB9E5 words stack verbatim");
        let mut cov = vec![Coverage::BAKED; 8];
        cov.extend_from_slice(&b.coverage);
        assert_eq!(c.coverage, cov);
        assert!(!c.is_directional());
        assert_eq!(c.entries[1].rect, [0, 2, 4, 2]);
        assert_eq!(regions[1].row, 2);
        for (map, row) in [(&a, 0u32), (&b, 2u32)] {
            for y in 0..map.height {
                for x in 0..map.width {
                    assert_eq!(
                        c.irradiance_at(x, row + y),
                        map.irradiance_at(x, y),
                        "texel ({x}, {y}) of the map at row {row}"
                    );
                }
            }
        }
        assert_eq!(
            c.to_bytes()
                .ok()
                .and_then(|bytes| Lightmap::from_bytes(&bytes).ok()),
            Some(c.clone())
        );

        // Uniform directionality stacks verbatim too.
        let mut da = small_map();
        da.set_directions(&[[0.0, 1.0, 0.0, 1.0]; 8])
            .expect("one per texel");
        let mut db = small_map();
        db.set_directions(&[[1.0, 0.0, 0.0, 0.5]; 8])
            .expect("one per texel");
        let (dc, _) = combine_lightmaps(&[&da, &db]).expect("combine directional");
        let mut dir = da.directions.clone();
        dir.extend_from_slice(&db.directions);
        assert_eq!(dc.directions, dir, "the sources' direction words stack verbatim");
        assert_eq!(dc.direction_at(0, 2), db.direction_at(0, 0));
    }

    /// A stack that would pass [`MAX_COMBINED_LIGHTMAP_EDGE`] appends pages instead of refusing
    /// (ADR-3300): the second level's atlas becomes page 1, every entry keeps its rectangle and
    /// reads the same light on its page; paged sources append all their pages, and more than
    /// `MAX_LIGHTMAP_PAGES` pages are refused.
    #[test]
    fn combine_lightmaps_appends_pages_when_one_would_overflow() {
        let a = small_map();
        let mut tall = small_map();
        tall.height = MAX_COMBINED_LIGHTMAP_EDGE;
        tall.entries[0].rect = [0, 0, 4, MAX_COMBINED_LIGHTMAP_EDGE];
        let n = 4 * MAX_COMBINED_LIGHTMAP_EDGE as usize;
        tall.texels = (0..n)
            .map(|i| regia_asset::baked_gi::rgb9e5([(i % 97) as f32 * tall.exposure; 3]))
            .collect();
        tall.coverage = vec![Coverage::BAKED; n];
        tall.validate().expect("valid tall map");
        let (c, regions) = combine_lightmaps(&[&a, &tall]).expect("pages");
        assert_eq!((c.pages(), c.width, c.height), (2, 4, MAX_COMBINED_LIGHTMAP_EDGE));
        assert_eq!((regions[1].page, regions[1].pages, regions[1].row), (1, 1, 0));
        assert_eq!(c.entries[1].page, 1);
        assert_eq!(c.entries[1].rect, tall.entries[0].rect);
        for local in [[0.3, 0.7], [0.6, 0.2], [0.9, 0.9]] {
            for (entry, map) in [(0usize, &a), (1, &tall)] {
                let (got, want) = (c.sample_entry(entry, local), map.sample_entry(0, local));
                assert!(
                    (0..3).all(|k| (got[k] - want[k]).abs() <= 1e-3 * want[k].abs().max(1.0)),
                    "entry {entry} {local:?}: {got:?} vs {want:?}"
                );
            }
        }
        assert_eq!(
            c.to_bytes().ok().and_then(|b| Lightmap::from_bytes(&b).ok()),
            Some(c.clone())
        );
        // A paged source keeps its pages: two levels of two pages make four.
        let (twice, _) = combine_lightmaps(&[&c, &c]).expect("four pages");
        assert_eq!(twice.pages(), 4);
        assert_eq!(twice.entries[3].page, 3);
        // The page limit.
        let mut many = small_map();
        let one = many.entries[0].clone();
        many.entries = (0..regia_asset::lightmap::MAX_LIGHTMAP_PAGES / 2 + 1)
            .map(|p| LightmapEntry {
                name: format!("e{p}"),
                page: p,
                ..one.clone()
            })
            .collect();
        many.unwraps = vec![a.unwraps[0].clone(); many.entries.len()];
        let texels = many.total_texels();
        many.texels = vec![a.texels[0]; texels];
        many.coverage = vec![Coverage::BAKED; texels];
        many.validate().expect("valid paged map");
        let e = combine_lightmaps(&[&many, &many]).expect_err("past the page limit");
        assert_eq!(e.code(), "lightmap_layout");
    }

    /// A paged combination whose last source has no entries (an additive level that placed
    /// nothing) keeps the other sources: its empty page is not left past the layout's pages.
    #[test]
    fn combine_as_pages_accepts_a_trailing_source_without_entries() {
        let a = small_map();
        let mut two = small_map();
        let mut second = two.entries[0].clone();
        second.name = "f".into();
        second.page = 1;
        two.entries.push(second);
        two.unwraps.push(two.unwraps[0].clone());
        let texels = two.total_texels();
        two.texels = vec![a.texels[0]; texels];
        two.coverage = vec![Coverage::BAKED; texels];
        two.validate().expect("valid two-page map");
        assert_eq!(two.pages(), 2);
        let mut empty = small_map();
        empty.entries.clear();
        empty.unwraps.clear();
        empty.validate().expect("valid entry-less map");
        let (c, regions) = combine_lightmaps(&[&two, &empty]).expect("a trailing empty source combines");
        assert_eq!(c.pages(), 2);
        assert_eq!(c.entries.len(), 2);
        assert_eq!(regions.len(), 2);
        c.validate().expect("valid combination");
        // In the middle it still takes its page, and the next source's entries land after it.
        let (mid, _) = combine_lightmaps(&[&two, &empty, &a]).expect("middle empty source");
        assert_eq!(mid.pages(), 4);
        assert_eq!(mid.entries[2].page, 3);
    }

    /// A compute shader around the lookup module: each invocation samples `uvs[i]` and writes the
    /// irradiance, in 2-D (`paged` false) or texture-array bindings; `sos[i]` is sample `i`'s
    /// packed `scale_offset`.
    fn probe_source_with(paged: bool) -> String {
        let tex = if paged {
            "texture_2d_array<f32>"
        } else {
            "texture_2d<f32>"
        };
        let lookup = lightmap_lookup_wgsl(paged);
        format!(
            "@group(0) @binding(0) var lightmap_atlas: {tex};\n\
             @group(0) @binding(1) var lightmap_sampler: sampler;\n\
             @group(0) @binding(2) var<uniform> lightmap_params: LightmapParams;\n\
             @group(0) @binding(3) var lightmap_direction: {tex};\n\
             @group(1) @binding(0) var<storage, read> uvs: array<vec4<f32>>;\n\
             @group(1) @binding(1) var<storage, read_write> results: array<vec4<f32>>;\n\
             @group(1) @binding(2) var<storage, read> normals: array<vec4<f32>>;\n\
             @group(1) @binding(3) var<storage, read> sos: array<vec4<f32>>;\n\
             {lookup}\n\
             @compute @workgroup_size(8)\n\
             fn probe(@builtin(global_invocation_id) id: vec3<u32>) {{\n\
                 let i = id.x;\n\
                 if (i >= arrayLength(&uvs)) {{ return; }}\n\
                 let e = lightmap_irradiance_for_normal(uvs[i].xy, sos[i], normals[i].xyz);\n\
                 results[i] = vec4<f32>(e, select(0.0, 1.0, lightmap_enabled()));\n\
             }}\n"
        )
    }

    #[test]
    fn the_lookup_module_parses_and_validates() {
        for paged in [false, true] {
            let src = probe_source_with(paged);
            let module =
                naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&src)));
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .unwrap_or_else(|e| panic!("paged {paged}: {e:?}"));
        }
    }

    /// The forward shaders in the texture-array form (ADR-3300): both declarations turn into
    /// arrays, the page reads are spliced in, and the 2-D form keeps the declarations as they are.
    #[test]
    fn the_paged_forward_source_declares_texture_arrays() {
        for (name, text) in [
            ("lab_forward", include_str!("../shaders/lab_forward.wgsl")),
            ("forward_mobile", include_str!("../shaders/forward_mobile.wgsl")),
        ] {
            assert!(
                text.contains(ATLAS_DECLARATION_2D) && text.contains(DIRECTION_DECLARATION_2D),
                "{name}"
            );
            let arrays = |t: &str| t.matches("texture_2d_array<f32>;").count();
            let flat = lightmap_forward_source_with(text, false);
            assert!(
                flat.contains(ATLAS_DECLARATION_2D) && arrays(&flat) == arrays(text),
                "{name}"
            );
            let paged = lightmap_forward_source_with(text, true);
            // No binding is declared 2-D any more (the lookup file's header comment still names them).
            let declared_2d = paged.lines().any(|l| {
                l.starts_with("@group")
                    && (l.contains(ATLAS_DECLARATION_2D) || l.contains(DIRECTION_DECLARATION_2D))
            });
            assert!(!declared_2d, "{name}");
            assert_eq!(arrays(&paged), arrays(text) + 2, "{name}");
            assert!(paged.contains("fn lightmap_page("), "{name}");
        }
        assert_eq!(lightmap_forward_source_with("no marker", true), "no marker");
        assert_eq!(
            lightmap_texture_entry(12).ty,
            wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: lightmap_view_dimension(),
                multisampled: false,
            }
        );
    }

    #[test]
    fn the_flag_is_off_by_default_and_status_reports_installs_and_errors() {
        let settings = LightmapSettings::default();
        assert!(!settings.enabled);
        let mut installed = InstalledLightmap::default();
        let s = installed.status(&settings);
        assert!(!s.installed && !s.enabled);
        assert!(s.forward_pass, "the forward pass shades receivers (ADR-1623)");
        assert_eq!(s.gpu_bytes, 0);
        installed.install("test", small_map());
        let s = installed.status(&LightmapSettings {
            enabled: true,
            intensity: 2.0,
            ..LightmapSettings::default()
        });
        assert!(s.installed && s.enabled);
        assert_eq!((s.width, s.height, s.entries, s.gpu_bytes), (4, 2, 1, 32));
        assert!(!s.has_directions && s.directional && s.bicubic);
        assert_eq!(s.contents, ["sky"]);
        assert_eq!(s.source.as_deref(), Some("test"));
        let mut directional = small_map();
        directional
            .set_directions(&[[0.0, 1.0, 0.0, 1.0]; 8])
            .expect("one per texel");
        installed.install("dir", directional);
        let s = installed.status(&settings);
        assert!(s.has_directions);
        assert_eq!(s.gpu_bytes, 64, "the direction texture doubles the bytes");
        // `regia_test_utils` is not a dev-dependency here: the same pid + counter scheme.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-render-lightmap-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let e = installed
            .load(&dir.join("missing.flmap"), "missing")
            .expect_err("missing");
        assert_eq!(e.code(), "lightmap_io");
        let s = installed.status(&settings);
        assert!(s.installed, "a failed load keeps the installed lightmap");
        assert_eq!(s.error_code.as_deref(), Some("lightmap_io"));
        let path = dir.join("ok.flmap");
        small_map().write(&path).expect("write");
        installed.load(&path, "ok.flmap").expect("load");
        assert_eq!(installed.status(&settings).error, None);
        installed.clear();
        assert!(!installed.status(&settings).installed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_finite_decode_scale_uploads_as_zero() {
        // A subnormal exposure (`1 / exposure` is infinite) never reaches the uniform (ADR-1660).
        let mut map = small_map();
        map.exposure = f32::from_bits(1);
        let on = LightmapSettings {
            enabled: true,
            ..LightmapSettings::default()
        };
        assert_eq!(LightmapParamsGpu::new(&map, &on).scale[0], 0.0);
        let map = small_map();
        let huge = LightmapSettings {
            intensity: f32::INFINITY,
            ..on
        };
        assert_eq!(LightmapParamsGpu::new(&map, &huge).scale[0], 0.0);
        assert!(LightmapParamsGpu::new(&map, &on).scale[0].is_finite());
    }

    /// The specular flag reaches the uniform only while lightmaps are on (ADR-4762).
    #[test]
    fn params_carry_the_specular_flag() {
        let map = small_map();
        let on = LightmapSettings {
            enabled: true,
            ..LightmapSettings::default()
        };
        assert!(LightmapSettings::default().specular, "on by default");
        assert_eq!(LightmapParamsGpu::new(&map, &on).spec, [1.0, 0.0, 0.0, 0.0]);
        let off = LightmapSettings {
            specular: false,
            ..on
        };
        assert_eq!(LightmapParamsGpu::new(&map, &off).spec, [0.0; 4]);
        assert_eq!(
            LightmapParamsGpu::new(&map, &LightmapSettings::default()).spec,
            [0.0; 4],
            "lightmaps off"
        );
        assert_eq!(std::mem::size_of::<LightmapParamsGpu>(), 32, "two vec4s");
        let status = InstalledLightmap::default().status(&off);
        assert!(!status.specular);
    }

    /// A uniform white furnace must not create energy when the specular is enabled.
    #[test]
    fn receiver_diffuse_and_specular_partition_the_uniform_light() {
        let lut = crate::build_brdf_lut();
        for texel in lut.rgba.as_chunks::<4>().0 {
            let ab = [f32::from(texel[0]) / 255.0, f32::from(texel[1]) / 255.0];
            for f0 in [[0.04; 3], [0.9, 0.6, 0.2], [1.0; 3]] {
                let weight = lightmap_diffuse_weight(f0, ab);
                for (w, f) in weight.into_iter().zip(f0) {
                    let specular = (f * ab[0] + ab[1]).clamp(0.0, 1.0);
                    assert!((w + specular - 1.0).abs() < 1e-6);
                    assert!((0.0..=1.0).contains(&w));
                }
            }
        }
    }

    /// ADR-5945, the receiver rule of both forward shaders on a uniform white furnace: with the
    /// material's multi-scatter share the three lobes still sum to the incident light, and a white
    /// conductor reflects all of it at every roughness and view angle. Without the share (the lite
    /// shader before) a rough one kept about half.
    #[test]
    fn receiver_multi_scatter_share_returns_what_single_scatter_loses() {
        let lut = crate::build_brdf_lut();
        let mut least_single = f32::MAX;
        for texel in lut.rgba.as_chunks::<4>().0 {
            let ab = [f32::from(texel[0]) / 255.0, f32::from(texel[1]) / 255.0];
            for f0 in [[0.04; 3], [0.95, 0.64, 0.54], [1.0; 3]] {
                let diffuse = lightmap_diffuse_weight(f0, ab);
                let lobe = crate::hard_surface::multi_scatter_lobe(f0, ab[0], ab[1]);
                for ((&f, &diffuse), &lobe) in f0.iter().zip(&diffuse).zip(&lobe) {
                    // `clamp(multi_scatter_ems(f0, dfg), 0, lm_diffuse_weight)` in both shaders.
                    let multi = lobe.clamp(0.0, diffuse);
                    let specular = 1.0 - diffuse;
                    let diffuse_left = diffuse - multi;
                    assert!((0.0..=1.0).contains(&diffuse_left), "{f0:?} {ab:?}");
                    assert!(
                        (specular + multi + diffuse_left - 1.0).abs() < 1e-6,
                        "{f0:?} {ab:?}"
                    );
                    if f >= 1.0 {
                        // A white conductor: no diffuse lobe, and nothing lost.
                        assert!(specular + multi > 0.999, "{ab:?}: {specular} + {multi}");
                        least_single = least_single.min(specular);
                    } else {
                        assert!(
                            specular + multi <= 1.0 + 1e-6,
                            "{f0:?} {ab:?}: {specular} + {multi}"
                        );
                    }
                }
            }
        }
        assert!(
            least_single < 0.6,
            "single scattering alone loses light on a rough conductor: {least_single}"
        );
    }

    /// Combined direct/indirect atlases cannot reconstruct indirect specular separately.
    #[test]
    fn direct_sun_maps_do_not_count_the_live_sun_specular_twice() {
        let mut map = small_map();
        map.bake.contents |= LightmapContents::SUN_DIRECT;
        let settings = LightmapSettings {
            enabled: true,
            ..LightmapSettings::default()
        };
        assert_eq!(LightmapParamsGpu::new(&map, &settings).spec, [0.0; 4]);
    }

    /// Uniform `[x, y, z]` on the sphere from two numbers in `0..1`.
    fn sphere(u: f32, v: f32) -> [f32; 3] {
        let z = 1.0 - 2.0 * u;
        let r = (1.0 - z * z).max(0.0).sqrt();
        let phi = std::f32::consts::TAU * v;
        [r * phi.cos(), r * phi.sin(), z]
    }

    fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
        a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
    }

    /// One field for both terms (ADR-4762): the cosine integral of the field the specular reads is
    /// the directional rebalance's irradiance, for any normal, while the field stays positive
    /// (`|d| <= 2/3`). A cosine-weighted mean over the hemisphere, by a fixed grid.
    #[test]
    fn the_specular_field_integrates_to_the_diffuse_rebalance() {
        let e = [300.0, 200.0, 100.0];
        let baked = [0.0, 1.0, 0.0];
        for d in [
            [0.0, 0.0, 0.0],
            [0.0, 2.0 / 3.0, 0.0],
            [0.3, 0.5, -0.2],
            [0.6, 0.1, 0.0],
        ] {
            let w = dot3(d, baked).clamp(0.0, 1.0);
            let direction = [d[0], d[1], d[2], w];
            for n in [baked, [1.0, 0.0, 0.0], [0.0, 0.6, 0.8], [0.0, -1.0, 0.0]] {
                let steps = 400;
                let mut sum = [0.0f64; 3];
                for i in 0..steps {
                    for j in 0..steps {
                        let omega = sphere((i as f32 + 0.5) / steps as f32, (j as f32 + 0.5) / steps as f32);
                        let c = dot3(omega, n);
                        if c <= 0.0 {
                            continue;
                        }
                        let l = lightmap_lobe_radiance(e, direction, omega);
                        for (s, l) in sum.iter_mut().zip(l) {
                            *s += f64::from(l * c);
                        }
                    }
                }
                // Each grid cell is 4π / steps² of solid angle.
                let cell = 4.0 * std::f64::consts::PI / f64::from(steps * steps);
                let want = regia_asset::lightmap::shade_directional(e, direction, n);
                for (got, want) in sum.iter().zip(want) {
                    let got = got * cell;
                    assert!(
                        (got - f64::from(want)).abs() <= 0.01 * f64::from(e[0]),
                        "d {d:?} n {n:?}: {got} against {want}"
                    );
                }
            }
        }
    }

    /// Without directions every lobe sees `e / π` (the split-sum's uniform light); a mirror toward
    /// a sun-only texel's light sees the field's peak; the field never goes negative.
    #[test]
    fn the_lobe_radiance_is_uniform_without_directions_and_never_negative() {
        let e = [300.0, 200.0, 100.0];
        let flat = [0.0; 4];
        for m in [[0.0, 1.0, 0.0], [0.0, 0.0, 0.0], [0.3, -0.4, 0.1]] {
            let l = lightmap_lobe_radiance(e, flat, m);
            for (l, e) in l.iter().zip(e) {
                assert!((l - e / std::f32::consts::PI).abs() < 1e-4, "{m:?}: {l}");
            }
        }
        let toward = [0.0, 0.5f32.sqrt(), 0.5f32.sqrt()];
        let sun = [toward[0], toward[1], toward[2], toward[1]];
        let peak = lightmap_lobe_radiance(e, sun, toward);
        let expected = 2.5 / (1.0 + toward[1]) / std::f32::consts::PI;
        assert!((peak[0] / e[0] - expected).abs() < 1e-5, "{peak:?}");
        let away = lightmap_lobe_radiance(e, sun, toward.map(|c| -c));
        assert_eq!(away, [0.0; 3], "clamped at zero");
    }

    /// The lite shader's lobe: a mirror along `r` with length ~1, a fully rough lobe on the normal
    /// and shorter (the ADR-1103 fit), in between it leans toward the normal as `α` grows.
    #[test]
    fn the_lite_lobe_runs_from_the_mirror_to_the_normal() {
        let n = [0.0, 1.0, 0.0];
        let r = [0.8, 0.6, 0.0];
        let mirror = lightmap_spec_lobe(n, r, 0.0);
        for (a, b) in mirror.iter().zip(r) {
            assert!((a - b).abs() < 1e-5, "{mirror:?}");
        }
        let rough = lightmap_spec_lobe(n, r, 1.0);
        assert!(rough[0].abs() < 1e-6 && rough[2].abs() < 1e-6, "{rough:?}");
        assert!((rough[1] - 12.244_397 / 19.729_788).abs() < 1e-4, "{rough:?}");
        let mut last_lean = f32::INFINITY;
        let mut last_len = f32::INFINITY;
        for k in 0..=10 {
            let m = lightmap_spec_lobe(n, r, k as f32 / 10.0);
            let len = dot3(m, m).sqrt();
            let lean = m[0] / len;
            assert!(
                lean <= last_lean + 1e-6 && len <= last_len + 1e-6,
                "α {k}/10: {m:?}"
            );
            (last_lean, last_len) = (lean, len);
        }
    }

    #[test]
    fn params_decode_the_exposure_and_carry_the_flag() {
        let map = small_map();
        let p = LightmapParamsGpu::new(
            &map,
            &LightmapSettings {
                enabled: true,
                intensity: 3.0,
                bicubic: false,
                directional: false,
                specular: true,
                ..LightmapSettings::default()
            },
        );
        assert_eq!(p.scale, [3.0 / map.exposure, 1.0, 0.0, 0.0]);
        // A lightmap holding the direct sun says so while it is on, and only then (ADR-2024).
        let mut with_sun = small_map();
        with_sun.bake.contents |= LightmapContents::SUN_DIRECT;
        let on_sun = LightmapSettings {
            enabled: true,
            ..LightmapSettings::default()
        };
        assert_eq!(LightmapParamsGpu::new(&with_sun, &on_sun).scale[1], 2.0);
        assert_eq!(
            LightmapParamsGpu::new(&with_sun, &LightmapSettings::default()).scale[1],
            0.0
        );
        // Bicubic is on by default; directions only count when the lightmap has them.
        let on = LightmapSettings {
            enabled: true,
            ..LightmapSettings::default()
        };
        assert_eq!(LightmapParamsGpu::new(&map, &on).scale[2..], [0.0, 1.0]);
        let mut directional = small_map();
        directional.set_directions(&[[0.0; 4]; 8]).expect("one per texel");
        assert_eq!(LightmapParamsGpu::new(&directional, &on).scale[2..], [1.0, 1.0]);
        assert_eq!(
            LightmapParamsGpu::new(&map, &LightmapSettings::default()).scale[1],
            0.0
        );
        let d = lightmap_diffuse([0.5, 1.0, 0.0], [std::f32::consts::PI; 3]);
        assert!((d[0] - 0.5).abs() < 1e-6 && (d[1] - 1.0).abs() < 1e-6 && d[2] == 0.0);
    }

    /// A `.flmap` cooked into a project's cache is found by its manifest path, with or without
    /// `assets/`; a path the manifest does not name is `None` (the caller reads the raw file).
    #[test]
    fn a_cooked_lightmap_reads_from_the_asset_server() {
        use regia_asset::{AssetServer, AssetServerSettings};
        let v1: &[u8] = include_bytes!("../../regia_asset/tests/fixtures/lightmap-v1.flmap");
        let root = regia_test_utils::unique_temp_dir("regia_render_cooked_lightmap");
        let maps = root.join("assets").join("maps");
        std::fs::create_dir_all(&maps).expect("maps dir");
        std::fs::write(maps.join("a.flmap"), v1).expect("write");
        let platform = regia_asset_pipeline::host_platform().to_string();
        let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
            project_root: root.clone(),
            platform: platform.clone(),
            incremental: false,
        })
        .expect("cook");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform,
            content_dir: None,
        })
        .expect("open");
        let want = Lightmap::from_bytes(v1).expect("v1 reads");
        for path in ["assets/maps/a.flmap", "maps/a.flmap"] {
            let got = read_cooked_lightmap(&mut server, path)
                .expect("in the manifest")
                .expect("reads");
            assert_eq!(got, want, "{path}");
        }
        assert!(read_cooked_lightmap(&mut server, "maps/missing.flmap").is_none());
        let mut installed = InstalledLightmap::default();
        let read = read_cooked_lightmap(&mut server, "maps/a.flmap").expect("in the manifest");
        installed.install_result("maps/a.flmap", read).expect("installs");
        assert!(installed.status(&LightmapSettings::default()).installed);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Runs the probe shader: `results[i]` is the lookup at `uvs[i]` for `normals[i]`, `w` the
    /// enabled flag.
    fn run_probe(
        device: &crate::RenderDevice,
        gpu: &LightmapGpu,
        uvs: &[[f32; 2]],
        normals: &[[f32; 3]],
    ) -> Vec<[f32; 4]> {
        let sos = vec![[1.0, 1.0, 0.0, 0.0]; uvs.len()];
        run_probe_with(device, gpu, LIGHTMAP_PAGED_BINDINGS, uvs, normals, &sos)
    }

    /// [`run_probe`] in either binding form, with a packed `scale_offset` per sample. The texture
    /// arrays bind the upload's array views.
    fn run_probe_with(
        device: &crate::RenderDevice,
        gpu: &LightmapGpu,
        paged: bool,
        uvs: &[[f32; 2]],
        normals: &[[f32; 3]],
        sos: &[[f32; 4]],
    ) -> Vec<[f32; 4]> {
        let d = &device.device;
        let words: Vec<[f32; 4]> = uvs.iter().map(|u| [u[0], u[1], 0.0, 0.0]).collect();
        let normal_words: Vec<[f32; 4]> = normals.iter().map(|n| [n[0], n[1], n[2], 0.0]).collect();
        let input = |label: &str, data: &[[f32; 4]]| {
            let buf = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (data.len() * 16) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            device.queue.write_buffer(&buf, 0, bytemuck::cast_slice(data));
            buf
        };
        let uv_buf = input("lightmap test uvs", &words);
        let normal_buf = input("lightmap test normals", &normal_words);
        let so_buf = input("lightmap test scale offsets", sos);
        let out_size = (words.len() * 16) as u64;
        let out = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lightmap test out"),
            size: out_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lightmap test readback"),
            size: out_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let storage = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let io_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lightmap test io"),
            entries: &[
                storage(0, true),
                storage(1, false),
                storage(2, true),
                storage(3, true),
            ],
        });
        let io = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lightmap test io"),
            layout: &io_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uv_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: out.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: normal_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: so_buf.as_entire_binding(),
                },
            ],
        });
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lightmap test probe"),
            source: wgpu::ShaderSource::Wgsl(probe_source_with(paged).into()),
        });
        let (lightmap_bgl, lightmap_bg) = if paged == LIGHTMAP_PAGED_BINDINGS {
            (gpu.layout.clone(), gpu.bind_group.clone())
        } else {
            let bgl = LightmapGpu::bind_group_layout_for(d, wgpu::ShaderStages::COMPUTE, paged);
            // The 2-D form binds layer 0 (the binding views are the array ones when the frame
            // binds texture arrays).
            let layer0 = |t: &wgpu::Texture| {
                t.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("lightmap test page 0"),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: 0,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            };
            let (atlas, dirs) = if paged {
                (gpu.array_view.clone(), gpu.direction_array_view.clone())
            } else {
                (layer0(&gpu.texture), layer0(&gpu.direction_texture))
            };
            let (atlas, dirs) = (&atlas, &dirs);
            let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("lightmap test pages"),
                layout: &bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(atlas),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: gpu.params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(dirs),
                    },
                ],
            });
            (bgl, bg)
        };
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("lightmap test probe"),
            bind_group_layouts: &[Some(&lightmap_bgl), Some(&io_bgl)],
            immediate_size: 0,
        });
        let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("lightmap test probe"),
            layout: Some(&layout),
            module: &module,
            entry_point: Some("probe"),
            compilation_options: Default::default(),
            cache: None,
        });
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("lightmap test probe"),
        });
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("lightmap test probe"),
                timestamp_writes: None,
            });
            pass.counted_set_pipeline(&pipeline);
            pass.counted_set_bind_group(0, &lightmap_bg, &[]);
            pass.counted_set_bind_group(1, &io, &[]);
            pass.counted_dispatch((words.len() as u32).div_ceil(8), 1, 1);
        }
        encoder.copy_buffer_to_buffer(&out, 0, &staging, 0, out_size);
        device.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        rx.recv().expect("map callback").expect("map");
        let got: Vec<[f32; 4]> =
            bytemuck::pod_collect_to_vec(&staging.slice(..).get_mapped_range().expect("mapped range"));
        staging.unmap();
        got
    }

    /// Three pages of one 4×4 rectangle each, page `p` lit by its own colour (red, green, blue;
    /// directions pointing three ways) (ADR-3300).
    fn three_pages() -> Lightmap {
        let entry = |page: u32| LightmapEntry {
            name: format!("page{page}"),
            source_vertices: 3,
            rect: [0, 0, 4, 4],
            scale_offset: [1.0, 1.0, 0.0, 0.0],
            vertex_count: 0,
            index_count: 0,
            first_vertex: 0,
            first_index: 0,
            charts: 1,
            area_m2: 1.0,
            texels_per_metre: 1.0,
            key: None,
            seams: None,
            page,
        };
        let unwrap = MeshUnwrap {
            xref: vec![0, 1, 2],
            uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            indices: vec![0, 1, 2],
        };
        let colour = [[400.0, 10.0, 10.0], [10.0, 300.0, 10.0], [10.0, 10.0, 200.0]];
        let irr: Vec<[f32; 3]> = (0..3).flat_map(|p| std::iter::repeat_n(colour[p], 16)).collect();
        let mut map = Lightmap::from_irradiance(
            LightmapLayout {
                width: 4,
                height: 4,
                density: 1.0,
                padding: 1,
                entries: (0..3).map(entry).collect(),
                unwraps: vec![unwrap.clone(), unwrap.clone(), unwrap],
            },
            &irr,
            vec![Coverage::BAKED; 48],
            LightmapBakeInfo {
                contents: LightmapContents::SKY,
                ..LightmapBakeInfo::default()
            },
        )
        .expect("valid");
        let dirs = [[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]];
        let per_texel: Vec<[f32; 4]> = (0..3).flat_map(|p| std::iter::repeat_n(dirs[p], 16)).collect();
        map.set_directions(&per_texel).expect("directions");
        map
    }

    /// A receiver on page 2 samples page 2 (ADR-3300): with texture-array bindings the GPU lookup at
    /// each entry's packed `scale_offset` reads that entry's page, bilinear and bicubic, with its
    /// page's directions, matching the CPU mirror (`Lightmap::shade_entry`). The upload holds every
    /// page as a layer and counts their bytes; the 2-D binding form reads page 0 only.
    #[test]
    fn a_receiver_on_page_2_samples_page_2_on_the_gpu() {
        let device = crate::device::test_device();
        let map = three_pages();
        assert_eq!(map.pages(), 3);
        for bicubic in [false, true] {
            let settings = LightmapSettings {
                enabled: true,
                intensity: 1.0,
                bicubic,
                directional: true,
                specular: true,
                ..LightmapSettings::default()
            };
            let gpu = LightmapGpu::upload(&device, &map, &settings, wgpu::ShaderStages::COMPUTE);
            assert_eq!(gpu.pages, 3);
            assert_eq!(gpu.texture.size().depth_or_array_layers, 3);
            assert_eq!(gpu.gpu_bytes(), 3 * 16 * 8, "atlas and directions, three pages");
            assert_eq!(installed_status(&map).gpu_bytes, gpu.gpu_bytes());
            let uvs = [[0.4, 0.6], [0.5, 0.5], [0.6, 0.3]];
            let normal = [0.0, 0.0, 1.0];
            for page in 0..3usize {
                let so = vec![map.entries[page].packed_scale_offset(); uvs.len()];
                let got = run_probe_with(&device, &gpu, true, &uvs, &[normal; 3], &so);
                for (uv, g) in uvs.iter().zip(&got) {
                    let want = map.shade_entry(page, *uv, normal, bicubic);
                    for (a, b) in g.iter().zip(want) {
                        assert!(
                            (a - b).abs() <= 0.02 * b.abs() + 0.05,
                            "page {page} uv {uv:?} bicubic {bicubic}: gpu {g:?} cpu {want:?}"
                        );
                    }
                    // The page's own colour dominates.
                    let top = (0..3).max_by(|&i, &j| g[i].total_cmp(&g[j])).unwrap_or(0);
                    assert_eq!(top, page, "page {page}: {g:?}");
                }
                // The 2-D form reads page 0 whatever the entry's page.
                let flat = run_probe_with(&device, &gpu, false, &uvs, &[normal; 3], &so);
                let top = (0..3)
                    .max_by(|&i, &j| flat[0][i].total_cmp(&flat[0][j]))
                    .unwrap_or(9);
                assert_eq!(top, 0, "2-D bindings: page 0, {:?}", flat[0]);
            }
        }
    }

    /// The status of `map` installed under default settings.
    fn installed_status(map: &Lightmap) -> LightmapStatus {
        let mut installed = InstalledLightmap::default();
        installed.install("test", map.clone());
        let status = installed.status(&LightmapSettings::default());
        assert_eq!(status.pages, map.pages());
        status
    }

    /// The GPU lookup matches the CPU mirror (`Lightmap::sample`) at texel centres and between
    /// them (RGB9E5 is filtered after decode; tolerance covers the hardware's 8-bit filter
    /// weights).
    #[test]
    fn the_gpu_lookup_matches_the_cpu_sample() {
        let device = crate::device::test_device();
        let map = small_map();
        let settings = LightmapSettings {
            enabled: true,
            intensity: 1.0,
            bicubic: false,
            directional: true,
            specular: true,
            ..LightmapSettings::default()
        };
        let gpu = LightmapGpu::upload(&device, &map, &settings, wgpu::ShaderStages::COMPUTE);
        assert_eq!(gpu.gpu_bytes(), 32);
        let uvs: Vec<[f32; 2]> = vec![
            [0.125, 0.25],
            [0.375, 0.75],
            [0.625, 0.25],
            [0.875, 0.75],
            [0.25, 0.5],
            [0.5, 0.25],
            [0.6, 0.6],
            [0.3, 0.4],
        ];
        // Without directions the normal changes nothing.
        let normals = vec![[1.0, 0.0, 0.0]; uvs.len()];
        let got = run_probe(&device, &gpu, &uvs, &normals);
        for (uv, g) in uvs.iter().zip(&got) {
            let want = map.sample(*uv);
            for (a, b) in g.iter().zip(want) {
                assert!(
                    (a - b).abs() <= 0.02 * b.abs() + 0.05,
                    "uv {uv:?}: gpu {g:?} cpu {want:?}"
                );
            }
            assert_eq!(g[3], 1.0, "the flag reaches the shader");
        }
    }

    /// Block-compressed atlases (ADR-2314) sample on the GPU like their CPU decode: BC6H with each
    /// of its four blocks in another one-region mode (so the device's own BC6H decoder checks every
    /// mode's bit layout where it samples BC), and ASTC HDR through `encode_texels` (decoded on the
    /// CPU on a device without ASTC HDR). At texel centres the lookup is the texel itself.
    #[test]
    fn block_encoded_atlases_sample_like_their_cpu_decode() {
        use regia_asset::baked_gi::{from_rgb9e5, rgb9e5};
        use regia_asset::hdr_blocks::{Bc6hMode, HdrBlockFormat, decode_hdr_blocks, encode_bc6h_block_mode};
        use regia_asset::lightmap::{LightmapBlocks, LightmapTexelEncoding};
        let device = crate::device::test_device();
        let (w, h) = (8u32, 8u32);
        let irr: Vec<[f32; 3]> = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| {
                [
                    40.0 * (1.3f32).powi(x as i32),
                    60.0 + 10.0 * y as f32,
                    300.0 - 15.0 * (x + y) as f32,
                ]
            })
            .collect();
        let base = Lightmap::from_irradiance(
            LightmapLayout {
                width: w,
                height: h,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "e".into(),
                    source_vertices: 3,
                    rect: [0, 0, w, h],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2],
                    uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                    indices: vec![0, 1, 2],
                }],
            },
            &irr,
            vec![Coverage::BAKED; (w * h) as usize],
            LightmapBakeInfo {
                contents: LightmapContents::SKY,
                ..LightmapBakeInfo::default()
            },
        )
        .expect("valid");
        let src: Vec<[f32; 3]> = base.texels.iter().map(|&t| from_rgb9e5(t)).collect();
        let mut data = Vec::new();
        for (k, mode) in Bc6hMode::ALL.into_iter().enumerate() {
            let (bx, by) = (k % 2, k / 2);
            let px: [[f32; 3]; 16] = std::array::from_fn(|i| src[(by * 4 + i / 4) * 8 + bx * 4 + i % 4]);
            data.extend_from_slice(&encode_bc6h_block_mode(&px, &[true; 16], mode));
        }
        let mut bc = base.clone();
        bc.texels = decode_hdr_blocks(HdrBlockFormat::Bc6h, w, h, &data)
            .expect("decodes")
            .into_iter()
            .map(rgb9e5)
            .collect();
        bc.blocks = Some(LightmapBlocks {
            encoding: LightmapTexelEncoding::Bc6h,
            data,
        });
        let mut astc = base.clone();
        astc.encode_texels(LightmapTexelEncoding::AstcHdr)
            .expect("encodes");
        let settings = LightmapSettings {
            enabled: true,
            intensity: 1.0,
            bicubic: false,
            directional: false,
            specular: true,
            ..LightmapSettings::default()
        };
        let uvs: Vec<[f32; 2]> = (0..w * h)
            .map(|i| {
                [
                    ((i % w) as f32 + 0.5) / w as f32,
                    ((i / w) as f32 + 0.5) / h as f32,
                ]
            })
            .collect();
        let normals = vec![[0.0, 0.0, 1.0]; uvs.len()];
        for map in [bc, astc] {
            let native = lightmap_native_blocks(&map, device.device.features()).is_some();
            let gpu = LightmapGpu::upload(&device, &map, &settings, wgpu::ShaderStages::COMPUTE);
            assert_eq!(gpu.native_blocks, native);
            assert_eq!(gpu.gpu_bytes(), if native { 64 } else { 256 });
            eprintln!(
                "{}: native blocks on this device: {native}",
                map.texel_encoding().name()
            );
            let got = run_probe(&device, &gpu, &uvs, &normals);
            for (k, (uv, g)) in uvs.iter().zip(&got).enumerate() {
                let want = map.sample(*uv);
                for (a, b) in g.iter().zip(want) {
                    assert!(
                        (a - b).abs() <= 0.01 * b.abs() + 0.05,
                        "{} texel {k} (block {}): gpu {g:?} cpu {want:?}",
                        map.texel_encoding().name(),
                        (k as u32 / w / 4) * 2 + (k as u32 % w) / 4
                    );
                }
            }
        }
    }

    /// BC6H texels and BC7 directions are the upload payload only when the device feature
    /// is on. Off, [`lightmap_native_blocks`] and [`lightmap_native_direction_blocks`]
    /// decline and the upload uses the decoded RGB9E5 and RGBA8 sections.
    #[test]
    fn native_block_upload_follows_the_compression_feature() {
        use regia_asset::lightmap::{
            LightmapBlocks, LightmapDirectionBlocks, LightmapDirectionEncoding, LightmapTexelEncoding,
        };
        let (w, h) = (4u32, 4u32);
        let irr = vec![[10.0, 20.0, 30.0]; (w * h) as usize];
        let mut map = Lightmap::from_irradiance(
            LightmapLayout {
                width: w,
                height: h,
                density: 1.0,
                padding: 0,
                entries: vec![LightmapEntry {
                    name: "e".into(),
                    source_vertices: 3,
                    rect: [0, 0, w, h],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2],
                    uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                    indices: vec![0, 1, 2],
                }],
            },
            &irr,
            vec![Coverage::BAKED; (w * h) as usize],
            LightmapBakeInfo {
                contents: LightmapContents::SKY,
                ..LightmapBakeInfo::default()
            },
        )
        .expect("valid");
        let blocks = vec![7u8; 16];
        let directions = vec![9u8; 16];
        map.blocks = Some(LightmapBlocks {
            encoding: LightmapTexelEncoding::Bc6h,
            data: blocks.clone(),
        });
        map.directions = vec![0; (w * h) as usize];
        map.direction_blocks = Some(LightmapDirectionBlocks {
            encoding: LightmapDirectionEncoding::Bc7,
            data: directions.clone(),
        });
        let on = wgpu::Features::TEXTURE_COMPRESSION_BC;
        let (format, bytes) = lightmap_native_blocks(&map, on).expect("bc6h");
        assert_eq!(format, wgpu::TextureFormat::Bc6hRgbUfloat);
        assert_eq!(bytes, blocks.as_slice());
        let (format, bytes) = lightmap_native_direction_blocks(&map, on).expect("bc7");
        assert_eq!(format, wgpu::TextureFormat::Bc7RgbaUnorm);
        assert_eq!(bytes, directions.as_slice());
        assert!(lightmap_native_blocks(&map, wgpu::Features::empty()).is_none());
        assert!(lightmap_native_direction_blocks(&map, wgpu::Features::empty()).is_none());
    }

    /// Block-compressed directions (ADR-2635): an 8×8 directional lightmap whose direction section
    /// is four solid 4×4 blocks (one direction per block; ASTC LDR void-extent, then BC7 mode 6) goes
    /// up as blocks where the device samples that format, else as the RGBA8 decode, and the GPU's directional lookup matches the
    /// CPU mirrors over the decoded directions either way.
    #[test]
    fn block_compressed_directions_reach_the_directional_lookup() {
        use regia_asset::lightmap::LightmapDirectionEncoding;
        let device = crate::device::test_device();
        let (w, h) = (8u32, 8u32);
        let irr: Vec<[f32; 3]> = (0..w * h)
            .map(|i| [100.0 + 10.0 * (i % w) as f32, 80.0, 60.0])
            .collect();
        for encoding in [LightmapDirectionEncoding::AstcLdr, LightmapDirectionEncoding::Bc7] {
            let mut map = Lightmap::from_irradiance(
                LightmapLayout {
                    width: w,
                    height: h,
                    density: 1.0,
                    padding: 1,
                    entries: vec![LightmapEntry {
                        name: "e".into(),
                        source_vertices: 3,
                        rect: [0, 0, w, h],
                        scale_offset: [1.0, 1.0, 0.0, 0.0],
                        vertex_count: 0,
                        index_count: 0,
                        first_vertex: 0,
                        first_index: 0,
                        charts: 1,
                        area_m2: 1.0,
                        texels_per_metre: 1.0,
                        key: None,
                        seams: None,
                        page: 0,
                    }],
                    unwraps: vec![MeshUnwrap {
                        xref: vec![0, 1, 2],
                        uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                        indices: vec![0, 1, 2],
                    }],
                },
                &irr,
                vec![Coverage::BAKED; (w * h) as usize],
                LightmapBakeInfo {
                    contents: LightmapContents::SKY,
                    ..LightmapBakeInfo::default()
                },
            )
            .expect("valid");
            map.set_directions(&vec![[0.0, 0.7, 0.0, 0.7]; (w * h) as usize])
                .expect("one per texel");
            // One direction per 4×4 block, leaning further toward +x block by block.
            let block_dirs = [
                [0.0, 0.7, 0.0, 0.7],
                [0.3, 0.7, 0.0, 0.7],
                [0.0, 0.7, 0.3, 0.7],
                [0.5, 0.6, 0.0, 0.6],
            ];
            let mut data = Vec::new();
            for d in block_dirs {
                let rgba = encode_direction(d).to_le_bytes();
                match encoding {
                    LightmapDirectionEncoding::AstcLdr => {
                        // An LDR void-extent block: header 0x1FC, no extent, RGBA as UNORM16 (8-bit × 257).
                        data.extend_from_slice(&[0xFC, 0xFD, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
                        for byte in rgba {
                            data.extend_from_slice(&(u16::from(byte) * 257).to_le_bytes());
                        }
                    }
                    _ => data.extend_from_slice(&bc7_solid_block(rgba)),
                }
            }
            map.set_direction_blocks(encoding, data).expect("blocks decode");
            assert_eq!(map.direction_encoding(), Some(encoding));
            // Exact for ASTC; BC7 mode 6 stores 7 bits a channel and one shared low bit.
            let close = |texel: usize, d: [f32; 4]| {
                let (got, want) = (
                    map.directions[texel].to_le_bytes(),
                    encode_direction(d).to_le_bytes(),
                );
                assert!(
                    got.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 1),
                    "{encoding:?} texel {texel}: {got:?} against {want:?}"
                );
            };
            close(0, block_dirs[0]);
            close(7, block_dirs[1]);
            close((4 * w) as usize, block_dirs[2]);
            let settings = LightmapSettings {
                enabled: true,
                bicubic: false,
                ..LightmapSettings::default()
            };
            let native = lightmap_native_direction_blocks(&map, device.device.features()).is_some();
            let gpu = LightmapGpu::upload(&device, &map, &settings, wgpu::ShaderStages::COMPUTE);
            eprintln!("{encoding:?} directions: native blocks on this device: {native}");
            assert_eq!(gpu.native_direction_blocks(), native);
            assert_eq!(gpu.gpu_bytes(), 4 * 64 + if native { 64 } else { 4 * 64 });
            let uvs: Vec<[f32; 2]> = vec![[0.125, 0.125], [0.875, 0.125], [0.125, 0.875], [0.875, 0.875]];
            let normals = vec![[1.0, 0.0, 0.0]; uvs.len()];
            let got = run_probe(&device, &gpu, &uvs, &normals);
            for ((uv, n), g) in uvs.iter().zip(&normals).zip(&got) {
                let d = map.sample_direction(*uv).expect("directional");
                let want = regia_asset::lightmap::shade_directional(map.sample(*uv), d, *n);
                for (a, b) in g.iter().zip(want) {
                    assert!(
                        (a - b).abs() <= 0.02 * b.abs() + 0.05,
                        "uv {uv:?}: gpu {g:?} cpu {want:?}"
                    );
                }
            }
            // The lean toward +x brightens a +x normal block by block, as the blocks say.
            assert!(got[3][1] > got[0][1], "{got:?}");
        }
    }

    /// One BC7 mode 6 block of a single colour: both endpoints `rgba` (7 bits a channel, the shared
    /// low bit set), every index 0.
    fn bc7_solid_block(rgba: [u8; 4]) -> [u8; 16] {
        let mut bits: u128 = 1 << 6;
        let mut at = 7;
        for channel in rgba {
            for _ in 0..2 {
                bits |= u128::from(channel >> 1) << at;
                at += 7;
            }
        }
        bits |= 0b11 << at;
        bits.to_le_bytes()
    }

    /// An 8×8 atlas with a smooth gradient and a direction section: the GPU's four-tap bicubic
    /// and directional rebalance match `Lightmap::sample_bicubic`, `sample_direction` and
    /// `shade_directional` (tolerance: the hardware's 8-bit filter weights and RGBA8 directions).
    #[test]
    fn the_gpu_bicubic_and_directional_lookup_match_the_cpu_mirrors() {
        let device = crate::device::test_device();
        let (w, h) = (8u32, 8u32);
        let irr: Vec<[f32; 3]> = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| {
                [
                    100.0 + 20.0 * x as f32,
                    60.0 + 10.0 * y as f32,
                    300.0 - 15.0 * (x + y) as f32,
                ]
            })
            .collect();
        let mut map = Lightmap::from_irradiance(
            LightmapLayout {
                width: w,
                height: h,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "e".into(),
                    source_vertices: 3,
                    rect: [0, 0, w, h],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2],
                    uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                    indices: vec![0, 1, 2],
                }],
            },
            &irr,
            vec![Coverage::BAKED; (w * h) as usize],
            LightmapBakeInfo {
                contents: LightmapContents::SKY,
                ..LightmapBakeInfo::default()
            },
        )
        .expect("valid");
        // Light mostly from above, leaning toward +x across the atlas; baked for an up normal.
        let dirs: Vec<[f32; 4]> = (0..w * h)
            .map(|i| {
                let lean = (i % w) as f32 / w as f32 * 0.6;
                let d = [lean, 0.7, 0.0];
                [d[0], d[1], d[2], d[1]]
            })
            .collect();
        map.set_directions(&dirs).expect("one per texel");
        let settings = LightmapSettings {
            enabled: true,
            ..LightmapSettings::default()
        };
        let gpu = LightmapGpu::upload(&device, &map, &settings, wgpu::ShaderStages::COMPUTE);
        assert_eq!(gpu.gpu_bytes(), 2 * 4 * 64);
        let uvs: Vec<[f32; 2]> = vec![
            [0.5, 0.5],
            [0.3125, 0.6875],
            [0.21, 0.43],
            [0.77, 0.19],
            [0.05, 0.95],
            [0.5, 0.5],
        ];
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let normals = vec![
            [0.0, 1.0, 0.0],
            [s, s, 0.0],
            [-s, s, 0.0],
            [0.0, s, s],
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
        ];
        let got = run_probe(&device, &gpu, &uvs, &normals);
        for ((uv, n), g) in uvs.iter().zip(&normals).zip(&got) {
            let e = map.sample_bicubic(*uv);
            let d = map.sample_direction(*uv).expect("directional");
            let want = regia_asset::lightmap::shade_directional(e, d, *n);
            for (a, b) in g.iter().zip(want) {
                assert!(
                    (a - b).abs() <= 0.02 * b.abs() + 0.05,
                    "uv {uv:?} n {n:?}: gpu {g:?} cpu {want:?}"
                );
            }
        }
        // The directions matter: at the same UV a sideways normal gets clearly less than the
        // baked (up) one, which gets the stored irradiance.
        assert!(got[5][0] < 0.9 * got[0][0], "{got:?}");
        // Bilinear and directions off: the plain lookup again.
        let plain = LightmapSettings {
            enabled: true,
            bicubic: false,
            directional: false,
            ..LightmapSettings::default()
        };
        gpu.update_params(&device, &map, &plain);
        let got = run_probe(&device, &gpu, &uvs, &normals);
        for (uv, g) in uvs.iter().zip(&got) {
            let want = map.sample(*uv);
            for (a, b) in g.iter().zip(want) {
                assert!(
                    (a - b).abs() <= 0.02 * b.abs() + 0.05,
                    "uv {uv:?}: gpu {g:?} cpu {want:?}"
                );
            }
        }
    }

    /// Defines of a forward variant.
    fn forward_defs(names: &[&str]) -> std::collections::HashMap<String, naga_oil::compose::ShaderDefValue> {
        names
            .iter()
            .map(|n| ((*n).to_string(), naga_oil::compose::ShaderDefValue::Bool(true)))
            .collect()
    }

    /// The `LIGHTMAP` forward variant composes with the lookup spliced in and validates; a variant
    /// without the define has neither the lookup nor the group 3 lightmap bindings.
    #[test]
    fn the_lightmap_forward_variant_composes_and_others_do_not_see_it() {
        let forward = include_str!("../shaders/lab_forward.wgsl");
        assert_eq!(forward.matches(LIGHTMAP_LOOKUP_MARKER).count(), 1);
        let spliced = lightmap_forward_source(forward);
        assert!(!spliced.contains(LIGHTMAP_LOOKUP_MARKER));
        assert!(spliced.contains("fn lightmap_receiver_irradiance("));
        assert_eq!(lightmap_forward_source("no marker"), "no marker");
        let has = |module: &naga::Module, name: &str| {
            module
                .functions
                .iter()
                .any(|(_, f)| f.name.as_deref() == Some(name))
        };
        let global = |module: &naga::Module, name: &str| {
            module
                .global_variables
                .iter()
                .any(|(_, g)| g.name.as_deref() == Some(name))
        };
        for defs in [
            &["LIGHTMAP", "ALPHA_TEST"][..],
            &["LIGHTMAP", "ALPHA_TEST", "skip_srgb_encode"],
        ] {
            let module = crate::shader_lib::compose_source(forward, "lab_forward.wgsl", forward_defs(defs))
                .unwrap_or_else(|e| panic!("{defs:?}: {e}"));
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .unwrap_or_else(|e| panic!("{defs:?}: {e:?}"));
            assert!(has(&module, "lightmap_receiver_irradiance"), "{defs:?}");
            assert!(global(&module, "lightmap_draw") && global(&module, "lightmap_atlas"));
        }
        let plain =
            crate::shader_lib::compose_source(forward, "lab_forward.wgsl", forward_defs(&["ALPHA_TEST"]))
                .expect("the standard variant");
        assert!(!has(&plain, "lightmap_receiver_irradiance"));
        assert!(!global(&plain, "lightmap_draw") && !global(&plain, "lightmap_atlas"));
    }

    /// A quad (4 source vertices) unwrapped with a seam that splits source vertex 2.
    fn seamed_receiver() -> Lightmap {
        Lightmap::from_irradiance(
            LightmapLayout {
                width: 4,
                height: 2,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "quad".into(),
                    source_vertices: 4,
                    rect: [2, 0, 2, 2],
                    scale_offset: [0.5, 1.0, 0.5, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 2,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2, 3, 2],
                    uv: vec![[0.0, 0.0], [0.5, 0.0], [0.5, 1.0], [0.0, 1.0], [1.0, 1.0]],
                    indices: vec![0, 1, 2, 0, 4, 3],
                }],
            },
            &[[1.0; 3]; 8],
            vec![Coverage::BAKED; 8],
            LightmapBakeInfo::default(),
        )
        .expect("valid")
    }

    fn quad_vertices() -> (Vec<crate::LabVertex>, Vec<u32>) {
        let n = [0.0, 1.0, 0.0];
        (
            vec![
                crate::LabVertex::from_pos_normal_uv([0.0, 0.0, 0.0], n, [0.0, 0.0]),
                crate::LabVertex::from_pos_normal_uv([1.0, 0.0, 0.0], n, [1.0, 0.0]),
                crate::LabVertex::from_pos_normal_uv([1.0, 0.0, 1.0], n, [1.0, 1.0]),
                crate::LabVertex::from_pos_normal_uv([0.0, 0.0, 1.0], n, [0.0, 1.0]),
            ],
            vec![0, 1, 2, 0, 2, 3],
        )
    }

    /// The receiver mesh follows the unwrap: the seam copy keeps the source vertex, the
    /// triangles are the source's, the UV2 is entry-local and the scale_offset is the entry's; a
    /// changed mesh or a missing entry is refused with `lightmap_layout`.
    #[test]
    fn the_receiver_mesh_follows_the_unwrap_and_refuses_a_changed_mesh() {
        let map = seamed_receiver();
        let (vertices, indices) = quad_vertices();
        let r = receiver_mesh(&map, 0, &vertices, &indices).expect("the baked mesh");
        assert_eq!(r.vertices.len(), 5);
        assert_eq!(r.uv2.len(), 5);
        assert_eq!(r.vertices[4].position, vertices[2].position);
        assert_eq!(r.vertices[4].uv, vertices[2].uv, "every attribute follows xref");
        assert_eq!(r.uv2[4], [1.0, 1.0]);
        assert_eq!(r.scale_offset, [0.5, 1.0, 0.5, 0.0]);
        let tris: Vec<[f32; 3]> = r
            .indices
            .iter()
            .map(|&i| r.vertices[i as usize].position)
            .collect();
        let want: Vec<[f32; 3]> = indices.iter().map(|&i| vertices[i as usize].position).collect();
        assert_eq!(tris, want);
        let changed = [0u32, 1, 2, 0, 3, 2];
        let e = receiver_mesh(&map, 0, &vertices, &changed).expect_err("changed");
        assert_eq!(e.code(), "lightmap_layout");
        assert!(e.to_string().contains("re-bake"));
        assert_eq!(
            receiver_mesh(&map, 1, &vertices, &indices)
                .expect_err("missing")
                .code(),
            "lightmap_layout"
        );
    }

    /// [`seamed_receiver`] with clusters built over its unwrap, as `--meshlets` stores them.
    fn seamed_with_clusters() -> Lightmap {
        let mut map = seamed_receiver();
        let (vertices, indices) = quad_vertices();
        let built = receiver_mesh(&map, 0, &vertices, &indices).expect("the baked mesh");
        let positions: Vec<[f32; 3]> = built.vertices.iter().map(|v| v.position).collect();
        map.meshlets =
            vec![crate::visbuffer::MeshletMesh::single_level(&positions, &built.indices).to_bytes()];
        map
    }

    /// `receiver_meshlets`: none without a blob, the clusters when they fit, a refusal when they
    /// do not decode, were built for another unwrap, or bound another mesh of the same counts.
    #[test]
    fn receiver_meshlets_decodes_only_what_fits_the_receiver() {
        let (vertices, indices) = quad_vertices();
        let drawn = receiver_mesh(&seamed_receiver(), 0, &vertices, &indices)
            .expect("the baked mesh")
            .vertices;
        assert!(receiver_meshlets(&seamed_receiver(), 0, &drawn).is_none());
        let map = seamed_with_clusters();
        let mesh = receiver_meshlets(&map, 0, &drawn).expect("a blob").expect("fits");
        assert_eq!(mesh.source_vertex_count, 5, "the split vertex counts");
        assert!(receiver_meshlets(&map, 1, &drawn).is_none(), "no such entry");

        let mut other = seamed_receiver();
        other.meshlets =
            vec![crate::visbuffer::MeshletMesh::single_level(&[[0.0; 3]; 4], &[0, 1, 2]).to_bytes()];
        let message = receiver_meshlets(&other, 0, &drawn)
            .expect("a blob")
            .expect_err("mismatch");
        assert!(message.contains("the unwrap has 5 / 6"), "{message}");

        let mut damaged = seamed_receiver();
        damaged.meshlets = vec![b"not clusters".to_vec()];
        assert!(receiver_meshlets(&damaged, 0, &drawn).expect("a blob").is_err());

        // A NaN in a stored sphere must not pass, nor a blob with no clusters.
        let positions: Vec<[f32; 3]> = drawn.iter().map(|v| v.position).collect();
        let split_indices = receiver_mesh(&seamed_receiver(), 0, &vertices, &indices)
            .expect("the baked mesh")
            .indices;
        let mut nan = crate::visbuffer::MeshletMesh::single_level(&positions, &split_indices);
        nan.bounds[3] = f32::NAN;
        let mut bad = seamed_receiver();
        bad.meshlets = vec![nan.to_bytes()];
        assert!(receiver_meshlets(&bad, 0, &drawn).expect("a blob").is_err());
        let mut hollow = crate::visbuffer::MeshletMesh::single_level(&positions, &split_indices);
        hollow.meshlets.clear();
        hollow.vertices.clear();
        hollow.triangles.clear();
        bad.meshlets = vec![hollow.to_bytes()];
        assert!(receiver_meshlets(&bad, 0, &drawn).expect("a blob").is_err());

        // The same counts, moved: the clusters' bounds would cull in the wrong place.
        let mut moved = drawn.clone();
        for v in &mut moved {
            v.position[0] += 10.0;
        }
        let message = receiver_meshlets(&map, 0, &moved)
            .expect("a blob")
            .expect_err("moved");
        assert!(message.contains("changed since the bake"), "{message}");
        // And scaled about its center.
        for (v, source) in moved.iter_mut().zip(&drawn) {
            v.position = source.position.map(|c| c * 2.0);
        }
        assert!(receiver_meshlets(&map, 0, &moved).expect("a blob").is_err());
    }

    /// With the visibility buffer on, a receiver's baked clusters are in the renderer under its
    /// receiver mesh's key and nowhere else, once; they leave with the profile, the component, the
    /// entity or the lightmap. Without it nothing is decoded. A blob that does not read is counted
    /// and costs the receiver only the batches.
    #[test]
    fn baked_clusters_follow_the_receiver_the_profile_and_the_lightmap() {
        let mut world = World::new();
        world.init_resource::<LightmapReceiverReport>();
        world.init_resource::<crate::RuntimeRenderer>();
        let settings = |on: bool| {
            let mut settings = crate::RenderSettings::default();
            settings.profile.visibility_buffer = on;
            settings
        };
        world.insert_resource(settings(true));
        let mut installed = InstalledLightmap::default();
        installed.install("test", seamed_with_clusters());
        world.insert_resource(installed);
        let (vertices, indices) = quad_vertices();
        let own = Arc::new(crate::CpuMesh { vertices, indices });
        let material = Arc::new(crate::LabMaterial::default());
        let entity = world
            .spawn((
                crate::MeshRenderer::new(own.clone(), material),
                LightmapReceiver { entry: "quad".into() },
            ))
            .id();
        // One schedule for the whole test: the system's record of what the renderer holds lives
        // as long as the system does.
        let mut schedule = Schedule::default();
        schedule.add_systems(apply_lightmap_receivers);

        schedule.run(&mut world);
        let drawn = world
            .get::<crate::MeshRenderer>(entity)
            .expect("mesh")
            .mesh
            .clone();
        assert!(!Arc::ptr_eq(&drawn, &own), "the entity draws its unwrap");
        let key = crate::mesh_key(&drawn);
        let has = |world: &World, key: u64| world.resource::<crate::RuntimeRenderer>().0.has_meshlets(key);
        assert!(has(&world, key));
        assert!(!has(&world, crate::mesh_key(&own)), "not under the source's key");
        let counts = |world: &World| {
            let r = world.resource::<LightmapReceiverReport>();
            (r.applied, r.clusters, r.no_clusters)
        };
        assert_eq!(counts(&world), (1, 1, 0));

        // A steady frame hands over nothing: what a host removed stays removed, and is reported.
        world
            .resource_mut::<crate::RuntimeRenderer>()
            .0
            .remove_meshlets(key);
        schedule.run(&mut world);
        assert!(!has(&world, key), "inserted once per receiver mesh");
        assert_eq!(counts(&world), (1, 0, 1));
        let vertices = drawn.vertices.clone();
        let clusters = receiver_meshlets(&seamed_with_clusters(), 0, &vertices)
            .expect("a blob")
            .expect("fits");
        world
            .resource_mut::<crate::RuntimeRenderer>()
            .0
            .insert_meshlets(key, Arc::new(clusters));
        schedule.run(&mut world);
        assert_eq!(counts(&world), (1, 1, 0));

        // The profile off: gone, and nothing counted. On again: back.
        world.insert_resource(settings(false));
        schedule.run(&mut world);
        assert_eq!(counts(&world), (1, 0, 0));
        assert!(!has(&world, key));
        world.insert_resource(settings(true));
        schedule.run(&mut world);
        assert!(has(&world, key));
        assert_eq!(counts(&world), (1, 1, 0));

        // A lightmap without clusters: the old ones go, the receiver still shades.
        world
            .resource_mut::<InstalledLightmap>()
            .install("plain", seamed_receiver());
        schedule.run(&mut world);
        let replaced = world
            .get::<crate::MeshRenderer>(entity)
            .expect("mesh")
            .mesh
            .clone();
        assert!(!has(&world, key));
        assert!(!has(&world, crate::mesh_key(&replaced)));
        assert_eq!(counts(&world), (1, 0, 1));

        // A blob that does not read: the same, and no panic.
        let mut damaged = seamed_receiver();
        damaged.meshlets = vec![b"not clusters".to_vec()];
        world
            .resource_mut::<InstalledLightmap>()
            .install("damaged", damaged);
        schedule.run(&mut world);
        assert_eq!(counts(&world), (1, 0, 1));

        // Back to clusters, then the component leaves: its clusters go with it.
        world
            .resource_mut::<InstalledLightmap>()
            .install("again", seamed_with_clusters());
        schedule.run(&mut world);
        let last = crate::mesh_key(&world.get::<crate::MeshRenderer>(entity).expect("mesh").mesh);
        assert!(has(&world, last));
        world.entity_mut(entity).remove::<LightmapReceiver>();
        schedule.run(&mut world);
        assert!(!has(&world, last));
        assert!(Arc::ptr_eq(
            &world.get::<crate::MeshRenderer>(entity).expect("mesh").mesh,
            &own
        ));

        // A material the visibility path cannot shade takes no clusters, and is counted without.
        let blended = Arc::new(crate::LabMaterial {
            alpha_mode: crate::ALPHA_BLEND,
            ..crate::LabMaterial::default()
        });
        world
            .entity_mut(entity)
            .insert(LightmapReceiver { entry: "quad".into() });
        world
            .get_mut::<crate::MeshRenderer>(entity)
            .expect("mesh")
            .material = blended;
        schedule.run(&mut world);
        let last = crate::mesh_key(&world.get::<crate::MeshRenderer>(entity).expect("mesh").mesh);
        assert!(!has(&world, last));
        assert_eq!(counts(&world), (1, 0, 1));
        world
            .get_mut::<crate::MeshRenderer>(entity)
            .expect("mesh")
            .material = Arc::new(crate::LabMaterial::default());
        schedule.run(&mut world);
        assert!(has(&world, last), "an opaque material takes them");

        // A despawn does the same as the component leaving.
        world.despawn(entity);
        schedule.run(&mut world);
        assert!(!has(&world, last));
        assert_eq!(counts(&world), (0, 0, 0));
    }

    /// One frame of [`apply_lightmap_receivers`] over `world`.
    fn run_receivers(world: &mut World) {
        let mut schedule = Schedule::default();
        // The entry counters are thread-local. The default executor is multi-threaded, so this
        // one system would record the walk on a worker the test cannot see.
        schedule.set_executor(regia_ecs::ecs::schedule::SingleThreadedExecutor::new());
        schedule.add_systems(apply_lightmap_receivers);
        schedule.run(world);
    }

    /// Thirty-two non-overlapping quads. Every entry matches [`quad_vertices`].
    fn many_receivers() -> Lightmap {
        const N: u32 = 32;
        const COLS: u32 = 8;
        let rows = N.div_ceil(COLS);
        let (width, height) = (COLS * 2, rows * 2);
        let quad = MeshUnwrap {
            xref: vec![0, 1, 2, 3],
            uv: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            indices: vec![0, 1, 2, 0, 2, 3],
        };
        let entries = (0..N)
            .map(|i| {
                let (col, row) = (i % COLS, i / COLS);
                let (x, y) = (col * 2, row * 2);
                LightmapEntry {
                    name: format!("e{i:02}"),
                    source_vertices: 4,
                    rect: [x, y, 2, 2],
                    scale_offset: [
                        2.0 / width as f32,
                        2.0 / height as f32,
                        x as f32 / width as f32,
                        y as f32 / height as f32,
                    ],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }
            })
            .collect();
        let texels = (width * height) as usize;
        Lightmap::from_irradiance(
            LightmapLayout {
                width,
                height,
                density: 1.0,
                padding: 0,
                entries,
                unwraps: vec![quad; N as usize],
            },
            &vec![[1.0; 3]; texels],
            vec![Coverage::BAKED; texels],
            LightmapBakeInfo::default(),
        )
        .expect("32 quads pack")
    }

    /// A steady frame of already-applied receivers must not call [`Lightmap::entry`] or walk names.
    #[test]
    fn asset1_perf_steady_lightmap_frame_does_not_scan_entries() {
        use regia_asset::lightmap::{
            lightmap_entry_calls, lightmap_entry_scans, reset_lightmap_entry_counts,
        };

        let map = many_receivers();
        let mut world = World::new();
        world.init_resource::<LightmapReceiverReport>();
        let (vertices, indices) = quad_vertices();
        let mesh = Arc::new(crate::CpuMesh { vertices, indices });
        let material = Arc::new(crate::LabMaterial::default());
        for entry in &map.entries {
            world.spawn((
                crate::MeshRenderer::new(mesh.clone(), material.clone()),
                LightmapReceiver {
                    entry: entry.name.clone(),
                },
            ));
        }
        let mut installed = InstalledLightmap::default();
        installed.install("test", map);
        world.insert_resource(installed);

        reset_lightmap_entry_counts();
        run_receivers(&mut world);
        let report = world.resource::<LightmapReceiverReport>();
        assert_eq!(report.applied, 32, "the first frame applies every receiver");
        assert!(
            lightmap_entry_calls() >= 32,
            "the first frame resolves names ({} calls)",
            lightmap_entry_calls()
        );

        reset_lightmap_entry_counts();
        run_receivers(&mut world);
        let applied = world.resource::<LightmapReceiverReport>().applied;
        let calls = lightmap_entry_calls();
        let scans = lightmap_entry_scans();
        assert!(
            applied == 32 && calls == 0 && scans == 0,
            "steady frame applied {applied}, entry calls {calls}, entry compares {scans}"
        );
    }

    /// The ECS mapping: a receiver entity draws its entry's unwrap with the UV2 stream; a bad
    /// entry and a changed mesh are reported with their codes and keep the entity's mesh; removing
    /// the lightmap or the component puts the entity's own mesh and UV set back.
    #[test]
    fn receiver_entities_swap_to_the_unwrap_and_back() {
        let mut world = World::new();
        world.init_resource::<LightmapReceiverReport>();
        let (vertices, indices) = quad_vertices();
        let own = Arc::new(crate::CpuMesh { vertices, indices });
        let material = Arc::new(crate::LabMaterial::default());
        let own_uv1 = crate::MeshUv1(Arc::new(vec![[0.25, 0.25]; 4]));
        let good = world
            .spawn((
                crate::MeshRenderer::new(own.clone(), material.clone()),
                LightmapReceiver { entry: "quad".into() },
                own_uv1.clone(),
            ))
            .id();
        let missing = world
            .spawn((
                crate::MeshRenderer::new(own.clone(), material.clone()),
                LightmapReceiver { entry: "nope".into() },
            ))
            .id();
        let (v3, _) = quad_vertices();
        let triangle = Arc::new(crate::CpuMesh {
            vertices: v3[..3].to_vec(),
            indices: vec![0, 1, 2],
        });
        let changed = world
            .spawn((
                crate::MeshRenderer::new(triangle.clone(), material),
                LightmapReceiver { entry: "quad".into() },
            ))
            .id();
        // No lightmap installed: nothing changes.
        run_receivers(&mut world);
        assert!(Arc::ptr_eq(
            &world.get::<crate::MeshRenderer>(good).expect("mesh").mesh,
            &own
        ));
        assert_eq!(world.resource::<LightmapReceiverReport>().applied, 0);
        let mut installed = InstalledLightmap::default();
        installed.install("test", seamed_receiver());
        world.insert_resource(installed);
        run_receivers(&mut world);
        let drawn = world.get::<crate::MeshRenderer>(good).expect("mesh").mesh.clone();
        assert_eq!(drawn.vertices.len(), 5, "the unwrap's vertices");
        assert_eq!(world.get::<crate::MeshUv1>(good).expect("uv2").0.len(), 5);
        assert_eq!(
            world
                .get::<AppliedLightmapReceiver>(good)
                .expect("applied")
                .entry(),
            0
        );
        let report = world.resource::<LightmapReceiverReport>().clone();
        assert_eq!((report.receivers, report.applied), (3, 1));
        let code = |e: Entity| {
            report
                .errors
                .iter()
                .find(|r| r.entity == e.to_bits())
                .map(|r| r.code)
        };
        assert_eq!(code(missing), Some("lightmap_entry"));
        assert_eq!(code(changed), Some("lightmap_layout"));
        assert!(Arc::ptr_eq(
            &world.get::<crate::MeshRenderer>(changed).expect("mesh").mesh,
            &triangle
        ));
        // A second frame keeps the same receiver mesh (built once per lightmap).
        run_receivers(&mut world);
        assert!(Arc::ptr_eq(
            &world.get::<crate::MeshRenderer>(good).expect("mesh").mesh,
            &drawn
        ));
        // The lightmap goes away: the entity's own mesh and second UV set come back.
        world.resource_mut::<InstalledLightmap>().clear();
        run_receivers(&mut world);
        assert!(Arc::ptr_eq(
            &world.get::<crate::MeshRenderer>(good).expect("mesh").mesh,
            &own
        ));
        assert_eq!(world.get::<crate::MeshUv1>(good).expect("own uv1").0, own_uv1.0);
        assert!(world.get::<AppliedLightmapReceiver>(good).is_none());
        // Installed again, then the component removed: restored too.
        world
            .resource_mut::<InstalledLightmap>()
            .install("again", seamed_receiver());
        run_receivers(&mut world);
        assert_eq!(
            world
                .get::<crate::MeshRenderer>(good)
                .expect("mesh")
                .mesh
                .vertices
                .len(),
            5
        );
        world.entity_mut(good).remove::<LightmapReceiver>();
        run_receivers(&mut world);
        assert!(Arc::ptr_eq(
            &world.get::<crate::MeshRenderer>(good).expect("mesh").mesh,
            &own
        ));
        assert!(world.get::<AppliedLightmapReceiver>(good).is_none());
    }

    /// `seamed_receiver` with its one entry keyed to `(guid, part)` (`regia bake-lightmaps --scene`).
    fn keyed(guid: regia_core::id::Guid, part: u32) -> Lightmap {
        let mut map = seamed_receiver();
        map.entries[0].key = Some(regia_asset::lightmap::LightmapEntryKey { entity: guid, part });
        map.entries[0].name = format!("entity_{guid}_{part}");
        map
    }

    /// Every part of a multi-part mesh finds its entry (ADR-2313): the level entity's own renderer
    /// is part 0, a `MeshPartOf` entity is its part; a part of another owner, an owner with no
    /// guid and a lightmap without keys find none.
    #[test]
    fn receivers_by_key_finds_every_part_through_its_owner() {
        use regia_scene::prelude::EntityGuid;
        let g = regia_core::id::Guid::from_u128(0xa1);
        let (vertices, indices) = quad_vertices();
        let mesh = Arc::new(crate::CpuMesh { vertices, indices });
        let material = Arc::new(crate::LabMaterial::default());
        let renderer = || crate::MeshRenderer::new(mesh.clone(), material.clone());
        let mut world = World::new();
        let owner = world.spawn((EntityGuid(g), renderer())).id();
        let part1 = world.spawn((renderer(), MeshPartOf { owner, part: 1 })).id();
        let part2 = world.spawn((renderer(), MeshPartOf { owner, part: 2 })).id();
        let orphan_owner = world.spawn_empty().id();
        world.spawn((
            renderer(),
            MeshPartOf {
                owner: orphan_owner,
                part: 1,
            },
        ));

        let found = receivers_by_key_with_parts(&mut world, &keyed(g, 1));
        assert_eq!(
            found,
            vec![KeyedReceiver {
                entity: part1,
                owner,
                part: 1,
                entry: format!("entity_{g}_1"),
            }]
        );
        let found = receivers_by_key(&mut world, &keyed(g, 0));
        assert_eq!(
            found,
            vec![(owner, format!("entity_{g}_0"))],
            "part 0 is the owner's own mesh"
        );
        assert_eq!(
            receivers_by_key(&mut world, &keyed(g, 2)),
            vec![(part2, format!("entity_{g}_2"))]
        );
        assert!(
            receivers_by_key(&mut world, &seamed_receiver()).is_empty(),
            "no keys, no receivers"
        );
        // The pure matcher agrees with the world-wide one.
        let candidates = [ReceiverCandidate {
            entity: part2,
            owner,
            part: 2,
            guid: g,
        }];
        assert_eq!(match_keyed_receivers(&keyed(g, 2), candidates).len(), 1);
        assert!(match_keyed_receivers(&keyed(g, 1), candidates).is_empty());
    }

    /// A receiver is level content (ADR-2313): it saves through the scene writer under its type
    /// path and spawns back with the same entry.
    #[test]
    fn a_receiver_saves_with_the_level_and_loads_back() {
        use regia_scene::prelude::EntityGuid;
        use regia_scene::spawn::{SnapshotFilter, SpawnOptions, snapshot_scene, spawn_scene};
        let mut app = App::new();
        crate::register_render_types(&mut app);
        let g = regia_core::id::Guid::from_u128(0xb2);
        app.world_mut().spawn((
            EntityGuid(g),
            LightmapReceiver {
                entry: format!("entity_{g}_0"),
            },
        ));
        let file = snapshot_scene(app.world_mut(), &SnapshotFilter::default()).expect("snapshot");
        let text = file.to_ron_string().expect("ron");
        assert!(
            text.contains("regia_render::lightmap::LightmapReceiver"),
            "{text}"
        );
        let back = regia_scene::format::SceneFile::from_ron_str(&text).expect("reads");

        let mut again = App::new();
        crate::register_render_types(&mut again);
        spawn_scene(again.world_mut(), &back, &SpawnOptions::default()).expect("spawn");
        let mut q = again.world_mut().query::<(&EntityGuid, &LightmapReceiver)>();
        let got: Vec<(regia_core::id::Guid, String)> = q
            .iter(again.world())
            .map(|(id, r)| (id.0, r.entry.clone()))
            .collect();
        assert_eq!(got, vec![(g, format!("entity_{g}_0"))]);
    }
}
