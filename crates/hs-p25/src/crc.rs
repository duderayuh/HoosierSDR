//! CRC-CCITT16 as used by TSBK: poly 0x1021, zero init, final inversion.
//! (Parameters are protocol facts per TIA-102.AABB; reference behavior
//! cross-checked against DSD-FME's ISC implementation.)

pub fn crc16_ccitt(bits: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in bits {
        let fb = (crc >> 15) as u8 ^ (b & 1);
        crc <<= 1;
        if fb == 1 {
            crc ^= 0x1021;
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_and_detects_change() {
        let mut bits = vec![0u8; 80];
        bits[3] = 1;
        bits[42] = 1;
        let c = crc16_ccitt(&bits);
        bits[42] = 0;
        assert_ne!(c, crc16_ccitt(&bits));
    }
}
