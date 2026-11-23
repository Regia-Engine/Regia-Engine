//! Pulls the full skeleton dependency set into the engine library.
//!
//! The engine crates are still stubs at M2 and do not use rapier, kira, wgpu... yet, so a dylib of
//! today's `regia` would be unrealistically small. Each arm below calls into one heavy dependency
//! the way the finished engine will, so the dylib links (and exports) the code the real one will.
//! `exercise` is only ever called with an unknown id; the arms exist to be compiled and linked.

/// Runs one dependency's smoke path. Unknown ids do nothing and return 0.
#[allow(clippy::too_many_lines)]
pub fn exercise(id: u32) -> usize {
    match id {
        1 => physics_step(),
        2 => {
            let settings = kira::AudioManagerSettings::<kira::DefaultBackend>::default();
            kira::AudioManager::<kira::DefaultBackend>::new(settings).map_or(0, |_| 1)
        }
        3 => {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapters = regia::core::tasks::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
            adapters.len()
        }
        4 => winit::event_loop::EventLoop::new().map_or(0, |_| 1),
        5 => {
            let mut tree: taffy::TaffyTree<()> = taffy::TaffyTree::new();
            let leaf = tree.new_leaf(taffy::Style::default()).ok();
            let Some(leaf) = leaf else { return 0 };
            let _ = tree.compute_layout(
                leaf,
                taffy::Size {
                    width: taffy::AvailableSpace::MaxContent,
                    height: taffy::AvailableSpace::MaxContent,
                },
            );
            tree.total_node_count()
        }
        6 => {
            let mut fonts = cosmic_text::FontSystem::new();
            let metrics = cosmic_text::Metrics::new(14.0, 20.0);
            let mut buffer = cosmic_text::Buffer::new(&mut fonts, metrics);
            buffer.set_text(
                "hot reload",
                &cosmic_text::Attrs::new(),
                cosmic_text::Shaping::Advanced,
                None,
            );
            buffer.shape_until_scroll(&mut fonts, false);
            buffer.layout_runs().count()
        }
        7 => {
            let lang: unic_langid::LanguageIdentifier = "en-US".parse().unwrap_or_default();
            let mut bundle = fluent_bundle::FluentBundle::new(vec![lang]);
            if let Ok(res) = fluent_bundle::FluentResource::try_new("hello = Hello { $name }".to_string()) {
                let _ = bundle.add_resource(res);
            }
            let mut errors = Vec::new();
            bundle
                .get_message("hello")
                .and_then(|m| m.value())
                .map_or(0, |p| bundle.format_pattern(p, None, &mut errors).len())
        }
        8 => {
            let mut composer = naga_oil::compose::Composer::default();
            composer
                .make_naga_module(naga_oil::compose::NagaModuleDescriptor {
                    source: "@fragment fn main() -> @location(0) vec4<f32> { return vec4(1.0); }",
                    file_path: "x.wgsl",
                    ..Default::default()
                })
                .map_or(0, |m| m.functions.len() + m.entry_points.len())
        }
        9 => gilrs_count(),
        10 => ktx2::Reader::new(&[0u8; 80][..]).map_or(0, |r| r.header().level_count as usize),
        11 => {
            let decoder = png::Decoder::new(std::io::Cursor::new(&[0u8; 8][..]));
            decoder.read_info().map_or(0, |r| r.info().width as usize)
        }
        12 => {
            let msg = tungstenite::Message::text("hello");
            let (mut a, _b) = match std::net::TcpListener::bind("127.0.0.1:0") {
                Ok(l) => (l, ()),
                Err(_) => return 0,
            };
            let _ = &mut a;
            match a.accept() {
                Ok((stream, _)) => {
                    tungstenite::accept(stream).map_or(0, |mut ws| usize::from(ws.send(msg).is_ok()))
                }
                Err(_) => 0,
            }
        }
        13 => {
            let watcher = notify::recommended_watcher(|_res: notify::Result<notify::Event>| {});
            usize::from(watcher.is_ok())
        }
        14 => {
            let data = vec![7u8; 4096];
            let packed = lz4_flex::compress_prepend_size(&data);
            let bytes = postcard::to_allocvec(&packed).unwrap_or_default();
            let hash = blake3::hash(&bytes);
            let id = uuid::Uuid::new_v4();
            let text = ron::to_string(&(id, hash.to_hex().to_string())).unwrap_or_default();
            lz4_flex::decompress_size_prepended(&packed).map_or(0, |d| d.len()) + text.len()
        }
        15 => {
            let file = std::fs::File::open("/nonexistent");
            // SAFETY: `/nonexistent` never opens, so nothing is mapped; this only links memmap2's code.
            #[allow(unsafe_code)]
            file.ok()
                .and_then(|f| unsafe { memmap2::Mmap::map(&f) }.ok())
                .map_or(0, |m| m.len())
        }
        _ => 0,
    }
}

fn physics_step() -> usize {
    use rapier3d::prelude::*;
    let mut bodies = RigidBodySet::new();
    let mut colliders = ColliderSet::new();
    let ground = ColliderBuilder::cuboid(100.0, 0.1, 100.0).build();
    colliders.insert(ground);
    let body = RigidBodyBuilder::dynamic()
        .translation(Vector::new(0.0, 10.0, 0.0))
        .build();
    let handle = bodies.insert(body);
    let ball = ColliderBuilder::ball(0.5).restitution(0.7).build();
    colliders.insert_with_parent(ball, handle, &mut bodies);
    let gravity = Vector::new(0.0, -9.81, 0.0);
    let params = IntegrationParameters::default();
    let mut pipeline = PhysicsPipeline::new();
    let mut islands = IslandManager::new();
    let mut broad = DefaultBroadPhase::new();
    let mut narrow = NarrowPhase::new();
    let mut impulse = ImpulseJointSet::new();
    let mut multibody = MultibodyJointSet::new();
    let mut ccd = CCDSolver::new();
    let mut soft = SoftBodySet::new();
    for _ in 0..10 {
        pipeline.step(
            gravity,
            &params,
            &mut islands,
            &mut broad,
            &mut narrow,
            &mut bodies,
            &mut colliders,
            &mut impulse,
            &mut multibody,
            &mut soft,
            &mut ccd,
            &(),
            &(),
        );
    }
    bodies.len()
}

#[cfg(not(target_os = "android"))]
fn gilrs_count() -> usize {
    gilrs::Gilrs::new().map_or(0, |g| g.gamepads().count())
}

#[cfg(target_os = "android")]
fn gilrs_count() -> usize {
    0
}
