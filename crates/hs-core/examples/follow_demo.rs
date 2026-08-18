use hs_core::follow::TrunkFollower;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let bytes = std::fs::read(&a[0]).unwrap();
    let iq: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let rate: f64 = a[1].parse().unwrap();
    let center: f64 = a[2].parse().unwrap();
    let ctl_nominal: f64 = a[3].parse().unwrap();
    let ctl_measured: f64 = a[4].parse().unwrap();

    let mut f = TrunkFollower::new(rate, center, ctl_nominal, ctl_measured);
    println!(
        "following control {:.4} MHz (measured {:.4}, tuner error {:+.0} Hz)\n",
        ctl_nominal / 1e6,
        ctl_measured / 1e6,
        f.correction_hz()
    );

    // Feed in blocks, as a live radio would.
    let block = (rate as usize / 10) * 2;
    let mut done: Vec<hs_core::follow::Call> = Vec::new();
    let mut syncs = 0u32;
    for chunk in iq.chunks(block) {
        let out = f.process(chunk);
        syncs += out.control_syncs;
        for (tg, hz) in &out.started {
            println!("  CALL START  TG {:<7} on {:.4} MHz", tg, *hz as f64 / 1e6);
        }
        done.extend(out.completed);
    }
    println!("\ncontrol channel: {syncs} frame syncs");
    println!("calls followed to completion: {}", done.len());
    for c in &done {
        let m = match c.modulation {
            Some(hs_core::decoder::Modulation::C4fm) => "C4FM",
            Some(hs_core::decoder::Modulation::Cqpsk) => "CQPSK",
            None => "-",
        };
        let patch = if c.patched_with.is_empty() {
            String::new()
        } else {
            format!("  patched with {:?}", c.patched_with)
        };
        println!("  TG {:<7} unit {:<9} {:.4} MHz  {m:5} (C4FM {} / CQPSK {} syncs)  {:.2}s audio{patch}",
            c.talkgroup, c.source_unit, c.freq_hz as f64/1e6,
            c.syncs_c4fm, c.syncs_cqpsk, c.pcm.len() as f64/8000.0);
    }
}
