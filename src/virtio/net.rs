//! VirtIO network device forwarding guest traffic to a host TUN
//! interface. The device answers the guest's ARP requests for the
//! host address and its DHCP requests, and forwards all other packets
//! to the TUN interface.

use super::mmio::*;
use crate::dev::Plic;
use crate::isa::elf::hex_dump;
use crate::logger::{Level, Log};
use crate::memory::Memory;
use crate::network::dhcp::{self, ClientInfo};
use crate::network::tun::{self, Tunnel};
use crate::network::*;
use crate::snapshot::{StateReader, StateWriter};
use crate::system::Netdev;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};

pub const NET_DEVICE_ID: u32 = 1;
pub const NET_SIZE: u64 = 4096;

// Feature bits.

/// Device handles packets with partial checksum offload.
pub const VIRTIO_NET_F_CSUM: u32 = 0;
/// Driver handles packets with partial checksum.
pub const VIRTIO_NET_F_GUEST_CSUM: u32 = 1;
/// Control channel offloads reconfiguration support.
pub const VIRTIO_NET_F_CTRL_GUEST_OFFLOADS: u32 = 2;
/// Device maximum MTU reporting is supported.
pub const VIRTIO_NET_F_MTU: u32 = 3;
/// Device has given MAC address.
pub const VIRTIO_NET_F_MAC: u32 = 5;
/// Driver can receive TSOv4.
pub const VIRTIO_NET_F_GUEST_TSO4: u32 = 7;
/// Driver can receive TSOv6.
pub const VIRTIO_NET_F_GUEST_TSO6: u32 = 8;
/// Driver can receive TSO with ECN.
pub const VIRTIO_NET_F_GUEST_ECN: u32 = 9;
/// Driver can receive UFO.
pub const VIRTIO_NET_F_GUEST_UFO: u32 = 10;
/// Device can receive TSOv4.
pub const VIRTIO_NET_F_HOST_TSO4: u32 = 11;
/// Device can receive TSOv6.
pub const VIRTIO_NET_F_HOST_TSO6: u32 = 12;
/// Device can receive TSO with ECN.
pub const VIRTIO_NET_F_HOST_ECN: u32 = 13;
/// Device can receive UFO.
pub const VIRTIO_NET_F_HOST_UFO: u32 = 14;
/// Driver can merge receive buffers.
pub const VIRTIO_NET_F_MRG_RXBUF: u32 = 15;
/// Configuration status field is available.
pub const VIRTIO_NET_F_STATUS: u32 = 16;
/// Control channel is available.
pub const VIRTIO_NET_F_CTRL_VQ: u32 = 17;
/// Control channel RX mode support.
pub const VIRTIO_NET_F_CTRL_RX: u32 = 18;
/// Control channel VLAN filtering.
pub const VIRTIO_NET_F_CTRL_VLAN: u32 = 19;
/// Control channel RX extra mode support.
pub const VIRTIO_NET_F_CTRL_RX_EXTRA: u32 = 20;
/// Driver can send gratuitous packets.
pub const VIRTIO_NET_F_GUEST_ANNOUNCE: u32 = 21;
/// Device supports multiqueue with automatic receive steering.
pub const VIRTIO_NET_F_MQ: u32 = 22;
/// Set MAC address through control channel.
pub const VIRTIO_NET_F_CTRL_MAC_ADDR: u32 = 23;
/// Device supports inner header hash for encapsulated packets.
pub const VIRTIO_NET_F_HASH_TUNNEL: u32 = 51;
/// Device supports virtqueue notification coalescing.
pub const VIRTIO_NET_F_VQ_NOTF_COAL: u32 = 52;
/// Device supports notifications coalescing.
pub const VIRTIO_NET_F_NOTF_COAL: u32 = 53;
/// Driver can receive USOv4 packets.
pub const VIRTIO_NET_F_GUEST_USO4: u32 = 54;
/// Driver can receive USOv6 packets.
pub const VIRTIO_NET_F_GUEST_USO6: u32 = 55;
/// Device can receive USO packets.
pub const VIRTIO_NET_F_HOST_USO: u32 = 56;
/// Device can report per-packet hash value and a type of calculated
/// hash.
pub const VIRTIO_NET_F_HASH_REPORT: u32 = 57;
/// Driver can provide the exact hdr_len value.
pub const VIRTIO_NET_F_GUEST_HDRLEN: u32 = 59;
/// Device supports RSS with Toeplitz hash calculation and
/// configurable hash parameters for receive steering.
pub const VIRTIO_NET_F_RSS: u32 = 60;
/// Device can process duplicated ACKs and report number of coalesced
/// segments and duplicated ACKs.
pub const VIRTIO_NET_F_RSC_EXT: u32 = 61;
/// Device may act as a standby for a primary device with the same
/// MAC address.
pub const VIRTIO_NET_F_STANDBY: u32 = 62;
/// Device reports speed and duplex.
pub const VIRTIO_NET_F_SPEED_DUPLEX: u32 = 63;

