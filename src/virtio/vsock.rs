//! VirtIO socket device (vsock): stream sockets between the guest and
//! host programs over a Unix domain socket, in the style of
//! Firecracker's hybrid vsock.
//!
//! - A host program connects to the device's Unix socket and writes
//!   `CONNECT <port>\n`. The device opens a connection to the guest
//!   port and answers `OK <host port>\n` when the guest accepts it;
//!   if the guest refuses, the device closes the host socket. After
//!   that, the socket carries the stream data.
//! - A guest connection to port P of the host (CID 2) is forwarded to
//!   the Unix socket `<path>_P`, if a host program listens on it.
//!
//! Connections do not survive a snapshot: the host end of a
//! connection is gone when the system is restored. A restored device
//! sends the guest a transport reset event, so that the guest resets
//! its connected sockets. Listening sockets stay.

use super::mmio::*;
use crate::dev::Plic;
use crate::memory::Memory;
use crate::snapshot::{StateReader, StateWriter};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

pub const VSOCK_DEVICE_ID: u32 = 19;
pub const VSOCK_SIZE: u64 = 0x200;

/// The CID of the host.
pub const HOST_CID: u64 = 2;

const RXQ: usize = 0;
const TXQ: usize = 1;
const EVENTQ: usize = 2;

/// Length of the packet header (struct virtio_vsock_hdr).
const HDR_LEN: usize = 44;

const TYPE_STREAM: u16 = 1;

const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;

/// Shutdown flags: the peer will receive or send no more data.
const SHUTDOWN_RCV: u32 = 1;
const SHUTDOWN_SEND: u32 = 2;

const EVENT_TRANSPORT_RESET: u32 = 0;

/// Receive buffer space of the device per connection: data from the
/// guest that has not been written to the host socket.
const BUF_ALLOC: u32 = 256 * 1024;

/// Largest host read forwarded to the guest at a time.
const MAX_READ: usize = 64 * 1024;

/// First port number of host-initiated connections.
const FIRST_HOST_PORT: u32 = 1 << 30;

/// Packet header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Hdr {
    src_cid: u64,
    dst_cid: u64,
    src_port: u32,
    dst_port: u32,
    len: u32,
    typ: u16,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}

impl Hdr {
    fn decode(b: &[u8]) -> Hdr {
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        Hdr {
            src_cid: u64_at(0),
            dst_cid: u64_at(8),
            src_port: u32_at(16),
            dst_port: u32_at(20),
            len: u32_at(24),
            typ: u16_at(28),
            op: u16_at(30),
            flags: u32_at(32),
            buf_alloc: u32_at(36),
            fwd_cnt: u32_at(40),
        }
    }

    fn encode(&self) -> [u8; HDR_LEN] {
        let mut b = [0u8; HDR_LEN];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.typ.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }
}

/// A connection is identified by the host and the guest port.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct Key {
    host_port: u32,
    guest_port: u32,
}

/// A packet to the guest. The credit fields are filled in when it is
/// delivered.
struct Packet {
    key: Key,
    op: u16,
    flags: u32,
    data: Vec<u8>,
}

/// Work for the thread writing guest data to the host socket.
enum WriterMsg {
    Data(Vec<u8>),
    /// The guest sends no more data.
    ShutdownWrite,
}

struct Conn {
    /// Unique among all connections of the device, so that the worker
    /// threads of a closed connection notice that it is gone even if
    /// its ports are reused.
    id: u64,
    stream: UnixStream,
    established: bool,
    writer: Option<mpsc::Sender<WriterMsg>>,
    /// The guest's receive buffer size and the bytes it has consumed.
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// Bytes sent to the guest.
    tx_cnt: u32,
    /// Bytes of guest data written to the host socket.
    fwd_cnt: u32,
    /// fwd_cnt last reported to the guest.
    fwd_cnt_sent: u32,
    credit_requested: bool,
    /// Shutdown flags received from the guest.
    guest_shutdown: u32,
}

impl Conn {
    fn new(id: u64, stream: UnixStream, established: bool) -> Conn {
        Conn {
            id,
            stream,
            established,
            writer: None,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            tx_cnt: 0,
            fwd_cnt: 0,
            fwd_cnt_sent: 0,
            credit_requested: false,
            guest_shutdown: 0,
        }
    }

