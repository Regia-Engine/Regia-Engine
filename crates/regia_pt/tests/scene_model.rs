// SPDX-License-Identifier: MPL-2.0
//! The path tracer's scene model (`scene.rs`): materials, light power, sky-body irradiance, terrain
//! heightfield sampling, camera basis, medium activity and the content hash. CPU only, no GPU, no golden.
//!
//! Analytic references (public radiometry): a uniform disc of angular radius r and radiance L delivers
//! E = π·L·sin²r to a surface facing it; a Lambertian emitter of radiance L and area A radiates π·L·A per side;
//! a sphere of radius r and radiance L radiates π·L·4πr²; a cone of half-angle θ subtends 2π(1 − cos θ) sr; the
//! Fresnel reflectance of a dielectric at normal incidence is ((n − 1)/(n + 1))².

use std::sync::Arc;

use regia_core::glam::{Mat4, Vec3};
use regia_pt::{
    PtCamera, PtCloudField, PtCloudPhase, PtHeightfield, PtLight, PtLightProfile, PtMaterial, PtMedia,
    PtMediumLayer, PtMesh, PtPhase, PtScene, PtSkyBody, PtSun, PtTexture,
};
use regia_terrain::{HoleShape, HolesMask, TerrainMap};

const PI: f32 = std::f32::consts::PI;

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn scene() -> PtScene {
    PtScene::new(PtCamera::pinhole([0.0, 1.0, 5.0], [0.0; 3], 0.8, 16, 16))
}

fn triangle() -> PtMesh {
    PtMesh {
        positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        normals: vec![[0.0, 0.0, 1.0]; 3],
        uvs: vec![[0.0; 2]; 3],
        colors: Vec::new(),
        tangents: Vec::new(),
        indices: vec![0, 1, 2],
    }
}

#[test]
fn material_constructors_set_their_documented_fields() {
    let d = PtMaterial::diffuse([0.2, 0.4, 0.6]);
    assert_eq!(d.base_color, [0.2, 0.4, 0.6, 1.0]);
    assert_eq!((d.metallic, d.roughness, d.lambert), (0.0, 1.0, false));
    let l = PtMaterial::lambert([0.5; 3]);
    assert!(l.lambert);
    assert_eq!(l.base_color, PtMaterial::diffuse([0.5; 3]).base_color);
    let e = PtMaterial::emitter([3.0, 0.0, 1.0]);
    assert_eq!(e.base_color, [0.0, 0.0, 0.0, 1.0]);
    assert!(e.is_emissive() && !d.is_emissive() && !PtMaterial::default().is_emissive());
    // A negative or zero emission is not emission.
    assert!(!PtMaterial::emitter([0.0, -1.0, 0.0]).is_emissive());
}

#[test]
fn dielectric_f0_follows_the_fresnel_normal_incidence_formula() {
    let f0 = |ior: f32| {
        PtMaterial {
            ior,
            ..PtMaterial::default()
        }
        .dielectric_f0()
    };
    assert!((f0(1.5) - 0.04).abs() < 1e-6, "glTF's default IOR gives 4 %");
    assert!((f0(2.0) - 1.0 / 9.0).abs() < 1e-6);
    assert_eq!(f0(1.0), 0.0, "index-matched: no reflection");
    // An invalid IOR (below 1, NaN, infinite) is glTF's 1.5.
    for bad in [0.5, -1.0, f32::NAN, f32::INFINITY] {
        assert!((f0(bad) - 0.04).abs() < 1e-6, "ior {bad}");
    }
}