// Packet header flags and GSO types.
pub const VIRTIO_NET_HDR_F_NEEDS_CSUM: u8 = 1;
pub const VIRTIO_NET_HDR_F_DATA_VALID: u8 = 2;
pub const VIRTIO_NET_HDR_F_RSC_INFO: u8 = 4;

pub const VIRTIO_NET_HDR_GSO_NONE: u8 = 0;
pub const VIRTIO_NET_HDR_GSO_TCPV4: u8 = 1;
pub const VIRTIO_NET_HDR_GSO_UDP: u8 = 3;
pub const VIRTIO_NET_HDR_GSO_TCPV6: u8 = 4;
pub const VIRTIO_NET_HDR_GSO_UDP_L4: u8 = 5;
pub const VIRTIO_NET_HDR_GSO_ECN: u8 = 0x80;

/// The receive queue index.
const RECEIVEQ: usize = 0;
/// The transmit queue index.
const TRANSMITQ: usize = 1;

/// Header of the transmitted and received packets.
#[derive(Clone, Copy, Debug, Default)]
pub struct NetHdr {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
    pub num_buffers: u16,
}

impl NetHdr {
    /// Decodes the header from data. The num_buffers field is present
    /// only in 12-byte headers.
    pub fn decode(data: &[u8]) -> Result<NetHdr, String> {
        if data.len() < 10 {
            return Err(format!("invalid transmit packet: len={}", data.len()));
        }
        let u16le = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
        Ok(NetHdr {
            flags: data[0],
            gso_type: data[1],
            hdr_len: u16le(2),
            gso_size: u16le(4),
            csum_start: u16le(6),
            csum_offset: u16le(8),
            num_buffers: if data.len() >= 12 { u16le(10) } else { 0 },
        })
    }
}

impl fmt::Display for NetHdr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut flags = Vec::new();
        if self.flags & VIRTIO_NET_HDR_F_NEEDS_CSUM != 0 {
            flags.push("NEEDS_CSUM");
        }
        if self.flags & VIRTIO_NET_HDR_F_DATA_VALID != 0 {
            flags.push("DATA_VALID");
        }
        if self.flags & VIRTIO_NET_HDR_F_RSC_INFO != 0 {
            flags.push("RSC_INFO");
        }
        let gso = match self.gso_type {
            VIRTIO_NET_HDR_GSO_NONE => "none",
            VIRTIO_NET_HDR_GSO_TCPV4 => "TCPV4",
            VIRTIO_NET_HDR_GSO_UDP => "UDP",
            VIRTIO_NET_HDR_GSO_TCPV6 => "TCP6",
            VIRTIO_NET_HDR_GSO_UDP_L4 => "UDP_L4",
            VIRTIO_NET_HDR_GSO_ECN => "GSO_ECN",
            _ => "",
        };
        write!(
            f,
            "NetHdr: flags={}, gso_type={}, hdr_len={}, gso_size={}, csum_start={}, csum_offset={}, num_buffers={}",
            flags.join(","),
            gso,
            self.hdr_len,
            self.gso_size,
            self.csum_start,
            self.csum_offset,
            self.num_buffers
        )
    }
}

/// A descriptor chain handed to the receiver or sender thread.
struct Chain {
    generation: u64,
    queue: Queue,
    head: u16,
}

/// Device state shared with the worker threads.
struct NetShared {
    log: Log,
    guest: GuestMem,
    host_mac: AtomicMac,
    guest_mac: AtomicMac,
    host_ip: Ipv4Addr,
    tun: Tunnel,
    dhcp: Mutex<dhcp::Server>,
    /// Length of the virtio-net packet header.
    hdr_len: AtomicUsize,
    /// Ethernet frames to deliver to the guest.
    frames: mpsc::SyncSender<Vec<u8>>,
    st_sent: AtomicU64,
    st_rcvd: AtomicU64,
}

