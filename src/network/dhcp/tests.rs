use super::*;
use crate::isa::elf::hex_dump;
use crate::network::*;
use std::net::Ipv4Addr;

const DHCP_PACKETS: [&str; 2] = [
    "00000000  ff ff ff ff ff ff 36 8c  76 8e 5e 96 08 00 45 c0  |......6.v.^...E.|
00000010  01 40 00 00 00 00 40 11  78 ee 00 00 00 00 ff ff  |.@....@.x.......|
00000020  ff ff 00 44 00 43 01 2c  47 1b 01 01 06 00 c8 33  |...D.C.,G......3|
00000030  c1 eb 00 01 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000040  00 00 00 00 00 00 36 8c  76 8e 5e 96 00 00 00 00  |......6.v.^.....|
00000050  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000060  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000070  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000080  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000090  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
000000a0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
000000b0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
000000c0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
000000d0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
000000e0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
000000f0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000100  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000110  00 00 00 00 00 00 63 82  53 63 35 01 01 3d 13 ff  |......c.Sc5..=..|
00000120  d9 d7 82 69 00 02 00 00  ab 11 1e c8 96 66 73 cb  |...i.........fs.|
00000130  b0 bb 37 0c 01 03 06 0c  0f 1a 21 2a 72 77 78 79  |..7.......!*rwxy|
00000140  39 02 05 c0 50 00 0c 05  67 6f 65 6d 75 ff        |9...P...goemu.|
",
    "00000000  ff ff ff ff ff ff 36 8c  76 8e 5e 96 08 00 45 c0  |......6.v.^...E.|
00000010  01 4a 00 00 00 00 40 11  78 e4 00 00 00 00 ff ff  |.J....@.x.......|
00000020  ff ff 00 44 00 43 01 36  56 ad 01 01 06 00 c8 33  |...D.C.6V......3|
00000030  c1 eb 00 01 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000040  00 00 00 00 00 00 36 8c  76 8e 5e 96 00 00 00 00  |......6.v.^.....|
00000050  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000060  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000070  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000080  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000090  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
000000a0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
000000b0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
000000c0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
000000d0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
000000e0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
000000f0  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000100  00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00  |................|
00000110  00 00 00 00 00 00 63 82  53 63 35 01 03 3d 13 ff  |......c.Sc5..=..|
00000120  d9 d7 82 69 00 02 00 00  ab 11 1e c8 96 66 73 cb  |...i.........fs.|
00000130  b0 bb 37 0c 01 03 06 0c  0f 1a 21 2a 72 77 78 79  |..7.......!*rwxy|
00000140  39 02 05 c0 36 04 c0 a8  2a fe 32 04 c0 a8 2a 02  |9...6...*.2...*.|
00000150  0c 05 67 6f 65 6d 75 ff                           |..goemu.|
",
];

/// Parses a hex dump in the format of hex_dump.
fn parse_hex_dump(dump: &str) -> Vec<u8> {
    let mut data = Vec::new();
    for line in dump.lines() {
        let hex = line.split('|').next().unwrap();
        for b in hex.split_whitespace().skip(1) {
            data.push(u8::from_str_radix(b, 16).expect("invalid hex byte"));
        }
    }
    data
}