#[test]
fn light_power_matches_the_radiometric_closed_forms() {
    let i = [10.0, 5.0, 2.0];
    let point = PtLight::Point {
        position: [0.0; 3],
        intensity: i,
    };
    assert!((point.power() - 4.0 * PI * lum(i)).abs() < 1e-3);
    // A spot whose inner and outer cones coincide emits intensity × the cone's solid angle.
    let cos_t = 0.8;
    let spot = PtLight::Spot {
        position: [0.0; 3],
        direction: [0.0, -1.0, 0.0],
        intensity: i,
        inner_cos: cos_t,
        outer_cos: cos_t,
    };
    assert!((spot.power() - 2.0 * PI * (1.0 - cos_t) * lum(i)).abs() < 1e-3);
    // A sphere of radiance L radiates π·L·(4π r²).
    let (r, l) = (0.5, [4.0, 4.0, 4.0]);
    let sphere = PtLight::Sphere {
        position: [0.0; 3],
        radius: r,
        radiance: l,
    };
    assert!((sphere.power() - PI * 4.0 * PI * r * r * lum(l)).abs() < 1e-2);
    // A sphere source on a spot: the sphere's on-axis intensity π r² L spread over the cone.
    let sized = PtLight::SphereSpot {
        position: [0.0; 3],
        radius: r,
        radiance: l,
        direction: [0.0, -1.0, 0.0],
        inner_cos: cos_t,
        outer_cos: cos_t,
    };
    assert!((sized.power() - 2.0 * PI * (1.0 - cos_t) * PI * r * r * lum(l)).abs() < 1e-2);
    // A quad radiates π·L·A per emitting side; area from the edge cross product (a 2 × 3 rectangle).
    let quad = |two_sided| PtLight::Quad {
        corner: [0.0; 3],
        edge_u: [2.0, 0.0, 0.0],
        edge_v: [0.0, 0.0, 3.0],
        radiance: [2.0; 3],
        two_sided,
    };
    assert!((quad(false).power() - PI * 6.0 * 2.0).abs() < 1e-3);
    assert!((quad(true).power() - 2.0 * quad(false).power()).abs() < 1e-3);
}

#[test]
fn a_uniform_sky_disc_delivers_pi_l_sin_squared_r() {
    // Irradiance of a uniform disc at its centre direction: E = π·L·sin²r (the table quadrature must agree within
    // the texel discretisation of the rim).
    for (r, l) in [(0.05f32, 1000.0f32), (0.3, 2.0), (0.8, 5.0)] {
        let body = PtSkyBody::uniform([0.3, 0.8, -0.2], r, [l; 3]);
        let expect = PI * l * r.sin().powi(2);
        let got = body.normal_irradiance();
        assert!((got / expect - 1.0).abs() < 0.03, "r {r}: {got} vs {expect}");
    }
    // Radiance is linear: doubling the table doubles the irradiance.
    let mut body = PtSkyBody::uniform([0.0, 1.0, 0.0], 0.2, [1.0; 3]);
    let e = body.normal_irradiance();
    for t in &mut body.radiance {
        *t = t.map(|c| c * 2.0);
    }
    assert!((body.normal_irradiance() / e - 2.0).abs() < 1e-4);
}

#[test]
fn sky_body_frame_is_orthonormal_with_north_toward_world_up() {
    for dir in [
        [0.3, 0.5, -0.8],
        [1.0, 0.0, 0.0],
        [0.0, -1.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0],
    ] {
        let body = PtSkyBody::uniform(dir, 0.1, [1.0; 3]);
        let (c, e1, e2) = body.frame();
        for v in [c, e1, e2] {
            assert!((v.length() - 1.0).abs() < 1e-5, "{dir:?}: {v:?}");
        }
        assert!(
            c.dot(e1).abs() < 1e-5 && c.dot(e2).abs() < 1e-5 && e1.dot(e2).abs() < 1e-5,
            "{dir:?}"
        );
        assert!((e2.cross(c) - e1).length() < 1e-5, "e1 = e2 x c");
        // A direction with a horizontal part has its north on the +Y side; at the poles it picks a finite axis.
        if dir[0] != 0.0 || dir[2] != 0.0 {
            assert!(e2.y > 0.0, "{dir:?}");
        }
    }
    // A zero direction falls back to +Y.
    assert_eq!(PtSkyBody::uniform([0.0; 3], 0.1, [1.0; 3]).frame().0, Vec3::Y);
}

