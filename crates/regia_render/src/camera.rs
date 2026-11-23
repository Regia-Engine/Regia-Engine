// SPDX-License-Identifier: MPL-2.0
//! Cameras and views (`specs/13` § 9).

use regia_app::prelude::*;

/// Where a camera renders.
#[derive(Reflect, Debug, Clone, PartialEq, Eq, Default)]
#[reflect(Clone)]
pub enum RenderTarget {
    /// Primary window swapchain.
    #[default]
    Window,
    /// Offscreen texture asset (handle filled in by the scene lane).
    Texture {
        /// Asset id placeholder until M4 texture handles are wired.
        asset_id: u64,
    },
}

/// Clear behaviour for a view.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Clone)]
pub enum ClearMode {
    /// Solid colour. The 3D frame does not use the color yet: it clears as for
    /// [`Self::Skybox`] (sky where the profile draws one, near-black elsewhere), and the first
    /// camera found with it logs a warning (ADR-869).
    Color([f32; 4]),
    /// Sky pass fills the background.
    Skybox,
    /// Load existing contents (e.g. overlay camera).
    None,
}

/// Camera component data mirrored on the render thread (`specs/13` § 9).
///
/// As a component, pair it with `regia_scene::Transform`/`GlobalTransform`:
/// the camera looks down the entity's −Z axis, and [`Self::exposure_ev`] pins
/// the frame's exposure (overriding auto exposure in the post stack).
///
/// The frame builds the view from that axis and world +Y, so a roll of the transform is not
/// rendered (agent picking, `screen_ray`, uses the full rotation). A camera looking straight
/// up or down takes world −Z (down) or +Z (up) as screen up, whatever its yaw (ADR-869).
///
/// # Example
///
/// ```
/// use regia_render::Camera;
///
/// let camera = Camera::default();
/// assert!(!camera.orthographic); // perspective by default
/// assert_eq!(camera.fov_y, std::f32::consts::FRAC_PI_4); // vertical FOV in radians
/// assert_eq!(camera.near, 0.1); // metres, near clip plane
/// ```
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default, Clone)]
pub struct Camera {
    /// Vertical field of view in radians (perspective).
    pub fov_y: f32,
    /// Orthographic half-extent in world units (when `orthographic` is true).
    pub ortho_size: f32,
    /// Perspective when false, orthographic when true.
    ///
    /// The 3D frame does not render orthographic views yet: it always draws a perspective
    /// projection with [`Self::fov_y`], and the first orthographic camera found logs a
    /// warning (ADR-869). Agent picking (`screen_ray`) already casts parallel rays for it.
    pub orthographic: bool,
    /// Near clip plane in meters. A value that is not finite and positive renders at 0.1 m
    /// (`view_math::FALLBACK_NEAR`).
    pub near: f32,
    /// Far clip plane in meters. The frame's depth is standard Z (near 0, far 1), not
    /// reverse-Z: `f32::INFINITY` renders with a 100 km far plane
    /// (`view_math::INFINITE_FAR_STANDIN`), and any other value not beyond `near` renders at
    /// `near × 10⁴` (ADR-869).
    pub far: f32,
    /// Viewport in normalized coordinates `[x, y, width, height]`, each in `0.0..=1.0` (a fraction
    /// of the target, not pixels); the pixel extent is carried separately in [`RenderView::width`]
    /// and [`RenderView::height`].
    pub viewport: [f32; 4],
    /// Render target.
    pub target: RenderTarget,
    /// Sort order when multiple cameras draw to the same target.
    pub order: i32,
    /// How the colour buffer is cleared before this view.
    pub clear: ClearMode,
    /// Layer mask: bit `i` set ⇒ layer `i` is visible. The editor draws it as the render-layer grid.
    #[reflect(@regia_ecs::attrs::LayerMask("render"))]
    pub layer_mask: u32,
    /// Reserved: not read yet. The frame always renders into an HDR target; the render
    /// profile decides the formats (ADR-869).
    pub hdr: bool,
    /// Optional fixed exposure in EV100 (overrides auto exposure when set).
    pub exposure_ev: Option<f32>,
    /// Reserved: not read yet. The sample count comes from the render profile
    /// (`FrameQuality`), whatever this holds (ADR-869).
    pub msaa_override: u32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            fov_y: std::f32::consts::FRAC_PI_4,
            ortho_size: 5.0,
            orthographic: false,
            near: 0.1,
            far: 1000.0,
            viewport: [0.0, 0.0, 1.0, 1.0],
            target: RenderTarget::Window,
            order: 0,
            clear: ClearMode::Skybox,
            layer_mask: 0xffff_ffff,
            hdr: true,
            exposure_ev: None,
            msaa_override: 0,
        }
    }
}

