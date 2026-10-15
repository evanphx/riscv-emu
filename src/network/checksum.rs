//! IPv4 header and UDP checksums.

use super::{be16, put_be16};

/// Adds the 16-bit big-endian words of data to sum. An odd trailing
/// byte is padded with zero.
fn sum_words(mut sum: u32, data: &[u8]) -> u32 {
    let (words, rest) = data.as_chunks::<2>();
    for w in words {
        sum += u16::from_be_bytes(*w) as u32;
    }
    if let [b] = rest {
        sum += (*b as u32) << 8;
    }
    sum
}

/// Folds the carries of sum and returns its ones' complement.
fn fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Returns the IPv4 header length in bytes.
pub fn ipv4_header_len(packet: &[u8]) -> usize {
    (packet[0] & 0x0f) as usize * 4
}

/// Computes the checksum of the 20-byte IPv4 header. The result is 0
/// for a header with a valid checksum.
pub fn checksum(packet: &[u8]) -> u16 {
    if packet.len() < 20 {
        return 0;
    }
    fold(sum_words(0, &packet[..20]))
}

/// Computes and stores the IPv4 header checksum.
pub fn compute_checksum(packet: &mut [u8]) -> bool {
    if packet.len() < 20 {
        return false;
    }
    put_be16(&mut packet[10..], 0);
    let sum = checksum(packet);
    put_be16(&mut packet[10..], sum);
    true
}

/// Verifies the IPv4 header checksum.
pub fn verify_checksum(packet: &[u8]) -> bool {
    packet.len() >= 20 && checksum(packet) == 0
}

/// Computes the UDP checksum of the IPv4 packet, including the
/// pseudo-header. The result is 0 for a packet with a valid
/// checksum.
pub fn udp_checksum(packet: &[u8]) -> u16 {
    if packet.len() < 20 + 8 {
        return 0;
    }
    let hdr_len = ipv4_header_len(packet);
    let payload_len = (be16(&packet[2..]) as usize).saturating_sub(hdr_len);

    // Pseudo-header: source and destination addresses, protocol, and
    // UDP length.
    let mut sum = sum_words(0, &packet[12..20]);
    sum += packet[9] as u32;
    sum += payload_len as u32;

    let end = (hdr_len + payload_len).min(packet.len());
    let start = hdr_len.min(end);
    fold(sum_words(sum, &packet[start..end]))
}

/// Computes and stores the UDP checksum of the IPv4 packet.
pub fn compute_udp_checksum(packet: &mut [u8]) -> bool {
    if packet.len() < 20 + 8 {
        return false;
    }
    let ofs = ipv4_header_len(packet) + 6;
    if ofs + 2 > packet.len() {
        return false;
    }
    put_be16(&mut packet[ofs..], 0);
    let sum = udp_checksum(packet);
    put_be16(&mut packet[ofs..], sum);
    true
}

/// Verifies the UDP checksum of the IPv4 packet.
pub fn verify_udp_checksum(packet: &[u8]) -> bool {
    packet.len() >= 20 + 8 && udp_checksum(packet) == 0
}
