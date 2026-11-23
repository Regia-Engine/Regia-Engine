// SPDX-License-Identifier: MPL-2.0
//! The renderer behind the player on a real device (the lavapipe driver on the farm), with an offscreen
//! target: a real package, real swaps. Skips when the machine offers no adapter.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use regia_viewer::BakedEnv;
use regia_viewer_pack::cook::{self, CookOptions};
use regia_viewer_pack::standin;
use regia_web::player::{Player, PlayerEvent};
use regia_web::product::PackageProduct;
use regia_web::scene::{FrameTarget, ViewerScene, player_config};
use regia_web::seams::{DrawOutcome, ProductModel, ViewModel};
use regia_web::view::ProductView;
use serde_json::{Value, json};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "regia-web-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

fn cook_marquise(root: &Path) -> PathBuf {
    cook_named(root, "marquise")
}

fn cook_named(root: &Path, wanted: &str) -> PathBuf {
    let set = standin::write(&root.join("in")).expect("stand-ins");
    for (recipe, graph) in set.recipes.iter().zip(&set.option_graphs) {
        let name = recipe
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("x")
            .to_string();
        if name.contains(wanted) {
            let out = root.join("out");
            let options = CookOptions {
                option_graph: Some(graph.clone()),
                materials: Some(set.library.clone()),
                ..CookOptions::default()
            };
            cook::pack(&set.kit_dir, recipe, &out, &options).expect("cooks");
            return out;
        }
    }
    panic!("no {wanted} stand-in");
}

/// An offscreen texture, read back after each frame.
struct Offscreen {
    device: wgpu::Device,
    queue: wgpu::Queue,
    texture: wgpu::Texture,
    last: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
}

fn texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
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

impl Offscreen {
    fn read(&self) -> Vec<u8> {
        let (w, h) = (self.texture.width(), self.texture.height());
        let stride = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(stride * h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            self.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("maps"));
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        let out = slice.get_mapped_range().expect("mapped").to_vec();
        buffer.unmap();
        out
    }
}

impl FrameTarget for Offscreen {
    fn resize(&mut self, width: u32, height: u32) {
        self.texture = texture(&self.device, width.max(1), height.max(1));
    }

    fn with_frame(
        &mut self,
        draw: &mut dyn FnMut(&wgpu::Texture) -> Result<(), regia_web::WebError>,
    ) -> Result<DrawOutcome, regia_web::WebError> {
        draw(&self.texture)?;
        *self.last.borrow_mut() = self.read();
        Ok(DrawOutcome::Drawn)
    }
}

struct Rig {
    player: Player,
    last: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    now: f64,
}

impl Rig {
    fn new(dir: &Path) -> Option<Rig> {
        Rig::with(dir, 128, player_config, &BakedEnv::studio_default())
    }

    fn with(
        dir: &Path,
        size: u32,
        config: fn(wgpu::TextureFormat, u32) -> regia_viewer::ViewerConfig,
        env: &BakedEnv,
    ) -> Option<Rig> {
        let (adapter, device, queue) = match regia_viewer::gpu::headless_device() {
            Ok(d) => d,
            Err(regia_viewer::ViewerError::NoAdapter(e)) => {
                assert!(
                    std::env::var_os("REGIA_VIEWER_REQUIRE_ADAPTER").is_none(),
                    "REGIA_VIEWER_REQUIRE_ADAPTER is set and there is no adapter: {e}"
                );
                println!("skipped: no adapter: {e}");
                return None;
            }
            Err(e) => panic!("the adapter refused a device: {e}"),
        };
        let sample_count = regia_viewer::gpu::sample_count_for(&adapter, FORMAT);
        let last = std::rc::Rc::default();
        let target = Offscreen {
            device: device.clone(),
            queue: queue.clone(),
            texture: texture(&device, size, size),
            last: std::rc::Rc::clone(&last),
        };
        let config = config(FORMAT, sample_count);
        let scene = ViewerScene::new(device, queue, config, env, target).expect("a scene");
        let mut player = Player::new(Box::new(|| 0.0));
        player.resize([size as f32, size as f32], [size, size]);
        player.attach_scene(Box::new(scene));
        let mut rig = Rig {
            player,
            last,
            now: 1000.0,
        };
        let mut product =
            PackageProduct::open(regia_viewer_pack::read_dir(dir).expect("package"), "studio-test")
                .expect("opens");
        let view = ProductView::for_product(&mut product).expect("view");
        rig.player
            .install(Box::new(product), Box::new(view))
            .expect("installs");
        rig.player.draw_first();
        rig.settle();
        Some(rig)
    }

