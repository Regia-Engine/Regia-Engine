// SPDX-License-Identifier: MPL-2.0
//! WGSL composition (`specs/13` § 7). Engine modules live in `shaders/` and are linked with naga_oil.

use std::borrow::Cow;

use crate::RenderError;
use crate::shader_reload::OverrideMap;
use naga_oil::compose::{
    ComposableModuleDescriptor, Composer, ComposerError, NagaModuleDescriptor, ShaderDefValue,
};
use regia_app::prelude::Local;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const UTILS: &str = include_str!("../shaders/utils.wgsl");
const VIEW: &str = include_str!("../shaders/view.wgsl");
const MESH: &str = include_str!("../shaders/mesh.wgsl");
const SURFACE: &str = include_str!("../shaders/pbr_surface.wgsl");
const FOLIAGE_SWAY: &str = include_str!("../shaders/foliage_sway.wgsl");
const LIGHTING: &str = include_str!("../shaders/pbr_lighting.wgsl");
const BRDF: &str = include_str!("../shaders/pbr_brdf.wgsl");
const SHADING_MODELS: &str = include_str!("../shaders/shading_models.wgsl");
const THIN_FILM: &str = include_str!("../shaders/thin_film.wgsl");
const ROUGH_TRANSMISSION: &str = include_str!("../shaders/rough_transmission.wgsl");
const BLUE_NOISE: &str = include_str!("../shaders/blue_noise.wgsl");
const SHADOWS: &str = include_str!("../shaders/shadows.wgsl");
const CLUSTERING: &str = include_str!("../shaders/clustering.wgsl");
const FOG: &str = include_str!("../shaders/fog.wgsl");
const GI: &str = include_str!("../shaders/gi.wgsl");
const AERIAL: &str = include_str!("../shaders/aerial.wgsl");
const SSSR: &str = include_str!("../shaders/sssr_common.wgsl");
const CLOUD_SHADOW: &str = include_str!("../shaders/cloud_shadow.wgsl");
const CLOUD_DENSITY: &str = include_str!("../shaders/cloud_density.wgsl");
const MIST: &str = include_str!("../shaders/mist.wgsl");
// The forward shader's own modules (ADR-1552 split of `lab_forward.wgsl`).
const FORWARD_LTC: &str = include_str!("../shaders/forward_ltc.wgsl");
const FORWARD_OCCLUDER_GEOMETRY: &str = include_str!("../shaders/forward_occluder_geometry.wgsl");
const FORWARD_OCCLUSION: &str = include_str!("../shaders/forward_occlusion.wgsl");
const FORWARD_ENV_GROUND: &str = include_str!("../shaders/forward_env_ground.wgsl");
const FORWARD_UTIL: &str = include_str!("../shaders/forward_util.wgsl");
const FORWARD_SURFACE_WEATHER: &str = include_str!("../shaders/forward_surface_weather.wgsl");
// Forward decals' pure helpers (ADR-3255).
const FORWARD_DECALS: &str = include_str!("../shaders/forward_decals.wgsl");

fn add_module(
    composer: &mut Composer,
    source: &str,
    file_path: &str,
    overrides: Option<&OverrideMap>,
) -> Result<(), RenderError> {
    let edited = overrides.and_then(|m| m.get(file_path));
    composer
        .add_composable_module(ComposableModuleDescriptor {
            source: edited.map_or(source, |text| text),
            file_path,
            ..Default::default()
        })
        .map(|_| ())
        .map_err(|err| shader_error(&err, composer))
}

fn shader_error(err: &ComposerError, composer: &Composer) -> RenderError {
    RenderError::Shader(err.emit_to_string(composer))
}