/// VirtIO network device.
pub struct Net {
    shared: Arc<NetShared>,
    frames: Option<mpsc::Receiver<Vec<u8>>>,
    recv_desc: Option<mpsc::Sender<Chain>>,
    send_desc: Option<mpsc::Sender<Chain>>,
}

/// A MAC address of the device, which is replaced when a snapshot is
/// restored.
struct AtomicMac(AtomicU64);

impl AtomicMac {
    fn new(mac: Mac) -> AtomicMac {
        let m = AtomicMac(AtomicU64::new(0));
        m.set(mac);
        m
    }

    fn get(&self) -> Mac {
        Mac::from_slice(&self.0.load(Ordering::Relaxed).to_le_bytes())
    }

    fn set(&self, mac: Mac) {
        let mut b = [0u8; 8];
        b[..6].copy_from_slice(&mac.0);
        self.0.store(u64::from_le_bytes(b), Ordering::Relaxed);
    }
}

/// Returns a random locally administered unicast MAC address.
fn random_mac(urandom: &mut File) -> Result<Mac, String> {
    let mut mac = [0u8; 6];
    urandom
        .read_exact(&mut mac)
        .map_err(|e| format!("/dev/urandom: {}", e))?;
    mac[0] = (mac[0] & 0xfe) | 0x02;
    Ok(Mac(mac))
}

impl Net {
    /// Creates a new network device and returns its bus segment. The
    /// device creates a TUN interface with the netdev gateway address
    /// as the local address and the netdev IP as the peer address.
    pub fn create(
        start: u64,
        plic: Plic,
        irq: u32,
        mem: Arc<Memory>,
        netdev: &Netdev,
    ) -> Result<VirtioMmio, String> {
        let tunnel = Tunnel::create()?;
        tunnel
            .configure(&tun::Config {
                local_ip: netdev.gw.clone(),
                remote_ip: netdev.ip.clone(),
            })
            .inspect_err(|e| println!("tunnel.Configure failed: {}", e))?;
        let host_ip: Ipv4Addr = netdev
            .gw
            .parse()
            .map_err(|_| format!("invalid host IP address {}", netdev.gw))?;
        let guest_ip: Ipv4Addr = netdev
            .ip
            .parse()
            .map_err(|_| format!("invalid guest IP address {}", netdev.ip))?;

        let mut state = VirtioState::new(
            "virtio-net",
            NET_DEVICE_ID,
            1 << VIRTIO_NET_F_MAC,
            start,
            NET_SIZE,
            plic.clone(),
            irq,
            mem.clone(),
            2,
        );
        state.log.level = Level::Info;
        plic.register_irq(irq, "net");

        let mut urandom = File::open("/dev/urandom").map_err(|e| format!("/dev/urandom: {}", e))?;
        let host_mac = random_mac(&mut urandom)?;
        let guest_mac = random_mac(&mut urandom)?;

        let mut server = dhcp::Server::new("", host_ip);
        server.domain_name = netdev.domainname.clone();
        server.dns.push(Ipv4Addr::new(8, 8, 8, 8));
        server.dns.push(Ipv4Addr::new(1, 1, 1, 1));
        server.add_client(
            guest_mac,
            ClientInfo {
                ip: guest_ip,
                hostname: netdev.hostname.clone(),
            },
        );

        let log = state.log.clone();
        log.info(format_args!("tunnel: {}", tunnel.name));
        log.info(format_args!("guest : {} {}", guest_mac, guest_ip));
        log.info(format_args!("host  : {} {}", host_mac, host_ip));

        let (frames_tx, frames_rx) = mpsc::sync_channel(0);
        let net = Net {
            shared: Arc::new(NetShared {
                log,
                guest: GuestMem { mem },
                host_mac: AtomicMac::new(host_mac),
                guest_mac: AtomicMac::new(guest_mac),
                host_ip,
                tun: tunnel,
                dhcp: Mutex::new(server),
                hdr_len: AtomicUsize::new(12),
                frames: frames_tx,
                st_sent: AtomicU64::new(0),
                st_rcvd: AtomicU64::new(0),
            }),
            frames: Some(frames_rx),
            recv_desc: None,
            send_desc: None,
        };
        let dev = VirtioDevice::new(state, Box::new(net));
        Ok(VirtioMmio::new("virtio", dev))
    }
}

