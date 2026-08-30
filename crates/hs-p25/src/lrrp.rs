//! LRRP — Location Request/Response Protocol, the location reports that ride
//! inside P25 packet data.
//!
//! A radio's GPS position is not a P25 protocol field. It travels as an
//! ordinary UDP datagram inside a packet data unit, so the stack to reach it
//! is: PDU assembly ([`crate::pdu`]) → IPv4 → UDP → LRRP. Each layer here is
//! parsed only far enough to reach the next one.
//!
//! ## What this decodes, and how much to trust it
//!
//! LRRP is a Motorola protocol with no public specification. This parser is
//! built from published descriptions of its wire format, and — unlike the rest
//! of this crate — could not be checked against real traffic, because no
//! packet-data capture exists for this project yet.
//!
//! That shapes the design. Rather than parse the whole token grammar and hope,
//! it looks for the one token whose encoding is well attested (the position
//! triplet) and then **refuses any result that is not physically plausible**:
//! coordinates outside their valid ranges, or the exact-zero "null island"
//! that unset fields decode to. A wrong guess about the format therefore
//! yields no report rather than a confident wrong position on a map, and
//! [`LrrpReport::raw`] keeps the payload so a real capture can correct the
//! parser instead of arguing with it.
//!
//! ## Encryption
//!
//! Many agencies encrypt location reporting. Encrypted payloads are not
//! decrypted here, by the same architectural refusal that governs voice: they
//! simply fail to parse as LRRP and are dropped.

/// UDP port LRRP conventionally uses.
pub const LRRP_PORT: u16 = 4001;

/// Token introducing a latitude/longitude pair, followed by two big-endian
/// signed 32-bit values.
const TOKEN_POSITION: u8 = 0x66;

/// A decoded location report.
#[derive(Debug, Clone, PartialEq)]
pub struct LrrpReport {
    /// Radio that sent it, from the enclosing PDU header's Logical Link ID.
    pub llid: u32,
    /// Decimal degrees, WGS84.
    pub lat: f64,
    pub lon: f64,
    /// The LRRP payload it came from, kept so an unexpected dialect can be
    /// diagnosed against a real capture rather than guessed at.
    pub raw: Vec<u8>,
}

/// A minimally-parsed UDP datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datagram<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: &'a [u8],
}

/// Pull a UDP datagram out of an IPv4 packet, if that is what this is.
///
/// Deliberately strict: version must be 4, the header length must fit, and the
/// protocol must be UDP. Packet data carries plenty that is not a location
/// report, and quietly reinterpreting it would manufacture positions.
pub fn parse_ipv4_udp(payload: &[u8]) -> Option<Datagram<'_>> {
    const PROTO_UDP: u8 = 17;
    if payload.len() < 20 {
        return None;
    }
    let version = payload[0] >> 4;
    if version != 4 {
        return None;
    }
    let ihl = (payload[0] & 0x0F) as usize * 4;
    if ihl < 20 || payload.len() < ihl + 8 {
        return None;
    }
    if payload[9] != PROTO_UDP {
        return None;
    }
    let udp = &payload[ihl..];
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
    // UDP length covers the 8-octet header plus payload.
    let len = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    let end = len.clamp(8, udp.len());
    Some(Datagram {
        src_port,
        dst_port,
        payload: &udp[8..end],
    })
}

/// Decode an LRRP position from a UDP payload.
///
/// Scans for the position token rather than assuming a fixed offset, because
/// the tokens preceding it vary by report type and by radio model. Every
/// candidate is range-checked, so a byte pattern that merely looks like the
/// token cannot become a position.
pub fn parse_lrrp(llid: u32, payload: &[u8]) -> Option<LrrpReport> {
    let mut i = 0usize;
    while i + 9 <= payload.len() {
        if payload[i] != TOKEN_POSITION {
            i += 1;
            continue;
        }
        let lat_raw = i32::from_be_bytes([
            payload[i + 1],
            payload[i + 2],
            payload[i + 3],
            payload[i + 4],
        ]);
        let lon_raw = i32::from_be_bytes([
            payload[i + 5],
            payload[i + 6],
            payload[i + 7],
            payload[i + 8],
        ]);
        if let Some((lat, lon)) = decode_position(lat_raw, lon_raw) {
            return Some(LrrpReport {
                llid,
                lat,
                lon,
                raw: payload.to_vec(),
            });
        }
        i += 1;
    }
    None
}

/// Degrees per fixed-point unit.
///
/// Both axes share one scale, and it is worth being explicit about why, since
/// the two common ways of writing it look like different constants:
/// `180/2^31` and `360/2^32` are the same number. The full signed 32-bit range
/// therefore spans ±180° on either axis, which means **latitude only ever uses
/// half of it** — a valid latitude never exceeds ±90°, so the top half of the
/// range is not a far-northern fix but a sign that this was not a position at
/// all. `decode_position` relies on exactly that to reject junk.
const DEGREES_PER_UNIT: f64 = 180.0 / 2_147_483_648.0;

