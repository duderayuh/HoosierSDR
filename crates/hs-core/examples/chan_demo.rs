use hs_core::decoder::{ChannelDecoder, EqMode, Modulation};
use hs_dsp::channelizer::Channelizer;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let bytes = std::fs::read(&a[0]).unwrap();
    let iq: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let rate: f64 = a[1].parse().unwrap();
    let center: f64 = a[2].parse().unwrap();
    let targets: Vec<f64> = a[3..].iter().map(|s| s.parse().unwrap()).collect();
    let offsets: Vec<f64> = targets.iter().map(|t| t - center).collect();

    let t0 = std::time::Instant::now();
    let mut ch = Channelizer::new(rate, &offsets);
    let outs = ch.process(&iq);
    let chan_time = t0.elapsed().as_secs_f64();

    let mut decoders: Vec<ChannelDecoder> = targets
        .iter()
        .map(|_| {
            ChannelDecoder::with_offset(ch.output_rate(), Modulation::Cqpsk, EqMode::Enabled, 0.0)
        })
        .collect();

    println!(
        "channelized {} channels in {:.1}s ({} samples each)\n",
        targets.len(),
        chan_time,
        outs[0].len() / 2
    );
    for (i, (t, o)) in targets.iter().zip(outs.iter()).enumerate() {
        let out = decoders[i].process(o);
        let d = decoders[i].diagnostics();
        let nac = d
            .nids
            .last()
            .map(|n| format!("0x{:03X}", n.nac))
            .unwrap_or("-".into());
        println!(
            "  {:.4} MHz  syncs={:<4} NAC {:<6} grants={:<4} voice_frames={}",
            t / 1e6,
            out.syncs,
            nac,
            d.grants.len(),
            d.voice_frames
        );
    }
    println!("\ntotal {:.1}s", t0.elapsed().as_secs_f64());
}
