//! Stream from an Airspy for a couple of seconds and report the delivered
//! sample rate, level, and drop counters. `cargo run -p hs-source --features
//! airspy --example airspy_smoke -- [freq_hz] [rate_hz]`.
#[cfg(feature = "airspy")]
fn main() {
    use hs_source::airspy::AirspySource;
    use hs_source::SdrSource;
    let mut a = std::env::args().skip(1);
    let freq: f64 = a
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(851_000_000.0);
    let rate: f64 = a.next().and_then(|s| s.parse().ok()).unwrap_or(2_500_000.0);
    println!("boards: {:x?}", AirspySource::list());
    let mut src = AirspySource::open(None, freq, rate, None).expect("open");
    let mut buf = vec![0f32; 65536 * 2];
    let mut pairs = 0u64;
    let mut sumsq = 0f64;
    let mut peak = 0f32;
    let t0 = std::time::Instant::now();
    // Discard the first half second (tuner settling), then time two seconds.
    let mut started = None;
    while started
        .map(|s: std::time::Instant| s.elapsed().as_secs_f64() < 2.0)
        .unwrap_or(true)
    {
        let n = src.read(&mut buf).expect("read");
        if started.is_none() {
            if t0.elapsed().as_secs_f64() > 0.5 {
                started = Some(std::time::Instant::now());
            }
            continue;
        }
        pairs += (n / 2) as u64;
        for &v in &buf[..n] {
            sumsq += (v * v) as f64;
            peak = peak.max(v.abs());
        }
    }
    let secs = started.unwrap().elapsed().as_secs_f64();
    println!(
        "delivered {:.3} MSPS over {secs:.2}s (asked {:.3}); rms {:.5} peak {:.4}; queue drops {} device drops {}",
        pairs as f64 / secs / 1e6,
        rate / 1e6,
        (sumsq / (pairs as f64 * 2.0)).sqrt(),
        peak,
        src.queue_drops(),
        src.device_drops()
    );
}
#[cfg(not(feature = "airspy"))]
fn main() {
    eprintln!("build with --features airspy");
}