/// Spawns a named device worker thread.
fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .unwrap_or_else(|e| panic!("failed to start {}: {}", name, e));
}

impl NetShared {
    /// Receiver thread: stores the frames into the receive queue
    /// descriptor chains.
    fn receiver(
        &self,
        dev: Weak<Mutex<VirtioDevice>>,
        descs: mpsc::Receiver<Chain>,
        frames: mpsc::Receiver<Vec<u8>>,
    ) {
        let mut pending: Option<Vec<u8>> = None;
        for chain in descs {
            crate::log_debug!(self.log, "receiving to descriptor {}", chain.head);
            loop {
                let frame = match pending.take() {
                    Some(f) => f,
                    None => match frames.recv() {
                        Ok(f) => f,
                        Err(_) => return,
                    },
                };
                crate::log_debug!(self.log, "received frame of {} bytes", frame.len());

                let Some(dev) = dev.upgrade() else {
                    return;
                };
                let mut d = dev.lock().unwrap();
                if d.state.generation != chain.generation {
                    // The chain predates a device reset: keep the frame
                    // for the next descriptor.
                    pending = Some(frame);
                    break;
                }
                if d.state.frozen {
                    // The system is being suspended; the network drops
                    // the frame and the chain stays in flight.
                    continue;
                }
                let tx = self
                    .store_frame(&chain.queue, chain.head, &frame)
                    .unwrap_or_else(|e| {
                        // Complete the descriptor anyway to avoid
                        // descriptor leaks.
                        self.log.error(format_args!("store packet failed: {}", e));
                        0
                    });
                d.state.complete_descriptor(RECEIVEQ, chain.head, tx);
                break;
            }
        }
    }

    /// Stores the packet header and the frame into the descriptor
    /// chain. Returns the number of bytes written.
    fn store_frame(&self, vq: &Queue, mut idx: u16, frame: &[u8]) -> Result<u32, String> {
        let hdr_len = self.hdr_len.load(Ordering::Relaxed);
        let mut packet = vec![0u8; hdr_len + frame.len()];
        packet[hdr_len..].copy_from_slice(frame);

        let mut data = &packet[..];
        let mut received = 0u32;
        while !data.is_empty() {
            let desc = self.guest.load_desc(vq, idx)?;
            crate::log_debug!(self.log, "desc: {}", desc);
            if desc.flags & VIRTQ_DESC_F_WRITE == 0 {
                return Err(format!(
                    "invalid receiveq{} flags: {:x}",
                    vq.index, desc.flags
                ));
            }
            let n = data.len().min(desc.len as usize);
            self.guest.write(desc.addr, &data[..n])?;
            received += n as u32;
            data = &data[n..];

            if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            idx = desc.next;
        }
        crate::log_debug!(
            self.log,
            "vq{}: transferred {} bytes, {} tail bytes ignored",
            vq.index,
            received,
            data.len()
        );
        if received as usize > hdr_len + 14 {
            // Count only IP bytes.
            self.st_rcvd
                .fetch_add(received as u64 - hdr_len as u64 - 14, Ordering::Relaxed);
        }
        Ok(received)
    }

    /// TUN reader thread: forwards IP packets from the TUN interface
    /// to the guest.
    fn tun_reader(&self) {
        let mut ip = vec![0u8; 1600];
        loop {
            let n = match self.tun.read(&mut ip) {
                Ok(n) => n,
                Err(e) => {
                    self.log.error(format_args!("tun.Read: {}", e));
                    continue;
                }
            };
            debug_ip(&self.log, &ip[..n]);

            let mut frame = vec![0u8; 14 + n];
            make_ethernet(
                &mut frame,
                self.guest_mac.get(),
                self.host_mac.get(),
                ETHERNET_IPV4,
            );
            frame[14..].copy_from_slice(&ip[..n]);
            if self.frames.send(frame).is_err() {
                return;
            }
        }
    }