fn base_composer(overrides: Option<&OverrideMap>) -> Result<Composer, RenderError> {
    // naga_oil validates each composed module with these capabilities. The texture table
    // variants (`texture_table`, ADR-863) declare a texture `binding_array`; wgpu still
    // validates every module against the device's own features.
    let mut composer = Composer::default().with_capabilities(
        naga::valid::Capabilities::default()
            | naga::valid::Capabilities::TEXTURE_AND_SAMPLER_BINDING_ARRAY
            | naga::valid::Capabilities::TEXTURE_AND_SAMPLER_BINDING_ARRAY_NON_UNIFORM_INDEXING,
    );
    for (source, file) in [
        (UTILS, "utils.wgsl"),
        (VIEW, "view.wgsl"),
        (MESH, "mesh.wgsl"),
        (SURFACE, "pbr_surface.wgsl"),
        (FOLIAGE_SWAY, "foliage_sway.wgsl"),
        (LIGHTING, "pbr_lighting.wgsl"),
        (THIN_FILM, "thin_film.wgsl"),
        (ROUGH_TRANSMISSION, "rough_transmission.wgsl"),
        (BLUE_NOISE, "blue_noise.wgsl"),
        (BRDF, "pbr_brdf.wgsl"),
        (SHADING_MODELS, "shading_models.wgsl"),
        (SHADOWS, "shadows.wgsl"),
        (CLUSTERING, "clustering.wgsl"),
        (FOG, "fog.wgsl"),
        (GI, "gi.wgsl"),
        (AERIAL, "aerial.wgsl"),
        (SSSR, "sssr_common.wgsl"),
        (CLOUD_SHADOW, "cloud_shadow.wgsl"),
        (CLOUD_DENSITY, "cloud_density.wgsl"),
        (MIST, "mist.wgsl"),
        (FORWARD_LTC, "forward_ltc.wgsl"),
        (FORWARD_OCCLUDER_GEOMETRY, "forward_occluder_geometry.wgsl"),
        (FORWARD_OCCLUSION, "forward_occlusion.wgsl"),
        (FORWARD_ENV_GROUND, "forward_env_ground.wgsl"),
        (FORWARD_UTIL, "forward_util.wgsl"),
        (FORWARD_SURFACE_WEATHER, "forward_surface_weather.wgsl"),
        (FORWARD_DECALS, "forward_decals.wgsl"),
    ] {
        add_module(&mut composer, source, file, overrides)?;
    }
    // GPU irradiance probes, bound in the frame's view group (bindings 24 and 25; 22 is the
    // contact shadows, 23 the large-mesh occluders).
    let ddgi = format!(
        "#define_import_path regia::ddgi\n\
         @group(0) @binding(24) var<uniform> ddgi: DdgiUniform;\n\
         @group(0) @binding(25) var<storage, read> ddgi_data: array<u32>;\n{}",
        crate::ddgi_gpu::ddgi_shading_wgsl()
    );
    add_module(&mut composer, &ddgi, "ddgi.wgsl", overrides)?;
    Ok(composer)
}

/// Composes an entry source (the frame pipelines' shaders) against the engine
/// shader library.
pub fn compose_source(
    source: &str,
    file_path: &str,
    shader_defs: HashMap<String, ShaderDefValue>,
) -> Result<naga::Module, RenderError> {
    compose_source_with(source, file_path, shader_defs, &[])
}

/// [`compose_source`] with extra composable modules `(source, file_path)` added after the
/// engine library, in order. Offline tools that share the engine's WGSL (the reference path
/// tracer, `specs/27` § 1.1) bring their own modules this way and `#import` both.
///
/// In a dev build with committed shader edits ([`crate::shader_reload`]) the edited text of any
/// engine file replaces its embedded text. If that composition fails, this build falls back to
/// the embedded text and logs the error, so an edit never breaks a frame.
///
/// # Errors
///
/// A module or the entry source fails to parse or compose.
pub fn compose_source_with(
    source: &str,
    file_path: &str,
    shader_defs: HashMap<String, ShaderDefValue>,
    extra_modules: &[(&str, &str)],
) -> Result<naga::Module, RenderError> {
    let Some(overrides) = crate::shader_reload::current_overrides() else {
        return compose_with_overrides(source, file_path, shader_defs, extra_modules, None);
    };
    let edited = compose_with_overrides(
        source,
        file_path,
        shader_defs.clone(),
        extra_modules,
        Some(&overrides),
    )
    .and_then(|module| {
        crate::shader_reload::naga_validate(&module)
            .map(|()| module)
            .map_err(RenderError::Shader)
    });
    match edited {
        Ok(module) => Ok(module),
        Err(err) => {
            tracing::warn!(
                target: "regia_render",
                file = file_path,
                "edited shader failed for this permutation; using the embedded shader: {err}"
            );
            compose_with_overrides(source, file_path, shader_defs, extra_modules, None)
        }
    }
}