    fn settle(&mut self) {
        let mut frames = 0;
        while self.player.lazy().pending() {
            self.now += 16.0;
            self.player.frame(self.now);
            frames += 1;
            assert!(frames < 10_000, "the loop never stopped");
        }
    }

    fn stat(&self, name: &str) -> Value {
        self.player.stats("test")[name].clone()
    }

    fn pick(&mut self, group: &str, value: &str) -> Vec<u8> {
        self.player
            .set_pick(group, &json!(value), false)
            .unwrap_or_else(|e| panic!("{group}={value}: {e}"));
        self.settle();
        self.last.borrow().clone()
    }
}

fn differing(a: &[u8], b: &[u8]) -> usize {
    a.chunks(4).zip(b.chunks(4)).filter(|(x, y)| x != y).count()
}

#[test]
fn swaps_show_at_once_and_build_no_pipeline() {
    let root = temp_dir("scene");
    let dir = cook_marquise(&root);
    let Some(mut rig) = Rig::new(&dir) else {
        return;
    };
    let events = rig.player.drain_events();
    assert!(
        events.iter().any(|e| matches!(e, PlayerEvent::Ready { .. })),
        "{events:?}"
    );
    let pipelines = rig.stat("pipelines");
    assert!(pipelines.as_u64().expect("count") > 0);
    let first = rig.last.borrow().clone();
    assert!(!first.is_empty(), "a frame was read back");

    let metal = rig.pick("Material", "14K Rose Gold");
    assert!(differing(&first, &metal) > 20, "a metal pick changes the picture");
    let gem = rig.pick("Gem", "Blue Sapphire (synthetic)");
    assert!(differing(&metal, &gem) > 5, "a gem pick changes the picture");
    let bigger = rig.pick("Gem Size", "4x2mm");
    assert!(differing(&gem, &bigger) > 5, "a size pick changes the geometry");
    let threaded = rig.pick("Threading", "Threaded");
    assert!(
        differing(&bigger, &threaded) > 5,
        "a part swap changes the picture"
    );
    assert_eq!(rig.stat("problems"), json!([]));
    rig.pick("Threading", "Threadless");
    rig.pick("Gem Size", "3x1.5mm");
    rig.pick("Gem", "Clear CZ");
    let same = rig.pick("Material", "14K Yellow Gold");
    assert_eq!(
        differing(&first, &same),
        0,
        "the first picks show the first picture again"
    );

    assert_eq!(
        rig.stat("pipelines"),
        pipelines,
        "0 pipeline creations after open"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// Correctness audit (corr-web-viewer): opening a product the renderer refuses (here a package mesh with no
/// triangles, which `mesh.rvm` allows and the renderer does not) used to unload the product on screen first, so
/// the player kept the old product but the canvas went blank on the next frame.
#[test]
fn corr_a_refused_open_leaves_the_shown_product_on_screen() {
    let root = temp_dir("refused-open");
    let dir = cook_marquise(&root);
    let Some(mut rig) = Rig::new(&dir) else {
        return;
    };
    let shown = rig.last.borrow().clone();
    assert!(!shown.is_empty(), "a frame was read back");

    let mut package = regia_viewer_pack::read_dir(&dir).expect("package");
    package.meshes.push(regia_viewer_pack::MeshData {
        name: "part:empty".into(),
        material_slot: "body".into(),
        ..regia_viewer_pack::MeshData::default()
    });
    let mut product = PackageProduct::open(package, "studio-test").expect("opens");
    let view = ProductView::for_product(&mut product).expect("view");
    let err = rig
        .player
        .install(Box::new(product), Box::new(view))
        .expect_err("the renderer refuses an empty mesh");
    assert_eq!(err.code, "render_failed", "{err}");

    // The next frame (any input or resize) still shows the product that was on screen.
    rig.player.invalidate();
    rig.settle();
    let after = rig.last.borrow().clone();
    assert_eq!(differing(&shown, &after), 0, "the shown product stays on screen");
    std::fs::remove_dir_all(&root).ok();
}

/// Correctness audit (corr-web-viewer): a pick the configurator accepts but the package cannot draw (an offered
/// gem size with no mesh) was refused with `package_invalid` while the pick stayed applied, against the contract
/// that a refusal changes nothing. No GPU needed.
#[test]
fn corr_a_pick_the_package_cannot_draw_changes_nothing() {
    let root = temp_dir("undrawable-pick");
    let dir = cook_marquise(&root);
    let mut package = regia_viewer_pack::read_dir(&dir).expect("package");
    // Keep only the first size of every gem slot: the option record still offers the others.
    let mut removed = 0;
    for slot in package.options["gem_slots"].as_array_mut().expect("gem slots") {
        let sizes = slot["sizes"].as_array_mut().expect("sizes");
        removed += sizes.len().saturating_sub(1);
        sizes.truncate(1);
    }
    assert!(removed > 0, "the stand-in offers more than one size");
    let mut product = PackageProduct::open(package, "studio-test").expect("opens");
    let view = regia_web::seams::ViewValues::default();
    let before = product.build(&view).expect("build");
    let plan_before = product.plan().expect("plan");
    let err = product
        .set_pick("Gem Size", &json!("4x2mm"), false)
        .expect_err("no mesh for the size");
    assert_eq!(err.code, "package_invalid", "{err}");
    assert_eq!(
        product.build(&view).expect("build"),
        before,
        "the refused pick changed the build"
    );
    assert_eq!(product.plan().expect("plan still draws"), plan_before);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn idle_draws_nothing_on_a_real_renderer() {
    let root = temp_dir("idle");
    let dir = cook_marquise(&root);
    let Some(mut rig) = Rig::new(&dir) else {
        return;
    };
    let draws = rig.stat("draws");
    for _ in 0..50 {
        rig.settle();
    }
    assert_eq!(rig.stat("draws"), draws);
    assert_eq!(rig.stat("pending"), json!(false));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_new_product_replaces_the_old_meshes() {
    let root = temp_dir("second");
    let dir = cook_marquise(&root);
    let Some(mut rig) = Rig::new(&dir) else {
        return;
    };
    rig.pick("Material", "14K Rose Gold");
    let mut product =
        PackageProduct::open(regia_viewer_pack::read_dir(&dir).expect("package"), "s").expect("opens");
    assert!(!product.plan().expect("plan").draws.is_empty());
    let view = ProductView::for_product(&mut product).expect("view");
    assert!(view.pose().near > 0.0);
    rig.player
        .install(Box::new(product), Box::new(view))
        .expect("installs");
    rig.player.draw_first();
    rig.settle();
    assert_eq!(
        rig.player.build().expect("open")["picks"]["Material"],
        "14K Yellow Gold"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_scene_refuses_nothing_on_resize() {
    let root = temp_dir("resize");
    let dir = cook_marquise(&root);
    let Some(mut rig) = Rig::new(&dir) else {
        return;
    };
    rig.player.resize([64.0, 200.0], [64, 200]);
    rig.settle();
    let pixels = rig.last.borrow().len();
    assert!(
        pixels >= 64 * 200 * 4,
        "the target follows the drawing size: {pixels}"
    );
    assert!(
        rig.player
            .drain_events()
            .iter()
            .all(|e| !matches!(e, PlayerEvent::Error(_)))
    );
    std::fs::remove_dir_all(&root).ok();
}

/// The renderer's own starting-point shaders, for comparing against what the players draw.
fn default_hooks(format: wgpu::TextureFormat, samples: u32) -> regia_viewer::ViewerConfig {
    let mut config = regia_viewer::ViewerConfig::new(format);
    config.sample_count = samples;
    config
}

#[test]
fn the_player_draws_the_looks_and_stats_say_so() {
    let root = temp_dir("looks");
    let dir = cook_marquise(&root);
    let Some(rig) = Rig::with(&dir, 256, player_config, &BakedEnv::studio_default()) else {
        return;
    };
    assert_eq!(rig.stat("looks"), json!(true));
    assert_eq!(rig.stat("problems"), json!([]));
    let looks = rig.last.borrow().clone();
    assert!(!looks.is_empty());

    let plain = Rig::with(&dir, 256, default_hooks, &BakedEnv::studio_default()).expect("the same machine");
    assert_eq!(
        plain.stat("looks"),
        json!(false),
        "the stat tells the shaders apart"
    );
    let differs = differing(&looks, &plain.last.borrow());
    assert!(
        differs > 200,
        "the looks shaders change the picture: {differs} pixels"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_size_pick_reaches_the_stone_shader() {
    let root = temp_dir("size");
    let dir = cook_marquise(&root);
    let mut product =
        PackageProduct::open(regia_viewer_pack::read_dir(&dir).expect("package"), "s").expect("opens");
    let before = product.plan().expect("plan").materials["gem:s1"].clone();
    // The stand-in marquise runs its 3 mm length along y and its 1.5 mm width along x.
    assert!(
        (before.stone.half_length_mm - 1.5).abs() < 0.02,
        "{:?}",
        before.stone
    );
    assert!(
        (before.stone.half_width_mm - 0.75).abs() < 0.02,
        "{:?}",
        before.stone
    );
    assert!(before.stone.depth_mm > 0.3, "{:?}", before.stone);
    product
        .set_pick("Gem Size", &json!("4x2mm"), false)
        .expect("a sold size");
    let after = product.plan().expect("plan").materials["gem:s1"].clone();
    assert!(
        (after.stone.half_length_mm - 2.0).abs() < 0.02,
        "{:?}",
        after.stone
    );
    assert_ne!(
        before.key, after.key,
        "the scene only rewrites a material whose key changed"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// Writes what a player draws to `$REGIA_WEB_EVIDENCE` as PNGs (the owner's look check). Does nothing without it.
#[test]
fn evidence_stills_through_the_player() {
    let Some(out) = std::env::var_os("REGIA_WEB_EVIDENCE").map(PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(&out).expect("evidence dir");
    let root = temp_dir("evidence");
    let env = BakedEnv::studio_default();
    let looks = [
        (
            "marquise",
            &[("Material", "14K Yellow Gold"), ("Gem", "Clear CZ")][..],
        ),
        (
            "marquise",
            &[
                ("Material", "14K Rose Gold"),
                ("Gem", "Blue Sapphire (synthetic)"),
            ][..],
        ),
        (
            "marquise",
            &[("Material", "Titanium"), ("Gem", "Green Spinel (synthetic)")][..],
        ),
        (
            "round",
            &[("Material", "14K White Gold"), ("Gem", "Clear CZ")][..],
        ),
    ];
    for (i, (recipe, picks)) in looks.iter().enumerate() {
        let dir = cook_named(&root.join(format!("{i}")), recipe);
        let Some(mut rig) = Rig::with(&dir, 1024, player_config, &env) else {
            return;
        };
        for (group, value) in *picks {
            rig.pick(group, value);
        }
        let still = regia_viewer::Still {
            width: 1024,
            height: 1024,
            rgba: rig.last.borrow().clone(),
        };
        let name = out.join(format!("player-{i}-{recipe}.png"));
        std::fs::write(&name, still.encode_png().expect("png")).expect("writes");
        println!("{}", name.display());
    }
    std::fs::remove_dir_all(&root).ok();
}

// Audit fix (sprint-fix-audit-jw, finding 3): anodize lookups belong to the product that made them. A
// product opened after another one gets its own tables even under the same material id, and a page that
// opens product after product never runs out of lookup rows.

/// A product of one flat square drawn with one anodized metal.
struct LutProduct {
    serial: u64,
    entries: Vec<[f32; 3]>,
    meshes: Vec<regia_viewer_pack::MeshData>,
}

impl LutProduct {
    fn new(serial: u64, entries: Vec<[f32; 3]>) -> Self {
        let mesh = regia_viewer_pack::MeshData {
            name: "square".into(),
            material_slot: "body".into(),
            positions: vec![
                [-6.0, -6.0, 0.0],
                [6.0, -6.0, 0.0],
                [6.0, 6.0, 0.0],
                [-6.0, 6.0, 0.0],
            ],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: None,
            indices: vec![0, 1, 2, 0, 2, 3],
        };
        LutProduct {
            serial,
            entries,
            meshes: vec![mesh],
        }
    }
}

impl ProductModel for LutProduct {
    fn serial(&self) -> u64 {
        self.serial
    }

    fn plan(&self) -> Result<regia_web::seams::ScenePlan, regia_web::WebError> {
        let look = regia_looks::LookMaterial::Metal(regia_looks::MetalLook {
            f0: [0.6, 0.6, 0.6],
            f82_tint: [1.0, 1.0, 1.0],
            roughness: 0.3,
            anodize_lut: Some(self.entries.clone()),
            grain_mm: 0.0,
        });
        let planned = regia_web::seams::PlannedMaterial {
            // The same material id and finish in every product: only the table differs.
            key: "ti-anod|polished|".into(),
            look,
            stone: regia_looks::StoneSize::default(),
        };
        Ok(regia_web::seams::ScenePlan {
            draws: vec![regia_web::seams::PlannedDraw {
                mesh: "square".into(),
                zone: "body".into(),
                transforms: vec![glam::Affine3A::IDENTITY],
            }],
            materials: [("body".to_string(), planned)].into_iter().collect(),
            problems: Vec::new(),
        })
    }

    fn meshes(&self) -> &[regia_viewer_pack::MeshData] {
        &self.meshes
    }

    fn options(&self) -> Value {
        json!({})
    }

    fn set_pick(
        &mut self,
        _group: &str,
        _value: &Value,
        _adjust: bool,
    ) -> Result<regia_web::seams::PickApplied, regia_web::WebError> {
        Ok(regia_web::seams::PickApplied::default())
    }

    fn build(&self, _view: &regia_web::seams::ViewValues) -> Result<Value, regia_web::WebError> {
        Ok(json!({}))
    }

    fn incomplete(&self) -> bool {
        false
    }
}

/// The pixels an offscreen target read back last.
type LastFrame = std::rc::Rc<std::cell::RefCell<Vec<u8>>>;

fn lut_scene() -> Option<(ViewerScene<Offscreen>, LastFrame)> {
    let (adapter, device, queue) = match regia_viewer::gpu::headless_device() {
        Ok(d) => d,
        Err(regia_viewer::ViewerError::NoAdapter(e)) => {
            assert!(
                std::env::var_os("REGIA_VIEWER_REQUIRE_ADAPTER").is_none(),
                "REGIA_VIEWER_REQUIRE_ADAPTER is set and there is no adapter: {e}"
            );
            println!("skipped: no adapter: {e}");
            return None;
        }
        Err(e) => panic!("the adapter refused a device: {e}"),
    };
    let sample_count = regia_viewer::gpu::sample_count_for(&adapter, FORMAT);
    let last = std::rc::Rc::default();
    let target = Offscreen {
        device: device.clone(),
        queue: queue.clone(),
        texture: texture(&device, 64, 64),
        last: std::rc::Rc::clone(&last),
    };
    let config = player_config(FORMAT, sample_count);
    let scene =
        ViewerScene::new(device, queue, config, &BakedEnv::studio_default(), target).expect("a scene");
    Some((scene, last))
}

fn square_pose() -> regia_web::seams::CameraPose {
    regia_web::seams::CameraPose {
        eye: [0.0, -8.0, 30.0],
        look_at: [0.0, 0.0, 0.0],
        up: [0.0, 1.0, 0.0],
        fov_short_deg: 30.0,
        near: 1.0,
        far: 100.0,
    }
}

fn shown(scene: &mut ViewerScene<Offscreen>, last: &LastFrame, product: &LutProduct) -> Vec<u8> {
    use regia_web::seams::SceneBackend;
    scene.sync_product(product, true).expect("syncs");
    assert_eq!(scene.draw(&square_pose()).expect("draws"), DrawOutcome::Drawn);
    last.borrow().clone()
}

const RED: [[f32; 3]; 2] = [[0.9, 0.05, 0.05], [0.8, 0.1, 0.1]];
const BLUE: [[f32; 3]; 2] = [[0.05, 0.05, 0.9], [0.1, 0.1, 0.8]];

#[test]
fn a_new_product_gets_its_own_anodize_table_under_the_same_material_id() {
    let Some((mut scene, last)) = lut_scene() else {
        return;
    };
    let red = shown(&mut scene, &last, &LutProduct::new(1, RED.to_vec()));
    let after_red = shown(&mut scene, &last, &LutProduct::new(2, BLUE.to_vec()));
    assert_eq!(
        scene.renderer().lut_rows(),
        1,
        "the first product's row was freed"
    );
    let Some((mut fresh, fresh_last)) = lut_scene() else {
        return;
    };
    let blue = shown(&mut fresh, &fresh_last, &LutProduct::new(7, BLUE.to_vec()));
    assert!(differing(&red, &blue) > 50, "the table shows in the picture");
    assert_eq!(
        differing(&after_red, &blue),
        0,
        "the second product is drawn with its own table, not the first one's"
    );
}

#[test]
fn opening_product_after_product_never_runs_out_of_lookup_rows() {
    let Some((mut scene, _last)) = lut_scene() else {
        return;
    };
    use regia_web::seams::SceneBackend;
    for serial in 0..u64::from(regia_viewer::LUT_ROWS) + 8 {
        let k = serial as f32 / 100.0;
        let product = LutProduct::new(serial, vec![[k, 0.2, 0.3], [0.3, k, 0.2]]);
        scene
            .sync_product(&product, true)
            .unwrap_or_else(|e| panic!("product {serial}: {e}"));
        assert_eq!(scene.renderer().lut_rows(), 1, "product {serial}");
    }
}