#[test]
fn tabulate_samples_unit_directions_inside_the_disc_and_clamps_the_rim() {
    let body = PtSkyBody::uniform([0.0, 0.0, 1.0], 0.4, [1.0; 3]);
    let c = body.frame().0;
    let table = body.tabulate(|d| {
        assert!((d.length() - 1.0).abs() < 1e-4, "direction {d:?}");
        // Texels outside the disc take the rim direction, so no sample leaves the cone.
        assert!(
            d.dot(c) >= body.angular_radius.cos() - 1e-4,
            "{d:?} outside the disc"
        );
        [d.dot(c), 0.0, 0.0]
    });
    assert_eq!(table.len(), (body.size * body.size) as usize);
    // The middle texels look almost straight at the centre of the disc.
    let n = body.size as usize;
    assert!(table[n / 2 * n + n / 2][0] > 0.999);
    // Corners are far outside the disc, so they carry the rim value cos(r).
    assert!(
        (table[0][0] - 0.4f32.cos()).abs() < 2e-3,
        "corner {}",
        table[0][0]
    );
    // A zero-size table is a 1 × 1 table, not a division by zero.
    let mut tiny = body.clone();
    tiny.size = 0;
    assert_eq!(tiny.tabulate(|_| [1.0; 3]).len(), 1);
}

#[test]
fn heightfield_is_exact_on_a_plane_and_follows_the_cell_diagonal() {
    let plane = |x: f32, z: f32| 2.0 + 0.5 * x + 0.25 * z;
    let (w, d) = (5u32, 4u32);
    let mut heights = Vec::new();
    for j in 0..d {
        for i in 0..w {
            heights.push(plane(10.0 + i as f32 * 2.0, -4.0 + j as f32 * 3.0));
        }
    }
    let hf = PtHeightfield {
        width: w,
        depth: d,
        origin: [10.0, -4.0],
        spacing: [2.0, 3.0],
        heights,
        material: 0,
    };
    // A plane is reproduced exactly by either triangle of a cell, so any in-grid point matches.
    for (x, z) in [
        (10.0, -4.0),
        (11.3, -2.2),
        (13.9, 3.1),
        (18.0, 5.0),
        (17.99, 4.99),
        (12.0, 0.0),
    ] {
        let got = hf.height_at(x, z).expect("inside the grid");
        assert!((got - plane(x, z)).abs() < 1e-4, "({x},{z}): {got}");
    }
    // Outside the grid there is no ground, including NaN coordinates.
    for (x, z) in [
        (9.99, -4.0),
        (10.0, -4.01),
        (18.01, 0.0),
        (12.0, 5.01),
        (f32::NAN, 0.0),
    ] {
        assert_eq!(hf.height_at(x, z), None, "({x},{z})");
    }
    // Index clamping.
    assert_eq!(hf.height(100, 100), hf.height(w - 1, d - 1));

    // One raised corner: the cell splits along the (i, j+1)-(i+1, j) diagonal.
    let bump = PtHeightfield {
        width: 2,
        depth: 2,
        origin: [0.0; 2],
        spacing: [1.0; 2],
        heights: vec![0.0, 0.0, 0.0, 1.0],
        material: 0,
    };
    assert!((bump.height_at(0.5, 0.5).expect("on the diagonal")).abs() < 1e-6);
    assert!((bump.height_at(0.75, 0.75).expect("far triangle") - 0.5).abs() < 1e-6);
    assert!((bump.height_at(0.25, 0.25).expect("near triangle")).abs() < 1e-6);
    assert!((bump.height_at(1.0, 1.0).expect("far corner") - 1.0).abs() < 1e-6);
    // A NaN corner drops the whole cell.
    let mut cut = bump.clone();
    cut.heights[0] = f32::NAN;
    assert_eq!(cut.height_at(0.9, 0.9), None);
    assert_eq!(cut.height_at(0.1, 0.1), None);
}