    /// Sender thread: transmits the transmit queue descriptor chains.
    fn sender(&self, dev: Weak<Mutex<VirtioDevice>>, descs: mpsc::Receiver<Chain>) {
        for chain in descs {
            let tx = self.send(&chain.queue, chain.head).unwrap_or_else(|e| {
                self.log.error(format_args!("send failed: {}", e));
                0
            });
            let Some(dev) = dev.upgrade() else {
                return;
            };
            let mut d = dev.lock().unwrap();
            if d.state.generation == chain.generation {
                d.state.complete_descriptor(TRANSMITQ, chain.head, tx);
            }
        }
    }

    /// Reads the packet from the descriptor chain and processes it.
    /// Returns the number of bytes read.
    fn send(&self, vq: &Queue, idx: u16) -> Result<u32, String> {
        let hdr_len = self.hdr_len.load(Ordering::Relaxed);
        let mut desc = self.guest.load_desc(vq, idx)?;
        crate::log_debug!(self.log, "desc: {}", desc);

        if desc.flags & VIRTQ_DESC_F_WRITE != 0 {
            return Err(format!(
                "invalid transmitq{} flags: {:x}",
                vq.index, desc.flags
            ));
        }
        if (desc.len as usize) < hdr_len {
            return Err(format!("truncated request: len={}", desc.len));
        }

        // The first descriptor holds the header and possibly the start
        // of the payload.
        let mut chunk = vec![0u8; desc.len as usize];
        self.guest.read(desc.addr, &mut chunk)?;
        let hdr = NetHdr::decode(&chunk)?;
        let mut payload = if desc.len <= 12 {
            Vec::new()
        } else {
            chunk.split_off(hdr_len)
        };
        let mut transferred = desc.len;

        // Collect the payload chunks.
        while desc.flags & VIRTQ_DESC_F_NEXT != 0 {
            desc = self.guest.load_desc(vq, desc.next)?;
            crate::log_debug!(self.log, "desc: {}", desc);
            if desc.flags & VIRTQ_DESC_F_WRITE != 0 {
                return Err(format!(
                    "invalid transmitq{} payload flags: {:x}",
                    vq.index, desc.flags
                ));
            }
            let ofs = payload.len();
            payload.resize(ofs + desc.len as usize, 0);
            self.guest.read(desc.addr, &mut payload[ofs..])?;
            transferred += desc.len;
        }

        self.process_send(&hdr, &payload)?;
        Ok(transferred)
    }

    /// Writes the packet to the TUN interface.
    fn tun_write(&self, packet: &[u8]) {
        if let Err(e) = self.tun.write(packet) {
            self.log.error(format_args!("tun.Write: {}", e));
        }
        self.st_sent
            .fetch_add(packet.len() as u64, Ordering::Relaxed);
    }

    /// Delivers the frame to the guest.
    fn deliver(&self, frame: Vec<u8>) {
        // The receiver exits only when the device is dropped.
        let _ = self.frames.send(frame);
    }

    /// Processes the Ethernet frame sent by the guest.
    fn process_send(&self, hdr: &NetHdr, data: &[u8]) -> Result<(), String> {
        crate::log_debug!(self.log, "process: {}", hdr);
        if data.len() < 14 {
            return Err(format!("truncated Ethernet frame: len={}", data.len()));
        }
        let frame_type = be16(&data[12..]);
        let packet = &data[14..];

        match frame_type {
            ETHERNET_IPV4 => {
                debug_ip(&self.log, packet);
                self.log.trace(format_args!("IPv4:\n{}", hex_dump(packet)));
                if !self.respond_ipv4(packet) {
                    self.tun_write(packet);
                }
            }
            ETHERNET_ARP => {
                let arp = Arp::parse(packet)?;
                if arp.oper == 1 {
                    crate::log_debug!(self.log, "ARP  who-has {} tell {}", arp.tpa, arp.spa);
                    if arp.tpa == self.host_ip {
                        // Respond to ARP requests for the host address.
                        let mut resp = vec![0u8; 14 + 28];
                        let host_mac = self.host_mac.get();
                        make_ethernet(&mut resp, arp.sha, host_mac, frame_type);
                        make_arp(&mut resp[14..], 2, host_mac, arp.tpa, arp.sha, arp.spa);
                        self.deliver(resp);
                    }
                } else {
                    crate::log_debug!(self.log, "ARP  {} is-at {}", arp.spa, arp.sha);
                }
            }
            ETHERNET_IPV6 => {
                debug_ip(&self.log, packet);
                self.log.trace(format_args!("IPv6:\n{}", hex_dump(packet)));
                self.tun_write(packet);
            }
            _ => {
                crate::log_debug!(self.log, "Ethernet frame {:04x}", frame_type);
                self.log
                    .trace(format_args!("Ethernet:\n{}", hex_dump(packet)));
                self.tun_write(packet);
            }
        }
        Ok(())
    }

