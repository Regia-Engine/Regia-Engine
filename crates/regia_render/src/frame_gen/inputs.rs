// SPDX-License-Identifier: MPL-2.0
//! The per-frame description an interpolator dispatch needs, in Regia Engine's conventions
//! (ADR-514 § 2).
//!
//! The fields follow what the FidelityFX SDK v1.1.4 documents for its frame interpolation
//! dispatch (`docs/techniques/frame-interpolation.md`, MIT): display and render size, the
//! interpolation rectangle, the dilated depth and motion of the upscaler, the reconstructed
//! previous depth, the optical flow field and its block size, the camera's near and far planes and
//! vertical field of view, metres per view unit, the frame time, the reset flag, the back buffer's
//! transfer function and the HDR luminance range. The GPU resources themselves come from
//! `FrameRenderer::frame_gen_surfaces` (feature `frame-gen`); this type carries the numbers and
//! states Regia Engine's conventions once, so the WGSL port has a single place to read them from.

use regia_core::glam::Mat4;

use super::optical_flow;

/// How the HUD is kept out of the interpolation. The codes are part of the agent protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HudComposition {
    /// The overlay (runtime UI, quick draw, 2D) is recorded again over every presented frame,
    /// generated and real, so it is never interpolated. Regia Engine records the overlay after the scene
    /// in every frame already, which makes this the natural fit (the SDK's UI callback mode).
    Redraw,
    /// Accepted. The presenter treats this as [`Self::Redraw`]: it does not blend a separate UI
    /// texture over each frame.
    UiTexture,
    /// Accepted. The presenter treats this as [`Self::Redraw`]: the interpolator is not given a
    /// HUD-less color, and both presents get the overlay redraw.
    HudLess,
}

impl HudComposition {
    /// Every mode, in protocol order.
    pub const ALL: [Self; 3] = [Self::Redraw, Self::UiTexture, Self::HudLess];

    /// The protocol code (lowercase ASCII and `_`).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Redraw => "redraw",
            Self::UiTexture => "ui_texture",
            Self::HudLess => "hud_less",
        }
    }

    /// The mode named `code`.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.code() == code)
    }
}

/// How the presented colour is encoded (the SDK's back buffer transfer function).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferFunction {
    /// SDR, sRGB-encoded (an sRGB surface or display-encoded values in a UNORM surface).
    Srgb,
    /// HDR10: Rec.2020 primaries, SMPTE ST 2084.
    Pq,
    /// scRGB: linear Rec.709, 1.0 = 80 cd/m².
    ScRgb,
}

impl TransferFunction {
    /// The encoding of a surface with this colour space, as the display stage picks it.
    #[must_use]
    pub fn for_surface(space: wgpu::SurfaceColorSpace) -> Self {
        match space {
            wgpu::SurfaceColorSpace::Bt2100Pq => Self::Pq,
            wgpu::SurfaceColorSpace::ExtendedSrgbLinear => Self::ScRgb,
            _ => Self::Srgb,
        }
    }

    /// The protocol code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Srgb => "srgb",
            Self::Pq => "pq",
            Self::ScRgb => "scrgb",
        }
    }

    /// Whether the encoding is HDR (the SDK's HDR colour input flag).
    #[must_use]
    pub const fn is_hdr(self) -> bool {
        matches!(self, Self::Pq | Self::ScRgb)
    }
}

/// The depth buffer's layout (the SDK's inverted and infinite depth flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DepthConvention {
    /// Near is 1 and far is 0 (reverse Z).
    pub inverted: bool,
    /// The far plane is at infinity.
    pub infinite: bool,
}

impl DepthConvention {
    /// Regia Engine's perspective cameras: standard depth (0 near, 1 far, depth test `LessEqual`)
    /// with a finite far plane. ADR-514 wrote reverse Z here; the renderer never used it
    /// (ADR-2067).
    pub const REGIA: Self = Self {
        inverted: false,
        infinite: false,
    };

    /// Regia Engine's standard depth, infinite when `far` is not finite.
    #[must_use]
    pub fn for_far(far: f32) -> Self {
        Self {
            inverted: false,
            infinite: !far.is_finite(),
        }
    }
}

