// SPDX-License-Identifier: MPL-2.0
//! Scripted capture of a product (ADR-2757, `book/agents/capture-hooks.md`): a build, a camera path and timed
//! picks, resolved to frames. Nothing here reads a clock: a frame's picks and camera come from its index alone.
//!
//! [`check_script`] walks the script with no GPU and refuses what the player would refuse: a build or pick the
//! snapshot does not sell, a camera outside the recipe's `view_range` (binding, `interfaces.md` § 3). On a native
//! host [`CaptureRig`] then draws it through the same [`Player`](crate::player::Player) and shaders as the web
//! player and the Android viewer, into an offscreen texture, so a captured frame is what those players draw.

use serde_json::{Value, json};

use crate::WebError;
use crate::product::PackageProduct;
use crate::seams::{CameraPose, ProductModel, ViewModel, ViewValues};
use crate::view::ProductView;

/// How far a requested view may be from its clamped value before the script is refused, in degrees, mm or zoom.
const VIEW_TOLERANCE: f32 = 1e-3;

/// The hash of a frame's pixels as the render-out manifest writes it: FNV-1a 64 over the RGBA8 bytes, 16 hex
/// digits. Equal pixels hash equal on any host.
#[must_use]
pub fn frame_hash(rgba: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in rgba {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// How a view moves into a key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Ease {
    /// Smoothstep from the key before (the default).
    #[default]
    Smooth,
    /// Straight from the key before.
    Linear,
    /// Holds the key before, then jumps on this key's frame.
    Hold,
}

impl Ease {
    /// The spelling a script uses: `smooth`, `linear` or `hold`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "smooth" => Some(Self::Smooth),
            "linear" => Some(Self::Linear),
            "hold" => Some(Self::Hold),
            _ => None,
        }
    }

    fn apply(self, t: f64) -> f64 {
        match self {
            Self::Smooth => t * t * (3.0 - 2.0 * t),
            Self::Linear => t,
            Self::Hold => 0.0,
        }
    }
}

/// A timed `setPick`.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptPick {
    /// The frame the pick applies on, before that frame draws.
    pub frame: u64,
    /// The option group.
    pub group: String,
    /// A label, or a list for a multi group.
    pub value: Value,
    /// `setPick`'s `adjust`: find the sold build that keeps this pick and changes the fewest other groups.
    pub adjust: bool,
}

/// A timed `setView`. What a key leaves out stays at the key before it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScriptViewKey {
    /// The frame the view reaches this key.
    pub frame: u64,
    /// New tilt in degrees.
    pub tilt: Option<[f32; 2]>,
    /// New spin in degrees.
    pub spin: Option<f32>,
    /// New pan in mm.
    pub pan: Option<[f32; 2]>,
    /// New zoom.
    pub zoom: Option<f32>,
    /// How the view moves into this key.
    pub ease: Ease,
}

/// The camera of a script.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ScriptCamera {
    /// The view the build was saved at (the product's default view without one), held.
    #[default]
    Hold,
    /// Keys in `setView`'s terms, from the start view; sorted by frame. After the last key the view holds.
    View(Vec<ScriptViewKey>),
    /// A camera path already sampled: one pose per frame, in mm in the piece's frame.
    Poses(Vec<CameraPose>),
}

/// A capture: what happens on which frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaptureScript {
    /// Frames on the timeline.
    pub frames: u64,
    /// The timed picks, in the order they apply.
    pub picks: Vec<ScriptPick>,
    /// The camera.
    pub camera: ScriptCamera,
}

/// Why a script was refused, with the frame it happens on when there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureRefusal {
    /// `build`, `pick` or `view`.
    pub kind: &'static str,
    /// What is wrong, naming the frame.
    pub message: String,
}

impl std::fmt::Display for CaptureRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A pick the walk accepted.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedPick {
    /// The frame it applies on.
    pub frame: u64,
    /// The option group.
    pub group: String,
    /// The value.
    pub value: Value,
    /// Groups changed on top of it (`adjust` only).
    pub adjusted: Vec<String>,
    /// The hash of the build after it.
    pub build_hash: Value,
    /// Whether that build resolves to something sold.
    pub orderable: Value,
}