    /// Responds to the IPv4 packets addressed to the device, i.e. the
    /// DHCP requests. Returns true if the packet was consumed.
    fn respond_ipv4(&self, packet: &[u8]) -> bool {
        if packet.len() < 20 {
            return false;
        }
        let hdr_len = ipv4_header_len(packet);
        let proto = packet[9];
        if proto != 17 {
            crate::log_debug!(self.log, "skipping protocol {}", proto);
            return false;
        }
        // UDP.
        if packet.len() < hdr_len + 8 {
            self.log.error(format_args!("len(packet)={}", packet.len()));
            return false;
        }
        let dst_port = be16(&packet[hdr_len + 2..]);
        if dst_port != 67 {
            crate::log_debug!(self.log, "skipping UDP port {}", dst_port);
            return false;
        }

        // DHCP server.
        let req = match dhcp::Dhcp::decode(&packet[hdr_len + 8..]) {
            Ok(req) => req,
            Err(e) => {
                self.log
                    .error(format_args!("failed to decode DHCP message: {}", e));
                return false;
            }
        };
        self.log
            .trace(format_args!("DHCP request data:\n{}", hex_dump(packet)));
        crate::log_debug!(self.log, "DHCP request: {}", req);

        let resp = match self.dhcp.lock().unwrap().request(&req) {
            Ok(Some(resp)) => resp,
            Ok(None) => {
                crate::log_info!(self.log, "ignoring DHCP request {}", req.msg_type);
                return true;
            }
            Err(e) => {
                self.log.error(format_args!("DHCP response: {}", e));
                return false;
            }
        };
        let iface = &self.tun.name;
        let req_chaddr = req.chaddr.prefix(req.hlen);
        let resp_chaddr = resp.chaddr.prefix(resp.hlen);
        let dst_mac = if req.msg_type == dhcp::DHCPDISCOVER {
            crate::log_info!(
                self.log,
                "{} from {} ({}) via {}",
                req.msg_type,
                req_chaddr,
                req.hostname,
                iface
            );
            Mac::BROADCAST
        } else {
            let addr = req
                .address_request
                .map(|a| a.to_string())
                .unwrap_or_default();
            crate::log_info!(
                self.log,
                "{} for {} ({}) from {} ({}) via {}",
                req.msg_type,
                addr,
                self.host_ip,
                req_chaddr,
                req.hostname,
                iface
            );
            self.guest_mac.get()
        };
        crate::log_info!(
            self.log,
            "{} on {} to {} ({}) via {}",
            resp.msg_type,
            resp.yiaddr,
            resp_chaddr,
            req.hostname,
            iface
        );
        crate::log_debug!(self.log, "DHCP response: {}", resp);

        let rdata = resp.encode();
        let mut frame = vec![0u8; 14 + 20 + 8 + rdata.len()];
        make_ethernet(&mut frame, dst_mac, self.host_mac.get(), ETHERNET_IPV4);

        // IP.
        let ip = &mut frame[14..];
        ip[0] = 0x45;
        ip[1] = 0x10;
        put_be16(&mut ip[2..], (20 + 8 + rdata.len()) as u16);
        ip[8] = 0x80; // TTL
        ip[9] = 0x11; // UDP
        ip[12..16].copy_from_slice(&self.host_ip.octets());
        ip[16..20].copy_from_slice(&BROADCAST_IP.octets());
        compute_checksum(ip);

        // UDP.
        put_be16(&mut ip[20..], 67);
        put_be16(&mut ip[22..], 68);
        put_be16(&mut ip[24..], (8 + rdata.len()) as u16);
        ip[28..].copy_from_slice(&rdata);
        compute_udp_checksum(ip);

        self.log
            .trace(format_args!("DHCP response data:\n{}", hex_dump(ip)));
        self.deliver(frame);
        true
    }
}