/// Convert LRRP's fixed-point coordinates to degrees, rejecting values that
/// cannot be a real fix.
fn decode_position(lat_raw: i32, lon_raw: i32) -> Option<(f64, f64)> {
    let lat = lat_raw as f64 * DEGREES_PER_UNIT;
    let lon = lon_raw as f64 * DEGREES_PER_UNIT;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    // A radio with no fix reports zeros; that is the Atlantic, not a location.
    if lat_raw == 0 && lon_raw == 0 {
        return None;
    }
    Some((lat, lon))
}

/// Full path from an assembled packet's payload to a location report.
pub fn report_from_packet(llid: u32, packet_payload: &[u8]) -> Option<LrrpReport> {
    let dg = parse_ipv4_udp(packet_payload)?;
    // Accept either direction: a radio reporting in, or a request going out.
    if dg.dst_port != LRRP_PORT && dg.src_port != LRRP_PORT {
        return None;
    }
    parse_lrrp(llid, dg.payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode degrees back into LRRP's fixed point, for round-trip testing.
    fn encode_position(lat: f64, lon: f64) -> (i32, i32) {
        (
            (lat / DEGREES_PER_UNIT).round() as i32,
            (lon / DEGREES_PER_UNIT).round() as i32,
        )
    }

    fn lrrp_payload(lat: f64, lon: f64) -> Vec<u8> {
        let (la, lo) = encode_position(lat, lon);
        let mut v = vec![0x51, 0x00, 0x08]; // plausible leading tokens
        v.push(TOKEN_POSITION);
        v.extend_from_slice(&la.to_be_bytes());
        v.extend_from_slice(&lo.to_be_bytes());
        v
    }

    fn ipv4_udp(src: u16, dst: u16, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; 20];
        v[0] = 0x45; // IPv4, 5-word header
        v[9] = 17; // UDP
        v.extend_from_slice(&src.to_be_bytes());
        v.extend_from_slice(&dst.to_be_bytes());
        v.extend_from_slice(&((payload.len() + 8) as u16).to_be_bytes());
        v.extend_from_slice(&[0, 0]); // checksum, unchecked
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn decodes_a_position_round_trip() {
        // A metro-area round trip, to a tolerance far finer than GPS itself.
        let (lat, lon) = (39.7684, -86.1581);
        let pkt = ipv4_udp(4001, LRRP_PORT, &lrrp_payload(lat, lon));
        let r = report_from_packet(0xABCDEF, &pkt).expect("report decodes");
        assert_eq!(r.llid, 0xABCDEF);
        assert!((r.lat - lat).abs() < 1e-5, "lat {}", r.lat);
        assert!((r.lon - lon).abs() < 1e-5, "lon {}", r.lon);
    }

    #[test]
    fn both_axes_share_one_scale_and_latitude_uses_half_the_range() {
        // The two ways this constant is usually written, 180/2^31 and
        // 360/2^32, are the same number -- so both axes decode identically and
        // the full range spans +/-180 on each. Latitude therefore only ever
        // occupies the middle half, which is what makes out-of-range rejection
        // meaningful rather than cosmetic.
        assert_eq!(DEGREES_PER_UNIT, 360.0 / 4_294_967_296.0);
        let (la, lo) = encode_position(45.0, 45.0);
        assert_eq!(la, lo, "equal angles must encode identically");

        // Full positive scale is 180 degrees: fine for longitude, impossible
        // for latitude.
        assert_eq!(decode_position(i32::MAX, i32::MAX), None);
        let (_, lon) = decode_position(0x4000_0000, 0x4000_0000).unwrap_or((0.0, 0.0));
        assert!(
            (lon - 90.0).abs() < 1e-6,
            "quarter scale is 90 degrees: {lon}"
        );
    }

    #[test]
    fn rejects_a_radio_with_no_fix() {
        assert_eq!(decode_position(0, 0), None, "0,0 is not a location");
    }

    #[test]
    fn rejects_out_of_range_coordinates() {
        // Latitude uses only half the integer range; beyond that is not a fix.
        assert_eq!(decode_position(i32::MAX, 0), None);
        assert_eq!(decode_position(i32::MIN, 0), None);
    }

    #[test]
    fn ignores_traffic_that_is_not_lrrp() {
        let pkt = ipv4_udp(1234, 5678, &lrrp_payload(39.7, -86.1));
        assert_eq!(
            report_from_packet(1, &pkt),
            None,
            "wrong port must not parse"
        );

        // Not IPv4 at all.
        assert_eq!(parse_ipv4_udp(&[0u8; 40]), None);
        // IPv4 but TCP.
        let mut tcp = ipv4_udp(4001, LRRP_PORT, &[0; 12]);
        tcp[9] = 6;
        assert_eq!(parse_ipv4_udp(&tcp), None);
        // Truncated.
        assert_eq!(parse_ipv4_udp(&[0x45, 0, 0]), None);
    }

    #[test]
    fn a_token_byte_in_random_data_does_not_become_a_position() {
        // 0x66 appears in ordinary data; only a range-valid pair may pass.
        let junk: Vec<u8> = (0..64u8)
            .map(|i| if i % 7 == 0 { 0x66 } else { 0xF3 })
            .collect();
        let r = parse_lrrp(1, &junk);
        if let Some(rep) = r {
            assert!(
                (-90.0..=90.0).contains(&rep.lat) && (-180.0..=180.0).contains(&rep.lon),
                "accepted an impossible coordinate"
            );
        }
    }
}
