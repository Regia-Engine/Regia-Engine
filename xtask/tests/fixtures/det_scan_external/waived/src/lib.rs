// Synthetic scan input; deliberately not compiled.
fn step() {
    let _ = 0.0f32.min(-0.0); // det-scan: allow(std_float_fn): synthetic waiver coverage
    let _ = 0.0f32.max(-0.0); // det-scan: allow(std_float_fn): synthetic waiver coverage
    let _ = f32::clamp(f32::NAN, -1.0, 1.0); // det-scan: allow(std_float_fn): synthetic waiver coverage
    let _ = [1.0f32].map(f32::sin); // det-scan: allow(std_float_fn): synthetic waiver coverage
    let _ = libm::sinf(0.5); // det-scan: allow(direct_libm): synthetic waiver coverage
    let _ = x.algebraic_add(y); // det-scan: allow(fast_math): synthetic waiver coverage
    let _ = text.parse::<f64>(); // det-scan: allow(f64_parse): synthetic waiver coverage
    let _: HashMap<u32, u32>; // det-scan: allow(hash_iteration): synthetic waiver coverage
    let _ = items.par_iter(); // det-scan: allow(rayon): synthetic waiver coverage
    let _ = glam::Vec3A::ZERO; // det-scan: allow(simd_type): synthetic waiver coverage
    let _ = glam::Mat3::from_rotation_y(0.5); // det-scan: allow(glam_trig): synthetic waiver coverage
    let _ = Instant::now(); // det-scan: allow(wall_clock): synthetic waiver coverage
    let _ = RandomState::new(); // det-scan: allow(os_random): synthetic waiver coverage
    let _ = std::env::var("MODE"); // det-scan: allow(env_read): synthetic waiver coverage
    let _ = value as *const _ as usize; // det-scan: allow(address_order): synthetic waiver coverage
    std::thread::spawn(step); // det-scan: allow(thread_spawn): synthetic waiver coverage
    let _ = cfg!(target_arch = "x86_64"); // det-scan: allow(arch_cfg): synthetic waiver coverage
    values.sort_by(|a,b| a.partial_cmp(b).unwrap()); // det-scan: allow(float_sort): synthetic waiver coverage
}
