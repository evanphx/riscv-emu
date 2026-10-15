//! VirtIO block device.

use super::mmio::*;
use crate::dev::Plic;
use crate::memory::Memory;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};

pub const BLK_DEVICE_ID: u32 = 2;
pub const BLK_SIZE: u64 = 4096;

pub const VIRTIO_BLK_T_IN: u32 = 0;
pub const VIRTIO_BLK_T_OUT: u32 = 1;
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;
pub const VIRTIO_BLK_T_GET_ID: u32 = 8;
pub const VIRTIO_BLK_T_GET_LIFETIME: u32 = 10;
pub const VIRTIO_BLK_T_DISCARD: u32 = 11;
pub const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;
pub const VIRTIO_BLK_T_SECURE_ERASE: u32 = 14;

pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

pub const VIRTIO_BLK_F_SIZE_MAX: u32 = 1;
pub const VIRTIO_BLK_F_SEG_MAX: u32 = 2;
pub const VIRTIO_BLK_F_GEOMETRY: u32 = 4;
pub const VIRTIO_BLK_F_RO: u32 = 5;
pub const VIRTIO_BLK_F_BLK_SIZE: u32 = 6;
pub const VIRTIO_BLK_F_FLUSH: u32 = 9;

fn blk_type_string(t: u32) -> String {
    match t {
        VIRTIO_BLK_T_IN => "read".into(),
        VIRTIO_BLK_T_OUT => "write".into(),
        VIRTIO_BLK_T_FLUSH => "flush".into(),
        VIRTIO_BLK_T_GET_ID => "get-id".into(),
        VIRTIO_BLK_T_GET_LIFETIME => "get-lifetime".into(),
        VIRTIO_BLK_T_DISCARD => "discard".into(),
        VIRTIO_BLK_T_WRITE_ZEROES => "write-zeroes".into(),
        VIRTIO_BLK_T_SECURE_ERASE => "secure-erase".into(),
        _ => format!("{{type {}}}", t),
    }
}

/// A block request handed to the worker thread.
struct Request {
    generation: u64,
    queue: Queue,
    head: u16,
}

struct BlkShared {
    file: File,
    readonly: bool,
    id: Vec<u8>,
    guest: GuestMem,
    log: crate::logger::Log,
    st_read: AtomicU64,
    st_wrote: AtomicU64,
}

/// VirtIO block device backed by a host file.
pub struct Blk {
    shared: Arc<BlkShared>,
    tx: Option<mpsc::Sender<Request>>,
    size_sectors: u64,
}

impl Blk {
    /// Creates a new block device. Returns the device and its bus
    /// segment.
    pub fn create(
        start: u64,
        plic: Plic,
        irq: u32,
        mem: Arc<Memory>,
        file: File,
        readonly: bool,
        id: &str,
    ) -> Result<VirtioMmio, String> {
        plic.register_irq(irq, "blk");
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        let state = VirtioState::new(
            "virtio-blk",
            BLK_DEVICE_ID,
            1 << VIRTIO_BLK_F_SEG_MAX,
            start,
            BLK_SIZE,
            plic,
            irq,
            mem.clone(),
            1,
        );
        let mut idv = id.as_bytes().to_vec();
        idv.push(0);
        let blk = Blk {
            shared: Arc::new(BlkShared {
                file,
                readonly,
                id: idv,
                guest: GuestMem { mem },
                log: state.log.clone(),
                st_read: AtomicU64::new(0),
                st_wrote: AtomicU64::new(0),
            }),
            tx: None,
            size_sectors: size.div_ceil(512),
        };
        let dev = VirtioDevice::new(state, Box::new(blk));
        Ok(VirtioMmio::new("virtio", dev))
    }
}