/// The composition behind [`compose_source_with`], with an explicit override map.
pub(crate) fn compose_with_overrides(
    source: &str,
    file_path: &str,
    shader_defs: HashMap<String, ShaderDefValue>,
    extra_modules: &[(&str, &str)],
    overrides: Option<&OverrideMap>,
) -> Result<naga::Module, RenderError> {
    let _span = tracing::trace_span!("shader_compose").entered();
    let _mem = regia_core::memory::enter_owner(&crate::heap_owners::SHADER_MODULES);
    COMPOSITIONS.fetch_add(1, Ordering::Relaxed);
    crate::forward_permutations::note_compose(file_path, &shader_defs);
    let start = std::time::Instant::now();
    let timing_defs = crate::device::pipeline_timing_enabled().then(|| shader_defs.clone());
    let module = if overrides.is_none() && extra_modules.is_empty() && composer_cache_enabled() {
        compose_cached(source, file_path, shader_defs)
    } else {
        compose_uncached(source, file_path, shader_defs, extra_modules, overrides)
    };
    crate::device::note_phase(crate::device::CostPhase::Compose, start);
    if let Some(defs) = timing_defs
        && composed_before(source, file_path, &defs, extra_modules)
    {
        crate::device::note_phase(crate::device::CostPhase::ComposeRepeat, start);
    }
    module
}

/// `REGIA_PIPELINE_TIMING=1`: whether this process composed the same source, file, defines
/// and extra modules before (the set of keys is only kept in that mode).
fn composed_before(
    source: &str,
    file_path: &str,
    shader_defs: &HashMap<String, ShaderDefValue>,
    extra_modules: &[(&str, &str)],
) -> bool {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<[u8; 32]>>> = std::sync::Mutex::new(None);
    let mut hasher = blake3::Hasher::new();
    for part in [source, file_path] {
        hasher.update(part.as_bytes());
        hasher.update(&[0]);
    }
    let mut defs: Vec<String> = shader_defs.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
    defs.sort();
    for def in defs {
        hasher.update(def.as_bytes());
        hasher.update(&[0]);
    }
    for (module, path) in extra_modules {
        hasher.update(module.as_bytes());
        hasher.update(path.as_bytes());
    }
    let key = *hasher.finalize().as_bytes();
    // A poisoned lock only means a panic while inserting; the set is still usable.
    let mut seen = SEEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    !seen.get_or_insert_with(Default::default).insert(key)
}

/// The entry source as composed: with the `LIGHTMAP` define on, the lightmap lookup functions
/// replace [`crate::lightmap::LIGHTMAP_LOOKUP_MARKER`] (ADR-1623), whether the text is embedded
/// or a dev edit ([`crate::shader_reload`]). Any other source or define set is unchanged.
fn splice_lightmap_lookup<'a>(
    source: &'a str,
    shader_defs: &HashMap<String, ShaderDefValue>,
) -> Cow<'a, str> {
    let on = matches!(shader_defs.get("LIGHTMAP"), Some(ShaderDefValue::Bool(true)));
    if on && source.contains(crate::lightmap::LIGHTMAP_LOOKUP_MARKER) {
        Cow::Owned(crate::lightmap::lightmap_forward_source(source))
    } else {
        Cow::Borrowed(source)
    }
}

