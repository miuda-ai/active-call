pub fn generate_hann_window(size: usize, periodic: bool) -> Vec<f32> {
    let mut window = Vec::with_capacity(size);
    let denom = if periodic {
        size as f32
    } else {
        (size - 1) as f32
    };

    for i in 0..size {
        let val = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / denom).cos());
        window.push(val);
    }
    window
}