impl Handler for Net {
    fn attach(&mut self, dev: Weak<Mutex<VirtioDevice>>) {
        let (recv_tx, recv_rx) = mpsc::channel();
        let (send_tx, send_rx) = mpsc::channel();
        self.recv_desc = Some(recv_tx);
        self.send_desc = Some(send_tx);
        let frames = self.frames.take().expect("virtio-net attached twice");

        let shared = self.shared.clone();
        let d = dev.clone();
        spawn("virtio-net-rx", move || shared.receiver(d, recv_rx, frames));
        let shared = self.shared.clone();
        spawn("virtio-net-tx", move || shared.sender(dev, send_rx));
        let shared = self.shared.clone();
        spawn("virtio-net-tun", move || shared.tun_reader());
    }

    fn ready(&mut self, st: &mut VirtioState) {
        let mut hdr_len = 12;
        if st.driver_features[1] & 1 == 0 {
            // Legacy version: 12-byte header only with
            // VIRTIO_NET_F_MRG_RXBUF.
            crate::log_debug!(st.log, "Legacy Version");
            if !st.driver_feature(VIRTIO_NET_F_MRG_RXBUF) {
                hdr_len = 10;
            } else {
                crate::log_debug!(st.log, "VIRTIO_NET_F_MRG_RXBUF");
            }
        } else {
            crate::log_debug!(st.log, "Version 1");
        }
        crate::log_debug!(st.log, "hdrLen={}", hdr_len);
        self.shared.hdr_len.store(hdr_len, Ordering::Relaxed);
    }

    fn drain_queues(&self) -> Vec<usize> {
        vec![TRANSMITQ]
    }

    fn save_state(&self, _st: &VirtioState, w: &mut StateWriter) -> Result<(), String> {
        let s = &self.shared;
        w.u64(s.hdr_len.load(Ordering::Relaxed) as u64);
        w.bytes(&s.host_mac.get().0);
        w.bytes(&s.guest_mac.get().0);
        Ok(())
    }

    fn load_state(&mut self, _st: &mut VirtioState, r: &mut StateReader) -> Result<(), String> {
        let s = &self.shared;
        s.hdr_len.store(r.u64()? as usize, Ordering::Relaxed);
        let mac = |b: &[u8]| {
            if b.len() == 6 {
                Ok(Mac::from_slice(b))
            } else {
                Err("invalid MAC address".to_string())
            }
        };
        let host_mac = mac(r.bytes()?)?;
        let guest_mac = mac(r.bytes()?)?;
        // The guest keeps using the MAC addresses of the suspended
        // system.
        s.host_mac.set(host_mac);
        let old = s.guest_mac.get();
        s.guest_mac.set(guest_mac);
        let mut dhcp = s.dhcp.lock().unwrap();
        if let Some(client) = dhcp.clients.remove(&old) {
            dhcp.add_client(guest_mac, client);
        }
        s.log.info(format_args!(
            "restored guest {}, host {}",
            guest_mac, host_mac
        ));
        Ok(())
    }

    fn device_stats(&self, st: &VirtioState) {
        let s = &self.shared;
        println!(
            "{}: sent    : {}",
            st.log.name,
            FileSize(s.st_sent.load(Ordering::Relaxed))
        );
        println!(
            "{}: rcvd    : {}",
            st.log.name,
            FileSize(s.st_rcvd.load(Ordering::Relaxed))
        );
    }

    fn execute_descriptor_chain(
        &mut self,
        st: &mut VirtioState,
        q: usize,
        idx: u16,
    ) -> Result<u32, String> {
        crate::log_debug!(st.log, "vq{}: chain: idx={}", q, idx);
        let chain = Chain {
            generation: st.generation,
            queue: st.queues[q],
            head: idx,
        };
        let ch = if q == RECEIVEQ {
            &self.recv_desc
        } else {
            &self.send_desc
        };
        if let Some(ch) = ch {
            ch.send(chain).map_err(|e| e.to_string())?;
        }
        Ok(0)
    }

    fn config_load(&mut self, st: &mut VirtioState, offset: u64, size: u64) -> Option<u64> {
        // 5.1.4 Device configuration layout: the MAC address at offset
        // 0x100.
        if size == 1 && (0x100..0x106).contains(&offset) {
            crate::log_debug!(st.log, "Load8(MAC[{}][0x{:03x}])", offset - 0x100, offset);
            return Some(self.shared.guest_mac.get().0[(offset - 0x100) as usize] as u64);
        }
        None
    }
}