/// What [`check_script`] found: a script the rig can run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CheckedScript {
    /// Where the view starts.
    pub start_view: ViewValues,
    /// The build at frame 0 before any pick.
    pub start_build: Value,
    /// The picks on the timeline, in order.
    pub picks: Vec<AppliedPick>,
    /// Picks at or past the end of the timeline, dropped.
    pub picks_past_end: usize,
    /// The build the timeline started from names another recipe than the package's.
    pub recipe_changed: bool,
    /// ... or another catalogue snapshot.
    pub snapshot_changed: bool,
    /// What fell back to a neutral look on some frame: a zone with no usable library entry.
    pub problems: Vec<String>,
}

/// Opens a product's view at the delivered size of a capture: its limits depend on the pixels (`max_px_per_mm`).
///
/// # Errors
/// `package_invalid` when the recipe's view does not read.
pub fn view_at_size(product: &mut PackageProduct, size: [u32; 2]) -> Result<ProductView, WebError> {
    let mut view = ProductView::for_product(product)?;
    view.set_viewport(size[0].max(1) as f32, size[1].max(1) as f32);
    Ok(view)
}

fn refused(kind: &'static str, message: String) -> CaptureRefusal {
    CaptureRefusal { kind, message }
}

fn close(a: &ViewValues, b: &ViewValues) -> bool {
    let near = |x: f32, y: f32| (x - y).abs() <= VIEW_TOLERANCE;
    near(a.tilt[0], b.tilt[0])
        && near(a.tilt[1], b.tilt[1])
        && near(a.spin, b.spin)
        && near(a.pan[0], b.pan[0])
        && near(a.pan[1], b.pan[1])
        && near(a.zoom, b.zoom)
}

/// The view of frame `frame`: the keys in frame order from `start`, with an implicit key at `start` on frame 0.
/// Keys that share a frame keep the order they were given.
#[must_use]
pub fn view_at(keys: &[ScriptViewKey], start: ViewValues, frame: u64) -> ViewValues {
    let mut ordered = keys.to_vec();
    ordered.sort_by_key(|key| key.frame);
    let mut before = start;
    let mut before_frame = 0;
    for key in &ordered {
        let reached = ViewValues {
            tilt: key.tilt.unwrap_or(before.tilt),
            spin: key.spin.unwrap_or(before.spin),
            pan: key.pan.unwrap_or(before.pan),
            zoom: key.zoom.unwrap_or(before.zoom),
        };
        if frame >= key.frame {
            before = reached;
            before_frame = key.frame;
            continue;
        }
        let span = (key.frame - before_frame) as f64;
        let t = key.ease.apply((frame - before_frame) as f64 / span);
        let mix = |a: f32, b: f32| (f64::from(a) + (f64::from(b) - f64::from(a)) * t) as f32;
        return ViewValues {
            tilt: [
                mix(before.tilt[0], reached.tilt[0]),
                mix(before.tilt[1], reached.tilt[1]),
            ],
            spin: mix(before.spin, reached.spin),
            pan: [
                mix(before.pan[0], reached.pan[0]),
                mix(before.pan[1], reached.pan[1]),
            ],
            zoom: mix(before.zoom, reached.zoom),
        };
    }
    before
}

fn field(build: &Value, pointer: &str) -> Value {
    build.pointer(pointer).cloned().unwrap_or(Value::Null)
}

/// Adds what the product's current picks draw with a fallback look, and refuses picks it cannot draw at all.
fn note_problems(
    product: &PackageProduct,
    problems: &mut Vec<String>,
    when: &str,
) -> Result<(), CaptureRefusal> {
    let plan = product.plan().map_err(|e| {
        refused(
            "pick",
            format!("{when}: the picks cannot be drawn: {}", e.message),
        )
    })?;
    for problem in plan.problems {
        if !problems.contains(&problem) {
            problems.push(problem);
        }
    }
    Ok(())
}