impl BlkShared {
    fn process(&self, vq: &Queue, idx: u16) -> Result<u32, String> {
        let g = &self.guest;
        let mut req = g.load_desc(vq, idx)?;
        if req.len != 16 || req.flags & VIRTQ_DESC_F_NEXT == 0 {
            return Err(format!("invalid blk header: {}", req));
        }
        let t = g.read_u32(req.addr)?;
        let sector = g.read_u64(req.addr + 8)?;
        crate::log_debug!(self.log, "req {} sector {}", blk_type_string(t), sector);

        let mut file_offset = sector * 512;
        let mut status = VIRTIO_BLK_S_OK;
        let mut status_seen = false;
        let mut transferred = 0u32;

        while req.flags & VIRTQ_DESC_F_NEXT != 0 {
            req = g.load_desc(vq, req.next)?;
            if req.flags & VIRTQ_DESC_F_NEXT != 0 {
                // Data block.
                match t {
                    VIRTIO_BLK_T_IN => {
                        // SAFETY: the descriptor buffer is owned by the
                        // device until the request completes.
                        let buf = match unsafe { g.slice_mut(req.addr, req.len as u64) } {
                            Ok(b) => b,
                            Err(e) => {
                                self.log.error(format_args!("guestData: {}", e));
                                status = VIRTIO_BLK_S_IOERR;
                                continue;
                            }
                        };
                        let n = read_full_at(&self.file, buf, file_offset);
                        match n {
                            Ok(n) => {
                                self.st_read.fetch_add(n as u64, Ordering::Relaxed);
                                file_offset += n as u64;
                                transferred += n as u32;
                            }
                            Err(e) => {
                                self.log.error(format_args!(
                                    "read failed from host file offset {}: {}",
                                    file_offset, e
                                ));
                                status = VIRTIO_BLK_S_IOERR;
                            }
                        }
                    }
                    VIRTIO_BLK_T_OUT => {
                        if self.readonly {
                            // Complete the chain without writing.
                            transferred += req.len;
                            status = VIRTIO_BLK_S_UNSUPP;
                        } else {
                            // SAFETY: see above.
                            let buf = match unsafe { g.slice(req.addr, req.len as u64) } {
                                Ok(b) => b,
                                Err(e) => {
                                    self.log.error(format_args!("guestData: {}", e));
                                    status = VIRTIO_BLK_S_IOERR;
                                    continue;
                                }
                            };
                            match self.file.write_all_at(buf, file_offset) {
                                Ok(()) => {
                                    let n = buf.len();
                                    self.st_wrote.fetch_add(n as u64, Ordering::Relaxed);
                                    file_offset += n as u64;
                                    transferred += n as u32;
                                }
                                Err(e) => {
                                    self.log.error(format_args!(
                                        "write failed to host file offset {}: {}",
                                        file_offset, e
                                    ));
                                    status = VIRTIO_BLK_S_IOERR;
                                }
                            }
                        }
                    }
                    VIRTIO_BLK_T_GET_ID => {
                        let n = (req.len as usize).min(self.id.len());
                        g.write(req.addr, &self.id[..n])?;
                        transferred += n as u32;
                    }
                    _ => {
                        self.log
                            .error(format_args!("type {} not supported", blk_type_string(t)));
                        status = VIRTIO_BLK_S_UNSUPP;
                    }
                }
            } else {
                // Status block.
                status_seen = true;
                if req.len != 1 {
                    return Err(format!("invalid blk status: {}", req));
                }
                g.write_u8(req.addr, status)?;
            }
        }
        if !status_seen {
            return Err("invalid chain: no status block".to_string());
        }
        Ok(transferred)
    }
}

/// Reads into buf from the file offset. Bytes past the end of file
/// read as zeros.
fn read_full_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read_at(&mut buf[n..], offset + n as u64) {
            Ok(0) => {
                buf[n..].fill(0);
                return Ok(buf.len());
            }
            Ok(m) => n += m,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

impl Handler for Blk {
    fn attach(&mut self, dev: Weak<Mutex<VirtioDevice>>) {
        let (tx, rx) = mpsc::channel::<Request>();
        self.tx = Some(tx);
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("virtio-blk".to_string())
            .spawn(move || {
                for req in rx {
                    let tx = match shared.process(&req.queue, req.head) {
                        Ok(n) => n,
                        Err(e) => {
                            shared.log.error(format_args!("process failed: {}", e));
                            0
                        }
                    };
                    let Some(dev) = dev.upgrade() else {
                        return;
                    };
                    let mut d = dev.lock().unwrap();
                    if d.state.generation == req.generation {
                        d.state.complete_descriptor(0, req.head, tx);
                    }
                }
            })
            .expect("failed to start virtio-blk worker");
    }

    fn device_stats(&self, st: &VirtioState) {
        println!(
            "{}: read    : {}",
            st.log.name,
            FileSize(self.shared.st_read.load(Ordering::Relaxed))
        );
        println!(
            "{}: wrote   : {}",
            st.log.name,
            FileSize(self.shared.st_wrote.load(Ordering::Relaxed))
        );
    }

    fn execute_descriptor_chain(
        &mut self,
        st: &mut VirtioState,
        q: usize,
        idx: u16,
    ) -> Result<u32, String> {
        let req = Request {
            generation: st.generation,
            queue: st.queues[q],
            head: idx,
        };
        if let Some(tx) = &self.tx {
            tx.send(req).map_err(|e| e.to_string())?;
        }
        Ok(0)
    }

    fn config_load(&mut self, _st: &mut VirtioState, offset: u64, size: u64) -> Option<u64> {
        if size != 4 {
            return None;
        }
        match offset {
            0x100 => Some(self.size_sectors & 0xffff_ffff),
            0x104 => Some(self.size_sectors >> 32),
            0x108 => Some(4096),                       // size_max
            0x10c => Some((QUEUE_NUM_MAX - 2) as u64), // seg_max
            _ => None,
        }
    }
}