#[test]
fn dhcp_decode() {
    let host_ip = Ipv4Addr::new(192, 168, 42, 254);
    let guest_ip = Ipv4Addr::new(192, 168, 42, 1);
    let guest_mac = Mac([0x36, 0x8c, 0x76, 0x8e, 0x5e, 0x96]);
    let host_mac = Mac([0x7e, 0xf1, 0x23, 0x40, 0x3f, 0xe6]);

    let mut server = Server::new("", host_ip);
    server.domain_name = "goemu.markkurossi.com".to_string();
    server.dns.push(Ipv4Addr::new(8, 8, 8, 8));
    server.dns.push(Ipv4Addr::new(1, 1, 1, 1));
    server.add_client(
        guest_mac,
        ClientInfo {
            ip: guest_ip,
            hostname: "freebsd".to_string(),
        },
    );

    let expected = [(DHCPDISCOVER, DHCPOFFER), (DHCPREQUEST, DHCPACK)];

    for (idx, packet) in DHCP_PACKETS.iter().enumerate() {
        // Skip Ethernet frame.
        let mut data = parse_hex_dump(packet)[14..].to_vec();

        print!("IP:\n{}", hex_dump(&data[..20]));
        assert!(verify_checksum(&data), "DHCP-{}: IP checksum", idx);
        assert!(compute_checksum(&mut data));
        assert!(verify_checksum(&data), "DHCP-{}: computed IP checksum", idx);
        print!("UDP:\n{}", hex_dump(&data[20..28]));
        assert!(verify_udp_checksum(&data), "DHCP-{}: UDP checksum", idx);
        assert!(compute_udp_checksum(&mut data));
        assert!(
            verify_udp_checksum(&data),
            "DHCP-{}: computed UDP checksum",
            idx
        );

        let req = Dhcp::decode(&data[28..]).unwrap();
        println!("DHCP:{}:0: {}", idx, req);
        assert_eq!(req.cookie, OPTIONS_COOKIE, "DHCP-{}: cookie", idx);
        assert_eq!(req.msg_type, expected[idx].0);
        assert_eq!(req.hostname, "goemu");

        let resp = server.request(&req).unwrap().expect("response");
        println!("DHCP:{}:1: {}", idx, resp);
        assert_eq!(resp.msg_type, expected[idx].1);
        assert_eq!(resp.yiaddr, guest_ip);
        assert_eq!(resp.xid, req.xid);

        let rdata = resp.encode();
        print!("Response:\n{}", hex_dump(&rdata));
        assert_eq!(rdata.len() % 4, 0);
        let dr = Dhcp::decode(&rdata).unwrap();
        println!("dhcp:{}:2: {}", idx, dr);
        assert_eq!(dr.msg_type, resp.msg_type);
        assert_eq!(dr.yiaddr, guest_ip);
        assert_eq!(dr.chaddr, resp.chaddr);
        assert_eq!(&dr.options[..resp.options.len()], &resp.options[..]);
        assert!(dr.options[resp.options.len()..]
            .iter()
            .all(|o| o.tag == TAG_PAD));

        // Construct an offer packet.
        let mut data = vec![0u8; 12 + 14 + 20 + 8 + rdata.len()];
        make_ethernet(&mut data[12..], guest_mac, host_mac, ETHERNET_IPV4);

        // IP.
        let ip = 12 + 14;
        data[ip] = 0x45;
        data[ip + 1] = 0x10;
        put_be16(&mut data[ip + 2..], (20 + 8 + rdata.len()) as u16);
        data[ip + 8] = 0x80; // TTL
        data[ip + 9] = 0x11; // UDP
        data[ip + 12..ip + 16].copy_from_slice(&host_ip.octets());
        data[ip + 16..ip + 20].copy_from_slice(&BROADCAST_IP.octets());
        assert!(compute_checksum(&mut data[ip..]));
        assert!(verify_checksum(&data[ip..]));

        // UDP.
        put_be16(&mut data[ip + 20..], 67);
        put_be16(&mut data[ip + 22..], 68);
        put_be16(&mut data[ip + 24..], (8 + rdata.len()) as u16);
        data[ip + 28..].copy_from_slice(&rdata);
        assert!(compute_udp_checksum(&mut data[ip..]));
        assert!(verify_udp_checksum(&data[ip..]));

        print!("Full {}:\n{}", resp.msg_type, hex_dump(&data[ip..]));
    }
}

#[test]
fn dhcp_unknown_client() {
    let server = Server::new("", Ipv4Addr::new(192, 168, 42, 254));
    let data = parse_hex_dump(DHCP_PACKETS[0]);
    let req = Dhcp::decode(&data[14 + 28..]).unwrap();
    assert!(server.request(&req).is_err());
}