/// Walks `script` on `product` with no GPU and leaves the product where frame 0 starts: every pick applied and
/// undone, every frame's camera checked against the recipe's limits. `build` is the `jewelry.build/1` the
/// timeline starts from.
///
/// # Errors
/// A [`CaptureRefusal`] naming the build, or the frame, group and value. Nothing is rendered for a script that
/// a player would refuse part way through.
pub fn check_script(
    product: &mut PackageProduct,
    view: &ProductView,
    build: Option<&Value>,
    script: &CaptureScript,
) -> Result<CheckedScript, CaptureRefusal> {
    let mut checked = CheckedScript::default();
    if let Some(build) = build {
        let (report, start) = product
            .load_build(build)
            .map_err(|e| refused("build", format!("the build cannot open: {}", e.message)))?;
        if !report.orderable_now {
            return Err(refused(
                "build",
                format!(
                    "the build is no longer orderable against this package's snapshot: {}",
                    report.resolution
                ),
            ));
        }
        checked.start_view = start;
        checked.recipe_changed = report.recipe_changed;
        checked.snapshot_changed = report.snapshot_changed;
    }
    let start_build = product
        .build(&checked.start_view)
        .map_err(|e| refused("build", format!("the start cannot make a build: {}", e.message)))?;
    note_problems(product, &mut checked.problems, "the start")?;
    match &script.camera {
        ScriptCamera::Poses(poses) => {
            if poses.len() as u64 != script.frames {
                return Err(refused(
                    "view",
                    format!(
                        "a camera path has {} poses for {} frames",
                        poses.len(),
                        script.frames
                    ),
                ));
            }
            for (frame, pose) in poses.iter().enumerate() {
                if let Some(why) = view.pose_outside(pose) {
                    return Err(refused("view", format!("frame {frame}: {why}")));
                }
            }
        }
        ScriptCamera::View(keys) => {
            for frame in 0..script.frames {
                let wanted = view_at(keys, checked.start_view, frame);
                let held = view.clamped(wanted);
                if !close(&wanted, &held) {
                    return Err(refused(
                        "view",
                        format!(
                            "frame {frame}: the view {wanted:?} is outside the limits at this size (held at {held:?})"
                        ),
                    ));
                }
            }
        }
        ScriptCamera::Hold => {
            let held = view.clamped(checked.start_view);
            if !close(&checked.start_view, &held) {
                return Err(refused(
                    "view",
                    format!(
                        "the build's view {:?} is outside the limits at this size (held at {held:?})",
                        checked.start_view
                    ),
                ));
            }
        }
    }
    for pick in &script.picks {
        if pick.frame >= script.frames {
            checked.picks_past_end += 1;
            continue;
        }
        let applied = product
            .set_pick(&pick.group, &pick.value, pick.adjust)
            .map_err(|e| {
                refused(
                    "pick",
                    format!(
                        "frame {}: setPick({:?}, {}) was refused with {}: {}",
                        pick.frame, pick.group, pick.value, e.code, e.message
                    ),
                )
            })?;
        let after = product
            .build(&checked.start_view)
            .map_err(|e| refused("pick", format!("frame {}: {}", pick.frame, e.message)))?;
        note_problems(product, &mut checked.problems, &format!("frame {}", pick.frame))?;
        checked.picks.push(AppliedPick {
            frame: pick.frame,
            group: pick.group.clone(),
            value: pick.value.clone(),
            adjusted: applied.adjusted,
            build_hash: field(&after, "/build_hash"),
            orderable: field(&after, "/resolution/orderable"),
        });
    }
    // Back to where frame 0 starts: every pick of the walk undone, the build's wear and camera as saved.
    product
        .load_build(&start_build)
        .map_err(|e| refused("build", format!("the start cannot reopen: {}", e.message)))?;
    checked.start_build = start_build;
    Ok(checked)
}