/// The layout of the dilated motion the FSR 3.1 upscaler writes (`fsr3_prepare_inputs.wgsl`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionVectorConvention {
    /// Vectors are in UV units of the render target (1 = the whole width or height).
    pub uv_units: bool,
    /// A vector points from the pixel's current position to its previous position
    /// (`previous uv − current uv`), so `current + vector = previous`.
    pub points_to_previous: bool,
    /// Factors applied before use (the SDK's motion vector scale).
    pub scale: [f32; 2],
}

impl MotionVectorConvention {
    /// The upscaler's dilated motion: UV units, current → previous, unscaled. Camera motion is
    /// included (the prepare pass reprojects depth), so the field is the full screen motion.
    pub const REGIA: Self = Self {
        uv_units: true,
        points_to_previous: true,
        scale: [1.0, 1.0],
    };

    /// `vector` in pixels of a `size` target, pointing to the previous position.
    #[must_use]
    pub fn to_pixels_toward_previous(&self, vector: [f32; 2], size: (u32, u32)) -> [f32; 2] {
        let units = if self.uv_units {
            [size.0 as f32, size.1 as f32]
        } else {
            [1.0, 1.0]
        };
        let sign = if self.points_to_previous { 1.0 } else { -1.0 };
        [
            vector[0] * self.scale[0] * units[0] * sign,
            vector[1] * self.scale[1] * units[1] * sign,
        ]
    }
}

/// The camera numbers an interpolator needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameGenCamera {
    /// Near plane distance (metres).
    pub near: f32,
    /// Far plane distance (metres); `f32::INFINITY` for an infinite projection.
    pub far: f32,
    /// Vertical field of view (radians).
    pub fov_vertical: f32,
    /// Metres per view-space unit (1: Regia Engine's view space is in metres).
    pub view_space_to_meters: f32,
}

impl FrameGenCamera {
    /// From a perspective projection matrix (any depth range): the vertical field of view is
    /// `2 · atan(1 / P[1][1])`.
    #[must_use]
    pub fn from_projection(projection: Mat4, near: f32, far: f32) -> Self {
        let y_scale = projection.y_axis.y;
        let fov_vertical = if y_scale.abs() > f32::EPSILON {
            2.0 * (1.0 / y_scale.abs()).atan()
        } else {
            0.0
        };
        Self {
            near,
            far,
            fov_vertical,
            view_space_to_meters: 1.0,
        }
    }
}

/// Why a [`FrameGenDispatch`] is refused. The codes are part of the agent protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameGenInputError {
    /// The display or render size is zero.
    EmptySize,
    /// The render size exceeds the display size (frame generation runs after upscaling).
    RenderLargerThanDisplay,
    /// The interpolation rectangle is empty or leaves the display.
    RectOutsideDisplay,
    /// Near is not positive, far is not beyond near, or the field of view is outside (0, π).
    BadCamera,
    /// The frame time is not a positive finite number of milliseconds.
    BadFrameTime,
    /// The HDR luminance range is not `0 ≤ min < max`.
    BadLuminance,
}

impl FrameGenInputError {
    /// The protocol code (lowercase ASCII and `_`).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::EmptySize => "empty_size",
            Self::RenderLargerThanDisplay => "render_larger_than_display",
            Self::RectOutsideDisplay => "rect_outside_display",
            Self::BadCamera => "bad_camera",
            Self::BadFrameTime => "bad_frame_time",
            Self::BadLuminance => "bad_luminance",
        }
    }
}

impl std::fmt::Display for FrameGenInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for FrameGenInputError {}

/// One interpolator dispatch: the numbers beside the GPU surfaces.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameGenDispatch {
    /// The presented size (the interpolated frame's size).
    pub display_size: (u32, u32),
    /// The size the scene rendered at; the dilated depth and motion have this size.
    pub render_size: (u32, u32),
    /// `[x, y, width, height]` of the display that is interpolated (letterbox bars stay out).
    pub interpolation_rect: [u32; 4],
    /// Camera numbers.
    pub camera: FrameGenCamera,
    /// Depth layout.
    pub depth: DepthConvention,
    /// Motion vector layout.
    pub motion: MotionVectorConvention,
    /// The presented colour's encoding.
    pub transfer: TransferFunction,
    /// `[min, max]` display luminance in cd/m² for the HDR encodings.
    pub min_max_luminance: [f32; 2],
    /// Time since the previous real frame (milliseconds).
    pub frame_time_delta_ms: f32,
    /// A camera cut or history reset: the interpolator copies the real frame instead.
    pub reset: bool,
    /// HUD handling.
    pub hud: HudComposition,
    /// Optical flow block size in pixels (8, the SDK's only size).
    pub flow_block_size: u32,
    /// Draw the tear-line bars on generated frames.
    pub debug_tear_lines: bool,
    /// Draw the interpolator's internal surfaces on generated frames.
    pub debug_view: bool,
}