/// [`compose_with_overrides`] with a new composer: the whole library is parsed and built.
fn compose_uncached(
    source: &str,
    file_path: &str,
    shader_defs: HashMap<String, ShaderDefValue>,
    extra_modules: &[(&str, &str)],
    overrides: Option<&OverrideMap>,
) -> Result<naga::Module, RenderError> {
    let mut composer = base_composer(overrides)?;
    for (module, path) in extra_modules {
        add_module(&mut composer, module, path, overrides)?;
    }
    let source = overrides
        .and_then(|m| m.get(file_path))
        .map_or(source, |text| text);
    let source = splice_lightmap_lookup(source, &shader_defs);
    let mut module = composer
        .make_naga_module(NagaModuleDescriptor {
            source: &source,
            file_path,
            shader_defs,
            ..Default::default()
        })
        .map_err(|err| shader_error(&err, &composer))?;
    // Mali's compiler crashes on image ops through texture parameters (`handle_args`).
    crate::handle_args::specialize_handle_arguments(&mut module);
    Ok(module)
}

/// [`compose_with_overrides`] over the embedded library alone, with a pooled composer: it
/// keeps every library module it has built (per shader-def set), so a shader links the library
/// instead of parsing and building all of it again. The module is the one [`compose_uncached`]
/// makes.
fn compose_cached(
    source: &str,
    file_path: &str,
    shader_defs: HashMap<String, ShaderDefValue>,
) -> Result<naga::Module, RenderError> {
    let cached = composer_pool().pop();
    let mut composer = match cached {
        Some(composer) => composer,
        None => {
            COMPOSER_BUILDS.fetch_add(1, Ordering::Relaxed);
            base_composer(None)?
        }
    };
    let source = splice_lightmap_lookup(source, &shader_defs);
    let composed = composer
        .make_naga_module(NagaModuleDescriptor {
            source: &source,
            file_path,
            shader_defs,
            ..Default::default()
        })
        .map_err(|err| shader_error(&err, &composer));
    // A failed composition may leave partial state behind: start over next time.
    if composed.is_ok() {
        composer_pool().push(composer);
    }
    let mut module = composed?;
    // Mali's compiler crashes on image ops through texture parameters (`handle_args`).
    crate::handle_args::specialize_handle_arguments(&mut module);
    Ok(module)
}

/// Composers over the embedded engine library ([`compose_with_overrides`] without overrides or
/// extra modules), ready for the next composition. naga_oil caches each imported module per
/// shader-def set, so a later shader links the library instead of building it. A composition
/// takes one out and gives it back, so concurrent compositions (warm workers) each have their
/// own, as the per-thread cache this replaces did; unlike that cache, the pool can be dropped
/// from any thread once composition has gone quiet ([`release_idle_composers`], ADR-3351).
static COMPOSER_POOL: Mutex<Vec<Composer>> = Mutex::new(Vec::new());
/// Compositions so far (every path).
static COMPOSITIONS: AtomicU64 = AtomicU64::new(0);
/// Library composers built for the pool.
static COMPOSER_BUILDS: AtomicU64 = AtomicU64::new(0);
/// Times the pool was dropped.
static COMPOSER_RELEASES: AtomicU64 = AtomicU64::new(0);

/// The pool, whatever a panicking holder left (a composer is only pushed after a successful
/// composition, so its contents are always usable).
fn composer_pool() -> std::sync::MutexGuard<'static, Vec<Composer>> {
    COMPOSER_POOL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// App frames without a composition after which [`release_idle_composers`] drops the pooled
/// composers. A start-up or level warm-up composes every frame and keeps them; an idle app frees
/// them half a second in, and the next composition builds one again.
pub const COMPOSER_IDLE_FRAMES: u32 = 30;

/// What the shader composer cache holds and did (ADR-3351).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ComposerCacheStats {
    /// Library composers pooled now (each keeps the parsed library and every module it built).
    pub pooled: usize,
    /// Compositions so far, cached or not.
    pub compositions: u64,
    /// Library composers built for the pool so far.
    pub builds: u64,
    /// Times the pool was dropped ([`release_composer_cache`]).
    pub releases: u64,
}

/// The composer cache's counters.
#[must_use]
pub fn composer_cache_stats() -> ComposerCacheStats {
    ComposerCacheStats {
        pooled: composer_pool().len(),
        compositions: COMPOSITIONS.load(Ordering::Relaxed),
        builds: COMPOSER_BUILDS.load(Ordering::Relaxed),
        releases: COMPOSER_RELEASES.load(Ordering::Relaxed),
    }
}