/// The `camera` row of a frame, for the render-out manifest.
#[must_use]
pub fn pose_json(frame: u64, pose: &CameraPose) -> Value {
    json!({
        "frame": frame,
        "eye_mm": pose.eye,
        "look_at_mm": pose.look_at,
        "up": pose.up,
        "fov_short_deg": pose.fov_short_deg,
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub use rig::{CaptureRig, CaptureRigError, RigFrame};

#[cfg(not(target_arch = "wasm32"))]
mod rig {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use regia_viewer::{BakedEnv, Still, ViewerError};

    use super::{CaptureScript, CheckedScript, ScriptCamera, view_at, view_at_size};
    use crate::WebError;
    use crate::player::{Player, PlayerEvent};
    use crate::product::PackageProduct;
    use crate::scene::{FrameTarget, ViewerScene, player_config};
    use crate::seams::{CameraPose, DrawOutcome, PointerClass, ViewModel, ViewRequest, ViewValues};
    use crate::view::ProductView;

    /// What frames are drawn in: encoded sRGB, the format of the player's own stills.
    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

    fn texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("capture frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }

    /// A frame target with no window: every frame is read back.
    struct OffscreenTarget {
        device: wgpu::Device,
        queue: wgpu::Queue,
        texture: wgpu::Texture,
        last: Rc<RefCell<Option<Still>>>,
    }

    impl OffscreenTarget {
        fn read(&self) -> Result<Still, WebError> {
            let failed = |e: String| WebError::new("render_failed", e);
            let (width, height) = (self.texture.width(), self.texture.height());
            let row = (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("capture readback"),
                size: u64::from(row) * u64::from(height),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("capture readback"),
                });
            encoder.copy_texture_to_buffer(
                self.texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row),
                        rows_per_image: Some(height),
                    },
                },
                self.texture.size(),
            );
            self.queue.submit([encoder.finish()]);
            let slice = buffer.slice(..);
            slice.map_async(wgpu::MapMode::Read, |_| {});
            self.device
                .poll(wgpu::PollType::wait_indefinitely())
                .map_err(|e| failed(e.to_string()))?;
            let mapped = slice.get_mapped_range().map_err(|e| failed(e.to_string()))?;
            let wide = (width * 4) as usize;
            let mut rgba = Vec::with_capacity(wide * height as usize);
            for line in mapped.chunks_exact(row as usize) {
                rgba.extend_from_slice(&line[..wide]);
            }
            drop(mapped);
            buffer.unmap();
            Ok(Still { width, height, rgba })
        }
    }

    impl FrameTarget for OffscreenTarget {
        fn resize(&mut self, width: u32, height: u32) {
            self.texture = texture(&self.device, width.max(1), height.max(1));
        }

        fn with_frame(
            &mut self,
            draw: &mut dyn FnMut(&wgpu::Texture) -> Result<(), WebError>,
        ) -> Result<DrawOutcome, WebError> {
            draw(&self.texture)?;
            *self.last.borrow_mut() = Some(self.read()?);
            Ok(DrawOutcome::Drawn)
        }
    }

    /// A product's view at a capture's delivered size whatever size the frames render at, so supersampling moves
    /// no limit. A camera path's pose replaces its own.
    struct CaptureView {
        inner: ProductView,
        delivered: [f32; 2],
        path_pose: Rc<Cell<Option<CameraPose>>>,
    }

    impl ViewModel for CaptureView {
        fn pointer_down(&mut self, id: u64, class: PointerClass, x: f32, y: f32, t_ms: f64) {
            self.inner.pointer_down(id, class, x, y, t_ms);
        }

        fn pointer_move(&mut self, id: u64, x: f32, y: f32, t_ms: f64) {
            self.inner.pointer_move(id, x, y, t_ms);
        }

        fn pointer_up(&mut self, id: u64, t_ms: f64) {
            self.inner.pointer_up(id, t_ms);
        }

        fn pointer_cancel(&mut self, id: u64) {
            self.inner.pointer_cancel(id);
        }

        fn wheel(&mut self, lines: f32) {
            self.inner.wheel(lines);
        }

        fn set_viewport(&mut self, _width: f32, _height: f32) {
            self.inner.set_viewport(self.delivered[0], self.delivered[1]);
        }

        fn update(&mut self, dt_s: f32) -> bool {
            self.inner.update(dt_s)
        }

        fn is_animating(&self) -> bool {
            self.inner.is_animating()
        }

        fn set_view(&mut self, view: ViewValues, immediate: bool) -> ViewValues {
            self.inner.set_view(view, immediate)
        }

        fn reset(&mut self) {
            self.inner.reset();
        }

        fn shown(&self) -> ViewValues {
            self.inner.shown()
        }

        fn pose(&self) -> CameraPose {
            self.path_pose.get().unwrap_or_else(|| self.inner.pose())
        }
    }

    /// The `setView` of a frame.
    fn view_request(view: ViewValues) -> ViewRequest {
        ViewRequest {
            tilt: Some(view.tilt),
            spin: Some(view.spin),
            pan: Some(view.pan),
            zoom: Some(view.zoom),
            animate: Some(false),
        }
    }

    /// One frame a rig drew.
    #[derive(Debug)]
    pub struct RigFrame {
        /// The picture at the render size.
        pub still: Still,
        /// The camera it was drawn from.
        pub pose: CameraPose,
    }

    /// The whole player on a headless device, offscreen. The web player's core and shaders: only the surface differs.
    pub struct CaptureRig {
        player: Player,
        last: Rc<RefCell<Option<Still>>>,
        path_pose: Rc<Cell<Option<CameraPose>>>,
        adapter: wgpu::AdapterInfo,
    }

    impl CaptureRig {
        /// A player on its own headless device with `product` open, in deterministic mode, drawing `render` pixels
        /// and holding the view's limits at `delivered` pixels. Every pipeline is built here.
        ///
        /// # Errors
        /// [`ViewerError`] when no adapter or device is to be had; `WebError` text otherwise.
        pub fn new(
            mut product: PackageProduct,
            render: [u32; 2],
            delivered: [u32; 2],
            env: &BakedEnv,
        ) -> Result<Self, CaptureRigError> {
            let (adapter, device, queue) = regia_viewer::gpu::headless_device()?;
            let samples = regia_viewer::gpu::sample_count_for(&adapter, FORMAT);
            let last = Rc::new(RefCell::new(None));
            let (width, height) = (render[0].max(1), render[1].max(1));
            check_render_size(width, height, &device.limits())?;
            let target = OffscreenTarget {
                device: device.clone(),
                queue: queue.clone(),
                texture: texture(&device, width, height),
                last: Rc::clone(&last),
            };
            let scene = ViewerScene::new(device, queue, player_config(FORMAT, samples), env, target)?;
            let path_pose = Rc::new(Cell::new(None));
            let view = CaptureView {
                inner: view_at_size(&mut product, delivered)?,
                delivered: [delivered[0].max(1) as f32, delivered[1].max(1) as f32],
                path_pose: Rc::clone(&path_pose),
            };
            let mut player = Player::new(Box::new(|| 0.0));
            player.set_deterministic(true);
            player.resize([width as f32, height as f32], [width, height]);
            player.attach_scene(Box::new(scene));
            player.install(Box::new(product), Box::new(view))?;
            Ok(Self {
                player,
                last,
                path_pose,
                adapter: adapter.get_info(),
            })
        }

        /// The player, for a script of its own (`set_pick`, `set_view`, `step`).
        pub fn player(&mut self) -> &mut Player {
            &mut self.player
        }

        /// The adapter's name and backend.
        #[must_use]
        pub fn adapter(&self) -> (String, String) {
            (self.adapter.name.clone(), format!("{:?}", self.adapter.backend))
        }

        /// Draws a frame of `dt_ms` and takes what the player reported.
        ///
        /// # Errors
        /// The player's error, or `render_failed` when no frame reached the target.
        pub fn draw(&mut self, dt_ms: f64) -> Result<RigFrame, WebError> {
            self.player.step_forced(dt_ms)?;
            for event in self.player.drain_events() {
                if let PlayerEvent::Error(error) = event {
                    return Err(error);
                }
            }
            let still = self
                .last
                .borrow_mut()
                .take()
                .ok_or_else(|| WebError::new("render_failed", "no frame reached the target"))?;
            let pose = self
                .player
                .pose()
                .ok_or_else(|| WebError::new("no_product", "open a product first"))?;
            Ok(RigFrame { still, pose })
        }

        /// Runs one frame of a checked script: its picks, its camera, the draw.
        ///
        /// # Errors
        /// The player's error for a frame the check did not foresee.
        pub fn script_frame(
            &mut self,
            script: &CaptureScript,
            checked: &CheckedScript,
            frame: u64,
            dt_ms: f64,
        ) -> Result<RigFrame, WebError> {
            let _span = tracing::info_span!("capture_frame").entered();
            for pick in script.picks.iter().filter(|pick| pick.frame == frame) {
                self.player.set_pick(&pick.group, &pick.value, pick.adjust)?;
            }
            match &script.camera {
                ScriptCamera::Hold => {
                    if frame == 0 {
                        self.player.set_view(&view_request(checked.start_view))?;
                    }
                }
                ScriptCamera::View(keys) => {
                    let view = view_at(keys, checked.start_view, frame);
                    self.player.set_view(&view_request(view))?;
                }
                ScriptCamera::Poses(poses) => {
                    // A missing pose keeps the previous path camera. Clearing it would draw the drag pose.
                    if let Some(pose) = usize::try_from(frame).ok().and_then(|i| poses.get(i)).copied() {
                        self.path_pose.set(Some(pose));
                    }
                }
            }
            self.draw(dt_ms)
        }
    }

    /// Why a rig could not be made.
    #[derive(Debug)]
    pub enum CaptureRigError {
        /// No adapter or no device.
        Device(ViewerError),
        /// The scene refused.
        Scene(WebError),
    }

    impl From<ViewerError> for CaptureRigError {
        fn from(error: ViewerError) -> Self {
            Self::Device(error)
        }
    }

    impl From<WebError> for CaptureRigError {
        fn from(error: WebError) -> Self {
            Self::Scene(error)
        }
    }

    impl std::fmt::Display for CaptureRigError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Device(error) => write!(f, "{error}"),
                Self::Scene(error) => write!(f, "{error}"),
            }
        }
    }

    /// Refuses a render the device cannot draw or read back. Past a limit wgpu raises a validation
    /// error, which panics without an error scope: a texture side past `max_texture_dimension_2d`
    /// in [`CaptureRig::new`], a readback past `max_buffer_size` at the first frame (viewer devices
    /// keep WebGL2's 256 MiB buffer cap). The agent's render plan allows 8192 x supersample 4.
    pub(super) fn check_render_size(width: u32, height: u32, limits: &wgpu::Limits) -> Result<(), WebError> {
        let refuse = |why: String| {
            Err(WebError::new(
                "bad_argument",
                format!("a render of {width}x{height}: {why}"),
            ))
        };
        let largest = limits.max_texture_dimension_2d;
        if width > largest || height > largest {
            return refuse(format!("this device's largest texture is {largest}"));
        }
        let row = u64::from(width)
            .saturating_mul(4)
            .next_multiple_of(u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT));
        let size = row.saturating_mul(u64::from(height));
        if size > limits.max_buffer_size {
            return refuse(format!(
                "a {size} byte readback; this device's largest buffer is {}",
                limits.max_buffer_size
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::check_render_size;

        /// Audit LOW (sprint-corr-web-viewer, "CaptureRig does not check the render size"): the
        /// agent's render plan allows 8192 px x supersample 4 per side, past the texture and the
        /// readback buffer a viewer device has, and wgpu panicked instead of refusing.
        #[test]
        fn corr_a_render_past_the_device_limits_is_refused() {
            let limits = wgpu::Limits {
                max_texture_dimension_2d: 16384,
                ..wgpu::Limits::downlevel_webgl2_defaults()
            };
            assert!(check_render_size(96, 96, &limits).is_ok());
            assert!(check_render_size(8192, 8192, &limits).is_ok(), "exactly 256 MiB");
            for (w, h) in [
                (9000, 8000),
                (65536, 4),
                (4, 65536),
                (32768, 32768),
                (u32::MAX, u32::MAX),
            ] {
                let err = check_render_size(w, h, &limits).expect_err("refused");
                assert_eq!(err.code, "bad_argument", "{w}x{h}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(frame: u64, tilt: [f32; 2], ease: Ease) -> ScriptViewKey {
        ScriptViewKey {
            frame,
            tilt: Some(tilt),
            spin: None,
            pan: None,
            zoom: None,
            ease,
        }
    }

    #[test]
    fn a_frame_hash_is_16_hex_digits_of_fnv1a() {
        assert_eq!(frame_hash(b""), "cbf29ce484222325");
        assert_eq!(frame_hash(b"a"), "af63dc4c8601ec8c");
    }

    #[test]
    fn views_ease_from_the_start_and_hold_after_the_last_key() {
        let start = ViewValues::default();
        let keys = [
            key(10, [10.0, 0.0], Ease::Linear),
            key(20, [10.0, 8.0], Ease::Smooth),
        ];
        assert_eq!(view_at(&keys, start, 0).tilt, [0.0, 0.0]);
        assert_eq!(view_at(&keys, start, 5).tilt, [5.0, 0.0]);
        assert_eq!(view_at(&keys, start, 10).tilt, [10.0, 0.0]);
        assert_eq!(
            view_at(&keys, start, 15).tilt,
            [10.0, 4.0],
            "smoothstep is 1/2 halfway"
        );
        assert_eq!(view_at(&keys, start, 16).tilt[1], 8.0 * 0.648);
        assert_eq!(view_at(&keys, start, 20).tilt, [10.0, 8.0]);
        assert_eq!(view_at(&keys, start, 999).tilt, [10.0, 8.0]);
    }

    #[test]
    fn hold_jumps_on_its_key_and_a_key_at_zero_replaces_the_start() {
        let start = ViewValues::default();
        let keys = [key(0, [3.0, 0.0], Ease::Smooth), key(4, [9.0, 0.0], Ease::Hold)];
        assert_eq!(view_at(&keys, start, 0).tilt, [3.0, 0.0]);
        assert_eq!(view_at(&keys, start, 3).tilt, [3.0, 0.0]);
        assert_eq!(view_at(&keys, start, 4).tilt, [9.0, 0.0]);
    }

    #[test]
    fn what_a_key_leaves_out_stays() {
        let start = ViewValues {
            zoom: 1.5,
            pan: [1.0, 2.0],
            ..ViewValues::default()
        };
        let keys = [key(2, [4.0, 4.0], Ease::Linear)];
        let shown = view_at(&keys, start, 2);
        assert_eq!(shown.zoom, 1.5);
        assert_eq!(shown.pan, [1.0, 2.0]);
    }

    #[test]
    fn ease_spellings_are_the_time_maps() {
        assert_eq!(Ease::parse("smooth"), Some(Ease::Smooth));
        assert_eq!(Ease::parse("linear"), Some(Ease::Linear));
        assert_eq!(Ease::parse("hold"), Some(Ease::Hold));
        assert_eq!(Ease::parse("cubic"), None);
    }

    /// Keys are a timeline. A later entry with an earlier frame used to hide the earlier key: the walk
    /// returned as soon as it saw a key still in the future, so frame 10 of (20 then 10) stayed at the start.
    #[test]
    fn audit_out_of_order_view_key() {
        let start = ViewValues::default();
        let keys = [
            key(20, [0.0, 0.0], Ease::Linear),
            key(10, [10.0, 0.0], Ease::Linear),
        ];
        assert_eq!(
            view_at(&keys, start, 10).tilt,
            [10.0, 0.0],
            "frame 10 is the earlier key, whichever order the list has"
        );
        assert_eq!(view_at(&keys, start, 15).tilt, [5.0, 0.0]);
        assert_eq!(view_at(&keys, start, 20).tilt, [0.0, 0.0]);
    }
}