impl FrameGenDispatch {
    /// A dispatch with Regia Engine's conventions, the whole display interpolated, SDR output and the
    /// HUD redrawn.
    #[must_use]
    pub fn new(
        display_size: (u32, u32),
        render_size: (u32, u32),
        camera: FrameGenCamera,
        frame_time_delta_ms: f32,
        reset: bool,
    ) -> Self {
        Self {
            display_size,
            render_size,
            interpolation_rect: [0, 0, display_size.0, display_size.1],
            camera,
            depth: DepthConvention::for_far(camera.far),
            motion: MotionVectorConvention::REGIA,
            transfer: TransferFunction::Srgb,
            min_max_luminance: [0.0, 80.0],
            frame_time_delta_ms,
            reset,
            hud: HudComposition::Redraw,
            flow_block_size: optical_flow::BLOCK_SIZE,
            debug_tear_lines: false,
            debug_view: false,
        }
    }

    /// The optical flow field's size for this display: one vector per block.
    #[must_use]
    pub fn optical_flow_size(&self) -> (u32, u32) {
        optical_flow::flow_field_size(self.display_size, self.flow_block_size)
    }

    /// Checks the numbers before a dispatch.
    pub fn validate(&self) -> Result<(), FrameGenInputError> {
        let (dw, dh) = self.display_size;
        let (rw, rh) = self.render_size;
        if dw == 0 || dh == 0 || rw == 0 || rh == 0 || self.flow_block_size == 0 {
            return Err(FrameGenInputError::EmptySize);
        }
        if rw > dw || rh > dh {
            return Err(FrameGenInputError::RenderLargerThanDisplay);
        }
        let [x, y, w, h] = self.interpolation_rect;
        let inside =
            |start: u32, len: u32, total: u32| start.checked_add(len).is_some_and(|end| end <= total);
        if w == 0 || h == 0 || !inside(x, w, dw) || !inside(y, h, dh) {
            return Err(FrameGenInputError::RectOutsideDisplay);
        }
        let camera = &self.camera;
        let fov_ok = camera.fov_vertical > 0.0 && camera.fov_vertical < std::f32::consts::PI;
        let near_ok = camera.near.is_finite() && camera.near > 0.0;
        let far_ok = camera.far > camera.near && (camera.far.is_finite() || self.depth.infinite);
        let scale_ok = camera.view_space_to_meters.is_finite() && camera.view_space_to_meters > 0.0;
        let camera_ok = fov_ok && near_ok && far_ok && scale_ok;
        if !camera_ok {
            return Err(FrameGenInputError::BadCamera);
        }
        let time_ok = self.frame_time_delta_ms.is_finite() && self.frame_time_delta_ms > 0.0;
        if !time_ok {
            return Err(FrameGenInputError::BadFrameTime);
        }
        let [min, max] = self.min_max_luminance;
        let luminance_ok = min.is_finite() && max.is_finite() && (0.0..max).contains(&min);
        if self.transfer.is_hdr() && !luminance_ok {
            return Err(FrameGenInputError::BadLuminance);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::glam::camera::rh::proj::directx as proj;

    fn camera() -> FrameGenCamera {
        let fov = 60f32.to_radians();
        FrameGenCamera::from_projection(proj::perspective(fov, 16.0 / 9.0, 0.1, 1000.0), 0.1, 1000.0)
    }

    #[test]
    fn the_field_of_view_comes_back_from_regias_projection() {
        let cam = camera();
        assert!(
            (cam.fov_vertical - 60f32.to_radians()).abs() < 1.0e-5,
            "{}",
            cam.fov_vertical
        );
        assert_eq!(cam.view_space_to_meters, 1.0);
        // Any other projection gives its angle too.
        let reverse = proj::perspective_infinite_reverse(45f32.to_radians(), 1.0, 0.5);
        let cam = FrameGenCamera::from_projection(reverse, 0.5, f32::INFINITY);
        assert!((cam.fov_vertical - 45f32.to_radians()).abs() < 1.0e-5);
    }

    #[test]
    fn regia_depth_is_standard_and_infinite_only_without_a_far_plane() {
        assert_eq!(DepthConvention::for_far(1000.0), DepthConvention::REGIA);
        assert_eq!(
            DepthConvention::for_far(f32::INFINITY),
            DepthConvention {
                inverted: false,
                infinite: true
            }
        );
        let d = FrameGenDispatch::new((1920, 1080), (1280, 720), camera(), 16.6, false);
        assert_eq!(d.depth, DepthConvention::REGIA);
    }

    #[test]
    fn motion_vectors_point_to_the_previous_position_in_pixels() {
        // An object that moved 32 px right and 9 px down since the last frame: its previous
        // position is to the left and above, so the stored vector is (−32, −9) px in UV units.
        let size = (1280, 720);
        let uv = [-32.0 / 1280.0, -9.0 / 720.0];
        let px = MotionVectorConvention::REGIA.to_pixels_toward_previous(uv, size);
        assert!(
            (px[0] + 32.0).abs() < 1.0e-4 && (px[1] + 9.0).abs() < 1.0e-4,
            "{px:?}"
        );
        // A convention that points forward flips the sign.
        let forward = MotionVectorConvention {
            points_to_previous: false,
            ..MotionVectorConvention::REGIA
        };
        let px = forward.to_pixels_toward_previous(uv, size);
        assert!((px[0] - 32.0).abs() < 1.0e-4);
    }

    #[test]
    fn hdr_surfaces_pick_their_transfer_function() {
        assert_eq!(
            TransferFunction::for_surface(wgpu::SurfaceColorSpace::Bt2100Pq),
            TransferFunction::Pq
        );
        assert_eq!(
            TransferFunction::for_surface(wgpu::SurfaceColorSpace::ExtendedSrgbLinear),
            TransferFunction::ScRgb
        );
        assert_eq!(
            TransferFunction::for_surface(wgpu::SurfaceColorSpace::Srgb),
            TransferFunction::Srgb
        );
        assert!(TransferFunction::Pq.is_hdr() && !TransferFunction::Srgb.is_hdr());
    }

    #[test]
    fn a_valid_dispatch_passes_and_each_bad_field_is_named() {
        let good = FrameGenDispatch::new((1920, 1080), (1280, 720), camera(), 16.6, false);
        assert_eq!(good.validate(), Ok(()));
        assert_eq!(good.optical_flow_size(), (240, 135));

        let cases: [(FrameGenDispatch, FrameGenInputError); 7] = [
            (
                FrameGenDispatch {
                    render_size: (0, 720),
                    ..good
                },
                FrameGenInputError::EmptySize,
            ),
            (
                FrameGenDispatch {
                    render_size: (2560, 1440),
                    ..good
                },
                FrameGenInputError::RenderLargerThanDisplay,
            ),
            (
                FrameGenDispatch {
                    interpolation_rect: [0, 100, 1920, 1000],
                    ..good
                },
                FrameGenInputError::RectOutsideDisplay,
            ),
            (
                FrameGenDispatch {
                    interpolation_rect: [u32::MAX, 0, 2, 2],
                    ..good
                },
                FrameGenInputError::RectOutsideDisplay,
            ),
            (
                FrameGenDispatch {
                    camera: FrameGenCamera {
                        near: 0.0,
                        ..camera()
                    },
                    ..good
                },
                FrameGenInputError::BadCamera,
            ),
            (
                FrameGenDispatch {
                    frame_time_delta_ms: f32::NAN,
                    ..good
                },
                FrameGenInputError::BadFrameTime,
            ),
            (
                FrameGenDispatch {
                    transfer: TransferFunction::Pq,
                    min_max_luminance: [500.0, 400.0],
                    ..good
                },
                FrameGenInputError::BadLuminance,
            ),
        ];
        for (dispatch, expected) in cases {
            assert_eq!(dispatch.validate(), Err(expected));
            let code = expected.code();
            assert!(
                code.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'),
                "{code}"
            );
        }
        // Letterboxed: only the picture between the bars is interpolated.
        let letterbox = FrameGenDispatch {
            interpolation_rect: [0, 140, 1920, 800],
            ..good
        };
        assert_eq!(letterbox.validate(), Ok(()));
    }

    #[test]
    fn hud_codes_round_trip() {
        for mode in HudComposition::ALL {
            assert_eq!(HudComposition::from_code(mode.code()), Some(mode));
        }
        assert_eq!(HudComposition::from_code("hudless"), None);
    }
}