/// Drops every pooled composer and returns how many there were; the next composition builds one
/// again (the composed modules are the same either way).
pub fn release_composer_cache() -> usize {
    let dropped = std::mem::take(&mut *composer_pool());
    let count = dropped.len();
    if count > 0 {
        COMPOSER_RELEASES.fetch_add(1, Ordering::Relaxed);
    }
    // Dropped outside the lock: freeing a composer's modules takes a while.
    drop(dropped);
    count
}

/// `Last` (added by `RenderPlugin`): drops the pooled composers once no shader was composed for
/// [`COMPOSER_IDLE_FRAMES`] app frames.
pub fn release_idle_composers(mut state: Local<(u64, u32)>) {
    let compositions = COMPOSITIONS.load(Ordering::Relaxed);
    if compositions != state.0 {
        *state = (compositions, 0);
        return;
    }
    state.1 = state.1.saturating_add(1);
    if state.1 == COMPOSER_IDLE_FRAMES {
        let _span = tracing::debug_span!("release_idle_composers").entered();
        let _mem = regia_core::memory::enter_owner(&crate::heap_owners::SHADER_MODULES);
        release_composer_cache();
    }
}

/// The per-thread composer cache is on unless `REGIA_COMPOSER_CACHE=0` (A/B timing).
fn composer_cache_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| regia_core::brand::env_var("REGIA_COMPOSER_CACHE").map_or(true, |v| v != "0"))
}

/// The cloud transmittance map lookup (`regia::cloud_shadow`, ADR-320) without its SPDX and
/// `#define_import_path` lines, for shaders that are not composed against the engine library:
/// the terrain splices it in and calls `cloud_sun_transmittance` with its own binding.
#[must_use]
pub fn cloud_shadow_wgsl() -> &'static str {
    library_snippet_body(CLOUD_SHADOW)
}

/// The body of a library module for verbatim splicing into a shader that is not composed
/// against the engine library: the source without its leading `// SPDX-License-Identifier:`
/// line (ADR-10225; the host file carries its own) and without the `#define_import_path` line
/// after it. A source without an SPDX line loses only its first line, as before.
pub(crate) fn library_snippet_body(source: &str) -> &str {
    let source = source
        .strip_prefix("// SPDX-License-Identifier:")
        .map_or(source, |rest| rest.split_once('\n').map_or("", |(_, body)| body));
    source.split_once('\n').map_or(source, |(_, rest)| rest)
}

