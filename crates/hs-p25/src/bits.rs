//! Dibit/bit packing helpers. Over-the-air order is MSB-first.

/// Expand dibits to individual bits (MSB of each dibit first).
pub fn dibits_to_bits(dibits: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(dibits.len() * 2);
    for &d in dibits {
        out.push((d >> 1) & 1);
        out.push(d & 1);
    }
    out
}

/// Read `n` bits MSB-first starting at `pos` into a u64.
pub fn read_bits(bits: &[u8], pos: usize, n: usize) -> u64 {
    assert!(n <= 64 && pos + n <= bits.len());
    let mut v = 0u64;
    for &b in &bits[pos..pos + n] {
        v = (v << 1) | b as u64;
    }
    v
}

/// Write `n` bits of `v` MSB-first into `bits` starting at `pos`.
pub fn write_bits(bits: &mut [u8], pos: usize, n: usize, v: u64) {
    for i in 0..n {
        bits[pos + i] = ((v >> (n - 1 - i)) & 1) as u8;
    }
}

/// Pack bits (MSB-first) into dibits.
pub fn bits_to_dibits(bits: &[u8]) -> Vec<u8> {
    assert!(bits.len().is_multiple_of(2));
    bits.chunks(2).map(|c| (c[0] << 1) | c[1]).collect()
}