/// Extracted view used during the render schedule.
#[derive(Debug, Clone)]
pub struct RenderView {
    /// World-space eye position.
    pub eye: [f32; 3],
    /// World-space look-at target.
    pub target: [f32; 3],
    /// Up vector.
    pub up: [f32; 3],
    /// Camera settings.
    pub camera: Camera,
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
}

impl crate::ExtractedCamera {
    /// Projects a top-left pixel rectangle of a complete image (ADR-8200).
    ///
    /// `image` is the full resolution; `rect` is `[x, y, width, height]` within it.
    /// The returned asymmetric projection keeps this camera's position and depth range.
    /// Its culling matrix covers the *complete* image so adjacent tiles retain the same
    /// visible objects. Use [`crate::FrameRenderer::render_camera_window`] to render a still
    /// tile with consistent full-image shadows and the documented still-effects policy.
    ///
    /// Returns `None` for an empty image/rectangle, overflow, or a rectangle outside the image.
    /// Invalid authored FOV and clip planes take the same finite fallbacks as ordinary views.
    #[must_use]
    pub fn window(&self, image: [u32; 2], rect: [u32; 4]) -> Option<crate::EyeCamera> {
        use regia_core::glam::{Mat4, Vec3, Vec4, camera::rh::proj::directx::perspective};
        let [x, y, width, height] = rect;
        if image.contains(&0)
            || width == 0
            || height == 0
            || x.checked_add(width)? > image[0]
            || y.checked_add(height)? > image[1]
        {
            return None;
        }
        let (fov, near, far) = crate::view_math::usable_projection(self.fov_y, self.near, self.far);
        let projection = perspective(fov, image[0] as f32 / image[1] as f32, near, far);
        let view = crate::view_math::look_at_view(
            Vec3::from_array(self.eye),
            Vec3::from_array(self.target),
            Vec3::Y,
        );
        // NDC x is left-to-right, NDC y bottom-to-top; framebuffer y is top-to-bottom.
        // Multiplication by this affine clip transform leaves z and w untouched.
        let sx = image[0] as f32 / width as f32;
        let sy = image[1] as f32 / height as f32;
        let tx = (image[0] as f64 - 2.0 * f64::from(x) - f64::from(width)) / f64::from(width);
        let ty = (2.0 * f64::from(y) + f64::from(height) - f64::from(image[1])) / f64::from(height);
        let crop = Mat4::from_cols(
            Vec4::new(sx, 0.0, 0.0, 0.0),
            Vec4::new(0.0, sy, 0.0, 0.0),
            Vec4::Z,
            Vec4::new(tx as f32, ty as f32, 0.0, 1.0),
        );
        Some(crate::EyeCamera {
            view,
            projection: crop * projection,
            cull: projection * view,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExtractedCamera;
    use regia_core::glam::{Vec3, Vec4};

    /// P-132: the render mask carries `@LayerMask("render")`, so the editor draws it as the bit
    /// grid (one toggle cell per render layer) instead of a plain number.
    #[test]
    fn camera_layer_mask_draws_as_a_layer_grid() {
        let attrs = regia_ecs::describe::field_attrs::<Camera>("layer_mask").unwrap_or_default();
        assert_eq!(attrs.layer_mask, Some("render"));
        // Only the attribute: the value and its default are unchanged.
        assert_eq!(Camera::default().layer_mask, u32::MAX);
    }

    #[test]
    fn camera_window_maps_full_image_pixels_to_tile_pixels_without_moving_depth() {
        let camera = ExtractedCamera {
            eye: [0.0, 0.0, 0.0],
            target: [0.0, 0.0, -1.0],
            fov_y: 1.0,
            near: 0.1,
            far: 100.0,
        };
        let full = camera.window([800, 600], [0, 0, 800, 600]).expect("full");
        assert_eq!(
            full.projection,
            regia_core::glam::camera::rh::proj::directx::perspective(1.0, 800.0 / 600.0, 0.1, 100.0)
        );
        assert_eq!(full.view, regia_core::glam::Mat4::IDENTITY);

        // Unequal edge tiles exercise aspect ratio and top-left pixel coordinates.
        for rect in [
            [0, 0, 400, 300],
            [400, 0, 400, 300],
            [0, 300, 400, 300],
            [400, 300, 400, 300],
            [701, 503, 99, 97],
        ] {
            let tile = camera.window([800, 600], rect).expect("tile");
            for point in [Vec3::new(-0.3, 0.2, -2.0), Vec3::new(1.0, -1.0, -4.0)] {
                let clip = full.projection * point.extend(1.0);
                let ndc = clip.truncate() / clip.w;
                let pixel = [(ndc.x + 1.0) * 400.0, (1.0 - ndc.y) * 300.0];
                let clip = tile.projection * point.extend(1.0);
                let actual = clip.truncate() / clip.w;
                let expected = Vec3::new(
                    2.0 * (pixel[0] - rect[0] as f32) / rect[2] as f32 - 1.0,
                    1.0 - 2.0 * (pixel[1] - rect[1] as f32) / rect[3] as f32,
                    ndc.z,
                );
                assert!(
                    (actual - expected).abs().max_element() < 4e-6,
                    "{rect:?}: {actual} != {expected}"
                );
            }
            assert_eq!(tile.view, full.view);
            assert_eq!(tile.cull, full.cull, "LOD/culling retain the complete frustum");
        }
        assert!((full.projection * Vec4::new(0.0, 0.0, -0.1, 1.0)).z.abs() < 1e-6);
    }

    #[test]
    fn camera_window_refuses_empty_overflowing_and_out_of_bounds_rectangles() {
        let camera = ExtractedCamera {
            eye: [0.0; 3],
            target: [0.0, 0.0, -1.0],
            fov_y: 1.0,
            near: 0.1,
            far: 100.0,
        };
        for rect in [
            [0, 0, 0, 10],
            [0, 0, 10, 0],
            [790, 0, 11, 10],
            [0, 590, 10, 11],
            [u32::MAX, 0, 10, 10],
        ] {
            assert!(camera.window([800, 600], rect).is_none());
        }
        assert!(camera.window([0, 600], [0, 0, 1, 1]).is_none());
    }
    #[test]
    fn camera_window_render_matches_full_capture_interior_and_seams() {
        use crate::{
            DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
            RenderDevice,
        };
        let device = RenderDevice::shared_headless().expect("lavapipe required");
        let quad = |z: f32, half: f32, material: usize| LabMesh {
            vertices: [
                [-half, -half, z],
                [half, -half, z],
                [half, half, z],
                [-half, half, z],
            ]
            .into_iter()
            .map(|p| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0, 0.0]))
            .collect(),
            indices: vec![0, 1, 2, 0, 2, 3],
            material,
            ..LabMesh::unskinned()
        };
        let lab = LabScene {
            width: 128,
            height: 96,
            eye: [0.0; 3],
            target: [0.0, 0.0, -1.0],
            fov_y: 1.0,
            near: 0.1,
            far: 100.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 4000.0,
            double_sided: true,
            post: Some(crate::PostSettings {
                // Tile-local effects must not change a still image at the seams.
                enabled: true,
                exposure_ev: 12.0,
                auto_exposure: true,
                bloom: true,
                fxaa: true,
                taa: true,
                vignette: 0.8,
                film_grain: 0.3,
                ..crate::PostSettings::default()
            }),
            directional: DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0; 3], 20000.0),
            local_lights: vec![],
            materials: vec![
                LabMaterial {
                    base_color: [0.2, 0.7, 0.3, 1.0],
                    ..Default::default()
                },
                LabMaterial {
                    base_color: [0.8, 0.2, 0.1, 1.0],
                    ..Default::default()
                },
            ],
            meshes: vec![quad(-8.0, 6.0, 0), quad(-2.0, 0.35, 1)],
        };
        let scene = lab.as_frame_scene_with_quality(FrameQuality::LAB);
        let mut renderer = FrameRenderer::new();
        let full = renderer
            .render_camera_window(&device, &scene, [0, 0, 128, 96])
            .expect("full");
        assert!(
            !renderer.complete_frames(),
            "caller's interactive mode is restored"
        );
        let mut joined = vec![0u8; full.pixels.len()];
        for [x, y, w, h] in [[0, 0, 64, 48], [64, 0, 64, 48], [0, 48, 64, 48], [64, 48, 64, 48]] {
            let tile = renderer
                .render_camera_window(&device, &scene, [x, y, w, h])
                .expect("tile");
            for row in 0..h as usize {
                let to = (((y as usize + row) * 128) + x as usize) * 4;
                let from = row * w as usize * 4;
                joined[to..to + w as usize * 4].copy_from_slice(&tile.pixels[from..from + w as usize * 4]);
            }
        }
        let (mut interior_sum, mut interior_n, mut seam_max) = (0u64, 0u64, 0u8);
        for y in 0..96usize {
            for x in 0..128usize {
                for c in 0..3 {
                    let at = (y * 128 + x) * 4 + c;
                    let diff = full.pixels[at].abs_diff(joined[at]);
                    if (62..=65).contains(&x) || (46..=49).contains(&y) {
                        seam_max = seam_max.max(diff);
                    } else {
                        interior_sum += u64::from(diff);
                        interior_n += 1;
                    }
                }
            }
        }
        let mean = interior_sum as f64 / interior_n as f64;
        eprintln!("camera window: interior mean absolute error {mean:.6}/255, seam maximum {seam_max}/255");
        assert!(mean <= 0.5, "interior {mean}");
        assert!(seam_max <= 3, "seam {seam_max}");
        assert!(
            full.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] > p[1].saturating_add(10)),
            "foreground is visible"
        );
        renderer.set_complete_frames(true);
        let odd = renderer
            .render_camera_window(&device, &scene, [67, 51, 61, 45])
            .expect("uneven edge tile");
        assert!(renderer.complete_frames(), "caller's complete mode is retained");
        renderer.set_complete_frames(false);
        let mut odd_sum = 0u64;
        for y in 0..45usize {
            for x in 0..61usize {
                for c in 0..3 {
                    odd_sum += u64::from(
                        odd.pixels[(y * 61 + x) * 4 + c]
                            .abs_diff(full.pixels[((y + 51) * 128 + x + 67) * 4 + c]),
                    );
                }
            }
        }
        assert!(
            odd_sum as f64 / (61.0 * 45.0 * 3.0) <= 0.5,
            "uneven crop uses full aspect"
        );
        let limit = device.device.limits().max_texture_dimension_2d;
        let mut panorama = scene.clone();
        panorama.width = limit + 1;
        panorama.height = 16;
        let left = renderer
            .render_camera_window(&device, &panorama, [0, 0, limit, 16])
            .expect("past-limit first tile");
        let right = renderer
            .render_camera_window(&device, &panorama, [limit, 0, 1, 16])
            .expect("past-limit last tile");
        assert_eq!(
            left.pixels.len() + right.pixels.len(),
            (limit as usize + 1) * 16 * 4
        );
        eprintln!(
            "camera window: rendered {} x 16 beyond device limit {limit}",
            limit + 1
        );
        assert!(
            renderer
                .render_camera_window(&device, &scene, [0, 0, 0, 1])
                .is_err()
        );
        let mut ordinary = scene.clone();
        ordinary.post = None;
        let after = renderer
            .render_offscreen(&device, &ordinary)
            .expect("state restored");
        let fresh = FrameRenderer::new()
            .render_offscreen(&device, &ordinary)
            .expect("fresh");
        assert_eq!(
            after.pixels, fresh.pixels,
            "window state does not leak into ordinary renders"
        );
    }
}