/// A shader source for a stand-alone WGSL string (no `#import`s), with texture and sampler
/// parameters specialised away like [`compose_source`] does, so the module compiles on Mali.
/// A source naga cannot parse is passed through as WGSL for wgpu to report.
pub fn wgsl_source(source: &str) -> wgpu::ShaderSource<'_> {
    match naga::front::wgsl::parse_str(source) {
        Ok(mut module) => {
            if crate::handle_args::specialize_handle_arguments(&mut module) {
                wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module))
            } else {
                wgpu::ShaderSource::Wgsl(source.into())
            }
        }
        Err(_) => wgpu::ShaderSource::Wgsl(source.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composes_library_imports() {
        let source = r#"
#import regia::clustering
#import regia::mesh
#import regia::pbr::lighting
#import regia::pbr::brdf

@fragment
fn main() -> @location(0) vec4<f32> {
    let n = regia::clustering::cluster_count();
    let m = regia::mesh::Mat3x4(vec4<f32>(1.0, 0.0, 0.0, 0.0), vec4<f32>(0.0, 1.0, 0.0, 0.0), vec4<f32>(0.0, 0.0, 1.0, 0.0));
    let p = regia::mesh::mul_mat3x4(m, vec3<f32>(1.0, 0.0, 0.0));
    let d = regia::pbr::lighting::d_ggx(1.0, 0.5);
    let lobes = regia::pbr::brdf::isotropic_lobes(1.0, 1.0, 1.0, 1.0, vec3<f32>(0.5), vec3<f32>(0.04), 0.25);
    return vec4<f32>(f32(n) * 0.0 + p.x * 0.0 + d * 0.0 + lobes.diffuse.x * 0.0, 0.0, 0.0, 1.0);
}
"#;
        compose_source(source, "library_probe.wgsl", HashMap::new()).expect("library");
    }

    /// A spliced library snippet (`cloud_shadow_wgsl`, `surface_weather_wgsl`) carries neither
    /// the module's SPDX line (ADR-10225) nor its `#define_import_path` line, so a host file
    /// holding the text verbatim needs neither, and the body itself is untouched.
    #[test]
    fn library_snippets_drop_the_spdx_and_import_lines() {
        for (snippet, source) in [
            (cloud_shadow_wgsl(), CLOUD_SHADOW),
            (
                crate::surface_weather_wgsl(),
                include_str!("../shaders/forward_surface_weather.wgsl"),
            ),
        ] {
            assert!(!snippet.contains("SPDX-License-Identifier"));
            assert!(!snippet.contains("#define_import_path"));
            assert!(source.ends_with(snippet));
            assert!(source.lines().count() >= snippet.lines().count() + 2);
        }
        assert_eq!(
            library_snippet_body(
                "// SPDX-License-Identifier: MPL-2.0\n#define_import_path a::b\nfn f() {}\n"
            ),
            "fn f() {}\n"
        );
        assert_eq!(
            library_snippet_body("#define_import_path a::b\nfn f() {}\n"),
            "fn f() {}\n"
        );
        assert_eq!(library_snippet_body("// SPDX-License-Identifier: MPL-2.0"), "");
        assert_eq!(library_snippet_body(""), "");
    }

    /// The cloud transmittance lookup (ADR-320) composes and validates, and every reader runs it
    /// only under its parameters' switch (`w`, 0 when `CloudSettings::shadow_map` /
    /// `fog_coupling` are off), so with the switches off the shaders take the old path. Both
    /// readers and the map pass compose and validate with and without their variant defines.
    #[test]
    fn cloud_shadow_lookup_composes_and_is_gated() {
        let probe = r#"
#import regia::cloud_shadow

@group(0) @binding(0) var probe_map: texture_2d<f32>;

@fragment
fn main() -> @location(0) vec4<f32> {
    let params = vec4<f32>(20000.0, 1500.0, 4000.0, 1.0);
    let t = regia::cloud_shadow::cloud_sun_transmittance(probe_map, params, vec3<f32>(10.0, 0.0, 20.0), vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.0, 2.0, 0.0));
    return vec4<f32>(t);
}
"#;
        let module =
            compose_source(probe, "cloud_shadow_probe.wgsl", HashMap::new()).expect("cloud shadow module");
        crate::shader_reload::naga_validate(&module).expect("cloud shadow probe validates");
        let forward = include_str!("../shaders/lab_forward.wgsl");
        let fog = include_str!("../shaders/volumetric_fog.wgsl");
        for (name, source, gate) in [
            ("lab_forward.wgsl", forward, "if (frame.cloud_sun.w > 0.5) {"),
            ("volumetric_fog.wgsl", fog, "if (u.cloud.w > 0.5) {"),
        ] {
            let at = source
                .find(gate)
                .unwrap_or_else(|| panic!("{name}: no switch around the map"));
            let call = source
                .find("cloud_sun_transmittance(")
                .unwrap_or_else(|| panic!("{name}: no map read"));
            assert_eq!(
                source.matches("cloud_sun_transmittance(").count(),
                1,
                "{name} reads the map once"
            );
            assert!(
                call > at && call - at < 200,
                "{name}: the map read is not under its switch"
            );
        }
        for (source, file, sets) in [
            (
                forward,
                "lab_forward.wgsl",
                &[
                    &[][..],
                    &["ALPHA_TEST"][..],
                    &["GPU_DRIVEN", "skip_srgb_encode"][..],
                ][..],
            ),
            (fog, "volumetric_fog.wgsl", &[&[][..]][..]),
            (
                include_str!("../shaders/clouds.wgsl"),
                "clouds.wgsl",
                &[&[][..]][..],
            ),
        ] {
            for set in sets {
                let module = compose_source(source, file, defs(set))
                    .unwrap_or_else(|err| panic!("{file} {set:?}: {err}"));
                crate::shader_reload::naga_validate(&module)
                    .unwrap_or_else(|err| panic!("{file} {set:?}: {err}"));
            }
        }
    }

    /// ADR-5944: the air reads the cloud transmittance map only under its switch. The aerial
    /// composite loads the map and the sun table in one function that returns first when
    /// `u.cloud.w` is off; the forward shader's haze and rain read the map in one helper, called
    /// only under `haze_under_clouds()` (the haze) or the map's own switch (the rain's veil). The
    /// ray lookups compose and validate, and so do the shaders that call them.
    #[test]
    fn the_air_reads_the_cloud_map_only_under_its_switch() {
        let probe = r#"
#import regia::cloud_shadow

@group(0) @binding(0) var probe_map: texture_2d<f32>;

@fragment
fn main() -> @location(0) vec4<f32> {
    let params = vec4<f32>(20000.0, 1500.0, 4000.0, 1.0);
    let eye = vec3<f32>(0.0, 1200.0, 0.0);
    let sun = vec3<f32>(0.0, 1.0, 0.0);
    let span = regia::cloud_shadow::cloud_air_span(params, eye, vec3<f32>(0.0, -0.6, 0.8), 2000.0, sun);
    let lit = regia::cloud_shadow::cloud_sun_on_ray(probe_map, params, eye, vec3<f32>(0.0, 0.0, 1600.0), sun);
    return vec4<f32>(span, lit);
}
"#;
        let module = compose_source(probe, "cloud_air_probe.wgsl", HashMap::new()).expect("cloud air module");
        crate::shader_reload::naga_validate(&module).expect("cloud air probe validates");

        let sky = include_str!("../shaders/outdoor_sky.wgsl");
        let removed = sky
            .split_once("fn cloud_air_removed(")
            .and_then(|(_, rest)| rest.split_once("\nfn under_clouds("))
            .map(|(body, _)| body)
            .unwrap_or_else(|| panic!("outdoor_sky.wgsl: no cloud_air_removed before under_clouds"));
        let gate = removed
            .find("if (u.cloud.w < 0.5) {\n        return vec3<f32>(0.0);")
            .unwrap_or_else(|| panic!("cloud_air_removed does not return first with the map off"));
        for read in ["cloud_sun_transmittance(", "aerial_sun_node(", "aerial_sun_at("] {
            let at = removed
                .find(read)
                .unwrap_or_else(|| panic!("cloud_air_removed never calls {read}"));
            assert!(at > gate, "{read} before the switch");
        }
        // Outside that function the map lookup is called once more, for the planet's surface,
        // and `cloud_air_removed` once more for the air in front of it: both in functions that
        // return the clear value first with the map off. Each aerial entry goes through
        // `under_clouds`, which returns `air` with the map off.
        let outside = sky.replace(removed, "");
        assert_eq!(outside.matches("cloud_sun_transmittance(").count(), 1);
        assert_eq!(
            outside.matches("cloud_air_removed(").count(),
            3,
            "its definition, the aerial entries' call and the planet's"
        );
        for (name, clear) in [
            ("fn planet_air_under_clouds(", "return inscatter;"),
            ("fn planet_ground_under_clouds(", "return u.ground.xyz;"),
        ] {
            let body = sky
                .split_once(name)
                .and_then(|(_, rest)| rest.split_once("\n}\n"))
                .map(|(body, _)| body)
                .unwrap_or_else(|| panic!("outdoor_sky.wgsl: no {name}"));
            let gate = body
                .find(&format!(
                    "if (u.cloud.w < 0.5 || hit_m <= 0.0) {{\n        {clear}"
                ))
                .unwrap_or_else(|| panic!("{name} does not return first with the map off"));
            for read in ["cloud_sun_transmittance(", "cloud_air_removed("] {
                if let Some(at) = body.find(read) {
                    assert!(at > gate, "{name}: {read} before the switch");
                }
            }
        }
        assert!(sky.contains(
            "fn under_clouds(view: vec3<f32>, dist: f32, air: vec4<f32>, table: vec3<f32>) -> vec4<f32> {\n    if (u.cloud.w < 0.5) {\n        return air;\n    }"
        ));
        assert_eq!(
            sky.matches("under_clouds(view, dist, ").count(),
            2,
            "fs_aerial and the table entries"
        );

        let forward = include_str!("../shaders/lab_forward.wgsl");
        assert_eq!(
            forward.matches("cloud_sun_on_ray(").count(),
            1,
            "one haze read of the map"
        );
        assert!(forward.contains(
            "fn haze_under_clouds() -> bool {\n    return frame.cloud_sun.w > 0.5 && frame.fog_params.w > 0.5 && frame.fog_params.x > 0.0;\n}"
        ));
        // The helper is called three times: by the aerial haze of the two media functions, under
        // `haze_under_clouds()`, and by the precipitation's veil, under the map's own switch
        // (`rain_under_clouds()`: a named gate, so the sun term's `if (frame.cloud_sun.w > 0.5) {`
        // stays the file's first, which `cloud_shadow_lookup_composes_and_is_gated` looks for).
        assert!(forward.contains("fn rain_under_clouds() -> bool {\n    return frame.cloud_sun.w > 0.5;\n}"));
        let gates = ["if (haze_under_clouds()) {", "if (rain_under_clouds()) {"];
        let mut calls = 0;
        for (at, _) in forward.match_indices("haze_sun_lit(") {
            if forward[..at].ends_with("fn ") {
                continue;
            }
            calls += 1;
            let before = &forward[at.saturating_sub(700)..at];
            assert!(
                gates.iter().any(|gate| before.contains(gate)),
                "a read of the cloud map for the haze or the rain outside its switch"
            );
        }
        assert_eq!(calls, 3, "the two aerial haze calls and the precipitation's");
        for (source, file) in [(sky, "outdoor_sky.wgsl"), (forward, "lab_forward.wgsl")] {
            let module =
                compose_source(source, file, HashMap::new()).unwrap_or_else(|err| panic!("{file}: {err}"));
            crate::shader_reload::naga_validate(&module).unwrap_or_else(|err| panic!("{file}: {err}"));
        }
        // The constants the shaders repeat.
        let aerial = include_str!("../shaders/aerial.wgsl");
        assert!(aerial.contains(&format!(
            "const HAZE_SKY_FLOOR: f32 = {:?};",
            crate::clouds::HAZE_SKY_FLOOR
        )));
        let lookup = cloud_shadow_wgsl();
        assert!(lookup.contains(&format!(
            "for (var i = 0; i < {}; i++) {{",
            crate::clouds::HAZE_CLOUD_STEPS
        )));
    }

    fn defs(names: &[&str]) -> HashMap<String, ShaderDefValue> {
        names
            .iter()
            .map(|n| ((*n).to_string(), ShaderDefValue::Bool(true)))
            .collect()
    }

    /// The per-thread composer (which links library modules it built for earlier shaders)
    /// makes exactly the module a new composer makes, for every def set, in any order.
    #[test]
    fn cached_composer_matches_a_new_one() {
        let forward = include_str!("../shaders/lab_forward.wgsl");
        let sets: [&[&str]; 4] = [
            &[],
            &["skip_srgb_encode"],
            &["GPU_DRIVEN", "skip_srgb_encode"],
            &["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST"],
        ];
        // Twice over, so the second pass composes from a composer that holds every set.
        for set in sets.iter().chain(sets.iter().rev()) {
            let fresh = compose_uncached(forward, "lab_forward.wgsl", defs(set), &[], None).expect("fresh");
            let cached = compose_cached(forward, "lab_forward.wgsl", defs(set)).expect("cached");
            assert_eq!(
                format!("{fresh:?}"),
                format!("{cached:?}"),
                "cached composition differs for defs {set:?}"
            );
        }
    }
}