#[test]
fn heightfield_from_a_terrain_map_reproduces_the_map_and_cuts_holes() {
    let n = 9u32;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| 5.0 + 0.1 * (i % n) as f32 + 0.05 * (i / n) as f32)
        .collect();
    let map = TerrainMap::from_heights(n, n, heights, 64.0, 20.0, 0.0).expect("map");
    let mut hf = PtHeightfield::from_terrain_map(&map, 3);
    assert_eq!((hf.width, hf.depth, hf.material), (n, n, 3));
    assert!((hf.spacing[0] - 8.0).abs() < 1e-4 && (hf.spacing[1] - 8.0).abs() < 1e-4);
    assert_eq!(hf.heights.len(), (n * n) as usize);
    for (col, row) in [(0, 0), (3, 5), (8, 8)] {
        let [x, z] = map.sample_to_world(col, row);
        let h = hf.height_at(x, z).expect("a sample position");
        assert!(
            (h - map.heights()[(row * n + col) as usize]).abs() < 1e-3,
            "sample ({col},{row})"
        );
    }
    // A hole around the world origin cuts the samples inside and leaves the rest.
    let holes = HolesMask::from_shapes(vec![HoleShape::Rect {
        center: [0.0, 0.0],
        half_extents: [5.0, 5.0],
        rotation_radians: 0.0,
    }])
    .expect("mask");
    let cut = hf.cut_holes(&holes);
    assert!(cut >= 1, "the centre sample is inside the rectangle");
    assert_eq!(hf.heights.iter().filter(|h| h.is_nan()).count(), cut);
    assert_eq!(hf.height_at(0.0, 0.0), None, "no ground in the hole");
    assert!(
        hf.height_at(-30.0, -30.0).is_some(),
        "far from the hole the ground stays"
    );
}

#[test]
fn camera_basis_is_orthonormal_and_survives_degenerate_views() {
    let cams = [
        PtCamera::pinhole([0.0, 1.0, 5.0], [0.0; 3], 0.8, 4, 4),
        PtCamera::pinhole([1.0, 8.0, 1.0], [1.0, 0.0, 1.0], 0.8, 4, 4), // straight down
        PtCamera::pinhole([0.0, -3.0, 0.0], [0.0, 3.0, 0.0], 0.8, 4, 4), // straight up
        PtCamera::pinhole([2.0, 2.0, 2.0], [2.0, 2.0, 2.0], 0.8, 4, 4), // eye on target
    ];
    for cam in cams {
        let (f, r, u) = cam.basis();
        for v in [f, r, u] {
            assert!((v.length() - 1.0).abs() < 1e-5, "{cam:?}");
        }
        assert!(
            f.dot(r).abs() < 1e-5 && f.dot(u).abs() < 1e-5 && r.dot(u).abs() < 1e-5,
            "{cam:?}"
        );
        assert!((r.cross(u) + f).length() < 1e-4, "right-handed {cam:?}");
    }
    let (f, r, u) = PtCamera::pinhole([0.0, 0.0, 5.0], [0.0; 3], 0.8, 4, 4).basis();
    assert!(
        (f - Vec3::NEG_Z).length() < 1e-6 && (r - Vec3::X).length() < 1e-6 && (u - Vec3::Y).length() < 1e-6
    );
    assert_eq!(
        cams[3].basis().0,
        Vec3::NEG_Z,
        "an eye on its target looks down -Z"
    );
}

#[test]
fn thin_lens_aperture_is_focal_length_over_twice_the_f_number() {
    let cam = PtCamera::pinhole([0.0; 3], [0.0, 0.0, -1.0], 0.8, 4, 4);
    assert_eq!(cam.aperture_radius, 0.0, "a pinhole has no aperture");
    let lens = cam.with_lens(50.0, 2.0, 3.5);
    assert!(
        (lens.aperture_radius - 0.0125).abs() < 1e-7,
        "{}",
        lens.aperture_radius
    );
    assert_eq!(lens.focus_distance, 3.5);
    // The f-number floors at 0.1 and the focus distance at a millimetre.
    let wild = cam.with_lens(50.0, 0.0, -2.0);
    assert!(
        (wild.aperture_radius - 0.25).abs() < 1e-6,
        "50 mm / (2 * 0.1) = 0.25 m"
    );
    assert_eq!(wild.focus_distance, 1e-3);
}

