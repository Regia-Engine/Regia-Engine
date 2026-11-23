// Synthetic scan input; deliberately not compiled.
fn step() {
    let _ = 0.0f32.min(-0.0);
    let _ = 0.0f32.max(-0.0);
    let _ = f32::clamp(f32::NAN, -1.0, 1.0);
    let _ = [1.0f32].map(f32::sin);
    let _ = libm::sinf(0.5);
    let _ = x.algebraic_add(y);
    let _ = text.parse::<f64>();
    let _: HashMap<u32, u32>;
    let _ = items.par_iter();
    let _ = glam::Vec3A::ZERO;
    let _ = glam::Mat3::from_rotation_y(0.5);
    let _ = Instant::now();
    let _ = RandomState::new();
    let _ = std::env::var("MODE");
    let _ = value as *const _ as usize;
    std::thread::spawn(step);
    let _ = cfg!(target_arch = "x86_64");
    values.sort_by(|a,b| a.partial_cmp(b).unwrap());
}