    /// Bytes the guest can receive.
    fn peer_credit(&self) -> u32 {
        self.peer_buf_alloc
            .saturating_sub(self.tx_cnt.wrapping_sub(self.peer_fwd_cnt))
    }

    fn close(&mut self) {
        self.writer = None;
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// Device state. It is locked after the device lock.
#[derive(Default)]
struct Inner {
    guest_cid: u64,
    conns: HashMap<Key, Conn>,
    next_port: u32,
    /// Packets waiting for receive buffers.
    pending: VecDeque<Packet>,
    /// Receive and event buffers posted by the guest, in order.
    rx_chains: VecDeque<u16>,
    event_chains: VecDeque<u16>,
    /// A transport reset event is waiting for an event buffer.
    reset_pending: bool,
    /// The guest has not handled the transport reset yet: new host
    /// connections would be reset with the old ones. It has when it
    /// posts an event buffer after the last one taken, at this
    /// available ring position, that is, after the restored buffers
    /// and the one that took the event.
    reset_unacked: Option<u16>,
}

struct Shared {
    uds: String,
    inner: Mutex<Inner>,
    /// Signaled when the guest grants credit or a connection closes.
    credit: Condvar,
    next_id: AtomicU64,
}

/// VirtIO socket device.
pub struct Vsock {
    shared: Arc<Shared>,
    listener: Option<UnixListener>,
    dev: Option<Weak<Mutex<VirtioDevice>>>,
}

impl Vsock {
    /// Creates a new socket device for the guest CID and returns its
    /// bus segment. Host programs connect to the guest through the
    /// Unix socket uds, which is created; an empty uds disables host
    /// connections.
    pub fn create(
        start: u64,
        plic: Plic,
        irq: u32,
        mem: Arc<Memory>,
        guest_cid: u64,
        uds: &str,
    ) -> Result<VirtioMmio, String> {
        if guest_cid <= HOST_CID || guest_cid >= u32::MAX as u64 {
            return Err(format!("invalid guest CID {}", guest_cid));
        }
        plic.register_irq(irq, "vsock");
        let state = VirtioState::new(
            "virtio-vsock",
            VSOCK_DEVICE_ID,
            0,
            start,
            VSOCK_SIZE,
            plic,
            irq,
            mem,
            3,
        );
        let listener = if uds.is_empty() {
            None
        } else {
            // A socket file left by an earlier run.
            if std::fs::symlink_metadata(uds)
                .is_ok_and(|m| std::os::unix::fs::FileTypeExt::is_socket(&m.file_type()))
            {
                let _ = std::fs::remove_file(uds);
            }
            Some(UnixListener::bind(uds).map_err(|e| format!("{}: {}", uds, e))?)
        };
        let dev = VirtioDevice::new(
            state,
            Box::new(Vsock {
                shared: Arc::new(Shared {
                    uds: uds.to_string(),
                    inner: Mutex::new(Inner {
                        guest_cid,
                        next_port: FIRST_HOST_PORT,
                        ..Default::default()
                    }),
                    credit: Condvar::new(),
                    next_id: AtomicU64::new(1),
                }),
                listener,
                dev: None,
            }),
        );
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

/// Locks the device and the device state.
fn lock<'a>(
    dev: &'a Mutex<VirtioDevice>,
    shared: &'a Shared,
) -> (MutexGuard<'a, VirtioDevice>, MutexGuard<'a, Inner>) {
    let d = dev.lock().unwrap();
    let inner = shared.inner.lock().unwrap();
    (d, inner)
}

impl Inner {
    fn send(&mut self, key: Key, op: u16, flags: u32, data: Vec<u8>) {
        self.pending.push_back(Packet {
            key,
            op,
            flags,
            data,
        });
    }

    fn rst(&mut self, key: Key) {
        self.send(key, OP_RST, 0, Vec::new());
    }

    fn remove(&mut self, key: Key) {
        if let Some(mut c) = self.conns.remove(&key) {
            c.close();
        }
    }

    /// Fills the next pending packet into the receive descriptor chain
    /// and returns the number of bytes written, or None if no packet
    /// is pending. A payload larger than the buffer is split.
    fn fill(&mut self, guest: &GuestMem, vq: &Queue, head: u16) -> Result<Option<u32>, String> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        // The writable buffers of the chain.
        let mut bufs = Vec::new();
        let mut cap = 0usize;
        let mut idx = head;
        loop {
            let desc = guest.load_desc(vq, idx)?;
            if desc.flags & VIRTQ_DESC_F_WRITE != 0 {
                bufs.push(desc);
                cap += desc.len as usize;
            }
            if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            idx = desc.next;
        }
        if cap <= HDR_LEN {
            return Err(format!("receive buffer of {} bytes", cap));
        }
        let pkt = self.pending.front_mut().unwrap();
        let n = pkt.data.len().min(cap - HDR_LEN);
        let data: Vec<u8> = if n == pkt.data.len() {
            std::mem::take(&mut pkt.data)
        } else {
            pkt.data.drain(..n).collect()
        };
        let (key, op, flags) = (pkt.key, pkt.op, pkt.flags);
        if pkt.data.is_empty() {
            self.pending.pop_front();
        }
        let mut hdr = Hdr {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: key.host_port,
            dst_port: key.guest_port,
            len: data.len() as u32,
            typ: TYPE_STREAM,
            op,
            flags,
            ..Default::default()
        };
        if let Some(c) = self.conns.get_mut(&key) {
            hdr.buf_alloc = BUF_ALLOC;
            hdr.fwd_cnt = c.fwd_cnt;
            c.fwd_cnt_sent = c.fwd_cnt;
        }
        let mut packet = Vec::with_capacity(HDR_LEN + data.len());
        packet.extend_from_slice(&hdr.encode());
        packet.extend_from_slice(&data);
        let mut rest = &packet[..];
        for desc in bufs {
            let k = rest.len().min(desc.len as usize);
            guest.write(desc.addr, &rest[..k])?;
            rest = &rest[k..];
            if rest.is_empty() {
                break;
            }
        }
        Ok(Some(packet.len() as u32))
    }

    /// Delivers the pending packets into the receive buffers that the
    /// guest has posted.
    fn pump(&mut self, st: &mut VirtioState) {
        if st.frozen {
            return;
        }
        while let Some(&head) = self.rx_chains.front() {
            let vq = st.queues[RXQ];
            match self.fill(&st.guest, &vq, head) {
                Ok(None) => break,
                Ok(Some(n)) => {
                    self.rx_chains.pop_front();
                    st.complete_descriptor(RXQ, head, n);
                }
                Err(e) => {
                    st.log.error(format_args!("receive: {}", e));
                    self.rx_chains.pop_front();
                    st.complete_descriptor(RXQ, head, 0);
                }
            }
        }
    }

    /// Writes a pending transport reset event into the event buffer.
    fn event(&mut self, guest: &GuestMem, vq: &Queue, head: u16) -> Result<u32, String> {
        let desc = guest.load_desc(vq, head)?;
        if desc.flags & VIRTQ_DESC_F_WRITE == 0 || desc.len < 4 {
            return Err("invalid event buffer".to_string());
        }
        guest.write(desc.addr, &EVENT_TRANSPORT_RESET.to_le_bytes())?;
        self.reset_pending = false;
        // The ring position after this buffer, or after the buffers
        // resubmitted with it.
        let pos = vq.last_avail_idx;
        self.reset_unacked = Some(match self.reset_unacked {
            Some(p) if p.wrapping_sub(pos) as i16 > 0 => p,
            _ => pos,
        });
        Ok(4)
    }
}

impl Shared {
    /// Accepts host connections on the Unix socket.
    fn acceptor(self: Arc<Shared>, dev: Weak<Mutex<VirtioDevice>>, listener: UnixListener) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let shared = self.clone();
            let dev = dev.clone();
            spawn("vsock-connect", move || shared.host_connect(dev, stream));
        }
    }