#[test]
fn media_is_active_only_with_a_scattering_layer_that_has_height() {
    let layer = PtMediumLayer {
        scattering: [0.01; 3],
        absorption: [0.0; 3],
        reference_height: 0.0,
        falloff: 0.0,
        min_height: 0.0,
        max_height: 10.0,
        phase: PtPhase::Rayleigh,
        aerial: false,
    };
    let media = |l: PtMediumLayer| PtMedia {
        layers: vec![l],
        escape_distance: 0.0,
    };
    assert!(!PtMedia::default().is_active());
    assert!(media(layer).is_active());
    assert!(
        media(PtMediumLayer {
            scattering: [0.0; 3],
            absorption: [0.0, 0.0, 0.5],
            ..layer
        })
        .is_active()
    );
    assert!(
        !media(PtMediumLayer {
            scattering: [0.0; 3],
            ..layer
        })
        .is_active(),
        "no coefficients"
    );
    assert!(
        !media(PtMediumLayer {
            max_height: 0.0,
            ..layer
        })
        .is_active(),
        "an empty slab"
    );
    assert!(
        !media(PtMediumLayer {
            min_height: 20.0,
            ..layer
        })
        .is_active(),
        "an inverted slab"
    );
}

#[test]
fn cloud_field_defaults_are_the_physical_reference() {
    let field = PtCloudField::from_settings(regia_render::CloudSettings::default(), 6_371_000.0);
    assert_eq!(field.albedo, 1.0);
    assert!(field.curl);
    assert_eq!(field.planet_radius_m, 6_371_000.0);
    assert_eq!(field.phase, PtCloudPhase::DROPLET);
    // The raster march's dual lobe: weights add to one and the lobes are within the HG range.
    let dual = PtCloudPhase::RASTER_DUAL_LOBE;
    assert!(dual.forward_weight > 0.0 && dual.forward_weight < 1.0);
    assert!(dual.forward_g.abs() <= 0.99 && dual.back_g.abs() <= 0.99);
    assert_eq!(PtCloudPhase::DROPLET.forward_weight, 1.0, "a single lobe");
}

#[test]
fn scene_builders_return_sequential_indices_and_the_view_albedo_textures() {
    let mut s = scene();
    assert_eq!(s.add_material(PtMaterial::default()), 0);
    assert_eq!(s.add_material(PtMaterial::lambert([1.0; 3])), 1);
    let m = s.add_mesh(triangle());
    assert_eq!(m, 0);
    assert_eq!(s.meshes[0].triangle_count(), 1);
    s.add_instance(m, 1, Mat4::from_translation(Vec3::X));
    assert_eq!((s.instances[0].mesh, s.instances[0].material), (0, 1));
    let tex = || PtTexture {
        width: 1,
        height: 1,
        rgba: std::sync::Arc::from(vec![255; 4]),
        srgb: true,
    };
    s.set_terrain_view_albedo([[0.0; 4]; 4], [1.0, 2.0, 3.0], tex(), Some(tex()));
    let v = s.terrain_view_albedo.as_ref().expect("installed");
    assert_eq!(
        (v.texture, v.normal, v.all_bounces, v.eye),
        (0, Some(1), true, [1.0, 2.0, 3.0])
    );
    assert_eq!(s.textures.len(), 2);
    s.set_terrain_view_albedo([[0.0; 4]; 4], [0.0; 3], tex(), None);
    let v = s.terrain_view_albedo.as_ref().expect("installed");
    assert_eq!((v.texture, v.normal), (2, None));
}

