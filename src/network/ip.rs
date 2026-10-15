//! Internet protocol (IP) helpers.

use super::{be16, ipv4_header_len};
use crate::isa::elf::hex_dump;
use crate::logger::Log;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The limited broadcast address 255.255.255.255.
pub const BROADCAST_IP: Ipv4Addr = Ipv4Addr::BROADCAST;

/// Returns the name of the IP protocol number.
pub fn ip_proto_name(proto: u8) -> String {
    let name = match proto {
        0 => "HOPOPT",
        1 => "ICMP",
        2 => "IGMP",
        3 => "GGP",
        4 => "IP-in-IP",
        5 => "ST",
        6 => "TCP",
        7 => "CBT",
        8 => "EGP",
        9 => "IGP",
        10 => "BBN-RCC-MON",
        11 => "NVP-II",
        12 => "PUP",
        13 => "ARGUS",
        14 => "EMCON",
        15 => "XNET",
        16 => "CHAOS",
        17 => "UDP",
        18 => "MUX",
        19 => "DCN-MEAS",
        20 => "HMP",
        21 => "PRM",
        22 => "XNS-IDP",
        23 => "TRUNK-1",
        24 => "TRUNK-2",
        25 => "LEAF-1",
        26 => "LEAF-2",
        27 => "RDP",
        28 => "IRTP",
        29 => "ISO-TP4",
        30 => "NETBLT",
        31 => "MFE-NSP",
        32 => "MERIT-INP",
        33 => "DCCP",
        34 => "3PC",
        35 => "IDPR",
        36 => "XTP",
        37 => "DDP",
        38 => "IDPR-CMTP",
        39 => "TP++",
        40 => "IL",
        41 => "IPv6",
        42 => "SDRP",
        43 => "IPv6-Route",
        44 => "IPv6-Frag",
        45 => "IDRP",
        46 => "RSVP",
        47 => "GRE",
        48 => "DSR",
        49 => "BNA",
        50 => "ESP",
        51 => "AH",
        52 => "I-NLSP",
        53 => "SwIPe",
        54 => "NARP",
        55 => "MOBILE",
        56 => "TLSP",
        57 => "SKIP",
        58 => "IPv6-ICMP",
        59 => "IPv6-NoNxt",
        60 => "IPv6-Opts",
        62 => "CFTP",
        64 => "SAT-EXPAK",
        65 => "KRYPTOLAN",
        66 => "RVD",
        67 => "IPPC",
        69 => "SAT-MON",
        70 => "VISA",
        71 => "IPCU",
        72 => "CPNX",
        73 => "CPHB",
        74 => "WSN",
        75 => "PVP",
        76 => "BR-SAT-MON",
        77 => "SUN-ND",
        78 => "WB-MON",
        79 => "WB-EXPAK",
        80 => "ISO-IP",
        81 => "VMTP",
        82 => "SECURE-VMTP",
        83 => "VINES",
        84 => "IPTM",
        85 => "NSFNET-IGP",
        86 => "DGP",
        87 => "TCF",
        88 => "EIGRP",
        89 => "OSPF",
        90 => "Sprite-RPC",
        91 => "LARP",
        92 => "MTP",
        93 => "AX.25",
        94 => "OS",
        95 => "MICP",
        96 => "SCC-SP",
        97 => "ETHERIP",
        98 => "ENCAP",
        100 => "GMTP",
        101 => "IFMP",
        102 => "PNNI",
        103 => "PIM",
        104 => "ARIS",
        105 => "SCPS",
        106 => "QNX",
        107 => "A/N",
        108 => "IPComp",
        109 => "SNP",
        110 => "Compaq-Peer",
        111 => "IPX-in-IP",
        112 => "VRRP",
        113 => "PGM",
        115 => "L2TP",
        116 => "DDX",
        117 => "IATP",
        118 => "STP",
        119 => "SRP",
        120 => "UTI",
        121 => "SMP",
        122 => "SM",
        123 => "PTP",
        124 => "IS-IS over IPv4",
        125 => "FIRE",
        126 => "CRTP",
        127 => "CRUDP",
        128 => "SSCOPMCE",
        129 => "IPLT",
        130 => "SPS",
        131 => "PIPE",
        132 => "SCTP",
        133 => "FC",
        134 => "RSVP-E2E-IGNORE",
        135 => "Mobility Header",
        136 => "UDPLite",
        137 => "MPLS-in-IP",
        138 => "manet",
        139 => "HIP",
        140 => "Shim6",
        141 => "WESP",
        142 => "ROHC",
        143 => "Ethernet",
        144 => "AGGFRAG",
        145 => "NSH",
        146 => "Homa",
        147 => "BIT-EMU",
        _ => return format!("{:02x}", proto),
    };
    name.to_string()
}

/// Returns the name of the ICMP message type.
pub fn icmp_type_name(t: u8) -> String {
    let name = match t {
        0 => "echo reply",
        3 => "destination unreachable",
        4 => "source quench",
        5 => "redirect message",
        8 => "echo",
        9 => "router advertisement",
        10 => "router solicitation",
        11 => "time exceeded",
        12 => "parameter problem",
        13 => "timestamp",
        14 => "timestamp reply",
        15 => "information request",
        16 => "information reply",
        17 => "address mask request",
        18 => "address mask reply",
        30 => "traceroute",
        42 => "extended echo",
        43 => "extended echo reply",
        _ => return format!("{:02x}", t),
    };
    name.to_string()
}

/// Verifies the IP packet and logs it to log at the debug level.
/// Returns the IP version of the packet, or 0 if the packet is
/// invalid.
pub fn debug_ip(log: &Log, packet: &[u8]) -> u8 {
    if packet.len() < 20 {
        log.error(format_args!("truncated IP packet:\n{}", hex_dump(packet)));
        return 0;
    }
    let v = packet[0] >> 4;
    let (hdr_len, proto, src, dst): (usize, u8, IpAddr, IpAddr) = match v {
        4 => {
            let addr = |o: usize| {
                IpAddr::V4(Ipv4Addr::new(
                    packet[o],
                    packet[o + 1],
                    packet[o + 2],
                    packet[o + 3],
                ))
            };
            (ipv4_header_len(packet), packet[9], addr(12), addr(16))
        }
        6 => {
            if packet.len() < 40 {
                log.error(format_args!("truncated IPv6 packet:\n{}", hex_dump(packet)));
                return 0;
            }
            let addr = |o: usize| {
                let mut a = [0u8; 16];
                a.copy_from_slice(&packet[o..o + 16]);
                IpAddr::V6(Ipv6Addr::from(a))
            };
            (40, packet[6], addr(8), addr(24))
        }
        _ => {
            log.error(format_args!(
                "invalid IP packet: version={}:\n{}",
                v,
                hex_dump(packet)
            ));
            return 0;
        }
    };
    if !log.enabled(crate::logger::Level::Debug) {
        return v;
    }

    let (src_port, dst_port) = if packet.len() >= hdr_len + 4 {
        (be16(&packet[hdr_len..]), be16(&packet[hdr_len + 2..]))
    } else {
        (0, 0)
    };
    match proto {
        1 => {
            let t = packet.get(hdr_len).copied().unwrap_or(0);
            log.debug(format_args!(
                "ICMP {} -> {} {}",
                src,
                dst,
                icmp_type_name(t)
            ));
        }
        6 | 17 => log.debug(format_args!(
            "{} {}:{} -> {}:{}",
            ip_proto_name(proto),
            src,
            src_port,
            dst,
            dst_port
        )),
        _ => log.debug(format_args!("{} {} -> {}", ip_proto_name(proto), src, dst)),
    }
    v
}
