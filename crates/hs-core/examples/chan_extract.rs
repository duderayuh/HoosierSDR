//! Write one channelizer output to a .cf32 file, so what the follower actually
//! receives can be decoded and compared against an independent extraction.
//!
//! Usage: chan_extract <in.cf32> <sample_rate> <offset_hz> <out.cf32>
use hs_dsp::channelizer::Channelizer;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (path, rate, offset, out) = (
        &a[1],
        a[2].parse::<f64>().unwrap(),
        a[3].parse::<f64>().unwrap(),
        &a[4],
    );
    let bytes = std::fs::read(path).unwrap();
    let iq: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let mut ch = Channelizer::new(rate, &[offset]);
    eprintln!(
        "requested {offset} Hz, tuned {:?}, out rate {}",
        ch.actual_offsets_hz(),
        ch.output_rate()
    );
    let o = ch.process(&iq).remove(0);
    let mut buf = Vec::with_capacity(o.len() * 4);
    for v in &o {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(out, buf).unwrap();
    eprintln!("wrote {} samples", o.len() / 2);
}