#[test]
fn content_hash_changes_with_every_input_that_changes_the_image() {
    let mut base = scene();
    let mat = base.add_material(PtMaterial::diffuse([0.5; 3]));
    let mesh = base.add_mesh(triangle());
    base.add_instance(mesh, mat, Mat4::IDENTITY);
    base.lights.push(PtLight::Point {
        position: [0.0, 2.0, 0.0],
        intensity: [10.0; 3],
    });
    let h0 = base.content_hash();
    assert_eq!(h0, base.clone().content_hash(), "deterministic");

    type Edit = Box<dyn Fn(&mut PtScene)>;
    let edits: Vec<(&str, Edit)> = vec![
        (
            "instance transform",
            Box::new(|s| s.instances[0].transform = Mat4::from_translation(Vec3::Y)),
        ),
        (
            "material colour",
            Box::new(|s| s.materials[0].base_color[0] = 0.6),
        ),
        (
            "light",
            Box::new(|s| {
                s.lights.push(PtLight::Point {
                    position: [1.0; 3],
                    intensity: [1.0; 3],
                })
            }),
        ),
        (
            "sun",
            Box::new(|s| {
                s.sun = Some(PtSun {
                    toward: [0.0, 1.0, 0.0],
                    color: [1.0; 3],
                    illuminance: 1e5,
                    angular_radius: 0.0047,
                });
            }),
        ),
        ("camera", Box::new(|s| s.camera.fov_y = 0.9)),
        ("stars", Box::new(|s| s.stars = 3.0)),
        (
            "media",
            Box::new(|s| {
                s.media.layers.push(PtMediumLayer {
                    scattering: [0.1; 3],
                    absorption: [0.0; 3],
                    reference_height: 0.0,
                    falloff: 0.0,
                    min_height: 0.0,
                    max_height: 5.0,
                    phase: PtPhase::HenyeyGreenstein([0.5; 3]),
                    aerial: false,
                });
            }),
        ),
        (
            "sky body",
            Box::new(|s| {
                s.sky_bodies
                    .push(PtSkyBody::uniform([0.0, 1.0, 0.0], 0.1, [1.0; 3]))
            }),
        ),
        (
            "clouds",
            Box::new(|s| {
                s.clouds = Some(PtCloudField::from_settings(
                    regia_render::CloudSettings::default(),
                    6.0e6,
                ))
            }),
        ),
        (
            "texture",
            Box::new(|s| {
                s.textures.push(PtTexture {
                    width: 1,
                    height: 1,
                    rgba: std::sync::Arc::from(vec![9; 4]),
                    srgb: false,
                })
            }),
        ),
    ];
    let mut seen = vec![h0];
    for (name, edit) in &edits {
        let mut s = base.clone();
        edit(&mut s);
        let h = s.content_hash();
        assert!(!seen.contains(&h), "{name} did not change the content hash");
        seen.push(h);
    }
}

#[test]
fn content_hash_sees_an_ies_light_profile() {
    // A profile changes the light's emission toward every direction, so a cache keyed on the hash must not serve
    // the image of the scene without it (ADR-2592).
    let mut base = scene();
    base.lights.push(PtLight::Point {
        position: [0.0, 2.0, 0.0],
        intensity: [10.0; 3],
    });
    let h0 = base.content_hash();
    let profile = |peak: f32| {
        Arc::new(regia_asset::ies::CookedIesProfile {
            peak_candela: peak,
            flux_lm: 100.0,
            opening_m: [0.0; 3],
            vertical_range: [0.0, 180.0],
            values: vec![1.0; 16],
        })
    };
    let with = |peak: f32, turn: u8| {
        let mut s = base.clone();
        s.light_profiles.push(PtLightProfile {
            light: 0,
            profile: profile(peak),
            nadir: [0.0, -1.0, 0.0],
            turn,
        });
        s
    };
    let a = with(500.0, 0).content_hash();
    assert_ne!(a, h0, "a profile on the light changes the image");
    assert_ne!(with(900.0, 0).content_hash(), a, "the profile's data");
    assert_ne!(with(500.0, 3).content_hash(), a, "the profile's turn");
    assert_eq!(with(500.0, 0).content_hash(), a, "equal profiles hash equally");
}