    /// Reads the CONNECT request of a host connection and opens the
    /// connection to the guest.
    fn host_connect(&self, dev: Weak<Mutex<VirtioDevice>>, mut stream: UnixStream) {
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        while line.len() < 64 {
            match stream.read(&mut b) {
                Ok(1) if b[0] == b'\n' => break,
                Ok(1) => line.push(b[0]),
                _ => return,
            }
        }
        let line = String::from_utf8_lossy(&line);
        let port = match line
            .trim()
            .strip_prefix("CONNECT ")
            .map(|p| p.trim().parse())
        {
            Some(Ok(p)) => p,
            _ => {
                let _ = stream.write_all(b"ERR invalid request\n");
                return;
            }
        };
        let Some(dev) = dev.upgrade() else {
            return;
        };
        let (mut d, mut inner) = lock(&dev, self);
        if !d.state.driver_ok() || d.state.frozen || inner.reset_unacked.is_some() {
            // The program retries.
            return;
        }
        let inner = &mut *inner;
        let mut key;
        loop {
            key = Key {
                host_port: inner.next_port,
                guest_port: port,
            };
            inner.next_port = inner.next_port.checked_add(1).unwrap_or(FIRST_HOST_PORT);
            if !inner.conns.contains_key(&key) {
                break;
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        inner.conns.insert(key, Conn::new(id, stream, false));
        inner.send(key, OP_REQUEST, 0, Vec::new());
        inner.pump(&mut d.state);
    }

    /// Starts the threads moving the data of an established connection.
    fn start_conn(self: &Arc<Shared>, dev: &Weak<Mutex<VirtioDevice>>, key: Key, conn: &mut Conn) {
        let (read_stream, write_stream) = match (conn.stream.try_clone(), conn.stream.try_clone()) {
            (Ok(r), Ok(w)) => (r, w),
            _ => return,
        };
        let (tx, rx) = mpsc::channel();
        conn.writer = Some(tx);
        let id = conn.id;
        let (shared, d) = (self.clone(), dev.clone());
        spawn("vsock-reader", move || {
            shared.reader(d, key, id, read_stream)
        });
        let (shared, d) = (self.clone(), dev.clone());
        spawn("vsock-writer", move || {
            shared.writer(d, key, id, write_stream, rx)
        });
    }

    /// Forwards host data to the guest within the guest's credit.
    fn reader(&self, dev: Weak<Mutex<VirtioDevice>>, key: Key, id: u64, mut stream: UnixStream) {
        let mut buf = vec![0u8; MAX_READ];
        loop {
            let Some(dev) = dev.upgrade() else {
                return;
            };
            // Wait for credit.
            let credit = {
                let mut d = dev.lock().unwrap();
                loop {
                    let mut guard = self.inner.lock().unwrap();
                    let inner = &mut *guard;
                    let Some(c) = inner.conns.get_mut(&key).filter(|c| c.id == id) else {
                        return;
                    };
                    if c.guest_shutdown & SHUTDOWN_RCV != 0 {
                        return;
                    }
                    let credit = c.peer_credit();
                    if credit > 0 {
                        break credit;
                    }
                    if !c.credit_requested {
                        c.credit_requested = true;
                        inner.send(key, OP_CREDIT_REQUEST, 0, Vec::new());
                        inner.pump(&mut d.state);
                    }
                    drop(guard);
                    d = self.credit.wait(d).unwrap();
                }
            };
            let n = match stream.read(&mut buf[..MAX_READ.min(credit as usize)]) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => 0,
            };
            let (mut d, mut inner) = lock(&dev, self);
            let inner = &mut *inner;
            let Some(c) = inner.conns.get_mut(&key).filter(|c| c.id == id) else {
                return;
            };
            if n == 0 {
                // The host closed its end: the guest receives no more
                // data.
                inner.send(key, OP_SHUTDOWN, SHUTDOWN_SEND, Vec::new());
                inner.pump(&mut d.state);
                return;
            }
            if d.state.frozen {
                // The system is being suspended.
                continue;
            }
            c.tx_cnt = c.tx_cnt.wrapping_add(n as u32);
            inner.send(key, OP_RW, 0, buf[..n].to_vec());
            inner.pump(&mut d.state);
        }
    }

    /// Writes guest data to the host socket and returns credit to the
    /// guest.
    fn writer(
        &self,
        dev: Weak<Mutex<VirtioDevice>>,
        key: Key,
        id: u64,
        mut stream: UnixStream,
        rx: mpsc::Receiver<WriterMsg>,
    ) {
        for msg in rx {
            let data = match msg {
                WriterMsg::Data(data) => data,
                WriterMsg::ShutdownWrite => {
                    let _ = stream.shutdown(std::net::Shutdown::Write);
                    continue;
                }
            };
            let ok = stream.write_all(&data).is_ok();
            let Some(dev) = dev.upgrade() else {
                return;
            };
            let (mut d, mut inner) = lock(&dev, self);
            let inner = &mut *inner;
            let Some(c) = inner.conns.get_mut(&key).filter(|c| c.id == id) else {
                // The guest closed the connection; finish writing the
                // data it sent before.
                if !ok {
                    break;
                }
                continue;
            };
            if !ok {
                inner.remove(key);
                inner.rst(key);
                self.credit.notify_all();
            } else {
                c.fwd_cnt = c.fwd_cnt.wrapping_add(data.len() as u32);
                if c.fwd_cnt.wrapping_sub(c.fwd_cnt_sent) >= BUF_ALLOC / 4 {
                    inner.send(key, OP_CREDIT_UPDATE, 0, Vec::new());
                }
            }
            inner.pump(&mut d.state);
            if !ok {
                return;
            }
        }
        // The connection is closed and all its data written.
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

impl Vsock {
    /// Processes a packet sent by the guest.
    fn transmit(
        &mut self,
        st: &mut VirtioState,
        dev: &Weak<Mutex<VirtioDevice>>,
        hdr: Hdr,
        data: Vec<u8>,
    ) {
        let mut inner = self.shared.inner.lock().unwrap();
        let inner = &mut *inner;
        let key = Key {
            host_port: hdr.dst_port,
            guest_port: hdr.src_port,
        };
        if hdr.dst_cid != HOST_CID || hdr.src_cid != inner.guest_cid {
            crate::log_debug!(st.log, "packet from {} to {}", hdr.src_cid, hdr.dst_cid);
            return;
        }
        if hdr.typ != TYPE_STREAM {
            if hdr.op != OP_RST {
                inner.rst(key);
            }
            return;
        }
        crate::log_debug!(st.log, "tx: {:?}", hdr);

        if hdr.op == OP_REQUEST {
            // The guest connects to a host port.
            if inner.conns.contains_key(&key) {
                inner.rst(key);
                return;
            }
            let path = format!("{}_{}", self.shared.uds, hdr.dst_port);
            match UnixStream::connect(&path) {
                Ok(stream) if !self.shared.uds.is_empty() => {
                    let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
                    let mut c = Conn::new(id, stream, true);
                    c.peer_buf_alloc = hdr.buf_alloc;
                    c.peer_fwd_cnt = hdr.fwd_cnt;
                    self.shared.start_conn(dev, key, &mut c);
                    inner.conns.insert(key, c);
                    inner.send(key, OP_RESPONSE, 0, Vec::new());
                }
                _ => inner.rst(key),
            }
            return;
        }

        let Some(c) = inner.conns.get_mut(&key) else {
            if hdr.op != OP_RST {
                inner.rst(key);
            }
            return;
        };
        c.peer_buf_alloc = hdr.buf_alloc;
        c.peer_fwd_cnt = hdr.fwd_cnt;
        c.credit_requested = false;
        match hdr.op {
            OP_RESPONSE if !c.established => {
                // The guest accepted a host connection.
                c.established = true;
                let reply = format!("OK {}\n", key.host_port);
                if c.stream.write_all(reply.as_bytes()).is_err() {
                    inner.remove(key);
                    inner.rst(key);
                } else {
                    self.shared.start_conn(dev, key, c);
                }
            }
            OP_RW if c.established => {
                if let Some(w) = &c.writer {
                    let _ = w.send(WriterMsg::Data(data));
                }
            }
            OP_CREDIT_UPDATE => {}
            OP_CREDIT_REQUEST => inner.send(key, OP_CREDIT_UPDATE, 0, Vec::new()),
            OP_SHUTDOWN => {
                c.guest_shutdown |= hdr.flags & (SHUTDOWN_RCV | SHUTDOWN_SEND);
                if c.guest_shutdown == SHUTDOWN_RCV | SHUTDOWN_SEND {
                    // The guest closed the socket and waits for the
                    // reset. The writer finishes the queued data before
                    // the socket closes.
                    if let Some(c) = inner.conns.remove(&key) {
                        if let Some(w) = &c.writer {
                            let _ = w.send(WriterMsg::ShutdownWrite);
                        }
                    }
                    inner.rst(key);
                } else if hdr.flags & SHUTDOWN_SEND != 0 {
                    if let Some(w) = &c.writer {
                        let _ = w.send(WriterMsg::ShutdownWrite);
                    }
                }
            }
            OP_RST => inner.remove(key),
            _ => {
                inner.remove(key);
                inner.rst(key);
            }
        }
        self.shared.credit.notify_all();
    }
}

impl Handler for Vsock {
    fn attach(&mut self, dev: Weak<Mutex<VirtioDevice>>) {
        self.dev = Some(dev.clone());
        if let Some(listener) = self.listener.take() {
            let shared = self.shared.clone();
            spawn("vsock-accept", move || shared.acceptor(dev, listener));
        }
    }

    fn reset(&mut self, _st: &mut VirtioState) -> Result<(), String> {
        let mut inner = self.shared.inner.lock().unwrap();
        for (_, mut c) in inner.conns.drain() {
            c.close();
        }
        inner.pending.clear();
        inner.rx_chains.clear();
        inner.event_chains.clear();
        inner.reset_pending = false;
        inner.reset_unacked = None;
        self.shared.credit.notify_all();
        Ok(())
    }

    fn execute_descriptor_chain(
        &mut self,
        st: &mut VirtioState,
        q: usize,
        idx: u16,
    ) -> Result<u32, String> {
        let vq = st.queues[q];
        match q {
            RXQ => {
                let mut inner = self.shared.inner.lock().unwrap();
                if inner.rx_chains.is_empty() && !st.frozen {
                    if let Some(n) = inner.fill(&st.guest, &vq, idx)? {
                        return Ok(n);
                    }
                }
                // Completed when a packet arrives.
                inner.rx_chains.push_back(idx);
                Ok(0)
            }
            TXQ => {
                let mut desc = st.guest.load_desc(&vq, idx)?;
                let mut packet = Vec::new();
                loop {
                    if desc.flags & VIRTQ_DESC_F_WRITE == 0 {
                        let ofs = packet.len();
                        packet.resize(ofs + desc.len as usize, 0);
                        st.guest.read(desc.addr, &mut packet[ofs..])?;
                    }
                    if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                        break;
                    }
                    desc = st.guest.load_desc(&vq, desc.next)?;
                }
                if packet.len() < HDR_LEN {
                    return Err(format!("short packet of {} bytes", packet.len()));
                }
                let hdr = Hdr::decode(&packet);
                let len = (hdr.len as usize).min(packet.len() - HDR_LEN);
                let data = packet[HDR_LEN..HDR_LEN + len].to_vec();
                let dev = self.dev.clone().expect("virtio-vsock not attached");
                self.transmit(st, &dev, hdr, data);
                self.shared.inner.lock().unwrap().pump(st);
                Ok(packet.len() as u32)
            }
            EVENTQ => {
                let mut inner = self.shared.inner.lock().unwrap();
                if inner.reset_pending && inner.event_chains.is_empty() {
                    return inner.event(&st.guest, &vq, idx);
                }
                if let Some(pos) = inner.reset_unacked {
                    // vq.last_avail_idx is after this buffer.
                    if vq.last_avail_idx.wrapping_sub(pos) as i16 > 0 {
                        inner.reset_unacked = None;
                    }
                }
                inner.event_chains.push_back(idx);
                Ok(0)
            }
            _ => Err(format!("invalid queue {}", q)),
        }
    }

    fn config_load(&mut self, _st: &mut VirtioState, offset: u64, size: u64) -> Option<u64> {
        // 5.10.4: the guest CID at offset 0x100.
        let ofs = offset.checked_sub(0x100)?;
        if ofs + size > 8 {
            return None;
        }
        let cid = self.shared.inner.lock().unwrap().guest_cid;
        let v = cid >> (ofs * 8);
        Some(if size == 8 {
            v
        } else {
            v & ((1u64 << (size * 8)) - 1)
        })
    }

    fn save_state(&self, _st: &VirtioState, w: &mut StateWriter) -> Result<(), String> {
        w.u64(self.shared.inner.lock().unwrap().guest_cid);
        Ok(())
    }

    fn load_state(&mut self, st: &mut VirtioState, r: &mut StateReader) -> Result<(), String> {
        let cid = r.u64()?;
        let mut inner = self.shared.inner.lock().unwrap();
        if cid != inner.guest_cid {
            return Err(format!("{}: snapshot has guest CID {}", st.log.name, cid));
        }
        // The connections of the suspended system are gone; the event
        // tells the guest to reset them. It is delivered when the
        // event buffers are resubmitted.
        inner.reset_pending = st.driver_ok();
        if inner.reset_pending {
            // Not acknowledged before a buffer posted after the
            // restored ones.
            inner.reset_unacked = Some(st.queues[EVENTQ].last_avail_idx);
        }
        Ok(())
    }
}
